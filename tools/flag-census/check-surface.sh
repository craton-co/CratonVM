#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
# Copyright 2024-2026 Craton Software Company
#
# Fail if the CRATONVM_* environment surface has grown behind the config.
#
# The surface reached 692 identifiers by growing roughly one per fixed bug with
# no retirement path (flag-census.md). It is now ten grouped
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
found=$(mktemp); trap 'rm -f "$found" "$expected" "$doctok" "$invtok" "$allowed" "$unread" "$declkeys" "$readkeys"' EXIT
for c in $CRATES; do
  [ -d "$ROOT/$c/src" ] || continue
  grep -rhoE '"CRATONVM_[A-Z0-9_]+"' --include='*.rs' "$ROOT/$c/src" 2>/dev/null
done | tr -d '"' \
  | sort -u > "$found"

expected=$(mktemp)
grep -vE '^\s*(#|$)' "$FIXTURE" | tr -d '\r' | sort -u > "$expected"

# Exemptions come from `flag_declaration_guard.rs::ALLOWED`, the Rust guard
# that already has to justify every one of them in prose. This script used to
# carry its own three-name copy, and it drifted:
# `CRATONVM_COMPATIBILITY_JDK_ONLY` (a C ABI constant) and
# `CRATONVM_ALLOW_UNKNOWN_TOKENS` (read while the token expansion itself runs,
# before the snapshot it would come from exists) are exempt there and were
# reported here on every run. A guard with two standing false positives is a
# guard nobody reads, and seven real omissions reached `dev` in a single day
# while it was in that state.
allowed=$(mktemp)
sed -n '/^const ALLOWED/,/^];/p' "$ROOT/types/tests/flag_declaration_guard.rs" \
  | grep -oE '"CRATONVM_[A-Z0-9_]*"' | tr -d '"' | sort -u > "$allowed"
if [ ! -s "$allowed" ]; then
  fail=1
  echo "error: no exemptions parsed from flag_declaration_guard.rs::ALLOWED --"
  echo "       the list moved or changed shape, so this check would score every"
  echo "       exempt name as undeclared."
fi

undeclared=$(comm -23 "$found" "$expected" | comm -23 - "$allowed")
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
  done | grep -v "^$ROOT/types/src/flags.rs:" \
       | grep -v "^$ROOT/vm/src/config.rs:.*let prev = std::env::var_os(key);"
)
# `vm/src/config.rs`'s `with_env` is the SAVE half of a save/set/restore pair
# for an UNDECLARED variable, and it debug_asserts the key is not a CRATONVM_
# flag. Restoring a variable is the one job the boundary cannot do for you.
if [ -n "$bypasses" ]; then
  fail=1
  echo "error: core runtime code bypasses cratonvm_types::flags::runtime_var[_os]."
  echo "       VM flags must use the immutable snapshot; ordinary OS/application"
  echo "       variables retain live-read semantics through the same boundary:"
  echo "$bypasses" | sed 's/^/         /'
fi

# 5. every declared key is READ by something
#
# The mirror of check 1. A key can outlive its last reader: `skip_list.rs` was
# deleted outright in `d1979bec5` and `CRATONVM_JIT_UNBAN_JUNITCORE` stayed in
# the inventory and in three generated docs, offering to unban something that
# could no longer be banned. That is the `CRATONVM_PRECISE_JIT_MAPS` failure
# mode check 2 guards from the docs side, arriving from the other one.
#
# One pass over the tree, not one per key: the obvious `for key; do grep; done`
# spelling takes ten minutes on this repo and would simply be disabled.
declkeys=$(mktemp); readkeys=$(mktemp); unread=$(mktemp)
grep -oE '(on_key|off_key): Some\("[A-Z0-9_]+"\)' "$INVENTORY"   | grep -oE 'CRATONVM_[A-Z0-9_]+' | sort -u > "$declkeys"
grep -rhoE '"CRATONVM_[A-Z0-9_]+"' --include='*.rs'      --exclude-dir=target --exclude-dir=.git      --exclude='flag_groups.rs' "$ROOT" 2>/dev/null   | tr -d '"' | sort -u > "$readkeys"
comm -23 "$declkeys" "$readkeys" > "$unread"
if [ -s "$unread" ]; then
  fail=1
  echo "error: these keys are declared in INVENTORY but no Rust source reads them."
  echo "       A knob whose last reader was deleted still appears in docs/CONFIG.md"
  echo "       and in the generated tables, where it reads as a supported lever:"
  sed 's/^/         /' "$unread"
fi

if [ "$fail" -eq 0 ]; then
  echo "flag surface ok: $(wc -l < "$expected") variables, $(wc -l < "$invtok") tokens, 15 user-facing names"
fi
exit "$fail"
