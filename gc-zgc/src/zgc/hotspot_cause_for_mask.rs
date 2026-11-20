#[allow(unused_imports)]
use super::*;


// ---------------------------------------------------------------------------
// The simulation is gone (2026-09-02)
// ---------------------------------------------------------------------------
//
// `ColoredPointer`, `LoadBarrier`, `ZPage`, `ZgcHeap`, `ZgcCollector` and
// `GenerationalZgc` used to live here: ~1,250 lines of a metadata-only
// SIMULATION of OpenJDK ZGC's model, with synthetic `u64` addresses, no
// backing storage, a `&mut self` "barrier" that recoloured a value rather
// than healing a slot, and a `concurrent_relocate` that copied no bytes.
// Selecting it emitted a one-time warning saying as much.
//
// The module docs called it "the scaffolding for that future work", and that
// was true when it was written and stopped being true as the real modules
// landed. Every concept in it now has an honest implementation beside it:
//
//     ColoredPointer   ->  zgc::vaddr::ZColoredWord
//     LoadBarrier      ->  zgc::barrier
//     ZPage            ->  zgc::page::ZPageReal
//     ZgcHeap          ->  ZgcRealHeap (below)
//     GenerationalZgc  ->  zgc::generation + ZgcRealHeap's Phase G
//
// So it was 1,250 lines of a production file, plus a scary warning path, that
// nothing outside its own tests referenced -- every mention elsewhere in the
// tree was already prose in a doc comment. `git log` has it if a future
// reader wants the shape it modelled.
// ---------------------------------------------------------------------------
// ZgcRealHeap — a real, functioning ZGC-flavoured collector
// ---------------------------------------------------------------------------
//
// Everything above this point is the colored-pointer / load-barrier / phase
// *simulation* (no backing storage). `ZgcRealHeap` is the real thing: owned
// memory, real object headers, a working mark-sweep collector, and the full
// `GarbageCollector` trait. It is intentionally self-contained and does not
// touch the simulation types.

/// Default total heap size for a [`ZgcRealHeap`]: 64 MB.
pub const ZGC_REAL_DEFAULT_HEAP: usize = 64 * 1024 * 1024;

/// Trigger a collection once live+dead allocation crosses this fraction of
/// total capacity.
pub const ZGC_REAL_GC_THRESHOLD_PERCENT: usize = 75;

/// The bits of [`ZgcCounters::trigger_seen`] — one per reason `needs_gc`
/// can answer yes for, charged at most once per collection by
/// [`ZgcRealHeap::note_trigger`], so that "which clause woke this cycle" is
/// answerable from one mask.
///
/// *Corrected 2026-09-21 (wave 4, lane P).* This header used to claim "the
/// nursery clause has one too". It does not, and never did — the G2d clause
/// returns `true` off `gen_nursery_triggered` without touching this mask, and
/// `rg 'TRIGGER_BIT_'` finds four constants, not five. A comment that names a
/// bit which does not exist is the drift this round exists to remove; the list
/// below is now exactly the mask.
pub const TRIGGER_BIT_STRESS: usize = 1 << 0;
pub const TRIGGER_BIT_THRESHOLD: usize = 1 << 1;
pub const TRIGGER_BIT_HEADROOM: usize = 1 << 2;
pub const TRIGGER_BIT_ALLOC_BUDGET: usize = 1 << 3;
/// Not a reason a collection happened — a reason one did **not**.
///
/// Charged when `headroom_low` is true and the `gc_rearm` floor refuses it
/// anyway: the collector declining to run for a reason expressed in live bytes
/// while the binding constraint was allocatable space. It shares the mask so it
/// is charged once per
/// inter-collection interval rather than once per poll, and because the mask is
/// cleared at the end of every collection it re-arms on exactly the terms the
/// four reasons above do. See [`ZgcCounters::trigger_headroom_starved`].
pub const TRIGGER_BIT_HEADROOM_BLOCKED: usize = 1 << 4;
/// The bypass arm was the sole admitting clause (bit 5), or was available and
/// declined by `last_cycle_reclaimed_nothing` (bit 6). Two bits rather than one
/// because "the flag did nothing" has two causes and an unattributable zero is
/// not evidence. See [`ZgcCounters::trigger_headroom_bypass`].
pub const TRIGGER_BIT_HEADROOM_BYPASS: usize = 1 << 5;
pub const TRIGGER_BIT_HEADROOM_BYPASS_SUPPRESSED: usize = 1 << 6;
/// The warm-up clause ([`ZGC_WARMUP_CYCLES`]) admitted this collection.
pub const TRIGGER_BIT_WARMUP: usize = 1 << 7;
/// zr z1/tr: the HotSpot-shaped rules of [`director`] admitted this
/// collection (`CRATONVM_ZGC_RATE_TRIGGER=1` only). One bit per rule, so the
/// `-Xlog:gc` / notification cause can name it ([`hotspot_cause_for_mask`]).
pub const TRIGGER_BIT_RATE: usize = 1 << 8;
pub const TRIGGER_BIT_PROACTIVE: usize = 1 << 9;
pub const TRIGGER_BIT_HIGH_USAGE: usize = 1 << 10;
/// zr z1/tr: the G2d nursery clause admitted this collection. It had no bit
/// (the header above records why that was once claimed and was not true);
/// it has one now so the cause of a minor cycle is attributable.
pub const TRIGGER_BIT_NURSERY: usize = 1 << 11;
/// zr z6/tp: not a reason -- the near-full re-arm floor
/// ([`director::rearm_floor`]) held back an occupancy collection the legacy
/// floor would have admitted. Charged once per interval, like
/// [`TRIGGER_BIT_HEADROOM_BLOCKED`]; no cause maps to it.
pub const TRIGGER_BIT_PACE_DEFERRED: usize = 1 << 12;
/// zr zs s1/tp: not a reason -- a director rule said yes and the anti-storm
/// floor (`gc_rearm`) held it back (`CRATONVM_ZGC_DIRECTOR_REARM_FLOOR`,
/// deciding arm only). Charged once per interval; no cause maps to it.
pub const TRIGGER_BIT_DIRECTOR_DEFERRED: usize = 1 << 13;
/// zt t7/tg: the director tick's `Timer` rule (`-XX:ZCollectionInterval`)
/// asked for this collection (`ZgcRealHeap::request_tick_cycle`).
pub const TRIGGER_BIT_TIMER: usize = 1 << 14;

/// zr z1/tr: HotSpot 25 ZGC's `GCCause` for a collection `needs_gc` asked
/// for, from the cycle's trigger mask -- the first admitting clause in
/// `needs_gc`'s own order wins. `None` for an empty mask (the caller's door
/// decides: `System.gc()`, an allocation that failed, ...).
///
/// HotSpot's ZGC never prints `Allocation Failure` (that is the Serial / G1
/// young-pause cause); its director's rules are `Warmup`, `Allocation Rate`,
/// `Proactive`, `High Usage` and `Timer`, and a stalled allocation is
/// `Allocation Stall`. This VM's live-bytes threshold, headroom, pause-target
/// budget, stress and nursery clauses have no HotSpot twin; each is an
/// "the allocator is getting ahead of the collector" rule, which is what
/// `Allocation Rate` names.
pub fn hotspot_cause_for_mask(mask: usize) -> Option<&'static str> {
    // zt t7/tg: `Timer` before `Warmup`, as HotSpot 25's
    // `make_major_gc_decision` asks `rule_major_timer` first.
    const ORDER: [(usize, &str); 10] = [
        (TRIGGER_BIT_STRESS, "Allocation Rate"),
        (TRIGGER_BIT_TIMER, "Timer"),
        (TRIGGER_BIT_WARMUP, "Warmup"),
        (TRIGGER_BIT_RATE, "Allocation Rate"),
        (TRIGGER_BIT_PROACTIVE, "Proactive"),
        (TRIGGER_BIT_HIGH_USAGE, "High Usage"),
        (TRIGGER_BIT_THRESHOLD, "Allocation Rate"),
        (TRIGGER_BIT_HEADROOM, "Allocation Rate"),
        (TRIGGER_BIT_ALLOC_BUDGET, "Allocation Rate"),
        (TRIGGER_BIT_NURSERY, "Allocation Rate"),
    ];
    ORDER
        .iter()
        .find(|(bit, _)| mask & bit != 0)
        .map(|(_, cause)| *cause)
}

/// zr z6/tp: the `[GC] zgc-pacing:` kind of a collection from its trigger
/// mask -- the threshold clause first (it is the one the near-full floor
/// paces), then headroom, warm-up, budget, the director rules, the rest;
/// `Unasked` when no reason bit is set (allocation failure, `System.gc()`).
pub fn pace_kind_for_mask(mask: usize) -> director::PaceKind {
    use director::PaceKind;
    if mask & TRIGGER_BIT_THRESHOLD != 0 {
        PaceKind::Threshold
    } else if mask & TRIGGER_BIT_HEADROOM != 0 {
        PaceKind::Headroom
    } else if mask & TRIGGER_BIT_WARMUP != 0 {
        PaceKind::Warmup
    } else if mask & TRIGGER_BIT_ALLOC_BUDGET != 0 {
        PaceKind::AllocBudget
    } else if mask & (TRIGGER_BIT_RATE | TRIGGER_BIT_PROACTIVE | TRIGGER_BIT_HIGH_USAGE) != 0 {
        PaceKind::Director
    } else if mask & (TRIGGER_BIT_NURSERY | TRIGGER_BIT_STRESS | TRIGGER_BIT_TIMER) != 0 {
        // zt t7/tg: a `Timer` cycle is paced by the clock, not by bytes.
        PaceKind::Other
    } else {
        PaceKind::Unasked
    }
}







/// `CRATONVM_ZGC_RATE_TRIGGER=1` -- stage 2 of
/// `proposal-e-allocation-rate-driven-trigger`: [`director`]'s HotSpot-shaped
/// rules decide, and `needs_gc`'s 75%-of-`-Xmx` live-bytes clause stands down
/// (every other clause, including the headroom backstop and the hard
/// allocation refusal, is unchanged). default on: it changes how often the
/// default collector runs. Read once per heap.
pub fn zgc_rate_trigger_enabled() -> bool {
    true
}

/// HotSpot ZGC's `rule_warmup`: until this many warm-up cycles have run,
/// collect once occupancy reaches `(warm-up cycles + 1) * 10%` of the soft
/// maximum -- 10, 20 and 30% (zr zs s1/tp: warm-up CYCLES, as HotSpot counts
/// them; it counted every collection, `System.gc()` included, until
/// `CRATONVM_ZGC_WARMUP_HOTSPOT_STEPS`) (r16 zgcarray, proposal ZA2;
/// `docs/feature-designs/zgc-architecture.md`).
///
/// Without it the first collection waited for the live-bytes threshold (75%
/// of `-Xmx`): an allocation-heavy program streamed ~3.2 GB of never-touched
/// memory at the ergonomic 4 GiB heap -- every page committed and
/// demand-zero faulted on its first write, `Object[4096]` at 8.8-11.5 us
/// before the first cycle against 2.2-3 us after it on Windows -- and then
/// paid one 760 ms pause over all of it, with nothing measured yet for the
/// pause-target loop to size its budget from. The warm-up cycles are each
/// over a tenth of the heap, run while the footprint is still small, and hand
/// the budget loop its first evidence early.
/// `CRATONVM_ZGC_WARMUP=0` is the kill switch.
pub const ZGC_WARMUP_CYCLES: usize = 3;



/// `CRATONVM_ZGC_NEAR_FULL_PROGRESS=<percent>` -- zr z6/tp, default
/// [`director::REARM_NEAR_FULL_DEFAULT_PERCENT`] (75), `=0` kill. While the
/// post-collection live set is at or above the live-bytes threshold, the
/// occupancy clauses of `needs_gc` wait for `max(1 MiB, percent% of the free
/// space)` of new allocation instead of `max(free / 4, 64 KiB)`; see
/// [`director::rearm_floor`] for the arithmetic and the argument. Read once
/// per heap (a construction seed).
pub fn zgc_near_full_progress_percent() -> usize {
    let v = None::<String>;
    director::near_full_progress_percent_from(v.as_deref())
}

/// `CRATONVM_ZGC_EXHAUSTED_FREE_FLOOR=<KiB>|on` -- zs s6/no, default on. The
/// exhausted verdict's floor in bytes ([`director::exhausted_free_floor_from`],
/// `on` = HotSpot's 2 MiB page); see [`ZgcRealHeap::arm_exhausted_verdict`].
/// Read once per heap.
pub fn zgc_exhausted_free_floor_bytes() -> usize {
    let v = None::<String>;
    director::exhausted_free_floor_from(v.as_deref())
}

/// zs s9/no2 (rv6 X1): the source of [`ZgcRealHeap`]'s exhausted-verdict key,
/// one serial per heap. An identity counter, not compatibility state: it
/// makes no VM answer differ, it only keeps two heaps' keys apart.
pub static ZGC_HEAP_SERIAL: AtomicUsize = AtomicUsize::new(1);

/// zt t20/x16 (`zgc-zt13x10-review-residuals-CLOSED-20261019.md` item 23e): the
/// production heaps ([`ZgcRealHeap::new_shared_with_initial`], the ones with a
/// `self_weak`) alive in this process, and the arena bases of those
/// `CRATONVM_ZGC_LOAD_BARRIER` armed. zt t18/zb1's arming raises the
/// PROCESS-WIDE codegen gate and clears the process-wide JIT read bounds, so it
/// is only sound to read its census and its CPU as one VM's when the process
/// has one VM: the opt-in arms only a single-heap process
/// ([`zgc_load_barrier_arming_refused`]). Identity census, not compatibility
/// state (it changes no Java-visible answer).
pub static ZGC_SHARED_HEAPS_LIVE: AtomicUsize = AtomicUsize::new(0);
pub static ZGC_LOAD_BARRIER_ARMED_BASES: std::sync::Mutex<Vec<usize>> =
    std::sync::Mutex::new(Vec::new());

/// zt t20/x16: does `CRATONVM_ZGC_LOAD_BARRIER` refuse to arm a heap built
/// while `live` production heaps (this one included) and `armed` armed ones
/// are alive? Yes whenever another heap exists. Pure.
pub fn zgc_load_barrier_arming_refused(live: usize, armed: usize) -> bool {
    live > 1 || armed > 0
}


thread_local! {
    /// zs s6/no: the exhausted verdict this thread holds, as
    /// `(heap identity, gc_count when armed)`; `(0, 0)` = none. Thread-local
    /// because the verdict belongs to the ladder that refused on this thread:
    /// another thread's allocations, and every infallible VM-internal
    /// allocation, never read it. It lapses with the next collection on the
    /// heap (`gc_count` moves), is lifted when the ladder's
    /// `OutOfMemoryError` is built ([`ZgcRealHeap::lift_exhausted_verdict`]),
    /// and is keyed by heap so a thread that moves between two VMs in one
    /// process cannot carry it across. Per-thread runtime state, not
    /// compatibility state.
    pub static ZGC_EXHAUSTED_VERDICT: std::cell::Cell<(usize, usize)> =
        const { std::cell::Cell::new((0, 0)) };
}







/// `CRATONVM_ZGC_SOFT_MAX_PACES` -- zt t1/tp, **default ON** since zt t20/fl1
/// (`=0` kill). With
/// `-XX:SoftMaxHeapSize` below the capacity, `needs_gc`'s live-bytes clause
/// also fires at [`ZGC_REAL_GC_THRESHOLD_PERCENT`] of the SOFT maximum
/// ([`ZgcRealHeap::pacing_threshold`]), not only at 75% of `-Xmx`.
///
/// HotSpot's ZGC reads the soft maximum in every director rule (warm-up,
/// allocation rate, high usage, proactive): `-Xmx1g -XX:SoftMaxHeapSize=256m`
/// keeps the heap near 256 MiB and lets it grow past only when the live set
/// needs it. Without the switch (`=0`) this VM read it in the warm-up clause
/// alone (the director, which reads it too, is opt-in), so after the three
/// warm-up cycles the same flags collected at 768 MiB. The re-arm floor is a
/// quarter of the soft headroom below the soft maximum (zt t6/tc), so a cycle
/// still needs real progress and a live set above the soft threshold cannot
/// storm. No effect without `-XX:SoftMaxHeapSize`, or with it at or above the
/// capacity. Flip evidence (t16 verdict, pr2 T10): `pr2_sm_P2_S` SAME
/// `peak=near` in both modes while the `=0` arm DIFFs `peak=far`; inert with
/// no soft maximum (`collections=25` on both arms, CPU 1.004x); the engaged
/// `gc_count` 84 is under 1.5x HotSpot's 98. Read once per heap.
pub fn zgc_soft_max_paces_enabled() -> bool {
    true
}

/// `CRATONVM_ZGC_SOFT_MAX_ERGONOMIC_PACING` -- zt t29/sm (opt-in): the
/// launcher's ergonomic soft maximum (`CRATONVM_ZGC_SOFT_MAX_ERGONOMIC`, 90 %
/// of an ergonomic `MaxHeapSize`) also paces the director, as an explicit
/// `-XX:SoftMaxHeapSize` does. Off, it is the reported value only (the
/// `gc,init` / `gc,heap` `Soft Max Capacity:` lines and the JMX row): the m8
/// rows `rt2_o8_tail_D` / `_H` (`Zs6NoExhaustedTail soft`, no `-Xmx`) ran
/// ~2500 back-to-back `Allocation Rate` collections above the 90 % line
/// (16x the CPU, past the 600 s limit), because this VM's collections stop
/// the mutator where HotSpot's run beside it.
pub fn zgc_soft_max_ergonomic_pacing_enabled() -> bool {
    false
}







/// zt t14/tg6: does a cycle with this trigger mask restart the proactive
/// rule's major clock? Not when HotSpot 25's director would have run it as a
/// MINOR: an allocation stall (`stall`, the collecting thread's
/// `gc_quiescence::allocation_stall_door` mark; HotSpot logs `Minor
/// Collection (Allocation Stall)`) or a cycle whose cause is `Allocation
/// Rate` / `High Usage` (`rule_minor_allocation_rate`,
/// `rule_minor_high_usage`). Every other cause (`Warmup`, `Proactive`,
/// `Timer`, an empty mask: `System.gc()` and the other doors) does. Pure.
pub fn cycle_restarts_major_clock(mask: usize, stall: bool) -> bool {
    !stall
        && !matches!(
            hotspot_cause_for_mask(mask),
            Some("Allocation Rate" | "High Usage")
        )
}



/// `CRATONVM_ZGC_DIRECTOR_BYTE_POLLED_INTERVAL` (default ON, `=0` kills): the
/// HotSpot-shaped rate rule of a concurrent heap reserves the poll latency in
/// bytes, not HotSpot's 100 ms director tick, for its reaction time
/// ([`director::DirectorInputs::byte_polled_interval`]). Read once per heap.
pub fn zgc_director_byte_polled_interval_enabled() -> bool {
    true
}

/// Collect once the arena can no longer serve a request this large, however
/// few LIVE bytes there are — see [`ZgcRealHeap::headroom_low`].
///
/// The margin has to cover the largest request a workload makes between two
/// native-call boundaries, because a native cannot collect where it stands: the
/// boundary hook in `vm/src/vm/vm_exec.rs` is the first place a collection can
/// run on its behalf. `capacity/128` with an 8 MiB floor is 16 MiB at
/// `-Xmx 2g` — under 1% of the heap, against a failure mode that costs the
/// whole run.
pub fn zgc_headroom_margin(capacity: usize) -> usize {
    (capacity / 128).max(8 * 1024 * 1024)
}

/// Maximum array length, mirroring `heap.rs` / HotSpot's practical limit.
///
/// **This is ABOVE [`ZgcRealHeap::MAX_PLAUSIBLE_ARRAY_LEN`] (`1 << 28`), the
/// corruption screen `alloc_size` applies.** Every array this constant admits
/// between the two is legal to allocate and unsizable to the collector; see that
/// constant's doc for what each consumer does about it, and for why the marker
/// must not be one of them.
pub const ZGC_REAL_MAX_ARRAY_LENGTH: usize = i32::MAX as usize;

// zr z3/lt: the count of registered objects the sweep refused to size, and
// the one-shot latch of its warning, used to be the process statics
// `ZGC_UNSIZABLE_OBJECTS` / `ZGC_UNSIZABLE_WARNED`, so a second VM's
// `[GC] zgc-integrity:` line counted the first VM's refusals. Per heap now:
// `heap_state::ZHeapLocalState::unsizable_objects`, read through
// [`ZgcRealHeap::unsizable_registered_objects`].

/// Teardown counterpart to the `publish_jit_read_bounds` call in
/// [`ZgcRealHeap::with_capacity`], mirroring `G1Collector`'s.
///
/// Without it the read table would keep naming an arena whose backing
/// allocation has been freed, and a guarded inline `getfield` would happily do
/// a raw load into it. Unconditional, matching that precedent: if another live
/// heap owns the table it re-publishes at its own construction, and until then
/// helper-only is a safe, merely slower, state.
impl Drop for ZgcRealHeap {
    fn drop(&mut self) {
        // zr z4/un: stop this heap's uncommitter (it holds only a `Weak`, so
        // it would also stop at its next wake-up on its own). The same for the
        // pre-fault worker (jr21 gc).
        self.stop_uncommitter();
        self.stop_prefaulter();
        // zt t20/x16 (page item 23e): uncount a production heap (one with a
        // `self_weak`, which only `new_shared_with_initial` sets) and its
        // `CRATONVM_ZGC_LOAD_BARRIER` arming. The process-wide gate an armed
        // heap raised stays raised (the arming's own rule: no disarm outside
        // a safepoint).
        if self.self_weak.get().is_some() {
            ZGC_SHARED_HEAPS_LIVE.fetch_sub(1, Ordering::AcqRel);
            ZGC_LOAD_BARRIER_ARMED_BASES
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .retain(|&base| base != self.arena_base);
        }
        // OWNER-CHECKED, both of them. These tables are PROCESS-GLOBAL and this
        // heap may not be their publisher, so a short-lived heap -- a sizing
        // probe, an init-time heap replaced once `-Xmx` is parsed, or the next
        // unit test over in another thread -- must not wipe the bounds a
        // different, still-live heap published.
        //
        // `GenerationalHeap::drop` reached that conclusion for
        // `JIT_REGION_BOUNDS` after measuring the consequence: an empty table
        // makes the frame-band verifier answer "nothing unpublished" for every
        // frame, VACUOUSLY, and 3 of 3 collections on a default-collector run
        // reported coverage incomplete because the table was empty for the
        // whole run. This `Drop` cleared the read table unconditionally, which
        // is the same defect one collector over; it is owner-checked now, and
        // the movable table is owner-checked from the start.
        //
        // The discriminator is this heap's own arena base, which is what both
        // publishes wrote into slot 0.
        cratonvm_gc_core::jit_bounds::clear_jit_read_bounds_owned_by(self.arena_base);
        cratonvm_gc_core::jit_bounds::clear_movable_bounds_owned_by(self.arena_base);
        cratonvm_gc_core::jit_bounds::clear_movable_commit_screens_owned_by(self.arena_base);
        // zt t20/zb3: the colored-slot decode base, owner-checked the same way
        // (a no-op unless this heap armed colored stores).
        cratonvm_types::narrow_oop::retract_colored_slot_base(self.arena_base as u64);
        // zt t22/zb5: and the color flip's compiled bad-mask cell, which is
        // freed with the counters (owner-checked; a no-op unless this heap
        // armed the flip).
        cratonvm_types::narrow_oop::retract_colored_bad_mask_cell(
            self.counters.lb_bad_cell.word_address(),
        );
        // Same owner check for the JIT's inline start-bit table: its `words`
        // is this heap's bitmap, which is freed with this heap.
        self.withdraw_jit_inline_announce();
        // Compiled bodies and their gate block have the same VM lifetime.
        // Dropping this heap never mutates another heap's barrier plan.
        // THE CENSUS SUMMARY, at the one end-of-run point this file owns.
        //
        // `ZSlotCensus` produced numbers nobody printed: `format_summary` had
        // no caller anywhere in `gc/src/`, so even an operator who set
        // CRATONVM_ZGC_CENSUS got a walk whose result was dropped. The
        // collector's other end-of-run lines are written by
        // `VmHeap::print_gc_summary` in `vm_heap.rs`, which is a different
        // file; teardown of the heap itself is the equivalent moment here and
        // is guaranteed to run.
        //
        // Gated on `is_enabled`, so a default run -- and the many short-lived
        // probe heaps this file's own tests build -- print nothing at all.
        //
        // zr z1/tr: NOT printed from here any more. This destructor does not
        // run in the shipped VM (the paragraph below), so the census summary
        // had a caller that never executed -- and it went through `tracing`,
        // which a shutdown may already have torn down. It is printed by
        // `emit_end_of_run_metrics` (called below, and from `vm-cli`'s
        // shutdown arms), with `eprintln!`, once per process.
        // THE METRICS SUMMARY AND THE TSV ROW.
        //
        // **This call site is a BACKSTOP, not the one that fires in a real
        // run.** `impl Drop for ZgcRealHeap` does not execute in the shipped
        // VM: the heap lives in an `Arc` the process holds until exit, and
        // `vm-cli` leaves through paths that do not unwind it. Measured
        // 2026-09-21 -- a run with `CRATONVM_ZGC_METRICS=1` printed every
        // `[GC] zgc-*` line from `VmHeap::print_gc_summary` and NOT the metrics
        // summary, and the TSV file was never created.
        //
        // That is the same defect the comment above this one congratulates
        // itself on fixing: `ZSlotCensus::format_summary` "had no caller
        // anywhere in `gc/src/`", and the repair gave it a caller in a
        // destructor that does not run. The instrument was still unreachable;
        // only the reason changed.
        //
        // So the real emission is `VmHeap::emit_zgc_end_of_run_metrics`, called
        // from `vm-cli`'s two shutdown arms and UNGATED by `--verbose:gc`,
        // because a harness that wants the machine-readable row has no reason
        // to want 1975 logs' worth of per-cycle stderr with it.
        // `emit_end_of_run_metrics` claims once per heap (zr z3/lt; it was once
        // per process), so having both call sites cannot produce two rows for
        // one VM -- and a test that really does drop a heap still gets its
        // summary. A heap that never collected (a sizing probe, an init-time
        // heap replaced once `-Xmx` is parsed) has no row to write and, with a
        // per-heap claim, would otherwise write an empty one.
        if !heap_state::zgc_metrics_row_per_heap() || self.gc_count() > 0 {
            self.emit_end_of_run_metrics();
        }
        // THE HEADROOM COUNTERFACTUAL, and it is behind NO FLAG at all.
        //
        // Everything else printed from this `Drop` is gated, and correctly so.
        // This one is not, for a reason specific to what it measures: the run
        // it is about is the run with logging off. `starved_intervals` counts
        // intervals in which the arena went short of contiguous space and the
        // live-bytes `gc_rearm` floor refused a collection anyway — the state
        // `CRATONVM_ZGC_HEADROOM_BYPASSES_REARM` exists to break — and the
        // whole point of the counter is to answer "does this ever happen on a
        // real workload?" without first setting the flag whose value it is
        // supposed to decide. A reading available only under `--verbose:gc` or
        // `CRATONVM_GC_STATS` cannot answer that, because nobody runs a suite
        // that way by accident.
        //
        // **Silent when the count is zero**, which is the common case and,
        // on the evidence so far, very likely the only case. A default run's
        // stderr is untouched unless the condition actually occurred; when it
        // did, one line is the whole record. That asymmetry is deliberate: a
        // gauge printed as `0` on every run is noise a reader learns to skip,
        // while a line that appears only when something happened is read.
        //
        // See `gap-p-headroom-flip-criteria.md`, and note that the orchestrator
        // could NOT reproduce the abort this condition was supposed to lead to
        // (§0 of that page): every exhaustion shape it constructed threw a
        // catchable `OutOfMemoryError` at near-full occupancy, on the
        // non-compacting arm too. So this counter is now the discriminator for
        // whether the predicate gap has any observable consequence at all,
        // rather than a step on the way to a flip that was assumed to be due.
        let (starved, gap, fired, suppressed) = self.headroom_flip_evidence();
        if starved != 0 {
            eprintln!(
                "[GC] zgc-headroom: starved_intervals={starved} \
                 starved_max_gap_bytes={gap} bypass_fired={fired} \
                 bypass_suppressed={suppressed} collections={} \
                 (the headroom clause was refused by the gc_rearm floor; see \
                 gap-p-headroom-flip-criteria.md)",
                self.gc_count.load(Ordering::Relaxed),
            );
        }
    }
}

// ---------------------------------------------------------------------------
// Object-start membership
// ---------------------------------------------------------------------------
//
// `ZObjectStartBits`, `ZObjectStarts` and `ZObjectStartsSnapshot` moved to
// `zgc/starts.rs` on 2026-09-02 -- one cohesive structure with a narrow
// interface, ~1,150 lines of a file that had outgrown review. See that
// module's header for the measurement the bitmap exists for and for the
// three-leg exactness argument.

