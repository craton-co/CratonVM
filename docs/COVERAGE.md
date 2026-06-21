# Code Coverage

CratonVM measures Rust test coverage with [`cargo-llvm-cov`](https://github.com/taiki-e/cargo-llvm-cov),
which drives the LLVM source-based coverage instrumentation behind a thin Cargo subcommand.
A CI job generates an LCOV report on every push and pull request.

Coverage here means **Rust unit/integration test coverage of the VM itself** — not Java-level
coverage of the programs CratonVM runs.

## Installing locally

`cargo-llvm-cov` needs the `llvm-tools-preview` rustup component:

```bash
rustup component add llvm-tools-preview
cargo install cargo-llvm-cov
```

## Generating a report

Run over the whole workspace. Pick the output format you want:

```bash
# Terminal summary (per-file line/region/function coverage)
cargo llvm-cov --workspace

# LCOV file — same command CI runs; feed it to editors/Codecov/genhtml
cargo llvm-cov --workspace --lcov --output-path lcov.info

# Browsable HTML report under target/llvm-cov/html/
cargo llvm-cov --workspace --html
```

`--workspace` covers every crate listed in [CONTRIBUTING.md](../CONTRIBUTING.md#project-structure)
(`reader`, `vm`, `jit`, the `native-*` crates, etc.). To scope to one crate, swap `--workspace`
for `-p <crate>`, e.g. `cargo llvm-cov -p reader`.

## Reading the report

- **Terminal / LCOV** — each row reports `Lines`, `Regions`, and `Functions` covered. Regions
  are LLVM coverage regions (finer than lines: separate branches of an expression count
  separately), so region coverage is usually lower than line coverage.
- **HTML** (`--html`) — open `target/llvm-cov/html/index.html`. Uncovered lines are highlighted
  red, partially covered regions amber. This is the easiest view for spotting an untested branch.
- **LCOV** (`lcov.info`) — a machine-readable format. VS Code's *Coverage Gutters* extension
  and tools like `genhtml` consume it directly.

## CI job

`.github/workflows/coverage.yml` defines a `cargo-llvm-cov` job that:

- **Triggers** on push and pull request to `main` and `dev`.
- **Runs** on `ubuntu-latest`: installs the stable toolchain with `llvm-tools-preview`,
  installs `cargo-llvm-cov`, then runs
  `cargo llvm-cov --workspace --lcov --output-path lcov.info`.
- **Uploads** the resulting `lcov.info` as a build artifact (`actions/upload-artifact`),
  so you can download it from the workflow run and inspect coverage for a given commit.

### Non-blocking by design

The coverage step is marked `continue-on-error: true`, so it is **advisory** — a failed or
incomplete coverage run will **not** turn the build red. The reason: some workspace tests boot
the VM and run real Java, which needs a JDK runtime image the coverage job does not provision.
Those tests skip gracefully when no JDK is present, but a run that exercises them can still fail.
Once a JDK is wired into this job, the `continue-on-error` flag can be dropped to make coverage a
blocking gate.

The normal correctness gates (format, clippy, build, test) still run separately in
`.github/workflows/ci.yml` and **are** blocking — see
[CONTRIBUTING.md](../CONTRIBUTING.md#before-submitting).

## Limitations / caveats

- **JDK-dependent tests skew the numbers.** Tests that require `javac`/a JDK runtime skip when
  it is absent. In a JDK-less environment (including the current CI coverage job) the code paths
  they exercise show up as uncovered, so the reported percentage understates real coverage. For
  a more representative local number, run with a JDK 17+ installed and on `PATH`.
- **The app gauntlet is not covered.** Coverage measures Rust tests only. The 50+ upstream Java
  applications CratonVM is validated against (Spring, Kafka, Tomcat, Elasticsearch, …) run
  through the CLI, not the Rust test harness, so they contribute nothing to these figures.
- **No coverage threshold is enforced.** There is no minimum-coverage gate; the report is
  informational. Don't treat a coverage delta as a merge blocker.
- **Region vs. line.** Prefer region coverage when hunting for untested branches — line coverage
  can read "100%" on a line whose error branch is never taken.
