use super::*;
use std::sync::atomic::Ordering;

fn header(kind: ObjectKind, elem: ArrayElementType, len: u32, slots: u32) -> ObjectHeader {
    ObjectHeader::new(ClassId::new(7), kind, elem, len, slots)
}

fn compact_header(class_id: u32) -> ObjectHeader {
    let mut h = ObjectHeader::new(
        ClassId::new(class_id),
        ObjectKind::Object,
        ArrayElementType::Reference,
        0,
        3,
    );
    h.set_compact_shape(3, 24);
    h
}

/// A 16-byte, 8-aligned scratch "object" to exercise the second word.
#[repr(C, align(8))]
struct Obj(std::cell::UnsafeCell<[u8; 32]>);

impl Obj {
    fn ptr(&self) -> *mut u8 {
        self.0.get().cast()
    }

    fn bytes(&self) -> [u8; 32] {
        // SAFETY: tests read bytes only when no concurrent writer exists.
        unsafe { *self.0.get() }
    }
}

fn materialise(h: &ObjectHeader) -> Box<Obj> {
    let o = Box::new(Obj(std::cell::UnsafeCell::new([0; 32])));
    unsafe { h.write_to(o.ptr()) };
    o
}

fn view(o: &Obj) -> &ObjectHeader {
    unsafe { &*(o.ptr() as *const ObjectHeader) }
}

// -- Layout ----------------------------------------------------------------

#[test]
fn header_sizes_and_offsets() {
    assert_eq!(HEADER_SIZE, 16);
    assert_eq!(COMPACT_HEADER_SIZE, 8);
    assert_eq!(MIN_OBJECT_SIZE, 16);
    assert_eq!(MARK_WORD_OFFSET, 4);
    assert_eq!(ARRAY_LENGTH_OFFSET, 8);
    assert_eq!(NUM_SLOTS_OFFSET, 8);
    assert_eq!(IDENTITY_HASH_OFFSET, 12);
    assert_eq!(FORWARDING_TARGET_OFFSET, 8);
    assert_eq!(KIND_TAGS_BYTE_OFFSET, 6);
    assert_eq!(GC_FLAGS_BYTE_OFFSET, 7);
    assert_eq!(ARRAY_DATA_OFFSET, 16);
    assert_eq!(std::mem::size_of::<ObjectHeader>(), 16);
    assert_eq!(std::mem::offset_of!(ObjectHeader, class_id), 0);
}

#[test]
fn slot_size_is_16() {
    assert_eq!(SLOT_SIZE, 16);
}

#[test]
fn ref_element_size_is_8() {
    assert_eq!(REF_ELEMENT_SIZE, 8);
}

/// The header word is `class_id | mark << 32`, which is what the JIT's
/// inline allocators store in one qword and what `CMP DWORD [recv+0]`
/// type guards read the low half of.
#[test]
fn the_header_word_is_class_id_then_mark() {
    let h = compact_header(0x1234);
    let w = h.header_word();
    assert_eq!(w as u32, 0x1234);
    assert_eq!((w >> 32) as u32, h.mark());
    let o = materialise(&h);
    assert_eq!(u64::from_le_bytes(o.bytes()[0..8].try_into().unwrap()), w);
    // The flags byte and kind byte are where the JIT tests them.
    assert_eq!(
        o.bytes()[GC_FLAGS_BYTE_OFFSET] & 0xF,
        GC_FLAG_COMPACT | GC_FLAG_HEADER
    );
    assert_eq!(o.bytes()[KIND_TAGS_BYTE_OFFSET] & KIND_TAG_BYTE_MASK, 0);
}

/// No header this constructor builds is all-zero: `new Object()` is
/// `ClassId(0)` with every quartet field zero, so `GC_FLAG_HEADER` is the
/// only thing that separates it from reclaimed arena space.
#[test]
fn a_published_header_word_is_never_zero() {
    let mut minimal = ObjectHeader::new(
        ClassId::new(0),
        ObjectKind::Object,
        ArrayElementType::Reference,
        0,
        0,
    );
    minimal.set_compact_shape(0, 16);
    assert_ne!(minimal.header_word(), 0);
    assert_eq!(minimal.header_word() as u32, 0);
}

// -- Header length -----------------------------------------------------------

#[test]
fn compact_instances_are_short_and_everything_else_is_long() {
    let c = compact_header(3);
    assert!(c.is_short());
    assert_eq!(c.header_size(), COMPACT_HEADER_SIZE);
    assert_eq!(c.payload_offset(), COMPACT_HEADER_SIZE);

    let legacy = header(ObjectKind::Object, ArrayElementType::Reference, 0, 4);
    assert!(!legacy.is_short());
    assert_eq!(legacy.header_size(), HEADER_SIZE);
    assert_eq!(legacy.num_slots(), 4);
    assert_eq!(legacy.payload_offset(), HEADER_SIZE);

    let arr = header(ObjectKind::Array, ArrayElementType::Int, 100, 100);
    assert!(!arr.is_short());
    assert_eq!(arr.array_length(), 100);
    assert_eq!(arr.element_type(), ArrayElementType::Int);
    assert_eq!(arr.payload_offset(), ARRAY_DATA_OFFSET);
}

