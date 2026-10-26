# no tool: command -v pytest -> /usr/bin/pytest (used as the runner; the tests themselves are stdlib unittest so they also run under python3 -m unittest). No installed tool can test an unreleased module's byte sequences, so the file is needed.
"""Offline tests for g502.profile_io: no hardware, no /dev/hidraw.

The fake device below is deliberately strict — it rejects a MEMORY_WRITE
outside an open window, a chunk that is not exactly 16 bytes, and a commit
whose byte count does not match the window — so a regression in the write
sequence fails here rather than against real flash.
"""
from __future__ import annotations

import json
import struct
import sys
import tempfile
import unittest
from dataclasses import dataclass
from pathlib import Path

# The repo has no install step; tests run from the checkout.
sys.path.insert(0, str(Path(__file__).resolve().parents[1]))

from g502 import profile_io as pio                      # noqa: E402
from g502.hidpp import HidppError                       # noqa: E402
from g502.profile_blob import (OFF_CRC, OFF_NAME, SECTOR_SIZE,  # noqa: E402
                               Profile, crc_ccitt)

FEATURE_IDX = 12  # 0x8100 sits at index 12 on this device


@dataclass(frozen=True)
class Call:
    func: int
    params: bytes
    long: bool


def make_sector(seed: int) -> bytes:
    """A deterministic 256-byte sector with a valid CRC."""
    body = bytes((seed + i) & 0xFF for i in range(OFF_CRC))
    return pio.stamp_crc(body + b"\x00\x00")


class FakeInfo:
    def __init__(self, **over):
        self.memory_model_id = 1
        self.profile_format_id = 2
        self.macro_format_id = 1
        self.profile_count = 5
        self.profile_count_oob = 5
        self.button_count = 11
        self.sector_count = 16
        self.sector_size = SECTOR_SIZE
        self.mechanical_layout = 0x0A
        self.various_info = 0
        self.__dict__.update(over)


class FakeDevice:
    """Records every HID++ call and emulates the 0x8100 memory window."""

    path = "/dev/hidraw-fake"

    def __init__(self, flash: dict[int, bytes] | None = None):
        self.calls: list[Call] = []
        self.flash: dict[int, bytes] = dict(flash or {})
        self.read_errors: set[int] = set()
        self.corrupt: dict[int, int] = {}  # sector -> offset to flip on commit
        self._window: tuple[int, int, int] | None = None
        self._buf = bytearray()

    def call(self, idx, func, params=b"", long=True, timeout=0.8):
        assert idx == FEATURE_IDX, f"wrong feature index {idx}"
        self.calls.append(Call(func, bytes(params), long))

        if func == pio.MEMORY_READ:
            sector, offset = struct.unpack(">HH", params[:4])
            if sector in self.read_errors:
                raise HidppError(0x02, idx, func)
            data = self.flash.get(sector, b"\xff" * SECTOR_SIZE)
            return data[offset:offset + 16] + b"\x00" * 4

        if func == pio.MEMORY_ADDR_WRITE:
            sector, sub, count = struct.unpack(">HHH", params[:6])
            self._window = (sector, sub, count)
            self._buf = bytearray()
            return b"\x00" * 16

        if func == pio.MEMORY_WRITE:
            if self._window is None:
                raise HidppError(0x02, idx, func)  # no window open
            if len(params) != 16:
                raise AssertionError(f"chunk is {len(params)} bytes, want 16")
            if len(self._buf) + 16 > self._window[2]:
                raise HidppError(0x03, idx, func)  # past the window
            self._buf += params
            return b"\x00" * 16

        if func == pio.MEMORY_WRITE_END:
            if self._window is None:
                raise HidppError(0x02, idx, func)
            sector, sub, count = self._window
            if len(self._buf) != count:
                raise AssertionError(
                    f"committed {len(self._buf)} bytes, window wanted {count}")
            out = bytearray(self.flash.get(sector, b"\xff" * SECTOR_SIZE))
            out[sub:sub + count] = self._buf
            if sector in self.corrupt:
                off = self.corrupt[sector]
                out[off] ^= 0xFF
            self.flash[sector] = bytes(out)
            self._window = None
            return b"\x00" * 16

        raise AssertionError(f"unexpected function 0x{func:02x}")


