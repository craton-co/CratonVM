use super::{class_loader_resource_triple, resource_body_declines_native};

/// The decline applies to `java/lang/ClassLoader`'s native and to nothing
/// else, and only when the receiver class's verdict is "JDK body".
#[test]
fn i73_decline_is_scoped_to_class_loader_and_the_verdict() {
    assert!(resource_body_declines_native(true, "java/lang/ClassLoader"));
    assert!(!resource_body_declines_native(false, "java/lang/ClassLoader"));
    assert!(!resource_body_declines_native(true, "java/net/URLClassLoader"));
    assert!(!resource_body_declines_native(true, "java/security/SecureClassLoader"));
    assert!(!resource_body_declines_native(true, "jdk/internal/loader/BuiltinClassLoader"));
    assert!(!resource_body_declines_native(true, "java/lang/Object"));
}

/// The three instance forms and only those: the static
/// `getSystemResource*` forms and the other descriptors keep the native.
#[test]
fn i73_triples_are_the_three_instance_resource_methods() {
    assert!(class_loader_resource_triple("getResource", "(Ljava/lang/String;)Ljava/net/URL;"));
    assert!(class_loader_resource_triple(
        "getResources",
        "(Ljava/lang/String;)Ljava/util/Enumeration;"
    ));
    assert!(class_loader_resource_triple(
        "getResourceAsStream",
        "(Ljava/lang/String;)Ljava/io/InputStream;"
    ));
    assert!(!class_loader_resource_triple(
        "getSystemResource",
        "(Ljava/lang/String;)Ljava/net/URL;"
    ));
    assert!(!class_loader_resource_triple(
        "getSystemResources",
        "(Ljava/lang/String;)Ljava/util/Enumeration;"
    ));
    assert!(!class_loader_resource_triple(
        "getSystemResourceAsStream",
        "(Ljava/lang/String;)Ljava/io/InputStream;"
    ));
    assert!(!class_loader_resource_triple(
        "getResource",
        "(Ljava/lang/String;)Ljava/lang/Object;"
    ));
    assert!(!class_loader_resource_triple(
        "findResource",
        "(Ljava/lang/String;)Ljava/net/URL;"
    ));
}

/// Every place `populate_virtual_invoke_cache` can publish the
/// `java/lang/ClassLoader` resource `Bridge` as a `VirtualNative` asks the
/// decline. Before the i73 closure only the `CRATONVM_ANCESTOR_WALK_HELPER=0`
/// loop did; the default walk and the forced-native block republished the
/// native, so the switch moved nothing and no behaviour probe could tell
/// (the answer is the same either way). The needle is assembled so this
/// test's own text is not what the scan finds.
#[test]
fn i73_every_native_publish_site_asks_the_decline() {
    let src = include_str!("../dispatch_virtual.rs");
    let production = src
        .split("mod i73_user_loader_resource_body_tests")
        .next()
        .unwrap_or(src);
    let needle = format!("{}{}", "resource_body_declines", "_native(");
    // The definition, the helper-walk `accept`, the loop's filter and the
    // forced-native block.
    assert_eq!(
        production.matches(needle.as_str()).count(),
        4,
        "a new VirtualNative publish site for a ClassLoader resource triple must ask the decline"
    );
}
