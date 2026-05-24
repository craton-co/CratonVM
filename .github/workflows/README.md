# GitHub Actions workflows

Every workflow in this directory is **active** — it runs on the trigger(s)
declared at the top of the file. If you find yourself moving a workflow
into `.github/.wf/` to "make CI pass", please open an issue first: parking
workflows wholesale is what got the previous round of CI rot started.

## Active workflows

| File | Trigger | Purpose |
| --- | --- | --- |
| `ci.yml` | push/PR to `main`/`dev` | Build, format, clippy (`-D warnings`), test on ubuntu + windows. The baseline gate every PR must pass. |
| `cuda-bridge.yml` | push/PR touching `cuda-bridge/**` | Build the `cuda-bridge` crate with both the default stub backend AND the `cuda` feature so the real cudarc-backed driver bridge cannot silently rot against cudarc API changes. |
| `dco.yml` | PR to `main` | Enforces the Developer Certificate of Origin: every commit in a PR must carry a `Signed-off-by:` trailer. Apache-2.0 projects need this to keep the provenance chain clean. |
| `release.yml` | tag push `v*` | Cross-platform release builds (linux-gnu, windows-msvc, darwin-aarch64) of the `cratonvm` binary; uploads tarballs/zips and creates a GitHub Release with auto-generated notes. |
| `bench-gate.yml` | push to `main`/`release/*`, PR touching JIT/GC/natives/bench | Runs the criterion microbenchmark suite and fails the build if any kernel regresses > 15% against `bench/baseline.json`. |
| `hotspot-baseline.yml` | `workflow_dispatch` OR `/capture-hotspot` PR comment from an owner/member | Refreshes `bench/hotspot-baseline.json` with fresh HotSpot C2 medians (Temurin JDK 25) and opens a PR. Unblocks the T10.8.2 geomean gate. |
| `jck.yml` | push/PR to `main`, `workflow_dispatch` | Runs the JCK conformance harness IF `secrets.JCK_HOME` / `vars.JCK_HOME` is configured; otherwise emits a notice and skips. Also runs differential tests (CratonVM vs HotSpot) unconditionally. See `docs/legal.md` for JCK licensing. |
| `ejbca-smoke.yml` | push/PR touching `bench/wildfly/**` / VM / classloading, nightly 03:17 UTC | Per-PR fast smoke for the WildFly/EJBCA minimum fixture. Drift against `bench/wildfly/bench-baseline.json` fails the job; logs upload on drift. |
| `forcing-function-smoke.yml` | nightly 06:00 UTC, PR touching `bench/**` | Broader nightly drift detector over the S2 forcing-function apps (Keycloak 16/26, EJBCA, Tomcat 10, Jetty 12, Quarkus 3, SpringBoot 3, Maven, Gradle, Kafka, Cassandra). 10-min per-app timeout. |
| `soak-weekly.yml` | weekly Sunday 04:00 UTC, `workflow_dispatch` | Placeholder for the WP8.1 24h soak harness. Today only re-runs the Keycloak 16 smoke for ~30 seconds as a "harness still launches" canary. |
| `pgo-build.yml` | weekly Sunday 06:00 UTC, `workflow_dispatch` | Runs `scripts/pgo.sh` (four-phase instrument → profile → merge → rebuild) and uploads the PGO-optimised `cratonvm` binary as an artifact. |
| `t2-census.yml` | push/PR to `main` | T2.1.1 "no-default-natives" gate: re-runs the `cratonvm-vm` test suite without `synthetic-jdk` and uploads `bench/missing-natives.json` for diffing against the committed census baseline. |

## Parked workflows (`.github/.wf/`)

GitHub Actions only scans `.github/workflows/`, so anything under
`.github/.wf/` is dormant. Each parked file carries a `TODO(orchestrator)`
comment at the top explaining what would need to change before it can be
re-activated.

| File | Why parked |
| --- | --- |
| `.github/.wf/ci.yml` | `name: CI` collides with the active `ci.yml`. Activating it as-is would run two "CI" workflows on every push/PR. Merge its extra jobs (coverage, audit, miri, `cargo doc`, `Forbid debug-trace prints`) into the active file, then delete this copy. |
| `.github/.wf/jvm-smoke.yml` | The RI.1..RI.17 matrix expects pre-staged external fixtures (SPECjvm2008, DaCapo, Keycloak distros) via mirror env vars that are not yet wired into repo secrets. Without them every slot fails red on every PR. |

## Conventions

* **Path filters**: real path filters look like `'.github/workflows/foo.yml'`
  or `'crate-name/**'`. A bare `'/foo.yml'` is a leading-slash typo and
  matches nothing — guard against it in code review.
* **Cargo target features**: anything that sets `RUSTFLAGS:` MUST include
  `-C target-feature=+sse4.2,+pclmul` to match `.cargo/config.toml`,
  otherwise the CI binary diverges from the shipping binary.
* **Binary names**: `cargo build -p cratonvm-cli` produces `cratonvm`
  (and `java`), not `cratonvm-cli`. See `vm-cli/Cargo.toml` for the
  `[[bin]]` entries.
