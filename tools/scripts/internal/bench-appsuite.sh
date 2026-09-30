#!/usr/bin/env bash
# e2e app-suite timing: runs the slf4j tests + passing probes as one suite
# under a single VM and reports total wall-clock time.
#
# Suite (8 main classes, one JVM process each — cold start dominates):
#   EnumTest, CipherProbe, SigProbe, CleanerProbe,
#   LogTest, ResTest, ServiceLoaderManual (slf4j),
#   CglibProbe
#
# Usage:  bench-appsuite.sh <hotspot|cratonvm|cratonvm-gpu|tornado>

set +e

ROOT="${ROOT:-$(git -C "$(dirname "${BASH_SOURCE[0]}")" rev-parse --show-toplevel 2>/dev/null || echo C:/craton/CratonVM)}"
# Relative cache path: the TornadoVM launcher mis-parses a `;`-separated
# classpath whose entries carry a Windows drive-letter colon (C:/...), so
# the bench `cd`s into ROOT and uses drive-letter-free relative paths.
BC=".bench-cache"
JH="${JAVA_HOME:-C:/Program Files/Java/jdk-25}"
RJVM="$ROOT/target/release/cratonvm.exe"

SLF="apps/slf4j;$BC/slf4j-api-2.0.13.jar;$BC/slf4j-simple-2.0.13.jar"
CGL="apps/cglib_probe/classes;$BC/cglib-nodep-3.3.0.jar"

MODE="${1:?usage: bench-appsuite.sh <hotspot|cratonvm|cratonvm-gpu|tornado>}"

if [ "$MODE" = "tornado" ]; then
    # shellcheck disable=SC1091
    source /c/craton/tornadovm/setvars.sh >/dev/null 2>&1
fi

cd "$ROOT" || exit 2

run_one() {
    local cp="$1" cls="$2"
    case "$MODE" in
        hotspot)      "$JH/bin/java.exe" -cp "$cp" "$cls" ;;
        cratonvm)     "$RJVM" --java-home "$JH" -c "$cp" "$cls" ;;
        cratonvm-gpu) "$RJVM" --java-home "$JH" --gpu -c "$cp" "$cls" ;;
        tornado)      tornado --classpath "$cp" "$cls" ;;
        *) echo "unknown mode: $MODE" >&2; exit 2 ;;
    esac
}

pass=0
fail=0
t0=$(date +%s%N)

for spec in \
    "apps/enumtest|EnumTest" \
    "apps/cipher_probe|CipherProbe" \
    "apps/sig_probe/classes|SigProbe" \
    "apps/cleaner_probe/classes|CleanerProbe" \
    "$SLF|LogTest" \
    "$SLF|ResTest" \
    "$SLF|ServiceLoaderManual" \
    "$CGL|CglibProbe"
do
    cp="${spec%%|*}"
    cls="${spec##*|}"
    out=$(run_one "$cp" "$cls" 2>&1)
    rc=$?
    if [ $rc -eq 0 ] && ! echo "$out" | grep -q 'Exception in thread'; then
        pass=$((pass + 1))
        echo "  PASS  $cls"
    else
        fail=$((fail + 1))
        echo "  FAIL  $cls (rc=$rc)"
    fi
done

t1=$(date +%s%N)
total_ms=$(( (t1 - t0) / 1000000 ))

echo "----"
echo "mode=$MODE pass=$pass fail=$fail total_ms=$total_ms"
