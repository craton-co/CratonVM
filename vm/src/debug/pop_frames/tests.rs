use super::*;

fn frame(descriptor: &str, locals: &[Value]) -> Frame {
    let mut f = Frame::new(
        crate::classloading::ClassId::new(0),
        "T".to_string(),
        "m".to_string(),
        descriptor.to_string(),
        None,
        vec![0xb1],
        vec![],
        8,
        8,
        &[],
    );
    let mut slot = 0u16;
    for &v in locals {
        f.set_local(slot, v);
        slot += if matches!(v, Value::Long(_) | Value::Double(_)) {
            2
        } else {
            1
        };
    }
    f
}

/// The arguments come back in order, of their declared kinds, with the
/// two-slot kinds skipping their second slot.
#[test]
fn a_popped_frames_arguments_are_read_by_its_descriptor() {
    let f = frame(
        "(IJ[Ljava/lang/String;DZ)V",
        &[
            Value::Int(3),
            Value::Long(-4),
            Value::Object(None),
            Value::Double(2.5),
            Value::Int(1),
        ],
    );
    assert_eq!(
        callee_arguments(&f, true),
        Some(vec![
            Value::Int(3),
            Value::Long(-4),
            Value::Object(None),
            Value::Double(2.5),
            Value::Int(1),
        ])
    );
    // A static method of no parameter.
    assert_eq!(callee_arguments(&frame("()V", &[]), true), Some(Vec::new()));
    // A slot holding another kind than the descriptor's.
    let changed = frame("(I)V", &[Value::Long(9)]);
    assert_eq!(callee_arguments(&changed, true), None);
    // An instance method whose receiver slot holds null.
    let null_receiver = frame("(I)V", &[Value::Object(None), Value::Int(1)]);
    assert_eq!(callee_arguments(&null_receiver, false), None);
}

/// Interpreter round i6 wave 134, lane L1 (i133-L1): a blocked thread's
/// `PopFrame` pops its top frame into the one under it; a lone frame has
/// no caller (`NO_MORE_FRAMES`), no frame at all is `OPAQUE_FRAME`.
#[test]
fn a_blocked_pop_names_the_top_frame_and_its_caller() {
    assert_eq!(blocked_pop_pair(3), Ok((2, 1)));
    assert_eq!(blocked_pop_pair(2), Ok((1, 0)));
    assert_eq!(blocked_pop_pair(1), Err(ERR_NO_MORE_FRAMES));
    assert_eq!(blocked_pop_pair(0), Err(ERR_OPAQUE_FRAME));
}
