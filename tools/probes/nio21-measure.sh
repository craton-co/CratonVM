#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
# Copyright 2024-2026 Craton Software Company
#
# nio21 wave 8 L1: the one quiet host measurement pass that retires (or keeps)
# the docs/known-issues/jit/r20-*/r21-* pages marked PENDING HOST MEASUREMENT.
# Plan and per-page PASS criteria:
#   docs/internal/fixed-bugs/r21-measurement-plan-w8-CLOSED-20261012.md
#
# Usage (on the Linux host, from anywhere):
#   tools/probes/nio21-measure.sh <T> [section-regex]  2>&1 | tee /data/nio21/out/m8-<T>.log
#     <T>             binary tag: runs $K/bin/cratonvm-nio21-<T>
#     section-regex   optional ERE over the section names (default: every section)
#                     e.g. 'bench-' (benches only), 'r21-.*cas', '^r20-fence'
#   env REPS=5        bench repetitions (interleaved by bench.sh)
#   env TMO=900       per-run timeout, seconds
#   env REBENCH=1     re-run a bench whose out/bench-<tag>.txt already exists
#   env R16CP / R17CP class dirs holding R16Fieldsite2Layout / R17NioRows (host kit,
#                     not in the repo); their sections are skipped when absent
#
# Kit (prepared by the orchestrator):
#   K=/data/nio21, probes javac'd into $K/cls from tools/probes/Nio2[01]*.java with
#   --add-exports java.base/jdk.internal.misc=ALL-UNNAMED; HotSpot `java` on PATH.
#   $K/bench.sh <outtag> <reps> <Probe> "<label>;<ENV=..>;<vmargs>;<tag>" ...
#       env JPROPS (-D props), PARGS (main args); "hs;;" = HotSpot; writes out/bench-<outtag>.txt
#       ASSUMPTION: several env assignments in one arm are separated by spaces.
#   python3 $K/med.py out/bench-<outtag>.txt  prints medians.
#
# Conformance here does NOT use $K/run.sh: most checks select rows or main args, and
# run.sh diffs against the one $K/ref/<Probe>.ref. `conf` below runs HotSpot once per
# distinct argument list (cached under $O/hs), runs cratonvm with the arm's env and VM
# args, drops `... ns/op` timing lines from both stdouts, and prints PASS / DIFF / VOID
# (timeout). Exit codes must match too. Every command line is saved next to its output.
#
# Launcher default is --jdk-only, so the "jo" arms pass no mode flag.

set -u
T=${1:?usage: nio21-measure.sh <binary-tag> [section-regex]}
SEL=${2:-.}
K=/data/nio21
export JAVA_HOME=/data/jdkimages/jdk25-linux/jdk-25.0.4+7
CVM=$K/bin/cratonvm-nio21-$T
CP=$K/cls
REPS=${REPS:-5}
TMO=${TMO:-900}
R16CP=${R16CP:-/data/jitr17/cls}
R17CP=${R17CP:-/data/jitr17/cls}
cd "$K" || exit 1
O=$K/out/m8-$T
mkdir -p "$O/hs" "$O/conf" "$O/log" out

# ---------------------------------------------------------------- arms (env strings)
CO="--compatible"
SPC="CRATONVM_TIER_C2_THRESHOLD=1000000000 CRATONVM_JIT_OSR_OPTIMIZING=0"   # single-pass control
EDG="CRATONVM_JIT_IR_CALL_SITE_EXCEPTION_EDGES=1"
IMP="CRATONVM_JIT_IR_CALL_SITE_EXCEPTION_EDGES=1 CRATONVM_JIT_IR_IMPLICIT_EXCEPTION_EDGES=1"
SCH="CRATONVM_JIT_IR_SPLICE_CALLEE_HANDLERS=1 CRATONVM_JIT_IR_CALL_SITE_EXCEPTION_EDGES=1"
SPX="CRATONVM_JIT_NO_EXC_TABLE_C2=1"
SPX1="CRATONVM_JIT_NO_EXC_TABLE_C2=1 CRATONVM_JIT_LOCAL_HANDLERS_WITHOUT_PRECISE_FRAMES=1"
EXIT0="CRATONVM_JIT_SP_NESTED_POST_CALL_SPLICE_EXITS=0 CRATONVM_JIT_IR_EXITS_BESIDE_DROPPED_SNAPSHOT_VALUES=0 CRATONVM_JIT_IR_BACK_EDGE_VIRTUAL_OBJECT_EXITS=0"
MASK0="CRATONVM_JIT_IR_MASK_GUARD_ELIDE=0 CRATONVM_JIT_IR_MASK_PREDICATE_EARLY=0 CRATONVM_JIT_IR_UNROLL_DROP_FENCED_LOAD=0"
W6OFF="CRATONVM_JIT_IR_SPLICE_NEVER_RUN_STATIC_EXIT=0 CRATONVM_JIT_IR_SPLICE_UNBINDABLE_NESTED_TO_CALL=0 CRATONVM_JIT_SPLICE_INTRINSIC_JDK_BODY_CALLS=0 CRATONVM_JIT_IR_GUARDED_SPLICE_HOT_CAP=0"
W5OFF="CRATONVM_JIT_SPLICE_INTRINSIC_JDK_BODY=0 CRATONVM_JIT_IR_SPLICE_FORWARDER_EXACT_HOT=0"
THIN0="CRATONVM_JIT_UNSAFE_VOLATILE_DIRECT=0 CRATONVM_JIT_UNSAFE_REF_STORE_DIRECT=0 CRATONVM_JIT_UNSAFE_FENCE_DIRECT=0 CRATONVM_JIT_REFERS_TO_DIRECT=0 CRATONVM_JIT_CARRIER_THREAD_DIRECT=0"
OFF5="CRATONVM_COMPAT_ALIGN=-jdk-scalar-atomics,-jdk-thread-interrupt,-jdk-unsafe-delegations,-jdk-uri,-jdk-shared-secrets"

# ---------------------------------------------------------------- helpers
want() { [[ $1 =~ $SEL ]] || return 1; echo; }
crit() { echo "[$(date -u +%H:%M:%S)] criterion: $*"; }

filt() { grep -v -E ' ns/op!?$' || true; }

# conf <label> "<env assignments>" "<cratonvm-only args>" <java args...>
#   HotSpot gets $HSARGS (HotSpot-only) + the java args; cratonvm gets env + its args + the java args.
conf() {
  local label=$1 envs=$2 xargs=$3; shift 3
  local key ref out d rh rc
  key=$(printf '%s ' "${HSARGS:-}" "$@" | md5sum | cut -c1-16)
  ref=$O/hs/$key
  if [ ! -e "$ref.rc" ]; then
    echo "java ${HSARGS:-} -cp $CP $*" >"$ref.cmd"
    timeout "$TMO" java ${HSARGS:-} -cp "$CP" "$@" >"$ref.out" 2>"$ref.err"; echo $? >"$ref.rc"
  fi
  out=$O/conf/$label
  if [ ! -e "$out.rc" ]; then
    echo "env $envs $CVM $xargs -cp $CP $*" >"$out.cmd"
    env $envs timeout "$TMO" "$CVM" $xargs -cp "$CP" "$@" >"$out.out" 2>"$out.err"; echo $? >"$out.rc"
  fi
  diff <(filt <"$ref.out") <(filt <"$out.out") >"$out.diff"; d=$?
  rh=$(cat "$ref.rc"); rc=$(cat "$out.rc")
  if [ "$rc" = 124 ]; then echo "VOID $label (timeout ${TMO}s)"
  elif [ "$d" -eq 0 ] && [ "$rh" = "$rc" ]; then echo "PASS $label"
  else
    echo "DIFF $label: $(grep -c '^[<>]' "$out.diff") lines, rc hs=$rh cvm=$rc  ($out.diff)"
    head -n 12 "$out.diff" | sed 's/^/    /'
  fi
}
# conf2: the same arm under --jdk-only (label.jo) and --compatible (label.co)
conf2() { local label=$1 envs=$2 xargs=$3; shift 3
  conf "$label.jo" "$envs" "$xargs" "$@"; conf "$label.co" "$envs" "$CO $xargs" "$@"; }
# show <conf-label> <ERE>: the matching stdout lines of a conf run (both VMs)
show() { echo "  [$1] cvm:"; grep -E -- "$2" "$O/conf/$1.out" | sed 's/^/    /' | head -n 40; }

# logrun <label> "<envs>" "<cratonvm args>" <java args...>: stdout+stderr into $O/log/<label>.log
logrun() { local label=$1 envs=$2 xargs=$3; shift 3; local f=$O/log/$label.log
  if [ ! -s "$f" ]; then echo "env $envs $CVM $xargs -cp $CP $*" >"$f.cmd"
    env $envs timeout "$TMO" "$CVM" $xargs -cp "$CP" "$@" >"$f" 2>&1; fi; }
# count <log-label> <ERE>...: one COUNT line per pattern
count() { local label=$1 p; shift
  for p in "$@"; do echo "COUNT $label /$p/ = $(grep -c -E -- "$p" "$O/log/$label.log")"; done; }
# lines <log-label> <ERE> [n]: the first n matching lines
lines() { grep -E -- "$2" "$O/log/$1.log" | head -n "${3:-12}" | sed 's/^/    /'; }

A() { printf '%s;%s;%s;%s' "$1" "$2" "$3" "$T"; }     # A <label> "<envs>" "<vmargs>"
HS="hs;;"
# bench <tag> <Probe> <arm>...   (JPROPS / PARGS taken from the environment of the call)
bench() { local tag=m8$T-$1; shift
  if [ -s "out/bench-$tag.txt" ] && [ -z "${REBENCH:-}" ]; then echo "(reusing out/bench-$tag.txt)"
  else echo "[$(date -u +%H:%M:%S)] JPROPS=${JPROPS:-} PARGS=${PARGS:-}"
    JPROPS="${JPROPS:-}" PARGS="${PARGS:-}" "$K/bench.sh" "$tag" "$REPS" "$@"; fi
  python3 "$K/med.py" "out/bench-$tag.txt"; }
# med <tag>: re-print a bench's medians inside a page section
med() { local f=out/bench-m8$T-$1.txt
  if [ -s "$f" ]; then echo "  medians of bench $1:"; python3 "$K/med.py" "$f" | sed 's/^/    /'
  else echo "  (bench $1 not run: select section bench-$1)"; fi; }

