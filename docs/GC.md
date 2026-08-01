# Garbage Collection in CratonVM — architecture and current state

*Last updated 2026-07-27. The historical four-wave G1/ZGC correctness
audit is retained in
[the internal fixed-issue archive](internal/fixed-suite-bugs/gc-audit-2026-07-10-open-findings.md).*

CratonVM ships three garbage-collector backends behind one dispatcher
(`gc/src/vm_heap.rs::VmHeap`). All are stop-the-world at the collection
level; G1 additionally runs its marking phase concurrently. Selection is
java-launcher-compatible:

| Flag | Backend | One-liner |
|---|---|---|
| *(default)* | `GenerationalHeap` (`gc/src/gen_heap.rs`) | Semi-space young gen + free-list old gen with a concurrent old-gen mark-sweep cycle. Young collections are **moving by default**; each cycle diverts to the non-moving sweep only when its own root-coverage proof fails (see "Backend details" below). |
| `-XX:+UseG1GC` | `G1Collector` (`gc/src/g1.rs`) | Region-based (1 MB regions, 2 MB above 4 GB heaps): young/mixed evacuation with remembered sets, SATB concurrent marking, humongous spans, region pinning. |
| `-XX:+UseZGC` / `-XX:+UseZ` | `ZgcRealHeap` (`gc/src/zgc.rs`) | **Not a real ZGC**: a memory-backed, non-moving, whole-heap stop-the-world mark-sweep over one arena, with a hash-set allocation registry. No colored pointers, no load barriers, no concurrency, no compaction. The colored-pointer/`ZPage` code above it in `zgc.rs` (and `zgc_concurrent.rs`) is a metadata-only simulation with no production consumer. |

Unrecognized `-XX:+Use*GC` selectors warn and fall back to Generational.
Heap size comes from `-Xmx`/`-Xms` as usual.

## The VM ↔ GC protocol

**Stop-the-world.** A GC-initiating thread posts a request on the
`GcBarrier` (`vm/src/threading/gc_barrier.rs`); mutators park at
interpreter safepoint polls (allocation sites and backward branches),
each depositing a **root snapshot** first. Threads inside blocking
natives are excluded from the arrival quota and covered by their
deposited snapshot plus a wake-time fixup (`check_post_block_gc`).
Threads stuck in compiled code that never polls are handled by the
**cross-thread JIT takeover** (INT-3, all backends since 2026-07-11):
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

A systematic audit (2026-07-10/11: four parallel deep code reviews plus a
deterministic differential probe kit diffed against HotSpot jdk25)
found and fixed, in four merged waves: G1 humongous accounting/reclaim
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

**Verified invariants** (probe kit, re-run on the current tip): all of
ChurnCheck / HumongousCheck / CopyChurn / MTChurn / RefCheck /
RefCheckOld / SpinPoll / SpinPollMark produce HotSpot-identical output on
Generational, G1 and ZGC at `-Xmx256m`; the gc crate's unit+integration
suites are green.

**Current limitations:**

1. `-XX:+UseZGC` selects the compatibility implementation described
   above, not HotSpot's concurrent colored-pointer ZGC.
2. `-XX:+UseStringDeduplication` is parsed but intentionally inert. The
   raw-address deduplication table must participate in every collector's
   remap and purge protocol before production callers can use it.
3. `CRATONVM_G1_PARALLEL_EVAC=1` is an experimental opt-in. Mixed
   collections stay on the serial evacuator, and the serial path remains
   the supported default.

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

**Diagnostics** (env-gated, in the release binary):

