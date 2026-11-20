#[allow(unused_imports)]
use super::*;


#[inline]
pub unsafe fn value_from_unaligned_ptr(ptr: *const u8) -> Value {
    // SAFETY: callers only pass bytes copied from a valid `Value` slot.
    unsafe { std::ptr::read_unaligned(ptr as *const Value) }
}

#[inline]
pub unsafe fn value_to_unaligned_ptr(value: Value, ptr: *mut u8) {
    // SAFETY: the byte buffer is large enough for one `Value`; alignment is
    // intentionally not assumed because stack `[u8; N]` buffers are align-1.
    unsafe { std::ptr::write_unaligned(ptr as *mut Value, value) };
}

#[inline]
pub fn value_from_bytes(bytes: &[u8; SLOT_SIZE]) -> Value {
    // SAFETY: callers only pass bytes copied from a valid `Value` slot.
    unsafe { value_from_unaligned_ptr(bytes.as_ptr()) }
}

#[inline]
pub fn value_to_bytes(value: Value, bytes: &mut [u8; SLOT_SIZE]) {
    // SAFETY: the byte buffer is large enough for one `Value`.
    unsafe { value_to_unaligned_ptr(value, bytes.as_mut_ptr()) };
}

/// Enumerate reference fields of a non-humongous object under either body
/// layout. `slot` points at the writable on-heap representation; `compact`
/// selects an 8-byte raw pointer versus a legacy 16-byte `Value` cell.
///
/// # The name is the contract (option C)
///
/// This entry point takes its element count from `header.num_slots()` and bounds
/// it by nothing. It cannot: it is handed a raw pointer and a `&ObjectHeader`,
/// and knows neither the region, the arena, nor the object's allocated size, so
/// `HEADER_SIZE + num_slots * SLOT_SIZE` is an address it has no way to check.
/// **It trusts its caller**, and until 2026-08-26 nothing in its signature said
/// so — fourteen call sites made that assumption silently.
///
/// The rename is the whole of option **C** in
/// `what-should-a-walker-do-with-an-unvalidated-header-count-FIXED-20260826.md`
/// §4: no runtime cost, no behaviour change, and fourteen silent assumptions
/// become fourteen readable ones. A caller that *does* hold region geometry
/// should use [`for_each_flat_object_reference_capped`] with
/// `holder_walkable_slots`, which is strictly stronger than trusting the count.
///
/// The trust does **not** extend to the object's KIND — see the refusal at the
/// top of [`for_each_flat_object_reference_capped`], which both entry points go
/// through.
#[track_caller]
pub fn for_each_flat_object_reference_trusting_header(
    obj_ptr: *const u8,
    header: &ObjectHeader,
    first_index: usize,
    visit: impl FnMut(*mut u8, usize, bool),
) {
    // Uncapped: the legacy walk trusts `header.num_slots()`. A caller that holds
    // region geometry should prefer the `_capped` form below — see its doc.
    for_each_flat_object_reference_capped(obj_ptr, header, first_index, usize::MAX, visit)
}

/// Per-worker L1 cache of `CompactLayout` descriptors keyed by `(class_id, field_count)`.
/// Invalidated automatically whenever [`cratonvm_types::layout_generation`] advances.
#[derive(Clone, Copy)]
pub struct CachedCompactRefs {
    pub total_size: u32,
    pub ref_len: u8,
    pub disps: [u32; 8],
}

#[derive(Clone)]
struct CacheEntry {
    class_id: u32,
    field_count: u32,
    inline: Option<CachedCompactRefs>,
    arc: Option<std::sync::Arc<cratonvm_types::CompactLayout>>,
}

impl CacheEntry {
    const fn empty() -> Self {
        Self {
            class_id: u32::MAX,
            field_count: 0,
            inline: None,
            arc: None,
        }
    }
}

pub struct CompactLayoutCache {
    gen: u64,
    entries: [CacheEntry; 8],
    cursor: usize,
    mru: u8,
}

impl Default for CompactLayoutCache {
    fn default() -> Self {
        Self {
            gen: u64::MAX,
            entries: [const { CacheEntry::empty() }; 8],
            cursor: 0,
            mru: 0,
        }
    }
}

impl CompactLayoutCache {
    /// Look up `(class_id, field_count)` in this worker's 8-entry cache before falling back
    /// to `cratonvm_types::class_layout_for_fields`, avoiding TLS, `RefCell`, and cross-core
    /// `Arc` refcount contention for objects with <= 8 reference fields.
    #[inline]
    fn find_or_insert_slot(&mut self, class_id: u32, field_count: u32) -> usize {
        let cur_gen = cratonvm_types::layout_generation();
        if self.gen == cur_gen {
            let mru = (self.mru as usize) & 7;
            if self.entries[mru].class_id == class_id && self.entries[mru].field_count == field_count {
                return mru;
            }
        } else {
            for e in &mut self.entries {
                *e = CacheEntry::empty();
            }
            self.gen = cur_gen;
            self.mru = 0;
        }
        if let Some(idx) = self
            .entries
            .iter()
            .position(|e| e.class_id == class_id && e.field_count == field_count)
        {
            self.mru = idx as u8;
            return idx;
        }
        let layout = cratonvm_types::class_layout_for_fields(class_id, field_count);
        let inline = layout.as_ref().and_then(|l| {
            if l.ref_disps.len() <= 8
                && l.ref_disps
                    .iter()
                    .all(|&d| (d as usize).saturating_add(8) <= l.total_size as usize)
            {
                let mut disps = [0u32; 8];
                for (i, &d) in l.ref_disps.iter().enumerate() {
                    disps[i] = d;
                }
                Some(CachedCompactRefs {
                    total_size: l.total_size,
                    ref_len: l.ref_disps.len() as u8,
                    disps,
                })
            } else {
                None
            }
        });
        let slot = self.cursor;
        self.cursor = (self.cursor + 1) & 7;
        self.entries[slot] = CacheEntry {
            class_id,
            field_count,
            inline,
            arc: layout,
        };
        self.mru = slot as u8;
        slot
    }

    #[inline(always)]
    pub fn get_inline(
        &mut self,
        class_id: u32,
        field_count: u32,
    ) -> Option<CachedCompactRefs> {
        let idx = self.find_or_insert_slot(class_id, field_count);
        self.entries[idx].inline
    }

    #[inline]
    pub fn get(
        &mut self,
        class_id: u32,
        field_count: u32,
    ) -> Option<&cratonvm_types::CompactLayout> {
        let idx = self.find_or_insert_slot(class_id, field_count);
        self.entries[idx].arc.as_deref()
    }

    #[inline]
    pub fn get_arc(
        &mut self,
        class_id: u32,
        field_count: u32,
    ) -> Option<std::sync::Arc<cratonvm_types::CompactLayout>> {
        let idx = self.find_or_insert_slot(class_id, field_count);
        self.entries[idx].arc.clone()
    }

    #[inline(always)]
    pub fn total_size(&mut self, class_id: u32, field_count: u32) -> Option<u32> {
        let idx = self.find_or_insert_slot(class_id, field_count);
        let e = &self.entries[idx];
        e.inline
            .map(|r| r.total_size)
            .or_else(|| e.arc.as_deref().map(|l| l.total_size))
    }
}

#[inline]
fn visit_compact_field(
    obj_ptr: *const u8,
    header: &ObjectHeader,
    layout: &cratonvm_types::CompactLayout,
    index: usize,
    offset: u32,
    cache: Option<&mut CompactLayoutCache>,
    visit: &mut impl FnMut(*mut u8, usize, bool, Option<&mut CompactLayoutCache>),
) {
    let end = offset as usize + 8;
    if end > layout.total_size as usize {
        let n = COMPACT_WALK_OFFSET_PAST_BODY.fetch_add(1, Ordering::Relaxed) + 1;
        if n <= 8 || n.is_power_of_two() {
            tracing::warn!(
                "[g1] compact reference walk REFUSED a field offset past the holder's own body (#{n}): holder=0x{:x} class_id={} field_count={} index={index} disp={offset} total_size={} -- the layout this walk resolved places a reference field outside the body that layout declares, so reading (and rewriting) it would land in the NEXT object. The field is skipped and the walk continues.",
                obj_ptr as usize,
                header.class_id.as_u32(),
                header.num_slots(),
                layout.total_size,
            );
        }
        return;
    }
    let slot = unsafe { obj_ptr.add(offset as usize) } as *mut u8;
    let raw = unsafe { std::ptr::read(slot as *const u64) } as usize;
    if raw != 0 {
        visit(slot, raw, true, cache);
    }
}

#[inline]
fn for_each_compact_layout_reference(
    obj_ptr: *const u8,
    header: &ObjectHeader,
    layout: &cratonvm_types::CompactLayout,
    first_index: usize,
    mut cache: Option<&mut CompactLayoutCache>,
    visit: &mut impl FnMut(*mut u8, usize, bool, Option<&mut CompactLayoutCache>),
) {
    if first_index == 0 {
        for &offset in &layout.ref_disps {
            visit_compact_field(obj_ptr, header, layout, usize::MAX, offset, cache.as_deref_mut(), visit);
        }
    } else {
        for (index, (&offset, &is_ref)) in
            layout.field_disps.iter().zip(layout.is_ref.iter()).enumerate()
        {
            if is_ref && index >= first_index {
                visit_compact_field(obj_ptr, header, layout, index, offset, cache.as_deref_mut(), visit);
            }
        }
    }
}

#[track_caller]
#[inline]
pub fn for_each_flat_object_reference_cached(
    obj_ptr: *const u8,
    header: &ObjectHeader,
    first_index: usize,
    max_slots: usize,
    cache: &mut CompactLayoutCache,
    mut visit: impl FnMut(*mut u8, usize, bool, &mut CompactLayoutCache),
) {
    for_each_flat_object_reference_impl(
        obj_ptr,
        header,
        first_index,
        max_slots,
        Some(cache),
        |slot, raw, compact, lc| visit(slot, raw, compact, lc.expect("compact layout cache")),
    );
}

#[track_caller]
pub fn for_each_flat_object_reference_capped(
    obj_ptr: *const u8,
    header: &ObjectHeader,
    first_index: usize,
    max_slots: usize,
    mut visit: impl FnMut(*mut u8, usize, bool),
) {
    for_each_flat_object_reference_impl(
        obj_ptr,
        header,
        first_index,
        max_slots,
        None,
        |slot, raw, compact, _| visit(slot, raw, compact),
    );
}