# ibench <tag> <cp> "<java args incl. -D props and main class>" <label;envs;vmargs;hs|cvm>...
# Interleaved bench for the host-kit R16 / R17 classes (bench.sh only knows $K/cls).
ibench() { local tag=$1 cp=$2 main=$3; shift 3; local f=$O/ibench-$tag.txt r arm lab envs vma kind
  if [ ! -s "$f" ] || [ -n "${REBENCH:-}" ]; then : >"$f"
    for ((r = 1; r <= REPS; r++)); do for arm in "$@"; do
      IFS=';' read -r lab envs vma kind <<<"$arm"
      if [ "$kind" = hs ]; then timeout "$TMO" java -cp "$cp" $main >"$O/ib.tmp" 2>&1
      else env $envs timeout "$TMO" "$CVM" $vma -cp "$cp" $main >"$O/ib.tmp" 2>&1; fi
      awk -v a="$lab" -v r="$r" '{ for (i = 2; i <= NF; i++) if ($i == "ns/op" || $i == "us/op") print a, $1, $(i-1), $i }' "$O/ib.tmp" >>"$f"
    done; done
  fi
  python3 - "$f" <<'PY'
import sys, statistics, collections
v = collections.defaultdict(list)
for ln in open(sys.argv[1]):
    p = ln.split()
    if len(p) == 4:
        try: v[(p[1], p[0], p[3])].append(float(p[2]))
        except ValueError: pass
for (row, arm, unit), xs in sorted(v.items()):
    print(f"  {row:14s} {arm:12s} {statistics.median(xs):12.1f} {unit}  (n={len(xs)})")
PY
}

# census <label> "<envs>" "<cratonvm args>" <java args...>: native-registry dump with exact invocations
census() { local label=$1 envs=$2 xargs=$3; shift 3
  [ -s "$O/log/$label.json" ] || env CRATONVM_CENSUS_EXACT_INVOCATIONS=1 $envs timeout "$TMO" "$CVM" $xargs \
      --dump-native-registry "$O/log/$label.json" -cp "$CP" "$@" >"$O/log/$label.log" 2>&1; }
# census_diff <a.json> <b.json> [top]: rows in a's top-N (by invocations) absent from b's top-N
census_diff() { python3 - "$1" "$2" "${3:-60}" <<'PY'
import json, sys
def rows(path):
    out = []
    def walk(x):
        if isinstance(x, dict):
            if "invocations" in x and "class" in x and "name" in x:
                out.append((int(x.get("invocations") or 0),
                            f'{x["class"]}.{x["name"]}{x.get("descriptor","")} [{x.get("kind","?")}]'))
            for v in x.values(): walk(v)
        elif isinstance(x, list):
            for v in x: walk(v)
    walk(json.load(open(path)))
    return sorted(out, reverse=True)
a, b, n = rows(sys.argv[1]), rows(sys.argv[2]), int(sys.argv[3])
bset = {r for _, r in b[:n]}
print(f"  top {n} of {sys.argv[1]} absent from the top {n} of {sys.argv[2]}:")
for c, r in a[:n]:
    if r not in bset and c > 0: print(f"    {c:10d} {r}")
PY
}

have_cls() { [ -f "$1/$2.class" ]; }

# ======================================================================== preflight
if want preflight; then
echo "## preflight"
echo "binary: $CVM"; ls -l "$CVM" || { echo "NO BINARY"; exit 1; }
"$CVM" -version 2>&1 | head -n 3
java -version 2>&1 | head -n 1
echo "nproc $(nproc)  load: $(cut -d' ' -f1-3 /proc/loadavg)"
for c in Nio20BufferTyped Nio21CasRace Nio21MaskedFence Nio21GuardedMulti Nio21CharAtTrap Nio21ParamCatch; do
  have_cls "$CP" "$c" || echo "MISSING $CP/$c.class (javac the kit first)"; done
echo "ZGC available? $("$CVM" -XX:+UseZGC -version >/dev/null 2>&1 && echo yes || echo NO)"
echo "R16: $(have_cls "$R16CP" R16Fieldsite2Layout && echo "$R16CP" || echo absent)   R17: $(have_cls "$R17CP" R17NioRows && echo "$R17CP" || echo absent)"
fi

# ======================================================================== benches (quiet first)
# One combined, interleaved bench per probe; the page sections below re-print the medians.

if want bench-bt; then echo "## bench-bt (Nio20BufferTyped rows)"
JPROPS="-Dnio20.rows=heapget,heapput,heaplong,directget,directput,directlong,le,be,single,straight" PARGS=rows \
bench bt Nio20BufferTyped "$HS" "$(A jo "" "")" "$(A co "" "$CO")" \
  "$(A jo-ghoist0 CRATONVM_JIT_IR_LOOP_FIELD_FORWARD_GUARD_HOISTED=0 "")" \
  "$(A co-ghoist0 CRATONVM_JIT_IR_LOOP_FIELD_FORWARD_GUARD_HOISTED=0 "$CO")" \
  "$(A jo-bab0 CRATONVM_JIT_IR_SCOPED_BYTE_ARRAY_BASE=0 "")" \
  "$(A co-bab0 CRATONVM_JIT_IR_SCOPED_BYTE_ARRAY_BASE=0 "$CO")" \
  "$(A jo-trap0 CRATONVM_JIT_IR_BUFFER_TRAP_MISS=0 "")" \
  "$(A jo-lenpred0 CRATONVM_JIT_IR_LENGTH_PREDICATE=0 "")" \
  "$(A jo-loneput0 CRATONVM_JIT_IR_BUFFER_TRAP_LONE_PUT_CALL=0 "")" \
  "$(A jo-spx0 CRATONVM_JIT_SP_NESTED_POST_CALL_SPLICE_EXITS=0 "")" \
  "$(A jo-edv0 CRATONVM_JIT_IR_EXITS_BESIDE_DROPPED_SNAPSHOT_VALUES=0 "")" \
  "$(A jo-bvo0 CRATONVM_JIT_IR_BACK_EDGE_VIRTUAL_OBJECT_EXITS=0 "")" \
  "$(A jo-exit0 "$EXIT0" "")" "$(A co-exit0 "$EXIT0" "$CO")" \
  "$(A jo-ffin0 CRATONVM_JIT_IR_FIELD_FINAL_FORWARD=0 "")" \
  "$(A jo-fload1 CRATONVM_JIT_IR_FIELD_LOAD_FORWARD=1 "")" \
  "$(A jo-afold0 CRATONVM_JIT_IR_SCOPED_ARENA_FOLD=0 "")"
fi

if want bench-ra; then echo "## bench-ra (Nio20RawAlias rows)"
JPROPS="-Dnio20.rows=heapbb,fieldmix,straight" PARGS=rows \
bench ra Nio20RawAlias "$HS" "$(A jo "" "")" "$(A co "" "$CO")" \
  "$(A jo-ghoist0 CRATONVM_JIT_IR_LOOP_FIELD_FORWARD_GUARD_HOISTED=0 "")" \
  "$(A jo-bab0 CRATONVM_JIT_IR_SCOPED_BYTE_ARRAY_BASE=0 "")" \
  "$(A jo-cse0 CRATONVM_JIT_IR_SCOPED_SCREEN_CSE=0 "")"
fi

if want bench-fr; then echo "## bench-fr (Nio20FenceRow, all rows)"
JPROPS="" PARGS="" \
bench fr Nio20FenceRow "$HS" "$(A jo "" "")" "$(A co "" "$CO")" \
  "$(A jo-drop0 CRATONVM_JIT_IR_UNROLL_DROP_FENCED_LOAD=0 "")" \
  "$(A jo-early0 CRATONVM_JIT_IR_MASK_PREDICATE_EARLY=0 "")" \
  "$(A jo-elide0 CRATONVM_JIT_IR_MASK_GUARD_ELIDE=0 "")"
fi

if want bench-mf; then echo "## bench-mf (Nio21MaskedFence rows)"
JPROPS="-Dnio21.rows=own,param,sum,nested" PARGS=rows \
bench mf Nio21MaskedFence "$HS" "$(A jo "" "")" "$(A co "" "$CO")" \
  "$(A jo-elide0 CRATONVM_JIT_IR_MASK_GUARD_ELIDE=0 "")" \
  "$(A jo-early0 CRATONVM_JIT_IR_MASK_PREDICATE_EARLY=0 "")" \
  "$(A jo-drop0 CRATONVM_JIT_IR_UNROLL_DROP_FENCED_LOAD=0 "")" \
  "$(A jo-mask0 "$MASK0" "")"
fi

if want bench-jr; then echo "## bench-jr (Nio20JitRows, all rows)"
JPROPS="" PARGS="" \
bench jr Nio20JitRows "$HS" "$(A jo "" "")" "$(A co "" "$CO")" \
  "$(A jo-elide0 CRATONVM_JIT_IR_MASK_GUARD_ELIDE=0 "")" \
  "$(A jo-edges1 "$EDG" "")" "$(A co-edges1 "$EDG" "$CO")" \
  "$(A jo-lte0 CRATONVM_JIT_IR_LOCAL_THROW_EDGES=0 "")" \
  "$(A jo-sp0 "$SPX" "")" "$(A jo-sp1 "$SPX1" "")" \
  "$(A co-sp0 "$SPX" "$CO")" "$(A co-sp1 "$SPX1" "$CO")"
fi

if want bench-lt; then echo "## bench-lt (Nio21LocalThrow bench)"
JPROPS="-Dnio21.rows=bench" PARGS="" \
bench lt Nio21LocalThrow "$HS" "$(A jo "" "")" \
  "$(A jo-lte0 CRATONVM_JIT_IR_LOCAL_THROW_EDGES=0 "")" "$(A jo-edges1 "$EDG" "")" \
  "$(A jo-sp0 "$SPX" "")" "$(A jo-sp1 "$SPX1" "")"
fi

if want bench-pc; then echo "## bench-pc (Nio21ParamCatch bench, single-pass only)"
JPROPS="-Dnio21.rows=bench" PARGS="" \
bench pc Nio21ParamCatch "$HS" "$(A jo "" "")" \
  "$(A jo-sp0 "$SPX" "")" "$(A jo-sp1 "$SPX1" "")" "$(A co-sp0 "$SPX" "$CO")" "$(A co-sp1 "$SPX1" "$CO")"
fi

if want bench-ct; then echo "## bench-ct (Nio21CallThrow bench)"
JPROPS="-Dnio21.rows=bench" PARGS="" \
bench ct Nio21CallThrow "$HS" "$(A jo "" "")" "$(A co "" "$CO")" \
  "$(A jo-edges1 "$EDG" "")" "$(A co-edges1 "$EDG" "$CO")"
fi

if want bench-ic; then echo "## bench-ic (Nio21ImplicitCatch bench)"
JPROPS="-Dnio21.rows=aioobe,npe,div0,nothrow,fieldnpe,fieldok" PARGS=bench \
bench ic Nio21ImplicitCatch "$HS" "$(A jo "" "")" \
  "$(A jo-imp1 "$IMP" "")" "$(A co-imp1 "$IMP" "$CO")" \
  "$(A jo-imp1-pad0 "$IMP CRATONVM_JIT_IR_IMPLICIT_PAD_FALLTHROUGH=0" "")"
fi

if want bench-sh; then echo "## bench-sh (Nio21SpliceHandler rows; lines are '<row> <N> <t> ns/op')"
JPROPS="-Dnio21.rows=decode,encode" PARGS=rows \
bench sh Nio21SpliceHandler "$HS" "$(A jo "" "")" "$(A co "" "$CO")" \
  "$(A jo-sch1 "$SCH" "")" "$(A co-sch1 "$SCH" "$CO")"
