use super::*;

type Answer = Option<(String, String, String, u8, u32)>;

const OWN: (&str, &str, &str) = ("p/Caller", "loop", "(I)J");

/// `invokevirtual` cp 9 is `MethodHandle.invokeExact(I)I`, cp 10 is
/// `MethodHandle.invokeExact()V`, cp 11 is `p/Box.add(I)I`.
fn names(cp: u16) -> Option<(String, String, String)> {
    let t = |c: &str, n: &str, d: &str| Some((c.into(), n.into(), d.into()));
    match cp {
        9 => t("java/lang/invoke/MethodHandle", "invokeExact", "(I)I"),
        10 => t("java/lang/invoke/MethodHandle", "invokeExact", "()V"),
        11 => t("p/Box", "add", "(I)I"),
        12 => t("java/lang/Integer", "valueOf", "(I)Ljava/lang/Integer;"),
        13 => t(
            "java/lang/invoke/MethodHandle",
            "invokeExact",
            "(Lp/Box;I)I",
        ),
        _ => None,
    }
}

fn twice(_invoke_cp: u16, _pc: usize, field_cp: u16) -> Answer {
    (field_cp == 3).then(|| ("p/Host".into(), "twice".into(), "(I)I".into(), 3, 0x7171))
}

/// No receiver null guard has failed anywhere.
fn never(_: usize) -> bool {
    false
}

fn shape(cp: u16) -> Option<(usize, bool)> {
    let (_, _, d) = names(cp)?;
    Some((count_param_slots(&d), return_type(&d) != b'V'))
}

/// `getstatic #3; iload_1; invokevirtual #9` -- the getstatic is the
/// receiver; so it is through an argument expression and a nested call.
#[test]
fn r16w17_a_getstatic_feeding_the_receiver_is_found() {
    // 0: getstatic #3  3: iload_1  4: invokevirtual #9
    let code = [0xb2, 0, 3, 0x1b, 0xb6, 0, 9, 0xac];
    let leaders = vec![false; code.len()];
    assert_eq!(
        mh_constant_receiver_getstatic(&code, code.len(), 4, 1, &leaders, &shape),
        Some((0, 3))
    );
    // 0: getstatic #3  3: iload_1  4: iconst_1  5: iadd
    // 6: invokestatic #12 (Integer.valueOf) 9: pop  10: iload_1
    // 11: invokevirtual #9
    let code = [
        0xb2, 0, 3, 0x1b, 0x04, 0x60, 0xb8, 0, 12, 0x57, 0x1b, 0xb6, 0, 9, 0xac,
    ];
    let leaders = vec![false; code.len()];
    assert_eq!(
        mh_constant_receiver_getstatic(&code, code.len(), 11, 1, &leaders, &shape),
        Some((0, 3))
    );
}

/// A join after the getstatic, a receiver from a local or a `dup`, and an
/// instruction the walk does not model all refuse.
#[test]
fn r16w17_anything_but_straight_line_code_refuses() {
    let code = [0xb2, 0, 3, 0x1b, 0xb6, 0, 9, 0xac];
    let mut leaders = vec![false; code.len()];
    leaders[3] = true;
    assert!(
        mh_constant_receiver_getstatic(&code, code.len(), 4, 1, &leaders, &shape).is_none()
    );
    leaders[3] = false;
    leaders[4] = true;
    assert!(
        mh_constant_receiver_getstatic(&code, code.len(), 4, 1, &leaders, &shape).is_none()
    );
    // 0: aload_0  1: iload_1  2: invokevirtual #9
    let local = [0x2a, 0x1b, 0xb6, 0, 9, 0xac];
    let none = vec![false; local.len()];
    assert!(mh_constant_receiver_getstatic(&local, local.len(), 2, 1, &none, &shape).is_none());
    // 0: getstatic #3  3: dup  4: invokevirtual #10 ()V  -- the receiver of
    // the call at 4 is the `dup`'s copy.
    let dup = [0xb2, 0, 3, 0x59, 0xb6, 0, 10, 0xb1];
    let none = vec![false; dup.len()];
    assert!(mh_constant_receiver_getstatic(&dup, dup.len(), 4, 0, &none, &shape).is_none());
    // 0: getstatic #3  3: iload_1  4: ifeq +3 ... a branch is not modelled.
    let branch = [0xb2, 0, 3, 0x1b, 0x99, 0, 3, 0x1b, 0xb6, 0, 9, 0xac];
    let none = vec![false; branch.len()];
    assert!(
        mh_constant_receiver_getstatic(&branch, branch.len(), 8, 1, &none, &shape).is_none()
    );
}

