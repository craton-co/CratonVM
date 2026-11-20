use super::*;

use crate::class_id::ClassId;
use crate::heap_types::{ArrayElementType, ObjectKind, HEADER_SIZE};

static REGISTRY_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// A generation move retires a scan-cache slot without clearing or
/// re-filling the table (i89: a define used to cost every thread an
/// `O(class ids)` clear and refill), and a slot still answers only at the
/// exact generation it was resolved at, high half included.
#[test]
fn scan_cache_slots_carry_their_own_generation() {
    assert_eq!(std::mem::size_of::<ScanEntry>(), 16);
    let mut cache = ScanCache::new();
    let g = 7u64;
    assert_eq!(cache.install(4000, 3, g, None), Some(4000));
    assert_eq!(cache.entries.len(), 4001);
    assert!(cache.find(4000, 3, g).is_some());
    assert!(cache.find(4000, 2, g).is_none());
    // A registration elsewhere moves the generation: the old slot misses,
    // and installing a lower id neither clears nor shrinks the table.
    assert!(cache.find(4000, 3, g + 1).is_none());
    assert_eq!(cache.install(10, 1, g + 1, None), Some(10));
    assert_eq!(cache.entries.len(), 4001);
    assert!(cache.find(10, 1, g + 1).is_some());
    assert!(cache.find(4000, 3, g + 1).is_none());
    assert_eq!(cache.install(4000, 3, g + 1, None), Some(4000));
    assert!(cache.find(4000, 3, g + 1).is_some());
    // Same low half, different high half: never a hit, and the install
    // that crosses it starts the table over.
    let far = (g + 1) + (1u64 << 32);
    assert!(cache.find(4000, 3, far).is_none());
    assert_eq!(cache.install(5, 1, far, None), Some(5));
    assert_eq!(cache.entries.len(), 6);
    assert!(cache.find(4000, 3, far).is_none());
    assert!(cache.find(4000, 3, g + 1).is_none());
    // Past the bound nothing is cached.
    assert_eq!(cache.install(SCAN_CACHE_MAX as u32, 1, far, None), None);
}

/// The replacement epoch a JIT site bakes must be the HEAP cell, not the
/// one in this image's `.data`.
///
/// Two things rest on that and neither would fail a behaviour test if a
/// refactor "simplified" the `LazyLock` back into a plain `static`:
///
/// * `CMP dword [rip+disp32], imm32` needs the counter within ±2GB of the
///   code, and as a `static` it sat ~140TB away — every guard would
///   silently return to three instructions and nineteen bytes;
/// * `jit::platform`'s `near_globals` ANCHORS its code-placement hint on
///   this address, and its whole premise is that the safepoint flag is a
///   few hundred megabytes from it because both come from the same
///   allocator. A counter in `.data`, or in its own `mmap` page, aims the
///   code cache somewhere the flag is not.
///
/// So the default arm is pinned here. (The `.data` diagnosis arm,
/// `CRATONVM_JIT_LAYOUT_EPOCH_STATIC`, was deleted in JIT round 14 wave 2.)
///
/// **2026-09-11: the second bullet has expired and the first has grown a
/// better answer.** `near_globals` no longer needs this cell as an anchor
/// for the poll — the poll has a cell of its own — and on a real VM boot
/// this counter is not in the heap at all but in one from the code cache's
/// allocator ([`install_layout_epoch_cell`]). What this test still pins is
/// the arm a process with no VM in it gets, which is every unit test: no
/// caller installed anything, so the `LazyLock` must fall back to the
/// leaked `Box` and must NOT quietly become the `.data` static. The
/// installed arm is pinned by
/// `an_installed_cell_is_what_a_jit_site_bakes` below.
#[test]
fn the_baked_epoch_address_is_the_heap_cell_by_default() {
    let (addr, _) = layout_replace_epoch_guard();
    assert!(!addr.is_null(), "a JIT site has an address to bake");
    assert_eq!(
        layout_epoch_cell_origin(),
        EpochCellOrigin::Heap,
        "no cell is installed in a unit test: the guarded counter is the leaked heap cell",
    );
    // JIT code reads it as one aligned 32-bit load; nothing else makes
    // that as atomic as the `MOV ECX` it replaced.
    assert_eq!(addr as usize % 4, 0, "the counter is 4-byte aligned");
    // And it is STABLE, because compiled code bakes it.
    let (again, _) = layout_replace_epoch_guard();
    assert_eq!(
        addr, again,
        "the baked address may not move under a compile"
    );
}

/// `HIB-DCAST-LATEPHASE.1`: a legacy (non-compact) object whose `shape`
/// field holds an implausible `num_slots` (e.g. from a GC walk that
/// desynced onto payload bytes and decoded them as a header) must not be
/// sized as `num_slots * SLOT_SIZE` — for tens of millions of "slots"
/// that is still small enough to fit inside a large arena, so a walker
/// deriving the object's extent from this value accepts it as valid, and
/// a later reference-slot scan strides through all of them into unmapped
/// memory. Asserts the cap fires and that adding `HEADER_SIZE` to the
/// result cannot overflow (every caller does exactly that).
#[test]
fn object_body_size_caps_an_implausible_legacy_num_slots() {
    let header = ObjectHeader::new(
        ClassId::new(0),
        ObjectKind::Object,
        ArrayElementType::Reference,
        0,
        33_000_000, // far past the 1<<24 (~16.7M) cap
    );
    assert_eq!(object_instance_size(&header), IMPLAUSIBLE_BODY_SIZE);
    assert!(
        HEADER_SIZE
            .checked_add(object_instance_size(&header))
            .is_some(),
        "HEADER_SIZE + object_instance_size(..) must not overflow"
    );
}

