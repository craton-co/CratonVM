//! A `MethodHandle` read from a non-final static (`ConstFoldTables::static_guess_const_refs`)
//! folds like a `static final` one behind an identity guard on the handle.

use super::*;

const SITE: &str = "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;";

fn leak(class_name: &'static str, method_name: &'static str, descriptor: &'static str, n: usize, ret: u8, kind: u8) -> usize {
    let info: &'static JitInvokeInfo = Box::leak(Box::new(JitInvokeInfo {
        class_name,
        method_name,
        descriptor,
        num_jit_args: n,
        return_type: ret,
        invoke_kind: kind,
        declaring_class_id: 99,
        owner_class_id: 0,
    }));
    info as *const JitInvokeInfo as usize
}

/// `getstatic MHV; aload_0; aload_1; invokevirtual invokeExact; areturn`, with
/// MHV's current handle in slot 0x4000 and that slot's converting fold.
fn build(despec: Option<std::sync::Arc<crate::deopt::DespecRegistry>>) -> (Graph, usize, usize) {
    let target = leak("p/Service", "price", "(I)I", 2, b'I', 0);
    let boxer = leak("java/lang/Integer", "valueOf", "(I)Ljava/lang/Integer;", 1, b'L', 3);
    let mh = leak("java/lang/invoke/MethodHandle", "invokeExact", SITE, 3, b'L', 0);
    let code = vec![0xb2, 0x00, 0x01, 0x2a, 0x2b, 0xb6, 0x00, 0x01, 0xb0];
    let mut b = IrBuilder::new(2, 3);
    let mut folds = ConstFoldTables::default();
    folds.static_guess_const_refs.insert((1, 7), (0x4000, 50));
    b.set_static_field_info(HashMap::from([(0, (1, 7, b'L', false))]));
    let mut conv = crate::compile_request::MhConstantConvertRow {
        main_kind: 0,
        arity: 2,
        cast_class_ids: [0; 8],
        cast_class_names: Default::default(),
        unbox_op: [0; 8],
        unbox_class_ids: [0; 8],
        box_return: 1,
        box_class_id: 15,
    };
    conv.unbox_op[1] = 1;
    conv.unbox_class_ids[1] = 15;
    folds.const_mh_invoke_infos.insert(
        (0x4000, SITE.to_string()),
        ConstMhFold { info_ptr: target, ret_type: b'I', convert: Some(conv), box_info_ptr: boxer },
    );
    b.set_const_folds(folds);
    b.set_static_guess_despec(despec, "T.m:(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;".to_string());
    b.set_invoke_info(HashMap::from([(5, (mh, 3, b'L'))]));
    (b.build(&code, code.len()).expect("build failed"), target, mh)
}

#[test]
fn a_non_final_static_handle_folds_behind_an_identity_guard() {
    let (g, target, mh) = build(Some(std::sync::Arc::new(crate::deopt::DespecRegistry::new())));
    assert!(g.nodes.iter().any(|n| n.op == Op::Call { info_ptr: target }), "the member is called");
    assert!(!g.nodes.iter().any(|n| n.op == Op::Call { info_ptr: mh }), "no invokeExact call");
    let load = g.nodes.iter().position(|n| matches!(n.op, Op::LoadStatic { class_id: 1, field_index: 7, .. }));
    let root = g.nodes.iter().position(|n| n.op == Op::IndyRootRef { slot_addr: 0x4000 });
    let (load, root) = (load.expect("the static is still read") as NodeId, root.expect("the guessed handle") as NodeId);
    let eq = g
        .nodes
        .iter()
        .position(|n| n.op == Op::Cmp(CmpOp::Eq) && n.inputs.contains(&load) && n.inputs.contains(&root))
        .expect("read == guess") as NodeId;
    assert!(
        g.nodes.iter().any(|n| matches!(n.op, Op::Guard { bci: 5 }) && n.inputs.contains(&eq)),
        "guarded at the call"
    );
}

#[test]
fn a_despeculated_site_or_no_registry_keeps_the_handle_call() {
    let despec = std::sync::Arc::new(crate::deopt::DespecRegistry::new());
    despec.insert("T.m:(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;", 5, crate::deopt::SPECULATION_ID_ANY);
    for d in [Some(despec), None] {
        let (g, target, mh) = build(d);
        assert!(g.nodes.iter().any(|n| n.op == Op::Call { info_ptr: mh }));
        assert!(!g.nodes.iter().any(|n| n.op == Op::Call { info_ptr: target }));
        assert!(!g.nodes.iter().any(|n| matches!(n.op, Op::IndyRootRef { .. })));
    }
}

const HELPER: &str = "T.viaHelper:(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;";

