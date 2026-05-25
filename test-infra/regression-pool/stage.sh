#!/usr/bin/env bash
# stage.sh — idempotent download/extract of every app in the regression pool.
#
# Usage:
#   bash test-infra/regression-pool/stage.sh [--force]
#   bash test-infra/regression-pool/stage.sh <name>          # one app only
#   bash test-infra/regression-pool/stage.sh --list          # print URLS table
#
# Apps land under test-infra/regression-pool/apps/ — gitignored, permanent
# (the gauntlet's apps/ directory is auto-managed and not safe for this).
# If an app's distro is already on disk under apps/<name>/ (from the gauntlet),
# it's symlinked into apps_root rather than re-downloaded.

set -e

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
APPS_ROOT="$REPO_ROOT/test-infra/regression-pool/apps"
GAUNTLET_APPS="$REPO_ROOT/apps"

mkdir -p "$APPS_ROOT"

# ---- URLS table -----------------------------------------------------------
# Each line: <name>|<url>|<extract_kind>|<top_dir_or_jar_name>
# extract_kind: tgz / tar.gz / tar.xz / zip / jar (single file) / war
URLS=$(cat <<'EOF'
wildfly-32.0.1.Final|https://github.com/wildfly/wildfly/releases/download/32.0.1.Final/wildfly-32.0.1.Final.tar.gz|tgz|wildfly-32.0.1.Final
wildfly-40.0.0.Final|https://github.com/wildfly/wildfly/releases/download/40.0.0.Final/wildfly-40.0.0.Final.tar.gz|tgz|wildfly-40.0.0.Final
keycloak-16.1.1|https://github.com/keycloak/keycloak/releases/download/16.1.1/keycloak-16.1.1.tar.gz|tgz|keycloak-16.1.1
keycloak-26.2.4|https://github.com/keycloak/keycloak/releases/download/26.2.4/keycloak-26.2.4.tar.gz|tgz|keycloak-26.2.4
kafka_2.13-3.7.0|https://archive.apache.org/dist/kafka/3.7.0/kafka_2.13-3.7.0.tgz|tgz|kafka_2.13-3.7.0
apache-hadoop-3.4.0|https://archive.apache.org/dist/hadoop/common/hadoop-3.4.0/hadoop-3.4.0.tar.gz|tgz|hadoop-3.4.0
apache-hbase-2.5.10|https://archive.apache.org/dist/hbase/2.5.10/hbase-2.5.10-bin.tar.gz|tgz|hbase-2.5.10
apache-cassandra-4.1.4|https://archive.apache.org/dist/cassandra/4.1.4/apache-cassandra-4.1.4-bin.tar.gz|tgz|apache-cassandra-4.1.4
apache-activemq-5.18.3|https://archive.apache.org/dist/activemq/5.18.3/apache-activemq-5.18.3-bin.tar.gz|tgz|apache-activemq-5.18.3
felix-framework-7.0.5|https://dlcdn.apache.org/felix/org-apache-felix-framework-7.0.5.tar.gz|tgz|felix-framework-7.0.5
apache-tomcat-10.1.31|https://archive.apache.org/dist/tomcat/tomcat-10/v10.1.31/bin/apache-tomcat-10.1.31.tar.gz|tgz|apache-tomcat-10.1.31
solr-9.5.0|https://archive.apache.org/dist/solr/solr/9.5.0/solr-9.5.0.tgz|tgz|solr-9.5.0
jenkins-2.452.3|https://repo.jenkins-ci.org/releases/org/jenkins-ci/main/jenkins-core/2.452.3/jenkins-core-2.452.3.jar|jar|jenkins-core.jar
spring-boot-4.0.6|maven-central:org.springframework.boot:spring-boot:4.0.6|jar|spring-boot-4.0.6.jar
EOF
)

# ---- helpers --------------------------------------------------------------

list_urls() {
    echo "$URLS"
}

# Check if app appears staged at APPS_ROOT/<top_dir_or_jar>
is_staged() {
    local target="$1"
    [ -e "$APPS_ROOT/$target" ]
}

