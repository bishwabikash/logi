"""0x8100 ONBOARD_PROFILES — mode, profile switching, profile memory.

Function 1 is setOnboardMode, a *setter*. Calling it expecting a read will
write the mode. Use mode() to read (function 2).
"""
from __future__ import annotations

import contextlib
import struct
import time

from . import features as F
from .hidpp import HidppError, HidppTimeout

GET_INFO = 0x00
SET_MODE = 0x01
GET_MODE = 0x02
SET_CURRENT_PROFILE = 0x03
GET_CURRENT_PROFILE = 0x04
MEMORY_READ = 0x05
MEMORY_ADDR_WRITE = 0x06
MEMORY_WRITE = 0x07
MEMORY_WRITE_END = 0x08
GET_CURRENT_DPI_INDEX = 0x0B
SET_CURRENT_DPI_INDEX = 0x0C

MODE_ONBOARD = 0x01   # device drives itself from stored profiles
MODE_HOST = 0x02      # host drives it; required for live 0x8070 lighting
MODE_NAMES = {MODE_ONBOARD: "onboard", MODE_HOST: "host"}


class ProfilesInfo:
    __slots__ = ("memory_model_id", "profile_format_id", "macro_format_id",
                 "profile_count", "profile_count_oob", "button_count",
                 "sector_count", "sector_size", "mechanical_layout",
                 "various_info")

    def __init__(self, p: bytes):
        (self.memory_model_id, self.profile_format_id, self.macro_format_id,
         self.profile_count, self.profile_count_oob, self.button_count,
         self.sector_count, self.sector_size, self.mechanical_layout,
         self.various_info) = struct.unpack_from(">BBBBBBBHBB", p, 0)

    def __repr__(self) -> str:
        return (f"<Profiles fmt={self.profile_format_id} "
                f"macro_fmt={self.macro_format_id} "
                f"profiles={self.profile_count}+{self.profile_count_oob}rom "
                f"buttons={self.button_count} "
                f"sectors={self.sector_count}x{self.sector_size}B>")


class OnboardProfiles:
    def __init__(self, dev):
        self.dev = dev
        self.idx = dev.feature_index(F.ONBOARD_PROFILES)
        self.info = ProfilesInfo(self.dev.call(self.idx, GET_INFO))

    # -- mode --------------------------------------------------------------
    def mode(self) -> int:
        return self.dev.call(self.idx, GET_MODE)[0]

    def set_mode(self, mode: int) -> None:
        if mode not in (MODE_ONBOARD, MODE_HOST):
            raise ValueError("mode must be MODE_ONBOARD(1) or MODE_HOST(2)")
        self.dev.call(self.idx, SET_MODE, bytes([mode]))
        time.sleep(0.05)  # firmware needs a moment before it honours writes

    @contextlib.contextmanager
    def host_mode(self):
        """Hold the device in host mode, then put it back.

        Leaving the mouse in host mode makes its lighting depend on this
        process still running, so the restore is unconditional.
        """
        previous = self.mode()
        if previous != MODE_HOST:
            self.set_mode(MODE_HOST)
        try:
            yield self
        finally:
            with contextlib.suppress(HidppError, HidppTimeout, OSError):
                self.set_mode(previous if previous in
                              (MODE_ONBOARD, MODE_HOST) else MODE_ONBOARD)

    # -- profile selection -------------------------------------------------
    def current(self) -> int:
        """Active profile, 0-based."""
        p = self.dev.call(self.idx, GET_CURRENT_PROFILE)
        # byte 0 is the memory bank, byte 1 the 1-based profile index
        return max(0, p[1] - 1)

    def set_current(self, index: int) -> None:
        if not (0 <= index < self.info.profile_count):
            raise ValueError(
                f"profile {index} out of range 0..{self.info.profile_count - 1}")
        self.dev.call(self.idx, SET_CURRENT_PROFILE, bytes([0x00, index + 1]))

    def dpi_index(self) -> int:
        return self.dev.call(self.idx, GET_CURRENT_DPI_INDEX)[0]

    def set_dpi_index(self, i: int) -> None:
        self.dev.call(self.idx, SET_CURRENT_DPI_INDEX, bytes([i]))

    # -- raw profile memory ------------------------------------------------
    def read_sector(self, sector: int, length: int | None = None) -> bytes:
        """Read one profile sector. Returns sector_size bytes by default.

        Reads come back 16 bytes at a time via long reports.
        """
        length = length or self.info.sector_size
        out = bytearray(length)
        offset = 0
        while offset < length:
            # The firmware rejects a read that would run past length-16, so
            # the tail is re-read from length-16 and overlaps what we have.
            if length - offset < 16:
                offset = length - 16
            p = self.dev.call(self.idx, MEMORY_READ, struct.pack(
                ">HH", sector, offset))
            out[offset:offset + 16] = p[:16]
            offset += 16
        return bytes(out)

    def write_sector(self, sector: int, data: bytes,
                     verify: bool = True) -> None:
        """Write one profile sector.

        `data` must already carry a valid trailing CRC — build it with
        profile_blob.Profile.serialize(). A sector with a bad CRC is
        rejected by the firmware and can leave the profile unusable, so the
        CRC is checked here before anything is sent.
        """
        from .profile_blob import crc_ccitt

        if len(data) != self.info.sector_size:
            raise ValueError(
                f"sector must be exactly {self.info.sector_size} bytes, "
                f"got {len(data)}")
        stored = (data[-2] << 8) | data[-1]
        computed = crc_ccitt(data[:-2])
        if stored != computed:
            raise ValueError(
                f"refusing to write: CRC 0x{stored:04x} but data computes "
                f"0x{computed:04x}. Serialize through Profile.serialize().")

        self.dev.call(self.idx, MEMORY_ADDR_WRITE,
                      struct.pack(">HHH", sector, 0, len(data)))
        for off in range(0, len(data), 16):
            self.dev.call(self.idx, MEMORY_WRITE, data[off:off + 16])
        self.dev.call(self.idx, MEMORY_WRITE_END, b"", long=False)
        time.sleep(0.05)

        if verify:
            back = self.read_sector(sector)
            if back != data:
                raise RuntimeError(
                    f"sector {sector} read back different after write")
