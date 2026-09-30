#!/usr/bin/env bash
# Audit a hand-written bc-java sweep classpath for entries that EXIST on disk
# and are missing from it. Prints one line per omission and exits non-zero.
#
# WHY A CHECKER AND NOT A GENERATOR. Twice a missing entry in the 53-class
# sweep's classpath file has been read as a VM verdict, and neither time did it
# look like a classpath problem:
#
#   * `unboundid-ldapsdk` missing made `jce.provider.test` die in `<clinit>` on
#     HOTSPOT too, so a real CratonVM defect read as "fails on HotSpot as well"
#     and was filed as not-our-bug;
#   * `pkix/build/resources/main` missing made five `pkix.test` cases fail on
#     both VMs with `MissingEntryException: Can't find entry
#     CertPathReviewer.noValidCrlFound.text` -- five invented divergences,
#     closed by one entry.
#
# Both produced plausible TEST failures, not classpath errors, which is why a
# results table cannot show them. Regenerating the file instead would silently
# change what the sweep measures (jar version choice, whether a module's test
# tree is on the path), so this only ever REPORTS -- the fixture stays
# hand-owned and comparable across the sweeps that already read it.
#
# Usage:
#   test-infra/bcjava-classpath-audit.sh [CLASSPATH_FILE] [BC_ROOT]
#
# Defaults: /data/bcjca-classpath.txt and the bc-java checkout it refers to.
set -u

CPFILE="${1:-/data/bcjca-classpath.txt}"
BC_ROOT="${2:-${BCJAVA_ROOT:-}}"
if [ -z "$BC_ROOT" ]; then
  for c in /data/cratonvm/apps/bc-java C:/craton/cratonvm/apps/bc-java C:/craton/apps/bc-java; do
    [ -d "$c" ] && BC_ROOT="$c" && break
  done
fi
if [ ! -f "$CPFILE" ]; then
  echo "bcjava-classpath-audit: no such classpath file: $CPFILE" >&2
  exit 2
fi
if [ -z "$BC_ROOT" ] || [ ! -d "$BC_ROOT" ]; then
  echo "bcjava-classpath-audit: no bc-java checkout found (pass one as \$2)" >&2
  exit 2
fi

CP="$(cat "$CPFILE")"
missing=0

# Every module the file already names, taken FROM the file so this never
# invents a module the sweep does not use.
modules="$(printf '%s' "$CP" | tr ':' '\n' \
  | sed -n 's#^\([a-z][a-z0-9]*\)/build/classes/java/main$#\1#p' | sort -u)"

for m in $modules; do
  for d in "build/resources/main" "build/resources/test"; do
    # On disk but not on the path is the whole bug. The reverse (on the path,
    # absent on disk) is harmless -- a JVM ignores a classpath entry that does
    # not exist -- so it is not reported.
    if [ -d "$BC_ROOT/$m/$d" ]; then
      case ":$CP:" in
        *":$m/$d:"*) ;;
        *":$BC_ROOT/$m/$d:"*) ;;
        *)
          echo "MISSING resources  $m/$d  (exists: $(find "$BC_ROOT/$m/$d" -type f | wc -l) files)"
          missing=$((missing + 1))
          ;;
      esac
    fi
  done
done

# The optional-dependency jars this corpus keeps beside itself. A test that
# needs one and does not get it fails in `<clinit>`, naming a class that has
# nothing to do with the omission.
if [ -d "$BC_ROOT/libs" ]; then
  for j in "$BC_ROOT/libs"/*.jar; do
    [ -f "$j" ] || continue
    case "$j" in *-sources.jar|*-javadoc.jar) continue ;; esac
    base="$(basename "$j")"
    # A jar the corpus keeps only for the pre-Jakarta namespace is SUPERSEDED,
    # not missing: `javax.mail` and `activation` are the EE8 spellings of the
    # `jakarta.mail` / `jakarta.activation-api` jars already on the path, and
    # putting both on one classpath is how you get two copies of
    # `javax.activation.DataHandler` and a resolution that depends on order.
    # Report them, but do not count them as omissions.
    superseded=""
    case "$base" in
      javax.mail-*) superseded="jakarta.mail-" ;;
      activation-*) superseded="jakarta.activation-api-" ;;
    esac
    case ":$CP:" in
      *"$base"*) continue ;;
    esac
    if [ -n "$superseded" ]; then
      case ":$CP:" in
        *"$superseded"*)
          echo "superseded jar     $base (the classpath carries ${superseded}* instead)"
          continue
          ;;
      esac
    fi
    echo "MISSING jar        $base"
    missing=$((missing + 1))
  done
fi

if [ "$missing" -eq 0 ]; then
  echo "bcjava-classpath-audit: $CPFILE is complete for $BC_ROOT"
  exit 0
fi
echo "bcjava-classpath-audit: $missing omission(s) in $CPFILE"
echo "Each one shows up as a TEST failure, on both VMs, naming something else."
exit 1
