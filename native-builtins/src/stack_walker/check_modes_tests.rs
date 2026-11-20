use super::*;
use crate::test_utils::MockNativeContext;
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};

#[test]
fn validate_accepts_default() {
    assert!(validate_stack_walk_modes(0x0));
}

#[test]
fn validate_accepts_known_bits() {
    // RETAIN_CLASS_REFERENCE | SHOW_HIDDEN_FRAMES
    assert!(validate_stack_walk_modes(0x1 | 0x20));
    // FILL_CLASS_REFS_ONLY | GET_CALLER_CLASS
    assert!(validate_stack_walk_modes(0x2 | 0x4));
    // LOCALS_AND_OPERANDS | RETAIN_CLASS_REFERENCE — JDK considers
    // this consistent (the LOCALS variant implies retaining refs).
    assert!(validate_stack_walk_modes(0x100 | 0x1));
}

#[test]
fn validate_rejects_unknown_bits() {
    // 0x800 is not in the recognised mask.
    assert!(!validate_stack_walk_modes(0x800));
    assert!(!validate_stack_walk_modes(0x1 | 0x80000000_u32 as i32));
}

#[test]
fn native_with_null_receiver_returns_true() {
    let mut ctx = MockNativeContext::new();
    let result = native_check_stack_walk_modes(&mut ctx, &[Value::Object(None)])
        .expect("native should not error")
        .expect("should return Some(Value)");
    assert_eq!(result, Value::Int(1));
}

#[test]
fn register_includes_check_stack_walk_modes() {
    use cratonvm_native_api::NativeMethodRegistry;
    let mut r = NativeMethodRegistry::new();
    register_stack_walker_boot(&mut r);
    // Sanity: the registry now contains both the StackWalker entries
    // (5+ from the existing test) plus the new AbstractStackWalker
    // entry, so we expect at least 6 registrations.
    assert!(r.len() >= 6);
}
