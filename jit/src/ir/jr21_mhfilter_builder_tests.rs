use super::*;

fn row(
    method_name: &'static str,
    descriptor: &'static str,
    kind: u8,
    num_jit_args: usize,
    return_type: u8,
) -> usize {
    let info: &'static crate::JitInvokeInfo = Box::leak(Box::new(crate::JitInvokeInfo {
        class_name: "p/Host",
        method_name,
        descriptor,
        num_jit_args,
        return_type,
        invoke_kind: kind,
        declaring_class_id: 0x7101,
        owner_class_id: 0,
    }));
    info as *const crate::JitInvokeInfo as usize
}

fn build(
    code: &[u8],
    params: &[IrType],
    rows: &[(usize, (usize, usize, u8))],
    site_pc: usize,
    site: IrMhFilterSite,
) -> Option<Graph> {
    let mut builder = IrBuilder::new(params.len(), params.len());
    builder.set_param_types(params);
    builder.set_static_field_info(HashMap::from([(0usize, (0x5171u32, 3usize, b'L', false))]));
    builder.set_invoke_info(rows.iter().copied().collect());
    builder.set_mh_constant_sites(HashMap::from([(site_pc, 0usize)]));
    builder.set_mh_constant_filter_sites(HashMap::from([(site_pc, site)]));
    builder.build(code, code.len())
}

fn call_of(g: &Graph, info_ptr: usize) -> Option<NodeId> {
    g.nodes
        .iter()
        .position(|n| n.op == Op::Call { info_ptr })
        .and_then(|i| NodeId::try_from(i).ok())
}

fn site(position: Option<u16>, main_virtual: bool, target_slot: u32) -> IrMhFilterSite {
    IrMhFilterSite {
        pre_pc: 0,
        position,
        main_virtual,
        pre_virtual: false,
        target_slot,
    }
}

// 0: getstatic #1  3: iload_0  4: invokevirtual #2 (I)I  7: ireturn
const STATIC_SITE: [u8; 8] = [0xb2, 0x00, 0x01, 0x1a, 0xb6, 0x00, 0x02, 0xac];

#[test]
fn jr21_an_argument_filter_calls_the_filter_then_the_target() {
    let filter = row("neg", "(I)I", 3, 1, b'I');
    let target = row("twice", "(I)I", 3, 1, b'I');
    let rows = [(0, (filter, 1, b'I')), (4, (target, 1, b'I'))];
    let g = build(
        &STATIC_SITE,
        &[IrType::Int],
        &rows,
        4,
        site(Some(0), false, 0),
    )
    .expect("builds");
    let pre = call_of(&g, filter).expect("the filter's call");
    let main = call_of(&g, target).expect("the target's call");
    assert!(pre < main, "the filter first");
    assert_eq!(
        g.nodes[pre as usize].bytecode_pc,
        Some(0),
        "at the getstatic"
    );
    assert_eq!(g.nodes[pre as usize].inputs.len(), 3);
    assert_eq!(
        g.nodes[g.nodes[pre as usize].inputs[2] as usize].op,
        Op::Param(0)
    );
    assert_eq!(g.nodes[main as usize].bytecode_pc, Some(4));
    assert_eq!(
        g.nodes[main as usize].inputs,
        vec![g.nodes[main as usize].inputs[0], pre, pre],
        "[ctrl, mem = the filter's call, the filtered argument]"
    );
    assert_eq!(g.mh_constant_call_pcs, vec![4], "no post-call exit");
    let at_invoke = g
        .safepoints
        .iter()
        .find(|sp| sp.bci == 4)
        .expect("a snapshot at the invoke");
    assert_eq!(at_invoke.stack.len(), 2, "the handle and the argument");
    assert!(!g.nodes.iter().any(|n| matches!(n.op, Op::Guard { .. })));
}

