use super::*;
use crate::ir::{IrBuilder, IrInlineFrameSites};
use std::collections::HashMap;

const INVOKE_PC: usize = 1;

extern "C" fn fake_enter(_vm: i64, obj: i64) -> i64 {
    obj
}
extern "C" fn fake_exit(_vm: i64, _obj: i64) -> i64 {
    1
}
extern "C" fn fake_ldc_class(_vm: i64, _holder: i64, _cp: i64) -> i64 {
    0x1000
}
// Distinct bodies: a release link folds identical functions (`/OPT:ICF`),
// and two `return 0` fakes sharing one address make every count below
// count both.
extern "C" fn fake_threw() -> i64 {
    0
}
extern "C" fn fake_callee(arg: i64) -> i64 {
    arg ^ 0x5a5a
}

/// `static R caller() { return syncValue(1); }`: `iconst_1; invokestatic
/// #5; <ret_op>`, the invoke at pc 1, with its caller-held sync row.
fn build_caller(ret: u8, ret_op: u8) -> crate::ir::Graph {
    let descriptor: &'static str = match ret {
        b'J' => "(I)J",
        _ => "(I)Ljava/lang/Object;",
    };
    let info: &'static crate::JitInvokeInfo = Box::leak(Box::new(crate::JitInvokeInfo {
        class_name: "C",
        method_name: "syncValue",
        descriptor,
        num_jit_args: 1,
        return_type: ret,
        invoke_kind: 3,
        declaring_class_id: 7,
        owner_class_id: 0,
    }));
    let code = [0x04, 0xb8, 0x00, 0x05, ret_op, 0, 0];
    let mut b = IrBuilder::new(0, 0);
    b.set_invoke_info(HashMap::from([(
        INVOKE_PC,
        (info as *const crate::JitInvokeInfo as usize, 1usize, ret),
    )]));
    b.set_sync_direct_calls(HashMap::from([(INVOKE_PC, (7u32, 12u16, 0usize))]));
    b.build(&code, 5).expect("the caller builds")
}

fn lower(graph: &crate::ir::Graph, dispatch_threw: usize) -> Option<crate::CompiledMethod> {
    let entry = fake_callee as *const () as usize;
    let rows = HashMap::from([(sync_direct_row_key(INVOKE_PC), (entry, false))]);
    // SAFETY: `JitRuntimeHelpers` is `#[repr(C)]` and every field is a `usize`, so all-zero is a valid value; the test wires only the slots it exercises.
    let mut helpers: crate::JitRuntimeHelpers = unsafe { std::mem::zeroed() };
    helpers.monitor_enter = fake_enter as *const () as usize;
    helpers.monitor_exit = fake_exit as *const () as usize;
    helpers.ldc_class_cp = fake_ldc_class as *const () as usize;
    helpers.dispatch_threw = dispatch_threw;
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
        &rows,
        &HashMap::new(),
        &HashMap::new(),
        &IrInlineFrameSites::default(),
    )
}

fn count(bytes: &[u8], addr: usize) -> usize {
    let needle = addr.to_le_bytes();
    bytes.windows(8).filter(|w| *w == needle).count()
}

#[test]
fn the_planner_admits_every_result_kind_and_wide_ones_only_with_the_peek() {
    for ret in *b"VIZBCSL[" {
        assert!(sync_direct_result_admitted(ret, 0), "{}", ret as char);
    }
    for ret in *b"JFD" {
        assert!(!sync_direct_result_admitted(ret, 0), "{}", ret as char);
        assert!(sync_direct_result_admitted(ret, 0x1234), "{}", ret as char);
    }
    assert!(!sync_direct_result_admitted(b'X', 0x1234));
}

/// A `long` result: one raw CALL, the class monitor released on both
/// edges, and the `dispatch_threw` peek on the cold side so a genuine
/// `Long.MIN_VALUE` keeps the normal edge.
#[test]
fn a_long_result_binds_with_the_dispatch_threw_peek() {
    if !crate::x64::merged_call_sentinel_enabled() {
        return;
    }
    let threw = fake_threw as *const () as usize;
    let cm = lower(&build_caller(b'J', 0xad), threw).expect("the caller lowers");
    let bytes = cm.code_bytes();
    assert_eq!(count(bytes, fake_callee as *const () as usize), 1);
    assert!(count(bytes, threw) >= 1, "the cold side peeks the signal");
    assert_eq!(count(bytes, fake_enter as *const () as usize), 1);
    assert_eq!(count(bytes, fake_exit as *const () as usize), 2);
}

/// Fail closed: without the peek helper a wide result cannot be told from
/// the sentinel, so the lowerer refuses the compile.
#[test]
fn a_long_result_without_the_peek_refuses_the_compile() {
    assert!(lower(&build_caller(b'J', 0xad), 0).is_none());
}

/// A reference result: bound and released on both edges, with no peek
/// (a reference is never `i64::MIN`).
#[test]
fn a_reference_result_binds_and_is_released_on_both_edges() {
    if !crate::x64::merged_call_sentinel_enabled() {
        return;
    }
    let threw = fake_threw as *const () as usize;
    let cm = lower(&build_caller(b'L', 0xb0), threw).expect("the caller lowers");
    let bytes = cm.code_bytes();
    assert_eq!(count(bytes, fake_callee as *const () as usize), 1);
    assert_eq!(count(bytes, threw), 0, "no peek for a reference result");
    assert_eq!(count(bytes, fake_exit as *const () as usize), 2);
}
