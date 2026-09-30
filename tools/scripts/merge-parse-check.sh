#!/usr/bin/env bash
# merge-parse-check.sh — does every .rs file this diff touched still PARSE?
#
# ---------------------------------------------------------------------------
# THE DEFECT THIS EXISTS FOR
# ---------------------------------------------------------------------------
#
# `native-builtins/tests/stub_ratchet.rs` **had not parsed since merge
# `26e4b5db4`**, which spliced two versions of one failure message and kept both
# argument lists. The whole `--test stub_ratchet` binary went with it, and that
# gate is the cited evidence for the P0 *Residual synthetic native set* row. It
# was blocking CI in BOTH configurations, `origin/dev` still carried the break
# days later, and combined with `G89-1`'s finding that the same gate had been
# RED in blocking CI since 2026-08-14, it adjudicated nothing for six days and
# then adjudicated nothing at all. (`H3-1`, `HANDOFF-20260820.md` §3.)
#
# `rustfmt --check` finds it in seconds and needs no build. REVERIFIED by this
# lane, independently, before the script was written:
#
#     rustfmt --check --edition 2021 <26e4b5db4's stub_ratchet.rs>
#       -> 32 errors, all lexer/parse ("unknown start of token"), 0 formatting
#     same file at HEAD
#       -> 0 errors, formatting diff only
#
# ---------------------------------------------------------------------------
# WHAT IT CATCHES, AND — MORE IMPORTANTLY — WHAT IT DOES NOT
# ---------------------------------------------------------------------------
#
# It catches: a file that does not LEX or PARSE. Unbalanced delimiters, a
# conflict-marker splice, a stray token, a duplicated argument list that leaves
# the token stream unbalanced, an editor that baked a control character into
# source (`HANDOFF-20260820.md` §8 has an instance: a heredoc wrote a literal
# 0x08 where `\b` was meant). It also catches a `mod` declaration whose file the
# merge deleted or moved.
#
# **IT IS A PARSE CHECK, NOT A TYPE CHECK.** A file that parses can still fail
# to compile in every way that matters:
#
#   * a call with the wrong number or type of arguments — including the exact
#     shape `H3-1` describes, IF the splice left the token stream balanced.
#     `assert!(c, "a {}", x, "b {}", y)` parses; the compiler rejects it. This
#     gate would pass that file. **A green result here is not "it compiles".**
#   * an unresolved name, a missing import, a borrow error, a trait bound, a
#     `cfg` that hides a module from the compiler but not from rustfmt;
#   * anything at all inside a macro body that is never expanded.
#
# So this is a fast pre-filter for one specific and historically expensive class
# of merge damage. It does not replace `cargo check`, and a CI file that puts it
# where `cargo check` used to be has made things worse.
#
# ---------------------------------------------------------------------------
# WHY THE FORMATTING DIFF IS NOT A FAILURE HERE
# ---------------------------------------------------------------------------
#
# **This repository is not rustfmt-clean and must not be made so by this gate.**
# `rustfmt --check` on an arbitrary in-tree file prints a formatting diff and
# exits 1; `types/src/flag_groups.rs` does, today, on an untouched checkout.
# A gate that failed on that would fail on nearly every file, which is
# `G89-1`'s species again — and an fmt step that goes red blocks every later
# gate in its job, so it would take `cargo check` down with it.
#
# The discriminator is measured, not assumed: **rustfmt writes parse errors to
# STDERR and formatting diffs to STDOUT.** This script fails only on stderr.
#
# ---------------------------------------------------------------------------
# WHY IT RUNS IN PLACE
# ---------------------------------------------------------------------------
#
# rustfmt follows `mod foo;` into the child file. Copying a source file to a
# scratch directory and checking it there reports `failed to resolve mod` for
# every child — a false positive that looks exactly like a merge that deleted a
# module. Both parents of `26e4b5db4` produce that error when checked out of
# tree and produce nothing when checked in place. So: always in the worktree,
# always relative to the repository root.
#
# The same recursion means a child module's parse error is reported while
# checking the PARENT. The error message names the real file and line, so blame
# is still accurate — but the file COUNT below can exceed the number of files
# the diff touched, and that is not a bug.
#
# ---------------------------------------------------------------------------
# USAGE
# ---------------------------------------------------------------------------
#
#   bash scripts/merge-parse-check.sh                     # HEAD vs its parents
#   bash scripts/merge-parse-check.sh --base origin/dev   # a branch or PR
#   bash scripts/merge-parse-check.sh --range A...B
#   bash scripts/merge-parse-check.sh --files "a/b.rs c/d.rs"
#   bash scripts/merge-parse-check.sh --all               # every tracked .rs
#
# With no arguments and HEAD a MERGE, the file set is the UNION of the diffs
# against EVERY parent — a bad conflict resolution differs from at least one
# parent, and taking only `HEAD^1..HEAD` would miss the half that came from the
# other side. With HEAD a normal commit it is `HEAD^..HEAD`.
#
# EXIT CODES
#   0  every checked file parsed (formatting diffs are reported, not counted)
#   1  at least one file did not parse, or a `mod` did not resolve
#   3  a prerequisite is missing (no rustfmt, no repo, an unusable range)
set -u

