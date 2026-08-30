#!/usr/bin/env bash
# Do the REAL corpora reach the 26 registrations this lane nominated?
#
# The probes and the regression suite both said no. Neither is the corpus, and
# the record says so. `/data/dod-out/cmd-<w>-strict.txt` holds complete,
# already-validated `--jdk-only` command lines for the DoD workloads; reuse them
# verbatim except for three substitutions:
#
#   the binary          -> this lane's frozen /data/vm-l4io
#   the report path     -> our own, so L7's files are untouched
#   + --dump-native-registry
#
# Everything else -- classpath, workload class, --Xmx 2g (which needs its size
# as a SEPARATE arg or no report is written) -- is left exactly as the lane that
# validated it wrote it.
set +e
OUT=/data/l4corp
rm -rf "$OUT"; mkdir -p "$OUT"

for W in h2jdbc sbsimple tcssl jdbc; do
  SRC=/data/dod-out/cmd-$W-strict.txt
  [ -f "$SRC" ] || { echo "$W: no command file"; continue; }
  CMD=$(sed 's/^DODCMD //' "$SRC")
  CMD=${CMD//\/data\/l7dod-target\/release\/cratonvm//data/vm-l4io}
  CMD=$(printf '%s' "$CMD" | sed "s#--jdk-only-report /data/dod-out/rep-$W-strict.json#--jdk-only-report $OUT/rep-$W.json --dump-native-registry $OUT/$W.json#")
  case "$CMD" in
    *--dump-native-registry*) ;;
    *) echo "$W: SUBSTITUTION FAILED — refusing to run a command I did not rewrite"; continue ;;
  esac
  echo "=== $W"
  timeout 1800 bash -c "$CMD" > "$OUT/$W.out" 2> "$OUT/$W.err"
  rc=$?
  echo "  rc=$rc dump=$([ -s "$OUT/$W.json" ] && echo yes || echo NO) stdout=$(wc -l < "$OUT/$W.out") lines"
  tail -3 "$OUT/$W.out" | sed 's/^/    /'
done
echo CORPUS-DONE
