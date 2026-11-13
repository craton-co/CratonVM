#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
# Copyright 2024-2026 Craton Software Company

# The whole measurement session, in one command, ON the AMD box.
#
#   bash tools/gpu/amd-remote/go.sh             # rows 1-5 + longer chains + Llama 1B and 8B
#   bash tools/gpu/amd-remote/go.sh --quick     # rows 1-5 only, 3 reps, no sweep
#   bash tools/gpu/amd-remote/go.sh --rows      # rows only (with the sweep)
#   bash tools/gpu/amd-remote/go.sh --llama     # Llama only
#   FORCE=1 bash tools/gpu/amd-remote/go.sh     # skip the pre-flight (you checked)
#
# ── PRE-FLIGHT ────────────────────────────────────────────────────────
#
# This box can be shared. A benchmark that starts while another job is on
# the GPU measures the neighbour, and — worse — disturbs the neighbour's own
# numbers. So before anything runs this looks, and REFUSES to start when
#
#   * the GPU reports more than IDLE_PCT percent use, or
#   * a job that is not ours is running (cargo bench, craton-bolt, another
#     cratonvm/java process outside this tree).
#
# `FORCE=1` overrides it, deliberately and loudly. The decision to measure
# on a busy box belongs to a person, not to this script.
set -u

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$HERE/../../.." && pwd)"
MODE="${1:-all}"
IDLE_PCT="${IDLE_PCT:-5}"
STAMP="$(date +%Y%m%d-%H%M%S)"
RES="${RES:-$ROOT/results-$STAMP}"
JDK="${JDK:-/usr/lib/jvm/java-25-openjdk-amd64}"
export PATH="/opt/rocm/bin:$PATH"

say() { printf '%s\n' "$*"; }

# ── pre-flight ────────────────────────────────────────────────────────
say "== pre-flight"
say "load: $(uptime | sed 's/.*load average/load average/')"
gpu_use="$(rocm-smi --showuse 2>/dev/null | grep -oE 'GPU use \(%\): *[0-9]+' | grep -oE '[0-9]+$' | head -1)"
say "GPU use: ${gpu_use:-unknown}%"
# Anything that looks like somebody else's benchmark. The brackets keep this
# command's own line from matching itself.
others="$(ps -eo pid,etime,pcpu,args | grep -E '[c]argo (bench|test|build)|[c]raton-bolt|[b]olt-amd|[r]ocprof' \
          | grep -v "$ROOT" || true)"
if [ -n "$others" ]; then
  say "other jobs:"; printf '%s\n' "$others" | cut -c1-180 | sed 's/^/  /'
fi
busy=0
[ "${gpu_use:-0}" -gt "$IDLE_PCT" ] 2>/dev/null && busy=1
[ -n "$others" ] && busy=1
if [ "$busy" = 1 ] && [ "${FORCE:-0}" != 1 ]; then
  say ""
  say "REFUSING TO START: the box is not idle (see above). Wait, or ask whoever is on it,"
  say "or re-run with FORCE=1 if you have checked and accept contaminated numbers."
  exit 3
fi
[ "$busy" = 1 ] && say "!! FORCE=1: measuring on a busy box. Treat every number below as contaminated."

for need in "$ROOT/target-fast/release/cratonvm" "$JDK/bin/java" "$HOME/craton-gpu.jar"; do
  [ -e "$need" ] || { say "missing prerequisite: $need"; exit 2; }
done
mkdir -p "$RES"
say "results -> $RES"
{
  echo "# Run $STAMP"; echo
  echo "revision: $(cat "$ROOT/.shipped-rev" 2>/dev/null || echo unknown)"
  rocminfo 2>/dev/null | grep -E 'Marketing Name|Name: +gfx' | head -2 | sed 's/^ *//'
  echo "load at start: $(uptime | sed 's/.*load average: //')"
  echo "GPU use at start: ${gpu_use:-unknown}%"
} > "$RES/environment.md"

# ── measurement ───────────────────────────────────────────────────────
run_rows() {
  say; say "== rows (HotSpot | TornadoVM | CratonVM)"
  if [ "$MODE" = "--quick" ]; then
    REPS=3 OUT="$RES/rows.md" WORK="$RES/work" bash "$HERE/bench-rows.sh"
  else
    SWEEP=1 OUT="$RES/rows.md" WORK="$RES/work" bash "$HERE/bench-rows.sh"
  fi
}
run_llama() {
  for spec in "Llama-3.2-1B-Instruct-F16.gguf:3:128" "Meta-Llama-3.1-8B-Instruct.f16.gguf:2:64"; do
    f="${spec%%:*}"; rest="${spec#*:}"; rounds="${rest%%:*}"; toks="${rest##*:}"
    [ -f "$HOME/models/$f" ] || { say "skip $f (not downloaded)"; continue; }
    say; say "== llama: $f ($rounds rounds, $toks tokens)"
    MODELS="$HOME/models/$f" ROUNDS="$rounds" TOKENS="$toks" OUT="$RES/llama-${f%.gguf}.md" \
      LOGS="$RES/llama-logs" bash "$HERE/bench-llama.sh"
  done
}
case "$MODE" in
  --rows|--quick) run_rows ;;
  --llama)        run_llama ;;
  *)              run_rows; run_llama ;;
esac

{
  echo "# Summary $STAMP"; echo
  cat "$RES/environment.md"; echo
  for f in "$RES"/rows.md "$RES"/llama-*.md; do [ -f "$f" ] && { cat "$f"; echo; }; done
} > "$RES/summary.md"
say; say "== done: $RES/summary.md"
