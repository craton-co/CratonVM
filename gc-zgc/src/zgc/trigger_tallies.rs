#[allow(unused_imports)]
use super::*;


impl ZgcRealHeap {
    /// `(stress, threshold, headroom, alloc_budget, hard_alloc_fail)` --
    /// why a cycle happened. The first four are `needs_gc`'s branches, charged
    /// once per collection rather than once per agreeing poll (see
    /// [`ZgcCounters::trigger_seen`]); the last is the arena refusing an
    /// allocation, which collects WITHOUT consulting `needs_gc` at all. See the
    /// counters' declaration. The warm-up and director tallies have their own
    /// readers ([`Self::trigger_warmup_count`], [`Self::trigger_director_count`]).
    pub fn trigger_tallies(&self) -> (usize, usize, usize, usize, usize) {
        (
            self.counters.trigger_stress.load(Ordering::Relaxed),
            self.counters.trigger_threshold.load(Ordering::Relaxed),
            self.counters.trigger_headroom.load(Ordering::Relaxed),
            self.counters.trigger_alloc_budget.load(Ordering::Relaxed),
            self.counters
                .trigger_hard_alloc_fail
                .load(Ordering::Relaxed),
        )
    }

    /// `(starved_intervals, starved_max_gap_bytes, bypass_fired, bypass_suppressed)`
    /// — **the whole flip decision for `CRATONVM_ZGC_HEADROOM_BYPASSES_REARM`,
    /// as four numbers a probe can read off one line.**
    ///
    /// How to read it, in the order the decision is made:
    ///
    /// | reading | verdict |
    /// |---|---|
    /// | `starved == 0` (either arm) | the shape this flag is about did not occur. The flag can change nothing here; an A/B on this workload measures noise and must not be quoted for or against the flip. |
    /// | `starved > 0`, flag OFF | the divergence is real on this workload and `starved_max_gap` says by how much. That is a *finding*, not yet a defect: the consequence the gap page claimed for it could not be reproduced (see that page's §0), so a non-zero reading here is the first evidence anyone has that the state occurs at all, and what it costs is the next question rather than a settled one. |
    /// | flag ON, `bypass_fired > 0` | the arm ENGAGED. Throughput and cycle-count comparisons against the OFF arm are now about the feature. |
    /// | flag ON, `bypass_fired == 0`, `bypass_suppressed > 0` | the narrowing term refused it: the previous cycle freed nothing, so collecting again was futile. The storm bound is working; the run says nothing about throughput. |
    /// | flag ON, both zero, `starved > 0` | **contradiction — do not proceed.** `headroom_low` went short below the floor and neither the bypass nor its suppressor was charged, which means the flag did not reach this heap (a `OnceLock`-latched reader, or a per-heap override). Fix the harness before reading any other number. |
    ///
    /// See [`ZgcCounters::trigger_headroom_starved`] for why
    /// [`Self::trigger_tallies`]'s `headroom` figure cannot substitute for any
    /// of these, and
    /// `docs/feature-designs/zgc-architecture.md` for
    /// the commands.
    pub fn headroom_flip_evidence(&self) -> (usize, usize, usize, usize) {
        (
            self.counters
                .trigger_headroom_starved
                .load(Ordering::Relaxed),
            self.counters
                .trigger_headroom_starved_gap
                .load(Ordering::Relaxed),
            self.counters
                .trigger_headroom_bypass
                .load(Ordering::Relaxed),
            self.counters
                .trigger_headroom_bypass_suppressed
                .load(Ordering::Relaxed),
        )
    }

    /// Old pages the young cycle's remembered-set scan skipped for want of a
    /// base address. **Must read `0`** — see
    /// [`ZgcCounters::remset_pages_without_base`].
    pub fn remset_pages_without_base(&self) -> usize {
        self.counters
            .remset_pages_without_base
            .load(Ordering::Relaxed)
    }

    /// Number of collections performed so far.
    pub fn gc_count(&self) -> usize {
        self.gc_count.load(Ordering::Relaxed)
    }

    /// Native-allocation pressure signal — see the
    /// [`native_alloc_pressure`](Self::native_alloc_pressure) field doc.
    /// Consumed at the `safe_native_call` boundary via
    /// `VmHeap::young_spill_pressure`. Mirrors `G1Collector::native_alloc_pressure`
    /// (`g1.rs:1779-1781`), including its `Relaxed` ordering: advisory
    /// diagnostic feeding a policy decision, not a correctness handshake.
    #[inline]
    pub fn native_alloc_pressure(&self) -> bool {
        self.native_alloc_pressure.load(Ordering::Relaxed)
    }

    /// Clear the native-allocation pressure latch. The consumer clears
    /// unconditionally after acting (including when its own gates said no), so
    /// this must be idempotent and cheap. `Relaxed` for the reason on the
    /// field: nothing is published through the bit. Mirrors `g1.rs:1785-1787`.
    #[inline]
    pub fn clear_native_alloc_pressure(&self) {
        self.native_alloc_pressure.store(false, Ordering::Relaxed);
    }

    /// Latch the native-allocation pressure signal from OUTSIDE the collector —
    /// the `VmHeap::note_young_spill_pressure` funnel used by allocation
    /// wrappers that spilled and cannot collect themselves. Mirrors
    /// `g1.rs:1793-1795`.
    ///
    /// Note this externally-noted edge deliberately bypasses the `gc_threshold`
    /// / `gc_rearm` predicate that [`Self::alloc_raw`] applies: the caller is
    /// asserting pressure the heap's own counters cannot see. It still cannot
    /// storm, because the *consumer* re-checks `needs_gc` and its own overhead
    /// gate before running a cycle, and each call latches once for one clear.
    #[inline]
    pub fn note_native_alloc_pressure(&self) {
        self.native_alloc_pressure.store(true, Ordering::Relaxed);
    }

    /// Account for retained off-heap GPU VRAM and arm `native_alloc_pressure`
    /// when external pacing policy thresholds are crossed.
    #[inline]
    pub fn note_external_gpu_retained(&self, delta_bytes: i64, wrapper_delta: i64) -> bool {
        self.external_gpu
            .apply_signed_delta(delta_bytes, wrapper_delta, &self.native_alloc_pressure)
    }

    /// Whether an allocation has been **refused** since the last collection —
    /// see the [`hard_alloc_failure`](Self::hard_alloc_failure) field doc.
    ///
    /// Consumed at the `safe_native_call` boundary, which is the one point on
    /// the native dispatch path where a collection is safe (every Java
    /// argument is pinned and remapped around it). It is deliberately NOT
    /// consumed inside the allocation wrappers themselves: those "must stay
    /// GC-free mid-callback, since their callers hold unrooted local
    /// `ObjectRef`s" (`vm_exec.rs`, the native-alloc young-pressure relief
    /// comment). Collecting there would be a use-after-free, not a fix.
    #[inline]
    pub fn hard_alloc_failure(&self) -> bool {
        self.hard_alloc_failure.load(Ordering::Relaxed)
    }

    /// Clear the hard-failure latch. Idempotent; the consumer clears after
    /// acting whether or not its overhead gate let the cycle run.
    #[inline]
    pub fn clear_hard_alloc_failure(&self) {
        self.hard_alloc_failure.store(false, Ordering::Relaxed);
    }

    /// Is the extra-root filter armed **across the concurrent mark phase**?
    ///
    /// *Stage 3 of `handoff-r-wire-the-concurrent-root-filter.md`.* The filter
    /// has always been armed for the stop-the-world mark; this is the switch
    /// that also arms it for the phase concurrent marking exists to create,
    /// together with the `late_owners()` repair that makes doing so sound.
    ///
    /// **Default OFF and the flag is not declared yet.** See
    /// `gap-p-conc-root-filter-flag.md`. Read once per concurrent cycle (twice
    /// per collection at worst), so the `OnceLock` in the reader is about
    /// consistency, not cost: a run in which the arm point and the harvest
    /// point disagreed would publish proofs it then refused to repair.
    #[inline]
    pub fn conc_root_filter_armed(&self) -> bool {
        self.conc_root_filter.load(Ordering::Relaxed)
    }

    /// Per-heap override for [`zgc_conc_root_filter`], for the same reason
    /// every other A/B switch here has one: the flag reader latches in a
    /// `OnceLock`, so the first test in a binary to touch it would decide the
    /// answer for every later test.
    ///
    /// **Both halves move together or neither does.** Arming the filter
    /// without the mark-end repair is not slower, it is *unsound* — wave 3
    /// declined to do exactly that — so there is deliberately no way to set
    /// one and not the other: `conc_root_filter_armed` gates the `rebuild()`
    /// in `start_concurrent_mark` and the `late_owners()` harvest in
    /// `collect_garbage`, and it is the same load.
    pub fn set_conc_root_filter(&self, on: bool) {
        self.conc_root_filter.store(on, Ordering::Relaxed);
    }

    /// Per-heap override for [`zgc_headroom_bypasses_rearm`].
    ///
    /// A setter and not only a flag for the reason every other A/B switch here
    /// has one: the flag reader caches in a `OnceLock`, so the first test in a
    /// binary to touch it would decide the answer for every later test. See
    /// [`Self::headroom_bypasses_rearm`].
    pub fn set_headroom_bypasses_rearm(&self, on: bool) {
        self.headroom_bypasses_rearm.store(on, Ordering::Relaxed);
    }

    /// `(headroom_low, bypass armed, the last cycle reclaimed nothing)` — the
    /// headroom clause's own state, so a test or a triage session can ask why
    /// it did or did not fire without re-deriving it from `gc_rearm`.
    pub fn headroom_trigger_state(&self) -> (bool, bool, bool) {
        (
            self.headroom_low.load(Ordering::Relaxed),
            self.headroom_bypasses_rearm.load(Ordering::Relaxed),
            self.last_cycle_reclaimed_nothing.load(Ordering::Relaxed),
        )
    }

    /// `(parallel_mark_cycles, compaction_cycles, objects_relocated)` — did
    /// the 2026-08-13 default-on features actually engage this run?
    ///
    /// Reported at shutdown. A zero here is not a failure — the relocation-set
    /// selector legitimately declines a heap with no garbage worth moving, and
    /// a small-core box legitimately marks serially — but it IS the difference
    /// between "the feature ran and behaved" and "the feature never ran and
    /// the run proves nothing about it".
    pub fn feature_engagement(&self) -> (usize, usize, usize) {
        (
            self.counters.parallel_mark_cycles.load(Ordering::Relaxed),
            self.counters.compaction_cycles.load(Ordering::Relaxed),
            self.counters.objects_relocated.load(Ordering::Relaxed),
        )
    }

    /// `(spans, bytes)` the slide handed back to the free list rather than
    /// losing — see [`ZgcRealHeap::compact_high_region`]'s neighbour,
    /// `Arena::compact_low_to`.
    ///
    /// (2026-09-20: this doc and [`Self::high_compaction_engagement`]'s were
    /// one run-on block attached to this function, so the accessor below was
    /// undocumented and this one described the wrong tuple. Split, unchanged
    /// otherwise.)
    pub fn vacated_publication(&self) -> (usize, usize) {
        (
            self.counters
                .vacated_spans_published
                .load(Ordering::Relaxed),
            self.counters
                .vacated_bytes_published
                .load(Ordering::Relaxed),
        )
    }

    /// `(cycles, declined, objects, bytes)` — did the LARGE-OBJECT end's
    /// compactor engage this run?
    ///
    /// Reported at shutdown beside `compaction_cycles`, and the pairing is the
    /// point: those numbers describe the low end only, and a heap can compact
    /// its low end on every cycle while the request that is actually failing is
    /// served from the other one. See [`ZgcRealHeap::compact_high_region`].
    pub fn high_compaction_engagement(&self) -> (usize, usize, usize, usize) {
        (
            self.counters.high_compaction_cycles.load(Ordering::Relaxed),
            self.counters
                .high_compaction_declined
                .load(Ordering::Relaxed),
            self.counters.high_objects_relocated.load(Ordering::Relaxed),
            self.counters.high_bytes_copied.load(Ordering::Relaxed),
        )
    }

    /// `(after_failure_cycles, tight_cycles, widened_live_bytes)` of
    /// `CRATONVM_ZGC_RELOCATE_PRESSURE_SELECT` (zr z2/cp): low slides whose
    /// selector ran with the widened occupancy cutoff, by tier, and the live
    /// bytes those selections carried. Printed on `[GC] zgc-capacity:`.
    pub fn relocation_pressure_select_stats(&self) -> (usize, usize, usize) {
        (
            self.counters.pressure_select[0].load(Ordering::Relaxed),
            self.counters.pressure_select[1].load(Ordering::Relaxed),
            self.counters.pressure_select[2].load(Ordering::Relaxed),
        )
    }

    /// `(slides_filtered, pages_kept, slides_emptied, failure_bypasses,
    /// bytes_moved_total)` of `CRATONVM_ZGC_SELECTIVE_RELOCATE` (zr z9/sl).
    /// The last field counts on every arm (it is the slide's copy volume).
    /// Printed on `[GC] zgc-selective:`.
    pub fn selective_relocate_stats(&self) -> (usize, usize, usize, usize, usize) {
        let s = &self.counters.selective_relocate;
        (
            s[0].load(Ordering::Relaxed),
            s[1].load(Ordering::Relaxed),
            s[2].load(Ordering::Relaxed),
            s[3].load(Ordering::Relaxed),
            self.counters.slide_moved_bytes[1].load(Ordering::Relaxed),
        )
    }

    /// zs s1/sl: `(cycles_admitted, cycles_declined, cycles_tight,
    /// fused_census_slides)` of the cycle gate
    /// (`CRATONVM_ZGC_SELECTIVE_RELOCATE=2`) and the fused census
    /// (`CRATONVM_ZGC_SELECTIVE_FUSED_FRAG`). The first three are zero unless
    /// the gate ran; the fourth counts on both selective modes. Printed on
    /// `[GC] zgc-selective:` as `sel_cycle_admit= sel_cycle_decline=
    /// sel_cycle_tight= sel_fused=`.
    pub fn selective_cycle_stats(&self) -> (usize, usize, usize, usize) {
        let s = &self.counters.selective_relocate;
        (
            s[4].load(Ordering::Relaxed),
            s[5].load(Ordering::Relaxed),
            s[6].load(Ordering::Relaxed),
            s[7].load(Ordering::Relaxed),
        )
    }

    /// Cycles that declined to relocate because a compiled frame was live.
    ///
    /// Printed in the shutdown summary beside `compaction_cycles`, because the
    /// two only mean something together: `compaction_cycles=0` alone reads as
    /// "compaction is broken", and `compaction_cycles=0
    /// relocation_skipped_jit=97` reads as "this workload is never JIT-quiet",
    /// which is a tuning problem and not a defect. On this collector
    /// compaction is also defragmentation, so a run that never compacts is a
    /// run that never defragments.
    pub fn relocation_skipped_jit(&self) -> usize {
        self.counters.relocation_skipped_jit.load(Ordering::Relaxed)
    }

    /// Cycles that compacted with a compiled frame live, on the per-cycle
    /// coverage proof. See the field doc for why a zero here is informative.
    pub fn relocation_on_proven_jit(&self) -> usize {
        self.counters
            .relocation_on_proven_jit
            .load(Ordering::Relaxed)
    }

    /// Refills served by a recycled block, split into the preferred rung and
    /// the starved rung. See [`Self::tlab_refill_recycled`].
    pub fn tlab_recycle_engagement(&self) -> (usize, usize, usize) {
        (
            self.counters.tlab_refill_recycled.load(Ordering::Relaxed),
            self.counters.tlab_refill_starved.load(Ordering::Relaxed),
            self.counters
                .tlab_refill_starved_bytes
                .load(Ordering::Relaxed),
        )
    }

    /// TLAB-chunk bytes whose zeroing was skipped because they had never been
    /// written (round 9 wave 10, `zgc10`). Zero with
    /// `CRATONVM_ZGC_PRISTINE_CHUNKS=0`.
    pub fn tlab_pristine_bytes_skipped(&self) -> usize {
        self.counters
            .tlab_pristine_bytes_skipped
            .load(Ordering::Relaxed)
    }

    /// TLAB-chunk bytes whose zeroing was skipped because the last slide had
    /// already zeroed them (r16 zgcarray, ZA1). Zero with
    /// `CRATONVM_ZGC_PRISTINE_CHUNKS=0`, which keeps the arena from opening the
    /// window at all.
    pub fn tlab_slide_zero_bytes_skipped(&self) -> usize {
        self.counters
            .tlab_slide_zero_bytes_skipped
            .load(Ordering::Relaxed)
    }

    /// zt t2/al: `(piece_bytes, chunks)` of the default on descending chunk
    /// zeroing (`CRATONVM_ZGC_CHUNK_ZERO_DESCENDING`): the piece size it runs
    /// with (`0` = off) and the chunks `carve_tlab_chunk` zeroed that way.
    pub fn chunk_zero_descending_stats(&self) -> (usize, usize) {
        (
            self.counters.chunk_zero_descending.load(Ordering::Relaxed),
            self.counters
                .chunk_zero_descending_chunks
                .load(Ordering::Relaxed),
        )
    }

    /// Per-term census of the relocation refusal — see
    /// [`relocation_skip_reason`].
    pub fn relocation_skip_reason_counts(&self) -> [usize; relocation_skip_reason::COUNT] {
        std::array::from_fn(|i| self.counters.relocation_skip_reasons[i].load(Ordering::Relaxed))
    }

    /// For refusals attributed to [`relocation_skip_reason::COVERAGE_INCOMPLETE`],
    /// the `gc_quiescence::incomplete_reason` the proof recorded.
    pub fn relocation_coverage_reason_counts(
        &self,
    ) -> [usize; cratonvm_gc_core::gc_quiescence::incomplete_reason::COUNT] {
        std::array::from_fn(|i| {
            self.counters.relocation_coverage_reasons[i].load(Ordering::Relaxed)
        })
    }

    /// Per-term census of the relocation refusal counted **unconditionally** —
    /// see [`ZgcCounters::relocation_refusal_terms`] for why this is a second
    /// array rather than a move of the existing one.
    ///
    /// Read it beside [`Self::relocation_skip_reason_counts`]. The difference
    /// between the two, term by term, is the number of cycles on which a named
    /// obligation failed and the slide ran anyway.
    pub fn relocation_refusal_term_counts(&self) -> [usize; relocation_skip_reason::COUNT] {
        std::array::from_fn(|i| self.counters.relocation_refusal_terms[i].load(Ordering::Relaxed))
    }

    /// `(pages_deferred, cycles_truncated)` — whether the evacuation budget
    /// (`forwarding::ZRelocationPolicy::max_evacuation_bytes`, set by
    /// `CRATONVM_ZGC_RELOCATE_BUDGET_MB` and otherwise
    /// [`forwarding::ZFWD_DEFAULT_MAX_EVACUATION_BYTES`]) is binding on this
    /// run.
    ///
    /// **`cycles_truncated == 0` means the constant is not binding**, and no
    /// A/B of its value can measure anything on this workload. That is the
    /// reading to take before touching the knob.
    pub fn relocation_budget_engagement(&self) -> (usize, usize) {
        (
            self.counters
                .relocation_pages_deferred
                .load(Ordering::Relaxed),
            self.counters
                .relocation_budget_truncated_cycles
                .load(Ordering::Relaxed),
        )
    }

    /// Lifetime count of TLAB cells a retire could not lock.
    pub fn tlab_retire_skipped(&self) -> usize {
        self.counters
            .tlab_retire_skipped_total
            .load(Ordering::Relaxed)
    }

    /// Cells the SAFEPOINT retire could not lock -- the dangerous half of
    /// [`Self::tlab_retire_skipped`].
    ///
    /// The number `relocate_stw`'s old soundness argument needed and did not
    /// have. `tlab_retire_skipped` counts four callers, three of which hold no
    /// safepoint and for none of which a skip means anything; a run reporting
    /// 42 there says nothing about whether any of them was this one. Zero here
    /// is the ruling-out that paragraph claimed.
    pub fn tlab_retire_skipped_at_safepoint(&self) -> usize {
        self.counters
            .tlab_retire_skipped_at_safepoint
            .load(Ordering::Relaxed)
    }

    /// Mark-driver fixed-point waits that expired instead of being woken. See
    /// [`Self::mark_park_timeouts`]; nonzero on a stop-the-world run means a
    /// notification is missing.
    pub fn mark_park_timeouts(&self) -> usize {
        self.counters.mark_park_timeouts.load(Ordering::Relaxed)
    }

    /// zr zs s1/mc: thread CPU nanoseconds the MARK has cost this process,
    /// cumulative, as `(stw_pool, serial, concurrent_pool)`:
    ///
    /// * `stw_pool` -- the stop-the-world mark pool's workers (the default
    ///   `CRATONVM_ZGC_PARMARK=auto` path). The collecting thread only waits
    ///   in the inline driver, so its share is not counted.
    /// * `serial` -- the single-threaded marker on the collecting thread (the
    ///   `PARMARK=0` / one-worker / fallback / finalizer / soft-probe calls).
    /// * `concurrent_pool` -- the concurrent pool's workers, concurrent phase
    ///   and mark-end drain together.
    ///
    /// All zero under `CRATONVM_ZGC_MARK_CPU_ACCOUNTING=0` or with no thread
    /// clock. What a concurrent arm costs ABOVE the stop-the-world arm on the
    /// same work is `concurrent_pool - stw_pool` here plus what the mutators
    /// pay for the SATB barrier and the floating garbage; the per-cycle halves
    /// are `cpu_us=` on `[GC] zgc-mark-parallel:` / `[GC] zgc-mark-serial:` and
    /// `worker_cpu_us=` on `[GC] zgc-markend:`.
    pub fn mark_cpu_totals(&self) -> (u64, u64, u64) {
        (
            self.counters.mark_cpu_stw_pool_ns.load(Ordering::Relaxed),
            self.counters.mark_cpu_serial_ns.load(Ordering::Relaxed),
            self.counters.mark_cpu_conc_pool_ns.load(Ordering::Relaxed),
        )
    }

    /// zs s2/mc2: the collecting thread's own thread CPU across every
    /// stop-the-world PARALLEL mark (`driver_cpu_us=` per cycle on
    /// `[GC] zgc-mark-parallel:`), in nanoseconds. It is CPU of a Java thread,
    /// so it sits inside `java_cpu_ms` on a probe's ThreadMXBean sum; add it to
    /// the pool's `stw_pool` figure for the whole mark. Zero under
    /// `CRATONVM_ZGC_MARK_CPU_ACCOUNTING=0`.
    pub fn mark_cpu_stw_driver(&self) -> u64 {
        self.counters.mark_cpu_stw_driver_ns.load(Ordering::Relaxed)
    }

    /// zr zs s1/mc: the part of [`Self::mark_cpu_totals`]' `concurrent_pool`
    /// spent on ABANDONED cycles, in thread CPU nanoseconds.
    pub fn mark_cpu_conc_abandoned(&self) -> u64 {
        self.counters
            .mark_cpu_conc_abandoned_ns
            .load(Ordering::Relaxed)
    }

    /// zt t1/mk (`CRATONVM_ZGC_FULL_REQUEST_FRESH_MARK`): concurrent cycles a
    /// full request (`System.gc()` and the other `force_full_collection`
    /// doors) abandoned so that its collection marks from scratch,
    /// cumulative. Zero without concurrent marking or under the `=0` kill.
    pub fn full_request_conc_abandons(&self) -> usize {
        self.counters
            .full_request_conc_abandons
            .load(Ordering::Relaxed)
    }