/// A real, memory-backed ZGC heap.
///
/// # Storage scheme
///
/// Objects live in a single bump-pointer [`Arena`] (the same allocator the
/// semi-space `Heap` and the non-moving young-gen sweep use). Each allocation
/// is laid out exactly like every other CratonVM heap:
///
/// ```text
/// [ObjectHeader (HEADER_SIZE)] [field0 (SLOT_SIZE)] ... [fieldN]
/// [ObjectHeader (HEADER_SIZE)] [elem0] [elem1] ...        (arrays, compact)
/// ```
///
/// so `ObjectRef`s handed out by [`Self::alloc_object`] / [`Self::alloc_array`]
/// are real `*mut u8` pointers into owned memory and are fully interoperable
/// with the shared header/slot decoders in `heap.rs`.
///
/// A side **object registry** (`Vec` of base addresses) records every live
/// allocation so the sweep can enumerate the heap without having to parse
/// arena holes. Allocation appends; the sweep rebuilds it from the survivors.
///
/// # Collection algorithm
///
/// [`Self::collect_garbage`] is a **stop-the-world, non-moving mark-sweep**:
///
/// 1. **Mark.** Clear every object's mark bit, then trace transitively from
///    `roots`: for each reachable object, set [`GC_FLAG_MARKED`] in its
///    header and push its reference-typed fields / reference array elements
///    onto a work stack (a real graph trace, not a page-level approximation).
/// 2. **Sweep.** Walk the registry; objects without the mark bit are dead —
///    their bytes are zeroed and handed back to the arena free list for
///    reuse. Survivors keep their address (non-moving) and have their mark
///    bit cleared for the next cycle.
///
/// Because it is non-moving, no `ObjectRef` ever changes — the returned
/// [`GcResult::pointer_map`] is therefore empty (no remapping needed), which
/// is exactly correct for a non-compacting collector.
/// Does this process publish ZGC's arena envelope into `JIT_READ_BOUNDS`?
///
/// Default **ON**; opt out with `CRATONVM_ZGC_NO_JIT_READ_BOUNDS=1`.
///
/// # Why this is sound, when two design docs say ZGC must not publish
///
/// `gen_heap::JIT_READ_BOUNDS`'s own doc comment and
/// `jit/src/x64/licm.rs::zgc_codegen_honours_read_barrier` both record the
/// reason ZGC was left out when the read table landed (2026-08-18): a compact
/// reference slot under ZGC was said to hold `Z_COLORED_TAG | colour | offset`
/// rather than a pointer, so admitting an inline reference load would hand
/// compiled code an un-barriered colored word.
///
/// **That premise is not true of this tree.** It describes the relocating ZGC
/// `feature-designs/zgc-jit-load-barrier.md` designs, not the one that runs.
/// `feature-designs/zgc-reference-slot-representation.md` opens with the
/// measured position — *"Reference slots are plain pointers; nothing in the
/// heap stores a colored word"* — and the code agrees: the only writer of the
/// barrier state, [`ZgcRealHeap::set_barrier_color`], has **no non-test
/// caller**, so `cratonvm_types::zgc_read_barrier_armed()` is false for the
/// whole life of every real process and no slot is ever coloured.
///
/// So the residual this unblocks — 56.9M helper calls per `SHA256Digest`
/// kernel run, 100% `outside-published-bounds`, 100% reference reads — was
/// being paid to protect against a representation that does not exist yet.
///
/// # What keeps it sound when the premise DOES change
///
/// Three mechanisms, in the order they engage:
///
/// 1. **Arming clears the table.** [`ZgcRealHeap::set_barrier_color`] zeroes
///    `JIT_READ_BOUNDS` before it publishes the armed state, and re-publishes
///    on disarm. The emitted containment sequence loads the table words at
///    RUNTIME (`jit/src/x64/objects.rs`), so this disables the inline branch in
///    code that was ALREADY COMPILED — which mechanism 2 alone cannot do, and
///    which is the gap that made "ZGC publishes nothing" the safer choice in
///    August.
/// 2. **Arming blocks the emission.** Every inline compact-field site is gated
///    on `narrow_oops_block_inline_fields()`, which folds in
///    `zgc_read_barrier_blocks_inline_fields()`. Methods compiled while a
///    barrier is armed contain no inline reference load at all.
/// 3. **Relocation is still gated separately** by `zgc_relocation_permitted`
///    and `CRATONVM_ZGC_RELOCATE`, neither of which this touches.
///
/// The ordering obligation mechanism 1 carries: a thread may pass the
/// containment compare and be preempted before the load, so arming must not
/// race a live inline sequence. Arming a real cycle is a safepoint operation
/// for the same reason the mark-start pause is; that is an obligation on
/// whoever writes the first non-test caller of `set_barrier_color`, and it is
/// stated at that function.
pub fn zgc_jit_read_bounds_enabled() -> bool {
    static ENABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ENABLED.get_or_init(zgc_jit_read_bounds_requested)
}

/// Uncached half of [`zgc_jit_read_bounds_enabled`] (zt t6/rv3): ON unless
/// `CRATONVM_ZGC_NO_JIT_READ_BOUNDS` holds an on-word. It read presence
/// before, so `=0` turned the read bounds OFF.
pub fn zgc_jit_read_bounds_requested() -> bool {
    !false
}

/// zt t18/zb1 (zr1br A2a / A3): what `CRATONVM_ZGC_LOAD_BARRIER` asks of a
/// new production heap. Opt-in, default [`ZLoadBarrierMode::Off`].
///
/// Both on-modes arm the read barrier in **plain-store mode**
/// ([`ZgcRealHeap::arm_load_barrier_plain_stores`]): every reference read the
/// heap serves goes through [`ZgcRealHeap::load_barrier_slot`], while every
/// store still writes a plain machine pointer (A2b, the coloring stores, is
/// not built). The barrier tells the two words apart by
/// [`vaddr::Z_COLORED_TAG`] (bit 63), which no user-space pointer carries.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ZLoadBarrierMode {
    /// Unset, empty, or an off-word: nothing is armed (the default).
    Off,
    /// Any other on-word: route reads through the barrier, mark nothing.
    Read,
    /// `mark`: as `Read`, and while a concurrent mark is running a plain
    /// word a load returns is ALSO published to the marker (A3, additive:
    /// SATB stays on, so a read the barrier misses loses nothing).
    Mark,
    /// zt t19/zb2 (A2b): `colored` asks for COLORED STORES -- every reference
    /// store into an 8-byte compact field or reference-array element writes
    /// the current good-colored word
    /// ([`ZgcRealHeap::arm_load_barrier_colored_stores`]).
    ///
    /// zt t20/zb3: **accepted** (opt-in). The GC-reader inventory of
    /// `docs/feature-designs/zgc-architecture.md` 3.3 (R1-R9) is
    /// closed by decoding at the two reader primitives every walker uses
    /// (`cratonvm_types::narrow_oop::read_ref_slot`,
    /// `cratonvm_types::read_compact_field`), by keeping LEGACY 16-byte cells
    /// plain, and by the slide's rewrite writing colored words; see
    /// `docs/feature-designs/zgc-architecture.md`. Still refused
    /// (stderr line, plain-store arming) when the colored arming itself
    /// declines: compressed oops, an arena wider than the 42-bit colored
    /// offset (zt t23/zb8), or another colored-store heap in this process
    /// owning the decode base.
    Colored,
}

/// Parse one `CRATONVM_ZGC_LOAD_BARRIER` value (see [`ZLoadBarrierMode`]).
pub fn parse_zgc_load_barrier_mode(raw: &str) -> ZLoadBarrierMode {
    let v = raw.trim();
    if v.is_empty() || cratonvm_types::flags::value_is_off(v) {
        ZLoadBarrierMode::Off
    } else if v.eq_ignore_ascii_case("mark") {
        ZLoadBarrierMode::Mark
    } else if v.eq_ignore_ascii_case("colored") {
        ZLoadBarrierMode::Colored
    } else {
        ZLoadBarrierMode::Read
    }
}

/// zt t19/zb2: the one stderr line a refused `CRATONVM_ZGC_LOAD_BARRIER=colored`
/// prints (see [`ZLoadBarrierMode::Colored`]). zt t20/zb3: printed only when
/// the colored arming itself declines.
pub const ZGC_COLORED_STORES_REFUSED: &str = "cratonvm: warning: \
     CRATONVM_ZGC_LOAD_BARRIER=colored refused: colored stores cannot be armed \
     on this heap (compressed oops, a heap wider than 4 TiB, or another \
     colored-store heap in this process); the load barrier is armed in \
     plain-store mode instead";

/// zt t23/zb8: can every address of an arena of `arena_bytes` be written as a
/// colored word (`offset < 2^42`, [`vaddr::Z_MAX_HEAP_SIZE`])? The largest
/// offset is `arena_bytes - 1`.
pub fn colored_arena_fits_offset(arena_bytes: usize) -> bool {
    arena_bytes as u64 <= vaddr::Z_MAX_HEAP_SIZE
}

// zt t20/zb3: the flagless colored-slot decode in `cratonvm-types` (used by
// `read_ref_slot` / `read_compact_field`, below this crate) restates the
// encoding; the two must never drift.
const _: () = assert!(
    vaddr::Z_COLORED_TAG == cratonvm_types::narrow_oop::COLORED_SLOT_TAG
        && vaddr::Z_OFFSET_MASK == cratonvm_types::narrow_oop::COLORED_SLOT_OFFSET_MASK
        && vaddr::Z_METADATA_MASK == cratonvm_types::narrow_oop::COLORED_SLOT_METADATA_MASK
        && vaddr::Z_REMAPPED == cratonvm_types::narrow_oop::COLORED_SLOT_REMAPPED
);

/// zt t22/zb5 (stage 2e): `CRATONVM_ZGC_COLOR_FLIP`, opt-in (default OFF),
/// read when a production heap is built and acting only on a heap
/// `CRATONVM_ZGC_LOAD_BARRIER=colored` armed with colored stores
/// ([`ZgcRealHeap::arm_load_barrier_color_flip`]). Per heap, not latched.
pub fn zgc_color_flip_requested() -> bool {
    zgc_reloc_stack_flag("CRATONVM_ZGC_COLOR_FLIP")
}

/// zt t22/zb5: the stderr line a refused `CRATONVM_ZGC_COLOR_FLIP` prints
/// (another colored-store heap of this process owns the compiled bad-mask
/// cell); the heap keeps colored stores with the good color pinned.
pub const ZGC_COLOR_FLIP_REFUSED: &str = "cratonvm: warning: \
     CRATONVM_ZGC_COLOR_FLIP refused: another heap in this process owns the \
     compiled bad-mask cell; the good color stays pinned at Remapped";

/// `CRATONVM_ZGC_LOAD_BARRIER`, read when a production heap is built
/// ([`ZgcRealHeap::new_shared_with_initial`]); per heap, not latched.
pub fn zgc_load_barrier_mode() -> ZLoadBarrierMode {
    match None::<String> {
        Some(v) => parse_zgc_load_barrier_mode(&v),
        // Unset: the concurrent-relocation profile arms the colored barrier.
        None if zgc_concurrent_relocation_profile_enabled() => ZLoadBarrierMode::Colored,
        None => ZLoadBarrierMode::Off,
    }
}

/// zt t18/zb1: `CRATONVM_DBG_ZGC_LOAD_BARRIER`, the armed read census.
///
/// `0` off (unset); `1` counts the reads the armed barrier serves and prints
/// `[GC] zgc-load-barrier:` at shutdown; `2` additionally makes the heap's own
/// field and element accessors SKIP their armed arm and count the raw read as
/// `lb_unrouted` -- the positive control for that counter. `2` is sound only
/// in plain-store mode (every slot holds a plain pointer, so the raw read
/// answers the same object), which is the only mode a production heap arms.
/// Cached: it is a diagnostic, read on the armed read path.
pub fn zgc_load_barrier_census_level() -> u8 {
    static LEVEL: std::sync::OnceLock<u8> = std::sync::OnceLock::new();
    *LEVEL.get_or_init(|| {
        match Err::<String, std::env::VarError>(std::env::VarError::NotPresent) {
            Ok(v) if v.trim() == "2" || v.trim().eq_ignore_ascii_case("bypass") => 2,
            Ok(v) if !v.trim().is_empty() && !cratonvm_types::flags::value_is_off(v.trim()) => 1,
            _ => 0,
        }
    })
}

/// zr z2/g2: one generational run's collector time, split by cycle KIND.
///
/// # Why this exists
///
/// The z1 host rows put generational mode at ~2x the default's CPU on
/// `Zr1GnGenShape`, and nothing printed could say whether that was more
/// cycles, dearer cycles, or the mutator. The per-cycle `[GC] zgc-pause:`
/// line has the laps but not the kind, and summing it by hand over ~40 cycles
/// per run per arm is how a row gets misread. This is the sum, by kind, at
/// shutdown (`[GC] zgc-gen-cost:`).
///
/// Filled only on a GENERATIONAL heap and only when the pause clock runs
/// (`--verbose:gc`, a pause target, or a sampling director), so a default run
/// pays one relaxed load per cycle and no clock read.
///
/// Row `[0]` is young cycles, `[1]` whole-heap cycles; the columns are the
/// `GEN_COST_*` indices.
#[derive(Default)]
pub struct ZGenCost {
    pub rows: [[AtomicU64; GEN_COST_COLUMNS]; 2],
}

/// Cycles of this kind.
pub const GEN_COST_CYCLES: usize = 0;
/// Whole stop-the-world, including the slide (`pause_with_relocate_us`).
pub const GEN_COST_PAUSE_US: usize = 1;
/// Concurrent-mark close + TLAB retire + registry snapshot.
pub const GEN_COST_PRE_US: usize = 2;
/// Mark-bit pass (incl. a young cycle's pre-mark), remembered-set roots, mark.
pub const GEN_COST_MARK_US: usize = 3;
/// Of `MARK_US`: the young cycle's remembered-set pass (`young_extra_roots`).
pub const GEN_COST_REMSET_US: usize = 4;
/// Finalizer resurrection + reference processing.
pub const GEN_COST_REFS_US: usize = 5;
/// Sweep + free-list hand-over + the PHASE G bookkeeping.
pub const GEN_COST_SWEEP_US: usize = 6;
/// Of `SWEEP_US`: the nursery census walk(s).
pub const GEN_COST_CENSUS_US: usize = 7;
/// The slide (0 on a cycle that did not slide).
pub const GEN_COST_RELOCATE_US: usize = 8;
/// Objects the sweep visited / found dead.
pub const GEN_COST_SWEPT: usize = 9;
pub const GEN_COST_DEAD: usize = 10;
/// Objects the slide moved.
pub const GEN_COST_MOVED: usize = 11;
/// zt t7/tg (GN-10): of `MARK_US`, the cycle's linear streams over the arena
/// before the trace -- the mark-bit clear (`mark_clear_all`) and a young
/// cycle's pre-mark (`premark_from_old_bits`, or the header walk without the
/// old-start bitmap). With `PRE_US` (the registry snapshot) and `SWEEP_US`
/// (the young-aged word walk) it is GN-10's deciding sum.
pub const GEN_COST_STREAM_US: usize = 12;
/// zt t16/gn6 (GN-10): of `SWEEP_US`, the young-aged walk's time over grains
/// that held no non-old base -- the word walk a summary could skip, as
/// opposed to the per-object visits beside it. Timed between busy grains
/// (two clock reads per grain that held a non-old base), summed over the
/// shards under `CRATONVM_ZGC_GEN_YOUNG_SWEEP_WORKERS`; 0 on a major and on a
/// young cycle off the aged arm. See `ZgcRealHeap::sweep_young_aged_words`.
pub const GEN_COST_SWEEP_WALK_US: usize = 13;
pub const GEN_COST_COLUMNS: usize = 14;

/// What [`ZgcRealHeap::sweep_young_complement`] hands the installer.
pub struct ZYoungComplement {
    /// The shard: pass (1)'s per-object work plus the nursery's tallies.
    /// Its `spans` hold only pass (1)'s and the large-object end's spans.
    pub sh: ZSweepShard,
    /// `(visited, bytes_freed, bytes_copied, promoted_bytes)` below the floor,
    /// the same slice `sweep_young_aged` reports.
    pub below: (usize, usize, usize, usize),
    /// The nursery's free spans, arena offsets, ascending, all at or above
    /// `c0`.
    pub spans: Vec<(usize, usize)>,
    /// Where the low cursor goes (arena offset).
    pub new_cursor_off: usize,
    pub floor_off: usize,
    /// Bytes pass (1) freed (all LOW) and counted in `sh.bytes_freed`.
    pub low_per_object_freed: usize,
    /// zt t16/gn5fix: the nursery's runs of dead space handed back -- one per
    /// span, plus one when the cursor retracts over a trailing run (which
    /// gets no span) -- for `gen_dead_runs`, as the per-object arm counts its
    /// flushed spans.
    pub runs: usize,
    /// t1 wave 8 lane ZT3GN: where pass (2)'s time goes, for the
    /// `[zgc-young-complement]` line (`CRATONVM_DBG_ZGC_RELOCATE_PHASES=1`):
    /// `(bitmap words walked, live old visited, live young visited, words
    /// whose dead bits were taken in bulk, pass (1) us, pass (2) us)`. The
    /// clocks read 0 unless the debug switch is on.
    pub diag: (usize, usize, usize, usize, u64, u64),
    /// t1 wave 8 lane ZT3GN (`CRATONVM_ZGC_GEN_YOUNG_BELOW_FLOOR_COMPLEMENT`):
    /// pass (1)'s complement windows below the floor, arena offsets,
    /// ascending (empty on every other arm). The installer drops the old free
    /// blocks that touch one and lists the windows instead.
    pub below_windows: Vec<(usize, usize)>,
}

/// Refusal codes of [`ZgcRealHeap::sweep_young_complement`], printed as
/// `complement_last_refusal=` on `[GC] zgc-gen-cost:`.
pub const YC_REFUSE_NO_MARK_BITS: usize = 1;
pub const YC_REFUSE_HASH_REGISTRY: usize = 2;
pub const YC_REFUSE_PER_OBJECT_SWITCH: usize = 3;
pub const YC_REFUSE_SPILL: usize = 4;
pub const YC_REFUSE_FLOOR: usize = 5;
pub const YC_REFUSE_SKIP_REGIONS: usize = 6;
pub const YC_REFUSE_BELOW_FLOOR_EXTENT: usize = 7;
pub const YC_REFUSE_INSIDE_STRADDLER: usize = 8;

impl ZGenCost {
    #[inline]
    pub fn add(&self, young: bool, col: usize, v: u64) {
        self.rows[usize::from(!young)][col].fetch_add(v, Ordering::Relaxed);
    }

    pub fn row(&self, young: bool) -> [u64; GEN_COST_COLUMNS] {
        let r = &self.rows[usize::from(!young)];
        std::array::from_fn(|i| r[i].load(Ordering::Relaxed))
    }
}

/// The cold half of [`ZgcRealHeap`] -- see [`ZgcRealHeap::counters`].
pub struct ZgcCounters {
    /// Has the JIT inline-load bound been withdrawn because the arena now has
    /// decommitted holes in it? A one-way latch; see the site that sets it in
    /// `collect_garbage`'s uncommit block.
    pub jit_read_bounds_retracted: AtomicBool,
    /// Fragmentation ratchet — Phase 2.2. See [`ZFragGauge`].
    ///
    /// Held as three plain atomics rather than a `Mutex<ZFragGauge>` because
    /// they are written once per collection, under the arena lock, and read at
    /// shutdown; there is no invariant across them that a torn read could
    /// break, only three numbers describing one sample.
    pub frag_samples: AtomicUsize,
    /// Worst `largest_free_block * 1000 / capacity` seen, or `usize::MAX` for
    /// "never sampled" — deliberately a sentinel rather than 1000, so that a
    /// run which never met the sampling condition cannot be mistaken for one
    /// that scored perfectly.
    pub frag_worst_permille: AtomicUsize,
    /// Free share of capacity at the worst sample, permille.
    pub frag_worst_free_permille: AtomicUsize,
    /// Collection number of the worst sample.
    pub frag_worst_cycle: AtomicUsize,
    /// One-shot latch for the floor warning.
    pub frag_floor_warned: AtomicBool,
    /// Cycles in which the concurrent-mark driver refused to certify a
    /// complete mark set and the single-threaded marker ran instead. Non-zero
    /// is not a crash — it is the fail-closed path doing its job — but it is
    /// the number that says the driver is not trustworthy on this workload.
    pub parallel_mark_fallbacks: AtomicUsize,
    /// zr z4/rv4: marked bases whose out-edges a failed parallel mark's
    /// serial fallback re-pushed (`parmark_fallback_rescan_seeds`).
    pub parallel_mark_fallback_rescans: AtomicUsize,
    /// `allocated` at the last stress-triggered collection — see `needs_gc`.
    pub gc_stress_mark: AtomicUsize,
    /// Addresses that must NOT be relocated, with a use count.
    ///
    /// Filled by `ZgcRealHeap::pin_critical` and drained by
    /// `ZgcRealHeap::unpin_critical`. Refcounted because the same array can be
    /// inside two nested critical sections, and the inner release must not
    /// unpin the outer.
    ///
    /// **No production caller fills it today.** The JNI array checkouts
    /// (`GetPrimitiveArrayCritical`, `Get<Type>ArrayElements`) stopped pinning
    /// in gc-common w2-c: they hand native code a detached copy and hold the
    /// source array through a remappable per-VM JNI global ref, so the
    /// copy-back at `Release` re-resolves the array's current address and a
    /// move is harmless (`VmHeap::pin_critical_region` was deleted in w3-b).
    /// The remaining callers are this file's tests; the GPU bridge keeps its
    /// own pins (`cuda-bridge/src/critical.rs`). The table stays so a future
    /// direct-pointer critical section has an immovability primitive.
    ///
    /// # Why this exists (2026-08-14)
    ///
    /// `pin_critical_region`'s ZGC arm returned `Vec::new()`, i.e. pinned
    /// nothing, and that was correct for as long as this collector never moved
    /// an object. It moves now. The hazard is not the native pointer — this VM
    /// hands native code a *copy* — it is the **copy-back at Release**, which
    /// re-resolves the Get-time array address. If the slide moved that array,
    /// the copy-back writes a whole array's worth of bytes over whatever
    /// object now occupies the old address. The JNI site's own comment names
    /// the outcome: "data loss / write to a recycled object".
    ///
    /// A `Mutex<FxHashMap>` rather than something clever: it is touched twice
    /// per critical section and once per collection, and it is empty in every
    /// workload that makes no critical calls at all.
    pub critical_pins: Mutex<FxHashMap<usize, usize>>,
    /// The PER-CYCLE pin set `ZRelocateContext::is_pinned` consults, hoisted
    /// once by [`ZgcRealHeap::begin_page_evac_cycle`].
    ///
    /// *Stage 1 of `handoff-h-adopt-zrelocate-stage-1.md`.* The trait asks
    /// "is this object pinned?" **per object**, and the two truthful sources
    /// are [`Self::critical_pins`] (a map, O(1)) and
    /// `gc_quiescence::pinned_jit_roots_snapshot()`, which **returns a
    /// `Vec<usize>`**. Calling the latter per object is `O(objects x pins)`
    /// plus an allocation each, which is not a slower answer inside a pause --
    /// it is the pause. So the union is taken once per cycle and the hot path
    /// is one hash probe.
    ///
    /// Empty outside a page-evacuation cycle, which is every cycle today: an
    /// empty set answers `false` for every address, and `false` here means
    /// "movable". That is only safe because the ARM is not wired (its switch,
    /// `CRATONVM_ZGC_PAGE_EVAC`, never had a reader on a shipping path and was
    /// retired in zt t12/rt). Whoever wires it must call
    /// `begin_page_evac_cycle` before the first `is_pinned`, and the assertion
    /// that they did is that the set is non-empty on any heap with a live
    /// critical section.
    pub page_evac_pins: Mutex<FxHashSet<usize>>,
    /// `from -> to` for every object the page-based relocator moved this cycle.
    ///
    /// **This is the single highest-risk line in stage 1** (§3c of
    /// `handoff-h-adopt-zrelocate-stage-1.md`).
    /// `<ZgcRealHeap as GarbageCollector>::collect_garbage` builds its
    /// `pointer_map` from the SLIDE's own pair vector. An arm that relocates
    /// through `ZRelocate` and leaves that alone ships an **empty** pointer map
    /// for a cycle in which objects moved -- and an empty map is not "no
    /// information", it is the positive claim *nothing moved*:
    /// `MonitorCleanup::remap_after_gc` early-returns on one, so the monitor
    /// table, the cas-lock table, JNI/JVMTI handles, class statics and the
    /// interned string pool all keep from-space addresses.
    ///
    /// Recorded here by [`ZRelocateContext::on_relocated`](crate::zgc::relocate::ZRelocateContext::on_relocated)
    /// so that the obligation is structural rather than a checklist item, and
    /// drained by [`ZgcRealHeap::end_page_evac_cycle`].
    pub page_evac_moves: Mutex<Vec<(usize, usize)>>,
    /// `CRATONVM_DBG_ZGC_CORPSE` -- the last slide's `from -> (to, class_id,
    /// size)`, so a later read of a vacated span can say WHAT used to live
    /// there instead of reporting an anonymous `num_slots=0`.
    ///
    /// This exists because `compact_low_to` zeroes the span it vacates, on
    /// purpose, so that a conservative scan cannot resurrect a corpse. The
    /// side effect is that a stale pointer resolves to a well-formed
    /// ALL-ZERO object: `class_id=0`, `num_slots=0`, and the reader walks off
    /// the end of a zero-length object. Every identifying byte is gone by the
    /// time anyone notices, which is why "who still points here?" has been
    /// unanswerable -- the evidence is destroyed by the same memset that makes
    /// the bug safe to survive.
    ///
    /// Keyed by the vacated address; interior addresses resolve through the
    /// stored size, because a stale pointer is not always to the base.
    ///
    /// Accumulates across cycles rather than keeping only the last one. A
    /// holder that reads the address in the same cycle it was vacated and one
    /// that cached it ten collections ago produce the identical `num_slots=0`
    /// warning, and they are different bugs -- the first is a missing remap,
    /// the second a raw address kept across a safepoint. Capped, oldest cycles
    /// evicted first, so a long run cannot grow it without bound.
    pub corpse_ledger: Mutex<FxHashMap<usize, (usize, u32, usize, u64)>>,

