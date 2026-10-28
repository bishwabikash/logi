//! 0x8110 MOUSE_BUTTON_SPY — report physical buttons to the host, and stop
//! them acting on their own.
//!
//! See PROTOCOL.md. Two things about this feature are worth stating up front:
//!
//!   * The remapping table is dangerous state. Writing zeros to all of it
//!     unmaps every button: the mouse still moves but nothing clicks, in
//!     either memory mode, and it survives unplugging. That happened during
//!     development. So `set_remapping` refuses an all-zero table, `new()`
//!     captures the table before anything else so a restore has a target, and
//!     `restore_identity` exists to dig a device out of that hole. That last
//!     one is the escape hatch, so it is deliberately the one operation here
//!     that needs nothing but the button count.
//!   * Function 1 is `startSpy`, not a getter, and function 4 is a setter.
//!     Never sweep this feature's functions with empty parameters.
//!
//! Binding a button to a host action is two independent steps: `start` makes
//! the device report presses, and `silence` stops the chosen buttons also
//! doing their physical job. Silencing without listening leaves a dead button;
//! listening without silencing gives both behaviours at once, which is what
//! you want for a button that keeps its click.

use crate::features;
use crate::hidpp::{Device, Error, Result, LONG_REPORT};

const GET_NB_OF_BUTTONS: u8 = 0x00;
const START_SPY: u8 = 0x01;
const STOP_SPY: u8 = 0x02;
const GET_REMAPPING: u8 = 0x03;
const SET_REMAPPING: u8 = 0x04;

/// A long report's payload is 16 bytes, so the table cannot exceed that.
pub const MAX_BUTTONS: u8 = 16;

/// The factory default table: button n maps to itself. Only the first
/// `button_count` bytes are ever read or written; the tail is padding so this
/// can be a constant.
pub const IDENTITY: [u8; 16] = [
    0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08,
    0x09, 0x0a, 0x0b, 0x0c, 0x0d, 0x0e, 0x0f, 0x10,
];

pub struct ButtonSpy {
    idx: u8,
    count: u8,
    /// The table as `new()` found it, or None when that read did not answer.
    ///
    /// None is survivable on purpose. `restore_identity` is the escape hatch
    /// for a mouse that cannot click, and it needs only `count`, so one
    /// dropped reply to `getRemapping` must not be what stands between the
    /// user and a working mouse. What None does forbid is `silence` and
    /// `restore`: both would have to invent a restore target, and inventing
    /// one means either leaving a button dead or flattening a remapping the
    /// device genuinely carries.
    saved: Option<Vec<u8>>,
}

impl ButtonSpy {
    /// Read the button count and snapshot the remapping table.
    ///
    /// The snapshot happens before anything else is sent, so a later
    /// `restore` has a target. The snapshot is run through the write gate and
    /// replaced by IDENTITY if it fails it: a table the gate would refuse is
    /// not a restore target at all, and a device already zeroed by something
    /// else must not have that state preserved and written back.
    ///
    /// Any *individual* zero is healed back to its own button for the same
    /// reason. A zero means that button is dead, the table is non-volatile, and
    /// the way a slot comes to be zero is a previous run that silenced it and
    /// was killed before it could restore. Keeping that zero would make the
    /// restore target itself carry a dead button, so the only way out would be
    /// `logi bind reset`.
    ///
    /// A snapshot that does not answer is *not* fatal, and that is the whole
    /// point of `saved` being an Option. Both recovery paths — `logi bind
    /// reset` and the panel's restore button — construct a ButtonSpy and then
    /// call `restore_identity`, which needs only the count. Failing here on a
    /// single lost `getRemapping` reply would take the escape hatch away from
    /// exactly the device that needs it: one whose buttons do nothing. The
    /// count stays fatal, because without it no table can be built at all.
    pub fn new(dev: &mut Device) -> Result<Self> {
        let idx = dev.feature_index(features::MOUSE_BUTTON_SPY)?;
        let p = dev.call(idx, GET_NB_OF_BUTTONS, &[])?;
        let count = p.first().copied().unwrap_or(0);
        if count == 0 || count > MAX_BUTTONS {
            return Err(Error::Protocol(format!(
                "0x8110 reports {count} buttons, which cannot be right"
            )));
        }
        let probe = ButtonSpy { idx, count, saved: None };
        let live = probe.get_remapping(dev).ok();
        Ok(ButtonSpy { idx, count, saved: live.map(|t| restorable(t, count)) })
    }

    /// The feature index on this device. Events carry it in byte 2; it is not
    /// the same number on every model.
    pub fn feature_index(&self) -> u8 {
        self.idx
    }

