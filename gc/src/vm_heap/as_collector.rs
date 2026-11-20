#![allow(unsafe_code)]
#[allow(unused_imports)]
use super::*;

impl VmHeap {
    /// Borrow the underlying collector behind the common [`GarbageCollector`] trait.
    #[inline]
    pub fn as_collector(&self) -> &dyn GarbageCollector {
        match &self.inner {
            VmHeapInner::Generational(h) => h,
            VmHeapInner::G1(h) => &**h,
            #[cfg(feature = "zgc")]
            VmHeapInner::Zgc(h) => &**h,
        }
    }

    /// zt t29/g1 (G1 carry page item 10a): HotSpot's `-XX:+AlwaysPreTouch`
    /// on G1 ([`G1Collector::set_always_pre_touch`]: the startup commit now,
    /// every region commit as it is made) and on the generational heap
    /// ([`GenerationalHeap::pre_touch_committed`]: the startup commit now,
    /// every later commit as it is made since g1gen), per
    /// VM, before the first allocation. Answers whether the option is in
    /// force (the `gc,init` block's `Pre-touch:` fact): `false` without the
    /// option, under `CRATONVM_GC_ALWAYS_PRE_TOUCH_ALL=0`, and on ZGC, whose
    /// heap applies it itself (`ZgcRealHeap::set_always_pre_touch`).
    pub fn set_always_pre_touch_non_zgc(&self, on: bool) -> bool {
        if !on || !gc_always_pre_touch_all_enabled() {
            return false;
        }
        match &self.inner {
            VmHeapInner::G1(state) => {
                let _ = state.collector.set_always_pre_touch(true);
                true
            }
            VmHeapInner::Generational(heap) => {
                let _ = heap.pre_touch_committed();
                true
            }
            #[cfg(feature = "zgc")]
            VmHeapInner::Zgc(_) => false,
        }
    }

    /// zki (page `zgc-zt23hunt4-transparent-huge-pages-flag-ignored` item
    /// 5(c)): HotSpot 25's `-XX:+UseTransparentHugePages` on G1 and Serial --
    /// the heap is advised `MADV_HUGEPAGE` (the VM decided the THP mode
    /// supports it). `false` on ZGC (its own decision,
    /// `ZgcRealHeap::advise_transparent_huge_pages`).
    pub fn advise_transparent_huge_pages_non_zgc(&self) -> bool {
        match &self.inner {
            VmHeapInner::G1(state) => state.collector.advise_transparent_huge_pages(),
            VmHeapInner::Generational(heap) => heap.advise_transparent_huge_pages(),
            #[cfg(feature = "zgc")]
            VmHeapInner::Zgc(_) => false,
        }
    }

    /// Create a new VmHeap with the specified backend and total capacity.
    pub fn new(backend: GcBackend, total_bytes: usize) -> Self {
        Self::new_with_overrides(backend, total_bytes, G1ConfigOverrides::default())
    }

    /// Like [`Self::new`] but applies explicit G1 tuning overrides (wired from
    /// the `-XX:` knobs: `InitiatingHeapOccupancyPercent`, `G1HeapRegionSize`,
    /// `MaxGCPauseMillis`, `±UseStringDeduplication`). Each `None` keeps the
    /// collector default; an explicit value wins over the heap-size-based
    /// region-size ergonomic. No effect on the generational backend.
    ///
    /// `-Xms` does **not** travel through here. [`Self::new_with_heap_sizing`]
    /// is the door that carries it; this one supplies `None`, which is the
    /// right answer for every caller that has only one size.
    pub fn new_with_overrides(
        backend: GcBackend,
        total_bytes: usize,
        overrides: G1ConfigOverrides,
    ) -> Self {
        Self::new_with_heap_sizing(backend, total_bytes, None, overrides).0
    }

    /// [`Self::new_with_overrides`] plus the second heap size: `initial_bytes`
    /// is `-Xms`, the bytes to COMMIT at startup, and the returned
    /// [`XmsDisposition`] says what this backend actually did with it.
    ///
    /// An `-Xms` above `-Xmx` is clamped rather than refused — the two are
    /// specified separately and a user who oversizes one should still get a VM
    /// — and a zero or absent value is [`XmsDisposition::NotSupplied`], never a
    /// commit of nothing.
    ///
    /// Callers that can tell whether the operator actually typed `-Xms` should
    /// warn on [`XmsDisposition::Dropped`]. This constructor deliberately does
    /// not warn itself: `VmConfig::initial_heap_size` carries a 16 MiB default
    /// that arrives here indistinguishable from an explicit flag, so a
    /// diagnostic raised here would fire on every run.
    pub fn new_with_heap_sizing(
        backend: GcBackend,
        total_bytes: usize,
        initial_bytes: Option<usize>,
        overrides: G1ConfigOverrides,
    ) -> (Self, XmsDisposition) {
        let initial = initial_bytes.filter(|n| *n > 0).map(|n| n.min(total_bytes));
        match backend {
            GcBackend::Generational => Self::new_generational(
                total_bytes,
                initial,
                &cratonvm_gc_core::gen_young_sizing::YoungGenSizing::default(),
                None,
            ),
            GcBackend::G1 => {
                let mut config = G1CollectorConfig::default();
                config.heap_size = total_bytes;
                config.region_size = g1_ergonomic_region_size(total_bytes);
                // Explicit -XX: overrides take precedence over the ergonomic,
                // but are held to the same shape — a region size is a power of
                // two no matter who chose it (explicit: `[1 MiB, 512 MiB]`, see
                // `explicit_region_size`). `G1Collector::new` would round a non-power-of-two
                // up anyway (see `normalize_region_size`); doing it here as
                // well is what keeps the value an operator reads back out of
                // the config equal to the one the collector is using.
                if let Some(rs) = overrides.region_size {
                    if rs > 0 {
                        // gci i11/gm finding 13: HotSpot's explicit bounds
                        // (1 MiB floor, 512 MiB ceiling), not the ergonomic ones.
                        config.region_size = explicit_region_size(rs, total_bytes);
                    }
                }
                if let Some(ihop) = overrides.ihop_percent {
                    config.ihop_percent = ihop.clamp(1, 100);
                }
                if let Some(pause) = overrides.max_gc_pause_ms {
                    config.max_gc_pause_ms = pause.max(1);
                }
                if let Some(p) = overrides.mixed_gc_live_threshold_percent {
                    config.mixed_gc_live_threshold_percent = p.clamp(1, 100);
                }
                if let Some(p) = overrides.heap_waste_percent {
                    config.heap_waste_percent = p.min(100);
                }
                if let Some(dedup) = overrides.string_dedup {
                    config.string_dedup_enabled = dedup;
                }
                // An explicit count is honoured by `parallel_worker_count`;
                // 0 would mean "auto" there, so a
                // `-XX:ParallelGCThreads=0` is rejected by the CLI rather than
                // being silently reinterpreted.
                if let Some(n) = overrides.parallel_gc_threads {
                    if n > 0 {
                        config.gc_worker_threads = n;
                    }
                }
                // F-16: `-Xms`. Also clamped to the reservation by
                // `G1Collector::new`, so an `-Xms` above `-Xmx` yields a heap
                // rather than a refusal.
                if let Some(n) = initial {
                    config.initial_heap_size = n;
                }
                (
                    VmHeap::from_inner(VmHeapInner::G1(G1State::new(config))),
                    match initial {
                        None => XmsDisposition::NotSupplied,
                        Some(n) => XmsDisposition::Committed(n),
                    },
                )
            }
            #[cfg(feature = "zgc")]
            GcBackend::Zgc => {
                let heap = ZgcRealHeap::new_shared_with_initial_and_large_pages(
                    total_bytes,
                    initial.unwrap_or(0),
                    overrides.zgc_explicit_large_pages,
                );
                // zr z7/rs: the same `[GC] zgc-footprint-spans:` line the
                // summary prints, once at construction too, so
                // `tools/rss-by-mapping.sh` knows the ranges WHILE the VM runs
                // (it counts their resident pages from `/proc/<pid>/pagemap`
                // at its snapshot, which a line printed at exit is too late
                // for). Behind `CRATONVM_GC_STATS`, like the summary.
                if None::<std::ffi::OsString>.is_some() {
                    eprintln!(
                        "[GC] zgc-footprint-spans: {} at=init",
                        heap.footprint_spans().render()
                    );
                }
                // `-XX:MaxGCPauseMillis` reached ONLY G1 before 2026-09-03.
                // On the DEFAULT collector an operator who asked for a pause
                // target got no answer and no diagnostic; ZGC now sizes its
                // allocation budget from it (`refresh_pause_target_budget`).
                // Applied after construction, so it wins over
                // `CRATONVM_ZGC_PAUSE_TARGET_MS`, which seeded the field --
                // an explicit flag must never be overridden by an A/B switch.
                //
                // `0` is refused by the CLI parser (`-XX:MaxGCPauseMillis=0`
                // is warned about and dropped), so `Some(0)` cannot arrive
                // here to mean "off"; that spelling belongs to the env var.
                //
                // zr z3/cs: NOT ANY MORE, by default. HotSpot's ZGC does not
                // read `-XX:MaxGCPauseMillis` (a G1 / Parallel ergonomic), so
                // there the flag leaves the collection frequency unchanged;
                // here it resized `alloc_trigger_bytes`. The CratonVM knob
                // `CRATONVM_ZGC_PAUSE_TARGET_MS` keeps the loop.
                // `CRATONVM_ZGC_IGNORE_MAX_GC_PAUSE_MILLIS=0` restores the door.
                if let Some(ms) = overrides.max_gc_pause_ms {
                    if !cratonvm_gc_zgc::zgc::zgc_ignore_max_gc_pause_millis() {
                        heap.set_pause_target_ms(ms.max(1));
                    }
                }
                #[cfg(feature = "gpu-offload")]
                {
                    let weak = std::sync::Arc::downgrade(&heap);
                    heap.external_gpu
                        .gpu_coord
                        .register_zgc_relocate_idle_hook(std::sync::Arc::new(move |drain| {
                            let h = weak.upgrade()?;
                            if drain {
                                Some(h.wait_for_concurrent_relocate_idle())
                            } else {
                                Some(false)
                            }
                        }));
                }
                // HONOURED as a PREFIX COMMIT, as of 2026-09-21.
                // `ZgcRealHeap::with_capacity_and_initial` builds its single
                // `Arena` once and NEVER grows it — the object-start bitmap and
                // `conservative_addr_span` are both gridded over the envelope
                // captured there — so `-Xms` for this collector can only ever
                // mean "commit this prefix", never "reserve less". Sizing the
                // arena from `-Xms` instead would make `-Xms512m -Xmx8g` a heap
                // that can never reach 8g, which is the "alias for `-Xmx`"
                // spelling the known-issues page above forbids.
                (
                    VmHeap::from_inner(VmHeapInner::Zgc(heap)),
                    match initial {
                        None => XmsDisposition::NotSupplied,
                        Some(n) => XmsDisposition::Committed(n),
                    },
                )            }        }    }

    /// [`Self::new_with_heap_sizing`] plus the operator's young-generation
    /// sizing (`-Xmn`, `-XX:NewSize`, `-XX:MaxNewSize`, `-XX:NewRatio`) —
    /// gen r4w2/alloc2, 2026-09-23.
    ///
    /// Honoured by the generational backend only
    /// ([`GenerationalHeap::with_capacity_and_young`]). G1 sizes its young
    /// generation adaptively from region counts and ZGC has none, so for those
    /// two this is exactly `new_with_heap_sizing` — the launcher, which is the
    /// layer that knows the flags were typed, prints the one-time "ignored on
    /// this collector" note. A default (`YoungGenSizing::default()`) request is
    /// exactly `new_with_heap_sizing` on every backend.
    pub fn new_with_heap_and_young_sizing(
        backend: GcBackend,
        total_bytes: usize,
        initial_bytes: Option<usize>,
        young: &cratonvm_gc_core::gen_young_sizing::YoungGenSizing,
        overrides: G1ConfigOverrides,
    ) -> (Self, XmsDisposition) {
        Self::new_with_heap_young_and_survivor_sizing(
            backend,
            total_bytes,
            initial_bytes,
            young,
            None,
            overrides,
        )
    }

    /// [`Self::new_with_heap_and_young_sizing`] plus the `-XX:SurvivorRatio`
    /// the VM will apply to a Generational heap
    /// (`GenerationalHeap::set_survivor_ratio`), which gcj yg3-residuals R3's
    /// Serial geometry needs at construction
    /// (`GenerationalHeap::with_capacity_young_and_survivor_ratio`). `None`
    /// (not typed) is [`Self::new_with_heap_and_young_sizing`] exactly; the
    /// other backends ignore it here.
    pub fn new_with_heap_young_and_survivor_sizing(
        backend: GcBackend,
        total_bytes: usize,
        initial_bytes: Option<usize>,
        young: &cratonvm_gc_core::gen_young_sizing::YoungGenSizing,
        survivor_ratio: Option<u64>,
        overrides: G1ConfigOverrides,
    ) -> (Self, XmsDisposition) {
        if (young.is_default() && survivor_ratio.is_none())
            || !matches!(backend, GcBackend::Generational)
        {
            return Self::new_with_heap_sizing(backend, total_bytes, initial_bytes, overrides);
        }
        let initial = initial_bytes.filter(|n| *n > 0).map(|n| n.min(total_bytes));
        Self::new_generational(total_bytes, initial, young, survivor_ratio)
    }

    /// The `GcBackend::Generational` arm of the heap constructors, split out
    /// (gen r4w2/alloc2) so the young sizing reaches it without touching the
    /// other two arms. `initial` is the already-clamped `-Xms`.
    pub(in crate::vm_heap) fn new_generational(
        total_bytes: usize,
        initial: Option<usize>,
        young: &cratonvm_gc_core::gen_young_sizing::YoungGenSizing,
        survivor_ratio: Option<u64>,
    ) -> (Self, XmsDisposition) {
        // gci i22/ed: `CRATONVM_GEN_SERIAL_EDEN_INITIAL` (default off) starts
        // the Serial-shaped young trigger at HotSpot's eden for this initial
        // heap; a no-op when off or when there is no initial heap.
        let heap = GenerationalHeap::with_capacity_young_and_survivor_ratio(
            total_bytes,
            young,
            survivor_ratio,
        )
            .follow_initial_heap_eden(initial.unwrap_or(0), young);
        // HONOURED, as of 2026-09-21. `commit_initial_heap` commits the
        // request across the two young semi-spaces, which sit on
        // `Arena` and so on `HeapStore` — the store that reserves the
        // address space and commits it in 2 MiB granules.
        //
        // gen r4w2/oldgen2 (2026-09-23): the OLD generation reserves too now
        // (`OldGen` on `HeapStore`), and holds whatever the young pair leaves
        // of the heap (half of it by default; `-Xmx` minus the young pair
        // under `-Xmn` / `NewRatio`). `commit_initial_heap` commits whatever
        // the young pair could not absorb as an old-gen prefix — so the
        // startup commit is at least `-Xms` for every `-Xms <= -Xmx`, and
        // `Committed(n)` stays the honest answer. (Before that date the old
        // generation was a `Vec` wholly committed at construction, which
        // covered the remainder by over-committing it.)
        if let Some(n) = initial {
            heap.commit_initial_heap(n);
        }
        (
            VmHeap::from_inner(VmHeapInner::Generational(heap)),
            match initial {
                None => XmsDisposition::NotSupplied,
                Some(n) => XmsDisposition::Committed(n),
            },
        )
    }

    // =====================================================================
    // Core allocation (both backends implement GarbageCollector trait)
    // =====================================================================

    /// Bind every backend to this VM's compact-layout domain.
    ///
    /// Must be called before the VM defines its first class, because
    /// `class_id` is a per-`ClassStore` index and the layout registry is
    /// process-global: an untold heap defaults to the FIRST domain, which is
    /// correct for a single-VM process and merely costs later VMs their compact
    /// layouts (tagged slots are always correct, just larger).
    pub fn set_layout_domain(&self, domain: u32) {
        dispatch!(&self.inner, set_layout_domain(domain))
    }

    /// `CRATONVM_DBG_VACATED_FRAMES` bookkeeping: an address the allocator has
    /// just issued is no longer evidence that anything was moved away from it.
    ///
    /// Every non-TLAB allocation door on this type funnels its result through
    /// here. The TLAB door is [`Self::refill_tlab`], which purges the whole
    /// chunk at once — see `gc_quiescence::note_allocated_range` for why the
    /// per-object door alone left the instrument reporting every fresh young
    /// object as a stale reference on the non-ZGC backends.
    #[inline]
    pub(in crate::vm_heap) fn note_alloc(o: ObjectRef) -> ObjectRef {
        if !cratonvm_gc_core::gc_quiescence::vacated_frames_enabled() {
            return o;
        }
        // The whole EXTENT, not just the base. An address the ledger holds
        // because a small object was moved away from it stops being evidence
        // the moment a LARGER object is allocated over it -- and only the base
        // of that larger object would be purged by an address-keyed door, so
        // every interior word stayed in the ledger for the rest of the run.
        // On BindableTests that is a quarter-megabyte of stale entries per
        // large array, which is the exact false-positive class this ledger was
        // repaired to stop producing.
        //
        // SAFETY: `o` is an object the allocator has just finished laying out;
        // its header is initialised and mapped.
        let base = o.as_ptr() as usize;
        let size =
            unsafe { cratonvm_gc_gen::gen_heap::gen_object_total_size(&*(base as *const ObjectHeader)) };
        cratonvm_gc_core::gc_quiescence::note_allocated_range(base, base.saturating_add(size.max(8)));
        o
    }

    #[inline]
    pub(in crate::vm_heap) fn note_alloc_opt(o: Option<ObjectRef>) -> Option<ObjectRef> {
        o.map(Self::note_alloc)
    }

    /// Legacy collection-free allocation; fatal on exhaustion. Java paths
    /// must use `try_alloc_object_full` and a rooted collecting ladder.
    pub fn alloc_object(&self, class_id: ClassId, num_fields: usize) -> ObjectRef {
        Self::note_alloc(dispatch!(&self.inner, alloc_object(class_id, num_fields)))
    }

    /// Legacy collection-free allocation; fatal on exhaustion or invalid length.
    pub fn alloc_array(
        &self,
        class_id: ClassId,
        element_type: ArrayElementType,
        length: usize,
    ) -> ObjectRef {
        Self::note_alloc(dispatch!(&self.inner, alloc_array(class_id, element_type, length)))
    }

    /// Try to allocate an object. Returns `None` on OOM (caller should trigger GC and retry).
    pub fn try_alloc_object(&self, class_id: ClassId, num_fields: usize) -> Option<ObjectRef> {
        Self::note_alloc_opt(match &self.inner {
            VmHeapInner::Generational(h) => h.try_alloc_object(class_id, num_fields),
            VmHeapInner::G1(h) => h.try_alloc_object(class_id, num_fields),
            #[cfg(feature = "zgc")]
            VmHeapInner::Zgc(h) => h.try_alloc_object(class_id, num_fields),
        })
    }

    /// Try to allocate directly in the old generation. This is only available
    /// for the generational heap; other heap implementations return `None` so
    /// callers can fall back to their normal allocation path.
    pub fn try_alloc_object_old(&self, class_id: ClassId, num_fields: usize) -> Option<ObjectRef> {
        Self::note_alloc_opt(match &self.inner {
            VmHeapInner::Generational(h) => h.try_alloc_object_old(class_id, num_fields),
            VmHeapInner::G1(_) => None,
            #[cfg(feature = "zgc")]
            VmHeapInner::Zgc(_) => None,
        })
    }

    /// Allocate a same-layout old-generation batch under one allocator lock.
    /// Only the generational backend currently exposes a non-moving old-gen
    /// pool; other backends return an empty batch so callers retain their
    /// ordinary allocation fallback.
    pub fn try_alloc_objects_old_batch(
        &self,
        class_id: ClassId,
        num_fields: usize,
        count: usize,
    ) -> Vec<ObjectRef> {
        let batch = match &self.inner {
            VmHeapInner::Generational(h) => h.try_alloc_objects_old_batch(class_id, num_fields, count),
            VmHeapInner::G1(_) => Vec::new(),
            #[cfg(feature = "zgc")]
            VmHeapInner::Zgc(_) => Vec::new(),
        };
        if cratonvm_gc_core::gc_quiescence::vacated_frames_enabled() {
            let addrs: Vec<usize> = batch.iter().map(|o| o.as_ptr() as usize).collect();
            cratonvm_gc_core::gc_quiescence::note_allocated(&addrs);
        }
        batch
    }

    /// Fallible twin of [`alloc_object`](Self::alloc_object): same (no-GC)
    /// allocation path including the old-generation spill, but returns `None`
    /// on true heap exhaustion instead of aborting the VM. Lets the JIT
    /// object-alloc helper raise a catchable `OutOfMemoryError`.
    pub fn try_alloc_object_full(&self, class_id: ClassId, num_fields: usize) -> Option<ObjectRef> {
        Self::note_alloc_opt(match &self.inner {
            VmHeapInner::Generational(h) => h.try_alloc_object_full(class_id, num_fields),
            VmHeapInner::G1(h) => h.try_alloc_object(class_id, num_fields),
            #[cfg(feature = "zgc")]
            VmHeapInner::Zgc(h) => h.try_alloc_object(class_id, num_fields),
        })
    }

    /// Fallible twin of [`alloc_array`](Self::alloc_array): same (no-GC)
    /// allocation path, but returns `None` on true heap exhaustion instead of
    /// aborting the VM. Lets native callers (e.g. `ArrayList(int)`) raise a
    /// catchable `OutOfMemoryError` for an over-large backing array.
    pub fn try_alloc_array_full(
        &self,
        class_id: ClassId,
        element_type: ArrayElementType,
        length: usize,
    ) -> Option<ObjectRef> {
        // Round 12 wave 3 (lane iropt2, `r12w3-iropt2-array-length-cap-at-the-heap-patch`):
        // enforced below every front end; see [`VM_MAX_ARRAY_LENGTH`].
        if length > VM_MAX_ARRAY_LENGTH {
            return None;
        }
        let obj = Self::note_alloc_opt(match &self.inner {
            VmHeapInner::Generational(h) => h.try_alloc_array_full(class_id, element_type, length),
            VmHeapInner::G1(h) => h.try_alloc_array(class_id, element_type, length),
            #[cfg(feature = "zgc")]
            VmHeapInner::Zgc(h) => h.try_alloc_array(class_id, element_type, length),
        });
        self.g1_check_old_usage_on_growth(obj);
        obj
    }

    /// gci i14/mp finding 4 (`gci-i14mp-memory-pool-and-gc-mxbean-review-findings-FIXED-20261016`):
    /// a G1 humongous array grows `G1 Old Gen` outside any collection, and
    /// HotSpot's allocator then runs
    /// `LowMemoryDetector::detect_low_memory_for_collected_pools`, so a usage
    /// threshold on the old pool fires without a collection. The G1 twin of
    /// Generational's `check_tenured_usage_on_growth` (gcf f4/r); the VM's
    /// allocation slow path (`maybe_gc`'s occupancy poll) delivers the request
    /// as it does that one's. One relaxed load unless a threshold is armed;
    /// then the region-table read lock for the humongous test and one pool
    /// sample. No lock is held here (the allocation has returned).
    pub(in crate::vm_heap) fn g1_check_old_usage_on_growth(&self, obj: Option<ObjectRef>) {
        let VmHeapInner::G1(h) = &self.inner else {
            return;
        };
        let Some(obj) = obj else {
            return;
        };
        let thresholds = h.gc_beans().notifications().pool_thresholds();
        if !thresholds.is_armed() || !h.is_humongous(obj) {
            return;
        }
        let pools = h.gc_beans().pools(&self.backend_pool_samples());
        if let Some(old) = pools
            .iter()
            .find(|p| p.name == cratonvm_gc_core::gc_metrics::JMX_POOL_G1_OLD)
        {
            let _ = thresholds.check_usage_on_growth(old);
        }
    }

    /// DBG (bc math-ec `0x4`): scan young from-space for the first `0x4` seed
    /// slot. See [`GenerationalHeap::dbg_first_young_small_ref`]. G1 unsupported.
    pub fn dbg_first_young_small_ref(&self) -> Option<(usize, u32, usize, usize, u64)> {
        match &self.inner {
            VmHeapInner::Generational(h) => h.dbg_first_young_small_ref(),
            VmHeapInner::G1(_) => None,
            #[cfg(feature = "zgc")]
            VmHeapInner::Zgc(_) => None,
        }
    }

    /// Allocate a new Java object and initialize primitive-typed slots to
    /// their spec-mandated typed zero based on JVM field descriptor bytes.
    ///
    /// This is the safe allocation entry point — it fixes the
    /// ConcurrentHashMap.initTable CAS livelock by guaranteeing that an
    /// unwritten `int` slot reads back as `Value::Int(0)` instead of
    /// `Value::Object(None)`. See
    /// [`cratonvm_gc_core::heap::default_value_for_descriptor`] for the mapping.
    pub fn alloc_object_with_descriptors(
        &self,
        class_id: ClassId,
        num_fields: usize,
        descriptor_bytes: &[u8],
    ) -> ObjectRef {
        match &self.inner {
            VmHeapInner::Generational(h) => {
                h.alloc_object_with_descriptors(class_id, num_fields, descriptor_bytes)
            }
            VmHeapInner::G1(h) => {
                h.alloc_object_with_descriptors(class_id, num_fields, descriptor_bytes)
            }
            #[cfg(feature = "zgc")]
            VmHeapInner::Zgc(h) => {
                h.alloc_object_with_descriptors(class_id, num_fields, descriptor_bytes)
            }
        }
    }

    /// Fallible variant: returns `None` when the heap is out of space.
    pub fn try_alloc_object_with_descriptors(
        &self,
        class_id: ClassId,
        num_fields: usize,
        descriptor_bytes: &[u8],
    ) -> Option<ObjectRef> {
        Self::note_alloc_opt(match &self.inner {
            VmHeapInner::Generational(h) => {
                h.try_alloc_object_with_descriptors(class_id, num_fields, descriptor_bytes)
            }
            VmHeapInner::G1(h) => {
                h.try_alloc_object_with_descriptors(class_id, num_fields, descriptor_bytes)
            }
            #[cfg(feature = "zgc")]
            VmHeapInner::Zgc(h) => {
                h.try_alloc_object_with_descriptors(class_id, num_fields, descriptor_bytes)
            }
        })
    }

    pub fn try_alloc_array(
        &self,
        class_id: ClassId,
        element_type: ArrayElementType,
        length: usize,
    ) -> Option<ObjectRef> {
        // Round 12 wave 3 (lane iropt2, `r12w3-iropt2-array-length-cap-at-the-heap-patch`):
        // enforced below every front end; see [`VM_MAX_ARRAY_LENGTH`].
        if length > VM_MAX_ARRAY_LENGTH {
            return None;
        }
        let obj = Self::note_alloc_opt(match &self.inner {
            VmHeapInner::Generational(h) => h.try_alloc_array(class_id, element_type, length),
            VmHeapInner::G1(h) => h.try_alloc_array(class_id, element_type, length),
            #[cfg(feature = "zgc")]
            VmHeapInner::Zgc(h) => h.try_alloc_array(class_id, element_type, length),
        });
        // gci i14/mp finding 4.
        self.g1_check_old_usage_on_growth(obj);
        obj
    }

    // =====================================================================
    // Header access
    // =====================================================================

    pub fn get_header(&self, obj: ObjectRef) -> &ObjectHeader {
        dispatch!(&self.inner, get_header(obj))
    }

