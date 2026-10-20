#!/usr/bin/env bash
# First contact with an AMD GPU, in ascending order of cost.
#
# Runs ON the remote Linux box, from the repository root. Everything in
# the tree has been built and tested against HIP without any AMD
# hardware; this is the script that finds out what that was worth.
#
#   bash tools/gpu/amd-remote/bringup.sh [--quick]
#
# `--quick` stops after stage 4 (the first real kernel launch), which is
# the answer to "does any of this work at all" and takes a couple of
# minutes plus the build.
#
# ── WHY THE ORDER IS THE ORDER ────────────────────────────────────────
#
# Each stage can only fail for reasons the stages before it have already
# ruled out. That is the whole design: a failure names its own cause,
# instead of a benchmark printing a wrong number twenty minutes in and
# leaving you to bisect the stack.
#
#   0  environment       — is there a GPU, and does ROCm see it
#   1  build             — does the binary link
#   2  no-driver guard   — does the panic guard hold (it is the one
#                          thing already tested in CI, so a failure here
#                          means the remote tree is not what CI built)
#   3  host bridge       — hiprtc compiles, a module loads, a
#                          hand-written kernel launches and returns
#   4  end to end        — a Java method is lowered, rendered, compiled
#                          and run through `--gpu`
#   5  correctness       — GPU answers match the CPU, kernel by kernel
#   6  measurement       — only now does a number mean anything
#
# Everything is appended to a report file, so the session leaves behind
# something readable rather than a scrollback.

set -uo pipefail

QUICK=0
[ "${1:-}" = "--quick" ] && QUICK=1

REPORT="${REPORT:-amd-bringup-$(date +%Y%m%d-%H%M%S).md}"
TARGET_DIR="${CARGO_TARGET_DIR:-target-hip}"
export CARGO_TARGET_DIR="$TARGET_DIR"
CV="$TARGET_DIR/release/cratonvm"

pass=0
fail=0
skip=0

say()  { printf '%s\n' "$*" | tee -a "$REPORT"; }
head_() { say ""; say "## $*"; say ""; }
ok()   { pass=$((pass + 1)); say "- PASS  $*"; }
bad()  { fail=$((fail + 1)); say "- FAIL  $*"; }
note() { skip=$((skip + 1)); say "- SKIP  $*"; }
run()  { say '```'; "$@" 2>&1 | tee -a "$REPORT" | tail -40; say '```'; }

say "# CratonVM HIP bring-up — $(date -u +%Y-%m-%dT%H:%M:%SZ)"
say ""
say "Report: \`$REPORT\`  ·  target dir: \`$TARGET_DIR\`"

# ── 0. Environment ────────────────────────────────────────────────────
head_ "0. Environment"
say '```'
{
  uname -a
  echo "--- rocm ---"
  command -v hipconfig >/dev/null && hipconfig --version 2>&1 | head -3 || echo "hipconfig: NOT FOUND"
  echo "--- devices ---"
  command -v rocminfo >/dev/null && rocminfo 2>/dev/null | grep -E "Name:|gfx" | head -20 || echo "rocminfo: NOT FOUND"
  echo "--- libraries ---"
  ldconfig -p 2>/dev/null | grep -E "amdhip64|hiprtc" | head -5 || echo "no amdhip64/hiprtc in ldconfig"
  echo "--- toolchain ---"
  cargo --version 2>&1
  (command -v g++ >/dev/null && g++ --version | head -1) || echo "g++: NOT FOUND"
  (command -v javac >/dev/null && javac -version 2>&1) || echo "javac: NOT FOUND"
} | tee -a "$REPORT"
say '```'

if ! command -v hipconfig >/dev/null && ! ls /opt/rocm/lib/libamdhip64.so* >/dev/null 2>&1; then
  bad "no ROCm found — stages 3 onward cannot pass. Install ROCm, or check \$PATH and /opt/rocm."
else
  ok "ROCm is present"
fi

# ── 1. Build ──────────────────────────────────────────────────────────
head_ "1. Build"
say "Building the HIP-backed launcher. Needs no ROCm to compile —"
say "\`cubecl-hip-sys\` dlopens at first use — so a failure here is a"
say "code or toolchain problem, not a driver one."
if cargo build --release -p cratonvm-cli --bin cratonvm --features gpu-driver-hip 2>&1 | tail -20 | tee -a "$REPORT"; then
  ok "cratonvm built with gpu-driver-hip"
else
  bad "build failed — nothing below can run"
  say ""
  say "**Stopped at stage 1.**"
  exit 1
fi

# ── 2. The guard CI already proves ────────────────────────────────────
head_ "2. No-driver guard"
say "Already green in public CI. A failure HERE means the remote tree is"
say "not the tree CI built — check the revision before reading anything"
say "below as a HIP result."
if cargo test --release -p cratonvm-cuda-bridge --features hip 2>&1 | tail -15 | tee -a "$REPORT" | grep -q "test result: ok"; then
  ok "cuda-bridge HIP unit + no-driver tests"
else
  bad "cuda-bridge HIP tests — the tree may not match CI"
fi

# ── 3. The host bridge, against the device ────────────────────────────
head_ "3. Host bridge on hardware"
say "\`--gpu-info\` first: it is the cheapest possible proof that the"
say "runtime loaded, a device was found, and its architecture and"
say "wavefront width were read."
run "$CV" --gpu-info
if "$CV" --gpu-info 2>&1 | grep -qE '^device [0-9]+:'; then
  ok "--gpu-info sees a device"
  ARCH=$("$CV" --gpu-info 2>&1 | grep -oE 'gfx[0-9a-z:+-]+' | head -1)
  WAVE=$("$CV" --gpu-info 2>&1 | grep -oE 'wave [0-9]+' | head -1)
  say ""
  say "Architecture: \`${ARCH:-unknown}\`  ·  ${WAVE:-wavefront unknown}"
