# Garbage Collection in CratonVM — architecture and current state

CratonVM has three garbage-collector backends behind one dispatcher
(`gc/src/vm_heap.rs::VmHeap`). All are stop-the-world at the collection
level; G1 additionally runs its marking phase concurrently. Selection is
java-launcher-compatible:

| Flag | Backend | One-liner |
|---|---|---|
| `-XX:+UseGenerationalGC` / `-XX:-UseZGC` | `GenerationalHeap` (`gc/src/gen_heap.rs`) | Semi-space young gen + free-list old gen with a concurrent old-gen mark-sweep cycle. Young collections are **moving by default**; each cycle diverts to the non-moving sweep only when its own root-coverage proof fails (see "Backend details" below). |
| `-XX:+UseG1GC` | `G1Collector` (`gc/src/g1.rs`) | Region-based (region size targets ~2048 regions, clamped to 1-32 MB): young/mixed evacuation with remembered sets, SATB concurrent marking, humongous spans, region pinning. The heap is **reserved** at `-Xmx` and committed on demand. |
| *(default)* / `-XX:+UseZGC` / `-XX:+UseZ` | `ZgcRealHeap` (`gc/src/zgc.rs`) | **Not a real ZGC**: a memory-backed, whole-heap mark-sweep over one arena with a bitmap allocation registry. No colored pointers, and **the load barrier is never armed** — `relocate_active` and `set_barrier_color(Some(..))` are written only by unit tests, so marking is published by an ordinary **SATB pre-write barrier** rather than on read. It is not non-compacting. **Default ON:** a stop-the-world sliding compactor (`CRATONVM_ZGC_RELOCATE`, default ON since 2026-08-13, `=0` restores the non-moving sweep), parallel by default (`CRATONVM_ZGC_PAR_RELOCATE`), which runs on every cycle whose per-cycle coverage proof holds. **Opt-in:** concurrent marking (`CRATONVM_ZGC_CONC_START=<percent>` or `auto`), parallel STW marking (`CRATONVM_ZGC_PARMARK=<workers>`) and a generational mode (`CRATONVM_ZGC_GENERATIONAL=1`). So a DEFAULT run is a stop-the-world whole-heap mark (serial) and sweep, followed by a parallel sliding compaction. The colored-pointer/`ZPage` code above it in `zgc.rs` (and `zgc_concurrent.rs`) has production consumers now — `forwarding::ZRelocationSet::select` ranks the slide's pages — though the barrier layer itself remains unreached. |

Unrecognized `-XX:+Use*GC` selectors warn and fall back to Generational.
Heap size comes from `-Xmx`/`-Xms` as usual, and on every backend those two
mean what they do on HotSpot: `-Xmx` is the size of the address-space
*reservation* and `-Xms` is the memory committed at startup. An `-Xms` above
`-Xmx` is clamped rather than refused.

**`-Xms` is honoured on all three backends** since 2026-09-21. G1 got it first
(F-16); ZGC commits the requested prefix of its single arena, and Generational
commits the request across its two young semi-spaces. Until 2026-09-20 the two
non-G1 backends accepted the flag and dropped it *in silence*, on the default
collector — `VmHeap::new_with_heap_sizing` now makes each backend arm state
what it did with the value (`XmsDisposition`), which is what stops that
returning. See
[`internal/gc/heap-xms-xmx-mean-different-things-per-backend-20260920-RETIRED-20260921.md`](internal/gc/heap-xms-xmx-mean-different-things-per-backend-20260920-RETIRED-20260921.md).

Two things `-Xms` deliberately does **not** do. It never sizes a *reservation*:
on ZGC the arena envelope is captured once and read without the arena lock by
`conservative_addr_span`, the object-start bitmap and the JIT read-bounds
publication, so an `-Xms` that shrank it would produce a heap that can never
reach `-Xmx`. And on Generational it commands the young pair only — the old
generation is a `Vec` committed in full at construction, so the startup commit
there is at least `-Xmx / 2` whatever `-Xms` says.

`VmHeap::os_committed_bytes()` is the number that moves when `-Xms` works;
`committed_bytes()` (`Runtime.totalMemory()`) is a sum of capacities and does
not.

What the other two backends do with `-Xmx` is **not** "allocate all of it up
front", which is what this paragraph used to claim:

* **ZGC** builds one `Arena`, and `Arena` sits on `reservation.rs`'s
  `HeapStore`, which reserves the address space and commits in 2 MiB granules
  as the allocator's cursors reach them. A fresh `-Xmx8g` ZGC process commits
  megabytes, not gigabytes.
* **Generational** is half and half. The two young semi-spaces are `Arena`s and
  reserve exactly as ZGC's does; the **old generation is not** — `OldGen::new`
  is a single `Vec<u8>` of its whole share, which under this constructor's
  50/50 split is half of `-Xmx`. It skips the zero pass, so the pages are not
  *resident* on Linux, but on Windows the allocation takes the full commit
  charge immediately. `-Xmx8g` on Generational charges ~4 GB at startup, and
  `-Xms` cannot lower it — an `-Xms` meant to do that is a larger change:
  putting `OldGen` on `HeapStore` like every other arena in the crate.
* `CRATONVM_GC_RESERVE=0`, and any reservation the OS refuses, put every arena
  back on the wholly-committed `alloc_zeroed` block, where all of `-Xmx` *is*
  charged up front.

**ZGC is the default**, and the `zgc` Cargo feature is default-ON (it gates
the `GcAlgorithm::Zgc` variant, so the default cannot be `Zgc` without it). It
is still **not a real ZGC** — everything the table above says about it holds.
The promotion is backed by measured suite behaviour, not maturity: across
every suite with a per-collector sweep (Tomcat, Spring Framework, H2,
Hibernate Reactive), ZGC is at parity with or ahead of Generational on PASS
count, ties or leads on HANG, and has never crashed.

Two consequences worth stating plainly:

* **It costs heap — but not by a known factor.** The collector compacts only
  on cycles whose coverage proof holds (`CRATONVM_ZGC_RELOCATE`, default ON).
  A workload whose cycles keep falling back to the non-moving sweep fragments
  like a non-compacting collector and needs more headroom. How much is a
  property of the workload's allocation shapes. Raise `-Xmx` to get moving after a
  post-flip `OutOfMemoryError`, and file it: every known instance of that
  shape so far has turned out to be an allocator bug rather than an inherent
  ZGC cost.
* **`-XX:+UseGenerationalGC` is the escape hatch**, in every build. A
  `--no-default-features` build has no ZGC at all and defaults to Generational.

The plan to make this a real, concurrent, generational, compacting ZGC is
[`docs/feature-designs/zgc-roadmap-20260920.md`](feature-designs/zgc-roadmap-20260920.md),
which sequences the four current direction documents into one ordered programme,
states the dependency edges between them, and lists what in
[`zgc-production-implementation-plan.md`](feature-designs/zgc-production-implementation-plan.md)
and
[`zgc-maturity-assessment-and-plan-20260813.md`](feature-designs/zgc-maturity-assessment-and-plan-20260813.md)
has since gone out of date. Read the roadmap first; those two remain the
authority on their own history.

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
the Generational collector instead diverts the whole cycle to its
non-moving young sweep, which moves nothing at all. See the
`divert_non_moving` table under "Backend details" for the exact term and
its opt-out — this paragraph and that section disagreed outright between
2026-07-28 and 2026-09-20, one saying a live JIT frame always forces the
sweep and the other saying it never does by itself.

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
3. G1's **young** evacuation is parallel **by default**, on a persistent
   worker pool. The **mixed** pause is serial by default, but that is now a
   measured decision rather than a missing route:
   `CRATONVM_G1_PARALLEL_MIXED=1` dispatches `mixed_collection` to
   `mixed_collection_parallel`, and `[GC] g1 mixed-route: serial=<n>
   parallel=<n>` (printed unconditionally, `gc_metrics.rs`) is the census
   that says which arm a run took. Leaving it off buys nothing measurable
   and costs nothing: on the shape a mixed pause actually has — an old live
   set reached through the remembered set — **100% of the bytes the pause
   copies are copied in Phase 2, by the driver thread alone**, and this
   lever parallelises Phase 3. Seventeen workers scan; none copies a byte.
   See `docs/internal/g1-2026-09-20/w6p-the-mixed-parallel-number-and-the-phase-it-is-not-about.md`
   for the measurement and
   `docs/internal/fixed-bugs/g1-mixed-collections-never-reach-the-parallel-evacuator-RETIRED-20260921.md`
   for the capability gap this replaced.

   Two claims this entry used to make were false in both halves, which is
   why they are spelled out here. The switch is `CRATONVM_G1_PARALLEL_EVAC`
   and it is `on_unless_zero` (`types/src/flags.rs`) — an opt-**out**, never
   an opt-in, and never spelled `CRATONVM_GC=g1-parallel-evac`. And the
   helpers stopped being a per-collection `thread::scope` when they became
   the persistent `EvacPool` (`gc/src/g1.rs`, `pool.scope`); putting thread
   creation inside every pause is the cost that move removed. A reader
   following the old text would have set a flag that does not exist and
   concluded the default path was serial.

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

