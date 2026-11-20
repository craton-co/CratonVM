//! JIT round 18 wave 9 (lane syncdoor6, SD17-3): the monitor-shape census
//! a self-locking body with `synchronized` blocks is admitted on, and the
//! marked `[method, block..]` hand-over question.

use super::{
    point_hands_self_lock_over, self_lock_method_monitor, taken_monitor_shape,
    TakenMonitorShape,
};
use crate::deopt::{
    DeoptAction, DeoptReason, DeoptimizationPoint, FrameState, FrameValue, MonitorInfo,
    ResumeSemantics,
};
use std::collections::HashSet;

const OBJ_OFF: i32 = 8;

/// `synchronized int blocked(int x) { synchronized (other) {
/// o += helper2(x); } return o; }` (`R16SyncDoorCalls bl`).
#[rustfmt::skip]
const BLOCKED: [u8; 36] = [
    0x2a, 0xb4, 0x00, 0x02,       //  0: aload_0; getfield other
    0x59, 0x4d, 0xc2,             //  4: dup; astore_2; monitorenter
    0x2a, 0x59, 0xb4, 0x00, 0x03, //  7: aload_0; dup; getfield o
    0x2a, 0x1b,                   // 12: aload_0; iload_1
    0xb6, 0x00, 0x04,             // 14: invokevirtual helper2
    0x60, 0xb5, 0x00, 0x03,       // 17: iadd; putfield o
    0x2c, 0xc3,                   // 21: aload_2; monitorexit
    0xa7, 0x00, 0x08,             // 23: goto 31
    0x4e, 0x2c, 0xc3, 0x2d, 0xbf, // 26: astore_3; aload_2; monitorexit; aload_3; athrow
    0x2a, 0xb4, 0x00, 0x03, 0xac, // 31: aload_0; getfield o; ireturn
];
const BLOCKED_TABLE: [(usize, usize, usize); 2] = [(7, 23, 26), (26, 29, 26)];

fn shape(code: &[u8], ranges: &[(usize, usize, usize)], elided: &[usize]) -> TakenMonitorShape {
    let elided: HashSet<usize> = elided.iter().copied().collect();
    taken_monitor_shape(
        code,
        code.len(),
        ranges,
        &[true; 2][..ranges.len().min(2)],
        &elided,
    )
}

#[test]
fn javac_blocks_are_described_and_everything_else_is_not() {
    assert_eq!(
        shape(&BLOCKED, &BLOCKED_TABLE, &[]),
        TakenMonitorShape::Described
    );
    // No monitor bytecode at all: aload_0; getfield o; ireturn.
    assert_eq!(
        shape(&[0x2a, 0xb4, 0x00, 0x03, 0xac], &[], &[]),
        TakenMonitorShape::NoMonitorOps
    );
    // An elided enter is not a taken level.
    assert_eq!(
        shape(&BLOCKED, &BLOCKED_TABLE, &[6]),
        TakenMonitorShape::Undescribed
    );
    // The exit names a different local than the enter stored: aload_3.
    let mut wrong_exit = BLOCKED;
    wrong_exit[21] = 0x2d;
    assert_eq!(
        shape(&wrong_exit, &BLOCKED_TABLE, &[]),
        TakenMonitorShape::Undescribed
    );
    // An exit with no enter.
    assert_eq!(
        shape(&[0x2a, 0xc3, 0xb1], &[], &[]),
        TakenMonitorShape::Undescribed
    );
    // A return with the block still open: aload_0; dup; astore_1;
    // monitorenter; iconst_0; ireturn.
    assert_eq!(
        shape(&[0x2a, 0x59, 0x4c, 0xc2, 0x03, 0xac], &[], &[]),
        TakenMonitorShape::Undescribed
    );
}

fn point(monitors: Vec<MonitorInfo>, marked: bool) -> DeoptimizationPoint {
    DeoptimizationPoint {
        native_offset: 0,
        bci: 14,
        reason: DeoptReason::PendingException,
        action: DeoptAction::Reinterpret,
        speculation_id: 0,
        frame_state: FrameState {
            class_id: 0,
            method_key: "C.blocked:(I)I".to_string(),
            bci: 14,
            locals: vec![FrameValue::StackSlotRef(-OBJ_OFF)],
            stack: Vec::new(),
            monitors,
            caller: None,
        },
        semantics: ResumeSemantics::RETHROW.with_method_monitor_first(marked),
    }
}

fn block(off: i32) -> MonitorInfo {
    MonitorInfo {
        object: FrameValue::StackSlotRef(-off),
        lock_depth: 1,
        relock: false,
    }
}

#[test]
fn only_a_marked_point_hands_over_beside_blocks() {
    let method = self_lock_method_monitor(OBJ_OFF);
    assert!(point_hands_self_lock_over(
        &point(vec![method.clone(), block(24)], true),
        OBJ_OFF
    ));
    assert!(
        !point_hands_self_lock_over(&point(vec![method.clone(), block(24)], false), OBJ_OFF),
        "unmarked: the pre-SD17-3 shape rule"
    );
    assert!(
        !point_hands_self_lock_over(&point(vec![block(24), method.clone()], true), OBJ_OFF),
        "the method monitor must come first"
    );
    let mut elided = block(24);
    elided.relock = true;
    assert!(!point_hands_self_lock_over(
        &point(vec![method.clone(), elided], true),
        OBJ_OFF
    ));
    assert!(
        !point_hands_self_lock_over(
            &point(vec![method.clone(), method.clone()], true),
            OBJ_OFF
        ),
        "the method monitor named twice"
    );
    // Marked or not, the method monitor alone hands over as before.
    assert!(point_hands_self_lock_over(
        &point(vec![method.clone()], true),
        OBJ_OFF
    ));
    assert!(point_hands_self_lock_over(
        &point(vec![method], false),
        OBJ_OFF
    ));
    assert!(!point_hands_self_lock_over(
        &point(Vec::new(), true),
        OBJ_OFF
    ));
}
