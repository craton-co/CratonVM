#!/usr/bin/env bash
# Interleaved A/B of GPULlama3's HOST dispatch cost, with a per-round
# single-thread CPU control.
#
# Why `submit_ms` and not tok/s: `submit_ms` is the host time one token
# spends BUILDING its 453 kernel submissions, which is the only part of the
# token a dispatch-path change can move. `drain_ms` beside it is device
# time and moves the other way when the host slows down -- the device
# finishes while the host is still submitting -- so an end-to-end token rate
# mixes the two and needs many more rounds to say anything.
#
# Why the control column: six interleaved rounds of the same pair on this
# box produced arm means spanning 11.6 to 22.6 tok/s. The control is a fixed
# single-threaded HotSpot CPU render in the same round, so it measures the
# host's CPU clock and nothing about the GPU path. Taking CPU away from the
# VM does reproduce the slow regime exactly (24 spinners: `submit_ms` 14 ->
# 60, `drain_ms` 24 -> 5, 23.3 -> 13.2 tok/s), so the control is the right
# thing to divide by when the host IS loaded. It is not the whole story: on
# a quiet box the control has held within 5% across rounds whose arms moved
# by 2x, so something on the GPU/driver side drifts too and is not
# identified here. Either way the pairing inside a round is what the
# comparison rests on; the control is there so a round measured under load
# is visible rather than silently averaged in.
set -u
A="${A:?arm A binary}"
B="${B:?arm B binary}"
ROUNDS="${ROUNDS:-6}"
N="${N:-32}"
JDK="${JDK:-C:/craton/TornadoVM/jdk-25.0.3}"
HSJDK="${HSJDK:-C:/Program Files/Eclipse Adoptium/jdk-25.0.3.9-hotspot}"
BENCH="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd -W)"
# The GPULlama3 checkout is not in this repository (`apps/` holds
# external app trees and is git-ignored). Override APP when running
# from a worktree that does not have one beside it.
APP="${APP:-$(dirname "${BASH_SOURCE[0]}")/../apps/GPULlama3.java}"
[ -f "$APP/run-craton-gpu.sh" ] || {
  echo "no GPULlama3 checkout at $APP -- set APP=<path to GPULlama3.java>" >&2
  exit 2
}

control() {
  "$HSJDK/bin/java" -cp "$BENCH" RayTracerKernel 1280 720 12 2>/dev/null \
    | sed -n 's/.*[[:space:]]best_ms=\([^[:space:]]*\).*/\1/p'
}

# The second control, and on this box the more informative one. An idle
# RTX 2060 sits at P8 and 300 MHz against a 2100 MHz maximum, and a
# 32-token run is 1.4 s of work -- not obviously enough to pull it up. A
# round whose `gpu_mhz` is near idle is measuring the ramp, not the
# binary. Sampled right after the arms run, so it reports the state they
# left the device in.
gpu_mhz() { nvidia-smi --query-gpu=clocks.sm --format=csv,noheader,nounits 2>/dev/null | head -1; }

# How many compiler processes are running. This machine is shared between
# concurrent sessions and four other `rustc` processes have been observed
# mid-measurement; a round taken against somebody else's build should be
# visible rather than silently averaged in.
busy() { ps -W 2>/dev/null | grep -cE 'rustc|cargo|link\.exe' || echo 0; }

# median submit_ms and the run's own achieved tok/s
run() {
  ( cd "$APP" && env $2 CRATON_EXTRA="-Dllama.craton.verbose=true" \
      bash run-craton-gpu.sh "$1" -p "Why is the sky blue?" -n "$N" 2>&1 ) \
  | awk '
      /kernels=/ { if (match($0, /submit_ms=[0-9,.]+/)) {
                     v = substr($0, RSTART+10, RLENGTH-10); gsub(",", ".", v); s[++ns] = v+0 } }
      /achieved tok\/s/ { if (match($0, /tok\/s: [0-9,.]+/)) {
                     v = substr($0, RSTART+7, RLENGTH-7); gsub(",", ".", v); tok = v+0 } }
      END {
        if (ns < 8) { print "FAIL FAIL"; exit }
        # Drop the first quarter: early tokens carry JIT warm-up and a
        # shorter KV cache, and are not the steady state being compared.
        lo = int(ns/4) + 1
        m = 0; c = 0
        for (i = lo; i <= ns; i++) { m += s[i]; c++ }
        printf "%.2f %.2f\n", m/c, tok
      }'
}

printf '%-6s %9s %7s %6s | %9s %8s %9s | %9s %8s %9s | %s\n' \
  round control gpuMHz build A_sub A_tok A_norm B_sub B_tok B_norm order
tmp="$(mktemp)"
for r in $(seq 1 "$ROUNDS"); do
  ctl=$(control)
  if [ $((r % 2)) -eq 1 ]; then
    a=$(run "$A" "$A_ENV"); b=$(run "$B" "$B_ENV"); order="A-then-B"
  else
    b=$(run "$B" "$B_ENV"); a=$(run "$A" "$A_ENV"); order="B-then-A"
  fi
  set -- $a; asub=$1; atok=$2
  set -- $b; bsub=$1; btok=$2
  read an bn <<<"$(awk -v c="$ctl" -v x="$asub" -v y="$bsub" \
      'BEGIN { if (c+0 > 0) printf "%.4f %.4f", x/c, y/c; else print "0 0" }')"
  printf '%-6s %9s %7s %6s | %9s %8s %9s | %9s %8s %9s | %s\n' \
    "$r" "$ctl" "$(gpu_mhz)" "$(busy)" "$asub" "$atok" "$an" "$bsub" "$btok" "$bn" "$order"
  echo "$an $bn" >> "$tmp"
done
echo
awk '$1+0 > 0 && $2+0 > 0 { a[++n] = $1; b[n] = $2; sa += $1; sb += $2; if ($2 < $1) w++ }
     END {
       if (!n) { print "no usable rounds"; exit }
       for (i = 1; i <= n; i++) { r[i] = b[i]/a[i] }
       for (i = 2; i <= n; i++) { t = r[i]; for (j = i-1; j >= 1 && r[j] > t; j--) r[j+1] = r[j]; r[j+1] = t }
       med = (n % 2) ? r[(n+1)/2] : (r[n/2] + r[n/2+1]) / 2
       printf "control-normalised submit cost: A %.4f, B %.4f\n", sa/n, sb/n
       printf "B lower in %d/%d rounds; median per-round B/A ratio %.4f (%.1f%% cheaper)\n",
              w+0, n, med, 100*(1-med)
     }' "$tmp"
rm -f "$tmp"
