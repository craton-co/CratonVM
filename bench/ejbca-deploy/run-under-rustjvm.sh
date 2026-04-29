#!/usr/bin/env bash
# bench/ejbca-deploy/run-under-rustjvm.sh
# WP8.11 — Boot WildFly+EJBCA under target/release/rustjvm and capture
# the first failure (or the install-wizard 200 OK).
#
# Delegates to bench/wildfly-boot/run-under-rustjvm.sh and passes
# --server-config=$(cat staged/server-config.txt). Polls
# https://localhost:8443/ejbca/ for up to 180 s.
#
# Exit codes:
#   0  install wizard responded 200 (success — EJBCA is up)
#   1  WildFly booted but ejbca.ear failed to deploy
#   2  WildFly itself failed before reaching deployment scan
#   10 missing prereqs
#   11 WP8.10 prereq absent
#   124 timeout waiting for :8443
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$HERE/../.." && pwd)"
WILDFLY_FIXTURE="$REPO_ROOT/bench/wildfly-boot"
STAGED_DIR="$HERE/staged"
RUN_LOG="$STAGED_DIR/run.log"
RUN_STDERR="$STAGED_DIR/run.stderr"
RUN_STDOUT="$STAGED_DIR/run.stdout"
SERVER_CONFIG_MARKER="$STAGED_DIR/server-config.txt"
ACCEPTANCE_LOG="$STAGED_DIR/acceptance.log"

mkdir -p "$STAGED_DIR"
: > "$RUN_LOG"; : > "$ACCEPTANCE_LOG"

log()  { echo "run-ejbca-deploy: $*" | tee -a "$RUN_LOG"; }
fail() { log "FAIL $*"; exit "${2:-1}"; }

if [[ ! -x "$WILDFLY_FIXTURE/run-under-rustjvm.sh" ]]; then
    fail "WP8.10 prereq missing: $WILDFLY_FIXTURE/run-under-rustjvm.sh" 11
fi
if [[ ! -f "$STAGED_DIR/server-config.txt" ]]; then
    fail "stage not run: $SERVER_CONFIG_MARKER missing — run stage.sh first" 10
fi
if ! command -v curl >/dev/null 2>&1; then
    fail "curl missing" 10
fi

SERVER_CONFIG="$(cat "$SERVER_CONFIG_MARKER")"
log "delegating boot to $WILDFLY_FIXTURE/run-under-rustjvm.sh"
log "  --server-config=$SERVER_CONFIG"
log "  WILDFLY_HOME=$STAGED_DIR/wildfly"

export WILDFLY_HOME="$STAGED_DIR/wildfly"
export SERVER_CONFIG="$SERVER_CONFIG"

( bash "$WILDFLY_FIXTURE/run-under-rustjvm.sh" > "$RUN_STDOUT" 2> "$RUN_STDERR" ) &
BOOT_PID=$!
log "rust-jvm boot PID=$BOOT_PID"

cleanup() {
    if kill -0 "$BOOT_PID" 2>/dev/null; then
        log "killing boot pid=$BOOT_PID"
        kill "$BOOT_PID" 2>/dev/null || true
        sleep 1
        kill -9 "$BOOT_PID" 2>/dev/null || true
    fi
}
trap cleanup EXIT

TIMEOUT_S="${EJBCA_BOOT_TIMEOUT_S:-180}"
deadline=$((SECONDS + TIMEOUT_S))
saw_deployed=0
http_status=0
while (( SECONDS < deadline )); do
    if ! kill -0 "$BOOT_PID" 2>/dev/null; then
        log "boot pid exited before acceptance"
        break
    fi
    if grep -qE 'Deployed "ejbca.ear"|WFLYSRV0010.*ejbca\.ear' "$RUN_STDOUT" "$RUN_STDERR" 2>/dev/null; then
        saw_deployed=1
        log "saw 'Deployed ejbca.ear' marker"
    fi
    code="$(curl -sk -o /dev/null -w '%{http_code}' --max-time 5 https://localhost:8443/ejbca/ || echo 000)"
    if [[ "$code" == "200" || "$code" == "302" ]]; then
        http_status="$code"
        log "ACCEPT https://localhost:8443/ejbca/ -> HTTP $code"
        echo "HTTP $code" >> "$ACCEPTANCE_LOG"
        cleanup
        exit 0
    fi
    sleep 3
done

log "timeout after ${TIMEOUT_S}s; saw_deployed=$saw_deployed http_status=$http_status"
log "  --- last 60 lines of run.stderr ---"
tail -n 60 "$RUN_STDERR" >> "$RUN_LOG" 2>/dev/null || true

if grep -q 'WFLYSRV0025.*started in' "$RUN_STDOUT" "$RUN_STDERR" 2>/dev/null; then
    log "WildFly started but ejbca.ear deployment failed"
    exit 1
elif grep -q 'WFLYSRV' "$RUN_STDOUT" "$RUN_STDERR" 2>/dev/null; then
    log "WildFly partial boot; deployment scan not reached"
    exit 2
else
    log "WildFly didn't even produce WFLYSRV log lines — JVM-level failure"
    exit 124
fi
