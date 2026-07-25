# Garbage Collection in CratonVM — architecture and current state

*Last updated 2026-07-11, after the four-wave G1/ZGC correctness audit
(see [known-issues/gc-audit-2026-07-10-open-findings.md](internal/gc-audit-2026-07-10-open-findings.md)
for the finding-by-finding record).*

CratonVM ships three garbage-collector backends behind one dispatcher
(`gc/src/vm_heap.rs::VmHeap`). All are stop-the-world at the collection
level; G1 additionally runs its marking phase concurrently. Selection is
java-launcher-compatible:

| Flag | Backend | One-liner |
|---|---|---|
| *(default)* | `GenerationalHeap` (`gc/src/gen_heap.rs`) | Semi-space young gen + free-list old gen with a concurrent old-gen mark-sweep cycle. Young collections run **non-moving** whenever any JIT frame is active. |
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

**Known open issues** (details, evidence and reproducers in
[known-issues/gc-audit-2026-07-10-open-findings.md](internal/gc-audit-2026-07-10-open-findings.md)):

1. **STW barrier quota race (finding 1)** — the barrier can count an
   excluded (blocked) thread's arrival toward its quota and release the
   initiator while a counted mutator still runs. Intermittent lost
   `synchronized` updates / wrong results / rare GC-time SIGSEGV under
   multi-threaded JIT churn on moving collectors (≈1-in-5 on the MTChurn
   probe), plus a *single-threaded* deep-recursion component
   (BinaryTrees under G1+JIT). A fix attempt is parked on
   `wip/gc-stw-quota-race-20260710` — its accounting is sound but a
   residual race survives it; do not merge as-is.
2. **Class unloading** (`gc/src/class_unloading.rs`) has no driver:
   classes, mirrors and statics are immortal roots (footprint, not
   correctness).
3. **String deduplication** is plumbed (`-XX:+UseStringDeduplication`)
   but intentionally inert: the dedup table is not remapped across
   collections and the API carries a do-not-wire-up warning.
4. **G1 parallel young evacuator** (`CRATONVM_G1_PARALLEL_EVAC`) has a
   documented residual race — keep it off (default).

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
allocation; while any thread holds a JIT frame the young collection is a
non-moving sweep with selective promotion (this is the load-bearing
reason conservative JIT roots are safe here). That default young
collection is PARALLEL in two phases. The transitive closure is drained
by several workers over a lock-free mark bitmap (one bit per 8 bytes of
from-space) — sound because the phase is pure and read-only on a frozen
heap and the only write is an atomic bit claim. The sweep walk, a linear
header chase that is inherently sequential, is split at anchors the
mark phase's exact-base oracle walk records for free: every offset that
walk parsed an object at is a verified grid position, and nothing
allocates, frees or resizes an object between the two walks. Each chunk
re-proves its own anchor by requiring its chain to land exactly on the
next one, and the parallel walker writes nothing — on any grid anomaly
it is abandoned wholesale and the untouched sequential walk (which owns
every diagnostic and the unwind/re-anchor recovery) runs from scratch.
Parallel EVACUATION does not exist here and must wait for the moving
young gen to be fixed (`docs/known-issues/moving-young-gen-drops-jit-held-oops.md`). Old gen is a free-list
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
