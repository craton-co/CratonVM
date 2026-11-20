use super::*;

/// A live `JitInvokeInfo` for `P.f(I)I` of the given kind. Leaked: an
/// `Op::Call` bakes the address and `self_recursive_call_row` reads it.
fn info(invoke_kind: u8) -> usize {
    let info: &'static JitInvokeInfo = Box::leak(Box::new(JitInvokeInfo {
        class_name: "P",
        method_name: "f",
        descriptor: "(I)I",
        num_jit_args: 1,
        return_type: b'I',
        invoke_kind,
        declaring_class_id: 7,
        owner_class_id: 0,
    }));
    info as *const JitInvokeInfo as usize
}

fn self_site(base: usize, code_len: usize) -> IrInlineSite {
    IrInlineSite {
        base,
        code_len,
        num_args: 1,
        max_locals: 1,
        arg_local_slots: vec![0],
        returns_value: true,
        receiver_is_arg0: false,
        method_key: "P.f:(I)I".to_string(),
        class_id: 7,
    }
}

/// A self site takes a copy while the budget lasts and becomes a leaf
/// bound to the caller's own kind-4 row after that; anything that is not
/// exactly the compiling method (another method, an instance method, an
/// unknown class id, a same-named class with another id) is `NotSelf`.
#[test]
fn a_self_recursion_plan_splices_up_to_its_budget_then_leaves() {
    let row = (0x1000usize, 1usize, b'I');
    let plan = IrSelfRecursion::new("P", "f", "(I)I", 7, row);
    assert_eq!(plan.copies(), 0);
    let IrSelfSplice::Splice(one) = plan.classify("P", "f", "(I)I", 7, true) else {
        panic!("the first self copy splices");
    };
    assert_eq!(one.copies(), 1);
    let IrSelfSplice::Splice(two) = one.classify("P", "f", "(I)I", 7, true) else {
        panic!("the second self copy splices (IR_RECURSIVE_INLINE_MAX_COPIES = 2)");
    };
    assert_eq!(two.copies(), IR_RECURSIVE_INLINE_MAX_COPIES);
    assert_eq!(
        two.classify("P", "f", "(I)I", 7, true),
        IrSelfSplice::Leaf(row),
        "past the budget the site is a direct self-call",
    );
    // A non-self site passes the plan through unchanged.
    for (c, m, d, id, st) in [
        ("P", "g", "(I)I", 7u32, true),
        ("P", "f", "(J)I", 7, true),
        ("Q", "f", "(I)I", 7, true),
        ("P", "f", "(I)I", 8, true),
        ("P", "f", "(I)I", 0, true),
        ("P", "f", "(I)I", 7, false),
    ] {
        assert_eq!(
            two.classify(c, m, d, id, st),
            IrSelfSplice::NotSelf,
            "{c}.{m}{d} id={id} static={st}"
        );
    }
    // A zero budget makes every self site a leaf straight away.
    let none = IrSelfRecursion::new("P", "f", "(I)I", 7, row).with_max_copies(0);
    assert_eq!(
        none.classify("P", "f", "(I)I", 7, true),
        IrSelfSplice::Leaf(row)
    );
    // An unknown compiling class id matches nothing.
    let unknown = IrSelfRecursion::new("P", "f", "(I)I", 0, row);
    assert_eq!(
        unknown.classify("P", "f", "(I)I", 0, true),
        IrSelfSplice::NotSelf
    );
}

/// The row is the caller's own kind-4 row at the lowest caller pc; a
/// kind-3 row and a row past `code_len` (a spliced body's) are ignored.
#[test]
fn the_self_recursive_row_is_the_callers_kind_4_row() {
    let k4_low = info(4);
    let k4_high = info(4);
    let k3 = info(3);
    let spliced = info(4);
    let mut b = IrBuilder::new(1, 1);
    b.set_invoke_info(HashMap::from([
        (1usize, (k3, 1usize, b'I')),
        (3usize, (k4_low, 1usize, b'I')),
        (6usize, (k4_high, 1usize, b'I')),
        (12usize, (spliced, 1usize, b'I')),
    ]));
    assert_eq!(b.self_recursive_call_row(9), Some((k4_low, 1, b'I')));
    let mut none = IrBuilder::new(1, 1);
    none.set_invoke_info(HashMap::from([(1usize, (k3, 1usize, b'I'))]));
    assert_eq!(none.self_recursive_call_row(9), None);
}

/// `static int f(int n) { return f(n - 1) + 1; }` with ONE self copy
/// spliced at its call and the copy's own self-call bound to the caller's
/// kind-4 row: the graph has exactly one `Op::Call`, at the copy's pc, and
/// it carries that row -- i.e. it lowers as this method's direct
/// self-call. This is the plan `lib.rs` builds for a leaf.
#[test]
fn a_leaf_row_inside_a_self_copy_builds_the_direct_self_call() {
    let body: [u8; 9] = [
        0x1a, // 0: iload_0
        0x04, // 1: iconst_1
        0x64, // 2: isub
        0xb8, 0x00, 0x02, // 3: invokestatic #2 (f)
        0x04, // 6: iconst_1
        0x60, // 7: iadd
        0xac, // 8: ireturn
    ];
    let code_len = body.len();
    let mut combined = body.to_vec();
    let base = combined.len();
    combined.extend_from_slice(&body);
    combined.extend_from_slice(&[0, 0]);

    let k4 = info(4);
    let row = (k4, 1usize, b'I');
    let mut b = IrBuilder::new(1, 1);
    b.set_invoke_info(HashMap::from([(3usize, row)]));
    assert_eq!(b.self_recursive_call_row(code_len), Some(row));
    let tables = IrInlineTables {
        sites: HashMap::from([(3usize, self_site(base, code_len))]),
        invoke_info: HashMap::from([(base + 3, row)]),
        ..IrInlineTables::default()
    };
    b.apply_inline_tables(tables);
    let graph = b
        .build(&combined, code_len)
        .expect("a self copy with a leaf self-call builds");
    let calls: Vec<(Option<usize>, usize)> = graph
        .nodes
        .iter()
        .filter_map(|n| match n.op {
            Op::Call { info_ptr } => Some((n.bytecode_pc, info_ptr)),
            _ => None,
        })
        .collect();
    assert_eq!(
        calls,
        vec![(Some(base + 3), k4)],
        "the spliced pc 3 emits no call; the copy's pc does, with the kind-4 row",
    );
    // The build published where the copy's pcs resume.
    let map = splice_resume_map_this_build().expect("a build with a splice publishes its map");
    assert_eq!(map.resume_bci(base + 3), Some(3));
    assert_eq!(map.resume_bci(2), Some(2));
}

