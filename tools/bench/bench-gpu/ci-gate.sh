#!/usr/bin/env bash
# GPU offload CI gate — real-hardware pass/fail checks for the self-hosted
# weekly job (.github/workflows/gpu-selfhosted.yml). Every real-GPU test in
# this tree is #[ignore]d or gpu-it-gated (see docs/gpu/README.md), so this
# is the only thing standing between a silent offload regression and a green
# build. It exists because one already slipped through undetected: the
# 2026-07-11 invoke-cache promotion bug made offload silently stop after the
# first call at a call site (see gpu-offload-followups-20260711.md item 7)
# — any checksum+timing gate on real hardware would have
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
#   GATE_REDUCTION  1 (default) to run the dot-reduction gate; 0 to skip
#                   it. On by default since 2026-09-05 — see gate e.
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
GATE_REDUCTION="${GATE_REDUCTION:-1}"
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
[ -f "$TG/GpuForwardRef.class" ] || { echo "FATAL: $TG/GpuForwardRef.class missing — compile test_classes/gpu fixtures first"; exit 1; }

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

# ── gate e: dot-reduction checksum + engagement ────────────────────────
# ON by default since 2026-09-05. It was off for ~2 months on the premise
# that "reduction dispatch hasn't shipped yet" — that premise was stale:
# `)I`/`)J` reductions have dispatched through
# `DispatchOutcome::HandledWithValue` since 2026-07-11. Confirmed on real
# hardware (RTX 2060, sm_75) on 2026-09-05.
#
# THE CHECKSUM ALONE CANNOT GATE THIS. `dotReduce` computes the same
# answer on the CPU, so a run that never offloaded still prints the right
# DOT_CHECKSUM — the exact vacuity the old comment warned about. Measured,
# same binary, one flag apart:
#
#   --gpu                       DOT_CHECKSUM correct, witness line present
#   --gpu --gpu-min-work 1e9    DOT_CHECKSUM correct, witness line ABSENT
#
# So the gate also requires a per-kernel engagement witness: the
# `H2D=... (GpuProbe.dotReduce([I[I)` line that
# `CRATONVM_GPU_TRACE_BYTES=1` emits at info level from
# `cratonvm_vm::runtime::offload`. Both halves are needed — RUST_LOG alone
# prints nothing without the flag, and the flag alone is a `tracing::info!`
# with no subscriber.
#
# The witness is the PRESENCE of the line, never a positive byte count.
# `GpuProbe` runs `vaddMap` over the same two arrays first, so the
# residency cache legitimately suppresses the re-upload and `dotReduce`
# reports `H2D=0 bytes`. Asserting `H2D > 0` here would fail on correct
# behaviour.
#
# A process-wide launch census would NOT do: `vaddMap` offloads in the
# same run, so any whole-process counter is non-zero whether or not the
# reduction dispatched. The witness has to name the kernel.
echo "--- gate e (GATE_REDUCTION=$GATE_REDUCTION): GpuProbe DOT_CHECKSUM + engagement ---"
if [ "$GATE_REDUCTION" = "1" ]; then
  if [ -f "$GO/GpuProbe.class" ]; then
    probe_gpu_out=$(RUST_LOG="cratonvm_vm::runtime::offload=info" \
      CRATONVM_GPU_TRACE_BYTES=1 \
      timeout "$TIMEOUT_S" "$CV_GPU" --gpu --java-home "$JDK" --Xmx 8g -cp "$GO" GpuProbe 2>&1)
    probe_hs_out=$(timeout "$TIMEOUT_S" "$HS" -Xmx8g -cp "$GO" GpuProbe 2>&1)
    # The trace lines go to the same capture, so print only the answers.
    answers() { echo "$1" | grep -E '^(n|MAP_CHECKSUM|DOT_CHECKSUM|MAX|OUT0)=' | tr '\n' ' '; }
    echo "cv-gpu: $(answers "$probe_gpu_out")"
    echo "hotspot: $(answers "$probe_hs_out")"

    gpu_dot=$(extract DOT_CHECKSUM "$probe_gpu_out")
    hs_dot=$(extract DOT_CHECKSUM "$probe_hs_out")
    # The engagement witness: this kernel, this run, actually dispatched.
    dot_witness=$(echo "$probe_gpu_out" | grep -cE "H2D=[0-9]+ bytes \(GpuProbe\.dotReduce")
    echo "engagement: dotReduce dispatch witness lines=$dot_witness"

    if [ -z "$gpu_dot" ] || [ -z "$hs_dot" ]; then
      fail "GpuProbe: could not parse DOT_CHECKSUM from one or both runs"
    elif [ "$gpu_dot" != "$hs_dot" ]; then
      fail "GpuProbe: DOT_CHECKSUM mismatch (cv-gpu=$gpu_dot hotspot=$hs_dot)"
    elif [ "$dot_witness" -eq 0 ]; then
      fail "GpuProbe: DOT_CHECKSUM matches ($gpu_dot) but dotReduce never dispatched — the reduction fell back to CPU and the checksum proves nothing. This is the gate's anti-vacuity clause, not a checksum failure."
    else
      pass "GpuProbe: DOT_CHECKSUM matches HotSpot ($gpu_dot), dotReduce really dispatched"
    fi
  else
    fail "GpuProbe.class missing — compile bench-gpu fixtures first (gate e requires it)"
  fi
