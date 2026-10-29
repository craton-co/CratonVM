// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Tests for fast-boot compatible mode un-retired startup bridges.
//!
//! Verifies that in `--compatible` mode over a real JDK, the high-frequency
//! startup bridges:
//! 2. `Parameter.getParameterAnnotations` / `Method` / `Constructor`
//!
//! are promoted to `NativeKind::Intrinsic` so they beat real JDK bytecode in
//! `resolve_dispatch`, recovering sub-second cold boot times.
//!
//! Under `--jdk-only`, strict HotSpot semantics are preserved byte-for-byte.
//!
//! Group 3 (`Method.invoke` / `Constructor.newInstance`) was withdrawn: its
//! native skipped the accessor's `asType` adapter frame and the direct
//! handle's customization, which `SHOW_HIDDEN_FRAMES` walks see
//! (`R22FlipHiddenFrames`, `R22RuntimeCtorCustomize`). The JDK's bodies run.

use cratonvm_native_api::{NativeCallback, NativeContext, NativeKind, NativeMethodRegistry};
use cratonvm_types::compat::CompatibilityMode;
use cratonvm_types::error::MethodCallResult;
use cratonvm_types::Value;

fn dummy_cb(_ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    Ok(Some(Value::Int(42)))
}

fn register_bridge(
    reg: &mut NativeMethodRegistry,
    class: &str,
    name: &str,
    descriptor: &str,
    cb: NativeCallback,
) {
    reg.with_category(NativeKind::Bridge, |r| {
        r.register(class, name, descriptor, cb);
    });
}

/// Group 1 (`javax/crypto/Cipher`) was withdrawn in j27 wave 9: the retired
/// native `Cipher` answered 13 `SbrW1Jca` rows off HotSpot 25 when promoted.
/// Under `Compatible` it must never beat the JDK's body.
#[test]
fn fast_boot_cipher_is_not_promoted_in_compatible_mode() {
    let mut reg = NativeMethodRegistry::new();
    reg.set_real_jdk(true);

    let class = "javax/crypto/Cipher";
    let method = "getInstance";
    let desc = "(Ljava/lang/String;)Ljavax/crypto/Cipher;";

    register_bridge(&mut reg, class, method, desc, dummy_cb);

    assert!(!cratonvm_native_api::retired_shadow::is_compat_fast_boot_intrinsic(
        class, method, desc
    ));
    assert_ne!(
        reg.resolve_id(class, method, desc).and_then(|id| reg.kind_of_id(id)),
        Some(NativeKind::Intrinsic),
        "Cipher.getInstance must not be promoted to Intrinsic under Compatible"
    );
}

#[test]
fn fast_boot_cipher_dropped_or_strict_in_jdk_only_mode() {
    let mut reg = NativeMethodRegistry::new();
    reg.set_compatibility_mode(CompatibilityMode::JdkOnly);
    reg.set_real_jdk(true);

    let class = "javax/crypto/Cipher";
    let method = "getInstance";
    let desc = "(Ljava/lang/String;)Ljavax/crypto/Cipher;";

    register_bridge(&mut reg, class, method, desc, dummy_cb);

    // In JdkOnly mode over real JDK, Cipher is a retired shadow and thus dropped:
    assert!(
        reg.resolve_id(class, method, desc).is_none(),
        "Cipher.getInstance must not be registered under JdkOnly"
    );
}

#[test]
fn fast_boot_get_parameter_annotations_promoted_to_intrinsic() {
    let mut reg = NativeMethodRegistry::new();
    reg.set_real_jdk(true);

    let class = "java/lang/reflect/Method";
    let method = "getParameterAnnotations";
    let desc = "()[[Ljava/lang/annotation/Annotation;";

    register_bridge(&mut reg, class, method, desc, dummy_cb);

    let id = reg.resolve_id(class, method, desc).expect("getParameterAnnotations should be registered");
    assert_eq!(
        reg.kind_of_id(id),
        Some(NativeKind::Intrinsic),
        "getParameterAnnotations must be promoted to Intrinsic under Compatible"
    );
}