/// The resume map sends a nested body's pc to the OUTERMOST invoke, and
/// answers `None` for a pc in no body or a caller chain that never reaches
/// the caller's code.
#[test]
fn the_splice_resume_map_walks_to_the_outermost_invoke() {
    // Caller 10 bytes: invoke at 2 -> body [10, 20); inside it an invoke
    // at 13 -> body [20, 26); invoke at 5 -> body [26, 30).
    let sites = HashMap::from([
        (2usize, self_site(10, 10)),
        (13usize, self_site(20, 6)),
        (5usize, self_site(26, 4)),
    ]);
    let map = IrSpliceResumeMap::from_sites(10, &sites);
    assert_eq!(map.bodies, vec![(10, 10, 2), (20, 6, 13), (26, 4, 5)]);
    assert_eq!(map.resume_bci(4), Some(4), "a caller pc resumes at itself");
    assert_eq!(map.resume_bci(10), Some(2));
    assert_eq!(map.resume_bci(19), Some(2));
    assert_eq!(map.resume_bci(21), Some(2), "nested: the outermost invoke");
    assert_eq!(map.resume_bci(29), Some(5));
    assert_eq!(map.resume_bci(30), None, "past every body");
    // A malformed table whose chain loops never reaches caller code.
    let looped = IrSpliceResumeMap {
        code_len: 10,
        bodies: vec![(10, 10, 15)],
    };
    assert_eq!(looped.resume_bci(12), None);
}

/// A build with no splice publishes no map, and the results scope clears
/// one a previous build left.
#[test]
fn a_build_without_a_splice_publishes_no_resume_map() {
    set_splice_resume_map_this_build(Some(IrSpliceResumeMap::default()));
    let code = [0x04, 0xac, 0, 0]; // iconst_1; ireturn
    let _ = IrBuilder::new(0, 0).build(&code, 2).expect("build");
    assert_eq!(splice_resume_map_this_build(), None);
    set_splice_resume_map_this_build(Some(IrSpliceResumeMap::default()));
    drop(IrBuildResultsScope::enter());
    assert_eq!(splice_resume_map_this_build(), None);
}

// ── Round 11 (lane irback) ───────────────────────────────────────────

/// A declared `max_locals` smaller than the parameter count is a build
/// refusal, not an index panic in `IrBuilder::new` on the compile thread.
#[test]
fn r11_a_frame_too_small_for_its_parameters_is_refused_not_a_panic() {
    let code = [0x1a, 0xac, 0, 0]; // iload_0; ireturn
    assert!(IrBuilder::new(3, 1).build(&code, 2).is_none());
    // The well-formed frame still builds.
    assert!(IrBuilder::new(3, 3).build(&code, 2).is_some());
}

/// A loop header whose fall-through is DEAD (it follows a `return`) must
/// not inherit the dead arm's locals.
///
/// ```text
/// static int f(int c) {
///     if (c != 0) { int t = c * 3; return t; }
///     do { c--; } while (c > 5);
///     return c;
/// }
///  0: iload_0   1: ifeq 10   4: iload_0  5: iconst_3  6: imul
///  7: istore_1  8: iload_1   9: ireturn
/// 10: iinc 0,-1              <- loop header, reached only by `ifeq` and
/// 13: iload_0  14: iconst_5  15: if_icmpgt 10   the back edge
/// 18: iload_0  19: ireturn
/// ```
///
/// Slot 1 (`t`) is unassigned on both edges into the header, but
/// `activate_loop_header` skipped it without clearing it, so every
/// snapshot inside the loop named the `imul` from the returned arm — a
/// node that does not dominate the loop.
#[test]
fn r11_a_loop_header_after_a_return_does_not_inherit_the_dead_arms_locals() {
    let code = [
        0x1a, 0x99, 0x00, 0x09, 0x1a, 0x06, 0x68, 0x3c, 0x1b, 0xac, 0x84, 0x00, 0xff, 0x1a,
        0x08, 0xa3, 0xff, 0xfb, 0x1a, 0xac, 0, 0,
    ];
    let g = IrBuilder::new(1, 2).build(&code, 20).expect("build");
    let mul = g
        .nodes
        .iter()
        .position(|n| n.op == Op::Mul)
        .expect("the returned arm computes c * 3") as NodeId;
    for sp in &g.safepoints {
        if (10..20).contains(&sp.bci) {
            assert!(
                !sp.locals.contains(&mul),
                "the snapshot at bci {} names n{mul} (the dead arm's `t`) in {:?}",
                sp.bci,
                sp.locals
            );
        }
    }
}
