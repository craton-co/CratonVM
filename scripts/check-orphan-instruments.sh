#!/usr/bin/env bash
# ---------------------------------------------------------------------------
# check-orphan-instruments.sh — an instrument nobody reads cannot warn anyone.
# VERSION 1, written 2026-09-21 in round 10 (lane `diagread`).
#
# WHY THIS GATE EXISTS, and why it is a ratchet rather than a bug report
# ---------------------------------------------------------------------
# Round 10 found the same defect five times, in five unrelated files, by five
# lanes that were not looking for each other's work:
#
#   * `jit::exec_memory::unregister_range` — a withdrawal entry point with NO
#     caller. Every implicit-null recovery entry outlived its buffer.
#   * `metrics::osr_compile_declined` — a metric row that nothing ever
#     incremented, so it read `0` on every run that has ever existed.
#   * `metrics::record_osr_event` — silently no-ops on a name that is not in
#     `OSR_EVENTS`, so a call site with a typo'd literal reports the same zero
#     as a site that never fired.
#   * `code_cache_lifecycle::record_allocation_failure` / `record_capacity_bytes`
#     / `record_free_space` — three gauges with no feeder.
#   * `implicit_null::stale_duplicates_retired` / `unlinked_chain_nodes` /
#     `base_mismatches` — three "should read zero forever" counters with no
#     reader outside `#[cfg(test)]`.
#
# One shape, and the reason it is dangerous is always the same sentence:
# **zero is indistinguishable from "this never happened".** Every other kind of
# bug in this tree produces a wrong answer, and a wrong answer has a test. An
# instrument with no wiring produces a CORRECT-LOOKING answer forever, and the
# person reading it concludes the hazard it watches has not occurred.
#
# Five instances in one round is the point at which hunting instance number six
# by hand stops paying. This gate is the mechanical version of that hunt. It
# does not fix anything and it does not claim the tree is clean; it claims that
# the population of orphaned instruments does not GROW.
#
# WHAT IT LOOKS FOR — three checks, each derived from one of the five finds
# ------------------------------------------------------------------------
# C1  REPORTING ENTRY POINTS.  `pub fn record_*` / `pub fn note_*` whose body
#     touches an atomic, with no call outside the file that defines it.
#     (`record_allocation_failure`, `record_free_space`, `record_capacity_bytes`.)
#
# C2  COUNTER GETTERS.  `pub fn NAME()` — zero arguments, no `&self` — returning
#     an integer, a tuple of integers, an `Option<int>` or a `Vec<(..)>`, whose
#     body touches an atomic, with no call outside the file that defines it.
#     (`stale_duplicates_retired`, `unlinked_chain_nodes`, `base_mismatches`.)
#
#     The zero-argument restriction is doing real work and is not an accident of
#     convenience. A zero-argument public function that loads an atomic and
#     returns an integer is, in a VM, overwhelmingly a read of a process-global
#     counter. Dropping the restriction (allowing `&self`) triples the candidate
#     population with accessor methods and inflates the allowlist to the point
#     where the gate is mostly allowlist — which is a gate that catches nothing.
#
# C3  METRIC ROWS.  A string literal inside a `*_EVENTS: [&str; N]` table whose
#     only occurrence in the whole workspace is the table row itself. That is
#     the `osr_compile_declined` shape exactly: the row exists, the counter
#     exists, the report prints it, and `record_*_event("...")` is never called
#     with that literal — and `record_*_event` ignores an unknown name by
#     design, so a typo'd feeder is the same reading as no feeder.
#
# "NO CALLER" IS DEFINED AS "NO CALLER IN ANOTHER FILE", and that is deliberate
# ----------------------------------------------------------------------------
# The rule asked for is "no caller outside its own definition, its wrapper, and
# its `#[cfg(test)]` block". Tracking `#[cfg(test)] mod` blocks precisely needs
# the column-0 block scanner in `untyped-alloc-ratchet.sh`, and every one of the
# five finds above was a function whose only references were IN ITS OWN FILE —
# the definition, the module prose, and a same-file test. So this gate uses the
# cruder rule that subsumes all five: a reference in the DEFINING FILE is never
# a caller, and `**/tests/**` is not searched for callers at all.
#
# That is a fail-OPEN simplification in one direction (see L2) and it buys a
# rule simple enough that the frozen allowlist below could be produced by hand
# with the same greps, which mattered because the author of this script was not
# permitted to execute it.
#
# THE LIMITS, stated here because a grep-based checker that does not state them
# reads as a proof
# ----------------------------------------------------------------------------
# L1  IT CANNOT SEE DYNAMIC DISPATCH. A counter read through a trait object,
#     a function pointer, or a `dyn Fn` stored in a table is invisible. This
#     gate proves "no textual call site", never "no caller".
# L2  IT CANNOT SEE A CROSS-FILE TEST-ONLY CALLER. A production instrument
#     called only from `foo/tests/bar.rs` is not searched at all, so it IS
#     flagged (correct). But one called only from a `#[cfg(test)] mod` in a
#     DIFFERENT production file counts as called, and is NOT flagged. That is
#     the one case the simplification above loses.
# L3  IT CANNOT SEE A NAME BUILT AT RUNTIME. `record_event(&format!("osr_{k}"))`
#     defeats C3 completely, and C3's whole premise is that the row names are
#     literals. FFI and macro-generated calls are invisible for the same reason.
# L4  IT CANNOT SEE A CALL THAT IS COMPILED OUT. A call behind `#[cfg(unix)]`
#     on a Windows tree, or behind a feature flag nothing enables, counts as a
#     caller here and is not one at runtime. C3 caught `osr_compile_declined`
#     precisely because nobody had written the call at all; a call written and
#     never reached is a DIFFERENT bug this gate does not address.
# L5  IT PROVES NOTHING ABOUT WHETHER A LIVE CALLER EVER FIRES. `unregister_range`
#     with a caller that is never reached reads exactly like one that is. That
#     needs a run, not a grep. This gate is the cheap half.
# L6  C3 TREATS ANY `TABLE[i]` REFERENCE AS A FEEDER, whether it increments the
#     row or merely names it for a report. `DEOPT_STASH_EVENTS` is exactly the
#     second kind — a PULL table whose getter names both rows by index — so C3
#     is structurally unable to say anything about it. Without that arm C3 is
#     worse, not better: a literal-only rule called four of this tree's 37 rows
#     unfed and every one of the four was wrong, because `SCHEDULING_EVENTS` is
#     fed entirely by index.
# L7  THE ALLOWLIST IS A DEBT REGISTER, NOT A LIST OF ACCEPTABLE THINGS. 112
#     entries were frozen on 2026-09-21 because a gate that is red on arrival
#     gets disabled, not fixed (99 as of 2026-09-22 -- the file is the count,
#     not this sentence). Several of them are probably the same defect as the
#     five above. Do not read an entry as "reviewed and fine".
#
# L8  A SAME-FILE PRODUCTION READER LOOKS EXACTLY LIKE NO READER, and unlike
#     L1-L7 this one is a FALSE POSITIVE rather than a miss. `g1_evac_copy_span`
#     was reported by version 1's first real run and filed as a defect
#     (`docs/internal/fixed-bugs/r10-gate-two-g1-censuses-have-no-reader-FIXED-20260922.md`),
#     and it was not one: `g1::g1_evac_worker_census_report` reads it, in the
#     same file, and that report reaches a shipped binary through
#     `gc_metrics::collector_decision_report` and `vm-cli`. The defining-file
#     rule above cannot tell that apart from the five founding finds, whose
#     only same-file reference was a `#[cfg(test)]` test -- and loosening the
#     rule to "any same-file call counts" is not available: measured on this
#     tree, 2026-09-22, it would rescue more than sixty of the 103 orphans,
#     almost all of them through exactly such a test.
#
#     So the escape hatch is NARROW, OPT-IN and CHECKED -- see "THE read-by
#     ANNOTATION" below. It is not a way to silence the gate; it is a way to
#     name the reader, and the gate verifies the name.
#
# THE `read-by` ANNOTATION
# ------------------------
# A line of the form
#
#     // orphan-gate: read-by <FN>
#
# anywhere in the DEFINING FILE exempts the instrument it names -- but only
# when BOTH of these hold, re-checked on every run:
#
#   * `<FN>` is defined in that same file as a top-level `fn`, and `<FN>`'s own
#     body contains a call to the instrument (scanned from its signature to the
#     first column-0 `}`, the same block rule `untyped-alloc-ratchet.sh` uses);
#   * `<FN>` is itself seen as CALLED FROM ANOTHER FILE by pass 2 -- i.e. the
#     reader chain actually leaves the file.
#
# Both halves are load-bearing. Without the first the annotation is an
# unchecked claim; without the second an orphan could be exempted by another
# orphan, which is how a dead report launders a dead counter. If either stops
# holding -- the reader is deleted, renamed, or loses its last cross-file
# caller -- the exemption lapses and the instrument is reported again, which is
# the behaviour that makes this an assertion rather than a mute button.
#
# The annotation sits on the INSTRUMENT, not on the reader, because that is
# where the next person reading the definition needs it.
#
# HOW TO REGENERATE THE ALLOWLIST
# -------------------------------
#   scripts/check-orphan-instruments.sh --update-allowlist
#
# That rewrites `scripts/baselines/orphan-instruments-allowlist.txt` from the
# census this run just took, and prints the delta. Per
# `scripts/baselines/README.md`: never hand-edit an entry, and record WHY it
# moved. Removing a name is the good direction and needs no ceremony; ADDING
# one is admitting a new orphan, and the commit that does it should say which
# instrument, and why it is acceptable that nothing reads it yet.
#
# PROVENANCE OF VERSION 1's ALLOWLIST, stated plainly
# ---------------------------------------------------
# The author of this script was not permitted to execute it. The frozen list
# was produced by running THIS SCRIPT'S OWN SEARCHES as separate `rg` and `awk`
# invocations against the worktree at `33957575f` plus this lane's two edits,
# and recording the result. The searches were run; the assembled script was
# READ, NOT EXECUTED. If the first CI run is red, the expected cause is a
# transcription difference between those invocations and this file — run
# `--update-allowlist` once, inspect the delta, and land it as a correction
# rather than treating a red first run as 112 new defects.
#
# Usage:
#   scripts/check-orphan-instruments.sh                   # check, exit 1 on a new orphan
#   scripts/check-orphan-instruments.sh --verbose         # print the whole census
#   scripts/check-orphan-instruments.sh --update-allowlist
# Env:
#   ORPHAN_ALLOWLIST   alternate allowlist path
# Exit: 0 ok · 1 new orphan(s) · 2 no allowlist · 3 the gate itself is broken
# ---------------------------------------------------------------------------
set -uo pipefail

