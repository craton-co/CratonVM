use super::{
    point_needs_unsound_replay, single_pass_first_trap_needing_unsound_replay, DeoptAction,
    DeoptReason, DeoptimizationPoint, FrameState, FrameValue, ResumeSemantics,
};

/// `putstatic` before a loop whose guard (bci 7) sits above a second
/// store:
///
/// ```text
///  0: iconst_1   1: putstatic #1   4: iconst_0   5: istore_0
///  6: iload_0    7: bipush 10      9: if_icmpge 22
/// 12: iconst_2  13: putstatic #1  16: iinc 0 1  19: goto 6
/// 22: return
/// ```
const STORE_THEN_LOOP: [u8; 23] = [
    0x04, 0xb3, 0x00, 0x01, 0x03, 0x3b, 0x1a, 0x10, 0x0a, 0xa2, 0x00, 0x0d, 0x05, 0xb3, 0x00,
    0x01, 0x84, 0x00, 0x01, 0xa7, 0xff, 0xf3, 0xb1,
];

/// `iload_0 iload_1 iadd ireturn`: commits nothing.
const PURE: [u8; 4] = [0x1a, 0x1b, 0x60, 0xac];

fn point(bci: u32, reason: DeoptReason, local: FrameValue) -> DeoptimizationPoint {
    DeoptimizationPoint {
        native_offset: 0,
        bci,
        reason,
        action: DeoptAction::Reinterpret,
        speculation_id: 0,
        frame_state: FrameState {
            class_id: 0,
            method_key: String::new(),
            bci,
            locals: vec![local],
            stack: Vec::new(),
            monitors: Vec::new(),
            caller: None,
        },
        semantics: ResumeSemantics::for_reason(reason),
    }
}

#[test]
fn a_point_the_sink_resumes_needs_no_replay() {
    let code = STORE_THEN_LOOP;
    let p = point(7, DeoptReason::BoundsCheck, FrameValue::Int(1));
    assert!(!point_needs_unsound_replay(
        &p,
        &code,
        code.len(),
        true,
        true
    ));
    // The same point with the body's gate shut: the replay would re-run
    // the putstatic at bci 1.
    assert!(point_needs_unsound_replay(
        &p,
        &code,
        code.len(),
        true,
        false
    ));
    // An undescribable slot: no sink resumes it whatever the gate says.
    let u = point(7, DeoptReason::BoundsCheck, FrameValue::Unsupported);
    assert!(point_needs_unsound_replay(
        &u,
        &code,
        code.len(),
        true,
        true
    ));
    // An OSR-exit map and a rethrow frame never reach a replay.
    let osr = point(7, DeoptReason::OsrExit, FrameValue::Unsupported);
    assert!(!point_needs_unsound_replay(
        &osr,
        &code,
        code.len(),
        true,
        false
    ));
    let rethrow = point(7, DeoptReason::PendingException, FrameValue::Unsupported);
    assert!(!point_needs_unsound_replay(
        &rethrow,
        &code,
        code.len(),
        true,
        false
    ));
}

#[test]
fn the_single_pass_check_asks_baked_points_and_frameless_traps() {
    let code = STORE_THEN_LOOP;
    let len = code.len();
    let resumable = point(7, DeoptReason::BoundsCheck, FrameValue::Int(1));
    let unresumable = point(7, DeoptReason::ReceiverTypeChanged, FrameValue::Unsupported);
    let none: [&DeoptimizationPoint; 0] = [];
    // Nothing baked, nothing frameless.
    assert_eq!(
        single_pass_first_trap_needing_unsound_replay(none, [], &code, len, true),
        None
    );
    // A resumable baked point is fine; an unresumable one is refused.
    assert_eq!(
        single_pass_first_trap_needing_unsound_replay([&resumable], [], &code, len, true),
        None
    );
    assert_eq!(
        single_pass_first_trap_needing_unsound_replay([&unresumable], [], &code, len, true),
        Some(7)
    );
    // A frameless trap before the first store replays exactly; one after
    // it, or inside the loop, does not.
    assert_eq!(
        single_pass_first_trap_needing_unsound_replay(none, [0], &code, len, true),
        None
    );
    assert_eq!(
        single_pass_first_trap_needing_unsound_replay(none, [4, 7], &code, len, true),
        Some(4)
    );
    // A body that commits nothing replays from anywhere.
    let pure = PURE;
    let u = point(2, DeoptReason::BoundsCheck, FrameValue::Unsupported);
    assert_eq!(
        single_pass_first_trap_needing_unsound_replay([&u], [2], &pure, pure.len(), false),
        None
    );
}
