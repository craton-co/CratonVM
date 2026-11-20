#[allow(unused_imports)]
use super::*;

impl ZgcRealHeap {
    /// Preserve per-slot read diagnostics when the punned-cell watch is armed.
    pub fn compact_batch_reads_enabled(&self) -> bool {
        !punned_store_watch_armed()
    }

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
        if zgc_corpse_enabled() || punned_store_watch_armed() {
            <Self as GarbageCollector>::set_field(self, obj, index, value);
            return;
        }
        // No reference is overwritten or installed, so neither SATB nor the
        // remembered-set barrier applies. This is set_field's primitive store.
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

    /// May a batch whose compact plan [`VmHeap::try_set_compact_fields`]
    /// validated store its REFERENCE slots raw, with one card for the batch
    /// ([`Self::set_resolved_compact_reference_field_no_card`] then
    /// [`Self::note_ref_store`])? Exactly when `set_field`'s compact reference
    /// store is that store: no marking (so no SATB pre-barrier), no colored
    /// stores, and neither diagnostic watch (corpse, punned slot) armed. The
    /// stack-walker residual-costs page, item 2.
    ///
    /// [`VmHeap::try_set_compact_fields`]: cratonvm_gc_core::gpu_critical::VmHeap::try_set_compact_fields
    #[inline]
    pub fn compact_reference_batch_allowed(&self) -> bool {
        !self.mark_active.load(Ordering::Relaxed)
            && !self.load_barrier_colored_stores()
            && !zgc_corpse_enabled()
            && !punned_store_watch_armed()
    }

