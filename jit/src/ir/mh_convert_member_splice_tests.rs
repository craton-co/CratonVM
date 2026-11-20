//! A box-returning converting constant handle (`asType` of a primitive
//! member) splices its member's body when that body cannot trap, then boxes
//! the result; a body that can trap keeps the member call.

use super::*;

fn row(class_name: &'static str, method_name: &'static str, descriptor: &'static str, kind: u8, n: usize, ret: u8) -> usize {
    let info: &'static crate::JitInvokeInfo = Box::leak(Box::new(crate::JitInvokeInfo {
        class_name,
        method_name,
        descriptor,
        num_jit_args: n,
        return_type: ret,
        invoke_kind: kind,
        declaring_class_id: 0x7101,
        owner_class_id: 0,
    }));
    info as *const crate::JitInvokeInfo as usize
}

// 0: getstatic #1 (the handle)  3: aload_0  4: invokevirtual #2
// invokeExact (Ljava/lang/Object;)Ljava/lang/Object;  7: areturn
const CALLER: [u8; 8] = [0xb2, 0x00, 0x01, 0x2a, 0xb6, 0x00, 0x02, 0xb0];

/// The site above with the static member `p/Host.f(I)I` (body `member`)
/// planned at the `getstatic` pc, as the planner appends it.
fn build(member: &[u8]) -> (Option<Graph>, usize, usize) {
    let target = row("p/Host", "f", "(I)I", 3, 1, b'I');
    let boxer = row("java/lang/Integer", "valueOf", "(I)Ljava/lang/Integer;", 3, 1, b'L');
    let base = CALLER.len();
    let mut combined = CALLER.to_vec();
    combined.extend_from_slice(member);
    combined.extend_from_slice(&[0, 0]);
    let mut b = IrBuilder::new(1, 1);
    b.set_param_types(&[IrType::Ref]);
    b.set_static_field_info(HashMap::from([(0usize, (0x5171u32, 3usize, b'L', false))]));
    b.set_mh_constant_sites(HashMap::from([(4usize, 0usize)]));
    let mut site = IrMhConvertSite {
        pre_pc: 0,
        main_kind: 3,
        arity: 1,
        cast_class_ids: [0; 8],
        cast_class_names: Default::default(),
        unbox_op: [0; 8],
        unbox_class_ids: [0; 8],
        box_return: 1,
    };
    site.unbox_op[0] = 1;
    site.unbox_class_ids[0] = 15;
    b.set_mh_constant_convert_sites(HashMap::from([(4usize, site)]));
    b.apply_inline_tables(IrInlineTables {
        sites: HashMap::from([(
            0usize,
            IrInlineSite {
                base,
                code_len: member.len(),
                num_args: 1,
                max_locals: 1,
                arg_local_slots: vec![0],
                returns_value: true,
                receiver_is_arg0: false,
                method_key: "p/Host.f:(I)I".to_string(),
                class_id: 0x7101,
            },
        )]),
        invoke_info: HashMap::from([(0usize, (target, 1usize, b'I')), (4usize, (boxer, 1usize, b'L'))]),
        ..IrInlineTables::default()
    });
    (b.build(&combined, CALLER.len()), target, boxer)
}

fn call_of(g: &Graph, info_ptr: usize) -> Option<&Node> {
    g.nodes.iter().find(|n| n.op == Op::Call { info_ptr })
}

#[test]
fn a_member_that_cannot_trap_is_spliced_and_its_result_boxed() {
    // iload_0  iconst_3  imul  ireturn
    let (g, target, boxer) = build(&[0x1a, 0x06, 0x68, 0xac]);
    let g = g.expect("builds");
    assert!(call_of(&g, target).is_none(), "the member is spliced, not called");
    let boxed = call_of(&g, boxer).expect("the result is boxed");
    let value = boxed.inputs[2];
    assert_eq!(g.nodes[value as usize].op, Op::Mul, "the box takes the spliced body's value");
    assert_eq!(boxed.bytecode_pc, Some(4), "boxed at the invokeExact");
    assert!(g.mh_constant_call_pcs.contains(&4), "no post-call exit at the invokeExact");
}

#[test]
fn a_member_that_can_trap_keeps_its_call() {
    // iload_0  iconst_3  idiv  ireturn: a division can raise
    let (g, target, boxer) = build(&[0x1a, 0x06, 0x6c, 0xac]);
    let g = g.expect("builds");
    let call = call_of(&g, target).expect("the member is called");
    let boxed = call_of(&g, boxer).expect("the result is boxed");
    assert!(boxed.inputs.contains(&(g.nodes.iter().position(|n| std::ptr::eq(n, call)).unwrap() as NodeId)));
    assert!(!g.nodes.iter().any(|n| n.op == Op::Div), "the body is not walked");
}
