#[allow(unused_imports)]
use super::*;

/// The seam `gc/src/zgc/relocate.rs` declares, implemented over the real heap.
///
/// Stage 1 of the adoption recorded in
/// `docs/feature-designs/zgc-architecture.md`,
/// which resolves the decision half of
/// `gap-b-two-relocators-one-of-them-unreachable.md`: **adopt in stages; do not
/// retire `relocate.rs`.** Its INVARIANTS are the asset -- ZR-1 is the rule the
/// shipping slide got wrong twice
/// (`zgc-relocation-slides-wrote-into-decommitted-granules-FIXED-20260904.md`,
/// `zgc-relocate-slid-survivors-over-an-unselected-page-FIXED-20260814.md`,
/// whose own summary is *"the rule was known; only the code disagreed"*) -- and
/// retiring the module would keep the prose and delete the executable statement
/// of the rule, which is the only version a future change can fail against.
///
/// **The point of this impl is that the page-based relocator stops being
/// unreachable, not that it becomes the default.** The arena slide remains the
/// shipping path. The driver ARM -- a `collect_garbage` branch running
/// `ZRelocate::relocate_stw` over the selected set and folding
/// `end_page_evac_cycle`'s pairs into the rewrite pass, the registry rebuild,
/// `counters.relocations` and the returned `PointerMap` -- is not wired; its
/// switch (`CRATONVM_ZGC_PAGE_EVAC`, never read by a shipping path) was retired
/// in zt t12/rt (`docs/feature-designs/zgc-architecture.md`).
impl relocate::ZRelocateContext for ZgcRealHeap {
    /// [`ZgcRealHeap::alloc_size`], the same figure the sweep and
    /// `walk_objects` use.
    ///
    /// `alloc_size` answers `None` for a header it cannot size, and the
    /// contract here is "return below `ZRELOCATE_MIN_OBJECT_BYTES`" -- so `0`
    /// is the right translation of `None`. That is what stops the page walk
    /// instead of striding into garbage.
    fn object_size(&self, addr: u64) -> usize {
        if addr == 0 {
            return 0;
        }
        Self::alloc_size(self.header_ref(addr as usize as *mut u8)).unwrap_or(0)
    }

    /// Served from the arena's bump-only TO-SPACE window (zr z1/rl, applying
    /// `handoff-q-arena-to-space-and-the-page-evac-arm.md` section 2a), and
    /// `None` whenever no window is open -- which is every cycle until the arm
    /// that calls [`ZgcRealHeap::begin_page_evac_cycle`] exists.
    ///
    /// The history below is why the window, and not `alloc_raw`, is the
    /// destination. `alloc_in`'s contract is explicit: to-space exhaustion "must **not** be
    /// reported by allocating out of the relocation set's own pages -- a
    /// destination inside a from-space page would be evacuated again and could
    /// alias its source."
    ///
    /// This heap has no allocator that can promise that. `Arena::alloc` serves
    /// from the **free list** before it bumps, and the sweep that runs
    /// immediately before relocation has just put the relocation set's own
    /// garbage holes on that list -- so the single most likely destination
    /// `alloc_raw` would return is a hole inside a page this cycle is about to
    /// evacuate. `Arena::alloc_high` is provably outside the low relocation set
    /// but is the large-object region, which routes by offset and is not where
    /// small survivors belong.
    ///
    /// So the missing piece was a **bump-only to-space** above the low cursor;
    /// `Arena::alloc_to_space` is it (it refuses without an open window). A
    /// `None` -- no window, or the window exhausted -- fails in the safe direction
    /// by construction: `ZRelocate::drain_page` reports the page as not
    /// drained, and ZR-1(a) then forbids marking it `Relocated`, so no byte is
    /// declared dead and nothing is freed under a live pointer.
    ///
    /// `gen_hint` is ignored: this heap's generation split is by ADDRESS
    /// (`gen_young_floor`), not by destination region.
    fn alloc_in(&self, _gen_hint: u8, bytes: usize, align: usize) -> Option<u64> {
        let p = self.arena.lock().alloc_to_space(bytes, align)?;
        Some(p as usize as u64)
    }

    /// Record the move for the cycle's `PointerMap`.
    ///
    /// **This is stage 1's single biggest integration trap** -- see
    /// [`ZgcCounters::page_evac_moves`]. Recorded structurally rather than left
    /// to the driver to remember, because an empty pointer map is not "no
    /// information": it is the positive claim that nothing moved, and
    /// `MonitorCleanup::remap_after_gc` early-returns on one.
    ///
    /// Fires exactly once per object per cycle, for the thread that performed
    /// the copy and won the publishing CAS.
    fn on_relocated(&self, from: u64, to: u64) {
        self.counters
            .page_evac_moves
            .lock()
            .push((from as usize, to as usize));
    }

    /// This cycle's mark bit, the same predicate the sweep retains on.
    fn is_live(&self, addr: u64) -> bool {
        addr != 0 && self.mark_is_set(addr as usize)
    }

    /// One object-start-bitmap probe. `ZObjectStarts` makes it `O(1)`.
    ///
    /// This is the wild-pointer / interior-pointer gate: answering `true` for
    /// an interior address would let it be decoded as a header, copied, and
    /// published under a non-base forwarding key.
    fn is_object_base(&self, addr: u64) -> bool {
        addr != 0 && self.is_object_address(addr as usize).is_some()
    }

    /// The hoisted per-cycle pin set — **not** a live query.
    ///
    /// Both sources are folded into it by
    /// [`ZgcRealHeap::begin_page_evac_cycle`]: `critical_pins` (the JNI/GPU
    /// use-counted table) and `gc_quiescence::pinned_jit_roots_snapshot()` (the
    /// conservative JIT-frame set the slide consults as `high_pins`). The
    /// handoff page wrote this method as a direct call to the latter and then
    /// flagged it as unacceptable: that function builds a `Vec<usize>`, so a
    /// per-object call is `O(objects x pins)` with an allocation each, inside
    /// the pause. See [`ZgcCounters::page_evac_pins`].
    fn is_pinned(&self, addr: u64) -> bool {
        self.counters
            .page_evac_pins
            .lock()
            .contains(&(addr as usize))
    }

    /// The mark word, taken **before** the raw copy.
    ///
    /// NOT optional over a real [`ObjectHeader`]: the copy spans an
    /// `AtomicU64`, and `copy_nonoverlapping` across one is UB. It is also the
    /// one field a concurrent mutator can be changing under us -- thin lock,
    /// inflated monitor pointer, identity hash, and since the 24 -> 16 shrink
    /// the `kind`/`element_type`/`gc_age`/`gc_flags` quartet. Mirrors
    /// `SharedEvac::evacuate` in `gc/src/g1.rs`, whose comment is worth
    /// repeating: re-*reading* the word after the copy instead of using this
    /// snapshot can pick up a racing writer's value.
    fn pre_copy_snapshot(&self, from: u64) -> u64 {
        if from == 0 {
            return 0;
        }
        self.header_ref(from as usize as *mut u8)
            .mark_word
            .load(Ordering::Acquire) as u64
    }

    /// Re-apply [`Self::pre_copy_snapshot`]'s value to the destination.
    fn post_copy_apply(&self, to: u64, snapshot: u64) {
        if to == 0 {
            return;
        }
        self.header_ref(to as usize as *mut u8)
            .mark_word
            .store(snapshot as u32, Ordering::Release);
    }
}