/// A plausible legacy `num_slots` must still size exactly as before —
/// the cap must not touch real objects.
#[test]
fn object_body_size_is_unaffected_below_the_cap() {
    let header = ObjectHeader::new(
        ClassId::new(0),
        ObjectKind::Object,
        ArrayElementType::Reference,
        0,
        4,
    );
    assert_eq!(object_instance_size(&header), HEADER_SIZE + 4 * SLOT_SIZE);
}

/// `HIB-DCAST-LATEPHASE.1`: a compact object (`GC_FLAG_COMPACT` set)
/// whose class layout is not registered for its `(class_id,
/// field_count)` must NOT be reported as a confident zero-byte body.
/// The old `.unwrap_or(0)` made `HEADER_SIZE + object_instance_size(..)`
/// come out to exactly `HEADER_SIZE`, which passes every caller's `total
/// < HEADER_SIZE` corruption check — so `OldGen::scan_region`'s walk
/// credited such an object only its header and advanced into the middle
/// of its real, larger body, decoding live field bytes as a brand-new
/// (bogus) header. Asserts the fallback is the same "impossibly large,
/// no arena can contain it" sentinel the legacy branch's cap uses.
#[test]
fn object_body_size_refuses_to_guess_zero_for_an_unresolved_compact_layout() {
    let mut header = ObjectHeader::new(
        ClassId::new(999_999), // no layout ever registered for this id
        ObjectKind::Object,
        ArrayElementType::Reference,
        0,
        4,
    );
    header.add_gc_flags(GC_FLAG_COMPACT);
    assert_eq!(object_instance_size(&header), IMPLAUSIBLE_BODY_SIZE);
    assert_ne!(
        HEADER_SIZE + object_instance_size(&header),
        HEADER_SIZE,
        "must not report a total that equals exactly HEADER_SIZE -- that \
         passes every caller's `total < HEADER_SIZE` corruption check"
    );
}

#[test]
fn layout_offsets_and_oopmap() {
    // Class with fields: ref, int, ref  (descriptors L, I, L)
    // natural-width layout: ref@0, int@8, align the next ref to ref@16.
    let layout = CompactLayout {
        field_disps: vec![0, 8, 16],
        is_ref: vec![true, false, true],
        field_kinds: vec![
            FieldStorageKind::Reference,
            FieldStorageKind::Int,
            FieldStorageKind::Reference,
        ],
        ref_disps: vec![0, 16],
        total_size: 24,
    };
    assert_eq!(layout.field_disp(0), Some(0));
    assert_eq!(layout.field_disp(1), Some(8));
    assert_eq!(layout.field_disp(2), Some(16));
    assert_eq!(layout.field_disp(3), None);
    assert_eq!(layout.field_is_ref(1), Some(false));
    assert_eq!(layout.field_count(), 3);
    assert_eq!(layout.ref_disps, vec![0, 16]);
}

/// Regression: mixed reference + `double`/`long`/`float` fields.
///
/// The compact layout stores a reference field as a bare 8-byte pointer but
/// keeps every primitive field (including `double`/`long`) as a 16-byte
/// tagged `Value` cell. A primitive's byte offset therefore shifts by only
/// **8** bytes per preceding reference field — NOT the uniform
/// `index * SLOT_SIZE` the non-compact layout assumes. Reading a `double`
/// with the non-compact assumption lands it on (or overlapping) a reference
/// slot and pushes a pointer where a `double` is expected — the
/// "expected double on stack, got ref(..)" class of failure this test
/// guards against (compact-ref SIGSEGV follow-up, `docs/feature-designs/
/// compact-ref-field-layout.md`).
#[test]
fn mixed_ref_double_long_float_offsets() {
    // Fields (declaration order): Object a; double x; Object b; double y;
    //                             long z;   Object c; float f
    // descriptors:                L        D         L        D
    //                             J        L         F
    // prefix-sum (ref=8, prim=16):
    //   a  L  off 0  (ref, +8)  -> 8
    //   x  D  off 8  (prim,+16) -> 24
    //   b  L  off 24 (ref, +8)  -> 32
    //   y  D  off 32 (prim,+16) -> 48
    //   z  J  off 48 (prim,+16) -> 64
    //   c  L  off 64 (ref, +8)  -> 72
    //   f  F  off 72 (prim,+16) -> 88
    let layout = CompactLayout {
        field_disps: vec![0, 8, 16, 24, 32, 40, 48],
        is_ref: vec![true, false, true, false, false, true, false],
        field_kinds: vec![
            FieldStorageKind::Reference,
            FieldStorageKind::Double,
            FieldStorageKind::Reference,
            FieldStorageKind::Double,
            FieldStorageKind::Long,
            FieldStorageKind::Reference,
            FieldStorageKind::Float,
        ],
        ref_disps: vec![0, 16, 40],
        total_size: 56,
    };

    // Cross-check the literal layout against the prefix-sum rule it encodes,
    // so a change to REF_FIELD_SIZE / SLOT_SIZE (or a mis-sized field) is
    // caught here rather than corrupting objects at runtime.
    let kinds = [
        FieldStorageKind::Reference,
        FieldStorageKind::Double,
        FieldStorageKind::Reference,
        FieldStorageKind::Double,
        FieldStorageKind::Long,
        FieldStorageKind::Reference,
        FieldStorageKind::Float,
    ];
    let mut expect_off = 0u32;
    let mut expect_refs = Vec::new();
    for (i, &kind) in kinds.iter().enumerate() {
        let align = kind.alignment();
        expect_off = (expect_off + align - 1) & !(align - 1);
        assert_eq!(
            layout.field_disp(i),
            Some(expect_off),
            "field {i} must be naturally aligned",
        );
        assert_eq!(
            layout.field_is_ref(i),
            Some(kind.is_reference()),
            "field {i} ref-ness"
        );
        if kind.is_reference() {
            expect_refs.push(expect_off);
        }
        expect_off += kind.size();
    }
    expect_off = (expect_off + 7) & !7;
    assert_eq!(
        layout.total_size, expect_off,
        "body size = sum of field sizes"
    );
    assert_eq!(layout.ref_disps, expect_refs, "oop-map = reference offsets");
    assert_eq!(layout.field_count(), 7);
    assert_eq!(layout.field_disp(7), None, "out-of-range index");

    // The crux of the bug: each `double` sits at its ref-shifted offset, and
    // its 16-byte cell never overlaps a reference slot. If the double were
    // read at the non-compact `index * SLOT_SIZE` offset it would land on a
    // ref slot and decode a pointer as a double.
    assert_eq!(layout.field_disp(1), Some(8)); // double x, one ref before
    assert_ne!(
        8,
        1 * SLOT_SIZE as u32,
        "non-compact assumption would use 16"
    );
    assert_eq!(layout.field_disp(3), Some(24)); // double y, two refs before
    assert_ne!(
        24,
        3 * SLOT_SIZE as u32,
        "non-compact assumption would use 48"
    );

    // No primitive field's [off, off+SLOT_SIZE) byte range may overlap any
    // reference field's [off, off+REF_FIELD_SIZE) range — that overlap is
    // exactly how a double read yields a ref's bytes.
    for (i, &kind) in kinds.iter().enumerate() {
        if kind.is_reference() {
            continue;
        }
        let p0 = layout.field_disp(i).unwrap();
        let p1 = p0 + kind.size();
        for &ro in &layout.ref_disps {
            let r1 = ro + FieldStorageKind::Reference.size();
            assert!(
                p1 <= ro || r1 <= p0,
                "primitive field {i} at [{p0},{p1}) overlaps ref slot [{ro},{r1})",
            );
        }
    }
}

