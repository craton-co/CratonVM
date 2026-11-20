use super::reflective_name_is_runtime_proxy;

/// The reflective / `MethodHandle` argument screens decline a generated
/// proxy, the synthetic shim and an annotation carrier, and JUDGE a user
/// class whose binary name merely contains `$Proxy`.
#[test]
fn shape_not_substring() {
    if !crate::reflect_annotations::proxy_user_subclass_is_ordinary() {
        return; // CRATONVM_PROXY_USER_SUBCLASS_ORDINARY=0 in this environment
    }
    for name in [
        "jdk/proxy1/$Proxy12",
        "com/sun/proxy/$Proxy0",
        "p/$Proxy3",
        "$Proxy7",
        "java/lang/reflect/Proxy$Instance",
        "java/lang/annotation/AnnotationProxy",
    ] {
        assert!(reflective_name_is_runtime_proxy(name), "{name}");
    }
    for name in [
        "R13Proxy4UserSubclassTypes$ProxyConfig",
        "p/Outer$ProxyFactory",
        "java/lang/reflect/Proxy$ProxyBuilder",
        "p/$Proxy",
        "p/$Proxy3x",
        "java/lang/reflect/Proxy",
        "java/lang/Runnable",
        // JIT round 21, lane p161: a user class ending in the carrier's
        // simple name is an ordinary class.
        "com/acme/MyAnnotationProxy",
        "AnnotationProxy",
    ] {
        assert!(!reflective_name_is_runtime_proxy(name), "{name}");
    }
}
