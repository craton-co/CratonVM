#!/usr/bin/env bash
# Block until this machine is quiet enough to trust an ABSOLUTE number.
#
# This box is shared between concurrent sessions, and a neighbouring
# `cargo build` is not a small perturbation: the same paired chunked-overlap
# measurement reads 1.452x on a quiet box and anywhere from 0.81x to 1.75x
# round to round while twenty-two `rustc` processes are running. Ratios
# taken inside one round survive that; a fitted fixed cost or a per-pixel
# slope does not.
#
# Source it, or run it before a measurement:
#
#   bash bench-gpu/wait-for-quiet.sh && bash bench-gpu/run-...
#
# MAX_BUILDS is the number of `rustc`/`cargo`/`link.exe` processes tolerated
# (0 by default), SETTLE the number of consecutive quiet samples required,
# and TIMEOUT the point at which it gives up and says so on stderr rather
# than blocking a script forever.
set -u
MAX_BUILDS="${MAX_BUILDS:-2}"
SETTLE="${SETTLE:-3}"
INTERVAL="${INTERVAL:-20}"
TIMEOUT="${TIMEOUT:-1800}"

# Count the processes that actually burn a core: `rustc` and the linker,
# by distinct WINDOWS pid. `ps -W` lists an MSYS and a Windows view of the
# same process, and a `cargo` driver waiting on its children is not load --
# counting either of those makes any threshold below "several" unreachable
# on a machine that always has one build somewhere.
busy() {
  ps -W 2>/dev/null     | grep -E 'rustc\.exe|link\.exe|cl\.exe'     | awk '{ print $4 }' | sort -u | grep -c . || echo 0
}

waited=0
quiet=0
while [ "$quiet" -lt "$SETTLE" ]; do
  n=$(busy)
  if [ "$n" -le "$MAX_BUILDS" ]; then
    quiet=$((quiet + 1))
  else
    if [ "$quiet" -gt 0 ] || [ $((waited % 120)) -eq 0 ]; then
      echo "wait-for-quiet: $n build process(es) running, waited ${waited}s" >&2
    fi
    quiet=0
  fi
  [ "$quiet" -ge "$SETTLE" ] && break
  if [ "$waited" -ge "$TIMEOUT" ]; then
    echo "wait-for-quiet: gave up after ${TIMEOUT}s with $n build process(es) running." >&2
    echo "wait-for-quiet: the numbers that follow are NOT quiet-host numbers." >&2
    exit 1
  fi
  sleep "$INTERVAL"
  waited=$((waited + INTERVAL))
done
echo "wait-for-quiet: quiet for $((SETTLE * INTERVAL))s after ${waited}s of waiting." >&2
