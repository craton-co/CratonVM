#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
# Copyright 2024-2026 Craton Software Company

# Benchmark rows 1-5 on the AMD box: HotSpot C2 | TornadoVM (OpenCL) | CratonVM (HIP).
#
# Runs ON the remote Linux host, from anywhere:
#
#   bash tools/gpu/amd-remote/bench-rows.sh                 # rows 1-5
#   ROWS="int dbl" bash tools/gpu/amd-remote/bench-rows.sh  # a subset
#   SWEEP=1 bash tools/gpu/amd-remote/bench-rows.sh         # + longer div chains
#
# ROWS: int dbl fma dot ray  (default: all)
#
# Three things this does on purpose, each learned the hard way:
#
#  * EVERY arm of a chain row is generated from the one committed source
#    by gen-chain.py at the same length. The committed double chain has
#    59 statements in the HotSpot/CratonVM class and 62 in the TornadoVM
#    twin while both comments and the published label say 64, so the
#    published "vs TornadoVM" ratio compared unequal work.
#  * The CratonVM number only counts if the kernel DISPATCHED. A kernel the
#    renderer refuses runs on the CPU and prints a correct checksum, so each
#    CratonVM run also captures the `H2D=... bytes (Class.method` witness and
#    the row reports ENGAGED=0 instead of a speedup when it is missing.
#  * Checksums are compared against HotSpot. A speed number whose checksum
#    differs is not a result.
set -u

# `pwd -W` (Git Bash) gives C:/... which java.exe understands; elsewhere it fails and plain pwd is used.
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && { pwd -W 2>/dev/null || pwd; })"
ROOT="$(cd "$HERE/../../.." && { pwd -W 2>/dev/null || pwd; })"
case "$(uname -s)" in MINGW*|MSYS*|CYGWIN*) PSEP=';'; export MSYS2_ARG_CONV_EXCL='*' MSYS_NO_PATHCONV=1;; *) PSEP=':';; esac
# an executable may be `java` or `java.exe`
isexe() { [ -x "$1" ] || [ -x "$1.exe" ]; }
BENCH="$ROOT/tools/bench/bench-gpu"
TWINS="$ROOT/tools/bench/bench-tornado"

CV="${CV:-$ROOT/target-fast/release/cratonvm}"
JDK="${JDK:-/usr/lib/jvm/java-25-openjdk-amd64}"
TVHOME="${TVHOME:-$HOME/tornado/tornadovm-5.2.0-jdk25-opencl}"
GPU_JAR="${GPU_JAR:-$HOME/craton-gpu.jar}"
N="${N:-16777216}"           # 2^24, the size the published tables use
REPS="${REPS:-5}"
RT_W="${RT_W:-7680}"; RT_H="${RT_H:-4320}"; RT_ITERS="${RT_ITERS:-3}"
INT_CHAIN="${INT_CHAIN:-48}"; DBL_CHAIN="${DBL_CHAIN:-64}"
SWEEP_INT="${SWEEP_INT:-192 768}"; SWEEP_DBL="${SWEEP_DBL:-256 1024}"
ROWS="${ROWS:-int dbl fma dot dotl ray}"
TIMEOUT_S="${TIMEOUT_S:-900}"
# HS_RUN=0 does not run HotSpot at all (a 768-division chain takes it 86 s a call). Correctness is then
# checked against REF_SUMS, lines of `<row id> <checksum>`: Java's arithmetic is deterministic, so a
# HotSpot checksum recorded on one machine is the reference on every other.
HS_RUN="${HS_RUN:-1}"; REF_SUMS="${REF_SUMS:-}"
TV_DESC="${TV_DESC:-OpenCL backend over ROCm}"
REF_N="${REF_N:-16777216}"; REF_RT="${REF_RT:-7680x4320}"
ref_sum() {
  [ -f "$REF_SUMS" ] || return 0
  if [ "$1" = ray ]; then [ "${RT_W}x${RT_H}" = "$REF_RT" ] || return 0; else [ "$N" = "$REF_N" ] || return 0; fi
  awk -v k="$1" '$1==k{print $2}' "$REF_SUMS"
}
WORK="${WORK:-$ROOT/bench-work}"
OUT="${OUT:-$ROOT/bench-results-$(date +%Y%m%d-%H%M%S).md}"

CLS="$WORK/cls"; TCLS="$WORK/tcls"; GEN="$WORK/gen"; TGEN="$WORK/tgen"; DUMPS="$WORK/dumps"
mkdir -p "$CLS" "$TCLS" "$GEN" "$TGEN" "$DUMPS"

