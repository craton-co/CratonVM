# Proposal: the allocation that found the concurrent cycle due should not wait for the whole cycle

> **STATUS (2026-09-28, gc defects round, proposal triage d8/y): KEEP (rank 8
> of 54).** Not built; the design (three edits) is on the page. It now also
> carries
> `../../internal/gc/gengc-r5w1-refs5-proposal-concurrent-start-from-the-young-pause-REJECTED-20260928.md`
> (section at the end): one owed-cycle mechanism. Step 2 (service thread as
> the default) has d7 evidence: every row with
> `CRATONVM_GEN_CONC_SERVICE_THREAD=1` prints HotSpot's lines
> (cjg_service_1..10, og3_steady_svc_1..3, og3_rset_svc_1..3, r5frag_svc_1..3,
> w1_steady_promo_svc, direct_old_growth_svc, w1_remark_refproc_on_1..3); its
> flip gate is in triage.md. Step 1 needs the per-thread JIT poll word that
> `common-a-proposal-per-thread-handshakes.md` also needs. **Gate:** the
> page's `new byte[64]` max-latency micro-probe falls from about one
> concurrent cycle to about one young pause; `[GC] conc_doors:` shows the owed
> cycles opened from the poll; `GenR4W4SteadyPromotionProbe` stdout unchanged.
> **Size:** M (step 1, with the JIT poll word), XS (step 2, a flag flip).

## STATUS (2026-09-26, gen r5w4/conc8): NOT IMPLEMENTED — it does not fit in conc8's files; the exact design for the owners of `jvm_thread.rs` and `jit/src/**`

**Why no code.** The owed bit is per THREAD, so it has to live in `JvmThread`
(`vm/src/threading/jvm_thread.rs`, lane jit8 this wave). Its only useful
consumer is the compiled poll's slow path (`jit/src/**`,
`vm/src/jit/helpers.rs`, also jit8). What conc8 owns (the driver in
`gc_and_alloc.rs`) could at most move the cycle from one allocation-failure
door visit to the next. That only shifts the tail by one collection, which
the unload7 status below already rejected. A per-VM "owed" word would not
help either: whichever thread reads it next is again inside an allocation.

**The design, as three edits:**

1. **`JvmThread`** (jit8): `pub conc_cycle_owed: bool` (default `false`), and
   `pub poll_word: AtomicU8`, the per-thread poll arm, laid out at a fixed
   offset the JIT can address from the thread pointer it already holds for
   TLAB allocation. Both are per thread, so no global is added.
2. **The door** (`gc_and_alloc.rs::maybe_gc_forced_collected`, conc8's file;
   ~6 lines behind a new opt-in `CRATONVM_GEN_CONC_OWED_POLL`). Where it now
   calls `maybe_concurrent_gc_at(shared, thread, MarkDoor::AllocFail)`:
   - when no service is attached and the flag is set, ask the trigger ONLY
     (`h.concurrent_cycle_due_verdict()` has to become `pub`; it is in
     `gen_heap.rs`, unowned);
   - if due, set `thread.conc_cycle_owed = true`, store 1 to
     `thread.poll_word` (Release), count
     `note_door(AllocFail, Due)`, and return without running the cycle.

   The allocation is retried at once.
3. **The consumers:**
   - `jit_safepoint_slow_path_body` (helpers.rs): after its
     `safepoint_check`, if `thread.conc_cycle_owed`, clear it and
     `poll_word`, then call
     `maybe_concurrent_gc_at(shared, thread, MarkDoor::JitDriver)`. The poll
     is at a back-edge or a method entry, which is not inside an allocation
     helper. The frame state is the one any safepoint publishes, so the
     cycle's pauses scan this thread exactly as they would scan a parked one.
   - Every compiled poll site (jit/src): test
     `stw_flag | thread.poll_word` instead of the STW flag alone. That is one
     extra load and OR per poll, which is the only per-poll cost.
   - The interpreter's `maybe_gc` entry: consume the bit the same way.
   - A new door variant, `GenConcDoor::OwedPoll`, which replaces `Other` for
     `MarkDoor::JitDriver`, so that `[GC] conc_doors:` shows cycles moving from
     `alloc_fail_opened` to `owed_poll_opened`.
   - A stale bit is harmless: the driver returns on `phase() != Idle`, and the
     trigger re-asks.

**Risk the owner must check.** `safepoint_check` is called from 24 other sites,
and the cycle driver itself calls it between slices. So the consumer must live
in the poll slow path, never in `safepoint_check`. If it lived in
`safepoint_check`, the driver's own Phase-2 poll would re-enter the driver
(`phase() != Idle` returns at once, but the owed bit would be consumed without
a cycle).

**Verify.** `GenR4W4SteadyPromotionProbe -Xmx512m` gives the same stdout.
`[GC] conc_doors:` shows `concdoor_alloc_fail_opened` falling to about 0 and
the new `owed_poll_opened` taking its count. `concdrv_cycles_completed` stays
within ±10 %. An allocation-latency micro-probe (the max time of
`new byte[64]` in a compiled loop at steady promotion) falls from about one
cycle to about one young pause.

## STATUS (2026-09-26, gen r5w3/unload7, superseded above): NARROWED, no code — step 1 as written has no consumption point in compiled code; what it needs is named

Read against the code this wave (unload7 owns `gc_and_alloc.rs`):

