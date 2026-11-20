use super::HwAtomicKind;

#[test]
fn float_and_double_cells_are_never_published_to_the_unsafe_rmw_prefixes() {
    assert!(HwAtomicKind::Int.publishes_unsafe_rmw_cell());
    assert!(HwAtomicKind::Long.publishes_unsafe_rmw_cell());
    assert!(HwAtomicKind::Reference.publishes_unsafe_rmw_cell());
    assert!(!HwAtomicKind::Float.publishes_unsafe_rmw_cell());
    assert!(!HwAtomicKind::Double.publishes_unsafe_rmw_cell());
}

#[test]
fn float_and_double_kinds_match_their_cells() {
    assert!(HwAtomicKind::Float.matches_descriptor(b'F'));
    assert!(!HwAtomicKind::Float.matches_descriptor(b'I'));
    assert!(HwAtomicKind::Double.matches_descriptor(b'D'));
    assert!(!HwAtomicKind::Double.matches_descriptor(b'J'));
    // `Value` tags (`field_cell_layout_matches_value_enum`): Float 2, Double 3.
    assert_eq!(HwAtomicKind::Float.legacy_tag(), 2);
    assert_eq!(HwAtomicKind::Double.legacy_tag(), 3);
    assert_eq!(HwAtomicKind::Float.width(), 4);
    assert_eq!(HwAtomicKind::Double.width(), 8);
    assert_eq!(
        HwAtomicKind::Float.payload_offset(),
        cratonvm_types::FIELD_CELL_PAYLOAD32_OFFSET
    );
    assert_eq!(
        HwAtomicKind::Double.payload_offset(),
        cratonvm_types::FIELD_CELL_PAYLOAD64_OFFSET
    );
}

/// The publish filter sits on the one publication site, and asks the kind.
#[test]
fn the_publication_site_filters_by_kind() {
    let src = concat!(include_str!("../vm_exec/mod.rs"), "\n", include_str!("../vm_exec/method.rs"), "\n", include_str!("../vm_exec/report_pad_access.rs"), "\n", include_str!("../vm_exec/diagnose_sync_on_value_based_class.rs"), "\n", include_str!("../vm_exec/thread_interrupted_field_slot_of.rs"), "\n", include_str!("../vm_exec/clear_breakpoints_of_redefined_class.rs"), "\n", include_str!("../vm_exec/load_class.rs"), "\n", include_str!("../vm_exec/native_new_initializes_class_first.rs"), "\n", include_str!("../vm_exec/new_object.rs"), "\n", include_str!("../vm_exec/gen_referent_read_keep_alive.rs"), "\n", include_str!("../vm_exec/append_native_standin_frames.rs"), "\n", include_str!("../vm_exec/unloaded_native_libraries.rs"), "\n", include_str!("../vm_exec/extract_annotations_from_attributes.rs"), "\n", include_str!("../vm_exec/invoke_static_settling_with_first.rs"), "\n", include_str!("../vm_exec/invoke_special_shared.rs"), "\n", include_str!("../vm_exec/proxy_resolve_declaring_method.rs"), "\n", include_str!("../vm_exec/invoke_on_class_shared_inner.rs"), "\n", include_str!("../vm_exec/jni_table_native_call.rs"));
    let start = src.find("fn hw_atomic_addr_checked(").expect("hw_atomic_addr_checked moved");
    let body = &src[start..];
    let body = &body[..body.find("\n}\n").expect("function end")];
    let filter = body
        .find("publish.filter(|_| kind.publishes_unsafe_rmw_cell())")
        .expect("the kind filter on the publication");
    let first_publish = body
        .find("note_compact_ref_field(")
        .expect("the reference publication");
    assert!(filter < first_publish, "the filter must precede every publication");
    assert_eq!(body.matches("note_compact_field(").count(), 1);
}