/// `m(a, b) { return viaHelper(a, b); }` with `viaHelper`'s body (the shape
/// of [`build`]'s code) spliced at caller pc 2: the guess is read inside the
/// splice, and its trap is charged to that outermost `invoke`.
fn build_spliced(despec: Option<std::sync::Arc<crate::deopt::DespecRegistry>>) -> (Graph, usize, usize) {
    let target = leak("p/Service", "price", "(I)I", 2, b'I', 0);
    let boxer = leak("java/lang/Integer", "valueOf", "(I)Ljava/lang/Integer;", 1, b'L', 3);
    let mh = leak("java/lang/invoke/MethodHandle", "invokeExact", SITE, 3, b'L', 0);
    let helper = leak("T", "viaHelper", SITE, 2, b'L', 3);
    let caller = [0x2a, 0x2b, 0xb8, 0x00, 0x01, 0xb0];
    let callee = [0xb2, 0x00, 0x01, 0x2a, 0x2b, 0xb6, 0x00, 0x01, 0xb0];
    let base = caller.len();
    let mut combined = caller.to_vec();
    combined.extend_from_slice(&callee);
    combined.extend_from_slice(&[0, 0]);
    let mut b = IrBuilder::new(2, 2);
    b.set_param_types(&[IrType::Ref, IrType::Ref]);
    b.graph.scope_method_key = "T.m:(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;".to_string();
    let mut folds = ConstFoldTables::default();
    folds.static_guess_const_refs.insert((1, 7), (0x4000, 50));
    let mut conv = crate::compile_request::MhConstantConvertRow {
        main_kind: 0,
        arity: 2,
        cast_class_ids: [0; 8],
        cast_class_names: Default::default(),
        unbox_op: [0; 8],
        unbox_class_ids: [0; 8],
        box_return: 1,
        box_class_id: 15,
    };
    conv.unbox_op[1] = 1;
    conv.unbox_class_ids[1] = 15;
    folds.const_mh_invoke_infos.insert(
        (0x4000, SITE.to_string()),
        ConstMhFold { info_ptr: target, ret_type: b'I', convert: Some(conv), box_info_ptr: boxer },
    );
    b.set_const_folds(folds);
    b.set_static_guess_despec(despec, "T.m:(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;".to_string());
    b.set_invoke_info(HashMap::from([(2usize, (helper, 2usize, b'L'))]));
    b.apply_inline_tables(IrInlineTables {
        sites: HashMap::from([(
            2usize,
            IrInlineSite {
                base,
                code_len: callee.len(),
                num_args: 2,
                max_locals: 2,
                arg_local_slots: vec![0, 1],
                returns_value: true,
                receiver_is_arg0: false,
                method_key: HELPER.to_string(),
                class_id: 1,
            },
        )]),
        invoke_info: HashMap::from([(base + 5, (mh, 3usize, b'L'))]),
        static_field_info: HashMap::from([(base, (1, 7, b'L', false))]),
        ..IrInlineTables::default()
    });
    (b.build(&combined, caller.len()).expect("build failed"), target, mh)
}

fn chain_traps_on() -> bool {
    ir_splice_frame_states_enabled() && ir_splice_chain_fences_enabled() && ir_site_trap_enabled()
}

#[test]
fn a_guess_read_inside_a_spliced_body_folds_behind_its_identity_guard() {
    if !chain_traps_on() {
        return;
    }
    let (g, target, mh) = build_spliced(Some(std::sync::Arc::new(crate::deopt::DespecRegistry::new())));
    assert!(g.nodes.iter().any(|n| n.op == Op::Call { info_ptr: target }), "the member is called");
    assert!(!g.nodes.iter().any(|n| n.op == Op::Call { info_ptr: mh }), "no invokeExact call");
    let root = g.nodes.iter().position(|n| n.op == Op::IndyRootRef { slot_addr: 0x4000 });
    assert!(root.is_some(), "the guessed handle");
    assert!(g.splice_chains_required, "the guard resumes through the chain snapshot");
}

#[test]
fn a_spliced_guess_charged_to_its_outer_invoke_keeps_the_handle_call() {
    let despec = std::sync::Arc::new(crate::deopt::DespecRegistry::new());
    despec.insert("T.m:(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;", 2, crate::deopt::SPECULATION_ID_ANY);
    let (g, target, mh) = build_spliced(Some(despec));
    assert!(g.nodes.iter().any(|n| n.op == Op::Call { info_ptr: mh }));
    assert!(!g.nodes.iter().any(|n| n.op == Op::Call { info_ptr: target }));
    assert!(!g.nodes.iter().any(|n| matches!(n.op, Op::IndyRootRef { .. })));
}
