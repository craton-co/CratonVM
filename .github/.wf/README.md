# GitHub Actions workflows — PARKED area

This directory (`.github/.wf/`) is the **parked / staging** area. GitHub
Actions only scans `.github/workflows/`, so **nothing in `.github/.wf/` runs**.
A workflow lives here while it still needs external fixtures, secrets, or a
baseline before it can be promoted. To activate one, MOVE it into
`.github/workflows/` (do not just copy — see the name-collision note below).

If you find yourself moving an *active* workflow back into `.github/.wf/` to
"make CI pass", please open an issue first: parking workflows wholesale is what
got the previous round of CI rot started.

## Promoted to `.github/workflows/` (now ACTIVE)

These were moved out of `.github/.wf/` into `.github/workflows/` and now run on
their declared triggers. They are listed here only for reference — the live
copies are under `.github/workflows/`.

| File | Trigger | Purpose |
| --- | --- | --- |
| `ci.yml` | push/PR to `main`/`dev` | Build, format, clippy (`-D warnings`), test on ubuntu + windows. The baseline gate every PR must pass. |
| `cuda-bridge.yml` | push/PR touching `cuda-bridge/**` | Build the `cuda-bridge` crate with both the default stub backend AND the `cuda` feature so the real cudarc-backed driver bridge cannot silently rot against cudarc API changes. |
| `dco.yml` | PR to `main` | Enforces the Developer Certificate of Origin: every commit in a PR must carry a `Signed-off-by:` trailer. Apache-2.0 projects need this to keep the provenance chain clean. |
| `release.yml` | tag push `v*` | Cross-platform release builds (linux-gnu, windows-msvc, darwin-aarch64) of the `cratonvm` binary; uploads tarballs/zips and creates a GitHub Release with auto-generated notes. |

### Name-collision note (`name: CI`)

There were two workflows declaring `name: CI`: the baseline gate (this
directory's former `ci.yml`, now promoted to `.github/workflows/ci.yml`) and a
heavier variant under `.github/_disabled-workflows/ci.yml` (adds macOS, JDK 25,
`cargo doc`, coverage, audit, and miri). Only the baseline gate was promoted, so
exactly one "CI" workflow is live and there is no collision. The
`_disabled-workflows/` copy stays parked; to revive its extra jobs, merge them
into the live `.github/workflows/ci.yml` rather than activating a second `CI`.

## Still parked here (`.github/.wf/`) — need fixtures/secrets/baselines

Each parked file should carry a comment near the top explaining what must change
before it can be promoted.

| File | Why parked |
| --- | --- |
| `bench-gate.yml` | Criterion regression gate needs a committed `bench/baseline.json` and stable runner timings before it can block PRs. |
| `hotspot-baseline.yml` | `workflow_dispatch` / `/capture-hotspot` flow needs a Temurin JDK 25 capture environment and write access to open the baseline PR. |
| `jck.yml` | JCK conformance harness needs `secrets.JCK_HOME` / `vars.JCK_HOME` and the license-gated JCK distribution (see `docs/legal.md`). |
| `ejbca-smoke.yml` | Needs the staged WildFly/EJBCA minimum fixture and a `bench/wildfly/bench-baseline.json` to diff against. |
| `forcing-function-smoke.yml` | Nightly drift detector over the S2 forcing-function apps (Keycloak, EJBCA, Tomcat, Jetty, Quarkus, SpringBoot, Maven, Gradle, Kafka, Cassandra) — needs all of those distros pre-staged on the runner. |
| `soak-weekly.yml` | WP8.1 24h soak harness placeholder; needs the long-running soak runner before it does more than the launch canary. |
| `pgo-build.yml` | Runs `scripts/pgo.sh` (instrument → profile → merge → rebuild); needs a profiling runner and validated PGO recipe. |
| `t2-census.yml` | T2.1.1 no-default-natives census gate needs a JDK runtime image wired into CI plus a committed census baseline to diff against. |

Also parked under `.github/_disabled-workflows/` (not this directory):

| File | Why parked |
| --- | --- |
| `_disabled-workflows/ci.yml` | Heavier `name: CI` variant (macOS, JDK 25, `cargo doc`, coverage, audit, miri). Would collide with the live baseline `ci.yml`; merge its jobs into the live file rather than activating a second `CI`. |
| `_disabled-workflows/jvm-smoke.yml` | The RI.1..RI.17 matrix expects pre-staged external fixtures (SPECjvm2008, DaCapo, Keycloak distros) via mirror env vars that are not yet wired into repo secrets. Without them every slot fails red on every PR. |

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
