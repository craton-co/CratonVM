// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! JDK-only mode — the `NativeMethodRegistry` half of the contract
//! (`docs/feature-designs/jdk-only-mode.md` §4).
//!
//! Everything here runs on a registry this test builds itself, with its own
//! callbacks. Nothing depends on `native-builtins`, on a real JDK image, or on
//! any Cargo feature: `native-api` declares no `synthetic-jdk` feature, and a
//! strict-mode test that only passed under a non-default feature set would be
//! a test nothing runs.
//!
//! Five claims are pinned:
//!
//! 1. **Default is permissive.** A registry nobody configures is
//!    `CompatibilityMode::Compatible`, and a `SyntheticStub` registers and is
//!    findable exactly as it did before this mode existed.
//! 2. **Strict registration refuses `SyntheticStub` and only `SyntheticStub`.**
//!    The triple does not enter the table, and a
//!    `JdkOnlyViolation::SyntheticNativeRegistered` naming class, method and
//!    descriptor is retained so the run can say *what* went missing.
//! 3. **Overwrite history survives.** The registry deliberately lets a later
//!    `register()` of the same triple replace an earlier one in place; the
//!    schema-v2 census keeps the displaced kind, so "stub later fixed to a
//!    bridge" is distinguishable from "always a bridge".
//! 4. **The census is deterministic.** Two registries populated by the same
//!    sequence produce identical rows — the property CI's zero-stub assertion
//!    rests on.
//! 5. **`invocations_of_kind` counts dispatches**, per kind, from the cheap
//!    `record_invocation` hook every dispatch path calls.
//!
//! The wave-1 invariant from the research plan — *the strict registry contains
//! zero `SyntheticStub` entries* — is asserted here against a synthetic mix and
//! again in `native-builtins/tests/stub_ratchet.rs` against the real
//! `register_essential_natives` surface.

use cratonvm_native_api::{NativeCallback, NativeContext, NativeKind, NativeMethodRegistry};
use cratonvm_types::compat::CompatibilityMode;
use cratonvm_types::error::{JdkOnlyViolation, MethodCallResult};
use cratonvm_types::Value;

fn cb_one(_ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    Ok(Some(Value::Int(1)))
}

fn cb_two(_ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    Ok(Some(Value::Int(2)))
}

/// Register one triple under an explicit kind, restoring the ambient category
/// afterwards. Mirrors how the real registrars use `with_category`.
fn register_as(
    reg: &mut NativeMethodRegistry,
    kind: NativeKind,
    class: &str,
    name: &str,
    descriptor: &str,
    cb: NativeCallback,
) {
    reg.with_category(kind, |r| r.register(class, name, descriptor, cb));
}

/// Diff-stable projection of one census row. `registered_by` is deliberately
/// excluded: it is a `file:line` of the *calling* helper, which is stable
/// across two runs of the same sequence but is checked separately so a failure
/// here reads as "the census changed", not "the helper moved".
type CensusRow = (String, String, String, NativeKind, Option<NativeKind>);

fn census_shape(reg: &NativeMethodRegistry) -> Vec<CensusRow> {
    reg.census()
        .into_iter()
        .map(|e| (e.class, e.name, e.descriptor, e.kind, e.overwrote))
        .collect()
}

// ---------------------------------------------------------------------------
// 1. Default is permissive
// ---------------------------------------------------------------------------

#[test]
fn fresh_registry_defaults_to_compatible() {
    let reg = NativeMethodRegistry::new();
    assert_eq!(
        reg.compatibility_mode(),
        CompatibilityMode::Compatible,
        "a registry nobody configures must behave as it did before JDK-only \
         mode existed — strictness is a runtime policy set at VM init, never a \
         default, a build feature or an env var (contract §1)"
    );
    assert!(
        reg.refused_registrations().is_empty(),
        "nothing can have been refused before anything was registered"
    );
}

#[test]
fn compatible_mode_registers_synthetic_stubs_unchanged() {
    let mut reg = NativeMethodRegistry::new();
    register_as(
        &mut reg,
        NativeKind::SyntheticStub,
        "com/example/Compat",
        "stubbed",
        "()I",
        cb_one,
    );

    assert!(
        reg.find("com/example/Compat", "stubbed", "()I").is_some(),
        "Compatible mode must keep today's behaviour byte-for-byte: the stub \
         registers and is findable (contract §10 — wave 1 is measurement)"
    );
    assert_eq!(
        reg.kind_of("com/example/Compat", "stubbed", "()I"),
        Some(NativeKind::SyntheticStub)
    );
    assert!(
        reg.refused_registrations().is_empty(),
        "Compatible mode refuses nothing, so it records no violations"
    );
}

