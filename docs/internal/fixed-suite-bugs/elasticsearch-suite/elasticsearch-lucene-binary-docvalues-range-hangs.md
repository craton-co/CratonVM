# Elasticsearch Lucene binary doc-values range query hangs

Status: RETIRED to docs/internal 2026-07-07 — all three root causes this doc
tracked are FIXED. End-to-end `testAllEqual` runs on the Windows box confirm
the failure mode changed accordingly: the historical DETERMINISTIC silent
stall (identical every run, including `--nojit`) is replaced by run-dependent
bimodal behaviour — the method now either FAILS fast (~70s) with the concrete
`IllegalMonitorStateException` this doc predicted from hold-count corruption,
or still stalls (observed once at a 400s cap under heavy box load). BOTH
remaining faces are now attributed, with direct evidence, to a DIFFERENT,
independently characterized defect — young-GC live-object reclamation
corrupting RRWL read-lock hold counts — tracked by the now-fixed retired doc
`../gen-heap-young-gc-live-object-reclaim-rrwl-holdcount-FIXED.md`,
which also retires this doc's "separate JIT-specific reader-vs-writer hang"
residual (it was never a JIT miscompile at all — see the 2026-07-07 final
update at the bottom).

Date observed: 2026-07-02
Date updated: 2026-07-07 (final update — retirement)

## Summary

Two Lucene binary doc-values range query tests hang under CratonVM until the
suite runner kills the process at the requested 300-second timeout. HotSpot
passes the same classes.

Root-caused to **two independent bugs** stacked in the same test run:

1. **FIXED** (branch `fix/es-binary-docvalues-range-hang`, merged to dev):
   CratonVM's JIT permanently blacklisted ANY method containing an
   `invokedynamic` instruction ANYWHERE in its bytecode, even on a dead code
   path — the classic case being `assert cond : "msg" + var;`, whose message
   string-concatenation compiles to `invokedynamic` behind a
   `$assertionsDisabled` guard. `org.apache.lucene.util.fst.NodeHash.add`,
   `NodeHash$PagedGrowableHash.nodesEqual`, and
   `FSTCompiler$UnCompiledNode.addArc`/`.replaceLast` — hot, per-FST-node
   methods exercised heavily while merging the term dictionary for
   `int_range_dv_field`/`long_range_dv_field` — all contain such an assert and
   were permanently stuck in the interpreter, a ~100-300x slowdown that alone
   was enough to blow the suite's 300s timeout for the Integer/Long range
   variants (Integer and Long share `RangeType.LONG`'s encode/query path,
   which produces far more distinct FST nodes than Float/Double, explaining
   why only these two classes hung). Fixed by making the JIT scanner accept
   `invokedynamic` and lowering it to an unconditional jump to the existing
   "uncommon trap" deopt stub (`DeoptReason::UnreachedCode`) instead of
   vetoing the whole method — see the commit on this branch for the full
   design and the standalone `FSTCompiler`/`NodeHash` stress repro used to
   isolate it without needing the ES harness.

2. **FIXED** (two commits, both merged to dev — `d6f26b56` on
   `investigate/es-lrucache-rrwl-contention` and a follow-up `96656a8e` on the
   same branch): root-caused the `ReentrantReadWriteLock`/`LRUQueryCache`
   contention from #2 above to a genuine permanent-hang bug, not just
   slowness, requiring two rounds to fully close:

   - **Round 1** (`d6f26b56`): JDK 25's `ReentrantReadWriteLock$Sync` extends
     the newer 64-bit-state `AbstractQueuedLongSynchronizer`, not the classic
     int-state `AbstractQueuedSynchronizer`. CratonVM has a hand-maintained
     JIT skip-list (`../../../../vm/src/jit/skip_list.rs`) banning compilation of the
     classic class's `acquire`/`release`/`signalNext`/`ConditionObject`
     methods — allocate-then-putfield register-allocation miscompile,
     corrupting the waiter linked list and losing wakeups permanently.
     `AbstractQueuedLongSynchronizer` is a near-line-for-line port with the
     identical hazard, but being a textually distinct class name matched none
     of the existing entries. Added the missing sibling entries plus
     `ReentrantReadWriteLock$Sync`/`HoldCounter`.
   - **Round 2** (`96656a8e`): round 1 did not actually take effect. An
     unrelated concurrent commit (`a4913d8b`, "disable callee-saved GPR local
     homes") had gated the *entire* `is_known_miscompile` skip-list —
     including round 1's brand-new entries — behind
     `callee_saved_gpr_local_homes_enabled()`, an env var that defaults to
     off on x86_64. That commit's premise (every skip-list entry belonged to
     one now-closed regalloc family) does not hold for this family: a
     16-thread heavy-contention repro reproduces a permanent hang for BOTH
     `ReentrantReadWriteLock` *and plain `ReentrantLock`* with the gate at
     its default value, and `CRATONVM_DBG_JITC` tracing pinned the actually-
     compiled culprits as `AbstractQueued(Long)?Synchronizer$Node`'s
     `getAndUnsetStatus`/`clearStatus` and the synchronizer's own
     `tryInitializeHead` (allocates the CLH queue's sentinel node and
     `casHead`s it in — same hazard, but reachable only once per lock
     instance's first contended acquire, hence invisible to light-contention
     repros) — none of which were ever on the historical list at all, for
     either AQS variant. Fixed with a new, unconditional
     `is_known_miscompile_aqs_family` covering the `Node` class and every
     queue-management helper for both variants.

   Verified after round 2: a 16-thread heavy-contention repro for both
   `ReentrantReadWriteLock` and `ReentrantLock` now completes with the exact
   correct result (was an unconditional, unbounded hang before). `cargo test
   -p cratonvm-jit`/`-p cratonvm-vm` (115 test groups) green on both rounds
   and after merging. `bt18` checksum unchanged (`68332206`).

