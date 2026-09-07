#!/usr/bin/env bash
# Re-run exactly the four array-kernel rows the README, BENCHMARK.md and both
# product presentations quote, so those documents can be unified against one
# measurement instead of three vintages.
#
#   bash bench-gpu/wait-for-quiet.sh && bash bench-gpu/rerun-table-rows.sh
#
# Why this exists rather than run-gpu-comparison.sh: that script sweeps five
# sizes and measures GpuCompute + GpuProbe, which is not the row set the
# published tables carry, and it times `GpuCompute` -- the COLD harness. The
# 2026-09-02 rerun recorded that trap: run cold, the 128-multiply-add row reads
# 67 ms for CratonVM against a warm 8 ms, because the single timed call carries
# the whole first-call compile (analyze, lower, ptxas, module load), while its
# TornadoVM twin warms up before its timer. The two arms were not measuring the
# same thing. `GpuComputeWarm` is that row's warm harness and is what this uses.
#
# MEASURED 2026-09-07, and the reason this file names GpuComputeWarmSelf rather
# than GpuComputeWarm: the offload gate only analyses the ENTRY class. Run
# `GpuComputeWarm`, whose main() calls `GpuCompute.heavy` in another class, and
# `GpuCompute.heavy` never appears in --print-gpu-decisions at all -- not
# rejected, never considered -- and the row reads 2611-2934 ms on the CPU. Move
# the identical kernel into the class that owns main() and the same run reads
# 8 ms with an Eligible verdict and a bit-identical checksum. That is a real
# CratonVM defect, filed separately; GpuComputeWarmSelf exists so this table can
# measure the kernel rather than the defect.
#
# Every bench here has the same shape: best-of-REPS, printing `<key>_ms=` and a
# `<KEY>_CHECKSUM=`. The checksum is the point -- a speed number whose checksum
# does not match HotSpot is not a result.
set -u
export MSYS2_ARG_CONV_EXCL='*' MSYS_NO_PATHCONV=1

ROOT="${ROOT:-C:/craton/cratonvm}"
CV_GPU="${CV_GPU:-$ROOT/target-gpu/release/cratonvm.exe}"
JDK="${JDK:-C:/Program Files/Eclipse Adoptium/jdk-25.0.3.9-hotspot}"
HS="$JDK/bin/java.exe"
JAVAC="$JDK/bin/javac.exe"

TVBASE="${TVBASE:-C:/craton/tornadovm/tornadovm-4.0.1-jdk25-ptx}"
TVM="${TVM:-C:/craton/tornadovm/jdk-25.0.3/bin/java.exe}"
TVJAVAC="${TVJAVAC:-C:/craton/tornadovm/jdk-25.0.3/bin/javac.exe}"
ARGFILE="$TVBASE/tornado-argfile"
TVJARS="$TVBASE/share/java/tornado"

GO="${GO:-$ROOT/bench-gpu}"
TVSRC="${TVSRC:-$ROOT/bench-tornado}"
N="${N:-16777216}"          # 2^24, the size every published table quotes
REPS="${REPS:-5}"
TIMEOUT_S="${TIMEOUT_S:-300}"
OUT="${1:-$ROOT/bench-gpu/results/table-rows-$(date +%Y%m%d-%H%M%S).md}"

for f in "$CV_GPU" "$HS" "$JAVAC"; do
  [ -x "$f" ] || { echo "ERROR: missing $f" >&2; exit 1; }
done

echo "[env] cratonvm : $CV_GPU"
echo "[env] hotspot  : $("$HS" -version 2>&1 | head -1)"
echo "[env] N        : $N   reps: $REPS"
"$CV_GPU" --gpu-info 2>&1 | head -1

# ── compile ───────────────────────────────────────────────────────────────────
echo "[build] bench-gpu classes ..."
"$JAVAC" -d "$GO" \
  "$GO/GpuDivChain.java" "$GO/GpuFloatDivChain.java" \
  "$GO/GpuComputeWarmSelf.java" "$GO/GpuDotBench.java" \
  || { echo "[build] FAILED" >&2; exit 1; }

TORNADO_OK=1
if [ -f "$ARGFILE" ] && [ -x "$TVJAVAC" ]; then
  echo "[build] tornado twins ..."
  # -g is required: TornadoVM's PTX compiler reads LocalVariableTable for
  # parameter names and silently produces nothing without it.
  "$TVJAVAC" -g --module-path "$TVJARS" \
    --add-modules tornado.annotation,tornado.api \
    --patch-module tornado.examples="$GO" -d "$GO" \
    "$TVSRC/TornadoDivChain.java" "$TVSRC/TornadoFloatDivChain.java" \
    "$TVSRC/TornadoGpuCompute.java" "$TVSRC/TornadoDotBench.java" 2>&1 | tail -3
  [ "${PIPESTATUS[0]}" = "0" ] || { echo "[build] tornado FAILED — that column will read n/a"; TORNADO_OK=0; }
else
  echo "[build] tornado unavailable (no argfile or javac) — that column will read n/a"
  TORNADO_OK=0
fi

