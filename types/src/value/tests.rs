use super::*;

#[test]
fn value_int() {
    let v = Value::Int(42);
    assert_eq!(v.as_int(), Some(42));
    assert!(!v.is_category2());
    assert!(!v.is_null());
}

#[test]
fn value_long_is_category2() {
    let v = Value::Long(100);
    assert!(v.is_category2());
}

#[test]
fn jlong_bits_as_aligned_object_ptr_matches_coerce_contract() {
    assert_eq!(jlong_bits_as_aligned_object_ptr(0), None);
    assert_eq!(jlong_bits_as_aligned_object_ptr(4), None);
    assert_eq!(jlong_bits_as_aligned_object_ptr(8), None);
    assert_eq!(
        jlong_bits_as_aligned_object_ptr(0x0000_5DDD_EEEE_F000),
        None
    );
    let known = 0x0000_5555_7777_8000usize;
    let _obj = unsafe { ObjectRef::from_raw(known as *mut u8) };
    assert_eq!(jlong_bits_as_aligned_object_ptr(known as u64), Some(known));
    assert_eq!(jlong_bits_as_aligned_object_ptr(1u64 << 47), None);
}

#[test]
fn value_null() {
    let v = Value::Object(None);
    assert!(v.is_null());
}

#[test]
fn value_discriminant_is_byte0_low32() {
    // `read_value_checked` validates corruption by reading the discriminant
    // as the low 32 bits of word 0. This pins that compiler-chosen layout:
    // every variant must encode its declaration-order discriminant there,
    // and every valid discriminant must be <= VALUE_MAX_DISCRIMINANT. If
    // the layout ever drifts (reordering the tag, or moving it off word 0),
    // this fails loudly — otherwise `read_value_checked` could reject valid
    // values (e.g. an `Int` whose payload aliases byte 0) or pass corrupt
    // ones.
    let cases: [(Value, u32); 7] = [
        (Value::Int(0), 0),
        (Value::Long(0), 1),
        (Value::Float(0.0), 2),
        (Value::Double(0.0), 3),
        (Value::Object(None), 4),
        (Value::ReturnAddress(0), 5),
        (Value::Uninitialized, 6),
    ];
    for (v, disc) in cases {
        // SAFETY: Value is asserted to be exactly 16 bytes; reinterpreting
        // it as two u64 words reads its own initialized bytes.
        let words = unsafe { std::mem::transmute::<Value, [u64; 2]>(v) };
        assert_eq!(
            words[0] as u32, disc,
            "discriminant of {v:?} is not at word-0 low-32"
        );
        assert!(disc <= VALUE_MAX_DISCRIMINANT);
    }
}

/// [`value_words`] agrees with the compiler's own layout for every byte
/// the variant actually USES, and writes zero for every byte it does not.
///
/// Asserted as a round trip through the real reader rather than against a
/// hand-written byte pattern: a hand-written pattern would re-state the
/// layout this file already pins in `value_discriminant_is_byte0_low32`,
/// and would pass even if `value_words` and `read_value_atomic` drifted
/// together. The round trip fails the moment they disagree.
#[test]
fn value_words_round_trips_through_the_atomic_reader() {
    for v in [
        Value::Int(i32::MIN),
        Value::Int(1),
        Value::Long(i64::MIN),
        Value::Float(-0.0),
        Value::Double(f64::NAN),
        Value::Object(None),
        Value::ReturnAddress(u32::MAX),
        Value::Uninitialized,
    ] {
        let mut cell = value_words(v);
        // SAFETY: `cell` is a 16-byte, 8-aligned buffer holding the words
        // `value_words` produced for a valid `Value`.
        let got = unsafe { read_value_atomic(cell.as_mut_ptr() as *const Value) };
        match (v, got) {
            // NaN is not `==` itself, so compare the bit pattern.
            (Value::Double(a), Value::Double(b)) => assert_eq!(a.to_bits(), b.to_bits()),
            (Value::Float(a), Value::Float(b)) => assert_eq!(a.to_bits(), b.to_bits()),
            _ => assert_eq!(got, v, "value_words({v:?}) did not round-trip"),
        }
    }
}

/// The invariant the containment argument rests on: a **narrow** variant
/// leaves the cell's 64-bit payload word — the word a compiled reference
/// `getfield` dereferences — as a hard zero, not as stack padding.
///
/// Written against a deliberately DIRTIED destination, because that is the
/// failure this pins: `transmute::<Value, [u64; 2]>` propagated whatever
/// the caller's temp held into `FIELD_CELL_PAYLOAD64_OFFSET`, so a cell
/// that already held a pointer-shaped word could keep it under an `Int`
/// tag. Zero here is what turns a punned slot into a benign null instead of
/// a wild pointer.
#[test]
fn narrow_variants_zero_the_payload64_word() {
    for v in [
        Value::Int(1),
        Value::Int(-1),
        Value::Float(1.0),
        Value::ReturnAddress(9),
        Value::Uninitialized,
    ] {
        let mut cell: [u64; 2] = [0xDEAD_BEEF_DEAD_BEEF, 0xCAFE_F00D_CAFE_F00D];
        // SAFETY: `cell` is a 16-byte, 8-aligned writable buffer.
        unsafe { write_value_atomic(cell.as_mut_ptr() as *mut Value, v) };
        assert_eq!(
            cell[1], 0,
            "{v:?} left the payload64 word non-zero — a compiled reference \
             getfield would dereference {:#x}",
            cell[1]
        );
    }
    // And the wide variants still carry their payload in that word.
    let mut cell: [u64; 2] = [0, 0];
    // SAFETY: as above.
    unsafe { write_value_atomic(cell.as_mut_ptr() as *mut Value, Value::Long(0x1234_5678)) };
    assert_eq!(cell[1], 0x1234_5678);
}

