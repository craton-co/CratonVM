use super::*;
use crate::threading::jvm_thread::CompiledHomeBody;

fn write() -> super::super::CompiledHomeWrite {
    super::super::CompiledHomeWrite {
        body: CompiledHomeBody {
            cm_ptr: 0x5000,
            frame_rbp: 0x7ff0,
        },
        exit_bci: 12,
        class_id: 7,
        method_id: 9,
        slot: 1,
        value: super::super::DeferredLocal::Plain(Value::Int(5)),
    }
}

#[test]
fn a_write_matches_its_own_activation_only() {
    let w = write();
    // The body that left, at the exit bci, on the frame base.
    assert!(compiled_home_write_matches(&w, 0x7ff0, Some(0x5000), 12, 7, 9));
    // Wave 124 (L2, i124-L9): the frame being built is the authority, so
    // another method's frame never takes the write, body named or not.
    assert!(!compiled_home_write_matches(&w, 0x7ff0, Some(0x5000), 12, 0, 0));
    assert!(!compiled_home_write_matches(&w, 0x7ff0, Some(0x5000), 12, 7, 8));
    // Another body, base or bci: not this activation.
    assert!(!compiled_home_write_matches(&w, 0x7ff0, Some(0x5008), 12, 7, 9));
    assert!(!compiled_home_write_matches(&w, 0x7fe0, Some(0x5000), 12, 7, 9));
    assert!(!compiled_home_write_matches(&w, 0x7ff0, Some(0x5000), 13, 7, 9));
    // An unnamed body falls back to the frame's class and method.
    assert!(compiled_home_write_matches(&w, 0x7ff0, None, 12, 7, 9));
    assert!(!compiled_home_write_matches(&w, 0x7ff0, None, 12, 7, 8));
    // No base: never.
    let mut zero = w;
    zero.body.frame_rbp = 0;
    assert!(!compiled_home_write_matches(&zero, 0, Some(0x5000), 12, 7, 9));
}
