#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
# Copyright 2024-2026 Craton Software Company
#
# =============================================================================
# pair-ab.sh -- per-CLASS alternating A/B for one CratonVM env lever, on a
# fork-per-class JUnit suite (netty, hibernate-reactive).
#
# WHY THIS EXISTS
#
# The suite runners are fork-per-class, so an A/B done the obvious way runs the
# whole suite with the lever on, then the whole suite with it off. On a SHARED
# host that cannot produce a timing number, and the failure is structural
# rather than bad luck: the two arms are ~17 minutes apart, and the box drifts
# more over 17 minutes than any JIT lever moves.
#
# Measured on 2026-09-06, netty, 200 classes, three arms (on / off / on again):
#
#     on   977 s   sum_class_ms 2,279,174
#     off 1061 s   sum_class_ms 2,374,963
#     on  1083 s   sum_class_ms 2,693,458
#
# The two IDENTICAL arms are 10.8% apart on wall and 18% apart on class-ms,
# both LARGER than the 7.9% / 4.0% the on-vs-off comparison showed. Repeating a
# sequential design does not fix this -- repeats just sample the drift again.
#
# WHAT THIS DOES INSTEAD
#
# The two arms for a given class run BACK TO BACK, so they see the same host.
# Each class is measured as an ABBA block:
#
#     A  B  B  A        (even class index)
#     B  A  A  B        (odd class index)
#
# ABBA is chosen over alternating AB because it cancels LINEAR drift exactly:
# the mean timestamp of the two A runs equals the mean timestamp of the two B
# runs, so a host that is steadily getting busier contributes equally to both.
# Flipping to BAAB on alternate classes cancels any residual asymmetry across
# the class list.
#
# The ABBA block also buys the thing the old shape could not have at any
# repetition count: a WITHIN-CLASS NOISE FLOOR. |A1-A2| and |B1-B2| are two
# runs of the SAME configuration, so they measure the host, not the lever. This
# script refuses to report an effect smaller than that floor -- see VERDICT
# below. A harness that cannot say "unmeasurable" will eventually say something
# false instead.
#
# The two floors are deliberately asymmetric. In ABBA the B runs are ADJACENT
# (positions 2 and 3) while the A runs are SEPARATED by them (positions 1 and
# 4), so |B1-B2| understates the noise a separated pair would see and |A1-A2|
# overstates it. The reported floor is max(A,B): the pessimistic one, because
# the failure this harness exists to prevent is claiming an effect that is
# really drift.
#
# SAME-WORK GATE
#
# A pair is only counted when all four runs report identical
# found/ok/failed/skipped. Two runs that executed different numbers of tests
# have incomparable `ms=`, and a flaky class silently contributes a timing
# difference that is really a work difference. Classes with ok=0 (NOTESTS),
# any failure, or any abort are dropped for the same reason.
#
# Runs are strictly SEQUENTIAL (no shards). Sharded forks compete with each
# other, so two members of a pair would see different contention -- which is
# the very thing this design exists to remove.
#
# USAGE
#   pair-ab.sh --list <file> [options]
#
#   --lever <NAME>     env var to flip     (default CRATONVM_JIT_IR_INLINE)
#   --on <VAL>         "A" value           (default 1)
#   --off <VAL>        "B" value           (default 0)
#   --list <file>      class list, one per line
#   --count <N>        first N classes     (default 0 = all)
#   --start <IDX>      0-based start index (default 0)
#   --min-ms <MS>      drop classes faster than this (default 400); below it
#                      VM startup dominates and the JIT barely runs
#   --timeout <SEC>    per-run cap         (default 180)
#   --out <dir>        results dir         (default ./pair-ab-runs/<ts>)
#   --runner-dir <d>   suite runner dir holding common.args / CratonRunner
#   --bin <path>       cratonvm binary
#   --dry-run          print the fork command for the first class and exit
# =============================================================================
set -uo pipefail

RUNNER_DIR="${RUNNER_DIR:-/data/cratonvm/apps/netty-suite-runner}"
NETTY_SRC="${NETTY_SRC:-/data/cratonvm/apps/netty}"
CV_BIN="${CV_BIN:-/data/cratonvm/target/release/cratonvm}"
JDK="${JDK:-/data/toolchain/jdk-25}"
CV_XMX="${CV_XMX:-1500m}"
RUNNER_CLASS="CratonRunner"