/// Marking a header compact clears the second word, so writing the whole
/// struct over a compact instance's fresh fields writes zeroes there.
#[test]
fn marking_compact_clears_the_second_word() {
    let c = compact_header(3);
    assert_eq!(c.raw_shape(), 0);
    assert_eq!(c.raw_aux(), 0);
    let o = materialise(&c);
    assert!(o.bytes()[8..32].iter().all(|&b| b == 0));
}

#[test]
fn a_long_header_writes_its_shape_word() {
    let arr = header(ObjectKind::Array, ArrayElementType::Long, 9, 9);
    let o = materialise(&arr);
    assert_eq!(u32::from_le_bytes(o.bytes()[8..12].try_into().unwrap()), 9);
    assert_eq!(view(&o).array_length(), 9);
}

#[test]
fn a_plain_object_never_records_an_element_type() {
    let h = header(ObjectKind::Object, ArrayElementType::Boolean, 0, 2);
    assert_eq!(ObjectHeader::element_type_tag(h.mark()), 0);
    assert_eq!(h.element_type(), ArrayElementType::Reference);
}

// -- GC flags and age ----------------------------------------------------------

#[test]
fn gc_flag_constants() {
    assert_eq!(GC_FLAG_OLD_GEN, 0x01);
    assert_eq!(GC_FLAG_MARKED, 0x02);
    assert_eq!(GC_FLAG_COMPACT, 0x04);
    assert_eq!(GC_FLAG_HEADER, 0x08);
}

#[test]
fn flags_add_clear_and_claim() {
    let h = header(ObjectKind::Object, ArrayElementType::Reference, 0, 1);
    assert!(!h.is_old_gen());
    h.add_gc_flags(GC_FLAG_OLD_GEN);
    assert!(h.is_old_gen());
    assert!(h.try_add_gc_flags(GC_FLAG_MARKED));
    assert!(!h.try_add_gc_flags(GC_FLAG_MARKED));
    assert!(!h.try_add_gc_flags(0));
    h.clear_gc_flags(GC_FLAG_MARKED);
    assert!(h.try_add_gc_flags(GC_FLAG_MARKED));
    assert_eq!(
        h.gc_flags(),
        GC_FLAG_OLD_GEN | GC_FLAG_MARKED | GC_FLAG_HEADER
    );
}

#[test]
fn every_gc_age_survives_every_transition() {
    for age in 0..=MAX_GC_AGE {
        let h = compact_header(5);
        h.set_gc_age(age);
        let m = h.mark();
        for w in [
            ObjectHeader::make_thin_locked(m, MAX_THIN_LOCK_SLOT, MAX_THIN_LOCK_RECURSION),
            ObjectHeader::make_inflated(m),
            ObjectHeader::make_neutral(m),
            ObjectHeader::make_neutral_hashed(m, -1),
        ] {
            assert_eq!(ObjectHeader::gc_age_of(w), age);
            assert_eq!(
                ObjectHeader::gc_flags_of(w),
                GC_FLAG_COMPACT | GC_FLAG_HEADER
            );
            assert_eq!(ObjectHeader::kind_of(w), ObjectKind::Object);
        }
    }
    let h = compact_header(5);
    h.set_gc_age(200);
    assert_eq!(h.gc_age(), MAX_GC_AGE);
}

// -- Thin locks -------------------------------------------------------------------

#[test]
fn thin_lock_words_round_trip() {
    let m = compact_header(5).mark();
    for slot in [0, 1, 0x155, MAX_THIN_LOCK_SLOT] {
        for rec in 0..=MAX_THIN_LOCK_RECURSION {
            let w = ObjectHeader::make_thin_locked(m, slot, rec);
            assert_eq!(ObjectHeader::mark_state(w), MARK_THIN_LOCKED);
            assert_eq!(ObjectHeader::thin_lock_owner(w), slot);
            assert_eq!(ObjectHeader::thin_lock_recursion(w), rec);
            assert_eq!(ObjectHeader::quartet_of(w), ObjectHeader::quartet_of(m));
            assert_eq!(ObjectHeader::short_hash_bits(w), 0);
        }
    }
}

#[test]
fn a_hashed_compact_word_is_not_thin_lockable_but_a_hashed_array_is() {
    let c = compact_header(5);
    assert!(ObjectHeader::thin_lockable(c.mark()));
    c.identity_hash(|| 0x7_1234).unwrap();
    assert!(!ObjectHeader::thin_lockable(c.mark()));

    let arr = header(ObjectKind::Array, ArrayElementType::Int, 3, 3);
    let o = materialise(&arr);
    view(&o).identity_hash(|| 42).unwrap();
    assert!(ObjectHeader::thin_lockable(view(&o).mark()));
}

