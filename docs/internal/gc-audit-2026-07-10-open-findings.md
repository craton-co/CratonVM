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

Per-item status as of 2026-07-11: finding 1 FIXED (status block inside);
finding 2 FIXED (earlier); the three Misc items remain OPEN but are inert by
default (details in §3). Severity ordered.

## 1. STW/monitor race family: GC and monitors vs excluded threads — FIXED 2026-07-11 (branch `fix/gc-finding1-stw-monitor-20260711`), see status block below

**STATUS UPDATE (2026-07-11, dedicated fix effort):**

**(a) Barrier quota race — FIXED**, via a from-scratch redesign, NOT the
parked `wip/gc-stw-quota-race-20260710` (which had two fatal flaws its own
validation caught: a starvation-prone `while stw_requested` flag-clear loop
under continuous pause pressure, and — the subtle one — it applied the
composed blocked-fixup to the frames BEFORE the synchronized flag-clear, so
a pause starting mid-application could move objects again and its fold was
keyed against slots the thread was concurrently rewriting: frames resumed
one pause stale, which is the `scan_and_evacuate_refs` SEGV it kept hitting).
The landed design:
- the census (`alive_count_blocked_and_os_tids`) returns the blocked
  threads' IDENTITIES, stored in `GcBarrierInner::excluded_blocked`
  atomically with `expected` under the barrier transition lock;
- `arrive_and_wait_auto` — participation decided per pause from that set,
  used at every `enter_blocked` `pre_stw` arm, the safepoint poll, and the
  jni/vm_init/vm_util blocked sites (an excluded arrival can no longer fill
  a counted mutator's quota slot);
- `GcBarrier::leave_blocked_region_flagged` — the wake path's drain and the
  `in_blocked_region` clear are ONE atomic step under the barrier lock
  (waits out each active pause generation-keyed, arrives auto exactly-once
  per pause that counted it, clears the flag only under a lock hold that
  confirmed no pause is active). Only THEN does `check_post_block_gc_refs`
  take + apply the fixup: any newer pause counts the thread and cannot
  complete (or fold) until its next safepoint arrival — the fold/fixup race
  is structurally gone;
- `deposit_root_snapshot_no_flag` for the wake-path snapshot refresh (the
  old tail transiently re-raised the flag and then plain-stored it false —
  the mutation window);
- TLAB retire reordered BEFORE the deposit at monitor_enter_blocking /
  monitor_enter_synchronized_method / monitor_wait / thread_join / park /
  ensure_class_initialized (the retire's gap-filler heap write must precede
  the flag raise, after which a pause may collect concurrently);
- both remark pauses (Generational + G1 concurrent-mark) converted from the
  legacy anonymous census to the identity census.

**(single-threaded component) ROOT-CAUSED + FIXED — it was never a GC race.**
The BinaryTrees(16) G1+JIT wrong total is a **compact-reference-field-layout
vs G1 field-accessor mismatch**: G1's `get_field`/`set_field` (and volatile
variants, `gc/src/g1.rs`) compute `index * SLOT_SIZE` unconditionally — zero
`GC_FLAG_COMPACT` awareness — while the backend-agnostic JIT inline-TLAB
allocation (`jit/src/x64.rs::emit_inline_tlab_new`) and `jit_tlab_post_init`
mark fresh objects of registered-layout classes compact
(`CRATONVM_COMPACT_REF_FIELDS` default-on). Every compact-marked object is
then WRITTEN as legacy 16-byte cells through `heap.set_field` (the second
ref field's cell lands past the object's end — silent neighbour stomp) and
READ as bare-8-byte compact slots by `jit_getfield`, whose plausibility gate
degrades the mis-read Value tag word to null. Hence: `Node.l == null` for
essentially EVERY JIT-built tree (~89k totals = the all-ones floor), the
occasional NPE from the crossed slot, heap-size independence (8 GB BT12 was
deterministically wrong with ~no GCs at all), corruption of even 30-node
d=4 trees no GC could intersect, `--nojit` exact (interpreter is
self-consistently legacy through the same G1 accessors), Gen exact (its
accessors are compact-aware), ZGC exact (no TLABs → no compact marking),
and `CRATONVM_COMPACT_REF_FIELDS=0` exact. Diagnosed by proving the G1 and
Gen disasm of `make`/`check` byte-identical (CRATONVM_DBG_JIT_DISASM), then
a minimal probe (`gcprobes-0710/G1Probe.java`) showing JIT'd `chk()` reading
`n.l == null` while the interpreted caller reads the SAME object correctly.
FIX: `SharedVm::new` force-disables compact-ref-fields
(`set_compact_ref_fields_enabled(false)`, first-set-wins, before any
classloading) whenever `config.gc_algorithm != Generational` — the process
is uniformly legacy under G1/ZGC, the exact (validated) behaviour of
`CRATONVM_COMPACT_REF_FIELDS=0`. Real compact support in G1/ZGC accessors,
scanners, and allocators is future work; until then this gate is the
correctness boundary. Validated: BT16/BT14/BT12(8g) G1+JIT exact 10/10,
G1Probe exact, ZGC/Gen-nojit exact, full probe battery green.

