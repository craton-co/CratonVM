# G1 / ZGC correctness audit (2026-07-10) — OPEN findings

Audit of the G1 and ZGC backends at dev `57ad9415` (4 parallel deep-reads of
`gc/` + the VM↔GC integration layer, plus a deterministic probe kit run on
Linux against HotSpot jdk25 baselines: ChurnCheck / HumongousCheck /
CopyChurn / MTChurn / RefCheck / BinaryTrees, `/data/data/gcprobes-0710` on
the Azure host).

**FIXED — first wave** (dev `8b8a0790`): G1 humongous-blind IHOP + no
full-GC fallback; TLAB gap-sentinel desync in all 9 g1.rs region walkers;
conservative-JIT-root pin registry was thread-local (initiator-only) — now
process-global per-thread; parallel CSet builders missing the JIT-pin
filter; adaptive IHOP decay-to-0 kill switch; stale IHOP occupancy after
cleanup; ZGC free-list never-coalesced fragmentation; ZGC `needs_gc` latch;
SATB pre-barrier missing on statics-side-table writes; unflushed
thread-local SATB buffers dropped at thread exit / JNI detach.

**FIXED — second wave** (branch `fix/gc-audit-g1-zgc-20260710`, commits
`6af7bf8c`..): java.lang.ref protocol (weak refs now CLEAR on enqueue —
queue linkage moved off the referent slot onto the real `next` field;
finalize() runs exactly once via GC resurrection on G1 AND ZGC plus
processor-entry flagging; backend-exact post-GC staleness guard replacing
the G1/ZGC-inert young-only check — RefCheck is HotSpot-identical on all
three backends). JNI local refs scanned on parked threads and REMAPPED
after moving GC on all three per-thread paths; JNI pin set spliced into
collect_roots for every backend + re-keyed after moves. ZGC: hash-set
registry (O(1) conservative-root validation), mark-phase wild-child
validation, in-place sweep prune (snapshot-race leak), monitor/cas-lock
dead-address pruning channel. G1: mixed CSet only selects Old regions with
marking data; the concurrent marker drains SATB shards each step (bounded
queue, smaller remark); unresolved-kept-region coherence after a wedged
evacuation-failure drain (rset edges recorded + precise liveness).
Generational: concurrent old-gen sweep gained a remark-time TAMS snapshot
(objects allocated remark→sweep are implicitly live).

The items below are REAL and still OPEN. Severity ordered.

## 1. STW/monitor race family: GC and monitors vs excluded threads (probe-confirmed; partially root-caused)
Reproducers: MTChurn (6 threads, `synchronized(locks[i&15]){counters[i&15]++}`,
-Xmx256m) intermittently loses increments on G1+JIT (~1/5 runs) and — less
often — on Generational under load; BinaryTrees(16) G1+JIT completes with a
wrong run-varying total; rare SIGSEGV in `scan_and_evacuate_refs` on a
forced-GC-from-exception path. `--nojit` and single-threaded runs are 100%
exact.

TWO stacked defects identified:

(a) **Barrier quota races** (root-caused, fix parked on
`wip/gc-stw-quota-race-20260710`): the STW barrier counts arrivals from
threads its request EXCLUDED as blocked (enter_blocked pre_stw /
post-block drain / safepoint-while-flagged) — an excluded arrival fills a
counted running mutator's quota slot and `wait_for_all()` releases while
that mutator still mutates. Adjacent: blocked-region exit clears
`in_blocked_region` with a plain store (excluded thread resumes bytecode
mid-pause), and `thread_join` deposits its snapshot before retiring its
TLAB. The parked fix's accounting (excluded-tid snapshot atomic with the
counts + `arrive_and_wait_auto` + `leave_blocked_region_synced`) is sound
in design and does NOT itself deadlock —

(b) **but a residual monitor-vs-evacuation race survives it**: a
gdb-captured incident shows ALL worker threads parked in
`Monitor::block_enter` simultaneously (16 locks, 5 waiters — a lost-wakeup
pile-up, not contention), main in `Object.wait`, and the last thread
SEGV-ing in `scan_and_evacuate_refs` inside a `maybe_gc_forced` reached
from `create_exception_object` (an exception thrown mid-churn is itself a
corruption symptom). With the quota fix the corruption's manifestation
shifts from lost increments to lost wakeups (silent hang). Hypothesis to
validate next: monitor identity/lock-word state diverges when the locked
object (or the contended-monitor table keying) is evacuated in the window
between `enter_or_contend` and `block_enter` / between thin-lock CAS and
inflation — i.e. the monitor table remap protocol is not atomic with
respect to threads mid-acquisition that the pause treated as excluded.
Next steps: CRATONVM_DBG_ATHROW to identify the precursor exception;
instrument enter_or_contend/remap_after_gc with a generation check;
consider keying inflated monitors by identity hash instead of address.
Affects all moving collections; Generational is only shielded by its
non-moving-under-JIT sweep.

## 2. G1/ZGC: STW hang risk when a JIT thread never polls (INT-3)
`stw_take_over_and_wait` falls back to a plain unbounded `wait_for_all()`
for non-Generational backends (`supports_jit_tlab_skip()` is
Generational-only). A compiled loop that neither allocates nor re-enters
the interpreter never arrives → whole-VM livelock under G1/ZGC + JIT.
Needs either xt-takeover extension to G1 (freeze + conservative scan +
pin) or back-edge safepoint polls. Related: the A5 unregistered-JIT-frame
detector is `#[cfg(windows)]`-only and needs `JIT_CODE_RANGES` (precise
maps) — port to Linux now that precise maps default ON.

## 3. Misc (unchanged from the first wave)
- Class unloading machinery (`gc/src/class_unloading.rs`) has no driver;
  statics/mirrors/class-locks are immortal roots (INT-7).
- G1 weak/soft refs with dead OLD-region referents are cleared only when
  the region is evacuated — concurrent-mark cleanup never feeds reference
  processing (INT-8: run the ReferenceProcessor against the mark bitmap
  after `g1_final_remark_and_cleanup`, before regions are freed).
- JIT inline putfield fast paths (default-off) elide the RSet post-barrier
  under G1 (`GC_FLAG_OLD_GEN` never set by G1) — must bail to helper or
  set the flag before ever enabling (INT-6).
- `RegionHeap` (gc/src/region.rs) is exported but unused, with
  known-unsound conservative slot rewriting — deprecate/hide (G1CORE-11).
- G1 string-dedup table stores raw addresses, never remapped — API now
  carries a DO-NOT-WIRE-UP doc warning; the remap is still needed before
  `-XX:+UseStringDeduplication` can do anything (G1CORE-6).
- Parallel young evacuator residual race (documented in-code) — keep
  `CRATONVM_G1_PARALLEL_EVAC` off (G1CORE-5).
- G1 overflow-rescan holes under a >1M-entry gray set (G1MARK-7) and
  missing header-plausibility validation in `concurrent_mark_step`
  (G1MARK-8) — defense-in-depth items.