#[test]
fn an_aged_array_is_still_thin_lockable() {
    let arr = header(ObjectKind::Array, ArrayElementType::Int, 3, 3);
    arr.set_gc_age(MAX_GC_AGE);
    arr.add_gc_flags(GC_FLAG_OLD_GEN | GC_FLAG_MARKED);
    assert!(ObjectHeader::thin_lockable(arr.mark()));
}

// -- Identity hash ----------------------------------------------------------------

#[test]
fn a_compact_hash_is_installed_once_and_then_stable() {
    let c = compact_header(11);
    let first = c.identity_hash(|| 0x0ABC_DEF1).unwrap();
    assert_ne!(first, 0);
    assert!(first >= 0);
    let again = c.identity_hash(|| 999).unwrap();
    assert_eq!(first, again);
    assert_eq!(c.installed_identity_hash(), first);
    assert_eq!(ObjectHeader::short_hash_bits(c.mark()), 0xCDEF1);
    // The quartet is untouched.
    assert!(c.is_short());
    assert_eq!(c.gc_flags(), GC_FLAG_COMPACT | GC_FLAG_HEADER);
}

#[test]
fn a_minted_zero_is_never_installed() {
    let c = compact_header(11);
    let h = c.identity_hash(|| 0x10_0000).unwrap();
    assert_ne!(ObjectHeader::short_hash_bits(c.mark()), 0);
    assert_eq!(c.identity_hash(|| 5).unwrap(), h);

    let arr = header(ObjectKind::Array, ArrayElementType::Int, 1, 1);
    let o = materialise(&arr);
    assert_eq!(view(&o).identity_hash(|| 0).unwrap(), 1);
}

#[test]
fn a_non_neutral_compact_instance_refuses_rather_than_minting() {
    let c = compact_header(11);
    let m = c.mark();
    c.mark_word
        .store(ObjectHeader::make_thin_locked(m, 3, 0), Ordering::Relaxed);
    assert_eq!(c.identity_hash(|| 77), Err(()));
    c.mark_word
        .store(ObjectHeader::make_inflated(m), Ordering::Relaxed);
    assert_eq!(c.identity_hash(|| 77), Err(()));
}

#[test]
fn a_long_header_hash_lives_beside_the_lock_state() {
    let legacy = header(ObjectKind::Object, ArrayElementType::Reference, 0, 2);
    let o = materialise(&legacy);
    let v = view(&o);
    let h = v.identity_hash(|| 0x4455_6677).unwrap();
    assert_eq!(h, 0x4455_6677);
    let m = v.mark();
    v.mark_word
        .store(ObjectHeader::make_thin_locked(m, 9, 2), Ordering::Relaxed);
    assert_eq!(v.identity_hash(|| 1).unwrap(), h);
    assert_eq!(v.num_slots(), 2, "the hash does not disturb the shape word");
}

#[test]
fn compact_hashes_of_different_classes_differ_in_their_high_bits() {
    let a = ObjectHeader::short_hash_value(1, 0x12345);
    let b = ObjectHeader::short_hash_value(2, 0x12345);
    assert_eq!(a & 0xF_FFFF, 0x12345);
    assert_eq!(b & 0xF_FFFF, 0x12345);
    assert_ne!(a, b);
    assert!(a > 0 && b > 0);
}

#[test]
fn concurrent_hashers_converge_on_one_value() {
    let c = std::sync::Arc::new(compact_header(13));
    let hs: Vec<i32> = (0..8)
        .map(|i| {
            let c = c.clone();
            std::thread::spawn(move || c.identity_hash(|| 1000 + i).unwrap())
        })
        .map(|t| t.join().unwrap())
        .collect();
    assert!(hs.windows(2).all(|w| w[0] == w[1]));
}

#[test]
fn inflating_or_forwarding_drops_the_compact_hash_bits() {
    let c = compact_header(11);
    c.identity_hash(|| 0xF_FFFF).unwrap();
    let m = c.mark();
    assert_eq!(
        ObjectHeader::short_hash_bits(ObjectHeader::make_inflated(m)),
        0
    );
    assert_eq!(
        ObjectHeader::make_inflated(m) & MARK_SHORT_HASH_HI_MASK,
        0,
        "an inflated compact word must not keep hash bits in byte 6"
    );
}

// -- Forwarding -----------------------------------------------------------------

