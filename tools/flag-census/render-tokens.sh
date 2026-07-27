#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
# Copyright 2024-2026 Craton Software Company
#
# Regenerate docs/flag-tokens.md from types/src/flag_groups.rs::INVENTORY.
#
# The prose header of that file is hand-written and preserved; everything from
# the first `## \`CRATONVM_` heading onward is replaced.
#
#   tools/flag-census/render-tokens.sh          # from the repo root
set -euo pipefail

ROOT="${1:-$(cd "$(dirname "$0")/../.." && pwd)}"
SRC="$ROOT/types/src/flag_groups.rs"
OUT="$ROOT/docs/flag-tokens.md"

rows=$(mktemp); body=$(mktemp); trap 'rm -f "$rows" "$body"' EXIT

grep -oE 'E \{ group: Group::[A-Z]+, token: "[a-z0-9-]+", on_key: (Some\("[A-Z_0-9]+"\)|None), off_key: (Some\("[A-Z_0-9]+"\)|None)' "$SRC" \
  | sed 's/E { group: Group:://; s/, token: "/\t/; s/", on_key: /\t/; s/, off_key: /\t/; s/Some("/ /g; s/")/ /g' \
  | sed 's/  */ /g' > "$rows"

awk -F'\t' '
BEGIN{
  order="DBG JIT GC REAL LOADER IO THREADS SECURITY COMPAT TEST";
}
{
  g=$1; t=$2; on=$3; off=$4;
  gsub(/^ +| +$/,"",on); gsub(/^ +| +$/,"",off);
  key  = (on !="None" && on !="") ? on  : "";
  okey = (off!="None" && off!="") ? off : "";
  legacy = key;
  if (okey != "") legacy = (key != "" ? key " / " okey : okey);
  rows[g] = rows[g] sprintf("| `%s` | `%s` |\n", t, legacy);
  n[g]++;
}
END{
  split(order, o, " ");
  for (i=1; i<=length(o); i++) {
    g=o[i];
    printf "## `CRATONVM_%s`\n\n%d tokens.\n\n| Token | Expands to |\n| --- | --- |\n%s\n", g, n[g], rows[g];
  }
}' "$rows" > "$body"

# Keep the hand-written prose; replace the generated tables.
head -n "$(( $(grep -n '^## `CRATONVM_' "$OUT" | head -1 | cut -d: -f1) - 1 ))" "$OUT" > "$OUT.tmp"
cat "$body" >> "$OUT.tmp"
mv "$OUT.tmp" "$OUT"

echo "wrote $OUT ($(grep -c '^| `' "$OUT") tokens)"