// ---------------------------------------------------------------------------
// 2. Strict registration refuses SyntheticStub — and only SyntheticStub
// ---------------------------------------------------------------------------

#[test]
fn strict_registration_refuses_synthetic_stub() {
    let mut reg = NativeMethodRegistry::new();
    reg.set_compatibility_mode(CompatibilityMode::JdkOnly);
    assert_eq!(reg.compatibility_mode(), CompatibilityMode::JdkOnly);

    register_as(
        &mut reg,
        NativeKind::SyntheticStub,
        "com/example/Strict",
        "fake",
        "(I)Ljava/lang/String;",
        cb_one,
    );

    assert!(
        reg.find("com/example/Strict", "fake", "(I)Ljava/lang/String;")
            .is_none(),
        "under JdkOnly a SyntheticStub must not enter the table at all — if it \
         is findable, dispatch can still reach the fake"
    );

    let refused = reg.refused_registrations();
    assert_eq!(
        refused.len(),
        1,
        "exactly one refusal expected, got {refused:?}"
    );
    match &refused[0] {
        JdkOnlyViolation::SyntheticNativeRegistered {
            class,
            method,
            descriptor,
            registered_by,
            survivor,
        } => {
            assert!(
                survivor.is_none(),
                "no earlier registration owned this triple, so the refusal DID \
                 retire the method; a survivor here would be the fall-through \
                 species, got {survivor:?}"
            );
            assert_eq!(class, "com/example/Strict");
            assert_eq!(method, "fake");
            assert_eq!(
                descriptor, "(I)Ljava/lang/String;",
                "the descriptor is what distinguishes an overload; a refusal \
                 report without it is not actionable (contract §1.7)"
            );
            // `#[track_caller]` provenance: the site must be *this* file, not
            // registry.rs. That is the whole point of the attribute — a
            // refusal that blames the registry's own source line says nothing
            // about which registrar produced the stub.
            let site = registered_by
                .as_deref()
                .expect("register() captures its caller via #[track_caller]");
            assert!(
                site.contains("jdk_only_registry.rs"),
                "registered_by must name the calling registrar, got {site:?}"
            );
        }
        other => panic!("expected SyntheticNativeRegistered, got {other:?}"),
    }
    assert_eq!(
        refused[0].kind(),
        "synthetic-native-registered",
        "the kind tag is a wire format shared with the JSON report"
    );
}

// ---------------------------------------------------------------------------
// 2b. A refusal is NOT automatically a retirement
// ---------------------------------------------------------------------------

/// `register` is last-write-wins and the `JdkOnly` arm returns WITHOUT
/// inserting, so refusing a `SyntheticStub` whose triple is already owned does
/// not hand the method to real JDK bytecode — **the earlier native survives as
/// the winner**, and the two modes run different code for that triple.
///
/// Measured on the shipping registry at nine triples (`--dump-native-registry`
/// in both modes, compared on the winning registration): four are deliberate
/// per-mode branching in `lang_system.rs`, and five are this shape —
/// `java/util/logging/Handler.{getLevel,setLevel}` and
/// `LogRecord.{getLevel,getMessage,getSequenceNumber}`, where a `phases_early`
/// `Intrinsic` outlives the later `SyntheticStub` that displaces it in
/// compatible mode.
///
/// Nothing could see that species before this field existed. `registrar_drift`
/// compares a synthetic-only pass against a shipping one and this is two
/// SHIPPING passes; `duplicate_registration_gate` records it as blind spot 3
/// (a dropped registration leaves no census row at all); and the §1.4 shadow
/// census skips `NativeKind::Intrinsic`, which is exactly the survivor's kind
/// in all five.
#[test]
fn a_refusal_over_an_owned_triple_names_the_survivor() {
    let mut reg = NativeMethodRegistry::new();
    reg.set_compatibility_mode(CompatibilityMode::JdkOnly);

    // The earlier registration — admissible under JdkOnly, so it enters.
    register_as(
        &mut reg,
        NativeKind::Intrinsic,
        "com/example/Survivor",
        "m",
        "()I",
        cb_one,
    );
    // The later one is refused. The slot does NOT revert to bytecode.
    register_as(
        &mut reg,
        NativeKind::SyntheticStub,
        "com/example/Survivor",
        "m",
        "()I",
        cb_two,
    );

    assert!(
        reg.find("com/example/Survivor", "m", "()I").is_some(),
        "the earlier Intrinsic still owns the slot — that is the defect this \
         test pins, not a bug in the test"
    );

    let survived = reg.refusals_that_left_a_survivor();
    assert_eq!(
        survived.len(),
        1,
        "one refusal landed on an owned triple, got {survived:?}"
    );
    let (class, method, descriptor, survivor) = survived[0];
    assert_eq!(
        (class, method, descriptor),
        ("com/example/Survivor", "m", "()I")
    );
    assert!(
        survivor.starts_with("intrinsic@"),
        "the survivor's KIND leads, because an Intrinsic survivor is the one \
         the shadow census can never report: {survivor:?}"
    );
    assert!(
        survivor.contains("jdk_only_registry.rs"),
        "and its site must name the registrar that produced it, got {survivor:?}"
    );

    // The JSON the `--jdk-only-report` carries must say so too: a consumer that
    // reads only `kind()` sees "synthetic-native-registered" for both shapes.
    let json = reg.refused_registrations()[0].to_json();
    assert!(
        json.contains("\"survivor\""),
        "the report row must carry the field, got {json}"
    );
}

