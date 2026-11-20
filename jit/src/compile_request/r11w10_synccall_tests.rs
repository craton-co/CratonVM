use super::*;

/// A sync row can never answer a plain-pc lookup, and two sites never
/// share a key.
#[test]
fn a_sync_row_key_is_disjoint_from_every_plain_pc() {
    for pc in [0usize, 1, 3, 65_535, 1 << 20, u32::MAX as usize] {
        let key = sync_direct_row_key(pc);
        assert_ne!(key, pc);
        assert_eq!(key & !SYNC_DIRECT_ROW_TAG, pc);
    }
    assert_ne!(sync_direct_row_key(3), sync_direct_row_key(6));
}

use crate::ir::{IrBuilder, IrInlineFrameSites, Op};
use std::collections::HashMap;

/// `static int caller() { return incSync(1); }`:
/// `iconst_1; invokestatic #5; ireturn`, the invoke at pc 1.
const CALLER: [u8; 7] = [0x04, 0xb8, 0x00, 0x05, 0xac, 0, 0];
const INVOKE_PC: usize = 1;

fn leaked_sync_info() -> usize {
    let info: &'static crate::JitInvokeInfo = Box::leak(Box::new(crate::JitInvokeInfo {
        class_name: "C",
        method_name: "incSync",
        descriptor: "(I)I",
        num_jit_args: 1,
        return_type: b'I',
        invoke_kind: 3,
        declaring_class_id: 7,
        owner_class_id: 0,
    }));
    info as *const crate::JitInvokeInfo as usize
}

fn build_caller(sync_row: bool) -> crate::ir::Graph {
    let mut b = IrBuilder::new(0, 0);
    b.set_invoke_info(HashMap::from([(
        INVOKE_PC,
        (leaked_sync_info(), 1usize, b'I'),
    )]));
    if sync_row {
        b.set_sync_direct_calls(HashMap::from([(INVOKE_PC, (7u32, 12u16, 0usize))]));
    }
    b.build(&CALLER, 5).expect("the caller builds")
}

/// The builder's half: `ConstClass; MonitorEnter; Call; MonitorExit` on
/// one object, threaded on the memory chain in that order, and no frame
/// state names the lock (contract item 2).
#[test]
fn a_sync_row_brackets_the_call_with_an_unrecorded_class_monitor() {
    let g = build_caller(true);
    fn find(g: &crate::ir::Graph, pred: impl Fn(&Op) -> bool) -> Option<crate::ir::NodeId> {
        g.nodes
            .iter()
            .position(|n| pred(&n.op))
            .map(|i| i as crate::ir::NodeId)
    }
    let m = find(&g, |op| {
        matches!(
            op,
            Op::ConstClass {
                holder_class_id: 7,
                cp_idx: 12,
                slot_addr: 0
            }
        )
    })
    .expect("the monitor is the Methodref's class mirror");
    let enter = find(&g, |op| *op == Op::MonitorEnter).expect("an enter");
    let call = find(&g, |op| matches!(op, Op::Call { .. })).expect("the call");
    let exit = find(&g, |op| *op == Op::MonitorExit).expect("an exit");
    let node = |id: crate::ir::NodeId| &g.nodes[id as usize];
    assert_eq!(node(enter).input_opt(1), Some(m));
    assert_eq!(node(enter).input_opt(2), Some(m));
    assert_eq!(node(call).input_opt(1), Some(enter));
    assert_eq!(node(exit).input_opt(1), Some(call));
    assert_eq!(node(exit).input_opt(2), Some(m));
    for id in [m, enter, call, exit] {
        assert_eq!(node(id).bytecode_pc, Some(INVOKE_PC));
    }
    assert!(
        g.safepoints.iter().all(|sp| sp.monitors.is_empty()),
        "no frame state may name a caller-held method monitor"
    );
    // Without the row the site is an ordinary call.
    let plain = build_caller(false);
    assert!(!plain.nodes.iter().any(|n| matches!(
        n.op,
        Op::MonitorEnter | Op::MonitorExit | Op::ConstClass { .. }
    )));
}

extern "C" fn fake_enter(_vm: i64, obj: i64) -> i64 {
    obj
}
extern "C" fn fake_exit(_vm: i64, _obj: i64) -> i64 {
    1
}
extern "C" fn fake_ldc_class(_vm: i64, _holder: i64, _cp: i64) -> i64 {
    0x1000
}
extern "C" fn fake_callee(_arg: i64) -> i64 {
    0
}

fn lower_caller(
    graph: &crate::ir::Graph,
    rows: &HashMap<usize, (usize, bool)>,
) -> Option<crate::CompiledMethod> {
    // SAFETY: `JitRuntimeHelpers` is `#[repr(C)]` and every field is a `usize`, so all-zero is a valid value; the test wires only the slots it exercises.
    let mut helpers: crate::JitRuntimeHelpers = unsafe { std::mem::zeroed() };
    helpers.monitor_enter = fake_enter as *const () as usize;
    helpers.monitor_exit = fake_exit as *const () as usize;
    helpers.ldc_class_cp = fake_ldc_class as *const () as usize;
    let schedule = crate::ir_schedule::schedule(graph);
    crate::ir_lower::lower_inner(
        graph,
        &schedule,
        0,
        0,
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

/// The lowerer's half: the tagged row binds a raw CALL to the callee, and
/// the class monitor is released on BOTH edges (the `MonitorExit` node and
/// the exceptional-edge release), each through the exit helper when the
/// thin path declines.
#[test]
fn a_tagged_row_lowers_to_a_direct_call_released_on_both_edges() {
    if !crate::x64::merged_call_sentinel_enabled() {
        return;
    }
    let entry = fake_callee as *const () as usize;
    let rows = HashMap::from([(sync_direct_row_key(INVOKE_PC), (entry, false))]);
    let cm = lower_caller(&build_caller(true), &rows).expect("the caller lowers");
    let bytes = cm.code_bytes();
    let count = |addr: usize| {
        let needle = addr.to_le_bytes();
        bytes.windows(8).filter(|w| *w == needle).count()
    };
    assert_eq!(count(entry), 1, "one raw CALL to the synchronized body");
    assert_eq!(count(fake_enter as *const () as usize), 1);
    assert_eq!(
        count(fake_exit as *const () as usize),
        2,
        "normal and exceptional release"
    );
}

/// Fail closed: a tagged row whose graph carries no monitor pair (the
/// builder was not told) is refused, never lowered to a monitor-free CALL.
#[test]
fn a_tagged_row_without_its_monitor_pair_refuses_the_compile() {
    let entry = fake_callee as *const () as usize;
    let rows = HashMap::from([(sync_direct_row_key(INVOKE_PC), (entry, false))]);
    assert!(lower_caller(&build_caller(false), &rows).is_none());
}