// G45: this impl has NO descriptor-aware field accessor of its own.
//
// `get_field_as`, `get_field_volatile_as`, `set_field_as` and
// `set_field_volatile_as` are inherited verbatim from the
// `GarbageCollector` defaults in `collector.rs`, and nothing anywhere under
// `zgc.rs` / `zgc/` / `zgc_concurrent.rs` names
// `coerce_field_value_by_descriptor`, `coerce_field_value_for_slot` or
// `FieldCoercionSite` — VERIFIED by grep, zero hits. This heap only supplies
// the raw `get_field` / `set_field` half that those defaults call.
//
// That is worth stating rather than leaving to be rediscovered, because ZGC
// is the DEFAULT collector (`vm/src/config.rs:769`, since 2026-08-10). So
// the coercion provenance of a default `cratonvm` run comes entirely from
// `collector.rs`, not from here: there is no ZGC-side site to repair, and a
// lane that goes looking for one in this file will find nothing and may
// conclude the instrument does not cover ZGC. It does. Conversely, a
// measurement blamed on `gen_heap.rs` is not measuring a default run at all
// — a previous performance lane lost time to exactly that.
//
// If this heap ever overrides one of the four for a fused fast path, it must
// pass a real `FieldCoercionSite` (see `collector.rs`), or the default
// collector silently reverts to `class_id=-1 index=-1`.
/// The SATB-suppressed store path, split out of `GarbageCollector::set_field`.
///
/// A separate `impl` block because an inherent method cannot live inside a
/// trait impl, and this one is deliberately NOT on the trait: adding it there
/// would oblige `GenerationalHeap` and `G1Collector` to grow an arm each, and
/// G1 already has its own (`set_field_no_satb`) with different semantics.
impl ZgcRealHeap {
    /// The store itself, with **no** SATB publication.
    ///
    /// # Why this is public and why it has exactly two callers
    ///
    /// `VmHeap::set_field_suppress_satb` is the declared channel for the
    /// weak-reference PROTOCOL writes -- the pre-collection referent null pass
    /// and the remark-time referent clears. Those are not semantic overwrites:
    /// the pre-collection nulls are restored for survivors before mutators
    /// resume, and the remark-time clears are a verdict the collector has
    /// already reached. Publishing them would hand the marker EVERY active
    /// referent as a mark root, so no weak, soft, phantom or cleaner reference
    /// could ever be cleared again -- a silent leak with no assertion able to
    /// catch it, because every reference test is satisfied by "the referent
    /// survived".
    ///
    /// That channel was a plain `set_field` on this backend until 2026-08-16,
    /// which was correct while ZGC had no concurrent cycle and no armed
    /// pre-write barrier of its own. It stopped being correct the moment
    /// `set_field` grew one.
    pub fn set_field_no_satb(&self, obj: ObjectRef, index: usize, value: Value) {
        self.set_field_no_card(obj, index, value);
        // ---- CARD BARRIER, AT THE ACCESSOR --------------------------------
        //
        // # This barrier was on no store path at all until 2026-08-17
        //
        // `note_ref_store` was reached only from
        // `GarbageCollector::write_barrier`, and this backend's `set_field`
        // never calls it. The interpreter says otherwise in as many words --
        // "write_barrier fires automatically inside set_field /
        // set_field_volatile" (`opcodes.rs`) -- and that is true of `gen_heap`,
        // whose `set_field` does call it, and false here. So every interpreted
        // `putfield` skipped the card barrier and the remembered set was empty
        // on any real workload. It is the shape of defect this tree keeps
        // finding: an INERT registration looks exactly like a missing feature,
        // and `remembered_roots` having no non-test caller read as an
        // unfinished phase rather than a hole.
        //
        // Placed at the accessor for the same reason the SATB barrier is (see
        // `set_field`'s note): coverage becomes a property of the ONE store path
        // instead of a property of a call-site census that has to stay complete
        // forever. Both `set_field` and `set_field_suppress_satb` funnel through
        // here, and the suppression channel must NOT suppress this one -- SATB
        // is about a reference being LOST, a card is about one now being HELD.
        //
        // AFTER the store, not before. A card written first could be cleaned by
        // a collection that stopped this thread between the card and the store,
        // and the edge would then exist with no card naming it. Same order as
        // `gen_heap`'s.
        //
        // Cost with no old generation: one relaxed load. `value` is deliberately
        // not consulted -- see `write_barrier`'s note on why over-carding is
        // safe and under-carding is a use-after-free.
        self.note_ref_store(obj.as_ptr() as usize);
    }

    /// [`Self::set_field_no_satb`] without the card -- the store itself. Split
    /// out so the card is paid once, after whichever of the three exits below
    /// this store takes, rather than repeated at each.

    /// Read-side arm of `CRATONVM_DBG_WATCH_PUN`: report a watched slot that
    /// holds a primitive, whether or not the JIT is on.
    ///
    /// Reads the raw 16-byte cell rather than going through the decode, so the
    /// report carries the same tag/payload32/payload64 triple the JIT-side
    /// detector prints and the two can be compared directly.
    #[cold]
    pub fn report_watched_punned_read(&self, obj: ObjectRef, index: usize) {
        let Some((_, want_slot)) = punned_store_watch() else {
            return;
        };
        if index != *want_slot {
            return;
        }
        let header = self.header(obj);
        if !punned_watch_class_matches(header.class_id.as_u32()) {
            return;
        }
        // Counted BEFORE the compact-layout and tag tests: this is the
        // denominator, and it has to mean "the watched slot was read on the
        // watched class", not "and it turned out to be interesting".
        let seen = PUN_WATCH_READS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        // The CONTROL for the punned dump. The corrupt object's cells cannot be
        // read without knowing what a healthy one looks like: whether a
        // reference cell normally carries anything in `payload32`, whether an
        // int cell normally leaves `payload64` zero. Three healthy objects of
        // the watched class, dumped the same way and by the same code, turn
        // that from an argument about layout constants into a diff.
        if seen < 3 {
            self.dump_watched_cells(obj, "healthy");
        }
        if zgc_compact_field_storage(header, index).is_some() {
            return; // compact cells carry no tag; nothing to mismatch
        }
        // SAFETY: `index` is in bounds (checked by the caller) and the object is
        // not compact, so its body is `num_slots` uniform 16-byte cells.
        let (tag, p32, p64) = unsafe {
            let cell = obj.as_ptr().add(HEADER_SIZE + index * SLOT_SIZE);
            (
                std::ptr::read_unaligned(
                    cell.add(cratonvm_types::FIELD_CELL_TAG_OFFSET) as *const u32
                ),
                std::ptr::read_unaligned(
                    cell.add(cratonvm_types::FIELD_CELL_PAYLOAD32_OFFSET) as *const u32
                ),
                std::ptr::read_unaligned(
                    cell.add(cratonvm_types::FIELD_CELL_PAYLOAD64_OFFSET) as *const u64
                ),
            )
        };
        if tag == cratonvm_types::FIELD_CELL_TAG_OBJECT {
            return;
        }
        PUN_WATCH_READS_PUNNED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        // The same split `jit_getfield_impl` makes, and for the same reason: a
        // reference cell of a freshly allocated object is still zero-filled, so
        // it decodes as `Int(0)` and its payload word is 0 -- the correct null,
        // by accident. Those are not corruption, and counting them with the
        // dangerous shape makes a benign number read as a defect number. Only a
        // NON-ZERO payload under a non-`Object` tag is a word compiled code
        // would have dereferenced as a pointer.
        if p64 == 0 {
            return;
        }
        PUN_WATCH_READS_PUNNED_NONZERO.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        self.dump_watched_cells(obj, "PUNNED");
        let name = self.diag_class_name(header.class_id.as_u32());
        tracing::error!(
            target: "cratonvm::gc::guard",
            class = %name,
            index,
            tag,
            payload32 = p32,
            payload64 = p64,
            "punned read watch: the watched slot does not hold an Object"
        );
    }
    /// Every 16-byte cell of `obj`, tagged with `why`.
    ///
    /// Legacy layout only -- a compact body has no cells to print, and printing
    /// its bytes as if it did is how a layout disagreement gets read as a
    /// corrupt value.
    #[cold]
    pub fn dump_watched_cells(&self, obj: ObjectRef, why: &str) {
        let header = self.header(obj);
        let n = header.num_slots() as usize;
        if zgc_compact_field_storage(header, 0).is_some() {
            eprintln!(
                "[watch-pun-cells] {why} class_id={} COMPACT body, no cells",
                header.class_id
            );
            return;
        }
        eprintln!(
            "[watch-pun-cells] {why} class_id={} num_slots={n} addr={:#x}",
            header.class_id,
            obj.as_ptr() as usize
        );
        for i in 0..n {
            // SAFETY: `i < num_slots` and the body is legacy 16-byte cells.
            let (t, p32, p64) = unsafe {
                let cell = obj.as_ptr().add(HEADER_SIZE + i * SLOT_SIZE);
                (
                    std::ptr::read_unaligned(
                        cell.add(cratonvm_types::FIELD_CELL_TAG_OFFSET) as *const u32
                    ),
                    std::ptr::read_unaligned(
                        cell.add(cratonvm_types::FIELD_CELL_PAYLOAD32_OFFSET) as *const u32
                    ),
                    std::ptr::read_unaligned(
                        cell.add(cratonvm_types::FIELD_CELL_PAYLOAD64_OFFSET) as *const u64
                    ),
                )
            };
            eprintln!(
                "[watch-pun-cells] {why}   slot {i}: tag={t} payload32={p32:#x} payload64={p64:#x}"
            );
        }
    }

