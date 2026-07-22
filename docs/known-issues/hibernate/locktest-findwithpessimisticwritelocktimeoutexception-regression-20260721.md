# `LockTest.testFindWithPessimisticWriteLockTimeoutException` — hardcoded 5s `assertTimeout` reliably exceeded (8/8 repro, load-independent)

| | |
|---|---|
| **Status** | 🟡 OPEN — genuine, reproducible CratonVM-specific timing gap. Root cause not fully isolated (two previously-fixed candidate mechanisms both ruled out as the dominant cost here — see below). |
| **Area** | JPA/Hibernate `assertTimeout`-wrapped nested-transaction lock-timeout test; likely raw interpreter/JIT dispatch overhead through a 3-level-nested `doInJPA`/H2-JDBC call chain, not GC- or compile-related. |
| **Discovered** | 2026-07-21, Hibernate ORM JUnit5 suite "passed" category rerun, `apps/hib-suite-runner/run-hib.sh`, binary from worktree `C:\craton\CratonVM-hib-local-0712` (branch `test/hib-local-0712`, merged with `origin/dev` @ `7aed580f0`). |
| **Contradicts** | `docs/known-issues/hibernate/README.md`'s "Residual clusters (this rerun)" entry, which marks the `LockTest` residual FIXED/RETIRED based on `docs/internal/hib-120s-junit-timeout-cluster-20260716.md`'s 2026-07-17 combined-run acceptance test (`LockTest 15/15 started tests passed, 8 expected skips, 7065ms`). |

## Symptom

Original failure, `run-20260721-175909-passed/on-real/shard-5/raw.log`:

```
@@FAIL org.hibernate.orm.test.jpa.lock.LockTest :: org.opentest4j.AssertionFailedError: execution exceeded timeout of 5000 ms by 10087 ms
@@FAIL org.hibernate.orm.test.jpa.lock.LockTest :: org.junit.platform.commons.JUnitException: Failed to close extension context
@@RESULT 94 org.hibernate.orm.test.jpa.lock.LockTest found=23 started=15 ok=14 failed=1 aborted=0 skipped=8 ms=42406
```

`-Dcraton.trace=1` isolates the failing method precisely — every repro's stack
trace bottoms out at the same line:

```
at org.hibernate.orm.test.jpa.lock.LockTest.testFindWithPessimisticWriteLockTimeoutException(LockTest.java:127)
```

The test (`LockTest.java:118-160`) wraps its entire body in
`assertTimeout(Duration.ofSeconds(5), () -> { ... })`: persist an entity,
open a second transaction that acquires `PESSIMISTIC_WRITE`, then — nested
inside that — open a *third* transaction attempting
`entityManager.find(..., PESSIMISTIC_WRITE, {JAKARTA_LOCK_TIMEOUT: 0L})` and
assert it immediately throws `LockTimeoutException`/`PessimisticLockException`
via H2's no-wait lock semantics. Three real `doInJPA` transactions (each its
own `EntityManager`/session/connection lifecycle) must complete inside the 5s
budget.

## Reproduction (isolated, `craton.batch=1`, single-class listfile)

Command:
```
cd C:/craton/CratonVM/apps/hib-suite-runner
"C:/craton/CratonVM-hib-local-0712/target/release/cratonvm.exe" --java-home "C:/Program Files/Eclipse Adoptium/jdk-25.0.3.9-hotspot" --Xmx 1500m @common.args -Dcraton.batch=1 -Dcraton.trace=1 CratonRunner <single-LockTest-listfile> 0
```

8/8 runs failed, always the same method, always `failed=1` out of the same
`found=23 started=15 ok=14 aborted=0 skipped=8` shape:

| Run | Host CPU load (snapshot before run) | Overshoot past 5000ms |
|---|---|---|
| Original suite run (shard-5, larger batch) | unknown (shared box, many concurrent worktrees) | **10087 ms** |
| Isolated run 1 | not sampled | 2394 ms |
| Isolated run 2 | not sampled | 149 ms |
| Isolated run 3 | not sampled | 580 ms |
| Isolated run 4 | 74% avg | 1345 ms |
| Isolated run 5 | **4% avg (quiet host)** | **1986 ms** |
| Isolated run 6 (`CRATONVM_DBG_TIER_ENQUEUE=1`) | not sampled | 6069 ms |
| Isolated run 7 (traced) | not sampled | 3042 ms |
| Isolated run 8 (traced) | not sampled | 3054 ms |

**This is not a host-load artifact.** Run 5 was captured at 4% average CPU
load (a genuinely quiet host by this box's usual standards — see
`reference_shared_host_multitenant_confound` — contrast with run 4's 74%
load, which actually overshot *less*) and still missed the budget by ~2s. No
run, at any load level, ever passed. The overshoot magnitude is highly
variable (149ms-10087ms, a ~68x spread) but the sign is never negative:
CratonVM's execution of this specific call shape appears to sit right at or
just past the 5s ceiling even in the best case, with ambient variance (host
load, JIT background-compile scheduling, etc.) pushing it further over by an
inconsistent amount.

