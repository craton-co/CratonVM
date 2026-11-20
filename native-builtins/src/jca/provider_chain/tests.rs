use super::*;
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};

// gc-common w22-b: the registry accessors take a VM identity now. These
// tests drive the mock, whose identity is `TEST_VM`, so the forms below
// shadow the module's (a local item wins over the `super::*` glob) and
// pin every direct call to that one registry.
fn snapshot() -> Vec<ChainEntry> {
    super::snapshot(TEST_VM)
}
fn find(name: &str) -> Option<(f64, &'static str)> {
    super::find(TEST_VM, name)
}
fn is_unbacked_provider(name: &str) -> bool {
    super::is_unbacked_provider(TEST_VM, name)
}
// No `add` / `insert_at` / `remove` forms: a test that changes a chain
// does it on a private VM (see `ForgetRegistry`), never on `TEST_VM`,
// whose whole chain `seed_chain_matches_the_platform_jdk25_provider_list`
// asserts.
fn get_service_entry(provider: &str, type_str: &str, algo: &str) -> Option<ServiceEntry> {
    super::get_service_entry(TEST_VM, provider, type_str, algo)
}
fn apply_legacy_put(provider: &str, key: &str, value: &str) -> bool {
    with_registry(TEST_VM, |reg| reg.apply_legacy_put(provider, key, value))
}
fn canonical_service_algorithm(
    provider: Option<&str>,
    type_str: &str,
    algo: &str,
) -> Option<String> {
    super::canonical_service_algorithm(TEST_VM, provider, type_str, algo)
}
fn find_service_provider(type_str: &str, algo: &str) -> Option<String> {
    super::find_service_provider_in(TEST_VM, type_str, algo)
}
fn service_implementation_class(type_str: &str, provider: &str, algo: &str) -> Option<String> {
    super::service_implementation_class(TEST_VM, type_str, provider, algo)
}
fn ssl_context_protocol_supported(protocol: &str) -> bool {
    super::ssl_context_protocol_supported_in(TEST_VM, protocol)
}
fn projected_registry_rows(name: &str) -> Vec<(String, String)> {
    read_registry(TEST_VM, |reg| reg.projected_registry_rows(name))
}
fn seed_direct_native_engine_services() {
    with_registry(TEST_VM, super::seed_direct_native_engine_services)
}
fn seed_sun_dsa_services() {
    with_registry(TEST_VM, super::seed_sun_dsa_services)
}
fn seed_sunjsse_services() {
    with_registry(TEST_VM, super::seed_sunjsse_services)
}
fn seed_sunjce_delegated_cipher_services() {
    with_registry(TEST_VM, super::seed_sunjce_delegated_cipher_services)
}
fn seed_sunjce_pbe_mac_services() {
    with_registry(TEST_VM, super::seed_sunjce_pbe_mac_services)
}
/// A copy of [`TEST_VM`]'s service map.
fn test_services() -> FxHashMap<String, ServiceMap> {
    read_registry(TEST_VM, |reg| reg.services.clone())
}

/// The seed chain is PER-PLATFORM: `SunMSCAPI` wraps the Windows CryptoAPI
/// and ships only in the Windows JDK, so HotSpot 25 answers thirteen
/// providers there and twelve everywhere else. This test asserted a flat
/// thirteen and so encoded a Windows capture as universal — measured
/// against `java -version 25.0.3` on Linux, which lists exactly the twelve
/// below.
#[test]
fn seed_chain_matches_the_platform_jdk25_provider_list() {
    let chain = snapshot();
    let names: Vec<&str> = chain.iter().map(|(n, _, _)| n.as_str()).collect();
    let mut expected = vec![
        "SUN",
        "SunRsaSign",
        "SunEC",
        "SunJSSE",
        "SunJCE",
        "SunJGSS",
        "SunSASL",
        "XMLDSig",
        "SunPCSC",
        "JdkLDAP",
        "JdkSASL",
    ];
    if cfg!(target_os = "windows") {
        expected.push("SunMSCAPI");
    }
    expected.push("SunPKCS11");
    // The whole ordered list, not a length plus three spot-checks: order is
    // what provider selection walks, so a chain that is the right length
    // with two entries swapped resolves algorithms to the wrong provider
    // and still passes a spot-check.
    assert_eq!(names, expected);
}

// -----------------------------------------------------------------
// C19 — coverage disclosure for unbacked providers.
// -----------------------------------------------------------------

#[test]
fn c19_seed_chain_marks_unbacked_providers() {
    // Only providers with no registered service surface stay placeholders.
    for unbacked in [
        "SunPKCS11",
        "SunPCSC",
        "JdkLDAP",
        "JdkSASL",
        "SunJGSS",
        "SunSASL",
        "XMLDSig",
    ] {
        assert!(
            is_unbacked_provider(unbacked),
            "{unbacked} must be flagged as unbacked so getService can log a hint"
        );
        let (_, cov) = find(unbacked).expect("seed provider present");
        assert_eq!(
            cov, COVERAGE_UNBACKED,
            "{unbacked} coverage string must be the canonical unbacked-disclosure constant"
        );
    }
    // `SunMSCAPI` is deliberately absent from the seed chain off Windows
    // (it wraps CryptoAPI and ships only in the Windows JDK), so asserting
    // it is present would be a gate that fails on the platform where the
    // right answer is "not there".
    let mut backed_names = vec!["SUN", "SunRsaSign", "SunJCE", "SunEC", "SunJSSE"];
    if cfg!(target_os = "windows") {
        backed_names.push("SunMSCAPI");
    } else {
        assert!(
            find("SunMSCAPI").is_none(),
            "SunMSCAPI must NOT be seeded off Windows: HotSpot's own \
             Security.getProviders() does not carry it there"
        );
    }
    for backed in backed_names {
        assert!(
            !is_unbacked_provider(backed),
            "{backed} backs at least one algorithm and must not be flagged unbacked"
        );
        let (_, cov) = find(backed).expect("seed provider present");
        assert!(
            cov.starts_with("coverage: ") && cov != COVERAGE_UNBACKED,
            "{backed} coverage must describe what is actually wired, got: {cov}"
        );
    }
}

// gc-common w25-b: the three tests below ADD to a provider chain, and
// `seed_chain_matches_the_platform_jdk25_provider_list` asserts
// `TEST_VM`'s whole ordered chain. Tests run in parallel, so a provider
// one of them had added to `TEST_VM` and not yet removed failed the seed
// test. Each chain-mutating test now owns a private VM (a fresh row is
// the seed chain) and drops it on exit, even when an assertion fails.
const W25B_USER_COV_VM: usize = 0x0B25_0108;
const W25B_IDEMPOTENT_VM: usize = 0x0B25_0208;
const W25B_ROUND_TRIP_VM: usize = 0x0B25_0308;

/// Drops one test's private provider registry.
struct ForgetRegistry(usize);
impl Drop for ForgetRegistry {
    fn drop(&mut self) {
        super::forget_vm_provider_registry(self.0);
    }
}

#[test]
fn c19_user_added_provider_carries_user_coverage_string() {
    let vm = W25B_USER_COV_VM;
    let _forget = ForgetRegistry(vm);
    // `Security.addProvider(...)` lands here; we don't introspect
    // the caller's Service map, so the disclosure says exactly that.
    let name = format!("__jca_test_user_cov_{}", std::process::id());
    let pos = super::add(vm, name.clone(), 7.5);
    assert!(pos >= 1);
    let (ver, cov) = super::find(vm, &name).expect("user-added provider visible to find()");
    assert_eq!(ver, 7.5);
    assert_eq!(cov, USER_PROVIDER_COVERAGE);
    // User-added providers are NOT seed unbacked entries — the
    // unbacked flag is set by string-equality against
    // COVERAGE_UNBACKED, so USER_PROVIDER_COVERAGE must not match.
    assert!(!super::is_unbacked_provider(vm, &name));
    super::remove(vm, &name);
}

#[test]
fn add_provider_idempotent() {
    let vm = W25B_IDEMPOTENT_VM;
    let _forget = ForgetRegistry(vm);
    // Count only `SUN` rows (a private VM's chain, but the count is what
    // the test is about).
    let suns = || {
        super::snapshot(vm)
            .iter()
            .filter(|(n, _, _)| n == "SUN")
            .count()
    };
    let before = suns();
    // Adding an existing name must not duplicate.
    let pos = super::add(vm, "SUN".to_string(), 25.0);
    // -1, not the position. `Security.addProvider` answers the
    // preference position it installed at, or -1 when a provider of
    // that name is ALREADY installed -- so a position here is
    // indistinguishable from a successful install at 1, and a caller
    // guarding on `>= 0` concludes it registered when it did not.
    // This assertion previously recorded what the code did rather
    // than what the JDK does; `L6JcaSweep` row 193 measures it
    // against a real JDK and the oracle says -1.
    assert_eq!(pos, -1, "re-adding an installed provider answers -1");
    assert_eq!(suns(), before);
}

#[test]
fn insert_then_remove_round_trip() {
    let vm = W25B_ROUND_TRIP_VM;
    let _forget = ForgetRegistry(vm);
    let unique_name = format!("__jca_test_{}", std::process::id());
    let pos = super::insert_at(vm, unique_name.clone(), 42.0, 5);
    assert!(pos >= 1);
    assert!(super::find(vm, &unique_name).is_some());
    super::remove(vm, &unique_name);
    assert!(super::find(vm, &unique_name).is_none());
}

// -----------------------------------------------------------------
// WP6.5: Provider$Service.<init> shim — verify the constructor
// populates all six instance fields without invoking real bytecode
// (no `knownEngines.get` lookup, no NPE).
// -----------------------------------------------------------------

use crate::test_utils::MockNativeContext;

// -----------------------------------------------------------------
// G43-1 — `make_provider`'s legacy slot mirror is synthetic-mode-only.
//
// The two arms below are the whole decision. Under `--real-jdk` the
// receiver is a real `java.security.Provider`, whose slots 0/1/2 are the
// inherited `Hashtable.table` / `count` / `threshold`; the slot-2 write
// published a heap ADDRESS into an `I` field, MEASURED twice per `RCrypto`
// run by `CRATONVM_DBG_COERCION=1` as species `pointer-into-primitive` at
// `provider_chain.rs:317`. Under `--synthetic-jdk` the same three slots
// ARE name/version/info and `provider_get_name` / `provider_get_version` /
// `provider_get_info` read them.
// -----------------------------------------------------------------

