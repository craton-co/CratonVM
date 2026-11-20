use super::*;
use crate::config::VmConfig;
use std::sync::Arc;

fn test_shared() -> Arc<SharedVm> {
    Arc::new(SharedVm::new(VmConfig::default()))
}

// -----------------------------------------------------------------------
// Java String helpers
// -----------------------------------------------------------------------

#[test]
fn create_and_read_string() {
    let shared = test_shared();
    let obj = create_java_string(&shared, "Hello");
    let result = read_java_string(&shared.mem.heap, obj);
    assert_eq!(result, Some("Hello".to_string()));
}

#[test]
fn create_empty_string() {
    let shared = test_shared();
    let obj = create_java_string(&shared, "");
    let result = read_java_string(&shared.mem.heap, obj);
    assert_eq!(result, Some(String::new()));
}

#[test]
fn create_unicode_string() {
    let shared = test_shared();
    let obj = create_java_string(&shared, "\u{1F600} emoji");
    let result = read_java_string(&shared.mem.heap, obj);
    assert_eq!(result, Some("\u{1F600} emoji".to_string()));
}

#[test]
fn string_pool_deduplicates() {
    let shared = test_shared();
    let obj1 = create_java_string(&shared, "same");
    let obj2 = create_java_string(&shared, "same");
    assert_eq!(obj1.as_ptr(), obj2.as_ptr());
}

/// gc-common w5-c: the fallible GC-safe constructor (which the unwinding
/// `NativeContext::create_string_uninterned_gc_safe` now collects and
/// retries around) builds the same String as the infallible form it backs,
/// and neither interns.
#[test]
fn gc_safe_threaded_string_is_fallible_and_unpooled() {
    use crate::threading::jvm_thread::{JvmThread, ThreadId};
    let shared = test_shared();
    let mut thread = JvmThread::new(ThreadId(0), "w5c-gc-safe-string");
    let a = try_create_java_string_uninterned_gc_safe_threaded(&shared, &mut thread, "w5c")
        .expect("an empty heap holds a String");
    let b = try_create_java_string_uninterned_gc_safe_threaded(&shared, &mut thread, "w5c")
        .expect("an empty heap holds a String");
    assert_eq!(
        read_java_string(&shared.mem.heap, a).as_deref(),
        Some("w5c")
    );
    assert_eq!(
        read_java_string(&shared.mem.heap, b).as_deref(),
        Some("w5c")
    );
    assert_ne!(a.as_ptr(), b.as_ptr());
}

/// gen r4w3/hunter: the fallible pooled constructor the unwinding
/// `NativeContext::create_string` uses builds the same string, reports a
/// miss as an allocation, and shares the pool with the infallible one.
#[test]
fn try_create_java_string_reporting_is_pooled_and_reports_allocation() {
    let shared = test_shared();
    let (a, allocated) =
        try_create_java_string_reporting(&shared, "r4w3-pooled").expect("room on a fresh heap");
    assert!(allocated, "a pool miss allocates");
    assert_eq!(
        read_java_string(&shared.mem.heap, a),
        Some("r4w3-pooled".to_string())
    );
    let (b, allocated) =
        try_create_java_string_reporting(&shared, "r4w3-pooled").expect("pool hit");
    assert!(!allocated, "a pool hit allocates nothing");
    assert_eq!(a.as_ptr(), b.as_ptr());
    assert_eq!(
        create_java_string(&shared, "r4w3-pooled").as_ptr(),
        a.as_ptr(),
        "one pool for both constructors"
    );
    let (u, allocated) =
        try_create_java_string_reporting(&shared, "\u{1F600} r4w3").expect("room");
    assert!(allocated);
    assert_eq!(
        read_java_string(&shared.mem.heap, u),
        Some("\u{1F600} r4w3".to_string())
    );
}