> **The spellings in this table were stale until 2026-09-20.** Every per-flag
> `CRATONVM_*` variable below is now a **token in a grouped variable**
> (`cratonvm_types::flag_groups`, expanded by `vm-cli/src/main.rs` before any
> thread starts). The grouped spelling is the supported one; the old per-flag
> name still works — `Resolved::get` falls through to the raw environment — but
> a run that sets one prints, once, at startup:
>
> ```
> [cratonvm] 3 per-flag variable(s) set directly; the supported spelling is now:
>            CRATONVM_DBG=gc-stress CRATONVM_GC=par-threads CRATONVM_GC=moving-young
> ```
>
> (`CRATONVM_DBG=-deprecations` silences it.) The rules, all from
> `flag_groups::resolve`:
>
> * `CRATONVM_<GROUP>=tok` enables, `=-tok` disables, `=+tok` is the same as
>   `=tok`, and tokens are comma-separated: `CRATONVM_GC=-par-evac,verify-rset`.
> * A knob that carries a **value** takes it after a second `=`:
>   `CRATONVM_GC=par-threads=8`. Presence alone means `1`.
> * **An unrecognised token is fatal** — the VM exits 2 rather than run a
>   configuration you did not ask for. A misspelt *legacy* variable is not,
>   because nothing claims it; it is simply ignored. That asymmetry is a reason
>   to prefer the grouped spelling in scripts.
> * `CRATONVM_<GROUP>=all` enables every token in that group.
>
> Per-row verdicts, including two rows in `docs/gc-tuning.md` that named
> variables which **do not exist**, are in
> `docs/internal/reviews/gengc-round2-plumbing2-20260920.md`.

