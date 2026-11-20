use super::*;
use crate::deopt::DeoptReason;
use crate::ir::IrBuilder;

/// `int f(int n) { if (n <= 0) return 0; return 1 + R.f(n - 1); }` with
/// `R` = `this` (`aload_0`) or `null` (`aconst_null`), the invokevirtual
/// at pc 11 bound to this method.
fn code(receiver_op: u8) -> [u8; 18] {
    [
        0x1b, // 0:  iload_1
        0x9d, 0x00, 0x05, // 1:  ifgt +5 -> 6
        0x03, // 4:  iconst_0
        0xac, // 5:  ireturn
        0x04, // 6:  iconst_1
        receiver_op, // 7:  aload_0 | aconst_null
        0x1b, // 8:  iload_1
        0x04, // 9:  iconst_1
        0x64, // 10: isub
        0xb6, 0x00, 0x02, // 11: invokevirtual #2 (this method)
        0x60, // 14: iadd
        0xac, // 15: ireturn
        0, 0,
    ]
}

extern "C" fn guard_ok(_vm: i64, _ctor_receiver: i64) -> i64 {
    0
}

extern "C" fn service(_vm: i64, _info: i64, _args: i64, _n: i64) -> i64 {
    0
}

/// The IR's kind-4 row and the VM's kind-1 twin, leaked.
fn rows() -> (usize, usize) {
    let ir_row: &'static JitInvokeInfo = Box::leak(Box::new(JitInvokeInfo {
        class_name: "pkg/Inst",
        method_name: "f",
        descriptor: "(I)I",
        num_jit_args: 2, // the receiver and `n`
        return_type: b'I',
        invoke_kind: 4,
        declaring_class_id: 0,
        owner_class_id: 0,
    }));
    let twin: &'static JitInvokeInfo = Box::leak(Box::new(JitInvokeInfo {
        invoke_kind: 1,
        ..*ir_row
    }));
    (
        ir_row as *const JitInvokeInfo as usize,
        twin as *const JitInvokeInfo as usize,
    )
}

fn graph(receiver_op: u8, with_twin: bool) -> (Graph, usize, usize) {
    let (ir_row, twin) = rows();
    let mut b = IrBuilder::new(2, 2);
    b.set_param_types(&[IrType::Ref, IrType::Int]);
    b.graph.receiver_param = Some(0);
    let mut invoke_info = HashMap::new();
    invoke_info.insert(11usize, (ir_row, 2usize, b'I'));
    b.set_invoke_info(invoke_info);
    let mut g = b.build(&code(receiver_op), 16).expect("IR build");
    if with_twin {
        g.self_call_service_infos = vec![(ir_row, twin)];
    }
    (g, ir_row, twin)
}

fn lower_graph(g: &Graph) -> Option<CompiledMethod> {
    let schedule = crate::ir_schedule::schedule(g);
    // SAFETY: an all-zero helper table is the "nothing wired" table every
    // lowering test starts from; the two this route needs are set below.
    let mut helpers: JitRuntimeHelpers = unsafe { std::mem::zeroed() };
    helpers.self_call_stack_guard = guard_ok as *const () as usize;
    helpers.service_callee_deopt = service as *const () as usize;
    let empty_hints: HashMap<usize, bool> = HashMap::new();
    let no_direct: HashMap<usize, (usize, bool)> = HashMap::new();
    let no_ic: HashMap<usize, (usize, usize)> = HashMap::new();
    let no_compact: HashMap<(usize, bool), (u32, bool, u8)> = HashMap::new();
    lower_inner(
        g,
        &schedule,
        2,
        2,
        &helpers,
        &empty_hints,
        &[],
        None,
        &no_direct,
        &no_ic,
        &no_compact,
        &crate::ir::IrInlineFrameSites::default(),
    )
}

fn the_call(g: &Graph) -> &Node {
    let calls: Vec<&Node> = g
        .nodes
        .iter()
        .filter(|n| Lowerer::is_direct_self_call(n))
        .collect();
    assert_eq!(calls.len(), 1, "fixture: one direct self-call");
    calls[0]
}

fn null_checks(cm: &CompiledMethod) -> usize {
    cm._deopt_point_boxes
        .iter()
        .filter(|p| p.reason == DeoptReason::NullCheck)
        .count()
}

fn bakes(cm: &CompiledMethod, word: usize) -> bool {
    let needle = (word as u64).to_le_bytes();
    cm.code_bytes().windows(8).any(|w| w == needle)
}

/// The receiver is the call's argument 0, and a static self-call row
/// (fib's `(I)I` with one argument) has none.
#[test]
fn the_receiver_is_argument_0_of_an_instance_row_only() {
    let (g, _, _) = graph(0x2a, true);
    let call = the_call(&g);
    let recv = Lowerer::direct_self_call_receiver(call).expect("a receiver");
    assert!(matches!(g.nodes[recv as usize].op, Op::Param(0)));
    let fib: &'static JitInvokeInfo = Box::leak(Box::new(JitInvokeInfo {
        num_jit_args: 1,
        // SAFETY: rows returns the leaked, initialized JitInvokeInfo retained by this fixture.
        ..*unsafe { &*(rows().0 as *const JitInvokeInfo) }
    }));
    assert!(!crate::self_call_info_has_receiver(fib));
}

/// `this.f(..)`: no null check (`this` is never null), and the VM is
/// handed the kind-1 twin, never the IR's kind-4 row.
#[test]
fn a_self_call_on_this_plants_no_null_check_and_bakes_the_twin() {
    let (g, ir_row, twin) = graph(0x2a, true);
    let cm = lower_graph(&g).expect("the instance self-call lowers");
    assert_eq!(null_checks(&cm), 0);
    assert!(bakes(&cm, twin), "the service is handed the kind-1 twin");
    assert!(!bakes(&cm, ir_row), "the kind-4 row never reaches the VM");
}

/// Any other receiver is null-checked before the call (JVMS 6.5), and its
/// bci keeps its frame state for that check's deopt.
#[test]
fn a_self_call_on_another_receiver_null_checks_it() {
    let (g, _, twin) = graph(0x01, true);
    {
        let call = the_call(&g);
        let recv = Lowerer::direct_self_call_receiver(call).expect("a receiver");
        assert!(!matches!(g.nodes[recv as usize].op, Op::Param(0)));
    }
    let cm = lower_graph(&g).expect("the instance self-call lowers");
    assert_eq!(null_checks(&cm), 1, "one NullCheck deopt, at the invoke");
    assert!(bakes(&cm, twin));
}

/// A receiver-carrying self-call no planner gave a VM row is refused
/// rather than lowered with the kind-4 row, which the VM would decode as
/// a receiver-less static call.
#[test]
fn a_receiver_carrying_self_call_without_its_twin_is_refused() {
    let (g, _, _) = graph(0x2a, false);
    assert!(lower_graph(&g).is_none());
}