#[test]
fn read_value_checked_round_trips_valid_and_rejects_corrupt() {
    // Valid values decode unchanged.
    for v in [
        Value::Int(97),
        Value::Long(-1),
        Value::Double(1.5),
        Value::Object(None),
        Value::ReturnAddress(7),
        Value::Uninitialized,
    ] {
        // SAFETY: &v is a valid, aligned 16-byte Value slot.
        let got = unsafe { read_value_checked(&v as *const Value) };
        assert_eq!(got, Some(v));
    }
    // A slot whose discriminant word is a heap-pointer-shaped value (the
    // HIB-CV-32 corruption shape: {disc = pointer-low-32, payload = 6}) is
    // rejected rather than decoded into a UB-on-match `Value`.
    let corrupt: [u64; 2] = [0x0000_0001_02e9_1188, 6];
    // SAFETY: `corrupt` is a 16-byte, 8-aligned buffer; we only read it.
    let got = unsafe { read_value_checked(corrupt.as_ptr() as *const Value) };
    assert_eq!(got, None);
}

#[test]
fn value_display() {
    assert_eq!(format!("{}", Value::Int(42)), "int(42)");
    assert_eq!(format!("{}", Value::Object(None)), "null");
    assert_eq!(format!("{}", Value::Uninitialized), "<uninitialized>");
}

#[test]
fn encode_decode_int() {
    let (val, tag) = encode_value(Value::Int(42));
    assert_eq!(decode_value(val, tag).as_int(), Some(42));
    let (val, tag) = encode_value(Value::Int(-1));
    assert_eq!(decode_value(val, tag).as_int(), Some(-1));
    let (val, tag) = encode_value(Value::Int(i32::MAX));
    assert_eq!(decode_value(val, tag).as_int(), Some(i32::MAX));
    let (val, tag) = encode_value(Value::Int(i32::MIN));
    assert_eq!(decode_value(val, tag).as_int(), Some(i32::MIN));
}

#[test]
fn encode_decode_long() {
    let (val, tag) = encode_value(Value::Long(123456789012345));
    assert_eq!(decode_value(val, tag).as_long(), Some(123456789012345));
    let (val, tag) = encode_value(Value::Long(-1));
    assert_eq!(decode_value(val, tag).as_long(), Some(-1));
    let (val, tag) = encode_value(Value::Long(i64::MAX));
    assert_eq!(decode_value(val, tag).as_long(), Some(i64::MAX));
    let (val, tag) = encode_value(Value::Long(i64::MIN));
    assert_eq!(decode_value(val, tag).as_long(), Some(i64::MIN));
}

#[test]
fn encode_decode_float() {
    let (val, tag) = encode_value(Value::Float(3.15));
    let decoded = decode_value(val, tag).as_float().unwrap();
    assert!((decoded - 3.15).abs() < 1e-6);
    let (val, tag) = encode_value(Value::Float(-0.0));
    assert_eq!(
        decode_value(val, tag).as_float().unwrap().to_bits(),
        (-0.0f32).to_bits()
    );
}

#[test]
fn encode_decode_double() {
    let (val, tag) = encode_value(Value::Double(2.719281828));
    let decoded = decode_value(val, tag).as_double().unwrap();
    assert!((decoded - 2.719281828).abs() < 1e-9);
}

#[test]
fn encode_decode_null() {
    let (val, tag) = encode_value(Value::Object(None));
    assert!(decode_value(val, tag).is_null());
}

#[test]
fn encode_decode_uninit() {
    let (val, tag) = encode_value(Value::Uninitialized);
    assert_eq!(decode_value(val, tag), Value::Uninitialized);
}

#[test]
fn encode_decode_object_ref() {
    // Use an aligned pointer (multiple of 8)
    let fake_ptr = 0x1234_5678_ABC0_u64 as *mut u8;
    let obj = unsafe { ObjectRef::from_raw(fake_ptr) };
    let (val, tag) = encode_value(Value::Object(Some(obj)));
    assert!(is_object_tag(tag));
    let decoded = decode_value(val, tag);
    assert!(matches!(decoded, Value::Object(Some(r)) if r.as_ptr() as u64 == 0x1234_5678_ABC0));
    let checked = decode_value_checked(val, tag, |addr| addr == val);
    assert!(matches!(checked, Value::Object(Some(r)) if r.as_ptr() as u64 == 0x1234_5678_ABC0));
}

#[test]
fn decode_value_rejects_unseen_plausible_object_payload() {
    let _guard = crate::compact_value::degrade_counter_test_lock();
    let unseen = 0x0000_6AAA_BBBB_C000u64;
    assert!(plausible_heap_pointer(unseen));
    assert!(matches!(
        decode_value(unseen, VTAG_OBJECT),
        Value::Object(None)
    ));
    assert!(matches!(
        decode_value_checked(unseen, VTAG_OBJECT, |_| false),
        Value::Object(None)
    ));
}

