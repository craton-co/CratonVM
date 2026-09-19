# Code coverage

CratonVM collects Rust source coverage with `cargo-llvm-cov`. Coverage measures
the VM's Rust tests, not Java application coverage or compatibility.

## Current claim

CI requires coverage generation to complete and retains `lcov.info` as an
artifact. The project does **not** claim 85% coverage—or any other minimum—until
a complete, reproducible baseline has been measured on the release CI image.
Missing coverage output fails the job.

## Local use

Install the LLVM tools and command:

```text
rustup component add llvm-tools-preview
cargo install cargo-llvm-cov
```

Generate the same report as CI:

```text
cargo llvm-cov --workspace --lcov --output-path lcov.info
```

For an explicitly chosen local threshold, the repository retains:

```text
pwsh scripts/check-lcov-threshold.ps1 -Path lcov.info -LineThreshold 85
```

That command is a measurement aid, not evidence that the repository currently
meets 85%.

## CI

`.github/workflows/coverage.yml` runs on pushes to `main` and `dev` and on pull
requests. It installs JDK 25 for VM-booting tests, generates the workspace LCOV
report without soft-fail behavior, and uploads the artifact with
`if-no-files-found: error`.

Coverage remains only one signal. The HotSpot differential suite, default and
feature test configurations, fuzz-build gate, Miri core gate, and application
compatibility work exercise risks that a line percentage cannot establish.
