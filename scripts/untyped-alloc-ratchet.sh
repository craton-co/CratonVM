#!/usr/bin/env bash
# ---------------------------------------------------------------------------
# untyped-alloc-ratchet.sh — watch the fabricated-carrier surface.
#
# WHAT IT COUNTS, and why this is not the stub ratchet
# ----------------------------------------------------
# `ClassId::new(0)` is the untyped-allocation sentinel. Passed to an object
# allocator, the VM substitutes `cratonvm/synthetic/AnonymousObject$N`
# (`vm/src/vm/vm_exec.rs`). The caller reached that line because it *resolved a
# class and FAILED*, and it then hands the object out as an instance of the
# class it named.
#
# `H0-6` measured what that costs: `AnonymousObject$4` IS the `HashMap.Node`
# ({hash,key,value,next}), and real `HashMap.resize()` storing into a `Node[]`
# throws `ArrayStoreException` on it. `H0-4` §7 then measured that the table is
# fully populated and correct under `size()`/`get()` — it is ITERATION that
# breaks, because the nodes are the wrong class.
#
# `native-builtins/tests/stub_ratchet.rs` CANNOT see any of this: it counts
# REGISTRATIONS, and a substitution is not a registration. That is the gap.
#
# THREE ALLOCATOR SPELLINGS, and each earlier version of this gate saw only some
# ----------------------------------------------------------------------------
#   alloc_object      an object whose CLASS is unknown      -> AnonymousObject$N
#   alloc_object_of   the same, one call site
#   new_ref_array     a reference ARRAY whose COMPONENT class is unknown -> Object[]
#
# `new_ref_array` is a different defect wearing the same sentinel, and it is not
# cosmetic: it is why a `HashMap.table` is `[Ljava/lang/Object;` where the real
# class declares `[Ljava/util/HashMap$Node;` — the array-store half of `H0-6`.
# It gets its own column rather than being folded in, because retiring an object
# fabrication and retiring an array component type are different work.
#
# THE HISTORY OF THIS FILE IS THE ARGUMENT FOR THE GUARDS IN IT
# -------------------------------------------------------------
#   v1  matched only the bare `ClassId::new(0)` spelling and counted 84 of 203
#       — 41% — while printing a clean "ok". Lane `H16` caught it by deleting
#       two sites and watching the number not move. The majority spelling is
#       `cratonvm_types::ClassId::new(0)` (100 sites), and
#       `native-builtins/src/t27_tls.rs` — production despite the test-shaped
#       name — is the single largest producer at 35, none of them visible to v1.
#   v2  widened the pattern but left `git grep` in BASIC regex, where `(` is a
#       literal, so the new grouping matched nothing and the count moved to 89
#       instead of 203.
#   v3  dropped the `EXCL` definition in an edit and printed
#       "IMPROVED by 84 … ok — no growth" with **rc=0 while matching NOTHING**.
#   v4  folded `new_ref_array`'s second argument into WIDTHS, where it is an
#       array LENGTH and not a field count, so 16/32/64 appeared as three new
#       "carrier families" that do not exist.
#
# Every one of those printed a confident number. **A gate that measures a
# FRACTION reads as good news** — this repository has a standing note saying
# exactly that, and this file was written by a lane that had quoted it to five
# others the same day. Hence the zero-guard below, the per-function breakdown,
# and the unit separation. Prefer a permissive pattern: a new spelling is far
# likelier than a false positive.
#
# WHAT THIS IS NOT: a grep over source text, not a runtime census. A site behind
# `cfg`, a macro-generated call, or a non-literal width is invisible to it. It is
# a ratchet against DRIFT, not a population count — do not quote its number as
# the size of the problem. `CRATONVM_DBG_ANONALLOC=1` is the runtime instrument,
# and `H0-6` §8 measured that it attributes only 4.4% of events, so neither is
# authoritative alone.
#
# Usage:  scripts/untyped-alloc-ratchet.sh [--update]
# Exit:   0 ok · 1 ratchet tripped · 2 no baseline · 3 gate is broken
# ---------------------------------------------------------------------------
set -u

cd "$(dirname "$0")/.." || exit 3
BASELINE="scripts/baselines/untyped-alloc-sites.txt"

# `vm/` and `gc/` are excluded on purpose: their `ClassId::new(0)` uses are the
# allocator and the collector themselves — where the sentinel is defined and
# consumed rather than produced.
CRATES="native-builtins/src native-collections/src native-io/src native-api/src
        native-builtins-crypto/src native-builtins-security/src native-awt/src"

# Test-support paths are excluded by PATHSPEC, not by grepping the matched line
# for "test": `-h` drops the path, which would turn a path filter into a CONTENT
# filter — keeping `test_mock.rs` while dropping any real site whose line
# contains "latest"/"fastest".
#
# `t27_tls.rs` is deliberately NOT excluded. It is production source.
EXCL=':!*/tests/*  :!*test_*.rs  :!*_test.rs  :!*/test_utils.rs  :!*/test_mock.rs'

