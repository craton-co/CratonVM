use super::{
    first_point_needing_unsound_replay_of_method, point_needs_unsound_replay_of_method,
    synchronized_frame_refuses_virtuals, DeoptAction, DeoptReason, DeoptimizationPoint,
    FrameState, FrameValue, ResumeSemantics, SinkMethodMonitor, VirtualObjectState,
};

/// `iconst_1 putstatic #1 ... ` with a guard at bci 7 inside a loop that
/// stores (the wave-4 fixture): a replay from entry commits again.
const STORE_THEN_LOOP: [u8; 23] = [
    0x04, 0xb3, 0x00, 0x01, 0x03, 0x3b, 0x1a, 0x10, 0x0a, 0xa2, 0x00, 0x0d, 0x05, 0xb3, 0x00,
    0x01, 0x84, 0x00, 0x01, 0xa7, 0xff, 0xf3, 0xb1,
];

fn virtual_pt(id: usize) -> FrameValue {
    FrameValue::VirtualObject(VirtualObjectState {
        array_element_type: None,
        id,
        class_id: 7,
        num_fields: 2,
        field_values: vec![FrameValue::Int(1), FrameValue::Int(2)],
    })
}

fn point_with_locals(locals: Vec<FrameValue>) -> DeoptimizationPoint {
    DeoptimizationPoint {
        native_offset: 0,
        bci: 7,
        reason: DeoptReason::ReceiverTypeChanged,
        action: DeoptAction::Reinterpret,
        speculation_id: 0,
        frame_state: FrameState {
            class_id: 0,
            method_key: String::new(),
            bci: 7,
            locals,
            stack: Vec::new(),
            monitors: Vec::new(),
            caller: None,
        },
        semantics: ResumeSemantics::for_reason(DeoptReason::ReceiverTypeChanged),
    }
}

#[test]
fn only_a_virtual_receiver_slot_is_refused() {
    let v = [virtual_pt(0)];
    let obj_then_v = [FrameValue::Object(0x1000), virtual_pt(0)];
    // Not synchronized, or nothing virtual: never refused.
    assert!(!synchronized_frame_refuses_virtuals(
        SinkMethodMonitor::None,
        &v,
        true
    ));
    assert!(!synchronized_frame_refuses_virtuals(
        SinkMethodMonitor::Instance,
        &v,
        false
    ));
    // A static method's monitor is the mirror: a virtual local 0 is not it.
    assert!(!synchronized_frame_refuses_virtuals(
        SinkMethodMonitor::Static,
        &v,
        true
    ));
    // An instance method whose receiver slot is a real object resumes.
    assert!(!synchronized_frame_refuses_virtuals(
        SinkMethodMonitor::Instance,
        &obj_then_v,
        true
    ));
    // Local 0 itself virtual: still refused.
    assert!(synchronized_frame_refuses_virtuals(
        SinkMethodMonitor::Instance,
        &v,
        true
    ));
    assert!(synchronized_frame_refuses_virtuals(
        SinkMethodMonitor::Instance,
        &[FrameValue::VirtualObjectRef(0)],
        true
    ));
}

/// The compile-time check refuses exactly what the builder refuses: before
/// wave 7 it took every virtual-bearing synchronized frame as resumed while
/// the builder refused all of them, and the doors re-ran from entry.
#[test]
fn the_replay_check_models_the_builder_refusal() {
    let code = STORE_THEN_LOOP;
    let len = code.len();
    let receiver_virtual = point_with_locals(vec![virtual_pt(0)]);
    let other_virtual = point_with_locals(vec![FrameValue::Object(0x1000), virtual_pt(0)]);
    assert!(point_needs_unsound_replay_of_method(
        &receiver_virtual,
        &code,
        len,
        true,
        true,
        SinkMethodMonitor::Instance
    ));
    assert!(!point_needs_unsound_replay_of_method(
        &other_virtual,
        &code,
        len,
        true,
        true,
        SinkMethodMonitor::Instance
    ));
    assert!(!point_needs_unsound_replay_of_method(
        &receiver_virtual,
        &code,
        len,
        true,
        true,
        SinkMethodMonitor::Static
    ));
    assert_eq!(
        first_point_needing_unsound_replay_of_method(
            [&other_virtual, &receiver_virtual],
            &code,
            len,
            true,
            true,
            SinkMethodMonitor::Instance
        ),
        Some(7)
    );
    assert_eq!(SinkMethodMonitor::of(true, true), SinkMethodMonitor::Static);
    assert_eq!(
        SinkMethodMonitor::of(true, false),
        SinkMethodMonitor::Instance
    );
    assert_eq!(SinkMethodMonitor::of(false, true), SinkMethodMonitor::None);
}
