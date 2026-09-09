#!/usr/bin/env bash
# DIFFERENTIAL soak of one behavioural JIT switch.
#
# Generalises `compact-tlab-soak.sh`, whose method is the part worth keeping and
# whose switch was hardcoded. A default-off JIT flag cannot be flipped on an
# argument; it is flipped on evidence that turning it on changes no answer, and
# this is what produces that evidence.
#
# The corpus is mostly BENCHMARKS, and a benchmark cannot testify about a
# correctness change: it prints wall times, rates and self-tuned iteration
# counts that differ run to run whatever the switch does. So the filter is
# MEASURED rather than hand-written — each workload runs twice with the switch
# OFF and must agree with itself before an ON run is compared against it. A
# workload that is not reproducible under a fixed configuration excludes itself,
# by its own evidence, with no list to maintain.
#
#   jit-flag-soak.sh <cratonvm.exe> <classpath-root>[:<package/dir>] <NAME=VALUE> [gcflag] [timeout_s]
#
# The second argument is the classpath root, optionally followed by `:` and
# the sub-directory holding the classes. That split is not decoration: the
# corpus worth soaking (`vm/tests/resources/cratonvm`) declares `package
# cratonvm`, so running it needs the PARENT on the classpath and the package
# on the class name. Passing the class directory as the classpath makes every
# workload fail to load, and the harness then reports `failed-off=142` and a
# soak that proved nothing -- which it did, on the first run of this script.
#
# Reading the result: `divergent=0` is the pass. `nondeterministic=N` is not a
# failure but it IS a bound on what the run proved — those N workloads said
# nothing. A soak whose corpus is mostly nondeterministic has not soaked
# anything, so print both and read both.
set -u
CV="$1"; SPEC="$2"; FLAG="$3"; GC="${4:--XX:+UseGenerationalGC}"; TMO="${5:-60}"
CP="${SPEC%%:*}"; SUB="${SPEC#*:}"
if [ "$SUB" = "$SPEC" ]; then SUB=""; PKG=""; DIR="$CP"
else DIR="$CP/$SUB"; PKG="$(echo "$SUB" | tr '/' '.')."; fi
J="C:/Program Files/Eclipse Adoptium/jdk-25.0.3.9-hotspot"
HERE="$(cd "$(dirname "$0")" && pwd)"
TAG=$(echo "${FLAG%%=*}$GC" | tr -cd 'A-Za-z0-9')
OUT="$HERE/flagsoak-$TAG"; rm -rf "$OUT"; mkdir -p "$OUT"

run() { # env outfile class -> rc
  timeout "$TMO" env "$1" "$CV" --java-home "$J" $GC -Xmx1g -c "$CP" "$3" \
    > "$2" 2>/dev/null
  echo $?
}
# A class that fails to LOAD exits 0 with an error on stderr, so the return
# code alone cannot filter the corpus. Ask the output.
loaded() { ! grep -q "Could not find or load main class" "$1"; }
strip() { grep -v '^\[cratonvm\]' "$1" | tr -d '\r'; }

agree=0; divergent=0; failed=0; nondet=0
for cls in $(cd "$DIR" && ls *.class 2>/dev/null | grep -v '\$' | sed 's/\.class$//'); do
  # `CRATONVM_AB_NOOP=1` is the OFF arm's placeholder: it sets an environment
  # variable of the same shape as the one under test and no meaning, so the two
  # arms differ in the flag's VALUE and in nothing else about how they are run.
  a=$(run CRATONVM_AB_NOOP=1 "$OUT/$cls.a" "$PKG$cls")
  b=$(run CRATONVM_AB_NOOP=1 "$OUT/$cls.b" "$PKG$cls")
  if [ "$a" -ne 0 ] || [ "$b" -ne 0 ] || ! loaded "$OUT/$cls.a"; then
    failed=$((failed+1)); rm -f "$OUT/$cls".[ab]; continue
  fi
  if ! diff -q <(strip "$OUT/$cls.a") <(strip "$OUT/$cls.b") >/dev/null 2>&1; then
    nondet=$((nondet+1)); rm -f "$OUT/$cls".[ab]; continue
  fi
  c=$(run "$FLAG" "$OUT/$cls.c" "$PKG$cls")
  if [ "$c" -ne "$a" ]; then
    divergent=$((divergent+1)); echo "RC-DIVERGENCE $cls off=$a on=$c"; continue
  fi
  if diff -q <(strip "$OUT/$cls.a") <(strip "$OUT/$cls.c") >/dev/null 2>&1; then
    agree=$((agree+1)); rm -f "$OUT/$cls".[abc]
  else
    divergent=$((divergent+1)); echo "OUTPUT-DIVERGENCE $cls"
  fi
done
echo "FLAGSOAK $FLAG $GC: deterministic-and-agree=$agree divergent=$divergent nondeterministic=$nondet failed-off=$failed"
