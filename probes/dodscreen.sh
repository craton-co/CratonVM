#!/usr/bin/env bash
# dodscreen.sh <MainClass> [more classes...] -- the roadmap's definition-of-done
# screen, run against whatever workload is available.
#
# "no fabricated class instantiated, WHATEVER ITS PACKAGE -- screened against
#  the refused-class set the VM reports, not against a prefix."
#
# Six of the nine fabricated classes do NOT match `cratonvm/internal/`, so this
# screens on the report's own rows and never on a name prefix.
#
# Three instrument traps this encodes, each recorded in the roadmap's §5:
#   * a dump flag placed AFTER the main class is silently ignored -- no file, no
#     warning, exit 0. All flags go before -cp.
#   * the report path must be WINDOWS-SHAPED on this host. Given `/c/Users/...`
#     the VM prints `os error 3`, continues, and the file never appears.
#   * the report is NOT written when the program calls System.exit.
set +e
export MSYS2_ARG_CONV_EXCL='*'
export MSYS_NO_PATHCONV=1
W=C:/craton/cratonvm/.claude/worktrees/h2-known-issues-206dee
SP=C:/Users/Victor/AppData/Local/Temp/claude/C--craton-cratonvm--claude-worktrees-h2-known-issues-206dee/1922dc23-b218-45e1-9b5a-362ec8568525/scratchpad
JDK="C:/Program Files/Microsoft/jdk-25.0.3.9-hotspot"
CV="$W/target/release/cratonvm.exe"
export CRATONVM_DISABLE_DEFAULT_WATCHDOG=1

for C in "$@"; do
  REP="$SP/rep-$C.json"          # Windows-shaped: $SP is already C:/...
  rm -f "$REP"
  timeout 600 "$CV" --java-home "$JDK" --jdk-only --explain-jdk-only \
      --jdk-only-report "$REP" -cp "$W/probes/out" "$C" > "$SP/dod-$C.out" 2>"$SP/dod-$C.err"
  rc=$?
  if [ ! -f "$REP" ]; then
    echo "$C: NO REPORT WRITTEN (rc=$rc) -- flag order, path shape, or System.exit"
    grep -a "os error 3" "$SP/dod-$C.err" | head -2
    continue
  fi
  echo "=== $C  rc=$rc  report $(wc -c < "$REP") bytes ==="
  python - "$REP" <<'PY'
import json,sys,collections
d=json.load(open(sys.argv[1],encoding='utf-8'))
v=d.get('violations') or []
print("  counts:", json.dumps(d.get('counts',{}), sort_keys=True))
print("  mode:", d.get('mode'), " jdk_feature:", d.get('jdk_feature'),
      " schema:", d.get('schema_version'))
by=collections.Counter(x.get('kind','?') for x in v)
for k,n in sorted(by.items()): print(f"  {k:34s} {n}")
# The DoD screen: every class the report says was REQUESTED as a fabrication,
# with no prefix filter of any kind.
fab=collections.Counter()
for x in v:
    if x.get('kind')=='compatibility-class-requested':
        fab[x.get('class','?')]+=1
print(f"  -- fabrication requests: {len(fab)} distinct classes, {sum(fab.values())} rows")
for c,n in sorted(fab.items(), key=lambda t:(-t[1],t[0])):
    print(f"     {n:5d}  {c}")
PY
done