class FakeProfiles:
    """Stands in for OnboardProfiles; reads go through the fake device."""

    def __init__(self, dev, info: FakeInfo | None = None):
        self.dev = dev
        self.idx = FEATURE_IDX
        self.info = info or FakeInfo()

    def read_sector(self, sector: int, length: int | None = None) -> bytes:
        length = length or self.info.sector_size
        out = bytearray(length)
        offset = 0
        while offset < length:
            if length - offset < 16:
                offset = length - 16
            p = self.dev.call(self.idx, pio.MEMORY_READ,
                              struct.pack(">HH", sector, offset))
            out[offset:offset + 16] = p[:16]
            offset += 16
        return bytes(out)


def fixture(sectors: int = 16, **info_over):
    flash = {s: make_sector(s * 7 + 1) for s in range(sectors)}
    dev = FakeDevice(flash)
    return dev, FakeProfiles(dev, FakeInfo(**info_over))


def write_calls(dev) -> list[Call]:
    return [c for c in dev.calls if c.func != pio.MEMORY_READ]


def read_calls(dev) -> list[Call]:
    return [c for c in dev.calls if c.func == pio.MEMORY_READ]


class TestCrcAssumption(unittest.TestCase):
    """profile_io trusts profile_blob.crc_ccitt; pin the algorithm."""

    def test_ccitt_false_check_value(self):
        # The documented check value for CRC-16/CCITT-FALSE.
        self.assertEqual(crc_ccitt(b"123456789"), 0x29B1)

    def test_stamp_crc_is_big_endian_at_0xfe(self):
        data = bytes(256)
        out = pio.stamp_crc(data)
        self.assertEqual(out[:OFF_CRC], data[:OFF_CRC])
        self.assertEqual(struct.unpack_from(">H", out, OFF_CRC)[0],
                         crc_ccitt(data[:OFF_CRC]))

    def test_stamp_crc_rejects_wrong_length(self):
        for n in (0, 255, 257):
            with self.assertRaises(ValueError):
                pio.stamp_crc(bytes(n))


class TestWriteSequence(unittest.TestCase):
    def test_exact_call_sequence(self):
        dev, profiles = fixture()
        payload = make_sector(0x40)
        pio.write_sector(dev, profiles, 3, payload, settle=0)

        want = [Call(pio.MEMORY_ADDR_WRITE, struct.pack(">HHH", 3, 0, 256), True)]
        want += [Call(pio.MEMORY_WRITE, payload[o:o + 16], True)
                 for o in range(0, 256, 16)]
        want.append(Call(pio.MEMORY_WRITE_END, b"", False))
        self.assertEqual(write_calls(dev), want)
        self.assertEqual(len(write_calls(dev)), 1 + 16 + 1)

    def test_write_end_is_a_short_report(self):
        dev, profiles = fixture()
        pio.write_sector(dev, profiles, 1, make_sector(5), settle=0)
        end = [c for c in dev.calls if c.func == pio.MEMORY_WRITE_END]
        self.assertEqual(len(end), 1)
        self.assertFalse(end[0].long, "write_end must go out as a short report")
        self.assertEqual(end[0].params, b"")
        # Everything else is a long report.
        self.assertTrue(all(c.long for c in dev.calls
                            if c.func != pio.MEMORY_WRITE_END))

    def test_chunks_are_contiguous_and_in_order(self):
        dev, profiles = fixture()
        payload = pio.write_sector(dev, profiles, 2, make_sector(9), settle=0)
        chunks = [c.params for c in dev.calls if c.func == pio.MEMORY_WRITE]
        self.assertEqual(b"".join(chunks), payload)
        self.assertTrue(all(len(c) == 16 for c in chunks))

    def test_readback_verification_happens(self):
        dev, profiles = fixture()
        pio.write_sector(dev, profiles, 4, make_sector(3), settle=0)
        reads = read_calls(dev)
        self.assertEqual([struct.unpack(">HH", c.params[:4]) for c in reads],
                         [(4, o) for o in range(0, 256, 16)])

    def test_flash_holds_the_written_bytes(self):
        dev, profiles = fixture()
        payload = pio.write_sector(dev, profiles, 6, make_sector(0x11), settle=0)
        self.assertEqual(dev.flash[6], payload)