/// j27 wave 2 (lane GCMAP): the PRE call's frame reports the
/// `invokeExact`'s bci (HotSpot's `StackFrame.getByteCodeIndex`, JDI
/// `Location.codeIndex`), while the node itself stays at the `getstatic`
/// pc its rows are keyed by; the `getstatic`'s own node still reports 0.
#[test]
fn j27_the_pre_call_reports_the_invoke_bci() {
    let filter = row("neg", "(I)I", 3, 1, b'I');
    let target = row("twice", "(I)I", 3, 1, b'I');
    let rows = [(0, (filter, 1, b'I')), (4, (target, 1, b'I'))];
    let g = build(
        &STATIC_SITE,
        &[IrType::Int],
        &rows,
        4,
        site(Some(0), false, 0),
    )
    .expect("builds");
    let pre = call_of(&g, filter).expect("the filter's call");
    let main = call_of(&g, target).expect("the target's call");
    assert_eq!(g.nodes[pre as usize].bytecode_pc, Some(0), "rows at the getstatic");
    assert_eq!(g.reported_call_bci(pre, 0), 4, "the PRE frame is the invoke's");
    assert_eq!(g.reported_call_bci(main, 4), 4);
    let field = g
        .nodes
        .iter()
        .position(|n| matches!(n.op, Op::LoadStatic { .. }))
        .and_then(|i| NodeId::try_from(i).ok())
        .expect("the handle's getstatic");
    assert_eq!(g.reported_call_bci(field, 0), 0, "only the call moves");
}

#[test]
fn jr21_a_filtered_receiver_is_guarded_with_the_target_handles_frame() {
    // 0: getstatic #1  3: aload_0  4: iload_1  5: invokevirtual #2
    // (Lp/Host;I)I  8: ireturn
    let code = [0xb2, 0x00, 0x01, 0x2a, 0x1b, 0xb6, 0x00, 0x02, 0xac];
    let params = [IrType::Ref, IrType::Int];
    let filter = row("swap", "(Lp/Host;)Lp/Host;", 3, 1, b'L');
    let target = row("plus", "(I)I", 0, 2, b'I');
    let rows = [(0, (filter, 1, b'L')), (5, (target, 2, b'I'))];
    let g = build(&code, &params, &rows, 5, site(Some(0), true, 4)).expect("builds");
    let pre = call_of(&g, filter).expect("the filter's call");
    let main = call_of(&g, target).expect("the target's call");
    assert_eq!(
        g.nodes[main as usize].inputs[2], pre,
        "the filtered receiver"
    );
    assert_eq!(
        g.nodes[g.nodes[main as usize].inputs[3] as usize].op,
        Op::Param(1)
    );
    let guards: Vec<&Node> = g
        .nodes
        .iter()
        .filter(|n| matches!(n.op, Op::Guard { bci: 5 }))
        .collect();
    assert_eq!(guards.len(), 1, "one null guard, after the filter");
    let named = guards[0].frame_snapshot.expect("the guard names its frame");
    let frame = &g.safepoints[named as usize];
    assert_eq!(frame.bci, 5);
    assert_eq!(frame.stack.len(), 3);
    let target_handle = &g.nodes[frame.stack[0] as usize];
    assert_eq!(
        target_handle.op,
        Op::Load(MemKind::Ref),
        "the species' argL0"
    );
    assert!(matches!(
        g.nodes[target_handle.inputs[2] as usize].op,
        Op::LoadStatic {
            class_id: 0x5171,
            field_index: 3,
            ..
        }
    ));
    assert_eq!(frame.stack[1], pre, "the filtered value");
    let first = g
        .safepoints
        .iter()
        .position(|sp| sp.bci == 5)
        .expect("the plain snapshot");
    assert!(first < named as usize, "the plain snapshot stays first");
    assert_eq!(g.safepoints[first].stack.len(), 3);
    assert_eq!(
        g.nodes[g.safepoints[first].stack[1] as usize].op,
        Op::Param(0)
    );
    assert!(g.synthetic_field_access_pcs.contains(&5));
    // Without the argL0 slot there is no frame to resume: refused.
    assert!(build(&code, &params, &rows, 5, site(Some(0), true, 0)).is_none());
}