# ── runners ───────────────────────────────────────────────────────────────────
run_hs()  { timeout "$TIMEOUT_S" "$HS" -Xmx8g -cp "$GO" "$1" "$N" "$REPS" 2>/dev/null; }
run_gpu() { timeout "$TIMEOUT_S" "$CV_GPU" --gpu --java-home "$JDK" --Xmx 8g -cp "$GO" "$1" "$N" "$REPS" 2>/dev/null; }
run_tvm() {
  [ "$TORNADO_OK" = "1" ] || return 0
  timeout "$TIMEOUT_S" "$TVM" "@$ARGFILE" -Xmx8g \
    --patch-module tornado.examples="$GO" \
    -m "tornado.examples/uk.ac.manchester.tornado.examples.$1" "$N" 2>/dev/null
}
val() { echo "$2" | grep -oE "$1=[^[:space:]]+" | head -1 | sed "s/$1=//"; }

declare -A MS SUM
row() {  # label | craton-class | tornado-class | ms-key | checksum-key
  local label="$1" cls="$2" tcls="$3" mk="$4" ck="$5" out
  printf '  %-34s' "$label"
  out=$(run_hs  "$cls"); MS["$label,hs"]=$(val "$mk" "$out");  SUM["$label,hs"]=$(val "$ck" "$out")
  printf ' hs=%-8s' "${MS[$label,hs]:-FAIL}"
  out=$(run_gpu "$cls"); MS["$label,cv"]=$(val "$mk" "$out");  SUM["$label,cv"]=$(val "$ck" "$out")
  printf ' craton=%-8s' "${MS[$label,cv]:-FAIL}"
  if [ "$TORNADO_OK" = "1" ]; then
    out=$(run_tvm "$tcls"); MS["$label,tv"]=$(val "$mk" "$out"); SUM["$label,tv"]=$(val "$ck" "$out")
    printf ' tornado=%-8s' "${MS[$label,tv]:-unimplemented}"
  fi
  if [ -n "${SUM[$label,hs]:-}" ] && [ "${SUM[$label,hs]:-}" = "${SUM[$label,cv]:-}" ]; then
    printf ' checksum=match\n'; MS["$label,ok"]=match
  else
    printf ' checksum=MISMATCH(hs=%s craton=%s)\n' "${SUM[$label,hs]:-?}" "${SUM[$label,cv]:-?}"; MS["$label,ok"]=MISMATCH
  fi
}

echo
echo "=== four array kernels, N=$N, best of $REPS ==="
row "int div-chain"        GpuDivChain      TornadoDivChain      divchain_ms  DIV_CHECKSUM
row "double div-chain"     GpuFloatDivChain TornadoFloatDivChain fdivchain_ms FDIV_CHECKSUM
row "128 multiply-adds"    GpuComputeWarmSelf TornadoGpuCompute  heavy_ms     COMPUTE_CHECKSUM
row "dot-product reduction" GpuDotBench     TornadoDotBench      dot_ms       DOT_CHECKSUM

# ── report ────────────────────────────────────────────────────────────────────
mkdir -p "$(dirname "$OUT")"
{
  echo "# Table rows re-run — $(date +%Y-%m-%d\ %H:%M)"
  echo
  echo "RTX 2060 (sm_75). CratonVM: \`target-gpu/release/cratonvm.exe\`."
  echo "HotSpot: $("$HS" -version 2>&1 | head -1)."
  echo "TornadoVM 4.0.1-jdk25-ptx. N = $N (2^24), best of $REPS, warm."
  echo
  echo "| Kernel | HotSpot C2 | TornadoVM | CratonVM GPU | vs HotSpot | vs TornadoVM | checksum |"
  echo "|---|---|---|---|---|---|---|"
  for label in "int div-chain" "double div-chain" "128 multiply-adds" "dot-product reduction"; do
    hs="${MS[$label,hs]:-}"; cv="${MS[$label,cv]:-}"; tv="${MS[$label,tv]:-}"
    vh="n/a"; vt="n/a"
    [ -n "$hs" ] && [ -n "$cv" ] && [ "$cv" != "0" ] && vh="$(awk -v a="$hs" -v b="$cv" 'BEGIN{printf "%.1fx", a/b}')"
    [ -n "$tv" ] && [ -n "$cv" ] && [ "$cv" != "0" ] && vt="$(awk -v a="$tv" -v b="$cv" 'BEGIN{printf "%.1fx", a/b}')"
    printf '| %s | %s ms | %s | **%s ms** | **%s** | **%s** | %s |\n' \
      "$label" "${hs:-FAIL}" "${tv:-unimplemented}" "${cv:-FAIL}" "$vh" "$vt" "${MS[$label,ok]:-?}"
  done
  echo
  echo "Checksums (bit-exactness is the gate, not the speed):"
  echo
  echo "| Kernel | HotSpot | CratonVM | TornadoVM |"
  echo "|---|---|---|---|"
  for label in "int div-chain" "double div-chain" "128 multiply-adds" "dot-product reduction"; do
    printf '| %s | `%s` | `%s` | `%s` |\n' "$label" \
      "${SUM[$label,hs]:--}" "${SUM[$label,cv]:--}" "${SUM[$label,tv]:--}"
  done
} > "$OUT"

echo
echo "wrote $OUT"
cat "$OUT"