#[test]
fn string_pool_different_strings() {
    let shared = test_shared();
    let obj1 = create_java_string(&shared, "alpha");
    let obj2 = create_java_string(&shared, "beta");
    assert_ne!(obj1.as_ptr(), obj2.as_ptr());
}

#[test]
fn read_string_non_string_object() {
    let shared = test_shared();
    // Create a plain object (not a string -- field 0 is not an array)
    let obj = shared.mem.heap.alloc_object(ClassId::new(0), 2);
    shared.mem.heap.set_field(obj, 0, Value::Int(0));
    let result = read_java_string(&shared.mem.heap, obj);
    assert!(result.is_none());
}

#[test]
fn read_string_null_value_field() {
    let shared = test_shared();
    let obj = shared.mem.heap.alloc_object(ClassId::new(0), 2);
    shared.mem.heap.set_field(obj, 0, Value::Object(None));
    let result = read_java_string(&shared.mem.heap, obj);
    assert!(result.is_none());
}

// -----------------------------------------------------------------------
// Compact string helpers
// -----------------------------------------------------------------------

#[test]
fn compact_string_latin1_roundtrip() {
    let shared = test_shared();
    // Enable compact string mode
    shared
        .classes
        .compact_strings
        .store(true, std::sync::atomic::Ordering::Relaxed);
    // Reset cached field count so it re-resolves (synthetic stub → 2 fields,
    // but we need 4 for compact layout). Manually set to 4.
    shared
        .classes
        .cached_string_num_fields
        .store(4, std::sync::atomic::Ordering::Relaxed);

    let obj = create_java_string(&shared, "Hello");
    let result = read_java_string(&shared.mem.heap, obj);
    assert_eq!(result, Some("Hello".to_string()));
}

#[test]
fn compact_string_utf16_roundtrip() {
    let shared = test_shared();
    shared
        .classes
        .compact_strings
        .store(true, std::sync::atomic::Ordering::Relaxed);
    shared
        .classes
        .cached_string_num_fields
        .store(4, std::sync::atomic::Ordering::Relaxed);

    let obj = create_java_string(&shared, "\u{1F600} emoji");
    let result = read_java_string(&shared.mem.heap, obj);
    assert_eq!(result, Some("\u{1F600} emoji".to_string()));
}

#[test]
fn compact_string_empty_roundtrip() {
    let shared = test_shared();
    shared
        .classes
        .compact_strings
        .store(true, std::sync::atomic::Ordering::Relaxed);
    shared
        .classes
        .cached_string_num_fields
        .store(4, std::sync::atomic::Ordering::Relaxed);

    let obj = create_java_string(&shared, "");
    let result = read_java_string(&shared.mem.heap, obj);
    assert_eq!(result, Some(String::new()));
}

#[test]
fn compact_string_latin1_boundary() {
    let shared = test_shared();
    shared
        .classes
        .compact_strings
        .store(true, std::sync::atomic::Ordering::Relaxed);
    shared
        .classes
        .cached_string_num_fields
        .store(4, std::sync::atomic::Ordering::Relaxed);

    // Latin-1 boundary: U+00FF (ÿ) is the last Latin-1 char
    let obj = create_java_string(&shared, "café\u{00FF}");
    let result = read_java_string(&shared.mem.heap, obj);
    assert_eq!(result, Some("café\u{00FF}".to_string()));
}

#[test]
fn compact_string_beyond_latin1() {
    let shared = test_shared();
    shared
        .classes
        .compact_strings
        .store(true, std::sync::atomic::Ordering::Relaxed);
    shared
        .classes
        .cached_string_num_fields
        .store(4, std::sync::atomic::Ordering::Relaxed);

    // U+0100 (Ā) is beyond Latin-1 → must use UTF16 coder
    let obj = create_java_string(&shared, "Hello\u{0100}World");
    let result = read_java_string(&shared.mem.heap, obj);
    assert_eq!(result, Some("Hello\u{0100}World".to_string()));
}

