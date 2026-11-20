//! JIT round 17 wave 2 (lane syncdoor, SY16-4): the per-stub rule and the
//! candidate bound of `Compiler::self_lock_handler_exits_admitted`.
use super::{local_handler_candidates_bounded, self_lock_handler_stub_admitted};

#[test]
fn only_exits_offered_to_the_frames_handlers_are_admitted() {
    let admitted = self_lock_handler_stub_admitted;
    // The direct `/ by zero` throw: free outside every range, behind a
    // site inside one.
    assert!(admitted(3, false, false, false, false, false));
    assert!(admitted(3, true, true, false, false, false));
    assert!(!admitted(3, true, false, false, false, false));
    // A precise exceptional frame: a site, and for reason 9 a propagate
    // route that survives the lookup's call.
    assert!(admitted(9, true, true, true, false, false));
    assert!(!admitted(9, true, true, false, false, false));
    assert!(!admitted(9, true, false, true, false, false));
    for reason in 10..=12 {
        assert!(admitted(reason, true, true, false, false, false));
        assert!(!admitted(reason, true, false, false, false, false));
        assert!(!admitted(reason, false, true, false, false, false));
    }
    // Guards only when they hand the monitor over.
    for reason in [2, 6] {
        assert!(admitted(reason, true, true, false, true, false));
        assert!(!admitted(reason, true, true, false, false, false));
    }
    // Everything else resumes or re-runs the method: never.
    for reason in [0, 7, 8, 13] {
        assert!(!admitted(reason, true, true, true, true, false));
    }
    // JIT round 19 wave 3 (lane syncdoor3, SYNCDOOR3-1): a mode exit
    // (reason 7, or the post-call own-point tag 13) only when its point
    // hands the hold over, protected or not; the flag means nothing for
    // any other reason.
    for reason in [7, 13] {
        for protected in [false, true] {
            assert!(admitted(reason, protected, false, false, false, true));
            assert!(!admitted(reason, protected, true, true, true, false));
        }
    }
    for reason in [0, 8] {
        assert!(!admitted(reason, true, true, true, true, true));
    }
    assert!(!admitted(9, true, false, true, false, true));
}

/// JIT round 17 wave 2 (lane syncdoor2, SD17-1): under the exceptional
/// hand-over a protected exit must keep the hold for the sink.
#[test]
fn the_exceptional_hand_over_admits_only_exits_that_keep_the_hold() {
    use super::self_lock_exc_exit_closes_window;
    assert!(self_lock_exc_exit_closes_window(3, false, false));
    assert!(!self_lock_exc_exit_closes_window(3, true, false));
    for reason in 9..=12 {
        assert!(self_lock_exc_exit_closes_window(reason, true, true));
        assert!(!self_lock_exc_exit_closes_window(reason, true, false));
    }
    // Guards are decided by the wave-2 rule alone.
    for reason in [2, 6] {
        assert!(self_lock_exc_exit_closes_window(reason, true, false));
    }
}


/// Emit the stubs of an INSTANCE self-locking body with one reason-9
/// precise exceptional frame whose point is `p`; the bytes past the body.
fn exc_stub_bytes(p: crate::deopt::DeoptimizationPoint, exit: usize) -> Vec<u8> {
    let mut c = super::super::emit::tests::test_compiler();
    c.direct_helpers.monitor_exit = exit;
    c.needs_heap = true;
    c.heap_local_offset = 16;
    c.self_lock_obj_off = 8;
    c.self_lock_release_armed = true;
    c.deopt_regs_base = 512;
    let patch = c.buf.pos();
    c.buf.emit(&[0, 0, 0, 0]);
    let boxed = Box::new(p.clone());
    let key: *const crate::deopt::DeoptimizationPoint = std::ptr::addr_of!(*boxed);
    // A fixture seeding the map, not an emitter filing site (see the
    // r13w9 fixture's note on `extend`).
    c.exc_frame_box_ptr_by_bci.extend([(10, key)]);
    c.deopt_boxes.push(boxed);
    c.deopt_points.push(p);
    c.deopt_stubs.push((patch, 10, 9));
    let start = c.buf.pos();
    c.emit_deopt_stubs();
    assert!(!c.failed);
    c.buf.as_slice()[start..].to_vec()
}

/// JIT round 17 wave 2 (lane syncdoor2, SD17-1): a reason-9 stub whose
/// point names the method monitor leaves WITHOUT releasing it (the VM sink
/// adopts or releases it); one whose point names nothing releases in its
/// epilogue, as before.
#[test]
fn a_reason_9_stub_naming_the_hold_keeps_it() {
    use crate::deopt::{
        DeoptAction, DeoptReason, DeoptimizationPoint, FrameState, FrameValue, MonitorInfo,
        ResumeSemantics,
    };
    const EXIT: usize = 0x0123_4567_89AB_CDE7;
    let point = |monitors: Vec<MonitorInfo>| DeoptimizationPoint {
        native_offset: 0,
        bci: 10,
        reason: DeoptReason::PendingException,
        action: DeoptAction::Reinterpret,
        speculation_id: 0,
        frame_state: FrameState {
            class_id: 0,
            method_key: "S.m:()I".to_string(),
            bci: 10,
            locals: vec![FrameValue::StackSlotRef(-8)],
            stack: Vec::new(),
            monitors,
            caller: None,
        },
        semantics: ResumeSemantics::for_reason(DeoptReason::PendingException),
    };
    let find = |hay: &[u8], needle: &[u8]| hay.windows(needle.len()).any(|w| w == needle);
    let handing = exc_stub_bytes(point(vec![super::self_lock_method_monitor(8)]), EXIT);
    assert!(
        !find(&handing, &EXIT.to_le_bytes()),
        "the hold is handed over"
    );
    let plain = exc_stub_bytes(point(Vec::new()), EXIT);
    assert!(
        find(&plain, &EXIT.to_le_bytes()),
        "a stub whose point names no hold releases it"
    );
}

#[test]
fn a_bci_in_more_ranges_than_a_site_tests_is_refused() {
    assert!(local_handler_candidates_bounded(&[]));
    let four: Vec<(usize, usize, usize, &'static str)> =
        (0..4).map(|i| (0, 10, 20 + i, "")).collect();
    assert!(local_handler_candidates_bounded(&four));
    let mut five = four.clone();
    five.push((5, 8, 30, "java/lang/Error"));
    assert!(
        !local_handler_candidates_bounded(&five),
        "bci 5 lies in five ranges"
    );
    // Disjoint ranges never add up.
    let disjoint: Vec<(usize, usize, usize, &'static str)> =
        (0..8).map(|i| (i * 2, i * 2 + 1, 40, "")).collect();
    assert!(local_handler_candidates_bounded(&disjoint));
}
