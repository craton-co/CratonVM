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
Windows. Coverage and semantic difftest jobs are advisory today; check the
current Actions run before treating a branch as release-ready.

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

The fuzz workspace is not a blocking CI gate today, and the current review
tracks a build blocker around the `legacy-synthetic-crypto` feature until the
native-builtins feature declarations and fuzz manifest are aligned.

## Writing tests

- Add unit tests in the same file as the code (`#[cfg(test)]`).
- Integration tests that need `javac` should skip gracefully if it isn't
  available.
- Use `RUST_MIN_STACK=8388608` for tests that recurse deeply.
- Prefer real `.java` fixtures compiled by `javac` over hand-rolled bytecode
  arrays.

## Code coverage

Coverage is generated with `cargo-llvm-cov` locally and in an advisory CI job.
No minimum coverage threshold is enforced today. See the repository's coverage
documentation for invocation details.