3. **OPEN, narrower residual — now root-caused to read-lock hold-count
   bookkeeping**: with #1 and #2 (both rounds) fixed and merged, the real
   `LongRandomBinaryDocValuesRangeQueryTests` now makes *substantial,
   concrete* progress — it used to stall permanently on the very first test
   method (`testRandomTiny`); it now runs through most of the class and
   reaches `testAllEqual` before stalling. At that point two `TaskExecutor`
   pool worker threads are permanently parked at the identical bytecode
   offset (bci 368, the `LockSupport.park(this)` call site) inside
   `AbstractQueuedLongSynchronizer.acquire`'s 6-arg overload, contending for
   the same `LRUQueryCache` write lock via `LRUQueryCache.putIfAbsent` →
   `ReentrantReadWriteLock$WriteLock.lock()`. No thread holds/uses the lock
   at dump time — a genuine lost wakeup, not slowness.

   **Re-ran on 2026-07-05 against current `dev` tip** with a direct
   `-Dtests.method=testAllEqual` `org.junit.runner.JUnitCore` invocation and
   a 90s `--stack-dump-on-timeout`: the hang reproduces identically, and
   **reproduces byte-for-byte identically with `--nojit`** (interpreter
   only) — this rules out the JIT/skip-list entirely. One specific
   hypothesis (the `ThreadRegistry::thread_obj_to_park` reverse-index keyed
   by a raw, GC-move-sensitive pointer) was checked and ruled out —
   `update_thread_objs_after_gc` (`../../../../vm/src/threading/thread_registry.rs`)
   already re-keys it on every moving GC, wired up at
   `vm/src/memory/gc.rs:568`. Likewise `MonitorTable::cas_locks`
   (`../../../../vm/src/threading/monitor.rs`) has an equivalent, well-tested
   `remap_after_gc`. A hypothesis that `Unsafe.compareAndSetReference`'s
   argument-recovery path (`recover_object_arg`,
   `../../../../native-builtins/src/lib.rs`) silently coerces a corrupted CAS argument to
   `null` was also checked with temporary diagnostic logging and did **not**
   fire during the repro — ruled out.

   **Breakthrough**: adding temporary `tracing::warn!` calls to `park()`/
   `unpark()` (`../../../../vm/src/vm/vm_exec.rs`) to trace the exact call sequence
   changed the race's timing enough that, instead of hanging, the exact same
   repro threw a **real, concrete exception**:
   `java.lang.IllegalMonitorStateException: attempt to unlock read lock, not
   locked by current thread` — thrown by
   `ReentrantReadWriteLock$Sync.tryReleaseShared` (via its private
   `unmatchedUnlockException()` helper) when a thread's read-lock hold count,
   as tracked by the `firstReader`/`firstReaderHoldCount` fast-path fields or
   the `cachedHoldCounter`/`readHolds` (`ThreadLocal<HoldCounter>`) fallback,
   doesn't show a positive count for the releasing thread. `testAllEqual`
   hits the cache via the **read** lock (`LRUQueryCache.get`) on 999 of its
   1000 identical searches (only the first is a miss going through the write
   lock), so this points squarely at read-lock hold-count tracking under
   heavy read contention — a substantially more tractable lead than "silent
   hang, cause unknown".

   **Root mechanism, confirmed by code inspection (not just a theory):**
   `firstReader`, `firstReaderHoldCount`, `cachedHoldCounter`, and `readHolds`
   are all plain `transient` (non-`volatile`) fields in real JDK
   (`java.util.concurrent.locks.ReentrantReadWriteLock$Sync`, confirmed via
   `javap`). Real JDK's design deliberately relies on a well-known, legal JMM
   pattern: a plain field write on one thread, followed by a `volatile`/CAS
   write to `state` (declared `volatile long state` in
   `AbstractQueuedLongSynchronizer`), "publishes" that plain write to any
   other thread that later does a synchronizing read of `state` — this works
   in real JVMs because a plain `int`/reference field fits in one
   machine word, which is naturally atomic on real hardware even without a
   fence. **CratonVM breaks this assumption**: every heap field slot is a
   16-byte tagged `Value` (`../../../../gc/src/heap.rs`), and the plain-field accessors
   `get_field`/`set_field` (used for ordinary, non-`volatile` `getfield`/
   `putfield`) read/write that 16-byte slot via bare `std::ptr::read`/
   `std::ptr::write` — **no lock, no fence, not even basic tear-freedom**.
   By contrast, the `volatile`-field accessors `get_field_volatile`/
   `set_field_volatile` (and `compare_and_swap_field`, used for `state`)
   correctly protect against exactly this by acquiring a per-slot stripe
   lock (`cratonvm_gc::collector::volatile_stripe_lock`) plus `SeqCst`
   fences — a comment on `get_field_volatile` already explains why: "the
   on-heap Value slot is 16 bytes, wider than any stable Rust atomic on
   x86-64... a concurrent writer mid-store would expose a torn (tag,
   payload) pair". That exact tearing risk applies equally to `firstReader`/
   `firstReaderHoldCount`, which get NONE of that protection because they're
   plain fields, not volatile ones. Under heavy contention (many threads
   racing to become/stop being "the first reader", exactly `testAllEqual`'s
   999-read-lock-hit shape), a torn 16-byte read/write of `firstReader` can
   yield a `Value` with a mismatched tag/payload pair — corrupting the
   reference-equality check `firstReader == currentThread` or the hold count
   in ways that can either silently swallow a signal (the observed hang) or
   surface as the exact thrown exception observed here, depending on timing.

   This is very likely a **broader-than-this-bug architectural gap**: any
   JDK-internal (or user) code relying on "plain field write is atomic
   because it's word-sized" — a legal, common pattern in real JVMs — is
   potentially exposed wherever CratonVM's 16-byte boxed `Value`
   representation makes that assumption false. Fixing it properly means
   giving plain field slot access at least tear-freedom (not necessarily
   full ordering) uniformly, which touches one of the hottest code paths in
   the whole VM and needs real performance evaluation — this is a
   substantially bigger, riskier change than anything else in this
   investigation and deserves its own dedicated, carefully-scoped effort
   rather than a quick patch. Not yet fixed — the temporary diagnostic
   tracing used to surface this (`../../../../vm/src/vm/vm_exec.rs` park/unpark trace
   logging) was reverted (not committed) after confirming the finding; a
   real fix needs to close the plain-slot tearing gap, then
   reconfirmation that the hang (not just the exception under perturbed
   timing) is resolved.

   `testAllEqual`'s shape: `Arrays.fill()` of `atLeast(1000)` range entries
   with a single random range repeated identically, then `verify()` — every
   leaf search during verification uses an `equals()`-identical query,
   hammering the *same* `LRUQueryCache` entry far more intensely than the
   varied-bounds tests (more contended acquire/release cycles → higher
   chance of hitting the race). Two increasingly targeted standalone
   Lucene-only repros built earlier to isolate this outside the full
   ES/randomizedtesting harness did NOT reproduce it standalone — consistent
   with this being a narrow, heavy-contention-only ordering race rather than
   a structural bug reachable by any read/write mix.

   Separately and NOT believed related (no code in this investigation
   touches GC header-writing, only JIT compile-eligibility): a `bt18` run
   during round-2 verification logged transient `GC: inconsistent header`/
   `mark_young: rejecting object ... corrupt header` warnings that hadn't
   appeared in earlier runs, before the GC's own defensive re-sync path
   recovered and the checksum still matched exactly. Likely from an
   unrelated concurrent commit on `dev`; flagged here in case it resurfaces
   elsewhere, but out of scope for this investigation.