    /// `set_field_no_card`'s compact reference store, for a slot VmHeap's
    /// complete batch plan resolved (an object value: never boxed), under
    /// [`Self::compact_reference_batch_allowed`]. The caller cards the object
    /// once after the batch.
    ///
    /// # Safety
    /// off/storage must describe this receiver's in-bounds reference slot.
    #[inline]
    pub unsafe fn set_resolved_compact_reference_field_no_card(
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

    /// The name of `class_id` in THIS heap's VM, for the guard and corpse
    /// diagnostics: [`cratonvm_gc_core::collector::class_name_for_diagnostics_in`] with
    /// the heap's layout domain, its VM's owner token. The domain-less namer
    /// answers from whichever VM has the id first, which with two VMs may name
    /// another VM's class (interpreter round i1 wave 23, lane L5).
    pub fn diag_class_name(&self, class_id: u32) -> String {
        cratonvm_gc_core::collector::class_name_for_diagnostics_in(self.layout_domain(), class_id)
    }

    /// The arena envelope, for conservative root scanning.
    ///
    /// `VmHeap::conservative_addr_span`'s ZGC arm answered `None`, justified by
    /// "ZGC keeps live bases in a registry, not a contiguous arena". That
    /// premise is false — this heap is one `Arena`, created in
    /// [`Self::with_capacity`] and never grown — and the consequence was real:
    /// the sole consumer (`vm/src/jit/conservative_roots.rs`) hoists this span
    /// out of its scan loop and rejects a word with an inline
    /// `w < lo || w >= hi`. With `None` it cannot, so EVERY 8-byte stack word
    /// went through `is_object_address`, which at the time opened with
    /// `registry.lock()` — one global mutex acquire per candidate qword, per
    /// frame, per thread. (That lock is gone as of the object-start bitmap; the
    /// span filter still earns its keep, because rejecting a word with a range
    /// compare beats even an atomic load and a mask.)
    ///
    /// The same two numbers grid the bitmap — see [`ZObjectStarts`] and the
    /// "Object-start membership" section header. That is not a coincidence but
    /// the same fact used twice: this arena is created once and never grown.
    ///
    /// **This is a filter, not an answer.** A word inside the span must still
    /// go through the exact-base registry test. Widening it into an acceptance
    /// path would readmit interior pointers as object bases — the `is_addr_live`
    /// unsoundness that was fixed separately, where the sweep's coalescing of
    /// free spans lets a dead object's base become interior to a live one.
    ///
    /// Answers without taking the arena lock; see the fields' note.
    pub fn conservative_addr_span(&self) -> Option<(usize, usize)> {
        if self.arena_end > self.arena_base {
            Some((self.arena_base, self.arena_end))
        } else {
            None
        }
    }

    /// Contiguous committed `(base, len)` span inside the ZGC arena covering
    /// `addr`, derived from the arena's committed-granule bitmap.
    ///
    /// Unlike [`Self::conservative_addr_span`], this excludes uncommitted
    /// reserved-only granules so callers such as GPU pinned-host registration
    /// (`cuMemHostRegister_v2`) never span unmapped virtual address space.
    pub fn committed_span_containing(&self, addr: usize) -> Option<(usize, usize)> {
        if self.arena_base == 0
            || self.arena_end <= self.arena_base
            || addr < self.arena_base
            || addr >= self.arena_end
        {
            return None;
        }
        let commit_bits = self.arena.lock().commit_bits();
        let total = self.arena_end - self.arena_base;
        let Some(bits) = commit_bits else {
            return Some((self.arena_base, total));
        };
        let granule = cratonvm_gc_core::reservation::GRANULE;
        let num_granules = total.div_ceil(granule);
        let g = (addr - self.arena_base) / granule;
        if g >= num_granules || !cratonvm_gc_core::reservation::granule_committed(&bits, g) {
            return None;
        }
        let mut start_g = g;
        while start_g > 0 && cratonvm_gc_core::reservation::granule_committed(&bits, start_g - 1) {
            start_g -= 1;
        }
        let mut end_g = g + 1;
        while end_g < num_granules && cratonvm_gc_core::reservation::granule_committed(&bits, end_g)
        {
            end_g += 1;
        }
        let start_off = start_g * granule;
        let end_off = (end_g * granule).min(total);
        Some((
            self.arena_base + start_off,
            end_off.saturating_sub(start_off),
        ))
    }

    /// All contiguous committed `(base, len)` spans in the ZGC arena.
    pub fn committed_arena_spans(&self) -> Vec<(usize, usize)> {
        if self.arena_base == 0 || self.arena_end <= self.arena_base {
            return Vec::new();
        }
        let commit_bits = self.arena.lock().commit_bits();
        let total = self.arena_end - self.arena_base;
        let Some(bits) = commit_bits else {
            return vec![(self.arena_base, total)];
        };
        let granule = cratonvm_gc_core::reservation::GRANULE;
        let num_granules = total.div_ceil(granule);
        let mut spans = Vec::new();
        let mut g = 0usize;
        while g < num_granules {
            if !cratonvm_gc_core::reservation::granule_committed(&bits, g) {
                g += 1;
                continue;
            }
            let start_g = g;
            g += 1;
            while g < num_granules && cratonvm_gc_core::reservation::granule_committed(&bits, g) {
                g += 1;
            }
            let start_off = start_g * granule;
            let end_off = (g * granule).min(total);
            spans.push((
                self.arena_base + start_off,
                end_off.saturating_sub(start_off),
            ));
        }
        spans
    }

    /// The arena ENVELOPE (the reservation, `-Xmx`) -- not what is committed.
    ///
    /// zr z1/tr: this said "Bytes committed for the Java heap", and its name
    /// says the same, but `VmHeap::committed_bytes` (`Runtime.totalMemory()`,
    /// the JMX `committed`) has answered [`Self::os_committed_bytes`] on this
    /// collector since 2026-09-23; this one equals [`Self::heap_capacity`].
    /// Kept for its callers; read `os_committed_bytes` for the committed heap.
    ///
    /// Fixed for the collector's lifetime: the arena is allocated once in
    /// [`Self::with_capacity`] and never grown, which is the same property that
    /// makes `arena_base`/`arena_end` safe to read without the lock.
    pub fn committed_bytes(&self) -> usize {
        self.arena_end.saturating_sub(self.arena_base)
    }

    /// Bytes of that envelope the arena has actually asked the OS to BACK.
    ///
    /// [`Self::committed_bytes`] is the reservation — fixed for the collector's
    /// lifetime and equal to `-Xmx`. This is what the process is holding: on a
    /// reserving store the arena commits in 2 MiB granules as the cursors reach
    /// them, so a fresh `-Xmx8g` ZGC process holds megabytes, and `-Xms` is what
    /// moves this number up at startup.
    ///
    /// Takes the arena lock, so it is a diagnostic and a test instrument, not
    /// something to call on an allocation path. See
    /// `docs/feature-designs/zgc-architecture.md`
    /// for why the distinction needed an instrument at all.
    pub fn os_committed_bytes(&self) -> usize {
        // zr z4/un: net of the granules a readable give-back reset and nothing
        // has reused since (`CRATONVM_ZGC_RESET_ACCOUNTING`, default ON; `=0`
        // keeps the gross count). The default give-back RESETS rather than
        // decommits, so the gross count never fell after a give-back while
        // HotSpot's `totalMemory()` falls after its uncommitter runs
        // (25.0.3 `Zr4UnIdleRss`: 291504128 -> 148897792 during the idle).
        self.arena.lock().net_committed_bytes()
    }

    /// Stable gate addresses owned by this VM. Both gates are isolated from
    /// other heaps, including the SATB pre gate while two ZGC heaps mark.
    pub fn jit_ref_store_plan(&self) -> (usize, usize, usize, u8) {
        let cell = self.counters.jit_gate_cell.load(Ordering::Acquire);
        // SAFETY: only `place_jit_ref_store_gates` stores a non-null pointer,
        // from a `&'static JitRefStoreGates`.
        let own: &cratonvm_gc_core::jit_bounds::JitRefStoreGates = if cell.is_null() {
            &self.jit_ref_store_gates
        } else {
            unsafe { &*cell }
        };
        (
            &own.pre_active as *const std::sync::atomic::AtomicU8 as usize,
            &own.post_active as *const std::sync::atomic::AtomicU8 as usize,
            &own.young_floor as *const std::sync::atomic::AtomicU8 as usize,
            0,
        )
    }

    /// t1 wave 3 lane Q: write one byte of this heap's JIT gate block and of
    /// its code-adjacent copy (`ZgcCounters::jit_gate_cell`), Box first. The ONLY
    /// writer of either, so both blocks always hold the same bytes and every
    /// arming / lowering order a caller keeps holds for both.
    ///
    /// t1 wave 5 lane Q (`t1-V3-zgc-gate-cell-late-placement-*` item 1): the
    /// Box store and the cell-pointer load are `SeqCst`, the other half of
    /// the Dekker pair with [`Self::place_jit_ref_store_gates`] (its `SeqCst`
    /// CAS, then its Box reads): either this load sees the cell and writes it,
    /// or the placer's re-read sees this Box byte. Gate writes happen at mark
    /// start / end and generational transitions only, so the fence is free.
    /// Two writers of the SAME byte racing each other are not ordered by this
    /// (they never are: the pause protocol serialises them).
    #[inline]
    pub fn store_jit_gate(
        &self,
        pick: impl Fn(&cratonvm_gc_core::jit_bounds::JitRefStoreGates) -> &std::sync::atomic::AtomicU8,
        v: u8,
    ) {
        pick(&self.jit_ref_store_gates).store(v, Ordering::SeqCst);
        let cell = self.counters.jit_gate_cell.load(Ordering::SeqCst);
        if !cell.is_null() {
            // SAFETY: only `place_jit_ref_store_gates` stores a non-null
            // pointer, from a `&'static JitRefStoreGates`.
            // t1 wave 6 lane Y4 (t1-V5): SeqCst, not Release, so this store is
            // in the single total order with the placer's re-copy loop and
            // `place_jit_ref_store_gates`' argument is a proof.
            pick(unsafe { &*cell }).store(v, Ordering::SeqCst);
        }
    }

    /// t1 wave 3 lane Q (`t1-R-zgc-gate-block-out-of-rip-reach`,
    /// `CRATONVM_ZGC_JIT_GATE_CELL`): place this heap's JIT gate block in
    /// `cell` (code-adjacent, from `jit::platform::alloc_code_adjacent_cell`),
    /// so the fused one-instruction gate (`CMP word [rip+d32], 0`) is in reach.
    /// First placement wins; `false` (nothing changed) once placed.
    ///
    /// Call right after construction, before the first compile and before any
    /// other thread can write a gate (the VM's heap construction): the copy is
    /// filled ARMED (the safe direction), published, then set to the Box's
    /// bytes. Code compiled before the call keeps reading the Box, which stays
    /// maintained.
    ///
    /// t1 wave 5 lane Q (`t1-V3-zgc-gate-cell-late-placement-*` item 1): a
    /// late call, racing a [`Self::store_jit_gate`] on another thread, is sound
    /// too. Each byte is copied until a re-read of the Box byte equals the
    /// value just stored: a writer that saw the published cell wrote the Box
    /// FIRST, so if its cell store landed before this copy's store, the
    /// re-read sees its Box value and the loop copies again; if it landed
    /// after, it wrote the Box's final value itself. (Before this, a placer
    /// that read `pre_active == 0`, then lost the race to a marker's arming,
    /// stored 0 over the marker's 1: cell lowered while the Box was armed.)
    pub fn place_jit_ref_store_gates(
        &self,
        cell: &'static cratonvm_gc_core::jit_bounds::JitRefStoreGates,
    ) -> bool {
        let own = &self.jit_ref_store_gates;
        cell.pre_active.store(1, Ordering::SeqCst);
        cell.post_active.store(1, Ordering::SeqCst);
        cell.young_floor.store(0, Ordering::SeqCst);
        cell.post_skip_mask.store(0, Ordering::SeqCst);
        cell.published
            .store(own.published.load(Ordering::SeqCst), Ordering::SeqCst);
        let ptr = cell as *const cratonvm_gc_core::jit_bounds::JitRefStoreGates as *mut _;
        if self
            .counters
            .jit_gate_cell
            .compare_exchange(
                std::ptr::null_mut(),
                ptr,
                Ordering::SeqCst,
                Ordering::SeqCst,
            )
            .is_err()
        {
            return false;
        }
        for (src, dst) in [
            (&own.young_floor, &cell.young_floor),
            (&own.post_skip_mask, &cell.post_skip_mask),
            (&own.post_active, &cell.post_active),
            (&own.pre_active, &cell.pre_active),
        ] {
            loop {
                let v = src.load(Ordering::SeqCst);
                dst.store(v, Ordering::SeqCst);
                if src.load(Ordering::SeqCst) == v {
                    break;
                }
            }
        }
        true
    }

    /// The startup commit (`-Xms`, the uncommit floor): `ZGC Old
    /// Generation`'s JMX `init` (gcd d10/o retirement). Takes the arena lock.
    pub fn initial_committed_bytes(&self) -> usize {
        // zr z3/fp: the startup commit as recorded at construction; the arena's
        // floor equals it unless `set_uncommit_floor` lowered the floor.
        self.counters.fp.initial_commit.load(Ordering::Relaxed)
    }

    /// The committed-byte count uncommit never goes below (zr z3/fp).
    pub fn uncommit_floor_bytes(&self) -> usize {
        self.arena.lock().uncommit_floor()
    }

    /// Lower (or raise) the uncommit floor to `bytes` -- HotSpot's
    /// `MinHeapSize` -- keeping the `ZUncommit` / `ZUncommitDelay` policy
    /// (zr z3/fp, `CRATONVM_ZGC_UNCOMMIT_TO_MIN_HEAP`, default ON; `=0`
    /// leaves the floor at the startup commit).
    ///
    /// # Why the startup commit was the wrong floor
    ///
    /// HotSpot ZGC commits `InitialHeapSize` at startup and uncommits unused
    /// memory down to `MinHeapSize` (`-Xlog:gc+init`: `Min Capacity: 8M`,
    /// `Initial Capacity: 256M`). This heap floored at the startup commit. Since
    /// the launcher adopted HotSpot's ergonomic initial heap (`physical / 64`,
    /// `CRATONVM_GC_HOTSPOT_INITIAL_HEAP`), that commit is the whole `-Xmx` on
    /// any host with 16 GiB or more at `-Xmx256m`, and at `-Xmx1g` on 64 GiB:
    /// the floor was the capacity, nothing could ever be given back, and the
    /// `[gc,init]` block still printed `Uncommit: Enabled` with `Min Capacity:
    /// 8M`. `-Xms` (which sets `MinHeapSize` too) keeps its floor.
    pub fn set_uncommit_floor(&self, bytes: usize) {
        let mut arena = self.arena.lock();
        if let Some((enabled, delay)) = arena.uncommit_settings() {
            arena.set_uncommit_policy(bytes, enabled, delay);
        }
    }

    /// Per-VM ZUncommit policy. Startup committed bytes are the floor even
    /// when the delay is zero; disabling uncommit covers every arena path.
    pub fn set_uncommit_options(&self, enabled: bool, delay_secs: u64) {
        let mut arena = self.arena.lock();
        let floor = arena.uncommit_floor();
        arena.set_uncommit_policy(floor, enabled, std::time::Duration::from_secs(delay_secs));
    }

    /// zt t23/hunt4 (`zgc-zt22hunt3-always-pre-touch-ignored-*`): HotSpot's
    /// `-XX:+AlwaysPreTouch`, per VM. Under `on` the bytes committed now (the
    /// `-Xms` startup commit) are touched before this returns, and every
    /// later commit that makes a granule resident touches it as it is made
    /// (`Arena::set_pre_touch`), as HotSpot ZGC pre-touches its initial
    /// capacity at heap initialisation and each later commit. Call before the
    /// first allocation, after [`Self::set_uncommit_options`] (the policy box
    /// carries the bit). Answers whether the option is in force -- `false`
    /// for `on == false`, and under `CRATONVM_ZGC_ALWAYS_PRE_TOUCH=0` (the
    /// option accepted and ignored, as before) -- which is what the
    /// `gc,init` block's `Pre-touch:` line states.
    pub fn set_always_pre_touch(&self, on: bool) -> bool {
        if !on || !zgc_always_pre_touch_enabled() {
            return false;
        }
        let touched = self.arena.lock().set_pre_touch(true);
        tracing::debug!(
            target: "cratonvm::gc",
            touched,
            "zgc: -XX:+AlwaysPreTouch touched the startup commit",
        );
        true
    }

    /// zt t24/hunt7 (`zgc-zt23hunt4-transparent-huge-pages-flag-ignored-*`):
    /// HotSpot's `-XX:+UseTransparentHugePages`, per VM: advise this heap's
    /// arena reservation `MADV_HUGEPAGE` (Linux), as HotSpot 25 ZGC advises
    /// its heap mapping when its large-page state is `Transparent`. The
    /// caller decides that state from the kernel's THP modes and
    /// `CRATONVM_ZGC_TRANSPARENT_HUGE_PAGES`; call before
    /// [`Self::set_always_pre_touch`] and the first allocation, so the
    /// startup commit is faulted in huge. Answers whether the kernel took the
    /// advice (`false` off Linux and on the owned fallback).
    pub fn advise_transparent_huge_pages(&self) -> bool {
        let advised = self.arena.lock().advise_huge_pages();
        tracing::debug!(
            target: "cratonvm::gc",
            advised,
            "zgc: -XX:+UseTransparentHugePages advised the heap MADV_HUGEPAGE",
        );
        advised
    }

    /// zki (page `zgc-zt23hunt4-*` item 4): is this heap backed by explicit
    /// hugetlbfs pages ([`Self::with_capacity_initial_and_large_pages`])?
    pub fn explicit_huge_pages(&self) -> bool {
        self.arena.lock().is_explicit_huge_pages()
    }

    /// zki: the startup commit on explicit hugetlbfs pages, `Some((refusals,
    /// short))`: how many commits the pool refused while making it (HotSpot
    /// logs `Failed to commit memory (Not enough space)` for each) and whether
    /// it fell short of the initial heap (HotSpot then fails VM creation with
    /// `Failed to allocate initial Java heap`). `None` on any other heap.
    pub fn explicit_initial_commit(&self) -> Option<(usize, bool)> {
        if !self.explicit_huge_pages() {
            return None;
        }
        Some((
            self.counters
                .fp
                .hugetlb_initial_refusals
                .load(Ordering::Relaxed),
            self.counters
                .fp
                .hugetlb_initial_short
                .load(Ordering::Relaxed),
        ))
    }

    /// zki: the pool refused HotSpot's uncommit probe on this heap
    /// ([`ZgcExplicitPages::uncommit_probe`]): uncommit is "not supported by
    /// operating system" (the VM disables it and says so in `gc,init`).
    pub fn explicit_uncommit_unsupported(&self) -> bool {
        self.counters
            .fp
            .hugetlb_uncommit_unsupported
            .load(Ordering::Relaxed)
    }

    /// zki: commits for use the hugetlbfs pool has refused since the heap was
    /// built (after its startup commit); `0` on any other heap.
    pub fn hugetlb_commit_refusals(&self) -> usize {
        self.arena.lock().hugetlb_commit_refusals()
    }

    /// zki (page `zgc-zt23hunt4-*` item 4): HotSpot 25 ZGC's report of a
    /// commit the hugetlbfs pool refused after start-up, once: its `Failed to
    /// commit memory` lines (the whole-or-halving commit of the refused
    /// request against an empty pool, [`zgc_explicit_commit_refusal_lines`])
    /// and `Forced to lower max Java heap size` from the maximum to what is
    /// committed now (`ZPageAllocator` lowers its current maximum and does
    /// not try to grow past it again, so later refusals print nothing).
    /// Measured on 25.0.4 with a 230-page pool (`-Xmx512m`, row `zki_f4`):
    /// two `Failed` lines and `from 512M(100%) to 218M(43%)`. `None` when no
    /// new refusal happened or the maximum was already lowered.
    pub fn take_hugetlb_refusal_report(&self) -> Option<ZgcHugetlbRefusalReport> {
        if !self.counters.fp.hugetlb_backed.load(Ordering::Acquire) {
            return None;
        }
        let (refusals, granules, committed) = {
            let arena = self.arena.lock();
            if !arena.is_explicit_huge_pages() {
                return None;
            }
            (
                arena.hugetlb_commit_refusals(),
                arena.hugetlb_last_refused_granules(),
                arena.committed_bytes(),
            )
        };
        let fp = &self.counters.fp;
        let reported = fp
            .hugetlb_refusals_reported
            .swap(refusals, Ordering::AcqRel);
        if refusals <= reported || fp.hugetlb_lowered_max.load(Ordering::Acquire) != 0 {
            return None;
        }
        let lowered_to = committed.max(1);
        fp.hugetlb_lowered_max.store(lowered_to, Ordering::Release);
        Some(ZgcHugetlbRefusalReport {
            failed_commit_lines: zgc_explicit_commit_refusal_lines(granules),
            max_capacity: self.heap_capacity(),
            lowered_to,
        })
    }

    /// zt t23/hunt4: bytes `-XX:+AlwaysPreTouch` has touched on this heap
    /// (startup commit plus later commits); `0` without the option.
    pub fn pre_touched_bytes(&self) -> usize {
        self.arena.lock().pre_touched_bytes()
    }

    /// Start this heap's UNCOMMITTER -- HotSpot's `ZUncommitter` (zr z4/un,
    /// `CRATONVM_ZGC_UNCOMMITTER`, default ON, `=0` kill): a per-heap thread
    /// that gives back memory unused for `ZUncommitDelay`, down to the floor
    /// (`MinHeapSize`), whether or not a collection runs. Before it the only
    /// give-back ran at the start of a collection, so an idle process kept
    /// its peak for ever (`zgc-zr3fp-no-uncommitter-an-idle-heap-never-gives-back-FIXED-20261012.md`).
    ///
    /// Call once, after [`Self::set_uncommit_options`] and
    /// [`Self::set_uncommit_floor`]. Starts nothing (and answers `false`) when
    /// the switch is off, uncommit is disabled (`-XX:-ZUncommit`), the floor
    /// is the whole heap (`-Xms == -Xmx`, HotSpot's "Implicitly Disabled"),
    /// the store is wholly committed (`CRATONVM_GC_RESERVE=0`), or the arena
    /// gives back by DECOMMITTING (`CRATONVM_ZGC_READABLE_GIVE_BACK=0`, or no
    /// JIT read bound published): a decommitted granule faults on touch, and
    /// this pass runs beside the mutators, so it only ever resets.
    ///
    /// `log` receives HotSpot's `[info][gc,heap]` line for every pass that
    /// released something ([`zgc_uncommitter_log_line`]); the VM routes it
    /// through `-Xlog`. `jfr` receives the same pass as
    /// `(bytes, start_epoch_nanos, duration_nanos)` for `jdk.ZUncommit`
    /// (zr z4/jf's event; HotSpot emits it from its uncommitter thread, so it
    /// cannot ride on a collection's facts). The thread holds a `Weak` reference and exits when the
    /// heap is gone or [`Self::stop_uncommitter`] runs. Per heap, never a
    /// process global: two VMs in one process each get their own.
    pub fn start_uncommitter(
        self: &std::sync::Arc<Self>,
        log: Option<std::sync::Arc<dyn Fn(&str) + Send + Sync>>,
        jfr: Option<ZUncommitJfrSink>,
    ) -> bool {
        // zt t1/rl (`zgc-zr5rv5-uncommitter-resets-beside-mutators-on-windows`,
        // option 1, opt-in): the Windows reset is a decommit plus a re-commit,
        // a fault window while mutators run. Under the switch no thread
        // starts there; the sweep still stamps idle granules and the
        // collection-start give-back (world stopped) releases them, the
        // pre-z4 behaviour. Other platforms ignore it (their reset has no
        // window).
        if zgc_uncommitter_refused_on_this_platform(
            cfg!(windows),
            zgc_uncommitter_windows_pause_only_enabled(),
        ) {
            return false;
        }
        {
            let arena = self.arena.lock();
            let Some((enabled, _delay)) = arena.uncommit_settings() else {
                return false;
            };
            if !enabled
                || arena.uncommit_floor() >= arena.capacity()
                || arena.commit_bits().is_none()
                || !arena.is_readable_give_back()
            {
                return false;
            }
        }
        let mut slot = self.counters.fp.uncommitter.lock();
        if slot.is_some() {
            return true;
        }
        let stop = std::sync::Arc::new((Mutex::new(false), parking_lot::Condvar::new()));
        let weak = std::sync::Arc::downgrade(self);
        let thread_stop = std::sync::Arc::clone(&stop);
        // Latched: the heap can be dropped in a thread-local destructor (a
        // JNI context's last `Arc<SharedVm>`), where an OS join deadlocks on
        // Windows (`cratonvm_types::latched_thread`).
        let spawned = cratonvm_types::latched_thread::spawn_latched(
            std::thread::Builder::new().name("ZUncommitter".to_string()),
            move || {
                let mut wait = ZGC_UNCOMMIT_MIN_WAIT;
                loop {
                    {
                        let (flag, cv) = &*thread_stop;
                        let mut stopped = flag.lock();
                        if !*stopped {
                            let _ = cv.wait_for(&mut stopped, wait);
                        }
                        if *stopped {
                            return;
                        }
                    }
                    let Some(heap) = weak.upgrade() else {
                        return;
                    };
                    let started = std::time::Instant::now();
                    let started_wall = std::time::SystemTime::now();
                    // zt t3/rl (t1/mk X-rl2): booked, the `uncommit_ms=`
                    // share of `[GC] zgc-cpu:`.
                    let pass = heap.uncommit_pass_booked(started);
                    let released = pass.reset + pass.decommitted;
                    if released != 0 {
                        let elapsed = started.elapsed();
                        if let Some(log) = log.as_ref() {
                            log(&zgc_uncommitter_log_line(
                                released,
                                heap.committed_bytes(),
                                elapsed,
                            ));
                        }
                        if let Some(jfr) = jfr.as_ref() {
                            let start_ns = started_wall
                                .duration_since(std::time::UNIX_EPOCH)
                                .map_or(0, |d| d.as_nanos().min(u64::MAX as u128) as u64);
                            let dur_ns = elapsed.as_nanos().min(u64::MAX as u128) as u64;
                            jfr(released, start_ns, dur_ns);
                        }
                    }
                    wait = pass.next_wait;
                    drop(heap);
                }
            },
        );
        match spawned {
            Ok(thread) => {
                *slot = Some(ZUncommitterHandle {
                    stop,
                    thread: Some(thread),
                });
                true
            }
            Err(e) => {
                tracing::warn!(
                    target: "cratonvm::gc",
                    error = %e,
                    "zgc: could not spawn the uncommitter; memory is given back only at collections",
                );
                false
            }
        }
    }

    /// Stop this heap's uncommitter, if it runs, and wait for it (zr z4/un).
    pub fn stop_uncommitter(&self) {
        let handle = self.counters.fp.uncommitter.lock().take();
        if let Some(handle) = handle {
            handle.stop_and_join();
        }
    }

    /// Is this heap's uncommitter thread running (zr z4/un)?
    pub fn uncommitter_running(&self) -> bool {
        self.counters.fp.uncommitter.lock().is_some()
    }

    /// zt t3/rl (`zgc-zr3fp-proposal-bitmap-pages-follow-arena-granules`,
    /// step 1): give back the pages of the object-start registry and of the
    /// mark bitmap that cover only arena bytes inside `spans` (arena
    /// offsets) the give-back just released. The caller holds the arena lock
    /// (no hand-out can set a start bit there), and the span holds no object,
    /// so no marker sets a mark bit there either -- which is the argument for
    /// the collection-start give-back, where a concurrent cycle may still be
    /// open (this collection closes it; zt t4/x3: this said "outside a mark",
    /// which only the uncommitter's pass guarantees). Each page is discarded
    /// only after it was read all zero, and only as whole kernel pages
    /// (`HeapBitmap::discard_zero_pages_over`). The old-object
    /// bits of the generational arm are left alone. Returns the bytes given
    /// back, also counted in `bitmap_pages_released` (`un_bitmap_released=`
    /// on `[GC] zgc-uncommitter:`).
    pub fn release_bitmap_pages_over(&self, spans: &[(usize, usize)]) -> usize {
        let base = self.arena_base;
        let mut released = 0usize;
        for &(lo, hi) in spans {
            let (lo, hi) = (base.saturating_add(lo), base.saturating_add(hi));
            if let Some(bits) = self.registry.bitmap() {
                released += bits.discard_zero_pages_over(lo, hi);
            }
            if let Some(marks) = self.mark_bits.as_ref() {
                released += marks.discard_zero_pages_over(lo, hi);
            }
        }
        self.counters
            .fp
            .bitmap_pages_released
            .fetch_add(released, Ordering::Relaxed);
        released
    }

    /// zt t3/rl (t1/mk X-rl2; zs3ac item 3): [`Self::uncommit_pass_at`] with
    /// the calling thread's CPU for the pass added to the `uncommit_ns`
    /// share ([`ZCollectionCpuTotals`], `uncommit_ms=` on `[GC] zgc-cpu:`).
    /// The `ZUncommitter` thread's entry point; nothing under
    /// `CRATONVM_ZGC_MARK_CPU_ACCOUNTING=0`.
    pub fn uncommit_pass_booked(&self, now: std::time::Instant) -> ZUncommitPass {
        let cpu = mark::ZThreadCpuSpan::start(self.collection_cpu_accounting_on());
        let pass = self.uncommit_pass_at(now);
        self.counters
            .uncommit_cpu_ns
            .fetch_add(cpu.elapsed_ns(), Ordering::Relaxed);
        pass
    }

    /// One uncommitter pass at `now` (zr z4/un): give back, in batches of
    /// [`ZGC_UNCOMMIT_BATCH_GRANULES`], every granule unused for the delay
    /// (`Arena::uncommit_idle_pass` states what makes that sound beside the
    /// mutators). Each batch holds the collection gate (`try_lock`: a
    /// collection in progress ends the pass, which retries shortly) and the
    /// arena lock, and is refused outright while a concurrent mark is open.
    /// The span holding the last slide's forwarding records is excluded:
    /// the load barrier reads them without a lock until the next collection
    /// retires them.
    ///
    /// Public for tests and the uncommitter thread; the VM never calls it.
    pub fn uncommit_pass_at(&self, now: std::time::Instant) -> ZUncommitPass {
        let fp = &self.counters.fp;
        let mut out = ZUncommitPass::default();
        let delay = match self.arena.lock().uncommit_settings() {
            Some((true, delay)) => delay,
            _ => {
                out.next_wait = ZGC_UNCOMMIT_MAX_WAIT;
                return out;
            }
        };
        let retry = delay.clamp(ZGC_UNCOMMIT_MIN_WAIT, std::time::Duration::from_secs(1));
        let mut next_due: Option<std::time::Instant> = None;
        loop {
            let Some(_gate) = fp.uncommit_gate.try_lock() else {
                fp.uncommit_busy.fetch_add(1, Ordering::Relaxed);
                out.busy = true;
                next_due = Some(now + retry);
                break;
            };
            if self.concurrent_mark_active() {
                fp.uncommit_busy.fetch_add(1, Ordering::Relaxed);
                out.busy = true;
                next_due = Some(now + retry);
                break;
            }
            let (lo, hi) = (
                self.stamp_lo.load(Ordering::Acquire),
                self.stamp_hi.load(Ordering::Acquire),
            );
            let exclude = if hi > lo && lo >= self.arena_base {
                (lo - self.arena_base)..(hi - self.arena_base)
            } else {
                0..0
            };
            let pass = {
                let mut arena = self.arena.lock();
                // zt t3/rl: the side-table pages over what this batch gave
                // back go too, under the SAME lock (nothing is handed out in
                // the span meanwhile), opt-in.
                let log = true;
                if log {
                    arena.set_release_log(true);
                }
                let pass = arena.uncommit_idle_pass(now, exclude, ZGC_UNCOMMIT_BATCH_GRANULES);
                if log {
                    let spans = arena.take_release_log();
                    self.release_bitmap_pages_over(&spans);
                }
                pass
            };
            out.reset += pass.reset;
            out.decommitted += pass.decommitted;
            if pass.decommitted != 0 {
                // A Windows reset whose re-commit the OS refused left a real
                // hole: withdraw the JIT's inline-load bound, exactly as the
                // collection-start give-back does (the one-way latch).
                self.counters
                    .bytes_uncommitted
                    .fetch_add(pass.decommitted, Ordering::Relaxed);
                if zgc_jit_read_bounds_enabled()
                    && !self
                        .counters
                        .jit_read_bounds_retracted
                        .swap(true, Ordering::Relaxed)
                {
                    cratonvm_gc_core::jit_bounds::publish_jit_read_bounds(0, 0, 0);
                }
            }
            next_due = pass.next_due;
            if !pass.budget_hit {
                break;
            }
        }
        fp.uncommit_passes.fetch_add(1, Ordering::Relaxed);
        if out.reset + out.decommitted != 0 {
            fp.uncommit_passes_released.fetch_add(1, Ordering::Relaxed);
            fp.uncommit_reset_bytes
                .fetch_add(out.reset, Ordering::Relaxed);
            fp.uncommit_decommitted_bytes
                .fetch_add(out.decommitted, Ordering::Relaxed);
        }
        // Sleep until the next granule is due -- or a whole delay when nothing
        // is pending (memory freed meanwhile is stamped by the sweep and seen
        // at the next wake-up) -- within [MIN, MAX].
        let wait = match next_due {
            Some(due) => due.saturating_duration_since(now),
            None if delay.is_zero() => std::time::Duration::from_secs(1),
            None => delay,
        };
        out.next_wait = wait.clamp(ZGC_UNCOMMIT_MIN_WAIT, ZGC_UNCOMMIT_MAX_WAIT);
        out
    }

    /// Create a heap with the default capacity ([`ZGC_REAL_DEFAULT_HEAP`]).
    pub fn new() -> Self {
        Self::with_capacity(ZGC_REAL_DEFAULT_HEAP)
    }

    /// Create a heap with the given total capacity in bytes (`-Xmx`), with no
    /// startup commit (`-Xms` absent). See
    /// [`Self::with_capacity_and_initial`].
    pub fn with_capacity(total_bytes: usize) -> Self {
        Self::with_capacity_and_initial(total_bytes, 0)
    }

    /// [`Self::with_capacity`] plus `-Xms`: `initial_bytes` of the arena are
    /// COMMITTED before this returns.
    ///
    /// # Startup commitment does not change the reservation
    ///
    /// The arena envelope and object-start bitmap remain sized by `-Xmx`.
    /// `-Xms` commits a budget across the low and high allocation ends, so
    /// initial high-end allocation does not add an avoidable granule above
    /// that budget. The arena's uncommit policy retains at least those bytes
    /// across every free-list and cursor-retraction path. It may release an
    /// idle low granule while retaining a live high granule: the floor is a
    /// committed-byte count, not a permanently pinned address interval.
    ///
    /// `0` means "not supplied" and commits nothing. The reservation, both
    /// cursors, and the heap's ability to reach `-Xmx` remain unchanged.
    pub fn with_capacity_and_initial(total_bytes: usize, initial_bytes: usize) -> Self {
        Self::with_capacity_initial_and_large_pages(total_bytes, initial_bytes, None)
    }

    /// zki (page `zgc-zt23hunt4-transparent-huge-pages-flag-ignored` item 4):
    /// [`Self::with_capacity_and_initial`] whose arena is backed by explicit
    /// hugetlbfs pages when `explicit_large_pages` is `Some` -- HotSpot 25
    /// ZGC's `Explicit` large-page state (`-XX:+UseLargePages` on a host whose
    /// pool passed the sanity mapping). Its `code_heap_bytes` are the bytes
    /// HotSpot's code cache takes from the pool first
    /// (`hotspot_code_cache_large_page_bytes`, `0` for none): they are held for
    /// the heap's life
    /// ([`cratonvm_gc_core::reservation::HugetlbHold`]; a pool that cannot give them is
    /// skipped, as HotSpot's code cache falls back silently). The startup
    /// commit is then HotSpot's
    /// (`Arena::commit_initial_explicit`: the whole initial heap, halving on
    /// the pool's refusals), and its outcome is kept for the VM
    /// ([`Self::explicit_initial_commit`]); later commits the pool refuses
    /// are counted ([`Self::hugetlb_commit_refusals`]) and handled as any
    /// refused commit (the allocation falls to a collection, then to
    /// `OutOfMemoryError`). When the kernel cannot back the arena so
    /// ([`cratonvm_gc_core::reservation::HeapStore::use_explicit_huge_pages`] refused),
    /// the heap is the plain one.
    pub fn with_capacity_initial_and_large_pages(
        total_bytes: usize,
        initial_bytes: usize,
        explicit_large_pages: Option<ZgcExplicitPages>,
    ) -> Self {
        // A new arena may reuse a dropped heap's addresses, which a thread's
        // owned-chunk record may still name (`vm_tlab::OwnedVmTlabChunk`).
        vm_tlab::invalidate_owned_vm_tlab_chunks();
        // ZGC AND COMPRESSED OOPS ARE MUTUALLY EXCLUSIVE BY ENCODING, not
        // merely unmigrated -- `zgc::vaddr`'s module docs give the arithmetic:
        // a 32-bit slot has no room for four metadata bits plus the
        // colored-word tag, and cannot express "> 2^47", so the escape
        // tripwire that the bit-63 design rests on has nothing to trip.
        // `vm/src/vm/vm_init.rs` already refuses the combination, but a
        // gc-crate test, an embedder, a fuzz target or a difftest harness can
        // reach this constructor without passing through it. `vaddr.rs` asks
        // for exactly this assertion rather than the assumption that the
        // vm_init check ran.
        //
        // `debug_assert!` and not `assert!`: the shipping path already refuses
        // the combination, so a release panic here would be reachable only from
        // a misconfigured test or embedder -- and a GC constructor that aborts
        // the process is worse than one that is wrong in a configuration
        // nothing ships. The `tracing::error!` is the release-build half, so
        // the misconfiguration is still on the record rather than silent.
        if cratonvm_types::narrow_oop::narrow_oops_enabled() {
            tracing::error!(
                target: "cratonvm::gc",
                "ZGC cannot run with compressed oops: a 32-bit reference slot \
                 cannot carry the 4 colored-pointer metadata bits plus the \
                 escape tag (see gc/src/zgc/vaddr.rs, \"Interaction with \
                 compressed oops\"). vm_init refuses this combination; this \
                 heap was built without passing through it",
            );
        }
        debug_assert!(
            !cratonvm_types::narrow_oop::narrow_oops_enabled(),
            "ZGC cannot run with compressed oops: a 32-bit reference slot cannot \
             carry the 4 colored-pointer metadata bits plus the escape tag \
             (see gc/src/zgc/vaddr.rs, \"Interaction with compressed oops\")",
        );
        let cap = total_bytes.max(4096);
        let mut arena = Arena::new(cap);
        // zr z7/rs: `CRATONVM_ZGC_ARENA_NO_THP` (opt-in), before anything is
        // committed or touched so no granule was faulted in huge first.
        let arena_no_thp = if !zgc_arena_no_thp_requested() {
            ZGC_NO_THP_UNSET
        } else if cratonvm_gc_core::reservation::heap_thp_enabled() {
            ZGC_NO_THP_YIELDED
        } else if arena.advise_no_huge_pages() {
            ZGC_NO_THP_APPLIED
        } else {
            ZGC_NO_THP_REFUSED
        };
        // Put a floor under the large-object end. Without it the split between
        // the two ends exists but carries no weight: TLAB chunks are carved at
        // 512 KiB apiece and a thread-heavy workload creates thousands of them,
        // so the low end reaches the high end's cursor while the large-object
        // region still holds almost nothing (measured: 3,829 chunk-sized spans,
        // 1.96 GB of a 2.15 GB arena, at the failing allocation). See
        // `Arena::high_reserve`, and note that the reserve is a preference the
        // low end may still overrun rather than fail.
        //
        // `capacity / 8` — 256 MiB at `-Xmx 2g`, ~120 live 2 MB buffers, which
        // is an order of magnitude more than any workload measured here holds
        // at once. The floor of 8 MiB keeps small heaps (and the gc unit tests,
        // which build arenas of a few KiB) from reserving a share that
        // `set_high_reserve`'s quarter-of-capacity clamp would then have to
        // take back.
        arena.set_high_reserve((cap / 8).max(8 * 1024 * 1024));
        // Commit startup bytes across both allocation ends without changing
        // the reservation or cursors. The floor belongs to this arena only.
        // zki: on explicit hugetlbfs pages, HotSpot's whole-or-halving commit
        // of the prefix instead.
        let mut code_heap_hold = None;
        let mut uncommit_unsupported = false;
        let explicit_initial = match explicit_large_pages {
            Some(pages) if arena.use_explicit_huge_pages() => {
                code_heap_hold =
                    cratonvm_gc_core::reservation::HugetlbHold::take(pages.code_heap_bytes);
                // HotSpot's `try_enable_uncommit` probe, before the startup
                // commit: one granule committed and given back.
                if pages.uncommit_probe {
                    uncommit_unsupported = !arena.probe_explicit_uncommit();
                }
                let (committed, refusals) = arena.commit_initial_explicit(initial_bytes);
                Some((committed, refusals + usize::from(uncommit_unsupported)))
            }
            _ => None,
        };
        let initial_commit_bytes = match explicit_initial {
            Some((committed, _)) => committed,
            None => arena.commit_initial_zgc(initial_bytes),
        };
        let explicit_initial = explicit_initial
            .map(|(committed, refusals)| (refusals, committed < initial_bytes.min(cap)));
        if initial_bytes != 0 {
            let committed = initial_commit_bytes;
            tracing::debug!(
                target: "cratonvm::gc",
                requested = initial_bytes,
                committed,
                capacity = cap,
                "zgc: -Xms startup commit",
            );
        }
        arena.set_uncommit_policy(
            initial_commit_bytes,
            true,
            std::time::Duration::from_secs(300),
        );
        // zr z3/fp: a release the `-Xms` floor refused keeps its idle stamp
        // (`CRATONVM_ZGC_FOOTPRINT_TRIM=0` restores the reset). Survives the
        // VM's later `set_uncommit_options`.
        arena.set_keep_refused_idle_stamps(zgc_default_on_switch("CRATONVM_ZGC_FOOTPRINT_TRIM"));
        // zr z4/un: count a reset granule as given back (floor and
        // `totalMemory()`), and never reset it twice
        // (`CRATONVM_ZGC_RESET_ACCOUNTING=0` restores the gross count).
        arena.set_reset_accounting(zgc_default_on_switch("CRATONVM_ZGC_RESET_ACCOUNTING"));
        // zt t2/al: a free-list fit whose commit the OS refuses puts its span
        // back on the list instead of dropping it until the next slide
        // (`CRATONVM_ZGC_REFUSED_FIT_RELISTS=0` restores the drop). Survives
        // the VM's later `set_uncommit_options`.
        arena.set_refused_fit_relists(true);
        // Let `carve_tlab_chunk` leave never-written bytes to the OS's own
        // zeroing (round 9 wave 10, `zgc10`); see `zgc_pristine_chunks_enabled`.
        arena.set_skip_pristine_zeroing(zgc_pristine_chunks_enabled());
        // zt t31/zc11: the slide's vacated tail is zeroed at hand-out.
        arena.set_defer_slide_tail_zeroing(zgc_slide_tail_zero_at_allocation_enabled());
        // Capture the arena envelope ONCE, as plain `usize`. This backs
        // `conservative_addr_span`, and it is deliberately not read through
        // `self.arena.lock()`: the span's whole purpose is to let a caller
        // reject a stack word with a range compare and no lock, so taking a
        // lock to answer it would reintroduce exactly the contention it
        // exists to delete. Sound because this arena is created here and
        // NEVER grown — there is no `Arena::grow` call anywhere in this file,
        // and `alloc_raw` is the single allocation chokepoint.
        let arena_base = arena.base_ptr() as usize;
        let arena_end = arena_base.saturating_add(arena.capacity());
        // Publish the READ-side bounds the JIT's guarded inline `getfield`
        // tests against, exactly as `G1Collector::new` does. See
        // [`zgc_jit_read_bounds_enabled`] for why this is sound under a
        // collector whose own design doc said it must not publish, and
        // [`Self::set_barrier_color`] for the coupling that keeps it sound
        // if that premise ever changes.
        if zgc_jit_read_bounds_enabled() {
            cratonvm_gc_core::jit_bounds::publish_jit_read_bounds(0, arena_base, arena_end);
            // Keep that bound TRUE across every give-back, rather than
            // withdrawing it for good at the first one (round 9 wave 9, `zgc9`;
            // see `zgc_readable_give_back_enabled`). Set before the arena is
            // shared, so no give-back ever runs in the other mode.
            arena.set_readable_give_back(zgc_readable_give_back_enabled());
        }
        // Publish the MOVABLE envelope — a third table, and deliberately not
        // `JIT_REGION_BOUNDS` (see `MovableBoundsTable`: filling that one would
        // re-enable an inline reference STORE fast path this collector must not
        // have). Without it the frame-band verifier's residency test answers
        // `false` for every address under ZGC, so the verifier inspects every
        // slot, classifies none, and reports "nothing unpublished" having
        // verified nothing — which is why `relocate_stw` could not consult the
        // coverage verdict at all.
        //
        // The WHOLE arena, not the low region the slide actually compacts. A
        // superset is the safe direction here: an address wrongly called
        // movable costs one declined compaction, an address wrongly called
        // immovable is a frame reported clean that was never inspected. The
        // envelope is also the one thing about this arena that provably never
        // changes — it is captured once above and there is no `Arena::grow`
        // call in this file — so it cannot go stale between the root scan and
        // the pause, which a cursor-tight bound could.
        cratonvm_gc_core::jit_bounds::publish_movable_bounds(0, arena_base, arena_end);
        // The envelope's commit screen beside it. The envelope is the whole
        // RESERVATION, and only the granules the cursor reached are mapped, so
        // the frame-band verifier's object screen (`band_word_is_an_object`)
        // must be able to ask "is this word backed?" before it reads a header.
        // Without a screen it read `[w + 6]` of a stale frame word on an
        // unmapped granule: SIGSEGV in `band_has_unpublished_young_word` under
        // `System.gc()` from compiled code (`R13Callcost3GcBase`, 4-6 of 16
        // runs on the round 13 w10f binary). The edit is item 2 of
        // `docs/feature-designs/zgc-architecture.md`.
        if zgc_movable_commit_screen_enabled() {
            cratonvm_gc_core::jit_bounds::publish_movable_commit_screens([
                (arena_base, arena_end, arena.commit_bits()),
                (0, 0, None),
                (0, 0, None),
            ]);
        }
        // And the capability-free geometry table. UNCONDITIONAL, unlike the two
        // above: `heap_geometry` carries no permission of any kind, so there is
        // nothing for this collector to withhold. It is what tells a consumer
        // that wants a base and a limit -- `compressed_oops::
        // enable_for_live_heap`, a conservative scanner's range prefilter --
        // that this heap exists and where, without making it read a table whose
        // emptiness under this collector means something else entirely.
        cratonvm_gc_core::heap_geometry::publish_heap_span(0, arena_base, arena_end);
        // Publish the reference-store barrier plan. Three bytes that let
        // compiled code skip a barrier CALL exactly when this collector's own
        // helper would have returned on its first test:
        //
        //   * pre   = `mark_active`     — `satb_pre_barrier`'s first load;
        //   * post  = `has_old_objects` — `note_ref_store`'s first load;
        //   * floor = age 0             — `note_ref_store_slow`'s age test,
        //     restricted to the bound that carries no ordering obligation
        //     (see `JIT_YOUNG_FLOOR_AGE_ZERO`).
        //
        // The gate block belongs to this heap. The pre gate starts disarmed
        // because mark_active starts false; another heap's marker is irrelevant.
        // The post gate is lowered only after a generational reset, and is
        // raised before promotion publishes has_old_objects.
        //
        // zs s3/bt: it STARTS at the truth, `has_old_objects` = false, so 0
        // (`CRATONVM_ZGC_JIT_POST_GATE_TRUTH`, default ON; `=0` starts it
        // armed as before). Sound: `note_ref_store` / `note_ref_store_element`
        // return on their first load while `has_old_objects` is false, and the
        // only writer that sets it (the promoting sweep) raises this byte
        // first. With both bytes zero the optimizing tier's fused gate
        // (`CRATONVM_JIT_ZGC_FUSED_REF_STORE_GATE`) stores at once, which on
        // the default non-generational collector is every compiled reference
        // store made while no mark runs.
        let jit_ref_store_gates =
            Box::new(cratonvm_gc_core::jit_bounds::JitRefStoreGates::unpublished());
        jit_ref_store_gates.pre_active.store(0, Ordering::Release);
        jit_ref_store_gates
            .post_active
            .store(u8::from(false), Ordering::Release);
        // zt t6/tc (GN-7): on a generational heap the floor is the promotion
        // age (`zgc_jit_young_floor`), so a compiled store into ANY young
        // receiver skips the helper, not only into an age-0 one.
        jit_ref_store_gates.young_floor.store(
            zgc_jit_young_floor(zgc_generational_enabled(), zgc_gen_promotion_age()),
            Ordering::Release,
        );
        jit_ref_store_gates.published.store(1, Ordering::Release);
        // zr z3/cs: `CRATONVM_ZGC_CONC_EVERY_TRIGGER=1` configures concurrent
        // marking by itself (a non-zero `conc_start_bytes` seed) and turns the
        // rate / mark-duration measurement on; see `ZConcTriggerState`.
        let conc_every_trigger = zgc_conc_every_trigger_enabled();
        let heap = Self {
            layout_domain: std::sync::atomic::AtomicU32::new(cratonvm_types::FIRST_LAYOUT_DOMAIN),
            _bounds_registration: cratonvm_gc_core::jit_bounds::RelocatableHeapRegistration::new(),
            arena_base,
            arena_end,
            arena: Mutex::new(arena),
            jit_ref_store_gates,
            // Built from `arena_base`/`arena_end` rather than by locking the
            // arena back out of the struct literal: those are the same two
            // numbers `conservative_addr_span` and the object-start bitmap are
            // gridded on, so the survey cannot describe a different window
            // than the collector does. The base is passed VERBATIM —
            // `over_reservation` takes it unrounded on purpose, and records
            // `base_is_granule_aligned()` rather than enforcing it, so the
            // stage that needs the alignment (stage 1, where a real page must
            // start on a granule) can check it instead of assuming it.
            //
            // `ZPageConfig::default()`'s granule is 2 MiB, which IS
            // `Self::Z_LOGICAL_PAGE_BYTES`, so granule *i* and logical page
            // *i* are the same cell and `adopt_views` has something to adopt.
            // If that constant ever moves, this pairing is the thing that
            // breaks silently — `views_rejected` is the counter that says so.
            page_survey: if page::page_survey_enabled() {
                match page::ZPageAllocator::over_reservation(
                    arena_base,
                    arena_end.saturating_sub(arena_base),
                    page::ZPageConfig::default(),
                ) {
                    Ok(survey) => Some(Box::new(survey)),
                    Err(e) => {
                        // Refused, not fatal: the survey is an index, and a
                        // heap without one is the default configuration.
                        tracing::warn!(
                            target: "zgc",
                            error = %e,
                            base = arena_base,
                            len = arena_end.saturating_sub(arena_base),
                            "zgc page survey refused; continuing without it"
                        );
                        None
                    }
                }
            } else {
                None
            },
            // The bitmap is gridded over the arena envelope captured above —
            // the SAME two numbers `conservative_addr_span` answers with, and
            // sound for the same reason (this arena is created here and never
            // grown). `arena_end` is derived from the arena's post-rounding
            // capacity, so the span covers every address `Arena::alloc` can
            // ever return.
            registry: ZObjectStarts::new(arena_base, arena_end.saturating_sub(arena_base)),
            next_hash_code: AtomicI32::new(1),
            allocated: AtomicUsize::new(0),
            gc_threshold: cap * ZGC_REAL_GC_THRESHOLD_PERCENT / 100,
            alloc_trigger_bytes: AtomicUsize::new(match zgc_alloc_trigger_percent() {
                0 => 0,
                pct => (cap / 100 * pct).max(ZGC_ALLOC_TRIGGER_FLOOR),
            }),
            alloc_trigger_percent_bytes: match zgc_alloc_trigger_percent() {
                0 => 0,
                pct => (cap / 100 * pct).max(ZGC_ALLOC_TRIGGER_FLOOR),
            },
            pause_target_ms: AtomicU64::new(zgc_pause_target_ms()),
            pause_affordable_span: AtomicU64::new(0),
            pause_overrun_span: AtomicU64::new(0),
            gc_rearm: AtomicUsize::new(0),
            native_alloc_pressure: AtomicBool::new(false),
            external_gpu:
                cratonvm_gc_core::gpu_critical::ExternalGpuPressureTracker::with_heap_capacity(cap),
            hard_alloc_failure: AtomicBool::new(false),
            mark_active: AtomicBool::new(false),
            self_weak: std::sync::OnceLock::new(),
            conc_cycle_active: AtomicBool::new(false),
            conc_bits_known_clear: AtomicBool::new(true),
            conc_start_bytes: AtomicUsize::new({
                // A seed only under every-trigger: the window is not used to
                // open cycles; non-zero means "concurrent marking is
                // configured" to every reader of this field. zr z11/rv9: an
                // EXPLICIT `CRATONVM_ZGC_CONC_START=0` stays the kill switch
                // (`conc_start_seed_percent`).
                let pct = conc_start_seed_percent(
                    conc_start_percent_setting(),
                    conc_start_explicitly_zero(),
                    conc_every_trigger,
                    true,
                );
                if pct == 0 {
                    0
                } else {
                    (cap * ZGC_REAL_GC_THRESHOLD_PERCENT / 100) / 100 * pct
                }
            }),
            barrier_good_mask: AtomicU64::new(vaddr::Z_REMAPPED),
            barrier_heal_mask: AtomicU64::new(vaddr::Z_REMAPPED),
            relocate_active: AtomicBool::new(false),
            barrier_armed: AtomicBool::new(false),
            remembered: remembered::ZRememberedSetTable::new(),
            relocation_enabled: AtomicBool::new(Self::relocation_requested_by_default()),
            // NOTE: the arena's `prefer_bump` is seeded to match at the end of
            // `with_capacity` -- it cannot be set here because the arena is being
            // moved into this literal. `set_generational_enabled` keeps the two in
            // step from then on.
            generational_enabled: AtomicBool::new(zgc_generational_enabled()),
            gen_promotion_age: std::sync::atomic::AtomicU32::new(zgc_gen_promotion_age()),
            gen_minors_per_major: AtomicUsize::new(zgc_gen_minors_per_major()),
            gen_force_major_next: AtomicBool::new(false),
            gen_nursery_bytes: AtomicUsize::new(cap / 100 * zgc_gen_nursery_percent()),
            cycle_alloc_watermark: AtomicUsize::new(0),
            gen_nursery_triggered: AtomicBool::new(false),
            gen_header_zero_only: AtomicBool::new(zgc_sweep_header_zero()),
            gen_dead_runs_enabled: AtomicBool::new(zgc_sweep_dead_runs()),
            gen_young_floor: AtomicUsize::new(0),
            gen_nursery_survivors: Box::new(generation::ZNurserySurvivorCensus::new()),
            gen_old_live_bytes: AtomicUsize::new(0),
            has_old_objects: AtomicBool::new(false),
            minors_since_major: AtomicUsize::new(0),
            publish_vacated_enabled: AtomicBool::new(publish_vacated_requested_by_default()),
            high_compaction_enabled: AtomicBool::new(zgc_high_compaction_requested_by_default()),
            headroom_low: AtomicBool::new(false),
            headroom_bypasses_rearm: AtomicBool::new(zgc_headroom_bypasses_rearm()),
            conc_root_filter: AtomicBool::new(zgc_conc_root_filter()),
            last_cycle_reclaimed_nothing: AtomicBool::new(false),
            gc_count: AtomicUsize::new(0),
            gc_log_enabled: AtomicBool::new(false),
            mark_ref_skip_armed: AtomicBool::new(false),
            // NOTE (E0063 class of break): this is the ONE struct literal for
            // `ZgcRealHeap` — `new()` and `Default` both delegate here — so a
            // new field must be initialised here and nowhere else. Verified by
            // grepping `ZgcRealHeap {` across the workspace.
            tlabs: ZArenaTlabRegistry::for_capacity(cap),
            tlab_enabled: AtomicBool::new(zgc_tlab_enabled_by_default()),
            sweep_worker_count: AtomicUsize::new(Self::sweep_workers_requested()),
            relocate_worker_count: AtomicUsize::new(Self::relocate_workers_requested()),
            stamp_lo: AtomicUsize::new(usize::MAX),
            stamp_hi: AtomicUsize::new(0),
            young_pages: {
                let pages = arena_end
                    .saturating_sub(arena_base)
                    .div_ceil(Self::Z_YOUNG_GRAIN_BYTES);
                (0..pages.div_ceil(64)).map(|_| AtomicU64::new(0)).collect()
            },
            young_page_grid_overflow: AtomicBool::new(false),
            tlab_retire_incomplete: AtomicBool::new(false),
            conc_start_adaptive: AtomicBool::new(conc_start_is_adaptive() || conc_every_trigger),
            rate_sample_bytes: AtomicU64::new(0),
            rate_sample_nanos: AtomicU64::new(0),
            alloc_rate_scaled: AtomicU64::new(0),
            conc_mark_nanos_ewma: AtomicU64::new(0),
            // THE SAME TWO NUMBERS as `registry` above, through the same
            // constructor. Any drift between the two grids would make an
            // address the registry accepts unrepresentable in the mark bitmap,
            // where it would silently spill to the exact side set.
            mark_bits: {
                let span = arena_end.saturating_sub(arena_base);
                if zgc_mark_bits_enabled() && span > 0 {
                    Some(ZObjectStartBits::labelled_side_table(
                        arena_base,
                        span,
                        "mark",
                        zgc_side_table_no_thp_enabled(),
                    ))
                } else {
                    None
                }
            },
            counters: Box::new(ZgcCounters {
                jit_read_bounds_retracted: AtomicBool::new(false),
                frag_samples: AtomicUsize::new(0),
                frag_worst_permille: AtomicUsize::new(usize::MAX),
                frag_worst_free_permille: AtomicUsize::new(0),
                frag_worst_cycle: AtomicUsize::new(0),
                frag_floor_warned: AtomicBool::new(false),
                parallel_mark_fallbacks: AtomicUsize::new(0),
                parallel_mark_fallback_rescans: AtomicUsize::new(0),
                gc_stress_mark: AtomicUsize::new(0),
                critical_pins: Mutex::new(FxHashMap::default()),
                page_evac_pins: Mutex::new(FxHashSet::default()),
                page_evac_moves: Mutex::new(Vec::new()),
                corpse_ledger: Mutex::new(FxHashMap::default()),
                relocations: Mutex::new(FxHashMap::default()),
                slide_epoch: std::sync::atomic::AtomicU32::new(0),
                relocations_prune_due_at: AtomicUsize::new(0),
                relocation_prunes_run: AtomicUsize::new(0),
                relocation_prunes_skipped: AtomicUsize::new(0),
                relocation_prune_entries_walked: AtomicU64::new(0),
                slide_exit_sizes: Mutex::new(FxHashMap::default()),
                corpse_reports: AtomicUsize::new(0),
                corpse_cycle: AtomicU64::new(0),
                relocation_skipped_jit: AtomicUsize::new(0),
                relocation_on_proven_jit: AtomicUsize::new(0),
                relocation_skip_reasons: std::array::from_fn(|_| AtomicUsize::new(0)),
                relocation_coverage_reasons: std::array::from_fn(|_| AtomicUsize::new(0)),
                relocation_refusal_terms: std::array::from_fn(|_| AtomicUsize::new(0)),
                relocation_pages_deferred: AtomicUsize::new(0),
                relocation_budget_truncated_cycles: AtomicUsize::new(0),
                tlab_retire_skipped_total: AtomicUsize::new(0),

                unwalkable_reports: AtomicUsize::new(0),
                rewrite_oversize_arrays: AtomicBool::new(zgc_rewrite_oversize_arrays_enabled()),
                rewrite_oversize_arrays_walked: AtomicUsize::new(0),
                oversize_ref_arrays_omitted: AtomicUsize::new(0),
                element_card_refused_scan: AtomicBool::new(true),
                element_card_refused_scans: AtomicUsize::new(0),
                oversize_survivors_sized: AtomicUsize::new(0),
                driver_passes: AtomicUsize::new(0),
                mark_ingress: mark::ZMarkIngress::new(),
                satb_thread_bufs: mark::ZSatbThreadBuffers::new(),
                conc_pool: Mutex::new(None),
                conc_pool_idle: Mutex::new(None),
                conc_pool_retired: Mutex::new(None),
                conc_pool_reuses: AtomicUsize::new(0),
                satb_flush_races: AtomicUsize::new(0),
                gc_threads: ZGcThreadFlags::default(),
                mark_loop: ZMarkLoopOpts::from_env(),
                conc_cycles_started: AtomicUsize::new(0),
                conc_cycles_completed: AtomicUsize::new(0),
                conc_black_allocations: cratonvm_types::striped_counter::StripedCounter::new(),
                satb_filter_marked: std::sync::atomic::AtomicU8::new(zgc_satb_filter_mode()),
                satb_defer_registry: AtomicBool::new(
                    None::<String>
                        .map(|v| {
                            let v = v.trim();
                            !v.is_empty() && !cratonvm_types::flags::value_is_off(v)
                        })
                        .unwrap_or(false),
                ),
                satb_deferred_refused: AtomicUsize::new(0),
                satb_deferred_screened: AtomicUsize::new(0),
                satb_filtered: cratonvm_types::striped_counter::StripedCounter::new(),
                satb_filtered_at_start: AtomicUsize::new(0),
                satb_filtered_batched: AtomicUsize::new(0),
                rate_alloc_at_entry: AtomicUsize::new(0),
                rate_nanos_at_entry: AtomicU64::new(0),
                conc_pool_spawn_failures: AtomicUsize::new(0),
                mark_spawn_backoff_left: AtomicUsize::new(0),
                mark_spawn_backoff_skips: AtomicUsize::new(0),
                rate_exclude_pause: AtomicBool::new(true),
                rate_from_allocation: AtomicBool::new(true),
                conc_window_extrapolate: AtomicBool::new(true),
                conc_ingress_replayed: AtomicUsize::new(0),
                conc_ingress_handed: AtomicUsize::new(0),
                conc_phase_nanos: AtomicU64::new(0),
                conc_mark_started_at: AtomicU64::new(0),
                last_closed_conc_mark_nanos: AtomicU64::new(0),
                conc_scanned_concurrently: AtomicU64::new(0),
                conc_overlay_repair_rows: AtomicU64::new(0),
                conc_overlay_repair_refs: AtomicU64::new(0),
                conc_scanned_at_safepoint: AtomicU64::new(0),
                conc_window_target_nanos: AtomicU64::new(0),
                barrier_stats: barrier::ZBarrierStats::default(),
                page_ages: Mutex::new(Vec::new()),
                gen_nursery_triggers: AtomicUsize::new(0),
                gen_nursery_after_warmup: AtomicBool::new(zgc_gen_nursery_after_warmup_enabled()),
                gen_nursery_warmup_held: AtomicUsize::new(0),
                gen_nursery_warmup_held_latch: AtomicBool::new(false),
                mark_park_timeouts: AtomicUsize::new(0),
                mark_cpu_stw_pool_ns: AtomicU64::new(0),
                mark_cpu_serial_ns: AtomicU64::new(0),
                mark_cpu_stw_driver_ns: AtomicU64::new(0),
                sweep_pool_cpu_ns: AtomicU64::new(0),
                relocate_pool_cpu_ns: AtomicU64::new(0),
                uncommit_cpu_ns: AtomicU64::new(0),
                vm_pause_cpu_ns: AtomicU64::new(0),
                satb_flush_cpu_ns: AtomicU64::new(0),
                pause_thread_cpu_ns: AtomicU64::new(0),
                pause_cpu_cycles: AtomicU64::new(0),
                satb_direct_handoff: AtomicBool::new(true),
                satb_thread_buffers_on: AtomicBool::new(zgc_satb_thread_buffers_setting()),
                jit_gate_cell: std::sync::atomic::AtomicPtr::new(std::ptr::null_mut()),
                satb_direct_handed: AtomicUsize::new(0),
                full_request_fresh_mark: AtomicBool::new(true),
                full_request_conc_abandons: AtomicUsize::new(0),
                last_collection_marked_concurrently: AtomicBool::new(false),
                conc_cycle_root_filter: AtomicBool::new(false),
                mark_cpu_conc_pool_ns: AtomicU64::new(0),
                mark_cpu_conc_abandoned_ns: AtomicU64::new(0),
                gen_nursery_overshoot_max: AtomicUsize::new(0),
                alloc_trigger_fires: AtomicUsize::new(0),
                last_sweep_us: AtomicU64::new(0),
                pause_target_unreachable: AtomicUsize::new(0),
                pause_target_unreachable_streak: AtomicUsize::new(0),
                pause_target_floor_overruns: AtomicUsize::new(0),
                pause_floor_verdict_enabled: AtomicBool::new(true),
                warmup_enabled: AtomicBool::new(true),
                director: director::ZDirector::new(
                    total_bytes as u64,
                    zgc_rate_trigger_enabled(),
                    zgc_trigger_shadow_enabled(),
                ),
                near_full_progress_pct: AtomicUsize::new(zgc_near_full_progress_percent()),
                gc_rearm_legacy: AtomicUsize::new(0),
                pacing: director::ZPacingCensus::default(),
                exhausted_free_floor: AtomicUsize::new(zgc_exhausted_free_floor_bytes()),
                exhausted_verdicts_armed: AtomicUsize::new(0),
                exhausted_verdict_refusals: AtomicUsize::new(0),
                exhausted_spent: AtomicBool::new(false),
                exhausted_verdict_key: ZGC_HEAP_SERIAL.fetch_add(1, Ordering::Relaxed),
                exhausted_spent_count: AtomicUsize::new(0),
                director_rearm_floor: AtomicBool::new(true),
                trigger_director_deferred: AtomicUsize::new(0),
                warmup_hotspot_steps: AtomicBool::new(true),
                rate_excludes_pause: AtomicBool::new(true),
                soft_max_paces: AtomicBool::new(zgc_soft_max_paces_enabled()),
                soft_max_report: AtomicUsize::new(0),
                soft_max_threshold: AtomicUsize::new(usize::MAX),
                warmup_cycles_done: AtomicUsize::new(0),
                hotspot_gc_cause: AtomicBool::new(true),
                stall_cause_from_trigger: AtomicBool::new(true),
                young_pool_since_cycle: AtomicBool::new(true),
                last_cycle_trigger_mask: AtomicUsize::new(0),
                trigger_warmup: AtomicUsize::new(0),
                trigger_director: AtomicUsize::new(0),
                proactive_poll_co_claim: AtomicUsize::new(0),
                conc_trigger: ZConcTriggerState::new(conc_every_trigger),
                csweep: concurrent_sweep::ZConcSweepState::new(
                    concurrent_sweep::zgc_concurrent_sweep_enabled(),
                ),
                crround: concurrent_ref_round::ZRefRoundState::new(
                    concurrent_ref_round::zgc_concurrent_ref_round_enabled(),
                ),
                rim: remap_in_mark::ZRemapInMarkState::new(
                    remap_in_mark::zgc_remap_in_mark_enabled(),
                ),
                rim_last_mark_color: AtomicU64::new(0),
                rim_cycle_young: AtomicBool::new(false),
                rim_keep_until_major: AtomicBool::new(false),
                crl: concurrent_relocate::ZConcRelocState::new(
                    concurrent_relocate::zgc_concurrent_relocate_enabled(),
                    arena_base,
                    arena_end.saturating_sub(arena_base),
                ),
                crefs: concurrent_refs::ZConcRefsState::new(
                    concurrent_refs::zgc_concurrent_refs_enabled(),
                ),
                mend: mark_end::ZMarkEndState::new(mark_end::zgc_mark_end_service_enabled()),
                rstart: rate_start::ZRateStartState::new(
                    rate_start::zgc_concurrent_rate_start_enabled(),
                ),
                skipcls: skip_class_filter::ZSkipClassFilter::new(
                    skip_class_filter::zgc_mark_skip_class_filter_enabled(),
                ),
                skipbits: std::sync::Arc::new(skip_bits::ZSkipBits::new(
                    skip_bits::zgc_mark_skip_bits_enabled(),
                )),
                weak_phantom_class_kind: (0..65536).map(|_| std::sync::atomic::AtomicU8::new(0)).collect::<Vec<_>>().into_boxed_slice(),
                weak_phantom_class_overflow: parking_lot::RwLock::new(FxHashMap::default()),
                discovered_weak_phantom: Box::new(std::array::from_fn(|_| Mutex::new(Vec::new()))),
                finalized_weak_phantom: Mutex::new(Vec::new()),
                gen_zero_bytes_skipped: AtomicUsize::new(0),
                sweep_dead_headers_skipped: AtomicUsize::new(0),
                sweep_prune_key_cycles: AtomicUsize::new(0),
                sweep_prune_keys_probed: AtomicUsize::new(0),
                sweep_prune_keys_dead: AtomicUsize::new(0),
                sweep_region_words_skipped: AtomicUsize::new(0),
                mark_summary_blocks_forgotten: AtomicUsize::new(0),
                gen_dead_runs: AtomicUsize::new(0),
                gen_dead_objects: AtomicUsize::new(0),
                gen_promotions_by_slide: AtomicUsize::new(0),
                gen_sweep_skipped: AtomicUsize::new(0),
                young_cycles: AtomicUsize::new(0),
                gen_old_retained: AtomicUsize::new(0),
                gen_remembered_roots: AtomicUsize::new(0),
                gen_promotions: AtomicUsize::new(0),
                gen_promoted_bytes: AtomicUsize::new(0),
                gen_recards_after_relocation: AtomicUsize::new(0),
                gen_old_bits: std::sync::OnceLock::new(),
                gen_old_bits_on: AtomicBool::new(zgc_gen_old_bitmap_enabled()),
                gen_bitmap_fallback_major: AtomicBool::new(zgc_gen_bitmap_fallback_major_enabled()),
                gen_old_bits_young_refusals: AtomicUsize::new(0),
                // A fresh heap holds no object, so an empty bitmap is exact.
                gen_old_bits_valid: AtomicBool::new(true),
                gen_old_bits_rebuilding: AtomicBool::new(false),
                gen_old_bits_premarks: AtomicUsize::new(0),
                gen_old_bits_fallbacks: AtomicUsize::new(0),
                gen_old_bits_verify_unsound: AtomicUsize::new(0),
                gen_old_bits_verify_imprecise: AtomicUsize::new(0),
                gen_young_aged_sweep_on: AtomicBool::new(zgc_gen_young_aged_sweep_enabled()),
                gen_young_aged_sweeps: AtomicUsize::new(0),
                gen_below_floor_visited: AtomicUsize::new(0),
                gen_below_floor_freed_bytes: AtomicUsize::new(0),
                gen_nursery_old_bytes: AtomicUsize::new(0),
                gen_nursery_survivors_young: Box::new(generation::ZNurserySurvivorCensus::new()),
                gen_census_age_on: AtomicBool::new(zgc_nursery_census_age_enabled()),
                gen_census_on: AtomicBool::new(zgc_nursery_census_enabled()),
                gen_element_cards_on: AtomicBool::new(zgc_gen_element_cards_enabled()),
                gen_element_granules_scanned: AtomicUsize::new(0),
                gen_element_cards_converted: AtomicUsize::new(0),
                gen_young_relocate_on: AtomicBool::new(zgc_gen_young_relocate()),
                young_evac: young_evacuate::ZYoungEvacCounters::new(zgc_gen_young_evacuate()),
                gen_young_complement_on: AtomicBool::new(zgc_gen_young_complement_enabled()),
                gen_major_promoted_percent: AtomicUsize::new(zgc_gen_major_promoted_percent()),
                gen_young_complement_sweeps: AtomicUsize::new(0),
                gen_young_complement_refusals: AtomicUsize::new(0),
                gen_young_complement_refusal: AtomicUsize::new(0),
                gen_young_sweep_workers: AtomicUsize::new(
                    ZgcRealHeap::gen_young_sweep_workers_requested(),
                ),
                gen_young_sweep_sharded: AtomicUsize::new(0),
                gen_young_sweep_shards: AtomicUsize::new(0),
                gen_tenure_all_cycle: AtomicBool::new(false),
                gen_tenure_all_sweeps: AtomicUsize::new(0),
                gen_weak_minors: AtomicUsize::new(0),
                gen_weak_minor_run: AtomicUsize::new(0),
                gen_weak_minor_max_run: AtomicUsize::new(0),
                gen_weak_minor_escalations: AtomicUsize::new(0),
                gen_escalate_weak_minor: AtomicBool::new(zgc_flag_default(
                    "CRATONVM_ZGC_GEN_ESCALATE_WEAK_MINOR",
                    false,
                )),
                soft_max_origin: std::sync::atomic::AtomicU8::new(0),
                timer: {
                    let t = timer::ZTimer::new();
                    t.set_idle_proactive(true);
                    t
                },
                gen_promoted_since_major: AtomicUsize::new(0),
                gen_majors_by_old_growth: AtomicUsize::new(0),
                gen_promoted_at_major: AtomicUsize::new(0),
                gen_empty_minor_needs_promotion: AtomicBool::new(
                    zgc_gen_empty_minor_needs_promotion_enabled(),
                ),
                gen_empty_minor_escalations_skipped: AtomicUsize::new(0),
                gen_last_cycle_young_aged: AtomicBool::new(false),
                gen_last_major_young_bytes: AtomicUsize::new(0),
                gen_last_major_alloc_stalled: AtomicBool::new(false),
                gen_last_young_promoted_bytes: AtomicUsize::new(0),
                gen_last_young_below_young_bytes: AtomicUsize::new(0),
                gen_last_young_xlog_exact: AtomicBool::new(false),
                gen_full_request_major: AtomicBool::new(zgc_gen_full_request_major_enabled()),
                gen_majors_by_request: AtomicUsize::new(0),
                gen_major_rule_causes: AtomicBool::new(zgc_gen_major_rule_causes_enabled()),
                gen_majors_by_rule_cause: AtomicUsize::new(0),
                gen_young_skip_refs_on: AtomicBool::new(zgc_gen_young_skip_references()),
                gen_young_predict_exact: AtomicBool::new(zgc_gen_young_predict_exact()),
                gen_young_refs_strong_armed: AtomicBool::new(false),
                gen_young_refs_strong_cycles: AtomicUsize::new(0),
                gen_young_refs_mispredicted: AtomicUsize::new(0),
                gen_young_fin_deferred: AtomicUsize::new(0),
                gen_armed_major_soft_clock_due: AtomicBool::new(false),
                gen_cost: ZGenCost::default(),
                parallel_mark_cycles: AtomicUsize::new(0),
                compaction_cycles: AtomicUsize::new(0),
                trigger_stress: AtomicUsize::new(0),
                trigger_threshold: AtomicUsize::new(0),
                trigger_headroom: AtomicUsize::new(0),
                trigger_alloc_budget: AtomicUsize::new(0),
                trigger_headroom_starved: AtomicUsize::new(0),
                trigger_headroom_starved_gap: AtomicUsize::new(0),
                trigger_headroom_bypass: AtomicUsize::new(0),
                trigger_headroom_bypass_suppressed: AtomicUsize::new(0),
                remset_pages_without_base: AtomicUsize::new(0),
                trigger_seen: AtomicUsize::new(0),
                trigger_hard_alloc_fail: AtomicUsize::new(0),
                objects_relocated: AtomicUsize::new(0),
                vacated_spans_published: AtomicUsize::new(0),
                vacated_bytes_published: AtomicUsize::new(0),
                high_compaction_cycles: AtomicUsize::new(0),
                high_compaction_declined: AtomicUsize::new(0),
                relocation_cost_gated: AtomicUsize::new(0),
                slide_pins_dropped_by_base: AtomicUsize::new(0),
                slide_targets_pin_dropped: AtomicUsize::new(0),
                slide_evacuate: std::array::from_fn(|_| AtomicUsize::new(0)),
                evac_quarantine: Mutex::new(Vec::new()),
                evac_quarantine_stats: std::array::from_fn(|_| AtomicUsize::new(0)),
                evac_quarantine_dead_check: std::array::from_fn(|_| AtomicUsize::new(0)),
                selective_gate_on_selection: std::array::from_fn(|_| AtomicUsize::new(0)),
                reloc_par_passes: std::array::from_fn(|_| AtomicUsize::new(0)),
                reloc_pool: std::sync::OnceLock::new(),
                region_reloc: region_forwarding::ZRegionRelocCounters::new(),
                pressure_select: std::array::from_fn(|_| AtomicUsize::new(0)),
                selective_relocate: std::array::from_fn(|_| AtomicUsize::new(0)),
                slide_moved_bytes: std::array::from_fn(|_| AtomicUsize::new(0)),
                stall_cycle_pending: AtomicBool::new(false),
                conc_mark_start_used: AtomicUsize::new(usize::MAX),
                last_mark_start_used: AtomicUsize::new(usize::MAX),
                cycle_after_alloc_failure: AtomicBool::new(false),
                exhaustion_unwind: std::sync::OnceLock::new(),
                exhaustion_unwind_offers: AtomicUsize::new(0),
                last_refusal_size: AtomicUsize::new(0),
                last_refusal_epoch: AtomicUsize::new(usize::MAX),
                verdict_records_refusal: AtomicBool::new(true),
                sub_request_floor: zgc_retire_sub_request_spans_floor(),
                sub_request_armed_available: AtomicUsize::new(0),
                sub_request_armed_used: AtomicUsize::new(0),
                sub_request_arms: AtomicUsize::new(0),
                sub_request_disarms: AtomicUsize::new(0),
                local: heap_state::ZHeapLocalState::from_env(),
                rewrite_primitive_cells_skipped: AtomicUsize::new(0),
                relocate_size_table_skips: AtomicUsize::new(0),
                sweep_size_hint_hits: AtomicUsize::new(0),
                sweep_live_hints_last: AtomicUsize::new(0),
                slide_no_move_declined: AtomicUsize::new(0),
                slide_no_move_missed: AtomicUsize::new(0),
                // Off in this crate's unit tests unless a test asks for it
                // (`set_carve_futile_latch`): many of them free arena blocks by
                // hand (`add_free_block`) between two carves, outside the paths
                // the latch watches (`ZCarveFutileLatch`), and would then see
                // up to `ZGC_CARVE_FUTILE_SKIPS` latched answers.
                carve_futile_on: AtomicBool::new(!cfg!(test)),
                carve_futile: ZCarveFutileLatch::default(),
                carve_space_gen: AtomicUsize::new(0),
                carve_futile_census: std::array::from_fn(|_| AtomicUsize::new(0)),
                size_memo: Mutex::new(None),
                size_memo_census: std::array::from_fn(|_| AtomicUsize::new(0)),
                relocation_targeted_overrun_bytes: AtomicUsize::new(0),
                high_objects_relocated: AtomicUsize::new(0),
                high_bytes_copied: AtomicUsize::new(0),
                tlab_refill_recycled: AtomicUsize::new(0),
                tlab_refill_starved: AtomicUsize::new(0),
                tlab_refill_starved_bytes: AtomicUsize::new(0),
                tlab_pristine_bytes_skipped: AtomicUsize::new(0),
                tlab_slide_zero_bytes_skipped: AtomicUsize::new(0),
                chunk_zero_descending: AtomicUsize::new(zgc_chunk_zero_descending_block()),
                chunk_zero_descending_chunks: AtomicUsize::new(0),
                ref_processor: Mutex::new(ReferenceProcessor::new()),
                pending_finalizer_roots: Mutex::new(Vec::new()),
                resurrected_finalizers: Mutex::new(Vec::new()),
                resurrection_closure: Mutex::new(Vec::new()),
                soft_precise_probe: Mutex::new(None),
                soft_precise_kept: Mutex::new(None),
                soft_precise_probe_cycles: AtomicUsize::new(0),
                soft_precise_deferred_roots: AtomicUsize::new(0),
                slot_census: {
                    // The census is a diagnostic with a real per-collection
                    // cost, so it stays off by default -- but "off by default"
                    // and "unreachable" are different things, and until now it
                    // was the second: `ZSlotCensus::enable` had no caller
                    // outside census.rs's own tests, so `run_walk` returned
                    // `None` before touching the heap for the life of every
                    // process. `enable_from_env` reads CRATONVM_ZGC_CENSUS /
                    // CRATONVM_ZGC_CENSUS_ACCESS once, here; disabled, the
                    // sweep-time gate is one relaxed load and a not-taken
                    // branch, exactly as before.
                    let census = census::ZSlotCensus::new();
                    census.enable_from_env();
                    census
                },
                // `phases_run_concurrently` stays at its `false` default, which
                // is the truth for this collector: `collect_garbage` holds one
                // `StopTheWorldToken` across mark, reference processing, sweep
                // and the slide. Flipping it would print the small-STW /
                // large-concurrent picture of a healthy low-latency collector
                // for a collector that stops the world for the whole cycle --
                // see `ZgcMetrics::set_phases_run_concurrently`. The concurrent
                // mark phase is charged honestly below, by recording it under
                // `ConcurrentMark` only when it actually ran off the safepoint.
                metrics: metrics::ZgcMetrics::new(),
                mark_ref_skip: parking_lot::RwLock::new(None),
                mark_ref_skip_bloom: Box::new(std::array::from_fn(|_| AtomicU64::new(0))),
                vm_conc_ref_skip: Mutex::new(Vec::new()),
                vm_conc_soft_skip: Mutex::new(Vec::new()),
                mark_root_filter: mark_roots::ZMarkRootFilter::default(),
                mark_ref_skip_warned: AtomicBool::new(false),
                dead_scratch: Mutex::new(Vec::new()),
                fp: Box::new(ZFootprintCounters::from_env_with_initial(
                    initial_commit_bytes,
                )),
                forwarding_words_stamped: AtomicUsize::new(0),
                forwarding_words_read: AtomicUsize::new(0),
                bytes_uncommitted: AtomicUsize::new(0),
                jfr: jfr_facts::ZJfrFacts::default(),
                conc_black_claims: AtomicUsize::new(0),
                tlab_retire_skipped_at_safepoint: AtomicUsize::new(0),
                relocation_on_page_pins: AtomicUsize::new(0),
                slide_verifications: AtomicUsize::new(0),
                slide_verified_survivors: AtomicUsize::new(0),
                slide_verify_missed_rewrites: AtomicUsize::new(0),
                slide_verify_unregistered: AtomicUsize::new(0),
                mark_pool: Mutex::new(None),
                #[cfg(test)]
                test_worker_scan_panic_at: AtomicUsize::new(0),
                #[cfg(test)]
                test_driver_safepoint_panic: AtomicBool::new(false),
                vm_tlab_enabled: AtomicBool::new(zgc_vm_tlab_enabled_by_default()),
                jit_tlab_skip: Mutex::new(Vec::new()),
                recharged_tails: Mutex::new((0, Vec::new())),
                published_tail_recharge_bytes: AtomicUsize::new(0),
                vm_tlab_refills: AtomicUsize::new(0),
                vm_tlab_refill_bytes: AtomicUsize::new(0),
                vm_tlab_tails_returned: AtomicUsize::new(0),
                vm_tlab_tail_bytes_returned: AtomicUsize::new(0),
                post_refusal_limit: zgc_dbg_post_refusal_limit(),
                post_refusal_budget: AtomicUsize::new(0),
                raw_zero_outside_lock: AtomicBool::new(zgc_raw_zero_outside_lock_enabled()),
                sweep_sizes_oversize_arrays: AtomicBool::new(
                    sweep::zgc_sweep_sizes_oversize_arrays_enabled(),
                ),
                oversize_arrays_swept: AtomicUsize::new(0),
                oversize_array_bytes_swept: AtomicUsize::new(0),
                oversize_arrays_refused: AtomicUsize::new(0),
                sweep_one_frees_oversize: AtomicBool::new(
                    sweep::zgc_sweep_one_frees_oversize_enabled(),
                ),
                sweep_one_oversize_freed: AtomicUsize::new(0),
                sweep_spill_high_spans: AtomicBool::new(sweep::zgc_sweep_spill_high_spans_enabled()),
                oversize_screen_memo_on: AtomicBool::new(zgc_oversize_screen_memo_enabled()),
                oversize_screen_memo_armed: AtomicBool::new(false),
                oversize_screen_memo: Mutex::new(Vec::new()),
                oversize_screen_memo_hits: AtomicUsize::new(0),
                oversize_screen_scans: AtomicUsize::new(0),
                forget_block_passes: AtomicUsize::new(0),
                forget_block_declined: AtomicUsize::new(0),
                forget_blocks_total: AtomicUsize::new(0),
                inflight_covers_lowered: AtomicUsize::new(0),
                inflight_cover_bytes: AtomicUsize::new(0),
                huge_bases: Box::new(std::array::from_fn(|_| AtomicUsize::new(0))),
                huge_hwm: AtomicUsize::new(0),
                huge_overflow: AtomicBool::new(false),
                huge_extent_hits: AtomicUsize::new(0),
                huge_extent_reseeds: AtomicUsize::new(0),
                lazy_starts: starts_lazy::ZLazyTlabStarts::new(
                    starts_lazy::zgc_lazy_tlab_starts_mode(),
                ),
                lb_plain_stores: AtomicBool::new(false),
                lb_mark_on_load: AtomicBool::new(false),
                lb_routed: AtomicUsize::new(0),
                lb_plain: AtomicUsize::new(0),
                lb_colored: AtomicUsize::new(0),
                lb_marked: AtomicUsize::new(0),
                lb_unrouted: AtomicUsize::new(0),
                lb_colored_stores: AtomicBool::new(false),
                lb_colored_refused: AtomicBool::new(false),
                lb_store_colored: AtomicUsize::new(0),
                lb_jit_slot: AtomicUsize::new(0),
                lb_gc_colored: AtomicUsize::new(0),
                lb_color_flip: AtomicBool::new(false),
                lb_flip_mask: vaddr::ZGoodMask::new(),
                lb_bad_cell: Box::new(vaddr::ZCompiledBadMask::new()),
                lb_flips_mark: AtomicUsize::new(0),
                lb_flips_remap: AtomicUsize::new(0),
                lbm: AtomicBool::new(false),
                lbm_colored_skipped: AtomicUsize::new(0),
                lbm_plain_logged: AtomicUsize::new(0),
                lbm_no_load_mark: AtomicBool::new(false),
                lb_plain_in_mark: AtomicUsize::new(0),
                lb_referent_keep_alive: AtomicUsize::new(0),
            }),
        };
        if heap.arena_end > heap.arena_base {
            heap.counters
                .skipbits
                .init_persistent(ZObjectStartBits::labelled_side_table(
                    heap.arena_base,
                    heap.arena_end - heap.arena_base,
                    "skip-persistent",
                    zgc_side_table_no_thp_enabled(),
                ));
            heap.counters
                .ref_processor
                .lock()
                .attach_persistent_skip_bits(std::sync::Arc::clone(&heap.counters.skipbits));
        }
        // G2c: seed the arena's allocation policy to match the mode the flag just
        // chose. `set_generational_enabled` keeps them in step afterwards; doing
        // it here rather than inside the literal is only because the arena is
        // being moved into it.
        if heap.generational_enabled.load(Ordering::Relaxed) {
            heap.arena.lock().set_prefer_bump(true);
        }
        // zr z3/cs: the director's rate reserve keeps HotSpot's cycle-duration
        // term whenever a cycle can have a concurrent phase.
        heap.counters
            .director
            .set_cycle_shape(heap.conc_start_bytes.load(Ordering::Relaxed) != 0);
        // zr zs s1/tp: the stop-the-world director model (opt-in); it stands
        // down under concurrent marking.
        heap.counters.director.set_stw_model(true);
        heap.counters
            .director
            .set_byte_polled_interval(zgc_director_byte_polled_interval_enabled());
        // zt t1/tp: HotSpot's warmth (warm-up cycles, not cycles).
        heap.counters.director.set_hotspot_warmth(true);
        // zt t14/tg6: the proactive rule reads the major clock.
        heap.counters.director.set_proactive_major_clock(true);
        heap.counters
            .fp
            .arena_no_thp
            .store(arena_no_thp, Ordering::Relaxed);
        *heap.counters.fp.code_heap_hold.lock() = code_heap_hold;
        heap.counters
            .fp
            .hugetlb_uncommit_unsupported
            .store(uncommit_unsupported, Ordering::Relaxed);
        if let Some((refusals, short)) = explicit_initial {
            heap.counters
                .fp
                .hugetlb_backed
                .store(true, Ordering::Release);
            heap.counters
                .fp
                .hugetlb_initial_refusals
                .store(refusals, Ordering::Relaxed);
            heap.counters
                .fp
                .hugetlb_initial_short
                .store(short, Ordering::Relaxed);
        }
        heap
    }

    // =====================================================================
    // GENUINE CONCURRENT MARKING (2026-08-16)
    // =====================================================================
    //
    // `docs/feature-designs/zgc-architecture.md`
    // states the property this section implements and the two items it was
    // owed: C1 (a real mark safepoint) and C2 (an owner for the coordinator).
    // What landed differs from that plan's *shape* in one deliberate way, and
    // the reason is architectural rather than a shortcut:
    //
    //   The plan expected `ZgcConcurrentMarkController`'s driver thread to
    //   take the mark-end safepoint itself, through the `ZgcMarkSafepoint`
    //   seam. **No background thread in this VM can take a safepoint.**
    //   `GcBarrier::request_stw_counted_with_live_blocked` is keyed on a
    //   registered `ThreadId`, `stw_take_over_and_wait` forcibly stops in-JIT
    //   peers using the initiator's own context, and `StopTheWorldToken` is
    //   `!Send` on purpose. G1 hit the same wall and answered it the same way:
    //   `g1_concurrent_mark_cycle` opens the cycle on a MUTATOR at a brief STW
    //   and `g1_final_remark_cleanup` closes it on a MUTATOR at the next one,
    //   with the background worker doing the tracing in between. This follows
    //   that, so ZGC and G1 have one safepoint story in this VM instead of two.
    //
    // The phases, and who runs each:
    //
    //   mark start   MUTATOR, brief STW   `start_concurrent_mark`
    //                                       clear mark bits, snapshot the
    //                                       reference skip set, push roots,
    //                                       arm the SATB barrier, arm the pool
    //   concurrent   MARK WORKERS         mutators run; the pool traces the
    //                                       transitive closure of the mark-start
    //                                       root set
    //   mark end     MUTATOR, the GC STW  `finish_concurrent_mark`
    //                                       replay the ingress, re-scan roots,
    //                                       drive to a certified fixed point
    //   sweep        MUTATOR, same STW    unchanged `collect_garbage` tail
    //
    // ## The discipline that makes it sound
    //
    // Marking is **snapshot-at-the-beginning**, because that is the barrier
    // this VM already has on every reference store (`VmHeap::satb_barrier`).
    // SATB needs exactly two things beyond the barrier itself, and both are
    // here:
    //
    //  1. **Objects allocated during the cycle are implicitly live.** They are
    //     born with `GC_FLAG_MARKED` already set -- see
    //     [`Self::allocate_black_if_marking`]. This is not an optimisation:
    //     without it the sweep frees every object the mutators allocated while
    //     the marker ran, because none of them is reachable from the
    //     mark-start root set. Allocating black also means the marker never
    //     scans an object whose header a mutator may still be writing.
    //  2. **The roots are re-scanned at mark end.** A thread created during
    //     the concurrent phase has a stack the mark-start scan never saw, and
    //     a reference parked only in a register or a native frame is reachable
    //     from no heap field the pre-write barrier could have caught. This is
    //     the same re-scan `g1_final_remark_and_cleanup` performs, for the
    //     same reason.
    //
    // SATB is *conservative*: an object that dies during the cycle is
    // reclaimed by the next one. That is floating garbage -- a throughput cost
    // -- not a correctness one.

    /// Build a heap that can mark concurrently.
    ///
    /// [`Self::with_capacity`] still exists and still works; a heap built with
    /// it simply never opens a concurrent cycle, because
    /// [`Self::start_concurrent_mark`] cannot mint the `Arc` the marking
    /// engine needs. Every production heap comes through here (`VmHeap::new`).
    pub fn new_shared(total_bytes: usize) -> std::sync::Arc<Self> {
        Self::new_shared_with_initial(total_bytes, 0)
    }

    /// [`Self::new_shared`] plus `-Xms` — see
    /// [`Self::with_capacity_and_initial`] for why the value can only commit a
    /// prefix on this collector. `VmHeap::new_with_heap_sizing`'s
    /// `GcBackend::Zgc` arm is the production caller.
    pub fn new_shared_with_initial(
        total_bytes: usize,
        initial_bytes: usize,
    ) -> std::sync::Arc<Self> {
        Self::new_shared_with_initial_and_large_pages(total_bytes, initial_bytes, None)
    }

    /// zki: [`Self::new_shared_with_initial`] on explicit hugetlbfs pages when
    /// `explicit_large_pages` is `Some` ([`Self::with_capacity_initial_and_large_pages`]).
    pub fn new_shared_with_initial_and_large_pages(
        total_bytes: usize,
        initial_bytes: usize,
        explicit_large_pages: Option<ZgcExplicitPages>,
    ) -> std::sync::Arc<Self> {
        let heap = std::sync::Arc::new(Self::with_capacity_initial_and_large_pages(
            total_bytes,
            initial_bytes,
            explicit_large_pages,
        ));
        // Ignore the `Err`: `OnceLock::set` can only fail if this ran twice on
        // one heap, which this constructor makes impossible.
        let _ = heap.self_weak.set(std::sync::Arc::downgrade(&heap));
        // zt t20/x16 (page item 23e): counted live here, uncounted in `Drop`
        // (the `self_weak` set above marks the heaps counted). zt t21/x17
        // (item 27f): right after the set, so no panic can land between the
        // mark `Drop` reads and the count it undoes.
        let live = ZGC_SHARED_HEAPS_LIVE.fetch_add(1, Ordering::AcqRel) + 1;
        // The VM thread's `Tlab::retire` hands its unused tail to whichever
        // sink owns the address; this heap is that sink for its own arena.
        // See `zgc/vm_tlab.rs`. A `Weak`, so a dropped heap stops answering.
        let weak: std::sync::Weak<Self> = std::sync::Arc::downgrade(&heap);
        let sink: std::sync::Weak<dyn cratonvm_gc_core::tlab::TlabTailSink> = weak;
        cratonvm_gc_core::tlab::register_tlab_tail_sink(sink);
        // zr z8/st: ask `Tlab::retire` for the consumed span too, only when
        // `CRATONVM_ZGC_LAZY_TLAB_STARTS` is on (one relaxed load per retire
        // otherwise, for every backend).
        heap.lazy_register_retired_span_listener();
        // This collector finds objects through its allocation-base registry,
        // never by walking a TLAB chunk, so the JIT's inline allocator must
        // keep calling the post-init helper that announces each one. Set
        // before any method can be compiled: heap construction precedes the
        // first compile. See `cratonvm_types::jit_tlab_registration_required`.
        //
        // PUBLISH `true`, NEVER `false` (2026-09-06). This was
        // `set_jit_tlab_registration_required(heap.vm_tlab_enabled())`, and the
        // `false` half of that is dead in the ordinary case and harmful in the
        // rest: the flag starts `false`, so a heap without VM TLABs publishing
        // `false` changes nothing -- while a SECOND heap built later publishes
        // over the FIRST one's `true` and switches announcing off for a heap
        // that is still handing out TLABs. `libcratonvm` / `cratonvm-embed`
        // allow more than one VM per process, which is where that ordering is
        // reachable, and the failure it produces is the one this flag exists to
        // prevent: an inline-allocated object the registry was never told
        // about, decoding as `null` at the next native boundary.
        //
        // The flag is fail-safe upward -- a spurious `true` costs one helper
        // call per inline allocation, a wrong `false` costs an object -- so a
        // latch is the direction to be wrong in.
        //
        // Its shadow was a test flake: `a_shared_heap_tells_the_jit_that_tlab_
        // objects_must_be_announced` sets the flag `false`, builds a
        // TLAB-enabled heap and asserts the flag came back `true`, and any
        // concurrently-running test constructing an ordinary shared heap
        // published `false` into that window. Measured at 2 failures in 500
        // runs of `zgc::vm_tlab` alone at `--test-threads=8`. (zt t12/rt: the
        // kill switch that restored the old unconditional publish,
        // `CRATONVM_ZGC_TLAB_FLAG_PUBLISH_FALSE`, was retired.)
        if heap.vm_tlab_enabled() {
            cratonvm_gc_core::tlab::register_owned_vm_tlab_chunk_epoch_hook(
                super::vm_tlab::owned_vm_tlab_chunk_epoch,
            );
            cratonvm_types::set_jit_tlab_registration_required(true);
            cratonvm_gc_core::tlab::register_owned_vm_tlab_chunk_epoch_hook(
                crate::zgc::owned_vm_tlab_chunk_epoch,
            );
            // The JIT's inline start-bit store (`CRATONVM_ZGC_JIT_INLINE_
            // ANNOUNCE`, default off): only a heap that hands out VM TLABs has
            // chunks it could apply to. See `cratonvm_gc_core::tlab::JitZgcAnnounceTable`.
            let _ = heap.publish_jit_inline_announce();
        }
        // zt t18/zb1 (zr1br A2a/A3): `CRATONVM_ZGC_LOAD_BARRIER` (opt-in,
        // default OFF) arms the read barrier in plain-store mode. Here, after
        // the JIT read bounds were published (`with_capacity_and_initial`) so
        // the arming clears them, and before any thread or compiled method
        // exists -- the safepoint obligation `set_barrier_color` states.
        let armed = ZGC_LOAD_BARRIER_ARMED_BASES
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .len();
        let explicit_barrier = None::<std::ffi::OsString>.is_some();
        match zgc_load_barrier_mode() {
            ZLoadBarrierMode::Off => {
                if armed > 0 {
                    eprintln!(
                        "[cratonvm] warning: CRATONVM_ZGC_LOAD_BARRIER armed another ZGC heap in \
                         this process; its process-wide gate also routes this heap's compiled reads"
                    );
                }
            }
            // Silent when the mode came from the concurrent-relocation
            // profile rather than from the operator: a second VM in a process
            // simply runs without the barrier (and without concurrent relocation).
            _ if zgc_load_barrier_arming_refused(live, armed) => {
                if explicit_barrier {
                    eprintln!(
                        "[cratonvm] warning: CRATONVM_ZGC_LOAD_BARRIER ignored: {live} ZGC heaps \
                         are live in this process (the arming is process-wide; it needs a single VM)"
                    );
                }
            }
            // zt t20/zb3: armed (opt-in) -- see `ZLoadBarrierMode::Colored`.
            // zt t19/zb2's refusal stays for the cases the arming declines.
            // Never a silent fallback: the operator sees which mode they got.
            ZLoadBarrierMode::Colored => {
                if heap.arm_load_barrier_colored_stores() {
                    ZGC_LOAD_BARRIER_ARMED_BASES
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .push(heap.arena_base);
                    // zt t22/zb5 (stage 2e): `CRATONVM_ZGC_COLOR_FLIP` (opt-in)
                    // arms the color flip on top -- before the first compile,
                    // so every compiled element load sees the cell.
                    if zgc_color_flip_requested()
                        && !heap.arm_load_barrier_color_flip()
                        && None::<std::ffi::OsString>.is_some()
                    {
                        eprintln!("{}", ZGC_COLOR_FLIP_REFUSED);
                    }
                } else if !explicit_barrier {
                    // The profile's default cannot arm here (narrow oops, a heap
                    // wider than 4 TiB): no barrier, no plain-store fallback.
                    heap.counters
                        .lb_colored_refused
                        .store(true, Ordering::Relaxed);
                } else {
                    heap.counters
                        .lb_colored_refused
                        .store(true, Ordering::Relaxed);
                    eprintln!("{}", ZGC_COLORED_STORES_REFUSED);
                    if heap.arm_load_barrier_plain_stores(false) {
                        ZGC_LOAD_BARRIER_ARMED_BASES
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .push(heap.arena_base);
                    }
                }
            }
            mode => {
                if heap.arm_load_barrier_plain_stores(mode == ZLoadBarrierMode::Mark) {
                    ZGC_LOAD_BARRIER_ARMED_BASES
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .push(heap.arena_base);
                }
            }
        }
        heap
    }

    /// This heap's own `Arc`, typed as the mark context — `None` when the heap
    /// was not built by [`Self::new_shared`], or when the kill switch is set.
    /// See [`zgc_mark_ctx_direct`].
    pub fn mark_context_arc(&self) -> Option<std::sync::Arc<dyn mark::ZMarkContext>> {
        if !self.counters.local.mark_ctx_direct() {
            return None;
        }
        let arc = self.self_weak.get()?.upgrade()?;
        Some(arc as std::sync::Arc<dyn mark::ZMarkContext>)
    }

    /// How many mark workers a CONCURRENT cycle should use.
    ///
    /// Deliberately a different knob from [`Self::parallel_mark_workers`],
    /// which sizes a stop-the-world pool. The trade-off is the opposite one:
    /// a stop-the-world pool wants every core because nothing else is running,
    /// while a concurrent pool is stealing cycles from the mutators it is
    /// running beside. HotSpot ZGC sizes `ConcGCThreads` at 25 % of the CPUs
    /// (12.5 % with `-XX:-UseDynamicNumberOfGCThreads`) against 60 % for
    /// `ParallelGCThreads` for exactly this reason. (This doc said "roughly an
    /// eighth" until zr z3/mt; HotSpot 25's own ergonomic is the ratio above.)
    ///
    /// In order (zr z3/mt):
    /// 1. `-XX:ConcGCThreads=<n>` given to this heap
    ///    ([`Self::set_gc_worker_threads`]): `n`, clamped to
    ///    `1..=Z_PARMARK_MAX_WORKERS`. An explicit flag wins over the A/B
    ///    switch below, the precedent `-XX:MaxGCPauseMillis` set.
    /// 2. `CRATONVM_ZGC_CONC_WORKERS=<n>`: `n`, clamped to `1..=32`.
    /// 3. HotSpot ZGC's ergonomic, [`zgc_nconcurrent_workers`] (CPU share,
    ///    capped at 2 % of the heap in 2 MiB pages).
    ///
    /// `CRATONVM_ZGC_GC_THREAD_FLAGS=0` skips (1) and restores the old default
    /// for (3): a quarter of the cores, floor 1, cap 4.
    ///
    /// Read per call, not latched: this runs once per concurrent mark start,
    /// and the old process-wide `OnceLock` made the first heap's answer every
    /// later heap's (a process global for a per-VM figure).
    pub fn conc_mark_workers(&self) -> usize {
        if let Some(n) = self.counters.gc_threads.conc() {
            return n.clamp(1, Z_PARMARK_MAX_WORKERS);
        }
        if let Some(n) = None::<std::ffi::OsString>
            .and_then(|v| v.to_str().and_then(|s| s.trim().parse::<usize>().ok()))
        {
            return n.clamp(1, 32);
        }
        let cores = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(2);
        if !zgc_gc_thread_flags_enabled() {
            return (cores / 4).clamp(1, 4);
        }
        let heap_bytes = self.arena_end.saturating_sub(self.arena_base);
        zgc_nconcurrent_workers(cores, heap_bytes, self.counters.gc_threads.dynamic())
    }

    /// Apply `-XX:ParallelGCThreads`, `-XX:ConcGCThreads` and
    /// `-XX:±UseDynamicNumberOfGCThreads` to THIS heap (zr z3/mt). `None` =
    /// the flag was not given. Called once by the VM after construction
    /// (`vm_init`), the shape `set_pause_target_ms` and
    /// `set_uncommit_options` have; a `0` is refused by the launcher, and is
    /// treated as "not given" here.
    ///
    /// # What each flag sizes
    ///
    /// * `ParallelGCThreads` sizes every STOP-THE-WORLD pool that is ON: the
    ///   parallel mark ([`Self::parallel_mark_workers`], when
    ///   `CRATONVM_ZGC_PARMARK` selects it -- in place of `auto`'s ergonomic or
    ///   a numeric count), the compaction's passes (`relocate_worker_count`,
    ///   on by default at 8) and the parallel sweep (`sweep_worker_count`,
    ///   when `CRATONVM_ZGC_PARSWEEP` turned it on). `1` makes all three
    ///   serial. It does NOT turn a pool on: the serial mark and the serial
    ///   sweep are measured defaults (the parallel arms lost on the host), and
    ///   `ParallelGCThreads` names a count, not an algorithm. The explicit
    ///   value is honoured as given up to `Z_PARMARK_MAX_WORKERS`, not
    ///   clamped to the core count -- HotSpot honours an oversubscribed
    ///   `ParallelGCThreads` too.
    /// * `ConcGCThreads` sizes the concurrent mark pool
    ///   ([`Self::conc_mark_workers`]).
    /// * `UseDynamicNumberOfGCThreads` changes only the concurrent default's
    ///   CPU share (25 % vs 12.5 %, HotSpot's `nconcurrent_workers`). This
    ///   VM's pools run a fixed count per cycle -- there is no dynamic
    ///   activation -- which is also why `-Xlog:gc+init` prints no
    ///   `GC Workers ... (dynamic)` line (`vm/src/runtime/unified_logging.rs`).
    ///
    /// HotSpot ZGC's own mapping differs in shape: its marking is concurrent,
    /// so `ConcGCThreads` sizes the marking workers and `ParallelGCThreads`
    /// its runtime (pause-time) workers. This VM's default mark is a pause,
    /// so the pause flag sizes it -- the same "pause workers vs concurrent
    /// workers" split, applied to where this collector does the work.
    ///
    /// `CRATONVM_ZGC_GC_THREAD_FLAGS=0` records the values and applies none.
    pub fn set_gc_worker_threads(
        &self,
        parallel: Option<usize>,
        conc: Option<usize>,
        dynamic: Option<bool>,
    ) {
        let parallel = parallel.filter(|n| *n > 0);
        let conc = conc.filter(|n| *n > 0);
        self.counters.gc_threads.store(parallel, conc, dynamic);
        if !zgc_gc_thread_flags_enabled() {
            return;
        }
        if let Some(p) = parallel {
            let p = p.min(Z_PARMARK_MAX_WORKERS);
            // Only a pool that is ON takes the count (see above); `1` turns
            // each one serial.
            if self.relocate_worker_count.load(Ordering::Relaxed) > 1 || p == 1 {
                self.relocate_worker_count.store(p, Ordering::Relaxed);
            }
            if self.sweep_worker_count.load(Ordering::Relaxed) > 1 || p == 1 {
                self.sweep_worker_count.store(p, Ordering::Relaxed);
            }
        }
    }

    /// `-XX:ParallelGCThreads` as applied to this heap, `None` when not given
    /// or when `CRATONVM_ZGC_GC_THREAD_FLAGS=0`. For `-Xlog:gc+init`'s
    /// `Runtime Workers:` line and tests.
    pub fn gc_parallel_threads(&self) -> Option<usize> {
        self.counters.gc_threads.parallel()
    }

    /// `-XX:ConcGCThreads` as applied to this heap (see
    /// [`Self::gc_parallel_threads`]).
    pub fn gc_conc_threads(&self) -> Option<usize> {
        self.counters.gc_threads.conc()
    }

    /// The compaction pool's current worker count (test and diagnostic hook).
    pub fn relocate_worker_count(&self) -> usize {
        self.relocate_worker_count.load(Ordering::Relaxed)
    }

    /// Mark every object in `[lo, hi)` black, in one pass.
    ///
    /// Returns `false` on the header arm, where there is no bulk operation and
    /// the caller must keep marking per object.
    ///
    /// # Why over-approximating is sound here
    ///
    /// The bitmap is one bit per 8 arena bytes and this sets ALL of them in the
    /// range, not only the ones that are object bases. Nothing reads a
    /// non-base bit: `mark_is_set` is only ever asked about a registered base
    /// (the sweep walks the object-start registry, the reference processor
    /// holds `Reference` object addresses), and the sweep enumerates the
    /// REGISTRY rather than the mark bitmap. So the extra bits are invisible,
    /// and `mark_clear_all` wipes them with everything else.
    ///
    /// What the range must therefore be is not "the objects" but "space only
    /// this thread can allocate into" -- a TLAB chunk, or the un-handed-out
    /// tail of one. See [`Self::blacken_live_tlab_tails`].
    pub fn blacken_range(&self, lo: usize, hi: usize) -> bool {
        match &self.mark_bits {
            Some(bits) => {
                bits.set_range(lo, hi);
                // zt t34: a chunk born black, for the R6 plan.
                if self.counters.crl.live.armed() {
                    self.counters.crl.live.note_range(lo, hi);
                }
                true
            }
            None => false,
        }
    }

    /// Blacken the un-handed-out tail of every live TLAB chunk.
    ///
    /// **Call at the mark-start safepoint, after the bits are cleared and after
    /// [`Self::set_mark_active`].** Every mutator is parked, so `reserved_tail`
    /// is race-free and `[cursor, end)` is exactly the space those threads will
    /// bump into next -- which is what makes the per-object claim skippable for
    /// the rest of the cycle.
    ///
    /// It blackens the TAIL and not the whole chunk on purpose. The already
    /// handed-out part holds objects that existed at mark start; they are in
    /// the snapshot and the marker will decide about them, and blackening them
    /// would retain up to `threads * chunk` bytes of genuinely dead data as
    /// floating garbage for the cycle.
    pub fn blacken_live_tlab_tails(&self) {
        if self.mark_bits.is_none() {
            return;
        }
        for cell in self.tlabs.cells() {
            let mut tlab = match cell.try_lock() {
                Some(t) => t,
                // A cell this thread cannot take is one whose owner is inside
                // an allocation. That cannot happen at a safepoint, and if it
                // somehow does, the fall-back is the per-object claim -- which
                // is what `black_end == 0` leaves in place. Never a correctness
                // hole, only a missed optimisation.
                None => continue,
            };
            match tlab.inner.reserved_tail() {
                Some((cursor, end)) => {
                    self.blacken_range(cursor, end);
                }
                None => {}
            }
        }
    }

    /// Recompute [`Self::conc_start_bytes`] from what this run has actually
    /// measured. **Call once per collection, at the safepoint.**
    ///
    /// # The regression this exists to remove
    ///
    /// `Z_CONC_START_PERCENT_DEFAULT` records the measurement that keeps
    /// concurrent marking off: the per-cycle pause falls 38-58%, which is the
    /// property the phase was built for, but **the cycle COUNT roughly doubles**
    /// (6 -> 12 single-threaded, 3 -> 5 multi-threaded) and on the
    /// single-threaded probe that turns a 38% per-cycle win into a WORSE total
    /// pause.
    ///
    /// The doubling is not inherent to marking concurrently. Everything
    /// allocated after mark start is floating garbage for that cycle, so the
    /// cost of the phase is exactly the size of the window -- and the window
    /// was a FIXED fraction of a fixed threshold, with no input from how fast
    /// the workload allocates or how long the mark actually takes. Open too
    /// early and the collector marks a heap that is mostly about to become
    /// garbage; open too late and the mark does not finish before the
    /// collection is due. One constant cannot be right for both.
    ///
    /// # What it computes
    ///
    /// `threshold - rate * duration * SAFETY`, i.e. "start early enough that
    /// the mark finishes just as the heap fills", which is how a production ZGC
    /// sizes it. Both inputs are measured:
    ///
    /// * `rate` -- bytes allocated per nanosecond between the last two
    ///   collections, smoothed. Read from `allocated` and a wall-clock stamp
    ///   that this function is the only writer of, so it costs one clock read
    ///   per collection and nothing at all on the allocation path.
    /// * `duration` -- how long the last concurrent phase ran, smoothed. Zero
    ///   until one has run, which is what `BOOTSTRAP_PERCENT` covers.
    ///
    /// Smoothed with a 1/2 exponential average rather than used raw: a single
    /// collection's rate is dominated by whatever the program happened to be
    /// doing, and a trigger that chases it oscillates between "no concurrent
    /// phase at all" and "marks the whole heap twice".
    ///
    /// # The floor, and why there is one
    ///
    /// `MIN_PERCENT` of the threshold. Going lower is not free: the SATB
    /// barrier is armed for the whole window and every allocation in it is
    /// black, so opening at 10% would arm the barrier for most of the
    /// program's life to mark a heap that is mostly about to die. The floor is
    /// what stops a burst of allocation from computing a window that large.
    pub fn refresh_adaptive_conc_start(&self, now_nanos: u64) {
        if !self.conc_start_adaptive.load(Ordering::Relaxed) {
            return;
        }
        /// Fraction of the threshold to open at before any concurrent phase has
        /// been timed. The value `Z_CONC_START_PERCENT_DEFAULT`'s own
        /// measurement was taken at, and the one its doc says to opt in with.
        const BOOTSTRAP_PERCENT: usize = 60;
        /// Never open below this fraction of the threshold -- see the floor
        /// note above.
        const MIN_PERCENT: usize = 25;
        /// Never open above this: at 100 the cycle would open when the
        /// collection is already due and there would be no concurrent phase at
        /// all.
        const MAX_PERCENT: usize = 95;
        /// How much longer than last time the mark is allowed to take before
        /// the window is too small. Marks get slower as the live set grows, so
        /// a window sized for the last one is systematically short.
        const SAFETY_NUMERATOR: u64 = 9;
        const SAFETY_DENOMINATOR: u64 = 8;

        // zr z7b/cm: WHICH `allocated`. This runs after the sweep has reset
        // `allocated` to the live bytes, so `allocated - prev` was the growth
        // of the LIVE set between two collections, not the allocation between
        // them: 0 on a steady live set (no sample: the 60 % bootstrap
        // forever) and a few MB of floating-garbage jitter otherwise, which
        // made the headroom ~0 and parked the window at the 90 % ceiling --
        // the last tenth of the threshold, too short for the mark to finish
        // (the z7b host row: 41 % of the
        // objects scanned at the safepoint, `window_actual_ms` half the
        // target). The far end of the sample is now `allocated` as the
        // collection FOUND it (`rate_alloc_at_entry`), the near end the
        // post-sweep figure of the previous collection; both ends are on the
        // same "since the last collection" scale. `=0` restores the old read.
        let allocated = self.allocated.load(Ordering::Relaxed) as u64;
        let prev_bytes = self.rate_sample_bytes.swap(allocated, Ordering::Relaxed);
        let prev_nanos = self.rate_sample_nanos.swap(now_nanos, Ordering::Relaxed);
        let from_allocation = self.counters.rate_from_allocation.load(Ordering::Relaxed);
        let sample_end = if from_allocation {
            self.counters.rate_alloc_at_entry.load(Ordering::Relaxed) as u64
        } else {
            allocated
        };
        // zr z11/rv9: the time span ends where the byte count does. The bytes
        // were read at the collection's ENTRY; `now_nanos` is after the pause,
        // so dividing by it biased the rate low by `pause / interval`. The
        // near end (`prev_nanos`) is the previous refresh, which is also where
        // its bytes were read. `CRATONVM_ZGC_CONC_RATE_EXCLUDE_PAUSE=0` (or a
        // missing stamp) keeps the z7b span.
        let sample_end_nanos = {
            let entry = self.counters.rate_nanos_at_entry.load(Ordering::Relaxed);
            if from_allocation
                && self.counters.rate_exclude_pause.load(Ordering::Relaxed)
                && entry > prev_nanos
                && entry <= now_nanos
            {
                entry
            } else {
                now_nanos
            }
        };
        // A sample needs both ends. The first collection has neither.
        if prev_nanos != 0 && sample_end_nanos > prev_nanos && sample_end > prev_bytes {
            let allocated = sample_end;
            // Bytes per nanosecond is far below 1, so it is carried scaled by
            // 2^20 -- integer throughout, because this runs at a safepoint and
            // a float would be the only one in the collector.
            let sample = ((allocated - prev_bytes) << 20) / (sample_end_nanos - prev_nanos);
            let prior = self.alloc_rate_scaled.load(Ordering::Relaxed);
            let smoothed = if prior == 0 {
                sample
            } else {
                (prior + sample) / 2
            };
            self.alloc_rate_scaled.store(smoothed, Ordering::Relaxed);
        }

        let threshold = self.gc_threshold as u64;
        let rate = self.alloc_rate_scaled.load(Ordering::Relaxed);
        let mark_nanos = self.conc_mark_nanos_ewma.load(Ordering::Relaxed);
        let want = if rate == 0 || mark_nanos == 0 {
            threshold * BOOTSTRAP_PERCENT as u64 / 100
        } else {
            // bytes = rate/2^20 * nanos * safety
            let headroom = (rate.saturating_mul(mark_nanos) >> 20).saturating_mul(SAFETY_NUMERATOR)
                / SAFETY_DENOMINATOR;
            threshold.saturating_sub(headroom)
        };
        let lo = threshold * MIN_PERCENT as u64 / 100;
        let hi = threshold * MAX_PERCENT as u64 / 100;
        let start_bytes = want.clamp(lo, hi);
        self.conc_start_bytes
            .store(start_bytes as usize, Ordering::Relaxed);
        // ---- THE WINDOW'S OWN SETPOINT, RETAINED ---------------------------
        //
        // *Applies `handoff-i-conc-window-fit.md`.* This function is the only
        // thing in the collector with an opinion about how long a mark takes,
        // and `note_conc_mark_duration` is fed the actual -- but until now the
        // figure it sized FROM was dropped on the floor, so
        // `[GC] zgc-markend:` could report how long the concurrent phase ran
        // and not whether that was the length it was aiming for. Those are the
        // two halves of one error term.
        //
        // Derived from the CLAMPED start, not from `want`: the clamp is what
        // the collector will actually get, and a target taken before it would
        // report a window the floor or the ceiling had already refused.
        //
        // `rate` is bytes-per-nanosecond scaled by 2^20 (see the sample above),
        // so nanos = (bytes << 20) / rate. Zero when no rate has been sampled,
        // which is the honest "no opinion yet" rather than a fabricated target.
        let target_nanos = if rate == 0 {
            0
        } else {
            // `saturating_mul` rather than `<< 20`: an absurd `-Xmx` would
            // otherwise overflow the shift, which panics in a debug build.
            threshold
                .saturating_sub(start_bytes)
                .saturating_mul(1 << 20)
                / rate
        };
        self.counters
            .conc_window_target_nanos
            .store(target_nanos, Ordering::Relaxed);
    }

    /// Set the pause target, in milliseconds. `0` turns the pause-target
    /// budget off.
    ///
    /// This is `-XX:MaxGCPauseMillis`'s door into this collector. The flag
    /// reached only G1 before 2026-09-03 (`G1ConfigOverrides::max_gc_pause_ms`);
    /// on the DEFAULT collector an operator who asked for a pause target got
    /// no answer and no diagnostic, which is worse than not supporting it.
    pub fn set_pause_target_ms(&self, ms: u64) {
        self.pause_target_ms.store(ms, Ordering::Relaxed);
    }

    /// `(target ms, affordable span bytes, budget bytes, cycles the target was
    /// unreachable in)` -- the pause-target loop's own state, so a run can be
    /// asked why it sized its budget the way it did.
    pub fn pause_target_state(&self) -> (u64, u64, usize, usize) {
        (
            self.pause_target_ms.load(Ordering::Relaxed),
            self.pause_affordable_span.load(Ordering::Relaxed),
            self.alloc_trigger_bytes.load(Ordering::Relaxed),
            self.counters
                .pause_target_unreachable
                .load(Ordering::Relaxed),
        )
    }

    /// Re-derive the allocation budget from the PAUSE TARGET, using what this
    /// cycle just cost. Called once per collection, at the safepoint.
    ///
    /// # A target is a CEILING, not a setpoint
    ///
    /// This is the whole design, and the first two versions of it got the
    /// point wrong. The obvious controller solves `pause == target`:
    ///
    /// ```text
    ///     affordable = span * target / pause
    ///     budget     = affordable - live
    /// ```
    ///
    /// whose fixed point is a pause exactly ON the target, from either side.
    /// Measured on `G1ChurnPauseProbe 50 600` at `-Xmx2048m`, whose natural
    /// worst pause is 175 ms: with a 200 ms target that controller fired seven
    /// cycles, grew the budget until pauses reached 203 ms, and cost 17% of
    /// wall. It did exactly what it was asked -- it held 200 ms -- by making
    /// the pauses LONGER and paying for the privilege. Nobody who sets
    /// `-XX:MaxGCPauseMillis=200` is asking for that.
    ///
    /// So the loop is one-sided, and it starts UNCONSTRAINED:
    ///
    /// * **A pause that overran the target TIGHTENS.**
    ///   `affordable = span * target / pause`, floored at half the previous
    ///   value so one anomalous pause cannot collapse the budget.
    /// * **A pause under THREE QUARTERS of the target RELAXES**, by a quarter,
    ///   never past seven-eighths of the span that last overran
    ///   ([`Self::pause_overrun_span`]), and hands back control entirely once
    ///   the affordable span reaches capacity. Without a relax path a single
    ///   transient spike would constrain the heap for the rest of the run.
    /// * **Anything in between leaves the budget alone** -- the hysteresis
    ///   band `[0.75, 1.0] x target` that stops the loop hunting. Three
    ///   quarters and not a half: pause length is noisy, so a loop that has
    ///   settled just under the target overruns every few cycles and tightens,
    ///   and a band that wide means the cycles in between never give anything
    ///   back. It RATCHETS. Measured at `-Xmx4096m` against a 100 ms target
    ///   with a half-target band: pauses held at 55-95 ms for a hundred
    ///   consecutive cycles while the budget walked one way from 199 MiB to
    ///   11 MiB and the run collected 139 times instead of 3.
    ///
    /// The shape is `CRATONVM_G1_YOUNG_PAUSE_TARGET`'s in `g1.rs` -- tighten
    /// after an overrun, relax while pauses stay comfortably under the goal --
    /// with the band narrowed for the reason above.
    ///
    /// The consequence that matters: on a workload whose pauses never reach
    /// the target the clause NEVER ENGAGES, and costs one relaxed load per
    /// collection. That is what makes a non-zero default defensible, where the
    /// percentage form ([`zgc_alloc_trigger_percent`]) had to ship off.
    ///
    /// # Why the correction is multiplicative
    ///
    /// The pause is not proportional to the span. It is roughly
    /// `fixed + k * span`, and on this collector the fixed part is large -- a
    /// measured 35 ms pause over a 5.7 MiB span next to a 48 ms pause over a
    /// 167 MiB one puts it near 34 ms. A controller that fitted `k` as a plain
    /// ratio of pause to span would attribute all of that fixed cost to the
    /// span: the first version did, and it reported coefficients an order of
    /// magnitude too high on the small early cycles, cutting the budget to
    /// 8-27 MiB where the converged answer was over 140. Scaling the span by
    /// `target / pause` never has to know `fixed` or `k`, because the only
    /// place the correction factor is 1 is the place the pause equals the
    /// target.
    ///
    /// # The clamps, and why an unmeetable target means DO NOTHING
    ///
    /// * **Ceiling** (capacity). A budget above the heap can never be reached,
    ///   which is the same as being unconstrained -- so reaching it hands
    ///   control back to [`zgc_alloc_trigger_percent`]'s constant.
    /// * **Floor** ([`ZGC_ALLOC_TRIGGER_FLOOR`], 8 MiB) for a budget the loop
    ///   wants SMALLER than that, against a live set the target can afford.
    /// * **`affordable <= live`: the clause goes inert**, hands control back
    ///   to [`zgc_alloc_trigger_percent`]'s constant, FORGETS its state, and
    ///   [`ZgcCounters::pause_target_unreachable`] counts the cycle.
    ///
    /// The last two look alike and are opposite, and collapsing them was a
    /// defect. When the LIVE SET alone projects past the target, the pause
    /// length is set by the live set and no budget changes it: collecting
    /// every 8 MiB against a 1.5 GiB live set buys pauses of exactly the same
    /// length, hundreds of times more often, and the clause deliberately does
    /// not consult `gc_rearm` (see `zgc_alloc_trigger_percent`) so nothing
    /// else would have stopped it. But when the live set is CHEAP and the loop
    /// merely wants a tight budget, going inert loses control completely --
    /// measured at `-Xmx4096m`, a tightening step that crossed the floor
    /// dropped the constraint and the next cycle ran unconstrained to 670 ms.
    ///
    /// Going inert also has to RESET the loop. The tighten arm never widens
    /// and the relax arm is capped by [`Self::pause_overrun_span`], so a stale
    /// affordable span is a ceiling on everything the controller can ever
    /// believe again: a startup cycle that overran by 3% on a 5 MiB span
    /// pinned it at 4.8 MiB, below the live set, and the run never recovered.
    /// "I have no constraint" is the same state as "I have never
    /// constrained".
    ///
    /// A climbing `unreachable` is the diagnosis: the target is not achievable
    /// at this live set, which is an answer, and a different one from "the
    /// loop is holding the target".
    ///
    /// Three CONSECUTIVE unreachable verdicts stop the loop for the run
    /// ([`Self::pause_target_ms`] goes to `0` and this function is inert
    /// thereafter), because re-engaging repeats the discovery and the
    /// discovery costs one unconstrained cycle each time. The word that
    /// carries the weight is *consecutive*:
    /// [`ZgcCounters::pause_target_unreachable_streak`] is what the test
    /// reads, and that field's doc records what reading the cumulative counter
    /// instead did to a long run.
    ///
    /// # What the clock covers
    ///
    /// By default `pause_ns` is the same figure `[GC] zgc-pause:` reports,
    /// which is taken BEFORE the compaction slide. Compaction is **on** by
    /// default (`relocation_requested_by_default`), and the slide measured at
    /// about twice that reading on `ZgcTlabStress 8 200`, so the loop holds its
    /// ceiling on roughly a third of the pause the mutators took. Worse, the
    /// correction it applies is multiplicative — `span * target / pause` — and
    /// `span` here is `live + freed`, which INCLUDES what the slide then moved.
    /// A ratio whose numerator and denominator describe different work is not a
    /// correction factor for anything.
    ///
    /// `CRATONVM_ZGC_PAUSE_TARGET_INCLUDES_RELOCATE=1` moves the reading to
    /// after the slide, where it matches `[GC] zgc-relocate:`'s
    /// `pause_with_relocate_us`. Off for one wave because it changes every
    /// budget on every workload in the tightening direction; see
    /// [`zgc_pause_target_includes_relocate`] for the flip criteria.
    pub fn refresh_pause_target_budget(
        &self,
        pause_ns: u64,
        live_bytes: usize,
        bytes_freed: usize,
    ) {
        let target_ms = self.pause_target_ms.load(Ordering::Relaxed);
        if target_ms == 0 {
            return;
        }
        let span_bytes = live_bytes.saturating_add(bytes_freed) as u64;
        if span_bytes == 0 || pause_ns == 0 {
            // Nothing was walked, or the clock could not resolve the pause.
            // Either way this cycle carries no correction factor -- `pause` is
            // the divisor -- and inventing one would size the next budget off
            // a cycle that did not happen.
            return;
        }
        let target_ns = target_ms.saturating_mul(1_000_000);
        // A CYCLE THAT MET THE TARGET BREAKS THE STREAK. Unconditionally, here,
        // before any of the arms below decide what to do about the budget.
        //
        // # Why this statement exists (2026-09-20)
        //
        // The give-up rule ~80 lines down reads
        // `pause_target_unreachable_streak` and its comment is emphatic that it
        // means *three CONSECUTIVE failures*. Wave 1 introduced the streak
        // counter to replace a cumulative one that did not — and reset it in
        // exactly two places, both of them on the **constrained** path: the
        // `Some(b)` arm and the `affordable >= cap` arm. Neither is reached by
        // a cycle that simply met its target.
        //
        // Trace the common shape. `prior == 0` (never constrained) and
        // `pause_ns <= target_ns` falls into the `else { return; }` at the end
        // of the `affordable` chain — the healthy steady state, and it returns
        // without touching the streak. So did the hysteresis band, which the
        // comment above it describes as *"the loop was holding the target
        // perfectly"*. Three unreachable verdicts scattered across an hour of
        // cycles that all met their target therefore still added up to `>= 3`
        // and disabled a default-on feature for the life of the process — the
        // precise defect the streak was introduced to fix, surviving the fix
        // because the counter changed and the reset condition did not.
        //
        // The honest predicate for "not a failure" is the one the loop is
        // controlling for: the pause came in at or under the target. The two
        // existing resets stay — they fire on OVERRUN cycles where the tighten
        // path nevertheless found a workable budget, which is a success this
        // test cannot see.
        if pause_ns <= target_ns {
            self.counters
                .pause_target_unreachable_streak
                .store(0, Ordering::Relaxed);
            self.counters
                .pause_target_floor_overruns
                .store(0, Ordering::Relaxed);
        }
        let prior = self.pause_affordable_span.load(Ordering::Relaxed);
        // A CYCLE RUN AT THE FLOOR BUDGET THAT STILL OVERRAN (round 16, lane
        // zgcarray; `docs/feature-designs/zgc-architecture.md`).
        //
        // The loop's model is `pause ~ span`, and the doc above says where it
        // breaks: the pause has a floor the budget cannot move (the arena's
        // span, host stalls, fixed per-cycle work). The `affordable <= live`
        // verdict below cannot see that case when the live set is tiny:
        // `affordable - live` stays positive, the budget is clamped UP to
        // `ZGC_ALLOC_TRIGGER_FLOOR`, and the heap collects every 8 MiB for the
        // rest of the run at a pause the budget does not move. Measured
        // (Windows, base c77f4e9218, `R16ZgcarrayAlloc -Dlens=4096
        // -Drounds=30`, ergonomic 4 GiB heap): the first cycle (760 ms, 3.2 GB
        // freed) halved the budget seven times down to the floor, then 590
        // more collections at 370-640 ms each; 160-265 s against 4 s with
        // `CRATONVM_ZGC_PAUSE_TARGET_MS=0` (3 collections).
        //
        // A cycle that overran with the budget already at the floor, and that
        // walked about a floor's worth of garbage, is direct evidence that no
        // budget meets the target: count it as unreachable, go inert, and give
        // up after `ZGC_PAUSE_FLOOR_OVERRUNS_TO_GIVE_UP` of them without a
        // cycle meeting the target in between (the streak above is no use
        // here: every re-engagement passes through the `Some(b)` arm, which
        // breaks it). `bytes_freed <= 4 * floor` keeps the
        // `a_budget_below_the_floor_is_floored_not_abandoned` shape (a cheap
        // live set walking 38 MiB per cycle) on the floor, as measured there.
        // `CRATONVM_ZGC_PAUSE_FLOOR_VERDICT=0` is the kill switch.
        let floor = ZGC_ALLOC_TRIGGER_FLOOR.min(self.heap_capacity());
        if pause_ns > target_ns
            && prior != 0
            && self
                .counters
                .pause_floor_verdict_enabled
                .load(Ordering::Relaxed)
            && self.alloc_trigger_bytes.load(Ordering::Relaxed) <= floor
            && bytes_freed <= floor.saturating_mul(4)
        {
            self.counters
                .pause_target_unreachable
                .fetch_add(1, Ordering::Relaxed);
            let overruns = self
                .counters
                .pause_target_floor_overruns
                .fetch_add(1, Ordering::Relaxed)
                + 1;
            // Forget everything, as the unreachable arm below does, so a later
            // re-engagement starts from that cycle's own evidence.
            self.pause_affordable_span.store(0, Ordering::Relaxed);
            self.pause_overrun_span.store(0, Ordering::Relaxed);
            if overruns >= ZGC_PAUSE_FLOOR_OVERRUNS_TO_GIVE_UP {
                self.pause_target_ms.store(0, Ordering::Relaxed);
            }
            self.alloc_trigger_bytes
                .store(self.alloc_trigger_percent_bytes, Ordering::Relaxed);
            return;
        }
        let affordable = if pause_ns > target_ns {
            // OVERRAN. Remember the span that did it -- the relax path may not
            // climb back into it -- and scale the span by how far over it
            // went. `u128` so the product cannot wrap: span is bounded by the
            // heap, but `target_ns` is a flag and `pause_ns` a clock reading,
            // and neither is bounded by anything this file owns.
            self.pause_overrun_span.store(span_bytes, Ordering::Relaxed);
            let sample = (u128::from(span_bytes) * u128::from(target_ns) / u128::from(pause_ns))
                .min(u128::from(u64::MAX)) as u64;
            if prior == 0 {
                sample
            } else {
                // Never widen on an overrun, and never cut by more than half.
                sample.clamp(prior / 2, prior)
            }
        } else if prior != 0 && pause_ns.saturating_mul(4) <= target_ns {
            // FAR under -- a quarter of the target or less. The span this
            // cycle walked is much cheaper than the remembered overrun says it
            // should be, so the memory is about a heap that no longer exists:
            // drop it and let the loop relax freely again.
            self.pause_overrun_span.store(0, Ordering::Relaxed);
            prior.saturating_add(prior / 4)
        } else if prior != 0 && pause_ns.saturating_mul(4) <= target_ns.saturating_mul(3) {
            // UNDER THREE QUARTERS of the target, and currently constrained:
            // give a quarter back, but NEVER past seven-eighths of the span
            // that last overran. See `pause_overrun_span` for what re-probing
            // that region costs.
            //
            // Three quarters and not a half. The band has to be narrow enough
            // that a loop sitting just under the target still relaxes,
            // otherwise it RATCHETS: pause length is noisy, so a settled loop
            // overruns every few cycles and tightens, while a band of
            // `[target/2, target]` means the cycles in between are never
            // comfortable enough to give anything back. Measured on
            // `G1ChurnPauseProbe 50 1800` at `-Xmx4096m` with a 100 ms target
            // and a half-target band: pauses sat at 55-95 ms for a hundred
            // consecutive cycles -- the loop was holding the target perfectly
            // -- while the budget walked one way from 199 MiB down to 11 MiB
            // and the run collected 139 times instead of 3. Noise alone drove
            // it, because tightening had no dead zone and relaxing was
            // unreachable.
            let wider = prior.saturating_add(prior / 4);
            match self.pause_overrun_span.load(Ordering::Relaxed) {
                0 => wider,
                overran => wider.min(overran / 8 * 7).max(prior),
            }
        } else {
            // In the hysteresis band, or never constrained. Leave it.
            return;
        };
        let cap = self.heap_capacity();
        if affordable >= cap as u64 {
            // Unconstrained again: hand the clause back to the percentage
            // form, which is `0` (off) unless an operator set one.
            //
            // A SUCCESS, so it breaks the unreachable streak for the same
            // reason the `Some(b)` arm below does -- the loop found a span the
            // target can afford, it just happens to be the whole heap.
            self.counters
                .pause_target_unreachable_streak
                .store(0, Ordering::Relaxed);
            self.pause_affordable_span.store(0, Ordering::Relaxed);
            self.alloc_trigger_bytes
                .store(self.alloc_trigger_percent_bytes, Ordering::Relaxed);
            return;
        }
        self.pause_affordable_span
            .store(affordable, Ordering::Relaxed);
        // TWO DIFFERENT SITUATIONS, and collapsing them into one clamp was a
        // defect worth its own paragraph.
        //
        //   * `affordable <= live` -- the LIVE SET alone projects past the
        //     target. No budget makes this pause fit, so go INERT: the pause
        //     length is set by the live set and a small budget would buy
        //     pauses of exactly the same length, hundreds of times more often.
        //     The clause deliberately skips `gc_rearm`, so nothing else would
        //     have stopped it.
        //   * `0 < affordable - live < FLOOR` -- the loop wants a SMALL budget
        //     against a cheap live set. That is not the same thing at all, and
        //     going inert there loses control completely: measured on
        //     `G1ChurnPauseProbe 50 1800` at `-Xmx4096m` with a 100 ms target,
        //     a tightening step that crossed the floor dropped the constraint,
        //     the next cycle ran unconstrained to 670 ms, and the loop spent
        //     the run oscillating between a tight budget and no budget. The
        //     floor is the right answer here -- it is the tightest useful
        //     budget, and against a live set the target can afford it is not a
        //     storm.
        let budget = match affordable.checked_sub(live_bytes as u64) {
            // Converted in `u64`: `affordable` can legitimately exceed
            // `usize::MAX` on a 32-bit host, where `as usize` would truncate a
            // huge budget into a small one.
            Some(b) if b > 0 => {
                // THE STREAK BREAKS HERE, and it is the statement that makes
                // the give-up rule below say what its comment says. See
                // `ZgcCounters::pause_target_unreachable_streak`.
                self.counters
                    .pause_target_unreachable_streak
                    .store(0, Ordering::Relaxed);
                usize::try_from(b)
                    .unwrap_or(usize::MAX)
                    .clamp(ZGC_ALLOC_TRIGGER_FLOOR.min(cap), cap)
            }
            _ => {
                self.counters
                    .pause_target_unreachable
                    .fetch_add(1, Ordering::Relaxed);
                let streak = self
                    .counters
                    .pause_target_unreachable_streak
                    .fetch_add(1, Ordering::Relaxed)
                    + 1;
                // AND FORGET EVERYTHING. "I have no constraint" has to be the
                // same state as "I have never constrained", or the loop can
                // lock itself out permanently -- which it did.
                //
                // The tighten arm never widens (`sample.clamp(prior / 2,
                // prior)`), and the relax arm is capped by
                // `pause_overrun_span`. So a stale `prior` is a CEILING on
                // everything the loop can ever believe again. Observed at
                // `-Xmx4096m` with a 100 ms target: a startup cycle overran by
                // 3% while walking a ~5 MiB span, pinning `affordable` at
                // 4.8 MiB; that is below the live set, so the clause went
                // inert; being inert means no constraint, so every following
                // cycle ran unconstrained at 640-733 ms; and each of those
                // could only ever clamp back down to the stale 4.8 MiB. The
                // run never recovered.
                //
                // Cleared, the next overrun re-seeds from `prior == 0` with
                // that cycle's own span, which is the freshest evidence there
                // is.
                self.pause_affordable_span.store(0, Ordering::Relaxed);
                self.pause_overrun_span.store(0, Ordering::Relaxed);
                // AND GIVE UP AFTER THE THIRD TIME.
                //
                // Re-engaging after an unreachable verdict repeats the
                // discovery, and the discovery costs one UNCONSTRAINED cycle
                // every time: inert means no budget, the next cycle walks the
                // whole span, and the loop tightens back down to the same
                // verdict. Measured at `-Xmx4096m` against a 100 ms target,
                // 56 cycles and +101% wall for a p50 of 103.8 ms -- worse on
                // both axes than not trying.
                //
                // The reason a target can be permanently unachievable is
                // structural, and it is worth stating because it is not
                // obvious from this function: THE PAUSE FLOOR IS THE ARENA'S
                // HIGH-WATER MARK, not the live set. The bitmap sweep covers
                // `[base, low_cursor)` and the cursor does not retract, so
                // once any cycle has run the bump cursor out to 1.3 GiB every
                // later pause pays a scan over that span whatever the budget
                // is. No allocation trigger can undo it; only compaction and a
                // cursor retraction can, and those are `CRATONVM_ZGC_RELOCATE`
                // and `Arena::retract_cursor_to`.
                //
                // Three, not one: a single verdict can come from a transient
                // live-set spike, and the reset above already makes the next
                // attempt an honest one. Three consecutive failures to find
                // ANY budget that meets the target is a property of the
                // workload, not of a bad cycle.
                //
                // CONSECUTIVE, and it reads the streak to mean it. This tested
                // `pause_target_unreachable`, which is CUMULATIVE and never
                // reset, so three spikes spread across an hour of cycles that
                // all met their target disabled the loop for the life of the
                // process -- the argument above with the opposite arithmetic
                // under it. See
                // `ZgcCounters::pause_target_unreachable_streak`.
                if streak >= 3 {
                    self.pause_target_ms.store(0, Ordering::Relaxed);
                }
                self.alloc_trigger_percent_bytes
            }
        };
        self.alloc_trigger_bytes.store(budget, Ordering::Relaxed);
    }

    /// Fold this cycle's concurrent-phase duration into the average
    /// [`Self::refresh_adaptive_conc_start`] sizes the next window from.
    pub fn note_conc_mark_duration(&self, nanos: u64) {
        if nanos == 0 {
            return;
        }
        let prior = self.conc_mark_nanos_ewma.load(Ordering::Relaxed);
        let next = if prior == 0 {
            nanos
        } else {
            (prior + nanos) / 2
        };
        self.conc_mark_nanos_ewma.store(next, Ordering::Relaxed);
    }

    /// `(allocation rate in bytes/sec, mean concurrent mark nanos, the trigger
    /// in bytes)` -- the adaptive trigger's own state, so a run can be asked
    /// why it opened its cycles where it did.
    pub fn adaptive_conc_start_state(&self) -> (u64, u64, usize) {
        (
            (self.alloc_rate_scaled.load(Ordering::Relaxed) * 1_000_000_000) >> 20,
            self.conc_mark_nanos_ewma.load(Ordering::Relaxed),
            self.conc_start_bytes.load(Ordering::Relaxed),
        )
    }

    /// Should a concurrent mark cycle open now?
    ///
    /// On the allocation path of both the interpreter (`maybe_gc`) and the JIT
    /// (`jit_new_object` / `jit_newarray`), so it is two relaxed loads and
    /// three comparisons against precomputed fields. The percentage is turned
    /// into [`Self::conc_start_bytes`] once, at construction: it used to be a
    /// division here, which is a strange thing to put on an allocation path
    /// for a number that cannot change.
    ///
    /// Everything expensive is behind [`Self::start_concurrent_mark`].
    ///
    /// (zr zs s1/mc: this doc had come adrift onto `blacken_range`, together
    /// with an `#[inline]` meant for this function.)
    #[inline]
    pub fn should_start_concurrent_mark(&self) -> bool {
        // `0` is the kill switch (`CRATONVM_ZGC_CONC_START=0`) AND the
        // disabled-by-default state, so this is the first and cheapest test.
        let trigger = self.conc_start_bytes.load(Ordering::Relaxed);
        if trigger == 0 {
            return false;
        }
        // zt t21/zc2 (`CRATONVM_ZGC_CONCURRENT_SWEEP`): the last cycle's sweep
        // has not installed, so that cycle has not ended and `allocated` still
        // holds its dead bytes; no window opens until the hook re-arms it.
        if self.counters.csweep.pending() {
            return false;
        }
        let a = self.allocated.load(Ordering::Relaxed);
        if self.conc_cycle_active.load(Ordering::Relaxed) {
            return false;
        }
        // zr z3/cs: under `CRATONVM_ZGC_CONC_EVERY_TRIGGER` a cycle opens on a
        // `needs_gc` request, never on the occupancy window.
        if self.conc_every_trigger_armed() {
            return self.conc_start_requested();
        }
        // t1 lane zgc (`CRATONVM_ZGC_GEN_EVERY_TRIGGER_WINDOW_INERT`, default
        // ON, `=0` kill): the every-trigger mode (and the concurrent profile
        // that turns it on) is documented inert in generational mode, and
        // `conc_every_trigger_armed` says so -- but its construction seed (60 %
        // of the threshold, `conc_start_seed_percent`) stayed in
        // `conc_start_bytes`, so a generational heap fell through to here and
        // ran the occupancy-window mode nobody configured. Every window cycle
        // closes as a major (`young_cycle_predicted` refuses an open cycle),
        // so the young cycles the mode exists for ran only between windows.
        // An explicit `CRATONVM_ZGC_CONC_START` (enabled == false) is the
        // other half, below. Page:
        // `docs/feature-designs/zgc-architecture.md`.
        //
        // t1 wave 2 lane Z (`CRATONVM_ZGC_GEN_CONC_START_WINDOW_INERT`, default
        // ON, `=0` kill): the explicit half. HotSpot 25's generational ZGC
        // marks its young collections concurrently and a concurrent mark never
        // turns a minor into a major; here a concurrently marked cycle is
        // always whole-heap, so the HotSpot-faithful answer (the minor/major
        // split, `ZGC Minor Cycles`) is to keep the young cycles and refuse the
        // window on a generational heap, as the every-trigger mode already is.
        // The refusal is named on the `[GC] zgc-conc-miss:` line (`gen_inert=1`).
        if self.generational_enabled.load(Ordering::Relaxed) && self.gen_conc_window_inert() {
            return false;
        }
        // zr z2/cc: the window ends where the NEXT collection will actually
        // be taken, not at `gc_threshold`. See `conc_start_window`.
        let (start, end) = self.conc_start_window(trigger, a);
        // The `< end` clause is not redundant. At the collection point a
        // collection is already due, and opening a cycle there would pay a
        // mark-start pause for a concurrent phase of zero length.
        a >= start && a < end
    }

    /// Is the occupancy window withdrawn on this (generational) heap? The
    /// every-trigger seed answers to
    /// `CRATONVM_ZGC_GEN_EVERY_TRIGGER_WINDOW_INERT`, an explicit
    /// `CRATONVM_ZGC_CONC_START` to `CRATONVM_ZGC_GEN_CONC_START_WINDOW_INERT`
    /// (t1 wave 2 lane Z). Callers test `generational_enabled` first, so the
    /// flag reads happen only on that combination of opt-ins.
    #[inline(never)]
    pub fn gen_conc_window_inert(&self) -> bool {
        if self.counters.conc_trigger.enabled.load(Ordering::Relaxed) {
            true
        } else {
            true
        }
    }

    /// The concurrent-start window `[start, end)` in `allocated` bytes, for
    /// the configured trigger `trigger` (`conc_start_bytes`), at `allocated`
    /// `a` (zr z2/cc).
    ///
    /// # Why the window was `[trigger, gc_threshold)` and why that was vacuous
    ///
    /// `gc_threshold` is only ONE of the points at which `needs_gc` collects.
    /// The warm-up clause collects at 10 / 20 / 30 % of the soft maximum for
    /// the first [`ZGC_WARMUP_CYCLES`] collections, and the allocation-budget
    /// clause collects after `alloc_trigger_bytes` of allocation since the
    /// last collection. When either binds first -- every short run, and every
    /// run whose live set is small next to the budget -- `allocated` never
    /// reaches a trigger placed at 60 % of the threshold before the
    /// collection resets it, and no cycle ever opens: `Zr1RfConcWeakProbe` at
    /// `-Xmx64m` ran ZERO concurrent cycles under `CONC_START=auto`, and
    /// `Zr1MkParmarkCost 22 30 4` at `-Xmx2g` about two per run.
    ///
    /// So the window ends at the NEAREST predictable collection point
    /// (`end`), and when that is earlier than the threshold the trigger keeps
    /// its PROPORTION of the way there from the last collection's watermark:
    /// `start = watermark + (end - watermark) * trigger / gc_threshold`. When
    /// the threshold is the nearest point the window is the old one, byte for
    /// byte. zr z3/cs: the director's published byte point (allocation rate,
    /// high usage; deciding only under `CRATONVM_ZGC_RATE_TRIGGER`) joins the
    /// nearest-point minimum. The headroom clause (arena fragmentation) and
    /// the proactive rule have no byte point and cannot be predicted from
    /// here; `CRATONVM_ZGC_CONC_EVERY_TRIGGER` closes them by turning their
    /// yes into a cycle request instead of predicting it.
    ///
    /// `CRATONVM_ZGC_CONC_START_NEAREST=0` restores `[trigger, gc_threshold)`.
    pub fn conc_start_window(&self, trigger: usize, _a: usize) -> (usize, usize) {
        let threshold = self.gc_threshold;
        if !zgc_conc_start_nearest() {
            return (trigger, threshold);
        }
        let mut end = threshold;
        // The warm-up clause's point (`needs_gc`): `soft_max / 10 * (done + 1)`,
        // admitted only above the re-arm floor.
        let done = self.warmup_cycles_done();
        if done < ZGC_WARMUP_CYCLES && self.counters.warmup_enabled.load(Ordering::Relaxed) {
            let warm = (self.soft_max_capacity() / 10).saturating_mul(done + 1);
            let point = warm.max(self.warmup_rearm_floor());
            end = end.min(point);
        }
        // The allocation-budget clause's point.
        let watermark = self.cycle_alloc_watermark.load(Ordering::Relaxed);
        let budget = self.alloc_trigger_bytes.load(Ordering::Relaxed);
        if budget > 0 {
            end = end.min(watermark.saturating_add(budget));
        }
        // zr z3/cs: the director's byte-expressible rules (allocation rate,
        // high usage), when they decide (`CRATONVM_ZGC_RATE_TRIGGER=1`). The
        // published point is `u64::MAX` until the first sample after a cycle
        // (no term) and `0` once a rule already says yes: the collection is
        // due, so the window is EMPTY (a cycle opened now would have no
        // concurrent phase). The proactive rule is a time rule with no byte
        // point; it and the headroom clause are served by
        // `CRATONVM_ZGC_CONC_EVERY_TRIGGER`.
        if self.counters.director.is_deciding() {
            let mut fire_at =
                usize::try_from(self.counters.director.fire_at_used()).unwrap_or(usize::MAX);
            // zs s2/rv: a director yes is HELD below `gc_rearm` by the rearm
            // floor (`needs_gc`, `CRATONVM_ZGC_DIRECTOR_REARM_FLOOR`), so the
            // collection comes at `gc_rearm`, not at `fire_at`. Without this
            // the held yes (`fire_at == 0`) emptied the window for the whole
            // floored interval and the cycle that finally ran was
            // stop-the-world. `gc_rearm == 0` (before the first cycle) and the
            // floor's kill leave `fire_at` as it was.
            if fire_at != usize::MAX && self.counters.director_rearm_floor.load(Ordering::Relaxed) {
                fire_at = fire_at.max(self.gc_rearm.load(Ordering::Relaxed));
            }
            // zr z11/rv9 (every-trigger residual 4): only `0` means "a rule
            // already says yes". A NON-zero point at or below the watermark
            // (a live set above `-XX:SoftMaxHeapSize`'s high-usage point, read
            // before the director re-evaluated) emptied the window, so every
            // cycle ran stop-the-world where HotSpot still runs concurrent
            // ones. It now drops out of the nearest-point minimum instead.
            // (zt t12/rt: the opt-in restore of the empty window was retired.)
            if fire_at == 0 {
                return (fire_at, fire_at);
            }
            if fire_at > watermark {
                end = end.min(fire_at);
            }
        }
        if end >= threshold || threshold == 0 || end <= watermark {
            return (trigger, threshold);
        }
        let span = end - watermark;
        // `trigger / threshold` as a proportion, in u128 so a large heap's
        // `span * trigger` cannot overflow.
        let lead = (span as u128 * trigger.min(threshold) as u128 / threshold as u128) as usize;
        (watermark.saturating_add(lead), end)
    }

    /// Arrange the concurrent-start trigger. **Test seam.**
    ///
    /// Production sets this once, in the constructor, from
    /// `CRATONVM_ZGC_CONC_START`. A test cannot go through the variable
    /// because the reader caches in a `OnceLock` and would fix the value for
    /// every other test in the binary.
    #[cfg(test)]
    pub fn dbg_set_conc_start_bytes(&self, bytes: usize) {
        // t1 wave 2 lane Z: the occupancy window is withdrawn on a
        // generational heap (`CRATONVM_ZGC_GEN_CONC_START_WINDOW_INERT`), so a
        // test arming the window pins the single-generation heap it is about,
        // as `dbg_set_conc_every_trigger` does; under
        // `CRATONVM_ZGC_GENERATIONAL=1` every test heap starts generational.
        if bytes != 0 && self.generational_enabled() {
            self.set_generational_enabled(false);
        }
        self.conc_start_bytes.store(bytes, Ordering::Relaxed);
    }

    /// Is a concurrent mark cycle in flight?
    #[inline]
    pub fn concurrent_mark_active(&self) -> bool {
        self.conc_cycle_active.load(Ordering::Relaxed)
    }

    /// Open a concurrent mark cycle. **Call at a stop-the-world safepoint.**
    ///
    /// `roots` is the VM's complete root set, exactly as `collect_garbage`
    /// receives it: every mutator is parked, every in-JIT peer has been
    /// forcibly stopped and conservatively scanned, and the caller holds the
    /// [`StopTheWorldToken`] that proves it.
    ///
    /// Returns `true` iff the cycle opened. It refuses -- and the collector
    /// simply stays stop-the-world -- when this heap has no `Arc` identity
    /// (a test heap), when a cycle is already open, or when the worker pool
    /// cannot be built.
    ///
    /// # Ordering, and why each step is where it is
    ///
    /// 1. **Clear every mark bit**, over a registry snapshot taken here. This
    ///    is the last moment at which "no object is marked" is true, and it
    ///    must precede arming the barrier or an allocate-black object's bit
    ///    would be cleared right back off.
    /// 2. **Snapshot the reference skip set.** `visit_refs` needs it for the
    ///    whole cycle; without it every weak/soft/phantom referent is traced
    ///    as a strong edge and can never be cleared.
    /// 3. **Arm the SATB barrier** (`set_mark_active(true)`), which also
    ///    clears the ingress. From here every reference store in the VM
    ///    publishes its overwritten value.
    /// 4. **Build the pool and push the roots**, then arm the workers. Roots
    ///    are pushed before `start_marking` because a worker that finds an
    ///    empty stripe set terminates immediately.
    ///
    /// # What this deliberately does NOT do: retire the TLABs
    ///
    /// `collect_garbage` retires every TLAB as its first statement, and the
    /// instinct is to copy that. It buys three things there -- chunk tails
    /// back on the free list, a walkable arena, and "no mutator owns a private
    /// slice" -- and marking needs none of them: it follows registered bases
    /// from roots, never walks, and a TLAB-served object enters the registry
    /// at hand-out. What retiring WOULD do here is put every chunk tail on the
    /// free list at the exact moment the mutators are about to resume and
    /// re-carve chunks, for no marking benefit, inside a pause this phase
    /// exists to keep short.
    pub fn start_concurrent_mark(&self, _stw: &StopTheWorldToken, roots: &[u64]) -> bool {
        // zt t21/zc2 (`CRATONVM_ZGC_CONCURRENT_SWEEP`): the previous cycle's
        // parked sweep reads this cycle's mark bits until its install clears
        // them, so it is finished before anything below can clear or set one.
        // One relaxed load when nothing is pending.
        let _ = self.finish_pending_sweep(ZSweepRunner::MarkStart);
        if self.conc_cycle_active.load(Ordering::Relaxed) {
            return false;
        }
        let Some(me) = self.self_weak.get().and_then(|w| w.upgrade()) else {
            // A heap built with `with_capacity` rather than `new_shared`. Not
            // an error: it is every unit test in this file, and the collector
            // is correct without concurrency.
            return false;
        };
        // zt t31/x10 (item 1): the occupancy at this mark start, for the
        // closing collection's `-Xlog` tables (a declined start leaves a
        // value the next collection discards: it did not mark concurrently).
        self.counters
            .conc_mark_start_used
            .store(self.allocated.load(Ordering::Relaxed), Ordering::Relaxed);

        // THE MARK-START PAUSE IS A PAUSE, and until 2026-08-17 nothing
        // measured it. `--verbose:gc`'s `pause_us` is taken inside
        // `collect_garbage`, so a concurrent cycle's cost was reported as one
        // pause when it is two -- and the unmeasured half contains a full
        // registry walk (the mark-bit clear). Any total-pause comparison that
        // omitted it understated the concurrent arm.
        let armed = self.gc_log_enabled.load(Ordering::Relaxed);
        let started_at = armed.then(std::time::Instant::now);
        let stw_started = self
            .counters
            .director
            .is_sampling()
            .then(std::time::Instant::now);
        let mut clock = ZPhaseClock::new(armed);

        // (0) zr z11/rv9 (`CRATONVM_ZGC_CONC_POOL_WEAK_HEAP`, default ON): spawn
        // the pool FIRST, over the inert context, so a spawn failure declines
        // the cycle (stays stop-the-world) before anything below has armed a
        // barrier or blackened a tail -- it used to panic inside this pause.
        // The heap is bound per cycle, WEAKLY, in (4): a pool built over the
        // heap's own `Arc` and parked in `conc_pool` made heap and pool own
        // each other while the cycle was open, so a heap dropped mid-cycle
        // (an embedder dropping its VM, a test that never finished the cycle)
        // was never freed and its workers stayed parked for the process.
        let weak_pool = true;
        let workers = self.conc_mark_workers();
        // t1 wave 2 lane Z (`CRATONVM_ZGC_CONC_POOL_REUSE`): the pool the
        // last certified cycle parked, when it has this cycle's size.
        let reused = if weak_pool {
            self.take_idle_conc_pool(workers)
        } else {
            None
        };
        let reused_pool = reused.is_some();
        let early_pool = if let Some(pool) = reused {
            Some(pool)
        } else if weak_pool {
            match mark::ZMarkCoordinator::try_new_inert(workers) {
                Ok(pool) => Some(std::sync::Arc::new(pool)),
                Err(e) => {
                    self.counters
                        .conc_pool_spawn_failures
                        .fetch_add(1, Ordering::Relaxed);
                    tracing::warn!(
                        target: "zgc",
                        workers,
                        error = %e,
                        "zgc concurrent mark: could not spawn the mark pool; the cycle stays stop-the-world"
                    );
                    return false;
                }
            }
        } else {
            None
        };
        let spawn_us = clock.lap();

        // (1) CLEAR THE MARK BITS -- only if they might not already be clear.
        //
        // This walk was 94-96% of the mark-start pause and a counter found
        // `stale_marked=0` on every one of 20 mark starts: it was clearing bits
        // that the previous sweep had already cleared. See
        // `conc_bits_known_clear` for the full argument and for the one path
        // (`abandon_concurrent_mark`) that makes it necessary.
        //
        // The registry snapshot goes with it: its only consumer was this loop.
        let mut stale = 0usize;
        let mut n_registered = 0usize;
        let mut snapshot_us = 0u128;
        let mut clearbits_us = 0u128;
        let known_clear = self.conc_bits_known_clear.load(Ordering::Acquire);
        // In a debug build, VERIFY the latch rather than trusting it: walk
        // anyway and count. A latch that silently lies here hands the sweep a
        // mark set with a previous cycle's bits in it, which is a retained
        // object at best. Release builds skip the walk entirely, which is the
        // whole point.
        // THE BITMAP ARM DOES NOT WALK AT ALL, in either build. A `clear_all`
        // is a linear store over `capacity / 512` bytes and does not scale with
        // the object count, so the release build's reason for skipping (it
        // would be clearing already-clear bits) and the debug build's reason
        // for walking (verify the latch) both stop applying: there is nothing
        // cheaper to skip to, and the counter the debug walk exists to produce
        // -- `stale` -- costs a full registry walk to compute. The verification
        // moves to `conc_bits_known_clear`'s own test.
        if !self.mark_clear_all() && (!known_clear || cfg!(debug_assertions)) {
            let registered = self.registry.snapshot();
            snapshot_us = clock.lap();
            // ONE `bases()` call: it materialises a `Vec` of every registered
            // base, so calling it again just to count would double a
            // multi-megabyte allocation inside the pause being measured.
            let bases = registered.bases();
            n_registered = bases.len();
            for base in bases {
                let h = self.header_mut(base as *mut u8);
                if h.gc_flags() & GC_FLAG_MARKED != 0 {
                    stale += 1;
                }
                h.clear_gc_flags(GC_FLAG_MARKED);
            }
            clearbits_us = clock.lap();
            debug_assert!(
                !known_clear || stale == 0,
                "zgc: conc_bits_known_clear was set but {stale} registered objects \
                 carried GC_FLAG_MARKED into mark start"
            );
        }
        // Whatever the state was, it is clear now and this cycle is about to
        // start setting bits again -- so the claim is false from here until the
        // sweep re-establishes it.
        self.conc_bits_known_clear.store(false, Ordering::Release);
        // zt t34: the R6 plan's inputs are booked from here, before the first
        // claim or blackening of the cycle (`ZMarkLiveness`).
        self.crl_liveness_arm_for_mark();

        // (2)
        let _skip = self.begin_concurrent_mark_cycle();

        // (3). Must follow (1): this is what makes the next allocation black.
        // zr z7/cm: the SATB filter's per-cycle base, before the barrier arms.
        self.counters
            .satb_filtered_at_start
            .store(self.satb_filtered_total(), Ordering::Relaxed);
        self.set_mark_active(true);
        // (3') zt t22/zb5 (`CRATONVM_ZGC_COLOR_FLIP`, opt-in under colored
        // stores): the mark flip -- after SATB arms (the load barrier's marks
        // feed the same ingress), before any mutator resumes. Nothing below
        // can decline the cycle (the pool was spawned in (0)), so the closing
        // pause's remap flip always follows. No-op unless armed.
        self.color_flip_to_mark();
        // (3a) zr z1/mk: start recording the native collection-overlay rows the
        // mutators WRITE during this phase. See `overlay_rows_written_during_mark`.
        self.begin_overlay_row_record();
        // ...and (3b), which must follow (3) for the same reason: blacken the
        // un-handed-out tail of every live TLAB chunk, so the objects those
        // threads bump into next are born black WITHOUT a per-object atomic.
        // Every mutator is parked here, which is what makes `reserved_tail`
        // race-free. See `blacken_live_tlab_tails`.
        self.blacken_live_tlab_tails();
        // zr z7/cm: (2)-(3b) apart from the pool spawn. `poolspawn_us` used to
        // carry the skip-set snapshot, the overlay record and the TLAB-tail
        // blackening as well, so a large figure could not be attributed.
        let open_us = clock.lap();

        // (4)
        // zt t9/x6 (review of t8/rl2, `zgc-zs3ac-*` item 2): the concurrent
        // pool's workers price themselves by THIS heap's accounting seed, as
        // the stop-the-world pool's do since t8 (`mark_parallel_stw`); the
        // pool read the process flag at its construction, so a heap seeded off
        // still booked `mark_cpu_*` from its concurrent workers. Identical
        // whenever the seed is the process flag (every launcher run).
        let cpu_on = self.collection_cpu_accounting_on();
        let coordinator = match early_pool {
            Some(pool) => {
                let weak: std::sync::Weak<ZgcRealHeap> = std::sync::Arc::downgrade(&me);
                drop(me);
                let weak: std::sync::Weak<dyn mark::ZMarkContext> = weak;
                pool.set_cpu_accounting(cpu_on);
                pool.begin_cycle_with_weak(weak);
                pool
            }
            None => {
                // `=0`: the z3 construction over the heap's own `Arc`.
                let ctx: std::sync::Arc<dyn mark::ZMarkContext> = me;
                let pool = std::sync::Arc::new(mark::ZMarkCoordinator::new(ctx, workers));
                pool.set_cpu_accounting(cpu_on);
                pool.begin_cycle();
                pool
            }
        };
        let pool_us = clock.lap() + spawn_us;
        // ---- (4b) ARM THE EXTRA-ROOT FILTER FOR THE CONCURRENT PHASE -----
        //
        // *Stage 3a of `handoff-r-wire-the-concurrent-root-filter.md`.* Until
        // wave 4 the filter was rebuilt in exactly one place -- the head of
        // `collect_garbage` -- so it was armed for the stop-the-world mark and
        // disarmed for the whole concurrent phase, i.e. for the phase
        // concurrent marking exists to create. The optimisation that halved
        // `mark_us` did not apply where N workers take the same global
        // `RwLock`s once per marked object.
        //
        // HERE and not earlier: `rebuild()` publishes a snapshot, and every
        // `false` taken against that snapshot is a PROOF. It must exist before
        // the first probe, and the first probe is `push_roots` below. It must
        // also follow `set_mark_active(true)` -- above -- so that no mutator
        // can register a root between the snapshot and the barrier going up
        // without the SATB machinery seeing it.
        //
        // WHAT MAKES IT SOUND is not this call. It is the pair at mark end:
        // `ZMarkRootFilter::late_owners()`, harvested in `collect_garbage`
        // BEFORE the stop-the-world `rebuild()` retires this snapshot's key
        // sets, and pushed through `push_roots` before `mark_to_completion`.
        // A generation check protects proofs taken AFTER a mismatch and does
        // nothing for the ones already taken: a loader scanned at T with the
        // filter answering "owns nothing", which defines a class at T+1, has a
        // mirror the marker will never revisit because the loader is already
        // marked. `late_owners()` roots that mirror directly. Read section 2 of
        // the handoff for the four-case argument; case 3 is the one this
        // repairs and case 4 is the one it refuses.
        //
        // DEFAULT ON since zr z7/cm (`CRATONVM_ZGC_MARK_ROOT_FILTER_CONC`,
        // `zgc-mark-root-filter-conc`, `=0` kill): disarmed, every object the
        // pool marked took three global `RwLock` reads and a shared counter
        // that the stop-the-world mark of the same graph does not. See
        // `zgc_conc_root_filter`.
        // zt t1/mk (residual 10 of `zgc-zs2mc2-mark-engine-review-residuals`):
        // the decision is LATCHED for this cycle, and the mark-end harvest
        // reads the latch, not the mutable switch. Two reads of
        // `conc_root_filter` let a mid-cycle `set_conc_root_filter(false)`
        // skip the repair of proofs this phase took (the unsound direction),
        // and a mid-cycle `true` diff against the previous collection's
        // stop-the-world bank. No production caller flips it mid-cycle; the
        // latch makes that a non-question.
        let filter_on = self.conc_root_filter_armed();
        self.counters
            .conc_cycle_root_filter
            .store(filter_on, Ordering::Relaxed);
        if filter_on {
            // zr z8/rv7: `_within` records which `loader_pin` ids are THIS
            // heap's, so the mark-end repair can ignore another VM's removals
            // (`CRATONVM_ZGC_ROOT_FILTER_REPAIR_OWN_ARENA`).
            self.counters
                .mark_root_filter
                .rebuild_within(self.arena_base..self.arena_end);
        }
        let marked_roots = coordinator.push_roots(roots);
        coordinator.start_marking();
        let roots_us = clock.lap();
        *self.counters.conc_pool.lock() = Some(coordinator);
        self.conc_cycle_active.store(true, Ordering::Release);
        self.counters
            .conc_cycles_started
            .fetch_add(1, Ordering::Relaxed);
        self.counters
            .conc_mark_started_at
            .store(Self::monotonic_nanos(), Ordering::Relaxed);
        if let Some(t0) = stw_started {
            self.counters.director.note_stw_pause(t0.elapsed());
        }

        if let Some(t0) = started_at {
            eprintln!(
                "[GC] zgc-markstart: pause_us={} snapshot_us={snapshot_us} \
                 clearbits_us={clearbits_us} open_us={open_us} poolspawn_us={pool_us} \
                 roots_us={roots_us} \
                 registered={n_registered} stale_marked={stale} roots={} \
                 marked_roots={marked_roots} workers={workers} pool_reused={}",
                t0.elapsed().as_micros(),
                roots.len(),
                reused_pool as u8,
            );
        }
        tracing::debug!(
            target: "zgc",
            workers,
            roots = roots.len(),
            marked_roots,
            allocated = self.allocated.load(Ordering::Relaxed),
            threshold = self.gc_threshold,
            "zgc concurrent mark: cycle OPEN -- mutators resume, the pool traces"
        );
        true
    }

    /// t1 wave 2 lane Z (`CRATONVM_ZGC_CONC_POOL_REUSE`): take the pool the
    /// last certified cycle parked, if it has `workers` threads and was not
    /// stopped. `None` with the switch off (one flag read per mark start).
    ///
    /// t1 wave 6 lane Y4 (V4 findings 2 and 3): a pool this start cannot use
    /// (the wrong size, stopped, or parked before the switch was turned off)
    /// is no longer JOINED here, inside the mark-start pause: it is told to
    /// stop and retired ([`Self::retire_conc_pool`]), and its already-exited
    /// workers are reaped later. Nothing is dropped while `conc_pool_idle`
    /// is locked: each `let` below releases the guard at its semicolon.
    pub fn take_idle_conc_pool(
        &self,
        workers: usize,
    ) -> Option<std::sync::Arc<mark::ZMarkCoordinator>> {
        let parked = self.counters.conc_pool_idle.lock().take();
        let pool = parked?;
        if pool.worker_count() != workers.max(1) || pool.is_stopped() {
            self.retire_conc_pool(pool);
            return None;
        }
        self.counters
            .conc_pool_reuses
            .fetch_add(1, Ordering::Relaxed);
        Some(pool)
    }

    /// t1 wave 6 lane Y4 (V4 finding 3): stop `pool` without joining and keep
    /// it in `conc_pool_retired` until a later, cheaper point joins it. The
    /// pool it replaces there was stopped at least one mark start ago, so
    /// dropping it (after the guard is released) only reaps exited threads.
    /// Only heap-free pools are ever parked, so a retired pool holds no heap.
    pub fn retire_conc_pool(&self, pool: std::sync::Arc<mark::ZMarkCoordinator>) {
        pool.stop();
        let older = self.counters.conc_pool_retired.lock().replace(pool);
        drop(older);
    }

    /// t1 wave 3 lane Q (R-5 of `t1-Z-concurrent-relocation-and-satb-review-residuals`):
    /// drop a refused closing pause's concurrent pool and book its worker CPU
    /// into `mark_cpu_conc_pool_ns` and `mark_cpu_conc_abandoned_ns`, as the
    /// root-filter refusal of `finish_concurrent_mark` does. Read after the
    /// drop, so a LOWER BOUND for the reason `abandon_concurrent_mark` gives.
    pub fn book_refused_conc_pool_cpu(&self, pool: std::sync::Arc<mark::ZMarkCoordinator>) {
        let shared = std::sync::Arc::clone(pool.shared());
        drop(pool);
        // Zero under `CRATONVM_ZGC_MARK_CPU_ACCOUNTING=0`.
        let cpu = shared.stats().worker_cpu_ns.load(Ordering::Relaxed);
        self.counters
            .mark_cpu_conc_pool_ns
            .fetch_add(cpu, Ordering::Relaxed);
        self.counters
            .mark_cpu_conc_abandoned_ns
            .fetch_add(cpu, Ordering::Relaxed);
    }

    /// t1 wave 2 lane Z: park a closed cycle's pool for the next mark start
    /// ([`Self::take_idle_conc_pool`]). Kept only when the switch is on, the
    /// pool is heap-free between cycles (built over the inert context), not
    /// stopped, and `pool` is its ONLY reference -- the closing pause took it
    /// out of `conc_pool`, so no other clone can be made, and a mutator still
    /// holding one from `hand_satb_batch_to_the_marker` would otherwise hand
    /// the closed cycle's addresses to the next. Anything else drops it, which
    /// stops and joins its workers, as before.
    pub fn park_idle_conc_pool(&self, pool: std::sync::Arc<mark::ZMarkCoordinator>) {
        // t1 wave 6 lane Y4: reap a pool a mark start retired. Its workers
        // were told to stop a whole concurrent cycle ago, so this join only
        // collects exited threads. Taken out under the lock, dropped after.
        let retired = self.counters.conc_pool_retired.lock().take();
        drop(retired);
        if !pool.is_heap_free_between_cycles()
            || pool.is_stopped()
            || std::sync::Arc::strong_count(&pool) != 1
        {
            return;
        }
        // V4 finding 2: a previously parked pool (normally none: the mark
        // start empties the slot) is joined AFTER the guard is released.
        let replaced = self.counters.conc_pool_idle.lock().replace(pool);
        drop(replaced);
    }

    /// t1 wave 2 lane Z: mark ends that refused certification because a SATB
    /// thread-buffer flush raced the drain (`CRATONVM_ZGC_SATB_FLUSH_PAUSE_SAFE`).
    pub fn satb_flush_races(&self) -> usize {
        self.counters.satb_flush_races.load(Ordering::Relaxed)
    }

    /// t1 wave 2 lane Z: mark starts that reused a parked concurrent pool.
    pub fn concurrent_pool_reuses(&self) -> usize {
        self.counters.conc_pool_reuses.load(Ordering::Relaxed)
    }

    /// Start the per-phase record of written collection-overlay rows: turn the
    /// providers' written-key record on and discard what it holds for THIS
    /// heap's owners, so the mark-end harvest sees only this phase's writes.
    /// No-op unless a provider has rows and `CRATONVM_ZGC_CONC_OVERLAY_REPAIR`
    /// is on. Mutators are stopped (mark-start safepoint). zr z1/mk.
    pub fn begin_overlay_row_record(&self) {
        if !zgc_conc_overlay_repair() || !cratonvm_gc_core::external_roots::has_row_providers() {
            return;
        }
        cratonvm_gc_core::external_roots::enable_owner_row_dirty_tracking();
        let (lo, hi) = (self.arena_base, self.arena_end);
        let _ =
            cratonvm_gc_core::external_roots::take_dirty_owner_row_keys(&|a| (lo..hi).contains(&a));
    }

    /// zt t7/cc: add one mark end's overlay repair to the cumulative
    /// engagement counters ([`Self::concurrent_overlay_repair_totals`]).
    pub fn note_overlay_repair(&self, rows: usize, refs: usize) {
        self.counters
            .conc_overlay_repair_rows
            .fetch_add(rows as u64, Ordering::Relaxed);
        self.counters
            .conc_overlay_repair_refs
            .fetch_add(refs as u64, Ordering::Relaxed);
    }

    /// The references held by collection-overlay rows a mutator WROTE during
    /// the concurrent phase, for owners that are marked: `(rows, refs)`.
    ///
    /// # The SATB hole this closes (zr z1/mk, 2026-10-12)
    ///
    /// An overlay row is not a heap slot, so no SATB pre-write barrier sees a
    /// reference leave or enter one. Snapshot-at-the-beginning is only sound
    /// when every slot a reference can leave through is barriered, and this
    /// one is not: `listB.add(listA.removeFirst())` on two native-backed
    /// `LinkedList`s, with `B` already scanned and `A` not yet, moves `v` out
    /// of the only row the marker would have found it in and into a row the
    /// marker has already read. `A` is scanned later and no longer holds `v`;
    /// `B` is never revisited; `v` is swept while `listB` is live. Only an
    /// opt-in concurrent cycle (`CRATONVM_ZGC_CONC_START`) has a phase to race.
    ///
    /// **Reachable only on the synthetic class library** (zt t7/cc,
    /// 2026-10-19). Over a real JDK no Java program writes an overlay row: a
    /// native-collections `Bridge` never runs in front of concrete JDK
    /// bytecode, in either mode, whatever `CRATONVM_COMPAT_ALIGN` says --
    /// every door that would let it (`admit_forced_native_id`'s
    /// `resolve_native_dispatch_wave1(.., true, true)`, the cached virtual
    /// door's `resolve_cached_native_registration_with_cm` -> `resolve_dispatch`)
    /// declines a non-`Intrinsic` registration over code. 35 concurrent cycles
    /// of `Zt7CcOverlayRepair` with `-jdk-hash-collections` read
    /// `overlay_repair_rows_total=0`. The unit test below is its coverage.
    ///
    /// The repair is the one G1's overlay remembered set already uses: the
    /// providers record every row they hand out mutably (`OverlayRows`), and
    /// at mark end -- mutators stopped -- the rows written since mark start are
    /// re-read and their references pushed as roots. An owner that is not
    /// marked needs nothing here: if the closure reaches it later, its scan
    /// reads the row as it stands now. A written row of an owner that died is
    /// skipped; a row of an owner marked but not yet scanned is pushed anyway,
    /// harmlessly (`push_roots` drops what is already marked).
    ///
    /// Rowless providers (`owner_rows: None`) keep no record and are not
    /// covered; production's `native-collections` provider has rows.
    /// `CRATONVM_ZGC_CONC_OVERLAY_REPAIR=0` turns the record and the harvest
    /// off.
    pub fn overlay_rows_written_during_mark(&self) -> (usize, Vec<u64>) {
        if !zgc_conc_overlay_repair() || !cratonvm_gc_core::external_roots::has_row_providers() {
            return (0, Vec::new());
        }
        let (lo, hi) = (self.arena_base, self.arena_end);
        let keys =
            cratonvm_gc_core::external_roots::take_dirty_owner_row_keys(&|a| (lo..hi).contains(&a));
        if keys.is_empty() {
            return (0, Vec::new());
        }
        let mut rows = 0usize;
        let mut refs: Vec<u64> = Vec::new();
        for (_provider, row) in cratonvm_gc_core::external_roots::owner_rows_for_keys(&keys) {
            if row.owner == 0 || !self.registry.contains(row.owner) || !self.mark_is_set(row.owner)
            {
                continue;
            }
            rows += 1;
            refs.extend(row.refs.iter().map(|r| r.as_ptr() as u64));
        }
        (rows, refs)
    }

    /// Close a concurrent mark cycle at the collection safepoint, producing a
    /// mark set the sweep may trust.
    ///
    /// Returns `Some(off_heap_children)` when the mark set is certified
    /// complete, `None` when it is not -- and `None` obliges the caller to
    /// mark from scratch, because a sweep against an uncertified mark set is a
    /// use-after-free. Either way the cycle is closed and the barrier is
    /// disarmed when this returns.
    ///
    /// # Why this is not simply `try_end_mark`
    ///
    /// Three things must reach the marker before the fixed point can be
    /// believed, and only the first is `try_end_mark`'s business:
    ///
    /// * **the mutator ingress** -- every reference the SATB barrier recorded.
    ///   `ZMarkCoordinator` has an ingress of its own, but this heap's barrier
    ///   feeds `self.counters.mark_ingress` (it predates the pool and is reachable from
    ///   `&self` with no handle), so the replay is explicit.
    /// * **the roots, again** -- see the section header's point 2.
    /// * **the closure of both** -- and mutators are stopped here, so the
    ///   producer set is empty and `mark_to_completion`'s contract ("the
    ///   caller can guarantee every mutator is stopped") is met exactly.
    pub fn finish_concurrent_mark(
        &self,
        roots: &[ObjectRef],
        late: Option<mark_roots::ZLateRoots>,
    ) -> Option<usize> {
        // zt t22/zb5: the remap flip, first thing in the closing pause, on
        // every exit of this function (and before the sweep / slide the caller
        // runs next). One relaxed-cost load when the flip is not armed.
        self.color_flip_to_remap();
        // zt t24/zc8 (`CRATONVM_ZGC_MARK_CONTINUE`, opt-in): every collection
        // closes the open cycle's Concurrent Mark Continue log here (its last
        // continue phase ends, its failed attempts become this collection's
        // `-Xlog:gc,phases` lines, the per-cycle count restarts). One relaxed
        // load with the switch off.
        self.mark_continue_close_cycle();
        // zt t31/zc11 (1f R6): this collection's figure, set below only on a
        // certified close.
        self.counters
            .last_closed_conc_mark_nanos
            .store(0, Ordering::Relaxed);
        if !self.conc_cycle_active.load(Ordering::Acquire) {
            return None;
        }
        let started = self.counters.conc_mark_started_at.load(Ordering::Relaxed);
        // THIS CYCLE's concurrent phase, kept rather than folded away: it is
        // `window_actual_ms` on the mark-end line, the other half of
        // `window_target_ms`. Before `handoff-i-conc-window-fit.md` it was added
        // to `conc_phase_nanos` and dropped, so the per-cycle error term was
        // recoverable only by differencing a cumulative counter across lines.
        let mut window_actual_nanos: u64 = 0;
        if started != 0 {
            let elapsed = Self::monotonic_nanos().saturating_sub(started);
            window_actual_nanos = elapsed;
            self.counters
                .conc_phase_nanos
                .fetch_add(elapsed, Ordering::Relaxed);
            // ...and into the average the ADAPTIVE window is sized from --
            // below, once the safepoint drain says whether the phase reached
            // its fixed point (zr z7b/cm). See `refresh_adaptive_conc_start`.
        }
        let coordinator = self.counters.conc_pool.lock().take();
        // zr z7b/cm: did the pool reach its fixed point before this
        // collection arrived? Read BEFORE the replay below re-arms it.
        let caught_up_at_entry = coordinator
            .as_ref()
            .map_or(true, |c| c.shared().terminator().is_terminated_hint());
        // The exits that never reach the drain feed the raw phase length.
        let note_raw = |me: &Self| me.note_conc_mark_duration(window_actual_nanos);
        // Whatever happens below, this cycle is over: the flag is cleared here
        // so no early return can leave the collector believing a pool it has
        // dropped is still tracing.
        self.conc_cycle_active.store(false, Ordering::Release);

        let Some(coordinator) = coordinator else {
            note_raw(self);
            self.set_mark_active(false);
            self.end_concurrent_mark_cycle();
            return None;
        };
        // zt t23/zc6 (stage 1f census, every arm): what the pool had marked
        // and scanned when this collection arrived, the closing drive's clock,
        // and what this pause pushes / newly claims below. One stats snapshot
        // and one clock pair per cycle (`zgc/mark_end.rs`).
        let mew_entry = coordinator.stats().snapshot();
        let mew_t0 = std::time::Instant::now();
        let mut mew_pushed = 0usize;
        let mut mew_claimed = 0usize;

        // Refresh the skip set. References registered DURING the concurrent
        // phase are not in the mark-start snapshot, so their referents were
        // traced as strong edges and survive this cycle -- floating garbage,
        // and legal. From here on the pause should use the current set, so the
        // resurrection passes below and this drain agree about what a referent
        // slot is.
        //
        // zr z2/cc: the SOFT rows leave the set first -- hidden for the
        // concurrent phase only. Their kept referents are pushed below.
        let soft_rows = std::mem::take(&mut *self.counters.vm_conc_soft_skip.lock());
        let _skip = self.begin_concurrent_mark_cycle_withdrawing(&soft_rows);

        // ---- replay the mutator ingress ---------------------------------
        //
        // BEFORE disarming the barrier, so nothing that a mutator published on
        // its way into this safepoint is stranded. Mutators are stopped, so
        // this drain is final.
        let mut pending: Vec<u64> = Vec::new();
        // t1 wave 2 lane Z (`CRATONVM_ZGC_SATB_FLUSH_PAUSE_SAFE`, default ON):
        // a thread-buffer flush whose batch left its buffer before this pause
        // and is not yet published would land after the drain below (cleared
        // unread, or claimed by a pool whose fixed point is certified, never
        // scanned). Wait for in-flight flushes first, and remember the epoch:
        // a flush begun after this point refuses certification below.
        let satb_flush_check = true;
        let satb_quiet = !satb_flush_check
            || self
                .counters
                .satb_thread_bufs
                .wait_for_flushes(std::time::Duration::from_millis(Z_SATB_FLUSH_WAIT_MS));
        let satb_epoch = self.counters.satb_thread_bufs.flush_epoch();
        let replayed = self.drain_mark_ingress(&mut pending);
        self.set_mark_active(false);
        if replayed > 0 {
            self.counters
                .conc_ingress_replayed
                .fetch_add(replayed, Ordering::Relaxed);
        }
        // `push_roots` is the right verb for these: it gates on `is_in_heap`,
        // claims each with the atomic `try_mark`, and distributes them across
        // the stripes. An address that a worker already marked is dropped,
        // which is exactly the dedup this needs.
        mew_pushed += pending.len();
        mew_claimed += coordinator.push_roots(&pending);

        // ---- re-scan the roots ------------------------------------------
        let root_addrs: Vec<u64> = roots.iter().map(|r| r.as_ptr() as u64).collect();
        mew_pushed += root_addrs.len();
        mew_claimed += coordinator.push_roots(&root_addrs);

        // ---- the overlay rows written during the phase (zr z1/mk) ---------
        let (overlay_rows, overlay_refs) = self.overlay_rows_written_during_mark();
        if !overlay_refs.is_empty() {
            mew_pushed += overlay_refs.len();
            mew_claimed += coordinator.push_roots(&overlay_refs);
        }
        // zt t7/cc: the cumulative engagement, logged or not.
        self.note_overlay_repair(overlay_rows, overlay_refs.len());

        // ---- the soft referents the policy kept (zr z2/cc) ---------------
        // `kept_soft_referent_roots`: the edges of every soft `Reference` the
        // phase marked with its slot hidden. Condemned slots are null by now
        // (the VM's pre-collection pass ran before this collection).
        let soft_roots = if soft_rows.is_empty() {
            Vec::new()
        } else {
            self.kept_soft_referent_roots(&soft_rows)
        };
        if !soft_roots.is_empty() {
            mew_pushed += soft_roots.len();
            mew_claimed += coordinator.push_roots(&soft_roots);
        }

        // ---- REPAIR THE FILTER'S NEGATIVE ANSWERS ------------------------
        //
        // *Stage 3b of `handoff-r-wire-the-concurrent-root-filter.md`.* `late`
        // is the difference between the snapshot the filter published at mark
        // start and the pin tables as they stand now, harvested by
        // `collect_garbage` before its own `rebuild()` retired that snapshot.
        //
        // An owner registered DURING the phase was not in the snapshot, so the
        // filter may have answered a wrong `false` for it -- and an owner
        // already scanned is already marked and is never revisited. Pushing the
        // VALUE (the mirror, the metadata root, the overlay root, or for
        // `loader_pin` the defining loader address) is the repair: it roots the
        // edge directly rather than trying to re-scan an owner nothing will
        // claim again.
        //
        // `push_roots` is the right verb -- it gates on `is_in_heap`, claims
        // with the atomic `try_mark` and drops an address a worker already
        // took -- and it must run BEFORE `mark_to_completion` so the workers
        // trace the transitive closure of what it adds.
        //
        // The over-approximation is named rather than hidden: a late owner's
        // values are pushed whether or not the owner itself is reachable, so an
        // unreachable late owner's values survive one extra cycle. That is
        // floating garbage, which SATB already produces by the cycleful; it is
        // the safe direction; and it is bounded by the size of a difference set
        // that is empty on almost every collection.
        let mut late_owner_addrs = 0usize;
        let mut late_sources_moved = 0u32;
        // zt t1/mk: late additions of ANOTHER heap the harvest screened out
        // (`ZLateRoots::screened_foreign`); `late_foreign=` on the line below.
        let mut late_foreign = 0usize;
        if let Some(late) = late {
            late_owner_addrs = late.addrs.len();
            late_sources_moved = late.sources_moved;
            late_foreign = late.screened_foreign;
            if !late.certifiable {
                // CASE 4, and it is unrepairable rather than merely awkward: a
                // row that was in the snapshot and is gone now means an owner
                // could have been added AND removed inside the phase, so a
                // wrong `false` may have been taken against it and there is
                // nothing left to enumerate. The safe response is the fallback
                // this function already has for an uncertified mark set, not a
                // best-effort repair -- sweeping against a mark set built on an
                // unrepairable proof frees reachable objects.
                tracing::error!(
                    target: "zgc",
                    sources_moved = late.sources_moved,
                    // zs s2/mc2: WHICH source refused (1 loader, 2 mirror,
                    // 4 metadata, 8 overlay owner removed, 16 overlay provider
                    // stopped enumerating) -- `mark_roots::Z_LATE_REFUSED_*`.
                    refused = late.refused,
                    "zgc concurrent mark: a pin registry row was REMOVED (or an \
                     overlay provider stopped enumerating its owners) during the \
                     concurrent phase, so the extra-root filter's negative answers \
                     cannot be repaired -- declining to certify this mark set and \
                     falling back to a stop-the-world mark"
                );
                self.counters
                    .parallel_mark_fallbacks
                    .fetch_add(1, Ordering::Relaxed);
                note_raw(self);
                coordinator.end_cycle();
                self.end_concurrent_mark_cycle();
                // zs s2/mc2 (s1/mc finding 13): this refused cycle's pool CPU
                // was missing from `mark_cpu_totals()`. Read after the drop,
                // as `abandon_concurrent_mark` does (a lower bound for the
                // same reason), and booked as abandoned: CPU with no mark set
                // to show.
                let shared = std::sync::Arc::clone(coordinator.shared());
                // The pool drops here; `ZMarkCoordinator::drop` joins every
                // worker. Same exit as the `!mark_set_complete()` arm below.
                drop(coordinator);
                // Zero under `CRATONVM_ZGC_MARK_CPU_ACCOUNTING=0`.
                let cpu = shared.stats().worker_cpu_ns.load(Ordering::Relaxed);
                self.counters
                    .mark_cpu_conc_pool_ns
                    .fetch_add(cpu, Ordering::Relaxed);
                self.counters
                    .mark_cpu_conc_abandoned_ns
                    .fetch_add(cpu, Ordering::Relaxed);
                // zt t23/zc6: a closing pause that fell back to a full mark.
                self.note_mark_end_fallback();
                return None;
            }
            if !late.addrs.is_empty() {
                mew_pushed += late.addrs.len();
                mew_claimed += coordinator.push_roots(&late.addrs);
            }
        }

        // ---- drive to a certified fixed point ---------------------------
        //
        // Snapshot `objects_scanned` on BOTH sides of this, because the
        // difference is the one number that says whether the concurrent phase
        // did its job: everything the pool had not finished tracing by the time
        // the collection arrived is traced HERE, inside the pause. A cycle
        // whose window was too short looks identical to one whose window was
        // ample in every other figure.
        let before = coordinator.stats().snapshot();
        let scanned_before = before.objects_scanned;
        let park_timeouts0 = coordinator.shared().terminator().park_timeouts();
        // zt t31/zc11 (`CRATONVM_DBG_ZGC_SOFT_ROWS`, debug): which soft rows
        // the phase had marked before the closing drain (read below).
        let soft_dbg = zgc_dbg_soft_rows_enabled();
        let soft_marked_before: Vec<bool> = if soft_dbg {
            soft_rows
                .iter()
                .map(|&r| r != 0 && self.mark_is_set(r))
                .collect()
        } else {
            Vec::new()
        };
        let report = coordinator.mark_to_completion(Z_CONC_MARK_END_RESTART_BUDGET);
        if soft_dbg {
            self.dbg_soft_rows_after_drain(&soft_rows, &soft_marked_before);
        }
        // zs s2/mc2: this wait is at a safepoint too, so a park TIMEOUT here
        // is the same "a notification is missing" signal the stop-the-world
        // pool folds into `mark_park_timeouts`; it was dropped with the pool.
        // The delta across this call only: the mark-end drive's own waits.
        self.counters.mark_park_timeouts.fetch_add(
            coordinator
                .shared()
                .terminator()
                .park_timeouts()
                .saturating_sub(park_timeouts0) as usize,
            Ordering::Relaxed,
        );
        let stats = coordinator.stats().snapshot();
        let scanned_at_safepoint = stats.objects_scanned.saturating_sub(scanned_before);
        // zr zs s1/mc: the concurrent pool's CPU, whole cycle, and the share
        // charged after the collection arrived (a lower bound on the mark-end
        // drain's: a worker still busy at entry charges its stretch later).
        self.counters
            .mark_cpu_conc_pool_ns
            .fetch_add(stats.worker_cpu_ns, Ordering::Relaxed);
        let end_cpu_ns = stats.worker_cpu_ns.saturating_sub(before.worker_cpu_ns);
        // zr z7b/cm: THE DURATION THE WINDOW IS SIZED FROM. A phase that the
        // collection cut short is not a measurement of how long the mark
        // takes -- it is a measurement of how short the window was. Feeding it
        // back made the next window shorter still (a ratchet down to the
        // ceiling). When the pool had not caught up, scale the phase by the
        // share of the work it got through: `elapsed * total / concurrent`,
        // capped at 4x. A phase that caught up feeds its elapsed time (an
        // over-estimate of the mark: the safe direction, a slightly earlier
        // open). `CRATONVM_ZGC_CONC_WINDOW_EXTRAPOLATE=0`: always elapsed.
        let needed_nanos = if !caught_up_at_entry
            && self
                .counters
                .conc_window_extrapolate
                .load(Ordering::Relaxed)
            && scanned_before > 0
            && scanned_at_safepoint > 0
        {
            let total = (scanned_before + scanned_at_safepoint) as u128;
            let scaled = window_actual_nanos as u128 * total / scanned_before as u128;
            scaled.min(window_actual_nanos as u128 * 4) as u64
        } else if caught_up_at_entry {
            // Use the workers' active mark time (arm -> fixed point) rather than
            // `window_actual_nanos`, which includes the safety headroom and idle
            // wait after termination and otherwise compounds `SAFETY_NUMERATOR`.
            let active = coordinator.shared().terminator().active_mark_nanos();
            if active > 0 {
                active.min(window_actual_nanos)
            } else {
                window_actual_nanos
            }
        } else {
            window_actual_nanos
        };
        self.note_conc_mark_duration(needed_nanos);
        // ---- THE AGGREGATE, which is what makes the bar answerable ---------
        //
        // *Applies `handoff-i-conc-window-fit.md`.* The per-cycle pair below is
        // already printed; `proposal-a-concurrent-mark-by-default.md`'s step-3
        // bar is about the MEDIAN cycle, which a log line cannot answer without
        // post-processing a run that had logging on. Two relaxed adds make it a
        // single ratio the process can report at exit -- see
        // `Self::concurrent_window_fit`.
        //
        // Unconditional, NOT behind `gc_log_enabled`: the whole point is that
        // the figure survives a run with logging off, which is the only run the
        // number is about.
        self.counters
            .conc_scanned_concurrently
            .fetch_add(scanned_before as u64, Ordering::Relaxed);
        self.counters
            .conc_scanned_at_safepoint
            .fetch_add(scanned_at_safepoint as u64, Ordering::Relaxed);
        if self.gc_log_enabled.load(Ordering::Relaxed) {
            // zt t2/x1 (finding X1-7): t1/mk split the ingress counter
            // (`concurrent_ingress_split`) but nothing read the split; it is
            // printed here, cumulative, beside this cycle's `satb_replayed=`.
            let (ingress_handed, ingress_at_end) = self.concurrent_ingress_split();
            eprintln!(
                "[GC] zgc-markend: scanned_concurrently={scanned_before} \
                 scanned_at_safepoint={scanned_at_safepoint} marked={} \
                 window_target_ms={} window_actual_ms={} \
                 satb_replayed={replayed} passes={} restarts={} \
                 late_owners={late_owner_addrs} late_sources={late_sources_moved} \
                 overlay_repair={overlay_rows}/{} ref_chunks={} \
                 soft_rows={} soft_roots={} satb_filtered={} root_filter={} \
                 worker_cpu_us={} worker_cpu_end_us={} satb_defer_screened_total={} satb_defer_refused_total={} \
                 late_foreign={late_foreign} roots_marked={} array_splits={} array_tasks={}/{} \
                 ingress_handed_total={ingress_handed} ingress_at_end_total={ingress_at_end} \
                 satb_flush_races_total={} conc_pool_reuses_total={}",
                stats.objects_marked,
                // WHAT THE SIZING AIMED FOR, beside WHAT IT GOT. `target=0`
                // means the adaptive sizing has no opinion -- a fixed
                // `CRATONVM_ZGC_CONC_START`, or no allocation rate sampled yet
                // -- and is not a claim that it aimed for nothing. An actual
                // far below the target with a large `scanned_at_safepoint` is
                // the collection arriving early, not the mark being slow.
                self.counters
                    .conc_window_target_nanos
                    .load(Ordering::Relaxed)
                    / 1_000_000,
                window_actual_nanos / 1_000_000,
                report.passes,
                report.restarts,
                overlay_refs.len(),
                stats.ref_chunks,
                soft_rows.len(),
                soft_roots.len(),
                // zr z7/cm: records the SATB filter dropped this cycle (the
                // old value was already marked), and whether the extra-root
                // filter was armed across the phase.
                self.satb_filtered_total()
                    .saturating_sub(self.counters.satb_filtered_at_start.load(Ordering::Relaxed)),
                // zt t1/mk: the decision this cycle's mark start latched.
                self.counters
                    .conc_cycle_root_filter
                    .load(Ordering::Relaxed) as u8,
                stats.worker_cpu_ns / 1_000,
                end_cpu_ns / 1_000,
                // zr zs s1/mc: cumulative, `CRATONVM_ZGC_SATB_DEFER_REGISTRY` only.
                self.counters
                    .satb_deferred_screened
                    .load(Ordering::Relaxed),
                self.counters
                    .satb_deferred_refused
                    .load(Ordering::Relaxed),
                // zt t1/mk (zs5mc3 proposal item 3; residual 6 of the s2/mc2
                // page): the root claims `marked=` leaves out, and the split
                // arrays with their chunk tasks (pushed / run), which only the
                // stop-the-world `[GC] zgc-mark-parallel:` line printed.
                stats.roots_marked,
                stats.array_splits,
                stats.array_tasks,
                stats.array_task_runs,
                // t1 wave 2 lane Z: cumulative; `satb_flush_races_total` is
                // expected 0 (`CRATONVM_ZGC_SATB_FLUSH_PAUSE_SAFE`), and the
                // reuses count only under `CRATONVM_ZGC_CONC_POOL_REUSE`.
                self.counters.satb_flush_races.load(Ordering::Relaxed),
                self.counters.conc_pool_reuses.load(Ordering::Relaxed),
            );
        }
        coordinator.end_cycle();
        self.end_concurrent_mark_cycle();

        // t1 wave 2 lane Z (`CRATONVM_ZGC_SATB_FLUSH_PAUSE_SAFE`): a flush
        // that outlived the wait, or began after it, may have published its
        // batch after the drain -- its objects are not in this mark set.
        // Fail closed: the stop-the-world mark below clears the bits and
        // marks from the roots, which no late batch can affect.
        if satb_flush_check
            && (!satb_quiet
                || self.counters.satb_thread_bufs.flushes_in_flight() != 0
                || self.counters.satb_thread_bufs.flush_epoch() != satb_epoch)
        {
            tracing::warn!(
                target: "zgc",
                quiet = satb_quiet,
                "zgc concurrent mark: a SATB thread-buffer flush raced the mark-end drain; \
                 declining to certify this mark set and marking stop-the-world"
            );
            self.counters
                .satb_flush_races
                .fetch_add(1, Ordering::Relaxed);
            self.counters
                .parallel_mark_fallbacks
                .fetch_add(1, Ordering::Relaxed);
            // t1 wave 3 lane Q (R-5): book this refused cycle's pool CPU as
            // the root-filter refusal above does (abandoned: CPU with no mark
            // set to show), so `mark_cpu_totals()` keeps it.
            self.book_refused_conc_pool_cpu(coordinator);
            self.note_mark_end_fallback();
            return None;
        }

        // `mark_set_complete()` rather than `!budget_exhausted`: the
        // coordinator's loop can also leave because the POOL was stopped, and
        // that report used to be indistinguishable from a converged one --
        // `budget_exhausted: false`, some passes, a stats snapshot -- so this
        // caller would have certified a PARTIAL mark set and the sweep would
        // have freed reachable objects. It is not reachable today, and the
        // reason lives three call frames away (the coordinator is moved into a
        // local `Arc` above, so nothing can drop it and set `should_stop` until
        // this function returns), which is exactly the shape of argument this
        // round exists to remove. See `ZMarkCycleReport::stopped_early`.
        if !report.mark_set_complete() {
            tracing::error!(
                target: "zgc",
                passes = report.passes,
                restarts = report.restarts,
                budget_exhausted = report.budget_exhausted,
                stopped_early = report.stopped_early,
                replayed,
                "zgc concurrent mark: the mark-end handshake could not certify a complete \
                 mark set AT A SAFEPOINT -- no mutator is running, so a budget_exhausted \
                 here cannot be a mutator race, and a stopped_early here means the pool was \
                 torn down under us. Falling back to a stop-the-world mark rather than \
                 sweeping against it"
            );
            self.counters
                .parallel_mark_fallbacks
                .fetch_add(1, Ordering::Relaxed);
            // zt t23/zc6: a closing pause that fell back to a full mark.
            self.note_mark_end_fallback();
            // The pool drops here; `ZMarkCoordinator::drop` joins every worker.
            // t1 Q (R-5): its CPU is booked as abandoned, as above.
            self.book_refused_conc_pool_cpu(coordinator);
            return None;
        }

        // zt t23/zc6 (stage 1f census): the certified close's work split --
        // what the phase did before this collection arrived against what this
        // pause pushed, claimed, marked and scanned to the fixed point.
        self.note_mark_end_work(mark_end::ZMarkEndWork {
            caught_up_at_entry,
            pushed: mew_pushed,
            claimed: mew_claimed,
            conc_marked: mew_entry.objects_marked,
            end_marked: stats
                .objects_marked
                .saturating_sub(mew_entry.objects_marked),
            conc_scanned: mew_entry.objects_scanned,
            end_scanned: stats
                .objects_scanned
                .saturating_sub(mew_entry.objects_scanned),
            end_ns: u64::try_from(mew_t0.elapsed().as_nanos()).unwrap_or(u64::MAX),
        });
        self.counters
            .conc_cycles_completed
            .fetch_add(1, Ordering::Relaxed);
        self.counters
            .last_closed_conc_mark_nanos
            .store(window_actual_nanos.max(1), Ordering::Relaxed);
        // t1 wave 2 lane Z (`CRATONVM_ZGC_CONC_POOL_REUSE`): keep the pool for
        // the next mark start instead of joining its workers in this pause.
        // `end_cycle` above dropped its context. Last use of `coordinator`.
        self.park_idle_conc_pool(coordinator);
        tracing::debug!(
            target: "zgc",
            passes = report.passes,
            restarts = report.restarts,
            replayed,
            marked = stats.objects_marked,
            scanned = stats.objects_scanned,
            black_allocations = self.counters.conc_black_allocations.get(),
            "zgc concurrent mark: cycle CLOSED, mark set certified"
        );
        Some(stats.off_heap_children as usize)
    }

    /// zt t31/zc11 (program page 1f residual R6): the concurrent phase of the
    /// cycle the last collection closed with a certified mark set, in
    /// nanoseconds (Concurrent Mark Continue phases included), taken (a second
    /// call answers `0`); `0` when that collection closed no cycle. Read inside
    /// the collection's pause by the VM's event seal.
    pub fn take_last_concurrent_mark_nanos(&self) -> u64 {
        self.counters
            .last_closed_conc_mark_nanos
            .swap(0, Ordering::Relaxed)
    }

    /// Abandon an open cycle without using its mark bits.
    ///
    /// The escape hatch for any path that must not inherit a half-marked heap
    /// -- today that is VM shutdown and any collector reconfiguration.
    /// Idempotent.
    pub fn abandon_concurrent_mark(&self) {
        if !self.conc_cycle_active.swap(false, Ordering::AcqRel) {
            return;
        }
        let pool = self.counters.conc_pool.lock().take();
        self.set_mark_active(false);
        // zt t22/zb5: an abandoned cycle gives the good color back too.
        self.color_flip_to_remap();
        // zt t33: and plans nothing.
        self.end_closing_heal_color_for("abandon");
        // zt t34: nor books into the plan's liveness.
        self.crl_liveness_invalidate();
        if let Some(p) = pool {
            p.end_cycle();
            // zr zs s1/mc: an abandoned cycle's CPU is the most wasted CPU a
            // concurrent arm spends, so it is counted too -- read after the
            // drop below. zs s2/rv: a LOWER BOUND, not "after the join": a
            // mutator inside `hand_satb_batch_to_the_marker` may hold the last
            // `Arc` (and a worker running the final drop detaches instead of
            // joining), so the workers' final charges can land after this read.
            let shared = std::sync::Arc::clone(p.shared());
            // Dropping the last `Arc` stops and joins every worker; the heap's
            // `Arc` inside it is released with them.
            drop(p);
            let cpu = shared.stats().worker_cpu_ns.load(Ordering::Relaxed);
            self.counters
                .mark_cpu_conc_pool_ns
                .fetch_add(cpu, Ordering::Relaxed);
            self.counters
                .mark_cpu_conc_abandoned_ns
                .fetch_add(cpu, Ordering::Relaxed);
        }
        self.end_concurrent_mark_cycle();
        // The partial trace's mark bits are still set and NO SWEEP FOLLOWS an
        // abandon, so the next mark start must do the walk. This is the only
        // path that makes that walk necessary, and dropping this store is how
        // a retained-object bug would be introduced.
        self.conc_bits_known_clear.store(false, Ordering::Release);
        tracing::debug!(target: "zgc", "zgc concurrent mark: cycle ABANDONED");
    }

    /// Born-marked, for an object allocated while a cycle is in flight.
    ///
    /// # Why this is a correctness requirement and not a heuristic
    ///
    /// A concurrent mark computes the transitive closure of the root set **as
    /// it was at mark start**. Nothing allocated afterwards is in it. The
    /// sweep frees every registered object without a mark bit, so without this
    /// every object the mutators allocated while the marker ran would be freed
    /// while live -- the whole live set of a busy allocator, at the end of the
    /// very first concurrent cycle.
    ///
    /// # Why the marker need not scan a black object
    ///
    /// Under snapshot-at-the-beginning every reference a mutator can store
    /// into a new object was either (a) read out of an object that existed at
    /// mark start -- in which case that object is in the snapshot and is
    /// traced, or the pre-write barrier recorded the reference when the field
    /// was overwritten -- or (b) a reference to another object allocated
    /// during the cycle, which is black for the same reason. Both are live.
    /// So the new object's out-edges need no tracing THIS cycle, and skipping
    /// them also keeps the marker away from a header a mutator is still
    /// writing.
    ///
    /// # Why it is called AFTER the header is written
    ///
    /// `try_alloc_object` registers the base inside `alloc_raw_tlab` and only
    /// then `ptr::write`s the header. A `ptr::write` of the whole
    /// `ObjectHeader` clobbers the flags byte, so a bit set before it would be
    /// erased -- and erased silently, producing exactly the use-after-free
    /// this function exists to prevent. Set it after, with the atomic
    /// `add_gc_flags`.
    #[inline]
    pub fn allocate_black_if_marking(&self, ptr: *mut u8) {
        if !self.mark_active.load(Ordering::Relaxed) {
            // zt t34: an allocation of the closing pause, after the mark and
            // before the R6 plan (`ZMarkLiveness`).
            if self.counters.crl.live.armed() {
                self.crl_liveness_note_marked(ptr as usize);
            }
            return;
        }
        let addr = ptr as usize;
        self.counters.conc_black_allocations.inc();
        // ---- ALREADY BLACK? -----------------------------------------------
        //
        // `tlab_refill` blackens a whole chunk at a time and
        // `blacken_live_tlab_tails` does the same for the chunks that were
        // already live at mark start, so on the TLAB path -- which is every
        // ordinary allocation -- this is a plain load and a not-taken branch
        // instead of a `fetch_or` on a bitmap word SHARED with the 63
        // neighbouring grid slots, i.e. with whatever every other allocating
        // thread is doing nearby. The probe `Z_CONC_START_PERCENT_DEFAULT`
        // records made 22.4M of those.
        //
        // **The bitmap is its own validity check.** There is no flag to go
        // stale: if `mark_clear_all` has run since the chunk was blackened, the
        // bit is simply clear again and the claim below runs. That is the whole
        // reason this test is here rather than a `black_end` watermark on the
        // TLAB -- a watermark would have to be invalidated by a clear that
        // happens at the END of a sweep, with every chunk still live and owned,
        // and a missed invalidation is an object swept while live.
        if self.mark_is_set(addr) {
            return;
        }
        self.mark_set(addr);
        self.counters
            .conc_black_claims
            .fetch_add(1, Ordering::Relaxed);
        // zt t34: born black one object at a time (the header is written).
        if self.counters.crl.live.armed() {
            self.crl_liveness_note_marked(addr);
        }
    }

    /// A monotonic clock reading in nanoseconds, never `0` (the "no stamp"
    /// sentinel of every field it is stored in).
    ///
    /// # It is not telemetry any more, so it must be monotonic (zr zs s1/mc)
    ///
    /// This used to read `SystemTime` -- the WALL clock -- under a doc saying
    /// "nothing branches on it". Since zr z7b/cm that is false: the phase
    /// length it measures feeds `note_conc_mark_duration`, which sizes the
    /// adaptive concurrent window, and since z11/rv9 it also stamps both ends
    /// of the allocation-rate sample (`rate_nanos_at_entry`,
    /// `refresh_adaptive_conc_start`). A wall-clock step (NTP slew or step, a
    /// VM resume, a manual date change) then turns into a zero, negative or
    /// huge phase length or rate, i.e. a window opened at the wrong point.
    /// `Instant` is the monotonic clock; the epoch is the first reading in the
    /// process (a clock origin, not compatibility state).
    /// `CRATONVM_ZGC_CONC_MONOTONIC_CLOCK=0` restores the wall clock.
    pub fn monotonic_nanos() -> u64 {
        static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        static EPOCH: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
        if *ON.get_or_init(|| true) {
            let epoch = *EPOCH.get_or_init(std::time::Instant::now);
            return (epoch.elapsed().as_nanos() as u64).saturating_add(1);
        }
        use std::time::{SystemTime, UNIX_EPOCH};
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0)
    }

