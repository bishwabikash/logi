# no tool: command -v ratbagctl -> /usr/bin/ratbagctl (ratbagd 0.18) + ratbagctl --help -> high-level profile/dpi/led edits only; no raw sector read/write, no CRC stamping, no byte-exact backup of all 16 sectors. Not coverage for this module, and this project is a from-scratch driver rather than a ratbagd client.
"""0x8100 onboard-profile write path, with backup and restore.

These are PERSISTENT FLASH writes to the mouse's onboard memory, which has
finite write endurance. Nothing here belongs in a loop or on a slider
callback: assemble the whole sector, write it once, verify it. Live,
transient changes belong in led.py / dpi.py / rate.py, which touch no flash.
Take a backup (dump_all, or save_backup) before the first write of a session;
restore_all puts it back.

The sequence and its argument layout are not guessed — they are taken from
libratbag src/hidpp20.c, hidpp20_onboard_profiles_write_sector plus the three
static helpers it calls. The CMD_* addresses there are `function << 4`, so
CMD_ONBOARD_PROFILES_MEMORY_ADDR_WRITE 0x60 is function 6. Confirmed from
that source:

  * write_start -> MEMORY_ADDR_WRITE, long report, params are
    `be16 sector | be16 sub_address | be16 count`
    (set_unaligned_be_u16 at parameters[0], [2], [4]), called with
    sub_address 0 and count = sector_size.
  * write_data -> MEMORY_WRITE, long report, params are 16 payload bytes and
    nothing else (`memcpy(msg.msg.parameters, data, 16)`). The window opened
    above carries the address, so chunks must be pushed in order, no gaps.
  * write_end -> MEMORY_WRITE_END, *short* report, no params
    (`.msg.report_id = REPORT_ID_SHORT`, unlike every other call here).
  * CRC -> `crc = hidpp_crc_ccitt(data, sector_size - 2);
    set_unaligned_be_u16(&data[sector_size - 2], crc);` i.e. big-endian over
    everything but the trailing two bytes.
  * profile N lives in sector N+1 (`uint16_t sector = index + 1;` in
    hidpp20_onboard_profiles_write_profile). Sector 0 is the profile
    directory (HIDPP20_USER_PROFILES_G402 == 0x0000).

A sector whose CRC does not match is discarded by the firmware, which then
falls back to the ROM profiles (libratbag's initialize() does exactly that on
a bad directory CRC). So the CRC is always recomputed here and never taken
from the caller, and every write is read back and compared before the call
returns.
"""
from __future__ import annotations

import json
import os
import struct
import time
from datetime import datetime
from pathlib import Path

from .hidpp import HidppError, HidppTimeout
from .profile_blob import OFF_CRC, SECTOR_SIZE, Profile, crc_ccitt

# 0x8100 function codes. Duplicated from profiles.py on purpose: the write
# path should not change meaning because another module renamed a constant.
MEMORY_READ = 0x05
MEMORY_ADDR_WRITE = 0x06
MEMORY_WRITE = 0x07
MEMORY_WRITE_END = 0x08

CHUNK = 16            # bytes of payload per long report
DIRECTORY_SECTOR = 0  # sector 0 is the profile directory, not a profile
DIR_ENTRY = 4         # bytes per directory entry
DIR_END = 0xFFFF      # HIDPP20_PROFILE_DIR_END

# What profile_blob.py and the directory layout below actually model. Mirrors
# hidpp20_onboard_profiles_validate(), narrowed to this device: the G502 HERO
# reports memory model 1, profile format 2, macro format 1.
MEMORY_MODEL_ID = 1
PROFILE_FORMAT_ID = 2
MACRO_FORMAT_ID = 1

BACKUP_VERSION = 1
BACKUP_DIR = Path(
    os.environ.get("XDG_DATA_HOME", Path.home() / ".local" / "share")
) / "g502"

# Flash needs a moment after the commit; a readback issued immediately can
# still answer with the pre-write contents.
SETTLE = 0.05


class ProfileWriteError(Exception):
    """Base for every refusal or failure in the flash write path."""


class UnsafeStateError(ProfileWriteError):
    """The device or the request is not in a state it is safe to write in."""


class SectorVerifyError(ProfileWriteError):
    """A sector read back different from what was written."""

    def __init__(self, sector: int, wrote: bytes, read: bytes):
        self.sector = sector
        self.wrote = wrote
        self.read = read
        self.offset = next(
            (i for i, (a, b) in enumerate(zip(wrote, read)) if a != b),
            min(len(wrote), len(read)),
        )
        detail = (
            f"wrote 0x{wrote[self.offset]:02x}, read back 0x{read[self.offset]:02x}"
            if self.offset < min(len(wrote), len(read))
            else f"readback is {len(read)} bytes, wrote {len(wrote)}"
        )
        super().__init__(
            f"sector {sector} verify failed at byte 0x{self.offset:02x}: "
            f"{detail}. The sector may be corrupt; restore the backup with "
            f"restore_all()."
        )