    pub fn class_id_of(&self, obj: ObjectRef) -> ClassId {
        // KINDOF-SENTINEL: see `kind_of` below — same idiom, same observed
        // sentinel (`0xFFFFFFFFFFFFFFFF`) reaching this dispatch unchecked.
        if !self.is_object_start(obj.as_ptr() as usize) {
            // The `ClassId(0)` this returns is what the H2 residual's own
            // `checkcast` reporter had to be taught to look past — see
            // `note_dead_base_deref`, which reports the swallow instead.
            self.note_dead_base_deref(obj, "class_id_of");
            return ClassId::new(0);
        }
        dispatch!(&self.inner, class_id_of(obj))
    }

    /// [`Self::class_id_of`] for a caller that has **just** obtained `obj` from
    /// [`Self::is_object_address`] and still holds it.
    ///
    /// The KINDOF-SENTINEL guard above is a full conservative header validation
    /// — region containment, kind/element tags, header plausibility, and an
    /// extent-fits-the-arena re-scan of the region table. It costs ~3 ns, which
    /// is nothing against a corrupted read, and everything when it is the third
    /// time the same address has been through it in one call.
    ///
    /// That is exactly what a compiled-code native call was doing:
    /// `try_jit_site_cached_native_dispatch` validates the receiver, then calls
    /// `class_id_of` (which validates it again), then `decode_dispatch_values`
    /// validates it a third time — measured in
    /// `jit::helpers::jit_native_dispatch_profile` at 3.2 ns for the validator
    /// and 5.0 ns for `class_id_of`, on a ~100 ns call.
    ///
    /// # Contract
    ///
    /// The caller must hold an `ObjectRef` that `is_object_address` returned
    /// `Some` for, on this heap, with no intervening safepoint. `ObjectRef`
    /// alone is not enough: the codebase constructs them from raw JIT slots and
    /// from JNI handles, and the sentinel above is the record of one arriving
    /// unvalidated. Anything less certain must keep using `class_id_of`.
    pub fn class_id_of_validated(&self, obj: ObjectRef) -> ClassId {
        if !validate_once_enabled() {
            return self.class_id_of(obj);
        }
        dispatch!(&self.inner, class_id_of(obj))
    }

    /// NEW-1.5 conservative validity check for a *raw stack-spill address*.
    ///
    /// Used by JIT frame root scanning to filter spurious values: returns
    /// `Some(ObjectRef)` only if `addr` lands on a live object header in
    /// this heap. See [`cratonvm_gc_gen::gen_heap::GenerationalHeap::is_object_address`]
    /// for the full contract.
    pub fn is_object_address(&self, addr: usize) -> Option<ObjectRef> {
        // No counter here any more (2026-09-23: this comment used to open with
        // "TOTAL walk counter, every caller" above a body that counts nothing).
        // The per-backend census that does exist is G1's
        // `g1_object_address_census`; the JIT-helper per-site census lives in
        // `vm/src/jit/helpers.rs`. A whole-VM total would belong here, behind a
        // gate, if one is ever needed again.
        match &self.inner {
            VmHeapInner::Generational(h) => h.is_object_address(addr),
            VmHeapInner::G1(h) => h.is_object_address(addr),
            #[cfg(feature = "zgc")]
            VmHeapInner::Zgc(h) => h.is_object_address(addr),
        }
    }

    /// [`Self::is_object_address`]'s yes/no, for the header guards
    /// (KINDOF-SENTINEL) that drop the `ObjectRef` it builds: the same
    /// predicate, without the construction's two thread-local accesses (the
    /// provenance record and the single-OS-thread diagnostic), which the
    /// caller's own reference already paid when it was built. Proposal
    /// `interpreter-L7-proposal-the-header-guards-validate-without-building-an-objectref-FIXED-20261013`
    /// (interpreter round i1 wave 49, lane L7).
    ///
    /// `#[inline(always)]` (wave 50, lane L7): the fat-LTO build left this
    /// three-way dispatch a call of its own, in front of the backend's
    /// `object_start` call, on the invoke doors' receiver check and the
    /// cast-site hit; `is_object_address`, which they asked before, had its
    /// dispatch inlined into them.
    #[inline(always)]
    pub fn is_object_start(&self, addr: usize) -> bool {
        match &self.inner {
            VmHeapInner::Generational(h) => h.is_object_start(addr),
            VmHeapInner::G1(h) => h.is_object_start(addr),
            #[cfg(feature = "zgc")]
            VmHeapInner::Zgc(h) => h.is_object_address(addr).is_some(),
        }
    }

    /// The header of the object at `addr`, when [`Self::is_object_start`]
    /// admits it: ONE validation and one dispatch, the kind, class id and
    /// element type then read through the one reference, where
    /// `kind_of_validated` followed by `class_id_of_validated` tested the
    /// validate-once gate and dispatched over the backends once per field.
    /// For a caller holding a raw address that its context types as a
    /// reference (the interpreter's cast-site hit reads its operand without
    /// decoding a `Value`; proposal
    /// `interpreter-L7-proposal-the-cast-hit-reads-the-receiver-header-in-one-call-FIXED-20261015`,
    /// interpreter round i1 wave 51, lane L7).
    ///
    /// `None` when the validator refuses the address, and also under
    /// `CRATONVM_GC_NO_VALIDATE_ONCE=1`: the caller then takes its
    /// re-validating path, as every `_validated` twin re-validates. The
    /// generational heap's header diagnostics (`CRATONVM_DBG_STALE_OBJREF`,
    /// the blocked-access gate) run as its `get_header` runs them.
    #[inline(always)]
    pub fn header_if_object_start(&self, addr: usize) -> Option<&ObjectHeader> {
        if !validate_once_enabled() || !self.is_object_start(addr) {
            return None;
        }
        match &self.inner {
            // SAFETY: `is_object_start` admitted `addr` on this heap.
            VmHeapInner::Generational(h) => Some(unsafe { h.header_at_object_start(addr) }),
            // SAFETY: `is_object_start` admitted `addr`: a live header of this
            // heap, which outlives `&self`. These backends' `get_header` is the
            // same cast.
            _ => Some(unsafe { &*(addr as *const ObjectHeader) }),
        }
    }

    /// [`Self::is_object_address`] for the conservative JIT stack-band root
    /// scans, whose candidates must be object STARTS.
    ///
    /// On G1 it adds a region-end size screen
    /// (`G1Collector::is_object_address_for_root_scan`): a header-shaped
    /// interior word whose decoded object would cross its region end is
    /// rejected, because only a humongous object crosses one, and those are
    /// accepted at their base alone. The Generational heap's check is already
    /// exact, and ZGC's answers from its live-base registry, so both keep the
    /// plain answer. It accepts a subset of what `is_object_address`
    /// accepts, and drops only words that are not objects. It is kept off the
    /// read barrier's path (`load_and_forward`), which does not need the screen.
    pub fn is_object_address_for_root_scan(&self, addr: usize) -> Option<ObjectRef> {
        match &self.inner {
            VmHeapInner::G1(h) => h.is_object_address_for_root_scan(addr),
            _ => self.is_object_address(addr),
        }
    }

    /// `[lo, hi)` envelope containing every address [`Self::is_object_address`]
    /// can possibly accept, or `None` when the backend cannot cheaply supply
    /// one.
    ///
    /// Purely an optimization hint for conservative stack scanning: a word
    /// outside the envelope is definitely not an object address, so the
    /// caller can skip the full per-word validator. A word inside it still
    /// has to go through `is_object_address` — the envelope is a **filter**,
    /// never an answer. Rooting a word on the strength of the range test alone
    /// would accept object *interiors* as bases, which is precisely the
    /// unsoundness [`Self::is_addr_live`]'s ZGC arm was fixed for (see the
    /// coalesced-free-block argument on that arm below).
    pub fn conservative_addr_span(&self) -> Option<(usize, usize)> {
        match &self.inner {
            VmHeapInner::Generational(h) => h.conservative_addr_span(),
            VmHeapInner::G1(h) => h.conservative_addr_span(),
            // ZGC answers `None` — but NOT, as this doc used to claim, because
            // "ZGC keeps live bases in a registry, not a contiguous arena".
            // The registry is only the live-base *index*; `ZgcRealHeap` backs
            // every object and array with one `Mutex<Arena>` (`zgc.rs:1464`)
            // through the single chokepoint `alloc_raw` (`zgc.rs:1772`), and
            // that arena is built once by `with_capacity` (`zgc.rs:1629`) and
            // never grown (no `Arena::grow` call exists in `zgc.rs`). The
            // envelope therefore EXISTS and is immutable for the heap's
            // lifetime — `[Arena::base_ptr(), +Arena::capacity())` — it is
            // simply not reachable from here: the field is private to the
            // `zgc` module and the only bound it publishes is
            // `heap_capacity()`, a length with no base.
            //
            // What the `None` costs: `conservative_roots.rs:4052-4064` hoists
            // this envelope out of the JIT frame scan exactly so the
            // overwhelming majority of stack words — return addresses, ints,
            // native pointers — die on an inline compare. With `None` every
            // 8-byte stack word instead calls `ZgcRealHeap::is_object_address`
            // (`zgc.rs:1892`), whose first act is `self.registry.lock()`: one
            // mutex acquire PER STACK WORD, per root-gathering pass, per
            // thread. `zgc-vmheap-arm-audit.md` §3.4 (AW-5) names this
            // as a competing explanation for the 35 PASS→HANG classes in
            // `zgc-real-fullsuite-regression-RETIRED-20260807.md`,
            // whose ApplicationContext boot/teardown shape is exactly deep
            // stacks × many threads. `zgc.rs:1467-1474` records that the same
            // shape already "read as a hang at scale" once — that fix covered
            // only the exact-base probe, never the per-word lock.
            //
            // Landed 2026-08-07: `ZgcRealHeap::conservative_addr_span` now
            // publishes the arena envelope, captured once in `with_capacity`
            // as two plain `usize` fields and answered without taking the
            // arena lock — the span exists to let the caller reject a word
            // with a range compare and NO lock, so locking to answer it
            // would defeat the point. Sound because the arena is never
            // grown. The arm's old `None` was justified by "ZGC keeps live
            // bases in a registry, not a contiguous arena" — a false
            // premise, and the reason this went unfixed.
            #[cfg(feature = "zgc")]
            VmHeapInner::Zgc(h) => h.conservative_addr_span(),
        }
    }

    /// The collection-overlay owners the last collection found dead in its
    /// collection set, by pre-collection address, taken (cleared) by the call.
    /// G1 only — the other backends follow owner edges in a marker whose
    /// verdict the post-collection prune already reads. See
    /// `G1Collector::take_overlay_condemned_owners` for why the prune's
    /// generic liveness answer is not enough on G1.
    pub fn take_overlay_condemned_owners(&self) -> rustc_hash::FxHashSet<usize> {
        match &self.inner {
            VmHeapInner::G1(h) => h.take_overlay_condemned_owners(),
            _ => rustc_hash::FxHashSet::default(),
        }
    }

    /// `[lo, hi)` covering every address THIS heap has reserved for Java
    /// objects, or `None` when it has published none. Per heap -- read from
    /// the collector itself, not from the process-global
    /// [`cratonvm_gc_core::heap_geometry`] table, whose slots a heap another VM built a
    /// moment later may have overwritten.
    ///
    /// Built for [`cratonvm_gc_core::compressed_oops::admit_heap`], which fit-checks a
    /// joining VM against the process's narrow-oop window (gc-common w8-f), and
    /// that use needs the RESERVATION, not the committed prefix: the window
    /// must cover every address the heap can ever produce. Today each arm's
    /// [`Self::conservative_addr_span`] is exactly that, and this delegates to
    /// it: Generational's three arena `[base, base + capacity)` bounds (the
    /// same values it publishes into `heap_geometry`; a young arena that later
    /// moves is caught by `compressed_oops::assert_region_encodable` on
    /// republish), G1's `arena_base .. arena_base + reserved_len()`, ZGC's
    /// never-grown arena. If an arm's filter span is ever tightened to the
    /// committed prefix, this must stop delegating to it.
    pub fn reserved_address_envelope(&self) -> Option<(usize, usize)> {
        self.conservative_addr_span()
    }

    /// BUG-03 — whether this heap backend supports the cross-thread STW JIT
    /// TLAB skip-region protocol, i.e. it can collect safely while a frozen
    /// in-JIT peer holds an un-retired TLAB:
    ///
    /// - Generational: the young collection degrades to the non-moving sweep
    ///   that consumes [`GenerationalHeap::set_jit_tlab_skip_regions`].
    /// - G1 (INT-3): the published tails are skipped by every region walker
    ///   and their regions (plus every region holding a conservative frozen-
    ///   peer root — the VM pins those via
    ///   [`cratonvm_gc_core::gc_quiescence::add_pinned_jit_root`]) are excluded from the
    ///   CSet, so nothing a frozen peer can address moves.
    /// - ZGC: since 2026-09-02 [`Self::refill_tlab`] DOES hand ZGC mutators a
    ///   chunk (`zgc/vm_tlab.rs`, kill switch `CRATONVM_ZGC_JIT_TLAB=0`), so
    ///   un-retired tails exist there too. The sweep walks the allocation-base
    ///   REGISTRY, never linear memory, so a tail (which holds no registered
    ///   base) is invisible to it by construction; the SLIDE consumes the
    ///   published list -- `relocate_stw` withholds every page a tail touches
    ///   from the relocation set and never lowers the bump cursor below a
    ///   tail's end. A frozen peer's conservative roots are ordinary
    ///   (pinned-by-design) mark roots.
    ///   The pre-2026-09-02 argument ("`refill_tlab` returns `None` on the
    ///   `Zgc` arm, so un-retired tails cannot exist") is GONE and must not be
    ///   reasoned from.
    ///   Do NOT reuse the "non-moving STW mark-sweep" justification that stood
    ///   here until 2026-09-01: `ZgcRealHeap` COMPACTS by default
    ///   (`CRATONVM_ZGC_RELOCATE`, default-on since 2026-08-13). Whether a
    ///   frozen in-JIT peer is safe against a MOVING cycle is a separate
    ///   question, decided by `zgc_relocation_permitted` and
    ///   `CRATONVM_ZGC_RELOCATE_UNDER_PROVEN_JIT` in `gc/src/zgc.rs` and not
    ///   by this predicate.
    ///
    /// The collector only engages the forcible in-JIT-peer take-over when
    /// this is `true` — now on every backend.
    pub fn supports_jit_tlab_skip(&self) -> bool {
        true
    }

    /// Return `(base, len)` of the contiguous backing memory region containing
    /// `addr`, or `None` if `addr` does not fall inside any mapped arena span.
    ///
    /// Guarantees `addr >= base && addr - base < len` when `Some((base, len))`
    /// is returned, and checks each backing region independently so gaps
    /// between disjoint generational arenas never match.
    pub fn arena_span_containing(&self, addr: usize) -> Option<(usize, usize)> {
        if addr == 0 {
            return None;
        }
        let contains = |base: usize, len: usize| -> Option<(usize, usize)> {
            if base != 0 && len > 0 && addr >= base && (addr - base) < len {
                Some((base, len))
            } else {
                None
            }
        };
        match &self.inner {
            VmHeapInner::Generational(h) => {
                for (base, _used, committed, _capacity) in h.exit_summary_arenas() {
                    if let Some(span) = contains(base, committed) {
                        return Some(span);
                    }
                }
                None
            }
            VmHeapInner::G1(h) => {
                let (base, _end) = h.reserved_span();
                let committed = h.committed_bytes();
                contains(base, committed)
            }
            #[cfg(feature = "zgc")]
            VmHeapInner::Zgc(h) => {
                h.wait_for_concurrent_relocate_idle();
                h.committed_span_containing(addr)
            }
        }
    }

    pub fn backing_arena_spans(&self) -> Vec<(usize, usize)> {
        match &self.inner {
            VmHeapInner::Generational(h) => {
                let arenas = h.exit_summary_arenas();
                vec![
                    (arenas[0].0, arenas[0].3),
                    (arenas[1].0, arenas[1].3),
                    (arenas[2].0, arenas[2].3),
                ]
            }
            VmHeapInner::G1(h) => {
                let (base, end) = h.reserved_span();
                vec![(base, end.saturating_sub(base))]
            }
            #[cfg(feature = "zgc")]
            VmHeapInner::Zgc(h) => h.committed_arena_spans(),
        }
    }

    /// BUG-03 / INT-3 — publish the reserved TLAB tails of forcibly-stopped
    /// in-JIT peers so the collection skips them (non-moving-sweep skip list
    /// on Generational; region-walker skip + CSet exclusion on G1; page
    /// withholding + bump-cursor floor for the slide on ZGC -- see
    /// [`Self::supports_jit_tlab_skip`]).
    pub fn set_jit_tlab_skip_regions(&self, regions: &[(usize, usize)]) {
        match &self.inner {
            VmHeapInner::Generational(h) => h.set_jit_tlab_skip_regions(regions),
            VmHeapInner::G1(h) => h.set_jit_tlab_skip_regions(regions),
            #[cfg(feature = "zgc")]
            VmHeapInner::Zgc(h) => h.set_jit_tlab_skip_regions(regions),
        }
    }

    /// BUG-03 / INT-3 — clear any published JIT TLAB skip regions.
    pub fn clear_jit_tlab_skip_regions(&self) {
        match &self.inner {
            VmHeapInner::Generational(h) => h.clear_jit_tlab_skip_regions(),
            VmHeapInner::G1(h) => h.clear_jit_tlab_skip_regions(),
            #[cfg(feature = "zgc")]
            VmHeapInner::Zgc(h) => h.clear_jit_tlab_skip_regions(),
        }
    }

    /// DIAGNOSTIC-ONLY (cceres3, CRATONVM_DBG_BLOCKGC frame-desync hunt): if
    /// `addr` is a heap address whose header is a forwarding marker (an
    /// already-evacuated old address kept readable by the stale-objref
    /// quarantine ring), return the forwarded (new) address. `None` when the
    /// stale-objref canary is off, the address is outside the heap, or the
    /// header is a live header. Generational backend only — the others never
    /// quarantine old addresses.
    pub fn debug_forwarded_target(&self, addr: usize) -> Option<usize> {
        match &self.inner {
            VmHeapInner::Generational(h) => h.debug_forwarded_target(addr),
            _ => None,
        }
    }

    /// DIAGNOSTIC-ONLY (cce0079): collection epoch for the `[SETFIELD-GC]`
    /// assertion ("did a GC complete inside this store?") and for test
    /// messages that count collections.
    ///
    /// Generational: its minor-GC count. G1 and ZGC: their whole collection
    /// count (every G1 pause and every ZGC cycle can move objects, which is
    /// the only thing the epoch is asked about). Until 2026-09-23 those two
    /// arms were a `_ => 0` catch-all, so the `[SETFIELD-GC]` probe could never
    /// fire on the default collector and every "minor GCs so far: 0" in a
    /// failing test's message was a constant, not a measurement.
    pub fn debug_minor_gc_count(&self) -> u64 {
        match &self.inner {
            VmHeapInner::Generational(h) => h.debug_minor_gc_count(),
            VmHeapInner::G1(_) => self.collection_count(),
            #[cfg(feature = "zgc")]
            VmHeapInner::Zgc(_) => self.collection_count(),
        }
    }

    /// The epoch the TLAB share sizer closes its sampling windows on
    /// (`Tlab::share_refill_request`): one tick per completed YOUNG pause,
    /// HotSpot's sample point for `TLABAllocationWeight`. gen r5w4/defaults8
    /// names the policy use of what [`Self::debug_minor_gc_count`] already
    /// returns (gen r5w2/alloc6 moved the sizer onto it), so a later change to
    /// the diagnostic cannot silently re-scale the sizer.
    ///
    /// Re-checked with the Generational tail sink on (its default since gen
    /// r5w2): `minor_gc_count` moves exactly once per completed young pause —
    /// at the end of the moving path, and inside the non-moving sweep for a
    /// diverted pause (`sweep_young_non_moving`, which the divert returns
    /// through) — including a pause that also collected the old generation
    /// (`major_gc_count` is separate, which is why `collection_count` was the
    /// wrong epoch there). A refused cycle moves neither. The tail sink
    /// retracts cursors and free-lists tails; it runs no collection and moves
    /// no counter, and the sizer's numerator
    /// (`Tlab::thread_allocated_bytes`) counts consumed bytes, not returned
    /// tails, so the sink leaves the sample exact. G1 and ZGC: their
    /// collection count, as `debug_minor_gc_count`.
    #[inline]
    pub fn young_pause_epoch(&self) -> u64 {
        self.debug_minor_gc_count()
    }

    /// Is the HotSpot-shaped TLAB share sizer (`CRATONVM_TLAB_SHARE_SIZER`)
    /// on for THIS heap? gen r5w4/defaults8: its default is per backend, and
    /// an explicit setting wins on all three
    /// (`GcFlags::tlab_share_sizer_for_backend`). Generational ON since gcg
    /// g6/jn, G1 ON since gci i48/zl3 (`alloc_policy_defaults::TLAB_SHARE_SIZER_G1`,
    /// which says why a G1 carve cannot overlap live memory), ZGC OFF. The first
    /// Generational flip
    /// was reverted at the r5w4 merge (`alloc_policy_defaults::TLAB_SHARE_SIZER`; see
    /// `docs/feature-designs/generative-gc-architecture.md`
    /// — gen r5w5/sizer9 corrected this line, which still said "ON on the
    /// Generational heap"). Per VM: the
    /// answer is the flag snapshot every other arm reads plus this heap's own
    /// variant, no process state. Read once per TLAB refill (never per
    /// allocation) by `tlab_alloc_shaped_inner`, which sizes the request and
    /// arms the new buffer's refill-waste limit with the same answer.
    #[inline]
    pub fn tlab_share_sizer_enabled(&self) -> bool {
        crate::gc_flags().tlab_share_sizer_for_backend(self.is_generational(), self.is_g1())
    }

    /// Resolve `addr` to the base of the object it points into, for PINNING.
    ///
    /// More permissive than [`Self::is_heap_addr`]: it accepts a MISALIGNED
    /// interior pointer and a ONE-PAST-THE-END cursor, both of which that
    /// method rejects and both of which a frozen peer's registers hold. See
    /// `ZgcRealHeap::resolve_interior_for_pin`. Non-ZGC arms fall back, so this
    /// is a no-op there.
    pub fn resolve_interior_for_pin(&self, addr: usize) -> Option<ObjectRef> {
        match &self.inner {
            #[cfg(feature = "zgc")]
            VmHeapInner::Zgc(h) => h.resolve_interior_for_pin(addr),
            _ => self.is_heap_addr(addr),
        }
    }

    /// Loose validity check: alignment + heap-region containment.
    ///
    /// Unlike [`Self::is_object_address`] this does NOT read the object
    /// header. Used by operand-stack root scanning to root ambiguous
    /// JVM-long-vs-jobject slots without rejecting legitimate references
    /// whose header is transiently unreadable (interior pointers, mid-
    /// initialisation slots, or stale-bit-pattern Long slots that the GC
    /// is well-equipped to ignore via its own size-sanity guard).
    //
    // Doc placement, 2026-09-20: this block had slid onto
    // `resolve_interior_for_pin` above, which meant the method that is
    // DEFINED as "alignment + containment" carried no contract at all while
    // its more-permissive sibling carried two. The three ZGC-arm comments in
    // the body below are the load-bearing part and are unchanged.
    pub fn is_heap_addr(&self, addr: usize) -> Option<ObjectRef> {
        match &self.inner {
            VmHeapInner::Generational(h) => h.is_heap_addr(addr),
            VmHeapInner::G1(h) => h.is_heap_addr(addr),
            // ZGC: apply this method's own stated screen — *alignment* +
            // containment — before entering the backend, because
            // `ZgcRealHeap::is_heap_addr` (`zgc.rs:1902`) is the one
            // implementation that performs neither test. Both shipping
            // backends open with this exact line (`gen_heap.rs:3293`,
            // `g1.rs:7601`); ZGC goes straight to `registry.lock()`.
            //
            // Why it matters more here than there: on ZGC a *miss* is not
            // cheap. After the exact-base hash probe misses, `is_heap_addr`
            // drops the registry lock, RE-TAKES it, and walks the entire live
            // registry dereferencing each base's header to test extents —
            // O(live) per probe, under the mutex (`zgc.rs:1908-1919`). The
            // callers are per-slot conservative root scanners over ambiguous
            // JVM-long-vs-jobject operand slots (`value_stack.rs:1301`,
            // `:1587`, `memory/roots.rs:200`, `frame.rs:1831`,
            // `interpreter/gc_and_alloc.rs:4260`), whose dominant population
            // is zeros, small integers and long bit patterns — every one of
            // which currently buys a full walk of the heap.
            // `zgc-vmheap-arm-audit.md` §3.4 (AW-5), one of the two
            // instrument-separable hypotheses for the 35 PASS→HANG classes in
            // `zgc-real-fullsuite-regression-RETIRED-20260807.md`.
            //
            // Why it cannot lose a root. The guard only ever returns `None`
            // sooner; it can never turn a `None` into a `Some`, so no interior
            // address can be promoted to a base by it.
            //   * `addr == 0`: no live base is 0 and `0 >= base` is false for
            //     every base, so the extent walk already answered `None`. Pure
            //     work elimination, bit-identical result.
            //   * misaligned: every ZGC allocation base is 8-aligned
            //     (`alloc_raw` calls `arena.alloc(size, 8)`, `zgc.rs:1775`),
            //     so no *base* is reachable this way and none can be dropped.
            //     Only a misaligned *interior* word could previously have been
            //     rooted, and that is a word Generational and G1 have rejected
            //     since this method existed — this arm converges on the
            //     contract, it does not invent one.
            //
            // See the `TODO(zgc)` on `conservative_addr_span` above: once
            // `ZgcRealHeap` publishes its arena envelope, the range compare
            // belongs here too, ahead of the lock.
            #[cfg(feature = "zgc")]
            VmHeapInner::Zgc(h) => {
                if addr == 0 || addr & 0x7 != 0 {
                    return None;
                }
                h.is_heap_addr(addr)
            }
        }
    }

    /// Diagnostic decomposition of [`Self::is_addr_live`] into its two arms,
    /// plus where the address actually sits.
    ///
    /// `is_addr_live` ORs an old-gen allocation test with a young-survivor
    /// test, so a `false` is indistinguishable between "the old-gen block is on
    /// the free list", "the address is in the wrong semispace" and "it is in
    /// neither generation". Those have completely different causes, and the one
    /// consumer that DESTROYS state on a `false` — the collection-overlay prune
    /// — has to be debugged against the specific arm.
    ///
    /// Returns `(old_gen_allocated, young_survivor, region)`.
    pub fn liveness_arms(&self, addr: usize) -> (bool, bool, &'static str) {
        match &self.inner {
            VmHeapInner::Generational(h) => {
                let old = h.is_live_old_gen_addr(addr);
                let young = h.is_live_young_survivor(addr);
                let region = if h.is_in_old(addr as *const u8) {
                    "old-gen"
                } else if h.is_heap_addr(addr).is_some() {
                    "young"
                } else {
                    "off-heap"
                };
                (old, young, region)
            }
            _ => (false, false, "n/a"),
        }
    }

    /// Rate limiter for the stale-barrier census in
    /// [`Self::load_and_forward_inner`], keyed by CALL SITE.
    ///
    /// Returns true at most once per distinct Rust backtrace, and at most
    /// `MAX_SITES` times overall. `CRATONVM_DBG_VACATED_FRAMES` only -- the
    /// capture alone is far too expensive for any other run.
    #[cold]
    #[inline(never)]
    pub(in crate::vm_heap) fn stale_barrier_site_is_new() -> bool {
        use std::collections::HashSet;
        use std::hash::{Hash, Hasher};
        const MAX_SITES: usize = 40;
        static SEEN: std::sync::Mutex<Option<HashSet<u64>>> = std::sync::Mutex::new(None);
        let bt = std::backtrace::Backtrace::force_capture().to_string();
        let mut h = std::collections::hash_map::DefaultHasher::new();
        bt.hash(&mut h);
        let key = h.finish();
        let mut g = match SEEN.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        let set = g.get_or_insert_with(HashSet::new);
        if set.len() >= MAX_SITES {
            return false;
        }
        set.insert(key)
    }