fi

if want bench-cat; then echo "## bench-cat (Nio21CharAtTrap bench)"
JPROPS="-Dnio21.rows=bench" PARGS="" \
bench cat Nio21CharAtTrap "$HS" "$(A jo "" "")" "$(A co "" "$CO")" \
  "$(A jo-rerun0 CRATONVM_JIT_RERUN_SINK_PER_BCI_DESPEC=0 "")" "$(A jo-edges1 "$EDG" "")"
fi

if want bench-gc; then echo "## bench-gc (Nio20GetClass rows)"
JPROPS="-Dnio20.rows=getclass,getclasseq,getclasslit,idhash,classhash,objclasshash -Dnio20.ops=5000000" PARGS="" \
bench gc Nio20GetClass "$HS" "$(A jo "" "")" "$(A co "" "$CO")" \
  "$(A jo-mint0 CRATONVM_JIT_LDC_SLOT_MINT_AT_OFFER=0 "")" \
  "$(A co-mint0 CRATONVM_JIT_LDC_SLOT_MINT_AT_OFFER=0 "$CO")" \
  "$(A jo-enq0 CRATONVM_JIT_LDC_SLOT_MINT_BEFORE_ENQUEUE=0 "")" \
  "$(A jo-sp "$SPC" "")" "$(A jo-sp-idh0 "$SPC CRATONVM_JIT_SP_IDENTITY_HASH_INLINE=0" "")" \
  "$(A co-sp "$SPC" "$CO")" "$(A co-sp-idh0 "$SPC CRATONVM_JIT_SP_IDENTITY_HASH_INLINE=0" "$CO")" \
  "$(A jo-sp-bhc0 "$SPC CRATONVM_JIT_SP_BOX_HASH_CLASS_ARM=0" "")" \
  "$(A co-sp-bhc0 "$SPC CRATONVM_JIT_SP_BOX_HASH_CLASS_ARM=0" "$CO")"
fi

if want bench-el; then echo "## bench-el (Nio21EncodeLoop rows)"
JPROPS="-Dnio21.rows=utf8-encode,buffer-ascii,buffer-mixed" PARGS=rows \
bench el Nio21EncodeLoop "$HS" "$(A jo "" "")" "$(A co "" "$CO")" \
  "$(A jo-npt1 CRATONVM_JIT_IR_SPLICE_NESTED_PROFILE_TRAP=1 "")" \
  "$(A co-npt1 CRATONVM_JIT_IR_SPLICE_NESTED_PROFILE_TRAP=1 "$CO")" \
  "$(A jo-offp0 CRATONVM_JIT_IR_OFFSET_PREDICATE=0 "")" \
  "$(A jo-lcp0 CRATONVM_JIT_IR_LOOP_CLASS_PREDICATE=0 "")" \
  "$(A jo-chs0 CRATONVM_JIT_IR_LENGTH_PREDICATE_CHARAT_SCREEN=0 "")" \
  "$(A jo-lffp0 CRATONVM_JIT_IR_LOOP_FIELD_FORWARD_PREDICATED=0 "")"
fi

if want bench-dl; then echo "## bench-dl (Nio21DecodeLoop rows)"
JPROPS="-Dnio21.rows=asciitail,mixed,cyrillic" PARGS=rows \
bench dl Nio21DecodeLoop "$HS" "$(A jo "" "")" "$(A co "" "$CO")" \
  "$(A jo-room0 CRATONVM_JIT_IR_BCE_ROOM_OFFSET=0 "")" "$(A co-room0 CRATONVM_JIT_IR_BCE_ROOM_OFFSET=0 "$CO")" \
  "$(A jo-mono0 CRATONVM_JIT_IR_MONOTONE_PREDICATE=0 "")" \
  "$(A jo-bcem0 CRATONVM_JIT_IR_BCE_MONOTONE=0 "")"
fi

if want bench-os; then echo "## bench-os (Nio21OffsetSlice rows)"
JPROPS="-Dnio21.rows=slice-sum,slice-fill,slice-copy" PARGS=rows \
bench os Nio21OffsetSlice "$HS" "$(A jo "" "")" "$(A co "" "$CO")" \
  "$(A jo-offp0 CRATONVM_JIT_IR_OFFSET_PREDICATE=0 "")" "$(A jo-bcev0 CRATONVM_JIT_IR_BCE_VAR_OFFSET=0 "")"
fi

if want bench-e20; then echo "## bench-e20 (Nio20EncodeLoop, default rows)"
JPROPS="" PARGS="" \
bench e20 Nio20EncodeLoop "$HS" "$(A jo "" "")" "$(A co "" "$CO")" \
  "$(A jo-surr0 CRATONVM_JIT_IR_CHAR_SURROGATE_SCALAR=0 "")"
fi

if want bench-tm; then echo "## bench-tm (Nio21TreeMapRows bench)"
JPROPS="-Dnio21.bench=1" PARGS="" \
bench tm Nio21TreeMapRows "$HS" "$(A jo "" "")" "$(A co "" "$CO")" \
  "$(A jo-srb0 CRATONVM_JIT_IR_SPLICE_SCALAR_ROW_BODY=0 "")" \
  "$(A jo-w6off "$W6OFF" "")" "$(A jo-w5off "$W5OFF" "")"
fi

if want bench-gm; then echo "## bench-gm (Nio21GuardedMulti bench)"
JPROPS="-Dnio21.bench=1" PARGS="" \
bench gm Nio21GuardedMulti "$HS" "$(A jo "" "")" "$(A co "" "$CO")" \
  "$(A jo-multi0 CRATONVM_JIT_IR_GUARDED_SPLICE_HOT_CAP_MULTI=0 "")" \
  "$(A co-multi0 CRATONVM_JIT_IR_GUARDED_SPLICE_HOT_CAP_MULTI=0 "$CO")"
fi

if want bench-uv; then echo "## bench-uv (Nio21UnsafeVolatile bench)"
JPROPS="-Dnio21.rows=bench" PARGS="" \
bench uv Nio21UnsafeVolatile "$HS" "$(A jo "" "")" "$(A co "" "$CO")" \
  "$(A jo-spct0 CRATONVM_JIT_SP_CURRENT_THREAD_PREFIX=0 "")" \
  "$(A co-spct0 CRATONVM_JIT_SP_CURRENT_THREAD_PREFIX=0 "$CO")" \
  "$(A jo-irct0 CRATONVM_JIT_IR_CURRENT_THREAD_PREFIX=0 "")" \
  "$(A jo-thin0 "$THIN0" "")"
fi

if want bench-cr; then echo "## bench-cr (Nio21CasRace bench)"
JPROPS="-Dnio21.rows=bench" PARGS="" \
bench cr Nio21CasRace "$HS" "$(A jo "" "")" "$(A co "" "$CO")" \
  "$(A jo-frmw0 CRATONVM_JIT_UNSAFE_FIELD_RMW_INLINE=0 "")" \
  "$(A jo-cae0 CRATONVM_JIT_UNSAFE_CAE_INLINE=0 "")" \
  "$(A jo-rmw0 CRATONVM_JIT_UNSAFE_RMW_INLINE=0 "")" \
  "$(A jo-refhw0 CRATONVM_UNSAFE_REF_ARRAY_HW_CAS=0 "")" \
  "$(A jo-arrhw0 CRATONVM_UNSAFE_ARRAY_HW_CAS=0 "")"
fi

if want bench-va; then echo "## bench-va (Nio21VolatileArray bench)"
JPROPS="-Dnio21.rows=bench" PARGS="" \
bench va Nio21VolatileArray "$HS" "$(A jo "" "")" "$(A co "" "$CO")" \
  "$(A jo-vol0 CRATONVM_JIT_UNSAFE_VOLATILE_INLINE=0 "")" "$(A jo-rmw0 CRATONVM_JIT_UNSAFE_RMW_INLINE=0 "")"
fi

if want bench-uo; then echo "## bench-uo (Nio21UnsafeOffheap rows)"
JPROPS="-Dnio21.rows=offget,offput,offlong" PARGS=rows \
bench uo Nio21UnsafeOffheap "$HS" "$(A jo "" "")" "$(A co "" "$CO")" \
  "$(A jo-iroff0 CRATONVM_JIT_IR_UNSAFE_OFFHEAP_PREFIX=0 "")" \
  "$(A co-iroff0 CRATONVM_JIT_IR_UNSAFE_OFFHEAP_PREFIX=0 "$CO")" \
  "$(A jo-spoff0 CRATONVM_JIT_SP_UNSAFE_OFFHEAP_PREFIX=0 "")"
fi

if want bench-af; then echo "## bench-af (Nio21ArenaFold rows)"
JPROPS="-Dnio21.rows=directget,directput,directlong,directle,segconfined,segglobal,segauto" PARGS=rows \
bench af Nio21ArenaFold "$HS" "$(A jo "" "")" "$(A co "" "$CO")" \
  "$(A jo-afold0 CRATONVM_JIT_IR_SCOPED_ARENA_FOLD=0 "")" \
  "$(A jo-ncmf0 CRATONVM_JIT_IR_SCOPED_NONCLOSEABLE_MODEL_FOLD=0 "")" \
  "$(A jo-nckind0 CRATONVM_FFM_SESSION_NONCLOSEABLE_KIND=0 "")" \
  "$(A jo-sp "$SPC" "")" "$(A jo-sp-ncs0 "$SPC CRATONVM_JIT_SP_SCOPED_NEVER_CLOSING_SESSION=0" "")"
fi

if want bench-si; then echo "## bench-si (Nio20StringIr rows)"
JPROPS="-Dnio20.rows=hash,equals,compareto,indexofc,indexofs,sbappend,sbappendstr,sbappendstr4 -Dnio20.ops=5000000" PARGS="" \
bench si Nio20StringIr "$HS" "$(A jo "" "")" "$(A co "" "$CO")" \
  "$(A jo-ssp0 CRATONVM_JIT_IR_STRING_SEARCH_PREFIX=0 "")" "$(A jo-sb0 CRATONVM_JIT_IR_SB_PREFIX=0 "")"
fi

# NOTE: Nio20LocaleProviders / Nio20CompatRows read their bench switch with Boolean.getBoolean,
# so it must be `=true`; the `=1` their headers and the locale page print runs conformance instead.
if want bench-lp; then echo "## bench-lp (Nio20LocaleProviders bench)"
JPROPS="-Dnio20lp.bench=true" PARGS="" \
bench lp Nio20LocaleProviders "$HS" "$(A jo "" "")" "$(A co "" "$CO")" \
  "$(A co-chm0 CRATONVM_COMPAT_ALIGN=-jdk-concurrent-hash-map "$CO")" \
  "$(A co-lp0 CRATONVM_COMPAT_ALIGN=-jdk-locale-providers "$CO")"
fi