    /// zt t24/hunt7 (`CRATONVM_ZGC_OOM_LADDER_FRESH_MARK`): did the last
    /// collection close a concurrent cycle with that cycle's mark set (its
    /// mark started before the collection was asked for)? `false` before any
    /// collection, after a stop-the-world mark (an abandoned or refused cycle
    /// included), and always on the default stop-the-world ZGC.
    pub fn last_collection_marked_concurrently(&self) -> bool {
        self.counters
            .last_collection_marked_concurrently
            .load(Ordering::Relaxed)
    }

    /// zs s3/ac: every accounted share of this heap's collection CPU, in
    /// thread CPU nanoseconds, cumulative. The shutdown `[GC] zgc-cpu:` line
    /// prints it. All zero under `CRATONVM_ZGC_MARK_CPU_ACCOUNTING=0` or with
    /// no thread clock.
    pub fn collection_cpu_totals(&self) -> ZCollectionCpuTotals {
        let c = &self.counters;
        ZCollectionCpuTotals {
            mark_stw_pool_ns: c.mark_cpu_stw_pool_ns.load(Ordering::Relaxed),
            mark_stw_driver_ns: c.mark_cpu_stw_driver_ns.load(Ordering::Relaxed),
            mark_serial_ns: c.mark_cpu_serial_ns.load(Ordering::Relaxed),
            mark_conc_pool_ns: c.mark_cpu_conc_pool_ns.load(Ordering::Relaxed),
            sweep_pool_ns: c.sweep_pool_cpu_ns.load(Ordering::Relaxed),
            relocate_pool_ns: c.relocate_pool_cpu_ns.load(Ordering::Relaxed),
            uncommit_ns: c.uncommit_cpu_ns.load(Ordering::Relaxed),
            vm_pause_ns: c.vm_pause_cpu_ns.load(Ordering::Relaxed),
            pause_thread_ns: c.pause_thread_cpu_ns.load(Ordering::Relaxed),
            pause_cycles: c.pause_cpu_cycles.load(Ordering::Relaxed),
            satb_flush_ns: c.satb_flush_cpu_ns.load(Ordering::Relaxed),
        }
    }

    /// zt t6/ob4 (`zgc-zs3ac-collection-cpu-residual-shares-SUPERSEDED-20261019.md`
    /// item 2): open the VM's span of one collection pause, on the
    /// collecting thread, before its safepoint request: `(thread CPU now,
    /// this heap's pause_thread share now)`. `None` (nothing is booked) under
    /// `CRATONVM_ZGC_MARK_CPU_ACCOUNTING=0`, under
    /// `CRATONVM_ZGC_VM_PAUSE_CPU=0`, or with no thread clock. One flag read
    /// and one clock read per collection.
    pub fn vm_pause_cpu_start(&self) -> Option<(u64, u64)> {
        if !(self.collection_cpu_accounting_on()) {
            return None;
        }
        let cpu = mark::thread_cpu_nanos()?;
        Some((cpu, self.counters.pause_thread_cpu_ns.load(Ordering::Relaxed)))
    }

    /// zt t6/ob4: close the span [`Self::vm_pause_cpu_start`] opened, after
    /// the VM's epilogue, on the same thread: the thread's CPU across the
    /// span less what `collect_garbage` booked to `pause_thread_ns` meanwhile
    /// ([`zgc_vm_pause_share`]) goes to the `vm_pause_ns` share
    /// (`vm_pause_ms=` on `[GC] zgc-cpu:`, inside `collection_ms`).
    pub fn book_vm_pause_cpu(&self, start: (u64, u64)) {
        let Some(cpu) = mark::thread_cpu_nanos() else {
            return;
        };
        let heap = self.counters.pause_thread_cpu_ns.load(Ordering::Relaxed);
        let ns = zgc_vm_pause_share(start, (cpu, heap));
        self.counters
            .vm_pause_cpu_ns
            .fetch_add(ns, Ordering::Relaxed);
    }

    /// zs s3/ac: the end of `collect_garbage`. Book the collecting thread's
    /// CPU for the whole collection and, under `--verbose:gc` (`before` is
    /// `Some`), print this cycle's shares as deltas of the cumulative ones:
    ///
    /// `[GC] zgc-pause-cpu: cycle= wall_us= pause_cpu_us= mark_pool_cpu_us=
    /// mark_driver_cpu_us= mark_serial_cpu_us= conc_pool_cpu_us=
    /// sweep_pool_cpu_us= relocate_pool_cpu_us= collection_cpu_us=`
    ///
    /// `mark_driver` and `mark_serial` are INSIDE `pause_cpu` (same thread);
    /// `collection_cpu_us` is the disjoint sum
    /// ([`ZCollectionCpuTotals::collection_ns`]). `conc_pool` is the
    /// concurrent pool's CPU booked by this collection's mark end, which
    /// includes the concurrent phase before the pause.
    pub fn book_pause_cpu(
        &self,
        cycle: usize,
        span: &mark::ZThreadCpuSpan,
        before: Option<ZCollectionCpuTotals>,
        wall_started: Option<std::time::Instant>,
    ) {
        let ns = span.elapsed_ns();
        if self.collection_cpu_accounting_on() {
            self.counters
                .pause_thread_cpu_ns
                .fetch_add(ns, Ordering::Relaxed);
            self.counters
                .pause_cpu_cycles
                .fetch_add(1, Ordering::Relaxed);
        }
        let Some(b) = before else {
            return;
        };
        if !self.gc_log_enabled.load(Ordering::Relaxed) {
            return;
        }
        let a = self.collection_cpu_totals();
        let d = ZCollectionCpuTotals {
            mark_stw_pool_ns: a.mark_stw_pool_ns.saturating_sub(b.mark_stw_pool_ns),
            mark_stw_driver_ns: a.mark_stw_driver_ns.saturating_sub(b.mark_stw_driver_ns),
            mark_serial_ns: a.mark_serial_ns.saturating_sub(b.mark_serial_ns),
            mark_conc_pool_ns: a.mark_conc_pool_ns.saturating_sub(b.mark_conc_pool_ns),
            sweep_pool_ns: a.sweep_pool_ns.saturating_sub(b.sweep_pool_ns),
            relocate_pool_ns: a.relocate_pool_ns.saturating_sub(b.relocate_pool_ns),
            // zt t3/rl: the uncommitter runs BETWEEN collections, so its CPU
            // is not this cycle's; it is in the cumulative shutdown line only.
            uncommit_ns: 0,
            // zt t6/ob4: the VM books its side after this line is printed.
            vm_pause_ns: 0,
            pause_thread_ns: a.pause_thread_ns.saturating_sub(b.pause_thread_ns),
            pause_cycles: 1,
            // Mutator CPU between pauses, never in a pause's share.
            satb_flush_ns: 0,
        };
        eprintln!(
            "[GC] zgc-pause-cpu: cycle={cycle} wall_us={} pause_cpu_us={} mark_pool_cpu_us={} \
             mark_driver_cpu_us={} mark_serial_cpu_us={} conc_pool_cpu_us={} \
             sweep_pool_cpu_us={} relocate_pool_cpu_us={} collection_cpu_us={}",
            wall_started.map_or(0, |s| s.elapsed().as_micros()),
            d.pause_thread_ns / 1_000,
            d.mark_stw_pool_ns / 1_000,
            d.mark_stw_driver_ns / 1_000,
            d.mark_serial_ns / 1_000,
            d.mark_conc_pool_ns / 1_000,
            d.sweep_pool_ns / 1_000,
            d.relocate_pool_ns / 1_000,
            d.collection_ns() / 1_000,
        );
    }

    /// zs s3/ac: whether the collection's CPU spans read a clock -- the one
    /// `CRATONVM_ZGC_MARK_CPU_ACCOUNTING` switch (default ON, `=0` kill) that
    /// gates the mark's accounting gates every other share too. One relaxed
    /// load per pass.
    #[inline]
    pub fn collection_cpu_accounting_on(&self) -> bool {
        self.counters
            .mark_loop
            .cpu_accounting
            .load(Ordering::Relaxed)
    }

    /// zs s3/ac: `(addresses the SATB direct hand-off gave the concurrent
    /// pool, switch on)`. See `CRATONVM_ZGC_SATB_DIRECT_HANDOFF`.
    pub fn satb_direct_handoff_report(&self) -> (usize, bool) {
        (
            self.counters.satb_direct_handed.load(Ordering::Relaxed),
            self.counters.satb_direct_handoff.load(Ordering::Relaxed),
        )
    }

    /// `(driver_passes, fallbacks)` — did `zgc_concurrent`'s controller drive
    /// the marking, and did it ever refuse to certify the result?
    ///
    /// `driver_passes == 0` with a non-zero `parallel_mark_cycles` would mean
    /// the pool marked without the driver, which is the state that adoption
    /// exists to leave behind. (2026-09-20: this doc had come adrift onto
    /// [`Self::relocation_skipped_jit`], where it described the wrong tuple.)
    pub fn driver_engagement(&self) -> (usize, usize) {
        (
            self.counters.driver_passes.load(Ordering::Relaxed),
            self.counters
                .parallel_mark_fallbacks
                .load(Ordering::Relaxed),
        )
    }

    /// zr z4/rv4: bases re-scanned by failed parallel marks' serial fallbacks.
    pub fn parallel_mark_fallback_rescans(&self) -> usize {
        self.counters
            .parallel_mark_fallback_rescans
            .load(Ordering::Relaxed)
    }

    /// The fragmentation ratchet's current reading — Phase 2.2 of the ZGC
    /// maturity plan, which asked to turn "ZGC fragments" from an anecdote per
    /// suite run into a tracked number.
    ///
    /// Reported at shutdown by `VmHeap::print_gc_summary` on the `[GC]
    /// zgc-real:` line, so a suite runner can extract it per class with a grep
    /// and a CI job can ratchet on it. See [`ZFragGauge`] for what the two
    /// numbers mean and why one of them alone means nothing.
    pub fn frag_gauge(&self) -> ZFragGauge {
        let worst = self.counters.frag_worst_permille.load(Ordering::Relaxed);
        ZFragGauge {
            samples: self.counters.frag_samples.load(Ordering::Relaxed),
            worst_permille: (worst != usize::MAX).then_some(worst),
            free_permille: self
                .counters
                .frag_worst_free_permille
                .load(Ordering::Relaxed),
            worst_cycle: self.counters.frag_worst_cycle.load(Ordering::Relaxed),
        }
    }

    /// Take one post-sweep fragmentation reading.
    ///
    /// Called at the end of every collection with the arena guard still held.
    /// Cost is one `largest_free_block()` (O(1) — a `BTreeMap` last key and a
    /// field read), two divisions and, on the rare improving edge, three
    /// relaxed stores.
    ///
    /// The sampling condition is the instrument: a collection that leaves less
    /// than [`ZGC_FRAG_GAUGE_MIN_FREE_PERMILLE`] of the heap free is not
    /// evidence about fragmentation at all, and counting it would turn this
    /// gauge into a second, worse occupancy trigger.
    pub fn sample_frag_gauge(&self, arena: &Arena, cycle: usize) {
        let capacity = arena.capacity();
        if capacity == 0 {
            return;
        }
        // Free = what the arena could still hand out at all: the un-bumped
        // middle plus both free lists. `remaining()` is exactly that sum.
        let free_permille = arena.remaining().saturating_mul(1000) / capacity;
        if free_permille < ZGC_FRAG_GAUGE_MIN_FREE_PERMILLE {
            return;
        }
        // ...against the biggest single thing it could hand out.
        //
        // NOT `largest_free_block()` alone. That is the largest FREE-LIST
        // block, and on a heap that has not yet bumped its way to capacity the
        // biggest servable run is the un-bumped middle between the two
        // cursors, which is on no free list at all. Scoring the free list by
        // itself reads a pristine 1 MiB arena as **0 permille fragmented** —
        // caught by `a_collection_with_room_to_spare_takes_a_reading`, which
        // is what that test is for. `remaining()` is middle + both free lists,
        // so subtracting the lists leaves the middle exactly.
        let middle = arena.remaining().saturating_sub(arena.free_list_bytes());
        let servable = middle.max(arena.largest_free_block());
        let largest_permille = servable.saturating_mul(1000) / capacity;
        self.counters.frag_samples.fetch_add(1, Ordering::Relaxed);
        if largest_permille < self.counters.frag_worst_permille.load(Ordering::Relaxed) {
            self.counters
                .frag_worst_permille
                .store(largest_permille, Ordering::Relaxed);
            self.counters
                .frag_worst_free_permille
                .store(free_permille, Ordering::Relaxed);
            self.counters
                .frag_worst_cycle
                .store(cycle, Ordering::Relaxed);
        }
        if largest_permille < ZGC_FRAG_FLOOR_PERMILLE
            && !self
                .counters
                .frag_floor_warned
                .swap(true, Ordering::Relaxed)
        {
            tracing::warn!(
                target: "cratonvm::gc::guard",
                cycle,
                largest_free_permille = largest_permille,
                free_permille,
                capacity,
                largest_servable_block = servable,
                "zgc frag gauge: the arena is broken up — {}.{}% of the heap is free but the largest single block is only {}.{}% of capacity, so a request above that size cannot be served however much is free. This collector does not compact, so the shape does not recover on its own.",
                free_permille / 10,
                free_permille % 10,
                largest_permille / 10,
                largest_permille % 10,
            );
        }
    }

    /// The SATB pre-write barrier, for the concurrent marker — Phase 3.
    ///
    /// # What calls this, and why that is the whole point
    ///
    /// Nothing new. `VmHeap::satb_barrier` is already called before **every**
    /// reference store in this VM — the interpreter's `putfield`/`aastore`,
    /// the JIT's `aastore` and `putfield` helpers, `deopt_materialize`, and
    /// `vm_init` — because G1 needs it. Its ZGC arm was `{}`. So the mutator
    /// ingress that `zgc_concurrent.rs` describes as the missing piece
    /// ("nothing calls `ZMarkHandle::mark_live_offset` from a `getfield`, so
    /// the mutator ingress is empty in practice") did not need a new call
    /// site threaded through three code generators. It needed this arm to stop
    /// being empty.
    ///
    /// # Cost while nothing is marking
    ///
    /// One relaxed load of a never-written cache line, then return. That is
    /// the reason `mark_active` is a separate flag rather than, say, an
    /// `Option` probe or a lock: this sits on the store path of every Java
    /// program the VM runs, including every program that will never see a
    /// concurrent cycle.
    ///
    /// # Why SATB and not the load barrier ZGC actually uses
    ///
    /// Real ZGC marks on **read**, in the load barrier, which is why
    /// `zgc_concurrent.rs`'s mark-end is a *decision point* rather than a
    /// conclusion: with a read barrier every mutator stays a producer until it
    /// is stopped, hence the restart loop. A pre-write barrier is the other
    /// discipline — snapshot-at-the-beginning — and it is what this VM already
    /// has plumbed everywhere, at zero additional emission cost.
    ///
    /// The two are **not interchangeable**, and adopting this one has a
    /// consequence that must be written down before anybody relies on it: SATB
    /// keeps everything live at the snapshot, so it is *conservative* (an
    /// object that dies during the cycle is collected in the next one), while
    /// ZGC's load barrier is precise. Conservative is a throughput cost, not a
    /// correctness one, which is the right side to be wrong on for a first
    /// adoption. The restart loop stays correct under it — it simply reaches
    /// `Complete` sooner, because a snapshot's producer set really is bounded.
    ///
    /// # Where it runs, and what is still NOT done
    ///
    /// This records the old value; it does not run a cycle. Since zr z2/cc
    /// each thread records into its own SATB buffer
    /// (`ZSatbThreadBuffers`, `CRATONVM_ZGC_SATB_THREAD_BUFFERS`, default
    /// on), and a full buffer goes to the running concurrent pool in one batch
    /// (zs s3/ac: straight to `push_mutator_batch`,
    /// `CRATONVM_ZGC_SATB_DIRECT_HANDOFF`) or, with no pool, into the heap
    /// ingress for the mark-end replay. The per-push bucket mutex below is the
    /// unbuffered arm (thread teardown, or the buffers switched off).
    /// [`mark::ZMarkMutatorBuffer`] is the load-barrier seam, not this path.
    ///
    /// **Reachable in production under `CRATONVM_ZGC_CONC_START`** (zr z1/br,
    /// 2026-10-12: this paragraph used to say `mark_active` was set by tests
    /// only, which stopped being true when [`Self::start_concurrent_mark`] got
    /// its mark-start pause, `gc_and_alloc::zgc_concurrent_mark_cycle`). With
    /// concurrent marking off (the default) `mark_active` stays `false` and
    /// this is the one relaxed load above. The compiled tiers do not call it
    /// while their own pre gate (`JitRefStoreGates::pre_active`, raised before
    /// `mark_active`, lowered after) reads idle; that gate is test-then-store,
    /// which the mark start's take-over can straddle -- see
    /// `gc_and_alloc::zgc_satb_window_refuses`.
    #[inline]
    pub fn satb_pre_barrier(&self, old_addr: usize) {
        // The entire cost of this barrier on a non-concurrent run.
        if !self.mark_active.load(Ordering::Relaxed) {
            return;
        }
        self.satb_pre_barrier_slow(old_addr);
    }

    /// Bulk SATB pre-barrier for `len` 8-byte reference slots starting at
    /// `base + off * 8`, called before those slots are overwritten.
    ///
    /// # Safety
    /// `base + (off + i) * 8` must be valid, 8-aligned, live reference-array
    /// slots for `0 <= i < len` with `!self.load_barrier_armed()` (uncolored
    /// pointers).
    #[inline]
    pub unsafe fn satb_pre_barrier_slot_range(&self, base: *const u8, off: usize, len: usize) {
        if !self.mark_active.load(Ordering::Relaxed) {
            return;
        }
        for i in 0..len {
            // SAFETY: caller guarantees valid, 8-aligned, uncolored live slots.
            let word = unsafe {
                (*(base.add((off + i) * 8) as *const std::sync::atomic::AtomicU64))
                    .load(Ordering::Relaxed)
            };
            if word != 0 {
                self.satb_pre_barrier_slow(word as usize);
            }
        }
    }

    /// Out-of-line remainder of [`Self::satb_pre_barrier`], so the fast path
    /// is a load and a branch and nothing else is inlined into every store
    /// site in the VM.
    #[cold]
    pub fn satb_pre_barrier_slow(&self, old_addr: usize) {
        if old_addr == 0 {
            return;
        }
        // zt t31/zc11 (R4): an overwritten stale word names from-space, which
        // the registry screen below would drop; log where it lives now.
        let old_addr = self.satb_forward(old_addr);
        let filter_mode = self.counters.satb_filter_marked.load(Ordering::Relaxed);
        // zr zs s1/mc (`CRATONVM_ZGC_SATB_DEFER_REGISTRY`, opt-in): the
        // registry screen below is one load of the object-start bitmap per
        // recorded store -- a line the mutator never otherwise touches, so a
        // likely miss on a big heap, taken serially, one store at a time. With
        // the flush-time filter (mode 2) the batch is already re-read at the
        // flush, 256 independent loads in one loop; the screen can ride along
        // there. Sound because nothing between the store and that flush
        // dereferences the address: the buffer holds a `u64`, the flush's
        // mark-bit test is a BITMAP test (hence `mark_bits.is_some()`: the
        // header arm would read a header through it), and every later
        // consumer gates on `is_in_heap` anyway (`push_mutator_batch`, the
        // mark-end `push_roots`). The registry only grows during a cycle (no
        // sweep runs inside one), so a base at the store is a base at the
        // flush.
        let defer = filter_mode == Z_SATB_FILTER_FLUSH
            && self.mark_bits.is_some()
            && self.counters.satb_defer_registry.load(Ordering::Relaxed)
            && self.satb_thread_buffers_on();
        if !defer && !self.registry.contains(old_addr) {
            // A null overwrite carries no edge, and an address this heap never
            // handed out is not ours to mark — the same gate
            // `ZMarkContext::is_in_heap` applies to every child pointer.
            return;
        }
        // zr z7/cm: AN ALREADY-MARKED OLD VALUE NEEDS NO RECORD
        // (`CRATONVM_ZGC_SATB_FILTER_MARKED`, default on, `=0` kill).
        //
        // During a cycle a mark bit is set by exactly two things: the
        // marker's claim (`try_mark` -> `mark_claim`, claim-on-push, so the
        // object is queued and WILL be scanned) and black allocation (an
        // object born after the snapshot, which needs no scan -- see
        // `allocate_black_if_marking`). Either way the object is already in
        // the mark set the sweep will trust, and recording it would only feed
        // `push_mutator_batch`'s `claim_unmarked`, which drops it. G1's SATB
        // queue filter (`SATBMarkQueueSet::filter`) is the same rule.
        //
        // Without it, every 256 records pay the hand-off -- the global
        // `conc_pool` lock, a drain of every ingress bucket, a claim pass --
        // for values that are overwhelmingly marked once the marker has passed
        // them: on a store loop over an old graph (`Zr1MkParmarkCost`,
        // `Zr7CmFixedWork`) that is nearly every record after the fixed point.
        //
        // WHERE THE BIT IS READ (zr z7b/cm). Per store (mode 1) adds one load
        // of the MARK bitmap to every store -- a cache line the mutator
        // otherwise never touches, i.e. a likely miss on a large heap with a
        // randomly walked graph -- plus a striped counter RMW, in exchange for
        // the thread-buffer lock and push. The host's non-interleaved z7b rows
        // could not show mode 1 cheaper than the kill arm. So the DEFAULT
        // (mode 2) leaves the per-store path exactly as it was and filters the
        // BATCH at the thread-buffer flush, below: 256 independent bit loads
        // in one loop (overlapping misses), one counter add, and the hand-off
        // skipped outright when nothing is left -- which is what the hand-off
        // savings were.
        //
        // The bits read here are this cycle's: `start_concurrent_mark` clears
        // them (or proves them clear) before `set_mark_active(true)`.
        if filter_mode == Z_SATB_FILTER_STORE && self.mark_is_set(old_addr) {
            self.counters.satb_filtered.inc();
            return;
        }
        // Bucket by THREAD, not by address -- see `satb_ingress_slot`. Any
        // well-distributed key is correct here (`drain_into` empties every
        // bucket, so the choice is pure performance), and a per-thread key is the
        // one that actually separates mutators: an address key collides whenever
        // two threads store into the same region, and makes each thread cycle
        // through all 16 locks instead of reusing one.
        // THE HANDOFF COUNT COMES OUT OF THE BUCKET'S OWN LOCK.
        //
        // This was `self.mark_ingress_pushes.fetch_add(1, Relaxed)` -- a
        // `fetch_add` on ONE cache line, per reference store, from every mutator.
        // Bucketing the ingress so mutators land on different mutexes and then
        // funnelling every push through a single shared counter leaves the
        // contention exactly where it was; the counter was the bottleneck the
        // buckets exist to remove. `push` now returns the count from inside the
        // lock it already took, so the per-store path has **no shared atomic at
        // all**.
        //
        // The count is now PER BUCKET, so the interval means "this many pushes
        // into one bucket" -- with `Z_MARK_INGRESS_BUCKETS` buckets and addresses
        // spread over them, handoffs are correspondingly rarer per bucket and
        // about as frequent overall. `Z_SATB_HANDOFF_INTERVAL` is divided by the
        // bucket count for that reason; see its own note.
        //
        // zr z2/cc: PER-THREAD BUFFER FIRST (`CRATONVM_ZGC_SATB_THREAD_BUFFERS`,
        // default on). The store takes this thread's own buffer lock and the
        // ingress is touched once per `Z_MARK_MUTATOR_BUFFER_CAPACITY` stores,
        // each such flush followed by a hand-off to the pool (so the concurrent
        // workers see the batch while the mutators still run). The mark-end
        // replay drains every thread's buffer (`drain_mark_ingress`).
        if self.satb_thread_buffers_on() {
            let mut batch: Vec<u64> = Vec::new();
            match self.counters.satb_thread_bufs.record(
                old_addr as u64,
                mark::Z_MARK_MUTATOR_BUFFER_CAPACITY,
                &mut batch,
            ) {
                mark::ZSatbRecord::Buffered => return,
                mark::ZSatbRecord::Full => {
                    // t1 wave 2 lane Z (`CRATONVM_ZGC_SATB_FLUSH_PAUSE_SAFE`):
                    // this flush is in flight until the guard drops, on every
                    // exit below (published, filtered empty, or unwound), so
                    // a mark end can wait for it and detect a late one.
                    let _flush = self.counters.satb_thread_bufs.flush_guard();
                    // zt t6/rv3 (`zgc-zt4x3-*` item 2): ONE CPU span per
                    // flush, opened here so the filter below (the registry
                    // screen and the mark-bit pass) is inside it. The
                    // hand-offs charge it once a pool is found; with no pool
                    // the closing clock read is skipped (it was a second
                    // span per flush, opened before the pool lookup).
                    let cpu = mark::ZThreadCpuSpan::start(self.collection_cpu_accounting_on());
                    // zr z7b/cm: the default filter runs HERE, once per
                    // buffer -- see the note above. An all-marked batch skips
                    // the ingress and the hand-off (the global `conc_pool`
                    // lock, the bucket drain, the claim pass) entirely.
                    if filter_mode == Z_SATB_FILTER_FLUSH {
                        if defer {
                            let before = batch.len();
                            self.counters
                                .satb_deferred_screened
                                .fetch_add(before, Ordering::Relaxed);
                            batch.retain(|&a| self.registry.contains(a as usize));
                            let refused = before - batch.len();
                            if refused > 0 {
                                self.counters
                                    .satb_deferred_refused
                                    .fetch_add(refused, Ordering::Relaxed);
                            }
                        }
                        let before = batch.len();
                        batch.retain(|&a| !self.mark_is_set(a as usize));
                        let dropped = before - batch.len();
                        if dropped > 0 {
                            self.counters
                                .satb_filtered_batched
                                .fetch_add(dropped, Ordering::Relaxed);
                        }
                        if batch.is_empty() {
                            self.counters
                                .satb_flush_cpu_ns
                                .fetch_add(cpu.elapsed_ns(), Ordering::Relaxed);
                            return;
                        }
                    }
                    // zs s3/ac (`CRATONVM_ZGC_SATB_DIRECT_HANDOFF`, default
                    // ON): with a pool running, the batch goes straight to
                    // `push_mutator_batch`. The heap ingress round trip
                    // (push into one bucket, then drain all of them back
                    // out) stays for the no-pool arm, which needs the entries
                    // kept for the mark-end replay.
                    if self.counters.satb_direct_handoff.load(Ordering::Relaxed)
                        && self.hand_thread_batch_to_the_marker(&batch, &cpu)
                    {
                        return;
                    }
                    self.counters
                        .mark_ingress
                        .push_batch(satb_ingress_slot(), &mut batch);
                    self.hand_satb_batch_to_the_marker_spanned(Some(&cpu));
                    return;
                }
                // No thread-local storage (thread teardown): the old path.
                mark::ZSatbRecord::Unbuffered => {}
            }
        }
        // t1 wave 3 lane Q (R-4): the per-store publish is in flight from
        // before its push until after its hand-off, as a buffer flush is, so a
        // mark end waits for it or refuses to certify (under
        // `CRATONVM_ZGC_SATB_FLUSH_PAUSE_SAFE`).
        let _publish = true
            .then(|| self.counters.satb_thread_bufs.begin_unbuffered_publish());
        let n = self
            .counters
            .mark_ingress
            .push(satb_ingress_slot(), old_addr as u64);
        if n % Z_SATB_HANDOFF_PER_BUCKET == 0 {
            self.hand_satb_batch_to_the_marker();
        }
    }