    /// T1.7.1 — Brooks-pointer read barrier.
    ///
    /// Consults the object's compact header: if the `LockState` is
    /// `Forwarded`, the object has been evacuated by a concurrent
    /// compaction cycle and the real object lives at
    /// `header.forwarding_ptr()`. The barrier self-heals stale
    /// pointers by returning the forwarded address.
    ///
    /// Under stop-the-world GC this is **always a no-op fast path**
    /// because the header's lock state is only set to `Forwarded`
    /// during evacuation, and the collector updates all root
    /// references before resuming mutators. The barrier becomes
    /// meaningful when concurrent compaction is enabled and a
    /// mutator loads a reference *while* the collector is relocating
    /// the target object.
    ///
    /// The fast path is a single load + mask + branch: an empty
    /// inline-able sequence on both x86-64 and AArch64. The slow
    /// path (forwarded) is one additional mask operation on the
    /// 30-bit shifted address.
    ///
    /// Idempotent: calling `load_and_forward` on an already-forwarded
    /// object returns the *terminal* forwarding target in one hop
    /// because Brooks pointers only ever point "forward" (the
    /// evacuation pass never builds chains longer than one link).
    ///
    /// # Safety
    ///
    /// The caller must hold a live root to `obj` (or the GC must be
    /// stopped) so the header read is against a valid object. The
    /// returned `ObjectRef` points into the same heap region.
    ///
    /// # Backend compatibility
    ///
    /// Every `VmHeap` backend lays objects out with the 16-byte
    /// `cratonvm_types::ObjectHeader`; a forwarded object carries its target in
    /// the mark word (`MARK_FORWARDED`), read through
    /// `ObjectHeader::forwarding_address`. No backend uses
    /// `cratonvm_gc_core::compact_header::CompactHeader`. (This paragraph described a
    /// 32-byte header with a separate `forwarding_ptr` field until 2026-09-23;
    /// that layout has not existed since 2026-08-07.)
    //
    // The two doc blocks above describe the public `load_and_forward` family;
    // the one below is this private helper's own.
    /// Shared body of [`Self::load_and_forward`],
    /// [`Self::load_and_forward_checked`] and
    /// [`Self::load_and_forward_validated`].
    ///
    /// `pre_validated` is the caller's promise that `is_object_address` has
    /// ALREADY answered `Some` for `obj` on this heap with no intervening
    /// safepoint - the same contract as [`Self::class_id_of_validated`]. It
    /// suppresses the ENTRY walk only; every other check below is unchanged,
    /// including the forwarding-target validation.
    ///
    /// The returned flag says whether the reference handed back is a
    /// **validated live base**. Only a walk that actually happened inside
    /// this call may set it: the `forwarded_after_slide` answer is reported
    /// UNvalidated even though the relocation table only holds live bases,
    /// because that keeps the flag's meaning to one sentence a caller can
    /// check rather than a chain of invariants it has to trust.
    //
    // DOC PLACEMENT, 2026-09-20. The two blocks above had slid onto
    // the `stale_barrier_site_is_new` helper: it was inserted between them
    // and their function, so that private rate limiter carried the Brooks-barrier
    // contract and this function carried none. The `#[inline]` that used to sit
    // BETWEEN the two doc blocks went with them and landed on a
    // `#[cold] #[inline(never)]` helper, where the two attributes contradict;
    // it is dropped rather than moved, because this function already has one.
    #[inline]
    pub(in crate::vm_heap) fn load_and_forward_inner(&self, obj: ObjectRef, pre_validated: bool) -> (ObjectRef, bool) {
        // KINDOF-SENTINEL: `obj` itself has been observed already invalid
        // here (not merely forwarded-to-garbage — see the forwarding-target
        // check below) when the caller's own value was reconstructed across
        // a JIT bail-to-interpreter boundary with a live JIT frame the
        // collector couldn't get a precise root map for. There is no valid
        // object to hand back in that case; returning `obj` unchanged is a
        // no-op, not a fix, but it at least avoids adding a SECOND
        // dereference on top of the one about to fail below, and every
        // reproduced crash site downstream (`kind_of`, `class_id_of`,
        // `element_type_of`, `identity_hash_code`) now validates its own
        // input independently — see those methods below.
        // `CRATONVM_DBG_VACATED_FRAMES`: was this barrier just asked to repair a
        // reference the collector really did move, and did it fail?
        //
        // This barrier repairs by reading a FORWARDING WORD at the OLD address.
        // ZGC's slide has none to read: `Arena::compact_low_to` zeroes the span
        // above the new cursor and the memmove overwrites everything below it,
        // so a stale reference either lands on zeroed bytes (not a registered
        // base — the early return right below) or on another live object (whose
        // header is not forwarded — the `is_forwarded` return after it). Every
        // caller that treats this call as the repair for "a collection may have
        // run since the frame read" is therefore unprotected on the DEFAULT
        // collector. The ledger is exact (`gc_quiescence::note_allocated`
        // removes re-issued addresses), so a hit here is proof, not a
        // suspicion.
        if cratonvm_gc_core::gc_quiescence::vacated_frames_enabled() {
            if let Some(moved_to) = cratonvm_gc_core::gc_quiescence::was_vacated(obj.as_ptr() as usize) {
                // ONE REPORT PER CALL SITE, not per occurrence.
                //
                // This barrier is the choke point every raw `ObjectRef` a
                // native still holds passes through -- `forward_boundary_value`
                // for `set_field` / `set_array_element`, `forward_boundary_args`
                // for the `invoke_*` family -- so a hit names a native that
                // captured a reference before an allocation and used it after.
                // The population is a handful of distinct sites hit thousands
                // of times each, and a flat count of 12 reported the first site
                // twelve times and every other one never. Keyed by the
                // backtrace so the census is of SITES.
                if Self::stale_barrier_site_is_new() {
                    tracing::error!(
                        target: "cratonvm::gc::guard",
                        obj = format!("{:#x}", obj.as_ptr() as usize),
                        moved_to = format!("{moved_to:#x}"),
                        backtrace = %std::backtrace::Backtrace::force_capture(),
                        "load_and_forward was handed a reference the collector MOVED, and cannot repair it: this collector leaves no forwarding word at the vacated address. The caller in the backtrace is holding a stale ObjectRef that nothing else will fix.",
                    );
                }
            }
        }
        if !pre_validated && !self.is_object_start(obj.as_ptr() as usize) {
            // Not a live base. On a collector that leaves a forwarding word
            // this is the end of the road; ZGC's slide leaves none, so ask its
            // relocation table instead — that is the whole point of
            // `ZgcRealHeap::relocations`, and without it this barrier is a
            // silent no-op on the default collector.
            #[cfg(feature = "zgc")]
            if let VmHeapInner::Zgc(h) = &self.inner {
                if let Some(moved_to) = h.forwarded_after_slide(obj.as_ptr() as usize) {
                    // SAFETY: `forwarded_after_slide` only answers with an
                    // address the object-start registry currently holds, i.e. a
                    // live object base inside the arena.
                    return (unsafe { ObjectRef::from_raw(moved_to as *mut u8) }, false);
                }
            }
            return (obj, false);
        }
        // SAFETY: the caller guarantees `obj` is a live root. Every
        // current backend lays out `ObjectHeader` at offset 0 of the
        // ObjectRef pointer with `forwarding_ptr` at the documented
        // structural offset; reading the field is well-formed.
        let header = unsafe { &*(obj.as_ptr() as *const cratonvm_gc_core::heap::ObjectHeader) };
        if !header.is_forwarded() {
            return (obj, true);
        }
        let addr = header.forwarding_address();
        if addr.is_null() {
            // Defensive fallback (shouldn't happen — is_forwarded()
            // already checks for a non-null pointer — but kept for
            // belt-and-braces against concurrent-GC races we did not
            // anticipate). Returning the original pointer is always
            // safe because the original object still exists in memory
            // until the evacuation epoch ends.
            return (obj, true);
        }
        // KINDOF-SENTINEL: `header` itself may be corrupted (the same
        // implausible-header family `old_gen::scan_region` guards against —
        // see `validate_header_tags_or_desync`), and unlike `kind_tag`/
        // `elem_tag`, the forwarding-pointer word was never validated
        // before this call trusted it outright. A corrupted header can set
        // the forwarded bit and hand back a garbage `forwarding_address()`
        // — observed as the all-ones sentinel `0xFFFFFFFFFFFFFFFF` — which
        // every caller of this read barrier then dereferences unchecked
        // (`kind_of`, `get_field`, `array_length`, ...). Validate the
        // extracted address is actually a live object in this heap before
        // trusting it; an implausible target falls back to the original
        // pointer, exactly like the null-address case above.
        if !self.is_object_start(addr as usize) {
            return (obj, true);
        }
        // SAFETY: the forwarding pointer was installed by the GC and
        // points at a valid object header within this heap.
        (unsafe { ObjectRef::from_raw(addr) }, true)
    }

    /// The software read barrier: repair `obj` if the collector moved it.
    ///
    /// This is **not** the ZGC colored-word load barrier, and it cannot be
    /// taught to be one: its parameter is an [`ObjectRef`] -- a machine
    /// pointer the caller has already fabricated -- not a reference SLOT, so
    /// a colored word never reaches it in a form it could repair. Handed one
    /// it would build an `ObjectRef` out of bit-63 bits, fail
    /// [`Self::is_object_address`], fall through the `forwarded_after_slide`
    /// lookup and return the same word unchanged: a silent no-op that also
    /// violates `zgc::vaddr::debug_assert_plain_word`. The colored-word
    /// barrier is [`Self::load_ref_slot_barriered`], which takes the slot.
    #[inline]
    pub fn load_and_forward(&self, obj: ObjectRef) -> ObjectRef {
        self.load_and_forward_inner(obj, false).0
    }

    /// [`Self::load_and_forward`], also reporting whether the reference it
    /// hands back is a validated live base.
    ///
    /// `load_and_forward` returns its argument UNCHANGED when validation
    /// fails, so its result is not safe to pass to a `_validated` twin - the
    /// twin would dereference a pointer nothing has checked. This variant
    /// hands the caller the one bit needed to tell the two cases apart, and
    /// costs nothing: the walk that decides it has already happened inside.
    #[inline]
    pub fn load_and_forward_checked(&self, obj: ObjectRef) -> (ObjectRef, bool) {
        self.load_and_forward_inner(obj, false)
    }

    /// [`Self::load_and_forward`] for a caller that has **just** validated
    /// `obj` through [`Self::is_object_address`] and still holds it.
    ///
    /// Skips the entry walk only. The result is always a validated live base:
    /// with the entry branch suppressed, every remaining exit either returns
    /// the caller's already-validated `obj` or a forwarding target this call
    /// validated itself.
    ///
    /// # Contract
    ///
    /// As [`Self::class_id_of_validated`]: `is_object_address` must have
    /// answered `Some` for `obj`, on this heap, with no intervening safepoint.
    #[inline]
    pub fn load_and_forward_validated(&self, obj: ObjectRef) -> ObjectRef {
        self.load_and_forward_inner(obj, validate_once_enabled()).0
    }

    /// Read a heap reference **SLOT** through this backend's load barrier.
    ///
    /// The value returned is a plain machine address (`0` = null) -- exactly
    /// what [`cratonvm_types::narrow_oop::read_ref_slot`] returns on a
    /// non-colored backend. A caller may therefore still run
    /// `plausible_heap_pointer` afterwards, and MUST run it on the *result*
    /// rather than on the slot word: a colored word is deliberately
    /// implausible, so filtering the word first is what silently nulls a live
    /// reference (risk J1 of `docs/feature-designs/zgc-jit-load-barrier.md`).
    ///
    /// # Why this exists at all, and why it is here and not in `vm/`
    ///
    /// The seven raw reference reads in `vm/src/jit/helpers.rs` panic as a
    /// tripwire on a colored word instead of barriering it. The two of them
    /// that still hold a SLOT when the plausibility filter runs -- the
    /// reference-array element load and the compact-reference field load --
    /// now funnel through `helpers::jit_load_ref_slot`, and this is the
    /// function that seam calls. It could not be written in `vm/`:
    /// `zgc::barrier::z_load` needs a `C: ZBarrierContext`, whose only
    /// production implementor is `ZgcRealHeap`, and `VmHeap` publishes no
    /// accessor that hands the context out. Dispatching here keeps every ZGC
    /// detail inside `gc/` instead of growing a per-site ZGC special case,
    /// which is the shape that already missed `emit_load_string_value_ptr`
    /// once.
    ///
    /// # What each arm does
    ///
    /// * `Generational` / `G1`: today's `read_ref_slot`, byte for byte.
    ///   Neither collector has a load barrier and neither may gain one here.
    /// * `Zgc`, barrier NOT armed: the same plain read. This is the only path
    ///   any shipping configuration takes -- see "Today it cannot fire".
    /// * `Zgc`, barrier armed: [`cratonvm_gc_zgc::zgc::ZgcRealHeap::load_barrier_slot`],
    ///   which is the one in-tree implementation of the colored-word barrier
    ///   and already gets right the two conversions a fresh one gets wrong. It
    ///   views the slot as an `AtomicU64`, runs
    ///   `barrier::load_barrier_fast_bad`, and on a bad color calls
    ///   `load_barrier_slow` (forward the offset, publish to the marker,
    ///   CAS-heal the slot). Crucially it then converts **offset to address**:
    ///   `z_load` hands back a bare 42-bit heap OFFSET, not a pointer, and
    ///   returning it uncorrected is a silent truncation that
    ///   `gc/src/zgc/relocate.rs` and `gc/src/zgc/mark.rs` both already carry
    ///   doc comments warning about. Re-deriving that conversion here rather
    ///   than delegating would be a second copy of exactly the knowledge those
    ///   comments say must live in one place.
    ///
    /// # TODAY IT CANNOT FIRE -- the armed arm is dead code
    ///
    /// No colored word is stored in a heap slot in any shipping
    /// configuration, so landing this changes nothing measurable:
    ///
    /// * `vm/src/vm/vm_init.rs` pins `const RELOCATION_REQUESTED: bool =
    ///   false`.
    /// * `ZgcRealHeap::set_barrier_color` -- the sole writer of the colored
    ///   state -- has no non-test caller. Its own comment records the
    ///   obligation on the first one.
    /// * `barrier_good_mask` is initialised to `vaddr::Z_REMAPPED` and nothing
    ///   moves it, so `load_barrier_armed()` is false for the process
    ///   lifetime and `zgc::vaddr::color` has no production caller.
    ///
    /// The unarmed cost is therefore one relaxed `AtomicBool` load and a
    /// not-taken branch on top of the read that already happened.
    ///
    /// # P1 -- SLOT WIDTH: compressed oops REFUSE the barrier, they do not get one
    ///
    /// `z_load` takes `&AtomicU64`, which is a promise about the SLOT: 8-byte
    /// aligned, exactly 8 bytes, and valid for WRITES, because the slow path
    /// self-heals with `slot.compare_exchange(observed, healed, AcqRel,
    /// Acquire)`. With `narrow_oops_enabled()` the slot is FOUR bytes
    /// (`read_ref_slot` branches on exactly that), so the `AtomicU64` view
    /// would read and CAS four bytes of the neighbouring field -- the same
    /// class of bug `emit_load_string_value_ptr` had to be fixed for once.
    ///
    /// A 4-byte path was considered and REJECTED, not deferred: a colored word
    /// is `Z_COLORED_TAG | color | 42-bit offset`, i.e. bit 63 plus bits 42-46
    /// plus a 42-bit payload. It does not fit in 32 bits under any encoding,
    /// so there is no narrow colored word for a narrow barrier to operate on.
    /// ZGC plus compressed oops is unsupported until the slot representation
    /// itself changes, which is `zgc-reference-slot-representation.md`'s
    /// problem and not this function's.
    ///
    /// So the narrow arm REFUSES. Unarmed it takes the same plain
    /// `read_ref_slot` as everything else (identical behaviour, and the only
    /// reachable case). Armed it panics, deliberately: the alternative is to
    /// hand compiled code a truncated colored word, and this subsystem's house
    /// rule -- the same one that makes `ZBarrierContext::on_forward_failure`
    /// return `!` -- is that an unrepresentable reference fails loudly rather
    /// than degrading to a wrong pointer. That panic is unreachable twice
    /// over: `vm/src/vm/vm_init.rs` already refuses the ZGC + compressed-oops
    /// combination at startup, and nothing arms the barrier. It is asserted
    /// here anyway because `narrow_oops_enabled()` reads a process-global
    /// `AtomicBool` that any code can set, so the init-time gate is a fact
    /// about a default run and not an invariant of this call -- the hardening
    /// `zgc-reference-slot-representation.md` asks for.
    ///
    /// # P3 -- offset 0 / null ambiguity: answered, with one residual
    ///
    /// `ZFastPath::Good(0)` is ambiguous between null and an object at heap
    /// offset 0 (`vaddr::color_offset_roundtrip_many_offsets` asserts offset 0
    /// is a legal non-null location). `load_barrier_slot` disambiguates it the
    /// way `barrier.rs` says a Rust caller can and machine code cannot: it
    /// re-reads the raw word and answers `None` only for `vaddr::Z_NULL`. So
    /// the decision is made once, here, and not per caller.
    ///
    /// RESIDUAL for whoever arms this: that null test is a SECOND load, so a
    /// mutator store landing between the barrier's load and it can be observed
    /// as "offset 0" rather than null, yielding the arena base instead of `0`.
    /// It is harmless while nothing is armed and it is not fixable without
    /// `ZFastPath::Good` carrying the raw word alongside the offset. The
    /// durable fix is open question 8 of `zgc-jit-load-barrier.md`: reserve
    /// offset 0 in the page allocator so the ambiguity has no legal instance.
    /// Risk J6 of that document is the same fact seen from the JIT side.
    ///
    /// # P4 -- `on_forward_failure` returns `!`: NOT handled here, and cannot be
    ///
    /// A `ZBarrierContext::forward` that answers `None` panics rather than
    /// returning. That is a property of `ZgcRealHeap::forward`, not of this
    /// call: it returns `Some(addr)` unconditionally while `relocate_active`
    /// is false, which is always, so the panic is unreachable today. It stops
    /// being unreachable the moment relocation is real and the forwarding
    /// table can miss, and no wrapper here can turn it into a recoverable
    /// answer -- the `!` return type is the whole point. Whoever makes
    /// relocation real owns that decision at `ZgcRealHeap::forward`.
    ///
    /// # Ordered work list -- THIS FILE IS THE AUTHORITY
    ///
    /// Folded in from `.agent-requests/A9-gc-barrier.txt`. These are the steps
    /// that must complete, in order, before `vm/src/vm/vm_init.rs`'s
    /// `RELOCATION_REQUESTED` may be flipped.
    ///
    /// **Status changes go HERE and nowhere else.**
    /// `gc/src/zgc/census.rs`'s `ZSlotShape::word_is_atomically_accessed_today`
    /// carried a second copy of this sequence until 2026-09-01. The two drifted
    /// apart inside three weeks and ended up each describing the other as the
    /// stale one, which is what two copies of an ordered sequence buy. That
    /// copy is now a pointer to this list plus the per-shape facts only it
    /// knows; do not start a third. If another file needs the status, cite this
    /// item.
    ///
    /// Status as of 2026-09-01. Every label below is a one-command check and
    /// the command is named; re-run it rather than trusting the label.
    ///
    /// 1. **DONE for the Rust writers (2026-09-01).** Every other writer of a
    ///    slot this barrier may CAS must be atomic: a plain write racing
    ///    `load_barrier_slow`'s `compare_exchange` on one location is a data
    ///    race, and the defect is the NON-ATOMICITY, not the ordering.
    ///    `cratonvm_types::narrow_oop::read_ref_slot` / `write_ref_slot` are
    ///    now `Relaxed` atomics in the wide (`AtomicU64`) and narrow
    ///    (`AtomicU32`) arms alike, with the four-part argument for `Relaxed`
    ///    rather than something stronger written out above them in
    ///    `types/src/narrow_oop.rs`. This item quoted a plain
    ///    `(ptr as *mut u64).write(addr)` until 2026-09-01; that write no
    ///    longer exists, and `grep -n 'mut u64).write' types/src/narrow_oop.rs`
    ///    is the check.
    ///    Still open under this heading, and tracked on the `LegacyField` row
    ///    of `zgc::census::ZSlotShape::atomicity_debt_note`: the collector-side
    ///    16-byte `Value` writers in `gc/src/gc.rs`, `gc/src/gen_heap.rs` and
    ///    `gc/src/g1.rs` are still plain `ptr::write` / `ptr::write_unaligned`.
    ///    Tracked, not blocking -- those are the Generational and G1 evacuation
    ///    loops, which never run over a `ZgcRealHeap`, so they are not slots
    ///    this barrier can reach and CAS.
    ///    The JIT-emitted inline reference stores under `jit/src/x64/` are NOT
    ///    this item's problem and never were: machine code is not a Rust memory
    ///    access, an aligned qword `mov` cannot tear against a `lock cmpxchg`,
    ///    and no Rust UB is in play. What they have is a COVERAGE obligation,
    ///    which is step 6.
    /// 2. **DONE.** The armed test:
    ///    [`cratonvm_gc_zgc::zgc::ZgcRealHeap::load_barrier_armed`] already existed, so
    ///    no new accessor was needed. Only its slot helper had to widen from
    ///    private to `pub(crate)`.
    /// 3. **DONE.** This function, with P1/P3/P4 decided above.
    ///    **Extended, DONE (2026-09-01), inside `gc/`:** the three
    ///    ZGC-internal accesses to a word the barrier would CAS --
    ///    `ZgcRealHeap::relocate_stw`'s compaction slot-rewrite STORE, and the
    ///    legacy payload READS in `ZgcRealHeap::visit_strong_refs_at` and in
    ///    `zgc::census::reference_slots` (all `gc/src/zgc.rs`). The compaction
    ///    store is the one that mattered: its SAFETY note rested on "the world
    ///    is stopped", which is precisely the property step 7 removes.
    /// 4. **HALF DONE.** `vm/`: route `helpers::jit_load_ref_slot`'s
    ///    `read_ref_slot(slot)` through this call. That seam is the single
    ///    chokepoint and both slot-holding sites funnel through it; it now
    ///    calls `VmHeap::load_ref_slot_barriered` on its `Some(&VmHeap)` arm
    ///    and falls back to a raw `read_ref_slot` on its `None` arm.
    ///    Site B, the compact-reference field load, is DONE (2026-09-01):
    ///    `jit_getfield` takes `vm_ptr` and has `vm` bound already, so it
    ///    passes `Some(&vm.mem.heap)` and dispatches here.
    ///    Site A, `jit_aaload`, is NOT, and cannot be without an ABI change --
    ///    it receives no `vm_ptr` and so has no route to a `&VmHeap`, and takes
    ///    the seam's raw arm. The exact change is written out in
    ///    `.agent-requests/B8-abi.txt` and is IN FLIGHT, not landed; check the
    ///    signature (`grep -n 'fn jit_aaload' vm/src/jit/helpers.rs`) before
    ///    believing either state. The census
    ///    `ref_load_census::COLORED_WORDS_SEEN` is what proves afterwards that
    ///    no Category-A site was missed.
    /// 5. **NOT DONE.** Sites D/E/F of `zgc-jit-load-barrier.md` 2.5.1, which
    ///    hold an `ObjectRef` rather than a slot: their barriers belong
    ///    upstream at `types/src/value.rs`'s `read_value_atomic` reference arm
    ///    and at `vm::get_static_shared`, where they are shared with the
    ///    interpreter rather than duplicated. A static slot is not atomic
    ///    today and so may not be CAS-healable -- it may need a non-healing
    ///    barrier kind. Blocked on the `StaticField` row of
    ///    `zgc::census::ZSlotShape::word_is_atomically_accessed_today`.
    /// 6. **DONE for the emitters (2026-09-01); the residual it names is not
    ///    closable here.** The nine Category-A inline emission points of 2.3
    ///    are kept routed to the helpers by
    ///    `x64::narrow_oops_block_inline_fields()`, which is
    ///    `narrow_oops_enabled() || zgc_read_barrier_blocks_inline_fields()`
    ///    (`jit/src/x64/licm.rs`). `aastore` was the one site that emitted the
    ///    slot load and the element store inline without consulting it -- not
    ///    the UB of step 1 but a coverage hole, since an armed cycle would read
    ///    a coloured word with no colour test and write a plain pointer into a
    ///    slot the barrier next classifies as `Good` and truncates to 42 bits.
    ///    That gate landed at the `0x53` arm of
    ///    `jit/src/x64/bytecode_walk.rs`, with `AASTORE_SITES_WALKED` as the
    ///    denominator that makes its expected ZERO fallback count readable as
    ///    "consulted and correctly declined" rather than "never reached".
    ///    The residual: an emission-time gate cannot reach ALREADY COMPILED
    ///    sequences, so arming must happen at a safepoint. That is step 7's
    ///    obligation, and it is recorded on `ZgcRealHeap::set_barrier_color`.
    /// 7. **NOT DONE.** Only then flip `RELOCATION_REQUESTED`.
    ///
    /// # Safety
    ///
    /// `slot` must point at a live reference slot of the current width
    /// ([`cratonvm_types::narrow_oop::ref_field_size`]) inside a live object --
    /// the same contract as `read_ref_slot`. On the armed ZGC path the slot
    /// must additionally be valid for WRITES, because the barrier self-heals
    /// it; every reference slot inside a live heap object is.
    #[inline]
    pub unsafe fn load_ref_slot_barriered(&self, slot: *const u8) -> u64 {
        // Written as an early return on the one arm that differs rather than
        // as a `match`, so the Generational/G1/unarmed-Zgc answer is LITERALLY
        // the expression `jit_load_ref_slot` evaluates today. A `match` with
        // three arms spelling the same call is three places for them to drift
        // apart, and "landing this changes nothing measurable" has to be
        // checkable by reading rather than by benchmarking.
        #[cfg(feature = "zgc")]
        if let VmHeapInner::Zgc(h) = &self.inner {
            return Self::zgc_load_ref_slot_barriered(h, slot);
        }
        cratonvm_types::narrow_oop::read_ref_slot(slot)
    }

    /// zt t18/zb1 (zr1br A2a): is this a ZGC heap whose read barrier is armed?
    /// `false` on every other backend and on every default run. For a raw
    /// reader outside `gc/` that holds a `&VmHeap` and must decline to a
    /// barriered accessor while armed (`jit_getfield`'s legacy-cell arm).
    #[inline]
    pub fn zgc_load_barrier_armed(&self) -> bool {
        #[cfg(feature = "zgc")]
        if let VmHeapInner::Zgc(h) = &self.inner {
            return h.load_barrier_armed();
        }
        false
    }

    /// Fast raw read of compact `Reference` slot 0 (`referent`), avoiding
    /// `ObjectRef::from_raw` / `Value` boxing on `Reference.get0()`.
    #[inline]
    pub fn fast_read_compact_ref_slot0(
        &self,
        obj_addr: usize,
        resolve_off: impl FnOnce(u32) -> Option<usize>,
    ) -> Option<usize> {
        #[cfg(feature = "zgc")]
        if let VmHeapInner::Zgc(h) = &self.inner {
            return h.fast_read_compact_ref_slot0(obj_addr, resolve_off);
        }
        if cratonvm_types::narrow_oop::narrow_oops_enabled() {
            return None;
        }
        let obj = self.is_object_address(obj_addr)?;
        let header = self.get_header(obj);
        if header.is_forwarded()
            || header.kind() != cratonvm_types::ObjectKind::Object
            || !cratonvm_types::is_compact_object(&header)
        {
            return None;
        }
        let off = resolve_off(header.class_id.as_u32())?;
        // SAFETY: `off` was validated against this class's 64-bit compact slot layout
        // on a verified live `Object` with narrow oops disabled.
        let raw = unsafe {
            &*((obj.as_ptr() as usize + off) as *const std::sync::atomic::AtomicUsize)
        }
        .load(std::sync::atomic::Ordering::Relaxed);
        if raw != 0 {
            let referent = self.is_object_address(raw)?;
            if self.satb_armed() {
                self.referent_keep_alive(Value::Object(Some(referent)));
            }
            return Some(referent.as_ptr() as usize);
        }
        Some(0)
    }

