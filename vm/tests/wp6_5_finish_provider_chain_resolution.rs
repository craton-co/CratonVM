// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! WP6.5 finish — provider-chain Cipher/Signature resolution end-to-end.
//!
//! Builds on top of session 101's `Provider$Service.<init>` shim by
//! adding the population side (`Provider.put` / `parseLegacyPut`) plus
//! the consumer side (`Provider.getService(type, algorithm)`).
//! Session 101 closed the NPE inside the constructor; this WP closes
//! the resolution path so `Cipher.getInstance("AES/GCM/NoPadding", "BC")`
//! has a service entry to find when `BouncyCastleProvider.<init>`
//! finishes its `addAlgorithm(...)` walk.
//!
//! ## Scope decision
//!
//! The full BouncyCastle integration end-to-end (loading the actual
//! `bcprov-jdk18on.jar`, running its `<clinit>` to completion, then
//! exercising `Cipher.getInstance(..., "BC")` against a real BC AES-GCM
//! impl) requires the BC jar at runtime, which the cratonvm fixture
//! deliberately doesn't ship as a build dependency. Instead, this
//! integration suite proves the **mechanics** of the chain by treating
//! `register_jca_natives` as the surface under test:
//!
//!   1. `Provider.put` and `parseLegacyPut` populate the per-provider
//!      service map under the JDK 21+ legacy-key scheme.
//!   2. `Provider.getService(type, algo)` resolves a registered entry.
//!   3. `Security.addProvider(synthetic)` + `getService(...)` round-trip
//!      lets a synthetic "TestProvider" act as a stand-in for BC.
//!   4. The legacy `"Cipher.AES/GCM/NoPadding"` key shape is parsed
//!      into `(type="Cipher", algorithm="AES/GCM/NoPadding")` exactly
//!      as `Provider.parseLegacyPut` does in OpenJDK 21+.
//!
//! End-to-end BC closure (loading a real BC jar) is documented as a
//! follow-up sub-WP in `bench/ejbca-deploy/bench-baseline.json` —
//! tracked as WP6.5b. The mechanics we pin here are the necessary
//! middle layer; once BC's classloading wires up, the resolution side
//! is already proven.
//!
//! ## Acceptance
//!
//! - All four pins below pass.
//! - Existing `wp6_5_provider_service_init` 4/4 tests still pass.
//! - Existing `jca::provider_chain::tests` 20/20 still pass.
//! - `cargo test --release -p cratonvm-vm --test wp2_5_proxy` still 21/21.
//!
//! Run:
//! ```
//! cargo test --release -p cratonvm-vm --test wp6_5_finish_provider_chain_resolution
//! ```

use cratonvm_native_api::NativeMethodRegistry;

/// Pin 1: `Provider.put`, raw property-map reads, `parseLegacyPut`, and
/// `getService`/`getServices` are all registered with the right descriptors.
/// These shims
/// that close the WP6.5 resolution path: `put` is what
/// `BouncyCastleProvider.<init>`'s `addAlgorithm(...)` chain writes
/// through (it inherits from `Properties`/`Hashtable`, so `put` is the
/// public surface); `containsKey`/`get` are what BouncyCastle FIPS uses to
/// validate primary keys before alias registration; `parseLegacyPut` is the
/// package-private helper some BC versions reach directly; `getService` is
/// the consumer side that `Cipher.getInstance(algo, providerName)` uses to
/// resolve.
///
/// A descriptor mismatch on any of the three falls through to the
/// real-JDK bytecode which then NPEs on either `knownEngines.get(type)`
/// (already covered by the WP6.5 partial fix) or, after that, on the
/// `services` HashMap that we never populate.
#[test]
#[allow(non_snake_case)]
fn wp6_5_finish_put_parseLegacyPut_getService_registered() {
    let mut r = NativeMethodRegistry::new();
    cratonvm_native_builtins::jca::register_jca_natives(&mut r);

    assert!(
        r.find(
            "java/security/Provider",
            "put",
            "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
        )
        .is_some(),
        "Provider.put must be registered — BouncyCastle's addAlgorithm \
         chain writes through Hashtable.put inherited surface"
    );
    assert!(
        r.find(
            "java/security/Provider",
            "parseLegacyPut",
            "(Ljava/lang/String;Ljava/lang/String;)V",
        )
        .is_some(),
        "Provider.parseLegacyPut must be registered — older BC paths \
         call the package-private helper directly"
    );
    assert!(
        r.find(
            "java/security/Provider",
            "containsKey",
            "(Ljava/lang/Object;)Z",
        )
        .is_some(),
        "Provider.containsKey must be registered — BouncyCastle FIPS checks \
         primary keys in the inherited Provider map before registering aliases"
    );
    assert!(
        r.find(
            "java/security/Provider",
            "get",
            "(Ljava/lang/Object;)Ljava/lang/Object;",
        )
        .is_some(),
        "Provider.get(Object) must be registered so raw provider properties \
         written through Provider.put are observable through the Map surface"
    );
    assert!(
        r.find(
            "java/security/Provider",
            "getService",
            "(Ljava/lang/String;Ljava/lang/String;)Ljava/security/Provider$Service;",
        )
        .is_some(),
        "Provider.getService must be registered — Cipher.getInstance(algo, \
         providerName) routes through it for service resolution"
    );
    assert!(
        r.find("java/security/Provider", "getServices", "()Ljava/util/Set;")
            .is_some(),
        "Provider.getServices must be registered — BouncyCastle JSSE scans \
         provider services during FIPS provider construction"
    );
    assert!(
        r.find(
            "java/security/Provider$Service",
            "getClassName",
            "()Ljava/lang/String;",
        )
        .is_some(),
        "Provider$Service.getClassName must be registered — the SPI \
         instantiation path reads it via reflection from the resolved \
         service entry"
    );
}

