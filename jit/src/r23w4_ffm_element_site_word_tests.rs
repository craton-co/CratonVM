use super::*;

#[test]
fn the_site_word_carries_the_kind_and_the_coordinate_form() {
    const SEG: &str = "java/lang/foreign/MemorySegment";
    if ffm_kind_for_descriptor("(Ljava/lang/foreign/ValueLayout$OfInt;J)I").is_none() {
        return; // `CRATONVM_JIT_NO_FFM_INTRINSIC=1` in this environment.
    }
    let key = concat!("CRATON", "VM_JIT_FFM_UNALIGNED_OFFSET_ACCESS");
    cratonvm_types::flags::with_thread_overrides(&[(key, None)], || {
        let w = ffm_element_site_word(SEG, "get", "(Ljava/lang/foreign/ValueLayout$OfInt;J)I");
        assert_eq!(
            w,
            FFM_ELEMENT_SITE_VALID
                | FFM_ELEMENT_SITE_BYTE_OFFSET
                | FFM_ELEMENT_SITE_UNALIGNED_OK
                | 3,
            "default ON: an offset site may take a raw byte offset"
        );
        let w = ffm_element_site_word(SEG, "getAtIndex", "(Ljava/lang/foreign/ValueLayout$OfInt;J)I");
        assert_eq!(w, FFM_ELEMENT_SITE_VALID | 3, "an index site never does");
    });
    assert_eq!(
        ffm_element_site_word(SEG, "get", "(Ljava/lang/foreign/ValueLayout$OfBoolean;J)Z"),
        0
    );
    assert_eq!(
        ffm_element_site_word(SEG, "asSlice", "(Ljava/lang/foreign/ValueLayout$OfInt;J)I"),
        0
    );
    assert_eq!(
        ffm_element_site_word("java/lang/invoke/VarHandle", "get", "(Ljava/lang/foreign/ValueLayout$OfInt;J)I"),
        0
    );
    let rec = VarHandleJudgedSite::new(
        1,
        2,
        SEG,
        "setAtIndex",
        "(Ljava/lang/foreign/ValueLayout$OfShort;JS)V",
        4,
        b'V',
        2,
        0,
        0,
    );
    assert_eq!(rec.ffm_element, FFM_ELEMENT_SITE_VALID | 1);
    let rec = VarHandleJudgedSite::new(1, 2, "java/lang/invoke/VarHandle", "get", "()I", 1, b'I', 0, 0, 0);
    assert_eq!(rec.ffm_element, 0);
}