usage() { sed -n '2,95p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'; }

MODE=auto
BASE=""; RANGE=""; FILES=""; EDITION=""
SKIP_PATTERNS=""
QUIET=""
while [ $# -gt 0 ]; do
    case "$1" in
        --base)    MODE=base;  BASE="${2:-}";  shift 2 ;;
        --range)   MODE=range; RANGE="${2:-}"; shift 2 ;;
        --files)   MODE=files; FILES="${2:-}"; shift 2 ;;
        --all)     MODE=all;   shift ;;
        --edition) EDITION="${2:-}"; shift 2 ;;
        # Escape hatch for a file rustfmt cannot check STANDALONE — an
        # `include!` fragment, or a `#[path]` target whose parent supplies the
        # path. Both look like "unresolved module" and neither is a defect.
        --skip)    SKIP_PATTERNS="$SKIP_PATTERNS ${2:-}"; shift 2 ;;
        --quiet)   QUIET=1; shift ;;
        -h|--help) usage; exit 0 ;;
        *) echo "ERROR: unknown option '$1'" >&2; exit 3 ;;
    esac
done

# FAIL LOUDLY rather than fall back to a guessed root: on this case-insensitive
# filesystem a fallback silently measures the MAIN CHECKOUT. Same incident, same
# reasoning, as regression-suite/run.sh's header.
ROOT="$(git -C "$(dirname "${BASH_SOURCE[0]}")" rev-parse --show-toplevel 2>/dev/null)"
[ -n "$ROOT" ] || { echo "ERROR: not inside a git repository." >&2; exit 3; }
cd "$ROOT" || exit 3

command -v rustfmt >/dev/null 2>&1 || {
    echo "ERROR: rustfmt is not on PATH. This gate needs the rustfmt COMPONENT," >&2
    echo "       not a toolchain build: rustup component add rustfmt." >&2
    exit 3
}

# The edition is load-bearing and a wrong one is a FALSE POSITIVE FACTORY:
# rustfmt defaults to edition 2015, where `async fn` and `dyn Trait` are parse
# errors. Read it from the workspace rather than hard-coding a number that will
# be wrong after the next edition bump.
if [ -z "$EDITION" ]; then
    EDITION="$(sed -n 's/^[[:space:]]*edition[[:space:]]*=[[:space:]]*"\([0-9]*\)".*/\1/p' Cargo.toml 2>/dev/null | head -1)"
fi
if [ -z "$EDITION" ]; then
    echo "ERROR: could not read 'edition' from $ROOT/Cargo.toml. Guessing it would" >&2
    echo "       report modern syntax as a parse error; pass --edition <year>." >&2
    exit 3
fi

# --- the file set ----------------------------------------------------------
collect() {
    case "$MODE" in
        files) printf '%s\n' $FILES ;;
        all)   git ls-files '*.rs' ;;
        range)
            git rev-parse --verify --quiet "${RANGE%%.*}" >/dev/null \
                || { echo "ERROR: unusable range '$RANGE'" >&2; exit 3; }
            git diff --name-only --diff-filter=ACMR "$RANGE" -- '*.rs'
            ;;
        base)
            git rev-parse --verify --quiet "$BASE" >/dev/null \
                || { echo "ERROR: '$BASE' is not a ref in this repository." >&2; exit 3; }
            # Three dots: what THIS branch changed since the merge base, not
            # everything that landed on the base meanwhile.
            git diff --name-only --diff-filter=ACMR "$BASE...HEAD" -- '*.rs'
            ;;
        auto)
            parents="$(git rev-list --parents -n 1 HEAD | cut -d' ' -f2-)"
            if [ -z "$parents" ]; then
                # A root commit has no parent; everything it introduced is new.
                git ls-tree -r --name-only HEAD -- '*.rs'
            else
                for p in $parents; do
                    git diff --name-only --diff-filter=ACMR "$p" HEAD -- '*.rs'
                done
            fi
            ;;
    esac
}

