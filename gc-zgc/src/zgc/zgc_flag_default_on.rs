#[allow(unused_imports)]
use super::*;

impl ZgcRealHeap {
    /// Is `CRATONVM_ZGC_CONC_EVERY_TRIGGER` acting on this heap? One relaxed
    /// load on the default (off) path.
    #[inline]
    pub fn conc_every_trigger_armed(&self) -> bool {
        self.counters.conc_trigger.enabled.load(Ordering::Relaxed)
            && self.conc_start_bytes.load(Ordering::Relaxed) != 0
            && !self.generational_enabled.load(Ordering::Relaxed)
    }

    /// Arrange `CRATONVM_ZGC_CONC_EVERY_TRIGGER` on this heap. **Test seam**
    /// (production reads the variable once, at construction).
    #[cfg(test)]
    pub fn dbg_set_conc_every_trigger(&self, on: bool) {
        // zt t27/gn5 (the gn page's item 10): the every-trigger mode never
        // acts in ZGC's generational mode ([`Self::conc_every_trigger_armed`]),
        // so a test arming it pins the single-generation heap it is about;
        // under `CRATONVM_ZGC_GENERATIONAL=1` every test heap starts
        // generational.
        if on && self.generational_enabled() {
            self.set_generational_enabled(false);
        }
        self.counters
            .conc_trigger
            .enabled
            .store(on, Ordering::Relaxed);
    }

    pub fn conc_trigger_reserve(&self) -> usize {
        let cap = self.heap_capacity();
        director::conc_urgent_reserve(cap, zgc_headroom_margin(cap))
    }

    /// Has the open cycle's marker reached its fixed point? A `try_lock`: a
    /// poll that loses the race reads "not yet" and asks again at its next
    /// poll. No pool with a cycle open reads "yes" (nothing left to wait for).
    pub fn conc_mark_caught_up(&self) -> bool {
        match self.counters.conc_pool.try_lock() {
            Some(pool) => pool
                .as_ref()
                .map_or(true, |c| c.shared().terminator().is_terminated_hint()),
            None => false,
        }
    }

    /// `needs_gc`'s gate under the switch: `Some(answer)` while a cycle is open
    /// or requested, `None` to let the ordinary clauses decide.
    pub fn conc_every_trigger_gate(&self, a: usize) -> Option<bool> {
        let st = &self.counters.conc_trigger;
        let active = self.conc_cycle_active.load(Ordering::Acquire);
        let requested_at = st.requested_at.load(Ordering::Relaxed);
        if !active && requested_at == 0 {
            return None;
        }
        let inputs = director::ConcTriggerInputs {
            active,
            caught_up: active && self.conc_mark_caught_up(),
            requested_at,
            refused: st.refused.load(Ordering::Relaxed),
            used: a,
            capacity: self.heap_capacity(),
            reserve: self.conc_trigger_reserve(),
        };
        let gate = director::conc_trigger_gate(&inputs);
        // zt t26/zc9: the allocation-rate rule's own request lapsed (its mark
        // start refused, or no site served it) with room left: it is dropped
        // and the clauses decide, rather than an early stop-the-world
        // collection. A clause's request, or any at the urgent reserve,
        // lapses into its collection as before. One relaxed load otherwise.
        if gate == director::ConcTriggerGate::RequestLapsed
            && self.conc_rate_start_drop_lapsed(
                requested_at,
                inputs.used,
                inputs.capacity,
                inputs.reserve,
            )
        {
            return None;
        }
        if gate == director::ConcTriggerGate::UrgentWhileOpen {
            st.urgent_seen.store(true, Ordering::Relaxed);
        }
        // zt t18/zc1: a fixed point the `System.gc()` caller is waiting for is
        // that caller's mark end, not the next allocating poll's: its own
        // collection closes the cycle under its door and cause, as HotSpot's
        // synchronous request completes the one cycle it started. Held only
        // above the urgent reserve and only until the owner's deadline.
        if gate == director::ConcTriggerGate::MarkFinished
            && self.system_gc_holds_mark_end(inputs.used, inputs.capacity, inputs.reserve)
        {
            st.sysgc_held.fetch_add(1, Ordering::Relaxed);
            return Some(false);
        }
        gate.answer()
    }

    /// zt t18/zc1: does the open cycle's mark end still belong to the
    /// `System.gc()` request that opened it? `true` while its deadline has not
    /// passed and free space is above the urgent reserve. One relaxed load when
    /// no request owns a cycle.
    pub fn system_gc_holds_mark_end(&self, used: usize, capacity: usize, reserve: usize) -> bool {
        let deadline = self
            .counters
            .conc_trigger
            .sysgc_owner_deadline
            .load(Ordering::Relaxed);
        deadline != 0
            && capacity.saturating_sub(used) > reserve
            && Self::monotonic_nanos() < deadline
    }

    /// zt t18/zc1 (`CRATONVM_ZGC_SYSTEM_GC_CONCURRENT`): a `System.gc()` /
    /// `jcmd GC.run` request asks for a concurrent cycle of its own. Records an
    /// every-trigger request (so the VM's mark start, which re-tests
    /// `should_start_concurrent_mark`, opens it) and makes this request the
    /// owner of the cycle about to open, BEFORE the mark start: a collection
    /// that runs in between consumes the ownership, and the request then falls
    /// back to the stop-the-world collection it always ran. `false` (nothing
    /// recorded) with the switch off, outside the every-trigger mode, in
    /// generational mode, or with a cycle already open (the VM closes an open
    /// cycle first, `zgc_full_request_closes_open_cycle`, so one still open here
    /// started before this request and is not its cycle).
    ///
    /// No headroom test, unlike an early trigger's request
    /// (`conc_every_trigger_early`): HotSpot's request always runs a concurrent
    /// cycle, and the urgent reserve still closes this one stop-the-world
    /// ([`Self::system_gc_holds_mark_end`] does not hold there).
    pub fn request_system_gc_concurrent_cycle(&self) -> bool {
        let st = &self.counters.conc_trigger;
        if !st.sysgc_enabled.load(Ordering::Relaxed)
            || !self.conc_every_trigger_armed()
            || self.conc_cycle_active.load(Ordering::Acquire)
        {
            return false;
        }
        st.sysgc_requests.fetch_add(1, Ordering::Relaxed);
        let a = self.allocated.load(Ordering::Relaxed);
        if st
            .requested_at
            .compare_exchange(0, a.max(1), Ordering::Relaxed, Ordering::Relaxed)
            .is_ok()
        {
            st.requests.fetch_add(1, Ordering::Relaxed);
        }
        // A request refused earlier would make `conc_start_requested` answer
        // no; this one is new, so the start is due again.
        st.refused.store(false, Ordering::Relaxed);
        // zt t26/zc9: a pending allocation-rate request is this caller's now
        // (its lapse must fall back to the caller's collection, not be
        // dropped).
        self.conc_rate_start_disown(false);
        let deadline = Self::monotonic_nanos().saturating_add(ZGC_SYSTEM_GC_CONC_HOLD_NANOS);
        st.sysgc_owner_deadline
            .store(deadline.max(1), Ordering::Relaxed);
        true
    }

    /// zt t18/zc1: the VM ran the mark start of a
    /// [`Self::request_system_gc_concurrent_cycle`] request; `opened` = a cycle
    /// is open now. Not open: the ownership is dropped (the request falls back
    /// to its stop-the-world collection, which then marks from scratch).
    pub fn note_system_gc_concurrent_cycle(&self, opened: bool) {
        let st = &self.counters.conc_trigger;
        if opened {
            st.sysgc_opened.fetch_add(1, Ordering::Relaxed);
        } else {
            st.sysgc_owner_deadline.store(0, Ordering::Relaxed);
            st.sysgc_not_opened.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// zt t18/zc1: has the open concurrent cycle's marker reached its fixed
    /// point? `false` with no cycle open. For the `System.gc()` caller's wait;
    /// a `try_lock` like every poll of it ([`Self::conc_mark_caught_up`]).
    pub fn concurrent_mark_caught_up(&self) -> bool {
        self.conc_cycle_active.load(Ordering::Acquire) && self.conc_mark_caught_up()
    }

    /// zt t18/zc1: take this collection's `System.gc()` cycle ownership
    /// (every collection takes it, at the collection safepoint, before the
    /// full-request abandon). `true` = a request owns the cycle open now; with
    /// no cycle open, a request whose mark start has not run yet, which then
    /// finds its ownership gone and falls back.
    pub fn take_system_gc_cycle_owner(&self) -> bool {
        self.counters
            .conc_trigger
            .sysgc_owner_deadline
            .swap(0, Ordering::Relaxed)
            != 0
    }

    /// Arrange `CRATONVM_ZGC_SYSTEM_GC_CONCURRENT` on this heap. **Test seam**
    /// (production reads the variable once, at construction).
    #[cfg(test)]
    pub fn dbg_set_system_gc_concurrent(&self, on: bool) {
        self.counters
            .conc_trigger
            .sysgc_enabled
            .store(on, Ordering::Relaxed);
    }

    /// An early `needs_gc` clause said yes at `a`. Returns `needs_gc`'s
    /// answer: `true` unless the switch turns the yes into a concurrent-cycle
    /// request.
    pub fn conc_every_trigger_early(&self, a: usize) -> bool {
        if !self.conc_every_trigger_armed() {
            return true;
        }
        let st = &self.counters.conc_trigger;
        let cap = self.heap_capacity();
        if director::conc_cycle_has_headroom(
            cap,
            a,
            self.conc_trigger_reserve(),
            self.alloc_rate_scaled.load(Ordering::Relaxed),
            self.conc_mark_nanos_ewma.load(Ordering::Relaxed),
        ) {
            if st
                .requested_at
                .compare_exchange(0, a.max(1), Ordering::Relaxed, Ordering::Relaxed)
                .is_ok()
            {
                st.refused.store(false, Ordering::Relaxed);
                st.requests.fetch_add(1, Ordering::Relaxed);
            }
            false
        } else {
            if !st.no_headroom_seen.swap(true, Ordering::Relaxed) {
                st.no_headroom.fetch_add(1, Ordering::Relaxed);
            }
            true
        }
    }

    /// Is a requested concurrent cycle waiting for a site to open it? For the
    /// VM's poll sites (`VmHeap::zgc_conc_start_requested`); one relaxed load
    /// with the switch off.
    #[inline]
    pub fn conc_start_requested(&self) -> bool {
        self.conc_every_trigger_armed()
            && self
                .counters
                .conc_trigger
                .requested_at
                .load(Ordering::Relaxed)
                != 0
            && !self.counters.conc_trigger.refused.load(Ordering::Relaxed)
            && !self.conc_cycle_active.load(Ordering::Relaxed)
    }

    /// The VM ran (or refused) a mark start. A refusal of a REQUESTED start
    /// lapses the request at once, so the next poll collects stop-the-world
    /// instead of waiting out the grace. No-op with the switch off.
    pub fn note_conc_start_outcome(&self, opened: bool) {
        let st = &self.counters.conc_trigger;
        // zr zs s1/tp: the open race's loser, in any concurrent mode (the
        // engagement figure for `CRATONVM_ZGC_CONC_START_RECHECK`).
        if !opened && self.conc_cycle_active.load(Ordering::Acquire) {
            st.lost_races.fetch_add(1, Ordering::Relaxed);
        }
        if !self.conc_every_trigger_armed() {
            return;
        }
        if opened {
            st.opened.fetch_add(1, Ordering::Relaxed);
            // zt t26/zc9: `crs_opened` when the request was the rate rule's.
            self.note_rate_start_opened();
        } else if self.conc_cycle_active.load(Ordering::Acquire) {
            // zr z4/rv4: another opener won. Two sites that both saw the
            // request race to `start_concurrent_mark`, whose first line refuses
            // the loser because a cycle is active; that is the request served,
            // not refused -- lapsing it here booked a refusal while the cycle
            // ran and broke `lapsed >= start_refusals`.
        } else if st.requested_at.load(Ordering::Relaxed) != 0 {
            st.refused.store(true, Ordering::Relaxed);
            st.start_refusals.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Book the collection now closing the interval. **At the collection
    /// safepoint, before `finish_concurrent_mark`** (which clears the open
    /// flag and drops the pool).
    pub fn conc_every_trigger_account_collection(&self) {
        if !self.conc_every_trigger_armed() {
            return;
        }
        let st = &self.counters.conc_trigger;
        if self.conc_cycle_active.load(Ordering::Acquire) {
            let caught_up = self
                .counters
                .conc_pool
                .lock()
                .as_ref()
                .map_or(true, |c| c.shared().terminator().is_terminated_hint());
            if caught_up {
                st.finished.fetch_add(1, Ordering::Relaxed);
            } else {
                st.abandoned.fetch_add(1, Ordering::Relaxed);
                if st.urgent_seen.load(Ordering::Relaxed) {
                    st.urgent.fetch_add(1, Ordering::Relaxed);
                }
            }
            let started = self.counters.conc_mark_started_at.load(Ordering::Relaxed);
            let phase = if started == 0 {
                0
            } else {
                Self::monotonic_nanos().saturating_sub(started)            };
            st.last_phase_nanos.store(phase, Ordering::Relaxed);        } else {
            st.last_phase_nanos.store(0, Ordering::Relaxed);
            if st.requested_at.load(Ordering::Relaxed) != 0 {
                st.lapsed.fetch_add(1, Ordering::Relaxed);
            }
        }
        st.requested_at.store(0, Ordering::Relaxed);
        st.refused.store(false, Ordering::Relaxed);
        st.urgent_seen.store(false, Ordering::Relaxed);
        st.no_headroom_seen.store(false, Ordering::Relaxed);
        // zt t26/zc9: the slot is empty again and the interval closed.
        self.conc_rate_start_disown(true);
    }

    /// zr zs s1/tp: the VM dropped a mark start before requesting its pause
    /// (`CRATONVM_ZGC_CONC_START_RECHECK`): the start was no longer due -- a
    /// cycle opened, a collection ran, or the window closed -- since the
    /// caller's own test.
    pub fn note_conc_start_skipped(&self) {
        self.counters
            .conc_trigger
            .start_rechecks
            .fetch_add(1, Ordering::Relaxed);
    }

    /// The `[GC] zgc-conc-trigger:` figures.
    pub fn conc_trigger_report(&self) -> ZConcTriggerReport {
        let st = &self.counters.conc_trigger;
        let n = |c: &AtomicUsize| c.load(Ordering::Relaxed);
        ZConcTriggerReport {
            enabled: self.conc_every_trigger_armed(),
            requests: n(&st.requests),
            opened: n(&st.opened),
            start_refusals: n(&st.start_refusals),
            finished: n(&st.finished),
            abandoned: n(&st.abandoned),
            urgent: n(&st.urgent),
            lapsed: n(&st.lapsed),
            no_headroom: n(&st.no_headroom),
            lost_races: n(&st.lost_races),
            start_rechecks: n(&st.start_rechecks),
            sysgc_enabled: st.sysgc_enabled.load(Ordering::Relaxed),
            sysgc_requests: n(&st.sysgc_requests),
            sysgc_opened: n(&st.sysgc_opened),
            sysgc_served: n(&st.sysgc_served),
            sysgc_lost: n(&st.sysgc_lost),
            sysgc_not_opened: n(&st.sysgc_not_opened),
            sysgc_held: n(&st.sysgc_held),
        }
    }
}

/// A per-heap construction seed for a default-ON zr switch: unset or empty =
/// on, any off word (`0`, `off`, `false`, `no`) = off.
pub fn zgc_flag_default_on(key: &str) -> bool {
    match cratonvm_types::flags::runtime_var(key) {
        Ok(value) if !value.trim().is_empty() => !cratonvm_types::flags::value_is_off(&value),
        _ => true,
    }
}

/// `CRATONVM_ZGC_ALWAYS_PRE_TOUCH` (zt t23/hunt4, default ON, `=0` kill):
/// `-XX:+AlwaysPreTouch` touches the ZGC heap's committed pages and the
/// `gc,init` block says `Pre-touch: Enabled`, as HotSpot 25
/// ([`ZgcRealHeap::set_always_pre_touch`]). `=0`: the option is accepted and
/// ignored, as before. Read once per VM start.
pub fn zgc_always_pre_touch_enabled() -> bool {
    true
}

/// `CRATONVM_ZGC_INTERVAL_ONLY_OCCUPANCY_DOORS_STALL` (zt t20/tg9, Defect D3
/// of `t16-row-verdicts.md`; default ON, `=0` kill): under
/// `-XX:+ZCollectionIntervalOnly` an occupancy-classified forced door's
/// collection is an allocation stall (`Allocation Stall`, the `Timer` clock
/// untouched); see [`ZgcRealHeap::occupancy_door_is_not_a_stall`]. Read only
/// when the flag is in force (a collection at such a door is rare). `=0`:
/// those doors carry `Allocation Rate` there, as t14.
pub fn zgc_interval_only_occupancy_doors_stall_enabled() -> bool {
    zgc_flag_default_on("CRATONVM_ZGC_INTERVAL_ONLY_OCCUPANCY_DOORS_STALL")
}

/// SATB filter mode: drop an already-marked old value at the STORE.
pub const Z_SATB_FILTER_STORE: u8 = 1;
/// SATB filter mode: drop already-marked entries when a thread buffer FLUSHES.
pub const Z_SATB_FILTER_FLUSH: u8 = 2;

/// `CRATONVM_ZGC_SATB_FILTER_MARKED` -- zr z7/cm. Drops SATB records whose
/// overwritten value is already marked (see
/// `ZgcRealHeap::satb_pre_barrier_slow`). Unset / `2` / `flush`: at the
/// thread-buffer flush (default since zr z7b); `1` / `store`: per store (the
/// z7 arm); `0` (or any off word): kill. Inert unless a concurrent cycle arms
/// the barrier. A construction seed (per heap), not cached.
pub fn zgc_satb_filter_mode() -> u8 {
    match None::<String> {
        Some(value) if !value.trim().is_empty() => {
            let v = value.trim().to_ascii_lowercase();
            if v == "1" || v == "store" {
                Z_SATB_FILTER_STORE            } else if cratonvm_types::flags::value_is_off(&v) {
                0
            } else {
                Z_SATB_FILTER_FLUSH            }        }
        _ => Z_SATB_FILTER_FLUSH,    }}

/// `CRATONVM_ZGC_SATB_THREAD_BUFFERS` (zr z2/cc, default ON, `=0` kill), as
/// a construction seed: t1 wave 2 lane Z moved it from a process-wide
/// `OnceLock` (compatibility state is per VM, AGENTS.md) to the heap's
/// `satb_thread_buffers_on`, read on the store path with one relaxed load of
/// a field beside the others that path already reads -- the cost of the
/// `OnceLock`'s own state load. A heap built under a test's thread override
/// now honours it.
pub fn zgc_satb_thread_buffers_setting() -> bool {
    match None::<std::ffi::OsString> {
        Some(raw) => {
            let v = raw.to_string_lossy().trim().to_ascii_lowercase();
            !matches!(v.as_str(), "0" | "off" | "false" | "no")
        }
        None => true,
    }
}

pub fn zgc_conc_overlay_repair() -> bool {
    match None::<std::ffi::OsString> {
        Some(raw) => {
            let v = raw.to_string_lossy().trim().to_ascii_lowercase();
            !matches!(v.as_str(), "0" | "off" | "false" | "no")
        }
        None => true,
    }
}

/// What the marking cycle uses when `CRATONVM_ZGC_PARMARK` is unset: **`auto`
/// since zr z3/mt** (HotSpot's pause-worker ergonomic, [`zgc_nparallel_workers`];
/// `CRATONVM_ZGC_PARMARK=0` is the serial kill switch). Was `0` workers (the
/// serial loop) from 2026-08-14 to 2026-10-12.
///
/// # The flip (orchestrator, z2b host rows, round head `7829ddde88` + zr z2/cc's idle spill)
///
/// 4 runs per arm, 20 pauses each, `-Xmx2g --verbose:gc`, all rows SAME (checksums ok):
///
/// | row | serial (`PARMARK=0`) | `auto` |
/// |---|---|---|
/// | `Zr2CcParmarkShapes narrow 22 20` pause sum | 5.8-7.1 s | 2.3-2.8 s |
/// | same, wall | 6.9-8.5 s | 3.4-4.0 s |
/// | same, CPU user+sys | 7.7-9.5 s | 9.8-10.4 s |
/// | `Zr2CcParmarkShapes wide 22 20` pause sum | 5.5-7.1 s | 2.0-3.1 s |
/// | same, CPU | 7.9-9.3 s | 9.0-10.7 s |
///
/// `Zr1MkParmarkCost 23 30 0 -Xmx4g` (3-18 cycles per run): per pause serial
/// ~690-1150 ms, `auto` ~260-1130 ms, `=4` ~310-420 ms; CPU medians serial ~35 s,
/// `auto` ~41 s, `=4` ~42 s; `auto` with `CRATONVM_ZGC_MARK_IDLE_SPILL=0` worse
/// than with the spill. So ~2.5x lower pause and ~2x lower wall for +15-20 %
/// CPU, and HotSpot marks with parallel workers too. Pending: the load battery.
///
/// # The history this replaces (2026-08-14, kept because it is why the flip waited)
///
/// The serial loop was the default "and that is a measurement":
///
/// Phase 3's exit criterion is "pause time falls measurably on a heap with a
/// large live set". Measured on `probes/BigLive.java` (1,000,088 live objects,
/// `-Xmx1500m`, relocation off, three interleaved reps, mean of the last five
/// cycles each):
///
/// | workers | mean pause | vs serial |
/// |---:|---:|---:|
/// | 0 — this hand-written loop | **95.2 ms** | — |
/// | 1 — driven, one worker | 124.9 ms | +31% |
/// | 4 — driven | 240.6 ms | +153% |
/// | 8 — driven | ~304 ms | +219% |
///
/// The criterion is therefore **measured and failed**, in both directions:
/// adding workers makes the pause worse monotonically (contention, not
/// start-up cost — a fixed overhead would flatten and then improve), and even
/// a single-worker driven cycle costs a third of the pause over the loop it
/// replaces.
///
/// **A note on how nearly this shipped wrong.** A single un-interleaved run of
/// the one-worker arm measured 95.7 ms, i.e. free, and a default of `1` was
/// briefly justified on it. Three interleaved reps put it at 124.9 ms with a
/// spread of 2.7 ms — the original number was machine noise on a box that had
/// just finished a build. Interleave, or do not compare.
///
/// The driver is not deleted and Phase 3's other clause still holds: it is
/// reachable, correct, and exercised end-to-end through `collect_garbage` by
/// `the_concurrent_mark_driver_drives_a_real_collection`. It is simply not
/// what a user's pauses pay for until C5 of the concurrent+generational plan
/// makes the marker scale.
pub const Z_PARMARK_DEFAULT_AUTO: bool = true;

/// [`cratonvm_types::compact_object_field_storage`] without the `Arc`
/// clone/drop: the same two-step refusal (`is_compact_object`, then a layout
/// registered for the exact `(class_id, num_slots)`), served by reference from
/// the thread's layout cache ([`cratonvm_types::with_class_layout`]).
///
/// # Why the mutator accessors must not clone the handle (2026-09-18)
///
/// Every thread storing into instances of one class clones and drops the SAME
/// `Arc<CompactLayout>`, i.e. two atomic RMWs per field access on one shared
/// cache line. On one thread that is noise; on eight it is a coherence storm.
/// `PutScale` (eight threads, 20M stores each into their own compact `P`
/// objects, `jitr9-probes/gczgc3/src/PutScale.java`) ran **27.9 s** on ZGC with
/// the VM TLAB on against **1.07 s** on one thread, and the native profile of
/// the multi-threaded GC probe put **30 %** of busy samples on the refcount
/// traffic inside `class_layout_for_fields` and **19 %** in
/// [`ZgcRealHeap::set_field_no_card`] (the inlined drop). The layout is
/// immutable once registered, so a borrow is exactly as correct, and the cache
/// entry owns its own handle for the length of the closure.
///
/// It surfaced when the VM-thread TLAB (`zgc/vm_tlab.rs`) was measured for
/// default-on: that TLAB lays objects out compact, and every compiled store to
/// a compact object that the emitter does not inline reaches this lookup.
///
/// Since round 9 wave 4 (`gc4`) the shared lookup borrows too, through
/// `with_class_layout_versioned` (the small per-thread MRU cache, rather than
/// the collectors' dense per-`class_id` table this used to borrow from, which
/// every mutator thread would otherwise grow). This is now a thin alias kept
/// so the four call sites, and the test that pins the two forms equal, read as
/// before.
#[inline]
pub fn zgc_compact_field_storage(
    header: &ObjectHeader,
    index: usize,
) -> Option<(usize, cratonvm_types::FieldStorageKind)> {
    cratonvm_types::compact_object_field_disp(header, index)
}

/// `CRATONVM_DBG_ZGC_VERIFY_SLIDE` — walk every survivor's reference slots
/// after a compaction and report any that do not resolve to a registered live
/// base. See [`ZgcRealHeap::verify_no_dangling_slots_after_slide`].
/// `CRATONVM_DBG_ZGC_CORPSE` -- keep a one-cycle ledger of what each slide
/// vacated, and report it when a stale read lands on a zeroed span. See
/// [`ZgcRealHeap::report_corpse_read`].
/// `CRATONVM_ZGC_LARGE_ARRAY_SIZING` (zr z1/rl) -- whether
/// [`ZgcRealHeap::alloc_size`] sizes a real array whose payload exceeds
/// `MAX_PLAUSIBLE_BODY` (256 MiB) but whose length is within
/// `MAX_PLAUSIBLE_ARRAY_LEN`. **Default ON**; `=0` restores the old byte cap.
/// Under it such arrays are no longer leaked or skipped outright: they fall
/// to the screens built for arrays past `MAX_PLAUSIBLE_ARRAY_LEN`. A DEAD one
/// is freed through `oversize_array_extent` by both sweeps (`sweep_one_high`
/// since zr z1/al, `sweep_one` since zs s2/rf), and a LIVE one is sized by
/// `oversize_live_array_extent` only when its length is also past
/// `MAX_PLAUSIBLE_ARRAY_LEN` -- so a live array between 256 MiB of payload
/// and `1 << 28` elements still stops the slide and is skipped by the
/// rewrite pass (`zgc relocate: rewrite target is not walkable`) at `=0`.
///
/// Read once per process, like the other sizing-path switches: `alloc_size`
/// is a per-object call of the sweep and must stay a load and a branch.
///
/// zr z3/lt: deliberately LEFT a process latch (class (a) of
/// `heap_state`'s inventory): `alloc_size` is an associated fn with ~30
/// callers and no `&self`, and the switch is a corruption-screen kill switch
/// that no embedder has a reason to run differently per VM.
/// zr z4/ab: the extent of the array whose header is `header` at `base`
/// when it exceeds [`ZgcRealHeap::MAX_ALLOC_EXTENT`]; `None` for an object, a
/// length past `ZGC_REAL_MAX_ARRAY_LENGTH` (a clobbered header), an extent
/// within the window, or one that ends past `arena_end`. The screen the
/// huge-extent table applies to every candidate (`huge_array_extent`).
pub fn huge_array_extent_of(header: &ObjectHeader, base: usize, arena_end: usize) -> Option<usize> {
    if header.kind() != ObjectKind::Array {
        return None;
    }
    let len = header.array_length() as usize;
    if len > ZGC_REAL_MAX_ARRAY_LENGTH {
        return None;
    }
    let size = array_data_size(len, header.element_type())
        .ok()
        .and_then(|d| ARRAY_DATA_OFFSET.checked_add(d))?;
    if size <= ZgcRealHeap::MAX_ALLOC_EXTENT {
        return None;
    }
    let end = base.checked_add(size)?;
    (end <= arena_end).then_some(size)
}



/// `CRATONVM_ZGC_REWRITE_OVERSIZE_ARRAYS` (zs s1/dg, **default ON**; `0` /
/// `off` / `false` / `no` restores the skip): the slide's rewrite pass and the
/// reference-slot walk size a live array past `MAX_PLAUSIBLE_ARRAY_LEN`
/// through [`ZgcRealHeap::oversize_live_array_extent`] instead of skipping it
/// as unwalkable (rewrite) or omitting its elements (slot walk). Read once per
/// heap, in `ZgcRealHeap::with_capacity`. Positive control: the
/// `NativeGrowthReclaimProbe` guard lines ("rewrite target is not walkable",
/// `array_length=273366446`) come back at `=0`.
pub fn zgc_rewrite_oversize_arrays_enabled() -> bool {
    true
}

/// `CRATONVM_ZGC_OVERSIZE_SCREEN_MEMO` (zs s2/rf, **default ON**; `=0` kills):
/// [`ZgcRealHeap::oversize_live_array_extent`] memoises its answer per
/// `(base, array_length)` for the duration of one stop-the-world
/// `collect_garbage`. Each call scans `size / 8` bits of the registry (33 MB of
/// bitmap for a 2 GB array) and one cycle asks about the same array about
/// seven times (mark, census, partition, slide or screen, rewrite, two
/// `note_survivor`s, the sweep's accounting). Same answers: inside one pause
/// no base can appear inside a real array's extent (nothing allocates, and a
/// slide never places a survivor inside a sized obstacle). The sweep removes
/// bases: a removed neighbour can turn a refusal into an acceptance (a
/// memoised refusal is the conservative answer), and a removed array base
/// would make a memoised POSITIVE stale -- so a hit re-checks the array's own
/// registry bit and its extent from the header before answering `Some` (zs
/// s3/rv2; the key `(base, length)` does not name the element type). Read once per
/// heap. Engagement: `ZgcRealHeap::oversize_screen_memo_stats`.
pub fn zgc_oversize_screen_memo_enabled() -> bool {
    true
}

/// zs s2/rf: arms [`ZgcRealHeap::oversize_live_array_extent`]'s memo for one
/// `collect_garbage`, and empties it on every exit path (`Drop`).
pub struct OversizeScreenMemoGuard<'a> {
    pub heap: &'a ZgcRealHeap,
    pub armed: bool,
}

impl<'a> OversizeScreenMemoGuard<'a> {
    pub fn arm(heap: &'a ZgcRealHeap) -> Self {
        let c = &heap.counters;
        // A nested pause (none today) keeps the outer arming and does not
        // clear it on its own exit.
        let armed = c.oversize_screen_memo_on.load(Ordering::Relaxed)
            && !c.oversize_screen_memo_armed.swap(true, Ordering::AcqRel);
        if armed {
            c.oversize_screen_memo.lock().clear();
        }
        Self { heap, armed }
    }
}

impl Drop for OversizeScreenMemoGuard<'_> {
    fn drop(&mut self) {
        if self.armed {
            let c = &self.heap.counters;
            c.oversize_screen_memo.lock().clear();
            c.oversize_screen_memo_armed.store(false, Ordering::Release);
        }
    }
}



pub fn zgc_corpse_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(zgc_corpse_enabled_requested)
}

/// Uncached half of [`zgc_corpse_enabled`] (zt t6/rv3):
/// `CRATONVM_DBG_ZGC_CORPSE` read by value. It read presence, so `=0`
/// turned it on.
pub fn zgc_corpse_enabled_requested() -> bool {
    false}

/// `CRATONVM_DBG_WATCH_PUN=<class-name-substring>:<slot>` — the parsed watch.
///
/// A primitive stored into a slot the class declares a REFERENCE is the G30-1
/// species, and the read side can only report that it HAPPENED. This names the
/// WRITER: it prints a backtrace at the store, which on Linux is 40 symbolized
/// frames (see the note on `report_corpse_read`'s `backtrace` field — the
/// in-process capture works, contrary to what two comments used to claim).
///
/// Written for `SQLChar.rawData` — declared `[C`, found holding `Int(1)`, and
/// the cell a compiled `arraylength` dereferenced as the pointer 1. Watch it
/// with `CRATONVM_DBG_WATCH_PUN=SQLChar:1`.
/// Whether `CRATONVM_DBG_WATCH_PUN` is armed. The interpreter's quickened field
/// arms bypass `get_field` / `set_field`, where the watch reports, so they stay
/// off while it is armed (see `vm::runtime::interpreter::field_fast`).
#[inline]
pub fn punned_store_watch_armed() -> bool {
    punned_store_watch().is_some()
}

pub fn punned_store_watch() -> Option<&'static (String, usize)> {
    static W: std::sync::OnceLock<Option<(String, usize)>> = std::sync::OnceLock::new();
    W.get_or_init(|| {
        let spec = Err::<String, std::env::VarError>(std::env::VarError::NotPresent).ok()?;
        let (class, slot) = spec.rsplit_once(':')?;
        Some((class.to_string(), slot.parse().ok()?))
    })
    .as_ref()
}