if want bench-cr20; then echo "## bench-cr20 (Nio20CompatRows bench)"
JPROPS="-Dnio20cr.bench=true -Dnio20cr.rows=bench-sw,bench-pwsw,bench-pwbaos,bench-chmobj,bench-chmlocale,bench-chmstr" PARGS="" \
bench cr20 Nio20CompatRows "$HS" "$(A jo "" "")" "$(A co "" "$CO")" \
  "$(A co-all CRATONVM_COMPAT_ALIGN=all "$CO")" \
  "$(A co-baos0 CRATONVM_COMPAT_ALIGN=-jdk-byte-array-streams "$CO")"
fi

if want bench-w3; then echo "## bench-w3 (Nio21W3Doors bench)"
JPROPS="-Dnio21.bench=1 -Dnio21.rows=bench-baos,bench-uri,bench-atomic,bench-interrupted,bench-isinterrupted,bench-wcas,bench-acq" PARGS="" \
bench w3 Nio21W3Doors "$HS" "$(A jo "" "")" "$(A co "" "$CO")" \
  "$(A co-baos0 CRATONVM_COMPAT_ALIGN=-jdk-byte-array-streams "$CO")" \
  "$(A co-off5 "$OFF5" "$CO")"
fi

if want bench-hh; then echo "## bench-hh (Nio20HttpHot, every row, against ONE HotSpot server)"
java -cp "$CP" Nio20HttpHot serve 18080 >"$O/log/hh-server.log" 2>&1 & HHPID=$!
for i in $(seq 1 60); do (exec 3<>/dev/tcp/127.0.0.1/18080) 2>/dev/null && break; sleep 0.5; done
JPROPS="-Dnio20.url=http://127.0.0.1:18080/get" PARGS="" \
bench hh Nio20HttpHot "$HS" "$(A jo "" "")" "$(A co "" "$CO")" "$(A co-off5 "$OFF5" "$CO")"
# census on hc, both modes (same server)
census hc-jo "" "" -Dnio20.rows=hc -Dnio20.url=http://127.0.0.1:18080/get Nio20HttpHot
census hc-co "" "$CO" -Dnio20.rows=hc -Dnio20.url=http://127.0.0.1:18080/get Nio20HttpHot
kill "$HHPID" 2>/dev/null; wait "$HHPID" 2>/dev/null
fi

if want bench-r16; then echo "## bench-r16 (host-kit R16Fieldsite2Layout: refarray swriter date osw locale)"
if have_cls "$R16CP" R16Fieldsite2Layout; then
ibench r16 "$R16CP" "-Dfs2.rows=refarray,swriter,date,osw,locale R16Fieldsite2Layout" \
  "hs;;;hs" "jo;;;cvm" "co;;$CO;cvm" "co-all;CRATONVM_COMPAT_ALIGN=all;$CO;cvm" \
  "co-baos0;CRATONVM_COMPAT_ALIGN=-jdk-byte-array-streams;$CO;cvm" \
  "co-chm0;CRATONVM_COMPAT_ALIGN=-jdk-concurrent-hash-map;$CO;cvm"
else echo "SKIP: no R16Fieldsite2Layout.class under R16CP=$R16CP"; fi
fi

if want bench-r17; then echo "## bench-r17 (host-kit R17NioRows: heapbb utfdecode utfencode getclass fence heapcb)"
if have_cls "$R17CP" R17NioRows; then
ibench r17 "$R17CP" "-Dnio17.rows=heapbb,utfdecode,utfencode,getclass,fence,heapcb R17NioRows" \
  "hs;;;hs" "jo;;;cvm" "co;;$CO;cvm" \
  "jo-bab0;CRATONVM_JIT_IR_SCOPED_BYTE_ARRAY_BASE=0;;cvm" \
  "jo-room0;CRATONVM_JIT_IR_BCE_ROOM_OFFSET=0;;cvm" \
  "jo-offp0;CRATONVM_JIT_IR_OFFSET_PREDICATE=0;;cvm" \
  "jo-mint0;CRATONVM_JIT_LDC_SLOT_MINT_AT_OFFER=0;;cvm" \
  "jo-drop0;CRATONVM_JIT_IR_UNROLL_DROP_FENCED_LOAD=0;;cvm" \
  "jo-fload1;CRATONVM_JIT_IR_FIELD_LOAD_FORWARD=1;;cvm"
else echo "SKIP: no R17NioRows.class under R17CP=$R17CP"; fi
fi

# ======================================================================== pages

if want r20-buffer-loop-barriers-left-after-the-raw-class-CLOSED-20261012.md; then
echo "## r20-buffer-loop-barriers-left-after-the-raw-class-CLOSED-20261012.md"
crit "conf PASS in all arms; '>=1 loop(s) with a hoisted class guard'; GET rows default <= ghoist0 arm"
conf2 bt-conf "" "" Nio20BufferTyped conf
conf2 bt-conf-ghoist0 CRATONVM_JIT_IR_LOOP_FIELD_FORWARD_GUARD_HOISTED=0 "" Nio20BufferTyped conf
logrun ra-heapbb-licm "CRATONVM_DBG_LICM=1 CRATONVM_DBG_JITC=1 CRATONVM_DBG_LOOP_FIELD_FORWARD=1" "" -Dnio20.rows=heapbb Nio20RawAlias rows
count ra-heapbb-licm 'loop\(s\) with a hoisted class guard' 'loop field forward: header bci' 'ordering or opaque write'
lines ra-heapbb-licm 'hoisted class guard|loop field forward: header bci|ordering or opaque write'
med bt; med ra
fi

if want r20-compatible-bytearrayoutputstream-carries-the-httpurlconnection-stream-hook-CLOSED-20261012.md; then
echo "## r20-compatible-bytearrayoutputstream-carries-the-httpurlconnection-stream-hook-CLOSED-20261012.md"
crit "R16 osw: co within noise of jo, and co-baos0 slower; fallback: bench-baos / bench-pwbaos co ~= jo"
crit "(retire also needs the 25/linux --compatible kind map regenerated and the stub ratchet re-frozen: not a probe)"
[ -s "$O/ibench-r16.txt" ] && ibench r16 "$R16CP" "" || echo "  (R16 bench absent)"
med w3; med cr20
fi

if want r20-compatible-locale-providers-pay-the-concurrenthashmap-stand-in-FIXED-20261012.md; then
echo "## r20-compatible-locale-providers-pay-the-concurrenthashmap-stand-in-FIXED-20261012.md"
crit "bench-chmobj co/jo no longer ~10x (target <= 2x); R16 locale co <= 2x jo (~32 us)"
med lp
[ -s "$O/ibench-r16.txt" ] && ibench r16 "$R16CP" "" || echo "  (R16 bench absent)"
fi

if want r20-compatible-refarray-swriter-date-gap-is-not-a-stand-in-CLOSED-20261012.md; then
echo "## r20-compatible-refarray-swriter-date-gap-is-not-a-stand-in-CLOSED-20261012.md"
crit "R16 refarray/swriter/date: co within noise (<=1.3x) of jo; fallback rows: Nio20JitRows date, CompatRows bench-sw/pwsw"
[ -s "$O/ibench-r16.txt" ] && ibench r16 "$R16CP" "" || echo "  (R16 bench absent)"
med jr; med cr20
echo "  if a gap remains and co-all does not close it: CRATONVM_CENSUS_EXACT_INVOCATIONS=1 on that row in both modes"
fi

if want r20-compiled-ldc-sites-baked-on-the-worker-call-the-helper-forever-FIXED-20261010.md; then
echo "## r20-compiled-ldc-sites-baked-on-the-worker-call-the-helper-forever-FIXED-20261010.md"
crit "conf PASS every arm; getclasseq ~= getclass; getclasslit default << mint0; no getclasseq loss vs enq0; 'ldc slots at offer' n>0"
conf2 gc-conf "" "" -Dnio20.rows=conf Nio20GetClass
conf2 gc-conf-mint0 CRATONVM_JIT_LDC_SLOT_MINT_AT_OFFER=0 "" -Dnio20.rows=conf Nio20GetClass
conf gc-conf-enq0.jo CRATONVM_JIT_LDC_SLOT_MINT_BEFORE_ENQUEUE=0 "" -Dnio20.rows=conf Nio20GetClass
logrun gc-ldc CRATONVM_DBG_JITC=1 "" -Dnio20.rows=getclasseq,getclasslit -Dnio20.ops=2000000 Nio20GetClass
count gc-ldc 'ldc slots at offer' 'ldc slots at offer .*: [1-9][0-9]*/'
grep -n -E 'ldc slots at offer|Nio20GetClass' "$O/log/gc-ldc.log" | head -n 16 | sed 's/^/    /'
med gc
fi

if want r20-directbb-every-access-is-two-calls-and-an-arena-lookup-CLOSED-20261012.md; then
echo "## r20-directbb-every-access-is-two-calls-and-an-arena-lookup-CLOSED-20261012.md"
crit "conf PASS every arm; offget/offput/offlong default << iroff0; directget/put/long default << afold0;"
crit "  segglobal/segauto: jo-sp < jo-sp-ncs0, segconfined unchanged"
conf2 uo-conf "" "" Nio21UnsafeOffheap conf
conf uo-conf-iroff0.jo CRATONVM_JIT_IR_UNSAFE_OFFHEAP_PREFIX=0 "" Nio21UnsafeOffheap conf
conf uo-conf-spoff0.jo CRATONVM_JIT_SP_UNSAFE_OFFHEAP_PREFIX=0 "" Nio21UnsafeOffheap conf
conf2 ua-conf "" "" Nio20UnsafeAccess conf
conf2 af-conf "" "" Nio21ArenaFold conf
logrun uo-inline CRATONVM_DBG_JITC=1 "" -Dnio21.rows=offget,offput,offlong Nio21UnsafeOffheap rows
count uo-inline 'ir-unsafe-inline n[0-9]+ off-heap arm \(arena table: true\)' 'off-heap arm'
med uo; med af; med bt
fi

if want r20-fence-row-residual-is-the-loop-and-the-call-not-the-fence-CLOSED-20261012.md; then
echo "## r20-fence-row-residual-is-the-loop-and-the-call-not-the-fence-CLOSED-20261012.md"
crit "check line PASS every arm; own & hoisted ~= noload and < drop0; nofence, inline unchanged"
conf2 fr-conf "" "" -Dnio20.n=20000 Nio20FenceRow
conf fr-conf-drop0.jo CRATONVM_JIT_IR_UNROLL_DROP_FENCED_LOAD=0 "" -Dnio20.n=20000 Nio20FenceRow
logrun fr-own-unroll "CRATONVM_DBG_UNROLL=1 CRATONVM_DBG_LICM=1" "" -Dnio20.rows=own Nio20FenceRow
count fr-own-unroll 'early \(unroll candidate\)' 'pure-body' 'fully unrolled' 'loop side effect'
med fr
fi