    /// Move what the SATB barrier has accumulated into the CONCURRENT marker's
    /// own queues, so the workers trace it while the mutators are still
    /// running.
    ///
    /// # Why this exists rather than draining only at mark end
    ///
    /// Two reasons, and the second would eventually have been a bug report
    /// rather than a slow phase:
    ///
    /// * **Pause time.** Everything still in the ingress when the collection
    ///   arrives is traced INSIDE the pause. Handing it over in batches moves
    ///   that tracing into the concurrent phase, which is the whole point of
    ///   the phase.
    /// * **Memory.** `mark_ingress` is unbounded. A store-heavy workload with
    ///   a long concurrent phase would park millions of `u64`s in it -- growth
    ///   proportional to reference stores, not to the live set, and invisible
    ///   in every heap figure the VM reports.
    ///
    /// The receiver is `ZMarkCoordinator::push_mutator_batch` (see the note
    /// in the body for why not `push_roots`): it gates each address through
    /// `is_in_heap`, claims it with the atomic `try_mark` (so an address a
    /// worker has already marked is dropped), lands the survivors in one
    /// ingress bucket of the POOL and wakes it.
    ///
    /// How often it runs (zs s3/ac, residual 13 of
    /// `zgc-zs2mc2-mark-engine-review-residuals-FIXED-20261019.md`): on the
    /// unbuffered arm once per [`Z_SATB_HANDOFF_PER_BUCKET`] pushes into a
    /// bucket; on the per-thread-buffer arm (the default) once per FULL
    /// thread buffer, i.e. every [`mark::Z_MARK_MUTATOR_BUFFER_CAPACITY`]
    /// recorded stores per thread -- and there only when no pool was running
    /// at the flush or `CRATONVM_ZGC_SATB_DIRECT_HANDOFF=0`, because the
    /// direct arm ([`Self::hand_thread_batch_to_the_marker`]) skips the heap
    /// ingress altogether. Each call takes the per-heap `conc_pool` mutex.
    #[cold]
    pub fn hand_satb_batch_to_the_marker(&self) {
        self.hand_satb_batch_to_the_marker_spanned(None);
    }

    /// [`Self::hand_satb_batch_to_the_marker`] charging `span` (the caller's
    /// one span per flush, which already covers its filter) when given, else
    /// a span of its own opened once a pool is found.
    #[cold]
    pub fn hand_satb_batch_to_the_marker_spanned(&self, span: Option<&mark::ZThreadCpuSpan>) {
        // zt t3/mk (item 1 of `zgc-zs3ac-collection-cpu-residual-shares`):
        // the flush half of the SATB mutator cost, one clock pair per flush.
        // zt t6/rv3: the span opens after the pool lookup, so the no-pool arm
        // reads no clock here.
        let pool = {
            let guard = self.counters.conc_pool.lock();
            match guard.as_ref() {
                Some(p) => std::sync::Arc::clone(p),
                // No pool: either the cycle is closing (the mark-end handshake
                // has already taken it) or the barrier is armed by a test. The
                // ingress keeps the entries either way, and
                // `finish_concurrent_mark` replays whatever is left.
                None => return,
            }
        };
        let cpu = span
            .copied()
            .unwrap_or_else(|| mark::ZThreadCpuSpan::start(self.collection_cpu_accounting_on()));
        let mut batch: Vec<u64> = Vec::new();
        if self.counters.mark_ingress.drain_into(&mut batch) == 0 {
            if span.is_some() {
                // The caller's filter ran inside this span; book it.
                self.counters
                    .satb_flush_cpu_ns
                    .fetch_add(cpu.elapsed_ns(), Ordering::Relaxed);
            }
            return;
        }
        self.counters
            .conc_ingress_replayed
            .fetch_add(batch.len(), Ordering::Relaxed);
        self.counters
            .conc_ingress_handed
            .fetch_add(batch.len(), Ordering::Relaxed);
        // THE INGRESS, NOT THE STRIPES. This runs on a MUTATOR thread, once
        // every `Z_SATB_HANDOFF_PER_BUCKET` reference stores, while the worker
        // pool is tracing. `push_roots` publishes into `ZMarkStripeSet`, taking
        // up to `stripe_count()` stripe mutexes (8 to 128) and then the
        // terminator state lock -- contending with the workers' STEAL path,
        // which is the path that runs when the pool is starved, i.e. exactly
        // when added latency hurts most. `ZMarkIngress`'s own design note
        // ("Why this is not just another stripe") forbids precisely this
        // arrangement; the old call site argued for `push_roots` on the
        // strength of its claiming behaviour and did not notice that the
        // publication target came along with it.
        //
        // `push_mutator_batch` claims each address identically -- same
        // `is_in_heap` gate, same atomic `try_mark`, same dedup, so the
        // "everything queued is already marked" invariant is unchanged -- and
        // lands the survivors in ONE ingress bucket with a single uncontended
        // mutex, followed by the same `note_work_published`.
        //
        // Keyed by `satb_ingress_slot()`, which is already what
        // `satb_pre_barrier_slow` keys the ingress by, so a thread stays on one
        // warm bucket for both the accumulate and the hand-off.
        //
        // Completeness is unchanged: `ZMarkTerminator::worker_idle` probes
        // `ingress.has_work()` under the same lock as `stripes.has_work()`, so
        // no fixed point can be declared over a non-empty ingress, and
        // `try_end_mark` folds the ingress into the stripes at the mark-end
        // safepoint. Prioritisation moves in the intended direction: a worker
        // reaches the ingress only after its own stripe, four random steals and
        // a full deterministic sweep -- only when it is starved, which is when
        // it should be looking at mutator work.
        let claimed = pool.push_mutator_batch(satb_ingress_slot(), &batch);
        Self::rearm_after_satb_claim(&pool, claimed);
        self.counters
            .satb_flush_cpu_ns
            .fetch_add(cpu.elapsed_ns(), Ordering::Relaxed);
    }

    /// zs s3/ac (`CRATONVM_ZGC_SATB_DIRECT_HANDOFF`, default ON, `=0` kill;
    /// residual 1 of `zgc-zs2mc2-mark-engine-review-residuals-FIXED-20261019.md`):
    /// hand one thread's full SATB buffer straight to the running concurrent
    /// pool. Returns `false`, having done nothing, when no pool is running;
    /// the caller then keeps the entries in the heap ingress for the mark-end
    /// replay, exactly as before.
    ///
    /// What it saves per flush, against [`Self::hand_satb_batch_to_the_marker`]:
    /// the push of the batch into a heap-ingress bucket, the drain of all
    /// [`mark::Z_MARK_INGRESS_BUCKETS`] buckets back out (16 locks, one fresh
    /// `Vec`), and the copy. The claim is the same call with the same batch.
    ///
    /// Sound for the same reasons the ingress route is: `push_mutator_batch`
    /// gates, claims and publishes identically whoever calls it; the pool's
    /// fixed point probes its own ingress under the terminator lock, so it
    /// cannot terminate over this batch; and a mutator inside this barrier is
    /// not at a safepoint, so the mark-end handshake (which takes the pool
    /// out of `conc_pool` at one) cannot run between the pool read and the
    /// push. Entries other threads left in the heap ingress (the unbuffered
    /// arm) are not drained here; they go on their own interval or at the
    /// mark-end replay, which drains every bucket.
    #[cold]
    pub fn hand_thread_batch_to_the_marker(&self, batch: &[u64], cpu: &mark::ZThreadCpuSpan) -> bool {
        // zt t3/mk: the flush's CPU, as in `hand_satb_batch_to_the_marker`.
        // zt t6/rv3: `cpu` is the caller's one span per flush (it covers the
        // filter), charged only past the pool lookup.
        let pool = {
            let guard = self.counters.conc_pool.lock();
            match guard.as_ref() {
                Some(p) => std::sync::Arc::clone(p),
                None => return false,
            }
        };
        self.counters
            .conc_ingress_replayed
            .fetch_add(batch.len(), Ordering::Relaxed);
        self.counters
            .conc_ingress_handed
            .fetch_add(batch.len(), Ordering::Relaxed);
        self.counters
            .satb_direct_handed
            .fetch_add(batch.len(), Ordering::Relaxed);
        let claimed = pool.push_mutator_batch(satb_ingress_slot(), batch);
        Self::rearm_after_satb_claim(&pool, claimed);
        self.counters
            .satb_flush_cpu_ns
            .fetch_add(cpu.elapsed_ns(), Ordering::Relaxed);
        true
    }

    /// The tail every SATB hand-off shares: re-arm a pool that has already
    /// declared its fixed point, unless the batch claimed nothing.
    pub fn rearm_after_satb_claim(pool: &mark::ZMarkCoordinator, claimed: usize) {
        // zs s2/mc2: a batch that claimed NOTHING (every address already
        // marked -- the steady state once the marker has caught up, and every
        // batch after `abandon_concurrent_mark` bound the inert context) added
        // no work, so it must not re-arm a finished pool: that woke every
        // worker to sweep all stripes and the ingress and run the handshake
        // again for zero objects. Work an earlier batch published re-armed the
        // pool when IT was handed over. Inert unless a concurrent cycle runs.
        if claimed == 0 {
            return;
        }

        // RE-ARM if the pool has already finished.
        //
        // `ZMarkTerminator::note_work_published` -- which `push_mutator_batch`
        // reaches, as `push_roots` did -- deliberately does NOT clear
        // `terminated`, and its doc says why: a
        // pool that has declared its fixed point has no running worker left to
        // clear the flag, so clearing it from outside would produce a state
        // nobody can leave. The supported transition back is `arm`, which is
        // what `start_marking` is.
        //
        // Without this, the FIRST time the marker catches up with the graph it
        // stops for good, and every SATB reference published afterwards sits in
        // the stripes until the collection pause drives `mark_to_completion` --
        // i.e. exactly the work this phase exists to move OFF the pause, banked
        // back onto it. Note the narrower case needs nothing: a pool that is
        // still active picks published work up by itself, because the last
        // worker out re-probes the queues under the termination lock and
        // resumes rather than terminating.
        //
        // Two mutators racing here both call `arm`, and that is safe: a worker
        // still inside the older generation leaves its handshake through the
        // `cycle_generation != my_cycle` arm without touching the active count
        // (which `arm` has already reset wholesale) and rejoins the new
        // generation from `ZMarkWorker::run`.
        if pool.shared().terminator().is_terminated() {
            pool.start_marking();
        }
    }

    /// Card an OLD object whose fields have just been written --
    /// `zgc::remembered`.
    ///
    /// # A card names an OBJECT to re-scan, not a slot
    ///
    /// The first version of this recorded the slot address and
    /// `remembered_roots` read a reference word straight out of it. That is
    /// the *precise* remembered set, and this VM cannot feed it: the barrier
    /// hook every reference store already reaches is
    /// `GarbageCollector::write_barrier(obj, stored_value)`, which is handed
    /// the **object**. Recording an object base and re-enumerating its
    /// reference slots at cycle time is the classic card design, it is what
    /// the available hook can actually supply, and it is robust to the four
    /// different slot shapes an object can have -- a hand-computed slot offset
    /// is not, which is how the first version carded the tag word of a
    /// 16-byte cell instead of its reference word.
    ///
    /// # Cost while there is no old generation
    ///
    /// One relaxed load and a not-taken branch -- see [`Self::has_old_objects`].
    /// It used to be an `is_empty` check on a mutex, which was free for a
    /// barrier nothing called and is not free for one on every `putfield`.
    #[inline]
    pub fn note_ref_store(&self, obj_addr: usize) {
        if !self.has_old_objects.load(Ordering::Relaxed) {
            return;
        }
        self.note_ref_store_slow(obj_addr);
    }

    #[cold]
    pub fn note_ref_store_slow(&self, obj_addr: usize) {
        // A store into a YOUNG object needs no card: a young cycle traces every
        // young object anyway. The test is the object's own age, one relaxed
        // load out of a header this thread has just written to.
        let promo = self.promotion_age();
        if self.header_ref(obj_addr as *mut u8).gc_age() < promo {
            return;
        }
        self.card_object(obj_addr);
    }

    /// Card `obj_addr` with no age test -- the promotion path and the
    /// post-relocation re-card, both of which already know the object is old.
    ///
    /// # The card is page-keyed and the object may be anywhere
    ///
    /// `page` and `offset` are a plain division of `addr - base`, so
    /// `base + page * PAGE + offset == addr` for **every** arena address,
    /// including the large-object region above the logical grid. The grid is
    /// therefore conceptual here rather than a bound: a card can name any object
    /// in the arena, and [`Self::young_extra_roots`] reconstructs the same
    /// address from the same arithmetic.
    pub fn card_object(&self, obj_addr: usize) {
        let base = self.arena_base;
        if base == 0 || obj_addr < base {
            return;
        }
        let page = ((obj_addr - base) / Self::Z_LOGICAL_PAGE_BYTES) as u64;
        let offset = (obj_addr - base) % Self::Z_LOGICAL_PAGE_BYTES;
        // Idempotent, and after the first store to a page it is a read-lock
        // probe. Registering on demand rather than pre-registering the whole
        // grid keeps the table to the pages that actually hold old objects with
        // written fields.
        // ONE lookup, not two. `register_old_page` already returns the set it
        // found or created; asking the table for the same page again -- which
        // `ZRememberedTable::remember(page_id, ..)` does -- is a second
        // `RwLock` reader acquire and a second FxHash probe on the hot path of
        // every old-generation reference store in the VM.
        //
        // `card_object_two_lookups` below keeps the old shape so the A/B in
        // `measure_the_card_barrier` is a call apart rather than a rebuild.
        //
        // CONTEXT-LOCAL, not allocator ids: `page` is `(addr - arena_base) /
        // Z_LOGICAL_PAGE_BYTES`, a dense index over this heap's own model of
        // the arena. `ZgcRealHeap` has no `ZPageAllocator`, so there is no
        // `ZPageReal::id()` for it to be. Declaring it correctly is what keeps
        // `ZRememberedSetTable`'s namespace latch able to catch a second
        // registrar that DOES pass allocator ids -- those collide numerically
        // with these rather than missing, so the latch is the only thing that
        // can see it. All three registration sites in this file changed
        // together (handoff-d-zgc-page-id-namespace); changing one would leave
        // the latch in conflict with the others and fire `latch_namespace`'s
        // `debug_assert!` on every debug run.
        //
        // ...AND WITH THE PAGE'S BASE ADDRESS (wave 3,
        // `handoff-n-census-engagement-and-namespaced-base.md`). Without it
        // `ZRememberedSetTable::iterate_slot_addresses` cannot turn a bitmap
        // offset back into a slot address, so it reports the page in
        // `pages_without_a_base` and yields no roots -- which is the same
        // silently-dropped-root shape the base was added to remove. The base is
        // free to compute here: it is the page index this line just derived,
        // multiplied back out.
        //
        // Cost on the card barrier: one relaxed load and a compare on a cache
        // line `register_old_page_in` has just touched, against an `RwLock`
        // read and an `FxHash` probe it already pays.
        let set = self.remembered.register_old_page_with_base_in(
            remembered::ZPageIdNamespace::ContextLocal,
            page,
            (base + page as usize * Self::Z_LOGICAL_PAGE_BYTES) as u64,
            Self::Z_LOGICAL_PAGE_BYTES,
        );
        set.remember(offset);
    }

    // =====================================================================
    // zr z2/g2 -- ELEMENT CARDS FOR LARGE REFERENCE ARRAYS (G2)
    // =====================================================================
    //
    // `card_object` cards the RECEIVER, and `young_extra_roots` re-scans every
    // slot of a carded object and keeps the card while any slot is young. For
    // an old `Object[]` of a million elements with one young element that is a
    // million slot reads on every young cycle, and the card never clears
    // (`zgc-zr1gn-object-cards-rescan-whole-old-arrays-SUPERSEDED-20261019.md`).
    //
    // Under `CRATONVM_ZGC_GEN_ELEMENT_CARDS=1` (opt-in, per heap) a store into
    // an old reference array of at least `Z_ELEMENT_CARD_MIN_LEN` elements
    // cards the 8-byte GRANULE holding the element instead
    // (`note_ref_store_element`), and the consumer treats a card that is not an
    // object base as such a granule (`scan_element_granule`). Object cards on
    // large arrays still arise (promotion, the slide's re-card, bulk copies,
    // and every producer that does not know the slot); the consumer CONVERTS
    // each into granule cards for the young elements it finds, once.
    //
    // # Why a granule card cannot drop an edge
    //
    // The card stays set until a young cycle finds no young element in its
    // granule (both of its elements under narrow oops). A granule whose array
    // moved is covered by the slide's object re-card at the destination
    // (`recard_relocated_old_objects`), which the next cycle converts. A
    // granule whose array died resolves to no reference array (or to another
    // one, which is merely scanned) and is dropped -- the edge died with it.
    //
    // # Why the switch is per heap and gates both ends
    //
    // The consumer's default arm DROPS a card that is not a registered base
    // ("the card outlived its object"). A granule card produced while the
    // consumer is in that arm would be a dropped old->young edge. So the one
    // per-heap bit gates the producer and the consumer together, and it is
    // fixed at construction outside tests.

    /// Smallest reference array that gets element cards. Below it the whole
    /// array is a few cache lines and an object card is cheaper to keep.
    pub const Z_ELEMENT_CARD_MIN_LEN: usize = 64;

    /// A reference-array store's card barrier: [`Self::note_ref_store`], but
    /// given the element's slot address so a large array can be carded per
    /// granule. `len` is the array's length (the caller has just bounds-checked
    /// against it). Cross-lane producers (the interpreter's `aastore` fast
    /// path, the JIT post-barrier helper) call this once they know the slot.
    #[inline]
    pub fn note_ref_store_element(&self, obj_addr: usize, slot_addr: usize, len: usize) {
        if !self.has_old_objects.load(Ordering::Relaxed) {
            return;
        }
        self.note_ref_store_element_slow(obj_addr, slot_addr, len);
    }

    #[cold]
    pub fn note_ref_store_element_slow(&self, obj_addr: usize, slot_addr: usize, len: usize) {
        let promo = self.promotion_age();
        if self.header_ref(obj_addr as *mut u8).gc_age() < promo {
            return;
        }
        if len >= Self::Z_ELEMENT_CARD_MIN_LEN
            && self.counters.gen_element_cards_on.load(Ordering::Relaxed)
        {
            self.card_object(slot_addr & !7);
        } else {
            self.card_object(obj_addr);
        }
    }

    /// `Some(len)` if the registered object at `addr` is a reference array
    /// long enough for element cards (and plausibly sized).
    pub fn large_reference_array_len(&self, addr: usize) -> Option<usize> {
        let header = self.header_ref(addr as *mut u8);
        if header.kind() != ObjectKind::Array
            || header.element_type() != ArrayElementType::Reference
        {
            return None;
        }
        let len = header.array_length() as usize;
        if len < Self::Z_ELEMENT_CARD_MIN_LEN {
            return None;
        }
        // zs s2/rf (dg residual page, row 3 -- and a soundness hole, not only
        // a cost): the store side cards an element GRANULE for every reference
        // array of `len >= Z_ELEMENT_CARD_MIN_LEN` (`note_ref_store_element_slow`
        // takes the header's length unscreened), so an over-long array's card
        // reached `scan_element_granule`, which answered "no array here" and
        // DROPPED it without rooting its young targets: a young object stored
        // only into an old array past `MAX_PLAUSIBLE_ARRAY_LEN` was freed by
        // the next young cycle. Such an array is sized through the live
        // screen (memoised per pause); kill with the screen's own
        // `CRATONVM_ZGC_REWRITE_OVERSIZE_ARRAYS=0`.
        if len <= Self::MAX_PLAUSIBLE_ARRAY_LEN
            || self.oversize_live_array_extent(addr, header).is_some()
        {
            Some(len)
        } else {
            None
        }
    }

