use super::*;

#[test]
fn jr21_g1vhsoft_layout_words_decode_and_refuse_nothing_admitted() {
    let w: [u64; crate::direct_helpers::REFERENCE_GET_LAYOUT_QUERY_WORDS] =
        [11, 12, 0, 17, 25, 3, 33, 1, 0];
    let l = ReferenceGetIrLayout::from_words(&w).expect("decodes");
    assert_eq!(
        l,
        ReferenceGetIrLayout {
            weak_class: 11,
            soft_class: 12,
            referent_index: 0,
            weak_referent_row: 17,
            soft_referent_row: 25,
            timestamp_index: 3,
            timestamp_row: 33,
            clock_index: 1,
            clock_volatile: false,
        }
    );
    // Neither class admitted: no layout.
    let mut none = w;
    none[0] = 0;
    none[1] = 0;
    assert_eq!(ReferenceGetIrLayout::from_words(&none), None);
    // A word past u32: refused.
    let mut wide = w;
    wide[5] = u64::from(u32::MAX) + 1;
    assert_eq!(ReferenceGetIrLayout::from_words(&wide), None);
    // The volatile flag is exactly 1.
    let mut vol = w;
    vol[8] = 1;
    assert!(ReferenceGetIrLayout::from_words(&vol).is_some_and(|l| l.clock_volatile));
}

/// The query size is distinct from every other size the shared producer
/// answers (the referent-only 1, the one-class 2, the full layout's 18+).
#[test]
fn jr21_g1vhsoft_query_size_is_distinct() {
    use crate::direct_helpers as dh;
    let n = dh::REFERENCE_GET_LAYOUT_QUERY_WORDS;
    assert_ne!(n, dh::REFERENCE_REFERENT_QUERY_WORDS);
    assert_ne!(n, dh::THREAD_LOCAL_RECEIVER_QUERY_WORDS);
    assert!(n < dh::THREAD_LOCAL_IR_LAYOUT_WORDS);
}
#[test]
fn r26_reflection_get_caller_class_fold_tests() {
    let info: &'static JitInvokeInfo = Box::leak(Box::new(JitInvokeInfo {
        class_name: "jdk/internal/reflect/Reflection",
        method_name: "getCallerClass",
        descriptor: "()Ljava/lang/Class;",
        num_jit_args: 0,
        return_type: b'L',
        invoke_kind: 3,
        declaring_class_id: 0,
        owner_class_id: 0,
    }));
    let info_ptr = info as *const JitInvokeInfo as usize;
    assert!(is_reflection_get_caller_class_info_row(info_ptr));
    assert!(!is_reflection_get_caller_class_info_row(0));

    let make_frame = |caller_mirror_slot: usize, security_walk_ignored: bool| SpliceFrame {
        class_id: 0,
        return_pc: 0,
        end: 0,
        saved_locals: Vec::new(),
        saved_stack: Vec::new(),
        returns_value: true,
        return_narrow: None,
        multi_return: false,
        bimorphic_reopen: None,
        exits: Vec::new(),
        exit_bci: 0,
        ranges: Vec::new(),
        next_range: 1,
        range_end: 0,
        ctor_receiver: None,
        receiver: 0,
        arg0: NO_NODE,
        param_nodes: Vec::new(),
        receiver_exact_class: 0,
        invoke_pc: 0,
        base: 0,
        method_key: String::new(),
        code_len: 0,
        max_locals: 0,
        sync_monitor: None,
        miss_call: None,
        frameless: true,
        opaque: false,
        reexec_args: Vec::new(),
        caller_mirror_slot,
        security_walk_ignored,
    };

    // 1. Empty splice stack: refuses fold.
    let mut b = IrBuilder::new(1, 1);
    b.set_root_caller_frame(0x1000, false);
    assert!(!b.try_fold_reflection_get_caller_class(info_ptr));

    // 2. Top frame is @CallerSensitive callee; previous non-ignored splice frame wins.
    b.splice.push(make_frame(0x2000, false));
    b.splice.push(make_frame(0x3000, true)); // e.g. Method.invoke
    b.splice.push(make_frame(0x4000, false)); // @CallerSensitive callee
    assert!(b.try_fold_reflection_get_caller_class(info_ptr));
    let top = b.stack.pop().expect("pushed IndyRootRef");
    assert!(matches!(b.graph.nodes[top as usize].op, Op::IndyRootRef { slot_addr: 0x2000 }));

    // 3. All enclosing splice frames ignored: falls back to root caller frame.
    b.splice.clear();
    b.splice.push(make_frame(0x3000, true));
    b.splice.push(make_frame(0x4000, false));
    assert!(b.try_fold_reflection_get_caller_class(info_ptr));
    let top = b.stack.pop().expect("pushed root IndyRootRef");
    assert!(matches!(b.graph.nodes[top as usize].op, Op::IndyRootRef { slot_addr: 0x1000 }));

    // 4. Root also ignored or slot_addr == 0: refuses fold.
    b.set_root_caller_frame(0x1000, true);
    assert!(!b.try_fold_reflection_get_caller_class(info_ptr));
    b.set_root_caller_frame(0, false);
    assert!(!b.try_fold_reflection_get_caller_class(info_ptr));
}


