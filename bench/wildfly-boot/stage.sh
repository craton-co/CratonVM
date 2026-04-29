#!/usr/bin/env bash
# bench/wildfly-boot/stage.sh
# WP8.10.1 — download and unpack a real WildFly 32.0.1.Final distribution
# under bench/wildfly-boot/staged/. Idempotent: skips download if cache hit
# AND sha256 matches; skips extract if staged/ has the expected layout.
#
# Behaviour:
#   * Cache dir defaults to bench/wildfly-boot/.cache/ (gitignored).
#   * Pinned URL + sha256 baked in below — bump together if WildFly is
#     rev'd. Network is the only failure mode worth retrying.
#   * Extracts the tarball into staged/, then renames the top-level
#     "wildfly-32.0.1.Final" dir to "wildfly" so downstream scripts can
#     point at $STAGED/wildfly without knowing the version.
#
# Exit codes:
#   0   staging succeeded (real or cached)
#   10  curl/wget missing AND no cached tarball
#   11  sha256 mismatch (likely partial/corrupt download)
#   12  extract failed (corrupt tarball, no tar tool)
#   13  shasum/sha256sum/certutil all unavailable
#
# Env:
#   WILDFLY_CACHE_DIR  override .cache location
#   WILDFLY_SKIP_SHA   "1" to bypass sha256 check (CI debugging only)
#
# Usage: bash bench/wildfly-boot/stage.sh
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$HERE/../.." && pwd)"

WILDFLY_VERSION="32.0.1.Final"
WILDFLY_URL="https://github.com/wildfly/wildfly/releases/download/${WILDFLY_VERSION}/wildfly-${WILDFLY_VERSION}.tar.gz"
# UNVERIFIED: sha256 below is a placeholder — agent has no network. The
# parent must replace this with the real sha256 from
# https://github.com/wildfly/wildfly/releases/tag/32.0.1.Final on first run
# (or set WILDFLY_SKIP_SHA=1 once to populate it from the actual download).
WILDFLY_SHA256="REPLACE_WITH_REAL_SHA256_ON_FIRST_RUN"

CACHE_DIR="${WILDFLY_CACHE_DIR:-$HERE/.cache}"
STAGED_DIR="$HERE/staged"
WILDFLY_HOME="$STAGED_DIR/wildfly"
TARBALL="$CACHE_DIR/wildfly-${WILDFLY_VERSION}.tar.gz"
STAMP="$STAGED_DIR/.staged-${WILDFLY_VERSION}"

mkdir -p "$CACHE_DIR" "$STAGED_DIR"

# Idempotency stamp.
if [[ -f "$STAMP" && -f "$WILDFLY_HOME/jboss-modules.jar" ]]; then
    echo "stage: WildFly $WILDFLY_VERSION already staged at $WILDFLY_HOME"
    exit 0
fi

# Download (or use cached). Prefer curl, fall back to wget.
download() {
    local url="$1" dest="$2"
    if command -v curl >/dev/null 2>&1; then
        echo "stage: curl $url -> $dest"
        curl -fL --retry 3 --retry-delay 5 -o "$dest" "$url"
    elif command -v wget >/dev/null 2>&1; then
        echo "stage: wget $url -> $dest"
        wget -O "$dest" "$url"
    else
        return 10
    fi
}

if [[ ! -f "$TARBALL" ]]; then
    if ! download "$WILDFLY_URL" "$TARBALL"; then
        echo "stage: ERROR no curl/wget AND no cached tarball at $TARBALL" >&2
        exit 10
    fi
else
    echo "stage: cache hit at $TARBALL"
fi

# sha256 check (skippable). Try shasum, sha256sum, then certutil (Windows).
compute_sha256() {
    local f="$1"
    if command -v sha256sum >/dev/null 2>&1; then
        sha256sum "$f" | awk '{print $1}'
    elif command -v shasum >/dev/null 2>&1; then
        shasum -a 256 "$f" | awk '{print $1}'
    elif command -v certutil >/dev/null 2>&1; then
        certutil -hashfile "$f" SHA256 2>/dev/null | awk 'NR==2 {gsub(/ /,""); print tolower($0)}'
    else
        return 13
    fi
}

if [[ "${WILDFLY_SKIP_SHA:-0}" != "1" ]]; then
    if ! ACTUAL_SHA="$(compute_sha256 "$TARBALL")"; then
        echo "stage: ERROR no sha256 tool (sha256sum/shasum/certutil) — set WILDFLY_SKIP_SHA=1 to bypass" >&2
        exit 13
    fi
    if [[ "$WILDFLY_SHA256" == "REPLACE_WITH_REAL_SHA256_ON_FIRST_RUN" ]]; then
        echo "stage: WARN pinned sha256 is placeholder; observed=$ACTUAL_SHA"
        echo "stage: WARN edit stage.sh and replace WILDFLY_SHA256 with the value above"
    elif [[ "$ACTUAL_SHA" != "$WILDFLY_SHA256" ]]; then
        echo "stage: ERROR sha256 mismatch" >&2
        echo "  expected: $WILDFLY_SHA256" >&2
        echo "  actual:   $ACTUAL_SHA"   >&2
        rm -f "$TARBALL"
        exit 11
    else
        echo "stage: sha256 OK ($ACTUAL_SHA)"
    fi
fi

# Extract. Tarball top-level dir is "wildfly-32.0.1.Final/"; rename to
# "wildfly/" so downstream paths are version-independent.
if ! command -v tar >/dev/null 2>&1; then
    echo "stage: ERROR no tar tool available" >&2
    exit 12
fi
rm -rf "$WILDFLY_HOME" "$STAGED_DIR/wildfly-${WILDFLY_VERSION}"
echo "stage: extracting $TARBALL into $STAGED_DIR"
if ! tar -xzf "$TARBALL" -C "$STAGED_DIR"; then
    echo "stage: ERROR tar extract failed (corrupt tarball?)" >&2
    exit 12
fi
if [[ -d "$STAGED_DIR/wildfly-${WILDFLY_VERSION}" ]]; then
    mv "$STAGED_DIR/wildfly-${WILDFLY_VERSION}" "$WILDFLY_HOME"
fi
if [[ ! -f "$WILDFLY_HOME/jboss-modules.jar" ]]; then
    echo "stage: ERROR expected $WILDFLY_HOME/jboss-modules.jar after extract" >&2
    exit 12
fi

date -u +%Y-%m-%dT%H:%M:%SZ > "$STAMP"
echo "stage: OK ($WILDFLY_HOME)"
exit 0
