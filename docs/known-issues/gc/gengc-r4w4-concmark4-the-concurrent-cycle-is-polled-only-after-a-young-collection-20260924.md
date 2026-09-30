# The concurrent old-gen cycle is only ever considered right after a young collection, so old-gen growth that no young collection accompanies never starts one early

> **STATUS (2026-09-29, gce ve2): OPEN -- CRATONVM_GEN_CONC_INLINE_START not decided: latched and taken 3/3, but concdrv_cycles_completed median 0 on both arms; needs a probe whose concurrent cycle completes.** Evidence: `docs/internal/gc-design-perf-round-20260929/ve2-verdicts.md`; next steps: `gce-handoff-gc-design-perf-round-close-20260929.md`.

> **STATUS (2026-09-29, gce e2/c): KEEP -- the inline start's counters now exist, so one row pair decides the flip.** `[GC] conc_cycles:` carries `conccyc_inline_start_latched=` and `conccyc_inline_start_taken=` (`ConcurrentGcState::{latch,take}_inline_start_request`). Rows (3 each, interleaved): `p inline_on_$r 600 "CRATONVM_DBG=gc-stats CRATONVM_GEN_CONC_INLINE_START=1" "-XX:+UseGenerationalGC -Xmx512m" GenR4W5DirectOldGrowthProbe` and `p inline_off_$r 600 "CRATONVM_DBG=gc-stats" "-XX:+UseGenerationalGC -Xmx512m" GenR4W5DirectOldGrowthProbe`. Both =HS. FLIP when every on-row shows `latched>=1 taken>=1`, `concdrv_cycles_completed` above the control's and `majcad_fallback_start_late` not above it; REJECT the switch if `taken=0` (the `maybe_gc` pickup is not reached on this probe).

> **STATUS (2026-09-29, gce e1/x): KEEP -- item 1 is opt-in and unjudged; the precedence pair this page's service-arm discussion leans on is now the DEFAULT.**
> - Flip: `CRATONVM_GEN_CONC_PRECEDENCE` and `CRATONVM_GEN_CONC_MARK_TAMS_STARTS` are default on since commit `3cd96a1a4` (evidence on `gengc-r4w3-oldgen3-...`).
> - Item 1: e1/c's collector half and the VM pickup (`gc_and_alloc.rs::maybe_gc`, `inline_start_requested`) are in the tree behind `CRATONVM_GEN_CONC_INLINE_START` (default off). `inline_start_1..3` (`verify-e1/ve1`, `GenR4W5DirectOldGrowthProbe -Xmx512m`, switch on) print HotSpot's line on e1, as on the base where the switch does not exist; the counters that judge it are not in the retained summaries.
> - **Remaining:** read `concdrv_cycles_completed` (>= 1) and `majcad_fallback_start_late` (below the flag-off run) from those rows' stderr, re-run on the flipped default, then decide the switch.

## STATUS (2026-09-29, gce e1/c): NARROWED -- item 1's collector half LANDED opt-in (`CRATONVM_GEN_CONC_INLINE_START`); the VM pickup is a cross-lane hunk

- **Collector half** (`gc/src/concurrent_mark.rs`, `gc/src/gen_heap.rs`): with NO service attached, `GenerationalHeap::poll_concurrent_start_after_direct_old_alloc` asks `ConcurrentGcState::inline_growth_poll_armed` (no service, nothing latched, idle, `used >=` the published start hint: four relaxed loads) and, under `CRATONVM_GEN_CONC_INLINE_START` and the concurrent-first policy, when `concurrent_start_due` says yes, latches `ServiceCells::inline_start` (`latch_inline_start_request`). `inline_start_requested` (one load) and `take_inline_start_request` (load, then swap) are the VM's side. Flag off: read at most once per direct old allocation above the hint, nothing latched, no decision changes. The flag is new: the orchestrator declares it (report request 3).
- **VM half** (cross-lane, `vm/src/runtime/interpreter/gc_and_alloc.rs::maybe_gc`, request 2 of `docs/internal/gc-design-perf-round-20260929/e1-c-report.md`): after the ZGC check, `if occupancy_poll && state.inline_start_requested() && state.take_inline_start_request() { maybe_concurrent_gc(shared, thread) }`. `maybe_gc` already runs collections, so the thread holds no unrooted result there; the driver re-asks the trigger.
- **Test:** `cargo test -j 5 -p cratonvm-gc --lib gce_e1c_inline_start_request_latches_once_and_is_taken_once`.
- **Run (after the hunk; NO service):** `CRATONVM_GEN_CONC_INLINE_START=1 CRATONVM_DBG=gc-stats cratonvm --java-home $JDK -XX:+UseGenerationalGC -Xmx512m -cp tools/bench GenR4W5DirectOldGrowthProbe`: stdout `direct-old-growth mib=80 keep=1 iters=48 checksum=2829651151571648512` (= HotSpot), `concdrv_cycles_completed>=1`, `majcad_fallback_start_late` below the flag-off run. The flip is then the orchestrator's.

## STATUS (2026-09-28, gcd d9/e): OPEN -- the service arm's zero cycles are EXPLAINED (not a service defect); and the d5/r claim that the default path polls direct old-gen growth was WRONG, so item 1 of this page is still open without the service; no code change on this page's path