#[test]
fn r26_const_fold_ifnonnull_indy_root_ref_and_tableswitch_dead_targets() {
    let mut code = vec![0u8; 52];
    code[0] = 0xb2; code[1] = 0; code[2] = 1; // getstatic #1 -> IndyRootRef
    code[3] = 0xc7; code[4] = 0; code[5] = 7; // ifnonnull -> 10 (pc 6 is dead)
    code[6] = 0x10; code[7] = 99; code[8] = 0xac; // bipush 99; ireturn (dead)
    code[10] = 0x04; // iconst_1
    code[11] = 0xaa; // tableswitch (pad=0)
    code[12..16].copy_from_slice(&34i32.to_be_bytes()); // default = 11 + 34 = 45
    code[16..20].copy_from_slice(&0i32.to_be_bytes());  // low = 0
    code[20..24].copy_from_slice(&1i32.to_be_bytes());  // high = 1
    code[24..28].copy_from_slice(&21i32.to_be_bytes()); // arm0 = 11 + 21 = 32
    code[28..32].copy_from_slice(&27i32.to_be_bytes()); // arm1 = 11 + 27 = 38
    code[32] = 0x10; code[33] = 10;
    code[34] = 0xa7; code[35] = 0; code[36] = 14; // goto 48
    code[38] = 0x10; code[39] = 20;
    code[40] = 0xa7; code[41] = 0; code[42] = 8;  // goto 48
    code[45] = 0x10; code[46] = 30;
    code[48] = 0xac; // ireturn

    let mut b = IrBuilder::new(0, 2);
    let mut folds = ConstFoldTables::default();
    folds.static_final_const_refs.insert((1, 0x1234), (0x1234, 42));
    b.set_const_folds(folds);
    b.set_static_field_info(HashMap::from([(0, (1, 0x1234, b'L', false))]));
    let g = b.build(&code, code.len()).expect("build failed");
    let has_20 = g.nodes.iter().any(|n| matches!(n.op, Op::Const(20)));
    let has_10 = g.nodes.iter().any(|n| matches!(n.op, Op::Const(10)));
    let has_30 = g.nodes.iter().any(|n| matches!(n.op, Op::Const(30)));
    let has_99 = g.nodes.iter().any(|n| matches!(n.op, Op::Const(99)));
    assert!(has_20 && !has_10 && !has_30 && !has_99);
}

#[test]
fn r26_const_mh_fold_converting_emits_unbox_call_and_box_in_splice_handler() {
    let target_info: &'static JitInvokeInfo = Box::leak(Box::new(JitInvokeInfo {
        class_name: "TestTarget",
        method_name: "add1",
        descriptor: "(I)I",
        num_jit_args: 2,
        return_type: b'I',
        invoke_kind: 0,
        declaring_class_id: 99,
        owner_class_id: 99,
    }));
    let box_info: &'static JitInvokeInfo = Box::leak(Box::new(JitInvokeInfo {
        class_name: "java/lang/Integer",
        method_name: "valueOf",
        descriptor: "(I)Ljava/lang/Integer;",
        num_jit_args: 1,
        return_type: b'L',
        invoke_kind: 3,
        declaring_class_id: 15,
        owner_class_id: 15,
    }));
    let mh_info: &'static JitInvokeInfo = Box::leak(Box::new(JitInvokeInfo {
        class_name: "java/lang/invoke/MethodHandle",
        method_name: "invokeExact",
        descriptor: "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
        num_jit_args: 3,
        return_type: b'L',
        invoke_kind: 0,
        declaring_class_id: 50,
        owner_class_id: 50,
    }));

    // 0: getstatic #1 (IndyRootRef MH @ 0x4000), 3: aload_0 (recv), 4: aload_1 (arg0: Integer), 5: invokevirtual #1, 8: areturn
    let code = vec![0xb2, 0x00, 0x01, 0x2a, 0x2b, 0xb6, 0x00, 0x01, 0xb0];
    let mut b = IrBuilder::new(2, 3);
    let mut folds = ConstFoldTables::default();
    folds.static_final_const_refs.insert((1, 0x4000), (0x4000, 50));
    b.set_static_field_info(HashMap::from([(0, (1, 0x4000, b'L', false))]));
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
    conv.cast_class_ids[0] = 10;
    conv.cast_class_names[0] = "java/lang/String".to_string();
    conv.unbox_op[1] = 1; // Integer -> IntValue
    conv.unbox_class_ids[1] = 15;
    folds.const_mh_invoke_infos.insert(
        (0x4000, "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;".to_string()),
        ConstMhFold {
            info_ptr: target_info as *const JitInvokeInfo as usize,
            ret_type: b'I',
            convert: Some(conv),
            box_info_ptr: box_info as *const JitInvokeInfo as usize,
        },
    );
    b.set_const_folds(folds);
    let mut invoke_infos: HashMap<usize, (usize, usize, u8)> = HashMap::new();
    invoke_infos.insert(5, (mh_info as *const JitInvokeInfo as usize, 3, b'L'));
    b.set_invoke_info(invoke_infos);
    let g = b.build(&code, code.len()).expect("build failed");
    assert!(g.nodes.iter().any(|n| matches!(n.op, Op::InstanceOf { .. })));
    assert!(g.nodes.iter().any(|n| matches!(n.op, Op::Unbox { op: UnboxOp::IntValue, .. })));
    let call_count = g.nodes.iter().filter(|n| matches!(n.op, Op::Call { .. })).count();
    assert_eq!(call_count, 2, "expected target Call + box Call");
}

