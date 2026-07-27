#!/usr/bin/env bash
# Run a set of keycloak JUnit test classes under CratonVM, one VM per class.
# Usage: run-kc-tests.sh <cp-file> <out-dir> <class1> [class2 ...]
# Env: KC_DISABLE_JIT=1 to run with CRATONVM_DISABLE_JIT=1
set +e
CP_FILE="$1"; shift
OUT="$1"; shift
CP="$(cat "$CP_FILE")"
if [ -z "${ROOT:-}" ]; then
    ROOT="$(git -C "$(dirname "${BASH_SOURCE[0]}")" rev-parse --show-toplevel 2>/dev/null)"
    if [ -z "$ROOT" ]; then
        echo "error: not inside a git checkout and \$ROOT is unset; set ROOT=<repo root>" >&2
        exit 2
    fi
fi
RJVM="${RJVM:-$ROOT/target/release/cratonvm.exe}"
JDK="${JDK:-${JAVA_HOME:?set JDK or JAVA_HOME to a JDK 25 home}}"
# NOTE: assertions (-ea) are intentionally NOT enabled by default. The native
# `desiredAssertionStatus` honours CRATONVM_ENABLE_ASSERTIONS, but turning it on
# globally fires asserts inside CratonVM's synthetic MethodHandle/MemberName
# layer (MemberName.vminfoIsConsistent) during serialization-constructor setup,
# breaking more tests than the handful of `assert`-dependent ones it fixes.
JITENV=""
[ "${KC_DISABLE_JIT:-0}" = "1" ] && JITENV="CRATONVM_DISABLE_JIT=1"
mkdir -p "$OUT"
pass=0; fail=0; crash=0
: > "$OUT/summary.txt"
for cls in "$@"; do
    out="$OUT/${cls}.log"
    timeout --foreground -k 5 150 env $JITENV "$RJVM" --java-home "$JDK" \
        --stack-dump-on-timeout 0 --Xmx 1g -c "$CP" \
        org.junit.runner.JUnitCore "$cls" < /dev/null > "$out" 2>&1
    rc=$?
    if [ $rc -eq 0 ] && grep -q "^OK" "$out"; then
        res="PASS "; pass=$((pass+1))
    elif grep -qE "^Tests run:.*Failures" "$out"; then
        res="FAIL "; fail=$((fail+1))
    else
        res="CRASH"; crash=$((crash+1))
    fi
    err=$(grep -aiE 'Exception|Error|SEGV|panic|ARRAY-LEN|undersized|stale pointer|NoClassDef|NoSuchMethod|AbstractMethod|VerifyError|not implemented|unimplemented|fatal error' "$out" \
        | grep -avE 'at org\.|at java\.|at jdk\.|at sun\.|jar_signer' | head -1 | head -c 150)
    line=$(printf "%-6s rc=%-3s %-58s %s" "$res" "$rc" "$cls" "$err")
    echo "$line" | tee -a "$OUT/summary.txt"
done
echo "----"
echo "PASS=$pass FAIL=$fail CRASH=$crash" | tee -a "$OUT/summary.txt"