/// Declare `java.security.Provider`'s OWN instance fields at slots that do
/// not overlap the legacy mirror, which is what a real-JDK layout looks
/// like from a native's point of view: the named writes resolve, and 0/1/2
/// belong to somebody else (`Hashtable`).
fn declare_real_provider_layout(ctx: &MockNativeContext, cid: cratonvm_types::ClassId) {
    let fields = ["name", "info", "version", "versionStr", "initialized"]
        .iter()
        .enumerate()
        .map(|(i, n)| cratonvm_native_api::FieldMetadata {
            name: (*n).to_string(),
            descriptor: "Ljava/lang/Object;".to_string(),
            access_flags: 0,
            // Slot 3 upward — 0/1/2 are the inherited Hashtable fields.
            slot_index: i + 3,
            declaring_class_id: cid,
            is_static: false,
        })
        .collect();
    ctx.set_declared_fields(cid, fields);
}

/// i7-L2: both `getProviders` natives return `Provider[]`, not `Object[]`
/// (JDK code clones and `checkcast`s it), and every slot is filled.
#[test]
fn i7_l2_get_providers_returns_a_provider_typed_array() {
    let mut ctx = MockNativeContext::new();
    let provider = ctx
        .ensure_class_initialized("java/security/Provider")
        .expect("mock registers the class");
    let arr = match security_get_providers(&mut ctx, &[]) {
        Ok(Some(Value::Object(Some(arr)))) => arr,
        other => panic!("getProviders() must answer an array, got {other:?}"),
    };
    assert_eq!(ctx.ref_array_component(arr), Some(provider));
    for i in 0..ctx.array_length(arr) {
        assert!(
            matches!(ctx.get_array_element(arr, i), Value::Object(Some(_))),
            "slot {i} of getProviders() is null"
        );
    }
}

#[test]
fn g43_1_make_provider_writes_the_legacy_slots_only_in_synthetic_mode() {
    let mut ctx = MockNativeContext::new();
    // No declared layout → every `set_field_by_name` for a Provider-only
    // name is a no-op, which is exactly what production does when the real
    // class has no bytes. `versionStr` cannot read back, so the mirror runs.
    let p = make_provider(&mut ctx, "SUN", 25.0, "test coverage").expect("alloc");

    assert!(
        matches!(ctx.get_field(p, 0), Value::Object(Some(_))),
        "synthetic slot 0 must carry the name String (provider_get_name reads it)"
    );
    assert_eq!(
        ctx.get_field(p, 1),
        Value::Double(25.0),
        "synthetic slot 1 must carry the numeric version (provider_get_version reads it)"
    );
    assert!(
        matches!(ctx.get_field(p, 2), Value::Object(Some(_))),
        "synthetic slot 2 must carry the info String (provider_get_info reads it)"
    );
    // The two Strings are distinct objects — a mirror that wrote the same
    // ref twice would satisfy the two assertions above vacuously.
    assert_ne!(
        ctx.get_field(p, 0),
        ctx.get_field(p, 2),
        "name and info must be different objects"
    );
}

#[test]
fn g43_1_make_provider_does_not_publish_an_address_into_a_real_provider_int_slot() {
    let mut ctx = MockNativeContext::new();
    let cid = ctx
        .ensure_class_initialized("java/security/Provider")
        .expect("mock registers the class");
    declare_real_provider_layout(&ctx, cid);

    let p = make_provider(&mut ctx, "SUN", 25.0, "test coverage").expect("alloc");

    // The named writes landed where the class says they go...
    assert!(
        matches!(ctx.get_field_by_name(p, "name"), Value::Object(Some(_))),
        "real-layout `name` must be written by name"
    );
    assert!(
        matches!(
            ctx.get_field_by_name(p, "versionStr"),
            Value::Object(Some(_))
        ),
        "real-layout `versionStr` must be written by name"
    );
    assert!(
        matches!(ctx.get_field_by_name(p, "info"), Value::Object(Some(_))),
        "real-layout `info` must be written by name"
    );

    // ...and the legacy mirror did NOT run. Slot 2 is `Hashtable.threshold:I`
    // on a real Provider; a reference there is the `pointer-into-primitive`
    // coercion, and slot 0 is `Hashtable.table`, whose occupant a live
    // `Hashtable.keys()` walks as an `Entry[]`.
    assert!(
        !matches!(ctx.get_field(p, 2), Value::Object(Some(_))),
        "slot 2 is Hashtable.threshold on a real Provider — writing a \
         reference there is the pointer-into-primitive coercion this fixes"
    );
    assert!(
        !matches!(ctx.get_field(p, 0), Value::Object(Some(_))),
        "slot 0 is Hashtable.table on a real Provider — a String there is \
         walked as an Entry[] by any live keys()/entrySet()"
    );
    assert_ne!(
        ctx.get_field(p, 1),
        Value::Double(25.0),
        "slot 1 is Hashtable.count on a real Provider — a Double there \
         decodes to the low half of its IEEE-754 bit pattern"
    );
}

#[test]
fn g43_1_provider_get_info_never_returns_a_primitive_from_the_slot_fallback() {
    // `provider_get_info`'s descriptor is `()Ljava/lang/String;`. On a real
    // Provider slot 2 is an `int`, and an unwritten reference slot reads
    // back as `Int(0)` besides — either way, handing the interpreter an
    // `Int` from a reference-returning native is a type error at the call
    // site, so the fallback must degrade to null instead.
    let mut ctx = MockNativeContext::new();
    let p = ctx.alloc_object(cratonvm_types::ClassId::new(0), 8);
    ctx.set_field(p, 2, Value::Int(12));

    let got = provider_get_info(&mut ctx, &[Value::Object(Some(p))])
        .expect("getInfo must not fail")
        .expect("getInfo returns a value");
    assert_eq!(
        got,
        Value::Object(None),
        "an `I` slot must not become a String"
    );

    // The reference case still passes through unchanged.
    let info = ctx.create_string("SUN security provider");
    ctx.set_field(p, 2, Value::Object(Some(info)));
    let got = provider_get_info(&mut ctx, &[Value::Object(Some(p))])
        .expect("getInfo must not fail")
        .expect("getInfo returns a value");
    assert_eq!(got, Value::Object(Some(info)));
}

/// Helper: allocate a synthetic Provider$Service heap entry sized
/// large enough for the slot fallback (synthetic getters at 0/1/2)
/// plus three more slots so `set_field_by_name` calls the production
/// code makes don't get rejected by `mock_jdk_field_slot` for unknown
/// field names (the mock silently ignores unknown names).
fn alloc_service(ctx: &mut MockNativeContext) -> ObjectRef {
    // Synthetic mode: alloc 7 slots (0..6) so writes via set_field
    // never overflow.
    ctx.alloc_object(cratonvm_types::ClassId::new(0), 7)
}

#[test]
fn wp6_5_provider_service_init_registered_with_jdk21_descriptor() {
    // Acceptance pin #1: the `<init>` shim must be registered on the
    // exact 6-arg descriptor that BouncyCastle's `addAlgorithm` call
    // chain reaches via `parseLegacyPut`. A descriptor mismatch would
    // silently fall through to the real-JDK bytecode and re-trigger
    // the `knownEngines.get(type)` NPE at pc=29.
    let mut r = NativeMethodRegistry::new();
    register(&mut r);
    let cb = r.find(
        "java/security/Provider$Service",
        "<init>",
        "(Ljava/security/Provider;Ljava/lang/String;Ljava/lang/String;\
         Ljava/lang/String;Ljava/util/List;Ljava/util/Map;)V",
    );
    assert!(
        cb.is_some(),
        "Provider$Service.<init> with the JDK 21+ 6-arg descriptor must be registered"
    );

    // Defence: register() must be idempotent — re-registering the
    // same triple from a sibling call site would otherwise panic on
    // a hash collision check.
    register(&mut r);
    assert!(r
        .find(
            "java/security/Provider$Service",
            "<init>",
            "(Ljava/security/Provider;Ljava/lang/String;Ljava/lang/String;\
             Ljava/lang/String;Ljava/util/List;Ljava/util/Map;)V",
        )
        .is_some());
}

#[test]
fn wp6_5_provider_service_init_populates_synthetic_slots() {
    // The synthetic-mode `Provider$Service` getters declared in
    // `phases_early::register_phase53_security` read from slots
    // 0/1/2 → (type, algorithm, provider). Verify the shim writes
    // those slots so subsequent `getType()` / `getAlgorithm()` /
    // `getProvider()` calls return the right references.
    let mut ctx = MockNativeContext::new();
    let this = alloc_service(&mut ctx);
    let provider = ctx.create_string("BC");
    let svc_type = ctx.create_string("MessageDigest");
    let algorithm = ctx.create_string("SHA-256");
    let class_name = ctx.create_string("org.bouncycastle.jcajce.provider.digest.SHA256$Digest");
    let aliases = ctx.new_array(cratonvm_types::ArrayElementType::Reference, 0);
    let attributes = ctx.new_array(cratonvm_types::ArrayElementType::Reference, 0);

    let args = [
        Value::Object(Some(this)),
        Value::Object(Some(provider)),
        Value::Object(Some(svc_type)),
        Value::Object(Some(algorithm)),
        Value::Object(Some(class_name)),
        Value::Object(Some(aliases)),
        Value::Object(Some(attributes)),
    ];
    let res = provider_service_init(&mut ctx, &args);
    assert!(res.is_ok(), "shim must not error");
    assert!(matches!(res.unwrap(), None), "constructor returns void");

    // Synthetic-mode slot mirror: type=0, algorithm=1, provider=2.
    assert!(
        matches!(ctx.get_field(this, 0), Value::Object(Some(o)) if o == svc_type),
        "slot 0 must hold the `type` argument (synthetic getType())"
    );
    assert!(
        matches!(ctx.get_field(this, 1), Value::Object(Some(o)) if o == algorithm),
        "slot 1 must hold the `algorithm` argument (synthetic getAlgorithm())"
    );
    assert!(
        matches!(ctx.get_field(this, 2), Value::Object(Some(o)) if o == provider),
        "slot 2 must hold the `provider` argument (synthetic getProvider())"
    );
    assert!(
        matches!(ctx.get_field(this, 3), Value::Object(Some(o)) if o == class_name),
        "slot 3 must hold the `className` argument (synthetic getClassName())"
    );
    let sk = provider_object_key(&ctx, this);
    assert_eq!(
        service_classname_table()
            .lock()
            .get(&sk)
            .map(String::as_str),
        Some("org.bouncycastle.jcajce.provider.digest.SHA256$Digest")
    );
}

