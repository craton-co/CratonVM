# G1 / ZGC correctness audit (2026-07-10) — OPEN findings

Audit of the G1 and ZGC backends at dev `57ad9415` (4 parallel deep-reads of
`gc/` + the VM↔GC integration layer, plus a deterministic probe kit run on
Linux against HotSpot jdk25 baselines: ChurnCheck / HumongousCheck /
CopyChurn / MTChurn / RefCheck, `/data/data/gcprobes-0710` on the Azure host).

**FIXED on branch `fix/gc-audit-g1-zgc-20260710`** (not listed below): G1
humongous-blind IHOP + no full-GC fallback (HumongousCheck OOM); TLAB
gap-sentinel desync in all 9 g1.rs region walkers; conservative-JIT-root pin
registry was thread-local (initiator-only) — now process-global per-thread;
parallel CSet builders missing the JIT-pin filter; adaptive IHOP decay-to-0
kill switch; stale IHOP occupancy after cleanup; ZGC free-list
never-coalesced fragmentation (CopyChurn OOM); ZGC `needs_gc` latch (GC storm
when live ≥ 75%); SATB pre-barrier missing on statics-side-table writes via
reflection/Unsafe/VarHandle/MethodHandle; unflushed thread-local SATB buffers
dropped at thread exit / JNI detach.

The items below are REAL and still OPEN. Severity ordered.

## 1. G1/ZGC: finalizer protocol absent + stale-address guard inert (probe-confirmed)
`VmHeap::collect_garbage_with_finalizers` ignores `finalizer_addrs` for G1
and ZGC (vm_heap.rs ~883-894, returns `Vec::new()`), so dead finalizable
objects are never resurrected → `finalize()` never runs (RefCheck:
`finalized=0/32` on G1 and ZGC vs HotSpot 32/32). Worse,
`process_references_after_gc`'s anti-stale guard `is_stale_young` is
hardwired `false` for G1/ZGC (`VmHeap::is_in_young_addr`), so a dead
finalizable/cleaner PRE-GC address can be enqueued and `run_finalizers`
later invokes `finalize()` on recycled memory (UAF / wrong-object
finalize). Fix shape: make the guard backend-generic
(`!pointer_map.contains_key(addr) && !heap.is_addr_live(addr)`), then
implement resurrection for G1 (root `finalizer_referent_addresses` during
the pause, mirror gen_heap's contract).

## 2. All backends: dead WeakReferences enqueued but referent never cleared (probe-confirmed)
RefCheck: `deadCleared=1/256 enqueued=256` on Generational, G1 AND ZGC
(HotSpot: 256/256). References are enqueued on the ReferenceQueue while
`get()` still returns the (dead) referent — a spec violation (clear must
happen before enqueue) pointing at the VM-level pre-GC-null/post-GC-restore
protocol restoring referents that the processor decided to clear. Also
`finalized=96/32` on Generational — finalize() runs ~3× per object
(re-registration / requeue bug).

## 3. STW barrier quota races: GC can run under a live mutator (probe-confirmed; root-caused; fix attempt parked)
6 threads churn + `synchronized(locks[i&15]){counters[i&15]++;}` at
-Xmx256m loses 2-78% of increments intermittently (~1 in 4-6 runs);
BinaryTrees(16) under G1+JIT completes with a WRONG run-varying total, and
the same family occasionally SEGVs in `scan_and_evacuate_refs` under host
load. `--nojit` and Generational/ZGC pass 100% (Generational is only
shielded by its non-moving-under-JIT sweep).

ROOT CAUSE (2026-07-10 deep-dive, instrumented with a watched-array
registry that caught workers writing through a superseded `counters[]`
copy while a pause was in flight): the stop-the-world barrier can satisfy
its arrival quota with the WRONG threads. A thread whose
`in_blocked_region` flag is up at request time is excluded from
`expected` — but when such a thread arrives at the barrier anyway
(`enter_blocked` pre_stw arm, `check_post_block_gc` drain, safepoint poll
while still flagged) `arrive_and_wait` counts it toward `arrived`,
filling a counted running mutator's quota slot; `wait_for_all()` then
releases while that mutator still runs and the collector evacuates
objects under live mutation. Two adjacent holes: blocked-region exit
clears `in_blocked_region` with a plain store (a pause requested in the
window between the drain loop and the clear excluded the thread, yet the
thread resumes bytecode mid-collection), and `thread_join` deposits its
root snapshot before retiring its TLAB.

FIX ATTEMPT parked on branch `wip/gc-stw-quota-race-20260710`
(excluded-tid snapshot atomic with the counts + `arrive_and_wait_auto` +
`leave_blocked_region_synced` + join retire-order): the approach closes
the accounting hole but AS IMPLEMENTED it intermittently HANGS the VM
outright under load (suspected starvation/livelock in the synced
blocked-region exit under continuous pause pressure) and still SEGVs on a
forced-GC path — do not merge without a redo + liveness argument. The
quota hole affects ALL moving collections, not just G1.
Repro: MTChurn.java / BinaryTrees.java in /data/data/gcprobes-0710.

## 4. G1/ZGC: STW hang risk when a JIT thread never polls (INT-3)
`stw_take_over_and_wait` falls back to a plain unbounded `wait_for_all()`
for non-Generational backends (`supports_jit_tlab_skip()` is
Generational-only). A compiled loop that neither allocates nor re-enters
the interpreter never arrives → whole-VM livelock under G1/ZGC + JIT.
Needs either xt-takeover extension to G1 (freeze + conservative scan +
pin) or back-edge safepoint polls.

## 5. JNI local references: scanned initiator-only, NEVER remapped (INT-2/INT-5)
`jni::update_local_refs_after_gc` has zero production callers (the comment
in roots.rs claiming gc.rs calls it is false) → any JNI local held across a
GC-triggering JNI call dangles after a moving (G1) collection. Parked
threads' JNI locals are not even scanned (`update_root_snapshot` never
calls `collect_local_ref_roots`) → reclaim of objects held only by a
non-initiator's JNI local. Same-family: `gc/src/pinned.rs` keep-alive set
is spliced only into the semispace `Heap` (INT-10).

