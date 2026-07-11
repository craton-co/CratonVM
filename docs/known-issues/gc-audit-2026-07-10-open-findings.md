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

**FIXED — third wave** (branch `fix/gc-open-items-20260710`): G1MARK-7 —
`remark()` plain-dropped ROOT/SATB seeds at the 1M gray-set cap (the
overflow rescan only re-walks MARKED objects, so a dropped unmarked seed
was freed live by cleanup); seeds are now marked black in place like the
CSet-drain path. G1MARK-8 — gray-set entries popped by
`concurrent_mark_step` now pass a header-plausibility gate (alignment +
allocated-prefix containment + the Generational marker's shared field
validator); a rejected entry sets a per-cycle fail-safe that makes
`cleanup` retain everything (no in-place frees, no humongous reclaim)
since the closure may be incomplete. INT-6 — both inline ref-putfield
arms (legacy + compact) now prepend the guarded-getfield receiver check
and require published region bounds: G1/ZGC publish none, so every
receiver bails to the full-barrier helper there (the GC_FLAG_OLD_GEN
"young" test is only meaningful under Generational). G1CORE-11 —
`RegionHeap` is `#[deprecated]` and no longer re-exported. INT-3
(diagnostics half) — the A5 unregistered-JIT-frame detector and the
moving-young-coverage check are ported to Linux
(`pthread_getattr_np`-based stack-high, memoized per thread).

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

SHARPENED (2026-07-10, INT-3 validation): the BinaryTrees(16) wrong total
reproduces SINGLE-THREADED under G1+JIT (~89.3k vs HotSpot 14723759;
run-varying; 100% exact with `--nojit`), identical with the INT-3 takeover
on and off — so this family has a single-threaded component (deep-recursion
JIT frame roots vs. evacuation, despite the wave-1 initiator pin-in-place)
that the multi-thread barrier/monitor races above cannot explain. The
earlier "single-threaded runs are 100% exact" note was measured on the
churn probes, not on deep recursion. Also: Gen+JIT BinaryTrees(16)
produces no output within 600s on BOTH the wave-3 dev binary and the INT-3
binary (pre-existing; JIT-frame-scan throughput class, cf. BUG-01).

## 2. G1/ZGC: STW hang risk when a JIT thread never polls (INT-3)
`stw_take_over_and_wait` falls back to a plain unbounded `wait_for_all()`
for non-Generational backends (`supports_jit_tlab_skip()` was
Generational-only). A compiled loop that neither allocates nor re-enters
the interpreter never arrives → whole-VM livelock under G1/ZGC + JIT.

**G1 core fix LANDED + LOAD-VALIDATED (2026-07-10, Azure probe host,
binary `gcprobes-0710/cratonvm-int3g1xt`).** Validation: new `SpinPoll`
probe (4 threads in a compiled, allocation-free, never-polling spin while
main forces 60 G1 GCs — the exact INT-3 shape) is HotSpot-exact 4/4 with
per-pause takeover telemetry (`linux took over tid=… 9 conservative
roots` + `pin_regions={0}` every young pause; the legacy path on the same
binary instead runs GC against stale snapshots — the corruption mode);
MTChurn G1+JIT 10/10 exact; ChurnCheck/CopyChurn/HumongousCheck/RefCheck
HotSpot-identical on G1; Generational unchanged (MTChurn 5/5, SpinPoll
exact); gc lib suite 775/775 on Linux. BinaryTrees G1+JIT stays wrong
pre- AND post-fix — that is finding 1's (sharpened) single-threaded
component, see above. The xt-takeover now engages under G1:
(a) frozen peers' un-retired TLAB tails are published to the G1 heap
(`G1Collector::set_jit_tlab_skip_regions`) and every linear region walker
(all 9 `gap_filler_len` sites) strides over them; (b) regions holding a
published tail are excluded from the CSet via `jit_pinned_region_set`
(un-gated on JIT activity — blocked-thread tails count too); (c) the VM
pins everything a frozen peer can address — its conservative xt/helper
roots AND its deposited snapshot roots (`pin_frozen_peer_roots_for_g1`,
`root_snapshots_for_os_tids`) — because an excused peer never applies the
cycle's pointer map to its frames, so under an evacuating collector those
objects must not move (Generational gets this for free from its non-moving
frozen-cycle sweep).

Still OPEN within INT-3:
- ZGC keeps the unbounded cooperative wait (no skip/pin protocol).
- The G1 concurrent-mark STW points (`brief_stw_counted*` in initial mark
  and final remark) still call plain `wait_for_all()` — a never-polling
  JIT loop livelocks those pauses too; they need the takeover threaded
  through (mark-only ⇒ no pin/skip required, just freeze + scan + excuse).
- (The A5 unregistered-JIT-frame detector's Linux port is NOT part of
  this item — it already landed in the third wave.)

## 3. Misc
- Class unloading machinery (`gc/src/class_unloading.rs`) has no driver;
  statics/mirrors/class-locks are immortal roots (INT-7).