#[test]
fn wp6_5_provider_service_init_does_not_npe_on_null_engines() {
    // The whole point of the shim: the constructor must NOT consult
    // `knownEngines` (which would NPE because the `Provider.<clinit>`
    // chain doesn't fully populate it under real-JDK class loading
    // here). Calling the shim with all-null aux arguments — the
    // pathological case BouncyCastle's `parseLegacyPut` produces
    // when an attribute map is empty — must return Ok(None) without
    // touching any static state.
    let mut ctx = MockNativeContext::new();
    let this = alloc_service(&mut ctx);
    let svc_type = ctx.create_string("Cipher");
    let algorithm = ctx.create_string("AES/GCM/NoPadding");

    let args = [
        Value::Object(Some(this)),
        Value::Object(None), // null provider
        Value::Object(Some(svc_type)),
        Value::Object(Some(algorithm)),
        Value::Object(None), // null className
        Value::Object(None), // null aliases
        Value::Object(None), // null attributes
    ];
    let res = provider_service_init(&mut ctx, &args);
    assert!(
        res.is_ok(),
        "shim must accept null aux args (parseLegacyPut produces these)"
    );

    // type and algorithm must still land in the synthetic slots.
    assert!(matches!(ctx.get_field(this, 0), Value::Object(Some(o)) if o == svc_type));
    assert!(matches!(ctx.get_field(this, 1), Value::Object(Some(o)) if o == algorithm));
    // Provider slot is null — that's OK, the BC chain only consults
    // `getType` / `getAlgorithm` for the cache key.
    assert!(matches!(ctx.get_field(this, 2), Value::Object(None)));
}

#[test]
fn wp6_5_provider_service_init_throws_on_null_this() {
    // Defensive: a null receiver must surface a NullPointerException
    // (via `obj_arg`'s contract), not panic or silently succeed.
    // BouncyCastle never produces this case but a regression in
    // bytecode dispatch could hand us a null `this`.
    let mut ctx = MockNativeContext::new();
    let args = [
        Value::Object(None), // null this
        Value::Object(None),
        Value::Object(None),
        Value::Object(None),
        Value::Object(None),
        Value::Object(None),
        Value::Object(None),
    ];
    let res = provider_service_init(&mut ctx, &args);
    assert!(
        res.is_err(),
        "null `this` must produce a NullPointerException, not silently succeed"
    );
}

#[test]
fn wp6_5_inner_class_clinits_registered() {
    // The inner-class clinit no-ops are part of the same fix:
    // without them, the JVM walks `Provider$ServiceKey.<clinit>` and
    // `Provider$EngineDescription.<clinit>` whose real bytecode
    // touches the same `knownEngines` map indirectly. No-opping
    // them keeps the fix self-consistent.
    let mut r = NativeMethodRegistry::new();
    register(&mut r);
    assert!(
        r.find("java/security/Provider$ServiceKey", "<clinit>", "()V")
            .is_some(),
        "Provider$ServiceKey.<clinit> must be no-op'd alongside the Service ctor shim"
    );
    assert!(
        r.find(
            "java/security/Provider$EngineDescription",
            "<clinit>",
            "()V"
        )
        .is_some(),
        "Provider$EngineDescription.<clinit> must be no-op'd alongside the Service ctor shim"
    );
}

// -----------------------------------------------------------------
// WP6.5 finish — service map population + lookup.
//
// These tests pin the unit-level behaviour of the parseLegacyPut
// and getService chain: that "Cipher.AES/GCM/NoPadding"-style keys
// crack into (Cipher, AES/GCM/NoPadding), that the entry lands in
// the per-provider service map, and that getService returns it
// case-insensitively.  Higher-level integration is in
// `vm/tests/wp6_5_finish_provider_chain_resolution.rs`.
//
// Each test calls `reset_service_state_for_tests()` first so global
// state from sibling tests doesn't bleed in.
// -----------------------------------------------------------------

#[test]
fn parse_legacy_key_primary_form() {
    let parsed = parse_legacy_key("Cipher.AES/GCM/NoPadding").unwrap();
    assert_eq!(parsed.0, "primary");
    assert_eq!(parsed.1, "Cipher");
    assert_eq!(parsed.2, "AES/GCM/NoPadding");
    assert!(parsed.3.is_none());
}

#[test]
fn parse_legacy_key_alias_form() {
    let parsed = parse_legacy_key("Alg.Alias.Cipher.AES").unwrap();
    assert_eq!(parsed.0, "alias");
    assert_eq!(parsed.1, "Cipher");
    assert_eq!(parsed.2, "AES");
    assert!(parsed.3.is_none());
}

#[test]
fn parse_legacy_key_attribute_form() {
    // "Type.Algorithm AttrName" — attr lands in the optional 4th tuple slot.
    let parsed = parse_legacy_key("Cipher.AES SupportedModes").unwrap();
    assert_eq!(parsed.0, "attr");
    assert_eq!(parsed.1, "Cipher");
    assert_eq!(parsed.2, "AES");
    assert_eq!(parsed.3.as_deref(), Some("SupportedModes"));
}

#[test]
fn parse_legacy_key_rejects_non_service_keys() {
    // Empty + dot-only strings must not parse — they'd otherwise
    // pollute the service map with bogus entries.
    assert!(parse_legacy_key("").is_none());
    assert!(parse_legacy_key(".").is_none());
    // A no-dot key (no Type.Algorithm separator) must reject.
    assert!(parse_legacy_key("UnstructuredKey").is_none());
    // An incomplete alias ("Alg.Alias.Cipher" with no third segment)
    // must reject — the OpenJDK helper requires both type and alias.
    assert!(parse_legacy_key("Alg.Alias.Cipher").is_none());
}

#[test]
fn put_service_round_trip_via_apply_legacy_put() {
    let _lock = reset_service_state_for_tests();
    let ok = apply_legacy_put(
        "TestProvider",
        "Cipher.AES/GCM/NoPadding",
        "com.example.TestAesGcm",
    );
    assert!(ok, "primary key must populate the service map");
    let entry = get_service_entry("TestProvider", "Cipher", "AES/GCM/NoPadding").unwrap();
    assert_eq!(entry.type_str, "Cipher");
    assert_eq!(entry.algorithm, "AES/GCM/NoPadding");
    assert_eq!(entry.class_name, "com.example.TestAesGcm");
}

#[test]
fn get_service_entry_is_case_insensitive() {
    let _lock = reset_service_state_for_tests();
    apply_legacy_put("BC", "Cipher.AES/GCM/NoPadding", "org.bc.AESGCM");
    // Lower-case lookup must hit.
    assert!(get_service_entry("BC", "cipher", "aes/gcm/nopadding").is_some());
    // Mixed case must hit.
    assert!(get_service_entry("BC", "CIPHER", "AES/GCM/nopadding").is_some());
    // Wrong provider name does NOT hit (provider names are stable).
    assert!(get_service_entry("SUN", "Cipher", "AES/GCM/NoPadding").is_none());
}

#[test]
fn direct_native_engine_seed_tracks_real_provider_ownership() {
    let _lock = reset_service_state_for_tests();
    seed_direct_native_engine_services();
    assert!(get_service_entry("SunRsaSign", "KeyFactory", "RSA").is_some());
    assert!(get_service_entry("SUN", "KeyFactory", "RSA").is_none());
    assert!(get_service_entry("SunRsaSign", "Signature", "SHA256withRSA").is_some());
    assert!(get_service_entry("SUN", "SecureRandom", "SHA1PRNG").is_some());
    assert!(get_service_entry("SunJCE", "Cipher", "AES").is_some());
    assert!(get_service_entry("SUN", "Cipher", "AES").is_none());
    assert!(get_service_entry("SunJCE", "Cipher", "AESWrap").is_some());
}