#[test]
fn forwarding_writes_the_second_word_and_keeps_the_header_word() {
    let target = materialise(&header(ObjectKind::Array, ArrayElementType::Int, 33, 33));
    let src = materialise(&header(ObjectKind::Array, ArrayElementType::Int, 33, 33));
    let s = view(&src);
    let before = s.header_word();
    let t = target.ptr();
    assert!(s.forwarding_address().is_null());
    s.set_forwarding_address(t);
    assert!(s.is_forwarded());
    assert!(!s.is_self_forwarded());
    assert_eq!(s.forwarding_address(), t);
    assert_eq!(s.class_id, ClassId::new(7));
    assert_eq!(s.kind(), ObjectKind::Array);
    assert_eq!(
        ObjectHeader::quartet_of(s.mark()),
        ObjectHeader::quartet_of((before >> 32) as u32)
    );
    // The shape word is gone from the source but resolves via the copy.
    assert_ne!(s.raw_shape(), 33);
    assert_eq!(s.array_length(), 33);

    s.retire_forwarding();
    assert!(!s.is_forwarded());
    assert_eq!(s.raw_shape(), 33, "retirement restores the second word");
}

#[test]
fn a_self_forward_leaves_the_body_alone() {
    let o = materialise(&compact_header(4));
    unsafe {
        std::ptr::copy_nonoverlapping(0xDEAD_BEEF_u64.to_le_bytes().as_ptr(), o.ptr().add(8), 8)
    };
    let s = view(&o);
    let me = o.ptr();
    s.set_forwarding_address(me);
    assert!(s.is_self_forwarded());
    assert_eq!(s.forwarding_address(), me);
    assert_eq!(
        u64::from_le_bytes(o.bytes()[8..16].try_into().unwrap()),
        0xDEAD_BEEF
    );
    view(&o).retire_forwarding();
    assert!(!view(&o).is_forwarded());
    assert!(view(&o).is_short());
}

#[test]
fn a_claim_is_won_once_and_published_after() {
    let target = materialise(&compact_header(4));
    let src = materialise(&compact_header(4));
    let s = view(&src);
    let observed = s.mark();
    assert!(s.try_claim_forwarding(observed).is_ok());
    assert!(s.try_claim_forwarding(observed).is_err());
    assert!(s.is_forwarded());
    s.publish_claimed_forwarding(target.ptr());
    assert_eq!(s.forwarding_address(), target.ptr());
}

#[test]
fn an_unpublished_claim_is_bounded_without_scheduler_yields() {
    let src = materialise(&compact_header(4));
    let s = view(&src);
    let observed = s.mark();
    assert!(s.try_claim_forwarding(observed).is_ok());
    let before = FORWARDING_BUSY_WAIT_ABANDONED.load(Ordering::Relaxed);

    assert!(s.forwarding_address().is_null());
    assert_eq!(
        FORWARDING_BUSY_WAIT_ABANDONED.load(Ordering::Relaxed),
        before + 1,
        "a stuck claim must abandon after the bounded probe"
    );
}

#[test]
fn only_the_forwarded_tag_reads_as_forwarded() {
    let m = compact_header(4).mark();
    assert!(!ObjectHeader::is_forwarded_mark(m));
    assert!(!ObjectHeader::is_forwarded_mark(
        ObjectHeader::make_thin_locked(m, 3, 3)
    ));
    assert!(!ObjectHeader::is_forwarded_mark(
        ObjectHeader::make_inflated(m)
    ));
    assert!(ObjectHeader::is_forwarded_mark(m | MARK_FORWARDED));
}

#[test]
#[should_panic(expected = "forwarding target")]
fn a_misaligned_forwarding_target_is_refused() {
    let src = materialise(&compact_header(4));
    view(&src).set_forwarding_address(0x1003 as *mut u8);
}

#[test]
fn shape_source_refuses_unmapped_forwarding_target_without_faulting() {
    // An object or candidate word whose mark happens to be MARK_FORWARDED and whose
    // second word holds an aligned 64-bit value in unmapped space (e.g. 0x1_0000_0000,
    // which decodes as shape=0, aux=1).
    let raw = ObjectHeader {
        class_id: ClassId::new(0),
        mark_word: AtomicU32::new(MARK_FORWARDED),
        shape: AtomicU32::new(0),
        aux: AtomicU32::new(1),
    };
    let h = &raw;
    assert_eq!(h.shape_source() as *const _, h as *const _);
    assert_eq!(h.num_slots(), 0);
    assert_eq!(h.array_length(), 0);
}

// -- Header screen ------------------------------------------------------------

#[test]
fn the_object_header_screen_separates_headers_from_numbers() {
    let obj = materialise(&header(
        ObjectKind::Object,
        ArrayElementType::Reference,
        0,
        3,
    ));
    assert!(unsafe { plausible_object_header_at(obj.ptr()) });
    let arr = materialise(&header(ObjectKind::Array, ArrayElementType::Long, 16, 16));
    assert!(unsafe { plausible_object_header_at(arr.ptr()) });
    let c = materialise(&compact_header(9));
    view(&c).identity_hash(|| 0xF_FFFF).unwrap();
    assert!(
        unsafe { plausible_object_header_at(c.ptr()) },
        "a hashed compact instance's hash bits are not a corrupt element type"
    );
    let filler = materialise(&header(
        ObjectKind::HumongousFiller,
        ArrayElementType::Reference,
        0,
        0,
    ));
    assert!(!unsafe { plausible_object_header_at(filler.ptr()) });

    let mut rejected = 0usize;
    const N: usize = 256;
    for i in 0..N {
        let junk = std::cell::UnsafeCell::new([
            0x1234_5678 ^ (i as u64),
            (i as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15),
            u64::MAX - i as u64,
            i as u64,
        ]);
        if !unsafe { plausible_object_header_at(junk.get().cast()) } {
            rejected += 1;
        }
    }
    assert!(rejected * 2 >= N, "rejected only {rejected} of {N}");
}