class TestCrcStamping(unittest.TestCase):
    def test_caller_crc_is_ignored_and_recomputed(self):
        dev, profiles = fixture()
        body = bytes((i * 3) & 0xFF for i in range(OFF_CRC))
        bogus = body + b"\xde\xad"
        payload = pio.write_sector(dev, profiles, 1, bogus, settle=0)

        self.assertEqual(payload[:OFF_CRC], body)
        self.assertNotEqual(payload[OFF_CRC:], b"\xde\xad")
        self.assertEqual(struct.unpack_from(">H", payload, OFF_CRC)[0],
                         crc_ccitt(body))
        # The CRC must be in the bytes that actually went to the device.
        last_chunk = [c.params for c in dev.calls
                      if c.func == pio.MEMORY_WRITE][-1]
        self.assertEqual(last_chunk[-2:], payload[OFF_CRC:])

    def test_written_sector_validates_as_a_profile(self):
        dev, profiles = fixture()
        pio.write_sector(dev, profiles, 1, make_sector(2)[:OFF_CRC] + b"\x00\x00",
                         settle=0)
        self.assertTrue(Profile.parse(dev.flash[1]).crc_ok())


class TestRefusals(unittest.TestCase):
    def test_wrong_length_sends_nothing(self):
        dev, profiles = fixture()
        for n in (0, 128, 255, 257, 512):
            with self.assertRaises(ValueError):
                pio.write_sector(dev, profiles, 1, bytes(n), settle=0)
        self.assertEqual(dev.calls, [])

    def test_sector_out_of_range_sends_nothing(self):
        dev, profiles = fixture()
        for sector in (-1, 16, 255, 0x1000):
            with self.assertRaises(pio.UnsafeStateError):
                pio.write_sector(dev, profiles, sector, make_sector(1), settle=0)
        self.assertEqual(dev.calls, [])

    def test_unknown_profile_format_sends_nothing(self):
        dev, profiles = fixture(profile_format_id=3)
        with self.assertRaises(pio.UnsafeStateError):
            pio.write_sector(dev, profiles, 1, make_sector(1), settle=0)
        self.assertEqual(dev.calls, [])

    def test_unknown_memory_model_and_macro_format(self):
        for over in ({"memory_model_id": 2}, {"macro_format_id": 2},
                     {"sector_size": 512}):
            dev, profiles = fixture(**over)
            with self.assertRaises(pio.UnsafeStateError):
                pio.write_sector(dev, profiles, 1, make_sector(1), settle=0)
            self.assertEqual(dev.calls, [], over)

    def test_device_error_propagates(self):
        dev, profiles = fixture()
        dev.read_errors.add(5)
        with self.assertRaises(HidppError):
            pio.write_sector(dev, profiles, 5, make_sector(1), settle=0)


class TestVerifyFailure(unittest.TestCase):
    def test_mismatch_raises_with_the_offset(self):
        dev, profiles = fixture()
        dev.corrupt[2] = 0x37
        with self.assertRaises(pio.SectorVerifyError) as ctx:
            pio.write_sector(dev, profiles, 2, make_sector(8), settle=0)
        self.assertEqual(ctx.exception.sector, 2)
        self.assertEqual(ctx.exception.offset, 0x37)
        self.assertIn("0x37", str(ctx.exception))
        self.assertIn("restore_all", str(ctx.exception))

    def test_short_readback_raises(self):
        dev, profiles = fixture()
        full = profiles.read_sector

        def truncated(sector, length=None):  # firmware stopped answering
            return full(sector, length)[:128]

        profiles.read_sector = truncated
        with self.assertRaises(pio.SectorVerifyError):
            pio.write_sector(dev, profiles, 1, make_sector(4), settle=0)