/// Does this class id match the watched class substring? Memoized, because
/// both arms of the watch ask it once per access to the watched SLOT INDEX
/// on any class, and `class_name_for_diagnostics` builds a `String`.
pub fn punned_watch_class_matches(class_id: u32) -> bool {
    // LATCHED, and deliberately not a `Mutex<HashMap>`.
    //
    // The first version of this memo took a process-global mutex on every
    // access to the watched slot, and that alone made the defect it was built
    // to observe VANISH: 14 punned cells in 600 runs became 0 in 600, with the
    // failure count going 16 -> 0 as well. A watch that serialises the thing it
    // is watching is not an instrument. (That the cell is racy is worth
    // knowing, and is recorded on the page -- but it has to be learned from a
    // measurement, not from the measurement's own overhead.)
    //
    // One atomic load and a compare once the watched class has been seen once.
    // Only the FIRST matching class latches, which is enough for a watch spec
    // that names one class; a substring matching two classes reports the first.
    static WATCHED_CID: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(u32::MAX);
    let latched = WATCHED_CID.load(std::sync::atomic::Ordering::Relaxed);
    if latched != u32::MAX {
        return class_id == latched;
    }
    let Some((want_class, _)) = punned_store_watch() else {
        return false;
    };
    if cratonvm_gc_core::collector::class_name_for_diagnostics(class_id).contains(want_class.as_str()) {
        WATCHED_CID.store(class_id, std::sync::atomic::Ordering::Relaxed);
        return true;
    }
    false
}

/// The four numbers `CRATONVM_DBG_WATCH_PUN` reports at exit.
///
/// The two DENOMINATORS are the point. A run of the watch that reports no
/// punned cell has said nothing until it also says how many times it looked:
/// the experiment this watch exists for turns the JIT off to ask whether
/// compiled code is the writer, and that also removes every COMPILED read and
/// write of the slot. "No punned read" then has two readings -- the cell was
/// never created, or nobody looked -- and only the denominator separates them.
/// 2026-08-27: 600 `--nojit` runs reported zero punned cells with
/// `reads=0`, which is the second reading and was very nearly read as the
/// first.
pub static PUN_WATCH_READS: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);
pub static PUN_WATCH_READS_PUNNED: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);
pub static PUN_WATCH_READS_PUNNED_NONZERO: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);
pub static PUN_WATCH_STORES: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);
pub static PUN_WATCH_STORES_PRIMITIVE: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

/// Print the watch census, or say plainly that the watch was never set.
///
/// Prints even when every counter is zero, for the reason on
/// [`PUN_WATCH_READS`]: the absence of a line cannot be told from a zero.
pub fn report_punned_watch_at_exit() {
    let Some((class, slot)) = punned_store_watch() else {
        return;
    };
    use std::sync::atomic::Ordering::Relaxed;
    eprintln!(
        "[WATCH-PUN] EXIT watch={class}:{slot} accessor_reads={} accessor_reads_punned={} \
accessor_reads_punned_nonzero={} accessor_stores={} accessor_stores_primitive={}",
        PUN_WATCH_READS.load(Relaxed),
        PUN_WATCH_READS_PUNNED.load(Relaxed),
        PUN_WATCH_READS_PUNNED_NONZERO.load(Relaxed),
        PUN_WATCH_STORES.load(Relaxed),
        PUN_WATCH_STORES_PRIMITIVE.load(Relaxed),
    );
}

pub fn zgc_verify_slide_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(zgc_verify_slide_enabled_requested)
}

/// Uncached half of [`zgc_verify_slide_enabled`] (zt t6/rv3):
/// `CRATONVM_DBG_ZGC_VERIFY_SLIDE` read by value. It read presence, so `=0`
/// turned it on.
pub fn zgc_verify_slide_enabled_requested() -> bool {
    false}

/// Below this share of capacity free, a small largest-block means the heap is
/// **full**, not fragmented — so the fragmentation gauge does not sample.
///
/// This condition is the difference between an instrument and a number. The
/// quantity Phase 2.2 asks to track is `largest_free_block / capacity`, and on
/// its own that quantity falls to nearly zero in two completely different
/// states: an arena broken into crumbs (which is the problem) and an arena
/// genuinely full of live objects (which is not — it is what a heap is for).
/// A gauge that cannot separate them would fire on every workload that uses
/// its heap, get muted, and then be worth nothing on the day it was right.
/// 250 permille — a quarter of the heap free — is where "there are plenty of
/// bytes, they are just not contiguous" becomes the only reading available.
pub const ZGC_FRAG_GAUGE_MIN_FREE_PERMILLE: usize = 250;

/// The ratchet's floor: one warning per process when the largest block a
/// sampled collection could hand out falls below 1% of capacity.
///
/// Sized against what it is protecting rather than picked: `ZGC_TLAB_MAX_CHUNK`
/// is 512 KiB, so on any heap up to 50 MiB a 1% largest block cannot serve even
/// one full TLAB chunk, and above that it cannot serve the large-object end's
/// first request. It is a floor, not a target — a healthy run does not
/// approach it, and the number to watch is the reported worst, not this.
pub const ZGC_FRAG_FLOOR_PERMILLE: usize = 10;

/// HotSpot ZGC's pause-worker ergonomic (`ZHeuristics::nparallel_workers`):
/// 60% of the CPUs rounded up, capped so the workers' relocation share stays
/// within 2% of the heap at one 2 MiB small page each, and never below one.
/// What `CRATONVM_ZGC_PARMARK=auto` asks for (zr z1/mk).
///
/// | cpus | heap | workers |
/// |---:|---:|---:|
/// | 1 | 4 GiB | 1 |
/// | 4 | 4 GiB | 3 |
/// | 8 | 4 GiB | 5 |
/// | 16 | 4 GiB | 10 |
/// | 8 | 256 MiB | 2 |
/// | 8 | 64 MiB | 1 |
pub fn zgc_nparallel_workers(cpus: usize, heap_bytes: usize) -> usize {
    const SMALL_PAGE: usize = 2 * 1024 * 1024;
    let by_cpu = (cpus.max(1) * 60).div_ceil(100);
    let by_heap = heap_bytes / 50 / SMALL_PAGE;
    by_cpu.min(by_heap).max(1)
}

/// HotSpot ZGC's concurrent-worker ergonomic (`ZHeuristics::nconcurrent_workers`):
/// 25 % of the CPUs rounded up (12.5 % with `-XX:-UseDynamicNumberOfGCThreads`),
/// under the same 2 %-of-the-heap cap as [`zgc_nparallel_workers`], never below
/// one. The default for [`ZgcRealHeap::conc_mark_workers`] since zr z3/mt.
///
/// Measured against HotSpot 25.0.3 on an 8-CPU box (`-Xlog:gc+init`,
/// `GC Workers for Old Generation:`): `-Xmx512m` -> 2 (dynamic), 1 with
/// `-XX:-UseDynamicNumberOfGCThreads`; `-Xmx64m` -> 1.
///
/// | cpus | heap | dynamic | workers |
/// |---:|---:|:---:|---:|
/// | 8 | 512 MiB | yes | 2 |
/// | 8 | 512 MiB | no | 1 |
/// | 8 | 64 MiB | yes | 1 |
/// | 16 | 4 GiB | yes | 4 |
/// | 32 | 4 GiB | yes | 8 |
pub fn zgc_nconcurrent_workers(cpus: usize, heap_bytes: usize, dynamic: bool) -> usize {
    const SMALL_PAGE: usize = 2 * 1024 * 1024;
    // `ceil(cpus * share / 100)` with share 25 or 12.5, in integers.
    let by_cpu = if dynamic {
        cpus.max(1).div_ceil(4)
    } else {
        cpus.max(1).div_ceil(8)
    };
    let by_heap = heap_bytes / 50 / SMALL_PAGE;
    by_cpu.min(by_heap).max(1)
}

/// `CRATONVM_ZGC_GC_THREAD_FLAGS` (zr z3/mt): honour `-XX:ParallelGCThreads`,
/// `-XX:ConcGCThreads` and `-XX:±UseDynamicNumberOfGCThreads` on ZGC, and size
/// the concurrent mark pool by HotSpot's ergonomic when `ConcGCThreads` is not
/// given. **Default on**; `0` / `off` / `false` / `no` restores the state before
/// (both flags inert on ZGC, concurrent pool `cores / 4` clamped to 1..4).
/// Read per call (once per heap set-up or concurrent mark start).
pub fn zgc_gc_thread_flags_enabled() -> bool {
    match None::<std::ffi::OsString> {
        Some(raw) => {
            let v = raw.to_string_lossy().trim().to_ascii_lowercase();
            !matches!(v.as_str(), "0" | "off" | "false" | "no")
        }
        None => true,
    }
}

/// The GC worker-count flags one heap was given (zr z3/mt). Zero = not given.
/// Atomics only so the setter can take `&self` on a shared heap; written once
/// at VM start-up.
#[derive(Debug, Default)]
pub struct ZGcThreadFlags {
    pub parallel: AtomicUsize,
    pub conc: AtomicUsize,
    /// `-XX:-UseDynamicNumberOfGCThreads` given (HotSpot's default is on).
    pub dynamic_off: AtomicBool,
}

impl ZGcThreadFlags {
    pub fn store(&self, parallel: Option<usize>, conc: Option<usize>, dynamic: Option<bool>) {
        self.parallel
            .store(parallel.unwrap_or(0), Ordering::Relaxed);
        self.conc.store(conc.unwrap_or(0), Ordering::Relaxed);
        self.dynamic_off
            .store(dynamic == Some(false), Ordering::Relaxed);
    }

    /// `ParallelGCThreads`, `None` when not given or the kill switch is set.
    pub fn parallel(&self) -> Option<usize> {
        let n = self.parallel.load(Ordering::Relaxed);
        (n > 0 && zgc_gc_thread_flags_enabled()).then_some(n)
    }

    /// `ConcGCThreads`, as [`Self::parallel`].
    pub fn conc(&self) -> Option<usize> {
        let n = self.conc.load(Ordering::Relaxed);
        (n > 0 && zgc_gc_thread_flags_enabled()).then_some(n)
    }

    /// `UseDynamicNumberOfGCThreads` (HotSpot default: true).
    pub fn dynamic(&self) -> bool {
        !self.dynamic_off.load(Ordering::Relaxed)
    }
}

/// The STW mark loop's per-object cost switches (zr z3/mt,
/// `zgc-zr1mk-proposal-mark-loop-costs-20261012.md`). Every arm marks the SAME
/// set; they differ only in how many loads and calls an object or an edge costs.
///
/// * `ref_disps` -- `CRATONVM_ZGC_MARK_REF_DISPS`, **default ON** (`=0` kill):
///   a compact object's edges come from its layout's oop-map (`ref_disps`)
///   instead of a zip of every field with its `is_ref` bit. Never more work
///   (one array instead of two, no per-primitive-field branch).
/// * `layout_memo` -- `CRATONVM_ZGC_MARK_LAYOUT_MEMO`, **default ON** (`=0`
///   kill): the serial marker keeps the last class's oop-map ([`ZLayoutMemo`]),
///   so a run of one class (a list, a tree, a node table) skips
///   `with_class_layout` -- a layout-generation load, a TLS access, a `RefCell`
///   borrow and a cache probe per object. A miss costs the copy of a few `u32`s
///   on top of the lookup it would have made anyway.
/// * `test_mark_first` -- `CRATONVM_ZGC_MARK_TEST_FIRST`, **default ON** (`=0`
///   kill): an edge whose target's mark bit is already set is dropped before the
///   registry screen. Both screens are bitmap words gridded over the arena (a
///   cache miss each on a large heap); an already-marked target -- every edge
///   but the first into a shared object -- now costs one. A set bit means the
///   edge would have been dropped either way (claimed already, or not a base:
///   only `wild_skipped` can count lower). Bitmap arm only: the header arm
///   would read a header through an unscreened address.
///
/// (zt t12/rt: the opt-in push-time header prefetch,
/// `CRATONVM_ZGC_MARK_PREFETCH`, gained nothing outside the noise and was
/// retired.)
#[derive(Debug)]
pub struct ZMarkLoopOpts {
    pub ref_disps: AtomicBool,
    pub layout_memo: AtomicBool,
    pub test_mark_first: AtomicBool,
    /// zr zs s1/mc, `CRATONVM_ZGC_MARK_ONE_DECODE` (default ON, `=0` kill):
    /// every marker (the serial loop, and the engine's pools -- stop-the-world
    /// AND concurrent, where it is sound for the same reason: a leaf has no
    /// slots and cannot be a `Reference`; corrected zs s2/mc2, after zs
    /// s2/rv's nit) decodes an object's header ONCE and sends a shape with no
    /// reference slots (primitive array, humongous filler) straight to its
    /// per-object pin / overlay edges, skipping the referent-hole lookup
    /// (skip-set bloom on the engine, `skip_for` on the serial loop) and the
    /// strong walk that would report nothing. Same edges, same order. See
    /// `ZgcRealHeap::visit_refs_chunked` and, on the default direct scan,
    /// `scan_and_claim`.
    pub one_decode: AtomicBool,
    /// zr zs s1/mc, `CRATONVM_ZGC_MARK_CPU_ACCOUNTING` (default ON, `=0`
    /// kill): the serial marker reads its thread CPU clock around each call,
    /// as the engine's workers do around their busy stretches
    /// ([`mark::mark_cpu_accounting_enabled`]). Telemetry only.
    pub cpu_accounting: AtomicBool,
    /// zt t3/mk (X1-11 of `zgc-zt2x1-wave-t1-review-residuals`): the serial
    /// marker's reference-array chunk ([`mark::ref_chunk_budget`]), read once
    /// per HEAP here rather than at every `mark_closure_serial_body` entry --
    /// the finalizer-resurrection pass enters it once per dead finalizable
    /// candidate, so the t1 unlatching put a flag-snapshot lookup per
    /// candidate inside the pause. Per heap, not per process (the zs2mc2
    /// residual-9 rule), like every other field here.
    pub ref_chunk: AtomicUsize,
}

impl ZMarkLoopOpts {
    pub fn from_env() -> Self {
        let on = |key: &str| cratonvm_types::flags::runtime_flag_default_on(key);
        ZMarkLoopOpts {
            ref_disps: AtomicBool::new(on("CRATONVM_ZGC_MARK_REF_DISPS")),
            layout_memo: AtomicBool::new(on("CRATONVM_ZGC_MARK_LAYOUT_MEMO")),
            test_mark_first: AtomicBool::new(on("CRATONVM_ZGC_MARK_TEST_FIRST")),
            one_decode: AtomicBool::new(on("CRATONVM_ZGC_MARK_ONE_DECODE")),
            cpu_accounting: AtomicBool::new(mark::mark_cpu_accounting_enabled()),
            ref_chunk: AtomicUsize::new(mark::ref_chunk_budget()),
        }
    }
}

/// The serial marker's one-entry oop-map memo (zr z3/mt, see
/// [`ZMarkLoopOpts`]): the last compact class's `ref_disps`, COPIED (a few
/// `u32`s), so nothing borrowed from the per-thread layout cache outlives the
/// call that produced it. Lives for one `mark_closure_serial` call.
#[derive(Default)]
pub struct ZLayoutMemo {
    pub key: Option<(u32, u32)>,
    pub ref_disps: Vec<u32>,
    pub hits: usize,
    pub misses: usize,
}

/// Hard ceiling on parallel-mark workers, whatever `CRATONVM_ZGC_PARMARK` and
/// the core count say.
///
/// Bounds a thread count, and the thing it bounds is a user-supplied integer —
/// i.e. unbounded — which is the shape the Phase 2.3 constant audit exists to
/// catch. 64 is far above any plausible collection-time parallelism and far
/// below a number that would exhaust the OS thread limit inside a safepoint.
///
/// zt t6/ob4 (`zgc-zr4rv4-mark-pool-default-residuals-DECIDED-20261019.md` item 3):
/// kept, and kept silent, by decision. HotSpot 25.0.3 runs the count it is
/// given (local `-XX:+UseZGC -Xlog:gc+init` rows: `-XX:ParallelGCThreads=100`
/// prints `Runtime Workers: 100`, `-XX:ConcGCThreads=0x1k` prints `GC Workers
/// Max: 1024 (dynamic)`, `-XX:ConcGCThreads=1g` dies with a fatal error report
/// creating the threads), so its Java-visible figures are the flag values:
/// this heap stores both counts unclamped
/// ([`ZgcRealHeap::set_gc_worker_threads`]), and those are what `-Xlog:gc+init`
/// and JFR report. GC workers are not Java threads, so the pools' own size is
/// not Java-visible; this cap bounds them only (a `1g` count does not crash
/// this VM), and a stderr line would be output HotSpot does not print.
/// Pinned by `zt6ob4_gc_thread_counts_are_stored_unclamped_and_pools_capped`.
pub const Z_PARMARK_MAX_WORKERS: usize = 64;

/// Restart budget for a stop-the-world parallel mark. See
/// [`ZgcRealHeap::mark_parallel_stw`] for why this is 1 and not 0.
pub const Z_PARMARK_RESTART_BUDGET: usize = 1;

/// Default for `CRATONVM_ZGC_CONC_START`: the percentage of the collection
/// threshold at which a CONCURRENT mark cycle opens. `0` is OFF.
///
/// # Why the default is 0, on a feature that works
///
/// Concurrent marking is built, reachable end to end and correct -- and the
/// measurement says it is not ready to be what every user's workload pays for.
/// Interleaved, 3 reps, 8-core Azure box, 2026-08-16:
///
/// | probe | arm | cycles | mean pause | wall clock |
/// |---|---|---:|---:|---:|
/// | single-threaded | stop-the-world | 6 | 370 ms | 5.35 s |
/// | single-threaded | concurrent, 1 worker | **12** | **230 ms** | 8.31 s |
/// | single-threaded | concurrent, 2 workers | **12** | 254 ms | 9.70 s |
/// | 8 mutator threads | stop-the-world | 3 | 609 ms | 11.8 s |
/// | 8 mutator threads | concurrent, 1 worker | 5 | 388 ms | 16.8 s |
/// | 8 mutator threads | concurrent, 2 workers | 5 | **253 ms** | 16.2 s |
///
/// The per-cycle pause falls 38-58%, which is the property the phase was
/// written for and it is real. Two things beside it are not acceptable as a
/// default:
///
/// * **The cycle COUNT roughly doubles** (6 -> 12, 3 -> 5). Everything
///   allocated after mark start is floating garbage for that cycle, so each
///   collection reclaims less and the next arrives sooner. On the
///   single-threaded probe that turns a 38% per-cycle win into a *worse* total
///   pause (2.22 s -> 2.76 s); on the multi-threaded one the total still
///   improves (1.83 s -> 1.27 s).
/// * **Wall clock rises 37-55%.** The mark workers compete with mutators that
///   already saturate the box, and the store and allocation paths grew work:
///   22.4M allocate-black claims and 1.4M SATB publications on the
///   single-threaded probe, 16.5M and 4.1M on the multi-threaded one.
///
/// This tree has shipped a ZGC marking feature default-ON verified only for
/// correctness before -- parallel STW marking, 2026-08-14, which cost +31% at
/// one worker and +153% at four and was reverted. The lesson is written down;
/// this is it being followed. `CRATONVM_ZGC_CONC_START=60` opts in, and the
/// numbers above say which workloads that is a good trade for: many mutator
/// threads, a large live set, and pause sensitivity.
pub const Z_CONC_START_PERCENT_DEFAULT: usize = 0;

/// Mark-end restart budget for the CONCURRENT cycle's closing handshake.
///
/// Larger than [`Z_PARMARK_RESTART_BUDGET`] and for a different reason. That
/// budget covers a stop-the-world mark, where a restart can only come from the
/// engine's own bookkeeping. This one closes a cycle whose ingress may have
/// been fed by every mutator in the VM right up to the safepoint, so the first
/// `try_end_mark` after the replay has real work to fold in more often. Still
/// bounded: mutators are stopped here, so the producer set is empty and the
/// loop provably converges -- the budget is a hang-stop, not a policy.
pub const Z_CONC_MARK_END_RESTART_BUDGET: usize = 8;

/// How many SATB publications accumulate before the mutator hands the batch to
/// the concurrent marker. See
/// [`ZgcRealHeap::hand_satb_batch_to_the_marker`].
///
/// Large enough that the pool mutex is untouched on all but 1 store in 8192,
/// small enough that the ingress cannot grow past ~64 KiB per handoff window
/// however hard the workload stores.
pub const Z_SATB_HANDOFF_INTERVAL: usize = 8192;

