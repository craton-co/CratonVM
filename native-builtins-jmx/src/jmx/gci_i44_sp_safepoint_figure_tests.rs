use super::*;

/// No accounting (the trait default, or `CRATONVM_JMX_SAFEPOINT_COUNTERS=0`
/// in the VM's impl) answers 0, the pre-i44 value; a figure is picked by
/// position and saturated into a `jlong`.
#[test]
fn gci_i44_sp_safepoint_figure_picks_and_saturates() {
    assert_eq!(safepoint_figure_of(None, |s| s.0), 0);
    let s = Some((7, 11, 3, 29));
    assert_eq!(safepoint_figure_of(s, |s| s.0), 7);
    assert_eq!(safepoint_figure_of(s, |s| s.1), 11);
    assert_eq!(safepoint_figure_of(s, |s| s.2), 3);
    assert_eq!(safepoint_figure_of(s, |s| s.3), 29);
    assert_eq!(safepoint_figure_of(Some((u64::MAX, 0, 0, 0)), |s| s.0), i64::MAX);
}

/// The four natives left the FLAGGED-0 batch and are registered once each
/// through `safepoint_figure`, still as `Bridge` (the registry census must not
/// move).
#[test]
fn gci_i44_sp_the_safepoint_natives_read_the_barrier() {
    let src = concat!(include_str!("../jmx/mod.rs"), "\n", include_str!("../jmx/jmx_ioex.rs"), "\n", include_str!("../jmx/register_garbage_collector_impl.rs"), "\n", include_str!("../jmx/alloc_basic_thread_info.rs")).replace("\r\n", "\n");
    let start = src
        .find("\npub fn register_vm_management_impl(")
        .expect("register_vm_management_impl");
    let body = &src[start..];
    let body = &body[..body.find("\n\u{7d}\n").expect("its end")];
    for name in [
        "getSafepointCount",
        "getTotalSafepointTime",
        "getSafepointSyncTime",
        "getTotalApplicationNonStoppedTime",
    ] {
        let quoted = format!("\"{name}\",");
        assert_eq!(body.matches(quoted.as_str()).count(), 1, "{name} registered once");
        let at = body.find(quoted.as_str()).expect(name);
        let rest = &body[at..];
        let reg = &rest[..rest.find(");").expect("the call's end")];
        assert!(reg.contains("safepoint_figure("), "{name} reads the barrier");
        assert!(reg.contains("NativeKind::Bridge"), "{name} stays a Bridge");
    }
}
