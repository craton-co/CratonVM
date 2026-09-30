> Please read the [contribution guide](../CONTRIBUTING.md) before opening this PR.

## Summary

Brief description of what this PR does.

## Changes

-

## Testing

- [ ] `cargo build --workspace --all-targets` run
- [ ] `cargo clippy --workspace --all-targets -- -D warnings` run
- [ ] `cargo fmt --all --check` run
- [ ] `cargo test --workspace` run
- [ ] Coverage checked or intentionally deferred; the CI job is blocking on the report being *generated* — no percentage bar is claimed or expected (see `docs/COVERAGE.md`)
- [ ] Semantic difftest smoke checked or intentionally deferred
- [ ] Fuzz build smoke checked or intentionally deferred
- [ ] Known failures or skipped checks explained
- [ ] New tests added for new functionality (if applicable)
- [ ] `CHANGELOG.md` updated under the Unreleased section (for user-facing changes)
- [ ] Release/package readiness checked against `docs/RELEASE_READINESS.md` (if release, packaging, license, notice, or public metadata changed)
- [ ] Benchmarks checked with `cargo run --release -p cratonvm-cli -- --classpath bench QuickBench` (if performance-related)

## Related Issues

Closes #