// `CRATONVM_ZGC_CONC_START` -- the percentage of the collection threshold at
// which a concurrent mark cycle opens -- read once per process.
//
// A free function rather than a method because the constructor needs it
// before there is a `self` to call it on: the answer is turned into
// `ZgcRealHeap::conc_start_bytes` there, so nothing on the allocation path
// ever divides.
//
// # What the number means, at either end
//
// At **100** the cycle would open at the instant the collection is needed and
// there would be no concurrent phase at all. The gap between the setting and
// 100 is the whole of the time the marker gets to run with mutators alive, so
// it has to be big enough to hold one transitive closure of the live set at
// the allocation rate the workload actually has.
//
// Going much **lower** is not free either: everything allocated after mark
// start is floating garbage for that cycle (allocate-black), and the SATB
// barrier is armed for the whole window. Opening at 10% would mark a heap
// that is mostly about to become garbage, and arm the barrier for most of the
// program's life.
//
// `60` -- i.e. 45% of capacity at the default `ZGC_REAL_GC_THRESHOLD_PERCENT`
// of 75 -- is the value the 2026-08-16 measurement was taken at, and the one
// to opt in with. `0` is the default and means never; see
// [`Z_CONC_START_PERCENT_DEFAULT`] for the numbers behind that.
// zr z4/ab (rv4 X2): uncached, like `conc_start_is_adaptive` -- both are
// construction seeds (`conc_start_bytes`, `conc_start_adaptive`).
pub fn conc_start_percent_setting() -> usize {
    {
        if conc_start_is_adaptive() {
            // `auto` opens the cycle wherever the last cycle's measurements say
            // it should, and `ZgcRealHeap::refresh_adaptive_conc_start` owns
            // the number from the first collection onwards. This seeds it, and
            // it seeds it at the value that measurement was taken at.
            return 60;
        }
        match None::<std::ffi::OsString>
            .and_then(|v| v.to_str().and_then(|s| s.trim().parse::<usize>().ok()))
        {
            Some(p) => p.min(100),
            None => Z_CONC_START_PERCENT_DEFAULT,
        }
    }
}

/// Was `CRATONVM_ZGC_CONC_START` set to a number that parses to 0 (the kill
/// spelling, as opposed to unset)? zr z11/rv9.
pub fn conc_start_explicitly_zero() -> bool {
    None::<std::ffi::OsString>
        .and_then(|v| v.to_str().and_then(|s| s.trim().parse::<usize>().ok()))
        == Some(0)
}

/// The construction seed of `conc_start_bytes`, in percent of the threshold
/// (zr z11/rv9, every-trigger residual 3). `setting` is
/// [`conc_start_percent_setting`]'s answer; under
/// `CRATONVM_ZGC_CONC_EVERY_TRIGGER=1` a zero setting is seeded to 60 so the
/// arm reads as configured -- except an explicit `CRATONVM_ZGC_CONC_START=0`,
/// which the round's docs name as the kill and which the z3 seed overrode.
/// `zero_kills = false` (`CRATONVM_ZGC_CONC_START_ZERO_KILLS=0`) restores the
/// z3 seed. Pure so it is testable despite the construction-time reads.
pub fn conc_start_seed_percent(
    setting: usize,
    explicit_zero: bool,
    every_trigger: bool,
    zero_kills: bool,
) -> usize {
    if setting == 0 && every_trigger && !(explicit_zero && zero_kills) {
        60
    } else {
        setting
    }
}

/// Is `CRATONVM_ZGC_CONC_START` set to `auto`?
///
/// The opt-in for the ADAPTIVE window -- see
/// [`ZgcRealHeap::refresh_adaptive_conc_start`] for why a fixed percentage is
/// the reason concurrent marking measures worse in total pause than a
/// stop-the-world mark, despite winning 38-58% per cycle.
///
/// A separate spelling rather than a separate variable so there is one place to
/// look for "when does a cycle open", and so `=60` still means exactly what it
/// meant.
pub fn conc_start_is_adaptive() -> bool {
    None::<std::ffi::OsString>
        .map(|v| v.to_string_lossy().trim().eq_ignore_ascii_case("auto"))
        .unwrap_or(false)
}

/// [`Z_SATB_HANDOFF_INTERVAL`] expressed per ingress bucket.
///
/// The handoff decision is made from a bucket's own push count (see
/// `satb_pre_barrier_slow`), so the interval has to be divided by the number of
/// buckets or handoffs would become `Z_MARK_INGRESS_BUCKETS` times rarer than
/// intended -- and everything still in the ingress when the collection arrives is
/// traced INSIDE the pause, which is the cost the batching exists to avoid.
///
/// Floored at 1: a bucket count above the interval would otherwise make this 0
/// and `n % 0` panics.
pub const Z_SATB_HANDOFF_PER_BUCKET: usize =
    if Z_SATB_HANDOFF_INTERVAL / mark::Z_MARK_INGRESS_BUCKETS > 0 {
        Z_SATB_HANDOFF_INTERVAL / mark::Z_MARK_INGRESS_BUCKETS
    } else {
        1
    };

/// A stable per-thread ingress slot, so SATB pushes distribute by THREAD.
///
/// # Why not by address, which is what this used
///
/// `ZMarkIngress` buckets its queues across [`mark::Z_MARK_INGRESS_BUCKETS`]
/// mutexes so mutators publishing SATB work land on different locks. The key was
/// `old_addr >> 3`, which distributes over the HEAP -- so two threads storing
/// into the same region collide, and every thread visits all 16 buckets in turn
/// and takes 16 cold locks instead of one warm one. Distributing by *thread*
/// gives each mutator its own bucket, which is what "per-thread mark buffers"
/// (C4) is actually asking for.
///
/// # Why this is a `usize` and not the buffer itself
///
/// C4's stated shape is a per-thread [`mark::ZMarkMutatorBuffer`], which removes
/// the lock entirely. It also introduces a lifetime hazard this does not: a
/// detached buffer dropped non-empty leaves objects **marked and unscanned**, and
/// the mark bit is what dedups them, so nothing will ever scan them again — a
/// use-after-free at thread exit. Getting that right needs the buffer's `Drop` to
/// flush into a heap that may already be gone, i.e. a `Weak` upgrade and a
/// registration protocol.
///
/// A thread-local holding one `usize` has no `Drop`, cannot outlive anything, and
/// captures the distribution half of the benefit. (zs s3/ac: the buffer half
/// exists since zr z2/cc as `mark::ZSatbThreadBuffers`, heap-keyed and drained
/// at the mark end; this slot still keys the ingress bucket of both arms.)
///
/// Values are handed out by a process-wide counter and never reused, which is
/// fine: only `slot & mask` is consulted, so the counter wrapping would merely
/// re-collide and never be wrong.
pub fn satb_ingress_slot() -> usize {
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    thread_local! {
        static SLOT: usize = NEXT.fetch_add(1, Ordering::Relaxed);
    }
    SLOT.with(|s| *s)
}



/// `CRATONVM_ZGC_SOFT_MAX_ZERO_HOTSPOT` -- default ON, `=0` kill (zt t7/x4b):
/// a `-XX:SoftMaxHeapSize` (or management write) of 0 is a soft maximum of
/// 0, as on HotSpot 25.0.3; `=0`: it means the capacity. Read per write
/// ([`ZgcRealHeap::set_soft_max_heap_bytes`]); the launcher reads the same
/// key to accept the 0.
pub fn zgc_soft_max_zero_hotspot_enabled() -> bool {
    true
}

/// zt t6/ob4: the VM's own share of one pause span, from `(thread CPU,
/// pause_thread share)` at its start and end: the thread's CPU across the
/// span less what the heap booked to `pause_thread_ns` meanwhile (that part
/// is already in a share), never negative. Pure.
pub fn zgc_vm_pause_share(start: (u64, u64), end: (u64, u64)) -> u64 {
    let thread = end.0.saturating_sub(start.0);
    let heap = end.1.saturating_sub(start.1);
    thread.saturating_sub(heap)
}

/// zs s3/ac: a heap's accounted collection CPU by owner, thread CPU
/// nanoseconds, cumulative ([`ZgcRealHeap::collection_cpu_totals`]).
///
/// # Which shares nest
///
/// Each figure is one set of threads (or, for `vm_pause_ns`, one span of a
/// thread's time), and a thread's CPU is in exactly one of the DISJOINT
/// shares [`Self::collection_ns`] adds (zt t7/x4: the text said "four"; there
/// are seven since t3's uncommit share and t6's VM-pause share):
///
/// * `pause_thread_ns` -- the collecting thread (a Java thread: the mutator
///   that hit the trigger) across the whole of `collect_garbage`. The two
///   mark figures that run on that thread, `mark_stw_driver_ns` and
///   `mark_serial_ns`, are INSIDE it and are printed only to split it.
/// * `mark_stw_pool_ns` / `mark_conc_pool_ns` -- the mark pools' workers.
/// * `sweep_pool_ns` -- the parallel sweep's spawned workers.
/// * `relocate_pool_ns` -- the spawned pullers of the slide passes.
/// * `uncommit_ns` -- the `ZUncommitter` thread, between collections (zt
///   t3/mk; HotSpot's `ZUncommitter` is a GC thread). Not part of any pause,
///   so the per-cycle `[GC] zgc-pause-cpu:` delta leaves it out.
/// * `vm_pause_ns` -- the collecting thread OUTSIDE `collect_garbage` but
///   inside the VM's pause (safepoint request and rendezvous, the common
///   epilogue: zt t6/ob4), booked after the per-cycle line, so that line
///   leaves it out too. The one share that is not a set of threads of its
///   own: it is the collecting thread's OTHER time, disjoint from
///   `pause_thread_ns` by subtraction (`zgc_vm_pause_share` takes the
///   thread's CPU across the VM's span less the `pause_thread_ns` booked
///   inside it), which is sound because only the thread that won the pause
///   books `pause_thread_ns` while its span is open (a losing requester
///   drops its span unbooked).
///
/// Printed beside them but in NO share (zt t3/mk): `satb_flush_ns`, the SATB
/// buffer flushes on the mutators -- Java-thread CPU, the flush half of the
/// barrier's cost.
///
/// Not measured at all: the per-store half of the SATB barrier.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ZCollectionCpuTotals {
    pub mark_stw_pool_ns: u64,
    pub mark_stw_driver_ns: u64,
    pub mark_serial_ns: u64,
    pub mark_conc_pool_ns: u64,
    pub sweep_pool_ns: u64,
    pub relocate_pool_ns: u64,
    pub uncommit_ns: u64,
    pub vm_pause_ns: u64,
    pub pause_thread_ns: u64,
    pub pause_cycles: u64,
    pub satb_flush_ns: u64,
}

impl ZCollectionCpuTotals {
    /// The collection's CPU with no thread counted twice: the collecting
    /// thread plus every pool, plus the uncommitter. See the type's doc.
    pub fn collection_ns(&self) -> u64 {
        self.pause_thread_ns
            .saturating_add(self.mark_stw_pool_ns)
            .saturating_add(self.mark_conc_pool_ns)
            .saturating_add(self.sweep_pool_ns)
            .saturating_add(self.relocate_pool_ns)
            .saturating_add(self.uncommit_ns)
            .saturating_add(self.vm_pause_ns)
    }

    /// The shutdown line, `[GC] zgc-cpu: ...`. `process_ns` is the whole
    /// process's CPU at the time of the print (`None`: no process clock, the
    /// share prints `-`).
    pub fn summary_line(&self, process_ns: Option<u64>, accounting: bool) -> String {
        let ms = |ns: u64| ns / 1_000_000;
        let collection = self.collection_ns();
        let (process, pct) = match process_ns {
            Some(p) if p > 0 => (
                ms(p).to_string(),
                format!("{:.1}", collection as f64 * 100.0 / p as f64),
            ),
            _ => ("-".to_string(), "-".to_string()),
        };
        format!(
            "[GC] zgc-cpu: mark_stw_pool_ms={} mark_stw_driver_ms={} mark_serial_ms={} \
             mark_conc_pool_ms={} sweep_pool_ms={} relocate_pool_ms={} uncommit_ms={} \
             pause_thread_ms={} collection_ms={} process_ms={process} collection_pct={pct} \
             pause_cycles={} accounting={} satb_flush_ms={} vm_pause_ms={}",
            ms(self.mark_stw_pool_ns),
            ms(self.mark_stw_driver_ns),
            ms(self.mark_serial_ns),
            ms(self.mark_conc_pool_ns),
            ms(self.sweep_pool_ns),
            ms(self.relocate_pool_ns),
            ms(self.uncommit_ns),
            ms(self.pause_thread_ns),
            ms(collection),
            self.pause_cycles,
            accounting as u8,
            ms(self.satb_flush_ns),
            ms(self.vm_pause_ns),
        )
    }
}

/// Words in the skip-set Bloom filter -- see
/// [`ZgcRealHeap::mark_ref_skip_bloom`]. 512 words is 32768 bits and 4 KiB per
/// heap; with 1000 `Reference` objects and two bits each the false-positive rate
/// is about 0.35%, so roughly one object in 285 falls through to the lock.
pub const Z_SKIP_BLOOM_WORDS: usize = 512;

/// `CRATONVM_ZGC_GENERATIONAL` -- run young-only collections. **Default off.**
///
/// # Why it is off, on a collector whose measured problem it is meant to fix
///
/// The 2026-08-17 pause anatomy named the reclaim rate as the number that
/// decides ZGC's total pause: concurrent marking halved it (54.3% of the
/// registry to 27.1%), because everything allocated during a cycle is floating
/// garbage, so the cycle count doubled and the total pause got worse even where
/// the per-cycle pause improved by a third. Generational is the structural
/// answer, and this flag is how that claim gets measured rather than assumed.
///
/// Off by default because this tree has been bitten by the opposite order: ZGC's
/// parallel marking shipped default-ON verified only for correctness and
/// engagement, and cost +31% pause at one worker and +153% at four. The exit
/// criterion here is a measurement on a real workload -- a young cycle doing
/// strictly less work than a full one AND a total pause that improves.
///
/// # What turning it on changes
///
/// * Every reference store cards its receiver once that receiver is old
///   ([`ZgcRealHeap::note_ref_store`]). Until the first promotion this is one
///   relaxed load.
/// * The sweep ages every survivor and cards it on the collection that promotes
///   it.
/// * Every [`zgc_gen_minors_per_major`]th collection is whole-heap, as are
///   collections driven by allocation failure and collections whose mark set
///   came from a concurrent cycle (that mark set is whole-heap already).
/// * `CRATONVM_ZGC_RELOCATE` still applies; a relocation that MOVES an object
///   re-cards the old generation from the post-slide live set.
pub fn zgc_generational_enabled() -> bool {
    match None::<std::ffi::OsString> {
        Some(raw) => {
            let v = raw.to_string_lossy().trim().to_ascii_lowercase();
            !matches!(v.as_str(), "" | "0" | "off" | "false" | "no")
        }
        None => false,
    }
}

/// `CRATONVM_ZGC_GEN_PROMOTION_AGE` -- collections an object must survive
/// before it is old. Default [`generation::Z_DEFAULT_PROMOTION_AGE`] (3), the
/// figure the `GenerationalZgc` simulation and [`crate::gen_heap`] both use.
///
/// Clamped into `1..=MAX_GC_AGE` where it is read: 0 would promote every object
/// on its first collection, and anything above 15 would promote none at all,
/// because [`ObjectHeader::set_gc_age`] saturates on a 4-bit field.
pub fn zgc_gen_promotion_age() -> u32 {
    match None::<String>
        .and_then(|v| v.trim().parse::<u32>().ok())
    {
        Some(a) => a,
        None => generation::Z_DEFAULT_PROMOTION_AGE,
    }
}

/// `CRATONVM_ZGC_GEN_NURSERY_PERCENT` -- the nursery's size as a percentage of
/// heap capacity. Default 10; `0` is the kill switch and restores the
/// whole-heap-only trigger.
///
/// # Why a young cycle needs a trigger of its own
///
/// `needs_gc`'s two clauses are both about the WHOLE HEAP -- live bytes against
/// `gc_threshold`, and allocatable space via `headroom_low`. Neither asks whether
/// enough has been allocated *since the last collection* to be worth a young
/// cycle, so before this a young cycle happened only when a full collection would
/// have. Young cycles were therefore exactly as rare as the collections they were
/// meant to replace: the 2026-08-17 measurement recorded **six** in a 600-round
/// run, and a generational split that runs six times cannot pay for the barrier
/// it costs on every store.
///
/// # Why 10 and not `Z_DEFAULT_YOUNG_FRACTION`'s 25
///
/// `generation::Z_DEFAULT_YOUNG_FRACTION` is 0.25 and describes a young
/// generation's share of a heap it *owns* -- a sized space that survivors are
/// evacuated out of. This nursery is not that: it is the tail of one arena, it is
/// reclaimed by a bounded sweep rather than by a cursor reset, and its cost is
/// proportional to the objects in it. A smaller budget therefore buys the thing
/// this trigger is for (frequent, cheap young cycles) where 25% would give six
/// large ones on a 1.2 GB heap -- which is what the measurement already showed.
/// Revisit when a young cycle reclaims by resetting a cursor.
pub fn zgc_gen_nursery_percent() -> usize {
    match None::<String>
        .and_then(|v| v.trim().parse::<usize>().ok())
    {
        Some(p) => p.min(100),
        None => 10,
    }
}

// `CRATONVM_ZGC_ALLOC_TRIGGER` -- collect once this PERCENT OF CAPACITY has
// been allocated since the last collection. **Default 0, i.e. OFF**; the
// measurement below is why.
//
// # What this fixes
//
// Every other clause of [`needs_gc`](GarbageCollector::needs_gc) is a
// question about the WHOLE HEAP: `allocated >= gc_threshold` is 75% of
// `-Xmx`, and `headroom_low` is "the arena can no longer serve a request".
// Neither asks how much GARBAGE there is. At `-Xmx2g` with 50 MiB live, the
// first clause fires when `allocated` reaches 1.5 GiB -- so every cycle lets
// ~1.45 GiB of garbage accumulate before it runs, and the object-start
// registry, the mark bitmap and the sweep all cover that whole span because
// the arena's bump cursor has run over it.
//
// **Pause work therefore scaled with the heap FLAG rather than with the
// garbage.** Doubling `-Xmx` on a workload whose live set did not change
// doubled every pause, which is the opposite of what a heap-size increase is
// supposed to buy. Triggering on allocation caps the span a pause has to walk
// at `budget + live` regardless of `-Xmx`.
//
// # MEASURED, and why it is off by default
//
// `G1ChurnPauseProbe 50 600` at `-Xmx2048m` -- 50 MiB retained, 2.4 GiB of
// garbage streamed past it -- release build, three runs a row, this binary
// with only this switch moved:
//
// | percent | wall ms | cycles/run | mean pause | max pause | registered at the worst pause |
// |---|---|---|---|---|---|
// | 0 (off) | 2891 | 2 | 116 ms | **202 ms** | 10,597,520 |
// | 50 | 3234 (+12%) | 3 | 113 ms | 174 ms | 7,485,260 |
// | 25 | 3463 (+20%) | 6 | 80 ms | 122 ms | 3,953,214 |
// | 12 | 3767 (+30%) | 12 | 57 ms | **79 ms** | 2,116,551 |
//
// The mechanism is exactly the predicted one: the span a pause walks tracks
// the budget, and the worst pause falls 2.6x for a 30% wall cost. But it IS a
// 30% wall cost, and it is not an artefact of measuring badly -- it is what
// collecting six times as often costs. Total sweep work across a run is not
// reduced (the same bytes are swept, in more and smaller pieces) and the
// per-cycle work that is proportional to the LIVE set -- the mark, the
// registry snapshot -- is paid six times as often instead of twice.
//
// So this is a pause-versus-throughput DIAL and not a fix, and a dial does not
// get to pick its own default on an argument: this tree has shipped a ZGC
// marking feature default-on on an argument before and reverted it (see
// `CRATONVM_ZGC_PARSWEEP`'s note). What would earn a non-zero default is a
// budget derived from a PAUSE TARGET rather than from a percentage --
// project the span the next cycle would walk from the last cycle's
// microseconds per registered object, and collect when it would exceed
// `max_gc_pause_ms`. That is a different change and it needs the suite time
// this one has not had.
//
// Off, the clause costs one relaxed load per `needs_gc`.
//
// # Why it needs no `gc_rearm` floor
//
// The same argument the nursery clause makes: [`Self::cycle_alloc_watermark`]
// is reset to the post-sweep `allocated` at the end of every collection, so
// firing again requires `budget` bytes of genuinely NEW allocation. A live
// set parked above the static threshold -- the shape `gc_rearm` exists to
// stop storming on -- cannot re-trigger this clause at all.
// zr z4/ab (rv4 X2): uncached -- a construction seed (`alloc_trigger_bytes`).
pub fn zgc_alloc_trigger_percent_explicit() -> Option<usize> {
    None::<String>
        .and_then(|v| v.trim().parse::<usize>().ok())
        .map(|p| p.min(100))
}

/// The percentage floor a PAUSE TARGET gets when the operator has not named
/// one: 25% of capacity.
///
/// # Why a target needs a floor at all
///
/// [`ZgcRealHeap::refresh_pause_target_budget`] is a feedback loop, so it is
/// blind to the FIRST cycle -- it has nothing to measure until a pause has
/// happened. The occupancy clause therefore lets that one run to 75% of
/// `-Xmx`, and on `G1ChurnPauseProbe 50 1800` at `-Xmx4096m` it is the worst
/// pause of the whole run by a factor of five: everything after the loop
/// engages settles at 65-115 ms, and that one cycle is 442.
///
/// A floor caps it, and the target then controls the steady state. Measured,
/// one binary, only these two switches moved:
///
/// | configuration | wall | worst pause |
/// |---|---|---|
/// | neither | 9091 ms | 466 ms |
/// | target 100 alone | 12943 ms | 509 ms |
/// | target 100 + floor 25 | 11180 ms | **190 ms** |
/// | target 200 alone | 14849 ms | 530 ms |
/// | target 200 + floor 25 | 13548 ms | **221 ms** |
///
/// Better than the target ALONE on both axes, which is the part that makes
/// this a default rather than a tuning knob: paying for the blind first cycle
/// up front costs less than the controller's recovery from it.
///
/// # Why 25 and not tighter
///
/// The floor's whole job is the cycle the controller cannot see. Once the loop
/// has a measurement it takes over, and a floor tighter than the budget the
/// loop wants is just the loop being overridden by a constant -- which is the
/// per-workload dial the target exists to replace. 25% is the loosest value
/// measured to cap the first cycle.
///
/// # It has had no reader since 2026-09-04
///
/// [`alloc_trigger_percent_for`] withdrew the implicit floor the day it
/// shipped, so this constant is referenced by nothing but its own doc.
/// Retained rather than deleted because the measurement above is the only
/// record of what the floor bought and what it cost, and re-instating it is a
/// live proposal (`H2 TestLargeBlob` has to stop crashing first). The
/// `dead_code` allow is here to say that on purpose: an unexplained allow is
/// how a constant that WAS wired up quietly stops being.
#[allow(dead_code)]
pub const ZGC_ALLOC_TRIGGER_PERCENT_UNDER_TARGET: usize = 25;

/// The percentage the allocation clause actually uses: the operator's if they
/// named one, else the floor under a pause target, else off.
///
/// `CRATONVM_ZGC_ALLOC_TRIGGER=0` is an explicit refusal and stays off even
/// with a target set -- that is the arm that measures the target alone.
pub fn zgc_alloc_trigger_percent() -> usize {
    alloc_trigger_percent_for(zgc_alloc_trigger_percent_explicit(), zgc_pause_target_ms())
}

/// The decision itself, without the environment: the operator's percentage if
/// they named one, else the floor under a pause target, else off.
///
/// Split out because the interesting cases here are the combinations, not
/// the parsing (both inputs are uncached env reads since zr z4/ab, but a
/// pure function still needs no overrides to test).
pub fn alloc_trigger_percent_for(explicit: Option<usize>, _pause_target_ms: u64) -> usize {
    // WITHDRAWN AS A DEFAULT ON 2026-09-04, the day it shipped. It stays
    // available as `CRATONVM_ZGC_ALLOC_TRIGGER=<percent>`; only the implicit
    // floor under a pause target is gone.
    //
    // The floor does what it was measured to do -- it caps the first cycle,
    // which the controller is blind to -- but making it a DEFAULT changed how
    // often the collector runs on every ZGC workload, and
    // `org.h2.test.db.TestLargeBlob` does not survive that:
    //
    //   pause target on (floor active)   34 GC cycles   SIGSEGV in libc
    //   pause target off (no floor)       0 GC cycles   PASS
    //
    // Same binary, same class, one switch, and reproduced 3 times out of 4 with
    // the floor on against 0 of 3 with it off. The fault is inside a
    // `FileChannelImpl.implWrite` -> `IOUtil.write` -> `DirectByteBuffer` path,
    // which is the shape of a native operation whose backing store a collection
    // reclaimed underneath it.
    //
    // THE CRASH IS ALMOST CERTAINLY OLDER THAN THIS FLAG. Without the floor
    // that test never collects at all, so nothing was exercising the path; the
    // floor is a reproducer, not the defect. But a default that turns a passing
    // test into a native crash is not one to ship while the underlying bug is
    // open, and "it only surfaces a pre-existing bug" is not a reason to leave
    // it on -- it is a reason to go and fix that bug with the reproducer this
    // gave us.
    explicit.unwrap_or(0)
}

/// `CRATONVM_ZGC_HEADROOM_BYPASSES_REARM` -- let the `headroom_low` clause of
/// [`needs_gc`](GarbageCollector::needs_gc) ask for a collection without first
/// clearing the `gc_rearm` floor. **default on**; `1` (or any non-off word)
/// turns it on.
///
/// # What it is for
///
/// `gc_rearm` is sized from LIVE bytes and `headroom_low` is about ALLOCATABLE
/// SPACE. On a collector that does not compact those two numbers come apart
/// without bound, so the shared floor can silence the headroom clause exactly
/// on the heap it was written for -- a large `-Xmx` with a small live set and a
/// fragmented free list. The failure is not a slow run: the infallible
/// `GarbageCollector::alloc_object` aborts the process rather than raising
/// `OutOfMemoryError`, so the collector never collecting is a crash with most
/// of the heap free. The `needs_gc` call site carries the worked arithmetic.
///
/// # Why it is off
///
/// Turning it on changes how often the default collector runs on every ZGC
/// workload, and `alloc_trigger_percent_for` records what that costs when it is
/// done on an argument rather than on suite time (`org.h2.test.db.TestLargeBlob`
/// went from PASS to a SIGSEGV in libc, 3 runs of 4). The narrowing term --
/// [`ZgcRealHeap::last_cycle_reclaimed_nothing`] -- bounds the storm but does
/// not answer the throughput question, and this lane could not build or run a
/// binary. What would earn the flip is in
/// `docs/feature-designs/zgc-architecture.md`.
/// **THE FLIP IS THIS LINE.** `false` -> `true` is the whole default change for
/// `CRATONVM_ZGC_HEADROOM_BYPASSES_REARM`; nothing else in `gc/` moves.
///
/// Two things must move WITH it and neither is in this crate:
///
/// * `types/src/flag_groups.rs`'s declaration takes
///   `off_word: Some("0")` at the same moment, because that shape means
///   "default-on, `=0` turns it off". It is `off_word: None` today, which is
///   the default-OFF shape. Getting that backwards publishes the flag as the
///   opposite of what it is, which has happened twice in this repo.
/// * `docs/gc-tuning.md`'s row and the generated `docs/config/flag-inventory.md`
///   row must both say ON. Note the inventory currently renders this flag as
///   *default-on* already, from an `off_word` generator artefact, and is wrong
///   today in the direction that would become right.
///
/// What earns the flip is in
/// `docs/feature-designs/zgc-architecture.md`, read off
/// `[GC] zgc-headroom:` / [`ZgcRealHeap::headroom_flip_evidence`].
pub const HEADROOM_BYPASSES_REARM_DEFAULT: bool = false;

