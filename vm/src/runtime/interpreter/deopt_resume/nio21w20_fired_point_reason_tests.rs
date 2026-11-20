use super::{nio21_charge_reason_from_fired_point, nio21_fired_point_reason};
use cratonvm_jit::deopt::{
    DeoptAction, DeoptReason, DeoptimizationPoint, FrameState, ResumeSemantics,
};

fn point(bci: u32, reason: DeoptReason) -> DeoptimizationPoint {
    DeoptimizationPoint {
        native_offset: 0,
        bci,
        reason,
        action: DeoptAction::Reinterpret,
        semantics: ResumeSemantics::REEXECUTE,
        speculation_id: 0,
        frame_state: FrameState {
            class_id: 0,
            method_key: String::new(),
            bci,
            locals: Vec::new(),
            stack: Vec::new(),
            monitors: Vec::new(),
            caller: None,
        },
    }
}

fn artifact() -> cratonvm_jit::CompiledMethod {
    let mut buf = cratonvm_jit::ExecutableBuffer::new(64).expect("alloc executable");
    buf.emit(&[0xC3]); // ret; metadata only
    let mut cm = cratonvm_jit::CompiledMethod::new(buf);
    cm.deopt_points
        .push(point(7, DeoptReason::TransferToInterpreter));
    cm._deopt_point_boxes
        .push(Box::new(point(7, DeoptReason::ReceiverTypeChanged)));
    cm
}

#[test]
fn nio21w20_the_fired_point_is_found_by_address_in_both_homes() {
    let cm = artifact();
    let boxed = &*cm._deopt_point_boxes[0] as *const DeoptimizationPoint as usize;
    let table = &cm.deopt_points[0] as *const DeoptimizationPoint as usize;
    assert_eq!(
        nio21_fired_point_reason(&cm, boxed),
        Some(DeoptReason::ReceiverTypeChanged)
    );
    assert_eq!(
        nio21_fired_point_reason(&cm, table),
        Some(DeoptReason::TransferToInterpreter)
    );
    assert_eq!(nio21_fired_point_reason(&cm, 0), None);
    assert_eq!(nio21_fired_point_reason(&cm, boxed + 8), None);    }