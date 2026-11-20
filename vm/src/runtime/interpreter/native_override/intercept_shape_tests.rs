use super::{
    class_reflection_shape, classloader_resource_shape, file_system_provider_retarget_shape,
    http_carrier_declaring_class, intercept_shape_of, INTERCEPT_SHAPE_CLASSLOADER_RESOURCE,
    INTERCEPT_SHAPE_CLASS_REFLECTION, INTERCEPT_SHAPE_FS_PROVIDER,
    INTERCEPT_SHAPE_HTTP_CARRIER,
};

/// The `FileSystemProvider` re-target is reachable from BOTH twins: every
/// provider class, for `newFileChannel` and the three link operations, sets
/// the bit the cached twin's arm is gated on, and the two interceptors call
/// the one predicate and the one dispatch helper.
#[test]
fn file_system_provider_retarget_is_shared_by_both_twins() {
    let triples = [
        (
            "newFileChannel",
            "(Ljava/nio/file/Path;Ljava/util/Set;[Ljava/nio/file/attribute/FileAttribute;)Ljava/nio/channels/FileChannel;",
        ),
        (
            "createSymbolicLink",
            "(Ljava/nio/file/Path;Ljava/nio/file/Path;[Ljava/nio/file/attribute/FileAttribute;)V",
        ),
        ("createLink", "(Ljava/nio/file/Path;Ljava/nio/file/Path;)V"),
        ("readSymbolicLink", "(Ljava/nio/file/Path;)Ljava/nio/file/Path;"),
    ];
    for class in [
        "java/nio/file/spi/FileSystemProvider",
        "sun/nio/fs/WindowsFileSystemProvider",
        "sun/nio/fs/UnixFileSystemProvider",
    ] {
        for (m, d) in triples {
            assert!(
                file_system_provider_retarget_shape(class, m, d),
                "{class}.{m}{d}"
            );
            assert_ne!(
                intercept_shape_of(class, m, d) & INTERCEPT_SHAPE_FS_PROVIDER,
                0,
                "{class}.{m}{d} must set the FileSystemProvider bit"
            );
        }
    }
    // Another class, or another `newFileChannel` overload, is not re-targeted.
    assert!(!file_system_provider_retarget_shape(
        "com/example/MyProvider",
        triples[0].0,
        triples[0].1
    ));
    assert!(!file_system_provider_retarget_shape(
        "sun/nio/fs/WindowsFileSystemProvider",
        "newFileChannel",
        "(Ljava/nio/file/Path;Ljava/util/Set;)Ljava/nio/channels/FileChannel;"
    ));

    let src = include_str!("../native_override.rs");
    let uncached_start = src
        .find("pub(super) fn intercept_force_registered_native(")
        .expect("the uncached interceptor is gone");
    let cached_start = src
        .find("pub(super) fn intercept_force_registered_native_cached(")
        .expect("the cached interceptor is gone");
    let uncached = &src[uncached_start..cached_start];
    let cached_rest = &src[cached_start..];
    // `\x7d` is `}`: spelled as an escape so the brace-counting panic
    // gates in `interpreter/tests.rs` do not read this literal as the end
    // of the test module and scan the rest of it as production code.
    let cached = &cached_rest[..cached_rest
        .find("\n\x7d\n")
        .expect("the cached interceptor has no end")];
    assert!(uncached.contains("file_system_provider_retarget_shape("));
    assert!(uncached.contains("intercept_file_system_provider_retarget("));
    assert!(cached.contains("INTERCEPT_SHAPE_FS_PROVIDER"));
    assert!(cached.contains("intercept_file_system_provider_retarget("));
}

