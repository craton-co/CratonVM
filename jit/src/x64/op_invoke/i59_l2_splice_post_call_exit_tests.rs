use super::{
    splice_caller_scope_is_transferable, POST_CALL_OWN_POINT_STUB_TAG,
    SPLICE_POST_CALL_STUB_TAG,
};
use crate::deopt::{FrameState, FrameValue, MonitorInfo};

fn caller(locals: Vec<FrameValue>, stack: Vec<FrameValue>) -> FrameState {
    FrameState {
        class_id: 0,
        method_key: "Caller.run:()I".to_string(),
        bci: 7,
        locals,
        stack,
        monitors: Vec::new(),
        caller: None,
    }
}

/// Interpreter round i2 wave 59, lane L2: a caller scope rebuilt at an
/// exit INSIDE a splice may name only what the spliced body cannot have
/// moved -- constants, frame words, callee-saved GPRs.
#[test]
fn a_caller_scope_is_transferable_only_from_homes_the_splice_keeps() {
    let homes = caller(
        vec![
            FrameValue::StackSlotRef(-16),
            FrameValue::Int(3),
            FrameValue::Undefined,
            FrameValue::RegisterRef(3),
            FrameValue::RegisterLong(12),
            FrameValue::Register(15),
        ],
        vec![FrameValue::StackSlotLong(-24), FrameValue::Object(0)],
    );
    assert!(splice_caller_scope_is_transferable(&homes));
    for clobbered in [
        FrameValue::Register(8),
        FrameValue::RegisterRef(9),
        FrameValue::Register(0),
        FrameValue::XmmDouble(1),
        FrameValue::XmmFloat(0),
        FrameValue::Unsupported,
        FrameValue::VirtualObjectRef(4),
    ] {
        let fs = caller(vec![clobbered.clone()], Vec::new());
        assert!(
            !splice_caller_scope_is_transferable(&fs),
            "{clobbered:?} in a local"
        );
        let fs = caller(Vec::new(), vec![clobbered.clone()]);
        assert!(
            !splice_caller_scope_is_transferable(&fs),
            "{clobbered:?} on the stack"
        );
    }
    let mut locked = caller(vec![FrameValue::StackSlotRef(-16)], Vec::new());
    locked.monitors.push(MonitorInfo {
        object: FrameValue::StackSlotRef(-16),
        lock_depth: 1,
        relock: false,
    });
    assert!(
        !splice_caller_scope_is_transferable(&locked),
        "a held monitor"
    );
    // Wave 62: RSI and RDI are callee-saved local homes on Windows only.
    for reg in [6u8, 7] {
        let fs = caller(vec![FrameValue::RegisterLong(reg)], Vec::new());
        assert_eq!(
            splice_caller_scope_is_transferable(&fs),
            cfg!(target_os = "windows"),
            "register {reg}"
        );
    }
    assert_eq!(
        super::splice_caller_scope_refusal(&caller(vec![FrameValue::Register(8)], Vec::new())),
        Some("local 0 = Register(8)".to_string())
    );
}

/// The splice exit's stub tag is its own: tag 13 resolves by bci, 14 by
/// index, and neither is a deopt reason the stub list already uses.
#[test]
fn the_splice_exit_stub_tag_is_distinct() {
    assert_ne!(SPLICE_POST_CALL_STUB_TAG, POST_CALL_OWN_POINT_STUB_TAG);
    assert!(!(2..=12).contains(&SPLICE_POST_CALL_STUB_TAG));
}
