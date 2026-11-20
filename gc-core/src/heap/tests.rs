use super::*;

/// gc-common w3-e: the relaxed-atomic element accessors are bit-exact
/// round trips for every primitive type, including the sign/zero extension
/// each arm owes (`byte` and `short` sign-extend, `char` and `boolean`
/// zero-extend), NaN payloads, and the `boolean` narrowing to one bit. Run
/// over an ALIGNED data area (the production shape) and a deliberately
/// misaligned one (the plain-access fallback), which must agree.
#[test]
fn primitive_element_accessors_round_trip_aligned_and_misaligned() {
    // 8-aligned backing store; `+ 1` below gives a misaligned `base`.
    let mut store = vec![0u64; 16];
    let aligned = store.as_mut_ptr() as *mut u8;
    let nan_bits_f = 0x7fc0_1234u32;
    let nan_bits_d = 0x7ff8_0000_dead_beefu64;
    let cases: [(ArrayElementType, Value, Value); 9] = [
        (ArrayElementType::Int, Value::Int(-7), Value::Int(-7)),
        (
            ArrayElementType::Long,
            Value::Long(i64::MIN + 3),
            Value::Long(i64::MIN + 3),
        ),
        (
            ArrayElementType::Float,
            Value::Float(f32::from_bits(nan_bits_f)),
            Value::Float(f32::from_bits(nan_bits_f)),
        ),
        (
            ArrayElementType::Double,
            Value::Double(f64::from_bits(nan_bits_d)),
            Value::Double(f64::from_bits(nan_bits_d)),
        ),
        (ArrayElementType::Byte, Value::Int(0xff), Value::Int(-1)),
        (ArrayElementType::Boolean, Value::Int(2), Value::Int(0)),
        (
            ArrayElementType::Char,
            Value::Int(0xfffe),
            Value::Int(0xfffe),
        ),
        (
            ArrayElementType::Short,
            Value::Int(0x8001),
            Value::Int(-32767),
        ),
        (
            ArrayElementType::Int,
            Value::Int(i32::MAX),
            Value::Int(i32::MAX),
        ),
    ];
    for base in [aligned, aligned.wrapping_add(1)] {
        for (et, stored, expected) in cases {
            // SAFETY: index 3 of an 8-byte element is byte 24..32 of
            // `base`, inside the 128-byte store even with the +1 offset.
            let got = unsafe {
                write_prim_element(base, 3, et, stored);
                read_prim_element(base, 3, et)
            };
            match (got, expected) {
                (Value::Float(a), Value::Float(b)) => assert_eq!(a.to_bits(), b.to_bits()),
                (Value::Double(a), Value::Double(b)) => assert_eq!(a.to_bits(), b.to_bits()),
                (a, b) => assert_eq!(a, b, "{et:?} misrounded at base {base:p}"),
            }
        }
    }
    drop(store);
}

/// gc-common w1-g: the borrowing oop-map entry point answers exactly what
/// the `Arc`-returning one does -- for a legacy object, for a compact
/// object with a registered layout, and for a compact object whose layout
/// is missing (where BOTH must count the fail-open).
///
/// Registers a layout for a `(class_id, field_count)` pair no other gc test
/// allocates with, and never unregisters it (monotone process state; see
/// the note on `g1::tests::parallel_evacuation_scans_compact_object_reference_fields`).
#[test]
fn with_compact_oop_scan_agrees_with_compact_oop_scan() {
    use cratonvm_types::field_layout::FieldStorageKind;
    const CID: u32 = 4343;
    cratonvm_types::register_class_layout(
        cratonvm_types::FIRST_LAYOUT_DOMAIN,
        CID,
        Arc::new(CompactLayout {
            field_disps: vec![8, 16],
            is_ref: vec![false, true],
            field_kinds: vec![FieldStorageKind::Int, FieldStorageKind::Reference],
            ref_disps: vec![16],
            total_size: 24,
        }),
    );

    // Legacy object: both say "not compact".
    let legacy = ObjectHeader::new(
        ClassId::new(CID),
        ObjectKind::Object,
        ArrayElementType::Reference,
        0,
        2,
    );
    assert!(compact_oop_scan(&legacy).is_none());
    assert!(with_compact_oop_scan(&legacy, |_, _| ()).is_none());

    // Compact object with a registered layout: same oop map, same body.
    let mut compact = ObjectHeader::new(
        ClassId::new(CID),
        ObjectKind::Object,
        ArrayElementType::Reference,
        0,
        2,
    );
    compact.set_compact_shape(2, 16);
    let (arc, body) = compact_oop_scan(&compact).expect("layout is registered");
    let borrowed = with_compact_oop_scan(&compact, |l, b| (l.ref_disps.clone(), b))
        .expect("layout is registered");
    assert_eq!(borrowed, (arc.ref_disps.clone(), body));
    assert_eq!(borrowed, (vec![16], 24));

    // Compact flag but no layout for its class (a compact instance's field
    // count IS its class layout's, so an orphan is a class with none):
    // both report the fail-open.
    let mut orphan = ObjectHeader::new(
        ClassId::new(CID + 1),
        ObjectKind::Object,
        ArrayElementType::Reference,
        0,
        3,
    );
    orphan.set_compact_shape(3, 24);
    let before = COMPACT_OOP_MAP_MISSING.load(Ordering::Relaxed);
    assert!(compact_oop_scan(&orphan).is_none());
    assert!(with_compact_oop_scan(&orphan, |_, _| ()).is_none());
    assert!(
        COMPACT_OOP_MAP_MISSING.load(Ordering::Relaxed) >= before + 2,
        "both entry points must count a missing oop map"
    );
}

/// gc-common w4-e: on a class with a registered compact layout,
/// `alloc_object_with_descriptors` must not stride 16-byte `Value` cells
/// over the packed body (it used to write `num_fields * SLOT_SIZE` bytes of
/// tagged cells into a 16-byte body -- the tag of the first cell then read
/// back as the `long`, and the second cell landed past the allocation),
/// and the fallible `try_alloc_object` plans the same shape as
/// `alloc_object`.
///
/// A `(class_id, field_count)` pair no other gc test allocates with; the
/// registration is monotone process state, as in the test above.
#[test]
fn descriptor_init_and_fallible_alloc_respect_the_compact_shape() {
    use cratonvm_types::field_layout::FieldStorageKind;
    const CID: u32 = 4351;
    cratonvm_types::register_class_layout(
        cratonvm_types::FIRST_LAYOUT_DOMAIN,
        CID,
        Arc::new(CompactLayout {
            field_disps: vec![8, 16],
            is_ref: vec![false, true],
            field_kinds: vec![FieldStorageKind::Long, FieldStorageKind::Reference],
            ref_disps: vec![16],
            total_size: 24,
        }),
    );
    let heap = Heap::with_capacity(1 << 20);

    let obj = heap.alloc_object_with_descriptors(ClassId::new(CID), 2, b"JL");
    assert_eq!(
        heap.get_field(obj, 0),
        Value::Long(0),
        "the long field's default"
    );
    assert_eq!(
        heap.get_field(obj, 1),
        Value::Object(None),
        "the reference default"
    );

    let infallible = heap.alloc_object(ClassId::new(CID), 2);
    let fallible = heap
        .try_alloc_object(ClassId::new(CID), 2)
        .expect("a 1 MiB semi-space has room for one small object");
    assert_eq!(
        cratonvm_types::is_compact_object(heap.get_header(infallible)),
        cratonvm_types::is_compact_object(heap.get_header(fallible)),
        "one class, one shape, whichever allocator served it"
    );
    heap.set_field(fallible, 0, Value::Long(-7));
    assert_eq!(heap.get_field(fallible, 0), Value::Long(-7));
    assert_eq!(heap.get_field(fallible, 1), Value::Object(None));
}

#[test]
fn header_size_check() {
    assert_eq!(std::mem::size_of::<ObjectHeader>(), HEADER_SIZE);
}

/// gci i27/gs (P1/ga, d10t): only roots STRICTLY inside a span that reads
/// unallocated are dropped; the span start, `end`, anything outside and
/// every root of a span that holds something are kept; each span is
/// judged once. Fake addresses; nothing is read.
#[test]
fn gci_i27_gs_takeover_drops_only_words_inside_unallocated_tails() {
    let at = |a: usize| unsafe { ObjectRef::from_raw(a as *mut u8) };
    let spans = [(0x1000usize, 0x2000usize), (0x4000, 0x5000)];
    let judged = std::cell::Cell::new(0usize);
    let judge = |s: usize, _e: usize| {
        judged.set(judged.get() + 1);
        if s == 0x1000 {
            SkipSpanMemory::Unallocated
        } else {
            SkipSpanMemory::Holds { offset: 0x10 }
        }
    };
    let mut roots = vec![
        at(0x1000), // span start: kept
        at(0x1008), // inside, unallocated: dropped
        at(0x1ff8), // inside, unallocated: dropped
        at(0x2000), // `end` is exclusive: kept
        at(0x0ff8), // below: kept
        at(0x4008), // inside a span that holds something: kept
    ];
    let dropped = drop_roots_inside_unallocated_spans_judged(&spans, &mut roots, &judge);
    assert_eq!(dropped, 2);
    let left: Vec<usize> = roots.iter().map(|r| r.as_ptr() as usize).collect();
    assert_eq!(left, vec![0x1000, 0x2000, 0x0ff8, 0x4008]);
    assert_eq!(judged.get(), 2, "each span judged once");

    // Nothing published, or no root: nothing judged, nothing dropped.
    let mut roots = vec![at(0x1008)];
    assert_eq!(drop_roots_inside_unallocated_spans_judged(&[], &mut roots, &judge), 0);
    assert_eq!(roots.len(), 1);

    // A real zeroed tail: the production reader agrees.
    let (buf, s, e) = zeroed_span(16);
    let mut roots = vec![at(s), at(s + 8), at(e - 8)];
    let n = unsafe { drop_roots_inside_unallocated_spans(&[(s, e)], &mut roots) };
    assert_eq!(n, 2);
    assert_eq!(roots.len(), 1);
    drop(buf);
}

/// The skip-span counting rule over FAKE addresses, with every span judged
/// stale (gcd d10/t): the tests below predate the span-memory judgement
/// and pin the bounds and the address representation, which must not read
/// the addresses they name. Production reads the span
/// (`judge_skip_span_memory`), tested separately over a real buffer.
fn judged_stale(spans: &[(usize, usize)], roots: &[ObjectRef], backend: &'static str) -> usize {
    let stale = |_: usize, _: usize| SkipSpanMemory::Holds { offset: 0x10 };
    skip_spans_hold_no_root_judged(spans, roots, backend, &stale)
}

/// A zeroed span, whole: `(buffer, start, end)` with `start` 8-aligned.
fn zeroed_span(words: usize) -> (Vec<u64>, usize, usize) {
    let buf = vec![0u64; words];
    let start = buf.as_ptr() as usize;
    (buf, start, start + words * 8)
}

