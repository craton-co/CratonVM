#!/usr/bin/env bash
# A-B-B-A interleaved comparison of two cratonvm binaries on one probe.
#
# Interleaved, not sequential: this build host is shared, and a sequential
# "all of A then all of B" prices whatever else was running during each half.
# A-B-B-A puts each arm on both sides of the run's midpoint, so a monotone
# drift in host load cancels rather than landing on one arm.
#
#   ab-natfunnel.sh <probe-class> <arm-a-binary> <arm-b-binary> [rounds]
#
# Reports, per rung, the MINIMUM last-pass ns/op each arm reached across its
# two runs — the minimum because every source of error on a shared host is
# additive, so the smallest observation is the closest to the machine's real
# cost.
set -u
PROBE="${1:?probe class}"
A="${2:?arm A binary}"
B="${3:?arm B binary}"
ROUNDS="${4:-3}"
JDK=/home/victor/jdk25
CP=/data/data/natfunnel-out
OUT=/data/data/natfunnel-ab
mkdir -p "$OUT"
rm -f "$OUT"/*.txt

run() { # run <arm-label> <binary> <tag>
  "$2" --java-home "$JDK" -cp "$CP" "$PROBE" 2>/dev/null | grep -E '^[a-zA-Z]' >> "$OUT/$1.$3.txt"
}

for i in $(seq 1 "$ROUNDS"); do
  run A "$A" "$i"
  run B "$B" "$i"
  run B "$B" "$i"
  run A "$A" "$i"
  echo "round $i done" >&2
done

python3 - "$OUT" <<'EOF'
import sys, os, re, glob
out = sys.argv[1]
def collect(arm):
    best = {}
    for f in glob.glob(os.path.join(out, f'{arm}.*.txt')):
        for line in open(f):
            m = re.match(r'^(.{1,60}?)\s{2,}([-\d.]+)\s+([-\d.]+)\s+([-\d.]+)\s+([-\d.]+)\s*$', line.rstrip())
            if not m:
                continue
            label = m.group(1).strip()
            last = float(m.group(5))
            if label not in best or last < best[label]:
                best[label] = last
    return best
a, b = collect('A'), collect('B')
labels = [k for k in a if k in b]
print(f"{'rung':<44}{'A (min)':>11}{'B (min)':>11}{'B/A':>8}")
for k in labels:
    ratio = b[k] / a[k] if a[k] else float('nan')
    print(f"{k:<44}{a[k]:>11.1f}{b[k]:>11.1f}{ratio:>8.2f}")
EOF