class TestWriteProfile(unittest.TestCase):
    def test_profile_index_maps_to_sector_plus_one(self):
        for index in range(5):
            dev, profiles = fixture()
            pio.write_profile(dev, profiles, index,
                              Profile.parse(make_sector(index)))
            addr = write_calls(dev)[0]
            self.assertEqual(addr.func, pio.MEMORY_ADDR_WRITE)
            self.assertEqual(struct.unpack(">HHH", addr.params[:6]),
                             (index + 1, 0, 256))

    def test_out_of_range_profile_sends_nothing(self):
        dev, profiles = fixture()
        for index in (-1, 5, 10):
            with self.assertRaises(pio.UnsafeStateError):
                pio.write_profile(dev, profiles, index,
                                  Profile.parse(make_sector(1)))
        self.assertEqual(dev.calls, [])

    def test_edit_roundtrips_through_flash(self):
        dev, profiles = fixture()
        before = Profile.parse(dev.flash[2])  # profile 1 lives in sector 2
        before.name = "Blender"
        before.dpi = [800, 1600, 3200, 0, 0]
        before.report_rate_ms = 1
        pio.write_profile(dev, profiles, 1, before)

        after = Profile.parse(dev.flash[2])
        self.assertTrue(after.crc_ok())
        self.assertEqual(after.name, "Blender")
        self.assertEqual(after.dpi, [800, 1600, 3200, 0, 0])
        self.assertEqual(after.report_rate_hz, 1000)
        # Bytes the codec does not model are carried through untouched.
        self.assertEqual(after._original[OFF_NAME - 4:OFF_NAME],
                         dev.flash[2][OFF_NAME - 4:OFF_NAME])


class TestDirectory(unittest.TestCase):
    def test_layout_matches_libratbag(self):
        _, profiles = fixture()
        blob = pio.directory_sector(profiles, [True, False, True, True, True])
        self.assertEqual(blob[0:4], b"\x00\x01\x01\x00")
        self.assertEqual(blob[4:8], b"\x00\x02\x00\x00")   # profile 2 disabled
        self.assertEqual(blob[8:12], b"\x00\x03\x01\x00")
        self.assertEqual(blob[20:24], b"\xff\xff\x00\x00")  # terminator
        self.assertEqual(blob[24:OFF_CRC], b"\xff" * (OFF_CRC - 24))
        self.assertEqual(len(blob), SECTOR_SIZE)

    def test_default_enables_every_profile(self):
        _, profiles = fixture()
        blob = pio.directory_sector(profiles)
        self.assertEqual([blob[i * 4 + 2] for i in range(5)], [1] * 5)

    def test_refuses_to_disable_everything(self):
        _, profiles = fixture()
        with self.assertRaises(pio.UnsafeStateError):
            pio.directory_sector(profiles, [False] * 5)

    def test_wrong_flag_count(self):
        _, profiles = fixture()
        with self.assertRaises(ValueError):
            pio.directory_sector(profiles, [True, True])

    def test_write_directory_targets_sector_zero_with_valid_crc(self):
        dev, profiles = fixture()
        pio.write_directory(dev, profiles, [True] * 5)
        addr = write_calls(dev)[0]
        self.assertEqual(struct.unpack(">HHH", addr.params[:6]), (0, 0, 256))
        stored = dev.flash[0]
        self.assertEqual(struct.unpack_from(">H", stored, OFF_CRC)[0],
                         crc_ccitt(stored[:OFF_CRC]))


