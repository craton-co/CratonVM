#!/usr/bin/env bash
# RI.16 — Keycloak 16 `-mp` boot past the XML parser AIOOBE.
# Mirrors the reproducer command for RA.1 in
# docs/roadmap-any-java-app.md — passing means the Buffer.checkIndex
# AIOOBE is gone; downstream ModuleNotFoundException is acceptable.

source "$(dirname "$0")/common.sh"

KC_DIR="$FIXTURE_CACHE/keycloak-16.1.1"
if [[ ! -d "$KC_DIR" ]]; then
    TGZ="$FIXTURE_CACHE/keycloak-16.1.1.tar.gz"
    smoke_download "https://github.com/keycloak/keycloak/releases/download/16.1.1/keycloak-16.1.1.tar.gz" "$TGZ"
    tar -C "$FIXTURE_CACHE" -xzf "$TGZ"
fi

SMOKE_TIMEOUT=180 smoke_run_cratonvm \
    --Xmx 2g \
    --jar "$KC_DIR/jboss-modules.jar" \
    -- -mp "$KC_DIR/modules" org.jboss.as.standalone "-Djboss.home.dir=$KC_DIR"

# RA.1 success criterion: no `java/nio/Buffer.checkIndex pc=11` AIOOBE.
smoke_forbid_signal "java/nio/Buffer.*checkIndex.*pc=11"