    /// `vacated address -> where the object went`, for every object this
    /// collector's slide has relocated and whose old address has not been
    /// handed out again.
    ///
    /// **This is what makes `VmHeap::load_and_forward` work on this backend.**
    /// That barrier repairs a possibly-stale reference by reading a FORWARDING
    /// WORD at the old address, and the slide leaves none: `compact_low_to`
    /// zeroes the span above the new cursor and the memmove overwrites
    /// everything below it. So on the DEFAULT collector the barrier was a
    /// silent no-op at all 46 of its call sites — every one of which exists
    /// because its caller holds an `ObjectRef` somewhere no root scan can see
    /// (a native's Rust local, an invoke's popped-argument buffer) across an
    /// operation that can collect.
    ///
    /// Measured before this table existed, with `CRATONVM_DBG_VACATED_FRAMES`
    /// armed on H2 `TestMultiThread.testConcurrentUpdate`: the barrier was
    /// handed an address the collector had moved an object away from, from
    /// `apps_h2::h2_comparison_compare`, `h2_comparison_get_value` and
    /// `properties_sidetable::mirror_loaded_entries_to_properties_backend`,
    /// and could not repair any of them.
    ///
    /// **Why no pruning is needed for correctness.** A lookup is only consulted
    /// when the address is NOT a registered object base, so a re-issued address
    /// never reaches the table — the entry is simply unreachable, not wrong.
    /// Pruning is therefore memory hygiene, and since zr z2/r2 it is
    /// amortized (`CRATONVM_ZGC_RELOCATIONS_PRUNE_AMORTIZED`): see
    /// `prune_relocations`.
    ///
    /// **The value carries the slide EPOCH that recorded it** (zr z2/r2,
    /// `slide_epoch`). One address is often both a source and a destination of
    /// the SAME slide -- Y slides from B down to C, then X slides from A down
    /// into B -- so the table holds `A -> B` and `B -> C` at once, and an
    /// unconditional chain walk answered `C` (Y) for a stale reference to X.
    /// `forwarded_after_slide` follows a hop only when the next entry is from
    /// a LATER slide than the one it arrived by, which is exactly the
    /// multi-cycle chain the walk exists for.
    pub relocations: Mutex<FxHashMap<usize, (usize, u32)>>,
    /// Slides published into `relocations` so far (zr z2/r2); the epoch stamped
    /// on each entry. Wraps after 4 G slides, which only weakens the chain rule
    /// for entries four billion collections old.
    pub slide_epoch: std::sync::atomic::AtomicU32,
    /// `relocations.len()` at which the next amortized prune is due (zr
    /// z2/r2): twice what the last prune left, floored at
    /// `ZGC_RELOCATIONS_PRUNE_MIN`. See `prune_relocations`.
    pub relocations_prune_due_at: AtomicUsize,
    /// Prunes that walked the table / collections that skipped it (zr z2/r2).
    pub relocation_prunes_run: AtomicUsize,
    pub relocation_prunes_skipped: AtomicUsize,
    /// Entries those prunes walked, summed (zr z2/r2): the cost the amortized
    /// rule bounds by twice the entries ever inserted.
    pub relocation_prune_entries_walked: AtomicU64,
    /// `base -> alloc_size(header)` as the LAST slide left it, taken after the
    /// registry rebuild and before any mutator resumed.
    ///
    /// The pre-sweep extent census consults it for every overlap it finds, and
    /// the answer splits the search in half: a base that was present then, with
    /// a smaller size then, has had its HEADER rewritten under it while
    /// mutators ran; a base that was absent arrived by an insert, and the
    /// insert audit is looking at the wrong call sites.
    ///
    /// Only populated under `CRATONVM_DBG_ZGC_CORPSE`, and replaced wholesale
    /// each slide so it can never describe two cycles at once.
    pub slide_exit_sizes: Mutex<FxHashMap<usize, usize>>,
    /// Cycles that declined to relocate because a compiled frame was live.
    ///
    /// Exported so the cost of the refusal is a number rather than a guess:
    /// on this collector compaction is also defragmentation, so a process that
    /// is permanently JIT-busy trades heap layout for the correctness of not
    /// sliding objects out from under registers the collector cannot rewrite.
    pub relocation_skipped_jit: AtomicUsize,
    /// Cycles that relocated **while a compiled frame was live**, on the
    /// strength of the collection's per-cycle coverage proof rather than on JIT
    /// quiet.
    ///
    /// The companion to [`Self::relocation_skipped_jit`], and the number that
    /// says whether the proof is doing anything: a run with `skipped_jit` high
    /// and this zero is one where the proof never passes -- which on a
    /// many-threaded workload is the expected answer, because the proof marks
    /// CROSS_THREAD_JIT_PEER incomplete whenever a peer is in compiled code.
    /// Without this counter the two are indistinguishable at the summary line.
    pub relocation_on_proven_jit: AtomicUsize,
    /// WHICH term of the relocation gate refused, per cycle.
    ///
    /// [`Self::relocation_skipped_jit`] is a COUNT, and a count of a
    /// five-term conjunction cannot be acted on: `skipped_jit=13` is the same
    /// number whether the kill switch is off, moving-young is disabled, the
    /// per-cycle coverage proof failed, or this thread was scanned
    /// conservatively. Each of those wants a different repair, and on
    /// `DefaultCatalogAndSchemaTest` (2026-08-30) the difference was the whole
    /// investigation: 13 of 14 cycles declined, and the generational
    /// collector's own reason census read `coverage_fallbacks=0` because
    /// `bump_reason_count` has exactly one caller and it is not on this
    /// collector's path (see `gc_quiescence::moving_young_incomplete_reason_mask`,
    /// which says so in as many words).
    ///
    /// Indexed by [`relocation_skip_reason`]. First failing term wins, in the
    /// order the gate evaluates them, so the index names what actually forced
    /// the decision rather than every term that happened to be false.
    pub relocation_skip_reasons: [AtomicUsize; relocation_skip_reason::COUNT],
    /// When the refusal was [`relocation_skip_reason::COVERAGE_INCOMPLETE`],
    /// the `gc_quiescence::incomplete_reason` code that proof recorded.
    ///
    /// Separate from the array above because it answers the NEXT question: the
    /// gate says "the proof failed", and this says which obligation it failed
    /// on. Sized by the quiescence module's own `COUNT` so a new reason there
    /// is a compile error here rather than a silently dropped column.
    pub relocation_coverage_reasons: [AtomicUsize; cratonvm_gc_core::gc_quiescence::incomplete_reason::COUNT],
    /// Per-term census of the relocation refusal, bumped **outside** the
    /// `compiled_frames_live && !frames_are_rewritable` gate.
    ///
    /// Same index space as [`Self::relocation_skip_reasons`], and the pair is
    /// only useful read together: this one answers *"which obligation failed
    /// this cycle"*, the other answers *"and did that stop the slide"*. A term
    /// with `relocation_refusal_terms[r] > 0` and
    /// `relocation_skip_reasons[r] == 0` fired on every cycle it was asked
    /// about and stopped none of them -- which is the bypass
    /// `docs/feature-designs/zgc-architecture.md`
    /// is about, made countable.
    ///
    /// A SECOND array rather than moving the two `fetch_add`s out of the `if`,
    /// deliberately: that move would re-define every
    /// `[GC] zgc-relocation-skip-reason:` number already collected, and those
    /// are quoted in dated records. Nothing here re-defines anything.
    ///
    /// **One row is structurally zero and that is not a defect.**
    /// [`relocation_skip_reason::TLAB_RETIRE_INCOMPLETE`] is an earlier,
    /// unconditional refusal that returns before this census is reached.
    /// (Code 6, the blanket JIT refusal, was retired in zt t12/rt.)
    ///
    /// `CRATONVM_ZGC_ASSUME_REWRITABLE` is deliberately **not** consulted: that
    /// flag licenses the slide, it does not discharge the obligation, and a
    /// census that hid the obligation under it would be blind to exactly the
    /// thing the flag exists to price.
    pub relocation_refusal_terms: [AtomicUsize; relocation_skip_reason::COUNT],
    /// Pages [`forwarding::ZRelocationSet::select`] ranked as profitable and
    /// then left behind because the next one would carry `live_total` past
    /// [`forwarding::ZRelocationPolicy::max_evacuation_bytes`].
    ///
    /// `select` is a prefix rule: it stops at the first candidate that would
    /// overrun, and every remaining eligible page is deferred. Until this
    /// counter existed, `deferred_for_budget()` had no caller outside the
    /// module's own tests, so "the slide evacuated the whole live set" and
    /// "the slide evacuated a 64 MiB prefix of it" produced byte-identical
    /// operator output -- one `compaction_cycles`, a large
    /// `objects_relocated`, and no skip reason at all, because the refusal
    /// census is about whether the slide RAN, not whether it FINISHED.
    ///
    /// The two readings point at opposite repairs for a fragmentation
    /// `OutOfMemoryError` on this collector, where compaction is the only
    /// defragmentation there is.
    pub relocation_pages_deferred: AtomicUsize,
    /// Cycles on which [`Self::relocation_pages_deferred`] grew at all -- i.e.
    /// cycles where the evacuation budget actually bound.
    ///
    /// Separate from the page total because the page total cannot distinguish
    /// one pathological cycle from a run that truncates every cycle, and those
    /// are different claims about the constant.
    pub relocation_budget_truncated_cycles: AtomicUsize,
    /// Lifetime count of TLAB cells a [`Self::retire_all_tlabs`] could not
    /// lock, and so could not close.
    ///
    /// A retained chunk would be unsound on a compacting heap: the collector
    /// cannot see it (every walker here is driven by the object-start
    /// registry), so nothing keeps the compaction cursor above it,
    /// `compact_low_to` retracts past it and zeroes it, and
    /// `clear_low_free_list` drops the only record that it was reserved --
    /// after which the arena and the chunk's owner both fill one span.
    ///
    /// **Measured zero** across the `ResourceLeakDetectorTest` repro that the
    /// reopened `zgc-rewrite-pass-walks-off-a-reference-array` page is about,
    /// which is what rules that story out as the source of the overlapping
    /// registry entries seen there. Exported so the ruling-out is re-checkable
    /// rather than remembered.
    pub tlab_retire_skipped_total: AtomicUsize,

