use super::*;
use crate::ir::IrBuilder;
use crate::ir_schedule;
use cratonvm_types::flags::with_thread_overrides;


fn no_helpers() -> JitRuntimeHelpers {
    // SAFETY: `JitRuntimeHelpers` is `#[repr(C)]` and every field is a `usize`, so all-zero is a valid value; the tests wire only the poll slots.
    unsafe { std::mem::zeroed() }
}

fn graph_of(code: &[u8], params: usize, locals: usize) -> (Graph, Schedule) {
    let graph = IrBuilder::new(params, locals)
        .build(code, code.len())
        .expect("IR build");
    let schedule = ir_schedule::schedule(&graph);
    (graph, schedule)
}

/// The slow path counts its stops in word 1 of the poll block it is
/// handed (word 0's low byte is the flag the poll tests). No `static`:
/// the jit crate's statics ratchet counts test ones too.
extern "C" fn counting_poll(flag_addr: i64, _body_id: i64) -> i64 {
    // SAFETY: `flag_addr` is word 0 of a leaked `[u64; 2]`
    // (`polling_helpers`).
    unsafe {
        let hits = (flag_addr as *mut u64).add(1);
        hits.write_volatile(hits.read_volatile() + 1);
    }
    0
}

/// Helpers with a set poll flag (every poll stops) and the leaked poll
/// block, `[flag, hits]`.
fn polling_helpers() -> (JitRuntimeHelpers, *mut u64) {
    let block: *mut u64 = Box::leak(Box::new([1u64, 0])).as_mut_ptr();
    let mut helpers = no_helpers();
    helpers.safepoint_flag_addr = block as usize; // Cast: leaked address
    helpers.safepoint_slow_path = counting_poll as *const () as usize;
    (helpers, block)
}

fn hits(block: *mut u64) -> usize {
    // SAFETY: word 1 of the leaked block `polling_helpers` made.
    let n = unsafe { block.add(1).read_volatile() };
    // Cast: a small call count.
    n as usize
}

fn reset_hits(block: *mut u64) {
    // SAFETY: as in `hits`.
    unsafe { block.add(1).write_volatile(0) }
}

/// `static int min(int a, int b) { return a < b ? a : b; }`
const MIN: [u8; 9] = [0x1a, 0x1b, 0xa2, 0x00, 0x05, 0x1a, 0xac, 0x1b, 0xac];
/// `static int div(int a, int b) { return a / b; }`
const DIV: [u8; 4] = [0x1a, 0x1b, 0x6c, 0xac];
/// `static int f(int n) { int s = 0, i = 0; do { s += i; i++; } while (i < n); return s; }`
const LOOP: [u8; 18] = [
    0x03, 0x3c, 0x03, 0x3d, 0x1b, 0x1c, 0x60, 0x3c, 0x84, 0x02, 0x01, 0x1c, 0x1a, 0xa1, 0xff,
    0xf7, 0x1b, 0xac,
];

#[test]
fn r25w6d_cycle_detection_is_exact_and_fails_closed() {
    // A diamond: no cycle.
    assert!(!block_cfg_has_cycle(&[
        vec![1, 2],
        vec![3],
        vec![3],
        vec![]
    ]));
    // A back edge and a self-loop.
    assert!(block_cfg_has_cycle(&[vec![1], vec![2], vec![1, 3], vec![]]));
    assert!(block_cfg_has_cycle(&[vec![0]]));
    // A cycle among unreachable blocks counts.
    assert!(block_cfg_has_cycle(&[vec![], vec![2], vec![1]]));
    // An out-of-range successor fails closed.
    assert!(block_cfg_has_cycle(&[vec![7]]));
    assert!(!block_cfg_has_cycle(&[]));
}

#[test]
fn r25w6d_the_allow_list_has_no_calling_or_trapping_op() {
    assert!(op_dispatches_java(&Op::Call { info_ptr: 0 }));
    assert!(op_dispatches_java(&Op::IndyBridge { site: 0 }));
    assert!(op_dispatches_java(&Op::LambdaIntToDouble));
    assert!(!op_dispatches_java(&Op::Add));
    for op in [
        Op::Add,
        Op::Phi,
        Op::Return,
        Op::Param(0),
        Op::Cmp(CmpOp::Lt),
    ] {
        assert!(op_is_leaf_frame_pure(&op), "{op:?}");
    }
    for op in [
        Op::Div,
        Op::Rem,
        Op::Throw,
        Op::Call { info_ptr: 0 },
        Op::Guard { bci: 0 },
        Op::ArrayLength,
        Op::MonitorEnter,
        Op::KeepAlive,
        Op::F2I,
    ] {
        assert!(!op_is_leaf_frame_pure(&op), "{op:?}");
    }
}

/// The plan, per body shape, with each switch alone and both.
#[test]
fn r25w6d_the_plan_follows_the_body_shape() {
    let (g, s) = graph_of(&MIN, 2, 2);
    let off = plan_leaf_entry(&g, &s, false, false, false);
    assert!(
        off.loop_free && off.java_calls == 0 && off.impure_ops == 0,
        "{off:?}"
    );
    assert!(
        !off.elide_entry_poll && !off.leaf_frame,
        "switches off: nothing"
    );
    let poll = plan_leaf_entry(&g, &s, true, false, false);
    assert!(poll.elide_entry_poll && !poll.leaf_frame);
    let frame = plan_leaf_entry(&g, &s, false, true, false);
    assert!(frame.elide_entry_poll && frame.leaf_frame);
    // A self-locking body is never a leaf frame.
    assert!(!plan_leaf_entry(&g, &s, true, true, true).leaf_frame);

    // A division: loop-free, no Java call, but not a leaf frame.
    let (g, s) = graph_of(&DIV, 2, 2);
    let p = plan_leaf_entry(&g, &s, true, true, false);
    assert!(p.loop_free && p.impure_ops > 0, "{p:?}");
    assert!(p.elide_entry_poll && !p.leaf_frame);

    // A loop: neither.
    let (g, s) = graph_of(&LOOP, 1, 3);
    let p = plan_leaf_entry(&g, &s, true, true, false);
    assert!(
        !p.loop_free && !p.elide_entry_poll && !p.leaf_frame,
        "{p:?}"
    );
}



/// The verification refuses a leaf frame that emitted any recorded GC
/// point or exit, and passes one that emitted none.
#[test]
fn r25w6d_the_verification_refuses_a_leaf_frame_with_a_gc_point() {
    let mut lo = super::tests::lowerer_with_resident_xmm(4096, None);
    lo.leaf_entry.plan.leaf_frame = true;
    lo.verify_leaf_entry();
    assert!(
        lo.take_latched_bailout().is_none(),
        "nothing emitted: accepted"
    );
    lo.leaf_entry.polls_seen = 1;
    lo.verify_leaf_entry();
    assert!(lo.take_latched_bailout().is_some(), "a poll refuses");
    lo.leaf_entry.polls_seen = 0;
    lo.leaf_entry.stub_exits = 2;
    lo.verify_leaf_entry();
    assert!(lo.take_latched_bailout().is_some(), "an exit refuses");
    lo.leaf_entry.stub_exits = 0;
    lo.shadow_pushed_any = true;
    lo.verify_leaf_entry();
    assert!(lo.take_latched_bailout().is_some(), "a shadow push refuses");
    // Not a leaf frame: never refused.
    lo.leaf_entry.plan.leaf_frame = false;
    lo.verify_leaf_entry();
    assert!(lo.take_latched_bailout().is_none());
}

