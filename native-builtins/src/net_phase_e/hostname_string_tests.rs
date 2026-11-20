#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};
#[test]
fn hostname_string_agrees_with_the_crate_wide_resolver() {
    // Two registrations of `InetAddress.getLocalHost()` exist (this phase-E
    // one and `net_uri_inet`'s). They must not disagree about the machine
    // name depending on which registered last.
    assert_eq!(super::hostname_string(), crate::resolve_real_hostname());
}

#[test]
fn hostname_string_is_non_empty_and_stable() {
    let first = super::hostname_string();
    assert!(!first.is_empty(), "hostname must never be empty");
    assert_eq!(
        first,
        super::hostname_string(),
        "must be stable across calls"
    );
}