#[test]
fn tagless_field_round_trip_all_storage_kinds() {
    let mut words = [0u64; 8];
    let ptr = words.as_mut_ptr() as *mut u8;
    let cases = [
        (FieldStorageKind::Boolean, Value::Int(1)),
        (FieldStorageKind::Byte, Value::Int(-17)),
        (FieldStorageKind::Char, Value::Int(0x20ac)),
        (FieldStorageKind::Short, Value::Int(-1234)),
        (FieldStorageKind::Int, Value::Int(-123_456_789)),
        (FieldStorageKind::Float, Value::Float(3.25)),
        (FieldStorageKind::Long, Value::Long(-9_876_543_210)),
        (FieldStorageKind::Double, Value::Double(-17.5)),
    ];
    for (kind, value) in cases {
        unsafe {
            write_compact_field(ptr, kind, value, Ordering::Release);
            assert_eq!(read_compact_field(ptr, kind, Ordering::Acquire), value);
        }
    }
    unsafe {
        write_compact_field(
            ptr,
            FieldStorageKind::Reference,
            Value::Object(None),
            Ordering::Release,
        );
        assert_eq!(
            read_compact_field(ptr, FieldStorageKind::Reference, Ordering::Acquire),
            Value::Object(None)
        );
    }
}

#[test]
fn registry_round_trip() {
    let _guard = REGISTRY_TEST_LOCK.lock().unwrap();
    clear_class_layouts();
    let layout = Arc::new(CompactLayout {
        field_disps: vec![0],
        is_ref: vec![true],
        field_kinds: vec![FieldStorageKind::Reference],
        ref_disps: vec![0],
        total_size: 8,
    });
    register_class_layout(FIRST_LAYOUT_DOMAIN, 7, layout.clone());
    assert!(class_layout(0).is_none());
    let got = class_layout(7).expect("registered");
    assert_eq!(got.total_size, 8);
    let layout2 = Arc::new(CompactLayout {
        field_disps: vec![0, 8],
        is_ref: vec![true, true],
        field_kinds: vec![FieldStorageKind::Reference, FieldStorageKind::Reference],
        ref_disps: vec![0, 8],
        total_size: 16,
    });
    register_class_layout(FIRST_LAYOUT_DOMAIN, 7, layout2);
    assert_eq!(class_layout(7).unwrap().total_size, 16);
    assert_eq!(class_layout_for_fields(7, 1).unwrap().total_size, 8);
    assert_eq!(class_layout_for_fields(7, 2).unwrap().total_size, 16);
    clear_class_layouts();
}

// --- class_layout_for_fields thread-local cache invalidation -------------
//
// The cache is validated against `layout_generation()`. Every registry
// mutation must bump it, or a warmed thread would keep serving a stale
// layout — which for the GC scan means walking an object with the wrong
// oop-map (missed or fabricated reference slots => heap corruption). These
// tests pin each mutation path.

fn one_ref_layout(total_size: u32) -> Arc<CompactLayout> {
    Arc::new(CompactLayout {
        field_disps: vec![0],
        is_ref: vec![true],
        field_kinds: vec![FieldStorageKind::Reference],
        ref_disps: vec![0],
        total_size,
    })
}

/// Replacing a layout for the SAME (class_id, field_count) must be visible
/// to a thread that already cached the old one.
#[test]
fn version_cache_sees_replacement() {
    let _guard = REGISTRY_TEST_LOCK.lock().unwrap();
    clear_class_layouts();
    register_class_layout(FIRST_LAYOUT_DOMAIN, 21, one_ref_layout(8));
    // Warm the cache.
    assert_eq!(class_layout_for_fields(21, 1).unwrap().total_size, 8);
    // Replace with a different layout at the same field count.
    register_class_layout(FIRST_LAYOUT_DOMAIN, 21, one_ref_layout(24));
    assert_eq!(
        class_layout_for_fields(21, 1).unwrap().total_size,
        24,
        "cached entry survived a replacement — generation bump missing"
    );
    clear_class_layouts();
}

