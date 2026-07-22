# DoHead family — mid-run HTTP/2 hangs + LifecycleException flakes — FIXED

**Status: FIXED / retired on 2026-07-15** (branch
`fix/dohead-http2-midrun-hang-20260715`). Two independent VM defects
root-caused and fixed; both long pre-dated the doc and every recent GC/JIT
change that was suspected along the way.

## Original symptoms (2026-07-14 doc, `f23a3f42a` full-suite rerun)

- `TestHttpServletDoHeadInvalidWrite1025ValidWrite513` and
  `TestHttpServletDoHeadInvalidWrite512ValidWrite1` hung the full 300 s
  timeout mid-run, the log stopping right after
  `Starting Servlet engine: [Apache Tomcat/12.0.0-M1-dev]`.
- 6 of 8 non-passing DoHead classes flaked 1–7 of 288 parameterizations with
  sporadic `LifecycleException: Failed to start component` — on a baseline
  that already contained the `threadpoolexecutor-prestart` fix, so that
  earlier (fixed) bug did not explain them.

Fresh reproduction on dev `a9b838c4e` at `-MaxHeap 500m` was far more
florid: SIGSEGVs (wild getfield receivers shaped like `Int(N)`-cell-read-
as-pointer, e.g. `0x0000000500000000`), engine-start
`IllegalThreadStateException`, per-response
`NullPointerException: Cannot read field "item" because "p" is null`,
mid-read connection deaths, `gen_heap` zero-header containment floods, and
executor worker threads from long-dead Tomcat instances parked forever.

## Root cause A — thread identity keyed by mirror address (aliasing)

`ThreadRegistry` resolved Java `Thread` mirrors to registry entries by RAW
POINTER comparison (`find_thread_id_by_thread_obj` linear walk;
`thread_obj_to_park` unpark index keyed by address). Dead threads' entries
are retained forever (for TERMINATED `getState()`), keeping their LAST
mirror address. Once a dead thread's mirror is collected and its address
recycled for a NEW `Thread`, the walk aliases the fresh mirror to the dead
entry:

- `Thread.getState()` on a freshly constructed utility-executor worker
  returned TERMINATED/RUNNABLE ≠ NEW → JDK 25
  `ThreadPoolExecutor.addWorker` (line 888) threw
  `IllegalThreadStateException` → `LifecycleException: Failed to start
  component [StandardEngine[Tomcat]]` at `ContainerBase.startInternal:764`
  (`scheduleWithFixedDelay` for the background-processor monitor) — the
  flake, at exactly the log position where the hangs froze.
- `Thread.interrupt()` / `LockSupport.unpark(Thread)` resolved through the
  same address-keyed paths and could target a dead thread's ParkState —
  losing `shutdownNow()` wakeups. Observed as workers from instances
  ~150 parameterizations old still parked in `getTask()` (cdb dumps in
  `apps/tomcat-suite-runner/stall-dumps/`).

**Fix** (`vm/src/threading/thread_registry.rs`, `vm/src/vm/vm_exec.rs`,
`vm/src/vm/vm_init.rs`): identity is now keyed by the Java-side
`Thread.tid` field — `final`, process-unique, never reused — via a
`java_tid → ThreadId` index captured at `thread_start` /
`set_java_thread_obj_with_tid`, with a tid-GUARDED pointer walk (plus lazy
backfill) for mirrors registered mid-construction, and a synthetic-layout
fallback to the legacy path. `mark_dead` now also removes the dead mirror's
address from the unpark reverse index (Arc-identity-guarded).
`thread_interrupt`, `thread_is_interrupted`, `unpark`, `getState`,
`isAlive` all resolve through the hardened path.

**Validation:** 0 `IllegalThreadStateException` / 0
`Failed to start component` across ~900 Tomcat boots that previously showed
~4 per 288; stall dumps show only the CURRENT instance's worker pool (no
stragglers); 412 `cratonvm-vm` thread unit tests pass.

## Root cause B — dying threads discarded buffered card-table edges

The generational write barrier queues old→young edge records in per-thread
buffers (`gc/src/card_table.rs`), auto-flushed at 64 entries or drained by
the collector at STW (`flush_all`). `DirtyBufferGuard::drop` (thread exit)
deregistered the buffer and **discarded any residual offsets**, on the
rationale that a tearing-down thread "holds no live references the
collector must preserve". That conflated the thread's stack roots with the
HEAP edges its writes created: a buffered offset records a reference stored
INTO THE HEAP, which outlives the thread.

The canonical victim (identified live via the new `BADRECV-Z` diagnostic
and the pre-existing `CRATONVM_DBG_RSET_AUDIT`): the STATIC
`FastHttpDateFormat` → `ConcurrentDateFormat` →
`java.util.concurrent.ConcurrentLinkedQueue`. Every HTTP response's Date
header does `queue.poll()` / `queue.add()` (CAS-installing young nodes into
the old, spill-promoted queue). A Tomcat worker thread's final responses
buffer those edges; the per-parameterization endpoint shutdown then retires
the whole pool, dropping the buffers. The next minor GC sees a CLEAN card
over a live old→young edge (`[rset-miss] OLD
java/util/concurrent/ConcurrentLinkedQueue @… fld[0]/fld[1] -> young …
CLEAN`) and frees the still-referenced node. Every subsequent request then
walks zeroed nodes:

- `NullPointerException: Cannot read field "item" because "p" is null`
  (CLQ traversal) → 500s / truncated responses / mid-read EOFs;