- **INT-8 FIXED** (branch `fix/int8-g1-remark-refproc-20260710`): G1
  weak/soft refs with dead OLD-region referents now clear at
  concurrent-mark completion, and `finalize()` runs for objects reclaimed
  by cleanup's in-place frees. The corrected four-part shape from the
  third-wave analysis was implemented exactly: (1) marker referent-slot
  hiding — Weak/Soft/Phantom Reference-object addresses snapshotted from
  the registry at initial mark into `G1Collector::reference_skip`,
  `scan_object_refs` skips slot 0 of those objects, and every evacuation
  pause re-keys survivors / prunes CSet casualties (Finalizer/Cleaner
  registrations excluded — their slot 0 is a strong field); (2) SATB
  suppression on the protocol writes (`set_field_no_satb` RAII scope: the
  pre-pause referent null pass and remark-time clears); (3) a
  `Reference.get()` keep-alive barrier
  (`NativeContext::gc_reference_keep_alive` → `write_barrier_pre`, the
  G1ReferenceGet equivalent; `refersTo` stays exempt per its JDK
  test-without-retain contract); (4) remark-time reference processing —
  `g1_final_remark_and_cleanup` invokes a VM callback between the
  fixed-point drain and cleanup with the bitmap+TAMS `is_live_after_mark`
  predicate, with dead-by-mark staleness guards, and resurrects (mark +
  re-drain) everything handed out: dead finalizables, cleaner actions,
  pending cleaner chains, policy-retained soft referents. Plus:
  `force_gc_from_native` now calls `maybe_concurrent_gc` — a
  `System.gc()`-driven app could previously never start or complete a G1
  cycle at all. Validated on the probe host: new `RefCheckOld` probe
  (old-promoted weak referents in ~95%-live keeper regions + finalizables
  in wholly-dead regions, `-XX:InitiatingHeapOccupancyPercent=1`) went
  from `oldCleared=0/64 enqueued=0 finalized=0/32` to HotSpot-identical
  `64/64 / 64 / 32/32 / softKept=8/8` (5/5 runs, two flag sets); full
  regression batch green (RefCheck/Churn/Copy/Humongous × Gen/G1/ZGC).
- G1 string-dedup table stores raw addresses, never remapped — API now
  carries a DO-NOT-WIRE-UP doc warning; the remap is still needed before
  `-XX:+UseStringDeduplication` can do anything (G1CORE-6).
- Parallel young evacuator residual race (documented in-code) — keep
  `CRATONVM_G1_PARALLEL_EVAC` off (G1CORE-5).

## Cross-confirmation from an independent real-world trigger (2026-07-10)

Investigating the ES `DiversifyingChildrenIVFKnnFloatSlicedVectorQueryTests` /
`IVFKnnFloatVectorQueryTests` hang cluster
(`docs/known-issues/elasticsearch-suite/ES-HANG-20260709-...`) independently
reproduced this same finding via a real Lucene `IndexWriter` workload — no
synthetic MTChurn harness involved. Worktree
`/data/data/wt-es-vectors-ivfknn-hang-20260710`.

Repro (`testSlicesDense`, `--nojit` or `CRATONVM_JIT_GETFIELD_HELPER=1`,
`--Xmx 2g`) is non-deterministic across runs, same seed:
- Most runs: permanent spin (82-116% CPU, zero forward progress) with the
  worker thread AND a `Lucene Merge Thread` both parked in
  `Monitor::block_enter`/`enter` (`vm/src/threading/monitor.rs:457/500`) —
  matching the "5 waiters, nobody woken" shape exactly, just with 2 waiters
  instead of 5 (Lucene's own concurrency here is far lighter than MTChurn's
  6-thread stress). Live gdb confirms both threads are genuinely parked
  (`parking_lot::Condvar::wait`), and with only 6 threads total in the process
  and 4 of them idle/legitimately-parked elsewhere, there is no live thread
  positioned to ever call the matching `exit()`/notify — consistent with
  the finding-1(b) orphaned-monitor hypothesis (a stale/wrong owner, or a
  lost wakeup, rather than genuine live contention).
- One run (153s, no permanent hang) instead **completed with 2 real
  failures**, including a genuine Lucene-internal NPE surfaced through
  `ConcurrentMergeScheduler.handleMergeException`:
  `NullPointerException: Cannot invoke
  "org.apache.lucene.index.ReadersAndUpdates.dropMergingUpdates()" because
  "rld" is null` — `rld` comes out of `IndexWriter`'s `readerPool` map and
  should never be null on this path. Immediately preceding this failure: a
  sustained ~49s burst of 544+ (rate-limited) `gen_heap::get_field`
  out-of-bounds WARNs, all `class_id=ClassId(0) num_slots=0
  java/lang/Object` — the "zeroed live object" shape — escalating in volume
  the longer the run's merge activity continues.

This is independent, real-world corroboration of finding 1: the SAME
underlying probabilistic GC/monitor race manifests as either a lost-wakeup
deadlock (most runs) or live data corruption surfacing as a downstream
Lucene NPE (this run) depending on exact timing — matching "intermittently
loses increments... (~1/5 runs)" and "an exception thrown mid-churn is
itself a corruption symptom" in the finding-1(b) writeup above, just via a
completely different trigger workload than MTChurn. The OOB-field-read WARN
burst right before the NPE is worth treating as a corruption *symptom*
here, not the previously-assumed-benign "collection-layout probe" — at
minimum the volume/timing correlation with a real NPE is suspicious enough
to warrant using it as an additional signal alongside `CRATONVM_DBG_ATHROW`
when chasing finding-1(b)'s precursor exception.

The finding-1(b) registry-miss tripwire (this doc's WARN counter) did NOT
fire during either manifestation of this repro — so if finding-1(b)'s
specific "second orphaned Monitor via a registry-lookup miss" mechanism is
involved here, it isn't the ONLY path to the same lost-wakeup symptom, or
this repro's race window differs enough from MTChurn's to not hit that
exact branch. Worth rechecking once finding-1(a)'s quota-race fix is
actually stabilized (current `wip/gc-stw-quota-race-20260710` attempt is
parked, not merged).

No fix attempted here — deferred to whoever picks up finding 1, given the
existing parked WIP attempt already found this subsystem is not safe to
patch quickly. This ES workload is a reusable, real-world (non-synthetic)
additional repro for validating any future fix attempt, in addition to
MTChurn/BinaryTrees(16).