#[test]
fn strict_registration_keeps_bridges_and_intrinsics() {
    let mut reg = NativeMethodRegistry::new();
    reg.set_compatibility_mode(CompatibilityMode::JdkOnly);

    register_as(
        &mut reg,
        NativeKind::Bridge,
        "com/example/Strict",
        "syscall",
        "()V",
        cb_one,
    );
    register_as(
        &mut reg,
        NativeKind::Intrinsic,
        "com/example/Strict",
        "fastpath",
        "()I",
        cb_two,
    );

    assert_eq!(
        reg.find_with_kind("com/example/Strict", "syscall", "()V")
            .map(|(_, k)| k),
        Some(NativeKind::Bridge),
        "a Bridge crosses a VM boundary the VM genuinely cannot run in \
         bytecode — strict mode keeps it (contract §1.5)"
    );
    assert_eq!(
        reg.find_with_kind("com/example/Strict", "fastpath", "()I")
            .map(|(_, k)| k),
        Some(NativeKind::Intrinsic),
        "a reviewed Intrinsic is semantics-preserving — strict mode keeps it \
         (contract §1.4)"
    );
    assert!(
        reg.refused_registrations().is_empty(),
        "SyntheticStub is the ONLY kind JdkOnly rejects"
    );
}

#[test]
fn native_kind_allowed_in_matches_the_terminology_table() {
    // Contract §1 terminology table, in code. Compatible admits everything;
    // JdkOnly admits everything except the fake.
    for kind in [
        NativeKind::Intrinsic,
        NativeKind::Bridge,
        NativeKind::SyntheticStub,
    ] {
        assert!(
            kind.allowed_in(CompatibilityMode::Compatible),
            "{} must be allowed under Compatible — that is today's behaviour",
            kind.as_str()
        );
    }
    assert!(NativeKind::Intrinsic.allowed_in(CompatibilityMode::JdkOnly));
    assert!(NativeKind::Bridge.allowed_in(CompatibilityMode::JdkOnly));
    assert!(
        !NativeKind::SyntheticStub.allowed_in(CompatibilityMode::JdkOnly),
        "SyntheticStub is the forbidden row of the terminology table"
    );

    // The `as_str` spellings are a wire format (census JSON `kind` field, and
    // `NativeShadowsBytecode::native_kind`). Pin them.
    assert_eq!(NativeKind::Intrinsic.as_str(), "intrinsic");
    assert_eq!(NativeKind::Bridge.as_str(), "bridge");
    assert_eq!(NativeKind::SyntheticStub.as_str(), "synthetic-stub");
}

// ---------------------------------------------------------------------------
// INVARIANT (research plan): a strict registry holds zero SyntheticStub rows
// ---------------------------------------------------------------------------