if want r20-heapbb-twelve-x-residual-is-guards-not-calls-CLOSED-20261012.md; then
echo "## r20-heapbb-twelve-x-residual-is-guards-not-calls-CLOSED-20261012.md"
crit "conf PASS both arms both modes; scoped-byte-array-base lines > 0; no new [ir] refusal; heap rows default <= bab0"
crit "retire only if RawAlias heapbb (and R17 heapbb) is within ~3x HotSpot"
conf2 bt-conf "" "" Nio20BufferTyped conf
conf2 bt-conf-bab0 CRATONVM_JIT_IR_SCOPED_BYTE_ARRAY_BASE=0 "" Nio20BufferTyped conf
conf2 sf-conf "" "" Nio20ScopedFold conf
conf2 sf-conf-bab0 CRATONVM_JIT_IR_SCOPED_BYTE_ARRAY_BASE=0 "" Nio20ScopedFold conf
logrun ra-bab CRATONVM_DBG_JITC=1 "" -Dnio20.rows=heapbb Nio20RawAlias rows
logrun ra-bab0 "CRATONVM_DBG_JITC=1 CRATONVM_JIT_IR_SCOPED_BYTE_ARRAY_BASE=0" "" -Dnio20.rows=heapbb Nio20RawAlias rows
count ra-bab 'scoped-byte-array-base \(ir\)' '\[ir\].*(refus|REFUS)'
count ra-bab0 'scoped-byte-array-base \(ir\)' '\[ir\].*(refus|REFUS)'
med ra; med bt
[ -s "$O/ibench-r17.txt" ] && ibench r17 "$R17CP" "" || echo "  (R17 bench absent)"
fi

if want r20-identity-hash-is-a-native-call-in-compiled-code-FIXED-20261012.md; then
echo "## r20-identity-hash-is-a-native-call-in-compiled-code-FIXED-20261012.md"
crit "single-pass arm (jo-sp/co-sp) idhash & classhash within ~2x HotSpot and << sp-idh0 (~60 ns); conf PASS every arm"
conf2 gc-conf-sp "$SPC" "" -Dnio20.rows=conf Nio20GetClass
conf2 gc-conf-sp-idh0 "$SPC CRATONVM_JIT_SP_IDENTITY_HASH_INLINE=0" "" -Dnio20.rows=conf Nio20GetClass
logrun gc-sp-idh "$SPC CRATONVM_DBG_JITC=1" "" -Dnio20.rows=idhash,classhash -Dnio20.ops=2000000 Nio20GetClass
count gc-sp-idh 'identity-hash-inline \(sp-dispatch\)' 'identity-hash-inline \(ir'
med gc
fi

if want r20-jdk-http-client-throughput-is-ten-to-fifty-x-hotspot-CLOSED-20261012.md; then
echo "## r20-jdk-http-client-throughput-is-ten-to-fifty-x-hotspot-CLOSED-20261012.md"
crit "hc within 2x HotSpot in BOTH modes -> retire; else keep open with the new row ratios"
crit "TreeMapRows: tmap, cicmp, hdr, eqic default < w6off; conf PASS (rmflip U+10400 is the known w6 diff)"
conf2 tm-conf "" "" Nio21TreeMapRows
conf tm-conf-w6off.jo "$W6OFF" "" Nio21TreeMapRows
for r in ciflip rmflip; do conf "tm-$r.jo" "" "" -Dnio21.rows=$r Nio21TreeMapRows; done
logrun tm-tmap-jitc CRATONVM_DBG_JITC=1 "" -Dnio21.bench=1 -Dnio21.rows=tmap Nio21TreeMapRows
count tm-tmap-jitc 'nest-unbindable-to-call' 'splice-never-run-static-exit' 'ir-splice-unbindable-surviving-call.*compare'
logrun tm-ciflip-deopt CRATONVM_DBG_DEOPT=1 "" -Dnio21.rows=ciflip Nio21TreeMapRows
count tm-ciflip-deopt 'ClassLoading'
med hh; med tm
fi

if want r20-jit-throughput-rows-throw-lock-optional-weakref-date-CLOSED-20261012.md; then
echo "## r20-jit-throughput-rows-throw-lock-optional-weakref-date-CLOSED-20261012.md"
crit "conf/check PASS both modes; record jo/HotSpot ratio per row (ite rlock optional weakref date throwlocal throwcall)"
crit "throwlocal: jo (local throw edges ON by default) << jo-lte0"
conf2 jr-conf "" "" -Dnio20.n=20000 Nio20JitRows
conf jr-conf-lte0.jo CRATONVM_JIT_IR_LOCAL_THROW_EDGES=0 "" -Dnio20.n=20000 Nio20JitRows
med jr
fi

if want r20-masked-index-loop-keeps-its-load-and-checks-CLOSED-20261012.md; then
echo "## r20-masked-index-loop-keeps-its-load-and-checks-CLOSED-20261012.md"
crit "conf PASS in default and every =0 arm, both modes; 'early (unroll candidate)' + 'pure-body' + 'fully unrolled', no 'loop side effect'"
crit "own/param near noload; sum/nested/hexmask/crcmask unchanged vs the =0 arms"
conf2 mf-conf "" "" Nio21MaskedFence conf
conf mf-conf-elide0.jo CRATONVM_JIT_IR_MASK_GUARD_ELIDE=0 "" Nio21MaskedFence conf
conf mf-conf-early0.jo CRATONVM_JIT_IR_MASK_PREDICATE_EARLY=0 "" Nio21MaskedFence conf
conf mf-conf-drop0.jo CRATONVM_JIT_IR_UNROLL_DROP_FENCED_LOAD=0 "" Nio21MaskedFence conf
conf2 mf-conf-mask0 "$MASK0" "" Nio21MaskedFence conf
logrun fr-own-unroll "CRATONVM_DBG_UNROLL=1 CRATONVM_DBG_LICM=1" "" -Dnio20.rows=own Nio20FenceRow
count fr-own-unroll 'early \(unroll candidate\)' 'pure-body' 'fully unrolled' 'loop side effect'
med mf; med fr; med jr
fi

if want r20-scoped-fold-deopts-on-a-non-null-session-CLOSED-20261012.md; then
echo "## r20-scoped-fold-deopts-on-a-non-null-session-CLOSED-20261012.md"
crit "ArenaFold conf PASS in default / ncmf0 / nckind0, both modes; ReceiverTypeChanged on segglobal/segauto = 0 by default (> 0 with ncmf0)"
conf2 af-conf "" "" Nio21ArenaFold conf
conf af-conf-ncmf0.jo CRATONVM_JIT_IR_SCOPED_NONCLOSEABLE_MODEL_FOLD=0 "" Nio21ArenaFold conf
conf af-conf-nckind0.jo CRATONVM_FFM_SESSION_NONCLOSEABLE_KIND=0 "" Nio21ArenaFold conf
logrun af-seg-deopt CRATONVM_DBG_DEOPT=1 "" -Dnio21.rows=segglobal,segauto Nio21ArenaFold rows
logrun af-seg-deopt-ncmf0 "CRATONVM_DBG_DEOPT=1 CRATONVM_JIT_IR_SCOPED_NONCLOSEABLE_MODEL_FOLD=0" "" -Dnio21.rows=segglobal,segauto Nio21ArenaFold rows
count af-seg-deopt 'ReceiverTypeChanged'
count af-seg-deopt-ncmf0 'ReceiverTypeChanged'
med af
fi

if want r20-scoped-memory-public-accessors-keep-their-typed-catch-CLOSED-20261012.md; then
echo "## r20-scoped-memory-public-accessors-keep-their-typed-catch-CLOSED-20261012.md"
crit "direct-buffer half only: BufferTyped/ArenaFold conf PASS and direct rows default << afold0 (see the directbb section)"
conf2 bt-conf "" "" Nio20BufferTyped conf
conf2 af-conf "" "" Nio21ArenaFold conf
med bt
fi

if want r20-sp-thread-current-thread-prefix-needs-a-per-vm-thread-word-FIXED-20261012.md; then
echo "## r20-sp-thread-current-thread-prefix-needs-a-per-vm-thread-word-FIXED-20261012.md"
crit "lock & tlget: default < spct0 (both modes) and within ~2x HotSpot; current-thread-inline (sp)/(ir) lines > 0 in both modes"
conf2 uv-conf "" "" Nio21UnsafeVolatile
conf uv-conf-spct0.jo CRATONVM_JIT_SP_CURRENT_THREAD_PREFIX=0 "" Nio21UnsafeVolatile
logrun uv-ct-jo CRATONVM_DBG_JITC=1 "" -Dnio21.rows=lock,tlget Nio21UnsafeVolatile
logrun uv-ct-co CRATONVM_DBG_JITC=1 "$CO" -Dnio21.rows=lock,tlget Nio21UnsafeVolatile
count uv-ct-jo 'current-thread-inline \(sp\)' 'current-thread-inline \(ir\)'
count uv-ct-co 'current-thread-inline \(sp\)' 'current-thread-inline \(ir\)'
med uv
fi

if want r20-string-pin-and-planning-gate-residual-CLOSED-20261012.md; then
echo "## r20-string-pin-and-planning-gate-residual-CLOSED-20261012.md"
crit "conf PASS in default / ssp0 / sb0, both modes; CharBufferRow conf PASS; bench rows default <= ssp0"
conf2 si-conf "" "" -Dnio20.rows=conf Nio20StringIr
conf2 si-conf-ssp0 CRATONVM_JIT_IR_STRING_SEARCH_PREFIX=0 "" -Dnio20.rows=conf Nio20StringIr
conf2 si-conf-sb0 CRATONVM_JIT_IR_SB_PREFIX=0 "" -Dnio20.rows=conf Nio20StringIr
conf2 cb-conf "" "" -Dnio20.rows=conf Nio20CharBufferRow
med si
fi

if want r20-utf8-encoder-buffer-loop-per-char-calls-CLOSED-20261012.md; then
echo "## r20-utf8-encoder-buffer-loop-per-char-calls-CLOSED-20261012.md"
crit "conf PASS with NESTED_PROFILE_TRAP=1 in both modes; buffer-mixed npt1 >5% faster than default and near buffer-ascii"
conf2 el-conf "" "" Nio21EncodeLoop conf
conf2 el-conf-npt1 CRATONVM_JIT_IR_SPLICE_NESTED_PROFILE_TRAP=1 "" Nio21EncodeLoop conf
logrun el-mixed-npt1 "CRATONVM_JIT_IR_SPLICE_NESTED_PROFILE_TRAP=1 CRATONVM_DBG_LOOP_FIELD_FORWARD=1 CRATONVM_DBG_JITC=1" "" -Dnio21.rows=buffer-mixed Nio21EncodeLoop rows
count el-mixed-npt1 'loop field forward: header bci 5' 'nest-profile-trap' 'refused'
lines el-mixed-npt1 'loop field forward: header bci 5|nest-profile-trap|refused' 16
med el
fi

if want r20-utf8-loops-residual-after-the-planner-fixes-CLOSED-20261012.md; then
echo "## r20-utf8-loops-residual-after-the-planner-fixes-CLOSED-20261012.md"
crit "no lever arm (room0 mono0 bcem0 lcp0 offp0 surr0) beats default; record jo & co vs HotSpot per row"
conf2 dl-conf "" "" Nio21DecodeLoop conf
conf2 e20-conf "" "" -Dnio20.n=2000 Nio20EncodeLoop
conf e20-conf-surr0.jo CRATONVM_JIT_IR_CHAR_SURROGATE_SCALAR=0 "" -Dnio20.n=2000 Nio20EncodeLoop
med dl; med el; med e20
[ -s "$O/ibench-r17.txt" ] && ibench r17 "$R17CP" "" || echo "  (R17 bench absent)"
fi

