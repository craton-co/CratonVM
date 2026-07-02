# Testing

CratonVM has a layered test strategy: Rust unit and integration tests, Java
integration classes run through the full pipeline, a fast HotSpot-differential
regression suite, and a fuzzing harness.

## Running the tests

```bash
# Everything
cargo test --all

# With a larger stack for deep-recursion tests
RUST_MIN_STACK=8388608 cargo test --all -- --test-threads=4
```

Tests that require `javac` skip gracefully when no JDK is on the `PATH`.

The CI pipeline is configured to run `cargo fmt --check`, `cargo build`,
`cargo clippy -D warnings`, and `cargo test` across the workspace on Linux and
Windows. Coverage, semantic difftest, and fuzz smoke jobs are advisory today;
check the current Actions run before treating a branch as release-ready.

## Local CI Checklist

For a release-readiness pass, record the result of each layer:

```bash
cargo fmt --all --check
cargo build --workspace --all-targets
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace --all-targets --no-fail-fast
cargo llvm-cov --workspace --lcov --output-path lcov.info
```

Then check the coverage target:

```powershell
pwsh scripts/check-lcov-threshold.ps1 -Path lcov.info -LineThreshold 85
```

The repository currently treats **85% line coverage** as the release-readiness
target. If a branch cannot produce a whole-workspace LCOV report, or reports
below 85%, call that out explicitly instead of treating the coverage job as
green.

## Test layers

| Layer | Where | What it covers |
|-------|-------|----------------|
| Unit tests | `#[cfg(test)]` modules beside the code | Individual functions and data structures. |
| Integration tests | `vm/tests/` | Compiled `.class` files run through the full VM pipeline. |
| Java test classes | `test_classes/`, `OUT_DIR/test-classes`, and test resources | Real Java sources compiled by `build.rs` when `javac` is available. Generated classes are staged in `OUT_DIR/test-classes` and exposed as `CRATONVM_TEST_CLASSES_DIR`; committed fixtures remain under test resources. |
| Regression suite | `regression-suite/` | A fast, deterministic set of Java classes diffed against HotSpot. |
| Differential harness | `difftest/` | Differential testing against a reference JDK. |
| Fuzzing | `fuzz/` (separate workspace) | libFuzzer harness, nightly-only. |

## The HotSpot-differential regression suite

A fast, deterministic suite of Java classes exercises the VM's critical paths
(JIT/GC, collections, strings, serialization, crypto, exceptions, reflection)
and **diffs CratonVM's output against HotSpot**. It runs in seconds and is the
quick "is the VM still healthy?" check.

```bash
# Build target/release/cratonvm first, then:
bash regression-suite/run.sh
```

It exits non-zero on any regression, so it's CI-ready. See the suite's own README
for what each class covers and how to add one.

## Fuzzing

The `fuzz/` directory is a separate, standalone workspace (a libFuzzer harness)
that is *not* a workspace member, because its `#![no_main]` harness trips the
production lints. It currently declares 11 targets. Build it on nightly:

```bash
cargo +nightly fuzz build
```

For a bounded smoke of an individual target, use a short run budget, for
example:

```bash
cargo +nightly fuzz run fuzz_classfile -- -runs=128
```

The fuzz workspace is not a blocking CI gate today. CI runs an advisory fuzz
build smoke so target drift is visible, and the current review tracks a build
blocker around the `legacy-synthetic-crypto` feature until the native-builtins
feature declarations and fuzz manifest are aligned.

## Semantic Differential Smoke

`cratonvm-difftest` compares CratonVM behavior with a reference JDK over the
seed corpus. The CI gate is advisory while the ledger is stabilized on hosted
runners. A local smoke run should use JDK 25 as the oracle:

```bash
cargo run -p cratonvm-difftest --bin cratonvm-difftest -- gate --corpus difftest/seeds
```

Exit code `0` means no new divergence was found. Exit code `1` or `2` indicates
new drift or reopened fixed behavior. Exit code `3` is bootstrap/non-fatal
when prerequisites such as `java` or the corpus are missing.

## Writing tests

- Add unit tests in the same file as the code (`#[cfg(test)]`).
- Integration tests that need `javac` should skip gracefully if it isn't
  available.
- Use `RUST_MIN_STACK=8388608` for tests that recurse deeply.
- Prefer real `.java` fixtures compiled by `javac` over hand-rolled bytecode
  arrays.

## Code coverage

Coverage is generated with `cargo-llvm-cov` locally and in an advisory CI job.
Release-ready branches are expected to meet the 85% line coverage target. See
the repository's coverage documentation for invocation details and the current
advisory-to-blocking promotion checklist.