#[test]
fn primitive_array_kind_bytes_have_clear_reserved_bits() {
    for d in ["[Z", "[C", "[F", "[D", "[B", "[S", "[I", "[J"] {
        let tag = primitive_array_kind_tags_byte(d).unwrap();
        let arr = header(
            ObjectKind::Array,
            array_element_type_from_tag(tag >> 2).unwrap(),
            1,
            1,
        );
        let o = materialise(&arr);
        assert_eq!(o.bytes()[KIND_TAGS_BYTE_OFFSET], tag, "{d}");
    }
}

/// Reinterpret a `Value` as its raw cell bytes, keeping the padding
/// `MaybeUninit`.
///
/// `Value` is an enum with padding, so `transmute::<Value, [u8; 16]>` is
/// Undefined Behaviour — it claims every byte is an initialised integer
/// when the padding is not. That is not pedantry: Miri rejects it, and it
/// is what this test used to do. Transmuting to `[MaybeUninit<u8>; 16]` is
/// always sound, and the readers below only ever `assume_init` the ranges
/// the layout defines as real data.
#[cfg(test)]
fn value_cell_bytes(value: crate::Value) -> [std::mem::MaybeUninit<u8>; SLOT_SIZE] {
    const _: () = assert!(std::mem::size_of::<crate::Value>() == SLOT_SIZE);
    // SAFETY: sizes are equal (checked above) and `MaybeUninit<u8>` imposes
    // no validity requirement on any byte, padding included.
    unsafe { std::mem::transmute(value) }
}

/// Read `N` initialised bytes at `offset` out of a cell.
///
/// # Panics (as a test failure)
/// Reading a padding byte here would be UB; the caller is responsible for
/// only naming offsets the layout assertions establish as initialised.
#[cfg(test)]
fn cell_bytes_at<const N: usize>(
    cell: &[std::mem::MaybeUninit<u8>; SLOT_SIZE],
    offset: usize,
) -> [u8; N] {
    let mut out = [0u8; N];
    for (i, slot) in out.iter_mut().enumerate() {
        // SAFETY: `offset .. offset + N` is a discriminant or payload
        // range, never padding — that is exactly what this test pins.
        *slot = unsafe { cell[offset + i].assume_init() };
    }
    out
}

