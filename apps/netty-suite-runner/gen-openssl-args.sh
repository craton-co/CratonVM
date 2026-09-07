#!/usr/bin/env bash
# =============================================================================
# gen-openssl-args.sh — derive an @argfile whose classpath can actually reach
# netty's OPENSSL code paths, from this fixture's own common.args.
#
# WHY THIS EXISTS
#
# `OpenSsl.isAvailable()` is false with the classpath `common.args` carries, and
# the way that presents is NOT an error. JUnit simply never generates the
# OPENSSL parameterisations, so the classes that exist to test them read as
# clean PASSES while running a fraction of their tests:
# `ParameterizedSslHandlerTest` reports success at 7 of 63.
#
# Making it true has two halves and only the first is obvious:
#
#   1. `netty-tcnative-boringssl-static-<ver>-<os>.jar` on the classpath. It
#      links BoringSSL statically, so the host's own OpenSSL version stops
#      mattering — the DYNAMIC `netty-tcnative-<ver>-<os>.jar` that the netty
#      Maven reactor resolves needs `OPENSSL_3.2.0` (`objdump -p` names the
#      version tag) and this host ships 3.0.13.
#
#   2. the dynamic `netty-tcnative-<ver>-<os>.jar` REMOVED. With both present
#      netty finds the dynamic one and `isAvailable()` stays false whatever
#      their order. This half is load-bearing and was learned by measurement.
#
# The CLASSES jar (`netty-tcnative-classes-<ver>.jar`) is the Java half both
# variants share and stays.
#
# `--bc18` additionally drops the three `*-jdk15on-1.70` jars. `common.args`
# lists them AHEAD of `bcprov-jdk18on-1.84`, so `bctls-jdk18on-1.84` resolves
# `NISTObjectIdentifiers` from 1.70 and dies in `TlsUtils.<clinit>` on the
# missing `id_ml_dsa_44` — identically on HotSpot and CratonVM, before any
# BouncyCastle JSSE code runs.
#
# Output is host-specific absolute paths, exactly like common.args, so it goes
# to a path you name rather than into the tree. This generator is tracked;
# its output is not.
#
# USAGE:
#   ./gen-openssl-args.sh [--bc18] [-o OUT]     (default OUT: /tmp/ossl.args)
#
# Then, always, before trusting any number out of an OPENSSL class:
#   java @OUT OpenSslAvailabilityProbe        # expect: OpenSsl.isAvailable = true
# =============================================================================
set -uo pipefail

SELF_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
COMMON="${NETTY_COMMON_ARGS:-$SELF_DIR/common.args}"
M2="${MAVEN_REPO_LOCAL:-/data/toolchain/m2repo}"
OUT=/tmp/ossl.args
DROP_JDK15ON=0

while [ $# -gt 0 ]; do
  case "$1" in
    --bc18) DROP_JDK15ON=1; shift ;;
    -o) OUT="${2:?-o needs a path}"; shift 2 ;;
    -h|--help) sed -n '2,44p' "$0"; exit 0 ;;
    *) echo "ERROR: unknown argument: $1" >&2; exit 1 ;;
  esac
done

[ -f "$COMMON" ] || { echo "ERROR: common.args not found: $COMMON" >&2; exit 1; }

# The version to pair with is whatever the fixture's own classpath already
# names, so this never drifts from the reactor. Read it off the classes jar,
# which is present in every variant.
VER="$(sed -n 2p "$COMMON" | tr ':' '\n' \
       | sed -n 's|.*/netty-tcnative-classes-\(.*\)\.jar$|\1|p' | head -1)"
[ -n "$VER" ] || { echo "ERROR: no netty-tcnative-classes-<ver>.jar on common.args' -cp" >&2; exit 1; }

OS="linux-x86_64"
BSSL="$M2/io/netty/netty-tcnative-boringssl-static/$VER/netty-tcnative-boringssl-static-$VER-$OS.jar"
BSSLC="$M2/io/netty/netty-tcnative-boringssl-static/$VER/netty-tcnative-boringssl-static-$VER.jar"

for j in "$BSSL" "$BSSLC"; do
  [ -f "$j" ] || {
    echo "ERROR: missing $j" >&2
    echo "       mvn -q dependency:get -Dartifact=io.netty:netty-tcnative-boringssl-static:$VER:jar:$OS" >&2
    exit 1
  }
done

# A stub is worse than a miss: `netty-tcnative-boringssl-static-2.0.78` in this
# local repo had an EMPTY META-INF/native/, which loads and then fails to find
# a library, so `isAvailable()` is false for a reason that looks like the
# version problem this script exists to route around.
if ! unzip -l "$BSSL" 2>/dev/null | grep -q 'META-INF/native/.*\.so'; then
  echo "ERROR: $BSSL carries no META-INF/native/*.so — it is a stub, not the native artifact" >&2
  exit 1
fi

awk -v add="$BSSL:$BSSLC" -v drop15="$DROP_JDK15ON" -v ver="$VER" '
  NR == 2 {
    n = split($0, a, ":");
    s = add;
    dyn = "/netty-tcnative/" ver "/";
    for (i = 1; i <= n; i++) {
      if (index(a[i], dyn) > 0) continue;                       # the dynamic artifact
      if (drop15 == 1 && a[i] ~ /jdk15on/) continue;            # bcprov/bcpkix/bcutil 1.70
      s = s ":" a[i];
    }
    print s;
    next;
  }
  { print }
' "$COMMON" > "$OUT"

echo "wrote $OUT"
sed -n 2p "$OUT" | tr ':' '\n' | grep -E 'tcnative|bcprov|bcpkix|bcutil|bctls' \
  | sed 's|.*/||' | sed 's/^/  cp: /'
