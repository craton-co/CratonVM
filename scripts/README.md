# Repository automation

Maintained automation is split by intent:

- `scripts/` contains operator-facing build, test, benchmark, and release commands.
- `tools/` contains repository validation, code-generation, and maintenance helpers.
- `scripts/internal/` is gitignored and reserved for machine-specific or
  worktree-specific experiments. Maintained documentation must not depend on it.

## Build and packaging

| Script | Purpose |
| --- | --- |
| `build-cpu.bat` | Standard CPU release build in `target/release`. |
| `build-cpu-java.bat` | Builds the opt-in `java.exe` launcher alias. |
| `build-debug.bat` | Release code generation with debug line information. |
| `build-gpu.bat` | GPU-driver build in `target-gpu`. |
| `build-libcratonvm.ps1` | Builds the embedding libraries and C acceptance harnesses. |
| `find-vcvars.bat` | Locates the MSVC toolchain (`VCVARS64`) the `build-*.bat` scripts and `.github/workflows/cross-platform.yml` depend on. |
| `release-crates-dry-run.ps1` | Packages/publish-checks crates in dependency order. |

## Running an app

| Script | Purpose |
| --- | --- |
| `run-app.sh` / `run-app.ps1` | Generic runner for anything under `apps/`: builds a classpath (from `--cp-file`, an app-local `craton-testcp.txt`, or `target/classes` + `target/test-classes` + `lib/*.jar`), runs one main class or jar under `cratonvm` or (`--vm hotspot`) a real JDK, and captures stdout/stderr/exit code/elapsed time. This is the common operation every `apps/*-suite-runner/` script duplicates in its own bespoke way; it does not replace them — framework-specific test discovery, sharding, and categorization stay in those scripts, which continue to exist locally under `apps/` (gitignored, no longer tracked in git). |

## Tests and quality gates

| Script or tool | Purpose |
| --- | --- |
| `check-no-diag-prints.sh` | Rejects landed bring-up print markers. |
| `check-lcov-threshold.ps1` | Evaluates an existing LCOV file against an explicit threshold. |
| `gc-flake-gate.sh` | GC flake-rate gate (`.github/workflows/ci.yml`). |
| `jck-matrix.sh` | Regenerates the JCK status matrix. |
| `merge-parse-check.sh` | Merge-conflict-marker / parse-sanity gate (`.github/workflows/ci.yml`). |
| `run-kc-tests.sh` | Runs a caller-supplied Keycloak class list. |
| `stale-receiver-audit.py` | Stale-receiver-address audit across every native crate (`.github/workflows/stale-receiver-audit.yml`). |
| `untyped-alloc-ratchet.sh` | Untyped-allocation-site ratchet (`.github/workflows/untyped-alloc-ratchet.yml`). |
| `tools/check_markdown_links.py` | Validates maintained local Markdown links and mdBook membership. |
| `tools/flag-census/check-surface.sh` | Enforces the declared `CRATONVM_*` flag surface. |

## Benchmarking

| Script | Purpose |
| --- | --- |
| `capture-hotspot-baseline.sh` / `.ps1` | Captures the HotSpot comparison baseline. |
| `pgo.sh` / `pgo.cmd` | Four-phase profile-guided optimization build. |
| `ab-native-funnel.sh` / `.ps1` | Interleaved A-B-B-A comparison of two `cratonvm` binaries on one native-call-funnel probe (order flipped each pass, so a monotone host-load drift cancels rather than favoring one arm). |
| `ab-native-funnel-report.py` | Summarizes `ab-native-funnel.sh` output into a per-rung A/B report. |

## `--jdk-only` audit toolkit

`--jdk-only` is an internal diagnostic, not a supported runtime mode — see
[`docs/README.md`](../docs/README.md#what-is---jdk-only-mode). These scripts
produce and adjudicate the data its CI gates and known-issue write-ups draw on.

CI gates:

| Script | Purpose |
| --- | --- |
| `jdk-only-census.sh` | Runtime census driving the synthetic-stub ratchet (`.github/workflows/ci.yml`); dumps publish under `target/jdk-only-audit/`. |
| `jdk-only-blast-radius.sh` | Blast-radius comparison gate (`.github/workflows/jdk-only-blast-radius.yml`). |
| `jdk-only-strict-probes.sh` | Strict-corpus probe gate (`.github/workflows/ci.yml`). |

Supporting analysis (not CI gates themselves — they produce or adjudicate the
data the gates above, and the known-issue write-ups under
[`docs/known-issues/jdk-only/`](../docs/known-issues/jdk-only/), check):

| Script | Purpose |
| --- | --- |
| `jdk-only-adjudicate.py` | Classifies a census run's violations against the current adjudication rules. |
| `jdk-only-bench.sh` | Non-regression benchmark harness for `--jdk-only` runs. |
| `jdk-only-bridge-ratchet.py` | Tracks the `SyntheticStub` → `Bridge`/`Intrinsic` promotion ratchet. |
| `jdk-only-dead-sweep.py` | Finds registrations with no live call site. |
| `jdk-only-image-method-index.py` / `jdk-only-image-method-sweep.py` | Cross-reference registrations against a real JDK image's actual method set. |
| `jdk-only-inherited-decl.sh` | Checks inherited-declaration coverage. |
| `jdk-only-interception.py` | Reports which dispatch doors a registration is actually reachable from. |
| `jdk-only-kind-map.py` | Maps every registration to its native kind (`SyntheticStub` / `Bridge` / `Intrinsic`). |
| `jdk-only-measure-refusals-and-overlays.sh` | Measures refusal and overlay counts. |
| `jdk-only-no-image-methods.py` / `jdk-only-no-image-receivers.py` | Find registrations with no counterpart in a real JDK image. |
| `jdk-only-platform-diff.py` | Diffs the registration surface across platforms. |
| `native-registration-adjudication.py` / `native-registration-census.sh` | General (not `--jdk-only`-specific) native registration census and adjudication. |
| `nio-concrete-receiver-audit.py` | Cross-crate coverage audit for abstract-to-concrete receiver relocations; consumes a `--dump-native-registry` JSON dump. |
| `cratonvm-prefix-args.sh` | A `cratonvm`-binary stand-in that prepends a fixed VM flag set, for measuring `--jdk-only` criteria through suite runners that don't take extra flags directly. |

`scripts/baselines/` holds the checked-in comparison data these ratchet
against: per-class JDK method inventories, blast-radius baselines, the
bridge-ratchet JSON, and stale-receiver/untyped-alloc site lists.

## Maven launcher shim

`tools/sync-cratonvm-maven-jdk.ps1` is the canonical implementation. It copies
the selected `target/<profile>/java.exe` into
`target/cratonvm-maven-jdk/bin/java.exe`, which satisfies Maven Surefire's JDK
path check. `scripts/sync-maven-java-shim.ps1` is a compatibility wrapper and
must not carry separate behavior.

## Framework smoke tests

The `scripts/smoke/` directory contains the numbered SPECjvm, DaCapo,
Commons Lang, Jackson, logging, Tomcat, Jetty, Spring Boot, Hibernate, Netty,
Kotlin, Scala, Groovy, Quarkus, and Keycloak smoke lanes. `common.sh` owns their
shared setup; keep per-framework scripts declarative.

Before adding a script, decide whether it is an operator workflow (`scripts/`)
or a repository-maintenance primitive (`tools/`), document it here, and add it
to CI when it represents a required gate. Release criteria live in
[`docs/RELEASE_READINESS.md`](../docs/RELEASE_READINESS.md).