LEVER="CRATONVM_JIT_IR_INLINE"
ON_VAL="1"
OFF_VAL="0"
LIST=""
COUNT=0
START=0
MIN_MS=400
TIMEOUT=180
OUT=""
DRY=0

while [ $# -gt 0 ]; do
  case "$1" in
    --lever)      LEVER="$2"; shift 2;;
    --on)         ON_VAL="$2"; shift 2;;
    --off)        OFF_VAL="$2"; shift 2;;
    --list)       LIST="$2"; shift 2;;
    --count)      COUNT="$2"; shift 2;;
    --start)      START="$2"; shift 2;;
    --min-ms)     MIN_MS="$2"; shift 2;;
    --timeout)    TIMEOUT="$2"; shift 2;;
    --out)        OUT="$2"; shift 2;;
    --runner-dir) RUNNER_DIR="$2"; shift 2;;
    --bin)        CV_BIN="$2"; shift 2;;
    --dry-run)    DRY=1; shift;;
    -h|--help)    sed -n '2,80p' "$0"; exit 0;;
    *) echo "unknown option: $1" >&2; exit 2;;
  esac
done

COMMON="$RUNNER_DIR/common.args"
MODSCOPE="$RUNNER_DIR/module-scoped-classes.tsv"
MODARGS_DIR="$RUNNER_DIR/module-args"

[ -n "$LIST" ] || { echo "ERROR: --list is required" >&2; exit 2; }
[ -f "$LIST" ] || { echo "ERROR: list not found: $LIST" >&2; exit 2; }
[ -f "$CV_BIN" ] || { echo "ERROR: binary not found: $CV_BIN" >&2; exit 2; }
[ -f "$COMMON" ] || { echo "ERROR: common.args not found: $COMMON" >&2; exit 2; }

[ -n "$OUT" ] || OUT="$PWD/pair-ab-runs/$(date +%Y%m%d-%H%M%S)"
mkdir -p "$OUT"
TSV="$OUT/pairs.tsv"
RAW="$OUT/raw.log"

# --- module-scoped classes (same table the suite runner uses) ---------------
declare -A CLASS_WORKDIR=()
declare -A CLASS_ARGFILE=()
if [ -f "$MODSCOPE" ]; then
  while IFS=$'\t' read -r cls mod art _rest; do
    case "$cls" in ''|\#*) continue;; esac
    [ -n "${art:-}" ] || continue
    if [ -f "$MODARGS_DIR/$art.args" ] && [ -d "$NETTY_SRC/$mod" ]; then
      CLASS_WORKDIR["$cls"]="$NETTY_SRC/$mod"
      CLASS_ARGFILE["$cls"]="$MODARGS_DIR/$art.args"
    fi
  done < "$MODSCOPE"
fi

# --- one forked run --------------------------------------------------------
# Emits "found ok failed skipped ms" on stdout, or nothing when the class did
# not produce an @@RESULT line (crash, hang, timeout).
run_one() {
  local cls="$1" val="$2"
  local args="${CLASS_ARGFILE[$cls]:-$COMMON}"
  local dir="${CLASS_WORKDIR[$cls]:-}"
  local tmp; tmp=$(mktemp)
  if [ -n "$dir" ]; then
    ( cd "$dir" && env "$LEVER=$val" CRATONVM_DISABLE_DEFAULT_WATCHDOG=1 \
        timeout "$TIMEOUT" "$CV_BIN" --java-home "$JDK" --Xmx "$CV_XMX" \
        @"$args" -Dcraton.batch=1 "$RUNNER_CLASS" "$cls" ) >"$tmp" 2>>"$RAW"
  else
    env "$LEVER=$val" CRATONVM_DISABLE_DEFAULT_WATCHDOG=1 \
      timeout "$TIMEOUT" "$CV_BIN" --java-home "$JDK" --Xmx "$CV_XMX" \
      @"$args" -Dcraton.batch=1 "$RUNNER_CLASS" "$cls" >"$tmp" 2>>"$RAW"
  fi
  local line; line=$(grep '^@@RESULT ' "$tmp" | head -1)
  cat "$tmp" >> "$RAW"; rm -f "$tmp"
  [ -n "$line" ] || return 0
  local f o fa sk ab ms
  f=$(printf '%s' "$line" | grep -o 'found=[0-9]*'   | cut -d= -f2)
  o=$(printf '%s' "$line" | grep -o 'ok=[0-9]*'      | cut -d= -f2)
  fa=$(printf '%s' "$line"| grep -o 'failed=[0-9]*'  | cut -d= -f2)
  sk=$(printf '%s' "$line"| grep -o 'skipped=[0-9]*' | cut -d= -f2)
  ms=$(printf '%s' "$line"| grep -o 'ms=[0-9]*'      | cut -d= -f2)
  ab=$(printf '%s' "$line"| grep -o 'aborted=[0-9]*' | cut -d= -f2)
  printf '%s %s %s %s %s %s\n' "${f:-0}" "${o:-0}" "${fa:-0}" "${sk:-0}" "${ab:-0}" "${ms:-0}"
}