/// A cached NEGATIVE lookup must not hide a subsequent registration.
#[test]
fn version_cache_sees_registration_after_negative_lookup() {
    let _guard = REGISTRY_TEST_LOCK.lock().unwrap();
    clear_class_layouts();
    // Warm a negative entry.
    assert!(class_layout_for_fields(22, 1).is_none());
    register_class_layout(FIRST_LAYOUT_DOMAIN, 22, one_ref_layout(8));
    assert!(
        class_layout_for_fields(22, 1).is_some(),
        "cached negative lookup hid a later registration"
    );
    clear_class_layouts();
}

/// `unregister_class_layout` must invalidate a warmed positive entry —
/// otherwise an unloaded class's layout could be applied to a recycled id.
#[test]
fn version_cache_sees_unregister() {
    let _guard = REGISTRY_TEST_LOCK.lock().unwrap();
    clear_class_layouts();
    register_class_layout(FIRST_LAYOUT_DOMAIN, 23, one_ref_layout(8));
    assert!(class_layout_for_fields(23, 1).is_some());
    unregister_class_layout(23);
    assert!(
        class_layout_for_fields(23, 1).is_none(),
        "cached entry survived unregister_class_layout"
    );
    clear_class_layouts();
}

/// `clear_class_layouts` must invalidate too (it is used between tests, so
/// a surviving entry would cross-contaminate them).
#[test]
fn version_cache_sees_clear() {
    let _guard = REGISTRY_TEST_LOCK.lock().unwrap();
    clear_class_layouts();
    register_class_layout(FIRST_LAYOUT_DOMAIN, 24, one_ref_layout(8));
    assert!(class_layout_for_fields(24, 1).is_some());
    clear_class_layouts();
    assert!(
        class_layout_for_fields(24, 1).is_none(),
        "cached entry survived clear_class_layouts"
    );
}

/// Every thread must agree with the registry, and repeated lookups (which
/// exercise hits) must agree with the first (which was a miss). Also
/// exercises eviction: more distinct keys than the 8-entry cache holds.
#[test]
fn version_cache_consistent_across_threads_and_evictions() {
    let _guard = REGISTRY_TEST_LOCK.lock().unwrap();
    clear_class_layouts();
    // Ids 30..50 plus their `+ VERSION_CACHE_LEN` aliases share cache slots
    // (direct-mapped), so entries get evicted and re-resolved; each
    // total_size encodes its class id.
    let n = VERSION_CACHE_LEN as u32;
    let ids: Vec<u32> = (30..50u32).chain((30..50).map(|c| c + n)).collect();
    for &cid in &ids {
        register_class_layout(FIRST_LAYOUT_DOMAIN, cid, one_ref_layout(cid * 8));
    }
    let mut handles = Vec::new();
    for _ in 0..4 {
        let ids = ids.clone();
        handles.push(std::thread::spawn(move || {
            // Two passes: the second pass hits entries the first warmed.
            for _ in 0..2 {
                for &cid in &ids {
                    let got = class_layout_for_fields(cid, 1)
                        .expect("registered layout must resolve");
                    assert_eq!(
                        got.total_size,
                        cid * 8,
                        "cache returned another class's layout for {cid}"
                    );
                }
            }
        }));
    }
    for h in handles {
        h.join().expect("lookup worker should not panic");
    }
    clear_class_layouts();
}

/// `with_class_layout` must agree with `class_layout_for_fields` on both
/// the miss (first call) and hit (second call) paths, and must report a
/// missing layout as `None` rather than running `f`.
#[test]
fn with_class_layout_matches_owning_accessor() {
    let _guard = REGISTRY_TEST_LOCK.lock().unwrap();
    clear_class_layouts();
    register_class_layout(FIRST_LAYOUT_DOMAIN, 60, one_ref_layout(40));
    // First call is a cache miss (installs), second is a hit.
    for pass in 0..2 {
        assert_eq!(
            with_class_layout(60, 1, |l| l.total_size),
            Some(40),
            "pass {pass}"
        );
        assert_eq!(
            with_class_layout(60, 1, |l| l.ref_disps.clone()),
            Some(vec![0]),
            "pass {pass}"
        );
    }
    // Unregistered: `f` must not run.
    let mut ran = false;
    let got = with_class_layout(61, 1, |_| {
        ran = true;
    });
    assert!(got.is_none());
    assert!(!ran, "`f` ran for a class with no registered layout");
    clear_class_layouts();
}

/// Generation invalidation must apply to the borrowing accessor too — it
/// shares one cache with `class_layout_for_fields`, so a replacement that
/// the owning accessor sees must not be hidden from this one.
#[test]
fn with_class_layout_sees_replacement_and_unregister() {
    let _guard = REGISTRY_TEST_LOCK.lock().unwrap();
    clear_class_layouts();
    register_class_layout(FIRST_LAYOUT_DOMAIN, 62, one_ref_layout(8));
    assert_eq!(with_class_layout(62, 1, |l| l.total_size), Some(8));
    register_class_layout(FIRST_LAYOUT_DOMAIN, 62, one_ref_layout(24));
    assert_eq!(
        with_class_layout(62, 1, |l| l.total_size),
        Some(24),
        "borrowed entry survived a replacement — generation bump missing"
    );
    unregister_class_layout(62);
    assert_eq!(
        with_class_layout(62, 1, |l| l.total_size),
        None,
        "borrowed entry survived unregister_class_layout"
    );
    clear_class_layouts();
}

