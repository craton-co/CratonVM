# Garbage Collection in CratonVM — architecture and current state

CratonVM has three garbage-collector backends behind one dispatcher
(`gc/src/vm_heap.rs::VmHeap`). All are stop-the-world at the collection
level; G1 additionally runs its marking phase concurrently. Selection is
java-launcher-compatible:

| Flag | Backend | One-liner |
|---|---|---|
| `-XX:+UseGenerationalGC` / `-XX:-UseZGC` | `GenerationalHeap` (`gc/src/gen_heap.rs`) | Semi-space young gen + free-list old gen with a concurrent old-gen mark-sweep cycle. Young collections are **moving by default**; each cycle diverts to the non-moving sweep only when its own root-coverage proof fails (see "Backend details" below). |
| `-XX:+UseG1GC` | `G1Collector` (`gc/src/g1.rs`) | Region-based (region size targets ~2048 regions, clamped to 1-32 MB): young/mixed evacuation with remembered sets, SATB concurrent marking, humongous spans, region pinning. The heap is **reserved** at `-Xmx` and committed on demand. |
| *(default)* / `-XX:+UseZGC` / `-XX:+UseZ` | `ZgcRealHeap` (`gc/src/zgc.rs`) | **Not a real ZGC**: a memory-backed, non-moving, whole-heap stop-the-world mark-sweep over one arena, with a hash-set allocation registry. No colored pointers, no load barriers, no concurrency, no compaction. The colored-pointer/`ZPage` code above it in `zgc.rs` (and `zgc_concurrent.rs`) is a metadata-only simulation with no production consumer. |

Unrecognized `-XX:+Use*GC` selectors warn and fall back to Generational.
Heap size comes from `-Xmx`/`-Xms` as usual — and under G1, since F-16, those
two mean what they do on HotSpot: `-Xmx` is the size of the address-space
*reservation* and `-Xms` is the memory committed at startup. The other backends
still allocate their whole `-Xmx` up front.

**ZGC is the default**, and the `zgc` Cargo feature is default-ON (it gates
the `GcAlgorithm::Zgc` variant, so the default cannot be `Zgc` without it). It
is still **not a real ZGC** — everything the table above says about it holds.
The promotion is backed by measured suite behaviour, not maturity: across
every suite with a per-collector sweep (Tomcat, Spring Framework, H2,
Hibernate Reactive), ZGC is at parity with or ahead of Generational on PASS
count, ties or leads on HANG, and has never crashed.

Two consequences worth stating plainly:

* **It costs heap — but not by a known factor.** No compaction means a
  non-compacting collector needs more headroom, and how much is a property of
  the workload's allocation shapes. Raise `-Xmx` to get moving after a
  post-flip `OutOfMemoryError`, and file it: every known instance of that
  shape so far has turned out to be an allocator bug rather than an inherent
  ZGC cost.
* **`-XX:+UseGenerationalGC` is the escape hatch**, in every build. A
  `--no-default-features` build has no ZGC at all and defaults to Generational.

The plan to make this a real, concurrent, generational, compacting ZGC is
[`docs/feature-designs/zgc-production-implementation-plan.md`](feature-designs/zgc-production-implementation-plan.md).

## The VM ↔ GC protocol

**Stop-the-world.** A GC-initiating thread posts a request on the
`GcBarrier` (`vm/src/threading/gc_barrier.rs`); mutators park at
interpreter safepoint polls (allocation sites and backward branches),
each depositing a **root snapshot** first. Threads inside blocking
natives are excluded from the arrival quota and covered by their
deposited snapshot plus a wake-time fixup (`check_post_block_gc`).
Threads stuck in compiled code that never polls are handled by the
**cross-thread JIT takeover** (INT-3, all backends):
the initiator freezes them at OS level, conservatively scans their
registers/stacks, publishes their un-retired TLAB tails as walker skip
regions, and — under G1 — pins every region they can address so nothing
moves under their unrewritable frames. The takeover also covers the four
concurrent-mark STW pauses.

**Roots and remap.** `vm/src/memory/roots.rs::collect_roots` gathers ~20
root families (frames, statics, class locks/mirrors, string pool, JNI
global+local refs, JNI pin set, thread mirrors, native-builtin
singletons, …); each has a paired write-back in
`vm/src/memory/gc.rs::update_all_roots` applying the collection's
pointer map. Parked threads are remapped on resume
(`apply_pointer_map_to_thread`), woken blocked threads via a composed
multi-GC fixup. Conservative JIT-frame roots cannot be rewritten, so
under G1 their regions are **pinned out of the collection set**
(process-global per-thread pin registry in `gc/src/gc_quiescence.rs`);
the Generational young sweep instead simply never moves anything while
JIT frames are live.

**Write barriers.** Reference stores fire an SATB pre-barrier (old value
logged to per-thread buffers spilling into a sharded queue) when a mark
cycle is active, and G1's remembered-set post-barrier. Both are internal
to the `VmHeap::set_field`/`set_array_element` accessors, so every
interpreter/native/JIT-helper store is covered by construction; the JIT's
inline ref-store fast paths bail to the helper unless the backend
publishes region bounds (only Generational does). Statics live in a
Rust-side table and fire the pre-barrier centrally in
`set_static_shared`.

**java.lang.ref.** Weak/Soft/Phantom semantics are driven by the VM
around each collection: referent slots are nulled pre-GC so the tracer
cannot keep referents alive, the shared `ReferenceProcessor`
(`gc/src/reference.rs`) decides clear/enqueue/finalize with a
backend-exact liveness predicate, survivors' referents are restored, and
queue linkage uses the Reference's real `next` field. Finalizable
objects dead in a collection are **resurrected** (evacuated/marked with
their subtree) so `finalize()` runs exactly once against valid memory —
on all three backends. Under G1, references whose referents die in
*uncollected Old regions* are additionally processed at concurrent-mark
completion against the mark bitmap (INT-8: referent-slot hiding during
marking + a `Reference.get()` keep-alive barrier + remark-time
processing callback). `System.gc()` runs a finalizer-aware collection
and also drives the G1 concurrent cycle forward.

