# CratonVM — Scripts & Infra Review (Fable, 2026-06-10)

Scope: `scripts/` (~30 shell/bat/ps1 + `scripts/smoke/`), `ci/`, `tools/`,
root `build-cpu*.bat`, `test-infra/*.sh` (excluding `suite-results/` data),
`fuzz/`, and `.github/` CI. Static review only. Produced inline by the
orchestrator after the `scripts` sub-agent was lost to the session limit.

## Summary

The script collection is a **developer toolbox, not a shippable surface** — and
that is mostly fine, *if* it is labeled as such. The build/bench/run scripts are
heavily **machine-specific**: hardcoded `C:/craton/CratonVM`, sibling worktree
paths (`C:/craton/CratonVM-h2val`, `…-kctests`, `.claude/worktrees/...`),
TornadoVM SDK paths, and `C:/Program Files/Java/jdk-25`. None will run on a
contributor's machine without edits. `scripts/README.md` is a good index but is
partly stale. **No secrets** were found in `scripts/`, `ci/`, or `tools/` (clean).

The two material infra findings are: **(1) CI is effectively absent** — 12
workflow files exist but live under `.github/.wf/`, which GitHub Actions never
scans, so nothing runs on push/PR (the README badge and CONTRIBUTING/RELEASING
claims are therefore false); and **(2) three `test-infra` scripts call
`taskkill //F //IM cratonvm.exe`**, which kills *every* cratonvm process by image
name — a documented cross-session footgun that also yields spurious `rc=1`
"failures" and would be hazardous on a shared CI runner.

---

## Bugs / defects

### B1 (HIGH, infra) — CI does not run; workflows are parked under `.github/.wf/`
`.github/` contains `CODEOWNERS`, `dependabot.yml`, `FUNDING.yml`, issue/PR
templates, and **12 workflow YAMLs under `.github/.wf/`** (`ci.yml`, `release.yml`,
`bench-gate.yml`, `cuda-bridge.yml`, `dco.yml`, `jck.yml`, etc.) plus
`.github/_disabled-workflows/`. GitHub Actions only scans `.github/workflows/`,
which **does not exist**. Net effect: zero automated build/format/clippy/test on
any push or PR. The `.github/.wf/README.md` is itself self-contradictory — its
header says "Every workflow in this directory is **active**" while a later section
admits "GitHub Actions only scans `.github/workflows/`, so anything under
`.github/.wf/` is dormant." **Fix:** move (at least) `ci.yml` to
`.github/workflows/`, reconcile the duplicate-name collision the README mentions,
and re-point the README badge.