if want r21-buffer-relative-le-be-rows-regressed-after-nio20-w6-FIXED-20261010.md; then
echo "## r21-buffer-relative-le-be-rows-regressed-after-nio20-w6-FIXED-20261010.md"
crit "le/be: does any of spx0 / edv0 / bvo0 / exit0 move them beyond noise (both modes)? census lists none of the probe's methods => noise"
logrun bt-lebe-census CRATONVM_DBG_JITC=1 "" -Dnio20.rows=le,be,single,straight,heapput Nio20BufferTyped rows
count bt-lebe-census 'ir-exit-cost-census' 'ir-exit-cost-census.*(naming_virtuals=[1-9]|beside_dropped_values=true)'
lines bt-lebe-census 'ir-exit-cost-census.*(naming_virtuals=[1-9]|beside_dropped_values=true)' 20
count bt-lebe-census 'sp-splice-post-call-exit'
med bt
fi

if want r21-buffer-trap-miss-slows-the-heap-index-rows-FIXED-20261010.md; then
echo "## r21-buffer-trap-miss-slows-the-heap-index-rows-FIXED-20261010.md"
crit "heapget/heapput/heaplong: default <= trap0 within noise -> retire; otherwise the logs below name the route"
logrun bt-trap-jitc CRATONVM_DBG_JITC=1 "" -Dnio20.rows=heapget,heapput,heaplong Nio20BufferTyped rows
logrun bt-trap0-jitc "CRATONVM_DBG_JITC=1 CRATONVM_JIT_IR_BUFFER_TRAP_MISS=0" "" -Dnio20.rows=heapget,heapput,heaplong Nio20BufferTyped rows
count bt-trap-jitc 'ir-guard-trap-miss' 'bg-direct-call' 'DECLINED'
count bt-trap0-jitc 'ir-guard-trap-miss' 'bg-direct-call' 'DECLINED'
logrun bt-trap-deopt CRATONVM_DBG_DEOPT=1 "" -Dnio20.rows=heapget,heapput,heaplong Nio20BufferTyped rows
count bt-trap-deopt 'UncommonTrap' 'ReceiverTypeChanged' 'per-bci de-spec'
med bt
fi

if want r21-callsite-service-chain-trap-despec-grain-differs-from-the-doors-OBSOLETE-20261012.md; then
echo "## r21-callsite-service-chain-trap-despec-grain-differs-from-the-doors-OBSOLETE-20261012.md"
crit "the service's reason= lines on the flip row name ReceiverTypeChanged (not TransferToInterpreter), and a 'per-bci de-spec (trapped callee)' line follows"
crit "(0 service lines = encodeBufferLoop never entered from compiled encode: inconclusive, keep open)"
logrun cat-deopt CRATONVM_DBG_DEOPT=1 "" Nio21CharAtTrap
count cat-deopt 'reason=ReceiverTypeChanged' 'reason=TransferToInterpreter' 'per-bci de-spec \(trapped callee\)' 'MakeNotCompilable'
lines cat-deopt 'trapped callee|TransferToInterpreter' 8
fi

if want r21-charat-trap-despec-skips-the-rerun-sinks-FIXED-20261012.md; then
echo "## r21-charat-trap-despec-skips-the-rerun-sinks-FIXED-20261012.md"
crit "default: >=1 'per-bci de-spec' for encode, 0 MakeNotCompilable for it; rerun0 arm: MakeNotCompilable returns; conf PASS both modes"
conf2 cat-conf "" "" Nio21CharAtTrap
logrun cat-rerun "CRATONVM_DBG_DEOPT=1 CRATONVM_JIT_DEOPT_SINK_RESUME=0" "" Nio21CharAtTrap
logrun cat-rerun0 "CRATONVM_DBG_DEOPT=1 CRATONVM_JIT_DEOPT_SINK_RESUME=0 CRATONVM_JIT_RERUN_SINK_PER_BCI_DESPEC=0" "" Nio21CharAtTrap
for l in cat-rerun cat-rerun0; do
  echo "COUNT $l per-bci de-spec (encode) = $(grep -E 'per-bci de-spec' "$O/log/$l.log" | grep -ci encode)"
  echo "COUNT $l MakeNotCompilable (encode) = $(grep -E 'MakeNotCompilable' "$O/log/$l.log" | grep -ci encode)"
done
med cat
fi

if want r21-charat-trap-reads-a-length-predicate-class-guard-FIXED-20261012.md; then
echo "## r21-charat-trap-reads-a-length-predicate-class-guard-FIXED-20261012.md"
crit "conf PASS with the screen on and =0; buffer-ascii jo vs jo-chs0 within noise (the screen's cost)"
conf2 cat-conf "" "" Nio21CharAtTrap
conf cat-conf-chs0.jo CRATONVM_JIT_IR_LENGTH_PREDICATE_CHARAT_SCREEN=0 "" Nio21CharAtTrap
conf2 el-conf "" "" Nio21EncodeLoop conf
conf el-conf-chs0.jo CRATONVM_JIT_IR_LENGTH_PREDICATE_CHARAT_SCREEN=0 "" Nio21EncodeLoop conf
logrun el-ascii-licm "CRATONVM_DBG_LICM=1 CRATONVM_DBG_IR_COMPILES=1" "" -Dnio21.rows=buffer-ascii Nio21EncodeLoop rows
logrun el-ascii-licm-chs0 "CRATONVM_DBG_LICM=1 CRATONVM_DBG_IR_COMPILES=1 CRATONVM_JIT_IR_LENGTH_PREDICATE_CHARAT_SCREEN=0" "" -Dnio21.rows=buffer-ascii Nio21EncodeLoop rows
count el-ascii-licm 'length predicate: .*: guard n' 'miss=deopt' 'class predicate'
count el-ascii-licm-chs0 'length predicate: .*: guard n' 'miss=deopt' 'class predicate'
med el
fi

if want r21-compare-to-ci-stays-a-call-on-the-character-case-natives-CLOSED-20261012.md; then
echo "## r21-compare-to-ci-stays-a-call-on-the-character-case-natives-CLOSED-20261012.md"
crit "conf rows ci,ignorecase,tmapci,ciflip PASS with srb on and =0, both modes; 'nest-static ...compareToCI ... SPLICED' >0, no"
crit "  'static-intrinsic-keeps-its-own-fast-path' for compareToCI in an IR resolution; cicmp/tmap/hdr/tmapcmp default < srb0"
conf2 tm-ci "" "" -Dnio21.rows=ci,ignorecase,tmapci,ciflip Nio21TreeMapRows
conf2 tm-ci-srb0 CRATONVM_JIT_IR_SPLICE_SCALAR_ROW_BODY=0 "" -Dnio21.rows=ci,ignorecase,tmapci,ciflip Nio21TreeMapRows
logrun tm-cicmp-jitc CRATONVM_DBG_JITC=1 "" -Dnio21.bench=1 -Dnio21.rows=cicmp Nio21TreeMapRows
count tm-cicmp-jitc 'nest-static java/lang/StringLatin1.compareToCI.*SPLICED' 'compareToCI.*static-intrinsic-keeps-its-own-fast-path' 'splice-scalar-intrinsics' 'nest-unbindable-to-call'
lines tm-cicmp-jitc 'compareToCI' 12
med tm
fi

if want r21-compatible-http-path-doors-left-after-the-wave-2-families-CLOSED-20261012.md; then
echo "## r21-compatible-http-path-doors-left-after-the-wave-2-families-CLOSED-20261012.md"
crit "census: the --compatible top 60 on hc has no row absent from --jdk-only's except syscall bridges (EPoll, EventFD, IOUtil, SocketDispatcher) and System\$1/System\$2"
crit "price: hc/uri/conc co vs co-off5 vs jo"
if [ -s "$O/log/hc-co.json" ] && [ -s "$O/log/hc-jo.json" ]; then census_diff "$O/log/hc-co.json" "$O/log/hc-jo.json" 60
else echo "  (census absent: run section bench-hh)"; fi
med hh
fi

if want r21-decode-loop-offset-accesses-keep-their-checks-CLOSED-20261012.md; then
echo "## r21-decode-loop-offset-accesses-keep-their-checks-CLOSED-20261012.md"
crit "conf PASS default and room0, both modes; cyrillic & mixed default faster than room0 (beyond noise)"
conf2 dl-conf "" "" Nio21DecodeLoop conf
conf2 dl-conf-room0 CRATONVM_JIT_IR_BCE_ROOM_OFFSET=0 "" Nio21DecodeLoop conf
med dl
fi