    /// How many unwalkable rewrite targets have already been logged in full,
    /// so a heap that produces thousands does not produce thousands of lines.
    pub unwalkable_reports: AtomicUsize,
    /// zs s1/dg (`CRATONVM_ZGC_REWRITE_OVERSIZE_ARRAYS`, default ON, `=0`
    /// kill): the rewrite pass and `reference_slots_with` size and walk a
    /// LIVE array past `MAX_PLAUSIBLE_ARRAY_LEN` that passes
    /// [`ZgcRealHeap::oversize_live_array_extent`] instead of skipping it
    /// (the rewrite) or omitting its elements (the slot walk). Read once per
    /// heap at construction.
    pub rewrite_oversize_arrays: AtomicBool,
    /// zs s1/dg: rewrite targets that `alloc_size` refused and the oversize
    /// screen sized (the engagement counter of the switch above).
    pub rewrite_oversize_arrays_walked: AtomicUsize,
    /// zs s1/dg: reference arrays past `MAX_PLAUSIBLE_ARRAY_LEN` whose
    /// elements `reference_slots_with` OMITTED (screen refused, or the
    /// switch is off). Each one is a set of slots the rewrite pass could not
    /// see, i.e. potential dangling references; logged at `error`, capped.
    pub oversize_ref_arrays_omitted: AtomicUsize,
    /// `CRATONVM_ZGC_ELEMENT_CARD_REFUSED_SCAN` (default ON, `=0` kill): a
    /// young cycle's element card on a reference array past
    /// `MAX_PLAUSIBLE_ARRAY_LEN` that the live screen REFUSES is scanned
    /// conservatively (the granule's own slots) and kept, instead of being
    /// dropped without rooting its young targets. Read once per heap.
    pub element_card_refused_scan: AtomicBool,
    /// Element granules scanned by the refused-screen arm above (engagement).
    pub element_card_refused_scans: AtomicUsize,
    /// zs s1/dg: survivors the slide's census / partition / both slides
    /// sized through the oversize screen ([`ZgcRealHeap::survivor_size`])
    /// after `alloc_size` refused them.
    pub oversize_survivors_sized: AtomicUsize,
    /// Slides completed, so a corpse can say HOW MANY cycles ago it was
    /// vacated -- "the slide that just ran" and "nine collections back" are
    /// different bugs with the same signature.
    pub corpse_cycle: AtomicU64,
    /// How many OOB reads have already been reported in full, so a workload
    /// that produces thousands does not produce thousands of backtraces.
    pub corpse_reports: AtomicUsize,
    /// Concurrent phases the DRIVER reported, summed over all cycles.
    ///
    /// This is the counter that distinguishes "the worker pool marked" from
    /// "`zgc_concurrent`'s controller drove the cycle" — the pool-only path
    /// this replaced produced identical mark bits, identical stats and an
    /// identical `parallel_mark_cycles`, so nothing else here can tell the two
    /// apart. `passes` exists only inside `ZgcMarkCycleOutcome`.
    pub driver_passes: AtomicUsize,
    /// Mutator ingress for the concurrent marker — Phase 3.
    ///
    /// Overwritten references arrive here from the VM's existing pre-write
    /// barrier and are drained by the marker. Allocated once with the heap and
    /// left empty while [`Self::mark_active`] is false, so a non-concurrent
    /// run pays for the buckets and nothing else.
    pub mark_ingress: mark::ZMarkIngress,
    /// zr z2/cc: the per-thread SATB buffers in front of `mark_ingress`
    /// (`CRATONVM_ZGC_SATB_THREAD_BUFFERS`). Drained into the ingress by
    /// [`ZgcRealHeap::drain_mark_ingress`] (the mark-end replay) and cleared
    /// with it by `set_mark_active`. See [`mark::ZSatbThreadBuffers`].
    pub satb_thread_bufs: std::sync::Arc<mark::ZSatbThreadBuffers>,
    /// The marking engine driving the CURRENT concurrent cycle, or `None`.
    ///
    /// # Why this is per-cycle and not a pool that outlives the heap
    ///
    /// The coordinator holds `Arc<dyn ZMarkContext>` == `Arc<ZgcRealHeap>`, so
    /// a coordinator parked here permanently would be a reference cycle and
    /// the heap (plus its worker threads) would never be freed. Building it at
    /// the mark-start safepoint and dropping it at the mark-end safepoint
    /// breaks the cycle at a point the collector controls, and
    /// [`mark::ZMarkCoordinator`]'s `Drop` stops and JOINS every worker there
    /// -- so no thread holding a clone of the heap outlives the cycle.
    ///
    /// The cost is one pool spawn per concurrent cycle. That is the same cost
    /// `mark_parallel_stw` already pays per collection, and it buys the whole
    /// transitive closure moving out of the pause.
    pub conc_pool: Mutex<Option<std::sync::Arc<mark::ZMarkCoordinator>>>,
    /// t1 wave 2 lane Z (`CRATONVM_ZGC_CONC_POOL_REUSE`, opt-in): the
    /// concurrent mark pool a CERTIFIED cycle closed with, parked between
    /// cycles so the next mark start does not spawn (and the closing pause
    /// does not join) `ConcGCThreads` threads. Per heap, never a process
    /// global. Parked only when it was built over the inert context (the weak
    /// arm, `CRATONVM_ZGC_CONC_POOL_WEAK_HEAP`), is not stopped, and the
    /// closing pause held its ONLY reference -- so no mutator can still hold a
    /// clone from the closed cycle (`hand_satb_batch_to_the_marker`) and feed
    /// a stale address into the next one. Between cycles its context is
    /// `None` (`end_cycle`), so it holds no heap and the heap's drop joins it.
    pub conc_pool_idle: Mutex<Option<std::sync::Arc<mark::ZMarkCoordinator>>>,
    /// t1 wave 6 lane Y4 (V4 finding 3): a parked pool the mark start could
    /// not use (wrong `ConcGCThreads` size, stopped, or the switch turned
    /// off), told to stop (`ZMarkCoordinator::stop`, no join) and kept here
    /// so its join happens later -- at the next certified close, the next
    /// retirement or the heap's drop -- when its workers have long exited and
    /// the join only reaps them. It used to be joined inside the mark-start
    /// pause. Never dropped while the mutex is held.
    pub conc_pool_retired: Mutex<Option<std::sync::Arc<mark::ZMarkCoordinator>>>,
    /// t1 wave 2 lane Z: mark starts that took [`Self::conc_pool_idle`]
    /// instead of spawning a pool.
    pub conc_pool_reuses: AtomicUsize,
    /// t1 wave 2 lane Z (`CRATONVM_ZGC_SATB_FLUSH_PAUSE_SAFE`): mark ends that
    /// refused certification because a SATB thread-buffer flush raced the
    /// drain. Expected 0; nonzero names a door that runs the barrier while a
    /// pause is in progress.
    pub satb_flush_races: AtomicUsize,
    /// `-XX:ParallelGCThreads` / `-XX:ConcGCThreads` /
    /// `-XX:±UseDynamicNumberOfGCThreads` as this VM handed them to this heap
    /// (zr z3/mt). Per heap, set once after construction by
    /// [`ZgcRealHeap::set_gc_worker_threads`]; never a process global. Cold:
    /// read once per cycle at most. See [`ZGcThreadFlags`].
    pub gc_threads: ZGcThreadFlags,
    /// The STW mark loop's per-object cost switches (zr z3/mt), seeded from
    /// the environment at construction and owned by this heap thereafter so a
    /// test can run both arms in one process. Cold side: the serial marker
    /// reads them once per call; `visit_strong_refs_at` one relaxed load per
    /// object behind the `counters` box (the hot struct is at its ratchet).
    /// See [`ZMarkLoopOpts`].
    pub mark_loop: ZMarkLoopOpts,
    /// Concurrent cycles started, and how many of them the mark-end handshake
    /// certified. A gap between the two is the collector falling back to a
    /// stop-the-world mark, which is safe and slow -- and invisible without
    /// this pair.
    pub conc_cycles_started: AtomicUsize,
    pub conc_cycles_completed: AtomicUsize,
    /// Objects that were allocated BLACK -- born marked because a cycle was in
    /// flight. See [`Self::allocate_black_if_marking`].
    ///
    /// zr z7/cm: STRIPED. This is bumped on EVERY allocation while a cycle is
    /// open, from every allocating thread; as one `AtomicUsize` it was a
    /// shared cache line taken exclusive per allocation by N mutators -- a
    /// cost only the concurrent arm pays, and on the TLAB path the counter was
    /// the only shared write left (the bit test above it is a plain load).
    pub conc_black_allocations: cratonvm_types::striped_counter::StripedCounter,
    /// zr z7/cm: drop an SATB record whose overwritten value is ALREADY
    /// MARKED (`CRATONVM_ZGC_SATB_FILTER_MARKED`, default on, `=0` kill).
    /// Seeded from the environment at construction, per heap so a test can
    /// run both arms. See [`ZgcRealHeap::satb_pre_barrier_slow`].
    /// `0` off, [`Z_SATB_FILTER_STORE`] per store, [`Z_SATB_FILTER_FLUSH`]
    /// at the thread-buffer flush (the default since zr z7b/cm).
    pub satb_filter_marked: std::sync::atomic::AtomicU8,
    /// zr zs s1/mc, `CRATONVM_ZGC_SATB_DEFER_REGISTRY` (**opt-in**, `1`): the
    /// per-store registry screen of the SATB barrier moves to the thread
    /// buffer's flush, beside the mark-bit filter (mode 2 + thread buffers +
    /// the mark bitmap only). See `satb_pre_barrier_slow`.
    pub satb_defer_registry: AtomicBool,
    /// Records the deferred screen refused at a flush (not a registered base).
    pub satb_deferred_refused: AtomicUsize,
    /// Records the deferred screen examined at a flush (the engagement count:
    /// non-zero only under the opt-in; one add per 256-entry flush).
    pub satb_deferred_screened: AtomicUsize,
    /// zr z7/cm: SATB records dropped by that filter (the store's old value
    /// was already marked), summed over cycles. Striped: it is bumped per
    /// store. Printed as `satb_filtered=` on `[GC] zgc-markend:` (per cycle).
    pub satb_filtered: cratonvm_types::striped_counter::StripedCounter,
    /// `satb_filtered` at this cycle's mark start, so `[GC] zgc-markend:` can
    /// print the cycle's own count. Written at the mark-start safepoint only.
    pub satb_filtered_at_start: AtomicUsize,
    /// zr z7b/cm: entries the flush-time filter (the default mode) dropped.
    /// One add per 256-entry buffer, so a plain atomic.
    pub satb_filtered_batched: AtomicUsize,
    /// zr z7b/cm: `allocated` as the collection found it, before the sweep
    /// resets it to the live bytes -- the far end of the adaptive window's
    /// allocation-rate sample. Written once per collection, at the safepoint,
    /// by `collect_garbage`; read by `refresh_adaptive_conc_start`.
    pub rate_alloc_at_entry: AtomicUsize,
    /// zr z11/rv9: concurrent mark starts declined because the mark pool
    /// could not be spawned (`CRATONVM_ZGC_CONC_POOL_WEAK_HEAP`; with `=0` a
    /// spawn failure panics as before).
    pub conc_pool_spawn_failures: AtomicUsize,
    /// zr z11/rv9 (`CRATONVM_ZGC_MARK_SPAWN_BACKOFF`): parallel-mark cycles
    /// still to send to the serial marker after a failed spawn, and the
    /// cumulative count of cycles it did send.
    pub mark_spawn_backoff_left: AtomicUsize,
    pub mark_spawn_backoff_skips: AtomicUsize,
    /// zr z11/rv9: the clock beside `rate_alloc_at_entry`, so the rate
    /// sample's time span ends where its byte count does (at the collection's
    /// entry) and not after the pause. `0` = not stamped (the refresh then
    /// uses its own clock, the z7b span). Written only with the adaptive
    /// window on.
    pub rate_nanos_at_entry: AtomicU64,
    /// zr z11/rv9: `CRATONVM_ZGC_CONC_RATE_EXCLUDE_PAUSE` (default on, `=0`
    /// kill). Per heap, seeded at construction.
    pub rate_exclude_pause: AtomicBool,
    /// zr z7b/cm: `CRATONVM_ZGC_CONC_RATE_FROM_ALLOCATION` (default on, `=0`
    /// kill) -- sample the allocation rate from the bytes ALLOCATED between
    /// collections. Per heap, seeded at construction.
    pub rate_from_allocation: AtomicBool,
    /// zr z7b/cm: `CRATONVM_ZGC_CONC_WINDOW_EXTRAPOLATE` (default on, `=0`
    /// kill) -- a concurrent phase cut short by its collection feeds the mark
    /// duration it would have NEEDED, not the one it got.
    pub conc_window_extrapolate: AtomicBool,
    /// Addresses the mutator ingress handed to the mark-end handshake, summed
    /// over all cycles. Zero with cycles started means the workload overwrote
    /// no references, which is a different fact from the barrier being unwired.
    ///
    /// The SUM of the concurrent hand-offs (both arms below) and the mark-end
    /// replay; [`Self::conc_ingress_handed`] is the first half on its own, so
    /// the replay is `conc_ingress_replayed - conc_ingress_handed` (zt t1/mk,
    /// residual 7 of `zgc-zs2mc2-mark-engine-review-residuals`).
    pub conc_ingress_replayed: AtomicUsize,
    /// Addresses handed to a RUNNING pool during the concurrent phase
    /// (`hand_satb_batch_to_the_marker` and `hand_thread_batch_to_the_marker`),
    /// cumulative. Also counted in [`Self::conc_ingress_replayed`].
    pub conc_ingress_handed: AtomicUsize,
    /// Wall-clock nanoseconds spent in the CONCURRENT phase, summed. Beside
    /// [`Self::conc_cycles_started`] it is the answer to "did the mutators
    /// actually run while this marked".
    pub conc_phase_nanos: AtomicU64,
    /// Wall-clock reading taken when the current cycle opened, so
    /// [`Self::conc_phase_nanos`] can be closed out at mark end. `0` when no
    /// cycle is open.
    pub conc_mark_started_at: AtomicU64,
    /// zt t31/zc11 (program page 1f residual R6): the concurrent phase of the
    /// cycle the last collection closed with a certified mark set (mark start
    /// to that collection's pause, Concurrent Mark Continue phases included),
    /// for its `-Xlog:gc,phases` `Concurrent Mark` line. `0` when the last
    /// collection closed none. Taken by
    /// [`ZgcRealHeap::take_last_concurrent_mark_nanos`].
    pub last_closed_conc_mark_nanos: AtomicU64,
    /// Objects the CONCURRENT phase finished tracing, summed over cycles.
    ///
    /// *Applies
    /// `docs/feature-designs/zgc-architecture.md`, which
    /// is step 1 of `proposal-a-concurrent-mark-by-default.md`. Instrumentation
    /// only: `CRATONVM_ZGC_CONC_START=0` stays the default.*
    pub conc_scanned_concurrently: AtomicU64,
    /// Objects traced INSIDE the mark-end pause because the concurrent window
    /// was too short, summed over cycles.
    ///
    /// **The ratio of this to [`Self::conc_scanned_concurrently`] is
    /// `proposal-a-concurrent-mark-by-default.md`'s acceptance bar** (step 3:
    /// below 0.1). The per-cycle figures are already on `[GC] zgc-markend:`,
    /// but a per-cycle `eprintln!` behind `--verbose:gc` cannot answer a
    /// question about the MEDIAN cycle without a log-parsing session per run,
    /// with logging on, which changes the thing being measured. These two
    /// cumulative counters make it a single figure the process reports at exit.
    /// Two relaxed adds per collection.
    pub conc_scanned_at_safepoint: AtomicU64,
    /// zt t7/cc: the concurrent overlay repair's engagement summed over
    /// cycles -- rows re-read and references pushed by
    /// `overlay_rows_written_during_mark` (`overlay_repair=<rows>/<refs>` on
    /// each `[GC] zgc-markend:` line, which only `--verbose:gc` prints). On
    /// `[GC] zgc-concurrent:` as `overlay_repair_rows_total=` /
    /// `overlay_repair_refs_total=`, so a row with `CRATONVM_GC_STATS=1`
    /// (stdout untouched) reads the positive control.
    pub conc_overlay_repair_rows: AtomicU64,
    pub conc_overlay_repair_refs: AtomicU64,
    /// Nanoseconds the adaptive sizing last aimed to leave between a cycle
    /// opening and the collection arriving — the window's own SETPOINT, beside
    /// which `conc_phase_nanos`'s per-cycle delta is the actual.
    ///
    /// Derived from the CLAMPED `conc_start_bytes` and the smoothed allocation
    /// rate, so it is what the collector will really get rather than what the
    /// unclamped arithmetic asked for. `0` means the sizing has no opinion:
    /// either `CRATONVM_ZGC_CONC_START` is a fixed percentage (adaptive off, so
    /// [`ZgcRealHeap::refresh_adaptive_conc_start`] returns before storing
    /// anything) or no allocation rate has been sampled yet.
    pub conc_window_target_nanos: AtomicU64,
    /// Barrier counters — slow-path entries, heals, forward lookups.
    pub barrier_stats: barrier::ZBarrierStats,
    /// Per-logical-page age, indexed by page id. A page's age is the number of
    /// cycles it has survived; `generation::ZPromotionPolicy` turns that into
    /// young-or-old. Grown on demand, never shrunk -- a page id is an index
    /// into the arena's logical grid and the arena does not shrink either.
    pub page_ages: Mutex<Vec<u32>>,
    /// Young collections the nursery-size trigger asked for.
    ///
    /// The engagement counter for G2d: this at zero on a generational run means
    /// every collection still came from the whole-heap trigger, so young cycles
    /// are as rare as full collections were and the nursery is unbounded in
    /// practice. That was the state before 2026-08-17.
    pub gen_nursery_triggers: AtomicUsize,
    /// zt t20/gn7 (D7): `CRATONVM_ZGC_GEN_NURSERY_AFTER_WARMUP` (default ON
    /// inside the generational mode, `=0` kill; latched at construction, an
    /// atomic so tests can flip it), the collection intervals in which it held
    /// the nursery clause back (`[GC] zgc-nursery-trigger:`
    /// `nursery_warmup_held=`), and that tally's once-per-interval latch
    /// (lowered by every collection). See the G2d clause of `needs_gc`.
    pub gen_nursery_after_warmup: AtomicBool,
    pub gen_nursery_warmup_held: AtomicUsize,
    pub gen_nursery_warmup_held_latch: AtomicBool,
    /// Bytes a sweep did NOT memset because it zeroed headers only.
    ///
    /// The engagement counter for [`zgc_sweep_header_zero`]: zero here with
    /// `young_cycles` above zero means every dead object was still being memset
    /// in full, i.e. the switch is on and inert. It is reported as *bytes* and
    /// not as a count because bytes are the cost — the whole point is that this
    /// term was O(reclaimed volume) rather than O(objects).
    pub gen_zero_bytes_skipped: AtomicUsize,
    /// Dead objects whose header the complement sweep did not write, under
    /// `CRATONVM_ZGC_SWEEP_SKIP_DEAD_HEADERS` (zr z1/al). Cumulative; the
    /// opt-in's engagement counter.
    pub sweep_dead_headers_skipped: AtomicUsize,
    /// zr z5/sw (`CRATONVM_ZGC_SWEEP_LIVE_PROPORTIONAL`): cycles whose monitor
    /// prune was fed by the key probe instead of the dead list, the keys
    /// probed, and the keys found dead (cumulative). `cycles == 0` on a run
    /// with an inflated monitor means the probe never engaged.
    pub sweep_prune_key_cycles: AtomicUsize,
    pub sweep_prune_keys_probed: AtomicUsize,
    pub sweep_prune_keys_dead: AtomicUsize,
    /// zr z5/sw: bitmap words the complement sweep handled by the region
    /// step (mark-summary-empty 32 KiB blocks), cumulative.
    pub sweep_region_words_skipped: AtomicUsize,
    /// zr z5/sw: mark-bitmap summary bits reset after the mark clear,
    /// cumulative. Zero means the region step can never engage.
    pub mark_summary_blocks_forgotten: AtomicUsize,
    /// Free-list spans a sweep handed over, and dead objects they covered.
    ///
    /// The engagement pair for [`zgc_sweep_dead_runs`]: `runs == objects` means no
    /// two dead objects were ever adjacent, so the merge is on and buying
    /// nothing. One number cannot say that — a small `runs` is equally
    /// consistent with a cycle that found little garbage — which is why both are
    /// kept.
    pub gen_dead_runs: AtomicUsize,
    /// Dead objects the runs in [`Self::gen_dead_runs`] covered.
    pub gen_dead_objects: AtomicUsize,
    /// The most the nursery has ever been over budget when a collection
    /// finally ran, in bytes.
    ///
    /// # Why this is a gauge and not a bug
    ///
    /// [`zgc_gen_nursery_percent`] is a TRIGGER: `needs_gc` answers `true` and
    /// the mutator keeps allocating until the VM reaches a safepoint and decides
    /// to collect. So the nursery passes its budget by however much is allocated
    /// in that window, and G2's open list has carried "a hard ceiling rather than
    /// a trigger" ever since — with no number, which is the same as no priority.
    ///
    /// **A ceiling is not implementable where this trigger lives.** Refusing the
    /// allocation would turn a servable request into an `OutOfMemoryError`, and
    /// collecting on the spot needs a safepoint the allocation path cannot take.
    /// A real ceiling therefore needs an allocation-site safepoint poll, which is
    /// a VM-wide change and not a collector one. This measures what that change
    /// would be worth: an overshoot that is a few percent of the budget prices
    /// the item at nothing, and one that is a multiple of it prices it high.
    ///
    /// Reported beside the budget it is measured against, because "17 MB over" is
    /// meaningless without it.
    pub gen_nursery_overshoot_max: AtomicUsize,
    /// Collections the ALLOCATION-RATE clause of `needs_gc` asked for
    /// (`zgc_alloc_trigger_percent`).
    ///
    /// The engagement counter for F1: this at zero on a default run means every
    /// collection still came from the 75%-of-`-Xmx` clause or from
    /// `headroom_low`, i.e. the budget is above what the workload allocates
    /// between the collections it was already having and the change measures
    /// nothing. Counted at the POLL, not at the collection, so a trigger that
    /// fires and is then overtaken by another clause still shows.
    pub alloc_trigger_fires: AtomicUsize,
    /// Microseconds the LAST collection spent in the sweep phase.
    ///
    /// The same figure `[GC] zgc-pause:` reports as `sweep_us`, kept where a
    /// test can read it. `measure_the_sharded_sweep` needs the PHASE and not
    /// the whole collection: the mark is serial in every arm, so timing the
    /// collection dilutes whatever the sweep does by a constant, and the
    /// alternative -- scraping the log line -- makes a measurement depend on
    /// logging being on.
    pub last_sweep_us: AtomicU64,
    /// Cycles in which the PAUSE TARGET was unreachable: the live set alone
    /// projects a pause past the target, or what the target leaves after it is
    /// below `ZGC_ALLOC_TRIGGER_FLOOR`.
    ///
    /// The distinguishing counter for the pause-target loop. A small budget is
    /// what a working loop produces on a workload with big pauses, and it is
    /// ALSO what a loop produces when it has given up and fallen back to the
    /// floor. Those are opposite situations -- one says the target is being
    /// met, the other says it cannot be -- and without this they report the
    /// same `alloc_trigger=` line.
    pub pause_target_unreachable: AtomicUsize,
    /// CONSECUTIVE unreachable verdicts, reset by any cycle that finds a usable
    /// budget.
    ///
    /// Separate from the cumulative counter above because the two answer
    /// different questions and one of them is load-bearing.
    /// `refresh_pause_target_budget` gives up on the target entirely -- it
    /// stores `pause_target_ms = 0`, permanently, for the life of the process --
    /// after three verdicts, and its own comment says why three: *"a single
    /// verdict can come from a transient live-set spike ... Three CONSECUTIVE
    /// failures to find ANY budget that meets the target is a property of the
    /// workload, not of a bad cycle."*
    ///
    /// It was testing the CUMULATIVE counter, which is never reset. So three
    /// isolated spikes an hour apart, in a run that met its target on every
    /// cycle in between, disabled the loop for good -- and silently, because
    /// the only externally visible difference is that `pause_target=0ms` starts
    /// appearing on the `zgc-pause` line that nobody reads on a healthy run.
    /// The argument in the comment is the right one; this field is what makes
    /// the code implement it.
    pub pause_target_unreachable_streak: AtomicUsize,
    /// Cycles that overran the target while already at the floor budget, since
    /// the last cycle that met the target. See
    /// `ZgcRealHeap::refresh_pause_target_budget` and
    /// `ZGC_PAUSE_FLOOR_OVERRUNS_TO_GIVE_UP`. Not reset by the `Some(b)` arm,
    /// unlike `pause_target_unreachable_streak`.
    pub pause_target_floor_overruns: AtomicUsize,
    /// `CRATONVM_ZGC_PAUSE_FLOOR_VERDICT`, read at construction.
    pub pause_floor_verdict_enabled: AtomicBool,
    /// `CRATONVM_ZGC_WARMUP`, read at construction; see [`ZGC_WARMUP_CYCLES`].
    pub warmup_enabled: AtomicBool,
    /// zr z1/tr: the HotSpot-shaped rules ([`director::ZDirector`]), sampled
    /// under `CRATONVM_ZGC_RATE_TRIGGER=1` (deciding) or
    /// `CRATONVM_ZGC_TRIGGER_SHADOW=1` (recording only); inert otherwise.
    pub director: director::ZDirector,
    /// zr z6/tp: `CRATONVM_ZGC_NEAR_FULL_PROGRESS` (percent of the free space
    /// after a collection that must be allocated before an occupancy-triggered
    /// collection while the live set is above the threshold; default
    /// [`director::REARM_NEAR_FULL_DEFAULT_PERCENT`], `0` = the legacy
    /// `free / 4` floor everywhere), read at construction. See
    /// [`director::rearm_floor`].
    pub near_full_progress_pct: AtomicUsize,
    /// zr z6/tp: the floor the pre-z6 rule would have published this cycle
    /// (`live + max(free / 4, 64 KiB)`), kept beside `gc_rearm` so `needs_gc`
    /// can count the intervals the near-full floor held back
    /// (`pace_floor_deferred=`).
    pub gc_rearm_legacy: AtomicUsize,
    /// zr z6/tp: the `[GC] zgc-pacing:` census.
    pub pacing: director::ZPacingCensus,
    /// zs s6/no: `CRATONVM_ZGC_EXHAUSTED_FREE_FLOOR` in bytes (`0` = off),
    /// read at construction; see [`ZgcRealHeap::arm_exhausted_verdict`].
    pub exhausted_free_floor: AtomicUsize,
    /// zs s6/no: exhausted verdicts armed (refusal collections that left
    /// under the floor free; `exhausted_armed=`).
    pub exhausted_verdicts_armed: AtomicUsize,
    /// zs s6/no: fallible attempts the verdict refused (`exhausted_refused=`).
    pub exhausted_verdict_refusals: AtomicUsize,
    /// zs s9/no2: a verdict already ended a fill in an `OutOfMemoryError`
    /// (set by [`ZgcRealHeap::lift_exhausted_verdict`] on a live verdict);
    /// no verdict arms again until a collection leaves the floor free.
    pub exhausted_spent: AtomicBool,
    /// zs s9/no2 (rv6 X1): this heap's key in `ZGC_EXHAUSTED_VERDICT`, a
    /// serial from [`ZGC_HEAP_SERIAL`] (never 0).
    pub exhausted_verdict_key: usize,
    /// zs s9/no2: verdicts spent that way (`exhausted_spent=`).
    pub exhausted_spent_count: AtomicUsize,
    /// zr zs s1/tp: `CRATONVM_ZGC_DIRECTOR_REARM_FLOOR`, read at construction.
    pub director_rearm_floor: AtomicBool,
    /// zr zs s1/tp: intervals in which a director rule said yes and the
    /// anti-storm floor held it back (`director_deferred=` on
    /// `[GC] zgc-trigger:`; the engagement gauge, 0 at `=0`).
    pub trigger_director_deferred: AtomicUsize,
    /// zr zs s1/tp: `CRATONVM_ZGC_WARMUP_HOTSPOT_STEPS`, read at construction.
    pub warmup_hotspot_steps: AtomicBool,
    /// zt t1/tp: `CRATONVM_ZGC_RATE_EXCLUDES_PAUSE`, read at construction
    /// ([`zgc_rate_excludes_pause_enabled`]).
    pub rate_excludes_pause: AtomicBool,
    /// zt t1/tp: `CRATONVM_ZGC_SOFT_MAX_PACES` (default ON, `=0` kill), read
    /// at construction.
    pub soft_max_paces: AtomicBool,
    /// zt t29/sm: the ergonomic soft maximum as REPORTED (`0`: none), when
    /// it does not pace ([`zgc_soft_max_ergonomic_pacing_enabled`]); an
    /// explicit or management write clears it.
    pub soft_max_report: AtomicUsize,
    /// zt t1/tp: the live-bytes threshold of the soft maximum
    /// ([`ZgcRealHeap::pacing_threshold`]); `usize::MAX` (none) unless
    /// [`Self::soft_max_paces`] is on and `-XX:SoftMaxHeapSize` is below the
    /// capacity.
    pub soft_max_threshold: AtomicUsize,
    /// zr zs s1/tp: collections whose trigger mask carried the warm-up bit --
    /// HotSpot's `ZStatCycle` warm-up count, which a `System.gc()` or an
    /// allocation failure does not advance (`gc_count` does).
    pub warmup_cycles_done: AtomicUsize,
    /// zr z1/tr: `CRATONVM_ZGC_HOTSPOT_GC_CAUSE`, read at construction.
    pub hotspot_gc_cause: AtomicBool,
    /// zr z2/t2: `CRATONVM_ZGC_STALL_CAUSE_FROM_TRIGGER`, read at construction.
    pub stall_cause_from_trigger: AtomicBool,
    /// zr z2/t2: `CRATONVM_ZGC_YOUNG_POOL_SINCE_CYCLE`, read at construction.
    pub young_pool_since_cycle: AtomicBool,
    /// zr z1/tr: the trigger mask of the most recent collection, kept after
    /// `trigger_seen` is cleared so a reader after the cycle can still name
    /// its cause.
    pub last_cycle_trigger_mask: AtomicUsize,
    /// Collections the warm-up clause admitted (`warmup=` on `[GC] zgc-trigger:`).
    pub trigger_warmup: AtomicUsize,
    /// zr z1/tr: collections a [`director`] rule admitted (deciding arm
    /// only), once per collection on the `trigger_seen` latch.
    pub trigger_director: AtomicUsize,
    /// zt t21/tg10 (`CRATONVM_ZGC_TICK_WITHDRAW_KEEPS_POLL_PROACTIVE`): the
    /// collection count + 1 (as [`timer::ZTimer::claim_still_owned`] stamps
    /// it; 0 = never) at the last allocation poll whose director asked for
    /// `Proactive` while the bit was already set -- a poll that co-claims the
    /// pending collection the tick's request charged. The tick's withdrawals
    /// leave such a bit on the latch: it is the poll's cause too.
    pub proactive_poll_co_claim: AtomicUsize,
    /// zr z3/cs: the every-trigger state ([`ZConcTriggerState`];
    /// `CRATONVM_ZGC_CONC_EVERY_TRIGGER=1`, opt-in).
    pub conc_trigger: ZConcTriggerState,
    /// zt t21/zc2: the deferred sweep (`CRATONVM_ZGC_CONCURRENT_SWEEP`,
    /// opt-in; `concurrent_sweep::ZConcSweepState`).
    pub csweep: concurrent_sweep::ZConcSweepState,
    /// zt t22/zc3: the hidden-referent protocol and the VM reference round's
    /// census (`CRATONVM_ZGC_CONCURRENT_REFS`, opt-in;
    /// `concurrent_refs::ZConcRefsState`).
    pub crefs: concurrent_refs::ZConcRefsState,
    /// zt t31/zc11 (`CRATONVM_ZGC_CONCURRENT_REF_ROUND`): the round after the
    /// release and the sweep-install hold (`concurrent_ref_round`).
    pub crround: concurrent_ref_round::ZRefRoundState,
    /// zt t31/zc11 (R4, `CRATONVM_ZGC_REMAP_IN_MARK`): the kept cycle for the
    /// hot paths, the slide's skip color and the census (`remap_in_mark`).
    pub rim: remap_in_mark::ZRemapInMarkState,
    /// zt t31/zc11 (R4): the mark color the last remap flip left (`0`: none
    /// since the last slide or mark start); taken by the slide.
    pub rim_last_mark_color: AtomicU64,
    /// zt t32 (section 6a, R4 in generational mode): this collection is a
    /// YOUNG cycle (set where `collect_garbage` decides it), and a slide kept
    /// an R4 cycle on a generational heap, which only a MAJOR mark end may
    /// release (a young mark does not visit the old generation's slots).
    pub rim_cycle_young: AtomicBool,
    pub rim_keep_until_major: AtomicBool,
    /// zt t32 (R6, `CRATONVM_ZGC_CONCURRENT_RELOCATE`): the published plan and
    /// the census (`concurrent_relocate`).
    pub crl: concurrent_relocate::ZConcRelocState,
    /// zt t23/zc6: the closing pause's census and the mark-end service
    /// (`CRATONVM_ZGC_MARK_END_SERVICE`, opt-in; `mark_end::ZMarkEndState`).
    pub mend: mark_end::ZMarkEndState,
    /// zt t26/zc9: the allocation-rate start of an every-trigger cycle
    /// (`CRATONVM_ZGC_CONCURRENT_RATE_START`, opt-in; `rate_start::ZRateStartState`).
    pub rstart: rate_start::ZRateStartState,
    /// zt t27/zc10: the member-class table in front of the referent-skip Bloom
    /// filter (`CRATONVM_ZGC_MARK_SKIP_CLASS_FILTER`, default ON;
    /// `skip_class_filter::ZSkipClassFilter`).
    pub skipcls: skip_class_filter::ZSkipClassFilter,
    /// zt t31/zc11 (`CRATONVM_ZGC_MARK_SKIP_BITS`, default ON, `=0` kill):
    /// the open cycle's skip set as a member bitmap, asked before the class
    /// table and the Bloom filter (`skip_bits`).
    pub skipbits: std::sync::Arc<skip_bits::ZSkipBits>,
    pub weak_phantom_class_kind: Box<[std::sync::atomic::AtomicU8]>,
    pub weak_phantom_class_overflow: parking_lot::RwLock<FxHashMap<u32, u8>>,
    pub discovered_weak_phantom: Box<[Mutex<Vec<(usize, u32, u8)>>; 16]>,
    pub finalized_weak_phantom: Mutex<Vec<(usize, u32, u8, Option<usize>, bool)>>,
    /// Fixed-point waits the mark driver served by TIMING OUT rather than by
    /// being woken, summed across every cycle — see
    /// [`mark::ZMarkTerminator::park_timeouts`].
    ///
    /// The production reader for that counter. It is here rather than only in a
    /// test because the failure it detects is a *slowdown*: a driver whose wait
    /// is never notified behaves identically to one whose wait is, only
    /// `Z_MARK_PARK_POLL_MS` slower per pass, and it took months to notice. A
    /// nonzero here on a stop-the-world run means a notification is missing
    /// again, and it is a count — so it says so on a loaded host, where a per-cycle
    /// timing could not.
    pub mark_park_timeouts: AtomicUsize,
    /// zr zs s1/mc (`CRATONVM_ZGC_MARK_CPU_ACCOUNTING`): thread CPU nanoseconds,
    /// cumulative, of the stop-the-world mark pool's workers, of the serial
    /// marker, and of the concurrent pool's workers (phase and mark-end drain
    /// together). See [`ZgcRealHeap::mark_cpu_totals`].
    pub mark_cpu_stw_pool_ns: AtomicU64,
    pub mark_cpu_serial_ns: AtomicU64,
    pub mark_cpu_conc_pool_ns: AtomicU64,
    /// The part of `mark_cpu_conc_pool_ns` spent on cycles that were
    /// abandoned (`abandon_concurrent_mark`): CPU with no mark set to show.
    pub mark_cpu_conc_abandoned_ns: AtomicU64,
    /// zs s2/mc2: thread CPU nanoseconds, cumulative, of the COLLECTING
    /// thread across the stop-the-world parallel mark (skip set, bloom,
    /// driver waits and handshakes): the mark's share of `java_cpu_ms`. See
    /// [`ZgcRealHeap::mark_cpu_stw_driver`].
    pub mark_cpu_stw_driver_ns: AtomicU64,
    /// zs s3/ac: the rest of a collection's CPU, so that "collection CPU /
    /// process CPU" comes from one run's stderr (`[GC] zgc-cpu:` at shutdown,
    /// `[GC] zgc-pause-cpu:` per cycle). Thread CPU nanoseconds, cumulative,
    /// all under `CRATONVM_ZGC_MARK_CPU_ACCOUNTING` (`=0`: every one stays 0).
    /// See [`ZgcRealHeap::collection_cpu_totals`] for which of them nest.
    ///
    /// The sweep pool's SPAWNED workers (`sweep_parallel`, the complement
    /// arm's shards); the serial sweep runs on the collecting thread and is
    /// in `pause_thread_cpu_ns`.
    pub sweep_pool_cpu_ns: AtomicU64,
    /// The SPAWNED pullers of every `zgc_par_chunks` pass (slide census,
    /// copy, rewrite, verification); the caller's own pull is in
    /// `pause_thread_cpu_ns`.
    pub relocate_pool_cpu_ns: AtomicU64,
    /// zt t3/mk (X-rl2, item 3 of `zgc-zs3ac-collection-cpu-residual-shares`):
    /// the `ZUncommitter` thread's CPU, one span per pass. HotSpot's
    /// `ZUncommitter` is a GC thread (`ConcurrentGCThread`), so its CPU is
    /// collection CPU (`uncommit_ms=` on `[GC] zgc-cpu:`).
    pub uncommit_cpu_ns: AtomicU64,
    /// zt t6/ob4 (item 2 of `zgc-zs3ac-collection-cpu-residual-shares`):
    /// the VM's side of each collection pause on the collecting thread --
    /// the safepoint request and rendezvous before `collect_garbage` and the
    /// common stop-the-world epilogue after it -- booked by the VM's door
    /// ([`ZgcRealHeap::book_vm_pause_cpu`]), less this heap's own
    /// `pause_thread_cpu_ns` share of the same span, so it is disjoint.
    pub vm_pause_cpu_ns: AtomicU64,
    /// zt t3/mk (item 1 of `zgc-zs3ac-collection-cpu-residual-shares`): the
    /// SATB buffer FLUSHES' CPU (`hand_thread_batch_to_the_marker`,
    /// `hand_satb_batch_to_the_marker`), on MUTATOR threads, one clock pair
    /// per flush (every 256 recorded stores per thread). Printed as
    /// `satb_flush_ms=` on `[GC] zgc-cpu:` but NOT in `collection_ms`: it is
    /// Java-thread CPU (term T2 of the every-trigger proposal), and the
    /// per-store half of the barrier is not in it.
    pub satb_flush_cpu_ns: AtomicU64,
    /// The collecting thread across the whole of `collect_garbage`. It
    /// CONTAINS `mark_cpu_stw_driver_ns` and `mark_cpu_serial_ns` (both run
    /// on that thread), and the serial sweep and slide.
    pub pause_thread_cpu_ns: AtomicU64,
    /// Collections whose `pause_thread_cpu_ns` share was booked.
    pub pause_cpu_cycles: AtomicU64,
    /// zs s3/ac (`CRATONVM_ZGC_SATB_DIRECT_HANDOFF`, default ON, `=0`
    /// kill): a full SATB thread buffer goes straight to the concurrent
    /// pool's `push_mutator_batch` when a pool exists, instead of through
    /// `mark_ingress` and a drain of all of its buckets. Read at construction.
    pub satb_direct_handoff: AtomicBool,
    /// t1 wave 3 lane Q (`t1-R-zgc-gate-block-out-of-rip-reach`): this heap's
    /// code-adjacent COPY of `ZgcRealHeap::jit_ref_store_gates`, or null. The Box
    /// lives on the Rust heap, out of disp32 reach of compiled code, so every
    /// fused gate took `MOV R11, imm64; CMP [R11]`. The VM places one cell per
    /// heap (`ZgcRealHeap::place_jit_ref_store_gates`) before its first compile; every
    /// gate write goes to both blocks (`ZgcRealHeap::store_jit_gate`), and
    /// `ZgcRealHeap::jit_ref_store_plan` names the copy once placed. Per heap, never
    /// freed (a cell is never unmapped), so its address may be baked.
    pub jit_gate_cell: std::sync::atomic::AtomicPtr<cratonvm_gc_core::jit_bounds::JitRefStoreGates>,
    /// zr z2/cc (`CRATONVM_ZGC_SATB_THREAD_BUFFERS`, default ON): the SATB
    /// barrier records into per-thread buffers. Per heap since t1 wave 2 lane
    /// Z (was a process `OnceLock`); seeded at construction.
    pub satb_thread_buffers_on: AtomicBool,
    /// Addresses handed over by that direct arm, cumulative (its engagement
    /// counter; also counted in `conc_ingress_replayed`, whose meaning is
    /// unchanged).
    pub satb_direct_handed: AtomicUsize,
    /// zt t1/mk (`CRATONVM_ZGC_FULL_REQUEST_FRESH_MARK`, default ON, `=0`
    /// kill; read at construction): a collection run for a FULL request
    /// (`System.gc()`, JVMTI `ForceGarbageCollection`, a VM-internal full
    /// collection -- the doors that set `gc_quiescence::request_major_gc` on
    /// the collecting thread) while a concurrent cycle is open ABANDONS that
    /// cycle and marks from scratch, instead of closing it with its
    /// snapshot-at-the-beginning mark set. See `collect_garbage`, beside
    /// `finish_concurrent_mark`'s call.
    pub full_request_fresh_mark: AtomicBool,
    /// Concurrent cycles abandoned that way, cumulative (the engagement
    /// counter; `[GC] zgc-conc-abandon:` per cycle under `--verbose:gc`).
    pub full_request_conc_abandons: AtomicUsize,
    /// zt t24/hunt7 (`CRATONVM_ZGC_OOM_LADDER_FRESH_MARK`): whether the last
    /// collection `collect_garbage` ran closed a concurrent cycle with that
    /// cycle's mark set (`marked_concurrently`), i.e. its mark STARTED before
    /// the collection was asked for. Written at every collection's mark end
    /// (both sweep paths pass there). Read by the allocation ladder's
    /// clear-soft rung (`ZgcRealHeap::last_collection_marked_concurrently`).
    pub last_collection_marked_concurrently: AtomicBool,
    /// zt t1/mk: whether the OPEN concurrent cycle's mark start armed the
    /// extra-root filter (`conc_root_filter_armed()` latched there), which is
    /// what the mark-end harvest must follow. See `start_concurrent_mark`.
    pub conc_cycle_root_filter: AtomicBool,
    /// Survivors a relocating cycle promoted by moving them below the nursery
    /// floor, cumulative.
    ///
    /// The engagement counter for G2's promotion: a run with `compaction_cycles`
    /// greater than zero and this at zero is one where the slide ran and the
    /// nursery was not re-established behind it, which is the state before
    /// 2026-08-17 (the floor was dropped and the next cycle forced to be a
    /// major).
    pub gen_promotions_by_slide: AtomicUsize,
    /// Registered objects a young cycle's sweep did NOT visit, cumulative.
    ///
    /// The engagement counter for the nursery floor, and it exists because the
    /// floor is exactly the kind of feature that can be on and inert: if
    /// `gen_young_floor` never advances past 0, `for_each_base_from(0, ..)` is the
    /// unbounded loop and every test still passes. A run with `young_cycles > 0`
    /// and `sweep_skipped = 0` swept the whole registry on every young cycle,
    /// which is the state the 2026-08-17 measurement was in.
    pub gen_sweep_skipped: AtomicUsize,
    /// Young (minor) collections completed, and minor cycles since the last
    /// whole-heap one -- the counter [`zgc_gen_minors_per_major`] bounds.
    pub young_cycles: AtomicUsize,
    /// Objects a young cycle retained WITHOUT tracing, because they are old.
    ///
    /// This is the number the whole phase is for: it counts the tracing a young
    /// cycle did not do. A generational run whose `gen_old_retained` is zero is
    /// doing full-heap work under a generational name, which is the vacuous
    /// green this tree keeps catching -- so it is on the `--verbose:gc` line and
    /// asserted by a test, not merely available.
    pub gen_old_retained: AtomicUsize,
    /// Extra roots a young cycle took from the remembered set.
    pub gen_remembered_roots: AtomicUsize,
    /// Objects promoted, and cards written after a relocation.
    pub gen_promotions: AtomicUsize,
    /// BYTES promoted, cumulative — the producer for
    /// [`ZgcRealHeap::bytes_promoted_total`], which is this collector's arm of
    /// `VmHeap::bytes_promoted_total`.
    ///
    /// Separate from `gen_promotions` because the consumer needs bytes: the
    /// GC-overhead productivity metric (`interpreter::note_gc_productivity`)
    /// credits a cycle that "conserves live bytes but did useful
    /// allocation-enabling work", and it subtracts byte quantities. A count of
    /// objects cannot be substituted for one, so before this counter existed
    /// the dispatcher answered a hard `0`
    /// (`docs/feature-designs/zgc-architecture.md`).
    pub gen_promoted_bytes: AtomicUsize,
    pub gen_recards_after_relocation: AtomicUsize,
    /// zr z1/gn: the OLD-START BITMAP -- one bit per object start, set for
    /// exactly the registered bases whose header age is at or above the
    /// promotion age. See [`ZgcRealHeap::premark_from_old_bits`] for what it
    /// replaces and [`ZgcRealHeap::note_old_start`] for the invariant.
    /// Allocated on first use (it is 1/64th of the arena, zero-page mapped),
    /// so a run that never turns it on pays nothing.
    pub gen_old_bits: std::sync::OnceLock<ZObjectStartBits>,
    /// `CRATONVM_ZGC_GEN_OLD_BITMAP` for this heap (default ON inside
    /// generational mode since zr z2/g2).
    pub gen_old_bits_on: AtomicBool,
    /// zr z3/rv3 (`CRATONVM_ZGC_GEN_BITMAP_FALLBACK_MAJOR`, default ON): with
    /// the bitmap ON but not usable (invalidated, or no side mark bits), a
    /// would-be young cycle runs whole-heap instead of falling back to the
    /// header walk + floor-bounded sweep -- the arm that leaves a dead young
    /// parent registered over a freed child. Per heap so a test drives both.
    pub gen_bitmap_fallback_major: AtomicBool,
    /// Young cycles that rule turned whole-heap (zr z3/rv3).
    pub gen_old_bits_young_refusals: AtomicUsize,
    /// Is [`Self::gen_old_bits`] exact right now? Lowered by every event that
    /// can make a header age disagree with the bitmap (mode toggles, a
    /// promotion-age change, the generational reset) and by the start of a
    /// whole-heap collection, which rebuilds it from its own sweep and raises
    /// it again at the end. A young cycle consumes the bitmap only while this
    /// is up; otherwise it takes the per-object header walk (counted in
    /// `gen_old_bits_fallbacks`).
    pub gen_old_bits_valid: AtomicBool,
    /// A whole-heap collection cleared the bitmap and its sweep is refilling
    /// it; raised by `begin_old_bits_rebuild`, consumed at the major's end.
    pub gen_old_bits_rebuilding: AtomicBool,
    /// Young cycles pre-marked from the bitmap / young cycles that wanted to
    /// but found it not valid.
    pub gen_old_bits_premarks: AtomicUsize,
    pub gen_old_bits_fallbacks: AtomicUsize,
    /// `CRATONVM_ZGC_GEN_OLD_BITMAP_VERIFY=1` cross-check, cumulative:
    /// `unsound` = pre-marked from the bitmap but YOUNG by header (a young
    /// object retained untraced -- its young children can be freed under it);
    /// `imprecise` = old by header but not in the bitmap (traced as young.
    /// zr z3/rv3: NOT merely work under the young-aged sweep, which visits
    /// `registered & !old` -- such an object reached only through an untraced
    /// old holder whose card was dropped is freed under it. Keep it 0 too).
    pub gen_old_bits_verify_unsound: AtomicUsize,
    pub gen_old_bits_verify_imprecise: AtomicUsize,
    /// `CRATONVM_ZGC_GEN_YOUNG_AGED_SWEEP` for this heap (default ON, but it
    /// only engages together with a valid old-start bitmap).
    pub gen_young_aged_sweep_on: AtomicBool,
    /// Young cycles swept by `sweep_young_aged`, and how many objects BELOW
    /// the nursery floor that sweep visited / freed (cumulative). The freed
    /// ones are exactly the objects the floor-bounded sweep leaves unmarked,
    /// unswept and still registered -- see
    /// `docs/feature-designs/zgc-architecture.md`.
    pub gen_young_aged_sweeps: AtomicUsize,
    pub gen_below_floor_visited: AtomicUsize,
    pub gen_below_floor_freed_bytes: AtomicUsize,
    /// Bytes of objects promoted AT OR ABOVE the floor by young cycles since
    /// the floor last moved. `sweep_young_aged` does not visit old objects, so
    /// the live figure has to carry these the way `gen_old_live_bytes`
    /// carries the region below the floor.
    pub gen_nursery_old_bytes: AtomicUsize,
    /// The AGE-FILTERED nursery census (`CRATONVM_ZGC_NURSERY_CENSUS_AGE=1`):
    /// the same per-page instrument as `gen_nursery_survivors`, fed only with
    /// survivors that were YOUNG when the cycle started. The experiment
    /// `measurement-nursery-survivor-census-20260921.md` names as the one that
    /// could still overturn proposal D.
    pub gen_nursery_survivors_young: Box<generation::ZNurserySurvivorCensus>,
    pub gen_census_age_on: AtomicBool,
    /// zr z2/g2: per-heap seeds of four generational switches, so a test can
    /// drive both arms in one process (the readers cache in `OnceLock`s).
    /// `CRATONVM_ZGC_NURSERY_CENSUS` (default on since z2/g2),
    /// `CRATONVM_ZGC_GEN_YOUNG_RELOCATE` (default OFF since z2/g2),
    /// `CRATONVM_ZGC_GEN_YOUNG_COMPLEMENT` (default ON since zt t14/gn5) and
    /// `CRATONVM_ZGC_GEN_MAJOR_PROMOTED_PERCENT` (default 0 = off).
    pub gen_census_on: AtomicBool,
    /// `CRATONVM_ZGC_GEN_ELEMENT_CARDS` (default OFF, opt-in): element
    /// (granule) cards for large reference arrays -- see the section note
    /// above `note_ref_store_element`. Gates the producer AND the consumer.
    pub gen_element_cards_on: AtomicBool,
    /// Granule cards the young cycles scanned / granules carded when an object
    /// card on a large array was converted (cumulative).
    pub gen_element_granules_scanned: AtomicUsize,
    pub gen_element_cards_converted: AtomicUsize,
    pub gen_young_relocate_on: AtomicBool,
    /// t1 wave 6 lane Y4 (`CRATONVM_ZGC_GEN_YOUNG_EVACUATE`, opt-in): the
    /// nursery-scoped young slide's per-heap seed and engagement counters
    /// (`young_evacuate`). Here, not in the hot heap struct.
    pub young_evac: young_evacuate::ZYoungEvacCounters,
    pub gen_young_complement_on: AtomicBool,
    pub gen_major_promoted_percent: AtomicUsize,
    /// Young cycles swept by `sweep_young_complement`, and young cycles that
    /// asked for it but were refused (`gen_young_complement_refusal` names the
    /// last reason). `sweeps=0` with the switch on and `young_cycles>0` is a
    /// vacuous row.
    pub gen_young_complement_sweeps: AtomicUsize,
    pub gen_young_complement_refusals: AtomicUsize,
    pub gen_young_complement_refusal: AtomicUsize,
    /// zt t6/tc (GN-17): `CRATONVM_ZGC_GEN_YOUNG_SWEEP_WORKERS` (default 1 =
    /// serial, opt-in) -- the young-aged sweep's shard count, per heap; and
    /// its engagement counters, the young-aged sweeps that ran on more than
    /// one shard and the shards they used (`[GC] zgc-gen-cost:`
    /// `young_sweep_sharded=` / `young_sweep_shards=`). See
    /// `ZgcRealHeap::sweep_young_aged_sharded`.
    pub gen_young_sweep_workers: AtomicUsize,
    pub gen_young_sweep_sharded: AtomicUsize,
    pub gen_young_sweep_shards: AtomicUsize,
    /// zt t6/tc (GN-5): is THIS cycle's sweep a `System.gc()` major that
    /// tenures every survivor (`age_survivor` ages straight to the promotion
    /// age)? Raised by `collect_garbage` just before the sweep of a
    /// whole-heap generational cycle whose collecting thread carries
    /// `gc_quiescence::system_gc_tenure_requested()` (and
    /// `CRATONVM_ZGC_GEN_SYSTEM_GC_TENURES`, default ON in the mode, `=0`
    /// kill), lowered right after the sweep. And the count of such sweeps.
    pub gen_tenure_all_cycle: AtomicBool,
    pub gen_tenure_all_sweeps: AtomicUsize,
    /// zt t7/tg (GN-12): the census GN-12's rows read -- young cycles that
    /// freed under 1 % of the capacity ([`gen_minor_is_weak`]), the current
    /// and the longest run of such minors in a row (a whole-heap cycle ends a
    /// run), and the majors the opt-in weak-minor escalation forced.
    pub gen_weak_minors: AtomicUsize,
    pub gen_weak_minor_run: AtomicUsize,
    pub gen_weak_minor_max_run: AtomicUsize,
    pub gen_weak_minor_escalations: AtomicUsize,
    /// zt t7/tg (GN-12): `CRATONVM_ZGC_GEN_ESCALATE_WEAK_MINOR=1` (opt-in), per
    /// heap: a young cycle that freed under an eighth of the nursery budget
    /// forces the next cycle to a major ([`gen_escalate_after_minor`]).
    pub gen_escalate_weak_minor: AtomicBool,
    /// zt t6/tc: who set `-XX:SoftMaxHeapSize` last, for the manageable JMX
    /// flag row's origin: 0 = nobody (the ergonomic default, the capacity),
    /// 1 = the command line (`set_soft_max_heap_bytes`), 2 = management
    /// (`set_soft_max_heap_bytes_by_management`).
    pub soft_max_origin: std::sync::atomic::AtomicU8,
    /// zt t7/tg: the `Timer` rule's clock and the director tick's state
    /// (`-XX:ZCollectionInterval`, `CRATONVM_ZGC_DIRECTOR_IDLE_TICK`).
    pub timer: timer::ZTimer,
    /// Bytes promoted since the last whole-heap cycle, and the majors the
    /// old-growth rule forced (`gen_major_by_old_growth`).
    pub gen_promoted_since_major: AtomicUsize,
    pub gen_majors_by_old_growth: AtomicUsize,
    /// zt t14/gn5: the part of `gen_promoted_since_major` the last whole-heap
    /// cycle seeded it with (that cycle's own promotions, every survivor of a
    /// tenuring `System.gc()`): objects live at that major. See
    /// [`ZgcRealHeap::gen_empty_minor_escalation_skipped`].
    pub gen_promoted_at_major: AtomicUsize,
    /// zt t14/gn5: `CRATONVM_ZGC_GEN_EMPTY_MINOR_NEEDS_PROMOTION` (default ON
    /// inside the generational mode, `=0` kill; latched at construction, an
    /// atomic so tests can flip it), and the escalations it withheld
    /// (`[GC] zgc-gen-cost:` `empty_minor_escalations_skipped=`).
    pub gen_empty_minor_needs_promotion: AtomicBool,
    pub gen_empty_minor_escalations_skipped: AtomicUsize,
    /// zt t16/gn6 (item 11): did the LAST collection run as a young cycle on
    /// the young-aged sweep? Stored by every young cycle, lowered by every
    /// whole-heap one (unlike `gen_last_young_xlog_exact`, which a major
    /// leaves standing). Read by [`ZgcRealHeap::jmx_young_old_split`] unless
    /// `CRATONVM_ZGC_GEN_JMX_YOUNG_BELOW_FLOOR=0` (default ON since zt t26/rt2).
    pub gen_last_cycle_young_aged: AtomicBool,
    /// zt t21/pl10 (item 11's second residual): the young-by-age survivor
    /// bytes below the large-object region of the LAST collection, when it was
    /// a whole-heap cycle (`ZSweepShard::gen_young_low_bytes`); 0 after a young
    /// cycle, which stores 0. They are inside `gen_old_live_bytes`; read by
    /// [`ZgcRealHeap::jmx_young_old_split`] under
    /// `CRATONVM_ZGC_GEN_JMX_MAJOR_YOUNG` (default ON in the mode, `=0` kill).
    pub gen_last_major_young_bytes: AtomicUsize,
    /// zt t16/gn6 (item 12): did the LAST collection (a latched failure makes
    /// it whole-heap) begin with an allocation failure latched (`hard_alloc_failure`, the copy
    /// `cycle_after_alloc_failure` takes, which the relocation selector
    /// consumes)? HotSpot 25's `is_alloc_stalling_for_old` clause of
    /// `should_preclean_young`; read by the `-Xlog` logger under
    /// `CRATONVM_ZGC_XLOG_STALL_PRECLEAN=1`.
    pub gen_last_major_alloc_stalled: AtomicBool,
    /// zt t10/gn2: bytes the LAST young cycle promoted (stored by every young
    /// cycle, 0 when it promoted nothing), for the `-Xlog` `y:` `Young
    /// Generation Statistics` table ([`ZgcRealHeap::gen_last_young_xlog_figures`]).
    pub gen_last_young_promoted_bytes: AtomicUsize,
    /// zt t11/gn3 (x8 C8 / hand-off G1): the young survivors BELOW the floor
    /// the last young-aged-sweep young cycle kept young (`below_copied -
    /// below_promoted`). They stay inside `gen_old_live_bytes`, so the `-Xlog`
    /// young table subtracts them from its old-generation figure. 0 after a
    /// young cycle on any other sweep arm.
    pub gen_last_young_below_young_bytes: AtomicUsize,
    /// zt t11/gn3 (x8 C8): did the last young cycle take the young-aged sweep
    /// (the only arm whose old figure the `-Xlog` young split can trust)?
    pub gen_last_young_xlog_exact: AtomicBool,
    /// zt t3/gn: `CRATONVM_ZGC_GEN_FULL_REQUEST_MAJOR` (default ON inside the
    /// generational mode, `=0` kill) -- a full request (`System.gc()`, `jcmd
    /// GC.run`: `gc_quiescence::request_major_gc` on the collecting thread)
    /// makes the collection whole-heap. See the `force_major` block of
    /// `collect_garbage`.
    pub gen_full_request_major: AtomicBool,
    /// zt t3/gn: collections a full request made whole-heap on a heap that
    /// had an old generation (the switch's engagement counter).
    pub gen_majors_by_request: AtomicUsize,
    /// zt t11/gn3: `CRATONVM_ZGC_GEN_MAJOR_RULE_CAUSES` (default ON inside
    /// the generational mode, `=0` kill; latched at construction, an atomic
    /// so tests can flip it) -- a collection whose HotSpot cause
    /// ([`hotspot_cause_for_mask`] of its trigger mask) is `Warmup` or
    /// `Proactive` is whole-heap. See [`gen_mask_is_major_rule`].
    pub gen_major_rule_causes: AtomicBool,
    /// zt t11/gn3: collections that switch made whole-heap on a heap that had
    /// an old generation, no earlier reason having decided it (the
    /// engagement counter, `[GC] zgc-gen-cost:` `majors_by_rule_cause=`).
    pub gen_majors_by_rule_cause: AtomicUsize,
    /// zt t7/cc: `CRATONVM_ZGC_GEN_YOUNG_SKIP_REFERENCES` (default on since
    /// zt t8, `=0` kill; latched at construction, an atomic so tests can flip it). See
    /// [`ZgcRealHeap::arm_young_refs_strong`].
    pub gen_young_skip_refs_on: AtomicBool,
    /// zt t9/gn: `CRATONVM_ZGC_GEN_YOUNG_PREDICT_EXACT` (default on, `=0`
    /// kill; latched at construction) -- the young-cycle prediction evaluates
    /// the opt-in major rules instead of failing closed on them. See
    /// `ZgcRealHeap::young_cycle_predicted`.
    pub gen_young_predict_exact: AtomicBool,
    /// zt t7/cc: set by the VM's pre-collection pass when it left every
    /// referent slot in place because the collection about to run was
    /// predicted young; taken (reset) by `collect_garbage` on every
    /// collection, so it never spans two.
    pub gen_young_refs_strong_armed: AtomicBool,
    /// zt t7/cc: young cycles that ran with references strong (the
    /// engagement counter), armed collections that turned out whole-heap
    /// (the misprediction count: their referents were traced strongly by a
    /// major, a one-major delay of the clear), and dead young finalizable
    /// objects such a young cycle kept for the next major instead of
    /// resurrecting them for `finalize()`.
    pub gen_young_refs_strong_cycles: AtomicUsize,
    pub gen_young_refs_mispredicted: AtomicUsize,
    pub gen_young_fin_deferred: AtomicUsize,
    /// zt t10/gn2: an armed collection ran whole-heap and its reference round
    /// has not yet advanced the soft clock
    /// ([`ZgcRealHeap::take_armed_major_soft_clock_due`]).
    pub gen_armed_major_soft_clock_due: AtomicBool,
    /// zr z2/g2: where a generational run's collector time goes, split by
    /// cycle kind -- `[GC] zgc-gen-cost:`. See [`ZGenCost`].
    pub gen_cost: ZGenCost,
    /// Cycles in which the parallel marker ran, and in which compaction moved
    /// at least one object, plus the objects it moved.
    ///
    /// Reported on the `[GC] zgc-real:` shutdown line. These exist because the
    /// first smoke test of the 2026-08-13 default flip could not tell whether
    /// either feature had actually engaged: the per-cycle `tracing::debug!`
    /// needs a subscriber the suite harness does not install, so a run that
    /// silently took the serial path looked exactly like a run that did not.
    /// **An opt-in feature turned on by default needs a counter, or its first
    /// measurement is a vacuous green.**
    pub parallel_mark_cycles: AtomicUsize,
    pub compaction_cycles: AtomicUsize,
    /// Why `needs_gc` said yes, one counter per branch.
    ///
    /// AUDIT 2026-09-03. `needs_gc` has four independent reasons and the
    /// stats reported only the TOTAL, so "13 collections against 2" could
    /// not be attributed. Measured on kfusion, one frame, same binary:
    /// `--gpu` collects 13 times and without it twice, for 27% more
    /// garbage -- a trigger difference, not an allocation difference, and
    /// no way to tell which trigger without these.
    /// ONE PER COLLECTION, not one per POLL. `needs_gc` is a predicate the
    /// safepoint machinery asks far more often than a collection happens --
    /// `VmHeap::young_spill_pressure` calls it at every native boundary
    /// (`gc/src/vm_heap.rs:2225`), `has_allocation_headroom` calls it twice
    /// more (`:2951`, `:2966`) -- and it used to `fetch_add` on every TRUE
    /// answer. So the four tallies counted *polls that agreed a collection was
    /// due*, while `trigger_hard_alloc_fail` beside them counts *events*, and
    /// the `[GC] zgc-trigger:` line put the two granularities side by side as
    /// though they were a reason breakdown of the same denominator.
    ///
    /// That is the counter-that-lies shape this tree has already paid for once
    /// (`78c1fe1e8 gc,vm: the allocation counter was wrong in both directions
    /// at once`): a workload that polls the boundary in a tight loop between
    /// two collections reports thousands of "threshold triggers" for two
    /// cycles, and an A/B that compares trigger mixes between two arms is then
    /// comparing native-call rates.
    ///
    /// [`ZgcCounters::trigger_seen`] is the fix: a bit is charged the first
    /// time a reason answers yes, and the mask is cleared at the end of every
    /// collection, so each tally counts *collections this reason asked for*.
    /// The sum can still exceed the collection count -- a cycle can be over the
    /// live-bytes threshold AND short of contiguous headroom, and which of the
    /// two is driving a workload is the whole question -- which is why this is
    /// deliberately not an either/or. It can also be SHORT of the collection
    /// count, and that is informative too: a `System.gc()`-driven run collects
    /// without `needs_gc` ever agreeing, and now reports zeroes rather than
    /// borrowing whichever clause happened to be true at an unrelated poll.
    pub trigger_stress: AtomicUsize,
    pub trigger_threshold: AtomicUsize,
    pub trigger_headroom: AtomicUsize,
    pub trigger_alloc_budget: AtomicUsize,
    /// **The three counters that decide `CRATONVM_ZGC_HEADROOM_BYPASSES_REARM`.**
    ///
    /// `trigger_headroom` beside them CANNOT decide it, and the flip criterion
    /// that said it could ("`trigger_headroom` must become non-zero in the `=1`
    /// arm") was unrunnable for a reason worth stating: that tally is charged
    /// whenever `headroom_low` is true AND the whole predicate passed, which
    /// includes every collection admitted through the ordinary `gc_rearm`
    /// floor. A non-zero reading proves the arena went short of contiguous
    /// space at some point before a collection that was going to happen
    /// anyway. It says nothing about whether the bypass arm was taken, so an
    /// A/B on it compares two numbers that can be equal in both arms while the
    /// flag does nothing at all. See
    /// `docs/feature-designs/zgc-architecture.md`.
    ///
    /// * [`Self::trigger_headroom_starved`] is the **counterfactual, readable
    ///   in the DEFAULT arm**: intervals between collections during which
    ///   `headroom_low` was true and the `gc_rearm` floor refused it anyway.
    ///   Zero on a workload means the flag cannot change that workload's
    ///   behaviour and an A/B over it measures nothing.
    ///
    ///   **This counter is now the whole question, not a step towards a flip
    ///   that was assumed due.** The consequence the gap page attributed to
    ///   this state — `std::process::abort()` with a mostly-free heap — was
    ///   measured for at the wave-4 base commit and **could not be
    ///   reproduced**: gradual growth at `-Xmx64m` and `-Xmx128m`, the
    ///   non-compacting arm (`CRATONVM_ZGC_RELOCATE=0`), the bypass-on arm and
    ///   a single over-sized request all threw a catchable
    ///   `OutOfMemoryError`, at ~63 MiB of a 64 MiB heap — near-full, not
    ///   mostly-free — and a deliberately fragmenting workload pushed 946 MB
    ///   through a 48 MiB heap without failing at all. The predicate gap is
    ///   real as code; whether it has an observable consequence is what this
    ///   counts. See `gap-p-headroom-flip-criteria.md` §0.
    /// * [`Self::trigger_headroom_bypass`] is the **engagement gauge for the
    ///   `=1` arm**: collections admitted *only* because the floor was skipped.
    ///   Non-zero is the proof the run exercised the feature.
    /// * [`Self::trigger_headroom_bypass_suppressed`] separates the two ways
    ///   `trigger_headroom_bypass` can read zero with the flag ON: the shape
    ///   never occurred (starved also zero), or the narrowing term
    ///   `last_cycle_reclaimed_nothing` refused it (this non-zero). Without it
    ///   a zero is unattributable, which is the "an untriggered instrument
    ///   reading zero is not evidence" shape this tree has already paid for.
    ///
    /// Charged once per inter-collection interval through the same
    /// [`Self::trigger_seen`] mask as the four tallies above, NOT once per
    /// agreeing poll — `needs_gc` is polled at every native boundary, and a
    /// per-poll counter here would measure the native-call rate exactly as the
    /// original four did.
    pub trigger_headroom_starved: AtomicUsize,
    /// The largest `gc_rearm - allocated` distance observed at the moment a
    /// [`Self::trigger_headroom_starved`] interval was charged, in bytes.
    ///
    /// "How far below the floor was the trigger when the arena had already run
    /// short?" A few kilobytes is a scheduling artefact; hundreds of megabytes
    /// is the divergence the gap page's worked arithmetic predicts (2 GiB heap,
    /// 10 MiB live, floor at ~507 MiB) and is the number that says the two
    /// quantities come apart without bound on this workload rather than in
    /// principle. Written once per interval, under the same latch.
    pub trigger_headroom_starved_gap: AtomicUsize,
    pub trigger_headroom_bypass: AtomicUsize,
    pub trigger_headroom_bypass_suppressed: AtomicUsize,
    /// Registered old pages that [`Self::young_extra_roots`]'s remembered-set
    /// scan skipped because the page carried no base address.
    ///
    /// **Every count is a dropped old→young edge, i.e. a use-after-free.** It
    /// is a counter rather than an assert because the scan sits on a collector
    /// path where a panic is worse than a lost root — but it must read `0`, and
    /// on `ZgcRealHeap` it must read `0` by construction: all three
    /// registration sites in this file go through
    /// `register_old_page_with_base_in` and `ZRememberedSetTable::remember`
    /// coarsens rather than auto-registering, so no baseless set can exist.
    /// This is what makes that argument checkable from a run instead of from a
    /// comment.
    pub remset_pages_without_base: AtomicUsize,
    /// Which reasons have already been charged for the collection that has not
    /// happened yet, as a mask of the `TRIGGER_BIT_*` constants.
    ///
    /// Read-then-RMW rather than an unconditional `fetch_or`: the steady state
    /// of a hot poll loop is "this bit is already set", and that arm is then a
    /// relaxed load and a branch rather than a contended read-modify-write on a
    /// line every mutator thread touches. The race a plain load admits is
    /// benign in both directions -- two threads crossing the same edge at once
    /// can charge the reason twice, which is one extra count on a diagnostic
    /// line, and the `fetch_or` that follows is what keeps it to two rather
    /// than to one per poll.
    pub trigger_seen: AtomicUsize,
    /// Allocations the arena genuinely REFUSED, which latch
    /// `hard_alloc_failure` and make the safepoint collect without ever
    /// consulting `needs_gc`. Counted because the four `trigger_*` tallies
    /// above came back ALL ZERO on a run that collected 13 times, leaving
    /// this as the only remaining explanation -- and a conclusion by
    /// elimination is worth exactly one counter.
    pub trigger_hard_alloc_fail: AtomicUsize,
    pub objects_relocated: AtomicUsize,
    pub vacated_spans_published: AtomicUsize,
    pub vacated_bytes_published: AtomicUsize,
    pub high_compaction_cycles: AtomicUsize,
    pub high_compaction_declined: AtomicUsize,
    /// Cycles whose LOW slide `CRATONVM_ZGC_RELOCATE_COST_GATE` declined
    /// (see `relocate_cost_gate_declines`). Printed on the `zgc-relocate`
    /// line, cumulatively.
    pub relocation_cost_gated: AtomicUsize,
    /// zs s2/sl2 (`CRATONVM_ZGC_SLIDE_PIN_BY_BASE`): selected low pages the
    /// pin screen dropped because a pin lay INSIDE an object based there (the
    /// raw word's own page was another one, or already dropped). Cumulative,
    /// on the `zgc-relocate` line as `pins_by_base=`.
    pub slide_pins_dropped_by_base: AtomicUsize,
    /// zs s2/sl2 (`CRATONVM_ZGC_TARGET_EXEMPTION_AFTER_PINS`): targeted
    /// cycles whose every targeted page the pin screen dropped (the gates then
    /// treat the cycle as untargeted). `targets_pin_dropped=`.
    pub slide_targets_pin_dropped: AtomicUsize,
    /// zs s2/sl2 (`CRATONVM_ZGC_SLIDE_EVACUATE`, opt-in): `[0]` slides that
    /// evacuated (zt t2/rv: booked only once the to-space window opened),
    /// `[1]` slides the switch asked for but the gate kept on the
    /// slide (failure, pressure tier, no headroom, vacated publication off),
    /// `[2]` evacuations the to-space stopped (it ran out, or zs s3/cr: its
    /// window could not be opened; both unexpected), `[3]` bytes evacuated, `[4]` moved objects whose old extent
    /// was freed on its own because the screen blocked their page's span. On
    /// the `zgc-relocate` line as `evac=[0]/[1]/[2]/[3]/[4]`.
    pub slide_evacuate: [AtomicUsize; 5],
    /// zs s3/cr (`CRATONVM_ZGC_EVAC_QUARANTINE`, opt-in): the evacuated
    /// spans withheld from the free list until the NEXT collection, as
    /// `(lo, hi)` arena OFFSETS. Empty unless the evacuating slide ran with
    /// the quarantine on. Released by `release_evac_quarantine` at the start
    /// of the next collection's sweep. See the arm in
    /// `relocate_stw_admitted_hinted`.
    pub evac_quarantine: Mutex<Vec<(usize, usize)>>,
    /// zs s3/cr: `[0]` spans quarantined, `[1]` bytes quarantined, `[2]`
    /// spans released to the free list, `[3]` bytes released, `[4]` spans
    /// DROPPED at release because something already claimed part of them (a
    /// free block or a registered base overlaps: unexpected; not listed, so
    /// never handed out twice, and left for a later complement sweep to
    /// recover -- zt t1/rl: this said "withheld again", which the code does
    /// not do), `[5]` slides whose blocked spans
    /// `CRATONVM_DBG_ZGC_EVAC_BLOCK_SPANS=1` forced (the positive control of
    /// the per-object free, `slide_evacuate[4]`). On the `zgc-relocate` line
    /// as `quarantine=[0]/[1]/[2]/[3]/[4] evac_forced_blocks=[5]`.
    pub evac_quarantine_stats: [AtomicUsize; 6],
    /// zt t1/rl (`CRATONVM_DBG_ZGC_QUARANTINE_DEAD_CHECK`, debug;
    /// `zgc-zs3cr-relocation-review-residuals` item 6): the dead-at-release
    /// census of `release_evac_quarantine`. `[0]` releases checked, `[1]`
    /// registered objects whose reference slots were walked, `[2]` reference
    /// words that still named an address inside a held span (a slot the
    /// slide's rewrite missed, or a later store of a stale address: what
    /// stage 3, remap in the next mark, would have to heal). On the
    /// `zgc-relocate` line as `qdead=[0]/[1]/[2]/[3]/[4]`. zt t8/rl2
    /// (`zgc-zt1rl-relocation-review-residuals` item 6): `[3]` words that
    /// named a held span but are not 8-aligned, so cannot be an object base --
    /// a primitive in a slot its class declares as a reference (the W7-84
    /// family) -- screened out of `[2]`. zt t31/zc11 (R4): `[4]` the part of
    /// `[2]` whose holder this cycle's mark found live. The release runs
    /// after the mark, so under R4 (`CRATONVM_ZGC_REMAP_IN_MARK`) every live
    /// holder's skipped slot was healed by the markers and `[4]` must stay 0;
    /// the rest of `[2]` is the dead holders R4 leaves stale by design.
    pub evac_quarantine_dead_check: [AtomicUsize; 5],
    /// zs s3/cr (`CRATONVM_ZGC_SELECTIVE_GATE_ON_SELECTION`, default ON): `[0]`
    /// cycles the `=2` gate priced on the pages the slide actually selected
    /// (after the budget prefix and the pin screen) instead of every page
    /// under the occupancy cutoff, `[1]` of those that the old input would
    /// have ADMITTED but the selection's price declined (the cycles the
    /// switch saves). On the `zgc-relocate` line as `gate_sel=[0]/[1]`.
    pub selective_gate_on_selection: [AtomicUsize; 2],
    /// zs s3/cr (`CRATONVM_ZGC_RELOCATE_POOL`, opt-in): `[0]` whole-live-set
    /// relocation passes that ran on more than one worker, `[1]` threads
    /// those passes SPAWNED (the scoped arm: `workers - 1` per pass), `[2]`
    /// passes dispatched to the heap's persistent pool instead. On the
    /// `zgc-relocate` line as `par=[0]/[1]/[2]`.
    pub reloc_par_passes: [AtomicUsize; 3],
    /// zs s3/cr: the persistent worker pool of the relocation passes, built
    /// on the first parallel pass under `CRATONVM_ZGC_RELOCATE_POOL=1`
    /// (per heap; `EvacPool` joins its threads on drop). Never built on the
    /// default arm.
    pub reloc_pool: std::sync::OnceLock<cratonvm_gc_core::evac_pool::EvacPool>,
    /// zt t23/zc5: R1's selection census (`CRATONVM_DBG_ZGC_RELOC_SELECT_CENSUS`)
    /// and R2's stop-the-world relocation through the cycle
    /// (`CRATONVM_ZGC_REGION_RELOCATE`): the pending census and both arms'
    /// counts. See `region_forwarding::ZRegionRelocCounters`.
    pub region_reloc: region_forwarding::ZRegionRelocCounters,
    /// zr z2/cp, `CRATONVM_ZGC_RELOCATE_PRESSURE_SELECT`: low slides whose
    /// selector ran with the WIDENED occupancy cutoff, by tier. `[0]` = the
    /// cycle followed a hard allocation failure (default ON), `[1]` = the
    /// heap was tight with no failure (opt-in `=2`). The engagement counters
    /// of the switch: `[0] == 0` on a run that raised an OOME means the arm
    /// never ran. `[2]` = live bytes the widened cycles' selections carried,
    /// summed, so the copy cost the widening bought is on the record.
    pub pressure_select: [AtomicUsize; 3],
    /// zr z9/sl, `CRATONVM_ZGC_SELECTIVE_RELOCATE`: the selective arm's
    /// engagement. `[0]` = low slides the filter ran on, `[1]` = pages the
    /// default selector would have evacuated that the filter kept in place
    /// (their free bytes are in holes the allocator can already use), `[2]` =
    /// slides it emptied (no page with a survivor left: no slide, no
    /// rewrite), `[3]` = slides that followed an allocation failure and so
    /// bypassed it (the full-compaction fallback). All zero with the switch
    /// unset. Printed on `[GC] zgc-selective:`.
    ///
    /// zs s1/sl, the cycle gate (`CRATONVM_ZGC_SELECTIVE_RELOCATE=2`):
    /// `[4]` = cycles whose candidate pages held enough fragmented garbage
    /// that the gate ran the default selector unchanged, `[5]` = cycles it
    /// declined outright (no slide, no rewrite), `[6]` = cycles it handed to
    /// the default selector because the low bump headroom was tight (the
    /// allocator is already living on holes), `[7]` = slides whose
    /// fragmented-garbage figure came from the fused `logical_pages` pass
    /// rather than a separate walk (`CRATONVM_ZGC_SELECTIVE_FUSED_FRAG`).
    pub selective_relocate: [AtomicUsize; 8],
    /// zr z9/sl: bytes the slide actually copied, both ends. `[0]` = the last
    /// slide's (reset by `relocate_refusal` each cycle, so a refused cycle
    /// reads 0), `[1]` = cumulative. `moved_bytes=` on `[GC] zgc-relocate:`;
    /// `bytes_copied=` on `[GC] zgc-real:` is the SURVIVOR total (G1's key
    /// name), not a copy.
    pub slide_moved_bytes: [AtomicUsize; 2],
    /// zt t31/x10 (item 5, `CRATONVM_ZGC_STALL_MARK_REACHES_JOINED_PAUSE`):
    /// an allocation stall is waiting for a collection. Raised by the stalled
    /// thread before it asks for its pause
    /// ([`ZgcRealHeap::note_stall_cycle_pending`]) and taken by the next
    /// cycle's end, whichever thread runs it, so a stall whose thread lost the
    /// race and took part in another thread's pause still leaves the `Timer`
    /// clock alone ([`ZgcRealHeap::record_metrics_for_cycle`]).
    pub stall_cycle_pending: AtomicBool,
    /// zt t31/x10 (item 1, `CRATONVM_ZGC_XLOG_EDGE_BOOK`): the occupancy
    /// (`allocated`) when the open concurrent cycle's mark started, taken by
    /// the collection that closes it; `usize::MAX` = none open.
    pub conc_mark_start_used: AtomicUsize,
    /// zt t31/x10: that figure for the LAST collection, when it closed a
    /// concurrent cycle (its `-Xlog` Mark Start column); `usize::MAX` for a
    /// stop-the-world collection, whose mark started inside its own pause.
    pub last_mark_start_used: AtomicUsize,
    /// zr z2/cp: [`ZgcRealHeap::hard_alloc_failure`] as it stood when THIS cycle
    /// began. `collect_garbage` lowers the latch itself before the slide runs
    /// (the re-arm block), so the relocation selector cannot read the latch;
    /// it reads this copy, taken where the cycle decides `force_major`. See
    /// [`relocate_pressure_occupancy`]. `false` outside a collection and for
    /// the direct `relocate_stw` callers (tests), which keeps them on the
    /// unwidened selector.
    pub cycle_after_alloc_failure: AtomicBool,
    /// zr z2/cp: the VM's heap-exhaustion unwind, offered by the INFALLIBLE
    /// trait arms (`GarbageCollector::alloc_object` / `alloc_array`) before
    /// they abort. Installed per VM by `vm_init`
    /// ([`ZgcRealHeap::set_exhaustion_unwind`]); unset in the gc crate's own tests
    /// and in embedders that never install one, which keeps the abort.
    ///
    /// The hook either DIVERGES -- it unwinds with the VM's
    /// `NativeAllocOom` payload when a `catch_unwind` that converts it into
    /// `java.lang.OutOfMemoryError` is the next handler up and no JIT frame
    /// is between (the `native_oom::unwind_ok` permission) -- or returns, and
    /// the arm aborts exactly as before. `CRATONVM_ZGC_INFALLIBLE_OOME_UNWIND=0`
    /// skips the offer.
    pub exhaustion_unwind: std::sync::OnceLock<fn(usize, &'static str)>,
    /// Times an infallible arm offered the hook above (zr z2/cp). The hook
    /// diverges when it raises, so this counts OFFERS; an offer the hook
    /// declined is followed by the abort.
    pub exhaustion_unwind_offers: AtomicUsize,
    /// zr z5/cr: the size of the latest request `alloc_raw` refused, and the
    /// collection count when it did (`gc_count`), so a refusal door can ask
    /// after its own collection whether the arena now has room for exactly
    /// that request (`room_for_last_refusal`). Written on the failure arm only.
    pub last_refusal_size: AtomicUsize,
    pub last_refusal_epoch: AtomicUsize,
    /// zt t2/al (`CRATONVM_ZGC_VERDICT_RECORDS_REFUSAL`, default ON, `=0`
    /// kills; latched at construction): an exhausted-verdict refusal
    /// (`try_alloc_object` / `try_alloc_array`, default on
    /// `CRATONVM_ZGC_EXHAUSTED_FREE_FLOOR`) writes the two fields above as
    /// `alloc_raw`'s failure arm does. See [`ZgcRealHeap::note_verdict_refusal`].
    pub verdict_records_refusal: AtomicBool,
    /// zs s10/sp: `CRATONVM_ZGC_RETIRE_SUB_REQUEST_SPANS` latched at
    /// construction -- the floor of the retire threshold (`0`: off, the `=0`
    /// kill; 2048 by default since zt t12). See
    /// [`ZgcRealHeap::note_final_refusal`].
    pub sub_request_floor: usize,
    /// zs s10/sp: what the arena could serve (free-list bytes plus the
    /// unbumped middle) and its bumped bytes when the rule armed, for the
    /// post-collection re-check ([`ZgcRealHeap::recheck_sub_request_spans`]).
    pub sub_request_armed_available: AtomicUsize,
    pub sub_request_armed_used: AtomicUsize,
    /// zs s10/sp: arms (final refusals that set the threshold) and disarms
    /// (collections that changed the picture), the rule's engagement counters.
    pub sub_request_arms: AtomicUsize,
    pub sub_request_disarms: AtomicUsize,
    /// zr z3/lt: the per-heap state that used to be process statics (switch
    /// seeds, the unsizable / targeted-page counters, the end-of-run metrics
    /// claim). See `heap_state`.
    pub(in crate::zgc) local: heap_state::ZHeapLocalState,
    /// Primitive legacy cells the rewrite pass left alone because their bits
    /// equalled a moved base (zr z1/rl,
    /// `CRATONVM_ZGC_REWRITE_REFERENCE_CELLS_ONLY`). Each one is a Java
    /// `long`/`double` the pass used to change.
    pub rewrite_primitive_cells_skipped: AtomicUsize,
    /// Survivors the slide's rewrite served from the size table without a
    /// header read (zr z7/pc, `CRATONVM_ZGC_RELOCATE_SIZE_TABLE`); `0` on the
    /// kill arm. The positive control of that switch.
    pub relocate_size_table_skips: AtomicUsize,
    /// Survivors the slide's page census sized from the sweep's hints instead
    /// of their header (zr z7/pc, `CRATONVM_ZGC_SWEEP_SIZE_HINTS`).
    pub sweep_size_hint_hits: AtomicUsize,
    /// Survivor hints the last hint-recording sweep produced (zt t26/pc1,
    /// `CRATONVM_ZGC_SWEEP_RESERVE_HINTS`): the next sweep reserves its lists
    /// from it (`ZSweepCfg::reserve_live_hints`). A sizing hint only.
    pub sweep_live_hints_last: AtomicUsize,
    /// zt t27/pc2 (`CRATONVM_ZGC_SLIDE_NO_MOVE_DECLINE`): relocating cycles
    /// whose low slide was declined because no selected survivor could move
    /// ([`slide_moves_nothing`]). Cumulative; printed on `[GC] zgc-cpu-skips:`.
    pub slide_no_move_declined: AtomicUsize,
    /// ... and the census of what it missed: relocating cycles whose low slide
    /// ran on a non-empty selection (not evacuating) and moved nothing. With
    /// the decline on, each one is a cycle the bound could not prove; with it
    /// off (`=0`), every moved-nothing slide. Printed on `[GC] zgc-cpu-skips:`.
    pub slide_no_move_missed: AtomicUsize,
    /// zt t27/pc2 (`CRATONVM_ZGC_TLAB_CARVE_FUTILE_LATCH`, default ON, `=0`
    /// kill): read once per heap, so the carve's check is one relaxed load.
    pub carve_futile_on: AtomicBool,
    /// The carve-futile latch (see `carve_tlab_chunk`), two slots of four
    /// words ([`ZCarveFutileLatch`]): per slot, the free-space generation of
    /// the failed carve plus one (`0` = no latch), its `want`, its `need`, and
    /// the skips left before a real attempt. Advisory: a torn or stale reading
    /// only sends an object to the per-object path a failed carve sends it
    /// to, or makes one more real attempt.
    pub carve_futile: ZCarveFutileLatch,
    /// Bumped where a TLAB tail rejoins the low free list between two
    /// collections (the cell retire and the VM tail sink), while a latch is
    /// set: half of the latch's generation (`gc_count` is the other half).
    pub carve_space_gen: AtomicUsize,
    /// `[0]` carves the latch answered without the arena lock, `[1]` latches
    /// set. Printed on `[GC] zgc-cpu-skips:`.
    pub carve_futile_census: [AtomicUsize; 2],
    /// zt t27/pc2 (`CRATONVM_ZGC_SWEEP_SIZE_MEMO`): the last hint-recording
    /// sweep's survivor size cells, minus every base its slide moved away from,
    /// stamped with the `gc_count` it is valid for. Taken by the next
    /// collection (and dropped by every other one). See [`ZSweepSizeMemo`].
    pub size_memo: Mutex<Option<ZSweepSizeMemo>>,
    /// `[0]` survivors the bitmap sweep sized from the memo (no header read),
    /// `[1]` sweeps that had a memo to use. Printed on `[GC] zgc-cpu-skips:`.
    pub size_memo_census: [AtomicUsize; 2],
    /// Live bytes `ZRelocationSet::select` admitted **above**
    /// `max_evacuation_bytes` because the page was targeted, summed over
    /// cycles. Printed on the `zgc-relocate` line as `targeted_overrun`.
    ///
    /// *Applies
    /// `docs/feature-designs/zgc-architecture.md`.*
    /// The budget rule in `forwarding::ZRelocationSet::select` is a **prefix**
    /// rule -- it ends the selection rather than skipping an unaffordable page
    /// -- and targeted pages sort first, so before wave 2 a single targeted
    /// page bigger than the 64 MiB budget ended the selection on its first
    /// iteration and the cycle evacuated *nothing at all*. The exemption
    /// (`ZRelocationPolicy::budget_exempts_targets`, default on) fixes that,
    /// and this is its measurement.
    ///
    /// Zero unless the exemption fired. A persistently non-zero value means a
    /// targeted window has grown well past the "34 objects in 49 runs" shape
    /// the mechanism was measured on, and the right response then is to bound
    /// the window -- not to re-impose the budget, which is what naming a target
    /// means you are not doing.
    pub relocation_targeted_overrun_bytes: AtomicUsize,
    pub high_objects_relocated: AtomicUsize,
    pub high_bytes_copied: AtomicUsize,
    /// TLAB refills served by a RECYCLED free-list block at or above the
    /// preferred floor (`want / 8`), and refills served below it by the
    /// starved floor.
    ///
    /// The starved floor is a switch (`CRATONVM_ZGC_TLAB_STARVED_RECYCLE`)
    /// whose engagement nothing reported, so "this workload never reaches the
    /// starved rung" and "it reaches it constantly" were the same run to a
    /// reader. It takes the arena's LARGEST low free block, so every firing
    /// lowers `largest_free_block` -- which is the number a direct
    /// (non-TLAB-eligible) allocation is about to be measured against.
    pub tlab_refill_recycled: AtomicUsize,
    /// ...and the starved rung specifically. See
    /// [`Self::tlab_refill_recycled`].
    pub tlab_refill_starved: AtomicUsize,
    /// Bytes the starved rung took off the free list.
    pub tlab_refill_starved_bytes: AtomicUsize,
    /// Chunk bytes `carve_tlab_chunk` did NOT `memset` because the arena
    /// proved they had never been written (round 9 wave 10, `zgc10`; see
    /// `Arena::pristine_lo` and [`zgc_pristine_chunks_enabled`]).
    pub tlab_pristine_bytes_skipped: AtomicUsize,
    /// Chunk bytes `carve_tlab_chunk` did NOT `memset` because the last slide
    /// had zeroed them and nothing had written them since (r16 zgcarray, ZA1;
    /// `Arena`'s `SlideZeroWindow`).
    pub tlab_slide_zero_bytes_skipped: AtomicUsize,
    /// zt t2/al (`CRATONVM_ZGC_CHUNK_ZERO_DESCENDING`, opt-in; latched at
    /// construction, an atomic only so unit tests can flip it): the piece
    /// size in bytes `carve_tlab_chunk` zeroes a chunk in, top piece first
    /// (`ZeroPlan::zero_descending`); `0` = the ascending whole-chunk
    /// `memset` (the default). See [`zgc_chunk_zero_descending_block`].
    pub chunk_zero_descending: AtomicUsize,
    /// Chunks zeroed that way (the engagement counter).
    pub chunk_zero_descending_chunks: AtomicUsize,
    /// This heap's own `java.lang.ref` reference processor.
    ///
    /// # It is EMPTY in a real run, and the machinery built on it is inert
    ///
    /// Nothing in the VM calls [`Self::discover_reference`] — the only callers
    /// anywhere are this file's own tests. Every reference a running program
    /// creates is registered with the VM-level processor
    /// (`SharedVm::mem::ref_processor`) by `native_ref_init` and friends, and
    /// is cleared and enqueued by `process_references_after_gc`. So this
    /// processor holds nothing, and three things built on it never do
    /// anything in production:
    ///
    /// * the `ref_skip_objs` referent skip set [`Self::collect_garbage`]
    ///   builds, and the [`Self::mark_ref_skip`] twin for concurrent cycles —
    ///   both derived from `reference_object_addresses()`, which returns an
    ///   empty vector here;
    /// * the soft-survivor resurrection pass that follows it;
    /// * the `process_references` call at the end of the cycle.
    ///
    /// **This is not a hole in reference semantics**, which is the conclusion
    /// the shape of this code invites and which was measured and rejected on
    /// 2026-08-15. The VM compensates one level up, and unconditionally:
    /// `weakref_null_referents_pre_gc` writes null into the referent slot of
    /// every active Weak and Phantom reference *before* any collector runs, so
    /// there is nothing for a skip set to skip — a marker that traces slot 0
    /// as a strong edge (which every CratonVM marker does) reads a null. Soft
    /// references take the same route since the same date, via
    /// `ReferenceProcessor::condemn_idle_soft_refs`, which applies the LRU
    /// policy ahead of the mark so only the entries it condemns are nulled.
    ///
    /// Verified end-to-end against HotSpot on all three collectors: a weak
    /// reference clears, a phantom enqueues, a soft reference is retained
    /// while the heap is roomy, cleared once it is tight, and cleared rather
    /// than allowed to become an `OutOfMemoryError`.
    ///
    /// Left in place rather than deleted because it is the only reference
    /// implementation this heap can be tested against in isolation, and
    /// several tests in this file do exactly that. Anyone reading it as the
    /// live path — or "fixing" a reference bug here — is reading the wrong
    /// file; start at `weakref_null_referents_pre_gc`.
    pub ref_processor: Mutex<ReferenceProcessor>,
    /// Finalizer-resurrection input for the current collection — see
    /// [`Self::collect_garbage_with_finalizers`]. Consumed (taken) by the
    /// mark phase; empty on every plain `collect_garbage`.
    pub pending_finalizer_roots: Mutex<Vec<usize>>,
    /// Output half: addresses of dead-but-finalizable objects the mark phase
    /// resurrected (non-moving, so pre == post address). Drained by
    /// [`Self::collect_garbage_with_finalizers`].
    pub resurrected_finalizers: Mutex<Vec<usize>>,
    /// The depth-2 finalizer set of the LAST collection
    /// (`VmHeap::take_resurrection_closure`): every object whose mark bit the
    /// resurrection pass set -- the dead finalizables and everything only they
    /// reach -- at its PRE-relocation address. `mark_claim` fails on an object
    /// the strong closure marked, so none of those is ever recorded. Cleared
    /// by `collect_garbage_with_finalizers` at entry; a LEAF lock.
    pub resurrection_closure: Mutex<Vec<usize>>,
    /// zr z5/rv5 (`CRATONVM_ZGC_SOFT_CLEAR_PRECISE_FIRST`): the condemned
    /// soft referents (pre-collection addresses) the VM armed for the NEXT
    /// collection -- its last-ditch clear-all one. Taken at the start of
    /// `collect_garbage`, so it never spans two collections. See
    /// [`ZgcRealHeap::arm_soft_precise_probe`].
    pub soft_precise_probe: Mutex<Option<Vec<usize>>>,
    /// zr z5/rv5: the probe's answer for the last collection -- the armed
    /// referents the mark reached from the PRECISE roots alone, before any
    /// conservative compiled-frame word was traced. `None`: the probe did not
    /// run (not armed, or a concurrently marked cycle). Leaf lock.
    pub soft_precise_kept: Mutex<Option<Vec<usize>>>,
    /// zr z5/rv5: cycles the probe ran, and roots it deferred to the second
    /// (conservative) drain. Cumulative.
    pub soft_precise_probe_cycles: AtomicUsize,
    pub soft_precise_deferred_roots: AtomicUsize,

    /// Reference-slot census — the instrument for the one unmeasured number in
    /// `docs/feature-designs/zgc-reference-slot-representation.md`.
    ///
    /// Why it lives on the heap rather than in a `static`: it is the same
    /// no-process-global rule [`census`]'s module header states, and the same
    /// rule this tree learned the hard way from process-global GC caches
    /// outliving the VM that filled them. One census per heap, reached only
    /// through `&self`.
    ///
    /// **Off by default**, so an untouched heap pays exactly one relaxed
    /// `AtomicBool` load per collection (the gate on the [`Self::collect_garbage`]
    /// call site) and nothing at all on any mutator path. Turn it on through
    /// [`Self::slot_census`].
    pub slot_census: census::ZSlotCensus,

    /// The OpenJDK-shaped pause/phase/allocation accounting in [`metrics`].
    ///
    /// # Why this field exists (2026-09-20)
    ///
    /// `metrics.rs` is 1,377 production lines whose only reference in the whole
    /// tree was a `TODO` comment in this file: `ZgcMetrics::record_phase`,
    /// `record_cycle`, `format_summary`, `to_tsv_row` and the
    /// `cratonvm_jfr::phase` integration had no caller, so the module its own
    /// header calls *"the instrument"* — on the rule that **the instrument goes
    /// in first** — had never taken a single sample. Meanwhile the collector
    /// grew a parallel, hand-rolled set of `eprintln!` phase timings, so the
    /// tree carried two per-phase timing systems: one used and ad-hoc, one
    /// designed and dead. See
    /// `docs/feature-designs/profile-guided-inlining.md`.
    ///
    /// # What is adopted, and what deliberately is not
    ///
    /// Adopted: the **recording** half. [`Self::collect_garbage`] feeds the
    /// laps its `ZPhaseClock` already takes into [`metrics::ZgcMetrics::record_phase`]
    /// and closes each cycle with [`metrics::ZgcMetrics::record_cycle`], so
    /// every counter, percentile ring and derived rate in that module is now
    /// live on every run.
    ///
    /// NOT adopted: the **reporting** half as a replacement for the existing
    /// lines. `format_cycle_line` renders a different format, and every
    /// harness, grep and archived comparison in this tree is keyed on
    /// `[GC] zgc-real:` / `[GC] zgc-pause:` as they stand. Swapping them is a
    /// wave with a compatibility window, not an edit. So the reports are
    /// reachable through `CRATONVM_ZGC_METRICS` (per-cycle line) and at
    /// teardown, and the default run's output is byte-for-byte what it was.
    ///
    /// Cost when nobody asks: a handful of relaxed `fetch_add`s and one
    /// `parking_lot` uncontended lock per collection — per *collection*, not
    /// per object — which is why this is unconditional rather than gated. A
    /// gated recorder is how the census came to be unreachable for a year.
    pub metrics: metrics::ZgcMetrics,

    /// The weak-referent skip set for a **concurrent** mark cycle: the
    /// heap-owned twin of the `ref_skip_objs` local that
    /// [`Self::collect_garbage`] builds on its own stack.
    ///
    /// # Why this is a field and not a parameter
    ///
    /// [`mark::ZMarkContext::visit_refs`] takes `&self` and an address and
    /// nothing else, and it is called from every mark worker and from
    /// arbitrary mutator threads. The skip set therefore cannot travel on the
    /// stack the way `collect_garbage`'s does; it has to live somewhere every
    /// caller can reach. It is also why the set is an [`std::sync::Arc`]: a
    /// worker clones the handle under a read lock, drops the guard, and walks
    /// the object with no lock held, which is the same shape the census's
    /// two-phase contract uses and the same reason — a registry-style guard
    /// held across a call back into the heap is a lock cycle.
    ///
    /// # Why it must be a SNAPSHOT
    ///
    /// [`ReferenceProcessor::reference_object_addresses`] is a live view: a
    /// mutator that constructs a `WeakReference` mid-cycle adds to it. If
    /// workers read that live view, an object could be traced as a strong
    /// edge before the `Reference` was discovered and skipped after, so
    /// whether the referent is immortal would depend on thread timing. Taken
    /// once at [`Self::begin_concurrent_mark_cycle`] and held for the whole
    /// cycle, the answer is the same for every worker.
    ///
    /// `None` means "no concurrent mark cycle is open" — see
    /// [`Self::concurrent_mark_skip_set`] for what `visit_refs` then does and
    /// why that direction was chosen.
    pub mark_ref_skip: parking_lot::RwLock<Option<std::sync::Arc<ZSkipSnapshot>>>,
    /// A Bloom filter over [`Self::mark_ref_skip`]'s addresses, so the
    /// per-object path does not take that `RwLock`.
    ///
    /// # The third per-object lock, and why it needed a different answer
    ///
    /// `visit_refs` asked `concurrent_mark_skip_set()` once per marked object,
    /// which is an `RwLock` read (a contended `compare_exchange` on one shared
    /// word) plus an `Arc` clone and drop (two more contended atomic RMWs on
    /// another). Three contended atomics per object, on every mark worker. That
    /// fits the 2026-08-14 signature exactly: four workers cost **+153% pause**
    /// against zero and the rise is monotonic in worker count, which is a lock.
    ///
    /// The other two per-object locks in this path were fixed with an
    /// "is it empty?" latch ([`cratonvm_types::metadata_pin`],
    /// [`cratonvm_gc_core::external_roots`]). That does not work here: the skip set is
    /// **non-empty during every cycle in a real run** — it is every registered
    /// weak, soft and phantom `Reference` object — so a latch would be true
    /// throughout and buy nothing.
    ///
    /// A Bloom filter does, because the question is per OBJECT and almost every
    /// object is not a `Reference`. Two bits per member over
    /// `Z_SKIP_BLOOM_WORDS * 64` bits: a miss is definitive and lock-free, and
    /// only a hit falls through to the exact set. With 1000 `Reference`s in
    /// 32768 bits the hit rate is ~0.35%, so ~99.6% of objects take no lock at
    /// all.
    ///
    /// **A Bloom filter can never give a false negative**, which is the only
    /// direction that would be a correctness bug: a member always sets both its
    /// bits, so an object in the skip set always reaches the exact check. A
    /// false positive costs one lock acquisition and nothing else.
    ///
    /// Deliberately NOT an `AtomicPtr` to the set itself, which would be one
    /// atomic instead of two bit tests. That would need the `Arc` to outlive
    /// every worker that can read the pointer, and the two mark paths reach
    /// `end_concurrent_mark_cycle` by different routes — one after a join, one
    /// after a handshake. A raw pointer whose lifetime argument depends on which
    /// of two call chains ran is the kind of thing that is correct until someone
    /// adds a third. Plain atomics have no such argument to get wrong.
    pub mark_ref_skip_bloom: Box<[AtomicU64; Z_SKIP_BLOOM_WORDS]>,
    /// zr z1/rf (`CRATONVM_ZGC_CONC_REF_SKIP`, default ON since dev
    /// `43e50d605c`; it said "opt-in" until zt t1/mk): the VM's ACTIVE Weak /
    /// Phantom `Reference` objects, handed in at the mark-start pause by
    /// [`ZgcRealHeap::set_concurrent_vm_reference_skip`] and unioned into
    /// every skip-set snapshot [`ZgcRealHeap::begin_concurrent_mark_cycle`]
    /// takes until [`ZgcRealHeap::end_concurrent_mark_cycle`] empties it.
    ///
    /// Why: the private [`Self::ref_processor`] is empty in a real run, so a
    /// concurrent cycle's skip set was empty and the concurrent marker traced
    /// every referent as a strong edge. The VM's pre-collection pass nulls the
    /// weak / phantom slots only at the collection that ENDS the cycle, after
    /// those referents were already marked, so a cycle that ended in a
    /// certified concurrent mark cleared no weak reference whose referent died
    /// before mark start (it is cleared a cycle later, or never when every
    /// cycle is concurrent). Empty unless the switch is set, and emptied at
    /// every cycle end, so a later stop-the-world mark never sees a stale
    /// (possibly re-issued) address.
    pub vm_conc_ref_skip: Mutex<Vec<usize>>,
    /// zr z2/cc: the VM processor's active SOFT `Reference` objects, hidden
    /// for the concurrent PHASE only (the mark-start snapshot), never at the
    /// mark end. `finish_concurrent_mark` takes them before it refreshes the
    /// skip set and pushes the referents of the ones already marked as roots
    /// (`kept_soft_referent_roots`), after the VM's pre-collection pass has
    /// nulled the slots the LRU policy condemned. See
    /// `docs/feature-designs/zgc-architecture.md`.
    pub vm_conc_soft_skip: Mutex<Vec<usize>>,
    /// Which objects and classes have edges the heap does not store in them.
    /// Rebuilt per collection; read once per marked object. See `mark_roots`.
    pub mark_root_filter: mark_roots::ZMarkRootFilter,

    /// One-shot latch for the "`visit_refs` ran with no skip-set snapshot"
    /// warning. Without it the warning is one line per object visited, which
    /// on a real heap is millions of lines and is itself a hang. `Relaxed`: it
    /// orders nothing and publishes nothing, it only de-duplicates a log line.
    pub mark_ref_skip_warned: AtomicBool,

    /// Reused backing store for the sweep's dead-address slice.
    ///
    /// # Why a field and not a local `Vec`
    ///
    /// The sweep pushes one `usize` per reclaimed object, and it is the only
    /// producer. A fresh `Vec::new()` per collection pays, inside the pause,
    /// for ~24 doubling reallocations and the memcpy of every one of them --
    /// and on the 13.0M-dead-object whole-heap cycle this file's own pause
    /// anatomy is written against, the final allocation alone is **104 MB**.
    /// That is the same shape as the `bases()` allocation the 2026-08-17
    /// anatomy measured at 13% of the pause and which
    /// [`ZObjectStartsSnapshot::for_each_base`] exists to remove; the fix
    /// never reached this vector.
    ///
    /// Held across collections so the capacity is paid once, at the high-water
    /// mark, instead of once per cycle. `clear()` keeps the allocation and
    /// drops the length, and `usize` has no destructor, so the reuse is a
    /// single store.
    ///
    /// # And it is usually not filled at all
    ///
    /// [`MonitorCleanup::wants_dead_addresses`] is asked once per collection,
    /// before the sweep. The VM's monitor table answers `false` whenever no
    /// monitor is inflated and no CAS lock is held -- which its own comment
    /// calls the common case -- and then the sweep pushes nothing and this
    /// stays at whatever capacity a previous contended cycle left it.
    ///
    /// A `Mutex` because `collect_garbage` takes `&self`. Uncontended by
    /// construction: the only lock site is inside the stop-the-world pause.
    pub dead_scratch: Mutex<Vec<usize>>,
    /// zr z3/fp: footprint state -- the three switches, the peaks the
    /// `zgc-footprint` summary line prints, and the trim/stamp engagement
    /// counters. See `ZFootprintCounters`.
    pub fp: Box<ZFootprintCounters>,

    /// Forwarding words the slide has stamped into vacated tails, and answers
    /// [`Self::forwarded_after_slide`] served from one rather than from
    /// [`Self::relocations`]. See [`Self::forwarding_word_engagement`].
    pub forwarding_words_stamped: AtomicUsize,
    pub forwarding_words_read: AtomicUsize,
    /// Bytes this heap has returned to the OS, summed over collections.
    ///
    /// The engagement counter for the reserving backing store's second half. A
    /// run with a large peak and a zero here is one where lazy commit bought
    /// the startup charge and nothing else -- which is the state before
    /// `Arena::decommit_unbumped_middle` existed.
    pub bytes_uncommitted: AtomicUsize,
    /// zr z4/jf: the last collection's laps, relocation set and uncommit, for
    /// the VM's JFR writers ([`jfr_facts`]). Taken by the event finish.
    pub jfr: jfr_facts::ZJfrFacts,

    /// Cells `retire_all_tlabs_at_safepoint` could not lock -- the DANGEROUS
    /// half of `tlab_retire_skipped_total`, which counts the opportunistic
    /// callers too. See `retire_all_tlabs_at_safepoint`.
    pub tlab_retire_skipped_at_safepoint: AtomicUsize,
    /// Allocate-black claims that actually had to touch the bitmap, i.e. the
    /// residual after the per-chunk blackening. `conc_black_allocations` counts
    /// every object born black; this counts the ones that cost an atomic. A run
    /// where the two are equal is one where the per-chunk path is inert.
    pub conc_black_claims: AtomicUsize,
    /// Cycles that relocated the unpinned pages despite an incomplete coverage
    /// proof. See [`Self::coverage_incompleteness_is_page_pinnable`].
    pub relocation_on_page_pins: AtomicUsize,
    /// ENGAGEMENT for [`ZgcRealHeap::verify_no_dangling_slots_after_slide`]:
    /// slides it actually walked, and survivors it walked them over.
    ///
    /// Without these the verifier's clean answer is INVISIBLE in a release
    /// build. It reports a finding at `error!` and a pass at `debug!`, and
    /// `tracing`'s `release_max_level_info` compiles the `debug!` away — so a
    /// release run under `CRATONVM_DBG_ZGC_VERIFY_SLIDE=1` prints nothing
    /// whether every slot resolved or the verifier never ran, and "no output"
    /// was the only thing a reader could quote. That is the
    /// zero-from-an-instrument-armed-where-it-cannot-fire shape this collector
    /// already refuses to accept from `pinned_jit_roots_snapshot` (see
    /// `gc_quiescence::conservative_jit_scans`), and it applies to a verifier
    /// exactly as it applies to a pin set.
    pub slide_verifications: AtomicUsize,
    pub slide_verified_survivors: AtomicUsize,
    /// Reference slots the verifier judged dangling, split the way its own
    /// report splits them: `missed_rewrite` is the collector's fault, the rest
    /// were never live bases.
    pub slide_verify_missed_rewrites: AtomicUsize,
    pub slide_verify_unregistered: AtomicUsize,
    /// The stop-the-world mark pool, kept ALIVE BETWEEN CYCLES so its worker
    /// threads are spawned once rather than per collection. `(workers, pool)`:
    /// a changed worker count rebuilds it. See `persistent_mark_pool`.
    pub mark_pool: Mutex<Option<(usize, std::sync::Arc<mark::ZMarkCoordinator>)>>,
    /// zs s3/rv2 test seam: a direct scan of this address on a `zgc-mark-*`
    /// pool worker panics (0 = never). Test builds only.
    #[cfg(test)]
    pub test_worker_scan_panic_at: AtomicUsize,
    /// zt t1/mk test seam (the "driver-panic seam" of
    /// `zgc-zs2mc2-mark-engine-review-residuals`): the stop-the-world mark's
    /// DRIVER panics at its first mark-end safepoint, with the pool's workers
    /// still inside the cycle. Test builds only.
    #[cfg(test)]
    pub test_driver_safepoint_panic: AtomicBool,
    /// `CRATONVM_ZGC_JIT_TLAB`: whether `refill_tlab` hands the VM thread's
    /// buffer a chunk. See `zgc/vm_tlab.rs`.
    pub vm_tlab_enabled: AtomicBool,
    /// Reserved tails of VM TLABs whose owners could not retire before this
    /// collection, published by the STW protocol and consumed by the slide.
    pub jit_tlab_skip: Mutex<Vec<(usize, usize)>>,
    /// zr z1/al residual: the published tails the last collection added back
    /// to `allocated`, merged, with the collection count that collection
    /// ended on. See `ZgcRealHeap::recharge_published_tails`.
    pub recharged_tails: Mutex<(usize, Vec<(usize, usize)>)>,
    /// Bytes `recharge_published_tails` has added back, over the heap's life.
    pub published_tail_recharge_bytes: AtomicUsize,
    /// Chunks handed to VM TLABs, and their bytes. The engagement counter for
    /// the JIT's inline allocator on this backend.
    pub vm_tlab_refills: AtomicUsize,
    pub vm_tlab_refill_bytes: AtomicUsize,
    /// Tails taken back from retiring VM TLABs through the sink, and their bytes.
    pub vm_tlab_tails_returned: AtomicUsize,
    pub vm_tlab_tail_bytes_returned: AtomicUsize,
    /// zs s5/ta: `CRATONVM_DBG_ZGC_POST_REFUSAL` -- how many `alloc_raw`
    /// hand-outs to log after each refusal (0: off, the default). Latched at
    /// construction, so the off path is one plain load. See
    /// [`zgc_dbg_post_refusal_limit`].
    pub post_refusal_limit: usize,
    /// zs s5/ta: hand-outs still to log since the last refusal.
    pub post_refusal_budget: AtomicUsize,
    /// zr z1/al: `CRATONVM_ZGC_RAW_ZERO_OUTSIDE_LOCK` (default ON). Whether
    /// `alloc_raw` zeroes a reused span after it has released the arena lock
    /// instead of under it. Per heap so tests can flip it. See `alloc_raw`.
    pub raw_zero_outside_lock: AtomicBool,
    /// zr z1/al: `CRATONVM_ZGC_SWEEP_SIZES_OVERSIZE_ARRAYS` (default ON).
    /// Whether the large-object end's sweep sizes a dead array that the
    /// `alloc_size` corruption screen refuses (payload > 256 MiB) from its
    /// header, after the extent checks in `ZgcRealHeap::oversize_array_extent`,
    /// instead of retaining it forever.
    pub sweep_sizes_oversize_arrays: AtomicBool,
    /// zr z1/al: dead oversize arrays the high-end sweep reclaimed through
    /// [`ZgcRealHeap::oversize_array_extent`], their bytes, and the ones it
    /// refused (an extent check failed, so the base is still retained). The
    /// engagement counters for `CRATONVM_ZGC_SWEEP_SIZES_OVERSIZE_ARRAYS`.
    pub oversize_arrays_swept: AtomicUsize,
    pub oversize_array_bytes_swept: AtomicUsize,
    pub oversize_arrays_refused: AtomicUsize,
    /// zs s2/rf: `CRATONVM_ZGC_SWEEP_ONE_FREES_OVERSIZE` (default ON, `=0`
    /// kill). Whether the PER-OBJECT sweep (`sweep_one`: young cycles, the
    /// per-object fallback arm, the spill set) frees a DEAD array that
    /// `alloc_size` refuses, in either region, through
    /// `ZgcRealHeap::oversize_array_extent`'s checks against the cycle's
    /// snapshot, instead of retaining it forever. Read once per heap.
    pub sweep_one_frees_oversize: AtomicBool,
    /// zs s2/rf: dead oversize arrays `sweep_one` freed (also counted in
    /// `oversize_arrays_swept`); the engagement counter of the switch above.
    pub sweep_one_oversize_freed: AtomicUsize,
    /// zs s2/rf: `CRATONVM_ZGC_SWEEP_SPILL_HIGH_SPANS` (default ON, `=0`
    /// kill). A spilled dead base in the large-object end always pushes its
    /// span; see `sweep_one`'s `already_reclaimed`. Read once per heap.
    pub sweep_spill_high_spans: AtomicBool,
    /// zs s2/rf: `CRATONVM_ZGC_OVERSIZE_SCREEN_MEMO` (default ON, `=0`
    /// kill). Whether [`ZgcRealHeap::oversize_live_array_extent`] memoises
    /// its answer per base for the duration of one stop-the-world
    /// `collect_garbage` (`oversize_screen_memo_armed`). Read once per heap.
    pub oversize_screen_memo_on: AtomicBool,
    /// zs s2/rf: set for the duration of one `collect_garbage` (the
    /// `OversizeScreenMemoGuard`); outside it the screen never memoises.
    pub oversize_screen_memo_armed: AtomicBool,
    /// zs s2/rf: `(base, array_length, answer)` per screened base this
    /// pause. The length is part of the key so a base that is re-headed in
    /// the pause (freed, then a slide destination) is screened afresh.
    pub oversize_screen_memo: Mutex<Vec<(usize, u32, Option<usize>)>>,
    /// zs s2/rf: screen answers served from the memo, and screens that ran
    /// the registry scan (the memo's engagement and its positive control).
    pub oversize_screen_memo_hits: AtomicUsize,
    pub oversize_screen_scans: AtomicUsize,
    /// zr z1/al: `forget_empty_registry_blocks` passes that ran, passes that
    /// declined (switch off or an unrewritable peer this cycle), and summary
    /// blocks forgotten in total. The engagement counters for
    /// `CRATONVM_ZGC_FORGET_EMPTY_BLOCKS`: it used to discard its count.
    pub forget_block_passes: AtomicUsize,
    pub forget_block_declined: AtomicUsize,
    pub forget_blocks_total: AtomicUsize,
    /// zr z1/al: published VM-TLAB tails whose start
    /// `CRATONVM_ZGC_SKIP_COVERS_INFLIGHT` lowered, and the bytes it added to
    /// the withheld set. The engagement (and cost) counters for that switch.
    pub inflight_covers_lowered: AtomicUsize,
    pub inflight_cover_bytes: AtomicUsize,
    /// zr z2/rv: the base of every allocation whose extent exceeds
    /// [`ZgcRealHeap::MAX_ALLOC_EXTENT`] -- the objects the interior probes'
    /// bounded backwards scan cannot reach from deep inside. See
    /// [`ZgcRealHeap::huge_extent_containing`]. A slot is free when it is
    /// `0`; a slot is only a CANDIDATE (a lookup re-validates it against the
    /// registry and the header); `huge_hwm` bounds the slots a lookup reads,
    /// and `huge_overflow` (every slot taken) sends a lookup to the unbounded
    /// scan instead. Stale slots (base no longer registered) are cleared at
    /// the end of every collection (`prune_huge_extents`).
    pub huge_bases: Box<[AtomicUsize; Z_HUGE_EXTENT_SLOTS]>,
    pub huge_hwm: AtomicUsize,
    pub huge_overflow: AtomicBool,
    /// Interior probes answered by the huge-extent table (engagement).
    pub huge_extent_hits: AtomicUsize,
    /// zr z4/ab: collections whose `prune_huge_extents` re-seeded the table
    /// out of overflow mode (engagement of `CRATONVM_ZGC_HUGE_EXTENT_RESEED`).
    pub huge_extent_reseeds: AtomicUsize,
    /// zr z8/st: `CRATONVM_ZGC_LAZY_TLAB_STARTS`'s live-chunk table and
    /// counters (`starts_lazy`). Inert unless the switch is on.
    pub lazy_starts: starts_lazy::ZLazyTlabStarts,
    /// zt t18/zb1: the heap is armed in PLAIN-STORE mode
    /// ([`ZgcRealHeap::arm_load_barrier_plain_stores`]): reads go through the
    /// barrier, stores still write plain pointers. Written before the armed
    /// flag (Release), so a reader that sees the flag sees this.
    pub lb_plain_stores: AtomicBool,
    /// zt t18/zb1 (A3, additive): a plain word a load returns while a
    /// concurrent mark is running is published to the marker as well.
    pub lb_mark_on_load: AtomicBool,
    /// zt t18/zb1: the `CRATONVM_DBG_ZGC_LOAD_BARRIER` census -- reads the
    /// armed barrier served (`routed`), split by the word it found (`plain`
    /// pointer / `colored` word; null is the rest), the plain words published
    /// to the marker (`marked`), and armed reads an accessor served raw
    /// (`unrouted`; only the census's own bypass level makes any). Counted
    /// only while the census is on.
    pub lb_routed: AtomicUsize,
    pub lb_plain: AtomicUsize,
    pub lb_colored: AtomicUsize,
    pub lb_marked: AtomicUsize,
    pub lb_unrouted: AtomicUsize,
    /// zt t19/zb2 (A2b): the heap is armed with COLORED STORES
    /// ([`ZgcRealHeap::arm_load_barrier_colored_stores`], tests only): every
    /// reference store the heap's accessors make writes the good-colored word.
    pub lb_colored_stores: AtomicBool,
    /// zt t19/zb2: `CRATONVM_ZGC_LOAD_BARRIER=colored` was asked for and
    /// refused (the heap armed in plain-store mode instead).
    pub lb_colored_refused: AtomicBool,
    /// zt t19/zb2: census -- reference stores encoded as a colored word
    /// (`colored_store_word`), and armed reads that came through the JIT
    /// helpers' seam (`VmHeap::load_ref_slot_barriered`: `jit_aaload`,
    /// `jit_getfield`'s compact reference arm). Counted only while the census
    /// is on.
    pub lb_store_colored: AtomicUsize,
    pub lb_jit_slot: AtomicUsize,
    /// zt t20/zb3: census -- reference slots the slide's rewrite pass wrote as
    /// a good-colored word on a colored-store heap (the GC-side twin of
    /// `lb_store_colored`). Counted only while the census is on.
    pub lb_gc_colored: AtomicUsize,
    /// zt t22/zb5 (stage 2e): the color flip is armed on this colored-store
    /// heap (`CRATONVM_ZGC_COLOR_FLIP`, [`ZgcRealHeap::arm_load_barrier_color_flip`]).
    pub lb_color_flip: AtomicBool,
    /// zt t22/zb5: the flip's phase machine -- the `Marked0` / `Marked1`
    /// rotation. [`ZgcRealHeap`]'s `barrier_good_mask` (what the barrier
    /// reads) is set from it at each flip; nothing reads this one on a load.
    pub lb_flip_mask: vaddr::ZGoodMask,
    /// zt t22/zb5: the bad mask compiled code tests (`TEST RAX, [cell]`), its
    /// address published to the JIT by
    /// `cratonvm_types::narrow_oop::publish_colored_bad_mask_cell`. Boxed so
    /// the address baked into code never moves and the counters keep their
    /// alignment; quiescent (the bad mask of `Remapped`) unless the flip is
    /// armed and a mark phase is open.
    pub lb_bad_cell: Box<vaddr::ZCompiledBadMask>,
    /// zt t22/zb5: flips taken (to a mark color, back to `Remapped`), counted
    /// always (two per concurrent cycle): the flip's engagement numbers on the
    /// `[GC] zgc-load-barrier:` census line.
    pub lb_flips_mark: AtomicUsize,
    pub lb_flips_remap: AtomicUsize,
    /// zt t32 (2f, `CRATONVM_ZGC_LOAD_BARRIER_MARKING`): load-barrier marking
    /// is armed -- the store side logs only the deletion of a PLAIN word
    /// (`lbm_pre_barrier`); a colored word's target is the load barrier's
    /// to mark. Armed with the color flip, never later.
    pub lbm: AtomicBool,
    /// 2f census: deletions of a colored word the store side did not log,
    /// and deletions of a plain word it did.
    pub lbm_colored_skipped: AtomicUsize,
    pub lbm_plain_logged: AtomicUsize,
    /// 2f positive control (`CRATONVM_DBG_ZGC_LBM_NO_LOAD_MARK=1`, only with
    /// `lbm`): the load barrier's slow path marks nothing, so nothing keeps
    /// an object whose only path a mutator deleted after loading it.
    pub lbm_no_load_mark: AtomicBool,
    /// zt t23/zb6 (stage 2f groundwork): census -- PLAIN words a routed load
    /// returned on a colored-store heap while a concurrent mark was running.
    /// The load barrier marks only colored words, so each of these is a
    /// reference whose target the barrier did not mark: only SATB keeps it
    /// alive. The number 2f (SATB off, load-barrier marking) must drive to
    /// zero (or cover with a plain-word mark) first. Counted only while the
    /// census is on.
    pub lb_plain_in_mark: AtomicUsize,
    /// zt t23/zb7 (stage 2f blocker (c)): census -- referent keep-alives
    /// (`Reference.get` and the other `referent_keep_alive` callers) served by
    /// the load barrier's mark step instead of the SATB pre-barrier, i.e.
    /// during a mark on a heap with the color flip armed. Counted only while
    /// the census is on.
    pub lb_referent_keep_alive: AtomicUsize,
}

/// Slots of the huge-extent table (zr z2/rv). One per simultaneously live
/// allocation past 256 MiB: 64 covers a 16 GiB heap full of them; past that
/// the table overflows into the unbounded scan, which is correct and slow.
pub const Z_HUGE_EXTENT_SLOTS: usize = 64;

// HIB-CV-24 (ZGC manifestation): class-unload reconciliation
// (`gc_reconcile_defining_loaders`, via `process_references_after_gc`) runs
// strictly AFTER `collect_garbage` returns, i.e. after sweep AND
// relocation/compaction, using `is_addr_live` (a pure address-keyed registry
// lookup). If compaction slides an unrelated survivor into the exact span a
// dead classloader just vacated in the SAME cycle, that lookup answers "still
// live" for the classloader's old address, wrongly keeping one class loaded
// (the `expected:<8> but was:<9>` shape). These two maps let a caller name
// specific addresses to watch BEFORE the slide and read back their EXACT
// mark-bitmap verdict (the same safe, pre-slide point `process_references`
// already uses `is_marked_addr` from) after the cycle completes — a
// side-channel, not a `GarbageCollector` trait change, so G1/Generational are
// untouched. Keyed by heap pointer identity to support multiple VMs.
pub fn zgc_reconcile_watch_map() -> &'static Mutex<HashMap<usize, Vec<usize>>> {
    static M: std::sync::OnceLock<Mutex<HashMap<usize, Vec<usize>>>> = std::sync::OnceLock::new();
    M.get_or_init(|| Mutex::new(HashMap::new()))
}