/// gcd d10/t: an unallocated tail reads zero past at most one in-flight
/// header at its start; anything else is the owner's allocation.
#[test]
fn span_memory_reads_unallocated_only_when_zero_past_one_header() {
    let mark_word = |flags: u8| -> u64 { (u64::from(flags) << 24) << 32 };
    // All zero: unallocated.
    let (buf, s, e) = zeroed_span(64);
    assert_eq!(
        unsafe { judge_skip_span_memory(s, e) },
        SkipSpanMemory::Unallocated
    );
    drop(buf);

    // A LEGACY in-flight header at the start (class word + mark word with
    // GC_FLAG_HEADER, then `num_slots` / hash): still unallocated.
    let (mut buf, s, e) = zeroed_span(64);
    buf[0] = 0x2a | mark_word(GC_FLAG_HEADER);
    buf[1] = 3;
    assert_eq!(
        unsafe { judge_skip_span_memory(s, e) },
        SkipSpanMemory::Unallocated
    );
    // ...but a non-zero field past that header is an allocation.
    buf[5] = 0xdead_b000;
    assert_eq!(
        unsafe { judge_skip_span_memory(s, e) },
        SkipSpanMemory::Holds { offset: 40 }
    );
    drop(buf);

    // A COMPACT in-flight header is ONE word: its second word is a field.
    let (mut buf, s, e) = zeroed_span(64);
    buf[0] = 0x2a | mark_word(GC_FLAG_HEADER | GC_FLAG_COMPACT);
    assert_eq!(
        unsafe { judge_skip_span_memory(s, e) },
        SkipSpanMemory::Unallocated
    );
    buf[1] = 0x1000;
    assert_eq!(
        unsafe { judge_skip_span_memory(s, e) },
        SkipSpanMemory::Holds { offset: 8 }
    );
    drop(buf);

    // A non-header first word (a non-zero mark dword without
    // GC_FLAG_HEADER) is not an in-flight object: the span holds
    // something else.
    let (mut buf, s, e) = zeroed_span(64);
    buf[0] = 0x1234 | (0x5678 << 32);
    assert_eq!(
        unsafe { judge_skip_span_memory(s, e) },
        SkipSpanMemory::Holds { offset: 0 }
    );
    drop(buf);

    // t1 wave 12 (ARGFIX): a HALF-WRITTEN header -- a peer frozen after
    // the class dword (and, for an array, the length) but before the
    // mark -- is the in-flight object, not an allocation.
    let (mut buf, s, e) = zeroed_span(64);
    buf[0] = 0x1234; // instance: class dword only
    assert_eq!(
        unsafe { judge_skip_span_memory(s, e) },
        SkipSpanMemory::Unallocated
    );
    buf[0] = 0; // `new long[6]`: class word 0, then the length
    buf[1] = 6;
    assert_eq!(
        unsafe { judge_skip_span_memory(s, e) },
        SkipSpanMemory::Unallocated
    );
    buf[0] = 0x2a; // `anewarray`: component class, then the length
    assert_eq!(
        unsafe { judge_skip_span_memory(s, e) },
        SkipSpanMemory::Unallocated
    );
    // ...a 64-bit second word is not a length: something else is there.
    buf[1] = 0x7f00_0000_1000;
    assert_eq!(
        unsafe { judge_skip_span_memory(s, e) },
        SkipSpanMemory::Holds { offset: 8 }
    );
    // ...and anything past the 16-byte header is still an allocation.
    buf[1] = 6;
    buf[2] = 0x2b | mark_word(GC_FLAG_HEADER);
    assert_eq!(
        unsafe { judge_skip_span_memory(s, e) },
        SkipSpanMemory::Holds { offset: 16 }
    );
    drop(buf);

    // A SECOND object past the first header: stale, whatever its fields.
    let (mut buf, s, e) = zeroed_span(64);
    buf[0] = 0x2a | mark_word(GC_FLAG_HEADER);
    buf[1] = 1;
    buf[4] = 0x2b | mark_word(GC_FLAG_HEADER);
    assert_eq!(
        unsafe { judge_skip_span_memory(s, e) },
        SkipSpanMemory::Holds { offset: 32 }
    );
    drop(buf);

    // Degenerate spans never read as unallocated.
    let (buf, s, _) = zeroed_span(4);
    assert_eq!(
        unsafe { judge_skip_span_memory(s + 4, s + 24) },
        SkipSpanMemory::Holds { offset: 0 }
    );
    assert_eq!(
        unsafe { judge_skip_span_memory(s, s) },
        SkipSpanMemory::Holds { offset: 0 }
    );
    drop(buf);
}

/// gcd d10/t (`gcd-d8x-tlab-skip-span-guard-sees-roots-inside-published-spans`):
/// the report shapes of the d7 runs over a REAL zeroed span -- the
/// companion of the TLAB `end` (`end - 8`) and an uncommitted new cursor
/// behind an in-flight header -- are counted in
/// `SKIP_SPAN_UNALLOCATED_ROOTS` and are not violations; the same roots in
/// a span its owner allocated into still are.
#[test]
fn roots_inside_an_unallocated_span_are_counted_not_reported() {
    let (mut buf, s, e) = zeroed_span(0x400);
    let at = |a: usize| unsafe { ObjectRef::from_raw(a as *mut u8) };
    let spans = [(s, e)];
    // The peer is frozen between its header stores and its commit: a
    // legacy header at the cursor, its new cursor `s + 0x50` in a register.
    buf[0] = 0x2a | ((u64::from(GC_FLAG_HEADER) << 24) << 32);
    buf[1] = 4;
    let roots = [at(e - 8), at(s + 0x50), at(s + 0x48)];
    let before = SKIP_SPAN_UNALLOCATED_ROOTS.load(Ordering::Relaxed);
    assert_eq!(skip_spans_hold_no_root(&spans, &roots, "test"), 0);
    assert!(SKIP_SPAN_UNALLOCATED_ROOTS.load(Ordering::Relaxed) >= before + 3);
    // The owner allocated a second object: every root inside is reported.
    buf[10] = 0x2b | ((u64::from(GC_FLAG_HEADER) << 24) << 32);
    assert_eq!(skip_spans_hold_no_root(&spans, &roots, "test"), 3);
    drop(buf);
}

/// The skip-span root invariant, and specifically the two ways it can be
/// made VACUOUS rather than wrong.
///
/// A guard that never matches reports a clean zero forever, and a zero
/// from a check nobody has exercised is indistinguishable from a healthy
/// collector -- which is exactly how the G1 sweep read as fine for a
/// session while it reclaimed live objects. So this pins the two things
/// that decide whether a match is even possible: the half-open bound, and
/// the address representation.
#[test]
fn a_root_inside_a_published_skip_span_is_a_violation() {
    // Spans are ABSOLUTE [start, end). Fake addresses; nothing is read.
    let spans = [(0x1000usize, 0x2000usize)];
    let at = |a: usize| unsafe { ObjectRef::from_raw(a as *mut u8) };

    // Inside.
    assert_eq!(
        judged_stale(&spans, &[at(0x1800)], "test"),
        1,
        "a root inside a published span must disprove the span"
    );

    // `end` is EXCLUSIVE. An off-by-one at the top bound is the difference
    // between a guard and a decoration. The first word past `start` is
    // inside; `start` itself is the TLAB cursor (gen r5w1/young5), counted
    // in `SKIP_SPAN_CURSOR_ROOTS` rather than reported.
    assert_eq!(judged_stale(&spans, &[at(0x1008)], "test"), 1);
    let before = SKIP_SPAN_CURSOR_ROOTS.load(Ordering::Relaxed);
    assert_eq!(
        judged_stale(&spans, &[at(0x1000)], "test"),
        0,
        "a root exactly at the span start is the allocation cursor, not an object"
    );
    assert!(SKIP_SPAN_CURSOR_ROOTS.load(Ordering::Relaxed) > before);
    assert_eq!(
        judged_stale(&spans, &[at(0x2000)], "test"),
        0,
        "the end bound is exclusive -- the first byte past a tail is not in it"
    );
    assert_eq!(judged_stale(&spans, &[at(0x0ff8)], "test"), 0);

    // Empty either side is silence, not a match.
    assert_eq!(judged_stale(&[], &[at(0x1800)], "test"), 0);
    assert_eq!(judged_stale(&spans, &[], "test"), 0);
}

/// `gen_heap` holds skip spans as young-from `(offset, size)` and G1 holds
/// them as absolute `[start, end)`. Handing this function the former
/// UNCONVERTED does not fail loudly -- it silently never matches. This
/// test states that trap as a fact, so a future caller that skips the
/// conversion has something to fail against.
#[test]
fn offsets_passed_as_addresses_silently_never_match() {
    let base = 0x1_0000usize;
    let (offset, size) = (0x800usize, 0x400usize);
    let root = unsafe { ObjectRef::from_raw((base + offset + 0x10) as *mut u8) };

    // The WRONG shape: the raw (offset, size) pair, as `gen_heap` holds it.
    assert_eq!(
        judged_stale(&[(offset, size)], &[root], "test"),
        0,
        "offsets cannot match an absolute root -- this is the vacuous case"
    );

    // The conversion `sweep_young_non_moving` actually performs.
    assert_eq!(
        judged_stale(&[(base + offset, base + offset + size)], &[root], "test"),
        1,
        "converted to absolute [start, end), the same span matches"
    );
}

/// Every root in a span counts, not just the first: the count is what says
/// how much of the root set a stale span swallowed (46, on the netty
/// repro), and reporting only the first would have understated it.
#[test]
fn every_root_in_a_span_is_counted() {
    let spans = [(0x1000usize, 0x2000usize), (0x5000usize, 0x5100usize)];
    let at = |a: usize| unsafe { ObjectRef::from_raw(a as *mut u8) };
    let roots = [at(0x1000), at(0x1ff8), at(0x3000), at(0x5080), at(0x5100)];
    // gen r5w1/young5: `0x1000` is EXACTLY the first span's start, i.e. its
    // TLAB cursor, which is counted in `SKIP_SPAN_CURSOR_ROOTS` and is not
    // a violation. The two roots strictly inside a span are.
    assert_eq!(judged_stale(&spans, &roots, "test"), 2);
}

/// gc-common w1-g: past `SKIP_SPAN_LINEAR_MAX` spans the check sorts a
/// copy and binary-searches. The count must be EXACTLY the nested loop's
/// -- including for an unsorted list, overlapping spans (a root in two
/// spans counts twice, as it always did) and empty/inverted spans.
#[test]
fn the_sorted_span_check_counts_exactly_what_the_nested_loop_does() {
    let at = |a: usize| unsafe { ObjectRef::from_raw(a as *mut u8) };
    let brute = |spans: &[(usize, usize)], roots: &[ObjectRef]| -> usize {
        roots
            .iter()
            .map(|r| {
                let a = r.as_ptr() as usize;
                // gen r5w1/young5: a root exactly at a span's start is
                // that span's TLAB cursor, not a violation.
                spans.iter().filter(|&&(s, e)| a > s && a < e).count()
            })
            .sum()
    };
    // Unsorted, one pair overlapping, one nested inside another, one
    // empty and one inverted span; more than SKIP_SPAN_LINEAR_MAX of them.
    let spans = [
        (0x9000usize, 0x9400usize),
        (0x1000, 0x2000),
        (0x1800, 0x2800), // overlaps the previous
        (0x4000, 0x8000),
        (0x5000, 0x5100), // nested inside the previous
        (0xA000, 0xA000), // empty
        (0xC000, 0xB000), // inverted
        (0x3000, 0x3008),
        (0x10_0000, 0x10_1000),
        (0x2_0000, 0x2_0010),
    ];
    assert!(
        spans.len() > SKIP_SPAN_LINEAR_MAX,
        "must exercise the sorted arm"
    );
    let roots: Vec<ObjectRef> = (0x800usize..0x11_000).step_by(0x88).map(at).collect();
    let expected = brute(&spans, &roots);
    assert!(expected > 0, "the fixture must contain violations");
    assert_eq!(judged_stale(&spans, &roots, "test"), expected);
    // Bounds, again, on the sorted arm.
    assert_eq!(judged_stale(&spans, &[at(0x2800)], "test"), 0);
    // Each of these is inside one span and exactly at the start (cursor)
    // of the other: one violation apiece.
    assert_eq!(judged_stale(&spans, &[at(0x1800)], "test"), 1);
    assert_eq!(judged_stale(&spans, &[at(0x5000)], "test"), 1);
}

