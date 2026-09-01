#!/bin/bash
# usage: ./rsh.sh 'command'   [timeout_seconds]
set +H
CMD="$1"
TMO="${2:-120}"
MARK="__DONE_$$_$RANDOM__"
START=$(wc -c < g1oop.log)
{ printf '%s\n' "$CMD"; printf 'echo %s rc=$?\n' "$MARK"; } > g1oop.fifo
T=0
while [ $T -lt $TMO ]; do
  if tail -c +$((START+1)) g1oop.log | grep -q "$MARK rc="; then break; fi
  sleep 2; T=$((T+2))
done
tail -c +$((START+1)) g1oop.log | grep -v "^echo $MARK"