    /// zt t1/mk (residual 7 of the s2/mc2 review page): the
    /// `ingress_replayed` figure of [`Self::concurrent_mark_stats`] split into
    /// `(handed_during_phase, replayed_at_mark_end)`, cumulative.
    pub fn concurrent_ingress_split(&self) -> (usize, usize) {
        let total = self.counters.conc_ingress_replayed.load(Ordering::Relaxed);
        let handed = self.counters.conc_ingress_handed.load(Ordering::Relaxed);
        (handed, total.saturating_sub(handed))
    }

    /// `(started, completed, black_allocations, ingress_replayed, phase_nanos)`
    /// -- the concurrent marker's whole telemetry surface, for tests and for
    /// the `--verbose:gc` line.
    pub fn concurrent_mark_stats(&self) -> (usize, usize, usize, usize, u64) {
        (
            self.counters.conc_cycles_started.load(Ordering::Relaxed),
            self.counters.conc_cycles_completed.load(Ordering::Relaxed),
            self.counters.conc_black_allocations.get(),
            self.counters.conc_ingress_replayed.load(Ordering::Relaxed),
            self.counters.conc_phase_nanos.load(Ordering::Relaxed),
        )
    }

    /// zr z7/cm: `(satb_filtered, satb_filter_mode, root_filter_conc_on)` --
    /// SATB records dropped because the overwritten value was already marked
    /// (summed over cycles), the filter's mode (`0` off, `1` per store, `2` at
    /// the buffer flush), and whether the root filter is armed across the
    /// phase. For the `[GC] zgc-concurrent:` summary.
    pub fn concurrent_cost_report(&self) -> (usize, u8, bool) {
        (
            self.satb_filtered_total(),
            self.counters.satb_filter_marked.load(Ordering::Relaxed),
            self.conc_root_filter_armed(),
        )
    }

