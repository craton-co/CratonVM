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
| `release-crates-dry-run.ps1` | Packages/publish-checks crates in dependency order. |

## Tests and quality gates

| Script or tool | Purpose |
| --- | --- |
| `check-no-diag-prints.sh` | Rejects landed bring-up print markers. |
| `check-lcov-threshold.ps1` | Evaluates an existing LCOV file against an explicit threshold. |
| `jck-matrix.sh` | Regenerates the JCK status matrix. |
| `run-kc-tests.sh` | Runs a caller-supplied Keycloak class list. |
| `tools/check_markdown_links.py` | Validates maintained local Markdown links and mdBook membership. |
| `tools/flag-census/check-surface.sh` | Enforces the declared `CRATONVM_*` flag surface. |

## Benchmarking

| Script | Purpose |
| --- | --- |
| `capture-hotspot-baseline.sh` / `.ps1` | Captures the HotSpot comparison baseline. |
| `pgo.sh` / `pgo.cmd` | Four-phase profile-guided optimization build. |

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