- **Step 1's two consumption points do not both exist for compiled code.**
  `maybe_gc` is reached only by interpreted allocation. The compiled poll's
  slow path (`jit::helpers::jit_safepoint_slow_path` → `safepoint_check`) runs
  only when the poll's flag is set, and that flag is the VM's STW flag
  (`vm_for_stw_flag_addr`): it is set only while some thread requests a pause.
  So a compiled loop that owes a cycle reaches no non-allocation point that
  would run it until the NEXT allocation failure — and running it there makes
  THAT allocation wait for the whole cycle. The tail moves by one collection;
  it does not shrink. Recording the owed bit in `safepoint_check` generally is
  also unsafe as it stands: the driver itself calls `safepoint_check` between
  slices, and 24 other sites do from contexts that were never reviewed for
  running a whole cycle.
- **What step 1 needs** (cross-lane, not this lane's files): a PER-THREAD poll
  arm. `JvmThread` gains `conc_cycle_owed: bool` (per VM, not a global); the
  alloc-fail door sets it and arms a per-thread poll word that compiled polls
  test alongside the STW flag (`jit/src/**`, lane live7), so the next
  back-edge / method-entry poll of THAT thread takes the slow path and
  `jit_safepoint_slow_path_body` calls `maybe_concurrent_gc_at(.., AllocFail)`
  after `safepoint_check`, outside any allocation helper. The interpreter's
  `maybe_gc` entry consumes the same bit. A thread that never polls again
  (it blocks) leaves the bit to the next door, which is harmless: the driver's
  `phase() != Idle` return and the trigger's own "not due any more" make a
  stale bit a no-op.
- **Step 2 (service thread default)** needs the gauntlet the page names; not
  run here. The per-door census that page's verification needs landed this
  wave (`[GC] conc_doors:`, `../../internal/gc/gengc-r5w2-conc6-proposal-generational-start-census-by-door-IMPLEMENTED-20260927.md`):
  `concdoor_alloc_fail_opened` counts the cycles an allocating thread ran
  inline, which is exactly the latency tail this page is about.

Verify (for whoever lands the per-thread arm): `GenR4W4SteadyPromotionProbe`
stdout unchanged; `[GC] conc_doors:` shows the owed cycles opened from the poll
(a new `GenConcDoor` variant) instead of `alloc_fail`, and
`concdrv_cycles_completed` unchanged.

*Filed 2026-09-26 by gen round 5 wave 2, lane `conc6`. Proposal; latency. Not a
work item.*

## Problem

Since gen r5w2/conc6, `CRATONVM_GEN_CONC_ALLOC_FAIL_DOOR` is on by default. With
no service thread, a due concurrent cycle runs inline, start to finish, on the
thread whose allocation just failed. That covers the initial-mark pause, the
sliced Phase 2, the remark, and the sliced sweep. The failing allocation retries
only after the sweep ends.

`maybe_gc`'s epilogue has always done the same for interpreted allocation. So
this is not new, but it is now the common case: compiled code funnels every
young collection through the allocation-failure door. A thread's allocation
latency then gets a tail the length of one whole concurrent cycle, on one
allocation in every few hundred collections. HotSpot's G1 and ZGC pay nothing
there: their concurrent threads run the cycle.

## Proposal

Two steps, the second only if the first is not enough.

1. **Retry first, cycle second.**
   - The door sets a per-THREAD "cycle owed" bit (a `JvmThread` field, not a
     process or VM global) instead of running the cycle.
   - The bit is consumed at this thread's next point that may run a cycle and
     is NOT inside an allocation helper: the next `maybe_gc` entry, and the
     slow path of a compiled back-edge / method-entry safepoint poll.
   - The caller's allocation is served first. The start is delayed by at most
     one poll interval, which the start policy's growth buffer already absorbs.
   - The winner of a pause on any door still evaluates the trigger once; only
     where the cycle runs changes.
2. **Make the service thread the default**
   (`CRATONVM_GEN_CONC_SERVICE_THREAD`), after the gc-stress gauntlet has run
   with it. The inline door then becomes the no-service fallback. Two things
   to settle first:
   - whether the service thread shows up in Java-visible thread counts
     (`ThreadMXBean`, `Thread.getAllStackTraces`), which would be a
     `--compatible` change;
   - the service's back-off after failed cycles.

## Cost and risk

- Step 1 adds one byte per thread and one load per poll slow path.
- The bit must be cleared when the cycle it asked for has run on another
  thread. The driver's `phase() != Idle` early return, and the trigger's own
  "not due any more", already make a stale bit a no-op.

## Verification

- `GenR4W4SteadyPromotionProbe` under `--verbose:gc`: the gap between an
  `AllocationFailure` pause line and the next `door=gen-initial-mark` line
  exists (step 1), and the `concdrv_cycles_completed` count is unchanged from
  the inline default.
- An allocation-latency microbenchmark: the maximum time one `new byte[64]`
  takes in a compiled loop at a steady old-gen promotion rate. It must fall
  from about one cycle to about one young pause.

## Merged from `gengc-r5w1-refs5-proposal-concurrent-start-from-the-young-pause` (d8/y, 2026-09-28): the pause-side view of the owed cycle

Retired as a duplicate of this page; its triggering defect (compiled code never
started a cycle) is fixed. Its design is the per-VM half of the same
mechanism: the young collection's epilogue evaluates
`concurrent_cycle_due_verdict` once per young pause, under the old-gen lock it
already holds, and either posts the hand-off to the service thread or sets a
per-VM "cycle owed" latch on `ConcurrentGcState` (an `AtomicBool`) that every
door consumes with one relaxed load. That makes the trigger independent of
which thread initiated the pause (the losing initiator of a pause race never
asks today). Design it together with this page's per-thread bit (who RUNS
the owed cycle) and with the `start_owed` bit of
`gengc-r5w4-conc8-proposal-concurrent-start-buffer-covers-one-growth-step-20260926.md`
(humongous growth between asks). Verify: `concdrv_start_asked` equals the
young pause count; a unit test that a young collection above the start sets
the latch and a door consumes it once.