#[test]
fn compact_string_pool_deduplicates() {
    let shared = test_shared();
    shared
        .classes
        .compact_strings
        .store(true, std::sync::atomic::Ordering::Relaxed);
    shared
        .classes
        .cached_string_num_fields
        .store(4, std::sync::atomic::Ordering::Relaxed);

    let obj1 = create_java_string(&shared, "same");
    let obj2 = create_java_string(&shared, "same");
    assert_eq!(obj1.as_ptr(), obj2.as_ptr());
}

#[test]
fn compact_ascii_uninterned_strings_are_fresh_and_roundtrip() {
    let shared = test_shared();
    shared
        .classes
        .compact_strings
        .store(true, std::sync::atomic::Ordering::Relaxed);
    shared
        .classes
        .cached_string_num_fields
        .store(4, std::sync::atomic::Ordering::Relaxed);

    let obj1 = create_java_string_uninterned(&shared, "5888890");
    let obj2 = create_java_string_uninterned(&shared, "5888890");
    assert_ne!(obj1.as_ptr(), obj2.as_ptr());
    assert_eq!(
        read_java_string(&shared.mem.heap, obj1),
        Some("5888890".to_string())
    );
    assert_eq!(
        read_java_string(&shared.mem.heap, obj2),
        Some("5888890".to_string())
    );
}

/// Build a `byte[]` payload directly and decode it via
/// `decode_java_string_value_array`, exercising the bulk byte read
/// (`read_byte_array_bulk`) added in the P1 perf fix. Pins exact output
/// for LATIN1 (1 byte/char), UTF16 little-endian (2 bytes/char), the empty
/// array, and an odd-length UTF16 buffer (the trailing odd byte must be
/// dropped, matching the previous `len / 2` per-element loop).
fn make_byte_array(shared: &SharedVm, bytes: &[u8]) -> ObjectRef {
    let arr = shared
        .mem
        .heap
        .alloc_array(ClassId::new(0), ArrayElementType::Byte, bytes.len());
    for (i, &b) in bytes.iter().enumerate() {
        let _ = shared
            .mem
            .heap
            .set_array_element(arr, i, Value::Int(b as i32));
    }
    arr
}

#[test]
fn decode_value_array_byte_paths_bulk() {
    let shared = test_shared();

    // LATIN1: one byte per char, U+0000..U+00FF map directly.
    let latin1 = make_byte_array(&shared, &[b'H', b'i', 0xFF]);
    assert_eq!(
        decode_java_string_value_array(&shared.mem.heap, latin1, CODER_LATIN1),
        Some("Hi\u{00FF}".to_string())
    );

    // UTF16 little-endian: low byte at even index. "Ā" = U+0100 → [0x00,0x01].
    let utf16 = make_byte_array(&shared, &[b'A', 0x00, 0x00, 0x01]);
    assert_eq!(
        decode_java_string_value_array(&shared.mem.heap, utf16, CODER_UTF16),
        Some("A\u{0100}".to_string())
    );

    // Empty byte[] decodes to the empty string under either coder.
    let empty = make_byte_array(&shared, &[]);
    assert_eq!(
        decode_java_string_value_array(&shared.mem.heap, empty, CODER_LATIN1),
        Some(String::new())
    );

    // Odd-length UTF16 buffer: the trailing odd byte is dropped, exactly
    // as the previous `num_units = len / 2` per-element loop did.
    let odd = make_byte_array(&shared, &[b'A', 0x00, 0x42]);
    assert_eq!(
        decode_java_string_value_array(&shared.mem.heap, odd, CODER_UTF16),
        Some("A".to_string())
    );
}

