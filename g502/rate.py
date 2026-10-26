"""0x8060 REPORT_RATE."""
from __future__ import annotations
from . import features as F

GET_RATE_LIST = 0x00
GET_RATE = 0x01
SET_RATE = 0x02


class ReportRate:
    def __init__(self, dev):
        self.dev = dev
        self.idx = dev.feature_index(F.REPORT_RATE)

    def supported(self) -> list[int]:
        """Bit N of the mask means an interval of (N+1) ms is supported."""
        mask = self.dev.call(self.idx, GET_RATE_LIST)[0]
        return sorted(1000 // (b + 1) for b in range(8) if mask & (1 << b))

    def get(self) -> int:
        ms = self.dev.call(self.idx, GET_RATE)[0]
        return 1000 // ms if ms else 0

    def set(self, hz: int) -> None:
        if hz not in self.supported():
            raise ValueError(f"{hz} Hz unsupported; have {self.supported()}")
        self.dev.call(self.idx, SET_RATE, bytes([1000 // hz]))
