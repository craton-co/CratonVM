"""Robust A/B summary for `ab-natfunnel.sh` output.

Why not the minimum. With 4 rounds the minimum was the right estimator and the
risk was undersampling. With 10 rounds the opposite risk dominates: 20 samples
per arm is enough that ONE fluke-quiet window sets the minimum, and two rungs
here show exactly that (arm B's `Atomic.CAS` reads 161.6 against a 299.7-374
bulk; its `identityHashCode` reads 101.0 against 181-230). A ratio of minima
then reports the fluke — 0.47 and 0.50 — instead of the change.

So report the MEDIAN as the headline and the minimum beside it. The median of 20
is insensitive to one outlier at either end, and on this host the untouched
control rungs are what calibrate it: they should read ~1.00, and any touched-rung
move has to clear whatever spread they show.
"""
import sys, os, re, glob, statistics


def collect(out, arm):
    obs = {}
    for f in sorted(glob.glob(os.path.join(out, f'{arm}.*.txt'))):
        for line in open(f):
            m = re.match(r'^(.{1,60}?)\s{2,}([-\d.]+)\s+([-\d.]+)\s+([-\d.]+)\s+([-\d.]+)\s*$',
                         line.rstrip())
            if m:
                obs.setdefault(m.group(1).strip(), []).append(float(m.group(5)))
    return obs


out = sys.argv[1]
# Rungs this branch does not touch: the noise floor is whatever THEY show.
CONTROLS = ('control:', 'onSpinWait', 'String.length', 'currentThread')
a, b = collect(out, 'A'), collect(out, 'B')

rows = []
for k in [k for k in a if k in b]:
    av, bv = sorted(a[k]), sorted(b[k])
    med = statistics.median(bv) / statistics.median(av)
    rows.append((k, statistics.median(av), statistics.median(bv), med,
                 av[0], bv[0], bv[0] / av[0], len(av),
                 any(c in k for c in CONTROLS)))

floor = [r[3] for r in rows if r[8]]
print(f"{'rung':<44}{'A med':>8}{'B med':>8}{'B/A med':>9}{'A min':>8}{'B min':>8}{'B/A min':>9}")
for k, am, bm, mr, ai, bi, ir, n, ctl in rows:
    tag = "  <- untouched control" if ctl else ""
    print(f"{k:<44}{am:>8.1f}{bm:>8.1f}{mr:>9.2f}{ai:>8.1f}{bi:>8.1f}{ir:>9.2f}{tag}")
print()
print(f"n = {rows[0][7]} observations per arm per rung")
print(f"control-rung median ratios: {[round(x, 2) for x in floor]}"
      f"  -> spread {min(floor):.2f}-{max(floor):.2f} is this run's noise floor")
moved = [(k, mr) for k, _, _, mr, _, _, _, _, ctl in rows if not ctl]
print("touched rungs, median ratio: " + ", ".join(f"{k.split(':')[-1].strip()} {mr:.2f}"
                                                 for k, mr in moved))
