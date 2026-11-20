use super::*;
use cratonvm_types::compat::CompatibilityMode;

fn noop(
    _: &mut dyn cratonvm_native_api::NativeContext,
    _: &[cratonvm_types::Value],
) -> cratonvm_types::error::MethodCallResult {
    Ok(None)
}

// Fixture classes inside the carrier packages: real JDK rows carry the
// registry's own per-class rules, which these tests are not about.
const SSL_FIXTURE: &str = "sun/security/ssl/CarrierFixture";
const SSL_PUBLIC_FIXTURE: &str = "javax/net/ssl/CarrierFixture";
const SUBPACKAGE_FIXTURE: &str = "javax/net/ssl/sub/CarrierFixture";

fn registry(mode: CompatibilityMode) -> NativeMethodRegistry {
    let mut r = NativeMethodRegistry::new();
    r.set_compatibility_mode(mode);
    r.register(
        "java/lang/Class",
        "getPackage",
        "()Ljava/lang/Package;",
        noop,
        NativeKind::Bridge,
    );
    r.register(
        "java/lang/Thread",
        "<init>",
        "()V",
        noop,
        NativeKind::Bridge,
    );
    r.register("java/lang/Thread", "start", "()V", noop, NativeKind::Bridge);
    r.register(SSL_FIXTURE, "m", "()V", noop, NativeKind::Bridge);
    r.register(
        SSL_PUBLIC_FIXTURE,
        "m",
        "()V",
        noop,
        NativeKind::SyntheticStub,
    );
    r.register(SUBPACKAGE_FIXTURE, "m", "()V", noop, NativeKind::Bridge);
    r
}

fn kinds(r: &NativeMethodRegistry) -> Vec<Option<NativeKind>> {
    vec![
        r.kind_of("java/lang/Class", "getPackage", "()Ljava/lang/Package;"),
        r.kind_of("java/lang/Thread", "<init>", "()V"),
        r.kind_of("java/lang/Thread", "start", "()V"),
        r.kind_of(SSL_FIXTURE, "m", "()V"),
        r.kind_of(SSL_PUBLIC_FIXTURE, "m", "()V"),
        r.kind_of(SUBPACKAGE_FIXTURE, "m", "()V"),
    ]
}

#[test]
fn compatible_carriers_become_intrinsic_and_keep_their_callback() {
    let mut r = registry(CompatibilityMode::Compatible);
    let before = kinds(&r);
    classify_compat_carriers(&mut r);
    let after = kinds(&r);
    let intrinsic = |k: Option<NativeKind>| k.map(|_| NativeKind::Intrinsic);
    assert_eq!(after[0], intrinsic(before[0]), "a listed triple");
    assert_eq!(
        after[1],
        intrinsic(before[1]),
        "a carrier class's constructor"
    );
    assert_eq!(
        after[2], before[2],
        "a carrier class's other rows, constructors only"
    );
    assert_eq!(after[3], intrinsic(before[3]), "a carrier package's class");
    assert_eq!(after[4], intrinsic(before[4]), "a carrier package's class");
    assert_eq!(
        after[5], before[5],
        "a package means its own classes, not a subpackage"
    );
    assert!(
        after[0].is_some() && after[3].is_some(),
        "the fixture registered"
    );
    // A listed triple nobody registered stays unregistered.
    assert_eq!(
        r.kind_of(
            "java/lang/ClassLoader",
            "getPlatformClassLoader",
            "()Ljava/lang/ClassLoader;"
        ),
        None
    );
}

#[test]
fn jdk_only_registrations_are_untouched() {
    let mut r = registry(CompatibilityMode::JdkOnly);
    let before = kinds(&r);
    classify_compat_carriers(&mut r);
    assert_eq!(kinds(&r), before);
}