### B2 (MEDIUM) — `taskkill //F //IM cratonvm.exe` kills unrelated processes
`test-infra/run-all-apps-suites.sh:21`, `run-comparison-full.sh:36`,
`run-cpu-rerun.sh:15` each force-kill **all** `cratonvm.exe` by image name. On a
machine running more than one CratonVM (parallel suites, another worktree, a
developer's session) this kills innocent processes, and `taskkill /F` exits
non-zero when no match, which several call sites then misread as a tool failure
(the documented "rc=1/empty-output" artifact). Prefer killing by PID captured at
launch, or scope to the suite's own child process.

### B3 (MEDIUM) — pervasive hardcoded absolute paths (non-portable)
Examples: `scripts/bench-4way.sh:13` `ROOT=C:/craton/CratonVM`;
`bench-poly-4way.sh:80-81` TornadoVM SDK jars; `run-h2-*.bat` /
`run-one-test.bat` / `run_decchurn_ab.bat` hardcode
`C:\craton\CratonVM...\cratonvm.exe` and `C:/Program Files/Java/jdk-25`;
`run-all-apps-cuda.sh:8` points `ROOT` at a specific ephemeral worktree
(`.claude/worktrees/angry-brown-38c5dc`). Parameterize via `${ROOT:-$(git rev-parse
--show-toplevel)}` and `JAVA_HOME`. (`test-infra/run-all-apps-suites.sh:7`
already does the `${ROOT:-...}` pattern — propagate it.)

### B4 (MEDIUM, personal info) — `scripts/build-devverify.bat` hardcodes a personal toolchain
Lines 7–8 invoke `C:\Users\Victor\.rustup\toolchains\…\hmrustc.exe` /
`hmcargo.exe` — a personal path *and* a custom `hm*` toolchain that exists on no
other machine. This file should be genericized or excluded from the public repo.
(Also flagged by oss-readiness as committed personal info.)

### B5 (LOW) — MSYS/GitBash assumptions
The `//F //IM` double-slash in the `taskkill` calls is the MSYS path-conversion
workaround and only works under Git-Bash; the bench scripts mix Windows paths into
shell scripts. These are Git-Bash-on-Windows-only. Fine for the author, but mark
them as such (a shebang/comment) so contributors on Linux/macOS do not assume
portability.

## Stubs / missing pieces

- **`fuzz/` has no fuzz targets.** The crate scan found `fuzz/src` with 0 `.rs`
  files; the workspace keeps `fuzz` as a member with `publish = false`. Confirm
  whether `fuzz/fuzz_targets/` exists and is populated; if the harness is empty it
  is a stub and either the targets should be committed or the crate dropped from
  the public workspace until it has content. CONTRIBUTING/README imply a working
  libfuzzer harness.
- **`scripts/README.md` is stale**: documents `build-h2.bat`, `build-rwd.bat`,
  `build-wt.bat` that are not in `scripts/` (root has `build-cpu.bat` /
  `build-cpu-rwd.bat`). Reconcile the index with the actual files.

## Performance / wastefulness

- `scripts/loop-run-all.sh` runs apps in fixed batches of 4 with no result caching;
  fine for a dev loop, slow for CI. Not a ship concern.
- The bench scripts re-`javac` fixtures each invocation; a guard on `.class` mtime
  would save repeated compiles in tight A/B loops.

## CI coverage assessment (the "tests" axis for infra)

Today: **none effective.** The *intended* CI (per `.github/.wf/`) is actually quite
good on paper — build+fmt+clippy(`-D warnings`)+test on ubuntu+windows (`ci.yml`),
a DCO gate, a tagged-release cross-platform build (`release.yml`), a criterion
bench-regression gate (`bench-gate.yml`), a `cuda` feature-build guard
(`cuda-bridge.yml`), JCK harness (`jck.yml`), and nightly app-smoke drift
detectors. The single highest-leverage infra fix in the whole review is to **make
the parked `ci.yml` actually run** by relocating it to `.github/workflows/`; until
then every "CI enforces X" claim across README/CONTRIBUTING/RELEASING is aspirational.

`ci/` holds `jck-config.jti` + `jck-runner.yml` (JCK harness config — internal,
license-gated per `docs/legal.md`); `tools/` holds a `junit_launcher_probe` and a
maven-shim sync script (internal dev tooling). Both are internal-only; fine to keep
but they are not user-facing.

## Classification (internal-dev vs shippable)

- **Internal-dev only** (hardcoded paths / worktrees / author's machine):
  `bench-4way.sh`, `bench-poly-4way.sh`, `bench-appsuite.sh`, `run-h2-*.bat`,
  `run-one-test.bat`, `run_decchurn_ab.bat`, `run-kc-tests.sh`,
  `run-all-apps-cuda.sh`, `build-devverify.bat`, the `loop-run-*`/`real-run-all`/
  `triage` app harnesses, and `test-infra/*.sh`.
- **Potentially shippable** (after path-genericizing): `build-cpu.bat`,
  `build-gpu.bat`, `capture-hotspot-baseline.{sh,ps1}`, `pgo.{sh,cmd}`,
  `check-no-diag-prints.sh`, `scripts/smoke/*` (the RI1–RI17 smoke suite is a
  genuinely useful reproducible harness if its fixture paths are parameterized).

## Recommendations

1. **Activate CI** — move `ci.yml` to `.github/workflows/`; this single change
   unlocks the format/clippy/test gate the docs already promise.
2. Replace all three `taskkill //F //IM cratonvm.exe` with PID-scoped kills.
3. Genericize `ROOT`/`JAVA_HOME`/binary paths across `scripts/` (the
   `${ROOT:-...}` idiom already used in `test-infra/run-all-apps-suites.sh`).
4. Remove or genericize `build-devverify.bat` (personal `hm*` toolchain path).
5. Resolve `fuzz/` (commit targets or drop from the published workspace).
6. Reconcile `scripts/README.md` with the actual file set.