#[test]
fn current_layout_cache_invalidates_without_reusing_historical_recipe() {
    let _guard = REGISTRY_TEST_LOCK.lock().unwrap();
    clear_class_layouts();
    register_class_layout(FIRST_LAYOUT_DOMAIN, 70, one_ref_layout(8));
    assert_eq!(
        compact_object_total_size(FIRST_LAYOUT_DOMAIN, 70, 1),
        Some(8)
    );
    assert_eq!(
        with_current_class_layout(70, |layout| layout.total_size),
        Some(8)
    );

    let grown = Arc::new(CompactLayout {
        field_disps: vec![0, 8],
        is_ref: vec![true, true],
        field_kinds: vec![FieldStorageKind::Reference, FieldStorageKind::Reference],
        ref_disps: vec![0, 8],
        total_size: 16,
    });
    register_class_layout(FIRST_LAYOUT_DOMAIN, 70, grown);

    assert_eq!(
        compact_object_total_size(FIRST_LAYOUT_DOMAIN, 70, 1),
        None,
        "allocation must not resurrect the historical one-field layout"
    );
    assert_eq!(
        compact_object_total_size(FIRST_LAYOUT_DOMAIN, 70, 2),
        Some(16)
    );
    assert_eq!(
        with_current_class_layout(70, |layout| layout.total_size),
        Some(16)
    );
    clear_class_layouts();
}

#[test]
fn current_layout_cache_invalidates_negative_and_unregister_entries() {
    let _guard = REGISTRY_TEST_LOCK.lock().unwrap();
    clear_class_layouts();
    assert!(with_current_class_layout(71, |_| ()).is_none());
    register_class_layout(FIRST_LAYOUT_DOMAIN, 71, one_ref_layout(8));
    assert_eq!(
        with_current_class_layout(71, |layout| layout.total_size),
        Some(8)
    );
    unregister_class_layout(71);
    assert!(with_current_class_layout(71, |_| ()).is_none());
    clear_class_layouts();
}

/// `f` runs while this thread's cache is borrowed, and the GC calls back
/// into the cache from inside `f` (`forward_object` -> `object_instance_size`)
/// on every object it copies. Nested lookups must therefore return correct
/// layouts — for the same class and for a different one — and must not
/// panic on the `RefCell`.
///
/// This is the property that makes the borrowing accessor usable inside the
/// Cheney scan loops at all; without it every nested lookup would fall
/// through to the registry and the change would be a net slowdown.
#[test]
fn with_class_layout_is_reentrant() {
    let _guard = REGISTRY_TEST_LOCK.lock().unwrap();
    clear_class_layouts();
    register_class_layout(FIRST_LAYOUT_DOMAIN, 63, one_ref_layout(8));
    register_class_layout(FIRST_LAYOUT_DOMAIN, 64, one_ref_layout(16));
    // Warm both so the nested lookups exercise the hit path.
    assert_eq!(class_layout_for_fields(63, 1).unwrap().total_size, 8);
    assert_eq!(class_layout_for_fields(64, 1).unwrap().total_size, 16);

    let observed = with_class_layout(63, 1, |outer| {
        // Same class, owning accessor (what `object_instance_size` used to do).
        let same = class_layout_for_fields(63, 1).map(|l| l.total_size);
        // Different class, borrowing accessor (what it does now) — this is
        // the referent-sizing call the scan loops make per copied object.
        let other = with_class_layout(64, 1, |inner| inner.total_size);
        // Three levels deep, to confirm nesting is not merely depth-1.
        let deep = with_class_layout(64, 1, |_| with_class_layout(63, 1, |l| l.total_size));
        (outer.total_size, same, other, deep)
    });
    assert_eq!(observed, Some((8, Some(8), Some(16), Some(Some(8)))));

    // A nested lookup for a class that is NOT cached cannot install (the
    // outer shared borrow blocks it), so it falls through to the registry.
    // It must still return the right answer.
    register_class_layout(FIRST_LAYOUT_DOMAIN, 65, one_ref_layout(32));
    let uncached = with_class_layout(63, 1, |_| with_class_layout(65, 1, |l| l.total_size));
    assert_eq!(
        uncached,
        Some(Some(32)),
        "a nested miss must fall through to the registry, not fail"
    );
    clear_class_layouts();
}

#[test]
fn registry_rejects_sparse_class_id_without_poisoning_lock() {
    let _guard = REGISTRY_TEST_LOCK.lock().unwrap();
    clear_class_layouts();
    let layout = Arc::new(CompactLayout {
        field_disps: vec![0],
        is_ref: vec![true],
        field_kinds: vec![FieldStorageKind::Reference],
        ref_disps: vec![0],
        total_size: 8,
    });

    let rejected = std::panic::catch_unwind({
        let layout = Arc::clone(&layout);
        move || register_class_layout(FIRST_LAYOUT_DOMAIN, u32::MAX, layout)
    });
    assert!(
        rejected.is_err(),
        "corrupt high class_id must be rejected before dense allocation"
    );
    // "before dense allocation" is the whole point, and asserting only that
    // the call panicked cannot see it: the pre-fix code resized
    // `CLASS_LAYOUT_OWNERS` to 2^32 entries (16 GiB) and THEN panicked, so
    // this test passed while OOM-killing the test binary on any host
    // without 16 GiB to spare. Assert the tables, not just the panic.
    assert_eq!(
        CLASS_LAYOUT_OWNERS.read().unwrap().len(),
        0,
        "rejected class_id must not have resized the owners table",
    );
    assert_eq!(
        CLASS_LAYOUTS.read().unwrap().len(),
        0,
        "rejected class_id must not have resized the layouts table",
    );

    // The guard fires before taking CLASS_LAYOUTS, so a later valid
    // registration must still succeed instead of observing a poisoned lock.
    register_class_layout(FIRST_LAYOUT_DOMAIN, 0, Arc::clone(&layout));
    assert!(Arc::ptr_eq(&class_layout(0).unwrap(), &layout));
    assert!(class_layout(u32::MAX).is_none());
    clear_class_layouts();
}