    pub fn set_resolved_compact_ref_field(
        &self,
        obj: ObjectRef,
        index: usize,
        offset: usize,
        storage: cratonvm_types::FieldStorageKind,
        value: Value,
    ) {
        if punned_store_watch().is_some() {
            self.set_field_no_card(obj, index, value);
            return;
        }
        self.audit_access_receiver(obj.as_ptr() as usize, index, "set_field");
        let ptr = unsafe { obj.as_ptr().add(offset) };
        unsafe { cratonvm_types::write_compact_field(ptr, storage, value, Ordering::Relaxed) };
    }

    pub fn set_field_no_card(&self, obj: ObjectRef, index: usize, value: Value) {
        self.audit_access_receiver(obj.as_ptr() as usize, index, "set_field");
        // WRITER-side trap for the G30-1 species. Off unless
        // `CRATONVM_DBG_WATCH_PUN=<class-substring>:<slot>` is set; when it is,
        // a primitive landing in the watched slot prints a backtrace naming the
        // code that stored it. The read side can only say a punned cell EXISTS.
        if let Some((_, want_slot)) = punned_store_watch() {
            if index == *want_slot {
                let class_id = self.header(obj).class_id.as_u32();
                if punned_watch_class_matches(class_id) {
                    // The denominator again -- see [`PUN_WATCH_READS`]. A
                    // zero `stores_primitive` beside a zero `stores` says
                    // nothing about who wrote the cell; beside a large
                    // `stores` it says the writer is not this accessor.
                    PUN_WATCH_STORES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    if !matches!(value, Value::Object(_)) {
                        PUN_WATCH_STORES_PRIMITIVE
                            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        let name = self.diag_class_name(class_id);
                        tracing::error!(
                            target: "cratonvm::gc::guard",
                            class = %name,
                            class_id,
                            index,
                            ?value,
                            backtrace = %std::backtrace::Backtrace::force_capture(),
                            "punned store watch: a PRIMITIVE was stored into the watched slot"
                        );
                    }
                }
            }
        }
        let header = self.header(obj);
        if self.check_field_index(header, index, "set").is_none() {
            return;
        }
        if let Some((offset, storage)) = zgc_compact_field_storage(header, index) {
            // A non-reference value into a declared-REFERENCE slot: box it,
            // rather than let `write_compact_field`'s `Reference` arm map it to
            // raw 0 and drop the write to null. Boxing is what `gen_heap` has
            // always done for fields, what all four heaps (this one included)
            // have always done for reference ARRAY elements, and what the
            // legacy 16-byte cell does by construction
            // (W7-84-primitive-in-reference-store.md).
            //
            // The allocation happens BEFORE `ptr` is taken. It does not have to
            // on this heap — ZGC does not move an object under its own mutator
            // — but the four implementations are kept in the same order so a
            // reader diffing them sees no difference to explain.
            let value = if storage.is_reference() {
                cratonvm_gc_core::autobox::box_for_reference_slot(value, header.class_id, index, |v| {
                    let wrapper = self.alloc_object(cratonvm_gc_core::heap::AUTOBOX_CLASS_ID, 1);
                    <Self as GarbageCollector>::set_field(self, wrapper, 0, v);
                    wrapper
                })
            } else {
                value
            };
            let ptr = unsafe { obj.as_ptr().add(offset) };
            // zt t19/zb2 (A2b): a colored-store heap writes the good-colored
            // word into an 8-byte reference slot -- the slot
            // `resolve_ref_slot` names and `get_field`'s armed arm decodes.
            // Off (every production heap) this is the store it was.
            if storage.is_reference()
                && storage.size_runtime() == 8
                && self.load_barrier_colored_stores()
            {
                if let Some(word) = self.colored_store_word_of(value) {
                    // SAFETY: an 8-byte, naturally aligned reference slot of
                    // this receiver's registered layout (`zgc_compact_field_storage`).
                    unsafe { (*(ptr as *const AtomicU64)).store(word, Ordering::Relaxed) };
                    return;
                }
            }
            unsafe { cratonvm_types::write_compact_field(ptr, storage, value, Ordering::Relaxed) };
            return;
        }
        // HIB-DCAST-LATEPHASE.1, write half — see the matching note in
        // `get_field` above. This one is strictly worse than the read: striding
        // a 16-byte `Value` cell through a compact-sized body WRITES past the
        // allocation, corrupting whatever object the allocator placed next.
        // Drop the store rather than commit it somewhere unrelated.
        if cratonvm_types::is_compact_object(header) {
            tracing::warn!(
                target: "cratonvm::gc::guard",
                obj = ?obj.as_ptr(),
                index,
                class_id = ?header.class_id,
                "zgc::set_field: compact receiver has no registered layout for \
                 its (class_id, field_count) — dropping the store rather than \
                 writing a legacy 16-byte cell past its packed compact body \
                 (HIB-DCAST-LATEPHASE.1)",
            );
            return;
        }
        // `write_value_atomic`, not `ptr::write`, for two reasons that both
        // matter on THIS collector: the concurrent marker scans field slots
        // while the mutator writes them (a plain 16-byte `ptr::write` racing
        // the marker's read is formally UB and can splice two stores into a
        // garbage pointer), and `ptr::write` of a narrow variant commits the
        // `Value`'s PADDING to the cell's 64-bit payload word — the word a
        // compiled reference `getfield` dereferences. See `value_words`.
        // `g1` and `gen_heap` already write field cells this way.
        //
        // zt t20/zb3: a LEGACY cell stays PLAIN on a colored-store heap too
        // (zt t19/zb2 wrote `[FIELD_CELL_TAG_OBJECT, colored word]` here).
        // A cell's readers move it as a whole `Value` -- `read_value_*`, the
        // marker's and the census's legacy arms, the JIT's cell reads, the
        // diagnostic dumps -- none of which goes through the colored-slot
        // decode, and a plain word is exactly what every routed reader
        // answers verbatim. Colored words therefore live only in the 8-byte
        // compact and array slots, whose readers all decode.
        // SAFETY: index validated < num_slots, so the slot is within bounds.
        unsafe {
            let ptr = obj.as_ptr().add(HEADER_SIZE + index * SLOT_SIZE);
            cratonvm_types::write_value_atomic(ptr as *mut Value, value);
        }
    }
}

