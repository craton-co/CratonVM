#!/usr/bin/env bash
# DIFFERENTIAL soak of the compact TLAB allocation shape, take 3.
#
# The corpus is mostly BENCHMARKS, and a benchmark cannot testify about a
# correctness change: it prints wall times, rates and self-tuned iteration
# counts that differ run to run whatever the switch does. Take 2 tried to mask
# those; the mask kept missing units (`ns/elem`) and could never fix a probe
# that varies its own line count.
#
# So the filter is measured, not hand-written: run each workload TWICE with the
# switch OFF and require those two runs to agree before comparing an ON run
# against them. A workload that is not reproducible under a fixed configuration
# is excluded, by its own evidence, with no list to maintain.
#
#   soak3.sh <cratonvm.exe> <classdir> <gcflag> [timeout_s]
set -u
CV="$1"; DIR="$2"; GC="${3:--XX:+UseGenerationalGC}"; TMO="${4:-60}"
J="C:/Program Files/Eclipse Adoptium/jdk-25.0.3.9-hotspot"
HERE="$(cd "$(dirname "$0")" && pwd)"
TAG=$(echo "$GC" | tr -cd 'A-Za-z')
OUT="$HERE/soak3-$TAG"; rm -rf "$OUT"; mkdir -p "$OUT"

run() { # env outfile class -> rc
  timeout "$TMO" env "$1" "$CV" --java-home "$J" $GC -Xmx1g -c "$DIR" "$3" \
    > "$2" 2>/dev/null
  echo $?
}
strip() { grep -v '^\[cratonvm\]' "$1" | tr -d '\r'; }

agree=0; divergent=0; failed=0; nondet=0
for cls in $(cd "$DIR" && ls *.class 2>/dev/null | grep -v '\$' | sed 's/\.class$//'); do
  a=$(run CRATONVM_AB_NOOP=1 "$OUT/$cls.a" "$cls")
  b=$(run CRATONVM_AB_NOOP=1 "$OUT/$cls.b" "$cls")
  if [ "$a" -ne 0 ] || [ "$b" -ne 0 ]; then
    failed=$((failed+1)); rm -f "$OUT/$cls".[ab]; continue
  fi
  if ! diff -q <(strip "$OUT/$cls.a") <(strip "$OUT/$cls.b") >/dev/null 2>&1; then
    # Not reproducible with the switch OFF: it cannot testify about the switch.
    nondet=$((nondet+1)); rm -f "$OUT/$cls".[ab]; continue
  fi
  c=$(run CRATONVM_COMPACT_TLAB_ALLOC=1 "$OUT/$cls.c" "$cls")
  if [ "$c" -ne "$a" ]; then
    divergent=$((divergent+1)); echo "RC-DIVERGENCE $cls off=$a on=$c"; continue
  fi
  if diff -q <(strip "$OUT/$cls.a") <(strip "$OUT/$cls.c") >/dev/null 2>&1; then
    agree=$((agree+1)); rm -f "$OUT/$cls".[abc]
  else
    divergent=$((divergent+1)); echo "OUTPUT-DIVERGENCE $cls"
  fi
done
echo "SOAK3 $GC: deterministic-and-agree=$agree divergent=$divergent nondeterministic=$nondet failed-off=$failed"
