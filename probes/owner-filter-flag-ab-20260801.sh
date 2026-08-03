#!/usr/bin/env bash
# ONE-BINARY A/B for the owner class filter on DefaultCatalogAndSchemaTest.
#
# Both arms come from the SAME build; only `CRATONVM_NO_OWNER_CLASS_FILTER`
# differs. That removes the two-binary confound that produced a false
# attribution earlier in this investigation, and it is why the filter was made
# a runtime opt-out rather than a compile-time change.
#
#   OFF arm = filter disabled  (equivalent to defect-4-only behaviour)
#   ON  arm = filter enabled   (defect 5)
#
# Arms alternate so both see the same background load. Each run is 35-75 min;
# do NOT run a build alongside this.
#
#   CV=<exe>   binary under test (required)
#   N=<count>  iterations per arm (default 2)
set -uo pipefail
export MSYS2_ARG_CONV_EXCL='*'
export MSYS_NO_PATHCONV=1

HERE="$(cd "$(dirname "$0")/.." && pwd)"
CV="${CV:?set CV to the cratonvm.exe under test}"
N="${N:-2}"
runs_dir="C:/craton/CratonVM/apps/hib-suite-runner/runs"

one() { # $1=label $2=iter $3=extra env (empty or the opt-out)
  local tag="$1-$2" log
  if [ -n "$3" ]; then
    env "$3" CV="$CV" TAG="$tag" bash "$HERE/probes/hib-mapresize-repro-20260731.sh" >/dev/null 2>&1
  else
    CV="$CV" TAG="$tag" bash "$HERE/probes/hib-mapresize-repro-20260731.sh" >/dev/null 2>&1
  fi
  log="$runs_dir/mapstale-$tag.log"
  printf '  %-10s %-8s %-46s stale=%s drops=%s\n' \
    "$tag" \
    "$(grep -o 'rc=[0-9]*' "$log" 2>/dev/null | tail -1)" \
    "$(grep -m1 -o 'found=[0-9]* started=[0-9]* ok=[0-9]* failed=[0-9]*' "$log" 2>/dev/null || echo '<crashed, no @@RESULT>')" \
    "$(grep -c 'Stale pointer detected' "$log" 2>/dev/null)" \
    "$(grep -c 'owner-filter' "$log" 2>/dev/null)"
}

echo "=== owner-filter one-binary A/B  N=$N ==="
echo "CV=$CV"
echo "HotSpot control: found=132 started=132 ok=132 failed=0"
for i in $(seq 1 "$N"); do
  one off "$i" "CRATONVM_NO_OWNER_CLASS_FILTER=1"
  one on  "$i" "CRATONVM_DBG_OWNER_FILTER=1"
done
