#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};
#[test]
fn ccpp_dbg_flag_is_latched_and_matches_environment() {
    // The `@Import` walker used to call `env::var` (which also allocates a
    // `String`) on every missing-class branch. The latched helper must
    // agree with the environment at first use and stay stable.
    let expected = cratonvm_types::flags::runtime_var_os("CCPP_DBG").is_some();
    assert_eq!(super::ccpp_dbg_enabled(), expected);
    assert_eq!(super::ccpp_dbg_enabled(), expected);
}

#[test]
fn ccpp_dbg_flag_is_off_in_a_clean_environment() {
    if cratonvm_types::flags::runtime_var_os("CCPP_DBG").is_none() {
        assert!(!super::ccpp_dbg_enabled());
    }
}