for f in "$CV" "$JDK/bin/java" "$JDK/bin/javac"; do
  isexe "$f" || { echo "ERROR: missing $f" >&2; exit 1; }
done
[ "$HS_RUN" = 1 ] || [ -f "$REF_SUMS" ] || { echo "ERROR: HS_RUN=0 needs REF_SUMS=<file of reference checksums>" >&2; exit 1; }
HAVE_TV=1; [ -f "$TVHOME/bin/tornado" ] || HAVE_TV=0
[ -f "$GPU_JAR" ] || echo "WARN: $GPU_JAR missing - the ray-tracer row will not compile" >&2

echo "[env] cratonvm : $CV"
echo "[env] hotspot  : $("$JDK/bin/java" -version 2>&1 | head -1)"
echo "[env] tornado  : $([ $HAVE_TV = 1 ] && cat "$TVHOME/etc/tornado.release" 2>/dev/null | head -2 | tr '\n' ' ' || echo none)"
echo "[env] N=$N reps=$REPS"
"$CV" --gpu-info 2>&1 | head -3

# ── build every class every arm needs ─────────────────────────────────────────
GEN_PY="$HERE/gen-chain.py"
for k in $INT_CHAIN $SWEEP_INT; do
  python3 "$GEN_PY" "$BENCH/GpuDivChain.java" "$k" "$GEN" >/dev/null
  python3 "$GEN_PY" "$TWINS/TornadoDivChain.java" "$k" "$TGEN" >/dev/null
done
for k in $DBL_CHAIN $SWEEP_DBL; do
  python3 "$GEN_PY" "$BENCH/GpuFloatDivChain.java" "$k" "$GEN" >/dev/null
  python3 "$GEN_PY" "$TWINS/TornadoFloatDivChain.java" "$k" "$TGEN" >/dev/null
done

# The committed dot twin crashes TornadoVM's scheduler (see gen-dot-variants.py);
# the variants find out which departure from the tested idiom is responsible.
python3 "$HERE/gen-dot-variants.py" "$TWINS/TornadoDotBench.java" "$TGEN" >/dev/null

