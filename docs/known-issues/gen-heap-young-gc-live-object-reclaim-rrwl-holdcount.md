# Young-GC live-object reclamation corrupts RRWL read-lock hold counts (ES binary-docvalues IMSE / probe hang)

Status: OPEN — root-caused to the GC family, exact reclamation hole not yet
isolated; a fast (<60s), highly reliable standalone repro now exists, plus a
regression window on dev.

Date filed: 2026-07-07 (split out of
`elasticsearch-lucene-binary-docvalues-range-hangs.md`, whose three original
root causes are fixed and whose doc is retired to `docs/internal/`).

## Summary

Under the JIT-active young-GC mode (non-moving sweep + selective promotion),
with sufficient young-GC frequency and heavy `ReentrantReadWriteLock`
read/write churn, **live young objects are reclaimed (zeroed) by the sweep
while still referenced** — observed directly on
`java/lang/ThreadLocal$ThreadLocalMap$Entry` (the RRWL `readHolds`
per-thread hold-counter storage) and on the repro's own static fields.

Downstream faces of the same corruption, all reproduced this session:

- `LongRandomBinaryDocValuesRangeQueryTests.testAllEqual` (JIT on) either
  fails in ~70s with `java.lang.IllegalMonitorStateException: attempt to
  unlock read lock, not locked by current thread` (`Sync.tryReleaseShared`
  finds a fresh count-0 HoldCounter because the thread's ThreadLocalMap
  entry was lost) or stalls in the RRWL contention phase — run-dependent,
  same bimodality as the standalone probe below.
- The standalone RRWL probe (8 readers + 1 cycling writer) hangs permanently:
  a reader's read count leaks (its entry lost, or the reader thread dies),
  `state`'s read count never returns to 0, the writer parks forever and all
  readers queue behind it — every contender ends parked at
  `AbstractQueuedLongSynchronizer.acquire` bci 368 (`LockSupport.park`),
  matching the historical ES hang signature exactly.
- Probe reader threads sometimes exit their `while (!stop)` loop with `stop`
  still false and die "normally" (no exception) — a corrupted read of the
  probe's own `static` field. Not an uncaught-exception-reporting bug; the
  `dispatchUncaughtException` path was verified correct.
- `[sweep-zero] RECLAIMED-LIVE receiver ptr=...: zeroed by non-moving sweep;
  invoked as java/lang/ThreadLocal$ThreadLocalMap$Entry.refersTo` under
  `ThreadLocalMap.cleanSomeSlots <- set <- ThreadLocal.set <-
  Sync.tryAcquireShared`, with the swept record showing an ALREADY all-zero
  header (`class_id=0 kind=0`) at sweep time.

## What is definitively ruled out (all verified this session, 2026-07-07)

1. **Any JIT miscompile of any specific method.** With `CRATONVM_JIT_BISECT_SKIP`
   covering ALL 32 methods that ever publish a compiled artifact in the probe
   (verified via `CRATONVM_DBG_JITC` audit per run), the hang still occurs.
   The prior session's "22 candidates ruled out" list was chasing a premise
   that was wrong from the start — no compiled method is the culprit, so
   skip-listing can never fix it.
2. **JIT-specificity itself.** `--nojit` + `CRATONVM_DBG_GC_STRESS=200000`
   (forced-frequent young GC) completes reliably; any JIT-active config with
   the same GC stress hangs — including configs where nothing meaningful is
   compiled. The differential is the GC MODE: any thread in JIT code routes
   collections to the non-moving sweep (`sweep_young_non_moving`), while
   nojit uses the moving path. "JIT-specific" in prior notes was an artifact
   of (a) that mode flip and (b) JIT throughput raising allocation/GC rates.
3. **Weak-reference clearing.** `CRATONVM_DBG_WATCHREF=1` across a full hang
   run: ~26k weak KEEP decisions, 0 CLEARs.
4. **`Reference.refersTo` false-negatives.** New gated diagnostic
   (`CRATONVM_DBG_REFERSTO=1`, commit on this branch) logs any
   `refersTo`/`refersTo0` answering false with both sides non-null: zero hits
   across hang runs.
5. **Lost `unpark` via Thread-mirror registry lookup.** New gated diagnostic
   (`CRATONVM_DBG_UNPARK_MISS=1`): zero lookup misses across hang runs.
6. **Selective promotion / evacuation.** `CRATONVM_NO_SELECTIVE_PROMOTE=1`
   still hangs — the pure non-moving sweep path suffices.
7. **The `is_object_address` strictness added by `a3728860`** (gen_heap hunk:
   `header_reserved_fields_plausible` + array_length/COMPACT shape rules).
   Reverting just that hunk on current dev does not stop the hang.

## Repro (Windows box, <60s per run)

