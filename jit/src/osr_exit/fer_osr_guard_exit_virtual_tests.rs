use super::*;
use crate::deopt::{DeoptAction, FrameState, FrameValue, VirtualObjectState};


fn boxed(bci: u32, reason: DeoptReason, locals: Vec<FrameValue>) -> Box<DeoptimizationPoint> {
    Box::new(DeoptimizationPoint {
        native_offset: 0x40 + bci,
        bci,
        reason,
        action: DeoptAction::Reinterpret,
        speculation_id: 0,
        frame_state: FrameState {
            class_id: 0,
            method_key: "P.loop:(I)J".to_string(),
            bci,
            locals,
            stack: Vec::new(),
            monitors: Vec::new(),
            caller: None,
        },
        semantics: ResumeSemantics::for_reason(reason),
    })
}

fn iterator() -> FrameValue {
    FrameValue::VirtualObject(VirtualObjectState {
        array_element_type: None,
        id: 1,
        class_id: 232,
        num_fields: 2,
        field_values: vec![FrameValue::Int(3), FrameValue::Int(-1)],        })    }