/// A planned site joins the invoke plan as an `invokestatic` keyed by its
/// field; the answer must be a static member of the site's own
/// descriptor, not this method, and agree across the field's sites.
#[test]
fn r16w17_a_constant_handle_site_is_planned_as_its_member() {
    // 0: getstatic #3  3: iload_1  4: invokevirtual #9  7: getstatic #3
    // 10: iload_1  11: invokevirtual #9  14: iadd  15: ireturn
    let code = [
        0xb2, 0, 3, 0x1b, 0xb6, 0, 9, 0xb2, 0, 3, 0x1b, 0xb6, 0, 9, 0x60, 0xac,
    ];
    let ops = [(4usize, 9u16, 0xb6u8), (11, 9, 0xb6)];
    let plan = ir_mh_constant_plan(
        Some(&twice),
        Some(&names),
        &code,
        code.len(),
        &[],
        &ops,
        (true, true),
        OWN,
        &never,
    );
    assert_eq!(plan.site_count(), 2);
    assert_eq!(
        plan.names(3),
        Some(("p/Host".into(), "twice".into(), "(I)I".into()))
    );
    assert_eq!(plan.class_id(3), Some(0x7171));
    assert_eq!(plan.class_context_at(11), Some(0x7171));
    assert_eq!(plan.receiver_pcs().get(&11), Some(&7));
    assert_eq!(
        plan.rewritten_invoke_ops(&ops),
        vec![(4, 3, 0xb8), (11, 3, 0xb8)]
    );
    // The static gate off, no resolver, another descriptor, a virtual or
    // self answer: nothing planned.
    let off = ir_mh_constant_plan(
        Some(&twice),
        Some(&names),
        &code,
        code.len(),
        &[],
        &ops,
        (false, false),
        OWN,
        &never,
    );
    assert!(off.is_empty());
    assert!(ir_mh_constant_plan(
        None,
        Some(&names),
        &code,
        code.len(),
        &[],
        &ops,
        (true, true),
        OWN,
        &never
    )
    .is_empty());
    let wide = |_: u16, _: usize, _: u16| -> Answer {
        Some(("p/Host".into(), "twice".into(), "(J)I".into(), 3, 0x7171))
    };
    assert!(ir_mh_constant_plan(
        Some(&wide),
        Some(&names),
        &code,
        code.len(),
        &[],
        &ops,
        (true, true),
        OWN,
        &never
    )
    .is_empty());
    let virt = |_: u16, _: usize, _: u16| -> Answer {
        Some(("p/Host".into(), "twice".into(), "(I)I".into(), 0, 0x7171))
    };
    assert!(ir_mh_constant_plan(
        Some(&virt),
        Some(&names),
        &code,
        code.len(),
        &[],
        &ops,
        (true, true),
        OWN,
        &never
    )
    .is_empty());
    let me = |_: u16, _: usize, _: u16| -> Answer {
        Some(("p/Caller".into(), "loop".into(), "(I)J".into(), 3, 0x7171))
    };
    let own_ops = [(4usize, 9u16, 0xb6u8)];
    assert!(ir_mh_constant_plan(
        Some(&me),
        Some(&names),
        &code,
        code.len(),
        &[],
        &own_ops,
        (true, true),
        ("p/Caller", "loop", "(I)J"),
        &never,
    )
    .is_empty());
    // A handler entry inside the window refuses the site.
    assert!(ir_mh_constant_plan(
        Some(&twice),
        Some(&names),
        &code,
        code.len(),
        &[3],
        &own_ops,
        (true, true),
        OWN,
        &never,
    )
    .is_empty());
    // Two sites of one field that answer differently: neither is planned.
    let split = |_: u16, pc: usize, _: u16| -> Answer {
        Some((
            "p/Host".into(),
            if pc == 4 { "twice" } else { "thrice" }.into(),
            "(I)I".into(),
            3,
            0x7171,
        ))
    };
    assert!(ir_mh_constant_plan(
        Some(&split),
        Some(&names),
        &code,
        code.len(),
        &[],
        &ops,
        (true, true),
        OWN,
        &never
    )
    .is_empty());
    // A non-handle invokevirtual is never asked.
    let plain = [(4usize, 11u16, 0xb6u8)];
    assert!(ir_mh_constant_plan(
        Some(&twice),
        Some(&names),
        &code,
        code.len(),
        &[],
        &plain,
        (true, true),
        OWN,
        &never
    )
    .is_empty());
}

