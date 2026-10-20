//! `--compatible` carrier natives: the registrations a JDK body cannot replace,
//! reviewed and classified `Intrinsic` in one place.
//!
//! Since the FFM merge (`resolve_step1_native`, `invoke_special_shared_impl`)
//! concrete JDK bytecode is authoritative over a registered `Bridge` or
//! `SyntheticStub` in BOTH policies. Under `--compatible` that is right for a
//! native that only re-implements a JDK body, but not for one that IS the
//! VM's own model of state the JDK body reads and `--compatible` never fills:
//! the VM's loader carriers (it never adopts the JDK's built-in loaders: the
//! carriers keep no `packages` map, and the JDK's `ClassLoaders` singletons are
//! not the objects `getClassLoader()` answers), its thread carrier, and its
//! rustls-backed JSSE and keystore carriers. Running the JDK body there reads
//! state the carrier never wrote. Measured on Linux (`regression-suite`,
//! 2026-10-11); each row below names its witness.
//!
//! The pass runs once, after every registrar, so it classifies whichever
//! last-write-wins site installed the callback (`Thread.<init>` alone has about
//! two dozen) and keeps that callback. `--jdk-only` never runs it: its loaders
//! are the JDK's own and its registrations stay as they are. Commits
//! `57f3176f87` (`Class.getClassLoader`), `6048b179c4` (`ClassLoader.loadClass`)
//! and `f763d0ba33` (`ClassLoader.getUnnamedModule`) classified carriers at
//! their sites; this is the same review for the rest.
//!
//! What would let a row go: `--compatible` adopting the JDK's built-in loaders
//! (`classloader::real_builtin_loaders_enabled`, jdk-only today) for the loader
//! rows, and the JDK's own JSSE over the same keystore for the TLS rows; each
//! row names the state its JDK body would need.

use cratonvm_native_api::{NativeKind, NativeMethodRegistry};

/// Exact triples whose JDK body reads loader state `--compatible` never fills.
const COMPAT_LOADER_CARRIERS: &[(&str, &str, &str)] = &[
    // `RLangPackages`: the body is `cl.definePackage(this)` on the loader
    // carrier, which keeps no `packages` map (even with `Class.classLoader`
    // filled, `f763d0ba33`), so each call minted a fresh `Package` and two
    // `getPackage()` calls on one class disagreed.
    ("java/lang/Class", "getPackage", "()Ljava/lang/Package;"),
    // `RLoaderIdentity`: the body answers the JDK's `ClassLoaders` singleton,
    // while every `getClassLoader()` answers the VM's carrier: two platform
    // loaders, and `X.class.getClassLoader() == getPlatformClassLoader()` false.
    (
        "java/lang/ClassLoader",
        "getPlatformClassLoader",
        "()Ljava/lang/ClassLoader;",
    ),
    (
        "java/lang/ClassLoader",
        "getSystemClassLoader",
        "()Ljava/lang/ClassLoader;",
    ),
    // `RBuiltinLoaderPackages`: the body reads the same unfilled `packages`
    // map, so a platform package stayed undefined after its class loaded.
    (
        "java/lang/ClassLoader",
        "getDefinedPackage",
        "(Ljava/lang/String;)Ljava/lang/Package;",
    ),
    // `RFsSingleton`, `RStrings`, `RJdkFormatLocale`, `RJdkIntrinsics2`: the
    // body asks the module's loader carrier `loadClass(Module, String)`, which
    // finds nothing, so `ServiceLoader` lost every module provider
    // (`ZipFileSystemProvider`, the CLDR locale data).
    (
        "java/lang/Class",
        "forName",
        "(Ljava/lang/Module;Ljava/lang/String;)Ljava/lang/Class;",
    ),
];

