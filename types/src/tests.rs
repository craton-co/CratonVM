use super::*;

/// Verify that all public re-exports from lib.rs are accessible and usable.
#[test]
fn reexport_class_id() {
    let id = ClassId::new(42);
    assert_eq!(id.as_u32(), 42);
}

#[test]
fn reexport_class_loader_id() {
    let _ = ClassLoaderId::Bootstrap;
    let _ = ClassLoaderId::Extension;
    let _ = ClassLoaderId::Application;
}

/// `compat.rs` is only reachable because of the `pub mod compat;` above.
/// Without it the module is orphaned: `native-api`, `classloading`, `vm`
/// and `vm-cli` all name `cratonvm_types::compat::CompatibilityMode`, and
/// every one of them fails to compile. The root re-export is the second
/// half — `vm/src/config.rs` re-exports it onward, so dropping it is the
/// same class of silent breakage the heap-constant test below guards.
#[test]
fn reexport_compat_policy() {
    assert_eq!(CompatibilityMode::default(), CompatibilityMode::Compatible);
    assert!(CompatibilityMode::JdkOnly.is_jdk_only());
    assert_eq!(CompatibilityMode::JdkOnly.as_str(), "jdk-only");

    let policy = ExecutionPolicy::jdk_only();
    assert!(policy.is_jdk_only());
    assert!(policy.real_jdk);
    assert!(!ExecutionPolicy::default().is_jdk_only());

    // The two paths to the same type must be the same type, not a copy.
    let via_module: compat::CompatibilityMode = CompatibilityMode::JdkOnly;
    assert_eq!(via_module, CompatibilityMode::JdkOnly);
}

#[test]
fn reexport_value_types() {
    let v = Value::Int(1);
    assert_eq!(v.as_int(), Some(1));

    let (bits, tag) = encode_value(Value::Long(99));
    assert_eq!(decode_value(bits, tag).as_long(), Some(99));
}

#[test]
fn reexport_value_tags() {
    assert_eq!(VTAG_INT, 0);
    assert_eq!(VTAG_LONG, 1);
    assert_eq!(VTAG_FLOAT, 2);
    assert_eq!(VTAG_DOUBLE, 3);
    assert_eq!(VTAG_OBJECT, 4);
    assert_eq!(VTAG_NULL, 5);
    assert_eq!(VTAG_UNINIT, 6);
    assert_eq!(VTAG_RETADDR, 7);
}

#[test]
fn reexport_is_object_tag() {
    assert!(is_object_tag(VTAG_OBJECT));
    assert!(!is_object_tag(VTAG_NULL));
    assert!(!is_object_tag(VTAG_INT));
}

