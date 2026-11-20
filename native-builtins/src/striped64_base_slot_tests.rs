use super::striped64_base_slot_from_layout;
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};

#[test]
fn real_jdk_inherited_base_slot_wins_over_synthetic_fallback() {
    assert_eq!(striped64_base_slot_from_layout(Some(1), 3), Some(1));
}

#[test]
fn one_field_synthetic_adder_keeps_slot_zero_fallback() {
    assert_eq!(striped64_base_slot_from_layout(None, 1), Some(0));
    assert_eq!(striped64_base_slot_from_layout(None, 0), None);
}
