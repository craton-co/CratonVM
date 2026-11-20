#[allow(unused_imports)]
use super::*;

impl G1Collector {
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
        // G1's primitive set_field arm has no SATB/RSet barrier; reference
        // slots never reach this door. The batch validated bounds and layout.
        // SAFETY: the caller supplied the validated, naturally aligned slot.
        unsafe {
            cratonvm_types::write_compact_field(
                obj.as_ptr().add(off),
                storage,
                value,
                Ordering::Relaxed,
            )
        };
    }

    /// May a batch whose compact plan `VmHeap::try_set_compact_fields`
    /// validated, on an object of `total_size` bytes, store its REFERENCE
    /// slots through [`Self::set_resolved_compact_reference_field`]? Exactly
    /// when `set_field` would take its lock-free arm with no SATB read for each
    /// of them: no marking window open and the object not possibly humongous
    /// (the terms [`Self::raw_ref_array_copy_admitted`] states for arrays).
    /// The stack-walker residual-costs page, item 2.
    #[inline]
    pub fn compact_reference_batch_allowed(&self, total_size: usize) -> bool {
        !self.satb_pre_barrier_required() && !self.may_be_humongous(total_size)
    }

    /// `set_field`'s compact reference store under
    /// [`Self::compact_reference_batch_allowed`]: the flat write, then the
    /// remembered-set post-barrier for a non-null reference.
    ///
    /// # Safety
    /// off/storage must describe this receiver's in-bounds reference slot.
    #[inline]
    pub unsafe fn set_resolved_compact_reference_field(
        &self,
        obj: ObjectRef,
        off: usize,
        storage: cratonvm_types::FieldStorageKind,
        value: Value,
    ) {
        debug_assert!(storage.is_reference() && matches!(value, Value::Object(_)));
        // SAFETY: the caller supplied the validated, naturally aligned slot.
        unsafe {
            cratonvm_types::write_compact_field(
                obj.as_ptr().add(off),
                storage,
                value,
                Ordering::Relaxed,
            )
        };
        if let Value::Object(Some(r)) = value {
            self.post_write_barrier_rset(obj, r);
        }
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

    /// Create a new G1 collector with the given configuration.
    pub fn new(config: G1CollectorConfig) -> Self {
        Self::new_with_heap_resize(config, g1_heap_resize_enabled())
    }

    /// [`Self::new`] with the adaptive sizing policy's SEED arm as a
    /// parameter (g1sz): `resize` decides whether the soft capacity and the
    /// young target start at `-Xms` (the policy's arm, HotSpot's
    /// `InitialHeapSize`) or at the `-Xmx` grid. `CRATONVM_G1_HEAP_RESIZE` is
    /// latched once per process, so a test that means one arm says which;
    /// the per-pause decisions take their own arm
    /// ([`Self::heap_target_regions_within`]).
    pub fn new_with_heap_resize(config: G1CollectorConfig, resize: bool) -> Self {
        // F-09 — the region size is rounded to a power of two HERE, before
        // anything derives from it, so `config.region_size`, `num_regions`, the
        // arena length, every region base and `region_shift` all describe one
        // geometry. Rounding it later (or only for the shift) would give the
        // collector two answers to "which region owns this address".
        let mut config = config;
        config.region_size = normalize_region_size(config.region_size);
        let region_shift = config.region_size.trailing_zeros();
        let num_regions = config.heap_size / config.region_size;
        assert!(
            num_regions > 0,
            "g1: heap must fit at least one region (heap_size={}, region_size={})",
            config.heap_size,
            config.region_size
        );

        // One contiguous arena carved into `num_regions` adjacent slices, so
        // region `i+1` physically follows region `i`. This is what makes a
        // humongous object spanning a contiguous run of region indices one
        // contiguous block (see the `arena` field doc and
        // `claim_humongous_span_locked`).
        // Allocated once and never moved/reallocated → every region base and
        // the `region_lookup` table are address-stable for the collector's life.
        // Fallible, so an -Xmx the OS will not reserve is reported as a heap
        // reservation failure rather than as `handle_alloc_error`'s bare
        // `memory allocation of N bytes failed` — see `alloc_zeroed_heap`.
        // F-16 — RESERVE the whole heap, COMMIT a prefix of it.
        //
        // `-Xmx` is address space from here on. The committed prefix starts at
        // `-Xms` (or the ergonomic when it is unset) and grows when a region is
        // claimed above it; see `commit_through_region`. On a platform without
        // a reservation implementation, or if the OS refuses one,
        // `ReservedHeap` falls back to exactly the previous fully-committed
        // allocation and the rest of the collector cannot tell.
        //
        // `CRATONVM_G1_RESERVE_HEAP=0` asks for that fallback deliberately:
        // every byte committed up front, which is where every G1 result before
        // F-16 was produced.
        let reserved = num_regions * config.region_size;
        let initial_commit = if config.initial_heap_size > 0 {
            config.initial_heap_size.min(reserved)
        } else {
            // A sixteenth of the heap, floored at four regions. Small enough
            // that a short-lived process pays almost nothing for a large
            // `-Xmx`, large enough that a normal startup does not spend its
            // first hundred allocations growing.
            (reserved / 16)
                .max(config.region_size.saturating_mul(4))
                .min(reserved)
        };
        let arena = cratonvm_gc_core::heap_reservation::ReservedHeap::new_with_thp(
            reserved,
            initial_commit,
            "G1 region array",
            false,
        );
        let arena_base = arena.base();
        let arena_end = arena_base + arena.reserved_len();

        // F-05 — one card table for the whole arena, shared by the collector
        // (which dirties from the write barrier) and by every region (which
        // clears its own range on `reset`). Allocated before the regions
        // because each region captures a clone of the `Arc`.
        // F-16: the RESERVATION, not the committed prefix. A region anywhere in
        // the reservation can be committed later, and the card table has to
        // have a byte for it before that happens — it is metadata beside the
        // heap, not inside it, so covering the whole span costs one byte per
        // 512 and buys never having to resize it.
        let cards = std::sync::Arc::new(crate::g1_cards::G1CardTable::new_with_block_offsets(
            arena_base,
            arena.reserved_len(),
            crate::g1::lane_a_block_offsets(),
        ));

        let regions: Vec<G1Region> = (0..num_regions)
            .map(|i| {
                G1Region::from_arena(
                    arena_base + i * config.region_size,
                    config.region_size,
                    std::sync::Arc::clone(&cards),
                )
            })
            .collect();

        // Build the address-to-region lookup table (sorted by base addr).
        // Bases are `arena_base + i*region_size` — already ascending; the sort
        // is retained for robustness and parity with the previous construction.
        // The arena is never reallocated so these addresses are stable for the
        // lifetime of the collector (see field doc).
        let mut region_lookup: Vec<(usize, usize)> = regions
            .iter()
            .enumerate()
            .map(|(i, r)| (r.data.as_ptr() as usize, i))
            .collect();
        region_lookup.sort_unstable_by_key(|(base, _)| *base);

        // Round-2 fix (HIGH — GC #5): no global mark bitmap any more —
        // bitmaps live per-region (see `G1Region::mark_bitmap`) so they
        // correctly cover the real heap addresses of each region's data
        // buffer. The previous global `MarkBitmap::new(0, heap_size)`
        // silently rejected every real address.

        let ihop_threshold = (config.heap_size as u64 * config.ihop_percent as u64 / 100) as usize;

        // Publish the READ-side bounds the JIT's guarded inline `getfield`
        // tests against. One pair covers the whole collector: G1's N regions
        // are carved out of this single `Box` (`arena_base + i*region_size`),
        // which `alloc_zeroed_heap` allocates once here and never moves or
        // resizes, so `[arena_base, arena_end)` is immutable for the
        // collector's lifetime and needs no refresh at GC boundaries.
        //
        // Deliberately NOT `JIT_REGION_BOUNDS`: that table is what G1-2
        // (`g1-audit.md` §8.1) keeps EMPTY under G1 so no inline
        // reference-STORE fast path is reachable and a JNI-pinned,
        // CSet-excluded region cannot lose its remembered-set edge. Publishing
        // reads here leaves that gate exactly as it was — see
        // `gen_heap::JIT_READ_BOUNDS` for why the two questions need two
        // tables.
        //
        // Sound for reference fields as well as primitives, which is what
        // makes it worth doing: a G1 reference field is a plain pointer (no
        // colored words, no load barrier), unlike ZGC's, which is why ZGC does
        // not publish here.
        // F-16: the COMMITTED prefix, not the whole reservation. This table
        // asserts "a raw load anywhere in this range cannot fault", and past
        // the committed prefix that is exactly false. `commit_through_region`
        // republishes it as the prefix grows.
        cratonvm_gc_core::jit_bounds::publish_jit_read_bounds(0, arena_base, arena_base + arena.committed_len());

        // The arena bounds, for the free functions that have no `&self`. See
        // [`G1_ARENA_BASE`]: the corrupt-cell report used
        // `plausible_heap_pointer`, which is a much looser question than the
        // one the REFUSING screens ask, and the two disagree on exactly the
        // input this family's central statistic was taken over.
        G1_ARENA_BASE.store(arena_base, Ordering::Relaxed);
        G1_ARENA_END.store(arena_end, Ordering::Relaxed);
        if gc_flags().g1_ref_write_watch {
            let _ = REF_WRITE_WATCH.set(RefWriteWatch {
                writes: Mutex::new(std::collections::HashMap::new()),
                out_of_body: AtomicUsize::new(0),
            });
        }

        // Precise root coverage — publish what this collector's relocating
        // phase MAY MOVE, which is the question the frame-band verifier asks
        // and which G1 was the only relocating collector not answering.
        //
        // `conservative_roots`'s verifier decides "does this compiled frame's
        // spill band hold a young-heap address the shadow stack never
        // published?" by classifying each band word with
        // `gen_heap::addr_is_movable` — the union of `JIT_REGION_BOUNDS` (which
        // G1 must keep empty, or defect G1-2 re-opens) and `MOVABLE_BOUNDS`
        // (which exists precisely so a collector can answer this WITHOUT
        // re-enabling the inline reference-store fast path). ZGC has published
        // its envelope there since 2026-08-21; G1 published neither, so
        // `movable_bounds_are_live()` was false, the verifier failed closed on
        // `YOUNG_BOUNDS_UNPUBLISHED` before inspecting a single frame, and
        // every G1 pause reported `root coverage: incomplete` — 100.00% of
        // them, measured. That verdict is what forces the conservative scan,
        // which fills the pin set, which is what pins regions out of every
        // collection set.
        //
        // The WHOLE reservation, not the committed prefix and not the young
        // regions: a superset is the safe direction, exactly as ZGC's comment
        // argues. An address wrongly called movable costs a declined
        // suppression; an address wrongly called immovable is a frame reported
        // clean that was never inspected. The envelope also never changes,
        // where the committed prefix grows under `commit_through_region` and
        // the young set changes every pause.
        // The COMMITTED prefix, not the whole reservation — and republished as
        // the prefix grows, exactly like `publish_jit_read_bounds` above.
        //
        // §14 published the reservation, arguing a superset is the safe
        // direction. It is, for the classification this feeds: an address
        // wrongly called immovable is a frame reported clean that was never
        // inspected. But the committed prefix is ALSO a superset of what can
        // hold an object — objects exist only in committed regions — so it is
        // the same safe direction, tighter, and it buys two things the
        // reservation does not.
        //
        // First, precision. `band_has_unpublished_word_with_map` classifies a
        // stack word as an unpublished oop on `addr_is_movable(w)` alone, so
        // every extra byte of range is extra stack words called oops by
        // coincidence. Reserved-but-uncommitted address space cannot hold an
        // object at all, so including it can only add false positives.
        //
        // Second, and the reason the range is not merely cosmetic: it is the
        // one thing standing between that test and a header check. The test
        // has no `is_object_address` screen — unlike every sibling instrument
        // in `conservative_roots` — and cannot get one while the range covers
        // pages that are not mapped, because reading a header there faults.
        // Bounding it by the commit makes such a screen safe to add.
        cratonvm_gc_core::jit_bounds::publish_movable_bounds(0, arena_base, arena_base + arena.committed_len());

        // The capability-free geometry table -- see `heap_geometry`. This is
        // the one table in the family G1 fills WITHOUT a caveat, because it
        // grants nothing: `JIT_REGION_BOUNDS` must stay empty here (defect G1-2,
        // inline reference stores), `JIT_READ_BOUNDS` is bounded by the commit
        // (a raw load must not reach an unmapped page) and `MOVABLE_BOUNDS`
        // likewise. None of that constrains "where did this heap reserve its
        // address space", which is the only thing this one says.
        //
        // The RESERVATION, not the committed prefix, and that is the difference
        // from the two publishes above. A narrow-oop window has to cover every
        // address the heap can ever produce; deriving it from a prefix that
        // grows would invalidate every reference already encoded against it.
        cratonvm_gc_core::heap_geometry::publish_heap_span(0, arena_base, arena_base + arena.reserved_len());

        // JIT round 21, lane vhcae (VHCAE21-1): the compiled post barrier's
        // edge filter, per collector (boxed: its address is what the VM hands
        // to the JIT, and it must survive `Self` moving).
        let jit_edge_filter = G1JitEdgeFilter::new(arena_base, num_regions, region_shift, &cards);

        // JIT round 23 wave 3, lane L4: the inline young-holder filter's table
        // (`JIT_G1_YOUNG`), only while the young-source prerequisite and filter
        // are on (`young_source_filter_on`, which also needs the cleanup
        // scrub). `regions` is the vector `Self` takes below; its buffer does
        // not move with it and is never resized.
        //
        // r23 wave 3 review: BEFORE `publish_jit_g1_barrier`, not after. The
        // compiled sequence indexes THIS table with an offset taken from the
        // barrier table's base. Both tables are one-owner-per-process: when
        // another G1 collector already owns them, both publishes below poison
        // instead of overwriting (`JIT_G1_YOUNG` shift 0; `JIT_G1_BARRIER`
        // base 0, mask `usize::MAX`), and the young poison comes first so no
        // reader pairs a live young table with a changed barrier base.
        if g1_young_source_filter_enabled() {
            cratonvm_gc_core::jit_bounds::publish_jit_g1_young_with_layout(
                regions.as_ptr() as usize,
                arena_base,
                region_shift,
                regions.len(),
                crate::g1::G1_REGION_STRIDE,
                crate::g1::G1_REGION_TYPE_OFFSET,
                crate::g1::G1_REGION_YOUNG_MAX,
            );
        }

        // F-08 - publish G1's geometry for the JIT's inline post-write barrier.
        //
        // A SEPARATE table from `JIT_REGION_BOUNDS`, whose emptiness under G1
        // closes defect G1-2 and must stay empty, and from `JIT_READ_BOUNDS`
        // just above, which answers the read-side "is this address mapped"
        // question. Three tables for three questions; see `JitG1BarrierTable`
        // for the argument and for why the mask is applied to `addr -
        // arena_base` rather than to `addr`.
        //
        // `region_size` is a power of two (`normalize_region_size`), so
        // `!(region_size - 1)` is the mask that isolates the region number.
        cratonvm_gc_core::jit_bounds::publish_jit_g1_barrier(
            arena_base,
            // F-16: the reservation. This word is the table's "is it published"
            // predicate rather than a bound anything loads through — the
            // emitted sequence reads only the base and the region mask — so it
            // describes the address range the geometry is valid for, which is
            // the whole reservation whatever fraction of it is committed.
            arena.reserved_len(),
            !(config.region_size - 1),
            cards.cards_base_addr(),
            crate::g1_cards::G1_CARD_SHIFT,
        );

        // gci i45/gy (`gci-i43gy-g1-young-bounds-use-max-regions-not-committed-RETIRED-UNFIXED-20261018`):
        // the young target starts at the default ceiling of the heap's
        // COMMITTED regions (HotSpot's `G1Policy::init` sizes the young list
        // from `num_committed_regions()`, i.e. `InitialHeapSize`), which is
        // the soft capacity's starting value. Without `CRATONVM_G1_HEAP_RESIZE`
        // that is the whole table, so the seed is byte-for-byte the old one.
        let young_bounds_follow_committed = g1_young_bounds_committed_enabled();
        let young_seed_regions = g1_young_sizer_heap_regions(
            young_bounds_follow_committed,
            initial_heap_target_regions(
                resize,
                initial_commit.div_ceil(config.region_size.max(1)),
                num_regions.max(1),
            ),
            num_regions,
        );

        Self {
            self_forward_marks: parking_lot::Mutex::new(Vec::new()),
            self_forward_tracking: AtomicBool::new(false),
            self_forward_count: AtomicUsize::new(0),
            self_forwarded_addrs: parking_lot::Mutex::new(Vec::new()),
            peer_object_pins: parking_lot::RwLock::new(Vec::new()),
            peer_object_pin_count: AtomicUsize::new(0),
            pin_filled_regions: parking_lot::Mutex::new(RegionSet::new()),
            active_evacuation_cset: RwLock::new(RegionSet::new()),
            active_evacuation: AtomicBool::new(false),
            layout_domain: std::sync::atomic::AtomicU32::new(cratonvm_types::FIRST_LAYOUT_DOMAIN),
            _bounds_registration: cratonvm_gc_core::jit_bounds::RelocatableHeapRegistration::new(),
            config: config.clone(),
            arena,
            regions: RwLock::new(regions),
            jit_tlab_skip_regions: Mutex::new(Vec::new()),
            cset_verify_cursor: AtomicUsize::new(0),
            cset_verify_resume_stamp: AtomicU64::new(0),
            flagless_sweep_prev: Mutex::new(FxHashSet::default()),
            eden_slots: (0..Self::eden_stripe_count(num_regions))
                .map(|_| AtomicUsize::new(usize::MAX)) // no eden yet
                .collect(),
            next_hash_code: AtomicI32::new(1),
            needs_gc_free_percent: AtomicUsize::new(25),
            young_survival_permille: AtomicUsize::new(1000),
            low_free_trigger_floor: AtomicUsize::new(0),
            eden_bytes_since_pause: AtomicUsize::new(0),
            // Every region starts Free (`G1Region::from_arena`). Seeding this
            // at 0 instead would make the first `needs_gc()` on a brand-new
            // heap answer "collect now".
            accessor_calls_lockfree: AtomicU64::new(0),
            accessor_calls_locked: AtomicU64::new(0),
            rset_fast_path_epoch_raced: AtomicU64::new(0),
            rset_edge_memo_hit: AtomicU64::new(0),
            rset_edge_recorded: AtomicU64::new(0),
            rset_edge_same_region: AtomicU64::new(0),
            free_region_count: AtomicUsize::new(num_regions),
            free_scan_hint: AtomicUsize::new(0),
            young_scan_hint: AtomicUsize::new(usize::MAX),
            evac_dest_survivor: AtomicUsize::new(usize::MAX),
            evac_dest_old: AtomicUsize::new(usize::MAX),
            young_region_count: AtomicUsize::new(0),
            // Starts at the ceiling — see the field doc: the cap must be a
            // no-op until a pause is measured to overrun the goal. gci i45/gy:
            // the ceiling of the committed regions (see `young_seed_regions`);
            // `Defaults.bounds(n).1` is `(n * G1_MAX_NEW_PERCENT / 100).max(1)`.
            young_target_regions: AtomicUsize::new(
                G1YoungSizer::Defaults.bounds(young_seed_regions).1,
            ),
            young_pinned_kept_regions: AtomicUsize::new(0),
            young_target_net_of_pins: g1_young_target_net_of_pins(), // gci i12/sx
            young_target_hotspot_model: g1_young_target_hotspot_model(), // gci i14/yt
            young_after_pause_regions: AtomicUsize::new(0),
            // gci i43/gy: `G1YoungSizer::Defaults` until `set_young_gen_sizing`.
            young_sizer_kind: AtomicU8::new(0),
            young_sizer_a: AtomicUsize::new(0),
            young_sizer_b: AtomicUsize::new(0),
            young_ceiling_binding: AtomicBool::new(false),
            young_bounds_follow_committed: AtomicBool::new(young_bounds_follow_committed),
            survivor_ratio: AtomicUsize::new(G1_SURVIVOR_TARGET_DIVISOR),
            young_survivor_equiv_regions: AtomicUsize::new(0),
            yt_work_ps_per_byte: AtomicU64::new(0),
            yt_base_us: AtomicU64::new(0),
            yt_surv_bytes_per_region: AtomicU64::new(0),
            survivor_plab_regions: AtomicU64::new(0),
            survivor_plab_chunks: AtomicU64::new(0),
            survivor_plab_tail_bytes: AtomicU64::new(0),
            survivor_plab_direct_objects: AtomicU64::new(0),
            survivor_plab_direct_bytes: AtomicU64::new(0),
            survivor_plab_switch_waits: AtomicU64::new(0),
            yt_surv_regions_last: AtomicUsize::new(0),
            yt_surv_bytes_last: AtomicU64::new(0),
            yt_surv_regions_in: AtomicUsize::new(0),
            yt_surv_bytes_in: AtomicU64::new(0),
            yt_surv_survival_permille: AtomicU64::new(0),
            yt_split_samples: AtomicU64::new(0),
            free_trigger_net_of_pins: g1_free_trigger_net_of_pins(), // gci i12/sx
            verify_unbounded_on_verbose_gc: g1_verify_unbounded_on_verbose_gc(), // gci i12/sx
            survivor_overflow_to_old: g1_survivor_overflow_to_old(), // gci i13/ef
            survivor_overflow_promotions: AtomicU64::new(0),
            pinned_young_as_survivor: g1_pinned_young_as_survivor(), // gci i13/ef
            pinned_eden_relabelled: AtomicU64::new(0),
            heap_min_free_ratio: AtomicU8::new(G1_DEFAULT_MIN_HEAP_FREE_RATIO),
            heap_max_free_ratio: AtomicU8::new(G1_DEFAULT_MAX_HEAP_FREE_RATIO),
            // LANE E wave 2 — the soft capacity started at the CEILING, so the
            // first pause under `CRATONVM_G1_HEAP_RESIZE=1` decided exactly
            // what the flag-off build decides. gci i13/ef finding 8: under the
            // opt-in it now starts at `-Xms`, as HotSpot's capacity starts at
            // `InitialHeapSize`; with the opt-in off it is the ceiling, as
            // before (and `heap_target_regions_within(false)` ignores it).
            heap_target_regions: AtomicUsize::new(initial_heap_target_regions(
                resize,
                initial_commit.div_ceil(config.region_size.max(1)),
                num_regions.max(1),
            )),
            gc_overhead_ppm: AtomicU64::new(0),
            last_pause_end: Mutex::new(None),
            heap_shrink_streak: std::sync::atomic::AtomicU32::new(0),
            heap_grow_events: AtomicU64::new(0),
            heap_shrink_events: AtomicU64::new(0),
            heap_uncommitted_bytes: AtomicU64::new(0),
            humongous_requests: AtomicU64::new(0),
            humongous_failures: AtomicU64::new(0),
            humongous_fail_free_regions: AtomicU64::new(0),
            humongous_fail_longest_run: AtomicU64::new(0),
            last_failed_humongous_span: AtomicUsize::new(0),
            pending_humongous_compaction: Mutex::new(None),
            humongous_compaction_avoid: Mutex::new(None),
            compaction_staged_old: Mutex::new(Vec::new()),
            compaction_stages: AtomicUsize::new(0),
            native_alloc_pressure: AtomicBool::new(false),
            external_gpu: cratonvm_gc_core::gpu_critical::ExternalGpuPressureTracker::with_heap_capacity(reserved),
            serial_recovery_requested: AtomicBool::new(false),
            futile_young_streak: AtomicUsize::new(0),
            serial_forward_hint: AtomicUsize::new(0),
            forward_buffers: Mutex::new(Vec::new()),
            sorted_buffers: Arc::new(cratonvm_types::pointer_map::SortedBufferPool::new(
                reserved / 6,
            )),
            futile_young_escalations: AtomicU64::new(0),
            starved_full_armed: AtomicBool::new(false),
            starved_full_this_pause: AtomicBool::new(false),
            in_place_only_armed: AtomicBool::new(false),
            in_place_only_this_pause: AtomicBool::new(false),
            in_place_fills_free_armed: AtomicBool::new(false),
            in_place_fills_free_this_pause: AtomicBool::new(false),
            fragmented_slide_span: AtomicUsize::new(0),
            starved_full_need: AtomicUsize::new(STARVED_FULL_STREAK),
            starved_young_streak: AtomicUsize::new(0),
            reserve_credit_used: AtomicUsize::new(RESERVE_CREDIT_UNARMED),
            reserve_credit_spent: AtomicUsize::new(0),
            reserve_last_chance: AtomicBool::new(false),
            reserve_last_chance_claims: AtomicU64::new(0),
            reserve_credit_claims: AtomicU64::new(0),
            reserve_credit_refusals: AtomicU64::new(0),
            mark_start_snapshot: Mutex::new(Vec::new()),
            gc_state: Arc::new(ConcurrentGcState::new()),
            satb_queue: Arc::new(SatbQueue::new()),
            collection_count: AtomicU64::new(0),
            refproc_kept_young_token: AtomicU64::new(0),
            kept_young_regions: Mutex::new((false, RegionSet::new())),
            mixed_collection_count: AtomicU64::new(0),
            total_pause_us: AtomicU64::new(0),
            pause_history: Mutex::new(VecDeque::new()),
            pause_history_dropped: AtomicU64::new(0),
            evac_ns_per_byte: AtomicU64::new(G1_EVAC_NS_PER_BYTE_DEFAULT),
            evac_cost_samples: AtomicU64::new(0),
            young_evac_ns_per_byte: AtomicU64::new(0),
            young_evac_cost_samples: AtomicU64::new(0),
            evac_cost_samples_refused: AtomicU64::new(0),
            old_gen_bytes: AtomicUsize::new(0),
            marking_threshold_bytes: AtomicUsize::new(ihop_threshold),
            mark_cycle_start: Mutex::new(None),
            mark_ms_ema: AtomicU64::new(0),
            alloc_rate_kib_per_ms: AtomicU64::new(0),
            ihop_headroom_percent: AtomicU64::new(100),
            ihop_late_events: AtomicU64::new(0),
            // --- lane W2-C state ---
            // `usize::MAX` is the "no cleanup has sampled one yet" sentinel;
            // the reclaim/occupancy pair starts at zero, which reads as "the
            // last cycle reclaimed nothing from an empty heap" — and since an
            // empty heap is below any threshold, the level test in
            // `check_ihop` rejects first and the back-off gate is never
            // consulted on a fresh collector. The back-off width starts at its
            // minimum, i.e. one region of growth re-arms the trigger.
            mark_cycle_gross_old_bytes: AtomicUsize::new(usize::MAX),
            last_cycle_reclaimed_bytes: AtomicUsize::new(0),
            last_cycle_end_old_bytes: AtomicUsize::new(0),
            mark_backoff_regions: AtomicUsize::new(1),
            // LANE W5-C — `u64::MAX` is "no cycle has ended yet". NOT 0: a
            // cycle that ends before the first evacuation pause legitimately
            // records 0, and a 0 sentinel would mistake that for "unset" and
            // hold the deadline shut for the life of the process.
            last_cycle_end_collections: AtomicU64::new(u64::MAX),
            mark_backoff_deadline_releases: AtomicU64::new(0),
            mark_backoff_suppressions: AtomicU64::new(0),
            // LANE W6-B — zero is "nothing published", which is the state the
            // default build never leaves.
            backoff_gate_open_at_bytes: AtomicUsize::new(0),
            backoff_fast_declines: AtomicU64::new(0),
            alloc_rate_bytes_per_ms: AtomicU64::new(0),
            ihop_disengaged_cycles: AtomicU64::new(0),
            survivor_age_bytes: std::array::from_fn(|_| AtomicUsize::new(0)),
            last_survivor_age_bytes: std::array::from_fn(|_| AtomicUsize::new(0)),
            bytes_promoted_total: AtomicU64::new(0),
            tenuring_threshold: AtomicU64::new(config.promotion_age as u64),
            // gci i22/sz: GC(0)'s line -- the starting young target's desired
            // size and the configured threshold, as HotSpot's first pause
            // derives them from an empty age table.
            tenuring_desired_bytes: AtomicU64::new(g1_desired_survivor_bytes(
                (num_regions * G1_MAX_NEW_PERCENT / 100).max(1),
                config.region_size,
            ) as u64),
            last_pause_tenuring_desired_bytes: AtomicU64::new(0),
            last_pause_tenuring_threshold: AtomicU64::new(config.promotion_age as u64),
            tenuring_xlog_fresh: AtomicBool::new(false),
            mixed_reserve_candidates: AtomicUsize::new(0),
            mixed_reserve_live_bytes: AtomicU64::new(0),
            mixed_reserve_selected_base: AtomicU64::new(0),
            humongous_conc_start_requested: AtomicBool::new(false),
            // gci i23/tn: HotSpot's defaults until `set_tenuring_flags`.
            max_tenuring_threshold: AtomicU8::new(config.promotion_age),
            always_tenure: AtomicBool::new(false),
            pause_tenures_all: AtomicBool::new(false),
            target_survivor_ratio: AtomicU8::new(G1_TARGET_SURVIVOR_RATIO as u8),
            survivor_region_cap: AtomicUsize::new(usize::MAX),
            survivor_regions_claimed: AtomicUsize::new(0),
            survivor_cap_refused_this_pause: AtomicBool::new(false),
            survivor_full_serial: AtomicBool::new(false),
            survivor_cap_promotions: AtomicU64::new(0),
            survivor_cap_pauses: AtomicU64::new(0),
            string_dedup_table: Mutex::new(FxHashMap::default()),
            gc_log_enabled: AtomicBool::new(false),
            marking_complete: AtomicBool::new(false),
            mixed_gc_remaining: AtomicU64::new(0),
            // LANE W6-M — the mixed-pause refusal census.
            mixed_gate_polls: AtomicU64::new(0),
            mixed_gate_no_cycle: AtomicU64::new(0),
            mixed_gate_no_work: AtomicU64::new(0),
            mixed_pauses_entered: AtomicU64::new(0),
            mixed_cand_no_mark_data: AtomicU64::new(0),
            mixed_cand_too_live: AtomicU64::new(0),
            mixed_cand_pinned: AtomicU64::new(0),
            mixed_work_refused_waste: AtomicU64::new(0),
            mixed_cset_capped_percent: AtomicU64::new(0),
            mixed_cset_capped_budget: AtomicU64::new(0),
            mixed_old_regions_selected: AtomicU64::new(0),
            mixed_hotspot_choice: true,
            mixed_min_old: AtomicUsize::new(0),
            mixed_phase_ended_pinned: AtomicU64::new(0),
            promoted_into_marked_bytes: AtomicU64::new(0),
            promoted_into_marked_copies: AtomicU64::new(0),
            mixed_fixup_charged_pauses: AtomicU64::new(0),
            mixed_count_skips_compaction: false,
            mixed_count_compaction_pauses: AtomicU64::new(0),
            prepare_mixed: false,
            prepare_mixed_pending: AtomicBool::new(false),
            prepare_mixed_pauses: AtomicU64::new(0),
            eager_reclaim_during_mark: true,
            eager_mark_spans: AtomicU64::new(0),
            eager_mark_bytes: AtomicU64::new(0),
            eager_mark_gray_dropped: AtomicU64::new(0),
            eager_mark_declined: AtomicU64::new(0),
            mixed_futile_streak: AtomicU64::new(0),
            mixed_futile_pauses: AtomicU64::new(0),
            mixed_productive_pauses: AtomicU64::new(0),
            mixed_futile_breaker_polls: AtomicU64::new(0),
            mixed_work_refused_futile: AtomicU64::new(0),
            mixed_futile_probes: AtomicU64::new(0),
            mixed_work_waived_tight: AtomicU64::new(0),
            mixed_futile_skipped_samples: AtomicU64::new(0),
            // WARNING gci-user-flip-20261015: default ON (vi18-mx S + M2, vi19-oo S
            // admissible; proposal triage 2026-10-15); `=0` kills.
            mixed_futile_reset_per_cycle: true,
            mixed_futile_phase_polled: AtomicBool::new(false),
            mixed_futile_cycle_resets: AtomicU64::new(0),
            young_sizes_to_reserve: false,
            old_regions_reclaimed: AtomicU64::new(0),
            cleanup_scrub_freed_targets: true,
            scrub_counters: CleanupScrubCounters::default(),
            layout_census_key: Mutex::new((u64::MAX, u64::MAX)),
            layout_census_incomplete: AtomicU64::new(0),
            mark_worklist: Mutex::new(Vec::new()),
            mark_deques: (0..concurrent_mark_worker_count(&config))
                .map(|_| Mutex::new(Vec::new()))
                .collect(),
            mark_active: AtomicUsize::new(0),
            mark_waker: Mutex::new(None),
            fixup_ns_ema: AtomicU64::new(0),
            fixup_regions_ema: AtomicU64::new(0),
            phase_totals: G1PhaseTotals::default(),
            pin_attribution: G1PinAttribution::default(),
            young_copy_ns_ema: AtomicU64::new(0),
            mark_steals: AtomicUsize::new(0),
            mark_worker_scans: (0..concurrent_mark_worker_count(&config))
                .map(|_| AtomicUsize::new(0))
                .collect(),
            mark_worklist_overflowed: AtomicBool::new(false),
            mark_saw_implausible: AtomicBool::new(false),
            root_regions_cycle: AtomicBool::new(false),
            defer_pin_seeds: AtomicBool::new(false),
            deferred_pin_seeds: Mutex::new(Vec::new()),
            last_mark_trusted: AtomicBool::new(false),
            full_compaction_armed: AtomicBool::new(false),
            last_pause_slid: Mutex::new(Vec::new()),
            tlab_reserve_floor: { 1 },
            survivor_region_count: AtomicUsize::new(0),
            free_trigger_eden_floor: true,
            breached_reserve_eden: true,
            breached_reserve_tenures: true,
            breached_reserve_backoff: true,
            breached_reserve_full: true,
            breached_full_free_floor: AtomicUsize::new(usize::MAX),
            breached_full_arms: AtomicU64::new(0),
            breached_reserve_last_region: true,
            old_tail_bytes: AtomicUsize::new(0),
            full_compaction_this_pause: AtomicBool::new(false),
            full_compaction_maximal: AtomicBool::new(false),
            full_compaction_pauses: AtomicU64::new(0),
            mark_oob_gray_skips: AtomicUsize::new(0),
            mark_gray_without_header_flag: AtomicU64::new(0),
            mark_lock_batches: AtomicUsize::new(0),
            alloc_shared_claims: AtomicUsize::new(0),
            alloc_exclusive_claims: AtomicUsize::new(0),
            tlab_shared_carves: AtomicUsize::new(0),
            tlab_exclusive_carves: AtomicUsize::new(0),
            mark_oob_report_count: AtomicUsize::new(0),
            gray_prov: Mutex::new(std::collections::HashMap::new()),
            v7b_reported: Mutex::new(std::collections::HashSet::new()),
            reference_skip: Mutex::new(FxHashSet::default()),
            reference_skip_len: AtomicUsize::new(0),
            pending_finalizer_roots: Mutex::new(Vec::new()),
            finalizer_addrs_this_pause: Mutex::new(Vec::new()),
            overlay_live_this_pause: Mutex::new(Vec::new()),
            overlay_condemned_owners: Mutex::new(FxHashSet::default()),
            overlay_snapshot: RwLock::new(overlay::OverlaySnapshot::default()),
            overlay_rset: Mutex::new(overlay::OverlayRemSet::default()),
            overlay_snapshot_len: AtomicUsize::new(0),
            overlay_snapshot_classes: (0..overlay::OVERLAY_CLASS_WORDS)
                .map(|_| AtomicU64::new(0))
                .collect(),
            resurrected_finalizers: Mutex::new(Vec::new()),
            resurrection_closure: Mutex::new(Vec::new()),
            resurrection_holders: Mutex::new(Vec::new()),
            humongous_referenced: Mutex::new(RegionSet::new()),
            kept_unresolved_regions: Mutex::new(RegionSet::new()),
            kept_unresolved_live: Mutex::new(std::collections::HashSet::new()),
            kept_unresolved_any: AtomicBool::new(false),
            // SECURITY FIX (V7a): start the RSet TLS-cache epoch at 0.
            rset_cache_epoch: AtomicU64::new(0),
            instance_id: NEXT_G1_INSTANCE_ID.fetch_add(1, Ordering::Relaxed),
            region_lookup,
            cards,
            jit_edge_filter,
            card_regions_offered: AtomicU64::new(0),
            card_regions_skipped: AtomicU64::new(0),
            card_bytes_scanned: AtomicU64::new(0),
            card_bytes_skipped: AtomicU64::new(0),
            card_first_window_scanned: AtomicU64::new(0),
            card_first_window_skipped: AtomicU64::new(0),
            card_skip_rate_ppm: AtomicU64::new(0),
            card_cards_cleaned: AtomicU64::new(0),
            card_cards_kept: AtomicU64::new(0),
            block_jumps_taken: AtomicU64::new(0),
            block_bytes_jumped: AtomicU64::new(0),
            block_entries_refused: AtomicU64::new(0),
            block_tail_breaks: AtomicU64::new(0),
            block_tail_bytes: AtomicU64::new(0),
            arena_base,
            arena_end,
            region_shift,
            initial_commit_bytes: initial_commit,
            finalizer_pause: AtomicBool::new(false),
            evac_pool: std::sync::OnceLock::new(),
            conc_mark_cpu_ns: std::sync::atomic::AtomicU64::new(0),
            driver_cpu_ns: std::sync::atomic::AtomicU64::new(0),
        }
    }

    /// Create a G1 collector with default configuration.
    pub fn with_defaults() -> Self {
        Self::new(G1CollectorConfig::default())
    }

    /// Get the number of regions.
    pub fn num_regions(&self) -> usize {
        self.regions.read().len()
    }

    /// Bytes committed for the Java heap. See
    /// [`crate::vm_heap::VmHeap::committed_bytes`] for what the quantity is for
    /// and why it must not track live bytes.
    ///
    /// F-16: this used to be `num_regions * region_size` and constant for the
    /// collector's lifetime, because the whole heap was allocated in the
    /// constructor. It now reports what is actually charged to the process,
    /// which grows as regions are claimed — the number an operator watching
    /// footprint wants, and the one `Runtime.totalMemory` should be built from.
    /// [`Self::reserved_bytes`] is the old quantity.
    pub fn committed_bytes(&self) -> usize {
        self.arena.committed_len()
    }

    /// F-16 — bytes of address space reserved, i.e. `-Xmx` rounded to the
    /// region grid. Fixed for the collector's lifetime.
    pub fn reserved_bytes(&self) -> usize {
        self.arena.reserved_len()
    }

    /// gci i14/hd: the reservation `[start, end)`, the address pair HotSpot
    /// G1's `print_heap_on` prints after `garbage-first heap ... used <n>K`
    /// (`_hrm.reserved()`): the `gc,exit` summary, `jcmd GC.heap_info` and the
    /// SIGQUIT `Heap` section. Fixed for the collector's lifetime.
    pub fn reserved_span(&self) -> (usize, usize) {
        (self.arena_base, self.arena_end)
    }

    /// F-16 — is the heap a real reservation, or the fully-committed fallback?
    /// Diagnostics and tests.
    pub fn heap_is_reserved(&self) -> bool {
        self.arena.is_reserved()
    }

    /// F-16 — return the pages of a trailing run of Free regions to the OS.
    ///
    /// The other half of the reserved heap: growth on demand is what stops a
    /// large `-Xmx` costing memory it does not use, and this is what lets a
    /// process that has finished a burst give the memory back rather than
    /// holding its high-water mark for its whole life.
    ///
    /// # The three conditions, and why each one
    ///
    /// * **Only a trailing run, and only Free regions.** The committed set has
    ///   to stay a prefix (see `commit_through_region`), so the only regions
    ///   that can be given back are the ones above the highest region still in
    ///   use — and every one of them must be Free, because a region of any
    ///   other type has bytes a walker will read.
    /// * **Never below `-Xms`.** An operator who asked for an initial heap
    ///   asked not to pay for growing back into it.
    /// * **Narrow the published JIT read bounds FIRST.** That table asserts "a
    ///   raw load anywhere in this range cannot fault", and a compiled
    ///   `getfield` tests against it at runtime. Unmapping pages a live bound
    ///   still describes is a fault in compiled code. The invariant
    ///   `commit_through_region` maintains in the growth direction — published
    ///   may lag committed, never exceed it — is maintained here by doing the
    ///   two steps in the opposite order.
    ///
    /// # Why only from `cleanup`
    ///
    /// This is the one reclamation point that runs stop-the-world with the
    /// regions lock held AND after every free of the cycle has been applied.
    /// An evacuation pause cannot do it: it frees the collection set, but a
    /// concurrent mark cycle may still hold gray addresses into regions it has
    /// not yet proven dead.
    ///
    /// Opt-in (`CRATONVM_G1_UNCOMMIT`). Growth on demand carries the finding on
    /// its own and is safe by construction; giving pages back is the half where
    /// getting the ordering wrong is a fault rather than a missed optimisation,
    /// so it ships behind its own switch and its own test.
    ///
    /// Returns the bytes released.
    pub fn uncommit_trailing_free_regions(&self, regions: &[G1Region]) -> usize {
        self.uncommit_trailing_free_regions_within(regions, gc_flags().g1_uncommit)
    }

    /// The shrink itself, with the opt-in as a PARAMETER rather than a read of
    /// a process-cached flag.
    ///
    /// Split out for the same reason `verify_no_dangling_into_cset_within` is:
    /// the flag is latched once per process, so a test that went through the
    /// wrapper could only ever exercise whichever arm the ambient environment
    /// selected — and for an opt-in flag that is always the arm that does
    /// nothing.
    pub fn uncommit_trailing_free_regions_within(&self, regions: &[G1Region], enabled: bool) -> usize {
        // LANE E wave 2 — the body moved to
        // [`Self::uncommit_above_region_within`], which takes the KEEP COUNT
        // rather than the region table.
        //
        // This function's only remaining job is to derive that count, and the
        // split is what gives the shrink a second caller: every evacuation
        // driver funnels through `record_collection_with_phases` holding the
        // regions guard as a WRITER, so it cannot borrow the table to compute
        // this — but the per-pause census already has
        // (`G1PausePhases::highest_used_region`). The finding this closes
        // (`lane-e-heap-never-shrinks-and-never-resizes-to-occupancy.md`) is
        // precisely that `cleanup` was the one production caller and most
        // workloads never reach it.
        //
        // The floor moved too, into `shrink_floor_bytes`: `-Xms` always, and
        // the adaptive soft capacity as well once `CRATONVM_G1_HEAP_RESIZE` is
        // armed. With the flag unset it is the same `initial_commit_bytes`
        // floor this function has always used.
        //
        // The highest region that is NOT Free, plus one: everything above it is
        // a candidate.
        let keep_regions = regions
            .iter()
            .rposition(|r| r.region_type() != RegionType::Free)
            .map_or(0, |i| i + 1);
        self.uncommit_above_region_within(keep_regions, enabled)
    }

    /// zt t29/g1 (G1 carry page item 10a, `-XX:+AlwaysPreTouch`,
    /// `CRATONVM_GC_ALWAYS_PRE_TOUCH_ALL`): touch the committed region prefix
    /// now (the `-Xms` startup commit) and every later region commit as it is
    /// made ([`cratonvm_gc_core::heap_reservation::ReservedHeap::set_pre_touch`]), as
    /// HotSpot G1 pre-touches the regions it commits. Returns the bytes
    /// touched now.
    pub fn set_always_pre_touch(&self, on: bool) -> usize {
        let touched = self.arena.set_pre_touch(on);
        tracing::debug!(
            target: "cratonvm::gc",
            touched,
            "g1: -XX:+AlwaysPreTouch touched the startup commit",
        );
        touched
    }

    /// zki (page `zgc-zt23hunt4-*` item 5(c)): HotSpot 25 G1 under
    /// `-XX:+UseTransparentHugePages` advises its heap `MADV_HUGEPAGE`; so
    /// does this heap's reservation. Call before the first allocation.
    pub fn advise_transparent_huge_pages(&self) -> bool {
        self.arena.advise_huge_pages()
    }

    /// zt t29/g1: whether `-XX:+AlwaysPreTouch` is in force on this heap.
    pub fn always_pre_touch(&self) -> bool {
        self.arena.pre_touch_enabled()
    }

    /// F-16 — make sure every byte up to the END of region `idx` is committed,
    /// and republish the JIT read bounds if the prefix grew.
    ///
    /// Returns `false` when the OS refused, which the caller must treat as "no
    /// such free region": the address space is reserved but the pages are not
    /// there, and handing the allocator a pointer into them is a fault, not an
    /// allocation.
    ///
    /// # Why through, and not just this region
    ///
    /// The committed set has to be a RANGE, because
    /// `gen_heap::publish_jit_read_bounds` — which compiled `getfield` fast
    /// paths test against to decide whether a raw load can fault — can only
    /// express one. Committing region `idx` therefore commits everything below
    /// it too. That costs almost nothing in practice: `claim_free_region`
    /// starts at zero and advances a rotating hint, so claims run roughly in
    /// index order.
    ///
    /// # The invariant, and why no lock is needed
    ///
    /// The published bound may LAG the committed prefix; it must never EXCEED
    /// it. A bound that is too narrow costs a compiled `getfield` a helper call
    /// and nothing else; a bound that is too wide is a fault.
    ///
    /// Commit happens before the republish, and the prefix only grows while
    /// mutators are running, so a racing republish can only ever publish a
    /// value that was committed at the moment it read it. That is why this is
    /// callable from an evacuation worker (which holds no lock but runs with
    /// every mutator parked) as well as from the allocator (which holds the
    /// regions lock for other reasons).
    pub fn commit_through_region(&self, idx: usize) -> bool {
        let want = (idx + 1).saturating_mul(self.config.region_size);
        if want <= self.arena.committed_len() {
            return true;
        }
        if !self.arena.commit_to(want) {
            return false;
        }
        cratonvm_gc_core::jit_bounds::publish_jit_read_bounds(
            0,
            self.arena_base,
            self.arena_base + self.arena.committed_len(),
        );
        // ...and the movable envelope with it, for the reason given where it is
        // first published: it is bounded by the commit, so it has to move when
        // the commit does.
        cratonvm_gc_core::jit_bounds::publish_movable_bounds(
            0,
            self.arena_base,
            self.arena_base + self.arena.committed_len(),
        );
        true
    }

    /// Generate the next identity hash code.
    ///
    /// Relaxed ordering is sufficient: hash codes are monotonic counters with
    /// no ordering requirements relative to other memory operations. Duplicate
    /// or slightly-stale values are acceptable per the JVM spec (identity
    /// hashes need not be unique).
    ///
    /// H1: exposed publicly so `interpreter::init_object_header` (TLAB
    /// fast path) can mint a unique hash at allocation time, matching the
    /// non-TLAB allocators.
    pub fn next_hash(&self) -> i32 {
        self.next_hash_code.fetch_add(1, Ordering::Relaxed)
    }

    // -----------------------------------------------------------------------
    // Allocation
    // -----------------------------------------------------------------------

    /// Whether a young pause started now could run out of to-space: the
    /// predicted copy of the `young` regions (all of them until a pause has
    /// measured otherwise) plus a 10% reserve no longer fits in `free`.
    ///
    /// The Free-fraction trigger alone let the young generation grow to 75%
    /// of the heap. When most of it is live -- a program filling the heap
    /// with data it keeps -- the pause could copy only a third of it, kept
    /// every other young region wholesale (live objects AND the dead
    /// originals of the ones it did copy) and left no Free region for the
    /// next pause at all: `NativeFactoryReclaimProbe -Xmx128m` went from
    /// 97 Eden / 31 Free to 128 Survivor / 0 Free in one pause and threw
    /// `OutOfMemoryError` on a heap holding 90 MiB of live data. HotSpot's
    /// G1 bounds the young generation by the same to-space reserve
    /// (`G1ReservePercent`).
    pub fn young_exceeds_evac_headroom(&self, young: usize, free: usize, total: usize) -> bool {
        let survival = self
            .young_survival_permille
            .load(Ordering::Relaxed)
            .min(1000);
        let predicted = (young * survival).div_ceil(1000);
        let reserve = to_space_reserve_regions(total);
        // The previous pause's survival ratio cannot bound the next live
        // set: a drop-heavy loop can immediately start retaining everything.
        // Keep the prediction's reserve, but also bound young by the actual
        // destination capacity before that phase change exhausts to-space.
        predicted + reserve >= free || young >= free
    }

    /// The G1 near-full proxy row (`CRATONVM_G1_BREACHED_RESERVE_TENURES`,
    /// default ON, `=0` kills; read once per collector): a collection that
    /// starts with the to-space reserve breached -- the Free pool plus the
    /// Eden claimed since the last pause is at most
    /// [`to_space_reserve_regions`] -- tenures every live young object, so
    /// its evacuation needs Old destinations only. HotSpot's survivor cap is
    /// `min(ceil(young target / SurvivorRatio), free regions)`, zero there.
    ///
    /// Why: [`Self::breached_reserve_defers`] lets Eden take all but one
    /// Free region, and a pause that then needs a Survivor AND an Old
    /// destination ran out of to-space about once in three (`Evacuation
    /// Failure: Allocation` 8000-11000 times per 18 runs of
    /// `Zs3Nd3UpcallMints proxy 4096 -Xmx64m`, against about 300 before), and
    /// a Survivor region that every young pause re-copied kept one region of
    /// the Free pool from Eden for good.
    pub fn breached_reserve_tenures(&self) -> bool {
        self.breached_reserve_tenures && self.to_space_reserve_breached()
    }

    /// The Free pool plus the Eden claimed since the last pause is at most
    /// [`to_space_reserve_regions`]: the state [`Self::breached_reserve_defers`],
    /// [`Self::breached_reserve_tenures`] and [`Self::breached_reserve_backs_off`]
    /// answer to. Three relaxed loads.
    pub fn to_space_reserve_breached(&self) -> bool {
        let total = self.region_table_len().max(1);
        let free = self.free_region_count.load(Ordering::Relaxed);
        let eden = self
            .young_region_count
            .load(Ordering::Relaxed)
            .saturating_sub(self.survivor_region_count.load(Ordering::Relaxed));
        free.saturating_add(eden) <= to_space_reserve_regions(total)
    }

    /// The G1 near-full proxy row (`CRATONVM_G1_BREACHED_RESERVE_BACKOFF`,
    /// default ON, `=0` kills; read once per collector): [`Self::check_ihop`]
    /// applies the mark-cycle back-off (`CRATONVM_G1_IHOP_BACKOFF`, opt-in
    /// everywhere else) while the to-space reserve is breached.
    ///
    /// Why: on `Zs3Nd3UpcallMints proxy 4096 -Xmx64m` the live set holds
    /// Old above the IHOP threshold for the whole run, and every cycle marks
    /// it and frees nothing. 20000 iterations ran 420 such cycles: 840
    /// Remark and Cleanup pauses and about 350 mixed or concurrent-start
    /// young pauses on top of the young ones (3166 pauses, HotSpot 1047),
    /// against 5 cycles and 1929 pauses with the back-off. A heap past its
    /// reserve is where a futile cycle costs most, and the old garbage a
    /// declined cycle would find is the full collection's to take there
    /// (122 `Pause Full` in HotSpot's run).
    pub fn breached_reserve_backs_off(&self) -> bool {
        self.breached_reserve_backoff && self.to_space_reserve_breached()
    }

    /// The G1 near-full proxy row (`CRATONVM_G1_BREACHED_RESERVE_FULL`,
    /// default ON, `=0` kills; read once per collector), at the end of every
    /// pause: after a full compaction (`compacted`), record the Free pool it
    /// left; after any other pause that leaves the to-space reserve breached,
    /// no concurrent cycle open, and room for at most one Eden region beside
    /// the TLAB reserve, arm an in-place full compaction for the next pause
    /// ([`Self::arm_in_place_compaction`]) -- but only once the Free pool has
    /// fallen below what the last full compaction left, so a heap whose live
    /// data holds the pool there does not compact pause after pause.
    ///
    /// Why: on `Zs3Nd3UpcallMints proxy 4096 -Xmx64m` a full compaction
    /// leaves Old at 61 of 64 regions. The young pauses then run on 2 Eden
    /// regions until promotion takes Old to 62, and then on ONE region, a
    /// dozen times, until an allocation fails into the ladder's full
    /// collection, which takes Old back to 61 (890 of 1860 young pauses in
    /// 20000 iterations, about 1900 pauses against HotSpot's 790-1020 on the
    /// same host). HotSpot's young pause there fails evacuation and its
    /// `Pause Full` follows at once.
    pub fn note_breached_reserve_full(&self, compacted: bool) -> bool {
        if !self.breached_reserve_full {
            return false;
        }
        let free = self.free_region_count.load(Ordering::Relaxed);
        if compacted {
            self.breached_full_free_floor.store(free, Ordering::Relaxed);
            return false;
        }
        if self.satb_queue.is_active()
            || !self.to_space_reserve_breached()
            || free.saturating_sub(self.tlab_reserve_floor) > 1
            || free >= self.breached_full_free_floor.load(Ordering::Relaxed)
        {
            return false;
        }
        if !self.arm_in_place_compaction() {
            return false;
        }
        self.breached_full_arms.fetch_add(1, Ordering::Relaxed);
        true
    }

    /// Full compactions [`Self::note_breached_reserve_full`] armed.
    pub fn breached_full_arms(&self) -> u64 {
        self.breached_full_arms.load(Ordering::Relaxed)
    }

    /// The G1 near-full proxy row (`CRATONVM_G1_BREACHED_RESERVE_EDEN`,
    /// default ON, `=0` kills; read once per collector): whether the low-Free
    /// arms (`needs_gc`'s Free-fraction and evacuation-headroom arms, the
    /// native-pressure latch) hold off because the to-space reserve was
    /// already breached when this young generation started -- the Free pool
    /// plus the Eden claimed since is at most [`to_space_reserve_regions`] --
    /// and the predicted survivors of `young` (at least one region) still
    /// leave a Free region over.
    ///
    /// Why: on `Zs3Nd3UpcallMints proxy 4096 -Xmx64m` the live set leaves 3
    /// Free regions against a reserve of 6. Both arms then fired at ONE Eden
    /// region (`predicted + 6 >= free`; `eden >= free / 4`), so 20000
    /// iterations took about 2600 young pauses of a megabyte each where
    /// HotSpot takes 523: once the reserve cannot be kept, HotSpot's young
    /// target is its minimum young length and Eden spends the Free pool (a
    /// pause that then runs out of to-space fails evacuation). Here Eden
    /// stops one region short of that, for the survivors.
    pub fn breached_reserve_defers(&self, young: usize, free: usize, total: usize) -> bool {
        if !self.breached_reserve_eden {
            return false;
        }
        let eden = young.saturating_sub(self.survivor_region_count.load(Ordering::Relaxed));
        if free.saturating_add(eden) > to_space_reserve_regions(total) {
            return false;
        }
        let survival = self
            .young_survival_permille
            .load(Ordering::Relaxed)
            .min(1000);
        let predicted = (young * survival).div_ceil(1000).max(1);
        predicted < free || (free >= 1 && self.breached_reserve_last_region_fits(young))
    }

    /// The G1 near-full proxy row (`CRATONVM_G1_BREACHED_RESERVE_LAST_REGION`,
    /// default ON, `=0` kills; read once per collector): with the to-space
    /// reserve breached, may Eden take the last Free region as well? Yes when
    /// the Old regions' unused tails at the last pause
    /// ([`Self::old_tail_bytes`]) hold twice the survivors `young` regions
    /// predict, plus a region's sixteenth: a collection that starts breached
    /// tenures every survivor ([`Self::breached_reserve_tenures`]), and the
    /// serial destination scan packs them into those tails
    /// (`alloc_in_type_locked_scan`).
    ///
    /// Why: [`Self::breached_reserve_defers`] keeps one Free region for the
    /// survivors, and on `Zs3Nd3UpcallMints proxy 4096 -Xmx64m` the live set
    /// leaves Eden two regions beside it, then one: about 1800 young pauses in
    /// 20000 iterations, where HotSpot's Eden spends the whole pool (400-550
    /// young pauses). Taken unconditionally, the last region cut the pauses
    /// to 1206 but ran out of to-space 114 times in one run (about 1.6 a run
    /// before): each failure keeps regions and wedges a drain on this heap,
    /// and costs a pause of copying for nothing.
    pub fn breached_reserve_last_region_fits(&self, young: usize) -> bool {
        if !self.breached_reserve_last_region {
            return false;
        }
        let survival = self
            .young_survival_permille
            .load(Ordering::Relaxed)
            .min(1000);
        let predicted = (young.saturating_add(1) * self.config.region_size)
            .saturating_mul(survival)
            / 1000;
        predicted
            .saturating_mul(2)
            .saturating_add(self.config.region_size / 16)
            <= self.old_tail_bytes.load(Ordering::Relaxed)
    }

    /// Publish [`Self::old_tail_bytes`] from `regions` (end of a pause).
    pub fn publish_old_tail_bytes(&self, regions: &[G1Region]) {
        if !self.breached_reserve_last_region {
            return;
        }
        let tail: usize = regions
            .iter()
            .filter(|r| r.region_type() == RegionType::Old)
            .map(|r| r.data.len().saturating_sub(r.cursor()) & !7)
            .sum();
        self.old_tail_bytes.store(tail, Ordering::Relaxed);
    }

    /// Whether the young generation has grown enough for a low-Free trigger
    /// (`needs_gc`'s Free-fraction and evacuation-headroom arms, the
    /// native-pressure latch) to be worth a pause: at least a quarter of the
    /// remaining Free pool and at least one region, and -- after a pause that
    /// freed less than one region -- at least [`Self::low_free_trigger_floor`]
    /// bytes of new Eden allocation.
    ///
    /// Region counts alone cannot tell a full Eden region from one holding a
    /// single freshly carved TLAB. On a heap full of live data the
    /// quarter-of-Free floor therefore still admitted one pause per TLAB
    /// (`GenR4W4HeapFullThrashProbe`: 16 objects copied and one 256 KiB carve
    /// freed per pause at 10 Free / 116 Old), and with one Eden stripe per
    /// thread its two-thread shape showed 7 Eden regions and freed 525 KiB per
    /// pause, 9,500 pauses in 110 s. A futile pause is the evidence that
    /// collecting this young generation buys nothing, so the next one waits
    /// for another quarter of the Free pool's worth of allocation; a
    /// productive pause clears the floor.
    ///
    /// gci i8/gs: under `CRATONVM_G1_FREE_TRIGGER_NET_OF_PINS=1` the young
    /// count is taken net of the regions the last young or mixed pause kept
    /// out of its collection set under a pin
    /// ([`Self::young_pinned_kept_regions`]); see `free_trigger_young`.
    /// Only this gate is netted: [`Self::young_exceeds_evac_headroom`] is a
    /// to-space bound and keeps the gross count.
    ///
    /// The G1 evacuation page (`CRATONVM_G1_FREE_TRIGGER_EDEN_FLOOR`, default
    /// ON, `=0` kills; read once per collector): the quarter-of-Free floor is
    /// measured in EDEN regions, not young regions (Survivor regions are left
    /// out of the count). Survivor regions met it on their own: on `R23W6KeepLong -Xmx1g`
    /// (everything survives) 33 Survivor regions against 135 Free let the
    /// trigger fire after ONE Eden region, so 30 pauses in a row each copied
    /// the same 33 MB of survivors for 1 MB of new allocation. HotSpot's young
    /// pause waits for its Eden budget.
    #[inline]
    pub fn low_free_trigger_allowed(&self, young: usize, free: usize) -> bool {
        if !g1_free_trigger_young_floor() {
            return true;
        }
        // The G1 evacuation page: Eden regions only (Survivor regions are
        // created only inside pauses, so the census count is exact between
        // them).
        let young = if self.free_trigger_eden_floor {
            young.saturating_sub(self.survivor_region_count.load(Ordering::Relaxed))
        } else {
            young
        };
        let young = if self.free_trigger_net_of_pins {
            free_trigger_young(
                young,
                self.young_pinned_kept_regions.load(Ordering::Relaxed),
            )
        } else {
            young
        };
        young >= (free / 4).max(1)
            && self.eden_bytes_since_pause.load(Ordering::Relaxed)
                >= self.low_free_trigger_floor.load(Ordering::Relaxed)
    }

    /// After a pause: set [`Self::low_free_trigger_floor`] to a quarter of the
    /// Free pool's bytes if it freed less than one region, clear it otherwise;
    /// restart the Eden byte count.
    pub fn note_pause_productivity(&self, bytes_freed: usize) {
        let floor = if bytes_freed < self.config.region_size {
            let free = self.free_region_count.load(Ordering::Relaxed);
            (free / 4).max(1) * self.config.region_size
        } else {
            0
        };
        self.low_free_trigger_floor.store(floor, Ordering::Relaxed);
        self.eden_bytes_since_pause.store(0, Ordering::Relaxed);
    }

    /// Count `bytes` of Eden handed out; see [`Self::eden_bytes_since_pause`].
    #[inline]
    pub fn note_eden_bytes(&self, bytes: usize) {
        self.eden_bytes_since_pause
            .fetch_add(bytes, Ordering::Relaxed);
    }

    /// Fold one young pause's copy ratio into [`Self::young_survival_permille`].
    pub fn note_young_survival(&self, young_bytes: usize, bytes_copied: usize) {
        if young_bytes == 0 {
            return;
        }
        let sample = (bytes_copied.saturating_mul(1000) / young_bytes).min(1000);
        let _ = self.young_survival_permille.try_update(
            Ordering::Relaxed,
            Ordering::Relaxed,
            |cur| {
                Some(if sample >= cur {
                    sample
                } else {
                    cur - (cur - sample) / 4
                })
            },
        );
    }

    /// Re-arm the native-allocation pressure latch after a region left the
    /// Free pool: latch [`Self::native_alloc_pressure`] when the pool has
    /// dropped below the `needs_gc` threshold.
    ///
    /// Called ONLY when a new region is consumed — once per `region_size`
    /// bytes of allocation, never per object. `regions` must already be locked
    /// by the caller, and is read for its LENGTH alone (the denominator of the
    /// free fraction); the numerator is the maintained cache, so this does not
    /// walk the table.
    ///
    /// Ten-findings item 6: this reads the maintained [`Self::free_region_count`]
    /// instead of re-counting the table. It used to call
    /// [`Self::publish_region_census`] — a full pass over every region — and
    /// so did `refill_tlab` just before claiming, which made every Eden region
    /// claimed cost TWO table scans under the exclusive lock. The count is now
    /// decremented at the one funnel every Free->occupied transition goes
    /// through ([`Self::note_free_regions_claimed`]) and re-derived by the
    /// census every pause runs at its end, so between pauses it is exact and
    /// O(1).
    pub fn note_region_consumed_locked(&self, regions: &[G1Region]) {
        let free = self.free_region_count.load(Ordering::Relaxed);
        let pct = self.needs_gc_free_percent.load(Ordering::Relaxed).max(1);
        // Behind the same young floor as `needs_gc`'s Free-fraction arm: on a
        // heap whose Old regions are full of live data this latch re-armed on
        // every region claim, and the next native call on ANY thread (the
        // Reference Handler's wait loop is one) collected a one-region young
        // generation for it.
        let young = self.young_region_count.load(Ordering::Relaxed);
        // gci i18/mx: the same Free-fraction predicate as `needs_gc`.
        if free_fraction_trigger_fires(free, regions.len(), pct, self.young_sizes_to_reserve)
            && self.low_free_trigger_allowed(young, free)
            && !self.breached_reserve_defers(young, free, regions.len())
        {
            self.native_alloc_pressure.store(true, Ordering::Relaxed);
        }
    }

    /// Item 6 — account `count` regions leaving the Free pool, `young` of
    /// which become Eden or Survivor. Called under the exclusive guard by every
    /// claim path; the pause-end census is the backstop that re-derives both
    /// counters from the table, so a drift here can only be transient.
    #[inline]
    pub fn note_free_regions_claimed(&self, count: usize, young: usize) {
        let _ = self
            .free_region_count
            .try_update(Ordering::Relaxed, Ordering::Relaxed, |v| {
                Some(v.saturating_sub(count))
            });
        if young > 0 {
            self.young_region_count.fetch_add(young, Ordering::Relaxed);
        }
    }

    /// Hand a freshly-counted Free-region total to [`Self::free_region_count`].
    /// Call this from anywhere that has just done the O(num_regions) count
    /// under the lock — the point of the cache is that nobody should ever do
    /// that count for `needs_gc` alone.
    #[inline]
    pub fn publish_free_region_count(&self, free: usize) {
        self.free_region_count.store(free, Ordering::Relaxed);
    }

    /// Count Free and young (Eden + Survivor) regions in ONE pass and publish
    /// both caches (G1AUD-9).
    ///
    /// Every site that used to count Free regions alone now calls this: the
    /// second counter is the young half of `needs_gc`, and taking it in a
    /// separate pass would double the cost of the one loop this collector was
    /// already careful to run only on region CONSUMPTION rather than per object.
    /// `regions` must already be locked by the caller.
    pub fn publish_region_census(&self, regions: &[G1Region]) -> usize {
        let mut free = 0usize;
        let mut young = 0usize;
        let mut survivor = 0usize;
        for r in regions.iter() {
            match r.region_type() {
                RegionType::Free => free += 1,
                RegionType::Eden => young += 1,
                RegionType::Survivor => {
                    young += 1;
                    survivor += 1;
                }
                _ => {}
            }
        }
        self.young_region_count.store(young, Ordering::Relaxed);
        self.survivor_region_count.store(survivor, Ordering::Relaxed);
        self.publish_free_region_count(free);
        free
    }

    /// Inclusive `(min, max)` bounds on [`Self::young_target_regions`].
    ///
    /// gci i43/gy: HotSpot 25's `G1YoungGenSizer::recalculate_min_max_young_length`
    /// of the operator's sizing ([`Self::set_young_gen_sizing`]); with none
    /// of `-Xmn` / `-XX:NewSize` / `-XX:MaxNewSize` / `-XX:NewRatio` given
    /// (or `CRATONVM_G1_YOUNG_GEN_SIZER=0`) exactly the bounds before i43,
    /// `G1NewSizePercent` (5) / `G1MaxNewSizePercent` (60) of the regions.
    ///
    /// gci i45/gy (`gci-i43gy-g1-young-bounds-use-max-regions-not-committed-RETIRED-UNFIXED-20261018`):
    /// "the regions" are the COMMITTED ones, as HotSpot's
    /// `G1YoungGenSizer::heap_size_changed(num_committed_regions)` has them --
    /// here the soft capacity ([`Self::heap_target_regions`]), which starts at
    /// `InitialHeapSize` and moves with the sizing policy. Without
    /// `CRATONVM_G1_HEAP_RESIZE` the soft capacity is the whole table, so the
    /// bounds are byte-for-byte the `-Xmx`-grid ones; `CRATONVM_G1_YOUNG_BOUNDS_COMMITTED=0`
    /// keeps the grid under the opt-in too. `-Xmn` / `NewSize` + `MaxNewSize`
    /// ([`G1YoungSizer::MaxAndNewSize`]) never depend on the count.
    #[inline]
    pub fn young_region_bounds(&self) -> (usize, usize) {
        self.young_region_bounds_within(g1_heap_resize_enabled())
    }

    /// [`Self::young_region_bounds`] with the `CRATONVM_G1_HEAP_RESIZE` arm as
    /// a PARAMETER (the flag is latched per process; see
    /// [`Self::heap_target_regions_within`]).
    pub fn young_region_bounds_within(&self, resize_enabled: bool) -> (usize, usize) {
        self.young_sizer().bounds(g1_young_sizer_heap_regions(
            self.young_bounds_follow_committed.load(Ordering::Relaxed),
            self.heap_target_regions_within(resize_enabled),
            self.region_table_len(),
        ))
    }

    /// gci i45/gy — HotSpot's `G1Policy::record_new_heap_size` ->
    /// `G1YoungGenSizer::heap_size_changed`: the soft capacity moved from
    /// `old_heap_regions` to `new_heap_regions`, so the young bounds did, and
    /// the young target is re-placed in the new ones
    /// ([`g1_young_target_after_heap_size_change`]): a target at the old
    /// ceiling follows the ceiling (the default ceiling keeps the young arm
    /// inert, the i14yt / i20ys rule), any other is clamped. HotSpot
    /// recomputes the target at its next `update_young_length_bounds`; the
    /// model here recomputes it at the next pause, against these bounds.
    ///
    /// Called by [`Self::commit_heap_resize`] (reached only under the opt-in)
    /// BEFORE a grow's capacity store and AFTER a shrink's, so a racing
    /// `needs_gc` sees the target at or above the ceiling it reads, never an
    /// active arm the resize did not mean. A no-op with the switch off.
    pub fn young_bounds_heap_size_changed(&self, old_heap_regions: usize, new_heap_regions: usize) {
        if !self.young_bounds_follow_committed.load(Ordering::Relaxed) {
            return;
        }
        let table = self.region_table_len();
        let sizer = self.young_sizer();
        let old = sizer.bounds(g1_young_sizer_heap_regions(true, old_heap_regions, table));
        let new = sizer.bounds(g1_young_sizer_heap_regions(true, new_heap_regions, table));
        if old == new {
            return;
        }
        let cur = self.young_target_regions.load(Ordering::Relaxed);
        self.young_target_regions.store(
            g1_young_target_after_heap_size_change(cur, old, new),
            Ordering::Relaxed,
        );
    }

    /// gci i43/gy — the young sizing this collector applies (decoded from the
    /// three fields [`Self::set_young_gen_sizing`] wrote).
    #[inline]
    pub fn young_sizer(&self) -> G1YoungSizer {
        G1YoungSizer::decode(
            self.young_sizer_kind.load(Ordering::Relaxed),
            self.young_sizer_a.load(Ordering::Relaxed),
            self.young_sizer_b.load(Ordering::Relaxed),
        )
    }

    /// gci i43/gy — `true` when the young ceiling is the operator's
    /// ([`G1YoungSizer::ceiling_is_operators`]); see the field.
    #[inline]
    pub fn young_ceiling_binding(&self) -> bool {
        self.young_ceiling_binding.load(Ordering::Relaxed)
    }

    /// gci i43/gy — HotSpot's `G1Policy::use_adaptive_young_list_length()`:
    /// `false` for a fixed young length (`-Xmn`, `NewSize == MaxNewSize`,
    /// `-XX:NewRatio` alone). Diagnostics / JFR.
    pub fn young_sizing_adaptive(&self) -> bool {
        self.young_sizer().adaptive()
    }

    /// gci i43/gy — the `(min, max)` young length in BYTES at the full heap
    /// (`max_num_regions`, as HotSpot's `G1YoungGenSizer::adjust_max_new_size`
    /// derives the ergonomic `MaxNewSize` from it). Diagnostics / JFR
    /// (`jdk.YoungGenerationConfiguration`).
    pub fn young_length_bounds_bytes(&self) -> (usize, usize) {
        // gci i45/gy: the full table explicitly, as the doc says
        // (`young_region_bounds` follows the committed regions now).
        let (min, max) = self.young_sizer().bounds(self.region_table_len().max(1));
        (
            min.saturating_mul(self.config.region_size),
            max.saturating_mul(self.config.region_size),
        )
    }

    /// gci i43/gy (`gci-i42ta-g1-ignores-xmn-newsize-maxnewsize-newratio-FIXED-20261016`)
    /// -- apply `-Xmn` / `-XX:NewSize` / `-XX:MaxNewSize` / `-XX:NewRatio` to
    /// G1 the way HotSpot 25's `G1YoungGenSizer` does, before the first
    /// allocation (`vm_init`'s G1 heap construction). Returns the
    /// `[warning][gc,ergo]` texts HotSpot logs for the request (the caller
    /// prints them); empty for a default request.
    ///
    /// A default request (none of the four), or `CRATONVM_G1_YOUNG_GEN_SIZER=0`,
    /// changes nothing: the bounds stay `G1NewSizePercent` /
    /// `G1MaxNewSizePercent` and the young-target model is the one the
    /// i14yt / i20ys rows test. Otherwise the bounds are the sizer's, the
    /// target restarts at the new ceiling (as it starts at the default one),
    /// GC(0)'s desired survivor size is re-derived from it, and an operator's
    /// ceiling binds the young-target arm ([`Self::young_ceiling_binding`]).
    pub fn set_young_gen_sizing(
        &self,
        req: &cratonvm_gc_core::gen_young_sizing::YoungGenSizing,
    ) -> Vec<String> {
        if req.is_default() || !g1_young_gen_sizer_enabled() {
            return Vec::new();
        }
        let (sizer, warnings) = g1_young_sizer_from_flags(req, self.config.region_size);
        let (kind, a, b) = sizer.encode();
        self.young_sizer_a.store(a, Ordering::Relaxed);
        self.young_sizer_b.store(b, Ordering::Relaxed);
        self.young_sizer_kind.store(kind, Ordering::Relaxed);
        self.young_ceiling_binding
            .store(sizer.ceiling_is_operators(), Ordering::Relaxed);
        let (_min, max) = self.young_region_bounds();
        self.young_target_regions.store(max, Ordering::Relaxed);
        self.tenuring_desired_bytes
            .store(self.survivor_target_bytes() as u64, Ordering::Relaxed);
        warnings
    }

    /// Current adaptive young-generation size, in regions. Diagnostics/tests.
    pub fn young_target_regions(&self) -> usize {
        self.young_target_regions.load(Ordering::Relaxed)
    }

    /// Cached Eden + Survivor region count. Diagnostics/tests.
    pub fn young_region_count(&self) -> usize {
        self.young_region_count.load(Ordering::Relaxed)
    }

    /// Cached Free region count — the MAINTAINED value, not a fresh scan.
    /// Diagnostics/tests; the twin of [`Self::young_region_count`], published
    /// by the same one-pass [`Self::publish_region_census`].
    ///
    /// Lane B added this so the cache can be compared against
    /// `count_regions(RegionType::Free)` from OUTSIDE the crate. That
    /// comparison is the whole content of the lane-E finding this collector
    /// used to fail: a pause driven straight through `young_collection` left
    /// the cache reading its pre-pause value. Without a public reader the only
    /// place that assertion could live was `g1.rs`'s own `mod tests`.
    pub fn free_region_count(&self) -> usize {
        self.free_region_count.load(Ordering::Relaxed)
    }

    /// G1AUD-9 — re-size the young generation from the pause that just ended.
    ///
    /// See [`Self::young_target_regions`] for what this is for, why the target
    /// starts at its ceiling, and why an unproductive pause must widen rather
    /// than tighten it.
    ///
    /// gci i20/ys: since 2026-10-15 the pause record reaches this loop only
    /// with `CRATONVM_G1_YOUNG_TARGET_HOTSPOT_MODEL=0` (the model is default
    /// ON: `record_collection_with_phases_within` takes
    /// [`Self::update_young_target_predicted`] instead). The loop stays as the
    /// kill switch's arm and its tests' subject.
    pub fn update_young_target(
        &self,
        collection_type: G1CollectionType,
        pause_us: u64,
        stats: &GcStats,
    ) {
        if !gc_flags().g1_young_pause_target {
            return;
        }
        if !matches!(
            collection_type,
            G1CollectionType::YoungOnly | G1CollectionType::Mixed
        ) {
            return;
        }
        let (min, max) = self.young_region_bounds();
        // Anti-storm: a pause that reclaimed nothing leaves the young count
        // where it was, so a tightened target would re-request a pause that
        // cannot help. Give the cap back its full slack and let a productive
        // overrun re-earn it.
        //
        // gci i8/gs: under `CRATONVM_G1_YOUNG_TARGET_FUTILE_BELOW_REGION=1` a
        // pause that freed less than one region is futile too -- the rule
        // `note_pause_productivity` already applies to the Free arms; see
        // `young_target_pause_futile`.
        if young_target_pause_futile(
            stats,
            self.config.region_size,
            g1_young_target_futile_below_region(),
        ) {
            self.young_target_regions.store(max, Ordering::Relaxed);
            return;
        }
        let goal_us = self.config.max_gc_pause_ms.saturating_mul(1_000);
        if goal_us == 0 {
            return;
        }
        let cur = self
            .young_target_regions
            .load(Ordering::Relaxed)
            .clamp(min, max);
        let next = if pause_us > goal_us {
            // Overran: take 20% off, never below the floor.
            (cur * 4 / 5).max(min)
        } else if pause_us.saturating_mul(2) < goal_us {
            // Comfortably under: give 12.5% back (plus one, so a target sitting
            // at the floor can still climb when the floor is small).
            (cur + cur / 8 + 1).min(max)
        } else {
            // Inside the hysteresis band — leave it alone rather than
            // oscillating around the goal.
            cur
        };
        // gci i19/ys — HotSpot's `absolute_min_young_length`: never a target
        // at or below the young regions this pause left, i.e. at least one
        // Eden region before the arm can fire again. Without it the floor was
        // `G1NewSizePercent` alone, and on G1 4-thread javac -Xmx512m
        // (vi15-bg def rows) the pause left 31-57 young regions (sparse
        // Survivor destinations + pinned-kept Eden) against a 25-region
        // target: `needs_gc` fired on the first poll after every pause, and
        // each pause copied ~58 KB and freed ~58 KB.
        // `CRATONVM_G1_YOUNG_TARGET_MIN_EDEN=0` restores the bare floor.
        let next = {
            next.max(young_target_absolute_min(
                min,
                max,
                self.young_after_pause_regions.load(Ordering::Relaxed),
            ))
        };
        self.young_target_regions.store(next, Ordering::Relaxed);
    }

    /// gci i15/bg — the DEFAULT young-target loop's entry from the pause
    /// record, with HotSpot's "base time is longer than pause time" case
    /// under `CRATONVM_G1_YOUNG_TARGET_BASE_AWARE=1` (default OFF; page
    /// `gci-i14yt-g1-default-young-loop-has-no-base-exceeds-goal-case-FIXED-20261016`).
    ///
    /// Switch OFF: exactly [`Self::update_young_target`] fed `sample_us` (the
    /// pause, or under `CRATONVM_G1_YOUNG_TARGET_NET_OF_HEAP_COSTS=1` the pause
    /// net of verify and fix-up), as before.
    ///
    /// Switch ON: the same gates as that loop (the `g1-young-pause-target`
    /// switch, young and mixed records only, the futile-pause reset to the
    /// ceiling), and the same 20 % / 12.5 % steps, but the overrun is first
    /// split the way HotSpot 25's `G1Policy::calculate_young_desired_length`
    /// splits it: a young-only pause teaches the predicted BASE time (the
    /// pause net of its copy, i14/yt's [`Self::teach_young_cost_model`], the
    /// same predictor the opt-in model reads) and then
    /// [`base_aware_young_target`] decides:
    ///
    ///  * predicted base at or above the goal: the minimum young length, at
    ///    once and not below it -- `max(G1NewSizePercent = 5 % of the heap in
    ///    regions, young regions already there + 1)` (HotSpot's
    ///    `absolute_min_young_length`; "already there" is the young census the
    ///    pause left, the default arm's own accounting). No further shrinking:
    ///    no young length can bring that pause under the goal;
    ///  * base under the goal and the pause over it: 20 % off, as before;
    ///  * the young-dependent part (`pause - base`) under half of what the goal
    ///    leaves it (`goal - base`): 12.5 % back, as before when the base is 0,
    ///    so a pause between 50 % and 100 % of the goal that is mostly base
    ///    can regrow the target (the default loop never did);
    ///  * otherwise unchanged; every arm is lifted to the minimum above.
    ///
    /// The WHOLE pause is used on this arm (the base separates the
    /// young-independent cost itself, as on the model's arm), so
    /// `NET_OF_HEAP_COSTS` does not apply under it. With
    /// `CRATONVM_G1_YOUNG_TARGET_HOTSPOT_MODEL=1` the record never reaches
    /// this function (the model's arm wins), so the two switches do not
    /// compose: the model already has this case and HotSpot's per-region fit.
    /// Read per record (`runtime_flag_on`), not latched, so a test's
    /// `with_thread_overrides` reaches it.
    pub fn update_young_target_base_aware(
        &self,
        collection_type: G1CollectionType,
        pause_us: u64,
        sample_us: u64,
        stats: &GcStats,
        phases: &G1PausePhases,
    ) {
        if !g1_young_target_base_aware() {
            self.update_young_target(collection_type, sample_us, stats);
            return;
        }
        if !gc_flags().g1_young_pause_target {
            return;
        }
        if !matches!(
            collection_type,
            G1CollectionType::YoungOnly | G1CollectionType::Mixed
        ) {
            return;
        }
        // Only young-only pauses teach (a mixed pause's closure covers the old
        // half too); a mixed record is sized from the young base.
        if collection_type == G1CollectionType::YoungOnly {
            self.teach_young_cost_model(pause_us, stats, phases);
        }
        let (min, max) = self.young_region_bounds();
        if young_target_pause_futile(
            stats,
            self.config.region_size,
            g1_young_target_futile_below_region(),
        ) {
            self.young_target_regions.store(max, Ordering::Relaxed);
            return;
        }
        let goal_us = self.config.max_gc_pause_ms.saturating_mul(1_000);
        if goal_us == 0 {
            return;
        }
        let cur = self
            .young_target_regions
            .load(Ordering::Relaxed)
            .clamp(min, max);
        let next = base_aware_young_target(
            cur,
            min,
            max,
            self.young_after_pause_regions.load(Ordering::Relaxed),
            goal_us,
            pause_us,
            self.yt_base_us.load(Ordering::Relaxed),
        );
        self.young_target_regions.store(next, Ordering::Relaxed);
    }

    /// gci i7/g1y — remember how many young regions this young or mixed pause
    /// kept out of its collection set under a pin; see
    /// [`Self::young_pinned_kept_regions`].
    ///
    /// gci i10/dl (`i8-gs-report.md` section 5 item 2): a full collection
    /// stores 0. It empties young, so the count the last young pause left
    /// names regions that no longer exist; kept, it made
    /// `CRATONVM_G1_YOUNG_TARGET_NET_OF_PINS` / `CRATONVM_G1_FREE_TRIGGER_NET_OF_PINS`
    /// subtract them until the next young pause, delaying the next trigger by
    /// up to that many Eden regions. (No production path records
    /// `G1CollectionType::Full` today -- G1's full collection is a marking
    /// cycle plus an armed mixed pause -- so this is latent.)
    pub fn note_young_pinned_kept(&self, collection_type: G1CollectionType, phases: &G1PausePhases) {
        let kept = match collection_type {
            G1CollectionType::YoungOnly | G1CollectionType::Mixed => {
                (phases.jni_pinned_out as usize).saturating_add(phases.jit_pinned_out as usize)
            }
            G1CollectionType::Full => 0,
        };
        self.young_pinned_kept_regions.store(kept, Ordering::Relaxed);
    }

    /// Young regions the last young or mixed pause kept out of its collection
    /// set under a pin. Diagnostics/tests.
    pub fn young_pinned_kept_regions(&self) -> usize {
        self.young_pinned_kept_regions.load(Ordering::Relaxed)
    }

    /// gci i14/yt — remember the young census this record's pause left
    /// behind: the young regions present at its end (Survivor destinations
    /// and every young region a pin kept) and the regions its survivors would
    /// fill packed. Written on every record that carries a census, drain
    /// passes included (a drain pass runs inside the pause and its census is
    /// the later, truer one); a record without a census
    /// (`record_collection`'s all-zero breakdown) keeps the previous values,
    /// except a full collection, which empties young.
    pub fn note_young_after_pause(&self, collection_type: G1CollectionType, phases: &G1PausePhases) {
        if phases.total_regions == 0 {
            if collection_type == G1CollectionType::Full {
                self.young_after_pause_regions.store(0, Ordering::Relaxed);
                self.young_survivor_equiv_regions.store(0, Ordering::Relaxed);
                if self.young_target_hotspot_model {
                    self.note_survivor_census_for_model(0, 0);
                }
            }
            return;
        }
        let after = (phases.eden_regions as usize).saturating_add(phases.surv_regions as usize);
        self.young_after_pause_regions.store(after, Ordering::Relaxed);
        let surv_equiv = survivor_equiv_regions(phases.surv_bytes, self.config.region_size);
        // gci i19/ys: a drain pass runs inside a pause that has already sized
        // the target from its own (earlier) census, and does not re-size it
        // (`teaches_controllers`); its later census can leave more young
        // regions than that target, which would fire the young-target arm
        // with no Eden allocated. Lift only (never lower), as
        // `update_young_target` does for every record it sizes.
        //
        // gci i20/ys: the HotSpot-model arm (default ON since 2026-10-15)
        // needs the same lift in ITS count: right after the pause the arm
        // counts `young - after + surv_equiv` = the packed survivor footprint
        // (`hotspot_young_arm_count`), and `hotspot_young_target` floors every
        // target it computes at `surv_equiv + 1` -- but from the containing
        // pause's census. A drain pass that copied more survivors (an
        // evacuation failure's residue) leaves a larger footprint than that
        // target, so the arm fired on the first poll with no Eden allocated.
        // HotSpot's `absolute_min_young_length` is survivors + 1 whatever
        // pause left them.
        let census = if self.young_target_hotspot_model {
            surv_equiv
        } else {
            after
        };
        if phases.drain_pass && gc_flags().g1_young_pause_target {
            let (min, max) = self.young_region_bounds();
            let _ = self
                .young_target_regions
                .fetch_max(young_target_absolute_min(min, max, census), Ordering::Relaxed);
        }
        self.young_survivor_equiv_regions
            .store(surv_equiv, Ordering::Relaxed);
        // gci i16/pl (finding 12): only the model reads the split.
        if self.young_target_hotspot_model {
            self.note_survivor_census_for_model(phases.surv_regions as usize, phases.surv_bytes);
        }
    }

    /// gci i16/pl (finding 12) — roll the model's Survivor census: what the
    /// last record left becomes what the record being made took in, and
    /// `(regions, bytes)` becomes the last. Called once per censused record,
    /// just before [`Self::update_young_target_predicted`] reads the "in"
    /// half for the same record.
    pub fn note_survivor_census_for_model(&self, regions: usize, bytes: u64) {
        let prev_regions = self.yt_surv_regions_last.swap(regions, Ordering::Relaxed);
        let prev_bytes = self.yt_surv_bytes_last.swap(bytes, Ordering::Relaxed);
        self.yt_surv_regions_in.store(prev_regions, Ordering::Relaxed);
        self.yt_surv_bytes_in.store(prev_bytes, Ordering::Relaxed);
    }

    /// gci i14/yt — teach the HotSpot-shaped young model from one young-only
    /// pause. HotSpot predicts a young pause as a BASE time (card / rset
    /// scan, roots, "other": nothing that scales with the Eden length) plus a
    /// per-region cost (that region's predicted survivors times the copy cost
    /// per byte). Ours, from the phase record:
    ///
    ///  * closure cost per byte of closure WORK: `closure_us + rset_us` over
    ///    `bytes_copied + rset_bytes_scanned`. The closure both copies and
    ///    walks the rset sources; on vi7-g1y J (i10) the ratio held at
    ///    1.4-2.5 ns/byte whether a pause copied 3 KB (the floor regime) or
    ///    46 MB (`CRATONVM_G1_YOUNG_PAUSE_TARGET=0`), so one figure prices both;
    ///  * base = the pause minus the copy at that price (fix-up, rset walk,
    ///    roots, free, verify, other); 183-229 ms median at the floor, 483 ms
    ///    with a 307-region target -- above the 200 ms goal either way;
    ///  * survivor bytes per collection-set region (HotSpot's survival-rate
    ///    predictor).
    ///
    /// Only young-only pauses teach (a mixed pause's closure covers the old
    /// half too; HotSpot keeps separate mixed predictors).
    pub fn teach_young_cost_model(&self, pause_us: u64, stats: &GcStats, phases: &G1PausePhases) {
        let copied = stats.bytes_copied as u64;
        let work = copied.saturating_add(phases.rset_bytes_scanned);
        // `closure_us + rset_us`: before gci i14/yt the parallel young path
        // timed the rset walk inside `closure_us` and reported `rset_us=0`
        // (vi7-g1y np_r1: 477 of 477 parallel young pauses), the serial one
        // timed it apart (114 of 114 serial pauses `rset_us>0`); the sum is
        // the same quantity on both, before and after the seed split.
        // Measure marginal evacuation rate from copied bytes and closure time alone:
        // mixing fixed remembered-set/card-scan cost (`rset_us`) with near-zero
        // `bytes_copied` inflates `yt_work_ps_per_byte` while `young_pause_base_us`
        // only subtracts `copied * ps`, starving `hotspot_young_target`.
        let work = copied;
        let work_us = phases.closure_us;
        if work_us > 0 && work >= YT_MIN_WORK_BYTES {
            let sample = work_us.saturating_mul(1_000_000) / work;
            yt_ema_into(&self.yt_work_ps_per_byte, sample);
        }
        let ps = self.yt_work_ps_per_byte.load(Ordering::Relaxed);
        yt_ema_into(&self.yt_base_us, young_pause_base_us(pause_us, copied, ps));
        // gci i16/pl (finding 12 of
        // `gci-i15ra-g1-evacuation-additions-review-findings-FIXED-20261016`):
        // HotSpot predicts Eden survival per EDEN region and prices the
        // Survivor regions in their own term
        // (`predict_survivor_regions_evac_time`, added to the base in
        // `calculate_young_desired_length`). `bytes_copied` mixes re-copies of
        // older survivors (and their promotions) into the Eden rate and
        // `cset_regions` counts the Survivor regions, so the fused figure
        // over-prices an Eden region as survivors age. The split: Eden's
        // copies are this pause's age-1 survivor bytes (an Eden object is
        // age 0 and is copied, never promoted, at any threshold >= 1; the
        // survivor-age histogram is filled by both evacuators on the
        // publishing arm and consumed only after this record), the rest is
        // Survivor-sourced; the Eden regions are the collection set minus
        // the Survivor regions the last census left. A pause with no
        // histogram (every copy promoted, or a record built by hand) keeps
        // the fused figure.
        let eden_copied = self.survivor_age_bytes[1].load(Ordering::Relaxed) as u64;
        let hist_any = self
            .survivor_age_bytes
            .iter()
            .any(|b| b.load(Ordering::Relaxed) > 0);
        if phases.total_regions > 0 && hist_any {
            let (eden_bytes, surv_bytes) = split_young_copied(copied, eden_copied);
            let eden_regions = young_eden_cset_regions(
                phases.cset_regions,
                self.yt_surv_regions_in.load(Ordering::Relaxed),
            );
            if eden_regions > 0 {
                yt_ema_into(&self.yt_surv_bytes_per_region, eden_bytes / eden_regions);
            }
            let surv_in = self.yt_surv_bytes_in.load(Ordering::Relaxed);
            if surv_in > 0 {
                yt_ema_into(
                    &self.yt_surv_survival_permille,
                    survivor_survival_permille(surv_bytes, surv_in),
                );
            }
            self.yt_split_samples.fetch_add(1, Ordering::Relaxed);
        } else if phases.cset_regions > 0 {
            yt_ema_into(
                &self.yt_surv_bytes_per_region,
                copied / u64::from(phases.cset_regions),
            );
        }
    }

    /// gci i14/yt — `CRATONVM_G1_YOUNG_TARGET_HOTSPOT_MODEL=1`: re-size the
    /// young target from the predictors instead of the 20 %-per-overrun loop
    /// of [`Self::update_young_target`]. Same gates as that loop (the
    /// `g1-young-pause-target` switch, young and mixed records only, the
    /// futile-pause reset to the ceiling); the target is
    /// [`hotspot_young_target`] of the current predictions, clamped to the
    /// ceiling (a survivor footprint at or above the ceiling leaves the arm
    /// inert and the Free arms in charge, as before).
    pub fn update_young_target_predicted(
        &self,
        collection_type: G1CollectionType,
        pause_us: u64,
        stats: &GcStats,
        phases: &G1PausePhases,
    ) {
        if !gc_flags().g1_young_pause_target {
            return;
        }
        if !matches!(
            collection_type,
            G1CollectionType::YoungOnly | G1CollectionType::Mixed
        ) {
            return;
        }
        if collection_type == G1CollectionType::YoungOnly {
            self.teach_young_cost_model(pause_us, stats, phases);
        }
        let (min, max) = self.young_region_bounds();
        if young_target_pause_futile(
            stats,
            self.config.region_size,
            g1_young_target_futile_below_region(),
        ) {
            self.young_target_regions.store(max, Ordering::Relaxed);
            return;
        }
        self.store_predicted_young_target(min, max, g1_young_reserves_mixed_min_old());
    }

    /// gci i22/sz -- the sizing half of
    /// [`Self::update_young_target_predicted`] (the teaching and the futile
    /// reset stay there): store [`hotspot_young_target`] of the current
    /// predictions, clamped to `max`. A zero goal leaves the target alone.
    ///
    /// `mixed_reserve` (`CRATONVM_G1_YOUNG_RESERVES_MIXED_MIN_OLD`, opt-in):
    /// when the next pause is mixed, the base also carries the predicted cost
    /// of the phase's MINIMUM old collection set ([`Self::mixed_min_old_reserve_us`]),
    /// HotSpot 25's `calculate_desired_eden_length_before_mixed`. Off, the
    /// young half takes the whole goal and the old half gets its own budget
    /// on top (finding 4 of
    /// `gci-i20ys-g1-young-model-and-mixed-futile-reset-as-default-review-findings-RETIRED-UNFIXED-20261018`).
    pub fn store_predicted_young_target(&self, min: usize, max: usize, mixed_reserve: bool) {
        let goal_us = self.config.max_gc_pause_ms.saturating_mul(1_000);
        if goal_us == 0 {
            return;
        }
        let reserve_us = if mixed_reserve && self.needs_mixed_gc() {
            self.mixed_min_old_reserve_us()
        } else {
            0
        };
        let ps = self.yt_work_ps_per_byte.load(Ordering::Relaxed);
        let region_ns = young_region_cost_ns(
            self.yt_surv_bytes_per_region.load(Ordering::Relaxed),
            ps,
        );
        // gci i16/pl (finding 12): HotSpot's `survivor_base_time_ms` -- the
        // base plus the predicted copy of the Survivor regions the next pause
        // will take (the census this record left, at the learned Survivor
        // survival). 0 until a split has been taught, i.e. i14/yt's model.
        let surv_term_us = survivor_copy_us(
            self.yt_surv_bytes_last.load(Ordering::Relaxed),
            self.yt_surv_survival_permille.load(Ordering::Relaxed),
            ps,
        );
        let target = hotspot_young_target(
            min,
            max,
            self.young_survivor_equiv_regions.load(Ordering::Relaxed),
            goal_us,
            self.yt_base_us
                .load(Ordering::Relaxed)
                .saturating_add(surv_term_us)
                .saturating_add(reserve_us),
            region_ns,
        );
        // gci i43/gy: under an operator's ceiling HotSpot's clamp is
        // `[absolute_min, max(max_sizer, absolute_min)]` -- a survivor
        // footprint at the ceiling gets one Eden region beyond it, and the
        // binding arm collects there. The default ceiling keeps the cap.
        let target = if self.young_ceiling_binding() {
            target
        } else {
            target.min(max)
        };
        self.young_target_regions.store(target, Ordering::Relaxed);
    }

    /// gci i22/sz -- the predicted cost (us) of the mixed phase's minimum old
    /// collection set, HotSpot 25 `calculate_desired_eden_length_before_mixed`:
    /// `min(calc_min_old_cset_length, remaining candidates)` regions, each
    /// priced at the candidates' mean live bytes (the census `cleanup` took,
    /// [`Self::note_mixed_reserve_census`]) times the old copy price
    /// ([`Self::old_evac_ns_per_byte`]). 0 with no census. Lock-free: the
    /// record path calls it with the regions guard held.
    pub fn mixed_min_old_reserve_us(&self) -> u64 {
        let candidates = self.mixed_reserve_candidates.load(Ordering::Relaxed);
        let selected = self
            .mixed_old_regions_selected
            .load(Ordering::Relaxed)
            .saturating_sub(self.mixed_reserve_selected_base.load(Ordering::Relaxed));
        mixed_min_old_reserve_us(
            candidates,
            selected,
            self.config.mixed_gc_count_target,
            self.mixed_reserve_live_bytes.load(Ordering::Relaxed),
            self.old_evac_ns_per_byte(),
        )
    }

    /// gci i22/sz -- `cleanup`'s candidate census for
    /// [`Self::mixed_min_old_reserve_us`] (switch arm only): the candidate
    /// count and their summed live bytes at the end of the mark, and the
    /// selected-regions counter at that moment (the remaining candidates are
    /// the census minus what the phase has selected since). Candidates as
    /// [`Self::mixed_phase_has_work_within`] counts them.
    pub fn note_mixed_reserve_census(&self, regions: &[G1Region]) {
        let region_size = self.config.region_size;
        let threshold = self.config.mixed_gc_live_threshold_percent as usize;
        let (mut candidates, mut live) = (0usize, 0u64);
        for r in regions {
            if r.region_type() != RegionType::Old
                || r.pinned
                || r.live_bytes == 0
            {
                continue;
            }
            let l = r.mixed_live(self.mixed_hotspot_choice);
            if l * 100 >= region_size * threshold {
                continue;
            }
            candidates += 1;
            live = live.saturating_add(l as u64);
        }
        self.mixed_reserve_candidates
            .store(candidates, Ordering::Relaxed);
        self.mixed_reserve_live_bytes.store(live, Ordering::Relaxed);
        self.mixed_reserve_selected_base.store(
            self.mixed_old_regions_selected.load(Ordering::Relaxed),
            Ordering::Relaxed,
        );
    }

    /// gci i14/yt — the `CRATONVM_G1_SURVIVOR_PLAB` census: `(regions,
    /// chunks, tail_bytes)` over this collector's life.
    pub fn survivor_plab_census(&self) -> (u64, u64, u64) {
        (
            self.survivor_plab_regions.load(Ordering::Relaxed),
            self.survivor_plab_chunks.load(Ordering::Relaxed),
            self.survivor_plab_tail_bytes.load(Ordering::Relaxed),
        )
    }

    /// gci i16/pl — the PLAB's direct allocations (finding 4): `(objects,
    /// bytes)` over this collector's life.
    pub fn survivor_plab_direct_census(&self) -> (u64, u64) {
        (
            self.survivor_plab_direct_objects.load(Ordering::Relaxed),
            self.survivor_plab_direct_bytes.load(Ordering::Relaxed),
        )
    }

    /// gci i25/pl — PLAB claims that waited on a peer's region switch, over
    /// this collector's life (see `survivor_plab_switch_waits`).
    pub fn survivor_plab_switch_waits(&self) -> u64 {
        self.survivor_plab_switch_waits.load(Ordering::Relaxed)
    }

    /// gci i16/pl — the young model's survivor split (finding 12), for the
    /// exit line and tests: `(survival_permille, surv_regions_in,
    /// surv_bytes_in, surv_bytes_last, split_samples)`.
    pub fn young_model_split_state(&self) -> (u64, usize, u64, u64, u64) {
        (
            self.yt_surv_survival_permille.load(Ordering::Relaxed),
            self.yt_surv_regions_in.load(Ordering::Relaxed),
            self.yt_surv_bytes_in.load(Ordering::Relaxed),
            self.yt_surv_bytes_last.load(Ordering::Relaxed),
            self.yt_split_samples.load(Ordering::Relaxed),
        )
    }

    /// gci i14/yt — the HotSpot-shaped model's state, for the exit line and
    /// tests: `(base_us, work_ps_per_byte, surv_bytes_per_region,
    /// survivor_equiv_regions, young_after_pause_regions)`.
    pub fn young_model_state(&self) -> (u64, u64, u64, usize, usize) {
        (
            self.yt_base_us.load(Ordering::Relaxed),
            self.yt_work_ps_per_byte.load(Ordering::Relaxed),
            self.yt_surv_bytes_per_region.load(Ordering::Relaxed),
            self.young_survivor_equiv_regions.load(Ordering::Relaxed),
            self.young_after_pause_regions.load(Ordering::Relaxed),
        )
    }

    /// Recount Free regions under the lock and publish the result.
    ///
    /// Only for callers that do NOT already hold `regions` — everything that
    /// does should call [`Self::publish_free_region_count`] with the count it
    /// already has.
    pub fn recount_free_regions(&self) -> usize {
        let regions = self.regions.read();
        self.publish_region_census(&regions)
    }

    /// Claim the next Free region, resuming the search where the last claim
    /// left off (G1AUD-9 — see [`Self::free_scan_hint`]).
    ///
    /// The caller must hold `regions`. Returns the index WITHOUT retyping it;
    /// the caller decides what the region becomes, which is why the hint is
    /// advanced past the returned index rather than to it.
    pub fn claim_free_region(&self, regions: &[G1Region]) -> Option<usize> {
        let start = self.free_scan_hint.load(Ordering::Relaxed);
        let compaction_avoid = self
            .humongous_compaction_avoid
            .lock()
            .as_ref()
            .map(|reservation| (reservation.start, reservation.end));
        // LANE E wave 2 — under `CRATONVM_G1_HUMONGOUS_RUN_GUARD`, do not hand
        // out a region from the INTERIOR of the heap's longest Free run.
        //
        // Wave 1 made the contiguous search best-fit, which stops a humongous
        // SPAN eating the head of a long run. This is the other half of the
        // same problem and the one best-fit cannot touch: a single-region Old
        // or humongous-start claim landing in the middle of a run of length `n`
        // leaves two runs whose lengths sum to `n - 1`, and a humongous
        // allocation can only ever use the LONGER of them — so the heap's
        // ability to satisfy a large span falls by far more than the one region
        // the claim took. G1 has no compaction pass that can rebuild a run, so
        // that loss is permanent for the life of the process. Taking the END of
        // the run instead costs exactly one region of span capacity.
        //
        // Placement only: both arms return a region that is Free, and neither
        // can fail where the other succeeds. See `run_preserving_free_region`.
        //
        // gci i13/ef (`gci-i13ef-g1-evacuation-failure-pinning-and-heap-sizing-review-findings-RETIRED-RULING-20261018`
        // finding 1): both arms below answer over the WHOLE table, and every
        // region above the committed prefix is Free by construction, so the
        // uncommitted tail is one Free run -- usually the longest. When the
        // prefix holds no Free region outside that run, the guard's fallback
        // ("the run's LAST region") is the top of the reservation, and
        // `commit_through_region` then commits all of `-Xmx` for one Old
        // destination region (`Runtime.totalMemory()` jumps to `maxMemory()`
        // at the first serial pause that promotes). `commit_aware_claim`
        // re-picks such a choice: the lowest Free region that is not in the
        // longest run's interior (either END of that run shortens it, so the
        // guard's property holds), which is a committed region when one
        // exists and the first uncommitted one otherwise.
        let committed_regions =
            (self.arena.committed_len() / self.config.region_size.max(1)).min(regions.len());
        let idx = if let Some(avoid) = compaction_avoid {
            // gci i47w hr47: a reservation guides placement; it must never make
            // the last Free regions unavailable (see
            // `release_compaction_reservation_for_claim`).
            match find_free_region_from_outside(regions, start, avoid) {
                Some(idx) => idx,
                None => {
                    let idx = find_free_region_from(regions, start)?;
                    self.release_compaction_reservation_for_claim(idx);
                    idx
                }
            }
        } else {
            let (found, probed) = run_preserving_free_region(regions, start);
            note_free_scan(probed);
            commit_aware_claim(
                |i| regions[i].region_type() == RegionType::Free,
                regions.len(),
                committed_regions,
                found?,
                true,
            )
        };
        // F-16: a Free region above the committed prefix is address space, not
        // memory. Commit through it before handing it out; a refusal is
        // indistinguishable, to every caller, from there being no free region —
        // which is the truth, and routes to a collection and then to a clean
        // OutOfMemoryError exactly as heap exhaustion always has.
        if !self.commit_through_region(idx) {
            return None;
        }
        // Resume AFTER this region next time: the caller is about to retype it,
        // so it will not be Free on the next probe.
        let next = if idx + 1 >= regions.len() { 0 } else { idx + 1 };
        self.free_scan_hint.store(next, Ordering::Relaxed);
        self.note_free_regions_claimed(1, 0);
        Some(idx)
    }

    /// The parallel evacuator's to-space pool: every Free region, the ones
    /// inside the committed prefix first (ascending), then the uncommitted
    /// tail (ascending), and how many of the leading entries are committed.
    /// See `SharedEvac::claim_pool_slot` for why the split exists.
    pub fn parallel_to_space_pool(&self, regions: &[G1Region]) -> (Vec<usize>, usize) {
        let region_size = self.config.region_size.max(1);
        let committed = (self.arena.committed_len() / region_size).min(regions.len());
        let (mut pool, tail): (Vec<usize>, Vec<usize>) = regions
            .iter()
            .enumerate()
            .filter(|(_, r)| r.region_type() == RegionType::Free)
            .map(|(i, _)| i)
            .partition(|&i| i < committed);
        let pool_committed = pool.len();
        pool.extend(tail);
        (pool, pool_committed)
    }

    /// Ten-findings item 8: claim a Free region for a YOUNG occupant — an Eden
    /// region, or a Survivor evacuation destination — from the TOP of the
    /// committed prefix, searching downward from [`Self::young_scan_hint`].
    ///
    /// [`Self::claim_free_region`] hands out the lowest Free region, and
    /// `find_contiguous_free` looks for a humongous run from index 0, so before
    /// this change Eden regions and humongous spans were both first-fit from
    /// the bottom: every Eden claim between two humongous allocations broke up
    /// the contiguous runs the next humongous allocation needed, and a heap
    /// with a few large buffers fragmented itself. HotSpot separates the two
    /// populations by taking young regions from the tail of the free list and
    /// old/humongous from the head; this is the same split. Old destinations
    /// and humongous spans keep claiming upward from 0, young claims downward
    /// from the top of what is committed, and the two grow toward each other.
    ///
    /// Lazy commit (F-16) is preserved: the prefix grows by one region only
    /// when the committed range holds no Free region at all. The first
    /// uncommitted index is Free by construction — decommit only ever releases
    /// trailing Free regions — so growing is claiming it. `commit_through_region`
    /// is a no-op for an index already inside the prefix and is kept on that
    /// path for the invariant it checks, not for the work it does.
    pub fn claim_free_region_young(&self, regions: &[G1Region]) -> Option<usize> {
        let compaction_avoid = self
            .humongous_compaction_avoid
            .lock()
            .as_ref()
            .map(|reservation| (reservation.start, reservation.end));
        if let Some(idx) = self.claim_free_region_young_avoiding(regions, compaction_avoid) {
            return Some(idx);
        }
        // gci i47w hr47: nothing Free outside the reservation. Placement
        // guidance yields to availability, as `claim_humongous_span_locked`'s
        // fallback already does -- see `release_compaction_reservation_for_claim`.
        if compaction_avoid.is_none() {
            return None;
        }
        let idx = self.claim_free_region_young_avoiding(regions, None)?;
        self.release_compaction_reservation_for_claim(idx);
        Some(idx)
    }

    /// gci i47w hr47 (`gci-i47w-ob47-g1-32m-headroom-search-dies-in-an-infallible-allocation-FIXED-20261016`):
    /// a region claim found no Free region OUTSIDE the allocation-failure
    /// compaction's reservation and took `idx` from the whole table instead.
    /// Drop the reservation when `idx` lies inside it: the window is no longer
    /// whole, so it no longer holds what it was kept for.
    ///
    /// Before, both region claims searched only outside the window and failed
    /// when nothing was Free there. The reservation is released only by a
    /// humongous claim of its window, and its owner's fallible retry is refused
    /// at the TLAB reserve BEFORE it reaches that claim whenever the window
    /// leaves fewer than `requested + reserve` Free regions (the default arming
    /// asks only for `requested`). So on G1 `-Xmx32m` a refused humongous probe
    /// left a reservation covering every Free region behind it, every Eden claim
    /// after it failed with 30 regions Free, the ladder threw OutOfMemoryError
    /// out of the probe's `println`, and the next infallible allocation (152
    /// bytes) aborted the VM. HotSpot has no such reservation: a Free region is
    /// always available to a mutator. Pause-side claims take the same fallback
    /// only where they would otherwise fail (the plan sized the window so that
    /// its blockers fit outside it).
    pub fn release_compaction_reservation_for_claim(&self, idx: usize) {
        let mut reservation = self.humongous_compaction_avoid.lock();
        if reservation
            .as_ref()
            .is_some_and(|r| (r.start..r.end).contains(&idx))
        {
            *reservation = None;
        }
    }

    /// [`Self::claim_free_region_young`] with the compaction window it must
    /// avoid passed in (`None`: the whole table).
    pub fn claim_free_region_young_avoiding(
        &self,
        regions: &[G1Region],
        compaction_avoid: Option<(usize, usize)>,
    ) -> Option<usize> {
        let region_size = self.config.region_size.max(1);
        let committed = (self.arena.committed_len() / region_size).min(regions.len());
        // LANE E (2026-09-20) — do not pay an exhaustive downward scan to learn
        // something the maintained census already knows.
        //
        // While the heap is GROWING, every region inside the committed prefix
        // is occupied by construction, so the scan below probes the whole
        // prefix, fails, and the claim grows the prefix by one. The next claim
        // probes a prefix one longer. That is quadratic in the region count
        // over a growth phase — on a 2048-region heap, ~2.1M region-type reads
        // under the EXCLUSIVE guard to fill the heap once — and it is exactly
        // the shape `free_scan_hint` was introduced to remove from
        // `claim_free_region`, reintroduced in the downward twin because the
        // hint cannot encode "there is nothing below me".
        //
        // The census can. Every region at or above `committed` is Free by
        // construction (a region is only ever typed after being committed, and
        // `uncommit_trailing_free_regions` releases only a TRAILING run of Free
        // regions), so the Free regions inside the prefix are
        // `free_region_count - (len - committed)`. Zero of them means the scan
        // cannot succeed.
        //
        // Applied ONLY while the prefix can still grow. `free_region_count` is
        // a cache with a pause-end backstop, so it can read low between a pause
        // that freed regions and the census that republishes it; taking the
        // shortcut then costs one extra region of commit and nothing else,
        // because the growth arm below still hands back a real Free region.
        // With the prefix already covering the table there is no growth arm to
        // fall through to, and a low cache would turn into a spurious "no free
        // region" — i.e. a spurious collection or OOM. So in that state the
        // exhaustive scan always runs, which is also the state in which it is
        // cheap relative to the alternative (there is no commit to skip).
        let prefix_can_grow = committed < regions.len();
        let free_in_prefix = self
            .free_region_count
            .load(Ordering::Relaxed)
            .saturating_sub(regions.len() - committed);
        let scan_can_succeed = !prefix_can_grow || free_in_prefix > 0;
        if committed > 0 && scan_can_succeed {
            let top = committed - 1;
            let hint = self.young_scan_hint.load(Ordering::Relaxed).min(top);
            let found = match compaction_avoid {
                Some(avoid) => find_free_region_down_from_outside(regions, hint, top, avoid),
                None => find_free_region_down_from(regions, hint, top),
            };
            if let Some(idx) = found {
                if !self.commit_through_region(idx) {
                    return None;
                }
                // Resume BELOW this region next time; it is about to be retyped.
                self.young_scan_hint
                    .store(idx.checked_sub(1).unwrap_or(top), Ordering::Relaxed);
                self.note_free_regions_claimed(1, 1);
                return Some(idx);
            }
        }
        // Nothing Free inside the prefix: grow it by one region.
        if prefix_can_grow {
            // The compactor may be targeting presently-uncommitted Free
            // regions. Commit through that window if necessary, but never use
            // one as to-space; otherwise the very evacuation intended to make
            // the span would punch a new hole into it.
            let idx = match compaction_avoid {
                Some((start, end)) if (start..end).contains(&committed) => end,
                _ => committed,
            };
            if idx >= regions.len() {
                return None;
            }
            if regions[idx].region_type() != RegionType::Free {
                // The "everything above the prefix is Free" invariant the
                // shortcut above rests on has been violated. Fail-safe rather
                // than assert: fall back to the exhaustive scan we may have
                // skipped, so a claim that COULD have been served still is.
                // (A `debug_assert!` here would abort inside a pause, which
                // the G1-11 note in the audit is explicit about.)
                let top = committed.saturating_sub(1);
                if committed > 0 && !scan_can_succeed {
                    if let Some(idx) = find_free_region_down_from(regions, top, top) {
                        if !self.commit_through_region(idx) {
                            return None;
                        }
                        self.young_scan_hint
                            .store(idx.checked_sub(1).unwrap_or(top), Ordering::Relaxed);
                        self.note_free_regions_claimed(1, 1);
                        return Some(idx);
                    }
                }
                return None;
            }
            if !self.commit_through_region(idx) {
                return None;
            }
            // The regions below `idx` were just proven occupied, so the next
            // search may as well start at the new top.
            self.young_scan_hint.store(idx, Ordering::Relaxed);
            self.note_free_regions_claimed(1, 1);
            return Some(idx);
        }
        None
    }

    /// Item 8 — the placement policy in one place: young occupants from the
    /// top, everything else from the bottom.
    #[inline]
    pub fn claim_free_region_for(&self, regions: &[G1Region], occupant: RegionType) -> Option<usize> {
        match occupant {
            RegionType::Eden | RegionType::Survivor => self.claim_free_region_young(regions),
            _ => self.claim_free_region(regions),
        }
    }

    /// F-11 — how many Eden stripes this collector allocates through.
    ///
    /// `CRATONVM_G1_EDEN_STRIPES=n` pins it; `=1` is the single global Eden the
    /// collector had before F-11. Otherwise it is the hardware parallelism,
    /// capped at an eighth of the heap's regions so striping can never eat a
    /// small heap: at 8 regions or fewer the cap gives 1, which is exactly the
    /// pre-F-11 shape, so no small-heap behaviour changes by default.
    ///
    /// Not `parallel_worker_count()`: that is a GC-worker ergonomic sized for
    /// pause work, and this is a MUTATOR-side fan-out. Tying them would make
    /// `CRATONVM_G1_WORKERS=1` — a determinism knob for the evacuator — quietly
    /// serialise allocation as well.
    pub fn eden_stripe_count(num_regions: usize) -> usize {
        if let Some(n) = gc_flags().g1_eden_stripes {
            return n.max(1);
        }
        let par = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(1)
            .max(1);
        par.min((num_regions / 8).max(1)).max(1)
    }

    /// The Eden stripe this thread allocates from.
    ///
    /// Assigned once per OS thread from a process-global round-robin and then
    /// cached, so the choice costs one thread-local load on the allocation fast
    /// path. Round-robin rather than a hash of the thread id because thread ids
    /// are not uniformly distributed and a collision costs exactly the
    /// contention this striping exists to remove. The counter is global rather
    /// than per-collector on purpose — it is a spreading device, not an
    /// identity, and a thread that outlives one collector should keep landing
    /// on a different stripe from its neighbours in the next one.
    #[inline]
    pub fn eden_slot(&self) -> usize {
        let stripes = self.eden_slots.len();
        if stripes == 1 {
            return 0;
        }
        thread_local! {
            static SLOT: std::cell::Cell<usize> = const { std::cell::Cell::new(usize::MAX) };
        }
        static NEXT_SLOT: AtomicUsize = AtomicUsize::new(0);
        let raw = SLOT.with(|c| {
            let cached = c.get();
            if cached != usize::MAX {
                return cached;
            }
            let assigned = NEXT_SLOT.fetch_add(1, Ordering::Relaxed);
            c.set(assigned);
            assigned
        });
        raw % stripes
    }

    /// Retire every Eden stripe whose region this pause is about to collect.
    ///
    /// The single-slot version of this was five copies of
    /// `if cset.contains(&cur) { store(usize::MAX) }`, one per collection path.
    /// Striping turns each of them into a loop, and a path that forgot to
    /// retire a stripe would keep bump-allocating into a region the pause had
    /// just reset — so there is one implementation and the five sites call it.
    pub fn retire_eden_slots_in_cset(&self, cset: &RegionSet) {
        for slot in self.eden_slots.iter() {
            let idx = slot.load(Ordering::Relaxed);
            if idx != usize::MAX && cset.contains(&idx) {
                slot.store(usize::MAX, Ordering::Relaxed);
            }
        }
    }

    /// Retire every Eden stripe unconditionally. Test/diagnostic use.
    #[cfg(test)]
    pub fn clear_eden_slots(&self) {
        for slot in self.eden_slots.iter() {
            slot.store(usize::MAX, Ordering::Relaxed);
        }
    }

    /// The region this thread is currently allocating into, if any.
    /// Test/diagnostic use — production code reads the slot it is about to
    /// bump, so that it and the bump see the same region.
    #[cfg(test)]
    pub fn dbg_current_eden(&self) -> usize {
        self.eden_slots[self.eden_slot()].load(Ordering::Relaxed)
    }

    /// Reset the Free-region scan hint to the head of the table.
    ///
    /// Called when a collection has just rebuilt the pool: the freed regions
    /// are overwhelmingly *behind* the hint (the CSet is the young generation,
    /// which the previous cycle filled front-to-back), so restarting the scan
    /// keeps allocation packed at the low indices instead of making every claim
    /// pay a full wrap. Purely a search-order choice — `find_free_region_from`
    /// is exhaustive either way.
    #[inline]
    pub fn reset_free_scan_hint(&self) {
        self.free_scan_hint.store(0, Ordering::Relaxed);
        // Item 8: `usize::MAX` reads as "the top of the committed prefix" in
        // `claim_free_region_young`, which clamps it.
        self.young_scan_hint.store(usize::MAX, Ordering::Relaxed);
    }

    /// Native-allocation pressure signal — see the field doc. Consumed at the
    /// `safe_native_call` boundary via `VmHeap::young_spill_pressure`.
    #[inline]
    pub fn native_alloc_pressure(&self) -> bool {
        self.native_alloc_pressure.load(Ordering::Relaxed)
    }

    /// Clear the native-allocation pressure latch.
    #[inline]
    pub fn clear_native_alloc_pressure(&self) {
        self.native_alloc_pressure.store(false, Ordering::Relaxed);
        self.serial_recovery_requested.store(false, Ordering::Relaxed);
    }

    /// Latch the native-allocation pressure signal from outside the collector
    /// (the `VmHeap::note_young_spill_pressure` funnel used by allocation
    /// wrappers that spilled and cannot collect themselves).
    #[inline]
    pub fn note_native_alloc_pressure(&self) {
        self.native_alloc_pressure.store(true, Ordering::Relaxed);
        // JIT r23 w7: an external note is a recovery request; the next young
        // pause takes the serial route for it (see `young_collection`).
        self.serial_recovery_requested.store(true, Ordering::Relaxed);
    }

    /// Account for retained off-heap GPU VRAM and arm `native_alloc_pressure`
    /// when external pacing policy thresholds are crossed.
    #[inline]
    pub fn note_external_gpu_retained(&self, delta_bytes: i64, wrapper_delta: i64) -> bool {
        self.external_gpu
            .apply_signed_delta(delta_bytes, wrapper_delta, &self.native_alloc_pressure)
    }

    /// Free regions held back from *speculative bulk* TLAB refills once the
    /// pool runs low, so the last of the heap serves real object allocations
    /// (which cannot be deferred) instead of TLAB tails that may never be
    /// used. Defense-in-depth for the native-alloc safepoint gap: it widens
    /// the window in which the `safe_native_call` boundary GC can still fire
    /// before the infallible allocator is cornered. `refill_tlab` returning
    /// `None` is an already-handled path (the caller falls back to per-object
    /// allocation), so this can never wedge allocation.
    pub fn tlab_reserve_regions(&self, total_regions: usize) -> usize {
        // Capped at an eighth of the heap so a very small heap (the unit
        // tests' 2-region collectors, an embedded `-Xmx4m`) is never starved
        // of TLABs by its own reserve — at 8 regions or fewer the cap decides,
        // and below 8 regions the reserve is zero.
        //
        // LANE E wave 2, small finding 5: that boundary used to be written as
        // "below 8 regions the reserve is zero" in the sentence above AND as
        // "at 8 regions or fewer" here, which cannot both be true. The
        // expression is `min(total / 8)`, so at exactly 8 regions the reserve
        // is 1 and it is 0 only strictly below 8. Harmless either way — the
        // reserve gates only SPECULATIVE TLAB refills, which have an
        // always-available per-object fallback — but an off-by-one boundary in
        // a comment is a trap for whoever next reasons about a small heap from
        // it rather than from the code.
        (total_regions / 64)
            .max(self.tlab_reserve_floor)
            .min(total_regions / 8)
    }

    /// Bump-allocate `size` bytes in the current Eden region.
    /// Returns `(pointer, region_index)` or `None` on failure.
    ///
    /// # F-11 — the common case runs under a SHARED guard
    ///
    /// This used to take the exclusive regions lock unconditionally, for every
    /// object, because bumping a `usize` cursor needed `&mut G1Region`. With an
    /// atomic cursor the overwhelmingly common case — there is a current Eden
    /// region and it has room — is a compare-exchange under `read()`, so N
    /// allocating threads no longer serialise, and neither do they exclude the
    /// concurrent marker (which also reads under `read()` since F-10).
    ///
    /// The exclusive guard is still taken for the two things that genuinely
    /// mutate the region TABLE rather than a region's cursor: a humongous span
    /// (which reserves a run of Free regions and retypes them) and claiming a
    /// fresh Eden (which retypes one, and must not hand the same Free slot to
    /// two claimers). Those are per-REGION events — one per megabyte of
    /// allocation at the default region size — not per-object ones.
    ///
    /// After queueing for the write guard the current-Eden probe is repeated,
    /// because a thread that got there first may have already installed a fresh
    /// region with room in it; without that re-check a burst of threads all
    /// arriving at an exhausted Eden would burn one Free region each.
    ///
    /// `CRATONVM_G1_SHARED_ALLOC=0` skips the shared fast path, so every
    /// allocation goes down the exclusive route the collector used before
    /// F-11. That arm is deliberately not a separate code path: it is the same
    /// slow path, entered without the fast probe.
    pub fn alloc_in_region(&self, size: usize) -> Option<(*mut u8, usize)> {
        self.alloc_in_region_initialized(size, |_| {})
    }

    /// [`Self::alloc_in_region`], with `init` run over the fresh span while the
    /// regions lock is still held.
    ///
    /// Every caller that writes an `ObjectHeader` should use this rather than
    /// writing it after the plain form returns, so the header cannot be
    /// forgotten at one of four call sites and the span is decodable the moment
    /// the allocator hands it back. See
    /// [`G1Region::bump_alloc_initialized`] for what this does and does not
    /// close: under F-11 the claim IS the publication, so this moves the
    /// caller's header store inside the allocator rather than reordering it
    /// against the cursor.
    pub fn alloc_in_region_initialized(
        &self,
        size: usize,
        init: impl Fn(*mut u8),
    ) -> Option<(*mut u8, usize)> {
        // ONE-BINARY A/B. `CRATONVM_G1_LATE_HEADER_WRITE=1` restores the old
        // arrangement -- the header written by the CALLER after the regions
        // lock is dropped -- on the same binary, so the change can be bisected
        // against a workload without a second build. Default OFF.
        if g1_late_header_write() {
            let result = self.alloc_in_region_locked(size, &|_| {});
            if let Some((ptr, _)) = result {
                init(ptr);
            }
            return result;
        }
        self.alloc_in_region_locked(size, &init)
    }

    /// The body of [`Self::alloc_in_region_initialized`]: takes the regions
    /// lock and runs `init` over the fresh span before publishing it.
    pub fn alloc_in_region_locked(
        &self,
        size: usize,
        init: &dyn Fn(*mut u8),
    ) -> Option<(*mut u8, usize)> {
        // NO GUARD HERE. Each arm below takes the guard it needs -- the
        // humongous claim exclusive then downgraded, the Eden fast path shared,
        // the slow path exclusive. An outer guard held across them deadlocks:
        // `parking_lot`'s RwLock is not reentrant, and this function held a
        // write guard while dev's F-10/F-11 arms asked for their own.
        let region_size = self.config.region_size;

        // Humongous check. Retypes a run of Free regions: exclusive.
        if size > region_size / 2 {
            let mut regions = self.regions.write();
            let (start, needed) = self.claim_humongous_span_locked(&mut regions, size)?;
            // A humongous span consumes several regions at once — always the
            // largest single bite out of the Free pool.
            self.note_region_consumed_locked(&regions);
            // Ten-findings item 7: the span is zeroed under the SHARED guard.
            // A 100 MB array used to be memset while every other allocating
            // thread waited on the exclusive lock; now they carve their TLABs
            // beside it. The claim above typed the regions with the start
            // region's cursor still 0, so nothing admitted under the shared
            // guard can walk into the span before the cursor is published
            // below — and a pause, which is the only thing that walks regions
            // wholesale, needs the exclusive guard and waits for this one.
            let regions = parking_lot::RwLockWriteGuard::downgrade(regions);
            let start_addr = regions[start].data.addr();
            unsafe {
                // SAFETY: `[start_addr, start_addr + needed*region_size)` is a
                // run of adjacent arena slices this claim just typed and
                // committed, and `size <= needed*region_size`.
                std::ptr::write_bytes(start_addr as *mut u8, 0, size);
            }
            // The header BEFORE the cursor. Here that ordering is real and
            // dev's own claim protocol provides it: the span was typed with the
            // start region's cursor still 0, so nothing under the shared guard
            // can walk into it until `set_cursor` below. This is the one
            // allocation path where "init before publication" is literally true.
            init(start_addr as *mut u8);
            regions[start].set_cursor(size);
            // Humongous bytes count toward the IHOP occupancy statistic; the
            // next pause's recompute replaces this running total.
            //
            // LANE E (2026-09-20) — in the UNIT the recompute uses. With
            // `CRATONVM_G1_IHOP_COUNTS_REGIONS` on, `recompute_old_gen_bytes`
            // charges a humongous span one full `region_size` per region it
            // occupies (occupancy is regions consumed, not bytes live in
            // them); this running total charged the object's byte count, so
            // between a humongous allocation and the next pause the two
            // disagreed by up to `region_size - 1` per region — always in the
            // under-counting direction, i.e. the direction that delays the
            // concurrent mark cycle that is the ONLY thing which reclaims dead
            // spans. The flag arms now agree at every instant, not just at
            // pause boundaries.
            let occupancy = if gc_flags().g1_ihop_counts_regions {
                needed.saturating_mul(region_size)
            } else {
                size
            };
            self.old_gen_bytes.fetch_add(occupancy, Ordering::Relaxed);
            return Some((start_addr as *mut u8, start));
        }

        // Fast path: this thread's Eden stripe, shared guard, atomic claim.
        let slot = self.eden_slot();
        {
            let regions = self.regions.read();
            let cur = self.eden_slots[slot].load(Ordering::Relaxed);
            // F-16: see `carve_tlab_from_eden_slot` — the commit check guards
            // every write into a region, on both guards.
            if cur < regions.len()
                && regions[cur].region_type() == RegionType::Eden
                && self.commit_through_region(cur)
            {
                if let Some(result) =
                    regions[cur].bump_alloc_initialized(size, 8, "obj:cur-eden", &init)
                {
                    self.alloc_shared_claims.fetch_add(1, Ordering::Relaxed);
                    self.note_eden_bytes(size);
                    return Some((result.0, cur));
                }
            }
        }

        // Slow path: no Eden for this stripe, or it is full. Exclusive.
        let mut regions = self.regions.write();
        self.alloc_exclusive_claims.fetch_add(1, Ordering::Relaxed);

        // Try this stripe's Eden region — re-probed under the write guard
        // because another thread on the same stripe may have installed a fresh
        // one while we queued (and because with the fast path off this is the
        // ONLY probe).
        let cur = self.eden_slots[slot].load(Ordering::Relaxed);
        if cur < regions.len()
            && regions[cur].region_type() == RegionType::Eden
            && self.commit_through_region(cur)
        {
            if let Some(result) =
                regions[cur].bump_alloc_initialized(size, 8, "obj:cur-eden", &init)
            {
                self.note_eden_bytes(size);
                return Some((result.0, cur));
            }
        }

        // Find a new free region for this stripe's Eden — from the top of the
        // committed prefix (item 8).
        if let Some(idx) = self.claim_free_region_young(&regions) {
            debug_free_region_cursor(&regions[idx], idx, "alloc_in_region");
            regions[idx].set_region_type(RegionType::Eden);
            self.eden_slots[slot].store(idx, Ordering::Relaxed);
            self.note_region_consumed_locked(&regions);
            if let Some(result) =
                regions[idx].bump_alloc_initialized(size, 8, "obj:fresh-eden", &init)
            {
                self.note_eden_bytes(size);
                return Some((result.0, idx));
            }
        }

        None
    }

    /// Item 7 — the exclusive half of a humongous allocation: find and type a
    /// contiguous run of `size.div_ceil(region_size)` Free regions, commit
    /// through it, and account the regions as consumed. The start region's
    /// cursor is deliberately left at 0: every walker bounds itself by the
    /// cursor, so a span whose cursor is unpublished holds no objects to any
    /// reader admitted under the shared guard. The caller zeroes the span
    /// under that guard and then publishes the cursor.
    ///
    /// Returns `(start_index, regions_needed)`.
    ///
    /// # The layout this claim establishes
    ///
    /// The object occupies ONE physically-contiguous block: the regions are
    /// adjacent slices of [`G1Collector::arena`], so a span of `regions_needed`
    /// consecutive region indices is `regions_needed * region_size` contiguous
    /// bytes. The real `ObjectHeader` lives at offset 0 of the start region and
    /// the payload flows straight through the following regions — exactly like
    /// any other array, just larger. This is what lets the JIT (and
    /// JNI-critical / Unsafe / arraycopy) address every element with flat
    /// `base + HEADER_SIZE + i*stride` arithmetic.
    ///
    /// Layout / bookkeeping:
    /// * `start` is `HumongousStart` with `cursor = size` (the full object
    ///   size). Heap walkers iterating `[0, cursor)` therefore read the single
    ///   humongous object once — reads cross region boundaries safely because
    ///   the arena is contiguous — then stop.
    /// * Continuation regions are `HumongousContinuation` with `cursor = 0`, so
    ///   walkers skip them (their bytes are the start object's payload, NOT
    ///   independent objects). No per-region header prefix and no
    ///   `HumongousFiller` sentinel: those would corrupt the contiguous
    ///   payload. (`is_humongous_filler` is now never true for live data and
    ///   stays only as a defensive no-op in the walkers.)
    ///
    /// Replaces the previous region-fragmented layout (each region a separate
    /// `Vec<u8>` with its own HEADER_SIZE prefix), which was safe for the
    /// GC-internal region-aware accessors but invisible to the JIT's flat
    /// addressing — a humongous element past the first region read/wrote
    /// unrelated memory (silent zero tail at ~1–2 MB, SIGSEGV at ~4 MB+).
    pub fn claim_humongous_span_locked(
        &self,
        regions: &mut Vec<G1Region>,
        size: usize,
    ) -> Option<(usize, usize)> {
        let region_size = self.config.region_size;
        if region_size == 0 || size < HEADER_SIZE {
            return None;
        }

        // Number of contiguous regions whose combined bytes hold the whole
        // object (header + payload). Contiguity in the arena makes this a
        // single block, so we size by the FULL object, not a per-region chunk.
        let regions_needed = size.div_ceil(region_size).max(1);

        // LANE E wave 2 (proposal P6) — count the request BEFORE the search, so
        // the denominator exists whether or not the search succeeds.
        self.note_humongous_request();

        let reservation = *self.humongous_compaction_avoid.lock();
        // The allocation-failure compactor has already made this exact span
        // available. Its owner first searches the reservation; a different
        // humongous request first searches around it as small/young claims do.
        // That prevents a post-pause helper allocation from punching the run
        // apart between compaction and this retry.
        let preferred_start = match reservation {
            Some(reservation) if regions_needed == reservation.requested_regions => {
                find_contiguous_free(&regions[reservation.start..reservation.end], regions_needed)
                    .map(|offset| reservation.start + offset)
            }
            Some(reservation) => find_contiguous_free_outside(
                regions,
                regions_needed,
                (reservation.start, reservation.end),
            ),
            None => find_contiguous_free(regions, regions_needed),
        };
        // Reservations guide placement; they cannot make available memory
        // unavailable to a different request, or strand the owner's retry if
        // its original window was not completely evacuated. This fallback
        // runs only when the preferred placement cannot satisfy the request.
        let start = preferred_start.or_else(|| find_contiguous_free(regions, regions_needed));
        let Some(start) = start else {
            // A failed humongous allocation and a failed ordinary allocation
            // are indistinguishable to everything above the collector: both
            // arms of `alloc_in_region_locked` return `None` and this `?` was
            // silent. So an `OutOfMemoryError` on a heap with 40 % of its
            // regions Free — the signature of humongous FRAGMENTATION, which no
            // amount of extra collection can fix because this collector has no
            // compaction pass — read exactly like running out of memory.
            //
            // The walk is O(regions) and runs only here, on a path that is
            // about to fail an allocation. `longest_run < regions_needed` with
            // a comfortable `free` is the fragmentation diagnosis, stated
            // rather than inferred.
            let (free, longest) = free_region_shape(regions);
            self.note_humongous_failure(free, longest, regions_needed);
            return None;
        };
        // F-16: the span crosses `regions_needed` regions and the object's
        // payload flows straight through them, so every one must be committed
        // before a byte is written. Committing through the LAST covers them all
        // (the prefix rule), and a refusal is a failed humongous allocation.
        if !self.commit_through_region(start + regions_needed - 1) {
            let (free, longest) = free_region_shape(regions);
            self.note_humongous_failure(free, longest, regions_needed);
            return None;
        }

        // Classify the span. `cursor = 0` on continuations makes walkers skip
        // them; the start region's cursor becomes `size` only once the span is
        // zeroed (see `alloc_in_region`).
        regions[start].set_region_type(RegionType::HumongousStart);
        regions[start].set_cursor(0);
        for i in 1..regions_needed {
            regions[start + i].set_region_type(RegionType::HumongousContinuation);
            regions[start + i].set_cursor(0);
        }
        if reservation.is_some_and(|reservation| {
            regions_needed == reservation.requested_regions
                || (start < reservation.end && start + regions_needed > reservation.start)
        }) {
            // The owner retried, or another request consumed the window.
            // Release the reservation only
            // after all regions have been retyped, so another allocator can
            // never observe a partially-claimed target window.
            *self.humongous_compaction_avoid.lock() = None;
        }
        self.last_failed_humongous_span.store(0, Ordering::Release);
        self.fragmented_slide_span.store(0, Ordering::Release);
        self.note_free_regions_claimed(regions_needed, 0);
        Some((start, regions_needed))
    }

    // LANE E wave 2, small finding 3 — `alloc_humongous_locked` is GONE.
    //
    // It was `#[allow(dead_code)]` and really was dead: its doc claimed it
    // "exists for callers that already hold the exclusive guard", and there
    // were none — `alloc_in_region_locked` does the claim-zero-publish sequence
    // itself so it can zero under the SHARED guard (item 7). The only reason it
    // was load-bearing was that five doc comments cited it as the authority on
    // humongous layout; those now name `claim_humongous_span_locked`, which
    // carries the layout under its own `# The layout this claim establishes`
    // heading. Deleting it removes a second, divergent description of the
    // sequence that nothing exercised and nothing could have caught drifting.

    /// The evacuation allocation cursor for `target_type` (G1AUD-9 — see
    /// [`Self::evac_dest_survivor`]). `None` for any other region type, which
    /// is what keeps the hint from ever being consulted for a type the
    /// evacuator does not allocate into.
    #[inline]
    pub fn evac_dest_hint(&self, target_type: RegionType) -> Option<&AtomicUsize> {
        match target_type {
            RegionType::Survivor => Some(&self.evac_dest_survivor),
            RegionType::Old => Some(&self.evac_dest_old),
            _ => None,
        }
    }

    /// Drop both evacuation allocation cursors.
    ///
    /// Called at the start of every pause, before anything is reclassified: a
    /// hint from the previous pause names a region that may now be Free, in the
    /// CSet, or re-typed. The validation in [`Self::alloc_in_type_locked`]
    /// would reject it anyway — this just keeps the first object of each pause
    /// from paying for a guaranteed miss, and keeps the field's meaning
    /// ("somewhere this pause has already placed an object") honest.
    #[inline]
    pub fn reset_evac_dest_hints(&self) {
        self.evac_dest_survivor.store(usize::MAX, Ordering::Relaxed);
        self.evac_dest_old.store(usize::MAX, Ordering::Relaxed);
    }

    /// gci i10/rh finding 1 -- an evacuation copy of `size` bytes landed in `region`:
    /// count it when the region is Old and carries mark data, i.e. when the mixed
    /// choice will read these bytes as garbage (default arm).
    #[inline]
    pub fn note_promoted_into_marked(&self, region: &G1Region, size: usize) {
        if region.region_type() == RegionType::Old && region.live_bytes > 0 {
            self.promoted_into_marked_bytes
                .fetch_add(size as u64, Ordering::Relaxed);
            self.promoted_into_marked_copies.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Allocate in a region of the specified type (Survivor or Old).
    pub fn alloc_in_type_locked(
        &self,
        regions: &mut Vec<G1Region>,
        target_type: RegionType,
        size: usize,
        cset: &RegionSet,
    ) -> Option<*mut u8> {
        // G1AUD-9 — try the region the previous object of this type landed in
        // before scanning. Validated exactly as the scan below validates a
        // candidate (right type, not in the CSet, `bump_alloc` succeeds), so a
        // hit is indistinguishable from what the scan would have returned and a
        // miss costs one region-type read. See `evac_dest_survivor` for the
        // cost this removes.
        let hint_cell = self.evac_dest_hint(target_type);
        if let Some(cell) = hint_cell {
            let hint = cell.load(Ordering::Relaxed);
            if hint < regions.len()
                && regions[hint].region_type() == target_type
                && !cset.contains(&hint)
                && self.commit_through_region(hint)
            {
                if let Some((ptr, _)) = regions[hint].bump_alloc(size, 8, "evac:hint-dest") {
                    self.note_promoted_into_marked(&regions[hint], size);
                    return Some(ptr);
                }
            }
        }
        let placed = Self::alloc_in_type_locked_scan(regions, target_type, size, cset, self);
        if let Some((_, idx)) = placed {
            self.note_promoted_into_marked(&regions[idx], size);
        }
        if let (Some(cell), Some((_, idx))) = (hint_cell, placed) {
            cell.store(idx, Ordering::Relaxed);
        }
        placed.map(|(ptr, _)| ptr)
    }

    /// The exhaustive half of [`Self::alloc_in_type_locked`]: scan for any
    /// non-CSet region of `target_type` with room, else claim a Free one.
    /// Returns the pointer AND the region it came from so the caller can record
    /// the allocation cursor.
    pub fn alloc_in_type_locked_scan(
        regions: &mut Vec<G1Region>,
        target_type: RegionType,
        size: usize,
        cset: &RegionSet,
        this: &Self,
    ) -> Option<(*mut u8, usize)> {
        // CORRECTNESS (evacuation destination must NOT be in the collection set):
        // a young GC evacuates *all* Survivor regions, so a partially-filled
        // Survivor region is itself in the CSet; a mixed GC likewise has selected
        // Old regions in the CSet. Reusing such a region as an evacuation
        // *destination* copies survivors into a region that Phase 5 then resets
        // (frees) — the copies are lost and every reference to them is left
        // dangling (silent live-object loss; the held-tree `got=1` repro). Skip
        // any CSet region here: young survivors land only in fresh Free regions,
        // mixed promotions only in non-CSet Old or fresh Free regions — matching
        // the semi-space "never allocate into from-space" invariant the
        // generational collector and the Step-9 parallel TLAB path already honour.
        for i in 0..regions.len() {
            if regions[i].region_type() == target_type
                && !cset.contains(&i)
                && this.commit_through_region(i)
            {
                if let Some((ptr, _)) = regions[i].bump_alloc(size, 8, "evac:cur-dest") {
                    return Some((ptr, i));
                }
            }
        }

        // gci i23/tn (`CRATONVM_G1_SURVIVOR_REGION_CAP`): no fresh Survivor
        // region past the pause's cap (HotSpot's `is_survivor_full`); the
        // caller copies the object into Old.
        if target_type == RegionType::Survivor && !this.survivor_cap_admits_region() {
            return None;
        }
        // Allocate a new free region (Free regions are never in the CSet).
        if let Some(idx) = this.claim_free_region_for(regions, target_type) {
            debug_free_region_cursor(&regions[idx], idx, "alloc_in_type_locked");
            regions[idx].set_region_type(target_type);
            if target_type == RegionType::Survivor {
                regions[idx].set_age(1);
            }
            if let Some((ptr, _)) = regions[idx].bump_alloc(size, 8, "evac:fresh-dest") {
                return Some((ptr, idx));
            }
        }

        None
    }

    /// The regions a pause keeps: every region holding a self-forwarded
    /// object, i.e. an identity entry of `pointer_map`. [`Self::free_or_keep_cset`]
    /// frees every other CSet region, and [`Self::retire_forwards`] retires
    /// only inside these.
    pub fn self_forwarded_regions(&self, pointer_map: &cratonvm_types::PointerMap) -> RegionSet {
        self.identity_keys(pointer_map)
            .into_iter()
            .filter_map(|k| self.lookup_region_for_addr(k))
            .collect()
    }

    /// g1pin: fill around the kept pins -- every byte of each `pin_only`
    /// region below its cursor that is not one of its identity-forwarded
    /// (pinned) objects becomes filler. Such a region is kept only so its
    /// pinned objects stay where they are;
    /// everything else in it moved or died, as in a region the pause frees, so
    /// O(pins) fillers replace the per-object walk a real evacuation failure
    /// takes (on `BinTreesClassic 18` that walk was ~30 ms a pause: a map
    /// lookup per object of every kept 8 MB Eden region).
    pub fn fill_around_kept_pins(
        &self,
        regions: &[G1Region],
        pin_only: &RegionSet,
        pointer_map: &cratonvm_types::PointerMap,
    ) {
        if pin_only.len() == 0 {
            return;
        }
        let keys = self.identity_keys(pointer_map);
        {
            let mut filled = self.pin_filled_regions.lock();
            for idx in pin_only.iter() {
                filled.insert(idx);
            }
        }
        for idx in pin_only.iter() {
            let region = &regions[idx];
            let base = region.data.as_ptr() as usize;
            let end = base + region.cursor();
            let first = keys.partition_point(|&k| k < base);
            let last = keys.partition_point(|&k| k < end);
            let mut at = base;
            for &obj in &keys[first..last] {
                // SAFETY: `[at, obj)` is dead space of a kept CSet region below
                // its cursor, inside this stop-the-world pause.
                if obj > at {
                    unsafe { cratonvm_gc_core::heap::write_filler(at, obj - at) };
                }
                // SAFETY: `obj` is a pinned object start whose header
                // `retire_forwards` restored.
                at = obj + unsafe { object_total_size(&*(obj as *const ObjectHeader)) };
            }
            if end > at {
                // SAFETY: as above.
                unsafe { cratonvm_gc_core::heap::write_filler(at, end - at) };
            }
        }
    }

    /// g1pin: the regions of `failed` that hold an identity forward other than
    /// a peer-stack object pin, i.e. a real evacuation failure. Without pins
    /// this is `failed` itself, with no pass over the map.
    pub fn to_space_failed_regions(
        &self,
        failed: &RegionSet,
        pointer_map: &cratonvm_types::PointerMap,
    ) -> RegionSet {
        if failed.is_empty() || self.peer_object_pin_count.load(Ordering::Acquire) == 0 {
            return failed.clone();
        }
        self.identity_keys(pointer_map)
            .into_iter()
            .filter(|&k| !self.peer_object_pinned(k))
            .filter_map(|k| self.lookup_region_for_addr(k))
            .collect()
    }

    /// Phase 5: free evacuated CSet regions — EXCEPT those that hold a
    /// self-forwarded (evacuation-failed) object, which must be KEPT so the
    /// still-live object that could not be relocated is not freed.
    ///
    /// A self-forwarded object is recorded as an identity entry (`key == value`)
    /// in the forwarding map by [`Self::evacuate_object`] (and the parallel
    /// evacuator) when to-space is exhausted. Young failed regions become Old,
    /// so a completed mark can reclaim their
    /// dead objects without needing another evacuation destination. Originals
    /// of objects that DID move are replaced with reference-free fillers before
    /// the region is kept. Returns bytes freed only from regions actually reset.
    ///
    /// At adequate heaps no evacuation fails, so `failed` is empty and this is
    /// exactly the old "reset every CSet region" behaviour, without the
    /// kept-region loops' passes over the map.
    pub fn free_or_keep_cset(
        &self,
        regions: &mut Vec<G1Region>,
        cset: &[usize],
        pointer_map: &cratonvm_types::PointerMap,
    ) -> usize {
        // gci i39/cv -- opt-in `CRATONVM_G1_DBG_FREED_SLOT_AUDIT`: snapshot the
        // CSet's rsets before the resets below clear them; the audit runs at
        // the end of this function (see `freed_slot_audit`).
        let freed_slot_pre = if g1_dbg_freed_slot_audit() {
            Some(FreedSlotAuditPre::take(&regions[..], cset.iter().copied(), cset))
        } else {
            None
        };
        // Regions that hold at least one self-forwarded (in-place) object.
        // `lookup_region_for_addr` consults the immutable region table, so it
        // does not borrow `regions` (no conflict with the mutable loop below).
        let failed = self.self_forwarded_regions(pointer_map);

        // F-15 — an evacuation failure IS "the heap filled before the
        // collector could reclaim it", and it is the only direct observation of
        // that available from inside a pause. Feed it to adaptive IHOP so the
        // next cycle starts earlier. See `note_to_space_exhausted`.
        // g1pin: the kept regions that hold a REAL evacuation failure, not
        // only peer-stack object pins. Only these signal a short to-space and
        // become Old; a region kept only for pins stays young (below).
        let to_space_failed = self.to_space_failed_regions(&failed, pointer_map);
        if !to_space_failed.is_empty() && gc_flags().g1_adaptive_ihop {
            self.note_to_space_exhausted();
        }

        let mut bytes_freed = 0usize;
        // G1AUD-5: every pause bumps `rset_cache_epoch` before it reclassifies
        // anything, so this reads the generation this pause owns.
        let generation = self.rset_generation();
        // The map proves that these originals are dead; every live slot has
        // already been remapped and the original's forwarding mark retired.
        // Keeping their old reference payloads makes a subsequent RSet walk
        // treat a dead duplicate as a source into freed/reused regions.
        // (No failed region, nothing to fill: skip the pass over the map.)
        // gci i13/ef finding 10: membership as a bitset, built once. g1pin:
        // the kept CSet regions' forwards only, by key range
        // (`for_each_forward_in_regions`), not a pass over every forward.
        //
        // g1pin: a region kept only for peer-stack pins holds nothing live but
        // its pinned objects, so it is filled around them below
        // (`fill_around_kept_pins`) instead of taking these two passes.
        // (A region holding a frozen peer's TLAB tail is never here:
        // `jit_tlab_skip_regions` pins it out of the collection set.)
        let jit_skips = self.jit_tlab_skip_spans();
        let pin_only: RegionSet = cset
            .iter()
            .copied()
            .filter(|i| failed.contains(i) && !to_space_failed.contains(i))
            .collect();
        let kept_cset: RegionSet = cset
            .iter()
            .copied()
            .filter(|i| failed.contains(i) && !pin_only.contains(i))
            .collect();
        self.for_each_forward_in_regions(pointer_map, &kept_cset, |from, to| {
            if from == to {
                return;
            }
            let size = unsafe { object_total_size(&*(from as *const ObjectHeader)) };
            // SAFETY: from is an evacuated object start in a kept CSet region,
            // and its complete, aligned allocation belongs to this STW pause.
            unsafe { cratonvm_gc_core::heap::write_filler(from, size) };
        });
        // A failed region also contains unreachable originals that never
        // entered the forwarding map. Their slots still point into this
        // pause's freed from-space. Once the region becomes Old, a later
        // region-level RSet walk would follow those stale slots, even when a
        // fresh mark has not yet covered the promotion. Clear the dead edges
        // now, using this evacuation's completed reachability closure.
        for &idx in cset {
            if !kept_cset.contains(&idx) {
                continue;
            }
            let region = &regions[idx];
            let base = region.data.as_ptr() as usize;
            let cursor = region.cursor();
            let mut offset = 0;
            while offset < cursor {
                let addr = base + offset;
                if let Some(skip) = jit_tlab_skip_span_len(&jit_skips, addr) {
                    offset += skip;
                    continue;
                }
                if let Some(gap) = gap_filler_len(addr as *const u8) {
                    offset += gap;
                    continue;
                }
                let header = unsafe { &*(addr as *const ObjectHeader) };
                if is_humongous_filler(header) {
                    break;
                }
                let size = object_total_size(header);
                if size < cratonvm_types::MIN_OBJECT_SIZE || size > cursor - offset {
                    break;
                }
                if pointer_map.get(&addr) != Some(&addr) && !is_tlab_filler_header(header) {
                    unsafe { clear_dead_object_reference_edges(addr, header, size) };
                }
                offset += size;
            }
        }
        self.fill_around_kept_pins(regions, &pin_only, pointer_map);
        for &cset_idx in cset {
            if failed.contains(&cset_idx) && !to_space_failed.contains(&cset_idx) {
                // g1pin: kept only for peer-stack object pins. It stays young,
                // as `relabel_pinned_eden_within` keeps a pinned-out Eden region
                // (see there for why Survivor is safe), so the next young pause
                // takes it again and frees its dead objects; as Old they would
                // wait for a mark, and the next pauses would walk the region
                // wholesale as a remembered-set source and in the fix-up.
                if regions[cset_idx].region_type() == RegionType::Eden {
                    regions[cset_idx].set_region_type(RegionType::Survivor);
                    regions[cset_idx].set_age(1);
                }
            } else if failed.contains(&cset_idx) {
                if matches!(
                    regions[cset_idx].region_type(),
                    RegionType::Eden | RegionType::Survivor
                ) {
                    regions[cset_idx].set_region_type(RegionType::Old);
                    // This promotion has no completed mark's liveness data.
                    regions[cset_idx].live_bytes = 0;
                    regions[cset_idx].gc_efficiency = 0.0;
                    regions[cset_idx].cursor_at_cleanup = 0;
                }
            } else {
                if gc_flags().g1_dbg_reach {
                    eprintln!(
                        "[g1][FREED] evac region={cset_idx} type={:?} cursor={:#x}",
                        regions[cset_idx].region_type(),
                        regions[cset_idx].cursor()
                    );
                }
                bytes_freed += regions[cset_idx].cursor();
                if regions[cset_idx].region_type() == RegionType::Old {
                    self.old_regions_reclaimed.fetch_add(1, Ordering::Relaxed);
                }
                regions[cset_idx].reset(generation);
            }
        }

        // Kept objects were scanned as CSet members, so the ordinary Phase-4
        // rebuild skipped them. They are now Old sources for the next young
        // pause, including callers that drive young_collection directly.
        //
        // g1sz/g1pin: the identities from the pause's own list
        // (`identity_keys`). The loop used to visit every forward of the
        // pause, 540 k of them per young pause on `BinTreesClassic 18`.
        for from in self.identity_keys(pointer_map) {
            // Retyping the region is also a promotion for the JIT's
            // inline stores: they test the header's OLD_GEN flag before
            // recording an Old -> young edge. Without this stamp, stores
            // made after an evacuation failure bypass the remembered set.
            let kept_old = self
                .lookup_region_for_addr(from)
                .is_some_and(|idx| regions[idx].region_type() == RegionType::Old);
            // g1pin: a region kept young (pins only) is in the next young
            // set, so its outgoing edges need no remembered-set entry.
            if !kept_old {
                continue;
            }
            unsafe { (&*(from as *const ObjectHeader)).add_gc_flags(GC_FLAG_OLD_GEN) };
            self.record_outgoing_rset_edges(regions, from);
        }

        // LANE B / lane-E finding — REPUBLISH THE FREE CENSUS. This is the one
        // funnel every occupied->Free transition in an evacuation pause goes
        // through, and until now nothing on this side of the cache existed.
        //
        // `free_region_count` is a MAINTAINED cache: `note_free_regions_claimed`
        // decrements it on every Free->occupied transition, and
        // `publish_region_census` re-derives it. The increment side was simply
        // missing — the only production republish points are
        // `GarbageCollector::collect_garbage` (via `recount_free_regions`) and
        // `cleanup`. So any caller that drives `young_collection` /
        // `mixed_collection` DIRECTLY — `g1_concurrent.rs`'s
        // `let _ = g1.young_collection(..)` is one such caller in the tree
        // today — left the cache reading the PRE-pause value until the
        // `NEEDS_GC_RECOUNT_INTERVAL` backstop in `needs_gc` fired.
        //
        // Every consumer of that cache reads a stale-LOW value as "the heap is
        // tight", and in all four that is the pause-storm direction: `needs_gc`
        // keeps answering `true` so the next allocation asks for a pause the
        // previous one already satisfied; `note_region_consumed_locked`
        // re-latches `native_alloc_pressure`; `refill_tlab`'s emergency reserve
        // refuses TLABs and funnels every thread through the exclusive
        // per-object allocator; and `mixed_phase_has_work`'s `tight` waiver
        // waives the `heap_waste_percent` floor. None of that is a correctness
        // bug — the backstop bounds the staleness — but it makes "the trigger
        // the collector cannot satisfy" reachable for a reason that has nothing
        // to do with the collector's policy.
        //
        // The full census rather than a `fetch_add(n)`: it is one pass over a
        // region table this function has just walked anyway (no object is
        // touched), it fixes the `young_region_count` half at the same time —
        // which the young->Old retype above has just changed — and it makes
        // the cache EXACT at the one moment in a pause when exactness is
        // cheapest. A `fetch_add` would only be exact if nothing else in the
        // pause had moved a region between types, and the loop above does
        // exactly that.
        //
        // NOT in `record_collection_with_phases`: every driver reaches that
        // function still holding the regions guard as a WRITER, and
        // `parking_lot`'s `RwLock` is not reentrant, so `regions.read()` there
        // deadlocks.
        //
        // The measurement behind the default: `w2b_free_census_is_exact_after_a_
        // direct_young_collection` in `gc/tests/g1_w2b_free_census.rs` asserts
        // the cache equals a fresh count of the table after a pause driven
        // straight through `young_collection`. It FAILS without this call. A
        // cache that is demonstrably wrong is not a behaviour worth defaulting
        // to, so this ships ON; `CRATONVM_G1_FREE_CENSUS_ON_RECLAIM=0` restores
        // the stale-until-backstop behaviour for bisection.
        // gci i13/ef finding 4 (`CRATONVM_G1_PINNED_YOUNG_AS_SURVIVOR`, default
        // OFF): before the census, so the census counts the retype.
        self.relabel_pinned_eden_within(regions, cset, self.pinned_young_as_survivor);
        if free_census_on_reclaim() {
            self.publish_region_census(regions);
        }
        // gci i39/cv -- every slot that still names a region this pass reset.
        if let Some(pre) = freed_slot_pre.as_ref() {
            let _ = freed_slot_audit(self, &regions[..], pre, "evac", Some(pointer_map));
        }
        bytes_freed
    }

    /// gci i13/ef finding 4: at the end of a young or mixed pause's Phase 5,
    /// retype every Eden region the pause did NOT collect -- by construction a
    /// region a pin (JNI critical or a conservative JIT root) kept out of the
    /// CSet, since every unpinned Eden region is in a young or mixed CSet --
    /// to `Survivor`, so that no Eden region outlives a pause. HotSpot's
    /// `-Xlog:gc+heap` prints `Eden regions: a->0` after every young pause,
    /// and `G1 Eden Space`'s collection usage reads 0; with the region left
    /// Eden this VM printed `a->b`, b > 0.
    ///
    /// Why Survivor and not Old (HotSpot retypes a JEP 423 pinned young region
    /// Old, as an evacuation failure): Survivor is young, so the region stays
    /// in the next young CSet and every invariant a young region carries
    /// holds unchanged -- no `GC_FLAG_OLD_GEN` stamps for the JIT's inline
    /// barrier test, no remembered-set entries for the region's outgoing
    /// edges, no mark data, and its dead objects never become an Old region's
    /// stale holders. Old would need all of that for every object of the
    /// region (`record_outgoing_rset_edges`, the header stamp) -- a design,
    /// not a relabel.
    ///
    /// What makes it safe: Survivor regions receive objects only inside pauses
    /// (the overflow rescan relies on it), so a region that still holds a
    /// frozen peer's un-retired TLAB tail (`jit_tlab_skip_spans`, published
    /// for this pause) is left Eden -- that peer resumes bump-allocating into
    /// it. Mutator Eden claims check `region_type() == Eden` before
    /// allocating into their stripe's region, under the regions guard this
    /// pause holds exclusively, so after the retype no mutator allocates into
    /// it; the next pause's survivor scan may (it is a non-CSet Survivor with
    /// room, the shape `alloc_in_type_locked_scan` already uses).
    ///
    /// Returns the regions retyped. The opt-in is a parameter, as for
    /// `uncommit_trailing_free_regions_within`.
    pub fn relabel_pinned_eden_within(
        &self,
        regions: &[G1Region],
        cset: &[usize],
        enabled: bool,
    ) -> usize {
        if !enabled {
            return 0;
        }
        let skips = self.jit_tlab_skip_spans();
        let cset_members: RegionSet = cset.iter().copied().collect();
        let mut relabelled = 0usize;
        for (i, r) in regions.iter().enumerate() {
            if r.region_type() != RegionType::Eden || cset_members.contains(&i) {
                continue;
            }
            let base = r.data.as_ptr() as usize;
            let end = base + r.data.len();
            if skips.iter().any(|&(s, e)| s < end && e > base) {
                continue;
            }
            r.set_region_type(RegionType::Survivor);
            r.set_age(1);
            relabelled += 1;
        }
        if relabelled > 0 {
            self.pinned_eden_relabelled
                .fetch_add(relabelled as u64, Ordering::Relaxed);
        }
        relabelled
    }

    /// gci i13/ef: the `[GC-STAT]` suffix for findings 4 and 9 --
    /// ` pinned_eden_relabelled=N` under `CRATONVM_G1_PINNED_YOUNG_AS_SURVIVOR`,
    /// ` survivor_overflow_promotions=N` under
    /// `CRATONVM_G1_SURVIVOR_OVERFLOW_TO_OLD`; empty with both off.
    pub fn gci_i13_ef_note(&self) -> String {
        let mut note = String::new();
        if self.pinned_young_as_survivor {
            note.push_str(&format!(" pinned_eden_relabelled={}", self.pinned_eden_relabelled()));
        }
        if self.survivor_overflow_to_old {
            note.push_str(&format!(
                " survivor_overflow_promotions={}",
                self.survivor_overflow_promotions()
            ));
        }
        // gci i23/tn: the Survivor region cap's census, once it has engaged
        // (`CRATONVM_G1_SURVIVOR_REGION_CAP`; absent on a run it never bit).
        let cap_promotions = self.survivor_cap_promotions();
        if cap_promotions > 0 {
            note.push_str(&format!(
                " survivor_cap_promotions={cap_promotions} survivor_cap_pauses={}",
                self.survivor_cap_pauses()
            ));
        }
        note
    }

    /// gci i13/ef finding 4: Eden regions retyped Survivor by
    /// `CRATONVM_G1_PINNED_YOUNG_AS_SURVIVOR` so far. Diagnostics/tests.
    pub fn pinned_eden_relabelled(&self) -> u64 {
        self.pinned_eden_relabelled.load(Ordering::Relaxed)
    }

    /// gci i13/ef finding 9: Survivor copies `CRATONVM_G1_SURVIVOR_OVERFLOW_TO_OLD`
    /// placed in Old so far. Diagnostics/tests.
    pub fn survivor_overflow_promotions(&self) -> u64 {
        self.survivor_overflow_promotions.load(Ordering::Relaxed)
    }

    /// Evacuation-failure recovery (the kept-region death-spiral fix).
    ///
    /// A young/mixed pass that exhausts its to-space pool self-forwards every
    /// remaining reached object, KEEPING each such object's region wholesale —
    /// including all the garbage those regions hold. Under allocation churn
    /// the free pool at trigger time is small (`needs_gc` fires at <25% free),
    /// so once young-live exceeds the pool a single pass can convert most of
    /// the heap into kept, mostly-garbage regions; every later collection then
    /// finds even fewer free regions and keeps even more, monotonically, until
    /// each pass reports `objects_copied == 0 && bytes_freed == 0` and the
    /// mutator dies on a heap that is largely garbage. Observed
    /// deterministically on the SteadyChurn recreation at `-Xmx16m` (serial
    /// G1, --nojit): from the 4th collection onward nothing is copied or
    /// freed, and the program aborts with a corrupted OOM-path throwable.
    ///
    /// Recovery: drain the self-forwarded (identity-forwarded) objects with a
    /// MINIMAL live-only pass — evacuate exactly those objects (plus any
    /// still-kept objects they transitively reference), rewrite all heap/root
    /// references through the drain's map, and free the kept regions that are
    /// now fully drained. Loop while progress is made and identities remain.
    ///
    /// The seeds are live BY CONSTRUCTION (only reached objects self-forward),
    /// and their slots were already rewritten in place when the failing pass
    /// scanned them, so the drain never touches a dead object and never walks
    /// a region wholesale. (Two earlier designs failed here: re-running full
    /// young collections re-copied the live set several times per pause —
    /// inflating `gc_age` until the whole churn set promoted — and any
    /// whole-region source walk resurrects dead objects' targets, a
    /// compounding "undead" population that permanently filled Old.)
    ///
    /// Healthy collections (no evacuation failure) pay one `pointer_map`
    /// scan; no drain runs.
    pub fn retry_after_evacuation_failure(
        &self,
        first: GcResult,
        roots: &mut [ObjectRef],
        monitors: &dyn MonitorCleanup,
    ) -> GcResult {
        // Hard cap: each productive drain frees at least one region (growing
        // the pool for the next), so real recoveries converge quickly.
        const MAX_EVAC_RETRY_PASSES: usize = 8;

        // Diagnostic kill-switch (bisection aid): CRATONVM_G1_NO_EVAC_RETRY=1
        // restores the single-pass behaviour (and with it, the kept-region
        // death spiral under to-space exhaustion).
        if gc_flags().g1_no_evac_retry {
            return first;
        }

        // g1pin: a peer-stack pinned object's identity forward is not an
        // evacuation failure. It must not move (the drain would copy it), and
        // it says nothing about the to-space pool the policy below resizes.
        let identities = |m: &cratonvm_types::PointerMap| -> Vec<usize> {
            let mut keys = self.identity_keys(m);
            keys.retain(|&k| !self.peer_object_pinned(k));
            keys
        };

        // Reset the unresolved-kept tracking for this pause; repopulated
        // below iff the drain gives up with seeds remaining.
        self.clear_kept_unresolved();

        let mut acc = first;
        let mut seeds = identities(&acc.pointer_map);
        if !seeds.is_empty() {
            // The drain below moves objects after the pause's overlay phase
            // judged which rows hold young references; the next pause reads
            // every row instead of trusting that judgement (`g1_overlay.rs`).
            self.overlay_rset_invalidate();
        }
        if seeds.is_empty() {
            // Clean pause: decay the adaptive trigger back toward baseline so
            // healthy workloads keep the larger eden.
            let cur = self.needs_gc_free_percent.load(Ordering::Relaxed);
            if cur > 25 {
                self.needs_gc_free_percent
                    .store((cur - 5).max(25), Ordering::Relaxed);
            }
            return acc;
        }
        // Evacuation failure: the free pool at trigger time was too small for
        // the live-young set. Trigger the NEXT collection earlier so its
        // to-space pool is larger (capped at 50% of the heap).
        //
        // gci i18/mx -- under `CRATONVM_G1_YOUNG_SIZES_TO_RESERVE` the lever
        // is the survival PREDICTION instead (HotSpot sizes young from its
        // survivor prediction and keeps `G1ReservePercent`): the headroom arm
        // then bounds the next young generation by `free - reserve` at 100 %
        // survival, and decays as `note_young_survival` measures. The ratchet
        // shrank every later young generation toward a quarter of Free on a
        // filling heap (gci i16/oo finding 1).
        if self.young_sizes_to_reserve {
            self.young_survival_permille.store(1000, Ordering::Relaxed);
        } else {
            let cur = self.needs_gc_free_percent.load(Ordering::Relaxed);
            self.needs_gc_free_percent
                .store((cur + 8).min(50), Ordering::Relaxed);
        }

        // LANE B, 2026-09-20 — WHY THE PER-PASS FIX-UP IS NOT HOISTED OUT OF
        // THIS LOOP, although `compose_forward_maps` below builds exactly the
        // composed map a single hoisted fix-up would need.
        //
        // The proposal (`lane-b-drain-fixup-is-still-whole-heap.md` §4) was to
        // run one fix-up over the composed map after the loop, "with no change
        // to what any slot ends up holding". It does not hold, and the reason
        // is an ADDRESS COLLISION that this loop creates on purpose.
        //
        // Each pass ends with `free_or_keep_cset`, which RESETS the kept
        // regions it fully drained. That is not incidental — it is the loop's
        // entire engine: a drain pass's to-space comes from
        // `alloc_in_type_locked_scan`, which claims Free regions, and the only
        // new Free regions inside this pause are the ones the previous pass
        // just reclaimed. Remove the per-pass free and pass 2 has exactly the
        // to-space pass 1 exhausted, so it self-forwards the same seeds,
        // `progressed` is false, and the recovery stops at one pass.
        //
        // So a region freed by pass 1 is re-typed and bump-allocated into by
        // pass 2, from cursor ZERO. The first object pass 2 copies into it
        // lands at the region's base — which is, with near certainty, also a
        // KEY of pass 1's forwarding map, because pass 1's from-space objects
        // in that same region started at that same base. After the loop, one
        // address `X` therefore carries two live meanings:
        //
        //   * a slot never fixed up by pass 1 holds `X` as a FROM-SPACE address
        //     and must be rewritten to `acc[X]`;
        //   * a slot `scan_and_evacuate_refs` rewrote during pass 2 holds `X`
        //     as the TO-SPACE address of a live object and must be left alone.
        //
        // `ForwardLookup::resolve` cannot tell them apart — it screens on "is
        // this address in a CSet region" and then probes the map — so a single
        // composed fix-up either corrupts the second kind (rewriting a live
        // pointer to a pass-1 target) or drops the first (leaving a genuine
        // dangling slot). Dropping the aliased keys does not escape it: both
        // readings are simultaneously live.
        //
        // The per-pass fix-up is what removes the ambiguity, by running BEFORE
        // that pass's free and therefore before any address can acquire the
        // second meaning. `compose_forward_maps`' own doc already records the
        // recycling ("the `next` key is a pass-1-freed address recycled as
        // later-pass to-space"); it is safe THERE because roots name
        // pause-start addresses only, and heap slots do not have that property.
        //
        // What the measurement says about the cost this would have saved: on
        // `HumongousChurn 48 6000 512 -Xmx160m --nojit`, the reproducer that
        // actually enters this path, the loop runs ONE pass. Eight walks is a
        // bound, not an observation. The 109 ms that pass does spend in the
        // fix-up is a WIDE-walk cost, not a repetition cost, and the lever for
        // it is `narrow_drain_fixup`. `DRAIN_FIXUP_WALKS` and its three
        // siblings are the counters that make the repetition visible if a
        // different workload ever does reach it.
        let mut passes = 1usize;
        while !seeds.is_empty() && passes < MAX_EVAC_RETRY_PASSES {
            let drain = self.drain_kept_self_forwards(&seeds, roots, monitors);
            if gc_flags().g1_dbg_reach {
                eprintln!(
                    "[g1][RETRY] drain pass={} seeds={} copied={} freed={} forwards={}",
                    passes + 1,
                    seeds.len(),
                    drain.stats.objects_copied,
                    drain.stats.bytes_freed,
                    drain.pointer_map.len(),
                );
            }
            let progressed = drain.stats.objects_copied > 0 || drain.stats.bytes_freed > 0;
            seeds = identities(&drain.pointer_map);
            self.compose_forward_maps(&mut acc.pointer_map, &drain.pointer_map);
            acc.stats.objects_copied += drain.stats.objects_copied;
            acc.stats.bytes_copied += drain.stats.bytes_copied;
            acc.stats.bytes_freed += drain.stats.bytes_freed;
            passes += 1;
            if !progressed {
                break; // genuinely wedged: the live set does not fit (true OOM)
            }
        }
        if !seeds.is_empty() {
            // The drain gave up (wedge / pass cap) with live self-forwarded
            // objects still parked in kept regions. Two follow-ups keep the
            // heap coherent until a later pause resolves them:
            //
            // (a) G1CORE-4: their ref slots were rewritten IN PLACE to
            //     to-space addresses by the failing pass — GC-internal edges
            //     no mutator barrier ever recorded. A kept OLD region is not
            //     re-collected automatically (unlike kept Eden→Survivor), so
            //     without remembered-set entries the next young pause never
            //     scans it as a source and frees its live young referents.
            //     Record each seed's outgoing cross-region edges now.
            //
            // (b) G1CORE-3: record the kept regions + their live addresses
            //     so `is_addr_in_live_region` reports the regions' DEAD
            //     bodies (never scanned, slots never rewritten) as dead —
            //     otherwise post-GC reference processing restores weak
            //     referents whose fields dangle into same-pause-freed
            //     regions.
            {
                let mut regions = self.regions.write();
                let mut rejected: Vec<usize> = Vec::new();
                for &seed in &seeds {
                    if !self.record_outgoing_rset_edges(&mut regions, seed) {
                        rejected.push(seed);
                    }
                }
                // A seed is an identity entry in the pause's own forwarding map,
                // so "it is not an object" is a statement about the map, not
                // just about the heap. Ask the map what else it says about the
                // address before the pause ends and the answer is gone: an
                // address that is ALSO another key's destination was evacuated
                // INTO, which makes the identity entry stale rather than a live
                // self-forward. One O(map) pass, only on a pause that already
                // rejected something, and only while the warnings are still
                // being printed.
                if !rejected.is_empty()
                    && KEPT_SEED_REJECTED.load(Ordering::Relaxed) <= rejected.len().max(8)
                {
                    for seed in rejected {
                        let inbound = acc
                            .pointer_map
                            .iter()
                            .filter(|(k, v)| **v == seed && **k != seed)
                            .count();
                        // Deliberately does NOT consult `kept_unresolved_*`:
                        // those are behind their own mutexes and this runs
                        // under the regions lock — the deadlock hazard
                        // `candidate_header_is_plausible` already documents.
                        let bytes = self
                            .lookup_region_for_addr(seed)
                            .and_then(|i| regions.get(i))
                            .map(|r| {
                                hexdump_around(
                                    r.data.as_ptr() as *mut u8,
                                    r.cursor(),
                                    seed.wrapping_sub(r.data.as_ptr() as usize),
                                )
                            })
                            .unwrap_or_default();
                        // Provenance. A seed that is an INTERIOR pointer had to
                        // enter the pause as one, and there are only two doors:
                        // the root array the caller handed us, and the
                        // conservative JIT-root snapshot.
                        let in_roots = roots.iter().any(|r| r.as_ptr() as usize == seed);
                        let jit_roots = cratonvm_gc_core::gc_quiescence::pinned_jit_roots_snapshot();
                        let in_jit_roots = jit_roots.contains(&seed);
                        tracing::warn!(
                            "[g1] rejected seed 0x{seed:x}: seeds={} map={} inbound_forwards={inbound} \
                             passes={passes} in_roots={in_roots} roots={} in_jit_roots={in_jit_roots} \
                             jit_roots={} {bytes}",
                            seeds.len(),
                            acc.pointer_map.len(),
                            roots.len(),
                            jit_roots.len(),
                        );
                    }
                }
            }
            let mut kept_regions = self.kept_unresolved_regions.lock();
            let mut kept_live = self.kept_unresolved_live.lock();
            for &seed in &seeds {
                if let Some(idx) = self.lookup_region_for_addr(seed) {
                    kept_regions.insert(idx);
                }
                kept_live.insert(seed);
            }
            self.kept_unresolved_any.store(true, Ordering::Release);
        }
        // Overwrites this pause's young/mixed record on purpose: when a drain
        // ran, the drain is what the operator needs to see, and `kind` names
        // it unambiguously. A wedged drain is the one G1 state that silently
        // converts most of the heap into kept, mostly-garbage regions, so it
        // must never be invisible.
        let mut degraded = cratonvm_gc_core::gc_metrics::g1_degraded::EVACUATION_FAILURE;
        if !seeds.is_empty() {
            degraded |= cratonvm_gc_core::gc_metrics::g1_degraded::EVACUATION_FAILURE_UNRESOLVED;
        }
        cratonvm_gc_core::gc_metrics::record_g1_cycle(
            cratonvm_gc_core::gc_metrics::g1_cycle_kind::KEPT_REGION_DRAIN,
            0,
            0,
            0,
            0,
            degraded,
        );
        acc
    }

    /// Record remembered-set edges for every cross-region reference held by
    /// the (live, in-place) object at `obj_addr` — the GC-internal
    /// counterpart of `post_write_barrier_rset` for slots the collector
    /// itself rewrote. See the unresolved-kept block in
    /// [`Self::retry_after_evacuation_failure`].
    ///
    /// Returns `false` iff the seed was refused as not-an-object, so the caller
    /// can report on the ones it rejected.
    pub fn record_outgoing_rset_edges(&self, regions: &mut [G1Region], obj_addr: usize) -> bool {
        let Some(src_idx) = self.lookup_region_for_addr(obj_addr) else {
            return false;
        };
        let obj_ptr = obj_addr as *mut u8;
        // `lookup_region_for_addr` only answers "is this word inside some
        // region's SPAN". That is not "is this an object", and the seeds are
        // not trustworthy enough for the difference to be ignorable: they are
        // the self-forwarded keys of a FAILED evacuation's pointer map, and
        // the same pause's ref-scan is on record rejecting holders out of that
        // very set ("evacuation ref-scan REJECTED a non-object HOLDER"). A
        // header read at a non-object address yields an arbitrary `num_slots`
        // / `array_length`, and the walks below then read past the region — the
        // measured H2 crash was exactly this: `collect_garbage` ->
        // `retry_after_evacuation_failure` -> `record_outgoing_rset_edges` ->
        // `for_each_flat_object_reference_trusting_header` faulting on a 4 MiB-aligned address
        // well past the committed arena.
        //
        // Same screen the ref-scan sibling applies to its own holders, and the
        // same one the reachability verifier gained in `1ccaf9caf`. Dropping a
        // non-object seed costs nothing: an address that is not an object has
        // no outgoing references to remember.
        if !self.candidate_header_is_plausible(regions, obj_addr) {
            let n = KEPT_SEED_REJECTED.fetch_add(1, Ordering::Relaxed) + 1;
            if n <= 8 || n.is_power_of_two() {
                let why = self.describe_rejected_address(regions, obj_addr);
                tracing::warn!(
                    "[g1] kept-seed rset recording REJECTED a non-object SEED (#{n}): \
                     obj=0x{obj_addr:x} {why} — walking its slots would have read outside any \
                     live region. Skipped; the pause continues."
                );
            }
            return false;
        }
        // SAFETY: `candidate_header_is_plausible` validated the tag bytes and
        // placed the address inside a live region's committed span.
        let header = unsafe { &*(obj_ptr as *const ObjectHeader) };
        // Clamp a reference array's element walk to what the holder's own
        // region actually holds, for the same reason `scan_and_evacuate_refs`
        // does: `array_length` is a u32 bounded only by `i32::MAX`, so
        // `HEADER_SIZE + len * 8` is not implied to be inside the region by the
        // header being plausible. Computed here, before `record` borrows
        // `regions` mutably.
        let walkable_elements = if header.kind() == ObjectKind::Array
            && header.element_type() == ArrayElementType::Reference
        {
            self.holder_walkable_slots(regions, obj_ptr, header.array_length() as usize, 8)
        } else {
            0
        };
        // Same clamp, same reason, for the NON-array holder. The comment above
        // argues `array_length` is not bounded by a plausible header — that is
        // equally true of `num_slots`, whose 16-byte `SLOT_SIZE` stride leaves
        // the region twice as fast, and the flat walk was the one in the
        // recorded crash path. Computed here for the same borrow reason.
        let walkable_slots = if header.kind() == ObjectKind::Array {
            0
        } else {
            self.holder_walkable_slots(regions, obj_ptr, header.num_slots() as usize, SLOT_SIZE)
        };
        // G1AUD-5: GC-internal edges are stamped with the pause's generation,
        // exactly like the mutator barrier's.
        let generation = self.rset_generation();
        let mut record = |raw: usize| {
            if raw == 0 {
                return;
            }
            if let Some(dst_idx) = self.lookup_region_for_addr(raw) {
                if dst_idx != src_idx && regions[dst_idx].region_type() != RegionType::Free {
                    regions[dst_idx]
                        .rset
                        .add_reference_in_generation(src_idx, generation);
                    // F-05 — a GC-internal edge producer must dirty the card
                    // the same way the mutator barrier does, or Phase 2's card
                    // screen skips the very region this call just named. The
                    // card is the HOLDER's, not the slot's: `any_dirty_in` is
                    // asked about an object's whole byte range, so a dirty card
                    // anywhere in the holder scans all of its slots.
                    //
                    // W5-B — and the address is now DERIVED from the holder's
                    // header rather than passed beside it. `obj_addr` and
                    // `header` were two values that had to agree; one of them
                    // is now the only one.
                    self.cards
                        .dirty_addr_at_object_start(crate::g1_cards::ObjectStart::of_header(
                            header,
                        ));
                }
            }
        };
        if header.kind() == ObjectKind::Array {
            for i in 0..walkable_elements {
                // SAFETY: i < the region-clamped element count.
                let raw = unsafe { std::ptr::read(obj_ptr.add(HEADER_SIZE + i * 8) as *const u64) };
                record(raw as usize);
            }
        } else {
            for_each_flat_object_reference_capped(
                obj_ptr,
                header,
                0,
                walkable_slots,
                |_, raw, _| record(raw),
            );
        }
        true
    }

    /// Minimal same-pause drain of evacuation-failed objects: evacuate exactly
    /// `seeds` (self-forwarded, hence LIVE, objects still sitting in kept
    /// from-space regions) and their transitively-kept referents, then run the
    /// normal Phase-4/5 fix-ups against the drain's forwarding map. See
    /// [`Self::retry_after_evacuation_failure`].
    pub fn drain_kept_self_forwards(
        &self,
        seeds: &[usize],
        roots: &mut [ObjectRef],
        monitors: &dyn MonitorCleanup,
    ) -> GcResult {
        let start = std::time::Instant::now();
        let mut regions = self.regions.write();
        // SECURITY FIX (V7a): Phase 5 below can reset/retype regions.
        self.bump_rset_cache_epoch();
        // Item 1 -- a pause's span marks belong to that pause.
        self.clear_humongous_references();
        // G1AUD-9: a destination cursor from the previous pause names a region
        // this one may free, retype, or put in the CSet. Drop both before any
        // reclassification so the first placement of each type does not start
        // from a hint that is guaranteed to miss.
        self.reset_evac_dest_hints();

        // The drain's collection set: exactly the regions holding seeds (the
        // kept regions). Only seed-reachable objects inside them are copied.
        let cset_set: RegionSet = seeds
            .iter()
            .filter_map(|&s| self.lookup_region_for_addr(s))
            .collect();
        let cset: Vec<usize> = cset_set.iter().collect();
        // JIT round 23 wave 3, lane L4: the young-source prerequisite. This
        // pause evacuates only the kept regions, so EVERY young region is
        // outside its set and may hold the only (filtered) reference into one.
        self.record_excluded_young_sources(&mut regions, &|i| cset_set.contains(&i));
        if cset.is_empty() {
            return GcResult {
                stats: GcStats {
                    objects_copied: 0,
                    bytes_copied: 0,
                    bytes_freed: 0,
                },
                pointer_map: cratonvm_types::PointerMap::default(),
            };
        }

        // LANE B — the two inputs `phase4_regions_to_walk` needs, captured
        // BEFORE anything is evacuated. Both are cheap and neither is gated:
        // gathering them costs one pass over two words per region and an
        // `rset` read per region, and doing it unconditionally keeps the
        // `CRATONVM_G1_NARROW_DRAIN_FIXUP` A/B a comparison of the WALK and not
        // of the bookkeeping that precedes it. See `narrow_drain_fixup` for the
        // number and for the soundness question the default answers.
        //
        // `live_rset_sources` is lane A's; it is CALLED here, not changed. It
        // answers "every live region" for a coarsened remembered set, so a
        // coarsened rset degrades this to the wide walk on its own — the same
        // property the young and mixed drivers rely on.
        let narrow_pre: Option<Vec<(RegionType, usize)>> = narrow_drain_fixup().then(|| {
            regions
                .iter()
                .map(|r| (r.region_type(), r.cursor()))
                .collect()
        });
        let narrow_sources: RegionSet = if narrow_drain_fixup() {
            Self::live_rset_sources(&regions, &cset)
        } else {
            RegionSet::new()
        };

        let mut pointer_map: cratonvm_types::PointerMap = cratonvm_types::PointerMap::default();
        let mut objects_copied = 0usize;
        let mut bytes_copied = 0usize;
        let mut work_list: Vec<*mut u8> = Vec::new();

        // g1pin: a peer-stack pinned object in a drained region is a seed too.
        // It stays where it is (`peer_object_pinned`), so its identity forward
        // keeps the region; unreached, the region would be freed under it.
        let mut pin_seeds = Vec::new();
        self.append_peer_object_pin_roots(&mut pin_seeds, &cset_set);
        for &seed in seeds.iter().chain(pin_seeds.iter()) {
            if !self.evacuation_supply_is_an_object(&regions, "evac-failure-drain-seed", seed) {
                continue;
            }
            if let Some((new_ptr, _fresh)) = self.evacuate_object(
                &mut regions,
                seed as *mut u8,
                &mut pointer_map,
                &mut objects_copied,
                &mut bytes_copied,
                &cset_set,
            ) {
                work_list.push(new_ptr);
            }
        }
        let mut scan_idx = 0;
        while scan_idx < work_list.len() {
            let obj_ptr = work_list[scan_idx];
            scan_idx += 1;
            let header = unsafe { &*(obj_ptr as *const ObjectHeader) };
            self.scan_and_evacuate_refs(
                &mut regions,
                obj_ptr,
                header,
                &cset_set,
                &mut pointer_map,
                &mut objects_copied,
                &mut bytes_copied,
                &mut work_list,
            );
        }

        // Roots may point directly at seeds — remap them like Phase 1 would.
        for root in roots.iter_mut() {
            if let Some(&new_addr) = pointer_map.get(&(root.as_ptr() as usize)) {
                *root = unsafe { ObjectRef::from_raw(new_addr as *mut u8) };
            }
        }

        // Phase 4/5 equivalents against the drain map.
        //
        // No eager humongous reclaim here. This path exists to drain regions a
        // previous pause could not evacuate, so it runs with a heap the
        // collector has already declined to reason about normally — exactly the
        // state in which a death certificate should not be issued.
        // G1AUD-10 / F-07 — the drain is a real pause and reported no phases.
        // It is also the pause an evacuation-failure investigation reads first,
        // so "which part of the drain is long" should not require a debug
        // build. The seed evacuation and closure above are fused here the same
        // way the parallel evacuator fuses them; charge them to `closure_us`.
        let mut phases = G1PausePhases::default();
        phases.closure_us = start.elapsed().as_micros() as u64;
        let mut phase_mark = std::time::Instant::now();
        // LANE B — this used to be an unconditional `None`, the fail-WIDE
        // value: every non-CSet, non-Free region walked end to end, object by
        // object, in the pause that is closest to an `OutOfMemoryError`. All
        // four pause drivers narrow their own fix-up; this one never did, and
        // the `Some(pre)`/`None` decision now rests on a lever with a number
        // behind it rather than on an omission. `phase4_regions_to_walk`
        // returns `None` (wide) when `CRATONVM_G1_NARROW_FIXUP` is off, when
        // the snapshot does not describe this table, or when the remembered set
        // is coarsened — so the wide walk is still reachable from three
        // directions and remains the default.
        let narrow = narrow_pre
            .as_deref()
            .and_then(|pre| self.phase4_regions_to_walk(&regions, Some(pre), &narrow_sources));
        let census = self.update_references_in_regions(
            &mut regions,
            &cset_set,
            &pointer_map,
            narrow.as_ref(),
        );
        phases.fixup_us = phase_mark.elapsed().as_micros() as u64;
        phases.fixup_regions = census.walked_regions;
        phases.fixup_bytes = census.walked_bytes;
        // LANE B — the drain's fix-up cost, ACCUMULATED across passes.
        //
        // `phases` is handed to `record_collection_with_phases` at the bottom of
        // this function, and `retry_after_evacuation_failure` calls this
        // function up to eight times per pause. So each pass OVERWRITES the
        // previous pass's record: a pause that spent most of its time in
        // several whole-heap walks reported the cost of the LAST one, and a
        // reader comparing two runs had no denominator for the repetition at
        // all. That is the instrumentation gap
        // `lane-b-drain-fixup-is-still-whole-heap.md` §4 names, and it is why
        // the eight-walk claim in that page had never been checked against a
        // run. (It does not hold on the reproducer: `HumongousChurn 48 6000 512
        // -Xmx160m` enters the drain ONCE and converges in ONE pass. See
        // `narrow_drain_fixup`.)
        //
        // Process-wide rather than per-pause because the question these answer
        // — "is the drain's fix-up a repeated O(heap) cost in this run?" — is a
        // question about the run. `reset_drain_fixup_totals` exists for tests.
        DRAIN_FIXUP_WALKS.fetch_add(1, Ordering::Relaxed);
        DRAIN_FIXUP_REGIONS.fetch_add(u64::from(census.walked_regions), Ordering::Relaxed);
        DRAIN_FIXUP_BYTES.fetch_add(census.walked_bytes, Ordering::Relaxed);
        DRAIN_FIXUP_US.fetch_add(phases.fixup_us, Ordering::Relaxed);
        phase_mark = std::time::Instant::now();
        // F-02: retire the from-space forwards this pause installed, before
        // Phase 5 decides which regions to free and which to keep. After the
        // fix-up (which resolves them), before the reclaim (which is what makes
        // a kept region's stale forward outlive the pause). See
        // `retire_forwards`.
        self.retire_forwards(&pointer_map, &cset_set);
        self.report_pending_corrupt_holders(&regions);
        let _ = self.count_unretired_forwards(&regions, &cset_set, &pointer_map);
        let bytes_freed = self.free_or_keep_cset(&mut regions, &cset, &pointer_map);
        if let Some(pre) = narrow_pre.as_deref() {
            self.record_kept_young_regions_from_pre(&regions, pre);
        }
        phases.free_us = phase_mark.elapsed().as_micros() as u64;
        // LANE W3-C — the region census, at the same protocol point the other
        // FOUR recording paths take it (immediately after `free_us`, while the
        // regions guard is still held). This path was the fifth and did not,
        // so every drain pass emitted an all-zero census. See
        // [`G1PausePhases::drain_pass`], and the PARITY note in
        // `young_collection_parallel` for the same bug's previous two
        // instances.
        phases.record_region_census(&regions);
        phases.cset_regions = cset.len() as u32;
        // And say what this record IS, so nothing downstream counts a drain
        // pass as a pause.
        phases.drain_pass = true;
        phase_mark = std::time::Instant::now();
        self.verify_no_dangling_into_cset(&regions, &cset_set, &pointer_map, roots);
        self.dbg_verify_rset_completeness(&regions, "kept-drain");
        self.dbg_verify_no_unrewritten_forward(&regions, &cset_set, &pointer_map, roots);
        self.dbg_scan_for_zeroed_refs(&regions, &cset_set, roots);
        self.dbg_verify_reachable_integrity(&regions, roots, "kept-drain");
        phases.verify_us = phase_mark.elapsed().as_micros() as u64;

        self.retire_eden_slots_in_cset(&cset_set);

        self.recompute_old_gen_bytes(&regions);
        self.publish_occupancy_metrics(&regions);

        monitors.remap_after_gc(&pointer_map);
        // INT-8: carry the referent-slot skip set across this pause
        // (re-key survivors, prune CSet casualties) — same protocol
        // point as the monitor-table remap. No-op outside a mark cycle.
        self.remap_reference_skip_set(&cset_set, &pointer_map);
        // The overlay mark snapshot, by the same protocol (`g1_overlay.rs`).
        self.remap_overlay_snapshot(&regions, &cset_set, &pointer_map, false);
        // G1CORE-6: same protocol point, same reason — the dedup table's
        // canonical addresses go stale on exactly the objects this pause moved.
        self.remap_string_dedup_table(&cset_set, &pointer_map);

        // Remap (or drop) stale concurrent-mark worklist entries — same
        // protocol as the young paths (done under STW, guard held).
        self.remap_gray_set_after_pause(&regions, &cset_set, &pointer_map);

        let pause_us = start.elapsed().as_micros() as u64;
        phases.close(pause_us);
        let stats = GcStats {
            objects_copied,
            bytes_copied,
            bytes_freed,
        };
        self.record_collection_with_phases(G1CollectionType::YoungOnly, pause_us, &stats, phases);
        GcResult { stats, pointer_map }
    }

    /// Forget the regions a wedged evacuation-failure drain left kept and the
    /// self-forwarded addresses that were their only live objects
    /// ([`Self::kept_unresolved_regions`]).
    pub fn clear_kept_unresolved(&self) {
        self.kept_unresolved_any.store(false, Ordering::Release);
        self.kept_unresolved_regions.lock().clear();
        self.kept_unresolved_live.lock().clear();
    }

    /// Compose the forwarding maps of two consecutive same-pause passes
    /// (`acc` ran first, `next` second) into `acc`.
    ///
    /// Values chase one hop: an object moved by an earlier pass and moved
    /// again — including a pass-1 self-forward `k -> k` that a retry resolved
    /// to a real copy — ends at its final address. New keys are added only if
    /// absent: the VM's `update_all_roots` remaps frame locals that hold
    /// PAUSE-START addresses, so when a `next` key collides with an existing
    /// `acc` key the `acc` entry is the meaningful one (the `next` key is a
    /// pass-1-freed address recycled as later-pass to-space — no frame local
    /// can name it).
    ///
    /// g1ep (`CRATONVM_G1_FULL_POINTER_MAP_SORTED`, default ON, `=0` kills):
    /// the same answer as a merge in the sorted form
    /// ([`cratonvm_types::PointerMap::compose_after`]); the loop below thawed
    /// the young pause's sorted map into hash shards and then hashed every
    /// compaction forward, 8% of `G1epKeepRounds` CPU on one thread.
    ///
    /// g1ep7: the sorted compose runs on the evacuation pool
    /// ([`cratonvm_types::PointerMap::compose_after_par`]).
    pub fn compose_forward_maps(
        &self,
        acc: &mut cratonvm_types::PointerMap,
        next: &cratonvm_types::PointerMap,
    ) {
        let pool = self.evac_pool();
        Self::compose_forward_maps_on(acc, next, pool.helpers() + 1, &|jobs, job| {
            let at = AtomicUsize::new(0);
            let body = |_helper: usize| loop {
                let i = at.fetch_add(1, Ordering::Relaxed);
                if i >= jobs {
                    break;
                }
                job(i);
            };
            pool.scope(jobs.saturating_sub(1), &body, || body(0));
        });
    }

    /// [`Self::compose_forward_maps`] with `jobs` jobs on `run`.
    pub fn compose_forward_maps_on(
        acc: &mut cratonvm_types::PointerMap,
        next: &cratonvm_types::PointerMap,
        jobs: usize,
        run: &dyn Fn(usize, &(dyn Fn(usize) + Sync)),
    ) {
        if g1_full_pointer_map_sorted_enabled() {
            acc.compose_after_par(next, jobs, run);
            return;
        }
        for v in acc.values_mut() {
            if let Some(&nv) = next.get(v) {
                *v = nv;
            }
        }
        for (&k, &v) in next {
            acc.entry(k).or_insert(v);
        }
    }

    // -----------------------------------------------------------------------
    // Young Collection (STW)
    // -----------------------------------------------------------------------

    /// Perform a young-only collection. Evacuates all Eden + Survivor regions.
    pub fn young_collection(
        &self,
        roots: &mut [ObjectRef],
        monitors: &dyn MonitorCleanup,
    ) -> GcResult {
        // Step 9: opt-in multi-threaded evacuator (`CRATONVM_G1_PARALLEL_EVAC`).
        // Behaviour-equivalent to the serial path below (byte-identical program
        // output); see the parallel-evacuation module note above.
        //
        // F-01 — this used to read
        //
        //     parallel_evac_enabled()
        //         && !cratonvm_gc_core::gc_quiescence::is_active()
        //         && self.pending_finalizer_roots.lock().is_empty()
        //
        // and both extra terms are gone, for different reasons.
        //
        // The JIT term said "only the serial path implements
        // conservative-JIT-root region pinning". It does not:
        // `young_collection_parallel` computes the same exclusion from the same
        // `pinned_region_set_including_non_object_roots`, and says in its own
        // comment that it does so deliberately in case this gate is ever
        // loosened. Pinning is a CSet filter applied before evacuation starts;
        // it does not constrain how the evacuation loop is scheduled. The term
        // cost a great deal: on a JIT-warm application it is true for nearly
        // every pause (330,263 of 330,264 in the audit's own measurement), so
        // the parallel evacuator was switched off exactly where it was needed
        // and the worker pool never ran. `CRATONVM_G1_PARALLEL_EVAC_IN_JIT=0`
        // restores it as a bisection lever — see the flag doc, and note that
        // defect G1-11 lives in this path.
        //
        // The finalizer term was true when it was written and is not any more:
        // Phase 3.5 now runs on the parallel drivers too, after
        // `parallel_evacuate` returns and while the regions guard is held
        // again, which is the same state the serial path runs it in.
        // A failed fallible/native allocation is a recovery request, not an
        // ordinary throughput pause.  The parallel path can self-forward a
        // large survivor set when its reserved destination pool is spent;
        // that preserves correctness but keeps the very regions the next
        // humongous retry needs.  The serial path's destination scan and
        // evacuation-failure drain are the established recovery mechanism.
        // The pressure latch clears after that pause, so normal young pauses
        // remain parallel.
        // JIT round 23 wave 7, lane L4 (`CRATONVM_G1_PARALLEL_UNDER_LOW_FREE_LATCH`,
        // default ON since g1ep, `=0` kills): `native_alloc_pressure` is ALSO the
        // ordinary "something must ask for a collection" latch -- the
        // free-fraction re-arm on every region claim, the TLAB reserve, the
        // fallible allocator at the reserve -- so on a heap whose live set
        // keeps the Free pool under the trigger it is set before nearly every
        // pause, and the parallel evacuator never ran there: `KeepLong 12
        // -XX:+UseG1GC -Xmx1g` on n23-w6 took 279 of 321 young pauses SERIAL
        // (`[GC] g1 young evacuation: parallel=42 serial=279`). The serial
        // recovery route is now taken for what it is for: an explicit
        // recovery request from outside the collector
        // (`note_native_alloc_pressure`, the VM's allocation-failure ladder),
        // consumed by this one pause, or a Free pool that cannot hold the
        // young collection set the parallel driver would copy (its to-space
        // pool is Free regions only; the serial driver's destination scan
        // also packs partly filled regions).
        // g1sz (`CRATONVM_G1_PARALLEL_UNDER_SHORT_FREE`, default ON, `=0`
        // kills): a Free pool a few regions short of the young set no longer
        // sends the pause to the serial driver. The parallel driver resumes
        // partly filled Old destinations too (`promo_dest
        // resumed_dest_regions`), and what it still cannot place it
        // self-forwards, as the serial driver does. On `G1epKeepRounds 10
        // -Xmx1g` the last young pause of each round (Free 58-68 regions
        // against a young set of 55-78) ran serial at 480-535 ms of closure
        // for ~0.45 M objects, against 52-99 ms for the same pause parallel.
        let serial_for_pressure = {
            self.serial_recovery_requested.swap(false, Ordering::Relaxed)
                || (false
                    && self.native_alloc_pressure.load(Ordering::Relaxed)
                    && self.free_region_count.load(Ordering::Relaxed)
                        < self.young_region_count.load(Ordering::Relaxed).max(1))
        } || (self.breached_reserve_last_region
            && self.free_region_count.load(Ordering::Relaxed) == 0);
        let parallel = use_parallel_young_evacuator(
            serial_for_pressure,
            parallel_evac_enabled(),
            cratonvm_gc_core::gc_quiescence::is_active(),
            true,
        );
        let result = if parallel {
            G1_YOUNG_PARALLEL.fetch_add(1, Ordering::Relaxed);
            // `G1_EVAC_WORKERS_LAST` is NOT stored here any more (lane D wave
            // 2, 2026-09-20). It used to be, which made it the width a pause
            // DISPATCHED at rather than the width one ran at: a pause that
            // then found an empty collection set and returned without
            // evacuating still moved the number. `parallel_evacuate` stores it
            // instead, which is the only point at which a worker set exists.
            self.young_collection_parallel(roots, monitors)
        } else {
            G1_YOUNG_SERIAL.fetch_add(1, Ordering::Relaxed);
            self.young_collection_serial(roots, monitors)
        };
        // JIT round 23 wave 8, lane L9: the futile-young streak, after either
        // route (`CRATONVM_G1_FUTILE_YOUNG_ESCALATION`); r23 w6: and the
        // starved one, which also asks whether the survivors dominated.
        self.note_futile_young_pause(survivors_dominated(
            result.stats.bytes_copied,
            result.stats.bytes_freed,
        ));
        result
    }

    /// JIT round 23 wave 8, lane L9 (`CRATONVM_G1_FUTILE_YOUNG_ESCALATION`,
    /// OPT-IN `=1` since the n23-w8g A/B): count a young pause that ended, while a
    /// concurrent mark cycle is open, with the Free pool under
    /// [`FUTILE_YOUNG_FREE_PCT`] percent of the heap's regions; any other young
    /// pause resets the streak.
    ///
    /// # Why
    ///
    /// `KeepLong 12 -XX:+UseG1GC -Xmx1g` (n23-w8c): once the previous round's
    /// dead chains fill Old, the Free pool sits at 16-19 of 1024 regions and
    /// the young target collapses to about 4 regions. 172 young pauses of
    /// 2.2 MB each (32 ms apiece, 5.5 s of a 12 s run) then run back to back
    /// while the concurrent mark (`mark_ms=4199`) crawls towards the cleanup
    /// that would free those dead regions (`mark-doors alloc_fail
    /// waiting=203`). HotSpot meets the same heap with an evacuation failure
    /// and a Full collection (33 ms, 1011 M -> 336 M) once per round.
    ///
    /// The VM's mark driver asks [`Self::take_futile_young_escalation`] in its
    /// `Waiting` arm and, on `true`, runs the final remark + cleanup NOW
    /// (`g1_final_remark_cleanup`, which joins the marker and drains the gray
    /// set to a fixed point inside the pause -- the step the force-full door
    /// already takes when its quiescence wait runs out), instead of letting
    /// the starved heap take another hundred pauses first.
    pub fn note_futile_young_pause(&self, survivors_dominated: bool) {
        if !g1_futile_young_escalation_enabled() && false {
            return;
        }
        let regions = (self.config.heap_size / self.config.region_size.max(1)).max(1);
        let free = self.free_region_count.load(Ordering::Relaxed);
        self.note_young_pause_streaks(self.satb_queue.is_active(), free, regions, survivors_dominated);
    }

    /// [`Self::note_futile_young_pause`] with its three inputs given, for a
    /// pause whose survivors dominated.
    pub fn note_futile_young_pause_with(&self, marking: bool, free: usize, regions: usize) {
        self.note_young_pause_streaks(marking, free, regions, true);
    }

    /// Both streaks: the futile one (Free under [`FUTILE_YOUNG_FREE_PCT`])
    /// and the starved full's (Free under [`STARVED_FULL_FREE_PCT`], and the
    /// pause's survivors dominated, [`survivors_dominated`] -- HotSpot's
    /// evacuation failure is survivors that do not fit; a pause that frees
    /// most of its Eden is collecting, not starving).
    pub fn note_young_pause_streaks(
        &self,
        marking: bool,
        free: usize,
        regions: usize,
        survivors_dominated: bool,
    ) {
        if futile_young_pause(marking, free, regions) {
            self.futile_young_streak.fetch_add(1, Ordering::Relaxed);
        } else {
            self.futile_young_streak.store(0, Ordering::Relaxed);
        }
        // r23 w6: the starved full's own streak, at HotSpot's reserve.
        if survivors_dominated && starved_young_pause(marking, free, regions) {
            self.starved_young_streak.fetch_add(1, Ordering::Relaxed);
        } else {
            self.starved_young_streak.store(0, Ordering::Relaxed);
        }
    }

    /// JIT round 23 wave 8, lane L9: has the futile-young streak reached
    /// [`FUTILE_YOUNG_STREAK`]? Consumes it (the streak restarts at 0) when it
    /// has, so one streak escalates once. `false` under
    /// `CRATONVM_G1_FUTILE_YOUNG_ESCALATION=0`, and whenever no cycle is open.
    pub fn take_futile_young_escalation(&self) -> bool {
        self.take_futile_young_escalation_with(self.satb_queue.is_active())
    }

    /// [`Self::take_futile_young_escalation`] with "is a cycle open" given.
    pub fn take_futile_young_escalation_with(&self, marking: bool) -> bool {
        if !g1_futile_young_escalation_enabled() || !marking {
            return false;
        }
        let streak = self.futile_young_streak.load(Ordering::Relaxed);
        if streak < FUTILE_YOUNG_STREAK {
            return false;
        }
        if self
            .futile_young_streak
            .compare_exchange(streak, 0, Ordering::Relaxed, Ordering::Relaxed)
            .is_err()
        {
            return false;
        }
        self.futile_young_escalations.fetch_add(1, Ordering::Relaxed);
        true
    }

    /// r23 w6 (`KeepLong`), `CRATONVM_G1_STARVED_FULL` (default ON, `=0`
    /// kills): has the starved-young streak (a cycle open, the Free pool under
    /// [`STARVED_FULL_FREE_PCT`] percent, pause after pause) reached the
    /// starved full's need ([`STARVED_FULL_STREAK`], doubled per starved full
    /// that freed little: [`Self::note_starved_full_outcome`])? Consumes the
    /// streak when it has. The VM then aborts the open cycle and runs one
    /// in-place full compaction (`gc_and_alloc::g1_starved_full`) -- HotSpot
    /// G1's evacuation failure followed by a `Pause Full`, instead of waiting
    /// out the starved mark behind a hundred 2 MB young pauses.
    pub fn take_starved_full(&self) -> bool {
        self.take_starved_full_with(self.satb_queue.is_active())
    }

    /// [`Self::take_starved_full`] with "is a cycle open" given.
    pub fn take_starved_full_with(&self, marking: bool) -> bool {
        if !marking {
            return false;
        }
        let streak = self.starved_young_streak.load(Ordering::Relaxed);
        if streak < self.starved_full_need.load(Ordering::Relaxed) {
            return false;
        }
        self.starved_young_streak
            .compare_exchange(streak, 0, Ordering::Relaxed, Ordering::Relaxed)
            .is_ok()
    }

    /// Arm the starved full's pause ([`Self::take_starved_full`]): the next
    /// pause is a full-compaction pause whose old half copies nothing (the
    /// aborted cycle left no trusted liveness), followed by the in-place
    /// compaction of every movable Old region whatever the Free pool holds.
    /// `false` (nothing armed) under `CRATONVM_G1_FULL_COMPACTION_IN_PLACE=0`.
    pub fn arm_starved_full(&self) -> bool {
        if !compact_in_place::g1_full_compaction_in_place_enabled() {
            return false;
        }
        self.starved_full_armed.store(true, Ordering::Release);
        self.full_compaction_armed.store(true, Ordering::Release);
        true
    }

    /// The G1 near-full proxy row (`CRATONVM_G1_IN_PLACE_ON_UNTRUSTED_MARK`,
    /// default ON, `=0` kills): arm a full-compaction pause whose old half
    /// copies nothing and whose in-place pass runs whatever the Free pool
    /// holds, for the allocation-failure ladder when
    /// [`Self::arm_full_compaction`] refused because the last cleanup did not
    /// trust its closure. The copying pass needs that mark's `live_bytes`;
    /// the in-place pass marks from the pause's roots itself. Under load the
    /// last-ditch cycle's mark was untrusted on a throwing ladder, so no
    /// compaction ran on a heap with a held region's megabyte of holes.
    /// `false` (nothing armed) when either switch is off.
    pub fn arm_in_place_compaction(&self) -> bool {
        if !compact_in_place::g1_full_compaction_in_place_enabled()
        {
            return false;
        }
        self.in_place_only_armed.store(true, Ordering::Release);
        self.full_compaction_armed.store(true, Ordering::Release);
        true
    }

    /// The R22 aastore-humongous page (`CRATONVM_G1_FRAGMENTED_HUMONGOUS_SLIDE`,
    /// default ON, `=0` kills): the allocation-failure ladder's last
    /// compaction for a humongous request that still fails on SHAPE -- enough
    /// Free regions in all, no run of the request's length -- after the
    /// recovery pause and the staged compactions. Those copy Old regions into
    /// Free ones and keep the Free count, but nothing puts the Free regions
    /// next to each other: `R22FlipGcRows -Xmx64m --nojit` threw for a 5-region
    /// `int[]` with 25 regions Free and a longest run of 3, where HotSpot's
    /// `Pause Full` slides every live object down and leaves one Free tail.
    ///
    /// Arms an in-place-only full compaction ([`Self::arm_in_place_compaction`]'s
    /// pause, whatever the pool's size) whose slide also takes the Free
    /// regions below its top movable region as destinations, so the Old data
    /// packs into the lowest regions and the Free ones gather above it.
    /// Returns `(requested, free, longest)` when it armed; `None` when the
    /// request is gone, was served, is not fragmented, or a switch is off.
    pub fn arm_fragmented_humongous_slide(&self) -> Option<(usize, usize, usize)> {
        if !compact_in_place::g1_full_compaction_in_place_enabled()
        {
            return None;
        }
        let requested = self.fragmented_slide_span.load(Ordering::Acquire);
        if requested <= 1 {
            return None;
        }
        let (free, longest) = free_region_shape(&self.regions.read());
        if longest >= requested || !self.fragmented_free_suffices(free, requested) {
            return None;
        }
        self.in_place_fills_free_armed
            .store(true, Ordering::Release);
        self.in_place_only_armed.store(true, Ordering::Release);
        self.full_compaction_armed.store(true, Ordering::Release);
        Some((requested, free, longest))
    }

    /// gcki-ngr: whether the humongous request the last allocation failure
    /// armed (and no claim has served since) fits the Free table now -- a
    /// run of its length, and the retry's reserve. The last-ditch rungs after
    /// the recovery pause copy Old data into Free regions, so once this holds
    /// they can only cut the run the retry needs.
    pub fn failed_humongous_request_fits(&self) -> bool {
        let requested = self.fragmented_slide_span.load(Ordering::Acquire);
        if requested <= 1 {
            return false;
        }
        let (free, longest) = free_region_shape(&self.regions.read());
        longest >= requested && self.fragmented_free_suffices(free, requested)
    }

    /// The starved full's back-off: a pass that freed under a sixteenth of
    /// the heap's regions doubles the streak the next one needs (capped at
    /// [`STARVED_FULL_STREAK_MAX`]), so a heap starved by LIVE data does not
    /// compact every few pauses; a productive one restores
    /// [`STARVED_FULL_STREAK`].
    pub fn note_starved_full_outcome(&self, freed_regions: usize, total_regions: usize) {
        let need = if freed_regions.saturating_mul(16) < total_regions {
            self.starved_full_need
                .load(Ordering::Relaxed)
                .saturating_mul(2)
                .min(STARVED_FULL_STREAK_MAX)
        } else {
            STARVED_FULL_STREAK
        };
        self.starved_full_need.store(need, Ordering::Relaxed);
        compact_in_place::note_starved_full(freed_regions);
    }

    /// JIT round 23 wave 8, lane L9 (`CRATONVM_G1_SERIAL_FORWARD_PRESIZE`,
    /// default ON, `=0` kills): the serial young/mixed pause's forwarding map,
    /// pre-sized from the last serial pause's forward count
    /// ([`Self::note_serial_forwards`]), capped at
    /// [`SERIAL_FORWARD_PRESIZE_CAP`]. On `KeepLong -Xmx1g` (n23-w8g) the
    /// map's growth (`RawTable::reserve_rehash`) was 3.3% of the serial pause.
    /// A capacity is not an entry: the map's contents, and so every lookup the
    /// pause and the VM's root remap make, are unchanged.
    pub fn serial_pointer_map(&self) -> cratonvm_types::PointerMap {
        if !g1_serial_forward_presize_enabled() {
            return cratonvm_types::PointerMap::default();
        }
        let hint = self
            .serial_forward_hint
            .load(Ordering::Relaxed)
            .min(SERIAL_FORWARD_PRESIZE_CAP);
        if hint == 0 {
            cratonvm_types::PointerMap::default()
        } else {
            cratonvm_types::PointerMap::with_capacity(hint)
        }
    }

    /// g1ep: an empty forward list for one parallel-evacuation worker, with
    /// the capacity an earlier pause left it ([`Self::forward_buffers`]).
    pub fn take_forward_buffer(&self) -> Vec<(usize, usize)> {
        self.forward_buffers.lock().pop().unwrap_or_default()
    }

    /// g1ep: give a forward list back for the next pause, emptied. The pool
    /// keeps at most [`FORWARD_BUFFER_POOL`] lists of at most
    /// [`FORWARD_BUFFER_MAX_PAIRS`] pairs; anything else is freed.
    pub fn give_forward_buffer(&self, mut buf: Vec<(usize, usize)>) {
        if buf.capacity() == 0 || buf.capacity() > FORWARD_BUFFER_MAX_PAIRS {
            return;
        }
        buf.clear();
        let mut pool = self.forward_buffers.lock();
        if pool.len() < FORWARD_BUFFER_POOL {
            pool.push(buf);
        }
    }

    /// Record how many forwards a serial pause made, for the next one's
    /// [`Self::serial_pointer_map`].
    pub fn note_serial_forwards(&self, forwards: usize) {
        self.serial_forward_hint.store(forwards, Ordering::Relaxed);
    }

}