/// The re-exported heap constants, checked against the properties that the
/// rest of the workspace actually depends on.
///
/// This used to open with `assert_eq!(HEADER_SIZE, 32);`. That pinned one
/// historical *value* rather than any invariant: it records what the header
/// happened to be, so a deliberate, fully-correct shrink trips it and a
/// careless change that keeps the size but breaks alignment does not. It
/// also cannot say *why* 32 mattered, which is the only thing the person
/// reading the failure needs.
///
/// Replaced — not deleted — with the three properties that are load-bearing
/// and that a layout change can actually violate:
///
/// 1. `HEADER_SIZE` stays on the 8-byte grid. Every heap walk, the inline
///    TLAB cursor bump and the qword body-zeroing loop step from it, and
///    the 8-byte `mark_word` must stay naturally aligned for the CAS on the
///    lock fast path.
/// 2. `HEADER_SIZE <= 127`. The JIT emits array element addressing as
///    `[base + index*scale + HEADER_SIZE]` with a **signed** `disp8`. Past
///    127 the byte is read back as negative and the emitted load addresses
///    memory *before* the object — no panic, just a wrong address. Same for
///    `ARRAY_LENGTH_OFFSET`, which is the `disp8` of the bounds-check load.
/// 3. The header is big enough to hold the fields whose offsets are also
///    exported, so no exported offset can point outside it.
///
/// `heap_types.rs` pins (2) at compile time; keeping it here as well means
/// the constraint is stated where the constant is *published*, and the
/// failure message names the emitter that breaks.
#[test]
fn reexport_heap_constants() {
    assert_eq!(
        HEADER_SIZE % 8,
        0,
        "HEADER_SIZE anchors the 8-byte object grid: heap walks, the inline \
         TLAB cursor bump and the qword body-zeroing loop all step from it"
    );
    assert!(
        COMPACT_HEADER_SIZE >= MARK_WORD_OFFSET + 4,
        "the 4-byte mark word must fit inside the header word"
    );
    assert!(
        HEADER_SIZE <= 127,
        "HEADER_SIZE is emitted as a signed disp8 in the JIT's array element \
         addressing (jit/src/x64.rs and jit/src/ir_lower.rs); above 127 the \
         displacement byte reads as negative and the load addresses memory \
         before the object"
    );
    assert!(
        ARRAY_LENGTH_OFFSET <= 127,
        "ARRAY_LENGTH_OFFSET is the signed disp8 of the JIT's array-length \
         load; same failure mode as HEADER_SIZE above"
    );
    assert!(ARRAY_LENGTH_OFFSET > 0);
    // The length word bounds against the *data* offset, not the header
    // size. They are the same today; they stop being the same at
    // HEADER_SIZE = 16, where the length moves into an 8-byte prefix at the
    // head of the array's body and only `ARRAY_DATA_OFFSET` still sits past
    // it. Stating it against `HEADER_SIZE` would make this assert fail on a
    // correct layout, which is the wrong way for an invariant to break.
    assert!(ARRAY_LENGTH_OFFSET + 4 <= ARRAY_DATA_OFFSET);
    assert!(
        ARRAY_DATA_OFFSET >= HEADER_SIZE && ARRAY_DATA_OFFSET % 8 == 0,
        "array data starts at or past the header end, on the 8-byte grid"
    );
    assert!(
        ARRAY_DATA_OFFSET <= 127,
        "ARRAY_DATA_OFFSET is the signed disp8 of the JIT's array element \
         addressing; same failure mode as HEADER_SIZE above"
    );
    assert_eq!(SLOT_SIZE, 16);
    assert_eq!(REF_ELEMENT_SIZE, 8);
    assert_eq!(AUTOBOX_CLASS_ID.as_u32(), u32::MAX);
}

/// `mod heap_types` is private. Anything it declares `pub` but that is left
/// off the `pub use heap_types::{…}` list above is unreachable from every
/// other crate in the workspace — the code exists and nothing can call it,
/// which is a default-off landing arrived at by omission rather than by
/// choice.
///
/// That is not hypothetical: `MARK_FORWARDED`, `FORWARDING_PTR_MASK` and
/// `IDENTITY_HASH_CODE_OFFSET` all landed in `heap_types.rs` on 2026-07-26
/// and were stranded exactly this way. Naming them here means the
/// re-export cannot be dropped again without failing to compile.
#[test]
fn every_public_heap_constant_is_reachable() {
    // The mark-word encoding for a relocated object. Consumers that match
    // on `ObjectHeader::mark_state(m)` need the tag constant itself, not
    // just the `is_forwarded_mark` helper.
    assert_eq!(MARK_FORWARDED & MARK_STATE_MASK, MARK_FORWARDED);
    assert_ne!(MARK_FORWARDED, MARK_NEUTRAL);
    assert_ne!(MARK_FORWARDED, MARK_THIN_LOCKED);
    assert_ne!(MARK_FORWARDED, MARK_INFLATED);

    // Forwarding keeps the header word and parks the target in the second
    // word, which every object has.
    assert_eq!(FORWARDING_TARGET_OFFSET, 8);
    assert!(MIN_OBJECT_SIZE >= FORWARDING_TARGET_OFFSET + 8);
    assert_eq!(MARK_FWD_SELF & MARK_STATE_MASK, 0);
    assert_eq!(MARK_FWD_BUSY & MARK_STATE_MASK, 0);

    // The last header field that had no named constant. `jit/src/x64.rs`
    // derived its own via `offset_of!` and `vm/src/jit/helpers.rs` still
    // writes a bare `raw_ptr.add(8)`; both should use this.

    // The JIT's inline `checkcast` fast path bakes this byte as an
    // immediate, so the packing it assumes is asserted here rather than
    // trusted: `kind` in bits 0..1, `element_type` in bits 2..5, bits 6..7
    // reserved zero. A `byte[]` is 0x21 and nothing else is.
    assert_eq!(primitive_array_kind_tags_byte("[B"), Some(0x21));
    assert_eq!(
        primitive_array_kind_tags_byte("[I"),
        Some(ObjectKind::Array as u8 | ((ArrayElementType::Int as u8) << 2)),
    );
    for d in ["[Z", "[C", "[F", "[D", "[B", "[S", "[I", "[J"] {
        let tag = primitive_array_kind_tags_byte(d).expect(d);
        assert_eq!(tag & KIND_TAG_BYTE_MASK, ObjectKind::Array as u8);
        assert_eq!(tag & 0xC0, 0, "bits 6..7 are reserved zero: {d}");
    }
    // ONE dimension only. `[[B` holds references to `byte[]` objects, so
    // its element type is `Reference` and this predicate must not claim it;
    // reference arrays and plain classes have no answer here either.
    for d in [
        "[[B",
        "[Ljava/lang/String;",
        "java/lang/String",
        "[",
        "",
        "B",
    ] {
        assert_eq!(primitive_array_kind_tags_byte(d), None, "{d}");
    }
}

