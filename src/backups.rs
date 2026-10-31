//! Safety backups taken automatically, once per launch, before any automatic flash write.
//! Stored in the data directory; nothing is written if the mouse is unchanged since the newest backup.

use std::path::{Path, PathBuf};

/// `$XDG_DATA_HOME/logi/backups`, or `~/.local/share/logi/backups`.
pub fn dir() -> PathBuf {
    let base = std::env::var("XDG_DATA_HOME")
        .ok()
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(std::env::var("HOME").unwrap_or_default()).join(".local/share")
        });
    base.join("logi/backups")
}

#[derive(Debug, PartialEq, Eq)]
pub enum Plan {
    Same(PathBuf),
    Write(PathBuf),
}

/// Where a backup goes, or that none is needed. Only the newest backup is compared, so A -> B -> A
/// still takes a new restore point.
pub fn plan(dir: &Path, json: &str, now_unix: u64) -> std::io::Result<Plan> {
    let mut names: Vec<String> = match std::fs::read_dir(dir) {
        Ok(rd) => rd
            .filter_map(|e| e.ok())
            .filter_map(|e| e.file_name().into_string().ok())
            .filter(|n| n.starts_with("backup-") && n.ends_with(".json"))
            .collect(),
        // No directory yet is the normal first run.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(e) => return Err(e),
    };
    // Fixed-width stamps, most significant first: name order is time order.
    names.sort();
    if let Some(newest) = names.last() {
        let path = dir.join(newest);
        if std::fs::read_to_string(&path).map(|t| t == json).unwrap_or(false) {
            return Ok(Plan::Same(path));
        }
    }

    std::fs::create_dir_all(dir)?;
    let stamp = stamp(now_unix);
    let mut candidate = format!("backup-{stamp}.json");
    // Two in one second would land on one name.
    let mut n = 2;
    while dir.join(&candidate).exists() {
        candidate = format!("backup-{stamp}-{n}.json");
        n += 1;
    }
    Ok(Plan::Write(dir.join(candidate)))
}

/// `20261029-163452`: UTC, so names sort by time and never go backwards with the local zone. No date library.
pub fn stamp(unix_secs: u64) -> String {
    let days = (unix_secs / 86_400) as i64;
    let rem = unix_secs % 86_400;
    let (y, m, d) = civil_from_days(days);
    format!(
        "{y:04}{m:02}{d:02}-{:02}{:02}{:02}",
        rem / 3600,
        (rem % 3600) / 60,
        rem % 60
    )
}

/// Days since 1970-01-01 to (year, month, day); Howard Hinnant's `civil_from_days`.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (y + (m <= 2) as i64, m as u32, d as u32)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "logi-backups-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.subsec_nanos())
                .unwrap_or(0)
        ));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    #[test]
    fn the_stamp_is_the_utc_time_in_a_sortable_form() {
        assert_eq!(stamp(0), "19700101-000000");
        // A date everyone can check: one billion seconds.
        assert_eq!(stamp(1_000_000_000), "20010909-014640");
        // The leap day, which a month-length table gets wrong.
        assert_eq!(stamp(951_782_400), "20000229-000000");
        // The last second of a day does not roll into the next.
        assert_eq!(stamp(86_399), "19700101-235959");
        assert_eq!(stamp(86_400), "19700102-000000");
    }

    #[test]
    fn stamps_sort_in_time_order() {
        let a = stamp(1_000_000_000);
        let b = stamp(1_000_000_001);
        let c = stamp(1_700_000_000);
        assert!(a < b && b < c);
    }

    #[test]
    fn the_first_run_creates_the_directory_and_picks_a_name() {
        let dir = temp("first");
        assert!(!dir.exists());
        let p = plan(&dir, "{}", 1_000_000_000).unwrap();
        assert!(dir.exists(), "a write was called for, so the directory is made");
        assert_eq!(p, Plan::Write(dir.join("backup-20010909-014640.json")));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_unchanged_mouse_writes_nothing() {
        let dir = temp("same");
        std::fs::create_dir_all(&dir).unwrap();
        let existing = dir.join("backup-20010909-014640.json");
        std::fs::write(&existing, "state A").unwrap();
        assert_eq!(plan(&dir, "state A", 1_700_000_000).unwrap(), Plan::Same(existing));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_changed_mouse_gets_a_new_file_and_the_old_one_stays() {
        let dir = temp("changed");
        std::fs::create_dir_all(&dir).unwrap();
        let old = dir.join("backup-20010909-014640.json");
        std::fs::write(&old, "state A").unwrap();
        let p = plan(&dir, "state B", 1_700_000_000).unwrap();
        assert_eq!(p, Plan::Write(dir.join("backup-20231114-221320.json")));
        assert_eq!(std::fs::read_to_string(&old).unwrap(), "state A");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn only_the_newest_backup_is_compared() {
        // A, then B, then back at A: comparing every file would skip the restore point.
        let dir = temp("newest");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("backup-20240101-000000.json"), "A").unwrap();
        std::fs::write(dir.join("backup-20240201-000000.json"), "B").unwrap();
        assert!(matches!(plan(&dir, "A", 1_800_000_000).unwrap(), Plan::Write(_)));
        assert!(matches!(plan(&dir, "B", 1_800_000_000).unwrap(), Plan::Same(_)));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn two_in_one_second_do_not_share_a_name() {
        let dir = temp("collide");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("backup-20010909-014640.json"), "A").unwrap();
        let p = plan(&dir, "B", 1_000_000_000).unwrap();
        assert_eq!(p, Plan::Write(dir.join("backup-20010909-014640-2.json")));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn files_that_are_not_backups_are_ignored() {
        let dir = temp("other");
        std::fs::create_dir_all(&dir).unwrap();
        // Sorts after every backup name, and must not be mistaken for one.
        std::fs::write(dir.join("zzz-notes.json"), "same text").unwrap();
        std::fs::write(dir.join("backup-20240101-000000.json.part"), "same text").unwrap();
        assert!(matches!(plan(&dir, "same text", 1_800_000_000).unwrap(), Plan::Write(_)));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_unreadable_directory_is_an_error_not_a_silent_skip() {
        // A file where the directory should be must be an error, not "nothing to compare".
        let dir = temp("blocked");
        std::fs::write(&dir, "i am a file").unwrap();
        assert!(plan(&dir, "x", 1).is_err());
        let _ = std::fs::remove_file(&dir);
    }
}