**(b) Monitor-vs-evacuation residual — PARTIALLY CLOSED; remainder
re-classified.** The (a) fix closes every mechanism this finding enumerated
(quota holes, the flag-clear window, the fold/fixup race, the
excluded-thread-runs-mid-pause family), and the compact fix removes the
pervasive G1 source of "impossible" field values. The ES
`InetAddressRandomBinaryDocValuesRangeQueryTests` IMSE+CCE manifestation,
however, PERSISTS at the baseline rate (fixed build: clean=18 imse=1 cce=2
of 20; dev-tip control: clean=17 cce=2 other=1 of 20 — statistically
identical), so its mechanism was never the barrier. New forensic evidence
(this effort, `CRATONVM_DBG_MONEXIT` — a new gated diagnostic in
`MonitorTable::exit`) pins the failure shape exactly:

```
[MONEXIT-IMSE] arm=thin-arm tid=2 obj=0x200253d3cd0 mark=0x0 state=NEUTRAL
               class_id=0 num_slots=0 registry_hit=false
```

— the synchronized-method receiver points at an ENTIRELY ZEROED slot at
monitorexit time: the object was never copied by the moving young
collection (a missed MARKING ROOT — not a missed remap: all three
pointer-map application paths were audited and each forwards frames,
`monitor_on_exit`, `native_pin_roots`, `java_thread_obj`, scoped values and
JIT/shadow slots) and young-from was reset over it. Reference processing is
exonerated as the sole writer: a 25-run arm with `CRATONVM_DBG_NO_REFPROC=1`
still reproduced the identical IMSE+CCE pair. This is the already-tracked
**moving-young GC-precision / missed-root family** (lost operand-stack tag
/ side-structure root gap — see `CRATONVM_GC_VERIFY_STALE`'s doc comment,
the compact-ref-fields memory's residual section, and the HIB temporal /
OSR-main corruptor cluster), NOT a monitor/STW-barrier defect. Repro for
whoever picks it up: `org.junit.runner.JUnitCore
org.elasticsearch.lucene.queries.InetAddressRandomBinaryDocValuesRangeQueryTests`
(server-module bundle at `/data/data/es-jit-deopt-gc-bundle-20260708-214648`,
`-Dtests.seed=B17AC9D3E1F2A0C4 -Dtests.asserts=false -Xmx2g`, ~10-15%/run,
~40-60s/run) with `CRATONVM_DBG_MONEXIT=1` and `CRATONVM_GC_VERIFY_STALE=1`.

Two adjacent latent holes WERE found and fixed/flagged while ruling
mechanisms out:
- `monitor_wait`'s error path early-returned BEFORE `check_post_block_gc`,
  leaving `in_blocked_region` permanently raised on a running thread (every
  later census would exclude it → moving GC concurrent with its bytecode).
  FIXED (run the wake re-sync before propagating the error).