## Current correctness state

A systematic review, backed by a deterministic differential probe kit diffed
against HotSpot JDK 25, found and fixed: G1 humongous accounting/reclaim
(IHOP-blind humongous, decay-to-zero IHOP, last-ditch full cycle before
OOM), TLAB gap-sentinel desync in every G1 region walker, initiator-only
JIT pinning, SATB holes (statics side-table, thread-exit buffer loss,
remark seed drops at the gray-set cap), the java.lang.ref protocol
(enqueued weak refs never read cleared; finalize never/thrice), JNI
local-ref scan/remap pairing, ZGC fragmentation + GC-storm latch +
registry/monitor leaks + mark validation, mixed-CSet selection of
liveness-unknown regions, kept-region coherence after evacuation
failure, a Generational remark→sweep TAMS window, and the cross-thread
JIT takeover for G1/ZGC (INT-3) including concurrent-mark pauses.

**Verified invariants** (probe kit): all of
ChurnCheck / HumongousCheck / CopyChurn / MTChurn / RefCheck /
RefCheckOld / SpinPoll / SpinPollMark produce HotSpot-identical output on
Generational, G1 and ZGC at `-Xmx256m`; the gc crate's unit+integration
suites are green.

**Standing invariants the collectors are checked against.** These are asserted
in `gc/`, not just documented:

- The published TLAB skip-offset list is sorted, coalesced and disjoint. Two
  partially overlapping spans would make the sweep walk resync twice and
  silently skip every object between them.
- A TLAB's published reserved tail starts 8-byte aligned. A non-aligned start
  is rounded up (the fail-safe direction) rather than dropped.
- A moving young collection **refuses to run** while a non-empty clipped tail
  set is published: the cycle over-retains, spills to old gen and retries,
  instead of relocating over a TLAB some mutator left un-retired.
- `OldGen::free` returns exactly the extent `alloc` reserved. An unrounded
  return would leave a remainder off the free list, and `walk_objects` derives
  allocated extents from the gaps *between* free blocks, so the walk would
  resume at a non-object-start and abandon the rest of the region.
- The old-gen in-place sweep runs a live-set closure before its free loop, so
  an unmarked block still referenced by a marked old-gen object is retained
  transitively rather than handed back.
- A conservative root that lands in a field or a mid-object spill is resolved
  to the object that contains it. Both plausibility screens are exact-base
  tests, so an interior root would otherwise mark nothing and let the sweep
  free a live block under it. The compacting arm cannot honour an interior
  root — a slid object leaves it dangling — and is downgraded to the in-place
  sweep for that cycle.

**Current limitations:**

1. `-XX:+UseZGC` selects the compatibility implementation described
   above, not HotSpot's concurrent colored-pointer ZGC.
2. `-XX:+UseStringDeduplication` is parsed but intentionally inert. The
   raw-address deduplication table must participate in every collector's
   remap and purge protocol before production callers can use it.
3. `CRATONVM_GC=g1-parallel-evac` is an experimental opt-in. Mixed
   collections stay on the serial evacuator, and the serial path remains
   the supported default. The live-object corruption this path used to carry
   is fixed; what keeps it opt-in now is that it has had no gauntlet run and
   still spawns a `thread::scope` worker pool per collection.

The STW barrier race and missing class-unloading driver described by
earlier versions of this page are fixed. For the current class-loader
liveness and reclamation contract, see
[Class-loader unloading and bounded metadata](architecture/class-loader-unloading.md).

## Verifying and debugging

**Probe kit** — `/data/data/gcprobes-0710` on the Linux probe host (self-
checking, deterministic, HotSpot-diffable):

```
./cratonvm --java-home <jdk> -XX:+UseG1GC -Xmx256m -c . ChurnCheck
```

ChurnCheck (linked-list churn + payload verification), HumongousCheck
(multi-region arrays), CopyChurn (ref-array `System.arraycopy` barrier
coverage), MTChurn (monitors + counters under moving GC), RefCheck /
RefCheckOld (weak/soft/finalizer protocol, young and old referents),
SpinPoll / SpinPollMark (never-polling compiled spins vs STW + concurrent
mark), BinaryTrees (deep recursion). Always diff against a real JDK run.

Two generational remembered-set probes live in `bench/` and are ordinary
tracked sources rather than part of the kit above:

* `OldGenRsetProbe [retainedDepth] [rounds] [churnDepth]` -- a large tenured
  set with **no** old-to-young edges, so every old-to-young scan it provokes is
  measurable waste. This is the pause-breakdown probe.
* `OldToYoungEdgeProbe [nodes] [rounds] [churnDepth]` -- its companion, which
  stores freshly allocated young objects into tenured fields and then verifies
  every one of them. This is the probe on which a card-table-only collector can
  actually be wrong, so it is the one to run under `CRATONVM_GC_VERIFY_RSET=1`;
  a verifier run whose `edges` is zero has tested nothing.

**Diagnostics** (env-gated, in the release binary):

