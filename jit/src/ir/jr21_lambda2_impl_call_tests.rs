use super::*;

const PROXY: u32 = 0x8000_0021;

fn static_row(method_name: &'static str, descriptor: &'static str, kind: u8) -> usize {
    let info: &'static crate::JitInvokeInfo = Box::leak(Box::new(crate::JitInvokeInfo {
        class_name: "p/Host",
        method_name,
        descriptor,
        num_jit_args: 1,
        return_type: b'I',
        invoke_kind: kind,
        declaring_class_id: 0x7121,
        owner_class_id: 0,
    }));
    info as *const crate::JitInvokeInfo as usize
}

// static int k(int j): IntSupplier f = () -> j; return f.getAsInt();
//  0: iload_0  1: invokedynamic #1 (I)L  6: astore_1  7: aload_1
//  8: invokeinterface #2 getAsInt()I, 1  13: ireturn
const CAPTURE: [u8; 14] = [
    0x1a, 0xba, 0x00, 0x01, 0x00, 0x00, 0x4c, 0x2b, 0xb9, 0x00, 0x02, 0x01, 0x00, 0xac,
];

fn build_capture(row: usize, tags: Option<Vec<u8>>) -> Option<Graph> {
    let mut builder = IrBuilder::new(1, 2);
    builder.set_param_types(&[IrType::Int]);
    builder.set_indy_trap_sites(HashMap::from([(1usize, (1usize, b'L'))]));
    if let Some(capture_tags) = tags {
        builder.set_indy_capture_news(HashMap::from([(
            1usize,
            IndyCaptureNew {
                proxy_class_id: PROXY,
                capture_tags,
            },
        )]));
    }
    builder.set_invoke_info(HashMap::from([(8usize, (row, 1usize, b'I'))]));
    builder.set_lambda_impl_sites(HashMap::from([(8usize, (1usize, 1usize, 1usize))]));
    builder.build(&CAPTURE, 14)
}

fn calls(g: &Graph) -> Vec<&Node> {
    g.nodes
        .iter()
        .filter(|n| matches!(n.op, Op::Call { .. }))
        .collect()
}

#[test]
fn a_capturing_sam_call_is_the_impl_call_on_the_captures() {
    let row = static_row("lambda$k$0", "(I)I", 3);
    let g = build_capture(row, Some(vec![b'I'])).expect("builds");
    let calls = calls(&g);
    assert_eq!(calls.len(), 1, "one plain call");
    assert_eq!(calls[0].op, Op::Call { info_ptr: row });
    assert_eq!(calls[0].bytecode_pc, Some(8));
    assert_eq!(calls[0].inputs.len(), 3, "[ctrl, mem, the capture]");
    assert_eq!(
        g.nodes[calls[0].inputs[2] as usize].op,
        Op::Param(0),
        "the captured value itself, not a load from the proxy"
    );
    let new = g
        .nodes
        .iter()
        .position(|n| {
            n.op == Op::New {
                class_id: PROXY,
                num_fields: 1,
            }
        })
        .expect("the proxy allocation") as NodeId;
    assert!(
        !calls[0].inputs.contains(&new),
        "the proxy is no argument of the call"
    );
    let at_invoke = g
        .safepoints
        .iter()
        .find(|sp| sp.bci == 8)
        .expect("a snapshot at the invoke");
    assert_eq!(
        at_invoke.stack,
        vec![new],
        "the interpreter's operands: the proxy"
    );
    assert_eq!(g.mh_constant_call_pcs, vec![8], "no post-call exit");
    assert!(g.site_trap_pcs.is_empty());
}

#[test]
fn a_trapped_capturing_site_hands_the_dead_call_typed_zeros() {
    if !ir_site_trap_enabled() {
        return;
    }
    let row = static_row("lambda$k$0", "(I)I", 3);
    // A row whose tag disagrees with its operand: the `invokedynamic`
    // keeps its trap, so the planned call after it is dead code and takes
    // a zero of the row's type for the capture.
    let g = build_capture(row, Some(vec![b'L'])).expect("builds");
    assert_eq!(g.site_trap_pcs, vec![1]);
    let calls = calls(&g);
    assert_eq!(calls.len(), 1);
    let arg = &g.nodes[calls[0].inputs[2] as usize];
    assert_eq!((arg.op.clone(), arg.ty), (Op::Const(0), IrType::Ref));
    // With no row at all there is no tag list: the method is refused
    // rather than guessed.
    assert!(build_capture(row, None).is_none());
}

#[test]
fn a_row_that_is_not_static_refuses_the_method() {
    let row = static_row("getAsInt", "()I", 2);
    assert!(build_capture(row, Some(vec![b'I'])).is_none());
}

// static int f(int x): IntUnaryOperator f = () -> ..; return f.applyAsInt(x);
//  0: invokedynamic #1 ()L  5: astore_1  6: aload_1  7: iload_0
//  8: invokeinterface #2 applyAsInt(I)I, 2  13: ireturn
const WORD: [u8; 14] = [
    0xba, 0x00, 0x01, 0x00, 0x00, 0x4c, 0x2b, 0x1a, 0xb9, 0x00, 0x02, 0x02, 0x00, 0xac,
];

#[test]
fn a_published_singletons_sam_call_drops_the_receiver() {
    let row = static_row("lambda$f$1", "(I)I", 3);
    let mut builder = IrBuilder::new(1, 2);
    builder.set_param_types(&[IrType::Int]);
    builder.set_indy_trap_sites(HashMap::from([(0usize, (0usize, b'L'))]));
    builder.add_indy_bridge_sites([(0usize, 0x5170usize)]);
    builder.set_invoke_info(HashMap::from([(8usize, (row, 1usize, b'I'))]));
    builder.set_lambda_impl_sites(HashMap::from([(8usize, (0usize, 0usize, 1usize))]));
    let g = builder.build(&WORD, 14).expect("builds");
    let calls = calls(&g);
    assert_eq!(calls.len(), 1, "the impl call only");
    assert_eq!(calls[0].op, Op::Call { info_ptr: row });
    assert_eq!(calls[0].inputs.len(), 3, "[ctrl, mem, the SAM argument]");
    assert_eq!(g.nodes[calls[0].inputs[2] as usize].op, Op::Param(0));
    let at_invoke = g
        .safepoints
        .iter()
        .find(|sp| sp.bci == 8)
        .expect("a snapshot at the invoke");
    assert_eq!(at_invoke.stack.len(), 2, "the singleton and the argument");
    assert!(matches!(
        g.nodes[at_invoke.stack[0] as usize].op,
        Op::IndyBridge { site: 0x5170 }
    ));
}

#[test]
fn a_receiver_that_is_not_the_planned_lambda_refuses_the_method() {
    // static int g(IntSupplier s): return s.getAsInt();
    //  0: aload_0  1: invokeinterface #2 getAsInt()I, 1  6: ireturn
    let code = [0x2a, 0xb9, 0x00, 0x02, 0x01, 0x00, 0xac];
    let row = static_row("lambda$k$0", "(I)I", 3);
    let mut builder = IrBuilder::new(1, 1);
    builder.set_param_types(&[IrType::Ref]);
    builder.set_invoke_info(HashMap::from([(1usize, (row, 1usize, b'I'))]));
    builder.set_lambda_impl_sites(HashMap::from([(1usize, (9usize, 1usize, 1usize))]));
    assert!(builder.build(&code, 7).is_none());
}