/// Pins the provenance-bitmap granule semantics: recording a reference
/// marks its whole 64-byte granule known (a documented false-positive
/// tradeoff vs. the old exact set — see the module note on
/// PROVENANCE_L1), while the neighboring granule stays unknown.
///
/// Uses an address region no other test records into, since the bitmap
/// is process-global and `cargo test` shares one process.
#[test]
fn provenance_bitmap_granule_semantics() {
    let base = 0x0000_4A11_2233_4400u64; // 64-byte aligned, unique region
    assert!(!object_ref_payload_is_known(base));
    assert!(!object_ref_payload_is_known(base + 0x40));
    let _obj = unsafe { ObjectRef::from_raw(base as *mut u8) };
    // The recorded address and its granule-mates are known …
    assert!(object_ref_payload_is_known(base));
    assert!(object_ref_payload_is_known(base + 8));
    assert!(object_ref_payload_is_known(base + 0x38));
    // … the adjacent granule is not, and implausible bits never are.
    assert!(!object_ref_payload_is_known(base + 0x40));
    assert!(!object_ref_payload_is_known(base + 1)); // unaligned
    assert!(!object_ref_payload_is_known(0));
}

/// g1sz: a `ProvenanceBatch` records exactly what the direct path would,
/// once flushed: every batched granule, across a word change, a flush and
/// a drop, and nothing else.
#[test]
fn provenance_batch_records_every_target_once_flushed() {
    // Two words of a region no other test records into, its leaf mapped by
    // a direct record first (a batch records a leafless GiB at once).
    let w0 = 0x0000_4A11_2255_0000u64;
    let w1 = w0 + 0x1000;
    let _seed = unsafe { ObjectRef::from_raw((w0 + 0x40 * 63) as *mut u8) };
    let (a, b, c, d) = (w0, w0 + 0x40 * 5, w1 + 0x80, w1 + 0x40 * 9);
    for addr in [a, b, c, d] {
        assert!(!object_ref_payload_is_known(addr));
    }
    let mut batch = ProvenanceBatch::default();
    batch.record(a as *mut u8);
    batch.record(b as *mut u8);
    // Still pending: the word has not changed and nothing flushed.
    assert!(!object_ref_payload_is_known(a));
    batch.record(c as *mut u8); // moves to the next word: `a` and `b` land
    assert!(object_ref_payload_is_known(a));
    assert!(object_ref_payload_is_known(b));
    assert!(!object_ref_payload_is_known(c));
    batch.flush();
    assert!(object_ref_payload_is_known(c));
    batch.record(d as *mut u8);
    drop(batch);
    assert!(object_ref_payload_is_known(d));
    assert!(!object_ref_payload_is_known(a + 0x40));
    assert!(!object_ref_payload_is_known(c + 0x40));
}

/// The per-thread `PROVENANCE_MEMO` must never suppress a *global* record.
/// It remembers granule bits per 4 KiB word, so the danger is a second
/// address in the same word being answered from the memo and never
/// reaching the bitmap. Record two granules in one word, then a third in
/// the next word, and check each is independently known while their
/// untouched neighbours are not.
#[test]
fn provenance_memo_does_not_suppress_records_within_a_word() {
    // 4 KiB aligned, in a region no other test records into.
    let word_base = 0x0000_4A11_2244_0000u64;
    let a = word_base;
    let b = word_base + 0x40 * 17; // same word (one word covers 64 granules)
    let c = word_base + 0x1000; // first granule of the NEXT word
    for addr in [a, b, c] {
        assert!(!object_ref_payload_is_known(addr));
    }
    let _pa = unsafe { ObjectRef::from_raw(a as *mut u8) };
    // `b` must take the slow path even though the memo now holds this
    // word: its own granule bit is still clear.
    let _pb = unsafe { ObjectRef::from_raw(b as *mut u8) };
    let _pc = unsafe { ObjectRef::from_raw(c as *mut u8) };
    for addr in [a, b, c] {
        assert!(object_ref_payload_is_known(addr), "{addr:#x} not recorded");
    }
    // Re-recording `a` after the memo has moved on to `c`'s word must
    // still leave it known (idempotence, not just first-write).
    let _pa2 = unsafe { ObjectRef::from_raw(a as *mut u8) };
    assert!(object_ref_payload_is_known(a));
    // Untouched neighbours in both words stay unknown.
    assert!(!object_ref_payload_is_known(a + 0x40));
    assert!(!object_ref_payload_is_known(c + 0x40));
}

#[test]
fn encode_decode_retaddr() {
    let (val, tag) = encode_value(Value::ReturnAddress(999));
    assert!(matches!(decode_value(val, tag), Value::ReturnAddress(999)));
}

#[test]
fn decode_value_rejects_null_object() {
    let _guard = crate::compact_value::degrade_counter_test_lock();
    // T14 graceful degradation: a VTAG_OBJECT tag paired with a null
    // pointer is treated as Value::Object(None) rather than panicking,
    // so corrupted or zero-initialized slots don't crash the VM.
    assert!(matches!(decode_value(0, VTAG_OBJECT), Value::Object(None)));
}

#[test]
fn decode_value_rejects_unaligned_object() {
    let _guard = crate::compact_value::degrade_counter_test_lock();
    // T14 graceful degradation: a VTAG_OBJECT tag paired with an
    // unaligned (non-8-byte-aligned) pointer is treated as
    // Value::Object(None) rather than panicking (KC16 SIGSEGV audit).
    assert!(matches!(
        decode_value(0x1001, VTAG_OBJECT),
        Value::Object(None)
    ));
}

