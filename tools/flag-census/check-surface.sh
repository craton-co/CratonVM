#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
# Copyright 2024-2026 Craton Software Company
#
# Fail if the CRATONVM_* environment surface has grown behind the config.
#
# The surface reached 692 identifiers by growing roughly one per fixed bug with
# no retirement path (docs/flag-census.md). It is now ten grouped
# variables plus five scalars, and every knob is a token in
# `types/src/flag_groups.rs::INVENTORY`.
#
# A new `std::env::var("CRATONVM_...")` call site that is not in the inventory
# is invisible to the grouped variables and to docs/CONFIG.md — the exact
# failure this consolidation exists to stop. This check makes adding one a
# deliberate edit of two files rather than a silent one.
#
#   tools/flag-census/check-surface.sh          # from the repo root
#   tools/flag-census/check-surface.sh /path/to/repo
set -uo pipefail

ROOT="${1:-$(cd "$(dirname "$0")/../.." && pwd)}"
FIXTURE="$ROOT/types/tests/flag-surface.txt"
INVENTORY="$ROOT/types/src/flag_groups.rs"
CARGO_BIN="${CARGO:-$(command -v cargo 2>/dev/null || true)}"
if [ -z "$CARGO_BIN" ] && [ -x "$HOME/.cargo/bin/cargo" ]; then
  CARGO_BIN="$HOME/.cargo/bin/cargo"
fi

CRATES="reader types native-api native-collections native-io native-builtins
        native-awt jit-api jit jit-cuda cuda-bridge classloading craton-gpu gc
        vm vm-cli jfr libcratonvm cratonvm-embed difftest"

fail=0

# ── 1. every CRATONVM_* literal in crate sources is a declared variable ──────
found=$(mktemp); trap 'rm -f "$found" "$expected" "$doctok" "$invtok"' EXIT
for c in $CRATES; do
  [ -d "$ROOT/$c/src" ] || continue
  grep -rhoE '"CRATONVM_[A-Z0-9_]+"' --include='*.rs' "$ROOT/$c/src" 2>/dev/null
done | tr -d '"' \
  | grep -vxE 'CRATONVM_(NONEXISTENT_VAR_12345|SOMETHING_BRAND_NEW|FOO)' \
  | sort -u > "$found"
# The three excluded names are deliberate "this variable does not exist" probes
# in unit tests (`vm.rs` System.getenv coverage, `flag_groups.rs` fall-through).
# They are literals, not configuration.

expected=$(mktemp)
grep -vE '^\s*(#|$)' "$FIXTURE" | tr -d '\r' | sort -u > "$expected"

undeclared=$(comm -23 "$found" "$expected")
if [ -n "$undeclared" ]; then
  fail=1
  echo "error: these CRATONVM_* variables are read by code but are not part of the"
  echo "       declared surface. Add a token for each to types/src/flag_groups.rs"
  echo "       INVENTORY and to types/tests/flag-surface.txt, or reuse an existing"
  echo "       token instead of minting a new variable:"
  echo "$undeclared" | sed 's/^/         /'
fi

# ── 2. every token named in the docs exists in the inventory ────────────────
invtok=$(mktemp)
grep -oE 'group: Group::[A-Z]+, token: "[a-z0-9-]+"' "$INVENTORY" \
  | sed 's/group: Group:://; s/, token: "/\//; s/"$//' \
  | tr 'A-Z' 'a-z' | sort -u > "$invtok"

doctok=$(mktemp)
# `CRATONVM_JIT=-bce,unroll` and `| `-bce` |` style references in the reference docs.
for f in "$ROOT/docs/CONFIG.md" \
         "$ROOT/docs/flag-tokens.md" \
         "$ROOT/docs/synthetic_methods.md" \
         "$ROOT/docs/book/src/reference/environment-variables.md"; do
  [ -f "$f" ] || continue
  grep -oE 'CRATONVM_(DBG|JIT|GC|REAL|LOADER|IO|THREADS|SECURITY|COMPAT|TEST)=[-+a-z0-9,=/.]+' "$f" \
    | while IFS='=' read -r var rest; do
        g=$(echo "${var#CRATONVM_}" | tr 'A-Z' 'a-z')
        echo "$rest" | tr ',' '\n' | sed 's/=.*//; s/^[-+]//' \
          | grep -E '^[a-z][a-z0-9-]*$' | sed "s|^|$g/|"
      done
done | sort -u | grep -vE '/(all|help)$' > "$doctok"

bogus=$(comm -23 "$doctok" "$invtok")
if [ -n "$bogus" ]; then
  fail=1
  echo "error: the reference docs name tokens that no inventory entry defines."
  echo "       A documented token that does nothing is how CRATONVM_PRECISE_JIT_MAPS"
  echo "       stayed in the docs for months while reading nothing:"
  echo "$bogus" | sed 's/^/         /'
fi

# ── 3. the fixture and the inventory agree ──────────────────────────────────
if [ -z "$CARGO_BIN" ] ||
   ! (cd "$ROOT" && "$CARGO_BIN" test -q -p cratonvm-types --test flag_surface >/dev/null 2>&1); then
  fail=1
  echo "error: types/tests/flag_surface.rs fails — the inventory and"
  echo "       types/tests/flag-surface.txt disagree. Run it for the diff:"
  echo "         cargo test -p cratonvm-types --test flag_surface"
fi

# ── 4. core runtime crates do not bypass the immutable config boundary ──────
#
# Non-CratonVM values still have live `std::env` semantics, but they must enter
# through `flags::runtime_var[_os]`. That function distinguishes declared VM
# flags (one immutable snapshot) from application/OS variables (live reads).
CORE_RUNTIME="reader types vm jit gc classloading native-api native-builtins native-collections"
bypasses=$(
  for c in $CORE_RUNTIME; do
    [ -d "$ROOT/$c/src" ] || continue
    grep -RnE 'std::env::var(_os)?[[:space:]]*\(' --include='*.rs' "$ROOT/$c/src" 2>/dev/null
  done | grep -v "^$ROOT/types/src/flags.rs:"
)
if [ -n "$bypasses" ]; then
  fail=1
  echo "error: core runtime code bypasses cratonvm_types::flags::runtime_var[_os]."
  echo "       VM flags must use the immutable snapshot; ordinary OS/application"
  echo "       variables retain live-read semantics through the same boundary:"
  echo "$bypasses" | sed 's/^/         /'
fi

if [ "$fail" -eq 0 ]; then
  echo "flag surface ok: $(wc -l < "$expected") variables, $(wc -l < "$invtok") tokens, 15 user-facing names"
fi
exit "$fail"