// `CRATONVM_ZGC_MARK_ROOT_FILTER_CONC` — arm the extra-root filter across the
// **concurrent** mark phase, with the `late_owners()` mark-end repair that
// makes doing so sound. Default **ON since zr z7/cm (2026-10-13)**, `=0` kill;
// inert unless a concurrent cycle opens (`CRATONVM_ZGC_CONC_START`).
//
// # Why it is on now (zr z7/cm)
//
// Disarmed, every object the concurrent pool marks asks four global tables
// (`loader_pin`, `mirror_pin`, `metadata_pin` -- each a `RwLock` read, i.e.
// an atomic RMW on one shared lock word -- and the overlay providers' class
// gate, a shared `fetch_add` per object in `native-collections`), from N
// workers at once, while the stop-the-world mark of the same graph answers
// the same questions from the lock-free bloom. Those NON_EMPTY latches are
// true in every real run (the application class loader pins mirrors), so
// this was the concurrent mark's per-object premium over the stop-the-world
// one -- the cost concurrent marking was being judged on. The soundness half
// has existed since the 2026-09-20 round: owners registered during the phase
// come back through `late_owners_within` at mark end, and a row REMOVED
// during the phase (precondition R; no single-VM remover runs outside a
// safepoint) declines to certify (`parallel_mark_fallbacks`). Engagement:
// `root_filter=1` and `late_owners=` / `late_sources=` on
// `[GC] zgc-markend:`.
//
// zr z8/rv7: the tables are process-wide, so "a row removed" counted ANOTHER
// VM's removals too (its collection's `replace_*`, its teardown), and every
// concurrent cycle that overlapped one re-marked stop-the-world. Only rows
// whose owner lies in this heap's arena count now
// (`CRATONVM_ZGC_ROOT_FILTER_REPAIR_OWN_ARENA`, `=0` kill; page
// `zgc-zr8rv7-root-filter-repair-refused-on-foreign-vm-removals-20261013.md`).
//
// # What it is for
//
// `ZMarkRootFilter` is consulted once per marked object and answers
// "definitely owns no extra roots" without touching the three `types/` pin
// registries or `external_roots`. It halved `mark_us` on the stop-the-world
// mark — and it is rebuilt in exactly one place, the head of
// `collect_garbage`, so it has always been **disarmed for the whole
// concurrent phase**: the phase concurrent marking exists to create, where N
// workers take the same global `RwLock`s once per object. That is
// `gap-a-root-filter-is-off-for-the-whole-concurrent-phase.md`.
//
// # Why it is off, and why the flag may not be declared yet
//
// Arming it without the repair is **unsound, not merely slower** — wave 3
// declined to do that, correctly. The repair now exists (lane R, wave 4:
// contents generations on `types/src/{loader,mirror,metadata}_pin.rs`, and
// `ZMarkRootFilter::late_owners()`), and wave 4 wires it here. What has not
// happened is a measurement: this moves cost out of the pause and into the
// concurrent phase, so `mark_us` is the wrong number and **wall clock** with
// `concurrent_phase_ms` is the right one, on a workload that actually has
// user class loaders and a populated `external_roots` provider.
//
// Declared in `types/src/flag_groups.rs` (`zgc-mark-root-filter-conc`,
// `off_word: "0"`) since 2026-09-21; this paragraph said it was not
// (corrected zs s2/mc2).
// zr z4/ab (rv4 X2): this and `zgc_headroom_bypasses_rearm` are uncached
// construction seeds (`conc_root_filter`, `headroom_bypasses_rearm`).
pub fn zgc_conc_root_filter() -> bool {
    match None::<String> {
        Some(value) if !value.trim().is_empty() => !cratonvm_types::flags::value_is_off(&value),
        // zr z7/cm: default ON (see the doc above), `=0` kill.
        _ => true,
    }
}

pub fn zgc_headroom_bypasses_rearm() -> bool {
    {
        // The env var, when the operator named one, else the default constant.
        // Spelled as an explicit presence test rather than
        // `runtime_flag_on(..)` so that the DEFAULT lives in one named place a
        // one-line edit can move, instead of being implied by which helper was
        // called. `runtime_flag_on` answers `false` for an unset variable, so
        // reading the default off it would make flipping the default a change
        // of helper -- and this tree has twice published a ZGC flag as the
        // opposite of its real default by editing one of the two spellings.
        match None::<String> {
            Some(value) if !value.trim().is_empty() => !cratonvm_types::flags::value_is_off(&value),
            _ => HEADROOM_BYPASSES_REARM_DEFAULT,
        }
    }
}

/// `CRATONVM_ZGC_BITMAP_BOUNDS` -- restrict the per-cycle object-start and
/// mark-bit passes to the arena's two BUMPED ends instead of its whole
/// capacity. **Default on**; `0`/`off`/`false`/`no` restores the
/// whole-capacity passes byte for byte, which is the A/B arm.
///
/// See [`ZgcRealHeap::live_bitmap_bounds`] for the measurement (a
/// capacity-proportional `snapshot_us` of 11-13 ms at `-Xmx2048m` against
/// 21-23 ms at `-Xmx4096m`, at the same object count) and for why the middle
/// between the two cursors is provably empty.
pub fn zgc_bitmap_bounds_enabled() -> bool {
    static CACHED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *CACHED.get_or_init(
        || match None::<String> {
            Some(value) => {
                let value = value.trim();
                !matches!(
                    value,
                    "0" | "false" | "False" | "FALSE" | "off" | "Off" | "OFF" | "no" | "No" | "NO"
                )
            }
            None => true,
        },
    )
}

/// The floor under [`zgc_alloc_trigger_percent`]'s budget.
///
/// A percentage of a small heap is a small number, and a budget below the
/// re-arm floor would trade the storm `gc_rearm` prevents for one this clause
/// causes. 8 MiB is the same figure [`zgc_headroom_margin`] floors at, for the
/// same reason: it is the smallest allocation window in which a real workload
/// does enough to be worth a cycle.
pub const ZGC_ALLOC_TRIGGER_FLOOR: usize = 8 * 1024 * 1024;

/// Floor-budget overruns, with no cycle meeting the target in between, after
/// which [`ZgcRealHeap::refresh_pause_target_budget`] stops trying for the run.
///
/// Two, not the three of the live-set verdict: reaching the floor at all takes
/// one overrun per halving from wherever the loop started (seven from a 3 GB
/// span), so the first floor overrun already closes a long run of evidence,
/// and the second only guards against that one floor cycle being a host stall.
/// Each re-engagement in between costs one unconstrained cycle plus the
/// halvings back down, i.e. a few seconds of pauses on a multi-GiB heap.
pub const ZGC_PAUSE_FLOOR_OVERRUNS_TO_GIVE_UP: usize = 2;



// `CRATONVM_ZGC_PAUSE_TARGET_MS` -- the pause this collector tries to stay
// under, in milliseconds. Default 200 (HotSpot's `MaxGCPauseMillis` default,
// and G1's default in this tree); `0` turns the pause-target budget off and
// leaves [`zgc_alloc_trigger_percent`] in charge of the allocation clause.
//
// `-XX:MaxGCPauseMillis=<n>` sets the same thing and WINS over this variable:
// the CLI flag is applied to the constructed heap
// ([`ZgcRealHeap::set_pause_target_ms`]) after this default has been read, so
// an operator's explicit target is never overridden by an A/B switch.
//
// # Why a target rather than a percentage
//
// `CRATONVM_ZGC_ALLOC_TRIGGER=<percent>` (2026-09-03) made pause work scale
// with the GARBAGE rather than with `-Xmx`, and its measurement showed the
// problem with expressing that as a percentage: the percent is a
// pause-versus-throughput dial that the operator has to tune per workload,
// because a percentage of capacity says nothing about how long the resulting
// pause will be. 12% bought a 79 ms worst pause for +30% wall on
// `G1ChurnPauseProbe`; on a workload with a tenth of the live set the same
// 12% would buy a pause nobody needed to shorten, at the same cost.
//
// A target inverts that. It is a CEILING: the clause starts unconstrained and
// only engages once a pause has actually OVERRUN the target, tightening in
// proportion to the overrun and relaxing back to unconstrained once pauses
// return to comfortably under it. So it spends throughput only where a pause
// would otherwise overrun, and on a workload whose pauses never reach the
// target it costs one relaxed load per collection and nothing else -- which
// is what makes a non-zero default defensible here where the percentage form
// had to ship off. See [`ZgcRealHeap::refresh_pause_target_budget`], whose
// doc also records what the two earlier, wrong versions of this loop did.
// zr z4/ab (rv4 X2): uncached -- read once per heap construction, where it
// seeds `pause_target_ms`; a process latch made the first heap decide every
// later heap's target.
pub fn zgc_pause_target_ms() -> u64 {
    match None::<String>
        .and_then(|v| v.trim().parse::<u64>().ok())
    {
        Some(ms) => ms,
        None => 200,
    }
}

/// `CRATONVM_ZGC_PAUSE_TARGET_INCLUDES_RELOCATE=1` -- feed
/// [`ZgcRealHeap::refresh_pause_target_budget`] the pause **including** the
/// relocation slide. Default **off** for one wave; see the staging plan below.
///
/// # The defect this exists to correct
///
/// `collect_garbage` read its pause and fed the controller BEFORE the
/// compaction block, while the slide runs ~150 lines later inside the same
/// [`StopTheWorldToken`]. The `zgc-relocate` line's own comment records the
/// size of what that omits: the slide measured at *about twice the logged
/// pause* on `ZgcTlabStress 8 200` (round 9 wave 5, `zgc5`). Compaction is
/// default-on ([`ZgcRealHeap::relocation_requested_by_default`]), so on that
/// workload the loop was told the pause was roughly a THIRD of what the
/// mutators took.
///
/// This is not a wrong log line. `refresh_pause_target_budget` sizes
/// `alloc_trigger_bytes`, which is a clause of `needs_gc`, so it decides how
/// OFTEN the collector runs. Fed a systematically low pause it under-tightens
/// (pauses overrun the 200 ms default target and the loop never sees it), and
/// it mis-fits its own model: the correction is multiplicative,
/// `span * target / pause`, which is only a correction factor if `pause` and
/// `span` describe the same work. `span` is `live + freed` -- the whole heap
/// the cycle touched, INCLUDING what the slide then moved -- so with the
/// pre-slide reading the ratio corrects for nothing.
///
/// # Why it is staged rather than simply fixed
///
/// Because it changes every budget on every workload, in the TIGHTENING
/// direction: the loop would see pauses up to 3x larger and cut the budget
/// accordingly, i.e. more collections. `alloc_trigger_percent_for` is this
/// file's standing record of what happens when a trigger-frequency change
/// ships on an argument instead of a measurement, so the A/B here is a re-run
/// rather than a rebuild. Flip the default when, on `ZgcTlabStress 8 200` and
/// `G1ChurnPauseProbe 50 1800`:
///
/// 1. the `=1` arm's `pause_with_relocate_us` p99 is at or under the target
///    where the `=0` arm overruns it -- that is the feature starting to work;
/// 2. wall clock in the `=1` arm is within what the round will spend (expect
///    it to be worse: more cycles); and
/// 3. `pause_target_unreachable` does not climb. If it does, the target is not
///    achievable once relocation counts, and the honest response is to raise
///    the default target rather than to keep measuring the wrong half.
///
/// `[GC] zgc-pause:`'s `total_us` is unaffected either way -- it is documented
/// as the pre-slide figure and `zgc-relocate`'s `pause_with_relocate_us` is
/// the whole.
/// **THE FLIP IS THIS LINE.** `false` -> `true` is the whole default change for
/// `CRATONVM_ZGC_PAUSE_TARGET_INCLUDES_RELOCATE`.
///
/// As with [`HEADROOM_BYPASSES_REARM_DEFAULT`], the declaration in
/// `types/src/flag_groups.rs` must take `off_word: Some("0")` in the same
/// commit and the two docs rows must follow.
///
/// # Lane P's recommendation, wave 4: this default is WRONG and should flip
///
/// Stated plainly because the page that staged it did not: there is no reading
/// of any probe that argues for feeding a control loop a pause that excludes
/// the largest component of that pause. Compaction is default-on, the slide is
/// measured at about twice the pre-slide figure, and the loop divides by a
/// `span` (`live + freed`) that *includes* the objects the slide moved. The
/// `=0` arm is not a conservative default; it is a measurement error in a
/// feedback loop that sizes how often the collector runs.
///
/// What is genuinely undecided is the **target**, not the clock. The 200 ms
/// default was calibrated, implicitly, against the pre-slide reading, so
/// flipping this alone tightens every budget by roughly 3x on a compacting
/// workload — which is the throughput regression
/// [`alloc_trigger_percent_for`] is this file's standing record of. So the
/// flip and the target are one decision, and
/// [`ZgcCounters::pause_target_unreachable`] is the gate between them: if it
/// climbs in the `=1` arm, the target is not achievable once relocation
/// counts, and the honest response is to raise the default target rather than
/// to keep measuring the wrong half. That is criterion 3 of the gap page,
/// promoted from a tie-breaker to the thing being decided.
///
/// Read `[GC] zgc-pause-dist:` for the p99 the criteria ask for; it is the
/// same quantity in both arms by construction (see
/// [`metrics::ZgcMetrics::cycle_pause_distribution`]), which is what makes it
/// a discriminator rather than a restatement of which clock was read.
pub const PAUSE_TARGET_INCLUDES_RELOCATE_DEFAULT: bool = false;

pub fn zgc_pause_target_includes_relocate() -> bool {
    // t1 wave 3 (lane Y): read per call, no process `OnceLock` latch.
    match None::<String> {
        // `value_is_off` and not a sixth hand-rolled truth table: that
        // helper exists because `map_or(true, |v| v != "0")` left
        // `CRATONVM_JIT_LICM=false` ON, silently, and its header asks new
        // sites to adopt it rather than re-derive it.
        Some(value) if !value.trim().is_empty() => !cratonvm_types::flags::value_is_off(&value),
        // Unset, or set to the empty string. An empty value is an operator
        // who exported the name and nothing else; it means "I did not
        // choose", so it takes the default rather than being read as OFF.
        // That is a behaviour change from the previous spelling, and it is
        // the right one in exactly one direction: it only matters once the
        // default is ON, which is when reading an empty value as OFF would
        // silently disable a default-on feature.
        _ => PAUSE_TARGET_INCLUDES_RELOCATE_DEFAULT,
    }
}

/// `CRATONVM_ZGC_METRICS=1` -- also print [`metrics::ZgcMetrics`]'s
/// OpenJDK-shaped cycle line, and its whole-run summary at teardown.
///
/// **Additive, never a replacement.** `[GC] zgc-real:` and `[GC] zgc-pause:`
/// are printed unchanged whether this is set or not, because every harness,
/// grep and archived comparison in this tree is keyed on their current text.
/// This switch is what lets a wave compare the two renderings on the same run
/// before deciding to retire either.
///
/// The recording half is **not** behind this flag: [`metrics::ZgcMetrics`] is
/// fed on every collection regardless (see
/// [`ZgcRealHeap::record_metrics_for_cycle`]). Gating the recorder as well is
/// how `ZSlotCensus` came to be unreachable — an instrument with an off switch
/// and no on switch — and the cost being gated would save is a handful of
/// relaxed `fetch_add`s per *collection*.
pub fn zgc_metrics_lines_enabled() -> bool {
    // t1 wave 3 (lane Y): read per call, no process `OnceLock` latch.
    match None::<String> {
            Some(value) => {
                let value = value.trim();
                !matches!(
                    value,
                    "" | "0"
                        | "false"
                        | "False"
                        | "FALSE"
                        | "off"
                        | "Off"
                        | "OFF"
                        | "no"
                        | "No"
                        | "NO"
                )
            }
            None => false,
        }
}

/// `CRATONVM_ZGC_TRIGGER_SHADOW=1` -- print one `[GC] zgc-trigger-shadow:`
/// line per collection showing what an allocation-rate-driven trigger **would**
/// have decided.
///
/// Pure observation: it changes no predicate, no budget and no existing line.
/// See [`ZgcRealHeap::log_trigger_shadow`] for the fields and for the
/// falsification test this exists to run, and
/// `docs/feature-designs/zgc-architecture.md`
/// for what stage 2 would do with the answer.
pub fn zgc_trigger_shadow_enabled() -> bool {
    // t1 wave 3 (lane Y): read per call, no process `OnceLock` latch.
    match None::<String> {
        Some(value) => {
            let value = value.trim();
            !matches!(
                value,
                "" | "0"
                    | "false"
                    | "False"
                    | "FALSE"
                    | "off"
                    | "Off"
                    | "OFF"
                    | "no"
                    | "No"
                    | "NO"
            )
        }
        None => false,
    }
}

/// `CRATONVM_ZGC_GEN_MINORS_PER_MAJOR` -- young cycles allowed between
/// whole-heap ones. Default [`generation::Z_DEFAULT_MINORS_PER_MAJOR`] (8).
///
/// # Why there has to be a ceiling at all
///
/// A young cycle retains every old object without asking whether it is
/// reachable, so garbage promoted before it died is invisible to every young
/// cycle. Without a periodic major that accumulates until the heap is full of
/// objects nothing points at -- the classic generational leak, and it presents
/// as an OutOfMemoryError on a workload with plenty of dead data rather than as
/// a slowdown.
pub fn zgc_gen_minors_per_major() -> usize {
    match None::<String>
        .and_then(|v| v.trim().parse::<usize>().ok())
    {
        Some(n) => n,
        None => generation::Z_DEFAULT_MINORS_PER_MAJOR as usize,
    }
}

pub fn zgc_flag_default(key: &str, default: bool) -> bool {
    match cratonvm_types::flags::runtime_var_os(key) {
        Some(raw) => {
            let v = raw.to_string_lossy().trim().to_ascii_lowercase();
            if default {
                !matches!(v.as_str(), "0" | "off" | "false" | "no")
            } else {
                !matches!(v.as_str(), "" | "0" | "off" | "false" | "no")
            }
        }
        None => default,
    }
}

/// `CRATONVM_ZGC_GEN_OLD_BITMAP` -- keep an exact old-object start bitmap and
/// pre-mark a young cycle's old generation from it (`mark |= registered &
/// old`) instead of reading every registered header. It also enables the
/// young-aged sweep (see `CRATONVM_ZGC_GEN_YOUNG_AGED_SWEEP`), which is what
/// closes `zgc-zr1gn-unmarked-objects-below-the-floor-stay-registered-FIXED-20261012.md`:
/// after a bitmap young cycle every unmarked object is unregistered, so the
/// VM's registry-membership liveness verdict agrees with the mark.
///
/// **Default ON inside generational mode since zr z2/g2** (opt-in in z1/gn).
/// The floor-bounded arm leaves a dead young parent registered over a freed
/// child, and the z1 host rows priced the bitmap at -7 % CPU on S4 and +2 %
/// (noise) on S1: soundness decides an arm whose cost is a wash. `=0`
/// restores the header-walk pre-mark and the floor-bounded sweep. Only read
/// with generational mode on (a non-generational heap never ages an object,
/// so the bitmap is never written).
pub fn zgc_gen_old_bitmap_enabled() -> bool {
    zgc_flag_default("CRATONVM_ZGC_GEN_OLD_BITMAP", true)
}

/// `CRATONVM_ZGC_PARMARK_FALLBACK_RESCAN` (zr z4/rv4) -- after a failed
/// parallel mark, seed the serial marker with the out-edges of every marked
/// base (`ZgcRealHeap::parmark_fallback_rescan_seeds`). **Default ON**; `=0`
/// restores the roots-only fallback, which skips every object the driver
/// claimed but did not scan. Read once per failed cycle.
pub fn zgc_parmark_fallback_rescan() -> bool {
    zgc_flag_default("CRATONVM_ZGC_PARMARK_FALLBACK_RESCAN", true)
}

/// `CRATONVM_DBG_ZGC_PARMARK_FAIL=1` (zr z4/rv4, diagnostic, default OFF):
/// every parallel stop-the-world mark claims its roots and reports failure,
/// so a host probe can drive the serial fallback (the positive control of
/// `CRATONVM_ZGC_PARMARK_FALLBACK_RESCAN`).
pub fn zgc_dbg_parmark_fail() -> bool {
    #[cfg(test)]
    if ZGC_TEST_PARMARK_FAIL_AFTER_ROOTS.with(|f| f.get()) {
        return true;
    }
    zgc_flag_default("CRATONVM_DBG_ZGC_PARMARK_FAIL", false)
}

/// `CRATONVM_ZGC_GEN_BITMAP_FALLBACK_MAJOR` (zr z3/rv3) -- with the old-start
/// bitmap on but not usable, a would-be young cycle runs whole-heap instead of
/// the header-walk + floor-bounded fallback. **Default ON**; `=0` restores the
/// fallback. Only consulted on a generational heap with the bitmap on.
///
/// zr z4/rv4: UNCACHED. It only seeds `ZgcCounters::gen_bitmap_fallback_major`
/// at heap construction, and rv3 landed it with a process `OnceLock` in the
/// same wave lt removed the latch from every other construction seed: the
/// first heap in a process decided it for every later one.
pub fn zgc_gen_bitmap_fallback_major_enabled() -> bool {
    zgc_flag_default("CRATONVM_ZGC_GEN_BITMAP_FALLBACK_MAJOR", true)
}

// `CRATONVM_ZGC_GEN_OLD_BITMAP_VERIFY=1` -- cross-check every bitmap
// pre-mark against the headers (O(registry); diagnostic, default OFF). zr
// z3/lt: seeded per heap (`heap_state::ZHeapLocalState::gen_old_bitmap_verify`).

/// `CRATONVM_ZGC_GEN_YOUNG_AGED_SWEEP` -- on a bitmap pre-marked young cycle,
/// sweep every non-old object (wherever it is) instead of the floor-bounded
/// nursery. Default ON, but it only engages under
/// `CRATONVM_ZGC_GEN_OLD_BITMAP=1`; `=0` keeps the floor-bounded sweep with
/// the bitmap pre-mark (the A/B that separates the two halves).
pub fn zgc_gen_young_aged_sweep_enabled() -> bool {
    zgc_flag_default("CRATONVM_ZGC_GEN_YOUNG_AGED_SWEEP", true)
}

/// `CRATONVM_ZGC_GEN_YOUNG_RELOCATE` -- may a YOUNG cycle run the slide?
///
/// **default on since zr z2/g2; `=1` restores the young slide.** The slide's
/// passes (live-set snapshot, page census, whole-live-set reference rewrite)
/// are O(live heap), so on a young cycle they are the one O(heap) cost the
/// generational split cannot bound. The z1 host rows measured
/// `Zr1GnGenShape 64 400 200000 5 8 64` at 10.8 s CPU with the young slide
/// refused against 12.6 s with it (-14 %), checksums equal to HotSpot's.
/// Whole-heap cycles still slide (`CRATONVM_ZGC_RELOCATE` governs those), and
/// a nursery survivor is promoted by AGE in the young sweep instead of by the
/// G2 slide. Seeded per heap (`ZgcCounters::gen_young_relocate_on`) so a test
/// can drive both arms in one process.
pub fn zgc_gen_young_relocate() -> bool {
    zgc_flag_default("CRATONVM_ZGC_GEN_YOUNG_RELOCATE", false)
}

/// `CRATONVM_ZGC_GEN_YOUNG_EVACUATE` -- zt3gn S2.4 slice 1 (t1 wave 6 lane
/// Y4): may a YOUNG cycle run a NURSERY-SCOPED slide?
///
/// **default on (default on, `=1`).** The scoped slide moves only young objects
/// on logical pages at or above the young floor that hold no old object, and
/// its rewrite visits only the moved region's survivors, the young survivors
/// elsewhere and the carded objects -- O(young live) instead of the whole
/// live set `CRATONVM_ZGC_GEN_YOUNG_RELOCATE` walks. A refused cycle (see
/// `young_evacuate::YE_REFUSE_*`) runs no young slide, unless
/// `CRATONVM_ZGC_GEN_YOUNG_RELOCATE=1` also asks for the whole-live-set one.
/// Seeded per heap (`ZgcCounters::young_evac`).
pub fn zgc_gen_young_evacuate() -> bool {
    zgc_flag_default("CRATONVM_ZGC_GEN_YOUNG_EVACUATE", false)
}

/// `CRATONVM_ZGC_GEN_INLINE_ANNOUNCE` -- may the JIT's inline start-bit store
/// (`cratonvm_gc_core::tlab::JIT_ZGC_ANNOUNCE`, round 9 wave 8) stay open on a
/// GENERATIONAL heap?
///
/// **Default ON (zr z2/g2); `=0` restores the old block**, under which every
/// compiled `new` on a generational heap called the announce helper
/// (`note_tlab_object`) -- the call the inline store exists to remove (~19 %
/// of `CratonBench bintrees` when it was measured) -- because the inline store
/// does not `note_young_page`.
///
/// # Why the young grain needs no per-object note under the inline store
///
/// The inline store is taken only while the thread's ownership epoch still
/// matches the chunk's carve (`Tlab::zgc_owned_epoch == epoch`, condition 2 of
/// `JitZgcAnnounceTable`'s contract). The carve marks every grain of the chunk
/// young (`carve_tlab_chunk` -> `mark_young_pages`) on a generational heap, and
/// the only thing that clears a young grain is `clear_young_pages`, which runs
/// inside `collect_garbage` -- whose first statements bump the ownership epoch
/// (`invalidate_owned_vm_tlab_chunks`). So for every object the inline store
/// can register, its grain was marked by the carve and has not been cleared
/// since: `note_young_page` would load the bit, find it set and return. The
/// helper's other duty, allocate-black, stays covered by the `mark_active`
/// half of the `blocked` word, which this switch does not touch.
pub fn zgc_gen_inline_announce_enabled() -> bool {
    static CACHED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *CACHED.get_or_init(|| zgc_flag_default("CRATONVM_ZGC_GEN_INLINE_ANNOUNCE", true))
}