/// Wave 4c: the sub-word lookup has no path into the `Unsafe` RMW cell
/// tables, and the legacy narrowing is `putfield`'s.
#[test]
fn sub_word_cells_are_never_published_and_narrow_like_putfield() {
    let src = concat!(include_str!("../vm_exec/mod.rs"), "\n", include_str!("../vm_exec/method.rs"), "\n", include_str!("../vm_exec/report_pad_access.rs"), "\n", include_str!("../vm_exec/diagnose_sync_on_value_based_class.rs"), "\n", include_str!("../vm_exec/thread_interrupted_field_slot_of.rs"), "\n", include_str!("../vm_exec/clear_breakpoints_of_redefined_class.rs"), "\n", include_str!("../vm_exec/load_class.rs"), "\n", include_str!("../vm_exec/native_new_initializes_class_first.rs"), "\n", include_str!("../vm_exec/new_object.rs"), "\n", include_str!("../vm_exec/gen_referent_read_keep_alive.rs"), "\n", include_str!("../vm_exec/append_native_standin_frames.rs"), "\n", include_str!("../vm_exec/unloaded_native_libraries.rs"), "\n", include_str!("../vm_exec/extract_annotations_from_attributes.rs"), "\n", include_str!("../vm_exec/invoke_static_settling_with_first.rs"), "\n", include_str!("../vm_exec/invoke_special_shared.rs"), "\n", include_str!("../vm_exec/proxy_resolve_declaring_method.rs"), "\n", include_str!("../vm_exec/invoke_on_class_shared_inner.rs"), "\n", include_str!("../vm_exec/jni_table_native_call.rs"));
    let start = src.find("fn hw_subword_cell(").expect("hw_subword_cell moved");
    let body = &src[start..];
    let body = &body[..body.find("\n}\n").expect("function end")];
    for publisher in ["note_compact_field(", "note_compact_ref_field(", "unsafe_inline", "hw_atomic_addr"] {
        assert!(!body.contains(publisher), "the sub-word lookup must not reach `{publisher}`");
    }
    // JIT round 21, lane atomics (UNSAFE21-2): the hardware sub-word CAS
    // publishes into the sub-word half only.
    let start = src
        .find("fn hw_cas_subword(")
        .expect("hw_cas_subword moved");
    let cas = &src[start..];
    let cas = &cas[..cas.find("\n}\n").expect("function end")];
    assert_eq!(cas.matches("note_compact_subword_field(").count(), 1);
    for publisher in [
        "note_compact_field(",
        "note_compact_ref_field(",
        "hw_atomic_addr",
    ] {
        assert!(
            !cas.contains(publisher),
            "the sub-word CAS must not reach `{publisher}`"
        );
    }
    use super::narrow_subword;
    assert_eq!(narrow_subword(b'B', 0x1FF), -1);
    assert_eq!(narrow_subword(b'B', 0x7F), 0x7F);
    assert_eq!(narrow_subword(b'S', 0x1_8000), -32768);
    assert_eq!(narrow_subword(b'C', -1), 0xFFFF);
    // Wave 5c: a boolean cell's CAS compares and stores the raw byte.
    assert_eq!(narrow_subword(b'Z', 3), 3);
    assert_eq!(narrow_subword(b'Z', 0x102), 2);
}

/// Wave 5b: a static cell's tag / payload encoding round-trips every kind
/// the hardware static RMW serves, bit-exactly for floats, and refuses the
/// rest (`ReturnAddress`, `Uninitialized`).
#[test]
fn static_cell_bits_round_trip() {
    use super::{static_cell_bits, static_cell_value};
    use cratonvm_types::Value;
    let nan = f64::from_bits(0x7ff8_0000_0000_0001);
    for v in [
        Value::Int(-7),
        Value::Long(i64::MIN),
        Value::Float(-0.0),
        Value::Double(nan),
        Value::Object(None),
    ] {
        let (tag, bits) = static_cell_bits(v).expect("served kind");
        let back = static_cell_value(tag, bits).expect("decodes");
        assert_eq!(static_cell_bits(back), Some((tag, bits)), "{v:?}");
    }
    assert_eq!(static_cell_bits(Value::Uninitialized), None);
    assert_eq!(static_cell_bits(Value::ReturnAddress(3)), None);
    assert_eq!(static_cell_value(6, 0), None);
}
