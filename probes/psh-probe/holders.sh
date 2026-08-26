#!/bin/bash
# holders.sh <dir> — who is HOLDING the stale references, per arm.
#
# `grep -c was_vacated` is the wrong metric for a fix aimed at one holder: it
# counts every stale-reference reporter in the process, from several unrelated
# sites, and each reporter caps itself at 12. Three PRE runs and three POST
# runs differed on it while naming completely different holders, which is noise
# dressed as signal.
#
# What a fix at one site moves is the count of guard events whose BACKTRACE
# names that site. So: strip the reporter's own frames (the guard internals and
# the `check_vacated_push` / `note_dead_base_deref` entry points), and count
# what is left, per arm.
set -u
D="$1"
REPORTER='gc/src/gc_quiescence.rs|gc/src/vm_heap.rs:11[0-9][0-9]|gc/src/vm_heap.rs:1153|vm/src/runtime/value_stack.rs:(40[0-9]|43[0-9]|45[0-9]|48[0-9])'
for arm in PRE POST; do
  n=0; ev=0; props=0
  : > "/tmp/holders-$arm.txt"
  for f in "$D"/$arm-*.log; do
    [ -f "$f" ] || continue
    n=$((n+1))
    ev=$(( ev + $(grep -ac "was_vacated" "$f") ))
    props=$(( props + $(grep -a -A 10 "note_dead_base_deref\|report_vacated" "$f" \
              | grep -ac "properties_sidetable.rs") ))
    grep -a -A 10 "note_dead_base_deref\|report_vacated" "$f" \
      | grep -a "at /data/cvm-nres-20260824" \
      | sed 's#.*/data/cvm-nres-20260824/##' \
      | grep -avE "$REPORTER" >> "/tmp/holders-$arm.txt"
  done
  echo "== $arm: runs=$n  was_vacated_lines=$ev  properties_sidetable_frames=$props"
  echo "   top holders (reporter frames stripped):"
  sort "/tmp/holders-$arm.txt" | uniq -c | sort -rn | head -10 | sed 's/^/     /'
done
echo "== NoSuchMethodError java/lang/Object, per arm =="
for arm in PRE POST; do
  c=0
  for f in "$D"/$arm-*.log; do
    [ -f "$f" ] || continue
    c=$(( c + $(grep -ac 'NoSuchMethodError.*java/lang/Object' "$f") ))
  done
  echo "   $arm: $c"
done