# -- preflight -------------------------------------------------------------
def check_writable(profiles) -> None:
    """Refuse to touch flash unless the memory layout is the one we model.

    profile_blob.py decodes exactly profile_format_id 2. Writing a sector
    built to that layout into a device with a different one would produce a
    valid CRC over wrong fields, which is worse than a rejected write.
    """
    info = profiles.info
    if info.memory_model_id != MEMORY_MODEL_ID:
        raise UnsafeStateError(
            f"memory model {info.memory_model_id} not supported "
            f"(expected {MEMORY_MODEL_ID})")
    if info.profile_format_id != PROFILE_FORMAT_ID:
        raise UnsafeStateError(
            f"profile format {info.profile_format_id} not supported; "
            f"profile_blob.py models format {PROFILE_FORMAT_ID}")
    if info.macro_format_id != MACRO_FORMAT_ID:
        raise UnsafeStateError(
            f"macro format {info.macro_format_id} not supported "
            f"(expected {MACRO_FORMAT_ID})")
    if info.sector_size != SECTOR_SIZE:
        raise UnsafeStateError(
            f"device sector size {info.sector_size} != {SECTOR_SIZE}")


def stamp_crc(data: bytes) -> bytes:
    """Return `data` with a freshly computed trailing CRC.

    The caller's last two bytes are overwritten unconditionally: a sector
    carrying a stale CRC is silently ignored by the firmware.
    """
    if len(data) != SECTOR_SIZE:
        raise ValueError(
            f"sector must be exactly {SECTOR_SIZE} bytes, got {len(data)}")
    buf = bytearray(data)
    struct.pack_into(">H", buf, OFF_CRC, crc_ccitt(bytes(buf[:OFF_CRC])))
    return bytes(buf)


# -- the write -------------------------------------------------------------
def write_sector(dev, profiles, sector: int, data: bytes,
                 settle: float = SETTLE) -> bytes:
    """Write one 256-byte sector to onboard flash and verify the readback.

    `data` is stamped with a correct CRC first; whatever the caller put in
    the last two bytes is ignored. Returns the bytes actually written.

    Raises UnsafeStateError before sending anything if the sector index or
    the device layout is wrong, HidppError/HidppTimeout if the device refuses
    a step, and SectorVerifyError if the readback differs. A persistent flash
    write: do not call this repeatedly.
    """
    check_writable(profiles)
    if not (0 <= sector < profiles.info.sector_count):
        raise UnsafeStateError(
            f"sector {sector} out of range 0..{profiles.info.sector_count - 1}")
    payload = stamp_crc(data)

    idx = profiles.idx
    dev.call(idx, MEMORY_ADDR_WRITE, struct.pack(">HHH", sector, 0, len(payload)))
    for off in range(0, len(payload), CHUNK):
        dev.call(idx, MEMORY_WRITE, payload[off:off + CHUNK])
    # Short report here, matching libratbag's write_end.
    dev.call(idx, MEMORY_WRITE_END, b"", long=False)

    if settle:
        time.sleep(settle)
    back = profiles.read_sector(sector)
    if back != payload:
        raise SectorVerifyError(sector, payload, back)
    return payload


def write_profile(dev, profiles, index: int, profile: Profile) -> bytes:
    """Write onboard profile `index` (0-based) into sector index+1."""
    if not (0 <= index < profiles.info.profile_count):
        raise UnsafeStateError(
            f"profile {index} out of range 0..{profiles.info.profile_count - 1}")
    return write_sector(dev, profiles, index + 1, profile.serialize())


def directory_sector(profiles, enabled: list[bool] | None = None) -> bytes:
    """Build the sector-0 directory blob.

    Layout from hidpp20_onboard_profiles_write_dict: one 4-byte entry per
    profile, `be16 address | enabled | 0x00`, where the address of profile i
    is 0x0000 | (i + 1) — the same sector number write_profile uses. The list
    is terminated by FF FF (DIR_END) 00 00 and padded with 0xFF.

    An entry with enabled == 0 makes the device skip that profile when
    cycling; a directory with every entry disabled leaves the mouse with no
    usable profile, so at least one must stay enabled.
    """
    n = profiles.info.profile_count
    if enabled is None:
        enabled = [True] * n
    if len(enabled) != n:
        raise ValueError(f"expected {n} enabled flags, got {len(enabled)}")
    if not any(enabled):
        raise UnsafeStateError("refusing to disable every profile")
    buf = bytearray(b"\xff" * SECTOR_SIZE)
    for i, on in enumerate(enabled):
        struct.pack_into(">HBB", buf, i * DIR_ENTRY, i + 1, 1 if on else 0, 0)
    struct.pack_into(">HH", buf, n * DIR_ENTRY, DIR_END, 0x0000)
    return bytes(buf)