class TestDumpRestore(unittest.TestCase):
    def test_dump_is_json_serialisable_and_complete(self):
        dev, profiles = fixture()
        backup = pio.dump_all(dev, profiles)
        reloaded = json.loads(json.dumps(backup))

        self.assertEqual(reloaded["version"], pio.BACKUP_VERSION)
        self.assertEqual(reloaded["device"], "/dev/hidraw-fake")
        self.assertEqual(reloaded["layout"]["sector_count"], 16)
        self.assertEqual(sorted(int(k) for k in reloaded["sectors"]),
                         list(range(16)))
        self.assertIn("0", reloaded["sectors"])  # the directory is included
        for key, hexdata in reloaded["sectors"].items():
            self.assertEqual(bytes.fromhex(hexdata), dev.flash[int(key)])

    def test_unreadable_sector_is_recorded_not_fatal(self):
        dev, profiles = fixture()
        dev.read_errors.add(9)
        backup = pio.dump_all(dev, profiles)
        self.assertIsNone(backup["sectors"]["9"])
        self.assertIn("9", backup["errors"])

        dev.read_errors.clear()
        dev.flash[9] = make_sector(0xAA)
        written = pio.restore_all(dev, profiles, backup)
        self.assertNotIn(9, written)  # nothing to restore it from

    def test_restoring_an_unchanged_device_writes_nothing(self):
        dev, profiles = fixture()
        backup = pio.dump_all(dev, profiles)
        dev.calls.clear()
        self.assertEqual(pio.restore_all(dev, profiles, backup), [])
        self.assertEqual(write_calls(dev), [])

    def test_restore_puts_changed_sectors_back(self):
        dev, profiles = fixture()
        backup = pio.dump_all(dev, profiles)
        original = dict(dev.flash)

        dev.flash[3] = make_sector(0xF0)
        dev.flash[7] = make_sector(0xF1)
        written = pio.restore_all(dev, profiles, backup)

        self.assertEqual(written, [3, 7])
        self.assertEqual(dev.flash, original)

    def test_directory_is_written_last(self):
        dev, profiles = fixture()
        backup = pio.dump_all(dev, profiles)
        dev.flash[0] = make_sector(0xC0)
        dev.flash[1] = make_sector(0xC1)
        dev.flash[5] = make_sector(0xC5)

        self.assertEqual(pio.restore_all(dev, profiles, backup), [1, 5, 0])

    def test_restore_honours_a_sector_subset(self):
        dev, profiles = fixture()
        backup = pio.dump_all(dev, profiles)
        dev.flash[2] = make_sector(0xB2)
        dev.flash[4] = make_sector(0xB4)

        self.assertEqual(pio.restore_all(dev, profiles, backup, sectors=[4]), [4])
        self.assertNotEqual(dev.flash[2], bytes.fromhex(backup["sectors"]["2"]))

    def test_restore_refuses_a_foreign_layout(self):
        dev, profiles = fixture()
        backup = pio.dump_all(dev, profiles)
        backup["layout"]["sector_count"] = 8
        with self.assertRaises(pio.UnsafeStateError):
            pio.restore_all(dev, profiles, backup)
        backup["layout"]["sector_count"] = 16
        backup["layout"]["profile_format_id"] = 4
        with self.assertRaises(pio.UnsafeStateError):
            pio.restore_all(dev, profiles, backup)

    def test_restore_rejects_a_wrong_sized_sector(self):
        dev, profiles = fixture()
        backup = pio.dump_all(dev, profiles)
        backup["sectors"]["4"] = "00" * 128
        with self.assertRaises(ValueError):
            pio.restore_all(dev, profiles, backup)


class TestBackupFiles(unittest.TestCase):
    def test_save_and_load_roundtrip(self):
        dev, profiles = fixture()
        with tempfile.TemporaryDirectory() as tmp:
            path = pio.save_backup(dev, profiles, Path(tmp) / "b.json")
            self.assertTrue(path.exists())
            self.assertFalse(path.with_suffix(".tmp").exists())
            loaded = pio.load_backup(path)
        self.assertEqual(loaded["sectors"]["1"], dev.flash[1].hex())
        self.assertEqual(pio.restore_all(dev, profiles, loaded), [])

    def test_load_rejects_junk(self):
        with tempfile.TemporaryDirectory() as tmp:
            bad = Path(tmp) / "bad.json"
            bad.write_text('{"version": 1}')
            with self.assertRaises(ValueError):
                pio.load_backup(bad)
            old = Path(tmp) / "old.json"
            old.write_text('{"version": 0, "sectors": {}}')
            with self.assertRaises(ValueError):
                pio.load_backup(old)


if __name__ == "__main__":
    unittest.main(verbosity=2)