**FIXED (2026-07-06, branch `fix/plain-field-slot-tearing`):** root-caused
the plain-field tearing gap all the way to `cratonvm_types::read_value_atomic`/
`write_value_atomic` (`../../../../types/src/value.rs`) -- already-proven, already-merged
primitives from an earlier, narrower fix (commit `4e6b560f`,
"atomic-per-word object-slot access for concurrent marking") that closed the
GC-marker-vs-JIT-store tearing race but never touched the INTERPRETER's own
plain `get_field`/`set_field` path. Two ordinary mutator threads doing plain
`getfield`/`putfield` on the SAME field slot could still tear each other's
writes through that path -- exactly what real JDK code like
`ReentrantReadWriteLock$Sync`'s plain `firstReader`/`firstReaderHoldCount`
legally relies on being tear-free.

Wired the existing atomic helpers into the three heap backends that had their
own independent, still-raw `ptr::read`/`ptr::write` slot access:
`../../../../gc/src/heap.rs` (`Heap::read_slot`/`write_slot`), `../../../../gc/src/gen_heap.rs`
(`GenerationalHeap::read_slot`/`write_slot` -- the default collector, added a
new `cratonvm_types::read_value_checked_atomic` to keep its existing
HIB-CV-32 corrupt-cell discriminant guard), and `../../../../gc/src/g1.rs`
(`G1Collector::get_field`/`set_field`'s common non-humongous path; the rare
humongous-object multi-region-copy path is a separate, much narrower
residual, not touched here since it doesn't apply to small objects like
`ReentrantReadWriteLock$Sync`).