    /// zt t19/zb2 (zr1br A2b): is this a ZGC heap armed with COLORED stores
    /// (`ZgcRealHeap::arm_load_barrier_colored_stores`, tests only)? A raw
    /// reference writer outside `gc/` that holds a `&VmHeap` delegates its
    /// store to the heap accessor while this holds (the JIT's `jit_aastore` /
    /// `jit_putfield_object`), because only the accessor encodes the word.
    /// `false` on every other backend and on every production heap.
    #[inline]
    pub fn zgc_load_barrier_colored_stores(&self) -> bool {
        #[cfg(feature = "zgc")]
        if let VmHeapInner::Zgc(h) = &self.inner {
            return h.load_barrier_colored_stores();
        }
        false
    }

    /// A reference compare-and-swap on one slot of an ARMED ZGC heap, as
    /// heal-then-CAS (nio21 wave 9 L3; `ZgcRealHeap::cas_ref_slot_healed` has
    /// the loop and its assumptions). `expected` / `new` are machine addresses,
    /// `0` for null. `Some((witness_address, swapped))`; `None` -- the caller
    /// keeps its own path -- on every other backend, on an unarmed ZGC heap
    /// (whose slots hold plain pointers, so a plain CAS is right), under
    /// compressed oops (a coloured word does not fit a 4-byte slot), and for an
    /// unencodable `new`.
    ///
    /// # Safety
    ///
    /// As [`Self::load_ref_slot_barriered`]: `slot` is a live, 8-aligned
    /// reference slot inside a live object, valid for writes.
    #[inline]
    pub unsafe fn cas_ref_slot_barriered(
        &self,
        slot: *mut u8,
        expected: u64,
        new: u64,
    ) -> Option<(u64, bool)> {
        #[cfg(feature = "zgc")]
        if let VmHeapInner::Zgc(h) = &self.inner {
            if cratonvm_types::narrow_oop::narrow_oops_enabled() || slot as usize % 8 != 0 {
                return None;
            }
            return h
                .cas_ref_slot_healed(slot as usize, expected as usize, new as usize)
                .map(|(w, ok)| (w as u64, ok));
        }
        let _ = (slot, expected, new);
        None
    }

    /// The raw reference read of declared field `slot` of a COMPACT instance,
    /// as the machine address the field holds (`0` for null), or `None`: a
    /// legacy (16-byte cell) body, an array, a slot the current compact layout
    /// does not have, a non-reference row, or a slot whose width / alignment is
    /// not the heap's reference width.
    ///
    /// JIT round 23 wave 1, lane L4 (`jit-r21-nio-proposals-w17-L2.md` item 2):
    /// the one copy of "compact displacement + barriered reference load" that
    /// the VM-side Rust helpers reading a known JDK field share, instead of each
    /// growing its own (`tl_load_ref_field`, `jit_getfield`'s compact arm,
    /// `field_fast`'s `load_ref`). The load goes through
    /// [`Self::load_ref_slot_barriered`] (plain `read_ref_slot` on Generational,
    /// G1 and an unarmed ZGC heap; the healing barrier while ZGC is armed; the
    /// compressed-oop width either way). Unlike `get_field` it does NOT unbox an
    /// `AUTOBOX_CLASS_ID` wrapper: a caller that may meet one screens the word
    /// itself.
    ///
    /// r23w1 (R23W1L4-4): the reader has no production caller. The three
    /// readers it was meant to replace each hold something it lacks (the JIT's
    /// per-thread displacement proof, the interpreter site's cached offset,
    /// the `ThreadLocal` helpers' proof-cache hit); see the L4 page's closure.
    /// It stays as the tested reference form for a VM-side caller that has
    /// no cached displacement.
    ///
    /// # Safety
    ///
    /// `obj` is a live, already-forwarded object of this heap that cannot move
    /// before the caller stops using the answer (no safepoint in between); on
    /// an armed ZGC heap the slot is also written (the heal), which every
    /// reference slot of a live object allows.
    #[inline]
    pub unsafe fn load_ref_field_compact(&self, obj: ObjectRef, slot: usize) -> Option<u64> {
        let header = self.get_header(obj);
        if header.kind() != ObjectKind::Object || !cratonvm_types::is_compact_object(header) {
            return None;
        }
        let domain = match &self.inner {
            VmHeapInner::Generational(h) => h.layout_domain(),
            VmHeapInner::G1(h) => h.layout_domain(),
            #[cfg(feature = "zgc")]
            VmHeapInner::Zgc(h) => h.layout_domain(),
        };
        // Domain-screened: a class id is per-`ClassStore`, so the blind form
        // could answer another VM's layout for the same number.
        let (disp, storage) = cratonvm_types::field_layout::compact_field_disp_storage_in(
            domain,
            header.class_id.as_u32(),
            slot,
        )?;
        let width = cratonvm_types::narrow_oop::ref_field_size();
        if !storage.is_reference() || storage.size_runtime() as usize != width {
            return None;
        }
        let addr = (obj.as_ptr() as usize).checked_add(disp)?;
        if addr % width != 0 {
            return None;
        }
        // SAFETY: `disp` is the compact displacement of reference field
        // `slot` in the layout of `obj`'s class, so `addr` is a naturally
        // aligned reference slot of the heap's width inside the live body.
        Some(unsafe { self.load_ref_slot_barriered(addr as *const u8) })
    }

    /// Does this heap's layout domain own `class_id`'s registered compact
    /// layout? The domain screen of [`Self::load_ref_field_compact`], asked
    /// once per object by the readers that then take the whole layout from the
    /// current-layout cache, which is not screened. `false` for a class with no
    /// fields, which those readers have no use for anyway.
    pub(in crate::vm_heap) fn owns_compact_layout(&self, class_id: ClassId) -> bool {
        let domain = match &self.inner {
            VmHeapInner::Generational(h) => h.layout_domain(),
            VmHeapInner::G1(h) => h.layout_domain(),
            #[cfg(feature = "zgc")]
            VmHeapInner::Zgc(h) => h.layout_domain(),
        };
        cratonvm_types::field_layout::compact_field_disp_storage_in(domain, class_id.as_u32(), 0)
            .is_some()
    }

    /// The reference fields of a compact instance, from field `from_slot` on,
    /// pushed onto `out` in slot order as the addresses [`Self::get_field`]
    /// hands back as references: nulls are left out, and a word the slot's
    /// wrapper screen unboxes into a primitive is left out too, exactly as
    /// `get_field` would have answered a primitive there.
    ///
    /// One layout lookup for the object, and only the oop-map's slots are read,
    /// through [`Self::load_ref_slot_barriered`]. The G1 remark's reference
    /// tracer walks millions of objects (a dead `ZipFileSystem`'s entry index)
    /// and paid a full `get_field` -- validation, layout lookup, coercion -- for
    /// every field, primitives included
    /// (`docs/feature-designs/jit-ir-optimizations.md`).
    ///
    /// `false`, with nothing pushed: not a compact instance, a reference width
    /// other than the heap's, more than one layout domain in the process (the
    /// current-layout cache is not domain-screened), or a field-access
    /// diagnostic armed. The caller then reads slot by slot.
    ///
    /// # Safety
    ///
    /// As [`Self::load_ref_field_compact`]: `obj` is a live, already-forwarded
    /// object of this heap that cannot move before the caller is done with the
    /// answer.
    pub unsafe fn push_compact_reference_words(
        &self,
        obj: ObjectRef,
        from_slot: usize,
        out: &mut Vec<usize>,
    ) -> bool {
        let flags = crate::gc_flags();
        if flags.dbg_badref || flags.dbg_deadref_store || flags.dbg_stale_objref {
            return false;
        }
        let header = self.get_header(obj);
        if usize::BITS < 64
            || header.kind() != ObjectKind::Object
            || !cratonvm_types::is_compact_object(header)
            || !self.owns_compact_layout(header.class_id)
        {
            return false;
        }
        let class_id = header.class_id;
        let width = cratonvm_types::narrow_oop::ref_field_size();
        let start = out.len();
        // `(slot << 32) | displacement` of each reference field, then each
        // entry replaced by the word it holds: no second buffer.
        let planned = cratonvm_types::with_current_class_layout(class_id.as_u32(), |layout| {
            for slot in from_slot..layout.field_count() {
                let storage = layout.field_storage(slot)?;
                if !storage.is_reference() {
                    continue;
                }
                let disp = layout.field_disp(slot)? as usize;
                if storage.size_runtime() as usize != width || disp % width != 0 {
                    return None;
                }
                out.push((slot << 32) | disp);
            }
            Some(())
        })
        .flatten()
        .is_some();
        if !planned {
            out.truncate(start);
            return false;
        }
        let mut kept = start;
        for i in start..out.len() {
            let (slot, disp) = (out[i] >> 32, out[i] & 0xffff_ffff);
            // SAFETY: `disp` is the compact displacement of reference field
            // `slot` of `obj`'s class, at the heap's reference width and
            // alignment, inside the live body.
            let word = unsafe { self.load_ref_slot_barriered(obj.as_ptr().add(disp) as *const u8) }
                as usize;
            if word == 0 {
                continue;
            }
            // SAFETY: only the tag is read unless the screen validates it.
            let value = Value::Object(Some(unsafe { ObjectRef::from_raw(word as *mut u8) }));
            let value = cratonvm_gc_core::autobox::unbox_reference_field(
                value,
                class_id,
                slot,
                |r| {
                    let r = self.is_object_address(r.as_ptr() as usize)?;
                    Some(self.class_id_of(r))
                },
                |r| self.get_field(r, 0),
            );
            if let Value::Object(Some(r)) = value {
                out[kept] = r.as_ptr() as usize;
                kept += 1;
            }
        }
        out.truncate(kept);
        true
    }

    /// The `Zgc` arm of [`Self::load_ref_slot_barriered`]. Split out so the
    /// common path above stays one branch and one call, and so the ZGC
    /// preconditions sit next to the code that depends on them.
    ///
    /// # Safety
    ///
    /// As [`Self::load_ref_slot_barriered`].
    #[cfg(feature = "zgc")]
    #[inline]
    pub(in crate::vm_heap) unsafe fn zgc_load_ref_slot_barriered(h: &ZgcRealHeap, slot: *const u8) -> u64 {
        // P1. Order matters: test the WIDTH before the armed flag, so the
        // unsupported combination is refused rather than silently reading and
        // CAS-ing 8 bytes out of a 4-byte slot. The unarmed narrow read below
        // is the ordinary one, which is what keeps a compressed-oops run
        // byte-identical.
        if cratonvm_types::narrow_oop::narrow_oops_enabled() {
            assert!(
                !h.load_barrier_armed(),
                "ZGC colored-pointer load barrier armed while compressed oops are enabled: \
                 the reference slot is 4 bytes and a colored word does not fit in 32 bits \
                 (Z_COLORED_TAG is bit 63). vm_init refuses this combination at startup and \
                 the one production arming (arm_load_barrier_plain_stores) refuses compressed \
                 oops, so reaching here means one of those two facts changed without this \
                 function being revisited. Refusing \
                 rather than truncating -- see load_ref_slot_barriered, section P1."
            );
            return cratonvm_types::narrow_oop::read_ref_slot(slot);
        }
        if !h.load_barrier_armed() {
            // The only path any shipping configuration takes.
            return cratonvm_types::narrow_oop::read_ref_slot(slot);
        }
        // `AtomicU64` requires 8-byte alignment, and `compare_exchange` on a
        // misaligned address is UB rather than a slow path. Every reference
        // slot is 8-aligned by construction (`HEADER_SIZE` and `SLOT_SIZE` are
        // both multiples of 8, and `ARRAY_DATA_OFFSET` likewise); this is a
        // debug assert because it is an invariant of the layout, not an input
        // to be validated on every load.
        debug_assert_eq!(
            slot as usize % 8,
            0,
            "reference slot must be 8-byte aligned for the AtomicU64 view the load barrier takes"
        );
        // zt t19/zb2: the JIT-seam engagement count (census only).
        h.note_load_barrier_jit_slot();
        // Delegates the color test, the slow-path heal, the null
        // disambiguation (P3) and the OFFSET -> ADDRESS conversion. `None` is
        // null, which this signature spells `0`, matching `read_ref_slot`.
        h.load_barrier_slot(slot as usize).map_or(0, |a| a as u64)
    }

    // `get_compact_header` was deleted 2026-09-23 (gc-common w2-e, lane G's
    // handoff `handoff-g-vm-heap-stale-header-docs.md`): it had no caller, and
    // on every backend it would have decoded the 16-byte header's
    // `class_id`/`shape` word as a `CompactHeader`.

    // KINDOF-SENTINEL (2026-08-04): `kind_of`/`element_type_of`/
    // `identity_hash_code` are the innermost dispatch point for the whole
    // `#[repr(u8)]`-header-validation family this file's `old_gen`/
    // `gen_heap` siblings already guard on the GC-internal walk side (see
    // `validate_header_tags_or_desync`). Those fixes hardened the
    // COLLECTOR's own scan of old-gen; they never touched this VM-level
    // read barrier, which every MUTATOR path (interpreter dispatch, JIT
    // helpers, native array/field accessors) funnels through with a bare
    // `ObjectRef` and no independent check of its own. Reproduced: a JIT
    // bail-to-interpreter transition on a thread whose innermost frame
    // belonged to an unguarded/unregistered JIT callee (no precise root
    // map for that root-gathering pass) handed one of these a receiver
    // that read back as the all-ones sentinel `0xFFFFFFFFFFFFFFFF` —
    // `EXCEPTION_ACCESS_VIOLATION` reading exactly that address, at three
    // distinct call sites (`kind_of` itself via
    // `dispatch_virtual::execute_invokevirtual_cached`, `class_id_of` via
    // `NativeHeapAccess::get_field`, and `load_and_forward` via a fourth).
    // `load_and_forward` (above) now validates the forwarding target it
    // hands back, but a corrupted header can also be reached directly
    // without ever going through that barrier, so each of these validates
    // independently rather than trusting an already-validated caller.
    pub fn kind_of(&self, obj: ObjectRef) -> ObjectKind {
        if !self.is_object_start(obj.as_ptr() as usize) {
            self.note_dead_base_deref(obj, "kind_of");
            return ObjectKind::Object;
        }
        dispatch!(&self.inner, kind_of(obj))
    }

    /// [`Self::kind_of`] for a caller holding a validated `ObjectRef`.
    /// Same contract as [`Self::class_id_of_validated`].
    pub fn kind_of_validated(&self, obj: ObjectRef) -> ObjectKind {
        if !validate_once_enabled() {
            return self.kind_of(obj);
        }
        dispatch!(&self.inner, kind_of(obj))
    }

    /// [`Self::element_type_of`] for a caller holding a validated
    /// `ObjectRef`. Same contract as [`Self::class_id_of_validated`].
    pub fn element_type_of_validated(&self, obj: ObjectRef) -> ArrayElementType {
        if !validate_once_enabled() {
            return self.element_type_of(obj);
        }
        dispatch!(&self.inner, element_type_of(obj))
    }

    pub fn element_type_of(&self, obj: ObjectRef) -> ArrayElementType {
        if !self.is_object_start(obj.as_ptr() as usize) {
            self.note_dead_base_deref(obj, "element_type_of");
            return ArrayElementType::Reference;
        }
        dispatch!(&self.inner, element_type_of(obj))
    }

    /// `CRATONVM_DBG_VACATED_FRAMES`: somebody just dereferenced an address
    /// that is **not a live object base**, and the sentinel guards above
    /// swallowed it into a default.
    ///
    /// This is the EARLY face of the stale-holder family, and the one every
    /// other instrument is blind to. A holder left naming an address the slide
    /// vacated reads a zeroed corpse until the allocator hands the span out
    /// again: `is_object_address` says no, `class_id_of` answers `ClassId(0)`,
    /// `kind_of` answers `Object`, and the caller carries on with a plausible
    /// default. Nothing is thrown, so nothing is reported — and by the time the
    /// address IS re-issued and the failure becomes visible as a
    /// `ClassCastException`, the exact vacated ledger has already dropped the
    /// entry (that pruning is what makes it exact) and every consumption-point
    /// detector goes quiet. That gap is why the H2 MVStore-writer residual
    /// could be measured, cornered to "a raw ObjectRef in VM-side state", and
    /// still not named.
    ///
    /// The backtrace is the whole point: it names the Rust frame holding the
    /// reference, which is the one fact none of the Java-side evidence carries.
    /// `moved_to` distinguishes the two reasons an address fails the live-base
    /// test — the collector moved the object (a stale holder, this defect) or
    /// the value was never an object at all (the `0xFFFF..` sentinel family the
    /// guards above were originally written for).
    #[cold]
    pub(in crate::vm_heap) fn note_dead_base_deref(&self, obj: ObjectRef, site: &'static str) {
        if !cratonvm_gc_core::gc_quiescence::vacated_frames_enabled() {
            return;
        }
        let addr = obj.as_ptr() as usize;
        // Null is not a stale holder; it is the ordinary absent reference, and
        // reporting it would bury the signal.
        if addr == 0 {
            return;
        }
        static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        if N.fetch_add(1, std::sync::atomic::Ordering::Relaxed) >= 24 {
            return;
        }
        let moved_to = cratonvm_gc_core::gc_quiescence::was_vacated(addr)
            .or_else(|| self.zgc_forwarded_after_slide(addr));
        tracing::error!(
            target: "cratonvm::gc::guard",
            site,
            obj = format!("{addr:#x}"),
            moved_to = moved_to.map(|a| format!("{a:#x}")).unwrap_or_else(|| "<unknown>".into()),
            was_vacated = moved_to.is_some(),
            backtrace = %std::backtrace::Backtrace::force_capture(),
            "a dereference of an address that is NOT a live object base was \
             swallowed into a default. When `was_vacated` is true this is a \
             holder the collector moved out from under and nothing repaired — \
             the caller in the backtrace is the one holding it.",
        );
    }

    /// ZGC's slide ledger, for [`Self::note_dead_base_deref`]. `None` on every
    /// other backend (they leave a forwarding word instead, which
    /// `load_and_forward` reads).
    pub(in crate::vm_heap) fn zgc_forwarded_after_slide(&self, addr: usize) -> Option<usize> {
        match &self.inner {
            #[cfg(feature = "zgc")]
            VmHeapInner::Zgc(h) => h.forwarded_after_slide(addr),
            _ => None,
        }
    }

    pub fn identity_hash_code(&self, obj: ObjectRef) -> i32 {
        // KINDOF-SENTINEL: see `kind_of` above.
        if !self.is_object_start(obj.as_ptr() as usize) {
            return 0;
        }
        dispatch!(&self.inner, identity_hash_code(obj))
    }

    /// H1: mint a fresh non-zero identity hash code for use at object
    /// construction time. Matches the value the slow-path allocators
    /// (`alloc_object` / `alloc_array`) write into the header. The TLAB
    /// fast path in `interpreter::init_object_header` calls this so all
    /// freshly allocated objects have a non-zero `identity_hash_code`
    /// header field, eliminating false-positive "Stale pointer detected"
    /// warnings when a fresh `new Object()` (cid=0, fields=0, hash=0)
    /// otherwise produces an all-zero first 16 bytes that the detector
    /// can't distinguish from genuine stale memory.
    pub fn next_identity_hash(&self) -> i32 {
        dispatch!(&self.inner, next_hash())
    }

    // =====================================================================
    // Field access
    // =====================================================================

    /// Both halves of the vacated-address instrument, fired on one accessor's
    /// receiver. `site` is what separates "who read through it" from "who wrote
    /// through it" in the report, so every call site passes a distinct string.
    ///
    /// A stale receiver makes every field read off it return whatever now
    /// occupies that memory — a perfectly VALID object of the wrong class,
    /// which is why the value being pushed looks clean and the `checkcast` one
    /// instruction later does not. That is the consumption point neither the
    /// stack-push nor the frame-local detector can see.
    ///
    /// # The write side (gen r4w2/obs, 2026-09-23)
    ///
    /// Until round 4 wave 2 only `get_field` called this. A write through a
    /// stale receiver is strictly worse than a read through one: it corrupts
    /// the live, unrelated object that now occupies the address, whose owner
    /// has no idea a field was overwritten, and the damage surfaces arbitrarily
    /// later in a third place with no backtrace — so `set_field`,
    /// `set_field_volatile`, `set_field_as`, `set_field_volatile_as`,
    /// `set_array_element` and `get_array_element` now call it too, each with
    /// its own site string.
    ///
    /// What held it back for three rounds was the default-path price on
    /// **every reference store in the VM**: the two probes each did their own
    /// `vacated_frames_enabled()` load and branch. The gate is now tested ONCE
    /// here, inline, and both probes live behind it in a `#[cold]`
    /// out-of-line call — so a store pays one relaxed load of a read-mostly
    /// `AtomicU8` and a never-taken branch, and `get_field` pays half what it
    /// paid before. The A/B the page asks for (`bench/HashMapOnly.java`, flag
    /// OFF, interleaved) is in the round-4 wave-2 obs review's probe list; if
    /// it shows a cost, drop the four `set_*` sites and keep the cold ones.
    /// See `docs/feature-designs/generative-gc-architecture.md`.
    #[inline]
    pub(in crate::vm_heap) fn check_receiver(&self, obj: ObjectRef, site: &'static str, receiver_site: &'static str) {
        if cratonvm_gc_core::gc_quiescence::vacated_frames_enabled() {
            Self::check_receiver_armed(obj.as_ptr() as usize, site, receiver_site);
        }
    }

    /// The armed half of [`Self::check_receiver`]; kept out of line so the
    /// accessors inline only the gate.
    #[cold]
    #[inline(never)]
    pub(in crate::vm_heap) fn check_receiver_armed(addr: usize, site: &'static str, receiver_site: &'static str) {
        // `CRATONVM_DBG_VACATED_FRAMES`: is the RECEIVER of this access an
        // address the collector moved an object away from?
        cratonvm_gc_core::gc_quiescence::report_vacated_receiver(addr, site);
        // The re-issue-proof half of the same question — see
        // `gc_quiescence::stale_use_verdict` for why the exact ledger above
        // cannot answer it.
        cratonvm_gc_core::gc_quiescence::check_stale_use(addr, receiver_site);
    }

    pub fn get_field(&self, obj: ObjectRef, index: usize) -> Value {
        self.check_receiver(obj, "get_field", "get_field receiver");
        dispatch!(&self.inner, get_field(obj, index))
    }

    /// zt t23/st1 (item 29a): a reference-field read that never keeps its
    /// target alive -- ZGC's `ZBarrierKind::Weak` load
    /// (`ZgcRealHeap::get_field_weak`), for the VM's reference round reading a
    /// referent slot to decide its fate. Identical to [`Self::get_field`]
    /// whenever ZGC's read barrier is unarmed and on every other backend
    /// (their `get_field` marks nothing).
    pub fn get_field_weak(&self, obj: ObjectRef, index: usize) -> Value {
        self.check_receiver(obj, "get_field", "get_field receiver");
        match &self.inner {
            #[cfg(feature = "zgc")]
            VmHeapInner::Zgc(h) => h.get_field_weak(obj, index),
            other => dispatch!(other, get_field(obj, index)),
        }
    }

    /// zt t32 (2f, `CRATONVM_ZGC_LOAD_BARRIER_MARKING`): is load-barrier
    /// marking armed? Then the store side logs only a PLAIN word's deletion
    /// ([`Self::satb_barrier_slot`]); the ZGC accessors apply the rule
    /// themselves. `false` off ZGC.
    #[inline]
    pub fn zgc_load_barrier_marking(&self) -> bool {
        match &self.inner {
            #[cfg(feature = "zgc")]
            VmHeapInner::Zgc(h) => h.load_barrier_marking(),
            _ => false,
        }
    }

    /// zt t32 (2f): the SATB pre-barrier of a store about to overwrite the
    /// 8-byte reference word at `slot`, whose decoded value is `old`. Under
    /// load-barrier marking the word is read raw and logged only when plain
    /// (`ZgcRealHeap::lbm_pre_barrier_slot`); otherwise exactly
    /// [`Self::satb_barrier`]`(old)`.
    #[inline]
    pub fn satb_barrier_slot(&self, slot: *const u8, old: Value) {
        #[cfg(feature = "zgc")]
        if let VmHeapInner::Zgc(h) = &self.inner {
            if h.load_barrier_marking() {
                if h.mark_active() {
                    h.lbm_pre_barrier_slot(slot);
                }
                return;
            }
        }
        let _ = slot;
        self.satb_barrier(old);
    }

    /// zt t32: [`Self::get_field_weak`] for a reference-array element
    /// (`ZgcRealHeap::get_array_element_weak`); the plain element read on
    /// every other backend.
    pub fn get_array_element_weak(&self, obj: ObjectRef, index: usize) -> Result<Value, i32> {
        match &self.inner {
            #[cfg(feature = "zgc")]
            VmHeapInner::Zgc(h) => h.get_array_element_weak(obj, index),
            other => dispatch!(other, get_array_element(obj, index)),
        }
    }

    pub fn set_field(&self, obj: ObjectRef, index: usize, value: Value) {
        self.check_receiver(obj, "set_field", "set_field receiver");
        // Task #25: this path fires the inherent write_barrier inside
        // `set_field`, which doesn't go through `VmHeap::write_barrier`
        // and would leave the triad sentinel armed. Clear it here so
        // the (pre, store-via-set_field) sequence closes cleanly.
        #[cfg(debug_assertions)]
        clear_pending_pre_barrier();
        dispatch!(&self.inner, set_field(obj, index, value))
    }

    /// Fallible twin of the descriptor-less [`set_field`](Self::set_field),
    /// for a store that may BOX a primitive into a compact reference slot
    /// (`autobox::box_for_reference_slot`).
    ///
    /// Returns `true` when the store happened exactly as `set_field` would
    /// have made it. Returns `false` when the value needed an
    /// `AUTOBOX_CLASS_ID` wrapper and the heap could not hold one without a
    /// collection: the slot then holds null and no wrapper exists. The caller
    /// must treat the object as unusable (the class-mirror populator drops the
    /// unpublished mirror and declines).
    ///
    /// gen r5w1/oom5, `gengc-r4w6-review6-class-mirror-creation-aborts-on-a-full-heap-outside-ldc-FIXED-20260927.md`
    /// item 2: the mirror's slot-0 class-id store used the backend's panicking
    /// `alloc_object(AUTOBOX_CLASS_ID, 1)` and aborted on a full heap even on
    /// the fallible mirror path. A store that does not box (a reference, or a
    /// legacy slot) is plain `set_field` and allocates nothing either way.
    pub fn try_set_field(&self, obj: ObjectRef, index: usize, value: Value) -> bool {
        if !cratonvm_gc_core::autobox::needs_reference_box(value) {
            self.set_field(obj, index, value);
            return true;
        }
        // SAFETY: `self` outlives `scope`: both live in this frame, and the
        // scope is dropped before this function returns.
        let scope = unsafe {
            cratonvm_gc_core::autobox::arm_fallible_wrapper(
                Self::try_alloc_wrapper_erased,
                (self as *const Self).cast::<()>(),
            )
        };
        self.set_field(obj, index, value);
        let stored = !scope.declined();
        drop(scope);
        stored
    }