/// Wave 2 (S108) regression: a non-String object whose `field 0` holds an
/// `Object[]` (Reference array) — e.g. Guava's `ImmutableList` or any
/// collection backing — must NOT short-circuit `read_java_string` into
/// returning `Some(garbage)`. Returning `Some(_)` would tell callers
/// "this is already a String" and skip the proper `toString()` virtual
/// dispatch, producing the `xs=㊘粹ƈ` mojibake the S107 GuavaTest hit.
/// The fix returns `None` for any non-Char/Byte array element type so
/// callers fall through to the toString() path.
#[test]
fn read_string_returns_none_for_reference_array_field() {
    let shared = test_shared();
    let obj = shared.mem.heap.alloc_object(ClassId::new(0), 1);
    let elem_a = shared.mem.heap.alloc_object(ClassId::new(0), 0);
    let elem_b = shared.mem.heap.alloc_object(ClassId::new(0), 0);
    let elem_c = shared.mem.heap.alloc_object(ClassId::new(0), 0);
    let arr = shared
        .mem
        .heap
        .alloc_array(ClassId::new(0), ArrayElementType::Reference, 3);
    shared
        .mem
        .heap
        .set_array_element(arr, 0, Value::Object(Some(elem_a)))
        .unwrap();
    shared
        .mem
        .heap
        .set_array_element(arr, 1, Value::Object(Some(elem_b)))
        .unwrap();
    shared
        .mem
        .heap
        .set_array_element(arr, 2, Value::Object(Some(elem_c)))
        .unwrap();
    shared.mem.heap.set_field(obj, 0, Value::Object(Some(arr)));
    // Pre-fix this returned Some(garbage); post-fix must return None.
    assert_eq!(read_java_string(&shared.mem.heap, obj), None);
}

// -----------------------------------------------------------------------
// Class mirror helpers
// -----------------------------------------------------------------------

/// NOTE (2026-08-12): the slot-0 assertion below is mode-dependent by
/// design. `get_or_create_class_mirror` now skips the slot-0 write when
/// `java/lang/Class` slot 0 is a compact REFERENCE field (real-JDK mode);
/// see the long note there. These tests run under `test_shared()`, i.e.
/// synthetic-jdk mode, where `java/lang/Class` is a fabricated
/// compatibility stub. `build_compact_layout_ordered` returns `None` for a
/// class whose slots are entirely padding, so no compact layout is ever
/// registered for it, `class_layout` answers `None`, the gate evaluates
/// `false`, and the write still happens. The reverse-map half of this test
/// is the part that holds in BOTH modes — and it is the half the runtime
/// actually depends on.
#[test]
fn class_mirror_stores_class_id_in_reverse_map() {
    let shared = test_shared();
    let class_id = ClassId::new(42);
    let mirror = get_or_create_class_mirror(&shared, class_id);
    // Field 0 still stores Int(class_id) for legacy compatibility.
    assert_eq!(shared.mem.heap.get_field(mirror, 0), Value::Int(42));
    // But class_id is also recoverable via the reverse map.
    let recovered = shared
        .classes
        .class_mirrors_reverse
        .read()
        .get(&mirror)
        .copied();
    assert_eq!(recovered, Some(class_id));
}

#[test]
fn class_mirror_caching() {
    let shared = test_shared();
    let class_id = ClassId::new(7);
    let m1 = get_or_create_class_mirror(&shared, class_id);
    let m2 = get_or_create_class_mirror(&shared, class_id);
    assert_eq!(m1.as_ptr(), m2.as_ptr());
}

#[test]
fn class_mirror_different_ids() {
    let shared = test_shared();
    let m1 = get_or_create_class_mirror(&shared, ClassId::new(1));
    let m2 = get_or_create_class_mirror(&shared, ClassId::new(2));
    assert_ne!(m1.as_ptr(), m2.as_ptr());
}

#[test]
fn class_mirror_name_field() {
    let shared = test_shared();
    let class_id = ClassId::new(999); // non-existent class
    let mirror = get_or_create_class_mirror(&shared, class_id);
    // Field 1 should contain a name string
    let name_val = shared.mem.heap.get_field(mirror, 1);
    match name_val {
        Value::Object(Some(name_ref)) => {
            let name = read_java_string(&shared.mem.heap, name_ref);
            // For unknown class, falls back to "unknown_999"
            assert!(name.is_some());
            assert!(name.unwrap().contains("999"));
        }
        _ => panic!("Expected name string in field 1"),
    }
}