/// zt t6/tc (GN-7, `CRATONVM_ZGC_GEN_JIT_PROMOTION_FLOOR`, default ON inside
/// the generational mode, `=0` kill): the JIT reference-store plan's young
/// floor, in `GC_FLAGS_BYTE_OFFSET` byte units. On a generational heap it is
/// `promotion_age << 4`: the emitter's unsigned `flags_byte < floor` (x64
/// `objects.rs`, the FLOOR arm, `JB`) is then exactly the helper's own
/// `gc_age < promotion_age` test (`note_ref_store_slow`,
/// `note_ref_store_element_slow`) -- the flags nibble is at most 15 -- so a
/// skipped call is one that would have returned. Otherwise (and under `=0`)
/// it is `JIT_YOUNG_FLOOR_AGE_ZERO`, the ordering-free bound. A store that
/// raced a promotion (a peer frozen between the test and the store) is
/// covered by the card every promotion writes (`age_survivor`), which the
/// next young cycle re-scans.
pub fn zgc_jit_young_floor(generational: bool, promotion_age: u32) -> u8 {
    if generational && zgc_flag_default("CRATONVM_ZGC_GEN_JIT_PROMOTION_FLOOR", true) {
        (promotion_age.clamp(1, cratonvm_types::MAX_GC_AGE as u32) as u8) << 4
    } else {
        cratonvm_gc_core::jit_bounds::JIT_YOUNG_FLOOR_AGE_ZERO
    }
}

/// `CRATONVM_ZGC_GEN_EMPTY_MINOR_NEEDS_PROMOTION` (zt t14/gn5; **default
/// ON**, `=0` kill) -- a young cycle that freed nothing escalates the next
/// collection to a major only when something was promoted since the last
/// major. See [`ZgcRealHeap::gen_empty_minor_escalation_skipped`]. Read once
/// per heap; inert outside the generational mode.
pub fn zgc_gen_empty_minor_needs_promotion_enabled() -> bool {
    zgc_flag_default("CRATONVM_ZGC_GEN_EMPTY_MINOR_NEEDS_PROMOTION", true)
}

/// `CRATONVM_ZGC_GEN_NURSERY_AFTER_WARMUP` (zt t20/gn7, D7; **default ON**,
/// `=0` kill) -- the nursery clause of `needs_gc` (G2d) waits until the
/// warm-up steps are spent ([`ZGC_WARMUP_CYCLES`] `Warmup` cycles), as
/// HotSpot 25's director runs its three warm-up majors at 10 / 20 / 30 % of
/// the soft maximum before any allocation-rate cycle. Read once per heap;
/// inert outside the generational mode and with `CRATONVM_ZGC_WARMUP=0`.
pub fn zgc_gen_nursery_after_warmup_enabled() -> bool {
    zgc_flag_default("CRATONVM_ZGC_GEN_NURSERY_AFTER_WARMUP", true)
}

/// `CRATONVM_ZGC_GEN_YOUNG_COMPLEMENT` -- sweep a young cycle's nursery as
/// the COMPLEMENT of its live set (zr z2/g2; **default ON since zt t14/gn5**,
/// `=0` kill). Decided by the t12 S2.1 rows (`Zr1GnGenShape`, five
/// interleaved pairs): CPU median 0.965x of the per-object sweep (max
/// 0.976x), `zr1gn-check:` SAME as HotSpot 25 on every row, and
/// `complement_sweeps == minor_cycles`, `complement_refusals=0` on every
/// switched run. Only a generational heap's young cycles read it. See
/// [`ZgcRealHeap::sweep_young_complement`].
pub fn zgc_gen_young_complement_enabled() -> bool {
    zgc_flag_default("CRATONVM_ZGC_GEN_YOUNG_COMPLEMENT", true)
}

/// `CRATONVM_ZGC_GEN_ELEMENT_CARDS=1` -- element (8-byte granule) cards for
/// old reference arrays of at least 64 elements (zr z2/g2, stage G2 of
/// `zgc-zr1gn-proposal-generational-direction-20261012.md`; **default OFF**,
/// opt-in with its deciding rows). See [`ZgcRealHeap::note_ref_store_element`].
pub fn zgc_gen_element_cards_enabled() -> bool {
    zgc_flag_default("CRATONVM_ZGC_GEN_ELEMENT_CARDS", false)
}



/// `CRATONVM_ZGC_GEN_MAJOR_PROMOTED_PERCENT=<p>` -- replace the fixed
/// `minors_per_major` ceiling with an OLD-GROWTH rule (zr z2/g2; **default 0
/// = off**). See [`ZgcRealHeap::gen_major_by_old_growth`].
pub fn zgc_gen_major_promoted_percent() -> usize {
    None::<String>
        .and_then(|v| v.trim().parse::<usize>().ok())
        .map_or(0, |p| p.min(100))
}

/// `CRATONVM_ZGC_GEN_FULL_REQUEST_MAJOR` (zt t3/gn; **default ON**, `=0`
/// kill) -- on a generational heap, a full request (`System.gc()`, `jcmd
/// GC.run`: the collecting thread's `gc_quiescence::request_major_gc`) makes
/// the collection whole-heap. HotSpot 25's generational ZGC serves
/// `System.gc()` with a major collection (`ZDriverMajor`). Before this the
/// request was never consulted by the cycle-kind decision, so a `System.gc()`
/// on a heap with an old generation ran a young cycle: dead old objects
/// survived it, their weak references stayed set and `-Xlog:gc` printed
/// `Minor Collection (System.gc())`. Read once per heap; inert outside the
/// generational mode.
pub fn zgc_gen_full_request_major_enabled() -> bool {
    zgc_flag_default("CRATONVM_ZGC_GEN_FULL_REQUEST_MAJOR", true)
}

/// `CRATONVM_ZGC_GEN_MAJOR_RULE_CAUSES` (zt t11/gn3; **default ON** inside
/// the generational mode, `=0` kill) -- on a generational heap with an old
/// generation, a collection whose cause is one of HotSpot 25's MAJOR-only
/// director rules ([`gen_mask_is_major_rule`]) is whole-heap. Before this the
/// cycle kind ignored the cause, so a warm-up step after a `System.gc()` (which
/// tenures every survivor, so an old generation exists from the first
/// collection) ran young and `-Xlog:gc` printed `Minor Collection (Warmup)`,
/// a line HotSpot never prints. Read once per heap; inert outside the
/// generational mode.
pub fn zgc_gen_major_rule_causes_enabled() -> bool {
    zgc_flag_default("CRATONVM_ZGC_GEN_MAJOR_RULE_CAUSES", true)
}

/// zt t12/tg5 (`zgc-zt11tg4-*` item 2, row TG3-O4;
/// `CRATONVM_ZGC_TIMER_SKIPS_STALL_CYCLES`, default ON, `=0` kill): an
/// allocation stall's cycle does not restart the `Timer` rule's clock
/// ([`timer::ZTimer::note_cycle_end_as`]). HotSpot 25 runs a stall's
/// collection as a minor, and its major timer reads the old generation's
/// last cycle, so `-XX:ZCollectionInterval=1` still gives a `Timer` cycle
/// each second of a stalling churn; here each stall restarted the clock and
/// none came. Reachable only with `-XX:ZCollectionInterval`. `=0`: every
/// whole-heap cycle restarts it, as before.
pub fn zgc_timer_skips_stall_cycles_enabled() -> bool {
    zgc_flag_default("CRATONVM_ZGC_TIMER_SKIPS_STALL_CYCLES", true)
}

/// zt t31/x10 (page `zgc-zt13x10-review-residuals-*` item 5,
/// `CRATONVM_ZGC_STALL_MARK_REACHES_JOINED_PAUSE`, default ON, `=0` kill):
/// the stall classification reaches the cycle that serves the stall, not only
/// a pause the stalled thread itself runs. The collecting thread's mark
/// (`gc_quiescence::with_allocation_stall_door`) is per thread, so a stalled
/// mutator that lost the stop-the-world race and took part in another
/// thread's pause left that cycle unmarked: it restarted the `Timer` clock.
/// `=0`: the collecting thread's mark only, as before.
pub fn zgc_stall_mark_reaches_joined_pause_enabled() -> bool {
    zgc_flag_default("CRATONVM_ZGC_STALL_MARK_REACHES_JOINED_PAUSE", true)
}

/// zt t12/x9: the trigger mask `collect_garbage` reads for its cycle kind,
/// from the latch a reader sees before the collection, on the collecting
/// thread: the warm-up bit dropped exactly where `collect_garbage` drops it
/// before that read -- a full request under `CRATONVM_ZGC_WARMUP_EXPLICIT_DOOR`
/// (default ON), a `Timer` cycle under `CRATONVM_ZGC_TIMER_NOT_WARMUP`
/// (default ON). Keep the two in step with those blocks;
/// `young_cycle_predicted` reads it so the prediction and the decision see
/// one mask. (zt t20/tg9 retired the third, the opt-in allocation-stall door:
/// no row ever reached its state.)
pub fn mask_after_warmup_doors(mask: usize) -> usize {
    let explicit = cratonvm_gc_core::gc_quiescence::major_gc_requested()
        && zgc_flag_default("CRATONVM_ZGC_WARMUP_EXPLICIT_DOOR", true);
    let timer = mask & TRIGGER_BIT_TIMER != 0
        && zgc_flag_default("CRATONVM_ZGC_TIMER_NOT_WARMUP", true);
    if explicit || timer {
        mask & !TRIGGER_BIT_WARMUP
    } else {
        mask
    }
}

/// zt t11/gn3: is a collection with this trigger mask one HotSpot 25's
/// generational ZGC can only run as a MAJOR collection? Its director asks
/// `rule_major_warmup` and `rule_major_proactive` only in
/// `make_major_gc_decision` (the minor decision has the timer, allocation
/// rate and high usage rules; see [`hotspot_cause_for_mask`]'s note on the
/// order), so `Warmup` and `Proactive` are major causes. Decided by the SAME
/// first-match as the cause the collection reports, so for one mask the kind
/// and the `-Xlog:gc` / MXBean cause agree (a mask whose first admitting
/// clause is `Allocation Rate` stays a minor candidate even with the warm-up
/// bit set). zt t13/x10 (`zgc-zt12x9-*` item 9): the printed cause is read
/// from the pending latch before the kind's read, and the director tick
/// writes that latch off-safepoint, so a `Proactive` bit landing in between
/// can still make the two disagree (rare; the masks differ, not this rule).
/// Pure.
pub fn gen_mask_is_major_rule(mask: usize) -> bool {
    matches!(hotspot_cause_for_mask(mask), Some("Warmup" | "Proactive"))
}

/// zt t7/tg (GN-12): a young cycle is WEAK when it freed under 1 % of the
/// heap's capacity -- the unit GN-12's rows count runs of. Pure.
pub fn gen_minor_is_weak(bytes_freed: usize, capacity: usize) -> bool {
    bytes_freed < capacity / 100
}

/// zt t7/tg (GN-12): must the cycle after a young cycle that freed
/// `bytes_freed` be a major? Always when it freed nothing (zr's rule); with
/// `weak_switch` (`CRATONVM_ZGC_GEN_ESCALATE_WEAK_MINOR=1`) also when it freed
/// under an eighth of the nursery budget (`gen_nursery_bytes`; a zero budget
/// is the nursery clause's kill switch and leaves only the zr rule). Pure.
pub fn gen_escalate_after_minor(bytes_freed: usize, nursery_budget: usize, weak_switch: bool) -> bool {
    bytes_freed == 0 || (weak_switch && bytes_freed < nursery_budget / 8)
}

/// `CRATONVM_ZGC_GEN_YOUNG_SKIP_REFERENCES` (zt t7/cc; **default on** since
/// zt t8, `=0` is the kill switch: rows yr/yf answer HotSpot 25.0.3's
/// `kept=true` / `finalized=false` young window only with it on) -- in the
/// generational mode, a collection predicted young
/// leaves every referent in place (the VM's pre-collection pass nulls
/// nothing) and keeps its dead young finalizable objects for the next major,
/// as HotSpot 25's minor does. See [`ZgcRealHeap::arm_young_refs_strong`].
/// Read once per heap; inert outside the generational mode.
pub fn zgc_gen_young_skip_references() -> bool {
    zgc_flag_default("CRATONVM_ZGC_GEN_YOUNG_SKIP_REFERENCES", true)
}

/// `CRATONVM_ZGC_GEN_YOUNG_PREDICT_EXACT` (zt t9/gn; **default on**, `=0` is
/// the kill switch) -- the young-cycle prediction behind
/// `CRATONVM_ZGC_GEN_YOUNG_SKIP_REFERENCES` evaluates the opt-in major rule
/// (`CRATONVM_ZGC_GEN_MAJOR_PROMOTED_PERCENT`; the headroom rule it also
/// covered was retired in zt t14/gn5) instead of predicting a major whenever
/// it is set. Changes nothing unless it is set. See
/// `ZgcRealHeap::young_cycle_predicted`. Read once per heap; inert outside
/// the generational mode.
pub fn zgc_gen_young_predict_exact() -> bool {
    zgc_flag_default("CRATONVM_ZGC_GEN_YOUNG_PREDICT_EXACT", true)
}

// `CRATONVM_ZGC_GEN_SLIDE_OLD_LIVE` -- after a YOUNG cycle's slide, carry the
// whole live figure (not only the young survivors) as the old live bytes.
// Default ON (a fix, zr z1/gn); `=0` restores the previous store. zr z3/lt:
// seeded per heap (`heap_state::ZHeapLocalState::gen_slide_old_live`).

/// `CRATONVM_ZGC_NURSERY_CENSUS` -- run the per-young-cycle nursery survivor
/// census walk.
///
/// **Default OFF since zr z2/g2; `=1` turns it on.** It is an instrument (it
/// feeds only `[GC] zgc-nursery:`'s `survivor_*` fields, the verdict on
/// proposal D), not a collector phase, and it walked every nursery base of the
/// pre-sweep snapshot plus a header per survivor on EVERY young cycle. A row
/// that wants the census asks for it; a run that did not reads
/// `survivor_cycles_observed=0`, which is that engagement field's job. Seeded
/// per heap (`ZgcCounters::gen_census_on`).
pub fn zgc_nursery_census_enabled() -> bool {
    zgc_flag_default("CRATONVM_ZGC_NURSERY_CENSUS", false)
}

/// `CRATONVM_ZGC_NURSERY_CENSUS_AGE` -- also run the AGE-FILTERED census
/// (survivors that were young when the cycle began). Default OFF.
pub fn zgc_nursery_census_age_enabled() -> bool {
    zgc_flag_default("CRATONVM_ZGC_NURSERY_CENSUS_AGE", false)
}

/// `CRATONVM_DBG_FINCAND=1` — per collection, report how each finalizable
/// candidate was classified: `unregistered` (not a current allocation),
/// `marked_alive` (something still roots it, so it is not finalizable yet), or
/// `dead_resurrected` (kept alive for the finalizer thread and enqueued).
///
/// "finalize() never ran" is the same observation for all three, which is why
/// the counters exist: they are the difference between "the object is still
/// reachable" and "the finalizer machinery never saw it".
pub fn dbg_fincand() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(dbg_fincand_requested)
}

/// Uncached half of [`dbg_fincand`] (zt t6/rv3):
/// `CRATONVM_DBG_FINCAND` read by value. It read presence, so `=0`
/// turned it on.
pub fn dbg_fincand_requested() -> bool {
    false}

/// `CRATONVM_DBG_ZGC_WILD=1` -- name what the mark's `wild_skipped` counted:
/// one `[ZGC-WILD]` line per refused edge (the first 64 per process) with its
/// parent object, class, edge kind (`field` / `cell` / `elem` / `pin` /
/// `overlay` / `root` / `remembered`) and index, and the registered object the
/// address falls inside. The collection pause adds `[ZGC-WILD-SECTIONS]` (which
/// part of the root vector each index range came from) and `[ZGC-WILD-SNAP]`
/// (the thread and frame slot of each parked thread's snapshot root that is not
/// an object start).
///
/// The warning alone cannot say whether a heap field or a root was wild. The
/// one it printed in `R16FfmInNativeBattery` was a parked `main`'s dead local
/// (`r19-zgc-mark-skips-wild-pointers-in-compatible-ffm-in-native-battery-FIXED-20261007`).
pub fn dbg_zgc_wild() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(dbg_zgc_wild_requested)
}

/// Uncached half of [`dbg_zgc_wild`] (zt t6/rv3):
/// `CRATONVM_DBG_ZGC_WILD` read by value. It read presence, so `=0`
/// turned it on.
pub fn dbg_zgc_wild_requested() -> bool {
    false}

impl ZgcRealHeap {
    /// One `[ZGC-WILD]` line for an edge whose target is not a registered
    /// object start; silent for one that is. See [`dbg_zgc_wild`].
    #[cold]
    #[inline(never)]
    pub fn dbg_wild_edge(&self, parent: usize, edge: &str, index: usize, child: u64) {
        let c = child as usize;
        if self.registry.contains(c) {
            return;
        }
        static PRINTED: AtomicUsize = AtomicUsize::new(0);
        if PRINTED.fetch_add(1, Ordering::Relaxed) >= 64 {
            return;
        }
        let (pkind, pcls, pslots, pname, preg) = if parent == 0 {
            (String::from("-"), 0, 0, String::from("-"), false)
        } else {
            let h = self.header_ref(parent as *mut u8);
            let cls = h.class_id.as_u32();
            (
                format!("{:?}", h.kind()),
                cls,
                h.num_slots(),
                self.diag_class_name(cls),
                self.registry.contains(parent),
            )
        };
        let in_arena = c >= self.arena_base && c < self.arena_end;
        let near = if in_arena {
            match self.registry.nearest_base_at_or_below(c) {
                Some(b) => {
                    let h = self.header_ref(b as *mut u8);
                    let cls = h.class_id.as_u32();
                    format!(
                        "{b:#x}+{} {:?} {}",
                        c - b,
                        h.kind(),
                        self.diag_class_name(cls)
                    )
                }
                None => String::from("none"),
            }
        } else {
            String::from("-")
        };
        let marking = self.conc_cycle_active.load(Ordering::Relaxed);
        eprintln!(
            "[ZGC-WILD] parent={parent:#x} reg={preg} {pkind} class={pcls}:{pname} slots={pslots} \
             edge={edge}[{index}] child={child:#x} in_arena={in_arena} near={near} \
             conc={marking} thread={:?}",
            std::thread::current().name()
        );
    }
}

/// `CRATONVM_ZGC_SWEEP_HEADER_ZERO` -- when the sweep reclaims a dead object,
/// zero its **header** rather than its whole body. **Default on**;
/// `0`/`off`/`false`/`no` restores the whole-body memset byte for byte.
///
/// # Why the body zeroing was never buying anything
///
/// The sweep's own comment gives the reason for zeroing: "so a later scan can't
/// see a stale header". That reason is satisfied entirely by the header --
/// `HEADER_SIZE` is 16 bytes and an `ObjectHeader` is exactly those 16, so a
/// zeroed header is `class_id=0, num_slots=0`, which is the identical
/// "well-formed ALL-ZERO object" a reader of a vacated span sees today (see
/// `corpse_ledger`). **The body is only reachable THROUGH the header**: every
/// field read sizes the object from `num_slots`, every extent walk sizes it from
/// `alloc_size(header)`, and every membership test goes through the registry the
/// sweep has just removed the base from. Nothing can reach past a zeroed header
/// to find the bytes this stops writing.
///
/// The other candidate reason -- "a reused block may contain stale bytes" -- is
/// already handled at the other end, and unconditionally: `alloc_raw` memsets
/// every allocation it hands out, and `tlab_refill` memsets a whole chunk. So
/// the sweep's body zeroing is *redundant* with the allocator's, and a dead
/// object was being memset twice: once when it died, once when its span was
/// handed out again.
///
/// # Why it is worth removing
///
/// It is a memset of the **entire reclaimed volume**, inside the pause, on every
/// cycle. §3b of `zgc-concurrent-and-generational-plan-20260813.md` measured
/// `sweep` at 182 ms of a 309 ms mean pause and found the generation split did
/// not move it; a young cycle that reclaims 900 MB spends that pause writing 900
/// MB of zeroes at memory bandwidth whatever the split says. This is the part of
/// "reclaim by resetting a cursor" that needs no page allocator.
///
/// # It applies to EVERY cycle, not only a young one
///
/// This landed on 2026-08-17 scoped to `young_cycle`, and the paragraph that
/// stood here said so -- "a whole-heap cycle is byte-for-byte unchanged, so no
/// default run moves". That restriction expired with the gauntlet it was
/// written for: `collect_garbage` now builds `ZSweepCfg::zero_header_only`
/// from this flag alone, with no cycle-kind term. The wording outlived the
/// code by a month, and it pointed the change AWAY from the cost it was
/// written for -- a young cycle's sweep is 6.3 ms of a 112 ms pause (5.6%)
/// where a whole-heap cycle's is 170 ms of 265 ms (64%), because it walks
/// 13.0M dead objects against a young cycle's 137k. The measured -30% was
/// being applied to the small one.
///
/// `docs/gc-tuning.md`'s "What does a young sweep do per dead object?" row
/// still carries the retired wording ("Both apply to young cycles only, so a
/// whole-heap sweep is byte-for-byte unchanged"); it is recorded in
/// `docs/feature-designs/zgc-architecture.md`.
pub fn zgc_sweep_header_zero() -> bool {
    match None::<std::ffi::OsString> {
        Some(raw) => {
            let v = raw.to_string_lossy().trim().to_ascii_lowercase();
            !matches!(v.as_str(), "0" | "off" | "false" | "no")
        }
        None => true,
    }
}

/// `CRATONVM_ZGC_SWEEP_DEAD_RUNS` -- hand the free list one span per **run** of
/// adjacent dead objects rather than one per object. **Default on**;
/// `0`/`off`/`false`/`no` restores the per-object calls.
///
/// EVERY CYCLE, not only a young one -- this said "on a young sweep" for the
/// same expired reason its sibling [`zgc_sweep_header_zero`] did, and
/// `collect_garbage` reads it into `ZSweepCfg::merge_dead_runs` with no
/// cycle-kind term. The saving is on the whole-heap cycle, which is the one
/// that walks millions of dead objects.
///
/// # Why this is free
///
/// The sweep walks the object-start bitmap ASCENDING -- which is already a
/// correctness requirement, because the coalescer only sees adjacent dead spans
/// as adjacent when they arrive in order. So the merge is a comparison against
/// the previous span's end, and objects die in runs, so the runs are long.
///
/// # Why the result is identical
///
/// `coalesce_free_list` already merges exactly these spans into exactly these
/// maximal runs a few statements later; this only does it before the entries
/// exist rather than after. The tier a span routes to differs on the way in (a
/// merged run is large where its members were small), and the coalescer
/// normalises that too -- it rebuilds the list from `low_blocks_sorted`, so the
/// post-coalesce state is the same set of spans in the same tiers either way.
/// `an_arena_coalesces_pre_merged_runs_to_the_same_shape` asserts that rather
/// than assuming it.
///
/// What it removes is the churn: 4.8M `push_block_routed` calls and a
/// 4.8M-element sort inside the pause, replaced by a few thousand of each.
pub fn zgc_sweep_dead_runs() -> bool {
    match None::<std::ffi::OsString> {
        Some(raw) => {
            let v = raw.to_string_lossy().trim().to_ascii_lowercase();
            !matches!(v.as_str(), "0" | "off" | "false" | "no")
        }
        None => true,
    }
}

// `CRATONVM_ZGC_MARK_CTX_DIRECT` -- hand the mark coordinator the heap's own
// `Arc` instead of wrapping it in [`ZHeapMarkBridge`]. **Default on**; `0` /
// `off` / `false` / `no` restores the wrapper, so the A/B is a re-run.
//
// # What it removes, and why it is the FIRST thing to try for C5
//
// `ZMarkShared` holds `Arc<dyn ZMarkContext>`, so every `try_mark`,
// `visit_refs`, `is_in_heap` and `object_size` on the parallel mark path is an
// indirect call. With the bridge in the way each of those is **two** indirect
// hops — bridge, then heap — because `ZHeapMarkBridge`'s methods do nothing but
// forward. `ZgcRealHeap` implements `ZMarkContext` itself, so when the heap was
// built by `new_shared` (which every VM-owned heap is) its own `Arc` can be
// handed over directly and one whole hop per call disappears.
//
// The 2026-08-17 profile is what points here: `try_mark` (7.14%) and
// `visit_refs` (2.26%) appear as their own frames on the one-worker arm and on
// no other, while the serial marker's equivalent work is inlined into
// `collect_garbage`. A symbol that exists on one arm and is inlined away on the
// other is a DISPATCH difference. Whether that is the whole of C5 is exactly
// what this switch is for — it is the cheap half of the monomorphisation
// hypothesis, and if it moves nothing the hypothesis is wrong and the striped
// queues and per-cycle pool construction get their turn.
//
// # It also deletes an unsafe pointer
//
// The bridge is a `*const ZgcRealHeap` with a hand-written `Send`/`Sync`, kept
// sound by a three-fact argument about the coordinator's `Drop`. The direct
// path has no raw pointer and needs no argument: the `Arc` keeps the heap alive
// by construction. The bridge stays as the fallback for a heap built by
// `with_capacity` (tests), which has no self-`Arc` to hand over.
//
// Default-on is safe here because the whole path is already opt-in: the
// parallel marker runs only when `CRATONVM_ZGC_PARMARK` asks for workers, and
// its default is zero.
// zr z4/ab (rv4 X2): uncached; seeded per heap into
// `ZHeapLocalState::mark_ctx_direct`, which `mark_context_arc` reads per cycle.
pub fn zgc_mark_ctx_direct() -> bool {
    match None::<std::ffi::OsString> {
        Some(raw) => {
            let v = raw.to_string_lossy().trim().to_ascii_lowercase();
            !matches!(v.as_str(), "0" | "off" | "false" | "no")
        }
        None => true,
    }
}

/// A fragmentation reading, taken post-sweep — the "steady state" of Phase 2.2.
///
/// `worst_permille` is `largest_free_block * 1000 / capacity` at its lowest
/// across every SAMPLED collection (see [`ZGC_FRAG_GAUGE_MIN_FREE_PERMILLE`]
/// for which collections those are), and `free_permille` is how much of the
/// heap was free at that same moment — the two have to be read together or
/// neither means anything.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ZFragGauge {
    /// Collections that met the sampling condition.
    pub samples: usize,
    /// Worst (lowest) `largest_free_block / capacity`, in permille. `None`
    /// when nothing was sampled — which is the normal state for a short or
    /// heap-light run and must not be reported as a perfect score.
    pub worst_permille: Option<usize>,
    /// Free share of capacity at the worst sample, in permille.
    pub free_permille: usize,
    /// The collection number the worst sample came from.
    pub worst_cycle: usize,
}

