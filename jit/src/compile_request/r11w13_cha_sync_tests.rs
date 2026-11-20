use super::*;
use crate::ir::{Graph, IrBuilder, IrInlineFrameSites, IrType, NodeId, Op};
use std::collections::HashMap;

const CLASS_ID: u32 = 0x0012_3457;

fn bind(implementor: u32, static_type: u32) -> Option<crate::UniqueConcreteBind> {
    Some(crate::UniqueConcreteBind {
        implementor_class_id: implementor,
        static_type_class_id: static_type,
        static_type_name: "C".to_string(),
    })
}

#[test]
fn the_planner_guards_only_on_the_methodref_class_itself() {
    assert_eq!(cha_sync_exact_class(bind(7, 7), Some(7)), Some(7));
    // A single concrete SUBCLASS provides the body: a guard on it would
    // select an inherited or overriding body the VM did not answer for.
    assert_eq!(cha_sync_exact_class(bind(8, 7), Some(7)), None);
    // The two resolvers disagree about which class the Methodref names.
    assert_eq!(cha_sync_exact_class(bind(7, 7), Some(9)), None);
    assert_eq!(cha_sync_exact_class(bind(7, 7), None), None);
    // No hierarchy answer ("many", or CHA switched off).
    assert_eq!(cha_sync_exact_class(None, Some(7)), None);
    // Zero is the builder's "no class guard" row.
    assert_eq!(cha_sync_exact_class(bind(0, 0), Some(0)), None);
}

fn leaked_info() -> usize {
    let info: &'static crate::JitInvokeInfo = Box::leak(Box::new(crate::JitInvokeInfo {
        class_name: "C",
        method_name: "inc",
        descriptor: "(I)I",
        num_jit_args: 2,
        return_type: b'I',
        // The planner re-marks a guarded CHA site statically bound.
        invoke_kind: 1,
        declaring_class_id: 7,
        owner_class_id: 0,
    }));
    info as *const crate::JitInvokeInfo as usize
}

/// `static int caller(C c) { return c.inc(1); }`: `aload_0; iconst_1;
/// invokevirtual #5; ireturn`, the invoke at pc 2.
const CALLER: [u8; 8] = [0x2a, 0x04, 0xb6, 0x00, 0x05, 0xac, 0, 0];

fn build(row_class_id: u32) -> Graph {
    let mut b = IrBuilder::new(1, 1);
    b.set_param_types(&[IrType::Ref]);
    b.set_invoke_info(HashMap::from([(2usize, (leaked_info(), 2usize, b'I'))]));
    b.set_sync_direct_calls(HashMap::from([(2usize, (row_class_id, 0u16, 0usize))]));
    b.build(&CALLER, 6).expect("the caller builds")
}

fn find_all(g: &Graph, pred: impl Fn(&Op) -> bool) -> Vec<NodeId> {
    g.nodes
        .iter()
        .enumerate()
        .filter(|(_, n)| pred(&n.op))
        .map(|(i, _)| i as NodeId)
        .collect()
}

/// `Guard(recv != null); Guard(ExactClassIs(recv)); MonitorEnter recv`,
/// all at the invoke's bci and in that order; a pinned row (`0`) has no
/// class guard at all.
#[test]
fn a_cha_row_guards_the_exact_class_before_the_enter() {
    let g = build(CLASS_ID);
    let receiver = find_all(&g, |op| matches!(op, Op::Param(0)))[0];
    let exact = find_all(&g, |op| *op == Op::ExactClassIs { class_id: CLASS_ID });
    assert_eq!(exact.len(), 1, "one class test");
    let exact = exact[0];
    let node = |id: NodeId| &g.nodes[id as usize];
    assert_eq!(node(exact).inputs, vec![receiver]);
    let guards = find_all(&g, |op| matches!(op, Op::Guard { bci: 2 }));
    assert_eq!(guards.len(), 2, "the null guard and the class guard");
    let class_guard = guards
        .iter()
        .copied()
        .find(|&gd| node(gd).input_opt(1) == Some(exact))
        .expect("a guard consumes the class test");
    let enter = find_all(&g, |op| *op == Op::MonitorEnter)[0];
    assert!(guards.iter().all(|&gd| gd < enter));
    assert!(class_guard < enter, "the class guard precedes the enter");
    assert_eq!(node(enter).input_opt(2), Some(receiver));
    assert!(g.safepoints.iter().all(|sp| sp.monitors.is_empty()));

    let pinned = build(0);
    assert!(find_all(&pinned, |op| matches!(op, Op::ExactClassIs { .. })).is_empty());
    assert_eq!(
        find_all(&pinned, |op| matches!(op, Op::Guard { .. })).len(),
        1,
        "a statically bound row keeps the null guard only"
    );
}

extern "C" fn fake_enter(_vm: i64, obj: i64) -> i64 {
    obj
}
extern "C" fn fake_exit(_vm: i64, _obj: i64) -> i64 {
    1
}
extern "C" fn fake_callee(_recv: i64, _arg: i64) -> i64 {
    0
}