/// Regression: the synthetic-stub `java/lang/Class` mirror is allocated
/// with only `CLASS_MIRROR_NUM_FIELDS` (2) slots, yet the legacy fixed
/// layout would place `primitive` at slot 7 and `classRedefinedCount` at
/// slot 12. `resolve_class_mirror_slots` must gate those against the
/// allocated field count so the populator never writes out of bounds —
/// only slots 0 (class_id) and 1 (name) are touched on a 2-field mirror.
#[test]
fn real_class_mirrors_preserve_the_constructor_slot_in_both_modes() {
    use cratonvm_types::compat::CompatibilityMode;
    for mode in [CompatibilityMode::JdkOnly, CompatibilityMode::Compatible] {
        let mut config = VmConfig::default();
        config.compatibility_mode = mode;
        config.use_synthetic_jdk = false;
        let shared = Arc::new(SharedVm::new(config));
        let class_id = {
            let mut cm = shared.classes.class_manager.write();
            let id = cm.get_loaded_class_id("java/lang/Class").unwrap();
            // The synthetic fixture supplies the allocation shape; marking
            // its origin as real exercises the same policy as a JDK image.
            cm.get_class_mut(id).unwrap().origin =
                cratonvm_classloading::ClassOrigin::ApplicationClassPath {
                    source: Arc::from("real-mirror-regression"),
                };
            id
        };
        assert!(!mirror_slot0_overlay(&shared, class_id), "{mode:?}");
        let mirror = get_or_create_class_mirror(&shared, ClassId::new(123));
        let untouched =
            alloc_class_mirror_object(&shared, class_id, shared.mem.heap.num_fields(mirror));
        let initial_slot = shared.mem.heap.get_field(untouched, 0);
        assert_eq!(
            shared.mem.heap.get_field(mirror, 0),
            initial_slot,
            "{mode:?}"
        );
        assert_eq!(
            class_id_from_mirror(&shared, mirror),
            Some(ClassId::new(123))
        );
    }
}

#[test]
fn class_mirror_synthetic_stub_respects_field_count() {
    let shared = test_shared();
    let mirror = get_or_create_class_mirror(&shared, ClassId::new(123));
    // The synthetic-stub mirror is sized to CLASS_MIRROR_NUM_FIELDS (2),
    // while the legacy fixed layout would place `primitive` at slot 7 and
    // `classRedefinedCount` at slot 12. Reaching this line at all proves
    // the slot gate kept those writes in bounds (an ungated write would
    // have panicked in `set_field`).
    let n = shared.mem.heap.num_fields(mirror);
    assert!(n >= CLASS_MIRROR_NUM_FIELDS);
    // Slot 0 holds the class id; the legacy name slot (1) holds the name.
    assert_eq!(shared.mem.heap.get_field(mirror, 0), Value::Int(123));
    assert!(matches!(
        shared.mem.heap.get_field(mirror, LEGACY_NAME_SLOT),
        Value::Object(Some(_))
    ));
}

// -----------------------------------------------------------------------
// Primitive mirror helpers
// -----------------------------------------------------------------------

#[test]
fn primitive_mirror_int() {
    let shared = test_shared();
    let mirror = get_or_create_primitive_mirror(&shared, "int");
    // Field 0 = Int(-1) marker for primitive mirrors.
    assert_eq!(shared.mem.heap.get_field(mirror, 0), Value::Int(-1));
    // Field 1 should be "int" string
    let name_val = shared.mem.heap.get_field(mirror, 1);
    match name_val {
        Value::Object(Some(name_ref)) => {
            let name = read_java_string(&shared.mem.heap, name_ref);
            assert_eq!(name, Some("int".to_string()));
        }
        _ => panic!("Expected name string in field 1"),
    }
}