/// Pin the in-memory layout of a `Value` field cell so the JIT's inline
/// `getfield` codegen (which emits a raw `MOV [recv + FIELD_CELL_*]`)
/// stays correct.
///
/// `Value` is `#[repr(u32)]` with explicit discriminants, so this is now a
/// *second* line of defence rather than the only one — `value.rs` asserts
/// the same four facts at compile time (search `value_tag_word`), which is
/// what actually protects the JIT. This test survives because it exercises
/// the real byte-reinterpretation path the JIT performs, including the
/// non-null `Object` case that const-eval cannot express (it would have to
/// read pointer provenance).
#[test]
fn field_cell_layout_matches_value_enum() {
    use crate::Value;

    // A 4-byte (`Int`) payload lives at FIELD_CELL_PAYLOAD32_OFFSET.
    let cell = value_cell_bytes(Value::Int(0x1234_5678));
    let tag = u32::from_le_bytes(cell_bytes_at::<4>(&cell, FIELD_CELL_TAG_OFFSET));
    assert_eq!(
        tag, 0,
        "Int discriminant must be 0 at FIELD_CELL_TAG_OFFSET"
    );
    let p32 = i32::from_le_bytes(cell_bytes_at::<4>(&cell, FIELD_CELL_PAYLOAD32_OFFSET));
    assert_eq!(
        p32, 0x1234_5678,
        "Int payload at FIELD_CELL_PAYLOAD32_OFFSET"
    );

    // An 8-byte (`Long`) payload lives at FIELD_CELL_PAYLOAD64_OFFSET.
    let cell = value_cell_bytes(Value::Long(0x0102_0304_0506_0708_i64));
    let tag = u32::from_le_bytes(cell_bytes_at::<4>(&cell, FIELD_CELL_TAG_OFFSET));
    assert_eq!(tag, 1, "Long discriminant must be 1");
    let p64 = i64::from_le_bytes(cell_bytes_at::<8>(&cell, FIELD_CELL_PAYLOAD64_OFFSET));
    assert_eq!(p64, 0x0102_0304_0506_0708_i64, "Long payload at +8");

    // The remaining discriminants. The JIT bakes only `0` (Int) and `4`
    // (Object) as literals, but it bakes them as *positions in this
    // sequence* — a reorder that left Int at 0 while moving Object would
    // still miscompile `x64/objects.rs`. Pin the whole run so any reorder
    // fails here, not in generated code.
    for (v, want, name) in [
        (Value::Float(1.5), 2u32, "Float"),
        (Value::Double(1.5), 3, "Double"),
        (Value::Object(None), 4, "Object"),
        (Value::ReturnAddress(7), 5, "ReturnAddress"),
        (Value::Uninitialized, 6, "Uninitialized"),
    ] {
        let cell = value_cell_bytes(v);
        let tag = u32::from_le_bytes(cell_bytes_at::<4>(&cell, FIELD_CELL_TAG_OFFSET));
        assert_eq!(tag, want, "{name} discriminant must be {want}");
    }

    // `Object(None)` (JVM null) must leave the 8-byte payload word zero.
    let cell = value_cell_bytes(Value::Object(None));
    let p64 = u64::from_le_bytes(cell_bytes_at::<8>(&cell, FIELD_CELL_PAYLOAD64_OFFSET));
    assert_eq!(p64, 0, "Object(None) payload word must be zero");

    // A non-null `Object` stores its raw pointer at FIELD_CELL_PAYLOAD64_OFFSET.
    let backing = Box::leak(Box::new(0u64));
    let raw = backing as *mut u64 as usize as u64;
    let oref = unsafe { crate::ObjectRef::from_raw(backing as *mut u64 as *mut u8) };
    let cell = value_cell_bytes(Value::Object(Some(oref)));
    let p64 = u64::from_le_bytes(cell_bytes_at::<8>(&cell, FIELD_CELL_PAYLOAD64_OFFSET));
    assert_eq!(
        p64, raw,
        "Object(Some) payload word must be the raw pointer"
    );
    // SAFETY: reclaim the leaked allocation.
    unsafe { drop(Box::from_raw(backing)) };
}

// -- element_byte_size --

#[test]
fn element_byte_size_boolean() {
    assert_eq!(element_byte_size(ArrayElementType::Boolean), 1);
}

#[test]
fn element_byte_size_byte() {
    assert_eq!(element_byte_size(ArrayElementType::Byte), 1);
}

#[test]
fn element_byte_size_char() {
    assert_eq!(element_byte_size(ArrayElementType::Char), 2);
}

#[test]
fn element_byte_size_short() {
    assert_eq!(element_byte_size(ArrayElementType::Short), 2);
}

#[test]
fn element_byte_size_int() {
    assert_eq!(element_byte_size(ArrayElementType::Int), 4);
}

#[test]
fn element_byte_size_float() {
    assert_eq!(element_byte_size(ArrayElementType::Float), 4);
}

#[test]
fn element_byte_size_long() {
    assert_eq!(element_byte_size(ArrayElementType::Long), 8);
}

#[test]
fn element_byte_size_double() {
    assert_eq!(element_byte_size(ArrayElementType::Double), 8);
}

#[test]
fn element_byte_size_reference() {
    assert_eq!(
        element_byte_size(ArrayElementType::Reference),
        REF_ELEMENT_SIZE
    );
}

// -- array_data_size / array_data_size_checked --

#[test]
fn array_data_size_zero_length() {
    assert_eq!(array_data_size(0, ArrayElementType::Int).unwrap(), 0);
}

#[test]
fn array_data_size_single_byte_element() {
    // 1 byte -> rounds up to 8
    assert_eq!(array_data_size(1, ArrayElementType::Byte).unwrap(), 8);
}

#[test]
fn array_data_size_eight_byte_elements() {
    // 8 bytes exactly -> no padding needed
    assert_eq!(array_data_size(1, ArrayElementType::Long).unwrap(), 8);
}

#[test]
fn array_data_size_alignment() {
    // 3 ints = 12 bytes -> rounds up to 16
    assert_eq!(array_data_size(3, ArrayElementType::Int).unwrap(), 16);
    // 5 ints = 20 bytes -> rounds up to 24
    assert_eq!(array_data_size(5, ArrayElementType::Int).unwrap(), 24);
    // 2 ints = 8 bytes -> exact
    assert_eq!(array_data_size(2, ArrayElementType::Int).unwrap(), 8);
}

#[test]
fn array_data_size_boolean_array() {
    // 10 booleans = 10 bytes -> rounds to 16
    assert_eq!(array_data_size(10, ArrayElementType::Boolean).unwrap(), 16);
}

#[test]
fn array_data_size_reference_array() {
    // 3 refs = 24 bytes -> exact
    assert_eq!(array_data_size(3, ArrayElementType::Reference).unwrap(), 24);
}

