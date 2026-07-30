#!/usr/bin/env bash
# hs.sh <fqcn> — run one suite class on HotSpot, from its module directory.
set -u
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
MOD=$(awk -F'\t' -v c="$1" '$2==c{print $1; exit}' "$HERE/meta/all-classes.tsv")
[ -n "$MOD" ] || { echo "not in index: $1" >&2; exit 1; }
CP="$HERE:$MOD/build/classes/java/main:$MOD/build/classes/kotlin/main:$MOD/build/resources/main:$(tr -d '\r' < "$MOD/build/cratonvm-testcp.txt")"
cd "$MOD" && exec timeout "${HS_TO:-300}" /home/victor/jdk25/bin/java -cp "$CP" KRun "$1"
