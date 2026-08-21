#!/usr/bin/env bash
# ---------------------------------------------------------------------------
# untyped-alloc-ratchet.sh — watch the fabricated-carrier surface.
#
# WHAT IT COUNTS, and why this is not the stub ratchet
# ----------------------------------------------------
# `alloc_object(ClassId::new(0), N)` is the untyped-allocation sentinel. The VM
# replaces it with `cratonvm/synthetic/AnonymousObject$N` (vm/src/vm/vm_exec.rs).
# The caller reached that line because it *resolved a class and FAILED*, and it
# then hands the object out as an instance of the class it named.
#
# `H0-6` measured what that costs: `AnonymousObject$4` IS the `HashMap.Node`
# ({hash,key,value,next}), and real `HashMap.resize()` storing into a `Node[]`
# throws `ArrayStoreException` on it. `H0-4` §7 then measured that the table is
# fully populated and correct under `size()`/`get()` — it is ITERATION that
# breaks, because the nodes are the wrong class.
#
# The existing `native-builtins/tests/stub_ratchet.rs` CANNOT see any of this:
# it counts REGISTRATIONS, and a substitution is not a registration. That is the
# gap this script closes.
#
# WHY A RATCHET AND NOT A ONE-OFF CENSUS
# --------------------------------------
# `H0-6` §3 measured the surface at 49 sites on 2026-08-12 and 84 on 2026-08-20
# — one method, two revisions, +71% in eight days — while the plan of record is
# to migrate these producers to real construction. The plan is written against a
# fixed target and the target is not fixed. A migration with no ratchet loses.
#
# TWO COLUMNS, deliberately, and they answer different questions:
#   * SITES  — how much surface exists. Growth means new fabrication.
#   * WIDTHS — which shapes. A NEW width is a new carrier family and is worth a
#              human look even when the site count falls (a refactor can trade
#              three width-4 sites for one width-9 site and look like progress).
#
# WHAT THIS IS NOT: it is a grep over source text, not a runtime census. A site
# behind `cfg`, a macro-generated call, or a non-literal width is invisible to
# it. It is a ratchet against DRIFT, not a population count — do not quote its
# number as the size of the problem. `CRATONVM_DBG_ANONALLOC=1` is the runtime
# instrument, and `H0-6` §8 measured that it attributes only 4.4% of events, so
# neither one is authoritative on its own.
#
# Usage:  scripts/untyped-alloc-ratchet.sh [--update]
# Exit:   0 ok · 1 ratchet tripped · 2 no baseline (run --update)
# ---------------------------------------------------------------------------
set -u

cd "$(dirname "$0")/.." || exit 3
BASELINE="scripts/baselines/untyped-alloc-sites.txt"

# The crates that produce carriers handed to real JDK bytecode. `vm/` and `gc/`
# are EXCLUDED on purpose: their `ClassId::new(0)` uses are the allocator and
# the collector themselves, which is where the sentinel is defined and consumed
# rather than produced. Test paths are excluded because a fixture fabricating a
# carrier is the fixture's business.
CRATES="native-builtins/src native-collections/src native-io/src native-api/src
        native-builtins-crypto/src native-builtins-security/src native-awt/src"

PAT='alloc_object(ClassId::new(0), *[1-9][0-9]*)'

# Test-support paths are excluded by PATHSPEC, not by grepping the matched line
# for "test". That distinction is not pedantic: `-h` drops the path, which turns
# a path filter into a CONTENT filter, and a content filter both keeps
# `test_mock.rs` (3 real exclusions here) and would silently drop any legitimate
# site whose line happens to contain the substring — `latest`, `fastest`, a
# `// tested by` comment. Caught by cross-checking two methods against one
# revision and getting 84 vs 87.
EXCL=':!*/tests/*  :!*test_*.rs  :!*_test.rs  :!*/test_utils.rs  :!*/test_mock.rs'

hits() {
  # shellcheck disable=SC2086
  git grep -hn "$PAT" -- $CRATES $EXCL 2>/dev/null
}

SITES=$(hits | grep -c . )
WIDTHS=$(hits | sed 's/.*ClassId::new(0), *//; s/).*//' | sort -n -u | tr '\n' ' ' | sed 's/ $//')

if [ "${1:-}" = "--update" ]; then
  mkdir -p "$(dirname "$BASELINE")"
  {
    echo "# untyped-alloc ratchet baseline"
    echo "# regenerate: scripts/untyped-alloc-ratchet.sh --update"
    echo "# see docs/known-issues/jdk-only/H0-6-the-fabrication-surface-is-growing-20260820.md"
    echo "sites=$SITES"
    echo "widths=$WIDTHS"
  } > "$BASELINE"
  echo "baseline written: sites=$SITES widths=[$WIDTHS]"
  exit 0
fi

if [ ! -f "$BASELINE" ]; then
  echo "UNTYPED-ALLOC RATCHET: no baseline at $BASELINE"
  echo "  measured now: sites=$SITES widths=[$WIDTHS]"
  echo "  run: scripts/untyped-alloc-ratchet.sh --update"
  exit 2
fi

B_SITES=$(sed -n 's/^sites=//p' "$BASELINE")
B_WIDTHS=$(sed -n 's/^widths=//p' "$BASELINE")
bad=0

echo "UNTYPED-ALLOC RATCHET"
echo "  sites  : $SITES (baseline $B_SITES)"
echo "  widths : [$WIDTHS] (baseline [$B_WIDTHS])"

if [ "$SITES" -gt "$B_SITES" ]; then
  echo "  TRIPPED: the fabricated-carrier surface GREW by $((SITES - B_SITES)) site(s)."
  echo "    Each new site is a native that resolved a class, failed, and will hand"
  echo "    out an AnonymousObject\$N as that class. If the addition is deliberate,"
  echo "    say why in the commit and re-baseline with --update."
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

[ "$bad" -eq 0 ] && echo "  ok — no growth, no new widths."
exit "$bad"
