use super::*;
use crate::ir::{Graph, IrBuilder, IrInlineFrameSites, IrType, NodeId, Op};
use std::collections::HashMap;

fn leaked_instance_info() -> usize {
    let info: &'static crate::JitInvokeInfo = Box::leak(Box::new(crate::JitInvokeInfo {
        class_name: "C",
        method_name: "inc",
        descriptor: "(I)I",
        num_jit_args: 2,
        return_type: b'I',
        invoke_kind: 1,
        declaring_class_id: 7,
        owner_class_id: 0,
    }));
    info as *const crate::JitInvokeInfo as usize
}

/// `static int caller(C c) { return c.inc(1); }` (`invokevirtual`, pc 2)
/// or the same with `invokespecial` (`super.inc(1)` on `this`): `aload_0;
/// iconst_1; invoke #5; ireturn`.
fn one_call(opcode: u8) -> [u8; 8] {
    [0x2a, 0x04, opcode, 0x00, 0x05, 0xac, 0, 0]
}

/// `c.inc(1) + c.inc(2)`: invokes at pc 2 and pc 7, 12 bytes plus the
/// VM's two padding bytes.
const TWO_CALLS: [u8; 14] = [
    0x2a, 0x04, 0xb6, 0x00, 0x05, // aload_0; iconst_1; invokevirtual #5
    0x2a, 0x05, 0xb6, 0x00, 0x05, // aload_0; iconst_2; invokevirtual #5
    0x60, 0xac, // iadd; ireturn
    0x00, 0x00,
];

fn build(code: &[u8], code_len: usize, sync_pcs: &[usize]) -> Graph {
    let info = leaked_instance_info();
    let mut b = IrBuilder::new(1, 1);
    b.set_param_types(&[IrType::Ref]);
    let mut invoke = HashMap::new();
    let mut sync = HashMap::new();
    for pc in [2usize, 7] {
        if pc + 3 <= code_len && matches!(code[pc], 0xb6 | 0xb7) {
            invoke.insert(pc, (info, 2usize, b'I'));
        }
    }
    for &pc in sync_pcs {
        sync.insert(pc, (0u32, 0u16, 0usize));
    }
    b.set_invoke_info(invoke);
    b.set_sync_direct_calls(sync);
    b.build(code, code_len).expect("the caller builds")
}

fn find_all(g: &Graph, pred: impl Fn(&Op) -> bool) -> Vec<NodeId> {
    g.nodes
        .iter()
        .enumerate()
        .filter(|(_, n)| pred(&n.op))
        .map(|(i, _)| i as NodeId)
        .collect()
}

/// The builder's half, for both statically bound opcodes: a guard on the
/// receiver at the invoke's bci BEFORE the enter, the enter and exit on
/// the receiver itself (no class mirror), the call between them on the
/// memory chain, and no frame state naming the lock.
#[test]
fn an_instance_sync_row_guards_the_receiver_before_locking_it() {
    for opcode in [0xb6u8, 0xb7] {
        let code = one_call(opcode);
        let g = build(&code, 6, &[2]);
        let receiver = find_all(&g, |op| matches!(op, Op::Param(0)))[0];
        let guards = find_all(&g, |op| matches!(op, Op::Guard { bci: 2 }));
        let enters = find_all(&g, |op| *op == Op::MonitorEnter);
        let calls = find_all(&g, |op| matches!(op, Op::Call { .. }));
        let exits = find_all(&g, |op| *op == Op::MonitorExit);
        assert_eq!(
            (guards.len(), enters.len(), calls.len(), exits.len()),
            (1, 1, 1, 1),
            "opcode {opcode:#04x}"
        );
        let (guard, enter, call, exit) = (guards[0], enters[0], calls[0], exits[0]);
        assert!(
            guard < enter,
            "the null check precedes every monitor action"
        );
        let node = |id: NodeId| &g.nodes[id as usize];
        assert_eq!(node(enter).input_opt(2), Some(receiver));
        assert_eq!(node(call).input_opt(1), Some(enter));
        assert_eq!(node(call).input_opt(2), Some(receiver));
        assert_eq!(node(exit).input_opt(1), Some(call));
        assert_eq!(node(exit).input_opt(2), Some(receiver));
        assert!(find_all(&g, |op| matches!(op, Op::ConstClass { .. })).is_empty());
        assert!(
            g.safepoints.iter().all(|sp| sp.monitors.is_empty()),
            "no frame state may name a caller-held method monitor"
        );
    }
    // Without the row the site is an ordinary call.
    let plain = build(&one_call(0xb6), 6, &[]);
    assert!(find_all(&plain, |op| matches!(
        op,
        Op::MonitorEnter | Op::MonitorExit | Op::Guard { .. }
    ))
    .is_empty());
}

