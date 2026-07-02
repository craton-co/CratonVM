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

# Use ripgrep when available (fast, respects .gitignore); fall back to find+grep.
if command -v rg >/dev/null 2>&1; then
    search() {
        rg --no-heading --line-number \
            --type rust \
            --glob '!**/tests/**' \
            --glob '!**/target/**' \
            -e "$PATTERN" \
            . 2>/dev/null || true
    }
else
    search() {
        find . \
            \( -path './.git' -o -path './target' -o -path '*/target' -o -path '*/tests' \) -prune \
            -o -type f -name '*.rs' \
            -exec grep --line-number --with-filename -E -- "$PATTERN" {} + \
            2>/dev/null || true
    }
fi

hits=$(search)
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