if want r21-encode-loop-field-hoist-and-buffer-bce-need-a-call-free-loop-FIXED-20261012.md; then
echo "## r21-encode-loop-field-hoist-and-buffer-bce-need-a-call-free-loop-FIXED-20261012.md"
crit "EncodeLoop + OffsetSlice conf PASS in default / offp0 / bcev0 / sitedespec0; 'offset predicate ... planted' >0;"
crit "  buffer-ascii & slice rows default < offp0; <=4 charAt traps per site then a per-bci de-spec"
conf2 el-conf "" "" Nio21EncodeLoop conf
conf2 os-conf "" "" Nio21OffsetSlice conf
for c in CRATONVM_JIT_IR_OFFSET_PREDICATE=0 CRATONVM_JIT_IR_BCE_VAR_OFFSET=0 CRATONVM_JIT_IR_SEQ_CHARAT_TRAP_SITE_DESPEC=0; do
  tag=${c%%=*}; tag=${tag#CRATONVM_JIT_IR_}
  conf2 "el-conf-$tag" "$c" "" Nio21EncodeLoop conf
  conf2 "os-conf-$tag" "$c" "" Nio21OffsetSlice conf
done
logrun el-ascii-offp CRATONVM_DBG_LICM=1 "" -Dnio21.rows=buffer-ascii Nio21EncodeLoop rows
count el-ascii-offp 'offset predicate: header .* planted [1-9]' 'class predicate' 'loop field forward'
logrun el-conf-deopt CRATONVM_DBG_DEOPT=1 "" Nio21EncodeLoop conf
count el-conf-deopt 'per-bci de-spec.*encodeBufferLoop' 'ReceiverTypeChanged'
med el; med os
fi

if want r21-final-field-read-forwarding-needs-a-final-bit-in-the-field-rows-OBSOLETE-20261012.md; then
echo "## r21-final-field-read-forwarding-needs-a-final-bit-in-the-field-rows-OBSOLETE-20261012.md"
crit "BufferTyped conf PASS with ffin0 and fload1, both modes; straight default <= ffin0; 'final: hits=' forwarded > 0"
conf2 bt-conf "" "" Nio20BufferTyped conf
conf2 bt-conf-ffin0 CRATONVM_JIT_IR_FIELD_FINAL_FORWARD=0 "" Nio20BufferTyped conf
conf2 bt-conf-fload1 CRATONVM_JIT_IR_FIELD_LOAD_FORWARD=1 "" Nio20BufferTyped conf
logrun bt-straight-final CRATONVM_DBG_IR_COMPILES=1 "" -Dnio20.rows=straight Nio20BufferTyped rows
count bt-straight-final 'final: hits=' 'final: hits=[0-9]+ forwarded=[1-9]'
med bt
fi

if want r21-huc-body-stream-carrier-needs-display-and-baos-family-FIXED-20261012.md; then
echo "## r21-huc-body-stream-carrier-needs-display-and-baos-family-FIXED-20261012.md"
crit "huc,hucbaos PASS under --compatible ('hucbaos instanceof-baos false java.io.FilterOutputStream'); 0 'wrapper refused';"
crit "  the WRAPS_MINTED_CARRIER=0 arm is EXPECTED to DIFF (instanceof true); full W3Doors conf PASS both modes"
conf2 w3-conf "" "" Nio21W3Doors
conf w3-huc.co "" "$CO" -Dnio21.rows=huc,hucbaos Nio21W3Doors
conf w3-huc-wrap0.co CRATONVM_HUC_STREAMING_WRAPS_MINTED_CARRIER=0 "$CO" -Dnio21.rows=huc,hucbaos Nio21W3Doors
show w3-huc.co 'hucbaos'
logrun w3-huc-dbg CRATONVM_DBG_HUC_STREAMING=1 "$CO" -Dnio21.rows=huc,hucbaos Nio21W3Doors
count w3-huc-dbg 'wrapper refused' 'huc-streaming'
fi

if want r21-interrupt-field-and-flag-can-split-on-a-consume-race-FIXED-20261009.md; then
echo "## r21-interrupt-field-and-flag-can-split-on-a-consume-race-FIXED-20261009.md"
crit "every run prints 'intr-race rounds=2000 field-only=0 flag-only=0' = HotSpot (HotSpot itself must now finish)"
for i in 1 2 3; do conf2 "ir-race-$i" "" "" Nio21InterruptRace; done
conf ir-race-lock0.jo CRATONVM_INTERRUPT_LOCK=0 "" Nio21InterruptRace
fi

if want r21-ir-call-site-exception-edges-need-an-exceptional-call-successor-CLOSED-20261012.md; then
echo "## r21-ir-call-site-exception-edges-need-an-exceptional-call-successor-CLOSED-20261012.md"
crit "CallThrow/JitRows/LocalThrow conf PASS with edges=1 in both modes (and off); GC-stress CallThrow PASS; sites_armed > 0;"
crit "  ite/throwcall faster with edges1 and no row regresses"
conf2 ct-conf "" "" Nio21CallThrow
conf2 ct-conf-edges1 "$EDG" "" Nio21CallThrow
conf ct-conf-edges1-gcstress.jo "$EDG CRATONVM_GC_STRESS=65536" "" Nio21CallThrow
conf2 jr-conf-edges1 "$EDG" "" -Dnio20.n=20000 Nio20JitRows
conf2 lt-conf "" "" Nio21LocalThrow
conf2 lt-conf-edges1 "$EDG" "" Nio21LocalThrow
logrun jr-ite-edges "$EDG CRATONVM_DBG_JITC=1" "" -Dnio20.rows=ite,throwcall -Dnio20.n=20000 Nio20JitRows
count jr-ite-edges 'ir-call-throw sites_armed=[1-9]' 'ir-call-throw site .* NOT armed'
med jr; med ct; med lt
fi

if want r21-ir-charat-trap-deopts-a-call-whose-exception-edges-are-armed-FIXED-20261012.md; then
echo "## r21-ir-charat-trap-deopts-a-call-whose-exception-edges-are-armed-FIXED-20261012.md"
crit "CharAtTrap conf PASS with edges unset and =1, both modes; <=4 ReceiverTypeChanged at oobSum's charAt bci, then a per-bci de-spec (or none planted)"
conf2 cat-conf "" "" Nio21CharAtTrap
conf2 cat-conf-edges1 "$EDG" "" Nio21CharAtTrap
logrun cat-oob-deopt "CRATONVM_DBG_DEOPT=1 $EDG" "" Nio21CharAtTrap
echo "COUNT cat-oob-deopt oobSum ReceiverTypeChanged = $(grep -E 'ReceiverTypeChanged' "$O/log/cat-oob-deopt.log" | grep -c oobSum)"
echo "COUNT cat-oob-deopt oobSum per-bci de-spec = $(grep -E 'per-bci de-spec' "$O/log/cat-oob-deopt.log" | grep -c oobSum)"
fi

if want r21-ir-check-elim-facts-cross-an-implicit-exception-edge-FIXED-20261010.md; then
echo "## r21-ir-check-elim-facts-cross-an-implicit-exception-edge-FIXED-20261010.md"
crit "ImplicitCatch conf (fence-npe, fence-aioobe, fence-shared and the rest) PASS with both edge switches on, both modes, and with FENCED_HANDLERS=0"
conf2 ic-conf-imp1 "$IMP" "" Nio21ImplicitCatch
conf ic-conf-imp1-fh0.jo "$IMP CRATONVM_JIT_IR_IMPLICIT_EDGE_FENCED_HANDLERS=0" "" Nio21ImplicitCatch
show ic-conf-imp1.jo '^(hot|cold) fence'
fi

if want r21-ir-implicit-exceptions-inside-a-try-still-leave-the-frame-FIXED-20261012.md; then
echo "## r21-ir-implicit-exceptions-inside-a-try-still-leave-the-frame-FIXED-20261012.md"
crit "ImplicitCatch conf PASS in edges off / on / ilh0 / field0 / pad0, both modes; hotmsg PASS vs HotSpot -XX:-OmitStackTraceInFastThrow;"
crit "  census sites_armed > 0; bench fieldok/nothrow not slower with pad on"
conf2 ic-conf "" "" Nio21ImplicitCatch
conf2 ic-conf-imp1 "$IMP" "" Nio21ImplicitCatch
conf ic-conf-imp1-ilh0.jo "$IMP CRATONVM_JIT_IMPLICIT_LOCAL_HANDLERS=0" "" Nio21ImplicitCatch
conf ic-conf-imp1-field0.jo "$IMP CRATONVM_JIT_IR_IMPLICIT_FIELD_EDGES=0" "" Nio21ImplicitCatch
conf ic-conf-imp1-pad0.jo "$IMP CRATONVM_JIT_IR_IMPLICIT_PAD_FALLTHROUGH=0" "" Nio21ImplicitCatch
HSARGS="-XX:-OmitStackTraceInFastThrow" conf ic-hotmsg-imp1.jo "$IMP" "" Nio21ImplicitCatch hotmsg
logrun ic-census "$IMP CRATONVM_DBG_JITC=1" "" Nio21ImplicitCatch
count ic-census 'ir-implicit-throw sites_armed=[1-9]' 'ir-implicit-throw site .* NOT armed'
grep -E 'ir-implicit-throw site .* NOT armed' "$O/log/ic-census.log" | sed 's/.*NOT armed: //' | sort | uniq -c | sort -rn | head -n 8 | sed 's/^/    /'
med ic
fi

if want r21-ir-splice-refuses-a-callee-with-a-real-exception-handler-CLOSED-20261012.md; then
echo "## r21-ir-splice-refuses-a-callee-with-a-real-exception-handler-CLOSED-20261012.md"
crit "SpliceHandler conf PASS with the switches unset and with CALLEE_HANDLERS=1+EDGES=1, both modes; decode/encode faster with sch1"
conf2 sh-conf "" "" Nio21SpliceHandler conf
conf2 sh-conf-sch1 "$SCH" "" Nio21SpliceHandler conf
logrun sh-sch1-jitc "$SCH CRATONVM_DBG_JITC=1" "" -Dnio21.rows=decode,encode Nio21SpliceHandler rows
count sh-sch1-jitc 'ir-splice-handler' 'callee-exception-table' 'ir-splice-callee-handlers-not-covered' 'ir-splice-handler REFUSED'
lines sh-sch1-jitc 'REFUSED .*(decode|encode)|ir-splice-handler' 10
med sh
fi

if want r21-jdk-only-volatile-unsafe-refersto0-and-carrier-thread-are-generic-native-calls-FIXED-20261012.md; then
echo "## r21-jdk-only-volatile-unsafe-refersto0-and-carrier-thread-are-generic-native-calls-FIXED-20261012.md"
crit "UnsafeVolatile conf PASS default and thin0, both modes; ainc refset lfu fence refersto tlget pipe: default < thin0"
conf2 uv-conf "" "" Nio21UnsafeVolatile
conf uv-conf-thin0.jo "$THIN0" "" Nio21UnsafeVolatile
med uv
fi

if want r21-loop-field-forward-read-class-tested-fields-before-the-test-FIXED-20261010.md; then
echo "## r21-loop-field-forward-read-class-tested-fields-before-the-test-FIXED-20261010.md"
crit "EncodeLoop and BufferTyped conf PASS with CLASS_INPUTS on and =0 (R16ArraylistSplit is not in the kit)"
conf2 el-conf "" "" Nio21EncodeLoop conf
conf el-conf-ci0.jo CRATONVM_JIT_IR_LOOP_FIELD_FORWARD_CLASS_INPUTS=0 "" Nio21EncodeLoop conf
conf bt-conf-ci0.jo CRATONVM_JIT_IR_LOOP_FIELD_FORWARD_CLASS_INPUTS=0 "" Nio20BufferTyped conf
logrun el-conf-lff CRATONVM_DBG_LOOP_FIELD_FORWARD=1 "" Nio21EncodeLoop conf
count el-conf-lff 'class_tested=true' 'loop field forward: header bci'
fi

if want r21-misaligned-raw-unsafe-cas-is-process-locked-FIXED-20261010.md; then
echo "## r21-misaligned-raw-unsafe-cas-is-process-locked-FIXED-20261010.md"
crit "'race misaligned markers=250 lost=0 top=250' in every default run, both modes; conf misaligned = HotSpot; =0 arm loses at least once (teeth)"
conf2 cr-all "" "" Nio21CasRace
for i in 1 2; do conf2 "cr-race-$i" "" "" -Dnio21.rows=race Nio21CasRace; done
for i in 1 2 3; do conf "cr-race-mis0-$i.jo" CRATONVM_UNSAFE_MISALIGNED_HW_CAS=0 "" -Dnio21.rows=race Nio21CasRace; done
for l in cr-all.jo cr-all.co cr-race-1.jo cr-race-1.co cr-race-2.jo cr-race-2.co cr-race-mis0-1.jo cr-race-mis0-2.jo cr-race-mis0-3.jo; do
  show "$l" '^(race|conf) misaligned'; done
fi

if want r21-primitive-array-unsafe-cas-is-lock-based-not-hardware-FIXED-20261010.md; then
echo "## r21-primitive-array-unsafe-cas-is-lock-based-not-hardware-FIXED-20261010.md"
crit "race int-cas int-xadd long-cas aia ala: lost=0 in every run, both modes and --nojit; conf PASS; UNSAFE_ARRAY_HW_CAS=0 loses at least once"
conf2 cr-all "" "" Nio21CasRace
for i in 1 2; do conf2 "cr-race-$i" "" "" -Dnio21.rows=race Nio21CasRace; done
conf cr-race-nojit.jo "" "--nojit" -Dnio21.rows=race Nio21CasRace
for i in 1 2 3; do conf "cr-race-arr0-$i.jo" CRATONVM_UNSAFE_ARRAY_HW_CAS=0 "" -Dnio21.rows=race Nio21CasRace; done
for l in cr-race-1.jo cr-race-1.co cr-race-nojit.jo cr-race-arr0-1.jo cr-race-arr0-2.jo cr-race-arr0-3.jo; do
  show "$l" '^race (int-cas|int-xadd|long-cas|aia|ala) '; done
med cr
fi

if want r21-profile-guarded-splice-caps-hot-sites-at-the-cold-size-FIXED-20261012.md; then
echo "## r21-profile-guarded-splice-caps-hot-sites-at-the-cold-size-FIXED-20261012.md"
crit "GuardedMulti conf PASS default and multi0, both modes; 'planned' census lines + ir-guarded-hot-cap; bi/poly default < multi0"
conf2 gm-conf "" "" Nio21GuardedMulti
conf2 gm-conf-multi0 CRATONVM_JIT_IR_GUARDED_SPLICE_HOT_CAP_MULTI=0 "" Nio21GuardedMulti
logrun gm-bi-jitc CRATONVM_DBG_JITC=1 "" -Dnio21.bench=1 -Dnio21.rows=bi Nio21GuardedMulti
count gm-bi-jitc 'ir-bimorphic-census.*planned' 'ir-polymorphic-census.*planned' 'ir-guarded-hot-cap' 'second-body-refused' 'ir-splice-refused'
med gm
fi

if want r21-reference-array-unsafe-cas-is-lock-based-FIXED-20261008.md; then
echo "## r21-reference-array-unsafe-cas-is-lock-based-FIXED-20261008.md"
crit "race ref-array / ara lost=0 in every run, both modes (w6: lost=3000..12800 in both modes AND --nojit -- known open);"
crit "  REF_ARRAY_HW_CAS=0 loses at least once; conf ref-array = HotSpot under generational and G1; field-ref lost=0 with compressed oops"
conf2 cr-all "" "" Nio21CasRace
for i in 1 2; do conf2 "cr-race-$i" "" "" -Dnio21.rows=race Nio21CasRace; done
conf cr-race-nojit.jo "" "--nojit" -Dnio21.rows=race Nio21CasRace
for i in 1 2; do conf "cr-race-ref0-$i.jo" CRATONVM_UNSAFE_REF_ARRAY_HW_CAS=0 "" -Dnio21.rows=race Nio21CasRace; done
conf cr-conf-g1.jo "" "-XX:+UseG1GC" -Dnio21.rows=conf Nio21CasRace
conf cr-conf-gen.jo "" "-XX:+UseGenerationalGC" -Dnio21.rows=conf Nio21CasRace
conf cr-race-coops.jo "" "-XX:+UseGenerationalGC -XX:+UseCompressedOops" -Dnio21.rows=race Nio21CasRace
for l in cr-race-1.jo cr-race-1.co cr-race-2.jo cr-race-nojit.jo cr-race-ref0-1.jo cr-race-ref0-2.jo cr-race-coops.jo; do
  show "$l" '^race (ref-array|ara|field-ref) '; done
show cr-conf-g1.jo '^conf ref-array'
fi

if want r21-sp-box-hash-prefix-has-no-class-arm-FIXED-20261010.md; then
echo "## r21-sp-box-hash-prefix-has-no-class-arm-FIXED-20261010.md"
crit "(needs a binary with nio21 wave 8 L5) single-pass objclasshash: jo-sp/co-sp << *-sp-bhc0 and near HotSpot; conf PASS with the arm on and =0, both modes"
conf2 gc-conf-sp "$SPC" "" -Dnio20.rows=conf Nio20GetClass
conf2 gc-conf-sp-bhc0 "$SPC CRATONVM_JIT_SP_BOX_HASH_CLASS_ARM=0" "" -Dnio20.rows=conf Nio20GetClass
logrun gc-sp-objhash "$SPC CRATONVM_DBG_JITC=1" "" -Dnio20.rows=objclasshash -Dnio20.ops=2000000 Nio20GetClass
count gc-sp-objhash 'box-hash-equals-prefix .*class=Some' 'box-hash-equals-prefix' 'identity-hash-inline'
med gc
fi

if want r21-single-pass-local-handlers-arm-only-with-precise-frames-CLOSED-20261012.md; then
echo "## r21-single-pass-local-handlers-arm-only-with-precise-frames-CLOSED-20261012.md"
crit "ParamCatch & LocalThrow conf PASS in arm 0 and 1, both modes (single-pass only); reassigned/local/second/throwlocal/throwcall"
crit "  ~1250 -> ~100 ns per catch with arm 1, missOuter unchanged; no more local_handlers_disarmed retries in arm 1 than arm 0"
for arm in "$SPX" "$SPX1"; do a=${arm##*=}; [ "$arm" = "$SPX" ] && a=0
  conf2 "pc-conf-sp$a" "$arm" "" Nio21ParamCatch
  conf2 "lt-conf-sp$a" "$arm" "" Nio21LocalThrow
done
logrun pc-sp0-jitc "$SPX CRATONVM_DBG_JITC=1" "" Nio21ParamCatch
logrun pc-sp1-jitc "$SPX1 CRATONVM_DBG_JITC=1" "" Nio21ParamCatch
count pc-sp0-jitc local_handlers_disarmed
count pc-sp1-jitc local_handlers_disarmed
med pc; med lt; med jr
echo "  (flip also needs: Spring Boot sample in both modes with the switch =1, and the jit / vm unit suites with it exported)"
fi

if want r21-unsafe-natives-test-synthetic-offset-before-the-array-FIXED-20261010.md; then
echo "## r21-unsafe-natives-test-synthetic-offset-before-the-array-FIXED-20261010.md"
crit "CasRace and VolatileArray output unchanged vs the m1/w7 battery in both modes (same rows as HotSpot apart from the known ref-array/ara rows),"
crit "  and identical between the default and ARRAY_BEFORE_SYNTHETIC=0 arms"
conf2 cr-all "" "" Nio21CasRace
conf cr-all-abs0.jo CRATONVM_UNSAFE_ARRAY_BEFORE_SYNTHETIC=0 "" Nio21CasRace
conf2 va-conf "" "" Nio21VolatileArray
conf va-conf-abs0.jo CRATONVM_UNSAFE_ARRAY_BEFORE_SYNTHETIC=0 "" Nio21VolatileArray
echo "  default vs =0 (conf lines only): $(diff <(grep '^conf' "$O/conf/cr-all.jo.out") <(grep '^conf' "$O/conf/cr-all-abs0.jo.out") >/dev/null && echo same || echo DIFFER)"
fi

if want r21-unsafe-volatile-accessors-are-thin-calls-not-inline-and-getandadd-is-not-one-xadd-FIXED-20261012.md; then
echo "## r21-unsafe-volatile-accessors-are-thin-calls-not-inline-and-getandadd-is-not-one-xadd-FIXED-20261012.md"
crit "CasRace conf + VolatileArray conf PASS in default / frmw0 / cae0, both modes; field-xadd field-long-cas field-unsafe lost=0;"
crit "  ufcas ufxadd lfcas fcae cae default < frmw0/cae0 arms, near HotSpot (6/3/6/6/3 ns on the lane box)"
conf2 cr-conf "" "" -Dnio21.rows=conf Nio21CasRace
conf cr-conf-frmw0.jo CRATONVM_JIT_UNSAFE_FIELD_RMW_INLINE=0 "" -Dnio21.rows=conf Nio21CasRace
conf cr-conf-cae0.jo CRATONVM_JIT_UNSAFE_CAE_INLINE=0 "" -Dnio21.rows=conf Nio21CasRace
conf2 va-conf "" "" Nio21VolatileArray
conf va-conf-vol0.jo CRATONVM_JIT_UNSAFE_VOLATILE_INLINE=0 "" Nio21VolatileArray
conf2 cr-race-1 "" "" -Dnio21.rows=race Nio21CasRace
for l in cr-race-1.jo cr-race-1.co; do show "$l" '^race field-'; done
med cr; med va
fi

if want r21-varhandle-array-get-and-add-falls-back-to-a-plain-store-FIXED-20261010.md; then
echo "## r21-varhandle-array-get-and-add-falls-back-to-a-plain-store-FIXED-20261010.md"
crit "--compatible race rows (int-xadd, aia, ala and the rest) lost=0 in every run; STUCK_GUARD=0 arm the same (no hang = no timeout)"
crit "(the page's 'eight-incrementer rows' do not exist: every Nio21CasRace race row runs two incrementers)"
for i in 1 2 3; do conf "cr-race-$i.co" "" "$CO" -Dnio21.rows=race Nio21CasRace; done
conf cr-race-stuck0.co CRATONVM_VH_RMW_STUCK_GUARD=0 "$CO" -Dnio21.rows=race Nio21CasRace
for l in cr-race-1.co cr-race-2.co cr-race-3.co cr-race-stuck0.co; do show "$l" '^race (int-xadd|aia|ala|int-cas) '; done
fi

if want r21-zgc-reference-array-unsafe-cas-stays-lock-based-FIXED-20261008.md; then
echo "## r21-zgc-reference-array-unsafe-cas-stays-lock-based-FIXED-20261008.md"
crit "under -XX:+UseZGC (check the preflight 'ZGC available?' line): race ref-array / ara lost=0 and conf ref-array = HotSpot ZGC, both modes"
conf2 cr-zgc "" "" -XX:+UseZGC Nio21CasRace
for l in cr-zgc.jo cr-zgc.co; do show "$l" '^(race (ref-array|ara)|conf ref-array) '; done
fi

# ---- pages with no measurement that closes them (see the plan page); listed so the log is complete
for p in r20-pin-probe-still-scans-swept-nursery-holes-CLOSED-20261012.md \
         r21-direct-heap-api-reference-stores-outside-the-jit-skip-generational-satb-FIXED-20261012.md \
         r21-invoke-cache-redefinition-count-is-process-wide-FIXED-20261012.md \
         r21-stackwalker-frame-lines-blank-on-another-vms-redefinition-FIXED-20261012.md; do
  if want "$p"; then echo "## $p"; echo "  not closable by this pass (see r21-measurement-plan-w8-CLOSED-20261012.md)"; fi
done

echo
echo "[$(date -u +%H:%M:%S)] done; conformance outputs under $O/conf, logs under $O/log, benches out/bench-m8$T-*.txt"
