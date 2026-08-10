# `FlywayAutoConfigurationTests` / `IntegrationAutoConfigurationTests` — 300s TIMEOUT

**Status: RETIRED — 2026-08-10** (branch `fix/flyway-integration-timeout-margin-20260810`).

Supersedes `docs/known-issues/springboot/flyway-integration-autoconfigurationtests-300s-margin-exhausted-windows-20260807.md`.
The live issue is now
[`moving-young-fallback-turns-three-classes-red-20260810.md`](../../../known-issues/springboot/moving-young-fallback-turns-three-classes-red-20260810.md).

## What the retired doc got right

Everything it checked. Its symptom table is exact (`FlywayAutoConfigurationTests`
HANG 300.026s, `IntegrationAutoConfigurationTests` HANG 300.152s in
`craton-fullsuite-windows-20260806-s3`), its verification that this is **not** a
recurrence of the recycled-`JitInvokeInfo` dispatch-aliasing bug is sound
(383e7f5cf still an ancestor, regression guard still in the tree, none of that
bug's exception shapes present), and its reading of both logs as continuous
forward progress rather than a stall is accurate.

Note for anyone re-checking it: the run is real but lives in the
`CratonVM-spring-boot-residual-20260728` worktree, not the main checkout —
every worktree keeps its own `apps/spring-boot-suite-runner/.suite/results/`.

## What it got wrong

Its conclusion — *"Not a code defect to fix in CratonVM. If this keeps recurring
on Windows full-suite runs, the actionable lever is the harness, not the VM"* —
is wrong, and its recommended fix (raise the per-class timeout, or lower
`-Parallel`) would have made the problem invisible rather than fixed.

It also treats the two classes as one story. They are not.

Measured standalone, one class at a time, `--Xmx 2g`, HotSpot control on the
same host:

| Class | HotSpot | CratonVM JIT | CratonVM `--nojit` | fallback peak |
|---|---:|---|---:|---:|
| Flyway | 10.0s ✓73/73 | 496.2s ✓73/73 | 218.5s ✓73/73 | 7 |
| Integration | 11.2s ✓34/34 | **OOM after 3.8h** | 221.9s ✓34/34 | **#16384** |

- **`Integration` is not a margin problem at all.** It does not run long and then
  pass; it dies of `OutOfMemoryError: Java heap space`. A longer timeout moves
  the failure later and produces no pass.
- **`Flyway` is not intrinsically over budget either.** It finishes in 218.5s
  with the JIT off — inside the existing 300s. The JIT-on cost of 496.2s is what
  puts it over, and it logs 7 `[moving-young]` fallbacks doing so.

Both complete inside the default budget with `--nojit` and zero fallbacks. The
`[moving-young]` fallback, not the timeout, is what makes these rows red.

## Two of its specific claims, corrected

- **"What's different this time (Windows, not Azure Linux) is the host
  contention."** The 300s ceiling is breached on Linux too —
  `craton-fullsuite-azure-20260805-s5` records `FlywayAutoConfigurationTests`
  HANG at 300.136s. Contention worsens the margin; it is not what creates it.
- **"Not confirmed further: whether the Windows binary/host is intrinsically
  slower … distinguishing them would need a lower-parallelism rerun … which this
  triage's scope does not cover."** That rerun already existed when the doc was
  written: `craton-nonpassed-20260806-s3`, a 7-class all-PASS run on the same
  Windows host, records `FlywayAutoConfigurationTests` PASS at **375.278s** —
  i.e. over the ceiling even with almost no contention. Across 34 recorded
  observations both classes have also passed at 426.5s and 469.1s.

## Why it is retired rather than updated

The class-level fact it exists to record — "these two classes sit near the 300s
line" — is no longer the useful summary. The useful summary is that a JIT-driven
GC fallback degrades or kills them, which is a VM defect with its own page and
its own reproduction, and which covers a third class
(`QuartzEndpointWebIntegrationTests`) the retired doc never mentioned.
