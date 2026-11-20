use super::*;

type Answer = Option<(String, String, String, u8, u32)>;
type Row = Option<(u32, u32)>;

/// `invokevirtual` cp 9 is `MethodHandle.invokeExact(I)I`.
fn names(cp: u16) -> Option<(String, String, String)> {
    (cp == 9).then(|| {
        (
            "java/lang/invoke/MethodHandle".into(),
            "invokeExact".into(),
            "(I)I".into(),
        )
    })
}

fn plus(_invoke_cp: u16, _pc: usize, field_cp: u16) -> Answer {
    (field_cp == 3).then(|| ("p/Acc".into(), "plus".into(), "(I)I".into(), 4, 0x7474))
}

fn never(_: usize) -> bool {
    false
}

fn row(invoke_cp: u16, pc: usize, field_cp: u16) -> Row {
    (invoke_cp == 9 && pc == 4 && field_cp == 3).then_some((5, 0x7575))
}

fn no_row(_: u16, _: usize, _: u16) -> Row {
    None
}

#[test]
fn jr21_a_bound_constant_handle_site_needs_its_row() {
    // 0: getstatic #3  3: iload_1  4: invokevirtual #9  7: ireturn
    let code = [0xb2, 0, 3, 0x1b, 0xb6, 0, 9, 0xac];
    let ops = [(4usize, 9u16, 0xb6u8)];
    let plan = |answer: &dyn Fn(u16, usize, u16) -> Answer,
                gates: (bool, bool),
                trapped: &dyn Fn(usize) -> bool| {
        ir_mh_constant_plan(
            Some(answer),
            Some(&names),
            &code,
            code.len(),
            &[],
            &ops,
            gates,
            ("p/Caller", "loop", "(I)J"),
            trapped,
        )
    };
    // Planned, then kept only with a row.
    assert_eq!(plan(&plus, (true, true), &never).site_count(), 1);
    assert!(plan(&plus, (true, true), &never)
        .with_bound_rows(None)
        .is_empty());
    assert!(plan(&plus, (true, true), &never)
        .with_bound_rows(Some(&no_row))
        .is_empty());
    let planned = plan(&plus, (true, true), &never).with_bound_rows(Some(&row));
    assert_eq!(planned.site_count(), 1);
    assert_eq!(
        planned.names(3),
        Some(("p/Acc".into(), "plus".into(), "(I)I".into()))
    );
    assert_eq!(planned.rewritten_invoke_ops(&ops), vec![(4, 3, 0xb6)]);
    assert_eq!(planned.receiver_pcs().get(&4), Some(&0));
    assert_eq!(planned.bound_slots().get(&4), Some(&5));
    assert_eq!(planned.bound_receiver_class_at(4), Some(0x7575));
    assert!(planned.interface_pcs().is_empty());
    // The virtual gate off, or a trapped receiver guard: nothing.
    assert!(plan(&plus, (true, false), &never).is_empty());
    assert!(plan(&plus, (true, true), &|pc| pc == 4).is_empty());
    // A member that is not the site's own descriptor: nothing.
    let wide = |_: u16, _: usize, _: u16| -> Answer {
        Some(("p/Acc".into(), "plus".into(), "(J)I".into(), 4, 0x7474))
    };
    assert!(plan(&wide, (true, true), &never).is_empty());
    // A non-bound site never asks the row and keeps its plan.
    let stat = |_: u16, _: usize, _: u16| -> Answer {
        Some(("p/Host".into(), "twice".into(), "(I)I".into(), 3, 0x7171))
    };
    let kept = plan(&stat, (true, true), &never).with_bound_rows(Some(&no_row));
    assert_eq!(kept.site_count(), 1);
    assert!(kept.bound_slots().is_empty());
    assert_eq!(kept.bound_receiver_class_at(4), None);
}