# If the gauntlet's apps/ has the same distro, mklink/junction it in
# rather than re-downloading.
link_from_gauntlet() {
    local app="$1"
    local target="$2"
    if [ -e "$GAUNTLET_APPS/$target" ] && [ ! -e "$APPS_ROOT/$target" ]; then
        echo "  [LINK] $target from $GAUNTLET_APPS/"
        # On Windows MSYS, cp -r is more reliable than ln -s for jboss-modules
        cp -r "$GAUNTLET_APPS/$target" "$APPS_ROOT/$target"
        return 0
    fi
    return 1
}

download_and_extract() {
    local name="$1"
    local url="$2"
    local kind="$3"
    local target="$4"
    local cache_dir="$APPS_ROOT/.downloads"
    mkdir -p "$cache_dir"

    # Resolve maven-central pseudo-URL
    if [[ "$url" == maven-central:* ]]; then
        IFS=':' read -r _ groupId artifactId version <<< "$url"
        local group_path="${groupId//.//}"
        url="https://repo1.maven.org/maven2/$group_path/$artifactId/$version/${artifactId}-${version}.jar"
    fi

    local cached="$cache_dir/$(basename "$url")"
    if [ ! -f "$cached" ] || [ ! -s "$cached" ]; then
        echo "  [DL]   $url"
        curl -sL --fail -o "$cached.tmp" "$url" && mv "$cached.tmp" "$cached" || {
            echo "  [FAIL] curl failed for $name ($url)"
            rm -f "$cached.tmp"
            return 1
        }
    fi

    case "$kind" in
        tgz|tar.gz)
            echo "  [EXTRACT tgz] $cached → $APPS_ROOT/"
            # Some distros (hadoop, glassfish) ship .so/.dylib symlinks
            # that MSYS tar can't create without admin. Strip the symlink
            # entries and proceed — they're native libs we don't run.
            tar -xzf "$cached" -C "$APPS_ROOT/" 2> >(grep -v "Cannot create symlink" >&2) || {
                # Real failure (not just symlinks). Retry with --skip if
                # the top-level dir at least exists.
                if [ -d "$APPS_ROOT/$target" ]; then
                    echo "  [WARN] tar errors but $target staged enough"
                else
                    echo "  [FAIL] tar -xzf"
                    return 1
                fi
            }
            ;;
        tar.xz)
            tar -xJf "$cached" -C "$APPS_ROOT/" || return 1
            ;;
        zip)
            unzip -q -o "$cached" -d "$APPS_ROOT/" || return 1
            ;;
        jar|war)
            mkdir -p "$APPS_ROOT/$(dirname "$target")"
            cp "$cached" "$APPS_ROOT/$target"
            ;;
        *)
            echo "  [FAIL] unknown kind $kind"
            return 1
            ;;
    esac
    return 0
}

# ---- main loop ------------------------------------------------------------

ONLY_NAME=""
FORCE=0
for arg in "$@"; do
    case "$arg" in
        --force) FORCE=1 ;;
        --list)  list_urls; exit 0 ;;
        --help|-h) sed -n '2,/^# Apps land/p' "$0" | sed 's/^# \?//'; exit 0 ;;
        --*) echo "unknown flag $arg" >&2; exit 2 ;;
        *) ONLY_NAME="$arg" ;;
    esac
done

OK=0
FAIL=0
SKIP=0
while IFS='|' read -r name url kind target; do
    [ -z "$name" ] && continue
    [[ "$name" =~ ^# ]] && continue
    if [ -n "$ONLY_NAME" ] && [ "$name" != "$ONLY_NAME" ]; then
        continue
    fi
    echo "[$name]"
    if [ "$FORCE" -eq 0 ] && is_staged "$target"; then
        echo "  [SKIP] already at $APPS_ROOT/$target"
        SKIP=$((SKIP+1))
        continue
    fi
    if [ "$FORCE" -eq 0 ] && link_from_gauntlet "$name" "$target"; then
        OK=$((OK+1))
        continue
    fi
    if download_and_extract "$name" "$url" "$kind" "$target"; then
        echo "  [OK]   $target"
        OK=$((OK+1))
    else
        FAIL=$((FAIL+1))
    fi
done <<< "$URLS"

echo
echo "stage.sh summary: ok=$OK skip=$SKIP fail=$FAIL"
[ "$FAIL" -gt 0 ] && exit 1
exit 0
