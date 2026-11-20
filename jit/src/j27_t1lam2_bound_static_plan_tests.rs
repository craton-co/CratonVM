use super::*;

type Answer = Option<(String, String, String, u8, u32)>;
type Row = Option<(u32, u32)>;

/// `invokevirtual` cp 9 is `MethodHandle.invokeExact(I)J`.
fn names(cp: u16) -> Option<(String, String, String)> {
    (cp == 9).then(|| {
        (
            "java/lang/invoke/MethodHandle".into(),
            "invokeExact".into(),
            "(I)J".into(),
        )
    })
}

/// `static long addL(Long, int)` bound on its `Long`.
fn add_l(_invoke_cp: u16, _pc: usize, field_cp: u16) -> Answer {
    (field_cp == 3).then(|| {
        (
            "p/S".into(),
            "addL".into(),
            "(Ljava/lang/Long;I)J".into(),
            6,
            0x7171,
        )
    })
}

fn never(_: usize) -> bool {
    false
}

fn row(invoke_cp: u16, pc: usize, field_cp: u16) -> Row {
    (invoke_cp == 9 && pc == 4 && field_cp == 3).then_some((7, 0))
}

fn no_row(_: u16, _: usize, _: u16) -> Row {
    None
}

// 0: getstatic #3  3: iload_1  4: invokevirtual #9  7: lreturn
const CODE: [u8; 8] = [0xb2, 0, 3, 0x1b, 0xb6, 0, 9, 0xad];
const OPS: [(usize, u16, u8); 1] = [(4, 9, 0xb6)];

fn plan(answer: &dyn Fn(u16, usize, u16) -> Answer, gates: (bool, bool)) -> IrMhConstantPlan {
    ir_mh_constant_plan(
        Some(answer),
        Some(&names),
        &CODE,
        CODE.len(),
        &[],
        &OPS,
        gates,
        ("p/Caller", "loop", "(I)J"),
        &never,
    )
}

#[test]
fn a_bound_static_site_is_an_invokestatic_with_its_row() {
    let planned = plan(&add_l, (true, false)).with_bound_rows(Some(&row));
    assert_eq!(planned.site_count(), 1);
    assert_eq!(planned.rewritten_invoke_ops(&OPS), vec![(4, 3, 0xb8)]);
    assert_eq!(
        planned.names(3),
        Some(("p/S".into(), "addL".into(), "(Ljava/lang/Long;I)J".into()))
    );
    assert_eq!(planned.bound_slots().get(&4), Some(&7));
    assert!(planned.bound_static_pcs().contains(&4));
    // No receiver: the guarded splice never takes a class from it.
    assert_eq!(planned.bound_receiver_class_at(4), None);
    assert_eq!(planned.receiver_pcs().get(&4), Some(&0));
}

#[test]
fn a_bound_static_site_without_its_row_or_gate_is_dropped() {
    // Planned as a plain kind 3 it would call addL without its Long.
    assert!(plan(&add_l, (true, true)).with_bound_rows(None).is_empty());
    assert!(plan(&add_l, (true, true)).with_bound_rows(Some(&no_row)).is_empty());
    // The static gate off.
    assert!(plan(&add_l, (false, true)).is_empty());
    // A member whose descriptor is not the site's plus one leading
    // reference (the bound handle is at another type: the door's WMTE).
    let same = |_: u16, _: usize, _: u16| -> Answer {
        Some(("p/S".into(), "addL".into(), "(I)J".into(), 6, 0x7171))
    };
    assert!(plan(&same, (true, true)).is_empty());
    let prim = |_: u16, _: usize, _: u16| -> Answer {
        Some(("p/S".into(), "addL".into(), "(JI)J".into(), 6, 0x7171))
    };
    assert!(plan(&prim, (true, true)).is_empty());
}

#[test]
fn a_bound_virtual_site_is_not_a_static_one() {
    let plus = |_: u16, _: usize, field_cp: u16| -> Answer {
        (field_cp == 3).then(|| ("p/Acc".into(), "plus".into(), "(I)J".into(), 4, 0x7474))
    };
    let planned = plan(&plus, (true, true)).with_bound_rows(Some(&row));
    assert_eq!(planned.site_count(), 1);
    assert!(planned.bound_static_pcs().is_empty());
    assert_eq!(planned.rewritten_invoke_ops(&OPS), vec![(4, 3, 0xb6)]);
}
