#[allow(unused_imports)]
use super::*;

impl GenerationalHeap {
    /// Store a primitive field whose canonical receiver and compact slot were
    /// validated by VmHeap's complete batch plan, with no intervening safepoint.
    ///
    /// # Safety
    /// off/storage must describe this receiver's in-bounds primitive slot.
    #[inline]
    pub unsafe fn set_resolved_compact_primitive_field(
        &self,
        obj: ObjectRef,
        index: usize,
        off: usize,
        storage: cratonvm_types::FieldStorageKind,
        value: Value,
    ) {
        debug_assert!(!storage.is_reference());
        let header = self.get_header(obj);
        // Keep the existing field/object watches and primitive atomic store.
        self.set_compact_field_resolved(obj, header, index, off, storage, value);
    }

    /// Bind this heap to its VM's compact-layout domain.
    pub fn set_layout_domain(&self, domain: u32) {
        self.layout_domain
            .store(domain, std::sync::atomic::Ordering::Release);
    }

    /// This heap's compact-layout domain.
    pub fn layout_domain(&self) -> u32 {
        self.layout_domain
            .load(std::sync::atomic::Ordering::Acquire)
    }

    /// `(name, declared field count)` of `class_id` in THIS heap's VM, for the
    /// field-access diagnostics: [`cratonvm_gc_core::gc::resolve_class_info_in`] with the
    /// heap's layout domain, which is its VM's owner token. The domain-less
    /// resolver answers from whichever VM has the id first, which with two VMs
    /// may name another VM's class (interpreter round i1 wave 23, lane L5).
    pub fn diag_class_info(&self, class_id: u32) -> Option<(String, usize)> {
        cratonvm_gc_core::gc::resolve_class_info_in(self.layout_domain(), class_id)
    }

    /// Create a new generational heap with default sizes.
    pub fn new() -> Self {
        Self::with_sizes(DEFAULT_YOUNG_SEMI_SIZE, DEFAULT_OLD_GEN_SIZE)
    }

    /// Create a generational heap with custom young semi-space and old gen sizes.
    ///
    /// Takes absolute arena sizes, not a heap budget to divide, so it imposes
    /// no whole-heap ceiling (`max_heap_bytes = usize::MAX`) and keeps the
    /// historical `young_semi * MAX_HEAP_EXPANSION_FACTOR` growth ceiling.
    /// The production path is [`with_capacity`], which does budget itself.
    pub fn with_sizes(young_semi_size: usize, old_gen_size: usize) -> Self {
        let max_young = young_semi_size
            .max(1024)
            .saturating_mul(MAX_HEAP_EXPANSION_FACTOR);
        Self::with_sizes_and_max_young(young_semi_size, old_gen_size, max_young, usize::MAX)
    }

    pub fn with_sizes_and_max_young(
        young_semi_size: usize,
        old_gen_size: usize,
        max_young_semi_size: usize,
        max_heap_bytes: usize,
    ) -> Self {
        Self::with_sizes_max_young_and_old_max(
            young_semi_size,
            old_gen_size,
            max_young_semi_size,
            max_heap_bytes,
            0,
        )
    }