/// Phase stopwatch for the `--verbose:gc` pause anatomy.
///
/// # Why the anatomy exists
///
/// The 2026-08-16 concurrent-marking measurement reported a per-cycle pause and
/// nothing about what was IN it, and that turned out to hide two things at once:
/// the mark-start pause was not being measured at all, and the pause that WAS
/// measured is mostly not the mark. A pause number with no breakdown cannot
/// answer "why is there still a pause on a concurrent collector", which is the
/// only question worth asking of one.
///
/// `None` when logging is off: every `lap` is then a branch on an `Option` and
/// no clock read at all.
#[derive(Debug)]
// ---------------------------------------------------------------------------
// ---------------------------------------------------------------------------
// The sweep
// ---------------------------------------------------------------------------
//
// `ZSweepShard`, `ZSweepCfg` and the four `ZgcRealHeap` methods that drive
// them moved to `zgc/sweep.rs` on 2026-09-02. See that module's header.
pub struct ZPhaseClock {
    pub last: Option<std::time::Instant>,
}

impl ZPhaseClock {
    #[inline]
    pub fn new(armed: bool) -> Self {
        ZPhaseClock {
            last: armed.then(std::time::Instant::now),
        }
    }

    /// Microseconds since the previous `lap` (or since construction), and
    /// restart. `0` when disarmed.
    #[inline]
    pub fn lap(&mut self) -> u128 {
        match self.last {
            Some(t) => {
                let now = std::time::Instant::now();
                let us = now.duration_since(t).as_micros();
                self.last = Some(now);
                us
            }
            None => 0,
        }
    }

    /// zs s4/ob3: the instant the last `lap` ended (or the clock started);
    /// `None` when disarmed. A phase boundary for the pause bean's split.
    #[inline]
    pub fn at(&self) -> Option<std::time::Instant> {
        self.last
    }
}

/// One collection's phase laps, in **microseconds**, as
/// [`ZgcRealHeap::collect_garbage`] already measures them with [`ZPhaseClock`].
///
/// A struct rather than nine positional arguments to
/// [`ZgcRealHeap::record_metrics_for_cycle`]: nine `u128`s in a row is a
/// call site where two transposed fields compile and report the wrong phase
/// forever, which is precisely the kind of silent instrument defect
/// `metrics.rs`'s header exists to argue against.
///
/// `timed` is `false` when the clock was disarmed (no `--verbose:gc` **and** no
/// pause target), in which case every lap is `0` and recording them would fill
/// `metrics.rs`'s percentile rings with zero samples — a p99 of 0 reads as "the
/// phase is free", which is worse than no data. The recorder skips the phase
/// half entirely in that case and still closes the cycle.
#[derive(Debug, Clone, Copy, Default)]
pub struct ZCycleLaps {
    /// Whether the laps below were measured (a clock was running). The
    /// director needs one on every cycle it samples, so `collect_garbage`
    /// starts the clock under `CRATONVM_ZGC_RATE_TRIGGER` /
    /// `CRATONVM_ZGC_TRIGGER_SHADOW` too (zr z1/tr).
    pub timed: bool,
    /// Whether this cycle's tracing happened off the safepoint.
    pub marked_concurrently: bool,
    /// Whether the compaction block ran at all this cycle.
    pub relocated: bool,
    pub markend_us: u128,
    pub tlab_us: u128,
    pub snapshot_us: u128,
    pub mark_us: u128,
    pub resurrect_us: u128,
    pub refs_us: u128,
    pub sweep_us: u128,
    pub relocate_us: u128,
}

impl ZCycleLaps {
    /// The whole cycle, slide included, in microseconds: the sum of the laps.
    pub fn total_us(&self) -> u128 {
        self.markend_us
            .saturating_add(self.tlab_us)
            .saturating_add(self.snapshot_us)
            .saturating_add(self.mark_us)
            .saturating_add(self.resurrect_us)
            .saturating_add(self.refs_us)
            .saturating_add(self.sweep_us)
            .saturating_add(self.relocate_us)
    }
}

impl ZgcRealHeap {
    /// Feed one completed collection into [`metrics::ZgcMetrics`].
    ///
    /// # The mapping, and why each lap lands where it does
    ///
    /// This collector's phases are not OpenJDK's, so the correspondence is
    /// stated here rather than left to a reader of two files:
    ///
    /// | `ZgcPhase` | fed from | note |
    /// |---|---|---|
    /// | `PauseMarkStart` | `tlab_us + snapshot_us` | the STW preamble: retire every TLAB, snapshot the object-start registry. Nothing is traced yet. |
    /// | `ConcurrentMark` | `mark_us`, **only** when `marked_concurrently` | otherwise not recorded at all, so `count` is the number of cycles that really did trace off the safepoint. |
    /// | `PauseMarkEnd` | `markend_us`, plus `mark_us` on a stop-the-world cycle | one record per cycle, summed, so `count` stays "cycles", not "laps". |
    /// | `ConcurrentProcessNonStrongRefs` | `resurrect_us + refs_us` | finalizer resurrection and weak/soft/phantom processing. |
    /// | `Sweep` | `sweep_us` | the module's one non-OpenJDK phase, declared `is_stw`. |
    /// | `ConcurrentRelocate` | `relocate_us` | the slide. See below. |
    ///
    /// `ConcurrentMarkContinue`, `ConcurrentResetRelocationSet`,
    /// `ConcurrentSelectRelocationSet`, `PauseRelocateStart` and
    /// `ConcurrentRemap` stay at `count=0`. That is a **finding, not a gap**,
    /// and it is the reading `metrics.rs`'s "silence is not zero" convention
    /// was built to make available: this collector has no relocation-set
    /// selection phase and no remap walk, and the report now says so instead of
    /// omitting the columns.
    ///
    /// # `ConcurrentRelocate` for a stop-the-world slide is not a mislabel
    ///
    /// `ZgcMetrics::phases_run_concurrently` is `false` for this heap, and
    /// `phase_was_stw` is `phase.is_stw() || !phases_run_concurrently()`. So the
    /// slide's time is charged to the **STW** total and counted against
    /// `stw_share`, which is the honest accounting — the whole cycle is inside
    /// one `StopTheWorldToken`. The phase name records what the work *is*
    /// (evacuation), the flag records how it *ran*. That separation is the
    /// module's design and the reason the flag exists.
    /// `CRATONVM_ZGC_METRICS_TSV=<path>` -- append this VM's whole-run
    /// [`metrics::ZgcMetrics::to_tsv_row`] to `path`, writing
    /// [`metrics::ZgcMetrics::tsv_header`] first if the file is new or empty.
    ///
    /// The one thing the module was built for and the one thing nothing did.
    /// `tsv_header`'s own doc states the intent -- a full-suite ZGC run joined
    /// against a Generational one with `sort`/`join` rather than by
    /// hand-parsing `[GC-SUMMARY]` out of 1975 logs -- and names the step that
    /// did not happen. This is that step.
    ///
    /// **From here the append-only column rule binds for real.** It was
    /// previously safe to reorder `ZgcPhase::ALL` because no row existed to
    /// misalign; once a harness has collected rows, a new phase goes at the END
    /// of that list even at the cost of cycle order. `tsv_header` says so.
    ///
    /// `CRATONVM_ZGC_METRICS_RUN=<label>` sets the `run` column, which is the
    /// join key. Unset, it stays whatever
    /// [`metrics::ZgcMetrics::set_run_label`] last saw -- the module's own
    /// default -- so a harness that forgets the label gets rows it cannot join
    /// rather than rows silently attributed to the wrong arm.
    ///
    /// # One `O_APPEND` write per VM, deliberately
    ///
    /// A suite fans out many VMs onto one file. Each writes exactly one
    /// `write_all` of one line to a handle opened with `append(true)`, which is
    /// the only interleaving-safe shape available without a lock file: a
    /// second process cannot land inside another's line. The header race is
    /// benign in the other direction -- two VMs can both see an empty file and
    /// both write a header, which duplicates a row a reader drops on the column
    /// name, rather than corrupting one.
    ///
    /// Every failure here is swallowed after one `eprintln!`. This runs from
    /// `Drop` at VM teardown; a panic there would turn an unwritable path into
    /// a crash on the way out of a run whose real work already succeeded.
    /// Everything this heap emits once, at the end of the run: the
    /// `CRATONVM_ZGC_METRICS` summary and the `CRATONVM_ZGC_METRICS_TSV` row.
    ///
    /// **Claims once per process.** Two call sites reach it (`vm-cli`'s
    /// shutdown, and `Drop` as a backstop for tests), both arms of `vm-cli`'s
    /// exit reach the first, and the contract the TSV publishes is *one row per
    /// VM*. A second row would not merely duplicate -- it would double every
    /// aggregate a harness computes over the file.
    ///
    /// zr z3/lt: claimed once per HEAP (it was once per process, so a second
    /// VM in the process never wrote its row);
    /// `CRATONVM_ZGC_METRICS_ROW_PER_HEAP=0` restores the process claim. See
    /// [`Self::claim_end_of_run_metrics`].
    pub fn emit_end_of_run_metrics(&self) {
        if !self.claim_end_of_run_metrics() {
            return;
        }
        // `eprintln!` rather than `ZgcMetrics::log_summary`, deliberately, and
        // that module's own doc says why: `log_summary` goes through `tracing`
        // so it honours the run's log filter, and it must be *"call[ed] while
        // the VM is still up"*. At shutdown the subscriber may already be gone,
        // so the summary takes `g1.rs::print_gc_summary`'s route instead. Only
        // under `CRATONVM_ZGC_METRICS`; a default run's stderr is untouched.
        if zgc_metrics_lines_enabled() {
            eprintln!("{}", self.counters.metrics.format_summary());
        }
        // zr z1/tr: the slot census summary, for the same reason and by the
        // same route. Its only call was in `Drop`, which the shipped VM never
        // runs, so `CRATONVM_ZGC_CENSUS=1` walked every sweep and printed
        // nothing (`gap-d-census-instrument-has-no-on-switch.md` was closed on
        // the caller existing, not on the line appearing).
        if self.counters.slot_census.is_enabled() {
            eprintln!("{}", self.counters.slot_census.format_summary());
        }
        // zt t18/zb1: the armed read census, by the same route.
        if zgc_load_barrier_census_level() != 0 {
            eprintln!("{}", self.load_barrier_census_line());
        }
        self.append_metrics_tsv_row();
    }