/// `HIB-DCAST-LATEPHASE.1`, mutator side — the fourth accessor family.
///
/// `compact_object_field_storage` answers `None` both for a legacy object
/// and for a genuinely compact one whose `(class_id, field_count)` no
/// longer resolves to a registered layout, and `Heap::get_field` /
/// `Heap::set_field` fell through to the uniform `index * SLOT_SIZE` stride
/// in *both* cases. On a real instance of the second state `alloc_object`
/// sized the body with `compact_object_body_size` and `num_slots()` is the
/// FIELD COUNT, so the `index < num_slots` assert does not bound that
/// stride: the read escapes the allocation and the write puts a 16-byte
/// `Value` cell past it. `g1.rs`, `gen_heap.rs` and `zgc.rs` all grew the
/// `is_compact_object` guard; this one did not.
#[test]
fn field_accessors_refuse_a_compact_object_with_no_registered_layout() {
    let heap = Heap::new();
    // No layout is registered for this class id, so the allocation is
    // LEGACY and the cells written below are real, readable cells. Setting
    // the header bit afterwards reproduces the racing state (header says
    // compact, registry cannot serve it) without a live redefinition.
    let obj = heap.alloc_object(ClassId::new(999_997), 4);
    heap.set_field(obj, 0, Value::Int(1));
    heap.set_field(obj, 1, Value::Int(2));

    let header = heap.get_header(obj);
    header.mark_word.fetch_or(
        u32::from(cratonvm_types::GC_FLAG_COMPACT) << 24,
        std::sync::atomic::Ordering::Relaxed,
    );

    // A compact instance's field count is its class layout's, and this
    // class has none: it has no fields to address at all, so neither the
    // compact nor the legacy stride can be reached (this heap's accessors
    // assert the bound; the production heaps refuse it quietly).
    assert_eq!(header.num_slots(), 0);
    assert!(cratonvm_types::compact_object_field_disp(header, 0).is_none());

    header.mark_word.fetch_and(
        !(u32::from(cratonvm_types::GC_FLAG_COMPACT) << 24),
        std::sync::atomic::Ordering::Relaxed,
    );
    assert!(
        matches!(heap.get_field(obj, 1), Value::Int(2)),
        "the dropped write must not have reached the object at all"
    );
}

#[test]
fn value_size_fits_slot() {
    assert!(
        std::mem::size_of::<Value>() <= SLOT_SIZE,
        "Value ({} bytes) exceeds SLOT_SIZE ({SLOT_SIZE} bytes)!",
        std::mem::size_of::<Value>()
    );
}

/// Serialises every test that reads `ref_element_degradation_count`
/// against every test that *increments* it.
///
/// Still sufficient after the merge into `cratonvm_types`' shared
/// per-source table: that accessor reads only the
/// `DegradationSource::ArrayElement` slot, and
/// `ref_element_word_implausible` in this file is that slot's only writer
/// in the whole tree. A `cratonvm_types` test bumping `Interpreter`, or a
/// `vm` test bumping `Jit`, is both in a different test binary AND in a
/// different slot.
///
/// The counter is process-wide and `cargo test` runs this crate's tests in
/// one process, in parallel — so a test that merely triggers a degradation
/// perturbs a concurrently-running test that measures one. Same reasoning
/// (and same mistake, already paid for once) as
/// `cratonvm_types::compact_value::degrade_counter_test_lock`. Deltas, never
/// absolute counts, and never a reset: a reset would destroy whatever window
/// a concurrent observer had already opened.
fn ref_degrade_test_lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

/// A null reference element is NOT a degradation.
///
/// `plausible_heap_pointer(0)` is `false`, so an ordinary null `Object[]`
/// element takes the same cold arm as genuine garbage. If that arm counted
/// it, [`ref_element_degradation_count`] would read in the millions on a
/// clean run and "non-zero means a live object was nulled" would be false on
/// first use — the counter would be worse than none at all.
#[test]
fn null_ref_element_is_not_counted_as_a_degradation() {
    let _guard = ref_degrade_test_lock();
    let before = ref_element_degradation_count();
    for _ in 0..1000 {
        assert_eq!(
            ref_element_word_implausible(0),
            Value::Object(None),
            "a null element must still decode to null"
        );
    }
    assert_eq!(
        ref_element_degradation_count() - before,
        0,
        "reading null elements must not move the degradation counter"
    );
}

/// The degrade stopped being silent.
///
/// Before this counter existed, `read_prim_element` handing Java a `null`
/// for a slot that held bits left no trace anywhere — no counter, no log, no
/// assert — while `cratonvm_types`' `object_degradation_count()` reported a
/// reassuring 0 because it only sees the interpreter's SoA/compact decode
/// paths, never this one.
#[test]
fn implausible_ref_element_degrades_to_null_and_is_counted() {
    let _guard = ref_degrade_test_lock();
    // The `0x8D8D..` class from commit `6a04b0e3c1`, plus an unaligned word
    // and a >47-bit word. None can be a live object pointer.
    let garbage: [u64; 3] = [0x8D8D_8D8D_8D8D_8D8D, 0x1001, 0x0001_0000_0000_0000];
    let before = ref_element_degradation_count();
    for raw in garbage {
        assert!(
            !cratonvm_types::plausible_heap_pointer(raw),
            "{raw:#018x} must fail the plausibility test for this test to mean anything"
        );
        assert_eq!(
            ref_element_word_implausible(raw),
            Value::Object(None),
            "garbage bits must still degrade to null, not fabricate an ObjectRef"
        );
    }
    assert_eq!(
        ref_element_degradation_count() - before,
        garbage.len() as u64,
        "every non-null degrade must be counted exactly once"
    );
}

/// An array-element degrade must move the SHARED total, not just this
/// path's own slot.
///
/// This is the assertion the other tests in this file cannot make. They all
/// measure deltas on `ref_element_degradation_count`, so they passed just as
/// happily when that read a `static REF_ELEMENT_DEGRADATIONS` private to
/// this file — and that arrangement was itself the bug: a triager reading
/// `cratonvm_types::compact_value::object_degradation_count()` (the name the
/// interpreter's twin publishes, and the one a crash report reaches for) saw
/// `0` while this path was nulling live elements. Re-privatising the counter
/// must fail a test, not merely go unnoticed.
///
/// Deltas on the total are asserted as a LOWER bound, deliberately. The
/// total sums all three sources and `ref_degrade_test_lock` only serialises
/// this one, so a concurrently-running test in this binary that trips a
/// `cratonvm_types` `Interpreter` decode may add to it. `>= n` still fails
/// closed for the regression this guards (a private counter moves the total
/// by 0 while the slot moves by `n`), and does not flake.
#[test]
fn an_array_element_degrade_is_visible_in_the_shared_process_wide_total() {
    use cratonvm_types::compact_value::{
        object_degradation_breakdown, object_degradation_count, DegradationSource,
    };
    let _guard = ref_degrade_test_lock();
    let garbage: [u64; 3] = [0x8D8D_8D8D_8D8D_8D8D, 0x1001, 0x0001_0000_0000_0000];
    let n = garbage.len() as u64;

    let before_slot = ref_element_degradation_count();
    let before_total = object_degradation_count();
    for raw in garbage {
        assert_eq!(ref_element_word_implausible(raw), Value::Object(None));
    }

    assert_eq!(
        ref_element_degradation_count() - before_slot,
        n,
        "the ArrayElement slot must count every non-null degrade exactly once",
    );
    assert!(
        object_degradation_count() - before_total >= n,
        "an array-element degrade was invisible to object_degradation_count() — \
         this path is back on a counter of its own",
    );
    assert_eq!(
        object_degradation_breakdown()[DegradationSource::ArrayElement.index()],
        ref_element_degradation_count(),
        "ref_element_degradation_count must BE the ArrayElement slot, not a \
         parallel tally that happens to agree",
    );
}

/// The fast path is unchanged: a plausible word still decodes verbatim and
/// costs nothing (it never reaches the cold arm, so it never touches the
/// counter).
#[test]
fn plausible_ref_element_decodes_verbatim_without_counting() {
    let _guard = ref_degrade_test_lock();
    let heap = Heap::new();
    let obj = heap.alloc_object(ClassId::new(9), 1);
    let raw: u64 = obj.as_ptr() as usize as u64;
    assert!(cratonvm_types::plausible_heap_pointer(raw));
    let before = ref_element_degradation_count();
    // SAFETY: `raw` is the address of a live object just allocated above.
    let decoded = unsafe { decode_ref_element_word(raw) };
    match decoded {
        Value::Object(Some(r)) => assert_eq!(r.as_ptr(), obj.as_ptr()),
        other => panic!("a live object pointer must decode verbatim, got {other:?}"),
    }
    assert_eq!(
        ref_element_degradation_count() - before,
        0,
        "the fast path must not touch the counter"
    );
}



#[test]
fn alloc_object_and_read_header() {
    let heap = Heap::new();
    let class_id = ClassId::new(42);
    let obj = heap.alloc_object(class_id, 3);

    let header = heap.get_header(obj);
    assert_eq!(header.class_id, class_id);
    assert_eq!(header.kind(), ObjectKind::Object);
    assert_eq!(header.num_slots(), 3);

    // The identity hash is installed LAZILY, on first request, into the
    // mark word -- not eagerly at allocation as it used to be.
    //
    // That change is not cosmetic. Minting at allocation would leave every
    // object's mark word non-zero, and `try_thin_lock` CASes from the
    // literal `MARK_NEUTRAL` -- so every `synchronized` block in the
    // program would lose that CAS and inflate a `Monitor`. Eager hashing
    // and a mark-word hash cannot coexist.
    assert_eq!(
        header.installed_identity_hash(),
        0,
        "a freshly allocated object must not carry a hash yet, or nothing can ever thin-lock"
    );
    let hash = heap.identity_hash_code(obj);
    assert_ne!(hash, 0);
    assert_eq!(heap.identity_hash_code(obj), hash, "must be stable");
}

/// `alloc_object` allocates through `alloc_zeroed`, and an all-zero slot
/// decodes as `Value::Int(0)` (the discriminant-0 variant of `Value`; see
/// `alloc_object_with_descriptors`, which exists precisely because that is
/// NOT `Object(None)`). Every field of a fresh object must read back as
/// that zero, including one carved out of arena bytes a previous object
/// has already written to.
///
/// Was vacuous: three `let _ = heap.get_field(obj, n);` and "just verify no
/// crash". Swapping `alloc_zeroed` for a non-zeroing bump in
/// `alloc_object`, so a recycled/dirty slot is handed back as-is, stayed
/// green.
#[test]
fn alloc_object_fields_zero_initialized() {
    let heap = Heap::new();
    let obj = heap.alloc_object(ClassId::new(0), 3);

    for i in 0..3 {
        assert_eq!(
            heap.get_field(obj, i),
            Value::Int(0),
            "field {i} of a fresh object must read as the zeroed slot"
        );
    }

    // Dirty this object, then allocate another: the new one's slots must
    // still be zero rather than whatever the arena last held.
    heap.set_field(obj, 0, Value::Long(-1));
    heap.set_field(obj, 1, Value::Double(1.5));
    heap.set_field(obj, 2, Value::Object(Some(obj)));
    let obj2 = heap.alloc_object(ClassId::new(0), 3);
    for i in 0..3 {
        assert_eq!(
            heap.get_field(obj2, i),
            Value::Int(0),
            "field {i} of a later object must be zeroed too"
        );
    }
}

#[test]
fn object_field_set_and_get() {
    let heap = Heap::new();
    let obj = heap.alloc_object(ClassId::new(1), 3);

    heap.set_field(obj, 0, Value::Int(42));
    heap.set_field(obj, 1, Value::Long(123_456_789));
    heap.set_field(obj, 2, Value::Float(3.125));

    assert_eq!(heap.get_field(obj, 0).as_int(), Some(42));
    assert_eq!(heap.get_field(obj, 1).as_long(), Some(123_456_789));
    assert_eq!(heap.get_field(obj, 2).as_float(), Some(3.125));
}