    /// SATB records dropped by either filter mode, summed over cycles.
    pub fn satb_filtered_total(&self) -> usize {
        self.counters.satb_filtered.get()
            + self.counters.satb_filtered_batched.load(Ordering::Relaxed)
    }

    /// Per-heap override of `CRATONVM_ZGC_SATB_FILTER_MARKED` (`0` / `1` /
    /// `2`). **Test seam** (production reads the variable once, at
    /// construction).
    #[cfg(test)]
    pub fn dbg_set_satb_filter_mode(&self, mode: u8) {
        self.counters
            .satb_filter_marked
            .store(mode, Ordering::Relaxed);
    }

    /// zt t7/cc: `(rows, refs)` the concurrent overlay repair re-read and
    /// pushed at mark end, summed over cycles (see the fields;
    /// `[GC] zgc-concurrent:` `overlay_repair_rows_total=` /
    /// `overlay_repair_refs_total=`).
    pub fn concurrent_overlay_repair_totals(&self) -> (u64, u64) {
        (
            self.counters
                .conc_overlay_repair_rows
                .load(Ordering::Relaxed),
            self.counters
                .conc_overlay_repair_refs
                .load(Ordering::Relaxed),
        )
    }

    /// `(scanned_concurrently, scanned_at_safepoint, window_target_ms)` — how
    /// well the concurrent window fitted the mark, cumulatively.
    ///
    /// *Applies
    /// `docs/feature-designs/zgc-architecture.md`, step 1
    /// of `proposal-a-concurrent-mark-by-default.md`.*
    ///
    /// **`scanned_at_safepoint / scanned_concurrently` below 0.1 is that
    /// proposal's step-3 acceptance bar**, and this accessor exists so that
    /// asking the question is one run rather than a log-parsing project. The
    /// per-cycle halves are on `[GC] zgc-markend:`; these are the sums, and
    /// they are collected whether or not `--verbose:gc` is on, because the run
    /// the number is about is the one with logging off.
    ///
    /// `scanned_concurrently = 0` means no concurrent cycle ever completed, so
    /// the ratio is about nothing — read it before the ratio. `window_target_ms
    /// = 0` means the adaptive sizing had no opinion (fixed
    /// `CRATONVM_ZGC_CONC_START`, or no allocation rate sampled).
    ///
    /// This accessor does **not** argue that the bar is met, and nothing here
    /// changes the default: `CRATONVM_ZGC_CONC_START=0` stands.
    pub fn concurrent_window_fit(&self) -> (u64, u64, u64) {
        (
            self.counters
                .conc_scanned_concurrently
                .load(Ordering::Relaxed),
            self.counters
                .conc_scanned_at_safepoint
                .load(Ordering::Relaxed),
            self.counters
                .conc_window_target_nanos
                .load(Ordering::Relaxed)
                / 1_000_000,
        )
    }

