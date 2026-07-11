#!/usr/bin/env bash
# GPU offload CI gate — real-hardware pass/fail checks for the self-hosted
# weekly job (.github/workflows/gpu-selfhosted.yml). Every real-GPU test in
# this tree is #[ignore]d or gpu-it-gated (see docs/gpu/README.md), so this
# is the only thing standing between a silent offload regression and a green
# build. It exists because one already slipped through undetected: the
# 2026-07-11 invoke-cache promotion bug made offload silently stop after the
# first call at a call site (see docs/known-issues/gpu-offload-followups-
# 20260711.md item 7) — any checksum+timing gate on real hardware would have
# caught it the day it landed.
#
# This script does NOT build anything; the caller (gpu-selfhosted.yml) builds
# the gpu-driver binary and compiles the Java fixtures first.
#
# Inputs (env, all optional except CV_GPU which needs a real gpu-driver
# build to mean anything):
#   CV_GPU          path to the --features gpu-driver cratonvm.exe
#   JDK             real JDK home (bin/java.exe, bin/javac.exe)
#   HS              path to HotSpot java.exe (defaults to $JDK/bin/java.exe)
#   GO              classpath dir holding the bench-gpu/*.java fixtures
#   TG              classpath dir holding test_classes/gpu/*.java fixtures
#   GATE_REDUCTION  1 to also run the dot-reduction gate (off by default —
#                   reduction dispatch hasn't shipped yet, see followups
#                   item 1)
#
# Usage: bash bench-gpu/ci-gate.sh
# Exit: 0 if every enabled gate PASSes, 1 if any gate FAILs or a
#       prerequisite is missing.
set +e
set +o pipefail

# Same MSYS path-mangling workaround as run-gpu-comparison.sh: without this,
# Git-Bash/MSYS rewrites things that look like Unix paths (leading "/", or
# flags like --gpu that could get glob-expanded) before cratonvm.exe ever
# sees them.
export MSYS2_ARG_CONV_EXCL='*' MSYS_NO_PATHCONV=1

# ── paths ────────────────────────────────────────────────────────────────
ROOT="${ROOT:-C:/craton/CratonVM}"
CV_GPU="${CV_GPU:-$ROOT/target-gpu/release/cratonvm.exe}"
JDK="${JDK:-C:/Program Files/Java/jdk-25}"
HS="${HS:-$JDK/bin/java.exe}"
GO="${GO:-$ROOT/bench-gpu}"
TG="${TG:-$ROOT/test_classes/gpu}"
GATE_REDUCTION="${GATE_REDUCTION:-0}"
TIMEOUT_S="${TIMEOUT_S:-180}"

FAILS=0

pass() { echo "PASS: $1"; }
fail() { echo "FAIL: $1"; FAILS=$((FAILS + 1)); }
skip() { echo "SKIP: $1"; }

extract() {   # $1=key $2=text
  echo "$2" | grep -oE "${1}=[^[:space:]]+" | head -1 | sed "s/${1}=//"
}

echo "=== gpu-offload ci-gate ==="
echo "CV_GPU=$CV_GPU"
echo "JDK=$JDK"
echo "HS=$HS"
echo "GO=$GO"
echo "TG=$TG"
echo

# ── prerequisites ───────────────────────────────────────────────────────
[ -x "$CV_GPU" ] || { echo "FATAL: missing/non-executable CV_GPU=$CV_GPU"; exit 1; }
[ -x "$HS" ]     || { echo "FATAL: missing/non-executable HS=$HS"; exit 1; }
[ -f "$GO/GpuWarm.class" ] || { echo "FATAL: $GO/GpuWarm.class missing — compile bench-gpu fixtures first"; exit 1; }
[ -f "$GO/GpuCompute.class" ] || { echo "FATAL: $GO/GpuCompute.class missing — compile bench-gpu fixtures first"; exit 1; }
[ -f "$TG/BoundsDeopt2.class" ] || { echo "FATAL: $TG/BoundsDeopt2.class missing — compile test_classes/gpu fixtures first"; exit 1; }