    /// Scan the elements of the reference array covering the 8-byte granule at
    /// `granule`: root every young target, and answer whether the card must be
    /// kept. A granule that resolves to no registered reference array -- its
    /// array died, or moved and was re-carded at the destination -- is dropped.
    pub fn scan_element_granule(
        &self,
        granule: usize,
        promotion_age: u8,
        last_array: &mut Option<(usize, usize, usize)>,
        roots: &mut Vec<usize>,
    ) -> bool {
        let (base, data, end) = match *last_array {
            Some((b, d, e)) if granule + 8 > d && granule < e && self.registry.contains(b) => {
                (b, d, e)
            }
            _ => {
                if granule <= self.arena_base {
                    return false;
                }
                let Some(b) = self
                    .registry
                    .nearest_base_within_total(granule, self.arena_base)
                else {
                    return false;
                };
                let Some(len) = self.large_reference_array_len(b) else {
                    return self.scan_refused_element_granule(b, granule, promotion_age, roots);
                };
                let d = b + ARRAY_DATA_OFFSET;
                let e = d + len * cratonvm_types::ref_element_size();
                if granule + 8 <= d || granule >= e {
                    return false; // the granule is not inside this array's elements
                }
                *last_array = Some((b, d, e));
                (b, d, e)
            }
        };
        let _ = base;
        let stride = cratonvm_types::ref_element_size();
        // The first element slot at or after the granule start.
        let first = if granule <= data {
            data
        } else {
            data + (granule - data).div_ceil(stride) * stride
        };
        let mut keep = false;
        let mut slot = first;
        while slot < granule + 8 && slot < end {
            // SAFETY: `slot` is an element slot of a registered, plausibly
            // sized reference array (`data <= slot < end`).
            let target =
                unsafe { cratonvm_types::narrow_oop::read_ref_slot(slot as *mut u8) } as usize;
            if target != 0
                && self.registry.contains(target)
                && self.addr_is_young(target, promotion_age)
            {
                roots.push(target);
                keep = true;
            }
            slot += stride;
        }
        keep
    }

    /// The arm of [`Self::scan_element_granule`] for a card whose nearest
    /// registered base `b` is a reference array past `MAX_PLAUSIBLE_ARRAY_LEN`
    /// that [`Self::large_reference_array_len`] refused (the live screen's
    /// switch is off, or the claimed extent swallows a neighbour or runs past
    /// the arena). The store side cards such an array's granule from its
    /// unscreened length (`note_ref_store_element_slow`), so dropping the card
    /// lost a young object stored only into it whenever the header is real
    /// and the screen is wrong.
    ///
    /// Scans only the granule's own slots (at most two), as CONSERVATIVE
    /// roots: a word is rooted only if it names a registered young base, so a
    /// wrong header can at worst keep a dead young object alive for one more
    /// cycle. The granule must lie past `b`'s array header and inside the
    /// arena; nothing else about the claimed length is trusted, and the
    /// array is not cached in `last_array`. Answers whether the card is kept
    /// (something young was rooted). `=0` of
    /// `CRATONVM_ZGC_ELEMENT_CARD_REFUSED_SCAN` restores the drop.
    pub fn scan_refused_element_granule(
        &self,
        b: usize,
        granule: usize,
        promotion_age: u8,
        roots: &mut Vec<usize>,
    ) -> bool {
        if !self
            .counters
            .element_card_refused_scan
            .load(Ordering::Relaxed)
        {
            return false;
        }
        let header = self.header_ref(b as *mut u8);
        if header.kind() != ObjectKind::Array
            || header.element_type() != ArrayElementType::Reference
            || (header.array_length() as usize) <= Self::MAX_PLAUSIBLE_ARRAY_LEN
            || granule < b + ARRAY_DATA_OFFSET
            || granule.saturating_add(8) > self.arena_end
        {
            return false;
        }
        self.counters
            .element_card_refused_scans
            .fetch_add(1, Ordering::Relaxed);
        let stride = cratonvm_types::ref_element_size();
        let data = b + ARRAY_DATA_OFFSET;
        let mut slot = data + (granule - data).div_ceil(stride) * stride;
        let mut keep = false;
        while slot + stride <= granule + 8 {
            // SAFETY: `[slot, slot + stride)` lies inside `[granule, granule +
            // 8)`, past a registered base's header and below `arena_end`; the
            // card was set by a store into this granule, so it is committed
            // arena memory. The word is only a candidate root.
            let target =
                unsafe { cratonvm_types::narrow_oop::read_ref_slot(slot as *mut u8) } as usize;
            if target != 0
                && self.registry.contains(target)
                && self.addr_is_young(target, promotion_age)
            {
                roots.push(target);
                keep = true;
            }
            slot += stride;
        }
        keep
    }

    /// Element granules [`Self::scan_refused_element_granule`] scanned.
    pub fn element_card_refused_scans(&self) -> usize {
        self.counters
            .element_card_refused_scans
            .load(Ordering::Relaxed)
    }

    /// `CRATONVM_ZGC_GEN_ELEMENT_CARDS` for THIS heap (tests). Only flip it on
    /// a heap with no cards yet: see the section note.
    pub fn set_gen_element_cards_enabled(&self, on: bool) {
        self.counters
            .gen_element_cards_on
            .store(on, Ordering::Relaxed);
    }

    /// [`Self::card_object`] as it was before 2026-09-04: register, then look
    /// the same page up a second time to set the bit. Kept only as the A/B arm
    /// for `measure_the_card_barrier`; nothing on a live path calls it.
    #[cfg(test)]
    pub fn card_object_two_lookups(&self, obj_addr: usize) {
        let base = self.arena_base;
        if base == 0 || obj_addr < base {
            return;
        }
        let page = ((obj_addr - base) / Self::Z_LOGICAL_PAGE_BYTES) as u64;
        let offset = (obj_addr - base) % Self::Z_LOGICAL_PAGE_BYTES;
        // Same namespace as `card_object` -- see the comment there. The A/B arm
        // has to agree with the arm it is compared against, or the first call
        // on whichever runs first latches the table and the other one trips the
        // conflict assert.
        // ...and the same base, for the same reason: an A/B arm that registers
        // differently from the arm it is compared against is measuring two
        // different registrations.
        self.remembered.register_old_page_with_base_in(
            remembered::ZPageIdNamespace::ContextLocal,
            page,
            (base + page as usize * Self::Z_LOGICAL_PAGE_BYTES) as u64,
            Self::Z_LOGICAL_PAGE_BYTES,
        );
        self.remember_old_to_young(page, offset);
    }

    /// The promotion age, clamped into what a 4-bit header field can express.
    ///
    /// [`cratonvm_types::MAX_GC_AGE`] is 15 and [`ObjectHeader::set_gc_age`]
    /// saturates there, so a promotion age above it would promote nothing, ever
    /// -- the phase would be on, cost the barrier, and never skip an object.
    #[inline]
    pub fn promotion_age(&self) -> u8 {
        self.gen_promotion_age
            .load(Ordering::Relaxed)
            .clamp(1, cratonvm_types::MAX_GC_AGE as u32) as u8
    }

    /// How many old-to-young edges are currently remembered. Diagnostic, and
    /// the discriminator a test needs between "the barrier is wired" and "the
    /// workload made no such store".
    pub fn remembered_edge_count(&self) -> usize {
        self.remembered
            .snapshot()
            .iter()
            .map(|s| s.bits_set())
            .sum()
    }

    /// Arm or disarm the concurrent-mark barrier — Phase 3 wiring and tests.
    ///
    /// Disarming clears the ingress, because a leftover address from a
    /// finished cycle would be republished into the next one as mark work
    /// against an arena that has since been swept and coalesced.
    pub fn set_mark_active(&self, active: bool) {
        if !active {
            self.mark_active.store(false, Ordering::Relaxed);
            // The JIT gate is lowered AFTER the truth, never before. While it
            // still reads armed, compiled code pays a helper call it did not
            // need — the harmless direction. See `JitRefStoreGates`.
            self.store_jit_gate(|g| &g.pre_active, 0);
            // Same direction for the inline start-bit store's `blocked` word
            // (it cannot allocate black): lowered after, and only when the
            // heap is not generational either.
            // (zr z2/g2: generational mode closes it only under
            // `CRATONVM_ZGC_GEN_INLINE_ANNOUNCE=0`.)
            self.set_jit_inline_announce_blocked(self.gen_blocks_inline_announce());
            // `clear` resets the buckets' cumulative counts too.
            self.counters.mark_ingress.clear();
            // ...and the per-thread buffers in front of it (zr z2/cc): the
            // same stale-cycle argument.
            self.counters.satb_thread_bufs.clear_all();
        } else {
            // Raised BEFORE the truth. Arming happens inside the mark-start
            // pause; raising the mirror first also covers any caller that is
            // not inside such a pause.
            //
            // zs s2/rf: "no mutator can sit between its inline gate test and
            // its store while this flips" holds for a COOPERATIVE pause only.
            // The cross-thread take-over freezes a peer at any JIT
            // instruction, including inside that window; the VM's mark start
            // therefore refuses to open a cycle whose pause froze a peer
            // (`zgc_satb_window_refuses`, `CRATONVM_ZGC_SATB_TAKEOVER_COVER`,
            // page `zgc-zr1br-takeover-straddles-the-satb-gate-FIXED-20261017.md`).
            // Any new caller that arms this from a pause with frozen peers
            // needs the same cover.
            self.store_jit_gate(|g| &g.pre_active, 1);
            // Keep the JIT inline announce open when side mark bits exist:
            // newly carved chunks are blackened whole by `carve_tlab_chunk`,
            // while bumping `ownership_epoch` below routes pre-mark TLAB tails
            // through `note_tlab_object` -> `allocate_black_if_marking`.
            self.set_jit_inline_announce_blocked(
                self.mark_bits.is_none() || self.gen_blocks_inline_announce(),
            );
            // `clear` resets the buckets' cumulative counts too.
            self.counters.mark_ingress.clear();
            self.counters.satb_thread_bufs.clear_all();
            self.mark_active.store(true, Ordering::SeqCst);
            if self.mark_bits.is_some() {
                vm_tlab::invalidate_owned_vm_tlab_chunks();
            }
        }
    }

    /// Is the concurrent-mark barrier armed?
    #[inline]
    pub fn mark_active(&self) -> bool {
        self.mark_active.load(Ordering::Relaxed)
    }

    /// How many overwritten references this barrier has published since the
    /// cycle began. See the field doc: zero with the barrier armed means the
    /// workload overwrote no references, which is a different fact from the
    /// barrier not being wired.
    pub fn mark_ingress_pushes(&self) -> usize {
        // Summed out of the buckets, under their own locks. The shared
        // `AtomicUsize` this used to read was removed from the per-store path --
        // see `satb_pre_barrier_slow` for why a striped queue behind one counter
        // is not striped.
        //
        // zr z2/cc: plus what still sits in the per-thread buffers, which the
        // ingress has not counted yet (a flush moves entries into the ingress,
        // where `push_batch` counts them, so nothing is counted twice).
        self.counters.mark_ingress.pushed_total() + self.counters.satb_thread_bufs.pending_total()
    }

    /// Drain the mutator ingress — what a coordinator's mark-end flush calls.
    ///
    /// zr z2/cc: every thread's SATB buffer FIRST, then the ingress buckets,
    /// so the mark-end replay sees every recorded store whichever path took it.
    pub fn drain_mark_ingress(&self, out: &mut Vec<u64>) -> usize {
        let buffered = self.counters.satb_thread_bufs.drain_all_into(out);
        buffered + self.counters.mark_ingress.drain_into(out)
    }

    /// Does this heap's SATB barrier record into per-thread buffers
    /// (`CRATONVM_ZGC_SATB_THREAD_BUFFERS`, zr z2/cc)? Per heap (t1 wave 2
    /// lane Z), one relaxed load.
    #[inline]
    pub fn satb_thread_buffers_on(&self) -> bool {
        self.counters.satb_thread_buffers_on.load(Ordering::Relaxed)
    }

    /// Per-thread SATB buffers registered with this heap (zr z2/cc). Test and
    /// diagnostic hook.
    pub fn satb_thread_buffers_registered(&self) -> usize {
        self.counters.satb_thread_bufs.registered()
    }

    /// How many mark workers a parallel stop-the-world mark should use.
    ///
    /// `0` means "do not go parallel": the caller runs the serial loop, which
    /// has no pool to spawn and no join to pay for. **`1` is NOT serial** --
    /// `collect_garbage` takes the driven path for any `>= 1`, one pool worker
    /// under the `zgc_concurrent` controller. (This doc said "`0` and `1` both
    /// mean do not go parallel" until zr z1/mk; the caller never agreed.)
    ///
    /// `CRATONVM_ZGC_PARMARK=auto` (zr z1/mk) asks for HotSpot ZGC's
    /// pause-worker ergonomic, [`zgc_nparallel_workers`], and maps an answer of
    /// one to `0`: a single driven worker is the coordination protocol with
    /// none of the parallelism, which is the arm measured slower than the
    /// serial loop.
    ///
    /// **Unset means `auto` since zr z3/mt** ([`Z_PARMARK_DEFAULT_AUTO`], the
    /// orchestrator's flip on the z2b host rows); `CRATONVM_ZGC_PARMARK=0` is
    /// the kill switch (the serial loop). An explicit `-XX:ParallelGCThreads=N`
    /// replaces the ergonomic (and a numeric count), `N = 1` meaning serial.
    pub fn parallel_mark_workers(&self) -> usize {
        // DEFAULT `auto` (zr z3/mt). Default-on once before for one day
        // (2026-08-13 to 2026-08-14, a fixed count), reverted when the driven
        // cycle measured +31% pause at one worker and +153% at four; the
        // claim-on-push engine, the persistent pool, the inline driver and
        // zr z2/cc's idle spill since made it scale (see
        // `Z_PARMARK_DEFAULT_AUTO` for the rows).
        //
        // `auto` is HotSpot's 60 %-of-the-CPUs ergonomic capped at 2 % of the
        // heap in 2 MiB pages, so a small heap (64 MiB on 8 CPUs) still gets
        // the serial loop.
        let cores = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(1);
        let raw = Err::<String, std::env::VarError>(std::env::VarError::NotPresent);
        let auto = match &raw {
            Ok(v) => v.trim().eq_ignore_ascii_case("auto"),
            Err(_) => Z_PARMARK_DEFAULT_AUTO,
        };
        // An explicit `-XX:ParallelGCThreads` under `auto` sets the count even
        // where the heap cap would have chosen the serial loop: the operator
        // named one.
        if auto {
            if let Some(p) = self.counters.gc_threads.parallel() {
                return if p <= 1 {
                    0
                } else {
                    p.min(Z_PARMARK_MAX_WORKERS)
                };
            }
        }
        let requested = if auto {
            let heap_bytes = self.arena_end.saturating_sub(self.arena_base);
            match zgc_nparallel_workers(cores, heap_bytes) {
                0 | 1 => 0,
                n => n,
            }
        } else {
            match &raw {
                Ok(v) => v.trim().parse::<usize>().unwrap_or(0),
                Err(_) => 0,
            }
        };
        if requested == 0 {
            return 0;
        }
        // zr z3/mt: `-XX:ParallelGCThreads` sizes the pool PARMARK selected,
        // in place of `auto`'s ergonomic or a numeric count (the explicit flag
        // wins over the A/B switch). One worker is the serial loop, as `auto`
        // maps it. Not clamped to the cores: see `set_gc_worker_threads`.
        if let Some(p) = self.counters.gc_threads.parallel() {
            return if p <= 1 {
                0
            } else {
                p.min(Z_PARMARK_MAX_WORKERS)
            };
        }
        // Never more workers than the machine has cores to run them on: this
        // is a stop-the-world phase, so oversubscription buys nothing and
        // costs context switches inside the pause it is meant to shorten.
        requested.min(cores).min(Z_PARMARK_MAX_WORKERS)
    }

    /// Mark the whole strong closure from `roots` using the parallel engine,
    /// at a stop-the-world — Phase 3 of the ZGC maturity plan.
    ///
    /// # This is PARALLEL, not CONCURRENT, and the difference is the point
    ///
    /// Every mutator is stopped for the whole of this call. That is what makes
    /// it adoptable today: with no mutator running there is no producer racing
    /// the marker, so it needs **no barrier of any kind** — not the load
    /// barrier real ZGC uses, and not the SATB pre-write barrier
    /// [`Self::satb_pre_barrier`] now feeds. `try_end_mark` is expected to
    /// answer `Complete` on the first pass for exactly that reason, and a
    /// `Restart` here would mean the engine found buffered work at a
    /// safepoint where by construction there can be none.
    ///
    /// It is therefore the honest intermediate step between the single-
    /// threaded sweep this collector has always run and the concurrent cycle
    /// the plan ends at: it exercises the coordinator, the striped queues, the
    /// work stealing and the termination handshake against a REAL heap and a
    /// real object graph, where the only prior driver was `TestMarkContext`.
    ///
    /// # What the caller must have done first
    ///
    /// `begin_concurrent_mark_cycle` must be open, or `visit_refs` traces
    /// weak/soft/phantom referents as strong edges and no reference can ever
    /// be cleared. It warns once if not, and this function does not rely on
    /// that warning — it is the caller's contract.
    ///
    /// Returns the engine's stats for the cycle, or `None` when the driver
    /// could not certify a complete mark set — see
    /// [`Self::mark_with_controller_stw`], whose refusal this forwards.
    pub fn mark_parallel_stw(&self, roots: &[u64], workers: usize) -> Option<mark::ZMarkStatsSnapshot> {
        self.mark_with_controller_stw(roots, workers)
    }

    /// zr z11/rv9: is the spawn backoff open? Consumes one skip. Always
    /// `false` under `CRATONVM_ZGC_MARK_SPAWN_BACKOFF=0`.
    pub fn mark_spawn_backoff_skips(&self) -> bool {
        let left = &self.counters.mark_spawn_backoff_left;
        if left.load(Ordering::Relaxed) == 0 {
            return false;
        }
        let taken = left
            .try_update(Ordering::Relaxed, Ordering::Relaxed, |n| n.checked_sub(1))
            .is_ok();
        if taken {
            self.counters
                .mark_spawn_backoff_skips
                .fetch_add(1, Ordering::Relaxed);
        }
        taken
    }

    /// zr z11/rv9: both spawns failed on this cycle; open the backoff.
    pub fn note_mark_spawn_failed(&self) {
        self.counters
            .mark_spawn_backoff_left
            .store(ZGC_MARK_SPAWN_BACKOFF_CYCLES, Ordering::Relaxed);
    }

    /// zr z11/rv9: parallel-mark cycles the spawn backoff sent to the serial
    /// marker.
    pub fn mark_spawn_backoff_skips_total(&self) -> usize {
        self.counters
            .mark_spawn_backoff_skips
            .load(Ordering::Relaxed)
    }

    /// `CRATONVM_ZGC_MARK_POOL_PERSISTENT`: keep the stop-the-world mark pool
    /// alive between collections instead of spawning and joining its worker
    /// threads on every one. Default on; `0` restores per-cycle construction.
    ///
    /// Measured cost of that construction, by
    /// `measure_the_pool_construction_cost`: 270us at one worker, 420us at
    /// two, 546us at four, 966us at eight -- per CYCLE. Worth removing, and
    /// not on its own enough to make parallel marking beat the serial loop:
    /// that gap is 1.6-4.0 ms and this is 10-18% of it. The rest is the
    /// coordination protocol, which this does not touch.
    ///
    /// # What the end-to-end arm can and cannot show
    ///
    /// BinTreesClassic 16 at -Xmx128m, release, four workers, interleaved,
    /// 8 cycles per run: mark_us mean 8238/9037/7526/6924 with the pool kept
    /// against 8412/8604/10147/10319 without -- the right direction in 3 of 4
    /// pairs, means 7.9 ms against 9.4 ms.
    ///
    /// Do not read that 1.5 ms as this change. The component measurement says
    /// 546us is removed at four workers, and a run-to-run spread of 6.9-10.3
    /// ms cannot resolve it: the arm is CONSISTENT with the saving and is not
    /// evidence for its size. `measure_the_pool_construction_cost` is the
    /// instrument that measures what this actually removes; the mark pause is
    /// dominated by the coordination protocol and moves under it.
    ///
    /// (zr zs s1/mc: this doc had come adrift onto `mark_spawn_backoff_skips`.)
    pub fn persistent_mark_pool(
        &self,
        workers: usize,
    ) -> Option<std::sync::Arc<mark::ZMarkCoordinator>> {
        match None::<std::ffi::OsString> {
            Some(raw) => {
                let v = raw.to_string_lossy().trim().to_ascii_lowercase();
                if matches!(v.as_str(), "0" | "off" | "false" | "no") {
                    return None;
                }
            }
            None => {}
        }
        let mut slot = self.counters.mark_pool.lock();
        if let Some((cached_workers, pool)) = slot.as_ref() {
            // zs s2/mc2: a pool a panicking worker stopped is replaced, not
            // reused (it would report every later cycle `stopped_early`).
            if *cached_workers == workers && !pool.is_stopped() {
                return Some(std::sync::Arc::clone(pool));
            }
        }
        // Built with the INERT context, never the heap: a pool that outlives
        // its cycles and held an `Arc` to the heap that owns it would keep
        // that heap alive forever, and its own threads with it. The real
        // context is bound per cycle by `begin_cycle_with`.
        let inert: std::sync::Arc<dyn mark::ZMarkContext> =
            std::sync::Arc::new(mark::ZInertMarkContext);
        // zr z3/mt: a failed spawn is `None` (the caller's per-cycle arm then
        // tries once more and, failing too, falls back to the serial marker)
        // rather than a panic inside the pause. The pool the slot held is
        // kept: it is still a working pool of another size.
        let pool = match mark::ZMarkCoordinator::try_new(inert, workers) {
            Ok(pool) => std::sync::Arc::new(pool),
            Err(e) => {
                tracing::warn!(
                    target: "zgc",
                    workers,
                    error = %e,
                    "zgc mark: could not spawn the stop-the-world mark pool"
                );
                return None;
            }
        };
        *slot = Some((workers, std::sync::Arc::clone(&pool)));
        Some(pool)
    }