    pub fn button_count(&self) -> u8 {
        self.count
    }

    /// The remapping table as it was when new() ran. The restore target, or
    /// None if that read did not answer.
    pub fn saved_remapping(&self) -> Option<&[u8]> {
        self.saved.as_deref()
    }

    /// The restore target, or the reason there is none.
    ///
    /// Everything that writes a table built from the device's own state goes
    /// through here, so a missing snapshot refuses the write rather than
    /// guessing. The message names the escape hatch because that is the one
    /// thing still available.
    fn snapshot(&self) -> Result<&[u8]> {
        self.saved.as_deref().ok_or_else(|| {
            Error::Protocol(
                "could not read the button remapping table when this device \
                 was opened, so there is nothing to restore it to and nothing \
                 has been silenced. Try again, or run `logi bind reset` to put \
                 the factory mapping back."
                    .into(),
            )
        })
    }

    pub fn start(&self, dev: &mut Device) -> Result<()> {
        dev.call(self.idx, START_SPY, &[])?;
        Ok(())
    }

    pub fn stop(&self, dev: &mut Device) -> Result<()> {
        let r = dev.call(self.idx, STOP_SPY, &[]);
        // Events already in the kernel queue would otherwise be read as if the
        // spy were still running. Drain even when the call failed: that is the
        // case where the spy is most likely still sending.
        dev.drain();
        r.map(|_| ())
    }

    pub fn get_remapping(&self, dev: &mut Device) -> Result<Vec<u8>> {
        let p = dev.call(self.idx, GET_REMAPPING, &[])?;
        if p.len() < self.count as usize {
            return Err(Error::Protocol(format!(
                "getRemapping returned {} bytes for {} buttons",
                p.len(),
                self.count
            )));
        }
        Ok(p[..self.count as usize].to_vec())
    }

    /// Write the table. Refuses anything that looks like the write that
    /// unmaps the whole mouse.
    ///
    /// An error from here does not mean the table is unchanged: the device can
    /// apply a write whose reply is then lost to the timeout. Treat a failure
    /// as "the table is now unknown" and `restore`.
    pub fn set_remapping(&self, dev: &mut Device, table: &[u8]) -> Result<()> {
        check_table(table, self.count)?;
        dev.call(self.idx, SET_REMAPPING, table)?;
        Ok(())
    }

    /// Silence exactly the listed buttons: every button *not* listed goes back
    /// to its own physical function at the same time.
    ///
    /// That "exactly" is the contract the callers rely on. All of them pass the
    /// complete set from `Bindings::to_silence()` and tell the user that the
    /// rest are restored, so an empty list is meaningful — it means put the
    /// whole table back — and a shorter list than last time has to un-silence
    /// the difference. An additive build on the device's live table cannot do
    /// either: a button dropped from Replace, or a run killed before its
    /// teardown, would stay dead across unplugging with nothing short of
    /// `logi bind reset` to recover it.
    ///
    /// So the table is built from the snapshot `new()` took, which is already
    /// gated and already healed of dead slots. That preserves any non-identity
    /// remapping the device genuinely carries while making the silenced set
    /// exactly what was asked for.
    ///
    /// The table is validated before anything is sent, an identical table is
    /// not rewritten — this one survives unplugging, so a pointless write is
    /// wear for nothing — and if the write itself fails the saved table is put
    /// back: a lost reply can hide a write that landed, and a button left dead
    /// is worse than one left un-silenced.
    pub fn silence(&self, dev: &mut Device, buttons: &[u8]) -> Result<()> {
        check_buttons(buttons, self.count)?;
        let want = silence_table(self.snapshot()?, buttons);
        check_table(&want, self.count)?;
        if self.get_remapping(dev)? == want {
            return Ok(());
        }
        if let Err(e) = self.set_remapping(dev, &want) {
            let _ = self.restore(dev);
            return Err(e);
        }
        Ok(())
    }

    /// Put the table back to what new() saved. Must be callable repeatedly,
    /// and cannot be refused by the gate: new() only ever saves a table the
    /// gate accepts. Errors only if there was no snapshot to save, which is
    /// also the case where nothing was ever silenced.
    pub fn restore(&self, dev: &mut Device) -> Result<()> {
        self.set_remapping(dev, self.snapshot()?)
    }

    /// Force the factory identity table, for recovering a device whose table
    /// was zeroed by something else.
    pub fn restore_identity(&self, dev: &mut Device) -> Result<()> {
        self.set_remapping(dev, &IDENTITY[..self.count as usize])
    }
}

