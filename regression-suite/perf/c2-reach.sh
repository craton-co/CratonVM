#!/bin/bash
# c2-reach.sh — how far into the OPTIMIZING (C2/IR) tier does a workload get?
#
# MEAS-02's increment 1: "a C2-reach column in the survey, so 'does this reach
# the optimizing tier' is answered before anything is anchored". This is the
# command that answers it, for any workload, in one run.
#
# It answers three questions and refuses to conflate them:
#
#   requests  compile requests that reached the admission chain at all. Most
#             are C1 requests (`optimize=false`); that is normal tiering, not
#             a gap, and it is counted separately below.
#   admitted  the chain's verdict was "admitted to the optimizing pipeline".
#   bodies    the optimizing backend actually produced a compiled body.
#
# `admitted - bodies` is the pipeline taking a method it then cannot lower —
# the `cov-*` lanes' population. `requests - admitted` is the admission chain
# declining up front, and the verdict histogram says which conjunct did it.
#
# What this is NOT: a timing. Every number here is a count, so a loaded host
# does not affect it — and no conclusion about speed can be drawn from it.
#
# Usage:
#   c2-reach.sh -Exe /abs/path/to/cratonvm [options] -- <vm args...>
#
#   c2-reach.sh -Exe "$PWD/target/release/cratonvm" --label bintrees -- \
#       -Xmx8g -cp /tmp/classes CratonBench bintrees
#
# Options:
#   -Exe PATH      CratonVM binary (required).
#   --label NAME   Row label in the output (default: the last VM argument).
#   --timeout S    Kill the run after S seconds (default 900).
#   --keep FILE    Keep the raw stderr here instead of a temp file.
#   --top N        How many histogram rows to print (default 12).
#   --tsv          Print one machine-readable TSV row instead of a report:
#                  label requests admitted bodies c1_requests c1 c2 osr
#                  (the last three are the tier manager's own compile counts)
#
# A zero here is a real answer — three of CratonBench's seven phases issue no
# compile request at all — so "no output" must never be allowed to look like
# it. The run therefore also asks for the tier manager's shutdown summary and
# uses it as an INDEPENDENT witness: it is emitted by a different module, and
# a C1 or C2 compile cannot happen without passing the admission chain. If
# that summary reports compiles and this scrape saw no admission line, the
# scrape is broken and the run is refused rather than reported as zero.
#
# OSR is subtracted before that comparison, and the reason is measured, not
# assumed. An OSR compile goes through `compile_osr_artifact`, which calls the
# backend directly — a second compile door that does not pass the admission
# chain — but it is still counted by the tier manager under the TIER it was
# requested at, so a C2-tier OSR compile lands in `c2=`. Measured on the Azure
# bench host 2026-08-03: CratonBench's `arithmetic` phase reports
# `c1=0 c2=1 osr=1` with zero admission lines. That one C2 compile is the OSR
# one. A check that did not subtract `osr` called this phase's genuine reach
# of zero a broken scrape — which is why the comparison is
# `c1 + c2 - osr > requests`, and why `osr>0` with `requests=0` is normal for
# a phase that is one long loop inside one method.
#
# Exit codes: 0 = measured (whatever the workload's own exit code), 2 = usage
# or setup error, 3 = refused — the witness disagrees with the scrape, or the
# VM emitted no shutdown summary to witness with.
set -u
export LC_ALL=C

EXE=""; LABEL=""; TIMEOUT=900; KEEP=""; TOP=12; TSV=0
while [ $# -gt 0 ]; do
    case "$1" in
        -Exe) EXE="$2"; shift 2 ;;
        --label) LABEL="$2"; shift 2 ;;
        --timeout) TIMEOUT="$2"; shift 2 ;;
        --keep) KEEP="$2"; shift 2 ;;
        --top) TOP="$2"; shift 2 ;;
        --tsv) TSV=1; shift ;;
        --) shift; break ;;
        *) echo "unknown arg: $1" >&2; exit 2 ;;
    esac
done

