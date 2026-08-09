# `FlywayAutoConfigurationTests` / `IntegrationAutoConfigurationTests` — 300s TIMEOUT on the 2026-08-06 Windows full suite — NOT a regression

**Status: OPEN (standing margin problem), confirmed NOT the recycled-`JitInvokeInfo`
dispatch-aliasing bug that previously hit `FlywayAutoConfigurationTests`, and
NOT any other correctness regression.**

## Symptom

2026-08-06 full-suite Windows run (`craton-fullsuite-windows-20260806-s3/all-jit`),
`-Xmx 2g`, 300s/class, default Generational GC, `Parallel=4` per shard × 4
shards = 16 concurrent CratonVM processes on the host:

| Class | Module | Result | Seconds |
|---|---|---|---:|
| `org.springframework.boot.flyway.autoconfigure.FlywayAutoConfigurationTests` | `module/spring-boot-flyway` | TIMEOUT/HANG | 300.026 |
| `org.springframework.boot.integration.autoconfigure.IntegrationAutoConfigurationTests` | `module/spring-boot-integration` | TIMEOUT/HANG | 300.152 |

Logs:
- `craton-fullsuite-windows-20260806-s3/all-jit/logs/module_spring-boot-flyway.org.springframework.boot.flyway.autoconfigure.Fly-a62f68dc70ec.{out,err}.log`
- `craton-fullsuite-windows-20260806-s3/all-jit/logs/module_spring-boot-integration.org.springframework.boot.integration.autocon-9d4bdd63bbf0.{out,err}.log`

## Investigated: is this the known, previously-FIXED Flyway hang recurring?

`FlywayAutoConfigurationTests` was the subject of
`fixed-suite-bugs/springboot/flywayautoconfigurationtests-timeout-jit-site-cache-aliasing-FIXED-20260805.md`,
a real HANG on 2026-08-05 caused by `383e7f5cf` (a recycled `JitInvokeInfo`
box address letting one JIT call site serve another site's cached dispatch
answer — 39 of 73 tests failed with corrupted-looking exceptions such as
`ClassCastException: class java.lang.Class cannot be cast to ...PoolEntry`,
and the failure-handling overhead is what pushed the class past 300s).

Two checks before assuming this is that bug again:

1. **Is the fix still present on `dev`?** Yes — `git merge-base --is-ancestor
   383e7f5cf HEAD` reports `383e7f5cf` as an ancestor of the current tip, and
   the regression-guard test the fix added
   (`a_jit_generation_change_clears_every_site_keyed_memo` in
   `vm/src/jit/helpers.rs`) is still in the tree. Not reverted.
2. **Does the current hang's signature match the old bug's?** No.
   `FlywayAutoConfigurationTests`'s 2026-08-06 `.out.log` (542 lines) shows
   **continuous, unbroken forward progress** — the usual
   `EmbeddedDatabaseFactory`/`FlywayExecutor`/`DbValidate`/`DbMigrate` cycle,
   over and over, right up to the last line before the kill. No
   `ClassCastException`, no `Bad method descriptor`, no `NullPointerException:
   Class.isAssignableFrom: argument is null`, no `ConstantPoolException` — none
   of the exception shapes the aliasing bug produced. The largest gap between
   consecutive timestamped log lines in the whole run is **11.9s** (measured by
   diffing every `HH:MM:SS.mmm` timestamp in the file), consistent with
   ordinary jitter under host load, not a stall. `IntegrationAutoConfigurationTests`
   shows the identical shape: continuous progress, max gap **13.2s**, no
   matching exception text anywhere in either log or its `.err.log`.

So this is not a regression of the fixed dispatch-aliasing bug, and nothing
in either log suggests any other new correctness defect — both classes were
still doing normal, successful work when the 300s timeout killed them.

## What this actually is: the class's own margin ran out

The FIXED doc for `FlywayAutoConfigurationTests` already flagged this
possibility in its own "The 300s margin is pre-existing, not a residual of
this bug" section: the class does 73 full `ApplicationContext` refreshes (one
embedded-database start/migrate/stop cycle each) and has cost ~220-232s on a
healthy Azure Linux run for as long as it has been passing — an ~8x-HotSpot
class that spends the large majority of a 300s budget on legitimate work.
`IntegrationAutoConfigurationTests` has the same shape: it also builds and
tears down Flyway-backed and HikariCP-backed contexts repeatedly (its own log
shows the identical `HikariPool-1 - Starting...` / `Added connection` /
`Flyway ... DbValidate` / `DbMigrate` cycle) and was previously recorded at
224.1s and 165.3s (`craton-fullsuite-azure-20260802` and `...-azure-20260805`
respectively) — already a wide run-to-run spread, and already close to the
300s ceiling on the slower of those two runs.

Reading the last progress lines against each class's own start time:

| Class | First line | Last line before kill | Elapsed |
|---|---|---|---:|
| `FlywayAutoConfigurationTests` | `00:45:08.365` | `00:50:03.361` | ~295s |
| `IntegrationAutoConfigurationTests` | `02:01:26.260` | `02:06:13.518` | ~287s |

Both ran essentially the entire 300s budget doing real, successful
`ApplicationContext` work and simply didn't finish — not a stall, a genuine
throughput shortfall against the timeout.

**What's different this time (Windows, not Azure Linux) is the host
contention.** This full-suite run uses `-Parallel 4` × 4 shards = 16
concurrent CratonVM processes (`run-spring-boot-suite.ps1:28,992`), the same
scale of contention the FIXED Flyway doc's own validation section called out
as the reason ordinary jitter grows to double digits ("16 CratonVM processes
on 16 cores ... stretches that into the observed ~212s of quiet"). Both
`.err.log` files show the `[moving-young] fallback` GC warning
(`unregistered-jit-frame-on-stack` / `innermost-rbp-belongs-to-unguarded-callee`)
firing twice each during the run — consistent with the non-moving-sweep
fallback documented elsewhere as degrading young-gen throughput
(`reference_non_moving_young_sweep_degenerates_into_an_unusable_free_list`),
which would compound an already-thin margin, though it was not isolated as
the deciding factor here (two fallbacks in ~5 minutes is not itself unusual;
it is recorded because it is one plausible contributor to the variance, not
because it was shown to be the cause).

**Not confirmed further**: whether the Windows binary/host is intrinsically
slower per `ApplicationContext` refresh than the Azure Linux boxes these
classes were last measured on, versus this run simply landing on the
unlucky/loaded end of the same variance already documented for these two
classes. Both are plausible and not mutually exclusive; distinguishing them
would need a lower-parallelism (`-Parallel 1` or `2`) rerun of just these two
classes on the same Windows box, which this triage's scope (no multi-minute
reruns) does not cover.

## Recommendation

Not a code defect to fix in CratonVM. If this keeps recurring on Windows
full-suite runs, the actionable lever is the harness, not the VM: either
raise these two classes' per-class timeout (the runner already has a
`slowClasses` timeout-override table in `run-spring-boot-suite.ps1` for
exactly this situation — see `ConfigurationPropertySourcesTests`'s 5400s
override next to it) or lower `-Parallel` for Windows full-suite runs to
reduce contention. Recorded here so the next TIMEOUT on either class reads as
"the margin ran out again" rather than a fresh investigation from zero.

## Affected classes

- `module/spring-boot-flyway` — `org.springframework.boot.flyway.autoconfigure.FlywayAutoConfigurationTests`
- `module/spring-boot-integration` — `org.springframework.boot.integration.autoconfigure.IntegrationAutoConfigurationTests`
