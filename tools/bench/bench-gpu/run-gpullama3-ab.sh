#!/usr/bin/env bash
# Interleaved A/B of two cratonvm GPU binaries on GPULlama3: one round is
# one run of each arm, arm order alternated per round.
#
# Never compare two GPU binaries in separate blocks on this host. The same
# binary measures 11.9 tok/s in one window and 22.2 in another an hour
# later, which is a larger difference than any change anyone has landed on
# this path -- and a 2026-08-29 report of a "42% regression" was exactly
# that, two blocks an hour apart read as two binaries. Alternating the arms
# inside a round is what makes the pair meaningful; the absolute numbers in
# any one round are not comparable to another day's.
#
# See `run-gpullama3-submit-ab.sh` for the same comparison on the host
# dispatch cost alone, with a per-round CPU control.
set -u
A="${A:?arm A binary}"
B="${B:?arm B binary}"
ROUNDS="${ROUNDS:-4}"
N="${N:-32}"
PROMPT="${PROMPT:-Why is the sky blue?}"
# The GPULlama3 checkout is not in this repository (`apps/` holds
# external app trees and is git-ignored). Override APP when running
# from a worktree that does not have one beside it.
APP="${APP:-$(dirname "${BASH_SOURCE[0]}")/../apps/GPULlama3.java}"
[ -f "$APP/run-craton-gpu.sh" ] || {
  echo "no GPULlama3 checkout at $APP -- set APP=<path to GPULlama3.java>" >&2
  exit 2
}
cd "$APP" || exit 1

tok() {
  bash run-craton-gpu.sh "$1" -p "$PROMPT" -n "$N" 2>&1 \
    | sed -n 's/.*achieved tok\/s: \([0-9,\.]*\).*/\1/p' | tr ',' '.'
}

printf '%-6s %10s %10s   %s\n' round A B order
for r in $(seq 1 "$ROUNDS"); do
  if [ $((r % 2)) -eq 1 ]; then
    a=$(tok "$A"); b=$(tok "$B"); order="A-then-B"
  else
    b=$(tok "$B"); a=$(tok "$A"); order="B-then-A"
  fi
  printf '%-6s %10s %10s   %s\n' "$r" "${a:-FAIL}" "${b:-FAIL}" "$order"
done
