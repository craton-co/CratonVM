#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
# Copyright 2024-2026 Craton Software Company
#
# tools/text-order/relink.sh -- relink an already-built `cratonvm` (Linux, LLD)
# with its boot-executed functions packed together. See README.md.
#
# Usage (from the workspace root, after `cargo build <ARGS> -p cratonvm-cli`):
#   tools/text-order/relink.sh <ARGS>        e.g. --release --target x86_64-unknown-linux-gnu
#
# Resolves tools/text-order/boot-text.order against the built binary's own
# symbols (order.py resolve), then re-runs only the launcher's final crate and
# link with `-Wl,--symbol-ordering-file`. Skips (exit 0, with a note) when the
# binary was not linked by LLD or under 50 % of the template resolves; any
# other failure is an error. TEXT_ORDER_TEMPLATE overrides the template.
set -euo pipefail

here="$(cd "$(dirname "$0")" && pwd)"
template="${TEXT_ORDER_TEMPLATE:-$here/boot-text.order}"

profile=debug target_dir="${CARGO_TARGET_DIR:-target}" triple=""
args=("$@")
for ((i = 0; i < ${#args[@]}; i++)); do
    case "${args[$i]}" in
        --release | -r) profile=release ;;
        --profile) profile="${args[$((i + 1))]}" ;;
        --profile=*) profile="${args[$i]#--profile=}" ;;
        --target) triple="${args[$((i + 1))]}" ;;
        --target=*) triple="${args[$i]#--target=}" ;;
        --target-dir) target_dir="${args[$((i + 1))]}" ;;
        --target-dir=*) target_dir="${args[$i]#--target-dir=}" ;;
    esac
done
[ "$profile" = dev ] && profile=debug
bin="$target_dir/${triple:+$triple/}$profile/cratonvm"
[ -x "$bin" ] || { echo "text-order: no binary at $bin; build it first" >&2; exit 1; }

if ! readelf -p .comment "$bin" 2>/dev/null | grep -q "Linker: LLD"; then
    echo "text-order: $bin was not linked by LLD; leaving it as built"
    exit 0
fi

work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT
out="$(python3 "$here/order.py" resolve "$template" "$bin" "$work/order.txt")"
echo "text-order: $out"
pct="$(sed -n 's/.*(\([0-9]*\)\.[0-9] %).*/\1/p' <<<"$out")"
if [ "${pct:-0}" -lt 50 ]; then
    echo "text-order: the template is stale for this build (${pct:-0} % resolves);"
    echo "text-order: regenerate it with tools/text-order/census.sh. Leaving the binary as built."
    exit 0
fi

cargo rustc "$@" -p cratonvm-cli --bin cratonvm -- \
    -C "link-arg=-Wl,--symbol-ordering-file=$work/order.txt" \
    -C "link-arg=-Wl,--no-warn-symbol-ordering"
first="$(grep -m1 . "$work/order.txt")"
echo "text-order: relinked $bin; first ordered function at 0x$(nm --defined-only "$bin" | awk -v s="$first" '$3 == s { print $1; exit }')"