#[test]
fn object_field_overwrite() {
    let heap = Heap::new();
    let obj = heap.alloc_object(ClassId::new(0), 1);

    heap.set_field(obj, 0, Value::Int(1));
    assert_eq!(heap.get_field(obj, 0).as_int(), Some(1));

    heap.set_field(obj, 0, Value::Int(999));
    assert_eq!(heap.get_field(obj, 0).as_int(), Some(999));
}

#[test]
fn object_reference_field() {
    let heap = Heap::new();
    let obj1 = heap.alloc_object(ClassId::new(0), 1);
    let obj2 = heap.alloc_object(ClassId::new(1), 0);

    // Store a reference to obj2 in obj1's field
    heap.set_field(obj1, 0, Value::Object(Some(obj2)));

    match heap.get_field(obj1, 0) {
        Value::Object(Some(r)) => assert_eq!(r, obj2),
        other => panic!("Expected Object(Some(...)), got {other:?}"),
    }

    // Null reference
    heap.set_field(obj1, 0, Value::Object(None));
    assert!(heap.get_field(obj1, 0).is_null());
}

#[test]
fn alloc_array_and_read_header() {
    let heap = Heap::new();
    let class_id = ClassId::new(10);
    let arr = heap.alloc_array(class_id, ArrayElementType::Int, 5);

    let header = heap.get_header(arr);
    assert_eq!(header.class_id, class_id);
    assert_eq!(header.kind(), ObjectKind::Array);
    assert_eq!(header.element_type(), ArrayElementType::Int);
    assert_eq!(header.array_length(), 5);
    assert_eq!(heap.array_length(arr), 5);
}

/// A plain-object field access on an array receiver is refused, counted,
/// and — crucially — leaves the ELEMENTS intact.
///
/// The index test cannot catch this on its own: `alloc_array` mirrors the
/// length into `num_slots`, so index 2 of a `byte[8]` is "in bounds" while
/// the byte offset it names (`16 + 2*16 = 48`) is past the whole 8-byte
/// body. Asserting the elements afterwards is what separates "refused" from
/// "wrote somewhere harmless".
#[test]
fn a_field_access_on_an_array_receiver_is_refused_and_counted() {
    let heap = Heap::new();
    let arr = heap.alloc_array(ClassId::new(0), ArrayElementType::Byte, 8);
    for i in 0..8 {
        heap.set_array_element(arr, i, Value::Int(i as i32 + 1))
            .unwrap();
    }
    let before = crate::heap::array_receiver_field_accesses();
    // The census the Spring Boot runner reads back at exit is a SECOND
    // counter, in `types`, and a refusal that moved only the local one
    // would report `array_receiver=0` on a sweep that found producers.
    let census_before = cratonvm_types::cell_census::array_receiver();
    let decoded_before = cratonvm_types::cell_census::decoded();

    // Index 0 lands inside the body; index 2 lands outside it entirely.
    // Both are "in bounds" by `num_slots` and both must be refused.
    assert!(heap.get_field(arr, 0).is_null(), "read must answer null");
    assert!(heap.get_field(arr, 2).is_null(), "read must answer null");
    heap.set_field(arr, 0, Value::Int(0x7f7f_7f7f));
    heap.set_field(arr, 2, Value::Int(0x7f7f_7f7f));

    // `>=`, not `==`: the counter is process-global and the test binary
    // runs its tests on parallel threads.
    assert!(
        crate::heap::array_receiver_field_accesses() - before >= 4,
        "every refused access must be counted, reads and writes alike"
    );
    for i in 0..8 {
        assert_eq!(
            heap.get_array_element(arr, i).unwrap().as_int(),
            Some(i as i32 + 1),
            "element {i} was overwritten by a refused field write"
        );
    }
    assert!(
        cratonvm_types::cell_census::array_receiver() - census_before >= 4,
        "the exit census must see the refusals too, or a sweep reports zero"
    );
    assert!(
        cratonvm_types::cell_census::decoded() - decoded_before >= 4,
        "and `decoded` must move as well: that is what makes the VM-side producer reporter name the door, the receiver and the Java frames"
    );
}

/// The same rule for a REFERENCE array, which is the shape that produced
/// the original defect: it carries its COMPONENT's class id, so a
/// `class_name == "java/lang/String"` fast path matches it, and its
/// elements are 8-byte raw pointers — so slot 0 decodes elements 0 and 1
/// as a `(tag, payload)` pair.
#[test]
fn a_field_access_on_a_reference_array_is_refused_too() {
    let heap = Heap::new();
    let a = heap.alloc_object(ClassId::new(7), 1);
    let b = heap.alloc_object(ClassId::new(7), 1);
    let arr = heap.alloc_array(ClassId::new(3), ArrayElementType::Reference, 2);
    heap.set_array_element(arr, 0, Value::Object(Some(a)))
        .unwrap();
    heap.set_array_element(arr, 1, Value::Object(Some(b)))
        .unwrap();
    let before = crate::heap::array_receiver_field_accesses();

    assert!(heap.get_field(arr, 0).is_null());
    heap.set_field(arr, 0, Value::Object(None));

    assert!(crate::heap::array_receiver_field_accesses() - before >= 2);
    assert_eq!(
        heap.get_array_element(arr, 0).unwrap(),
        Value::Object(Some(a))
    );
    assert_eq!(
        heap.get_array_element(arr, 1).unwrap(),
        Value::Object(Some(b))
    );
}

#[test]
fn array_set_and_get() {
    let heap = Heap::new();
    let arr = heap.alloc_array(ClassId::new(0), ArrayElementType::Int, 3);

    heap.set_array_element(arr, 0, Value::Int(10)).unwrap();
    heap.set_array_element(arr, 1, Value::Int(20)).unwrap();
    heap.set_array_element(arr, 2, Value::Int(30)).unwrap();

    assert_eq!(heap.get_array_element(arr, 0).unwrap().as_int(), Some(10));
    assert_eq!(heap.get_array_element(arr, 1).unwrap().as_int(), Some(20));
    assert_eq!(heap.get_array_element(arr, 2).unwrap().as_int(), Some(30));
}

#[test]
fn array_bounds_check() {
    let heap = Heap::new();
    let arr = heap.alloc_array(ClassId::new(0), ArrayElementType::Int, 2);

    // In-bounds
    assert!(heap.set_array_element(arr, 0, Value::Int(1)).is_ok());
    assert!(heap.set_array_element(arr, 1, Value::Int(2)).is_ok());

    // Out of bounds
    assert_eq!(heap.set_array_element(arr, 2, Value::Int(3)), Err(2));
    assert_eq!(heap.get_array_element(arr, 5), Err(5));
}

#[test]
fn reference_array() {
    let heap = Heap::new();
    let obj = heap.alloc_object(ClassId::new(0), 0);
    let arr = heap.alloc_array(ClassId::new(1), ArrayElementType::Reference, 2);

    heap.set_array_element(arr, 0, Value::Object(Some(obj)))
        .unwrap();
    heap.set_array_element(arr, 1, Value::Object(None)).unwrap();

    match heap.get_array_element(arr, 0).unwrap() {
        Value::Object(Some(r)) => assert_eq!(r, obj),
        other => panic!("Expected Object(Some), got {other:?}"),
    }
    assert!(heap.get_array_element(arr, 1).unwrap().is_null());
}

#[test]
fn multiple_objects_independent() {
    let heap = Heap::new();
    let obj1 = heap.alloc_object(ClassId::new(0), 1);
    let obj2 = heap.alloc_object(ClassId::new(1), 1);

    heap.set_field(obj1, 0, Value::Int(111));
    heap.set_field(obj2, 0, Value::Int(222));

    // They don't interfere with each other
    assert_eq!(heap.get_field(obj1, 0).as_int(), Some(111));
    assert_eq!(heap.get_field(obj2, 0).as_int(), Some(222));
}

#[test]
fn identity_hash_codes_unique() {
    let heap = Heap::new();
    let obj1 = heap.alloc_object(ClassId::new(0), 0);
    let obj2 = heap.alloc_object(ClassId::new(0), 0);

    assert_ne!(heap.identity_hash_code(obj1), heap.identity_hash_code(obj2),);
}

#[test]
fn allocated_bytes_grows() {
    let heap = Heap::new();
    let before = heap.allocated_bytes();
    let _ = heap.alloc_object(ClassId::new(0), 100);
    assert!(heap.allocated_bytes() > before);
}

#[test]
fn zero_length_array() {
    let heap = Heap::new();
    let arr = heap.alloc_array(ClassId::new(0), ArrayElementType::Int, 0);
    assert_eq!(heap.array_length(arr), 0);
    // Out of bounds on any index
    assert!(heap.get_array_element(arr, 0).is_err());
}

// -- Tests for integer overflow protection --

#[test]
fn array_data_size_checked_small_values() {
    assert_eq!(
        array_data_size_checked(10, ArrayElementType::Int),
        Some(40) // 10 * 4 = 40, already 8-aligned
    );
    assert_eq!(
        array_data_size_checked(3, ArrayElementType::Long),
        Some(24) // 3 * 8 = 24, already 8-aligned
    );
    assert_eq!(
        array_data_size_checked(5, ArrayElementType::Byte),
        Some(8) // 5 * 1 = 5, rounded up to 8
    );
}

#[test]
fn array_data_size_checked_overflow_returns_none() {
    // usize::MAX / 8 + 1 elements * 8 bytes each would overflow
    let huge = usize::MAX / 8 + 1;
    assert_eq!(array_data_size_checked(huge, ArrayElementType::Long), None);
}

#[test]
fn array_data_size_returns_err_on_overflow() {
    let huge = usize::MAX / 8 + 1;
    assert!(array_data_size(huge, ArrayElementType::Long).is_err());
}

#[test]
#[should_panic(expected = "overflow")]
fn alloc_object_panics_on_field_count_overflow() {
    let heap = Heap::new();
    // This should trigger checked_mul overflow
    let _ = heap.alloc_object(ClassId::new(0), usize::MAX);
}

#[test]
fn array_data_size_zero_length() {
    assert_eq!(array_data_size(0, ArrayElementType::Int).unwrap(), 0);
    assert_eq!(array_data_size(0, ArrayElementType::Reference).unwrap(), 0);
}

#[test]
fn element_byte_sizes() {
    assert_eq!(element_byte_size(ArrayElementType::Boolean), 1);
    assert_eq!(element_byte_size(ArrayElementType::Byte), 1);
    assert_eq!(element_byte_size(ArrayElementType::Char), 2);
    assert_eq!(element_byte_size(ArrayElementType::Short), 2);
    assert_eq!(element_byte_size(ArrayElementType::Int), 4);
    assert_eq!(element_byte_size(ArrayElementType::Float), 4);
    assert_eq!(element_byte_size(ArrayElementType::Long), 8);
    assert_eq!(element_byte_size(ArrayElementType::Double), 8);
    assert_eq!(
        element_byte_size(ArrayElementType::Reference),
        REF_ELEMENT_SIZE
    );
}

#[test]
fn alloc_object_checked_success() {
    let heap = Heap::new();
    let obj = heap.alloc_object_checked(ClassId::new(1), 2);
    assert!(obj.is_some());
    let obj = obj.unwrap();
    let header = heap.get_header(obj);
    assert_eq!(header.class_id, ClassId::new(1));
    assert_eq!(header.num_slots(), 2);
}

#[test]
fn alloc_object_checked_overflow_returns_none() {
    let heap = Heap::new();
    // usize::MAX fields will overflow the size calculation
    assert!(heap
        .alloc_object_checked(ClassId::new(0), usize::MAX)
        .is_none());
}

#[test]
fn alloc_array_checked_success() {
    let heap = Heap::new();
    let arr = heap.alloc_array_checked(ClassId::new(1), ArrayElementType::Int, 10);
    assert!(arr.is_some());
    let arr = arr.unwrap();
    assert_eq!(heap.array_length(arr), 10);
}

