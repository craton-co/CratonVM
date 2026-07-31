#!/usr/bin/env bash
# Toggle the WFLYCTL0079 canary patch jar into/out of the transactions module.
# Usage: canary_toggle.sh on|off
set -eu
WF=/data/data/wildfly-dist-keep/wildfly-32.0.1.Final
M="$WF/modules/system/layers/base/org/jboss/as/transactions/main"
PATCHJAR="$M/cvm-dupattr-canary.jar"
BACKUP="$M/module.xml.orig"

[ -f "$BACKUP" ] || cp "$M/module.xml" "$BACKUP"

case "${1:-}" in
  on)
    (cd /data/tmp/wfly0079/patch/classes && jar cf "$PATCHJAR" .)
    python3 - "$M/module.xml" "$BACKUP" <<'PY'
import sys
out, orig = sys.argv[1], sys.argv[2]
text = open(orig).read()
anchor = '    <resources>\n'
assert text.count(anchor) == 1
text = text.replace(anchor, anchor + '        <resource-root path="cvm-dupattr-canary.jar"/>\n')
open(out, 'w').write(text)
PY
    echo "canary ON"
    grep -n 'resource-root' "$M/module.xml"
    ;;
  off)
    cp "$BACKUP" "$M/module.xml"
    rm -f "$PATCHJAR"
    echo "canary OFF"
    grep -n 'resource-root' "$M/module.xml"
    ;;
  *)
    echo "usage: $0 on|off" >&2
    exit 2
    ;;
esac
