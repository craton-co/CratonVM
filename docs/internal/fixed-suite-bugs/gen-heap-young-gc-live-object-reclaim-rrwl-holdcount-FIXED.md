# Young-GC live-object reclamation corrupts RRWL read-lock hold counts (ES binary-docvalues IMSE / probe hang)

Status: RETIRED to `docs/internal/fixed-suite-bugs` 2026-07-08. The
primary reference-processing hole was fixed on 2026-07-07; the crawl and
ThreadIdentifiers subcases were fixed on 2026-07-07/08; the final tracked
extreme-GC-stress RRWL residual is fixed by the 2026-07-08 Linux helper-window
JIT-root scan plus GC-safe native ClassLoader allocation pinning described in
the final section below. Direct ES `testAllEqual` rerun was not possible on the
Azure host because neither `/data/data/cratonvm/apps/elasticsearch` nor
`server/build/craton-testcp.txt` exists there; the standalone RRWL mechanism
and its stress residual now pass focused validation, so this canonical issue
note is retired.

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
- archived [`elasticsearch-suite/elasticsearch-engine-merge-policy-hangs.md`](../internal/fixed-suite-bugs/elasticsearch-engine-merge-policy-hangs.md)'s 2026-07-05 note — "GC:
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

## 2026-07-07 root cause and fix (primary hole)

**The bug was never object reclamation in the common face — it was
reference-processing survivorship.** `ThreadLocalMap$Entry` IS a
`WeakReference`. Before every collection,
`weakref_null_referents_pre_gc` nulls every active weak/phantom referent
slot (so the marker cannot keep referents alive through their References);
after the collection the restore pass writes the referent back into every
SURVIVING Reference object. Survival is judged by
`pointer_map.contains_key(addr) || heap.is_addr_live(addr)` — and for the
Generational heap `is_addr_live` was **old-gen-only**, while the NON-MOVING
young sweep keeps survivors in place with **no pointer_map entries**. Net
effect, every JIT-active young GC:

- live young Reference objects were judged dead → their nulled referent was
  never restored → `ThreadLocalMap` saw `refersTo(null) == true` → the
  MUTATOR ITSELF expunged the live entry → RRWL `readHolds` hold count lost
  → `IllegalMonitorStateException: attempt to unlock read lock` (reader dies
  or, in the probe, exits silently) → leaked read count → writer parks
  forever → the all-parked bci-368 hang;
- live young entries were pruned from the reference processor
  (`remove_collected`), compounding across cycles;
- the same mechanism ate `WeakHashMap<Thread,…>` entries — the
  RandomizedContext `getPerThread()` NPE residual, whose earlier fix
  (watched REFERENTS → identity pointer_map entries) covered only half the
  predicate: the Reference OBJECTS' side was still judged dead.

Why every prior instrument stayed silent: nothing is wrongly swept in this
face (the Entry stays alive, header intact — SWEEP_ZERO/holder scans find
nothing), no weak ref is CLEARED (the referent side was already patched —
WATCHREF shows KEEPs only), and `refersTo` computes honestly on a
genuinely-nulled slot.

**Fix (two complementary halves, both merged):**
1. `weakref_null_referents_pre_gc` now watches the Reference OBJECTS' own
   addresses alongside their referents, so the sweep's existing
   watched-survivor machinery mints identity `pointer_map` entries for them
   (`vm/src/runtime/interpreter.rs`; regression test
   `vm/src/vm.rs::weakref_pre_gc_watch_includes_reference_objects`, note the
   vm.rs tests module is `--features synthetic-jdk`-gated).
2. `VmHeap::is_addr_live` (Generational arm) now also recognizes
   kept-in-place young survivors via
   `GenerationalHeap::is_live_young_survivor` — in current from-space +
   non-zero first header word (the sweep zeroes everything it reclaims;
   sound only in the post-collection STW window where reference processing
   runs, which is exactly where the predicate is used). This half is robust
   where the identity-entry half is not: desync-retained walk stretches
   whose survivors the sweep never visits (common at ES heap sizes).

**Verification:** probe plain mode 0/16 hangs (was ~6/6), ~200M ops/8s
healthy; `cargo test -p cratonvm-gc` green (768+ tests);
`MappingStatsTests` NPE-free (14/14 run, 1 pre-existing separate
serialization-diff failure).