| Switch | What it does |
|---|---|
| `--verbose:gc` | Per-pause `[GC-STAT]` lines + exit `[GC-SUMMARY]` |
| `CRATONVM_G1_DBG_REACH=1` | Post-pause BFS-from-roots corruption detector; `[g1][FREED]`/`[WALKBRK]` traces |
| `CRATONVM_G1_DBG_PINS=1` | Per-pause conservative-JIT-pin census |
| `CRATONVM_GC_VERIFY_STALE=1` | Post-GC stale-frame-slot verifier (recycled drain destinations are recognized as benign) |
| `CRATONVM_DBG_WEAKREF=1` | Weak/Phantom null/restore pass tracing |
| `CRATONVM_G1_NO_EVAC_RETRY=1` | Disable the evacuation-failure drain (bisection) |
| `CRATONVM_G1_PARALLEL_EVAC=1` | Opt-in parallel young evacuator (known race — testing only) |
| `CRATONVM_DBG_GC_STRESS=<bytes>` | Force young GCs every N allocated bytes (Generational) |
| `CRATONVM_GC_PAR_THREADS=<n>` | Generational young-GC worker count. `0`/`1` forces the sequential collector; `>= 2` forces that many workers regardless of heap size. Unset = `min(available_parallelism, 8)` once the young gen passes the size floor. `available_parallelism` follows CPU affinity, so a `taskset -c N` run is automatically sequential |
| `CRATONVM_GC_PAR_MIN_BYTES=<bytes>` | Young-gen size floor below which the young GC stays sequential (default 16 MiB) |
| `CRATONVM_GC_SWEEP_ANCHOR_STRIDE=<bytes>` | Byte spacing of the parallel-sweep anchors (default 8 MiB). Lower it to drive the parallel sweep on a small young gen under `CRATONVM_DBG_GC_STRESS` |

Note: `tracing::debug!` is compiled out of release builds
(`release_max_level_info`); for cycle-phase confirmation attach gdb to
un-inlined gc-crate symbols (LTO is off for `cratonvm-cli` dev builds).

**Tuning knobs** (java-compatible): `-Xmx`/`-Xms`,
`-XX:G1HeapRegionSize=<bytes>`, `-XX:InitiatingHeapOccupancyPercent=<n>`
(adaptive around the static value, floored at max(1 % of heap, one
region)), `-XX:MaxGCPauseMillis=<n>` (drives adaptive IHOP and the mixed
collection's copy-time budget), `-XX:MaxHeapSize`,
`-XX:+HeapDumpOnOutOfMemoryError`.

## Backend details worth knowing

**Generational.** Young is a pair of semi-spaces with TLAB bump
allocation. A live JIT frame does **not** by itself force the non-moving
sweep: moving-young is on by default, and each cycle diverts to the
sweep only if its per-cycle coverage proof fails, if `promotion_oom_risk`
is honoured, or if `System.gc()` requested a full cycle. A JIT-warm
workload may legitimately spend most cycles non-moving — but that is a
*measured* fallback rate, not a rule, and the running process states its
own answer through `gc_metrics::collector_decision_report()`. (The older
"any JIT frame ⇒ non-moving" rule was the pre-2026-07-26 behaviour and is
reachable today only under `CRATONVM_NO_MOVING_YOUNG` or
`CRATONVM_MOVING_YOUNG_NO_JIT=1`; the evidence is in
[the TLAB/card audit §3.2](gc/tlab-and-card-audit.md).) That default young
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
intervals that actually contain a candidate. The 2026-07-18
truncated-oracle fail-safe survives as `verified_spans`: an interval
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

**G1.** A contiguous arena split into fixed regions with an O(log R)
address→region table. Young pauses evacuate all Eden+Survivor regions
except pinned ones (JNI-critical pins, conservative-JIT pins, frozen-peer
tails); remembered sets (per-region source sets fed by the post-barrier
plus a GC-internal rebuild each pause) drive old→young discovery. Mixed
pauses add the most-garbage Old regions bounded by count and copy-time
budget — only regions with liveness data from a completed mark cycle are
eligible. Marking is SATB tri-color with a background worker that also
drains the SATB shards each step; cleanup frees wholly-dead Old regions
in place, reclaims dead humongous spans (the ONLY humongous reclaimer)
and arms mixed collections. Humongous objects (> half a region) occupy
physically contiguous region runs and are never evacuated. Evacuation
failure self-forwards live objects in place, keeps their regions, and a
same-pause drain recovers them; a wedged drain leaves the kept regions
coherent (remembered-set edges recorded, precise liveness answers).
Allocation failure escalates: young pause → synchronous full mark cycle
→ OOM.

**ZgcRealHeap.** One arena + free list (post-sweep coalesced) + hash-set
registry of allocation bases. `needs_gc` triggers at 75 % occupancy with
a post-sweep re-arm so a large live set cannot storm. The sweep prunes
dead bases in place and feeds the exact dead list to the monitor
registry. Non-moving ⇒ the pointer map is always empty and no barriers
are needed; reference semantics come entirely from the VM-level
protocol. Mutators have no TLABs on this backend (every allocation takes
the arena lock) — it is a correctness-first reference backend, not a
throughput one.