# Render tenths-of-a-percent as a signed decimal. Bash integer division
# truncates TOWARD ZERO, so the naive `n/10 . n%10` split renders -42 as
# "-4.8" rather than "-4.2"; sign and magnitude have to be split first.
fmt_tenths() {
  local v="$1" sign=""
  if [ "$v" -lt 0 ]; then sign="-"; v=$(( -v )); fi
  printf '%s%d.%d' "$sign" $(( v / 10 )) $(( v % 10 ))
}

# --- class slice -----------------------------------------------------------
mapfile -t ALL < <(grep -vE '^[[:space:]]*(#|$)' "$LIST")
if [ "$COUNT" -gt 0 ]; then
  CLASSES=("${ALL[@]:$START:$COUNT}")
else
  CLASSES=("${ALL[@]:$START}")
fi
[ "${#CLASSES[@]}" -gt 0 ] || { echo "ERROR: no classes selected" >&2; exit 2; }

if [ "$DRY" = 1 ]; then
  c="${CLASSES[0]}"
  echo "lever      : $LEVER  A=$ON_VAL  B=$OFF_VAL"
  echo "classes    : ${#CLASSES[@]} (4 runs each = $(( ${#CLASSES[@]} * 4 )) forks)"
  echo "first class: $c"
  echo "argfile    : ${CLASS_ARGFILE[$c]:-$COMMON}"
  echo "workdir    : ${CLASS_WORKDIR[$c]:-<suite root>}"
  echo "command    : env $LEVER=$ON_VAL CRATONVM_DISABLE_DEFAULT_WATCHDOG=1 \\"
  echo "               timeout $TIMEOUT $CV_BIN --java-home $JDK --Xmx $CV_XMX \\"
  echo "               @${CLASS_ARGFILE[$c]:-$COMMON} -Dcraton.batch=1 $RUNNER_CLASS $c"
  exit 0
fi

printf 'class\ta1\ta2\tb1\tb2\ta_mean\tb_mean\tdelta_pct\tnoise_pct\n' > "$TSV"
echo "pair-ab: lever=$LEVER A=$ON_VAL B=$OFF_VAL classes=${#CLASSES[@]} timeout=${TIMEOUT}s min_ms=$MIN_MS"
echo "pair-ab: ABBA per class, sequential, out=$OUT"