    /// The [`cratonvm_gc_core::autobox::TryAllocWrapper`] of [`Self::try_set_field`]:
    /// the backend's non-aborting object entry, then the wrapper's own slot 0
    /// (a legacy cell: `AUTOBOX_CLASS_ID` never has a compact layout, so this
    /// store cannot box again).
    pub(in crate::vm_heap) fn try_alloc_wrapper_erased(heap: *const (), value: Value) -> Option<ObjectRef> {
        // SAFETY: armed only by `try_set_field` with `self`, which outlives
        // the scope this runs inside.
        let heap = unsafe { &*heap.cast::<VmHeap>() };
        let wrapper = heap.try_alloc_object_full(cratonvm_types::AUTOBOX_CLASS_ID, 1)?;
        heap.set_field(wrapper, 0, value);
        Some(wrapper)
    }

    /// INT-8: field store with the SATB pre-barrier SUPPRESSED. Reserved
    /// for the weak-reference PROTOCOL writes (the pre-collection referent
    /// null pass and the remark-time referent clears): those are not
    /// semantic overwrites, and SATB-logging them recorded every active
    /// referent as a mark root — the taint that made bitmap-based reference
    /// processing inert (see `G1Collector::set_field_no_satb`).
    ///
    /// **ZGC joined the suppressed set on 2026-08-16, and it had to.** This was
    /// a plain `set_field` on that backend, correctly, for as long as ZGC had
    /// no concurrent cycle and no armed pre-write barrier of its own. Genuine
    /// concurrent marking gave it both, and `ZgcRealHeap::set_field` now
    /// publishes the overwritten reference itself — so the unsuppressed arm
    /// would have handed the concurrent marker EVERY active referent as a mark
    /// root at the pre-collection null pass, which runs while the cycle is
    /// still armed. No weak, soft, phantom or cleaner reference would ever
    /// have been cleared again, and no reference test would have caught it:
    /// they are all satisfied by "the referent survived".
    ///
    /// Generational stays a plain `set_field` — its reference protocol never
    /// depended on hiding these writes (it uses the watched-referents channel).
    pub fn set_field_suppress_satb(&self, obj: ObjectRef, index: usize, value: Value) {
        #[cfg(debug_assertions)]
        clear_pending_pre_barrier();
        match &self.inner {
            VmHeapInner::G1(h) => h.collector.set_field_no_satb(obj, index, value),
            #[cfg(feature = "zgc")]
            VmHeapInner::Zgc(h) => h.set_field_no_satb(obj, index, value),
            other => dispatch!(other, set_field(obj, index, value)),
        }
    }

    pub fn get_field_volatile(&self, obj: ObjectRef, index: usize) -> Value {
        dispatch!(&self.inner, get_field_volatile(obj, index))
    }

    pub fn set_field_volatile(&self, obj: ObjectRef, index: usize, value: Value) {
        self.check_receiver(obj, "set_field_volatile", "set_field_volatile receiver");
        // Task #25: see `set_field` comment — also closes the triad
        // since `set_field_volatile` likewise routes through the
        // inherent write_barrier, bypassing `VmHeap::write_barrier`.
        #[cfg(debug_assertions)]
        clear_pending_pre_barrier();
        dispatch!(&self.inner, set_field_volatile(obj, index, value))
    }

    // ----- T10.9.E descriptor-aware field access --------------------------

    /// Descriptor-aware get — dispatches to the underlying heap's
    /// [`get_field_as`](cratonvm_gc_core::collector::GarbageCollector::get_field_as),
    /// which normalizes the returned `Value` against the declared field
    /// type. See the trait-level documentation for the coercion rules.
    pub fn get_field_as(&self, obj: ObjectRef, index: usize, desc_byte: u8) -> Value {
        dispatch!(&self.inner, get_field_as(obj, index, desc_byte))
    }

    /// Volatile descriptor-aware get.
    pub fn get_field_volatile_as(&self, obj: ObjectRef, index: usize, desc_byte: u8) -> Value {
        dispatch!(&self.inner, get_field_volatile_as(obj, index, desc_byte))
    }

    /// Descriptor-aware set.
    pub fn set_field_as(&self, obj: ObjectRef, index: usize, value: Value, desc_byte: u8) {
        self.check_receiver(obj, "set_field_as", "set_field_as receiver");
        // Like `set_field`, descriptor-aware stores use the collector's
        // inherent barrier path. Close the debug SATB triad sentinel here;
        // otherwise an interpreter/JIT pre-barrier followed by putfield via
        // this typed entry point leaves it armed until the next store.
        #[cfg(debug_assertions)]
        clear_pending_pre_barrier();
        dispatch!(&self.inner, set_field_as(obj, index, value, desc_byte))
    }

    /// Read a small compact object's typed fields after one complete layout
    /// validation. None leaves out untouched for the scalar diagnostic path.
    /// Reference slots retain the collectors' shared wrapper-unboxing policy.
    pub fn try_get_compact_fields(
        &self,
        obj: ObjectRef,
        slots: &[(usize, u8)],
        out: &mut [Value],
    ) -> Option<ObjectRef> {
        const MAX_BATCH: usize = 16;
        let count = slots.len().min(out.len());
        if !(2..=MAX_BATCH).contains(&count) {
            return None;
        }
        let flags = crate::gc_flags();
        if flags.dbg_badref || flags.dbg_deadref_store || flags.dbg_stale_objref {
            return None;
        }
        #[cfg(feature = "zgc")]
        if let VmHeapInner::Zgc(heap) = &self.inner {
            // zt t18/zb1 (zr1br A2a): the batch reads reference slots raw;
            // armed, the per-slot path (`get_field`, which barriers) serves.
            if !heap.compact_batch_reads_enabled() || heap.load_barrier_armed() {
                return None;
            }
        }
        let (obj, valid) = self.load_and_forward_checked(obj);
        if !valid {
            return None;
        }
        let header = self.get_header(obj);
        if header.kind() != ObjectKind::Object || !cratonvm_types::is_compact_object(header) {
            return None;
        }
        let class_id = header.class_id;
        let mut plan = [(0usize, cratonvm_types::FieldStorageKind::Boolean); MAX_BATCH];
        // Compact objects are never allocated for mutable-layout classes
        // (`field_layout::is_compact_object`), so the current layout is the
        // exact layout this object was allocated against; consulting it
        // directly avoids `header.num_slots()`'s redundant layout lookup.
        let ready = cratonvm_types::with_current_class_layout(
            class_id.as_u32(),
            |layout| {
                // Small objects have contiguous backing on every collector,
                // including G1. Humongous/sparse layouts keep their scalar doors.
                if layout.total_size > 1024 {
                    return false;
                }
                for (i, (slot, _)) in slots[..count].iter().enumerate() {
                    let Some(off) = layout.field_disp(*slot) else {
                        return false;
                    };
                    let Some(storage) = layout.field_storage(*slot) else {
                        return false;
                    };
                    plan[i] = (off as usize, storage);                }
                true            },
        )
        .unwrap_or(false);
        if !ready {
            return None;
        }
        // Drop the layout borrow before unboxing can consult another layout.
        // Each slot is an independent relaxed atomic read, as on get_field_as;
        // a batch is not a multi-field snapshot or a volatile access.
        for (i, ((index, desc), dst)) in slots[..count].iter().zip(&mut out[..count]).enumerate() {
            let (off, storage) = plan[i];
            // SAFETY: the entire plan was validated against this live compact
            // receiver's layout. No safepoint or Java allocation intervenes.
            let raw = unsafe {
                cratonvm_types::read_compact_field(
                    obj.as_ptr().add(off),
                    storage,
                    std::sync::atomic::Ordering::Relaxed,
                )
            };
            let raw = if storage.is_reference() {
                cratonvm_gc_core::autobox::unbox_reference_field(
                    raw,
                    class_id,
                    *index,
                    |r| {
                        let r = self.is_object_address(r.as_ptr() as usize)?;
                        Some(self.class_id_of(r))
                    },
                    |r| self.get_field(r, 0),
                )
            } else {
                raw
            };
            *dst = cratonvm_gc_core::heap::coerce_field_value_for_slot(
                raw,
                *desc,
                cratonvm_gc_core::heap::FieldCoercionSite::read(Some(class_id), *index),
            );
        }
        Some(obj)
    }

    /// Try one layout resolution for a batch of compact primitive stores.
    /// None performs no writes, so the caller can retain the normal per-slot
    /// path for references, legacy layouts, invalid slots and diagnostics.
    pub fn try_set_compact_primitive_fields(
        &self,
        obj: ObjectRef,
        slots: &[(usize, u8)],
        values: &[Value],
    ) -> Option<ObjectRef> {
        const MAX_BATCH: usize = 16;
        let count = slots.len().min(values.len());
        if !(2..=MAX_BATCH).contains(&count)
            || slots[..count].iter().any(|(_, desc)| {
                !matches!(desc, b'Z' | b'B' | b'C' | b'S' | b'I' | b'J' | b'F' | b'D')
            })
            || values[..count]
                .iter()
                .any(|value| matches!(value, Value::Object(_)))
        {
            return None;
        }
        let flags = crate::gc_flags();
        if flags.dbg_badref || flags.dbg_deadref_store || flags.dbg_stale_objref {
            return None;
        }
        let (obj, valid) = self.load_and_forward_checked(obj);
        if !valid {
            return None;
        }
        let header = self.get_header(obj);
        if header.kind() != ObjectKind::Object || !cratonvm_types::is_compact_object(header) {
            return None;
        }
        let class_id = header.class_id;
        let mut plan = [(0usize, cratonvm_types::FieldStorageKind::Boolean); MAX_BATCH];
        let ready = cratonvm_types::with_current_class_layout(
            class_id.as_u32(),
            |layout| {
                // Keep G1's fragmented humongous backing on its scalar path.
                if layout.total_size > 1024 {
                    return false;
                }
                for (i, (slot, _)) in slots[..count].iter().enumerate() {
                    let Some(off) = layout.field_disp(*slot) else {
                        return false;
                    };
                    let Some(storage) = layout.field_storage(*slot) else {
                        return false;
                    };
                    // A primitive hint is not enough: the actual declared storage
                    // must be primitive too, or boxing and reference barriers apply.
                    if storage.is_reference() {
                        return false;
                    }
                    plan[i] = (off as usize, storage);
                }
                true
            },
        )
        .unwrap_or(false);
        if !ready {
            return None;
        }
        #[cfg(debug_assertions)]
        clear_pending_pre_barrier();
        // Release the layout-cache borrow before diagnostics/store helpers can
        // consult metadata themselves. No Java call or collection intervenes.
        for (i, ((index, desc), value)) in slots[..count].iter().zip(&values[..count]).enumerate() {
            let (off, storage) = plan[i];
            let value = cratonvm_gc_core::heap::coerce_field_value_for_slot(
                *value,
                *desc,
                cratonvm_gc_core::heap::FieldCoercionSite::store(Some(class_id), *index),
            );
            // SAFETY: the complete plan resolved each slot of this live compact
            // receiver, proved it primitive, and retained its natural alignment.
            // The coercion and stores cannot reach a safepoint or allocate Java.
            unsafe {
                dispatch!(
                    &self.inner,
                    set_resolved_compact_primitive_field(obj, *index, off, storage, value)
                );
            }
        }
        Some(obj)
    }

    /// The per-call precondition of [`Self::copy_compact_object_fields`]: no
    /// field-access diagnostic armed and, on ZGC, compact batch reads allowed
    /// (the primitive reads are raw; the punned-cell watch reads per slot).
    pub fn compact_object_copy_ready(&self) -> bool {
        let flags = crate::gc_flags();
        if flags.dbg_badref || flags.dbg_deadref_store || flags.dbg_stale_objref {
            return false;
        }
        #[cfg(feature = "zgc")]
        if let VmHeapInner::Zgc(heap) = &self.inner {
            return heap.compact_batch_reads_enabled();
        }
        true
    }

    /// The per-class precondition of [`Self::copy_compact_object_fields`]:
    /// this heap owns `class_id`'s compact layout (a class with no fields has
    /// none) and one plan covers it ([`COMPACT_COPY_MAX_SLOTS`] fields,
    /// [`COMPACT_COPY_MAX_BYTES`] bytes, every slot resolved). `false` means
    /// the copy refuses EVERY instance of the class, so a caller can decline
    /// before allocating the copy. Fixed for the life of the class.
    pub fn compact_object_copy_admits_class(&self, class_id: ClassId) -> bool {
        let mut plan: CompactCopyPlan =
            [(0usize, cratonvm_types::FieldStorageKind::Boolean); COMPACT_COPY_MAX_SLOTS];
        self.compact_copy_plan(class_id, &mut plan).is_some()
    }

    /// The copy plan of `class_id` into `plan`: `(field count, body bytes,
    /// every reference slot at the heap's width and alignment)`, or `None`
    /// when one plan cannot cover the class. The one decision both
    /// [`Self::copy_compact_object_fields`] and
    /// [`Self::compact_object_copy_admits_class`] read.
    pub(in crate::vm_heap) fn compact_copy_plan(
        &self,
        class_id: ClassId,
        plan: &mut CompactCopyPlan,
    ) -> Option<(usize, usize, bool)> {
        // The current-layout cache below is not domain-screened.
        if !self.owns_compact_layout(class_id) {
            return None;
        }
        let mut total_size = 0usize;
        // Every reference slot at the heap's reference width and alignment:
        // the precondition of the copy's barriered slot read.
        let mut words_readable = true;
        let width = cratonvm_types::narrow_oop::ref_field_size();
        // As in `try_get_compact_fields`: a compact object's class has a fixed
        // layout, so the current one is the one its objects were allocated by.
        let count = cratonvm_types::with_current_class_layout(class_id.as_u32(), |layout| {
            let count = layout.field_count();
            if layout.total_size as usize > COMPACT_COPY_MAX_BYTES || count > COMPACT_COPY_MAX_SLOTS
            {
                return None;
            }
            total_size = layout.total_size as usize;
            for (slot, step) in plan[..count].iter_mut().enumerate() {
                *step = (
                    layout.field_disp(slot)? as usize,
                    layout.field_storage(slot)?,
                );
                if step.1.is_reference()
                    && (step.1.size_runtime() as usize != width || step.0 % width != 0)
                {
                    words_readable = false;
                }
            }
            Some(count)
        })
        .flatten()?;
        Some((count, total_size, words_readable))
    }

    /// `Object.clone()`'s field copy: every instance field of `src` into
    /// `dst`, a freshly allocated object of the same class, verbatim.
    ///
    /// One validation of each object and one layout resolution serve the whole
    /// copy. Primitive slots are copied raw through the batch store door
    /// (`set_resolved_compact_primitive_field`, the one
    /// [`Self::try_set_compact_primitive_fields`] uses); every reference slot
    /// still goes through [`Self::get_field`] / [`Self::set_field`], so each
    /// backend's read and write barriers apply to it exactly as on the
    /// slot-by-slot copy. `dst` must be fresh: nothing it held before is
    /// published to a concurrent marker.
    ///
    /// The slot-by-slot copy validated and resolved the layout for every read
    /// and every write: a 14-field object cloned in ~4.5 us against HotSpot's
    /// ~60 ns, and `TimeZone.getDefault()` clones one per call
    /// (`docs/feature-designs/jit-ir-optimizations.md`).
    ///
    /// `None` writes nothing: either object is not a live compact instance of
    /// one class, the object is large (G1's humongous backing keeps its scalar
    /// door) or has more slots than one plan holds, or a field-access
    /// diagnostic is armed. The caller then copies slot by slot.
    ///
    /// j27 wave 4 (lane CLONE2): the two refusals that do not depend on the
    /// objects are asked by name, so a caller that must allocate `dst` first
    /// can ask them BEFORE it allocates: [`Self::compact_object_copy_ready`]
    /// (per call) and [`Self::compact_object_copy_admits_class`] (per class).
    pub fn copy_compact_object_fields(&self, src: ObjectRef, dst: ObjectRef) -> Option<()> {
        if !self.compact_object_copy_ready() {
            return None;
        }
        let (src, src_valid) = self.load_and_forward_checked(src);
        let (dst, dst_valid) = self.load_and_forward_checked(dst);
        if !src_valid || !dst_valid {
            return None;
        }
        let (src_header, dst_header) = (self.get_header(src), self.get_header(dst));
        if src_header.kind() != ObjectKind::Object
            || dst_header.kind() != ObjectKind::Object
            || !cratonvm_types::is_compact_object(src_header)
            || !cratonvm_types::is_compact_object(dst_header)
            || src_header.class_id != dst_header.class_id
        {
            return None;
        }
        let mut plan: CompactCopyPlan =
            [(0usize, cratonvm_types::FieldStorageKind::Boolean); COMPACT_COPY_MAX_SLOTS];
        let (count, total_size, words_readable) =
            self.compact_copy_plan(src_header.class_id, &mut plan)?;
        // A reference slot's word is copied as it stands (the barriered read of
        // the source slot) and stored through the collector's batch store when
        // it admits one: a verbatim copy, as `Object.clone` specifies, without
        // two validated field doors per slot. Otherwise (a SATB window open)
        // through `get_field` / `set_field`.
        let batch = words_readable && self.compact_reference_batch_admitted(total_size);
        let mut stored_reference = false;
        #[cfg(debug_assertions)]
        clear_pending_pre_barrier();
        // Drop the layout borrow before a reference door consults metadata.
        // No safepoint or Java allocation intervenes from here on.
        for (index, &(off, storage)) in plan[..count].iter().enumerate() {
            if storage.is_reference() && batch {
                // SAFETY: the plan resolved this reference slot of both live
                // compact objects, at the heap's width and alignment, and the
                // collector admitted the batch.
                unsafe {
                    let word = self.load_ref_slot_barriered(src.as_ptr().add(off)) as usize;
                    let value =
                        Value::Object((word != 0).then(|| ObjectRef::from_raw(word as *mut u8)));
                    self.set_resolved_compact_reference(dst, index, off, storage, value);
                }
                stored_reference = true;
                continue;
            }
            if storage.is_reference() {
                let value = self.get_field(src, index);
                self.set_field(dst, index, value);
                continue;
            }
            // SAFETY: the plan resolved this primitive slot against both live
            // compact objects' shared layout, at its natural alignment.
            unsafe {
                let value = cratonvm_types::read_compact_field(
                    src.as_ptr().add(off),
                    storage,
                    std::sync::atomic::Ordering::Relaxed,
                );
                dispatch!(
                    &self.inner,
                    set_resolved_compact_primitive_field(dst, index, off, storage, value)
                );
            }
        }
        self.note_compact_reference_batch(dst, stored_reference);
        Some(())
    }

    /// [`Self::try_set_compact_primitive_fields`] for a batch that also stores
    /// REFERENCES (the stack-walker residual-costs page, item 2): one
    /// forwarding read, one header read and one layout plan for the batch,
    /// then each slot's store as the collector's own accessor makes it in the
    /// state the collector admitted: ZGC while its compact reference store is
    /// a plain write (`ZgcRealHeap::compact_reference_batch_allowed`; one
    /// card for the batch), G1 while no marking window is open and the object
    /// cannot be humongous (the flat write and its remembered-set
    /// post-barrier), the generational heap while no SATB window is open (its
    /// resolved compact store, watches and card included). `None` performs no
    /// writes. Reference values must already be forwarded (the caller's
    /// boundary forward). A reference slot given a primitive (boxed by the
    /// accessors) or a primitive slot given an object declines.
    pub fn try_set_compact_fields(
        &self,
        obj: ObjectRef,
        slots: &[(usize, u8)],
        values: &[Value],
    ) -> Option<ObjectRef> {
        const MAX_BATCH: usize = 16;
        let count = slots.len().min(values.len());
        if !(2..=MAX_BATCH).contains(&count) {
            return None;
        }
        let flags = crate::gc_flags();
        if flags.dbg_badref || flags.dbg_deadref_store || flags.dbg_stale_objref {
            return None;
        }
        let (obj, valid) = self.load_and_forward_checked(obj);
        if !valid {
            return None;
        }
        let header = self.get_header(obj);
        if header.kind() != ObjectKind::Object || !cratonvm_types::is_compact_object(header) {
            return None;
        }
        let class_id = header.class_id;
        let mut plan = [(0usize, cratonvm_types::FieldStorageKind::Boolean); MAX_BATCH];
        let mut total_size = 0usize;
        let ready = cratonvm_types::with_current_class_layout(
            class_id.as_u32(),
            |layout| {
                if layout.total_size > 1024 {
                    return false;
                }
                total_size = layout.total_size as usize;
                for (i, ((slot, _), value)) in slots[..count].iter().zip(&values[..count]).enumerate()
                {
                    let Some(off) = layout.field_disp(*slot) else {
                        return false;
                    };
                    let Some(storage) = layout.field_storage(*slot) else {
                        return false;
                    };
                    if storage.is_reference() != matches!(value, Value::Object(_)) {
                        return false;
                    }
                    plan[i] = (off as usize, storage);
                }
                true
            },
        )
        .unwrap_or(false);
        if !ready {
            return None;
        }
        if !self.compact_reference_batch_admitted(total_size) {
            return None;
        }
        #[cfg(debug_assertions)]
        clear_pending_pre_barrier();
        let mut stored_reference = false;
        for (i, ((index, desc), value)) in slots[..count].iter().zip(&values[..count]).enumerate() {
            let (off, storage) = plan[i];
            let value = cratonvm_gc_core::heap::coerce_field_value_for_slot(
                *value,
                *desc,
                cratonvm_gc_core::heap::FieldCoercionSite::store(Some(class_id), *index),
            );
            if !(storage.is_reference() && matches!(value, Value::Object(_))) {
                // SAFETY: the complete plan resolved this primitive slot of the
                // live compact receiver. No safepoint or Java allocation.
                unsafe {
                    dispatch!(
                        &self.inner,
                        set_resolved_compact_primitive_field(obj, *index, off, storage, value)
                    );
                }
                continue;
            }
            stored_reference = true;
            // SAFETY: the plan resolved this reference slot of the live
            // compact receiver, and the collector admitted the batch.
            unsafe { self.set_resolved_compact_reference(obj, *index, off, storage, value) };
        }
        self.note_compact_reference_batch(obj, stored_reference);
        Some(obj)
    }

    /// May a batch store REFERENCES into a compact object of `total_size`
    /// bytes through [`Self::set_resolved_compact_reference`]? Each
    /// collector's own terms: no SATB window open (and, on G1, an object that
    /// cannot be humongous).
    pub(in crate::vm_heap) fn compact_reference_batch_admitted(&self, total_size: usize) -> bool {
        match &self.inner {
            VmHeapInner::Generational(h) => h.compact_reference_batch_allowed(),
            VmHeapInner::G1(h) => h.compact_reference_batch_allowed(total_size),
            #[cfg(feature = "zgc")]
            VmHeapInner::Zgc(h) => h.compact_reference_batch_allowed(),
        }
    }

    /// One reference store of an admitted batch
    /// ([`Self::compact_reference_batch_admitted`]): the flat write and the
    /// collector's per-store barrier (Generational's card, G1's remembered
    /// set). ZGC cards the object once after the batch
    /// ([`Self::note_compact_reference_batch`]).
    ///
    /// # Safety
    /// `off`/`storage` must describe `obj`'s in-bounds reference slot `index`,
    /// and the batch must be admitted.
    pub(in crate::vm_heap) unsafe fn set_resolved_compact_reference(
        &self,
        obj: ObjectRef,
        index: usize,
        off: usize,
        storage: cratonvm_types::FieldStorageKind,
        value: Value,
    ) {
        match &self.inner {
            VmHeapInner::Generational(h) => {
                h.set_resolved_compact_reference_field(obj, index, off, storage, value)
            }
            // SAFETY: the caller's contract.
            VmHeapInner::G1(h) => unsafe {
                h.set_resolved_compact_reference_field(obj, off, storage, value)
            },
            #[cfg(feature = "zgc")]
            VmHeapInner::Zgc(h) => unsafe {
                h.set_resolved_compact_reference_field_no_card(obj, off, storage, value)
            },
        }
    }

    /// ZGC's `set_field_no_satb` card, once for a batch that stored a
    /// reference into `obj`, after the stores (the other two collectors
    /// barrier per store).
    pub(in crate::vm_heap) fn note_compact_reference_batch(&self, obj: ObjectRef, stored_reference: bool) {
        #[cfg(feature = "zgc")]
        if let (VmHeapInner::Zgc(h), true) = (&self.inner, stored_reference) {
            h.note_ref_store(obj.as_ptr() as usize);
        }
        let _ = (obj, stored_reference);
    }

    /// Volatile descriptor-aware set.
    pub fn set_field_volatile_as(&self, obj: ObjectRef, index: usize, value: Value, desc_byte: u8) {
        self.check_receiver(
            obj,
            "set_field_volatile_as",
            "set_field_volatile_as receiver",
        );
        // Same inherent-barrier path and debug-triad closure as
        // `set_field_as` above.
        #[cfg(debug_assertions)]
        clear_pending_pre_barrier();
        dispatch!(&self.inner, set_field_volatile_as(obj, index, value, desc_byte))
    }

    // =====================================================================
    // GPU offload — safepoint coordination
    // =====================================================================