PAT='[A-Za-z0-9_]+\(([A-Za-z0-9_]+::)*ClassId::new\(0\), *[1-9][0-9]*\)'
OBJPAT='alloc_object(_of)?\(([A-Za-z0-9_]+::)*ClassId::new\(0\), *[1-9][0-9]*\)'

hits() {
  # shellcheck disable=SC2086
  git grep -hnE "$PAT" -- $CRATES $EXCL 2>/dev/null
}

breakdown() {
  # shellcheck disable=SC2086
  git grep -hoE "$PAT" -- $CRATES $EXCL 2>/dev/null \
    | sed -E 's/\(.*//' | sort | uniq -c | sort -rn \
    | awk '{printf "%s=%s ", $2, $1}'
}

SITES=$(hits | grep -c .)

# WIDTHS from the OBJECT allocators only — see v4 in the history above.
# shellcheck disable=SC2086
WIDTHS=$(git grep -hoE "$OBJPAT" -- $CRATES $EXCL 2>/dev/null \
  | sed -E 's/.*ClassId::new\(0\), *//; s/\).*//' | sort -n -u | tr '\n' ' ' | sed 's/ $//')

# A ZERO is a broken pattern, not a clean tree. v3 printed "ok" at zero.
if [ "$SITES" -eq 0 ]; then
  echo "UNTYPED-ALLOC RATCHET: pattern matched ZERO sites."
  echo "  That is a broken gate, not a clean tree — this sentinel is used"
  echo "  throughout the native crates. Check PAT / EXCL / CRATES before"
  echo "  trusting any number from this script."
  exit 3
fi

BYFN=$(breakdown)

if [ "${1:-}" = "--update" ]; then
  mkdir -p "$(dirname "$BASELINE")"
  {
    echo "# untyped-alloc ratchet baseline"
    echo "# regenerate: scripts/untyped-alloc-ratchet.sh --update"
    echo "# see docs/known-issues/jdk-only/H0-6-the-fabrication-surface-is-growing-20260820.md"
    echo "sites=$SITES"
    echo "widths=$WIDTHS"
    echo "byfn=$BYFN"
  } > "$BASELINE"
  echo "baseline written: sites=$SITES widths=[$WIDTHS] byfn=[$BYFN]"
  exit 0
fi

if [ ! -f "$BASELINE" ]; then
  echo "UNTYPED-ALLOC RATCHET: no baseline at $BASELINE"
  echo "  measured now: sites=$SITES widths=[$WIDTHS] byfn=[$BYFN]"
  echo "  run: scripts/untyped-alloc-ratchet.sh --update"
  exit 2
fi

B_SITES=$(sed -n 's/^sites=//p' "$BASELINE")
B_WIDTHS=$(sed -n 's/^widths=//p' "$BASELINE")
B_BYFN=$(sed -n 's/^byfn=//p' "$BASELINE")
bad=0

echo "UNTYPED-ALLOC RATCHET"
echo "  sites  : $SITES (baseline $B_SITES)"
echo "  widths : [$WIDTHS] (baseline [$B_WIDTHS])"
echo "  by fn  : $BYFN(baseline $B_BYFN)"

if [ "$SITES" -gt "$B_SITES" ]; then
  echo "  TRIPPED: the fabricated-carrier surface GREW by $((SITES - B_SITES)) site(s)."
  echo "    Each new site is a caller that resolved a class, failed, and will hand"
  echo "    out an AnonymousObject\$N (or a bare Object[]) as that class. If the"
  echo "    addition is deliberate, say why in the commit and re-baseline."
  bad=1
elif [ "$SITES" -lt "$B_SITES" ]; then
  echo "  IMPROVED by $((B_SITES - SITES)) site(s) — re-baseline with --update so the"
  echo "    gain is held. A ratchet only holds if clearing it is what makes it green."
fi

for w in $WIDTHS; do
  case " $B_WIDTHS " in
    *" $w "*) ;;
    *) echo "  TRIPPED: NEW carrier width $w — a shape no baseline has seen."
       echo "    Widths are families: width 4 is the HashMap.Node (H0-6 §7)."
       bad=1 ;;
  esac
done

# A new allocator SPELLING is the failure mode that produced v1 and v2. Catch it
# as a named line rather than letting it hide inside the total.
for kv in $BYFN; do
  fn="${kv%%=*}"
  case " $B_BYFN " in
    *" $fn="*) ;;
    *) echo "  TRIPPED: NEW allocator spelling '$fn' — no baseline has seen it."
       echo "    Two earlier versions of this gate were wrong by exactly this."
       bad=1 ;;
  esac
done

[ "$bad" -eq 0 ] && echo "  ok — no growth, no new widths, no new spellings."
exit "$bad"
