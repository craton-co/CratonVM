use super::*;

/// `getstatic #1; ireturn` (a builder fixture): no `static_field_info` row.
const GETSTATIC: [u8; 6] = [0xb2, 0x00, 0x01, 0xac, 0, 0];
/// `aconst_null; getfield #1; ireturn`: no `field_info` row.
const GETFIELD: [u8; 7] = [0x01, 0xb4, 0x00, 0x01, 0xac, 0, 0];

fn build(code: &[u8], code_len: usize, trap_pc: Option<usize>) -> Option<Graph> {
    let mut b = IrBuilder::new(0, 0);
    if let Some(pc) = trap_pc {
        b.set_field_trap_sites(HashMap::from([(pc, b'I')]));
    }
    b.build(code, code_len)
}

#[test]
fn an_admitted_getstatic_builds_a_trap_at_its_bci() {
    if !ir_site_trap_enabled() {
        return;
    }
    let g = build(&GETSTATIC, 4, Some(0)).expect("the trap keeps the method");
    assert!(g.nodes.iter().any(|n| matches!(n.op, Op::Guard { bci: 0 })));
    assert!(!g.nodes.iter().any(|n| matches!(n.op, Op::LoadStatic { .. })));
    assert_eq!(g.trap_cause_at(0), Some(TrapCause::UnresolvedField));
}

#[test]
fn an_admitted_getfield_builds_a_trap_at_its_bci() {
    if !ir_site_trap_enabled() {
        return;
    }
    let g = build(&GETFIELD, 5, Some(1)).expect("the trap keeps the method");
    assert!(g.nodes.iter().any(|n| matches!(n.op, Op::Guard { bci: 1 })));
    assert_eq!(g.trap_cause_at(1), Some(TrapCause::UnresolvedField));
}

#[test]
fn a_site_not_admitted_bails_as_before() {
    assert!(build(&GETSTATIC, 4, None).is_none());
    assert!(build(&GETSTATIC, 4, Some(3)).is_none(), "admitted at another pc");
}

#[test]
fn the_cause_has_its_own_code_and_census_row() {
    assert_eq!(TrapCause::UnresolvedField.as_str(), "unresolved-field");
    assert_eq!(
        TrapCause::from_code(TrapCause::UnresolvedField.code()),
        Some(TrapCause::UnresolvedField)
    );
    assert!(ir_trap_census().iter().any(|(c, _)| *c == "unresolved-field"));
    assert!(ir_trap_refusal_census()
        .iter()
        .any(|(c, _)| *c == "unresolved-field"));
}
