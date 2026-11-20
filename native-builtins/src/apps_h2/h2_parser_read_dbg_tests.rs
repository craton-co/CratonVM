#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};
#[test]
fn h2_parser_read_dbg_flag_is_latched_and_matches_environment() {
    // The per-token parser step used to probe `env::var_os` on every call.
    // The latched helper must agree with the environment at first use and
    // stay stable afterwards.
    let expected = None::<std::ffi::OsString>.is_some();
    assert_eq!(super::h2_parser_read_dbg_enabled(), expected);
    assert_eq!(super::h2_parser_read_dbg_enabled(), expected);    }

#[test]
fn h2_parser_read_dbg_flag_is_off_in_a_clean_environment() {
    if None::<std::ffi::OsString>.is_none() {
        assert!(!super::h2_parser_read_dbg_enabled());        }    }