/// A converting handle whose member is an INTERFACE method (`findVirtual` on
/// an interface, then `asType`) folds like a virtual one: the receiver is
/// guarded as an instance of the interface (a `null` fails the guard too),
/// and the member is called as an `invokeinterface` row.
#[test]
fn a_const_mh_fold_of_an_interface_member_guards_the_receiver_and_calls_it() {
    let target_info: &'static JitInvokeInfo = Box::leak(Box::new(JitInvokeInfo {
        class_name: "p/Service",
        method_name: "price",
        descriptor: "(I)I",
        num_jit_args: 2,
        return_type: b'I',
        invoke_kind: 2,
        declaring_class_id: 99,
        owner_class_id: 0,
    }));
    let box_info: &'static JitInvokeInfo = Box::leak(Box::new(JitInvokeInfo {
        class_name: "java/lang/Integer",
        method_name: "valueOf",
        descriptor: "(I)Ljava/lang/Integer;",
        num_jit_args: 1,
        return_type: b'L',
        invoke_kind: 3,
        declaring_class_id: 15,
        owner_class_id: 15,
    }));
    let mh_info: &'static JitInvokeInfo = Box::leak(Box::new(JitInvokeInfo {
        class_name: "java/lang/invoke/MethodHandle",
        method_name: "invokeExact",
        descriptor: "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
        num_jit_args: 3,
        return_type: b'L',
        invoke_kind: 0,
        declaring_class_id: 50,
        owner_class_id: 50,
    }));
    // 0: getstatic #1 (the folded handle), 3: aload_0, 4: aload_1, 5: invokevirtual, 8: areturn
    let code = vec![0xb2, 0x00, 0x01, 0x2a, 0x2b, 0xb6, 0x00, 0x01, 0xb0];
    let mut b = IrBuilder::new(2, 3);
    let mut folds = ConstFoldTables::default();
    folds.static_final_const_refs.insert((1, 0x4000), (0x4000, 50));
    b.set_static_field_info(HashMap::from([(0, (1, 0x4000, b'L', false))]));
    let mut conv = crate::compile_request::MhConstantConvertRow {
        main_kind: 2,
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
        (0x4000, "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;".to_string()),
        ConstMhFold {
            info_ptr: target_info as *const JitInvokeInfo as usize,
            ret_type: b'I',
            convert: Some(conv),
            box_info_ptr: box_info as *const JitInvokeInfo as usize,
        },
    );
    b.set_const_folds(folds);
    b.set_invoke_info(HashMap::from([(5, (mh_info as *const JitInvokeInfo as usize, 3, b'L'))]));
    let g = b.build(&code, code.len()).expect("build failed");
    let target = target_info as *const JitInvokeInfo as usize;
    assert!(
        g.nodes.iter().any(|n| n.op == Op::Call { info_ptr: target }),
        "the interface member is called"
    );
    let guard = g.nodes.iter().position(|n| matches!(n.op, Op::InstanceOf { .. })).expect("receiver guard");
    assert_eq!(g.nodes[guard].inputs[2], g.nodes.iter().position(|n| n.op == Op::Param(0)).unwrap() as NodeId);
    assert!(!g.nodes.iter().any(|n| n.op == Op::Call { info_ptr: mh_info as *const JitInvokeInfo as usize }));
}