/// JIT round 17 wave 3 (lane mh4; MHCONST17-2): a virtual handle's site is
/// planned as an `invokevirtual` of its member keyed by the field, under
/// the virtual-call gate, only when the site is the member's descriptor
/// with a leading receiver and its receiver guard never trapped.
#[test]
fn r17w3_a_virtual_constant_handle_site_is_planned_as_invokevirtual() {
    // 0: getstatic #3  3: aload_1  4: iload_2  5: invokevirtual #13
    // 8: ireturn
    let code = [0xb2, 0, 3, 0x2b, 0x1c, 0xb6, 0, 13, 0xac];
    let ops = [(5usize, 13u16, 0xb6u8)];
    let add = |_: u16, _: usize, _: u16| -> Answer {
        Some(("p/Box".into(), "add".into(), "(I)I".into(), 0, 0x7272))
    };
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
            OWN,
            trapped,
        )
    };
    let planned = plan(&add, (true, true), &never);
    assert_eq!(planned.site_count(), 1);
    assert_eq!(
        planned.names(3),
        Some(("p/Box".into(), "add".into(), "(I)I".into()))
    );
    assert_eq!(planned.class_context_at(5), Some(0x7272));
    assert_eq!(planned.receiver_pcs().get(&5), Some(&0));
    assert_eq!(planned.rewritten_invoke_ops(&ops), vec![(5, 3, 0xb6)]);
    // The virtual gate off, a trapped guard, or a member that is not the
    // site less its receiver: nothing planned.
    assert!(plan(&add, (true, false), &never).is_empty());
    assert!(plan(&add, (true, true), &|pc| pc == 5).is_empty());
    let wide = |_: u16, _: usize, _: u16| -> Answer {
        Some(("p/Box".into(), "add".into(), "(J)I".into(), 0, 0x7272))
    };
    assert!(plan(&wide, (true, true), &never).is_empty());
    // A static answer at this site is the static rule's: the member would
    // have to take the receiver too.
    let stat = |_: u16, _: usize, _: u16| -> Answer {
        Some(("p/Box".into(), "add".into(), "(I)I".into(), 3, 0x7272))
    };
    assert!(plan(&stat, (true, true), &never).is_empty());
    // No interface site in a virtual plan.
    assert!(planned.interface_pcs().is_empty());
}

/// JIT round 19 wave 1 (lane mhiface; MHRESID18-4): an interface handle's
/// site (`invoke_kind 2`) is planned by the virtual rule as an
/// `invokeinterface` row of its member keyed by the field, and its pc is
/// handed to the builder for the receiver's `instanceof` guard.
#[test]
fn r19w1_an_interface_constant_handle_site_is_planned_as_invokeinterface() {
    // 0: getstatic #3  3: aload_1  4: iload_2  5: invokevirtual #13
    // 8: ireturn
    let code = [0xb2, 0, 3, 0x2b, 0x1c, 0xb6, 0, 13, 0xac];
    let ops = [(5usize, 13u16, 0xb6u8)];
    let apply = |_: u16, _: usize, _: u16| -> Answer {
        Some(("p/Fn".into(), "apply".into(), "(I)I".into(), 2, 0x7373))
    };
    let plan = |gates: (bool, bool), trapped: &dyn Fn(usize) -> bool| {
        ir_mh_constant_plan(
            Some(&apply),
            Some(&names),
            &code,
            code.len(),
            &[],
            &ops,
            gates,
            OWN,
            trapped,
        )
    };
    let planned = plan((true, true), &never);
    assert_eq!(planned.site_count(), 1);
    assert_eq!(
        planned.names(3),
        Some(("p/Fn".into(), "apply".into(), "(I)I".into()))
    );
    assert_eq!(planned.class_context_at(5), Some(0x7373));
    assert_eq!(planned.rewritten_invoke_ops(&ops), vec![(5, 3, 0xb9)]);
    assert_eq!(
        planned.interface_pcs(),
        std::collections::HashSet::from([5usize])
    );
    // The virtual gate off or a trapped guard: nothing planned.
    assert!(plan((true, false), &never).is_empty());
    assert!(plan((true, true), &|pc| pc == 5).is_empty());
}

#[test]
fn r17w3_the_virtual_site_shape_is_the_member_after_one_reference() {
    assert!(mh_constant_virtual_site_matches("(Lp/Box;I)I", "(I)I"));
    assert!(mh_constant_virtual_site_matches("(Lp/Box;)V", "()V"));
    assert!(!mh_constant_virtual_site_matches("(I)I", "(I)I"));
    assert!(!mh_constant_virtual_site_matches("([Lp/Box;)V", "()V"));
    assert!(!mh_constant_virtual_site_matches("(Lp/Box;I)J", "(I)I"));
    assert!(!mh_constant_virtual_site_matches("(L;)V", "()V"));
}
