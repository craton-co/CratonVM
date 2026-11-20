use super::*;

/// G-8: on the `CRATONVM_G1_REMARK_UNLOAD_AFTER_KEEP=0` arm the remark
/// callback marks the keep closure before the JNI weak-global clear.
#[test]
fn gci_i14gq_the_jni_weak_clear_follows_the_keep_marking_on_both_arms() {
    let src = GC_AND_ALLOC_SRC;
    let start = src
        .find("fn g1_final_remark_cleanup(")
        .expect("g1_final_remark_cleanup");
    let body = &src[start..];
    let keep = body
        .find("let keep = g1_remark_process_references(shared, is_live);")
        .expect("the rows");
    let mark = body
        .find("g1_remark_mark_kept_closure(shared, &keep);")
        .expect("the =0 arm marks the keep closure");
    let jni = body
        .find("clear_weak_global_refs_unmarked(shared, is_live, &keep)")
        .expect("the JNI clear");
    assert!(keep < mark && mark < jni);
}

/// Source pin: each `remark_reference_rows` call of
/// `g1_remark_process_references` is preceded by the note (both orders of
/// `CRATONVM_G1_REMARK_UNLOAD_AFTER_KEEP`).
#[test]
fn gci_i14gq_both_g1_remark_orders_note_the_round() {
    let src = GC_AND_ALLOC_SRC;
    let start = src
        .find("fn g1_remark_process_references(")
        .expect("g1_remark_process_references");
    let end = start
        + src[start..]
            .find("fn g1_remark_weak_before_final_enabled(")
            .expect("the switch reader follows the function");
    let body = &src[start..end];
    assert_eq!(
        body.matches("remark_reference_rows(shared, is_marked, &[])")
            .count(),
        2
    );
    assert_eq!(body.matches("g1_remark_note_round(shared);").count(), 2);
}