- **d7 evidence** (gcd d8/x, `307f0c6a2`): `GenR4W5DirectOldGrowthProbe
  -Xmx512m` with the service =HS (`direct_old_growth_svc`); on
  `GenR4W4SteadyPromotionProbe 4` the service arm completed NO cycle
  (`og3_steady_svc_1..3`: `concpol_cycles_completed=0`, five pre-empting STW
  majors each).
- **Why the service arm completes no cycle:** the service runs the same
  driver under the same policy as the inline arm; nothing service-specific
  was found (re-read on `d84029110`: `ConcurrentGcState::{hand_off_to_service,
  service_wait, serve}`, `vm/src/runtime/interpreter/gen_conc_service.rs`
  `ServiceHooks`, the driver's re-ask, the back-off, which skips periodic
  wakes only). The difference is load: inline, the thread that runs the
  cycle stops allocating for its whole length; with the service all four
  mutators keep promoting while the cycle runs, so each cycle sees at least
  4/3 of the inline growth. Together with the two policy/marker causes on
  `gengc-r4w3-oldgen3-stw-major-and-concurrent-cycle-share-one-trigger-DONE-20260929.md`
  (STATUS 2026-09-28, gcd d9/e: the start's growth hysteresis re-opens every
  cycle at the midpoint to the floor whatever the prediction says, and every
  Phase-2 slice after a promotion re-walks the old generation), every
  service cycle crosses the 90 % ceiling or fails an allocation before its
  sweep ends. The fix is on that page, opt-in (`CRATONVM_GEN_CONC_PRECEDENCE`,
  `CRATONVM_GEN_CONC_MARK_TAMS_STARTS`), and its run includes the service arm
  with both flags; the new `[GC] conc_cycles:` line states per run which
  cause fired.
- **Correction to the d5/r STATUS below.** It says a direct old allocation
  polls the concurrent start "by default". It does not:
  `GenerationalHeap::poll_concurrent_start_after_direct_old_alloc` returns at
  its first test unless a service is attached
  (`ConcurrentGcState::service_growth_poll_armed` requires
  `service_attached()`). Without the service, old-gen growth with no young
  collection (humongous arrays, young spills) is still asked about only by
  the next collection a thread runs (the allocation-failure door) -- this
  page's item 1, still open on the default path. A poll that could run the
  cycle inline needs a registered mutator outside the old-gen lock; filed as
  `gcd-d9e-proposal-inline-start-request-from-direct-old-growth-REJECTED-20260929.md`
  (collector latch + a VM pickup at the allocation slow path).
- **What closes this page:** either the service's default flip (gate in the
  d5/r block below, now also requiring the oldgen3 page's `$B` service arm to
  complete cycles 3/3), or the inline start request above landing default-on
  with `GenR4W5DirectOldGrowthProbe -Xmx512m` (no service) printing
  `concdrv_cycles_completed>=1` and HotSpot's stdout. Owner: orchestrator.

## STATUS (2026-09-28, gcd d5/r): the page's premise is CLOSED on the default path; what is left is the service thread's default flip (a policy change), whose gate is below; no code change

- **The premise, re-read on d916d1c40.** Old-gen growth without a young
  collection now asks the concurrent start by default: a direct old
  allocation polls it (`GenerationalHeap::poll_concurrent_start_after_direct_old_alloc`,
  its growth signal), and every forced (allocation-failure) collection a
  thread RUNS asks `maybe_concurrent_gc_at(.., MarkDoor::AllocFail)`
  (`CRATONVM_GEN_CONC_ALLOC_FAIL_DOOR`, default on, `=0` restores the
  pre-r5w2 door). With the opt-in service thread
  (`CRATONVM_GEN_CONC_SERVICE_THREAD`) the periodic check and the growth
  signal run off the mutators too. So "polled only after a young
  collection" no longer describes the default; the page stays open only
  for the service's default flip.
- **d3/d4 changes that touch this area, checked:** the service's teardown
  wait (d1/c) is in; d4/j's `CRATONVM_GC_OVERHEAD_PROGRESS` second majors
  and d4/n's true-root seed run only on the allocation ladder's requested
  majors, which `GenR4W4SteadyPromotionProbe` never reaches (no OOM); expect
  `oldsz_true_root_majors=0` there. `gen_conc_service.rs`'s module doc said
  the stop "does not wait"; corrected by d5/r (doc only).
- **The gate for making the service the default** (unchanged in substance
  from the conc9 block below; one binary, arms interleaved, 3 rounds; stdout
  must equal the no-flag run and HotSpot where the probe defines it):
  ```
  S="CRATONVM_GEN_CONC_SERVICE_THREAD=1"
  P="--java-home $JDK -XX:+UseGenerationalGC -cp tools/bench"
  env $S CRATONVM_DBG=gc-stats cratonvm $P -Xmx512m GenR4W5DirectOldGrowthProbe
  for arm in "" "$S"; do env $arm CRATONVM_DBG=gc-stats cratonvm $P -Xmx512m GenR4W4SteadyPromotionProbe 4; done
  env $S CRATONVM_GEN_CONC_REMARK_REFPROC_HOOK=1 CRATONVM_DBG=gc-stats cratonvm $P -Xmx256m GenR5W5RemarkRefsProbe
  env $S CRATONVM_GEN_CONC_MARK_SLICE=64 CRATONVM_DBG=gc-stats cratonvm $P -Xmx512m GenR4W6ConcCadenceProbe
  for i in $(seq 20); do env $S cratonvm $P -Xmx512m GenR4W4SteadyPromotionProbe 1 >/dev/null || echo FAIL; done
  cargo test -j 5 -p cratonvm-vm --lib gen_conc_service
  cargo test -j 5 -p cratonvm-gc --lib concurrent_mark
  ```
  Expected: `GenR4W4SteadyPromotionProbe 4` prints
  `steady-promotion threads=4 ring=150000 iters=3000000 checksum=558000555608192`
  in both arms; the service arms show `concdrv_service_attached=true
  concdrv_service_cycles_completed>=1`; the 20-run teardown loop prints no
  `FAIL` and none takes over 3x its median; the RemarkRefs arm prints its
  HotSpot line (see the mark2 page); zero `WALK_DESYNC_HITS` /
  `SCAN_REGION_BREAK_HITS`. Plus the gc-stress gauntlet (MtChurn,
  HashMapOnly, BinT; 3 runs each with `$S`) and the netty
  `io.netty.buffer` x4 / Spring Boot sample on the Linux host, same pass
  counts as the no-flag runs.
