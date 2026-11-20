use super::{
    compile_java_regex, is_matcher_realjdk_native_callback,
    matcher_realjdk_build_offset_tables, matcher_realjdk_capture_layout_valid,
    matcher_realjdk_group_index_in_bounds, matcher_realjdk_group_slice,
    matcher_realjdk_native_callback,
};
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};

#[test]
fn cached_group_slice_preserves_utf16_boundaries() {
    let text = "x😀value12,y";
    let (_, utf16_to_byte) = matcher_realjdk_build_offset_tables(text);
    assert_eq!(
        matcher_realjdk_group_slice(text, &utf16_to_byte, 3, 10),
        Some("value12")
    );
    assert_eq!(
        matcher_realjdk_group_slice(text, &utf16_to_byte, 2, 10),
        None
    );
    assert_eq!(
        matcher_realjdk_group_slice(text, &utf16_to_byte, 1, 2),
        None
    );
}

#[test]
fn cached_group_slice_ascii_identity_without_offset_table() {
    let text = "id=42,value=109,";
    assert_eq!(matcher_realjdk_group_slice(text, &[], 3, 5), Some("42"));
    assert_eq!(matcher_realjdk_group_slice(text, &[], 12, 15), Some("109"));
    assert_eq!(matcher_realjdk_group_slice(text, &[], 0, 0), Some(""));
    assert_eq!(matcher_realjdk_group_slice(text, &[], 5, 3), None);
    assert_eq!(matcher_realjdk_group_slice(text, &[], 0, 99), None);
}

#[test]
fn jit_dispatch_resolver_covers_only_real_layout_matcher_intrinsics() {
    for (name, descriptor) in [
        ("find", "()Z"),
        ("find", "(I)Z"),
        ("start", "()I"),
        ("start", "(I)I"),
        ("end", "()I"),
        ("end", "(I)I"),
        ("group", "()Ljava/lang/String;"),
        ("group", "(I)Ljava/lang/String;"),
    ] {
        let callback = matcher_realjdk_native_callback(name, descriptor).unwrap();
        assert!(is_matcher_realjdk_native_callback(callback));
    }
    assert!(matcher_realjdk_native_callback("matches", "()Z").is_none());
    assert!(
        matcher_realjdk_native_callback("group", "(Ljava/lang/String;)Ljava/lang/String;")
            .is_none()
    );
}

#[test]
fn capture_range_visitor_reports_std_and_fancy_offsets() {
    let std = compile_java_regex(r"value(\d+),", 0).unwrap();
    let mut std_ranges = Vec::new();
    assert_eq!(
        std.visit_capture_ranges_at("xvalue12,y", 1, |i, range| {
            assert_eq!(i, std_ranges.len());
            std_ranges.push(range);
        }),
        Some(2)
    );
    assert_eq!(std_ranges, vec![Some((1, 9)), Some((6, 8))]);

    let fancy = compile_java_regex(r"(?=(value(\d+),))", 0).unwrap();
    let mut fancy_ranges = Vec::new();
    assert_eq!(
        fancy.visit_capture_ranges_at("xvalue12,y", 1, |i, range| {
            assert_eq!(i, fancy_ranges.len());
            fancy_ranges.push(range);
        }),
        Some(3)
    );
    assert_eq!(fancy_ranges, vec![Some((1, 1)), Some((1, 9)), Some((6, 8))]);
}

#[test]
fn overallocated_groups_array_is_capacity_not_capture_count() {
    assert!(matcher_realjdk_capture_layout_valid(2, 2, 20));
    assert!(matcher_realjdk_group_index_in_bounds(1, 2, 20));
    assert!(!matcher_realjdk_group_index_in_bounds(2, 2, 20));
}

#[test]
fn layout_rejects_semantic_count_mismatch_or_short_capacity() {
    assert!(!matcher_realjdk_capture_layout_valid(3, 2, 20));
    assert!(!matcher_realjdk_capture_layout_valid(2, 3, 20));
    assert!(!matcher_realjdk_capture_layout_valid(2, 2, 3));
    assert!(!matcher_realjdk_capture_layout_valid(0, 0, 0));
    assert!(!matcher_realjdk_group_index_in_bounds(-1, 2, 20));
    assert!(!matcher_realjdk_group_index_in_bounds(1, 2, 3));
}