else
  bad "--gpu-info found no device — stages 4+ will fail the same way"
fi

say ""
say "Now the host battery: hiprtc compiles, a code object loads, a"
say "hand-written kernel launches with the arguments it was given, and"
say "the result comes back. The kernels are hand-written on purpose — a"
say "lowering bug and a host bug produce the same symptom, and this"
say "stage exists to rule out the host half."
if cargo test --release -p cratonvm-cuda-bridge --features hip,gpu-it -- --ignored 2>&1 | tail -25 | tee -a "$REPORT" | grep -q "test result: ok"; then
  ok "hip_module_it — hiprtc, module load, launch, readback, deopt flag"
else
  bad "hip_module_it — the host half does not work on this device"
fi

# ── 4. End to end ─────────────────────────────────────────────────────
head_ "4. Java through the whole pipeline"
say "Bytecode → analyzer → kernel IR → HIP C++ → hiprtc → launch."
say "This is the first stage that exercises the renderer on hardware."
if ! command -v javac >/dev/null; then
  note "javac not installed — cannot compile the fixtures. Install a JDK to run stages 4-6."
else
  FIX=$(mktemp -d)
  if javac -d "$FIX" tools/bench/bench-gpu/GpuWarm.java tools/bench/bench-gpu/GpuProbe.java 2>&1 | tee -a "$REPORT"; then
    ok "bench fixtures compiled"
    say ""
    say "Kernel source is dumped so the generated HIP C++ can be read"
    say "directly — that is the artefact to look at first if a number"
    say "comes out wrong."
    DUMP="$PWD/hip-dump"
    mkdir -p "$DUMP"
    say '```'
    CRATONVM_GPU_DUMP_PTX="$DUMP" "$CV" --gpu --print-gpu-decisions -cp "$FIX" GpuWarm 2>&1 | tail -30 | tee -a "$REPORT"
    say '```'
    if ls "$DUMP"/*.hip >/dev/null 2>&1; then
      ok "a kernel was lowered and rendered as HIP C++ ($(ls "$DUMP"/*.hip | wc -l) file(s) in $DUMP)"
      say ""
      say "First rendered kernel:"
      say '```cpp'
      head -40 "$(ls "$DUMP"/*.hip | head -1)" | tee -a "$REPORT"
      say '```'
    else
      bad "no HIP kernel was rendered — check the --print-gpu-decisions output above"
    fi
  else
    bad "javac could not compile the bench fixtures"
  fi
fi

if [ "$QUICK" = "1" ]; then
  head_ "Summary (quick)"
  say "$pass passed, $fail failed, $skip skipped."
  say ""
  say "Stopped after stage 4 (\`--quick\`). Re-run without it for the"
  say "correctness and measurement stages."
  exit $((fail > 0))
fi

# ── 5. Correctness ────────────────────────────────────────────────────
head_ "5. GPU answers match the CPU"
say "A GPU result that merely *exists* proves nothing: the whole"
say "subsystem's characteristic failure is a plausible wrong number."
say "Every kernel is run twice, with and without \`--gpu\`, and the"
say "checksums compared."
if [ -n "${FIX:-}" ] && [ -d "${FIX:-/nonexistent}" ]; then
  for cls in GpuWarm GpuProbe; do
    GPU_OUT=$("$CV" --gpu -cp "$FIX" "$cls" 2>&1 | grep -oE 'checksum[= ][-0-9]+' | head -1)
    CPU_OUT=$("$CV" -cp "$FIX" "$cls" 2>&1 | grep -oE 'checksum[= ][-0-9]+' | head -1)
    if [ -z "$GPU_OUT" ] || [ -z "$CPU_OUT" ]; then
      note "$cls — no checksum line to compare (fixture may not print one)"
    elif [ "$GPU_OUT" = "$CPU_OUT" ]; then
      ok "$cls — GPU and CPU agree ($GPU_OUT)"
    else
      bad "$cls — GPU $GPU_OUT vs CPU $CPU_OUT"
    fi
  done
else
  note "no compiled fixtures — stage 5 needs stage 4"
fi

say ""
say "The marshalling sweep, which counts host-to-device transfers PER"
say "KERNEL and fails when a type never dispatched. A kernel that"
say "silently ran on the CPU returns correct answers, so only counting"
say "engagement can see it."
if [ -x tools/bench/bench-gpu/marshal-stress.sh ] || [ -f tools/bench/bench-gpu/marshal-stress.sh ]; then
  say '```'
  CV_GPU="$CV" CV="$CV" bash tools/bench/bench-gpu/marshal-stress.sh 2>&1 | tail -30 | tee -a "$REPORT"
  say '```'
else
  note "marshal-stress.sh not found"
fi

# ── 6. Measurement ────────────────────────────────────────────────────
head_ "6. Measurement"
say "Only now. A number from a stage that has not proved correctness is"
say "a number for the wrong thing."
if [ -f tools/bench/bench-gpu/ci-gate.sh ]; then
  say '```'
  CV_GPU="$CV" CV="$CV" JDK="${JAVA_HOME:-}" bash tools/bench/bench-gpu/ci-gate.sh 2>&1 | tail -40 | tee -a "$REPORT"
  say '```'
else
  note "ci-gate.sh not found"
fi

head_ "Summary"
say "$pass passed, $fail failed, $skip skipped."
say ""
say "Report written to \`$REPORT\`. Rendered kernels, if any, are in"
say "\`hip-dump/\`."
exit $((fail > 0))
