use super::*;

#[test]
fn no_fall_through_has_no_cause() {
    reset_ir_fall_through_signal();
    note_ir_pipeline_entered();
    note_ir_rowless_field_site_bail(4, 0xb2);
    assert_eq!(last_compile_fall_through_cause(), None);
}

#[test]
fn a_rowless_build_bail_is_the_fall_through_cause() {
    reset_ir_fall_through_signal();
    note_ir_pipeline_entered();
    note_ir_rowless_field_site_bail(9, 0xb4);
    note_single_pass_entered();
    assert_eq!(
        last_compile_fall_through_cause(),
        Some(IrFallThroughCause::RowlessFieldSite { pc: 9, opcode: 0xb4 })
    );
}

#[test]
fn a_later_attempt_answers_for_itself() {
    reset_ir_fall_through_signal();
    note_ir_pipeline_entered();
    note_ir_rowless_field_site_bail(9, 0xb4);
    // A second optimizing attempt of the same compile that bails elsewhere.
    note_ir_pipeline_entered();
    note_single_pass_entered();
    assert_eq!(
        last_compile_fall_through_cause(),
        Some(IrFallThroughCause::Unclassified)
    );
    // The next compile starts clean.
    reset_ir_fall_through_signal();
    assert_eq!(last_compile_fall_through_cause(), None);
}
