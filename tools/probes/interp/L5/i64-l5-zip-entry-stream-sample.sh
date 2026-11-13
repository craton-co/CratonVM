#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
# Copyright 2024-2026 Craton Software Company
#
# i64-L5 closure (2026-11-02): the sampling procedure for the proposal
# "serve ZipFile entry streams from the VM archive". Runs ONE application start
# under the VM twice (the census, the sampler) and prints the upper bound of the
# share of that start an intrinsic on `InflaterInputStream.read` of a ZipFile
# entry could win, in the form the page's decision rule reads.
#
# usage: i64-l5-zip-entry-stream-sample.sh <cratonvm> <mode> <ceiling-ns> -- <vm args...>
#   <cratonvm>    the launcher binary
#   <mode>        --real-jdk | --compatible
#   <ceiling-ns>  per-stream ceiling from I64L5ZipEntryStreamCeilingBench in the SAME
#                 mode (its `TIME <entry> <consumer> ceiling ns/stream` row for the
#                 entry shape closest to the start's streams: props/all for a
#                 .properties / services read, tld/lines for a TLD scan)
#   <vm args>     what follows the mode: e.g. -jar app.jar, or -cp out Main. The
#                 application must EXIT by itself once started (a Spring Boot sample
#                 with a self-closing context, a Tomcat fixture that stops after
#                 "Started"); a run killed from outside prints no census.
#
# Output (stdout): wall ms, the census lines, the sampler's rows for
# InflaterInputStream.read and Resource.getBytes, then
#   BOUND census-share=<pct>   streams-ended(kind=inflater-stream via=other) * ceiling / wall
#   BOUND sampler-share=<pct>  (inclusive InflaterInputStream.read - inclusive Resource.getBytes)
#                              as a percentage of the attributed samples
# and a verdict line: BUILD if either bound is >= 2.0, else CLOSE.
set -u
B=${1:?cratonvm binary}
MODE=${2:?--real-jdk or --compatible}
CEIL=${3:?per-stream ceiling ns from the bench}
shift 3
[ "${1:-}" = "--" ] && shift
OUT=${I64_OUT_DIR:-.}
census="$OUT/i64l5-census$MODE.err"
prof="$OUT/i64l5-profile$MODE.err"

t0=$(date +%s%N)
CRATONVM_DBG=zip-stream-census "$B" "$MODE" "$@" >/dev/null 2>"$census"
t1=$(date +%s%N)
wall_ms=$(( (t1 - t0) / 1000000 ))
echo "WALL(census run, census overhead included) ms=$wall_ms"
grep 'zip-stream-census' "$census"

t0=$(date +%s%N)
CRATONVM_PROFILE_SAMPLE_MS=5 "$B" "$MODE" "$@" >/dev/null 2>"$prof"
t1=$(date +%s%N)
plain_ms=$(( (t1 - t0) / 1000000 ))
echo "WALL(sampler run) ms=$plain_ms"
grep -E '^\[profile\] execution samples' "$prof"
grep -E '^\[profile-inc\].*(InflaterInputStream\.read|Resource\.getBytes|ZipFile\$ZipFileInflaterInputStream)' "$prof"

# streams-ended of the plain-jar / custom-InflaterInputStream kind read OUTSIDE getBytes.
ended=$(grep -E 'kind=inflater-stream via=other' "$census" | sed -n 's/.*streams-ended=\([0-9]*\).*/\1/p' | head -1)
ended=${ended:-0}
# The wall of an UNINSTRUMENTED run is the denominator: time the sampler-armed run
# (its overhead is the sampler's, small); the census run pays a stack capture per inflate.
awk -v ended="$ended" -v ceil="$CEIL" -v wall="$plain_ms" 'BEGIN {
  share = (wall > 0) ? (ended * ceil / 1e6) * 100.0 / wall : 0;
  printf "BOUND census-share=%.3f  (streams-ended=%d x ceiling=%d ns / wall=%d ms)\n", share, ended, ceil, wall;
}'
inc_read=$(grep -E '^\[profile-inc\].*InflaterInputStream\.read$' "$prof" | head -1 | awk '{print $2}' | tr -d '%')
inc_get=$(grep -E '^\[profile-inc\].*Resource\.getBytes$' "$prof" | head -1 | awk '{print $2}' | tr -d '%')
inc_read=${inc_read:-0}
inc_get=${inc_get:-0}
awk -v r="$inc_read" -v g="$inc_get" 'BEGIN {
  d = r - g; if (d < 0) d = 0;
  printf "BOUND sampler-share=%.3f  (InflaterInputStream.read %s%% - Resource.getBytes %s%%; includes the consumer-side Java under read, so an over-estimate of what an intrinsic saves)\n", d, r, g;
}'
cs=$(awk -v ended="$ended" -v ceil="$CEIL" -v wall="$plain_ms" 'BEGIN { print (wall > 0 && (ended*ceil/1e6)*100.0/wall >= 2.0) ? 1 : 0 }')
ss=$(awk -v r="$inc_read" -v g="$inc_get" 'BEGIN { d = r - g; print (d >= 2.0) ? 1 : 0 }')
if [ "$cs" = 1 ] || [ "$ss" = 1 ]; then
  echo "VERDICT BUILD (a bound is >= 2.0 percent of the start): reopen the page"
else
  echo "VERDICT CLOSE (both bounds < 2.0 percent of the start)"
fi