    /// Drive one whole marking cycle through
    /// [`crate::zgc_concurrent::ZgcConcurrentMarkController`] against this
    /// heap — the adoption Phase 3's exit criterion asks for.
    ///
    /// # What this closes
    ///
    /// The plan's Phase 3 exits on "`zgc_concurrent`'s coordinator drives a
    /// real collection". Until this function that was false in a way no
    /// measurement would have caught: the driver existed, it was complete, it
    /// was unit-tested, and its only context was `TestMarkContext`. The heap
    /// marked with its own single-threaded loop and later with a bespoke
    /// `mark_to_completion` call that used the worker pool but **not the
    /// driver** — so the restart loop, the mark-end handshake and the
    /// reference re-drain were all still bypassed on the real heap.
    ///
    /// # Why `ZgcNoMutatorSafepoint` is the CORRECT safepoint here
    ///
    /// Its doc says it is legal "only when the thread driving the heap is the
    /// sole mutator", and that is exactly the state a `StopTheWorldToken`
    /// proves: every other mutator is parked (or was forcibly stopped and
    /// conservatively scanned) before `collect_garbage` is entered. There is
    /// nothing to stop and no per-thread buffer to flush, so a no-op
    /// implementation is not a stub standing in for the real thing — it is the
    /// right answer to the question the trait asks. A **concurrent** cycle
    /// needs the real one (plan item C1); this one does not.
    ///
    /// # Why `refs: None` is safe here, against the parameter's own warning
    ///
    /// `ZgcConcurrentMarkParams`'s doc says `None` "skips the phase entirely,
    /// which is only correct for a heap with no registered `Reference`
    /// objects (i.e. a test)". That warning is about a **concurrent** cycle,
    /// where the mark-end safepoint is the only place a complete mark set
    /// exists and therefore the only place the reference phase can run.
    ///
    /// Here the phase is not skipped — it runs immediately after this returns,
    /// in `collect_garbage`, exactly where it always has, including its
    /// existing INT-8 remark that re-drains whatever `keep_alive` resurrects.
    /// Moving it into the hook is plan item C3 and belongs with C1, not before
    /// it: doing it now would duplicate a working reference phase for no
    /// change in behaviour.
    ///
    /// # Fail-closed
    ///
    /// `mark_set_complete` is the driver's load-bearing verdict and a sweep
    /// against an incomplete mark set is a use-after-free. This returns `None`
    /// rather than stats when the driver cannot certify one, and the caller
    /// falls back to the single-threaded marker. That fallback is not
    /// belt-and-braces: it is the only reason this adoption can be default-on.
    ///
    /// (zr zs s1/mc: this doc had come adrift onto `mark_spawn_backoff_skips`.)
    pub fn mark_with_controller_stw(
        &self,
        roots: &[u64],
        workers: usize,
    ) -> Option<mark::ZMarkStatsSnapshot> {
        use crate::zgc_concurrent::{
            ZgcConcurrentMarkController, ZgcConcurrentMarkParams, ZgcNoMutatorSafepoint,
        };
        // zr z4/rv4 (fault injection, `CRATONVM_DBG_ZGC_PARMARK_FAIL=1` or the
        // unit-test thread flag): the state a driver failure leaves behind --
        // the roots claimed (`push_roots` claims through the context) and
        // nothing traced -- deterministically, without a pool. One flag read
        // per parallel-mark cycle.
        if zgc_dbg_parmark_fail() {
            for &r in roots {
                if r != 0 {
                    let _ = self.mark_claim(r as usize);
                }
            }
            return None;
        }
        // ONE FEWER INDIRECT HOP PER CALL -- see `zgc_mark_ctx_direct`. Falls
        // back to the wrapper for a heap with no self-`Arc` (a `with_capacity`
        // test heap), which is also what the kill switch selects.
        let ctx: std::sync::Arc<dyn mark::ZMarkContext> = match self.mark_context_arc() {
            Some(direct) => direct,
            None => std::sync::Arc::new(ZHeapMarkBridge { heap: self }),
        };
        // The pool, kept from the last collection when it can be. The context
        // is bound per cycle either way, so the two arms differ only in
        // whether N threads are spawned and joined here.
        // CLEARED ON EVERY EXIT, including a panic unwinding through. The
        // context bound below may be a `ZHeapMarkBridge`, which holds a RAW
        // POINTER to this heap and is sound only while it cannot outlive the
        // borrow it was built from. A pool dropped at the end of this function
        // gave that for free; a PERSISTENT one does not, and a missed
        // `end_cycle` would leave the pointer in a pool that survives to the
        // next cycle. A guard, not a reading of the control flow.
        struct ClearCycleContext(Option<std::sync::Arc<mark::ZMarkCoordinator>>);
        impl Drop for ClearCycleContext {
            fn drop(&mut self) {
                if let Some(pool) = self.0.take() {
                    pool.end_cycle();
                }
            }
        }
        // zr z3/mt: ONE pool lookup, and the guard armed BEFORE `push_roots`.
        // The guard used to be built after `push_roots` from a second
        // `persistent_mark_pool` call: two env reads and two `mark_pool` lock
        // round trips per cycle, and a panic inside `push_roots` (it claims
        // through the bound context) would have skipped the clear this guard
        // promises "on every exit".
        // zr z11/rv9 (mark-pool residual 2, `CRATONVM_ZGC_MARK_SPAWN_BACKOFF`,
        // default ON): after a cycle on which neither pool could be spawned,
        // the next `ZGC_MARK_SPAWN_BACKOFF_CYCLES` parallel-mark cycles go
        // straight to the serial marker instead of spawning (and joining) up
        // to N-1 threads inside each pause and logging a warning per cycle.
        // Nothing has been claimed yet, so `None` here is the plain serial
        // fallback.
        if self.mark_spawn_backoff_skips() {
            return None;
        }
        // zt t8/rl2 (`zgc-zs3ac-collection-cpu-residual-shares` item 2): the
        // pool's workers price themselves by THIS heap's accounting seed, as
        // every other share does (`collection_cpu_accounting_on`); the pool
        // read the process flag at its construction. Set before the cycle
        // opens: a worker reads it once per cycle.
        let cpu_on = self.collection_cpu_accounting_on();
        let (coordinator, persistent) = match self.persistent_mark_pool(workers) {
            Some(pool) => {
                pool.set_cpu_accounting(cpu_on);
                pool.begin_cycle_with(ctx);
                (pool, true)
            }
            None => match mark::ZMarkCoordinator::try_new(ctx, workers) {
                Ok(fresh) => {
                    let fresh = std::sync::Arc::new(fresh);
                    fresh.set_cpu_accounting(cpu_on);
                    fresh.begin_cycle();
                    (fresh, false)
                }
                // No pool at all: the single-threaded marker takes this cycle,
                // the same fallback as a refused certification.
                Err(e) => {
                    tracing::warn!(
                        target: "zgc",
                        workers,
                        error = %e,
                        "zgc mark: could not spawn mark workers; marking this cycle serially"
                    );
                    self.note_mark_spawn_failed();
                    return None;
                }
            },
        };
        let _clear_ctx = ClearCycleContext(persistent.then(|| std::sync::Arc::clone(&coordinator)));
        coordinator.push_roots(roots);

        // One restart is budgeted rather than zero, for the reason the old
        // bespoke path gave: no mutator can race us, but budgeting zero would
        // turn any flush that legitimately produced work into an "incomplete
        // mark set" verdict on a set the sweep is about to trust.
        #[allow(unused_mut)]
        let mut safepoint: std::sync::Arc<dyn crate::zgc_concurrent::ZgcMarkSafepoint> =
            std::sync::Arc::new(ZgcNoMutatorSafepoint);
        // zt t1/mk test seam: a driver that panics inside the cycle.
        #[cfg(test)]
        {
            if self
                .counters
                .test_driver_safepoint_panic
                .load(Ordering::Relaxed)
            {
                safepoint = std::sync::Arc::new(ZtPanickingDriverSafepoint);
            }
        }
        let params = ZgcConcurrentMarkParams::new(std::sync::Arc::clone(&coordinator), safepoint)
            .with_max_mark_end_restarts(Z_PARMARK_RESTART_BUDGET);

        // ON THIS THREAD (zr z1/mk): the collecting thread only ever joined the
        // driver, so the driver thread was an OS spawn + join per pause for
        // nothing. `CRATONVM_ZGC_MARK_INLINE_DRIVER=0` restores the spawn.
        // zt t8/rl2 (zs3ac items 2-3): a spawned driver is priced by this
        // heap's seed (`cpu_on`), and `driver_cpu_ns` receives its CPU even
        // when it panics, for the panic arm below.
        let mut driver_cpu_ns = 0u64;
        let driven = if zgc_mark_inline_driver() {
            ZgcConcurrentMarkController::run_inline(params)
        } else {
            // zs s5/mc3 (residual 11): a failed driver spawn drives inline
            // instead of panicking inside the pause.
            let mut spawned = false;
            ZgcConcurrentMarkController::spawn_and_join_or_inline(
                params,
                &mut spawned,
                cpu_on.then_some(&mut driver_cpu_ns),
            )
        };
        let outcome = match driven {
            Ok(o) => o,
            Err(_) => {
                tracing::error!(
                    target: "zgc",
                    "zgc mark: the concurrent-mark driver PANICKED; falling back to the single-threaded marker for this cycle"
                );
                coordinator.end_cycle();
                // zs s2/mc2: STOP AND JOIN the pool before the serial fallback
                // runs. The driver unwound with workers still inside the cycle
                // (armed, no fixed point), and a worker keeps the context it
                // took for its busy drains -- possibly a `ZHeapMarkBridge`'s
                // raw heap pointer -- so it went on claiming and tracing beside
                // the serial marker and the sweep: an object it claimed after
                // the fallback's rescan passed it had its children traced only
                // by that worker. The pool also leaves the persistent slot.
                drop(_clear_ctx);
                // zt t1/mk (rv3 item 3): this arm returned without booking
                // the failed pool's CPU (the uncertified arm below books it).
                // Read after the retire has stopped and joined the workers --
                // each charges before it parks -- as `finish_concurrent_mark`'s
                // refusal arm does: a lower bound, never a double count (a
                // retired pool is not reused).
                // zt t2/x1 (finding X1-3): the retire reads it after the join.
                // The t1 cut held a clone of the shared state across the
                // retire, whose shared-pool arm waits for exactly two holders
                // and would have spun its 2 s deadline on that third one.
                let cpu = self.retire_parallel_mark_pool(coordinator, persistent);
                // zt t8/rl2 (zs3ac item 3): under
                // `CRATONVM_ZGC_MARK_INLINE_DRIVER=0` the PANICKED driver
                // thread's own CPU too (0 on the inline arm, whose CPU is this
                // thread's pause span): it has no outcome to carry it.
                self.counters
                    .mark_cpu_stw_pool_ns
                    .fetch_add(cpu.saturating_add(driver_cpu_ns), Ordering::Relaxed);
                return None;
            }
        };
        let stats = coordinator.stats().snapshot();
        // zr zs s1/mc: the pool's CPU for this cycle (charged by each worker
        // before it parked, so complete at the fixed point).
        self.counters
            .mark_cpu_stw_pool_ns
            .fetch_add(stats.worker_cpu_ns, Ordering::Relaxed);
        // zt t1/mk (rv3 item 2): under `CRATONVM_ZGC_MARK_INLINE_DRIVER=0` the
        // driver ran on a spawned thread, which no other share covers (the
        // collecting thread's `driver_cpu_us=` only saw its `join`). It is a
        // helper thread of this pool, so it is booked with the pool's workers,
        // keeping `ZCollectionCpuTotals::collection_ns` a sum of disjoint
        // shares. Zero on the default inline arm.
        self.counters
            .mark_cpu_stw_pool_ns
            .fetch_add(outcome.driver_thread_cpu_ns, Ordering::Relaxed);
        // BEFORE `end_cycle`, and before the coordinator is dropped a few lines
        // below: this is the only point at which the terminator is still
        // reachable. See `mark_park_timeouts`.
        self.counters.mark_park_timeouts.fetch_add(
            coordinator.shared().terminator().park_timeouts() as usize,
            Ordering::Relaxed,
        );
        coordinator.end_cycle();
        self.counters
            .driver_passes
            .fetch_add(outcome.passes, Ordering::Relaxed);

        if !outcome.mark_set_complete {
            tracing::error!(
                target: "zgc",
                passes = outcome.passes,
                restarts = outcome.restarts,
                redrains = outcome.redrains,
                "zgc mark: the driver could not certify a complete mark set AT A SAFEPOINT — no mutator is running, so this cannot be a mutator race. Falling back to the single-threaded marker rather than sweeping against it"
            );
            // zs s3/rv2: STOP AND JOIN here too, as the driver-panic arm does.
            // The common way to reach this arm is a PANICKING WORKER (mc2's
            // catch_unwind sets `should_stop`, and the driver reports the
            // cycle `stopped_early`); the other workers are then still inside
            // `drain_with`, which tests only the pause flag, holding the
            // context they took for the busy stretch. Returning with them
            // running let a straggler claim an object after the serial
            // fallback's rescan passed it and drop its local stack at its next
            // `should_stop` check: the object read as marked to the serial
            // loop, so its children were swept while reachable. Uncertified
            // exits are failure paths (an error is logged above), so the
            // respawn the next cycle pays is not on any healthy run.
            drop(_clear_ctx);
            // zt t8/rl2 (`zgc-zt8rl2-relocation-footprint-cpu-residuals` item
            // 6): the snapshot above was booked at the fixed point; what the
            // stopped stragglers charged after it is the retire's same-cycle
            // total less that snapshot (`ZMarkStats` resets only at a cycle's
            // begin, so never a double count).
            let retired = self.retire_parallel_mark_pool(coordinator, persistent);
            self.counters.mark_cpu_stw_pool_ns.fetch_add(
                zgc_mark_straggler_cpu(stats.worker_cpu_ns, retired),
                Ordering::Relaxed,
            );
            return None;
        }
        tracing::debug!(
            target: "zgc",
            workers,
            passes = outcome.passes,
            restarts = outcome.restarts,
            marked = stats.objects_marked,
            "zgc mark: driven by zgc_concurrent's coordinator"
        );
        Some(stats)
        // `coordinator` drops here. The controller thread was joined by
        // `join_cycle` above, and `ZMarkCoordinator::drop` stops and JOINS
        // every worker — so no thread holding a clone of `bridge` outlives
        // this borrow of `self`.
    }

    /// zs s2/mc2 + s3/rv2: take a failed cycle's pool out of the persistent
    /// slot and STOP AND JOIN its workers, so no worker can claim or trace
    /// beside the serial fallback and the sweep that follow. The caller has
    /// already dropped its cycle-context guard (it holds a clone).
    ///
    /// When another holder keeps the pool alive (none on today's paths:
    /// `run_inline` consumes its params and `join_cycle` drops the
    /// controller's clone), the workers are told to stop and this waits, in
    /// bounded steps, for the pool's shared state to be released by every
    /// worker; it gives up after ~2 s with an error line rather than wedging
    /// the pause.
    ///
    /// Answers the retired pool's worker CPU (`ZMarkStats::worker_cpu_ns`),
    /// read after the join (each worker charges before it parks): a lower
    /// bound on the shared arm if the deadline passed. zt t2/x1: so a caller
    /// that books it need not hold a clone of the shared state across this
    /// call, which the shared arm's holder count would read as a worker.
    pub fn retire_parallel_mark_pool(
        &self,
        coordinator: std::sync::Arc<mark::ZMarkCoordinator>,
        persistent: bool,
    ) -> u64 {
        if persistent {
            let mut slot = self.counters.mark_pool.lock();
            if slot
                .as_ref()
                .is_some_and(|(_, p)| std::sync::Arc::ptr_eq(p, &coordinator))
            {
                *slot = None;
            }
        }
        match std::sync::Arc::try_unwrap(coordinator) {
            Ok(pool) => {
                let shared = std::sync::Arc::clone(pool.shared());
                pool.shutdown();
                shared.stats().worker_cpu_ns.load(Ordering::Relaxed)
            }
            Err(still_shared) => {
                still_shared.stop();
                // Each worker holds one clone of the shared state; the pool
                // holds one more. Wait for the workers' clones to go.
                let shared = std::sync::Arc::clone(still_shared.shared());
                drop(still_shared);
                let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
                // The pool's own clone (held by the other owner) plus ours.
                while std::sync::Arc::strong_count(&shared) > 2 {
                    if std::time::Instant::now() >= deadline {
                        tracing::error!(
                            target: "zgc",
                            "zgc mark: a shared failed mark pool's workers did not leave within 2 s; continuing with the serial marker"
                        );
                        break;
                    }
                    std::thread::sleep(std::time::Duration::from_millis(1));
                }
                shared.stats().worker_cpu_ns.load(Ordering::Relaxed)
            }
        }
    }

    /// The pool-only marking path this heap used between 2026-08-13 and
    /// 2026-08-14. Kept as a private helper so the driver-based path above has
    /// something to be compared against in a bench, and referenced by name in
    /// the plan's Phase 3 note; not on any live path.
    #[cfg(test)]
    pub fn mark_pool_only_stw(&self, roots: &[u64], workers: usize) -> mark::ZMarkStatsSnapshot {
        let bridge: std::sync::Arc<dyn mark::ZMarkContext> =
            std::sync::Arc::new(ZHeapMarkBridge { heap: self });
        let coordinator = mark::ZMarkCoordinator::new(bridge, workers);
        coordinator.begin_cycle();
        coordinator.push_roots(roots);
        // One restart is budgeted rather than zero. Not because a mutator can
        // race us — none is running — but because budgeting zero would turn
        // any future flush that legitimately produces work into an
        // "INCOMPLETE mark set" verdict, and an incomplete mark set is what
        // the sweep is about to act on.
        let report = coordinator.mark_to_completion(Z_PARMARK_RESTART_BUDGET);
        // Same question, same spelling as `finish_concurrent_mark`: anywhere
        // this tree asks "may I sweep against this mark set", the question is
        // `mark_set_complete()` and not `!budget_exhausted`. A pool stopped
        // mid-`mark_to_completion` reports a converged-looking cycle otherwise.
        if !report.mark_set_complete() {
            // Cannot happen with the world stopped, which is exactly why it is
            // worth saying loudly if it ever does: it would mean the mark set
            // the sweep is about to trust is incomplete.
            tracing::error!(
                target: "zgc",
                passes = report.passes,
                restarts = report.restarts,
                budget_exhausted = report.budget_exhausted,
                stopped_early = report.stopped_early,
                "zgc parallel mark: the mark set could not be certified AT A SAFEPOINT — \
                 no mutator is running, so this cannot be a mutator race; the mark \
                 set may be incomplete"
            );
        }
        coordinator.end_cycle();
        report.stats
        // `coordinator` drops here; its Drop stops and JOINS every worker, so
        // no thread holding a clone of `bridge` outlives this borrow of `self`.
    }

    /// zr z4/rv4: seed the single-threaded marker after a FAILED parallel mark.
    ///
    /// Every serial arm traces an object only when its `mark_claim` wins, and
    /// a failed driver (a panic, an uncertified mark set) leaves objects it
    /// CLAIMED but never scanned -- the roots first of all, which
    /// `push_roots` claims. Re-pushing the roots, as the fallback did, admits
    /// nothing: their bits are set, so their children were never traced and
    /// the sweep freed them. Since zr z3/mt made the parallel mark the default
    /// this was every ZGC collection's failure arm.
    ///
    /// The repair pushes the out-edges (fields, pin and overlay edges) of
    /// EVERY marked registered base, so the closure the serial loop computes
    /// is a superset of the true one: a claimed-but-unscanned object's
    /// children are admitted, and anything already scanned is idempotent. On
    /// a young cycle this also traces the pre-marked old objects, which only
    /// retains more for one cycle. O(registry), on the failure arm only.
    /// Returns the bases rescanned. `CRATONVM_ZGC_PARMARK_FALLBACK_RESCAN=0`
    /// restores the roots-only fallback.
    pub fn parmark_fallback_rescan_seeds(
        &self,
        registered: &ZObjectStartsSnapshot,
        skip_for: &dyn Fn(usize) -> Option<usize>,
        work: &mut Vec<usize>,
    ) -> usize {
        let mut rescanned = 0usize;
        registered.for_each_base(|base| {
            if !self.mark_is_set(base) {
                return;
            }
            rescanned += 1;
            let class_id = self.header_ref(base as *mut u8).class_id.as_u32();
            self.enumerate_references(base as *mut u8, work, skip_for(base));
            let (may_own_pins, may_own_overlay) = self.extra_root_probe(base);
            self.visit_pin_edges(base, class_id, may_own_pins, &mut |edge| work.push(edge));
            self.visit_overlay_edges(base, class_id, may_own_overlay, &mut |edge| work.push(edge));
        });
        rescanned
    }

    /// The single-threaded strong-closure marker of a stop-the-world cycle:
    /// mark everything reachable from `seeds` and trace it, with ALL of an
    /// object's out-edges -- its reference slots (less the `skip_for` referent
    /// hole), its loader / mirror / metadata pin edges and its native
    /// collection-overlay edges. Returns how many candidate addresses were
    /// refused as not a registered base (`wild_skipped`).
    ///
    /// `claimed`, when given, receives every object THIS call marked, in claim
    /// order (the finalizer resurrection's depth-2 set).
    ///
    /// # One closure for the three serial loops (zr z1/mk, 2026-10-12)
    ///
    /// `collect_garbage` used to carry three hand-written copies of this loop
    /// -- the main mark, the finalizer resurrection and the INT-8 soft remark --
    /// and the third had drifted: it traced only `enumerate_references`, so a
    /// soft referent the LRU policy KEPT, reachable only through its
    /// `SoftReference`, survived with its reference slots marked and its pin
    /// and overlay edges NOT. A policy-kept `SoftReference<HashMap>` whose
    /// contents live in a native collection overlay, or a soft-held instance
    /// whose class loader nothing else reaches, then had live state swept
    /// under it. One function cannot drift from itself.
    ///
    /// *Scope (zr z2/rv):* the soft remark's input is this heap's PRIVATE
    /// processor ([`ZgcCounters::ref_processor`]), which is empty in a real
    /// run -- the VM's references live in `SharedVm::mem::ref_processor` and a
    /// policy-kept soft referent's slot is simply traced as a strong edge by
    /// the main loop. So the drift was reachable from this file's own tests
    /// only, not on the shipped default path; the unification still matters for
    /// the finalizer resurrection, which IS fed in production.
    ///
    /// # Claim on push, not on pop (`CRATONVM_ZGC_MARK_CLAIM_ON_PUSH`)
    ///
    /// The old loop pushed EVERY edge and decided at pop time (registry test,
    /// mark-bit claim). Most edges of a shared graph point at something already
    /// marked, so the work stack carried every edge once, and a reference array
    /// pushed its whole length in one go -- a live `Object[100_000_000]` was an
    /// 800 MB `Vec` grown by doubling inside the pause. Claiming at push time
    /// (HotSpot's mark-on-push) means only newly marked objects enter the
    /// stack, and a reference array longer than `mark::ref_chunk_budget()` is
    /// scanned in chunks with a continuation, children first, so the stack
    /// stays bounded by `chunk x nesting depth` rather than by the array
    /// length. The mark SET is identical: the same registry screen and the
    /// same exactly-once claim run on the same candidates, only earlier.
    /// `=0` restores the pop-time loop (no chunking) byte for byte, with the
    /// full edge set kept in all three callers.
    pub fn mark_closure_serial(
        &self,
        seeds: &mut Vec<usize>,
        registered: &ZObjectStartsSnapshot,
        skip_for: &dyn Fn(usize) -> Option<usize>,
        claimed: Option<&mut Vec<usize>>,
    ) -> ZSerialMarkStats {
        // zr zs s1/mc: the call's thread CPU, beside the wall `mark_us`.
        let cpu0 = if self
            .counters
            .mark_loop
            .cpu_accounting
            .load(Ordering::Relaxed)
        {
            mark::thread_cpu_nanos()
        } else {
            None
        };
        let mut stats = self.mark_closure_serial_body(seeds, registered, skip_for, claimed);
        if let (Some(t0), Some(t1)) = (cpu0, cpu0.and_then(|_| mark::thread_cpu_nanos())) {
            stats.cpu_ns = t1.saturating_sub(t0);
            self.counters
                .mark_cpu_serial_ns
                .fetch_add(stats.cpu_ns, Ordering::Relaxed);
        }
        stats
    }

