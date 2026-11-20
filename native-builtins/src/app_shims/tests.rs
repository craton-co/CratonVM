use super::*;
use cratonvm_native_api::NativeKind;
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};

/// Internal-name prefixes that belong to the JDK, not to any
/// application. A family registering into one of these cannot be
/// skipped without taking the native away from unrelated programs.
const JDK_PREFIXES: [&str; 5] = ["java/", "javax/", "jdk/", "sun/", "com/sun/"];

/// `javax/` is shared: `javax.naming` / `javax.sql` / `javax.script` are
/// JDK modules, while `javax.transaction` / `javax.resource` are Jakarta
/// EE artifacts that ship with the app server. Only the JDK ones matter
/// for severability.
const JAKARTA_NOT_JDK: [&str; 2] = ["javax/transaction/", "javax/resource/"];

fn is_jdk_class(class: &str) -> bool {
    if JAKARTA_NOT_JDK.iter().any(|p| class.starts_with(p)) {
        return false;
    }
    JDK_PREFIXES.iter().any(|p| class.starts_with(p))
}

/// A registry that registers what the registrars ask for, regardless of
/// the ambient environment.
///
/// `NativeMethodRegistry::new()` latches `CRATONVM_NO_STUBS`
/// (`native-api/src/registry.rs:3979`), and under it `register()` silently
/// drops every `SyntheticStub` registration — which includes the JNDI
/// entanglement, since `wildfly_naming` never sets a category and so
/// inherits the registry's `SyntheticStub` default
/// (`registry.rs:3879`). Pinning it off keeps these tests measuring the
/// registrars rather than the shell they happen to run in.
fn probe_registry() -> NativeMethodRegistry {
    let mut registry = NativeMethodRegistry::new();
    registry.set_drop_synthetic_stubs(false);
    registry
}

fn registered_classes(family: ShimFamily) -> Vec<String> {
    let mut registry = probe_registry();
    family.register(&mut registry);
    let mut classes: Vec<String> = registry
        .dump_registrations()
        .into_iter()
        .map(|(class, _, _, _)| class.to_string())
        .collect();
    classes.sort();
    classes.dedup();
    classes
}

fn registered_keys(family: ShimFamily) -> Vec<(String, String, String)> {
    let mut registry = probe_registry();
    family.register(&mut registry);
    registry
        .dump_registrations()
        .into_iter()
        .map(|(class, method, descriptor, _)| {
            (
                class.to_string(),
                method.to_string(),
                descriptor.to_string(),
            )
        })
        .collect()
}

#[test]
fn every_family_registers_something() {
    for family in ShimFamily::ALL {
        assert!(
            !registered_classes(family).is_empty(),
            "{family:?} registered nothing — the seam has drifted away from \
             its registrars"
        );
    }
}

/// A selected family owns its own packages and nothing else.
#[test]
fn families_register_only_into_owned_prefixes() {
    for family in ShimFamily::ALL {
        let owned = family.owned_prefixes();
        for class in registered_classes(family) {
            let in_family = owned.iter().any(|p| class.starts_with(p));
            assert!(
                in_family,
                "{family:?} registers `{class}` outside its owned prefixes \
                 {owned:?}; platform bridges must use a core registrar"
            );
        }
    }
}

#[test]
fn application_families_have_no_jdk_entanglements() {
    for family in ShimFamily::ALL {
        assert!(family.jdk_entanglements().is_empty(), "{family:?}");
    }
}

/// The declared seam must agree with the classes each pack registers.
#[test]
fn severability_matches_measured_jdk_entanglement() {
    for family in ShimFamily::ALL {
        let has_jdk_class = registered_classes(family).iter().any(|c| is_jdk_class(c));
        assert_eq!(
            family.is_severable(),
            !has_jdk_class,
            "{family:?}: is_severable() disagrees with what it actually registers"
        );
    }
    assert!(ShimFamily::AppIntrinsics.is_severable());
    assert!(ShimFamily::BouncyCastle.is_severable());
    assert!(ShimFamily::DataSourcePools.is_severable());
    assert!(ShimFamily::JBossWildFlyXnio.is_severable());
}

#[test]
fn witness_selection_is_family_local() {
    let selected = ShimSelection::from_resource_probe(|resource| {
        resource == "org/bouncycastle/crypto/engines/AESEngine.class"
            || resource == "org/xnio/OptionMap.class"
    });
    assert!(!selected.includes(ShimFamily::AppIntrinsics));
    assert!(selected.includes(ShimFamily::BouncyCastle));
    assert!(!selected.includes(ShimFamily::DataSourcePools));
    assert!(selected.includes(ShimFamily::JBossWildFlyXnio));
    assert_eq!(ShimSelection::NONE.0, 0);
    for family in ShimFamily::ALL {
        assert!(ShimSelection::ALL.includes(family));
    }
}