| Switch | What it does |
|---|---|
| `--verbose:gc` | Per-pause `[GC-STAT]` lines + exit `[GC-SUMMARY]` |
| `CRATONVM_G1_DBG_REACH=1` | Post-pause BFS-from-roots corruption detector; `[g1][FREED]`/`[WALKBRK]` traces |
| `CRATONVM_G1_DBG_PINS=1` | Per-pause conservative-JIT-pin census |
| `CRATONVM_GC_VERIFY_STALE=1` | Post-GC stale-frame-slot verifier (recycled drain destinations are recognized as benign) |
| `CRATONVM_DBG_WEAKREF=1` | Weak/Phantom null/restore pass tracing |
| `CRATONVM_G1_NO_EVAC_RETRY=1` | Disable the evacuation-failure drain (bisection) |
| `CRATONVM_G1_PARALLEL_EVAC=0` | Force the single-threaded evacuator. Parallel evacuation is the **default**; the worker threads are not respawned per pause. Still owed: a gauntlet-scale soak and a throughput number, so this remains the bisection lever for any suspected parallel-evacuation regression. |
| `CRATONVM_G1_PARALLEL_EVAC_IN_JIT=0` | Restore the serial fallback for pauses taken while a thread is in compiled code. That fallback used to be unconditional, on the stated ground that only the serial path pinned conservative JIT roots — which was stale (the parallel driver applies the same collection-set exclusion). It mattered because on a JIT-warm application it is true for nearly every pause, so G1 copied single-threaded in production. **First thing to try for any G1 crash seen only with the JIT warm** — defect G1-11 lives in this path. |
| `CRATONVM_G1_CLEANUP_WALK=1` | Make the concurrent-cycle cleanup pause recompute per-region liveness by walking every object, instead of reading the byte counter the marker maintains. The walk was cleanup's only implementation until F-06 — an O(heap) STW pass at the end of every cycle. `=1` restores it as the authority; a debug build runs both and asserts they agree. First thing to try if a cycle is suspected of freeing a live Old region. |
| `CRATONVM_G1_ADAPTIVE_IHOP=0` | Restore the pause-time-driven marking threshold. By default the threshold is planned from the measured mark duration and old-generation growth rate and tightened on to-space exhaustion; pause time drives only the young size, which is what it actually describes. |
| `CRATONVM_G1_ADAPTIVE_TENURING=0` | Restore the fixed `promotion_age` (15). By default the tenuring threshold is re-derived after each pause from an age histogram of surviving bytes, and may tenure earlier than configured — never later — when survivor space would overflow. |
| `CRATONVM_G1_RESERVE_HEAP=0` | Commit the whole heap at startup instead of reserving `-Xmx` and committing on demand. Also the state a platform without a reservation implementation is in anyway. First thing to try for a G1 fault at a heap address that looks mapped. |
| `CRATONVM_G1_UNCOMMIT=1` | **Opt-in.** Return the pages of a trailing run of Free regions to the OS at the end of a concurrent-mark cleanup, so a process that has finished a burst does not hold its high-water mark for life. Never shrinks below `-Xms`, and only above the highest region still in use — the committed set has to stay a prefix. Opt-in because the two halves of the reserved heap have different failure modes: getting growth wrong is a missed optimisation, getting the shrink wrong is a fault in compiled code. |
| `CRATONVM_G1_EAGER_HUMONGOUS=0` | Restore cleanup-only humongous reclaim. By default an evacuation pause also frees humongous spans it can prove nothing references. This is the only path that frees memory outside the collection set, so it is the first thing to rule out if a live humongous object goes missing. |
| `CRATONVM_G1_YOUNG_PAUSE_TARGET=1` | **Opt-in.** Let `max_gc_pause_ms` bound the YOUNG generation too, not just the old half of a mixed collection set: G1 also collects once the Eden+Survivor region count reaches an adaptive target, tightened by 20% after any PRODUCTIVE pause that overruns the goal and relaxed while pauses stay under half of it. Does nothing until such an overrun is measured (the target starts at its 60%-of-regions ceiling and a target at the ceiling is not a trigger). Measured trade on `G1ChurnPauseProbe` at `-Xmx2048m`: p50 -21%, p99 +3%, wall +4.2%, one extra pause — see the young-sizing paragraph under Backend details for the full table and why it is not a default. |
| `CRATONVM_G1_WORKERS=<n>` | Force the evacuation worker count; `=1` drains the parallel path serially, which separates a concurrency race from a logic divergence. Overrides `-XX:ParallelGCThreads`, which in turn overrides the machine-derived default |
| `CRATONVM_DBG_GC_STRESS=<bytes>` | Force young GCs every N allocated bytes (Generational) |
| `CRATONVM_GC_PAR_THREADS=<n>` | Generational young-GC worker count. `0`/`1` forces the sequential collector; `>= 2` forces that many workers regardless of heap size. Unset = `min(available_parallelism, 8)` once the young gen passes the size floor. `available_parallelism` follows CPU affinity, so a `taskset -c N` run is automatically sequential |
| `CRATONVM_GC_PAR_MIN_BYTES=<bytes>` | Young-gen size floor below which the young GC stays sequential (default 16 MiB) |
| `CRATONVM_GC_PAR_EVAC=0` | Force the single-threaded evacuator for the Generational **moving** (Cheney) young cycle. Parallel evacuation is the default, but it engages only where `CRATONVM_GC_PAR_THREADS` policy already asks for two or more workers, the cycle is a moving one, and to-space has room for the survivors plus one per-worker buffer each — so a run that never sees it is common and expected. Workers run on the same persistent `evac_pool` threads G1 uses (parked on a condvar, never respawned per pause), claim to-space in per-cycle buffers sized against the live set off one atomic cursor, promote through the old generation's allocator under a lock, and claim each object with a tagged CAS on its mark word (the same copy-then-CAS protocol G1 uses); a retired buffer's tail is stamped with a `TLAB_FILLER`/`GAP_FILLER` sentinel so the arena stays walkable as the next cycle's from-space. Both evacuators seed from the same three sources and produce the same forwarding map, so `=0` is a one-run bisection lever rather than a behaviour switch. |
| — | `--verbose:gc` prints `[GC] par_evac: cycles=… helper_scans=… cas_losses=… declined_for_slack=… filler_bytes=… promotions=… deferred_cards=…` at exit, unconditionally, including the all-zero line. Read `cycles` first: a zero says the path never engaged, which is a different claim from "it engaged and did nothing". `helper_scans` says whether it was actually *parallel* — a cycle where the driver did everything and the helpers scanned nothing is a load-balancing regression that every correctness test in the suite passes. `promotions` and `deferred_cards` are the two arms whose absence would not fail immediately: promotion is the only shared-lock contention point in the copy phase, and a deferred card is the old→young edge whose loss surfaces a cycle later somewhere else — a zero in either means whatever you ran never exercised it. `declined_for_slack` should stay at 0; a young GC triggers with from-space ~99.9% full, so the slack the Cheney invariant leaves is small (measured: 121 KB out of 128 MB on bt18) and the cycle runs bufferless rather than declining. `filler_bytes` prices the per-worker buffering, and is 0 on a bufferless cycle. |
| — | **Driving the copy phase in a soak.** A plain benchmark run yields one moving cycle or none. `CRATONVM_DBG_GC_STRESS=250000` turns that into ~500–1000 per process, which is what makes a soak mean anything: `CRATONVM_GC_PAR_THREADS=8 CRATONVM_MOVING_YOUNG=1 CRATONVM_DBG_GC_STRESS=250000 cratonvm --XX:UseGc Generational -Xmx256m --verbose:gc -cp … BinT 14`. Use oracles that are not a second run of this VM — `bench/BinT.java` sums to `10 * (2^(d+1) − 1)`, and `bench/HashMapOnly.java` / `bench/StringRegexOnly.java` document their checksums in their own headers. |
| `CRATONVM_GC_SWEEP_ANCHOR_STRIDE=<bytes>` | Byte spacing of the parallel-sweep anchors (default 8 MiB). Also sizes the MOVING path's parallel object-start-walk chunks. Lower it to drive either on a small young gen under `CRATONVM_DBG_GC_STRESS` |
| `CRATONVM_GC_VERIFY_RSET=1` | After each young collection's old->young seeding, walk the whole old generation and report `[rset-verify] site=.. edges=N missing=M seeded=S`. `missing > 0` names an edge the card table did not deliver, and the first one's referrer/class/slot. **Read `edges` too**: `missing=0` on a run that found no edges at all is vacuous, not clean. Costs a full old-gen walk per young GC |
| `CRATONVM_GC_FULL_RSET_SCAN=1` | Restore the pre-2026-09-02 whole-old-generation old->young walk on every young collection. The revert lever for the default flip below; the first thing to try if a premature-reclamation defect is suspected under Generational |
| `CRATONVM_GC_YOUNG_TRIGGER_PERCENT=<n>` | Moving young collection trigger, as a percent of from-space capacity (default 50, clamped 1..=95). Raising it collects less often and copies more survivors per cycle; see "Young sizing" below |