#[test]
fn decode_value_rejects_null_page_object() {
    let _guard = crate::compact_value::degrade_counter_test_lock();
    assert!(matches!(decode_value(8, VTAG_OBJECT), Value::Object(None)));
    assert!(matches!(
        decode_value(0xff8, VTAG_OBJECT),
        Value::Object(None)
    ));
}

#[test]
fn decode_value_rejects_out_of_range_object() {
    let _guard = crate::compact_value::degrade_counter_test_lock();
    assert!(matches!(
        decode_value(1u64 << 47, VTAG_OBJECT),
        Value::Object(None)
    ));
}

/// HIGH long↔object audit: a `VTAG_OBJECT` slot whose pointer is null or
/// otherwise implausible degrades to `Object(None)` and must bump the
/// shared degradation counter so the reclassification is countable in release.
/// Mirrors the precedent of `decode_value_rejects_null_object` /
/// `decode_value_rejects_unaligned_object`, which exercise the same
/// release-mode degrade path.
#[test]
fn decode_value_object_degrade_increments_counter() {
    let _guard = crate::compact_value::degrade_counter_test_lock();
    use crate::compact_value::object_degradation_count;
    // DELTA, not a reset: the counter is process-wide and zeroing it would
    // break whatever other degrading test is mid-count. The guard above
    // keeps that "other test" from running concurrently at all.
    let base = object_degradation_count();
    // Null pointer with VTAG_OBJECT → degrade.
    assert!(matches!(decode_value(0, VTAG_OBJECT), Value::Object(None)));
    // Unaligned non-null pointer with VTAG_OBJECT → degrade.
    assert!(matches!(
        decode_value(0x1001, VTAG_OBJECT),
        Value::Object(None)
    ));
    // Aligned null-page pointer with VTAG_OBJECT -> degrade.
    assert!(matches!(decode_value(8, VTAG_OBJECT), Value::Object(None)));
    // Above the supported 47-bit user-space range -> degrade.
    assert!(matches!(
        decode_value(1u64 << 47, VTAG_OBJECT),
        Value::Object(None)
    ));
    assert!(
        object_degradation_count() - base >= 4,
        "expected at least 4 degradations recorded, got {}",
        object_degradation_count() - base
    );
}

#[test]
fn object_ref_as_ptr_roundtrip() {
    let ptr = 0xDEAD_BEE0_u64 as *mut u8;
    let obj = unsafe { ObjectRef::from_raw(ptr) };
    assert_eq!(obj.as_ptr(), ptr);
}

// ---- single-OS-thread Send/Sync tripwire ----

#[test]
fn current_thread_token_is_nonzero_and_stable() {
    // Must never collide with the SINGLE_THREAD_GUARD_UNSET sentinel,
    // otherwise a freshly-claimed guard would look unclaimed.
    let a = current_thread_token();
    let b = current_thread_token();
    assert_ne!(a, SINGLE_THREAD_GUARD_UNSET);
    assert_eq!(a, b, "token must be stable for the same OS thread");
}

#[test]
fn single_thread_guard_first_claim_then_same_thread_ok() {
    // A fresh guard cell: first claim succeeds, repeat from the same token
    // succeeds (no violation), exercising the cooperative-scheduler case.
    let guard = AtomicU64::new(SINGLE_THREAD_GUARD_UNSET);
    let token = current_thread_token();
    assert_eq!(check_single_thread_against(&guard, token), Ok(()));
    assert_eq!(check_single_thread_against(&guard, token), Ok(()));
    // The owner is now recorded as our token.
    assert_eq!(guard.load(Ordering::Acquire), token);
}

#[test]
fn single_thread_guard_detects_second_thread() {
    // Simulate a second OS thread by claiming the guard with one token,
    // then probing with a different one — must report a violation that
    // hands back the recorded owner.
    let guard = AtomicU64::new(SINGLE_THREAD_GUARD_UNSET);
    let first = 0xAAAA_AAAA_AAAA_AAA1_u64; // low bit set, like real tokens
    let second = 0xBBBB_BBBB_BBBB_BBB1_u64;
    assert_eq!(check_single_thread_against(&guard, first), Ok(()));
    assert_eq!(check_single_thread_against(&guard, second), Err(first));
    // Recorded owner is unchanged by a failed probe.
    assert_eq!(guard.load(Ordering::Acquire), first);
}

#[test]
fn single_thread_guard_concurrent_claim_loses_one() {
    // Two threads racing to construct the first ObjectRef: exactly one
    // wins the CAS; the other observes the winner's token and (since the
    // tokens differ) would trip the tripwire. This proves the guard does
    // not silently accept a genuine second OS thread.
    let guard = std::sync::Arc::new(AtomicU64::new(SINGLE_THREAD_GUARD_UNSET));
    let g2 = std::sync::Arc::clone(&guard);
    let token_a = 0x1111_1111_1111_1111_u64;
    let token_b = 0x2222_2222_2222_2223_u64;
    let handle = std::thread::spawn(move || check_single_thread_against(&g2, token_b));
    let r_a = check_single_thread_against(&guard, token_a);
    let r_b = handle.join().unwrap();
    // Exactly one of the two distinct tokens wins the claim.
    let recorded = guard.load(Ordering::Acquire);
    assert!(recorded == token_a || recorded == token_b);
    // The winner sees Ok, the loser sees Err(recorded-winner).
    let oks = [r_a, r_b].iter().filter(|r| r.is_ok()).count();
    assert_eq!(oks, 1, "exactly one claimant may win");
    let errs = [r_a, r_b];
    let err = errs.iter().find(|r| r.is_err()).unwrap();
    assert_eq!(*err, Err(recorded));
}