# ── gate a: --gpu-info sees a real device ──────────────────────────────
# `--gpu-info` always exits 0 (both "device found" and "no CUDA driver" are
# clean early-exits — see vm-cli/src/main.rs). On a GPU box that means exit
# code alone can't distinguish "driver is healthy" from "driver vanished";
# we also require the "device " line the success path prints.
echo "--- gate a: --gpu-info ---"
info_out=$(timeout "$TIMEOUT_S" "$CV_GPU" --gpu-info 2>&1)
info_rc=$?
echo "$info_out"
if [ "$info_rc" -eq 0 ] && echo "$info_out" | grep -qE '^device [0-9]+:'; then
  pass "--gpu-info exit=0, device line present"
else
  fail "--gpu-info exit=$info_rc, device line missing (driver/hardware gone?)"
fi
echo

# ── gate b: GpuWarm warm-timing + correctness ──────────────────────────
# A silent offload-to-CPU regression (e.g. the 2026-07-11 invoke-cache bug)
# does not change the SAMPLE checksum — it just makes the run slow, because
# the CPU fallback still computes the right answer, just ~2000-3400ms
# instead of the GPU's tens-of-ms warm round-trip at this size. So the
# timing threshold is the actual regression detector here; the checksum is
# a sanity check that we're comparing like-for-like results, not a
# substitute for the timing bound.
#
# 500ms is deliberately generous: it's roughly 4-6x the observed GPU warm
# time yet still ~4-7x below the CPU-fallback floor (~2000-3400ms), so a
# loaded/noisy runner won't false-alarm but an actual regression to CPU
# cannot sneak under it.
echo "--- gate b: GpuWarm f 2^24 5 (--gpu) vs HotSpot ---"
GPUWARM_N=16777216
GPUWARM_REPS=5
GPUWARM_MS_LIMIT=500

warm_gpu_out=$(timeout "$TIMEOUT_S" "$CV_GPU" --gpu --java-home "$JDK" --Xmx 8g -cp "$GO" GpuWarm f "$GPUWARM_N" "$GPUWARM_REPS" 2>&1)
warm_hs_out=$(timeout "$TIMEOUT_S" "$HS" -Xmx8g -cp "$GO" GpuWarm f "$GPUWARM_N" "$GPUWARM_REPS" 2>&1)
echo "cv-gpu: $warm_gpu_out"
echo "hotspot: $warm_hs_out"

gpu_sample=$(extract SAMPLE "$warm_gpu_out")
hs_sample=$(extract SAMPLE "$warm_hs_out")
gpu_ms=$(extract warm_ms "$warm_gpu_out")

if [ -z "$gpu_sample" ] || [ -z "$hs_sample" ] || [ -z "$gpu_ms" ]; then
  fail "GpuWarm: could not parse SAMPLE/warm_ms from one or both runs"
elif [ "$gpu_sample" != "$hs_sample" ]; then
  fail "GpuWarm: SAMPLE mismatch (cv-gpu=$gpu_sample hotspot=$hs_sample)"
elif ! [[ "$gpu_ms" =~ ^[0-9]+$ ]]; then
  fail "GpuWarm: warm_ms not numeric ($gpu_ms)"
elif [ "$gpu_ms" -ge "$GPUWARM_MS_LIMIT" ]; then
  fail "GpuWarm: warm_ms=$gpu_ms >= ${GPUWARM_MS_LIMIT}ms (looks like a CPU-fallback regression; SAMPLE matched so it's not a correctness bug)"
else
  pass "GpuWarm: SAMPLE matches HotSpot ($gpu_sample), warm_ms=$gpu_ms < ${GPUWARM_MS_LIMIT}ms"
fi
echo

# ── gate c: GpuCompute checksum at 2^26 ────────────────────────────────
echo "--- gate c: GpuCompute 2^26 (--gpu) vs HotSpot ---"
GPUCOMPUTE_N=67108864

compute_gpu_out=$(timeout "$TIMEOUT_S" "$CV_GPU" --gpu --java-home "$JDK" --Xmx 8g -cp "$GO" GpuCompute "$GPUCOMPUTE_N" 2>&1)
compute_hs_out=$(timeout "$TIMEOUT_S" "$HS" -Xmx8g -cp "$GO" GpuCompute "$GPUCOMPUTE_N" 2>&1)
echo "cv-gpu: $compute_gpu_out"
echo "hotspot: $compute_hs_out"

