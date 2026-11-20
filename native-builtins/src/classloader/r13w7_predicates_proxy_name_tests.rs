use super::is_generated_proxy_name;

/// A package-private interface's proxy (`p/$Proxy3`) is a generated proxy
/// name too; a user nested class whose name merely contains `$Proxy` is not.
#[test]
fn package_private_proxy_is_recognised() {
    if !crate::reflect_annotations::proxy_user_subclass_is_ordinary() {
        return; // CRATONVM_PROXY_USER_SUBCLASS_ORDINARY=0 in this environment
    }
    for name in [
        "jdk/proxy1/$Proxy12",
        "com/sun/proxy/$Proxy0",
        "p/$Proxy3",
        "$Proxy4",
    ] {
        assert!(is_generated_proxy_name(name), "{name}");
    }
    for name in [
        "jdk/proxy1/Outer$ProxyConfig",
        "p/Outer$Proxy3",
        "java/lang/reflect/Proxy$Instance",
        "jdk.proxy1.$Proxy12",
    ] {
        assert!(!is_generated_proxy_name(name), "{name}");
    }
}