/// Zero the named buttons, keep every other byte byte-for-byte.
///
/// Buttons are 1-based, so button 0 has no slot. `checked_sub` rather than
/// `- 1` because an unguarded caller would otherwise underflow and panic.
fn silence_table(base: &[u8], buttons: &[u8]) -> Vec<u8> {
    let mut t = base.to_vec();
    for b in buttons {
        if let Some(slot) = (*b as usize).checked_sub(1).and_then(|i| t.get_mut(i)) {
            *slot = 0;
        }
    }
    t
}

/// Pick a table that `restore` can always write, and that leaves no button
/// dead: the device's own with any unmapped slot healed back to its own
/// button, or the factory identity if the gate still refuses it.
///
/// Healing matters because this table is non-volatile. A slot reads zero when
/// a previous run silenced it and did not live to restore it, and carrying
/// that zero forward would make every later `restore` write the dead button
/// back again.
fn restorable(live: Vec<u8>, count: u8) -> Vec<u8> {
    let healed = heal(live, count);
    if check_table(&healed, count).is_ok() {
        healed
    } else {
        IDENTITY[..count as usize].to_vec()
    }
}

/// Put every unmapped slot back to its own button, leaving any other
/// remapping the device carries alone.
fn heal(mut table: Vec<u8>, count: u8) -> Vec<u8> {
    if table.len() != count as usize {
        return IDENTITY[..count as usize].to_vec();
    }
    for (i, b) in table.iter_mut().enumerate() {
        if *b == 0 {
            *b = IDENTITY[i];
        }
    }
    table
}

/// The gate in front of every write to the remapping table.
fn check_table(table: &[u8], count: u8) -> Result<()> {
    if table.len() != count as usize {
        return Err(Error::Protocol(format!(
            "remapping table is {} bytes, device has {count} buttons",
            table.len()
        )));
    }
    if table.iter().all(|b| *b == 0) {
        return Err(Error::Protocol(
            "refusing to write an all-zero remapping table: that unmaps every \
             button on the mouse, in both memory modes, and survives \
             unplugging. Name the buttons to silence instead."
                .into(),
        ));
    }
    if let Some(bad) = table.iter().find(|b| **b > count) {
        return Err(Error::Protocol(format!(
            "remapping entry {bad} is not a button on a {count}-button device"
        )));
    }
    Ok(())
}

fn check_buttons(buttons: &[u8], count: u8) -> Result<()> {
    if let Some(bad) = buttons.iter().find(|b| **b == 0 || **b > count) {
        return Err(Error::Protocol(format!(
            "button {bad}: this device has buttons 1..{count}"
        )));
    }
    Ok(())
}

/// Decode one unsolicited report into the pressed-button bitmap.
/// Returns None if this report is not a spy event for this feature index.
///
/// The device index in byte 1 is deliberately not checked: it is 0xff wired
/// but 1..6 behind a receiver, and the caller knows which device it opened.
pub fn decode_event(report: &[u8], feature_index: u8) -> Option<u16> {
    // Header plus the two bitmap bytes.
    if report.len() < 6 {
        return None;
    }
    if report[0] != LONG_REPORT || report[2] != feature_index {
        return None;
    }
    // Byte 3 is (function or event id) << 4 | software id. A reply always
    // carries a nonzero software id in the low nibble, so that nibble alone
    // separates event from reply; the high nibble is checked too because an
    // event id other than 0 would be some other 0x8110 event, whose payload is
    // not a button bitmap.
    if report[3] != 0 {
        return None;
    }
    Some(u16::from_be_bytes([report[4], report[5]]))
}

/// Edge-detect between two bitmaps. Returns (newly pressed, newly released)
/// as 1-based button numbers.
pub fn transitions(prev: u16, now: u16) -> (Vec<u8>, Vec<u8>) {
    (bits(now & !prev), bits(prev & !now))
}