#[test]
fn jr21_a_return_filter_calls_the_target_then_the_filter() {
    let target = row("inc", "(I)I", 3, 1, b'I');
    let filter = row("twice", "(I)I", 3, 1, b'I');
    let rows = [(0, (target, 1, b'I')), (4, (filter, 1, b'I'))];
    let g =
        build(&STATIC_SITE, &[IrType::Int], &rows, 4, site(None, false, 0)).expect("builds");
    let pre = call_of(&g, target).expect("the target's call");
    let main = call_of(&g, filter).expect("the filter's call");
    assert_eq!(g.nodes[pre as usize].bytecode_pc, Some(0));
    assert_eq!(
        g.nodes[g.nodes[pre as usize].inputs[2] as usize].op,
        Op::Param(0)
    );
    assert_eq!(g.nodes[main as usize].inputs[2], pre, "the target's result");
    assert_eq!(g.mh_constant_call_pcs, vec![4]);
}

#[test]
fn jr21_a_filter_site_that_does_not_fit_refuses_the_method() {
    let filter = row("neg", "(I)I", 3, 1, b'I');
    let target = row("twice", "(I)I", 3, 1, b'I');
    let rows = [(0, (filter, 1, b'I')), (4, (target, 1, b'I'))];
    // The PRE row is not the getstatic's.
    let elsewhere = IrMhFilterSite {
        pre_pc: 3,
        ..site(Some(0), false, 0)
    };
    assert!(build(&STATIC_SITE, &[IrType::Int], &rows, 4, elsewhere).is_none());
    // No PRE row.
    assert!(build(
        &STATIC_SITE,
        &[IrType::Int],
        &rows[1..],
        4,
        site(Some(0), false, 0)
    )
    .is_none());
    // A static MAIN row at a site planned as a virtual one.
    assert!(build(
        &STATIC_SITE,
        &[IrType::Int],
        &rows,
        4,
        site(Some(0), true, 4)
    )
    .is_none());
    // An argument past the site's.
    assert!(build(
        &STATIC_SITE,
        &[IrType::Int],
        &rows,
        4,
        site(Some(1), false, 0)
    )
    .is_none());
    // A PRE row of the wrong kind (a virtual filter).
    let virtual_filter = row("neg", "(I)I", 0, 1, b'I');
    assert!(build(
        &STATIC_SITE,
        &[IrType::Int],
        &[(0, (virtual_filter, 1, b'I')), (4, (target, 1, b'I'))],
        4,
        site(Some(0), false, 0)
    )
    .is_none());
}

/// A `static final` handle the static-final fold builds as a constant root
/// (`ConstFoldTables::static_final_const_refs`) is still that field's handle:
/// its plain and filter sites build as calls instead of refusing the method.
#[test]
fn a_folded_static_final_handle_still_builds_its_sites() {
    let fold = |b: &mut IrBuilder| {
        let mut folds = ConstFoldTables::default();
        folds.static_final_const_refs.insert((0x5171, 3), (0x9000, 1));
        b.set_const_folds(folds);
    };
    let target = row("twice", "(I)I", 3, 1, b'I');
    let mut b = IrBuilder::new(1, 1);
    b.set_param_types(&[IrType::Int]);
    b.set_static_field_info(HashMap::from([(0usize, (0x5171u32, 3usize, b'L', false))]));
    b.set_invoke_info(HashMap::from([(4, (target, 1, b'I'))]));
    b.set_mh_constant_sites(HashMap::from([(4, 0usize)]));
    fold(&mut b);
    let g = b.build(&STATIC_SITE, STATIC_SITE.len()).expect("plain site builds");
    assert!(call_of(&g, target).is_some(), "the member's call");
    assert_eq!(g.mh_constant_call_pcs, vec![4]);

    let filter = row("neg", "(I)I", 3, 1, b'I');
    let target = row("twice", "(I)I", 3, 1, b'I');
    let mut b = IrBuilder::new(1, 1);
    b.set_param_types(&[IrType::Int]);
    b.set_static_field_info(HashMap::from([(0usize, (0x5171u32, 3usize, b'L', false))]));
    b.set_invoke_info(HashMap::from([(0, (filter, 1, b'I')), (4, (target, 1, b'I'))]));
    b.set_mh_constant_sites(HashMap::from([(4, 0usize)]));
    b.set_mh_constant_filter_sites(HashMap::from([(4, site(Some(0), false, 0))]));
    fold(&mut b);
    let g = b.build(&STATIC_SITE, STATIC_SITE.len()).expect("filter site builds");
    assert!(call_of(&g, filter).is_some() && call_of(&g, target).is_some());
}