script_path=${BASH_SOURCE[0]//\\//}
script_dir=$(cd -- "$(dirname -- "$script_path")" && pwd)
cd -- "$script_dir/.." || exit 3

ALLOWLIST="${ORPHAN_ALLOWLIST:-scripts/baselines/orphan-instruments-allowlist.txt}"

verbose=0
update=0
case "${1:-}" in
    --verbose) verbose=1 ;;
    --update-allowlist) update=1 ;;
    "") ;;
    *) echo "unknown argument: $1" >&2; exit 3 ;;
esac

# Locate ripgrep, including where a Windows dev box keeps it. This is NOT a
# fallback matcher: every path below resolves to the SAME `rg` binary, so the
# scan rules the frozen allowlist was taken with are unchanged. It exists
# because without it this gate is unrunnable by anyone whose `rg` is not on
# PATH — which on this project included every review lane (the gate refused with
# exit 3 inside a nested `bash -c` even where `rg` resolved in the outer shell)
# and would include a CI runner without ripgrep installed. A gate nobody can run
# is a gate nobody runs.
if ! command -v rg >/dev/null 2>&1; then
    for _rg_candidate in "${RIPGREP:-}" /c/Users/*/AppData/Local/OpenAI/Codex/bin/*/rg.exe /c/Users/*/AppData/Local/Programs/*/resources/app/node_modules/@vscode/ripgrep/bin/rg.exe /c/Program*Files/ripgrep/rg.exe "$HOME"/.cargo/bin/rg.exe "$HOME"/.cargo/bin/rg
    do
        if [ -x "$_rg_candidate" ]; then
            PATH="$(dirname "$_rg_candidate"):$PATH"
            export PATH
            break
        fi
    done
    unset _rg_candidate