#[track_caller]
#[inline]
fn for_each_flat_object_reference_impl(
    obj_ptr: *const u8,
    header: &ObjectHeader,
    first_index: usize,
    max_slots: usize,
    mut cache: Option<&mut CompactLayoutCache>,
    mut visit: impl FnMut(*mut u8, usize, bool, Option<&mut CompactLayoutCache>),
) {
    // OPTION D — refuse, and count the refusal. See `FLAT_WALK_REFUSED_ARRAY`
    // for why refusing is safe here and why the earlier census-only form was
    // wrong to hedge: all fifteen call sites of this helper pre-branch on
    // `header.kind() == ObjectKind::Array` and run their own 8-byte element
    // walk in that arm, so no caller reaches this walk for an array ON PURPOSE.
    // An `Array` arriving here therefore means the kind the caller read and the
    // kind this header reports are not the same kind — a torn or rewritten
    // header, i.e. the exact kind confusion §3 of the page describes — and
    // walking it flat would stride SLOT_SIZE over array payload for
    // `array_length` iterations, because `num_slots` and `array_length` are the
    // same `shape` dword.
    if header.kind() == ObjectKind::Array {
        let n = FLAT_WALK_REFUSED_ARRAY.fetch_add(1, Ordering::Relaxed) + 1;
        if n <= 8 || n.is_power_of_two() {
            tracing::warn!(
                "[g1] flat 16-byte-slot walk REFUSED an ARRAY header (#{n}): \
                 obj=0x{:x} class_id={} element_type={:?} array_length={} — `num_slots` and \
                 `array_length` share the `shape` dword, so this walk would have strided \
                 SLOT_SIZE over {} elements of array payload and decoded each as a `Value`. \
                 Every caller pre-branches on kind, so this is a kind confusion, not an \
                 intended array walk; see \
                 what-should-a-walker-do-with-an-unvalidated-header-count-FIXED-20260826.md",
                obj_ptr as usize,
                header.class_id.as_u32(),
                header.element_type(),
                header.array_length(),
                header.array_length(),
            );
        }
        return;
    }
    if cratonvm_types::is_compact_object(header) {
        if let Some(cache_ref) = cache.as_deref_mut() {
            let inline = if first_index == 0 {
                cache_ref.get_inline(header.class_id.as_u32(), header.num_slots())
            } else {
                None
            };
            if let Some(refs) = inline {
                for i in 0..refs.ref_len as usize {
                    let slot = unsafe { obj_ptr.add(refs.disps[i] as usize) } as *mut u8;
                    let raw = unsafe { std::ptr::read(slot as *const u64) } as usize;
                    if raw != 0 {
                        visit(slot, raw, true, cache.as_deref_mut());
                    }
                }
            } else if let Some(layout) = cache_ref.get_arc(header.class_id.as_u32(), header.num_slots()) {
                for_each_compact_layout_reference(
                    obj_ptr,
                    header,
                    &layout,
                    first_index,
                    cache,
                    &mut visit,
                );
            }
        } else {
            let _ = cratonvm_types::with_class_layout(
                header.class_id.as_u32(),
                header.num_slots(),
                |layout| {
                    for_each_compact_layout_reference(
                        obj_ptr,
                        header,
                        layout,
                        first_index,
                        None,
                        &mut visit,
                    );
                },
            );
        }
    } else {
        // `max_slots` is the caller's region-derived bound; `usize::MAX` from
        // the uncapped entry point leaves the old behaviour exactly as it was.
        let end = (header.num_slots() as usize).min(max_slots);
        for index in first_index..end {
            let slot = unsafe { obj_ptr.add(HEADER_SIZE + index * SLOT_SIZE) } as *mut u8;
            // Discriminant-screened: see `heap::read_value_cell_checked`. An
            // unchecked transmute here turns a swept-and-reused cell into a
            // `Value` with an out-of-range tag — UB, and it hands `visit` a
            // garbage reference. Corrupt cells decode to `Value::Object(None)`
            // and are skipped by the `if let` below.
            let value = unsafe {
                cratonvm_gc_core::heap::read_value_cell_checked(
                    slot as *const Value,
                    "g1::for_each_object_reference",
                )
            };
            if let Value::Object(Some(reference)) = value {
                visit(slot, reference.as_ptr() as usize, false, cache.as_deref_mut());
            }
            // NAME THE HOLDER, not just the slot.
            //
            // `read_value_cell_checked` already reports a corrupt cell, but it
            // reports a SLOT ADDRESS and two raw words, and it stops after 32
            // (`n < 32` there). Thirty-two identical-looking lines cannot say
            // WHICH object was being walked, or why this walk took the legacy
            // 16-byte-cell arm for it -- and "always exactly 32" is that cap,
            // not a property of the defect.
            //
            // Keyed on THIS CELL'S OWN DISCRIMINANT, not on a counter.
            //
            // It used to be keyed on `cell_census::decoded()` moving across the
            // read, with the reasoning that a corrupt cell decodes to
            // `Value::Object(None)` and so cannot be told from a genuine null
            // by its value alone. That reasoning is right and the mechanism was
            // wrong: `cell_census` is a PROCESS-GLOBAL counter and this walk
            // runs on every evacuation worker at once, so under
            // `CRATONVM_G1_WORKERS=16` one worker's corrupt cell moved the
            // counter that a different worker then attributed to whatever cell
            // it had just read.
            //
            // MEASURED 2026-09-08 (`TestHostConfigAutomaticDeploymentXmlExternalWarXml`,
            // Linux, -Xmx1g, 16 workers): of 20 grid verdicts in one run, two of
            // the first three named cells holding `raw0=0x4 raw1=<a heap
            // pointer>` -- an ordinary, VALID `Value::Object`, one of which the
            // reference-write watch showed this same pause had just written
            // itself. The population this family has been counted from is
            // therefore mixed, and the mixing rate rises with worker count.
            //
            // `read_value_checked_atomic`'s own test is `w0 as u32 >
            // VALUE_MAX_DISCRIMINANT`, over the sixteen bytes already read
            // here. Applying it directly is exact, thread-local and free.
            //
            // Measured 2026-09-06 on `org.h2.test.store.TestMVStoreTool`
            // (-Xmx256m, G1, `CRATONVM_G1_JIT_MARK_DRIVER=1`): >=32 corrupt
            // cells per run, in one burst inside a single second, always after
            // mixed evacuation has run, one run in three escalating to a fatal
            // `forwarding target must have its low 2 bits clear`. Never once in
            // a driver-off run, including one that took 232 young pauses -- so
            // it is not "more GC finds more". Ruled out by ablation:
            // `CRATONVM_COMPACT_TLAB_ALLOC=0` (2/2 still corrupt) and
            // `CRATONVM_G1_SCRUB_FREE=1` (this file's own first suggestion for
            // a G1 corruption hunt -- 1/2 still corrupt).
            //
            // What is left is this branch's own premise: `is_compact_object`
            // said the holder is NOT compact, so the walk strode 16-byte cells.
            // These are the fields that decided that.
            //
            // ANSWERED, first run it fired (2026-09-06, 700 s, 30 mixed pauses,
            // rc=139). The second horn: the holder is not an object.
            //
            //   holder=0x1f506711e40 class_id=163587104 num_slots=501
            //                        mark=0x310001f509c02470 gc_age=3
            //   holder=0x1f50e091ad8 class_id=163587104 num_slots=501
            //                        mark=0x200001f509c02470 gc_age=2
            //   holder=0x1f50e646b20 class_id=163587104 num_slots=501
            //                        mark=0x100001f509c02470 gc_age=1
            //
            // Three addresses, one shape, ages 1/2/3: ONE object that three
            // successive evacuations copied. `class_id` 163587104 is 0x09C02470
            // and `num_slots` 501 is 0x1F5, so the header's first eight bytes are
            // the single word 0x000001F509C02470 -- a HEAP POINTER, in a run
            // whose heap is at 0x1f5.......  The mark word beside it is
            // well-formed (a plausible quartet, an age that increments per
            // copy), so this is not a torn header: something wrote a pointer
            // over the `class_id`/`shape` dword pair and left the mark alone.
            //
            // The cells the walk then read are the payload of a Java string --
            // raw0=0x6f57206f6c6c6548 is "Hello Wo", raw1=0x3532363620646c72 is
            // "rld 6625". The walk is reading string bytes as reference cells.
            //
            // The producer is named one line earlier in the same log:
            //   [g1] IMPLAUSIBLE legacy header at ref-slot-candidate (#5):
            //   obj=0x1f507f00300 class_id=163587104 num_slots=501
            //   mark=0x000001f50e646b23 claims=0x1f60 bytes source=r127/Old/off=0x30
            // -- mark 0x...b23 is 0x1f50e646b20 tagged 0x3, a FORWARDING
            // pointer, and 0x1f50e646b20 is the third holder above. A fourth
            // report has mark 0x000001f505b00383 against holder 0x1f505b00380:
            // its own address, tagged 3 -- a SELF-forward, the
            // evacuation-failure path.
            //
            // So the next question is not about layout at all: who leaves a
            // forwarding pointer where a walk later reads a `class_id`, and why
            // does `retire_forwards` not reach it? That is where a fix goes.
            //
            // ANSWERED AGAIN, and it was not `retire_forwards`. That pointer
            // came from the PARALLEL evacuator, which had none of the serial
            // arm's header screens and is the default arm -- fixed the same day
            // under `CRATONVM_G1_PARALLEL_EVAC_SCREEN`. The `class_id` above,
            // 163587104 = 0x09C02470, is the low half of the heap pointer
            // 0x1f509c02470 printed beside it, which is that page's fabrication
            // pattern verbatim. Re-measured on the merged tip: 0 corrupt cells
            // in 3/3 driver-on runs, where every run before it had >=32.
            //
            // This report therefore has no known live producer. It stays
            // because the crash it was built to explain did NOT go with it:
            // 2 of 3 driver-on runs still SIGSEGV, tracking `IMPLAUSIBLE legacy
            // header` refusals (13 and 8 in the two that crashed, 0 in the one
            // that did not) rather than corrupt cells. If this fires again it
            // means a second producer, and the fields above are what separate
            // it from the one already fixed.
            // SAFETY: the walk read sixteen bytes at `slot` above.
            let cell_w0 = unsafe { std::ptr::read(slot as *const u64) };
            if (cell_w0 as u32) > cratonvm_types::VALUE_MAX_DISCRIMINANT {
                let n = FLAT_WALK_CORRUPT_CELL_HOLDER.fetch_add(1, Ordering::Relaxed) + 1;
                // Same rate limit as the report below, NOT `n <= 8` alone: `n` is a
                // RUN-GLOBAL counter, so `n <= 8` samples only the first eight
                // corrupt cells a run ever sees. If those are pushed before any
                // drain runs -- which is exactly what happened on the Tomcat
                // class, where the drain was wired to the serial bodies only --
                // they sit in the list, nothing further is ever sampled, and a
                // crash before the next drain loses the whole run. Refilling on
                // the power-of-two samples keeps the instrument alive for the
                // rest of the run at the same bounded cost.
                if n <= 8 || n.is_power_of_two() {
                    // SAFETY: the walk just read sixteen bytes here.
                    let (raw0, raw1) = unsafe {
                        (
                            std::ptr::read(slot as *const u64),
                            std::ptr::read((slot as *const u64).add(1)),
                        )
                    };
                    queue_pending_corrupt_holder(
                        obj_ptr as *mut u8,
                        header,
                        slot as usize,
                        index,
                        raw0,
                        raw1,
                        "corrupt-cell",
                    );
                }
                if n <= 8 || n.is_power_of_two() {
                    // WHICH WALK, and WHAT the header's first word actually is.
                    //
                    // The report used to print `class_id` and `num_slots` as
                    // two decimal fields, and every reader has had to recombine
                    // them by hand to see the thing that matters: on the
                    // measured population they are the two halves of ONE HEAP
                    // POINTER (`class_id` 163587104 = 0x09C02470 under
                    // `num_slots` 501 = 0x1F5, in a heap at 0x1f5...).
                    // `header_word0` is that recombination and
                    // `word0_plausible_ptr` is the test, so "someone wrote a
                    // pointer over the class_id/shape dword pair" is a FIELD
                    // rather than an inference the reader has to redo.
                    //
                    // `caller` is the `#[track_caller]` location of whichever of
                    // this file's fourteen walk sites is running. The holder
                    // shape alone cannot say whether this is the evacuator, the
                    // mark scan, the rset recorder or the verifier, and those
                    // have different producers and different fixes. Measured
                    // 2026-09-06: the same shape appears with
                    // `CRATONVM_G1_PARALLEL_EVAC=0`, so the producer is NOT the
                    // parallel evacuator that produced the 2026-09-05 family.
                    // r11w18 g1store: the REAL first word since the 8-byte
                    // header -- `class_id` + mark word, not `class_id` + shape.
                    let word0 = header_first_word(header);
                    // SAFETY: the walk read sixteen bytes at `slot` above.
                    let (raw0, raw1) = unsafe {
                        (
                            std::ptr::read(slot as *const u64),
                            std::ptr::read((slot as *const u64).add(1)),
                        )
                    };
                    tracing::warn!(
                        "[g1] legacy 16-byte-cell walk hit a CORRUPT CELL (#{n}): caller={} holder=0x{:x} class_id={} num_slots={} kind={:?} header_word0=0x{word0:016x} word0_arena_ptr={} word0_plausible_ptr_LOOSE={} mark=0x{:016x} gc_flags=0x{:x} gc_age={} is_compact={} slot=0x{:x} slot_index={index} end={end} raw0=0x{raw0:016x} raw1=0x{raw1:016x} raw0_tag={} raw0_untagged_is_arena_ptr={} {} -- the cell screen rejected this word.",
                        std::panic::Location::caller(),
                        obj_ptr as usize,
                        header.class_id.as_u32(),
                        header.num_slots(),
                        header.kind(),
                        // THE FIELD THIS REPORT USED TO CARRY WAS THE WRONG TEST.
                        //
                        // `word0_plausible_ptr` was
                        // `cratonvm_types::plausible_heap_pointer(word0)`, which
                        // asks only non-null / 8-aligned / under 2^47. It answers
                        // TRUE for `0x0000001200000040` -- an ordinary
                        // `class_id=64 / num_slots=18` header pair -- and that
                        // value sits 2157 GiB below any measured arena base.
                        // `g1-eight-byte-write-at-a-live-objects-base-20260906`
                        // rests its "19 of 19 holders had word0_plausible_ptr=true"
                        // on this field; re-taken against the arena bounds the
                        // refusing screens use, the same population scored 0 of 25.
                        // The arena test is now first and the loose one is kept
                        // beside it, renamed so the two can never again be quoted
                        // as one number.
                        word_is_arena_pointer(word0),
                        cratonvm_types::plausible_heap_pointer(word0),
                        header.mark_word.load(Ordering::Relaxed),
                        header.gc_flags(),
                        header.gc_age(),
                        cratonvm_types::is_compact_object(header),
                        slot as usize,
                        // `raw0 & 3 == 3` is `MARK_FORWARDED`. A corrupt cell
                        // whose first word is a TAGGED heap pointer and whose
                        // second word is zero is bit-for-bit the first sixteen
                        // bytes of a FORWARDED object -- mark word, then a zero
                        // body word. That is the "read misaligned by 8" horn, and
                        // it is distinguished from the "8-byte write into a legacy
                        // cell" horn by `ref_write_watch` below, which says
                        // whether a collector write actually landed here.
                        raw0 & 0b11,
                        word_is_arena_pointer(raw0 & !0b11u64),
                        ref_write_provenance(slot as usize),
                    );
                }
            }
        }
    }
}

/// How many objects the parallel evacuator placed in an EXISTING destination
/// region after its Free pool ran out, instead of failing the evacuation.
///
/// Every one of these was an evacuation FAILURE before 2026-09-06 — a
/// self-forward, a kept CSet region, and a `retry_after_evacuation_failure`
/// pass — while the serial arm would have found the same space.
pub static PARALLEL_SHARED_DEST_ALLOCS: AtomicUsize = AtomicUsize::new(0);

/// How many times a promotion TLAB took up an Old region a previous pause left
/// partly filled, instead of consuming a fresh Free one.
///
/// Read it against the pause count: with `w` evacuation workers, `w` claims per
/// pause and a resume count near zero is the pre-2026-09-06 behaviour, in which
/// the old generation grew by `w` regions every young pause however little was
/// promoted. See `SharedEvac::resume`.
pub static PARALLEL_EVAC_RESUMED_DEST_REGIONS: AtomicUsize = AtomicUsize::new(0);

/// How many times the parallel evacuator's TLAB allocator ran out of POOL —
/// the Free regions reserved before the dispatch — and sent an object down the
/// evacuation-failure path.
///
/// Its serial twin does not fail at the same point: it first scans every
/// existing non-CSet region of the destination type. A non-zero count here
/// beside a non-zero `with_room` in the report means the parallel arm declared
/// to-space exhaustion while to-space still had room.
pub static PARALLEL_TLAB_POOL_EXHAUSTED: AtomicUsize = AtomicUsize::new(0);

/// Rate-limit counter for the corrupt-cell holder report above.
pub static FLAT_WALK_CORRUPT_CELL_HOLDER: AtomicUsize = AtomicUsize::new(0);

/// Corrupt-cell holders awaiting a GRID VERDICT, drained once per pause by
/// [`G1Collector::report_pending_corrupt_holders`].
///
/// The report in `for_each_flat_object_reference_capped` can name the holder's
/// header but not whether that address is a REAL OBJECT START, because it is a
/// free function with no collector and no region table. That one field is the
/// whole question: `grid=OBJECT-START` says an allocator put an object there
/// and something later wrote over its header, so there is a writer to find;
/// `grid=INTERIOR` says the address was never an object start, the words read
/// as a header are a neighbour's slots, and there is no header writer at all.
/// Bounded, because the answer does not get truer after the eighth sample.
/// One corrupt-cell report, held until a pause body can answer the questions a
/// free function cannot: is the HOLDER at a real object start, and -- the
/// question added here -- does the region's own object grid place the CORRUPT
/// SLOT inside that holder, or inside the object that follows it?
///
/// The second question is the discriminator this family has been missing. A
/// slot the grid places in a NEIGHBOUR means the holder's declared body is
/// longer than the bytes the allocator gave it, so the walk over-strode and the
/// "corrupt cell" is the neighbour's header and mark word read as a `Value`
/// (`raw0 = target | MARK_FORWARDED`, `raw1 = 0` is exactly a forwarded
/// object's first sixteen bytes). A slot the grid places INSIDE the holder is a
/// genuine foreign write into a live object's body, which is the defect this
/// page has been hunting.
#[derive(Clone, Copy)]
pub struct PendingCorruptHolder {
    pub holder: usize,
    pub class_id: u32,
    pub num_slots: u32,
    pub mark: u64,
    /// The cell the screen rejected, and its two raw words as the walk read
    /// them.
    pub slot: usize,
    pub slot_index: usize,
    pub raw0: u64,
    pub raw1: u64,
    /// `#[track_caller]` location of the WALK that read it.
    pub caller: &'static std::panic::Location<'static>,
    /// Which screen queued this holder. `"corrupt-cell"` is the legacy-cell
    /// walk's own report; the others are the three holder REFUSALS, which
    /// until 2026-09-08 produced no grid verdict at all -- so the population
    /// `g1-eight-byte-write-at-a-live-objects-base-20260906` calls "the genuine
    /// shape" was the one population never asked whether it was at an object
    /// start.
    pub reason: &'static str,
}

/// Queue a holder for the end-of-pause grid verdict. Bounded at eight per
/// drain, like the corrupt-cell path it was factored out of.
///
/// `slot`/`raw0`/`raw1` describe the CELL for the corrupt-cell route; for a
/// holder refusal there is no single cell, so the holder's own first sixteen
/// bytes are recorded instead -- which is exactly what the refusal is about.
#[track_caller]
pub fn queue_pending_corrupt_holder(
    holder: *mut u8,
    header: &ObjectHeader,
    slot: usize,
    slot_index: usize,
    raw0: u64,
    raw1: u64,
    reason: &'static str,
) {
    let mut pend = PENDING_CORRUPT_HOLDERS.lock();
    if pend.len() >= 8 {
        return;
    }
    pend.push(PendingCorruptHolder {
        holder: holder as usize,
        class_id: header.class_id.as_u32(),
        num_slots: header.num_slots(),
        mark: header.mark_word.load(Ordering::Relaxed) as u64,
        slot,
        slot_index,
        raw0,
        raw1,
        caller: std::panic::Location::caller(),
        reason,
    });
}

pub static PENDING_CORRUPT_HOLDERS: Mutex<Vec<PendingCorruptHolder>> = Mutex::new(Vec::new());

/// How many holders the parallel reference scan refused because their first
/// header word is a pointer into the collector's own arena.
///
/// Expected to be ZERO. Non-zero means an eight-byte write landed at a live
/// object's base DURING a pause — the holder was a sound object when
/// `evacuate` copied it onto this queue.
pub static PARALLEL_SCAN_HOLDER_WORD0_IS_POINTER: AtomicUsize = AtomicUsize::new(0);

/// How many holders the SERIAL reference scan refused for the same reason.
///
/// Expected to be ZERO. Split from the parallel counter on purpose: the two
/// arms are reached by different pauses, and one number for both cannot say
/// which walk a run's corruption came through.
pub static SERIAL_SCAN_HOLDER_WORD0_IS_POINTER: AtomicUsize = AtomicUsize::new(0);

/// How many holders the reference scan refused because their declared body is
/// larger than a whole region.
///
/// Expected to be ZERO. Measured as the residual of the word0 screen on H2,
/// 2026-09-06: 20 of 20 reports in two runs were one `num_slots=65536` holder.
///
/// gci i19/yc: counts BOTH evacuators now. The parallel `process_object` (the
/// default arm) clamped such a holder to its region end and then walked -- and
/// REWROTE -- every cell to that end; it refuses it now, as the serial arm and
/// the Phase-4 fix-up already did. No new static: the name keeps its old
/// spelling, the population is "either arm".
pub static SERIAL_SCAN_HOLDER_BODY_TOO_BIG: AtomicUsize = AtomicUsize::new(0);

/// How many holders the PHASE-4 fixup refused, by either impossibility test.
///
/// Expected to be ZERO. Separate from the evacuation-time counters because
/// Phase 4 runs after them: a non-zero count here with zeros there means a
/// header went bad between evacuation and the fixup, which is a different
/// window from the one the evacuation screens close.
pub static PHASE4_HOLDER_REFUSED: AtomicUsize = AtomicUsize::new(0);

/// Forwards still set after `retire_forwards`, across the run.
///
/// Expected ZERO. `CRATONVM_G1_VERIFY_FORWARDS_RETIRED=1` only.
pub static UNRETIRED_FORWARDS: AtomicUsize = AtomicUsize::new(0);

/// How many evacuation candidates carried the EMPTY-HEADER shape and were
/// therefore proved against the region's object grid.
///
/// This is the cost side of [`G1Collector::empty_header_is_a_real_object`]: one
/// linear region walk each. Read it against
/// [`EVAC_EMPTY_HEADER_REFUSED`] -- a large count here with a zero
/// there means the shape is common and genuine, and the screen is paying for
/// nothing.
pub static EVAC_EMPTY_HEADER_PROVED: AtomicUsize = AtomicUsize::new(0);

/// How many empty-header candidates were WAIVED because this pause had already
/// spent its grid-proof budget.
///
/// Expected to be ZERO on any workload that is not pathological. A non-zero
/// count means the empty-header shape arrives more than
/// [`EMPTY_HEADER_PROOF_BUDGET_PER_PAUSE`] times in one pause, so the screen is
/// no longer covering all of them and the budget is what to raise -- or the
/// shape is genuine and common, which the ratio to
/// [`EVAC_EMPTY_HEADER_REFUSED`] says.
pub static EVAC_EMPTY_HEADER_WAIVED: AtomicUsize = AtomicUsize::new(0);

/// Grid proofs this pause has already paid for; reset at the pause funnel.
pub static EMPTY_HEADER_PROOFS_THIS_PAUSE: AtomicUsize = AtomicUsize::new(0);

/// How many O(region) grid walks one pause will pay to prove empty-header
/// candidates.
///
/// The proof is exact and the shape is rare -- 9 to 17 refusals per WHOLE RUN
/// on the class this was measured against -- but "rare" is a property of a
/// workload and this is a per-candidate cost on the evacuation path, so it is
/// bounded rather than trusted. Above the budget the candidate is accepted, as
/// it was before 2026-09-08: a refusal that is WRONG drops a live reference,
/// and an unproved candidate is not a proved-bad one.
pub const EMPTY_HEADER_PROOF_BUDGET_PER_PAUSE: usize = 64;

/// How many of those the grid said were not object starts.
///
/// Expected to be ZERO. Non-zero means a reference slot (or a conservative
/// root) named the PAYLOAD WORD of a null `Value` cell, whose sixteen bytes
/// read as a zero-field class-0 object and pass every header screen there is.
pub static EVAC_EMPTY_HEADER_REFUSED: AtomicUsize = AtomicUsize::new(0);

/// How many evacuation CANDIDATES were refused because their first header
/// word recombines to a pointer into this collector's arena.
///
/// Expected to be ZERO. Non-zero means a reference slot named an address that
/// is not an object start -- and every one of them, before 2026-09-08, was
/// followed into `evacuate`, which installed a forwarding mark word eight bytes
/// into whatever live object actually owned those bytes.
pub static EVAC_REF_REJECTED_ARENA: AtomicUsize = AtomicUsize::new(0);

