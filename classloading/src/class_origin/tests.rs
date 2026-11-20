use super::*;

#[test]
fn default_is_vm_internal_not_a_stub() {
    assert_eq!(ClassOrigin::default(), ClassOrigin::VmInternal);
    assert!(!ClassOrigin::default().is_compatibility_stub());
}

/// The whole point of the enum: strict mode rejects exactly one variant.
#[test]
fn only_compatibility_stub_is_rejected_under_jdk_only() {
    let every = [
        ClassOrigin::BootImage {
            module: Some(Arc::from("java.base")),
            source: Arc::from("jrt:/java.base"),
        },
        ClassOrigin::ApplicationClassPath {
            source: Arc::from("file:/app.jar"),
        },
        ClassOrigin::UserDefined {
            loader: ClassLoaderId::UserDefined(7),
            source: None,
        },
        ClassOrigin::VmArray,
        ClassOrigin::HiddenClass { host: None },
        ClassOrigin::GeneratedLambda { host: None },
        ClassOrigin::GeneratedProxy {
            interfaces: Arc::from(Vec::<ClassId>::new()),
        },
        ClassOrigin::ReflectionAccessor { host: None },
        ClassOrigin::VmInternal,
    ];
    for origin in &every {
        assert!(
            origin.allowed_in(CompatibilityMode::JdkOnly),
            "{origin:?} must stay legal under --real-jdk"
        );
        assert!(origin.allowed_in(CompatibilityMode::Compatible));
    }
    let stub = ClassOrigin::compatibility_stub("test");
    assert!(stub.allowed_in(CompatibilityMode::Compatible));
    assert!(!stub.allowed_in(CompatibilityMode::JdkOnly));
    assert!(stub.is_compatibility_stub());
    assert_eq!(stub.reason(), Some("test"));
}

/// Array classes are the variant most at risk of being lumped in with
/// stubs — they are VM-created and have no class file. Both modes allow
/// them (contract §5, "Array classes get `VmArray`, never
/// `CompatibilityStub`, in **both** modes").
#[test]
fn arrays_are_allowed_in_both_modes() {
    assert!(ClassOrigin::VmArray.allowed_in(CompatibilityMode::JdkOnly));
    assert!(ClassOrigin::VmArray.allowed_in(CompatibilityMode::Compatible));
    assert!(!ClassOrigin::VmArray.is_compatibility_stub());
}

#[test]
fn tags_are_distinct_and_stable() {
    let tags = [
        ClassOrigin::BootImage {
            module: None,
            source: Arc::from(""),
        }
        .as_str(),
        ClassOrigin::ApplicationClassPath {
            source: Arc::from(""),
        }
        .as_str(),
        ClassOrigin::UserDefined {
            loader: ClassLoaderId::Bootstrap,
            source: None,
        }
        .as_str(),
        ClassOrigin::VmArray.as_str(),
        ClassOrigin::HiddenClass { host: None }.as_str(),
        ClassOrigin::GeneratedLambda { host: None }.as_str(),
        ClassOrigin::GeneratedProxy {
            interfaces: Arc::from(Vec::<ClassId>::new()),
        }
        .as_str(),
        ClassOrigin::ReflectionAccessor { host: None }.as_str(),
        ClassOrigin::VmInternal.as_str(),
        ClassOrigin::compatibility_stub("r").as_str(),
    ];
    let mut sorted = tags.to_vec();
    sorted.sort_unstable();
    sorted.dedup();
    assert_eq!(sorted.len(), tags.len(), "origin tags must be injective");
    assert_eq!(ClassOrigin::VmArray.as_str(), "vm-array");
    assert_eq!(
        ClassOrigin::compatibility_stub("r").as_str(),
        "compatibility-stub"
    );
}

#[test]
fn real_bytes_predicate() {
    assert!(ClassOrigin::BootImage {
        module: None,
        source: Arc::from("jrt:/java.base"),
    }
    .has_real_bytes());
    assert!(ClassOrigin::HiddenClass { host: None }.has_real_bytes());
    assert!(!ClassOrigin::VmArray.has_real_bytes());
    assert!(!ClassOrigin::VmInternal.has_real_bytes());
    assert!(!ClassOrigin::compatibility_stub("r").has_real_bytes());
}