    /// Run a collection with finalizer-aware resurrection: any address in
    /// `finalizer_addrs` (registered, not-yet-enqueued finalizable objects)
    /// that the mark phase finds DEAD is marked live — with its transitive
    /// closure — so the sweep keeps it and `finalize()` can later run
    /// against valid memory. Non-moving, so the returned dead-finalizer
    /// addresses are the same addresses that went in. The caller must
    /// enqueue them for finalization AND mark their processor entries
    /// enqueued (once-only finalization) — mirrors the generational
    /// backend's contract.
    pub fn collect_garbage_with_finalizers(
        &self,
        stw: &cratonvm_gc_core::collector::StopTheWorldToken,
        roots: &mut [ObjectRef],
        finalizer_addrs: &[usize],
        monitors: &dyn MonitorCleanup,
    ) -> (GcResult, Vec<usize>) {
        *self.counters.pending_finalizer_roots.lock() = finalizer_addrs.to_vec();
        self.counters.resurrected_finalizers.lock().clear();
        self.counters.resurrection_closure.lock().clear();
        let result = <Self as cratonvm_gc_core::collector::GarbageCollector>::collect_garbage(
            self, stw, roots, monitors,
        );
        self.counters.pending_finalizer_roots.lock().clear();
        let dead = std::mem::take(&mut *self.counters.resurrected_finalizers.lock());
        (result, dead)
    }