Verified:
- New regression test `../../../../gc/tests/plain_field_no_tearing.rs`: two threads
  racing plain `set_field`/`get_field` on the same slot (both an `Int` and an
  `Object` variant) via `GenerationalHeap`, asserting the reader only ever
  observes one of the two legitimately-written values across millions of
  iterations -- passes.
- Full regression suite green: `cratonvm-gc` (764), `cratonvm-types` (306),
  `cratonvm-vm` (thousands of tests across ~90 integration test binaries,
  all passing except pre-existing/unrelated issues -- see below), `cratonvm-jit`
  (873, same 4 pre-existing `aarch64` release-mode failures as before, `../../../../jit/src/aarch64.rs`
  untouched by this diff).
- Two issues encountered during verification are PRE-EXISTING on unmodified
  `dev` (confirmed by reproducing them on a clean checkout with zero local
  changes) and unrelated to this fix -- flagged separately rather than fixed
  here: (1) `cargo test -p cratonvm-vm --release --lib` SIGSEGVs partway
  through, around `threading::monitor::tests::cas_lock_idle_dead_entry_is_reclaimed`;
  (2) a handful of `runtime::lock_order`/`jit::skip_list`/`runtime::frame`
  tests fail in `--release` builds because they assert on `debug_assert!`-only
  panics, compiled out in release (same category as the already-known
  `aarch64` release-mode failures).
- Micro-benchmark (plain int/reference/long field get+set in a tight loop,
  50M iterations, `--nojit` to isolate the interpreter path this fix touches):
  ~1-4% overhead vs. an unfixed baseline built from the same commit, within
  run-to-run noise -- consistent with the original `4e6b560f` commit's own
  "perf-neutral on x86" finding for the same 2x-`AtomicU64`-relaxed technique.