/// W7-15 ratchet: every `Cipher` transformation this provider ADVERTISES
/// must be one `jca::cipher` can actually COMPUTE.
///
/// The census that opened this lane found the two lists disagreeing in both
/// directions simultaneously — `ChaCha20` / `ChaCha20-Poly1305` advertised
/// and silently served as AES-256-ECB, `AES/KW/PKCS5Padding` and
/// `AES/KWP/NoPadding` advertised and served by nothing, `DES` / `DESede`
/// computed correctly and never advertised at all. Both directions are
/// defects and only one of them is loud: an advertised-but-absent algorithm
/// raises at `getInstance`, while a served-but-unadvertised one is
/// whatever the default arm felt like doing.
///
/// A census run by hand drifts again by the next wave. This is the same
/// closure the bridge-wave population used: the measurement becomes a test,
/// so the next person to add a name to either list has to add it to both.
#[test]
fn every_advertised_sunjce_cipher_is_serviceable() {
    let _lock = reset_service_state_for_tests();
    seed_direct_native_engine_services();
    // The second seeder, for the same reason the `Mac` ratchet calls its
    // own: a population read out of the registry only covers what the
    // seeders this test CALLS have put there.
    seed_sunjce_delegated_cipher_services();
    let advertised: Vec<String> = test_services()
        .get("SunJCE")
        .expect("the SunJCE seed must have run")
        .values()
        .filter(|e| e.type_str == "Cipher")
        .map(|e| e.algorithm.clone())
        .collect();
    assert!(
        advertised.len() >= 12,
        "the SunJCE Cipher seed looks empty: {advertised:?}"
    );
    for algorithm in &advertised {
        // SERVICEABLE is a disjunction, and the second arm is what the
        // seventeen delegated transformations rest on.
        //
        // `transformation_is_serviceable` asks whether THIS engine computes
        // the name. `Cipher.getInstance`'s anonymous overload does not stop
        // there: on a refusal it walks the chain and instantiates the class
        // the service row names, which is the whole implementation for a
        // name with no Rust engine (PBES2 at the PRFs `pbes2_aes_params`
        // has no arm for, the PKCS#12 PBE family, `DESedeWrap`, `RC2`, and
        // the six AES key-wrap paddings).
        //
        // The second arm has to be a REAL class. A row carrying the
        // `.Native` MARKER is not one — it means "a Rust engine answers
        // this" — and a marker row for a name the engine does not compute
        // is exactly the W7-15 defect. It is also why
        // `jca-provider-population-gap-20260830-FIXED.md` §5.2 concluded a
        // service row could never be sufficient: the six rows it added
        // carried the marker.
        let routed = get_service_entry("SunJCE", "Cipher", algorithm)
            .map(|e| {
                let c = e.class_name.trim().to_string();
                !c.is_empty() && !c.ends_with(".Native")
            })
            .unwrap_or(false);
        assert!(
            crate::jca::cipher::transformation_is_serviceable(algorithm) || routed,
            "SunJCE advertises Cipher.{algorithm}, but Cipher.getInstance refuses it — \
             advertising an algorithm the engine cannot compute is the defect W7-15 closed"
        );
    }
    // And the reverse direction for the names that were the actual bug.
    // They were removed on 2026-08-11 while unimplemented and put back the
    // same day once implemented, so what this asserts is the INVARIANT —
    // advertised and computable are one set — rather than a fixed verdict
    // about these four names. The loop above already proves each is
    // serviceable; this proves the seed did not quietly drop them.
    for implemented in [
        "ChaCha20",
        "ChaCha20-Poly1305",
        "AES/KW/PKCS5Padding",
        "AES/KWP/NoPadding",
        // W7-39, 2026-08-12: same history, one wave later. Both were pulled
        // from the seed on 08-11 while `getInstance` served them as
        // AES-128-ECB, and both are back now that `jca::cipher::
        // drive_real_ecb_cipher` computes them through the real SunJCE SPI.
        "ARCFOUR",
        "Blowfish",
    ] {
        assert!(
            get_service_entry("SunJCE", "Cipher", implemented).is_some(),
            "{implemented} is implemented but no longer advertised: the two lists have drifted, which is the defect this test exists for"
        );
        assert!(
            crate::jca::cipher::transformation_is_serviceable(implemented),
            "{implemented} is advertised but Cipher.getInstance refuses it"
        );
    }
    // An ALIAS is resolvable without being advertised — that is the whole
    // point of one — so `RC4` has to be checked through the alias map rather
    // than through the advertised list, which is where the loop above would
    // never have looked. Measured on HotSpot 25: `Cipher.getInstance("RC4")`
    // resolves, answers `getProvider()=SunJCE`, and produces the same bytes
    // as `ARCFOUR`, while `Security.getAlgorithms("Cipher")` names only
    // ARCFOUR.
    //
    // `TripleDES` is deliberately NOT in this loop. It resolves through the
    // alias map like the two below, but `Cipher.getInstance("TripleDES")` is
    // still refused, because SunJCE maps it to the BARE `DESede` and a bare
    // DESede defaults to ECB — a mode this engine does not route. That is a
    // real divergence from HotSpot and it predates W7-39; asserting
    // serviceability for it here would red the test for a gap this lane did
    // not open and does not close.
    for (alias, canonical) in [("RC4", "ARCFOUR"), ("AESWrap", "AES/KW/NoPadding")] {
        assert!(
            get_service_entry("SunJCE", "Cipher", alias).is_some(),
            "{alias} must resolve through the alias map to {canonical}"
        );
        assert!(
            !advertised.iter().any(|a| a.eq_ignore_ascii_case(alias)),
            "{alias} is an ALIAS and must stay out of Security.getAlgorithms, \
             which is where HotSpot keeps it"
        );
        assert!(
            crate::jca::cipher::transformation_is_serviceable(alias),
            "{alias} resolves as a service but Cipher.getInstance refuses it"
        );
    }
}

/// Collect every algorithm `provider` advertises for `type_str` straight
/// out of the seed map.
///
/// Deliberately not via `algorithms_for_service`: that walks the live
/// PROVIDER CHAIN, and the chain's coverage here is not what the seed
/// wrote, so a chain-based read could answer a shorter set and the loops
/// below would pass by iterating less than they think. Each caller also
/// asserts a minimum row count before looping, because a loop over nothing
/// is a probe that cannot fail.
fn advertised_for(provider: &str, type_str: &str) -> Vec<String> {
    test_services()
        .get(provider)
        .map(|m| {
            m.values()
                .filter(|e| e.type_str == type_str)
                .map(|e| e.algorithm.clone())
                .collect()
        })
        .unwrap_or_default()
}

/// Every `(provider, algorithm)` the seed advertises for `type_str`,
/// across EVERY provider in the map.
///
/// The hardcoded provider list this replaces would have missed the
/// `SunJCE` `KeyFactory` `ML-KEM` umbrella entirely — an
/// advertised-and-refused name that neither originating record lists and
/// that only the census found. A ratchet that checks a subset of the
/// population is a ratchet with a hole in exactly the place nobody is
/// looking.
fn all_advertised(type_str: &str) -> Vec<(String, String)> {
    let providers: Vec<String> = test_services().keys().cloned().collect();
    let mut out = Vec::new();
    for provider in providers {
        for algorithm in advertised_for(&provider, type_str) {
            out.push((provider.clone(), algorithm));
        }
    }
    out
}

/// W7-63 ratchet: every `MessageDigest` name `SUN` advertises must be one
/// `MessageDigest.getInstance` will serve, and every digest this VM
/// implements must be advertised.
///
/// Both directions, because this pair drifted in both at once and only one
/// of them was loud. `MD2` sat in the advertised array for three waves
/// while `algorithm_supported` refused it — three lines above a comment
/// declining to advertise SHAKE for exactly that reason. `SHAKE128-256`
/// and `SHAKE256-512` were the quiet direction: correctly withheld while
/// unimplemented, and then still withheld after the dependency to
/// implement them was already in the tree. A census run by hand finds one
/// of those and drifts again by the next wave; a test finds both and
/// cannot.
#[test]
fn every_advertised_sun_message_digest_is_serviceable() {
    let _lock = reset_service_state_for_tests();
    seed_direct_native_engine_services();
    let advertised = all_advertised("MessageDigest");
    assert!(
        advertised.len() >= 15,
        "the MessageDigest seed looks empty: {advertised:?}"
    );
    for (provider, algorithm) in &advertised {
        assert!(
            crate::jca::message_digest::algorithm_supported_public(algorithm),
            "{provider} advertises MessageDigest.{algorithm}, but \
             MessageDigest.getInstance refuses it — advertising a digest the engine \
             cannot compute is the defect W7-63 closed"
        );
    }
    // The reverse direction. These are the three names the two records
    // were actually about; the loop above proves each is serviceable, this
    // proves the seed did not quietly drop it.
    for implemented in ["MD2", "SHAKE128-256", "SHAKE256-512"] {
        assert!(
            get_service_entry("SUN", "MessageDigest", implemented).is_some(),
            "{implemented} is implemented but not advertised: the two lists have drifted, \
             which is the defect this test exists for"
        );
    }
    // And the ALIAS half, which decides whether the advertised count lands
    // on HotSpot's 15 or on 17. `SHAKE128` / `SHAKE256` resolve through
    // `getInstance` (measured on HotSpot 25: byte-identical digests to the
    // hyphenated primaries) while `Security.getAlgorithms("MessageDigest")`
    // must NOT list them. An alias registered as a service would pass
    // every other assertion in this test.
    for (alias, canonical) in [("SHAKE128", "SHAKE128-256"), ("SHAKE256", "SHAKE256-512")] {
        assert!(
            get_service_entry("SUN", "MessageDigest", alias).is_some(),
            "{alias} must resolve through the alias map to {canonical}"
        );
        assert!(
            !advertised
                .iter()
                .any(|(_, a)| a.eq_ignore_ascii_case(alias)),
            "{alias} is an ALIAS and must stay out of Security.getAlgorithms, \
             which is where HotSpot keeps it"
        );
    }
}

