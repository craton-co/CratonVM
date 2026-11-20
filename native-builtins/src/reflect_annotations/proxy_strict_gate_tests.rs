#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};
/// increment 3 (§3): STRICT proxy mode is opt-in and OFF by default, so the
/// silent-degrade safety net remains the default behaviour until the soak
/// flips it. (The throw path itself requires a live VM and is exercised by
/// the reflection suites under `CRATONVM_REAL_PROXY_STRICT=1`.)
#[test]
fn real_proxy_strict_defaults_off() {
    assert!(!super::real_proxy_strict());
}

/// proxy-real-classfile real-super gate: real `java.lang.reflect.Proxy` is the
/// super by DEFAULT (real Java by default; the synthetic `Proxy$Instance` super
/// is the experimental opt-out via `CRATONVM_REAL_PROXY_SUPER=0`). Both paths
/// are retained — nothing is deleted. (Env-unset default asserted here; the
/// opt-out is exercised by the proxy soak under `CRATONVM_REAL_PROXY_SUPER=0`.)
#[test]
fn real_proxy_super_defaults_on() {
    // Only meaningful when the env var is not set in the test environment.
    if !crate::nbflags().real_proxy_super_set {
        assert!(super::real_proxy_super());
        assert_eq!(super::proxy_super_class_name(), "java/lang/reflect/Proxy");
    }
}

/// Round 13 wave 9 (lane proxy6): the dynamic-module name of a generated
/// all-public proxy is its package (`ProxyBuilder`: `pn = mn`), and only the
/// exact `jdk/proxy<N>/$Proxy<M>` shape has one.
#[test]
fn proxy_dynamic_module_name_reads_only_the_generated_shape() {
    assert_eq!(
        super::proxy_dynamic_module_name("jdk/proxy1/$Proxy0"),
        Some(("jdk.proxy1".to_string(), "jdk/proxy1".to_string()))
    );
    assert_eq!(
        super::proxy_dynamic_module_name("jdk/proxy12/$Proxy345"),
        Some(("jdk.proxy12".to_string(), "jdk/proxy12".to_string()))
    );
    // Round 13 wave 10: the module's non-exported package.
    assert_eq!(
        super::proxy_dynamic_module_name("com/sun/proxy/jdk/proxy3/$Proxy9"),
        Some((
            "jdk.proxy3".to_string(),
            "com/sun/proxy/jdk/proxy3".to_string()
        ))
    );
    for other in [
        "com/sun/proxy/jdk/proxy/$Proxy0",
        "com/sun/proxy/jdk/proxy1/sub/$Proxy0",
        "com/sun/proxy/jdk/proxy1/Settings$ProxyConfig",
        "$Proxy0",
        "p/$Proxy0",
        "jdk/proxy/$Proxy0",
        "jdk/proxyA/$Proxy0",
        "jdk/proxy1/Settings$ProxyConfig",
        "jdk/proxy1/$Proxy",
        "jdk/proxy1/sub/$Proxy0",
        "com/sun/proxy/$Proxy0",
    ] {
        assert_eq!(super::proxy_dynamic_module_name(other), None, "{other}");
    }
}

/// Round 15 wave 5 (lane proxy7): `jdk.proxyN` is numbered per VM, so a
/// second VM's first loader is `jdk.proxy1` again, as in a JVM of its own.
/// The VM ids are this test's own; their rows are purged at the end.
#[test]
fn proxy_module_numbers_are_per_vm() {
    if !super::proxy_module_number_per_vm() {
        return;
    }
    const VM_A: usize = 0x7A0F_0001;
    const VM_B: usize = 0x7A0F_0002;
    assert_eq!(super::proxy_module_number(VM_A, 2), 1);
    assert_eq!(super::proxy_module_number(VM_A, 7), 2);
    assert_eq!(
        super::proxy_module_number(VM_A, 2),
        1,
        "same loader, same module"
    );
    assert_eq!(
        super::proxy_module_number(VM_B, 2),
        1,
        "another VM starts at 1"
    );
    crate::forget_vm_proxy_classes(VM_A);
    crate::forget_vm_proxy_classes(VM_B);
    assert_eq!(
        super::proxy_module_number(VM_B, 9),
        1,
        "a purged VM starts over"
    );
    crate::forget_vm_proxy_classes(VM_B);
}

/// Round 15 wave 5 (lane proxy8, PX7-4): `$ProxyM` is numbered per VM from
/// 0, as `ProxyBuilder.nextUniqueNumber` in a JVM of its own, and a purged
/// VM's row is gone. The VM ids are this test's own.
#[test]
fn proxy_class_numbers_are_per_vm() {
    if !super::proxy_class_number_per_vm() {
        return;
    }
    const VM_A: usize = 0x7A0F_8001;
    const VM_B: usize = 0x7A0F_8002;
    assert_eq!(super::next_proxy_class_number(VM_A), 0);
    assert_eq!(super::next_proxy_class_number(VM_A), 1);
    assert_eq!(
        super::next_proxy_class_number(VM_B),
        0,
        "another VM starts at 0"
    );
    assert_eq!(super::next_proxy_class_number(VM_A), 2);
    crate::forget_vm_proxy_classes(VM_A);
    assert_eq!(
        super::next_proxy_class_number(VM_A),
        0,
        "a purged VM starts over"
    );
    assert_eq!(super::next_proxy_class_number(VM_B), 1);
    crate::forget_vm_proxy_classes(VM_A);
    crate::forget_vm_proxy_classes(VM_B);
}
