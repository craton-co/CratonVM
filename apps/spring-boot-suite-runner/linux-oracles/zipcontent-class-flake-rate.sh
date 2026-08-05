#!/bin/bash
# nestedZip64CanBeRead passes 10/10 in ISOLATION on both arms, yet failed once
# in three when the whole class ran. So the failure needs the other 28 tests to
# have run first - cross-test interference, not a per-test property. This
# measures the class-level rate on each arm to settle the page's "JIT-only"
# claim with a number instead of a single observation.
exec > /tmp/classflake-results.txt 2>&1
echo "host: $(cat /proc/loadavg)"
for arm in "jit:" "nojit:--nojit"; do
  name="${arm%%:*}"; extra="${arm#*:}"
  echo "--- NEW/$name (6 reps, full class) ---"
  for i in $(seq 1 6); do
    out=$(CRATONVM_BIN=/tmp/cratonvm-zj2 CRATONVM_EXTRA_ARGS="$extra" /tmp/ziporacle.sh ZipContentTests 2>&1)
    v=$(echo "$out" | tail -1)
    why=$(grep -oE "Zip64 .{0,50}|OutOfMemoryError.{0,30}" /tmp/oracle.out 2>/dev/null | head -1)
    m=$(grep -oE "methodName = '[a-zA-Z0-9]+'" /tmp/oracle.out 2>/dev/null | head -1)
    echo "[NEW/$name rep$i] $v ${m:+$m} ${why:+<$why>}"
  done
done
echo; echo "=== DONE ==="; date -u