// ------------------------------------------------------------------
// RawSlot — untagged, verifier-map-driven slots
// ------------------------------------------------------------------

/// The property that motivates the whole design. `CompactValue::int(0)` and
/// `CompactValue::long(0xFFFC_0000_0000_0000)` are bit-identical, so a
/// tagged 8-byte slot cannot tell them apart without a side array. Untagged
/// slots carry no tag bits at all, so they are *supposed* to be
/// indistinguishable — and both decode correctly once the type comes from
/// the caller.
#[test]
fn rawslot_int_and_colliding_long_share_bits_but_decode_correctly() {
    let collide: i64 = 0xFFFC_0000_0000_0000u64 as i64;

    let as_long = RawSlot::from_long(collide);
    let as_int = RawSlot::from_int(0);

    // The bit patterns genuinely differ here (a long keeps all 64 bits, an
    // int zero-extends 32), which is already better than the NaN-boxed
    // case — but that is incidental. What matters is the next assertion.
    assert_eq!(as_long.bits(), 0xFFFC_0000_0000_0000);
    assert_eq!(as_int.bits(), 0);

    // With the type supplied externally, both round-trip exactly.
    assert_eq!(as_long.decode(SlotType::Long), Value::Long(collide));
    assert_eq!(as_int.decode(SlotType::Int), Value::Int(0));
}

/// The stronger statement: for EVERY `i64`, the untagged bits are the value
/// verbatim, so no long can ever be mistaken for anything. This is exactly
/// what `CompactValue` cannot promise, and it is why the `kinds` side array
/// becomes removable.
#[test]
fn rawslot_long_is_bit_exact_for_every_tag_pattern() {
    let hostile: [i64; 12] = [
        0,
        -1,
        i64::MIN,
        i64::MAX,
        0xFFFC_0000_0000_0000u64 as i64, // the NaN-box marker pattern
        0xFFFD_0000_0000_0000u64 as i64, // BouncyCastle LongArray lxor shape
        0xFFFE_0000_0000_0000u64 as i64,
        0xFFFF_FFFF_FFFF_FFF8u64 as i64,
        0x7FF8_0000_0000_0000u64 as i64, // canonical quiet NaN bits
        0x0000_5555_7777_8000u64 as i64, // looks exactly like a heap pointer
        1,
        -1234567890123456789,
    ];
    for v in hostile {
        let s = RawSlot::from_long(v);
        assert_eq!(s.bits(), v as u64, "long {v:#x} did not store verbatim");
        assert_eq!(s.as_long(), v);
        assert_eq!(s.decode(SlotType::Long), Value::Long(v));
    }
}

/// Every primitive slot type round-trips through `encode`/`decode`.
#[test]
fn rawslot_encode_decode_round_trip() {
    let cases = [
        Value::Int(0),
        Value::Int(-1),
        Value::Int(i32::MIN),
        Value::Int(i32::MAX),
        Value::Long(0),
        Value::Long(i64::MIN),
        Value::Float(0.0),
        Value::Float(-1.5),
        Value::Float(f32::MIN),
        Value::Double(0.0),
        Value::Double(3.141_592_653_589_793),
        Value::Double(f64::MAX),
        Value::Object(None),
        Value::ReturnAddress(0),
        Value::ReturnAddress(u32::MAX),
        Value::Uninitialized,
    ];
    for v in cases {
        let (slot, ty) = RawSlot::encode(v);
        assert_eq!(slot.decode(ty), v, "{v:?} did not round-trip");
    }
}

/// `RawSlot` stores a double verbatim — including NaN payloads that
/// `CompactValue::double` is forced to canonicalise into the hardware quiet
/// NaN because they collide with its tag space.
#[test]
fn rawslot_preserves_nan_payloads_compactvalue_must_canonicalise() {
    // A NaN whose bits set the NaN-box marker; CompactValue rewrites it.
    let exotic = f64::from_bits(0xFFFC_0000_0000_0001);
    assert!(exotic.is_nan());

    let compact = crate::compact_value::CompactValue::double(exotic);
    assert_ne!(
        compact.raw_bits(),
        0xFFFC_0000_0000_0001,
        "CompactValue is expected to canonicalise this NaN"
    );

    let raw = RawSlot::from_double(exotic);
    assert_eq!(raw.bits(), 0xFFFC_0000_0000_0001);
    assert!(raw.as_double().is_nan());
}

/// A reference slot holds a bare pointer; `oop()` is a plain read and
/// `set_oop` a plain store, which is what lets a moving collector relocate a
/// root without rebuilding tag bits.
#[test]
fn rawslot_reference_is_a_bare_pointer() {
    let addr = 0x0000_5555_7777_8000u64;
    let mut slot = RawSlot::from_oop(addr);
    assert_eq!(slot.bits(), addr, "reference must NOT be NaN-boxed");
    assert_eq!(slot.oop(), Some(addr));

    // Relocation.
    let moved = 0x0000_5555_7777_9000u64;
    slot.set_oop(moved);
    assert_eq!(slot.oop(), Some(moved));
    assert_eq!(slot.bits(), moved);

    // Null.
    let null = RawSlot::from_oop(0);
    assert_eq!(null.oop(), None);
    assert_eq!(null.decode(SlotType::Reference), Value::Object(None));
    assert_eq!(RawSlot::ZERO, null);
}