    /// Enter a GPU-critical section. Returns a `SafepointToken` whose
    /// lifetime brackets a no-GC window for the calling thread.
    ///
    /// The counter is process-wide (a module-level `AtomicU32`), so
    /// every `VmHeap` instance in the process shares the same gate.
    /// In practice every CratonVM process has exactly one `VmHeap`,
    /// so this is equivalent to per-heap.
    ///
    /// Every backend waits on [`wait_for_gpu_critical_drain`] before
    /// collecting — `GenerationalHeap` and `G1Collector` at their own entry,
    /// ZGC at this dispatcher (`zgc_recording_decision`, since 2026-09-23;
    /// before that the default collector did not wait at all) — so a kernel
    /// running under this token will not observe its inputs being moved.
    #[cfg(feature = "gpu-offload")]
    pub fn enter_gpu_critical(&self) -> cratonvm_gc_core::safepoint::SafepointToken<'static> {
        let token = cratonvm_gc_core::safepoint::SafepointToken::new(&GPU_CRITICAL_COUNT);
        #[cfg(feature = "zgc")]
        if let VmHeapInner::Zgc(h) = &self.inner {
            h.wait_for_concurrent_relocate_idle();
        }
        token
    }

    /// Current number of live `SafepointToken`s. Useful for tests.
    #[cfg(feature = "gpu-offload")]
    pub fn gpu_critical_count(&self) -> u32 {
        GPU_CRITICAL_COUNT.load(std::sync::atomic::Ordering::Acquire)
            & !cratonvm_gc_core::safepoint::GPU_COLLECTING
    }

    // =====================================================================
    // Array access
    // =====================================================================

    pub fn array_length(&self, obj: ObjectRef) -> usize {
        dispatch!(&self.inner, array_length(obj))
    }

    /// Returns the element type of an array object.
    /// For non-array objects the returned value is meaningless.
    pub fn array_element_type(&self, obj: ObjectRef) -> Option<ArrayElementType> {
        let header = self.get_header(obj);
        if header.kind() == ObjectKind::Array {
            Some(header.element_type())
        } else {
            None
        }
    }

    pub fn get_array_element(&self, obj: ObjectRef, index: usize) -> Result<Value, i32> {
        self.check_receiver(obj, "get_array_element", "get_array_element receiver");
        dispatch!(&self.inner, get_array_element(obj, index))
    }

    pub fn set_array_element(&self, obj: ObjectRef, index: usize, value: Value) -> Result<(), i32> {
        self.check_receiver(obj, "set_array_element", "set_array_element receiver");
        // Task #25: closes the triad sentinel (same rationale as
        // `set_field`); ref-array stores also route through the
        // inherent write_barrier.
        #[cfg(debug_assertions)]
        clear_pending_pre_barrier();
        dispatch!(&self.inner, set_array_element(obj, index, value))
    }

    // `pin_critical_region` / `unpin_critical_regions` -- the G1 region / ZGC
    // page pin `GetPrimitiveArrayCritical` used to take for its section -- were
    // deleted in gc-common w3-b (`handoff-w2c-retire-jni-pin-machinery.md`
    // item 3). Since w2-c the critical section finds its array again through a
    // remappable per-VM JNI global ref (`CriticalCopy::array_gref`), nothing
    // needs the array kept in place, and neither function had a caller. The
    // cuda-bridge critical path keeps its own pins (`cuda-bridge/src/critical.rs`),
    // and `ZgcRealHeap::pin_critical` / `G1Collector::pin_region_for_addr` stay
    // for their collectors' own callers and tests.

    /// Read an array element with auto-unboxing of wrapper types.
    ///
    /// Only the generational collector needs a separate entry point: G1 and ZGC
    /// un-box inside their own `get_array_element`, so routing them here would
    /// double-decode nothing and the plain accessor already satisfies this
    /// method's contract. Both arms below are therefore un-boxing reads, not
    /// fallbacks — ZGC's was a genuine fallback until 2026-08-09, which is what
    /// made `Stream.mapToLong(...).toArray()` return zeros under `-XX:+UseZGC`.
    pub fn get_array_element_unboxing(&self, obj: ObjectRef, index: usize) -> Result<Value, i32> {
        match &self.inner {
            VmHeapInner::Generational(h) => h.get_array_element_unboxing(obj, index),
            VmHeapInner::G1(h) => h.get_array_element(obj, index),
            #[cfg(feature = "zgc")]
            VmHeapInner::Zgc(h) => h.get_array_element(obj, index),
        }
    }

    /// Bulk-read a char[] array into a `Vec<u16>`.
    pub fn read_char_array_bulk(&self, obj: ObjectRef) -> Vec<u16> {
        match &self.inner {
            VmHeapInner::Generational(h) => h.read_char_array_bulk(obj),
            VmHeapInner::G1(h) => {
                // Residual humongous OOB fix: a G1 humongous char[] (any
                // char[] larger than ~region_size/2 — common for large
                // Strings) is laid out across SEVERAL NON-contiguous region
                // buffers, each with its own HEADER_SIZE prefix. The previous
                // code read the whole payload as one flat contiguous run from
                // `obj.as_ptr() + HEADER_SIZE`, which walks off the end of the
                // start region's buffer once the offset exceeds one region's
                // usable payload — a heap out-of-bounds read.
                //
                // vm_heap has no reachable G1 API to (a) detect that `obj` is a
                // HumongousStart or (b) translate a flat offset through the
                // region map (`humongous_span` / `humongous_copy` /
                // `lookup_region_for_addr` are all private to g1.rs). The
                // region-aware per-element accessor `G1Collector::get_array_element`
                // IS reachable, and it already routes humongous arrays through
                // `humongous_copy` (see g1.rs `get_array_element`), so every read
                // is confined to the object's own backing memory regardless of
                // how many regions it spans. Use it for the whole array: this is
                // correct for both ordinary and humongous arrays and can never
                // read out of bounds. (We deliberately do NOT keep the flat
                // fast-path for the non-humongous case, because vm_heap cannot
                // soundly tell the two apart without a new g1.rs API.)
                let len = h.array_length(obj);
                let mut out = vec![0u16; len];
                for (i, slot) in out.iter_mut().enumerate() {
                    // Char elements decode to `Value::Int(u16 as i32)`; mask
                    // back to the 16-bit code unit. `i < len` so the index is
                    // always in bounds and `get_array_element` returns `Ok`.
                    if let Ok(v) = h.get_array_element(obj, i) {
                        *slot = v.as_int().unwrap_or(0) as u16;
                    }
                }
                out
            }
            #[cfg(feature = "zgc")]
            VmHeapInner::Zgc(h) => {
                let len = h.array_length(obj);
                let mut out = vec![0u16; len];
                for (i, slot) in out.iter_mut().enumerate() {
                    if let Ok(v) = h.get_array_element(obj, i) {
                        *slot = v.as_int().unwrap_or(0) as u16;
                    }
                }
                out
            }
        }
    }

    /// Raw pointer to the array data region (just past the header).
    ///
    /// Every array now has a contiguous payload starting at
    /// `obj.as_ptr() + HEADER_SIZE`, so this returns `Some(ptr)` valid for the
    /// full `len * stride` span: generational and ordinary single-region G1
    /// arrays trivially, and G1 **humongous** arrays because all regions are
    /// adjacent slices of one backing arena, making a humongous span one
    /// physically-contiguous block (see `G1Collector`'s `arena` /
    /// `alloc_humongous_locked`). Before that arena change a humongous array
    /// was fragmented across non-contiguous region buffers and this returned
    /// `None` to force callers onto the region-aware per-element fallback; that
    /// fallback (`get_array_element` / `set_array_element` /
    /// `read_char_array_bulk`) is still correct but no longer required for
    /// humongous, and bulk consumers (arraycopy, GPU marshalling, Unsafe) now
    /// get the fast contiguous path. The `Option` is retained for API
    /// stability and so a future non-contiguous layout could opt back out.
    ///
    /// SAFETY (the `Some` case): `obj` is a live array `ObjectRef` in the heap
    /// arena and `HEADER_SIZE` is the layout-documented start of the payload.
    /// The pointer is valid for `len * stride` contiguous bytes for the
    /// lifetime of the object (which the caller must not let be collected or
    /// moved while holding the pointer).
    /// Fast-path reader for a compact LATIN1 `java.lang.String`'s backing
    /// `byte[]` slice (`(base_ptr, len)`). Returns `None` when the receiver is
    /// not a valid compact LATIN1 String (or when ZGC load-barrier marking is
    /// armed), leaving the caller to use the general path.
    pub fn try_read_compact_string_latin1_raw(
        &self,
        str_class_id: ClassId,
        obj: ObjectRef,
    ) -> Option<(*const u8, usize)> {
        if self.zgc_load_barrier_armed() {
            return None;
        }
        let (obj, valid) = self.load_and_forward_checked(obj);
        if !valid {
            return None;
        }
        let header = self.get_header(obj);
        if header.kind() != ObjectKind::Object
            || header.class_id != str_class_id
            || header.num_slots() < 2
        {
            return None;
        }
        let (val_off, val_stor) = cratonvm_types::compact_object_field_disp(&header, 0)?;
        let (cod_off, cod_stor) = cratonvm_types::compact_object_field_disp(&header, 1)?;
        if !val_stor.is_reference() || cod_stor.is_reference() {
            return None;
        }
        let coder = unsafe {
            cratonvm_types::read_compact_field(
                obj.as_ptr().add(cod_off),
                cod_stor,
                std::sync::atomic::Ordering::Relaxed,
            )
        };
        if !matches!(coder, Value::Int(0)) {
            return None;
        }
        let val = unsafe {
            cratonvm_types::read_compact_field(
                obj.as_ptr().add(val_off),
                val_stor,
                std::sync::atomic::Ordering::Relaxed,
            )
        };
        let Value::Object(Some(arr)) = val else {
            return None;
        };
        let (arr, arr_valid) = self.load_and_forward_checked(arr);
        if !arr_valid {
            return None;
        }
        let arr_header = self.get_header(arr);
        if arr_header.kind() != ObjectKind::Array
            || arr_header.element_type() != ArrayElementType::Byte
        {
            return None;
        }
        let len = arr_header.array_length() as usize;
        let base = if len == 0 {
            std::ptr::NonNull::<u8>::dangling().as_ptr() as *const u8
        } else {
            unsafe { arr.as_ptr().add(ARRAY_DATA_OFFSET) as *const u8 }
        };
        Some((base, len))
    }

    /// Fast-path initialization of a reference field on a newly-allocated
    /// compact TLAB object (`old == null`, ZGC only).
    pub fn try_init_compact_ref_field_no_barrier(
        &self,
        obj: ObjectRef,
        index: usize,
        target: ObjectRef,
    ) -> bool {
        #[cfg(feature = "zgc")]
        let is_zgc = matches!(&self.inner, VmHeapInner::Zgc(_));
        #[cfg(not(feature = "zgc"))]
        let is_zgc = false;
        if !is_zgc || self.zgc_load_barrier_armed() {
            return false;
        }
        let flags = crate::gc_flags();
        if flags.dbg_badref || flags.dbg_deadref_store || flags.dbg_stale_objref {
            return false;
        }
        let header = self.get_header(obj);
        if header.kind() != ObjectKind::Object {
            return false;
        }
        let Some((off, stor)) = cratonvm_types::compact_object_field_disp(&header, index) else {
            return false;
        };
        if !stor.is_reference() {
            return false;
        }
        unsafe {
            cratonvm_types::write_compact_field(
                obj.as_ptr().add(off),
                stor,
                Value::Object(Some(target)),
                std::sync::atomic::Ordering::Relaxed,
            );
        }
        true
    }

    pub fn array_data_ptr(&self, obj: ObjectRef) -> Option<*mut u8> {
        // Contiguous from `obj + HEADER_SIZE` for every array: generational and
        // single-region G1 trivially, and G1 humongous because all regions are
        // adjacent slices of one arena (so a humongous span is one block).
        Some(unsafe { obj.as_ptr().add(ARRAY_DATA_OFFSET) })
    }

    // =====================================================================
    // GC operations
    // =====================================================================

    pub fn needs_gc(&self) -> bool {
        match &self.inner {
            VmHeapInner::Generational(h) => h.needs_gc(),
            VmHeapInner::G1(h) => h.needs_gc(),
            #[cfg(feature = "zgc")]
            VmHeapInner::Zgc(h) => h.needs_gc(),
        }
    }

    /// GC trigger queried from a JIT allocation/refill helper while the
    /// compiled caller remains discoverable on the native stack.
    pub fn needs_gc_for_jit_allocation(&self) -> bool {
        match &self.inner {
            VmHeapInner::Generational(h) => h.needs_gc_for_jit_allocation(),
            VmHeapInner::G1(h) => h.needs_gc(),
            #[cfg(feature = "zgc")]
            VmHeapInner::Zgc(h) => h.needs_gc(),
        }
    }

    /// Native-wrapper allocation-pressure signal, consumed at the
    /// `safe_native_call` boundary to run the GC the wrappers themselves
    /// cannot.
    ///
    /// Each collector latches this from the point where it notices the
    /// mutator is outrunning it *inside* a native callback — where no
    /// collection can be initiated, because the callback's locals are not all
    /// rooted yet. Generational: a young→old spill
    /// (`GenHeap::young_spill_pressure`). G1: a new Eden region claimed with
    /// the Free pool already under the `needs_gc` threshold
    /// (`G1Collector::native_alloc_pressure`) — without it, a workload that
    /// allocates only from inside natives never reaches ANY safepoint and G1's
    /// infallible allocator aborts the process on a heap full of garbage (see
    /// `g1-native-alloc-no-safepoint-oom-FIXED.md`).
    ///
    /// ZGC: the same defect was live here, verbatim. `ZgcRealHeap` is an
    /// infallible allocator too — `alloc_object` and `alloc_array` end in
    /// `ZgcRealHeap::report_fatal_exhaustion` then `std::process::abort()`
    /// (`ZgcRealHeap`'s `GarbageCollector` impl) — and this arm answered a
    /// hardwired `false`, so the one hook that can run a collection on behalf of
    /// a native (`safe_native_call_impl`, `vm/src/vm/vm_exec.rs`) never fired on
    /// this backend. An
    /// allocate-only-from-natives workload therefore reached no safepoint at
    /// all and died by `abort()` on a heap full of garbage, with no Java-visible
    /// `OutOfMemoryError` ever thrown.
    ///
    /// 2026-08-07: this arm used to COMPUTE the signal from `h.needs_gc()`
    /// alone, carrying a `TODO(zgc)` that asserted `ZgcRealHeap` had no pressure
    /// field and that this file could not add one. That claim stopped being true
    /// the same day, and the stale comment was the only thing keeping the gap
    /// open: `zgc.rs` now owns a real `native_alloc_pressure: AtomicBool` on
    /// `ZgcRealHeap` — armed in `alloc_raw`, disarmed at the end of
    /// `collect_garbage` where `gc_rearm` is recomputed — behind the same
    /// `native_alloc_pressure()` / `clear_native_alloc_pressure()` /
    /// `note_native_alloc_pressure()` trio G1 exposes. The two halves of one fix
    /// were written from opposite ends and never met; these three arms are the
    /// join.
    ///
    /// The latch ADDS to the occupancy test rather than replacing it, which is
    /// where this deliberately differs from the G1 arm above (a bare latch
    /// read). G1 can afford that because `note_region_consumed_locked` re-arms
    /// on every region consumption below the threshold. Here, dropping
    /// `|| h.needs_gc()` would NARROW behaviour that is already load-bearing:
    /// the consumer (`vm/src/vm/vm_exec.rs`) clears unconditionally after
    /// acting — including when its own gates said no — so a just-cleared latch
    /// would answer `false` over a heap that is genuinely over its trigger. The
    /// disjunction keeps the `abort()` case covered by construction, and the
    /// latch adds the edge the occupancy test cannot see (a caller that noted
    /// pressure below the trigger).
    ///
    /// Neither term can recreate the `gc_rearm` GC storm. The latch is armed on
    /// `needs_gc`'s predicate VERBATIM — `allocated >= gc_threshold &&
    /// allocated >= gc_rearm`, inlined in `ZgcRealHeap::alloc_raw` — so it
    /// inherits the re-arm floor each collection raises to
    /// `live + max(headroom/4, 64 KiB)`, and can never ask for a collection
    /// `needs_gc` would refuse. The second term IS `needs_gc`, i.e. exactly what
    /// this arm answered before. And the consumer re-checks both
    /// `gc_overhead_limit_exceeded` and `needs_gc` before running anything, so
    /// even the externally-noted edge (which bypasses the heap's own predicate,
    /// by design) buys at most one gate evaluation per note.
    #[inline]
    pub fn young_spill_pressure(&self) -> bool {
        match &self.inner {
            VmHeapInner::Generational(h) => h.young_spill_pressure(),
            VmHeapInner::G1(h) => h.native_alloc_pressure(),
            #[cfg(feature = "zgc")]
            // SBR5 lane ncall: every native call asks this, and `needs_gc` is
            // ~13 ns of atomic loads (6% of a 220 ns crossing). The relief it
            // buys is not latency-critical -- a native allocation loop crosses
            // the threshold and collects a few native calls later -- so the
            // occupancy predicate is sampled on every eighth call of the
            // thread. The latch (`native_alloc_pressure`) stays exact.
            VmHeapInner::Zgc(h) => {
                if h.native_alloc_pressure() {
                    return true;
                }
                thread_local! {
                    static TICK: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
                }
                let tick = TICK.with(|c| {
                    let v = c.get().wrapping_add(1);
                    c.set(v);
                    v
                });
                tick & 7 == 0 && h.needs_gc()
            }
        }
    }

    /// Whether an allocation was **refused** since the last collection.
    ///
    /// The hard twin of [`Self::young_spill_pressure`], and the difference is
    /// the whole point: the boundary consumer re-checks `needs_gc()` before
    /// acting on the soft signal, which is right for an advisory note and
    /// wrong for a request that already failed. On a non-compacting heap those
    /// two states come apart completely — an arena can refuse a 2 MB array
    /// while `allocated` sits at 7% of capacity, because the bytes are there
    /// and no single hole is — and in that state `needs_gc()` answers no and
    /// discards the only signal that knew better.
    ///
    /// Non-ZGC backends answer `false`: Generational's own spill latch plus
    /// `old_gen_needs_gc()` already cover the same ground for it, and G1
    /// relocates, so "no hole this big" is not a durable state there.
    #[inline]
    pub fn hard_alloc_failure(&self) -> bool {
        match &self.inner {
            VmHeapInner::Generational(_) => false,
            VmHeapInner::G1(_) => false,
            #[cfg(feature = "zgc")]
            VmHeapInner::Zgc(h) => h.hard_alloc_failure(),
        }
    }

    /// Clear the hard-allocation-failure latch — see [`Self::hard_alloc_failure`].
    #[inline]
    pub fn clear_hard_alloc_failure(&self) {
        match &self.inner {
            VmHeapInner::Generational(_) => {}
            VmHeapInner::G1(_) => {}
            #[cfg(feature = "zgc")]
            VmHeapInner::Zgc(h) => h.clear_hard_alloc_failure(),
        }
    }

    /// Clear the native-wrapper allocation-pressure signal.
    #[inline]
    pub fn clear_young_spill_pressure(&self) {
        match &self.inner {
            VmHeapInner::Generational(h) => h.clear_young_spill_pressure(),
            VmHeapInner::G1(h) => h.clear_native_alloc_pressure(),
            // 2026-08-07: was a no-op, on the (by then false) grounds that ZGC's
            // signal was computed rather than latched and so had nothing to
            // clear. `ZgcRealHeap` owns the latch now, so this is G1's plain
            // delegation. Idempotent and cheap on purpose — the consumer clears
            // unconditionally after acting, including when its own gates said
            // no. Note this lowers only the LATCH; the `|| h.needs_gc()` half of
            // [`Self::young_spill_pressure`] is the heap's own occupancy and
            // clears itself when a collection actually runs.
            #[cfg(feature = "zgc")]
            VmHeapInner::Zgc(h) => h.clear_native_alloc_pressure(),
        }
    }

    /// Record a native-wrapper allocation-pressure event — see
    /// [`Self::young_spill_pressure`].
    #[inline]
    pub fn note_young_spill_pressure(&self) {
        match &self.inner {
            VmHeapInner::Generational(h) => h.note_young_spill_pressure(),
            VmHeapInner::G1(h) => h.note_native_alloc_pressure(),
            // 2026-08-07: was a no-op under a `TODO(zgc)` specifying the latch
            // `ZgcRealHeap` should grow. It grew it (field + arming edge in
            // `alloc_raw` + disarm in `collect_garbage` + the accessor trio), so
            // the TODO is discharged and this is G1's plain delegation.
            //
            // What the no-op cost: `Self::young_spill_pressure` read the heap's
            // own occupancy, which covers the case that actually aborts the
            // process (the heap really is over the trigger) but NOT a caller
            // that spilled BELOW the trigger and wants the next native boundary
            // to collect anyway. That was a gap in the mechanism rather than a
            // live defect — this method still has no call site outside `gc/` —
            // but a silently-dropped signal is a bad thing to leave armed for
            // the first caller that does appear.
            //
            // This edge deliberately bypasses the `gc_threshold` / `gc_rearm`
            // predicate the heap applies to itself: the caller is asserting
            // pressure the heap's counters cannot see. It cannot storm, because
            // the consumer re-checks `gc_overhead_limit_exceeded` and
            // `needs_gc` before collecting and clears the latch either way, so
            // one note buys one gate evaluation.
            #[cfg(feature = "zgc")]
            VmHeapInner::Zgc(h) => h.note_native_alloc_pressure(),
        }
    }

    /// DBG: young-arena state snapshot — see `GenHeap::young_arena_diag`.
    pub fn young_arena_diag(&self) -> (usize, usize, usize, usize) {
        match &self.inner {
            VmHeapInner::Generational(h) => h.young_arena_diag(),
            _ => (0, 0, 0, 0),
        }
    }

    /// Live-bytes estimate for GC-productivity accounting — see
    /// `GenHeap::live_bytes_estimate` (young free-list-aware; the raw bump
    /// cursor never retreats under the non-moving sweep).
    ///
    /// # Why the other two arms are spelled out
    ///
    /// They answer `allocated_bytes()`, which is the same answer the `_ =>`
    /// catch-all gave — but written as a catch-all it was indistinguishable
    /// from the `bytes_promoted_total` catch-all two functions down, which was
    /// answering a constant for a backend that had already grown the counter it
    /// denied
    /// (`docs/feature-designs/zgc-architecture.md`).
    /// Here the delegation is real on both arms and the reason differs per
    /// backend, so each says its own.
    pub fn live_bytes_estimate(&self) -> usize {
        match &self.inner {
            VmHeapInner::Generational(h) => h.live_bytes_estimate(),
            // G1 reclaims a whole region at a time and `allocated_bytes` is
            // summed over the live regions' occupancy, so it already retreats
            // when a collection frees anything — there is no non-moving sweep
            // leaving a bump cursor high, which is the effect the generational
            // arm's estimate exists to correct for.
            VmHeapInner::G1(_) => self.allocated_bytes(),
            // ZGC's sweep rebuilds the arena free list and retracts the bump
            // cursor into its free tail, so `allocated_bytes` falls with the
            // live set here too.
            #[cfg(feature = "zgc")]
            VmHeapInner::Zgc(_) => self.allocated_bytes(),
        }
    }

    /// Total bytes promoted young→old across all collections (selective
    /// promotion + the moving collector's tenuring). Used by the
    /// GC-overhead productivity metric: a promotion-only cycle conserves
    /// live bytes but did useful allocation-enabling work.
    ///
    /// # Why this is not a catch-all any more
    ///
    /// It was `_ => 0` until 2026-09-20, and that single arm was doing two
    /// completely different jobs: it swallowed a counter one backend already
    /// had, and it denied a counter another backend did not have yet. Both read
    /// as "this backend does not promote", which is false on all three.
    ///
    /// That is the hazard a catch-all carries in this dispatcher generally: it
    /// keeps answering a constant long after a backend grows the thing it
    /// denies, and nothing goes red. Spelling the arms out means the next
    /// backend that grows a promotion counter gets a non-exhaustive-match
    /// compile error here instead of a silent zero.
    ///
    /// # All three arms now delegate
    ///
    /// *2026-09-21.* G1 counts bytes copied into Old regions at the winning arm
    /// of both evacuators (`G1Collector::note_promoted_bytes`), and ZGC counts
    /// bytes at the sweep site that ages an object across the promotion
    /// threshold (`ZCounters::gen_promoted_bytes`). Neither is derived from an
    /// occupancy delta: on G1 an Old-region count moves for reasons that are not
    /// promotion (humongous allocation, cleanup reclaim, mixed-CSet evacuation
    /// of Old into Old), so a derived figure would be wrong in both directions
    /// and — unlike a hard zero — would be trusted.
    ///
    /// A backend that cannot promote still answers `0`, and now says so from
    /// its own counter: outside `CRATONVM_ZGC_GENERATIONAL` there is no old
    /// generation for ZGC to promote into, so its counter never moves and the
    /// zero is the truth rather than a missing consumer.
    pub fn bytes_promoted_total(&self) -> u64 {
        match &self.inner {
            VmHeapInner::Generational(h) => h.stats().snapshot().bytes_promoted,
            VmHeapInner::G1(h) => h.collector.bytes_promoted_total(),
            #[cfg(feature = "zgc")]
            VmHeapInner::Zgc(h) => h.bytes_promoted_total(),
        }
    }

    /// Bytes the TENURED space can still absorb — the "is the heap actually
    /// wedged?" half of the GC-overhead limit (see
    /// `interpreter::note_gc_productivity`).
    ///
    /// The freed-bytes threshold on its own cannot tell a genuine
    /// retained-allocation death spiral (survivors promoted into an old
    /// generation that is already full, so nothing drains) from a young
    /// generation that simply has nothing to promote while old gen sits nearly
    /// empty. The distinguishing fact is whether old gen can still absorb a
    /// young drain, which is a question about the OLD generation specifically,
    /// not about total fullness: in the spiral the young semi is emptied every
    /// cycle, so *total* fullness parks near young/total and never looks
    /// exhausted — which is exactly why a total-fullness gate was rejected.
    ///
    /// Non-generational backends have no separate tenured space; report their
    /// whole-heap headroom, which for a single-space collector is the same
    /// question. Spelled out per backend rather than left to a catch-all, for
    /// the reason [`Self::live_bytes_estimate`] states.
    pub fn old_gen_headroom(&self) -> usize {
        match &self.inner {
            VmHeapInner::Generational(h) => h.old_gen_capacity().saturating_sub(h.old_gen_used()),
            // G1's Old regions are drawn from the same free-region pool as
            // Eden and Survivor, so "what can old still absorb" is "what is
            // left of that pool" -- less the to-space reserve the mutator
            // never gets (gcd d10/o retirement: it was `capacity - allocated`,
            // which read 12 % at G1's `OutOfMemoryError`, so the overhead
            // limit's 2 % wedge test could never pass on G1). See
            // `G1Collector::old_absorbable_bytes`.
            VmHeapInner::G1(h) => h.old_absorbable_bytes(),
            // ZGC's old generation is a header label rather than a space (see
            // `zgc::generation`), so a promoted object occupies the same arena
            // it always did and whole-heap headroom is the whole question.
            #[cfg(feature = "zgc")]
            VmHeapInner::Zgc(_) => self.heap_capacity().saturating_sub(self.allocated_bytes()),
        }
    }

    /// The crawl backstop's old-space budget, including growth the
    /// Generational allocator can still take after a refused request. Other
    /// backends already draw their old allocations from the whole-heap pool.
    pub fn old_gen_headroom_with_growth(&self) -> usize {
        match &self.inner {
            VmHeapInner::Generational(h) => h.old_gen_headroom_with_growth(),
            VmHeapInner::G1(_) => self.old_gen_headroom(),
            #[cfg(feature = "zgc")]
            VmHeapInner::Zgc(_) => self.old_gen_headroom(),
        }
    }

    /// `(used, free-list bytes, largest free block, capacity)` for the young
    /// from-space, for diagnostics only.
    ///
    /// SB-LOADER-ZIPCONTENT (2026-08-04). `live_bytes_estimate` reports
    /// `young.used - young.free_list` summed with old, which cannot distinguish
    /// "young is genuinely full of live objects" from "young was bumped to the
    /// top once and is now a free list nobody can carve an 8 KB array out of".
    /// Those two want opposite fixes, and the second is what a run of
    /// non-moving young sweeps produces — so the number that tells them apart
    /// belongs next to the overhead-limit numbers that motivated the question.
    ///
    /// Non-generational backends have no young from-space; they report zeros.
    pub fn young_occupancy(&self) -> (usize, usize, usize, usize) {
        match &self.inner {
            VmHeapInner::Generational(h) => h.young_from_occupancy(),
            _ => (0, 0, 0, 0),
        }
    }

    /// Run a garbage collection cycle.
    ///
    /// The `stw` parameter is type-level proof that the caller is in a
    /// stop-the-world phase — see [`cratonvm_gc_core::collector::StopTheWorldToken`].
    pub fn collect_garbage(
        &self,
        stw: &cratonvm_gc_core::collector::StopTheWorldToken,
        roots: &mut [ObjectRef],
        monitors: &dyn MonitorCleanup,
    ) -> GcResult {
        // Retire every published FFM fast-path verdict — see the identical
        // call in [`Self::collect_garbage_with_finalizers`] for why a verdict
        // keyed by a carrier's ADDRESS cannot survive a collection.
        //
        // That comment claimed the bump sat "at the one dispatcher every
        // collector goes through", and it did not: THIS is a second public
        // door into the same three collectors, and it had no bump. Production
        // happens to reach the collectors only through the finalizer-aware
        // twin today (every in-tree caller of this entry point is a test or a
        // bench), so the gap is latent rather than live — which is exactly the
        // kind of gap the claim was written to prevent, and the reason to close
        // it here rather than to restate the claim. One relaxed increment per
        // cycle, never per access.
        cratonvm_types::ffm_epoch::bump_ffm_epoch();
        #[cfg(feature = "gpu-offload")]
        {
            let cycle = self.gpu_coord().before_collection();
            if cycle.extra_roots.is_empty() {
                let result = self.collect_garbage_dispatch(stw, roots, monitors);
                cycle.after_collection(&[]);
                return result;
            }
            // Splice the GPU keep-alive roots onto the caller's, run the
            // collector over both, then hand each half back to its owner
            // with the post-collection addresses.
            let n = roots.len();
            let mut all: Vec<ObjectRef> = Vec::with_capacity(n + cycle.extra_roots.len());
            all.extend_from_slice(roots);
            all.extend_from_slice(&cycle.extra_roots);
            let result = self.collect_garbage_dispatch(stw, &mut all, monitors);
            roots.copy_from_slice(&all[..n]);
            cycle.after_collection(&all[n..]);
            result
        }
        #[cfg(not(feature = "gpu-offload"))]
        {
            self.collect_garbage_dispatch(stw, roots, monitors)
        }
    }

    pub(in crate::vm_heap) fn collect_garbage_dispatch(
        &self,
        stw: &cratonvm_gc_core::collector::StopTheWorldToken,
        roots: &mut [ObjectRef],
        monitors: &dyn MonitorCleanup,
    ) -> GcResult {
        // JVMTI `GarbageCollectionStart` / `GarbageCollectionFinish` — see
        // [`Self::collect_with_finalizers_dispatch`] for why they are fired
        // HERE and not by the collectors (Generational excepted: it fires its
        // own, see `jvmti_gc_hooks_fired_here`).
        let fire = self.jvmti_gc_hooks_fired_here();
        if fire {
            cratonvm_gc_core::gc::fire_gc_start();
        }
        let result = match &self.inner {
            VmHeapInner::Generational(h) => h.collect_garbage(stw, roots, monitors),
            VmHeapInner::G1(h) => h.collect_garbage(stw, roots, monitors),
            #[cfg(feature = "zgc")]
            VmHeapInner::Zgc(h) => {
                self.zgc_recording_decision(h, || h.collect_garbage(stw, roots, monitors))
            }
        };
        if fire {
            cratonvm_gc_core::gc::fire_gc_finish();
        }
        result
    }

    /// Run one ZGC collection and record the collector decision it took —
    /// the ZGC arm of `gc_metrics::record_collector_decision`, which until
    /// 2026-09-23 had no ZGC caller at all, so on the DEFAULT backend
    /// `collector_decision_report()` had nothing to say and
    /// `decision_reason::NON_MOVING_BACKEND_HAS_NO_YOUNG_COPY` was defined,
    /// labelled, tested and never recorded
    /// (`docs/feature-designs/zgc-architecture.md`).
    ///
    /// # Why the dispatcher can answer this after all
    ///
    /// That page declined "option 2" (record from here) on the ground that the
    /// dispatch point "does not know whether the slide ran", so it could only
    /// say "a ZGC collection happened". It does know, from ZGC's own
    /// monotonic, per-heap engagement counters, read before and after the one
    /// call and compared: `compaction_cycles` moves iff this cycle's slide ran,
    /// and `relocation_coverage_reasons[i]` moves iff this cycle's refusal was
    /// the coverage proof failing on obligation `i`. Both are bumped inside the
    /// collection, the world is stopped, and only one collection runs at a
    /// time, so a delta is exactly this cycle's verdict — not a re-derivation
    /// of it.
    ///
    /// The codes, in precedence order:
    ///
    /// * the slide ran → [`cratonvm_gc_core::gc_metrics::decision_reason::MOVING_BACKEND_COMPACTED`];
    /// * a GPU device vetoed relocation this cycle →
    ///   `NON_MOVING_GPU_CRITICAL` (already documented as covering "the ZGC
    ///   slide");
    /// * the per-cycle coverage proof refused →
    ///   `NON_MOVING_COVERAGE_INCOMPLETE` with the obligation that failed;
    /// * anything else (the cost gate, `CRATONVM_ZGC_RELOCATE=0`, a
    ///   generational young cycle, nothing worth sliding) →
    ///   `NON_MOVING_BACKEND_HAS_NO_YOUNG_COPY`.
    ///
    /// Cost: two small counter snapshots and one relaxed histogram bump per
    /// COLLECTION, never per allocation.
    #[cfg(feature = "zgc")]
    pub(in crate::vm_heap) fn zgc_recording_decision<R>(&self, h: &ZgcRealHeap, collect: impl FnOnce() -> R) -> R {
        use cratonvm_gc_core::gc_metrics::decision_reason;
        use cratonvm_gc_core::gc_quiescence::incomplete_reason;
        let _gpu_drain = wait_for_gpu_critical_drain();
        let compactions_before = h.feature_engagement().1;
        let coverage_before = h.relocation_coverage_reason_counts();
        let result = collect();
        // Read BEFORE the caller's `CycleGuard::after_collection` clears it.
        #[cfg(feature = "gpu-offload")]
        let gpu_forbidden = self.gpu_coord().relocation_forbidden();
        #[cfg(not(feature = "gpu-offload"))]
        let gpu_forbidden = false;
        let compacted = h.feature_engagement().1 > compactions_before;
        let coverage_after = h.relocation_coverage_reason_counts();
        let coverage_refusal = coverage_after
            .iter()
            .zip(coverage_before.iter())
            .position(|(after, before)| after > before);
        let (reason, unproven) = if compacted {
            (
                decision_reason::MOVING_BACKEND_COMPACTED,
                incomplete_reason::NONE,
            )
        } else if gpu_forbidden {
            (
                decision_reason::NON_MOVING_GPU_CRITICAL,
                incomplete_reason::NONE,
            )
        } else if let Some(obligation) = coverage_refusal {
            (decision_reason::NON_MOVING_COVERAGE_INCOMPLETE, obligation)
        } else {
            (
                decision_reason::NON_MOVING_BACKEND_HAS_NO_YOUNG_COPY,
                incomplete_reason::NONE,
            )
        };
        cratonvm_gc_core::gc_metrics::record_collector_decision("zgc", reason, unproven);
        result
    }

    /// PRE-collection addresses of every object the last
    /// [`Self::collect_garbage_with_finalizers`] marked ONLY in its
    /// finalizer-resurrection drain (evacuated, forwarded, or set the mark bit
    /// of) and that its strong closure had NOT marked -- the dead finalizable
    /// objects themselves included. Drained: a second call answers empty.
    ///
    /// The VM notes this set with the reference processor before its round
    /// (`note_resurrected_finalizables_for_reference_processing` in
    /// `vm/src/runtime/interpreter/gc_and_alloc.rs`), so a weak or soft
    /// reference to an object reachable only THROUGH a finalizable one is
    /// cleared before `finalize()` runs, as HotSpot does
    /// (`docs/feature-designs/profile-guided-inlining.md`,
    /// depth 2).
    ///
    /// gc-common w18-a: the dispatcher half of
    /// `docs/feature-designs/generative-gc-architecture.md`.
    /// An EMPTY answer is exactly the depth-1 behaviour (the VM still derives
    /// the dead finalizables themselves from `dead_finalizers`). A collector
    /// owner fills in its arm with a `take_resurrection_closure` of its own,
    /// recorded where its drain marks, reset at the start of every collection,
    /// and holding only objects the strong closure had not marked -- that last
    /// condition is the soundness one: a strongly reachable object in this set
    /// would lose its weak references.
    ///
    /// Generational: filled in (gc-common w36-d,
    /// `GenerationalHeap::take_resurrection_closure`) by the non-moving young
    /// sweep's resurrection drain, the moving Phase 2.5b and the old
    /// generation's finalizer pass. G1: Phase 3.5's fresh evacuations
    /// (`G1Collector::take_resurrection_closure`). ZGC: every object the
    /// resurrection pass's `mark_claim` wins (`ZgcRealHeap::take_resurrection_closure`).
    pub fn take_resurrection_closure(&self) -> Vec<usize> {
        match &self.inner {
            VmHeapInner::Generational(h) => h.take_resurrection_closure(),
            VmHeapInner::G1(h) => h.take_resurrection_closure(),
            #[cfg(feature = "zgc")]
            VmHeapInner::Zgc(h) => h.take_resurrection_closure(),
        }
    }

    /// gci i17/fz (i16/fr's residual, `CRATONVM_STW_UNDISCOVERED_REFS_STRONG`):
    /// take extra resurrection candidates that are not finalizable (the VM's
    /// stop-the-world probe referents, POST-collection addresses) back out of
    /// what the collection just published to the concurrent remark
    /// (`GenerationalHeap::retract_concurrent_finalizer_candidates`). Called
    /// inside the same pause. G1 and ZGC publish no such list: no-op.
    pub fn retract_concurrent_finalizer_candidates(&self, post_addrs: &[usize]) {
        match &self.inner {
            VmHeapInner::Generational(h) => h.retract_concurrent_finalizer_candidates(post_addrs),
            VmHeapInner::G1(_) => {}
            #[cfg(feature = "zgc")]
            VmHeapInner::Zgc(_) => {}
        }
    }

    /// GC with finalizer-aware resurrection: dead-but-finalizable objects
    /// are kept alive by the collection (evacuated under G1, marked under
    /// ZGC, forwarded under the semispace young collector) and their
    /// POST-collection addresses returned so the caller can enqueue them
    /// for `finalize()` — and mark their `ReferenceProcessor` entries
    /// enqueued so each object is finalized at most once.
    ///
    /// The `stw` parameter is type-level proof that the caller is in a
    /// stop-the-world phase — see [`cratonvm_gc_core::collector::StopTheWorldToken`].
    pub fn collect_garbage_with_finalizers(
        &self,
        stw: &cratonvm_gc_core::collector::StopTheWorldToken,
        roots: &mut [ObjectRef],
        finalizer_addrs: &[usize],
        monitors: &dyn MonitorCleanup,
    ) -> (GcResult, Vec<usize>) {
        // Retire every published FFM fast-path verdict. Those verdicts are keyed
        // by the carrier's ADDRESS, and after a collection an address no longer
        // identifies the object it identified before: a reclaimed carrier's
        // address can be handed to a different object, and a stale verdict would
        // vouch for it. One relaxed increment per CYCLE, never per access — see
        // `cratonvm_types::ffm_epoch`. Bumped at the VmHeap entry point rather
        // than inside a collector, so a future collector cannot silently miss
        // it — on BOTH public entry points, which is a correction: this comment
        // said "the one dispatcher" while `collect_garbage` was a second one
        // with no bump at all.
        cratonvm_types::ffm_epoch::bump_ffm_epoch();
        #[cfg(feature = "gpu-offload")]
        {
            // Same splice as `collect_garbage`; see there.
            let cycle = self.gpu_coord().before_collection();
            if cycle.extra_roots.is_empty() {
                let result =
                    self.collect_with_finalizers_dispatch(stw, roots, finalizer_addrs, monitors);
                cycle.after_collection(&[]);
                return result;
            }
            let n = roots.len();
            let mut all: Vec<ObjectRef> = Vec::with_capacity(n + cycle.extra_roots.len());
            all.extend_from_slice(roots);
            all.extend_from_slice(&cycle.extra_roots);
            let result =
                self.collect_with_finalizers_dispatch(stw, &mut all, finalizer_addrs, monitors);
            roots.copy_from_slice(&all[..n]);
            cycle.after_collection(&all[n..]);
            result
        }
        #[cfg(not(feature = "gpu-offload"))]
        {
            self.collect_with_finalizers_dispatch(stw, roots, finalizer_addrs, monitors)
        }
    }

    pub(in crate::vm_heap) fn collect_with_finalizers_dispatch(
        &self,
        stw: &cratonvm_gc_core::collector::StopTheWorldToken,
        roots: &mut [ObjectRef],
        finalizer_addrs: &[usize],
        monitors: &dyn MonitorCleanup,
    ) -> (GcResult, Vec<usize>) {
        // JVMTI `GarbageCollectionStart` / `GarbageCollectionFinish`.
        //
        // The VM installs these two hooks at boot (`vm_init.rs`,
        // `install_gc_start_hook` / `install_gc_finish_hook`) on the stated
        // premise that "the GC driver (gc::collect / collect_with_finalizers)
        // calls these unconditionally at entry / exit". Until 2026-09-23 the
        // ONLY callers of `fire_gc_start` / `fire_gc_finish` were those two
        // functions in `gc.rs` — the Cheney collector of the vestigial
        // semi-space `Heap`, which no `GcBackend` selects. So on Generational,
        // G1 and ZGC alike a JVMTI agent that enabled either event received
        // NOTHING, for every collection of every run. Fired here, at the one
        // dispatcher both public entry points go through, so no backend can
        // opt out by omission. Zero-cost without an agent: one `Acquire` load
        // of `GC_HOOKS_ACTIVE` each (the VM installs the adapters on every run,
        // and the adapter itself returns at `is_event_enabled`).
        //
        // Pause-scoped, like HotSpot's: this runs on the collecting thread
        // with the world stopped, and a JVMTI callback for these two events is
        // restricted by the spec to raw-monitor operations for that reason.
        //
        // Generational fires its own pair inside
        // `GenerationalHeap::collect_garbage{,_with_finalizers}` (gen
        // r4/plumbing, landed on dev the same day), so it is skipped here —
        // firing at both levels delivered every event TWICE to an agent.
        let fire = self.jvmti_gc_hooks_fired_here();
        if fire {
            cratonvm_gc_core::gc::fire_gc_start();
        }
        let result = match &self.inner {
            VmHeapInner::Generational(h) => {
                h.collect_garbage_with_finalizers(stw, roots, finalizer_addrs, monitors)
            }
            VmHeapInner::G1(h) => {
                // zt pl4 item 3: the collecting thread's share of a G1 pause
                // is worker work (HotSpot's G1 runs it on its parallel GC
                // workers): `gc_parallel_workers`, not the VM thread.
                let cpu = cratonvm_gc_core::thread_cpu::CpuSpan::begin();
                let result =
                    h.collect_garbage_with_finalizers(stw, roots, finalizer_addrs, monitors);
                let ns = cpu.elapsed_ns();
                h.collector.note_driver_cpu_ns(ns);
                cratonvm_gc_core::thread_cpu::note_pause_work_cpu(ns);
                result
            }
            #[cfg(feature = "zgc")]
            VmHeapInner::Zgc(h) => {
                // zt pl4 item 3: HotSpot's ZGC collects on its own worker
                // threads (no published group): the collecting thread's
                // share is not the VM thread's.
                let cpu = cratonvm_gc_core::thread_cpu::CpuSpan::begin();
                let result = self.zgc_recording_decision(h, || {
                    h.collect_garbage_with_finalizers(stw, roots, finalizer_addrs, monitors)
                });
                cratonvm_gc_core::thread_cpu::note_pause_work_cpu(cpu.elapsed_ns());
                result
            }
        };
        if fire {
            cratonvm_gc_core::gc::fire_gc_finish();
        }
        result
    }

    /// Whether the dispatcher fires the JVMTI `GarbageCollectionStart` /
    /// `GarbageCollectionFinish` pair for this backend. False for
    /// Generational, whose collector fires its own at its two public entry
    /// points; true for G1 and ZGC, which fire none.
    #[inline]
    pub(in crate::vm_heap) fn jvmti_gc_hooks_fired_here(&self) -> bool {
        !matches!(&self.inner, VmHeapInner::Generational(_))
    }

    pub fn write_barrier(&self, obj: ObjectRef, stored_value: Value) {
        // Task #25 debug-triad assertion: if this thread recently fired
        // a `write_barrier_pre` (post-store sequence), the matching
        // `write_barrier` MUST follow within the same logical store —
        // the SATB / card-marking invariants depend on (pre, store,
        // post) being co-located. We clear the flag here so the next
        // pre-barrier starts a fresh triad. The flag is per-thread and
        // updated only under `debug_assertions`, so release builds pay
        // nothing.
        #[cfg(debug_assertions)]
        clear_pending_pre_barrier();
        match &self.inner {
            VmHeapInner::Generational(h) => h.write_barrier(obj, stored_value),
            VmHeapInner::G1(h) => h.write_barrier(obj, stored_value),
            #[cfg(feature = "zgc")]
            VmHeapInner::Zgc(h) => h.write_barrier(obj, stored_value),
        }
    }

    /// Task #25: SATB pre-store barrier dispatched through the
    /// `GarbageCollector::write_barrier_pre` trait method. Call BEFORE
    /// overwriting a heap-managed reference slot. `old` is the value
    /// that will be lost; concurrent marking treats it as a root for
    /// the remainder of the cycle.
    ///
    /// `slot` is reserved for future debug triad-assertions and may be
    /// `null_mut()` if the caller only has the value (e.g. SATB log
    /// drain rebroadcasts).
    #[inline]
    pub fn write_barrier_pre(&self, slot: *mut ObjectRef, old: ObjectRef) {
        // Task #25: arm the per-thread triad sentinel. Cleared on the
        // matching `write_barrier`. Debug-only — release builds elide.
        #[cfg(debug_assertions)]
        arm_pending_pre_barrier();
        match &self.inner {
            VmHeapInner::Generational(h) => {
                <GenerationalHeap as GarbageCollector>::write_barrier_pre(h, slot, old)
            }
            VmHeapInner::G1(h) => <G1Collector as GarbageCollector>::write_barrier_pre(h, slot, old),
            #[cfg(feature = "zgc")]
            VmHeapInner::Zgc(h) => <ZgcRealHeap as GarbageCollector>::write_barrier_pre(h, slot, old),
        }
    }

    /// Enqueue a reference as live for an active SATB mark cycle without
    /// performing a heap store. This is used by `Reference.get()` paths: the
    /// referent was read, not overwritten, so it must not participate in the
    /// debug `(pre, store, post)` triad tracked by [`Self::write_barrier_pre`].
    #[inline]
    pub fn write_barrier_keep_alive(&self, referent: ObjectRef) {
        match &self.inner {
            VmHeapInner::Generational(h) => <GenerationalHeap as GarbageCollector>::write_barrier_pre(
                h,
                std::ptr::null_mut(),
                referent,
            ),
            VmHeapInner::G1(h) => <G1Collector as GarbageCollector>::write_barrier_pre(
                h,
                std::ptr::null_mut(),
                referent,
            ),
            #[cfg(feature = "zgc")]
            VmHeapInner::Zgc(h) => <ZgcRealHeap as GarbageCollector>::write_barrier_pre(
                h,
                std::ptr::null_mut(),
                referent,
            ),
        }
    }

    pub fn allocated_bytes(&self) -> usize {
        match &self.inner {
            VmHeapInner::Generational(h) => h.allocated_bytes(),
            VmHeapInner::G1(h) => h.allocated_bytes(),
            #[cfg(feature = "zgc")]
            VmHeapInner::Zgc(h) => h.allocated_bytes(),
        }
    }

    /// The heap's current COMMITTED size — HotSpot's `CollectedHeap::capacity()`,
    /// i.e. `Runtime.totalMemory()`, the JMX heap `MemoryUsage.getCommitted()`,
    /// the `(NNNM)` of an `-Xlog:gc` line and a JFR `GCHeapSummary`'s
    /// `committed`. `freeMemory()` is this minus [`Self::allocated_bytes`].
    ///
    /// # What changed on 2026-09-23, and why
    ///
    /// Until then the Generational and ZGC arms returned their RESERVATION —
    /// both young semi-spaces plus the old generation at full capacity, and
    /// ZGC's whole arena envelope — so `totalMemory()` was `-Xmx` on the
    /// default collector from the first instruction:
    /// `XmsProbe.java -Xms64m -Xmx512m` printed `total=512m`
    /// where HotSpot prints `total=64m` (orchestrator baseline battery,
    /// `docs/feature-designs/generative-gc-architecture.md`).
    /// An application that sizes a cache from `totalMemory()`/`freeMemory()`
    /// saw a heap eight times larger than the one it had. Every arm is now what
    /// the heap has brought into use:
    ///
    /// * Generational — the granules ONE young semi-space has committed (the
    ///   larger of the two arenas' commits; `Arena::committed_bytes`, so
    ///   `-Xms` moves it), plus the old generation's committed granules
    ///   (`OldGen::committed_bytes`; it reserves and commits on demand since
    ///   gen r4w2/oldgen2). On the wholly committed fallback, where the old
    ///   generation is committed whole, its TOUCHED prefix
    ///   (`OldGen::high_water`) is counted instead — counting the whole store
    ///   there made the old generation alone report half of `-Xmx`.
    ///   [`Self::os_committed_bytes`] counts every arena's committed bytes as
    ///   they are: that is the OS-accounting question. gen r5w2/obs6: the
    ///   second semi-space (the copy reserve) is no longer counted — HotSpot
    ///   Serial's `capacity()` leaves its to-space out too — see
    ///   `GenerationalHeap::usable_committed_parts`.
    /// * G1 — the committed prefix of its reservation (`committed_len`),
    ///   unchanged. (Until 2026-09-30 this read `-Xmx` after the first
    ///   parallel young pause, truthfully: that pause committed the whole
    ///   reservation — `docs/feature-designs/concurrent-gc-maturation.md`.)
    /// * ZGC — the arena's committed granules
    ///   ([`ZgcRealHeap::os_committed_bytes`]). Uncommit respects the startup
    ///   committed-byte floor and the per-VM ZUncommit/ZUncommitDelay policy.
    ///   See `docs/feature-designs/zgc-architecture.md`.
    ///
    /// # It must not track OCCUPANCY
    ///
    /// HotSpot's `totalMemory()` moves when the heap grows or shrinks, and
    /// callers rely on that: H2's `Utils.collectGarbage()` is written as "gc
    /// until `totalMemory()` stops changing". Every arm is a count of committed
    /// granules or regions, which an allocation inside already-committed space
    /// does not move; a collection moves it only by giving memory back, and two
    /// back-to-back collections with nothing allocated in between give back
    /// the same memory, so that loop still converges (in one extra `gc()` on
    /// ZGC). `committed_bytes_does_not_track_occupancy` pins the first half.
    ///
    /// Takes the arena lock on ZGC and the young/old locks on Generational (the
    /// same locks [`Self::allocated_bytes`] already takes on Generational). A
    /// reporting call, never an allocation-path one; do not call it while
    /// holding a collector lock.
    pub fn committed_bytes(&self) -> usize {
        match &self.inner {
            // gen r5w2/obs6 (2026-09-26): HotSpot Serial's `capacity()` — ONE
            // semi-space's committed granules plus the old generation's
            // committed size (`GenerationalHeap::usable_committed_parts`, which
            // the three JMX pools sum to). Until then this arm counted BOTH
            // semi-spaces' commits, so the empty copy reserve — never
            // allocatable while mutators run — was reported as usable heap
            // (`gengc-r4w3-hunter-runtime-memory-counts-the-copy-reserve`).
            // The old-generation term is unchanged (its derivation moved to
            // `gen_heap::reported_old_committed`).
            VmHeapInner::Generational(h) => h.usable_committed_bytes(),
            VmHeapInner::G1(h) => h.committed_bytes(),
            #[cfg(feature = "zgc")]
            VmHeapInner::Zgc(h) => h.os_committed_bytes(),
        }
    }

    /// The heap's MAXIMUM usable size — HotSpot's `CollectedHeap::max_capacity()`,
    /// i.e. `Runtime.maxMemory()` and the heap `MemoryUsage.getMax()` — where
    /// the backend knows it to differ from `-Xmx`; `None` means "report the
    /// configured `-Xmx`" (the VM's `max_heap_bytes`).
    ///
    /// gen r5w2/obs6 (2026-09-26): Generational answers
    /// `GenerationalHeap::max_usable_heap_bytes` — `-Xmx` less the copy
    /// reserve (one semi-space at its ceiling), `3/4 · -Xmx` at the default
    /// split, as HotSpot Serial reports `-Xmx` less one survivor space. A heap
    /// built without an `-Xmx` budget answers `None`. G1 and ZGC answer
    /// `None` (unchanged: their whole reservation is allocatable, and HotSpot
    /// reports `-Xmx` for both).
    pub fn max_usable_bytes(&self) -> Option<usize> {
        match &self.inner {
            VmHeapInner::Generational(h) => h.max_usable_heap_bytes(),
            VmHeapInner::G1(_) => None,
            #[cfg(feature = "zgc")]
            VmHeapInner::Zgc(_) => None,
        }
    }

    /// Bytes this heap's stores have actually asked the OS to BACK.
    ///
    /// [`Self::committed_bytes`] answers `Runtime.totalMemory()`. Since
    /// 2026-09-23 the two agree on G1 and ZGC; on Generational this one counts
    /// the old generation's whole `Vec` (reserved and, on Windows, charged at
    /// construction) where `committed_bytes` counts only its touched prefix,
    /// and (gen r5w2/obs6) both semi-spaces' commits where `committed_bytes`
    /// counts one.
    /// (Before that date `committed_bytes` was a sum of capacities on two
    /// backends and this was the only committed measurement.)
    ///
    /// # Why it exists
    ///
    /// `docs/feature-designs/generative-gc-architecture.md`
    /// recorded that the three backends' documented `-Xmx`/`-Xms` semantics and
    /// their actual resident behaviour had drifted apart, and that **nothing in
    /// the tree measured which of the two a running process matched**. A flag
    /// whose only effect is on committed bytes cannot be tested — or trusted —
    /// without a number that moves when it works, and `committed_bytes` was not
    /// that number on two of three backends until 2026-09-23.
    ///
    /// Takes a lock on the Generational and ZGC arms. A diagnostic and a test
    /// instrument, never an allocation-path call.
    pub fn os_committed_bytes(&self) -> usize {
        match &self.inner {
            VmHeapInner::Generational(h) => h.os_committed_bytes(),
            // Already the committed prefix rather than the reservation — G1 is
            // the backend whose `committed_bytes` never had the gap.
            VmHeapInner::G1(h) => h.committed_bytes(),
            #[cfg(feature = "zgc")]
            VmHeapInner::Zgc(h) => h.os_committed_bytes(),
        }
    }

    /// Return stable generational card-table metadata for JIT inline barriers.
    ///
    /// G1 and ZGC require collector-specific remembered-set/barrier protocols,
    /// so they return `None` and generated code retains the helper call.
    pub fn jit_card_table_info(&self) -> Option<(usize, usize, usize)> {
        match &self.inner {
            VmHeapInner::Generational(heap) => Some(heap.jit_card_table_info()),
            VmHeapInner::G1(_) => None,
            #[cfg(feature = "zgc")]
            VmHeapInner::Zgc(_) => None,
        }
    }

    /// gcj j2/jt2 (`CRATONVM_JIT_POST_ONLY_CARD_MARK`): compiled
    /// reference-store sites this heap's JIT emitted with no pre-store card
    /// check. Always `0` on G1 and ZGC, which publish no generational card
    /// view (so no such site exists for them).
    pub fn jit_post_only_card_sites(&self) -> u64 {
        match &self.inner {
            VmHeapInner::Generational(heap) => heap.jit_post_only_card_sites(),
            VmHeapInner::G1(_) => 0,
            #[cfg(feature = "zgc")]
            VmHeapInner::Zgc(_) => 0,
        }
    }

    /// gcj j2/jt2: the BUG-03 take-over's completion of the post-only card
    /// mark for one frozen peer's register words
    /// (`GenerationalHeap::takeover_complete_post_only_card_marks`). A no-op
    /// returning `0` on G1 and ZGC, and on Generational while no post-only
    /// site was compiled.
    pub fn takeover_complete_post_only_card_marks(&self, words: &[usize]) -> usize {
        match &self.inner {
            VmHeapInner::Generational(heap) => heap.takeover_complete_post_only_card_marks(words),
            VmHeapInner::G1(_) => 0,
            #[cfg(feature = "zgc")]
            VmHeapInner::Zgc(_) => 0,
        }
    }

    /// Return the total number of GC collections performed so far.
    ///
    /// For the generational heap, this is the sum of minor + major cycle
    /// counts (sampled with `Relaxed` ordering — see `HeapStats::snapshot`).
    /// Used by the JMX `GarbageCollectorMXBean.getCollectionCount/Time`
    /// natives; H2's `Utils.collectGarbage()` polls this in a
    /// `while(prev == cur) { System.gc(); }` loop, so a constant `0` here
    /// hangs the H2 test runner indefinitely.
    pub fn collection_count(&self) -> u64 {
        match &self.inner {
            // gci i2/wr: two relaxed loads, not the 19-load snapshot (every
            // address-memo lookup on Generational lands here).
            VmHeapInner::Generational(h) => h.stats().collection_count(),
            VmHeapInner::G1(h) => h.collection_count(),
            #[cfg(feature = "zgc")]
            VmHeapInner::Zgc(h) => h.gc_count() as u64,
        }
    }

    /// `true` while ZGC has concurrent relocation or forwarding state live.
    pub fn concurrent_relocation_active(&self) -> bool {
        match &self.inner {
            #[cfg(feature = "zgc")]
            VmHeapInner::Zgc(h) => crate::zgc::barrier::ZBarrierContext::is_relocating(h.as_ref()),
            _ => false,
        }
    }

    /// The epoch for ADDRESS-keyed memos: it moves whenever an address may come
    /// to name a different object. Not a statistic: only equality across two
    /// reads means anything (MXBeans keep [`Self::collection_count`]).
    ///
    /// gci i1/wr and gch h4/g1 X4, lane gcI wave 9 (the same fix of
    /// `r15w4-mh2-address-memo-epoch-misses-g1-cleanup-patch`, step 2). G1
    /// answers [`G1Collector::address_epoch`], which also moves at `cleanup`'s
    /// in-place reclaim of whole Old regions and dead humongous spans (it books
    /// no collection); every other backend answers [`Self::collection_count`],
    /// as the memos read before. The Generational concurrent sweep, which frees
    /// old blocks between pauses without booking a collection, is not covered
    /// here (the heap does not own the cycle state): the VM's
    /// `HeapRealm::address_epoch` adds `ConcurrentGcState::sweep_slices_begun`
    /// (gci i2/wr, page
    /// `gci-i1wr-generational-concurrent-sweep-does-not-move-the-address-memo-epoch-FIXED-20261012`).
    /// Address-keyed memos read that, not this.
    pub fn address_epoch(&self) -> u64 {
        match &self.inner {
            VmHeapInner::G1(h) => h.address_epoch(),
            _ => self.collection_count(),
        }
    }

    /// Accumulated collection pause time in milliseconds, for
    /// `GarbageCollectorMXBean.getCollectionTime()`, where the backend keeps
    /// one; `None` where it does not (ZGC). gc-common w5-f,
    /// `docs/feature-designs/zgc-architecture.md`.
    ///
    /// Every `Some` figure is the whole milliseconds of the backend's
    /// MICROSECOND pause sum, converted once, as HotSpot's
    /// `GCMemoryManager::_accumulated_timer` is ([`pause_sum_as_collection_time_ms`]).
    /// It does NOT move on every sub-millisecond collection; H2's
    /// `Utils.collectGarbage()` needs it to move only eventually (see
    /// `gc_metrics::serial_collectors`). gce e1/o,
    /// `docs/feature-designs/zgc-architecture.md`:
    /// G1 and this fallback added one millisecond per collection until then.
    ///
    /// * Generational — `GenerationalHeap::gc_pause_totals().1`. Only the
    ///   single-bean fallback reads it; the Serial beans this backend always
    ///   describes floor their own sums (`gc_metrics::serial_collectors`);
    /// * G1 — the collector's microsecond accumulator (`total_pause_us`, bumped
    ///   together with `collection_count` for every recorded pause).
    pub fn collection_time_ms(&self) -> Option<u64> {
        match &self.inner {
            VmHeapInner::Generational(h) => Some(pause_sum_as_collection_time_ms(h.gc_pause_totals().1)),
            VmHeapInner::G1(h) => Some(pause_sum_as_collection_time_ms(h.total_pause_us())),
            #[cfg(feature = "zgc")]
            VmHeapInner::Zgc(_) => None,
        }
    }

    /// The JMX bean state of a backend whose beans the common GC event
    /// plumbing books (gc-common w7-f): G1's, held by [`G1State`]. `None` on
    /// Generational (its beans are the heap's own: `jmx_collectors`,
    /// `gc_notifications`) and on ZGC, whose state lives beside the heap in the
    /// VM (`HeapRealm::backend_gc_beans` in `vm`): a ZGC heap is an
    /// `Arc<ZgcRealHeap>`, which this dispatcher cannot add a field to.
    pub fn backend_gc_beans(&self) -> Option<&Arc<cratonvm_gc_core::gc_metrics::BackendGcBeans>> {
        match &self.inner {
            VmHeapInner::G1(h) => Some(h.gc_beans()),
            _ => None,
        }
    }

    /// A handle that samples this heap's JMX pools without borrowing the
    /// dispatcher (it holds the collector's `Arc`), for a pause timer that
    /// outlives the borrow it was created under. `None` on Generational, which
    /// describes its pools itself (`GenerationalHeap::jmx_memory_pools`).
    /// gc-common w7-f.
    pub fn jmx_pool_sampler(&self) -> Option<JmxPoolSampler> {
        match &self.inner {
            VmHeapInner::Generational(_) => None,
            VmHeapInner::G1(h) => Some(JmxPoolSampler::G1(Arc::clone(&h.collector))),
            #[cfg(feature = "zgc")]
            VmHeapInner::Zgc(h) => Some(JmxPoolSampler::Zgc(Arc::clone(h))),
        }
    }

    /// The pool samples [`cratonvm_gc_core::gc_metrics::BackendGcBeans::pools`] names, in
    /// [`cratonvm_gc_core::gc_metrics::BackendBeanShape::pools`] order; empty on
    /// Generational. See [`JmxPoolSampler::sample`].
    pub fn backend_pool_samples(&self) -> Vec<cratonvm_gc_core::gc_metrics::JmxPoolSample> {
        self.jmx_pool_sampler()
            .map_or_else(Vec::new, |s| s.sample())
    }

    /// Young-only cycles completed so far: ZGC's opt-in generational mode's
    /// minor cycles (`ZgcRealHeap::generational_stats().0`), `0` on every
    /// other backend and in ZGC's default mode. One relaxed load. The GC event
    /// plumbing samples it on both edges of a collection to tell a `ZGC Minor`
    /// cycle from a `ZGC Major` one (gcd d10/o retirement).
    pub fn young_only_cycles(&self) -> usize {
        match &self.inner {
            VmHeapInner::Generational(_) | VmHeapInner::G1(_) => 0,
            #[cfg(feature = "zgc")]
            VmHeapInner::Zgc(h) => h.generational_stats().0,
        }
    }

    /// Wave 9 lane G: G1's old-collecting pauses so far
    /// ([`cratonvm_gc_g1::g1::G1Collector::old_collecting_pause_count`]); `None` on
    /// every other backend. Sampled on both edges of a collection to tell a
    /// young-only G1 pause from a mixed one (HotSpot's
    /// `all_memory_pools_affected`).
    pub fn g1_old_collecting_pauses(&self) -> Option<u64> {
        match &self.inner {
            VmHeapInner::G1(h) => Some(h.collector.old_collecting_pause_count()),
            #[allow(unreachable_patterns)]
            _ => None,
        }
    }

    /// zt pl4 item 3: on G1, `(parallel workers, concurrent mark)` thread CPU
    /// in nanoseconds (`G1Collector::gc_thread_cpu_ns`), the figures behind
    /// HotSpot G1's `sun.threads.cpu_time.gc_parallel_workers` /
    /// `gc_conc_mark` and `total_gc_cpu_time`; `None` on the other
    /// collectors (HotSpot's ZGC and Serial publish neither, and read 0).
    pub fn g1_gc_thread_cpu_ns(&self) -> Option<(u64, u64)> {
        match &self.inner {
            VmHeapInner::G1(state) => Some(state.collector.gc_thread_cpu_ns()),
            _ => None,
        }
    }

    /// zr z1/tr: HotSpot 25 ZGC's `GCCause` for an allocation-driven
    /// collection about to run on this heap -- `Allocation Stall` when the
    /// allocation already failed (`allocation_failed`), else the rule
    /// `needs_gc` charged (`Warmup`, `Allocation Rate`, `Proactive`,
    /// `High Usage`; `Allocation Rate` when none was charged). zr z2/t2: the
    /// failure door also names a pending rule (the compiled refill trigger
    /// takes that door on an occupancy verdict), and says `Allocation Stall`
    /// only when no rule was charged. Call it BEFORE
    /// the collection: it reads the live trigger latch, which the collection
    /// clears. `None` on Generational and G1 (their door's `Allocation
    /// Failure` is HotSpot's own spelling there) and under
    /// `CRATONVM_ZGC_HOTSPOT_GC_CAUSE=0`. HotSpot's ZGC never says
    /// `Allocation Failure`; this VM's ZGC said it on every allocation-driven
    /// collection until 2026-10-12.
    pub fn zgc_hotspot_gc_cause(&self, allocation_failed: bool) -> Option<&'static str> {
        match &self.inner {
            VmHeapInner::Generational(_) | VmHeapInner::G1(_) => None,
            #[cfg(feature = "zgc")]
            VmHeapInner::Zgc(h) => {
                if !h.hotspot_gc_causes_enabled() {
                    None
                } else if allocation_failed {
                    // zr z3/t2: a failure door with no occupancy
                    // classification: `Allocation Stall` (see
                    // `zgc_forced_door_gc_cause` for the classified form).
                    h.allocation_failure_gc_cause(false)
                } else {
                    Some(h.hotspot_gc_cause(true).unwrap_or("Allocation Rate"))
                }
            }
        }
    }

    /// zr z3/t2: HotSpot 25 ZGC's cause for a collection taken through the
    /// allocation-failure door whose site the VM classified: `occupancy`
    /// (an occupancy-verdict door) carries the pending rule, any other door is
    /// `Allocation Stall` (`ZgcRealHeap::allocation_failure_gc_cause`). Call
    /// it BEFORE the collection. `None` off ZGC and under
    /// `CRATONVM_ZGC_HOTSPOT_GC_CAUSE=0`.
    pub fn zgc_forced_door_gc_cause(&self, occupancy: bool) -> Option<&'static str> {
        match &self.inner {
            VmHeapInner::Generational(_) | VmHeapInner::G1(_) => {
                let _ = occupancy;
                None
            }
            #[cfg(feature = "zgc")]
            VmHeapInner::Zgc(h) => h.allocation_failure_gc_cause(occupancy),
        }
    }

    /// zt t31/x10 (item 1): the last ZGC collection's Mark Start occupancy,
    /// when it closed a concurrent cycle (`ZgcRealHeap::last_mark_start_used`);
    /// `None` off ZGC and after a stop-the-world mark.
    pub fn zgc_last_mark_start_used(&self) -> Option<usize> {
        match &self.inner {
            #[cfg(feature = "zgc")]
            VmHeapInner::Zgc(h) => h.last_mark_start_used(),
            _ => None,
        }
    }

    /// zt t31/x10 (item 1): the bytes the ZGC slides have copied so far
    /// (`ZgcRealHeap::slide_moved_bytes_total`); 0 off ZGC.
    pub fn zgc_slide_moved_bytes_total(&self) -> usize {
        match &self.inner {
            #[cfg(feature = "zgc")]
            VmHeapInner::Zgc(h) => h.slide_moved_bytes_total(),
            _ => 0,
        }
    }

    /// zt t31/x10 (item 5): an allocation stall is about to ask for its
    /// collection; the cycle that serves it, whoever runs it, is a stall's
    /// (`ZgcRealHeap::note_stall_cycle_pending`). No-op off ZGC.
    pub fn zgc_note_stall_cycle_pending(&self) {
        #[cfg(feature = "zgc")]
        if let VmHeapInner::Zgc(h) = &self.inner {
            h.note_stall_cycle_pending();
        }
    }

    /// zr z3/t2: whether a forced collection at a door the VM classified is an
    /// ALLOCATION STALL on ZGC. With `CRATONVM_ZGC_STALL_CAUSE_FROM_TRIGGER`
    /// on (default): every non-occupancy door. At `=0`: every door but the
    /// compiled refill trigger's (`refill_trigger`), the z2 reading. `false`
    /// off ZGC.
    pub fn zgc_forced_collection_is_stall(&self, occupancy: bool, refill_trigger: bool) -> bool {
        match &self.inner {
            VmHeapInner::Generational(_) | VmHeapInner::G1(_) => {
                let _ = (occupancy, refill_trigger);
                false
            }
            #[cfg(feature = "zgc")]
            VmHeapInner::Zgc(h) => {
                if h.stall_classification_by_door() {
                    // zt t20/tg9: under `-XX:+ZCollectionIntervalOnly` an
                    // occupancy door's collection is a stall too
                    // (`ZgcRealHeap::occupancy_door_is_not_a_stall`).
                    !occupancy || !h.occupancy_door_is_not_a_stall()
                } else {
                    !refill_trigger
                }
            }
        }
    }

    /// zr z2/t2: record one mutator allocation stall of `nanos` on ZGC
    /// (`ZgcRealHeap::note_allocation_stall`); a no-op elsewhere.
    pub fn zgc_note_allocation_stall(&self, nanos: u64) {
        match &self.inner {
            VmHeapInner::Generational(_) | VmHeapInner::G1(_) => {
                let _ = nanos;
            }
            #[cfg(feature = "zgc")]
            VmHeapInner::Zgc(h) => h.note_allocation_stall(nanos),
        }
    }

    /// zr z5/rv5: arm the next ZGC collection's soft-precise probe
    /// (`ZgcRealHeap::arm_soft_precise_probe`); a no-op on every other
    /// backend.
    pub fn zgc_arm_soft_precise_probe(&self, candidates: Vec<usize>) {
        match &self.inner {
            VmHeapInner::Generational(_) | VmHeapInner::G1(_) => {
                let _ = candidates;
            }
            #[cfg(feature = "zgc")]
            VmHeapInner::Zgc(h) => h.arm_soft_precise_probe(candidates),
        }
    }

    /// zr z5/rv5: the last ZGC collection's soft-precise answer
    /// (`ZgcRealHeap::take_soft_precise_kept`); `None` elsewhere.
    pub fn zgc_take_soft_precise_kept(&self) -> Option<Vec<usize>> {
        match &self.inner {
            VmHeapInner::Generational(_) | VmHeapInner::G1(_) => None,
            #[cfg(feature = "zgc")]
            VmHeapInner::Zgc(h) => h.take_soft_precise_kept(),
        }
    }

    /// zr z4/jf: the last ZGC collection's JFR facts
    /// (`ZgcRealHeap::take_jfr_cycle`), taken once; `None` elsewhere.
    #[cfg(feature = "zgc")]
    pub fn zgc_take_jfr_cycle(&self) -> Option<cratonvm_gc_zgc::zgc::ZJfrCycle> {
        match &self.inner {
            VmHeapInner::Generational(_) | VmHeapInner::G1(_) => None,
            VmHeapInner::Zgc(h) => h.take_jfr_cycle(),
        }
    }

    /// zs s7/jf: the reference pass's per-type counts (soft, weak, final,
    /// phantom) for the current ZGC collection's `jdk.GCReferenceStatistics`;
    /// a no-op elsewhere (Generational and G1 write no such event).
    pub fn zgc_note_jfr_reference_counts(&self, counts: [u64; 4]) {
        match &self.inner {
            VmHeapInner::Generational(_) | VmHeapInner::G1(_) => {
                let _ = counts;
            }
            #[cfg(feature = "zgc")]
            VmHeapInner::Zgc(h) => h.note_jfr_reference_counts(counts),
        }
    }

    /// zs s7/jf: the tenuring threshold a ZGC young (`ZGC Minor`) collection
    /// reports on `jdk.ZYoungGarbageCollection`
    /// (`ZgcRealHeap::jfr_tenuring_threshold`): `Some` only on ZGC in its
    /// generational mode. Kept apart from [`Self::tenuring_threshold`], whose
    /// `None` on ZGC keeps the Serial/G1 young event off ZGC collections.
    pub fn zgc_jfr_tenuring_threshold(&self) -> Option<u32> {
        match &self.inner {
            VmHeapInner::Generational(_) | VmHeapInner::G1(_) => None,
            #[cfg(feature = "zgc")]
            VmHeapInner::Zgc(h) => h.jfr_tenuring_threshold(),
        }
    }

    /// zs s7/jf: the page HotSpot 25 ZGC would have stalled on for the request
    /// this heap's arena last refused (`ZgcRealHeap::jfr_stall_request_bytes`,
    /// mapped by `hotspot_stall_page` over `max_heap_bytes`): the `type` and
    /// `size` of `jdk.ZAllocationStall`. `None` off ZGC and when no recent
    /// refusal is on record.
    pub fn zgc_jfr_stall_page(&self, max_heap_bytes: usize) -> Option<(&'static str, u64)> {
        match &self.inner {
            VmHeapInner::Generational(_) | VmHeapInner::G1(_) => {
                let _ = max_heap_bytes;
                None
            }
            #[cfg(feature = "zgc")]
            VmHeapInner::Zgc(h) => h
                .jfr_stall_request_bytes()
                .map(|bytes| cratonvm_gc_zgc::zgc::hotspot_stall_page(bytes as u64, max_heap_bytes as u64)),
        }
    }

    /// zs s4/ob3: the last ZGC collection's phase boundaries for the pause
    /// bean's split (`ZgcRealHeap::take_pause_split`), taken once; `None`
    /// elsewhere (G1 and Generational book one record per pause, unchanged).
    pub fn zgc_take_pause_split(&self) -> Option<cratonvm_gc_core::gc_metrics::ZgcPauseSplit> {
        match &self.inner {
            VmHeapInner::Generational(_) | VmHeapInner::G1(_) => None,
            #[cfg(feature = "zgc")]
            VmHeapInner::Zgc(h) => h.take_pause_split(),
        }
    }

    /// zr z1/tr: `-XX:SoftMaxHeapSize` on ZGC (the warm-up rule and the
    /// `CRATONVM_ZGC_RATE_TRIGGER` rules read it); a no-op elsewhere (HotSpot
    /// reads the flag on ZGC only).
    pub fn set_soft_max_heap_bytes(&self, bytes: usize) {
        match &self.inner {
            VmHeapInner::Generational(_) | VmHeapInner::G1(_) => {}
            #[cfg(feature = "zgc")]
            VmHeapInner::Zgc(h) => h.set_soft_max_heap_bytes(bytes),
        }
    }

    /// zt t6/tc: the manageable `SoftMaxHeapSize` JMX row on ZGC: `(soft
    /// maximum, capacity, origin kind)` (see
    /// `ZgcRealHeap::soft_max_origin_kind`); `None` elsewhere (the row is
    /// listed on ZGC only, which is where it has an effect).
    pub fn zgc_soft_max_flag(&self) -> Option<(u64, u64, u8)> {
        match &self.inner {
            VmHeapInner::Generational(_) | VmHeapInner::G1(_) => None,
            #[cfg(feature = "zgc")]
            VmHeapInner::Zgc(h) => Some((
                h.reported_soft_max_capacity() as u64,
                h.heap_capacity() as u64,
                h.soft_max_origin_kind(),
            )),
        }
    }

    /// zt t6/tc: `setVMOption("SoftMaxHeapSize", ..)` on ZGC; `false`
    /// elsewhere (nothing applied).
    pub fn zgc_set_soft_max_by_management(&self, bytes: u64) -> bool {
        match &self.inner {
            VmHeapInner::Generational(_) | VmHeapInner::G1(_) => {
                let _ = bytes;
                false
            }
            #[cfg(feature = "zgc")]
            VmHeapInner::Zgc(h) => {
                h.set_soft_max_heap_bytes_by_management(
                    usize::try_from(bytes).unwrap_or(usize::MAX),
                );
                true
            }
        }
    }

    /// zr z1/tr: HotSpot's `soft_max_capacity()` on ZGC (`None` elsewhere),
    /// for the `gc,init` block's `Soft Max Capacity:` line.
    pub fn soft_max_capacity(&self) -> Option<usize> {
        match &self.inner {
            VmHeapInner::Generational(_) | VmHeapInner::G1(_) => None,
            #[cfg(feature = "zgc")]
            VmHeapInner::Zgc(h) => Some(h.reported_soft_max_capacity()),
        }
    }

    /// The age at which a surviving object is promoted, for the backend's
    /// YOUNG generation — `None` where there is no young generation to promote
    /// out of.
    ///
    /// Exists for the JFR `jdk.YoungGarbageCollection` event, whose one
    /// production caller passed the literal `15` for every backend until
    /// 2026-09-23 (G1's configured ceiling; Generational promotes at 3). Per
    /// arm:
    ///
    /// * Generational — `GenerationalHeap::hotspot_tenuring_threshold`, in
    ///   HotSpot's unit like G1's (gen r5w2/obs6): `gen_heap::PROMOTION_AGE -
    ///   1` (= 2) by default, the adaptive threshold itself when adaptive
    ///   tenuring is engaged. Until gen r5w2/obs6 this arm reported the
    ///   collector's "survivals before promotion" (`PROMOTION_AGE`, 3), one
    ///   above what HotSpot and the G1 arm report for the same policy
    ///   (`gengc-r4w6-young6-tenuring-threshold-units-differ-across-backends`).
    /// * G1 — the threshold the NEXT pause will use (adaptive by default; the
    ///   configured `promotion_age` under `CRATONVM_G1_ADAPTIVE_TENURING=0`).
    /// * ZGC — `None`: the default collector is non-generational, and its
    ///   opt-in generational mode ages objects by header label rather than by
    ///   a young space a JFR young-GC event describes.
    pub fn tenuring_threshold(&self) -> Option<u32> {
        match &self.inner {
            VmHeapInner::Generational(h) => Some(u32::from(h.hotspot_tenuring_threshold())),
            VmHeapInner::G1(h) => Some(u32::from(h.collector.tenuring_threshold())),
            #[cfg(feature = "zgc")]
            VmHeapInner::Zgc(_) => None,
        }
    }

    /// Return the total heap capacity in bytes.
    pub fn heap_capacity(&self) -> usize {
        match &self.inner {
            VmHeapInner::Generational(h) => {
                // 2 semi-spaces (only one active) + old gen
                h.young_semi_capacity() + h.old_gen_capacity()
            }
            VmHeapInner::G1(h) => h.heap_capacity(),
            #[cfg(feature = "zgc")]
            VmHeapInner::Zgc(h) => h.heap_capacity(),
        }
    }

    /// Return young generation (used, capacity) in bytes.
    ///
    /// G1 reports Eden + Survivor regions — HotSpot's two young pools, "G1 Eden
    /// Space" and "G1 Survivor Space" (gc-common w4-f; Survivor was on the old
    /// side until then). Everything else a G1 heap holds is in
    /// [`Self::old_gen_stats`]. Both arms read one
    /// [`G1Collector::region_census`] each.
    pub fn young_gen_stats(&self) -> (usize, usize) {
        match &self.inner {
            VmHeapInner::Generational(h) => {
                let from = h.young_from_used();
                let cap = h.young_semi_capacity();
                (from, cap)
            }
            VmHeapInner::G1(h) => {
                let c = h.region_census();
                (
                    c.eden.0 + c.survivor.0,
                    (c.eden.1 + c.survivor.1) * c.region_size,
                )
            }
            #[cfg(feature = "zgc")]
            VmHeapInner::Zgc(_) => (0, 0),
        }
    }

    /// Return old generation (used, capacity) in bytes.
    ///
    /// # G1: everything that is not Eden (2026-09-23)
    ///
    /// This arm returned `G1Collector::old_gen_stats` — regions of type `Old`
    /// only, capacity = `count(Old) × region_size`. Survivor, `HumongousStart`
    /// / `HumongousContinuation` and Free regions were in NEITHER this nor
    /// [`Self::young_gen_stats`], so the serviceability heap summary built from
    /// the pair reported, after a young pause, a heap the size of its Old
    /// regions, and a 100 MiB humongous array as 0 bytes used
    /// (`docs/feature-designs/concurrent-gc-maturation.md`).
    ///
    /// It is now HotSpot's "G1 Old Gen": `used` = Old + humongous regions'
    /// bytes (a humongous start region's cursor spans the whole object),
    /// `capacity = heap_capacity − young capacity` (so Free regions count,
    /// HotSpot's "old absorbs the rest" convention). The young/old pair
    /// therefore still partitions the whole heap. Since gc-common w4-f
    /// Survivor regions are on the YOUNG side ([`Self::young_gen_stats`]),
    /// read from [`G1Collector::region_census`] — one region-table walk, so
    /// the families within one call are a snapshot; the young and old calls
    /// are two walks, so under a racing allocation the PAIR is a sample
    /// (capacity saturating, never below `used`).
    pub fn old_gen_stats(&self) -> (usize, usize) {
        match &self.inner {
            VmHeapInner::Generational(h) => {
                let cap = h.old_gen_capacity();
                let used = h.old_gen_used();
                (used, cap)
            }
            VmHeapInner::G1(h) => {
                let c = h.region_census();
                let young_cap = (c.eden.1 + c.survivor.1) * c.region_size;
                let used = c.old.0 + c.humongous.0;
                (used, h.heap_capacity().saturating_sub(young_cap).max(used))
            }
            #[cfg(feature = "zgc")]
            VmHeapInner::Zgc(h) => (h.allocated_bytes(), h.heap_capacity()),
        }
    }

}

