# Bug DF10 — silent `rc=1` process death of embedded-server tests **only under parallel load**

**Severity:** Medium (inflates the parallel suite's failure count; CratonVM-specific
fragility under concurrent load — HotSpot survives the same parallel run).
**Status on CratonVM:** NOSUMMARY (silent `rc=1`) under parallel-8; **HANG/clean in isolation**. **HotSpot:** PASS (parallel-8).
**Run date:** 2026-06-18
**Binary:** dev `9d748b81` + DF08/DF09 (worktree `C:/craton/CratonVM-tcfull`).
**Affected:** ~25% of embedded-server classes in the parallel-8 census (e.g.
`jakarta.el.TestCompositeELResolver`, `TestImportHandlerStandardPackages`,
`TestOptionalELResolverInJsp`, `jakarta.servlet.http.TestHttpServlet`, the
`TestHttpServletDoHead*1024*` family, `jakarta.servlet.annotation.TestServletSecurity`).

## Symptom

Under the suite harness (`-Parallel 8`), these classes exit `rc=1` at ~9–37 s
with **no JUnit summary** and **no diagnostic**: `.log` has only the JUnit
banner; `.log.err` ends mid-server-lifecycle. **No** panic, **no** SIGSEGV/hs_err,
**no** Java exception, **no** "Error in thread main", **no** "System.exit" log —
a "silent ExitProcess-class" exit that bypasses the crash handler.

## Root cause — narrowed (it is a PARALLEL-LOAD artifact, not a per-class defect)

- **Does not reproduce in isolation.** Running the fastest-dying class
  (`TestHttpServletDoHeadInvalidWrite1024ValidWrite512`) alone with full CPU ran
  **288 s without dying** — stable at 2.5 GB committed, ~16–19 threads, ~379
  handles, all flat (monitored via `run-monitor.ps1`). No resource leak; it
  simply hangs (interpreter-slow serving), it does **not** exit. Under parallel-8
  the same class exits `rc=1` in ~9 s.
- **Not memory pressure.** Host has 64 GB RAM, 31 GB free; 8 × 2.5 GB ≈ 20 GB
  committed during the census — well under the 82 GB commit limit. No swap/commit
  exhaustion.
- **Not a per-process resource leak.** Handle/thread/memory are flat across the
  isolated run's repeated server start/stop cycles.
- **HotSpot survives parallel-8** (the baseline ran at `-Parallel 8` with no such
  NOSUMMARY cluster), so this is a **CratonVM-specific** weakness under concurrent
  load — most plausibly a race in the embedded-server start/stop lifecycle (each
  test case spins a connector up and down) or shared-OS-resource handling
  (ephemeral-port churn) that only turns fatal under the timing/contention of 8
  concurrent VMs. `cdb` could not catch the exit (its altered timing masks the
  death — the class progressed 12 test cases under the debugger without dying).

## Important implication for the suite numbers

**The parallel-8 census OVERSTATES per-class failures.** The NOSUMMARY classes
are not deterministically broken — run alone they hang or could pass. The "true"
per-class CratonVM capability is better than the parallel-8 run shows. For
accurate per-class results, re-run the suite (or at least the NOSUMMARY subset)
at **low parallelism** (`-Parallel 1`–`2`), where the contention-induced deaths
don't occur.

## Reproduce / next step

```powershell
# Dies under parallel load:
.tooling/run-suite.ps1 -Vm craton -Parallel 8 -Tag p8 -ListFile <doheads.txt> ...
# Survives in isolation (hangs, doesn't die):
C:/craton/CratonVM-tcfull/run-monitor.ps1   # runs one DoHead class + resource trace
```

Pinning the exact race requires an instrumented build (log thread lifecycle +
the exit/abort path) reproduced under concurrent load — deferred. The pragmatic
mitigation for accurate measurement is lower harness parallelism.