/// `decode(Reference)` must never fabricate an `ObjectRef` from bits that
/// cannot be a heap address, even though the verifier map claimed the slot
/// is a reference — a corrupt frame degrades to null instead of producing a
/// wild pointer that the caller would dereference.
#[test]
fn rawslot_decode_reference_degrades_implausible_bits() {
    let _guard = crate::compact_value::degrade_counter_test_lock();
    for bad in [1u64, 4, 0xFFF, (1u64 << 47) | 8] {
        let slot = RawSlot::from_bits(bad);
        assert_eq!(
            slot.decode(SlotType::Reference),
            Value::Object(None),
            "implausible pointer {bad:#x} must degrade to null"
        );
    }
}

/// Unlike `decode_value`, the untagged reference decode does NOT consult the
/// provenance bitmap: a pointer that never crossed `ObjectRef::from_raw` is
/// still decoded, because the verifier map — not a heuristic — is the
/// authority. Dropping that probe is the throughput argument for untagged
/// slots, so pin it.
#[test]
fn rawslot_decode_reference_needs_no_provenance_record() {
    let _guard = crate::compact_value::degrade_counter_test_lock();
    // Deliberately an address this process has never constructed an
    // ObjectRef for. Plausible (aligned, above the guard page, < 2^47) but
    // unknown to the provenance bitmap.
    let never_seen = 0x0000_4242_4242_4000u64;
    assert!(
        !object_ref_payload_is_known(never_seen),
        "test precondition: this address must be unrecorded"
    );

    // The tagged SoA decoder rejects it...
    assert_eq!(
        decode_value(never_seen, VTAG_OBJECT),
        Value::Object(None),
        "decode_value is expected to reject an unrecorded pointer"
    );

    // ...the untagged decoder accepts it, on the verifier's authority.
    match RawSlot::from_oop(never_seen).decode(SlotType::Reference) {
        Value::Object(Some(r)) => assert_eq!(r.as_ptr() as u64, never_seen),
        other => panic!("expected an object reference, got {other:?}"),
    }
    assert_eq!(RawSlot::from_oop(never_seen).oop(), Some(never_seen));
}

/// The migration bridge: a frame can be half-converted, so `RawSlot` and
/// `CompactValue` must agree on every value they can both represent.
#[test]
fn rawslot_compact_value_bridge_round_trips() {
    let cases: [(RawSlot, SlotType); 9] = [
        (RawSlot::from_int(-7), SlotType::Int),
        (RawSlot::from_int(i32::MIN), SlotType::Int),
        (RawSlot::from_long(i64::MIN), SlotType::Long),
        (
            RawSlot::from_long(0xFFFC_0000_0000_0000u64 as i64),
            SlotType::Long,
        ),
        (RawSlot::from_float(-2.5), SlotType::Float),
        (RawSlot::from_double(1.25), SlotType::Double),
        (RawSlot::from_oop(0), SlotType::Reference),
        (RawSlot::from_return_address(99), SlotType::ReturnAddress),
        (RawSlot::ZERO, SlotType::Uninitialized),
    ];
    for (slot, ty) in cases {
        let compact = slot.to_compact(ty);
        let back = RawSlot::from_compact(compact, ty);
        assert_eq!(back, slot, "{slot:?} as {ty:?} did not survive the bridge");
    }

    // A non-null reference needs a plausible address for CompactValue's
    // (release-active) constructor assertions.
    let addr = 0x0000_5555_7777_8000u64;
    let slot = RawSlot::from_oop(addr);
    let compact = slot.to_compact(SlotType::Reference);
    assert_eq!(compact.as_object_ptr(), Some(addr));
    assert_eq!(RawSlot::from_compact(compact, SlotType::Reference), slot);
}

/// `RawSlot` must stay layout-identical to `u64` / `CompactValue` so the
/// existing frame-pool `Vec<u64>` buffers can back untagged frames and the
/// three GC scanners that transmute `Vec<u64> <-> Vec<CompactValue>` keep
/// working during the migration.
#[test]
fn rawslot_is_layout_compatible_with_u64_and_compact_value() {
    use crate::compact_value::CompactValue;
    assert_eq!(std::mem::size_of::<RawSlot>(), std::mem::size_of::<u64>());
    assert_eq!(std::mem::align_of::<RawSlot>(), std::mem::align_of::<u64>());
    assert_eq!(
        std::mem::size_of::<RawSlot>(),
        std::mem::size_of::<CompactValue>()
    );

    let v: [RawSlot; 2] = [RawSlot::from_long(-1), RawSlot::from_int(5)];
    // SAFETY: RawSlot is repr(transparent) over u64 with identical size and
    // alignment, so a slice of one reinterprets as a slice of the other —
    // the same property the frame pool relies on for Vec<u64> reuse.
    let as_u64: &[u64] =
        unsafe { std::slice::from_raw_parts(v.as_ptr().cast::<u64>(), v.len()) };
    assert_eq!(as_u64, &[u64::MAX, 5]);
}

