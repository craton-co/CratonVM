# Building CratonVM

This guide consolidates the build, test, lint, and benchmark workflow for
CratonVM in one place. For a project overview see [README.md](README.md); for
contribution guidelines see [CONTRIBUTING.md](CONTRIBUTING.md); for binary
installation see [docs/INSTALL.md](docs/INSTALL.md).

## Prerequisites

- **Rust 1.77+** — install via [rustup.rs](https://rustup.rs).
- **JDK 17+** *(optional)* — only needed to compile the Java test classes and
  to boot against a real `java.base`. CratonVM runs standalone (synthetic JDK)
  without one.
- **Visual Studio Build Tools** *(Windows only)* — for the MSVC toolchain/linker.

## Building

```bash
git clone https://github.com/craton-co/cratonvm.git
cd cratonvm
cargo build --release -p cratonvm-cli
```

The package is `cratonvm-cli`, but the binary it produces is `cratonvm`, so the
executable lands at `target/release/cratonvm` (or `cratonvm.exe` on Windows).

To build the whole workspace (all member crates and their targets):

```bash
cargo build --workspace --all-targets
```

### Optional `java` binary alias

CratonVM does **not** install a `java` binary by default — that would shadow the
system JDK launcher. If you need a `java[.exe]` launcher (e.g. for Maven
Surefire's `-Djvm=...` validation, which requires the binary basename to be
`java`), opt in with the `java-bin-alias` feature:

```bash
cargo build --release -p cratonvm-cli --features java-bin-alias
# now both target/release/cratonvm and target/release/java exist
```

### GPU offload (opt-in)

The default `cargo build` produces a **CPU-only** JVM with no GPU code linked.
GPU offload is gated behind Cargo features; see
[docs/gpu/README.md](docs/gpu/README.md) for the build modes, CLI flags, and
architecture.

## Running tests

```bash
# All tests
cargo test --all

# With increased stack for deep-recursion tests
RUST_MIN_STACK=8388608 cargo test --all -- --test-threads=4
```

Tests that require `javac` skip gracefully when no JDK is on the `PATH`.

## Linting and formatting

These are the same checks CI gates on (`.github/workflows/ci.yml`, run on
`ubuntu-latest` and `windows-latest`):

```bash
cargo fmt --all --check
cargo clippy --all-targets --workspace -- -D warnings
```

> Note: the workspace `[lints]` table in the root `Cargo.toml` `allow`s
> `dead_code`/`unused_*` and a few rustdoc lints, so "0 clippy warnings" is
> relative to that configuration, not the full default lint set.

## Benchmarking

CratonVM is benchmarked against HotSpot (JDK 25 C2). A representative result set
(QuickBench, Binary Trees) and the methodology live in the README
[Benchmark section](README.md#benchmark-vs-hotspot-jdk-25-c2) and the full
26-round JIT optimization write-up in
[docs/PRESENTATION.md](docs/PRESENTATION.md). Profiling guidance is in
[docs/PROFILING.md](docs/PROFILING.md).

A typical run compiles the benchmark's Java sources with `javac`, then runs the
class under both `java` and the release `cratonvm` binary, comparing wall-clock
time. Use `--Xmx` to give larger benchmarks more heap (e.g. `--Xmx 8g` for the
binary-trees workload) and `--nojit` to isolate interpreter-only timings.

## Workspace layout

The workspace has 17 member crates plus a `fuzz` harness (18 `Cargo.toml` files
in total). See [ARCHITECTURE.md](ARCHITECTURE.md) for the detailed structure and
[CONTRIBUTING.md](CONTRIBUTING.md#project-structure) for the per-crate purpose
table.