- node memory reuse → `Int(N)`-cell-as-pointer SIGSEGVs in `Getfield`;
- `ConcurrentLinkedQueue`'s `restartFromHead` spinning on a corrupted
  chain → the silent, log-frozen hang shape;
- `gen_heap` zero-header containment WARN floods.

Expression is gated by GC frequency × thread churn — florid at
`-MaxHeap 500m` (every run), flake-level at default heap (the doc's 1–7/288
+ occasional hang), absent at `-MaxHeap 2g` (288/288 PASS pre-fix). This
also explains why the family's flake floor has resisted "host contention"
explanations across many sessions, and it is the same defect class the
card table's own `CRATONVM_DBG_RSET_AUDIT` was built for (BouncyCastle
`FixedPointTest` premature reclamation).

**Fix** (`gc/src/card_table.rs`): retain-and-reap. A dying thread's
non-empty buffer now STAYS registered (the registry's `Arc` keeps it alive
with no owner); the collector's STW `flush_all` drains it like any live
buffer and reaps orphans (strong_count == 2 ∧ all buckets empty — a dead
thread can never push again). Regression test:
`dying_thread_residual_offsets_survive_until_flush_all` (fails pre-fix).
All 38 card_table tests pass.

**Sibling filed:** G1's per-thread SATB buffers have the same
dying-thread loss shape (`Weak` registry slots) — spun off separately
(G1 is opt-in, not the default collector).

## What was ruled out along the way

- `beb254e31` (old-space reclaim during JIT sweeps) and `a9c580aff`
  (revived selective promotion): A/B env gates (`CRATONVM_OLD_SWEEP_JIT=0`,
  `CRATONVM_NO_SELECTIVE_PROMOTE=1`) showed the disease persists with both
  off. The old sweep does AMPLIFY expression (recycles freed node memory
  faster → more SIGSEGV faces) but is not the root cause.
- A 07-13→07-15 regression window: a control binary built from the "clean
  sweep" tip `ee203e9f1` crashed 4/4 identically at 500m. The 07-13 sweep
  was clean only because it ran below the pressure threshold.
- Write-barrier EMISSION gaps: interpreter putfield, native CAS
  (`compare_and_swap_field`), and JIT putfield/aastore all fire the card
  barrier; there are no JIT CAS intrinsics.

## Validation (fixed binary, both fixes; results under `apps/tomcat/.suite/results/dhh5-*`)

- **500 m amplifier config** (pre-fix: 4/4 CRASH within ~2 min each): 4/4
  PASS 288/288, twice consecutively (8/8 class-runs), corruption telemetry
  silent (1 contained OOB WARN total vs 650–770 per class pre-fix, 0
  `item`/`p` NPEs).
- **Full 64-class family sweep** (1 g, parallel 2, on a box concurrently
  running two more suites + an active cryptominer): **56/64 clean
  288/288, 7 at 287/288**, each a singleton from the catalogued residual
  families (2× WinSock 10053 host abort, 2× teardown
  `ClosedSelectorException`, 1× header-count assertion, 2× mid-read EOF,
  1× `URLClassLoader.ucp` null) — the same order as the best historical
  sweep floor (58/64 + 6×287/288 on 2026-07-13, measured on a quieter
  box) — plus 1× a distinct native stack-overflow singleton. All residuals
  filed in `../../known-issues/tomcat/dohead-post-fix-sporadic-residuals.md`.
- **The doc's two original hang classes**: full 288/288 PASS at 500 m
  (both), 2 g (both), and in the family sweep. The mid-run hang shape and
  the `LifecycleException`/ITSE flake shape occurred **zero** times across
  ~21,000 Tomcat start/stop cycles this session.
- Post-merge (56 fresh dev commits merged in) sanity: 288/288 +
  287/288-with-clean-rerun at 500 m; unit suites: `cratonvm-gc` 788/788,
  `cratonvm-vm` thread tests 412/412.

## Diagnostics added (kept, env-gated, default-inert)

- `BADRECV-Z` (under `CRATONVM_DBG_BADRECV=1`): getfield on a VALID-heap
  but ZEROED (all-zero header) receiver prints the Java frame stack +
  field, capped — names the holder of a freed-while-live reference
  (`vm/src/runtime/interpreter.rs`).
- A2 old-sweep free records (`CRATONVM_DBG_A2=1`): `record_old_sweep_free`
  (kind sentinel 0xFE) preserves pre-free class/slots/size of blocks freed
  by the in-place old sweep so later zero-header hits attribute
  (`gc/src/a2dbg.rs`, hook in `old_gen_gc`).
- `CRATONVM_OLD_SWEEP_JIT=0`: opt-out escape hatch for the in-place old
  sweep on the conservative-roots path (`gc/src/gen_heap.rs`), retained as
  a bisection knob.

## Reproduction / amplification recipe (for future family work)

```powershell
cd apps\tomcat-suite-runner
# florid expression (pre-fix: crashes/EOF floods within ~2 min/class):
.\run-tomcat-suite.ps1 -Vm craton -Jit on -Jdk real -RunName x -TimeoutSec 900 `
  -Parallel 1 -MaxHeap 500m -ClassFilter 'DoHeadInvalidWrite1023ValidWrite1023$'
# clean control: same but -MaxHeap 2g
```

Note: the whole `apps/tomcat-suite-runner` script set was found wiped to
0 bytes (2026-07-14 13:35) and was reconstructed this session
(`run-tomcat-suite.ps1` with a new `-ClassFilter` param, `cdb-hang.ps1`,
`watch-stall.ps1`, `dhh-validate.ps1`); the class list regenerates from
`apps/tomcat/output/testclasses` (646 classes).
