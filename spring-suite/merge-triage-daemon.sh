#!/usr/bin/env bash
# Merge the 4 shard result files into results-cv/results.tsv, then run the
# resumable HotSpot triage. Loops on a ~2-min cadence until all 4 shard workers
# have finished (each writes DONE marker to its run.console.log), then does a
# final merge+triage and exits.
set -u
cd /c/craton/CratonVM-spring/spring-suite
H=/c/craton/CratonVM-spring/spring-suite
RES="results-cv/results.tsv"

merge() {
  cat results-cv/s*/results.tsv         > "$RES" 2>/dev/null
  cat results-cv/s*/failcauses.log      > results-cv/failcauses.log 2>/dev/null
  cat results-cv/s*/crashes.log         > results-cv/crashes.log 2>/dev/null
  # aggregate summary
  awk -F'\t' '{c[$2]++; f+=$3; s+=$4; x+=$5} END{
    for (k in c) printf "  %-9s %d\n",k,c[k]; print "test-level: found="f" succeeded="s" failed="x}' "$RES" > results-cv/summary.txt
}

done_count() { grep -l "CratonVM AGGREGATE" results-cv/s*/run.console.log 2>/dev/null | wc -l; }

while :; do
  merge
  CVOUT="$H/results-cv" HS_TO=70 bash triage-spring.sh >> triage/triage.console.log 2>&1
  nuniq=$(awk -F'\t' '$4=="CV-UNIQUE"' triage/triage.tsv 2>/dev/null | wc -l)
  echo "$(date +%H:%M:%S) results=$(wc -l < "$RES" 2>/dev/null) cv-unique=$nuniq shards_done=$(done_count)/4" >> triage/daemon.log
  if [ "$(done_count)" -ge 4 ]; then
    merge
    CVOUT="$H/results-cv" HS_TO=70 bash triage-spring.sh >> triage/triage.console.log 2>&1
    echo "$(date +%H:%M:%S) ALL SHARDS DONE — final merge+triage complete" >> triage/daemon.log
    break
  fi
  for s in $(seq 1 24); do sleep 5; done
done