skipped=""
LIST=""
while IFS= read -r f; do
    [ -n "$f" ] || continue
    [ -f "$f" ] || continue          # deleted by the diff; nothing to parse
    drop=""
    for pat in $SKIP_PATTERNS; do
        case "$f" in $pat) drop=1 ;; esac
    done
    if [ -n "$drop" ]; then skipped="$skipped $f"; continue; fi
    LIST="$LIST$f
"
done <<EOF
$(collect | sort -u)
EOF

n=0; for f in $LIST; do n=$((n+1)); done
echo "MERGE PARSE CHECK — $n .rs file(s), edition $EDITION, $(rustfmt --version)"
echo "  mode=$MODE  root=$ROOT  rev=$(git rev-parse --short HEAD 2>/dev/null || echo '?')"
[ -z "$skipped" ] || echo "  skipped (--skip):$skipped"
if [ "$n" -eq 0 ]; then
    # An empty set is a legitimate answer (a docs-only merge). It must SAY so
    # rather than print a green summary that reads like a sweep.
    echo "  NOTHING TO CHECK: this diff touched no .rs file. That is not a pass over"
    echo "  any file; it is the absence of anything to pass."
    exit 0
fi

# --- the sweep -------------------------------------------------------------
err_out="$(mktemp)"; fmt_out="$(mktemp)"
trap 'rm -f "$err_out" "$fmt_out"' EXIT

bad=""; badmod=""; fmt=0; ok=0
for f in $LIST; do
    : > "$err_out"; : > "$fmt_out"
    rustfmt --check --edition "$EDITION" "$f" > "$fmt_out" 2> "$err_out"
    if [ -s "$err_out" ]; then
        # Two classes, kept apart because the remedies are different: a parse
        # error is a broken FILE, an unresolved mod is a missing one.
        if grep -q 'failed to resolve mod\|couldn.t read' "$err_out" \
           && ! grep -q 'unknown start of token\|unclosed delimiter\|expected\|mismatched closing' "$err_out"; then
            badmod="$badmod $f"
            echo
            echo "UNRESOLVED MODULE: $f"
            sed 's/^/    /' "$err_out"
        else
            bad="$bad $f"
            echo
            echo "DOES NOT PARSE: $f"
            sed 's/^/    /' "$err_out"
        fi
    elif [ -s "$fmt_out" ]; then
        fmt=$((fmt+1))
        [ -n "$QUIET" ] || echo "  fmt-only  $f"
    else
        ok=$((ok+1))
        [ -n "$QUIET" ] || echo "  clean     $f"
    fi
done

nbad=0;    for f in $bad;    do nbad=$((nbad+1)); done
nbadmod=0; for f in $badmod; do nbadmod=$((nbadmod+1)); done
echo
echo "SUMMARY: $ok clean, $fmt formatting-only, $nbad PARSE FAILURE(S), $nbadmod UNRESOLVED MODULE(S)"
echo "  'formatting-only' is NOT a finding. This repository is not rustfmt-clean and"
echo "  this gate does not try to make it so; only stderr counts."
echo "  A clean result means the files PARSE. It does NOT mean they compile — see the"
echo "  header: a wrong argument list that leaves the tokens balanced passes here."
if [ -n "$bad" ]; then
    echo
    echo "PARSE FAILURE:$bad"
    echo "  This is the H3-1 shape. Look for a merge that spliced two versions of one"
    echo "  expression and kept fragments of both, a stray conflict marker, or a"
    echo "  programmatic patch that baked a control character into the source"
    echo "  (verify bytes: sed -n '<line>p' <file> | cat -A)."
fi
if [ -n "$badmod" ]; then
    echo
    echo "UNRESOLVED MODULE:$badmod"
    echo "  A 'mod x;' whose file is not where rustfmt looked. Usually a merge deleted"
    echo "  or moved the child. If instead the file is an include!/#[path] fragment that"
    echo "  cannot be checked standalone, exclude it with --skip '<glob>' and say why."
fi
[ -z "$bad" ] && [ -z "$badmod" ]