/// `C.isAssignableFrom(o.getClass())` with `C` a constant mirror of a known
/// class is built as `o instanceof C` (`DirectMethodHandleAccessor.checkReceiver`
/// on an interface method): no `isAssignableFrom` call is left.
#[test]
fn a_constant_mirror_is_assignable_from_get_class_is_instanceof() {
    let info = |m: &'static str, d: &'static str, cls: &'static str, n: usize, r: u8| -> usize {
        let i: &'static JitInvokeInfo = Box::leak(Box::new(JitInvokeInfo {
            class_name: cls,
            method_name: m,
            descriptor: d,
            num_jit_args: n,
            return_type: r,
            invoke_kind: 0,
            declaring_class_id: 1,
            owner_class_id: 0,
        }));
        i as *const JitInvokeInfo as usize
    };
    let get_class = info("getClass", "()Ljava/lang/Class;", "java/lang/Object", 1, b'L');
    let assignable = info("isAssignableFrom", "(Ljava/lang/Class;)Z", "java/lang/Class", 2, b'Z');
    // 0: getstatic #1 (the folded mirror)  3: aload_0  4: invokevirtual getClass
    // 7: invokevirtual isAssignableFrom  10: ireturn
    let code = vec![0xb2, 0, 1, 0x2a, 0xb6, 0, 2, 0xb6, 0, 3, 0xac];
    let mut b = IrBuilder::new(1, 1);
    let mut folds = ConstFoldTables::default();
    folds.static_final_const_refs.insert((1, 0x10), (0x5000, 12));
    folds.const_mirror_classes.insert(0x5000, (77, "p/Service".to_string()));
    b.set_const_folds(folds);
    b.set_static_field_info(HashMap::from([(0, (1, 0x10, b'L', false))]));
    b.set_invoke_info(HashMap::from([(4, (get_class, 1, b'L')), (7, (assignable, 2, b'Z'))]));
    let g = b.build(&code, code.len()).expect("build failed");
    assert!(!g.nodes.iter().any(|n| n.op == Op::Call { info_ptr: assignable }));
    let inst = g.nodes.iter().find(|n| matches!(n.op, Op::InstanceOf { .. })).expect("instanceof");
    assert_eq!(g.nodes[inst.inputs[2] as usize].op, Op::Param(0));
}

/// `((Integer) Integer.valueOf(x)).intValue()` spliced as `checkcast` and
/// `getfield value` builds as `x`: the box's class is exactly `Integer`, and
/// its final `value` holds `x`. No cast and no load are left on the box.
#[test]
fn a_box_calls_cast_and_payload_read_fold_to_its_value() {
    let value_of: &'static JitInvokeInfo = Box::leak(Box::new(JitInvokeInfo {
        class_name: "java/lang/Integer",
        method_name: "valueOf",
        descriptor: "(I)Ljava/lang/Integer;",
        num_jit_args: 1,
        return_type: b'L',
        invoke_kind: 3,
        declaring_class_id: 15,
        owner_class_id: 0,
    }));
    // 0: iload_0  1: invokestatic valueOf  4: checkcast Integer  7: getfield value  10: ireturn
    let code = vec![0x1a, 0xb8, 0, 1, 0xc0, 0, 2, 0xb4, 0, 3, 0xac];
    let mut b = IrBuilder::new(1, 1);
    b.set_param_types(&[IrType::Int]);
    b.set_invoke_info(HashMap::from([(1, (value_of as *const JitInvokeInfo as usize, 1, b'L'))]));
    let (np, nl) = crate::intern_typecheck_target("java/lang/Integer", Some(15));
    b.set_checkcast_info(HashMap::from([(4, (np as usize, nl))]));
    b.set_field_info(HashMap::from([(7, (0usize, b'I'))]));
    b.set_field_decl_classes(HashMap::from([(7, 15u32)]));
    let g = b.build(&code, code.len()).expect("build failed");
    assert!(!g.nodes.iter().any(|n| matches!(n.op, Op::CheckCast { .. })), "no cast");
    assert!(!g.nodes.iter().any(|n| matches!(n.op, Op::Load(_))), "no payload load");
    let ret = g.nodes.iter().find(|n| n.op == Op::Return).expect("return");
    assert!(ret.inputs.iter().any(|&i| g.nodes[i as usize].op == Op::Param(0)), "returns x");
}
