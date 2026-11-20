use super::*;

type Answer = Option<IrIndyMutableAnswer>;

const OWN: (&str, &str, &str) = ("p/Gen", "add", "(II)I");

fn guarded(
    class: &str,
    name: &str,
    desc: &str,
    kind: u8,
    id: u32,
    words: (usize, usize, usize, u32),
) -> Answer {
    Some(((class.into(), name.into(), desc.into(), kind, id), words, (0, 0), 0))
}

/// mcs21 (MCS21-2): the VM's `CallSite.target` row rides the answer into
/// the planned guard, and two instructions of one entry that disagree
/// about it plan nothing (the whole guard is compared).
#[test]
fn mcs21_the_guard_row_rides_the_answer_into_the_plan() {
    let with_row = |row: u32| {
        move |_: u16, _: usize| -> Answer {
            Some((
                ("p/Host".into(), "s".into(), "(II)I".into(), 3, 0x7101),
                WORDS,
                (0, 0),
                row,
            ))
        }
    };
    let plan_of = |r: &dyn Fn(u16, usize) -> Answer, ops: &[(usize, u16)]| {
        let mut plan = IrIndyDirectPlan::default();
        ir_indy_mutable_plan_into(&mut plan, Some(r), ops, true, true, OWN, true);
        plan
    };
    let named = with_row(17);
    let plan = plan_of(&named, &[(4, 7)]);
    assert_eq!(plan.mutable_guards()[&4].field_row, 17);
    let none = with_row(0);
    assert_eq!(plan_of(&none, &[(4, 7)]).mutable_guards()[&4].field_row, 0);
    let split = |_: u16, pc: usize| -> Answer {
        Some((
            ("p/Host".into(), "s".into(), "(II)I".into(), 3, 0x7101),
            WORDS,
            (0, 0),
            pc as u32,
        ))
    };
    assert!(
        plan_of(&split, &[(2, 7), (9, 7)]).is_empty(),
        "one entry, two rows: the guards differ, so nothing is planned"
    );
}

const WORDS: (usize, usize, usize, u32) = (0x5170, 0x6100, 0x6200, 3);

/// A mutable site joins the plan as its invoke (names, class context,
/// merged invoke ops) but NOT as a pseudo-opcode: the `0xba` arm builds it
/// from its guard. A constant entry keeps the constant plan's answer.
#[test]
fn r20p_a_mutable_site_is_planned_as_a_guard_not_an_opcode() {
    let never = |_: usize| false;
    let constant = |cp: u16, _pc: usize| -> Option<(String, String, String, u8, u32)> {
        (cp == 7).then(|| ("p/Host".into(), "c".into(), "(II)I".into(), 3, 0x7101))
    };
    let mut plan =
        ir_indy_direct_plan(Some(&constant), &[(4, 7), (12, 8)], true, true, OWN, &never);
    let mutable = |cp: u16, _pc: usize| -> Answer {
        match cp {
            // Never consulted: the constant plan owns cp 7.
            7 => guarded("p/Host", "other", "(II)I", 3, 0x7101, WORDS),
            8 => guarded("p/Host", "inst", "(I)I", 0, 0x7102, WORDS),
            _ => None,
        }
    };
    ir_indy_mutable_plan_into(
        &mut plan,
        Some(&mutable),
        &[(4, 7), (12, 8)],
        true,
        true,
        OWN,
        true,
    );
    assert_eq!(plan.site_count(), 2);
    assert_eq!(
        plan.names(7),
        Some(("p/Host".into(), "c".into(), "(II)I".into())),
        "the constant entry is untouched"
    );
    assert_eq!(plan.class_context_at(12), Some(0x7102));
    let opcodes = plan.pseudo_opcodes();
    assert_eq!(opcodes.get(&4), Some(&0xb8));
    assert_eq!(
        opcodes.get(&12),
        None,
        "a guarded site is built by the 0xba arm"
    );
    let guards = plan.mutable_guards();
    assert_eq!(guards.len(), 1);
    let g = guards[&12];
    assert_eq!(
        (
            g.bridge_site,
            g.call_site_slot,
            g.target_slot,
            g.field_index
        ),
        WORDS
    );
    assert!(g.virtual_target);
    assert_eq!(
        plan.merged_invoke_ops(&[]),
        vec![(4, 7, 0xb8), (12, 8, 0xb6)],
        "both join the invoke plan"
    );
}

/// Nothing is planned without a resolver, without the bridge (the miss
/// edge), for a gated-off kind, a refused shape, a self-call, a zero word,
/// or instructions of one entry that disagree about the guard.
#[test]
fn r20p_a_mutable_site_without_its_terms_keeps_the_bridge() {
    fn plan_of(
        r: &dyn Fn(u16, usize) -> Answer,
        ops: &[(usize, u16)],
        wired: bool,
        gates: (bool, bool),
    ) -> IrIndyDirectPlan {
        let mut plan = IrIndyDirectPlan::default();
        ir_indy_mutable_plan_into(&mut plan, Some(r), ops, gates.0, gates.1, OWN, wired);
        plan
    }
    let st = |_: u16, _: usize| guarded("p/Host", "s", "(II)I", 3, 0x7101, WORDS);
    assert!(!plan_of(&st, &[(4, 7)], true, (true, true)).is_empty());
    let mut none = IrIndyDirectPlan::default();
    ir_indy_mutable_plan_into(&mut none, None, &[(4, 7)], true, true, OWN, true);
    assert!(none.is_empty());
    assert!(
        plan_of(&st, &[(4, 7)], false, (true, true)).is_empty(),
        "bridge not wired"
    );
    assert!(
        plan_of(&st, &[(4, 7)], true, (false, true)).is_empty(),
        "static gate off"
    );
    let virt = |_: u16, _: usize| guarded("p/Host", "v", "(I)I", 0, 0x7102, WORDS);
    assert!(
        plan_of(&virt, &[(4, 7)], true, (true, false)).is_empty(),
        "virtual gate off"
    );
    let me = |_: u16, _: usize| guarded("p/Gen", "add", "(II)I", 3, 0x7103, WORDS);
    assert!(
        plan_of(&me, &[(4, 7)], true, (true, true)).is_empty(),
        "self-call"
    );
    let zero =
        |_: u16, _: usize| guarded("p/Host", "s", "(II)I", 3, 0x7101, (0x5170, 0, 0x6200, 3));
    assert!(
        plan_of(&zero, &[(4, 7)], true, (true, true)).is_empty(),
        "zero word"
    );
    let split = |_: u16, pc: usize| {
        guarded(
            "p/Host",
            "s",
            "(II)I",
            3,
            0x7101,
            (0x5170 + pc, 0x6100, 0x6200, 3),
        )
    };
    assert!(
        plan_of(&split, &[(2, 7), (9, 7)], true, (true, true)).is_empty(),
        "two instructions of one entry with different bridge sites"
    );
    let same = plan_of(&st, &[(2, 7), (9, 7)], true, (true, true));
    assert_eq!(
        same.mutable_guards().len(),
        2,
        "one entry, one guard, both pcs"
    );
}