else
  skip "dot-reduction gate (GATE_REDUCTION=0 — it is on by default; something turned it off)"
fi
echo

# ── gate f: a kernel in ANOTHER CLASS still offloads ───────────────────
# `offload_jit_gate` populates the compiled-tier offload registry as a side
# effect of scanning CALLERS, and it can only judge a call target whose
# declaring class is already loaded. A caller is scanned when it is ADMITTED to
# the JIT, which is before it runs — so a callee in a second class has usually
# never been touched at that moment. Until 2026-09-06 such a target was written
# off by `try_compiled_offload` as NotKernel for the life of the process, on the
# FIRST execution of the site, using a registry that could not yet know about a
# class that same dispatch was on its way to loading.
#
# Every other GPU fixture in this tree declares its kernels beside its driver —
# GpuLdcSplit, GpuIntensitySweep, GpuProbe, GpuWarm — so none of them can see
# this, and the whole existing battery was green while it was live. That is what
# this gate is for.
#
# THE CHECKSUM CANNOT GATE IT, for the same reason as gate e: the kernel
# computes the same answer on the CPU. GpuForwardRef prints one, and it is here
# only to prove the two arms did the same work.
#
# The arms differ in NOTHING but which class the kernel is declared in, so this
# is a same-config control rather than a threshold:
#
#   sameclass  : GpuForwardRef.scaleHere — the gate can always judge it. Never
#                affected by this defect, and it is what "working" looks like.
#   otherclass : GpuForwardRefKernel.scale — the identical body, one class away.
#
# Both must dispatch. `otherclass` is allowed one non-dispatching call: the
# first execution of the site is the thing that loads the class, so the helper
# re-asks and offloads from the second call on. Requiring ALL of them would be
# a gate that fails on correct behaviour.
echo "--- gate f: GpuForwardRef, a kernel one class away from its caller ---"
if [ -f "$TG/GpuForwardRef.class" ]; then
  FWD_ITERS=40
  fwd_ok=1
  fwd_cs=""
  for arm in sameclass otherclass; do
    if [ "$arm" = "sameclass" ]; then pat='GpuForwardRef\.scaleHere'; else pat='GpuForwardRefKernel\.scale'; fi
    arm_out=$(RUST_LOG="cratonvm_vm::runtime::offload=info" \
      CRATONVM_GPU_TRACE_BYTES=1 \
      timeout "$TIMEOUT_S" "$CV_GPU" --gpu --gpu-min-work 1 --java-home "$JDK" \
      --Xmx 4g -cp "$TG" GpuForwardRef "$arm" 262144 "$FWD_ITERS" 2>&1)
    arm_cs=$(extract checksum "$arm_out")
    arm_witness=$(echo "$arm_out" | grep -cE "H2D=[0-9]+ bytes \($pat")
    echo "  $arm: checksum=$arm_cs dispatch witness lines=$arm_witness/$FWD_ITERS"
    if [ -z "$arm_cs" ]; then
      fail "GpuForwardRef[$arm]: could not parse checksum"
      fwd_ok=0
    elif [ -z "$fwd_cs" ]; then
      fwd_cs="$arm_cs"
    elif [ "$arm_cs" != "$fwd_cs" ]; then
      fail "GpuForwardRef: checksum differs between arms ($fwd_cs vs $arm_cs) — the arms are not doing the same work"
      fwd_ok=0
    fi
    if [ "$arm_witness" -lt $((FWD_ITERS - 1)) ]; then
      fail "GpuForwardRef[$arm]: dispatched $arm_witness/$FWD_ITERS times. The answer is still right — the kernel just ran on the CPU. On 'otherclass' this is the 2026-09-06 forward-reference regression (CRATONVM_GPU_JIT_GATE_LATE_REGISTER=0 reproduces it deliberately); on 'sameclass' it is broader than that."
      fwd_ok=0
    fi
  done
  [ "$fwd_ok" -eq 1 ] && pass "GpuForwardRef: both arms dispatched, checksums agree ($fwd_cs)"
else
  fail "GpuForwardRef.class missing — compile test_classes/gpu fixtures first (gate f requires it)"
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
