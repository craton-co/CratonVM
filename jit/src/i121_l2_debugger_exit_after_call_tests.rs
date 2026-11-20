use super::*;

#[test]
fn only_a_recorded_call_has_an_exit_door() {
    let mut cm = CompiledMethod::new(ExecutableBuffer::new(64).unwrap());
    assert!(!cm.debugger_exit_after_call(0));
    assert!(!cm.debugger_exit_after_call(7));
    cm.debugger_exit_call_bcis = vec![(3, 6), (7, 10), (12, 15)];
    assert!(cm.debugger_exit_after_call(7));
    assert!(cm.debugger_exit_after_call(12));
    assert!(!cm.debugger_exit_after_call(8));
    assert!(!cm.debugger_exit_after_call(u32::MAX));
    assert_eq!(cm.debugger_exit_bci_after_call(7), Some(10));
    assert_eq!(cm.debugger_exit_bci_after_call(6), None);
}

fn state(bci: u32, locals: usize) -> deopt::FrameState {
    deopt::FrameState {
        class_id: 0,
        method_key: String::new(),
        bci,
        locals: vec![deopt::FrameValue::Undefined; locals],
        stack: Vec::new(),
        monitors: Vec::new(),
        caller: None,
    }
}

fn exit_point(bci: u32, locals: usize, chained: bool) -> deopt::DeoptimizationPoint {
    let reason = deopt::DeoptReason::OsrExit;
    let mut frame_state = state(bci, locals);
    if chained {
        frame_state.caller = Some(Box::new(state(3, 1)));
    }
    deopt::DeoptimizationPoint {
        native_offset: 0,
        bci,
        reason,
        action: deopt::DeoptAction::Reinterpret,
        speculation_id: 0,
        frame_state,
        semantics: deopt::ResumeSemantics::for_reason(reason),
    }
}

/// Interpreter round i6 wave 122, lane L2 (stage 3b, first slice): the
/// write door opens only on a kept single-frame exit whose map describes
/// every local, and names each refusal.
#[test]
fn the_write_door_opens_only_on_a_describing_single_frame_exit() {
    let mut cm = CompiledMethod::new(ExecutableBuffer::new(64).unwrap());
    assert_eq!(cm.debugger_write_door(7, 2), DebuggerWriteDoor::NoExit);
    cm.debugger_exit_call_bcis = vec![(7, 10)];
    assert_eq!(cm.debugger_write_door(7, 2), DebuggerWriteDoor::NoMap { exit_bci: 10 });
    cm.deopt_points = vec![exit_point(10, 1, false)];
    assert_eq!(
        cm.debugger_write_door(7, 2),
        DebuggerWriteDoor::ShortMap {
            exit_bci: 10,
            described: 1
        }
    );
    cm.deopt_points = vec![exit_point(10, 3, false)];
    assert_eq!(cm.debugger_write_door(7, 2), DebuggerWriteDoor::Open { exit_bci: 10 });
    cm._deopt_point_boxes = vec![Box::new(exit_point(10, 3, true))];
    assert_eq!(
        cm.debugger_write_door(7, 2),
        DebuggerWriteDoor::ChainExit { exit_bci: 10 }
    );
    cm._deopt_point_boxes.clear();
    cm.exits_spared_as_obsolete
        .store(true, std::sync::atomic::Ordering::Release);
    assert_eq!(cm.debugger_write_door(7, 2), DebuggerWriteDoor::SparedAsObsolete);
}

/// Interpreter round i6 wave 123, lane L2 (stage 3b, force slice): the
/// verdict says `Leaves` only for an open door on a withdrawn,
/// non-obsolete body whose post-call sites were all written.
#[test]
fn the_force_verdict_leaves_only_for_a_withdrawn_forced_body() {
    let mut cm = CompiledMethod::new(ExecutableBuffer::new(64).unwrap());
    assert_eq!(
        cm.debugger_force_verdict(7, 2, true, false),
        DebuggerForceVerdict::Door(DebuggerWriteDoor::NoExit)
    );
    cm.debugger_exit_call_bcis = vec![(7, 10)];
    cm.deopt_points = vec![exit_point(10, 3, false)];
    assert_eq!(
        cm.debugger_force_verdict(7, 2, false, false),
        DebuggerForceVerdict::NotWithdrawn { exit_bci: 10 }
    );
    assert_eq!(
        cm.debugger_force_verdict(7, 2, true, true),
        DebuggerForceVerdict::Obsolete { exit_bci: 10 }
    );
    assert_eq!(
        cm.debugger_force_verdict(7, 2, true, false),
        DebuggerForceVerdict::SitesNotForced { exit_bci: 10 }
    );
    assert!(!cm.post_call_exits_forced());
    cm.post_call_exits_forced
        .store(true, std::sync::atomic::Ordering::Release);
    assert_eq!(
        cm.debugger_force_verdict(7, 2, true, false),
        DebuggerForceVerdict::Leaves { exit_bci: 10 }
    );
    assert_eq!(cm.debugger_force_verdict(7, 2, true, false).name(), "leaves");
    assert_eq!(
        cm.debugger_force_verdict(8, 2, true, false).name(),
        "refused:no-exit"
    );
}
