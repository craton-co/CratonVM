use super::*;

fn row(method_name: &'static str, descriptor: &'static str) -> JitInvokeInfo {
    JitInvokeInfo {
        class_name: "java/lang/foreign/MemorySegment",
        method_name,
        descriptor,
        num_jit_args: 3,
        return_type: b'I',
        invoke_kind: 2,
        declaring_class_id: 0,
        owner_class_id: 0,
    }
}

/// r23 w3 review: an index site passes its index through; an offset site
/// converts a width-aligned byte offset (negative included: the road
/// declines it) and declines any other offset to the JDK body.
#[test]
fn offsets_convert_only_when_aligned() {
    let int_get = "(Ljava/lang/foreign/ValueLayout$OfInt;J)I";
    if cratonvm_jit::ffm_kind_for_descriptor(int_get).is_none() {
        // `CRATONVM_JIT_NO_FFM_INTRINSIC=1` in this environment.
        return;
    }
    let at = row("getAtIndex", int_get);
    assert_eq!(ffm_element_site_kind_index(&at, 5), Some((FFM_KIND_INT, 5)));
    let off = row("get", int_get);
    assert_eq!(ffm_element_site_kind_index(&off, 8), Some((FFM_KIND_INT, 2)));
    assert_eq!(ffm_element_site_kind_index(&off, 6), None);
    assert_eq!(ffm_element_site_kind_index(&off, -4), Some((FFM_KIND_INT, -1)));
    let byte_set = row("set", "(Ljava/lang/foreign/ValueLayout$OfByte;JB)V");
    assert_eq!(ffm_element_site_kind_index(&byte_set, 7), Some((FFM_KIND_BYTE, 7)));
    let long_get = row("get", "(Ljava/lang/foreign/ValueLayout$OfLong;J)J");
    assert_eq!(ffm_element_site_kind_index(&long_get, 12), None);
    assert_eq!(ffm_element_site_kind_index(&long_get, 16), Some((FFM_KIND_LONG, 2)));
}
