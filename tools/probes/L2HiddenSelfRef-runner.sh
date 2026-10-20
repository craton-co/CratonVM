#!/usr/bin/env bash
# One row per PROCESS: a door that aborts the VM must not hide the others.
#   doorrun.sh <binary> <label> [extra-vm-args]
set -u
BIN="$1"; LABEL="$2"; shift 2
EXTRA="${*:-}"
J=/data/jdkimages/jdk25-linux/jdk-25.0.4+7
CP=/data/hidden/cls
OUT=/data/hidden/out-$LABEL; rm -rf "$OUT"; mkdir -p "$OUT"
ROWS=$("$J/bin/java" -cp "$CP" L2HiddenSelfRef __list__ 2>/dev/null | grep -a "^ROWS " | cut -d' ' -f2-)
if [ -z "$ROWS" ]; then echo "*** could not get ROW_NAMES from the probe"; echo DOORRUN_DONE; exit 1; fi
echo "== doorrun $LABEL $(date +%H:%M:%S) rows=$(echo $ROWS | wc -w) =="
ok=0; bad=0
for r in $ROWS; do
  d="$OUT/$r"; mkdir -p "$d"
  if [ "$LABEL" = oracle ]; then
    ( cd "$d" && timeout 120 "$J/bin/java" -cp "$CP" L2HiddenSelfRef "$r" > o.txt 2>e.txt )
  else
    ( cd "$d" && CRATONVM_DISABLE_DEFAULT_WATCHDOG=1 timeout 200 \
        "$BIN" --java-home "$J" $EXTRA -cp "$CP" L2HiddenSelfRef "$r" > o.txt 2>e.txt )
  fi
  rc=$?
  if grep -aq "^PASS" "$d/o.txt"; then
    printf "  %-38s PASS\n" "$r"; ok=$((ok+1))
  else
    bad=$((bad+1))
    why=$(grep -a "^FAIL row" "$d/o.txt" | head -1)
    [ -z "$why" ] && why=$(grep -aoE "(class file error|Error in thread \"main\")[^\"]*" "$d/e.txt" | head -1)
    [ -z "$why" ] && why="rc=$rc, no PASS and no diagnosis"
    printf "  %-38s FAIL  %s\n" "$r" "$why"
  fi
done
echo "  $LABEL: $ok pass / $bad fail"
echo DOORRUN_DONE
