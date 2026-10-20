#!/usr/bin/env bash
# A/B one GPU benchmark across the two driver backends: cudarc (`gpu-driver`)
# and NVlabs cuda-core (`gpu-driver-oxide`).
#
#   CV_CUDARC=... CV_OXIDE=... bash bench-gpu/backend-ab.sh [class] [n] [iters]
#
# Defaults to GpuAllocChurn, the shape where a difference is actually
# predicted (see that file). GpuTransferFloor is the other useful class.
#
# ── THREE THINGS THIS SCRIPT EXISTS TO PREVENT ────────────────────────────
#
# 1. A CROSS-BINARY A/B THAT PRETENDS NOT TO BE ONE. The backend is a
#    compile-time choice; there is no runtime switch, so the arms are
#    necessarily two binaries. Both must come from the SAME commit and the
#    same `cargo build` invocation modulo one feature flag. Even then, read
#    the result as "these two builds differ by this much".
#
# 2. A MEASUREMENT WITH NO NOISE FLOOR. The `ctl` arm runs the SAME cudarc
#    binary a second time inside the same round. Its ratio against `cudarc`
#    is what this box can resolve. If oxide/cudarc is not clearly outside
#    ctl/cudarc, the run has NOT resolved a difference and must not be
#    reported as one.
#
#    This is not hypothetical. On 2026-09-04 a single unpaired run of
#    GpuAllocChurn showed oxide 34% slower -- exactly the direction the
#    alloc-pool difference predicts. Under pairing with a rotated control it
#    came back +0.4% (95% CI [-4.4%, +5.1%]). The mechanism is real; the
#    effect was not.
#
# 3. AN ORDER EFFECT READ AS A RESULT. Arm order rotates through all three
#    permutations. Use a ROUNDS that is a MULTIPLE OF 3 so every arm spends
#    an equal number of runs in each slot: at 8 rounds the slots looked
#    monotonically slower (1.338/1.381/1.402 ms) and that drift vanished at
#    21 rounds. A fixed order manufactures a win for whichever arm runs
#    while the box is quietest.
#
# Gate this behind bench-gpu/wait-for-quiet.sh; absolute numbers on a shared
# box are meaningless, and even ratios widen a lot under a neighbouring
# build.
set -u
export MSYS2_ARG_CONV_EXCL='*' MSYS_NO_PATHCONV=1

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$HERE/.." && pwd)"

CV_CUDARC="${CV_CUDARC:-$ROOT/cratonvm-cudarc.exe}"
CV_OXIDE="${CV_OXIDE:-$ROOT/cratonvm-oxide.exe}"
JDK="${JDK:-C:/Program Files/Eclipse Adoptium/jdk-25.0.3.9-hotspot}"
GO="${GO:-$HERE}"

CLASS="${1:-GpuAllocChurn}"
N="${2:-1048576}"
ITERS="${3:-50}"
ROUNDS="${ROUNDS:-21}"

for f in "$CV_CUDARC" "$CV_OXIDE"; do
  [ -x "$f" ] || { echo "FATAL: missing binary $f"; exit 1; }
done

TMP="$(mktemp -d)"; trap 'rm -rf "$TMP"' EXIT

bin_for() { case "$1" in cudarc|ctl) echo "$CV_CUDARC";; oxide) echo "$CV_OXIDE";; esac; }

run_arm() {
  local out ms sum
  out=$("$(bin_for "$1")" --gpu --java-home "$JDK" --Xmx 8g -cp "$GO" "$CLASS" "$N" "$ITERS" 2>/dev/null)
  ms=$(echo "$out"  | grep -oE 'best_ms=[0-9.]+'  | head -1 | sed 's/best_ms=//')
  sum=$(echo "$out" | grep -oE 'checksum=[-0-9]+' | head -1 | sed 's/checksum=//')
  echo "${ms:-NA} ${sum:-NA}"
}

echo "=== backend A/B: $CLASS n=$N iters=$ITERS rounds=$ROUNDS ==="
echo "cudarc = $CV_CUDARC"
echo "oxide  = $CV_OXIDE"
echo "ctl    = $CV_CUDARC   (same binary as cudarc: the noise floor)"
[ $((ROUNDS % 3)) -eq 0 ] || echo "WARNING: ROUNDS=$ROUNDS is not a multiple of 3; slots are unbalanced."
echo

ORDERS=("cudarc oxide ctl" "oxide ctl cudarc" "ctl cudarc oxide")
for r in $(seq 1 "$ROUNDS"); do
  order=${ORDERS[$(( (r - 1) % 3 ))]}
  line=$(printf "round %-3s" "$r")
  for arm in $order; do
    read -r ms sum <<<"$(run_arm "$arm")"
    echo "$ms" >> "$TMP/$arm.ms"; echo "$sum" >> "$TMP/$arm.sum"
    line="$line  $(printf '%-6s=%9s ms' "$arm" "$ms")"
  done
  echo "$line"
done

echo
echo "=== correctness: checksum must be identical across arms ==="
for arm in cudarc oxide ctl; do
  printf "  %-7s %s\n" "$arm" "$(sort -u "$TMP/$arm.sum" | tr '\n' ' ')"
done
if [ "$(sort -u "$TMP/cudarc.sum")" = "$(sort -u "$TMP/oxide.sum")" ]; then
  echo "  CHECKSUM MATCH across backends"
else
  echo "  CHECKSUM MISMATCH -- timings are void, fix correctness first"
  exit 1
fi

# Paired per-round ratios with a 95% CI. Paired because both arms of a ratio
# ran inside the same round, so load drift is shared and largely cancels --
# the property bench-gpu/wait-for-quiet.sh says survives a busy box.
echo
paste -d' ' "$TMP/cudarc.ms" "$TMP/oxide.ms" "$TMP/ctl.ms" | awk '
  { c=$1; o=$2; t=$3; if (c>0) { oc[++n]=o/c; tc[n]=t/c } }
  function report(name, a, n_,   i, m, sd, se, lo, hi) {
    m=0; for (i=1;i<=n_;i++) m+=a[i]; m/=n_
    sd=0; for (i=1;i<=n_;i++) sd+=(a[i]-m)^2; sd=(n_>1)?sqrt(sd/(n_-1)):0
    se=(n_>0)?sd/sqrt(n_):0; lo=(m-1.96*se-1)*100; hi=(m+1.96*se-1)*100
    printf "  %-13s n=%-3d mean=%+6.1f%%  95%% CI [%+.1f%%, %+.1f%%]\n", name, n_, (m-1)*100, lo, hi
    return (lo<0 && hi>0)
  }
  END {
    print "=== paired ratios vs cudarc (>0% means slower) ==="
    z1=report("oxide", oc, n)
    z2=report("ctl (NOISE)", tc, n)
    print ""
    if (z1) print "  oxide CI includes zero -> NO backend difference resolved."
    else    print "  oxide CI excludes zero -> a real difference at this shape."
    if (!z2) print "  WARNING: the control CI EXCLUDES zero. The same binary differs"
    if (!z2) print "  from itself, so this run is not calibrated -- distrust it."
  }'
