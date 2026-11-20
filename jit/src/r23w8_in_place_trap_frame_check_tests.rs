use super::{
    deopt, first_unrebuildable_in_place_trap, in_place_trap_frame_is_plain,
    in_place_typecheck_trap_frames_rebuild, ir_in_place_trap_frame_check_enabled,
    ir_unresolved_typecheck_trap_pcs, CompiledMethod, ExecutableBuffer,
};
use deopt::{DeoptReason, FrameValue};

fn frame(bci: u32, local: FrameValue) -> deopt::FrameState {
    deopt::FrameState {
        class_id: 0,
        method_key: String::new(),
        bci,
        locals: vec![local],
        stack: vec![FrameValue::Object(0x1000)],
        monitors: Vec::new(),
        caller: None,
    }
}

fn point(
    bci: u32,
    reason: DeoptReason,
    fs: deopt::FrameState,
) -> Box<deopt::DeoptimizationPoint> {
    Box::new(deopt::DeoptimizationPoint {
        native_offset: 0,
        bci,
        reason,
        action: deopt::DeoptAction::Reinterpret,
        speculation_id: 0,
        frame_state: fs,
        semantics: deopt::ResumeSemantics::for_reason(reason),
    })
}

fn body(points: Vec<Box<deopt::DeoptimizationPoint>>) -> CompiledMethod {
    let mut buf = ExecutableBuffer::new(64).unwrap();
    buf.emit(&[0xC3]); // ret
    let mut cm = CompiledMethod::new(buf);
    cm._deopt_point_boxes = points;
    cm
}

/// A plain frame at the trap passes; an unresumable or a virtual-object
/// slot there discards the body; the same frame at another pc, under a
/// caller scope, or on a rethrow point is not this check's business.
#[test]
fn only_a_plain_frame_at_an_in_place_trap_survives() {
    let trap = DeoptReason::UncommonTrap;
    let pcs = [7usize];
    let ok = body(vec![point(7, trap, frame(7, FrameValue::Int(3)))]);
    assert!(in_place_typecheck_trap_frames_rebuild(&ok, &pcs));
    let bad = body(vec![point(7, trap, frame(7, FrameValue::Unsupported))]);
    assert!(!in_place_typecheck_trap_frames_rebuild(&bad, &pcs));
    // Wave 8c: a virtual object is materialized by the sink (default), and
    // refused again under `CRATONVM_JIT_IR_IN_PLACE_TRAP_VIRTUALS=0`.
    let virt = body(vec![point(7, trap, frame(7, FrameValue::VirtualObjectRef(0)))]);
    let vkey = concat!("CRATON", "VM_JIT_IR_IN_PLACE_TRAP_VIRTUALS");
    cratonvm_types::flags::with_thread_overrides(&[(vkey, None)], || {
        assert!(in_place_typecheck_trap_frames_rebuild(&virt, &pcs));
    });
    // The diagnostic names the pc and the rule.
    assert_eq!(
        first_unrebuildable_in_place_trap(&bad, &pcs),
        Some((7, "not resumable (local 0)".to_string()))
    );
    // Not a trap pc, or no trap at all: nothing to check.
    let other = body(vec![point(9, trap, frame(9, FrameValue::Unsupported))]);
    assert!(in_place_typecheck_trap_frames_rebuild(&other, &pcs));
    assert!(in_place_typecheck_trap_frames_rebuild(&bad, &[]));
    // A spliced callee's point that happens to share the bci.
    let mut chained = frame(7, FrameValue::Unsupported);
    chained.caller = Some(Box::new(frame(2, FrameValue::Int(0))));
    let spliced = body(vec![point(7, trap, chained)]);
    assert!(in_place_typecheck_trap_frames_rebuild(&spliced, &pcs));
    // A rethrow point is the pad's, not a guard.
    let rethrow = body(vec![point(
        7,
        DeoptReason::PendingException,
        frame(7, FrameValue::Unsupported),
    )]);
    assert!(in_place_typecheck_trap_frames_rebuild(&rethrow, &pcs));
}

#[test]
fn a_monitor_or_caller_scope_is_not_plain() {
    assert!(in_place_trap_frame_is_plain(&frame(1, FrameValue::Object(0))));
    let mut held = frame(1, FrameValue::Int(0));
    held.monitors.push(deopt::MonitorInfo {
        object: FrameValue::Object(0x2000),
        lock_depth: 1,
        relock: false,
    });
    assert!(!in_place_trap_frame_is_plain(&held));
    let mut chained = frame(1, FrameValue::Int(0));
    chained.caller = Some(Box::new(frame(0, FrameValue::Int(0))));
    assert!(!in_place_trap_frame_is_plain(&chained));
}

#[test]
fn the_trap_pcs_are_the_in_place_trap_causes_only() {
    let mut g = crate::ir::Graph::empty(0);
    g.note_trap_cause(4, crate::ir::TrapCause::UnresolvedTypeCheck);
    g.note_trap_cause(9, crate::ir::TrapCause::UnresolvedNew);
    g.note_trap_cause(12, crate::ir::TrapCause::UnresolvedTypeCheck);
    g.note_trap_cause(15, crate::ir::TrapCause::UnresolvedField);
    let key = concat!("CRATON", "VM_JIT_IR_UNRESOLVED_FIELD_IN_PLACE");
    cratonvm_types::flags::with_thread_overrides(&[(key, None)], || {
        assert_eq!(ir_unresolved_typecheck_trap_pcs(&g), vec![4, 12, 15]);
    });
    assert!(ir_unresolved_typecheck_trap_pcs(&crate::ir::Graph::empty(0)).is_empty());
}

#[test]
fn the_check_is_default_on_and_killed_by_zero() {
    let key = concat!("CRATON", "VM_JIT_IR_IN_PLACE_TRAP_FRAME_CHECK");
    cratonvm_types::flags::with_thread_overrides(&[(key, None)], || {
        assert!(ir_in_place_trap_frame_check_enabled());
    });
}

/// Wave 8 (R23-W8-FLIP): the in-place trap itself is default ON now that
/// the frame check above backs it; `=0` restores the refusal.
#[test]
fn the_in_place_typecheck_trap_is_default_on_and_killed_by_zero() {
    let key = concat!("CRATON", "VM_JIT_IR_UNRESOLVED_TYPECHECK_IN_PLACE");
    cratonvm_types::flags::with_thread_overrides(&[(key, None)], || {
        assert!(crate::ir::ir_unresolved_typecheck_in_place_enabled());
    });
}