#[test]
fn primitive_mirror_caching() {
    let shared = test_shared();
    let m1 = get_or_create_primitive_mirror(&shared, "boolean");
    let m2 = get_or_create_primitive_mirror(&shared, "boolean");
    assert_eq!(m1.as_ptr(), m2.as_ptr());
}

#[test]
fn primitive_mirror_different_types() {
    let shared = test_shared();
    let m_int = get_or_create_primitive_mirror(&shared, "int");
    let m_long = get_or_create_primitive_mirror(&shared, "long");
    assert_ne!(m_int.as_ptr(), m_long.as_ptr());
}

// -----------------------------------------------------------------------
// Static field access
// -----------------------------------------------------------------------

#[test]
fn get_static_default_value() {
    let shared = test_shared();
    let val = get_static_shared(&shared, ClassId::new(99), 0);
    assert_eq!(val, Value::Int(0));
}

#[test]
fn set_and_get_static() {
    let shared = test_shared();
    let class_id = ClassId::new(10);
    set_static_shared(&shared, class_id, 0, Value::Int(42));
    assert_eq!(get_static_shared(&shared, class_id, 0), Value::Int(42));
}

#[test]
fn set_static_auto_resizes() {
    let shared = test_shared();
    let class_id = ClassId::new(11);
    set_static_shared(&shared, class_id, 5, Value::Long(100));
    assert_eq!(get_static_shared(&shared, class_id, 5), Value::Long(100));
    // Indices below should be default
    assert_eq!(get_static_shared(&shared, class_id, 0), Value::Int(0));
}

#[test]
fn set_static_overwrite() {
    let shared = test_shared();
    let class_id = ClassId::new(12);
    set_static_shared(&shared, class_id, 0, Value::Int(1));
    set_static_shared(&shared, class_id, 0, Value::Int(2));
    assert_eq!(get_static_shared(&shared, class_id, 0), Value::Int(2));
}

/// Once a class's block is published, a store must not need the `statics`
/// lock: a reader holding it (standing in for every other thread's
/// `putstatic`) must not stall this one. Run on a second thread with a
/// timeout so a regression fails instead of deadlocking the test binary.
#[test]
fn set_static_steady_state_store_takes_no_statics_lock() {
    let shared = test_shared();
    let class_id = ClassId::new(13);
    // First store creates and publishes the block (locked path).
    set_static_shared(&shared, class_id, 1, Value::Long(7));
    if statics_index_disabled() {
        return; // the kill switch routes every store through the lock
    }
    let guard = shared.classes.statics.read();
    let (tx, rx) = std::sync::mpsc::channel();
    let shared_ref: &SharedVm = &shared;
    let stored = std::thread::scope(|s| {
        s.spawn(move || {
            set_static_shared(shared_ref, class_id, 1, Value::Long(8));
            let _ = tx.send(());
        });
        let stored = rx.recv_timeout(std::time::Duration::from_secs(10)).is_ok();
        // Release the lock before the scope joins, whichever way it went.
        drop(guard);
        stored
    });
    assert!(
        stored,
        "a store to a published static waited on the statics lock"
    );
    assert_eq!(get_static_shared(&shared, class_id, 1), Value::Long(8));
    // The map's block is the published one, so the locked reader agrees.
    assert_eq!(
        shared
            .classes
            .statics
            .read()
            .get(&class_id)
            .and_then(|f| f.get(1))
            .copied(),
        Some(Value::Long(8))
    );
    // Past the published length still grows through the locked path.
    set_static_shared(&shared, class_id, 64, Value::Int(3));
    assert_eq!(get_static_shared(&shared, class_id, 64), Value::Int(3));
    assert_eq!(get_static_shared(&shared, class_id, 1), Value::Long(8));
}