// ---------------------------------------------------------------------------
// The arena-backed TLAB
// ---------------------------------------------------------------------------
//
// `ZArenaTlab`, `ZArenaTlabRegistry`, the `ZTlabHeapHooks` impl and the
// heap-side refill/retire methods moved to `zgc/arena_tlab.rs` on 2026-09-02.
// See that module's header for why it is not `zgc::tlab`.
/// Everything the one-shot fragmentation report needs, gathered while the
/// arena lock is held so the logging — which resolves class NAMES through the
/// VM's class manager — can happen after it is released.
///
/// Splitting it is not tidiness. [`ZgcRealHeap::alloc_raw`] reaches its failure
/// arm holding the arena mutex, and [`cratonvm_gc_core::collector::class_name_for_diagnostics`]
/// calls back into the VM to take the class-manager lock; naming under the
/// arena lock would invert `arena -> class_manager` against every ordinary
/// allocation path (`class_manager -> arena`) — i.e. turn a diagnostic into a
/// deadlock reachable only when the heap is already failing.
pub struct ZFragReport {
    pub profile: cratonvm_gc_core::arena::FragProfile,
    /// The large-object end's downward bump cursor. `capacity` means the end
    /// was never used at all, which is a different diagnosis from "used and
    /// full".
    pub high_cursor: usize,
    /// `free_high` blocks, bytes and largest — the split's OWN free list. "The
    /// split is in place" and "the split has room in it" are different claims,
    /// and only the second predicts whether a large request can be served.
    pub high_blocks: usize,
    pub high_bytes: usize,
    pub high_max: usize,
    /// Bytes of the large-object floor the high end has not claimed yet. Large
    /// here and a failing large allocation together mean the reserve was
    /// respected but the region never grew into it; small means the region is
    /// as big as it is allowed to get and the floor is the thing to raise.
    pub high_reserve_unclaimed: usize,
    /// `(class_id, objects, bytes)` for the occupants of the winning window's
    /// walls, largest byte total first.
    pub occupants: Vec<(u32, usize, usize)>,
    /// Bytes inside the window that are below the cursor but are neither
    /// free-listed nor a registered object base.
    pub unregistered_bytes: usize,
}

impl ZFragReport {
    /// Emit the report. Called with **no** heap lock held — see the type doc.
    pub fn log(&self, request: usize) {
        let p = &self.profile;
        tracing::warn!(
            target: "cratonvm::gc::guard",
            request,
            spans = p.spans,
            largest_span = p.largest_span,
            free_bytes = p.free_bytes,
            walls = p.walls,
            wall_bytes = p.wall_bytes,
            span_hist = %cratonvm_gc_core::arena::format_log2_hist(&p.span_hist),
            wall_hist = %cratonvm_gc_core::arena::format_log2_hist(&p.wall_hist),
            high_cursor = self.high_cursor,
            high_blocks = self.high_blocks,
            high_bytes = self.high_bytes,
            high_max = self.high_max,
            high_reserve_unclaimed = self.high_reserve_unclaimed,
            "zgc frag: the shape of the free list at the failing request \
             (histograms are log2 lower bounds: `512K:3892` = 3892 spans of \
             512 KiB..1 MiB)",
        );
        let Some(w) = p.cheapest else {
            tracing::warn!(
                target: "cratonvm::gc::guard",
                request,
                "zgc frag: no contiguous stretch of the swept region is even {request} bytes \
                 wide — the walls are not the problem, the request exceeds the whole span \
                 from the lowest free block to the highest.",
            );
            return;
        };
        tracing::warn!(
            target: "cratonvm::gc::guard",
            request,
            window_start = w.start,
            window_end = w.end,
            window_bytes = w.end - w.start,
            window_free = w.free_bytes,
            wall_bytes = w.wall_bytes,
            walls = w.walls,
            "zgc frag: the CHEAPEST window that could serve this request — {} live bytes \
             in {} run(s) are all that stand between {} free bytes spread over {} bytes of \
             contiguous arena. A small number here means a handful of survivors is holding \
             the window hostage; a number near the request means the region is a genuine \
             live/dead mosaic.",
            w.wall_bytes,
            w.walls,
            w.free_bytes,
            w.end - w.start,
        );
        for &(class_id, count, bytes) in &self.occupants {
            tracing::warn!(
                target: "cratonvm::gc::guard",
                class = %cratonvm_gc_core::collector::class_name_for_diagnostics(class_id),
                count,
                bytes,
                "zgc frag: wall occupant",
            );
        }
        if self.unregistered_bytes != 0 {
            tracing::warn!(
                target: "cratonvm::gc::guard",
                bytes = self.unregistered_bytes,
                "zgc frag: bytes inside the window that are below the cursor but neither \
                 free-listed nor a registered object base — memory no allocator and no \
                 sweep can see.",
            );
        }
    }
}

/// Heap-side seam for the reference-slot census.
///
/// # Why this exists at all
///
/// `docs/feature-designs/zgc-reference-slot-representation.md` ends with exactly
/// one unresolved number — *what fraction of live reference slots are legacy
/// 16-byte cells?* — and says in as many words that it is the only guess in the
/// document and the one that sizes the work. [`census`] is the instrument; this
/// impl is the only thing that makes it non-inert. Without it the census module
/// compiles, unit-tests, and measures nothing.
///
/// # This is a FORK of [`ZgcRealHeap::enumerate_references`], not a reuse of it
///
/// The marker and the census want different things out of the same three arms,
/// and every difference is load-bearing rather than incidental:
///
/// * **Nulls are kept.** The marker discards `raw == 0` because null has no
///   out-edge. The census must count it: a null slot is a barrier fast-path hit,
///   so a heap of null legacy fields is *not* a heap of barrier work, and the
///   two are indistinguishable once the nulls are dropped.
/// * **Every legacy slot is reported, tagged, whatever its tag.** The marker
///   only wants `Value::Object(Some(_))`. Filtering here would merge the
///   never-written population (`tag == 0` from the zero-fill) into "this class
///   has no reference fields", which is the difference the census's
///   `legacy_unwritten_slots` column exists to keep.
/// * **No `skip_index`.** A `java.lang.ref.Reference`'s referent is not a strong
///   edge, so the marker hides it; but it *is* a reference slot, and the census
///   counts slots.
/// * **Raw reads, never `read_prim_element`.** That helper's plausibility
///   degrade (`gc/src/heap.rs:1687-1714`) turns an implausible word into
///   `Value::Object(None)`. For a colored pointer that degrade is exactly the
///   corruption the study names as hazard 2, and a census that inherits it
///   reports the corrupted population as null instead of as a finding.
///
/// # The two-phase contract
///
/// [`census::ZSlotCensus::run_walk`] drains
/// [`census::ZCensusHeapView::for_each_live_object`] into a local `Vec` **before**
/// it calls [`census::ZCensusHeapView::reference_slots`] for anything, so the
/// registry lock taken below is never held across a call back into this heap.
/// That is the whole reason the trait is shaped this way; see this tree's
/// standing note that a registry guard held across a call-back is a lock cycle.
impl census::ZCensusHeapView for ZgcRealHeap {
    /// Visit every **marked** registered allocation.
    ///
    /// "Live" here means [`GC_FLAG_MARKED`], so this is only meaningful between
    /// the end of the mark phase and the sweep that clears those bits — which is
    /// exactly where [`ZgcRealHeap::collect_garbage`] calls the census from. Run
    /// outside that window every bit is clear and the walk reports
    /// `ZCensusVerdict::NoData`, which the census prints as "nothing was
    /// decided" rather than as a row of zeroes.
    fn for_each_live_object(&self, f: &mut dyn FnMut(u64, ClassId, census::ZCensusObjectKind)) {
        // Heap-walking entry point: retire every TLAB first
        // ([`ZgcRealHeap::retire_all_tlabs`]). Inside `collect_garbage` — the
        // only caller today — this is a no-op second retire, because the cycle
        // retires before its mark snapshot and the world is stopped in between.
        // It is here for the census driver that calls `run_walk` directly,
        // which reaches no safepoint of its own.
        self.retire_all_tlabs();

        // Materialise the base list BEFORE reading a single header. The trait
        // permits holding the registry for the whole callback, but this heap's
        // `header_mut` reads arena bytes and `effectively_compact_header`
        // reaches the layout cache, so the shorter window is free to take —
        // and on the `Hash` arm of `ZObjectStarts` there is still a real guard
        // to keep out of that window.
        let bases: Vec<usize> = self.registry.bases();

        for base in bases {
            let header = self.header_mut(base as *mut u8);
            if !self.mark_is_set(base) {
                continue;
            }
            let kind = match header.kind() {
                ObjectKind::Object => {
                    // The EFFECTIVE predicate, not the header bit — see
                    // `effectively_compact_header`. An object the read path
                    // treats as a 16-byte cell is a legacy instance for census
                    // purposes no matter what its flags say.
                    if Self::effectively_compact_header(header) {
                        census::ZCensusObjectKind::CompactInstance
                    } else {
                        census::ZCensusObjectKind::LegacyInstance
                    }
                }
                ObjectKind::Array => {
                    if header.element_type() == ArrayElementType::Reference {
                        census::ZCensusObjectKind::ReferenceArray
                    } else {
                        census::ZCensusObjectKind::NoReferenceSlots
                    }
                }
                ObjectKind::HumongousFiller => census::ZCensusObjectKind::NoReferenceSlots,
            };
            let class_id = header.class_id;
            f(base as u64, class_id, kind);
        }
    }

    /// The second, independent derivation of compactness the census cross-checks
    /// against the kind reported above.
    ///
    /// It must agree with [`census::ZCensusObjectKind::CompactInstance`] for
    /// every object or the walk is void
    /// ([`census::ZWalkResult::kind_disagreements`]). Both sides route through
    /// [`ZgcRealHeap::effectively_compact_header`]; the `kind() == Object` screen
    /// is what makes them agree for arrays and fillers, which are never compact
    /// instances regardless of what a stale flag byte might read.
    fn is_compact(&self, addr: u64) -> bool {
        if addr == 0 {
            return false;
        }
        let header = self.header_mut(addr as usize as *mut u8);
        header.kind() == ObjectKind::Object && Self::effectively_compact_header(header)
    }

    /// Report every slot of the object at `addr` that could hold a reference.
    ///
    /// See the impl header for how each arm differs from
    /// [`ZgcRealHeap::enumerate_references`].
    fn reference_slots(&self, addr: u64, f: &mut dyn FnMut(census::ZSlotObservation)) {
        // The body is `reference_slots_with` (zr z2/r2), generic so the
        // slide's rewrite pass gets it monomorphised; the census keeps
        // its trait-object form through this one call.
        self.reference_slots_with(addr, f)
    }

    // `static_reference_slots` is left at its default no-op ON PURPOSE. A
    // `StaticsBlock` is `Box::leak`ed memory owned by
    // `vm/src/vm/realms/class_realm.rs:43-46`, outside this crate and outside
    // this collector's registry, so nothing here can reach it. The census
    // therefore reports the `StaticField` column as NOT WIRED rather than 0 —
    // see `ZSlotCensus::statics_wired`, which this heap never sets.
}

impl ZgcRealHeap {
    /// The body of [`census::ZCensusHeapView::reference_slots`] for this heap,
    /// generic over the visitor (zr z2/r2). The census calls it through the
    /// trait with a `&mut dyn FnMut`; the slide's reference rewrite calls it
    /// directly with a closure, so the per-slot visit inlines instead of
    /// being an indirect call per slot of every survivor. ONE body, so the
    /// census and the rewrite cannot drift on which slots exist.
    pub fn reference_slots_with<F: FnMut(census::ZSlotObservation) + ?Sized>(
        &self,
        addr: u64,
        f: &mut F,
    ) {
        if addr == 0 {
            return;
        }
        let base = addr as usize as *mut u8;
        let header = self.header_mut(base);
        match header.kind() {
            ObjectKind::Object => {
                let class_id = header.class_id.as_u32();
                let num_slots = header.num_slots();
                // The RAW header bit here, deliberately, and it is the one place
                // in this impl where the effective predicate is the wrong tool:
                // the bit decides the object's BODY LAYOUT (`alloc_object` sizes
                // a flagged object with `layout.body_size`, not
                // `num_slots * SLOT_SIZE`), while the effective predicate decides
                // how the read path INTERPRETS that body. Striding 16-byte cells
                // through a compact-sized body because the layout lookup failed
                // would read past the allocation — `get_field`'s fall-through
                // does exactly that, and it is a latent bug there that this
                // instrument must not reproduce.
                let header_bit_compact = cratonvm_types::is_compact_object(header);
                if header_bit_compact {
                    let laid_out =
                        cratonvm_types::with_class_layout(class_id, num_slots, |layout| {
                            for (&offset, &is_ref) in
                                layout.field_disps.iter().zip(layout.is_ref.iter())
                            {
                                // No `skip_index` and no `raw == 0` filter; see
                                // the impl header.
                                if !is_ref {
                                    continue;
                                }
                                // SAFETY: `offset` comes from this object's own
                                // registered layout, which is the layout its body
                                // was sized with at allocation, so the field lies
                                // inside the allocation. `read_ref_slot` reads
                                // `ref_field_size()` bytes — the width the layout
                                // reserved for a reference field — and is used
                                // rather than a bare `read::<u64>` so the
                                // observation matches what `read_compact_field`
                                // (`types/src/field_layout.rs:978-993`) would
                                // actually load under narrow oops.
                                let slot = unsafe { base.add(offset as usize) };
                                let raw =
                                    unsafe { cratonvm_types::narrow_oop::read_ref_slot(slot) };
                                f(census::ZSlotObservation::bare(
                                    census::ZSlotShape::CompactField,
                                    slot as u64,
                                    raw,
                                ));
                            }
                        })
                        .is_some();
                    if !laid_out {
                        // Flagged compact, no layout for `(class_id, num_slots)`:
                        // `for_each_live_object` reported this object as a LEGACY
                        // instance (the read path will treat it as one), but its
                        // body is compact-sized, so its cells cannot be walked
                        // without reading out of bounds. Report no slots and say
                        // so. The object is still counted; only its slots are
                        // missing, which biases the legacy share DOWN — the
                        // conservative direction for a decision rule whose
                        // failure mode is deferring work that is not deferrable.
                        tracing::warn!(
                            target: "zgc",
                            class_id,
                            num_slots,
                            "zgc census: compact-flagged object has no registered layout; \
                             its slots are omitted from the census (cells cannot be walked \
                             through a compact-sized body)"
                        );
                    }
                } else {
                    let n = num_slots as usize;
                    // The same "no real class has 1<<24 fields" screen
                    // `check_field_index` applies. A desynced walk that decoded
                    // payload bytes as a header would otherwise stride millions
                    // of cells straight out of the arena.
                    if n > (1 << 24) {
                        tracing::debug!(
                            target: "zgc",
                            num_slots = n,
                            "zgc census: suspect header, slots omitted"
                        );
                        return;
                    }
                    for i in 0..n {
                        // SAFETY: a non-compact object's body is exactly
                        // `num_slots * SLOT_SIZE` bytes (`alloc_object`'s
                        // `fields_size` fallback), so cell `i` and both words
                        // read out of it lie inside the allocation. The two
                        // reads are 4- and 8-byte aligned because `SLOT_SIZE` is
                        // 16 and `HEADER_SIZE` is 8-byte aligned.
                        let cell = unsafe { base.add(HEADER_SIZE + i * SLOT_SIZE) };
                        let tag = unsafe {
                            std::ptr::read(
                                cell.add(cratonvm_types::FIELD_CELL_TAG_OFFSET) as *const u32
                            )
                        };
                        let word_ptr =
                            unsafe { cell.add(cratonvm_types::FIELD_CELL_PAYLOAD64_OFFSET) };
                        // A RELAXED ATOMIC load, for the reason spelled out
                        // at the same read in `visit_strong_refs_at`: this
                        // payload word is the word the load barrier CAS-heals
                        // for a legacy field, and it is the word the compaction
                        // rewrite pass in `relocate_stw` stores back through --
                        // this walk is what produces the `slot_addr` that store
                        // uses, so the read and the write have to be the same
                        // kind of access or the pair is still a race. Mixing a
                        // plain read with a `compare_exchange` on one location
                        // is UB; `Relaxed` closes it and costs the same `mov`.
                        //
                        // The TAG read above stays plain -- see that same site.
                        let word =
                            unsafe { (&*(word_ptr as *const AtomicU64)).load(Ordering::Relaxed) };
                        // EVERY slot, tag and all — the census does the tag
                        // arithmetic itself. Filtering to `Value::Object` here is
                        // what would merge the never-written population into the
                        // "no reference fields" one and make the answer wrong.
                        f(census::ZSlotObservation::tagged(
                            census::ZSlotShape::LegacyField,
                            word_ptr as u64,
                            word,
                            tag,
                        ));
                    }
                }
            }
            ObjectKind::Array => {
                if header.element_type() == ArrayElementType::Reference {
                    let len = header.array_length() as usize;
                    // The screen the legacy-object arm above already had, and
                    // this arm did not. Without it a clobbered length strides
                    // straight out of the arena — which is not hypothetical:
                    // it is `zgc::reference_slots` faulting at a page-aligned
                    // address inside `relocate_stw`'s rewrite pass on a
                    // `--nojit` netty run (2/10).
                    //
                    // zs s1/dg: a REAL reference array can be that long
                    // (`ZGC_REAL_MAX_ARRAY_LENGTH` is `i32::MAX`; an `Object[]`
                    // of 300M elements is 2.4 GB, which a large ergonomic heap
                    // admits). Omitting its elements here made the slide's
                    // rewrite pass leave every slot of it naming a moved
                    // object's OLD address -- silently, at `debug`. Such an
                    // array is walked now when the oversize screen sizes it
                    // from independent evidence; an omission is counted and
                    // logged at `error`, because it is a dangling-reference
                    // hazard and not a census detail.
                    if len > Self::MAX_PLAUSIBLE_ARRAY_LEN
                        && self
                            .oversize_live_array_extent(base as usize, header)
                            .is_none()
                    {
                        let n = self
                            .counters
                            .oversize_ref_arrays_omitted
                            .fetch_add(1, Ordering::Relaxed);
                        if n < 16 {
                            tracing::error!(
                                target: "cratonvm::gc::guard",
                                base = base as usize,
                                len,
                                switch_on = self
                                    .counters
                                    .rewrite_oversize_arrays
                                    .load(Ordering::Relaxed),
                                "zgc census: a reference array past MAX_PLAUSIBLE_ARRAY_LEN \
                                 failed the oversize screen -- its elements are OMITTED from \
                                 this slot walk (a rewrite pass cannot see them)"
                            );
                        }
                        return;
                    }
                    // `ref_element_size()`, not the `REF_ELEMENT_SIZE` constant:
                    // the constant is the wide (8-byte) width, and the stride is
                    // 4 under narrow oops. This is the stride
                    // `read_prim_element`'s own reference arm uses, so the census
                    // reads the same words the VM does.
                    let stride = cratonvm_types::ref_element_size();
                    // SAFETY: the data area begins at `base + ARRAY_DATA_OFFSET`
                    // and holds `array_length()` elements of `stride` bytes;
                    // `i < len`.
                    let data = unsafe { base.add(ARRAY_DATA_OFFSET) };
                    for i in 0..len {
                        let slot = unsafe { data.add(i * stride) };
                        // A RAW read. `read_prim_element` would apply
                        // `plausible_heap_pointer` and hand back
                        // `Value::Object(None)` for a colored or corrupt word —
                        // the study's hazard 2 — silently reclassifying the exact
                        // population this census exists to find as null.
                        let raw = unsafe { cratonvm_types::narrow_oop::read_ref_slot(slot) };
                        f(census::ZSlotObservation::bare(
                            census::ZSlotShape::ArrayElement,
                            slot as u64,
                            raw,
                        ));                    }                }
                // Primitive arrays have no reference slots.            
}
            ObjectKind::HumongousFiller => {}        }    }}

/// The concurrent marking engine's view of this heap.
///
/// # This makes the engine POSSIBLE; it does not adopt it
///
/// `gc/src/zgc/mark.rs` is a complete concurrent marker (striped
/// work-stealing stacks, a termination handshake, a mutator ingress) and
/// `gc/src/zgc_concurrent.rs` is a complete driver for it, but until this impl
/// existed nothing implemented [`mark::ZMarkContext`] for a real heap, so both
/// were unreachable code that compiled and marked nothing.
///
/// [`ZgcRealHeap::collect_garbage`] is deliberately unchanged: it still runs
/// its own single-threaded stop-the-world mark loop, and this impl is not
/// wired into it. Switching the production path from that loop to the
/// concurrent engine is a separate and much riskier change — it needs a
/// mark-start safepoint, a mutator write barrier feeding
/// [`mark::ZMarkIngress`], and a decision about what happens to
/// [`Self::collect_garbage_with_finalizers`]'s resurrection pass — and it is
/// not this change's call to make. Two consequences follow, and both are
/// intentional:
///
/// * The two markers now enumerate references through *different* code
///   ([`ZgcRealHeap::visit_strong_refs_at`] here,
///   [`ZgcRealHeap::enumerate_references`] there). They now agree on the words
///   they load — the narrow-oop defect that once separated them has been fixed
///   at the source; see [`ZgcRealHeap::visit_strong_refs_at`]'s doc comment for
///   what remains a deliberate difference (shape, and the legacy arm's
///   tear-safe raw tag/payload read).
/// * Nothing calls [`ZgcRealHeap::begin_concurrent_mark_cycle`] yet, so on
///   today's execution path this impl is inert.
///
/// # Address domain
///
/// Every `u64` crossing this trait is an unmasked machine address of an object
/// base, never a [`vaddr::ZColoredWord`]. This heap stores raw pointers in its
/// slots and never colors them, so there is nothing to uncolor on the way in;
/// a colored word that somehow reached a slot would be refused by
/// [`Self::is_in_heap`] like any other wild child, which is the outcome
/// `vaddr`'s bit-63 tag exists to produce.
/// Lends a `&ZgcRealHeap` to the mark engine for the duration of ONE
/// stop-the-world collection.
///
/// # Why this exists
///
/// [`mark::ZMarkCoordinator::new`] takes an `Arc<dyn ZMarkContext>`, and
/// `ZgcRealHeap` is held **by value** inside `VmHeap` — there is no `Arc` to
/// hand it and no safe way to mint one. That is the whole of the reason the
/// marking engine sat unadopted after its `ZMarkContext` impl landed; it is not
/// a missing feature, it is an ownership mismatch.
///
/// # Why it is sound
///
/// Three facts, and all three are needed:
///
/// 1. **The bridge is created, used and destroyed inside a single
///    [`ZgcRealHeap::mark_parallel_stw`] call**, which takes `&self`. It is
///    never stored on the heap, never returned, and never handed to anything
///    that outlives that call.
/// 2. **[`mark::ZMarkCoordinator`]'s `Drop` joins every worker.** Unusually
///    for this crate it does not detach — its own doc says so — so when the
///    coordinator goes out of scope, no thread holding a clone of this `Arc`
///    is still running. That covers the panic path as well as the normal one,
///    which is why `mark_parallel_stw` needs no explicit guard.
/// 3. **The heap cannot move while `&self` is live**, and `&self` outlives the
///    coordinator by construction of (1).
///
/// The cost of that safety is a worker-pool spawn and join per collection.
/// That is real and it is why this path is opt-in; caching the pool across
/// collections would require the heap to be `Arc`-owned, which is the larger
/// change this deliberately does not make.
pub struct ZHeapMarkBridge {
    pub heap: *const ZgcRealHeap,
}

// SAFETY: the pointer is only ever dereferenced through `ZMarkContext`, whose
// methods all take `&self` on a heap that is Sync; and the bridge cannot
// outlive the borrow it was built from — see the type doc's three facts. The
// `!Send`ness being overridden here is `*const T`'s blanket one, not a
// property of `ZgcRealHeap`.
unsafe impl Send for ZHeapMarkBridge {}
// SAFETY: as above.
unsafe impl Sync for ZHeapMarkBridge {}

impl ZHeapMarkBridge {
    #[inline]
    pub fn heap(&self) -> &ZgcRealHeap {
        // SAFETY: see the type doc. The referent outlives every call.
        unsafe { &*self.heap }
    }
}

impl mark::ZMarkContext for ZHeapMarkBridge {
    fn good_mask(&self) -> u64 {
        self.heap().good_mask()
    }
    fn try_mark(&self, addr: u64) -> bool {
        self.heap().try_mark(addr)
    }
    fn is_marked(&self, addr: u64) -> bool {
        self.heap().is_marked(addr)
    }
    fn visit_refs(&self, addr: u64, f: &mut dyn FnMut(u64)) {
        self.heap().visit_refs(addr, f)
    }
    fn is_in_heap(&self, addr: u64) -> bool {
        self.heap().is_in_heap(addr)
    }
    fn object_size(&self, addr: u64) -> usize {
        self.heap().object_size(addr)
    }
    /// Forwarded, NOT inherited. The trait's default is one `visit_refs` that
    /// never splits, so a wrapper that stops at `visit_refs` silently reverts
    /// [`ZgcRealHeap`]'s chunked array walk and with it the yield-latency bound
    /// on a huge reference array -- no compile error, no log line, identical
    /// mark set, and `ZMarkStats::ref_chunks` stuck at zero as the only tell.
    fn visit_refs_chunked(
        &self,
        addr: u64,
        cursor: usize,
        budget: usize,
        f: &mut dyn FnMut(u64),
    ) -> Option<usize> {
        self.heap().visit_refs_chunked(addr, cursor, budget, f)
    }
    /// Forwarded for the same reason, and this half predates the round. The
    /// default is `None`, and `ZgcRealHeap::heap_base`'s own doc says `None`
    /// makes `ZMarkHandle::mark_live_offset` "silently discard everything it is
    /// handed".
    fn heap_base(&self) -> Option<u64> {
        self.heap().heap_base()
    }
    /// Forwarded under the rule below (zr z1/mk added the override). The
    /// default is the same decision through two calls, so a missing forward
    /// would be slower rather than wrong -- forwarded anyway, so the
    /// `CRATONVM_ZGC_MARK_CTX_DIRECT=0` arm stays an A/B over ONE hop.
    #[inline(always)]
    fn claim_child(&self, addr: u64) -> mark::ZChildClaim {
        self.heap().claim_child(addr)
    }
    /// Forwarded under the same rule (zs s2/mc2): the default is the closure
    /// path, so a missing forward would make `CRATONVM_ZGC_MARK_CTX_DIRECT=0`
    /// also turn the direct scan off.
    fn scan_and_claim(
        &self,
        addr: u64,
        cursor: usize,
        budget: usize,
        scratch: &mut mark::ZMarkScratch,
        sink: &mut mark::ZMarkEdgeSink<'_>,
    ) -> Option<usize> {
        self.heap()
            .scan_and_claim(addr, cursor, budget, scratch, sink)
    }
    /// Forwarded under the same rule (zs s5/mc3): the default is `None`, so a
    /// missing forward would make `CRATONVM_ZGC_MARK_CTX_DIRECT=0` also turn
    /// the stealable array chunks off.
    fn ref_array_len(&self, addr: u64) -> Option<usize> {
        self.heap().ref_array_len(addr)
    }
}

