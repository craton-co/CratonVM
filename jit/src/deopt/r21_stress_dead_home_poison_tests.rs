use super::*;

const POISON: u64 = cratonvm_types::jit_activation::DEAD_HOME_POISON;

fn frame(method: &str, locals: Vec<FrameValue>) -> ReconstructedFrame {
    ReconstructedFrame {
        class_id: 0,
        method_key: method.to_string(),
        bci: 7,
        locals,
        ..ReconstructedFrame::default()
    }
}

#[test]
fn a_frame_without_the_poison_reports_nothing() {
    let f = frame(
        "p/C.m:()V",
        vec![
            FrameValue::Object(0),
            FrameValue::Int(POISON as i64),
            FrameValue::Long(POISON as i64),
        ],
    );
    assert_eq!(
        dead_home_poison_slot(&f),
        None,
        "only a reference reads as the poison"
    );
}

#[test]
fn a_poisoned_reference_local_stack_slot_or_monitor_is_named() {
    let f = frame(
        "p/C.m:()V",
        vec![FrameValue::Object(8), FrameValue::Object(POISON)],
    );
    assert_eq!(
        dead_home_poison_slot(&f),
        Some(("p/C.m:()V".to_string(), 7, "local", 1))
    );
    let mut s = frame("p/C.m:()V", Vec::new());
    s.stack = vec![FrameValue::Object(POISON)];
    assert_eq!(dead_home_poison_slot(&s).map(|h| h.2), Some("stack"));
    let mut m = frame("p/C.m:()V", Vec::new());
    m.monitors = vec![MonitorInfo {
        object: FrameValue::Object(POISON),
        lock_depth: 1,
        relock: false,
    }];
    assert_eq!(dead_home_poison_slot(&m).map(|h| h.2), Some("monitor"));
}

#[test]
fn a_poisoned_field_of_a_virtual_object_or_a_caller_scope_is_named() {
    let vo = FrameValue::VirtualObject(VirtualObjectState {
        array_element_type: None,
        id: 0,
        class_id: 1,
        num_fields: 1,
        field_values: vec![FrameValue::Object(POISON)],
    });
    let f = frame("p/C.m:()V", vec![vo]);
    assert_eq!(dead_home_poison_slot(&f).map(|h| h.2), Some("local"));
    let mut outer = frame("p/C.outer:()V", vec![FrameValue::Object(16)]);
    outer.caller_frames = vec![frame("p/C.caller:()V", vec![FrameValue::Object(POISON)])];
    assert_eq!(
        dead_home_poison_slot(&outer).map(|h| h.0),
        Some("p/C.caller:()V".to_string())
    );
}