/// The lowerer's half, guards included: the class test is emitted (its
/// `CMP DWORD [RAX+0], id`), nothing that can deopt lands inside the held
/// window (`sync_direct_call_monitor` would refuse the compile), and the
/// callee is CALLed raw once with the receiver locked once.
#[test]
fn a_cha_row_lowers_with_both_guards_outside_the_window() {
    if !crate::x64::merged_call_sentinel_enabled() {
        return;
    }
    let g = build(CLASS_ID);
    let entry = fake_callee as *const () as usize;
    let rows = HashMap::from([(sync_direct_row_key(2), (entry, false))]);
    // SAFETY: `JitRuntimeHelpers` is `#[repr(C)]` and every field is a `usize`, so all-zero is a valid value; the test wires only the slots it exercises.
    let mut helpers: crate::JitRuntimeHelpers = unsafe { std::mem::zeroed() };
    helpers.monitor_enter = fake_enter as *const () as usize;
    helpers.monitor_exit = fake_exit as *const () as usize;
    let schedule = crate::ir_schedule::schedule(&g);
    let cm = crate::ir_lower::lower_inner(
        &g,
        &schedule,
        1,
        1,
        &helpers,
        &HashMap::new(),
        &[],
        None,
        &rows,
        &HashMap::new(),
        &HashMap::new(),
        &IrInlineFrameSites::default(),
    )
    .expect("the guarded caller lowers");
    let bytes = cm.code_bytes();
    let mut cmp = vec![0x81u8, 0x78, 0x00];
    cmp.extend_from_slice(&CLASS_ID.to_le_bytes());
    assert!(
        bytes.windows(cmp.len()).any(|w| w == cmp.as_slice()),
        "the exact class compare is emitted"
    );
    let count = |addr: usize| {
        let needle = addr.to_le_bytes();
        bytes.windows(8).filter(|w| *w == needle).count()
    };
    assert_eq!(count(entry), 1, "one raw CALL to the synchronized body");
    assert_eq!(count(fake_enter as *const () as usize), 1);
}

/// EXECUTED: `int f(Object o) { return ExactClassIs(o); }` against
/// hand-made headers -- the class id word at offset 0 and the kind byte.
#[test]
fn exact_class_is_answers_non_null_plain_objects_of_that_class_only() {
    let mut g = Graph::empty(0);
    let start = g.add(Op::Start, IrType::Control, vec![], None);
    let ctrl = g.add(Op::Proj(0), IrType::Control, vec![start], None);
    let _mem = g.add(Op::Proj(1), IrType::Memory, vec![start], None);
    let obj = g.add(Op::Param(0), IrType::Ref, vec![start], None);
    let test = g.add(
        Op::ExactClassIs { class_id: CLASS_ID },
        IrType::Int,
        vec![obj],
        Some(0),
    );
    g.exit = g.add(Op::Return, IrType::Void, vec![ctrl, test], Some(0));
    let schedule = crate::ir_schedule::schedule(&g);
    // SAFETY: `JitRuntimeHelpers` is `#[repr(C)]` and every field is a `usize`, so all-zero is a valid value; this body calls no helper.
    let helpers: crate::JitRuntimeHelpers = unsafe { std::mem::zeroed() };
    let cm = crate::ir_lower::lower(&g, &schedule, 1, 1, &helpers).expect("lowers");
    assert!(
        cratonvm_types::KIND_TAGS_BYTE_OFFSET < 32,
        "precondition: the kind byte lies in the fake header"
    );
    let run = |class_id: u32, kind_byte: u8| -> i32 {
        let mut header = [0u64; 4];
        let base = header.as_mut_ptr().cast::<u8>();
        // SAFETY: both writes are inside the 32-byte, 8-aligned `header`.
        unsafe {
            base.cast::<u32>().write(class_id);
            *base.add(cratonvm_types::KIND_TAGS_BYTE_OFFSET) = kind_byte;
        }
        // SAFETY: an `int f(ref)` body with no call and no guard; the
        // reference is a live 32-byte buffer. Cast: pointer as the oop.
        let got = unsafe { cm.try_call(&[base as i64]) }.expect("call");
        // Cast: an `Int` result is the low 32 bits.
        got as i32
    };
    let plain = cratonvm_types::ObjectKind::Object as u8;
    assert_eq!(run(CLASS_ID, plain), 1, "exact class, plain object");
    assert_eq!(run(CLASS_ID + 1, plain), 0, "another class");
    assert_eq!(
        run(CLASS_ID ^ 0x0001_0000, plain),
        0,
        "a high-half difference"
    );
    assert_eq!(
        run(CLASS_ID, plain | 1),
        0,
        "an array whose COMPONENT is the class"
    );
    // SAFETY: as above; a null reference is answered without a load.
    let null = unsafe { cm.try_call(&[0]) }.expect("call");
    // Cast: an `Int` result is the low 32 bits.
    assert_eq!(null as i32, 0, "null");
}