// THE RULE THIS BRIDGE LIVES UNDER. Every `ZMarkContext` method `ZgcRealHeap`
// overrides must be forwarded here, defaulted or not. `zgc_mark_ctx_direct`
// documents the wrapper arm as the same collector minus one indirect hop per
// call, and `CRATONVM_ZGC_MARK_CTX_DIRECT=0` is sold as an A/B over dispatch
// cost -- both claims are false the moment a forward is missing, because the
// arm then also differs in what it marks or how it yields. A defaulted method
// is a question an implementor can forget, which is the argument
// `zgc::relocate` used to make `is_object_base` / `is_pinned` required;
// `gc/tests/zgc_m_delegating_mark_context.rs` states the property executably,
// since the bridge itself is private.

/// Phase 4 of the ZGC maturity plan: the load-barrier seam.
///
/// # What this is, and what it is emphatically not
///
/// This is the `ZBarrierContext` the built-and-unadopted `zgc::barrier` module
/// has been waiting for. Implementing it makes the barrier *drivable* against
/// this heap. It does **not** put a barrier on any read path: no interpreter
/// `getfield`, no JIT-emitted load and no native accessor calls
/// `load_barrier_fast` today, so in a normal run every method below is
/// unreachable and `good_mask` never leaves `Z_REMAPPED`.
///
/// That distinction is the whole discipline of this phase, and the hazard it
/// guards is real: a COLORED slot that a JIT-compiled load reads without a
/// barrier is a use-after-free, and no amount of correctness *here* changes
/// that.
///
/// What this comment used to say -- that `zgc_relocation_permitted` "stays shut
/// with the JIT on" -- is **no longer true** and was the third copy of the
/// claim
/// `docs/feature-designs/zgc-architecture.md`
/// corrects. That gate has answered `true` for a JIT run since stage (a) of
/// `zgc-jit-load-barrier.md` landed (2026-08-13), because compiled
/// compact-field accesses route through `jit_getfield` / `jit_putfield_object`,
/// which barrier. What keeps *this* phase safe is not a refusal but the fact
/// that nothing in a normal run stores a colored word at all: the slots hold
/// plain machine pointers, so there is no colour for an unbarriered load to
/// misread. The day that changes, the gate is not what will save it -- the
/// emission gate above (`zgc_read_barrier_blocks_inline_fields`) is.
impl barrier::ZBarrierContext for ZgcRealHeap {
    fn good_mask(&self) -> u64 {
        self.barrier_good_mask.load(Ordering::Acquire)
    }

    /// zt t33: `barrier_heal_mask`, the good mask outside the closing pause
    /// of a cycle R6 may plan (the field's doc).
    fn heal_color(&self) -> u64 {
        vaddr::Z_COLORED_TAG | self.barrier_heal_mask.load(Ordering::Acquire)
    }

    fn is_marking(&self) -> bool {
        // zt t33: not in the closing pause's heal window (after its remap
        // flip, while R6 may plan): the mark is closed there, and the words
        // the pause healed stay in the mark color (`barrier_heal_mask`), so a
        // strong read of one (a referent a weak read healed) would take the
        // slow path and mark what the pause had not, keeping weak referents
        // alive (`Zt31Zc11RoundStress` stopped enqueueing). Outside R6 the
        // window never opens and this is `mark_active`.
        self.mark_active.load(Ordering::Relaxed)
            && self.barrier_heal_mask.load(Ordering::Relaxed)
                == self.barrier_good_mask.load(Ordering::Relaxed)
    }

    fn is_relocating(&self) -> bool {
        // zt t31/zc11 (R4): a published kept cycle forwards stale words too.
        // zt t32 (R6): and a published plan, which copies on the way.
        self.relocate_active.load(Ordering::Relaxed)
            || self.counters.rim.cycle_published()
            || self.counters.crl.published()
    }

    /// Map a heap **offset** to where that object lives now.
    ///
    /// Identity for anything this cycle has not moved, which is every object
    /// while `relocate_active` is false — so a caller that reaches the slow
    /// path outside a relocating cycle gets its own offset back rather than a
    /// `None` the barrier would turn into
    /// [`on_forward_failure`](barrier::ZBarrierContext::on_forward_failure)'s
    /// panic.
    ///
    /// Returns a **bare offset**, never an address: `is_bare_offset` on the
    /// barrier's slow path checks exactly that, in release builds too, because
    /// returning an address here was the shape that made this method's return
    /// type `Option<u64>` in the first place.
    fn forward(&self, addr: u64) -> Option<u64> {
        if !self.relocate_active.load(Ordering::Relaxed) {
            // zt t31/zc11 (R4): a word the slide left stale (`remap_in_mark`);
            // identity for everything the kept cycle did not move.
            return Some(self.remap_in_mark_forward_offset(addr));
        }
        // THE SAME TABLE the slide writes and `load_and_forward` reads
        // ([`Self::note_relocation`] / [`Self::counters::relocations`]), not a
        // second one.
        //
        // Until 2026-09-03 this read a `counters.forwarding:
        // Mutex<FxHashMap<u64, u64>>` that NOTHING in the collector ever
        // inserted into -- the only writer in the tree was the one unit test
        // below that arms the barrier by hand. So the barrier's forward step
        // was structurally incapable of answering: had `relocate_active` ever
        // been set by a real slide, every lookup would have missed and every
        // stale reference would have been "healed" back to the address the
        // object had just been moved off. It was a duplicate of `relocations`
        // that had been left empty, keyed in offsets where `relocations` is
        // keyed in addresses, and the difference between the two was the only
        // reason it looked like a different table.
        // `self.arena_base`, not `self.arena.lock().base_ptr()`: this is a
        // BARRIER slow path, reached from an ordinary field or element read,
        // and a barrier that takes the allocator lock is a barrier that can
        // deadlock against any caller holding it. The field is fixed at
        // construction and documented as safe to read without the lock.
        let base = self.arena_base as u64;
        let from = base.checked_add(addr)? as usize;
        let to = self.counters.relocations.lock().get(&from).map(|e| e.0);
        match to {
            Some(to) => Some((to as u64).checked_sub(base)?),
            None => Some(addr),
        }
    }

    /// Publish `addr` (an offset) to the concurrent marker.
    ///
    /// Shares the ingress `satb_pre_barrier` feeds — one queue per heap, so a
    /// cycle drains mutator-published work from the store barrier and the load
    /// barrier together rather than needing two drains that could disagree
    /// about when they are empty.
    fn mark_live(&self, addr: u64) {
        // zt t22/zb5: no `addr == 0 => return` any more. `addr` is an OFFSET,
        // and offset 0 is the object at the arena base (the null word never
        // reaches the slow path: `load_barrier_slow` returns before marking),
        // so the old early return left that one object unmarked by the load
        // barrier -- the same null-versus-offset-0 slip zt t21/zb4 fixed in
        // `load_barrier_slot`. The registry screen below answers for a base
        // that holds no object.
        //
        // And `self.arena_base`, not `ZMarkContext::heap_base` (which takes
        // the arena lock): the color flip makes this a MUTATOR path -- the
        // first load of a `Remapped` slot during a mark phase lands here from
        // an ordinary field or element read -- and `forward` above states why
        // a barrier must not take that lock. The two are one address.
        let base = self.arena_base as u64;
        let absolute = base.wrapping_add(addr) as usize;
        if !self.registry.contains(absolute) {
            return;
        }
        // zt t32 (2f positive control, `CRATONVM_DBG_ZGC_LBM_NO_LOAD_MARK`).
        if self.counters.lbm_no_load_mark.load(Ordering::Relaxed) {
            return;
        }
        // The count comes back from the bucket's own lock; there is no shared
        // counter on this path either, and the key is per-thread for the same
        // reason. See `satb_pre_barrier_slow` and `satb_ingress_slot`.
        let _ = self
            .counters
            .mark_ingress
            .push(satb_ingress_slot(), absolute as u64);
    }

    fn stats(&self) -> &barrier::ZBarrierStats {
        &self.counters.barrier_stats
    }
}

impl ZgcRealHeap {
    /// The four edges an object has that are not stored in the object: its
    /// class's pinned defining loader, and the mirrors and metadata a loader
    /// owns. Written once and called by all three mark loops, so they cannot
    /// disagree about what is reachable.
    ///
    /// Each of the three tables is a global `RwLock` over a hash map, and each
    /// answer is overwhelmingly "nothing" -- so the filter is asked first, and
    /// answers from a lock-free bitmap or bloom. See [`mark_roots`].
    /// `(may_own_pins, may_own_overlay)` for this object, from ONE bloom probe.
    ///
    /// Both edge visitors below take the pair rather than asking the filter
    /// themselves. They are called back to back for the same object by all
    /// three mark loops, and each used to ask `may_own_extra_roots(base)` --
    /// the second by way of `may_own_overlay` -- so every marked object paid
    /// the splitmix64 mixing pass, the two `addr_bloom` word loads and the
    /// `armed` acquire-load TWICE, for an answer that cannot change in
    /// between: the filter is rebuilt once per collection
    /// (`counters.mark_root_filter.rebuild()` at the head of `collect_garbage`)
    /// and is read-only for the rest of it.
    ///
    /// That is the entire per-object cost of the filter, doubled. The absolute
    /// number is small, which is the point: the filter exists because small
    /// per-object costs ARE the mark pause -- `mark_roots`'s own header records
    /// `mark_us` halving when four global lookups per object were removed.
    ///
    /// `may_pin_loader` is deliberately not folded in. It is keyed by CLASS ID
    /// rather than by address and answers from the class bitmap rather than
    /// from the bloom, so it shares nothing with this probe.
    #[inline]
    pub fn extra_root_probe(&self, base: usize) -> (bool, bool) {
        self.counters.mark_root_filter.extra_root_probe(base)
    }

    #[inline]
    pub fn visit_pin_edges(
        &self,
        base: usize,
        class_id: u32,
        may_own_pins: bool,
        f: &mut dyn FnMut(usize),
    ) {
        let filter = &self.counters.mark_root_filter;
        if filter.may_pin_loader(class_id) {
            // The row inside this heap's arena envelope when several live
            // VMs share the class id (the `common-w4b` loader-pin collision):
            // live heaps do not overlap, so that is this VM's defining loader.
            // The envelope is immutable and lock-free, as the predicate must
            // be. A base the registry spilled outside the envelope is not
            // accepted, which falls back to the VM-less answer -- never worse.
            let (arena_base, arena_end) = (self.arena_base, self.arena_end);
            if let Some(loader) = cratonvm_types::loader_pin::loader_pin_addr_where(class_id, |a| {
                (arena_base..arena_end).contains(&a)
            }) {
                f(loader);
            }
        }
        // The caller's half of `extra_root_probe`; see that method.
        if !may_own_pins {
            return;
        }
        if let Some(mirrors) = cratonvm_types::mirror_pin::mirrors_for_loader(base) {
            for m in mirrors {
                f(m);
            }
        }
        if let Some(metadata) = cratonvm_types::metadata_pin::roots_for_loader(base) {
            for m in metadata {
                f(m);
            }
        }
    }

    /// The native collection-overlay edge, behind the same filter.
    ///
    /// Split from [`Self::visit_pin_edges`] only because one caller
    /// (`visit_refs`) carries a long comment about why this edge exists at all
    /// and it belongs next to the call.
    #[inline]
    pub fn visit_overlay_edges(
        &self,
        base: usize,
        class_id: u32,
        may_own_overlay: bool,
        f: &mut dyn FnMut(usize),
    ) {
        // The ONE table with no `NON_EMPTY` latch that ever fires:
        // `native-collections` registers its provider at VM startup, so
        // `PROVIDER_COUNT` is non-zero on every run and every marked object
        // used to take the provider lock and that module's own mutex to be
        // told it owns nothing. This test is what replaces both -- and it is
        // now the SECOND half of the caller's `extra_root_probe`, rather than a
        // second probe of the same bloom with the same key.
        if !may_own_overlay {
            return;
        }
        // The scratch-buffer form: past the latch above this runs per marked
        // object, and the returning form allocates for every owner that has
        // anything.
        cratonvm_gc_core::external_roots::with_external_roots_for_owner(base, Some(class_id), |refs| {
            for overlay_ref in refs {
                f(overlay_ref.as_ptr() as usize);
            }
        });
    }

    /// The pin and overlay edges of ONE object, as
    /// [`mark::ZMarkContext::visit_refs`] emits them.
    ///
    /// Extracted so that [`Self::visit_refs`] and
    /// [`Self::visit_refs_chunked`](mark::ZMarkContext::visit_refs_chunked)
    /// cannot drift. These edges are per **object**, not per reference slot:
    /// the chunked walk must emit them exactly once, on the chunk that finishes
    /// the object. Emitting them on every chunk is not incorrect — the children
    /// are already marked — but it multiplies the per-object cost this round
    /// spent effort removing (see [`Self::extra_root_probe`]); emitting them on
    /// no chunk drops a class loader's mirrors, which is a use-after-free.
    #[inline]
    pub fn visit_extra_object_edges(&self, base: usize, f: &mut dyn FnMut(u64)) {
        // The pin edges, in `collect_garbage`'s order. `class_id` is read
        // through the shared header view, like every other read here.
        let class_id = self.header_ref(base as *mut u8).class_id.as_u32();
        self.visit_extra_object_edges_of(base, class_id, f);
    }

    /// The referent-hole field index of the object at `base` for a mark visit:
    /// `Some(0)` for a registered `Weak`/`Soft`/`Phantom`/`Cleaner` `Reference`
    /// in the open cycle's skip set, `None` otherwise -- including when no cycle
    /// is open, which traces the referent strongly and latches the one-shot
    /// warning (see [`Self::concurrent_mark_skip_set`]).
    ///
    /// Split out of [`mark::ZMarkContext::visit_refs`] (zs s2/mc2) so the
    /// direct scan (`scan_and_claim`) asks the identical question.
    #[inline]
    pub fn mark_skip_index(&self, base: usize) -> Option<usize> {
        self.mark_skip_index_classed(base, None)
    }

    /// zt t27/zc10 (`CRATONVM_ZGC_MARK_SKIP_CLASS_FILTER`): is the object at
    /// `base` certainly NOT in the open cycle's skip set, by its class? `false`
    /// when the member-class table is not in use. `class_id` is the caller's
    /// decode, or `None` to read it from the header (only when the table is in
    /// use).
    #[inline]
    pub fn skip_class_excludes(&self, base: usize, class_id: Option<u32>) -> bool {
        let table = &self.counters.skipcls;
        if !table.usable() {
            return false;
        }
        let class_id = match class_id {
            Some(id) => id,
            None => self.header_ref(base as *mut u8).class_id.as_u32(),
        };
        !table.may_contain(class_id)
    }

    /// [`Self::mark_skip_index`] for a caller that has already decoded the
    /// object's class id (`scan_and_claim`), so the member-class table costs
    /// no header read (zt t27/zc10, `CRATONVM_ZGC_MARK_SKIP_CLASS_FILTER`).
    /// `None` reads it from the header, only when the table is in use.
    #[inline]
    pub fn note_weak_phantom_class(&self, class_id: u32, kind_code: u8) {
        let idx = class_id as usize;
        if idx < self.counters.weak_phantom_class_kind.len() {
            let slot = &self.counters.weak_phantom_class_kind[idx];
            if slot.load(Ordering::Relaxed) == 0 {
                slot.store(kind_code, Ordering::Release);
            }
        } else {
            let mut map = self.counters.weak_phantom_class_overflow.write();
            map.entry(class_id).or_insert(kind_code);
        }
    }

    #[inline]
    pub fn weak_phantom_class_kind(&self, base: usize, class_id: Option<u32>) -> Option<(u32, u8)> {
        let cid = match class_id {
            Some(id) => id,
            None => self.header_ref(base as *mut u8).class_id.as_u32(),
        };
        let idx = cid as usize;
        let code = if idx < self.counters.weak_phantom_class_kind.len() {
            self.counters.weak_phantom_class_kind[idx].load(Ordering::Relaxed)
        } else {
            self.counters.weak_phantom_class_overflow.read().get(&cid).copied().unwrap_or(0)
        };
        (code != 0).then_some((cid, code))
    }

    #[inline]
    pub fn record_discovered_weak_phantom(&self, base: usize, cid: u32, kind_code: u8) {
        let shard = (base >> 4) & 15;
        self.counters.discovered_weak_phantom[shard].lock().push((base, cid, kind_code));
    }

    pub fn finalize_discovered_weak_phantom(&self) {
        let mut out = self.counters.finalized_weak_phantom.lock();
        out.clear();
        for shard in self.counters.discovered_weak_phantom.iter() {
            let mut v = shard.lock();
            for (base, cid, kind_code) in v.drain(..) {
                let obj = unsafe { ObjectRef::from_raw(base as *mut u8) };
                let raw_ref = match self.get_field(obj, 0) {
                    Value::Object(Some(r)) => Some(r.as_ptr() as usize),
                    _ => None,
                };
                let alive = raw_ref.is_some_and(|r| self.is_marked_addr(r));
                out.push((base, cid, kind_code, raw_ref, alive));
            }
        }
    }

    pub fn take_discovered_weak_phantom(&self) -> Vec<(usize, u32, u8, Option<usize>, bool)> {
        std::mem::take(&mut *self.counters.finalized_weak_phantom.lock())
    }

    #[inline]
    pub fn mark_skip_index_classed(&self, base: usize, class_id: Option<u32>) -> Option<usize> {
        // THE FILTER FIRST -- see `mark_ref_skip_bloom`. `armed` plus two relaxed
        // loads answers this for ~99.6% of objects without touching the `RwLock`
        // or the `Arc`, which between them were three contended atomic RMWs per
        // object on every mark worker.
        if !self.mark_ref_skip_armed.load(Ordering::Acquire) {
            // No cycle open. Trace everything (including referents) and say so
            // ONCE -- see `concurrent_mark_skip_set` for why leaking beats
            // dropping an edge, and why the latch is not optional.
            if !self
                .counters
                .mark_ref_skip_warned
                .swap(true, Ordering::Relaxed)
            {
                tracing::warn!(
                    target: "zgc",
                    "zgc concurrent mark: visit_refs ran with no skip-set snapshot; \
                     begin_concurrent_mark_cycle was not called, so weak/soft/phantom \
                     referents are being traced as STRONG edges and cannot be cleared"
                );
            }
            None
        } else {
            if let Some((cid, kind_code)) = self.weak_phantom_class_kind(base, class_id) {
                self.record_discovered_weak_phantom(base, cid, kind_code);
                return Some(0usize);
            }
            let class_excludes = self.skip_class_excludes(base, class_id);
            if let Some(member) =
                self.counters
                    .skipbits
                    .answer_classed(base, class_id, class_excludes)
            {
                // zt t31/zc11 (`CRATONVM_ZGC_MARK_SKIP_BITS`): exact, one bit test
                // -- every member's bit is set before the arm and none is cleared
                // while it stands (`skip_bits`), fronted by `skipcls` +
                // `persistent_classes` so non-Reference objects skip the
                // whole-heap bitmap reads.
                member.then_some(0usize)
            } else if class_excludes {
                // zt t27/zc10: definitive too -- every member's class bit is set
                // before the arm and never cleared while the cycle is armed
                // (`skip_class_filter`). Ahead of the Bloom filter because a heap
                // with many live `Reference`s saturates that one (200 000 members
                // set all its 32 768 bits), and then every object took the lock.
                None
            } else if !self.skip_bloom_may_contain(base) {
                // Definitive: a member always sets both its bits, so a filter miss
                // cannot be a false negative.
                None
            } else {
            match self.concurrent_mark_skip_set() {
                Some(skip) => {
                    if skip.contains(&base) {
                        Some(0usize)
                    } else {
                        None
                    }
                }
                None => {
                    // No cycle open. Trace everything (including referents) and
                    // say so ONCE — see `concurrent_mark_skip_set` for why leaking
                    // beats dropping an edge, and why the latch is not optional.
                    if !self
                        .counters
                        .mark_ref_skip_warned
                        .swap(true, Ordering::Relaxed)
                    {
                        tracing::warn!(
                            target: "zgc",
                            "zgc concurrent mark: visit_refs ran with no skip-set snapshot; \
                             begin_concurrent_mark_cycle was not called, so weak/soft/phantom \
                             referents are being traced as STRONG edges and cannot be cleared"
                        );
                    }
                    None
                }
            }
            }
        }
    }

    /// [`Self::visit_extra_object_edges`] for a caller that has already read
    /// the header's `class_id` (zr zs s1/mc, the one-decode arm).
    #[inline(always)]
    pub fn visit_extra_object_edges_of(&self, base: usize, class_id: u32, f: &mut dyn FnMut(u64)) {
        let (may_own_pins, may_own_overlay) = self.extra_root_probe(base);
        self.visit_pin_edges(base, class_id, may_own_pins, &mut |edge| {
            if dbg_zgc_wild() {
                self.dbg_wild_edge(base, "pin", 0, edge as u64);
            }
            f(edge as u64)
        });
        if may_own_overlay {
            self.visit_overlay_edges(base, class_id, true, &mut |edge| {
                if dbg_zgc_wild() {
                    self.dbg_wild_edge(base, "overlay", 0, edge as u64);
                }
                f(edge as u64)
            });
        }
    }

    /// Emit the reference-array slots `[start, end)` of the array based at
    /// `base`, exactly as [`Self::visit_strong_refs_at`]'s `ObjectKind::Array`
    /// arm does.
    ///
    /// The caller has already established that this header is an
    /// `ObjectKind::Array` with `element_type() == Reference` and that
    /// `end <= array_length()`.
    ///
    /// # Safety
    ///
    /// Same as the arm it was split from: the data area begins at
    /// `base + ARRAY_DATA_OFFSET` and holds `array_length()` elements of
    /// `ref_element_size()` bytes, and `end` is bounded by that length, so every
    /// slot read lies inside the allocation. `read_ref_slot` reads exactly
    /// `ref_field_size()` bytes — the narrow (4-byte) load under compressed
    /// oops, never a raw 8-byte read.
    ///
    /// Generic over the visitor (zs s2/mc2): a `&mut dyn FnMut` caller is the
    /// `F = dyn FnMut(u64)` instance, as before; the direct scan
    /// (`scan_and_claim`) passes its claim closure by type, so the per-slot
    /// claim inlines into this loop.
    #[inline]
    pub fn visit_array_ref_slots<F: FnMut(u64) + ?Sized>(
        &self,
        base: usize,
        start: usize,
        end: usize,
        f: &mut F,
    ) {
        // `ref_element_size()`, not the wide `REF_ELEMENT_SIZE` constant: the
        // stride is 4 under narrow oops.
        let stride = cratonvm_types::ref_element_size();
        let data = unsafe { (base as *mut u8).add(ARRAY_DATA_OFFSET) };
        let dbg_wild = dbg_zgc_wild();
        let heals = self.remap_in_mark_heals();
        if !dbg_wild && !heals {
            if stride == 8 {
                let ptrs = unsafe { std::slice::from_raw_parts(data as *const u64, end) };
                for &raw in &ptrs[start..end] {
                    if raw != 0 {
                        f(raw);
                    }
                }
            } else {
                for i in start..end {
                    let slot = unsafe { data.add(i * stride) };
                    let raw = unsafe { cratonvm_types::narrow_oop::read_ref_slot(slot) };
                    if raw != 0 {
                        f(raw);
                    }
                }
            }
            return;
        }
        for i in start..end {
            let slot = unsafe { data.add(i * stride) };
            let raw = unsafe { cratonvm_types::narrow_oop::read_ref_slot(slot) };
            if raw != 0 {
                if dbg_wild {
                    self.dbg_wild_edge(base, "elem", i, raw);
                }
                f(self.mark_slot_word(slot, raw));
            }
        }
    }

    /// The direct scan's compact-object walk through the worker's
    /// direct-mapped oop-map memo (zs s2/mc2, `CRATONVM_ZGC_MARK_DIRECT_SCAN`,
    /// `CRATONVM_ZGC_MARK_MEMO_WAYS`): the `ref_disps` of `(class_id,
    /// num_slots)` at the current layout generation
    /// ([`mark::ZMarkScratch::layout_ref_disps`]), copied into `scratch` on a
    /// miss, then each non-null slot to
    /// `f` in `ref_disps` order -- the walk `visit_strong_refs_at`'s oop-map
    /// arm makes for an object with no referent hole.
    ///
    /// Returns `false` (and has called `f` for nothing) when no layout is
    /// registered for the key: the caller then takes `visit_strong_refs_at`,
    /// whose loud warning for that case stays in one place.
    ///
    /// The generation is read BEFORE the lookup, so a registration racing the
    /// fill can only tag the memo with an older generation than the layout it
    /// holds; the next object then misses and refills. That is the rule
    /// `with_class_layout`'s own per-thread cache keys on, so a hit answers
    /// what that lookup would -- concurrently with mutators as well as in a
    /// pause.
    #[inline]
    pub fn claim_compact_via_memo<F: FnMut(u64)>(
        &self,
        base: usize,
        class_id: u32,
        num_slots: u32,
        skip_index: Option<usize>,
        scratch: &mut mark::ZMarkScratch,
        f: &mut F,
    ) -> bool {
        let generation = cratonvm_types::layout_generation();
        let Some((ref_disps, skip0_disp)) =
            scratch.layout_ref_disps_with_skip0(class_id, num_slots, generation, |disps| {
                cratonvm_types::with_class_layout(class_id, num_slots, |layout| {
                    disps.clear();
                    disps.extend_from_slice(&layout.ref_disps);
                    layout.field_disp(0)
                })
            })
        else {
            return false;
        };
        let skip_disp = if skip_index == Some(0) {
            skip0_disp
        } else {
            None
        };
        let p = base as *mut u8;
        let heals = self.remap_in_mark_heals();
        if !heals && skip_disp.is_none() && ref_disps.len() <= 8 {
            let mut children = [0u64; 8];
            let mut n = 0usize;
            for &offset in ref_disps {
                let slot = unsafe { p.add(offset as usize) };
                let raw = unsafe { cratonvm_types::narrow_oop::read_ref_slot(slot) };
                if raw != 0 {
                    children[n] = raw;
                    n += 1;
                }
            }
            if n == 0 {
                return true;
            }
            if n >= 2 {
                if let (Some(mb), Some(rb)) = (&self.mark_bits, self.registry.bitmap()) {
                    let same = rb.same_grid(mb);
                    for &c in &children[0..n] {
                        if let Some((w, _)) = mb.locate(c as usize) {
                            unsafe {
                                mb.prefetch_word(w);
                                if same {
                                    rb.prefetch_word(w);
                                }
                            }
                        }
                    }
                }
                mark::prefetch_read(children[n - 1]);
                if n == 2 {
                    mark::prefetch_read(children[0]);
                }
            } else {
                mark::prefetch_read(children[0]);
            }
            for &c in &children[0..n] {
                f(c);
            }
            return true;
        }
        for &offset in ref_disps {
            if skip_disp == Some(offset) {
                continue;
            }
            let slot = unsafe { p.add(offset as usize) };
            let raw = unsafe { cratonvm_types::narrow_oop::read_ref_slot(slot) };
            if raw != 0 {
                let w = if heals {
                    self.mark_slot_word(slot, raw)
                } else {
                    raw
                };
                f(w);
            }
        }
        true
    }
}