// --- layout domains: the read side of `HIB-DCAST-LATEPHASE.1` ------------
//
// These two tests are the only ones in this module that need a SECOND
// layout domain, and calling `next_layout_domain` permanently advances the
// process-global counter, which retires `layout_owner`'s single-domain fast
// path for the rest of this test binary. That is deliberate and harmless
// here: every other test registers under `FIRST_LAYOUT_DOMAIN` after a
// `clear_class_layouts` (which wipes the owners table), so the slow path
// they now take answers "unclaimed" or "owner == domain" for each of them
// exactly as the fast path did. There is no wall-clock or ordering
// assumption in either direction.

/// A foreign domain must get `None` from the `class_id`-only read
/// accessors, not another `ClassStore`'s offsets.
///
/// This is the path a second VM reaches with no race and no class unload:
/// `register_class_layout` refuses its registration, so its class has no
/// layout of its own — and the unscreened lookup then answers with the
/// first VM's. Wrong field, right-looking value, no guard.
#[test]
fn foreign_domain_gets_none_from_the_class_id_keyed_accessors() {
    let _guard = REGISTRY_TEST_LOCK.lock().unwrap();
    clear_class_layouts();
    let owner_domain = next_layout_domain();
    let foreign_domain = next_layout_domain();
    assert_ne!(owner_domain, foreign_domain);

    register_class_layout(owner_domain, 90, one_ref_layout(8));

    assert_eq!(
        compact_field_disp_in(owner_domain, 90, 0),
        Some((0, true)),
        "the owning domain must still resolve its own layout"
    );
    assert!(compact_field_disp_storage_in(owner_domain, 90, 0).is_some());

    assert_eq!(
        compact_field_disp_in(foreign_domain, 90, 0),
        None,
        "a foreign domain must be refused, not served another class's offsets"
    );
    assert!(compact_field_disp_storage_in(foreign_domain, 90, 0).is_none());

    // The unscreened forms are the hazard being documented: they answer for
    // anyone. Pinned so that if they ever grow a screen of their own, this
    // test fails loudly instead of quietly becoming redundant.
    assert!(
        compact_field_disp_storage(90, 0).is_some(),
        "compact_field_disp_storage is domain-blind by contract"
    );
    clear_class_layouts();
}

/// Unloading a class must NOT hand its `class_id` slot to another domain.
///
/// The layout itself has to go (an unloaded class must stop resolving), but
/// releasing the OWNER stamp is what let domain B claim a slot whose
/// still-live compact instances in domain A keep the same `class_id` in
/// their headers — after which A decodes them through B's offsets and
/// storage kinds, silently. Ownership monotonicity is what lets the whole
/// flagged read path stay domain-free and pay nothing per access.
#[test]
fn unregister_keeps_the_slot_reserved_to_its_original_domain() {
    let _guard = REGISTRY_TEST_LOCK.lock().unwrap();
    clear_class_layouts();
    let owner_domain = next_layout_domain();
    let foreign_domain = next_layout_domain();
    assert_ne!(owner_domain, foreign_domain);

    register_class_layout(owner_domain, 91, one_ref_layout(8));
    unregister_class_layout(91);
    assert!(
        class_layout(91).is_none(),
        "unregister must still drop the layout — an unloaded class cannot resolve"
    );

    let refusals_before = foreign_layout_refusals();
    register_class_layout(foreign_domain, 91, one_ref_layout(16));
    assert!(
        foreign_layout_refusals() > refusals_before,
        "a foreign domain's registration into an unloaded slot must be refused"
    );
    assert!(
        class_layout(91).is_none(),
        "the foreign domain must not have claimed the slot"
    );
    assert!(
        compact_field_disp_storage_in(foreign_domain, 91, 0).is_none(),
        "and must not read through it either"
    );

    // The original owner may still re-register: this is exactly the
    // unload-then-redefine flow, and it must not be collateral damage.
    register_class_layout(owner_domain, 91, one_ref_layout(24));
    assert_eq!(class_layout(91).map(|l| l.total_size), Some(24));
    clear_class_layouts();
}

/// The object-carrying accessor's explicit screen: a compact receiver whose
/// `class_id` belongs to another domain must read as `None`, which routes it
/// into the `HIB-DCAST-LATEPHASE.1` guard in `gc/src/gen_heap.rs::get_field`
/// (a logged null read) instead of a wrong-offset field access.
#[test]
fn compact_object_field_storage_in_screens_a_foreign_domain() {
    let _guard = REGISTRY_TEST_LOCK.lock().unwrap();
    clear_class_layouts();
    let owner_domain = next_layout_domain();
    let foreign_domain = next_layout_domain();
    assert_ne!(owner_domain, foreign_domain);

    register_class_layout(owner_domain, 92, one_ref_layout(8));
    let mut header = ObjectHeader::new(
        ClassId::new(92),
        ObjectKind::Object,
        ArrayElementType::Reference,
        0,
        1, // one field: matches `one_ref_layout`'s field count
    );
    header.add_gc_flags(GC_FLAG_COMPACT);

    assert!(
        compact_object_field_disp_in(owner_domain, &header, 0).is_some(),
        "the owning domain must still resolve its own compact receiver"
    );
    assert!(
        compact_object_field_disp_in(foreign_domain, &header, 0).is_none(),
        "a foreign domain must degrade to the guard, not to wrong offsets"
    );
    clear_class_layouts();
}