idx=0; kept=0; dropped=0
for cls in "${CLASSES[@]}"; do
  # ABBA on even classes, BAAB on odd: the A/B asymmetry inside one block is
  # cancelled by the block on the next class.
  if [ $(( idx % 2 )) -eq 0 ]; then seq_vals=("$ON_VAL" "$OFF_VAL" "$OFF_VAL" "$ON_VAL"); seq_tag=(a b b a)
  else                              seq_vals=("$OFF_VAL" "$ON_VAL" "$ON_VAL" "$OFF_VAL"); seq_tag=(b a a b); fi
  idx=$((idx+1))

  a1=""; a2=""; b1=""; b2=""; sig=""; bad=0
  for i in 0 1 2 3; do
    out=$(run_one "$cls" "${seq_vals[$i]}")
    if [ -z "$out" ]; then bad=1; break; fi
    read -r f o fa sk ab ms <<< "$out"
    # Same-work gate: every run of this class must have executed the same
    # tests, or the `ms` values are not comparable.
    this="$f/$o/$fa/$sk/$ab"
    if [ -z "$sig" ]; then sig="$this"; elif [ "$this" != "$sig" ]; then bad=2; break; fi
    if [ "$o" -eq 0 ] || [ "$fa" -ne 0 ] || [ "$ab" -ne 0 ]; then bad=3; break; fi
    case "${seq_tag[$i]}" in
      a) if [ -z "$a1" ]; then a1=$ms; else a2=$ms; fi;;
      b) if [ -z "$b1" ]; then b1=$ms; else b2=$ms; fi;;
    esac
  done

  if [ "$bad" != 0 ] || [ -z "$a1" ] || [ -z "$a2" ] || [ -z "$b1" ] || [ -z "$b2" ]; then
    dropped=$((dropped+1))
    echo "DROP $cls reason=$bad" >> "$OUT/dropped.txt"
    continue
  fi
  am=$(( (a1 + a2) / 2 )); bm=$(( (b1 + b2) / 2 ))
  if [ "$am" -lt "$MIN_MS" ] || [ "$bm" -lt "$MIN_MS" ]; then
    dropped=$((dropped+1)); echo "DROP $cls reason=too-fast ${am}/${bm}ms" >> "$OUT/dropped.txt"; continue
  fi
  # delta: how much slower B is than A, in tenths of a percent.
  delta=$(( (bm - am) * 1000 / am ))
  da=$(( a1 > a2 ? a1 - a2 : a2 - a1 ))
  db=$(( b1 > b2 ? b1 - b2 : b2 - b1 ))
  na=$(( da * 1000 / am )); nb=$(( db * 1000 / bm ))
  noise=$(( na > nb ? na : nb ))
  printf '%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\n' \
     "$cls" "$a1" "$a2" "$b1" "$b2" "$am" "$bm" "$delta" "$noise" >> "$TSV"
  kept=$((kept+1))
  echo "[$kept] $cls A=${am}ms B=${bm}ms delta=$(fmt_tenths "$delta")% noise=$(fmt_tenths "$noise")%"
done