#[test]
fn alloc_array_checked_overflow_returns_none() {
    let heap = Heap::new();
    assert!(heap
        .alloc_array_checked(ClassId::new(0), ArrayElementType::Int, usize::MAX)
        .is_none());
}

#[test]
fn alloc_array_checked_rejects_oversized() {
    // Arrays larger than MAX_ARRAY_LENGTH (i32::MAX) should be rejected
    let heap = Heap::with_capacity(8 * 1024);
    let result = heap.alloc_array_checked(
        ClassId::new(0),
        ArrayElementType::Int,
        (i32::MAX as usize) + 1,
    );
    assert!(result.is_none());
}

#[test]
fn alloc_array_checked_oom_returns_none() {
    // Create a tiny heap (1 KB total = 512 bytes per semi-space)
    let heap = Heap::with_capacity(1024);
    // Try to allocate an array larger than the semi-space
    let result = heap.alloc_array_checked(ClassId::new(0), ArrayElementType::Long, 1_000_000);
    assert!(result.is_none());
}

#[test]
#[should_panic(expected = "out of bounds")]
fn test_get_field_out_of_bounds_panics() {
    let heap = Heap::new();
    let obj = heap.alloc_object(ClassId::new(0), 2);
    let _ = heap.get_field(obj, 2); // index 2 is out of bounds for 2 fields
}

#[test]
#[should_panic(expected = "out of bounds")]
fn test_set_field_out_of_bounds_panics() {
    let heap = Heap::new();
    let obj = heap.alloc_object(ClassId::new(0), 2);
    heap.set_field(obj, 5, Value::Int(42));
}

#[test]
#[should_panic(expected = "not an array")]
fn test_array_length_on_non_array_panics() {
    let heap = Heap::new();
    let obj = heap.alloc_object(ClassId::new(0), 1);
    let _ = heap.array_length(obj);
}

#[test]
#[should_panic(expected = "not an array")]
fn test_get_array_element_on_non_array_panics() {
    let heap = Heap::new();
    let obj = heap.alloc_object(ClassId::new(0), 1);
    let _ = heap.get_array_element(obj, 0);
}

#[test]
fn test_try_alloc_object_returns_some() {
    let heap = Heap::new();
    let result = heap.try_alloc_object(ClassId::new(7), 3);
    assert!(result.is_some());
    let obj = result.unwrap();
    let header = heap.get_header(obj);
    assert_eq!(header.class_id, ClassId::new(7));
    assert_eq!(header.kind(), ObjectKind::Object);
    assert_eq!(header.num_slots(), 3);
}

#[test]
fn test_try_alloc_object_returns_none_on_oom() {
    let heap = Heap::with_capacity(1024); // 512 bytes per semi-space
                                          // Keep allocating until we get None
    let mut count = 0;
    loop {
        match heap.try_alloc_object(ClassId::new(0), 4) {
            Some(_) => count += 1,
            None => break,
        }
        // Safety limit to avoid infinite loop in case of bug
        assert!(count < 1000, "expected OOM but allocated {count} objects");
    }
    assert!(count > 0, "should have allocated at least one object");
}

#[test]
fn test_try_alloc_array_returns_some() {
    let heap = Heap::new();
    let result = heap.try_alloc_array(ClassId::new(5), ArrayElementType::Int, 10);
    assert!(result.is_some());
    let arr = result.unwrap();
    assert_eq!(heap.array_length(arr), 10);
    let header = heap.get_header(arr);
    assert_eq!(header.class_id, ClassId::new(5));
    assert_eq!(header.kind(), ObjectKind::Array);
    assert_eq!(header.element_type(), ArrayElementType::Int);
}

#[test]
fn test_try_alloc_array_returns_none_on_oom() {
    let heap = Heap::with_capacity(1024); // 512 bytes per semi-space
    let mut count = 0;
    loop {
        match heap.try_alloc_array(ClassId::new(0), ArrayElementType::Long, 8) {
            Some(_) => count += 1,
            None => break,
        }
        assert!(count < 1000, "expected OOM but allocated {count} arrays");
    }
    assert!(count > 0, "should have allocated at least one array");
}

#[test]
fn test_try_alloc_array_rejects_oversized() {
    let heap = Heap::new();
    let result = heap.try_alloc_array(
        ClassId::new(0),
        ArrayElementType::Int,
        (i32::MAX as usize) + 1, // MAX_ARRAY_LENGTH + 1
    );
    assert!(result.is_none());
}

#[test]
fn test_volatile_field_access() {
    let heap = Heap::new();
    let obj = heap.alloc_object(ClassId::new(0), 2);

    heap.set_field_volatile(obj, 0, Value::Int(42));
    heap.set_field_volatile(obj, 1, Value::Long(999_999));

    assert_eq!(heap.get_field_volatile(obj, 0).as_int(), Some(42));
    assert_eq!(heap.get_field_volatile(obj, 1).as_long(), Some(999_999));

    // Overwrite via volatile and read back
    heap.set_field_volatile(obj, 0, Value::Int(100));
    assert_eq!(heap.get_field_volatile(obj, 0).as_int(), Some(100));
}

// -------------------------------------------------------------------
// R1 — Primitive-field default initialization tests.
//
// Regression coverage for the ConcurrentHashMap.initTable CAS livelock
// observed on Keycloak 16/26 boot. Zero-initialized heap memory
// decodes as `Value::Object(None)` (the zero-discriminant `Value`
// variant) when read via `std::ptr::read::<Value>()`. For a primitive
// `int` field that has never been explicitly written, this causes
// `Unsafe.compareAndSetInt(obj, off, Int(0), new)` to permanently
// fail because `values_equal_for_cas(Object(None), Int(0))` returns
// `false` — the Java caller spins forever in its retry loop.
//
// The fix is `alloc_object_with_descriptors`: at allocation time we
// write the correctly-tagged zero `Value` variant into every
// primitive-typed slot based on the class's field descriptors. For
// reference slots we leave the zeroed memory alone because it
// already decodes as `Object(None)` (i.e. `null`), which is the
// spec-mandated default.
// -------------------------------------------------------------------

/// CAS semantics that mirror `vm::vm_exec::values_equal_for_cas`,
/// reproduced locally so the tests below can exercise the full
/// alloc→read→CAS→write cycle without pulling in the `vm` crate.
/// Keep in sync with that definition.
fn values_equal_for_cas_stub(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Int(x), Value::Int(y)) => x == y,
        (Value::Long(x), Value::Long(y)) => x == y,
        (Value::Float(x), Value::Float(y)) => x.to_bits() == y.to_bits(),
        (Value::Double(x), Value::Double(y)) => x.to_bits() == y.to_bits(),
        (Value::Object(None), Value::Object(None)) => true,
        (Value::Object(Some(a)), Value::Object(Some(b))) => {
            std::ptr::eq(a.as_ptr(), b.as_ptr())
        }
        _ => false,
    }
}

#[test]
fn r1_alloc_object_int_field_default_is_int_zero() {
    let heap = Heap::new();
    // Single int field — descriptor "I".
    let obj = heap.alloc_object_with_descriptors(ClassId::new(0), 1, b"I");
    // Without the fix this would read Value::Object(None).
    assert_eq!(
        heap.get_field(obj, 0),
        Value::Int(0),
        "primitive int field must default to Value::Int(0), not Object(None)"
    );
}

#[test]
fn r1_alloc_object_long_field_default_is_long_zero() {
    let heap = Heap::new();
    let obj = heap.alloc_object_with_descriptors(ClassId::new(0), 1, b"J");
    assert_eq!(
        heap.get_field(obj, 0),
        Value::Long(0),
        "primitive long field must default to Value::Long(0)"
    );
}

#[test]
fn r1_alloc_object_float_field_default_is_float_zero() {
    let heap = Heap::new();
    let obj = heap.alloc_object_with_descriptors(ClassId::new(0), 1, b"F");
    match heap.get_field(obj, 0) {
        Value::Float(f) => assert_eq!(
            f.to_bits(),
            0.0_f32.to_bits(),
            "primitive float field must default to 0.0 bits"
        ),
        other => panic!("expected Value::Float(0.0), got {other:?}"),
    }
}

#[test]
fn r1_alloc_object_double_field_default_is_double_zero() {
    let heap = Heap::new();
    let obj = heap.alloc_object_with_descriptors(ClassId::new(0), 1, b"D");
    match heap.get_field(obj, 0) {
        Value::Double(d) => assert_eq!(
            d.to_bits(),
            0.0_f64.to_bits(),
            "primitive double field must default to 0.0 bits"
        ),
        other => panic!("expected Value::Double(0.0), got {other:?}"),
    }
}

#[test]
fn r1_alloc_object_boolean_field_default_is_int_zero() {
    let heap = Heap::new();
    // Z descriptor is stored as Int per JVM spec (boolean lives in
    // Value::Int at the interpreter stack-slot level).
    let obj = heap.alloc_object_with_descriptors(ClassId::new(0), 1, b"Z");
    assert_eq!(
        heap.get_field(obj, 0),
        Value::Int(0),
        "boolean (Z) must default to Value::Int(0), not Object(None)"
    );
}

#[test]
fn r1_alloc_object_reference_field_default_is_object_none() {
    let heap = Heap::new();
    // L-descriptor: reference to java/lang/Object. Descriptor bytes
    // start with 'L' (full form is "Ljava/lang/Object;") — only the
    // first byte matters for default-init dispatch.
    let obj = heap.alloc_object_with_descriptors(ClassId::new(0), 1, b"L");
    assert!(
        heap.get_field(obj, 0).is_null(),
        "reference field must retain the Object(None) zero-bits default"
    );
    // Array descriptor '[' must also default to null.
    let arr_field = heap.alloc_object_with_descriptors(ClassId::new(0), 1, b"[");
    assert!(
        heap.get_field(arr_field, 0).is_null(),
        "array-ref field must retain the Object(None) default"
    );
}

#[test]
fn r1_unsafe_compare_and_set_int_on_uninit_slot_succeeds() {
    // This is the concrete KC16/KC26 livelock repro. Before the fix,
    // a freshly-allocated ConcurrentHashMap had its `sizeCtl` slot
    // reading back as Object(None), so CAS(Int(0), Int(N)) never
    // matched and the VM spun at ~5,740 iterations/s.
    let heap = Heap::new();
    let obj = heap.alloc_object_with_descriptors(ClassId::new(0), 1, b"I");

    // Read the slot as the Java caller's `Getfield` would.
    let current = heap.get_field(obj, 0);
    assert_eq!(
        current,
        Value::Int(0),
        "uninit int slot must read as Int(0) for CAS against Int(0)"
    );

    // Simulate Unsafe.compareAndSetInt(obj, off, expected=Int(0), new=Int(42)).
    let expected = Value::Int(0);
    let new_val = Value::Int(42);
    assert!(
        values_equal_for_cas_stub(&current, &expected),
        "CAS compare must succeed on the uninit primitive slot"
    );
    heap.set_field(obj, 0, new_val);

    // Post-CAS, the slot must hold the new value.
    assert_eq!(
        heap.get_field(obj, 0),
        Value::Int(42),
        "post-CAS int slot must read as Int(42)"
    );
}

#[test]
fn r1_unsafe_compare_and_set_long_on_uninit_slot_succeeds() {
    let heap = Heap::new();
    let obj = heap.alloc_object_with_descriptors(ClassId::new(0), 1, b"J");

    let current = heap.get_field(obj, 0);
    assert_eq!(
        current,
        Value::Long(0),
        "uninit long slot must read as Long(0)"
    );

    let expected = Value::Long(0);
    let new_val = Value::Long(0xDEAD_BEEF_CAFE_F00D_u64 as i64);
    assert!(
        values_equal_for_cas_stub(&current, &expected),
        "CAS compare must succeed on the uninit primitive long slot"
    );
    heap.set_field(obj, 0, new_val);

    assert_eq!(
        heap.get_field(obj, 0),
        new_val,
        "post-CAS long slot must read the new value"
    );
}