    /// The body of [`Self::mark_closure_serial`] (zr zs s1/mc split it off so
    /// the CPU reading brackets both arms).
    pub fn mark_closure_serial_body(
        &self,
        seeds: &mut Vec<usize>,
        registered: &ZObjectStartsSnapshot,
        skip_for: &dyn Fn(usize) -> Option<usize>,
        mut claimed: Option<&mut Vec<usize>>,
    ) -> ZSerialMarkStats {
        let dbg_wild = dbg_zgc_wild();
        let mut wild = 0usize;
        if !zgc_mark_claim_on_push() {
            // The pop-time loop, as it stood before zr z1/mk.
            let work = seeds;
            while let Some(addr) = work.pop() {
                if addr == 0 {
                    continue;
                }
                if !registered.contains(addr) {
                    wild += 1;
                    continue;
                }
                if !self.mark_claim(addr) {
                    continue;
                }
                if let Some(c) = claimed.as_deref_mut() {
                    c.push(addr);
                }
                let class_id = self.header_ref(addr as *mut u8).class_id.as_u32();
                self.enumerate_references(addr as *mut u8, work, skip_for(addr));
                let (may_own_pins, may_own_overlay) = self.extra_root_probe(addr);
                self.visit_pin_edges(addr, class_id, may_own_pins, &mut |edge| {
                    if dbg_wild {
                        self.dbg_wild_edge(addr, "pin", 0, edge as u64);
                    }
                    work.push(edge)
                });
                self.visit_overlay_edges(addr, class_id, may_own_overlay, &mut |edge| {
                    if dbg_wild {
                        self.dbg_wild_edge(addr, "overlay", 0, edge as u64);
                    }
                    work.push(edge)
                });
            }
            return ZSerialMarkStats {
                wild_skipped: wild,
                ..ZSerialMarkStats::default()
            };
        }

        // Claim-on-push. `stack` holds CLAIMED bases only; `partial` holds the
        // continuation `(array base, next element)` of a chunked reference
        // array. The stack is drained before a continuation is resumed, so an
        // array's children are traced before its next chunk is read.
        // zt t3/mk (X1-11): the heap's copy, read at construction; one relaxed
        // load per call where the t1 cut paid a flag lookup per call (once
        // per dead finalizable candidate in the resurrection pass).
        let chunk = self
            .counters
            .mark_loop
            .ref_chunk
            .load(Ordering::Relaxed)
            .max(1);
        let mut stack: Vec<usize> = Vec::with_capacity(seeds.len().max(64));
        let mut partial: Vec<(usize, usize)> = Vec::new();
        let mut chunks = 0usize;
        let mut objects = 0usize;
        // zr z3/mt: the loop's cost switches, read once per call (`ZMarkLoopOpts`).
        // The mark-first screen needs the bitmap arm: on the header arm the
        // "is it marked" question reads a header through an unscreened address.
        let opts = &self.counters.mark_loop;
        let mark_first_bits = if opts.test_mark_first.load(Ordering::Relaxed) {
            self.mark_bits.as_ref()
        } else {
            None
        };
        let one_decode = opts.one_decode.load(Ordering::Relaxed);
        let mut memo = opts
            .layout_memo
            .load(Ordering::Relaxed)
            .then(ZLayoutMemo::default);
        let mut mark_first_skips = 0usize;
        {
            let mut admit = |a: usize, stack: &mut Vec<usize>, wild: &mut usize| {
                if a == 0 {
                    return;
                }
                // Already marked: dropped whatever the registry says (claimed
                // before, or a non-base the claim would refuse anyway).
                if let Some(bits) = mark_first_bits {
                    if bits.contains(a) {
                        mark_first_skips += 1;
                        return;
                    }
                }
                if !registered.contains(a) {
                    *wild += 1;
                    return;
                }
                if self.mark_claim(a) {
                    if let Some(c) = claimed.as_deref_mut() {
                        c.push(a);
                    }
                    stack.push(a);
                }
            };
            for a in seeds.drain(..) {
                admit(a, &mut stack, &mut wild);
            }
            loop {
                if let Some(addr) = stack.pop() {
                    objects += 1;
                    let header = self.header_ref(addr as *mut u8);
                    let class_id = header.class_id.as_u32();
                    let kind = header.kind();
                    let ref_array_len = (kind == ObjectKind::Array
                        && header.element_type() == ArrayElementType::Reference)
                        .then(|| header.array_length() as usize);
                    // zr zs s1/mc (`CRATONVM_ZGC_MARK_ONE_DECODE`): no
                    // reference slots (primitive array, humongous filler), so
                    // no `skip_for` lookup and no strong walk -- see
                    // `ZgcRealHeap::visit_refs_chunked`. The pin / overlay
                    // edges below still run, per object.
                    let no_slots =
                        one_decode && kind != ObjectKind::Object && ref_array_len.is_none();
                    match ref_array_len {
                        _ if no_slots => {}
                        Some(len) if len > chunk => {
                            self.visit_array_ref_slots(addr, 0, chunk, &mut |raw| {
                                admit(raw as usize, &mut stack, &mut wild)
                            });
                            partial.push((addr, chunk));
                        }
                        _ => match memo.as_mut() {
                            Some(memo) => self.visit_strong_refs_memo(
                                addr as *mut u8,
                                skip_for(addr),
                                memo,
                                &mut |raw| admit(raw as usize, &mut stack, &mut wild),
                            ),
                            None => self.visit_strong_refs_at(
                                addr as *mut u8,
                                skip_for(addr),
                                &mut |raw| admit(raw as usize, &mut stack, &mut wild),
                            ),
                        },
                    }
                    let (may_own_pins, may_own_overlay) = self.extra_root_probe(addr);
                    self.visit_pin_edges(addr, class_id, may_own_pins, &mut |edge| {
                        if dbg_wild {
                            self.dbg_wild_edge(addr, "pin", 0, edge as u64);
                        }
                        admit(edge, &mut stack, &mut wild)
                    });
                    self.visit_overlay_edges(addr, class_id, may_own_overlay, &mut |edge| {
                        if dbg_wild {
                            self.dbg_wild_edge(addr, "overlay", 0, edge as u64);
                        }
                        admit(edge, &mut stack, &mut wild)
                    });
                    continue;
                }
                let Some((base, cursor)) = partial.pop() else {
                    break;
                };
                // Re-read the length rather than carrying it: the header is
                // immutable during a stop-the-world mark, and this keeps the
                // continuation two words.
                let len = self.header_ref(base as *mut u8).array_length() as usize;
                let end = cursor.saturating_add(chunk).min(len);
                if cursor < end {
                    self.visit_array_ref_slots(base, cursor, end, &mut |raw| {
                        admit(raw as usize, &mut stack, &mut wild)
                    });
                }
                chunks += 1;
                if end < len {
                    partial.push((base, end));
                }
            }
        }
        let (memo_hits, memo_misses) = memo.map_or((0, 0), |m| (m.hits, m.misses));
        ZSerialMarkStats {
            wild_skipped: wild,
            objects,
            chunks,
            // CAPACITY, not length: the stack is empty here, and its capacity
            // is what it grew to -- the allocation the pause paid for.
            stack_peak: stack.capacity(),
            memo_hits,
            memo_misses,
            mark_first_skips,
            // Filled by the `mark_closure_serial` wrapper.
            cpu_ns: 0,
        }
    }

    /// [`Self::visit_strong_refs_at`] for the serial marker, with a one-entry
    /// oop-map memo (zr z3/mt, `CRATONVM_ZGC_MARK_LAYOUT_MEMO`).
    ///
    /// Takes the memo arm only for a compact OBJECT with no referent hole and
    /// no `CRATONVM_DBG_ZGC_WILD` (which names field indices); everything else
    /// -- arrays, legacy cells, `Reference` objects, a layout that cannot be
    /// resolved (whose loud warning stays in one place) -- is the shared walk,
    /// byte for byte. The memo holds a COPY of the class's `ref_disps`, the
    /// same oop-map `visit_strong_refs_at` walks, so the edge set is identical.
    /// Layouts cannot change under a stop-the-world mark, and the memo lives
    /// for one `mark_closure_serial` call.
    ///
    /// Generic over the visitor (zs s2/mc2), so the serial loop's `admit`
    /// inlines into the memo's slot loop; the fallbacks coerce it to the
    /// shared walk's `&mut dyn FnMut`.
    pub fn visit_strong_refs_memo<F: FnMut(u64)>(
        &self,
        base: *mut u8,
        skip_index: Option<usize>,
        memo: &mut ZLayoutMemo,
        f: &mut F,
    ) {
        let header = self.header_ref(base);
        if skip_index.is_some()
            || header.kind() != ObjectKind::Object
            || !cratonvm_types::is_compact_object(header)
            || dbg_zgc_wild()
        {
            return self.visit_strong_refs_at(base, skip_index, f);
        }
        let key = (header.class_id.as_u32(), header.num_slots());
        if memo.key == Some(key) {
            memo.hits += 1;
        } else {
            memo.key = None;
            let ZLayoutMemo { ref_disps, .. } = memo;
            let resolved = cratonvm_types::with_class_layout(key.0, key.1, |layout| {
                ref_disps.clear();
                ref_disps.extend_from_slice(&layout.ref_disps);
            })
            .is_some();
            if !resolved {
                return self.visit_strong_refs_at(base, skip_index, f);
            }
            memo.key = Some(key);
            memo.misses += 1;
        }
        for &offset in &memo.ref_disps {
            // SAFETY: `offset` is from this object's own registered layout
            // (the memo key is its `(class_id, num_slots)`), so the slot lies
            // inside the allocation; see `visit_strong_refs_at`.
            let slot = unsafe { base.add(offset as usize) };
            let raw = unsafe { cratonvm_types::narrow_oop::read_ref_slot(slot) };
            if raw != 0 {
                f(self.mark_slot_word(slot, raw));
            }
        }
    }

    /// zr z9/sl (`CRATONVM_ZGC_SELECTIVE_RELOCATE`): per logical page of the
    /// low region, the free bytes that lie in FRAGMENTED holes -- gaps between
    /// consecutive survivors (and at the region's two ends) shorter than
    /// `hole_min`. A gap at least `hole_min` long is free space the
    /// allocator's contiguous requests (TLAB chunks, large objects) can already
    /// use, so a slide gains nothing by closing it; the rest is what a slide
    /// turns into contiguity. A gap that crosses a page boundary is split
    /// between the pages it covers.
    ///
    /// `live` is the slide's survivor list (ascending on the production
    /// caller) and `sizes` its size table (`logical_pages`' cells; empty on the
    /// table's kill arm, where the header is read). An out-of-order list only
    /// makes the answer approximate: this is a selection input, never a
    /// safety argument -- the slide's own screens still check every move.
    ///
    /// One read of `live` and the size cell per survivor, split across
    /// `workers` like the other whole-live-set passes (each chunk reads its
    /// predecessor from the shared slice, so chunk edges need no fix-up).
    pub fn fragmented_garbage_per_page(
        &self,
        live: &[usize],
        sizes: &[u32],
        base: usize,
        low_end: usize,
        hole_min: usize,
        workers: usize,
    ) -> Vec<usize> {
        let pages = (low_end.saturating_sub(base)).div_ceil(Self::Z_LOGICAL_PAGE_BYTES);
        let end_of = |i: usize| -> usize {
            let b = live[i];
            let cell = sizes.get(i).copied().unwrap_or(0);
            b.saturating_add(self.size_via_cell(cell, b).unwrap_or(0))
        };
        let parts = self.reloc_par_chunks(live, workers, |chunk: &[usize]| {
            let mut part = vec![0usize; pages.max(1)];
            let off = zgc_chunk_offset(live, chunk);
            for (j, &b) in chunk.iter().enumerate() {
                if b < base || b >= low_end {
                    continue;
                }
                let i = off + j;
                let prev_end = if i == 0 || live[i - 1] < base {
                    base
                } else {
                    end_of(i - 1)
                };
                add_fragmented_gap(&mut part, base, prev_end, b, hole_min);
            }
            part
        });
        let mut frag = vec![0usize; pages.max(1)];
        for part in parts {
            for (sum, add) in frag.iter_mut().zip(part) {
                *sum += add;
            }
        }
        // The gap above the highest survivor, up to the bump extent.
        let last = live.iter().rposition(|&b| b >= base && b < low_end);
        let tail_from = last.map_or(base, end_of);
        add_fragmented_gap(&mut frag, base, tail_from, low_end, hole_min);
        frag
    }

    /// Logical page size for the relocation-set view over the arena.
    ///
    /// The arena is one flat span with no pages, and `zgc::page`'s allocator
    /// is not adopted -- replacing `Arena` with it is a rewrite of the
    /// allocation path, not an increment. But `forwarding`'s relocation-set
    /// SELECTOR only needs pages as an accounting unit: a page id, the live
    /// bytes on it, and its allocated extent. Imposing a logical grid over the
    /// arena gives it all three, which is what turns compaction from "slide
    /// the whole region" into "evacuate the pages whose garbage pays for the
    /// copy".
    ///
    /// 2 MiB matches `ZPageConfig`'s Small page size, so the accounting unit
    /// is the one the rest of the ZGC modules already reason in, and it is
    /// four times `ZGC_TLAB_MAX_CHUNK` -- big enough that a page is not a
    /// single thread's chunk, small enough to be a useful granularity on the
    /// 64 MiB default heap.
    pub const Z_LOGICAL_PAGE_BYTES: usize = 2 * 1024 * 1024;

    /// Granularity of the young-region set -- see [`Self::note_young_page`].
    ///
    /// Deliberately FINER than [`Self::Z_LOGICAL_PAGE_BYTES`], and deliberately
    /// coarser than a snapshot word (512 arena bytes) so the sweep can ask
    /// "young?" once per word rather than once per object. Both halves are
    /// load-bearing; the doc on `note_young_page` carries the measurement that
    /// says why the logical page was too coarse.
    pub const Z_YOUNG_GRAIN_BYTES: usize = 64 * 1024;

    /// The logical grid as real [`page::ZPageReal`] views, with per-page
    /// `used` and `live_bytes` filled in from the live set.
    ///
    /// Views, not pages: they allocate nothing and free nothing -- see
    /// `ZPageReal::view`. They exist so the page-keyed consumers in this crate
    /// (`forwarding`'s selector, `generation`'s scope, `remembered`'s table)
    /// can be driven against this arena-backed heap without the arena being
    /// replaced by the page allocator first.
    ///
    /// zs s1/sl (`CRATONVM_ZGC_SELECTIVE_FUSED_FRAG`): with `frag_hole_min`
    /// set, the same pass also books every survivor-to-survivor gap shorter
    /// than it into a per-page vector (the selective arm's fragmented
    /// garbage, [`Self::fragmented_garbage_per_page`]'s answer on an
    /// ascending list), returned as the second value. Each chunk books its
    /// inner gaps and reports its first start and last end; the gaps at the
    /// chunk seams, at the region base and above the highest survivor are
    /// booked once, in order, after the merge. No header is read for it: the
    /// size the pass already took for `live_bytes` is the end of the object.
    #[allow(clippy::too_many_arguments)]
    pub fn logical_pages(
        &self,
        live: &[usize],
        base: usize,
        low_end: usize,
        workers: usize,
        sizes: Option<&mut Vec<u32>>,
        hints: &ZSweepSizeHints,
        frag_hole_min: Option<usize>,
    ) -> (Vec<std::sync::Arc<page::ZPageReal>>, Option<Vec<usize>>) {
        let pages = (low_end - base).div_ceil(Self::Z_LOGICAL_PAGE_BYTES);
        let want_cells = sizes.is_some();
        // zr z7/pc: the sweep's hints are only consulted beside the table (the
        // kill of the parent switch turns both off), and must be ascending
        // and paired, or they are ignored outright.
        let hints = if want_cells && hints.is_usable() {
            Some(hints)
        } else {
            None
        };
        // One header read per live object: split across `workers` (round 9
        // wave 7, `zgc7`), each chunk summing into its own per-page vector,
        // then added up. Addition commutes, so the totals are the serial ones.
        //
        // zr z7/pc (`CRATONVM_ZGC_RELOCATE_SIZE_TABLE`): the same read also
        // fills one `size_cell` per live object (`0` outside the low region),
        // so the slide's later passes do not read the header again. The chunks
        // tile `live` in order, so concatenating them aligns the table with it.
        let partials = self.reloc_par_chunks(live, workers, |chunk: &[usize]| {
            let mut part = vec![0usize; pages.max(1)];
            let mut cells: Vec<u32> = if want_cells {
                Vec::with_capacity(chunk.len())
            } else {
                Vec::new()
            };
            // A merge walk over the hints: both lists ascend, so one cursor
            // per chunk, started by binary search, visits each hint once.
            let mut h = hints.map_or(0, |hs| {
                chunk
                    .first()
                    .map_or(0, |first| hs.addrs.partition_point(|a| a < first))
            });
            let mut hinted = 0usize;
            // zs s1/sl: the fused fragmented-gap census (see the doc above).
            let mut fpart: Vec<usize> = if frag_hole_min.is_some() {
                vec![0usize; pages.max(1)]
            } else {
                Vec::new()
            };
            let mut first_start: Option<usize> = None;
            let mut prev_end: Option<usize> = None;
            for addr in chunk {
                if *addr < base || *addr >= low_end {
                    if want_cells {
                        cells.push(0);
                    }
                    continue;
                }
                let idx = (*addr - base) / Self::Z_LOGICAL_PAGE_BYTES;
                let mut hinted_size: Option<usize> = None;
                if let Some(hs) = hints {
                    while h < hs.addrs.len() && hs.addrs[h] < *addr {
                        h += 1;
                    }
                    if h < hs.addrs.len() && hs.addrs[h] == *addr {
                        let cell = hs.cells[h];
                        h += 1;
                        hinted += 1;
                        cells.push(cell);
                        hinted_size = Some((cell & !ZSIZE_CELL_NOREF) as usize);
                    }
                }
                let sz = match hinted_size {
                    Some(sz) => sz,
                    None => {
                        let header = self.header_ref(*addr as *mut u8);
                        if want_cells {
                            let cell = Self::size_cell(header);
                            cells.push(cell);
                            if cell != 0 {
                                (cell & !ZSIZE_CELL_NOREF) as usize
                            } else {
                                self.survivor_size(*addr, header).unwrap_or(0)
                            }
                        } else {
                            self.survivor_size(*addr, header).unwrap_or(0)
                        }
                    }
                };
                part[idx] += sz;
                if let Some(hole_min) = frag_hole_min {
                    match prev_end {
                        Some(pe) => add_fragmented_gap(&mut fpart, base, pe, *addr, hole_min),
                        None => first_start = Some(*addr),
                    }
                    prev_end = Some(addr.saturating_add(sz));
                }
            }
            (part, cells, hinted, fpart, first_start, prev_end)
        });
        let mut live_bytes = vec![0usize; pages.max(1)];
        let mut table = sizes;
        let mut hinted_total = 0usize;
        if let Some(t) = table.as_mut() {
            t.clear();
            t.reserve(live.len());
        }
        let mut frag: Option<Vec<usize>> = frag_hole_min.map(|_| vec![0usize; pages.max(1)]);
        // The end of the last survivor booked so far, across chunks: the
        // seam gaps are the ones no chunk could see.
        let mut carry_end = base;
        for (part, cells, hinted, fpart, first_start, last_end) in partials {
            hinted_total += hinted;
            for (sum, add) in live_bytes.iter_mut().zip(part) {
                *sum += add;
            }
            if let Some(t) = table.as_mut() {
                t.extend_from_slice(&cells);
            }
            if let (Some(f), Some(hole_min)) = (frag.as_mut(), frag_hole_min) {
                for (sum, add) in f.iter_mut().zip(fpart) {
                    *sum += add;
                }
                if let (Some(fs), Some(le)) = (first_start, last_end) {
                    add_fragmented_gap(f, base, carry_end, fs, hole_min);
                    carry_end = le;
                }
            }
        }
        if let (Some(f), Some(hole_min)) = (frag.as_mut(), frag_hole_min) {
            // The gap above the highest survivor, up to the bump extent.
            add_fragmented_gap(f, base, carry_end, low_end, hole_min);
        }
        debug_assert!(table.is_none_or(|t| t.len() == live.len()));
        self.counters
            .sweep_size_hint_hits
            .fetch_add(hinted_total, Ordering::Relaxed);
        // NO `page.set_age(..)` HERE.
        //
        // Until 2026-09-03 this took `counters.page_ages.lock()` and stamped
        // each view's age from it. Nothing downstream read that word: the only
        // consumers of these views are `adapters::page_candidates` -- whose
        // `forwarding::PageCandidate` carries `page_id`, `live_bytes`,
        // `capacity_bytes` and `size_class_index`, and no age -- and the
        // young/old split, which reads `counters.page_ages` DIRECTLY in
        // `age_pages_and_split`. (`relocate.rs`'s `gen_hint = page.age()` reads
        // pages out of `ZPageAllocator`, not these views.) So the stamp bought
        // a mutex acquire per cycle and a number that could only ever mislead
        // a reader into thinking a view's age meant something.
        let views: Vec<std::sync::Arc<page::ZPageReal>> = (0..pages)
            .map(|i| {
                let page_base = base + i * Self::Z_LOGICAL_PAGE_BYTES;
                let span = Self::Z_LOGICAL_PAGE_BYTES;
                // `used` is the arena's bump extent inside this cell, which is
                // what `walk_bounds` must report and what the selector's
                // "capacity" means -- bytes never allocated cost nothing to
                // reclaim.
                let used = low_end.saturating_sub(page_base).min(span);
                let p = page::ZPageReal::view(
                    i as u64,
                    page::ZPageSizeClass::Small,
                    page_base,
                    span,
                    used,
                    live_bytes[i],
                );
                std::sync::Arc::new(p)
            })
            .collect();
        // ---- STAGE 0: HAND THE GRID TO THE SURVEY ------------------------
        //
        // One write lock and `heap_bytes / 2 MiB` pointer stores — 1,024 of
        // them at `-Xmx2g` — inside a pause that is hundreds of milliseconds.
        // Off by default; `None` here is one branch.
        //
        // `adopt_views` refuses storage pages and out-of-range views
        // individually and counts them, so a grid built against a different
        // base shows up as `views_rejected` rather than as a survey quietly
        // describing the wrong heap. The reading that matters is
        // `grid_refreshes == collections` and `views_installed == pages`;
        // anything else means the survey and the collector disagree about the
        // arena. See `[GC] zgc-page-survey:`.
        if let Some(survey) = self.page_survey.as_ref() {
            survey.adopt_views(&views);
        }
        (views, frag)
    }

    /// The page survey's engagement counters, or `None` when
    /// `CRATONVM_ZGC_PAGE_SURVEY` is off.
    ///
    /// **Zero `grid_refreshes` with the survey present is a wiring defect**,
    /// not an observation about the heap: it means `logical_pages` never ran
    /// or never reached `adopt_views`. `views_rejected > 0` is the other one —
    /// the grid was built against a different base than the survey was. See
    /// `handoff-s-page-survey-stage-0.md`.
    pub fn page_survey_stats(&self) -> Option<page::ZPageSurveyStats> {
        self.page_survey.as_ref().map(|s| s.survey_stats())
    }

    /// Did the arena's base land on a granule boundary?
    ///
    /// `None` when there is no survey. Stage 0 records this rather than
    /// enforcing it; stage 1 needs it to be `true`, because a real page must
    /// start on a granule.
    pub fn page_survey_base_aligned(&self) -> Option<bool> {
        self.page_survey
            .as_ref()
            .map(|s| s.base_is_granule_aligned())
    }

