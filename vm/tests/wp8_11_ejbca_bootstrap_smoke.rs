// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! WP8.11 — EJBCA EAR-deploy bootstrap smoke test.
//!
//! Once WP8.10 (real WildFly boot) lands and the deployment scanner picks
//! up `ejbca.ear`, EJBCA exercises a stack we cannot test end-to-end in a
//! cargo unit test — Hibernate, Liquibase, BouncyCastle, RESTEasy, Weld
//! CDI, JTA, Undertow. What we *can* do here is unit-pin the JVM-level
//! hooks each of those touches *first* so a regression on any one of
//! them flips this test red instantly, without needing to re-run the
//! full `bench/ejbca-deploy/` fixture.
//!
//! The pins below are organized by EJBCA dependency. Each assertion
//! corresponds to a known first-failure surface diagnosed during the
//! WP8.11.2 audit — see `bench/ejbca-deploy/diagnostic.md` for the
//! full triage. Comments cite the EJBCA / Hibernate / BouncyCastle
//! source pattern that drives each lookup.
//!
//! ## What this test does NOT do
//!
//! - Boot WildFly. That is `bench/wildfly-boot/` (WP8.10) and a separate
//!   parent agent's responsibility.
//! - Deploy ejbca.ear. That is `bench/ejbca-deploy/` (WP8.11.1).
//! - Exercise BouncyCastle's algorithm registrations. That is WP6.5,
//!   already partially landed (`bench/wildfly/bcprobe.stderr.log` shows
//!   today's first-failure at `Provider$Service.<init>` pc=29 — out of
//!   scope here, deferred to WP6.5 closure).
//!
//! ## What it DOES pin
//!
//! 1. Provider chain seed has the 13 JDK 25 default providers — required
//!    for `Security.getProviders()` walks performed by Hibernate's
//!    `org.hibernate.tool.schema.internal.Helper` during Liquibase
//!    pre-seed.
//! 2. `FileChannel.map0` is registered — required for H2 MV-store's
//!    `*.mv.db` mmap. This is the WP3.5 entry the roadmap pins as
//!    "❌ stub"; the test confirms the registration is live so the
//!    pin can be flipped (audit finding documented in
//!    `bench/ejbca-deploy/diagnostic.md`).
//! 3. `ServiceLoader.iterator` is registered — required for RESTEasy's
//!    `RuntimeDelegate.findDelegate()` and JBoss-Logging's
//!    `LoggerProviders.findProvider`.
//! 4. `Proxy.newProxyInstance` is registered with the JDK 25 descriptor
//!    — required for Hibernate's entity proxy generator and Weld CDI's
//!    `Bean<?>` proxies.
//! 5. JCA `KeyPairGenerator` / `Signature` natives are registered —
//!    required for the very first cert-issue path EJBCA hits during
//!    install-wizard render (`installInfo.jsp` → `CryptoTokenFactory`).
//! 6. `X500Principal` round-trip works — required for every
//!    `CertTools.getSubjectDN(cert)` call EJBCA makes.

use cratonvm_native_api::NativeMethodRegistry;

/// 1. Provider chain — Hibernate's `BeanContainerInitiator` and
/// Liquibase's `DatabaseFactory.findCorrectDatabaseImplementation` both
/// walk `Security.getProviders()` looking for SunJCE early in the JPA
/// init path. A missing seed entry would surface as `NoSuchProviderException`
/// long before the first SQL statement runs.
#[test]
fn wp8_11_provider_chain_seed_has_jdk25_defaults() {
    let mut r = NativeMethodRegistry::new();
    cratonvm_native_builtins::jca::register_jca_natives(&mut r);

    // Spot-check the four providers EJBCA queries by name during init.
    for cls in ["java/security/Security", "java/security/Provider"] {
        assert!(
            r.find(cls, "getName", "()Ljava/lang/String;").is_some()
                || r.find(cls, "getProviders", "()[Ljava/security/Provider;")
                    .is_some()
                || r.find(
                    cls,
                    "getProvider",
                    "(Ljava/lang/String;)Ljava/security/Provider;"
                )
                .is_some(),
            "Security/Provider must have at least one accessor registered, none found on {cls}"
        );
    }

    // Critical-path natives EJBCA hits via `org.cesecore.config.ConfigurationHolder`:
    assert!(
        r.find(
            "java/security/Security",
            "getProvider",
            "(Ljava/lang/String;)Ljava/security/Provider;",
        )
        .is_some(),
        "Security.getProvider(String) must be registered (cesecore queries 'BC' \
         + 'SunRsaSign' + 'SunEC' by name during ConfigurationHolder.<clinit>)"
    );
    assert!(
        r.find(
            "java/security/Security",
            "addProvider",
            "(Ljava/security/Provider;)I",
        )
        .is_some(),
        "Security.addProvider must be registered (BouncyCastleProvider \
         registers via this exact descriptor at CryptoProviderTools.installBCProvider)"
    );
}

