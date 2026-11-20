#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
# Copyright 2024-2026 Craton Software Company

# GPULlama3 decode tok/s on the AMD box: HotSpot CPU | TornadoVM (OpenCL) | CratonVM (HIP).
#
# Runs ON the remote Linux host:
#
#   MODELS="$HOME/models/Llama-3.2-1B-Instruct-F16.gguf" bash tools/gpu/amd-remote/bench-llama.sh
#   MODELS="a.gguf b.gguf" ROUNDS=3 TOKENS=128 ARMS="hs tv cv" bash ...
#
# The arms are interleaved inside each round and the MEDIAN is reported:
# the same GPU binary has measured 11.9 and 22.2 tok/s an hour apart on a
# shared host (see run-gpullama3-ab.sh), so back-to-back blocks measure the
# neighbours, not the binaries.
#
# Correctness oracle: at temperature 0 the decode is deterministic, so the
# generated text of every arm is saved and compared with HotSpot's. A GPU
# arm that is fast and says something different is not a result.
#
# CratonVM's arm is the EXPLICIT executor path (GpuExecutor / GpuArray), not
# transparent offload. On HIP it runs with graph capture OFF
# (-Dllama.craton.graph=false) because hipGraph capture is not wired up in
# the HIP backend; the CUDA number in the published table had it ON.
set -u

# `pwd -W` (Git Bash) gives C:/... which java.exe understands; elsewhere it fails and plain pwd is used.
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && { pwd -W 2>/dev/null || pwd; })"
ROOT="$(cd "$HERE/../../.." && { pwd -W 2>/dev/null || pwd; })"
case "$(uname -s)" in MINGW*|MSYS*|CYGWIN*) PSEP=';';; *) PSEP=':';; esac
isexe() { [ -x "$1" ] || [ -x "$1.exe" ]; }
# java.exe and cratonvm.exe get Windows-form paths verbatim; TornadoVM's launcher is a bash script
# that hands POSIX paths to python and NEEDS MSYS conversion, so only the first two are shielded.
noconv() { env MSYS_NO_PATHCONV=1 MSYS2_ARG_CONV_EXCL='*' "$@"; }
APP="${APP:-$HOME/GPULlama3.java}"
CV="${CV:-$ROOT/target-fast/release/cratonvm}"
JDK="${JDK:-/usr/lib/jvm/java-25-openjdk-amd64}"
TVHOME="${TVHOME:-$HOME/tornado/tornadovm-5.2.0-jdk25-opencl}"
GPU_JAR="${GPU_JAR:-$HOME/craton-gpu.jar}"
MODELS="${MODELS:-$HOME/models/Llama-3.2-1B-Instruct-F16.gguf}"
PROMPT="${PROMPT:-Why is the sky blue?}"
TOKENS="${TOKENS:-128}"
ROUNDS="${ROUNDS:-3}"
ARMS="${ARMS:-hs tv cv}"
XMX="${XMX:-48g}"                       # CratonVM heap: weights live in Java arrays first
TV_GPU_MEM="${TV_GPU_MEM:-120GB}"       # TornadoVM device allocation, MI300X has 192 GB
TV_JDK="${TV_JDK:-$JDK}"                # TornadoVM 4.0.1 must run on the JDK it ships with
TV_FLAG="${TV_FLAG:---opencl}"          # TornadoVM backend: --opencl | --ptx
TV_HEAP_MIN="${TV_HEAP_MIN:-16g}"
TV_DESC="${TV_DESC:-OpenCL}"; CV_DESC="${CV_DESC:-HIP}"
CRATON_EXTRA="${CRATON_EXTRA:--Dllama.craton.graph=false}"
TIMEOUT_S="${TIMEOUT_S:-1800}"
JAR="$APP/target/gpu-llama3-1.0.0-jdk25.jar"
LOGS="${LOGS:-$ROOT/bench-work/llama}"
OUT="${OUT:-$ROOT/bench-llama-$(date +%Y%m%d-%H%M%S).md}"
mkdir -p "$LOGS"

[ -f "$JAR" ] || { echo "ERROR: no app jar at $JAR (set APP=<GPULlama3.java dir>)" >&2; exit 2; }
isexe "$CV"  || { echo "ERROR: no cratonvm at $CV" >&2; exit 2; }

tok() { sed -n 's/.*achieved tok\/s: \([0-9,\.]*\).*/\1/p' | tr ',' '.' | sed 's/[.]$//' | tail -1; }
median() { sort -n | awk '{a[NR]=$1} END{ if (NR==0) print "FAIL"; else if (NR%2) print a[(NR+1)/2]; else printf "%.2f\n", (a[NR/2]+a[NR/2+1])/2 }'; }

