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

## 2. G1/ZGC: STW hang risk when a JIT thread never polls (INT-3)
`stw_take_over_and_wait` falls back to a plain unbounded `wait_for_all()`
for non-Generational backends (`supports_jit_tlab_skip()` is
Generational-only). A compiled loop that neither allocates nor re-enters
the interpreter never arrives → whole-VM livelock under G1/ZGC + JIT.
Needs either xt-takeover extension to G1 (freeze + conservative scan +
pin) or back-edge safepoint polls. Scoping note (2026-07-10 third wave):
the cross-thread JIT-pin registry and the CSet pin filter are already
process-global on G1 (first wave), so the remaining blocker for the
xt-takeover route is the frozen-peer TLAB protocol — a frozen peer never
retires its TLAB (no gap sentinel), and every g1.rs region walker would
walk its uninitialized tail. G1 needs the equivalent of the Generational
"JIT TLAB skip regions" side channel, consumed by all region walkers,
plus load validation on the Linux probe host before it can be trusted.
The A5 unregistered-JIT-frame detector port to Linux LANDED (third wave).

## 3. Misc
- Class unloading machinery (`gc/src/class_unloading.rs`) has no driver;
  statics/mirrors/class-locks are immortal roots (INT-7).
- G1 weak/soft refs with dead OLD-region referents are cleared only when
  the region is evacuated — concurrent-mark cleanup never feeds reference
  processing (INT-8). CORRECTED ANALYSIS (third wave): the originally
  proposed fix — "run the ReferenceProcessor against the mark bitmap
  after remark" — is INERT as stated, because the mark bitmap is TAINTED
  for exactly the referents it would judge: (a) the concurrent marker's
  `scan_object_refs` has no Reference-class awareness and traces straight
  through the (restored) `referent` slot of every live Reference, and
  (b) each mid-cycle young pause's `weakref_null_referents_pre_gc` writes
  fire the SATB pre-barrier, recording every active referent as a
  mark-cycle root. A real fix needs all four of: (1) marker referent-slot
  hiding — a Reference-address skip set snapshotted at mark start and
  remapped across every evacuation pause; (2) SATB suppression on the
  protocol's null-pass writes (they are not semantic overwrites); (3) a
  `Reference.get()`/`refersTo` keep-alive barrier while marking is active
  (single hook family: `native_ref_get`/`native_soft_ref_get` — without
  it a mutator can `get()` a referent, store it into a black object, and
  remark clears the weak ref while a strong path exists → UAF, the exact
  race HotSpot's G1ReferenceGet intrinsic barrier exists for); (4)
  remark-time reference processing that resurrects `to_finalize` (mark +
  re-drain) BEFORE `cleanup` frees wholly-dead regions. Related spec gap
  in the same family: cleanup's in-place free reclaims dead FINALIZABLE
  objects without resurrection — the post-GC staleness guard then
  correctly skips them, so no UAF, but their `finalize()` silently never
  runs.
- G1 string-dedup table stores raw addresses, never remapped — API now
  carries a DO-NOT-WIRE-UP doc warning; the remap is still needed before
  `-XX:+UseStringDeduplication` can do anything (G1CORE-6).
- Parallel young evacuator residual race (documented in-code) — keep
  `CRATONVM_G1_PARALLEL_EVAC` off (G1CORE-5).