#[test]
fn fast_boot_reflective_invoke_is_not_promoted_in_compatible_mode() {
    let mut reg = NativeMethodRegistry::new();
    reg.set_real_jdk(true);

    for (class, method, desc) in [
        (
            "java/lang/reflect/Method",
            "invoke",
            "(Ljava/lang/Object;[Ljava/lang/Object;)Ljava/lang/Object;",
        ),
        (
            "java/lang/reflect/Constructor",
            "newInstance",
            "([Ljava/lang/Object;)Ljava/lang/Object;",
        ),
    ] {
        assert!(
            !cratonvm_native_api::retired_shadow::is_compat_fast_boot_intrinsic(class, method, desc),
            "{class}.{method} must not be a fast-boot intrinsic"
        );
        register_bridge(&mut reg, class, method, desc, dummy_cb);
        // Never an `Intrinsic`: the JDK's own body (with its adapter frame and
        // handle customization) must win at every door.
        assert_ne!(
            reg.resolve_id(class, method, desc).and_then(|id| reg.kind_of_id(id)),
            Some(NativeKind::Intrinsic),
            "{class}.{method} must not be promoted to Intrinsic under Compatible"
        );
    }
}

#[test]
fn fast_boot_reflective_invoke_dropped_in_jdk_only_mode() {
    let mut reg = NativeMethodRegistry::new();
    reg.set_compatibility_mode(CompatibilityMode::JdkOnly);
    reg.set_real_jdk(true);

    let class = "java/lang/reflect/Method";
    let method = "invoke";
    let desc = "(Ljava/lang/Object;[Ljava/lang/Object;)Ljava/lang/Object;";

    register_bridge(&mut reg, class, method, desc, dummy_cb);

    assert!(
        reg.resolve_id(class, method, desc).is_none(),
        "Method.invoke must not be registered under JdkOnly (JDK bytecode runs)"
    );
}

/// Group 4 (`ResourceBundle`, `LocaleResources`, `Bundles`) was withdrawn in
/// j27 wave 9 (lane W9RPL): promoted, the natives answered
/// `J27CompatFastBootGroupsRows` off HotSpot 25 in `--compatible` (bundle
/// class, locale, base name, cache identity, missing-key messages, casts,
/// null checks, `Control`s ignored) and `NumberFormat.getCurrencyInstance`
/// threw `ClassCastException` from `LocaleData.getCurrencyNames`. The JDK's
/// bodies win again, as in `--jdk-only`.
#[test]
fn fast_boot_locale_and_resource_bundle_are_not_promoted() {
    let mut reg = NativeMethodRegistry::new();
    reg.set_real_jdk(true);
    for (class, method, desc) in [
        ("java/util/ResourceBundle", "getBundle", "(Ljava/lang/String;)Ljava/util/ResourceBundle;"),
        ("java/util/ResourceBundle", "getString", "(Ljava/lang/String;)Ljava/lang/String;"),
        ("sun/util/locale/provider/LocaleResources", "getBreakIteratorInfo", "(Ljava/lang/String;)Ljava/lang/Object;"),
        (
            "sun/util/resources/Bundles",
            "of",
            "(Ljava/lang/String;Ljava/util/Locale;Lsun/util/resources/Bundles$Strategy;)Ljava/util/ResourceBundle;",
        ),
    ] {
        assert!(
            !cratonvm_native_api::retired_shadow::is_compat_fast_boot_intrinsic(class, method, desc),
            "{class}.{method} must not be a fast-boot intrinsic"
        );
        register_bridge(&mut reg, class, method, desc, dummy_cb);
        assert_ne!(
            reg.resolve_id(class, method, desc).and_then(|id| reg.kind_of_id(id)),
            Some(NativeKind::Intrinsic),
            "{class}.{method} must not be promoted to Intrinsic under Compatible"
        );
    }
}


/// Same page: as `Intrinsic`s the `System` property accessors answered from a
/// `Properties` whose constructor never ran (`map == null`), so `File.<clinit>`
/// threw and nothing printed. They are not fast-boot intrinsics.
#[test]
fn fast_boot_system_property_accessors_are_not_intrinsics() {
    for (method, desc) in [
        ("getProperty", "(Ljava/lang/String;)Ljava/lang/String;"),
        ("setProperty", "(Ljava/lang/String;Ljava/lang/String;)Ljava/lang/String;"),
        ("clearProperty", "(Ljava/lang/String;)Ljava/lang/String;"),
        ("getProperties", "()Ljava/util/Properties;"),
    ] {
        assert!(
            !cratonvm_native_api::retired_shadow::is_compat_fast_boot_intrinsic(
                "java/lang/System",
                method,
                desc
            ),
            "System.{method} must not be a fast-boot intrinsic"
        );
    }
}