pub fn zgc_reconcile_results_map() -> &'static Mutex<HashMap<usize, HashMap<usize, bool>>> {
    static M: std::sync::OnceLock<Mutex<HashMap<usize, HashMap<usize, bool>>>> =
        std::sync::OnceLock::new();
    M.get_or_init(|| Mutex::new(HashMap::new()))
}

pub struct ZgcRealHeap {
    /// Compact-layout domain of the VM that owns this heap. See
    /// `Heap::set_layout_domain`: `class_id` is a per-`ClassStore` index, so
    /// allocating against another domain's registry entry would give the object
    /// a foreign shape. Defaults to the first domain, so an untold heap behaves
    /// as it did before domains existed.
    pub layout_domain: std::sync::atomic::AtomicU32,

    /// This heap's entry in the process-global live-heap registry.
    ///
    /// RAII only — see `gen_heap::RELOCATABLE_HEAPS_LIVE`. This collector
    /// publishes its arena envelope into `MOVABLE_BOUNDS`, which is
    /// single-tenant by slot-0 ownership, so a SECOND live heap leaves one of
    /// the two unrepresented and the frame-band verifier's residency test then
    /// answers `false` for every one of its addresses — a vacuous pass, not an
    /// absence of movable words.
    pub _bounds_registration: cratonvm_gc_core::jit_bounds::RelocatableHeapRegistration,