impl mark::ZMarkContext for ZgcRealHeap {
    /// [`vaddr::Z_REMAPPED`] — the quiescent good mask, unconditionally.
    ///
    /// # Why a constant and not a cycle-flipped mask
    ///
    /// Colored pointers are not on this heap's live path. `ZgcRealHeap` keeps
    /// liveness in the object header's [`GC_FLAG_MARKED`] bit, not in a
    /// pointer's metadata bits, and the engine states that it does not branch
    /// on this value: it is surfaced for logging and for the load barrier,
    /// neither of which this heap drives. Any answer is therefore inert, and
    /// the only question is which one is *honest*.
    ///
    /// `Z_REMAPPED` is what [`vaddr::ZGoodMask::new`] installs and what
    /// `flip_to_remap` returns — the "no mark parity is good; addresses are
    /// plain" state. It is also what [`mark::mark_color_for`] maps to `None`,
    /// so a metrics line reads "this cycle has no mark color" rather than
    /// claiming a `Marked0`/`Marked1` parity that nothing in this heap
    /// maintains. Returning a mark color instead would be a lie that a future
    /// barrier could act on.
    ///
    /// When colored pointers do land, this becomes a [`vaddr::ZGoodMask`]
    /// field on the heap and this method becomes `self.good_mask.good()`; the
    /// flip belongs to whoever owns the phase machine, not here.
    fn good_mask(&self) -> u64 {
        vaddr::Z_REMAPPED
    }

    /// Atomically claim the object at `addr` for this cycle.
    ///
    /// # Why `try_add_gc_flags` and not the STW marker's read-then-set
    ///
    /// `collect_garbage`'s loop tests `gc_flags() & GC_FLAG_MARKED` and then
    /// calls `add_gc_flags`, which is correct only because it is the sole
    /// thread running. With N workers plus mutators, two threads can both read
    /// "unmarked" and both proceed: the harmless outcome is a double scan, but
    /// the caller's contract here is *"`true` means you now own the obligation
    /// to scan it"*, so two `true`s for one object is only wasteful while a
    /// lost update is fatal — the object is pushed once, popped once, and its
    /// out-edges are never traced. Its children are then swept while it is
    /// alive and pointing at them.
    ///
    /// [`ObjectHeader::try_add_gc_flags`] is the exactly-once primitive that
    /// closes it: a CAS loop with `AcqRel`/`Acquire`, returning `true` iff
    /// *this* call transitioned the bits. Its sibling `add_gc_flags` returns
    /// nothing and cannot be used here.
    ///
    /// **Only [`GC_FLAG_MARKED`] is claimed**, never a set that also carries
    /// [`cratonvm_types::GC_FLAG_OLD_GEN`] or
    /// [`cratonvm_types::GC_FLAG_COMPACT`]. The claim is all-or-nothing
    /// by design, so a set containing a sticky flag that is already set loses
    /// forever and the object would never be marked at all.
    ///
    /// The engine calls [`Self::is_in_heap`] immediately before every
    /// `try_mark`; the null screen below is the only extra gate, since a
    /// header write through an unregistered address is the wild-write cascade
    /// this collector's `wild_skipped` counter exists to prevent.
    fn try_mark(&self, addr: u64) -> bool {
        if addr == 0 {
            return false;
        }
        debug_assert!(
            self.registry.contains(addr as usize),
            "try_mark on an address the engine did not gate through is_in_heap"
        );
        self.mark_claim(addr as usize)
    }

    /// Is the object at `addr` marked live for this cycle? **Query only.**
    ///
    /// The non-strong reference phase asks this about every referent to decide
    /// whether the `Reference` must be cleared. An implementation that marked
    /// as a side effect would answer "live, because I just made it live" for
    /// every referent, and no weak, soft, phantom or cleaner reference would
    /// ever be cleared again — a leak that no assertion in the reference tests
    /// would catch, because every one of them is satisfied by "the referent
    /// survived".
    ///
    /// This is [`Self::is_marked_addr`]'s predicate, read through
    /// [`Self::header_ref`] rather than `header_mut` so that concurrent
    /// callers do not mint aliasing `&mut`s.
    fn is_marked(&self, addr: u64) -> bool {
        if addr == 0 {
            return false;
        }
        self.mark_is_set(addr as usize)
    }

    /// Report the object's **strong** out-edges, plus the three pin edges.
    ///
    /// # The referent hole
    ///
    /// Slot 0 of a registered `Weak`/`Soft`/`Phantom`/`Cleaner` `Reference` is
    /// its referent and must not be reported. Report it and the referent is
    /// reachable through its own `Reference`, so it is always marked, so
    /// `process_references` never clears it: `WeakReference.get()` never
    /// returns null and `Cleaner` never runs. Nothing throws, nothing logs —
    /// the program just leaks. The skip set is the cycle-long snapshot taken
    /// by [`Self::begin_concurrent_mark_cycle`]; see
    /// [`Self::concurrent_mark_skip_set`] for the deliberately-leaky fallback
    /// when no cycle is open.
    ///
    /// # The three pin edges
    ///
    /// `collect_garbage`'s loop pushes them by hand after every
    /// `enumerate_references`, and they are edges, not decoration: the class's
    /// loader ([`cratonvm_types::loader_pin`]), that loader's class mirrors
    /// ([`cratonvm_types::mirror_pin`]) and its metadata roots
    /// ([`cratonvm_types::metadata_pin`]). Dropping them lets a loader whose
    /// only remaining reference is one of its own loaded classes be swept,
    /// taking every mirror and method structure with it — a class-loader
    /// unloading bug that presents as a crash in unrelated reflection, not as
    /// a GC bug. They are reported here for the same reason and in the same
    /// order.
    ///
    /// The Arc handle is cloned and the read guard dropped **before** the walk
    /// begins, so no lock is held across the `f` callback — the engine calls
    /// back into its own stripe pusher from inside it, and a lock held across
    /// that is a cycle.
    fn visit_refs(&self, addr: u64, f: &mut dyn FnMut(u64)) {
        if addr == 0 {
            return;
        }
        let base = addr as usize;

        // The referent hole, from the filter first: see `mark_skip_index`.
        let skip_index = self.mark_skip_index(base);

        self.visit_strong_refs_at(base as *mut u8, skip_index, f);

        // The pin and overlay edges. Shared verbatim with `visit_refs_chunked`,
        // which emits them on its FINAL chunk -- see `visit_extra_object_edges`.
        self.visit_extra_object_edges(base, f);
    }

    /// Resumable form of [`Self::visit_refs`], for the reference-ARRAY arm only.
    ///
    /// *Applies `docs/feature-designs/zgc-architecture.md`,
    /// which closes the `zgc.rs` half of
    /// `gap-a-huge-reference-arrays-bound-the-yield-latency.md`.*
    ///
    /// `cursor` is an **element index** into the array's reference slots. Every
    /// other shape finishes in one call and returns `None`, which is exactly the
    /// inherited default — the split exists for the one shape that can have a
    /// million out-edges, and a `HashMap` table, a `String` intern table and an
    /// `ArrayList` backing array are all that shape.
    ///
    /// # Why the non-array shapes delegate rather than chunk
    ///
    /// A non-array object's edge count is bounded by its field count, which
    /// [`Self::visit_strong_refs_at`] already screens at `1 << 24` and which is
    /// in practice two digits. Chunking it would buy nothing and would oblige
    /// this method to re-derive the compact/legacy field walk with a cursor,
    /// i.e. a second copy of the walk `visit_strong_refs_at`'s header spends a
    /// page explaining was merged down to one.
    ///
    /// # The three gates, and how each is honoured
    ///
    /// 1. **The referent hole.** `skip_index` is computed by [`Self::visit_refs`]
    ///    and is the *field index* of a registered `Reference`'s referent slot.
    ///    It is structurally inapplicable to the array arm:
    ///    `visit_strong_refs_at`'s `ObjectKind::Array` arm does not consult
    ///    `skip_index` at all, because an `ObjectKind::Array` is never a
    ///    registered `Weak`/`Soft`/`Phantom`/`Cleaner` `Reference` object — those
    ///    are `ObjectKind::Object` with a referent in slot 0. So the array chunks
    ///    below report the same edges the unchunked walk would, and the
    ///    non-array path delegates to `visit_refs`, which recomputes the skip
    ///    from `concurrent_mark_skip_set` / `skip_bloom_may_contain` exactly as
    ///    before. The hole is honoured on every chunk because there is never a
    ///    hole to honour on the chunked shape.
    /// 2. **The pin and overlay edges exactly once**, on the chunk that returns
    ///    `None`. They are per object, not per slot.
    /// 3. **The cursor strictly advances.** `budget` is clamped to at least one
    ///    element, so a returned `Some(end)` always has `end > cursor`; the only
    ///    way to return without advancing is to return `None`, which ends the
    ///    object. [`mark::ZMarkWorker::drain`] `debug_assert`s this, and a cursor
    ///    that stood still would present as a GC hang rather than a wrong answer.
    ///
    /// A cursor at or past the array length is treated as "finished": it emits
    /// no slots, emits the per-object edges and returns `None`. That cannot
    /// arise from this method's own arithmetic — `end` is clamped to `len` — but
    /// a shrinking array header read concurrently must not be able to turn into
    /// an out-of-bounds stride.
    fn visit_refs_chunked(
        &self,
        addr: u64,
        cursor: usize,
        budget: usize,
        f: &mut dyn FnMut(u64),
    ) -> Option<usize> {
        if addr == 0 {
            return None;
        }
        let base = addr as usize;

        // Decide the shape from the header ONCE, and hold nothing across `f`.
        //
        // zr zs s1/mc (`CRATONVM_ZGC_MARK_ONE_DECODE`, default ON): the same
        // read also answers "no reference slots at all" (a primitive array or
        // a humongous filler). Such an object has nothing for the
        // referent-hole lookup to hide (only an `ObjectKind::Object` can be a
        // registered `Reference`) and nothing for the strong walk to report,
        // so it goes straight to its per-object pin / overlay edges -- the
        // only edges `visit_refs` would have produced for it, in the same
        // order. On a live set of `int[]` (the `grow` / near-OOM shape) that
        // is the skip-set bloom, the `RwLock`-free armed load, two more header
        // decodes and two call frames per marked object.
        let (array_len, leaf_class) = {
            let header = self.header_ref(base as *mut u8);
            match header.kind() {
                ObjectKind::Array if header.element_type() == ArrayElementType::Reference => {
                    (Some(header.array_length() as usize), None)
                }
                ObjectKind::Array | ObjectKind::HumongousFiller
                    if self.counters.mark_loop.one_decode.load(Ordering::Relaxed) =>
                {
                    (None, Some(header.class_id.as_u32()))
                }
                _ => (None, None),
            }
        };

        if let Some(class_id) = leaf_class {
            // `cursor == 0` for the reason the arm below gives: this shape
            // never returns `Some`, so a non-zero cursor cannot be asked for.
            if cursor == 0 {
                self.visit_extra_object_edges_of(base, class_id, f);
            }
            return None;
        }

        let Some(len) = array_len else {
            // Not the chunkable shape. `cursor == 0` rather than unconditional,
            // for the same reason the trait's default body tests it: this arm
            // never returns `Some`, so the engine can never ask it to resume,
            // and visiting the whole object again would double every edge
            // rather than finish it.
            if cursor == 0 {
                self.visit_refs(addr, f);            }
            return None;
        };

        // At least one element per call, or the cursor cannot advance.
        let budget = budget.max(1);
        let start = cursor.min(len);
        let end = start.saturating_add(budget).min(len);

        self.visit_array_ref_slots(base, start, end, f);

        if end < len {
            // `end > cursor`: `start == cursor` here (cursor < len), and
            // `budget >= 1`, so the engine's advance assertion holds.
            return Some(end);
        }

        // FINAL chunk: the per-object edges, exactly once. See
        // `visit_extra_object_edges`.
        self.visit_extra_object_edges(base, f);
        None
    }

    /// [`Self::visit_refs_chunked`] with the claim inside (zs s2/mc2,
    /// `CRATONVM_ZGC_MARK_DIRECT_SCAN`, default on; see
    /// [`mark::ZMarkContext::scan_and_claim`]).
    ///
    /// Per object: ONE header decode (kind, class id, slot count, compact
    /// flag, array shape) into locals, so nothing borrowed from the header is
    /// held across a claim. Per edge: the slot read, then
    /// [`Self::claim_child`] called statically (inlined) and the outcome
    /// handed to the sink -- no `dyn FnMut` and no `dyn ZMarkContext` call.
    ///
    /// Same edges in the same order as `visit_refs_chunked`:
    /// - reference array: slots `[cursor, cursor + budget)`, the per-object
    ///   pin / overlay edges on the final chunk;
    /// - primitive array / humongous filler: the pin / overlay edges only
    ///   (the one-decode arm; under `CRATONVM_ZGC_MARK_ONE_DECODE=0` this
    ///   delegates to `visit_refs_chunked`, so that kill arm is unchanged);
    /// - Object: the referent hole from [`Self::mark_skip_index`] (the same
    ///   lookup `visit_refs` makes); a compact object with no hole walks its
    ///   oop-map from the worker's memo
    ///   ([`ZgcRealHeap::claim_compact_via_memo`]); a `Reference`, a legacy
    ///   object, `CRATONVM_DBG_ZGC_WILD`, `CRATONVM_ZGC_MARK_REF_DISPS=0` or
    ///   an unresolvable layout take `visit_strong_refs_at`, byte for byte;
    ///   then the pin / overlay edges.
    fn scan_and_claim(
        &self,
        addr: u64,
        cursor: usize,
        budget: usize,
        scratch: &mut mark::ZMarkScratch,
        sink: &mut mark::ZMarkEdgeSink<'_>,
    ) -> Option<usize> {
        if addr == 0 {
            return None;
        }
        let base = addr as usize;
        #[cfg(test)]
        {
            let at = self
                .counters
                .test_worker_scan_panic_at
                .load(Ordering::Relaxed);
            if at != 0
                && at == base
                && std::thread::current()
                    .name()
                    .is_some_and(|n| n.starts_with("zgc-mark-"))
            {
                panic!("zs3rv2 test: injected mark-worker scan panic");
            }
        }
        // ONE decode, into locals.
        let (kind, class_id, num_slots, compact, ref_array_len) = {
            let header = self.header_ref(base as *mut u8);
            let kind = header.kind();
            let ref_array_len = (kind == ObjectKind::Array
                && header.element_type() == ArrayElementType::Reference)
                .then(|| header.array_length() as usize);
            let compact = kind == ObjectKind::Object && cratonvm_types::is_compact_object(header);
            let num_slots = if kind == ObjectKind::Object {
                header.num_slots()
            } else {
                0
            };
            (
                kind,
                header.class_id.as_u32(),
                num_slots,
                compact,
                ref_array_len,
            )
        };
        let mut claim = |child: u64| {
            if child != 0 {
                let outcome = <Self as mark::ZMarkContext>::claim_child(self, child);
                sink.accept(outcome, child);
            }
        };

        if let Some(len) = ref_array_len {
            // As `visit_refs_chunked`'s array arm: at least one element per
            // call, so a returned cursor always advances.
            let budget = budget.max(1);
            let start = cursor.min(len);
            let end = start.saturating_add(budget).min(len);
            self.visit_array_ref_slots(base, start, end, &mut claim);
            if end < len {
                return Some(end);
            }
            self.visit_extra_object_edges_of(base, class_id, &mut claim);
            return None;
        }

        if kind != ObjectKind::Object {
            // A primitive array or a humongous filler.
            if !self.counters.mark_loop.one_decode.load(Ordering::Relaxed) {
                return self.visit_refs_chunked(addr, cursor, budget, &mut claim);
            }
            if cursor == 0 {
                self.visit_extra_object_edges_of(base, class_id, &mut claim);
            }
            return None;
        }

        // An Object finishes in one call; a non-zero cursor cannot be asked
        // for (see `visit_refs_chunked`).
        if cursor != 0 {
            return None;
        }
        if num_slots == 0 {
            self.visit_extra_object_edges_of(base, class_id, &mut claim);
            return None;
        }
        let skip_index = self.mark_skip_index_classed(base, Some(class_id));
        let via_memo = (skip_index.is_none() || skip_index == Some(0))
            && compact
            && self.counters.mark_loop.ref_disps.load(Ordering::Relaxed)
            && !dbg_zgc_wild()
            && self.claim_compact_via_memo(base, class_id, num_slots, skip_index, scratch, &mut claim);
        if !via_memo {
            self.visit_strong_refs_at(base as *mut u8, skip_index, &mut claim);
        }
        self.visit_extra_object_edges_of(base, class_id, &mut claim);
        None
    }

    /// The wild-pointer gate: is `addr` the base of a live allocation?
    ///
    /// A reference slot is raw bytes that a racing mutator may be mid-store
    /// into, so a child address read out of one can be anything at all. This
    /// is `collect_garbage`'s `registered.contains(&addr)` screen; the engine
    /// counts refusals in `ZMarkStats::off_heap_children`, which is that
    /// loop's `wild_skipped`. Interior pointers are refused too — an object
    /// base is the only thing whose first bytes are a header.
    fn is_in_heap(&self, addr: u64) -> bool {
        addr != 0 && self.registry.contains(addr as usize)
    }

    /// The element count of a reference array (zs s5/mc3, the stealable
    /// chunks; see [`mark::ZMarkContext::ref_array_len`]).
    ///
    /// The contract holds by construction of the two walks above:
    /// `visit_refs_chunked` and `scan_and_claim` share the array arm (cursor =
    /// element index, elements `[cursor, min(cursor + budget, len))`, the pin
    /// / overlay edges only on the call that reaches `len`), and an array is
    /// never a registered `Reference`, so there is no referent hole to split.
    /// Asked once per split array and once per chunk task (one header decode
    /// per `ref_chunk_budget()` elements), never per ordinary object.
    fn ref_array_len(&self, addr: u64) -> Option<usize> {
        if addr == 0 {
            return None;
        }
        let header = self.header_ref(addr as usize as *mut u8);
        (header.kind() == ObjectKind::Array && header.element_type() == ArrayElementType::Reference)
            .then(|| header.array_length() as usize)
    }

    /// [`Self::is_in_heap`] then [`Self::try_mark`], in that order, answered
    /// from ONE virtual call (zr z1/mk).
    ///
    /// The engine asks this of every EDGE. The inherited default makes it two
    /// more trait-object calls per edge (`is_in_heap`, `try_mark`) on top of
    /// the one the edge already cost through `visit_refs_chunked`'s callback;
    /// the serial marker pays neither. Same gate, same claim, same order: the
    /// registry test refuses a wild word before anything dereferences it.
    #[inline(always)]
    fn claim_child(&self, addr: u64) -> mark::ZChildClaim {
        if addr == 0 {
            return mark::ZChildClaim::OffHeap;
        }
        let a = addr as usize;
        if let Some(bits) = &self.mark_bits {
            if self
                .counters
                .mark_loop
                .test_mark_first
                .load(Ordering::Relaxed)
            {
                if let Some((w, mask, already)) = bits.locate_and_test(a) {
                    if already {
                        return mark::ZChildClaim::AlreadyMarked;
                    }
                    let in_registry = match self.registry.bitmap() {
                        Some(reg_bits) if reg_bits.same_grid(bits) => {
                            // SAFETY: `same_grid` guarantees `w < reg_bits.nwords`.
                            (unsafe { reg_bits.contains_located_relaxed(w, mask) }
                                || (reg_bits.has_spill() && self.registry.contains(a)))
                        }
                        _ => self.registry.contains(a),
                    };
                    if !in_registry {
                        return mark::ZChildClaim::OffHeap;
                    }
                    // SAFETY: `(w, mask)` came from `bits.locate_and_test(a)`.
                    return if unsafe { bits.claim_located(w, mask) } {
                        mark::ZChildClaim::Claimed
                    } else {
                        mark::ZChildClaim::AlreadyMarked
                    };
                }
            }
        }
        if !self.registry.contains(a) {
            return mark::ZChildClaim::OffHeap;
        }
        if self.mark_claim(a) {
            mark::ZChildClaim::Claimed
        } else {
            mark::ZChildClaim::AlreadyMarked
        }
    }

    /// Bytes to charge to live-set accounting — [`ZgcRealHeap::alloc_size`],
    /// the same figure the sweep and [`ZgcRealHeap::walk_objects`] use.
    ///
    /// Returning the default `0` would compile and disable liveness
    /// accounting silently, which is how a relocation-set chooser comes to
    /// believe every page is empty.
    ///
    /// zs s1/dg: through [`ZgcRealHeap::survivor_size`], so a live array past
    /// `MAX_PLAUSIBLE_ARRAY_LEN` (up to 2 GB of payload) is charged instead of
    /// counted as 0 bytes -- an under-count that hid exactly the heap pressure
    /// the occupancy triggers exist to see.
    fn object_size(&self, addr: u64) -> usize {
        if addr == 0 {
            return 0;
        }
        // A header this collector cannot size contributes no accounted bytes.
        // `alloc_size`'s `None` is a corrupt/unresolvable header (see its doc),
        // and charging a 1 TiB sentinel to the live set would make every
        // occupancy figure derived from it meaningless.
        self.survivor_size(addr as usize, self.header_ref(addr as usize as *mut u8))
            .unwrap_or(0)
    }

    /// The arena's base address, so a colored word's 42-bit **offset** can be
    /// turned back into the address `try_mark` and `is_in_heap` expect.
    ///
    /// This defaults to `None` in the trait, and the default's own doc explains
    /// why: `ZgcRealHeap`'s slots hold raw pointers and are never coloured, so
    /// `None` was "the *honest* answer for it today". That stopped being true
    /// when this heap grew a `ZBarrierContext` (Phase 4), because the barrier
    /// speaks offsets and `mark_live` must convert — with `None` it would have
    /// returned early on every call, i.e. a barrier wired to a marker that
    /// silently discards everything it is handed.
    ///
    /// The promise the trait extracts for `Some(base)` is that `base + offset`
    /// is an address this context's own `try_mark`/`is_in_heap` accept, and it
    /// holds here by construction: both go through `self.registry`, which is
    /// keyed by the same arena addresses `base_ptr` is the start of.
    fn heap_base(&self) -> Option<u64> {
        Some(self.arena.lock().base_ptr() as u64)
    }

    // `on_worker_start` / `on_worker_end` are left at their defaults on
    // purpose. They exist for per-thread context state (a TLAB-like scratch
    // buffer, a JFR thread binding); this heap has none — allocation is
    // serialized through one `Mutex<Arena>` and the marker allocates nothing
    // — so an override would be an empty function that later reads as a hook
    // somebody forgot to fill in.
}

// ---------------------------------------------------------------------------
// The page-based relocator's seam, implemented over the real heap
// ---------------------------------------------------------------------------

impl ZgcRealHeap {
    /// Hoist the per-cycle pin set the page-based relocator consults.
    ///
    /// **Call this once, before the first
    /// [`relocate::ZRelocateContext::is_pinned`], and pair it with
    /// [`Self::end_page_evac_cycle`].** The union of both pin sources is taken
    /// here because `gc_quiescence::pinned_jit_roots_snapshot()` returns a
    /// freshly built `Vec<usize>`: asking it per object is `O(objects x pins)`
    /// plus an allocation each, inside a stop-the-world pause. See
    /// [`ZgcCounters::page_evac_pins`].
    ///
    /// The mutators must already be stopped. A pin registered after this
    /// snapshot would not be seen, and the whole point of a pin is that it is
    /// honoured — which is why this is a safepoint operation and not a lazy
    /// cache.
    pub fn begin_page_evac_cycle(&self) {
        let mut pins = self.counters.page_evac_pins.lock();
        pins.clear();
        pins.extend(self.critical_pin_addrs());
        // The CONSERVATIVE JIT-frame set. Over-approximate by construction
        // (`is_heap_addr` is a range check, so an interior pointer or a plain
        // `long` can present as a root), which is exactly why the objects it
        // names must not MOVE: the collector cannot rewrite the slot the
        // address came from, because it may not be a slot.
        pins.extend(cratonvm_gc_core::gc_quiescence::pinned_jit_roots_snapshot_honouring_movable(
            zgc_movable_pins_honoured(),
        ));
        // ...and the OBJECT each of them lies in (zr z1/rl). `is_pinned` is an
        // exact-address test and `drain_page` asks it of object BASES, so a
        // conservative root that is an interior pointer pinned nothing on this
        // arm. The slide does not have the gap only because it pins whole
        // pages. Resolved after the raw extend so the raw words stay too.
        let raw: Vec<usize> = pins.iter().copied().collect();
        for p in raw {
            if let Some(obj) = self.resolve_interior_for_pin(p) {
                pins.insert(obj.as_ptr() as usize);
            }
        }
        self.counters.page_evac_moves.lock().clear();
        // The bump-only to-space `alloc_in` serves from (lane Q's arena half).
        // Refused only when a window is already open, which `open_to_space`
        // reports itself; `alloc_in` then still answers from that window.
        let _ = self.arena.lock().open_to_space();
    }

    /// Close a page-evacuation cycle, returning every `from -> to` pair the
    /// relocator published.
    ///
    /// **The caller must fold these into the cycle's `PointerMap`.** See
    /// [`ZgcCounters::page_evac_moves`] for what an empty map claims and why
    /// that claim is a use-after-free when objects moved.
    pub fn end_page_evac_cycle(&self) -> Vec<(usize, usize)> {
        self.counters.page_evac_pins.lock().clear();
        let _ = self.arena.lock().close_to_space();
        std::mem::take(&mut *self.counters.page_evac_moves.lock())
    }

    /// Objects the page-based relocator has moved in the open cycle.
    /// Diagnostics and tests.
    pub fn page_evac_moved_count(&self) -> usize {
        self.counters.page_evac_moves.lock().len()
    }
}