/// Descriptor-driven decode, the call/return-boundary type source.
#[test]
fn rawslot_decode_by_descriptor() {
    assert_eq!(
        RawSlot::from_int(1).decode_by_descriptor(b'Z'),
        Value::Int(1)
    );
    assert_eq!(
        RawSlot::from_int(-1).decode_by_descriptor(b'I'),
        Value::Int(-1)
    );
    assert_eq!(
        RawSlot::from_long(i64::MIN).decode_by_descriptor(b'J'),
        Value::Long(i64::MIN)
    );
    assert_eq!(
        RawSlot::from_double(0.5).decode_by_descriptor(b'D'),
        Value::Double(0.5)
    );
    assert_eq!(
        RawSlot::from_oop(0).decode_by_descriptor(b'L'),
        Value::Object(None)
    );
    assert_eq!(
        RawSlot::from_oop(0).decode_by_descriptor(b'['),
        Value::Object(None)
    );
    // `V` and junk are not slot types.
    assert_eq!(
        RawSlot::ZERO.decode_by_descriptor(b'V'),
        Value::Uninitialized
    );
    assert_eq!(
        RawSlot::ZERO.decode_by_descriptor(b'?'),
        Value::Uninitialized
    );
}

#[test]
fn slot_type_classification() {
    assert!(SlotType::Reference.is_reference());
    assert!(!SlotType::Long.is_reference());
    assert!(SlotType::Long.is_category2());
    assert!(SlotType::Double.is_category2());
    assert!(!SlotType::Int.is_category2());
    assert!(!SlotType::Reference.is_category2());

    assert_eq!(SlotType::from_descriptor_byte(b'J'), Some(SlotType::Long));
    assert_eq!(SlotType::from_descriptor_byte(b'C'), Some(SlotType::Int));
    assert_eq!(
        SlotType::from_descriptor_byte(b'['),
        Some(SlotType::Reference)
    );
    assert_eq!(SlotType::from_descriptor_byte(b'V'), None);
}

/// A `RawSlot` is 8 bytes in both narrow-oop configurations: frame slots are
/// not heap slots and are never narrowed. Pins the interaction so a future
/// compressed-oops change cannot silently narrow the interpreter's frames.
#[test]
fn rawslot_width_is_independent_of_compressed_oops() {
    assert_eq!(std::mem::size_of::<RawSlot>(), 8);
    // Heap reference slots do vary; frame slots must not.
    assert!(
        crate::narrow_oop::ref_field_size() == 4 || crate::narrow_oop::ref_field_size() == 8
    );
    let addr = 0x0000_5555_7777_8000u64;
    assert_eq!(
        RawSlot::from_oop(addr).oop(),
        Some(addr),
        "RawSlot holds a decoded 64-bit address regardless of narrow oops"
    );
}

#[test]
fn send_sync_invariant_layout_holds() {
    // The Send/Sync impls assume ObjectRef stays a bare pointer-sized
    // value (no added synchronization state). Re-assert it here so a field
    // addition that would invalidate the soundness argument is caught.
    assert_eq!(
        std::mem::size_of::<ObjectRef>(),
        std::mem::size_of::<*mut u8>()
    );
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<ObjectRef>();
}

// -----------------------------------------------------------------------
// `ObjectRef` concurrency contract
// (docs/threading/objectref-concurrency-contract.md)
//
// These pin the parts of the contract that are checkable from inside
// `types`. The parts that are NOT — root coverage, permitted GC phase,
// staleness across safepoints and blocking regions — live in `vm`/`gc` and
// are listed as unenforced invariants in §7 of the doc.
// -----------------------------------------------------------------------

fn assert_send<T: Send>() {}
fn assert_sync<T: Sync>() {}

/// §6 of the contract: `Send` and `Sync` are asserted separately, and they
/// must survive the compositions the VM actually relies on to move roots
/// between OS threads (`Arc<Mutex<Vec<ObjectRef>>>` is literally
/// `ThreadRegistry`'s `root_snapshot`).
#[test]
fn objectref_send_and_sync_bounds_hold_for_the_shapes_the_vm_uses() {
    assert_send::<ObjectRef>();
    assert_sync::<ObjectRef>();
    assert_send::<Option<ObjectRef>>();
    assert_sync::<Option<ObjectRef>>();
    assert_send::<Value>();
    assert_sync::<Value>();
    assert_send::<Vec<ObjectRef>>();
    assert_sync::<Vec<ObjectRef>>();
    // The cross-thread root-publication shape.
    assert_send::<std::sync::Arc<std::sync::Mutex<Vec<ObjectRef>>>>();
    assert_sync::<std::sync::Arc<std::sync::Mutex<Vec<ObjectRef>>>>();
    // The pointer-map rewrite shape.
    assert_send::<std::collections::HashMap<ObjectRef, usize>>();
}

/// §2: copying an `ObjectRef` is a bit copy. Both copies name the same
/// address, compare equal, hash equal, and neither is invalidated by the
/// other — which is exactly why `Send` is sound (nothing is transferred,
/// unshared or dropped).
#[test]
fn objectref_copy_semantics_are_a_pure_bit_copy() {
    let addr = 0x0000_5555_7777_9000usize;
    // SAFETY: aligned, non-null sentinel address; never dereferenced here.
    let a = unsafe { ObjectRef::from_raw(addr as *mut u8) };
    let b = a; // Copy, not a move-out.
    assert_eq!(a, b);
    assert_eq!(a.as_ptr(), b.as_ptr());
    assert_eq!(a.as_nonnull(), b.as_nonnull());
    assert_eq!(a.as_ptr() as usize, addr);
    // `a` is still usable after `b` was created — no move occurred.
    assert_eq!(a.as_ptr() as usize, addr);

    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let hash_of = |r: ObjectRef| {
        let mut h = DefaultHasher::new();
        r.hash(&mut h);
        h.finish()
    };
    assert_eq!(hash_of(a), hash_of(b), "Hash must follow address equality");

    // Address identity, not object identity: a DIFFERENT address is a
    // different `ObjectRef` even if a moving collection would call them
    // the same object. This is unenforced-invariant #3 in the doc.
    // SAFETY: aligned, non-null sentinel address; never dereferenced.
    let moved = unsafe { ObjectRef::from_raw((addr + 0x1000) as *mut u8) };
    assert_ne!(a, moved);
    assert_ne!(hash_of(a), hash_of(moved));
}