Probe source: `docs/known-issues/repros/rwl-holdcount/RwlReadTearingProbe.java`
— 8 reader threads doing `readLock().lock(); sum += shared; unlock()` against
1 writer cycling `writeLock().lock(); shared=++v; unlock()`, 8s duration,
then join. (`RwlReadDominantProbe.java` beside it is the read-only-after-one-
write control shape, which does NOT reproduce.) Compile with jdk-25 javac.

```powershell
& $exe --java-home "C:\Program Files\Java\jdk-25" -cp <dir> RwlReadTearingProbe 8 8000
```

- Current dev (`c15cee62`): hangs ~6/6 (plain, no env needed).
- Any config + `CRATONVM_DBG_GC_STRESS=200000`: hangs (JIT) / completes (nojit).
- Diagnosis battery: `CRATONVM_DBG_SWEEP_ZERO=1` (RECLAIMED-LIVE attribution),
  `CRATONVM_DBG_SWEEP_EDGES=1` (inbound-edge classifier — NOTE: fixed on this
  branch to respect the Family-A side-mark set; before that fix it reported
  every live object as "unmarked" and its (1)/(2)/(3) edge hits were noise),
  `--stack-dump-on-timeout N` (all-parked-at-bci-368 confirmation).

## Regression window on dev (hang RATE, this box, 8s/45s probe)

- `f6aa11c9` (2026-07-05 23:18 build): 0 hangs / 3 runs, completions FAST
  (~100-120M probe ops).
- `a1bf33fb` ("fix: wildfly bugs", mid-window): 1 hang / 8 runs; completions
  ~20x slower (~6M ops) than f6aa11c9.
- `007e620a` (2026-07-06 00:33 build) and later (`2a425550`, `f9a740da`,
  `c15cee62`): hang ~always (6/6 observed at tip); non-hang runs degrade to a
  crawl regime (~7 lock ops in 8s — corrupted-state near-livelock, lock
  handoffs effectively serialized).

So the family got dramatically amplified between f6aa11c9 and 007e620a
(prime candidate: the `a3728860` WildFly sweep — the only commit in the
window touching xt_root_scan/gc_barrier/thread_registry/jvm_thread/gen_heap —
minus its already-ruled-out gen_heap hunk; `b09fea46` "guard JIT virtual
dispatch against cid0 receivers" and the concurrent_mark trio are also in
range). A single-commit verdict needs ~8-16 probe runs per bisect step
because the hang is probabilistic below the tip; `git bisect` between
f6aa11c9..007e620a with the probe is the mechanical path.

## Relationship to other open issues (very likely the same family)

- `reference_randctx_weakhashmap_jit_suspect` — RandomizedContext
  `WeakHashMap<Thread,...>` entry loss, "JIT-only, --nojit ok": same shape
  (weak-keyed table entry loss under JIT-active GC mode).
- `elasticsearch-engine-merge-policy-hangs.md`'s 2026-07-05 note — "GC:
  inconsistent header - kind=Object but array_length=512; inline-alloc forgot
  kind=Array" + `mark_young: rejecting object ... implausible extent`.
- The blocked-thread / young-sweep all-zero-header family
  (`reference_blocked_thread_gc_gap`, Tomcat DoHead teardown corruption).

## Impact

Blocks end-to-end closure of
`IntegerRandomBinaryDocValuesRangeQueryTests` /
`LongRandomBinaryDocValuesRangeQueryTests` (the classes now FAIL fast with
IMSE instead of hanging the suite — strictly better, but not green), and
plausibly a broad set of GC-pressure-sensitive suite flakiness.

## Next steps

1. Finish the probabilistic bisect (f6aa11c9..007e620a, probe x8 per step) to
   name the amplifying commit; audit it rather than revert blindly — the
   pre-window state still hung at low rate on Linux (2026-07-06 Azure notes),
   so the underlying hole predates the window.
2. Instrument the sweep-zero RECLAIMED-LIVE consumer to also print the A2
   breadcrumb (`CRATONVM_DBG_A2`) for the reclaimed address — distinguishes
   never-header-written / allocated-then-clobbered / double-allocated at the
   exact victim.
3. ~~Re-run the (now-fixed) `CRATONVM_DBG_SWEEP_EDGES` classifier~~ DONE
   (2026-07-07, this branch's binary, GC-stress probe run): with side-marks
   respected the classifier reports sane counts (e.g. `marked=17 unmarked=2`)
   and **zero** (1)/(2)/(3) edge hits across every sweep of the run — i.e.
   the swept-live victims' only reference is outside roots+cards+heap:
   **case (a), a register/native-stack root the sweep cannot see, or a
   sweep-walk/sizing defect**. This narrows the hunt to the conservative
   root coverage of running/suspended threads (xt takeover, root snapshots,
   TLAB tails) and the sweep's linear-walk bookkeeping — NOT mark/seed logic.