impl VmHeap {
    pub fn note_external_gpu_retained(&self, delta_bytes: i64, wrapper_delta: i64) -> bool {
        match &self.inner {
            VmHeapInner::Generational(h) => h.note_external_gpu_retained(delta_bytes, wrapper_delta),
            VmHeapInner::G1(h) => h.collector.note_external_gpu_retained(delta_bytes, wrapper_delta),
            #[cfg(feature = "zgc")]
            VmHeapInner::Zgc(h) => h.note_external_gpu_retained(delta_bytes, wrapper_delta),
        }
    }

    /// Current estimated off-heap GPU VRAM (bytes) retained by live wrappers.
    #[inline]
    pub fn external_gpu_retained_bytes(&self) -> u64 {
        match &self.inner {
            VmHeapInner::Generational(h) => h.external_gpu.retained_bytes(),
            VmHeapInner::G1(h) => h.collector.external_gpu.retained_bytes(),
            #[cfg(feature = "zgc")]
            VmHeapInner::Zgc(h) => h.external_gpu.retained_bytes(),
        }
    }

    /// Current count of live GPU wrapper handles attributed to this heap.
    #[inline]
    pub fn external_gpu_live_wrappers(&self) -> u64 {
        match &self.inner {
            VmHeapInner::Generational(h) => h.external_gpu.live_wrappers(),
            VmHeapInner::G1(h) => h.collector.external_gpu.live_wrappers(),
            #[cfg(feature = "zgc")]
            VmHeapInner::Zgc(h) => h.external_gpu.live_wrappers(),
        }
    }