    /// Backing storage for all objects.
    pub arena: Mutex<Arena>,
    pub jit_ref_store_gates: Box<cratonvm_gc_core::jit_bounds::JitRefStoreGates>,
    /// The page-allocator **survey** over this heap's arena — roadmap C0,
    /// stage 0 (`handoff-s-page-survey-stage-0.md`,
    /// `proposal-c-page-based-heap.md` §4).
    ///
    /// `None` unless `CRATONVM_ZGC_PAGE_SURVEY` is set. It owns nothing,
    /// allocates nothing and **cannot** — `alloc_page_locked` refuses with
    /// `ZPageError::SurveyOnly`, which covers `alloc_page`, `alloc_object` and
    /// `refill_shared`. That refusal is what makes stage 0 free: the arena is
    /// still serving these bytes, and a page carved here would be a range two
    /// allocators both believed they owned.
    ///
    /// What it does is index the arena on the same grid `logical_pages` builds,
    /// so that a page address and an arena offset are the same number plus
    /// [`Self::arena_base`]. That identity is the blocker every later stage of
    /// the page programme names: it makes `sweep.rs`'s
    /// `arena.add_free_block(base - arena_base, size)` correct **by
    /// construction** for any page address, and it is what
    /// `proposal-b-page-based-evacuation.md`'s stage 1 is stuck on
    /// (`alloc_in` has no to-space provably outside the relocation set).
    ///
    /// Nothing reads the grid back yet, deliberately: stage 0 is "the grid is
    /// indexed", not "the grid is consumed". The first consumer to move is
    /// `is_object_address`'s range screen, which becomes an O(1) page-table
    /// probe **in addition to** `ZObjectStarts`, never a replacement for it.
    ///
    /// **Boxed, and that is load-bearing.** `ZPageAllocator` is ~440 bytes and
    /// this struct is on the allocation and barrier paths;
    /// `the_hot_heap_struct_does_not_grow_back` caught it inline at 1640 bytes
    /// against a 1200-byte bound. Nothing on either hot path reads the survey
    /// -- it is default-off diagnostic state -- so it pays one pointer here and
    /// keeps its bulk off the cache lines the allocator touches.
    pub page_survey: Option<Box<page::ZPageAllocator>>,
    /// Immutable arena envelope, captured at construction — see
    /// [`Self::conservative_addr_span`]. Plain `usize`, deliberately outside
    /// the `Mutex`: the conservative-root filter must answer without locking.
    pub arena_base: usize,
    /// Exclusive upper bound of the arena envelope.
    pub arena_end: usize,
    /// Base address of every live allocation. Pruned (dead bases removed in
    /// place) by each sweep.
    ///
    /// Membership must be O(1): `is_object_address` is consulted per
    /// conservative-root candidate (every operand-stack root and JIT-frame
    /// qword), and the original `Vec` linear scan made every GC's root
    /// collection O(roots × live) and read as a hang at scale (ZGC-5/6
    /// hardening). The sweep prunes DEAD bases IN PLACE and never
    /// wholesale-replaces the structure, so an allocation registered between
    /// the mark snapshot and the sweep publish cannot be silently dropped.
    ///
    /// It was an `FxHashSet<usize>` behind a `Mutex` until 2026-08-07, when
    /// `bench/BinTreesClassic.java` measured this backend at 6.6x/11.3x/18.4x
    /// the generational collector at bt12/14/16 — superlinear, with exactly ONE
    /// collection in the whole bt16 run, so the cost was neither the collector
    /// nor the arena lock but a per-allocation global-mutex hash insert whose
    /// table grows with the live set. It is now an object-start BITMAP; see the
    /// "Object-start membership" section header above this struct for the
    /// measurement, the 2026-07-26 `gen_heap` precedent it copies, the exactness
    /// argument, and the `CRATONVM_ZGC_STARTBITS` kill switch.
    pub registry: ZObjectStarts,
    /// Monotonic identity-hash-code source (matches `Heap::next_hash`).
    pub next_hash_code: AtomicI32,
    /// Bytes of live+dead object payload currently outstanding (drops on
    /// sweep). Used by [`Self::needs_gc`] and [`Self::allocated_bytes`].
    pub allocated: AtomicUsize,
    /// Collection is triggered once `allocated` crosses this byte count.
    pub gc_threshold: usize,
    /// Post-GC re-arm floor: `needs_gc` stays `false` until `allocated`
    /// also crosses this. Set by each sweep to
    /// `live + max(remaining_headroom / 4, 64 KiB)` so a live set that sits
    /// above the static 75% threshold cannot latch `needs_gc` permanently
    /// true — which made `maybe_gc` (polled after EVERY allocation
    /// bytecode) run a full STW mark-sweep per allocation: a livelock-grade
    /// GC storm with no OOME ever surfacing.
    pub gc_rearm: AtomicUsize,
    /// "`needs_gc` went true while allocating" latch, read at the native-call
    /// boundary via `VmHeap::young_spill_pressure`.
    ///
    /// Why it exists: this collector's `GarbageCollector::alloc_object` /
    /// `alloc_array` are INFALLIBLE — they end in
    /// [`Self::report_fatal_exhaustion`] followed by `std::process::abort()`.
    /// A workload that allocates only from inside native wrappers reaches no
    /// safepoint of its own, so without a signal the one hook that can collect
    /// on a native's behalf (`vm/src/vm/vm_exec.rs`) never fires here and the
    /// process dies by `abort()` on a heap full of garbage, with no Java-visible
    /// `OutOfMemoryError` ever thrown. G1 closed exactly this defect with
    /// `G1Collector::native_alloc_pressure`, armed by
    /// `note_region_consumed_locked` (`g1.rs:1765-1774`); this field is the ZGC
    /// analogue, armed from [`Self::alloc_raw`].
    ///
    /// It CANNOT recreate the `gc_rearm` GC-storm livelock documented on the
    /// field above, because the arming predicate is `needs_gc`'s predicate
    /// verbatim — `allocated >= gc_threshold && allocated >= gc_rearm` — so it
    /// inherits the re-arm floor. A live set parked above the static threshold
    /// leaves `gc_rearm` above `allocated` after every sweep, so the latch
    /// simply stays down until genuinely new allocation clears the floor. It
    /// adds a signal below no gate that `needs_gc` does not already have.
    ///
    /// `Relaxed` throughout, matching G1: this is an advisory diagnostic
    /// feeding a policy decision ("should this native boundary collect?"), not
    /// a correctness handshake. Nothing is published *through* the bit — the
    /// consumer re-reads the heap's own counters and re-checks its own gates
    /// before acting — so no acquire/release pairing is needed, and a read that
    /// observes the store one boundary late merely defers a collection to the
    /// next boundary.
    pub native_alloc_pressure: AtomicBool,
    /// A request that the arena **actually refused**, as distinct from the
    /// advisory pressure above.
    ///
    /// # Why the two cannot be one bit
    ///
    /// `native_alloc_pressure` is consumed through
    /// `VmHeap::young_spill_pressure`, whose boundary consumer re-checks
    /// `needs_gc()` before it collects — deliberately, so that an advisory note
    /// buys one gate evaluation and cannot storm. That is right for a *soft*
    /// signal and wrong for a hard one, and [`Self::alloc_raw`] latches the
    /// same bit for both. Its own comment says why the re-check is wrong there:
    /// a request that just failed "is stronger evidence that a cycle is due
    /// than the `allocated >= gc_threshold` predicate, which counts LIVE bytes
    /// and therefore cannot see the bump space this heap never rewinds."
    ///
    /// So the arming site and the consuming site disagreed, and the consuming
    /// site won: on the exact shape this collector fails in — an arena full of
    /// TLAB *reservations* with `allocated` far below the threshold, i.e.
    /// Tomcat's `TestNonBlockingAPI` on 2026-08-13 — `needs_gc()` answered
    /// **no**, the latch was cleared without collecting, and the one signal
    /// that knew better was discarded. This bit is that signal, kept separate
    /// so the boundary can honour it without loosening the soft path.
    ///
    /// Cannot storm: it is set only where an allocation genuinely failed, the
    /// consumer clears it after acting, and `gc_overhead_limit_exceeded` still
    /// gates it — the same bound the soft path relies on.
    pub hard_alloc_failure: AtomicBool,
    /// Whether a concurrent mark cycle is in progress — Phase 3.
    ///
    /// This is the **only** thing on the mutator store path while no cycle is
    /// running: [`Self::satb_pre_barrier`] loads it and returns. Everything
    /// else behind the barrier is reachable only when it is `true`.
    pub mark_active: AtomicBool,
    /// A `Weak` handle to this heap, installed by [`Self::new_shared`].
    ///
    /// # Why the heap needs a handle to itself
    ///
    /// Genuine concurrent marking needs a [`mark::ZMarkCoordinator`], and that
    /// takes an `Arc<dyn mark::ZMarkContext>`. `ZgcRealHeap` **is** the
    /// context (see `impl mark::ZMarkContext for ZgcRealHeap`), so opening a
    /// cycle from `&self` requires turning `&self` back into an
    /// `Arc<ZgcRealHeap>`. This is the only supported way to do that, and it
    /// is why `VmHeap::Zgc` holds an `Arc`.
    ///
    /// `None` for a heap built with [`Self::with_capacity`] directly -- every
    /// unit test that does so gets a heap on which
    /// [`Self::start_concurrent_mark`] refuses and the collector stays
    /// stop-the-world. That is deliberate: a test heap is a local that Rust
    /// may move, and a moved heap with live worker threads is exactly the
    /// hazard the `Arc` exists to remove.
    pub self_weak: std::sync::OnceLock<std::sync::Weak<ZgcRealHeap>>,
    /// `true` between [`Self::start_concurrent_mark`] and the mark-end
    /// handshake in `collect_garbage`.
    ///
    /// Distinct from [`Self::mark_active`], which is the *barrier's* arming
    /// flag: the barrier is disarmed a few statements before the cycle is
    /// closed, and a reader that conflated the two would see a window in which
    /// the mark bits are live but nothing says a cycle owns them.
    pub conc_cycle_active: AtomicBool,
    /// Is every registered object's [`GC_FLAG_MARKED`] known to be clear?
    ///
    /// # Why this exists: a 34-66 ms pause spent clearing bits that are clear
    ///
    /// `start_concurrent_mark` used to walk the whole registry clearing the mark
    /// bit. The 2026-08-17 pause anatomy measured that walk at **94-96% of the
    /// mark-start pause** -- 34 ms of 35 on a 4.6M-entry registry, 66 of 69 on a
    /// 10.8M one -- and then a counter showed `stale_marked=0` on **every one of
    /// 20 mark starts**, at two different window settings. The walk was clearing
    /// a bit that was already clear, every time.
    ///
    /// It is redundant because the sweep is exhaustive: it visits every
    /// registered object and clears `GC_FLAG_MARKED` on every survivor, zeroes
    /// every corpse, and clears the bit even on the object it refuses to size.
    /// Objects allocated afterwards are born with clear flags, and
    /// `allocate_black_if_marking` is a no-op with no cycle open.
    ///
    /// It is NOT unconditionally redundant, which is why this is a latch and not
    /// a deletion: [`Self::abandon_concurrent_mark`] drops a partially-traced
    /// cycle with **no sweep following it**, so the bits that trace set are still
    /// there. That is the one path that clears this flag.
    ///
    /// Starts `true`: a fresh heap has no objects, so the claim holds vacuously.
    pub conc_bits_known_clear: AtomicBool,
    /// `allocated` at which a concurrent cycle opens, precomputed from
    /// [`conc_start_percent_setting`] and [`Self::gc_threshold`]. `0` means
    /// concurrent marking is off for this heap, which is the default.
    ///
    /// Atomic only so a test can arrange a trigger without setting a process-
    /// wide environment variable -- `conc_start_percent_setting` caches its
    /// answer in a `OnceLock`, so a test that set `CRATONVM_ZGC_CONC_START`
    /// would decide the value for every other test in the binary. Nothing
    /// writes it at runtime.
    pub conc_start_bytes: AtomicUsize,
    /// Phase 4: the barrier's good mask, and the phase machine behind it.
    ///
    /// `Z_REMAPPED` — "no mark parity is good; addresses are plain" — until a
    /// cycle arms it. The barrier gates on this and needs no separate
    /// activation flag; see `zgc::barrier::ZBarrierContext::good_mask`.
    pub barrier_good_mask: AtomicU64,
    /// zt t33: the color a heal or a colored store writes (`heal_color()`).
    /// The good mask, except from the closing pause's remap flip until a
    /// slide starts or the pause ends, when R6 may plan: then the mark color
    /// the flip left, so every word the pause heals or stores reads BAD after
    /// the release, as every slot the concurrent marker visited does, and the
    /// plan needs no walk for it ([`Self::end_closing_heal_color`]).
    pub barrier_heal_mask: AtomicU64,
    /// Phase 4: whether a relocating cycle is in progress. Distinct from
    /// [`Self::mark_active`] because the barrier's mark and relocate slow
    /// paths are separately armed.
    pub relocate_active: AtomicBool,
    /// Whether reference slots currently hold COLOURED words, and so whether
    /// the read-path load barrier may run at all.
    ///
    /// Deliberately not inferred from the good mask. `Z_REMAPPED` is both the
    /// quiescent "addresses are plain" state AND a real ZGC colour, so
    /// `good_mask() != Z_REMAPPED` answers "is a mark parity good", which is a
    /// different question and is false during the remap phase — exactly when
    /// the barrier is most needed.
    pub barrier_armed: AtomicBool,
    /// Old-to-young edges, per old page -- `zgc::remembered`.
    ///
    /// Fed by [`Self::satb_pre_barrier`], which every reference store in the
    /// VM already reaches. Consumed as extra roots by a young-scoped cycle,
    /// which is the whole reason a generational collector can look at less
    /// than the whole heap.
    pub remembered: remembered::ZRememberedSetTable,
    /// Generational mode, and its two tunables, **per heap**.
    ///
    /// Seeded from [`zgc_generational_enabled`], [`zgc_gen_promotion_age`] and
    /// [`zgc_gen_minors_per_major`] at construction, then owned by this
    /// instance -- the same shape as `tlab_enabled`. `zgc::generation`'s module
    /// header says why ("Every piece of state is instance-owned. There is no
    /// `static`, no `OnceLock`, and no process-global cache -- this tree has had
    /// parallel-test crashes caused by process-global GC caches, and a VM host
    /// may own more than one heap at a time"), and it is also the only way a
    /// test can exercise the phase: the flag readers cache in a `OnceLock`, so
    /// the first test to touch one fixes it for the whole process.
    /// The stop-the-world slide, per heap -- see
    /// [`Self::relocation_requested_by_default`].
    pub relocation_enabled: AtomicBool,
    pub generational_enabled: AtomicBool,
    pub gen_promotion_age: std::sync::atomic::AtomicU32,
    pub gen_minors_per_major: AtomicUsize,
    /// Must the next collection be whole-heap?
    ///
    /// # Why an escalation latch exists instead of a stricter trigger
    ///
    /// The first version of the trigger forced a major whenever `headroom_low`
    /// was set, reasoning that a young cycle retains the whole old generation
    /// unexamined and so is the wrong tool for "the heap is full". **The
    /// 2026-08-17 measurement showed that reasoning turns the phase off
    /// entirely.** On a heap that is large relative to its live set, the
    /// live-bytes threshold is never reached and *every* collection is
    /// allocation-driven — so every collection was forced major and
    /// `young_cycles` was **0 on every arm**, with the flag on and 3.2M
    /// promotions recorded. The engagement counter is the only reason that read
    /// as a vacuous measurement rather than as "generational does not help".
    ///
    /// `headroom_low` is a *soft* signal (the arena cannot serve a
    /// `zgc_headroom_margin` request) and a young cycle answers it perfectly
    /// well when the garbage is young, which on that workload was 800 MB of 1.2
    /// GB. So the trigger now tries young and **escalates on evidence**: a young
    /// cycle that reclaimed nothing sets this, and the next collection is
    /// whole-heap. `hard_alloc_failure` — an allocation that actually failed
    /// after a collection — still forces a major immediately, because there the
    /// evidence is already in.
    ///
    /// The cost of being wrong is one cycle, and it is self-correcting. The cost
    /// of the stricter rule was the whole feature.
    ///
    /// zt t14/gn5: not when nothing was promoted since the last major and the
    /// empty cycle saw a live nursery
    /// ([`ZgcRealHeap::gen_empty_minor_escalation_skipped`],
    /// `CRATONVM_ZGC_GEN_EMPTY_MINOR_NEEDS_PROMOTION`): HotSpot 25 runs
    /// minors there, and the major would free nothing either.
    pub gen_force_major_next: AtomicBool,
    /// The arena's low cursor at the end of the last WHOLE-HEAP collection.
    /// `0` means no boundary has been established yet.
    ///
    /// # The nursery this defines, and why it is an address and not an age
    ///
    /// Phase G splits the generations by object age, which is what decides how
    /// much a young cycle has to TRACE. It does nothing about how much a young
    /// cycle has to SWEEP, and the 2026-08-17 measurement found the sweep was
    /// the pause: 182 ms of a 309 ms mean, identical with the split on, because
    /// the sweep walks every registered object whatever the split says.
    ///
    /// The sweep can only be bounded by ADDRESS, because the registry is a
    /// bitmap over the arena and a range of addresses is a range of words.
    /// Everything allocated since the last whole-heap collection lies at or above
    /// its final cursor, so `[gen_young_floor, cursor)` is a nursery in the
    /// ordinary bump-allocator sense — and a young cycle sweeps only that.
    ///
    /// # What it costs, stated
    ///
    /// The free list hands out space BELOW the floor, so an object allocated
    /// into a hole left by a previous sweep is inside the old region. The
    /// young-grain set (`young_pages`, see [`Self::note_young_page`]) is what
    /// sends the young sweep there. *Corrected 2026-10-12 (zr z1/gn): this
    /// paragraph said such an object "is still pre-marked"; it is not -- the
    /// pre-mark is by header age, and a fresh object is age 0.*
    ///
    /// # What the floor still leaves behind (zr z1/gn)
    ///
    /// An object below the floor whose age is BELOW the promotion age -- every
    /// survivor of a whole-heap cycle that has not yet been promoted, and every
    /// nursery survivor a slide moved under the new floor -- is traced when it
    /// is reachable but, when it dies, is neither marked nor swept by a young
    /// cycle. It stays registered, so the VM's post-collection liveness test
    /// (`VmHeap::is_addr_live`, an exact-registry probe on this backend)
    /// answers "survived" for an object the mark called dead, while the young
    /// children only it reached ARE swept. See
    /// `docs/feature-designs/zgc-architecture.md`;
    /// `sweep_young_aged` (behind `CRATONVM_ZGC_GEN_OLD_BITMAP`) closes it.
    ///
    /// *Corrected 2026-10-12:* a relocation no longer invalidates the floor or
    /// arms [`Self::gen_force_major_next`]; since G2 the slide MOVES the floor to
    /// the post-slide cursor (the PHASE G block after `relocate_stw_admitted`).
    pub gen_young_floor: AtomicUsize,
    /// Per nursery page, did it hold a survivor? The one counter that can
    /// refute `docs/feature-designs/zgc-architecture.md`
    /// before it is built.
    ///
    /// See [`generation::ZNurserySurvivorCensus`] for the decision rule, which
    /// is stated there BEFORE the run rather than chosen after the numbers:
    /// `survivor_page_percent() >= 90` over at least three young cycles refutes
    /// the young-space programme, `<= 60` supports it, and anything between is
    /// inconclusive. Surfaced on `[GC] zgc-nursery:` through
    /// [`Self::nursery_survivor_stats`].
    ///
    /// The statistic is deliberately **one bit per logical page**, not a
    /// survival *rate*: a nursery can be 1% live and still have a survivor on
    /// every page, and that is precisely the case that defeats page-granular
    /// reclamation while a byte-ratio calls it healthy.
    ///
    /// **Boxed for the same reason [`Self::page_survey`] is.** Wave 4 gave the
    /// census four per-cycle percentage fields (`cycle_pct_first/last/min/max`)
    /// so an aggregate biased by the carry-over ratchet could be told apart
    /// from a real one. That is per-cycle INSTRUMENTATION, which
    /// `the_hot_heap_struct_does_not_grow_back` exists to keep off this
    /// struct's cache lines; `note_survivor` runs during a young SWEEP, never
    /// on the allocation or barrier path, so the indirection is not on a hot
    /// path and the bytes are.
    pub gen_nursery_survivors: Box<generation::ZNurserySurvivorCensus>,
    /// Live bytes below [`Self::gen_young_floor`], as of the last whole-heap
    /// collection.
    ///
    /// A young cycle's sweep does not visit those objects, so it cannot count
    /// them — and `allocated` (which drives `gc_rearm` and the collection
    /// threshold) has to keep meaning "live bytes in this heap" or the trigger
    /// mis-fires in whichever direction the omission points. Carried forward
    /// instead of recomputed, which is the whole point.
    pub gen_old_live_bytes: AtomicUsize,
    /// Bytes of fresh allocation that fill the nursery, precomputed from
    /// [`zgc_gen_nursery_percent`] so the allocation path never divides.
    pub gen_nursery_bytes: AtomicUsize,
    /// Bytes of fresh allocation that trigger a collection on their own,
    /// precomputed from [`zgc_alloc_trigger_percent`] so the allocation path
    /// never divides. `0` disables the clause.
    ///
    /// Unlike [`Self::gen_nursery_bytes`] this is armed on EVERY run, not only
    /// a generational one: it is the answer to "how much garbage is worth a
    /// cycle", which is a question a non-generational heap has too.
    ///
    /// Atomic because [`Self::refresh_pause_target_budget`] REWRITES it at the
    /// end of every collection while a pause target is CONSTRAINING -- that is
    /// the whole of the pause-target mode's effect on the trigger -- and so a
    /// test can flip the clause on a live heap and assert both arms of it.
    pub alloc_trigger_bytes: AtomicUsize,
    /// What [`zgc_alloc_trigger_percent`] alone asked for, fixed at
    /// construction. `0` when the percentage form is off, which is the default.
    ///
    /// The pause-target loop is a CEILING and starts unconstrained; when it
    /// releases its constraint -- pauses came back comfortably under the
    /// target, or the target turned out to be unachievable -- it has to hand
    /// [`Self::alloc_trigger_bytes`] back to something, and that something is
    /// this. Without it, releasing would leave whatever the last constrained
    /// cycle happened to compute.
    pub alloc_trigger_percent_bytes: usize,
    /// The pause this collector tries to stay under, in MILLISECONDS. `0`
    /// means no target, and then [`Self::alloc_trigger_bytes`] keeps whatever
    /// [`zgc_alloc_trigger_percent`] gave it at construction.
    ///
    /// Seeded from [`zgc_pause_target_ms`]; `-XX:MaxGCPauseMillis` overwrites
    /// it through [`Self::set_pause_target_ms`] after construction.
    pub pause_target_ms: AtomicU64,
    /// The span that produced the most recent OVERRUN, in bytes. `0` means
    /// "no overrun is remembered".
    ///
    /// The relax path may not widen [`Self::pause_affordable_span`] past
    /// seven-eighths of this. Without that memory the loop re-probes the
    /// region it already knows overruns: it relaxes a quarter at a time, the
    /// span climbs back to where the last long pause happened, the pause
    /// overruns again, and it cuts. Measured on `G1ChurnPauseProbe 50 1800` at
    /// `-Xmx2048m` with a 200 ms target, that cycle cost 13% of wall and left
    /// the worst pause at 290 ms against an unconstrained 296 -- all of the
    /// cost of a ceiling and none of the benefit. This is the `ssthresh` of a
    /// multiplicative-decrease loop, and leaving it out is the same mistake.
    ///
    /// Forgotten when a pause comes in at or below a QUARTER of the target:
    /// that is a span far cheaper than the remembered overrun implies, which
    /// means the workload changed and the memory is now about a heap that no
    /// longer exists.
    pub pause_overrun_span: AtomicU64,
    /// The span, in BYTES, that a pause of [`Self::pause_target_ms`] can
    /// afford to walk -- the controller's state. `0` means "never measured",
    /// and nothing can be projected from it, which is why the first cycle of
    /// any run is still triggered by the occupancy clauses. You cannot predict
    /// a pause you have never taken.
    ///
    /// The budget is this minus the live bytes. Held in the units of the thing
    /// being controlled rather than as a cost coefficient, which is what makes
    /// the rate limit in [`ZgcRealHeap::refresh_pause_target_budget`]
    /// expressible at all.
    pub pause_affordable_span: AtomicU64,
    /// `allocated` as of the end of the last collection.
    ///
    /// `allocated - cycle_alloc_watermark` is BYTES ALLOCATED SINCE THE LAST
    /// COLLECTION. Exact, O(1), and it needs no counter of its own: `allocated`
    /// is already incremented on the allocation path for the live-bytes
    /// trigger, and already read by `needs_gc`.
    ///
    /// Two clauses of [`needs_gc`](GarbageCollector::needs_gc) read it: the
    /// nursery-size trigger (generational only — there it is the nursery's
    /// zero, and the difference IS the nursery's size on a bump-first heap),
    /// and the allocation-rate trigger below it, which is on for every run.
    /// It was called `cycle_alloc_watermark` while only the first existed.
    pub cycle_alloc_watermark: AtomicUsize,
    /// Did the nursery-size clause of `needs_gc` fire? Consumed by the
    /// collection it asked for.
    ///
    /// A latch and not a `fetch_add`, because `needs_gc` is POLLED on the
    /// allocation path: the condition stays true from the moment it is reached
    /// until the collection actually runs, so counting on each observation would
    /// report allocations, not collections.
    pub gen_nursery_triggered: AtomicBool,
    /// Zero only a dead object's header on a young sweep — see
    /// [`zgc_sweep_header_zero`]. Per heap so a test can A/B it without deciding
    /// the question for every other test in the binary.
    pub gen_header_zero_only: AtomicBool,
    /// Hand the free list one span per run of adjacent dead objects on a young
    /// sweep — see [`zgc_sweep_dead_runs`]. Per heap for the same reason.
    pub gen_dead_runs_enabled: AtomicBool,
    /// Has any object been promoted yet? The card barrier's gate.
    ///
    /// # Why a flag and not a lock
    ///
    /// The card barrier now sits on the store ACCESSOR (see
    /// [`Self::set_field_no_satb`]), which is every reference store in the VM.
    /// Its original gate asked `old_page_ids.lock().is_empty()` -- fine for a
    /// barrier nothing called, and a mutex acquire per `putfield` for one that
    /// is called. This is the same shape as the SATB gate: one relaxed load and
    /// a not-taken branch for the whole of a run that never promotes, which is
    /// every run with `CRATONVM_ZGC_GENERATIONAL` unset.
    ///
    /// Monotone. An object's age never falls, so once one has been promoted the
    /// barrier stays armed until [`Self::reset_generational_state`].
    pub has_old_objects: AtomicBool,
    pub minors_since_major: AtomicUsize,
    /// The same three questions for the LARGE-OBJECT end's compactor, plus the
    /// one the low end does not need: how often it looked and declined.
    ///
    /// `compact_high_region` runs on every relocating cycle and does nothing
    /// unless merging this end's holes would gain at least one large object's
    /// worth of contiguity, so `high_compaction_cycles=0
    /// high_compaction_declined=812` is the reading for "this workload never
    /// fragmented its large-object end" and is a different fact from
    /// `high_compaction_declined=0`, which would mean the pass never ran at
    /// all. The predecessor feature this replaces
    /// (`CRATONVM_ZGC_TARGETED_COMPACTION`) shipped with `targeted_pages=0` on
    /// every workload measured, and the only reason anyone found out is that it
    /// carried a counter.
    /// Per heap, seeded from `CRATONVM_ZGC_HIGH_COMPACTION` at construction —
    /// the same shape as [`ZgcRealHeap::relocation_enabled`] and for the same
    /// reason. A `OnceLock` on the variable would latch the answer for the
    /// whole PROCESS, so the A/B this switch exists to be could not be run
    /// inside one test binary, and `a_pinned_large_object_...`'s control arm
    /// would silently inherit whichever arm ran first.
    /// Spans and bytes the slide emptied and handed BACK to the free list.
    ///
    /// The engagement counter for the 2026-08-29 repair described on
    /// `Arena::compact_low_to`. `spans=0` on a run with a non-zero
    /// `compaction_cycles` means every cycle's vacated space was inside the
    /// cursor drop, which is the benign case; a large `bytes` is the measure of
    /// what used to be lost outright.
    pub publish_vacated_enabled: AtomicBool,
    pub high_compaction_enabled: AtomicBool,
    /// Addresses this barrier has published since the cycle began. Telemetry
    /// for the adoption work — it is how you tell "the barrier is wired" from
    /// "the barrier is wired and the workload actually overwrites references",
    /// which are the two states an inert-looking instrument confuses.
    /// "The arena can no longer serve a request of [`headroom_margin`] bytes."
    ///
    /// # Why the live-bytes trigger is not enough on THIS backend
    ///
    /// [`needs_gc`](GarbageCollector::needs_gc) asks `allocated >= gc_threshold`,
    /// and the sweep stores *retained* bytes back into `allocated` — so it is a
    /// LIVE-BYTES question. On a compacting heap that is the right question,
    /// because live bytes and allocatable space move together.
    ///
    /// This heap does not compact. The arena's bump cursor never rewinds, and
    /// reclaimed space comes back only as free-list holes. Allocatable space is
    /// therefore `max(capacity - cursor, largest_free_block)`, and it falls as
    /// the *garbage* grows — a quantity `allocated` cannot see, because the
    /// sweep subtracts exactly that garbage from it.
    ///
    /// The two diverge by however much garbage there is, and the divergence is
    /// not academic: on `ZipContentTests` at `-Xmx 2g`, ten collections ran and
    /// an 8 KB array allocation still failed with live at **1,434,932,032 of
    /// 2,147,483,648 bytes** — 66.8%, well under the 75% threshold. `needs_gc`
    /// answered "no collection needed" while the allocation that raised
    /// `OutOfMemoryError` was failing, because it was answering about live
    /// bytes and the wall the workload hit was allocatable space.
    ///
    /// Armed from [`Self::alloc_raw`] under the arena lock, consulted by
    /// `needs_gc`, cleared by the sweep. It is gated by the same `gc_rearm`
    /// floor as the threshold term, so it cannot re-create the GC storm that
    /// field exists to prevent: right after a sweep `gc_rearm` exceeds
    /// `allocated`, so a still-low headroom simply waits for genuinely new
    /// allocation instead of firing a cycle per allocation.
    ///
    /// **And that gate is also how this signal goes unheard.** The floor is
    /// sized from LIVE bytes and this flag is about ALLOCATABLE SPACE, so on a
    /// heap whose last cycle left a small live set the floor sits hundreds of
    /// megabytes above `allocated` and the clause cannot fire however short the
    /// arena is. See the long note at the `needs_gc` call site and
    /// [`Self::headroom_bypasses_rearm`].
    pub headroom_low: AtomicBool,
    /// `CRATONVM_ZGC_HEADROOM_BYPASSES_REARM=1` — let [`Self::headroom_low`]
    /// ask for a collection without clearing the `gc_rearm` floor, while the
    /// last cycle reclaimed something.
    ///
    /// **Default off**, because it changes how often the DEFAULT collector runs
    /// and that is a suite-time decision, not an argument. Seeded per heap from
    /// [`zgc_headroom_bypasses_rearm`] and settable by
    /// [`Self::set_headroom_bypasses_rearm`] so a test can drive both arms in
    /// one binary. The reasoning, and the measurement that would earn a flip,
    /// are at the `needs_gc` call site and in
    /// `docs/feature-designs/zgc-architecture.md`.
    pub headroom_bypasses_rearm: AtomicBool,
    /// Arm the extra-root filter across the concurrent mark phase — stage 3 of
    /// `handoff-r-wire-the-concurrent-root-filter.md`.
    ///
    /// Seeded from [`zgc_conc_root_filter`], overridable per heap by
    /// [`Self::set_conc_root_filter`]. See
    /// [`Self::conc_root_filter_armed`], which is the only reader.
    pub conc_root_filter: AtomicBool,
    /// Did the LAST collection reclaim nothing at all?
    ///
    /// The anti-storm term for the bypass above, and the only thing standing
    /// between it and a collection per allocation on a heap that is genuinely
    /// full of live data: `headroom_low` is cleared by every collection and
    /// re-raised by the next allocation that finds the arena short, so without
    /// this the bypass would loop. `bytes_freed == 0` and not a ratio --
    /// "reclaimed literally nothing" needs no tuning parameter and cannot be
    /// wrong about the direction, which is the argument
    /// [`Self::gen_force_major_next`] already makes for the escalation latch
    /// one clause over.
    ///
    /// Starts `false`: a heap that has never collected has no evidence that
    /// collecting is futile, and the fail-open direction here is "try a cycle".
    pub last_cycle_reclaimed_nothing: AtomicBool,
    /// Lifetime collection counter (observability).
    pub gc_count: AtomicUsize,
    /// `--verbose:gc` per-collection logging gate — see
    /// [`Self::enable_gc_logging`]. Off by default; flipped by
    /// `VmHeap::enable_gc_logging`. Mirrors G1's `gc_log_enabled`
    /// (`g1.rs:6770-6777`). `Relaxed`: a logging toggle orders nothing.
    pub gc_log_enabled: AtomicBool,
    /// Is [`Self::mark_ref_skip_bloom`] meaningful — i.e. is a cycle open?
    ///
    /// Separate from the filter because "no cycle" and "the filter says no" are
    /// different answers: the first must trace referents as strong edges and warn
    /// (see [`Self::concurrent_mark_skip_set`] for why leaking beats dropping an
    /// edge), the second must not. Published `Release` after the filter is
    /// filled and read `Acquire`, so a worker that sees it armed sees the bits.
    pub mark_ref_skip_armed: AtomicBool,