/// Every `MessageDigest` alias HotSpot's SUN declares is one this seed
/// declares too, resolves to a service this VM can compute, and stays out
/// of `Security.getAlgorithms`.
///
/// The list below is the forty rows of
/// `Security.getProvider("SUN").keySet()` on HotSpot 25, filtered to
/// `Alg.Alias.MessageDigest.*` and written out independently of the seed —
/// which is the whole value of it. Three of the forty were present before
/// 2026-08-24; `MessageDigest.getInstance("SHA1", "SUN")` was one of the
/// thirty-seven that were not, and it is a `NoSuchAlgorithmException`
/// against a provider that has served SHA-1 since the first seed.
///
/// Two directions, both load-bearing:
///
///   * the alias must RESOLVE — `check_provider_ownership` reads
///     `get_service_entry`, so a missing row refuses the call outright;
///   * the alias must not be ADVERTISED — registering these as services
///     instead would satisfy the first half and make
///     `Security.getAlgorithms("MessageDigest")` answer 55 where HotSpot
///     answers 15.
#[test]
fn sun_message_digest_aliases_all_resolve_to_a_seeded_service() {
    let _lock = reset_service_state_for_tests();
    seed_direct_native_engine_services();
    let advertised = all_advertised("MessageDigest");
    const HOTSPOT_SUN_MD_ALIASES: &[(&str, &str)] = &[
        ("SHA", "SHA-1"),
        ("SHA1", "SHA-1"),
        ("SHA224", "SHA-224"),
        ("SHA256", "SHA-256"),
        ("SHA384", "SHA-384"),
        ("SHA512", "SHA-512"),
        ("SHA512/224", "SHA-512/224"),
        ("SHA512/256", "SHA-512/256"),
        ("SHAKE128", "SHAKE128-256"),
        ("SHAKE256", "SHAKE256-512"),
        ("1.2.840.113549.2.2", "MD2"),
        ("1.2.840.113549.2.5", "MD5"),
        ("1.3.14.3.2.26", "SHA-1"),
        ("2.16.840.1.101.3.4.2.1", "SHA-256"),
        ("2.16.840.1.101.3.4.2.2", "SHA-384"),
        ("2.16.840.1.101.3.4.2.3", "SHA-512"),
        ("2.16.840.1.101.3.4.2.4", "SHA-224"),
        ("2.16.840.1.101.3.4.2.5", "SHA-512/224"),
        ("2.16.840.1.101.3.4.2.6", "SHA-512/256"),
        ("2.16.840.1.101.3.4.2.7", "SHA3-224"),
        ("2.16.840.1.101.3.4.2.8", "SHA3-256"),
        ("2.16.840.1.101.3.4.2.9", "SHA3-384"),
        ("2.16.840.1.101.3.4.2.10", "SHA3-512"),
        ("2.16.840.1.101.3.4.2.11", "SHAKE128-256"),
        ("2.16.840.1.101.3.4.2.12", "SHAKE256-512"),
        ("OID.1.2.840.113549.2.2", "MD2"),
        ("OID.1.2.840.113549.2.5", "MD5"),
        ("OID.1.3.14.3.2.26", "SHA-1"),
        ("OID.2.16.840.1.101.3.4.2.1", "SHA-256"),
        ("OID.2.16.840.1.101.3.4.2.2", "SHA-384"),
        ("OID.2.16.840.1.101.3.4.2.3", "SHA-512"),
        ("OID.2.16.840.1.101.3.4.2.4", "SHA-224"),
        ("OID.2.16.840.1.101.3.4.2.5", "SHA-512/224"),
        ("OID.2.16.840.1.101.3.4.2.6", "SHA-512/256"),
        ("OID.2.16.840.1.101.3.4.2.7", "SHA3-224"),
        ("OID.2.16.840.1.101.3.4.2.8", "SHA3-256"),
        ("OID.2.16.840.1.101.3.4.2.9", "SHA3-384"),
        ("OID.2.16.840.1.101.3.4.2.10", "SHA3-512"),
        ("OID.2.16.840.1.101.3.4.2.11", "SHAKE128-256"),
        ("OID.2.16.840.1.101.3.4.2.12", "SHAKE256-512"),
    ];
    assert_eq!(
        HOTSPOT_SUN_MD_ALIASES.len(),
        40,
        "the measured HotSpot 25 alias set is forty rows; a loop over a \
         truncated list passes vacuously"
    );
    for (alias, canonical) in HOTSPOT_SUN_MD_ALIASES {
        let entry = get_service_entry("SUN", "MessageDigest", alias);
        assert!(
            entry.is_some(),
            "SUN answers MessageDigest.{alias} on HotSpot 25 and this seed \
             has no row for it, so check_provider_ownership refuses \
             getInstance({alias:?}, \"SUN\") outright"
        );
        assert!(
            crate::jca::message_digest::algorithm_supported_public(canonical),
            "{alias} resolves to {canonical}, which the digest engine \
             cannot compute: the alias would trade a refusal for a failure \
             one call later"
        );
        assert!(
            !advertised
                .iter()
                .any(|(_, a)| a.eq_ignore_ascii_case(alias)),
            "{alias} is an ALIAS and must stay out of \
             Security.getAlgorithms, which is where HotSpot keeps it"
        );
    }
    assert_eq!(
        advertised
            .iter()
            .filter(|(provider, _)| provider == "SUN")
            .count(),
        15,
        "SUN advertises fifteen MessageDigest services on HotSpot 25; \
         adding aliases must not move that number"
    );
}

/// Every row of [`MEASURED_JDK25_PROVIDER_ALIASES`] resolves, and resolves
/// to a service its own provider actually registers.
///
/// The table is a transcript of HotSpot 25 filtered by what this VM can
/// serve, and both halves of that sentence are load-bearing:
///
///   * a row whose ALIAS does not resolve is dead weight —
///     `check_provider_ownership` reads `get_service_entry`, so the row
///     exists precisely to make that lookup succeed;
///   * a row whose CANONICAL is not a registered service of the SAME
///     provider would trade a refusal at `getInstance` for a failure one
///     call later, which is the trap the 70 excluded rows are excluded to
///     avoid. Seeding those is how an alias table starts lying.
///
/// Also asserts the table did not shrink and that aliases stay out of
/// `Security.getAlgorithms`, which is where HotSpot keeps them.
#[test]
fn every_measured_alias_resolves_to_a_serviceable_canonical() {
    let _lock = reset_service_state_for_tests();
    seed_direct_native_engine_services();
    // The measurement was taken on a VM with real-JCA routing on, and
    // three seeders are conditional on it (`register` calls them only
    // under `real_jca_mode() || route_ec_to_real() || route_dsa_to_real()`).
    // `SUN AlgorithmParameters DSA` is one of the services they add, and
    // an alias row pointing at it is honest only in the configuration
    // that has it — so seed what the measured VM had, rather than
    // weakening the assertion until the narrower default passes.
    seed_sun_dsa_services();
    seed_sunjsse_services();
    assert_eq!(
        MEASURED_JDK25_PROVIDER_ALIASES.len(),
        264,
        "the measured table is 264 rows; a loop over a truncated list \
         passes vacuously"
    );
    for (provider, engine, alias, canonical) in MEASURED_JDK25_PROVIDER_ALIASES {
        // The row itself. This is the invariant the seed establishes, and
        // it holds for every engine whether or not that engine registers
        // SERVICES: `KeyAgreement` and `KEM` are served from hand-written
        // SPI tables and register none, so asserting a service entry here
        // would fail on rows that work perfectly.
        let resolved =
            canonical_service_algorithm(Some(provider), engine, alias).unwrap_or_default();
        assert!(
            resolved.eq_ignore_ascii_case(canonical),
            "{provider} declares Alg.Alias.{engine}.{alias} = {canonical} on \
             HotSpot 25; this seed resolves it to {resolved:?}"
        );
        // And where the canonical IS a registered service, the alias must
        // reach it through `get_service_entry` too — that is the lookup
        // `check_provider_ownership` performs, and the gate every
        // named-provider `getInstance` passes through.
        if get_service_entry(provider, engine, canonical).is_some() {
            assert!(
                get_service_entry(provider, engine, alias).is_some(),
                "{provider}.{engine}.{canonical} is a registered service but \
                 the alias {alias} does not reach it, so \
                 check_provider_ownership refuses \
                 getInstance({alias:?}, {provider:?}) outright"
            );
        }
        let advertised = all_advertised(engine);
        assert!(
            !advertised
                .iter()
                .any(|(p, a)| p == provider && a.eq_ignore_ascii_case(alias)),
            "{provider}.{engine}.{alias} is an ALIAS and must stay out of \
             Security.getAlgorithms"
        );
    }
}

/// A seeded provider's property view is its registry, in the provider's
/// own spelling.
///
/// `keySet` answered EMPTY for every JDK provider until 2026-08-27 —
/// `put_service` / `put_alias` captured into the structured maps and
/// nothing mirrored them into the property table `keySet`, `entrySet`,
/// `values`, `keys`, `elements`, `size`, `isEmpty` and `getProperty` all
/// read. Measured against HotSpot 25, which answers 257 keys on SUN.
///
/// The SPELLING half is the part a test is needed for. Lookups normalise
/// to upper case, and rendering a row from the normalised key would
/// produce `ALG.ALIAS.MESSAGEDIGEST.SHA1`, which is not a key any caller
/// recognises and not one `getProperty` could match. That is why
/// `AliasEntry` keeps the raw spellings at all, and it is what the exact
/// string comparisons below pin.
#[test]
fn a_seeded_provider_renders_its_registry_as_property_rows() {
    let _lock = reset_service_state_for_tests();
    seed_direct_native_engine_services();
    let rows = projected_registry_rows("SUN");
    assert!(
        rows.len() >= 100,
        "SUN projects {} rows; HotSpot 25 answers 257 and an empty view is \
         the defect this closes",
        rows.len()
    );
    let find = |key: &str| -> Option<String> {
        rows.iter().find(|(k, _)| k == key).map(|(_, v)| v.clone())
    };
    // A SERVICE row: `<Type>.<Algorithm>` in the provider's spelling.
    let digest = find("MessageDigest.SHA-256")
        .expect("SUN must project MessageDigest.SHA-256 as a property row");
    assert!(
        !digest.trim().is_empty(),
        "a service row's value is its implementation class, never empty"
    );
    // An ALIAS row, and the value is the canonical it names.
    assert_eq!(
        find("Alg.Alias.MessageDigest.SHA1").as_deref(),
        Some("SHA-1"),
        "SUN must project Alg.Alias.MessageDigest.SHA1 = SHA-1, in that \
         spelling: an upper-cased key matches nothing a caller asks for"
    );
    // The upper-cased forms must NOT appear — that is what rendering
    // from the normalised lookup key would have produced.
    for mangled in ["ALG.ALIAS.MESSAGEDIGEST.SHA1", "MESSAGEDIGEST.SHA-256"] {
        assert!(
            find(mangled).is_none(),
            "{mangled} is a normalised LOOKUP key, not a property key"
        );
    }
    // An alias-only service artefact carries no class and is not a row.
    assert!(
        rows.iter().all(|(_, v)| !v.trim().is_empty()),
        "a property row with an empty value is not one any provider advertises"
    );
}

/// W7-63 ratchet: no advertised `Signature` name may be refused at
/// `getInstance`.
///
/// Only this direction is asserted, and the asymmetry is the point.
/// `Signature.getInstance` used to accept EVERY string and defer the
/// failure to `sign()`/`verify()` as a `SignatureException`, so a caller
/// probing with `catch (NoSuchAlgorithmException)` took the wrong branch.
/// The gate that closed it (`signature::signature_name_is_offered`) is a
/// disjunction — a name this engine has an index for, OR a name some
/// provider advertises — so it can only fail in one direction, and this is
/// that direction.
///
/// It deliberately does NOT assert that every advertised name can compute.
/// Nine `SunRsaSign` names (`MD2withRSA`, `SHA3-*withRSA`,
/// `SHA512/224withRSA` and friends) have no `sign_dispatch` arm and fail
/// at `sign()` with a checked, catchable `SignatureException`. That is an
/// ordinary unimplemented-algorithm gap, not this record's species: the
/// name is real, the advertisement is truthful, and the failure is closed.
/// Asserting serviceability here would red the test for a gap W7-63 did
/// not open and does not close.
#[test]
fn every_advertised_signature_name_is_offered_by_get_instance() {
    let _lock = reset_service_state_for_tests();
    seed_direct_native_engine_services();
    let advertised = all_advertised("Signature");
    assert!(
        advertised.len() >= 20,
        "the Signature seed looks empty ({} rows): a loop over nothing \
         passes vacuously",
        advertised.len()
    );
    for (provider, algorithm) in &advertised {
        assert!(
            crate::jca::signature::get_instance_offers(TEST_VM, algorithm),
            "{provider} advertises Signature.{algorithm}, but \
             Signature.getInstance refuses it"
        );
    }
    // The anti-vacuity half: prove the gate can say NO. Without this the
    // loop above is satisfied by a predicate that returns `true`
    // unconditionally, which is exactly what `sig_get_instance` used to do.
    for bogus in ["NO-SUCH-SIG", "ML-KEM", "AES", "HmacSHA256", ""] {
        assert!(
            !crate::jca::signature::get_instance_offers(TEST_VM, bogus),
            "Signature.getInstance must refuse {bogus:?} — HotSpot 25 raises \
             NoSuchAlgorithmException for it, measured"
        );
    }
}