#[test]
fn set_static_multiple_classes() {
    let shared = test_shared();
    set_static_shared(&shared, ClassId::new(1), 0, Value::Int(10));
    set_static_shared(&shared, ClassId::new(2), 0, Value::Int(20));
    assert_eq!(
        get_static_shared(&shared, ClassId::new(1), 0),
        Value::Int(10)
    );
    assert_eq!(
        get_static_shared(&shared, ClassId::new(2), 0),
        Value::Int(20)
    );
}

// -----------------------------------------------------------------------
// G30 — the class-mirror overlay and the coercion it must never meet
// -----------------------------------------------------------------------

/// The two mirror populators write their VM-internal tag into slot 0 of an
/// object stamped `java/lang/Class`, whose real JDK 25 slot 0 is
/// `Constructor<T> cachedConstructor` — a REFERENCE. Which store they use
/// decides what ends up there, and the two answers are not close:
///
/// | store | path | slot 0 afterwards |
/// |---|---|---|
/// | `set_field` (descriptor-less) | `gc::autobox::box_for_reference_slot` | an `AUTOBOX_CLASS_ID` wrapper that `get_field` un-boxes back to `Int` |
/// | `set_field_as(.., b'L')` | `gc::heap::coerce_field_value_for_slot` | **`Value::Object(None)`** — the tag is gone |
///
/// `mirror_class_id` falls back to slot 0 when `class_mirrors_reverse`
/// misses, and that fallback is load-bearing: gating this write off was
/// measured to fail `RJdkHello` at `System.out instanceof PrintStream`
/// (see the long note at the write site). Nulling it would do the same
/// thing by a different route, and would do it silently, because the
/// coercion does not refuse — it answers `null` and returns.
///
/// This has to scan source. The defect it guards against is a change that
/// compiles, runs, and produces a mirror that is wrong only where the
/// reverse map happens to miss, so there is nothing to observe from a
/// unit test that does not know which lookups will miss.
#[test]
fn the_class_mirror_id_is_written_without_a_field_descriptor() {
    // Scan only what is ABOVE this test module. Every needle below also
    // appears verbatim in this test's own assertions, so scanning the
    // whole file would find each one inside itself and the test would
    // pass whatever the real code said.
    let src = include_str!("../vm_object.rs")
        .split("mod tests {")
        .next()
        .expect("split always yields a first element");
    for (marker, what) in [
        (
            "set_field(mirror, 0, Value::Int(class_id.as_u32() as i32))",
            "get_or_create_class_mirror",
        ),
        (
            "set_field(mirror, 0, Value::Int(-1))",
            "get_or_create_primitive_mirror",
        ),
    ] {
        assert!(
            src.contains(marker),
            "{what} must still write the mirror's ClassId tag into slot 0 \
             through the DESCRIPTOR-LESS `set_field`. If this fired \
             because the call was switched to `set_field_as`, revert it: \
             the `b'L'` arm of `gc::heap::coerce_field_value_for_slot` \
             maps `Int(_)` to `Object(None)`, so the tag would be replaced \
             by null and `mirror_class_id`'s fallback would answer None. \
             If it fired because the write was deleted, see the measured \
             `RJdkHello` regression recorded at the write site.",
        );
    }
    assert!(
        !src.contains("set_field_as(mirror, 0"),
        "slot 0 of a class mirror must never be written through the \
         descriptor-aware setter — see the table on this test",
    );
}