    /// Per-thread allocation buffers carved out of [`Self::arena`] — the
    /// adoption of `gc::zgc::tlab`'s hook/statistics contract onto this heap.
    ///
    /// # Why (the measurement, not a guess)
    ///
    /// Every allocation on this backend used to serialise on
    /// [`Self::arena`]'s single `Mutex<Arena>`, and that mutex is not held for
    /// a pointer bump: it covers `Arena::alloc`'s free-list tier scans (the
    /// large tier is a linear scan of every reclaimed span) *and* the
    /// per-object `write_bytes` zeroing. The generational and G1 backends have
    /// TLABs; this one had none. The full Spring Boot suite under
    /// `-XX:+UseZGC` measured 1860 PASS / 49 HANG / 22 FAIL against
    /// 1902 / 18 / 11 on the default collector — **35 classes PASS -> HANG**,
    /// concentrated on `*AutoConfigurationTests`, i.e. `ApplicationContext`
    /// boot/teardown, the most allocation-heavy workload in the suite
    /// (`zgc-real-fullsuite-regression-RETIRED-20260807.md`).
    ///
    /// That record does not attribute the hangs to this lock and neither does
    /// this field: a global lock on every allocation is the wrong answer at
    /// any heap size, and it is the cheaper of the two suspects to remove.
    ///
    /// See [`ZArenaTlabRegistry`] for why the buffers are carved from the
    /// arena rather than from `zgc::tlab`'s [`crate::zgc::page::ZPageAllocator`].
    pub tlabs: ZArenaTlabRegistry,