/// How many `MARK_FORWARDED` mark words decoded to a target that
/// [`cratonvm_types::ObjectHeader::make_forwarded`] could never have installed.
///
/// Expected to be ZERO, and non-zero is not a degradation to tolerate: the
/// decoded value is returned to the evacuator as "where this object went", is
/// STORED into the reference slot being scanned, and is pushed onto the gray
/// worklist as a holder. See
/// [`G1Collector::decode_forwarding_target`].
pub static FORWARD_TARGET_IMPLAUSIBLE: AtomicUsize = AtomicUsize::new(0);

/// How many addresses the three unscreened evacuation SUPPLY routes offered
/// that are not object starts. See
/// [`G1Collector::evacuation_supply_is_an_object`].
///
/// Expected to be ZERO. A non-zero count is the producer
/// `g1-eight-byte-write-at-a-live-objects-base-20260906` spent its whole census
/// looking for on the READ side.
pub static EVAC_SUPPLY_NON_OBJECT: AtomicUsize = AtomicUsize::new(0);

/// How many compact reference-field offsets were skipped because the resolved
/// layout places them past the body that same layout declares.
///
/// Expected to be ZERO. A non-zero count means the layout an object was
/// allocated under and the layout it is being walked with are not the same,
/// which for the COMPACT arm is a write of eight bytes into the next object.
pub static COMPACT_WALK_OFFSET_PAST_BODY: AtomicUsize = AtomicUsize::new(0);

pub fn write_flat_object_reference(slot: *mut u8, raw: usize, compact: bool) {
    if compact {
        unsafe { std::ptr::write(slot as *mut u64, raw as u64) };
    } else {
        let value = Value::Object(Some(unsafe { ObjectRef::from_raw(raw as *mut u8) }));
        unsafe { cratonvm_types::write_value_atomic(slot as *mut Value, value) };
    }
}

/// [`write_flat_object_reference`], recorded by [`RefWriteWatch`] when the
/// watch is armed.
///
/// Every reference-slot rewrite in this file goes through this or
/// [`note_ref_write`], so "did a collector write land on this slot" is a
/// lookup. `#[track_caller]` so the location the watch stores is the WALK's,
/// which is the only part of the stack that distinguishes the six flat-walk
/// sites from each other.
///
/// `holder_end` is the extent the CALLER believes the holder has. It is
/// derived from the holder's own header, so it is a circular bound and cannot
/// by itself prove a write stayed inside a real object -- that is what the
/// drain's `grid_object_containing` is for. What it does catch, for free, is a
/// walk whose stride and whose bound disagree.
#[track_caller]
#[inline]
pub fn write_flat_object_reference_watched(
    slot: *mut u8,
    raw: usize,
    compact: bool,
    holder: *mut u8,
    holder_end: usize,
) {
    note_ref_write(
        slot as usize,
        raw as u64,
        holder as usize,
        holder_end,
        if compact { 8 } else { 16 },
    );
    write_flat_object_reference(slot, raw, compact);
}

/// The extent a walk believes its holder has: `holder + object_total_size`,
/// saturating, and `0` when the header does not size (which
/// [`note_ref_write`] reads as "no bound to check").
#[inline]
pub fn holder_extent(obj_ptr: *mut u8, header: &ObjectHeader) -> usize {
    if REF_WRITE_WATCH.get().is_none() {
        return 0;
    }
    let size = object_total_size(header);
    if size < HEADER_SIZE {
        return 0;
    }
    (obj_ptr as usize).saturating_add(size)
}

#[inline]
pub unsafe fn array_element_from_unaligned_ptr(element_type: ArrayElementType, p: *const u8) -> Value {
    // SAFETY: `p` points at the native-endian bytes for one array element.
    unsafe {
        match element_type {
            ArrayElementType::Int => Value::Int(std::ptr::read_unaligned(p as *const i32)),
            ArrayElementType::Long => Value::Long(std::ptr::read_unaligned(p as *const i64)),
            ArrayElementType::Float => Value::Float(std::ptr::read_unaligned(p as *const f32)),
            ArrayElementType::Double => Value::Double(std::ptr::read_unaligned(p as *const f64)),
            ArrayElementType::Byte => Value::Int(std::ptr::read_unaligned(p as *const i8) as i32),
            ArrayElementType::Boolean => Value::Int(std::ptr::read_unaligned(p) as i32),
            ArrayElementType::Short => Value::Int(std::ptr::read_unaligned(p as *const i16) as i32),
            ArrayElementType::Char => Value::Int(std::ptr::read_unaligned(p as *const u16) as i32),
            ArrayElementType::Reference => {
                let r: u64 = std::ptr::read_unaligned(p as *const u64);
                if r == 0 {
                    Value::Object(None)
                } else {
                    Value::Object(Some(ObjectRef::from_raw(r as usize as *mut u8)))
                }
            }
        }
    }
}

#[inline]
pub fn array_element_from_bytes(element_type: ArrayElementType, raw: &[u8; 8]) -> Value {
    // SAFETY: `raw` holds the native-endian bytes for one array element.
    unsafe { array_element_from_unaligned_ptr(element_type, raw.as_ptr()) }
}

#[inline]
pub unsafe fn array_element_to_unaligned_ptr(element_type: ArrayElementType, value: Value, p: *mut u8) {
    // SAFETY: every write is at most 8 bytes and the destination may be align-1.
    unsafe {
        match element_type {
            ArrayElementType::Int => {
                std::ptr::write_unaligned(p as *mut i32, value.as_int().unwrap_or(0))
            }
            ArrayElementType::Long => {
                std::ptr::write_unaligned(p as *mut i64, value.as_long().unwrap_or(0))
            }
            ArrayElementType::Float => std::ptr::write_unaligned(
                p as *mut f32,
                match value {
                    Value::Float(f) => f,
                    _ => 0.0,
                },
            ),
            ArrayElementType::Double => std::ptr::write_unaligned(
                p as *mut f64,
                match value {
                    Value::Double(d) => d,
                    _ => 0.0,
                },
            ),
            ArrayElementType::Byte | ArrayElementType::Boolean => {
                std::ptr::write_unaligned(p as *mut i8, value.as_int().unwrap_or(0) as i8)
            }
            ArrayElementType::Short => {
                std::ptr::write_unaligned(p as *mut i16, value.as_int().unwrap_or(0) as i16)
            }
            ArrayElementType::Char => {
                std::ptr::write_unaligned(p as *mut u16, value.as_int().unwrap_or(0) as u16)
            }
            ArrayElementType::Reference => {
                let r: u64 = match value {
                    Value::Object(Some(r)) => r.as_ptr() as u64,
                    _ => 0u64,
                };
                std::ptr::write_unaligned(p as *mut u64, r)
            }
        }
    }
}

#[inline]
pub fn array_element_to_bytes(element_type: ArrayElementType, value: Value, raw: &mut [u8; 8]) {
    // SAFETY: every write is at most 8 bytes.
    unsafe { array_element_to_unaligned_ptr(element_type, value, raw.as_mut_ptr()) };
}

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

/// Object budget for the post-evacuation CSet verification pass in a build
/// where it is not already unbounded (audit §9 item 2).
///
/// `CRATONVM_G1_VERIFY_BUDGET=<n>`; `0` disables the pass. The default is
/// deliberately small enough to be affordable on every pause rather than large
/// enough to feel thorough: the pass is a SAMPLER whose start region rotates,
/// so its value comes from running always and accumulating coverage, not from
/// any single pause being complete. A 4096-object budget is on the order of a
/// hundred microseconds of pointer chasing, against pauses measured in
/// milliseconds.
pub fn verify_budget() -> usize {
    match Err::<String, std::env::VarError>(std::env::VarError::NotPresent) {
        Ok(v) => v.trim().parse::<usize>().unwrap_or(4096),
        Err(_) => 4096,
    }
}

/// gci i7/g1y — `CRATONVM_G1_VERIFY_UNBOUNDED_ON_VERBOSE_GC`, default OFF:
/// should `--verbose:gc` switch the post-pause CSet verifier
/// ([`G1Collector::verify_no_dangling_into_cset`]) from its budgeted sampler to
/// the whole-heap sweep, as it did before this wave?
///
/// OFF by default because a logging switch must not change the pause it logs:
/// the whole-heap sweep cost ~96 ms of a ~200-255 ms young pause on G1
/// 4-thread javac at -Xmx512m and grew with the old generation. `=1` is the
/// verification arm for a run that wants the old whole-heap coverage.
///
/// gci i12/sx: read once per collector, into
/// `G1Collector::verify_unbounded_on_verbose_gc`, not in a process `OnceLock`.
pub fn g1_verify_unbounded_on_verbose_gc() -> bool {
    false}

/// Does the CSet verifier run its whole-heap sweep? Debug builds always do;
/// `--verbose:gc` does only under [`g1_verify_unbounded_on_verbose_gc`].
#[inline]
pub fn verify_unbounded_for(debug_build: bool, gc_log_enabled: bool, on_verbose_gc: bool) -> bool {
    debug_build || (gc_log_enabled && on_verbose_gc)
}

/// [`verify_budget`], for the `[GC] g1 cset-verify:` census line.
///
/// W3-C's rule: a census line must name the switch that governs it, or its
/// zeros cannot be read. This line has two switches and they interact, so both
/// are printed beside the numbers they produced.
pub fn verify_budget_for_report() -> usize {
    verify_budget()
}

/// [`verify_sweep_pauses`], for the same line and the same reason.
pub fn verify_sweep_pauses_for_report() -> usize {
    verify_sweep_pauses()
}

/// LANE W4-B — the sampler's target SWEEP PERIOD, in pauses
/// (`CRATONVM_G1_VERIFY_SWEEP_PAUSES=<n>`). `0`, the default, keeps the flat
/// object budget above.
///
/// # Why a flat object budget stops meaning anything as the heap grows
///
/// [`verify_budget`]'s doc rests the flat number on the claim that the pass
/// "is a SAMPLER whose start region rotates, so its value comes from running
/// always and accumulating coverage". The accumulation is real — wave 1 fixed
/// the half that made it a lie, by resuming inside a truncated region instead
/// of at the next one — but the RATE was never checked against a heap size,
/// and the rate is the whole of the claim. Measured on `HumongousChurn`, the
/// same shape at three sizes, budgeted arm against an unbounded arm of the
/// same run (`CRATONVM_GC_STATS=1` versus `--verbose:gc`; see the note in
/// [`G1Collector::verify_no_dangling_into_cset`] about why those are two
/// different budgets):
///
/// | heap | walkable objects per pause | budgeted | coverage/pause | pauses per sweep |
/// |---|---|---|---|---|
/// | 160 MiB | 1 309 466 | 4 096 | 0.31 % | 320 |
/// | 384 MiB | 3 133 209 | 4 096 | 0.13 % | 765 |
/// | 768 MiB | 6 326 395 | 4 096 | 0.065 % | 1 545 |
///
/// The sweep period is exactly linear in the live set, because the budget is
/// constant and the population is not. Extrapolated to the heaps this
/// collector is meant for, one sweep of a 32 GiB heap takes on the order of
/// 65 000 pauses — which is not "slow coverage", it is no coverage, reported
/// as a rotating sampler. Every one of those runs printed
/// `budget_truncated=5` out of 5, i.e. the budget bound on EVERY pause at
/// every size.
///
/// # Why the budget is in BYTES here and in objects there
///
/// The quantity that has to be held constant is the sweep period, and the
/// population the sweep covers is a number of BYTES (the walk advances by
/// `object_total_size`), not a number of objects. Sizing an object budget to
/// a target period would need a mean object size — a second estimator, with
/// its own warm-up and its own way of being wrong. The bytes are already in
/// hand: `walkable_bytes` is one pass over the region table reading `cursor()`,
/// on a walk that reads every one of those cursors anyway.
///
/// The object budget is NOT deleted and is not overridden lightly: it is the
/// cost ceiling, and a budget derived from the heap has to be able to hit one.
/// Both bind, and `budget_truncated` counts a stop by either.
///
/// # Cost
///
/// The unbounded arm of the 160 MiB run walks 1.3 M objects in a `verify_us`
/// of 10.5-18.4 ms, i.e. ~9 ns per object. A sweep period of 64 pauses on the
/// 768 MiB heap is ~99 000 objects per pause, ~0.9 ms — against pauses of
/// 230-330 ms on that workload, so ~0.3 % of the pause, and it SCALES with
/// the pause rather than against it: a bigger heap has both more to verify
/// and longer pauses to hide the verification in. The flat 4 096 is ~38 µs,
/// which is not a budget anyone was defending; it is a number chosen before
/// there was a measurement.
pub fn verify_sweep_pauses() -> usize {
    match Err::<String, std::env::VarError>(std::env::VarError::NotPresent) {
        Ok(v) => v.trim().parse::<usize>().unwrap_or(0),
        Err(_) => 0,
    }
}

/// Is the multi-threaded evacuator available to this process at all?
///
/// `CRATONVM_G1_PARALLEL_EVAC` is an OPT-OUT: the flag is parsed by
/// [`cratonvm_types::flags`]'s `on_unless_zero`, so ON is the answer for unset
/// and for every value except exactly `0` (`CRATONVM_GC=-g1-parallel-evac`
/// resolves to the same `0`). It is read from the process-wide flag snapshot,
/// which is latched once, so the answer cannot change under a running pause.
///
/// This is availability, not dispatch. It says the evacuator may be used; who
/// actually uses it is decided per pause and per phase:
///
/// * YOUNG — [`use_parallel_evacuator`] combines this with the in-JIT term,
///   and `young_collection` takes the parallel driver whenever it says yes,
///   which on a default configuration is every pause.
/// * MIXED — `mixed_collection` requires this AND
///   [`parallel_mixed_enabled`] (`CRATONVM_G1_PARALLEL_MIXED`). This flag alone
///   does NOT route a mixed pause anywhere; see that function for the history
///   of the asymmetry and `g1_mixed_route_counts` for the engagement census
///   that says which arm a run actually took.
///
/// # Why the default was flipped (2026-08-13)
///
/// The flag was an opt-in for as long as G1-9 was open. G1-9 turned out to be a
/// compact-layout scan divergence in this evacuator's own object walk — not a
/// race, and reproducing identically at one worker — which is fixed and
/// covered by `parallel_evacuation_scans_compact_object_reference_fields`.
/// What the flag now selects is a throughput decision, and `=0` is the
/// bisection lever for any suspected parallel-evacuation regression.
///
/// What the flip does NOT claim: it is not a gauntlet result, the worker pool
/// is still spawned per collection rather than persistent, and the
/// G1-as-default-collector question is untouched and separate.
pub fn parallel_evac_enabled() -> bool {
    gc_flags().g1_parallel_evac
}

/// F-01 — should this young pause use the parallel evacuator?
///
/// Extracted from `young_collection`'s dispatch so the truth table can be
/// stated as a test rather than as a comment. The whole content of the change
/// is the third argument: before it, `in_jit` alone vetoed the parallel path,
/// and on a JIT-warm application `in_jit` is true for nearly every pause.
///
/// The reason it could be dropped is in
/// [`cratonvm_gc_core::gc_flags`]`().g1_parallel_evac_in_jit`: the parallel driver applies
/// the same conservative-JIT-root region exclusion the serial one does, because
/// pinning is a collection-set filter and not a property of the evacuation
/// loop.
#[inline]
pub fn use_parallel_evacuator(enabled: bool, in_jit: bool, allowed_in_jit: bool) -> bool {
    enabled && (allowed_in_jit || !in_jit)
}

/// A fallible native allocation is recovery work, not an ordinary throughput
/// pause.  Route that retry through the serial collector, whose destination
/// scan is the established recovery path; ordinary young pauses retain their
/// normal parallel dispatch.
#[inline]
pub fn use_parallel_young_evacuator(
    native_alloc_pressure: bool,
    enabled: bool,
    in_jit: bool,
    allowed_in_jit: bool,
) -> bool {
    !native_alloc_pressure && use_parallel_evacuator(enabled, in_jit, allowed_in_jit)
}

/// Young collections G1 drove through the PARALLEL evacuator, and through the
/// serial one.
///
/// # Why this did not exist, and why that made a measurement impossible
///
/// `EvacPool` is a persistent worker pool with a careful safety argument, and
/// `use_parallel_evacuator` decides per pause whether to use it — but nothing
/// anywhere counted the outcome. G1's `[GC-STAT]` line has no `workers=` field,
/// `evac_pool.rs` has no census, and the dispatcher recorded nothing. So the
/// question "does adding GC workers still cost more than it saves" could not be
/// answered on this backend at all: an A/B over `CRATONVM_GC_PAR_THREADS` cannot
/// be read without knowing whether the lever engaged, and the nearest-looking
/// counter (`[GC] par_evac`) belongs to the GENERATIONAL evacuator in
/// `gen_evac.rs` and reads zero under G1 whatever G1 did.
///
/// That is how a zero from an instrument armed where it cannot fire gets
/// mistaken for a finding, which is exactly what nearly happened here.
///
/// Both halves, always: `parallel=0` alone cannot distinguish the flag being
/// off, the in-JIT term declining every pause, and a run that never collected.
pub static G1_YOUNG_PARALLEL: AtomicUsize = AtomicUsize::new(0);
pub static G1_YOUNG_SERIAL: AtomicUsize = AtomicUsize::new(0);
/// Workers the most recent parallel young collection actually asked for.
///
/// The count is an ergonomic over `CRATONVM_GC_PAR_THREADS` and the machine,
/// not the flag value, so reporting the flag would not say what ran.
pub static G1_EVAC_WORKERS_LAST: AtomicUsize = AtomicUsize::new(0);

/// `(parallel_cycles, serial_cycles, workers_last)` for G1 young evacuation.
pub fn g1_young_evac_counts() -> (usize, usize, usize) {
    (
        G1_YOUNG_PARALLEL.load(Ordering::Relaxed),
        G1_YOUNG_SERIAL.load(Ordering::Relaxed),
        G1_EVAC_WORKERS_LAST.load(Ordering::Relaxed),
    )
}

