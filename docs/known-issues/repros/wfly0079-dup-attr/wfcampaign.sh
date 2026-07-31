#!/usr/bin/env bash
# Drive N WildFly boots in a slot, each with the WFLYCTL0079 canary amplified.
# Usage: wfcampaign.sh <slot> <boots> <reps> <binary> <tag>
set -u
SLOT="$1"; BOOTS="$2"; REPS="$3"; BIN="$4"; TAG="$5"
ROOT=/data/tmp/wfly0079/campaign/$TAG
mkdir -p "$ROOT"
LOG="$ROOT/slot$SLOT.log"
: > "$LOG"
hits=0; ok=0; bad=0
for i in $(seq 1 "$BOOTS"); do
  OUT="$ROOT/slot$SLOT-run"
  rc=0
  /data/tmp/wfly0079/wfboot.sh "$SLOT" "$OUT" "$BIN" "$REPS" >/dev/null 2>&1 || rc=$?
  case $rc in
    0) ok=$((ok+1));;
    3) hits=$((hits+1))
       mkdir -p "$ROOT/hits"
       cp "$OUT/console.log" "$ROOT/hits/slot$SLOT-boot$i.log"
       echo "HIT boot=$i" >> "$LOG"
       grep -E 'CVM-DUPATTR.*FAIL|already registered|WFLYCTL0043' "$OUT/console.log" | head -5 >> "$LOG"
       ;;
    *) bad=$((bad+1)); echo "INCOMPLETE boot=$i" >> "$LOG";;
  esac
  echo "boot=$i rc=$rc ok=$ok hits=$hits incomplete=$bad" >> "$LOG"
done
echo "SLOT$SLOT DONE boots=$BOOTS ok=$ok hits=$hits incomplete=$bad" >> "$LOG"