**Residual: could not get a minimal standalone Java repro to fail-then-pass
across this fix.** Consistent with EVERY prior attempt in this investigation
(see the two earlier failed Lucene-only repros above), a hand-rolled
`ReentrantReadWriteLock` stress probe matching `testAllEqual`'s actual shape
(one initial write-lock cycle, then sustained multi-threaded read-lock-only
contention) completes cleanly on BOTH the unfixed baseline and this fix --
neither reproduces a hang, so this probe shape doesn't exercise the actual
trigger condition either (matching the historical pattern that this bug
seemingly needs the real Lucene/ES code paths, not a minimal JDK-only
repro). A DIFFERENT probe shape (sustained reader threads PLUS a
continuously-cycling writer, unlike testAllEqual's single upfront write) does
reliably hang on CratonVM regardless of this fix -- but confirmed via
`--nojit` and a real-HotSpot control run that this is a SEPARATE, JIT-specific
bug, not the plain-field tearing gap this fix addresses (flagged
separately, see the project tracking for this investigation). Given the
already-strong code-level evidence (the layout invariant, the reused
already-proven atomic primitives, the new regression test, zero suite
regressions), this fix is being merged on that evidence rather than blocked
on an elusive minimal repro -- but the real `LongRandomBinaryDocValuesRangeQueryTests
testAllEqual` end-to-end run still needs to happen on a host with the ES
checkout (the Windows box, not this Linux build host) to fully close out
root cause #3.

**Follow-up on the separate JIT-specific reader-vs-writer bug (2026-07-06):**
investigated with `CRATONVM_DBG_JITC=1` compile tracing and direct
rebuild-and-retest bisection (forcing suspect methods onto the JIT skip-list
one batch at a time). Ruled out: 7 small AQS/RRWL helper methods that get
JIT-compiled during the repro (`readLock`, `getState`, `compareAndSetState`,
`sharedCount`, `exclusiveCount`, `readerShouldBlock`,
`apparentlyFirstQueuedIsExclusive`) and the entire `java.lang.ThreadLocal`/
`ThreadLocalMap` family (12 methods) -- forcing all of these to interpret
does NOT fix the hang. Also ruled out as a red herring: the reader loop
(`lambda$main$0`-shaped methods with a `try/finally` around `unlock()`) never
successfully OSR-compiles at all (0 successes across 105 attempts in one
run) because such a method's bytecode contains `athrow` (the standard
javac try-finally lowering), and CratonVM deliberately never OSR-compiles an
`athrow`-containing method -- by design, not a bug.

Along the way, found and fixed a genuinely separate, real bug: `jit_getfield`
(`../../../../vm/src/jit/helpers.rs`) -- the native helper JIT-compiled code calls to
execute a `getfield` -- read the 16-byte `Value` slot via a bare, non-atomic
`std::ptr::read`, asymmetric with `jit_putfield_int/long/float/double/object`
(which already use `write_value_atomic`, per commit `4e6b560f`). This is the
exact same tearing gap as root cause #3's mechanism, just in the JIT's own
field-read helper rather than the interpreter's `get_field` -- a real,
independently-valuable fix (own regression test added), but empirically
confirmed via rebuild-and-retest that it does NOT fix this specific
reader-vs-continuously-cycling-writer hang either.

Net result: the actual mechanism behind this second, JIT-specific hang
remains unidentified after 22 candidate methods ruled out plus one real (but
insufficient) bug fixed and merged. Flagged for a future session with a
narrower, more targeted approach (e.g. binary-search bisection of the
FULL compiled-method set rather than trace-informed guessing, or JIT-compiled
code disassembly via `CRATONVM_DBG_JIT_DISASM`) -- see the project tracking
for this investigation for the full list of what's been ruled out, so a
future session doesn't repeat the same 22-method sweep.

## Current full-suite result

Run `es-current-full-jiton-20260702`, `all[1..2701]`, CratonVM JIT-on,
`-TimeoutSec 300` found 2 CratonVM-only `HANG` rows in this family.

Representative row:

```text
index=2163
module=server
class=org.elasticsearch.lucene.queries.IntegerRandomBinaryDocValuesRangeQueryTests
CratonVM=HANG, 300.200s
HotSpot=PASS, 15.336s
```

Affected classes:

```text
org.elasticsearch.lucene.queries.IntegerRandomBinaryDocValuesRangeQueryTests
org.elasticsearch.lucene.queries.LongRandomBinaryDocValuesRangeQueryTests
```

## Repro

```powershell
powershell.exe -NoProfile -ExecutionPolicy Bypass `
  -File apps\elasticsearch-suite-runner\run-elasticsearch-suite.ps1 `
  -Vm craton -Category all -Jit on -Start 2163 -Count 1 -Parallel 1 -TimeoutSec 300 `
  -RunName es-binary-docvalues-range-hang-repro-20260702 `
  -ElasticsearchRoot C:\craton\CratonVM\apps\elasticsearch `
  -WorkDir C:\craton\CratonVM-elasticsearch-current-suite-20260702\apps\elasticsearch-suite-runner\.suite `
  -Exe C:\craton\CratonVM-elasticsearch-current-suite-20260702\target\release\cratonvm-elasticsearch-current-suite-20260702.exe
```

## Evidence