echo "[build] bench classes ..."
"$JDK/bin/javac" -cp "$GPU_JAR" -d "$CLS" \
  "$GEN"/*.java "$BENCH/GpuComputeWarm.java" "$BENCH/GpuCompute.java" \
  "$BENCH/GpuDotBench.java" "$BENCH/GpuDotLongBench.java" "$BENCH/RayTracerKernel.java" "$BENCH/FrameDiff.java" \
  || { echo "[build] FAILED" >&2; exit 1; }

if [ $HAVE_TV = 1 ]; then
  echo "[build] tornado twins (-g is required: no LocalVariableTable = silent sequential run) ..."
  # shellcheck disable=SC2086
  "$JDK/bin/javac" -g -cp "$TVHOME/share/java/tornado/*" -d "$TCLS" \
    "$TGEN"/*.java "$TWINS/TornadoGpuCompute.java" "$TWINS/TornadoDotBench.java" "$TWINS/TornadoDotLongBench.java" \
    "$TWINS/RayTracerTornado.java" 2>&1 | tail -5
  [ "${PIPESTATUS[0]}" = 0 ] || { echo "[build] tornado twins FAILED - that column reads n/a"; HAVE_TV=0; }
fi

# ── runners ───────────────────────────────────────────────────────────────────
val() { grep -oE "(^|[[:space:]])$1=[^[:space:]]+" <<<"$2" | head -1 | sed -E "s/.*$1=//"; }
PKG=uk.ac.manchester.tornado.examples

run_hs() { # class args...
  local c="$1"; shift
  timeout "$TIMEOUT_S" "$JDK/bin/java" -Xmx8g -cp "$CLS$PSEP$GPU_JAR" "$c" "$@" 2>/dev/null
}
ERR="$WORK/cv.err"
run_cv() { # class args... ; engagement witness count is written to $WORK/cv.witness
  local c="$1"; shift
  local out
  out=$(RUST_LOG="cratonvm_vm::runtime::offload=info" CRATONVM_GPU_TRACE_BYTES=1 \
        timeout "$TIMEOUT_S" "$CV" --gpu --gpu-min-work 1 --java-home "$JDK" --Xmx 8g \
        -cp "$CLS$PSEP$GPU_JAR" "$c" "$@" 2>"$ERR")
  grep -cE "H2D=[0-9]+ bytes" "$ERR" >"$WORK/cv.witness"
  printf '%s\n' "$out"
}
run_tv() { # class args...
  [ $HAVE_TV = 1 ] || return 0
  local c="$1"; shift
  # RayTracerTornado has no package; every other twin is in $PKG.
  [ "$c" = RayTracerTornado ] || c="$PKG.$c"
  env -u MSYS_NO_PATHCONV -u MSYS2_ARG_CONV_EXCL JAVA_HOME="${TV_JDK:-$JDK}" TORNADOVM_HOME="$TVHOME" \
    timeout "$TIMEOUT_S" bash "$TVHOME/bin/tornado" --jvm="-Xmx8g" --classpath "$TCLS" "$c" "$@" 2>&1
}

declare -A HS CVV TV SUM_H SUM_C SUM_T ENG NOTE TVVIA TVWHY
ORDER=()

# row <id> <label> <hs/cv class> <tornado class> <ms key> <checksum key> [args...]
row() {
  local id="$1" label="$2" cls="$3" tcls="$4" mk="$5" ck="$6"; shift 6
  local args=("$@") out
  [ ${#args[@]} -eq 0 ] && args=("$N" "$REPS")
  ORDER+=("$id"); NOTE[$id]="$label"
  printf '  %-44s' "$label"
  if [ "$HS_RUN" = 1 ]; then
    out=$(run_hs "$cls" "${args[@]}");  HS[$id]=$(val "$mk" "$out");  SUM_H[$id]=$(val "$ck" "$out")
    printf ' hs=%-8s' "${HS[$id]:-FAIL}"
  else
    HS[$id]=""; SUM_H[$id]="$(ref_sum "$id")"; printf ' hs=%-8s' "skipped"
  fi
  out=$(run_cv "$cls" "${args[@]}");  CVV[$id]=$(val "$mk" "$out"); SUM_C[$id]=$(val "$ck" "$out"); ENG[$id]=$(cat "$WORK/cv.witness")
  printf ' craton=%-8s engaged=%-3s' "${CVV[$id]:-FAIL}" "${ENG[$id]}"
  out=$(run_tv "$tcls" "${args[@]}"); TV[$id]=$(val "$mk" "$out");  SUM_T[$id]=$(val "$ck" "$out")
  grep -q "Bailout" <<<"$out" && TV[$id]="BAILED-OUT-TO-CPU"
  # The committed twin produced nothing: try the named variants, in order, and
  # say which one answered. A variant only counts if it reproduces HotSpot's
  # checksum, otherwise it is measuring a different computation.
  if [ -z "${TV[$id]}" ] && [ -n "${ALT_TV:-}" ]; then
    for alt in $ALT_TV; do
      out=$(run_tv "$alt" "${args[@]}"); ms=$(val "$mk" "$out"); cs=$(val "$ck" "$out")
      if [ -n "$ms" ] && [ "$cs" = "${SUM_H[$id]}" ]; then
        TV[$id]="$ms"; SUM_T[$id]="$cs"; TVVIA[$id]="$alt"; break
      fi
    done
    # Neither answered: run the diagnostic once so the report can say whether the
    # supported idiom itself works on this backend. Never a table value.
    if [ -z "${TV[$id]}" ] && [ -n "${DIAG_TV:-}" ]; then
      out=$(run_tv "$DIAG_TV" "${args[@]}")
      if [ -n "$(val "$mk" "$out")" ]; then NOTE_DIAG="$DIAG_TV ran: the idiom works, so the failing departure is in the committed twin"
      else NOTE_DIAG="$DIAG_TV also failed: the idiom itself does not work on this backend"; fi
      echo; echo "  [diag] $NOTE_DIAG"
    fi
  fi
  TVWHY[$id]="${TV_WHY:-}"
  printf ' tornado=%-8s%s' "${TV[$id]:-n/a}" "${TVVIA[$id]:+ (via ${TVVIA[$id]})}"
  if [ -n "${SUM_H[$id]}" ] && [ "${SUM_H[$id]}" = "${SUM_C[$id]:-}" ]; then echo ' checksum=match'
  elif [ -z "${SUM_H[$id]}" ] && [ "$HS_RUN" != 1 ]; then echo ' checksum=unchecked (no reference at this size)'
  else echo " checksum=MISMATCH(hs=${SUM_H[$id]:-?} craton=${SUM_C[$id]:-?})"; fi
}

has() { [[ " $ROWS " == *" $1 "* ]]; }

echo
echo "=== rows, N=$N, best of $REPS ==="
has int && row "int$INT_CHAIN" "Integer div-chain ($INT_CHAIN divs/elem)" "GpuDivChain$INT_CHAIN" "TornadoDivChain$INT_CHAIN" divchain_ms DIV_CHECKSUM
has dbl && row "dbl$DBL_CHAIN" "Double div-chain ($DBL_CHAIN divs/elem)" "GpuFloatDivChain$DBL_CHAIN" "TornadoFloatDivChain$DBL_CHAIN" fdivchain_ms FDIV_CHECKSUM
has fma && row fma "128 multiply-adds/elem (data-dependent multiplier)" GpuComputeWarm TornadoGpuCompute heavy_ms COMPUTE_CHECKSUM
# TornadoVM 5.2.0 cannot compile a reduction whose accumulator is wider than its input
# (int -> long): ReduceGPUSnippets reaches `forBoxed`, which is "unimplemented". Checked
# locally on the OpenCL backend across five shapes; long->long, int->int and
# long*long->long all compile and are correct. The committed 300-statement twin hides
# that behind a different crash (it overruns TornadoVM's 256-task table), so the runner
# uses the single-statement twin and reports the real cause.
has dot && TV_WHY="TornadoVM does not implement an int->long reduction (ReduceGPUSnippets.forBoxed: unimplemented; verified on 5.2.0 OpenCL locally, and 4.0.1 PTX fails the same way per the 2026-08-02 audit in TornadoDotBench.java); long->long, int->int and long*long->long reductions compile and are correct" \
  row dot "Dot-product reduction (int*int -> long, x300/elem)" GpuDotBench TornadoDotBenchFold dot_ms DOT_CHECKSUM
# The same dot product on a shape every arm can run (long[]*long[] -> long), identical
# source in all three arms, so the row compares speed instead of showing one "n/a".
has dotl && row dotl "Dot-product reduction (long*long -> long, x300/elem; same source in all arms)" GpuDotLongBench TornadoDotLongBench dot_ms DOT_CHECKSUM

if [ "${SWEEP:-0}" = 1 ]; then
  echo; echo "=== longer chains: where the GPU/CPU gap widens ==="
  has int && for k in $SWEEP_INT; do
    row "int$k" "Integer div-chain ($k divs/elem)" "GpuDivChain$k" "TornadoDivChain$k" divchain_ms DIV_CHECKSUM; done
  has dbl && for k in $SWEEP_DBL; do
    row "dbl$k" "Double div-chain ($k divs/elem)" "GpuFloatDivChain$k" "TornadoFloatDivChain$k" fdivchain_ms FDIV_CHECKSUM; done
fi

# Ray tracer reports mean/best/checksum on a RAYTRACER_RESULT line and can dump the
# frame, so a checksum disagreement is diagnosable with FrameDiff.
if has ray; then
  id=ray; ORDER+=("$id"); NOTE[$id]="Ray tracer kernel, ${RT_W}x${RT_H} ($((RT_W * RT_H / 1000000)).$(( (RT_W * RT_H / 100000) % 10 ))M px)"
  printf '  %-44s' "${NOTE[$id]}"
  a=("$RT_W" "$RT_H" "$RT_ITERS")
  if [ "$HS_RUN" = 1 ]; then
    out=$(run_hs RayTracerKernel "${a[@]}" "$DUMPS/hs.bin"); l=$(grep RAYTRACER_RESULT <<<"$out")
    HS[$id]=$(val best_ms "$l"); SUM_H[$id]=$(val checksum "$l"); printf ' hs=%-8s' "${HS[$id]:-FAIL}"
    dcv=("$DUMPS/cv.bin"); dtv=("$DUMPS/tv.bin")
  else
    HS[$id]=""; SUM_H[$id]="$(ref_sum ray)"; printf ' hs=%-8s' "skipped"; dcv=(); dtv=()
  fi
  out=$(run_cv RayTracerKernel "${a[@]}" "${dcv[@]}"); l=$(grep RAYTRACER_RESULT <<<"$out")
  CVV[$id]=$(val best_ms "$l"); SUM_C[$id]=$(val checksum "$l"); ENG[$id]=$(cat "$WORK/cv.witness")
  printf ' craton=%-8s engaged=%-3s' "${CVV[$id]:-FAIL}" "${ENG[$id]}"
  out=$(run_tv RayTracerTornado "${a[@]}" "${dtv[@]}"); l=$(grep RAYTRACER_RESULT <<<"$out")
  TV[$id]=$(val best_ms "$l"); SUM_T[$id]=$(val checksum "$l")
  grep -q "Bailout" <<<"$out" && TV[$id]="BAILED-OUT-TO-CPU"
  printf ' tornado=%-8s' "${TV[$id]:-n/a}"
  if [ "${SUM_H[$id]:-}" = "${SUM_C[$id]:-x}" ]; then echo ' checksum=match'
  elif [ "$HS_RUN" != 1 ] && [ -z "${SUM_H[$id]}" ]; then echo ' checksum=unchecked (no reference at this size)'
  elif [ "$HS_RUN" != 1 ]; then echo " checksum=MISMATCH(ref=${SUM_H[$id]:-?} craton=${SUM_C[$id]:-?})"
  else
    v=$("$JDK/bin/java" -cp "$CLS" FrameDiff "$RT_W" "$RT_H" "$DUMPS/hs.bin" "$DUMPS/cv.bin" 2>/dev/null | sed -n 's/.*verdict=//p')
    echo " checksum=MISMATCH framediff=${v:-?}"
  fi
fi

# ── report ────────────────────────────────────────────────────────────────────
ratio() { awk -v a="$1" -v b="$2" 'BEGIN{ if (a+0>0 && b+0>0) { r=a/b; printf (r>=100? "%.0fx" : "%.1fx"), r } else printf "n/a" }'; }
{
  echo "# AMD bench — $(date -u +%Y-%m-%dT%H:%MZ)"
  echo
  echo "$("$CV" --gpu-info 2>&1 | head -2 | tr '\n' ' ')"
  if [ "$HS_RUN" = 1 ]; then hsdesc="HotSpot: $("$JDK/bin/java" -version 2>&1 | head -1)"
  else hsdesc="HotSpot: not run; correctness checked against the reference checksums in $(basename "$REF_SUMS")"; fi
  echo "$hsdesc. TornadoVM: $TV_DESC. N=$N, best of $REPS, warm."
  echo
  echo "| Kernel | HotSpot C2 | TornadoVM GPU | CratonVM GPU | vs HotSpot | vs TornadoVM | checks |"
  echo "|---|---|---|---|---|---|---|"
  for id in "${ORDER[@]}"; do
    h="${HS[$id]:-}"; c="${CVV[$id]:-}"; t="${TV[$id]:-}"
    chk="match"; [ "${SUM_H[$id]:-a}" = "${SUM_C[$id]:-b}" ] || chk="CHECKSUM MISMATCH"
    [ "$HS_RUN" != 1 ] && [ -z "${SUM_H[$id]:-}" ] && chk="unchecked (no reference at this size)"
    if [ "${ENG[$id]:-0}" = 0 ]; then chk="$chk; ENGAGED=0 (ran on CPU?)"; vh="n/a"; vt="n/a"
    else vh=$(ratio "$h" "$c"); vt=$(ratio "$t" "$c"); fi
    # A speed next to a wrong answer is not a result.
    case "$chk" in "CHECKSUM MISMATCH"*) vh="n/a"; vt="n/a";; esac
    case "$t" in "") tcell="n/a";; *[0-9]) tcell="$t ms";; *) tcell="$t";; esac
    [ -n "${TVVIA[$id]:-}" ] && tcell="$tcell (twin: ${TVVIA[$id]#TornadoDotBench})"
    [ -n "${SUM_T[$id]:-}" ] && [ "${SUM_T[$id]}" != "${SUM_H[$id]:-}" ] && chk="$chk; TORNADOVM CHECKSUM DIFFERS FROM HOTSPOT"
    if [ "$HS_RUN" = 1 ]; then hcell="${h:-FAIL} ms"; else hcell="not run"; vh="n/a"; fi
    printf '| %s | %s | %s | **%s ms** | **%s** | **%s** | %s |\n' \
      "${NOTE[$id]}" "$hcell" "$tcell" "${c:-FAIL}" "$vh" "$vt" "$chk"
  done
  for id in "${ORDER[@]}"; do
    [ -z "${TV[$id]:-}" ] && [ -n "${TVWHY[$id]:-}" ] && echo "* ${NOTE[$id]}: TornadoVM n/a. ${TVWHY[$id]}."
  done
  echo
  echo "Checksums: HotSpot (or reference) / CratonVM / TornadoVM"
  for id in "${ORDER[@]}"; do
    printf -- '- %s: `%s` / `%s` / `%s`\n' "${NOTE[$id]}" "${SUM_H[$id]:--}" "${SUM_C[$id]:--}" "${SUM_T[$id]:--}"
  done
} | tee "$OUT"
echo; echo "wrote $OUT"
