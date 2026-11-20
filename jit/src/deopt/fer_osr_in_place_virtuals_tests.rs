use super::{
    frame_state_names_virtual, osr_in_place_virtuals_refusal_with, FrameState, FrameValue,
    MonitorInfo, VirtualObjectState,
};

fn scope(locals: Vec<FrameValue>, caller: Option<FrameState>) -> FrameState {
    FrameState {
        class_id: 0,
        method_key: "T.m:()V".to_string(),
        bci: 7,
        locals,
        stack: Vec::new(),
        monitors: Vec::new(),
        caller: caller.map(Box::new),
    }
}

fn vobj(id: usize, fields: Vec<FrameValue>) -> FrameValue {
    FrameValue::VirtualObject(VirtualObjectState {
        array_element_type: None,
        id,
        class_id: 9,
        num_fields: fields.len(),
        field_values: fields,
    })
}



#[test]
fn a_flat_or_chain_frame_naming_a_well_formed_object_is_admitted_with_the_switch() {
    let flat = scope(
        vec![FrameValue::Int(1), vobj(3, vec![FrameValue::Int(5)])],
        None,
    );
    assert!(frame_state_names_virtual(&flat));
    assert_eq!(osr_in_place_virtuals_refusal_with(&flat, || true), None);
    assert!(osr_in_place_virtuals_refusal_with(&flat, || false).is_some());
    // Defined in the caller scope, referenced by the spliced callee.
    let chain = scope(
        vec![FrameValue::VirtualObjectRef(3)],
        Some(scope(vec![vobj(3, vec![FrameValue::Int(5)])], None)),
    );
    assert!(frame_state_names_virtual(&chain));
    assert_eq!(osr_in_place_virtuals_refusal_with(&chain, || true), None);
}

#[test]
fn a_frame_naming_no_object_never_reads_the_switch() {
    let plain = scope(
        vec![FrameValue::Int(1)],
        Some(scope(vec![FrameValue::Object(0x1000)], None)),
    );
    assert!(!frame_state_names_virtual(&plain));
    assert_eq!(
        osr_in_place_virtuals_refusal_with(&plain, || panic!("the switch was read")),
        None
    );
}

#[test]
fn a_monitor_a_bad_field_or_a_broken_graph_refuses() {
    let on = || true;
    let mut locked = scope(vec![vobj(3, Vec::new())], None);
    locked.monitors.push(MonitorInfo {
        object: FrameValue::StackSlotRef(-24),
        lock_depth: 1,
        relock: false,
    });
    assert!(osr_in_place_virtuals_refusal_with(&locked, on).is_some());
    let bad_field = scope(vec![vobj(3, vec![FrameValue::Unsupported])], None);
    assert!(osr_in_place_virtuals_refusal_with(&bad_field, on).is_some());
    let dangling = scope(vec![FrameValue::VirtualObjectRef(4)], None);
    assert!(osr_in_place_virtuals_refusal_with(&dangling, on).is_some());
    let twice = scope(vec![vobj(3, Vec::new()), vobj(3, Vec::new())], None);
    assert!(osr_in_place_virtuals_refusal_with(&twice, on).is_some());
}