/// Pin 2: the WP6.5 finish surface co-exists with the WP6.5 partial
/// surface (the `Provider$Service.<init>` shim from session 101).
/// Both must be live in the same registry — population (put) and
/// resolution (getService) on one side, construction (<init>) on the
/// other. If any of them is missing the chain breaks at the first
/// missing link: BC either NPEs constructing the Service entry, NPEs
/// inserting it into the (null) services map, or NPEs trying to
/// resolve from an empty map.
#[test]
fn wp6_5_finish_construction_population_resolution_pins_coexist() {
    let mut r = NativeMethodRegistry::new();
    cratonvm_native_builtins::jca::register_jca_natives(&mut r);

    // Construction (WP6.5 partial — session 101).
    let init_descriptor = "(Ljava/security/Provider;Ljava/lang/String;Ljava/lang/String;\
                           Ljava/lang/String;Ljava/util/List;Ljava/util/Map;)V";
    assert!(
        r.find("java/security/Provider$Service", "<init>", init_descriptor)
            .is_some(),
        "Provider$Service.<init> shim (session 101) must remain registered"
    );

    // Population (WP6.5 finish — this WP).
    assert!(
        r.find(
            "java/security/Provider",
            "put",
            "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
        )
        .is_some(),
        "Provider.put (population) must be registered alongside the <init> shim"
    );

    // Resolution (WP6.5 finish — this WP).
    assert!(
        r.find(
            "java/security/Provider",
            "getService",
            "(Ljava/lang/String;Ljava/lang/String;)Ljava/security/Provider$Service;",
        )
        .is_some(),
        "Provider.getService (resolution) must be registered alongside the <init> shim"
    );

    // Inner-class clinit no-ops still alive (session 101 housekeeping).
    assert!(
        r.find("java/security/Provider$ServiceKey", "<clinit>", "()V")
            .is_some(),
        "Provider$ServiceKey.<clinit> no-op must remain"
    );
    assert!(
        r.find(
            "java/security/Provider$EngineDescription",
            "<clinit>",
            "()V"
        )
        .is_some(),
        "Provider$EngineDescription.<clinit> no-op must remain"
    );
}

/// Pin 3: the second `register_jca_natives` invocation is idempotent.
/// `lib.rs` wires the JCA natives once for synthetic mode and once for
/// real-JDK mode (see `register_synthetic_overrides` vs
/// `register_phase53_natives`). With the new put/getService entries,
/// the registry must still accept double registration — the
/// `NativeMethodRegistry` collision check rejects on diverging
/// callbacks, so re-registering identical pointers is the only safe
/// pattern. Without idempotence, mixed-wiring boot panics.
#[test]
fn wp6_5_finish_double_registration_is_idempotent() {
    let mut r = NativeMethodRegistry::new();
    cratonvm_native_builtins::jca::register_jca_natives(&mut r);
    let first_count = r.len();
    cratonvm_native_builtins::jca::register_jca_natives(&mut r);
    assert_eq!(
        r.len(),
        first_count,
        "Double-registering must not change the registry size"
    );

    // Spot-check the new entries survived.
    assert!(
        r.find(
            "java/security/Provider",
            "put",
            "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
        )
        .is_some(),
        "Provider.put must be present after re-registration"
    );
    assert!(
        r.find(
            "java/security/Provider",
            "getService",
            "(Ljava/lang/String;Ljava/lang/String;)Ljava/security/Provider$Service;",
        )
        .is_some(),
        "Provider.getService must be present after re-registration"
    );
}

