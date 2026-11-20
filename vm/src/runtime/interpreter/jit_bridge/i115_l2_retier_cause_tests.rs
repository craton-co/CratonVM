use super::retier_cause_admits;
use cratonvm_jit::IrFallThroughCause;

const ROWLESS: Option<IrFallThroughCause> = Some(IrFallThroughCause::RowlessFieldSite {
    pc: 7,
    opcode: 0xb2,
});

#[test]
fn switch_off_admits_every_cause() {
    assert!(retier_cause_admits(ROWLESS, || false));
    assert!(retier_cause_admits(Some(IrFallThroughCause::Unclassified), || false));
    assert!(retier_cause_admits(None, || false));
}

#[test]
fn switch_on_admits_only_a_rowless_field_site() {
    assert!(retier_cause_admits(ROWLESS, || true));
    assert!(!retier_cause_admits(Some(IrFallThroughCause::Unclassified), || true));
    assert!(!retier_cause_admits(None, || true));
}

#[test]
fn a_rowless_cause_never_reads_the_switch() {
    assert!(retier_cause_admits(ROWLESS, || panic!("switch read")));
}