/// Every triple that any of the three arms can fire on MUST set its bit.
///
/// This is the guard, and the failure it guards against is not a slow path:
/// `intercept_force_registered_native_cached` skips an arm outright when the
/// bit is clear, so an arm that grows a name its classifier does not know
/// stops running. Both halves are derived from the same two helpers here, so
/// the test is only meaningful together with
/// `the_arms_use_the_shared_helpers` below — which is what pins that the
/// dispatch code and the classifier read the SAME predicate rather than two
/// copies that can drift.
#[test]
fn intercept_shape_agrees_with_the_arms_it_gates() {
    let classloader = [
        ("getResource", "(Ljava/lang/String;)Ljava/net/URL;"),
        (
            "getResources",
            "(Ljava/lang/String;)Ljava/util/Enumeration;",
        ),
        (
            "getResourceAsStream",
            "(Ljava/lang/String;)Ljava/io/InputStream;",
        ),
    ];
    for (m, d) in classloader {
        assert!(classloader_resource_shape(m, d), "{m}{d}");
        let shape = intercept_shape_of("some/user/Loader", m, d);
        assert_ne!(
            shape & INTERCEPT_SHAPE_CLASSLOADER_RESOURCE,
            0,
            "{m}{d} must set the ClassLoader bit"
        );
    }

    let reflection = [
        ("getProtectionDomain", "()Ljava/security/ProtectionDomain;"),
        ("isArray", "()Z"),
        ("getComponentType", "()Ljava/lang/Class;"),
        ("componentType", "()Ljava/lang/Class;"),
    ];
    for (m, d) in reflection {
        assert!(class_reflection_shape(m, d), "{m}{d}");
        let shape = intercept_shape_of("java/lang/Class", m, d);
        assert_ne!(
            shape & INTERCEPT_SHAPE_CLASS_REFLECTION,
            0,
            "{m}{d} must set the reflection bit"
        );
    }

    let carriers = [
        "java/net/URLConnection",
        "java/net/HttpURLConnection",
        "javax/net/ssl/HttpsURLConnection",
        "sun/net/www/protocol/http/HttpURLConnection",
        "sun/net/www/protocol/https/HttpsURLConnectionImpl",
    ];
    for c in carriers {
        assert!(http_carrier_declaring_class(c), "{c}");
        let shape = intercept_shape_of(c, "getResponseCode", "()I");
        assert_ne!(
            shape & INTERCEPT_SHAPE_HTTP_CARRIER,
            0,
            "{c} must set the carrier bit"
        );
    }
}

/// The shape of an ordinary call site is ZERO — which is the whole point:
/// that is the case the fast path exists for, and if some innocuous triple
/// set a bit the memo would buy nothing.
///
/// `int callee(int)` is `probes/InvokeAttributionProbe.java`'s own callee,
/// i.e. the exact shape the 1.50% measurement was taken on.
#[test]
fn an_ordinary_call_site_reaches_none_of_the_arms() {
    for (c, m, d) in [
        ("InvokeAttributionProbe", "callee", "(I)I"),
        ("java/lang/String", "length", "()I"),
        ("java/util/ArrayList", "add", "(Ljava/lang/Object;)Z"),
        (
            "org/apache/tomcat/util/bcel/classfile/ConstantPool",
            "getConstant",
            "(ILjava/lang/Class;)Lorg/apache/tomcat/util/bcel/classfile/Constant;",
        ),
        // Right names, WRONG owner: the reflection arm is keyed on the
        // method pair alone, so this one legitimately sets a bit and the
        // receiver check below it is what declines. Recorded so a future
        // reader does not "tighten" the classifier by adding an owner test
        // the arm itself does not make.
    ] {
        assert_eq!(
            intercept_shape_of(c, m, d),
            0,
            "{c}.{m}{d} must not reach any special-case arm"
        );
    }
}

/// The UNCACHED interceptor tests the same helpers the cached twin's memo is
/// built from, rather than literal copies of their name lists.
///
/// `intercept_shape_agrees_with_the_arms_it_gates` above says it is "only
/// meaningful together with" this test; for a while that test did not
/// exist, and the uncached arms carried their own literal `matches!` copies
/// of the ClassLoader-resource and Class-reflection lists. Pinned here so a
/// literal copy cannot grow back and drift from the classifier.
#[test]
fn the_arms_use_the_shared_helpers() {
    let src = include_str!("../native_override.rs");
    let start = src
        .find("pub(super) fn intercept_force_registered_native(")
        .expect("the uncached interceptor is gone, so this scan reads the wrong text");
    let rest = &src[start..];
    let end = rest
        .find("pub(super) fn intercept_force_registered_native_cached(")
        .expect("the cached interceptor no longer follows the uncached one");
    let body = &rest[..end];
    for helper in [
        "classloader_resource_shape(",
        "class_reflection_shape(",
        "real_http_url_connection_native(",
    ] {
        assert!(
            body.contains(helper),
            "intercept_force_registered_native no longer calls `{helper}..)`; its arm \
             and the cached twin's shape memo can now disagree"
        );
    }
    for literal in ["\"getResourceAsStream\"", "\"getProtectionDomain\""] {
        assert!(
            !body.contains(literal),
            "intercept_force_registered_native spells {literal} out again instead of \
             asking the shared helper"
        );
    }
}

/// A near miss on each list must NOT set the bit — the classifier has to be
/// exact, because a false positive silently reintroduces the per-call string
/// work this change removed.
#[test]
fn near_misses_do_not_set_a_bit() {
    assert!(!classloader_resource_shape(
        "getResource",
        "(Ljava/lang/String;)Ljava/io/InputStream;"
    ));
    assert!(!class_reflection_shape("isArray", "()Ljava/lang/Class;"));
    assert!(!http_carrier_declaring_class(
        "org/example/MyHttpURLConnection"
    ));
    assert!(!http_carrier_declaring_class("java/net/URL"));
}
