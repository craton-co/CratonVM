set -euo pipefail
ROOT=/data/data/cratonvm-worktrees/20260708-191002-es-nonpassed-rerun
ES="$ROOT/apps/elasticsearch"
CP=$(tr -d '\r' < "$ES/libs/core/build/craton-testcp.txt" | paste -sd: -)
python3 - <<'PY' "$CP"
import sys, zipfile, os
for p in sys.argv[1].split(':'):
    if not p: continue
    if os.path.isdir(p):
        q=os.path.join(p,'org/elasticsearch/index/IndexVersions.csv')
        if os.path.exists(q): print('DIR',q)
    elif zipfile.is_zipfile(p):
        with zipfile.ZipFile(p) as z:
            if 'org/elasticsearch/index/IndexVersions.csv' in z.namelist():
                data=z.read('org/elasticsearch/index/IndexVersions.csv')
                print('JAR',p,'size',len(data),'head',data[:80])
PY