#[test]
fn reexport_heap_types() {
    let _ = ObjectKind::Object;
    let _ = ObjectKind::Array;
    let _ = ArrayElementType::Int;

    assert_eq!(element_byte_size(ArrayElementType::Int), 4);
    assert_eq!(array_data_size(10, ArrayElementType::Int).unwrap(), 40);
    assert_eq!(array_data_size_checked(10, ArrayElementType::Int), Some(40));
    assert_eq!(object_kind_from_tag(1), Some(ObjectKind::Array));
    assert_eq!(object_kind_from_tag(0x7f), None);
    assert_eq!(array_element_type_from_tag(10), Some(ArrayElementType::Int));
    assert_eq!(array_element_type_from_tag(0x7f), None);
}

#[test]
fn reexport_gc_flags() {
    assert_eq!(GC_FLAG_OLD_GEN, 0x01);
    assert_eq!(GC_FLAG_MARKED, 0x02);
}

#[test]
fn reexport_object_header() {
    let header = ObjectHeader::new(
        ClassId::new(0),
        ObjectKind::Object,
        ArrayElementType::Boolean,
        0,
        0,
    );
    assert!(!header.is_forwarded());
    assert!(!header.is_old_gen());
}

#[test]
fn reexport_object_ref() {
    let fake_ptr = 0x1000_u64 as *mut u8;
    let r = unsafe { ObjectRef::from_raw(fake_ptr) };
    assert_eq!(r.as_ptr() as u64, 0x1000);
}

#[test]
fn reexport_handle_types() {
    struct S {
        slots: Vec<Option<ObjectRef>>,
    }
    impl HandleStorage for S {
        fn root(&mut self, r: ObjectRef) -> u32 {
            self.slots.push(Some(r));
            (self.slots.len() - 1) as u32
        }
        fn unroot(&mut self, slot: u32) {
            self.slots[slot as usize] = None;
        }
        fn get(&self, slot: u32) -> ObjectRef {
            self.slots[slot as usize].unwrap()
        }
    }
    let mut storage = S { slots: Vec::new() };
    let fake_ptr = 0x2000_u64 as *mut u8;
    let r = unsafe { ObjectRef::from_raw(fake_ptr) };
    let handle = RootedHandle::new(&mut storage, r);
    assert_eq!(handle.get(&storage), r);
    let mut scope = HandleScope::new(&mut storage);
    let h2 = scope.root(r);
    assert_eq!(scope.get(&h2), r);
}