    /// Whether external GPU wrapper pressure currently warrants a GC cycle.
    #[inline]
    pub fn external_gpu_should_trigger_gc(&self) -> bool {
        match &self.inner {
            VmHeapInner::Generational(h) => h.external_gpu.should_trigger_gc(),
            VmHeapInner::G1(h) => h.collector.external_gpu.should_trigger_gc(),
            #[cfg(feature = "zgc")]
            VmHeapInner::Zgc(h) => h.external_gpu.should_trigger_gc(),
        }
    }

    /// Override external GPU pressure thresholds on Generational/ZGC/G1 heaps.
    pub fn set_external_gpu_pressure_policy(
        &self,
        trigger_bytes: u64,
        per_object_bias_bytes: u64,
        min_floor_bytes: u64,
    ) {
        match &self.inner {
            VmHeapInner::Generational(h) => h
                .external_gpu
                .set_policy(trigger_bytes, per_object_bias_bytes, min_floor_bytes),
            VmHeapInner::G1(h) => h
                .collector
                .external_gpu
                .set_policy(trigger_bytes, per_object_bias_bytes, min_floor_bytes),
            #[cfg(feature = "zgc")]
            VmHeapInner::Zgc(h) => h
                .external_gpu
                .set_policy(trigger_bytes, per_object_bias_bytes, min_floor_bytes),
        }
    }

}