#[test]
fn array_data_size_checked_overflow() {
    // Extremely large length should return None
    let result = array_data_size_checked(usize::MAX, ArrayElementType::Long);
    assert!(result.is_none());
}

#[test]
fn array_data_size_checked_valid() {
    let result = array_data_size_checked(10, ArrayElementType::Int);
    assert_eq!(result, Some(40));
}

#[test]
fn array_data_size_returns_err_on_overflow() {
    let result = array_data_size(usize::MAX, ArrayElementType::Long);
    assert!(result.is_err());
    assert!(result.unwrap_err().contains("overflow"));
}

// -- ObjectKind --

#[test]
fn object_kind_values() {
    assert_eq!(ObjectKind::Object as u8, 0);
    assert_eq!(ObjectKind::Array as u8, 1);
}

#[test]
fn object_kind_equality() {
    assert_eq!(ObjectKind::Object, ObjectKind::Object);
    assert_ne!(ObjectKind::Object, ObjectKind::Array);
}

// -- ArrayElementType repr values --

#[test]
fn array_element_type_repr_values() {
    assert_eq!(ArrayElementType::Reference as u8, 0);
    assert_eq!(ArrayElementType::Boolean as u8, 4);
    assert_eq!(ArrayElementType::Char as u8, 5);
    assert_eq!(ArrayElementType::Float as u8, 6);
    assert_eq!(ArrayElementType::Double as u8, 7);
    assert_eq!(ArrayElementType::Byte as u8, 8);
    assert_eq!(ArrayElementType::Short as u8, 9);
    assert_eq!(ArrayElementType::Int as u8, 10);
    assert_eq!(ArrayElementType::Long as u8, 11);
}

#[test]
fn autobox_class_id_constant() {
    // LOW (2026-06-17): moved from the unreserved mid-range 0xAB00_0000
    // to u32::MAX so it can never collide with a sequentially-assigned id.
    assert_eq!(AUTOBOX_CLASS_ID.as_u32(), u32::MAX);
}

/// LOW (2026-06-17): the auto-box sentinel must live outside the dense,
/// from-0 sequential `ClassId` allocation range so it can never alias a
/// real loaded class. Pins the reservation invariant that the (non-const)
/// compile-time assert cannot express.
#[test]
fn autobox_class_id_is_reserved() {
    assert!(
        AUTOBOX_CLASS_ID.as_u32() >= MAX_SEQUENTIAL_CLASS_ID,
        "AUTOBOX_CLASS_ID must be outside the sequential allocation range"
    );
}

// -- ObjectHeader for array --

// -- gen r5w6/pin10: pure mark helpers and the screen-only resolution ------

/// The pure helpers compute exactly what the RMW setters leave in memory,
/// for every age (including past `MAX_GC_AGE`) and each non-compact flag.
#[test]
fn the_pure_mark_helpers_agree_with_the_setters() {
    let bases = [
        header(ObjectKind::Object, ArrayElementType::Reference, 0, 3).mark(),
        header(ObjectKind::Array, ArrayElementType::Int, 4, 4).mark(),
        compact_header(9).mark(),
        ObjectHeader::make_thin_locked(compact_header(9).mark(), 5, 2),
    ];
    let plain = || header(ObjectKind::Object, ArrayElementType::Reference, 0, 1);
    for base in bases {
        for age in [0u8, 1, 2, 3, 14, 15, 16, 200, 255] {
            let o = materialise(&plain());
            view(&o).mark_word.store(base, Ordering::Relaxed);
            view(&o).set_gc_age(age);
            let pure = ObjectHeader::mark_with_gc_age(base, age);
            assert_eq!(pure, view(&o).mark(), "{base:#x} {age}");
        }
        let flag_sets = [
            GC_FLAG_OLD_GEN,
            GC_FLAG_MARKED,
            GC_FLAG_HEADER,
            GC_FLAG_OLD_GEN | GC_FLAG_MARKED,
        ];
        for flags in flag_sets {
            let o = materialise(&plain());
            view(&o).mark_word.store(base, Ordering::Relaxed);
            view(&o).add_gc_flags(flags);
            let pure = ObjectHeader::mark_with_gc_flags(base, flags);
            assert_eq!(pure, view(&o).mark(), "{base:#x} {flags:#x}");
        }
    }
    // The evacuator's young-copy mark: age + 1 of the snapshot's age.
    let m = ObjectHeader::mark_with_gc_age(bases[0], 2);
    let next = ObjectHeader::mark_with_gc_age(m, ObjectHeader::gc_age_of(m).saturating_add(1));
    assert_eq!(ObjectHeader::gc_age_of(next), 3);
    let age_bits: u32 = 0xF << 28;
    assert_eq!(next & !age_bits, m & !age_bits, "only the age bits change");
}

/// Two whole headers, 8-aligned, side by side: `[0]` the candidate, `[1]`
/// a possible forward target.
struct Pair([ObjectHeader; 2]);