/// The `KeyPairGenerator` ratchet, in BOTH directions — which is what makes
/// it different from its `Signature` / `KeyFactory` siblings.
///
/// The one-way form (advertised ⇒ serviceable) cannot see the residual this
/// closes. `Security.getProviders("KeyPairGenerator.Ed25519")` answered
/// `<none>` for an algorithm `getInstance("Ed25519")` served, because
/// `security_get_providers_filtered` reads the service registry and the
/// built-in algorithms were in NO registry — under-advertised, not
/// over-advertised, and therefore invisible to a one-way check. A caller
/// that picks its provider by filter concluded the algorithm did not exist.
///
/// So both halves are asserted: nothing advertised is refused, and nothing
/// served is unadvertised.
#[test]
fn every_kpg_algorithm_this_vm_serves_is_advertised() {
    let _lock = reset_service_state_for_tests();
    seed_direct_native_engine_services();
    let advertised = all_advertised("KeyPairGenerator");
    assert!(
        advertised.len() >= 15,
        "the KeyPairGenerator seed looks empty ({} rows): a loop over \
         nothing passes vacuously",
        advertised.len()
    );
    for (provider, algorithm) in &advertised {
        assert!(
            crate::jca::key_factory::kpg_get_instance_offers(algorithm),
            "{provider} advertises KeyPairGenerator.{algorithm}, but \
             KeyPairGenerator.getInstance refuses it"
        );
    }
    // The direction the filter needs, and the one the residual was in.
    // `DH` and `ECDSA` are absent by design: HotSpot registers `DH` as an
    // ALIAS of `DiffieHellman` (aliases resolve but are not advertised),
    // and registers no `ECDSA` generator at all.
    for algorithm in [
        "RSA",
        "RSASSA-PSS",
        "EC",
        "DSA",
        "Ed25519",
        "Ed448",
        "EdDSA",
        "X25519",
        "X448",
        "XDH",
        "ML-DSA",
        "ML-DSA-44",
        "ML-KEM",
        "ML-KEM-512",
        "DiffieHellman",
    ] {
        assert!(
            crate::jca::key_factory::kpg_get_instance_offers(algorithm),
            "{algorithm} must be serviceable"
        );
        assert!(
            find_service_provider("KeyPairGenerator", algorithm).is_some(),
            "KeyPairGenerator.{algorithm} is served but advertised by no \
             provider — Security.getProviders(\"KeyPairGenerator.{algorithm}\") \
             answers <none> and a caller that picks its provider by filter \
             cannot find it"
        );
    }
    // The alias resolves without being advertised, exactly as on HotSpot.
    assert!(
        find_service_provider("KeyPairGenerator", "DH").is_some(),
        "the DH alias must resolve to the DiffieHellman service"
    );
    // Anti-vacuity: prove the registry can say NO.
    for bogus in ["NO-SUCH-KPG", "SLH-DSA", "AES", ""] {
        assert!(
            find_service_provider("KeyPairGenerator", bogus).is_none(),
            "no provider may advertise KeyPairGenerator.{bogus:?}"
        );
    }
}

/// W7-63 ratchet: every `KeyFactory` name the seed advertises must be one
/// `KeyFactory.getInstance` will serve.
///
/// The `SUN` seed advertised the `ML-DSA` UMBRELLA name while
/// `key_factory::kf_algo_idx` had arms only for `ML-DSA-44/65/87` and
/// `kf_get_instance` threw on the negative index — advertised and refused.
/// `signature::algo_idx` carries the umbrella arm, so the two engines
/// disagreed about one name. The umbrella is now absent from the
/// `KeyFactory` seed and still present in the `Signature` seed, which
/// makes each engine's advertisement match ITS OWN implementation rather
/// than making the two engines agree with each other — that is the
/// correct invariant, and it is what this asserts.
#[test]
fn every_advertised_key_factory_name_is_serviceable() {
    let _lock = reset_service_state_for_tests();
    seed_direct_native_engine_services();
    let advertised = all_advertised("KeyFactory");
    assert!(
        advertised.len() >= 12,
        "the KeyFactory seed looks empty ({} rows): a loop over nothing \
         passes vacuously",
        advertised.len()
    );
    for (provider, algorithm) in &advertised {
        assert!(
            crate::jca::key_factory::get_instance_offers(TEST_VM, algorithm),
            "{provider} advertises KeyFactory.{algorithm}, but \
             KeyFactory.getInstance refuses it"
        );
    }
    // Both umbrellas WERE stop-advertising fixes and are now
    // implementations: `kf_algo_idx` carries `ALGO_MLDSA_GENERIC` /
    // `ALGO_MLKEM_GENERIC` and `pqc_umbrella_keyfactory_class` drives the
    // JDK's own non-nested factory for each. The rule the earlier
    // assertion encoded has not changed — "either implement the umbrella
    // arm or leave the name out, but never both" — only which side of it
    // these two names sit on. The loop above is what enforces it
    // generically; this is the named pin for the pair that drifted.
    for (provider, umbrella) in [("SUN", "ML-DSA"), ("SunJCE", "ML-KEM")] {
        assert!(
            get_service_entry(provider, "KeyFactory", umbrella).is_some(),
            "{provider} implements the {umbrella} KeyFactory umbrella and must \
             advertise it — an implemented-but-unadvertised name is invisible to \
             Security.getProviders(filter), which is how a caller picks a provider"
        );
        assert!(
            crate::jca::key_factory::get_instance_offers(TEST_VM, umbrella),
            "{umbrella} is advertised for KeyFactory and must be serviceable"
        );
    }
    assert!(
        get_service_entry("SUN", "Signature", "ML-DSA").is_some(),
        "Signature DOES implement the ML-DSA umbrella (signature::algo_idx \
         carries SIG_MLDSA) and must keep advertising it — the two engines are \
         each truthful about THEMSELVES, which is the invariant, not that they \
         agree with each other"
    );
    for param_set in ["ML-DSA-44", "ML-DSA-65", "ML-DSA-87"] {
        assert!(
            get_service_entry("SUN", "KeyFactory", param_set).is_some(),
            "{param_set} is serviceable and must stay advertised"
        );
    }
    for param_set in ["ML-KEM-512", "ML-KEM-768", "ML-KEM-1024"] {
        assert!(
            get_service_entry("SunJCE", "KeyFactory", param_set).is_some(),
            "{param_set} is serviceable and must stay advertised"
        );
    }
}

/// The same ratchet for `Mac`, in the shape
/// `every_advertised_sunjce_cipher_is_serviceable` established — and for the
/// same reason: `Mac` is where the wrong-algorithm defect was WORST, because
/// a MAC that verifies is itself the security decision, and two peers
/// computing the same wrong MAC interoperate happily.
///
/// This reads the advertised set out of the registry rather than restating
/// it, so the list cannot be updated on one side only. `HmacSHA224` was
/// added to both sides on 2026-08-12; a census written by hand would have
/// been the thing that went stale.
#[test]
fn every_advertised_sunjce_mac_is_computable() {
    let _lock = reset_service_state_for_tests();
    seed_direct_native_engine_services();
    // The SECOND seeder, added 2026-09-02, and the reason this line is
    // here: the population below is read out of the registry, but the
    // registry holds only what the seeders this test CALLS have put in it.
    // Sixteen new `Mac` services seeded from a function this test did not
    // call left the ratchet passing over a set that no longer matched the
    // VM's — the same "population transcribed from the registrar rather
    // than from the platform" shape `every_public_mac_method_is_registered`
    // was caught by (E25-R11).
    seed_sunjce_pbe_mac_services();
    let advertised: Vec<String> = test_services()
        .get("SunJCE")
        .expect("the SunJCE seed must have run")
        .values()
        .filter(|e| e.type_str == "Mac")
        .map(|e| e.algorithm.clone())
        .collect();
    assert!(
        advertised.len() >= 28,
        "the SunJCE Mac seed looks empty: {advertised:?}"
    );
    for algorithm in &advertised {
        let bytes = crate::phases_late::ssl_security::mac_compute_hmac(algorithm, b"k", b"d");
        // SERVICEABLE, which is a disjunction — and was a single term until
        // the PKCS#12 / PBMAC1 / SSL MAC families landed.
        //
        // None of those sixteen is an HMAC this crate computes (PKCS#12
        // v1.0 B.2 derivation, PBKDF2-then-HMAC, and the SSL 3.0 pad-byte
        // construction respectively), and none needs to be:
        // `Mac.getInstance` falls to `build_real_mac` for a name it cannot
        // compute, so the platform's own class serves them. The W4-3
        // property this test exists for is "advertised implies
        // serviceable", not "advertised implies computed HERE" — but the
        // second arm has to be a REAL class, because a `.Native` marker row
        // is precisely an advertisement with nothing behind it.
        let routed = get_service_entry("SunJCE", "Mac", algorithm)
            .map(|e| {
                let c = e.class_name.trim().to_string();
                !c.is_empty() && !c.ends_with(".Native")
            })
            .unwrap_or(false);
        assert!(
            bytes.is_some() || routed,
            "SunJCE advertises Mac.{algorithm}, and neither mac_compute_hmac nor a real \
             implementation class serves it"
        );
        if bytes.is_none() {
            continue;
        }
        let len = crate::phases_late::ssl_security::mac_output_length(algorithm);
        assert!(
            bytes.is_some(),
            "SunJCE advertises Mac.{algorithm}, but mac_compute_hmac produces nothing — \
             advertising a MAC the engine cannot compute is the W4-3 defect"
        );
        assert_eq!(
            len,
            bytes.as_ref().map(Vec::len),
            "Mac.{algorithm}: getMacLength() and the bytes doFinal returns must agree — \
             the retired `_ => 32` arm made them agree with each other and with nothing else"
        );
    }
    // The reverse direction, which is the one a census does not enumerate:
    // implemented but unadvertised. Every name `mac_compute_hmac` answers
    // must have a service row, or `Security.getAlgorithms("Mac")` under-
    // reports what `Mac.getInstance` will actually serve.
    for implemented in [
        "HmacMD5",
        "HmacSHA1",
        "HmacSHA224",
        "HmacSHA256",
        "HmacSHA384",
        "HmacSHA512",
    ] {
        assert!(
            get_service_entry("SunJCE", "Mac", implemented).is_some(),
            "{implemented} is computed by mac_compute_hmac but not advertised: the two \
             lists have drifted, which is the defect this test exists for"
        );
    }
}

