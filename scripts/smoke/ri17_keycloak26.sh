#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
# Copyright 2024-2026 Craton Software Company

# RI.17 — Keycloak 26 Quarkus boot.

source "$(dirname "$0")/common.sh"

KC_DIR="$FIXTURE_CACHE/keycloak-26.0.0"
if [[ ! -d "$KC_DIR" ]]; then
    TGZ="$FIXTURE_CACHE/keycloak-26.0.0.tar.gz"
    smoke_download "https://github.com/keycloak/keycloak/releases/download/26.0.0/keycloak-26.0.0.tar.gz" "$TGZ"
    tar -C "$FIXTURE_CACHE" -xzf "$TGZ"
fi

SMOKE_TIMEOUT=300 smoke_run_cratonvm \
    --Xmx 2g \
    --jar "$KC_DIR/lib/quarkus-run.jar" \
    -- start-dev --http-port=0 &
BOOT_PID=$!
for _ in $(seq 1 60); do
    if grep -q -E "(Running in development mode|Keycloak .* started)" "$SMOKE_LOG" 2>/dev/null; then
        kill "$BOOT_PID" 2>/dev/null || true
        smoke_require_signal "(Running in development mode|Keycloak .* started)"
        exit 0
    fi
    sleep 3
done
kill "$BOOT_PID" 2>/dev/null || true
echo "RI.17: FAIL — KC26 never printed boot signal" >&2
tail -n 30 "$SMOKE_LOG" >&2
exit 1
