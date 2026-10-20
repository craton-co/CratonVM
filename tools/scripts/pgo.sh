#!/usr/bin/env bash
# Round-9 cross-cutting HIGH-3: profile-guided optimization (PGO) recipe.
#
# The release profile already enables fat LTO + codegen-units=1, but cargo
# does not run PGO out of the box. This script wires the four-phase
# instrument -> profile -> merge -> rebuild flow that the Rust toolchain
# expects, pinning the same `+sse4.2,+pclmulqdq` target features the CI
# workflows use so the binary produced here matches what CI ships.
#
# Requirements:
#   - llvm-tools-preview component: `rustup component add llvm-tools-preview`
#   - `llvm-profdata` on PATH (ships under
#     `$(rustc --print sysroot)/lib/rustlib/<host>/bin/llvm-profdata`;
#     symlink into PATH or call with the absolute sysroot path).
#
# Usage:
#   bash scripts/pgo.sh
#
# Output: the `target/release/` binaries are rebuilt with PGO applied,
# replacing whatever was last linked there. The merged profile data is
# left in $PROFILE_DIR for inspection.

set -euo pipefail

PROFILE_DIR="${PROFILE_DIR:-/tmp/cratonvm-pgo}"
TARGET_FEATURES="+sse4.2,+pclmulqdq"

echo "[pgo] resetting profile directory: $PROFILE_DIR"
rm -rf "$PROFILE_DIR"
mkdir -p "$PROFILE_DIR"

# Phase 1: instrumented build. `profile-generate` injects coverage probes
# into every codegen unit; the resulting binaries are slower but emit
# `.profraw` files into $PROFILE_DIR when run.
echo "[pgo] phase 1/4: instrumented build"
RUSTFLAGS="-C profile-generate=$PROFILE_DIR -C target-feature=$TARGET_FEATURES" \
    cargo build --release --workspace

# Phase 2: collect profile data by running a representative workload.
# `cargo bench --quick` drives criterion through one sample per group,
# which is enough to exercise interpreter dispatch, GC, JIT, native
# dispatch, and string creation without spending the full bench budget.
echo "[pgo] phase 2/4: collect profile data via bench suite"
RUSTFLAGS="-C profile-generate=$PROFILE_DIR -C target-feature=$TARGET_FEATURES" \
    cargo bench --workspace -- --quick || true

# Phase 3: merge the per-process `.profraw` shards into a single
# `.profdata` file the optimizer can consume. llvm-profdata ships with
# the `llvm-tools-preview` rustup component.
echo "[pgo] phase 3/4: merge profiles"
llvm-profdata merge -o "$PROFILE_DIR/merged.profdata" "$PROFILE_DIR"

# Phase 4: optimized rebuild. `pgo-warn-missing-function` surfaces any
# function the profile failed to cover (often dead code or natives that
# never fired during bench); investigate those rather than ignoring.
echo "[pgo] phase 4/4: optimized rebuild"
RUSTFLAGS="-C profile-use=$PROFILE_DIR/merged.profdata -C llvm-args=-pgo-warn-missing-function -C target-feature=$TARGET_FEATURES" \
    cargo build --release --workspace

echo "[pgo] done; binaries in target/release/ now PGO-optimized"