/// §2: `None` is the all-zero niche. This is what lets `Value` stay 16
/// bytes, and it is why a torn/zeroed 16-byte slot decodes as `Int(0)`
/// rather than as a null reference (see `read_slot`'s R-niche rule in
/// `gc/src/heap.rs`).
#[test]
fn objectref_none_is_the_all_zero_niche() {
    assert_eq!(
        std::mem::size_of::<Option<ObjectRef>>(),
        std::mem::size_of::<ObjectRef>()
    );
    // SAFETY: both types are pointer-sized (asserted above and at compile
    // time); reading `None`'s own initialized bytes as a `usize`.
    let none_bits: usize = unsafe { std::mem::transmute(None::<ObjectRef>) };
    assert_eq!(none_bits, 0, "None must be the zero bit pattern");

    // SAFETY: aligned, non-null sentinel address; never dereferenced.
    let some = Some(unsafe { ObjectRef::from_raw(0x0000_5555_7777_A000usize as *mut u8) });
    // SAFETY: same size/validity argument as above.
    let some_bits: usize = unsafe { std::mem::transmute(some) };
    assert_eq!(some_bits, 0x0000_5555_7777_A000);

    assert!(Value::Object(None).is_null());
    assert!(!Value::Object(some).is_null());
}

/// The local half of "is this reference plausible?". Reports rather than
/// panics precisely because the workspace mints out-of-range sentinel refs
/// — pinning that behaviour so a future hard assert in `as_ptr` is a
/// deliberate decision, not an accident.
#[test]
fn objectref_plausibility_reports_and_does_not_panic() {
    // A realistic heap address.
    // SAFETY: aligned, non-null; never dereferenced.
    let ok = unsafe { ObjectRef::from_raw(0x0000_5555_7777_B000usize as *mut u8) };
    assert!(ok.is_plausible_heap_pointer());

    // A sentinel above the 47-bit user-address window, of the shape the
    // workspace actually uses for synthetic identities. `as_ptr` must NOT
    // panic on it (debug builds included) — only the report says "no".
    let sentinel = 0xdead_beef_0000_0100usize;
    assert_eq!(sentinel % 8, 0, "sentinel must still be 8-byte aligned");
    // SAFETY: aligned, non-null; never dereferenced.
    let odd = unsafe { ObjectRef::from_raw(sentinel as *mut u8) };
    assert_eq!(odd.as_ptr() as usize, sentinel);
    assert!(!odd.is_plausible_heap_pointer());

    // And the null-guard page is rejected by the same predicate.
    assert!(!plausible_heap_pointer(0x8));
    assert!(!plausible_heap_pointer(0));
}

/// §2 layout invariant, checked at runtime as well as at compile time so a
/// `cfg`-dependent field addition cannot slip past the `const _` asserts on
/// one target only.
#[test]
fn objectref_layout_is_pinned_for_size_and_alignment() {
    assert_eq!(
        std::mem::size_of::<ObjectRef>(),
        std::mem::size_of::<*mut u8>()
    );
    assert_eq!(
        std::mem::align_of::<ObjectRef>(),
        std::mem::align_of::<*mut u8>()
    );
    assert_eq!(
        std::mem::align_of::<Option<ObjectRef>>(),
        std::mem::align_of::<*mut u8>()
    );
    assert_eq!(std::mem::size_of::<Value>(), 16);
    assert!(std::mem::align_of::<Value>() <= 8);
}

/// §6 (1): `ObjectRef` must stay `Drop`-free. A `Drop` impl would make the
/// `Send` argument ("sending transfers a bit pattern; nothing is
/// deallocated") false, and would also break `Copy`.
#[test]
fn objectref_has_no_drop_glue() {
    assert!(
        !std::mem::needs_drop::<ObjectRef>(),
        "ObjectRef must have no drop glue — see contract doc §6(1)"
    );
    assert!(!std::mem::needs_drop::<Option<ObjectRef>>());
    assert!(!std::mem::needs_drop::<Value>());
}

/// The contract explicitly permits copying an `ObjectRef` to another OS
/// thread and reading it there. Exercised for real (not just via trait
/// bounds) so the `Send`/`Sync` claim is checked by execution too.
#[test]
fn objectref_survives_a_real_cross_thread_transfer() {
    let addr = 0x0000_5555_7777_C000usize;
    // SAFETY: aligned, non-null sentinel address; never dereferenced.
    let r = unsafe { ObjectRef::from_raw(addr as *mut u8) };

    // Send: move a copy into another OS thread.
    let moved = std::thread::spawn(move || r.as_ptr() as usize)
        .join()
        .expect("worker thread panicked");
    assert_eq!(moved, addr);

    // Sync: share `&ObjectRef` across threads.
    let shared = std::sync::Arc::new(r);
    let mut handles = Vec::new();
    for _ in 0..4 {
        let s = std::sync::Arc::clone(&shared);
        handles.push(std::thread::spawn(move || s.as_ptr() as usize));
    }
    for h in handles {
        assert_eq!(h.join().expect("worker thread panicked"), addr);
    }
}