Note: `tracing::debug!` is compiled out of release builds
(`release_max_level_info`); for cycle-phase confirmation attach gdb to
un-inlined gc-crate symbols (LTO is off for `cratonvm-cli` dev builds).

**Tuning knobs** (java-compatible): `-Xmx` (reserved heap under G1),
`-Xms` (committed at startup under G1; accepted and ignored by the other
backends), `-XX:G1HeapRegionSize=<bytes>` (rounded to a power of two and
clamped to 1-32 MB), `-XX:InitiatingHeapOccupancyPercent=<n>` — a **ceiling**
on the adaptive threshold, never a floor, floored at max(1 % of heap, one
region) — `-XX:MaxGCPauseMillis=<n>` (the young-generation size target and the
mixed collection's copy-time budget), `-XX:ParallelGCThreads=<n>`,
`-XX:G1MixedGCLiveThresholdPercent=<n>` (default 85: an Old region at or above
this percent live is never a mixed-collection candidate),
`-XX:G1HeapWastePercent=<n>` (default 5: the mixed phase ends early once the
candidates' garbage is below this percent of the heap),
`-XX:MaxHeapSize`, `-XX:+HeapDumpOnOutOfMemoryError`.

**Reading a G1 pause.** `--verbose:gc` prints one `[GC-STAT]` line per pause
whose phase fields are a *partition* of the pause, not a sample of it:

```
roots_us + rset_us + closure_us + fixup_us + free_us + verify_us + other_us == pause_us
```

`fixup_regions` / `fixup_bytes` are the denominator for `fixup_us` — a long
fix-up on a big old generation and a long fix-up on a small one are different
problems. `verify_us` is the budgeted post-pause dangling-reference sweep, which
runs in release builds (`CRATONVM_G1_VERIFY_BUDGET=0` opts out); it used to be
charged to no phase at all, so the rows did not sum and the difference was
invisible. On the parallel evacuator phases 1-3 are fused into one work-stealing
closure and are reported wholly as `closure_us`, with `roots_us`/`rset_us` zero
— that is the honest reading, not a missing measurement.

## Backend details worth knowing

**Generational.** Young is a pair of semi-spaces with TLAB bump
allocation. A live JIT frame does **not** by itself force the non-moving
sweep: moving-young is on by default, and each cycle diverts to the
sweep only if its per-cycle coverage proof fails, if `promotion_oom_risk`
is honoured, or if `System.gc()` requested a full cycle. A JIT-warm
workload may legitimately spend most cycles non-moving — but that is a
*measured* fallback rate, not a rule, and the running process states its
own answer through `gc_metrics::collector_decision_report()`. (The older
"any JIT frame ⇒ non-moving" rule is reachable today only under
`CRATONVM_NO_MOVING_YOUNG` or `CRATONVM_MOVING_YOUNG_NO_JIT=1`.) That default young
collection is PARALLEL in two phases. The transitive closure is drained
by several workers over a lock-free mark bitmap (one bit per 8 bytes of
from-space) — sound because the phase is pure and read-only on a frozen
heap and the only write is an atomic bit claim. The sweep walk, a linear
header chase that is inherently sequential, is split at anchors the
ALLOCATOR supplies rather than ones a walk rediscovers. `arena.rs` keeps
one verified object start per 4 KiB bucket, armed on `new`/`grow`,
cleared on `reset`, and recorded by the TLAB-refill, young slow-path and
Cheney-copy paths for a shift, a bounds-checked load, a compare and a
per-bucket-once store under a lock the caller already holds. The grid is
completed by the END of every pre-existing free/TLAB-skip block -- a
sweep coalesces dead spans up to a survivor and never past one, so a
region that survived an earlier collection is never re-handed-out and
would otherwise contribute no anchor -- plus offset 0 and `used` as
terminals. Anchors landing inside a free block are filtered out, because
one minted by adjacent uncoalesced blocks would abort the whole parallel
attempt. This replaced a full-arena exact-base oracle walk costing ~240
ms and 2 GiB walked per collection; the same grid, subsampled at
`CRATONVM_GC_SWEEP_ANCHOR_STRIDE`, now costs ~0 ms and 4.7 MB walked,
and the conservative-candidate oracle traverses only those anchor
intervals that actually contain a candidate. The truncated-oracle fail-safe survives as `verified_spans`: an interval
counts as proved only if its chain lands EXACTLY on the next anchor, an
unproved interval has its ranges discarded rather than trusted, and a
candidate outside every proved span falls back to direct validation. Each chunk
re-proves its own anchor by requiring its chain to land exactly on the
next one, and the parallel walker writes nothing — on any grid anomaly
it is abandoned wholesale and the untouched sequential walk (which owns
every diagnostic and the unwind/re-anchor recovery) runs from scratch.
Parallel EVACUATION does not exist here. The moving young gen's
JIT-held-oop corruption is fixed, and it is now the **default**
(`types/src/flags.rs::DEFAULT_MOVING_YOUNG`), with
`CRATONVM_NO_MOVING_YOUNG` as the compatibility opt-out. The flag being
on is not the same as a cycle having compacted: every cycle must carry
its own root-coverage proof, and one that cannot prove complete coverage
diverts to the non-moving sweep rather than relocating — so read
`moving_young: cycles=N coverage_fallbacks=M`, and the
`[GC] decision #N:` line rendered by
`gc_metrics::collector_decision_report()`, before attributing any
cost or behaviour to compaction.
Old gen is a free-list
allocator collected by a VM-driven concurrent cycle (initial mark STW →
concurrent trace → remark STW → concurrent sweep, with a remark-time
TAMS snapshot gating the sweep).

*Where a moving young pause actually goes, and the 2026-09-02 changes.*
`CRATONVM_DBG=gcpause` reports a per-phase breakdown for the MOVING cycle.
Before 2026-09-02, on `bench/OldGenRsetProbe 19 700 16` at `-Xmx1g`
(medians of the 12 collections after the retained set tenures, total median
pause 229 ms):

| phase | before | after | scales with |
|---|---:|---:|---|
| the from-space object-start walk | 120 ms | **22-39 ms** | young *allocated* |
| `full_old_rset_scan` -- the whole old-gen walk | 50 ms | **0 ms** | old live set |
| `cheney_drain` -- copying the survivors | 29 ms | 32-63 ms | young *live* |
| `cardclear+young_reset` | 23 ms | 28-52 ms | card count |
| `scan_dirty_cards` | 8 ms | **0 ms** | old-gen size |
| **total pause** | **229 ms** | **100-133 ms** | |

Only ~12 % of a minor collection copied live objects. The two largest phases
were O(young allocated) and O(old live set), which is the shape a generational
collector exists to avoid. Afterwards the copy is the largest phase, and only
6 of 14 collections still cross the 100 ms threshold `gcpause` reports at.
Five things changed:

* **The object-start walk is parallel.** It is split at the allocator's own
  anchor grid and chunked across `young_gc_threads()` workers, each chunk
  proved by requiring its chain to land exactly on the next anchor -- the same
  contract the non-moving sweep's parallel walk has always had, and which the
  MOVING (default) path did not use. Any refusal abandons the attempt
  wholesale and the untouched sequential walk runs from scratch against a
  FRESH bitmap, because a partially-filled one is worse than none. The walk is
  now its own `objstart_walk` phase mark with `objstart_chunks` /
  `objstart_parallel` counters beside it: `pre_evacuate` also covered the
  safepoint spin and the arena locks, and a 52 % attribution to a mark that
  wide was a hypothesis, not a measurement.
* **The whole-old-generation scan is off by default.** It ran AFTER the
  dirty-card scan had already answered the same question, and made young pause
  time grow permanently with old-gen size. `CRATONVM_GC_FULL_RSET_SCAN=1`
  restores it; `CRATONVM_GC_VERIFY_RSET=1` replaces it, running the same walk
  as a checker that prints `edges=N missing=M` -- the shape G1 already uses for
  its own remembered set.
* **The card map is no longer scanned with atomic RMWs.** `take_dirty_cards`
  used `swap(AcqRel)` on every card byte and `clear_all` stored over every byte
  again: two O(cards) locked passes per cycle, ~8.3 ns/card, measured linear
  from 98 K to 1 M cards while finding nothing. Both now read first (`Acquire`,
  a plain `mov`) and write only the bytes that are genuinely dirty.
* **The write barrier marks the card directly.** The interpreter/native
  barrier went through a TLS lookup, an `Arc`, a `parking_lot::Mutex` and a
  growable `Vec` per reference store, with no deduplication; it now performs
  the same conditional byte store the JIT's inline barrier emits, so there is
  one card-marking rule in the VM instead of two.
* **The compiled reference store is NOT part of this batch.** An inline SATB
  gate was written for it here and then withdrawn: `ref_store_pre_gate` (helper
  ABI v10) had landed on dev first and is a strict superset -- it gates the
  post barrier and a young-age floor as well, and does not require the field's
  old value to be null. Shipping a second mechanism into the same emitter is
  how `region_bounds_addr` came to mean two things at once. Note that those
  gates are published by ZGC only: `ref_store_gates()` requires all three
  slots and Generational cannot express its post-barrier as an age floor (it
  keys on `GC_FLAG_OLD_GEN`, a mask test), so under `-XX:+UseGenerationalGC`
  every compiled reference store still pays the helper call. Closing that is
  its own piece of work.

*Young sizing.* `CRATONVM_GC_YOUNG_TRIGGER_PERCENT` (default 50) is the
percentage of from-space occupancy that triggers a moving collection. The 50 %
is documented as leaving room for survivors, but to-space has the SAME capacity
as from-space and promotion drains to old gen on top of that, so the copying
collector's real constraint permits considerably more. Raising it collects less
often and copies more survivors per cycle; which effect wins is a property of
the workload's survival rate, which is why this ships as a measurable knob at
its historical default rather than as a new default nobody has swept.

**G1.** A contiguous arena split into fixed regions with an O(log R)
address→region table. Young pauses evacuate all Eden+Survivor regions
except pinned ones (JNI-critical pins, conservative-JIT pins, frozen-peer
tails); remembered sets (per-region source sets fed by the post-barrier
plus a GC-internal rebuild each pause) drive old→young discovery. Mixed
pauses add the most-garbage Old regions bounded by count and copy-time
budget — only regions with liveness data from a completed mark cycle are
eligible. Marking is SATB tri-color with a background worker that also
drains the SATB shards each step; cleanup frees wholly-dead Old regions
in place, reclaims dead humongous spans and arms mixed collections.
Humongous objects (> half a region) occupy physically contiguous region
runs and are never evacuated — but a pause can still free one: after
Phase 5 it reclaims any span that neither a root nor any object its
Phase-4 walk visited refers to, which is what stops short-lived
humongous garbage waiting on a mark cycle that may never fire. Evacuation
failure self-forwards live objects in place, keeps their regions, and a
same-pause drain recovers them; a wedged drain leaves the kept regions
coherent (remembered-set edges recorded, precise liveness answers).
Allocation failure escalates: young pause → synchronous full mark cycle
→ OOM.

*Young sizing, opt-in.* `max_gc_pause_ms` reaches exactly one
decision by default — how many OLD regions a mixed collection set may take.
The young half is bounded by the free pool alone (`needs_gc` fires below
25 % free), so Eden grows to roughly three quarters of `-Xmx` and young
pause time scales with the heap SIZE. `CRATONVM_G1_YOUNG_PAUSE_TARGET=1`
adds an adaptive young-region target: shrink 20 % after a pause that
overruns the goal, give 12.5 % back while pauses stay under half of it,
floor/ceiling 5 %/60 % of regions. It does nothing until a PRODUCTIVE pause
has been measured to overrun — the target starts at its ceiling and a
target at the ceiling is not a trigger — and an unproductive pause (nothing
copied, nothing freed) resets it to the ceiling so it can never storm.

It is **not** a default, and the reason is measured. On
`probes/G1ChurnPauseProbe 96 900` at `-Xmx2048m` (96 MiB retained, 3.6 GiB
of garbage, 200 ms goal), medians of 3 interleaved reps:

| arm | wall | pauses | total pause | p50 | p99 |
|---|---|---|---|---|---|
| pre-audit baseline | 5773 ms | 3 | 3801 ms | 1082 ms | 1726 ms |
| audit fixes, flag OFF | 2633 ms | 3 | 719 ms | 236 ms | 243 ms |
| audit fixes, flag ON | 2744 ms | 4 | 814 ms | 187 ms | 250 ms |

The 7x pause reduction there belongs to the audit's evacuation-destination,
free-region-scan, region-scrub and remembered-set fixes — the middle row has
this flag off. The flag itself buys the third row against the second: p50
−21 %, p99 **+3 %**, wall +4.2 %, one extra pause. p99 is what a pause goal
is about and it did not move, and on an adaptive scheme it cannot: the
target only tightens after a pause has already overrun, so the largest pause
is always paid in full and it is the one p99 reports. A latency-sensitive
workload may still want the median improvement — turn it on and measure your
own pause distribution.

**That table predates five findings, and it was re-run.** The +4.2 % wall-clock
is the cost of taking MORE pauses at the per-pause price of the day, and that
price changed: the parallel evacuator now runs on JIT-warm pauses instead of
falling back to serial (F-01), forwarding moved out of a side hash map (F-02),
collection-set membership stopped being a SipHash lookup on the innermost loop
(F-03), the parallel driver stopped taking the whole-heap fix-up (F-04), and
cleanup stopped walking the heap (F-06).

Re-run 2026-09-02 on the same probe and heap, RELEASE build, eight interleaved
reps per arm, medians:

| | flag OFF | flag ON | delta |
|---|---|---|---|
| p50 pause | 644 ms | 580 ms | **&minus;10 %** |
| max pause | 826 ms | 749 ms | **&minus;9 %** |
| total pause | 2074 ms | 2007 ms | &minus;3 % |
| wall | 8102 ms | 8258 ms | **+1.9 %** |
| pauses | 3 | 5 | +2 |

The trade improved in the predicted direction and by roughly the predicted
amount: the wall-clock cost more than halved (+4.2 % &rarr; +1.9 %), total pause
went from a cost to a small saving, and the reduction now reaches the MAXIMUM
pause as well as the median — which is what a pause goal is actually about, and
what the original measurement could not show.

**It still ships opt-in, and the reason is the host, not the numbers.**
`/proc/loadavg` read 28&ndash;44 throughout, from other work on the machine, and
this document's own rule — the one every number in the older table was taken
under — is that a contended host inverts an A/B of this size. A +1.9 % wall cost
measured at load 40 is not evidence that the default should change. What would
settle it is the same eight reps on an idle machine; the harness and the probe
are both in the tree now, so that is a twenty-minute job rather than a
reconstruction.

One obstacle had to be cleared first: **the probe was not in the tree**. It was
committed with the measurement, then deleted along with the rest of `probes/` by
a "major doc consistency update" while every citation of it survived — including
this document's, which also named the wrong directory. It is restored, and its
`checksum` line is there so a run can be diffed against a real JDK's — all
sixteen runs above produced `checksum=2063754854400`, identical to JDK 25's.

*Where a young pause actually goes.* Every `--verbose:gc`
`[GC-STAT]` line now carries a per-phase breakdown — `roots_us`, `rset_us`,
`closure_us`, `fixup_us`, `free_us`, `verify_us`, `other_us` — with `fixup_us`
printed beside the `fixup_regions` / `fixup_bytes` it covered, because a slow
walk and a large old generation are different problems. The seven fields SUM to
`pause_us`: `verify_us` is the budgeted post-pause dangling-reference sweep,
which runs in release builds and used to be charged to no phase at all, and
`other_us` is the derived remainder. A table whose rows do not sum to the total
cannot be used to argue that a cost was removed rather than moved, which is
exactly what the rest of this section tries to do with it.

The first thing the completed partition showed is a cost nobody had a number
for. On `probes/G1ChurnPauseProbe 8 40` at `-Xmx256m`, ten runs, **`verify_us`
is 10.8-14.5 % of every young pause** — the budgeted post-pause
dangling-reference sweep, which runs in release builds and was previously
charged to no phase at all. It is defensible while G1-11 is open, but it is a
choice, and `CRATONVM_G1_VERIFY_BUDGET` is now a decision an operator can
actually make. (Also from those runs: the seven fields summed to `pause_us`
EXACTLY, all ten times.)

*Parallel evacuation on JIT-warm pauses (F-01), first reading.* Same probe and
heap, five interleaved reps per arm, one pause per run:

| arm | median pause | median wall |
|---|---|---|
| `CRATONVM_G1_PARALLEL_EVAC_IN_JIT=0` (the old fallback) | 305 ms | 7606 ms |
| default | 268 ms | 7683 ms |

Pause −12 %, wall unchanged, and the probe's checksum was identical across all
ten runs and equal to a real JDK 25's. Read it as a direction, not a
measurement: it is a **debug build** on a host running two other agents'
compiles, so the absolute numbers mean nothing and the copy loop's share is not
the release build's. The release-build version of this is owed, together with
the young-sizing re-run above.

One asymmetry in it is real and expected: the parallel arm's fix-up walked 21-26
regions against the serial arm's 12. N workers claim N to-space regions, so more
regions are "written into" and the narrowed Phase-4 set is correspondingly
wider. The parallel closure pays for it and then some. On `probes/G1ChurnPauseProbe 96 900`
at `-Xmx2048m` a 330 ms young pause split: roots 0.7 %, remembered-set
walks 0.03 %, Cheney closure 38 %, whole-heap fix-up 10-18 %, freeing the
collection set **42 %**. That last figure is why the phase breakdown exists
at all: the reclaim phase was the most expensive part of a G1 pause and
nobody had ever looked.

It was `G1Region::reset` scrubbing every reclaimed region — 1.61 GB at
11.9 GB/s, which is memset bandwidth and nothing else — and it was
redundant with the allocator's own zeroing (`bump_alloc` zeroes exactly the
range it hands out; `alloc_humongous_locked` zeroes its whole span; no
inter-object padding can exist because every object size is a multiple of
8; no walker reads a `Free` region). Removed: single-binary A/B, medians of
3 interleaved reps, identical program checksums in both arms — p50 403 ->
265 ms, p99 412 -> 267 ms, total pause 1213 -> 790 ms, the free phase
itself 152 -> 18 ms. `CRATONVM_G1_SCRUB_FREE=1` restores it, and that is
the first thing to try if a G1 heap-corruption investigation wants the old
"a freed region reads as zeros" world back.

*The inline G1 write barrier reaches only one of the two JIT tiers.*
`CRATONVM_G1_INLINE_BARRIER=1` emits a real G1 post-write barrier inline
(null test, same-region test, out-of-line helper for anything those two cannot
dismiss) from all four reference-store emitters in the single-pass /
bytecode-walk tier. It cannot reach the IR tier at all: `ir_lower` has **no
reference-store site**, as its own `read_bounds_addr` doc states — it asks only
the read-side "is this address mapped" question — so a method the IR tier
compiles keeps the out-of-line `putfield_object` helper whatever the flag says.

Measured, not inferred. With `RUST_LOG=cratonvm_jit=info` the emitter logs
`jit: G1 inline post-write barrier ACTIVE` once per process: it appears on
`apps/g1_probe/G1CardChurn` with the flag on and never with it off, and never on
`probes/G1ChurnPauseProbe` in either arm. So the workload that exhibits the
barrier and the workload that exhibits pause behaviour are different ones, which
is why the flag ships opt-in with no pause-level number — a `jit/` gap, not a
`gc/` one.

Watch the log filter when checking this: a bare `RUST_LOG=info` shows nothing,
because the launcher builds its filter as
`from_default_env().add_directive(WARN)` and a global WARN ties with a global
`info` on specificity, resolving last-added-wins. Use the target-scoped form.

*Free-region search: measured, and still linear.* Both searches
(`find_free_region_from`, `find_contiguous_free`) are O(regions), and the
region count used to grow with `-Xmx`. `[GC] g1 free-scan:` reports calls,
regions probed and the worst single scan for each. Two readings:

| workload | single | contiguous |
|---|---|---|
| young churn, 2048 regions | 1543 calls, 1543 probed, **worst 1** | never called |
| 400 humongous allocations, 256 regions | 15 calls, 1027 probed, worst 195 | 400 calls, 35943 probed, **worst 227** |

The ordinary path is free — the rotating hint answers in one probe, always. The
humongous path scans most of the heap per call, and that is still 36,000
comparisons of an enum against a constant across a whole run, on a path that
then memsets megabytes. A hint for it was tried and measured at 0.9% (35,943 →
35,617 probes) and dropped: the free-scan cursor tracks single-region Eden
claims and has no relationship to where a humongous span was freed.

What bounds it is the region-size ergonomic above: ~2048 regions at any heap
size means the scan is bounded by a constant rather than by `-Xmx`, which is the
property the concern was actually about. A free-region bitmap would buy those
comparisons at the price of a second source of truth for "is this region Free" —
read in ~200 places, written in 8 — and one that says Free about a live region
hands the allocator memory that is in use. Refused on the number; the instrument
stays so it can be revisited against a workload.

*The Phase-4 walk, narrowed.* A young pause's reference fix-up walks only the
collection set's remembered-set sources plus every region the pause WROTE
INTO — not every object of every non-CSet region, which would make pause
time O(live heap) rather than O(young live set). `CRATONVM_G1_NARROW_FIXUP=0`
restores the whole-heap walk, and is the first lever to pull for any
suspected G1 dangling-reference or lost-edge defect.

Why that set is sufficient: a slot needing a forwarding rewrite points at an
evacuated object, so it lives in a root (Phase 1 rewrites those), in the CSet
(Phase 3 scans every to-space copy), or in a non-CSet region reachable only
through the remembered set (Phase 2 walks exactly those). The remembered set
is complete because every mutator reference store reaches
`post_write_barrier_rset` — the interpreter's and every native's directly,
and every JIT-compiled one through `jit_putfield_object` since G1-2 closed.
The walk's other job, the GC-internal edge rebuild, only concerns regions the
pause wrote into, and those are found by diffing a pre-evacuation
`(region_type, cursor)` snapshot rather than by asking the evacuator — so no
allocation path, present or future, can forget to register itself. The
humongous census still forces the wide walk, because "nothing in the heap
references this span" is a whole-heap claim.

Scope: the SERIAL young pause. Mixed pauses and the parallel evacuator still
walk wide.

Measured (single binary, one env flag, interleaved, `G1ChurnPauseProbe`): on
a workload whose live set stays YOUNG the narrow set equals the wide one and
nothing changes — `fixup_regions` is 116 in both arms. On one whose live set
has settled into Old (`-Xmx512m`, 48 MiB live, 12 GiB of garbage) the walk
drops from **60 regions / 58 MiB to 1 region / 0 MiB**, p50 27 -> 17 ms,
total pause -12%, p99 unchanged, checksums identical. The size of the win is
the ratio of settled old generation to young — which is the shape the
original criticism was always about.

Verification, and its limits. The unit suite CANNOT discriminate this change:
every pre-existing test passes even when Phase 4 walks nothing, because on
every constructible fixture the mutator barrier alone already records every
edge. The tests that do discriminate check the narrow SET's composition. The
consequence is checked at runtime instead — `CRATONVM_G1_DBG_RSET=1` verifies
after each pause that every cross-region edge into a collectable region is
named in that region's remembered set, and prints `edges=N missing=M` so a
green result cannot hide a vacuous one. On the shape that genuinely skips 59
of 60 regions it reports `edges=2114 missing=0`, and a unit test proves that
checker can fail (clear every remembered set and all 2114 are reported
missing). What is still owed is a suite-scale soak.

**ZgcRealHeap.** One arena + free list (post-sweep coalesced) + hash-set
registry of allocation bases. `needs_gc` triggers at 75 % occupancy with
a post-sweep re-arm so a large live set cannot storm. The sweep prunes
dead bases in place and feeds the exact dead list to the monitor
registry. Non-moving ⇒ the pointer map is always empty and
no barriers are needed; reference semantics come entirely from the VM-level
protocol.

**Mutators DO have TLABs on this backend.** `VmHeap::refill_tlab` returns
`None` for `VmHeap::Zgc` — the buffers are not reached that way.
`ZgcRealHeap::alloc_raw_tlab` (over `gc/src/zgc/tlab.rs`)
is the funnel for every object and every array, it is **on by default**, and
`CRATONVM_ZGC_TLAB=0` is the kill switch. A TLAB chunk is *reserved* space that
no collection can reclaim while its owning thread lives, so it is invisible to
any trigger that counts live bytes; the reservation budget is bounded by the
live buffer count (`ZGC_TLAB_RESERVATION_SHARE`) for exactly that reason.

The arena is **two-ended**: small objects and TLAB chunks bump up from offset 0,
allocations at or above `ZGC_LARGE_OBJECT_MIN` (64 KiB — the size no TLAB will
ever serve) bump *down* from capacity with their own free list, and a reserve
(`capacity / 8`) keeps the low end from consuming the whole large-object end.

The always-empty pointer map and the neutral `VmHeap::Zgc` arms that go
with it are correct *only* while the collector is non-moving, and they fail
silently rather than loudly if it ever moves an object. That is tracked as
Phase 4 of
[`docs/feature-designs/zgc-production-implementation-plan.md`](feature-designs/zgc-production-implementation-plan.md),
which must land before any compaction does.