[ -n "$EXE" ] && [ -x "$EXE" ] || { echo "FATAL: -Exe PATH required (executable)" >&2; exit 2; }
[ $# -gt 0 ] || { echo "FATAL: no VM arguments after --" >&2; exit 2; }
[ -n "$LABEL" ] || LABEL="${*: -1}"

if [ -n "$KEEP" ]; then ERR="$KEEP"; else ERR=$(mktemp); trap 'rm -f "$ERR"' EXIT; fi

# The grouped spelling. The per-flag name (CRATONVM_DBG_IR_COMPILES=1) still
# works but makes the VM open stderr with a deprecation line, and a harness
# whose first line of output is a configuration warning is one whose output
# stops being read.
if command -v timeout >/dev/null 2>&1; then
    timeout "$TIMEOUT" env CRATONVM_DBG='jit-method-stats,ir-compiles' "$EXE" "$@" >/dev/null 2>"$ERR"
else
    env CRATONVM_DBG='jit-method-stats,ir-compiles' "$EXE" "$@" >/dev/null 2>"$ERR"
fi
WORKLOAD_RC=$?

# `grep -c` (not `grep | wc -l`): an empty match exits non-zero with a count
# of 0, and 0 is a measurement here, not a missing field.
REQ=$(grep -c '^\[ir\] admission ' "$ERR"); REQ=${REQ:-0}
ADM=$(grep -c ': admitted to the optimizing pipeline$' "$ERR"); ADM=${ADM:-0}
BOD=$(grep -c '^\[ir\] optimizing backend produced a body ' "$ERR"); BOD=${BOD:-0}
C1REQ=$(grep -c ': optimize=false' "$ERR"); C1REQ=${C1REQ:-0}

# The independent witness.
CSEG=$(grep -oE 'compiles: c1=[0-9]+ c2=[0-9]+ osr=[0-9]+' "$ERR" | head -1)
if [ -z "$CSEG" ]; then
    echo "REFUSED: the VM printed no 'compiles: c1=… c2=… osr=…' shutdown summary." >&2
    echo "  Without it nothing corroborates the reach scrape, and a scrape with no" >&2
    echo "  witness reports zero when it breaks. Check that this binary honours" >&2
    echo "  CRATONVM_DBG=jit-method-stats." >&2
    echo "  Raw stderr: $ERR (workload exit $WORKLOAD_RC)" >&2
    exit 3
fi
WC1=$(printf '%s' "$CSEG" | grep -oE 'c1=[0-9]+' | head -1 | grep -oE '[0-9]+$')
WC2=$(printf '%s' "$CSEG" | grep -oE ' c2=[0-9]+' | head -1 | grep -oE '[0-9]+$')
WOSR=$(printf '%s' "$CSEG" | grep -oE 'osr=[0-9]+' | head -1 | grep -oE '[0-9]+$')

if [ "$REQ" -lt "$ADM" ] || [ "$ADM" -lt "$BOD" ]; then
    echo "REFUSED: requests=$REQ admitted=$ADM bodies=$BOD is not monotone." >&2
    echo "  Every body comes from an admitted method and every admitted method from a" >&2
    echo "  request, so this is the scrape misreading the log, not the VM." >&2
    exit 3
fi
NONOSR=$(( WC1 + WC2 - WOSR )); [ "$NONOSR" -lt 0 ] && NONOSR=0
if [ "$NONOSR" -gt "$REQ" ]; then
    echo "REFUSED: the tier manager reports c1=$WC1 c2=$WC2 osr=$WOSR — $NONOSR non-OSR" >&2
    echo "  compile(s) — but only $REQ compile request(s) reached the admission chain." >&2
    echo "  A non-OSR compile cannot do that, so the '[ir] admission' line in" >&2
    echo "  jit/src/lib.rs has moved or been reworded and this scrape is reading a log" >&2
    echo "  that no longer says what it expects. Raw stderr: $ERR" >&2
    exit 3
fi

if [ "$TSV" = 1 ]; then
    printf '%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\n' \
        "$LABEL" "$REQ" "$ADM" "$BOD" "$C1REQ" "$WC1" "$WC2" "$WOSR"
    exit 0
fi

echo "workload : $LABEL"
echo "command  : $EXE $*"
echo "exit     : $WORKLOAD_RC"
echo
printf 'compile requests reaching the admission chain : %s\n' "$REQ"
printf '  ...of which C1 requests (optimize=false)    : %s\n' "$C1REQ"
printf 'admitted to the optimizing pipeline           : %s\n' "$ADM"
printf 'bodies the optimizing backend produced        : %s\n' "$BOD"
printf 'admitted but never lowered                    : %s\n' "$(( ADM - BOD ))"
echo
printf 'tier manager, same run: c1=%s c2=%s osr=%s\n' "$WC1" "$WC2" "$WOSR"
echo "  osr compiles do NOT pass the admission chain (compile_osr_artifact calls the"
echo "  backend directly), so osr>0 with requests=0 is a workload that is one long"
echo "  loop in one method — not a broken measurement."
echo

# The identifier on an admission line never contains a space, and a verdict
# can (`RBC.6: a handler reads a non-parameter local`) — so the split is on
# the FIRST ": " after the identifier, not the last.
echo "admission verdicts:"
awk '/^\[ir\] admission /{ sub(/^\[ir\] admission [^ ]+: /, ""); c[$0]++ }
     END { for (v in c) printf "%8d  %s\n", c[v], v }' "$ERR" | sort -rn | head -"$TOP"

GAPS=$(awk '/has no lowering for opcode /{ sub(/^.*opcode /, ""); c[$1]++ }
            END { for (v in c) printf "%8d  opcode %s\n", c[v], v }' "$ERR" | sort -rn | head -"$TOP")
if [ -n "$GAPS" ]; then
    echo
    echo "IrBuilder::build opcode gaps (why an ADMITTED method produced no body):"
    printf '%s\n' "$GAPS"
fi

REFUSALS=$(awk '/IrBuilder::build refused at /{ sub(/^.*refused at /, ""); c[$1]++ }
                END { for (v in c) printf "%8d  %s\n", c[v], v }' "$ERR" | sort -rn | head -"$TOP")
if [ -n "$REFUSALS" ]; then
    echo
    echo "IrBuilder::build structural refusals (site in ir.rs):"
    printf '%s\n' "$REFUSALS"
fi

[ -n "$KEEP" ] && echo && echo "raw stderr kept at: $KEEP"
exit 0
