use super::{
    deopt, deopt_real_enabled, deopt_sink_resume_enabled, ir_precise_frames_trap_post_check,
    CompiledMethod, ExecutableBuffer,
};
use deopt::{DeoptReason, FrameValue};

fn point(reason: DeoptReason, local: FrameValue) -> Box<deopt::DeoptimizationPoint> {
    Box::new(deopt::DeoptimizationPoint {
        native_offset: 0,
        bci: 1,
        reason,
        action: deopt::DeoptAction::Reinterpret,
        speculation_id: 0,
        frame_state: deopt::FrameState {
            class_id: 0,
            method_key: String::new(),
            bci: 1,
            locals: vec![local],
            stack: Vec::new(),
            monitors: Vec::new(),
            caller: None,
        },
        semantics: deopt::ResumeSemantics::for_reason(reason),
    })
}

fn body(
    points: Vec<Box<deopt::DeoptimizationPoint>>,
    can_deopt_resume: bool,
) -> CompiledMethod {
    let mut buf = ExecutableBuffer::new(64).unwrap();
    buf.emit(&[0xC3]); // ret
    let mut cm = CompiledMethod::new(buf);
    cm._deopt_point_boxes = points;
    cm.can_deopt_resume = can_deopt_resume;
    cm
}

/// `iconst_1; ireturn` and `aload_0; monitorenter; aload_0; monitorexit;
/// return`.
const PLAIN: &[u8] = &[0x04, 0xac];
const LOCKS: &[u8] = &[0x2a, 0xc2, 0x2a, 0xc3, 0xb1];

#[test]
fn only_bodies_whose_guard_exits_resume_precisely_survive_the_waiver() {
    // A rethrow point is not a guard; its frame is the pad's business.
    let cm = body(
        vec![point(
            DeoptReason::PendingException,
            FrameValue::Unsupported,
        )],
        false,
    );
    assert!(ir_precise_frames_trap_post_check(
        &cm,
        PLAIN,
        PLAIN.len(),
        false
    ));
    // An unresumable guard frame would be replayed from entry: discard.
    let cm = body(
        vec![point(DeoptReason::NullCheck, FrameValue::Unsupported)],
        true,
    );
    assert!(!ir_precise_frames_trap_post_check(
        &cm,
        LOCKS,
        LOCKS.len(),
        false
    ));
    // A monitor body the lowerer did not vouch for: no sink resumes it.
    let cm = body(
        vec![point(DeoptReason::NullCheck, FrameValue::Int(1))],
        false,
    );
    assert!(!ir_precise_frames_trap_post_check(
        &cm,
        LOCKS,
        LOCKS.len(),
        false
    ));
    let cm = body(
        vec![point(DeoptReason::NullCheck, FrameValue::Int(1))],
        true,
    );
    assert_eq!(
        ir_precise_frames_trap_post_check(&cm, LOCKS, LOCKS.len(), false),
        deopt_real_enabled()
    );
    // A lock-free body leans on the default-on sink resume.
    let cm = body(
        vec![point(DeoptReason::NullCheck, FrameValue::Int(1))],
        false,
    );
    assert_eq!(
        ir_precise_frames_trap_post_check(&cm, PLAIN, PLAIN.len(), false),
        deopt_sink_resume_enabled()
    );
}

/// Round 14 wave 1 (lane sync): a SYNCHRONIZED method's guard exits are
/// refused by the sinks' second arm whatever its bytecode, so a lock-free
/// body survives only as a monitor body would.
#[test]
fn a_synchronized_body_leans_only_on_the_vouched_resume() {
    let unvouched = body(
        vec![point(DeoptReason::NullCheck, FrameValue::Int(1))],
        false,
    );
    assert!(!ir_precise_frames_trap_post_check(
        &unvouched,
        PLAIN,
        PLAIN.len(),
        true
    ));
    let vouched = body(
        vec![point(DeoptReason::NullCheck, FrameValue::Int(1))],
        true,
    );
    assert_eq!(
        ir_precise_frames_trap_post_check(&vouched, PLAIN, PLAIN.len(), true),
        deopt_real_enabled()
    );
    // A rethrow point is still not a guard.
    let pad_only = body(
        vec![point(
            DeoptReason::PendingException,
            FrameValue::Unsupported,
        )],
        false,
    );
    assert!(ir_precise_frames_trap_post_check(
        &pad_only,
        PLAIN,
        PLAIN.len(),
        true
    ));
}
