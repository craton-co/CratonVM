#!/bin/bash
# guarded-inline-reach.sh — how much of a workload can guarded inlining reach?
#
# PGO-02's last open question (`docs/feature-designs/profile-guided-inlining.md`
# §8 item 8). Guarded virtual/interface inlining lives in the SINGLE-PASS x64
# emitter. The optimizing (IR) tier serves a virtual site from a MIC/PIC
# cascade instead and plans no inline at all, so every method the optimizing
# tier accepts leaves this feature's reach entirely — and the `cov-*` lanes
# exist to make the optimizing tier accept more methods.
#
# That makes "is guarded inlining worth extending?" a question about POPULATION,
# not about the lowering. This answers it for any workload, in one run:
#
#   installed        compiled bodies that were actually installed
#   single_pass      of those, bodies the single-pass emitter produced — the
#                    only ones guarded inlining can ever apply to
#   optimizing       of those, bodies the optimizing tier produced — out of
#                    reach, permanently, unless the IR path grows its own
#                    guarded-inline lowering
#   with_candidates  single-pass bodies where the inliner was asked about at
#                    least one site (a measured `inline_candidates`)
#   speculative      single-pass bodies that actually got a guarded splice
#   refusals         why the rest were refused, by category — this is the
#                    actionable part: `cold-site` and `no-profile-evidence`
#                    mean "wait longer", `megamorphic` means "never",
#                    `callee-unresolved` means "the body was not inlineable",
#                    and `guard-not-emittable` means the feature was off.
#
# Every number is a COUNT, so a loaded host does not affect it, and no
# conclusion about speed can be drawn from it.
#
# Usage:
#   guarded-inline-reach.sh -Exe /abs/path/to/cratonvm [--label NAME] -- <vm args...>
#
#   guarded-inline-reach.sh -Exe "$PWD/target/release/cratonvm" --label bintrees -- \
#       -Xmx4g -cp /tmp/classes CratonBench bintrees
#
# The run sets TWO flags itself, and it takes both:
#
#   CRATONVM_JIT_GUARDED_VIRTUAL_INLINE=1  the lowering. Without it every
#                                          virtual site is refused
#                                          `guard-not-emittable`.
#   CRATONVM_TIER_PGO=1                    the EVIDENCE. Receiver-type
#                                          recording is opt-in
#                                          (`SharedVm::new` only calls
#                                          `enable_profiling(true)` under this
#                                          gate), so without it every virtual
#                                          site is refused
#                                          `no-profile-evidence` and the
#                                          feature reaches exactly nothing —
#                                          measured, not assumed.
#
# Pass --off to measure the default configuration instead.

set -uo pipefail

EXE=""
LABEL=""
TIMEOUT=900
KEEP=""
FLAG_ON=1

while [[ $# -gt 0 ]]; do
  case "$1" in
    -Exe) EXE="$2"; shift 2 ;;
    --label) LABEL="$2"; shift 2 ;;
    --timeout) TIMEOUT="$2"; shift 2 ;;
    --keep) KEEP="$2"; shift 2 ;;
    --off) FLAG_ON=0; shift ;;
    --) shift; break ;;
    *) echo "unknown option: $1" >&2; exit 2 ;;
  esac
done

if [[ -z "$EXE" ]]; then
  echo "guarded-inline-reach.sh: -Exe is required" >&2
  exit 2
fi
if [[ $# -eq 0 ]]; then
  echo "guarded-inline-reach.sh: no VM arguments after --" >&2
  exit 2
fi
[[ -n "$LABEL" ]] || LABEL="${!#}"

OUT="${KEEP:-$(mktemp)}"
rm -f "$OUT"

env_args=(CRATONVM_JIT_METRICS=1 "CRATONVM_JIT_METRICS_OUT=$OUT" CRATONVM_QUIET_DEPRECATIONS=1)
if [[ "$FLAG_ON" == "1" ]]; then
  env_args+=(CRATONVM_JIT_GUARDED_VIRTUAL_INLINE=1 CRATONVM_TIER_PGO=1)
fi

timeout "$TIMEOUT" env "${env_args[@]}" "$EXE" "$@" >/dev/null 2>&1
run_status=$?

if [[ ! -s "$OUT" ]]; then
  # A zero here would be a real answer for some workloads, so it must never be
  # confused with "the instrument did not run". No file at all means the
  # latter.
  echo "guarded-inline-reach.sh: no metrics file was written (run exit $run_status)." >&2
  echo "  CRATONVM_JIT_METRICS is read through the live-environment path; check that" >&2
  echo "  the binary is recent enough to declare it." >&2
  exit 1
fi

python3 - "$OUT" "$LABEL" "$FLAG_ON" <<'PY'
import json, sys, collections

path, label, flag_on = sys.argv[1], sys.argv[2], sys.argv[3] == "1"
installed = single = optimizing = with_candidates = speculative = 0
refusals = collections.Counter()
spliced_bytecodes = 0

def value(field):
    """A Measured<T> renders as null when nobody measured it. `null` and 0 are
    different claims and this script must not merge them."""
    return field if isinstance(field, (int, float)) else None

for line in open(path, encoding="utf-8"):
    line = line.strip()
    if not line:
        continue
    try:
        r = json.loads(line)
    except json.JSONDecodeError:
        continue
    if r.get("outcome") != "installed":
        continue
    installed += 1
    if r.get("path") == "single_pass":
        single += 1
    elif r.get("path") == "optimizing":
        optimizing += 1
        continue
    cands = value(r.get("inline_candidates"))
    if cands is not None:
        with_candidates += 1
    spec = value(r.get("speculative_inlined_sites")) or 0
    if spec > 0:
        speculative += 1
    spliced_bytecodes += value(r.get("inlined_bytecodes")) or 0
    for category, n in (r.get("inline_refusals") or {}).items():
        refusals[category] += n

pct = lambda n, d: f"{100.0 * n / d:5.1f}%" if d else "    - "

print(f"guarded-inline reach — {label}"
      f"  ({'GUARDED_VIRTUAL_INLINE=1 TIER_PGO=1' if flag_on else 'default flags'})")
print(f"  installed bodies         {installed:6d}")
print(f"  single-pass              {single:6d}  {pct(single, installed)}   <- the only population this feature can reach")
print(f"  optimizing (IR)          {optimizing:6d}  {pct(optimizing, installed)}   <- out of reach; the IR tier plans no inline")
print(f"  single-pass w/ candidates{with_candidates:6d}  {pct(with_candidates, single)} of single-pass")
print(f"  single-pass w/ a splice  {speculative:6d}  {pct(speculative, single)} of single-pass")
print(f"  callee bytecodes spliced {spliced_bytecodes:6d}")
if refusals:
    print("  refusals (single-pass bodies):")
    for category, n in refusals.most_common():
        print(f"    {category:34s} {n:6d}")
else:
    print("  refusals: none recorded")
PY
