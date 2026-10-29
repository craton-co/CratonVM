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

# Phase 2b (SBR round 5, lane warmup): the bench suite above exercises the Rust
# crates, not the VM running the JDK's own cold bytecode, which is where a
# `cratonvm` run spends its first second (boot, class loading and verification, the
# interpreter on code that never gets hot). When a JDK 25 is named (`PGO_JDK`, or
# `JAVA_HOME`), also run the instrumented launcher over the warm-up probes. Measured on
# the Linux host with no LTO in either arm, a launcher built with this phase against one
# built without a profile: `Sbr5WarmupHello` 380 -> 310 ms CPU, `Sbr5WarmupCold` 1130 ->
# 910 ms, held-out `--nojit` probes 5-30% (docs/feature-designs/jdk-only-mode.md
# SBR5-WARMUP-cold-start-cpu-decomposed-and-four-fixes-RETIRED-20261006.md).
PGO_JDK="${PGO_JDK:-${JAVA_HOME:-}}"
if [ -n "$PGO_JDK" ] && [ -x "$PGO_JDK/bin/javac" ]; then
    echo "[pgo] phase 2b: warm-up probes under $PGO_JDK"
    # Outside $PROFILE_DIR: phase 3 merges every file in that directory.
    PROBE_CLASSES="${PROFILE_DIR}-probe-classes"
    mkdir -p "$PROBE_CLASSES"
    "$PGO_JDK/bin/javac" -d "$PROBE_CLASSES" tools/probes/Sbr5Warmup*.java
    CV="${CV:-target/release/cratonvm}"
    probe() { JAVA_HOME="$PGO_JDK" timeout 600 "$CV" "$@" >/dev/null 2>&1 || true; }
    for _ in 1 2 3; do
        probe -cp "$PROBE_CLASSES" Sbr5WarmupHello
        probe -cp "$PROBE_CLASSES" Sbr5WarmupCold
    done
    for _ in 1 2; do probe -cp "$PROBE_CLASSES" Sbr5WarmupJrt; done
    for m in empty static virtual interface getfield putfield getstatic alloc array string hashmap lambda; do
        probe --nojit -cp "$PROBE_CLASSES" Sbr5WarmupCalls "$m" 200000
        probe -cp "$PROBE_CLASSES" Sbr5WarmupCalls "$m" 600000
    done
    for r in walk counted hidden; do probe -cp "$PROBE_CLASSES" Sbr5WarmupDownstream "$r" 100; done
else
    echo "[pgo] phase 2b skipped (set PGO_JDK or JAVA_HOME to a JDK 25 to train on the VM's own boot)"
fi

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