## 6. G1: evacuation-failure kept regions — two edge gaps (G1CORE-3/4)
(a) `is_addr_in_live_region` treats every below-cursor address of a KEPT
region as live → dead weak referents there are "restored" with fields
still pointing into same-pause-freed regions (UAF via WeakReference.get()).
(b) A kept OLD region after a wedged drain holds GC-rewritten Old→young
edges recorded in NO remembered set → next young pause frees the live
young referent. Both need evacuation-failure pressure (near-OOM) to hit.

## 7. G1: mixed CSet selects liveness-unknown regions as "100% garbage" (G1CORE-7)
Regions promoted after the last cleanup have `live_bytes=0` /
`gc_efficiency=0` defaults → sorted FIRST with estimated cost 0, blowing
the pause budget copying fully-live regions and inviting evacuation
failure. HotSpot only mixes regions with marking data.

## 8. G1: open marking cycle grows SATB shards unboundedly (G1MARK-6)
Final remark is driven only from allocation-triggered GCs; a
mutation-heavy/allocation-free phase keeps SATB active with nothing
draining the shards (no size bound) → native memory bloat + O(entries)
remark pause. Needs a completion nudge (marker-thread drain or time/volume
trigger).

## 9. ZGC assorted (ZGC-3/4/5/6, INT-9)
- Monitor/cas-lock tables never pruned under ZGC (empty pointer_map
  early-return) → unbounded leak + stale-monitor inheritance on recycled
  addresses. Needs a dead-address channel through `MonitorCleanup`.
- Mark phase traces unvalidated child pointers (no containment/sanity
  check, unlike gen_heap/concurrent_mark) — corruption amplifier.
- `is_object_address`/`is_heap_addr`/`is_addr_live` are O(live-objects)
  linear registry scans under a mutex; no TLAB → GC pauses scale
  pathologically and reads as hangs (BUG-01 family, quadratically worse).
- Registry snapshot→publish window in `collect_garbage` can drop
  concurrently-registered allocations (unregistered-thread allocators).
- `ZgcRealHeap`'s internal ReferenceProcessor + all of
  ZgcHeap/ZgcCollector/zgc_concurrent.rs are dead code (metadata-only
  simulation, no VM consumer) — delete or wire up.

## 10. Misc
- Class unloading machinery (`gc/src/class_unloading.rs`) has no driver;
  statics/mirrors/class-locks are immortal roots (INT-7).
- G1 weak/soft refs with dead OLD-region referents are cleared only when
  the region is evacuated — concurrent-mark cleanup never feeds reference
  processing (INT-8).
- JIT inline putfield fast paths (default-off) elide the RSet post-barrier
  under G1 (`GC_FLAG_OLD_GEN` never set by G1) — must bail to helper or set
  the flag before ever enabling (INT-6).
- `RegionHeap` (gc/src/region.rs) is exported but unused, with known-unsound
  conservative slot rewriting — deprecate/hide (G1CORE-11).
- G1 string-dedup table stores raw addresses, never remapped (feature is
  default-off and API has no callers — latent landmine, G1CORE-6).
- Parallel young evacuator residual race (documented in-code) — keep
  `CRATONVM_G1_PARALLEL_EVAC` off (G1CORE-5).
- Generational (not G1/ZGC, noted in passing): `ConcurrentMarker` sweep has
  no TAMS — old-gen objects allocated between remark and sweep can be freed
  live (G1MARK-3).