# --- verdict ---------------------------------------------------------------
{
echo "=========================================================="
echo "pair-ab summary   lever=$LEVER  A=$ON_VAL  B=$OFF_VAL"
echo "classes kept=$kept dropped=$dropped"
[ "$kept" -gt 0 ] || { echo "no usable pairs"; exit 0; }
awk -F'\t' 'NR>1{
    d[n]=$8; s[n]=$9; w[n] = ($6 < $7) ? 1 : 0; n++
    if ($6 < $7) wins++
    ad8 = ($8 < 0) ? -$8 : $8
    if (ad8 > $9) { cn++; if ($6 < $7) cw++ }
    sd += $8; ss += $9
  }
  END{
    asort_d(d,n); asort_d(s,n)
    med_d = med(d,n); med_s = med(s,n)
    printf "A faster than B in %d of %d classes  (fair coin under no effect)\n", wins, n
    printf "median per-class delta : %+.1f%%   (B slower than A by this much)\n", med_d/10
    printf "median within-arm noise: %.1f%%   (SAME config, two runs)\n", med_s/10
    if (cn > 0)
      printf "  of the %d classes whose own delta beats their own noise: A faster in %d\n", cn, cw
    else
      printf "  no class had a delta larger than its own within-arm noise\n"
    printf "mean   per-class delta : %+.1f%%\n", sd/n/10
    printf "----------------------------------------------------------\n"
    if (med_d < 0) ad = -med_d; else ad = med_d
    # The SIGN TEST is a separate question from the effect size: a small effect
    # that survives averaging over many classes is what a per-class harness is
    # FOR, and comparing the median effect to the median noise alone throws
    # that away.
    #
    # But a significant z over ONE set of classes is NOT a result. Measured
    # 2026-09-07 on hibernate, same binary and lever: classes 0-39 gave
    # z=+2.47 and classes 40-119 gave z=-2.49. Pooled, 54 of 115 -- a coin.
    # Classes carry their own systematic differences (how much of the run is
    # JIT-visible at all), so slicing a NULL effect can hand you significance
    # in either direction. Hence the SPLIT-HALF check below: this run scores
    # its own two halves and says so when they disagree.
    z = (n > 0) ? (2*wins - n) / sqrt(n) : 0
    az = (z < 0) ? -z : z
    printf "sign test on the paired count: z = %+.2f%s\n", z,
           (az >= 2 ? "  (consistent, p < 0.05)" : "  (a coin)")
    # SPLIT-HALF: score the two halves of this run separately. A direction
    # that is real shows up in BOTH; one that is an artifact of which
    # classes were sampled flips. This is the check that would have caught
    # the 2026-09-07 hibernate reversal inside one run, not a day later.
    h = int(n/2)
    if (h >= 8) {
      w1 = 0; for (i = 0; i < h; i++) w1 += w[i]
      w2 = 0; for (i = h; i < n; i++) w2 += w[i]
      n1 = h; n2 = n - h
      z1 = (2*w1 - n1) / sqrt(n1)
      z2 = (2*w2 - n2) / sqrt(n2)
      printf "split-half   : first %d classes z = %+.2f | last %d classes z = %+.2f\n", n1, z1, n2, z2
      if (!((z1 >= 0 && z2 >= 0) || (z1 < 0 && z2 < 0))) {
        split_warn = 1
        printf "  ** THE HALVES DISAGREE IN SIGN. What this run measured is not\n"
        printf "     stable across WHICH classes were sampled. Do not report a\n"
        printf "     direction from it; take a larger or different sample.\n"
      }
    } else {
      printf "split-half   : n=%d is too small to split (need 16+)\n", n
    }
    printf "----------------------------------------------------------\n"
    if (ad <= med_s && az < 2) {
      printf "VERDICT: UNMEASURABLE. The effect (%.1f%%) is not larger than the\n", ad/10
      printf "         noise floor (%.1f%%) taken from two runs of the SAME\n", med_s/10
      printf "         configuration, and the paired count is a coin. Report no\n"
      printf "         throughput number from this run.\n"
    } else if (ad <= med_s) {
      if (split_warn) {
        printf "VERDICT: UNMEASURABLE (SPLIT-HALF DISAGREEMENT). A wins %d of %d\n", wins, n
        printf "         overall, but the two halves of this run point OPPOSITE ways.\n"
        printf "         The count is a property of WHICH classes were sampled, not of\n"
        printf "         the lever. Report no direction. See the split-half line above.\n"
      } else {
      printf "VERDICT: SMALL BUT CONSISTENT. Each class is noise-dominated"\
             "  (%.1f%% effect against a %.1f%% floor), but A wins %d of %d,\n",
             ad/10, med_s/10, wins, n
      printf "         which a fair coin does not do. Report the DIRECTION and the\n"
      printf "         paired count; the per-class magnitude is not resolved.\n"
      printf "         NOT A RESULT YET -- confirm on a DISJOINT set of classes\n"
      printf "         (--start past this run) before believing the direction.\n"
      printf "         Measured 2026-09-07: hibernate classes 0-39 gave z=+2.47,\n"
      printf "         classes 40-119 gave z=-2.49, SAME binary and lever. Pooled,\n"
      printf "         54 of 115 -- a coin. A significant count over ONE sample of\n"
      printf "         classes is a reason to take a SECOND sample, not a result.\n"
      }
    } else {
      printf "VERDICT: effect %.1f%% exceeds the %.1f%% same-config noise floor.\n", ad/10, med_s/10
      printf "         Read the paired count above as the primary statistic.\n"
    }
  }
  function med(arr, m) { return (m % 2) ? arr[int(m/2)] : (arr[m/2-1]+arr[m/2])/2 }
  function asort_d(arr, m,   i,j,t) {
    for (i=1;i<m;i++) { t=arr[i]; for (j=i-1;j>=0 && arr[j]>t;j--) arr[j+1]=arr[j]; arr[j+1]=t }
  }' "$TSV"
echo "=========================================================="
echo "per-class table: $TSV"
} | tee "$OUT/SUMMARY.txt"