- **Flip:** `gen_conc_service_thread` in `types/src/flags.rs` from
  `non_empty_non_zero` to `on_unless_zero` (`=0` is then its switch), the
  `types` test that asserts the default (`assert!(!d.gc.gen_conc_service_thread)`
  in `flags.rs`'s tests) inverted, and `gen_conc_service.rs::tests::generational_vm(false)`
  passing `Some("0")` instead of `None`. Retire this page on the flip, or as "premise
  closed" if the orchestrator decides the service stays opt-in. Owner:
  orchestrator.

## STATUS (2026-09-27, gcd d2/g, superseded above): soundness RE-VERIFIED against gcd d1/c and d2/g; teardown item CLOSED on this base; the gate runs below still decide the flip

- **Teardown (was "documented, not changed" below): FIXED on this base.**
  `Drop for Vm` now calls
  `gen_conc_service::stop_gen_concurrent_service_thread` (d1/c's cross-lane
  request, applied as `6a0483962`, `vm/src/vm/vm_init.rs`), which stops the
  service and waits for it inside a GC-blocked region; `serve` refuses to
  leave idle after a stop (`service_shutting_down`), and the driver abandons
  Phase 2 on a stopped service (`gen_conc_service_stopped_here`). Test:
  `cargo test -j 5 -p cratonvm-vm --lib gen_conc_service`.
- **d1/c's sweep reference-row drop on the service thread:** it takes the
  processor lock once per slice, outside the old-gen guard, from whichever
  thread drives the sweep. A mutator constructing a `Reference` on a block the
  slice just freed registers a row with a LATER sequence number, which the
  prune keeps. d2/g closed the one window that remained there: before the
  prune both rows share the address, and a rebuilt application-retirement
  index could hand the NEW reference's `clear()` / `enqueue()` to the dead
  row when the two were of different kinds (`reference.rs::rebuild_app_row_index`
  now prefers the later registration; test
  `gcd_d2g_the_later_registration_owns_a_shared_active_address`).
- **d1/c's STW young-instance → loader seed:** a stop-the-world collection's;
  the service thread runs none.
- **d2/g's STW retained-layout census** runs inside a stop-the-world pause on
  the collecting thread (`process_references_after_gc`), never on the service
  thread; its release takes the class manager's write lock there as the unload
  transaction already does.
- The gate below is unchanged. Owner: orchestrator (opt-in flip).

## STATUS (2026-09-27, gen r5w5/conc9, superseded above): default-safety audit of the service thread — three races/hangs FIXED, one cross-lane, two documented; NOT yet default (the gate runs below decide)

Every interaction of the service thread with a pause, the safepoint protocol
and thread exit was read (`vm/src/runtime/interpreter/gen_conc_service.rs`,
`ConcurrentGcState::{serve, service_wait, hand_off_to_service, shutdown_service}`,
the driver `maybe_concurrent_gc_at` / `gen_concurrent_remark_pause`, the
registry and barrier calls they make).

**Correct as it stands** (the 2026-09-26 re-read below holds): idle =
GC-blocked through `begin_blocking_region` (arrives at a pause already
requested, excluded by identity afterwards); leaving idle waits out a pause in
flight; running = a counted mutator that requests its own pauses through the
identity census and polls `safepoint_check` between slices; startup becomes
countable through `mark_stw_ready` exactly as a `Thread.start()` worker; the
exit is one barrier-serialized leave+`mark_dead`; the slot mutex is a leaf; no
lock is held across a wait; two drivers cannot overlap (`try_open_cycle`); the
service runs no Java (queue wake-ups, finalizers and cleaner actions are
recorded for the delivery thread); the JIT take-over never freezes it.

**Fixed (unbuilt):**
1. **A panic on the service thread wedged the VM.** Its `JoinHandle` was
   dropped at spawn, so a service that unwound (a panic in the driver outside
   a pause) stayed `alive` and running in the registry forever: every later
   pause counted it, the take-over found its OS thread gone (`Take::Gone`,
   excuses nobody) and every mutator stayed parked. The handle now goes to the
   registry (`set_join_handle`), so a finished thread is dead to every census
   (`entry_is_stw_live` → `note_finished_but_alive`). Same fix for the
   reference-delivery thread (`start_finalizer_thread`), which had the same
   shape (default path; only its unwind path changes).
2. **A panic INSIDE either concurrent pause left the world stopped** (inline
   driver and service alike): the initial-mark and remark pauses are
   open-coded, with no RAII release, so an unwind left `stw_requested` raised
   and the frozen peers suspended. New `GenConcPauseGuard` (armed before the
   take-over): on an unwind it resumes the peers (or clears the skip spans if
   the take-over itself unwound) and completes the pause with an empty map,
   as `NonMovingPause`'s `Drop` does; the normal path releases it and runs its
   own resume/release unchanged (the `every_non_collection_pause_has_a_pause_line`
   order check still sees `retire_skip_spans_and_resume(shared, taken)`).
3. **A hand-off after `shutdown_service` was dropped.** The service still reads
   as attached until its loop returns, but `service_wait` answers `Shutdown`
   before it looks at `pending`, and the detach clears it; `Drop for Vm` runs
   pending finalizers (which allocate) in that window.
   `hand_off_to_service` now refuses once `shutdown` is set, so the caller runs
   the cycle itself. Test: `cargo test -p cratonvm-gc --lib r5w5_a_stopping_service_refuses_hand_offs`.

**Cross-lane (old9, `gen_heap.rs::shrink_cycle_due`):** the periodic shrink
verdict is one-shot (a `true` restarts its clock), and the service path asks
the verdict TWICE (the mutator's ask that hands off, then the driver's re-ask
on the service; or the periodic hook's ask, then the re-ask), so under
`CRATONVM_GC_OLD_SHRINK_AFTER_CONCURRENT` with a service attached no periodic
shrink cycle ever opens. Filed with the diff, since fixed (`749095499`):
`docs/internal/gc/gengc-r5w5-conc9-the-periodic-shrink-verdict-is-consumed-by-the-first-ask-FIXED-20260927.md`.

**Documented, not changed:** teardown (`shutdown_service` does not wait; a
service mid-cycle keeps requesting pauses through the rest of `Drop for Vm`,
and may drop the last `Arc<SharedVm>`), filed as
`../../internal/gc/gengc-r5w5-conc9-the-service-thread-can-outlive-vm-teardown-FIXED-20260928.md`;
and a behaviour note for the flip: with the service, the finalizers and
cleaner actions a remark queues wait for the delivery thread or the next door
drain instead of the initiating mutator's inline drain.

**The gate before making the service the default** (each arm: one binary,
interleaved; stdout must equal the no-flag run, and HotSpot where the probe
defines it):
```
S="CRATONVM_GEN_CONC_SERVICE_THREAD=1"
# 1. the page's own probe and the steady-promotion cadence
env $S CRATONVM_DBG=gc-stats cratonvm --java-home "$JDK" -XX:+UseGenerationalGC -Xmx512m -cp tools/bench GenR4W5DirectOldGrowthProbe
for arm in "" "$S"; do env $arm CRATONVM_DBG=gc-stats cratonvm --java-home "$JDK" -XX:+UseGenerationalGC -Xmx512m -cp tools/bench GenR4W4SteadyPromotionProbe 4; done
# 2. cycles on the service thread with the hook, class unloading and finalization
env $S CRATONVM_GEN_CONC_REMARK_REFPROC_HOOK=1 CRATONVM_DBG=gc-stats cratonvm --java-home "$JDK" -XX:+UseGenerationalGC -Xmx256m -cp tools/bench GenR5W5RemarkRefsProbe
env $S CRATONVM_GEN_CONC_CLASS_UNLOAD=1 CRATONVM_GEN_CONC_REMARK_REFPROC_HOOK=1 CRATONVM_GC_CONC_START_PERCENT=40 CRATONVM_DBG=gc-stats cratonvm --java-home "$JDK" -XX:+UseGenerationalGC -Xmx256m -cp tools/bench GenR5W5ConcUnloadProbe
# 3. a pause requested while the service is mid-slice, many mutators, JIT peers
env $S CRATONVM_GEN_CONC_MARK_SLICE=64 CRATONVM_DBG=gc-stats cratonvm --java-home "$JDK" -XX:+UseGenerationalGC -Xmx512m -cp tools/bench GenR4W6ConcCadenceProbe
# 4. gc-stress gauntlet with the flag (MtChurn, HashMapOnly, BinT), 3 runs each
# 5. netty io.netty.buffer x4 and the Spring Boot sample (exploded): startup, one request, clean exit
# 6. teardown: 20 back-to-back short runs of GenR4W4SteadyPromotionProbe 1 with the flag (no hang at exit)
cargo test -p cratonvm-vm --lib gen_conc_service
cargo test -p cratonvm-gc --lib concurrent_mark
```
Expected: every stdout equals its no-flag run; the service arms show
`concdrv_service_attached=true concdrv_service_cycles_completed>=1`; no hang
(any run over 3x its no-flag wall time is a failure); zero
`WALK_DESYNC_HITS`/`SCAN_REGION_BREAK_HITS`. Only then flip
`gen_conc_service_thread` (defaults lane, `off_word Some("0")`).

## STATUS (2026-09-26, gen r5w1/refs5): service PASSES its probe (orchestrator); code re-read, no bug; NOT yet safe as the default; the page's premise was narrower than the real gap

- **Orchestrator, dev `9e252c8b2`:** `CRATONVM_GEN_CONC_SERVICE_THREAD=1
  CRATONVM_DBG=gc-stats ... -Xmx512m GenR4W5DirectOldGrowthProbe` prints
  `service_attached=true`, `growth_signals=25`, `service_cycles_completed=15`,
  `concpol_stw_preempted=3`, `minor=26 major=25`, with stdout equal to HotSpot's.
  This page's status-line criteria are met.
- **The real gap is wider than "polled only after a young collection".** Without
  the service, a JIT-compiled program opens NO concurrent cycle at all: its young
  collections come through the allocation-failure door, which never asked the
  trigger (orchestrator: `GenR4W4SteadyPromotionProbe`, `concdrv_cycles_started=0`,
  `major=7`). Filed as
  `../../internal/gc/gengc-r5w1-refs5-concurrent-cycle-never-starts-from-compiled-code-FIXED-20260927.md`.
  With the service attached, that door now HANDS the due cycle to it (r5w1,
  `maybe_gc_forced_collected`), in addition to the periodic check and the growth
  signals.
- **Code re-read** (`vm/src/runtime/interpreter/gen_conc_service.rs`,
  `ConcurrentGcState::serve` and the hand-off):
  - The idle service holds only a `Weak`, and it is GC-blocked, so no pause waits
    for it.
  - Its `cycle_due` takes the old-gen and policy locks while blocked. A pause
    that needs the old-gen lock waits only for that critical section, which waits
    on nothing.
  - The running service is a counted mutator that arrives between slices.
  - It is never frozen by the takeover (it is not in JIT code).
  - Exit is the barrier-serialized blocked-region leave that the AIO dispatcher
    also uses.
  - `Drop for Vm` does not wait; the thread is a daemon.

  No defect found.
- **Default: not yet.** No correctness evidence exists beyond one probe:
  - the gauntlet under `gc-stress` with the flag on (MtChurn, HashMapOnly, BinT)
    has never been run;
  - neither has the page's cadence A/B (`GenR4W6ConcCadenceProbe`,
    `GenR4W4SteadyPromotionProbe 1` and `4`, flag on and off, interleaved,
    medians of 5).

  Flipping also makes concurrent cycles run in JIT-heavy programs by default for
  the first time, which exposes the concurrent marker to workloads it has never
  seen. Recommended order: run the gauntlet with the flag, then flip the service
  (it keeps the cycle off the allocating thread), and leave
  `CRATONVM_GEN_CONC_ALLOC_FAIL_DOOR` as the no-service fallback.
- **Probe for the flip** (extends the status line):

  ```
  for arm in "" "CRATONVM_GEN_CONC_SERVICE_THREAD=1"; do
    env $arm CRATONVM_DBG=gc-stats cratonvm --java-home "$JDK" -XX:+UseGenerationalGC \
        -Xmx512m -cp tools/bench GenR4W4SteadyPromotionProbe 2>&1 \
      | grep -E 'steady-promotion|conc_driver:|generational:|major_cadence:'
  done
  ```

  - **Service arm:** `concdrv_cycles_completed >= 1`, `major=` lower than the
    default arm's, and `majcad_fallback_start_late` lower too.
  - **Both arms:** stdout equal to `java -XX:+UseSerialGC`.

*Filed 2026-09-24 by gen round 4 wave 4, lane `concmark4`. Status: **FIX LANDED,
awaiting probe** (opt-in `CRATONVM_GEN_CONC_SERVICE_THREAD`: the collector half in gen
round 4 wave 5, lane `concmark5`; the VM half, the thread that runs
`ConcurrentGcState::serve`, in wave 6, lane `concsvc6`, see the last section). Probe
`CRATONVM_GEN_CONC_SERVICE_THREAD=1 CRATONVM_DBG=gc-stats cratonvm
--java-home "$JDK" -XX:+UseGenerationalGC -Xmx512m -cp tools/bench
GenR4W5DirectOldGrowthProbe` → `[GC] conc_driver: concdrv_service_enabled=true
concdrv_service_attached=true ... concdrv_growth_signals=G ...
concdrv_service_cycles_completed=S` with G >= 1 and S >= 1, and stdout
`direct-old-growth mib=80 keep=1 iters=48 checksum=2829651151571648512`, which is what
`java -XX:+UseSerialGC -Xmx512m -cp tools/bench GenR4W5DirectOldGrowthProbe` prints.
See the last section.*
*Severity: **perf** (a STW old-gen collection where a concurrent cycle was due). No
correctness claim.*

## Code location

- `vm/src/runtime/interpreter/gc_and_alloc.rs` — the two callers of the generational
  concurrent-cycle driver `maybe_concurrent_gc_at`: `maybe_gc`, only after
  `run_collection_pause(.., GcDoor::AllocationThreshold)` returns `Some` (this thread
  initiated a young collection), and the `System.gc()` door (`MarkDoor::SystemGc`).
- `gc/src/gen_heap.rs::concurrent_cycle_due` — the concurrent-first policy's start trigger
  (gen r4w4/concmark4), which is therefore asked at those two points only.

## What is wrong

Under the concurrent-first policy the cycle is meant to open when the old generation
crosses its initiating occupancy `T` (45 % of the generation before one cycle is measured,
adaptive after). But nothing looks at `T` except the epilogue of a young collection this
thread initiated. Old-gen growth that does not come with young collections is invisible
to it:

1. **Direct old-gen allocation.** Humongous arrays and young-spill allocations go straight
   into the old generation. A workload made of them (the `ZipContentTests` shape
   `vm_exec.rs`'s spill gate was written for) runs few or no young collections, so the
   generation climbs from below `T` to the 75 % STW floor without a concurrent cycle ever
   being considered. The spill-pressure gates (`vm_exec.rs`, `jit/helpers.rs`) ask
   `old_gen_needs_gc` — deliberately the STW trigger, not `T` — so the first collection is
   the STW one: exactly the legacy behaviour.
2. **A losing initiator.** When two threads race for the young pause, only the winner runs
   the epilogue; with the policy's growth hysteresis a missed consideration can defer the
   cycle by a whole young period. Minor.

## Why not fixed this wave

The natural poll sites are the old-gen allocation slow path (`try_alloc_*_old`,
`gen_heap.rs` allocation functions) and `vm_exec.rs`'s spill gate — neither in this lane's
files — and a cycle must be driven by a registered mutator that can request a pause, which
an allocation path deep in the heap is not. Widening `old_gen_needs_gc` to "`T` reached"
would make the spill gates force a young collection on every spill above `T` while a cycle
runs, which is worse.

## Proposed fix (M)

1. **A dedicated marker thread** (the wave-2 `concmark` review's proposal): a VM-registered
   daemon `JvmThread` that polls safepoints, wakes when `concurrent_cycle_due()` turns true
   (a condvar signalled from the old-gen allocation slow path when `used` crosses `T`, one
   comparison per slow-path allocation), and runs `maybe_concurrent_gc_at` itself. This also
   takes Phase 2 off the allocating thread, whose own allocation stalls for the whole cycle
   today.
2. **Cheaper first step:** in `vm_exec.rs`'s spill gate, after `maybe_gc_forced_pub_at`
   returns, call `maybe_concurrent_gc` (it returns at once unless `concurrent_cycle_due`),
   so a spill-driven forced collection gets the same epilogue an allocation-triggered one
   does.

## How to verify

A probe that allocates `new byte[1 << 20]` arrays into a retained ring (straight to the old
generation) with no small allocations, at `-Xmx256m`, `CRATONVM_DBG=gc-stats`: today
`[GC] conc_policy: concpol_cycles_completed=0` and `[GC] generational: major=M` with `M >= 1`;
after the fix `concpol_cycles_completed >= 1` and `M` near 0. Program output must match
HotSpot.

---

## 2026-09-24 round 4 wave 5 (lane `concmark5`): a service thread drives the cycle. Collector half landed (opt-in); VM half requested

**Unbuilt when written** (the lane could not run cargo or the VM).

### What the code actually does today (a correction to the framing)

Once a driver opens a cycle, `maybe_concurrent_gc_at` runs ALL of it on that thread:
the initial-mark pause, Phase 2 in slices (with a safepoint poll between slices), the
remark pause (retried up to 4 times), and the sweep in slices. So phases do not stall
waiting for the next young collection. Two things are wrong instead:

1. **The start is polled only from a young-collection epilogue** (and the
   `System.gc()` door). Old-gen growth without young collections (humongous arrays,
   young-spill objects) is never considered below the STW floor. This is this page's
   item 1.
2. **The cycle runs on the mutator that found it due.** Its own allocation stalls for
   all of Phase 2 and the sweep. A single-threaded program pays for the whole cycle
   inline, and there is no concurrency at all.

### The design (HotSpot's `G1ConcurrentMarkThread` shape)

A dedicated, VM-registered service thread owns the cycle:

* **Idle** in the GC-blocked region, so it never holds a pause up, parked on a per-VM
  condvar (`ConcurrentGcState::service_wait`).
* **Woken by** one of three things:
  * a **hand-off**: with a service attached, `GenerationalHeap::concurrent_cycle_due`
    on any other thread posts a request and answers `false`, so the mutator's epilogue
    returns to the program at once;
  * a **growth signal**: a direct old-gen allocation that crossed the start threshold.
    The new poll site `GenerationalHeap::poll_concurrent_start_after_direct_old_alloc`
    sits in `try_alloc_array_humongous`, `try_alloc_object_old` and
    `try_alloc_objects_old_batch`, under the old-gen lock they already hold. It costs
    one relaxed load with no service, and three relaxed loads below the published
    threshold (`ConcurrentGcState::service_growth_poll_armed`). Only then does it take
    the policy-lock check;
  * the **period** (`GEN_CONC_SERVICE_PERIOD_MS`, 20 ms). On a periodic wake the
    service asks the trigger itself, which covers growth through any path with no poll
    site. It also covers the losing-initiator case in item 2.
* **Runs the cycle** as a counted mutator through the VM's existing driver. That driver
  requests the initial-mark and remark pauses itself and polls safepoints between
  slices, so the phases advance on the service's schedule. Mutators only meet the
  pauses.
* **Fallbacks are unchanged.** `major_trigger_decision` still runs the STW collection
  through an open cycle in three cases: on a request, on an old-gen allocation
  failure, and at 90 %. A cycle that fails (lost remark, stale epoch) is abandoned
  exactly as before. The service then backs its periodic re-check off: after `n`
  consecutive cycles that opened and failed it skips `2^n − 1` periodic wakes, capped
  at 63 (`service_backoff_skips`). A requested wake is never skipped.
* **Fail-safe.** The attachment is an RAII guard. If the service exits or unwinds, it
  detaches, and mutators run due cycles inline again, exactly as with no service.

### What landed (collector side, `gc/`)

* `concurrent_mark.rs`:
  * `ConcurrentServiceHooks` (VM callbacks `enter_idle`, `leave_idle`, `cycle_due`,
    `run_cycle`);
  * `ConcurrentGcState::{serve, attach_service_thread, hand_off_to_service,
    service_growth_poll_armed, signal_old_gen_growth, service_wait, shutdown_service,
    service_enabled, driver_census_line}`;
  * `ConcurrentServiceWake` and `service_backoff_skips`;
  * a published start-threshold hint.

  All the state is per VM, on the heap's shared `ConcurrentGcState`, with no new
  process global.
* `gen_heap.rs`:
  * `concurrent_cycle_due` hands off: a verdict split into `concurrent_cycle_due_verdict`;
  * the poll site above, at the three direct old-gen allocation paths;
  * `concurrent_driver_census_line`.
* `vm_heap.rs`:
  * `VmHeap::{wants_concurrent_service_thread, concurrent_cycle_due}`;
  * a shutdown line: `[GC] conc_driver: concdrv_service_enabled=… concdrv_service_attached=…
    concdrv_handoffs=… concdrv_growth_signals=… concdrv_wakes_requested=…
    concdrv_wakes_periodic=… concdrv_periodic_due=… concdrv_backoff_skips=…
    concdrv_service_attempts=… concdrv_service_cycles_completed=…
    concdrv_remark_refproc_hook_calls=…`.
* Flag `CRATONVM_GEN_CONC_SERVICE_THREAD` (`CRATONVM_GC=gen-conc-service-thread`),
  opt-in. Unset, no service is attached, and every new path is inert: the hand-off
  answers "no service", and the poll stops at its first load.

### The VM half (cross-lane request: owner of `gc_and_alloc.rs` / `native/jni.rs` / VM start-up)

About 40 lines, modelled on `jni.rs::aio_dispatcher_main`, which is already a
foreign-attached daemon VM thread idling in the GC-blocked region:

```rust
// after the heap and thread registry exist (where the other VM service threads start):
if shared.mem.heap.wants_concurrent_service_thread() {
    let shared = Arc::clone(&shared);
    std::thread::Builder::new()
        .name("Craton Gen Concurrent Mark".into())
        .spawn(move || gen_concurrent_service_main(shared))?;
}

fn gen_concurrent_service_main(shared: Arc<SharedVm>) {
    // aio_dispatcher_main's prologue: attach as a foreign DAEMON thread, settle
    // into the GC-blocked region, set the JNI context/thread TLS.
    struct Hooks<'a> { shared: &'a SharedVm, running: Option<ForeignCallGuard> }
    impl ConcurrentServiceHooks for Hooks<'_> {
        fn enter_idle(&mut self) { self.running = None; }        // drop -> blocked region
        fn leave_idle(&mut self) { self.running = Some(ForeignCallGuard::enter()); }
        fn cycle_due(&mut self) -> bool { self.shared.mem.heap.concurrent_cycle_due() }
        fn run_cycle(&mut self) {
            let _ = with_jni_context(|shared, thread| {
                maybe_concurrent_gc_at(shared, thread, MarkDoor::MaybeGc)
            });
        }
    }
    let mut hooks = Hooks { shared: &shared, running: None };
    shared.mem.concurrent_gc_state.serve(
        Duration::from_millis(cratonvm_gc::concurrent_mark::GEN_CONC_SERVICE_PERIOD_MS),
        &mut hooks,
    );
    // aio_dispatcher_main's epilogue: mark dead, detach (the thread is idle here).
}
// VM teardown: shared.mem.concurrent_gc_state.shutdown_service();
```

Notes for the implementer:

* `ForeignCallGuard` is private to `jni.rs`: make it `pub(crate)`, or put the loop there.
* `maybe_concurrent_gc_at` needs no change. On the service thread its
  `concurrent_cycle_due()` returns the verdict, never a hand-off.
* A dedicated `MarkDoor::ServiceThread` (in `g1.rs`, G1 owner) would separate the service
  in the door census. It is optional: `MaybeGc` is accurate enough.
* The thread must be a DAEMON: it must not keep the VM alive, and `shutdown_service` must
  run before the registry is torn down.

### Tests (unrun)

* `concurrent_mark.rs`:
  * `the_service_back_off_is_exponential_and_capped`;
  * `a_due_cycle_is_handed_to_an_attached_service_and_never_to_itself` (two real threads);
  * `a_growth_signal_wakes_the_service_and_the_pre_check_gates_it`;
  * `the_service_loop_runs_backs_off_and_shuts_down` (scripted hooks: idle/run
    alternation, the back-off skip, shutdown returns idle and detached, one service per
    VM).
* `gen_heap.rs`:
  * `a_due_concurrent_cycle_is_handed_to_the_service_thread`;
  * `a_direct_old_gen_allocation_past_the_start_wakes_the_service` (humongous array,
    spilled object and batch: inert with no service, gated below the start, one pending
    request at a time).

### Probes

`tools/bench/GenR4W5DirectOldGrowthProbe.java` (the status line). It allocates only
80 MiB `long[]` arrays (humongous at `-Xmx512m`) into a ring of one. HotSpot G1 shows the
reference shape: `Pause Young (Concurrent Start) (G1 Humongous Allocation)`, cycles
started by humongous allocation alone.

The default arm (no service) must print `concdrv_growth_signals=0` and the same stdout.
With the service, the mutator-stall half is also visible on
`GenR4W4SteadyPromotionProbe 1` (single-threaded) as follows:
* the `door=gen-initial-mark` / `door=gen-remark` pause lines are unchanged;
* the program's own wall time no longer includes Phase 2;
* A/B the total run time, interleaved, medians of 5.

### Retire when

The VM half has landed and the status-line probe passes on both arms, with stdout
matching HotSpot. Then decide the default: the service is opt-in until the A/B on
`GenR4W4SteadyPromotionProbe` (1 and 4 threads) and `OldGenRsetProbe 19 700 16` shows no
regression.

---

## 2026-09-24 round 4 wave 6 (lane `concsvc6`): the VM half landed (opt-in). FIX LANDED, awaiting probe

**Unbuilt when written** (the lane could not run cargo or the VM).

### What landed

* `vm/src/runtime/interpreter/gen_conc_service.rs` (new, declared in
  `runtime/interpreter.rs`):
  * `start_gen_concurrent_service_thread(shared)`, called by `Vm::new` right after the
    main thread is registered. It does nothing unless
    `VmHeap::wants_concurrent_service_thread()` (Generational and the flag), or when the
    `SharedVm` has no owning `Arc` (unit fixtures).
  * The thread, `Craton Gen Concurrent Mark`, is modelled on the reference-delivery
    thread (`finalizer_thread_main`), not on `aio_dispatcher_main`: the AIO dispatcher
    reaches its VM through `process_vm()`, a process global, and the service must be per
    VM. It registers STARTING and daemon, becomes STW-visible itself
    (`run_if_no_stw_requested` + `mark_stw_ready`), then runs
    `ConcurrentGcState::serve(GEN_CONC_SERVICE_PERIOD_MS, hooks)`.
  * The four hooks:
    * `enter_idle`: `NativeContextImpl::begin_blocking_region` (TLAB retired, empty root
      snapshot deposited, arrives at a pause already requested). It then drops the
      strong `Arc<SharedVm>`, so an idle service holds only a `Weak`.
    * `leave_idle`: upgrade, then `end_blocking_region`, which waits out a pause in
      flight.
    * `cycle_due`: `heap.concurrent_cycle_due()`. On the service thread this is the
      verdict, never a hand-off.
    * `run_cycle`: `maybe_concurrent_gc_at(.., MarkDoor::MaybeGc)`, unchanged. It
      requests the initial-mark and remark pauses with this thread as initiator and
      polls `safepoint_check` between slices.
  * Exit (after `serve` returns, or when it refused a second service): idle, then ONE
    `mark_blocked_region_leave_after` step that retires the TLAB, withdraws the TLAB
    address and marks the thread dead, as the AIO dispatcher and `thread_start` workers
    do.
* `vm/src/vm/vm_init.rs`:
  * `Vm::new` starts the service;
  * `Drop for Vm` calls `shutdown_service()` before the pending finalizers. It does not
    wait for the thread: the dropping thread is a counted mutator that does not poll,
    and the service may be requesting a pause.

No process global was added: the registration and wake state are on the heap's
`ConcurrentGcState`, the identity is in the VM's thread registry, and the VM handle is a
`Weak`.

### Probe (status line)

```
javac -d tools/bench tools/bench/GenR4W5DirectOldGrowthProbe.java
java -XX:+UseSerialGC -Xmx512m -cp tools/bench GenR4W5DirectOldGrowthProbe
for arm in "" "CRATONVM_GEN_CONC_SERVICE_THREAD=1"; do
  env $arm CRATONVM_DBG=gc-stats cratonvm --java-home "$JDK" -XX:+UseGenerationalGC \
      -Xmx512m -cp tools/bench GenR4W5DirectOldGrowthProbe 2>&1 \
    | grep -E 'direct-old-growth|conc_driver:|major_cadence:'
done
```

Expected:

* stdout on every VM: `direct-old-growth mib=80 keep=1 iters=48
  checksum=2829651151571648512` (computed from the program: 48 iterations, each adding
  `163840·1000003·i + 64·163840·163839/2`, with the running checksum multiplied by 31 and
  wrapped at 64 bits);
* flag arm: `concdrv_service_enabled=true concdrv_service_attached=true`,
  `concdrv_growth_signals >= 1`, `concdrv_service_cycles_completed >= 1`;
* default arm: `concdrv_service_enabled=false concdrv_service_attached=false
  concdrv_growth_signals=0`.

The cadence A/B is `tools/bench/GenR4W6ConcCadenceProbe.java` (see
`docs/internal/reviews/gengc-round4-w6-concsvc6-20260924.md`).

### Tests (unrun)

* `runtime::interpreter::gen_conc_service::tests::the_service_thread_starts_with_a_generational_vm_and_stops_with_it`:
  attaches, refuses a second service (whose thread exits and is marked dead), and
  detaches after `Drop for Vm`.
* `runtime::interpreter::gen_conc_service::tests::no_service_thread_without_the_flag`.

### Retire when

The status-line probe passes on both arms with stdout matching HotSpot, and the two
tests pass. The default flip is a separate decision. It needs
`GenR4W6ConcCadenceProbe`, `GenR4W4SteadyPromotionProbe 1` and `… 4`, flag on vs off,
interleaved, with:

* `concdrv_cycles_completed` and `majcad_fallback_conc_too_slow` no worse;
* the wall time no worse;
* the gauntlet (MtChurn, HashMapOnly, BinT) passing under `gc-stress` with the flag on.

Until then the service stays opt-in.