/// Classes whose registrations are the VM's carrier: `true` for every row on
/// the class, `false` for its constructors only.
///
/// * `java/lang/Thread`, constructors (`RJdkIntrinsics3` `tlocal`): the carrier
///   constructor snapshots the creator's `InheritableThreadLocal`s, which
///   `--compatible` keeps in its own model; the JDK constructor copies the JDK
///   map, which that model does not use, so a child read the parent's value
///   live at `start()`.
/// * `sun/security/ssl/SSLSessionImpl`, every row (`RSslNullSession`): the
///   carrier `<init>` builds the synthetic `SSLSession` layout
///   (`tls::init_ssl_session_fields`), so a JDK accessor reads a field that
///   layout does not have: `SSLSessionImpl.nullSession.getCipherSuite()` threw
///   `NullPointerException` where HotSpot answers `SSL_NULL_WITH_NULL_NULL`.
/// * `javax/security/auth/x500/X500Principal`, every row (`RSslLiveSession`):
///   the carrier constructors (`jca::x500`) write the carrier's layout, and once
///   `getEncoded` ran the JDK body over it the TLS peer principal no longer
///   `equals` the certificate's subject.
const COMPAT_CARRIER_CLASSES: &[(&str, bool)] = &[
    ("java/lang/Thread", false),
    ("sun/security/ssl/SSLSessionImpl", true),
    ("javax/security/auth/x500/X500Principal", true),
];

/// The keystore engine surfaces (`keystore::CARRIER_ENGINE_CLASSES`, every
/// row; `RSslLiveSession`, `RSslEndpointIdentification`): they keep entries in
/// the side table the rustls TLS carrier reads, so a key stored through the
/// JDK's `KeyStoreDelegator` body never reached TLS ("No TLS key/cert
/// configured").
///
/// Packages every registration in which is the VM's carrier.
///
/// The JSSE family (`RSslNullSession`, `RSslLiveSession`,
/// `RSslEndpointIdentification`): `--compatible`'s TLS is rustls behind
/// carriers minted AS the `sun.security.ssl` Impl classes (`tls_carriers`),
/// with the rows mirrored onto them. Once the public entry points
/// (`SSLSocketFactory.getDefault`, `SSLContext.getDefault`) ran their JDK
/// bodies, real JDK objects met carrier ones: `SSLSocketImpl.getSession()`
/// read the `conContext` a carrier socket does not have
/// (`NullPointerException`), and `SSLContextImpl.<clinit>` reached `JceSecurity`.
/// Every row in these packages is a member of that one model.
const COMPAT_CARRIER_PACKAGES: &[&str] = &["javax/net/ssl/", "sun/security/ssl/"];

/// Classify the `--compatible` carriers `Intrinsic`. Call once, after every
/// registrar, and only for a `--compatible` registry over a real JDK.
pub fn classify_compat_carriers(r: &mut NativeMethodRegistry) {
    if r.compatibility_mode().is_jdk_only() {
        return;
    }
    for (class, method, descriptor) in COMPAT_LOADER_CARRIERS {
        r.restate_kind(class, method, descriptor, NativeKind::Intrinsic);
    }
    let rows: std::collections::BTreeSet<(String, String, String)> = r
        .dump_registrations()
        .into_iter()
        .filter(|(c, m, _, _)| {
            COMPAT_CARRIER_CLASSES
                .iter()
                .any(|(class, all_rows)| c == class && (*all_rows || *m == "<init>"))
                || crate::keystore::CARRIER_ENGINE_CLASSES.contains(c)
                || COMPAT_CARRIER_PACKAGES.iter().any(|package| {
                    c.strip_prefix(package)
                        .is_some_and(|rest| !rest.contains('/'))
                })
        })
        .map(|(c, m, d, _)| (c.to_string(), m.to_string(), d.to_string()))
        .collect();
    for (class, method, descriptor) in rows {
        r.restate_kind(&class, &method, &descriptor, NativeKind::Intrinsic);
    }
}

#[cfg(test)]
mod tests {
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
        r.register_with_kind(
            "java/lang/Class",
            "getPackage",
            "()Ljava/lang/Package;",
            noop,
            NativeKind::Bridge,
        );
        r.register_with_kind(
            "java/lang/Thread",
            "<init>",
            "()V",
            noop,
            NativeKind::Bridge,
        );
        r.register_with_kind("java/lang/Thread", "start", "()V", noop, NativeKind::Bridge);
        r.register_with_kind(SSL_FIXTURE, "m", "()V", noop, NativeKind::Bridge);
        r.register_with_kind(
            SSL_PUBLIC_FIXTURE,
            "m",
            "()V",
            noop,
            NativeKind::SyntheticStub,
        );
        r.register_with_kind(SUBPACKAGE_FIXTURE, "m", "()V", noop, NativeKind::Bridge);
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
}