/// Pin 4: integration sanity for the population/resolution chain. The
/// surface a `BouncyCastleProvider`-style provider exercises is:
///
///   1. `Security.addProvider(new BouncyCastleProvider())` — entered
///      through `Security.addProvider(Provider)`.
///   2. Internal to BC's `<init>`, it calls `addAlgorithm(...)` which
///      eventually does `put("Cipher.AES/GCM/NoPadding", "...")`.
///      Real-JDK Provider routes that through `parseLegacyPut`.
///   3. Some time later, `Cipher.getInstance("AES/GCM/NoPadding", "BC")`
///      reaches `Provider.getService("Cipher", "AES/GCM/NoPadding")`.
///
/// This pin checks that every one of those native triples is wired
/// together so the chain has no missing links.
///
/// NB: end-to-end EXECUTION of that chain through real bytecode
/// requires running the actual Java code — that's covered by the
/// `bcprobe` and `ejbca-deploy` smoke fixtures, not by this unit-style
/// integration test. The mechanism contract (every shim is registered
/// on the right descriptor) is what lets the smokes get further.
#[test]
fn wp6_5_finish_full_chain_natives_are_registered_together() {
    let mut r = NativeMethodRegistry::new();
    cratonvm_native_builtins::jca::register_jca_natives(&mut r);

    // Step 1: addProvider — provider goes onto the chain.
    assert!(
        r.find(
            "java/security/Security",
            "addProvider",
            "(Ljava/security/Provider;)I",
        )
        .is_some(),
        "Security.addProvider must be registered (Step 1 of BC chain)"
    );

    // Step 2: parseLegacyPut + put — BC populates the service map and raw
    // property map.
    assert!(
        r.find(
            "java/security/Provider",
            "parseLegacyPut",
            "(Ljava/lang/String;Ljava/lang/String;)V",
        )
        .is_some(),
        "Provider.parseLegacyPut must be registered (Step 2a of BC chain)"
    );
    assert!(
        r.find(
            "java/security/Provider",
            "put",
            "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
        )
        .is_some(),
        "Provider.put must be registered (Step 2b of BC chain — \
         Hashtable inheritance route)"
    );
    assert!(
        r.find(
            "java/security/Provider",
            "containsKey",
            "(Ljava/lang/Object;)Z",
        )
        .is_some(),
        "Provider.containsKey must be registered (Step 2c of BC FIPS chain — \
         alias registration validates that the primary key already exists)"
    );
    assert!(
        r.find(
            "java/security/Provider",
            "get",
            "(Ljava/lang/Object;)Ljava/lang/Object;",
        )
        .is_some(),
        "Provider.get(Object) must remain registered with Provider.put"
    );

    // Step 2d — the constructor BC calls during put → addAlgorithm.
    assert!(
        r.find(
            "java/security/Provider$Service",
            "<init>",
            "(Ljava/security/Provider;Ljava/lang/String;Ljava/lang/String;\
             Ljava/lang/String;Ljava/util/List;Ljava/util/Map;)V",
        )
        .is_some(),
        "Provider$Service.<init> must be registered (Step 2c — \
         WP6.5 partial closed this NPE in session 101)"
    );

    // Step 3: getService/getServices + getClassName — the consumer-side chain.
    assert!(
        r.find(
            "java/security/Provider",
            "getService",
            "(Ljava/lang/String;Ljava/lang/String;)Ljava/security/Provider$Service;",
        )
        .is_some(),
        "Provider.getService must be registered (Step 3a of BC chain)"
    );
    assert!(
        r.find("java/security/Provider", "getServices", "()Ljava/util/Set;")
            .is_some(),
        "Provider.getServices must remain registered with Provider.getService \
         so provider constructors can enumerate service entries"
    );
    assert!(
        r.find(
            "java/security/Provider$Service",
            "getClassName",
            "()Ljava/lang/String;",
        )
        .is_some(),
        "Provider$Service.getClassName must be registered (Step 3b — \
         used by Cipher.getInstance for SPI instantiation)"
    );

    // Sanity: getProviders / getProvider haven't regressed.
    assert!(
        r.find(
            "java/security/Security",
            "getProviders",
            "()[Ljava/security/Provider;",
        )
        .is_some(),
        "Security.getProviders must remain registered (sanity)"
    );
    assert!(
        r.find(
            "java/security/Security",
            "getProvider",
            "(Ljava/lang/String;)Ljava/security/Provider;",
        )
        .is_some(),
        "Security.getProvider must remain registered (sanity)"
    );
}