fi

if ! command -v rg >/dev/null 2>&1; then
    # Unlike `check-no-diag-prints.sh`, there is no find+grep fallback arm here.
    # C1/C2 need a multi-pattern FIXED-STRING pass over the whole workspace
    # (`rg -F -f`), which is the only reason this completes in seconds rather
    # than minutes, and a `find -exec grep` rewrite of it would be a second
    # implementation of the matching rules — i.e. a second thing to keep in step
    # with the frozen allowlist. Refusing is better than silently scanning with
    # different rules than the ones the baseline was taken with.
    echo "ERROR: ripgrep (rg) is required by this gate and was not found."
    echo "       Refusing rather than scanning the tree with a different"
    echo "       matcher than the one the frozen allowlist was taken with."
    echo "       Install ripgrep, put it on PATH, or set RIPGREP=/path/to/rg."
    exit 3
fi

TMP=$(mktemp -d) || exit 3
trap 'rm -rf "$TMP"' EXIT

# `rg` prints `.\a\b.rs` on Windows and `./a/b.rs` elsewhere. Normalise the
# WHOLE line, not just the path: the classifier below scans the source text for
# `name(` and a backslash inside a Rust string literal is not part of any
# identifier, so flattening it is harmless and keeps one code path.
#
# `tr '\134'` rather than a literal backslash: the octal escape survives every
# layer of quoting between a CI runner's shell and `tr`, and a lost backslash
# here would silently stop normalising Windows paths, which turns every
# defining-file comparison below into a mismatch and reports the whole census
# as orphaned.
norm() { tr '\134' '/' | sed 's@^\./@@'; }