```text
C:\craton\CratonVM-elasticsearch-current-suite-20260702\apps\elasticsearch-suite-runner\.suite\results\es-current-full-jiton-20260702\all-jit\results.tsv
C:\craton\CratonVM-elasticsearch-current-suite-20260702\apps\elasticsearch-suite-runner\.suite\results\es-current-full-jiton-20260702\all-jit\logs\server.org.elasticsearch.lucene.queries.IntegerRandomBinaryDocValuesRangeQueryTests.out.log
C:\craton\CratonVM-elasticsearch-full-suite-20260702\apps\elasticsearch-suite-runner\.suite\results\es-full-hotspot-20260702\hotspot-jit\results.tsv
```

## 2026-07-07 final update: end-to-end confirmation done; residual re-diagnosed and split out; doc retired

Work on branch `fix/es-binary-docvalues-range-close-20260706` (worktree
`C:\craton\CratonVM-es-bdvr-close-20260706`, unique binary
`cratonvm-es-bdvr-close-20260706.exe`), base `c15cee62`.

**End-to-end confirmation of root causes #1–#3 (the pending item):** direct
`JUnitCore -Dtests.method=testAllEqual` runs of
`LongRandomBinaryDocValuesRangeQueryTests` on the Windows box with the real
ES checkout (seed B17AC9D3E1F2A0C4). The historical behaviour — a
DETERMINISTIC silent stall at `testAllEqual` reproducing identically on
every run including `--nojit` (two TaskExecutor workers parked forever at
`AbstractQueuedLongSynchronizer.acquire` bci 368) — is replaced by
run-dependent bimodal behaviour: run 1 completed the search workload and
failed in ~70s (JUnit `Time: 70.19`) with
`java.lang.IllegalMonitorStateException: attempt to unlock read lock, not
locked by current thread` — exactly the exception this doc's root-cause-#3
analysis predicted surfaces when read-lock hold-count bookkeeping is
corrupted and the silent-lost-wakeup path is closed; a later run under heavy
box load reached the RRWL contention phase (~49s in) and stalled to a 400s
cap. #1 (invokedynamic JIT blacklist), #2 (AQS skip-list gaps, both rounds)
and #3's tearing mechanism (interpreter + jit_getfield atomic slot access)
are confirmed effective; BOTH remaining faces (fast IMSE / stall) are the
new GC defect's two documented regimes (its standalone probe shows the same
IMSE-or-hang bimodality with identical stack signatures).

**The surviving IMSE failure is a different defect, now properly
characterized** (it also subsumes the "separate JIT-specific
reader-vs-writer hang" residual this doc carried):

- It is NOT a JIT miscompile. With `CRATONVM_JIT_BISECT_SKIP` covering every
  method that publishes a compiled artifact in the repro (audited per-run via
  `CRATONVM_DBG_JITC`), the probe still hangs. The prior session's 22-method
  rule-out list was chasing a wrong premise — no compiled method is the
  culprit, which is why skip-listing "never worked".
- The true differential is the GC mode + frequency: any thread executing JIT
  code routes young collections to the non-moving sweep
  (`sweep_young_non_moving`); under GC pressure that path reclaims (zeroes)
  live young objects. Directly observed via `CRATONVM_DBG_SWEEP_ZERO`:
  `RECLAIMED-LIVE ... invoked as java/lang/ThreadLocal$ThreadLocalMap$Entry.
  refersTo` under `Sync.tryAcquireShared` — the RRWL `readHolds` per-thread
  hold-counter storage. Lost entry → fresh count-0 HoldCounter at unlock →
  IMSE (the ES failure), or leaked read count → writer parks forever → total
  hang (the probe failure). `--nojit` + `CRATONVM_DBG_GC_STRESS` completes;
  JIT-active + same stress hangs even with nothing meaningful compiled.
- Also ruled out with new gated diagnostics (merged on this branch): GC weak
  ref clearing (0 CLEARs in 26k decisions), `refersTo` false-negatives
  (`CRATONVM_DBG_REFERSTO`), unpark Thread-mirror lookup misses
  (`CRATONVM_DBG_UNPARK_MISS`).
- dev regression data: hang rate on the standalone probe went from 0/3
  (f6aa11c9, 2026-07-05) through 1/8 (a1bf33fb) to ~6/6 at tip (c15cee62) —
  the amplifying window is f6aa11c9..007e620a.

Full evidence, repro recipe (probe sources committed under
`../../../known-issues/repros/rwl-holdcount`), ruled-out list, regression data
and final validation live in the now-fixed retired doc:
`../gen-heap-young-gc-live-object-reclaim-rrwl-holdcount-FIXED.md`.

Per the known-issues triage rule, this doc's primary defects (the three
root causes) are fixed and the residual is tracked by the now-fixed RRWL doc,
so this doc stays retired in `..`.
