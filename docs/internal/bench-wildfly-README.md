# bench/wildfly — EJBCA-vs-cratonvm smoke harness

Infrastructure built by **WP0.5** of `docs/wildfly-ejbca-roadmap.md`. Pins today's (2026-04-24) EJBCA-on-cratonvm reality as a repeatable baseline so future waves can detect regressions.

## What this is

A minimum, reproducible pipeline:

```
stage-ejbca-min.{sh,ps1}  ->  compile a tiny EJBCA/cesecore-shaped Java fixture
          |
          v
run-under-cratonvm.{sh,ps1}  ->  execute under target/release/cratonvm[.exe],
                                 capture stdout/stderr/rc
          |
          v
diff-baseline.sh          ->  compare to bench-baseline.json
                              (returns 0 = match, !=0 = drift)
```

It is deliberately **tiny** — a single Java main class, no WildFly, no EJBCA deploy. The point is to have a canonical failure shape we trust end-to-end before investing in the heavier staging work. See the "Expected baseline today" section below for what that shape is.

## Files

| File | Purpose |
|---|---|
| `stage-ejbca-min.sh` / `.ps1` | Compile the fixture into `staged/classes/`. Auto-detects whether the rich `C:/craton/ejbca-test-run/` tree is present; falls back to `apps/ejbca_min_fixture/src/Main.java`. |
| `run-under-cratonvm.sh` / `.ps1` | Run the staged fixture under `target/release/cratonvm[.exe]` with the classpath set to `staged/classes/` plus any staged JARs. Writes `last-run.{stdout,stderr}.log`, `last-run.rc`, `last-run.meta.json`. |
| `bench-baseline.json` | Canonical expected failure set. Schema (v1): `expected_final_rc`, `expected_stderr_contains[]`, `expected_stderr_forbidden[]`, `expected_stdout_contains[]`, plus `steps[]` and `known_blockers[]` annotations. |
| `diff-baseline.sh` | Compare `last-run.*` to `bench-baseline.json`. Exit 0 = match, 1 = drift. Prefers `jq`; falls back to `python3`. |
| `staged/` | Output of the staging step: `classes/`, optionally `junit.jar` / `hamcrest.jar`, plus `compile.log` and `main-class.txt`. Regenerated on every stage. |
| `last-run.*` | Produced by every run. Safe to delete; will be recreated. |

## Running it

### Bash (Git Bash / MSYS / Linux / macOS)
```bash
# from repo root
cargo build --release -p cratonvm-cli   # once
bash bench/wildfly/stage-ejbca-min.sh
bash bench/wildfly/run-under-cratonvm.sh
bash bench/wildfly/diff-baseline.sh
echo "baseline-check rc=$?"
```

### PowerShell (Windows)
```powershell
cargo build --release -p cratonvm-cli
powershell -ExecutionPolicy Bypass -File bench\wildfly\stage-ejbca-min.ps1
powershell -ExecutionPolicy Bypass -File bench\wildfly\run-under-cratonvm.ps1
# diff-baseline is bash-only; invoke via Git Bash or WSL for the compare step.
bash bench\wildfly\diff-baseline.sh
```

### Environment variables
- `JAVA_HOME` — javac discovery (required if javac not on PATH).
- `EJBCA_TEST_RUN_DIR` — override the default `C:/craton/ejbca-test-run`.
- `CRATONVM_BIN` — override the default `target/release/cratonvm[.exe]` path.
- `TIMEOUT_SEC` — default 60; bump for heavier fixtures post-WP0.1.

## Expected baseline today (2026-04-24, pre-Wave-1)

With the placeholder fixture (`apps/ejbca_min_fixture/src/Main.java`):
- `stage` exits **rc=0** (deterministic javac compile).
- `run`   exits **rc=1** — cratonvm throws `NullPointerException: Cannot invoke write on null` on the 2nd `System.out.println`. This is the **WP0.1** blocker documented in `memory/finding_println_regression.md`. The fixture deliberately emits two prints so the harness will notice the day WP0.1 lands.
- `diff-baseline.sh` exits **0** ("baseline match") because the current run shape matches `bench-baseline.json`.

The day WP0.1 lands, `run` will return rc=0, the stderr NPE goes away, and the harness will **fail** (drift detected). That failure is the green signal — update `bench-baseline.json`'s `expected_final_rc` to 0, clear `expected_stderr_contains`, add `"ejbca_min_fixture: ok"` to `expected_stdout_contains`, and commit both the fix and the updated baseline in the same PR.

## How CI should invoke this

A minimal CI job (example given as `.github/workflows/ejbca-smoke.yml`, optional — only add it if the team wants nightly coverage):

1. Build cratonvm once: `cargo build --release -p cratonvm-cli`.
2. Install JDK 21+ (for `javac`). `actions/setup-java@v4` with `java-version: '21'` is sufficient — we don't need Oracle-specific features.
3. Run the three scripts above. If any exits non-zero, upload `bench/wildfly/last-run.*` and `bench/wildfly/staged/compile.log` as artefacts.
4. On PR, comment the diff-baseline result on the PR (see `.github/workflows/jvm-smoke.yml` summary-job pattern for the idiom).

Note: `C:/craton/ejbca-test-run/` is a **developer-local** tree (it contains pinned cesecore sources we use for reproducers). CI will always hit the placeholder branch of `stage-ejbca-min.sh`. That's fine — the placeholder is designed to exercise the same WP0.1 failure mode.

## Diffing results vs the baseline manually

If `diff-baseline.sh` isn't handy:

```bash
# actual rc
cat bench/wildfly/last-run.rc

# actual stderr signals
grep -E 'NullPointerException|Cannot invoke write on null' bench/wildfly/last-run.stderr.log

# actual stdout signals
cat bench/wildfly/last-run.stdout.log
```

Match these manually against `bench-baseline.json`.

## Updating the baseline

A drift (diff-baseline rc != 0) requires a decision:

1. **Is this a regression?** Investigate via `last-run.stderr.log`. File an issue.
2. **Is this an intentional fix?** Update `bench-baseline.json` in the same PR that ships the fix. Bump `generated_at`. Add the WP number to `known_blockers[]` if one was resolved. Never "fix" a drift by silently loosening the baseline without a code change.

## Anchored greps (for future WPs)

Future work packages refer to this harness via stable names:
- `bench/wildfly/stage-ejbca-min.sh` — stable across waves, never rename.
- `bench/wildfly/bench-baseline.json` — schema stable at `$schema_version: 1`; bump if fields change shape.
- Placeholder class name `Main` (FQCN `Main`, no package) is stable; real mode uses `DirectRunner`.

## What this does NOT cover

- Full WildFly boot — Wave 14+ territory.
- Real EJBCA deployment — Wave 17.
- JDK-compat JUnit4 harness — blocked by WP0.2 (`ObjectStreamClass.serializableConstructor`).

This harness exists to catch the earliest possible regression on Java program startup before the heavier rigs are built. It pays for itself the moment WP0.1 lands: the automation flips from "baseline match" to "drift" without any human looking at it.