    pub fn append_metrics_tsv_row(&self) {
        use std::io::Write as _;
        let Some(path) = None::<std::ffi::OsString> else {
            return;
        };
        if path.is_empty() {
            return;
        }
        if let Some(label) = None::<std::ffi::OsString> {
            if let Some(label) = label.to_str() {
                if !label.is_empty() {
                    self.counters.metrics.set_run_label(label);
                }
            }
        }
        let path = std::path::PathBuf::from(path);
        let needs_header = match std::fs::metadata(&path) {
            Ok(m) => m.len() == 0,
            Err(_) => true,
        };
        let opened = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path);
        let mut f = match opened {
            Ok(f) => f,
            Err(e) => {
                eprintln!(
                    "[GC] zgc-metrics-tsv: cannot open {} ({e}); the row for this VM is lost",
                    path.display()
                );
                return;
            }
        };
        let mut out = String::new();
        if needs_header {
            out.push_str(metrics::ZgcMetrics::tsv_header());
            out.push('\n');
        }
        out.push_str(&self.counters.metrics.to_tsv_row());
        out.push('\n');
        if let Err(e) = f.write_all(out.as_bytes()) {
            eprintln!(
                "[GC] zgc-metrics-tsv: cannot write {} ({e}); the row for this VM is lost",
                path.display()
            );
        }
    }

    /// zr z4/jf: take the last collection's JFR facts (its laps as sub-phase
    /// rows, its relocation set, its uncommit), leaving none. Called once by
    /// the VM's collection event finish; `None` when nothing was recorded.
    pub fn take_jfr_cycle(&self) -> Option<jfr_facts::ZJfrCycle> {
        self.counters.jfr.take()
    }

    /// zs s4/ob3: take the last collection's phase boundaries for the pause
    /// bean's split (`CRATONVM_ZGC_PAUSE_PHASES`), leaving none. Called once by
    /// the VM's collection seal, inside the pause.
    pub fn take_pause_split(&self) -> Option<cratonvm_gc_core::gc_metrics::ZgcPauseSplit> {
        self.counters.jfr.take_pause_split()
    }

    /// zr z4/jf: `bytes` returned to the OS in `nanos` inside the current
    /// collection, for `jdk.ZUncommit`. For the uncommit step to call once it
    /// times itself (cross-lane hunk for lane `un`); a no-op for `bytes == 0`.
    pub fn note_jfr_uncommit(&self, bytes: usize, nanos: u64) {
        self.counters.jfr.note_uncommit(bytes as u64, nanos);
    }

    /// zs s7/jf: the reference pass's per-type counts (soft, weak, final,
    /// phantom) for the current collection's `jdk.GCReferenceStatistics`.
    pub fn note_jfr_reference_counts(&self, counts: [u64; 4]) {
        self.counters.jfr.note_reference_counts(counts);
    }

    /// zs s10/sp (`CRATONVM_ZGC_RETIRE_SUB_REQUEST_SPANS`, default ON since zt t12): an
    /// `OutOfMemoryError` is being built for a refused allocation. When the
    /// refusal that led to it was a SMALL request of `S` bytes refused by
    /// `alloc_raw` since the latest collection (the ladder's last attempt),
    /// the low free list stops serving from blocks smaller than
    /// `T = max(S, floor)` (`Arena::set_min_free_span`): HotSpot's analogue of
    /// the TLAB it retires before the refill that fails -- after that refusal
    /// no small hole serves a later request; only fresh bump space, or a
    /// block of at least `T`, does. The blocks stay on the free list (nothing
    /// is removed, so nothing leaks); [`Self::recheck_sub_request_spans`]
    /// re-admits them after a collection that changed the picture.
    ///
    /// Why: the L4 launcher probes reach `main` on CratonVM through a 16-byte
    /// free-list remainder the fill left (and, after F1 of zs s9/la, through
    /// the dead fill node's 32-byte span), where HotSpot fails the receiver
    /// (`zgc-zs9la-launcher-oome-rows-a-free-span-remainder-serves-the-receiver-FIXED-20261019.md`).
    pub fn note_final_refusal(&self) {
        let floor = self.counters.sub_request_floor;
        if floor == 0 {
            return;
        }
        let epoch = self.counters.last_refusal_epoch.load(Ordering::Relaxed);
        if epoch == usize::MAX || epoch != self.gc_count.load(Ordering::Relaxed) {
            return;
        }
        let size = self.counters.last_refusal_size.load(Ordering::Relaxed);
        if size == 0 || size >= ZGC_LARGE_OBJECT_MIN {
            return;
        }
        let threshold = size.max(floor);
        let mut arena = self.arena.lock();
        if arena.min_free_span() >= threshold {
            return;
        }
        arena.set_min_free_span(threshold);
        let used = arena.used();
        let available = arena.free_list_bytes() + arena.capacity().saturating_sub(used);
        self.counters
            .sub_request_armed_available
            .store(available, Ordering::Relaxed);
        self.counters
            .sub_request_armed_used
            .store(used, Ordering::Relaxed);
        drop(arena);
        self.counters
            .sub_request_arms
            .fetch_add(1, Ordering::Relaxed);
        // zt t1/sp: the rule's engagement, named under the existing
        // `CRATONVM_DBG_ZGC_POST_REFUSAL` diagnostic (stderr; off: one load).
        // The arms / disarms counters had no reader outside the unit tests,
        // so a host row could not tell "armed and the receiver still served"
        // from "never armed".
        if self.counters.post_refusal_limit != 0 {
            eprintln!(
                "[zgc-post-refusal] sub-request-spans armed threshold={threshold} \
                 refused={size} available={available} used={used} gc={}",
                self.gc_count.load(Ordering::Relaxed)
            );
        }
    }

    /// zs s10/sp: the post-collection half of [`Self::note_final_refusal`].
    /// The threshold is dropped (every listed block serves again) when this
    /// collection changed the picture by at least
    /// `progress = max(64 KiB, capacity / 64)`: the arena's bumped bytes went
    /// DOWN by that much since the rule armed (a slide relocated the live set
    /// down: HotSpot's page relocation), or what the arena can serve grew by
    /// that much (a dropped data structure). Otherwise the threshold stands
    /// and the blocks the collection freed join the set-aside ones, as dead
    /// bytes in a full page do on HotSpot. Called once per stop-the-world
    /// collection after its sweep, and again after a slide that moved
    /// something (zt t2/x1: the first call runs before the slide), under the
    /// pause; one arena read when the rule is off or not armed.
    ///
    /// zt t1/sp review: the s10/sp cut disarmed on ANY drop of the bumped
    /// bytes (`used < armed_used`). A sweep's cursor retraction lowers them by
    /// the dead bytes at the top of the bump region -- on the L4 launcher
    /// fill that is the dead 32-byte node and the last error, i.e. exactly
    /// the bytes the rule exists to keep from serving `main`'s receiver -- so
    /// the first collection after the final refusal re-admitted every
    /// set-aside block. A retraction moves free-list bytes into the bump tail
    /// and does not change what the arena can serve, so it now needs the same
    /// `progress` as the other clause.
    pub fn recheck_sub_request_spans(&self) {
        if self.counters.sub_request_floor == 0 {
            return;
        }
        let mut arena = self.arena.lock();
        if arena.min_free_span() == 0 {
            return;
        }
        let used = arena.used();
        let available = arena.free_list_bytes() + arena.capacity().saturating_sub(used);
        let progress = (arena.capacity() / 64).max(64 * 1024);
        let armed_available = self
            .counters
            .sub_request_armed_available
            .load(Ordering::Relaxed);
        let armed_used = self.counters.sub_request_armed_used.load(Ordering::Relaxed);
        if armed_used.saturating_sub(used) >= progress
            || available >= armed_available.saturating_add(progress)
        {
            arena.set_min_free_span(0);
            drop(arena);
            self.counters
                .sub_request_disarms
                .fetch_add(1, Ordering::Relaxed);
            if self.counters.post_refusal_limit != 0 {
                eprintln!(
                    "[zgc-post-refusal] sub-request-spans disarmed used={used} \
                     armed_used={armed_used} available={available} \
                     armed_available={armed_available} gc={}",
                    self.gc_count.load(Ordering::Relaxed)
                );
            }
        }
    }

    /// zs s10/sp: `(threshold now, arms, disarms)` of the final-refusal rule.
    pub fn sub_request_span_stats(&self) -> (usize, usize, usize) {
        (
            self.arena.lock().min_free_span(),
            self.counters.sub_request_arms.load(Ordering::Relaxed),
            self.counters.sub_request_disarms.load(Ordering::Relaxed),
        )
    }

    /// zs s7/jf: the byte size of the request `alloc_raw` last refused, when
    /// that refusal came no earlier than the collection before the latest one
    /// (the request an allocation stall waited for), for `jdk.ZAllocationStall`.
    /// `None` when no refusal is that recent (a door that stalled without an
    /// arena refusal). Reads the zr z5/cr refusal record; two relaxed loads,
    /// once per stall.
    pub fn jfr_stall_request_bytes(&self) -> Option<usize> {
        let epoch = self.counters.last_refusal_epoch.load(Ordering::Relaxed);
        if epoch == usize::MAX || self.gc_count.load(Ordering::Relaxed).wrapping_sub(epoch) > 1 {
            return None;
        }
        Some(self.counters.last_refusal_size.load(Ordering::Relaxed))
    }

    /// zs s7/jf: `jdk.ZYoungGarbageCollection.tenuringThreshold` of a young
    /// (`ZGC Minor`) collection, in HotSpot's unit: the young collections an
    /// object survives before it is promoted, minus one (HotSpot ZGC promotes
    /// a page whose age is at or above the threshold, eden being age 0; this
    /// heap promotes an object once its header age reaches the promotion age,
    /// a first-cycle survivor being age 1). `None` outside the generational
    /// mode: a whole-heap collection has no tenuring decision to report.
    pub fn jfr_tenuring_threshold(&self) -> Option<u32> {
        self.generational_enabled()
            .then(|| u32::from(self.promotion_age()).saturating_sub(1))
    }

    pub fn record_metrics_for_cycle(
        &self,
        laps: ZCycleLaps,
        bytes_in_use_before: usize,
        live_bytes: usize,
        capacity_bytes: usize,
        trigger_mask: usize,
    ) {
        let m = &self.counters.metrics;
        // zt t7/tg: restart the `Timer` rule's clock at the end of a
        // whole-heap cycle (a young-only cycle moved `young_cycles`, and
        // HotSpot's major timer reads the old generation's clock). Two
        // relaxed loads and two stores per collection.
        //
        // zt t12/tg5 (TG3-O4, `CRATONVM_ZGC_TIMER_SKIPS_STALL_CYCLES`, default
        // ON, `=0` kill): nor does an allocation stall's cycle (the VM marks
        // the collecting thread by its forced site,
        // `gc_quiescence::with_allocation_stall_door`), as HotSpot's stall
        // runs a minor that leaves the old generation's clock alone. Only
        // the `Timer` rule reads this clock, so the switch matters only with
        // `-XX:ZCollectionInterval`. One TLS read, plus the flag read for a
        // stall's cycle only.
        // zt t31/x10 (item 5): or a stall another thread raised before its
        // pause, served by this cycle (taken here whatever the answer).
        let pending_stall = self.counters.stall_cycle_pending.swap(false, Ordering::AcqRel);
        let stall_cycle = (cratonvm_gc_core::gc_quiescence::allocation_stall_door() || pending_stall)
            && zgc_timer_skips_stall_cycles_enabled();
        self.counters.timer.note_cycle_end_as(
            std::time::Instant::now(),
            self.counters.young_cycles.load(Ordering::Relaxed),
            stall_cycle,
        );
        // zt t1/jf (X1): the JFR `Concurrent Mark` row needs this fact even
        // when no lap was timed.
        self.counters.jfr.note_marked_concurrently(laps.marked_concurrently);
        if laps.timed {
            // `as u64` after `min`: a lap wider than u64 nanoseconds is ~584
            // years and cannot happen, but saturating beats wrapping to a tiny
            // value that would silently improve every percentile.
            let ns = |us: u128| -> u64 { us.saturating_mul(1_000).min(u64::MAX as u128) as u64 };
            m.record_phase(
                metrics::ZgcPhase::PauseMarkStart,
                ns(laps.tlab_us.saturating_add(laps.snapshot_us)),
            );
            if laps.marked_concurrently {
                m.record_phase(metrics::ZgcPhase::ConcurrentMark, ns(laps.mark_us));
                m.record_phase(metrics::ZgcPhase::PauseMarkEnd, ns(laps.markend_us));
            } else {
                m.record_phase(
                    metrics::ZgcPhase::PauseMarkEnd,
                    ns(laps.markend_us.saturating_add(laps.mark_us)),
                );
            }
            m.record_phase(
                metrics::ZgcPhase::ConcurrentProcessNonStrongRefs,
                ns(laps.resurrect_us.saturating_add(laps.refs_us)),
            );
            m.record_phase(metrics::ZgcPhase::Sweep, ns(laps.sweep_us));
            // zt t3/tp: the pacing census's cost columns (`[GC] zgc-pacing:`
            // `pace_us` / `pace_nf_*_us`), one charge per timed collection.
            let us = |v: u128| -> u64 { v.min(u64::MAX as u128) as u64 };
            let mark_us = laps.tlab_us + laps.snapshot_us + laps.mark_us + laps.markend_us;
            let reloc_us = if laps.relocated { laps.relocate_us } else { 0 };
            self.counters.pacing.note_cost(
                us(mark_us),
                us(laps.resurrect_us + laps.refs_us),
                us(laps.sweep_us),
                us(reloc_us),
            );
            if laps.relocated {
                m.record_phase(metrics::ZgcPhase::ConcurrentRelocate, ns(laps.relocate_us));
            }
            // zr z4/jf: the same laps as `jdk.GCPhasePauseLevel1` rows
            // (`jfr_facts::lap_marks`); taken by the VM's event finish.
            self.counters.jfr.note_laps(
                jfr_facts::lap_marks(
                    laps.marked_concurrently,
                    laps.relocated,
                    laps.markend_us,
                    laps.tlab_us,
                    laps.snapshot_us,
                    laps.mark_us,
                    laps.resurrect_us,
                    laps.refs_us,
                    laps.sweep_us,
                    laps.relocate_us,
                ),
                laps.marked_concurrently,
            );
        }
        // ALWAYS, even on an untimed run: the cycle counter, the occupancy
        // gauges and the derived allocation rate need no clock of their own
        // (`record_cycle` takes its own `Instant`), and a run whose cycle count
        // stopped advancing because logging was off would be the census defect
        // in a second place.
        //
        // zt t1/tp (`CRATONVM_ZGC_RATE_EXCLUDES_PAUSE`, default on): the
        // interval the rate is derived from is mutator time -- this cycle's
        // pause laps are subtracted (a mark traced off the safepoint is not:
        // the mutators ran through it). `0` (nothing subtracted) on an untimed
        // cycle and at `=0`.
        //
        // zt t2/x1 (finding X1-2): EVERY lap is pause time. The laps are all
        // taken inside `collect_garbage`, under its `StopTheWorldToken`; on a
        // concurrently marked cycle `mark_us` is that pause's own mark lap
        // (bit clearing and the end-of-mark work, "zero on a concurrently-
        // marked cycle" where it is taken), never the concurrent phase, which
        // ran before this collection and is in no lap. The t1 cut subtracted
        // `mark_us` from the pause on such a cycle, i.e. counted a stopped-
        // world lap as mutator time.
        let excludes_pause = self.counters.rate_excludes_pause.load(Ordering::Relaxed);
        let pause_ns = if laps.timed && excludes_pause {
            let pause_us = laps.total_us();
            pause_us.saturating_mul(1_000).min(u64::MAX as u128) as u64
        } else {
            0
        };
        m.record_cycle_excluding_pause(
            bytes_in_use_before as u64,
            live_bytes as u64,
            capacity_bytes as u64,
            pause_ns,
        );
        // The per-cycle line, opt-in. `[GC] zgc-real:` and `[GC] zgc-pause:`
        // are unchanged and still printed by the block that owns them, so a
        // harness keyed on either keeps parsing; this is the OpenJDK-shaped
        // rendering of the same cycle, printed beside them for the wave that
        // decides whether to switch over. See `zgc_metrics_lines_enabled`.
        if zgc_metrics_lines_enabled() {
            // zr zs s1/tp: the cycle's own cause (it printed `Allocation Rate`
            // for every cycle). `Unasked` is a collection no `needs_gc` clause
            // asked for -- `System.gc()` or an allocation that failed, which
            // only the VM's door can tell apart (its `-Xlog:gc` line does).
            let cause = hotspot_cause_for_mask(trigger_mask).unwrap_or("Unasked");
            eprintln!("{}", m.format_cycle_line_with_cause(cause));
        }
        // zr z1/tr: close the director's interval -- one cycle-duration sample
        // (the laps' sum, which is the whole pause including the slide), the
        // post-cycle occupancy, and the shadow record of where its rules first
        // said yes. A no-op returning `None` unless the director samples.
        let director_first_yes = if self.counters.director.is_sampling() {
            // zt t1/tp (`CRATONVM_ZGC_DIRECTOR_HOTSPOT_WARMTH`): a warm-up
            // cycle advances HotSpot's warmth; with the warm-up rule off every
            // cycle does (the zr count), so the rate rule is not left off.
            let warmup_cycle = trigger_mask & TRIGGER_BIT_WARMUP != 0;
            if warmup_cycle || !self.counters.warmup_enabled.load(Ordering::Relaxed) {
                self.counters.director.note_warmup_cycle();
            }
            let stw_pause = laps
                .timed
                .then(|| laps.total_us())
                .filter(|us| *us > 0)
                .map(|us| std::time::Duration::from_micros(us.min(u64::MAX as u128) as u64));
            if let Some(pause) = stw_pause {
                self.counters.director.note_stw_pause(pause);
            }
            let mut duration = stw_pause;
            // zr z3/cs: under `CRATONVM_ZGC_CONC_EVERY_TRIGGER` a cycle that
            // marked concurrently lasted from its mark start to this pause's
            // end, and that is what HotSpot's rate rule reserves for (the
            // mutators allocate during the concurrent phase).
            if laps.marked_concurrently && self.conc_every_trigger_armed() {
                let phase = self
                    .counters
                    .conc_trigger
                    .last_phase_nanos
                    .load(Ordering::Relaxed);
                if phase != 0 {
                    // Concurrent marking overlaps with mutator allocation; exclude
                    // STW sweep/relocate pause time so cycle_avg/sd do not reserve
                    // heap headroom for STW intervals when mutators are stopped.
                    duration = Some(std::time::Duration::from_nanos(phase));
                }
            }
            let now = std::time::Instant::now();
            let used_after = self.allocated.load(Ordering::Relaxed) as u64;
            let first_yes = self
                .counters
                .director
                .note_cycle_end(now, duration, used_after);
            // zt t14/tg6 (`CRATONVM_ZGC_PROACTIVE_MAJOR_CLOCK`): the proactive
            // rule's clock moves only for a cycle HotSpot would run as a major
            // (`cycle_restarts_major_clock`); read only under the switch.
            if cycle_restarts_major_clock(
                trigger_mask,
                cratonvm_gc_core::gc_quiescence::allocation_stall_door(),
            ) {
                self.counters.director.note_major_cycle_end(now, used_after);
            }
            first_yes
        } else {
            None
        };
        if zgc_trigger_shadow_enabled() {
            self.log_trigger_shadow(&laps, trigger_mask, bytes_in_use_before, director_first_yes);
        }
    }

    /// Stage 1 of `proposal-e-allocation-rate-driven-trigger`: print what the
    /// rate trigger **would** have decided, and change nothing.
    ///
    /// The proposal replaces `needs_gc`'s four independent clauses with one
    /// predicate — *collect when the projected time to exhaustion of
    /// allocatable space falls below the projected duration of a cycle* — and
    /// its own falsification test is the cheapest thing in it:
    ///
    /// > if the shadow projection's time-to-exhaustion is within a factor of
    /// > two of the observed one on the five workloads above, the idea holds;
    /// > if it swings by an order of magnitude between adjacent cycles on any
    /// > of them, it does not.
    ///
    /// That is a measurement, not a feature, and it is the step every previous
    /// attempt at this predicate skipped. So: one line per collection, behind
    /// `CRATONVM_ZGC_TRIGGER_SHADOW=1`, with the four inputs and the ratio they
    /// produce.
    ///
    /// | field | proposal's name | source |
    /// |---|---|---|
    /// | `rate_bps` | `R` | [`metrics::ZgcMetrics::last_allocation_rate_bytes_per_sec`], derived from the occupancy gauges so it cannot disagree with them |
    /// | `free_bytes` | `A` | `Arena::remaining` — un-bumped middle plus both free lists |
    /// | `largest_bytes` | `L` | `max(middle, largest_free_block)`, the biggest single request this arena could serve. On a non-compacting heap this is the number that actually binds |
    /// | `cycle_us` | `P` | this cycle's own measured cost, summed from the laps |
    /// | `affordable_span` | — | what the *existing* pause controller currently believes, for comparison |
    /// | `fired` | — | which `needs_gc` clause actually asked for this collection |
    ///
    /// `ttl_ms` is `A / R`. `ratio` is `ttl_ms / cycle_ms`: the predicate is
    /// `ratio < safety`, so a run whose `ratio` is already below 1 on cycles
    /// the legacy clauses did not fire for is a run the rate trigger would
    /// have collected *earlier*, and vice versa. **Read `ratio`'s stability
    /// between adjacent cycles, not its value** — the value is what stage 2
    /// tunes `safety` against; the stability is what decides whether stage 2
    /// happens at all.
    ///
    /// `rate_bps=0` prints `ttl_ms=inf` and `ratio=inf` rather than dividing:
    /// the first cycle of a run has no previous cycle to measure an interval
    /// against, and a fabricated rate there would be the noisiest sample in the
    /// series sitting at its start.
    ///
    /// # zr z1/tr: the director's columns
    ///
    /// Stage 2 now exists ([`director`], `CRATONVM_ZGC_RATE_TRIGGER=1`), and
    /// the shadow line carries its view in either arm, so the A/B the
    /// proposal asks for is one run, not two:
    ///
    /// | field | meaning |
    /// |---|---|
    /// | `used_at_start` | bytes in use when this collection started -- where the clause in `fired=` actually collected |
    /// | `hs_first_yes` | `<rule>@<bytes>`: the first poll in the interval at which the director's rules said yes, and the occupancy then (`none`: they never did). Below `used_at_start` = HotSpot's rules would have collected EARLIER |
    /// | `hs_rate_avg_bps` / `hs_rate_sd_bps` | the 1 s window of 100 ms allocation-rate samples |
    /// | `hs_cycle_avg_us` / `hs_cycle_sd_us` | the decaying cycle-duration estimate the rate rule budgets for |
    /// | `hs_time_until_gc_ms` | the rate rule's slack at the last sample (`inf`: disabled, i.e. no cycle timed yet) |
    ///
    /// `fired=` names every clause bit: it omitted `warmup` (added r16, after
    /// this line) and the nursery clause, so a warm-up cycle printed
    /// `fired=none` and read as a `System.gc()`.
    pub fn log_trigger_shadow(
        &self,
        laps: &ZCycleLaps,
        trigger_mask: usize,
        used_at_start: usize,
        first_yes: Option<(u64, director::DirectorRule)>,
    ) {
        // The arena lock is taken ONLY under the flag. `sample_frag_gauge`
        // reads the same three quantities under a lock the caller already
        // holds, but it early-returns when free space is low -- precisely the
        // condition this projection is about -- so it cannot be reused here.
        let (free_bytes, largest_bytes) = {
            let arena = self.arena.lock();
            let middle = arena.remaining().saturating_sub(arena.free_list_bytes());
            (arena.remaining(), middle.max(arena.largest_free_block()))
        };
        let rate_bps = self.counters.metrics.last_allocation_rate_bytes_per_sec();
        let cycle_us = laps.total_us();
        let cycle_ms = cycle_us as f64 / 1_000.0;
        let (ttl_ms, ratio) = if rate_bps > 0.0 {
            let ttl = (free_bytes as f64 / rate_bps) * 1_000.0;
            let ratio = if cycle_ms > 0.0 {
                format!("{:.2}", ttl / cycle_ms)
            } else {
                "inf".to_string()
            };
            (format!("{ttl:.1}"), ratio)
        } else {
            ("inf".to_string(), "inf".to_string())
        };
        // Same `mask` spelling as the four `zgc-trigger:` tallies, so a reader
        // can line this up against them: an empty `fired=` is a collection no
        // `needs_gc` clause asked for -- `System.gc()`, or the arena's hard
        // allocation refusal, which bypasses the poll entirely.
        let mut fired = String::new();
        for (bit, name) in [
            (TRIGGER_BIT_STRESS, "stress"),
            (TRIGGER_BIT_WARMUP, "warmup"),
            (TRIGGER_BIT_RATE, "hs_rate"),
            (TRIGGER_BIT_PROACTIVE, "hs_proactive"),
            (TRIGGER_BIT_HIGH_USAGE, "hs_high_usage"),
            (TRIGGER_BIT_THRESHOLD, "threshold"),
            (TRIGGER_BIT_HEADROOM, "headroom"),
            (TRIGGER_BIT_ALLOC_BUDGET, "alloc_budget"),
            (TRIGGER_BIT_NURSERY, "nursery"),
            (TRIGGER_BIT_TIMER, "timer"),
        ] {
            if trigger_mask & bit != 0 {
                if !fired.is_empty() {
                    fired.push('+');
                }
                fired.push_str(name);
            }
        }
        if fired.is_empty() {
            fired.push_str("none");
        }
        let (hs_rate_avg, hs_rate_sd, hs_cycle_avg, hs_cycle_sd, hs_last) =
            self.counters.director.snapshot();
        let hs_first_yes = first_yes.map_or_else(
            || "none".to_string(),
            |(at, rule)| format!("{}@{at}", rule.key()),
        );
        let hs_time_until_gc_ms = match hs_last.map(|d| d.time_until_gc) {
            Some(t) if t.is_finite() => format!("{:.1}", t * 1_000.0),
            _ => "inf".to_string(),
        };
        eprintln!(
            "[GC] zgc-trigger-shadow: cycle={} rate_bps={rate_bps:.0} free_bytes={free_bytes} \
             largest_bytes={largest_bytes} cycle_us={cycle_us} ttl_ms={ttl_ms} ratio={ratio} \
             affordable_span={} fired={fired} used_at_start={used_at_start} \
             hs_first_yes={hs_first_yes} hs_rate_avg_bps={hs_rate_avg:.0} \
             hs_rate_sd_bps={hs_rate_sd:.0} hs_cycle_avg_us={:.0} hs_cycle_sd_us={:.0} \
             hs_time_until_gc_ms={hs_time_until_gc_ms}",
            self.counters.metrics.cycles(),
            self.pause_affordable_span.load(Ordering::Relaxed),
            hs_cycle_avg * 1e6,
            hs_cycle_sd * 1e6,
        );
    }
}