// --- Robustness: unknown / malformed descriptor bytes fail open. ---

#[test]
fn r1_alloc_object_unknown_descriptor_falls_back_to_object_none() {
    let heap = Heap::new();
    // 0xFF is not a valid JVM field descriptor byte — must fall back
    // to the zeroed default (Object(None)) rather than panic.
    let obj = heap.alloc_object_with_descriptors(ClassId::new(0), 1, &[0xFF]);
    assert!(
        heap.get_field(obj, 0).is_null(),
        "unknown descriptor must fail-open to Object(None)"
    );
}

#[test]
fn r1_alloc_object_short_descriptor_slice_leaves_extra_slots_zeroed() {
    // num_fields > descriptor_bytes.len() — extra slots keep the
    // zeroed default (Object(None)). This is important because
    // inherited fields may not all have their descriptors passed in
    // a single call.
    let heap = Heap::new();
    let obj = heap.alloc_object_with_descriptors(ClassId::new(0), 3, b"I");
    assert_eq!(heap.get_field(obj, 0), Value::Int(0));
    // Slots 1 and 2 never had their descriptor — they remain as the
    // zero-bits Object(None).
    assert!(heap.get_field(obj, 1).is_null());
    assert!(heap.get_field(obj, 2).is_null());
}

#[test]
fn r1_default_value_for_descriptor_mapping_is_exhaustive() {
    // Every JVM primitive descriptor byte → expected Value variant.
    assert_eq!(default_value_for_descriptor(b'I'), Some(Value::Int(0)));
    assert_eq!(default_value_for_descriptor(b'B'), Some(Value::Int(0)));
    assert_eq!(default_value_for_descriptor(b'C'), Some(Value::Int(0)));
    assert_eq!(default_value_for_descriptor(b'S'), Some(Value::Int(0)));
    assert_eq!(default_value_for_descriptor(b'Z'), Some(Value::Int(0)));
    assert_eq!(default_value_for_descriptor(b'J'), Some(Value::Long(0)));
    match default_value_for_descriptor(b'F') {
        Some(Value::Float(f)) => assert_eq!(f.to_bits(), 0.0_f32.to_bits()),
        other => panic!("F must map to Float(0.0), got {other:?}"),
    }
    match default_value_for_descriptor(b'D') {
        Some(Value::Double(d)) => assert_eq!(d.to_bits(), 0.0_f64.to_bits()),
        other => panic!("D must map to Double(0.0), got {other:?}"),
    }
    // Reference types and unknown bytes map to None → keep zero.
    assert_eq!(default_value_for_descriptor(b'L'), None);
    assert_eq!(default_value_for_descriptor(b'['), None);
    assert_eq!(default_value_for_descriptor(0x00), None);
    assert_eq!(default_value_for_descriptor(0xFF), None);
}

// ── T10.9.E descriptor-aware heap-field tests ────────────────────────

#[test]
fn t10_9_e_coerce_value_j_from_double_is_long() {
    // Exact Session 93 drift: a long whose bit pattern round-tripped
    // through CompactValue::to_value landed as Value::Double. The
    // helper reinterprets and returns Value::Long.
    let v_double = Value::Double(f64::from_bits(5));
    let coerced = coerce_field_value_by_descriptor(v_double, b'J');
    assert_eq!(coerced, Value::Long(5));
}

#[test]
fn t10_9_e_coerce_value_j_preserves_existing_long() {
    let v = Value::Long(0xDEAD_BEEF_CAFE_BABEu64 as i64);
    let coerced = coerce_field_value_by_descriptor(v, b'J');
    assert_eq!(coerced, Value::Long(0xDEAD_BEEF_CAFE_BABEu64 as i64));
}

#[test]
fn t10_9_e_coerce_value_j_widens_int() {
    let coerced = coerce_field_value_by_descriptor(Value::Int(-7), b'J');
    assert_eq!(coerced, Value::Long(-7));
}

#[test]
fn t10_9_e_coerce_value_j_from_null_is_zero() {
    let coerced = coerce_field_value_by_descriptor(Value::Object(None), b'J');
    assert_eq!(coerced, Value::Long(0));
}

#[test]
fn t10_9_e_coerce_value_d_from_long_is_double() {
    let v_long = Value::Long(f64::to_bits(std::f64::consts::PI) as i64);
    let coerced = coerce_field_value_by_descriptor(v_long, b'D');
    match coerced {
        // Bit equality, not a tolerance. This asserts a REINTERPRETATION:
        // the whole claim is that the 64 bits survive, and a tolerance of
        // 1e-12 admits ~5,100 ulps of drift in a value that cannot legally
        // drift at all — it would pass a slot that silently narrowed the
        // double to f32 and back.
        Value::Double(d) => assert_eq!(d.to_bits(), std::f64::consts::PI.to_bits()),
        other => panic!("expected Double, got {other:?}"),
    }
}

#[test]
fn t10_9_e_coerce_value_f_from_int_reinterprets() {
    let f_bits = 1.5_f32.to_bits();
    let coerced = coerce_field_value_by_descriptor(Value::Int(f_bits as i32), b'F');
    match coerced {
        Value::Float(f) => assert_eq!(f.to_bits(), f_bits),
        other => panic!("expected Float, got {other:?}"),
    }
}

#[test]
fn t10_9_e_coerce_value_reference_rejects_nonzero_primitive_unchanged() {
    // A reference descriptor leaves most primitives as-is (they will
    // have been rejected by the verifier); only zero-valued primitives
    // normalize to null.
    let coerced = coerce_field_value_by_descriptor(Value::Int(0), b'L');
    assert!(matches!(coerced, Value::Object(None)));
    let coerced = coerce_field_value_by_descriptor(Value::Long(0), b'L');
    assert!(matches!(coerced, Value::Object(None)));
}

#[test]
fn t10_9_e_coerce_value_unknown_desc_preserves_value() {
    let coerced = coerce_field_value_by_descriptor(Value::Int(99), b'V');
    assert_eq!(coerced, Value::Int(99));
}

#[test]
fn t10_9_e_heap_get_field_as_normalizes_j() {
    // Allocate an object and directly write a Value::Double that
    // represents a long bit pattern — simulating upstream drift.
    // `get_field_as(b'J')` must surface Value::Long.
    let heap = Heap::new();
    let obj = heap.alloc_object(ClassId::new(1), 1);
    heap.set_field(obj, 0, Value::Double(f64::from_bits(12345)));
    match heap.get_field_as(obj, 0, b'J') {
        Value::Long(l) => assert_eq!(l, 12345),
        other => panic!("expected Long(12345), got {other:?}"),
    }
}

#[test]
fn t10_9_e_heap_set_field_as_normalizes_on_write() {
    let heap = Heap::new();
    let obj = heap.alloc_object(ClassId::new(1), 1);
    // Write a Double through the descriptor-aware setter with J descriptor.
    heap.set_field_as(obj, 0, Value::Double(f64::from_bits(42)), b'J');
    // Direct read should already yield Long (normalized on write).
    match heap.get_field(obj, 0) {
        Value::Long(l) => assert_eq!(l, 42),
        other => panic!("expected Long(42), got {other:?}"),
    }
}

#[test]
fn t10_9_e_heap_get_field_volatile_as_normalizes_j() {
    let heap = Heap::new();
    let obj = heap.alloc_object(ClassId::new(1), 1);
    heap.set_field_volatile(obj, 0, Value::Double(f64::from_bits(999)));
    match heap.get_field_volatile_as(obj, 0, b'J') {
        Value::Long(l) => assert_eq!(l, 999),
        other => panic!("expected Long(999), got {other:?}"),
    }
}

#[test]
fn t10_9_e_heap_get_field_as_reference_passthrough() {
    let heap = Heap::new();
    let obj = heap.alloc_object(ClassId::new(1), 1);
    let other = heap.alloc_object(ClassId::new(2), 0);
    heap.set_field(obj, 0, Value::Object(Some(other)));
    match heap.get_field_as(obj, 0, b'L') {
        Value::Object(Some(o)) => assert_eq!(o.as_ptr(), other.as_ptr()),
        other => panic!("expected Object(Some), got {other:?}"),
    }
}

// ----- G30: the descriptor-coercion loss instrument ---------------------
//
// These pin two separate things, and the second matters more than the
// first: (a) that the instrument exists and classifies correctly, and
// (b) that it changed NO ANSWER. 400 measured sites in this tree write a
// primitive at a reference slot; `java.util.HashMap` cannot survive that
// store being refused, and `RJdkHello` cannot survive it being boxed.
//
// Counter isolation: the counters are process-global and `cargo test`
// runs this module in parallel, so every test that reads them takes
// `G30_COUNTERS` first. The pre-G30 `t10_9_e_*` tests deliberately do NOT
// need the lock — they call `coerce_field_value_by_descriptor`, which
// lands in the `Unattributed` COLUMN, while every assertion here is on
// the `Read`/`Store` columns.

static G30_COUNTERS: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn g30_lock() -> std::sync::MutexGuard<'static, ()> {
    // A panicking test must not poison the rest of the suite into
    // failing for an unrelated reason.
    G30_COUNTERS.lock().unwrap_or_else(|e| e.into_inner())
}

fn g30_count(loss: FieldCoercionLoss, kind: FieldAccessKind) -> u64 {
    field_coercion_loss_counts()[loss.index()][kind.index()]
}

/// The census's name tables are a COPY of these two enums' `name()`, and
/// the copy is what the one line a default run prints is labelled with.
///
/// `cratonvm-gc` depends on `cratonvm-types` and not the reverse, so the
/// summary printed from the `System.exit` trailer cannot ask the enums for
/// their own names — it has to carry duplicates. Drift would fail nothing
/// at runtime; it would mislabel a species in the only line the guard now
/// emits, which is the hardest kind of wrong to notice, and the census's
/// module comment cites this test as the reason duplicating the names is
/// acceptable at all. Pinned here, on the side that owns the enums.
#[test]
fn the_census_tables_match_the_coercion_enums() {
    use cratonvm_types::compact_value::coercion_census as census;

    for (loss, expected) in [
        (FieldCoercionLoss::PrimitiveIntoReference, 0usize),
        (FieldCoercionLoss::PrimitiveIntoReferenceUncoerced, 1usize),
        (FieldCoercionLoss::NullIntoPrimitive, 2usize),
        (FieldCoercionLoss::PointerIntoPrimitive, 3usize),
    ] {
        assert_eq!(loss.index(), expected, "{} moved row", loss.name());
        assert_eq!(
            census::SPECIES_NAMES[expected],
            loss.name(),
            "coercion_census::SPECIES_NAMES row {expected} no longer names the species the exit summary will attribute its count to",
        );
    }

    for (kind, expected) in [
        (FieldAccessKind::Read, 0usize),
        (FieldAccessKind::Store, 1usize),
        (FieldAccessKind::Unattributed, 2usize),
    ] {
        assert_eq!(kind.index(), expected, "{} moved column", kind.name());
        assert_eq!(
            census::KIND_NAMES[expected],
            kind.name(),
            "coercion_census::KIND_NAMES column {expected} no longer names the access direction the exit summary will report",
        );
    }

    assert_eq!(census::SPECIES_NAMES.len(), FieldCoercionLoss::COUNT);
    assert_eq!(census::KIND_NAMES.len(), FieldAccessKind::COUNT);
}

