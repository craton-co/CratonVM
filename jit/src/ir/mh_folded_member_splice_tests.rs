//! A folded constant handle's member, planned at the `invokeExact` itself
//! (the VM resolver's `folded_handle_member_row`), is spliced through the
//! fold when its body cannot raise and the receiver is proven its class; the
//! invoke arms never splice it as the call.

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

/// `getstatic MHV; getstatic RECV; aload_1; invokevirtual invokeExact; areturn`:
/// MHV's handle folds (guessed slot 0x4000) to `p/Service.price(I)I` behind
/// a converting row; RECV is a trusted `static final` of exact class 50, and
/// `p/Impl.price`'s body `member` is planned at the `invokeExact` (pc 7).
fn build(member: &[u8], fold: bool) -> (Option<Graph>, usize, usize, usize) {
    let target = leak("p/Service", "price", "(I)I", 2, b'I', 0);
    let boxer = leak("java/lang/Integer", "valueOf", "(I)Ljava/lang/Integer;", 1, b'L', 3);
    let mh = leak("java/lang/invoke/MethodHandle", "invokeExact", SITE, 3, b'L', 0);
    let caller = [0xb2, 0x00, 0x01, 0xb2, 0x00, 0x02, 0x2b, 0xb6, 0x00, 0x03, 0xb0];
    let base = caller.len();
    let mut combined = caller.to_vec();
    combined.extend_from_slice(member);
    combined.extend_from_slice(&[0, 0]);
    let mut b = IrBuilder::new(2, 2);
    b.set_param_types(&[IrType::Ref, IrType::Ref]);
    let mut folds = ConstFoldTables::default();
    folds.static_guess_const_refs.insert((1, 7), (0x4000, 60));
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
    if fold {
        folds.const_mh_invoke_infos.insert(
            (0x4000, SITE.to_string()),
            ConstMhFold { info_ptr: target, ret_type: b'I', convert: Some(conv), box_info_ptr: boxer },
        );
    }
    b.set_const_folds(folds);
    b.set_static_guess_despec(
        Some(std::sync::Arc::new(crate::deopt::DespecRegistry::new())),
        "T.m:(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;".to_string(),
    );
    b.set_static_field_info(HashMap::from([(0usize, (1, 7, b'L', false)), (3usize, (2, 8, b'L', false))]));
    b.set_static_final_exact_classes(HashMap::from([((2, 8), 50)]));
    b.apply_inline_tables(IrInlineTables {
        sites: HashMap::from([(
            7usize,
            IrInlineSite {
                base,
                code_len: member.len(),
                num_args: 2,
                max_locals: 2,
                arg_local_slots: vec![0, 1],
                returns_value: true,
                receiver_is_arg0: true,
                method_key: "p/Impl.price:(I)I".to_string(),
                class_id: 50,
            },
        )]),
        invoke_info: HashMap::from([(7usize, (mh, 3usize, b'L'))]),
        ..IrInlineTables::default()
    });
    b.add_exact_receiver_site(7, 50);
    (b.build(&combined, caller.len()), target, boxer, mh)
}

fn call_of(g: &Graph, info_ptr: usize) -> Option<&Node> {
    g.nodes.iter().find(|n| n.op == Op::Call { info_ptr })
}

#[test]
fn a_folded_member_that_cannot_raise_is_spliced_and_its_result_boxed() {
    // iload_1  iconst_3  imul  ireturn
    let (g, target, boxer, mh) = build(&[0x1b, 0x06, 0x68, 0xac], true);
    let g = g.expect("builds");
    assert!(call_of(&g, target).is_none(), "the member is spliced");
    assert!(call_of(&g, mh).is_none(), "the handle call is folded");
    let boxed = call_of(&g, boxer).expect("the result is boxed");
    assert_eq!(g.nodes[boxed.inputs[2] as usize].op, Op::Mul, "the box takes the spliced value");
}

#[test]
fn a_folded_member_that_can_raise_keeps_its_call() {
    // iload_1  iconst_3  idiv  ireturn
    let (g, target, boxer, _) = build(&[0x1b, 0x06, 0x6c, 0xac], true);
    let g = g.expect("builds");
    assert!(call_of(&g, target).is_some(), "the member is called");
    assert!(call_of(&g, boxer).is_some());
    assert!(!g.nodes.iter().any(|n| n.op == Op::Div), "the body is not walked");
}

#[test]
fn without_the_fold_the_member_body_is_never_spliced_as_the_handle_call() {
    let (g, target, _, mh) = build(&[0x1b, 0x06, 0x68, 0xac], false);
    let g = g.expect("builds");
    assert!(call_of(&g, mh).is_some(), "the handle is called");
    assert!(call_of(&g, target).is_none());
    assert!(!g.nodes.iter().any(|n| n.op == Op::Mul), "the member body is not walked");
}