/// Round 9 wave 4 (`gc4`): the per-field-access lookup
/// ([`compact_object_field_disp`]) and the TLAB sizing lookup
/// ([`compact_tlab_total_size`]) moved from OWNING forms
/// (`class_layout_for_fields` / `class_layout`, an `Arc` clone/drop per
/// call) to BORROWING ones. They must answer exactly what the owning forms
/// answer: a hit, every index in and out of range, a field-count mismatch,
/// a legacy header, a redefine and an unregister.
#[test]
fn borrowing_access_lookups_agree_with_the_owning_ones() {
    let _guard = REGISTRY_TEST_LOCK.lock().unwrap();
    clear_class_layouts();
    let two = |total_size: u32| {
        Arc::new(CompactLayout {
            field_disps: vec![0, 8],
            is_ref: vec![true, false],
            field_kinds: vec![FieldStorageKind::Reference, FieldStorageKind::Long],
            ref_disps: vec![0],
            total_size,
        })
    };
    const CID: u32 = 93;
    register_class_layout(FIRST_LAYOUT_DOMAIN, CID, two(16));
    let header = |fields: u32, compact: bool| {
        let h = ObjectHeader::new(
            ClassId::new(CID),
            ObjectKind::Object,
            ArrayElementType::Reference,
            0,
            fields,
        );
        if compact {
            h.add_gc_flags(GC_FLAG_COMPACT);
        }
        h
    };
    // The pre-wave-4 body of `compact_object_field_disp`, verbatim.
    let owning = |h: &ObjectHeader, index: usize| -> Option<(usize, FieldStorageKind)> {
        if !is_compact_object(h) {
            return None;
        }
        let layout = class_layout_for_fields(h.class_id.as_u32(), h.num_slots())?;
        Some((
            layout.field_disp(index)? as usize,
            layout.field_storage(index)?,
        ))
    };
    // The pre-wave-4 body of `compact_tlab_total_size`, minus its gates.
    let owning_tlab = |fc: usize| {
        class_layout(CID)
            .filter(|l| l.field_count() == fc)
            .map(|l| l.total_size as usize)
    };
    let tlab_gated = compact_ref_fields_enabled() && single_layout_domain();
    let check = |what: &str| {
        for (fields, compact) in [(2u32, true), (1, true), (3, true), (2, false)] {
            let h = header(fields, compact);
            for index in 0..4 {
                // Twice: the second call is a cache hit.
                for _ in 0..2 {
                    assert_eq!(
                        compact_object_field_disp(&h, index),
                        owning(&h, index),
                        "{what}: fields {fields}, compact {compact}, index {index}"
                    );
                }
            }
            assert_eq!(
                with_class_layout_versioned(CID, fields, |l| l.total_size),
                class_layout_for_fields(CID, fields).map(|l| l.total_size),
                "{what}: versioned borrow, fields {fields}"
            );
            if tlab_gated {
                assert_eq!(
                    compact_tlab_total_size(CID, fields as usize),
                    owning_tlab(fields as usize),
                    "{what}: tlab size, fields {fields}"
                );
            }
        }
    };
    check("registered");
    assert_eq!(
        compact_object_field_disp(&header(2, true), 1),
        Some((8, FieldStorageKind::Long))
    );
    register_class_layout(FIRST_LAYOUT_DOMAIN, CID, two(24));
    check("redefined");
    if tlab_gated {
        assert_eq!(
            compact_tlab_total_size(CID, 2),
            Some(24),
            "a redefine must not serve the old size"
        );
    }
    unregister_class_layout(CID);
    check("unregistered");
    assert_eq!(compact_object_field_disp(&header(2, true), 0), None);
    clear_class_layouts();
}

/// The two `with_class_layout` arms must be indistinguishable.
///
/// They resolve through the same registry, so a difference is a bug in the
/// dense cache and never a tuning question — which is what makes
/// `CRATONVM_GC_LAYOUT_SCAN_CACHE=0` a usable A/B arm. Covers the four
/// cases the dense table adds: a hit, a cached NEGATIVE, a class whose
/// `field_count` does not match the cached slot, and a `class_id` past
/// `SCAN_CACHE_MAX` (which the table declines to index and must therefore
/// still answer correctly).
#[test]
fn both_with_class_layout_arms_agree() {
    let _guard = REGISTRY_TEST_LOCK.lock().unwrap();
    clear_class_layouts();
    for cid in [3u32, 4, 5, 900] {
        register_class_layout(FIRST_LAYOUT_DOMAIN, cid, one_ref_layout(cid * 8 + 8));
    }
    let probe = |cid: u32, fc: u32| {
        let gen = layout_generation();
        let mru = with_class_layout_mru(cid, fc, gen, |l| l.total_size);
        let dense = with_class_layout_scan_cached(cid, fc, gen, |l| l.total_size);
        assert_eq!(
            mru, dense,
            "arms disagree for (class_id {cid}, field_count {fc})"
        );
        // Twice, so the second call is served from each arm's cache.
        let mru2 = with_class_layout_mru(cid, fc, gen, |l| l.total_size);
        let dense2 = with_class_layout_scan_cached(cid, fc, gen, |l| l.total_size);
        assert_eq!(mru2, dense2, "cached arms disagree for ({cid}, {fc})");
        assert_eq!(mru, mru2, "a cached hit differs from the first resolve");
        dense
    };
    for cid in [3u32, 4, 5, 900] {
        assert_eq!(probe(cid, 1), Some(cid * 8 + 8), "registered class {cid}");
    }
    // Cached negatives: never registered, and registered-but-wrong-count.
    assert_eq!(probe(6, 1), None, "an unregistered class has no layout");
    assert_eq!(probe(3, 2), None, "field_count is part of the key");
    // Re-probe the original count: the slot for class 3 was just overwritten
    // by the (3, 2) negative, so this exercises the flap path.
    assert_eq!(
        probe(3, 1),
        Some(32),
        "a flapped slot must re-resolve, not go stale"
    );
    // Past the dense table's index bound. `register_class_layout` refuses
    // ids at or above MAX_DENSE_CLASS_LAYOUTS, so the answer is None on
    // both arms — but it must come back, not hang on a 4-billion resize.
    assert!(SCAN_CACHE_MAX < MAX_DENSE_CLASS_LAYOUTS);
    assert_eq!(
        probe(SCAN_CACHE_MAX as u32, 1),
        None,
        "past the dense bound"
    );
    assert_eq!(probe(u32::MAX, 1), None, "AUTOBOX_CLASS_ID has no layout");
    // A registration bumps the generation and must invalidate the table.
    register_class_layout(FIRST_LAYOUT_DOMAIN, 3, one_ref_layout(4096));
    assert_eq!(
        probe(3, 1),
        Some(4096),
        "a redefine must not serve the old layout"
    );
    clear_class_layouts();
}