fn pair_header(p: &Pair, i: usize) -> &ObjectHeader {
    &p.0[i]
}

fn set_pair(p: &Pair, i: usize, mark: u32, second: u64) {
    p.0[i].mark_word.store(mark, Ordering::Relaxed);
    p.0[i].shape.store(second as u32, Ordering::Relaxed);
    p.0[i].aux.store((second >> 32) as u32, Ordering::Relaxed);
}

fn pair() -> Pair {
    Pair(std::array::from_fn(|_| {
        header(ObjectKind::Object, ArrayElementType::Reference, 0, 0)
    }))
}

#[test]
fn the_screen_resolves_a_real_shape_and_a_forward_it_was_allowed_to_follow() {
    let arr = header(ObjectKind::Array, ArrayElementType::Int, 5, 5).mark();
    let p = pair();
    // Not forwarded: its own shape, `inside` never asked.
    set_pair(&p, 0, arr, 5);
    let own = pair_header(&p, 0).screen_shape(|_| panic!("asked"));
    assert_eq!(own, Some(5));
    assert_eq!(own, Some(pair_header(&p, 0).array_length()));
    // Forwarded to [1], which `inside` accepts: [1]'s shape, as
    // `array_length` resolves it.
    let target = pair_header(&p, 1) as *const ObjectHeader as u64;
    set_pair(&p, 1, arr, 9);
    set_pair(&p, 0, arr | MARK_FORWARDED, target);
    // `shape_source` follows only a target in a span that has recorded a
    // reference (dev `ed3a63469`), which `set_forwarding_address` does for
    // every real forward; this hand-written one records it the same way.
    crate::value::record_object_ref_payload(target as *mut u8);
    assert_eq!(pair_header(&p, 0).array_length(), 9);
    let followed = pair_header(&p, 0).screen_shape(|t| t as u64 == target);
    assert_eq!(followed, Some(9));
    // Self-forwarded: its own shape word, no hop.
    set_pair(&p, 0, arr | MARK_FORWARDED | MARK_FWD_SELF, 5);
    assert_eq!(pair_header(&p, 0).screen_shape(|_| false), Some(5));
}

#[test]
fn the_screen_refuses_every_forward_no_collector_installs() {
    let arr = header(ObjectKind::Array, ArrayElementType::Int, 5, 5).mark();
    let p = pair();
    let here = pair_header(&p, 0) as *const ObjectHeader as u64;
    let target = pair_header(&p, 1) as *const ObjectHeader as u64;
    set_pair(&p, 1, arr, 9);
    // `0x1_0000_0000` is the netty shape: aligned and plausible, refused
    // by `inside`. The rest are refused before `inside` is asked: null,
    // misaligned, beyond 2^47, the header itself.
    let bad_targets = [
        0u64,
        0x1_0000_0000,
        0x1_0000_0004,
        1u64 << 47,
        here,
        target + 4,
    ];
    for bad in bad_targets {
        set_pair(&p, 0, arr | MARK_FORWARDED, bad);
        let screened = pair_header(&p, 0).screen_shape(|t| t as u64 == target);
        assert_eq!(screened, None, "{bad:#x}");
    }
    // A two-header cycle is cut at the hop bound.
    set_pair(&p, 0, arr | MARK_FORWARDED, target);
    set_pair(&p, 1, arr | MARK_FORWARDED, here);
    let both = |t: usize| t as u64 == here || t as u64 == target;
    assert_eq!(pair_header(&p, 0).screen_shape(both), None);
}

/// The busy-spin page
/// (`docs/feature-designs/generative-gc-architecture.md`):
/// a mark that reads `FORWARDED | BUSY` and never publishes is refused
/// after `SCREEN_BUSY_REREADS` reads, and `forwarding_address`'s
/// 4096-round wait is never entered.
#[test]
fn a_busy_looking_candidate_is_refused_without_the_long_wait() {
    let arr = header(ObjectKind::Array, ArrayElementType::Long, 5, 5).mark();
    let p = pair();
    let busy = MARK_FORWARDED | MARK_FWD_BUSY;
    set_pair(&p, 0, arr | busy, 0x1_0000_0000);
    let declined = ObjectHeader::screen_busy_declined();
    // `inside` accepts everything: the refusal must come from the BUSY
    // bound alone, not from the target test. (64 `spin_loop` rounds,
    // against the 262 144 of `resolved_shape` x `forwarding_address`.)
    assert_eq!(pair_header(&p, 0).screen_shape(|_| true), None);
    assert!(ObjectHeader::screen_busy_declined() > declined);
    // A legacy-instance-shaped word behaves the same.
    let legacy = header(ObjectKind::Object, ArrayElementType::Reference, 0, 1).mark();
    set_pair(&p, 1, legacy | busy, 0x1_0000_0000);
    assert_eq!(pair_header(&p, 1).screen_shape(|_| true), None);
}