**Bisect postscript:** the dev "regression window" (f6aa11c9→007e620a,
hang 0/3 → ~6/6) bisected to `5117469c` ("Fix Spring messaging rsocket
failures") — which contains nothing GC- or lock-related. Perturbing the
GOOD-era binary (12 threads instead of 8) reproduced the corrupted regime
directly, proving the window was **layout/timing amplification of the
pre-existing hole**, not an introduced bug. Do not revert anything.

## Remaining OPEN faces (this doc stays open for these)

1. **ES `LongRandomBinaryDocValuesRangeQueryTests.testAllEqual` still red**,
   bimodal: ~2/3 of runs stall in STARTUP (the crawl regime below; never
   reach the test body), ~1/3 reach the body and fail with the same IMSE at
   ~60s. The IMSE face therefore has at least one more mechanism beyond the
   fixed refproc hole — full-JIT-compiled ThreadLocalMap/AQS paths and
   conservative JIT-frame coverage are the standing suspects (the probe's
   IMSE/hang went away; ES differs in real compiled frames + much bigger
   young gen). All gated diagnostics stayed silent on a stall run except 19
   benign hash-probe `refersTo` misses.
2. **Extreme-GC-stress hang residual**: with `CRATONVM_DBG_GC_STRESS=200000`
   (young GC every ~200KB — diagnostic sledgehammer, ~1000x production
   cadence) the probe still hangs ~2/5. Plain mode is clean. Likely the
   genuine register/native-window conservative-coverage gap (case (a)).
3. **Crawl regime**: runs (probe AND ES startup) sometimes degenerate to
   ~1 lock handoff/second from t=0 (probe total=4..11 in 8s; 31s wall for a
   10s workload). Predates the window (reproduced on f6aa11c9 with 12
   threads). Completes, so no correctness loss proven — but it gates ES
   startup within timeouts and likely explains a broad class of
   "HANG at N s" suite rows. Untriaged mechanism; suspect STW-pause
   domination (GC-per-handoff feedback) rather than lock-protocol failure.

Repro assets and diagnostic env vars are unchanged (see above). Forensic
additions on the branch: `CRATONVM_DBG_SWEEP_CENSUS` (per-cycle swept-class
census + per-victim young/old holder scan + roots-membership check),
`CRATONVM_SWEEP_FULL_OLD_SCAN` (card-bypass seed), A2 lifecycle dumps at the
RECLAIMED-LIVE / OOB-guard consumers, and `a2dbg::history_at`.

## 2026-07-07 (second pass): crawl regime FIXED; Thread.tid collision FIXED; ES face narrowed further

**Crawl regime — ROOT-CAUSED AND FIXED (class-init missed-notify).** Phase
timestamps in the probe decomposed the "crawl" into (a) a body that
sometimes crawls from t=0 and (b) a constant ~21.4s `Thread.join` tail —
and per-thread exit stamps showed the tail is ONE straggler thread. The
recovery always landed at process lifetime ≈ +30.6s (2s-duration probes:
tail 27.9s; 8s probes: tail 21.37s±15ms), naming a 30-second timeout. It is
the class-INITIALIZATION waiter in `vm/src/vm/vm_util.rs`: it locked the
waiter pair's mutex and called `wait_for(30s)` WITHOUT consulting the
`done` flag that `finalize_class_init` sets under that same mutex before
`notify_all` — a textbook missed-notify: any thread whose class-state check
raced the initializer's completion ate the full 30s. One such stall inside
the RRWL protocol serializes every handoff behind it (the crawl body); at
shutdown it surfaces as the join tail; during Elasticsearch startup several
in sequence blow the suite timeout. Fix: wait only while `!*done`.
Verified: 24/24 probe runs fast (0 crawls, 0 hangs, 0 join tails; wall time
10.1s→7.7s; throughput ~200M→~240M ops/8s); pre-fix baseline was 5/10
tails + 2-4/10 crawls.

**Thread.tid collision — REAL BUG, FIXED.** `TidProbe` (committed beside
the RRWL probes) showed `Thread.threadId()`: main=1, spawned threads
0,1,2… — tid 0 is invalid and tid 1 DUPLICATES main. Two numbering
authorities: Java-constructed threads take `ThreadIdentifiers.next()`
(whose static counter lives in the Unsafe static-long side store and starts
at 0 — see lead below), while VM-fabricated mirrors (main, attached
threads) took `vm_id.max(1)`. Colliding tids break every tid-keyed
algorithm — most relevantly `ReentrantReadWriteLock$Sync`'s
`cachedHoldCounter.tid == LockSupport.getThreadId(current)` check, which
then lets DIFFERENT threads share one hold counter (cross-thread hold-count
corruption ⇒ IMSE / leaked read counts). Fixed by assigning VM-fabricated
tids from a disjoint high range (`(1<<40) + vm_id`). The probe never saw
this because its main thread doesn't touch the lock; ES's test thread does.

**Also fixed:** `Thread.onSpinWait`'s native shadow was
`std::thread::yield_now()` — semantically wrong (HotSpot lowers onSpinWait
to the PAUSE hint); on a loaded host every AQS pre-park spin surrendered a
scheduler quantum. Now `std::hint::spin_loop()`. (Measured: not the crawl's
cause — that was the missed-notify — but wrong and fixed.)

**New autopsy tooling:** `CRATONVM_DBG_IMSE=1` dumps the complete
`RRWL$Sync` hold-count state at the exact `IllegalMonitorStateException`
throw site (firstReader identity, cachedHoldCounter addr/count/tid, and the
current thread's `readHolds` ThreadLocalMap entry, walking the real table).
Also `CRATONVM_DBG_PARKLAT` (unpark→wake latency ≥50ms) and
`CRATONVM_DBG_GCPAUSE` (collections ≥100ms).

**ES `testAllEqual` — STILL RED (the one remaining face).** Post-fixes it
stalls (3/3 at 400s) in the RRWL contention phase; earlier IMSE-face
autopsy captured `cachedHoldCounter{count=0, tid=0}` on a thread whose
`readHolds` map entry was already removed (expected post-throw, since
`tryReleaseShared` removes before throwing). With collisions fixed, the
count=0 cached counter points at a LOST INCREMENT or torn/broken counter
update under full JIT (compiled ThreadLocalMap/HoldCounter paths) — catch
it again with `CRATONVM_DBG_IMSE=1` now that tids are unique; the dump
prints whether the broken invariant is the identity, the cache, or the map.

**Filed lead (FIXED 2026-07-08):** the
`ThreadIdentifiers` counter lead had two parts. First,
`Thread.getNextThreadIdOffset()` returned `0`, so
`ThreadIdentifiers.next()` called `Unsafe.getAndAddLong(null, 0, 1)` and
allocated Java-created tids from a zero-seeded side store (`0,1,2,...`).
Second, null-base Unsafe long operations on registered static offsets used
that side store instead of the VM's real class statics. Current fix returns
a dedicated synthetic `NEXT_TID_OFFSET` seeded to `1` and routes registered
static long offsets (`getLong`, `putLong`, CAS, get-and-add/set,
compare-and-exchange; volatile and plain forms where applicable) through
real static storage.

## 2026-07-08 third pass: ThreadIdentifiers offset fixed; family still open

Root cause for this subcase: the real JDK's
`java/lang/Thread$ThreadIdentifiers.NEXT_TID_OFFSET` is initialized from the
native `Thread.getNextThreadIdOffset()`. CratonVM's stub returned `0`, so the
subsequent `Unsafe.getAndAddLong(null, NEXT_TID_OFFSET, 1)` path used the
generic null-base `static_long_store` at offset 0 and allocated illegal
Java-created tids `0,1,2,...`. That no longer collided with VM-fabricated
main-thread mirrors after the 2026-07-07 high-range split, but tid 0 is still
invalid and leaves RRWL/AQS bookkeeping in a non-HotSpot state.

Fix on branch `codex/fix-young-gc-rrwl-holdcount-20260708-142835`:

- `Thread.getNextThreadIdOffset()` now returns a dedicated synthetic offset
  (`0x20000000` in the validation run) backed by a null-base Unsafe long
  counter seeded to `1`.
- Null-base Unsafe long operations for registered `staticFieldOffset` values
  now hit real VM static storage instead of an independent side store
  (`getLong`, `putLong`, CAS, get-and-add/set, compare-and-exchange; volatile
  and plain forms where applicable).

Focused validation on Azure host `20.83.144.174`, worktree
`/data/data/cratonvm-worktrees/20260708-142835-young-gc-rrwl-holdcount`,
unique binary
`/data/data/cratonvm-worktrees/bin/cratonvm-young-gc-rrwl-holdcount-20260708-142835`:

- `cargo test -p cratonvm-native-builtins unsafe_static_field_offset_tests -- --nocapture`
  passed: 3/3 tests, including the new real-static-storage and seeded tid
  offset regressions.
- Reflective `ThreadIdentifiersFields20260708` now prints
  `NEXT_TID_OFFSET=536870912` instead of `0`.
- Reflective `TidProbeCompat20260708` now prints
  `main=1099511627776`, spawned tids `1,2,3`, and
  `LockSupport.getThreadId` matches each `Thread.threadId()` value.
- Plain `RwlReadTearingProbe 8 8000`: 3/3 completed with `DONE` and no hang.
- Extreme stress `CRATONVM_DBG_GC_STRESS=200000 RwlReadTearingProbe 8 8000`:
  2/3 completed with `DONE`, 1/3 timed out at 45s; logs still show
  `STW cross-thread JIT takeover is still waiting`, stale receiver fallback,
  and implausible/corrupt young headers. This residual remains open.

No direct ES `LongRandomBinaryDocValuesRangeQueryTests.testAllEqual` rerun was
possible on this Azure host: neither `/data/data/cratonvm/apps/elasticsearch`
nor a `server/build/craton-testcp.txt` classpath exists in the available
checkouts. This historical note was superseded by the final 2026-07-08 pass below.

## 2026-07-08 final pass: Linux helper-window roots and native ClassLoader pins

Two remaining gaps explained the extreme `CRATONVM_DBG_GC_STRESS=200000`
RRWL residual after the earlier reference-processing, crawl, and tid fixes.

1. Linux had no implementation for the STW helper-window scan. Blocked threads
   excluded from the cooperative barrier could still have JIT return addresses
   and live object words on their native stacks, but `helper_window_pass` was a
   stub returning `(0, 0)`. The Linux signal rendezvous now has a helper mode:
   it samples blocked peer registers and stack words, classifies a window as
   relevant only when a JIT return address is present, contributes conservative
   object candidates as roots, resumes the peer, and marks the cycle as moving
   young coverage-incomplete when such helper roots are used.
2. The built-in ClassLoader construction path held raw `ObjectRef`s across
   re-entrant native allocations and the `Object.<init>` call used for
   `assertionLock`. Under forced young GC those locals could become stale before
   later field writes, showing up as `java/lang/Object` slot-0/5/6 OOB writes.
   `get_or_create_platform_loader`, `get_or_create_app_loader`,
   `alloc_default_protection_domain`, `alloc_classloader`, and
   `alloc_url_classloader` now pin objects held across those calls and reread
   them through `read_native_pin` before use.

Focused validation on Azure host `20.83.144.174`, worktree
`/data/data/cratonvm-worktrees/20260708-151146-young-gc-rrwl-holdcount-retire`,
unique binary
`/data/data/cratonvm-worktrees/bin/cratonvm-young-gc-rrwl-holdcount-retire-20260708-151146`:

- `cargo check -p cratonvm-vm` passed.
- `cargo test -p cratonvm-vm helper_window_classifier -- --nocapture` passed:
  3/3 tests.
- Release build of the unique binary passed.
- `RwlReadTearingProbe 8 8000` with `CRATONVM_DBG_GC_STRESS=200000`: 10/10
  runs completed with `DONE` at a 45s cap (previous pass: 2/5 completed,
  3/5 timed out; immediately prior ThreadIdentifiers pass: 2/3 completed,
  1/3 timed out).
- Diagnostic census run with `CRATONVM_DBG_STW_CENSUS=1`,
  `CRATONVM_DBG_XT_JIT_ROOT_SCAN=1`, and `CRATONVM_DBG_MTROOTS=1` completed
  with `DONE`.

Direct ES `LongRandomBinaryDocValuesRangeQueryTests.testAllEqual` validation
was still unavailable on this Azure host because the Elasticsearch checkout and
Craton test classpath were absent. Any future ES-suite confirmation should be a
fresh suite-validation note, not a reason to keep this RRWL mechanism document
open.
