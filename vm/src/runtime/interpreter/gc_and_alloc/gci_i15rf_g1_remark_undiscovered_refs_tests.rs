use super::*;

/// Source pin: the set is computed before the dead-`Reference` pre-prune
/// and kept live by it, noted before the round, and its referents join the
/// keep list.
#[test]
fn gci_i15rf_remark_rows_keep_undiscovered_references_in_order() {
    let src = GC_AND_ALLOC_SRC.replace("\r\n", "\n");
    let at = src
        .find("fn remark_reference_rows(")
        .expect("remark_reference_rows");
    let body = &src[at..];
    let body = &body[..body.find("\n\u{7d}\n").expect("end")];
    let set = body
        .find("g1_remark_undiscovered_references(")
        .expect("the set");
    let live = body
        .find("|| undiscovered_refs.contains(&addr)")
        .expect("the pre-prune keeps the rows");
    let prune = body
        .find("ref_proc.remove_collected_reference_objects(&reference_is_live)")
        .expect("the pre-prune");
    let note = body
        .find("ref_proc.note_undiscovered_references(&refs);")
        .expect("the note");
    let round = body
        .find("ref_proc.process_references_with_finalizer_trace(")
        .expect("the round");
    let keep = body
        .find("resurrect.extend(undiscovered_referents);")
        .expect("the referents are kept");
    assert!(set < live && live < prune && prune < note && note < round && round < keep);
}

/// G-7: only a G1 evacuation pause that reports as itself takes HotSpot
/// G1's nested `gc,phases,ref` block; Generational / ZGC pauses and a pause
/// folded into a G1 `Pause Full` report keep the top-level block.
#[test]
fn gci_i15rf_reference_block_indent_rule() {
    assert_eq!(reference_block_indent_levels_rule(true, false), 2);
    assert_eq!(reference_block_indent_levels_rule(true, true), 0);
    assert_eq!(reference_block_indent_levels_rule(false, false), 0);
    assert_eq!(reference_block_indent_levels_rule(false, true), 0);
}

/// G-7 wiring: the stop-the-world round defers its block at the pause's
/// depth; the remark round keeps the top-level block.
#[test]
fn gci_i15rf_only_the_stw_round_indents_its_reference_block() {
    let src = GC_AND_ALLOC_SRC.replace("\r\n", "\n");
    let body = crate::runtime::interpreter::gc_and_alloc::process_references_text(&src);
    let body = body.as_str();
    assert!(body.contains("defer_xlog_reference_processing_at("));
    assert!(body.contains("reference_block_indent_levels(shared),"));
    let at = src
        .find("fn remark_reference_rows(")
        .expect("remark_reference_rows");
    let rows = &src[at..];
    let rows = &rows[..rows.find("\n\u{7d}\n").expect("end")];
    assert!(!rows.contains("defer_xlog_reference_processing_at("));
}