/// How many candidate references the evacuation ref-scan refused to
/// dereference because they did not look like live object headers.
///
/// Expected to be ZERO. A non-zero value means some writer put a word into a
/// reference slot that is inside the heap's address span but is not an object —
/// which, before the guard, was a SIGSEGV inside `scan_and_evacuate_refs`.
///
/// Reported by `gc_metrics::collector_decision_report`, unconditionally and on
/// both shutdown arms. It used to be printed by
/// [`G1Collector::print_gc_summary`], which is reached only when GC stats are
/// requested — so the zero this counter exists to let someone cite was, on
/// every JUnit workload in the suites, never printed at all.
pub static EVAC_REF_REJECTED: AtomicUsize = AtomicUsize::new(0);

/// The value of [`EVAC_REF_REJECTED`].
pub fn evacuation_refs_rejected() -> usize {
    EVAC_REF_REJECTED.load(Ordering::Relaxed)
}

/// The subset of [`EVAC_REF_REJECTED`] whose refusal is evidence of
/// CORRUPTION rather than of an ordinary dead referent.
///
/// The same split the marker's gate already carries as [`GrayRefusal`], and
/// for the same reason: a candidate at or above its region's cursor, or in a
/// `Free` region, was never a live object in that region's CURRENT
/// incarnation, and a slot naming one is ROUTINE -- freed regions are
/// deliberately not scrubbed (G1AUD-10) and the evacuation walks name dead
/// objects' slots as well as live ones. A candidate INSIDE the allocated
/// prefix whose header does not decode is the other thing entirely: something
/// was placed there and its header does not describe it.
///
/// `EVAC_REF_REJECTED` counted both, which is one bit too few to act on. That
/// is verbatim the defect the 2026-08-30 `classify_mark_scan_target` split
/// fixed on the MARKING side, where 2 226 of 2 249 refusals turned out to be
/// the routine kind and the remaining 23 were the whole story.
/// `TestKillProcessWhileWriting` reports this evacuation-side family in the
/// MILLIONS (`#134217728`), and until this split nothing said which of the two
/// populations that number was.
pub static EVAC_REF_REJECTED_TORN: AtomicUsize = AtomicUsize::new(0);

/// How many reference-slot candidates were refused because their legacy header
/// is IMPLAUSIBLE — a class id in the band no loader mints, or class 0 carrying
/// thousands of fields — rather than because their tag bytes failed to decode.
///
/// Separate from [`EVAC_REF_REJECTED`] because the two name different
/// producers. A tag-screen refusal is a word that is not a header at all; this
/// one is a word whose header DECODES and is still not an object, which is the
/// interior-pointer face `addr_is_followable_object` was written for. Expected
/// to be ZERO.
pub static EVAC_REF_REJECTED_IMPLAUSIBLE: AtomicUsize = AtomicUsize::new(0);

/// The value of [`EVAC_REF_REJECTED_TORN`].
pub fn evacuation_refs_rejected_torn() -> usize {
    EVAC_REF_REJECTED_TORN.load(Ordering::Relaxed)
}

/// How many times the evacuator copied a header claiming to be a LEGACY OBJECT
/// of class 0 with an implausible field count.
///
/// `ClassId(0)` is the class every primitive array carries (`newarray` passes
/// it verbatim) and the MIC/PIC empty-slot sentinel; the objects allocated
/// under it have zero to a handful of fields. Nothing in this VM allocates a
/// class-0 legacy object with thousands of slots, so such a header is not an
/// object -- and a legacy slot count is a SIXTEEN-byte stride, so the walk it
/// authorises is eight times the extent of the reference array whose torn
/// header reads this way.
///
/// The report is the producer question this counter exists to answer: it names
/// the SOURCE region, offset and reuse epoch, and -- under
/// `CRATONVM_G1_DBG_REACH=1` -- the allocation site that carved that span, which
/// is what separates "a TLAB handed this out" from "the out-of-line allocator
/// did". Expected to be ZERO.
pub static EVAC_IMPLAUSIBLE_CLASS0_COPY: AtomicUsize = AtomicUsize::new(0);

/// The value of [`EVAC_IMPLAUSIBLE_CLASS0_COPY`].
pub fn evacuation_implausible_class0_copies() -> usize {
    EVAC_IMPLAUSIBLE_CLASS0_COPY.load(Ordering::Relaxed)
}

/// How many times an evacuation copy's DESTINATION carried a different class,
/// shape or kind than its source.
///
/// The memcpy and the two quartet updates (`set_gc_age`, `add_gc_flags`) are
/// the only writes `evacuate_object` performs, and none of them touches the
/// class id, the shape word or the kind. A non-zero value therefore means the
/// destination span was not exclusively this object's. Expected to be ZERO.
pub static EVAC_COPY_SHAPE_DRIFT: AtomicUsize = AtomicUsize::new(0);

/// The value of [`EVAC_COPY_SHAPE_DRIFT`].
pub fn evacuation_copy_shape_drifts() -> usize {
    EVAC_COPY_SHAPE_DRIFT.load(Ordering::Relaxed)
}

/// How many CSet roots were left unevacuated because they are not the start of
/// a live object.
///
/// Replaces [`NON_OBJECT_ROOT_COPIED`]'s subject: the collector used to copy
/// such a root and rewrite the root slot to the copy. Non-zero means a
/// conservatively-scanned stack word named an interior or dead address, which
/// is the condition conservative scanning exists to tolerate -- and which,
/// until 2026-09-02, this collector turned into a fabricated object.
pub static NON_OBJECT_ROOT_SKIPPED: AtomicUsize = AtomicUsize::new(0);

/// The value of [`NON_OBJECT_ROOT_SKIPPED`].
pub fn non_object_roots_skipped() -> usize {
    NON_OBJECT_ROOT_SKIPPED.load(Ordering::Relaxed)
}

/// How many objects the evacuation ref-scan refused to WALK because their own
/// header did not look like a live object. Expected to be ZERO.
pub static EVAC_HOLDER_REJECTED: AtomicUsize = AtomicUsize::new(0);

/// How many times the RSet source-region LINEAR walk found bytes that are not
/// an object header where the next object should start, i.e. how many times a
/// source region's object grid did not describe its contents.
///
/// Expected to be ZERO. Non-zero means the region was recycled or rewritten
/// between the remembered-set edge being recorded and this pause walking it —
/// which, before the check, became a wild read off the end of the region.
pub static EVAC_SOURCE_WALK_DESYNC: AtomicUsize = AtomicUsize::new(0);

/// How many holders' element walks were clamped to their region's extent —
/// i.e. how many headers claimed more reference slots than could physically be
/// there. Expected to be ZERO.
///
/// LEGACY holders only (gci i9/ng): a compact holder's cap is computed with the
/// legacy 16-byte stride over a field count and its walk ignores the cap, so it
/// is not counted. Before i9 it was, and the netty rows read 30 000-77 000.
pub static EVAC_HOLDER_CLAMPED: AtomicUsize = AtomicUsize::new(0);

/// How many candidates the PARALLEL evacuator's last-ditch alignment guard
/// refused, i.e. how many times `SharedEvac::evacuate` was handed an address
/// that cannot be an object start.
///
/// Expected to be ZERO now that both worker walks screen their candidates the
/// way the serial arm does. It is not redundant with that screen: `evacuate`
/// also has three DRIVER-side callers (the Phase-1 root seed, the Phase-1b
/// keep-alive seed, and `drain_deferred_self_forwarded`), and an unaligned
/// address reaching the evacuation-failure arm is a `make_forwarded` ASSERT —
/// which is an abort of the whole VM from a `g1-evac-N` worker, the loudest and
/// least recoverable way for this to be reported. See
/// `g1-evac-forwarding-assert-and-three-sigsegv-clusters`.
pub static PARALLEL_EVAC_UNALIGNED_CANDIDATE: AtomicUsize = AtomicUsize::new(0);

/// How many times the legacy 16-byte-slot walk was handed — and REFUSED — a
/// header whose kind is `Array`.
///
/// The flat walk has no array branch: it strides `SLOT_SIZE` (16) over what an
/// array stores as 8-byte (or 2-byte, or 1-byte) elements, and decodes each as
/// a `Value`. It also takes its element count from `header.num_slots()`, and
/// `NUM_SLOTS_OFFSET == ARRAY_LENGTH_OFFSET` — the same `shape` dword — so an
/// array handed to this walk yields not a garbage count but the array's own
/// LENGTH, a plausible positive integer of unbounded size.
///
/// That is not a hypothetical pairing: the corrupt-`Value`-cell producer closed
/// on 2026-08-22 was exactly this kind confusion, identified as
/// `receiver_class=java/lang/String receiver_kind=Array`
/// (`corrupt-value-cell-producer-was-a-string-array-FIXED-20260822.md`).
///
/// # Why refusing is safe, and why the census-only form was wrong to hedge
///
/// The first version of this counter (2026-08-24) counted without refusing, on
/// the stated grounds that "several of this helper's fifteen callers do not
/// visibly pre-branch on kind" and that skipping could drop marking work. That
/// premise was never checked. It is false: **all fifteen call sites pre-branch
/// on `header.kind() == ObjectKind::Array`** and run their own 8-byte element
/// walk in that arm. In file order: `process_object`, `seed_source_region`,
/// `record_outgoing_rset_edges` (the capped one), `scan_and_evacuate_refs`,
/// `scan_source_region_for_cset_refs`, `collect_outgoing_cross_region_edges`,
/// `rset_completeness_counts`, `verify_no_dangling_into_cset_within`,
/// `dbg_verify_no_unrewritten_forward`, `dbg_scan_for_zeroed_refs`,
/// `dbg_verify_reachable_integrity`, `debug_assert_no_reference_into_spans`,
/// `update_object_refs`, and `assert_rset_covers_every_cross_region_edge`.
///
/// So no caller reaches the flat walk for an array on purpose, and refusing
/// drops no marking work any caller was relying on. An `Array` arriving here
/// means the kind the caller read and the kind this header now reports disagree
/// — a torn or concurrently-rewritten header — and in that case the flat walk is
/// wrong whatever it does; not doing it is the cheap correct answer. One tag
/// comparison, on a tag already loaded.
///
/// This is option **D** of
/// `what-should-a-walker-do-with-an-unvalidated-header-count-FIXED-20260826.md`
/// §4, paired with option C (the `_trusting_header` rename). **Expected to be
/// ZERO.** Observed zero on 2026-08-26 across the regression suite run once per
/// collector (72/72 on each of ZGC, G1 and Generational) and across a G1 arm
/// measured with the walk proved engaged — 66 young evacuation pauses,
/// 2,202,360 objects copied, every one of them enumerated through this helper
/// or its callers' array arm.
pub static FLAT_WALK_REFUSED_ARRAY: AtomicUsize = AtomicUsize::new(0);

/// The value of [`FLAT_WALK_REFUSED_ARRAY`].
pub fn flat_walks_refused_for_array() -> usize {
    FLAT_WALK_REFUSED_ARRAY.load(Ordering::Relaxed)
}

/// How many unresolved-kept SEEDS the post-evacuation-failure rset recording
/// refused to walk because their header did not look like a live object.
///
/// Expected to be ZERO. Non-zero means `retry_after_evacuation_failure` handed
/// [`G1Collector::record_outgoing_rset_edges`] a self-forwarded address that is
/// region-resident but is not an object start — which, before the guard, was a
/// SIGSEGV inside that walk (see the internal record
/// `g1-sigsegv-shared-fault-site-20260811-FIXED.md`).
pub static KEPT_SEED_REJECTED: AtomicUsize = AtomicUsize::new(0);

/// The value of [`KEPT_SEED_REJECTED`].
pub fn kept_seeds_rejected() -> usize {
    KEPT_SEED_REJECTED.load(Ordering::Relaxed)
}

/// How many CSet-resident ROOTS the evacuator was handed that are not the start
/// of a live object.
///
/// The root array is deliberately over-approximate: `collect_roots`' operand-
/// stack filter is `is_heap_addr` (a RANGE check), widened from the strict
/// `is_object_address` on 2026-08-04 because the strict probe dropped genuine
/// young / mid-initialisation roots. A `long` on the operand stack, or a
/// computed interior address, whose bits land in the heap range is therefore a
/// root. That is safe for a non-moving sweep, where a false positive only
/// over-retains — the moving collectors have to screen it themselves.
pub static NON_OBJECT_ROOT_SEEN: AtomicUsize = AtomicUsize::new(0);

/// How many pauses have run with a live compiled frame and an EMPTY
/// conservative JIT root publication — the state that forces
/// `G1Collector::collect_garbage` to decline evacuation.
///
/// Drives the throttled warning only; the reported metric is
/// `gc_metrics::g1_empty_jit_publication_count`, which this deliberately does
/// not duplicate as a source of truth. Separate from `NON_OBJECT_ROOT_SEEN`
/// because the two answer different questions: that one counts roots whose
/// BYTES are not an object, this one counts pauses with no roots at all.
pub static EMPTY_JIT_PUBLICATION_SEEN: AtomicUsize = AtomicUsize::new(0);

/// How many of [`NON_OBJECT_ROOT_SEEN`] were nevertheless COPIED to a new
/// address by `evacuate_object` — i.e. how many times the evacuator computed a
/// size from garbage, memcpy'd that many bytes, installed a forwarding entry
/// for the address, and rewrote the root to point at the copy.
///
/// STRUCTURALLY ZERO since 2026-09-02: the root loops now `continue` on a
/// non-object root instead of evacuating it, so nothing can increment this.
/// It is kept, and now printed, paired with [`NON_OBJECT_ROOT_SKIPPED`],
/// because the pair is the readable form of the change - "seen, and NOT
/// copied".
///
/// **It is not a regression guard, and the sentence claiming it was one has
/// been removed.** Nothing increments this any more, so an edit that
/// reintroduced the copy would leave it at zero exactly as it is now: a
/// counter no writer bumps cannot report the thing it names. Whoever
/// reintroduces a copy on a non-object root is the one who has to bump it
/// here, at the copy.
///
/// LANE W4-B - the doc used to say "still printed" and that was not true
/// either: `collector_decision_report` printed `non_object_roots_skipped`
/// alone, and [`non_object_root_counts`] - the accessor carrying SEEN and
/// COPIED - had no caller anywhere in the workspace. Both halves are on the
/// `[GC] g1 non_object_roots_skipped=` line now, `COPIED` in capitals because
/// a non-zero is an object-identity defect rather than an engagement number.
/// So: readable now, but read it as a report, not as a guard.
pub static NON_OBJECT_ROOT_COPIED: AtomicUsize = AtomicUsize::new(0);

/// The values of [`NON_OBJECT_ROOT_SEEN`] and [`NON_OBJECT_ROOT_COPIED`].
pub fn non_object_root_counts() -> (usize, usize) {
    (
        NON_OBJECT_ROOT_SEEN.load(Ordering::Relaxed),
        NON_OBJECT_ROOT_COPIED.load(Ordering::Relaxed),
    )
}

/// Which step of [`G1Collector::classify_candidate_header`] decided an address
/// is not the start of a live object.
///
/// The distinctions matter to whoever reads the rejection warnings:
/// `AboveCursor` says the word is inside a live region but past everything ever
/// allocated in it (a stale or recycled address), while `BadKindTag` says it is
/// *below* the cursor and still not an object (a walk that lost the object
/// grid). They have different upstream producers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HeaderVerdict {
    /// Passed every check: this is the start of a live object.
    Object,
    /// Null, or not 8-aligned.
    NullOrUnaligned,
    /// Not inside `[arena_base, arena_end)`.
    OutsideArena,
    /// `region_size == 0` — the collector has no region geometry.
    NoRegionGeometry,
    /// The computed region index is past the end of the region table.
    NoSuchRegion,
    /// The owning region is `Free`.
    RegionFree,
    /// Below the owning region's data base (region table / arena disagreement).
    BelowRegionBase,
    /// Inside the region's span but at or above its allocation cursor.
    AboveCursor,
    /// The `kind` tag byte does not decode to an `ObjectKind`.
    BadKindTag,
    /// The element-type tag byte does not decode to an `ArrayElementType`.
    BadElementTag,
    /// A humongous filler, which is not walkable.
    HumongousFiller,
    /// Tags decode, but the shape word claims an impossible slot/element count.
    ImplausibleShape,
}

/// The values of [`EVAC_HOLDER_REJECTED`] and [`EVAC_HOLDER_CLAMPED`].
pub fn evacuation_holder_counts() -> (usize, usize) {
    (
        EVAC_HOLDER_REJECTED.load(Ordering::Relaxed),
        EVAC_HOLDER_CLAMPED.load(Ordering::Relaxed),
    )
}

/// The value of [`EVAC_SOURCE_WALK_DESYNC`].
pub fn evacuation_source_walk_desyncs() -> usize {
    EVAC_SOURCE_WALK_DESYNC.load(Ordering::Relaxed)
}