    /// Kill switch for [`Self::tlabs`], seeded from `CRATONVM_ZGC_TLAB` at
    /// construction and flippable at runtime through
    /// [`Self::set_tlab_enabled`]. **Default on.**
    ///
    /// Seeded per heap rather than latched in a `OnceLock` (or declared in
    /// `cratonvm_types::GcFlags`) deliberately: a latched process-global gate
    /// makes `set_var` invisible to a test that runs after the first read, and
    /// the point of a kill switch is that a suite run can A/B it. `Relaxed`:
    /// nothing is published through the bit — a mutator that observes the flip
    /// one allocation late merely takes the other path for one object.
    ///
    /// Turning it OFF does **not** disable [`Self::retire_all_tlabs`]: chunks
    /// already handed out must still be closed and returned, or their tails
    /// leak.
    pub tlab_enabled: AtomicBool,

    /// This cycle's mark bits, OUT of the object headers.
    ///
    /// `None` restores [`GC_FLAG_MARKED`] in the header byte-for-byte, so the
    /// A/B is a re-run and not a rebuild -- see [`zgc_mark_bits_enabled`]. Also
    /// `None` for a degenerate arena, where the grid covers nothing.
    ///
    /// # The three costs this removes
    ///
    /// A mark bit in the header means every mark operation is a
    /// read-modify-write of a 64-byte line somewhere in the arena, and there
    /// are three passes that do nothing else:
    ///
    ///  1. **The mark-start clearing walk.** One RMW per registered object,
    ///     scattered. Measured at 94-96% of the mark-start pause (34 ms of 35
    ///     at 4.6M objects; 66 of 69 at 10.8M). It is now
    ///     [`ZObjectStartBits::clear_all`] -- a linear store over `span / 512`
    ///     bytes, 8 MB for a 4.2 GB heap, independent of the object count.
    ///  2. **The trace's set.** The marker reads the object's fields anyway, so
    ///     the LINE is touched either way -- but a header write DIRTIES it, and
    ///     a dirty line has to be written back. Marking 13M objects meant
    ///     ~832 MB of writeback for 13M bits of information. In the bitmap the
    ///     same information is `live_objects / 8` bytes, densely packed.
    ///  3. **The sweep's per-survivor clear.** On a non-generational run the
    ///     sweep otherwise only READS a survivor's header (`alloc_size`), so
    ///     dropping this write means a whole-heap sweep no longer dirties the
    ///     entire live set.
    ///
    /// # Geometry
    ///
    /// Constructed from the same `(arena_base, capacity)` as
    /// [`Self::registry`], through the same constructor, so the two bitmaps
    /// agree by construction about which addresses are representable. Costs a
    /// second 1/64th of the arena, committed the same way and demand-faulted
    /// the same way.
    ///
    /// # The invariant
    ///
    /// **Between collections every bit is clear**, exactly as
    /// [`GC_FLAG_MARKED`] was: the sweep ends with a `clear_all`. That is
    /// deliberately the same observable state as before rather than the
    /// cheaper "leave them set, they are stale anyway" -- the between-cycles
    /// readers ([`Self::is_marked_addr`], the census view) would otherwise all
    /// need to learn about epochs, and each one that did not would silently
    /// report last cycle's answer.
    pub mark_bits: Option<ZObjectStartBits>,

    /// Threads the stop-the-world sweep runs on. `1` is serial, and is the
    /// default -- see [`Self::sweep_workers`] for why a phase that should
    /// parallelise ships off until it is measured.
    ///
    /// Seeded from `CRATONVM_ZGC_PARSWEEP` at construction and owned by this
    /// instance thereafter, the same shape as `tlab_enabled`.
    pub sweep_worker_count: AtomicUsize,
    /// Threads the compaction's whole-live-set passes (the page census, the
    /// partition, the ceiling screen and the reference rewrite) run on. `1`
    /// is serial. Seeded from `CRATONVM_ZGC_PAR_RELOCATE` at construction (see
    /// [`Self::relocate_workers_requested`]) and owned by this instance
    /// thereafter, the same shape as `sweep_worker_count`, so a test can run
    /// both arms in one process. Round 9 wave 7, `zgc7`.
    pub relocate_worker_count: AtomicUsize,
    /// The address range the last slide's forwarding records occupy, and the
    /// only range [`Self::forwarding_word_at`] will dereference.
    ///
    /// Empty (`hi == 0`, `lo == usize::MAX`) between slides. It exists because
    /// the heap commits lazily: an arbitrary in-arena address is not
    /// necessarily mapped, so a read has to be bounded by something the
    /// collector VERIFIED rather than by an argument about where a slide leaves
    /// things.
    pub stamp_lo: AtomicUsize,
    pub stamp_hi: AtomicUsize,
    /// Regions that have received a fresh allocation since the last whole-heap
    /// collection, as a bitmap over [`Self::Z_YOUNG_GRAIN_BYTES`] grains.
    ///
    /// The repair for `gen_young_floor`'s documented hole -- see
    /// [`Self::note_young_page`]. One bit per 64 KiB of arena, so a 4 GiB heap
    /// costs 8 KiB.
    pub young_pages: Vec<AtomicU64>,
    /// Did THIS collection's safepoint retire leave a TLAB chunk behind?
    ///
    /// Armed by `retire_all_tlabs_at_safepoint`, consulted by `relocate_stw`,
    /// cleared at the start of every collection. A gate reads it, so it stays
    /// on the hot struct rather than in [`ZgcCounters`].
    pub tlab_retire_incomplete: AtomicBool,
    /// Did an allocation land off the grain grid, making the young set
    /// incomplete? Forces every cycle whole-heap while set.
    pub young_page_grid_overflow: AtomicBool,
    /// Is [`Self::conc_start_bytes`] recomputed from measured allocation rate
    /// and mark duration, rather than fixed at a percentage?
    /// `CRATONVM_ZGC_CONC_START=auto`.
    pub conc_start_adaptive: AtomicBool,
    /// `allocated` and the wall clock at the last collection -- the two ends of
    /// the allocation-rate sample. Written only by
    /// [`Self::refresh_adaptive_conc_start`].
    pub rate_sample_bytes: AtomicU64,
    pub rate_sample_nanos: AtomicU64,
    /// Bytes per nanosecond, scaled by 2^20 and exponentially smoothed. Integer
    /// throughout: this is computed at a safepoint and a float would be the
    /// only one in the collector.
    pub alloc_rate_scaled: AtomicU64,
    /// How long a concurrent mark phase takes, smoothed. The other half of the
    /// window calculation.
    pub conc_mark_nanos_ewma: AtomicU64,
    /// Counters and diagnostic side tables, off the hot struct.
    ///
    /// # Why a box
    ///
    /// `ZgcRealHeap` had ~124 fields, most of them counters read once at
    /// shutdown or under a `CRATONVM_DBG_*` flag. Rust reorders fields freely,
    /// so declaration order cannot keep the hot ones together -- the only way
    /// to stop `arena_base`, `registry`, `allocated`, `gc_threshold`,
    /// `mark_active`, `mark_bits` and `tlabs` being scattered through eighty
    /// atomics is to take the cold ones out of the struct entirely.
    ///
    /// A plain `Box` rather than `Option<Box<..>>`: these are read on the
    /// `--verbose:gc` path and by the engagement accessors every one of this
    /// file's features is judged by, so making them optional would put a
    /// branch in front of every increment and an `unwrap` in front of every
    /// read, to save one allocation per heap.
    ///
    /// It is a grouping, not a policy. Nothing here decides anything; a field
    /// that a gate reads belongs in the struct proper, and moving one in here
    /// to tidy it would put a pointer chase on that gate's path.
    pub counters: Box<ZgcCounters>,
    pub external_gpu: cratonvm_gc_core::gpu_critical::ExternalGpuPressureTracker,
}

// SAFETY: identical argument to `Heap`/`G1Collector` (heap.rs:143). The only
// non-Send/Sync state is the raw `*mut u8` arena pointers, which live behind
// `Mutex<Arena>`; all allocation and collection serialize through that mutex,
// and the moving... (there is no moving — this collector is non-moving) so
// shared `&ZgcRealHeap` use across threads is sound under the same protocol
// the other collectors document.
unsafe impl Send for ZgcRealHeap {}
unsafe impl Sync for ZgcRealHeap {}