    /// Age every logical page by one cycle and return the young/old split
    /// under `policy`.
    ///
    /// This is `zgc::generation`'s promotion rule applied to the grid: a page
    /// whose age after this cycle reaches the policy's promotion age is old,
    /// and everything else is young.
    pub fn age_pages_and_split(
        &self,
        page_count: usize,
        policy: &generation::ZPromotionPolicy,
    ) -> (Vec<u64>, Vec<u64>) {
        let mut ages = self.counters.page_ages.lock();
        if ages.len() < page_count {
            ages.resize(page_count, 0);
        }
        let mut young = Vec::new();
        let mut old = Vec::new();
        for (i, age) in ages.iter_mut().enumerate().take(page_count) {
            *age = age.saturating_add(1);
            if policy.should_promote(*age) {
                old.push(i as u64);
            } else {
                young.push(i as u64);
            }
        }
        (young, old)
    }

    /// Record an old-to-young edge for a young-scoped cycle -- `zgc::remembered`.
    ///
    /// `slot_page` must already be known old and the value it now holds young;
    /// this only writes the bit.
    ///
    /// **Test-only since 2026-09-21 (wave 4, lane P), and the doc that said
    /// "called from the store barrier" was already wrong when it was written.**
    /// The live store barrier is `card_object`, which sets the bit on the
    /// `Arc<ZRememberedSet>` that `register_old_page_with_base_in` just handed
    /// it — one lookup, not two; the whole point of the 2026-09-04 change
    /// recorded there. This wrapper's only other caller was
    /// `young_extra_roots`'s re-card, which now retains through
    /// `iterate_snapshot_retaining` on the set itself. What is left is the A/B
    /// arm `card_object_two_lookups` needs, so it is scoped to that rather than
    /// left looking like a live path.
    #[cfg(test)]
    pub fn remember_old_to_young(&self, slot_page: u64, page_relative_offset: usize) {
        let _ = self.remembered.remember(slot_page, page_relative_offset);
    }

    /// The remembered set's extra roots for a young cycle: every old-page slot
    /// recorded as pointing into young.
    ///
    /// Without these a young collection is simply wrong -- an object reachable
    /// only from an old-generation field has no path from the thread roots and
    /// would be swept while live. That is the entire justification for the
    /// store barrier's existence, so it is asserted rather than assumed by
    /// `a_young_scope_treats_remembered_old_slots_as_roots`.
    pub fn remembered_roots(&self, base: usize) -> Vec<usize> {
        let mut roots = Vec::new();
        for set in self.remembered.snapshot() {
            let page_base = base + set.page_id() as usize * Self::Z_LOGICAL_PAGE_BYTES;
            let mut carded: Vec<usize> = Vec::new();
            set.iterate(|offset| carded.push(page_base + offset));
            for obj in carded {
                // A card names an OBJECT to re-scan. Re-check the registry:
                // the carded object may have died since the card was written,
                // and a card is allowed to be stale -- that is the price of
                // recording eagerly on the store path.
                if !self.registry.contains(obj) {
                    continue;
                }
                use census::ZCensusHeapView;
                self.reference_slots(obj as u64, &mut |slot| {
                    let target = slot.raw_word as usize;
                    if target != 0 && self.registry.contains(target) {
                        roots.push(target);
                    }
                });
            }
        }
        roots
    }

    // =====================================================================
    // PHASE G -- A YOUNG-ONLY COLLECTION
    // =====================================================================
    //
    // # A young cycle is the same collection with the old generation pre-marked
    //
    // The obvious shape is a second `collect_garbage` scoped to young objects,
    // which is what `zgc::generation`'s `collect_young` does over the page
    // allocator. It is the wrong shape for THIS heap: `collect_garbage` also
    // runs a finalizer resurrection pass, the reference processor, a
    // soft-referent remark, two censuses and a sweep that coalesces the arena,
    // and a second copy of all of it would be a second place for the old code
    // and the new to disagree about what "live" means.
    //
    // So a young cycle inverts one arm of a pass that already runs. The mark-bit
    // clear SETS the bit on every OLD object instead of clearing it, and every
    // phase downstream is then correct with no change at all:
    //
    //   * the mark loop's `already visited` test skips old objects, so their
    //     fields are never enumerated -- that is the entire saving;
    //   * `process_references` asks `is_marked_addr`, so an old referent reads
    //     as live and is not cleared;
    //   * the finalizer pass sees an old finalizable object as a survivor, so it
    //     waits for a major instead of being finalized early;
    //   * the sweep sees old objects as survivors, clears the bit, retains them.
    //
    // What has to be written is only what that inversion cannot express: the
    // roots the old generation contributes ([`Self::young_extra_roots`]), the
    // aging and promotion the sweep performs, and the card the promotion has to
    // leave behind.
    //
    // # Why the split is per OBJECT and the page grid is not used
    //
    // `page_ages` + `age_pages_and_split` age the logical grid, and that cannot
    // be the generational split here. A page's age only rises; the bump cursor
    // sits inside a page that has usually already aged past the promotion age;
    // and in steady state the allocator serves most requests from free-list
    // holes scattered over every page. A freshly allocated object -- precisely
    // what a young cycle exists to collect -- would therefore be born into an
    // old page and read as old, and the phase would reclaim nothing while still
    // paying for the barrier. `ObjectHeader::gc_age` has none of that: it is 0
    // at allocation wherever the bytes came from, and it is the same field
    // `gen_heap` and G1 already promote on.
    //
    // The page-keyed remembered set stays, because a card table is exactly what
    // it is: the page id is an index, not a claim about the generation of
    // everything on that page.

    /// Is the object at `addr` young -- has it survived fewer than
    /// `promotion_age` collections?
    #[inline]
    /// Mark the logical page holding `addr` as one a young cycle must sweep.
    ///
    /// # The hole this closes
    ///
    /// [`Self::gen_young_floor`] is the arena's low cursor at the last
    /// whole-heap collection, and a young cycle sweeps `[floor, cursor)`. That
    /// is the only way to BOUND the sweep when the registry is a bitmap over
    /// one contiguous arena -- but the free list hands out space BELOW the
    /// floor, so an object allocated into a hole a previous sweep left is born
    /// into the old region and no young cycle will ever reclaim it. The field's
    /// own doc calls it over-retention and it is; the effect is that on a
    /// steady-state workload -- where most allocation comes from free-list
    /// holes rather than the bump -- the young generation reclaims almost
    /// nothing and the `minors_per_major` budget is spent on cycles that saved
    /// nothing.
    ///
    /// Fixing it needs the young set to be a property of the SPACE rather than
    /// an address comparison.
    ///
    /// # Why the grain is 64 KiB and not the 2 MiB logical page
    ///
    /// The logical page is the unit `card_object`, `logical_pages` and
    /// `generation::ZPromotionPolicy` are keyed by, and it was the obvious
    /// choice. It is too coarse: a page that holds one old object and one fresh
    /// one is swept whole, and on any heap whose old generation is smaller than
    /// a page that is EVERY page -- the union degenerates to "sweep
    /// everything" and the nursery saves nothing.
    /// `a_young_cycle_sweeps_only_the_nursery_and_still_reports_the_live_set`
    /// caught exactly that, at `skipped=0`.
    ///
    /// [`Self::Z_YOUNG_GRAIN_BYTES`] is 64 KiB: fine enough that an old region
    /// and the allocations that follow it rarely share one, and still one bit
    /// per 64 KiB -- 8 KiB of bitmap for a 4 GiB heap. It also stays coarser
    /// than a snapshot word (512 arena bytes), which is what lets the sweep ask
    /// the question once per word instead of once per object.
    ///
    /// So the young region is the UNION of two things: everything at or above
    /// the floor (contiguous, free, and what a bump-dominated workload
    /// produces) and the pages below it that have received a fresh allocation
    /// since the last major. On a bump-dominated run the second set is empty
    /// and this costs a branch; on a free-list-dominated one it is the whole of
    /// what the floor was missing.
    ///
    /// # Cost, and where it is paid
    ///
    /// One `fetch_or` on a word covering 64 grains -- 4 MiB of arena -- so
    /// adjacent allocations share it and the line stays hot. It is skipped
    /// entirely when generational mode is off (one relaxed load), and the
    /// TLAB path pays it once per CHUNK rather than once per object, because a
    /// chunk is contiguous and its pages are known at refill.
    ///
    /// # Test before the `fetch_or` (round 9 wave 4, `gc4`)
    ///
    /// The per-object callers (`note_tlab_object`, the VM-TLAB announce on
    /// every JIT inline `new`) almost always land in a grain the chunk's carve
    /// already marked, so the `fetch_or` was a locked RMW that changed nothing,
    /// once per object, on a word shared by every thread allocating within the
    /// same 4 MiB. A plain load that finds the bit set now returns instead.
    /// That is exactly as strong: a `fetch_or` of a bit that is already set
    /// linearises at the same point as the load that saw it set, whichever
    /// side of a concurrent `clear_young_pages` that falls, and a no-op RMW
    /// publishes nothing a reader relies on (the young sweep reads the set at
    /// a safepoint). A clear bit still takes the `fetch_or`.
    #[inline]
    pub fn note_young_page(&self, addr: usize) {
        if !self.generational_enabled.load(Ordering::Relaxed) {
            return;
        }
        let base = self.arena_base;
        if base != 0 && addr >= base {
            let pg = (addr - base) / Self::Z_YOUNG_GRAIN_BYTES;
            if let Some(word) = self.young_pages.get(pg >> 6) {
                if word.load(Ordering::Relaxed) & (1u64 << (pg & 63)) != 0 {
                    return;
                }
            }
        }
        self.mark_young_pages(addr, addr + 1);
    }

    /// [`Self::note_young_page`] for a whole span. Used at TLAB refill, where
    /// the chunk's extent is known and every object it will serve is inside it.
    pub fn mark_young_pages(&self, lo: usize, hi: usize) {
        let base = self.arena_base;
        if base == 0 || hi <= lo || lo < base {
            return;
        }
        let first = (lo - base) / Self::Z_YOUNG_GRAIN_BYTES;
        let last = (hi - 1 - base) / Self::Z_YOUNG_GRAIN_BYTES;
        for pg in first..=last {
            let Some(word) = self.young_pages.get(pg >> 6) else {
                // Off the grid entirely. The heap cannot serve such an address,
                // so failing to record it cannot lose an object -- but a young
                // sweep that never visits it would, which is why
                // `young_page_grid_overflow` exists and why a non-zero there
                // forces every cycle whole-heap.
                self.young_page_grid_overflow.store(true, Ordering::Relaxed);
                return;
            };
            word.fetch_or(1u64 << (pg & 63), Ordering::Release);
        }
    }

    /// Is the page holding arena offset `off` in the young set?
    #[inline]
    pub fn young_page_at_offset(&self, off: usize) -> bool {
        let pg = off / Self::Z_YOUNG_GRAIN_BYTES;
        self.young_pages
            .get(pg >> 6)
            .is_some_and(|w| w.load(Ordering::Acquire) & (1u64 << (pg & 63)) != 0)
    }

    /// Forget every young page. Called by a whole-heap collection, which has
    /// just examined all of them.
    pub fn clear_young_pages(&self) {
        for w in self.young_pages.iter() {
            w.store(0, Ordering::Relaxed);
        }
        self.young_page_grid_overflow
            .store(false, Ordering::Relaxed);
        std::sync::atomic::fence(Ordering::Release);
    }

    /// Pages below the floor that a young cycle must still sweep, and whether
    /// the set is trustworthy.
    ///
    /// `false` means an allocation landed off the page grid, so the set is
    /// incomplete and the caller must treat the cycle as whole-heap. Fail
    /// closed: an incomplete young set retains garbage forever rather than
    /// merely under-collecting one cycle.
    pub fn young_pages_are_complete(&self) -> bool {
        !self.young_page_grid_overflow.load(Ordering::Relaxed)
    }

    /// How many 64 KiB grains below the floor the young set currently names.
    ///
    /// The engagement counter for this fix. Zero on a bump-dominated workload
    /// (the floor already covers everything, which is the case the floor was
    /// designed for and is right about); above zero is the free-list-served
    /// allocation the floor could not see.
    pub fn young_pages_below_floor(&self) -> usize {
        let floor = self.gen_young_floor.load(Ordering::Relaxed);
        if floor <= self.arena_base {
            return 0;
        }
        let limit = (floor - self.arena_base) / Self::Z_YOUNG_GRAIN_BYTES;
        (0..limit)
            .filter(|pg| {
                self.young_pages
                    .get(pg >> 6)
                    .is_some_and(|w| w.load(Ordering::Relaxed) & (1u64 << (pg & 63)) != 0)
            })
            .count()
    }

    pub fn addr_is_young(&self, addr: usize, promotion_age: u8) -> bool {
        self.header_ref(addr as *mut u8).gc_age() < promotion_age
    }

    /// The young roots one remembered card names: every reference slot of the old
    /// object at `addr` that holds a young registered object. `true` keeps the card.
    pub fn card_young_roots(&self, addr: u64, promotion_age: u8, roots: &mut Vec<usize>) -> bool {
        use census::ZCensusHeapView;
        let mut still_needed = false;
        self.reference_slots(addr, &mut |slot| {
            let target = slot.raw_word as usize;
            if target == 0 || !self.registry.contains(target) {
                return;
            }
            if self.addr_is_young(target, promotion_age) {
                roots.push(target);
                still_needed = true;
            }
        });
        still_needed
    }

    /// [`Self::young_extra_roots`] section (1) over `workers` threads, for a heap
    /// with no element cards. Returns the pages without a base, like the serial walk.
    pub fn remset_roots_sharded(
        &self,
        promotion_age: u8,
        workers: usize,
        roots: &mut Vec<usize>,
    ) -> usize {
        use cratonvm_types::narrow_oop::read_ref_slot;
        let sets = self.remembered.snapshot();
        let idx: Vec<usize> = (0..sets.len()).collect();
        let parts = zgc_par_chunks_with(&idx, workers, ZGC_PAR_CHUNKS_PER_WORKER, None, |chunk| {
            let (mut local, mut baseless, mut big) = (Vec::new(), 0usize, Vec::new());
            for &i in chunk {
                let set = &sets[i];
                if set.base_address().is_none() {
                    baseless += 1;
                    continue;
                }
                set.iterate_snapshot_retaining(|offset| match set.slot_address(offset) {
                    Some(a) if self.registry.contains(a as usize) => {
                        // A big reference array is ONE card but thousands of slots (a
                        // 66k-element array took 5 ms on one thread): scan it below,
                        // split by element range.
                        if let Some(len) = self.large_reference_array_len(a as usize) {
                            big.push((a as usize, len));
                            return false;
                        }
                        self.card_young_roots(a, promotion_age, &mut local)
                    }
                    _ => false,
                });
            }
            (local, baseless, big)
        });
        let (mut baseless, mut big) = (0, Vec::new());
        for (mut local, b, mut bg) in parts {
            roots.append(&mut local);
            baseless += b;
            big.append(&mut bg);
        }
        // Element ranges of every deferred array, one work item each.
        const RANGE: usize = 4096;
        let items: Vec<usize> = (0..big.len())
            .flat_map(|k| (0..big[k].1.div_ceil(RANGE)).map(move |r| (k << 32) | r))
            .collect();
        let stride = cratonvm_types::ref_element_size();
        let ranges = zgc_par_chunks_with(&items, workers, ZGC_PAR_CHUNKS_PER_WORKER, None, |chunk| {
            let mut local = Vec::new();
            let mut young_in = Vec::new();
            for &it in chunk {
                let (k, r) = (it >> 32, it & 0xFFFF_FFFF);
                let (obj, len) = big[k];
                let data = obj + ARRAY_DATA_OFFSET;
                let mut any = false;
                for i in r * RANGE..((r + 1) * RANGE).min(len) {
                    // SAFETY: `i < len` of a registered reference array whose
                    // length passed `large_reference_array_len`'s screen.
                    let target =
                        unsafe { read_ref_slot((data + i * stride) as *mut u8) } as usize;
                    if target != 0
                        && self.registry.contains(target)
                        && self.addr_is_young(target, promotion_age)
                    {
                        local.push(target);
                        any = true;
                    }
                }
                if any {
                    young_in.push(k);
                }
            }
            (local, young_in)
        });
        // Re-card an array whose slots still hold a young object, as the
        // serial walk keeps its card; into the CURRENT buffer, after the walk.
        let mut keep: Vec<usize> = Vec::new();
        for (mut local, young_in) in ranges {
            roots.append(&mut local);
            keep.extend(young_in);
        }
        keep.sort_unstable();
        keep.dedup();
        for k in keep {
            self.card_object(big[k].0);
        }
        if baseless > 0 {
            tracing::warn!(
                target: "zgc::remembered",
                pages = baseless,
                "remembered-set scan skipped pages with no base address"
            );
        }
        baseless
    }

