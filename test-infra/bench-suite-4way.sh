#!/usr/bin/env bash
# Numeric benchmark suite x 4-way (CratonVM CPU, CratonVM GPU, HotSpot, TornadoVM).
# One process per (benchmark, variant) so a crash doesn't lose the batch.
# Captures wall ms (VM-internal RESULT line), checksum, and crash/timeout state.
set +e
ROOT="C:/craton/CratonVM"
CV_CPU="$ROOT/target/release/cratonvm.exe"
CV_GPU="$ROOT/target-gpu/release/cratonvm.exe"
JDK="C:/Program Files/Java/jdk-25"
HOTSPOT="$JDK/bin/java.exe"
TORNADO="C:/craton/tornadovm/jdk-25.0.3/bin/java.exe"
TORNADO_ARGF="@C:/craton/tornadovm/tornadovm-4.0.1-jdk25-ptx/tornado-argfile"
BENCH="$ROOT/bench"
HEAP="8g"
TIMEOUT="${TIMEOUT:-300}"
TS=$(date +%Y%m%d-%H%M%S)
OUT="$ROOT/test-infra/suite-results/bench-suite-4way-$TS.tsv"
mkdir -p "$(dirname "$OUT")"
printf "bench\tvariant\trc\tstate\tms\tchecksum\n" > "$OUT"

VARIANTS="${VARIANTS:-cratonvm-cpu cratonvm-gpu hotspot tornadovm}"
BENCHES="${BENCHES:-arith1500M fib44 sieve250k matrix600 bintrees18 vadd2_28}"

prefix() {
  case "$1" in
    cratonvm-cpu) JPRE=("$CV_CPU" --java-home "$JDK" --stack-dump-on-timeout 0 --Xmx "$HEAP");;
    cratonvm-gpu) JPRE=("$CV_GPU" --gpu --java-home "$JDK" --stack-dump-on-timeout 0 --Xmx "$HEAP");;
    hotspot)      JPRE=("$HOTSPOT" -Xmx$HEAP);;
    tornadovm)    JPRE=("$TORNADO" "$TORNADO_ARGF" -Xmx$HEAP);;
  esac
}

for b in $BENCHES; do
  echo "================= benchmark: $b ================="
  for v in $VARIANTS; do
    prefix "$v"
    log=$(mktemp)
    timeout "$TIMEOUT" "${JPRE[@]}" -cp "$BENCH" BenchSuite "$b" < /dev/null > "$log" 2>&1
    rc=$?
    ms=$(grep -aoE "ms=[0-9]+" "$log" | head -1 | grep -oE "[0-9]+")
    chk=$(grep -aoE "checksum=-?[0-9]+" "$log" | head -1 | sed 's/checksum=//')
    state=OK
    if [ "$rc" -eq 124 ]; then state=TIMEOUT
    elif [ -z "$ms" ]; then
      state=CRASH
      # capture a short crash signature
      sig=$(grep -aiE "stack overflow|SIGSEGV|SEGV|panic|OutOfMemory|implausible|Exception|fatal|abort" "$log" | grep -avE "^\s*at " | head -1 | head -c 90)
    fi
    printf "  %-13s rc=%-3s %-8s %7s ms  chk=%s %s\n" "$v" "$rc" "$state" "${ms:-—}" "${chk:-—}" "${sig:-}"
    printf "%s\t%s\t%s\t%s\t%s\t%s\n" "$b" "$v" "$rc" "$state" "${ms:-NA}" "${chk:-NA}" >> "$OUT"
    sig=""
    rm -f "$log"
  done
done
echo
echo "Results: $OUT"
