# Code Coverage

CratonVM measures Rust test coverage with [`cargo-llvm-cov`](https://github.com/taiki-e/cargo-llvm-cov),
which drives LLVM source-based coverage instrumentation behind a Cargo
subcommand. A CI job generates an LCOV report on every push and pull request.

Coverage here means **Rust unit/integration test coverage of the VM itself**,
not Java-level coverage of the programs CratonVM runs.

## Coverage Target

Release-ready branches should demonstrate **at least 85% line coverage** for
the Rust workspace, but this is an advisory target today. The current CI
coverage job runs a target checker, yet both generation and target-check steps
are marked `continue-on-error`; no coverage threshold is enforced by CI.

Use this posture until the gate is promoted:

- Treat coverage below 85% as a readiness gap that needs a note in the PR.
- Treat missing or incomplete coverage output as a release-readiness blocker,
  even though CI currently reports it as advisory.
- Promote the CI job to blocking only after workspace tests compile/run
  reliably under coverage and the hosted runner has the required JDK setup.

## Installing Locally

`cargo-llvm-cov` needs the `llvm-tools-preview` rustup component:

```bash
rustup component add llvm-tools-preview
cargo install cargo-llvm-cov
```

## Generating A Report

Run over the whole workspace. Pick the output format you want:

```bash
# Terminal summary (per-file line/region/function coverage)
cargo llvm-cov --workspace

# LCOV file, same format CI uploads
cargo llvm-cov --workspace --lcov --output-path lcov.info

# Browsable HTML report under target/llvm-cov/html/
cargo llvm-cov --workspace --html
```

`--workspace` covers every crate listed in [CONTRIBUTING.md](../CONTRIBUTING.md#project-structure)
(`reader`, `vm`, `jit`, the `native-*` crates, etc.). To scope to one crate,
swap `--workspace` for `-p <crate>`, e.g. `cargo llvm-cov -p reader`.

## Checking The 85% Advisory Target

For local or CI LCOV output, use the repository checker:

```powershell
pwsh scripts/check-lcov-threshold.ps1 -Path lcov.info -LineThreshold 85
```

`cargo-llvm-cov` can also enforce the target directly in a local or future
blocking job when the coverage run itself is healthy enough to be treated as a
gate:

```bash
cargo llvm-cov --workspace --fail-under-lines 85
```

The LCOV checker is useful when coverage generation and threshold evaluation
need to be separate steps, such as the current advisory workflow.

## Reading The Report

- **Terminal / LCOV**: each row reports `Lines`, `Regions`, and `Functions`
  covered. Regions are LLVM coverage regions, so region coverage is usually
  lower than line coverage.
- **HTML** (`--html`): open `target/llvm-cov/html/index.html`. Uncovered lines
  are highlighted red, partially covered regions amber.
- **LCOV** (`lcov.info`): a machine-readable format. VS Code's Coverage
  Gutters extension and tools like `genhtml` consume it directly.

## CI Job

`.github/workflows/coverage.yml` defines a `cargo-llvm-cov` job that:

- **Triggers** on push and pull request to `main` and `dev`.
- **Runs** on `ubuntu-latest`: installs the stable toolchain with
  `llvm-tools-preview`, installs `cargo-llvm-cov`, then runs
  `cargo llvm-cov --workspace --lcov --output-path lcov.info`.
- **Checks** `lcov.info` with `scripts/check-lcov-threshold.ps1` against the
  85% line coverage target in advisory mode.
- **Uploads** `lcov.info` as a build artifact so you can inspect coverage for a
  given commit.

### Advisory By Design

The coverage generation and threshold steps are marked `continue-on-error:
true`, so they are **advisory**. A failed or incomplete coverage run will not
turn the build red yet.

The reason: some workspace tests boot the VM and run real Java, which needs a
JDK runtime image and stable test compilation under coverage. Once the hosted
job has those prerequisites and produces stable results, remove
`continue-on-error` from both coverage steps before describing 85% line coverage
as a blocking gate.

The normal correctness gates (format, clippy, build, test) still run separately
in `.github/workflows/ci.yml`; see
[CONTRIBUTING.md](../CONTRIBUTING.md#before-submitting).

## Limitations / Caveats

- **JDK-dependent tests skew the numbers.** Tests that require `javac` or a JDK
  runtime skip when it is absent. In a JDK-less environment, those code paths
  show up as uncovered.
- **The app gauntlet is not covered.** Coverage measures Rust tests only. The
  upstream Java applications CratonVM is validated against run through the CLI,
  not the Rust test harness.
- **Line coverage is the advisory readiness target.** Prefer region coverage
  when hunting for untested branches, but do not describe any coverage threshold
  as enforced until CI is promoted out of advisory mode.
