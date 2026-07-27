#!/usr/bin/env bash
# one.sh <fqcn> [extra cratonvm args...] — run a single suite class directly,
# from the owning module's directory, with this worktree's binary.
set -u
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
SPRING="${SPRING:-/data/data/wt-springsuite8b-20260726/apps/spring-framework}"
JDK="${JDK25:-/home/victor/jdk25}"
BIN="${CRATONVM_BIN:-/data/data/wt-sprbuglist-20260727/localbin/cratonvm-sprbuglist-20260727.bin}"
CLS="$1"; shift
MOD=$(awk -F'\t' -v c="$CLS" '$2==c{print $1; exit}' "$HERE/meta/all-classes.tsv")
[ -n "$MOD" ] || { echo "class not in index: $CLS" >&2; exit 1; }
CP="$HERE:$(tr -d '\r' < "$MOD/build/cratonvm-testcp.txt")"
AF="$(mktemp /tmp/af-XXXXXX.txt)"; { echo "-cp"; echo "$CP"; } > "$AF"
export CRATONVM_DEFAULT_HEAP_MAX_MB="${CRATONVM_DEFAULT_HEAP_MAX_MB:-2048}"
echo "[one] mod=$MOD  bin=$BIN" >&2
cd "$MOD" && exec "$BIN" --java-home "$JDK" "$@" "@$AF" KRun "$CLS"
