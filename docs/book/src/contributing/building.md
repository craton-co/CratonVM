# Building from Source

This chapter consolidates the build, lint, and benchmark workflow. For installing
a pre-built binary instead, see [Installation](../getting-started/installation.md).

## Prerequisites

- **Rust 1.88+** — install via [rustup.rs](https://rustup.rs). CratonVM uses
  edition 2021.
- **A JDK (17+) is optional** — needed only to compile the Java test classes and
  to boot against a real `java.base`. CratonVM runs standalone without one.
- **Visual Studio Build Tools** (Windows only) — for the MSVC toolchain/linker.

## Building the launcher

```bash
git clone https://github.com/craton-co/cratonvm.git
cd cratonvm
cargo build --release -p cratonvm-cli
```

The package is `cratonvm-cli`, but the binary it produces is `cratonvm`, so the
executable lands at `target/release/cratonvm` (or `cratonvm.exe` on Windows).

Build the whole workspace (all crates and targets):

```bash
cargo build --workspace --all-targets
```

## Optional build features

| Feature | Crate | Effect |
|---------|-------|--------|
| `java-bin-alias` | `cratonvm-cli` | Also build a `java[.exe]` binary (for tools that require the launcher basename to be `java`). Off by default to avoid shadowing the system JDK. |
| `synthetic-jdk` | `cratonvm-cli` / `vm` | Force synthetic standard-library mode at compile time. |
| `gpu` / `gpu-driver` | `cratonvm-cli` | Enable GPU offload plumbing (stub) / real CUDA driver. See [GPU Offload](../gpu/overview.md). |
| `mimalloc` | `cratonvm-cli` | Use mimalloc as the global allocator (on by default; faster on Windows). |
| `awt` | `vm` | AWT/Swing/Java2D natives (on by default). |
| `zgc` | `cratonvm-cli` → `vm` → `gc` | Compile the ZGC backend in. **On by default since 2026-08-10**, because it gates the `GcAlgorithm::Zgc` variant and that variant is now the default collector. Turning it off (`--no-default-features`) leaves a launcher where `-XX:+UseZGC` warns and falls back to Generational. It can **mark concurrently** since 2026-08-16, opt-in via `CRATONVM_ZGC_CONC_START=60`; the sweep and any relocation are still stop-the-world, and it is still not production ZGC — see [The Garbage Collector](../internals/garbage-collector.md#collectors). |

```bash
# A java[.exe] alias alongside cratonvm
cargo build --release -p cratonvm-cli --features java-bin-alias

# GPU offload with the real CUDA driver
cargo build --release -p cratonvm-cli --features gpu-driver

# A launcher with NO ZGC at all: Generational becomes the default and
# `-XX:+UseZGC` warns and falls back. `--no-default-features` also drops
# `mimalloc`, so name it back unless you mean to drop that too.
cargo build --release -p cratonvm-cli --no-default-features --features mimalloc
```

The default `cargo build` produces a CPU-only JVM with **no** GPU code linked.

## Linting & formatting

These are the lint and formatting checks CI is configured to run on Linux and
Windows:

```bash
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
```

> The workspace `[lints]` table allows `dead_code`/`unused_*` and a few rustdoc
> lints, so `clippy -D warnings` measures the repository's configured lint
> policy, not the full default lint set. Do not describe a branch as
> warning-free unless the current clippy job actually passes.

## Benchmarking

A typical run compiles a benchmark's Java sources with `javac`, then runs the
class under both `java` and the release `cratonvm` binary, comparing wall-clock
time:

```bash
cargo build --release -p cratonvm-cli
mkdir -p .bench-cache/quickbench
git show 2cea208:bench/QuickBench.java > .bench-cache/quickbench/QuickBench.java
git show 2cea208:bench/binarytrees.java > .bench-cache/quickbench/binarytrees.java
javac -d .bench-cache/quickbench .bench-cache/quickbench/QuickBench.java .bench-cache/quickbench/binarytrees.java
target/release/cratonvm --classpath .bench-cache/quickbench QuickBench
java -cp .bench-cache/quickbench QuickBench
```

Give larger benchmarks more heap (e.g. `--Xmx 8g` for Binary Trees) and use
`--nojit` to isolate interpreter-only timings. See
[Benchmarks](../performance/benchmarks.md) and [Profiling](../performance/profiling.md).

## Next

- [Testing](testing.md) — running the test and regression suites.
- [Contributing Guide](contributing.md) — workflow and how to add opcodes/natives.
