#!/usr/bin/env bash
# Does host mode actually stop the buttons working, or was something else
# going on? Watches the kernel's input events for the mouse in each mode.
#
# Read-only on the input device; the only thing it changes is the memory mode,
# and it always puts that back to onboard at the end, even on Ctrl-C.
set -uo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/.."
G=./target/release/g502

EV=$(grep -A5 'Name="Logitech G502 HERO Gaming Mouse"' /proc/bus/input/devices \
     | grep -m1 Handlers | grep -o 'event[0-9]*')
[ -z "$EV" ] && { echo "could not find the mouse's event node"; exit 1; }
echo "watching /dev/$EV"

restore() { echo; echo "restoring onboard mode..."; $G mode onboard; }
trap restore EXIT INT TERM

watch_for() {
  sudo python3 - "$1" "$2" <<'PY'
import struct, select, sys, time
secs=float(sys.argv[2]); node=sys.argv[1]
FMT="llHHi"; SZ=struct.calcsize(FMT)
BTN={0x110:"left",0x111:"right",0x112:"middle",0x113:"side",0x114:"extra",
     0x115:"forward",0x116:"back",0x117:"task"}
keys=set(); rel=0
with open(node,"rb",buffering=0) as f:
    end=time.time()+secs
    while time.time()<end:
        if select.select([f],[],[],0.2)[0]:
            d=f.read(SZ)
            if not d or len(d)<SZ: continue
            _,_,t,c,v=struct.unpack(FMT,d)
            if t==0x01 and v==1: keys.add(BTN.get(c,hex(c)))
            elif t==0x02: rel+=1
print(f"    movement events : {rel}")
print(f"    buttons seen    : {', '.join(sorted(keys)) if keys else 'NONE'}")
PY
}

echo
echo "=== 1/2  ONBOARD mode (the baseline) ==="
$G mode onboard >/dev/null
echo "    MOVE the mouse and CLICK every button you can, for 8 seconds. Go:"
watch_for "/dev/$EV" 8

echo
echo "=== 2/2  HOST mode ==="
$G mode host >/dev/null
echo "    Do exactly the same again, for 8 seconds. Go:"
watch_for "/dev/$EV" 8