    /// [`Self::with_sizes_and_max_young`], with the old generation RESERVED up
    /// to `old_gen_max` bytes (gen r5w3/oldgen7, `CRATONVM_GC_OLD_BORROW_YOUNG`;
    /// see `OldGen::with_growth_headroom`). `old_gen_max <= old_gen_size` (the
    /// `0` every other constructor passes) is exactly the historical heap.
    /// The card table covers the whole reservation, because the barrier
    /// indexes it by address and a grown generation's objects lie above the
    /// construction capacity.
    pub fn with_sizes_max_young_and_old_max(
        young_semi_size: usize,
        old_gen_size: usize,
        max_young_semi_size: usize,
        max_heap_bytes: usize,
        old_gen_max: usize,
    ) -> Self {
        let young_semi_size = young_semi_size.max(1024);
        let old_gen_size = old_gen_size.max(1024);
        // gcg h1/ob: the percent knob, read once for this heap.
        let young_trigger_pct = young_trigger_percent();
        let threshold = young_semi_size * young_trigger_pct / 100;
        let max_young = max_young_semi_size.max(young_semi_size);

        let old_gen = if old_gen_max > old_gen_size {
            OldGen::with_growth_headroom(old_gen_size, old_gen_max)
        } else {
            OldGen::new(old_gen_size)
        };
        // `growth_max()` is the capacity itself unless the headroom was
        // really reserved (a fallback store cannot grow in place).
        // t1 lane D: the young semispaces are built before the card table, so
        // a whole-heap table (`CRATONVM_GC_WHOLE_HEAP_CARDS`) can cover them.
        // t1 wave 8 lane CARDTBL (`gen_whole_heap_young_headroom_enabled`):
        // reserve up to the young maximum, so young growth stays in the hull.
        let young_headroom = max_young > young_semi_size && gen_whole_heap_young_headroom_enabled();
        let young_arena = || {
            let mut a = young_headroom
                .then(|| Arena::with_headroom(young_semi_size, max_young))
                .flatten()
                .unwrap_or_else(|| Arena::new(young_semi_size));
            a.arm_object_starts();
            a.set_uncommit_policy(0, true, GEN_YOUNG_UNCOMMIT_DELAY);
            a
        };
        let young_from_arena = young_arena();
        let young_to_arena = young_arena();
        let card_table = whole_heap_card_table(&old_gen, [&young_from_arena, &young_to_arena])
            .unwrap_or_else(|| {
                CardTable::new(old_gen.base_ptr() as usize, old_gen.growth_max())
            });

        // Cache the host NUMA shape at construction. The slow-path
        // allocator consults this through `numa_slow_path_hint`. On the
        // ~all-current-targets single-node case the hint is just 0 and
        // we behave exactly as before (no per-node arena yet — see TODO
        // on `numa_node_hint`).
        // gce e2/o: `CRATONVM_GEN_PRISTINE_CHUNKS` is removed (subsumed by
        // zero-once); the young arenas keep the arena's default (no pristine
        // skip), which is what the switch's default was.
        let topology = cratonvm_gc_core::numa::global_topology();
        let numa_num_nodes = topology.num_nodes.max(1);
        let numa_node_hint = if numa_num_nodes == 1 {
            0
        } else {
            // Multi-node: prefer the node of the constructing thread so
            // long-lived heap metadata lands near whoever booted the VM.
            // Falls back to 0 if the platform's current-thread probe is
            // unavailable, which keeps single-arena behavior stable.
            let n = cratonvm_gc_core::numa::NumaTopology::current_thread_node();
            if n < numa_num_nodes {
                n
            } else {
                0
            }
        };

        let heap = Self {
            layout_domain: std::sync::atomic::AtomicU32::new(cratonvm_types::FIRST_LAYOUT_DOMAIN),
            // Armed here rather than in `Arena::new`: this collector's
            // `is_object_address` is the only one that consults the bitmap, and
            // charging every other `Arena` in the process for it (ZGC's whole
            // heap among them) buys nothing. See `Arena::arm_object_starts`.
            young_from: Mutex::new(young_from_arena),
            young_from_pub_used: AtomicUsize::new(0),
            young_from_pub_free: AtomicUsize::new(0),
            young_from_pub_capacity: AtomicUsize::new(young_semi_size),
            young_to: Mutex::new(young_to_arena),
            old_gen: Mutex::new(old_gen),
            quarantine: Mutex::new(VecDeque::new()),
            evac_pool: std::sync::OnceLock::new(),
            region_bounds: [
                (AtomicUsize::new(0), AtomicUsize::new(0)),
                (AtomicUsize::new(0), AtomicUsize::new(0)),
                (AtomicUsize::new(0), AtomicUsize::new(0)),
            ],
            commit_bits: CommitScreenTable::new(), // gen r5w1/young5
            object_starts: [AtomicUsize::new(0), AtomicUsize::new(0)],
            object_starts_hold: Mutex::new([None, None]),
            object_starts_retired: Mutex::new([None, None]), // gcf f4/y
            lazy_dirty_mirrors: [AtomicUsize::new(0), AtomicUsize::new(0)], // r23 w4 R-L4z
            lazy_dirty_mirrors_hold: Mutex::new([None, None]),
            lazy_dirty_mirrors_retired: Mutex::new([None, None]),
            card_table,
            jit_ref_store_gates: Box::new(JitRefStoreGates::unpublished()), // gen r4w4/cards4
            next_hash_code: AtomicI32::new(1),
            young_gc_threshold: AtomicUsize::new(threshold),
            satb_queue: None,
            conc_cycle_state: None,
            shrink_cycle_clock: [AtomicU64::new(0), AtomicU64::new(0)],
            resurrection_closure: Mutex::new(Vec::new()),
            parked_old_only_roots: Mutex::new(Vec::new()), // gci i13/vg
            max_young_semi_size: max_young,
            initial_young_semi_size: young_semi_size,
            max_heap_bytes,
            serial_pool_max_total: 0,
            serial_tenured_pool_max: 0,
            initial_old_gen_capacity: old_gen_size,
            young_ceded_cap: AtomicUsize::new(0), // gcj j3/og3
            numa_node_hint,
            numa_num_nodes,
            stats: HeapStats::default(),
            force_promote_all: std::sync::atomic::AtomicBool::new(false),
            tenuring: tenuring::TenuringState::new(), // gen r4w6/young6
            prev_survivor_count: std::sync::atomic::AtomicUsize::new(0),
            young_spill_pressure: std::sync::atomic::AtomicBool::new(false),
            young_trigger_seen_gc_count: std::sync::atomic::AtomicU64::new(0),
            young_trigger_floor: std::sync::atomic::AtomicUsize::new(0),
            young_cycle_skipped: std::sync::atomic::AtomicBool::new(false),
            last_cycle_drain_blocked: std::sync::atomic::AtomicBool::new(false), // gcd d9/c
            jit_tlab_skip_regions: Mutex::new(Vec::new()),
            _bounds_registration: RelocatableHeapRegistration::new(),
            evacuated_wipe: Mutex::new(None),
            young_trigger_feedback: Mutex::new(TriggerFeedback::for_new_heap()), // gci i2/yg
            wipe_in_flight: std::sync::atomic::AtomicBool::new(false),
            gc_log_enabled: std::sync::atomic::AtomicBool::new(false),
            pause_totals: [AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0)], // gen r4/plumbing
            promote_pressure_census: [const { AtomicU64::new(0) }; 4], // gen r4w2/youngpolicy
            young_stress_epochs: [AtomicU64::new(u64::MAX), AtomicU64::new(u64::MAX)], // gen r4w2/youngmark
            young_stress_floor: std::sync::atomic::AtomicUsize::new(0), // gen r4w2/youngmark
            young_stress_net_carved_at_epoch: AtomicU64::new(0),        // gen r4w6/tlab6
            tlab_tail_sink: gc_flags().gen_tlab_tail_sink,              // gen r4w6/tlab6
            young_xms_floor: AtomicUsize::new(0),                       // gen r5w5/sizer9
            pinned_copy_defrag: true,       // gcf f2/y
            young_defrag_pending: std::sync::atomic::AtomicBool::new(false), // gcf f2/y
            pinned_copy_defrag_census: [const { AtomicU64::new(0) }; 5], // gcf f2/y, gch h2/yg
            pinned_copy_takes_promotion_risk: gen_pinned_copy_takes_promotion_risk_enabled(), // gcj j1/yg
            pinned_copy_survivor_reserve: true, // gcj j1/yg
            term4_offer_census: [const { AtomicU64::new(0) }; T4O_LEN], // gcj j1/yg
            young_known_zero_carry: gen_young_known_zero_carry_enabled(), // gcj j2/yg2
            tlab_tail_dirty_declines: true, // t1 w8 YGZERO
            sweep_lazy_zero: gen_sweep_lazy_zero_enabled(), // r23 w6
            min_tlab_floor: cratonvm_gc_core::tlab::min_tlab_size(), // gcj j3/yg3
            young_premium_census: [const { AtomicU64::new(0) }; YP_LEN], // gcf f3/q
            young_wedged_trigger: true,   // gcd d9/b
            young_serial_eden: 0, // gcf f7/c; armed by `arm_serial_eden_trigger`
            young_serial_survivor: 0, // gcj j1/yg; armed with it
            pinned_survivor_estimate: AtomicUsize::new(0), // gcj j1/yg
            lazy_starts_backoff: LazyStartsBackoff::default(), // gkr
            young_geometry: None, // gcj yg3-residuals R3; set by `with_serial_young_geometry`
            young_geometry_carry: AtomicUsize::new(0),
            young_geometry_seen_gc: AtomicU64::new(u64::MAX),
            serial_survivor_ratio: tenuring::SURVIVOR_RATIO as usize, // gci i45/sc
            young_serial_eden_now: AtomicUsize::new(0), // gci i22/ed; armed by `follow_initial_heap_eden`
            serial_eden_follow: Mutex::new(SerialEdenFollow::default()), // gci i22/ed
            young_age_clock_eden: 0, // gci i1/yg; armed by `arm_serial_eden_trigger`
            age_clock: Mutex::new(AgeClock::default()), // gci i1/yg
            young_age_clock_old_direct: false, // gci i3/yg; armed with the clock
            age_clock_old_direct_pending: AtomicUsize::new(0), // gci i3/yg
            young_cycle_forced: std::sync::atomic::AtomicBool::new(false), // gci i3/yg
            young_pause_goal_ms: young_pause_goal_ms_from_env(), // gci i4/yg
            young_exhausted_refill_collects: gen_young_exhausted_refill_collects_enabled(), // gci i4/yg
            tlab_refill_smallest_whole: gen_tlab_refill_smallest_whole_enabled(), // gci i22/dr
            young_refill_epoch: AtomicU64::new(u64::MAX), // gci i4/yg
            young_refill_carved_at_epoch: AtomicU64::new(0), // gci i4/yg
            young_exhausted_fired_epoch: AtomicU64::new(u64::MAX), // gci i4/yg
            young_exhausted_census: [const { AtomicU64::new(0) }; 3], // gci i4/yg
            young_age_eden: 0, // gc wave 9 / gcC; armed by `arm_serial_eden_trigger`
            young_age_epoch: [const { AtomicUsize::new(0) }; 4], // gc wave 9 / gcC
            young_age_this_cycle: std::sync::atomic::AtomicBool::new(true), // gc wave 9 / gcC
            young_trigger_pct, // gcg h1/ob
            young_wedged_census: [const { AtomicU64::new(0) }; 3], // gcd d9/b
            oome_ladder_census: [const { AtomicU64::new(0) }; oome_ladder::LEN], // gcd d9/b
            old_pause_totals: [AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0)], // gen r4w2/obs
            escalated_young_totals: [AtomicU64::new(0), AtomicU64::new(0)], // gcg g6/yg
            escalation_mark: Mutex::new(EscalationState::default()),        // gcg g6/yg, h1/ob
            last_collection_usage: [
                AtomicUsize::new(0),
                AtomicUsize::new(0),
                AtomicUsize::new(0),
            ], // gen r4w2/obs
            last_collection_committed: [
                AtomicUsize::new(0),
                AtomicUsize::new(0),
                AtomicUsize::new(0),
            ], // gci i14/mp
            serial_pool_snapshot: Mutex::new(None),
            serial_pool_snapshot_fallbacks: AtomicU64::new(0),
            serial_collection_used: [const { AtomicUsize::new(0) }; 3],
            non_moving_latch: [AtomicU64::new(u64::MAX), AtomicU64::new(0)], // gen r4w3/young2
            gc_notifications: cratonvm_gc_core::gc_metrics::GcNotificationQueue::new(), // gen r4w3/obs2
            forward_refusals: Arc::new(ForwardRefusalLedger::default()),    // gcd d3/m
            pinned_takeover_census: [const { AtomicU64::new(0) }; 4],       // gcd d3/m
            pinned_unproven_census: [const { AtomicU64::new(0) }; 4],       // gcj j1/yg
            pinned_takeover_declines: [const { AtomicU64::new(0) }; takeover_decline::LEN], // gcd d5/s
            conservative_divert_census: [const { AtomicU64::new(0) };
                CONSERVATIVE_DIVERT_CENSUS_LEN], // gen r4w4/young4
            pinned_young_census: [const { AtomicU64::new(0) }; PINNED_YOUNG_CENSUS_LEN], // gen r4w5/pinned5
            pinned_exact_census: [const { AtomicU64::new(0) }; 10],                      // gcg g1/y
            young_in_place_vacated: Mutex::new(Vec::new()), // gen r5w5/pin9
            card_scan_scratch: Mutex::new((Vec::new(), Vec::new())), // gcj j1yg-pause
            evac_shard_scratch: Mutex::new(Vec::new()), // gcj pcp
            card_scan_parts: Mutex::new(Vec::new()), // gcj pcp
            sorted_map_buffers: std::sync::Arc::new(
                cratonvm_types::pointer_map::SortedBufferPool::new(SORTED_MAP_BUFFER_CAP),
            ), // gcj pcp
            minor_promised: std::sync::atomic::AtomicBool::new(false), // gcj j1yg-pause r3
            minor_promise_deferrals: AtomicU64::new(0), // gcj j1yg-pause r3
            minor_promise_just_deferred: std::sync::atomic::AtomicBool::new(false), // gcj j1yg-pause r3
            unrecorded_root_census: [const { AtomicU64::new(0) }; 9], // gcd d2/h, gcf f4/y, gch h1/yp, gch h2/yg, gci i10/gr
            last_unrecorded_root: AtomicUsize::new(0),                // gcd d2/h
            blocked_peer_interior_census: [const { AtomicU64::new(0) }; BPI_LEN], // gch h2/yg
            pinned_ledger_incomplete_raw_jni: AtomicU64::new(0),      // gch h2/yg
            true_root_watched_left_out: Mutex::new(Vec::new()),       // gch h4/wr
            true_root_watched_left_out_len: AtomicUsize::new(0),      // gch h4/wr
            true_root_watched_left_out_total: AtomicU64::new(0),      // gch h4/wr
            ihash_witness: gci_i11jd_ihash_witness_enabled(),         // gci i12/sx
            external_gpu: cratonvm_gc_core::gpu_critical::ExternalGpuPressureTracker::with_heap_capacity(
                young_semi_size
                    .saturating_mul(2)
                    .saturating_add(old_gen_size),
            ),
        };
        // Publish the initial region bounds so the lock-free
        // `is_object_address` containment check is correct from the first
        // allocation (before any GC has run to refresh them).
        heap.refresh_region_bounds();
        // Let compiled reference stores gate their barriers inline.
        //
        // Until 2026-09-02 only ZGC published a plan, so under
        // `-XX:+UseGenerationalGC` every compiled reference store paid a full
        // `jit_putfield_object` call — measured as `gated=0 declined=2` against
        // ZGC's `gated=2 declined=0` on the same workload and the same sites.
        // The blocker was the gate SHAPE, not the collector: see
        // `JitRefStoreGates::post_skip_mask`.
        //
        // `GC_FLAG_OLD_GEN` is exactly this collector's post-barrier question —
        // `write_barrier` cards a store only when the receiver lies in the
        // old-generation arena, and that arena's objects are the ones carrying
        // the flag. A receiver without it is young, and a young receiver needs
        // no card.
        //
        // gen r4w4/cards4 (2026-09-24): published into THIS heap's own block,
        // not the process-global `JIT_REF_STORE_GATES` a ZGC heap in the same
        // process rewrites. `build_helpers_for` reads it back through
        // `Self::jit_ref_store_plan` for the VM that owns this heap only.
        if jit_ref_store_gates_enabled() {
            // The PRE gate compiled code reads is the process-wide marker
            // disjunction; make it current, as the global publish did.
            refresh_jit_ref_store_pre_active();
            heap.jit_ref_store_gates.publish_masked(GC_FLAG_OLD_GEN);
            // "There may be old objects" is left permanently armed: the mask
            // above already skips the young receivers, which is the case that
            // matters, and an accurate `post_active` would need a hook on every
            // path that creates an old object for a win it has already taken.
            // Nothing else writes this heap's block, so it stays armed.
        }
        heap
    }

    /// Start zeroing `spans` (absolute `(addr, len)` pairs inside the inactive
    /// young semi-space) on a helper thread. See [`Self::evacuated_wipe`].
    ///
    /// Sound because the spans are owned by nobody until
    /// [`Self::join_evacuated_wipe`] returns: the arena they lie in is the
    /// inactive semi-space, which no mutator allocates into and which the next
    /// collection joins this thread before touching; `is_object_address`
    /// declines it while [`Self::wipe_in_flight`] is set; and `Drop` joins
    /// before the backing is freed. If the OS cannot spawn a thread the wipe
    /// runs here, inside the pause, exactly as `Arena::reset` did.
    pub fn spawn_evacuated_wipe(&self, spans: Vec<(usize, usize)>) {
        if spans.is_empty() {
            return;
        }
        // Never two in flight: the previous one was joined at the top of
        // this cycle, but a caller that reaches here twice must not race.
        self.join_evacuated_wipe();
        self.wipe_in_flight.store(true, Ordering::Release);
        let job = spans.clone();
        // Latched (`cratonvm_types::latched_thread`): the heap's `Drop`
        // joins it and can run in a thread-local destructor, where an OS join
        // deadlocks on Windows.
        let spawned = cratonvm_types::latched_thread::spawn_latched(
            std::thread::Builder::new().name("cratonvm-gc-young-wipe".into()),
            move || {
                for (addr, len) in job {
                    // SAFETY: the span is committed memory of the inactive
                    // young semi-space, reserved for this thread until the
                    // heap joins it (see the method doc).
                    unsafe { std::ptr::write_bytes(addr as *mut u8, 0, len) };
                }
            },
        );
        match spawned {
            Ok(handle) => *self.evacuated_wipe.lock() = Some(handle),
            Err(_) => {
                for (addr, len) in spans {
                    // SAFETY: as above, on the calling thread instead.
                    unsafe { std::ptr::write_bytes(addr as *mut u8, 0, len) };
                }
                self.wipe_in_flight.store(false, Ordering::Release);
            }
        }
    }

    /// Hand the EVACUATED young semi-space back to the OS.
    ///
    /// `CRATONVM_GEN_UNCOMMIT`, default ON; `=0` is the kill switch and is the
    /// first thing to set if a compiled frame ever faults on a young address.
    /// Returns the bytes released.
    ///
    /// # Why this collector had nothing like it
    ///
    /// It was the only backend that never gave memory back. ZGC decommits its
    /// arena's un-bumped middle and free lists on every collection; G1 shrinks
    /// its committed prefix on request (`CRATONVM_G1_UNCOMMIT`); `gen_heap.rs`
    /// contained no `decommit` call at all, so a run that peaked and then idled
    /// held its peak for the life of the process.
    ///
    /// # Why the inactive semi-space is the right target, and the old gen is not
    ///
    /// After the flip, `young_to` is the space the collection evacuated FROM.
    /// Nothing live is in it — that is what the flip means — and the very next
    /// thing this cycle does to it today is zero it
    /// ([`Self::spawn_evacuated_wipe`]). Decommitting is the same statement
    /// made to the OS instead of to the bytes: the pages go back, and the next
    /// allocation into them arrives through [`cratonvm_gc_core::arena::Arena::alloc`] →
    /// `hand_out`, which re-commits the granule it hands out.
    ///
    /// The OLD generation is not a candidate and cannot be made one here: it is
    /// a `Vec<u8>`, committed in full at construction, with no reservation to
    /// shrink. Giving it lazy commit is a change to its allocator, not a call
    /// site.
    ///
    /// # What it costs
    ///
    /// (gen r4/alloc, 2026-09-23: this heading said "and why it is off by
    /// default", and the paragraph at its end argued for OFF — while the first
    /// line of this doc says "default ON" and `types/src/flags.rs` parses
    /// `gen_uncommit` with `on_unless_zero`. The code is ON; the argument
    /// below is kept as the reason the kill switch exists, not as a statement
    /// about the default.)
    ///
    /// A decommitted granule FAULTS on touch. It does not read back as zero —
    /// `Arena::decommit_free_blocks` says so, and records that believing
    /// otherwise is how two ZGC slides came to write into released granules.
    /// This collector publishes its young arenas' FULL reserved range into
    /// `JIT_REGION_BOUNDS` (inline reference stores) and `JIT_READ_BOUNDS`
    /// (inline `getfield`), so a compiled access through a STALE reference into
    /// the evacuated semi-space would fault rather than read a stale value.
    ///
    /// That window is not created by this method — the young arenas already
    /// commit lazily in 2 MiB granules while the published bound covers the
    /// whole reservation — but it is WIDENED by it, from "granules never yet
    /// allocated into" to "granules that held objects one collection ago". The
    /// interpreter's own probe is safe either way: `is_object_address` screens
    /// every candidate through `region_range_committed` before it dereferences
    /// anything, and declines an address in a released granule.
    ///
    /// That trade is a measurement and not a judgement, which is why the
    /// switch exists: `CRATONVM_GEN_UNCOMMIT=0` makes it in one binary.
    /// (`g1_uncommit`, which this used to cite as having "the same default",
    /// is the opposite: OFF unless set.)
    ///
    /// # The `-Xms` floor (gen r5w5/sizer9)
    ///
    /// HotSpot never shrinks the heap below its initial size, and the old
    /// generation here honours that (`OldGen::commit_floor`). The young half
    /// did not: after the first moving collection this released the whole
    /// evacuated semi-space, `-Xms` prefix included, so `Runtime.totalMemory()`
    /// (`usable_committed_parts`: the larger semi-space's commit plus the old
    /// generation's) fell below `-Xms` and stayed there
    /// (`docs/feature-designs/generative-gc-architecture.md`).
    /// Now the evacuated arena is left with at least the `young_xms_floor`
    /// field's bytes committed: it is the from-space again after the next flip, and while
    /// it is the to-space it is the larger of the two commits the young part
    /// reports. The floor is 0 without `-Xms`, which leaves this function
    /// byte for byte what it was.
    ///
    /// gcd d1/d: the give-back is floor-aware (`Arena::decommit_unused_above`),
    /// so the prefix is no longer released and committed again every pause
    /// (sizer9 had to, `arena.rs` was not its file); the commit below now only
    /// fills in a prefix granule that was never committed. The return value is
    /// what was released — all of it at or above the floor, so none of it is
    /// committed again — and the caller clips the wipe's spans
    /// (`retain_committed_spans`) whenever it is non-zero.
    pub fn uncommit_evacuated_young(&self, evacuated: &mut Arena) -> usize {
        self.uncommit_evacuated_young_keeping(evacuated, 0)
    }

    /// [`Self::uncommit_evacuated_young`] that also keeps the first `keep`
    /// bytes of the evacuated semi-space committed (on top of the `-Xms`
    /// floor, capped at its capacity).
    ///
    /// gce e1/y (item 2 of `gengc-r4w4-young4-parallel-evacuator-scaling-limits-CLOSED-20261012.md`):
    /// the evacuated semi-space is the NEXT cycle's to-space, and the copy
    /// bumps it from offset 0. Released whole, every survivor the next copy
    /// writes first faults a granule back in INSIDE that pause. The moving
    /// arm passes the bytes it just copied into young (plus a quarter) under
    /// the opt-in `CRATONVM_GEN_UNCOMMIT_KEEP_SURVIVORS`, so the next copy
    /// finds that prefix committed; its stale bytes go to the off-pause wipe
    /// with the rest (`retain_committed_spans`), exactly as the `-Xms`
    /// floor's do. `keep = 0` is the function above, byte for byte.
    pub fn uncommit_evacuated_young_keeping(&self, evacuated: &mut Arena, keep: usize) -> usize {
        if !gc_flags().gen_uncommit {
            return 0;
        }
        let floor = self
            .young_xms_floor
            .load(Ordering::Relaxed)
            .max(keep.min(evacuated.capacity()));
        // BOTH halves, for the reason ZGC's site gives: the middle is the space
        // no allocation ever reached, the free lists are the space that WAS
        // allocated and has since been freed. After an evacuating flip the
        // second is most of it.
        //
        // gcd d1/d: neither below the `-Xms` floor. Without `-Xms` the floor
        // is 0 and this is `decommit_unbumped_middle() + decommit_free_blocks()`
        // exactly; with it, the floor's granules are no longer released here
        // only to be committed again by the arm below (which stays, for a
        // prefix some other path left uncommitted).
        let released = evacuated.decommit_unused_above(floor);
        if floor != 0 {
            // A refusal commits nothing and is not an error: the arena commits
            // on demand, as it did before the floor existed. Already-committed
            // granules cost a bitmap test each, no syscall
            // (`HeapStore::commit_range`).
            let _ = evacuated.commit_initial_prefix(floor);
        }
        // gcd d1/d: the GROSS release, which is now also the net one — every
        // released granule lies at or above the floor, and the commit above
        // touches only `[0, floor)`. sizer9 returned `before - after` after
        // releasing the prefix too and committing it again; that difference
        // could be 0 while granules ABOVE the floor had been released (when
        // the re-commit filled in as many never-committed prefix granules),
        // and the caller then skipped `retain_committed_spans` and handed the
        // off-pause wipe spans over released granules, which fault on write.
        if released != 0 {
            YOUNG_BYTES_UNCOMMITTED.fetch_add(released as u64, Ordering::Relaxed);
        }
        released
    }

    /// Wait for the off-pause wipe, if one is running. Idempotent and cheap
    /// when none is.
    pub fn join_evacuated_wipe(&self) {
        let handle = self.evacuated_wipe.lock().take();
        if let Some(handle) = handle {
            let _ = handle.join();
        }
        self.wipe_in_flight.store(false, Ordering::Release);
    }

    /// Republish the lock-free [`region_bounds`] cache from the live arenas.
    ///
    /// Locks each region briefly to read its current `[base, base+capacity)`
    /// and stores it with `Release` ordering. Called at construction and at the
    /// start/end of every GC cycle — the only points where a young arena's
    /// backing can move (swap/grow) or the heap is first sized. Cheap (3 short
    /// lock/read/store), and never on the hot mutator path.
    pub fn refresh_region_bounds(&self) {
        let yf = self.lock_young_from();
        let yt = self.young_to.lock();
        let og = self.old_gen.lock();
        self.store_region_bounds_locked(&yf, &yt, &og);
    }

    /// Store the three regions' `[base, end)` into [`region_bounds`] from
    /// already-held guards (used inside GC, where the arenas are locked and
    /// re-locking would deadlock). Mirror of [`refresh_region_bounds`].
    pub fn store_region_bounds_locked(&self, yf: &Arena, yt: &Arena, og: &OldGen) {
        let pairs = [
            (yf.base_ptr() as usize, yf.capacity()),
            (yt.base_ptr() as usize, yt.capacity()),
            (og.base_ptr() as usize, og.capacity()),
        ];
        for (i, (slot, (base, cap))) in self.region_bounds.iter().zip(pairs).enumerate() {
            // Compressed oops fix a base/shift at VM init from these very
            // regions. A region that later moves outside that window (young-gen
            // growth reallocates the arena) makes every reference into it
            // unencodable — fail loudly here rather than corrupt the heap.
            cratonvm_gc_core::compressed_oops::assert_region_encodable(base, base.wrapping_add(cap));
            slot.0.store(base, Ordering::Release);
            slot.1.store(base.wrapping_add(cap), Ordering::Release);
            // Mirror into the process-global table the JIT's guarded inline
            // getfield bakes as an absolute address (see JIT_REGION_BOUNDS).
            JIT_REGION_BOUNDS.words[i * 2].store(base, Ordering::Release);
            JIT_REGION_BOUNDS.words[i * 2 + 1].store(base.wrapping_add(cap), Ordering::Release);
            // ...and the read-side sibling. Same values on this backend; the
            // two tables only diverge on G1 (read-only publish) and ZGC
            // (neither). See `JIT_READ_BOUNDS`.
            publish_jit_read_bounds(i, base, base.wrapping_add(cap));
            // ...and the capability-free "where is the heap" table. Same three
            // spans, but a DIFFERENT question -- see `heap_geometry`. This one
            // every backend fills, which is what lets a consumer such as
            // `compressed_oops::enable_for_live_heap` ask it without having to
            // read a table whose emptiness under another collector is
            // load-bearing.
            cratonvm_gc_core::heap_geometry::publish_heap_span(i, base, base.wrapping_add(cap));
        }
        // Republish the commit bitmaps beside the bounds. All three regions
        // need a screen whenever the reserve/commit backing store is in use —
        // gen r4w2/oldgen2 (2026-09-23): the old gen (slot 2) too, now that
        // `OldGen` sits on `HeapStore` instead of a wholly committed `Vec<u8>`.
        // Its map is the live `Arc` the store updates at every commit, so a
        // granule committed by a mutator-time old-gen allocation between two
        // publishes is visible to the screen at once.
        publish_young_commit_bits([
            (
                pairs[0].0,
                pairs[0].0.wrapping_add(pairs[0].1),
                yf.commit_bits(),
            ),
            (
                pairs[1].0,
                pairs[1].0.wrapping_add(pairs[1].1),
                yt.commit_bits(),
            ),
        ]);
        self.store_commit_bits_locked(
            pairs,
            [yf.commit_bits(), yt.commit_bits(), og.commit_bits()],
        );
        // ...and the exact object-start bitmaps, on the same schedule and for
        // the same reason: the arenas may have been swapped or re-backed, so a
        // pointer published before this call can describe the wrong arena.
        self.store_object_starts_locked(yf.object_starts(), yt.object_starts());
        // r23 w4 review (R-L4z item 2): and the lazy-dirty mirrors, same slots.
        self.store_lazy_dirty_mirrors_locked(yf.lazy_dirty_mirror(), yt.lazy_dirty_mirror());
    }

    /// r23 w4 review (R-L4z item 2): publish the young arenas' lazy-dirty
    /// mirrors into [`Self::lazy_dirty_mirrors`], with
    /// [`Self::store_object_starts_locked`]'s retire-then-drop ordering.
    pub fn store_lazy_dirty_mirrors_locked(
        &self,
        yf: Option<Arc<cratonvm_gc_core::heap_bitmap::HeapBitmap>>,
        yt: Option<Arc<cratonvm_gc_core::heap_bitmap::HeapBitmap>>,
    ) {
        let mut hold = self.lazy_dirty_mirrors_hold.lock();
        let mut retired = self.lazy_dirty_mirrors_retired.lock();
        for (i, bits) in [yf, yt].into_iter().enumerate() {
            let p = bits.as_ref().map_or(0, |b| Arc::as_ptr(b) as usize);
            self.lazy_dirty_mirrors[i].store(p, Ordering::Release);
            retired[i] = std::mem::replace(&mut hold[i], bits);
        }
    }

    /// JIT round 23 wave 6, lane L4: publish young FROM-space's lazy-dirty
    /// mirror (slot 0) alone, for a caller that holds only the from-space lock
    /// (the non-moving sweep). Slot 1 is left as published. Same
    /// retire-then-drop ordering as [`Self::store_lazy_dirty_mirrors_locked`].
    pub fn store_lazy_dirty_mirror_from_slot(
        &self,
        yf: Option<Arc<cratonvm_gc_core::heap_bitmap::HeapBitmap>>,
    ) {
        let mut hold = self.lazy_dirty_mirrors_hold.lock();
        let mut retired = self.lazy_dirty_mirrors_retired.lock();
        let p = yf.as_ref().map_or(0, |b| Arc::as_ptr(b) as usize);
        self.lazy_dirty_mirrors[0].store(p, Ordering::Release);
        retired[0] = std::mem::replace(&mut hold[0], yf);
    }

    /// r23 w4 review (R-L4z item 2): is `addr` inside a lazy-dirty span of
    /// young slot `slot` (0 = from, 1 = to), i.e. certainly not an object?
    /// `false` when no mirror is published. Lock-free.
    #[inline]
    pub fn young_lazy_dirty_at(&self, slot: usize, addr: usize) -> bool {
        let Some(cell) = self.lazy_dirty_mirrors.get(slot) else {
            return false;
        };
        let ptr = cell.load(Ordering::Acquire) as *const cratonvm_gc_core::heap_bitmap::HeapBitmap;
        if ptr.is_null() {
            return false;
        }
        // SAFETY: published by `store_lazy_dirty_mirrors_locked` from an `Arc`
        // that `lazy_dirty_mirrors_hold` owns while current and
        // `lazy_dirty_mirrors_retired` keeps for one more publish (the
        // `arena_object_start` argument). `contains` takes `&self`; a mirror
        // whose arena was re-backed was cleared before it was dropped, so a
        // stale pointer only ever answers `false` (accept).
        unsafe { (*ptr).contains(addr) }
    }

    /// Publish the two young arenas' object-start bitmaps into the lock-free
    /// [`Self::object_starts`] mirror. Mirror of
    /// [`Self::store_commit_bits_locked`], with the same retire-then-drop
    /// ordering.
    pub fn store_object_starts_locked(
        &self,
        yf: Option<Arc<cratonvm_gc_core::heap_bitmap::HeapBitmap>>,
        yt: Option<Arc<cratonvm_gc_core::heap_bitmap::HeapBitmap>>,
    ) {
        let mut hold = self.object_starts_hold.lock();
        // gcf f4/y (`gengc-r5w1-young5` P3): retire-then-drop in ONE call
        // only stops a reader from LOADING the old pointer after this store; a
        // lock-free reader (`arena_object_start`) that loaded it just before
        // the store still dereferences it after `hold[i] = bits` dropped the
        // last `Arc` -- a use-after-free window. The generation this call
        // replaces now moves to `object_starts_retired` and is dropped at the
        // NEXT publish, a whole collection later, by which time no reader
        // that loaded it before this publish can still be inside its
        // `contains` probe.
        let mut retired = self.object_starts_retired.lock();
        for (i, bits) in [yf, yt].into_iter().enumerate() {
            match &bits {
                Some(b) => {
                    self.object_starts[i].store(Arc::as_ptr(b) as usize, Ordering::Release);
                }
                None => {
                    // Retire the pointer first, so no NEW reader is handed
                    // the generation being replaced.
                    self.object_starts[i].store(0, Ordering::Release);
                }
            }
            // Dropping the assignment's previous value frees the generation
            // retired by the previous publish, never the one just replaced.
            retired[i] = std::mem::replace(&mut hold[i], bits);
        }
    }

    /// Publish the three regions' commit bitmaps, with the `[base, base +
    /// capacity)` spans they describe, into the per-heap [`Self::commit_bits`]
    /// screen as ONE snapshot (gen r5w1/young5; see that field).
    ///
    /// `pairs` are the `(base, capacity)` pairs `store_region_bounds_locked`
    /// just published into `region_bounds`, in the same slot order, so the
    /// snapshot names exactly the arenas the bounds name.
    pub fn store_commit_bits_locked(
        &self,
        pairs: [(usize, usize); 3],
        bits: [Option<Arc<[AtomicU64]>>; 3],
    ) {
        let [yf, yt, og] = bits;
        let screen = |(base, cap): (usize, usize), bits: Option<Arc<[AtomicU64]>>| {
            Some(CommitScreen {
                base,
                end: base.wrapping_add(cap),
                bits,
            })
        };
        self.commit_bits.publish([
            screen(pairs[0], yf),
            screen(pairs[1], yt),
            screen(pairs[2], og),
        ]);
    }

    /// Is `[addr, addr + len)` backed by committed pages, for an `addr` the
    /// caller found inside one of [`Self::region_bounds`]' regions?
    ///
    /// `true` when that region publishes no bitmap — a wholly-committed store
    /// has every byte of its capacity mapped, so there is nothing to screen —
    /// and before the first publish. `false` when `addr` is in no arena of the
    /// current snapshot: bounds and snapshot then came from different
    /// publishes, which only a reader overlapping a pause can see, and "do not
    /// read" is the safe answer.
    ///
    /// This is the guard that makes [`Self::is_object_address`]'s header
    /// dereference safe. See [`Self::commit_bits`] for why the region-bounds
    /// check alone is not.
    #[inline]
    pub fn region_range_committed(&self, addr: usize, len: usize) -> bool {
        self.commit_bits.range_committed_or(addr, len, false)
    }

    /// Return the primary NUMA node hint recorded at construction.
    ///
    /// Public observer so tests and profiling code can confirm the heap
    /// picked up the expected node. The field itself remains private to
    /// keep room for the multi-arena refactor.
    pub fn numa_node_hint(&self) -> usize {
        self.numa_node_hint
    }

    /// Number of NUMA nodes the heap was constructed for (>=1).
    pub fn numa_num_nodes(&self) -> usize {
        self.numa_num_nodes
    }

    /// Compute the node the *calling* thread would prefer for a young-gen
    /// slow-path allocation, falling back to the heap's primary hint.
    ///
    /// This is the seam where the multi-arena refactor will plug in: it
    /// returns the index that `try_alloc_young_initialized`/`refill_tlab`
    /// would use to pick a per-node arena. Today there is only one arena pair, so the
    /// return value is consumed only by the tracing stub below — but the
    /// query path is exercised on every slow-path allocation, which means
    /// the platform probe and topology cache are validated in production
    /// long before we flip the multi-arena switch.
    #[inline]
    pub fn numa_slow_path_hint(&self) -> usize {
        if self.numa_num_nodes <= 1 {
            return self.numa_node_hint;
        }
        let n = cratonvm_gc_core::numa::NumaTopology::current_thread_node();
        if n < self.numa_num_nodes {
            n
        } else {
            self.numa_node_hint
        }
    }

    /// The persistent parallel-evacuation worker pool, created on first use.
    ///
    /// See the `evac_pool` field for why this is lazy. `workers` is the total
    /// the policy asked for, so the pool holds one fewer — the driver is a
    /// worker too.
    /// g1gen/c6: this heap's own evacuation-pool census for the
    /// `[GC] evac_pool:` shutdown line; `None` while no collection has built
    /// the pool.
    pub fn evac_pool_census(&self) -> Option<cratonvm_gc_core::evac_pool::EvacPoolCensus> {
        self.evac_pool.get().map(cratonvm_gc_core::evac_pool::EvacPool::census)
    }

    pub fn evac_pool(&self, workers: usize) -> &cratonvm_gc_core::evac_pool::EvacPool {
        self.evac_pool.get_or_init(|| {
            // `gen-evac-N`, not `g1-evac-N` (gen r4w2/youngpolicy): a stuck
            // young pause's native stacks should name this collector.
            cratonvm_gc_core::evac_pool::EvacPool::with_name(workers.saturating_sub(1), "gen-evac")
        })
    }

    /// Return a handle to the GC statistics counters.  Counters are
    /// updated during GC cycles and allocation fast-paths.
    pub fn stats(&self) -> &HeapStats {
        &self.stats
    }

    /// Create a generational heap with a total capacity split proportionally.
    ///
    /// HotSpot-equivalent sizing: young gen takes ~50% of the total heap
    /// (the from+to semi-space pair together), split across two
    /// semi-spaces. So each semi-space is ~25% of -Xmx. HotSpot's default
    /// is closer to NewRatio=2 (young = 1/3 of heap) but our copying
    /// collector trades old-gen room for young-gen room more aggressively
    /// because (a) the non-moving sweep that runs while JIT frames are
    /// active cannot promote, and so depends on the young semi being big
    /// enough to hold the entire transient working set; and (b) Cheney
    /// copy cost scales with *survivors*, not capacity, so a larger
    /// young semi is essentially free unless the working set actually
    /// grows to fill it.
    ///
    /// For the common ranges:
    ///   - 64 KiB heap (test):   young_semi = 16 KiB
    ///   - 256 MiB heap (def.):  young_semi = 64 MiB
    ///   - 1 GiB heap:           young_semi = 256 MiB
    ///   - 4 GiB heap (-Xmx4g):  young_semi = 1 GiB
    ///   - 16 GiB heap:          young_semi = 4 GiB
    ///
    /// (gen r4w2/alloc2, 2026-09-23: this paragraph said the young semi "can
    /// still *grow* up to `young_semi * MAX_HEAP_EXPANSION_FACTOR`". Not under
    /// this constructor: the growth ceiling passed below is the initial semi
    /// itself, because the `-Xmx` budget leaves nothing for growth to take —
    /// see the comment above `max_young_semi`. Only [`Self::with_sizes`], which
    /// has no budget, keeps the historical 4x ceiling.)
    ///
    /// A smaller initial semi-space (tried: capped at 512 MiB) was measured
    /// to be a NET REGRESSION for large `-Xmx` workloads on this collector:
    /// every young GC that fires under this workload already falls back to
    /// the non-moving sweep (a separate, pre-existing defect —
    /// `gc_quiescence`'s `missing-exact-rbp` fallback), so shrinking the
    /// semispace just multiplies how often that expensive fallback fires,
    /// which costs more than the page-fault savings recoup. See
    /// `binarytrees-bt18-half-gap-20260730.md`.
    pub fn with_capacity(total_bytes: usize) -> Self {
        let total = total_bytes.max(4096);
        // Young takes 1/2 of total, split across from+to semi-spaces (so
        // each semi gets 1/4 of total). The other half goes to old gen.
        let young_semi_raw = total / 4;
        // Clamp: floor at 512 bytes (the smallest test heap) so very
        // tiny test heaps still produce a non-trivial arena. No ceiling —
        // the user passed -Xmx N to get N bytes of heap, not "N capped
        // at some arbitrary internal constant".
        const YOUNG_SEMI_MIN: usize = 512;
        let young_semi = young_semi_raw.max(YOUNG_SEMI_MIN);
        // Old gen gets whatever's left after the young (from+to). Use
        // `saturating_sub` so a tiny `total` (`max(4096)`) doesn't
        // underflow when the clamped young is larger than half of it.
        let young_pair = young_semi.saturating_mul(2);
        let old_size = total.saturating_sub(young_pair).max(512);
        // `-Xmx N` means "this process may commit N bytes of Java heap", not
        // "start at N and grow from there". The young growth ceiling is
        // therefore whatever is left of the budget after the (fixed,
        // non-expandable) old generation, split across the two semi-spaces:
        // both semis must fit, because a copying collection needs a to-space
        // as large as the from-space it is evacuating, and the pair swaps
        // roles every cycle.
        //
        // Under this constructor's own 50/50 split that ceiling equals the
        // initial semi, i.e. expansion has nothing left to give — which is
        // the correct outcome, not a loss: the young semi already *starts* at
        // the largest size the budget permits, so a heap that would previously
        // have grown into it is strictly better off starting there. What the
        // clamp removes is only the ability to exceed `-Xmx`. A workload that
        // genuinely needs more room now surfaces a catchable
        // `OutOfMemoryError` (the honest answer, and what HotSpot does) rather
        // than silently committing 2.5x its stated maximum and risking a
        // kernel OOM-kill, which is unrecoverable.
        //
        // `max(young_semi)` keeps the ceiling coherent for the tiny-heap
        // corner where `YOUNG_SEMI_MIN` clamping pushed the pair past `total`.
        let max_young_semi = total.saturating_sub(old_size) / 2;
        let max_young_semi = max_young_semi.max(young_semi);
        // gen r5w3/oldgen7 (`CRATONVM_GC_OLD_BORROW_YOUNG`, opt-in): reserve
        // the old generation up to HotSpot Serial's `MaxHeapSize -
        // MaxNewSize` — 2/3 of `-Xmx` at its default `NewRatio = 2` — so a
        // refused allocation can grow it in place (`OldGen::grow_after_refusal`)
        // instead of ending in a false `OutOfMemoryError`. Its CAPACITY is
        // still `old_size`; only the default split gets headroom (an explicit
        // young size is a user's choice of split, honoured exactly).
        // JIT r20 wave 18, lane gcI: reserved to `old_borrow_hard_max`, with
        // the free-ratio slack still capped at `old_borrow_max` (below).
        let old_max = if gc_flags().old_borrow_young {
            old_borrow_hard_max(total)
        } else {
            0
        };
        let heap = Self::with_sizes_max_young_and_old_max(
            young_semi,
            old_size,
            max_young_semi,
            total,
            old_max,
        );
        if old_max > 0 {
            heap.old_gen
                .lock()
                .set_growth_soft_max(old_borrow_max(total));
        }
        heap.arm_serial_eden_trigger(total, &cratonvm_gc_core::gen_young_sizing::YoungGenSizing::default())
    }

    /// gcf f7/c: under `CRATONVM_GEN_YOUNG_TRIGGER_SERIAL_EDEN`, record
    /// HotSpot Serial's eden for a `total_bytes` (`-Xmx`) heap with the
    /// operator's `young` flags ([`serial_young_gen_bytes`],
    /// [`serial_eden_bytes`]) and start the young trigger at
    /// [`serial_eden_young_trigger`] instead of the percent knob. Off, the
    /// heap is returned untouched. Only the `-Xmx` constructors call this;
    /// [`Self::with_sizes`] has no `-Xmx` to derive an eden from.
    ///
    /// The pause-goal loop still runs (`CRATONVM_GC_YOUNG_PAUSE_MS`); this
    /// value is its start and its ceiling ([`Self::configured_young_trigger`]).
    /// HotSpot Serial has no pause goal: `CRATONVM_GC_YOUNG_PAUSE_MS=0` with
    /// this switch is the pure Serial shape.
    pub fn arm_serial_eden_trigger(
        mut self,
        total_bytes: usize,
        young: &cratonvm_gc_core::gen_young_sizing::YoungGenSizing,
    ) -> Self {
        if total_bytes > 0 && total_bytes != usize::MAX {
            (self.serial_pool_max_total, self.serial_tenured_pool_max) =
                serial_pool_maxima(total_bytes, young, self.serial_survivor_ratio);
        }
        // gci i1/yg: the HotSpot age clock (`CRATONVM_GEN_SERIAL_EDEN_AGE_CLOCK`)
        // counts survivor ages in the same eden; it arms independently of the
        // trigger switch and leaves the trigger alone.
        if gen_serial_eden_age_clock_enabled() {
            self.young_age_clock_eden =
                serial_eden_bytes(serial_young_gen_bytes(total_bytes, young)).max(1);
            // gci i3/yg: and, opt-in, charges old-direct allocations too.
            self.young_age_clock_old_direct = gen_serial_eden_age_clock_old_direct_enabled();
        }
        // gc wave 9 (lane gcC): `CRATONVM_GEN_AGE_BY_SERIAL_EDEN` arms the
        // aging epoch from the same eden, independently of the trigger.
        // merge-dev-20261015: the epoch and the gci age clock are two fixes of
        // the same f5c divergence; on one heap they would each need an eden per
        // age. The clock (steps above 1, the old-direct charge) wins when both
        // are on, so the epoch arms only on a heap the clock left unarmed
        // (`CRATONVM_GEN_SERIAL_EDEN_AGE_CLOCK=0`).
        let serial_trigger = gen_young_trigger_serial_eden_enabled();
        let age_epoch = gen_age_by_serial_eden_enabled() && self.young_age_clock_eden == 0;
        if !serial_trigger && !age_epoch {
            return self;
        }
        let eden = serial_eden_bytes(serial_young_gen_bytes(total_bytes, young)).max(1);
        if age_epoch {
            self.young_age_eden = eden;
        }
        if !serial_trigger {
            return self;
        }
        self.young_serial_eden = eden;
        // gcj j1/yg: HotSpot's survivor for the same young generation
        // (`Y = eden + 2 x survivor`).
        self.young_serial_survivor =
            serial_young_gen_bytes(total_bytes, young).saturating_sub(eden) / 2;
        let semi = *self.young_from_pub_capacity.get_mut();
        let trigger = serial_eden_young_trigger(semi, eden);
        *self.young_gc_threshold.get_mut() = trigger;
        // gcj j2/yg2 (`CRATONVM_GEN_SERIAL_EDEN_NO_PAUSE_GOAL`, default ON
        // since gcj j1/yg): HotSpot Serial's shape has no pause goal; an explicit
        // `CRATONVM_GC_YOUNG_PAUSE_MS` still wins.
        if None::<std::ffi::OsString>.is_none()
        {
            self.young_pause_goal_ms = 0;
        }
        tracing::debug!(
            target: "cratonvm::gc",
            serial_eden = eden,
            semi,
            trigger,
            "generational: Serial-shaped young trigger",
        );
        self
    }

    /// gci i22/ed (`CRATONVM_GEN_SERIAL_EDEN_INITIAL`,
    /// [`gen_serial_eden_initial_enabled`]): start the Serial-shaped trigger at
    /// HotSpot Serial's eden for an `initial_bytes` (`-Xms`, or the launcher's
    /// ergonomic initial heap) start instead of its maximum, and let
    /// [`Self::serial_eden_follow_after_cycle`] resize it after full
    /// collections. Called by the VM's Generational constructor
    /// (`VmHeap::new_generational`) with the initial heap it commits; `young`
    /// is the same sizing the heap was built with.
    ///
    /// The heap is returned untouched when the switch is off, when the
    /// Serial-shaped trigger did not arm (`young_serial_eden == 0`), when the
    /// heap has no `-Xmx` budget, and when the initial young generation is
    /// already the maximum (`-Xms == -Xmx`, or an initial heap at least
    /// `-Xmx`): HotSpot's shape and today's agree there, so nothing is
    /// followed and the heap pays nothing.
    pub fn follow_initial_heap_eden(
        mut self,
        initial_bytes: usize,
        young: &cratonvm_gc_core::gen_young_sizing::YoungGenSizing,
    ) -> Self {
        if self.young_serial_eden == 0 || initial_bytes == 0 || !gen_serial_eden_initial_enabled()
        {
            return self;
        }
        let total = self.max_heap_bytes;
        if total == usize::MAX || total == 0 {
            return self;
        }
        let follow = SerialEdenFollow::arm(initial_bytes, total, young);
        if follow.young_now >= follow.young_max {
            return self;
        }
        // Never above the maximum eden the trigger switch armed.
        let eden = serial_eden_bytes(follow.young_now)
            .min(self.young_serial_eden)
            .max(1);
        *self.young_serial_eden_now.get_mut() = eden;
        *self.serial_eden_follow.get_mut() = follow;
        let semi = *self.young_from_pub_capacity.get_mut();
        let trigger = serial_eden_young_trigger(semi, eden);
        *self.young_gc_threshold.get_mut() = trigger;
        tracing::debug!(
            target: "cratonvm::gc",
            initial_bytes,
            serial_eden_initial = eden,
            serial_eden_max = self.young_serial_eden,
            trigger,
            "generational: Serial-shaped young trigger follows the initial heap",
        );
        self
    }

    /// gcj j3/yg3 (`CRATONVM_GEN_TYPED_MIN_TLAB_FLOOR`, default ON, `=0`
    /// kills): install the VM's `TlabSizing::min_size()` (`-XX:MinTLABSize`,
    /// 8 KiB when untyped) as this heap's smallest useful young TLAB, the floor
    /// of the smallest-whole refill and the tail sink's `min_free`. Call it once,
    /// after construction and before the heap is shared (`vm_init.rs`, next to
    /// [`Self::set_survivor_ratio`]). With the switch off the floor stays the
    /// 8 KiB constant ([`min_tlab_floor_for`]).
    pub fn set_min_tlab_floor(&mut self, typed_min_tlab_size: usize) {
        self.min_tlab_floor =
            min_tlab_floor_for(typed_min_tlab_size, true);
    }

    /// gcj j3/yg3: the floor [`Self::set_min_tlab_floor`] installed.
    pub fn min_tlab_floor(&self) -> usize {
        self.min_tlab_floor
    }

    /// gci i45/sc (`gci-i42ta-launcher-drops-survivor-ratio-and-tlab-flags-RETIRED-UNFIXED-20261018`,
    /// the open consumer half): apply the launcher's `-XX:SurvivorRatio`
    /// (`VmConfig::survivor_ratio`, validated `>= 1` and against HotSpot's
    /// `AfterMemoryInit` maximum) to this heap, as HotSpot Serial sizes eden
    /// and its survivors from it (`DefNewGeneration::compute_survivor_size`):
    ///
    /// * the Serial eden every arm derived at construction
    ///   ([`Self::arm_serial_eden_trigger`]: the age clock's, the aging
    ///   epoch's, the Serial-shaped young trigger's maximum, and the trigger
    ///   itself), and the followed current eden of
    ///   [`Self::follow_initial_heap_eden`], re-derived with
    ///   [`serial_eden_bytes_at`] from the same young sizes;
    /// * the survivor reserve the pools report
    ///   (`CRATONVM_GEN_SURVIVOR_POOL_RESERVE`, [`serial_survivor_reserve_at`])
    ///   and the full-collection resize
    ///   ([`Self::serial_eden_follow_after_cycle`]), which read
    ///   [`Self::serial_survivor_ratio`];
    /// * the tenuring target's survivor capacity (`eden / SurvivorRatio`,
    ///   `TenuringState::set_survivor_ratio`).
    ///
    /// Call it once, after construction and before the heap is shared (the
    /// VM's Generational construction, `vm_init.rs`), with the same `young`
    /// sizing the heap was built with. `None` (nothing typed), HotSpot's
    /// default 8, and `CRATONVM_GEN_SURVIVOR_RATIO_FLAG=0` (the kill switch,
    /// read here once per heap) leave the heap exactly as built. A heap with no
    /// `-Xmx` budget ([`Self::with_sizes`]) derives no Serial eden: only the
    /// reserve and the tenuring target take the ratio there.
    ///
    /// The pause-goal loop (`CRATONVM_GC_YOUNG_PAUSE_MS`) has not moved the
    /// trigger yet at this point, so the trigger is re-armed outright.
    pub fn set_survivor_ratio(
        &mut self,
        survivor_ratio: Option<u64>,
        young: &cratonvm_gc_core::gen_young_sizing::YoungGenSizing,
    ) {
        let Some(ratio) = survivor_ratio else {
            return;
        };
        if ratio == tenuring::SURVIVOR_RATIO || !gen_survivor_ratio_flag_enabled() {
            return;
        }
        // The launcher's range is `1 ... max_uintx - 2` and its constraint
        // `MaxHeapSize / SpaceAlignment`; clamp only so the `+ 2` of the
        // eden arithmetic can never wrap.
        let ratio = ratio.clamp(1, u64::from(u32::MAX));
        let r = ratio as usize;
        self.serial_survivor_ratio = r;
        self.tenuring.set_survivor_ratio(ratio);
        let total = self.max_heap_bytes;
        if total == usize::MAX || total == 0 {
            return;
        }
        (self.serial_pool_max_total, self.serial_tenured_pool_max) =
            serial_pool_maxima(total, young, r);
        let eden = serial_eden_bytes_at(serial_young_gen_bytes(total, young), r).max(1);
        if self.young_age_clock_eden > 0 {
            self.young_age_clock_eden = eden;
        }
        if self.young_age_eden > 0 {
            self.young_age_eden = eden;
        }
        if self.young_serial_eden == 0 {
            return;
        }
        self.young_serial_eden = eden;
        let current = if *self.young_serial_eden_now.get_mut() > 0 {
            let young_now = self.serial_eden_follow.get_mut().young_now;
            let now = serial_eden_bytes_at(young_now, r).min(eden).max(1);
            *self.young_serial_eden_now.get_mut() = now;
            now
        } else {
            eden
        };
        let semi = *self.young_from_pub_capacity.get_mut();
        *self.young_gc_threshold.get_mut() = serial_eden_young_trigger(semi, current);
        tracing::debug!(
            target: "cratonvm::gc",
            survivor_ratio = r,
            serial_eden = eden,
            serial_eden_now = current,
            "generational: -XX:SurvivorRatio sizes the Serial eden",
        );
    }

    /// gci i45/sc: this heap's `-XX:SurvivorRatio` (8 unless
    /// [`Self::set_survivor_ratio`] applied a typed one).
    pub fn serial_survivor_ratio(&self) -> usize {
        self.serial_survivor_ratio
    }

    /// gci i22/ed: the Serial eden the trigger follows now: HotSpot's current
    /// eden under `CRATONVM_GEN_SERIAL_EDEN_INITIAL`, else the maximum
    /// ([`Self::young_serial_eden`]; `0` when the Serial-shaped trigger is
    /// off). One relaxed load.
    pub fn serial_eden_current(&self) -> usize {
        match self.young_serial_eden_now.load(Ordering::Relaxed) {
            0 => self.young_serial_eden,
            now => now,
        }
    }

    /// gci i22/ed: the eden the HotSpot age clock counts in: the followed
    /// eden while `CRATONVM_GEN_SERIAL_EDEN_INITIAL` follows one (HotSpot's
    /// collections happen per CURRENT eden), else
    /// [`Self::young_age_clock_eden`]. `0` = the clock is off.
    pub fn age_clock_eden_now(&self) -> usize {
        if self.young_age_clock_eden == 0 {
            return 0;
        }
        match self.young_serial_eden_now.load(Ordering::Relaxed) {
            0 => self.young_age_clock_eden,
            now => now,
        }
    }

    /// gci i22/ed: after a collection that was not refused, feed the
    /// old-generation occupancy to [`SerialEdenFollow`] and, when the cycle
    /// reclaimed the old generation (HotSpot's full collection), resize as
    /// HotSpot Serial does and move the young trigger with the eden. A single
    /// relaxed load on a heap that does not follow.
    ///
    /// Runs on the collecting thread straight after `collect_garbage_inner`
    /// (from [`Self::feed_pause_goal_loop`]), with no arena lock held; trigger
    /// writes there are stop-the-world, as in
    /// [`Self::adapt_young_trigger_to_pause`]. Takes the old-generation lock,
    /// then the follow mutex, one at a time.
    ///
    /// The trigger moves by the eden's change: a growth adds the difference
    /// (so a pause-goal halving keeps its offset, and with the loop off the
    /// trigger is exactly the new eden), a shrink caps the trigger at the new
    /// eden; the old generation's borrowed-budget cap still applies.
    pub fn serial_eden_follow_after_cycle(&self) {
        let eden_before = self.young_serial_eden_now.load(Ordering::Relaxed);
        if eden_before == 0 {
            return;
        }
        let major = cratonvm_gc_core::gc_quiescence::old_gen_reclaimed_last_cycle();
        let old_used = self.old_gen.lock().used();
        let young_now = {
            let mut follow = self.serial_eden_follow.lock();
            if major {
                follow.after_full(old_used);
            } else {
                follow.note_old_used(old_used);
            }
            follow.young_now
        };
        if !major {
            return;
        }
        // gci i45/sc: at the heap's `-XX:SurvivorRatio`.
        let eden = serial_eden_bytes_at(young_now, self.serial_survivor_ratio)
            .min(self.young_serial_eden)
            .max(1);
        if eden == eden_before {
            return;
        }
        self.young_serial_eden_now.store(eden, Ordering::Relaxed);
        let capacity = match self.young_from_pub_capacity.load(Ordering::Relaxed) {
            0 => self.lock_young_from().capacity(),
            published => published,
        };
        let before = serial_eden_young_trigger(capacity, eden_before);
        let after = serial_eden_young_trigger(capacity, eden);
        let current = self.young_gc_threshold.load(Ordering::Relaxed);
        let mut next = if after >= before {
            current.saturating_add(after - before).min(after)
        } else {
            current.min(after)
        };
        if let Some(cap) = self.borrowed_young_trigger_cap(self.old_gen_capacity()) {
            next = next.min(cap);
        }
        self.young_gc_threshold.store(next.max(1), Ordering::Relaxed);
        if gc_flags().dbg_gcpause {
            eprintln!(
                "[gcpause] serial eden {}KB -> {}KB, young trigger {}KB -> {}KB",
                eden_before / 1024,
                eden / 1024,
                current / 1024,
                next / 1024
            );
        }
    }

    /// The young trigger as CONFIGURED for a from-space of `semi_capacity`
    /// bytes, before the pause-goal loop moves it: the Serial-shaped trigger
    /// when [`Self::young_serial_eden`] is armed (gcf f7/c), else
    /// `CRATONVM_GC_YOUNG_TRIGGER_PERCENT` of the semi-space. The loop's
    /// ceiling and the expansion path's reset read this.
    ///
    /// gci i22/ed: the Serial eden is the CURRENT one
    /// ([`Self::serial_eden_current`]) under `CRATONVM_GEN_SERIAL_EDEN_INITIAL`.
    pub fn configured_young_trigger(&self, semi_capacity: usize) -> usize {
        if self.young_serial_eden > 0 {
            serial_eden_young_trigger(semi_capacity, self.serial_eden_current())
        } else {
            semi_capacity * self.young_trigger_pct / 100
        }
    }

    /// The eden adaptive tenuring sizes HotSpot's survivor target from
    /// (`tenuring::desired_survivor_bytes`): HotSpot Serial's eden itself when
    /// the Serial-shaped trigger is armed (gcf f7/c -- Serial's survivor
    /// space, and so its target, is `eden / SurvivorRatio` whatever this
    /// collector's semi-space is), else [`tenuring_eden_bytes`].
    ///
    /// gci i1/yg: the same eden under the HotSpot age clock alone, whose
    /// ages count HotSpot's collections, so its survivor target is HotSpot's.
    ///
    /// gci i26/ed2: the MAXIMUM eden even while
    /// `CRATONVM_GEN_SERIAL_EDEN_INITIAL` follows a smaller one. i22/ed sized
    /// the target from the followed (initial) eden, which is HotSpot's own
    /// survivor space -- but not HotSpot's survivor POPULATION: this
    /// collector's first young cycles also carry the JDK boot set HotSpot
    /// keeps in its CDS archive. With a large initial eden the first young
    /// cycle comes after `main` has started, so the boot set and the
    /// program's first objects share one age cohort; a target halved to
    /// `initial eden / 16` (17 MiB at `-Xmx2g`, RAM/64 = 1 GiB) is then
    /// overrun by that cohort, the adaptive threshold collapses to its age,
    /// and the program's objects tenure with it (vi22-ed `ed22_f_ogfin_erg`:
    /// `finalized=57/64`, HotSpot 64/64; `gci-i26ed2-...` page). The
    /// trigger and the age clock still follow the current eden; only the
    /// survivor target keeps the eden the default geometry sizes it from.
    pub fn tenuring_eden_for(&self, semi_capacity: usize) -> usize {
        if self.young_serial_eden > 0 {
            self.young_serial_eden
        } else if self.young_age_clock_eden > 0 {
            self.young_age_clock_eden
        } else if self.young_age_eden > 0 {
            // gc wave 9 (lane gcC): aging by Serial's eden ages survivors at
            // Serial's rate, so their target is Serial's survivor space too.
            self.young_age_eden
        } else {
            tenuring_eden_bytes(semi_capacity, self.young_trigger_pct)
        }
    }

    /// gci i1/yg: the age step of the collection in progress
    /// ([`AgeClock::step`]): 1 when the HotSpot age clock is off, which is
    /// every heap unless `CRATONVM_GEN_SERIAL_EDEN_AGE_CLOCK` was on as it was
    /// built.
    pub fn age_clock_step(&self) -> u8 {
        if self.young_age_clock_eden == 0 {
            1
        } else {
            self.age_clock.lock().step
        }
    }

    /// gci i1/yg: before a collection, charge the young allocation since the
    /// last one (young live bytes now, less what the last collection left) to
    /// the age clock and fix this collection's age step. Takes and drops the
    /// young lock, so it runs before the collection takes it. A no-op on an
    /// unarmed heap.
    pub fn age_clock_begin(&self) {
        if self.young_age_clock_eden == 0 {
            return;
        }
        let (used, free, _, _) = self.young_from_occupancy();
        let live = used.saturating_sub(free);
        // gci i3/yg: old-direct allocations since the last collection
        // (`CRATONVM_GEN_SERIAL_EDEN_AGE_CLOCK_OLD_DIRECT`); 0 when off.
        let old_direct = if self.young_age_clock_old_direct {
            self.age_clock_old_direct_pending.swap(0, Ordering::Relaxed)
        } else {
            0
        };
        let mut clock = self.age_clock.lock();
        let alloc = live.saturating_sub(clock.exit_live).saturating_add(old_direct);
        // gci i22/ed: the current eden under `CRATONVM_GEN_SERIAL_EDEN_INITIAL`.
        let (credit, step) = age_clock_advance(clock.credit, alloc, self.age_clock_eden_now());
        clock.entry_old_direct = old_direct;
        clock.old_direct_total = clock.old_direct_total.saturating_add(old_direct as u64);
        clock.entry_live = live;
        clock.credit_before = clock.credit;
        clock.credit = credit;
        clock.step = step;
    }

    /// gci i1/yg: after a collection, record what it left in young as the
    /// next charge's base. A REFUSED young cycle (`young_cycle_skipped`) aged
    /// nobody, so it gives the tick it took back: the credit becomes the
    /// pre-cycle credit plus the allocation, and the base is the entry figure
    /// (young did not change). A no-op on an unarmed heap.
    pub fn age_clock_end(&self) {
        if self.young_age_clock_eden == 0 {
            return;
        }
        let refused = self.young_cycle_skipped.load(Ordering::Relaxed);
        let live = if refused {
            0
        } else {
            let (used, free, _, _) = self.young_from_occupancy();
            used.saturating_sub(free)
        };
        let mut clock = self.age_clock.lock();
        if refused {
            // gci i3/yg: the old-direct charge is refunded with the rest.
            let alloc = clock
                .entry_live
                .saturating_sub(clock.exit_live)
                .saturating_add(clock.entry_old_direct);
            clock.credit = clock.credit_before.saturating_add(alloc);
            clock.exit_live = clock.entry_live;
            return;
        }
        clock.exit_live = live;
        if clock.step > 0 {
            clock.ticks += 1;
        } else {
            clock.holds += 1;
        }
    }

    /// gci i3/yg: charge `total_size` bytes allocated straight in the old
    /// generation to the age clock's next collection, when
    /// `CRATONVM_GEN_SERIAL_EDEN_AGE_CLOCK_OLD_DIRECT` armed with the clock and
    /// the allocation is smaller than the clock's eden (HotSpot Serial puts
    /// anything below its young capacity in eden; a larger one it allocates
    /// in the old generation as well). One branch on a field when off.
    #[inline]
    pub fn age_clock_charge_old_direct(&self, total_size: usize) {
        // gci i22/ed: HotSpot's eden-capacity cut is its CURRENT eden
        // (`age_clock_eden_now`); the load runs only when the charge is armed.
        if self.young_age_clock_old_direct && total_size < self.age_clock_eden_now() {
            self.age_clock_old_direct_pending
                .fetch_add(total_size, Ordering::Relaxed);
        }
    }

    /// gci i3/yg: the VM's allocation-failure door marks the collection it is
    /// about to request as FORCED (`true`), and clears the mark once its door
    /// returns (`false`), so a mark the door's pause did not consume -- the
    /// door lost the pause race, or its pause ran a path that does not reach
    /// [`Self::feed_pause_goal_loop`] -- cannot leak into a later,
    /// trigger-fired cycle. Read by the pause-goal loop under
    /// `CRATONVM_GC_YOUNG_PAUSE_SKIP_FORCED` only; with the switch off the mark
    /// is cleared and ignored.
    ///
    /// A door that loses the race leaves its mark for the winner's cycle; that
    /// cycle collected for an allocation that failed, so the loop skipping it
    /// is the intended reading, not a misattribution of consequence.
    pub fn mark_next_young_cycle_forced(&self, forced: bool) {
        self.young_cycle_forced.store(forced, Ordering::Relaxed);
    }

    /// [`Self::with_capacity`] honouring an explicit young size: `-Xmn`,
    /// `-XX:NewSize`, `-XX:MaxNewSize`, `-XX:NewRatio` (gen r4w2/alloc2,
    /// 2026-09-23). The resolution rules are
    /// [`cratonvm_gc_core::gen_young_sizing::plan_young_gen`]'s.
    ///
    /// With none of the four given this IS `with_capacity(total_bytes)` —
    /// the same call, not a re-derivation — so the default geometry cannot
    /// drift. With one given, the planned pair is fixed for the life of the
    /// heap exactly as `with_capacity`'s is (growth ceiling = the initial semi):
    /// the old generation gets the rest of the `-Xmx` budget, and the budget
    /// argument on `with_capacity` holds unchanged — the pair plus the old
    /// generation is `-Xmx`.
    ///
    /// Clamps are reported by the plan (`YoungGenPlan::adjustments`), which the
    /// launcher prints once; here they only reach `tracing`, so an embedder
    /// that calls this directly still has a trace of what it got.
    pub fn with_capacity_and_young(
        total_bytes: usize,
        young: &cratonvm_gc_core::gen_young_sizing::YoungGenSizing,
    ) -> Self {
        Self::with_capacity_young_and_survivor_ratio(total_bytes, young, None)
    }

    /// [`Self::with_capacity_and_young`] for a VM that will apply
    /// `survivor_ratio` (`-XX:SurvivorRatio`, [`Self::set_survivor_ratio`]):
    /// gcj yg3-residuals R3 sizes the Serial geometry's semis for it.
    pub fn with_capacity_young_and_survivor_ratio(
        total_bytes: usize,
        young: &cratonvm_gc_core::gen_young_sizing::YoungGenSizing,
        survivor_ratio: Option<u64>,
    ) -> Self {
        if let Some(heap) = Self::with_serial_young_geometry(total_bytes, young, survivor_ratio) {
            return heap;
        }
        let Some(plan) = cratonvm_gc_core::gen_young_sizing::plan_young_gen(total_bytes, young) else {
            return Self::with_capacity(total_bytes);
        };
        for note in &plan.adjustments {
            tracing::debug!(target: "cratonvm::gc", "generational young sizing: {note}");
        }
        let heap = Self::with_sizes_and_max_young(
            plan.young_semi,
            plan.old_size,
            plan.young_semi,
            plan.total_bytes,
        );
        heap.arm_serial_eden_trigger(plan.total_bytes, young)
    }

    /// gcj yg3-residuals R3: the heap under HotSpot Serial's young geometry
    /// ([`gen_serial_young_geometry_enabled`], [`serial_young_geometry_for`]),
    /// or `None` (switch off, or a budget the geometry cannot plan) for the
    /// caller's semi-space pair. Both semis reserve `eden + survivor`; the old
    /// generation starts at `-Xmx - Y`, so the default heap's initial old
    /// generation is HotSpot's two thirds of `-Xmx` (the pair's split gave it
    /// half). The independent default-on `CRATONVM_GC_OLD_BORROW_YOUNG` policy
    /// may still grow old gen into reserved headroom after a refusal; the young
    /// trigger then cedes the borrowed budget as in the semi-space constructor.
    pub fn with_serial_young_geometry(
        total_bytes: usize,
        young: &cratonvm_gc_core::gen_young_sizing::YoungGenSizing,
        survivor_ratio: Option<u64>,
    ) -> Option<Self> {
        if !gen_serial_young_geometry_enabled() {
            return None;
        }
        let total = total_bytes.max(4096);
        // The ratio `set_survivor_ratio` will apply (the same kill switch and
        // clamp), so the semis are sized for the eden it arms.
        let ratio = survivor_ratio
            .filter(|_| gen_survivor_ratio_flag_enabled())
            .map_or(tenuring::SURVIVOR_RATIO, |r| {
                r.clamp(1, u64::from(u32::MAX))
            }) as usize;
        let (geom, old) = serial_young_geometry_for(total, young, ratio)?;
        // Each semi RESERVES `Y` (`eden + 2 S`), one survivor more than the
        // `eden + S` it is used for and reported as (`reported_semi_cap`): a
        // moving cycle keeps at most `S` young, so `eden + S` is all a semi
        // holds between moving cycles, but the pinned in-place copy has no
        // to-space and copies this cycle's survivors into the same from-space
        // that holds eden and the last cycle's carry. The extra `S` is that
        // copy's survivor space. Address space only: the arenas commit lazily.
        // Preserve the default-on old-generation borrowing policy for the
        // default split. An explicit young-size/ratio flag remains the user's
        // chosen split and receives no borrowing headroom. For the default,
        // geometry must not erase the reserved headroom that lets a full old
        // generation grow by ceding young trigger budget after a refused
        // allocation. Without this, it is permanently capped at `Xmx - Y`;
        // the previous semi-space constructor allowed the same default-on
        // policy to satisfy a tight live set (including the 192 MiB javac
        // acceptance heap).
        let old_max = if gc_flags().old_borrow_young && young.is_default() {
            old_borrow_hard_max(total)
        } else {
            0
        };
        let mut heap = Self::with_sizes_max_young_and_old_max(
            geom.young_budget,
            old,
            geom.young_budget,
            total,
            old_max,
        );
        if old_max > 0 {
            heap.old_gen
                .lock()
                .set_growth_soft_max(old_borrow_max(total));
        }
        heap.young_geometry = Some(geom);
        tracing::debug!(
            target: "cratonvm::gc",
            young_budget = geom.young_budget,
            eden = geom.eden,
            survivor = geom.survivor,
            semi_reserve = geom.semi_reserve,
            old,
            "generational: HotSpot Serial young geometry",
        );
        Some(heap.arm_serial_eden_trigger(total, young))
    }

    /// gcj yg3-residuals R3: the geometry this heap was built with, if any.
    pub fn young_geometry(&self) -> Option<cratonvm_gc_core::gen_young_sizing::SerialPairGeometry> {
        self.young_geometry
    }

    /// gcj yg3-residuals R3: a young semi-space capacity `cap` as the
    /// Java-visible readers count it (`Runtime.maxMemory()`, `totalMemory()`,
    /// the memory pools): under the Serial geometry HotSpot's `eden + S`
    /// ([`cratonvm_gc_core::gen_young_sizing::SerialPairGeometry::semi_reserve`]), not the
    /// `Y` each semi reserves for the in-place copy's headroom; `cap` itself
    /// otherwise.
    pub fn reported_semi_cap(&self, cap: usize) -> usize {
        self.young_geometry.map_or(cap, |g| g.semi_reserve.min(cap))
    }

    /// gcj yg3-residuals R3: `[GC] young_geometry:`. `ygeo_armed=0` (and
    /// zeroes) on a heap built as the semi-space pair; `ygeo_carry` is the
    /// last sampled young carry, `ygeo_cap_promotions` the survivors tenured
    /// at the survivor cap over the process.
    pub fn young_geometry_census_line(&self) -> String {
        let g = self
            .young_geometry
            .unwrap_or(cratonvm_gc_core::gen_young_sizing::SerialPairGeometry {
                young_budget: 0,
                eden: 0,
                survivor: 0,
                semi_reserve: 0,
            });
        format!(
            "[GC] young_geometry: ygeo_armed={} ygeo_young_budget={} ygeo_eden={} ygeo_survivor={} ygeo_semi_reserve={} ygeo_carry={} ygeo_cap_promotions={}",
            u8::from(self.young_geometry.is_some()),
            g.young_budget,
            g.eden,
            g.survivor,
            g.semi_reserve,
            self.young_geometry_carry.load(Ordering::Relaxed),
            GEOMETRY_CAP_PROMOTIONS.load(Ordering::Relaxed),
        )
    }

    /// The `-Xmx` budget for the whole committed heap, or `usize::MAX` when
    /// this heap was built from explicit arena sizes (see [`max_heap_bytes`]).
    pub fn heap_budget_bytes(&self) -> usize {
        self.max_heap_bytes
    }

    /// zt t29/g1 (G1 carry page item 10a, `-XX:+AlwaysPreTouch`,
    /// `CRATONVM_GC_ALWAYS_PRE_TOUCH_ALL`): touch the committed bytes of both
    /// young semi-spaces and the old generation now -- the `-Xms` startup
    /// commit ([`Self::commit_initial_heap`]) -- as HotSpot Serial pre-touches
    /// its generations' initial commit. Returns the bytes touched.
    ///
    /// g1gen (the residual t29/g1 left: "a Serial heap's commits after startup
    /// are not pre-touched"): each arena's store keeps the bit, so every later
    /// commit -- a semi-space or old-generation expansion, a recommit after an
    /// uncommit -- is touched as it is made, as HotSpot pre-touches an
    /// expansion ([`cratonvm_gc_core::reservation::HeapStore::set_pre_touch`]).
    /// zki (page `zgc-zt23hunt4-*` item 5(c)): HotSpot 25 Serial under
    /// `-XX:+UseTransparentHugePages` advises its heap `MADV_HUGEPAGE`; so do
    /// this heap's three stores. `true` when the kernel took every advice.
    pub fn advise_transparent_huge_pages(&self) -> bool {
        let young_from = self.young_from.lock().advise_huge_pages();
        let young_to = self.young_to.lock().advise_huge_pages();
        let old = self.old_gen.lock().advise_store_huge_pages();
        young_from && young_to && old
    }

    pub fn pre_touch_committed(&self) -> usize {
        let young_from = self.young_from.lock().set_store_pre_touch(true);
        let young_to = self.young_to.lock().set_store_pre_touch(true);
        let old = self.old_gen.lock().set_store_pre_touch(true);
        tracing::debug!(
            target: "cratonvm::gc",
            young_from,
            young_to,
            old,
            "generational: -XX:+AlwaysPreTouch touched the startup commit",
        );
        young_from + young_to + old
    }

    /// `-Xms`: commit `bytes` across the young pair NOW, and report how many
    /// young bytes were committed.
    ///
    /// # What this can and cannot honour
    ///
    /// gen r4w2/oldgen2 (2026-09-23): all of it. The young pair takes the
    /// request first (up to `-Xmx / 2` between them) and the old generation
    /// commits the remainder as a prefix (`OldGen::commit_initial_prefix`),
    /// so the startup commit covers every `-Xms <= -Xmx`. Before that date the
    /// old generation was a `Vec` allocated whole at construction, which made
    /// the startup commit **at least** `-Xmx / 2` whatever `-Xms` said — the
    /// one arena `-Xms` could not lower
    /// (`docs/feature-designs/generative-gc-architecture.md`).
    /// It now reserves like every other arena in this crate and commits as
    /// allocation reaches it, so an `-Xms` below `-Xmx / 2` really is the
    /// startup commit. The return value still counts the young pair only.
    ///
    /// Split evenly rather than poured into the from-space: the two semis swap
    /// roles every cycle, so a collection that starts with a committed
    /// from-space and a cold to-space takes the commit faults it was asked to
    /// avoid on its first evacuation. Each half is clamped to its semi's
    /// capacity by [`Arena::commit_initial_prefix`].
    ///
    /// # `CRATONVM_GEN_XMS_USABLE_FIRST` (gen r5w3/oldgen7; default ON since gen r5w4/defaults8)
    ///
    /// The even split below is now the `=0` arm: `GenR5W3XmsTotalProbe`
    /// passes (= HotSpot Serial) with the usable-first split and fails with
    /// the even one (`types::flags::alloc_policy_defaults::GEN_XMS_USABLE_FIRST`
    /// carries the evidence). The even split's own reasoning above still holds
    /// for it; the price of the default is the to-space commit faults named at
    /// the end of this section.
    ///
    /// The even split has a Java-visible price since gen r5w2/obs6 moved
    /// `Runtime.totalMemory()` to HotSpot Serial's `capacity()`, which leaves
    /// the copy reserve out: the half of `-Xms` committed into the to-space is
    /// committed but not usable, so a fresh `-Xms64m -Xmx512m` heap reads
    /// `totalMemory() = 32m` where HotSpot Serial reads ~62m
    /// (`gengc-r5w2-obs6-xms-startup-commit-is-half-copy-reserve`). With the
    /// flag the split is [`xms_usable_first_split`]'s — HotSpot's own model of
    /// an initial heap — and `totalMemory()` starts at about `-Xms`. The
    /// startup commit is `-Xms` either way; what moves is which arena holds
    /// it. The price is the latency argument above: the first evacuations
    /// commit the to-space granules the even split would have pre-faulted.
    pub fn commit_initial_heap(&self, bytes: usize) -> usize {
        if bytes == 0 {
            return 0;
        }
        if gc_flags().gen_xms_usable_first {
            return self.commit_initial_heap_usable_first(bytes);
        }
        let per_semi = bytes / 2;
        // `lock_young_from` republishes the from-space's `(used, free,
        // capacity)` triple on drop. Nothing here moves any of the three — a
        // prefix commit touches only the store's granule map — so the
        // republished values equal the ones already there; taking the guard
        // rather than the raw mutex is what keeps that true if the arena ever
        // grows a side effect here.
        let a = self.lock_young_from().commit_initial_prefix(per_semi);
        let b = self.young_to.lock().commit_initial_prefix(per_semi);
        self.note_young_xms_floor(a, b);
        // gen r4w2/oldgen2 (2026-09-23): the old generation reserves now, so
        // it no longer covers the rest of `-Xms` by having committed its whole
        // capacity at construction. Commit whatever the young pair could not
        // absorb as an old-gen prefix, so the startup commit is still at least
        // `-Xms` for every `-Xms <= -Xmx` (young takes up to half; the old
        // generation holds the other half). A refusal commits less and is not
        // an error — the allocator commits on demand.
        let old = self
            .old_gen
            .lock()
            .commit_initial_prefix(bytes.saturating_sub(a + b));
        tracing::debug!(
            target: "cratonvm::gc",
            requested = bytes,
            young_from = a,
            young_to = b,
            old = old,
            "generational: -Xms startup commit",
        );
        a + b
    }

    /// [`Self::commit_initial_heap`] under `CRATONVM_GEN_XMS_USABLE_FIRST`
    /// (gen r5w3/oldgen7): commit `bytes` as [`xms_usable_first_split`]
    /// divides it, and report the young bytes committed (the same contract as
    /// the even split). The old generation takes whatever the young pair did
    /// not, so a refusal in either semi-space still leaves the startup commit
    /// at `-Xms` where the old generation has room for it; its prefix becomes
    /// the shrink floor (`OldGen::commit_initial_prefix`), as before.
    ///
    /// Invariants kept (`gc/tests/gengc_r4w2_obs.rs`): the JMX pools still sum
    /// to [`Self::usable_committed_bytes`] (nothing here changes how the parts
    /// are counted, only how much each arena holds), and `totalMemory <=
    /// maxMemory` (each part is clamped to its arena's capacity, and the young
    /// part counts one semi-space).
    pub fn commit_initial_heap_usable_first(&self, bytes: usize) -> usize {
        let from_cap = self.lock_young_from().capacity();
        let to_cap = self.young_to.lock().capacity();
        let old_cap = self.old_gen.lock().capacity();
        let (from_req, to_req) = xms_usable_first_split(bytes, from_cap, to_cap, old_cap);
        // See the even split for why the guard, not the raw mutex.
        let a = self.lock_young_from().commit_initial_prefix(from_req);
        let b = self.young_to.lock().commit_initial_prefix(to_req);
        self.note_young_xms_floor(a, b);
        let old = self
            .old_gen
            .lock()
            .commit_initial_prefix(bytes.saturating_sub(a + b));
        tracing::debug!(
            target: "cratonvm::gc",
            requested = bytes,
            young_from = a,
            young_to = b,
            old = old,
            "generational: -Xms startup commit (usable first)",
        );
        a + b
    }

    /// Record `-Xms`'s young floor from the two semi-space prefixes the
    /// startup commit made: the LARGER one, i.e. the share that was meant to
    /// be usable (both halves under the even split, the from-space's under
    /// usable-first). gen r5w5/sizer9.
    pub fn note_young_xms_floor(&self, from_prefix: usize, to_prefix: usize) {
        self.young_xms_floor
            .store(from_prefix.max(to_prefix), Ordering::Relaxed);
    }

    /// `-Xms`'s young floor (see the field), `0` without `-Xms`. Tests and
    /// diagnostics.
    pub fn young_xms_floor(&self) -> usize {
        self.young_xms_floor.load(Ordering::Relaxed)
    }

    /// The CAPACITIES of both young semi-spaces and the old generation summed —
    /// the quantity [`heap_budget_bytes`] bounds (a young expansion must keep
    /// it within `-Xmx`).
    ///
    /// Not a committed size despite the name (gen r5w2/obs6 corrected this
    /// doc, which said "bytes currently committed"): it counts the copy
    /// reserve and the old generation's reservation. `Runtime.totalMemory()`
    /// and the JMX pools answer [`Self::usable_committed_bytes`].
    pub fn committed_heap_bytes(&self) -> usize {
        let from = self.lock_young_from().capacity();
        let to = self.young_to.lock().capacity();
        let old = self.old_gen.lock().capacity();
        from + to + old
    }

    /// Bytes this heap has actually asked the OS to BACK, as opposed to the
    /// address space it holds ([`Self::committed_heap_bytes`], which is a sum
    /// of capacities).
    ///
    /// The two differ on a reserving store: each young semi-space sits on
    /// `Arena`, and so on `HeapStore`, which reserves `-Xmx / 4` apiece and
    /// commits it in 2 MiB granules as the bump cursor reaches them. Since gen
    /// r4w2/oldgen2 (2026-09-23) the old generation does the same (it used to
    /// be a `Vec` allocated whole, contributing its full capacity from
    /// construction whatever `-Xms` said).
    ///
    /// # Why this is separate from `committed_heap_bytes`
    ///
    /// That one is the `-Xmx` budget's sum of capacities.
    /// `Runtime.totalMemory()`, which callers require to move only when the
    /// heap grows, is [`Self::usable_committed_bytes`] (see
    /// `VmHeap::committed_bytes`; gen r5w2/obs6 corrected this line, which
    /// named `committed_heap_bytes`). This one is the
    /// measurement: it is what tells a `-Xms` that claims to have committed
    /// bytes apart from one that accepted the flag and dropped it, and until it
    /// existed nothing in the tree could distinguish them
    /// (`docs/feature-designs/generative-gc-architecture.md`).
    pub fn os_committed_bytes(&self) -> usize {
        let from = self.lock_young_from().committed_bytes();
        let to = self.young_to.lock().committed_bytes();
        // gen r4w2/oldgen2: the old generation's COMMITTED bytes, now that it
        // reserves (its capacity on the wholly committed fallback, as before).
        let old = self.old_gen.lock().committed_bytes();
        from + to + old
    }

    // ----- Allocation --------------------------------------------------------

    /// Allocate a new Java object in the young generation.
    pub fn alloc_object(&self, class_id: ClassId, num_fields: usize) -> ObjectRef {
        // Checked arithmetic — an overflowed `total_size` would size the
        // allocation incorrectly. Mirrors `try_alloc_object`'s checked path.
        // `plan_object_alloc` also picks the compact reference-field layout when
        // enabled (smaller `total_size`, `array_length` = body bytes,
        // `GC_FLAG_COMPACT`).
        let (total_size, array_len, compact_flag) =
            plan_object_alloc(self.layout_domain(), class_id, num_fields).unwrap_or_else(|| {
                eprintln!(
                    "FATAL: object size overflow in gen_heap alloc_object \
                     (num_fields={})",
                    num_fields
                );
                std::process::abort();
            });
        // The slot count MUST fit the `u32` header field. Clamping to
        // `u32::MAX` would write a count that disagrees with `total_size`,
        // causing later GC scans to walk off the end of the object.
        let num_slots_u32 = u32::try_from(num_fields).unwrap_or_else(|_| {
            eprintln!(
                "FATAL: object field count exceeds u32 in gen_heap alloc_object \
                 (num_fields={})",
                num_fields
            );
            std::process::abort();
        });
        // Young fast path; on exhaustion spill to old gen (non-moving) BEFORE
        // the hard abort. This panicking entry point is used by the native
        // `ctx.new_*` allocators, which cannot safely GC-and-retry (a moving
        // young GC would dangle their unrooted local ObjectRefs). See
        // [`try_alloc_object_old`]. Only when old gen is also full does
        // `alloc_young_initialized` fire the OOM diagnostic and abort.
        let init = |ptr: *mut u8| {
            let mut header = ObjectHeader::new(
                class_id,
                ObjectKind::Object,
                ArrayElementType::Reference,
                array_len,
                num_slots_u32,
            );
            header.add_gc_flags(compact_flag);
            // SAFETY: `ptr` was just allocated from the young arena with
            // `total_size` bytes and remains protected by the arena lock until
            // this header write completes.
            unsafe {
                std::ptr::write(ptr as *mut ObjectHeader, header);
            }
        };
        let ptr = match self.try_alloc_young_initialized(total_size, &init) {
            Some(p) => p,
            None => {
                // Young exhausted: flag it so the next native-call boundary
                // runs a GC (this method itself must stay GC-free — see the
                // `young_spill_pressure` field doc).
                self.note_young_spill_pressure();
                if let Some(obj) = self.try_alloc_object_old(class_id, num_fields) {
                    return obj;
                }
                self.alloc_young_initialized(total_size, &init)
            }
        };

        // SAFETY: `ptr` is non-null and the header was fully initialized by
        // `try_alloc_young_initialized` / `alloc_young_initialized` before the
        // young arena lock was released.
        unsafe {
            cratonvm_gc_core::a2dbg::record(
                ptr as usize,
                class_id.as_u32(),
                ObjectKind::Object as u8,
                ArrayElementType::Reference as u8,
                array_len,
                num_slots_u32,
                total_size,
            );
            ObjectRef::from_raw(ptr)
        }
    }

    /// Fallible twin of [`alloc_object`]: walks the identical
    /// young → old-gen spill path, but returns `None` instead of aborting the
    /// process when both generations are exhausted (or the size overflows).
    /// This lets a *native*/JIT caller surface a catchable
    /// `java.lang.OutOfMemoryError` ("Java heap space") rather than the VM
    /// hard-aborting in [`alloc_young_initialized`]. Like `alloc_object` it performs no GC,
    /// so it is safe to call from a context holding unrooted local `ObjectRef`s
    /// (the JIT object-alloc helper GC-and-retries before calling this).
    pub fn try_alloc_object_full(&self, class_id: ClassId, num_fields: usize) -> Option<ObjectRef> {
        let (total_size, array_len, compact_flag) =
            plan_object_alloc(self.layout_domain(), class_id, num_fields)?;
        let num_slots_u32 = u32::try_from(num_fields).ok()?;

        // Young fast path; on exhaustion spill into old gen (non-moving) BEFORE
        // reporting OOM — mirrors `alloc_object`, but returns `None` instead of
        // aborting in `alloc_young_initialized` when old gen is also full (the divergence).
        let init = |ptr: *mut u8| {
            let mut header = ObjectHeader::new(
                class_id,
                ObjectKind::Object,
                ArrayElementType::Reference,
                array_len,
                num_slots_u32,
            );
            header.add_gc_flags(compact_flag);
            // SAFETY: the alloc helper hands `init` a freshly bump-allocated, exclusively-owned,
            // zeroed, 8-byte-aligned region of `total_size` bytes; writing the object header at its start is in-bounds.
            unsafe {
                std::ptr::write(ptr as *mut ObjectHeader, header);
            }
        };
        let ptr = match self.try_alloc_young_initialized(total_size, &init) {
            Some(p) => p,
            None => {
                // Young exhausted: flag for the boundary GC (see the
                // `young_spill_pressure` field doc).
                self.note_young_spill_pressure();
                if let Some(obj) = self.try_alloc_object_old(class_id, num_fields) {
                    return Some(obj);
                }
                // Both generations exhausted. Leave the same once-per-run
                // "full vs fragmented" breadcrumb `try_alloc_array_full` leaves
                // (gen r4/alloc): this twin returned `None` silently, so an
                // object-shaped `OutOfMemoryError` from a native was the one
                // fallible-allocation OOM with no occupancy picture behind it.
                self.report_fallible_alloc_exhaustion(total_size);
                return None;
            }
        };

        // SAFETY: identical invariants to `alloc_object` — `ptr` is a freshly
        // bump-allocated, exclusively-owned, zeroed region of `total_size` bytes
        // with 8-byte alignment, so writing the header and wrapping it in an
        // `ObjectRef` are sound.
        unsafe {
            cratonvm_gc_core::a2dbg::record(
                ptr as usize,
                class_id.as_u32(),
                ObjectKind::Object as u8,
                ArrayElementType::Reference as u8,
                array_len,
                num_slots_u32,
                total_size,
            );
            Some(ObjectRef::from_raw(ptr))
        }
    }

    /// Allocate a new Java object and initialize primitive-typed slots to
    /// their spec-mandated typed zero based on `descriptor_bytes`.
    ///
    /// See [`cratonvm_gc_core::heap::default_value_for_descriptor`] for the
    /// descriptor-byte-to-default-Value mapping. This method is the
    /// canonical allocation entry point for the VM's `new` bytecode path —
    /// it fixes a subtle correctness bug where primitive fields on a
    /// freshly-allocated object decoded as `Value::Object(None)` instead
    /// of the correctly-tagged zero, breaking `Unsafe.compareAndSetInt`
    /// comparisons against `Int(0)`.
    ///
    /// Fields beyond `descriptor_bytes.len()`, reference-typed (`L`/`[`)
    /// fields, and unknown-descriptor fields are initialized to an explicit
    /// `Value::Object(None)`, the reference-slot spec default.
    ///
    /// R-niche fix: zeroed memory no longer decodes as `Object(None)` (after
    /// the `NonNull` niche it decodes as `Int(0)`), so the `null` default must
    /// be written explicitly rather than left to `alloc_zeroed`. See
    /// [`cratonvm_gc_core::heap::Heap::alloc_object_with_descriptors`] for the full
    /// rationale.
    pub fn alloc_object_with_descriptors(
        &self,
        class_id: ClassId,
        num_fields: usize,
        descriptor_bytes: &[u8],
    ) -> ObjectRef {
        let obj = self.alloc_object(class_id, num_fields);
        for i in 0..num_fields {
            let default = descriptor_bytes
                .get(i)
                .and_then(|&b| cratonvm_gc_core::heap::default_value_for_descriptor(b))
                .unwrap_or(Value::Object(None));
            self.set_field(obj, i, default);
        }
        obj
    }

    /// Fallible variant of [`Self::alloc_object_with_descriptors`] that
    /// returns `None` on young-gen exhaustion (caller should trigger GC
    /// and retry).
    pub fn try_alloc_object_with_descriptors(
        &self,
        class_id: ClassId,
        num_fields: usize,
        descriptor_bytes: &[u8],
    ) -> Option<ObjectRef> {
        let obj = self.try_alloc_object(class_id, num_fields)?;
        // R-niche fix: write every slot's default explicitly (primitive typed
        // zero, else `Object(None)`) — zeroed memory no longer decodes as null.
        // See `alloc_object_with_descriptors` for the rationale.
        for i in 0..num_fields {
            let default = descriptor_bytes
                .get(i)
                .and_then(|&b| cratonvm_gc_core::heap::default_value_for_descriptor(b))
                .unwrap_or(Value::Object(None));
            self.set_field(obj, i, default);
        }
        Some(obj)
    }

    /// Allocate a new Java array in the young generation.
    ///
    /// Arrays use compact element sizes: 1 byte for boolean/byte, 2 for char/short,
    /// 4 for int/float, 8 for long/double/reference.
    ///
    /// **Humongous routing** (see [`HUMONGOUS_YOUNG_FRACTION_PERCENT`]):
    /// when `HEADER_SIZE + array_data_size` exceeds half of one young
    /// semi-space, the array is allocated directly in the old generation
    /// instead of young.  This mirrors HotSpot G1's humongous handling and
    /// unblocks the case where a single huge array would force the user
    /// to over-size `--Xmx` to ≥ 4× the array size just to make it fit in
    /// one young semi-space.
    pub fn alloc_array(
        &self,
        class_id: ClassId,
        element_type: ArrayElementType,
        length: usize,
    ) -> ObjectRef {
        let data_size = array_data_size(length, element_type)
            .unwrap_or_else(|_| { eprintln!("FATAL: array data size overflow in gen_heap alloc_array (length={}, element_type={:?})", length, element_type); std::process::abort(); });
        let total_size = ARRAY_DATA_OFFSET.checked_add(data_size).unwrap_or_else(|| {
            eprintln!(
                "FATAL: array size overflow in gen_heap alloc_array (length={}, element_type={:?})",
                length, element_type
            );
            std::process::abort();
        });
        // The length MUST fit the `u32` header field. Clamping to `u32::MAX`
        // would record a length that disagrees with the allocated `data_size`,
        // causing later GC scans to walk off the end of the array.
        let length_u32 = u32::try_from(length).unwrap_or_else(|_| {
            eprintln!(
                "FATAL: array length exceeds u32 in gen_heap alloc_array (length={}, element_type={:?})",
                length, element_type
            );
            std::process::abort();
        });

        // Humongous path: skip young, allocate straight into old gen.
        if self.is_humongous(total_size) {
            if let Some(obj) = self.try_alloc_array_humongous(class_id, element_type, length_u32) {
                return obj;
            }
            // Old gen was full — fall through to the young-gen path so the
            // standard OOM diagnostic fires (`alloc_young_initialized` aborts
            // hard with a young-gen-exhaustion message; a future humongous-OOM
            // diagnostic would live here).
        }

        // Young fast path; on exhaustion spill into old gen (non-moving) BEFORE
        // the hard abort. The panicking `alloc_array` is used by the native
        // `ctx.new_array`/`new_ref_array` allocators, which cannot safely
        // GC-and-retry (a moving young GC would dangle their unrooted local
        // ObjectRefs). `try_alloc_array_humongous` allocates in old gen
        // regardless of size; only when old gen is also full does
        // `alloc_young_initialized` fire the OOM diagnostic and abort. See
        // [`try_alloc_object_old`].
        let init = |ptr: *mut u8| {
            let header = ObjectHeader::new(
                class_id,
                ObjectKind::Array,
                element_type,
                length_u32,
                length_u32,
            );
            // SAFETY: the alloc helper hands `init` a freshly bump-allocated, exclusively-owned,
            // zeroed, 8-byte-aligned region of `total_size` bytes; writing the array header at its start is in-bounds.
            unsafe {
                std::ptr::write(ptr as *mut ObjectHeader, header);
            }
        };
        let ptr = match self.try_alloc_young_initialized(total_size, &init) {
            Some(p) => p,
            None => {
                // Young exhausted: flag for the boundary GC (see the
                // `young_spill_pressure` field doc).
                self.note_young_spill_pressure();
                if let Some(obj) =
                    self.try_alloc_array_humongous(class_id, element_type, length_u32)
                {
                    return obj;
                }
                self.alloc_young_initialized(total_size, &init)
            }
        };

        // SAFETY: `ptr` was allocated, zeroed, and header-initialized before
        // the young arena lock was released.
        unsafe {
            cratonvm_gc_core::a2dbg::record(
                ptr as usize,
                class_id.as_u32(),
                ObjectKind::Array as u8,
                element_type as u8,
                length_u32,
                length_u32,
                total_size,
            );
            // Data region already zeroed by the locked young-allocation helper.
            ObjectRef::from_raw(ptr)
        }
    }

    /// Fallible twin of [`alloc_array`]: walks the identical
    /// young → humongous(old-gen) spill path, but returns `None` instead of
    /// aborting the process when the request cannot be satisfied (size/length
    /// overflow, or both generations exhausted). This lets a *native* caller
    /// surface a catchable `java.lang.OutOfMemoryError` (matching HotSpot's
    /// "Requested array size exceeds VM limit" / "Java heap space") rather than
    /// the VM hard-aborting in [`alloc_young_initialized`]. Like `alloc_array` it performs
    /// no GC, so it is safe to call from a native method holding unrooted local
    /// `ObjectRef`s.
    pub fn try_alloc_array_full(
        &self,
        class_id: ClassId,
        element_type: ArrayElementType,
        length: usize,
    ) -> Option<ObjectRef> {
        let data_size = array_data_size(length, element_type).ok()?;
        let total_size = ARRAY_DATA_OFFSET.checked_add(data_size)?;
        let length_u32 = u32::try_from(length).ok()?;

        // Humongous path: skip young, allocate straight into old gen.
        if self.is_humongous(total_size) {
            if let Some(obj) = self.try_alloc_array_humongous(class_id, element_type, length_u32) {
                return Some(obj);
            }
            // Old gen full — fall through to the young path (mirrors `alloc_array`).
        }

        let init = |ptr: *mut u8| {
            let header = ObjectHeader::new(
                class_id,
                ObjectKind::Array,
                element_type,
                length_u32,
                length_u32,
            );
            // SAFETY: the alloc helper hands `init` a freshly bump-allocated, exclusively-owned,
            // zeroed, 8-byte-aligned region of `total_size` bytes; writing the array header at its start is in-bounds.
            unsafe {
                std::ptr::write(ptr as *mut ObjectHeader, header);
            }
        };
        let ptr = match self.try_alloc_young_initialized(total_size, &init) {
            Some(p) => p,
            None => {
                // Young exhausted: flag for the boundary GC (see the
                // `young_spill_pressure` field doc).
                self.note_young_spill_pressure();
                if let Some(obj) =
                    self.try_alloc_array_humongous(class_id, element_type, length_u32)
                {
                    return Some(obj);
                }
                // Both generations exhausted: report OOM to the caller instead
                // of aborting (the divergence from `alloc_array`).
                //
                // Say WHY, once per run: "full" and "too fragmented to serve
                // this request" are very different bugs and the caller only
                // ever saw `OutOfMemoryError`. The old generation's free list
                // is size-segregated and only coalesces on demand or after an
                // in-place sweep (see `OldGen::coalesce_free_blocks`), so a
                // large `free_bytes` next to a small `largest_free_block` is
                // the fragmentation face.
                self.report_fallible_alloc_exhaustion(total_size);
                return None;
            }
        };

        // SAFETY: identical invariants to `alloc_array` — `ptr` is a freshly
        // bump-allocated, exclusively-owned, zeroed region of `total_size`
        // bytes with 8-byte alignment, so writing the header and wrapping it in
        // an `ObjectRef` are sound.
        unsafe {
            cratonvm_gc_core::a2dbg::record(
                ptr as usize,
                class_id.as_u32(),
                ObjectKind::Array as u8,
                element_type as u8,
                length_u32,
                length_u32,
                total_size,
            );
            Some(ObjectRef::from_raw(ptr))
        }
    }

    /// One-shot breadcrumb for a fallible allocation that neither generation
    /// could satisfy. Rate-limited to the first few occurrences: the
    /// user-visible signal is the `OutOfMemoryError` the native boundary is
    /// about to throw, this just makes it diagnosable.
    pub fn report_fallible_alloc_exhaustion(&self, total_size: usize) {
        static N: AtomicU64 = AtomicU64::new(0);
        if N.fetch_add(1, Ordering::Relaxed) >= 4 {
            return;
        }
        let (young_used, young_free, young_cap) = {
            let from = self.lock_young_from();
            (from.used(), from.free_list_bytes(), from.capacity())
        };
        let (old_used, old_cap, old_free, old_largest, old_blocks) = {
            let og = self.old_gen.lock();
            let (free, largest) = og.free_bytes_and_largest();
            (
                og.used(),
                og.capacity(),
                free,
                largest,
                og.free_block_count(),
            )
        };
        tracing::warn!(
            target: "cratonvm::gc::guard",
            request_bytes = total_size,
            young_used,
            young_free_list = young_free,
            young_capacity = young_cap,
            old_used,
            old_capacity = old_cap,
            old_free_bytes = old_free,
            old_largest_free_block = old_largest,
            old_free_blocks = old_blocks,
            "fallible allocation could not be satisfied by either generation — \
             compare old_free_bytes against old_largest_free_block: a large \
             gap means FRAGMENTATION, not exhaustion",
        );
    }

    /// Try to allocate a Java object. Returns `None` if young gen is exhausted.
    pub fn try_alloc_object(&self, class_id: ClassId, num_fields: usize) -> Option<ObjectRef> {
        // M6 (round-12 gc): make the `+ HEADER_SIZE` add checked too, so a
        // near-`usize::MAX` field count can't wrap past the checked multiply.
        // `plan_object_alloc` also selects the compact reference-field layout.
        let (total_size, array_len, compact_flag) =
            plan_object_alloc(self.layout_domain(), class_id, num_fields)?;
        let num_slots_u32 = u32::try_from(num_fields).ok()?;
        let init = |ptr: *mut u8| {
            let mut header = ObjectHeader::new(
                class_id,
                ObjectKind::Object,
                ArrayElementType::Reference,
                array_len,
                num_slots_u32,
            );
            header.add_gc_flags(compact_flag);
            // SAFETY: the alloc helper hands `init` a freshly bump-allocated, exclusively-owned,
            // zeroed, 8-byte-aligned region of `total_size` bytes; writing the object header at its start is in-bounds.
            unsafe {
                std::ptr::write(ptr as *mut ObjectHeader, header);
            }
        };
        let ptr = self.try_alloc_young_initialized(total_size, &init)?;
        // SAFETY: `ptr` was bump-allocated from the young arena with sufficient size
        // and 8-byte alignment via `try_alloc_young_initialized`. The pointer
        // is exclusively owned, so creating an `ObjectRef` is sound.
        unsafe {
            cratonvm_gc_core::a2dbg::record(
                ptr as usize,
                class_id.as_u32(),
                ObjectKind::Object as u8,
                ArrayElementType::Reference as u8,
                array_len,
                num_slots_u32,
                total_size,
            );
            Some(ObjectRef::from_raw(ptr))
        }
    }

    /// Try to allocate a Java array. Returns `None` if young gen (or, for
    /// humongous arrays, the old gen) is exhausted.
    ///
    /// **Humongous routing** (see [`HUMONGOUS_YOUNG_FRACTION_PERCENT`]):
    /// when `HEADER_SIZE + array_data_size` exceeds half of one young
    /// semi-space, the array is allocated directly in the old generation
    /// (G1-style humongous handling).  Without this routing a single
    /// huge array would have to fit in one young semi (each is `Xmx / 4`),
    /// forcing the user to size `--Xmx` to at least `4 * array_size`.
    pub fn try_alloc_array(
        &self,
        class_id: ClassId,
        element_type: ArrayElementType,
        length: usize,
    ) -> Option<ObjectRef> {
        let data_size = array_data_size(length, element_type).ok()?;
        let total_size = ARRAY_DATA_OFFSET.checked_add(data_size)?;
        let length_u32 = u32::try_from(length).ok()?;

        // Humongous path: route straight to old gen so a single array
        // larger than half of one young semi-space doesn't force the
        // caller to over-size `--Xmx`.  See module docs on
        // `HUMONGOUS_YOUNG_FRACTION_PERCENT` for the rationale.
        if self.is_humongous(total_size) {
            if let Some(obj) = self.try_alloc_array_humongous(class_id, element_type, length_u32) {
                return Some(obj);
            }
            // Old gen full — fall through to the young path. If young is
            // also too small, the caller (`gc_alloc_array`) will trigger
            // a GC and retry, which may free old-gen space; if that still
            // can't satisfy the request, the standard OOM fires.
        }

        let init = |ptr: *mut u8| {
            let header = ObjectHeader::new(
                class_id,
                ObjectKind::Array,
                element_type,
                length_u32,
                length_u32,
            );
            // SAFETY: the alloc helper hands `init` a freshly bump-allocated, exclusively-owned,
            // zeroed, 8-byte-aligned region of `total_size` bytes; writing the array header at its start is in-bounds.
            unsafe {
                std::ptr::write(ptr as *mut ObjectHeader, header);
            }
        };
        let ptr = self.try_alloc_young_initialized(total_size, &init)?;
        // SAFETY: `ptr` was bump-allocated from the young arena with sufficient size
        // for the array header + data and 8-byte alignment. The pointer is exclusively
        // owned, so writing the header and creating an `ObjectRef` are sound.
        unsafe {
            cratonvm_gc_core::a2dbg::record(
                ptr as usize,
                class_id.as_u32(),
                ObjectKind::Array as u8,
                element_type as u8,
                length_u32,
                length_u32,
                total_size,
            );
            Some(ObjectRef::from_raw(ptr))
        }
    }

    /// Returns `true` if a `total_size`-byte allocation should bypass
    /// young-gen and go straight to the old generation.
    ///
    /// Threshold = [`HUMONGOUS_YOUNG_FRACTION_PERCENT`] of one young
    /// semi-space capacity, which tracks any in-flight expansion (see
    /// `collect_garbage_inner`'s adaptive growth path).
    ///
    /// The capacity is read from the lock-free [`region_bounds`] mirror
    /// rather than `young_from.lock().capacity()`. `is_humongous` runs on
    /// EVERY array allocation, so the mutex made array allocation a global
    /// serialisation point: measured on this box with a bare
    /// `new long[16]` loop, aggregate allocation throughput was FLAT at
    /// ~11.4 Mops/s from 1 to 4 threads (1.01x for 4x the threads) while
    /// `new Object()` — which never calls this — scaled 3.3x, and HotSpot
    /// scaled ~27x. Any allocation-heavy concurrent workload was capped at
    /// one thread's worth of array allocation. This is the same fix, for
    /// the same reason, that [`region_bounds`] itself documents for
    /// `is_object_address`'s old triple-lock.
    ///
    /// Freshness is identical to the locked read: slot 0 holds young
    /// from-space's `[base, base + capacity)` and is republished at
    /// construction and at the start AND end of every GC — the only points
    /// where the young arenas swap or grow. Mutators allocate only between
    /// GC cycles (GC is stop-the-world), so they always observe the current
    /// capacity. A zero span means the mirror has not been published yet
    /// (pre-construction ordering), so fall back to the authoritative
    /// locked read rather than treat every allocation as humongous.
    #[inline]
    pub fn is_humongous(&self, total_size: usize) -> bool {
        let base = self.region_bounds[0].0.load(Ordering::Acquire);
        let end = self.region_bounds[0].1.load(Ordering::Acquire);
        let semi = match end.checked_sub(base) {
            Some(span) if span != 0 => span,
            _ => self.lock_young_from().capacity(),
        };
        // `semi / 100` first, so `* PERCENT` cannot overflow on a heap sized
        // near `usize::MAX`. It costs up to 99 * PERCENT / 100 bytes of
        // precision, which is 32 bytes on a 64 MiB semi — immaterial to a
        // threshold whose job is "is this array a sizeable fraction of the
        // nursery".
        //
        // gengc-round1-core (2026-09-20): the previous note here credited the
        // `saturating_mul` with preventing "a 0-byte threshold under integer
        // truncation". It does not — the truncation is the DIVISION, and
        // saturation has nothing to say about it. What actually clamps is
        // `threshold.max(HEADER_SIZE)` on the last line, and `HEADER_SIZE`
        // (16) sits BELOW the smallest allocatable object (`HEADER_SIZE +
        // SLOT_SIZE` = 32), so on a semi-space under 100 bytes every
        // allocation is still classified humongous — the outcome the old note
        // claimed was prevented. Left as-is rather than raised, because such a
        // semi-space cannot hold one object either way and routing to old gen
        // is the correct answer there; recorded so the next reader does not
        // rely on a guarantee that is not present.
        let frac_threshold = (semi / 100).saturating_mul(HUMONGOUS_YOUNG_FRACTION_PERCENT);
        // Cap the fraction-based threshold so large arrays bypass the young
        // copying space regardless of heap size (see
        // `HUMONGOUS_ABSOLUTE_CAP_BYTES`). On small/default heaps the fraction
        // is far below the cap, so `min` leaves behaviour unchanged.
        let threshold = frac_threshold.min(HUMONGOUS_ABSOLUTE_CAP_BYTES);
        total_size > threshold.max(HEADER_SIZE)
    }

    /// Allocate a humongous array directly in the old generation.
    ///
    /// The returned object has `GC_FLAG_OLD_GEN` already set on its
    /// header, so the next minor GC will not try to copy it as if it
    /// lived in young from-space.  Returns `None` if old gen is full.
    pub fn try_alloc_array_humongous(
        &self,
        class_id: ClassId,
        element_type: ArrayElementType,
        length_u32: u32,
    ) -> Option<ObjectRef> {
        // Recompute total_size from the (already-validated) length —
        // callers have all run `array_data_size(length, ..)?` upstream so
        // a fresh `array_data_size` cannot overflow here either, but use
        // checked arithmetic just in case the validated path is ever
        // narrowed in a future refactor.
        let data_size = array_data_size(length_u32 as usize, element_type).ok()?;
        let total_size = ARRAY_DATA_OFFSET.checked_add(data_size)?;

        let mut header = ObjectHeader::new(
            class_id,
            ObjectKind::Array,
            element_type,
            length_u32,
            length_u32,
        );
        // Mark as already-promoted so minor GC's `forward_object` does not
        // try to relocate this object — it lives in old gen, not the
        // young from-space arena that gets reset every minor cycle.
        // `OldGen::alloc` zeroed the data region; the header overwrite
        // below initializes the rest of the bookkeeping.
        header.add_gc_flags(GC_FLAG_OLD_GEN);

        // gen r4w4/alloc4 (2026-09-24): a PRIMITIVE humongous array's body is
        // zeroed after the old-generation lock drops. See
        // `humongous_body_zeroes_outside_the_lock` for the protocol and why a
        // reference array keeps the locked memset.
        let unlocked = humongous_body_zeroes_outside_the_lock(
            gc_flags().gen_humongous_zero_unlocked,
            element_type,
            total_size,
        );
        // gen r5w1/oldgen5 (2026-09-26): a humongous REFERENCE array's body is
        // zeroed outside the lock too, under the (default ON since gcg g5)
        // `CRATONVM_GEN_HUMONGOUS_REF_ZERO_UNLOCKED`, behind a `long[]`
        // DISGUISE of the same footprint (`humongous_ref_disguise`, whose doc
        // is the protocol). `None` (a length the disguise cannot encode)
        // keeps the locked memset.
        let ref_disguise = if !unlocked
            && humongous_ref_body_zeroes_outside_the_lock(
                gc_flags().gen_humongous_ref_zero_unlocked,
                element_type,
                total_size,
            ) {
            humongous_ref_disguise(class_id, data_size)
        } else {
            None
        };
        let ref_unlocked = ref_disguise.is_some();
        // The extent `OldGen::alloc` reserves: the request rounded to the
        // 8-byte grid (`alloc_from_buckets` rounds `size` up to `align`).
        let reserved = total_size.checked_add(7)? & !7;
        // gen r5w2/alloc6: only a request that IS humongous arms the
        // compaction below. This function is also the young-exhausted SPILL of
        // `alloc_array` / `try_alloc_array_full` for an ordinary array, and
        // there a refusal says "old gen is full", not "a large array found no
        // hole": arming made the next old-gen collection a whole-generation
        // compaction (`compact_walked` on the moving path) for a small array
        // the fragmentation trigger (`OldGen::note_refused_request`) already
        // judges on its own. Asked before the old-gen lock: `is_humongous` can
        // fall back to the young lock, which is taken before the old one.
        let arm_on_refusal = gc_flags().old_oom_compact && self.is_humongous(total_size);
        // gen r5w3/oldgen7 (`CRATONVM_GC_OLD_HUMONGOUS_TOP`, opt-in): a request
        // that IS humongous — not the young-exhausted spill of an ordinary
        // array, see above — is carved from the TOP of the old generation
        // (`OldGen::alloc_from_top`), which falls back to the ordinary best fit
        // (and its refusal bookkeeping) when nothing fits there. Asked before
        // the old-gen lock, for the same reason as `arm_on_refusal`.
        let from_top = gc_flags().old_humongous_top && self.is_humongous(total_size);
        let ptr = {
            let mut og = self.old_gen.lock();
            // gen r5w1/oldgen5: a refused humongous request asks the next
            // old-gen collection to COMPACT (`OldGen::note_humongous_refusal`),
            // under the allocation-failure ladder's own switch
            // (`CRATONVM_GC_OLD_OOM_COMPACT`, default on). Without it the
            // collection this refusal provokes frees the dead neighbours of
            // the survivors IN PLACE and the request is refused again all the
            // way to a false `OutOfMemoryError` (`GenR4W4HumongousFragProbe`).
            //
            // gen r5w2/alloc6: a refusal whose YOUNG fallback then succeeds
            // (`try_alloc_array` falls through to the nursery for an array that
            // fits there) deliberately leaves the request armed. The array
            // still needs an old-gen hole of the same size when it is promoted,
            // and the generation is as fragmented as the refusal found it; the
            // request does not force a collection by itself (only
            // `arm_fragmentation_compaction`'s pending request does), it only
            // makes the next one that runs compact instead of sweeping.
            let refused = |og: &mut OldGen| {
                if arm_on_refusal {
                    let _ = og.note_humongous_refusal(reserved);
                }
            };
            let ptr = if unlocked || ref_unlocked {
                let placed = if from_top {
                    og.alloc_from_top(total_size, 8, false)
                } else {
                    og.alloc_unzeroed(total_size, 8)
                };
                let Some(ptr) = placed else {
                    refused(&mut *og);
                    return None;
                };
                // gen r5w1/oldgen5: a reference array wears its `long[]`
                // disguise until its body is zero (see below).
                let first = match ref_disguise {
                    Some(disguise) => disguise,
                    None => header,
                };
                // SAFETY: `alloc_unzeroed` returned `reserved` bytes of
                // committed, 8-byte-aligned old-gen memory exclusive to this
                // allocation (`reserved >= total_size > ARRAY_DATA_OFFSET` —
                // the array is humongous). Zeroing its first `ARRAY_DATA_OFFSET`
                // bytes and writing the header there stays inside it. After
                // these two writes the block is one well-formed `T[]` whose
                // header spans the whole reserved extent, which is the
                // `alloc_unzeroed` obligation ("no walker may reach the block
                // before it holds objects end to end") discharged before the
                // lock that makes the block visible to a walker is released.
                // (The disguise spans exactly the same extent.)
                unsafe {
                    std::ptr::write_bytes(ptr, 0, ARRAY_DATA_OFFSET);
                    std::ptr::write(ptr as *mut ObjectHeader, first);
                }
                ptr
            } else {
                let placed = if from_top {
                    og.alloc_from_top(total_size, 8, true)
                } else {
                    og.alloc(total_size, 8)
                };
                let Some(ptr) = placed else {
                    refused(&mut *og);
                    return None;
                };
                // SAFETY: `OldGen::alloc` (or `alloc_from_top` with `zero`)
                // returned a pointer to `total_size`
                // bytes of zeroed, 8-byte-aligned memory exclusive to this
                // allocation. The old-gen lock is still held, so major GC cannot
                // walk the span until this complete header has been published.
                unsafe {
                    std::ptr::write(ptr as *mut ObjectHeader, header);
                }
                ptr
            };
            // gen r4w5/concmark5: direct old-gen growth, no young collection.
            self.poll_concurrent_start_after_direct_old_alloc(&og);
            ptr
        };
        if unlocked || ref_unlocked {
            // SAFETY: `[ptr + ARRAY_DATA_OFFSET, ptr + reserved)` is the body of
            // the block reserved above for this caller alone. No thread holds a
            // reference to the array until this function returns it, and the
            // only readers that can reach the block without one — old-gen
            // walkers striding by headers — read the header written under the
            // lock and never a primitive body (see
            // `humongous_body_zeroes_outside_the_lock`; a reference array's
            // header is a primitive `long[]` until the re-lock below).
            unsafe {
                std::ptr::write_bytes(ptr.add(ARRAY_DATA_OFFSET), 0, reserved - ARRAY_DATA_OFFSET);
            }
            // The zeroes happen-before any publication of the reference: a
            // thread that later loads the reference from a heap slot and then
            // reads the body (a concurrent marker scanning a holder, say) must
            // not see the old bytes on a weakly ordered CPU.
            std::sync::atomic::fence(Ordering::Release);
            if ref_unlocked {
                // gen r5w1/oldgen5: the body is zero; drop the disguise. Under
                // the old-gen lock, so a concurrent reader (every one holds
                // that lock: the concurrent mark's slices and overflow rescan,
                // the concurrent sweep's) sees either the whole `long[]` or the
                // whole reference array, and — by the lock's release/acquire —
                // a zeroed body with the latter. Nothing but this thread has
                // written the header since the first lock (no reference to the
                // block exists yet, and no collector writes a header outside a
                // pause, which cannot start while this thread is here).
                let mut real = ObjectHeader::new(
                    class_id,
                    ObjectKind::Array,
                    element_type,
                    length_u32,
                    length_u32,
                );
                real.add_gc_flags(GC_FLAG_OLD_GEN);
                let _og = self.old_gen.lock();
                // SAFETY: `ptr` is the start of the block reserved above for
                // this caller alone; the header span is inside it.
                unsafe { std::ptr::write(ptr as *mut ObjectHeader, real) };
            }
            self.stats
                .humongous_unlocked_zeroes
                .fetch_add(1, Ordering::Relaxed);
        }
        self.stats.old_allocations.fetch_add(1, Ordering::Relaxed);
        self.age_clock_charge_old_direct(total_size); // gci i3/yg
        // SAFETY: `ptr` points at the header just written above; it is a valid,
        // fully-initialized, heap-owned object so wrapping it as an `ObjectRef` is sound.
        Some(unsafe { ObjectRef::from_raw(ptr) })
    }

    /// Allocate a Java object DIRECTLY in the old generation (non-moving),
    /// used as an overflow fallback when the young from-space is exhausted.
    /// Returns `None` if the old gen is also full.
    ///
    /// This is the object analogue of [`try_alloc_array_humongous`] and exists
    /// for the same safety reason that motivates routing the *panicking*
    /// `alloc_object`/`alloc_array` here on young-full: those entry points are
    /// used by the convenience native allocators (`ctx.new_object`/`new_array`/
    /// `new_ref_array`, `alloc_concurrent_synthetic`, …). A native holds raw
    /// `ObjectRef`s in Rust locals that are NOT in any GC root set, so we cannot
    /// trigger a moving/promoting young GC from there (it would relocate those
    /// objects and leave the native's locals dangling — exactly the stale-ref
    /// class of SEGV). The old-gen allocator never relocates a live object, so
    /// spilling the single allocation into old gen lets a young-full native call
    /// succeed without GC instead of `std::process::abort()`-ing the whole VM.
    /// The `GC_FLAG_OLD_GEN` mark keeps minor GC from trying to forward it.
    pub fn try_alloc_object_old(
        &self,
        class_id: ClassId,
        num_fields: usize,
    ) -> Option<ObjectRef> {
        let (total_size, array_len, compact_flag) =
            plan_object_alloc(self.layout_domain(), class_id, num_fields)?;
        let mut header = ObjectHeader::new(
            class_id,
            ObjectKind::Object,
            ArrayElementType::Reference,
            array_len,
            u32::try_from(num_fields).ok()?,
        );
        header.add_gc_flags(GC_FLAG_OLD_GEN | compact_flag);
        let ptr = {
            let mut og = self.old_gen.lock();
            let ptr = og.alloc(total_size, 8)?;
            // SAFETY: `OldGen::alloc` returned `total_size` bytes of zeroed,
            // 8-byte-aligned memory exclusive to this allocation. Holding the
            // old-gen lock through the header write prevents major GC from
            // observing the allocation before it is a valid object.
            unsafe {
                std::ptr::write(ptr as *mut ObjectHeader, header);
            }
            // gen r4w5/concmark5: direct old-gen growth, no young collection.
            self.poll_concurrent_start_after_direct_old_alloc(&og);
            ptr
        };
        self.stats.old_allocations.fetch_add(1, Ordering::Relaxed);
        self.age_clock_charge_old_direct(total_size); // gci i3/yg
        // SAFETY: `ptr` points at the header just written above; it is a valid,
        // fully-initialized, heap-owned object so wrapping it as an `ObjectRef` is sound.
        Some(unsafe { ObjectRef::from_raw(ptr) })
    }

    /// Allocate a same-layout old-generation batch in `OldGen`,
    /// holding its lock once. Native wrapper allocation uses this after young
    /// space is exhausted, keeping unused objects in a GC-visible per-thread
    /// pool. A partial batch is valid when old space runs out.
    pub fn try_alloc_objects_old_batch(
        &self,
        class_id: ClassId,
        num_fields: usize,
        count: usize,
    ) -> Vec<ObjectRef> {
        // This batch refill only runs once young can no longer supply a TLAB
        // (see `NativeContextImpl::alloc_object`), i.e. on a young-exhaustion
        // spill: arm the boundary-GC pressure flag (advisability-gated; one
        // extra old-gen lock per 2048-object batch, not per allocation).
        self.note_young_spill_pressure();
        let Some((total_size, array_len, compact_flag)) =
            plan_object_alloc(self.layout_domain(), class_id, num_fields)
        else {
            return Vec::new();
        };
        let Ok(num_slots) = u32::try_from(num_fields) else {
            return Vec::new();
        };
        let mut objects = Vec::with_capacity(count);
        let mut og = self.old_gen.lock();
        for _ in 0..count {
            let Some(ptr) = og.alloc(total_size, 8) else {
                break;
            };
            let mut header = ObjectHeader::new(
                class_id,
                ObjectKind::Object,
                ArrayElementType::Reference,
                array_len,
                num_slots,
            );
            header.add_gc_flags(GC_FLAG_OLD_GEN | compact_flag);
            // SAFETY: `OldGen::alloc` returned an exclusive, zeroed span and
            // the old-generation lock remains held until the header is valid.
            unsafe {
                std::ptr::write(ptr as *mut ObjectHeader, header);
                objects.push(ObjectRef::from_raw(ptr));
            }
        }
        // gen r4w5/concmark5: direct old-gen growth, no young collection (one
        // poll per batch, under the lock the batch already holds).
        if !objects.is_empty() {
            self.poll_concurrent_start_after_direct_old_alloc(&og);
        }
        self.stats
            .old_allocations
            .fetch_add(objects.len() as u64, Ordering::Relaxed);
        objects
    }

    /// DIAGNOSTIC-ONLY (cce0079 tree-key tail): current minor-GC count, for
    /// the `[SETFIELD-GC]` epoch assertion in the VM's `set_field` wrapper.
    pub fn debug_minor_gc_count(&self) -> u64 {
        self.stats.minor_gc_count.load(Ordering::Relaxed)
    }

    // ----- Header access -----------------------------------------------------

    /// Read the object header from a heap reference.
    ///
    /// PERF: this is *the* accessor for `get_field`/`set_field`/`class_id_of`/
    /// `kind_of`/`identity_hash_code`, so a single native collection call
    /// reaches it several times. Both diagnostics it hosts are default-inert
    /// and gated on a cached bool, but with the whole body out of line every
    /// header read still cost a call/return pair — 7.0% of the `CratonBench
    /// hashmap` phase, as its own profile symbol. The pointer read is now
    /// inline and both gated bodies live in the `#[cold]`
    /// [`Self::get_header_diagnostics`].
    #[inline]
    pub fn get_header(&self, obj_ref: ObjectRef) -> &ObjectHeader {
        // SAFETY: `obj_ref` was created by one of this heap's `alloc_*` methods (or
        // forwarded during GC), so its pointer targets a valid, fully initialized
        // `ObjectHeader` within a heap-owned arena. The reference lifetime is bounded
        // by `&self`, ensuring the arena stays alive.
        let header = unsafe { &*(obj_ref.as_ptr() as *const ObjectHeader) };
        if cratonvm_gc_core::stale_objref_debug::enabled() || cratonvm_gc_core::blocked_access_debug::enabled() {
            self.get_header_diagnostics(obj_ref, header);
        }
        header
    }

    /// [`Self::get_header`] for an address [`Self::is_object_start`] admitted,
    /// without building an `ObjectRef` (two thread-local accesses) unless a
    /// header diagnostic is armed (`VmHeap::header_if_object_start`,
    /// interpreter round i1 wave 51, lane L7).
    ///
    /// # Safety
    ///
    /// `addr` must be an object start of this heap, as `is_object_start`
    /// answered with no intervening safepoint.
    #[inline(always)]
    pub unsafe fn header_at_object_start(&self, addr: usize) -> &ObjectHeader {
        // SAFETY: the caller's contract: a live, fully initialized header.
        let header = unsafe { &*(addr as *const ObjectHeader) };
        if cratonvm_gc_core::stale_objref_debug::enabled() || cratonvm_gc_core::blocked_access_debug::enabled() {
            // SAFETY: an object start: non-null and 8-byte aligned.
            self.get_header_diagnostics(unsafe { ObjectRef::from_raw(addr as *mut u8) }, header);
        }
        header
    }

    /// The two default-inert diagnostics [`Self::get_header`] hosts. Reached
    /// only when `CRATONVM_DBG_STALE_OBJREF` or the blocked-access gate is
    /// armed; both re-test their own gate, so behaviour is unchanged when
    /// exactly one of them is on.
    #[cold]
    #[inline(never)]
    pub fn get_header_diagnostics(&self, obj_ref: ObjectRef, header: &ObjectHeader) {
        // gen r5w4/pin8: the pinned in-place cycle's half of the canary. That
        // cycle does not keep a forwarding header at a moved object's old
        // address (it re-serves the bytes); under this flag it leaves a
        // POISONED FILLER there for one cycle instead
        // (`finish_in_place_young_cycle`), so a caller reaching one through
        // `get_header` holds exactly the stale reference the forwarded-header
        // arm below catches on a Cheney cycle.
        if cratonvm_gc_core::stale_objref_debug::enabled()
            && is_quarantine_stamp(obj_ref.as_ptr() as usize, header)
        {
            let addr = obj_ref.as_ptr() as usize;
            let vacated = young_freed_lookup(addr)
                .map(|(b, s, c, _, _)| {
                    format!(" (span [{b:#x},{:#x}) vacated by young cycle {c})", b + s)
                })
                .unwrap_or_default();
            cratonvm_gc_core::stale_objref_debug::LAST_STALE_ADDR.store(addr, Ordering::Release);
            panic!(
                "CRATONVM_DBG_STALE_OBJREF: stale ObjectRef detected at {addr:#x} -- a pinned \
                 in-place young cycle moved this object and quarantined its old bytes{vacated}, \
                 but native/interpreter code dereferenced the OLD address: a reference the \
                 cycle's root channels did not rewrite. See \
                 docs/feature-designs/generative-gc-architecture.md."
            );
        }
        // CRATONVM_DBG_STALE_OBJREF: `get_header` is the accessor native code
        // and interpreter bytecode dispatch use to inspect a supposedly-live
        // object (`get_field`/`set_field`/`class_id_of`/`array_length`/
        // `identity_hash_code`/etc. all funnel through here) — the GC's own
        // internal forward/remap machinery reads headers through its own raw
        // pointer casts instead (see `forward_object_impl`), so it never hits
        // this check. A forwarded header reaching a caller here means the
        // caller is holding a raw `ObjectRef` local across a GC-triggering
        // call without `pin_native_root`/`read_native_pin` — exactly the
        // "Family 1" stale-ObjectRef pattern documented in
        // wildfly-parallel-boot-stale-objectref-residual.md.
        // Only reachable when the quarantine dance in `collect_garbage_inner`
        // is active (see the `quarantine` field), since without it the
        // evacuated memory would already have been zeroed by the time a
        // mutator could observe it.
        if cratonvm_gc_core::stale_objref_debug::enabled() && header.is_forwarded() {
            // DIAGNOSTIC-ONLY (attrib-cce-investigate, 2026-07-13): read the
            // object's REAL, fully-valid header at the forwarded (new)
            // address so the panic message identifies which class/kind went
            // stale — the old-address header is just a forwarding marker by
            // this point. Not a functional change: only executed on the
            // already-panicking path, gated behind the same debug flag.
            let fwd_ptr = header.forwarding_address();
            // 2026-08-03: `is_forwarded()` is a bare `!fwd_ptr.is_null()` check
            // with no validation of what it points at — the same trust this
            // diagnostic itself used to extend to `fwd_ptr` before dereferencing
            // it below. When the header under suspicion is not a genuine
            // forwarding marker but a corrupted one (garbage bytes that happen
            // to leave `forwarding_ptr` non-null — the same corruption class
            // `old_gen_mark_candidate_plausible` screens for elsewhere), that
            // deref reads unmapped memory and SIGSEGVs the diagnostic itself
            // instead of reporting the corruption it exists to catch. Route
            // through `is_object_address` (lock-free, bounds+alignment+tag
            // validated) exactly like every other "is this byte pattern a real
            // object base" check in this file, and report an implausible target
            // as data rather than crashing on it.
            let (fwd_class_id, fwd_kind) = if !fwd_ptr.is_null() {
                match self.is_object_address(fwd_ptr as usize) {
                    Some(fwd_obj) => {
                        // SAFETY: `is_object_address` validated `fwd_ptr` as a
                        // plausible object base inside a managed region before
                        // returning it; read-only, diagnostic-only (stale-objref
                        // debug) path.
                        let fwd_header = unsafe { &*(fwd_obj.as_ptr() as *const ObjectHeader) };
                        // Raw byte, not `Debug` — see `warn_non_object_kind_in_object_arm`.
                        (
                            fwd_header.class_id.as_u32(),
                            format!(
                                "0x{:02x}",
                                ObjectHeader::kind_tag(
                                    fwd_header.mark_word.load(Ordering::Relaxed)
                                )
                            ),
                        )
                    }
                    None => (
                        u32::MAX,
                        format!("<implausible-forward 0x{:x}>", fwd_ptr as usize),
                    ),
                }
            } else {
                (u32::MAX, "<null-forward>".to_string())
            };
            // DIAGNOSTIC-ONLY (cceres2, 2026-07-16): before dying, name every
            // heap slot that STILL holds the stale (old) address. A holder in
            // OLD gen indicts the old->young scan/remap (card path) for that
            // slot's store site; NO heap holder means the stale copy lived
            // only in a frame/register/native local (root-remap gap). Card
            // state is printed as-of-panic (the GC consumes dirty bits, so
            // false here does NOT prove the card was clean at GC time).
            let stale_usize = obj_ref.as_ptr() as usize;
            let mut holders = String::new();
            let mut holder_n = 0usize;
            {
                use std::fmt::Write as _;
                if let Some(old) = self.old_gen.try_lock() {
                    'oldscan: for (optr, sz) in old.walk_objects() {
                        // SAFETY: walk_objects yields valid object starts.
                        let oh = unsafe { &*(optr as *const ObjectHeader) };
                        // Re-derive this object's total size from the header
                        // `for_each_ref_slot` is about to trust (`num_slots`/
                        // `array_length`) and compare it against `sz`, the size
                        // `walk_objects` already validated fits this object's
                        // slot in the region a moment ago. This is the same
                        // "the caller must not trust a header `for_each_ref_slot`
                        // hasn't screened" gap `get_header_diagnostics`'s
                        // `fwd_ptr` deref had — a header this diagnostic reads a
                        // second time, on a heap the panic path does not hold a
                        // GC-exclusive lock over, can read differently than the
                        // walk that produced `sz`. Trusting it blindly here made
                        // `for_each_ref_slot` stride `num_slots`/`array_length`
                        // slots past a plausible-looking `optr` and SIGSEGV the
                        // diagnostic itself, exactly like the `fwd_ptr` case
                        // above. Skip (report nothing for this object) rather
                        // than crash — the same "over-retain / under-report, but
                        // never dereference blind" tradeoff as everywhere else in
                        // this holder scan.
                        if gen_object_total_size(oh) != sz {
                            continue 'oldscan;
                        }
                        let mut hits: Vec<usize> = Vec::new();
                        // SAFETY: header/object pair valid for the walk; `sz`
                        // re-check above confirms the header is self-consistent
                        // with what `walk_objects` measured for this slot.
                        unsafe {
                            for_each_ref_slot(optr, oh, |raw, idx| {
                                if raw as usize == stale_usize {
                                    hits.push(idx);
                                }
                            });
                        }
                        for idx in hits {
                            // gen r4w4/cards4: the card a barrier would have
                            // dirtied for THIS slot (the element's card for a
                            // precise reference array), not only the header's.
                            let (card_dirty_now, _) =
                                rset_edge_card_is_dirty(&self.card_table, optr as usize, oh, idx);
                            let _ = write!(
                                holders,
                                "\n  OLD holder {:p} class_id={} kind=0x{:02x} slot={} card_dirty_now={}",
                                optr,
                                oh.class_id.as_u32(),
                                ObjectHeader::kind_tag(oh.mark_word.load(Ordering::Relaxed)),
                                idx,
                                card_dirty_now,
                            );
                            holder_n += 1;
                            if holder_n >= 16 {
                                break 'oldscan;
                            }
                        }
                    }
                } else {
                    let _ = write!(holders, "\n  (old_gen lock busy — old holders not scanned)");
                }
                // Young from-space: lock-free raw word scan over the published
                // region bounds (object-walk under mutation is not crash-safe).
                let yf_base = self.region_bounds[0].0.load(Ordering::Acquire);
                let yf_end = self.region_bounds[0].1.load(Ordering::Acquire);
                if yf_base != 0 && yf_end > yf_base && holder_n < 16 {
                    // gen r4w5/review5 (2026-09-24): `[yf_base, yf_end)` is the
                    // arena's whole CAPACITY, and that is not all mapped: the
                    // young arenas commit lazily, and `uncommit_evacuated_young`
                    // (default ON) hands the evacuated semi-space -- the NEXT
                    // cycle's from-space -- back to the OS. This scan faulted on
                    // the first released granule. Screen each granule once
                    // through the region's commit map and skip the released ones
                    // (they hold no word, let alone a holder).
                    let granule = cratonvm_gc_core::reservation::GRANULE;
                    let mut screened_until = yf_base;
                    let mut addr = yf_base;
                    while addr + 8 <= yf_end {
                        if addr >= screened_until {
                            let next = yf_base + ((addr - yf_base) / granule + 1) * granule;
                            if !self.region_range_committed(addr, 8) {
                                addr = next;
                                continue;
                            }
                            screened_until = next;
                        }
                        // SAFETY: `addr` is 8-aligned inside `[yf_base, yf_end)`
                        // and its granule is committed (screened just above; an
                        // 8-aligned word never crosses a granule boundary). The
                        // caller is a mutator on its panic path, so no collection
                        // can decommit it underneath this read.
                        let w: u64 = unsafe { std::ptr::read_volatile(addr as *const u64) };
                        if w as usize == stale_usize {
                            let _ = write!(
                                holders,
                                "\n  YOUNG word 0x{addr:x} (from-space offset 0x{:x}) still holds the stale address",
                                addr - yf_base,
                            );
                            holder_n += 1;
                            if holder_n >= 16 {
                                break;
                            }
                        }
                        addr += 8;
                    }
                }
                if holder_n == 0 {
                    let _ = write!(
                        holders,
                        "\n  NO heap holder found — the stale copy lived only in a \
                         frame/register/native local (root-remap gap), or its holder \
                         was itself already collected",
                    );
                }
            }
            cratonvm_gc_core::stale_objref_debug::LAST_STALE_ADDR
                .store(obj_ref.as_ptr() as usize, Ordering::Release);
            panic!(
                "CRATONVM_DBG_STALE_OBJREF: stale ObjectRef detected at {:p} — this \
                 object was evacuated by a moving GC to {:p} (class_id={fwd_class_id} \
                 kind={fwd_kind}), but native/interpreter code \
                 dereferenced the OLD address. This means a raw ObjectRef local was held \
                 across a GC-triggering call without pin_native_root/read_native_pin. See \
                 wildfly-parallel-boot-stale-objectref-residual.md.\
                 \nHolder scan:{holders}",
                obj_ref.as_ptr(),
                fwd_ptr,
            );
        }
        // CRATONVM_DBG_BLOCKED_ACCESS: a header access on a thread whose own
        // `in_blocked_region` flag is raised races any concurrently running
        // collection — the STW census excluded this thread, so nothing on it
        // may touch the heap until `check_post_block_gc` re-syncs it. No-op
        // (one cached-bool branch) when the gate is off. See
        // `blocked_access_debug` for the full rationale.
        cratonvm_gc_core::blocked_access_debug::check_blocked_access(
            "heap header access",
            obj_ref.as_ptr() as usize,
        );
    }

    /// Get the class id of a heap object.
    pub fn class_id_of(&self, obj_ref: ObjectRef) -> ClassId {
        self.get_header(obj_ref).class_id
    }

    /// Get the kind (Object or Array) of a heap allocation.
    pub fn kind_of(&self, obj_ref: ObjectRef) -> ObjectKind {
        self.get_header(obj_ref).kind()
    }

    /// Get the element type of an array object.
    pub fn element_type_of(&self, obj_ref: ObjectRef) -> ArrayElementType {
        self.get_header(obj_ref).element_type()
    }

    /// Get the identity hash code of a heap object.
    ///
    /// The JIT's inline `new` fast path (`jit/src/x64.rs`) deliberately
    /// leaves this header field TLAB-zeroed rather than paying a mint on
    /// every allocation, deferring to what its own comment calls a "lazy-
    /// mint contract". This is that contract: when the stored value is 0,
    /// mint a fresh one here and CAS it into the header so it becomes a
    /// durable, GC-move-stable property of the object from this point on
    /// (every mover in this file copies the header's bytes verbatim, so
    /// once non-zero this field survives every future relocation). Without
    /// this, callers that need a *stable* per-object key (e.g.
    /// `native-builtins/src/keystore.rs`'s `store_id_by_identity`, or
    /// `VarHandle` root dedup in `vm_exec.rs`) would otherwise have to
    /// derive one from the object's current address — which silently goes
    /// stale the moment a moving GC relocates the object. See
    /// `tomcat-embedded-server-keystore-empty-cert-chain-intermittent-FIXED.md`.
    pub fn identity_hash_code(&self, obj_ref: ObjectRef) -> i32 {
        let header = self.get_header(obj_ref);
        // A long header keeps the hash in its aux word; a compact instance in
        // a NEUTRAL mark word, installed lazily by one CAS. `Err` means a
        // compact instance is locked -- its hash lives in its Monitor.
        let (hash, displaced) = match header.identity_hash(|| match self.next_hash() {
            0 => i32::MAX,
            h => h,
        }) {
            Ok(hash) => (hash, false),
            Err(()) => {
                // Deliberately no mint on this path. Minting for a non-NEUTRAL
                // object would return a different value on every call, i.e. an
                // identity hash that changes under a live object -- which a
                // test calling it once cannot see.
                (
                    cratonvm_gc_core::collector::displaced_identity_hash(obj_ref.as_ptr() as usize),
                    true,
                )
            }
        };
        if self.ihash_witness {
            gci_i11jd_ihash_witness(obj_ref, header, hash, displaced);
        }
        hash
    }

    /// BUG-03 — publish the reserved-tail regions of forcibly-stopped in-JIT
    /// peers' TLABs so the next non-moving young sweep skips them (see
    /// [`Self::jit_tlab_skip_regions`]). Replaces any previously-set list.
    /// Pass an empty slice (or call [`Self::clear_jit_tlab_skip_regions`]) to
    /// reset. Must be set under STW, immediately before the collection, and
    /// cleared immediately after.
    pub fn set_jit_tlab_skip_regions(&self, regions: &[(usize, usize)]) {
        let mut g = self.jit_tlab_skip_regions.lock();
        g.clear();
        g.extend_from_slice(regions);
    }

    /// BUG-03 — clear the JIT TLAB skip regions (see
    /// [`Self::set_jit_tlab_skip_regions`]).
    pub fn clear_jit_tlab_skip_regions(&self) {
        self.jit_tlab_skip_regions.lock().clear();
    }

    /// BUG-03 — the published JIT TLAB skip regions as young-from byte
    /// offsets `(offset, size)`, filtered to those that fall wholly inside the
    /// given `[from_base, from_end)` window, sorted ascending, and **coalesced
    /// so no two entries overlap**. Empty on the normal collection path.
    ///
    /// # Why the coalesce is load-bearing (TLAB audit, `tlab-and-card-audit.md`)
    ///
    /// Both consumers merge this list with `Arena::free_blocks_sorted()` and
    /// then feed the result to [`skip_free_blocks`], whose contract is a list of
    /// **ascending, disjoint** spans: it advances the walk cursor to `off + sz`
    /// and drops the entry. Two entries covering the same bytes therefore make
    /// the walker "skip the same tail twice" — the second entry is consumed
    /// harmlessly only when the cursor already sits past it, but a *partially*
    /// overlapping pair (`[a, c)` then `[b, d)` with `a < b < c < d`) resyncs the
    /// cursor to `c`, then immediately to `d`, silently swallowing the objects
    /// in `[c, d)`. On the object-start walk that is a missed object grid entry;
    /// on the non-moving sweep it is a live object never visited.
    ///
    /// Before this change the disjointness was asserted in prose at both call
    /// sites ("both inputs are ascending & disjoint") and guaranteed by nothing:
    /// the producer is `ThreadRegistry::collect_reserved_tlab_tails`, which
    /// reads one `(cursor, end)` pair per ALIVE registry entry through a
    /// published raw address. Two entries can name the same `Tlab` — a virtual
    /// thread's boxed `JvmThread` is published under its own thread id at
    /// `install_runtime` and re-published at every remount, and a registry that
    /// ever holds two live ids for one continuation (or a foreign thread that
    /// re-attaches before its previous entry is reaped) yields the identical
    /// pair twice. Duplicates are also exactly what a stale, never-cleared
    /// publication looks like.
    ///
    /// Coalescing is the fail-safe direction: over-skipping a span can only
    /// over-retain (the span is treated as reserved, so nothing in it is
    /// reclaimed), whereas the desync it prevents frees live memory. The
    /// `debug_assert!` records the invariant the callers rely on.
    pub fn jit_tlab_skip_offsets(&self, from_base: usize, from_end: usize) -> Vec<(usize, usize)> {
        // `CRATONVM_GC_NO_TLAB_SKIP=1` -- ignore every published skip span.
        //
        // A published span is supposed to be a still-RESERVED, un-allocated
        // TLAB tail, and the sweep's linear walks skip it so an un-retired tail
        // cannot desync them (BUG-03). A span that is STALE -- published by an
        // earlier collection and not cleared, by which time the owning thread
        // has bump-allocated into it -- has the opposite effect: it hides LIVE
        // objects from every walk, and the mark oracle then answers "gap space,
        // not an object" for any root pointing into one, so the object is
        // neither scanned nor swept while everything it references is freed.
        //
        // The switch exists to price that, in one binary, against
        // `io.netty.util.internal.ObjectCleanerTest` -- whose reclaimed lambda
        // is held by a `Collections$ReverseComparator2` that the cycle-0 sweep
        // reports as `in_jit_tlab_skip=Some(..)`.
        if false {
            return Vec::new();
        }
        let g = self.jit_tlab_skip_regions.lock();
        if g.is_empty() {
            return Vec::new();
        }
        let mut v: Vec<(usize, usize)> = g
            .iter()
            .filter_map(|&(c, e)| {
                if c >= from_base && e <= from_end && e > c && (c & 0x7) == 0 {
                    Some((c - from_base, e - c))
                } else {
                    None
                }
            })
            .collect();
        drop(g);
        v.sort_by_key(|&(off, _)| off);
        // Coalesce overlapping and touching spans into one. `sz` is recomputed
        // from the merged end so the result is always `(start, end - start)`.
        let mut merged: Vec<(usize, usize)> = Vec::with_capacity(v.len());
        for (off, sz) in v {
            match merged.last_mut() {
                Some((last_off, last_sz)) if off <= *last_off + *last_sz => {
                    let end = (off + sz).max(*last_off + *last_sz);
                    *last_sz = end - *last_off;
                }
                _ => merged.push((off, sz)),
            }
        }
        debug_assert!(
            merged
                .windows(2)
                .all(|w| w[0].0 + w[0].1 <= w[1].0 && w[0].1 > 0),
            "jit_tlab_skip_offsets must return ascending, disjoint, non-empty spans — \
             `skip_free_blocks` resyncs the walk cursor to each span's end and would \
             swallow the objects between two partially-overlapping spans: {merged:?}",
        );
        merged
    }

    /// Lock-free `[min_base, max_end)` envelope over every arena this heap
    /// can hold an object in (young-from, young-to, old-gen).
    ///
    /// Conservative stack scanning tests one word at a time and the
    /// overwhelming majority of those words are not heap addresses at all.
    /// [`Self::is_object_address`] already rejects them, but only after an
    /// out-of-line call through [`cratonvm_gc_core::gpu_critical::VmHeap`]'s backend dispatch
    /// and three pairs of `Acquire` loads. Hoisting this envelope out of the
    /// scan loop lets the caller reject those words with a single compare,
    /// calling the full validator only for words that could plausibly be
    /// object headers. Returns `None` when no arena is published yet, which
    /// callers must read as "no fast filter available" (scan everything).
    ///
    /// Only valid while the bounds are stable — they are republished under
    /// STW at GC start/end, which is exactly when no scan is in flight.
    pub fn conservative_addr_span(&self) -> Option<(usize, usize)> {
        let mut lo = usize::MAX;
        let mut hi = 0usize;
        for (base, end) in self.region_bounds.iter() {
            let b = base.load(Ordering::Acquire);
            let e = end.load(Ordering::Acquire);
            if b == 0 || e <= b {
                continue;
            }
            lo = lo.min(b);
            hi = hi.max(e);
        }
        if hi > lo {
            Some((lo, hi))
        } else {
            None
        }
    }

    /// Ask arena `slot`'s exact object-start bitmap about `addr`.
    ///
    /// `None` means "not tracked" -- the switch is off, or the arena was grown
    /// and dropped its bitmap -- and is a different answer from `Some(false)`.
    /// Only slots 0 and 1 (the young semi-spaces) are `Arena`s; slot 2 is the
    /// old generation, which is a `Vec<u8>` with no arena and therefore no
    /// bitmap.
    pub fn arena_object_start(&self, slot: usize, addr: usize) -> Option<bool> {
        // Slot 2 is the old generation, which is a `Vec<u8>` with no `Arena`
        // and therefore no bitmap.
        let cell = self.object_starts.get(slot)?;
        let ptr = cell.load(Ordering::Acquire) as *const cratonvm_gc_core::heap_bitmap::HeapBitmap;
        if ptr.is_null() {
            return None;
        }
        // SAFETY: the pointer was published by `store_object_starts_locked`
        // from an `Arc` that `object_starts_hold` owns while it is current,
        // and that `object_starts_retired` keeps for one more publish after it
        // is replaced (gcf f4/y), so a pointer loaded before a publish stays
        // valid until the publish after it -- a whole collection later.
        // `HeapBitmap` is `Sync` and `contains` takes `&self`.
        Some(unsafe { (*ptr).contains(addr) })
    }

    /// Conservative validity check for a *raw address* — used by NEW-1.5
    /// JIT frame root scanning to filter spurious stack values.
    ///
    /// Returns `Some(ObjectRef)` if `addr` lands on a live object header in
    /// any of this heap's three storage regions (young from-space, young
    /// to-space, or old-gen). Returns `None` for any address that is null,
    /// outside every region, or fails the header sanity checks (alignment,
    /// kind tag, plausible num_slots).
    ///
    /// This is intentionally a structural check only — we do not consult
    /// the live-object marking bitmap or the class manager. False positives
    /// are acceptable because the caller treats every returned `ObjectRef`
    /// as a *root* (which only inflates retention; it cannot cause incorrect
    /// behavior). False negatives (missing a real object) would be wrong, so
    /// we err on the inclusive side.
    ///
    /// (gen r4/plumbing, 2026-09-23: this block used to sit on
    /// `set_jit_tlab_skip_regions`, ~170 lines up, so this function — "the
    /// VM's hottest validator" — rendered with no documentation at all.)
    #[inline]
    pub fn is_object_address(&self, addr: usize) -> Option<ObjectRef> {
        // SAFETY: `object_start` accepts only an address that passed the
        // region containment and header sanity checks (and the bitmap or the
        // extent proof), i.e. a valid object header inside a heap arena.
        self.object_start(addr)
            .map(|raw| unsafe { ObjectRef::from_raw(raw) })
    }

    /// [`Self::is_object_address`]'s yes/no, without building the
    /// `ObjectRef` (whose construction records its provenance and runs the
    /// single-OS-thread diagnostic, two thread-local accesses a guard that
    /// drops the value does not need). The same predicate: the header guards
    /// of `VmHeap` ask this (proposal
    /// `interpreter-L7-proposal-the-header-guards-validate-without-building-an-objectref-FIXED-20261013`,
    /// interpreter round i1 wave 49, lane L7).
    #[inline]
    pub fn is_object_start(&self, addr: usize) -> bool {
        self.object_start(addr).is_some()
    }

    /// The body of [`Self::is_object_address`]: the accepted address as a raw
    /// header pointer.
    pub fn object_start(&self, addr: usize) -> Option<*mut u8> {
        // Reject obvious garbage.
        if addr == 0 {
            return None;
        }
        // Object headers are 8-byte aligned (and HEADER_SIZE itself is a
        // multiple of 8). A pointer to a real object never has its low 3
        // bits set.
        if addr & 0x7 != 0 {
            return None;
        }
        let raw = addr as *const u8;

        // Region check: must fall inside one of the three arenas. LOCK-FREE —
        // read the cached `[base, end)` bounds (published under lock at
        // construction and at GC start/end; see `region_bounds`). The old
        // triple-mutex check (`young_from.lock() || young_to.lock() ||
        // old_gen.lock()`) ran per operand-stack object on every
        // object-returning native call and contended catastrophically with the
        // concurrent GC/allocator. The bounds can only change during a STW GC,
        // when no mutator is reading; the `Acquire` loads pair with the GC's
        // `Release` stores.
        //
        // Slot 1 is the INACTIVE semi-space. While the off-pause wipe is
        // zeroing it, its bytes are neither the objects they were nor yet
        // zero, so a candidate there is declined outright. No live object is
        // ever in the inactive semi-space, so this changes no valid answer.
        let skip_inactive = self.wipe_in_flight.load(Ordering::Acquire);
        let mut hit: Option<(usize, usize, usize)> = None;
        for (i, (base, end)) in self.region_bounds.iter().enumerate() {
            if skip_inactive && i == 1 {
                continue;
            }
            let b = base.load(Ordering::Acquire);
            let e = end.load(Ordering::Acquire);
            if addr >= b && addr < e {
                // Carry the END out too. The extent check further down used to
                // re-scan all three bounds pairs -- six more `Acquire` loads --
                // to find the very region this loop just identified.
                hit = Some((i, b, e));
                break;
            }
        }
        let Some((slot, _region_base, region_end)) = hit else {
            return None;
        };

        // COMMIT SCREEN. `region_bounds` covers RESERVED address space, and
        // the young arenas commit lazily in 2 MiB granules, so being inside
        // the envelope does NOT mean the bytes are mapped. Everything below
        // this line dereferences `raw`; a candidate in an unmapped granule has
        // to be declined here or the validator segfaults instead of answering.
        //
        // A whole header, not just its first byte: `HEADER_SIZE` is 16 and a
        // candidate eight bytes short of a granule boundary straddles two.
        //
        // Declining is always SAFE. The screen can only refuse an address in a
        // granule no allocator has touched, and an object cannot live in a
        // granule that was never committed — so no live object is ever lost,
        // and there is no false-negative root to worry about.
        if !self.region_range_committed(addr, HEADER_SIZE) {
            return None;
        }

        // r23 w4 review (R-L4z item 2): a young address inside a span a pinned
        // in-place cycle left unzeroed (`CRATONVM_GEN_IN_PLACE_LAZY_ZERO`) is a
        // dead payload, not an object, however plausible its bytes look -- an
        // `Object[]` value cell decodes as `class_id=4`, `num_slots=4`, and a
        // dead `long[]` can hold any header at all. Declined before a byte of
        // it is read. Exact (the mirror never covers a live object), so no
        // live root is lost; one Acquire load while no lazy cycle has run.
        if slot < 2 && self.young_lazy_dirty_at(slot, addr) {
            return None;
        }

        // Validate raw enum tags before constructing an `ObjectHeader`
        // reference. Conservative root scans can land on arbitrary arena words;
        // invalid `#[repr(u8)]` discriminants must be rejected as bytes, not
        // reached through a typed enum match.
        // SAFETY: `raw` was confirmed inside a mapped heap region above (`in_region`), so the single-byte
        // tag reads at the fixed `OBJECT_KIND`/`ARRAY_ELEMENT_TYPE` header offsets are in-bounds.
        let kind = unsafe { object_kind_from_tag(cratonvm_types::kind_tag_at(raw)) }?;
        if !unsafe { cratonvm_types::element_type_tag_ok_at(raw) } {
            return None;
        }

        // Object/Array are the only
        // valid kinds; anything else means we landed in the middle of a
        // field or in stale memory. HumongousFiller is a synthetic
        // walker-sentinel (round-9 gc CRIT-1) and never represents a
        // real object reachable from a root.
        if kind == ObjectKind::HumongousFiller {
            return None;
        }

        // SAFETY: The region check above confirmed `raw` is inside one of the
        // three arenas, and the enum tag bytes have been validated.
        let header = unsafe { &*(raw as *const ObjectHeader) };
        // Cheap structural sanity before trusting a conservative root
        // candidate as an object header. These invariants are written by every
        // allocator before publication; payload/interior words often satisfy
        // the loose kind/slot checks below but fail one of these bytes.
        if !header_reserved_fields_plausible(header) {
            return None;
        }
        // gen r5w5/pin9: a FORWARDED-looking candidate must forward into this
        // heap. Everything below that sizes the candidate (`num_slots`,
        // `array_length`, `object_instance_size`) resolves the shape word
        // through the forward, and the bitmap's early accept hands the caller a
        // reference it will size the same way; a forward out of the heap is
        // what faulted at `addr=0x100000004` (see `forward_chain_stays_inside`).
        // A real forward (a from-space object inside a moving pause) names
        // to-space, the in-place destination or old gen, all inside the
        // bounds and commit maps this cycle published, so no object is lost.
        //
        // gen r5w6/pin10: through `ObjectHeader::screen_shape`, which ALSO
        // refuses a mark that stays BUSY (the 262 144-round spin of
        // `docs/feature-designs/generative-gc-architecture.md`)
        // and a forward no collector publishes (null / self / misaligned /
        // beyond 2^47). And the shape it resolved is the one every size check
        // below uses, instead of a second `num_slots()` / `array_length()`:
        // bytes a mutator rewrites between the screen and a re-read (a
        // conservative word inside a live array) can no longer turn FORWARDED
        // after the screen passed them.
        let Some(screened_shape) =
            header.screen_shape(|t| self.region_range_committed(t, HEADER_SIZE))
        else {
            FORWARD_SCREEN_DECLINED.fetch_add(1, Ordering::Relaxed);
            return None;
        };

        // Merge note (gen round 5 wave 6, reconciling dev `ed3a63469`, which
        // fixed the same `0x100000004` crash by rejecting EVERY forwarded
        // candidate here): the screen above already refuses a forward that
        // leaves this heap, is null / self / misaligned / implausible, or stays
        // BUSY, which covers both of that commit's test shapes
        // (`is_object_address_rejects_candidate_with_forwarding_bits_without_faulting`).
        // A from-space object whose forward stays inside the heap is still
        // accepted, as before either fix: inside a moving pause a root that
        // names an object another root already evacuated must still resolve,
        // or it is left naming reset from-space
        // (`is_object_address_still_follows_a_forward_into_the_heap`).

        // EXACT ANSWER, once the bytes have been shown to be an object header.
        //
        // Everything BELOW this point exists to catch one thing: an address that
        // is not an allocation base but whose bytes decode as a plausible one.
        // The comment on the extent check names it -- an interior 16-byte
        // `Value` cell of an `Object[]` gives `class_id=4` and `num_slots=4`,
        // self-consistent and entirely coincidental -- and the way it is caught
        // is by computing the claimed extent and proving it fits the arena and
        // the commit map. That is the expensive half of this function.
        //
        // `Arena::is_object_start` settles the same question by construction: the
        // bit was set in `hand_out` and cleared in `add_free_block`, so a hit
        // means this arena really did hand `addr` out and has not taken it back.
        // No interior address can hold that bit, so there is nothing left for
        // the extent computation to catch.
        //
        // WHY IT IS HERE AND NOT BEFORE THE HEADER CHECKS, which is where it was
        // first written. `hand_out` is the door for every allocation, not every
        // OBJECT: a TLAB chunk is one hand-out, and its base carries the bit
        // whether or not an object was ever bump-allocated at it. Accepting on
        // the bit alone therefore hands a caller an `ObjectRef` to a chunk, whose
        // first bytes are not an `ObjectHeader` -- the exact corruption this
        // function exists to prevent, reintroduced by the thing meant to make it
        // exact. The kind, element-type and reserved-field checks above are cheap
        // byte tests and they are what rules that out; the extent arithmetic and
        // the second commit probe are what the bitmap replaces.
        //
        // ACCEPT ONLY. `Some(false)` and `None` fall through unchanged, because
        // the bitmap is knowably incomplete -- objects bump-allocated inside a
        // TLAB chunk never reach `hand_out` -- and rejecting on it would drop
        // live roots.
        let mut bitmap_declined = false;
        if let Some(hit) = self.arena_object_start(slot, addr) {
            if hit {
                OBJECT_START_HITS.inc();
                // `raw` is inside a mapped arena region (checked above), its
                // header decodes as a real object, and the bitmap confirms the
                // arena handed this exact address out as an allocation base.
                return Some(raw as *mut u8);
            }
            OBJECT_START_MISSES.inc();
            bitmap_declined = true;
        }

        // Cap num_slots at a sanity limit so a stale word can't fool us
        // into "validating" a slot count that would exceed the arena.
        // Multi-array reloc fix (2026-05-22): for arrays, `num_slots` is a
        // mirror of `array_length` (see `alloc_array` and `try_alloc_array`),
        // so a legitimate 256 MB int[] has num_slots = 2^26 > 1<<24 and
        // would be falsely rejected here. Bound num_slots only for non-arrays.
        // Use raw_shape() rather than num_slots() / array_length() so
        // candidate screening never follows forwarding pointers.
        const MAX_PLAUSIBLE_SLOTS: u32 = 1 << 24; // 16M slots -> 256 MB obj
        let is_array = kind == ObjectKind::Array;
        let is_compact = !is_array && is_compact_object(header);
        if is_array && header.gc_flags() & GC_FLAG_COMPACT != 0 {
            return None;
        }
        // gen r5w6/pin10: every count below is `screened_shape` (resolved once,
        // bounded, above) or, for a compact instance, its class's registered
        // field count -- never a second `num_slots()` / `array_length()`, which
        // would resolve the shape word again through whatever the bytes read
        // as NOW. The values are the ones those accessors return for a real
        // object: `num_slots` of a compact instance is `compact_field_count`,
        // of a legacy one the resolved shape; `array_length` of an array is
        // the resolved shape. (The former `!is_array && !is_compact &&
        // array_length() != 0` refusal is gone: `array_length()` of a
        // non-array is 0 by definition, so it never refused anything.)
        let class_id = header.class_id.as_u32();
        // Family-A fix (2026-07-03): also validate the object's EXTENT against
        // the arena it claims to live in — the same hardening `mark_young`
        // (sweep_young_non_moving) already applies, now centralized here so
        // every one of this function's ~28 callers gets it. Without this, a
        // conservative root candidate that lands NOT on a real object start
        // but on an interior 16-byte `Value` cell of a live `Object[]` (every
        // element's discriminant word is `VTAG_OBJECT = 4`, so `class_id=4`
        // decodes at the candidate offset AND `num_slots=4` decodes 16 bytes
        // later — a self-consistent-looking but entirely coincidental fake
        // header) passes every check above. Bit-plausible headers of this
        // shape are common: a hot proxy/reflection `invoke(Object,Object[])`
        // path keeps element-interior pointers alive in registers/stack slots
        // that a conservative scan (`scan_one_frame` et al.) then feeds
        // through this function as candidate roots. `mark_young` catches this
        // via its extent check and safely discards such a candidate WITHOUT
        // writing through it; but every OTHER caller of `is_object_address`
        // (root-set seeding for old-gen `major_gc`, the selective-promotion
        // pin set, `is_movable_jit_root`, cross-thread snapshot validation,
        // and more) had no such check and could WRITE through the accepted
        // "object" (mark bit, gc_age, forwarding_ptr) at a byte offset that
        // is actually the interior of an unrelated live array — corrupting
        // that array's real header/body bytes. This reproduced as the
        // long-standing "kind=Object but array_length=N; inline-alloc forgot
        // to set kind=Array" corruption family (Family-A / MiniThrottle /
        // the independently-found Hibernate-batch corruption): the "N" is
        // not random — it is bits of the flipped mark/age/forwarding write
        // landing inside the victim array's real `array_length` field.
        let claimed_extent = if is_array {
            // The JVM spec caps arrays at `Integer.MAX_VALUE` elements.
            if screened_shape > i32::MAX as u32 {
                return None;
            }
            match array_data_size(screened_shape as usize, header.element_type()) {
                Ok(data) => ARRAY_DATA_OFFSET.checked_add(data),
                Err(_) => None,
            }
        } else if is_compact {
            // Single `compact_total_size` lookup (a registry miss declines).
            cratonvm_types::field_layout::compact_total_size(class_id)
                .filter(|&s| s > 0)
                .map(|s| s as usize)
        } else {
            if screened_shape > MAX_PLAUSIBLE_SLOTS {
                return None;
            }
            Some(HEADER_SIZE + screened_shape as usize * SLOT_SIZE)
        };
        let Some(extent) = claimed_extent else {
            return None;
        };
        let Some(obj_end) = addr.checked_add(extent) else {
            return None;
        };
        // The object's full extent must fit within the SAME arena the region
        // check above matched (not just "some" arena — a header claiming to
        // span from young into old gen is exactly the interior-cell false
        // positive this guards against).
        //
        // "The same arena" is the arena the containment loop already found, so
        // ask it directly instead of re-scanning all three. The three arenas are
        // separate allocations and therefore disjoint, so the old `any` could
        // only ever have been satisfied by this same region — but the fallback
        // below keeps the answer bit-identical without resting on that, at the
        // cost of a scan that now runs only when the fast test fails, i.e.
        // essentially never on a real heap.
        //
        // This function is the VM's hottest validator: it is what every
        // conservative stack word, every ambiguous operand slot and every JIT
        // helper probe goes through, and the loop it replaces was six `Acquire`
        // loads on top of the six the containment check had already done.
        let extent_fits = obj_end <= region_end
            || self.region_bounds.iter().any(|(base, end)| {
                let b = base.load(Ordering::Acquire);
                let e = end.load(Ordering::Acquire);
                addr >= b && addr < e && obj_end <= e
            });
        if !extent_fits {
            return None;
        }
        // ...and the whole extent must be BACKED, not merely inside the arena.
        // Skip when `[addr, obj_end)` stays inside the same 2 MiB granule already
        // confirmed committed at the header screen above.
        if (addr ^ (obj_end - 1)) >= cratonvm_gc_core::reservation::GRANULE
            && !self.region_range_committed(addr, extent)
        {
            return None;
        }

        if bitmap_declined {
            // The bitmap had no bit for an address the deduction just certified
            // as a real object base. That is the recoverable half of the miss
            // population -- see `OBJECT_START_MISSED_OBJECTS`.
            OBJECT_START_MISSED_OBJECTS.inc();
        }
        // `raw` passed the region containment, header sanity, and
        // extent-containment checks above, so it points to a valid object
        // header within a heap arena.
        Some(raw as *mut u8)
    }

    /// DIAGNOSTIC-ONLY (cceres3): see `VmHeap::debug_forwarded_target`.
    pub fn debug_forwarded_target(&self, addr: usize) -> Option<usize> {
        if !cratonvm_gc_core::stale_objref_debug::enabled() || addr == 0 || addr % 8 != 0 {
            return None;
        }
        // PROBE FIX (cce0079 tree-key tail): `is_heap_addr` covers only the
        // LIVE arenas — but a stale (evacuated) address lives in the
        // quarantine RING, which is deliberately unpublished. Gating on
        // `is_heap_addr` alone made this probe (and both PIN canaries built
        // on it) structurally blind to exactly the addresses it exists to
        // catch. Also accept addresses inside any ring arena.
        let in_live = self.is_heap_addr(addr).is_some();
        if !in_live {
            let in_ring = self
                .quarantine
                .lock()
                .iter()
                .any(|a| a.contains(addr as *const u8));
            if !in_ring {
                return None;
            }
        }
        // SAFETY: containment in a mapped (live or quarantined) arena was
        // just confirmed; the quarantine ring keeps evacuated from-space
        // readable while the canary flag is on.
        let header = unsafe { &*(addr as *const ObjectHeader) };
        if header.is_forwarded() {
            let fwd = header.forwarding_address() as usize;
            if fwd != 0 {
                return Some(fwd);
            }
        }
        None
    }

    /// Loose validity check: alignment + region containment only.
    ///
    /// Unlike [`Self::is_object_address`] this does **not** read the object
    /// header. It is intended for GC root scanning of ambiguous slots (JVM
    /// long vs jobject smuggled as jlong) where the header may not yet be
    /// initialised, the pointer may target an interior offset, or the slot
    /// may transiently look like a pointer mid-construction.
    ///
    /// False positives (passing a non-object aligned heap-range address) are
    /// safe: the generational collector's `forward_object` already drops
    /// "suspected false roots" whose computed extent runs off the end of the
    /// from-space arena, so the worst case is over-retention rather than a
    /// deref of garbage.
    ///
    /// # Cost (gen r4w2/obs, 2026-09-23)
    ///
    /// A real root is ACCEPTED from the lock-free [`Self::region_bounds`]
    /// mirror, so the common case takes no lock. Until round 4 wave 2 every
    /// call took up to all three arena mutexes, and the callers are per-root:
    /// the root filters in `vm/src/memory/roots.rs` and the per-thread
    /// safepoint deposit, where N parking peers took and released the same
    /// `young_from` mutex once per root, contending with each other exactly at
    /// safepoint entry. See
    /// `docs/feature-designs/generative-gc-architecture.md`.
    ///
    /// Only an ACCEPT is taken from the mirror; a miss falls through to the
    /// locked test below, so a false negative is impossible whatever the
    /// mirror says. A false ACCEPT would need the mirror to be stale, and it is
    /// stale only between an arena swap/grow and the republish at the end of
    /// `collect_garbage_inner` — a window in which the collecting thread holds
    /// `young_from` (the guards are taken at cycle entry and held until after
    /// the republish). The fast path is therefore taken only while
    /// `young_from` is UNLOCKED; if it is held — by a collection, or by an
    /// allocator for a few instructions — this answers exactly as it always
    /// did, by waiting for the lock. `[base, end)` is the same span
    /// [`Arena::contains`] / `OldGen::contains` test (`base + data.len()`,
    /// published by `store_region_bounds_locked` from `capacity()`, which is
    /// `data.len()`).
    pub fn is_heap_addr(&self, addr: usize) -> Option<ObjectRef> {
        if addr == 0 || addr & 0x7 != 0 {
            return None;
        }
        // gen r4w2/obs: fast ACCEPT from the published mirror (see the doc).
        if !self.young_from.is_locked()
            && self.region_bounds.iter().any(|(base, end)| {
                let b = base.load(Ordering::Acquire);
                let e = end.load(Ordering::Acquire);
                b != 0 && addr >= b && addr < e
            })
        {
            // SAFETY: as for the locked arm below — alignment + containment in
            // a live arena's published span; nothing is dereferenced here.
            return Some(unsafe { ObjectRef::from_raw(addr as *mut u8) });
        }
        let raw = addr as *const u8;
        let in_region = self.lock_young_from().contains(raw)
            || self.young_to.lock().contains(raw)
            || self.old_gen.lock().contains(raw);
        if !in_region {
            return None;
        }
        // SAFETY: alignment + region containment guarantee a valid heap
        // pointer; constructing an ObjectRef from it is safe — only deref
        // through it is gated by downstream header sanity checks.
        Some(unsafe { ObjectRef::from_raw(raw as *mut u8) })
    }

    // ----- Field access ------------------------------------------------------

    /// Get the value of a field at the given index.
    pub fn get_field(&self, obj_ref: ObjectRef, index: usize) -> Value {
        // The read side of the receiver screen. A stale receiver is just as
        // wrong when it is read — the answer is whatever the pre-move copy
        // held, typically the JVM default, and nothing about the value read
        // says where it came from. Same predicate, same flag, same backtrace.
        if gc_flags().dbg_deadref_store {
            self.note_deadref_recv(
                "get_field",
                obj_ref.as_ptr() as usize,
                index,
                Value::Object(None),
            );
        }
        // KC16 SIGSEGV audit: runtime (not debug-only) bounds check. A
        // corrupted ObjectRef whose fake header has a huge num_slots would
        // happily pass the old debug_assert in release and then read
        // arbitrary memory.  For the suspect-header case we still panic
        // (that's a real bug).  For in-bounds-of-reality but out-of-bounds
        // of this object's layout (the common case when synthetic and
        // real-JDK class layouts disagree), return Object(None) — the
        // interpreter's primitive-field coercion will further normalize
        // it.  This converts what would be a silent SIGSEGV / panic into
        // a benign null read, matching HotSpot's behavior when an object
        // is accessed through a Reflection path that resolved a
        // larger-than-actual layout.
        let header = self.get_header(obj_ref);
        // An ARRAY receiver is never a plain-object field access, and neither
        // test below can see it: `alloc_array` MIRRORS the length into
        // `num_slots`, so every real element index is "in bounds" while the
        // byte offset this accessor computes names neither that element nor,
        // past the first few, any part of the allocation. See
        // `heap::refuse_array_receiver_field_access`.
        if header.kind() == ObjectKind::Array {
            // SAFETY: `obj_ref` is a live allocation base and `header` is its
            // header, resolved one line above.
            unsafe {
                cratonvm_gc_core::heap::refuse_array_receiver_field_access(
                    obj_ref.as_ptr() as usize,
                    header,
                    index,
                    "gen_heap::get_field",
                )
            };
            return Value::Object(None);
        }
        if is_compact_object(header) {
            if let Some((off, storage)) =
                cratonvm_types::compact_field_disp_storage(header.class_id.as_u32(), index)
            {
                let base = unsafe { obj_ref.as_ptr().add(off) };
                if !storage.is_reference() {
                    return unsafe {
                        cratonvm_types::read_compact_field(
                            base,
                            storage,
                            std::sync::atomic::Ordering::Relaxed,
                        )
                    };
                }
                let v = unsafe { read_prim_element(base, 0, ArrayElementType::Reference) };
                return cratonvm_gc_core::autobox::unbox_reference_field(
                    v,
                    header.class_id,
                    index,
                    |r| {
                        self.is_object_address(r.as_ptr() as usize)?;
                        // SAFETY: address validated as a live heap object.
                        let h = unsafe { &*(r.as_ptr() as *const ObjectHeader) };
                        Some(h.class_id)
                    },
                    |r| self.get_field(r, 0),
                );
            }
        }
        let num_slots = header.num_slots() as usize;
        if num_slots > (1 << 24) {
            tracing::debug!(
                target: "cratonvm::gc::guard",
                obj = ?obj_ref.as_ptr(),
                index,
                num_slots,
                class_id = ?header.class_id,
                kind_byte = ObjectHeader::kind_tag(header.mark_word.load(Ordering::Relaxed)),
                gc_flags = format!("{:x}", header.gc_flags()),
                "gen_heap::get_field: suspect header (returning null)",
            );
            // Rate-limited warn — do NOT panic. Returning null lets the
            // caller raise a normal Java exception / continue in a degraded
            // mode, matching how `array_length` is handled above. Without
            // this, Keycloak / Kafka tests abort the JVM on a corrupt
            // synthetic-receiver dispatch instead of surfacing a real
            // error to the Java side.
            return Value::Object(None);
        }
        if index >= num_slots {
            // Out-of-bounds field read. Resolve the class name + real
            // declared field count so the orchestrator can spot which of
            // two distinct bug classes this is:
            //
            //   (A) `real_field_count > num_slots` — TRUE undersized layout.
            //       The class declares more instance fields than the
            //       allocation site asked for, so even an own-class
            //       `getfield` can read past the object. This is the real
            //       allocator-side bug (the `PrintStream` 1-field-stub fix
            //       in commit cf1b478 was this case).
            //
            //   (B) `real_field_count <= num_slots` (typically equal) —
            //       CALLER-SIDE wrong-slot dispatch. The class layout is
            //       correct, but the caller used a slot index past the
            //       receiver's layout. The canonical pattern is a
            //       speculative collection-layout probe in
            //       `collect_collection_elements` / `read_java_string`
            //       reading slot 1 or 2 on a 1-slot object (e.g.
            //       `TypeList$Generic$Empty`, `RegularImmutableList`,
            //       `IdentityHashMap$Values`, `Collections$EmptyList`)
            //       or slot 0 on a 0-slot object (e.g. cglib's
            //       `MethodInterceptorGenerator`).
            //
            // Both cases return `Value::Object(None)` here so the caller
            // sees a benign null read instead of a SIGSEGV. The two cases
            // are distinguished by log level (error vs. warn) and message
            // text so the orchestrator's `grep undersized` continues to
            // flag (A) while (B) is triageable as a separate workstream.
            //
            // Rate-limit the diagnostics. `resolve_class_info` allocates a String
            // and the warn!/error! formats on EVERY OOB read, but the benign
            // case-(B) caller-side speculative probe (e.g. a collection-layout
            // probe landing on `Collections$EmptyMap`) fires thousands of times per
            // JUnit discovery — that unconditional per-call cost dominated kafka
            // `consumer.internals` discovery wall time. Cap the diagnostics to the
            // first N occurrences globally (a persistent case-(A) undersized-layout
            // bug surfaces well within N); past the cap the OOB read still returns a
            // benign null, just without the per-call logging. `CRATONVM_DBG_OOBFIELD`
            // forces full diagnostics regardless.
            static OOB_DIAG_COUNT: std::sync::atomic::AtomicU64 =
                std::sync::atomic::AtomicU64::new(0);
            const OOB_DIAG_CAP: u64 = 512;
            let oob_dbg = gc_flags().dbg_oobfield;
            if oob_dbg
                || OOB_DIAG_COUNT.fetch_add(1, std::sync::atomic::Ordering::Relaxed) < OOB_DIAG_CAP
            {
                let (class_name, real_fields) = match self.diag_class_info(header.class_id.as_u32())
                {
                    Some((name, n)) => (name, Some(n)),
                    None => ("<unresolved>".to_string(), None),
                };
                // CRATONVM_DBG_OOBFIELD=<substr>: dump a backtrace for OOB field
                // reads whose class name contains <substr>, to localize the reader.
                if let Some(want) = gc_flags().dbg_oobfield_value.as_deref() {
                    if !want.is_empty() && class_name.contains(want) {
                        eprintln!(
                            "[OOBFIELD_ASRTAG_V1 READ] class={} index={} num_slots={}\n{}",
                            class_name,
                            index,
                            num_slots,
                            std::backtrace::Backtrace::force_capture()
                        );
                    }
                }
                let is_true_undersized = real_fields.is_some_and(|n| n > num_slots);
                // A class that declares NO fields at all, probed at any index.
                //
                // Split out of the general arm below because that arm's triage
                // clause -- "a REAL JDK class whose `num_slots` equals
                // `real_field_count` is NOT benign" -- is VACUOUSLY true here
                // and reads as an accusation. The clause names an ALIASING
                // layout: two natives writing and reading different shapes into
                // one object. A zero-field class has no room for a second
                // layout, so there is no other writer to disagree with, and the
                // read can only be a caller-side speculative probe.
                //
                // MEASURED: `java/nio/Bits$1` (`real_field_count=Some(0)`,
                // `num_slots=0`) fired this once per boot from
                // `buffer_pool_get_name`, on runs that then passed 12/12, and
                // it was filed as the lead on an unrelated SIGSEGV minutes
                // later. That probe is fixed at its caller; this arm is so the
                // NEXT one is not read the same way.
                let declares_no_fields = num_slots == 0 && real_fields == Some(0);
                if is_true_undersized {
                    tracing::error!(
                        target: "cratonvm::gc::guard",
                        obj = ?obj_ref.as_ptr(),
                        index,
                        num_slots,
                        class_id = ?header.class_id,
                        class_name = %class_name,
                        real_field_count = ?real_fields,
                        "gen_heap::get_field: out-of-bounds field read dropped \
                         (undersized object layout — class declares more fields \
                         than the object was allocated with)",
                    );
                } else if declares_no_fields {
                    tracing::warn!(
                        target: "cratonvm::gc::guard",
                        obj = ?obj_ref.as_ptr(),
                        index,
                        num_slots,
                        class_id = ?header.class_id,
                        class_name = %class_name,
                        real_field_count = ?real_fields,
                        "gen_heap::get_field: out-of-bounds field read dropped \
                         (caller used slot index past receiver's layout). This \
                         class declares NO fields at all, so it cannot be \
                         carrying a second, aliasing layout, and the \
                         `num_slots` == `real_field_count` clause of the \
                         general warning does NOT apply to it: the read is a \
                         caller-side speculative probe against a receiver that \
                         does not match, and the null it gets back is the \
                         answer the caller's own fallback already expects. Fix \
                         it at the CALLER, by asking the receiver's class \
                         before reading a slot -- it is not evidence of data \
                         loss.",
                    );
                } else {
                    tracing::warn!(
                        target: "cratonvm::gc::guard",
                        obj = ?obj_ref.as_ptr(),
                        index,
                        num_slots,
                        class_id = ?header.class_id,
                        class_name = %class_name,
                        real_field_count = ?real_fields,
                        "gen_heap::get_field: out-of-bounds field read dropped \
                         (caller used slot index past receiver's layout — the \
                         class layout is correct, the caller's slot \
                         computation is not). Two causes, and the fields above \
                         tell them apart: if `class_name` is a fabricated or \
                         collection class this is usually a speculative \
                         collection-layout probe on a non-matching receiver, \
                         which is benign. If `class_name` is a REAL JDK class \
                         and `num_slots` equals `real_field_count`, it is not: \
                         a native is reading a synthetic layout that this \
                         class does not have, and the writer of that layout is \
                         a different native writing a different one. That is \
                         total, silent data loss — it emptied Tomcat's CGI \
                         response body. Set CRATONVM_DBG_LAYOUT_ALIAS=1 to \
                         list every class carrying a second layout, and \
                         intersect with these warnings.",
                    );
                }
                if gc_flags().dbg_oobfield_value.is_some() {
                    eprintln!(
                    "[OOBFIELD_ASRTAG_V1 READ] class={class_name} index={index} num_slots={num_slots}\n{}",
                    std::backtrace::Backtrace::force_capture()
                );
                }
                // A2 forensic breadcrumb (CRATONVM_DBG_A2): an OOB read on a
                // ClassId(0)/num_slots=0 receiver is the "zeroed live object"
                // face — dump the address's recorded alloc/free lifecycle so
                // the zeroing event is attributable (in-place clobber vs
                // reclaim-and-reuse vs never-allocated).
                if cratonvm_gc_core::a2dbg::enabled() && header.class_id.as_u32() == 0 {
                    let victim = obj_ref.as_ptr() as usize;
                    let hist = cratonvm_gc_core::a2dbg::history_at(victim, 12);
                    if hist.is_empty() {
                        eprintln!("[oob-field]   [A2] NO event touches {victim:#x}");
                    } else {
                        for r in hist {
                            if r.kind == 0xFF {
                                eprintln!("[oob-field]   [A2] seq={} FREE @{:#x}", r.seq, r.addr);
                            } else {
                                eprintln!(
                                    "[oob-field]   [A2] seq={} ALLOC @{:#x} class_id={} kind={} et={} alen={} ns={} size={}{}",
                                    r.seq, r.addr, r.class_id, r.kind, r.element_type,
                                    r.array_length, r.num_slots, r.size,
                                    if r.addr != victim { " (covering)" } else { "" },
                                );
                            }
                        }
                    }
                }
            } // end rate-limited OOB-read diagnostics
              // RESID-DIAG (dohead residuals investigation, 20260718): narrow,
              // unconditional backtrace for the zero-slot-receiver shape seen in
              // the known-issues residual logs — confirmed (2026-07-19) to
              // occur ONLY within the exact failing parameterization's test
              // case in every repro sample checked (zero occurrences in the
              // preceding passing cases of the same class), so this is NOT the
              // benign high-frequency case (B) the OOB_DIAG_CAP above guards
              // against — widened from the original index-4/5-only guess to
              // any index once indices 0/1/3 were also observed correlating.
            if num_slots == 0 {
                // RATE-LIMIT (2026-07-21): the WildFly real-JDK boot hits this
                // shape on EVERY framework log line (a delegating stream whose
                // field walk lands on a plain `Object`); an unconditional
                // symbolized `Backtrace::force_capture()` per occurrence turned
                // each boot into minutes of stderr spam. Keep the first few
                // full backtraces (they carry the diagnostic value) and a
                // count-only heartbeat afterwards.
                static RESID_READ_DIAG_COUNT: std::sync::atomic::AtomicU64 =
                    std::sync::atomic::AtomicU64::new(0);
                let n = RESID_READ_DIAG_COUNT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                if n < 5 || n.is_power_of_two() {
                    let diag_class_name = self
                        .diag_class_info(header.class_id.as_u32())
                        .map(|(n, _)| n)
                        .unwrap_or_else(|| "<unresolved>".to_string());
                    if n < 5 {
                        eprintln!(
                            "[RESID-DIAG READ] class={diag_class_name} index={index} num_slots={num_slots} obj={:p}\n{}",
                            obj_ref.as_ptr(),
                            std::backtrace::Backtrace::force_capture()
                        );
                    } else {
                        eprintln!(
                            "[RESID-DIAG READ] class={diag_class_name} index={index} num_slots={num_slots} obj={:p} (occurrence #{n}, backtrace suppressed)",
                            obj_ref.as_ptr(),
                        );
                    }
                }
            }
            return Value::Object(None);
        }
        // HIB-DCAST-LATEPHASE.1 (mutator side). A compact receiver whose layout
        // resolved above already returned; reaching here with `GC_FLAG_COMPACT`
        // means its `(class_id, index)` has no registered layout, which must NOT
        // fall through to the uniform 16-byte `Value` cell read below.
        //   (2) "this IS a compact object (`GC_FLAG_COMPACT`, set at
        //       allocation) but its `(class_id, num_slots)` no longer
        //       resolves to a registered layout" — its `with_class_layout`
        //       miss, e.g. a class redefinition racing the layout registry.
        //
        // Case (2) must NOT fall through. A compact object's body was sized
        // by `compact_object_body_size` (see `plan_object_alloc`), and its
        // `num_slots()` is the FIELD COUNT, not a legacy slot count — so the
        // `index >= num_slots` screen above does not bound
        // `HEADER_SIZE + index * SLOT_SIZE`, and an entirely in-range `index`
        // still strides past the allocation for `read_slot` to dereference.
        // That is the read half of the `SIGSEGV` observed against the real
        // `DefaultCatalogAndSchemaTest` workload, whose Hibernate/ByteBuddy
        // proxies churn redefinitions.
        //
        // Testing `is_compact_object(header)` — the per-object header bit,
        // independent of the registry — is what tells the two cases apart; it
        // is the same discriminator the GC walkers use, see the matching
        // notes in `for_each_ref_slot`/`forward_ref_slots` below. Those
        // walkers SKIP such an object ("no provably-safe reference slots to
        // visit ... rather than guessing"). An accessor cannot skip, so it
        // degrades the way this function's other unserviceable-access guards
        // already do (suspect header, out-of-bounds index): a benign null
        // read plus a loud `cratonvm::gc::guard` record. Deliberately not a
        // panic — those guards' own comments give the reason (a corrupt
        // receiver must let the Java side surface an error instead of
        // aborting the JVM) — and deliberately not a silent fall-through,
        // which is precisely what made the original `SIGSEGV` unattributable.
        //
        // TODO(types/src/field_layout.rs): the converse hazard is still open
        // and cannot be closed from this file. `compact_object_body_size`
        // refuses a FOREIGN `layout_domain`'s entry at allocation time, but
        // the read path (`with_class_layout` / `compact_object_field_storage`)
        // performs no domain check at all — so a same-numbered `class_id`
        // registered by a different `ClassStore` resolves here and is read as
        // if it described this class. Those two functions need the
        // `layout_owner(class_id) != domain` screen that
        // `compact_object_body_size` already applies.
        if is_compact_object(header) {
            tracing::warn!(
                target: "cratonvm::gc::guard",
                obj = ?obj_ref.as_ptr(),
                index,
                num_slots,
                class_id = ?header.class_id,
                "gen_heap::get_field: compact receiver has no registered layout \
                 for its (class_id, field_count) — returning null rather than \
                 striding its packed compact body as legacy 16-byte cells \
                 (HIB-DCAST-LATEPHASE.1)",
            );
            return Value::Object(None);
        }
        // SAFETY: `obj_ref` points to a valid heap object and `index` is within
        // `num_slots` (checked above). `slot_ptr` computes
        // `obj_ref + HEADER_SIZE + index * SLOT_SIZE`, which is within the
        // object's allocated region. `read_slot` reads a `Value` from that address.
        unsafe {
            let ptr = slot_ptr(obj_ref, index);
            // gcstress residual face-1 diagnostics (CRATONVM_DBG_CELLCORRUPT):
            // identify the HOLDER of a corrupt Value cell before `read_slot`
            // masks it with a benign null. The holder's class/kind/layout — and
            // whether the stale raw0 pointer lands in current young-from,
            // young-to, old gen, or nowhere — discriminates the candidate
            // mechanisms (8-vs-16-byte slot-layout confusion vs un-remapped
            // holder after a moving young GC vs stale freed arena after grow).
            if cell_corrupt_diag_enabled()
                && cratonvm_types::read_value_checked(ptr as *const Value).is_none()
            {
                self.dump_corrupt_cell_holder(obj_ref, header, index, ptr);
            }
            read_slot(ptr)
        }
    }

    /// Set the value of a field at the given index.
    ///
    /// Automatically fires the write barrier for generational GC correctness.
    /// Callers do NOT need to call `write_barrier` separately.
    pub fn set_field(&self, obj_ref: ObjectRef, index: usize, value: Value) {
        // gcf f2/p (`gcf-f1p-set-field-pays-per-store-diagnostic-gates`): the
        // three value/receiver diagnostics below used to read the flag snapshot
        // once each (`gc_flags()` twice and `stale_objref_debug::enabled()`),
        // on every interpreter `putfield`, every native field store and the
        // tail of `jit_putfield_object`. One read, one predicted branch; the
        // bodies are unchanged and live in a cold function. Not latched per
        // heap: tests arm `CRATONVM_DBG_DEADREF_STORE` through
        // `with_thread_overrides` AFTER the heap is built.
        let f = gc_flags();
        if f.dbg_badref || f.dbg_deadref_store || f.dbg_stale_objref {
            self.set_field_value_diagnostics(obj_ref, index, value);
        }
        let header = self.get_header(obj_ref);
        // gcf f2/p (`gcf-f1p-jit-putfield-object-resolves-the-compact-layout-up-to-four-times`):
        // a COMPACT receiver's layout is resolved once. `compact_field_slot`
        // answers "is `index` a field of this layout" and "where" together
        // (`field_disp(index)?` is `None` past the layout's field count, and the
        // layout is looked up BY that count), so the `num_slots()` bounds test
        // below -- itself a layout-count lookup for a compact header -- adds
        // nothing for it. A `None` (no registered layout, or an index past the
        // count) takes the full path below, which reports both exactly as
        // before. Arrays never take this arm (`refuse_array_receiver_field_access`
        // below stays their only door).
        if is_compact_object(header) && header.kind() != ObjectKind::Array {
            if let Some((off, storage)) = compact_field_slot(header, index) {
                self.set_compact_field_resolved(obj_ref, header, index, off, storage, value);
                return;
            }
        }
        self.set_field_full(obj_ref, header, index, value);
    }

    /// gcf f2/p: [`Self::set_field`]'s three per-store diagnostics, moved out
    /// of line unchanged (the bodies are the pre-f2 ones). Reached only when
    /// one of `CRATONVM_DBG_BADREF`, `CRATONVM_DBG_DEADREF_STORE`,
    /// `CRATONVM_DBG_STALE_OBJREF` is armed; each re-tests its own gate.
    #[cold]
    #[inline(never)]
    pub fn set_field_value_diagnostics(&self, obj_ref: ObjectRef, index: usize, value: Value) {
        // DIAGNOSTIC (bc math-ec): catch a fake `0x4`-style reference (a
        // collision-long payload) persisted into a heap field via ANY caller of
        // the heap set API (bytecode putfield, native ctx.set_field, reflection).
        if let Value::Object(Some(p)) = value {
            let a = p.as_ptr() as usize;
            if a != 0 && a < 0x1_0000 && gc_flags().dbg_badref {
                eprintln!(
                    "[BADREF:set_field] recv_class_id={} idx={} ptr=0x{:x}",
                    self.get_header(obj_ref).class_id.as_u32(),
                    index,
                    a,
                );
            }
        }
        if gc_flags().dbg_deadref_store {
            // The RECEIVER, before the value. `note_deadref_store` asks whether
            // the value being stored names a live object; it never asked the
            // same question about the object being stored INTO, and those are
            // two different defects with the same cause. A native that holds a
            // receiver across an allocation writes a perfectly live value into
            // a dead address: `[deadref-store]` sees nothing wrong, the write
            // lands in the pre-move copy, and the surviving object keeps the
            // field's JVM default. That is silent in every arm of the
            // `CRATONVM_DBG_DEADREF_STORE` family — which is how
            // `native_assertj_lightweight_comparable_assert`'s `objects` store
            // survived the sweep that fixed ten of its siblings.
            self.note_deadref_recv("set_field", obj_ref.as_ptr() as usize, index, value);
            if let Value::Object(Some(p)) = value {
                self.note_deadref_store(
                    "set_field",
                    obj_ref.as_ptr() as usize,
                    index,
                    p.as_ptr() as usize,
                );
            }
        }
        // CRATONVM_DBG_STALE_OBJREF (cce0079): a stale VALUE stored into a
        // reference field is otherwise silent (only the receiver's header is
        // read below) — surface the store site itself while the quarantine
        // holds the forwarding marker (producer-side attribution; matches
        // the equivalent check in `set_array_element`).
        if cratonvm_gc_core::stale_objref_debug::enabled() {
            if let Value::Object(Some(v)) = value {
                // Operand attribution (cce0079 tree-key tail): peek the
                // header word first so the imminent canary panic can be
                // attributed to the STORED VALUE (vs the receiver, whose
                // own get_header follows below).
                // SAFETY: `v` is a non-null `ObjectRef` (matched `Object(Some(v))`), so reading its header word is
                // sound; diagnostic-only (stale-objref debug) path.
                let peek = unsafe { &*(v.as_ptr() as *const ObjectHeader) };
                if peek.is_forwarded() {
                    eprintln!(
                        "[storechk] set_field: STALE stored VALUE 0x{:x} \
                         (receiver 0x{:x} slot {index}) thread={} cycle={} — canary panic follows",
                        v.as_ptr() as usize,
                        obj_ref.as_ptr() as usize,
                        std::thread::current().name().unwrap_or("?"),
                        self.stats.minor_gc_count.load(Ordering::Relaxed),
                    );
                }
                let _ = self.get_header(v);
            }
        }
    }

    /// gcf f2/p: the store diagnostics that need the RESOLVED receiver
    /// (`CRATONVM_DBG_FIELD_WATCH`, `CRATONVM_DBG_OBJ_WATCH`), shared by
    /// [`Self::set_field`]'s compact arm and [`Self::set_field_full`] so both
    /// report exactly what the single pre-f2 path did. Each is a cached gate.
    #[inline]
    pub fn note_field_store_watches(
        &self,
        header: &ObjectHeader,
        obj_ref: ObjectRef,
        index: usize,
        value: Value,
    ) {
        // FIELD-WATCH (TestUpgrade RootReference/MVMap residual, software
        // watchpoint — see cratonvm_types::field_watch and
        // bug-h2-suite-residual-fail-triage-FIXED.md).
        // Zero cost unless CRATONVM_DBG_FIELD_WATCH is set AND obj_ref was
        // explicitly registered via field_watch::watch() at construction.
        // Every write is reported (not deduped) — a count reaching 2 for a
        // `final` field is the direct signal we're chasing.
        if cratonvm_types::field_watch::is_watched(obj_ref) {
            let count = cratonvm_types::field_watch::record_write(obj_ref, index);
            let old = self.get_field(obj_ref, index);
            let (class_name, _) = self
                .diag_class_info(header.class_id.as_u32())
                .unwrap_or_else(|| ("<unresolved>".to_string(), 0));
            eprintln!(
                "[FIELD-WATCH] obj={:p} class={} index={} write_count={} old={:?} new={:?} thread={}",
                obj_ref.as_ptr(),
                class_name,
                index,
                count,
                old,
                value,
                std::thread::current().name().unwrap_or("?"),
            );
        }
        note_obj_watch_store(header, obj_ref, index, value);
    }

    /// gcf f2/p: the store into a COMPACT receiver whose slot the caller has
    /// already resolved (`off`, `storage` = `compact_field_slot(header, index)`),
    /// the body of [`Self::set_field`]'s former compact arm, unchanged. The
    /// reference arm boxes a non-reference value (a type-punning native) and
    /// fires the generational card barrier; a primitive arm stores the
    /// tagless payload.
    #[inline]
    /// May a batch whose compact plan `VmHeap::try_set_compact_fields`
    /// validated store its REFERENCE slots through
    /// [`Self::set_resolved_compact_reference_field`]? While no SATB window
    /// is open: the native store's pre-barrier (`publish_overwritten_field`,
    /// which this heap needs at the call site) then publishes nothing. The
    /// stack-walker residual-costs page, item 2.
    #[inline]
    pub fn compact_reference_batch_allowed(&self) -> bool {
        !self.satb_queue_ref().is_some_and(|q| q.is_active())
    }

    /// [`Self::set_field`]'s compact arm for a reference slot VmHeap's batch
    /// plan resolved: [`Self::set_compact_field_resolved`] itself (its
    /// watches, the store and the card barrier).
    pub fn set_resolved_compact_reference_field(
        &self,
        obj_ref: ObjectRef,
        index: usize,
        off: usize,
        storage: FieldStorageKind,
        value: Value,
    ) {
        let header = self.get_header(obj_ref);
        self.set_compact_field_resolved(obj_ref, header, index, off, storage, value);
    }

    pub fn set_compact_field_resolved(
        &self,
        obj_ref: ObjectRef,
        header: &ObjectHeader,
        index: usize,
        off: usize,
        storage: FieldStorageKind,
        value: Value,
    ) {
        debug_assert_eq!(compact_field_slot(header, index), Some((off, storage)));
        self.note_field_store_watches(header, obj_ref, index, value);
        if storage.is_reference() {
            // A non-reference value written into a reference slot (a
            // type-punning native, or a typeless `Unsafe.put*`): box it
            // into a 1-field `AUTOBOX_CLASS_ID` wrapper, exactly as
            // compact reference *arrays* do. Shared with `zgc`, `g1` and
            // `heap` through `cratonvm_gc_core::autobox` since 2026-08-12 — those
            // three used to drop the write to null instead, so which
            // answer a program got depended on the collector flag
            // (W7-84-primitive-in-reference-store.md).
            let value =
                cratonvm_gc_core::autobox::box_for_reference_slot(value, header.class_id, index, |v| {
                    let wrapper = self.alloc_object(AUTOBOX_CLASS_ID, 1);
                    self.set_field(wrapper, 0, v);
                    wrapper
                });
            // Compute the slot pointer AFTER the boxing closure: it may have
            // allocated, and this is a copying heap. `obj_ref` itself is the
            // caller's handle and is out of our hands either way, but the slot
            // address is the one thing this function can honestly refresh.
            //
            // SAFETY: `off` is the in-bounds reference slot `compact_field_slot`
            // resolved for this receiver's layout (an index past the layout's
            // field count resolves to `None` and never reaches here); writing a
            // reference element there is sound. `value` is a reference by
            // construction — `box_for_reference_slot` returns either the
            // caller's `Value::Object(_)` or a fresh wrapper.
            let base = unsafe { obj_ref.as_ptr().add(off) };
            // The compact reference store was the one heap write primitive
            // `CRATONVM_DBG_WATCH_CELL` could not see. Every other writer
            // — `write_slot`, the two evacuation copies, the three
            // `forward_ref_slots` arms — reports; this one did not, so a
            // watch aimed at a compact object's reference field answered
            // "nobody wrote it" for a cell that had in fact been written.
            cratonvm_gc_core::heap::cell_watch_check(
                base as usize,
                ref_field_size(),
                "set_field-compact-ref",
                &value,
            );
            unsafe { write_prim_element(base, 0, ArrayElementType::Reference, value) };
            self.write_barrier(obj_ref, value);
        } else {
            // SAFETY: `off` is within the body (as above); the class layout
            // guarantees natural alignment.
            let base = unsafe { obj_ref.as_ptr().add(off) };
            // SAFETY: class layout guarantees natural alignment and bounds.
            unsafe {
                cratonvm_types::write_compact_field(
                    base,
                    storage,
                    value,
                    std::sync::atomic::Ordering::Relaxed,
                )
            };
        }
    }

    /// gcf f2/p: the JIT helper's door into [`Self::set_compact_field_resolved`]
    /// for a REFERENCE value into a compact reference field whose slot
    /// `jit_putfield_object` has already resolved (`vm/src/jit/helpers.rs`,
    /// `jit_compact_field_slot`). Saves the helper's tail the second and third
    /// layout resolution (`num_slots()` and `compact_field_slot` again) that a
    /// plain [`Self::set_field`] pays. Anything the resolved arm does not
    /// cover -- the value diagnostics armed, an array or legacy receiver --
    /// takes [`Self::set_field`] itself, so the behaviour is set_field's on
    /// every input.
    #[inline]
    pub fn set_resolved_compact_ref_field(
        &self,
        obj_ref: ObjectRef,
        index: usize,
        off: usize,
        value: Value,
    ) {
        let f = gc_flags();
        if f.dbg_badref || f.dbg_deadref_store || f.dbg_stale_objref {
            self.set_field(obj_ref, index, value);
            return;
        }
        let header = self.get_header(obj_ref);
        if !is_compact_object(header) || header.kind() == ObjectKind::Array {
            self.set_field(obj_ref, index, value);
            return;
        }
        self.set_compact_field_resolved(
            obj_ref,
            header,
            index,
            off,
            FieldStorageKind::Reference,
            value,
        );
    }

}
