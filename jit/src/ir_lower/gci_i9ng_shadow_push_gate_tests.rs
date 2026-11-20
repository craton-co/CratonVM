//! gci i9/ng (i8/nt review item 6): the IR tier's per-call shadow push is
//! gated on the shadow-stack codegen, as the frame block and the
//! single-pass backend already were.
use super::*;

#[test]
fn gci_i9ng_shadow_push_needs_the_shadow_codegen() {
    let on = || true;
    let off = || false;
    assert!(Lowerer::shadow_push_emits(0x1000, 16, 24, 3, on), "wired, codegen on");
    assert!(
        !Lowerer::shadow_push_emits(0x1000, 16, 24, 3, off),
        "shadow emission off: no push, so no overflow bail"
    );
}

#[test]
fn gci_i9ng_shadow_push_wiring_is_checked_before_the_codegen_switch() {
    let never = || -> bool { panic!("the switch is read only for a wired push") };
    assert!(!Lowerer::shadow_push_emits(0, 16, 24, 3, never), "no thread helper");
    assert!(!Lowerer::shadow_push_emits(0x1000, 0, 24, 3, never), "no thread slot");
    assert!(!Lowerer::shadow_push_emits(0x1000, 16, 0, 3, never), "no savebase slot");
    assert!(!Lowerer::shadow_push_emits(0x1000, 16, 24, 0, never), "nothing to publish");
}
