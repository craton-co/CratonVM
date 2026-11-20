use cratonvm_native_api::NativeCallSiteExt;
use super::*;

fn jdk_only_vm() -> SharedVm {
    let mut config = crate::config::VmConfig::default();
    config.compatibility_mode = cratonvm_types::compat::CompatibilityMode::JdkOnly;
    // `JdkOnly` with the synthetic library is a rejected pair.
    config.use_synthetic_jdk = false;
    SharedVm::new(config)
}

/// The macro numbers the gates densely, in declaration order, and the
/// count sizes the per-VM array exactly.
#[test]
fn nio21_w8_gate_ordinals_are_dense_and_in_declaration_order() {
    assert_eq!(ALL_DIRECT_HELPER_GATES.len(), DIRECT_HELPER_GATE_COUNT);
    for (i, (name, gate)) in ALL_DIRECT_HELPER_GATES.iter().enumerate() {
        assert_eq!(gate.ordinal, i, "{name}");
    }
}

/// VM A in `--real-jdk` and VM B in `--compatible`, in one process: every
/// gate has a distinct memo per VM, resolving in A leaves B's memo cold,
/// and resolving in B does not evict A's (the thrash the shared static
/// memo had: one word, re-filled by whichever VM asked last).
#[test]
fn nio21_w8_two_vms_in_two_modes_do_not_share_a_gate_memo() {
    let a = jdk_only_vm();
    let b = SharedVm::new(crate::config::VmConfig::default());
    assert!(crate::vm::dispatch_policy(&a).is_jdk_only());
    assert!(!crate::vm::dispatch_policy(&b).is_jdk_only());
    for (name, gate) in ALL_DIRECT_HELPER_GATES {
        assert!(
            !std::ptr::eq(gate.site(&a), gate.site(&b)),
            "{name}: the two VMs share a memo"
        );
    }
    let ra = &a.natives.native_methods;
    let rb = &b.natives.native_methods;
    for gate in [
        &MD_UPDATE_BYTE_GATE,
        &UNSAFE_CAS_INT_GATE,
        &THREAD_CURRENT_THREAD_GATE,
    ] {
        let (c, m, _) = gate.triple();
        assert!(!gate.site(&a).is_warm(ra), "{c}.{m}: A warm before any ask");
        assert!(!gate.site(&b).is_warm(rb), "{c}.{m}: B warm before any ask");
        let _ = direct_helper_tail_would_refuse(&a, gate);
        assert!(
            gate.site(&a).is_warm(ra),
            "{c}.{m}: A's ask did not fill A's memo"
        );
        assert!(
            !gate.site(&b).is_warm(rb),
            "{c}.{m}: A's ask warmed B's memo"
        );
        let _ = direct_helper_tail_would_refuse(&b, gate);
        assert!(
            gate.site(&b).is_warm(rb),
            "{c}.{m}: B's ask did not fill B's memo"
        );
        assert!(
            gate.site(&a).is_warm(ra),
            "{c}.{m}: B's ask evicted A's memo"
        );
    }
}

/// The bypass-marking latch is per VM: marking A does not latch B.
#[test]
fn nio21_w8_the_bypass_marking_latch_is_per_vm() {
    let a = jdk_only_vm();
    let b = SharedVm::new(crate::config::VmConfig::default());
    let ga = a.natives.native_methods.generation();
    let gb = b.natives.native_methods.generation();
    assert!(!a.jit.direct_helper_gates.helper_natives_marked_at(ga));
    mark_direct_call_helper_natives_incomplete(&a);
    assert!(a.jit.direct_helper_gates.helper_natives_marked_at(ga));
    assert!(!b.jit.direct_helper_gates.helper_natives_marked_at(gb));
    mark_direct_call_helper_natives_incomplete(&b);
    assert!(b.jit.direct_helper_gates.helper_natives_marked_at(gb));
    assert!(a.jit.direct_helper_gates.helper_natives_marked_at(ga));
}