| Switch | What it does |
|---|---|
| `--verbose:gc` | **G1:** per-pause `[GC-STAT]` lines + exit `[GC-SUMMARY]`. **ZGC:** one `[GC] zgc-real:` line per collection. **Generational** *(since 2026-09-20)*: one line per collection, `[GC] generational: cycle=N kind=moving\|non-moving\|refused[+major] young=A->B old=C->D pause=X.XXXms [divert=<reason>]`. `kind` comes from `gc_metrics::last_collector_decision()`, i.e. from the branch that decided, not a re-derivation; `refused` is a cycle that did not collect at all; `divert=` names the `divert_non_moving` term that refused the copy. *(2026-09-21)* A `kind=refused` line carries `refusal=<reason>` instead of `divert=` — nothing was diverted, neither collector ran — and the reason names which of the three refusal causes fired (`skipped-young-to-space-undersized`, `skipped-young-walk-incomplete`, `skipped-young-reserved-tlab-tails`). Before that date this arm emitted an affirmative at `info` level and then no per-collection output for the rest of the run — while the *shutdown* `[GC] …` census did run, which the round-1 write-up overstated (`docs/internal/reviews/gengc-round1-probe-results-20260920.md`). All three backends print the shutdown census |
| `CRATONVM_DBG=g1-dbg-reach` *(alias: `CRATONVM_G1_DBG_REACH=1`)* | Post-pause BFS-from-roots corruption detector; `[g1][FREED]`/`[WALKBRK]` traces |
| `CRATONVM_DBG=g1-dbg-pins` *(alias: `CRATONVM_G1_DBG_PINS=1`)* | Per-pause conservative-JIT-pin census |
| `CRATONVM_DBG=gc-verify-stale` *(alias: `CRATONVM_GC_VERIFY_STALE=1`)* | Post-GC stale-frame-slot verifier (recycled drain destinations are recognized as benign) |
| `CRATONVM_DBG=weakref` *(alias: `CRATONVM_DBG_WEAKREF=1`)* | Weak/Phantom null/restore pass tracing |
| `CRATONVM_GC=-g1-evac-retry` *(alias: `CRATONVM_G1_NO_EVAC_RETRY=1`)* | Disable the evacuation-failure drain (bisection) |
| `CRATONVM_GC=-g1-parallel-evac` *(alias: `CRATONVM_G1_PARALLEL_EVAC=0`)* | Force the single-threaded evacuator. Parallel evacuation is the **default**; the worker threads are not respawned per pause. Still owed: a gauntlet-scale soak and a throughput number, so this remains the bisection lever for any suspected parallel-evacuation regression. |
| `CRATONVM_GC=-g1-parallel-evac-in-jit` *(alias: `CRATONVM_G1_PARALLEL_EVAC_IN_JIT=0`)* | Restore the serial fallback for pauses taken while a thread is in compiled code. That fallback used to be unconditional, on the stated ground that only the serial path pinned conservative JIT roots — which was stale (the parallel driver applies the same collection-set exclusion). It mattered because on a JIT-warm application it is true for nearly every pause, so G1 copied single-threaded in production. **First thing to try for any G1 crash seen only with the JIT warm** — defect G1-11 lives in this path. |
| `CRATONVM_GC=g1-cleanup-walk` *(alias: `CRATONVM_G1_CLEANUP_WALK=1`)* | Make the concurrent-cycle cleanup pause recompute per-region liveness by walking every object, instead of reading the byte counter the marker maintains. The walk was cleanup's only implementation until F-06 — an O(heap) STW pass at the end of every cycle. `=1` restores it as the authority; a debug build runs both and asserts they agree. First thing to try if a cycle is suspected of freeing a live Old region. |
| `CRATONVM_GC=-g1-adaptive-ihop` *(alias: `CRATONVM_G1_ADAPTIVE_IHOP=0`)* | Restore the pause-time-driven marking threshold. By default the threshold is planned from the measured mark duration and old-generation growth rate and tightened on to-space exhaustion; pause time drives only the young size, which is what it actually describes. |
| `CRATONVM_GC=-g1-adaptive-tenuring` *(alias: `CRATONVM_G1_ADAPTIVE_TENURING=0`)* | Restore the fixed `promotion_age` (15). By default the tenuring threshold is re-derived after each pause from an age histogram of surviving bytes, and may tenure earlier than configured — never later — when survivor space would overflow. |
| `CRATONVM_GC=-g1-reserve-heap` *(alias: `CRATONVM_G1_RESERVE_HEAP=0`)* | Commit the whole heap at startup instead of reserving `-Xmx` and committing on demand. Also the state a platform without a reservation implementation is in anyway. First thing to try for a G1 fault at a heap address that looks mapped. |
| `CRATONVM_GC=g1-uncommit` *(alias: `CRATONVM_G1_UNCOMMIT=1`)* | **Opt-in.** Return the pages of a trailing run of Free regions to the OS at the end of a concurrent-mark cleanup, so a process that has finished a burst does not hold its high-water mark for life. Never shrinks below `-Xms`, and only above the highest region still in use — the committed set has to stay a prefix. Opt-in because the two halves of the reserved heap have different failure modes: getting growth wrong is a missed optimisation, getting the shrink wrong is a fault in compiled code. |
| `CRATONVM_GC=-g1-tlab-clamp` *(alias: `CRATONVM_G1_TLAB_CLAMP=0`)* | Restore the pre-2026-09-20 behaviour: `refill_tlab` REFUSES a request larger than half a region instead of clamping it to that bound. The refusal was a one-way cliff — `tlab::TlabPressureTracker` doubles a hot thread's request up to `MAX_TLAB_SIZE` (1 MiB) and the region-size ergonomic gives 1 MiB regions up to a 2 GiB heap, so the ladder's last rung is over the bound, and the sizer only re-sizes on a refill that actually happened. A thread that reached it fell back to per-object allocation through the exclusive guard for the rest of its life. `[GC] g1 tlab: oversize_clamped=` is the engagement counter; a zero means the ladder never climbed that far on your workload. |
| `CRATONVM_GC=-g1-humongous-best-fit` *(alias: `CRATONVM_G1_HUMONGOUS_BEST_FIT=0`)* | Restore first-fit for the humongous contiguous-run search. By default it is best-fit — the SHORTEST run of free regions that still fits, ties to the lowest index — so a small humongous object spends a tight hole instead of eating the head of a long run. G1 has no compaction pass that can manufacture a run back, so a long run consumed by a request that would have fitted in a hole is gone until the regions above it are freed. Both arms return a run of exactly `count` adjacent Free regions, so this is a placement policy, not a correctness switch; first-fit stops at its first hit while best-fit always scans the table, and `[GC] g1 free-scan: contiguous(...)` prices both. |
| `CRATONVM_GC=g1-heap-resize` *(alias: `CRATONVM_G1_HEAP_RESIZE=1`)* | **Opt-in.** The adaptive heap-sizing policy: a *soft capacity* between `-Xms` and the `-Xmx` region grid, grown when GC overhead exceeds 5 % of wall clock or occupancy reaches 70 % of it, and shrunk after four consecutive pauses below 40 % occupancy and 1 % overhead — to a point that leaves occupancy at 60 %, i.e. strictly inside the dead band, so a shrink cannot land where the next pause grows it back. The capacity is the denominator of the free-fraction trigger AND the floor for the trailing-Free-run shrink, which this flag also moves from `cleanup` to the end of every evacuation pause (gated on phase Idle, SATB inactive and an empty gray set, the same gate `eager_reclaim_early_decline` computes). Without it, `-Xmx` is a one-way ratchet on RSS for any workload that never crosses IHOP, because `cleanup` is the shrink's only caller. Watch `heap_grows` and `heap_shrinks` on the `[GC-STAT]` line: two large, nearly equal counts are the hysteresis being too weak. |
| `CRATONVM_GC=g1-pause-cost-model` *(alias: `CRATONVM_G1_PAUSE_COST_MODEL=1`)* | **Opt-in.** Make the mixed-CSet copy budget price the pause it is actually buying. Two terms: the YOUNG half of the collection set is charged first (every Eden and Survivor region is in the CSet unconditionally, so `max_gc_pause_ms` was spending its whole goal on the *second* copy of the pause), and each old candidate is charged the MARGINAL fix-up cost it causes — `fixup_ns_ema / fixup_regions_ema` times the walk regions it adds (its to-space destination, plus its remembered-set sources that no already-selected candidate has paid for). Before this, `fixup_ns_ema` was a constant with respect to the very choice being made, so the model under-estimated in the direction that overruns. Both terms are subtractions from the budget, so arming it can only make a mixed collection set SMALLER — safe for the pause goal, and a throughput question for the mixed phase, which is why it is opt-in. The selector still takes one region unconditionally for forward progress. |
| `CRATONVM_GC=g1-humongous-run-guard` *(alias: `CRATONVM_G1_HUMONGOUS_RUN_GUARD=1`)* | **Opt-in.** The other half of humongous placement. Best-fit stops a humongous SPAN eating a long run; nothing stopped a single-region Old or humongous-start claim landing in that run's INTERIOR, which leaves two runs summing to `n - 1` where only the longer is usable — so span capacity falls by far more than the one region taken, permanently, because G1 has no compaction pass. With this on, `claim_free_region` prefers a Free region outside the longest run, and failing that takes the run's END, shortening it by one instead of splitting it. Placement only: both arms return a Free region and neither can fail where the other succeeds. Opt-in because it replaces a hinted scan that stops at its first hit with a full table pass on a path that runs under the exclusive guard; `[GC] g1 free-scan: single(...)` prices both arms, and `[GC] g1 humongous: requests= failures= free_at_failure= longest_run_at_failure=` is the outcome census — a failure with a comfortable free count and a short longest run IS the fragmentation this exists to delay. |
| `CRATONVM_GC=-g1-eager-humongous` *(alias: `CRATONVM_G1_EAGER_HUMONGOUS=0`)* | Restore cleanup-only humongous reclaim. By default an evacuation pause also frees humongous spans it can prove nothing references. This is the only path that frees memory outside the collection set, so it is the first thing to rule out if a live humongous object goes missing. |
| `CRATONVM_GC=g1-young-pause-target` *(alias: `CRATONVM_G1_YOUNG_PAUSE_TARGET=1`)* | **Opt-in.** Let `max_gc_pause_ms` bound the YOUNG generation too, not just the old half of a mixed collection set: G1 also collects once the Eden+Survivor region count reaches an adaptive target, tightened by 20% after any PRODUCTIVE pause that overruns the goal and relaxed while pauses stay under half of it. Does nothing until such an overrun is measured (the target starts at its 60%-of-regions ceiling and a target at the ceiling is not a trigger). Measured trade on `G1ChurnPauseProbe` at `-Xmx2048m`: p50 -21%, p99 +3%, wall +4.2%, one extra pause — see the young-sizing paragraph under Backend details for the full table and why it is not a default. |
| `CRATONVM_GC=g1-workers=<n>` *(alias: `CRATONVM_G1_WORKERS=<n>`)* | Force the evacuation worker count; `=1` drains the parallel path serially, which separates a concurrency race from a logic divergence. Overrides `-XX:ParallelGCThreads`, which in turn overrides the machine-derived default |
| `CRATONVM_DBG=gc-stress=<bytes>` *(alias: `CRATONVM_DBG_GC_STRESS=<bytes>`)* | Force young GCs every N allocated bytes (Generational) |
| `CRATONVM_GC=par-threads=<n>` *(alias: `CRATONVM_GC_PAR_THREADS=<n>`)* | Generational young-GC worker count. `0`/`1` forces the sequential collector; `>= 2` forces that many workers regardless of heap size. Unset = `min(available_parallelism, 8)` once the young gen passes the size floor. `available_parallelism` follows CPU affinity, so a `taskset -c N` run is automatically sequential |
| `CRATONVM_GC=par-min-bytes=<bytes>` *(alias: `CRATONVM_GC_PAR_MIN_BYTES=<bytes>`)* | Young-gen size floor below which the young GC stays sequential (default 16 MiB) |
| `CRATONVM_GC=sync-young-wipe` *(alias: `CRATONVM_GC_SYNC_YOUNG_WIPE=1`)* | Zero the evacuated young semi-space INSIDE the pause, as before 2026-09-02. By default the memset runs on a helper thread after the pause (the arena is the next cycle's to-space, which no mutator allocates into) and is joined before the next collection; `[gcpause]` reports `wipe_deferred_bytes=`. The first lever to pull if a conservative root is ever reported inside the inactive semi-space |
| `CRATONVM_GC=-par-evac` *(alias: `CRATONVM_GC_PAR_EVAC=0`)* | Force the single-threaded evacuator for the Generational **moving** (Cheney) young cycle. Parallel evacuation is the default, but it engages only where `CRATONVM_GC_PAR_THREADS` policy already asks for two or more workers, the cycle is a moving one, and to-space has room for the survivors plus one per-worker buffer each — so a run that never sees it is common and expected. Workers run on the same persistent `evac_pool` threads G1 uses (parked on a condvar, never respawned per pause), claim to-space in per-cycle buffers sized against the live set off one atomic cursor, promote through the old generation's allocator under a lock, and claim each object with a tagged CAS on its mark word (the same copy-then-CAS protocol G1 uses); a retired buffer's tail is stamped with a `TLAB_FILLER`/`GAP_FILLER` sentinel so the arena stays walkable as the next cycle's from-space. Both evacuators seed from the same three sources and produce the same forwarding map, so `=0` is a one-run bisection lever rather than a behaviour switch. |
| — | `--verbose:gc` prints `[GC] par_evac: cycles=… helper_scans=… cas_losses=… declined_for_slack=… filler_bytes=… promotions=… deferred_cards=…` at exit, unconditionally, including the all-zero line. Read `cycles` first: a zero says the path never engaged, which is a different claim from "it engaged and did nothing". `helper_scans` says whether it was actually *parallel* — a cycle where the driver did everything and the helpers scanned nothing is a load-balancing regression that every correctness test in the suite passes. `promotions` and `deferred_cards` are the two arms whose absence would not fail immediately: promotion is the only shared-lock contention point in the copy phase, and a deferred card is the old→young edge whose loss surfaces a cycle later somewhere else — a zero in either means whatever you ran never exercised it. `declined_for_slack` should stay at 0; a young GC triggers with from-space ~99.9% full, so the slack the Cheney invariant leaves is small (measured: 121 KB out of 128 MB on bt18) and the cycle runs bufferless rather than declining. `filler_bytes` prices the per-worker buffering, and is 0 on a bufferless cycle. |
| — | **Driving the copy phase in a soak.** A plain benchmark run yields one moving cycle or none. `CRATONVM_DBG=gc-stress=250000` turns that into ~500–1000 per process, which is what makes a soak mean anything: `CRATONVM_GC=par-threads=8,moving-young CRATONVM_DBG=gc-stress=250000 cratonvm --XX:UseGc Generational -Xmx256m --verbose:gc -cp … BinT 14`. (The legacy `CRATONVM_GC_PAR_THREADS=8 CRATONVM_MOVING_YOUNG=1 CRATONVM_DBG_GC_STRESS=250000` spelling still works and warns once; note that two tokens of the SAME group go in one comma-separated value — a second `CRATONVM_GC=` assignment would replace the first.) Use oracles that are not a second run of this VM — `bench/BinT.java` sums to `10 * (2^(d+1) − 1)`, and `bench/HashMapOnly.java` / `bench/StringRegexOnly.java` document their checksums in their own headers. |
| `CRATONVM_GC=sweep-anchor-stride=<bytes>` *(alias: `CRATONVM_GC_SWEEP_ANCHOR_STRIDE=<bytes>`)* | Byte spacing of the parallel-sweep anchors (default 8 MiB). Also sizes the MOVING path's parallel object-start-walk chunks. Lower it to drive either on a small young gen under `CRATONVM_DBG_GC_STRESS` |
| `CRATONVM_GC=verify-rset` *(alias: `CRATONVM_GC_VERIFY_RSET=1`)* | After each young collection's old->young seeding, walk the whole old generation and report `[rset-verify] site=.. edges=N missing=M seeded=S`. `missing > 0` names an edge the card table did not deliver, and the first one's referrer/class/slot. **Read `edges` too**: `missing=0` on a run that found no edges at all is vacuous, not clean. Costs a full old-gen walk per young GC |
| `CRATONVM_GC=full-rset-scan` *(alias: `CRATONVM_GC_FULL_RSET_SCAN=1`)* | Restore the pre-2026-09-02 whole-old-generation old->young walk on every young collection. The revert lever for the default flip below; the first thing to try if a premature-reclamation defect is suspected under Generational |
| `CRATONVM_GC=young-trigger-percent=<n>` *(alias: `CRATONVM_GC_YOUNG_TRIGGER_PERCENT=<n>`)* | Moving young collection trigger, as a percent of from-space capacity (default 50, clamped 1..=95). Raising it collects less often and copies more survivors per cycle; see "Young sizing" below. The NON-moving sweep has its own, higher trigger — `NON_MOVING_YOUNG_GC_THRESHOLD_PERCENT`, 90, not settable — because it reclaims in place and needs no Cheney headroom |
| `CRATONVM_GC=-peer-pin-divert` *(alias: `CRATONVM_GC_NO_PEER_PIN_DIVERT=1`)* | Let a moving young cycle proceed even when a conservatively-discovered JIT root was published this cycle. Restores the pre-2026-09-06 behaviour. **This is the single largest lever on whether a JIT-warm Generational run compacts at all** — the term it disables (`unrewritable_conservative_jit_roots`, see the `divert_non_moving` table below) fires on essentially every cycle once compiled code is live. It is off by default because a conservative pin landing in young-from names an object whose holder word nothing can rewrite; turn it on only to A/B a suspected over-diversion, and read `[GC] moving_young:` on both arms |
| `CRATONVM_DBG=gcpause` (alias `CRATONVM_DBG_GCPAUSE=1`) | Per-phase breakdown of the MOVING young cycle, printed for any collection over 100 ms. **The rows partition the pause since 2026-09-20**, as G1's `[GC-STAT]` line does: every figure is microseconds rendered `{:.3}ms`, and an `other=` residual is printed unconditionally, including zero, so the rows plus `other` equal the total by construction. Until that date there was no residual — everything before the first mark and after the last was charged to nothing — and every figure was `as_millis()`, so twenty sub-millisecond phases printed twenty zeros against a 12 ms total. **Any `[gcpause]` output quoted from before 2026-09-20 has both defects**, including the table under "Where a moving young pause actually goes" below. `docs/internal/gaps/gengc-plumbing-gcpause-phases-do-not-sum-20260920.md` |

Note: `tracing::debug!` is compiled out of release builds
(`release_max_level_info`); for cycle-phase confirmation attach gdb to
un-inlined gc-crate symbols (LTO is off for `cratonvm-cli` dev builds).

**Tuning knobs** (java-compatible): `-Xmx` (reserved address space on every
backend; the Generational old generation is the one part committed up front),
`-Xms` (committed at startup, **honoured on all three backends** since
2026-09-21 — see the heap-size note at the top of this
file), `-XX:G1HeapRegionSize=<bytes>` (rounded to a power of two and
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
allocation.

*When a young cycle relocates, stated against the code.* The decision is
`gen_heap::collect_garbage_inner`'s `divert_non_moving`, and it has **six**
terms, any one of which selects the non-moving sweep:

| term | fires when |
|---|---|
| `!moving_young` | moving-young is not in effect for this cycle — either the operator opted out (`CRATONVM_GC=-moving-young`, alias `CRATONVM_NO_MOVING_YOUNG=1`) or this cycle's coverage proof failed. Subsumes the legacy rule (a live JIT frame, or an unregistered compiled frame on the stack, while moving-young is OFF) |
| `unrewritable_conservative_jit_roots` | **moving-young is ON**, a live JIT frame, *and* a conservative JIT scan ran this cycle (`gc_quiescence::conservative_jit_scans() > 0`). Added 2026-09-06; `CRATONVM_GC=-peer-pin-divert` (alias `CRATONVM_GC_NO_PEER_PIN_DIVERT=1`) is the opt-out |
| `honor_promotion_oom_risk` | both generations ≥ 90 % full *and* conservative roots exist (or `CRATONVM_GC=promotion-oom-guard-broad`, alias `CRATONVM_PROMOTION_OOM_GUARD_BROAD=1`) |
| `divert_for_incomplete_moving_coverage` | this cycle's per-frame rewritable-root proof failed |
| `explicit_full_gc` | `System.gc()` asked for an old-gen-inclusive cycle |
| `gpu_relocation_forbidden` | a GPU device held the arena in place past the collector's bounded wait |

Only `CRATONVM_DBG=force-moving` (alias `CRATONVM_DBG_FORCE_MOVING=1`) can carry a
cycle past the first three.

> **The first term lost a conjunct on 2026-09-21, and that is the repair for a
> measured defect.** It read `has_conservative_roots && !moving_young`, so on a
> process with **no live compiled frame** the opt-out selected nothing: no term
> of the OR fired and the cycle ran the Cheney copy, having already recorded
> `moving_young_requested=false` on its own decision line. Measured on `BinT 10`
> at `-Xmx256m` under `CRATONVM_DBG_GC_STRESS=250000`: 1512 of 1512 cycles
> `kind=moving reason=moving-no-jit-frames-live`. That is the configuration this
> page and `types/src/flags.rs:560` both call the supported compatibility
> opt-out, and it is the only relocation bisect this backend offers — so an
> operator on a short repro that never tiered up set it, saw identical
> behaviour, and concluded relocation was not the cause. Filed as
> `docs/internal/gaps/gengc-probe-no-moving-young-optout-does-not-opt-out-20260921.md`.
>
> **Reason code caveat.** A cycle diverted by the opt-out *alone* is recorded as
> `nonmoving-conservative-jit-roots`, because that is the nearest code that
> exists and an out-of-range one would vanish from the histogram entirely. On
> such a cycle there are no conservative JIT roots: the decision line beside it
> reads `jit_active=false unregistered_jit_frame=false`, and the `--verbose:gc`
> line qualifies it as `divert=nonmoving-conservative-jit-roots/no-moving-young-optout`.
> The honest repair is a code of its own
> (`NON_MOVING_YOUNG_COMPACTION_DISABLED`) in `gc/src/gc_metrics.rs`; it is
> tracked with the other requested code split in
> `docs/internal/gaps/gengc-plumbing-conservative-root-divert-reason-20260920.md`.

**This page said for a month that a live JIT frame "does not by itself force
the non-moving sweep", and that the older `any JIT frame ⇒ non-moving` rule was
"reachable today only under `CRATONVM_GC=-moving-young` or
`CRATONVM_GC=-moving-young-jit-frames`" (aliases `CRATONVM_NO_MOVING_YOUNG=1` /
`CRATONVM_MOVING_YOUNG_NO_JIT=1`). Both claims are false on the shipped
default.** The second term above restores that rule almost exactly: every
mutator publishes a conservative JIT-root scan on its way into the pause, so on
a JIT-warm workload `conservative_jit_scans() > 0` holds on essentially every
cycle, and a live compiled frame anywhere in the process diverts the collection
to the non-moving sweep with moving-young switched on. What changed in 2026-09-06
is the *justification* (a conservative pin in young-from cannot be rewritten, so
the cycle must not relocate), not the outcome.

A JIT-warm workload therefore spends most or all of its cycles non-moving. That
is still a *measured* rate rather than a claim to be taken on trust, and the
running process states its own answer through
`gc_metrics::collector_decision_report()`. Note that the diversion above is
recorded as `nonmoving-conservative-jit-roots`, the **same** reason code as the
legacy rule. Two ways to tell them apart, and use the second for a
distribution:

* on the last-cycle line, `moving_young_requested=true` beside that reason **is**
  the second term, unambiguously;
* since 2026-09-20 the report also prints, under the histogram,

  ```
  [GC] conservative-jit-root diverts: moving_young_off=N (legacy rule …) moving_young_on=M (unrewritable conservative roots, 2026-09-06 …)
  ```

  which partitions that histogram row exactly. `moving_young_off` should be `0`
  unless you set `CRATONVM_GC=-moving-young`, and since 2026-09-21 it counts
  **two** things on such a run: the legacy rule (a live compiled frame with the
  gate off) and the opt-out on its own (no compiled frame at all). The advice
  the line prints — "the lever is turning moving-young ON" — is right for both;
  `moving_young_on` is the 2026-09-06 term, and it is the one whose only lever
  is `CRATONVM_GC=-peer-pin-divert`. Pointing an operator at the wrong one of
  those was the defect
  (`docs/internal/gaps/gengc-plumbing-conservative-root-divert-reason-20260920.md`).
  The split is exact rather than indicative because the coverage-proof arm is
  tested first, so by the time this reason is chosen
  `moving_young == moving_young_requested`.

That default young
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
Parallel EVACUATION exists here since 2026-09-02 (`gc/src/gen_evac.rs`):
the transitive-closure copy runs on the same worker count as the mark, each
worker bump-allocating into its own to-space chunk and old-gen promotion
buffer, claiming each source object by a compare-and-swap of its mark word;
`CRATONVM_GC_PAR_EVAC=0` restores the sequential drain. The evacuated
semi-space is zeroed off-pause on a helper thread
(`CRATONVM_GC_SYNC_YOUNG_WIPE=1` restores the in-pause memset). The moving young gen's
JIT-held-oop corruption is fixed, and it is now the **default**
(`types/src/flags.rs::DEFAULT_MOVING_YOUNG`), with
`CRATONVM_GC=-moving-young` (alias `CRATONVM_NO_MOVING_YOUNG=1`) as the
compatibility opt-out.

**What the opt-out actually does, since 2026-09-21.** Every generational cycle
takes the non-moving young sweep, whatever is or is not compiled — `kind=non-moving`
on every `--verbose:gc` line and `moving=0` in the decision histogram. Because
`major_gc` (the old-gen **mark-compact**) has exactly one caller and it is on the
moving young path, the opt-out also stops the old generation compacting: old-gen
reclamation drops to the in-place `sweep_old_gen_non_moving`. So the flag is a
whole-heap "nothing relocates" lever, which is what makes it a usable bisect for
a suspected relocation defect — and also why it costs fragmentation headroom and
is not a throughput setting. Before that date it was none of those things on a
process with no live compiled frame; see the note under the `divert_non_moving`
table.

*Two diagnostics to know about on that run.* `[GC] moving_young:` is printed
only when `gc_quiescence::moving_young_enabled()` is true or a coverage fallback
was recorded (`VmHeap::print_gc_summary`, `gc/src/vm_heap.rs:4202`), so under the opt-out the line is
**absent**. It is now absent for an honest reason — there is nothing it would
report but zeros — but it does mean the shutdown census cannot confirm the
opt-out held. Read `[GC] decision histogram: moving=0 non_moving=N skipped=K`
instead, which is printed unconditionally, or the per-collection `kind=` field.

The flag being
on is not the same as a cycle having compacted: every cycle must carry
its own root-coverage proof, and one that cannot prove complete coverage
diverts to the non-moving sweep rather than relocating — so read
`[GC] moving_young: moving_cycles_total=… non_moving_cycles_total=…
refused_cycles_total=… moving_cycles_under_live_jit=… coverage_fallbacks=…`, and the
`[GC] decision #N:` line rendered by
`gc_metrics::collector_decision_report()`, before attributing any
cost or behaviour to compaction.

**Read the first two fields; the third is expected to be zero and cannot tell
you what its name suggests.** This is the single account of that line — two
independent round-1 lanes reached it from different directions (a rename in
`vm_heap.rs`, and a reachability argument about the call site) and this
paragraph replaces both.

| field | source | what it can tell you | what it CANNOT |
|---|---|---|---|
| `moving_cycles_total` | `gc_metrics::decision_histogram()`, summed over the `moving-*` reason codes | how many generational collections relocated | — |
| `non_moving_cycles_total` | the same histogram, `nonmoving-*` codes | how many took the in-place sweep instead. The per-reason rows, and the `[GC] conservative-jit-root diverts:` breakdown under them, say **why** | — |
| `refused_cycles_total` *(2026-09-21)* | the same histogram, `skipped-*` codes | how many young cycles ran **neither** collector: to-space could not cover from-space, the from-space object-start walk did not complete, or a live mutator still owned a reserved TLAB tail inside the arena (T-3). Each has its own reason code, so the per-reason rows say which. A refused cycle over-retains and is retried on the next trigger | whether the heap recovered — read the following cycles' `kind=` for that |
| `moving_cycles_under_live_jit` | `gc_quiescence::moving_young_cycle_count()` | in principle, moving cycles taken while a compiled frame was live | **anything, in practice.** See below |
| `coverage_fallbacks` | `moving_young_coverage_fallback_count()` | cycles whose per-cycle root-coverage proof failed | — |

`moving_cycles_under_live_jit` is **structurally zero once compiled code is
live**, and a zero there is not evidence of anything. Its only bump site sits
behind `if moving_young && has_conservative_roots` — but twenty lines earlier
`unrewritable_conservative_jit_roots` requires the same two terms plus
`conservative_jit_scans() > 0`, and diverts the cycle to the non-moving sweep
before it can arrive; that count "is positive whenever anything is compiled"
(the term's own comment). So the conjunction is false in any JIT-warm process
and true only where nobody publishes a conservative scan — a unit test driving
the collector directly, or `CRATONVM_GC=precise-only-roots`. Measured
2026-09-20 on `BinT 14` under `CRATONVM_DBG=gc-stress=250000`:
`moving_cycles_total=1510 non_moving_cycles_total=2
moving_cycles_under_live_jit=0 coverage_fallbacks=0`
(`docs/internal/reviews/gengc-round1-probe-results-20260920.md`) — 1510 cycles
that demonstrably copied, and the JIT-active count reads zero because that soak
never exercised the JIT-frame path at all. Do not read it as "the collector
never moved under the JIT"; read it as "this run says nothing about that case".
Whether the counter is widened or deleted is open
(`docs/internal/gaps/gengc-core-moving-young-cycle-counter-inert-20260920.md`);
if it is deleted, this row and `vm_heap.rs`'s field go with it.

**The key changed on 2026-09-20.** Until then the line's only cycle count was
keyed `cycles=` and carried the `moving_cycles_under_live_jit` number, so a run
with no compiled frames could relocate on every collection and still print
`moving_young: cycles=0 coverage_fallbacks=0` — which reads as "the young
generation never moved", the precise inverse of the truth, and is the reading
several earlier investigations started from (see
`docs/known-issues/jdk-only/G27-1-…`, whose separate point is that a DEFAULT run
is ZGC and never reaches this collector at all). **Any quotation of
`moving_young: cycles=N` from a log older than 2026-09-20 is a JIT-active count,
not a moving-cycle count**, and `ARCHITECTURE.md:343` and
`docs/feature-designs/default-moving-young-gen.md:30` still instruct readers to
read the old field.
Old gen is a free-list
allocator collected by a VM-driven concurrent cycle (initial mark STW →
concurrent trace → remark STW → concurrent sweep, with a remark-time
TAMS snapshot gating the sweep).

*Where a moving young pause actually goes, and the 2026-09-02 changes.*

> **These numbers were taken with a broken instrument and have not been
> re-measured.** At the time, the `[gcpause]` rows had no residual/`other` term
> and every figure was quantised to whole milliseconds, so they did not
> partition the pause the way the "before"/"after" columns assume. Concretely,
> the "after" column reads 22–39 + 0 + 32–63 + 28–52 + 0 = **82–154 ms** against
> a stated total of **100–133 ms**; the rows do not sum, the gap is not stated,
> and nothing in the output says whether the difference is unmeasured phases or
> quantisation. The instrument was repaired on 2026-09-20 (microsecond marks, an
> `epilogue` mark, an unconditional `other=` residual, and a `debug_assert`
> against double-counting — see
> `docs/internal/gaps/gengc-plumbing-gcpause-phases-do-not-sum-20260920.md`);
> **this table has not been re-run on it.** The direction of the 2026-09-02
> changes is not in doubt (the `full_old_rset_scan` row went to a hard zero),
> but the magnitudes are not citable to better than "large" until someone
> repeats the run.

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

*What the pause DECIDED, beside what it cost (2026-09-20).* The phase
breakdown says where a pause went and the region census says what it bought;
neither says what the collector chose. Every `[GC-STAT]` line now also carries
the state of the four adaptive controllers, read after they have been updated
for that pause — so the fields say what the NEXT pause will be sized by:

| field | controller | read it when |
|---|---|---|
| `goal_us` | `max_gc_pause_ms`, in the unit everything else on the line uses | always — it is the denominator of every other decision |
| `young_target` / `young_bounds` / `young_now` | `update_young_target` | pause frequency looks wrong. `young_target` at the top of `young_bounds` is the ceiling, which is also the state in which the young half of `needs_gc` is deliberately inert |
| `free_pool` / `free_trigger_pct` | `needs_gc`'s free-fraction arm | a pause storm: a pause that frees bytes without returning REGIONS leaves this trigger latched |
| `tenuring` / `survivor_target` | `update_tenuring_threshold` | survivors are being copied too many times, or promoted too early |
| `evac_ns_per_byte` / `fixup_ns_ema` / `old_budget_ns` | `update_evac_cost`, `old_cset_copy_budget_ns` | a mixed collection set looks too small. `old_budget_ns` is the goal MINUS the fix-up walk's rolling cost, so a fix-up that has grown to the whole goal leaves the old half nothing but its one guaranteed region |
| `mixed_remaining` / `old_gen_bytes` / `mark_threshold` | the IHOP and mixed-phase state | old-generation growth is not being reclaimed |

At exit, `[GC] g1 young-sizing:` reports the same young controller's end state
beside the `[GC] g1 ihop:` and `[GC] g1 tenuring:` lines that were already
there — the young target was the one adaptive policy nothing printed.

The tenuring histogram on that line used to be structurally zero whenever
`CRATONVM_G1_ADAPTIVE_TENURING=0`: the evacuators fill it unconditionally and
the only thing that snapshots and clears it ran behind the flag, so the off arm
both reported nothing and accumulated forever. The snapshot now runs on every
pause; the flag gates only whether the derived threshold is published.

*The card screen's skip-rate, as a trend (2026-09-20, wave 2).* Phase 2's card
screen has counted the bytes it scanned and the bytes it stepped over since
F-05, and those totals were rendered in exactly one place — `[GC] g1
card-clean:` at shutdown, which is reached only from `vm-cli`'s normal-return
teardown. A workload that ends in `System.exit` never unwinds to it, so on the
JUnit suites the rate was never printed at all, and a `debug!` would not have
helped either: `release_max_level_info` compiles it out and no `RUST_LOG` value
recovers it from a shipped binary.

That made one specific experiment unrunnable, and it is the one that decides
`CRATONVM_G1_CARD_CLEAN`'s default. G1's card table is **additive-only**: it
saturates by construction, so the screen's skip-rate decays over a long run and
cleaning is the only thing that pushes back. Whether cleaning earns its cost is
therefore a question about the SHAPE of that decay, which a single cumulative
total averages away. Every `[GC-STAT]` line now carries:

| field | meaning |
|---|---|
| `card_scanned_total` / `card_skipped_total` | run-cumulative bytes the screen visited and stepped over |
| `card_skip_rate_pct` | the cumulative rate, derived from those two — not accumulated separately, because two accumulators drift |
| `card_skip_rate_first_pct` | the rate over the run's **first 64 instrumented pauses**, frozen. The trend's left endpoint |
| `card_skip_rate_now_pct` | the same rate smoothed 7/8 over every pause. The trend's right endpoint |
| `card_clean` | whether `CRATONVM_G1_CARD_CLEAN` is armed, so a comparison between two runs cannot be mislabelled |

`card_skip_rate_now_pct` well below `card_skip_rate_first_pct` IS the
saturation, happening. `[GC] g1 card-screen trend:` repeats the three at exit
with a `decayed=` verdict. A pause that offered the screen no source regions at
all (`rset_regions_offered=0`) is excluded from the rolling rate rather than
folded in as a 0 % sample — "Phase 2 had nothing to do" is a different fact from
"the screen ran and refused every region", and averaging them together is
exactly how a decaying rate hides behind a growing pile of empty pauses.

*What the pause decided about the HEAP (2026-09-20, wave 2).* Under
`CRATONVM_G1_HEAP_RESIZE=1` the same line also carries
`heap_target_regions` / `heap_target_bytes` / `heap_floor_regions`,
`gc_overhead_ppm`, `shrink_streak`, `heap_grows` / `heap_shrinks`,
`uncommitted_bytes` and `committed_bytes`, and every actual resize is logged at
INFO as `[GC] g1 heap-resize: grow|shrink …` outside the `--verbose:gc` gate —
a heap that changed size is a fact about the process, not a GC statistic, and
an operator watching RSS move has to be able to attribute it without having
asked for per-pause logging in advance.

`heap_grows` and `heap_shrinks` are meant to be read **together**. Two large,
nearly equal counts are the hysteresis being too weak, i.e. the policy paying
the decommit and the page faults forever and converging on nothing — the one
failure mode a single pause's line cannot show.

`[GC-STAT]`'s old `CRATONVM_G1_DBG_REACH` region census is gone. Every
collection path reached the emitter holding the regions guard as a writer, so
its `try_read` always returned `None` and the six counts it was armed to print
were never once printed; the flag arm rendered a strict subset of the default
line. The same six numbers are in `phase_note` (`free_regions`, `eden_regions`,
…), taken inside the pause where the table is already borrowed. An instrument
armed where it cannot fire is worse than none, because its zero reads as a
measurement.

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
| `CRATONVM_GC=-g1-parallel-evac-in-jit` (the old fallback) | 305 ms | 7606 ms |
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

Scope: all four evacuation drivers — serial young, mixed, and both parallel
arms — each pass a narrowed set to `update_references_in_regions`. (This
paragraph used to say "the SERIAL young pause; mixed pauses and the parallel
evacuator still walk wide". That was true when the narrowing landed and has
not been since the other three drivers adopted it; a reader sizing a mixed
pause from it would have budgeted for a whole-heap fix-up that no longer
happens.) What genuinely still walks wide is the evacuation-**failure**
drain, `drain_kept_self_forwards`, which passes `None` — and the humongous
census, for the reason given above.

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
a post-sweep re-arm so a large live set cannot storm. That clause makes pause
work scale with the heap FLAG rather than with the garbage: at `-Xmx2g` with
50 MiB live it fires when `allocated` reaches 1.5 GiB, so every cycle lets
~1.45 GiB accumulate and the registry, the mark bitmap and the sweep all cover
the span the bump cursor ran over — doubling `-Xmx` doubles every pause on a
workload whose live set did not change.

`CRATONVM_ZGC_ALLOC_TRIGGER=<percent>` (added 2026-09-03; **default 0, i.e.
off, with or without a pause target**. This line read "0 without a pause target,
25 with one" until 2026-09-20, describing the implicit floor that was made the
default on 2026-09-03 and **withdrawn on 2026-09-04** — see the pairing below
for the `TestLargeBlob` crash that withdrew it. `alloc_trigger_percent_for`
ignores the pause target and returns the operator's percent or zero.) adds a
second clause that
collects once that percent of capacity has been allocated since the last
cycle, capping the span a pause walks at `budget + live`. It is a pause-versus-throughput DIAL, measured on
`G1ChurnPauseProbe 50 600` at `-Xmx2048m` (release, three runs a row, one
binary, only this switch moved):

| percent | wall ms | cycles/run | mean pause | max pause | registered at the worst pause |
|---|---|---|---|---|---|
| 0 (off) | 2891 | 2 | 116 ms | **202 ms** | 10,597,520 |
| 50 | 3234 (+12 %) | 3 | 113 ms | 174 ms | 7,485,260 |
| 25 | 3463 (+20 %) | 6 | 80 ms | 122 ms | 3,953,214 |
| 12 | 3767 (+30 %) | 12 | 57 ms | **79 ms** | 2,116,551 |

The worst pause falls 2.6× for a 30 % wall cost, and the cost is not an
artefact — collecting six times as often pays the live-set-proportional half
of a cycle (the mark, the registry snapshot) six times as often. It is off by
default for that reason: a percentage of capacity says nothing about how long
the resulting pause will be, so it is a dial the operator has to tune per
workload. The pause-target form below is what replaced it. `[GC] zgc-pause:` prints
`alloc_trigger=<fires>/<budget bytes>` as its engagement counter. The
regression suite is 88/88 both with the clause off (the shipped default) and
with `CRATONVM_ZGC_ALLOC_TRIGGER=12`, so the switch is safe to turn on — what
it has NOT had is suite time on the larger corpora, which is what a default
change would need.

`-XX:MaxGCPauseMillis=<n>` (or `CRATONVM_ZGC_PAUSE_TARGET_MS`, **default 200**,
added 2026-09-03) is the form that ships on. The flag reached only G1 before
that date, so on the DEFAULT collector an operator who asked for a pause target
got no answer and no diagnostic. It is a CEILING, not a setpoint: the clause
starts unconstrained and engages only once a pause has actually overrun the
target, then scales the span it will allow by `target / pause` — multiplicative,
so it never has to model the pause cost curve, whose fixed part is large
(~34 ms here). It tightens on an overrun, holds inside `[0.75, 1.0] × target`,
and relaxes a quarter at a time but never back past ⅞ of the span that last
overran. On a workload whose pauses never reach the target it never engages at
all, which is what makes a non-zero default defensible where the percentage form
had to ship off. `refresh_pause_target_budget` records the control law, the
three earlier and wrong versions of it, and what each one measured.

Measured on `G1ChurnPauseProbe 50 1800` at `-Xmx2048m`, whose unconstrained
worst pause is 244 ms (release, three interleaved reps, one binary, only the
target moved). "p50 after engagement" excludes the overruns the controller had
to *observe* in order to react — no feedback loop can prevent those:

| target | wall ms | cycles/run | p50 after engagement | worst |
|---|---|---|---|---|
| off | 8773 | 6 | — | 244 ms |
| **200 ms (default)** | 9234 (+5.3 %) | 6.3 | 193 ms | 210 ms |
| 100 ms | 11352 (+29 %) | 24 | **68 ms** | 196 ms |
| 80 ms | 10210 (+16 %) | 26 | **57 ms** | 143 ms |
| `ALLOC_TRIGGER=12` | 10525 (+20 %) | 36 | — | 93 ms |

**Why the unit had to change.** Doubling `-Xmx` on a workload whose live set did
not move nearly doubles the worst pause — and the *percentage* form doubles with
it, because 12 % of a bigger heap is a bigger budget. Only a target holds:

| `-Xmx` | off | `ALLOC_TRIGGER=12` | target 100 ms |
|---|---|---|---|
| 2048m | 313 ms worst | 85 ms worst, 246 MiB budget | p50 73.5 ms |
| 4096m | 595 ms worst | **249 ms** worst, 492 MiB budget | p50 79.7 ms |

**What it does NOT control, and why.** It holds the MEDIAN; the tail stays high.
The pause floor on this collector is the arena's HIGH-WATER MARK, not the live
set: the bitmap sweep covers `[base, low_cursor)` and the cursor does not
retract, so once any cycle has run the bump cursor out to 1.3 GiB, every later
pause pays a scan over that span whatever the allocation budget is. No
allocation trigger can undo that — only compaction and a cursor retraction can
(`CRATONVM_ZGC_RELOCATE`, `Arena::retract_cursor_to`). That is why a 100 ms
target is reachable at `-Xmx2048m` on this probe and not at `-Xmx4096m`, and
why the loop gives up after three unreachable verdicts rather than paying one
unconstrained cycle per retry (measured: 56 cycles and +101 % wall for a p50 of
103.8 ms — worse on both axes than never trying).

`[GC] zgc-pause:` prints `alloc_trigger=<fires>/<budget bytes>` and
`pause_target=<ms>/<affordable span>/unreachable=<n>` as the engagement
counters. A climbing `unreachable` says the target is not achievable at this
live set, which is a different answer from "the loop is holding the target" and
produces an identical budget without it.
 The sweep prunes
dead bases in place and feeds the exact dead list to the monitor
registry. Non-moving ⇒ the pointer map is always empty and
no barriers are needed; reference semantics come entirely from the VM-level
protocol.

**The per-cycle bitmap passes are bounded by the arena's bumped ends**
(`CRATONVM_ZGC_BITMAP_BOUNDS`, default on, added 2026-09-03). The object-start
registry and the mark bits are sized by CAPACITY — one bit per 8 arena bytes,
so `-Xmx / 512` bytes of words — and the snapshot the mark phase takes was
copying all of it every collection: 8.4 million atomic loads into a fresh
64 MiB `Vec` at `-Xmx4g`, paid whether the heap holds ten objects or ten
million. That is a pause floor proportional to the heap FLAG, and it is what
made a 100 ms pause target reachable at 2 GiB and unreachable at 4 GiB.

`Arena` is two-ended, so the span between the low bump cursor and the
large-object end has never been handed out: no allocation starts there, no bit
in it is set, and copying it transfers zeroes. The snapshot and the mark-bit
clear now visit only the two ends. Measured on `G1ChurnPauseProbe 50 1800` at
`-Xmx4096m` with a 100 ms pause target, one binary and this switch the only
variable, at matched cycle counts:

| | snapshot | mark | sweep | pause p50 |
|---|---|---|---|---|
| whole capacity | 18.8 ms | 13.8 ms | 48.5 ms | 82.5 ms |
| bounded | 13.7 ms | 8.2 ms | 43.2 ms | **66.4 ms** |

**This is also what pays for the cursor retraction.**
`Arena::retract_cursor_to` has lowered the cursor onto the last survivor after
every sweep since 2026-09-02, but with capacity-sized bitmap passes that only
helped the *allocator* find contiguous space — the pause work was the same
either way. Bounded, retraction shrinks the pause: the tail of garbage a burst
allocated is handed back and the next cycle's snapshot, clear and complement
sweep all stop at the new cursor.

The bound is the HIGH-WATER mark and not the cursor, which is a correctness
requirement rather than a refinement: retraction lowers the cursor *after* the
sweep, and the mark bitmap has bits above the new cursor set earlier in the
same cycle. Clearing bounded by the cursor would leave them, and the next cycle
would read a mark set carrying a previous cycle's bits and retain whatever they
name. `Arena::low_high_water` is `cursor.max(pre_retract_high)`, reset once per
collection after both bitmaps are clear.
`ZObjectStartBits::debug_assert_clear_outside` verifies the invariant in debug
builds — it walks the skipped words and asserts every one is zero — and it is
what turned that ordering bug into a failing test on the first run.

**Pair a pause target with a percentage floor.** The target cannot bound the
FIRST cycle: it has nothing to measure until a pause has happened, so the
occupancy clause lets that one run to 75 % of `-Xmx`, and on this probe it is
the worst pause of the run by a factor of five. Setting
`CRATONVM_ZGC_ALLOC_TRIGGER` as well caps it, and the target then controls the
steady state. Measured at `-Xmx4096m`:

| configuration | wall | worst pause |
|---|---|---|
| neither | 9091 ms | 466 ms |
| target 100 ms alone | 12943 ms | 509 ms |
| **target 100 ms + `ALLOC_TRIGGER=25`** | 11180 ms | **190 ms** |
| target 200 ms alone | 14849 ms | 530 ms |
| **target 200 ms + `ALLOC_TRIGGER=25`** | 13548 ms | **221 ms** |

The pairing is better than the target alone on BOTH axes: the floor stops the
one cycle the controller is blind to, and paying for that cycle up front costs
less than the controller's recovery from it. **It was made the default on 2026-09-03 and WITHDRAWN on 2026-09-04**, the day after: arming the floor implicitly changed how often the collector runs on every ZGC workload, and `org.h2.test.db.TestLargeBlob` went from 0 collections and a PASS to 34 collections and a SIGSEGV inside `FileChannelImpl.implWrite` -> `IOUtil.write` -> `DirectByteBuffer` (3 crashes in 4 runs with the floor on, 0 in 3 with it off, same binary). The crash is almost certainly older than the flag — without the floor that test never collects at all, so nothing exercised the path — but a default that turns a passing test into a native crash does not ship while that bug is open. Pair them explicitly with `CRATONVM_ZGC_ALLOC_TRIGGER=<percent>` if you want it. `CRATONVM_ZGC_ALLOC_TRIGGER=0` is an explicit refusal
rather than an absence, and is how the "target alone" arm is measured; any
other explicit value wins over the floor in both directions.

The shipped default measured against what it replaced, and against no trigger
at all — same probe, one binary, switches only:

| `-Xmx` | configuration | wall | pause p50 | worst |
|---|---|---|---|---|
| 2048m | no trigger | 11323 ms | 227.8 ms | 276.8 ms |
| 2048m | target 200 alone (the old default) | 11184 ms | 199.4 ms | 242.1 ms |
| 2048m | **target 200 + floor 25 (shipped 2026-09-03, WITHDRAWN 2026-09-04)** | 11234 ms | **92.7 ms** | **116.0 ms** |
| 4096m | no trigger | 11129 ms | 421.4 ms | 565.6 ms |
| 4096m | target 200 alone (the old default) | 12449 ms | 147.2 ms | 654.7 ms |
| 4096m | **target 200 + floor 25 (shipped 2026-09-03, WITHDRAWN 2026-09-04)** | 11485 ms | 182.6 ms | **227.1 ms** |

At 2048m it more than halves both the median and the worst pause for no wall
cost at all; at 4096m it cuts the worst pause by 60 % against no trigger and by
65 % against the target alone, and costs 3 % of wall against no trigger while
being 8 % FASTER than the old default. The old default was the worse of the
three on the tail at both sizes — the controller was paying for a first cycle
it could not see and then recovering from it, which is precisely what the floor
removes. **None of that is the default any more**: the floor was withdrawn the
day after it shipped (see above), so reproducing these rows needs
`CRATONVM_ZGC_ALLOC_TRIGGER=25` named explicitly alongside the target.

### ZGC switches added in the 2026-09-20 round

Full table with rationale in
[`docs/gc-tuning.md`](gc-tuning.md#zgc-flags-added-or-renamed-in-the-2026-09-20-round).
In brief:

- **`CRATONVM_ZGC_VM_TLAB_SHARE`** (default **on**) — counts the VM-thread
  buffer against the TLAB reservation share. See the TLAB paragraph below for
  the regression it closes.
- **`CRATONVM_ZGC_HEADROOM_BYPASSES_REARM`** (default **off**) — lets the
  `headroom_low` clause of `needs_gc` fire below the `gc_rearm` floor, but only
  while the last cycle reclaimed something. `gc_rearm` is sized from **live
  bytes** and `headroom_low` is about **allocatable space**; on a heap whose
  cycles may decline to compact those come apart without bound, and the shared
  floor can silence the headroom clause exactly on the heap it was written for
  (large `-Xmx`, small live set, fragmented free list). The failure is not a
  slow run — the infallible `alloc_object` aborts rather than raising
  `OutOfMemoryError`, so "never collects" is a crash with most of the heap free.
- **`CRATONVM_ZGC_CENSUS`** / **`CRATONVM_ZGC_CENSUS_ACCESS`** (default
  **off**) — turn on `ZSlotCensus`, the sweep-time reference-slot walk. Before
  this, `ZSlotCensus::enable()` had no caller outside its own tests, so the
  instrument built to measure the legacy-slot share had never run in any
  process. Read `legacy_share` off the **`last_walk`** block, not the cumulative
  one, which is lifetime-weighted and reads high on legacy.
- **`CRATONVM_ZGC_PAUSE_TARGET_INCLUDES_RELOCATE`** (default **off**) — feed
  the pause-target controller the pause *including* the slide, which
  `collect_garbage` read before the compaction block. That controller sizes
  `alloc_trigger_bytes`, a clause of `needs_gc`, so it decides how often the
  collector runs; the pre-slide reading was about half the real pause on
  `ZgcTlabStress 8 200`.
- **`CRATONVM_ZGC_METRICS`** (default **off**) — also print `ZgcMetrics`'s
  OpenJDK-shaped per-cycle line and its run summary at heap teardown.
  *Recording* is unconditional now; only the rendering is opt-in, because the
  format differs from `[GC] zgc-real:` and the harnesses key on that.
- **`CRATONVM_ZGC_TRIGGER_SHADOW`** (default **off**) — one
  `[GC] zgc-trigger-shadow:` line per collection showing what an
  allocation-rate-driven trigger would have decided. Stage 1 of
  `proposal-e-allocation-rate-driven-trigger.md`, i.e. the measurement that
  decides whether the rest is worth building.

All seven are declared in `types/src/flag_groups.rs:2910`–`:2918`, so each also
has a `CRATONVM_GC=<token>` spelling (`docs/flag-tokens.md`) and a row in
`docs/config/flag-inventory.md`. *(This paragraph said the opposite — "not yet
in `types/src/flag_groups.rs`" — until 2026-09-21; the registration landed in
wave 2 and the rendering was corrected in wave 3, `9a03106ff`.)* Three further
`CRATONVM_ZGC_*` keys the round added or made operator-relevant are documented
in `docs/gc-tuning.md` rather than here: `CRATONVM_ZGC_MARK_REF_CHUNK`
(numeric, default 4096, live on every configuration),
`CRATONVM_ZGC_TLAB_RESERVED_BYTES` (default off, counter readable anyway) and
`CRATONVM_ZGC_PAGE_EVAC` (default off, and switches nothing today).

Two renames, from when the sweep optimisations stopped being scoped to young
cycles: `CRATONVM_ZGC_GEN_HEADER_ZERO` → `CRATONVM_ZGC_SWEEP_HEADER_ZERO` and
`CRATONVM_ZGC_GEN_DEAD_RUNS` → `CRATONVM_ZGC_SWEEP_DEAD_RUNS`. The `GEN_` names
are read by nothing and set silently to nothing.

**Mutators have TLABs on this backend, and since 2026-09-02 the JIT's inline
allocator can have one too -- default-on since 2026-09-18 (JIT review round 9;
`CRATONVM_ZGC_JIT_TLAB=0` turns it off).** `VmHeap::refill_tlab` on the `Zgc` arm
hands the VM thread's own `Tlab` a zeroed chunk from the low arena
(`gc/src/zgc/vm_tlab.rs`) unless `CRATONVM_ZGC_JIT_TLAB=0`, so the interpreter's
`new` and the compiled inline bump both hit; each object is registered in the start bitmap
the moment its header is complete (`VmHeap::note_tlab_object`), the unused tail
goes back to the arena free list when the thread retires the buffer
(`Tlab::retire` → `TlabTailSink`), and a tail the STW protocol publishes for a
blocked or frozen peer pins its pages against the slide. Before that date the
arm returned `None`, every compiled allocation took the `jit_new_object` helper,
and the former VM-wide TLAB-hit metric was zero here by construction. Below the VM buffer,
`ZgcRealHeap::alloc_raw_tlab` (over `gc/src/zgc/arena_tlab.rs`) remains the
funnel for the helper path and every native-side allocation; it is **on by
default**, and `CRATONVM_ZGC_TLAB=0` is its kill switch (which also switches the
VM buffer off, since both carve from the same source). **The VM buffer was
opt-in, on a measurement that said it was slower; that reversed on 2026-09-18
and it is default-on** — see `zgc_vm_tlab_enabled_by_default` and
`ZGC_VM_TLAB_DEFAULT_ON` for the numbers (bintrees ~6x faster than the helper
path) and for the register-only helper the win needed. This paragraph carried
both statements at once until 2026-09-20.

A TLAB chunk is *reserved* space that
no collection can reclaim while its owning thread lives, so it is invisible to
any trigger that counts live bytes; the reservation budget is bounded by the
buffer count (`ZGC_TLAB_RESERVATION_SHARE`, a sixteenth of the arena divided by
the buffers claiming one and clamped up to `ZGC_TLAB_MAX_CHUNK`) for exactly
that reason. **That divisor counted only `zgc::arena_tlab`'s own cells until
2026-09-20**, and a thread allocating through the VM buffer registers no such
cell — so from the 2026-09-18 default flip until then, essentially all
allocation ran through a path the reservation budget could not see and every
thread was sized as if it were alone. `CRATONVM_ZGC_VM_TLAB_SHARE=0` restores
the old arithmetic; the failure it re-opened is Tomcat's `TestNonBlockingAPI` at
`-Xmx2g`, ~4 000 threads x 512 KiB against a 2 GB heap.

The arena is **two-ended**: small objects and TLAB chunks bump up from offset 0,
allocations at or above `ZGC_LARGE_OBJECT_MIN` (64 KiB — the size no TLAB will
ever serve) bump *down* from capacity with their own free list, and a reserve
(`capacity / 8`) keeps the low end from consuming the whole large-object end.

**The pointer map is no longer always empty, and this paragraph said it was
until 2026-09-20 — **five weeks** after the slide became the default.** *(This
sentence said "three months" until 2026-09-21, contradicting its own next
paragraph four lines down, which gives the correct date. Three pages carried
that same wrong interval; all three are fixed.)* It read:
*"The always-empty pointer map and the neutral `VmHeap::Zgc` arms that go with
it are correct only while the collector is non-moving … which must land before
any compaction does."* Compaction has been default-on since 2026-08-13, so a
reader taking that at face value would conclude a stale-reference crash could
not be the slide.

What is true is the narrower statement. A default cycle returns a **non-empty**
`PointerMap`, and every consumer of one therefore runs for this collector: JIT
frame maps, monitor tables, external root providers and native side tables.
Those consumers are collector-agnostic and already ran for the generational
moving-young path, and the two `VmHeap::Zgc` predicates that take a pre-GC
address were audited and tested (R6) — but "already runs for another collector"
is not "has run for this one". **A crash, a stale-reference warning or a
silently-wrong result that disappears under `CRATONVM_ZGC_RELOCATE=0` is that
class of bug, and the flag is the bisect.** The remaining hardening is Phase 4
of
[`docs/feature-designs/zgc-production-implementation-plan.md`](feature-designs/zgc-production-implementation-plan.md).