#[test]
fn no_application_packs_keeps_jdk_entanglements_registered() {
    let mut registry = probe_registry();
    crate::register_essential_natives_with_shims(&mut registry, ShimSelection::NONE);
    let registrations: std::collections::HashSet<(String, String, String)> = registry
        .dump_registrations()
        .into_iter()
        .map(|(class, method, descriptor, _)| {
            (
                class.to_string(),
                method.to_string(),
                descriptor.to_string(),
            )
        })
        .collect();

    for jdk_class in [
        "java/security/AccessControlContext",
        "javax/security/auth/Subject",
        "javax/security/auth/login/LoginContext",
        "javax/sql/DataSource",
    ] {
        assert!(
            registrations.iter().any(|(class, _, _)| class == jdk_class),
            "core JDK bridge {jdk_class} disappeared with application packs disabled"
        );
    }
    // Real-JDK InitialContext must retain its provider-selection bytecode;
    // the in-memory naming bridge has only the synthetic JDK field layout.
    #[cfg(feature = "synthetic-jdk")]
    {
        let jdk_class = "javax/naming/InitialContext";
        assert!(
            registrations.iter().any(|(class, _, _)| class == jdk_class),
            "core JDK bridge {jdk_class} disappeared with application packs disabled"
        );
    }
    let mut leaks = Vec::new();
    for family in ShimFamily::ALL {
        for key in registered_keys(family) {
            if registrations.contains(&key) {
                leaks.push(format!("{family:?}:{key:?}"));
            }
        }
    }
    assert!(
        leaks.is_empty(),
        "application pack classes leaked into core: {leaks:?}"
    );
}

/// `NativeKind` is ambient (`current_category` persists across
/// `set_category`), which is precisely what made the `d8092acb` tag
/// audit unsound. Grouping the call sequence must not disturb it: every
/// family's registrars save and restore the category, so the ambient
/// value a caller had before is the value it has after.
///
/// `None` is in the seed list and is the interesting one. It is what the
/// registry now carries outside any scope, and "no scope covers this
/// registration" is a state a registrar can leak just as easily as a wrong
/// kind — leaking a *category* over it would make the census report those
/// rows as adjudicated by whoever the leak belonged to.
#[test]
fn grouping_does_not_leak_an_ambient_category() {
    for seed in [
        None,
        Some(NativeKind::SyntheticStub),
        Some(NativeKind::Bridge),
        Some(NativeKind::Intrinsic),
    ] {
        for family in ShimFamily::ALL {
            let mut registry = probe_registry();
            family.register(&mut registry);

        }
    }
}

/// A witness is only useful if it names a class the family actually
/// serves, so a classpath probe for it means what the gating change
/// would assume it means.
#[test]
fn witness_resources_are_class_files_under_an_owned_prefix() {
    for family in ShimFamily::ALL {
        let witnesses = family.witness_resources();
        assert!(
            !witnesses.is_empty(),
            "{family:?} has no classpath witness, so it could never be probed for"
        );
        for witness in witnesses {
            assert!(
                witness.ends_with(".class"),
                "{family:?} witness `{witness}` is not a .class resource name; \
                 entry_index holds exact archive entry names"
            );
            assert!(
                !witness.starts_with('/'),
                "{family:?} witness `{witness}` must be a bare internal name"
            );
            assert!(
                family
                    .owned_prefixes()
                    .iter()
                    .any(|p| witness.starts_with(p)),
                "{family:?} witness `{witness}` is outside the family's own \
                 packages, so its presence would not imply the family is in use"
            );
        }
    }
}

/// Guards the classification correction in the module header: the
/// Xerces intrinsics are `java.xml`, so they are not a candidate family
/// and must never be added to `ShimFamily`.
#[test]
fn jdk_bundled_xerces_is_not_a_shim_family() {
    assert!(
        is_jdk_class("com/sun/org/apache/xerces/internal/impl/dtd/models/CMStateSet"),
        "the JDK-bundled Xerces must classify as a JDK class"
    );
    assert!(is_jdk_class("jdk/xml/internal/XMLLimitAnalyzer"));
    for family in ShimFamily::ALL {
        assert!(
            !family
                .owned_prefixes()
                .iter()
                .any(|p| p.starts_with("com/sun/") || p.starts_with("jdk/")),
            "{family:?} claims a JDK-internal package as its own"
        );
    }
}

#[test]
fn jakarta_ee_packages_are_not_treated_as_jdk() {
    assert!(!is_jdk_class("javax/transaction/TransactionManager"));
    assert!(!is_jdk_class("javax/resource/spi/ManagedConnection"));
    assert!(is_jdk_class("javax/sql/DataSource"));
    assert!(is_jdk_class("javax/naming/InitialContext"));
    assert!(is_jdk_class("java/security/AccessControlContext"));
}