    /// `VmHeap::take_resurrection_closure`'s ZGC arm: PRE-collection addresses
    /// of every object the last collection's finalizer-resurrection pass
    /// marked that the strong closure had not (the resurrected finalizables
    /// included). The VM's reference processor treats them as unmarked for
    /// soft and weak processing, which is HotSpot's order: a weak reference to
    /// an object reachable only through a finalizable is cleared before
    /// `finalize()` runs. Drained: a second call answers empty.
    pub fn take_resurrection_closure(&self) -> Vec<usize> {
        std::mem::take(&mut *self.counters.resurrection_closure.lock())
    }

    /// zr z5/rv5 (`CRATONVM_ZGC_SOFT_CLEAR_PRECISE_FIRST`): arm the NEXT
    /// collection -- the VM's last-ditch clear-all one -- to tell which of
    /// `candidates` (condemned soft referents, pre-collection addresses) are
    /// reachable WITHOUT the conservative compiled-frame words.
    ///
    /// That collection marks serially in two drains: first from every root
    /// that is not a conservative JIT word (`gc_quiescence::
    /// pinned_jit_roots_snapshot`), then -- after recording which candidates
    /// that first closure marked -- from the conservative words. The mark set
    /// is the same as one drain over all roots (marking is monotone), so
    /// nothing is freed differently; only the soft decision reads the answer
    /// ([`Self::take_soft_precise_kept`]). A concurrently marked cycle cannot
    /// split its mark and answers `None`. An empty list arms nothing.
    pub fn arm_soft_precise_probe(&self, candidates: Vec<usize>) {
        let candidates: Vec<usize> = candidates.into_iter().filter(|&a| a != 0).collect();
        *self.counters.soft_precise_probe.lock() = (!candidates.is_empty()).then_some(candidates);
    }