/// And for `KeyGenerator`, which is where the drift was widest: the seed
/// carried three names while `phases_early::keygen_default_bits` implemented
/// fourteen, so `KeyGenerator.getInstance("Blowfish")` answered
/// `NoSuchAlgorithmException: Blowfish KeyGenerator not available` —
/// measured on this tree's own binary — for an algorithm BOTH modes could
/// serve.
///
/// The check runs in the implemented→advertised direction only, on purpose.
/// The other direction is not this test's to make: in `--fast-boot` mode
/// `KeyGenerator.getInstance` resolves through this registry and then
/// INSTANTIATES the named class out of the real image, so an advertised row
/// is serviceable if and only if that class loads — which no unit test in
/// this crate can observe. `keygen_default_bits` is the `--synthetic-jdk`
/// half, and it is the half a Rust test can hold to account.
#[test]
fn every_keygenerator_the_engine_implements_is_advertised() {
    let _lock = reset_service_state_for_tests();
    seed_direct_native_engine_services();
    for implemented in [
        "AES",
        "ARCFOUR",
        "Blowfish",
        "ChaCha20",
        "DES",
        "DESede",
        "HmacMD5",
        "HmacSHA1",
        "HmacSHA224",
        "HmacSHA256",
        "HmacSHA384",
        "HmacSHA512",
        "RC2",
    ] {
        assert!(
            get_service_entry("SunJCE", "KeyGenerator", implemented).is_some(),
            "phases_early::keygen_default_bits generates a {implemented} key, but SunJCE \
             does not advertise it — implemented-but-unadvertised is the quiet half of \
             this defect, because nothing asks for a name nobody publishes"
        );
    }
    // `RC4` is the alias, `ARCFOUR` the service, exactly as on SunJCE.
    assert!(get_service_entry("SunJCE", "KeyGenerator", "RC4").is_some());
    // THE BICONDITIONAL, over HotSpot's own list, and derived rather than
    // restated.
    //
    // This was two hand-written lists — implemented-and-advertised above,
    // and a literal `["HmacSHA3-256", "HmacSHA512/256", "SunTlsPrf"]` of
    // names that must NOT be advertised. The second went stale the moment
    // `keygen_default_bits` grew arms for the first two (2026-09-02), and
    // it failed saying they "must not be advertised" — which by then was
    // false. A list that states the answer cannot check it.
    //
    // So: for every `KeyGenerator` name SunJCE registers on HotSpot 25,
    // advertised HERE if and only if this engine can generate it. Both
    // directions are the defect: advertising a name the synthetic path
    // refuses, and refusing to publish a name it serves.
    const HOTSPOT_SUNJCE_KEYGENERATORS: &[&str] = &[
        "AES",
        "ARCFOUR",
        "Blowfish",
        "ChaCha20",
        "DES",
        "DESede",
        "HmacMD5",
        "HmacSHA1",
        "HmacSHA224",
        "HmacSHA256",
        "HmacSHA384",
        "HmacSHA512",
        "HmacSHA512/224",
        "HmacSHA512/256",
        "HmacSHA3-224",
        "HmacSHA3-256",
        "HmacSHA3-384",
        "HmacSHA3-512",
        "RC2",
        "SunTls12Prf",
        "SunTlsKeyMaterial",
        "SunTlsMasterSecret",
        "SunTlsPrf",
        "SunTlsRsaPremasterSecret",
    ];
    // ADVERTISED iff SERVICEABLE, and "serviceable" is now a disjunction:
    // this crate generates the key itself (`keygen_default_bits`), or the
    // row names a REAL platform class that `build_real_key_generator`
    // instantiates on the engine's refusal. Before 2026-09-02 only the
    // first arm existed and the five `SunTls*` names were asserted ABSENT;
    // widening the predicate rather than deleting the assertion is what
    // keeps this a check instead of a restatement.
    for name in HOTSPOT_SUNJCE_KEYGENERATORS {
        let generates = crate::phases_early::keygen_default_bits(name).is_some();
        let delegates = service_implementation_class("KeyGenerator", "SunJCE", name).is_some();
        let advertised = get_service_entry("SunJCE", "KeyGenerator", name).is_some();
        assert_eq!(
            generates || delegates,
            advertised,
            "KeyGenerator.{name}: generates={generates} delegates={delegates} but SunJCE \
             advertises={advertised} — advertised and serviceable must agree in BOTH \
             directions"
        );
    }
    // The five `SunTls*` KDFs take `TlsKeyMaterialParameterSpec`-family
    // specs, so this crate must NOT claim to generate them from a key size
    // — they are served by routing to the platform's own generator, and a
    // `keygen_default_bits` arm appearing here would mean someone had
    // fabricated a key where a KDF belongs. The disjunction above is what
    // admits them; this pins WHICH arm may do it.
    for tls in [
        "SunTlsPrf",
        "SunTls12Prf",
        "SunTlsMasterSecret",
        "SunTlsKeyMaterial",
        "SunTlsRsaPremasterSecret",
    ] {
        assert!(
            crate::phases_early::keygen_default_bits(tls).is_none(),
            "{tls} is a parameter-spec-driven KDF, not a key size"
        );
        let class = service_implementation_class("KeyGenerator", "SunJCE", tls)
            .unwrap_or_else(|| panic!("{tls} must be advertised with a real class"));
        assert!(
            class.starts_with("com.sun.crypto.provider.Tls") && !class.ends_with(".Native"),
            "{tls} must route to the platform generator, got {class:?}"
        );
    }
}

/// W3-7. Every name the platform JDK 25 SunJSSE provider registers must be
/// accepted, in the spellings a caller actually writes. `RJdkSecurity.tls`
/// only probes the negative half; the positive half is what breaks every
/// HTTPS suite if an accept list is drawn too narrow, so pin it here.
#[test]
fn ssl_context_protocol_supported_accepts_every_jdk25_name() {
    for p in [
        "TLS", "tls", "TLSv1", "TLSv1.1", "TLSv1.2", "tlsv1.2", "TLSV1.2", "TLSv1.3", "SSL",
        "ssl", "SSLv3", "SSLV3", "Default", "default", "DEFAULT", "DTLS", "dtls", "DTLSv1.0",
        "DTLSv1.2",
    ] {
        assert!(
            ssl_context_protocol_supported(p),
            "{p:?} is a real JDK 25 SSLContext protocol and must not be refused"
        );
    }
}

/// MUST RAISE. Every one of these was probed on jdk-25.0.3.9-hotspot and
/// answered `NoSuchAlgorithmException: <name> SSLContext not available`.
/// The last four pin the normalisation: JCA lookup folds case and NOTHING
/// else, so a punctuation-stripping or space-trimming accept would
/// fabricate a context HotSpot refuses.
#[test]
fn ssl_context_protocol_supported_rejects_names_hotspot_rejects() {
    let _lock = reset_service_state_for_tests();
    for p in [
        "NO-SUCH-TLS",
        "SSLv2",
        "TLSv1.4",
        "NoSuchThing",
        "",
        "TLSv12",
        "T-L-S",
        "TLS ",
        " TLS",
    ] {
        assert!(
            !ssl_context_protocol_supported(p),
            "{p:?} is not a JDK SSLContext protocol and must be refused"
        );
    }
}

/// A protocol a caller's own provider registered must resolve even though
/// it is absent from the built-in list — the second accept ground.
#[test]
fn ssl_context_protocol_supported_honours_a_caller_registered_service() {
    let _lock = reset_service_state_for_tests();
    assert!(!ssl_context_protocol_supported("Conscrypt-TLS"));
    apply_legacy_put(
        "SUN",
        "SSLContext.Conscrypt-TLS",
        "org.conscrypt.OpenSSLContextImpl",
    );
    assert!(ssl_context_protocol_supported("Conscrypt-TLS"));
}

#[test]
fn sunjsse_seed_registers_the_full_jdk25_sslcontext_table() {
    let _lock = reset_service_state_for_tests();
    seed_sunjsse_services();
    for algo in [
        "TLS", "TLSv1", "TLSv1.1", "TLSv1.2", "TLSv1.3", "Default", "DTLS", "DTLSv1.0",
        "DTLSv1.2",
    ] {
        assert!(
            get_service_entry("SunJSSE", "SSLContext", algo).is_some(),
            "SunJSSE must service SSLContext.{algo}"
        );
    }
    // Aliases, with the platform's own targets.
    assert_eq!(
        get_service_entry("SunJSSE", "SSLContext", "SSL")
            .unwrap()
            .algorithm,
        "TLS"
    );
    assert_eq!(
        get_service_entry("SunJSSE", "SSLContext", "SSLv3")
            .unwrap()
            .algorithm,
        "TLSv1"
    );
    // And the negative half: no fabricated entry for a bogus name.
    assert!(get_service_entry("SunJSSE", "SSLContext", "NO-SUCH-TLS").is_none());
}

#[test]
fn alias_resolves_to_canonical_service() {
    let _lock = reset_service_state_for_tests();
    // Register the canonical service plus an alias pointing to it.
    apply_legacy_put("BC", "MessageDigest.SHA-256", "org.bc.SHA256");
    apply_legacy_put("BC", "Alg.Alias.MessageDigest.SHA256", "SHA-256");
    // Lookup by alias (no dash) must resolve to the canonical entry.
    let via_alias = get_service_entry("BC", "MessageDigest", "SHA256").unwrap();
    assert_eq!(via_alias.algorithm, "SHA-256");
    assert_eq!(via_alias.class_name, "org.bc.SHA256");
    // Direct canonical lookup still works.
    let direct = get_service_entry("BC", "MessageDigest", "SHA-256").unwrap();
    assert_eq!(direct.class_name, "org.bc.SHA256");
}