// ===========================================================================
// Step 9 — parallel STW evacuation (gated behind `CRATONVM_G1_PARALLEL_EVAC`)
// ===========================================================================
//
// The serial evacuator threads `&mut Vec<G1Region>`, a `HashMap` forwarding
// map, and a `Vec<*mut u8>` work list through one call chain under one big
// `regions.lock()`. The parallel evacuator implements the four-piece protocol
// from the design's §3.4:
//
//   1. Atomic forwarding — the dedup/race winner is decided by a CAS on the
//      from-space object's own `ObjectHeader::forwarding_ptr` field (treated as
//      an `AtomicUsize`), not a shared map lookup. G1's serial young/mixed path
//      never uses the from-space header's `forwarding_ptr` (it uses the
//      `pointer_map`), and every live object starts a collection with
//      `forwarding_ptr == null`, so the field is free to repurpose as the
//      per-object install slot. Only worker-vs-worker races exist (mutators are
//      parked at the STW safepoint; CSet objects are not mutated).
//   2. Per-worker forward shards — each worker records its winning
//      `(old, new)` pairs into a thread-local `Vec`; they are merged into the
//      `GcResult.pointer_map` after the closure (consumed by the VM's
//      `update_all_roots`, by Phase-4 region remap, monitors and the mark
//      worklist — exactly as the serial map).
//   3. Per-worker GC-TLAB allocation — replaces the single big lock. Each
//      worker claims whole Free regions from a shared pool via a lock-free
//      `fetch_add` index, then bump-allocates within its claimed region with a
//      thread-local cursor (single owner ⇒ no atomics needed inside a region);
//      the final cursor is written back when the region is retired.
//   4. Shared work queue — a `Mutex<Vec<usize>>` of gray (already-evacuated,
//      not-yet-scanned) to-space addresses with per-worker local batches, and
//      an `outstanding` counter for termination (children are added to the
//      counter before the parent is subtracted, so it never transiently hits 0
//      while work remains). This is the design's explicit "first cut" in place
//      of work-stealing deques.
//
// SAFETY MODEL. The `regions.lock()` guard is held by the driver for the whole
// collection (so the concurrent marker — which takes the same lock per step —
// stays excluded). The driver derives the regions' raw base (`as_mut_ptr()`)
// once and does NOT deref the guard again until after the parallel scope joins;
// all region access in between goes through that raw base under a strict
// disjointness discipline (the `split_at_mut`-style pattern):
//   * CSet (from-space) regions are only ever READ (copy source) plus an atomic
//     CAS on each object's `forwarding_ptr` — never `&mut`-aliased.
//   * To-space regions are each claimed by exactly one worker (unique pool
//     index) and `&mut`-accessed only by that owner.
//   * `region_for_ptr` resolves via the immutable, never-mutated
//     `region_lookup` table, not the regions Vec.
// `EvacPool::scope`'s completion barrier is the synchronisation point after
// which the driver resumes normal guard access for Phases 4/5. (It was
// `std::thread::scope`'s join until the helpers became persistent; the
// boundary is the same one, paid for by a barrier instead of a join — see the
// note at the `pool.scope` call in `parallel_evacuate`.)

/// Raw base pointer of the regions `Vec`, shared across evacuation workers.
///
/// `Send`/`Sync` is sound under the disjointness discipline documented above:
/// workers only `&mut`-access regions they exclusively claimed and only read /
/// atomically-CAS shared (CSet) regions.
#[derive(Clone, Copy)]
pub struct RegionsBase(pub *mut G1Region);
// SAFETY: see the module-level SAFETY MODEL note — disjoint per-worker access.
unsafe impl Send for RegionsBase {}
unsafe impl Sync for RegionsBase {}

/// Whether a followability test may pay for an O(region) grid walk to settle
/// the EMPTY-HEADER shape.
///
/// # Why this is a parameter and not a policy
///
/// `empty_header_is_a_real_object` is exact and it is the only thing that can
/// separate a real zero-field object from the payload word of a null `Value`
/// cell. It is also a linear region walk, and the two kinds of caller are three
/// orders of magnitude apart:
///
///  * the ROOT routes (`note_root_object_plausibility`, the root-pin scan) and
///    the three evacuation SUPPLY routes ask once per root / keep-alive entry /
///    seed -- tens to hundreds per pause, and **every empty-header refusal
///    measured on 2026-09-08 came from `root-pin-scan`**;
///  * the two evacuator ENTRY screens ask once per object evacuated. Measured
///    on the same class: 21 000 - 43 000 empty-header candidates in one run,
///    against 27 - 46 refusals. Proving those is a per-copy region walk for a
///    population that is overwhelmingly genuine.
///
/// So the entry screens keep the cheap half of the test -- tag bytes, cursor
/// containment, the class-id band and the arena test -- and leave the grid to
/// the routes that supply them. That is not a coverage hole: an address only
/// reaches `evacuate` through a root, a supply route or a reference slot, and
/// the first two now prove it.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum GridProof {
    /// Walk the region's object grid for an empty-header candidate.
    Yes,
    /// Do not; this caller is on the per-object path.
    No,
}

/// Which end of a holder's region bounds a reference walk.
///
/// See [`G1Collector::holder_walkable_slots_view`] for why the two evacuators
/// need different answers: the serial arm publishes a destination cursor as it
/// copies, the parallel arm carves TLABs and only writes the cursor back when a
/// worker retires one.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum HolderBound {
    /// The region's published allocation cursor — exact, and what every
    /// serial-arm caller has always used.
    Cursor,
    /// The region's committed extent. Weaker than `Cursor`, but true for a
    /// to-space holder still sitting inside an unretired worker TLAB.
    RegionEnd,
}

/// Every to-space copy this pause made, as it looked the instant it was made.
///
/// `CRATONVM_G1_EVAC_COPY_WATCH=1` only. The question it exists to answer is
/// the one `g1-eight-byte-write-at-a-live-objects-base-20260906` could not:
/// the corrupt holders that walk reports find are TO-SPACE COPIES whose
/// `class_id`/`shape` dword pair — the object's first eight bytes — has become
/// a pointer into this arena, with the mark word at +8 intact. Whether they
/// were sound when `evacuate` copied them was an INFERENCE (candidates screen
/// clean, holders do not). This makes it a timestamp.
///
/// Recording is a push per copy under one lock, which is why it is opt-in: the
/// point is to be able to run it, not to run it always.
#[derive(Default)]
pub struct CopyWatch {
    /// `(addr, src, class_id, shape, region_idx, reuse_epoch)` at the moment
    /// of the copy. The SOURCE is recorded so a mismatch names the from-space
    /// object it came from; the REGION and its reuse epoch are recorded so the
    /// cross-pause checkpoint can drop an entry whose region has since been
    /// freed and handed out again — that is a legitimate change, not a write.
    pub entries: Mutex<Vec<(usize, usize, u32, u32, usize, u64)>>,
    /// How many mismatches each checkpoint found, so a later checkpoint does
    /// not re-report what an earlier one already did.
    pub reported: AtomicUsize,
}

/// The one [`CopyWatch`] a run has, hoisted to module scope so the corrupt-cell
/// drain can ASK IT a question the checkpoints cannot answer: was this holder
/// a copy THIS pause made at all?
///
/// The checkpoints prove a copy's first word does not change after the copy.
/// They are blind to two things, and those two are now the whole search: a
/// SOURCE that was already corrupt when it was copied (the copy faithfully
/// reproduces it and nothing 'changes'), and an object never copied at all
/// (never in the ledger, so no checkpoint ever looks at it).
pub static COPY_WATCH: std::sync::OnceLock<CopyWatch> = std::sync::OnceLock::new();

/// One reference-slot write the collector made, as [`RefWriteWatch`] records it.
#[derive(Clone, Copy)]
pub struct RefWrite {
    /// The bytes stored. For `width = 8` this is the raw pointer; for
    /// `width = 16` it is the `Value` cell's DISCRIMINANT word, which is what a
    /// later corrupt-cell report reads back and rejects.
    pub value: u64,
    /// The object the walk was walking when it made this write.
    pub holder: usize,
    /// `holder + object_total_size(holder_header)` at write time -- the extent
    /// the writing walk believed the holder had.
    pub holder_end: usize,
    /// 8 for the compact/array arm's raw pointer store, 16 for a legacy
    /// `Value` cell. THE WHOLE POINT: the measured corruption is an eight-byte
    /// quantity where a sixteen-byte cell's first word belongs, so which of the
    /// two wrote a given slot is the question.
    pub width: u8,
    /// `#[track_caller]` location of the write site.
    pub site: &'static std::panic::Location<'static>,
}

/// `CRATONVM_G1_REF_WRITE_WATCH=1`: every reference-slot write the collector
/// makes during a pause, keyed by SLOT ADDRESS.
///
/// # Why the read side could not answer this
///
/// `g1-eight-byte-write-at-a-live-objects-base-20260906` measured 58 corrupt
/// cells whose first word is a tagged heap pointer and whose second word is
/// zero, and named two producers it could not distinguish:
///
///  1. an 8-byte reference write into a 16-byte legacy cell -- a walk striding
///     8 bytes over its neighbours stamps word0 of each cell and leaves word1;
///  2. a READ misaligned by 8 into a forwarded object's mark word, where
///     `target | MARK_FORWARDED` is followed by a zero body word.
///
/// They differ in exactly one observable: whether a collector write actually
/// landed on that slot. This records the write side so the question is a
/// lookup rather than an inference. [`CopyWatch`] cannot answer it -- it
/// records COPIES, and a write into a resident object's body is not one, which
/// is why it read clean at every checkpoint while holders kept appearing.
///
/// A `HashMap` rather than [`CopyWatch`]'s `Vec`: the interesting query is
/// "who wrote THIS slot", asked once per corrupt cell against a ledger with
/// hundreds of thousands of entries, and a linear scan of that is minutes.
/// Last writer wins, which is the one a corrupt read is complaining about.
pub struct RefWriteWatch {
    pub writes: Mutex<std::collections::HashMap<usize, RefWrite>>,
    /// Writes whose slot lay outside the holder's own declared body. This is
    /// the CIRCULAR half of the test (the extent comes from the same header the
    /// walk took its count from) and so it is expected to be zero even when the
    /// defect is present; it is here because a non-zero count would name a
    /// walk whose bound and whose stride disagree, which is free to check.
    pub out_of_body: AtomicUsize,
}

/// The one [`RefWriteWatch`] a run has. `None` unless
/// `CRATONVM_G1_REF_WRITE_WATCH=1`.
pub static REF_WRITE_WATCH: std::sync::OnceLock<RefWriteWatch> = std::sync::OnceLock::new();

/// How many reference-slot writes carried a value whose low three bits are not
/// clear -- i.e. NOT an object address.
///
/// Expected to be ZERO, and it is the counter to read first. See
/// [`note_ref_write`]'s tripwire.
pub static REF_WRITE_TAGGED_VALUE: AtomicUsize = AtomicUsize::new(0);

/// Record one reference-slot write, and TRIPWIRE any write whose value is not
/// an object address.
///
/// # The tripwire runs whether or not the watch is armed
///
/// It is a mask and a branch on a path that is already storing to memory, and
/// it is the check that names the defect
/// `g1-eight-byte-write-at-a-live-objects-base-20260906` measured from the read
/// side only. Measured 2026-09-08 on
/// `TestHostConfigAutomaticDeploymentXmlExternalWarXml` (Linux, `-Xmx1g`,
/// `CRATONVM_G1_WORKERS=16`, `RESUME_DEST=0`): live class-6 objects carrying a
/// reference slot whose value is `0x78695c60002b` -- an address with
/// `MARK_FORWARDED` still on it, which is a mark WORD where an object ADDRESS
/// belongs. Every consumer of such a slot is wrong in a different way: the
/// evacuation candidate screen refuses it (`verdict=NullOrUnaligned`) and so
/// leaves a live referent unevacuated, the interpreter degrades it to `null`,
/// and the JIT dereferences it.
///
/// The read side cannot say who produced it, because the value is COPIED along
/// with its holder by every subsequent evacuation -- the three reports in that
/// run are one corrupted object and two later copies of it. Only a check at
/// the write catches the first one.
///
/// The ledger below is separate and opt-in
/// (`CRATONVM_G1_REF_WRITE_WATCH=1`): the arming is a single `OnceLock::get`,
/// so an unarmed run pays one predictable load per rewritten slot.
///
/// `#[track_caller]` all the way down: the caller of interest is the WALK, not
/// this function and not [`write_flat_object_reference`], so every wrapper
/// between the two carries the attribute.
#[track_caller]
#[inline]
pub fn note_ref_write(slot: usize, value: u64, holder: usize, holder_end: usize, width: u8) {
    // THE TRIPWIRE. An object address is 8-aligned by construction
    // (`plausible_heap_pointer` is asserted in `make_forwarded` and
    // `make_inflated`, and every allocator aligns to 8), so a non-zero value
    // with any low bit set is not one. `| MARK_FORWARDED` (0b11) is the shape
    // measured; `| MARK_INFLATED` (0b10) would be a monitor pointer and is the
    // same class of mistake.
    if value & 0b111 != 0 {
        let n = REF_WRITE_TAGGED_VALUE.fetch_add(1, Ordering::Relaxed) + 1;
        if n <= 16 || n.is_power_of_two() {
            tracing::warn!(
                "[g1] a REFERENCE-SLOT WRITE carried a TAGGED value (#{n}): \
                 slot=0x{slot:x} value=0x{value:016x} low3={} untagged=0x{:x} \
                 holder=0x{holder:x} holder_end=0x{holder_end:x} width={width} site={} -- \
                 an object address is 8-aligned, so this is a MARK WORD (or an \
                 interior/tagged word) being stored where an object reference \
                 belongs. Whoever reads this slot next reads it as a reference.",
                value & 0b111,
                value & !0b111,
                std::panic::Location::caller(),
            );
        }
    }
    let Some(w) = REF_WRITE_WATCH.get() else {
        return;
    };
    let rec = RefWrite {
        value,
        holder,
        holder_end,
        width,
        site: std::panic::Location::caller(),
    };
    // The holder's OWN header length: a compact instance's fields start at
    // `COMPACT_HEADER_SIZE` (8) since the 8-byte header, so testing every
    // holder against `HEADER_SIZE` (16) reported each write to a compact
    // object's first field as out-of-body. Read only on this armed path.
    let holder_header = if holder != 0 {
        // SAFETY: `holder` is the object whose slots the caller is rewriting,
        // so its header word is live, mapped memory.
        unsafe { &*(holder as *const ObjectHeader) }.header_size()
    } else {
        HEADER_SIZE
    };
    if holder != 0
        && holder_end > holder + holder_header
        && (slot < holder + holder_header || slot + width as usize > holder_end)
    {
        let n = w.out_of_body.fetch_add(1, Ordering::Relaxed) + 1;
        if n <= 8 || n.is_power_of_two() {
            tracing::warn!(
                "[g1][ref-write-watch] a reference write landed OUTSIDE its own holder's \
                 body (#{n}): slot=0x{slot:x} width={width} value=0x{value:016x} \
                 holder=0x{holder:x} holder_end=0x{holder_end:x} site={} -- the walk's \
                 bound and its stride disagree, so this write reached the object that \
                 follows the holder.",
                rec.site,
            );
        }
    }
    w.writes.lock().insert(slot, rec);
}

/// What wrote `slot` this pause, if anything, rendered for a corrupt-cell
/// report. Also asks about `slot + 8` and `slot - 8`: an eight-byte write into
/// a sixteen-byte cell lands on ONE of the cell's two words, and which one it
/// is decides whether the reader saw a stamped discriminant or a stamped
/// payload.
pub fn ref_write_provenance(slot: usize) -> String {
    let Some(w) = REF_WRITE_WATCH.get() else {
        return "ref_write_watch=off".to_string();
    };
    let map = w.writes.lock();
    let one = |a: usize| -> String {
        match map.get(&a) {
            Some(r) => format!(
                "0x{a:x}<-width={} value=0x{:016x} holder=0x{:x} holder_end=0x{:x} site={}",
                r.width, r.value, r.holder, r.holder_end, r.site
            ),
            None => format!("0x{a:x}<-none"),
        }
    };
    format!(
        "ref_write_watch=[{} | {} | {}]",
        one(slot.wrapping_sub(8)),
        one(slot),
        one(slot.wrapping_add(8)),
    )
}

/// The collector's arena bounds, published for the free functions that have no
/// `&self` and must nonetheless ask the ONE question this family turns on.
///
/// `word0_plausible_ptr` -- the field
/// `g1-eight-byte-write-at-a-live-objects-base-20260906` rests its "19 of 19"
/// on -- is `cratonvm_types::plausible_heap_pointer`, which asks only
/// non-null / 8-aligned / under 2^47. It answers TRUE for
/// `0x0000001200000040`, an ordinary `class_id=64 num_slots=18` header pair
/// sitting 2157 GiB below any arena. The screens that REFUSE a holder ask a
/// different and much narrower question -- is the recombined pair inside
/// `[arena_base, arena_end)` -- and the two disagree on exactly that input.
/// Publishing the bounds is what lets the free-function report ask the
/// refusing screens' question instead of the loose one.
pub static G1_ARENA_BASE: AtomicUsize = AtomicUsize::new(0);
pub static G1_ARENA_END: AtomicUsize = AtomicUsize::new(0);

/// Is `word` a pointer into the collector's arena, by the SAME test
/// `G1Collector::holder_word0_arena_pointer` applies? `false` when no
/// collector has published bounds yet.
pub fn word_is_arena_pointer(word: u64) -> bool {
    let (base, end) = (
        G1_ARENA_BASE.load(Ordering::Relaxed),
        G1_ARENA_END.load(Ordering::Relaxed),
    );
    end > base && (word as usize) >= base && (word as usize) < end
}

/// r11w18 g1store — the eight bytes at an object's base, read as ONE word:
/// the `class_id` dword with the MARK-WORD dword above it.
///
/// Every arena-pointer screen in this file ("was a heap pointer written over
/// this header?") asks its question of this word. They used to recombine
/// `class_id` with `num_slots()` instead, which WAS the header's first word
/// while `ObjectHeader` was `class_id`(4) + `shape`(4) + `mark_word`(8). The
/// 8-byte-header merge (2026-09-24) moved the mark word to +4 and the shape to
/// +8, and the screens were not updated. Two defects:
///
/// * the screens stopped seeing what they were built for: an 8-byte pointer
///   written at an object's base now lands in `class_id` + MARK, and the old
///   pairing combined its low half with an unrelated shape word;
/// * they REFUSE REAL OBJECTS -- and did before the merge too, because "a real
///   header is a small class id beside a small slot count" forgot that the
///   arena's own high dword is a small number. A real array's
///   `class_id | length << 32` is inside `[arena_base, arena_end)` whenever its
///   length equals the arena
///   base's high dword (and its class id is at or above the base's low dword)
///   or that high dword plus one (and its class id is below it). On Windows
///   the high dword is a few hundred (`0x1ba` = 442 in the orchestrator's w17
///   hang log, whose "Phase-4 fixup REFUSED a HOLDER ... class_id=4044482304
///   num_slots=442" is exactly the TLAB filler `int[442]` of
///   [`cratonvm_gc_core::tlab::TLAB_FILLER_CLASS_ID`] ending a region). Refused as a
///   CANDIDATE, such an array is not evacuated and its slot is left naming
///   from-space that the pause then frees; refused as a HOLDER, its own slots
///   are neither evacuated nor fixed up.
///
/// Every published header carries [`cratonvm_types::GC_FLAG_HEADER`] in its
/// flags byte (object byte 7, word bit 59), so a real header's first word is at
/// least 2^59 and can never fall inside an arena below 2^47. A heap pointer
/// written over the base (or an interior word that holds one, such as a
/// reference field of a compact instance) can.
#[inline]
pub fn header_first_word(header: &ObjectHeader) -> u64 {
    (header.class_id.as_u32() as u64) | ((header.mark() as u64) << 32)
}