gpu_cs=$(extract COMPUTE_CHECKSUM "$compute_gpu_out")
hs_cs=$(extract COMPUTE_CHECKSUM "$compute_hs_out")

if [ -z "$gpu_cs" ] || [ -z "$hs_cs" ]; then
  fail "GpuCompute: could not parse COMPUTE_CHECKSUM from one or both runs"
elif [ "$gpu_cs" != "$hs_cs" ]; then
  fail "GpuCompute: COMPUTE_CHECKSUM mismatch (cv-gpu=$gpu_cs hotspot=$hs_cs)"
else
  pass "GpuCompute: COMPUTE_CHECKSUM matches HotSpot ($gpu_cs)"
fi
echo

# ── gate d: BoundsDeopt2 integrity under --gpu --nojit ─────────────────
# The GPU kernel's per-access bounds check must still trigger a deopt back
# to the CPU, and the CPU-side re-run must actually raise the required
# AIOOBE (see docs/known-issues/jit-bce-multi-array-oob-store-20260711.md —
# with the JIT on, a *separate* JIT bug can silently swallow the exception,
# which is why this gate pins --nojit rather than exercising the JIT bug).
echo "--- gate d: BoundsDeopt2 (--gpu --nojit) ---"
bounds_out=$(timeout "$TIMEOUT_S" "$CV_GPU" --gpu --nojit --java-home "$JDK" -cp "$TG" BoundsDeopt2 2>&1)
echo "$bounds_out"

if echo "$bounds_out" | grep -q "THROWN java.lang.ArrayIndexOutOfBoundsException"; then
  pass "BoundsDeopt2: THROWN java.lang.ArrayIndexOutOfBoundsException observed"
else
  fail "BoundsDeopt2: expected 'THROWN java.lang.ArrayIndexOutOfBoundsException', got: $bounds_out"
fi
echo

# ── gate e (optional): dot-reduction checksum ──────────────────────────
# Off by default: reduction kernels are analyzed/lowered but never actually
# dispatched yet (the void-return gate in try_dispatch falls through to CPU
# for any non-void kernel — see followups item 1). Flip GATE_REDUCTION=1
# once dispatch-side reduction read-back ships, so this gate starts
# meaning something instead of trivially passing on a CPU fallback.
echo "--- gate e (optional, GATE_REDUCTION=$GATE_REDUCTION): GpuProbe DOT_CHECKSUM ---"
if [ "$GATE_REDUCTION" = "1" ]; then
  if [ -f "$GO/GpuProbe.class" ]; then
    probe_gpu_out=$(timeout "$TIMEOUT_S" "$CV_GPU" --gpu --java-home "$JDK" --Xmx 8g -cp "$GO" GpuProbe 2>&1)
    probe_hs_out=$(timeout "$TIMEOUT_S" "$HS" -Xmx8g -cp "$GO" GpuProbe 2>&1)
    echo "cv-gpu: $probe_gpu_out"
    echo "hotspot: $probe_hs_out"

    gpu_dot=$(extract DOT_CHECKSUM "$probe_gpu_out")
    hs_dot=$(extract DOT_CHECKSUM "$probe_hs_out")
    if [ -z "$gpu_dot" ] || [ -z "$hs_dot" ]; then
      fail "GpuProbe: could not parse DOT_CHECKSUM from one or both runs"
    elif [ "$gpu_dot" != "$hs_dot" ]; then
      fail "GpuProbe: DOT_CHECKSUM mismatch (cv-gpu=$gpu_dot hotspot=$hs_dot)"
    else
      pass "GpuProbe: DOT_CHECKSUM matches HotSpot ($gpu_dot)"
    fi
  else
    fail "GpuProbe.class missing — compile bench-gpu fixtures first (GATE_REDUCTION=1 requires it)"
  fi
else
  skip "dot-reduction gate (set GATE_REDUCTION=1 to enable)"
fi
echo

# ── summary ─────────────────────────────────────────────────────────────
echo "=== summary ==="
if [ "$FAILS" -eq 0 ]; then
  echo "ALL GATES PASSED"
  exit 0
else
  echo "$FAILS GATE(S) FAILED"
  exit 1
fi