    /// The roots a young cycle takes from the old generation, and the card
    /// cleaning that keeps the set from growing without bound.
    ///
    /// Without these a young collection is simply WRONG: an object reachable
    /// only from an old-generation field has no path from the thread roots and
    /// would be swept while live. The reference stays in place and the memory is
    /// zeroed, so the failure surfaces later as a field that reads null or a
    /// header with `num_slots=0` -- not as a wild pointer at the point of the
    /// bug.
    ///
    /// # Why the cards are cleaned rather than accumulated
    ///
    /// A card that is never cleared is correct and unbounded: every old object
    /// ever written stays in the set forever, so this scan converges on
    /// re-enumerating the whole old generation and the phase stops saving
    /// anything. The fix is why [`remembered::ZRememberedSet`] has two buffers:
    /// `swap_all` moves the current bits aside and clears them, the scan runs off
    /// the snapshot, and a card is **re-dirtied only if the object still holds a
    /// young reference**.
    ///
    /// Clearing unconditionally would be a use-after-free -- the edge outlives
    /// the store that created it, and nothing would rewrite that field to
    /// re-card it. Both directions are asserted by
    /// `a_card_is_kept_while_the_edge_lives_and_dropped_when_it_dies`, because
    /// each alone is a green that hides the opposite bug.
    ///
    /// Safe here and nowhere else: the world is stopped, so no store can land
    /// between the swap and the re-dirty.
    pub fn young_extra_roots(&self, promotion_age: u8) -> Vec<usize> {
        use census::ZCensusHeapView;
        // NO `arena_base` HERE ANY MORE. The card scan below resolves page
        // bases through `remembered`'s own registration rather than
        // recomputing them; see the note on section (1).
        let dbg_t0 = zgc_dbg_relocate_phases_enabled().then(std::time::Instant::now);
        let mut roots: Vec<usize> = Vec::new();

        // ---- (0) THE EDGES NO CARD COVERS -------------------------------
        //
        // `collect_garbage`'s mark loop pushes four kinds of edge for every
        // object it VISITS: the class's loader, that loader's class mirrors, its
        // metadata roots, and the native collection overlays it owns. A young
        // cycle never visits an old object, so it never pushes any of them --
        // and **not one of the four is written through `set_field`**, so the card
        // barrier cannot see them either. An old `HashMap` with a native overlay
        // whose contents are young would have those contents freed while the map
        // is live: a use-after-free with no wild pointer and no failing
        // assertion, surfacing later as an empty collection.
        //
        // Rooted WHOLESALE rather than per old object, for two reasons. It is
        // O(registry) once instead of O(old objects) times four global lookups
        // -- and those lookups are the per-object shared-cache-line cost
        // `metadata_pin::snapshot`'s own note warns about. And it is safe in the
        // right direction: over-approximating retains an edge whose owner is
        // dead for one more cycle, while under-approximating frees a live
        // object.
        //
        // `gen_heap` does exactly this for the overlays already
        // (`external_roots_for_matching_owners(&|_| true)`), which is the
        // precedent for the shape and the reason the predicate form exists.
        //
        // Every one of these registries is empty in a `--real-jdk` or unit-test
        // run and each has a relaxed-load latch for that, so an arm that
        // exercises none of them pays four loads per CYCLE.
        roots.extend(cratonvm_types::loader_pin::all_pinned_loaders());
        roots.extend(cratonvm_types::mirror_pin::all_pinned_mirrors());
        if let Some(metadata) = cratonvm_types::metadata_pin::snapshot() {
            for (_loader, objects) in metadata {
                roots.extend(objects);
            }
        }
        // THIS heap's owners only: another VM's row can name only that VM's
        // objects, which the `retain` below would drop anyway, so enumerating
        // them was pure cost (the w12-c note on every-VM matching seeds).
        let (arena_base, arena_end) = (self.arena_base, self.arena_end);
        for overlay in cratonvm_gc_core::external_roots::external_roots_for_matching_owners(&|owner| {
            owner >= arena_base && owner < arena_end
        }) {
            roots.push(overlay.as_ptr() as usize);
        }
        // Anything the registries name that this heap does not own -- another
        // VM's rows, or an address a previous cycle has already reclaimed -- is
        // dropped here rather than handed to a marker as a wild root.
        roots.retain(|addr| self.registry.contains(*addr));
        let pinned = roots.len();
        let dbg_t1 = dbg_t0.map(|_| std::time::Instant::now());
        let mut dbg_visited = 0u64;
        let dbg_dist = dbg_t0.map(|_| {
            let sets = self.remembered.snapshot();
            let counts: Vec<usize> = sets.iter().map(|s| s.bits_set_in_snapshot()).collect();
            (sets.len(), counts.iter().filter(|c| **c > 0).count(), counts.iter().copied().max().unwrap_or(0))
        });

        // ---- (1) the cards, cleaned as they are consumed -----------------
        //
        // # Through `remembered`'s own API, not a second copy of its arithmetic
        //
        // *Wired 2026-09-21 (wave 4, lane P), closing
        // `handoff-n-census-engagement-and-namespaced-base.md`.* This loop used
        // to recompute `page_base = arena_base + page_id * Z_LOGICAL_PAGE_BYTES`
        // and turn every bitmap offset into an address itself. That is the
        // consumer-side `page_id -> base` map wave 3 added
        // `register_old_page_with_base_in` to abolish, kept alive here — and a
        // consumer-side map is the shape whose missing entry is a **silently
        // dropped root**, i.e. a live object freed. `iterate_slot_addresses`
        // does the same conversion from the base the registrar supplied, and
        // **refuses** rather than guessing when there is none.
        //
        // The two derivations are identical by construction: all three
        // registration sites in this file
        // (`card_object`, `card_object_two_lookups`, the promotion site) pass
        // `arena_base + page_id * Z_LOGICAL_PAGE_BYTES` as the base, which is
        // exactly the expression deleted here. So this is a change of OWNER,
        // not of value — and `baseless` below is what makes that a checked
        // claim rather than a comment.
        //
        // `retain` replaces the hand-rolled `still_needed` re-card. Same
        // semantics, one lookup fewer: `iterate_snapshot_retaining` re-sets the
        // bit into `current` (the buffer that becomes the next snapshot)
        // directly on the set, where `remember_old_to_young` went back through
        // the table and could coarsen a page it failed to find.
        self.remembered.swap_all();
        // zr z2/g2 (`CRATONVM_ZGC_GEN_ELEMENT_CARDS=1`): ELEMENT cards for large
        // reference arrays. A card that is not an object base names an 8-byte
        // GRANULE inside such an array (`note_ref_store_element`); an object card
        // on such an array is converted into granule cards for its young
        // elements, collected here and written after the walk.
        let element_cards = self.counters.gen_element_cards_on.load(Ordering::Relaxed);
        let mut granules_to_card: Vec<usize> = Vec::new();
        // The last array a granule resolved to, `(base, data_start, data_end)`:
        // the walk is in address order, so consecutive granules of one array
        // pay one base lookup between them.
        let mut last_array: Option<(usize, usize, usize)> = None;
        let mut granule_cards = 0usize;
        // Pages are independent: each owns its snapshot and re-dirties only itself.
        let workers = self.gen_young_sweep_workers();
        let baseless = if workers > 1 && !element_cards {
            self.remset_roots_sharded(promotion_age, workers, &mut roots)
        } else {
        let (visited, _retained, baseless) =
            self.remembered
                .iterate_slot_addresses(true, &mut |addr: u64| {
                    let obj = addr as usize;
                    if !self.registry.contains(obj) {
                        if element_cards {
                            granule_cards += 1;
                            return self.scan_element_granule(
                                obj,
                                promotion_age,
                                &mut last_array,
                                &mut roots,
                            );
                        }
                        return false; // the card outlived its object
                    }
                    if element_cards {
                        if let Some(len) = self.large_reference_array_len(obj) {
                            // Convert: root the young elements, card their
                            // granules, drop the whole-object card.
                            let stride = cratonvm_types::ref_element_size();
                            let data = obj + ARRAY_DATA_OFFSET;
                            for i in 0..len {
                                let slot = data + i * stride;
                                // SAFETY: `i < len` of a registered reference
                                // array whose length passed the plausibility
                                // screen; the slot lies inside its body.
                                let target = unsafe {
                                    cratonvm_types::narrow_oop::read_ref_slot(slot as *mut u8)
                                } as usize;
                                if target != 0
                                    && self.registry.contains(target)
                                    && self.addr_is_young(target, promotion_age)
                                {
                                    roots.push(target);
                                    granules_to_card.push(slot & !7);
                                }
                            }
                            return false;
                        }
                    }
                    self.card_young_roots(addr, promotion_age, &mut roots)
                });
        dbg_visited = visited;
        baseless
        };
        // zr z2/g2: the converted arrays' granule cards, into the CURRENT buffer
        // (the next cycle's snapshot) -- written after the walk so no card is
        // added to a set while it is being iterated.
        if element_cards {
            let converted = granules_to_card.len();
            granules_to_card.dedup();
            for g in granules_to_card {
                self.card_object(g);
            }
            self.counters
                .gen_element_granules_scanned
                .fetch_add(granule_cards, Ordering::Relaxed);
            self.counters
                .gen_element_cards_converted
                .fetch_add(converted, Ordering::Relaxed);
        }
        // MUST BE ZERO, and now says so from a run instead of from an argument.
        // A skipped page is every old->young edge on it dropped for this cycle,
        // which frees live young objects. It cannot happen on this heap — see
        // `ZgcCounters::remset_pages_without_base` — so a non-zero reading is a
        // registration site that was added without a base, not a heap state.
        if baseless != 0 {
            self.counters
                .remset_pages_without_base
                .fetch_add(baseless, Ordering::Relaxed);
            tracing::error!(
                target: "cratonvm::gc::guard",
                pages = baseless,
                "zgc young cycle: remembered-set pages skipped for want of a base \
                 address -- their old->young edges are NOT roots this cycle. Every \
                 registrar on this heap supplies a base, so this is a new call site \
                 using register_old_page or register_old_page_in instead of \
                 register_old_page_with_base_in.",
            );
        }
        let dbg_t2 = dbg_t0.map(|_| std::time::Instant::now());
        let mut emptied: Vec<u64> = Vec::new();
        for set in self.remembered.snapshot() {
            set.clear_snapshot();
            if set.bits_set() == 0 {
                emptied.push(set.page_id());
            }
        }
        // Drop the sets that hold nothing. The barrier re-registers on demand,
        // so this is not a decision the store path can be wrong about -- it is
        // only the table not keeping a bitmap per page forever.
        for page in emptied {
            self.remembered.remove(page);
        }

        // ---- (2) the COARSENED pages -------------------------------------
        //
        // `ZRememberedSetTable::remember` cannot always record a precise slot:
        // an unregistered page, or an offset outside the page's bitmap, makes
        // it return `ZRememberOutcome::Coarsened` and add the page id to the
        // coarse set instead. Both of that machinery's docs say what is then
        // owed -- *"the next young collection must scan the whole page
        // conservatively"*, and *"this exists so a barrier can never silently
        // drop an old->young edge"*.
        //
        // NOTHING PAID IT. `take_coarse_pages` has no caller anywhere in the
        // tree, on any collector, so the coarse set is written and never read:
        // an edge the table believed it had over-approximated into safety is
        // in fact dropped, and dropping an old->young edge frees a live object.
        // The promise and its own `warn!` both sit behind a call nobody makes
        // -- the guard-with-no-consumer shape, where the *absence* of the
        // warning reads as "this never happens" whether or not it does.
        //
        // What "scan the page conservatively" means on THIS heap: the registry
        // is a bitmap of allocation bases, so every base inside a coarsened
        // page becomes a root. That retains those objects and their subgraphs
        // for this cycle, which is exactly the over-approximation the enum's
        // doc prices ("over-approximating one costs a scan").
        //
        // Cost when nothing coarsened -- which is every cycle today, because
        // `card_object` registers the page before setting the bit -- is one
        // mutex acquire on an empty set. The registry walk happens only when
        // the set is non-empty, i.e. only when a wiring bug has already put a
        // live object at risk.
        let dbg_t3 = dbg_t0.map(|_| std::time::Instant::now());
        let coarse = self.remembered.take_coarse_pages();
        // t1 wave 6 lane Y4: a coarsened or baseless page means the carded
        // set does not name every old holder of a young reference, so this
        // cycle's nursery-scoped slide (`CRATONVM_ZGC_GEN_YOUNG_EVACUATE`)
        // must not move anything. Written on every young cycle.
        self.counters
            .young_evac
            .remset_incomplete
            .store(!coarse.is_empty() || baseless != 0, Ordering::Relaxed);
        let mut coarse_roots = 0usize;
        if !coarse.is_empty() {
            let ranges: Vec<(usize, usize)> = coarse
                .iter()
                .map(|id| {
                    // `self.arena_base`, not a local `base`. The generational
                    // round wrote this block against a local binding that the
                    // ZGC round's rewrite of this function removed in the same
                    // week -- the card scan below now resolves page bases
                    // through the remembered set instead (see the note at the
                    // head of this function). A COARSENED page is the one case
                    // with no recorded base to resolve, because coarsening is
                    // what happens when the table could not map the address,
                    // so it is computed from the arena origin exactly as
                    // `register_old_page_with_base_in`'s callers do.
                    let lo = self.arena_base + (*id as usize) * Self::Z_LOGICAL_PAGE_BYTES;
                    (lo, lo + Self::Z_LOGICAL_PAGE_BYTES)
                })
                .collect();
            self.registry.for_each_base(usize::MAX, &mut |addr| {
                if ranges.iter().any(|(lo, hi)| addr >= *lo && addr < *hi) {
                    roots.push(addr);
                    coarse_roots += 1;
                }
                true
            });
            tracing::warn!(
                target: "zgc",
                pages = coarse.len(),
                roots = coarse_roots,
                "zgc young cycle: rooted whole coarsened old pages -- a store \
                 barrier recorded an edge the page's bitmap could not express, \
                 so the page is retained wholesale this cycle. This is a wiring \
                 bug (register_old_page was not called for the page), not a heap \
                 bug, and it costs retention rather than correctness"
            );
        }

        if let (Some(t0), Some(t1), Some(t2), Some(t3)) = (dbg_t0, dbg_t1, dbg_t2, dbg_t3) {
            eprintln!(
                "[zgc-young-remset] pinned_us={} scan_us={} clear_us={} pinned={pinned} cards={dbg_visited} roots={} sets_nonzero_max={:?}",
                t1.duration_since(t0).as_micros(),
                t2.duration_since(t1).as_micros(),
                t3.duration_since(t2).as_micros(),
                roots.len() - pinned,
                dbg_dist,
            );
        }
        tracing::debug!(
            target: "zgc",
            pinned,
            carded = roots.len() - pinned - coarse_roots,
            coarse_roots,
            "zgc young cycle: extra roots"
        );
        roots
    }

    /// Age one survivor and, if this collection promoted it, card it.
    ///
    /// Called from the sweep, which already holds the header and already writes
    /// to it, so the cost is one branch per survivor on a generational run.
    ///
    /// # The card at promotion is what makes the young cycle sound
    ///
    /// The barrier cards a store only when the receiver is ALREADY old, so every
    /// reference an object wrote during its young life is un-carded at the moment
    /// it is promoted. A young cycle that trusted the (empty) card set for a
    /// freshly promoted object would miss exactly those edges. Carding it here
    /// makes the debt explicit and settles it once: the next young cycle
    /// re-scans the object, keeps the card if it still points into young, and
    /// drops it otherwise.
    ///
    /// This is the per-object answer to the hazard the plan described in page
    /// terms ("the first young cycle after a promotion must treat the newly-old
    /// page as wholly dirty"). Per object it is exact and costs one card instead
    /// of a scan of every object on the page.
    ///
    /// Returns `true` if this call promoted the object.
    ///
    /// zt t6/tc (GN-5): on a `System.gc()` major that tenures every survivor
    /// (`gen_tenure_all_cycle`, HotSpot's full collection; gci i47w/sg47's
    /// `CRATONVM_GC_SYSTEM_GC_TENURES_ALL` on the other backends) a young
    /// survivor ages straight to the promotion age, with the same card and
    /// old-start bit as an ordinary promotion.
    pub fn age_survivor(&self, base: usize, header: &ObjectHeader, promotion_age: u8) -> bool {
        let age = header.gc_age();
        if age >= promotion_age {
            // Already old; nothing to age and nothing to card. A whole-heap
            // cycle's sweep is what REBUILDS the old-start bitmap, so the bit
            // is (re)asserted here too -- see `note_old_start`.
            self.note_old_start(base);
            return false;
        }
        let next = if self.counters.gen_tenure_all_cycle.load(Ordering::Relaxed) {
            promotion_age
        } else {
            age.saturating_add(1)
        };
        header.set_gc_age(next);
        if next < promotion_age {
            return false;
        }
        self.card_object(base);
        self.note_old_start(base);
        true
    }

    // =====================================================================
    // zr z1/gn -- THE OLD-START BITMAP
    // =====================================================================
    //
    // # What a young cycle paid that was O(heap)
    //
    // The pre-mark (the PHASE G arm of the mark-bit pass in `collect_garbage`)
    // walked EVERY registered base and read its header's `gc_age` to decide
    // whether to pre-mark it. On a heap whose old generation is millions of
    // objects that is millions of scattered header loads per young cycle --
    // the bulk of the `mark_us` the 2026-08-17 anatomy measured at ~90% of a
    // young pause, and the reason that anatomy found a young mark costing MORE
    // than a whole-heap one.
    //
    // The bitmap answers the same question for a whole 512-byte word at once:
    // `mark |= registered & old` is a linear pass over `capacity / 512` bytes,
    // the same shape and cost as `mark_clear_all`, with no header touched.
    //
    // # The invariant, and who keeps it
    //
    // For every registered base `b` on the grid: `old(b)` <=> `gc_age(b) >=
    // promotion_age`. The direction that matters is `old(b)` => old by header:
    // a bit on a YOUNG object would pre-mark it, its fields would never be
    // traced, and the young children only it reaches would be swept while it
    // is live. So:
    //
    //  * a whole-heap cycle CLEARS the bitmap before its sweep
    //    (`begin_old_bits_rebuild`) and its sweep re-sets a bit for every old
    //    survivor through `age_survivor` -- dead old objects lose theirs;
    //  * a young cycle frees only UNMARKED objects, and every bit-holder was
    //    pre-marked, so no freed address keeps a bit;
    //  * a slide moves the bits with the objects (`remap_old_bits_after_slide`);
    //  * anything that can change ages behind the bitmap's back (a mode
    //    toggle, a promotion-age change, the generational reset) lowers
    //    `gen_old_bits_valid`, and young cycles fall back to the header walk
    //    until the next whole-heap cycle has rebuilt it.
    //
    // The other direction (old by header, no bit) is imprecision for the
    // pre-mark (such an object is traced like a young one) but NOT for the
    // young-aged sweep, which visits `registered & !old` and frees what is
    // unmarked: an old object reachable only through an old holder that is
    // pre-marked (untraced) and carries no card would be freed under it (zr
    // z3/rv3). No production path ages an object without `note_old_start`,
    // and an invalid bitmap now turns the young cycle whole-heap
    // (`gen_young_refused_for_bitmap`). `CRATONVM_ZGC_GEN_OLD_BITMAP_VERIFY=1`
    // counts both directions; both must read 0.

    /// Record that the object at `base` is old. See the section note.
    #[inline]
    pub fn note_old_start(&self, base: usize) {
        if !self.counters.gen_old_bits_on.load(Ordering::Relaxed) {
            return;
        }
        let bits = self.old_bits();
        if !bits.contains(base) {
            bits.insert(base);
        }
    }

    /// The old-start bitmap, allocated on first use over the same geometry as
    /// the registry and the mark bits.
    pub fn old_bits(&self) -> &ZObjectStartBits {
        self.counters.gen_old_bits.get_or_init(|| {
            ZObjectStartBits::labelled_side_table(
                self.arena_base,
                self.arena_end.saturating_sub(self.arena_base),
                "old",
                zgc_side_table_no_thp_enabled(),
            )
        })
    }

    /// Can THIS young cycle pre-mark from the bitmap?
    pub fn old_bits_usable(&self) -> bool {
        self.counters.gen_old_bits_on.load(Ordering::Relaxed)
            && self.counters.gen_old_bits_valid.load(Ordering::Relaxed)
            && self.mark_bits.is_some()
            && self.counters.gen_old_bits.get().is_some()
    }

    /// zr z3/rv3: must THIS would-be young cycle run whole-heap because the
    /// bitmap is on but not usable? The young cycle's fallback is the header
    /// walk plus the FLOOR-BOUNDED sweep (`young_aged_sweep` needs a bitmap
    /// pre-mark), which is the arm that corrupts weak referents (z2 host row
    /// W1, `Zr2G2WeakBelowFloor` with `CRATONVM_ZGC_GEN_OLD_BITMAP=0`: 256/256)
    /// -- and, under the young-aged sweep, an old object the bitmap does not
    /// name would be visited and freed while an untraced old holder still
    /// points at it. A whole-heap cycle is sound and rebuilds the bitmap.
    /// Reached after an invalidation (mode toggle, promotion-age change,
    /// generational reset) and on every young cycle when the side mark bits
    /// are off (`CRATONVM_ZGC_MARKBITS=0`). Counts the refusal. `false`
    /// with the bitmap off (`CRATONVM_ZGC_GEN_OLD_BITMAP=0` keeps its old
    /// arm) or under `CRATONVM_ZGC_GEN_BITMAP_FALLBACK_MAJOR=0`.
    pub fn gen_young_refused_for_bitmap(&self) -> bool {
        if !self.counters.gen_old_bits_on.load(Ordering::Relaxed)
            || !self
                .counters
                .gen_bitmap_fallback_major
                .load(Ordering::Relaxed)
            || self.old_bits_usable()
        {
            return false;
        }
        self.counters
            .gen_old_bits_young_refusals
            .fetch_add(1, Ordering::Relaxed);
        true
    }

    /// zr z3/rv3: per-heap `CRATONVM_ZGC_GEN_BITMAP_FALLBACK_MAJOR` (tests).
    pub fn set_gen_bitmap_fallback_major(&self, on: bool) {
        self.counters
            .gen_bitmap_fallback_major
            .store(on, Ordering::Relaxed);
    }

    /// zr z3/rv3: young cycles run whole-heap by
    /// [`Self::gen_young_refused_for_bitmap`].
    pub fn gen_old_bits_young_refusals(&self) -> usize {
        self.counters
            .gen_old_bits_young_refusals
            .load(Ordering::Relaxed)
    }

    /// Lower the bitmap's validity. Called by every path that can make a
    /// header age and the bitmap disagree.
    pub fn invalidate_old_bits(&self) {
        self.counters
            .gen_old_bits_valid
            .store(false, Ordering::Relaxed);
    }

    /// Turn the old-start bitmap on or off for THIS heap (tests; the
    /// process default is `CRATONVM_ZGC_GEN_OLD_BITMAP`). Either way it is not
    /// trusted until the next whole-heap cycle has rebuilt it.
    pub fn set_gen_old_bitmap_enabled(&self, on: bool) {
        self.counters.gen_old_bits_on.store(on, Ordering::Relaxed);
        self.invalidate_old_bits();
    }

    /// Turn the young-aged sweep on or off for THIS heap (tests; the process
    /// default is `CRATONVM_ZGC_GEN_YOUNG_AGED_SWEEP`).
    pub fn set_gen_young_aged_sweep_enabled(&self, on: bool) {
        self.counters
            .gen_young_aged_sweep_on
            .store(on, Ordering::Relaxed);
    }

    /// Turn the age-filtered nursery census on or off for THIS heap.
    pub fn set_nursery_census_age_enabled(&self, on: bool) {
        self.counters.gen_census_age_on.store(on, Ordering::Relaxed);
    }

    /// A whole-heap cycle is starting: clear the bitmap so its sweep can
    /// rebuild it from the survivors. STW. Bounded by the same ranges as the
    /// mark clear, read BEFORE `mark_clear_all` resets the water marks: every
    /// bit sits on a registered base, and every registered base is inside
    /// those ranges.
    pub fn begin_old_bits_rebuild(&self) {
        if !self.counters.gen_old_bits_on.load(Ordering::Relaxed) {
            return;
        }
        self.invalidate_old_bits();
        let bits = self.old_bits();
        if zgc_bitmap_bounds_enabled() {
            self.clear_bitmap_within(bits, &self.live_bitmap_bounds());
        } else {
            bits.clear_all();
        }
        self.counters
            .gen_old_bits_rebuilding
            .store(true, Ordering::Relaxed);
    }

    /// The whole-heap cycle's sweep has re-set every old survivor's bit.
    pub fn finish_old_bits_rebuild(&self) {
        if self
            .counters
            .gen_old_bits_rebuilding
            .swap(false, Ordering::Relaxed)
            && self.counters.gen_old_bits_on.load(Ordering::Relaxed)
        {
            self.counters
                .gen_old_bits_valid
                .store(true, Ordering::Relaxed);
        }
    }

    /// The young cycle's pre-mark from the bitmap: `mark |= registered & old`,
    /// word by word, plus the (normally empty) spill set one base at a time.
    /// Returns the number of objects pre-marked -- the same `old_retained`
    /// figure the header walk produces.
    pub fn premark_from_old_bits(&self, registered: &ZObjectStartsSnapshot) -> usize {
        let (Some(marks), Some(old)) = (self.mark_bits.as_ref(), self.counters.gen_old_bits.get())
        else {
            return 0;
        };
        let len = registered.words.len().min(marks.nwords).min(old.nwords);
        let mut n = 0usize;
        for (w0, w1) in registered.segments(0, len) {
            for w in w0..w1 {
                let m = registered.words[w] & old.word_at(w);
                if m != 0 {
                    marks.note_word(w);
                    // SAFETY: `w < len <= marks.nwords`.
                    unsafe { (*marks.words.add(w)).fetch_or(m, Ordering::Release) };
                    n += m.count_ones() as usize;
                }
            }
        }
        for b in registered.extra.iter() {
            if old.contains(*b) {
                self.mark_set(*b);
                n += 1;
            }
        }
        n
    }

    /// `CRATONVM_ZGC_GEN_OLD_BITMAP_VERIFY=1`: compare the pre-mark just
    /// taken from the bitmap against every registered header. Must run before
    /// any marker sets a bit. O(registry) -- a diagnostic, never a default.
    pub fn verify_old_premark(&self, registered: &ZObjectStartsSnapshot, promo_age: u8) {
        let (mut unsound, mut imprecise) = (0usize, 0usize);
        registered.for_each_base(|b| {
            let old_by_header = self.header_ref(b as *mut u8).gc_age() >= promo_age;
            let premarked = self.mark_is_set(b);
            if premarked && !old_by_header {
                unsound += 1;
            } else if old_by_header && !premarked {
                imprecise += 1;
            }
        });
        if unsound != 0 {
            tracing::error!(
                target: "cratonvm::gc::guard",
                unsound,
                "zgc young cycle: the old-start bitmap pre-marked YOUNG objects -- they are retained untraced and their young children can be freed under them. The bitmap missed a clear (a free or a move that did not go through the sweep or the slide's remap)."
            );
        }
        self.counters
            .gen_old_bits_verify_unsound
            .fetch_add(unsound, Ordering::Relaxed);
        self.counters
            .gen_old_bits_verify_imprecise
            .fetch_add(imprecise, Ordering::Relaxed);
    }

    /// Move the old-start bits with the objects a slide moved: clear every
    /// source first, then set every destination whose header is old. Two
    /// passes because a destination is often another object's source.
    pub fn remap_old_bits_after_slide(&self, map: &cratonvm_types::PointerMap, promo_age: u8) {
        if !self.counters.gen_old_bits_on.load(Ordering::Relaxed) {
            return;
        }
        let Some(old) = self.counters.gen_old_bits.get() else {
            return;
        };
        for (from, _to) in map.iter() {
            old.remove(*from);
        }
        for (_from, to) in map.iter() {
            if !self.addr_is_young(*to, promo_age) {
                old.insert(*to);
            }
        }
    }

    /// THE YOUNG-AGED SWEEP: on a young cycle whose pre-mark came from a valid
    /// old-start bitmap, visit every registered base that is NOT old --
    /// `registered & !old`, word by word -- wherever it is, instead of
    /// `[floor, cursor)` plus the young grains.
    ///
    /// # Why the floor is the wrong bound once the bitmap exists
    ///
    /// The floor-bounded sweep leaves one population alone: objects below the
    /// floor that are still YOUNG by age (survivors of the last whole-heap
    /// cycle, nursery survivors a slide moved under the new floor). When they
    /// die they are unmarked and unswept, so they stay registered while the
    /// young children only they reached are freed -- and the VM's
    /// post-collection liveness probe reads "registered" as "survived". They
    /// also never AGE between majors, so with the default ceiling of eight
    /// minors per major an object needs three majors (not three collections)
    /// to promote and is traced on every young cycle in between.
    ///
    /// Visiting exactly the non-old set fixes both and costs what the young
    /// generation IS: the pre-mark already says which objects are old, so the
    /// complement is free to enumerate. Old objects are not visited at all (the
    /// floor-bounded sweep visited the ones promoted inside the nursery).
    ///
    /// Ascending, like every sweep arm, because the free-list hand-over needs
    /// ascending spans. Returns the shard plus the below-floor slice of its
    /// accounting: `(visited, bytes_freed, bytes_copied, promoted_bytes)` for bases under
    /// `floor`, which the live figure must not count twice (they are inside
    /// `gen_old_live_bytes` already).
    ///
    /// zt t6/tc (GN-17): the walk is `sweep_young_aged_words`, sharded over
    /// `CRATONVM_ZGC_GEN_YOUNG_SWEEP_WORKERS` threads when that is above 1
    /// (`sweep_young_aged_sharded`, opt-in; 1 = this thread, the old loop).
    pub fn sweep_young_aged(
        &self,
        registered: &ZObjectStartsSnapshot,
        floor: usize,
        cfg: &ZSweepCfg,
    ) -> (ZSweepShard, (usize, usize, usize, usize)) {
        self.sweep_young_aged_sharded(
            registered,
            registered.words.len(),
            floor,
            usize::MAX,
            cfg,
            self.gen_young_sweep_workers(),
        )
    }

}