// The argument in `header_first_word`'s doc, checked: the header flag sits in
// the object's byte 7, i.e. at or above bit 56 of the first word, far above any
// user-space address.
const _: () = assert!(
    ((cratonvm_types::GC_FLAG_HEADER as u64) << (8 * cratonvm_types::GC_FLAGS_BYTE_OFFSET))
        >= (1u64 << 48)
);

/// gcd d1/e — `class_id = 0`, no slots, an `Object`, not compact: the
/// EMPTY-HEADER shape `addr_is_followable_object_view` sends to the grid proof.
///
/// The mark FIRST. `num_slots()` resolves a FORWARDED mark through the
/// header's second word, and the bytes screened here are not known to be an
/// object: a class-id-0 word followed by a forwarded-looking mark (state
/// `0b11`, SELF clear) sent the old order's `num_slots()` to whatever that
/// word named, and a BUSY-looking one into the forwarding wait (the pin9 / pin10
/// shapes, `docs/feature-designs/concurrent-gc-maturation.md`).
/// A forwarded header answers `false`, which changes nothing for a real
/// object: the proof that follows only REFUSES a candidate the object grid
/// places inside another object, and a genuinely forwarded object sits at an
/// object start. Its forward is screened where it is decoded
/// (`forwarding_target`'s refusal). Every other answer is the old conjunction's.
#[inline]
pub fn header_has_empty_shape(header: &ObjectHeader) -> bool {
    !ObjectHeader::is_forwarded_mark(header.mark_word.load(Ordering::Relaxed))
        && header.class_id.as_u32() == 0
        && header.num_slots() == 0
        && header.kind() == ObjectKind::Object
        && !cratonvm_types::is_compact_object(header)
}

/// gcd d1/e — `dbg_scan_for_zeroed_refs`' "this target reads as zeroed
/// memory": class id, slots, kind tag and array length all zero. A non-zero
/// mark is legal here (a hashed or locked header), so the screen first proves
/// it is not FORWARDED, for the reason [`header_has_empty_shape`] gives; a
/// forwarded header is not zeroed memory either way.
#[inline]
pub fn header_reads_zeroed(header: &ObjectHeader) -> bool {
    let mark = header.mark_word.load(Ordering::Relaxed);
    !ObjectHeader::is_forwarded_mark(mark)
        && header.class_id.as_u32() == 0
        && ObjectHeader::kind_tag(mark) == 0
        && header.num_slots() == 0
        && header.array_length() == 0
}

/// r11w18 g1store — is this the `int[]` TLAB tail filler
/// ([`cratonvm_gc_core::tlab::TLAB_FILLER_CLASS_ID`]) rather than an object?
///
/// A filler is a well-formed `int[]` precisely so that walkers which do not
/// know it can stride past it, and every G1 walker does exactly that. But it
/// is dead space, not an object: nothing may be SEEDED at it. No real array
/// carries this class id (a primitive array's class id is 0).
#[inline]
pub fn is_tlab_filler_header(header: &ObjectHeader) -> bool {
    header.class_id.as_u32() == cratonvm_gc_core::tlab::TLAB_FILLER_CLASS_ID.as_u32()
        && header.kind() == ObjectKind::Array
}

impl CopyWatch {
    #[inline]
    #[allow(clippy::too_many_arguments)]
    pub fn note_copy(
        &self,
        addr: usize,
        src: usize,
        class_id: u32,
        shape: u32,
        region_idx: usize,
        reuse_epoch: u64,
    ) {
        self.entries
            .lock()
            .push((addr, src, class_id, shape, region_idx, reuse_epoch));
    }

    /// Drop every recorded entry. The cross-pause checkpoint verifies the
    /// PREVIOUS pause's copies and then starts again, so one pause's worth of
    /// entries is the most this ever holds.
    /// Was `addr` a to-space copy THIS pause made, and if so what did its
    /// header look like at the instant of the copy, and where did it come from?
    ///
    /// This is the split the checkpoints cannot make. A corrupt holder that IS
    /// in the ledger was copied from `src` with the recorded `class_id`/`shape`;
    /// if those match what the walk later read, the SOURCE was already corrupt
    /// and the search moves to from-space. A corrupt holder that is NOT in the
    /// ledger was never evacuated this pause, so no copy path touched it and the
    /// writer reached a resident object in place.
    pub fn lookup(&self, addr: usize) -> Option<(usize, u32, u32)> {
        self.entries
            .lock()
            .iter()
            .find(|e| e.0 == addr)
            .map(|e| (e.1, e.2, e.3))
    }

    pub fn clear(&self) {
        self.entries.lock().clear();
    }

    /// Re-read every recorded copy's first word and say which ones changed.
    ///
    /// `label` names the interval that just closed. Two checkpoints separate
    /// "a worker did it during the closure" from "the serial drain did it",
    /// which is the split that decides where to look next.
    pub fn verify(
        &self,
        label: &'static str,
        arena: std::ops::Range<usize>,
        regions: Option<RegionView<'_>>,
    ) {
        let entries = self.entries.lock();
        let mut changed = 0usize;
        let mut skipped = 0usize;
        let mut first: Vec<String> = Vec::new();
        for &(addr, src, cid, shape, ridx, epoch) in entries.iter() {
            // CROSS-PAUSE ONLY: a region that has been freed and handed out
            // again since the copy legitimately holds different bytes. Dropping
            // those is what keeps this checkpoint from reporting the collector
            // doing its job. `regions = None` is the in-pause case, where no
            // region can have been recycled yet.
            if let Some(view) = regions {
                match view.get(ridx) {
                    Some(r) if r.region_type() != RegionType::Free && r.reuse_epoch == epoch => {}
                    _ => {
                        skipped += 1;
                        continue;
                    }
                }
            }
            // SAFETY: `addr` is a to-space copy this pause made; the pause is
            // still open, so the region is still typed and mapped.
            let h = unsafe { &*(addr as *const ObjectHeader) };
            let (now_cid, now_shape) = (h.class_id.as_u32(), h.num_slots());
            if now_cid == cid && now_shape == shape {
                continue;
            }
            changed += 1;
            if first.len() < 8 {
                // r11w18 g1store: the header's real first word (class id +
                // mark), the word the refusing screens test.
                let paired = header_first_word(h);
                first.push(format!(
                    "obj=0x{addr:x} was(class_id={cid} shape={shape}) now(class_id={now_cid} shape={now_shape}) paired=0x{paired:016x} pair_is_arena_pointer={} mark=0x{:016x}",
                    arena.contains(&(paired as usize)),
                    h.mark_word.load(Ordering::Relaxed),
                ));
            }
        }
        let _ = skipped;
        if changed == 0 {
            // WARN, not INFO. At the default level an `info!` here is invisible,
            // and "0 changed" then reads exactly like "the checkpoint never
            // ran" — which is the one thing a reader must be able to tell
            // apart, because a clean result is the interesting one here.
            tracing::warn!(
                "[g1][copy-watch] {label}: {} copies, 0 changed since the copy ({skipped} skipped as legitimately recycled).",
                entries.len()
            );
            return;
        }
        self.reported.fetch_add(changed, Ordering::Relaxed);
        tracing::error!(
            "[g1][copy-watch] {label}: {changed} of {} to-space copies had their FIRST WORD rewritten after `evacuate` copied them. `class_id`(4)+`shape`(4) IS that word, so every one of these is an eight-byte write at a live object's base, inside this pause, in the interval this checkpoint closes. First few: {}",
            entries.len(),
            first.join(" | "),
        );
        let _ = skipped;
    }
}

/// Read-only access to the region table for the screens BOTH evacuators share.
///
/// The serial arm holds the regions guard and can hand out a `&[G1Region]`.
/// The parallel arm deliberately cannot: the driver drops its guard-derived
/// borrow for the whole dispatch (see the module SAFETY MODEL note) and workers
/// reach individual regions through [`RegionsBase`], so materialising a
/// whole-table slice there would assert an exclusivity the discipline does not
/// have — a `&[G1Region]` over the array aliases the `&mut G1Region` a worker
/// holds for the region it claimed.
///
/// This enum is what lets ONE body serve both arms. The alternative — a second
/// copy of `classify_candidate_header` / `holder_walkable_slots` for the
/// parallel arm — is the twin-pair shape this file's history already has one
/// defect from (G1-9, a divergence between the two ref walks), and the whole
/// reason the parallel arm needs these screens at all is that the serial arm's
/// 2026-09-02 hardening never crossed to it.
#[derive(Clone, Copy)]
pub enum RegionView<'a> {
    /// The serial arm: a slice borrowed from the held regions guard.
    Slice(&'a [G1Region]),
    /// The parallel arm: base pointer plus length, read ONE region at a time.
    Raw(RegionsBase, usize),
}

impl<'a> From<&'a [G1Region]> for RegionView<'a> {
    fn from(s: &'a [G1Region]) -> Self {
        RegionView::Slice(s)
    }
}