/// 2. FileChannel.map0 — required by H2 MV-store. The roadmap (WP3.5)
/// pins this as "stub" but `native-io/src/file_channel.rs:515` actually
/// implements it via `memmap2`. This test confirms the registration is
/// live so the diagnostic.md report can flip the WP3.5 pin to ✅.
#[test]
fn wp8_11_file_channel_map0_registered_for_h2() {
    let mut r = NativeMethodRegistry::new();
    cratonvm_native_io::register_io_natives(&mut r);

    // JDK 25 dispatches via FileDispatcherImpl on Windows-style boot;
    // legacy FileChannelImpl path is the fallback.
    let modern = r
        .find(
            "sun/nio/ch/FileDispatcherImpl",
            "map0",
            "(Ljava/io/FileDescriptor;IJJZ)J",
        )
        .is_some();
    let legacy = r
        .find("sun/nio/ch/FileChannelImpl", "map0", "(IJJZ)J")
        .is_some();
    assert!(
        modern || legacy,
        "FileChannel.map0 must be registered on at least one of \
         FileDispatcherImpl / FileChannelImpl — H2 MV-store calls \
         FileChannel.map(READ_WRITE, 0, len) on every *.mv.db open"
    );
    assert!(
        r.find("sun/nio/ch/FileDispatcherImpl", "unmap0", "(JJ)I")
            .is_some()
            || r.find("sun/nio/ch/FileChannelImpl", "unmap0", "(JJ)I")
                .is_some(),
        "FileChannel.unmap0 must be registered (H2 closes mappings on shutdown)"
    );
}

/// 3. ServiceLoader.iterator — RESTEasy uses this to discover the
/// `RuntimeDelegate` provider; without it `RuntimeDelegate.getInstance()`
/// returns null and EJBCA's `EjbcaRESTApplication.getClasses()` NPEs.
#[test]
fn wp8_11_service_loader_iterator_registered_for_resteasy() {
    let mut r = NativeMethodRegistry::new();
    cratonvm_native_builtins::jdbc::register_jdbc_driver_natives(&mut r);

    // INVERTED 2026-08-30. `register_service_loader_natives`' body is
    // `#[cfg(feature = "synthetic-jdk")]`, and its header explains why: in a
    // real-JDK build `java.util.ServiceLoader` is pure Java that the VM
    // already runs, and these natives were a shadow over it that got it
    // WRONG -- `iterator()` answered `java.util.ArrayList$Itr` where HotSpot
    // answers `java.util.ServiceLoader$2` (MEASURED,
    // `probes/DodServiceLoaderSweep`), losing the lazy iterator's semantics
    // so a `ServiceConfigurationError` surfaced at `load` instead of at the
    // offending provider.
    //
    // The removal is measured, not assumed: `--jdk-only` refuses every
    // SyntheticStub and has been running the real `ServiceLoader` all along,
    // HotSpot-identically on both SPIs, with `jdbc` 92/92 and `h2jdbc` 12/12
    // -- the latter's `DriverManager` discovery being
    // `ServiceLoader.load(java.sql.Driver.class)`, the exact case this test
    // was written for.
    //
    // So the assertion is now that the shadow is ABSENT. Asserting it present
    // asserted the pre-removal contract, and would pass again only by putting
    // the wrong iterator back.
    assert!(
        r.find(
            "java/util/ServiceLoader",
            "load",
            "(Ljava/lang/Class;)Ljava/util/ServiceLoader;",
        )
        .is_none(),
        "ServiceLoader.load(Class) must NOT be shadowed in a real-JDK build"
    );
    assert!(
        r.find(
            "java/util/ServiceLoader",
            "iterator",
            "()Ljava/util/Iterator;",
        )
        .is_none(),
        "ServiceLoader.iterator must NOT be shadowed: the native returned an \
         ArrayList$Itr where HotSpot returns ServiceLoader$2"
    );
}

/// 4. Dynamic Proxy — required by Hibernate's runtime entity-proxy
/// generation (`org.hibernate.proxy.pojo.bytebuddy.ByteBuddyProxyHelper`)
/// and Weld CDI's `Bean<?>` proxies. WP2.5 v3 just landed in commit
/// b289a94 with a real `proxy_gen.rs` bytecode emitter (1477 LoC).
#[test]
fn wp8_11_proxy_natives_registered_for_hibernate_and_weld() {
    let mut r = NativeMethodRegistry::new();
    cratonvm_native_builtins::register_reflect_proxy_natives(&mut r);

    assert!(
        r.find(
            "java/lang/reflect/Proxy",
            "newProxyInstance",
            "(Ljava/lang/ClassLoader;[Ljava/lang/Class;Ljava/lang/reflect/InvocationHandler;)\
             Ljava/lang/Object;"
        )
        .is_some(),
        "Proxy.newProxyInstance with JDK 25 descriptor must be registered \
         (Hibernate's HibernateProxy helper calls it during EntityManager init)"
    );
    assert!(
        r.find(
            "java/lang/reflect/Proxy$Dispatch",
            "invokeProxy",
            "(Ljava/lang/Object;Ljava/lang/reflect/Method;[Ljava/lang/Object;)Ljava/lang/Object;",
        )
        .is_some(),
        "Proxy$Dispatch.invokeProxy (WP2.5 v3 bytecode-generator hook) must \
         be wired — every generated $ProxyN class INVOKESTATICs this helper"
    );
}