extern "C" fn fake_enter(_vm: i64, obj: i64) -> i64 {
    obj
}
extern "C" fn fake_exit(_vm: i64, _obj: i64) -> i64 {
    1
}
extern "C" fn fake_instance_callee(_recv: i64, _arg: i64) -> i64 {
    0
}

fn lower(graph: &Graph, rows: &HashMap<usize, (usize, bool)>) -> Option<crate::CompiledMethod> {
    // SAFETY: `JitRuntimeHelpers` is `#[repr(C)]` and every field is a `usize`, so all-zero is a valid value; the test wires only the slots it exercises.
    let mut helpers: crate::JitRuntimeHelpers = unsafe { std::mem::zeroed() };
    helpers.monitor_enter = fake_enter as *const () as usize;
    helpers.monitor_exit = fake_exit as *const () as usize;
    let schedule = crate::ir_schedule::schedule(graph);
    crate::ir_lower::lower_inner(
        graph,
        &schedule,
        1,
        1,
        &helpers,
        &HashMap::new(),
        &[],
        None,
        rows,
        &HashMap::new(),
        &HashMap::new(),
        &IrInlineFrameSites::default(),
    )
}

fn count(bytes: &[u8], addr: usize) -> usize {
    let needle = addr.to_le_bytes();
    bytes.windows(8).filter(|w| *w == needle).count()
}

/// Drop the receiver guards (and nothing else) so these lowering tests
/// exercise the monitor pair alone; the guard is the builder test's.
fn without_guards(mut g: Graph) -> Graph {
    for id in find_all(&g, |op| matches!(op, Op::Guard { .. })) {
        g.kill(id);
    }
    g
}

/// The lowerer's half: one raw CALL to the callee, the receiver locked
/// once and released on both edges.
#[test]
fn an_instance_row_lowers_to_a_direct_call_released_on_both_edges() {
    if !crate::x64::merged_call_sentinel_enabled() {
        return;
    }
    let entry = fake_instance_callee as *const () as usize;
    let rows = HashMap::from([(sync_direct_row_key(2), (entry, false))]);
    let g = without_guards(build(&one_call(0xb6), 6, &[2]));
    let cm = lower(&g, &rows).expect("the caller lowers");
    let bytes = cm.code_bytes();
    assert_eq!(
        count(bytes, entry),
        1,
        "one raw CALL to the synchronized body"
    );
    assert_eq!(count(bytes, fake_enter as *const () as usize), 1);
    assert_eq!(
        count(bytes, fake_exit as *const () as usize),
        2,
        "normal and exceptional release"
    );
    // Fail closed: the tagged row with no pair in the graph.
    let bare = without_guards(build(&one_call(0xb6), 6, &[]));
    assert!(lower(&bare, &rows).is_none());
}

/// `c.inc(1) + c.inc(2)` after lock coarsening merged the first exit with
/// the second enter (`ir_optimize::coarsen_adjacent_monitors`, simulated
/// here exactly as it rewires): one enter, both calls on the chain, one
/// exit. Both calls still lower to the caller-held route -- each with its
/// own exceptional release -- instead of refusing the method.
#[test]
fn a_coarsened_pair_of_instance_calls_still_lowers() {
    if !crate::x64::merged_call_sentinel_enabled() {
        return;
    }
    let entry = fake_instance_callee as *const () as usize;
    let rows = HashMap::from([
        (sync_direct_row_key(2), (entry, false)),
        (sync_direct_row_key(7), (entry, false)),
    ]);
    let mut g = without_guards(build(&TWO_CALLS, 12, &[2, 7]));
    let enters = find_all(&g, |op| *op == Op::MonitorEnter);
    let exits = find_all(&g, |op| *op == Op::MonitorExit);
    assert_eq!((enters.len(), exits.len()), (2, 2));
    let (first_exit, second_enter) = (exits[0], enters[1]);
    let token = g.nodes[first_exit as usize]
        .input_opt(1)
        .expect("the first exit has a token");
    g.replace_all_uses(second_enter, token);
    g.kill(second_enter);
    g.kill(first_exit);
    let cm = lower(&g, &rows).expect("the coarsened chain lowers");
    let bytes = cm.code_bytes();
    assert_eq!(count(bytes, entry), 2);
    assert_eq!(count(bytes, fake_enter as *const () as usize), 1);
    assert_eq!(
        count(bytes, fake_exit as *const () as usize),
        3,
        "one normal release and one exceptional release per call"
    );
}
