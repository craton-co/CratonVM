#!/usr/bin/env bash
# =============================================================================
# gen-module-args.sh — regenerate module-args/<artifactId>.args
#
# One argfile per entry in module-scoped-classes.tsv, holding the Maven-faithful
# test classpath for that single netty module: the module's own target/classes +
# target/test-classes, then exactly what
#   mvn -o dependency:build-classpath -DincludeScope=test
# resolves for it, then this fixture dir (for CratonRunner) and the JUnit
# Platform Launcher jar (surefire supplies it under Maven, so it is not a
# declared test dependency of any module).
#
# The generated files are host-specific absolute paths, exactly like common.args,
# and are therefore NOT tracked; this generator and module-scoped-classes.tsv are.
# run-netty-suite.sh calls this automatically when an argfile is missing.
#
# Requires: mvn on PATH (source /data/toolchain/env.sh) and a populated local
# repo — it runs offline (-o), so the reactor must already have been installed.
#
# USAGE: ./gen-module-args.sh [artifactId]      (no argument = all entries)
# =============================================================================
set -uo pipefail

SELF_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
NETTY_SRC="${NETTY_SRC:-/data/cratonvm/apps/netty}"
TABLE="${NETTY_MODULE_SCOPED:-$SELF_DIR/module-scoped-classes.tsv}"
OUTDIR="$SELF_DIR/module-args"
M2="${MAVEN_REPO_LOCAL:-/data/toolchain/m2repo}"
LAUNCHER="${JUNIT_LAUNCHER_JAR:-}"

[ -f "$TABLE" ] || { echo "ERROR: table not found: $TABLE" >&2; exit 1; }
[ -d "$NETTY_SRC" ] || { echo "ERROR: netty source tree not found: $NETTY_SRC (set NETTY_SRC)" >&2; exit 1; }
command -v mvn >/dev/null || { echo "ERROR: mvn not on PATH (source /data/toolchain/env.sh)" >&2; exit 1; }

# The launcher jar is what CratonRunner itself needs; surefire owns it under
# Maven, so no netty module declares it and it has to be appended by hand. It
# must be the SAME junit-platform version the module already resolves for
# junit-platform-commons: this local repo also holds a JUnit 6 launcher, and
# pairing that with netty's 1.14.x platform gets every class rejected before it
# runs with "conflicting versions were detected".
launcher_for() {                      # $1 = the module's resolved classpath
  local ver jar
  ver="$(printf '%s' "$1" | tr ':' '\n' \
         | sed -n 's#.*/junit-platform-commons-\([0-9][^/]*\)\.jar$#\1#p' | head -1)"
  if [ -n "$ver" ]; then
    jar="$M2/org/junit/platform/junit-platform-launcher/$ver/junit-platform-launcher-$ver.jar"
    if [ -f "$jar" ]; then printf '%s' "$jar"; return 0; fi
  fi
  jar="$(find "$M2/org/junit/platform/junit-platform-launcher" \
           -name 'junit-platform-launcher-*.jar' ! -name '*-sources.jar' 2>/dev/null \
         | sort -V | head -1)"
  [ -n "$jar" ] && { printf '%s' "$jar"; return 0; }
  return 1
}

mkdir -p "$OUTDIR"
ONLY="${1:-}"                         # optional: regenerate one artifactId only
made=0 failed=0
while IFS=$'\t' read -r cls mod grp art _rest; do
  case "${cls:-}" in ''|\#*) continue;; esac
  [ -n "${art:-}" ] || continue
  if [ -n "$ONLY" ] && [ "$ONLY" != "$art" ]; then continue; fi
  out="$OUTDIR/$art.args"
  deps="$OUTDIR/.$art.cp"
  if ! ( cd "$NETTY_SRC" && mvn -o -q -pl "$mod" dependency:build-classpath \
             -DincludeScope=test -Dmdep.outputFile="$deps" -DskipTests >/dev/null 2>&1 ); then
    echo "WARNING: mvn could not resolve the test classpath for $mod ($art) — skipped" >&2
    failed=$((failed+1)); rm -f "$deps"; continue
  fi
  cp="$(cat "$deps")"; rm -f "$deps"
  if [ -n "$LAUNCHER" ]; then lj="$LAUNCHER"; else lj="$(launcher_for "$cp")"; fi
  if [ -z "$lj" ] || [ ! -f "$lj" ]; then
    echo "WARNING: no junit-platform-launcher jar for $mod ($art) — skipped" >&2
    failed=$((failed+1)); continue
  fi
  {
    printf -- '-cp\n'
    printf '%s:%s:%s:%s:%s\n' "$SELF_DIR" \
      "$NETTY_SRC/$mod/target/classes" "$NETTY_SRC/$mod/target/test-classes" "$cp" "$lj"
    printf -- '-Duser.timezone=UTC\n'
    printf -- '-Djunit.jupiter.execution.timeout.default=120s\n'
    # What netty's pom.xml hands surefire from ${project.groupId}/${project.artifactId};
    # without these ChannelHandlerMetadataUtil builds a `null/null` resource path.
    printf -- '-DnativeImage.handlerMetadataGroupId=%s\n' "$grp"
    printf -- '-Dnativeimage.handlerMetadataArtifactId=%s\n' "$art"
  } > "$out"
  made=$((made+1))
done < "$TABLE"

if [ "$failed" -eq 0 ]; then
  echo "gen-module-args: wrote $made argfile(s) to $OUTDIR"
else
  echo "gen-module-args: wrote $made argfile(s) to $OUTDIR ($failed skipped)"
fi
[ "$failed" -eq 0 ]