/// Interpreter round i3 wave 80, lane L5: `CRATONVM_CLASS_MIRROR_LAYOUT_MEMO`
/// mints the same mirrors as the slow path, and fills its memo only from a
/// real (not compatibility-stub) `java/lang/Class`. The switch is set in
/// each VM's config explicitly; no default is pinned.
#[test]
fn i80_the_mirror_layout_memo_mints_the_slow_paths_mirrors() {
    let make_vm = |memo: bool| {
        let mut config = VmConfig::default();
        config.class_mirror_layout_memo = memo;
        Arc::new(SharedVm::new(config))
    };
    let shape = |shared: &SharedVm, mirror: ObjectRef| -> Vec<String> {
        (0..shared.mem.heap.num_fields(mirror))
            .map(|i| match shared.mem.heap.get_field(mirror, i) {
                // Which object differs per VM; that a slot holds one does not.
                Value::Object(Some(_)) => "obj".to_string(),
                other => format!("{other:?}"),
            })
            .collect()
    };
    let off = make_vm(false);
    let on = make_vm(true);
    for cid in [1u32, 2, 3, 1] {
        let a = get_or_create_class_mirror(&off, ClassId::new(cid));
        let b = get_or_create_class_mirror(&on, ClassId::new(cid));
        assert_eq!(shape(&*off, a), shape(&*on, b), "mirror of class {cid}");
    }
    let pa = get_or_create_primitive_mirror(&off, "int");
    let pb = get_or_create_primitive_mirror(&on, "int");
    assert_eq!(shape(&*off, pa), shape(&*on, pb), "int.class");
    assert!(
        off.classes.class_mirror_layout.get().is_none(),
        "the switch off never fills the memo"
    );
    if let Some(layout) = on.classes.class_mirror_layout.get() {
        let cm = on.classes.class_manager.read();
        let class = cm
            .get_class(layout.class_class_id)
            .expect("the memo names a loaded class");
        assert_eq!(&*class.name, "java/lang/Class");
        assert!(
            !class.origin.is_compatibility_stub(),
            "a stub `Class` never fills the memo"
        );
        assert_eq!(class.num_total_fields, layout.mirror_field_count);
    }
}

/// Interpreter round i6 wave 115, lane L5:
/// `CRATONVM_CLASS_MIRROR_ONE_LOADER_LOOKUP` mints the mirrors the three
/// lookups mint (the same slots set, both execution policies).
///
/// Wave 116: a slot is compared by what it holds -- null / zero, or an
/// object -- not by the `Value` variant it reads back as. Two VMs of one
/// process need not allocate `java/lang/Class` the same way: the real-JDK
/// pair (`jdk_only`) read one mirror as a compact object (an unwritten
/// primitive slot reads `Int(0)`) and the other as a legacy one (an
/// unwritten slot reads `Object(None)`). That choice is the heap's
/// (`plan_object_alloc` asks the layout registry) and is made at the
/// allocation, before any code the switch gates runs; the switch only
/// writes `classLoader` / `module` (references) and the mirror pin.
#[test]
fn i115_one_loader_lookup_mints_the_same_mirrors() {
    for jdk_only in [false, true] {
        let make_vm = |one: bool| {
            let mut config = VmConfig::default();
            config.class_mirror_one_loader_lookup = one;
            if jdk_only {
                config.compatibility_mode =
                    cratonvm_types::compat::CompatibilityMode::JdkOnly;
                config.use_synthetic_jdk = false;
            }
            Arc::new(SharedVm::new(config))
        };
        let shape = |shared: &SharedVm, mirror: ObjectRef| -> Vec<String> {
            (0..shared.mem.heap.num_fields(mirror))
                .map(|i| match shared.mem.heap.get_field(mirror, i) {
                    Value::Object(Some(_)) => "obj".to_string(),
                    Value::Object(None) | Value::Int(0) | Value::Long(0) => "zero".to_string(),
                    Value::Float(0.0) => "zero".to_string(),
                    Value::Double(0.0) => "zero".to_string(),
                    other => format!("{other:?}"),
                })
                .collect()
        };
        let off = make_vm(false);
        let on = make_vm(true);
        for cid in [1u32, 2, 3, 1] {
            let a = get_or_create_class_mirror(&off, ClassId::new(cid));
            let b = get_or_create_class_mirror(&on, ClassId::new(cid));
            assert_eq!(
                shape(&*off, a),
                shape(&*on, b),
                "class {cid}, jdk_only {jdk_only}"
            );
        }
    }
}