# ---------------------------------------------------------------------------
# PASS 1 — CANDIDATES (C1 and C2), with the atomic-body filter.
#
# `-A 12` because the filter is a property of the BODY, not the signature: a
# `pub fn foo() -> usize` that returns `self.len` is not an instrument, and one
# that returns `X.load(Ordering::Relaxed)` is. Twelve lines is the whole body of
# every counter getter in this tree and the first statement of every wider one;
# it is a floor on the filter, and the failure direction is admitting a
# non-instrument to the census (which the allowlist then absorbs) rather than
# dropping a real one.
# ---------------------------------------------------------------------------
DEF_C1='^[[:space:]]*pub fn (record|note)_[a-z_0-9]+[[:space:]]*\('
DEF_C2='^[[:space:]]*pub fn [a-z_0-9]+\(\)[[:space:]]*->[[:space:]]*(usize|u64|u32|u16|i64|i32|\(usize|\(u64|\(u32|Option<usize>|Option<u64>|Vec<\()'

rg --no-heading --line-number --type rust \
   --glob '!**/tests/**' --glob '!**/target/**' \
   -A 12 -e "$DEF_C1" -e "$DEF_C2" . 2>/dev/null | norm > "$TMP/defctx" || true

awk '
function flush(){ if (cur != "" && atomic) print curfile, cur; cur=""; atomic=0 }
/^--$/ { flush(); next }
{
  line = $0
  if (match(line, /^[^:]+:[0-9]+:/)) {
     flush()
     p = index(line, ":"); curfile = substr(line, 1, p - 1)
     rest = line; sub(/^[^:]+:[0-9]+:/, "", rest)
     if (match(rest, /pub fn [a-z_0-9]+/)) cur = substr(rest, RSTART + 7, RLENGTH - 7); else cur = ""
     body = rest
  } else {
     body = line
  }
  if (body ~ /Ordering::|fetch_add\(|fetch_sub\(|\.load\(|\.store\(|Atomic|swap\(/) atomic = 1
}
END { flush() }
' "$TMP/defctx" | sort -u > "$TMP/cands"

cut -d' ' -f2 "$TMP/cands" | sort -u > "$TMP/names"
awk '{ print $2, $1 }' "$TMP/cands" | sort -u > "$TMP/defs"

# POSITIVE CONTROL, half one. `implicit_null::stale_duplicates_retired` is a
# zero-argument `pub fn -> usize` whose body is a single relaxed load, i.e. the
# textbook C2 candidate, and it is the counter whose orphaning is the reason
# this gate exists. If the census cannot see it, the census is broken and an
# empty orphan list would be a lie about the whole workspace rather than good
# news. Deleting that function is a legitimate edit — retire the sentinel in
# the same change, and say in the commit what replaced it.
SENTINEL_NAME='stale_duplicates_retired'
SENTINEL_DEF_FILE='jit/src/implicit_null.rs'
if ! grep -qx "$SENTINEL_NAME" "$TMP/names"; then
    echo "ERROR: the candidate census does not contain \`$SENTINEL_NAME\`."
    echo "       That is a zero-argument \`pub fn -> usize\` whose body is one"
    echo "       atomic load, in $SENTINEL_DEF_FILE. If this gate cannot see"
    echo "       it, the definition pass is broken and an empty result means"
    echo "       nothing was scanned, not that nothing is orphaned."
    echo ""
    echo "       Check: did \`--type rust\` or a --glob change? is the working"
    echo "       directory the repository root? was the function renamed or"
    echo "       deleted (in which case retire the sentinel deliberately)?"
    exit 3
fi

# ---------------------------------------------------------------------------
# PASS 2 — CALL SITES. One FIXED-STRING multi-pattern pass.
#
# `-F` rather than a regex alternation is not a micro-optimisation: 770-odd
# regex alternatives with `\b` anchors took over two minutes over this tree,
# and the same names as literals take about eight seconds, because rg can drop
# to Aho-Corasick only when every pattern is literal. The word-boundary check
# `\b` would have done is performed below on the preceding character instead,
# which is what keeps `is_record_class(` from counting as a call to
# `record_class`.
# ---------------------------------------------------------------------------
awk '{ print $0 "(" }' "$TMP/names" > "$TMP/lits"

rg --no-heading --line-number --type rust \
   --glob '!**/tests/**' --glob '!**/target/**' \
   -F -f "$TMP/lits" . 2>/dev/null | norm > "$TMP/hits" || true

# `-v NAMES=`, not a trailing `NAMES=` operand: a command-line assignment is
# applied when awk REACHES it in the operand list, which is after BEGIN has
# already run. Read from BEGIN it is the empty string, and `getline n < ""` is a
# fatal null-redirection error -- so the census loaded nothing, every name came
# back uncalled, and the positive control below fired on a tree that was fine.
awk -v NAMES="$TMP/names" '
BEGIN { while ((getline n < NAMES) > 0) names[++N] = n }
{
  line = $0
  p = index(line, ":");  if (p == 0) next
  file = substr(line, 1, p - 1)
  rest = substr(line, p + 1)
  q = index(rest, ":");  if (q == 0) next
  text = substr(rest, q + 1)

  # A line comment quoting an instrument is not a call site, and the lines most
  # likely to quote one are exactly the doc comments that explain what it is
  # for. Block comments still slip through; that is a written-down limit, not an
  # assumption. (Same reasoning as `untyped-alloc-ratchet.sh`.)
  t = text; sub(/^[ \t]*/, "", t)
  if (substr(t, 1, 2) == "//") next

  for (i = 1; i <= N; i++) {
    nm = names[i]; pat = nm "("
    s = text; base = 0
    while ((k = index(s, pat)) > 0) {
      abs = base + k
      if (abs == 1) { print nm, file; break }
      c = substr(text, abs - 1, 1)
      if (c !~ /[A-Za-z0-9_]/) { print nm, file; break }
      base = abs + length(pat) - 1
      s = substr(text, base + 1)
    }
  }
}
' "$TMP/hits" | sort -u > "$TMP/callmap"

# A reference in the DEFINING file is never a caller — see the header.
awk 'NR==FNR { d[$1 FS $2] = 1; next } !(($1 FS $2) in d) { print $1 }' \
    "$TMP/defs" "$TMP/callmap" | sort -u > "$TMP/called"

# POSITIVE CONTROL, half two, and it is the one that matters. The sentinel must
# be seen as CALLED FROM ANOTHER FILE. That exercises the whole second pass and
# the cross-file classification together: the `-F` search, the preceding-
# character check, the comment filter, and the defining-file subtraction. Half
# one only proves the definition pass ran.
#
# The caller is `vm-cli/src/main.rs`, added in the same change as this gate. If
# that read is ever removed, this exits 3 rather than quietly reclassifying the
# sentinel as an orphan and reporting a 113-name failure.
if ! grep -qx "$SENTINEL_NAME" "$TMP/called"; then
    echo "ERROR: \`$SENTINEL_NAME\` is in the census but is not seen as called"
    echo "       from any file other than $SENTINEL_DEF_FILE."
    echo ""
    echo "       It IS called, from vm-cli/src/main.rs's"
    echo "       \`[cratonvm] implicit null-check table health:\` line. So either"
    echo "       that read was deleted -- in which case the counter is orphaned"
    echo "       again and this gate is telling you so in the loudest way it"
    echo "       has -- or the call-site pass is broken and every 'no caller'"
    echo "       verdict below it is worthless."
    echo ""
    echo "       Check: rg -F 'stale_duplicates_retired(' vm-cli/src/main.rs"
    exit 3
fi

comm -23 "$TMP/names" "$TMP/called" > "$TMP/uncalled"

# ---------------------------------------------------------------------------
# PASS 2b — THE `read-by` EXEMPTION, checked on both halves. See L8 and
# "THE `read-by` ANNOTATION" in the header for why this is narrow and opt-in.
#
# Runs only over the names pass 2 could not find a cross-file caller for, so it
# costs at most a couple of file reads per uncalled instrument, and nothing at
# all on a tree that carries no annotations.
#
# The annotation must be ATTACHED TO THE DEFINITION -- i.e. inside the
# unbroken run of comment and attribute lines directly above `pub fn NAME(` --
# not merely somewhere in the file. The first version of this pass searched the
# whole file and every uncalled instrument in `gc/src/g1.rs` (a 30,000-line
# file with eight of them) inherited the one annotation written for
# `g1_evac_copy_span`. An exemption that leaks to its neighbours is worse than
# no exemption.
# ---------------------------------------------------------------------------
: > "$TMP/exempt"
while read -r name; do
    [[ -z "$name" ]] && continue
    deffile=$(awk -v n="$name" '$1 == n { print $2; exit }' "$TMP/defs")
    [[ -z "$deffile" || ! -f "$deffile" ]] && continue

    # The annotation, read out of the comment block attached to the definition.
    reader=$(awk -v n="$name" '
        $0 ~ ("^[[:space:]]*(pub )?(pub[(]crate[)] )?fn " n "[(]") { print ann; exit }
        {
            t = $0; sub(/^[ \t]*/, "", t)
            if (t ~ /^\/\// || t ~ /^#\[/ || t == "") {
                if (match(t, /^\/\/[[:space:]]*orphan-gate:[[:space:]]*read-by[[:space:]]+[A-Za-z_][A-Za-z_0-9]*/)) {
                    m = substr(t, RSTART, RLENGTH)
                    sub(/^.*read-by[[:space:]]+/, "", m)
                    ann = m
                }
            } else {
                ann = ""      # any non-comment line breaks the attached block
            }
        }
    ' "$deffile")
    [[ -z "$reader" ]] && continue

    # HALF ONE: the named reader really is defined here and really calls the
    # instrument. Scanned from its signature to the first column-0 `}`, the
    # same block rule as `untyped-alloc-ratchet.sh`.
    if ! awk -v r="$reader" -v n="$name" '
        $0 ~ ("^[[:space:]]*(pub )?(pub[(]crate[)] )?fn " r "[(]") { inbody = 1; seen = 1 }
        inbody && index($0, n "(") > 0 {
            t = $0; sub(/^[ \t]*/, "", t)
            if (substr(t, 1, 2) != "//" && index($0, "fn " n "(") == 0) { found = 1 }
        }
        inbody && /^\}/ { inbody = 0 }
        END { exit((seen && found) ? 0 : 1) }
    ' "$deffile"; then
        echo "NOTE: \`$name\` names \`$reader\` as its reader (orphan-gate: read-by),"
        echo "      but no call to \`$name(\` was found in a \`$reader\` defined in"
        echo "      $deffile. The annotation is an unchecked claim; ignoring it."
        continue
    fi

    # HALF TWO: the reader chain leaves the file. Asked with the same search
    # pass 2 uses, and asked SEPARATELY rather than against `$TMP/called`
    # because the reader is usually not an instrument itself -- here it returns
    # a `String`, so it is not in the C1/C2 census at all and would never
    # appear in that set however well it is called.
    if ! rg --no-heading --line-number --type rust \
            --glob '!**/tests/**' --glob '!**/target/**' \
            -F "$reader(" . 2>/dev/null | norm \
         | grep -v "^${deffile}:" \
         | awk '{ t = $0; sub(/^[^:]+:[0-9]+:[ \t]*/, "", t); if (substr(t,1,2) != "//") { found = 1 } }
                END { exit(found ? 0 : 1) }'; then
        echo "NOTE: \`$name\` names \`$reader\` as its reader (orphan-gate: read-by),"
        echo "      but \`$reader\` is itself called from no file outside $deffile."
        echo "      An orphan cannot exempt another orphan -- that is how a dead"
        echo "      report launders a dead counter -- so the annotation is ignored"
        echo "      and \`$name\` is reported below."
        continue
    fi

    echo "$name" >> "$TMP/exempt"
done < "$TMP/uncalled"

sort -u "$TMP/exempt" -o "$TMP/exempt"
comm -23 "$TMP/uncalled" "$TMP/exempt" > "$TMP/orphans_fn"

# ---------------------------------------------------------------------------
# PASS 3 — C3, unfed metric rows.
#
# Restricted to `*_EVENTS: [&str; N]` tables, which is the shape that comes with
# a `record_*_event(name: &str)` lookup that IGNORES an unknown name. Other
# `[&str; N]` tables in this tree are opcode names, prefixes and slot labels,
# where a single occurrence is normal, so they are deliberately not scanned.
#
# A row is FED if EITHER its literal occurs more than once in the workspace, OR
# the spelling `TABLE[i]` occurs anywhere. The second arm is not optional and
# the first version of this check did not have it: this tree feeds
# `SCHEDULING_EVENTS` entirely BY INDEX
# (`record_scheduling_event(SCHEDULING_EVENTS[6])`, 37 such references across 8
# rows), and `DEOPT_STASH_EVENTS` is a PULL table whose rows are only ever named
# as `DEOPT_STASH_EVENTS[0]`/`[1]` by the getter that emits them. A literal-only
# rule reported four of this tree's 37 rows as unfed, and all four were wrong.
#
# The arm does not weaken the check where it earned its place: `OSR_EVENTS` has
# ZERO `OSR_EVENTS[` references in the whole tree — every one of its feeders
# spells the literal — so `osr_compile_declined`, the row this check exists for,
# would still have been caught by it. Verified by count, 2026-09-21.
# ---------------------------------------------------------------------------
: > "$TMP/orphans_row"
rg --no-heading --line-number --type rust \
   --glob '!**/tests/**' --glob '!**/target/**' \
   -A 400 -e '^[[:space:]]*(pub )?(const|static) [A-Z_0-9]*_EVENTS: \[&.?.?.?.?str; [0-9]+\][[:space:]]*=' . 2>/dev/null \
   | norm > "$TMP/rowctx" || true

# Bounded by the table's own closing `];`, not by the `-A` window. The window
# is generous on purpose (the OSR table's rows carry several paragraphs of
# prose apiece), and without the terminator the tail of it walks into whatever
# array or match arm comes next -- which is how a checker starts reporting
# somebody else's string literals as unfed metric rows.
awk '
{
  line = $0
  ismatch = (line ~ /^[^:]+:[0-9]+:/)
  text = line
  if (ismatch) { sub(/^[^:]+:[0-9]+:/, "", text) } else { sub(/^[^-]+-[0-9]+-/, "", text) }
  t = text; sub(/^[ \t]*/, "", t); sub(/[ \t]*$/, "", t)

  if (ismatch) {
    active = 1; idx = 0; tbl = "?"
    if (match(t, /[A-Z_0-9]+_EVENTS/)) tbl = substr(t, RSTART, RLENGTH)
    # A one-line table: `... = ["a", "b"];` -- take its literals here and close.
    if (t ~ /\];[ \t]*$/) {
      s = t
      sub(/^[^=]*=/, "", s)
      while (match(s, /"[a-z0-9_]+"/)) {
        print tbl "\t" idx "\t" substr(s, RSTART + 1, RLENGTH - 2)
        idx++
        s = substr(s, RSTART + RLENGTH)
      }
      active = 0
    }
    next
  }
  if (!active) next
  if (t == "];" || t ~ /^\];/) { active = 0; next }
  # One row per line, which is the shape every multi-line table here uses.
  # Anything else inside the braces (prose, an attribute) is ignored -- and the
  # index only advances on a row, so it tracks the array subscript exactly.
  if (t ~ /^"[a-z0-9_]+",?$/) {
    match(t, /"[a-z0-9_]+"/)
    print tbl "\t" idx "\t" substr(t, RSTART + 1, RLENGTH - 2)
    idx++
  }
}
' "$TMP/rowctx" | sort -u > "$TMP/rows"

if [[ ! -s "$TMP/rows" ]]; then
    echo "ERROR: no \`*_EVENTS: [&str; N]\` row literals were found at all."
    echo "       jit/src/metrics.rs defines OSR_EVENTS, LOOP_XFORM_EVENTS,"
    echo "       SCHEDULING_EVENTS and DEOPT_STASH_EVENTS; a scan that finds"
    echo "       none of their rows is broken, not looking at a tree without"
    echo "       metric tables."
    exit 3
fi

while IFS=$'\t' read -r tbl idx row; do
    lit=$(rg --no-heading --type rust --glob '!**/target/**' -F "\"$row\"" . 2>/dev/null | wc -l)
    [[ "$lit" -gt 1 ]] && continue
    byidx=$(rg --no-heading --type rust --glob '!**/target/**' -F "${tbl}[${idx}]" . 2>/dev/null | wc -l)
    [[ "$byidx" -gt 0 ]] && continue
    echo "$row" >> "$TMP/orphans_row"
done < "$TMP/rows"

# ---------------------------------------------------------------------------
# VERDICT
# ---------------------------------------------------------------------------
{ sed 's/^/fn /' "$TMP/orphans_fn"; sed 's/^/row /' "$TMP/orphans_row"; } \
    | sort -u > "$TMP/found"

if [[ $update -eq 1 ]]; then
    mkdir -p "$(dirname "$ALLOWLIST")"
    if [[ -f "$ALLOWLIST" ]]; then
        grep -v '^#' "$ALLOWLIST" | grep -v '^[[:space:]]*$' | sort -u > "$TMP/old" || true
    else
        : > "$TMP/old"
    fi
    {
        echo "# Frozen allowlist for scripts/check-orphan-instruments.sh."
        echo "#"
        echo "# ONE LINE PER ORPHANED INSTRUMENT, as \`fn <name>\` or \`row <name>\`."
        echo "#"
        echo "# This is a DEBT REGISTER, not a list of things that are fine. Every"
        echo "# entry is an instrument (or a metric row) that reads zero forever"
        echo "# because nothing outside its own file feeds or reads it, and zero is"
        echo "# indistinguishable from 'this never happened'. Several are certainly"
        echo "# the same defect round 10 found five times by hand."
        echo "#"
        echo "# Written by \`scripts/check-orphan-instruments.sh --update-allowlist\`."
        echo "# Never hand-edit a line (scripts/baselines/README.md). Removing a name"
        echo "# is the good direction. ADDING one admits a new orphan, and the commit"
        echo "# that does it has to say which instrument and why nothing reads it yet."
        echo "#"
        echo "# Generated: $(date -u '+%Y-%m-%d') UTC"
        echo "# Entries:   $(wc -l < "$TMP/found" | tr -d ' ')"
        cat "$TMP/found"
    } > "$ALLOWLIST"
    echo "wrote $ALLOWLIST ($(wc -l < "$TMP/found" | tr -d ' ') entries)"
    echo ""
    echo "REMOVED (an instrument that gained a reader -- good):"
    comm -23 "$TMP/old" "$TMP/found" | sed 's/^/  - /'
    echo "ADDED (a new orphan -- say why in the commit):"
    comm -13 "$TMP/old" "$TMP/found" | sed 's/^/  + /'
    exit 0
fi

if [[ ! -f "$ALLOWLIST" ]]; then
    echo "ERROR: no allowlist at $ALLOWLIST."
    echo "       This gate is a ratchet and cannot report without a baseline to"
    echo "       ratchet against. Take one with --update-allowlist and commit it"
    echo "       in a change that says what the numbers are."
    exit 2
fi

grep -v '^#' "$ALLOWLIST" | grep -v '^[[:space:]]*$' | sort -u > "$TMP/allow"
comm -23 "$TMP/found" "$TMP/allow" > "$TMP/new"
comm -13 "$TMP/found" "$TMP/allow" > "$TMP/fixed"

if [[ $verbose -eq 1 ]]; then
    echo "candidates (C1+C2): $(wc -l < "$TMP/names" | tr -d ' ')"
    echo "orphaned fns:       $(wc -l < "$TMP/orphans_fn" | tr -d ' ')"
    echo "read-by exempt:     $(wc -l < "$TMP/exempt" | tr -d ' ')"
    echo "event rows scanned: $(wc -l < "$TMP/rows" | tr -d ' ')"
    echo "unfed rows:         $(wc -l < "$TMP/orphans_row" | tr -d ' ')"
    echo "allowlisted:        $(wc -l < "$TMP/allow" | tr -d ' ')"
    echo ""
    echo "--- full census ---"
    cat "$TMP/found"
    echo "-------------------"
    echo ""
fi

if [[ -s "$TMP/fixed" ]]; then
    # Never automatic. `SLACK` is zero and stays zero, per
    # scripts/baselines/README.md: an improvement that the gate absorbs on its
    # own re-admits exactly that many new orphans silently.
    echo "NOTE: $(wc -l < "$TMP/fixed" | tr -d ' ') allowlisted instrument(s) now have a reader:"
    sed 's/^/  /' "$TMP/fixed"
    echo "      Re-freeze with --update-allowlist to lock the improvement in."
    echo ""
fi

if [[ ! -s "$TMP/new" ]]; then
    exit 0
fi

echo "ERROR: $(wc -l < "$TMP/new" | tr -d ' ') instrument(s) with no reader, not on the allowlist:"
echo ""
sed 's/^/  /' "$TMP/new"
echo ""
echo "       Each of these reads ZERO on every run, and zero is"
echo "       indistinguishable from 'the thing it watches never happened'."
echo "       That is the defect round 10 found five times in five files."
echo ""
echo "       Fix it by giving it a reader, not by adding it here:"
echo "         * a counter getter -> read it in vm-cli's method-stats dump,"
echo "           beside the counters it has to be read next to;"
echo "         * a \`record_*\`/\`note_*\` -> call it from the path it reports on;"
echo "         * an \`*_EVENTS\` row -> pass the literal to the matching"
echo "           \`record_*_event\`, or delete the row."
echo ""
echo "       If it genuinely has to land unread, add it with"
echo "       --update-allowlist and say in the commit which instrument and why."
exit 1