/// COMPONENT MEASUREMENT for the collector's per-object layout lookup.
///
/// `with_class_layout` is called once per scanned object by every
/// collector's mark and sweep loops. It is served by an 8-entry
/// thread-local MRU cache; the argument for that size is "the GC scans
/// long runs of same-class objects". A tree of one node type does. A real
/// heap does not: the collector's working set is every class with a live
/// instance, which is hundreds. Past 8 the cache thrashes and every lookup
/// pays `registry_layout_for_fields` -- an `RwLock` reader acquire and a
/// SipHash probe -- plus an install.
///
/// This times the lookup alone, at working sets either side of the cache,
/// so the cost can be compared against a real cycle's object count BEFORE
/// anything is rebuilt. Ignored: wants `--release` and a quiet box.
///
///   cargo test -p cratonvm-types --release --lib -- --ignored --nocapture     ///     measure_the_layout_lookup_at_working_set
#[test]
#[ignore = "timing measurement; wants --release and a quiet box"]
fn measure_the_layout_lookup_at_working_set() {
    let _guard = REGISTRY_TEST_LOCK.lock().unwrap();
    clear_class_layouts();
    const CLASSES: u32 = 8192;
    for cid in 0..CLASSES {
        register_class_layout(FIRST_LAYOUT_DOMAIN, cid, one_ref_layout(cid * 8 + 8));
    }
    // Warm the registry's own lock/hash path once.
    for cid in 0..CLASSES {
        let _ = class_layout_for_fields(cid, 1);
    }
    // Both arms in ONE process, called directly rather than through the
    // `OnceLock` switch, so the comparison cannot be a cross-binary one.
    const LOOKUPS: u32 = 2_000_000;
    println!("working_set   8-entry MRU ns   dense scan ns    speedup");
    for &ws in &[1u32, 4, 8, 16, 64, 256, 1024, 8192] {
        let gen = layout_generation();
        let mut run = |dense: bool| -> f64 {
            let t = std::time::Instant::now();
            let mut acc = 0usize;
            for i in 0..LOOKUPS {
                let cid = i % ws;
                let got = if dense {
                    with_class_layout_scan_cached(cid, 1, gen, |l| l.total_size as usize)
                } else {
                    with_class_layout_mru(cid, 1, gen, |l| l.total_size as usize)
                };
                acc += got.unwrap_or(0);
            }
            let el = t.elapsed();
            assert!(acc > 0, "the loop must not be optimised away");
            el.as_nanos() as f64 / LOOKUPS as f64
        };
        let mru = run(false);
        let dense = run(true);
        println!(
            "{ws:>11}   {mru:>13.2}   {dense:>13.2}   {:>8.2}x",
            mru / dense
        );
    }
    clear_class_layouts();
}

#[test]
fn compact_object_field_disp_cache_hits_and_invalidates_on_redefine() {
    let _guard = REGISTRY_TEST_LOCK.lock().unwrap();
    clear_class_layouts();
    const CID: u32 = 94;
    let layout_a = Arc::new(CompactLayout {
        field_disps: vec![0, 8],
        is_ref: vec![true, false],
        field_kinds: vec![FieldStorageKind::Reference, FieldStorageKind::Long],
        ref_disps: vec![0],
        total_size: 16,
    });
    let layout_b = Arc::new(CompactLayout {
        field_disps: vec![4, 16],
        is_ref: vec![false, true],
        field_kinds: vec![FieldStorageKind::Int, FieldStorageKind::Reference],
        ref_disps: vec![16],
        total_size: 24,
    });
    register_class_layout(FIRST_LAYOUT_DOMAIN, CID, layout_a);
    let h = ObjectHeader::new(
        ClassId::new(CID),
        ObjectKind::Object,
        ArrayElementType::Reference,
        0,
        2,
    );
    h.add_gc_flags(GC_FLAG_COMPACT);
    assert_eq!(compact_object_field_disp(&h, 0), Some((0, FieldStorageKind::Reference)));
    assert_eq!(compact_object_field_disp(&h, 1), Some((8, FieldStorageKind::Long)));
    assert_eq!(compact_object_field_disp(&h, 1), Some((8, FieldStorageKind::Long)));

    register_class_layout(FIRST_LAYOUT_DOMAIN, CID, layout_b);
    assert_eq!(compact_object_field_disp(&h, 0), Some((4, FieldStorageKind::Int)));
    assert_eq!(compact_object_field_disp(&h, 1), Some((16, FieldStorageKind::Reference)));
    clear_class_layouts();
}