- The Generational CONCURRENT OLD-GEN SWEEP (`concurrent_mark.rs::
  concurrent_sweep`) frees old objects with NO dead-address notification:
  unlike ZGC (which calls `MonitorTable::prune_dead` — "prevents a recycled
  address from inheriting a dead object's monitor"), Gen never prunes
  monitor/cas-lock registry entries for swept objects, and
  `VmHeap::is_addr_live`'s Gen arm treats EVERY old-gen address as live
  (region-granular), so reference-processing verdicts for old-gen referents
  cannot see concurrent-sweep kills. Neither was demonstrated as the ES
  trigger (the forensics point at young-gen zeroing), but both are real
  recycled-identity hazards — tracked as OPEN follow-ups in §3.

Historical analysis below preserved for context.
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

FRESHNESS CHECK (2026-07-11, dev tip incl. all four waves + INT-3
residuals, binary `gcprobes-0710/cratonvm-docverify`, idle host): MTChurn
G1+JIT passed 9/9 — the multi-threaded lost-increment manifestation is now
substantially rarer than the original ~1/5 (the INT-3 takeover + wave
fixes narrowed the window; NOT proven closed — it remains load-sensitive
and the quota-race mechanism is still present on dev). BinaryTrees(16)
G1+JIT remains the reliable reproducer (TOTAL 89207 vs 14723759 this
run). Everything else in the kit — RefCheck/RefCheckOld/Churn/Copy/
Humongous/SpinPoll × Gen/G1/ZGC — is HotSpot-identical on this tip.

VALIDATION APPENDIX (2026-07-11 fix effort, branch
`fix/gc-finding1-stw-monitor-20260711`, binaries `cratonvm-gcf1-fix{b,c,d}`
at `/data/wt-gc-finding1-20260711`): full battery green — BT16-G1-JIT 5/5
exact (14723759, was 0/5 at ~89k), BT14-G1-JIT 3/3, BT12-G1-8g 2/2,
BT16-ZGC 2/2, BT16-Gen-nojit 1/1, G1Probe-G1 2/2, MTChurn-G1 15/15 +
8/8 (fixc), MTChurn-Gen 10/10 + 5/5, MTChurn2-G1 10/10 + 5/5, MTChurn3-G1
10/10 + 5/5, ChurnCheck-G1/Gen 3/3 + 3/3, CopyChurn-G1 3/3,
HumongousCheck-G1 3/3, RefCheck-G1 3/3, RefCheckOld-G1 2/2, SpinPoll-G1
3/3, SpinPoll-Gen 2/2. vm-crate `--lib` tests: 2184 passed, 14 failed —
the identical 14 fail at the unmodified base commit (pre-existing
release-mode lock_order/vm_init cluster); `threading::` filter 281/281.
No liveness regressions observed anywhere (the parked WIP's failure mode).

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

**Residuals CLOSED + probe-host-VALIDATED (2026-07-10/11 second pass,
binary `gcprobes-0710/cratonvm-int3resid`).** Validation: `SpinPollMark`
probe (spinners hold a live Node in JIT state and run allocation-free
compiled spins for the whole choreography; main retains 58MB, ages it
into Old with 24 forced GCs — tenuring ~15, plain churn never promotes —
then churns young so IHOP=25 starts the cycle) is HotSpot-exact on
G1/ZGC/Gen, with gdb breakpoint confirmation (SIGUSR2 passthrough — gdb
otherwise intercepts the takeover's rendezvous signal) of THREE full
concurrent-mark cycles per run: `g1_start_concurrent_mark` ×3 +
`g1_final_remark_and_cleanup` ×3, all inside never-polling spin windows.
ZGC: SpinPoll 3/3 + MTChurn 5/5 + Churn/Copy/RefCheck HotSpot-identical.
G1 regression: MTChurn 10/10 + SpinPoll 3/3 + all gates exact. Gen:
MTChurn 5/5 + both spin probes exact (256m; 80–128m Gen runs OOM on the
retained set — binary-parity, pre-existing sizing). NOTE for future
probes: `tracing::debug!` is compiled out of release
(`release_max_level_info`) and `-verbose:gc` is parsed but unconsumed —
use gdb breakpoints on un-inlined cross-crate (LTO-off) gc-crate symbols
for cycle confirmation. A pre-existing `POST-GC STALE LOCAL` tripwire
(main's frame local under OOM-pressure G1) fires identically pre/post —
ROOT-CAUSED (2026-07-11) as a benign detector artifact: under the G1
evacuation-failure retry, a drain pass allocates to-space from regions the
first pass freed, so a first-pass FROM-address (map key) is legitimately
handed out again as a drain DESTINATION (map value) for a different
object; a slot correctly rewritten to that recycled address still matches
a key and tripped the detector (`CRATONVM_DBG_BUG03` shows the rewrite
happening in the same pause; every firing follows a `[g1][RETRY]` drain).
`verify_no_stale_refs` now recognises recycled destinations (benign,
reported only under `CRATONVM_GC_VERIFY_STALE=1`); the frame remap itself
was always correct.
- ZGC: `supports_jit_tlab_skip()` now returns true for every backend. ZGC
  is trivially safe for the takeover — `ZgcRealHeap` is a non-moving STW
  mark-sweep whose sweep walks the allocation-base REGISTRY (never linear
  memory), and `VmHeap::refill_tlab` never hands ZGC mutators a TLAB, so
  un-retired tails cannot exist; frozen peers' conservative roots are
  ordinary mark roots.
- The four concurrent-mark STW pauses (Generational initial-mark + remark
  in `maybe_concurrent_gc`, G1 initial-mark in `g1_concurrent_mark_cycle`,
  G1 final-remark in `g1_final_remark_cleanup`, which `g1_force_full_cycle`
  reuses) are open-coded as request → `stw_take_over_and_wait` → work →
  clear/resume → `complete_gc` instead of `brief_stw_counted*`'s plain
  internal `wait_for_all()`. Frozen peers' conservative roots join the
  MARK root sets (their stale deposit snapshot alone could miss a live
  root → cleanup/sweep frees a live object). Mark pauses move nothing, so
  no pins — but the G1 final-remark `cleanup` linearly walks every
  non-Free region, so the takeover's frozen-TLAB-tail publication is
  load-bearing there and stays.

Remaining note:
- (The A5 unregistered-JIT-frame detector's Linux port is NOT part of
  this item — it already landed in the third wave.)

## 3. Misc

Status 2026-07-11: the three OPEN items below are all inert in default
configuration (no driver / opt-in flag off / feature not wired). They are
real gaps to close before their features can ship, but none is a live
correctness issue on a default run. Left OPEN deliberately by the finding-1
fix effort — each needs its own design work, and bolting quick patches onto
G1 internals is how the parked quota-race WIP went wrong.

- Class unloading machinery (`gc/src/class_unloading.rs`) has no driver;
  statics/mirrors/class-locks are immortal roots (INT-7). OPEN — memory
  growth only (unbounded class-metadata retention), not corruption; needs a
  real lifecycle design (mirrors/statics/class-locks + JIT code invalidation).
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
  `-XX:+UseStringDeduplication` can do anything (G1CORE-6). OPEN — inert
  (the flag is parsed but the table is never wired up, so no correctness
  exposure); implement the evacuation-pause remap before wiring.
- Parallel young evacuator residual race (documented in-code) — keep
  `CRATONVM_G1_PARALLEL_EVAC` off (G1CORE-5). OPEN — opt-in flag, default
  off; the serial evacuator is the supported path.
- NEW (found by the 2026-07-11 finding-1 effort, tracked here): G1 and ZGC
  have no compact-reference-field-layout support in their field accessors /
  allocators (see the finding-1 single-threaded-component fix above —
  compact is now force-disabled for non-Generational backends in
  `SharedVm::new`). Re-enabling compact under G1/ZGC requires:
  compact-aware `get_field`/`set_field`(+volatile) and humongous
  translation, compact-aware evacuation/mark scanners, and compact marking
  in the backend allocators — plus removing the `SharedVm::new` gate.
- NEW (2026-07-11 finding-1 effort): the Generational concurrent old-gen
  sweep frees objects with no dead-address channel — no
  `MonitorTable::prune_dead` (ZGC has this; a recycled old address can
  inherit a dead object's monitor/cas-lock registry entry) and no
  reference-processor reconciliation (`is_addr_live`'s region-granular
  old-gen arm classifies swept referents as survivors, so a dead old-gen
  weak referent can be RESTORED into a Reference and, after free-list
  reuse, `get()` returns an unrelated object — silent type confusion).
  Fix shape: have `concurrent_sweep` return its freed-address list; the
  maybe_concurrent_gc driver (interpreter.rs) forwards it to
  `shared.monitors.prune_dead` and to a new ref-processor
  dead-referent reconciliation; alternatively/additionally make the Gen
  `is_addr_live` old-gen arm object-granular via a freed-address set
  maintained by `OldGen::free`/`alloc`.

## Cross-confirmation from an independent real-world trigger (2026-07-10)

> STATUS (2026-07-11 fix effort): the IVFKnn hang could NOT be
> re-confirmed as a deadlock on the fixed build — a live gdb attach during
> a "hung" `testSlicesDense` run (fix build, `--nojit`, loaded host) shows
> NO thread in `Monitor::block_enter`: the SUITE worker is actively
> executing (memcmp/string-eq), main is in a legitimate `Object.wait`, and
> the launcher in `pthread_join` — i.e. a slow interpreted run under host
> load (load avg 14+), not the documented lost-wakeup pile-up. The
> `gen_heap::get_field` OOB WARN burst does still occur and per its own
> message text can be benign speculative collection-layout probing; treat
> it as a signal only when correlated with real failures. The
> InetAddress-range manifestation (section below) is the cheaper, still
> reproducing tracker for the residual — see finding 1(b)'s status block
> for its forensic classification (missed-marking-root family, not
> barrier/monitor).

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


## Second cross-confirmation from an independent real-world trigger (2026-07-11)

> STATUS (2026-07-11 fix effort): still reproduces at the same rate on the
> fixed build (fixed: 18 clean / 1 imse / 2 cce of 20; dev-tip control:
> 17 clean / 2 cce / 1 other of 20) — mechanism forensically classified as
> the moving-young missed-marking-root family, NOT the barrier/monitor
> races this finding originally hypothesized. See finding 1(b)'s status
> block for the `[MONEXIT-IMSE]` capture and the repro-with-diagnostics
> recipe.

While investigating a `ClassCastException` spotted once during verification of
the (separate, since-fixed) `InetAddress` GC-root bug in
[`ES-HANG-20260709-...-inetaddressrandombinarydocvaluesrangequerytests-51a9c7ea93-FIXED.md`](elasticsearch-suite/ES-HANG-20260709-server-org-elasticsearch-lucene-queries-inetaddressrandombinarydocvaluesrangequerytests-51a9c7ea93-FIXED.md),
reproduced this same finding a THIRD independent way, via yet another real
Lucene/ES workload — `org.elasticsearch.lucene.queries.InetAddressRandomBinaryDocValuesRangeQueryTests`
(`testRandomMedium`, seed `B17AC9D3E1F2A0C4`, `--Xmx 2g`, clean dev tip
`48c2d92d`). Worktree `/data/data/wt-inetaddress-cce-20260711` on the Azure
host, no code changes (docs-only).

20 runs (2 parallel streams of 10), all against the identical seed:
- **17/20 clean.**
- **3/20 (15%)** hit an identical pair of symptoms, always on
  `testRandomMedium`, never on the other 5 test methods in the class:
  ```
  WARN cratonvm_vm::vm::vm_exec: implicit monitorexit on synchronized-method exit failed thread_id=ThreadId(2) error=InternalError(Runtime(IllegalMonitorStateException { message: "thread Thread-2 does not own the monitor for object at 0x..." }))
  ...
  java.lang.ClassCastException: java.util.ArrayList cannot be cast to java.lang.String
  ```
  (the `ClassCastException` carries **zero stack-trace frames** — printed
  directly under the JUnit `1) testRandomMedium(...)` header with no
  intervening `at ...` lines at all).
- **1/20** hit the separate, already-known `java/util/Set` GC-staleness NPE
  (side-table-unpinned-locals class of bug, tracked independently — not
  this finding).
- The `IllegalMonitorStateException`/"does not own the monitor" warning
  appears in **exactly** the 3 runs that hit the `ClassCastException`, and
  in **none** of the other 17 — a tight, 100%-correlated pairing across this
  sample.

This matches finding 1(b)'s core hypothesis precisely: a stale/wrong
monitor owner (not genuine contention — nothing in this single-writer,
mostly-single-thread-visible test workload should ever contend a lock hard
enough to matter) immediately preceding a corrupted downstream exception
(here: a `ClassCastException` with an impossible empty stack trace, rather
than the IVFKnn corroboration's `NullPointerException` through
`ConcurrentMergeScheduler` or the original writeup's SEGV reached from
`create_exception_object` — the same "an exception thrown mid-churn is
itself a corruption symptom" pattern, a third distinct downstream shape).
`Thread-2` here is almost certainly `IndexWriter`'s background merge
machinery or a `RandomizedRunner` policing thread (not confirmed via gdb in
this pass — this repro is CPU-cheap enough, ~30-40s/run, single JUnit class,
no WildFly/no custom harness, that a future gdb session attaching mid-run
across a handful of repeats should be able to pin the identity quickly).

Not investigated further here (per the same deferral rationale as the
IVFKnn corroboration above — this is `gc_barrier`/monitor/evacuation
territory the parked WIP fix attempt already found unsafe to patch
quickly). Filed as a third reusable, real-world, non-synthetic repro for
whoever picks up finding 1(b): `org.junit.runner.JUnitCore
org.elasticsearch.lucene.queries.InetAddressRandomBinaryDocValuesRangeQueryTests`
against the `server` module test classpath, `-Dtests.seed=B17AC9D3E1F2A0C4`,
`--Xmx 2g` — cheaper to iterate than MTChurn/BinaryTrees/WildFly/IVFKnn
(single small test class, ~30-40s/run, ~15% hit rate observed over 20 runs).
