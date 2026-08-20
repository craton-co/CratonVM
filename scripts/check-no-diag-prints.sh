#!/usr/bin/env bash
# RJ.1: forbid reintroduction of debug-trace eprintln! / println! markers.
#
# These markers (`[WP*]`, `[DIAG*]`, `[TRACE*]`, `[SB-TRACE]`, `[BUFFER]`,
# `[CV*]`, `[FJP*]`, `[FUT*]`) were used during bring-up to correlate
# Rust-side behaviour with failing Java workloads. They MUST NOT appear
# in landed code — they pollute stderr, race with structured tracing,
# and mask real errors. Structured diagnostics belong in
# `tracing::{debug,info,warn,error}!`.
#
# Allowed exceptions (do NOT trip this gate):
#   * `[cratonvm] ...`        — intentional user-visible CLI messages
#                              (watchdog banner, System.exit notice, etc.)
#   * `tracing::*!(...)`     — structured logger, filterable
#   * env-gated under `CRATONVM_DBG_*` — opt-in diagnostics
#   * anything inside `#[cfg(test)]` or under `tests/`
#
# Usage:
#   scripts/check-no-diag-prints.sh            # scan, exit 1 on hit
#   scripts/check-no-diag-prints.sh --verbose  # print every hit

set -euo pipefail

script_path=${BASH_SOURCE[0]//\\//}
script_dir=$(cd -- "$(dirname -- "$script_path")" && pwd)
cd -- "$script_dir/.."

verbose=0
if [[ "${1:-}" == "--verbose" ]]; then
    verbose=1
fi

# Forbidden bracket-tag prefixes after `eprintln!("` / `println!("`.
# Anchored to the literal `"[<prefix>` start so we don't false-match on
# legitimate strings that happen to mention `WP` mid-message.
PATTERN='(println|eprintln)![[:space:]]*\([[:space:]]*"\[(WP|DIAG|TRACE|SB-TRACE|BUFFER|CV|FJP|FUT)'

# A pattern that MUST match somewhere in this repository. It is the positive
# control for the search itself — see `SENTINEL` below.
SENTINEL_PATTERN='^(pub )?fn [a-z_]'

# Use ripgrep when available (fast, respects .gitignore); fall back to find+grep.
#
# Both arms end in `|| true`, which is deliberate — `rg`/`grep` exit 1 on "no
# matches", and no-matches is the SUCCESS case here. But it also swallows a
# search that FAILED, and an empty `hits` cannot tell the two apart. That is a
# self-disabling gate: mutate the search (e.g. `--type rustx`, a typo in a
# glob, a `find` predicate that prunes everything) and this blocking CI step
# reports a clean tree and exits 0 while scanning nothing. Same species as
# `regression-suite/harness-guard.sh`'s G4 — an oracle that is sick must not
# score green — and as the fixture-did-not-build hole fixed in
# `scripts/jdk-only-strict-probes.sh` on 2026-08-12. Hence the sentinel.
if command -v rg >/dev/null 2>&1; then
    search() {
        rg --no-heading --line-number \
            --type rust \
            --glob '!**/tests/**' \
            --glob '!**/target/**' \
            -e "$1" \
            . 2>/dev/null || true
    }
else
    search() {
        find . \
            \( -path './.git' -o -path './target' -o -path '*/target' -o -path '*/tests' \) -prune \
            -o -type f -name '*.rs' \
            -exec grep --line-number --with-filename -E -- "$1" {} + \
            2>/dev/null || true
    }
fi

# POSITIVE CONTROL. Run the identical machinery against a pattern this
# repository cannot fail to contain. If it comes back empty the search is
# broken, not the tree clean, and a silent exit 0 here would be a lie about
# every `.rs` file in the workspace.
sentinel=$(search "$SENTINEL_PATTERN")
if [[ -z "$sentinel" ]]; then
    echo "ERROR: the diag-print search matched no Rust function definition."
    echo "       That means the SEARCH is broken, not that the tree is clean:"
    echo "       this gate scans thousands of .rs files and every one of them"
    echo "       defines at least one fn. Refusing rather than reporting a"
    echo "       clean tree it never read."
    echo ""
    echo "       Check: is ripgrep present and does it still accept"
    echo "       --type rust? did a glob or a find predicate change? is the"
    echo "       working directory the repository root?"
    exit 2
fi

hits=$(search "$PATTERN")
if [[ -z "$hits" ]]; then
    exit 0
fi

echo "ERROR: forbidden debug-trace prints detected. Remove the following"
echo "       eprintln!/println! calls or convert to tracing::debug!/info!:"
echo ""
if [[ $verbose -eq 1 ]]; then
    echo "$hits"
else
    echo "$hits" | head -40
    count=$(echo "$hits" | wc -l | tr -d ' ')
    if [[ "$count" -gt 40 ]]; then
        echo ""
        echo "(plus $((count - 40)) more — run with --verbose)"
    fi
fi
exit 1
