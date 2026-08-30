#!/usr/bin/env bash
# Re-run the two DoD workloads with the CURRENT binary, keeping stderr.
#
# The saved .err files from the earlier run show 0 UNCLASSIFIED warns, but that
# run predates the instrument -- a zero from a build that could not emit the
# line is not a measurement. These are real Spring Boot and real Tomcat+SSL,
# which is the population the regression corpus cannot stand in for.
set +e
ulimit -c 0
source /data/toolchain/env.sh
OUT=/data/l1u-dod5
rm -rf "$OUT"; mkdir -p "$OUT"
export CRATONVM_DISABLE_DEFAULT_WATCHDOG=1

for name in sbsimple tcssl; do
  cmd=$(sed 's/^DODCMD //' "/data/l1u-dod4/cmd-$name-strict.txt")
  # Point the report at this run so the old one is not overwritten.
  cmd=${cmd//\/data\/l1u-dod4\//$OUT\/}
  ( cd "$OUT" && timeout 420 bash -c "$cmd" > "$OUT/$name.out" 2> "$OUT/$name.err" )
  echo "$name rc=$?"
  echo "  UNCLASSIFIED warns: $(sed 's/\x1b\[[0-9;]*m//g' "$OUT/$name.err" | grep -c UNCLASSIFIED-NULL-BASE)"
  echo "  post-clinit fixup lines (proof this build's warns reach the file): $(sed 's/\x1b\[[0-9;]*m//g' "$OUT/$name.err" | grep -c 'Post-clinit fixup')"
  echo "  last stdout line: $(tail -1 "$OUT/$name.out")"
done
echo DOD-DONE
