use super::*;
use crate::registry::{NativeContext, NativeKind, NativeMethodRegistry};
use cratonvm_types::compat::CompatibilityMode;
use cratonvm_types::error::MethodCallResult;
use cratonvm_types::Value;

fn cb(_ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    Ok(None)
}

fn registry(mode: CompatibilityMode, real_jdk: bool) -> NativeMethodRegistry {
    let mut r = NativeMethodRegistry::new();
    r.set_compatibility_mode(mode);
    r.set_real_jdk(real_jdk);
    r
}

#[test]
fn the_tables_are_the_census_rows_and_none_is_a_retired_shadow() {
    assert_eq!(JDK_ONLY_YIELDED_BRIDGE_TRIPLES.len(), 7);
    #[cfg(unix)]
    assert_eq!(JDK_ONLY_YIELDED_MIRROR_TRIPLES.len(), 31);
    #[cfg(not(unix))]
    assert!(JDK_ONLY_YIELDED_MIRROR_TRIPLES.is_empty());
    for (c, m, d) in JDK_ONLY_YIELDED_BRIDGE_TRIPLES
        .iter()
        .chain(JDK_ONLY_YIELDED_MIRROR_TRIPLES.iter())
    {
        assert!(is_jdk_only_yielded_bridge(c, m, d), "{c}.{m}{d}");
        assert!(
            !triple_is_retired_shadow(c, m, d),
            "{c}.{m}{d} is a retired row, not this table's"
        );
    }
    // The reviewed Internal twins and the abstract interface rows stay.
    assert!(!is_jdk_only_yielded_bridge(
        "jdk/internal/misc/ScopedMemoryAccess",
        "getCharInternal",
        "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;J)C",
    ));
    assert!(!is_jdk_only_yielded_bridge(
        "java/util/Spliterator",
        "tryAdvance",
        "(Ljava/util/function/Consumer;)Z",
    ));
}

#[test]
fn only_a_strict_real_jdk_bridge_is_not_registered() {
    for &(c, m, d) in JDK_ONLY_YIELDED_BRIDGE_TRIPLES {
        let mut strict = registry(CompatibilityMode::JdkOnly, true);
        strict.register(c, m, d, cb, NativeKind::Bridge);
        assert_eq!(strict.kind_of(c, m, d), None, "{c}.{m}{d}");

        let mut intrinsic = registry(CompatibilityMode::JdkOnly, true);
        intrinsic.register(c, m, d, cb, NativeKind::Intrinsic);
        assert_eq!(
            intrinsic.kind_of(c, m, d),
            Some(NativeKind::Intrinsic),
            "{c}.{m}{d}"
        );

        let mut no_image = registry(CompatibilityMode::JdkOnly, false);
        no_image.register(c, m, d, cb, NativeKind::Bridge);
        assert_eq!(
            no_image.kind_of(c, m, d),
            Some(NativeKind::Bridge),
            "{c}.{m}{d}"
        );

        let mut compatible = registry(CompatibilityMode::Compatible, false);
        compatible.register(c, m, d, cb, NativeKind::Bridge);
        assert_eq!(
            compatible.kind_of(c, m, d),
            Some(NativeKind::Bridge),
            "{c}.{m}{d}"
        );
    }
}

/// Wave 19: `ClassLoader`'s three instance resource lookups are a reviewed
/// `Intrinsic` under `--compatible` over a real JDK, from an ambient
/// `Bridge` registration too, and stay a `Bridge` everywhere else.
#[test]
fn the_loader_resource_rows_are_compatible_intrinsics_only() {
    assert_eq!(COMPAT_LOADER_RESOURCE_INTRINSIC_TRIPLES.len(), 3);
    for &(c, m, d) in COMPAT_LOADER_RESOURCE_INTRINSIC_TRIPLES {
        assert!(is_compat_loader_resource_intrinsic(c, m, d), "{c}.{m}{d}");
        assert!(!triple_is_retired_shadow(c, m, d), "{c}.{m}{d}");
        assert!(!is_jdk_only_yielded_bridge(c, m, d), "{c}.{m}{d}");

        let mut compatible = registry(CompatibilityMode::Compatible, true);
        compatible.register(c, m, d, cb, NativeKind::Bridge);
        assert_eq!(
            compatible.kind_of(c, m, d),
            Some(NativeKind::Intrinsic),
            "{c}.{m}{d}"
        );

        let mut strict = registry(CompatibilityMode::JdkOnly, true);
        strict.register(c, m, d, cb, NativeKind::Bridge);
        assert_eq!(
            strict.kind_of(c, m, d),
            Some(NativeKind::Bridge),
            "{c}.{m}{d}"
        );

        let mut synthetic = registry(CompatibilityMode::Compatible, false);
        synthetic.register(c, m, d, cb, NativeKind::Bridge);
        assert_eq!(
            synthetic.kind_of(c, m, d),
            Some(NativeKind::Bridge),
            "{c}.{m}{d}"
        );
    }
    // The static `getSystemResource*` forms share the callbacks and stay
    // `Bridge`s (every door already resolves them on `ClassLoader` itself).
    assert!(!is_compat_loader_resource_intrinsic(
        "java/lang/ClassLoader",
        "getSystemResource",
        "(Ljava/lang/String;)Ljava/net/URL;",
    ));
}
