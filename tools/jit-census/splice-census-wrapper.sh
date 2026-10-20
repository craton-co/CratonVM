#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
# Copyright 2024-2026 Craton Software Company
#
# Count IR-tier splices without landing the log that counting them normally
# costs.
#
# `CRATONVM_DBG_IR_COMPILES=1` is the only way to see `spliced N callee bodies`,
# and it prints a line per compile decision. Under a suite runner that appends
# every fork's stderr to one `raw.log`, that is not a diagnostic, it is a disk
# hazard: on 2026-09-04 a single netty class produced **1.8 GB** and took the
# shared Azure volume from 75 GB free to 44 GB before the run was stopped. Forty
# classes would have filled it and broken every other session on the box.
#
# The fix is not to log less, it is not to KEEP the log. This wrapper stands in
# for the `--bin` a suite runner forks: it runs the VM with the flag on, pipes
# stderr through a counter, and writes one tally line per invocation. Nothing
# proportional to the compile count ever reaches disk.
#
#   CENSUS_OUT=/data/splices.tsv \
#   run-netty-suite-jdkonly.sh --bin tools/jit-census/splice-census-wrapper.sh ...
#
# `CENSUS_VM` is the real binary. Every argument is forwarded untouched, and the
# VM's own exit status is preserved, so the runner's pass/fail verdicts are
# unchanged — the wrapper must not turn a crash into a pass.
set -uo pipefail

VM="${CENSUS_VM:?set CENSUS_VM to the cratonvm binary}"
OUT="${CENSUS_OUT:-/dev/null}"

# The VM's stderr goes to the counter; its stdout is untouched, because the
# runner parses it. `exec 3>&1` keeps a handle on the real stdout so the pipe
# below cannot swallow it.
tally=$(mktemp)
{
  CRATONVM_DBG_IR_COMPILES=1 "$VM" "$@" 2>&1 1>&3 |
    awk '
      /spliced [0-9]+ callee bod/ { s++ }
      /inline-plan/               { p++ }
      /NO invoke_info/            { n++ }
      END { printf "%d\t%d\t%d\n", s+0, p+0, n+0 }
    ' > "$tally"
} 3>&1
rc=${PIPESTATUS[0]}

# One line per class, appended: splices, plans, methods that lost invoke_info.
# `flock` because the runner may fork several of these at once even at
# `--shards 1` (its own retry path re-invokes the binary).
if [ "$OUT" != "/dev/null" ]; then
  ( flock 9; printf '%s\t%s\n' "$(cat "$tally")" "${CENSUS_LABEL:-$*}" >> "$OUT" ) 9>>"$OUT"
fi
rm -f "$tally"
exit "$rc"
