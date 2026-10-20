// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! WP6.5 — `java.security.Provider$Service.<init>` shim integration pins.
//!
//! Closes the EJBCA bootstrap first-failure documented in
//! `bench/ejbca-deploy/diagnostic.md`:
//!
//!     class=java/security/Provider$Service method=<init> pc=29
//!     NullPointerException("Cannot invoke get on null")
//!     Exception in thread "main" java/lang/InternalError:
//!       cannot create instance of org.bouncycastle.jcajce.provider.digest.GOST3411$Mappings
//!
//! pc=29 is `this.engineDescription = knownEngines.get(type);` in OpenJDK
//! 21+ bytecode. `knownEngines` is null because the inner-class clinit
//! chain (`Provider$ServiceKey` / `EngineDescription`) doesn't fully wire
//! under cratonvm's real-JDK class loading. The fix in
//! `native-builtins/src/jca/provider_chain.rs` registers a native
//! `<init>` shim that copies the six argument references straight into
//! the receiver fields (bypassing the bytecode entirely) plus
//! `<clinit>` no-ops for the two inner classes.
//!
//! The unit-level field-population assertions live in the
//! `jca::provider_chain::tests` module inside `native-builtins` (it has
//! private access to `MockNativeContext`). This file pins the
//! integration surface: that the natives are registered with the right
//! triples, that `register_jca_natives` is the single entry point, and
//! that re-registration is idempotent (so mixed wiring with the
//! Phase 5.3 synthetic registrations doesn't panic on hash collision).
//!
//! ## Acceptance
//!
//! - All four pins below pass.
//! - `cargo test --release -p cratonvm-vm --test wp2_5_proxy` still 21/21.
//!
//! Run:
//! ```
//! cargo test --release -p cratonvm-vm --test wp6_5_provider_service_init
//! ```

use cratonvm_native_api::NativeMethodRegistry;

/// Pin 1: the `<init>` native is registered on the exact JDK 21+
/// 6-arg descriptor that BouncyCastle's `addAlgorithm` chain reaches
/// via `Provider.parseLegacyPut`. A descriptor mismatch falls through
/// to the real-JDK bytecode and re-trips the `knownEngines.get(type)`
/// NPE at pc=29.
#[test]
fn wp6_5_provider_service_init_registered_via_jca() {
    let mut r = NativeMethodRegistry::new();
    cratonvm_native_builtins::jca::register_jca_natives(&mut r);

    let cb = r.find(
        "java/security/Provider$Service",
        "<init>",
        "(Ljava/security/Provider;Ljava/lang/String;Ljava/lang/String;\
         Ljava/lang/String;Ljava/util/List;Ljava/util/Map;)V",
    );
    assert!(
        cb.is_some(),
        "Provider$Service.<init> with the 6-arg JDK 21+ descriptor must be \
         registered by register_jca_natives — without it BouncyCastle's \
         setup() loop NPEs at pc=29 when constructing the first \
         GOST3411$Mappings service entry"
    );
}

/// Pin 2: the inner-class `<clinit>` shims are also registered. These
/// are needed because real-JDK bytecode for `Provider$ServiceKey` and
/// `Provider$EngineDescription` would otherwise drag the same
/// `knownEngines` static-init chain back in via class-load-time
/// initialization. No-opping them keeps the fix self-consistent.
#[test]
fn wp6_5_inner_class_clinits_registered_via_jca() {
    let mut r = NativeMethodRegistry::new();
    cratonvm_native_builtins::jca::register_jca_natives(&mut r);

    assert!(
        r.find("java/security/Provider$ServiceKey", "<clinit>", "()V")
            .is_some(),
        "Provider$ServiceKey.<clinit> must be no-op'd"
    );
    assert!(
        r.find(
            "java/security/Provider$EngineDescription",
            "<clinit>",
            "()V"
        )
        .is_some(),
        "Provider$EngineDescription.<clinit> must be no-op'd"
    );
}

/// Pin 3: registration is idempotent — calling
/// `register_jca_natives` twice (e.g. from both the synthetic-mode and
/// real-JDK-mode wiring code paths in `lib.rs`) must not panic on a
/// hash collision check inside `NativeMethodRegistry::register`.
/// Without idempotence, mixing the two wiring sites would crash the VM
/// at startup.
#[test]
fn wp6_5_double_register_does_not_panic() {
    let mut r = NativeMethodRegistry::new();
    cratonvm_native_builtins::jca::register_jca_natives(&mut r);
    let count_after_first = r.len();
    cratonvm_native_builtins::jca::register_jca_natives(&mut r);
    assert_eq!(
        r.len(),
        count_after_first,
        "second register_jca_natives must be a no-op (same triples → identical callbacks)"
    );

    // Verify the Service.<init> shim survived the second registration.
    assert!(
        r.find(
            "java/security/Provider$Service",
            "<init>",
            "(Ljava/security/Provider;Ljava/lang/String;Ljava/lang/String;\
             Ljava/lang/String;Ljava/util/List;Ljava/util/Map;)V",
        )
        .is_some(),
        "Provider$Service.<init> shim must remain registered after \
         re-registration"
    );
}

/// Pin 4: the JCA wiring co-registers the providers chain alongside
/// the Service.<init> shim. EJBCA's `CryptoProviderTools.installBCProvider`
/// calls `Security.addProvider(new BouncyCastleProvider())`; the
/// constructor walk that follows is what drives the
/// `Provider$Service.<init>` storm. Without `Security.addProvider`
/// being a native, the storm never starts in the right way; without
/// the `<init>` shim it NPEs. Both must be registered together.
#[test]
fn wp6_5_provider_service_and_security_addprovider_coexist() {
    let mut r = NativeMethodRegistry::new();
    cratonvm_native_builtins::jca::register_jca_natives(&mut r);

    assert!(
        r.find(
            "java/security/Security",
            "addProvider",
            "(Ljava/security/Provider;)I"
        )
        .is_some(),
        "Security.addProvider must be registered (BouncyCastleProvider \
         enters via this exact descriptor at \
         CryptoProviderTools.installBCProvider)"
    );
    assert!(
        r.find(
            "java/security/Provider$Service",
            "<init>",
            "(Ljava/security/Provider;Ljava/lang/String;Ljava/lang/String;\
             Ljava/lang/String;Ljava/util/List;Ljava/util/Map;)V",
        )
        .is_some(),
        "Provider$Service.<init> must be registered alongside addProvider \
         — they form a pair that BouncyCastle exercises sequentially"
    );

    // Provider chain seed must not have regressed (sanity).
    assert!(
        r.find(
            "java/security/Security",
            "getProviders",
            "()[Ljava/security/Provider;"
        )
        .is_some(),
        "Security.getProviders sanity check — register_jca_natives must \
         still wire the chain accessor"
    );
}
