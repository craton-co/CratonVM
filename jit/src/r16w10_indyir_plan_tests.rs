use super::*;

type Answer = Option<(String, String, String, u8, u32)>;

fn call(class: &str, name: &str, desc: &str, kind: u8, id: u32) -> Answer {
    Some((class.into(), name.into(), desc.into(), kind, id))
}

const OWN: (&str, &str, &str) = ("p/Gen", "add", "(II)I");

fn never(_: usize) -> bool {
    false
}

/// A planned site joins the invoke plan as its invoke, in pc order, with
/// the target class as its class context; an unplanned one is untouched.
#[test]
fn r16w10_a_constant_site_joins_the_invoke_plan_as_its_invoke() {
    let resolver = |cp: u16, _pc: usize| -> Answer {
        match cp {
            7 => call("p/Host", "addImpl", "(II)I", 3, 0x7101),
            8 => call("p/Host", "inst", "(I)I", 0, 0x7102),
            _ => None,
        }
    };
    let plan = ir_indy_direct_plan(
        Some(&resolver),
        &[(4, 7), (12, 8), (20, 9)],
        true,
        true,
        OWN,
        &never,
    );
    assert_eq!(plan.site_count(), 2);
    assert_eq!(
        plan.names(7),
        Some(("p/Host".into(), "addImpl".into(), "(II)I".into()))
    );
    assert_eq!(plan.class_id(8), Some(0x7102));
    assert_eq!(plan.class_context_at(4), Some(0x7101));
    assert_eq!(plan.class_context_at(20), None, "cp 9 keeps its bridge");
    let ops = plan.merged_invoke_ops(&[(0, 3, 0xb7), (16, 5, 0xb6)]);
    assert_eq!(
        ops,
        vec![(0, 3, 0xb7), (4, 7, 0xb8), (12, 8, 0xb6), (16, 5, 0xb6)]
    );
    let opcodes = plan.pseudo_opcodes();
    assert_eq!(opcodes.get(&4), Some(&0xb8));
    assert_eq!(opcodes.get(&12), Some(&0xb6));
    assert_eq!(opcodes.len(), 2);
}

/// Nothing is planned without a resolver, for a gated-off call kind, for
/// a shape the plan would refuse the method over, or for a self-call.
#[test]
fn r16w10_a_site_that_could_cost_the_method_its_plan_keeps_the_bridge() {
    let none = ir_indy_direct_plan(None, &[(4, 7)], true, true, OWN, &never);
    assert!(none.is_empty());
    assert_eq!(none.merged_invoke_ops(&[(0, 3, 0xb8)]), vec![(0, 3, 0xb8)]);
    let st = |_: u16, _: usize| call("p/Host", "addImpl", "(II)I", 3, 0x7101);
    assert!(ir_indy_direct_plan(Some(&st), &[(4, 7)], false, true, OWN, &never).is_empty());
    let virt = |_: u16, _: usize| call("p/Host", "inst", "(I)I", 0, 0x7102);
    assert!(ir_indy_direct_plan(Some(&virt), &[(4, 7)], true, false, OWN, &never).is_empty());
    let me = |_: u16, _: usize| call("p/Gen", "add", "(II)I", 3, 0x7103);
    assert!(ir_indy_direct_plan(Some(&me), &[(4, 7)], true, true, OWN, &never).is_empty());
    let no_id = |_: u16, _: usize| call("p/Host", "addImpl", "(II)I", 3, 0);
    assert!(ir_indy_direct_plan(Some(&no_id), &[(4, 7)], true, true, OWN, &never).is_empty());
    let special = |_: u16, _: usize| call("p/Host", "addImpl", "(II)I", 1, 0x7101);
    assert!(ir_indy_direct_plan(Some(&special), &[(4, 7)], true, true, OWN, &never).is_empty());
    // A virtual site whose null guard already trapped keeps the bridge; a
    // static one has no guard to have trapped.
    let trapped = |pc: usize| pc == 4;
    assert!(ir_indy_direct_plan(Some(&virt), &[(4, 7)], true, true, OWN, &trapped).is_empty());
    assert!(!ir_indy_direct_plan(Some(&virt), &[(8, 7)], true, true, OWN, &trapped).is_empty());
    assert!(!ir_indy_direct_plan(Some(&st), &[(4, 7)], true, true, OWN, &trapped).is_empty());
}

/// Two instructions of one entry (`addTwice`) are planned only when both
/// answer the same call: the plan's resolvers are asked by the entry.
#[test]
fn r16w10_instructions_sharing_an_entry_must_agree() {
    let same = |_: u16, _: usize| call("p/Host", "addImpl", "(II)I", 3, 0x7101);
    let plan = ir_indy_direct_plan(Some(&same), &[(2, 7), (9, 7)], true, true, OWN, &never);
    assert_eq!(plan.site_count(), 2);
    assert_eq!(plan.class_context_at(9), Some(0x7101));
    let split = |_: u16, pc: usize| -> Answer {
        if pc == 2 {
            call("p/Host", "addImpl", "(II)I", 3, 0x7101)
        } else {
            call("p/Host", "subImpl", "(II)I", 3, 0x7101)
        }
    };
    assert!(
        ir_indy_direct_plan(Some(&split), &[(2, 7), (9, 7)], true, true, OWN, &never)
            .is_empty()
    );
    let half = |_: u16, pc: usize| -> Answer {
        (pc == 2)
            .then(|| call("p/Host", "addImpl", "(II)I", 3, 0x7101))
            .flatten()
    };
    assert!(
        ir_indy_direct_plan(Some(&half), &[(2, 7), (9, 7)], true, true, OWN, &never).is_empty()
    );
    assert!(
        ir_indy_direct_plan(Some(&half), &[(9, 7), (2, 7)], true, true, OWN, &never).is_empty()
    );
}