#[test]
fn strict_registry_census_contains_zero_synthetic_stub_entries() {
    let mut reg = NativeMethodRegistry::new();
    reg.set_compatibility_mode(CompatibilityMode::JdkOnly);

    // A representative mix: two keepers and two fakes, interleaved so a
    // "refuse the tail after the first stub" bug would be visible.
    register_as(&mut reg, NativeKind::Bridge, "a/A", "one", "()V", cb_one);
    register_as(
        &mut reg,
        NativeKind::SyntheticStub,
        "a/A",
        "two",
        "()V",
        cb_one,
    );
    register_as(
        &mut reg,
        NativeKind::Intrinsic,
        "a/A",
        "three",
        "()I",
        cb_two,
    );
    register_as(
        &mut reg,
        NativeKind::SyntheticStub,
        "b/B",
        "four",
        "()V",
        cb_two,
    );

    let census = reg.census();
    let stubs: Vec<_> = census
        .iter()
        .filter(|e| e.kind == NativeKind::SyntheticStub)
        .map(|e| format!("{}.{}{}", e.class, e.name, e.descriptor))
        .collect();
    assert!(
        stubs.is_empty(),
        "acceptance criterion (contract §11): the final native registry in \
         strict mode must contain zero SyntheticStub entries; found {stubs:?}"
    );
    assert_eq!(
        census.len(),
        2,
        "the two keepers must still be registered — strict mode drops the \
         fakes, not the surface"
    );
    assert_eq!(
        reg.refused_registrations().len(),
        2,
        "every refusal is recorded, not merely dropped: the operator has to be \
         able to name what went missing"
    );
}

// ---------------------------------------------------------------------------
// 3. Overwrite history
// ---------------------------------------------------------------------------

#[test]
fn census_retains_overwrite_history() {
    // Compatible mode, because the interesting case is exactly the one the
    // 157-stub backlog will produce: a stub registered by an early pass and
    // replaced by a correct bridge from a later one. Under JdkOnly the stub
    // never lands, so there is nothing to overwrite.
    let mut reg = NativeMethodRegistry::new();
    register_as(
        &mut reg,
        NativeKind::SyntheticStub,
        "com/example/Over",
        "m",
        "()V",
        cb_one,
    );
    register_as(
        &mut reg,
        NativeKind::Bridge,
        "com/example/Over",
        "m",
        "()V",
        cb_two,
    );

    let census = reg.census();
    assert_eq!(
        census.len(),
        2,
        "re-registration is a second registration *event*; the census is the \
         history, not the live table (which `dump_registrations` order also \
         reflects)"
    );
    assert_eq!(census[0].kind, NativeKind::SyntheticStub);
    assert_eq!(
        census[0].overwrote, None,
        "a first registration displaced nothing"
    );
    assert_eq!(census[1].kind, NativeKind::Bridge);
    assert_eq!(
        census[1].overwrote,
        Some(NativeKind::SyntheticStub),
        "Some(SyntheticStub) on a Bridge row is progress and must be visible; \
         without it, 'fixed' and 'was always fine' look identical"
    );

    // The live table is the winner.
    assert_eq!(
        reg.kind_of("com/example/Over", "m", "()V"),
        Some(NativeKind::Bridge)
    );
}

#[test]
fn census_records_the_registration_site() {
    let mut reg = NativeMethodRegistry::new();
    register_as(&mut reg, NativeKind::Bridge, "s/S", "m", "()V", cb_one);
    let site = reg.census()[0]
        .registered_by
        .clone()
        .expect("register() is #[track_caller]");
    assert!(
        site.contains("jdk_only_registry.rs"),
        "the census must attribute a registration to its registrar, not to \
         registry.rs; got {site:?}"
    );
    assert!(
        site.rsplit(':')
            .next()
            .is_some_and(|l| l.parse::<u32>().is_ok()),
        "registered_by is 'file:line'; got {site:?}"
    );
}

// ---------------------------------------------------------------------------
// 4. Determinism
// ---------------------------------------------------------------------------

/// Populate a registry with a fixed sequence. Extracted so both halves of the
/// determinism test share one set of `#[track_caller]` sites.
fn populate(reg: &mut NativeMethodRegistry) {
    register_as(reg, NativeKind::Bridge, "d/D", "alpha", "()V", cb_one);
    register_as(reg, NativeKind::SyntheticStub, "d/D", "beta", "()I", cb_two);
    register_as(reg, NativeKind::Bridge, "d/D", "beta", "()I", cb_one);
    register_as(reg, NativeKind::Intrinsic, "d/E", "gamma", "(J)J", cb_two);
}

#[test]
fn census_is_deterministic_across_registries() {
    let mut a = NativeMethodRegistry::new();
    let mut b = NativeMethodRegistry::new();
    populate(&mut a);
    populate(&mut b);

    assert_eq!(
        census_shape(&a),
        census_shape(&b),
        "two registries built by the same sequence must census identically — \
         CI diffs this artifact between runs and between OS legs, so a \
         hash-order-dependent census would make the gate flap"
    );

    // ...and repeating the query on one registry is stable too (`census()` is
    // a pure read; it must not consume or reorder anything).
    assert_eq!(census_shape(&a), census_shape(&a));
}

