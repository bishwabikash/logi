"""0x2201 ADJUSTABLE_DPI."""
from __future__ import annotations
from . import features as F

GET_SENSOR_COUNT = 0x00
GET_SENSOR_DPI_LIST = 0x01
GET_SENSOR_DPI = 0x02
SET_SENSOR_DPI = 0x03

RANGE_MARKER = 0xE000  # high bits mark "step" rather than a discrete value


class Dpi:
    def __init__(self, dev):
        self.dev = dev
        self.idx = dev.feature_index(F.ADJUSTABLE_DPI)

    def sensor_count(self) -> int:
        return self.dev.call(self.idx, GET_SENSOR_COUNT)[0]

    def capability(self, sensor: int = 0) -> dict:
        """Decode the DPI descriptor.

        The list is uint16 values. A value with 0xE000 set is not a DPI but a
        step size, meaning the surrounding values describe a continuous range
        (min, step, max) instead of a discrete list. The HERO sensor uses the
        range form.
        """
        p = self.dev.call(self.idx, GET_SENSOR_DPI_LIST, bytes([sensor]))
        words = [(p[i] << 8) | p[i + 1] for i in range(1, 15, 2)]
        values, step = [], None
        for w in words:
            if w == 0:
                break
            if w & RANGE_MARKER == RANGE_MARKER:
                step = w & 0x1FFF
            else:
                values.append(w)
        if step and len(values) >= 2:
            return {"kind": "range", "min": values[0], "max": values[-1],
                    "step": step, "raw": words}
        return {"kind": "list", "values": values, "raw": words}

    def get(self, sensor: int = 0) -> int:
        p = self.dev.call(self.idx, GET_SENSOR_DPI, bytes([sensor]))
        return (p[1] << 8) | p[2]

    def default(self, sensor: int = 0) -> int:
        p = self.dev.call(self.idx, GET_SENSOR_DPI, bytes([sensor]))
        return (p[3] << 8) | p[4]

    def set(self, dpi: int, sensor: int = 0) -> int:
        cap = self.capability(sensor)
        if cap["kind"] == "range":
            lo, hi, st = cap["min"], cap["max"], cap["step"]
            if not (lo <= dpi <= hi):
                raise ValueError(f"dpi {dpi} outside {lo}-{hi}")
            # snap to the nearest representable step
            dpi = lo + round((dpi - lo) / st) * st
        elif dpi not in cap["values"]:
            raise ValueError(f"dpi {dpi} not in {cap['values']}")
        self.dev.call(self.idx, SET_SENSOR_DPI,
                      bytes([sensor, dpi >> 8, dpi & 0xFF]))
        return dpi