impl<'a> RegionView<'a> {
    #[inline]
    pub fn get(&self, idx: usize) -> Option<&'a G1Region> {
        match *self {
            RegionView::Slice(s) => s.get(idx),
            // SAFETY: `idx < len` bounds the offset into the live regions
            // table, which outlives the dispatch. This hands out a SHARED
            // reference to ONE region — exactly the access `seed_source_region`
            // already takes (`&*self.regions_base.0.add(source_idx)`) — and
            // every field read through it (`region_type`, `data`, `cursor()`)
            // is either immutable for the pause or an atomic.
            RegionView::Raw(base, len) => {
                if idx < len {
                    Some(unsafe { &*base.0.add(idx) })
                } else {
                    None
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Region-index sets (F-03)
// ---------------------------------------------------------------------------

/// Upper bound on a region index this set will store, and the tripwire that
/// keeps it a REGION set.
///
/// A region index and a heap ADDRESS are both `usize`, so nothing in the type
/// system distinguishes the two kinds of `usize` set this file keeps — and this
/// one is a dense bitset, where handing it an address would try to reserve
/// terabytes. The bound turns that mistake from an out-of-memory abort with no
/// attribution into an assertion naming the value. 2^26 is 67 million regions:
/// a 64 TiB heap at the 1 MiB minimum region size has 67 million regions, so no
/// configuration that fits in memory can reach the bound, while any real heap
/// address (at least 2^30 on every platform this runs on) is far above it.
pub const MAX_TRACKABLE_REGION_INDEX: usize = 1 << 26;

/// A set of region indices, as a bitset.
///
/// # Why not `HashSet<usize>`
///
/// This replaces `std::collections::HashSet<usize>` at every site that holds
/// region indices — the collection set above all. `contains` on those sets runs
/// once per non-null reference slot in Phase 2, Phase 3, Phase 4 and the
/// post-pause verifier, i.e. inside the two phases that are, since the
/// free-scrub deletion, most of a young pause.
///
/// `std::collections::HashSet` hashes with SipHash-1-3 under a per-process
/// random seed. That is the right default for a set keyed by untrusted input
/// and the wrong one for a dense integer index bounded by
/// `heap_size / region_size`: the key already IS the slot number. Note that the
/// replaced sites did not even use the `FxHashSet` this file imports and uses
/// elsewhere — the std default was inherited, not chosen.
///
/// A `contains` is now a shift, a bounds compare, a load and a test. The set
/// itself is `region_count / 8` bytes — 32 bytes for the 256-region default
/// heap, 4 KiB for a 32 GiB one — so it also stops being a per-pause heap
/// allocation of any consequence.
///
/// # Shape
///
/// Deliberately `HashSet`-shaped (`insert` / `contains(&idx)` / `len` / `iter`
/// / `extend` / `collect`) so the converted call sites read the same
/// afterwards. `iter` yields `usize` rather than `&usize` — a bitset has no
/// `usize` to borrow — which is the one place a call site had to change
/// (`.iter().copied()` becomes `.iter()`).
#[derive(Clone, Default, PartialEq, Eq)]
pub struct RegionSet {
    pub words: Vec<u64>,
    pub len: usize,
}

impl RegionSet {
    /// An empty set with no reserved capacity.
    #[inline]
    pub fn new() -> Self {
        Self::default()
    }

    /// An empty set sized for `regions` indices up front, so filling it with
    /// consecutive indices does not re-grow the backing vector.
    #[inline]
    pub fn with_region_capacity(regions: usize) -> Self {
        Self {
            words: vec![0; regions.div_ceil(64)],
            len: 0,
        }
    }

    /// Insert `idx`; returns `true` if it was not already present.
    #[inline]
    pub fn insert(&mut self, idx: usize) -> bool {
        assert!(
            idx < MAX_TRACKABLE_REGION_INDEX,
            "g1: {idx} (0x{idx:x}) is not a region index — a RegionSet is a dense bitset over \
             region numbers and this looks like a heap address. See MAX_TRACKABLE_REGION_INDEX."
        );
        let (w, bit) = (idx / 64, 1u64 << (idx % 64));
        if w >= self.words.len() {
            self.words.resize(w + 1, 0);
        }
        let already = self.words[w] & bit != 0;
        if !already {
            self.words[w] |= bit;
            self.len += 1;
        }
        !already
    }

    /// Is `idx` present? Takes `&usize` to mirror `HashSet::contains`, so a
    /// converted `filter(|(i, _)| set.contains(i))` — where `i` is already a
    /// reference — reads unchanged.
    #[inline]
    pub fn contains(&self, idx: &usize) -> bool {
        let idx = *idx;
        let w = idx / 64;
        w < self.words.len() && self.words[w] & (1u64 << (idx % 64)) != 0
    }

    #[inline]
    pub fn len(&self) -> usize {
        self.len
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Drop every index, keeping the allocated words.
    #[inline]
    pub fn clear(&mut self) {
        self.words.iter_mut().for_each(|w| *w = 0);
        self.len = 0;
    }

    /// Indices in ascending order. Yields `usize`, not `&usize`.
    pub fn iter(&self) -> impl Iterator<Item = usize> + '_ {
        self.words.iter().enumerate().flat_map(|(w, &word)| {
            let base = w * 64;
            BitIter { word }.map(move |b| base + b)
        })
    }
}

/// Set bits of one word, lowest first.
pub struct BitIter {
    pub word: u64,
}

impl Iterator for BitIter {
    type Item = usize;
    #[inline]
    fn next(&mut self) -> Option<usize> {
        if self.word == 0 {
            return None;
        }
        let b = self.word.trailing_zeros() as usize;
        self.word &= self.word - 1;
        Some(b)
    }
}

impl<'a> IntoIterator for &'a RegionSet {
    type Item = usize;
    type IntoIter = Box<dyn Iterator<Item = usize> + 'a>;
    fn into_iter(self) -> Self::IntoIter {
        Box::new(self.iter())
    }
}

impl IntoIterator for RegionSet {
    type Item = usize;
    type IntoIter = std::vec::IntoIter<usize>;
    fn into_iter(self) -> Self::IntoIter {
        self.iter().collect::<Vec<_>>().into_iter()
    }
}

impl Extend<usize> for RegionSet {
    fn extend<I: IntoIterator<Item = usize>>(&mut self, iter: I) {
        for idx in iter {
            self.insert(idx);
        }
    }
}

impl FromIterator<usize> for RegionSet {
    fn from_iter<I: IntoIterator<Item = usize>>(iter: I) -> Self {
        let mut set = Self::new();
        set.extend(iter);
        set
    }
}

impl std::fmt::Debug for RegionSet {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_set().entries(self.iter()).finish()
    }
}

/// A per-worker, per-destination-type thread-local allocation buffer.
pub struct Tlab {
    pub dest_type: RegionType,
    /// Index of the currently-owned to-space region, if any.
    pub region_idx: Option<usize>,
    /// Base address of the owned region.
    pub base: usize,
    /// Length (bytes) of the owned region.
    pub len: usize,
    /// Local bump cursor (offset from `base`).
    pub offset: usize,
    /// gci i14/yt (`CRATONVM_G1_SURVIVOR_PLAB`): this buffer is a PLAB chunk
    /// `[offset, len)` carved out of a SHARED Survivor region, not a whole
    /// region this worker owns. The region's cursor is the shared bump
    /// pointer, so `retire_tlab` must NOT store `offset` over it; it stamps a
    /// filler over the unused tail instead. See `SharedEvac::claim_survivor_plab`.
    pub shared: bool,
    /// JIT round 23 wave 10, lane L9 (`CRATONVM_G1_OLD_PLAB`, opt-in): this
    /// worker's Old TLAB may take chunks of the pause's shared Old region. Set
    /// once per pause by `parallel_evacuate` for the workers BEYOND the
    /// whole-region budget (`old_plab_worker_split`); the others keep a whole
    /// Old region of their own, as with the switch off. Survives
    /// `retire_tlab`.
    pub plab_ok: bool,
}

impl Tlab {
    pub fn new(dest_type: RegionType) -> Self {
        Self {
            dest_type,
            region_idx: None,
            base: 0,
            len: 0,
            offset: 0,
            shared: false,
            plab_ok: false,
        }
    }
}

/// How many times `evacuate`'s parallel path LOST the forwarding CAS to another
/// worker and adopted the winner's target.
///
/// This is the arm whose forward went unrecorded until 2026-08-26, and the
/// counter exists so the fix is not taken on faith: a repair to an arm nothing
/// reaches is a no-op dressed as a fix. Non-zero on a workload means the hole
/// was live on it; zero means the defect that workload shows is somewhere else.
///
/// Read it with [`evacuate_cas_loser_forwards`].
pub static EVACUATE_CAS_LOSER_FORWARDS: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

/// How many times the parallel evacuator met a peer's claim still `BUSY` and
/// waited it out (`SharedEvac::peer_forwarding_target`). Before 2026-09-30 a
/// wait longer than `FORWARDING_BUSY_WAIT_LIMIT` rounds gave up and left the
/// slot on the from-space copy; this counts the waits, so a run can say whether
/// the window was reached at all. Read it with [`parallel_evac_claim_waits`].
pub static PARALLEL_EVAC_CLAIM_WAITS: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

/// Snapshot of [`PARALLEL_EVAC_CLAIM_WAITS`].
pub fn parallel_evac_claim_waits() -> u64 {
    PARALLEL_EVAC_CLAIM_WAITS.load(std::sync::atomic::Ordering::Relaxed)
}

/// How long [`wait_out_peer_claim`] waits before it gives up. A genuine claim
/// publishes two stores after it is taken, so even a winner descheduled on a
/// badly oversubscribed host is back within milliseconds; this is a safety
/// valve for a word that passed the supply screen without being a claim, so
/// that it degrades to the old bounded answer instead of hanging the pause.
/// That is the failure `g1-forwarding-busy-wait-spin-livelock-FIXED-20260925`
/// recorded for the conservative ROOT screen, which is why that path keeps
/// `ObjectHeader::forwarding_address`'s short bound and never comes here.
pub const PEER_CLAIM_WAIT_LIMIT: std::time::Duration = std::time::Duration::from_secs(2);

/// Waits that hit [`PEER_CLAIM_WAIT_LIMIT`]. Nonzero means a BUSY word that
/// never published reached the evacuator, which no genuine claim can produce.
pub static PARALLEL_EVAC_CLAIM_WAITS_ABANDONED: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

/// Wait until `hdr`'s mark word is no longer a `FORWARDED | BUSY` claim and
/// return the mark it settled on — or, past [`PEER_CLAIM_WAIT_LIMIT`], the
/// still-BUSY mark (whose decode then answers "not forwarded", the pre-fix
/// behaviour). Only for a PROVED object start whose claim is a peer worker's
/// of this same pause; see `SharedEvac::peer_forwarding_target`.
pub fn wait_out_peer_claim(hdr: &ObjectHeader) -> u32 {
    let mut started: Option<std::time::Instant> = None;
    let mut rounds: u32 = 0;
    loop {
        let mark = hdr.mark_word.load(Ordering::Acquire);
        if !ObjectHeader::is_forwarded_mark(mark) || mark & cratonvm_types::MARK_FWD_BUSY == 0 {
            if started.is_some() {
                PARALLEL_EVAC_CLAIM_WAITS.fetch_add(1, Ordering::Relaxed);
            }
            return mark;
        }
        let since = *started.get_or_insert_with(std::time::Instant::now);
        rounds = rounds.wrapping_add(1);
        if rounds % 64 == 0 && since.elapsed() >= PEER_CLAIM_WAIT_LIMIT {
            let n = PARALLEL_EVAC_CLAIM_WAITS_ABANDONED.fetch_add(1, Ordering::Relaxed) + 1;
            if n <= 8 || n.is_power_of_two() {
                tracing::warn!(
                    "[g1] parallel evacuation gave up waiting on a FORWARDED|BUSY source after {:?} (#{n}): header=0x{:x} mark={mark:#010x} -- a genuine claim publishes two stores after it is taken, so this word is not one. Answered as not forwarded.",
                    PEER_CLAIM_WAIT_LIMIT,
                    hdr as *const ObjectHeader as usize,
                );
            }
            return mark;
        }
        for _ in 0..64 {
            std::hint::spin_loop();
        }
        std::thread::yield_now();
    }
}

/// Forwards whose target word in the source header disagreed with the target
/// the pause recorded for them when `retire_forwards` ran. Nonzero means a
/// store reached an abandoned source's second word during the pause.
pub static RETIRE_TARGET_MISMATCH: AtomicU64 = AtomicU64::new(0);

/// Roots `pinned_region_set_including_non_object_roots` refused because the
/// word they name carries no `GC_FLAG_HEADER` -- interior pointers, their
/// regions pinned rather than evacuated.
pub static ROOT_WITHOUT_HEADER_FLAG_PINNED: AtomicU64 = AtomicU64::new(0);

/// Snapshot of [`EVACUATE_CAS_LOSER_FORWARDS`].
pub fn evacuate_cas_loser_forwards() -> u64 {
    EVACUATE_CAS_LOSER_FORWARDS.load(std::sync::atomic::Ordering::Relaxed)
}

/// How many times `evacuate_object` was asked to evacuate a from-space object
/// this pause had ALREADY forwarded, with the forwarding tag no longer in the
/// object's mark word.
///
/// Expected to be ZERO. The tag is the only "already evacuated?" test
/// `evacuate_object` has had since F-02 (it is a load and a tag compare, not a
/// `pointer_map` probe), so a cleared tag makes a second copy of a live object
/// and OVERWRITES the correct `pointer_map` entry with the duplicate's address.
/// Nothing crashes: the duplicate is a well-formed object, because
/// `retire_forwards` deliberately preserves the shape quartet. What breaks is
/// object IDENTITY -- two live copies of one object, some holders rewritten to
/// each -- which is what the Spring Boot residual measured as reflection
/// metadata that reads back null, or names the wrong class, or is missing a
/// method the class declares.
///
/// Read it with [`reevacuated_after_retire`]. Non-zero means a driver retired
/// its forwards before a phase that still evacuates; see
/// `CRATONVM_G1_RETIRE_FORWARDS_LATE`.
pub static REEVACUATED_AFTER_RETIRE: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

/// Snapshot of [`REEVACUATED_AFTER_RETIRE`].
pub fn reevacuated_after_retire() -> u64 {
    REEVACUATED_AFTER_RETIRE.load(std::sync::atomic::Ordering::Relaxed)
}

/// Pauses on which Phase 3.5 (`resurrect_dead_finalizers`) had candidates and
/// therefore evacuated something AFTER the main closure had finished.
///
/// This is the denominator for [`REEVACUATED_AFTER_RETIRE`]. A retirement
/// ordering defect between the closure and Phase 3.5 can only show on a
/// workload where Phase 3.5 actually runs, and "does it run on THIS workload"
/// was not a question the collector could answer about itself.
pub static RESURRECT_PHASE_RUNS: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);
/// Objects Phase 3.5 pulled out of the collection set — see
/// [`RESURRECT_PHASE_RUNS`].
pub static RESURRECT_PHASE_COPIED: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

/// Snapshot of the Phase 3.5 census: `(runs, objects copied)`.
pub fn resurrect_phase_census() -> (u64, u64) {
    (
        RESURRECT_PHASE_RUNS.load(std::sync::atomic::Ordering::Relaxed),
        RESURRECT_PHASE_COPIED.load(std::sync::atomic::Ordering::Relaxed),
    )
}

/// One evacuation worker's private results, merged by the driver after the
/// completion barrier.
///
/// Kept per-worker rather than behind a shared lock because `forwards` is
/// appended to for every single object copied — the hottest write in the
/// pause — and a shared destination would serialise exactly the work the
/// parallel evacuator exists to spread.
#[derive(Default)]
pub struct EvacShard {
    pub objs: usize,
    pub bytes: usize,
    /// `(from_space_addr, to_space_addr)` for every object this worker copied.
    pub forwards: Vec<(usize, usize)>,
    /// Evacuation-failed (self-forwarded) objects this worker saw; scanned
    /// serially by the driver once all workers have stopped.
    pub deferred: Vec<usize>,
    /// Lane D (2026-09-20) — this worker's engagement row; see
    /// [`EvacWorkerCensus`].
    pub census: EvacWorkerCensus,
}

/// Lane D (2026-09-20) — one evacuation worker's ENGAGEMENT row: what it
/// actually did with the pause it was woken for.
///
/// # Why this exists
///
/// Until this wave the evacuator published exactly two numbers about its own
/// parallelism — `g1_young_evac_counts()`'s `parallel=`/`serial=` pair and
/// `workers_last=` — and both of them are about the DISPATCH, not the work. A
/// pause that woke 23 workers and then did every byte of the copying on the
/// driver produced character-for-character the same output as a pause that
/// spread it evenly. `lane-d-proposals.md` §3 says this is what blocks
/// evaluating proposals 3 and 4 (a real termination protocol, per-worker
/// deques): both are changes to how work REACHES a worker, and nothing in the
/// tree could see whether it reached one.
///
/// The concurrent marker already learned this lesson — `mark_worker_scans`
/// exists verbatim because "a total tells you marking happened; only the split
/// tells you whether it happened in parallel". This is that instrument for the
/// evacuator.
///
/// # Reading a row
///
/// * `scanned` is the Phase-3 closure's `process_object` count — the direct
///   analogue of `mark_worker_scans`. **The distribution is the point.** Worker
///   0 is always the driver; `scanned` concentrated on row 0 with zeros
///   beneath it is a serial evacuator wearing a parallel evacuator's clothes.
/// * `seed_regions` is Phase 2's split. It is zero on every helper unless
///   `CRATONVM_G1_PARALLEL_SEED=1`, which is exactly how an operator confirms
///   that lever engaged rather than merely being set.
/// * `seed_objs`/`seed_bytes` are copies made BEFORE the closure (roots,
///   keep-alives and the seed walk); `objs_copied`/`bytes_copied` are copies
///   made DURING it. Split because the driver is the only thread that can do
///   the first kind, so folding them together would make every pause look
///   driver-heavy whether or not the closure was.
/// * `lifts`/`lifted` count acquisitions of the SHARED gray queue and the
///   items they yielded. This evacuator has no per-worker deques yet
///   (`lane-d-proposals.md` §4), so a lift is the only way work crosses from
///   one worker to another: it is this design's steal, and `lifted == 0` on a
///   helper means that helper was never handed anything at all.
/// * `spills` counts the reverse direction — a worker whose local stack grew
///   past [`EVAC_LOCAL_PUBLISH_HIGH`] giving its oldest half back.
/// * `idle_ns` is wall time spent in the empty-queue arm with work still
///   outstanding, measured per idle EPISODE rather than per probe (one
///   `Instant::now()` pair per episode, not per `spin_loop`). Large `idle_ns`
///   beside small `scanned` is the termination-protocol cost §3 is about.
#[derive(Default, Clone, Copy, Debug)]
pub struct EvacWorkerCensus {
    /// Pauses in which this worker slot was dispatched at all.
    pub pauses: u64,
    /// Phase-2 remembered-set source regions this worker walked.
    pub seed_regions: u64,
    /// Objects copied before the Phase-3 closure began (roots, keep-alives,
    /// seed walk).
    pub seed_objs: u64,
    /// Bytes copied before the Phase-3 closure began.
    pub seed_bytes: u64,
    /// Phase-3 `process_object` calls — the reference scans.
    pub scanned: u64,
    /// Objects copied during the Phase-3 closure.
    pub objs_copied: u64,
    /// Bytes copied during the Phase-3 closure.
    pub bytes_copied: u64,
    /// Acquisitions of the shared gray queue that yielded work.
    pub lifts: u64,
    /// Items those acquisitions yielded.
    pub lifted: u64,
    /// Times this worker gave its oldest local half back to the shared queue.
    pub spills: u64,
    /// Of `spills`, the ones made because a peer was idle with the shared
    /// queue empty, below [`EVAC_LOCAL_PUBLISH_HIGH`]
    /// (`CRATONVM_G1_EVAC_IDLE_SPILL`).
    pub idle_spills: u64,
    /// Wall nanoseconds spent waiting with the queue empty and work still
    /// outstanding.
    pub idle_ns: u64,
    /// Empty probes made (spin or yield), for reading `idle_ns` against.
    pub idle_probes: u64,
}

impl EvacWorkerCensus {
    /// Fold `other` into `self`. Used once per worker per pause to publish a
    /// per-pause row into the process-lifetime table, and in the report to sum
    /// a column.
    pub fn merge(&mut self, other: &EvacWorkerCensus) {
        self.pauses += other.pauses;
        self.seed_regions += other.seed_regions;
        self.seed_objs += other.seed_objs;
        self.seed_bytes += other.seed_bytes;
        self.scanned += other.scanned;
        self.objs_copied += other.objs_copied;
        self.bytes_copied += other.bytes_copied;
        self.lifts += other.lifts;
        self.lifted += other.lifted;
        self.spills += other.spills;
        self.idle_spills += other.idle_spills;
        self.idle_ns += other.idle_ns;
        self.idle_probes += other.idle_probes;
    }

    /// Everything this worker copied, both phases. The denominator the
    /// distribution is read against.
    pub fn total_bytes(&self) -> u64 {
        self.seed_bytes + self.bytes_copied
    }
}

/// Each worker holds one Survivor TLAB (young survivors) and one Old TLAB
/// (tenured promotions).
pub struct TlabSet {
    pub survivor: Tlab,
    pub old: Tlab,
    /// gci i23/tn (`CRATONVM_G1_SURVIVOR_REGION_CAP`): this worker's Survivor
    /// copy failed after the pause's Survivor region cap refused a region,
    /// so every later copy of this worker goes to Old -- HotSpot's
    /// `allocate_in_next_plab` sets the worker's `_tenuring_threshold` to 0
    /// for the same reason (no new Survivor region will come, and each retry
    /// would take the slow path again). Per worker, per pause.
    pub survivor_full: bool,
    /// gci i26/pc: this worker's share of `G1Collector::survivor_age_bytes`
    /// (bytes PUBLISHED to Survivor space, by the age stamped into the copy),
    /// accumulated without atomics and flushed into the collector-wide
    /// histogram by [`Self::flush_copy_counters`] when `retire_all` retires the
    /// set. HotSpot's per-worker `G1ParScanThreadState::_age_table`, merged
    /// once in `G1ParScanThreadStateSet::flush_stats`. Before this every
    /// winning copy did a `fetch_add` on the collector's word -- in a young
    /// pause nearly every survivor has the same age, so every worker hit ONE
    /// cache line (`gci-i25pl-g1-parallel-copy-updates-two-global-counters-per-object-FIXED-20261017`).
    pub age_bytes: [usize; G1_MAX_TENURING_AGE],
    /// gci i26/pc: this worker's share of `G1Collector::bytes_promoted_total`
    /// (bytes PUBLISHED into Old), flushed with [`Self::age_bytes`].
    pub promoted_bytes: u64,
    /// g1ep: this worker's shares of `G1Collector::survivor_cap_promotions`
    /// and `survivor_overflow_promotions` (copies a full Survivor cap, or the
    /// overflow opt-in, sent to Old), flushed with [`Self::age_bytes`].
    pub cap_promotions: u64,
    pub overflow_promotions: u64,
    /// g1sz: this worker's share of the closure-derived to-space edges
    /// (`CRATONVM_G1_FIXUP_SKIP_CLOSURE_TOSPACE`); see [`ClosureTospaceEdges`].
    /// Moved into the pause's shared collection by `SharedEvac::retire_all`.
    pub tospace: ClosureTospaceEdges,
    /// g1sz: the provenance records of this worker's published forwards,
    /// flushed by `SharedEvac::retire_all` (and on drop).
    pub provenance: cratonvm_types::ProvenanceBatch,
}

impl Default for TlabSet {
    fn default() -> Self {
        Self {
            survivor: Tlab::new(RegionType::Survivor),
            old: Tlab::new(RegionType::Old),
            survivor_full: false,
            age_bytes: [0; G1_MAX_TENURING_AGE],
            promoted_bytes: 0,
            cap_promotions: 0,
            overflow_promotions: 0,
            tospace: ClosureTospaceEdges::default(),
            provenance: cratonvm_types::ProvenanceBatch::default(),
        }
    }
}

/// g1sz (`g1-default-sizing-never-collects-a-short-allocation-run`): the
/// cross-region edges of the objects the parallel closure copied, recorded by
/// the closure itself, so that Phase 4 need not walk the regions that hold
/// nothing else (`CRATONVM_G1_FIXUP_SKIP_CLOSURE_TOSPACE`, default ON, `=0`
/// kills).
///
/// # Why
///
/// The closure already reads every reference slot of every object it copies,
/// and rewrites each one that names a collection-set object
/// (`SharedEvac::process_object`). Phase 4 then walked the same objects a
/// second time, serially, to find slots to rewrite (none are left) and to
/// re-derive the holders' outgoing cross-region edges. On
/// `BinTreesClassic 18 -XX:+UseG1GC` with the sizing policy armed, that walk
/// was 40 ms of a 95 ms young pause (13.5 MB of survivors in two fresh
/// regions). HotSpot G1's whole pause on that row is 5-9 ms. HotSpot records
/// the edge when it copies (`G1ParScanThreadState::do_oop_evac` ->
/// `enqueue_card_if_tracked`) and has no second walk.
///
/// # What it records
///
/// Exactly what `collect_outgoing_cross_region_edges_noting_young` derives for
/// a holder outside the collection set, without a census and without the
/// young-card screen (a fresh region is "changed", and that screen never
/// takes a changed region). For each non-null slot, after the rewrite, a
/// target in another region that is collectable or a humongous start dirties
/// the holder's start card and names `(target, holder)` for the target's
/// remembered set.
///
/// # When Phase 4 still walks
///
/// `incomplete` is set by everything after which a fresh region could hold a
/// slot the closure did not settle: a refused holder or candidate, a failed or
/// self-forwarded evacuation, a clamped walk, a slot into a humongous
/// continuation or a Free region. The driver also refuses the skip when
/// anything copied objects after the closure (the serial self-forward drain,
/// Phase 3.5, the overlay follow), since the closure did not scan those
/// copies.
///
/// Collection-overlay rows (`g1_overlay.rs`) need no exception: an owner's
/// rows live in a side table, not in its slots, so Phase 4 never derived an
/// edge for them either, and the elements the closure expands from them are
/// ordinary children it copies and scans.
#[derive(Default)]
pub struct ClosureTospaceEdges {
    /// `(target region, holder region)`, deduplicated only by [`Self::memo`].
    pub edges: Vec<(usize, usize)>,
    /// A direct-mapped memo of recently pushed pairs, so a tree whose parents
    /// and children sit in two regions pushes a pair once per run of slots
    /// rather than once per slot.
    pub memo: [(usize, usize); 8],
    /// Set by any slot or holder the closure did not settle; see the type doc.
    pub incomplete: bool,
}

impl ClosureTospaceEdges {
    #[inline]
    pub fn push(&mut self, target: usize, holder: usize) {
        let way = (target ^ holder.wrapping_mul(0x9E37_79B9)) & 7;
        // `(0, 0)` is the empty way. A self-edge is never pushed, so it is
        // never a real entry.
        if self.memo[way] != (target, holder) {
            self.memo[way] = (target, holder);
            self.edges.push((target, holder));
        }
    }

    /// Move this worker's share into `into`, leaving this one empty.
    pub fn drain_into(&mut self, into: &mut ClosureTospaceEdges) {
        into.edges.append(&mut self.edges);
        into.incomplete |= std::mem::take(&mut self.incomplete);
        self.memo = Default::default();
    }
}





/// g1ep: `CRATONVM_G1_FULL_POINTER_MAP_SORTED` (default ON, `=0` kills): the
/// starved-full / retry path keeps its maps in the sorted form too -- the
/// in-place compaction builds its map by [`sorted_pointer_map_by_region`]
/// instead of folding it into the hash shards, and
/// [`G1Collector::compose_forward_maps`] merges two passes' maps as sorted
/// lists ([`cratonvm_types::PointerMap::compose_after`]) instead of thawing
/// the first into the shards and hashing the second into it. Read once per
/// compaction and per compose.
pub fn g1_full_pointer_map_sorted_enabled() -> bool {
    true
}





/// g1ep: how many forward lists [`G1Collector::give_forward_buffer`] keeps:
/// one per evacuation worker and the driver on the widest pool, with room.
pub const FORWARD_BUFFER_POOL: usize = 64;

/// g1ep: the largest forward list kept between pauses (16 M pairs, 256 MB); a
/// pause that recorded more frees it rather than holding it for the run.
pub const FORWARD_BUFFER_MAX_PAIRS: usize = 16 << 20;

/// g1ep: the shortest keep-alive list the pool takes (below it a dispatch costs
/// more than the copies).
pub const PARALLEL_KEEPALIVE_SEED_MIN: usize = 2048;

pub use cratonvm_gc_core::region::sorted_pointer_map_by_region;


impl TlabSet {
    /// gci i26/pc: [`G1Collector::note_survivor_age`] into this worker's
    /// private table. Same bucketing (ages at or above the last bucket share
    /// it) and the same wrapping arithmetic as the collector's `fetch_add`.
    #[inline]
    pub fn note_survivor_age(&mut self, age: u8, bytes: usize) {
        let idx = (age as usize).min(G1_MAX_TENURING_AGE - 1);
        self.age_bytes[idx] = self.age_bytes[idx].wrapping_add(bytes);
    }

    /// gci i26/pc: [`G1Collector::note_promoted_bytes`] into this worker's
    /// private counter.
    #[inline]
    pub fn note_promoted_bytes(&mut self, bytes: usize) {
        self.promoted_bytes = self.promoted_bytes.wrapping_add(bytes as u64);
    }

    /// gci i26/pc: add this worker's copy counters into the collector-wide
    /// ones -- one `fetch_add` per non-zero age bucket and one for the
    /// promoted bytes -- and zero them, so a second flush (a helper slot
    /// retired by its worker and again by the driver's sweep) adds nothing.
    ///
    /// The totals equal the per-copy calls' exactly: both are sums of the same
    /// `obj_size`s over the same winning arms. Only WHEN the collector's words
    /// move changes (at retirement instead of at each copy), and every reader
    /// (`teach_young_cost_model`, `update_tenuring_threshold_merging`,
    /// `bytes_promoted_total`) runs after `parallel_evacuate` has retired
    /// every `TlabSet` it created. The flush happens on the worker's own
    /// thread before the pool barrier, which orders it before the driver's
    /// reads exactly as the per-copy `Relaxed` adds were ordered.
    pub fn flush_copy_counters(&mut self, gc: &G1Collector) {
        for (age, b) in self.age_bytes.iter_mut().enumerate() {
            if *b != 0 {
                gc.survivor_age_bytes[age].fetch_add(*b, Ordering::Relaxed);
                *b = 0;
            }
        }
        if self.promoted_bytes != 0 {
            gc.bytes_promoted_total
                .fetch_add(self.promoted_bytes, Ordering::Relaxed);
            self.promoted_bytes = 0;
        }
        if self.cap_promotions != 0 {
            gc.survivor_cap_promotions
                .fetch_add(self.cap_promotions, Ordering::Relaxed);
            self.cap_promotions = 0;
        }
        if self.overflow_promotions != 0 {
            gc.survivor_overflow_promotions
                .fetch_add(self.overflow_promotions, Ordering::Relaxed);
            self.overflow_promotions = 0;
        }
    }
}

/// Width of each of the three claim counters packed into
/// [`SharedEvac::pool_next`]. 2^21 regions is 2 TiB at the 1 MiB minimum
/// region size.
pub const POOL_CURSOR_BITS: u32 = 21;

/// Immutable shared state handed to every evacuation worker (the driver and the
/// spawned threads). Auto-`Sync` because every field is `Sync` (`RegionsBase`
/// via the `unsafe impl` above).
pub struct SharedEvac<'a> {
    pub collector: &'a G1Collector,
    pub regions_base: RegionsBase,
    /// Length of the regions table `regions_base` points at, so the shared
    /// header screens can be reached through a [`RegionView::Raw`]. Without it
    /// the parallel arm has no bounded way to read a region and had to go
    /// without the screens the serial arm has applied since 2026-09-02.
    pub regions_len: usize,
    /// CSet membership (region indices being evacuated FROM).
    pub cset: &'a RegionSet,
    /// Free-region indices available for to-space TLAB claiming: the Free
    /// regions inside the COMMITTED prefix, ascending, followed by the Free
    /// regions of the uncommitted tail, ascending
    /// ([`G1Collector::parallel_to_space_pool`] builds it). The order is
    /// load-bearing — see [`Self::claim_pool_slot`].
    pub pool: &'a [usize],
    /// How many leading entries of [`Self::pool`] are committed regions. The
    /// rest are address space that `commit_through_region` must grow into.
    pub pool_committed: usize,
    /// [`Self::pool`] as a membership test. A region in the pool is, or is
    /// about to be, some worker's exclusively-owned TLAB, and `retire_tlab`
    /// STORES its cursor — so the shared-destination fallback below must never
    /// bump-allocate into one, or that store would clobber the bump.
    pub pool_set: &'a RegionSet,
    /// Lock-free claim cursor into [`Self::pool`]: three counters of
    /// [`POOL_CURSOR_BITS`] bits packed into one word. The lowest counts claims
    /// taken from the front of the committed part (Old), the middle claims
    /// taken from the back of the committed part (Survivor), the highest
    /// claims that grew into the uncommitted tail. See
    /// [`Self::claim_pool_slot`] for why, and for why they share one word
    /// rather than being separate atomics.
    pub pool_next: &'a AtomicU64,
    /// Non-CSet regions of the OLD destination type that still have room --
    /// the regions a previous pause's promotion TLABs left partly filled.
    ///
    /// WHY THIS EXISTS. `tlab_alloc` used to reach only for `pool`, i.e. FREE
    /// regions, so every worker's Old TLAB consumed a whole fresh region in
    /// every pause and abandoned it partly filled at `retire_all`. The old
    /// generation therefore grew by ONE REGION PER WORKER PER YOUNG PAUSE
    /// whatever the promoted volume was. Measured on the Tomcat
    /// `TestHostConfigAutomaticDeploymentXmlExternalWarXml` class at
    /// `-Xmx2g` on a 32-CPU box (23 evacuation workers), one binary,
    /// arms interleaved:
    ///
    /// | arm | modal Old growth per pause | peak Old | verdict |
    /// |---|---|---|---|
    /// | default, 23 workers | **+23** | 2026 of 2048 | OOM |
    /// | `CRATONVM_G1_WORKERS=4` | **+4** | 297 | PASS |
    /// | `CRATONVM_G1_PARALLEL_EVAC=0` | +1 | **30** | PASS |
    ///
    /// The quantum IS the worker count. The serial arm does not have the
    /// defect because `alloc_in_type_locked_scan` scans every existing
    /// non-CSet region of the destination type before it claims a Free one --
    /// which is exactly what this list restores, at TLAB granularity.
    ///
    /// Old only. On a young pause every Survivor region is in the CSet, so a
    /// Survivor resume list would be empty by construction; on a mixed pause
    /// the CSet filter below removes the Old regions being collected.
    pub resume: &'a [usize],
    /// [`Self::resume`] as a membership test, for the same reason
    /// [`Self::pool_set`] exists: a resumed region is a worker's exclusively
    /// owned TLAB and `retire_tlab` STORES its offset as the cursor, so
    /// [`Self::shared_dest_alloc`] must not bump into it.
    pub resume_set: &'a RegionSet,
    /// Lock-free claim cursor into `resume`.
    pub resume_next: &'a AtomicUsize,
    /// Lane D wave 2 (2026-09-20) — the SMALLEST request size for which
    /// [`Self::shared_dest_alloc`] has already proved, by a full pass, that no
    /// eligible region of that destination type has room. Index 0 is Survivor,
    /// index 1 is Old; `usize::MAX` means "not proved". See
    /// [`Self::shared_dest_proved_empty`] for why a proof survives the rest of
    /// the pause.
    pub dest_empty_from: &'a [AtomicUsize; 2],
    /// Gray-object work queue (to-space addresses awaiting a ref scan).
    pub queue: &'a Mutex<Vec<usize>>,
    /// Termination counter: queued + in-progress items (see protocol note 4).
    pub outstanding: &'a AtomicUsize,
    /// Workers currently in an idle episode (queue empty, work outstanding),
    /// read by [`SharedEvac::run_worker`]'s idle-peer spill. A hint only: it
    /// decides when a busy worker gives work away, never whether any object
    /// is reached.
    pub idle_peers: &'a AtomicUsize,
    /// Tenuring threshold copied from the collector config.
    pub promotion_age: u8,
    /// `CRATONVM_G1_EVAC_COPY_WATCH=1` only; see [`CopyWatch`].
    pub copy_watch: Option<&'a CopyWatch>,
    /// The frozen-peer TLAB skip spans, hoisted to ONE clone per pause.
    ///
    /// `seed_source_region` used to call `collector.jit_tlab_skip_spans()`
    /// itself, which takes a mutex and CLONES the span Vec — once per source
    /// region, on a phase whose whole job is to walk every remembered-set
    /// source. The spans cannot change during a pause (every mutator is
    /// parked), so the clone is pure per-region overhead, and under the
    /// parallel seed below it would be a contended mutex as well.
    pub jit_skips: &'a [(usize, usize)],
    /// Per-pause snapshots of the flags this evacuator reads inside its
    /// hottest loops.
    ///
    /// `gc_flags()` is a `OnceLock` deref — an atomic load and a branch — and
    /// `screens_armed` alone was reached once per holder AND once per
    /// CSet-resident candidate. The values are latched for the life of the
    /// process, so snapshotting them once per pause changes nothing about what
    /// the pause decides and takes the load off the inner loops. It also makes
    /// the decision uniform across the workers of one pause by construction,
    /// which is the same argument `promotion_age` above is snapshotted for.
    pub screens: bool,
    pub supply_screen: bool,
    pub shared_dest: bool,
    /// gci i14/yt — `CRATONVM_G1_SURVIVOR_PLAB` (default ON since 2026-10-15,
    /// `=0` kills), read once per pause on the driver: Survivor copies go into PLAB chunks of a shared
    /// current Survivor region ([`Self::survivor_plab_region`]) instead of one
    /// whole region per copying worker. See [`Self::claim_survivor_plab`].
    pub survivor_plab: bool,
    /// The shared current Survivor region of this pause, `usize::MAX` before
    /// the first claim, [`SURVIVOR_PLAB_SWITCHING`] while one worker installs
    /// the next one (gci i15/ra). Per pause (a local of `parallel_evacuate`).
    pub survivor_plab_region: &'a AtomicUsize,
    /// JIT round 23 wave 10, lane L9 (`CRATONVM_G1_OLD_PLAB`, OPT-IN `=1` since
    /// the n23-w10a A/B): Old (promotion) copies go into PLAB chunks of a shared current
    /// Old region ([`Self::old_plab_region`]) instead of one whole Free region
    /// per promoting worker, as the Survivor copies have since gci i14/yt.
    /// Read once per pause on the driver. See [`Self::claim_old_plab`].
    pub old_plab: bool,
    /// The shared current Old region of this pause, `usize::MAX` before the
    /// first claim, [`SURVIVOR_PLAB_SWITCHING`] while one worker installs the
    /// next one. Per pause (a local of `parallel_evacuate`).
    pub old_plab_region: &'a AtomicUsize,
    /// JIT round 23 wave 10, lane L9: the Old PLAB chunk size this pause
    /// (`old_plab_bytes_for`: about `region_size / workers`), so a worker keeps
    /// a long run of its own chain's promotions in one chunk and the shared
    /// region switches about once per worker per region, not once per 32 KB.
    pub old_plab_bytes: usize,
    /// gci i25/pl — `CRATONVM_G1_PLAB_COPY_END_ELIDE` (default ON, `=0`
    /// restores the per-copy publish), read once per pause on the driver: a
    /// copy placed in a SHARED Survivor PLAB chunk (or a direct copy into the
    /// shared region) skips [`G1Region::publish_parallel_copy_end`], because
    /// the region's own cursor already covers it. See
    /// [`plab_copy_end_needs_publish`].
    pub plab_copy_end_elide: bool,
    /// `CRATONVM_G1_EVAC_ONE_STORE_MARK` (default ON), read once per pause on
    /// the driver: see [`destination_mark`].
    pub one_store_mark: bool,
    /// `CRATONVM_G1_EVAC_COPY_END_OWNER_STORE` (default ON), read once per
    /// pause on the driver: see [`copy_end_owned_by`].
    pub copy_end_owner_store: bool,
    /// `CRATONVM_G1_EVAC_DEST_LATCH` — see [`Self::dest_empty_slot`].
    pub dest_latch: bool,
    pub card_rset: bool,
    pub card_clean: bool,
    /// Holders the last trustworthy completed mark proves dead; see
    /// [`MarkProvenDead`]. Absent unless such a mark exists and no cycle is
    /// running.
    pub precise_rset_liveness: Option<&'a MarkProvenDead>,
    /// Pinned-source walks are deliberately conservative: an interior JIT
    /// root may not appear in the precise Java root graph, so never filter
    /// their holders.
    pub wholesale_sources: &'a RegionSet,
    /// `CRATONVM_G1_EVAC_SHARE_CENSUS` — see [`EvacShareCensus`]. Snapshotted
    /// on the DRIVER thread with the rest of the flags above, which is what
    /// makes it reachable from a `with_thread_overrides` test: the workers are
    /// spawned threads and a thread-local flag override does not follow them.
    pub share_census: bool,
    /// `CRATONVM_G1_EVAC_IDLE_SPILL` and `CRATONVM_G1_EVAC_IDLE_SLEEP`, read on
    /// the driver once per pause for the same reason as `share_census`. See
    /// [`Self::run_worker`].
    pub idle_spill: bool,
    pub idle_sleep: bool,
    /// JIT round 23 wave 8, lane L9: `CRATONVM_G1_EVAC_LEAF_NO_PUSH`, read on
    /// the driver once per pause. See [`Self::record_fresh_child_screened`].
    pub leaf_no_push: bool,
    /// g1sz: `CRATONVM_G1_EVAC_PREFETCH`, read on the driver once per pause.
    /// See [`Self::prefetch_cset_referent`].
    pub prefetch: bool,
    /// g1pin: the collection's per-object peer-stack pins, sorted, copied on
    /// the driver once per pause (`G1Collector::peer_object_pins_snapshot`).
    /// [`Self::evacuate_screened`] self-forwards these instead of copying.
    pub object_pins: Vec<usize>,
    /// Region bitset covering `object_pins` so `object_pinned` skips `binary_search` in O(1)
    /// for all non-pinned regions.
    pub object_pin_regions: RegionSet,
    /// JIT round 23 wave 8, lane L9: `CRATONVM_G1_EVAC_IDLE_TRY_LOCK`, read on
    /// the driver once per pause. See [`Self::run_worker`]'s idle probe.
    pub idle_try_lock: bool,
    /// Total evacuation workers this pause (driver included), for sizing the
    /// batch a worker lifts out of the shared queue. See [`Self::run_worker`].
    pub workers: usize,
    /// Overlay rows read before the closure (`g1_overlay.rs`): a worker that
    /// freshly copies a CSet owner evacuates its rows as children.
    pub(in crate::g1) overlay: Option<&'a overlay::OverlayPrepass>,
    /// g1sz: record the copied objects' edges (`CRATONVM_G1_FIXUP_SKIP_CLOSURE_TOSPACE`,
    /// read on the driver once per pause). See [`ClosureTospaceEdges`].
    pub tospace_edges: bool,
    /// g1sz: where [`Self::retire_all`] moves each worker's
    /// [`TlabSet::tospace`].
    pub tospace: &'a Mutex<ClosureTospaceEdges>,
}