#[test]
fn strict_census_is_deterministic() {
    let mut a = NativeMethodRegistry::new();
    a.set_compatibility_mode(CompatibilityMode::JdkOnly);
    let mut b = NativeMethodRegistry::new();
    b.set_compatibility_mode(CompatibilityMode::JdkOnly);
    populate(&mut a);
    populate(&mut b);

    assert_eq!(census_shape(&a), census_shape(&b));
    let refused_a: Vec<_> = a
        .refused_registrations()
        .iter()
        .map(|v| v.summary())
        .collect();
    let refused_b: Vec<_> = b
        .refused_registrations()
        .iter()
        .map(|v| v.summary())
        .collect();
    assert_eq!(
        refused_a, refused_b,
        "the refusal list is part of the strict-mode artifact and must be \
         order-stable in registration order (contract §4)"
    );
}

// ---------------------------------------------------------------------------
// 5. Invocation counters
// ---------------------------------------------------------------------------

#[test]
fn invocations_of_kind_counts_dispatches() {
    let mut reg = NativeMethodRegistry::new();
    register_as(
        &mut reg,
        NativeKind::Bridge,
        "i/I",
        "bridged",
        "()V",
        cb_one,
    );
    register_as(
        &mut reg,
        NativeKind::Intrinsic,
        "i/I",
        "fast",
        "()I",
        cb_two,
    );
    register_as(
        &mut reg,
        NativeKind::SyntheticStub,
        "i/I",
        "fake",
        "()V",
        cb_one,
    );

    for kind in [
        NativeKind::Bridge,
        NativeKind::Intrinsic,
        NativeKind::SyntheticStub,
    ] {
        assert_eq!(
            reg.invocations_of_kind(kind),
            0,
            "no dispatch has happened yet ({})",
            kind.as_str()
        );
    }

    let bridged = reg
        .resolve_id("i/I", "bridged", "()V")
        .expect("registered triple must resolve to a slot handle");
    let fast = reg
        .resolve_id("i/I", "fast", "()I")
        .expect("registered triple must resolve to a slot handle");

    // `record_invocation` takes `&self` on purpose: dispatch holds a shared
    // borrow of the frozen registry and cannot take `&mut`.
    let frozen: &NativeMethodRegistry = &reg;
    for _ in 0..3 {
        frozen.record_invocation(bridged);
    }
    frozen.record_invocation(fast);

    assert_eq!(reg.invocations_of_kind(NativeKind::Bridge), 3);
    assert_eq!(reg.invocations_of_kind(NativeKind::Intrinsic), 1);
    assert_eq!(
        reg.invocations_of_kind(NativeKind::SyntheticStub),
        0,
        "a stub nothing calls is free to delete — that difference is the whole \
         point of the counter (contract §9 counts block)"
    );

    // The per-row view agrees with the per-kind totals.
    let census = reg.census();
    let by_name = |n: &str| {
        census
            .iter()
            .find(|e| e.name == n)
            .unwrap_or_else(|| panic!("no census row for {n}"))
            .invocations
    };
    assert_eq!(by_name("bridged"), 3);
    assert_eq!(by_name("fast"), 1);
    assert_eq!(by_name("fake"), 0);
}

#[test]
fn invocation_counters_survive_re_registration_on_the_winning_slot() {
    // Documented consequence of counting per *slot*: a re-registered triple's
    // superseded row reports 0 and the winner owns the whole run's count. This
    // pins the documented behaviour so a future refactor to per-registration
    // counters is a deliberate, visible change rather than silent drift.
    let mut reg = NativeMethodRegistry::new();
    register_as(
        &mut reg,
        NativeKind::SyntheticStub,
        "r/R",
        "m",
        "()V",
        cb_one,
    );
    let id = reg.resolve_id("r/R", "m", "()V").expect("resolvable");
    reg.record_invocation(id);
    register_as(&mut reg, NativeKind::Bridge, "r/R", "m", "()V", cb_two);
    reg.record_invocation(id);

    let census = reg.census();
    assert_eq!(census.len(), 2);
    assert_eq!(
        census[0].invocations, 0,
        "the superseded row does not own the slot any more"
    );
    assert_eq!(
        census[1].invocations, 2,
        "the winner reports the slot's whole-run count"
    );
}