#[test]
fn put_native_populates_service_via_string_keys() {
    let _lock = reset_service_state_for_tests();
    let mut ctx = MockNativeContext::new();
    // Build a Provider receiver with `name="BC"` populated.
    let prov = ctx.alloc_object(cratonvm_types::ClassId::new(0), 8);
    let name = ctx.create_string("BC");
    ctx.set_field(prov, 0, Value::Object(Some(name)));
    ctx.set_field(prov, 1, Value::Double(1.80));

    let key = ctx.create_string("Cipher.AES/GCM/NoPadding");
    let value = ctx.create_string("org.bouncycastle.jcajce.provider.symmetric.AES$GCM");
    let args = [
        Value::Object(Some(prov)),
        Value::Object(Some(key)),
        Value::Object(Some(value)),
    ];
    let res = provider_put_native(&mut ctx, &args);
    assert!(res.is_ok());
    // Hashtable.put returns previous value — null for first put.
    assert!(matches!(res.unwrap(), Some(Value::Object(None))));

    // The service map must now have the entry under "BC".
    let entry = get_service_entry("BC", "Cipher", "AES/GCM/NoPadding").unwrap();
    assert_eq!(
        entry.class_name,
        "org.bouncycastle.jcajce.provider.symmetric.AES$GCM"
    );
}

#[test]
fn parse_legacy_put_native_alias_round_trip() {
    let _lock = reset_service_state_for_tests();
    let mut ctx = MockNativeContext::new();
    let prov = ctx.alloc_object(cratonvm_types::ClassId::new(0), 8);
    let name = ctx.create_string("BC");
    ctx.set_field(prov, 0, Value::Object(Some(name)));
    ctx.set_field(prov, 1, Value::Double(1.80));

    // First, the canonical service.
    let key1 = ctx.create_string("MessageDigest.SHA-256");
    let val1 = ctx.create_string("org.bc.SHA256");
    let _ = provider_parse_legacy_put_native(
        &mut ctx,
        &[
            Value::Object(Some(prov)),
            Value::Object(Some(key1)),
            Value::Object(Some(val1)),
        ],
    );
    // Then, an alias.
    let key2 = ctx.create_string("Alg.Alias.MessageDigest.SHA256");
    let val2 = ctx.create_string("SHA-256");
    let _ = provider_parse_legacy_put_native(
        &mut ctx,
        &[
            Value::Object(Some(prov)),
            Value::Object(Some(key2)),
            Value::Object(Some(val2)),
        ],
    );

    // Resolve via alias.
    let resolved = get_service_entry("BC", "MessageDigest", "SHA256").unwrap();
    assert_eq!(resolved.algorithm, "SHA-256");
}

#[test]
fn get_service_native_returns_null_for_unknown_algo() {
    let _lock = reset_service_state_for_tests();
    let mut ctx = MockNativeContext::new();
    let prov = ctx.alloc_object(cratonvm_types::ClassId::new(0), 8);
    let name = ctx.create_string("BC");
    ctx.set_field(prov, 0, Value::Object(Some(name)));
    ctx.set_field(prov, 1, Value::Double(1.80));

    let type_s = ctx.create_string("Cipher");
    let algo_s = ctx.create_string("MissingAlgo");
    let res = provider_get_service_native(
        &mut ctx,
        &[
            Value::Object(Some(prov)),
            Value::Object(Some(type_s)),
            Value::Object(Some(algo_s)),
        ],
    );
    assert!(res.is_ok());
    // Null = NoSuchAlgorithmException at the JDK call site.
    assert!(matches!(res.unwrap(), Some(Value::Object(None))));
}

#[test]
fn provider_service_new_instance_recovers_class_name_from_registry() {
    let _lock = reset_service_state_for_tests();
    apply_legacy_put(
        "WildFlyElytron",
        "SaslServerFactory.JBOSS-LOCAL-USER",
        "com.example.NoSuchSaslServerFactory",
    );

    let mut ctx = MockNativeContext::new();
    let provider = ctx.alloc_object(cratonvm_types::ClassId::new(0), 8);
    let provider_name = ctx.create_string("WildFlyElytron");
    ctx.set_field(provider, 0, Value::Object(Some(provider_name)));
    ctx.set_field(provider, 1, Value::Double(1.0));

    let service = alloc_service(&mut ctx);
    let svc_type = ctx.create_string("SaslServerFactory");
    let algorithm = ctx.create_string("JBOSS-LOCAL-USER");
    ctx.set_field(service, 0, Value::Object(Some(svc_type)));
    ctx.set_field(service, 1, Value::Object(Some(algorithm)));
    ctx.set_field(service, 2, Value::Object(Some(provider)));
    ctx.set_field(service, 3, Value::Object(None));

    let recovered = provider_service_get_class_name(&mut ctx, &[Value::Object(Some(service))])
        .expect("getClassName should not throw");
    let recovered = match recovered {
        Some(Value::Object(Some(s))) => ctx.read_string(s).unwrap_or_default(),
        other => panic!("expected recovered className string, got {other:?}"),
    };
    assert_eq!(recovered, "com.example.NoSuchSaslServerFactory");

    let instantiated = provider_service_new_instance(
        &mut ctx,
        &[Value::Object(Some(service)), Value::Object(None)],
    )
    .expect("registry-recovered className should reach instantiation in the mock VM");
    assert!(
        matches!(instantiated, Some(Value::Object(Some(_)))),
        "expected mock VM allocation after registry fallback, got {instantiated:?}"
    );
}

#[test]
fn provider_put_service_native_uses_slot_fallback_and_remembers_class_name() {
    let _lock = reset_service_state_for_tests();
    let mut ctx = MockNativeContext::new();
    let provider = ctx.alloc_object(cratonvm_types::ClassId::new(0), 8);
    let provider_name = ctx.create_string("WildFlyElytron");
    ctx.set_field(provider, 0, Value::Object(Some(provider_name)));
    ctx.set_field(provider, 1, Value::Double(1.0));

    let service = alloc_service(&mut ctx);
    let svc_type = ctx.create_string("SaslServerFactory");
    let algorithm = ctx.create_string("JBOSS-LOCAL-USER");
    let class_name =
        ctx.create_string("org.wildfly.security.sasl.localuser.LocalUserServerFactory");
    ctx.set_field(service, 0, Value::Object(Some(svc_type)));
    ctx.set_field(service, 1, Value::Object(Some(algorithm)));
    ctx.set_field(service, 2, Value::Object(Some(provider)));
    ctx.set_field(service, 3, Value::Object(Some(class_name)));

    provider_put_service_native(
        &mut ctx,
        &[Value::Object(Some(provider)), Value::Object(Some(service))],
    )
    .expect("putService native should not throw");

    let entry = get_service_entry("WildFlyElytron", "SaslServerFactory", "JBOSS-LOCAL-USER")
        .expect("putService should populate the provider service registry");
    assert_eq!(
        entry.class_name,
        "org.wildfly.security.sasl.localuser.LocalUserServerFactory"
    );
    let sk = provider_object_key(&ctx, service);
    assert_eq!(
        service_classname_table()
            .lock()
            .get(&sk)
            .cloned()
            .as_deref(),
        Some("org.wildfly.security.sasl.localuser.LocalUserServerFactory")
    );
}

#[test]
fn get_service_native_returns_populated_service_for_registered_algo() {
    let _lock = reset_service_state_for_tests();
    // Pre-populate the service map for "BC".
    apply_legacy_put(
        "BC",
        "Cipher.AES/GCM/NoPadding",
        "org.bouncycastle.jcajce.provider.symmetric.AES$GCM",
    );
    let mut ctx = MockNativeContext::new();
    let prov = ctx.alloc_object(cratonvm_types::ClassId::new(0), 8);
    let name = ctx.create_string("BC");
    ctx.set_field(prov, 0, Value::Object(Some(name)));
    ctx.set_field(prov, 1, Value::Double(1.80));

    let type_s = ctx.create_string("Cipher");
    let algo_s = ctx.create_string("AES/GCM/NoPadding");
    let res = provider_get_service_native(
        &mut ctx,
        &[
            Value::Object(Some(prov)),
            Value::Object(Some(type_s)),
            Value::Object(Some(algo_s)),
        ],
    );
    let svc = match res.unwrap() {
        Some(Value::Object(Some(o))) => o,
        other => panic!("expected non-null Service object, got {other:?}"),
    };
    // Synthetic slot 0 = type, slot 1 = algorithm, slot 3 = className.
    let svc_type = match ctx.get_field(svc, 0) {
        Value::Object(Some(s)) => ctx.read_string(s).unwrap_or_default(),
        _ => String::new(),
    };
    let svc_algo = match ctx.get_field(svc, 1) {
        Value::Object(Some(s)) => ctx.read_string(s).unwrap_or_default(),
        _ => String::new(),
    };
    let svc_class = match ctx.get_field(svc, 3) {
        Value::Object(Some(s)) => ctx.read_string(s).unwrap_or_default(),
        _ => String::new(),
    };
    assert_eq!(svc_type, "Cipher");
    assert_eq!(svc_algo, "AES/GCM/NoPadding");
    assert_eq!(
        svc_class,
        "org.bouncycastle.jcajce.provider.symmetric.AES$GCM"
    );
}

#[test]
fn put_get_service_natives_registered_via_register_fn() {
    // The natives must be exposed via `register()` so the WP6.5
    // wiring in `jca::register_jca_natives` picks them up.
    let mut r = NativeMethodRegistry::new();
    register(&mut r);

    assert!(
        r.find(
            "java/security/Provider",
            "put",
            "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;"
        )
        .is_some(),
        "Provider.put must be registered for BouncyCastle's addAlgorithm chain"
    );
    assert!(
        r.find(
            "java/security/Provider",
            "parseLegacyPut",
            "(Ljava/lang/String;Ljava/lang/String;)V"
        )
        .is_some(),
        "Provider.parseLegacyPut must be registered for BC's older-API path"
    );
    assert!(
        r.find(
            "java/security/Provider",
            "getService",
            "(Ljava/lang/String;Ljava/lang/String;)Ljava/security/Provider$Service;"
        )
        .is_some(),
        "Provider.getService must be registered for Cipher.getInstance lookups"
    );
    assert!(
        r.find(
            "java/security/Provider$Service",
            "getClassName",
            "()Ljava/lang/String;"
        )
        .is_some(),
        "Provider$Service.getClassName must be registered for SPI instantiation"
    );
}