run_arm() { # arm model log
  local arm="$1" model="$2" log="$3"
  case "$arm" in
    hs)
      noconv timeout "$TIMEOUT_S" "$JDK/bin/java" --enable-preview --add-modules jdk.incubator.vector \
        --enable-native-access=ALL-UNNAMED -Xmx"$XMX" -cp "$JAR" org.beehive.gpullama3.LlamaApp \
        -m "$model" -p "$PROMPT" -n "$TOKENS" --temperature 0 >"$log" 2>&1 ;;
    tv)
      [ -f "$TVHOME/bin/tornado" ] || { echo "no TornadoVM" >"$log"; return 1; }
      ( cd "$APP" && JAVA_HOME="$TV_JDK" TORNADOVM_HOME="$TVHOME" LLAMA_ROOT="$APP" \
        timeout "$TIMEOUT_S" python3 ./llama-tornado --gpu $TV_FLAG --model "$model" \
          --prompt "$PROMPT" -n "$TOKENS" --temperature 0 --gpu-memory "$TV_GPU_MEM" \
          --heap-min "$TV_HEAP_MIN" --heap-max "$XMX" >"$log" 2>&1 ) ;;
    cv)
      ( cd "$APP" && CRATONVM_GPU_APPROX_MATH=1 RUST_LOG="cratonvm_vm::runtime::offload=info" \
        noconv timeout "$TIMEOUT_S" "$CV" --gpu --java-home "$JDK" --Xmx "$XMX" \
          --add-modules jdk.incubator.vector --enable-native-access=ALL-UNNAMED \
          -cp "classes-craton$PSEP$JAR$PSEP$GPU_JAR" -Dllama.craton.gpu=true $CRATON_EXTRA \
          org.beehive.gpullama3.LlamaApp -m "$model" --temperature 0 -p "$PROMPT" -n "$TOKENS" \
          >"$log" 2>&1 ) ;;
  esac
}

{
  echo "# GPULlama3 decode — $(date -u +%Y-%m-%dT%H:%MZ)"
  echo
  echo "Prompt: \`$PROMPT\`, $TOKENS tokens, temperature 0, $ROUNDS interleaved rounds, median."
  echo
  echo "| Model | HotSpot CPU | TornadoVM GPU ($TV_DESC) | CratonVM GPU ($CV_DESC) | vs HotSpot | vs TornadoVM | text vs HotSpot |"
  echo "|---|---|---|---|---|---|---|"
} >"$OUT"

for model in $MODELS; do
  name="$(basename "$model" .gguf)"
  [ -f "$model" ] || { echo "skip: $model not found" >&2; continue; }
  declare -A V=([hs]="" [tv]="" [cv]="")
  for r in $(seq 1 "$ROUNDS"); do
    for arm in $ARMS; do
      log="$LOGS/$name.$arm.r$r.log"
      echo "[$name] round $r  $arm ..." >&2
      run_arm "$arm" "$model" "$log"
      t="$(tok <"$log")"
      echo "[$name] round $r  $arm = ${t:-FAIL} tok/s" >&2
      [ -n "$t" ] && V[$arm]+="$t"$'\n'
    done
  done
  hs="$(printf '%s' "${V[hs]}" | median)"; tv="$(printf '%s' "${V[tv]}" | median)"; cv="$(printf '%s' "${V[cv]}" | median)"
  ratio() { awk -v a="$1" -v b="$2" 'BEGIN{ if (a+0>0 && b+0>0) printf "%.1fx", a/b; else printf "n/a" }'; }
  # Exact comparison of the generated text with HotSpot's, per round (see the script).
  same="$(python3 "$HERE/compare-llama-text.py" "$LOGS" "$name" --summary 2>/dev/null || echo n/a)"
  # ARMS may leave HotSpot out (e.g. a local run against TornadoVM only).
  if [[ " $ARMS " == *" hs "* ]]; then hcell="$hs tok/s"; else hcell="not run"; hs=""; fi
  printf '| %s | %s | %s | **%s** | **%s** | **%s** | %s |\n' "$name" "$hcell" "$tv tok/s" "$cv tok/s" \
    "$(ratio "$cv" "$hs")" "$(ratio "$cv" "$tv")" "$same" >>"$OUT"
  unset V
done
cat "$OUT"
echo "logs: $LOGS"