    /// zr z5/rv5: the armed candidates the last collection reached from the
    /// precise roots alone (see [`Self::arm_soft_precise_probe`]); `None` when
    /// the probe did not run. Drained.
    pub fn take_soft_precise_kept(&self) -> Option<Vec<usize>> {
        self.counters.soft_precise_kept.lock().take()
    }

    /// zr z5/rv5: `(cycles the probe ran, roots it deferred)`, cumulative.
    pub fn soft_precise_probe_stats(&self) -> (usize, usize) {
        (
            self.counters
                .soft_precise_probe_cycles
                .load(Ordering::Relaxed),
            self.counters
                .soft_precise_deferred_roots
                .load(Ordering::Relaxed),
        )
    }

    /// Register a discovered `java.lang.ref.Reference` with this heap's shared
    /// [`ReferenceProcessor`].
    ///
    /// The runtime calls this during marking (the same point the generational
    /// and G1 backends discover references) so the next
    /// [`Self::collect_garbage`] can clear/enqueue it per Java semantics.
    /// `reference_obj` is the address of the `Reference` object, `referent`
    /// the object it points at, and `queue` the associated `ReferenceQueue`
    /// (if any).
    pub fn discover_reference(
        &self,
        ref_type: ReferenceType,
        reference_obj: ObjectRef,
        referent: ObjectRef,
        queue: Option<ObjectRef>,
    ) {
        self.counters.ref_processor.lock().discover_reference(
            ref_type,
            reference_obj.as_ptr() as usize,
            referent.as_ptr() as usize,
            queue.map(|q| q.as_ptr() as usize),
        );
    }