/// Set bits as 1-based button numbers, lowest first.
fn bits(mask: u16) -> Vec<u8> {
    (0..16).filter(|i| mask & (1 << i) != 0).map(|i| i + 1).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The real factory table off a G502 HERO, 11 buttons.
    fn factory() -> Vec<u8> {
        vec![0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0a, 0x0b]
    }

    #[test]
    fn identity_prefix_is_the_factory_table() {
        assert_eq!(&IDENTITY[..11], factory().as_slice());
    }

    #[test]
    fn decodes_the_real_button_one_event() {
        // 11 ff 0d 00 | 00 01 00 00
        let r = [0x11, 0xff, 0x0d, 0x00, 0x00, 0x01, 0x00, 0x00];
        assert_eq!(decode_event(&r, 0x0d), Some(0x0001));
    }

    #[test]
    fn decodes_the_real_all_released_event() {
        let r = [0x11, 0xff, 0x0d, 0x00, 0x00, 0x00, 0x00, 0x00];
        assert_eq!(decode_event(&r, 0x0d), Some(0x0000));
    }

    #[test]
    fn bitmap_is_big_endian() {
        // Button 9 is bit 8, which lands in the first payload byte.
        let r = [0x11, 0xff, 0x0d, 0x00, 0x01, 0x00, 0x00, 0x00];
        assert_eq!(decode_event(&r, 0x0d), Some(0x0100));
        assert_eq!(transitions(0, 0x0100).0, vec![9]);
    }

    #[test]
    fn rejects_short_report_id() {
        let r = [0x10, 0xff, 0x0d, 0x00, 0x00, 0x01, 0x00];
        assert_eq!(decode_event(&r, 0x0d), None);
    }

    #[test]
    fn rejects_another_features_event() {
        let r = [0x11, 0xff, 0x0e, 0x00, 0x00, 0x01, 0x00, 0x00];
        assert_eq!(decode_event(&r, 0x0d), None);
    }

    #[test]
    fn rejects_a_reply_carrying_a_software_id() {
        // getRemapping's own answer: function 3, software id 8.
        let r = [0x11, 0xff, 0x0d, 0x38, 0x01, 0x02, 0x03, 0x04];
        assert_eq!(decode_event(&r, 0x0d), None);
    }

    #[test]
    fn rejects_a_truncated_report() {
        let r = [0x11, 0xff, 0x0d, 0x00, 0x00];
        assert_eq!(decode_event(&r, 0x0d), None);
        assert_eq!(decode_event(&[], 0x0d), None);
    }

    #[test]
    fn press_and_release_are_single_edges() {
        assert_eq!(transitions(0x0000, 0x0001), (vec![1], vec![]));
        assert_eq!(transitions(0x0001, 0x0000), (vec![], vec![1]));
    }

    #[test]
    fn no_change_is_no_edge() {
        assert_eq!(transitions(0x0005, 0x0005), (vec![], vec![]));
        assert_eq!(transitions(0, 0), (vec![], vec![]));
    }

    #[test]
    fn simultaneous_buttons_edge_independently() {
        // 1 and 3 held, then 3 released and 11 pressed.
        let (down, up) = transitions(0b0000_0101, 0b0100_0000_0001);
        assert_eq!(down, vec![11]);
        assert_eq!(up, vec![3]);
    }

    #[test]
    fn all_released_reports_everything_held() {
        let (down, up) = transitions(0b0000_0000_0001_0011, 0);
        assert!(down.is_empty());
        assert_eq!(up, vec![1, 2, 5]);
    }

    #[test]
    fn silencing_touches_only_the_named_buttons() {
        assert_eq!(
            silence_table(&factory(), &[6, 7]),
            vec![0x01, 0x02, 0x03, 0x04, 0x05, 0x00, 0x00, 0x08, 0x09, 0x0a, 0x0b]
        );
    }

    #[test]
    fn silencing_is_cumulative_over_a_live_table() {
        let once = silence_table(&factory(), &[11]);
        assert_eq!(
            silence_table(&once, &[6]),
            vec![0x01, 0x02, 0x03, 0x04, 0x05, 0x00, 0x07, 0x08, 0x09, 0x0a, 0x00]
        );
    }

    #[test]
    fn silencing_nothing_changes_nothing() {
        assert_eq!(silence_table(&factory(), &[]), factory());
    }

    #[test]
    fn button_zero_does_not_underflow() {
        // 0 - 1 as a usize would panic in a debug build.
        assert_eq!(silence_table(&factory(), &[0]), factory());
        assert_eq!(silence_table(&factory(), &[0, 99]), factory());
    }

    #[test]
    fn a_usable_live_table_is_the_restore_target() {
        assert_eq!(restorable(factory(), 11), factory());
        // A remapping that is not the identity is genuine device state and is
        // kept: only dead slots are touched.
        let mut remapped = factory();
        remapped[3] = 0x05;
        assert_eq!(restorable(remapped.clone(), 11), remapped);
    }

    #[test]
    fn a_dead_slot_is_never_carried_into_the_restore_target() {
        // The table survives unplugging, so this is what is read back after a
        // run that silenced button 6 and was killed before its teardown.
        // Restoring that verbatim would leave button 6 dead for ever.
        let silenced = silence_table(&factory(), &[6]);
        assert_eq!(restorable(silenced, 11), factory());
        // Only the dead slot is healed; the rest of the table is untouched.
        let mut remapped = factory();
        remapped[3] = 0x05;
        let silenced = silence_table(&remapped, &[11]);
        assert_eq!(restorable(silenced, 11), remapped);
    }

    #[test]
    fn silencing_nothing_against_the_saved_table_restores_it() {
        // `silence(&[])` is the GUI's "every button back to its own job": with
        // the snapshot as the base it writes the whole table back, rather than
        // being the no-op that leaves earlier Replaces silenced.
        let saved = factory();
        assert_eq!(silence_table(&saved, &[]), saved);
        // And dropping a button from the set un-silences it, because the base
        // is the snapshot and not the already-silenced live table.
        let want = silence_table(&saved, &[6]);
        assert_eq!(want[7], 0x08, "button 8 silenced last time must come back");
        assert_eq!(want[5], 0x00);
    }

    #[test]
    fn an_unwritable_live_table_falls_back_to_identity() {
        // Zeroed by somebody else: keeping it would make restore re-break the
        // mouse.
        assert_eq!(restorable(vec![0u8; 11], 11), factory());
        // And a table the gate would refuse for any other reason is no
        // restore target either, or restore would fail forever.
        let mut odd = factory();
        odd[3] = 0x0c;
        assert_eq!(restorable(odd, 11), factory());
        assert_eq!(restorable(vec![1, 2, 3], 11), factory());
    }

    #[test]
    fn every_restore_target_passes_the_gate() {
        for live in [factory(), vec![0u8; 11], vec![0xff; 11], vec![], vec![1, 2]] {
            check_table(&restorable(live, 11), 11).unwrap();
        }
    }

    #[test]
    fn a_read_snapshot_is_what_silence_and_restore_build_on() {
        let spy = ButtonSpy { idx: 0x0d, count: 11, saved: Some(factory()) };
        assert_eq!(spy.saved_remapping(), Some(factory().as_slice()));
        assert_eq!(spy.snapshot().unwrap(), factory().as_slice());
    }

    #[test]
    fn a_failed_snapshot_blocks_silencing_but_not_the_escape_hatch() {
        // What `new()` builds when getRemapping does not answer. Erroring out
        // of `new()` instead would take `logi bind reset` and the panel's
        // restore button away from the one device that needs them: both go
        // through ButtonSpy::new, and both want only restore_identity.
        let blind = ButtonSpy { idx: 0x0d, count: 11, saved: None };
        assert_eq!(blind.saved_remapping(), None);

        // Nothing that has to invent a restore target may proceed: guessing
        // would either leave a button dead or flatten a remapping the device
        // genuinely carries.
        let e = blind.snapshot().unwrap_err();
        let msg = format!("{e}");
        assert!(msg.contains("nothing has been silenced"), "{msg}");
        assert!(msg.contains("logi bind reset"), "{msg}");

        // restore_identity needs only the count, so it is still writable.
        check_table(&IDENTITY[..blind.count as usize], blind.count).unwrap();
    }

    #[test]
    fn an_all_zero_table_is_refused() {
        let e = check_table(&[0u8; 11], 11).unwrap_err();
        assert!(format!("{e}").contains("unmaps every button"));
    }

    #[test]
    fn silencing_every_button_is_refused() {
        let all: Vec<u8> = (1..=11).collect();
        let t = silence_table(&factory(), &all);
        assert!(check_table(&t, 11).is_err(), "that write is what broke a real mouse");
    }

    #[test]
    fn a_partially_zeroed_table_is_allowed() {
        check_table(&silence_table(&factory(), &[6, 7]), 11).unwrap();
        check_table(&factory(), 11).unwrap();
    }

    #[test]
    fn wrong_length_tables_are_refused() {
        assert!(check_table(&factory(), 16).is_err());
        assert!(check_table(&IDENTITY, 11).is_err());
    }

    #[test]
    fn entries_outside_the_button_range_are_refused() {
        let mut t = factory();
        t[0] = 0x0c;
        assert!(check_table(&t, 11).is_err());
    }

    #[test]
    fn button_numbers_are_validated_against_the_count() {
        assert!(check_buttons(&[1, 11], 11).is_ok());
        assert!(check_buttons(&[0], 11).is_err(), "buttons are 1-based");
        assert!(check_buttons(&[12], 11).is_err());
        assert!(check_buttons(&[], 11).is_ok());
    }
}