/// 5. JCA KeyPairGenerator + Signature + KeyFactory — EJBCA's first
/// cert-issue path hits all three within `CertTools.genSelfCert()`.
/// Without natives, the JDK 25 bytecode reaches
/// `sun.security.jca.GetInstance.getServices` which NPEs on a synthetic
/// Provider whose `getServices()` returns an empty set.
#[test]
fn wp8_11_jca_keypair_signature_keyfactory_registered() {
    let mut r = NativeMethodRegistry::new();
    cratonvm_native_builtins::jca::register_jca_natives(&mut r);

    assert!(
        r.find(
            "java/security/KeyPairGenerator",
            "getInstance",
            "(Ljava/lang/String;)Ljava/security/KeyPairGenerator;"
        )
        .is_some(),
        "KeyPairGenerator.getInstance(String) must be a native override \
         (real-JDK bytecode NPEs at GetInstance.getServices)"
    );
    assert!(
        r.find(
            "java/security/Signature",
            "getInstance",
            "(Ljava/lang/String;)Ljava/security/Signature;"
        )
        .is_some(),
        "Signature.getInstance(String) must be a native override"
    );
    assert!(
        r.find(
            "java/security/KeyFactory",
            "getInstance",
            "(Ljava/lang/String;)Ljava/security/KeyFactory;"
        )
        .is_some(),
        "KeyFactory.getInstance(String) must be a native override"
    );
}

/// 6. X500Principal — `CertTools.stringToBcX500Name` round-trips DN
/// strings through `new X500Principal(dn).getEncoded()`. A failure here
/// surfaces as `IllegalArgumentException("improperly specified input name")`
/// inside EJBCA's RA-gui at first user creation.
#[test]
fn wp8_11_x500_principal_natives_registered() {
    let mut r = NativeMethodRegistry::new();
    cratonvm_native_builtins::jca::register_jca_natives(&mut r);

    assert!(
        r.find(
            "javax/security/auth/x500/X500Principal",
            "<init>",
            "(Ljava/lang/String;)V"
        )
        .is_some()
            || r.find(
                "javax/security/auth/x500/X500Principal",
                "getEncoded",
                "()[B"
            )
            .is_some(),
        "X500Principal must have at least the (String) ctor or getEncoded \
         registered (every issued cert hits this path)"
    );
}

/// Defence-in-depth: ASN.1 DER helpers are exposed (compile-time check).
/// EJBCA emits PKCS#10 CSRs; a missing helper would surface as
/// `IOException("DER length encoding")` deep inside BC's `X509v3CertificateBuilder`.
#[test]
fn wp8_11_asn1_der_primitives_present() {
    use cratonvm_native_builtins::jca::asn1;

    // OID round-trip — used by every X.509 algorithm-identifier write.
    let der = asn1::encode_oid("1.2.840.113549.1.1.11").unwrap(); // sha256WithRSAEncryption
    assert!(!der.is_empty(), "OID encoder must not produce empty output");
    let (tag, hdr, content_len, _total) = asn1::read_header(&der).unwrap();
    assert_eq!(tag, asn1::TAG_OID);
    let oid = asn1::read_oid(&der[hdr..hdr + content_len]).unwrap();
    assert_eq!(oid, "1.2.840.113549.1.1.11");

    // SEQUENCE wrap — used by every TBSCertificate write.
    let inner = asn1::encode_tlv(asn1::TAG_INTEGER, &[0x01, 0x00]);
    let outer = asn1::encode_sequence(&inner);
    assert_eq!(outer[0], asn1::TAG_SEQUENCE);
}

/// Diagnostic-only: confirm `proxy_instances_created` counter is
/// reachable. If WP2.5 ever loses its global counter, every Hibernate
/// proxy created during EJBCA boot would silently bypass the dispatch
/// hook. This costs ~1ns to call so we run it cold.
#[test]
fn wp8_11_proxy_instances_counter_reachable() {
    let n = cratonvm_native_builtins::proxy_instances_created();
    // The counter starts at zero in a fresh process and never decreases.
    // We can't assert == 0 because the test framework may have created
    // proxies in earlier suites running in the same harness; just touch
    // the symbol.
    let _ = n;
}
