use super::*;

const PROXY: u32 = 0x8000_0031;

fn static_row(method_name: &'static str, descriptor: &'static str) -> usize {
    let info: &'static crate::JitInvokeInfo = Box::leak(Box::new(crate::JitInvokeInfo {
        class_name: "p/Host",
        method_name,
        descriptor,
        num_jit_args: 1,
        return_type: b'I',
        invoke_kind: 3,
        declaring_class_id: 0x7131,
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

fn build_capture(need_splice: bool) -> Option<Graph> {
    let row = static_row("lambda$k$0", "(I)I");
    let mut builder = IrBuilder::new(1, 2);
    builder.set_param_types(&[IrType::Int]);
    builder.set_indy_trap_sites(HashMap::from([(1usize, (1usize, b'L'))]));
    builder.set_indy_capture_news(HashMap::from([(
        1usize,
        IndyCaptureNew {
            proxy_class_id: PROXY,
            capture_tags: vec![b'I'],
        },
    )]));
    builder.set_invoke_info(HashMap::from([(8usize, (row, 1usize, b'I'))]));
    builder.set_lambda_impl_sites(HashMap::from([(8usize, (1usize, 1usize, 1usize))]));
    builder.set_lambda_impl_captures_need_splice(need_splice);
    builder.build(&CAPTURE, 14)
}

#[test]
fn an_unspliced_capturing_impl_call_refuses_the_build() {
    assert!(build_capture(true).is_none());
}

#[test]
fn with_the_switch_off_the_unspliced_call_is_built_as_before() {
    let g = build_capture(false).expect("builds");
    assert_eq!(
        g.nodes
            .iter()
            .filter(|n| matches!(n.op, Op::Call { .. }))
            .count(),
        1
    );
}

// static int f(int x): IntUnaryOperator f = () -> ..; return f.applyAsInt(x);
//  0: invokedynamic #1 ()L  5: astore_1  6: aload_1  7: iload_0
//  8: invokeinterface #2 applyAsInt(I)I, 2  13: ireturn
const WORD: [u8; 14] = [
    0xba, 0x00, 0x01, 0x00, 0x00, 0x4c, 0x2b, 0x1a, 0xb9, 0x00, 0x02, 0x02, 0x00, 0xac,
];

#[test]
fn a_non_capturing_site_needs_no_splice() {
    let row = static_row("lambda$f$1", "(I)I");
    let mut builder = IrBuilder::new(1, 2);
    builder.set_param_types(&[IrType::Int]);
    builder.set_indy_trap_sites(HashMap::from([(0usize, (0usize, b'L'))]));
    builder.add_indy_bridge_sites([(0usize, 0x5170usize)]);
    builder.set_invoke_info(HashMap::from([(8usize, (row, 1usize, b'I'))]));
    builder.set_lambda_impl_sites(HashMap::from([(8usize, (0usize, 0usize, 1usize))]));
    builder.set_lambda_impl_captures_need_splice(true);
    let g = builder.build(&WORD, 14).expect("builds");
    assert_eq!(
        g.nodes
            .iter()
            .filter(|n| matches!(n.op, Op::Call { .. }))
            .count(),
        1
    );
}
