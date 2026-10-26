"""HID++ 2.0 transport for wired Logitech devices.

Speaks the vendor-defined ff00.0001 (short, 7 byte) and ff00.0002 (long,
20 byte) reports that Logitech's own software uses. Everything here is
synchronous request/response with a software-id tag so we can tell our
own replies apart from unsolicited event notifications.
"""
from __future__ import annotations

import errno
import os
import select
import time

SHORT_REPORT = 0x10
LONG_REPORT = 0x11
VERY_LONG_REPORT = 0x12

SHORT_LEN = 7
LONG_LEN = 20
VERY_LONG_LEN = 64

RECEIVER_IDX = 0xFF  # wired / direct attach
SOFTWARE_ID = 0x08   # our tag in the low nibble of byte 3

ERR_NAMES = {
    0x00: "no error",
    0x01: "unknown",
    0x02: "invalid argument",
    0x03: "out of range",
    0x04: "hardware error",
    0x05: "logitech internal",
    0x06: "invalid feature index",
    0x07: "invalid function id",
    0x08: "busy",
    0x09: "unsupported",
}


class HidppError(Exception):
    def __init__(self, code: int, feature: int, function: int):
        self.code = code
        self.feature = feature
        self.function = function
        super().__init__(
            f"HID++ error 0x{code:02x} ({ERR_NAMES.get(code, '?')}) "
            f"on feature idx 0x{feature:02x} func 0x{function:x}"
        )


class HidppTimeout(Exception):
    pass


class Device:
    """One open HID++ control endpoint."""

    def __init__(self, path: str, device_index: int = RECEIVER_IDX):
        self.path = path
        self.device_index = device_index
        self._fd = os.open(path, os.O_RDWR | os.O_NONBLOCK)
        self._feature_cache: dict[int, int] = {}

    # -- lifecycle ---------------------------------------------------------
    def close(self) -> None:
        if self._fd is not None:
            os.close(self._fd)
            self._fd = None

    def __enter__(self) -> "Device":
        return self

    def __exit__(self, *exc) -> None:
        self.close()

    # -- raw io ------------------------------------------------------------
    def _drain(self, window: float = 0.01) -> None:
        """Discard anything already queued so a stale event isn't mistaken
        for our reply."""
        while select.select([self._fd], [], [], window)[0]:
            try:
                if not os.read(self._fd, VERY_LONG_LEN):
                    break
            except OSError as e:
                if e.errno in (errno.EAGAIN, errno.EWOULDBLOCK):
                    break
                raise

    def call(
        self,
        feature_index: int,
        function: int,
        params: bytes = b"",
        long: bool = True,
        timeout: float = 0.8,
    ) -> bytes:
        """Issue one request and return its payload (bytes 4..end).

        Raises HidppError on a device-reported error, HidppTimeout if the
        device never answers.
        """
        report = LONG_REPORT if long else SHORT_REPORT
        size = LONG_LEN if long else SHORT_LEN
        head = bytes(
            [report, self.device_index, feature_index, (function << 4) | SOFTWARE_ID]
        )
        if len(head) + len(params) > size:
            raise ValueError(f"params too long for {'long' if long else 'short'} report")
        packet = head + params + b"\x00" * (size - len(head) - len(params))

        self._drain()
        os.write(self._fd, packet)

        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            if not select.select([self._fd], [], [], 0.05)[0]:
                continue
            try:
                resp = os.read(self._fd, VERY_LONG_LEN)
            except OSError as e:
                if e.errno in (errno.EAGAIN, errno.EWOULDBLOCK):
                    continue
                raise
            if len(resp) < 4 or resp[0] not in (SHORT_REPORT, LONG_REPORT, VERY_LONG_REPORT):
                continue
            # Error reports come back as feature 0xFF with the original
            # feature/function echoed in the payload.
            if resp[2] == 0xFF:
                raise HidppError(resp[5] if len(resp) > 5 else resp[3],
                                 feature_index, function)
            # Match our own software id so events don't satisfy the wait.
            if (
                resp[2] == feature_index
                and (resp[3] >> 4) == function
                and (resp[3] & 0x0F) == SOFTWARE_ID
            ):
                return resp[4:]
        raise HidppTimeout(
            f"no reply for feature idx 0x{feature_index:02x} func 0x{function:x}"
        )

    def send_nowait(
        self,
        feature_index: int,
        function: int,
        params: bytes = b"",
        long: bool = True,
    ) -> None:
        """Fire a request without waiting for its reply.

        Animation pushes a frame every few milliseconds; waiting for each
        round trip caps throughput at roughly 20 fps. The device still
        queues a response, so the caller must drain periodically or the
        replies accumulate. Only use for idempotent writes where a dropped
        frame does not matter.
        """
        report = LONG_REPORT if long else SHORT_REPORT
        size = LONG_LEN if long else SHORT_LEN
        head = bytes(
            [report, self.device_index, feature_index, (function << 4) | SOFTWARE_ID]
        )
        packet = head + params + b"\x00" * (size - len(head) - len(params))
        try:
            os.write(self._fd, packet)
        except BlockingIOError:
            pass  # kernel queue full: drop this frame rather than stall

    def flush(self) -> None:
        """Discard queued replies left by send_nowait."""
        self._drain()

    # -- root feature ------------------------------------------------------
    def feature_index(self, feature_id: int) -> int:
        """Resolve a feature id (e.g. 0x8070) to its index on this device."""
        if feature_id in self._feature_cache:
            return self._feature_cache[feature_id]
        payload = self.call(0x00, 0x00, bytes([feature_id >> 8, feature_id & 0xFF]))
        idx = payload[0]
        if idx == 0:
            raise KeyError(f"device does not support feature 0x{feature_id:04x}")
        self._feature_cache[feature_id] = idx
        return idx

    def has_feature(self, feature_id: int) -> bool:
        try:
            self.feature_index(feature_id)
            return True
        except (KeyError, HidppError, HidppTimeout):
            return False

    def protocol_version(self) -> tuple[int, int]:
        p = self.call(0x00, 0x01)
        return p[0], p[1]