## Root-cause investigation: two previously-fixed candidate mechanisms both ruled out as the dominant cost

This exact class/symptom shape (`LockTest`, hardcoded ~5s internal timeout,
overshoot on the order of seconds) was investigated twice before and marked
FIXED:

1. **JIT compile-time tax** (`docs/internal/fixed-suite-bugs/hib-misc-residuals-20260716-FIXED.md`) —
   raising `CompilationPolicy` thresholds (`fix/jit-compile-time-tax-20260716`)
   was explicitly validated as **not** fixing `LockTest`'s timeout.
2. **`scan_active_jit_frames` conservative-root-scan cost** (`f377eb694`,
   2026-07-17) — the mechanism ultimately credited with fixing `LockTest`
   (`0/5 → 5/5 clean`, `~7-9s` per full-class run). Confirmed present in the
   tested binary (`f377eb694` is a strict ancestor of both
   `test/hib-local-0712`'s merge base `7aed580f0` and current `dev` tip).

Re-checked both against this specific failure:

- **`CRATONVM_DBG_TIER_ENQUEUE=1`**: 314 real C1 compile-task enqueues occur
  during the run (JUnit5 reflection + H2 internals, matching the prior
  doc's description), confirming JIT compilation is active — so mechanism
  (2)'s trigger condition (`jit_code_range_count() > 0`) is satisfied. But:
- **`CRATONVM_DBG_ROOTSNAP=1`** (the exact diagnostic used to root-cause and
  validate the `f377eb694` fix) shows the conservative-scan cost is
  **negligible** for this workload: `avg_us` climbs only 1.32 → 1.76 → 2.09
  µs per call as call count grows 200k → 400k → 600k, for a **cumulative
  total of ~1.25ms** by 600k calls — three orders of magnitude too small to
  account for a multi-second overshoot. The `f377eb694` fix is working
  correctly here (no runaway per-call cost); it just isn't what's consuming
  the missing seconds.

So the dominant cost for **this specific method** is neither of the two
previously-identified/fixed mechanisms. It has not been further isolated in
this session — the most likely remaining candidate, based on the test's
shape (three sequential, non-overlapping `doInJPA` transactions, each doing
real entity persistence/lock-acquisition/JDBC round-trips against H2, on top
of JUnit5's per-test reflection overhead), is baseline interpreted/JIT
dispatch overhead through that call chain simply being too slow, in
aggregate, to fit inside a HotSpot-tuned 5-second budget — an architectural
throughput gap rather than a discrete bug, similar in flavor to the
`update_root_snapshot` O(depth) and JIT-tax findings elsewhere in this
codebase, but not confirmed to be either of those specifically. Would need
per-phase wall-clock instrumentation (e.g. bracketing each of the three
`doInJPA` calls) or a `perf`/sampling profile of an isolated run to pin down
further.

## Scope

Only `testFindWithPessimisticWriteLockTimeoutException` was observed to fail
across all 8 repro attempts (`failed=1` every time, confirmed via
`-Dcraton.trace=1` to be this method specifically). Three sibling methods in
the same file share the identical `assertTimeout(Duration.ofSeconds(5), ...)`
+ nested-`doInJPA` pattern
(`testQuerySingleResultPessimisticWriteLockTimeoutException`,
`testQueryResultListPessimisticWriteLockTimeoutException`,
`testNamedQueryResultListPessimisticWriteLockTimeoutException`, lines
186/244/303) but were not observed to fail in these runs — worth rechecking
if this doc's fix (once found) is applied, since they are structurally
almost identical and may just not have been the first one JUnit5 happened to
schedule close to the margin.

`org.hibernate.orm.test.jpa.lock.LockTest.testContendedPessimisticLock`
(a *different*, non-`assertTimeout`-wrapped, 20-second-latch background-
thread contention test later in the same class) was suspected initially
from the raw log's proximity but is **not** the failing test — it logged its
`(BG) about to issue...`/`got write lock` lines successfully and is not
implicated by any trace.

## Not investigated / ruled out

- Not `vm/src/threading/monitor.rs` or JVM-level monitor/lock code — this
  test's timing is about wall-clock JPA/JDBC/H2 SQL-level lock-timeout
  behavior (`JAKARTA_LOCK_TIMEOUT=0`, H2 no-wait locking), not JVM
  intrinsic-lock contention.
- Not a stale-classpath harness artifact (the classpath-staleness bug found
  earlier in this investigation session was unrelated and already fixed).

## Recommendation

Do not treat `docs/known-issues/hibernate/README.md`'s "Residual clusters
(this rerun)" `LockTest` FIXED/RETIRED line as covering this method — narrow
that claim or add a pointer to this doc. Next session should add per-phase
timing instrumentation around `testFindWithPessimisticWriteLockTimeoutException`'s
three `doInJPA` calls (or run under a sampling profiler) to find which of the
three transactions/phases is actually slow, since neither of this repo's two
previously-fixed `LockTest`-adjacent mechanisms (JIT compile-time tax,
conservative-root-scan cost) accounts for the overshoot here.
