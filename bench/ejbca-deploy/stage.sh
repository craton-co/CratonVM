#!/usr/bin/env bash
# bench/ejbca-deploy/stage.sh
# WP8.11 — Stage EJBCA CE 8.3.2 onto a checked-out WildFly tree.
#
# Behaviour:
#   1. Delegate to bench/wildfly-boot/stage.sh (WP8.10 — must exist).
#   2. Locate or download the EJBCA CE 8.3.2 binary distribution zip.
#   3. Extract the zip into bench/ejbca-deploy/staged/ejbca/.
#   4. Copy dist/ejbca.ear into staged/wildfly/standalone/deployments/.
#   5. Copy conf/standalone-ejbca.xml (if present) into
#      staged/wildfly/standalone/configuration/.
#   6. Drop a marker file recording which standalone-*.xml the runner
#      should pass to --server-config.
#
# Exit codes:
#   0   OK
#   10  curl/unzip/javac missing
#   11  WP8.10 prerequisite (bench/wildfly-boot/) missing
#   12  EJBCA dist not locatable (and no offline cache)
#   13  EAR extraction failed
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$HERE/../.." && pwd)"
WILDFLY_FIXTURE="$REPO_ROOT/bench/wildfly-boot"
STAGED_DIR="$HERE/staged"
EJBCA_STAGED="$STAGED_DIR/ejbca"
WILDFLY_STAGED="$STAGED_DIR/wildfly"
STAGE_LOG="$STAGED_DIR/stage.log"
SERVER_CONFIG_MARKER="$STAGED_DIR/server-config.txt"

mkdir -p "$STAGED_DIR" "$EJBCA_STAGED"
: > "$STAGE_LOG"

log() { echo "stage-ejbca-deploy: $*" | tee -a "$STAGE_LOG"; }

# Step 1 — chain to WP8.10's stage.sh.
if [[ ! -x "$WILDFLY_FIXTURE/stage.sh" ]]; then
    log "ERROR WP8.10 prerequisite missing: $WILDFLY_FIXTURE/stage.sh not found"
    log "  this fixture chains to bench/wildfly-boot/ which Agent B owns"
    exit 11
fi
log "delegating WildFly stage to $WILDFLY_FIXTURE/stage.sh"
if ! bash "$WILDFLY_FIXTURE/stage.sh" >> "$STAGE_LOG" 2>&1; then
    log "ERROR bench/wildfly-boot/stage.sh failed; see $STAGE_LOG"
    exit 11
fi

# Discover WildFly home (Agent B's stage.sh stages at staged/wildfly/).
WILDFLY_HOME=""
for cand in \
    "$WILDFLY_FIXTURE/staged/wildfly" \
    "$WILDFLY_FIXTURE/staged/wildfly-32.0.1.Final" \
    "$WILDFLY_FIXTURE/staged"/wildfly-*; do
    if [[ -d "$cand" && -f "$cand/jboss-modules.jar" ]]; then
        WILDFLY_HOME="$cand"; break
    fi
done
if [[ -z "$WILDFLY_HOME" ]]; then
    log "ERROR WildFly home not found under $WILDFLY_FIXTURE/staged/"
    exit 11
fi
log "using WILDFLY_HOME=$WILDFLY_HOME"

rm -rf "$WILDFLY_STAGED"
if ln -s "$WILDFLY_HOME" "$WILDFLY_STAGED" 2>/dev/null; then
    log "linked staged/wildfly -> $WILDFLY_HOME"
else
    log "symlink unavailable; copying WildFly tree (this is slow)"
    cp -R "$WILDFLY_HOME" "$WILDFLY_STAGED"
fi

# Step 2 — locate or fetch EJBCA dist.
EJBCA_VERSION="${EJBCA_VERSION:-8.3.2}"
EJBCA_ZIP="$STAGED_DIR/ejbca_ce_${EJBCA_VERSION//./_}.zip"

# UNVERIFIED candidate URLs.
EJBCA_URL_CANDIDATES=(
    "https://github.com/Keyfactor/ejbca-ce/releases/download/v${EJBCA_VERSION}/ejbca_ce_${EJBCA_VERSION//./_}.zip"
    "https://sourceforge.net/projects/ejbca/files/ejbca6/${EJBCA_VERSION}/ejbca_ce_${EJBCA_VERSION//./_}.zip/download"
)

LOCAL_CACHE="${EJBCA_DIST_CACHE:-/c/craton/ejbca-ce/dist/ejbca_ce_${EJBCA_VERSION//./_}.zip}"
if [[ -f "$LOCAL_CACHE" ]]; then
    log "using local EJBCA dist cache: $LOCAL_CACHE"
    cp -f "$LOCAL_CACHE" "$EJBCA_ZIP"