def write_directory(dev, profiles, enabled: list[bool] | None = None) -> bytes:
    """Rewrite the profile directory, enabling/disabling profiles.

    The riskiest single write on the device: a wrong directory hides every
    profile. The firmware falls back to the ROM profiles rather than
    bricking, but the user's stored profiles stay invisible until it is
    rewritten. Back up first.
    """
    return write_sector(dev, profiles, DIRECTORY_SECTOR,
                        directory_sector(profiles, enabled))


# -- backup / restore ------------------------------------------------------
def dump_all(dev, profiles) -> dict:
    """Read every sector of onboard memory into a JSON-serialisable dict.

    Sector 0 (the directory) is included. Sectors are hex strings because
    JSON has no byte type; a sector the device refuses to read (never
    written, so ERR_INVALID_ARGUMENT on some firmware) is recorded as null
    with the reason under "errors", and restore_all skips it.

    This is the safety net. Call it before any write.
    """
    info = profiles.info
    sectors: dict[str, str | None] = {}
    errors: dict[str, str] = {}
    for sector in range(info.sector_count):
        try:
            sectors[str(sector)] = profiles.read_sector(sector).hex()
        except (HidppError, HidppTimeout) as e:
            sectors[str(sector)] = None
            errors[str(sector)] = str(e)
    return {
        "version": BACKUP_VERSION,
        "created": datetime.now().astimezone().isoformat(timespec="seconds"),
        "device": getattr(dev, "path", None),
        "layout": {
            "memory_model_id": info.memory_model_id,
            "profile_format_id": info.profile_format_id,
            "macro_format_id": info.macro_format_id,
            "profile_count": info.profile_count,
            "sector_count": info.sector_count,
            "sector_size": info.sector_size,
        },
        "sectors": sectors,
        "errors": errors,
    }


def save_backup(dev, profiles, path: Path | str | None = None) -> Path:
    """dump_all() to a JSON file; returns where it landed."""
    backup = dump_all(dev, profiles)
    if path is None:
        stamp = datetime.now().strftime("%Y%m%d-%H%M%S")
        path = BACKUP_DIR / f"profiles-{stamp}.json"
    path = Path(path)
    path.parent.mkdir(parents=True, exist_ok=True)
    tmp = path.with_suffix(".tmp")
    with open(tmp, "w") as fh:
        json.dump(backup, fh, indent=2)
        fh.write("\n")
    tmp.replace(path)  # atomic: never leave a half-written backup
    return path


def load_backup(path: Path | str) -> dict:
    with open(path) as fh:
        backup = json.load(fh)
    if not isinstance(backup, dict) or "sectors" not in backup:
        raise ValueError(f"{path} is not a g502 profile backup")
    if backup.get("version") != BACKUP_VERSION:
        raise ValueError(
            f"backup version {backup.get('version')} != {BACKUP_VERSION}")
    return backup


def restore_all(dev, profiles, backup: dict,
                sectors: list[int] | None = None) -> list[int]:
    """Push a dump_all()/load_backup() dict back onto the device.

    Returns the sectors actually written. A sector whose contents already
    match is skipped, so restoring an unchanged device spends no flash
    cycles. The directory (sector 0) goes last: until it is rewritten it
    still describes the old profiles, so an interrupted restore never leaves
    the directory pointing at a half-written profile.
    """
    check_writable(profiles)
    layout = backup.get("layout") or {}
    info = profiles.info
    for key, mine in (("profile_format_id", info.profile_format_id),
                      ("sector_count", info.sector_count),
                      ("sector_size", info.sector_size)):
        theirs = layout.get(key)
        if theirs is not None and theirs != mine:
            raise UnsafeStateError(
                f"backup {key} {theirs} != this device's {mine}; "
                f"refusing to restore across layouts")

    wanted = list(range(info.sector_count)) if sectors is None else list(sectors)
    # Directory last; see docstring.
    order = sorted(s for s in wanted if s != DIRECTORY_SECTOR)
    if DIRECTORY_SECTOR in wanted:
        order.append(DIRECTORY_SECTOR)

    written: list[int] = []
    for sector in order:
        hexdata = backup["sectors"].get(str(sector))
        if not hexdata:
            continue  # unreadable at dump time, or not in this backup
        data = bytes.fromhex(hexdata)
        if len(data) != SECTOR_SIZE:
            raise ValueError(
                f"backup sector {sector} is {len(data)} bytes, "
                f"expected {SECTOR_SIZE}")
        try:
            if profiles.read_sector(sector) == data:
                continue  # identical: spend no flash cycles on it
        except (HidppError, HidppTimeout):
            pass  # unreadable now; write it and let the verify decide
        write_sector(dev, profiles, sector, data)
        written.append(sector)
    return written