/// THE PIN. `java.util.HashMap.table` is declared
/// `[Ljava/util/HashMap$Node;`, and CratonVM's synthetic init paths write
/// `Value::Int(capacity)` there. The `b'L' | b'['` arm turning that into
/// `null` is what lets `HashMap.resize()`'s
/// `(oldTab == null) ? 0 : oldTab.length` handle the never-initialised
/// case; without it the JDK's own `arraylength` aborts with
/// "expected object reference, got int(N)". This is rule `S111r29` and it
/// is LOAD-BEARING.
///
/// The obvious reading of the G30 defect — "stop nulling, refuse instead"
/// — breaks exactly this, which is why the instrument counts and does not
/// intervene. If a later lane deletes the degrade, this test is the thing
/// that says so.
#[test]
fn the_hashmap_table_degrade_to_null_is_pinned() {
    let _g = g30_lock();
    let heap = Heap::new();
    // Slot 2 stands for `HashMap.table`; `[` is its real descriptor byte.
    let map = heap.alloc_object(ClassId::new(7), 3);

    for capacity in [Value::Int(16), Value::Int(1), Value::Long(64)] {
        heap.set_field_as(map, 2, capacity, b'[');
        assert_eq!(
            heap.get_field(map, 2),
            Value::Object(None),
            "a capacity written at HashMap.table must degrade to null so \
             `(oldTab == null) ? 0 : oldTab.length` takes the null branch; \
             refusing or boxing the store breaks HashMap.resize()",
        );
        assert_eq!(
            heap.get_field_as(map, 2, b'['),
            Value::Object(None),
            "and it must still read back as null through the \
             descriptor-aware getter",
        );
    }

    // A genuine table array is untouched — the degrade must not fire on
    // the case it exists to make possible.
    let table = heap.alloc_object(ClassId::new(8), 0);
    heap.set_field_as(map, 2, Value::Object(Some(table)), b'[');
    match heap.get_field_as(map, 2, b'[') {
        Value::Object(Some(o)) => assert_eq!(o.as_ptr(), table.as_ptr()),
        other => panic!("a real table array must survive, got {other:?}"),
    }
}

/// The whole point of G30: the instrument observes, it does not repair.
/// Every answer below is the answer this function gave BEFORE the
/// instrument existed. If any of these move, 400 call sites move with
/// them.
#[test]
fn the_g30_instrument_changes_no_answer() {
    let _g = g30_lock();
    let heap = Heap::new();
    let obj = heap.alloc_object(ClassId::new(1), 1);
    let ptr = obj.as_ptr() as usize;
    let site = FieldCoercionSite::store(None, 0);
    let c = |v: Value, d: u8| coerce_field_value_for_slot(v, d, site);

    // Lossless normalisation — untouched arms, listed so a refactor that
    // "tidies" them fails here.
    assert_eq!(c(Value::Int(-7), b'J'), Value::Long(-7));
    assert_eq!(c(Value::Double(f64::from_bits(42)), b'J'), Value::Long(42));
    assert_eq!(c(Value::Long(5), b'I'), Value::Int(5));

    // `Uninitialized` is the allocator's tag, not a claim about the slot:
    // still the typed zero, and (see the next test) still silent.
    assert_eq!(c(Value::Uninitialized, b'J'), Value::Long(0));
    assert_eq!(c(Value::Uninitialized, b'I'), Value::Int(0));
    assert_eq!(c(Value::Uninitialized, b'L'), Value::Uninitialized);

    // null -> primitive: the typed zero. G25-1 §1: this is the arm that
    // turned an `Object(None)` aimed at a model slot into
    // `java.net.ServerSocket.closed = false`.
    assert_eq!(c(Value::Object(None), b'J'), Value::Long(0));
    assert_eq!(c(Value::Object(None), b'I'), Value::Int(0));
    assert_eq!(c(Value::Object(None), b'Z'), Value::Int(0));
    assert_eq!(c(Value::Object(None), b'F'), Value::Float(0.0));
    assert_eq!(c(Value::Object(None), b'D'), Value::Double(0.0));

    // pointer -> primitive: the address, as a number.
    assert_eq!(c(Value::Object(Some(obj)), b'J'), Value::Long(ptr as i64));
    assert_eq!(c(Value::Object(Some(obj)), b'I'), Value::Int(ptr as i32));

    // primitive -> reference: null, for Int/Long/Double...
    assert_eq!(c(Value::Int(99), b'L'), Value::Object(None));
    assert_eq!(c(Value::Long(99), b'['), Value::Object(None));
    assert_eq!(c(Value::Double(1.5), b'L'), Value::Object(None));
    // ...but NOT for Float, which is stored as-is. That asymmetry is
    // pre-existing and disclosed, not introduced here.
    assert_eq!(c(Value::Float(1.5), b'L'), Value::Float(1.5));

    // Unknown descriptor still fails open.
    assert_eq!(c(Value::Int(99), b'V'), Value::Int(99));

    // And the descriptor-only entry point agrees with the sited one.
    for (v, d) in [
        (Value::Int(3), b'L'),
        (Value::Object(None), b'I'),
        (Value::Float(1.5), b'L'),
        (Value::Uninitialized, b'J'),
    ] {
        assert_eq!(
            coerce_field_value_by_descriptor(v, d),
            coerce_field_value_for_slot(v, d, site),
            "provenance must not change the answer for {v:?} at {}",
            d as char,
        );
    }
}

/// Each destroyed value lands in its own (species, direction) cell.
///
/// Direction is the part that earns its keep: MEASURED on the pre-G30
/// binary, 336 of the 352 cross-type accesses in one `RJdkNet` run are
/// READS of `java.lang.ref.ReferenceQueue.head` on a slot that was never
/// descriptor-initialised — benign, because the read answers `null`,
/// which is what the field means. Counting those in the same bucket as a
/// store is how the 16 real stores become invisible.
#[test]
fn each_destroyed_value_is_counted_under_its_own_species_and_direction() {
    let _g = g30_lock();
    let heap = Heap::new();
    let obj = heap.alloc_object(ClassId::new(1), 2);
    let store = FieldCoercionSite::store(Some(ClassId::new(1)), 0);
    let read = FieldCoercionSite::read(Some(ClassId::new(1)), 0);

    let before = field_coercion_loss_counts();
    let total_before = field_coercion_loss_total();

    coerce_field_value_for_slot(Value::Int(42), b'L', store);
    coerce_field_value_for_slot(Value::Double(1.0), b'[', store);
    coerce_field_value_for_slot(Value::Float(1.0), b'L', store);
    coerce_field_value_for_slot(Value::Object(None), b'I', store);
    coerce_field_value_for_slot(Value::Object(Some(obj)), b'J', store);
    coerce_field_value_for_slot(Value::Int(42), b'L', read);

    let after = field_coercion_loss_counts();
    let d = |l: FieldCoercionLoss, k: FieldAccessKind| {
        after[l.index()][k.index()] - before[l.index()][k.index()]
    };
    // Fully qualified rather than glob-imported: `Read` and `Store` are
    // names a future `use` in this module could easily shadow, and the
    // failure mode would be a type error a long way from here.
    assert_eq!(
        d(
            FieldCoercionLoss::PrimitiveIntoReference,
            FieldAccessKind::Store
        ),
        2,
        "Int and Double",
    );
    assert_eq!(
        d(
            FieldCoercionLoss::PrimitiveIntoReference,
            FieldAccessKind::Read
        ),
        1,
    );
    assert_eq!(
        d(
            FieldCoercionLoss::PrimitiveIntoReferenceUncoerced,
            FieldAccessKind::Store
        ),
        1,
        "Float",
    );
    assert_eq!(
        d(FieldCoercionLoss::NullIntoPrimitive, FieldAccessKind::Store),
        1,
    );
    assert_eq!(
        d(
            FieldCoercionLoss::PointerIntoPrimitive,
            FieldAccessKind::Store
        ),
        1,
    );
    assert_eq!(field_coercion_loss_total() - total_before, 6);
}

/// The zero-init contract must stay silent.
///
/// `alloc_object_with_descriptors` leaves slots tagged `Uninitialized`,
/// and normalising those to the typed zero is the FIX from R1, not a
/// defect. If they were counted, every allocation would look like a
/// violation and the instrument would be worthless.
#[test]
fn an_uninitialized_slot_is_not_reported_as_a_loss() {
    let _g = g30_lock();
    let site = FieldCoercionSite::store(None, 0);
    let before = field_coercion_loss_total();
    for d in *b"JDFIBCSZL[" {
        coerce_field_value_for_slot(Value::Uninitialized, d, site);
    }
    assert_eq!(
        field_coercion_loss_total(),
        before,
        "Uninitialized is the allocator's 'no value yet' tag, not a \
         writer's claim about the slot — it must never be counted",
    );
}

/// The report renders only what fired, and names the species it renders.
#[test]
fn the_loss_report_names_every_species_that_fired() {
    let _g = g30_lock();
    let site = FieldCoercionSite::store(None, 0);
    coerce_field_value_for_slot(Value::Int(1), b'L', site);
    let report =
        field_coercion_loss_report().expect("a loss has fired, so the report must not be None");
    assert!(
        report.contains("primitive-into-reference"),
        "report should name the species that fired: {report}",
    );
    assert!(
        report.contains("store="),
        "report should break the count down by access direction: {report}",
    );
}

/// The heap's own descriptor-aware setter reports as a STORE and carries
/// the class and slot. This is the shape NOMINATION 1 asks the three
/// live collectors to adopt; pinning it here means the nomination can be
/// applied mechanically.
#[test]
fn the_descriptor_aware_setter_reports_a_store_with_provenance() {
    let _g = g30_lock();
    let heap = Heap::new();
    let obj = heap.alloc_object(ClassId::new(3), 1);
    let before = g30_count(
        FieldCoercionLoss::PrimitiveIntoReference,
        FieldAccessKind::Store,
    );
    heap.set_field_as(obj, 0, Value::Int(1234), b'L');
    assert_eq!(
        g30_count(
            FieldCoercionLoss::PrimitiveIntoReference,
            FieldAccessKind::Store,
        ),
        before + 1,
        "Heap::set_field_as must attribute its loss to the STORE column",
    );
    assert_eq!(heap.get_field(obj, 0), Value::Object(None));
}

// ----- G52: the Double arm, and the theorem clone rests on -------------

/// Bit-exact `Value` comparison. `PartialEq` on `f32`/`f64` says a NaN is
/// not itself, and several arms below legitimately produce one (a long
/// bit pattern reinterpreted as a float usually IS a NaN), so a plain
/// `assert_eq!` would fail on values that are in fact identical.
fn same(a: Value, b: Value) -> bool {
    match (a, b) {
        (Value::Float(x), Value::Float(y)) => x.to_bits() == y.to_bits(),
        (Value::Double(x), Value::Double(y)) => x.to_bits() == y.to_bits(),
        _ => a == b,
    }
}