elif [[ -f "$EJBCA_ZIP" ]]; then
    log "EJBCA zip already present: $EJBCA_ZIP (skip download)"
else
    if ! command -v curl >/dev/null 2>&1; then
        log "ERROR curl missing; cannot download EJBCA"
        exit 10
    fi
    fetched=""
    for url in "${EJBCA_URL_CANDIDATES[@]}"; do
        log "trying $url"
        if curl -fL --retry 2 --connect-timeout 30 -o "$EJBCA_ZIP" "$url" >> "$STAGE_LOG" 2>&1; then
            fetched="$url"; break
        else
            log "  failed (continuing)"
            rm -f "$EJBCA_ZIP"
        fi
    done
    if [[ -z "$fetched" ]]; then
        log "ERROR could not download EJBCA from any candidate URL"
        log "  set EJBCA_DIST_CACHE=/path/to/ejbca_ce_${EJBCA_VERSION//./_}.zip to bypass"
        exit 12
    fi
    log "downloaded from $fetched -> $EJBCA_ZIP"
fi

# Step 3 — extract.
if ! command -v unzip >/dev/null 2>&1; then
    log "ERROR unzip missing"
    exit 10
fi
log "extracting EJBCA zip into $EJBCA_STAGED"
rm -rf "$EJBCA_STAGED"
mkdir -p "$EJBCA_STAGED"
if ! unzip -q -o "$EJBCA_ZIP" -d "$EJBCA_STAGED" >> "$STAGE_LOG" 2>&1; then
    log "ERROR unzip failed; see $STAGE_LOG"
    exit 13
fi

if [[ ! -d "$EJBCA_STAGED/dist" ]]; then
    inner="$(find "$EJBCA_STAGED" -maxdepth 2 -type d -name 'dist' | head -n1)"
    if [[ -n "$inner" ]]; then
        EJBCA_TOP="$(dirname "$inner")"
        log "flattening EJBCA top dir from $EJBCA_TOP"
    else
        log "ERROR no dist/ directory inside the EJBCA zip"
        exit 13
    fi
else
    EJBCA_TOP="$EJBCA_STAGED"
fi

# Step 4 — drop ejbca.ear.
EAR_SRC="$EJBCA_TOP/dist/ejbca.ear"
if [[ ! -f "$EAR_SRC" ]]; then
    EAR_SRC="$(find "$EJBCA_TOP" -maxdepth 4 -name 'ejbca.ear' -type f | head -n1)"
fi
if [[ -z "$EAR_SRC" || ! -f "$EAR_SRC" ]]; then
    log "ERROR no ejbca.ear in distribution — 8.x may require 'ant deployear'"
    log "  this fixture only supports the binary-EAR path; build-from-source out of scope"
    exit 12
fi
DEPLOY_DIR="$WILDFLY_STAGED/standalone/deployments"
mkdir -p "$DEPLOY_DIR"
cp -f "$EAR_SRC" "$DEPLOY_DIR/ejbca.ear"
touch "$DEPLOY_DIR/ejbca.ear.dodeploy"
log "staged ejbca.ear -> $DEPLOY_DIR/"

# Step 5 — standalone-ejbca.xml.
SERVER_CONFIG="standalone-ejbca.xml"
SE_SRC=""
for cand in \
    "$EJBCA_TOP/dist/wildfly/standalone-ejbca.xml" \
    "$EJBCA_TOP/conf/standalone-ejbca.xml" \
    "$EJBCA_TOP/dist/standalone-ejbca.xml"; do
    if [[ -f "$cand" ]]; then SE_SRC="$cand"; break; fi
done

CONFIG_DIR="$WILDFLY_STAGED/standalone/configuration"
if [[ -n "$SE_SRC" ]]; then
    cp -f "$SE_SRC" "$CONFIG_DIR/standalone-ejbca.xml"
    log "staged standalone-ejbca.xml from $SE_SRC"
else
    log "WARN standalone-ejbca.xml not in dist; falling back to standalone.xml"
    log "  (real install needs JNDI EjbcaDS + Hibernate dialect H2; this v1 won't init the DB)"
    SERVER_CONFIG="standalone.xml"
fi
echo "$SERVER_CONFIG" > "$SERVER_CONFIG_MARKER"
log "server-config = $SERVER_CONFIG"

log "OK staged at $STAGED_DIR"
exit 0
