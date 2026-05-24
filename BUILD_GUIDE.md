# CratonVM — Build & Run Guide

## Project Overview

CratonVM is a Java Virtual Machine written entirely in Rust with a custom x86-64 JIT compiler.

**Project stats:**
- **~323,000+ lines** of Rust across ~200+ files
- 18 Cargo.toml files (17 workspace member crates + a `fuzz` libfuzzer harness): `reader`, `types`, `native-api`, `native-collections`, `native-io`, `native-builtins`, `native-awt`, `jit-api`, `jit`, `jit-cuda`, `cuda-bridge`, `craton-gpu`, `classloading`, `gc`, `vm`, `vm-cli`, `jfr` (plus the out-of-workspace `fuzz` harness)
- **6,000+ tests** passing, **0** clippy warnings
- **~3,100+ native method** registrations
- **~7,200 lines** of JIT compiler code (custom x86-64)
- Dependencies: `thiserror 2`, `bitflags 2`, `tracing 0.1`, `clap 4`, `cesu8`, `strum`, `bitfield-struct`, `regex`, `parking_lot`, `zip`, `libloading`, `indexmap`
- Minimum Rust: 1.77, Edition 2021

---

## Prerequisites

1. **Rust toolchain** — install via [rustup.rs](https://rustup.rs)
   ```
   rustc --version    # 1.77+ required
   cargo --version
   ```

2. **JDK 17+** — for compiling test Java classes
   ```
   javac --version    # JDK 17+ required
   ```

3. **Visual Studio Build Tools** (Windows) — C++ workload for MSVC linker

---

## Building

```bash
# Full build (all crates + tests)
cargo build --all-targets

# Release build (optimized — required for benchmarks)
cargo build --release --all-targets

# Output binary
target/release/cratonvm    # or target/debug/cratonvm
```

### Optional `java` binary alias

By default, only the `cratonvm` binary is produced. To additionally
build a `java[.exe]` launcher (same code, alias name — required by
Maven Surefire's `-Djvm=...` path validation), enable the
`java-bin-alias` feature on `cratonvm-cli`:

```bash
cargo build --release -p cratonvm-cli --features java-bin-alias
# Produces both:
#   target/release/cratonvm[.exe]
#   target/release/java[.exe]
```

The alias is **off by default** so that `cargo install cratonvm-cli`
does not drop a `java` binary into `~/.cargo/bin/` that would shadow
the system JDK on the user's PATH. The helper scripts
`scripts/sync-maven-java-shim.ps1` and `tools/sync-cratonvm-maven-jdk.ps1`
both require this feature.

**Build times:**
- Clean build: ~60-120 seconds
- Incremental: ~5-15 seconds
- Disk usage: `target/` ~1-2 GB

---

## Running Tests

```bash
# All tests (recommended: increase stack size for deep recursion tests)
RUST_MIN_STACK=8388608 cargo test --all -- --test-threads=4

# Quick check
cargo test --all

# Specific crate
cargo test -p cratonvm-vm
cargo test -p cratonvm-reader

# Specific test
cargo test -p cratonvm-vm -- test_name
```

**Expected:** 6,000+ tests pass, 0 failures.

---

## Linting

```bash
# Clippy (zero warnings required)
cargo clippy --all-targets -- -D warnings

# Formatting
cargo fmt --all --check    # check only
cargo fmt --all            # auto-fix
```

---

## Running Java Programs

```bash
# Run a compiled .class file
cargo run --release -p cratonvm-cli -- --classpath . ClassName

# Run the benchmark suite
cargo run --release -p cratonvm-cli -- --classpath bench QuickBench
```

**Compile Java test classes:**
```bash
javac -source 8 -target 8 test_classes/*.java
javac -source 8 -target 8 bench/*.java
```

---

## Benchmarking

```bash
# Run QuickBench (fib42, sieve*500, matrix 500x500, arithmetic 300M)
cargo run --release -p cratonvm-cli -- --classpath bench QuickBench

# Compare with JDK
java -cp bench QuickBench          # JDK C2 (full JIT) — baseline
java -Xint -cp bench QuickBench    # JDK interpreter only
```

**Expected results (vs HotSpot JDK 25 C2):**

| Benchmark | JDK 25 C2 | CratonVM | Ratio |
|-----------|-----------|---------|-------|
| Arithmetic 300M | 889 ms | 1,676 ms | 1.89x |
| Fibonacci(42) | 1,876 ms | 2,457 ms | 1.31x |
| Sieve 100K×500 | 324 ms | 510 ms | 1.57x |
| Matrix 500×500 | 351 ms | 518 ms | 1.48x |
| **TOTAL** | **3,440 ms** | **5,161 ms** | **1.50x** |

*Measured 2026-03-31 on Windows 11, JDK 25.0.1 LTS. Round 26 JIT: OSR, loop unrolling, speculative BCE, graph-coloring regalloc.*

---

## Project Structure

```
cratonvm/
  reader/              # .class file parser
  types/               # Shared types (Value, ClassId, ObjectRef)
  native-api/          # NativeContext trait & FD table
  native-builtins/     # java.lang.* native methods
  native-collections/  # java.util.* native methods
  native-io/           # java.io/nio native methods
  native-awt/          # AWT/Swing/Java2D native peers
  jit-api/             # JIT compiler API types
  jit/                 # x86-64 / AArch64 JIT compiler
  jit-cuda/            # Java bytecode -> PTX lowering for GPU offload
  cuda-bridge/         # Thin CUDA Driver API bridge for GPU offload
  craton-gpu/          # GPU offload runtime integration
  classloading/        # Class loading & bytecode verification
  gc/                  # Garbage collectors (semi-space, G1, ZGC)
  jfr/                 # Java Flight Recorder
  vm/                  # VM runtime engine
    src/
      vm.rs              # SharedVm, NativeContextImpl, invoke_shared
      runtime/
        interpreter.rs   # Bytecode interpreter, fast-path dispatch
        frame.rs         # Frame struct with SoA locals
        value_stack.rs   # SoA operand stack
      threading/
        jvm_thread.rs    # Per-thread state
  vm-cli/              # CLI entry point
  bench/               # QuickBench.java and compiled .class
  test_classes/        # Test Java source + compiled .class
  demo/                # Demo programs
```

---

## Key Configuration

| Setting | Value | Location |
|---------|-------|----------|
| JIT threshold | 100 invocations | `interpreter.rs` |
| OSR threshold | 10,000 back-edges | `interpreter.rs` |
| GC nursery size | 16 MB | `gen_heap.rs` |
| Max stack depth | 512 frames | `VmConfig` |
| SIMD | AVX2 (runtime detected) | `x64.rs` |

---

## Generating API Documentation

```bash
cargo doc --all --no-deps --open
```

This generates browsable HTML documentation for all public APIs in `target/doc/`.

---

## Verification Checklist

After any changes:
1. `cargo build --all-targets` — compiles
2. `cargo clippy --all-targets -- -D warnings` — 0 warnings
3. `RUST_MIN_STACK=8388608 cargo test --all -- --test-threads=4` — all tests pass
4. `cargo run --release -p cratonvm-cli -- --classpath bench QuickBench` — benchmark runs
