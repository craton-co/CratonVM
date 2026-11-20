use super::*;
use crate::config::{GcAlgorithm, VmConfig};

/// Source pin: the Generational remark notes the clock right before its
/// reference round, under the G1 switch's reader.
#[test]
fn gci_i48_zl2_the_gen_remark_notes_the_soft_clock_before_its_round() {
    let src = super::GC_AND_ALLOC_SRC;
    let start = src
        .find("fn gen_remark_process_references(")
        .expect("gen_remark_process_references");
    let end = start + src[start..].find("\n\u{7d}\n").expect("its end");
    let body = &src[start..end];
    let note = body
        .find("gen_remark_note_soft_clock(shared);")
        .expect("the note");
    let rows = body
        .find("remark_reference_rows(shared, is_marked, retained)")
        .expect("the round");
    assert!(note < rows, "noted before the round consumes it");
    let start = src
        .find("fn gen_remark_note_soft_clock(")
        .expect("gen_remark_note_soft_clock");
    let end = start + src[start..].find("\n\u{7d}\n").expect("its end");
    let helper = &src[start..end];
    assert!(helper.contains("no_refproc() || !g1_remark_soft_clock_now_enabled()"));
    assert!(helper.contains(".note_round_clock_now()"));
}