    /// Charge `bit`'s reason for the collection that has not happened yet, at
    /// most once, and return whether this call was the one that charged it.
    ///
    /// The whole point is the *at most once*: see
    /// [`ZgcCounters::trigger_seen`] for why counting every agreeing poll
    /// made the `[GC] zgc-trigger:` line a measurement of the native-call rate.
    /// The mask is cleared by [`Self::collect_garbage`] at the same point it
    /// resets `gc_rearm` and the allocation watermark.
    #[inline]
    pub fn claim_trigger(&self, bit: usize) -> bool {
        // The hot arm: already charged, so a relaxed load and a branch. Only
        // the first agreeing poll after a collection reaches the RMW.
        if self.counters.trigger_seen.load(Ordering::Relaxed) & bit != 0 {
            return false;
        }
        self.counters.trigger_seen.fetch_or(bit, Ordering::Relaxed) & bit == 0
    }

    /// [`Self::claim_trigger`] plus the one tally that reason feeds.
    #[inline]
    pub fn note_trigger(&self, bit: usize, tally: &AtomicUsize) {
        if self.claim_trigger(bit) {
            tally.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Collections the warm-up clause admitted ([`ZGC_WARMUP_CYCLES`]).
    ///
    /// zr zs s1/tp: three doc blocks had run together above this function
    /// (`gc_count`'s, `trigger_tallies`'s and this one); each is back on its
    /// own function.
    pub fn trigger_warmup_count(&self) -> usize {
        self.counters.trigger_warmup.load(Ordering::Relaxed)
    }

    /// zr z6/tp: the `[GC] zgc-pacing:` line (which rule asked for each
    /// collection taken near full, and how much was allocated since the one
    /// before; `pace_floor_deferred` is the near-full floor's engagement).
    pub fn pacing_line(&self) -> String {
        self.counters
            .pacing
            .line(self.counters.near_full_progress_pct.load(Ordering::Relaxed))
    }
}
