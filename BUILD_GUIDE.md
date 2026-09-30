# Building CratonVM

This guide consolidates the build, test, lint, and benchmark workflow for
CratonVM in one place. For a project overview see [README.md](README.md); for
contribution guidelines see [CONTRIBUTING.md](CONTRIBUTING.md); for binary
installation see [docs/INSTALL.md](docs/INSTALL.md).

## Prerequisites

- **Rust 1.88+** — install via [rustup.rs](https://rustup.rs).
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

### Building with GPU offload

GPU offload is entirely opt-in and gated behind Cargo features on
`cratonvm-cli`. There are three build levels; the exact feature chain lives
in [`vm-cli/Cargo.toml`](vm-cli/Cargo.toml):

| Command | Feature chain | What you get |
| --- | --- | --- |
| `cargo build --release -p cratonvm-cli` | *(default)* | **CPU-only.** No GPU code linked, no `--gpu*` flags — byte-identical to a build of a tree that never had the feature. |
| `cargo build --release -p cratonvm-cli --features gpu` | `gpu = ["dep:cuda-bridge", "cratonvm-vm/gpu-offload"]` | Stub mode. Links `cuda-bridge` (its no-driver `backend_stub.rs`) and exposes the `--gpu*` CLI flags, but every device probe returns `DeviceError::NoDriver`. Lets you exercise the GPU plumbing — CLI parsing, the analyze/cache/dispatch code paths, tests — on a machine with no NVIDIA hardware. |
| `cargo build --release -p cratonvm-cli --features gpu-driver` | `gpu-driver = ["gpu", "cuda-bridge/cuda"]` | Real CUDA. Compiles `cuda-bridge`'s `backend_cuda.rs` against `cudarc` 0.13 (`features = ["driver", "cuda-12060"]`, see [`cuda-bridge/Cargo.toml`](cuda-bridge/Cargo.toml)). |

The `gpu-driver` build needs the CUDA Toolkit's headers/import libs
available at *build* time. At *run* time it does **not** statically link a
CUDA runtime — `cudarc` dynamically loads the NVIDIA driver
(`nvcuda.dll` on Windows, `libcuda.so` on Linux) the first time the device
is probed, so the binary it produces only works on a machine with a
current NVIDIA driver installed.

Because `gpu-driver` compiles a different feature set than a plain build,
building it into the default `target/` directory would invalidate the
incremental-build cache for `cratonvm-vm`, `cratonvm-gc`, and friends every
time you switched between a CPU build and a GPU build. This repo avoids
that by pointing the GPU build at its own target directory —
[`scripts/build-gpu.bat`](scripts/build-gpu.bat):

```bat
set "CARGO_TARGET_DIR=C:\craton\CratonVM\target-gpu"
cargo build --release -p cratonvm-cli --bin cratonvm --features gpu-driver
```

(non-Windows equivalent: `CARGO_TARGET_DIR=target-gpu cargo build --release
-p cratonvm-cli --features gpu-driver`). The GPU binary then lands at
`target-gpu/release/cratonvm[.exe]`, and an ordinary `cargo build`'s
`target/release/cratonvm` is left untouched — you can rebuild either one
without invalidating the other's cache.

**Runtime prerequisites (`gpu-driver` binary only):** an NVIDIA GPU with a
current driver on the system search path. No CUDA Toolkit install is
required on the machine that *runs* the binary — only on the machine that
*builds* it. In-repo validation used an RTX 2060 (sm_75) on Windows 11 with
driver 591.86.

**Smoke test:**

```
target-gpu/release/cratonvm --gpu-info
```

probes the device, prints its name / compute capability / memory, and
exits without booting the JVM. If `--gpu` is requested (on any build) and
no driver is found, the flag is silently demoted and the JVM runs on CPU
instead.

See [bench-gpu/run-gpu-comparison.sh](bench-gpu/run-gpu-comparison.sh) for
the CratonVM-CPU vs. CratonVM-GPU vs. HotSpot vs. TornadoVM benchmark
suite, and [docs/gpu/README.md](docs/gpu/README.md) for the full
architecture and CLI reference.

## Running tests

```bash
# All tests
cargo test --all

# With increased stack for deep-recursion tests
RUST_MIN_STACK=8388608 cargo test --all -- --test-threads=4
```

Tests that require `javac` skip gracefully when no JDK is on the `PATH`.

### Demanding that those skips did not happen

Most of `vm/tests` drives a real `cratonvm` binary, and a few also need a real
JDK. When a prerequisite is missing, those tests print a note and return — and
cargo then reports `test ... ok`, which is **indistinguishable from a real
pass**. A whole suite can report green having asserted nothing. This is not
hypothetical: a build cut off mid-link once left two JIT tests reporting
`ok ... finished in 0.00s` with zero coverage, and the duration was the only
tell.

Set `CRATONVM_REQUIRE_E2E=1` to turn every such skip into a failure naming what
was missing:

```bash
cargo build --release -p cratonvm-cli
CRATONVM_REQUIRE_E2E=1 cargo test -p cratonvm-vm
```

Leave it unset for ordinary development — default behaviour is unchanged, so a
contributor with no JDK and no build is never blocked. `0` and the empty string
also read as unset. See [`vm/tests/common/mod.rs`](vm/tests/common/mod.rs).

## Linting and formatting

These are the same checks CI is configured to run
(`.github/workflows/ci.yml`, on `ubuntu-latest` and `windows-latest`):

```bash
cargo fmt --all --check
cargo clippy --all-targets --workspace -- -D warnings
```

> Note: the workspace `[lints]` table in the root `Cargo.toml` `allow`s
> `dead_code`/`unused_*` and a few rustdoc lints, so this command measures
> the repository's configured lint policy, not the full default lint set.

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

The workspace has 22 member crates (including `libcratonvm`, `cratonvm-embed`,
and `cratonvm-difftest`); the `fuzz` harness is a separate, standalone
workspace, not a member. See [ARCHITECTURE.md](ARCHITECTURE.md) for the detailed structure and
[CONTRIBUTING.md](CONTRIBUTING.md#project-structure) for the per-crate purpose
table.
