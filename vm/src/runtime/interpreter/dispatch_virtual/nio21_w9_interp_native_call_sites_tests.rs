use super::*;
use crate::vm::realms::native_realm::{InterpNativeSite, INTERP_NATIVE_SITE_COUNT};

fn jdk_only_vm() -> SharedVm {
    let mut config = crate::config::VmConfig::default();
    config.compatibility_mode = cratonvm_types::compat::CompatibilityMode::JdkOnly;
    // `JdkOnly` with the synthetic library is a rejected pair.
    config.use_synthetic_jdk = false;
    SharedVm::new(config)
}

/// The ordinals are dense and the count sizes the per-VM array exactly.
#[test]
fn nio21_w9_interp_site_ordinals_are_dense() {
    assert_eq!(InterpNativeSite::ALL.len(), INTERP_NATIVE_SITE_COUNT);
    for (i, site) in InterpNativeSite::ALL.iter().enumerate() {
        assert_eq!(*site as usize, i, "{site:?}");
    }
}

/// VM A in `--real-jdk` and VM B in `--compatible`, in one process: every
/// interpreter site has a distinct memo per VM, resolving through the real
/// call in A leaves B's memo cold, and resolving in B does not evict A's
/// (the thrash the shared `static NCS_*` cells had).
#[test]
fn nio21_w9_two_vms_in_two_modes_do_not_share_an_interpreter_ncs_memo() {
    let a = jdk_only_vm();
    let b = SharedVm::new(crate::config::VmConfig::default());
    assert!(crate::vm::dispatch_policy(&a).is_jdk_only());
    assert!(!crate::vm::dispatch_policy(&b).is_jdk_only());
    for site in InterpNativeSite::ALL {
        assert!(
            !std::ptr::eq(
                a.natives.interp_call_sites.site(site),
                b.natives.interp_call_sites.site(site)
            ),
            "{site:?}: the two VMs share a memo"
        );
    }
    let ra = &a.natives.native_methods;
    let rb = &b.natives.native_methods;
    let rows = [
        (
            InterpNativeSite::DispatchMethodInvoke,
            "java/lang/reflect/Method",
            "invoke",
            "(Ljava/lang/Object;[Ljava/lang/Object;)Ljava/lang/Object;",
        ),
        (
            InterpNativeSite::DispatchConstructorNewInstance,
            "java/lang/reflect/Constructor",
            "newInstance",
            "([Ljava/lang/Object;)Ljava/lang/Object;",
        ),
    ];
    for (site, c, m, d) in rows {
        let sa = a.natives.interp_call_sites.site(site);
        let sb = b.natives.interp_call_sites.site(site);
        assert!(!sa.is_warm(ra), "{site:?}: A warm before any ask");
        assert!(!sb.is_warm(rb), "{site:?}: B warm before any ask");
        let _ = native_override_for_cached_reflect_invoke(&a, c, m, d);
        assert!(sa.is_warm(ra), "{site:?}: A's ask did not fill A's memo");
        assert!(!sb.is_warm(rb), "{site:?}: A's ask warmed B's memo");
        let _ = native_override_for_cached_reflect_invoke(&b, c, m, d);
        assert!(sb.is_warm(rb), "{site:?}: B's ask did not fill B's memo");
        assert!(sa.is_warm(ra), "{site:?}: B's ask evicted A's memo");
    }
    // Application workarounds use the registry directly rather than a memo.
    let (c, m, d) = (
        "org/apache/maven/surefire/junitplatform/LazyLauncher",
        "discover",
        "(Lorg/junit/platform/launcher/LauncherDiscoveryRequest;)Lorg/junit/platform/launcher/TestPlan;",
    );
    for vm in [&a, &b] {
        let registry = &vm.natives.native_methods;
        assert!(registry.has_app_workaround(
            cratonvm_native_api::AppWorkaroundKind::ReceiverSubtypeNativeRetarget,
            c,
            m,
            d,
        ));
        assert_eq!(
            native_override_for_cached_reflect_invoke(vm, c, m, d).map(|f| f as usize),
            registry.find(c, m, d).map(|f| f as usize),
        );
        assert!(!vm
            .natives
            .interp_call_sites
            .site(InterpNativeSite::DispatchLazyLauncherDiscover)
            .is_warm(registry));
    }
    // The other sites were never asked in B: still cold.
    for site in &InterpNativeSite::ALL[2..] {
        assert!(
            !b.natives.interp_call_sites.site(*site).is_warm(rb),
            "{site:?}"
        );
    }
}
