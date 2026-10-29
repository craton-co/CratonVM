#!/usr/bin/env bash
# Forbid an UNSCREENED `Value` decode of a heap cell.
#
# `cratonvm_types::read_value_atomic` loads two words and `transmute`s them
# into a `Value` with no validation of the `#[repr(u32)]` discriminant. When
# the 16 bytes were not a `Value` — array payload read through the flat-object
# path, a punned write, a stale cell — the result is an enum holding an
# out-of-range tag, which is UB the instant it exists.
#
# The cost is not a fault at the read. A `match` on a Rust enum needs no
# default arm and so gets NO bounds check: LLVM emits
#
#     movsxd rax, dword ptr [r10 + rax*4]      ; rax = the enum's u32 tag
#
# and jumps through the result. That instruction, in
# `gc::heap::coerce_field_value_for_slot`, is the decoded faulting instruction
# of the eight hibernate-orm JSON/XML `hs_err` files — and it lives one frame
# ABOVE the reader that built the bad `Value`, which is why every audit that
# looked for jump tables AT the read sites came back empty. See
# `internal/fixed-bugs/hib-orm-json-xml-function-tests-segfault-g1-zgc-FIXED-20260901.md`.
#
# The screened readers are `cratonvm_types::read_value_checked_atomic` and its
# two reporting wrappers, `gc::heap::read_value_cell_checked` and
# `jit::helpers::jit_read_value_cell_checked`. Every heap-cell decode goes
# through one of them.
#
# ALLOWED (and why each is not a heap-cell decode):
#   * `types/src/value.rs`  — defines both readers and round-trips them in its
#                             own tests; `read_value_checked_atomic` is
#                             literally implemented beside `read_value_atomic`.
#   * `*/tests/**`, `#[cfg(test)]` — test code writes the cell it then reads.
#   * comments and doc comments  — this gate is about calls, and every call
#                                  site it removed left a comment explaining
#                                  why.
#
# Usage:
#   scripts/check-value-cell-reads.sh            # scan, exit 1 on hit
#   scripts/check-value-cell-reads.sh --verbose  # print every hit

set -euo pipefail

script_path=${BASH_SOURCE[0]//\\//}
script_dir=$(cd -- "$(dirname -- "$script_path")" && pwd)
cd -- "$script_dir/.."

verbose=0
if [[ "${1:-}" == "--verbose" ]]; then
    verbose=1
fi

# A CALL to the unchecked reader: the name followed by an open paren, with any
# path qualification in front. Anchored past a `//` so the many comments that
# name the function (deliberately — they explain why the call went away) do not
# trip it.
PATTERN='^[^/]*(^|[^_[:alnum:]])read_value_atomic[[:space:]]*\('

# A pattern that MUST match somewhere in this repository. It is the positive
# control for the search itself — see `SENTINEL` below.
SENTINEL_PATTERN='read_value_checked_atomic[[:space:]]*\('

# Use ripgrep when available (fast, respects .gitignore); fall back to
# find+grep. Both arms end in `|| true` because "no matches" is the SUCCESS
# case and both tools exit 1 for it — which is exactly why the sentinel below
# is not optional: an empty result set cannot, on its own, tell a clean tree
# apart from a search that never ran. Same reasoning as
# `scripts/check-no-diag-prints.sh`.
if command -v rg >/dev/null 2>&1; then
    search() {
        rg --no-heading --line-number \
            --type rust \
            --glob '!**/tests/**' \
            --glob '!**/target/**' \
            --glob '!types/src/value.rs' \
            -e "$1" \
            . 2>/dev/null || true
    }
else
    search() {
        find . \
            \( -path './.git' -o -path './target' -o -path '*/target' -o -path '*/tests' \) -prune \
            -o -type f -name '*.rs' ! -path './types/src/value.rs' \
            -exec grep --line-number --with-filename -E -- "$1" {} + \
            2>/dev/null || true
    }
fi

# POSITIVE CONTROL. The screened reader has call sites in `gc` and `vm`; if the
# search cannot find those, it would not have found an unscreened one either,
# and a silent exit 0 would be a lie about every `.rs` file in the workspace.
sentinel=$(search "$SENTINEL_PATTERN")
if [[ -z "$sentinel" ]]; then
    echo "ERROR: the value-cell search matched no call to the CHECKED reader."
    echo "       That means the SEARCH is broken, not that the tree is clean."
    echo "       \`read_value_checked_atomic\` is called from gc/src/heap.rs and"
    echo "       vm/src/jit/helpers.rs; a scan that misses those is scanning"
    echo "       nothing."
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

echo "ERROR: unscreened Value-cell decode(s) detected."
echo ""
echo "       \`read_value_atomic\` transmutes 16 bytes into a \`Value\` without"
echo "       validating the discriminant. A later \`match\` on the result is a"
echo "       jump table indexed by the tag with NO bounds check — an unchecked"
echo "       jump through .rdata, which is how the hibernate-orm JSON/XML"
echo "       SIGSEGVs happened."
echo ""
echo "       Use \`read_value_checked_atomic\`, or one of its reporting"
echo "       wrappers:"
echo "         gc:  crate::heap::read_value_cell_checked(ptr, site)"
echo "         vm:  jit_read_value_cell_checked(ptr, site)"
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