/// THE `b'I'` DOUBLE ARM, PINNED. `G43-1` NOMINATION 5 asked for
/// `d as i32` in place of `d.to_bits() as i32`. This test is why that
/// must not happen.
///
/// `Value::Long(l)` and `Value::Double(f64::from_bits(l as u64))` are the
/// SAME untagged compact slot read two ways —
/// `CompactValue::to_value()` surfaces an untagged slot as a `Double`,
/// which is the whole reason the `b'J'` arm's Session-93 repair exists.
/// They must therefore agree about the slot's low 32 bits. The bit
/// projection makes them agree for every `l`; the numeric narrowing would
/// make `CompactValue::long(5)` read back as `0` at an `int` field.
#[test]
fn a_double_at_an_integral_slot_decodes_like_the_untagged_long_it_usually_is() {
    let _g = g30_lock();
    let site = FieldCoercionSite::store(None, 0);
    let c = |v: Value, d: u8| coerce_field_value_for_slot(v, d, site);

    // NaN payloads: every pattern below is either not a NaN at all or is
    // a QUIET NaN (mantissa MSB set), which `f64::from_bits`/`to_bits`
    // round-trip. Signalling patterns are deliberately not used.
    for l in [0i64, 1, 5, -1, 0x0123_4567_89AB_CDEF, i64::MIN] {
        let as_double = Value::Double(f64::from_bits(l as u64));
        for d in *b"IBCSZ" {
            assert!(
                same(c(Value::Long(l), d), c(as_double, d)),
                "a long and the untagged slot it decodes from must agree \
                 at descriptor {}: long {l} gave {:?}, double gave {:?}",
                d as char,
                c(Value::Long(l), d),
                c(as_double, d),
            );
        }
        // ...and the same slot at `J`, which is where the rule the `I`
        // arm mirrors is already documented.
        assert!(same(c(as_double, b'J'), Value::Long(l)));
    }

    // `G43-1` §5.2's arithmetic, spelled out so the record and the code
    // cannot drift apart. A synthetic `Provider` version landing on the
    // real `java.util.Hashtable.count`:
    //   25.0 -> 0x4039_0000_0000_0000, low half 0 -> count == 0, and
    //           `Hashtable.getEnumeration` early-returns. LATENT.
    //   1.8  -> 0x3FFC_CCCC_CCCC_CCCD, low half 0xCCCC_CCCD.  LIVE.
    // Under `d as i32` the first line would read `count == 25` and walk
    // 25 buckets of a table holding a `String` — i.e. the proposed repair
    // makes that record's own case WORSE, not safer.
    assert_eq!(c(Value::Double(25.0), b'I'), Value::Int(0));
    assert_eq!(c(Value::Double(1.8), b'I'), Value::Int(-858_993_459));
    assert_ne!(
        c(Value::Double(25.0), b'I'),
        Value::Int(25),
        "this arm is a bit projection, not a numeric narrowing; see the \
         comment at the arm before changing it",
    );
}

/// The `Double`/`Long` pair agrees at every integral descriptor and at
/// `J`/`D` — and disagrees at exactly one place, `b'F'`, because that
/// arm's `Long` case is a bit decode and its `Double` case is a numeric
/// one. Pinned so the known asymmetry cannot drift in silence, and so
/// whoever decides it has a test to change rather than a surprise.
#[test]
fn the_double_and_long_arms_disagree_only_at_a_float_slot() {
    let _g = g30_lock();
    let site = FieldCoercionSite::store(None, 0);
    let c = |v: Value, d: u8| coerce_field_value_for_slot(v, d, site);
    let l = 5i64;
    let as_double = Value::Double(f64::from_bits(l as u64));

    for d in *b"IJD" {
        assert!(
            same(c(Value::Long(l), d), c(as_double, d)),
            "the pair must agree at {}",
            d as char,
        );
    }
    assert!(
        !same(c(Value::Long(l), b'F'), c(as_double, b'F')),
        "b'F' is the one cell where the pair disagrees; if this now \
         passes, someone unified them — good, but update the comment at \
         the arm and the G52-1 record",
    );
    assert!(same(
        c(Value::Long(l), b'F'),
        Value::Float(f32::from_bits(5))
    ));
    assert!(same(c(as_double, b'F'), Value::Float(0.0)));
}

/// IDEMPOTENCE, and it is not an academic property.
///
/// `native_object_clone` (`native-builtins/src/lib.rs`) copies a field
/// with `ctx.get_field` then `ctx.set_field`, and BOTH of those resolve
/// the slot's descriptor and land here (`vm_exec.rs`'s
/// `NativeContextImpl`). So every cloned field is coerced TWICE, and the
/// clone's contents equal the original's coerced contents only if this
/// function is idempotent. Nothing asserted that before G52.
///
/// It also bounds any future caller that composes two coercions — a
/// read-modify-write through the descriptor-aware pair, a CAS, a
/// re-decode after a slot move.
#[test]
fn the_descriptor_coercion_is_idempotent() {
    let _g = g30_lock();
    let heap = Heap::new();
    let obj = heap.alloc_object(ClassId::new(1), 1);
    let site = FieldCoercionSite::store(None, 0);

    let values = [
        Value::Int(7),
        Value::Long(-3),
        Value::Float(1.5),
        Value::Double(2.5),
        Value::Double(f64::from_bits(5)),
        Value::Object(None),
        Value::Object(Some(obj)),
        Value::Uninitialized,
        Value::ReturnAddress(9),
    ];
    for v in values {
        for d in *b"JDFIBCSZL[V" {
            let once = coerce_field_value_for_slot(v, d, site);
            let twice = coerce_field_value_for_slot(once, d, site);
            assert!(
                same(once, twice),
                "coercing {v:?} at {} twice must equal coercing it once, \
                 got {once:?} then {twice:?} — Object.clone() copies \
                 every field through two of these",
                d as char,
            );
        }
    }
}

// ----- Part F: GPU/GC coordination tests --------------------------------
//
// The whole block is doubly-gated (#[cfg(test)] from the surrounding
// module + #[cfg(feature = "gpu-offload")]) so that the default
// `cargo test -p cratonvm-gc` invocation does not even compile this code.

#[cfg(feature = "gpu-offload")]
mod gpu_offload_tests {
    use super::*;
    use crate::collector::MonitorCleanup;
    use std::collections::HashMap;

    /// MonitorCleanup stub for tests — matches the pattern in
    /// `tests/phase_h_integration.rs` and `gen_heap.rs::NoOpMonitors`.
    struct NoMonitors;
    impl MonitorCleanup for NoMonitors {
        fn remap_after_gc(&self, _pointer_map: &cratonvm_types::PointerMap) {}
    }

    /// Test-only `StopTheWorldToken`. These tests are mostly single-
    /// threaded; the worker-thread test in
    /// `safepoint_check_skips_gc_while_token_held` deliberately races a
    /// GC against a pin/unpin sequence and the harness itself ensures
    /// no other mutator is touching the heap concurrently.
    #[inline]
    fn stw() -> crate::collector::StopTheWorldToken {
        // SAFETY: these unit tests run the heap single-threaded.
        unsafe { crate::collector::StopTheWorldToken::new() }
    }

    #[test]
    fn enter_gpu_critical_increments_counter() {
        let heap = Heap::new();
        assert_eq!(heap.gpu_critical_count(), 0, "fresh heap has zero tokens");

        let _t = heap.enter_gpu_critical();
        assert_eq!(
            heap.gpu_critical_count(),
            1,
            "one token alive after enter_gpu_critical"
        );
    }

    #[test]
    fn drop_token_decrements_counter() {
        let heap = Heap::new();
        {
            let _t = heap.enter_gpu_critical();
            assert_eq!(heap.gpu_critical_count(), 1);
        }
        assert_eq!(
            heap.gpu_critical_count(),
            0,
            "counter returns to zero on token drop"
        );
    }

    #[test]
    fn nested_tokens_count_independently() {
        let heap = Heap::new();
        let t1 = heap.enter_gpu_critical();
        let t2 = heap.enter_gpu_critical();
        assert_eq!(heap.gpu_critical_count(), 2, "two tokens alive");

        drop(t1);
        assert_eq!(
            heap.gpu_critical_count(),
            1,
            "dropping one token leaves one alive"
        );

        drop(t2);
        assert_eq!(heap.gpu_critical_count(), 0, "all tokens released");
    }

    #[test]
    fn safepoint_check_skips_gc_while_token_held() {
        // Observable side-effect: gpu_blocked_gc_count increments each
        // time collect_garbage is called while a token is alive.
        //
        // We do the GC call on a worker thread so we can drop the token
        // from the main thread and join cleanly. The worker thread first
        // races to bump gpu_blocked_gc_count, then proceeds with a
        // (now no-op) collection.
        use std::sync::Arc;
        use std::thread;

        let heap = Arc::new(Heap::new());
        // Allocate an unrooted object so collect_garbage has something
        // (or nothing — that's fine; we only care about the gate).
        let _scratch = heap.alloc_object(ClassId::new(0), 1);

        let token = heap.enter_gpu_critical();
        assert_eq!(heap.gpu_blocked_gc_count(), 0);

        let heap_clone = Arc::clone(&heap);
        let gc_thread = thread::spawn(move || {
            let mut roots: Vec<ObjectRef> = Vec::new();
            heap_clone.collect_garbage(&stw(), &mut roots, &NoMonitors);
        });

        // Spin until the worker has noticed the token. The blocked-count
        // increment happens before the worker enters the yield loop, so
        // observing it tells us the gate fired. A 5s ceiling matches
        // GPU_CRITICAL_DEADLINE_SECS and is plenty for a wakeup.
        let start = std::time::Instant::now();
        while heap.gpu_blocked_gc_count() == 0 {
            if start.elapsed() > std::time::Duration::from_secs(5) {
                panic!("GC worker never observed the token");
            }
            std::thread::yield_now();
        }
        assert!(
            heap.gpu_blocked_gc_count() >= 1,
            "GC saw the token and stepped aside",
        );

        // Release the token so the worker can finish.
        drop(token);
        gc_thread.join().expect("GC thread joined cleanly");
        assert_eq!(heap.gpu_critical_count(), 0);
    }

    #[test]
    fn pinned_ref_survives_gc_with_real_heap_alloc() {
        // Sequence:
        // 1. Allocate a real array via the Heap API — no synthesised
        //    ObjectRefs.
        // 2. Pin it.
        // 3. Run a GC with an empty `roots` slice. Without the pin,
        //    the array would be collected (the snapshot we kept would
        //    become a dangling-ish address). With the pin, the array
        //    is walked as a root and survives.
        // 4. After the GC, the (updated) pinned ref still has the
        //    correct header.
        let heap = Heap::new();
        let arr = heap.alloc_array(ClassId::new(7), ArrayElementType::Int, 4);
        heap.set_array_element(arr, 0, Value::Int(11)).unwrap();
        heap.set_array_element(arr, 3, Value::Int(44)).unwrap();

        heap.pin_ref(arr);
        assert_eq!(
            heap.gpu_pinned_refs_snapshot().len(),
            1,
            "exactly one pinned ref before GC"
        );

        // Empty caller-roots: only the pin keeps `arr` alive.
        let mut roots: Vec<ObjectRef> = Vec::new();
        heap.collect_garbage(&stw(), &mut roots, &NoMonitors);

        // After GC the address may have moved. The pinned-refs set
        // contains the post-GC address.
        let pinned_after: Vec<ObjectRef> = heap.gpu_pinned_refs_snapshot();
        assert_eq!(pinned_after.len(), 1, "pin preserved across GC");
        let arr_new = pinned_after[0];

        // The header decodes correctly via the new address.
        let h = heap.get_header(arr_new);
        assert_eq!(h.kind(), ObjectKind::Array);
        assert_eq!(h.array_length(), 4);
        assert_eq!(h.class_id, ClassId::new(7));
        // And the element values survived.
        assert_eq!(
            heap.get_array_element(arr_new, 0).unwrap().as_int(),
            Some(11),
        );
        assert_eq!(
            heap.get_array_element(arr_new, 3).unwrap().as_int(),
            Some(44),
        );

        heap.unpin_ref(arr_new);
        assert_eq!(
            heap.gpu_pinned_refs_snapshot().len(),
            0,
            "unpin_ref removes the entry",
        );
    }

    #[test]
    fn root_walker_includes_pinned_refs() {
        // Variant of the test above that asserts the GC's pointer_map
        // contains an entry for the pinned ref — i.e. that the root
        // walker actually visited it. With the feature off this code
        // doesn't compile, so we can rely on the gated combined-roots
        // path running.
        let heap = Heap::new();
        let arr = heap.alloc_array(ClassId::new(3), ArrayElementType::Int, 2);
        let old_addr = arr.as_ptr() as usize;

        heap.pin_ref(arr);

        let mut roots: Vec<ObjectRef> = Vec::new();
        let result = heap.collect_garbage(&stw(), &mut roots, &NoMonitors);

        assert!(
            result.pointer_map.contains_key(&old_addr),
            "pinned ref must appear in the GC pointer map (root walker saw it)",
        );

        heap.unpin_ref(arr);
    }
}
