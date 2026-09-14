// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! DUPLICATE-REGISTRATION DETECTION — the analysis half of the shadowed-native
//! gate (`docs/known-issues/jdk-only/W6-4-duplicate-registration-gate.md`).
//!
//! [`NativeMethodRegistry::register`] is last-write-wins and **updates the
//! existing slot in place**, so a correct, guarded native silently loses to an
//! unguarded twin registered later and every symptom points at the wrong file.
//! Four instances each cost a full measure-fix-rebuild cycle before anyone
//! looked for the pattern:
//!
//!   1. `MethodHandles$Lookup.defineHiddenClass` — a late placeholder shadowed
//!      the real implementation; `lookupClass()` answered null.
//!   2. `Files.copy(Path,Path,CopyOption...)` — the winner never read its
//!      options argument; a copy onto an existing file overwrote silently.
//!   3. `Module.getResourceAsStream` — twice from inside the SAME function,
//!      ~9k lines apart; the wave-2 `opens` gate was dead code.
//!   4. `SSLContext.getInstance` — four competing registrations disagreeing
//!      with each other.
//!
//! The gate itself — the one that censuses the real boot path — lives in
//! `native-builtins/tests/duplicate_registration_gate.rs`, because building the
//! boot registry needs the registrar crates and `native-api` is below them in
//! the dependency graph. **This file pins the detector those assertions rest
//! on**, on registries it builds itself with its own callbacks, so a bug in the
//! analysis cannot quietly turn the gate into a test of nothing.
//!
//! Five claims:
//!
//!   1. A triple registered once is not reported.
//!   2. A triple registered twice reports the FIRST as shadowed and names the
//!      SECOND as the winner, in both directions of the provenance pair.
//!   3. A triple registered N times reports N-1 rows, all naming one winner —
//!      the shape instance 4 has, and the shape a ratchet needs (fixing one of
//!      four removes exactly one row).
//!   4. `kind_disagreement` fires when a stub displaces a bridge (instance 1),
//!      and `cross_file` is false for two registrations in one file
//!      (instance 3's shape — the classifier must not claim that one).
//!   5. `key()` carries no line number, so moving a registration within its
//!      file cannot turn a frozen baseline red.

use cratonvm_native_api::registry::{shadowed_registrations_in, ShadowedRegistration};
use cratonvm_native_api::{NativeContext, NativeKind, NativeMethodRegistry};
use cratonvm_types::error::MethodCallResult;
use cratonvm_types::Value;

fn stub_a(_ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    Ok(Some(Value::Int(1)))
}

fn stub_b(_ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    Ok(Some(Value::Int(2)))
}

fn stub_c(_ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    Ok(Some(Value::Int(3)))
}

/// A registry where nothing is registered twice reports nothing.
///
/// The negative control. Without it, a detector that returned an empty `Vec`
/// unconditionally would pass every other test in this file.
#[test]
fn a_registry_with_no_duplicates_reports_none() {
    let mut registry = NativeMethodRegistry::new();
    registry.register("a/A", "m", "()I", stub_a);
    registry.register("a/A", "m", "()J", stub_b);
    registry.register("b/B", "m", "()I", stub_c);

    let shadowed = registry.shadowed_registrations();
    assert!(
        shadowed.is_empty(),
        "three distinct triples, none shadowed; got {shadowed:?}"
    );
    // Same triple, different descriptor and different class are DIFFERENT
    // triples. `register` keys on all three, and a detector that grouped on
    // (class, name) alone would report two false positives here.
    assert_eq!(registry.census().len(), 3);
}

/// The core claim: the LOSER is reported, and it names the WINNER.
///
/// Both halves are load-bearing and were the expensive part of all four
/// instances. Knowing a triple was registered twice is not actionable; knowing
/// *which* of the two files the dispatch actually reaches is the whole fix.
#[test]
fn a_second_registration_shadows_the_first_and_the_winner_is_named() {
    let mut registry = NativeMethodRegistry::new();
    registry.register("a/A", "m", "()I", stub_a);
    let first_line = line!() - 1;
    registry.register("a/A", "m", "()I", stub_b);
    let second_line = line!() - 1;

    let shadowed = registry.shadowed_registrations();
    assert_eq!(shadowed.len(), 1, "one loser, not one row per registration");
    let row = &shadowed[0];
    assert_eq!(row.triple(), "a/A.m()I");

    let loser_site = row.shadowed_at.as_deref().expect("provenance recorded");
    let winner_site = row.winner_at.as_deref().expect("provenance recorded");
    assert!(
        loser_site.ends_with(&format!(":{first_line}")),
        "the FIRST registration must be the shadowed one; got {loser_site}"
    );
    assert!(
        winner_site.ends_with(&format!(":{second_line}")),
        "the SECOND registration must own the slot; got {winner_site}"
    );

    // And the registry agrees: dispatch reaches the winner's callback. This is
    // the fact that makes the row above mean something — `owns_slot` is derived
    // from the same reverse map the invocation counter is read through.
    let census = registry.census();
    assert!(!census[0].owns_slot, "row 0 lost");
    assert!(census[1].owns_slot, "row 1 won");
}

/// Four registrations of one triple — instance 4's shape — produce THREE rows,
/// all naming the same winner.
///
/// This is the shape a ratchet needs. A count that collapsed to one row per
/// triple would not move when three of the four competing `SSLContext
/// .getInstance` registrations were removed, so the gate could not score a
/// partial fix.
#[test]
fn n_registrations_produce_n_minus_one_rows_naming_one_winner() {
    let mut registry = NativeMethodRegistry::new();
    for cb in [stub_a, stub_b, stub_c, stub_a] {
        registry.register("javax/net/ssl/SSLContext", "getInstance", "()V", cb);
    }
    let shadowed = registry.shadowed_registrations();
    assert_eq!(shadowed.len(), 3, "four registrations, three losers");

    let winners: Vec<_> = shadowed
        .iter()
        .filter_map(|r| r.winner_at.clone())
        .collect();
    assert_eq!(winners.len(), 3);
    assert!(
        winners.windows(2).all(|w| w[0] == w[1]),
        "every loser must name the same winner; got {winners:?}"
    );
    // Exactly one census row owns the slot, whatever the multiplicity.
    assert_eq!(
        registry.census().iter().filter(|r| r.owns_slot).count(),
        1,
        "last-write-wins updates the slot in place; there is only ever one"
    );
}

/// `kind_disagreement` is the high-signal classifier: instance 1 was a
/// placeholder STUB displacing a real BRIDGE.
///
/// It is not cosmetic. `NativeKind` decides three separate things —
/// `CompatibilityMode::JdkOnly` refuses a `SyntheticStub`, `CRATONVM_NO_STUBS`
/// drops one, and `synthetic_stub_kind_should_yield_to_real_bytecode`
/// arbitrates for one — so a bridge displaced this way stops dispatching under
/// `--jdk-only` while its stated original would have been allowed.
#[test]
fn kind_disagreement_flags_a_stub_displacing_a_bridge() {
    let mut registry = NativeMethodRegistry::new();
    registry.register_with_kind("a/A", "m", "()I", stub_a, NativeKind::Bridge);
    // `register_with_kind` rather than plain `register`: `register`'s downgrade
    // rule deliberately PRESERVES a prior chosen kind over an unchosen
    // re-registration, so an ambient-default second call would report no
    // disagreement — correctly, because the kind did not actually change.
    registry.register_with_kind("a/A", "m", "()I", stub_b, NativeKind::SyntheticStub);

    let shadowed = registry.shadowed_registrations();
    assert_eq!(shadowed.len(), 1);
    let row = &shadowed[0];
    assert_eq!(row.shadowed_kind, NativeKind::Bridge);
    assert_eq!(row.winner_kind, NativeKind::SyntheticStub);
    assert!(
        row.kind_disagreement(),
        "a stub displacing a bridge is the instance-1 shape"
    );
    // Two registrations from this one file: `cross_file` must NOT claim it.
    // Instance 3 was same-file (same *function*, ~9k lines apart), which is why
    // the gate cannot be a cross-file check alone.
    assert!(
        !row.cross_file(),
        "both sites are in this test file; cross_file must be false"
    );
}

/// `key()` is line-number-free, so a frozen baseline survives edits above the
/// call site.
///
/// Fixed line bands in this repo's source-witness tests have gone stale exactly
/// this way. A baseline that goes red when someone adds a comment is a baseline
/// people delete.
#[test]
fn the_baseline_key_carries_no_line_number() {
    let mut registry = NativeMethodRegistry::new();
    registry.register("java/nio/file/Files", "copy", "()V", stub_a);
    registry.register("java/nio/file/Files", "copy", "()V", stub_b);

    let shadowed = registry.shadowed_registrations();
    let key = shadowed[0].key();
    assert!(
        key.starts_with("java/nio/file/Files.copy()V\t"),
        "the triple leads the key so two triples sharing a file pair stay \
         distinct rows; got {key}"
    );
    assert!(
        !key.rsplit('\t').next().unwrap_or_default().contains(':'),
        "no `file:line` may survive into the key; got {key}"
    );
    // Both registrations are in THIS file, so both halves resolve to it and the
    // pair is a same-file pair.
    let file = shadowed[0].winner_file().expect("provenance recorded");
    assert!(
        file.ends_with("shadowed_registration_detection.rs"),
        "provenance must name this test file, not registry.rs — `register` is \
         `#[track_caller]` and must stay that way; got {file}"
    );
    assert!(
        !file.contains('\\'),
        "separators must be normalised: {file}"
    );
}

/// The free function and the registry method are the same analysis.
///
/// The gate in `native-builtins/tests` calls the free function over a census it
/// already has in hand; this file mostly exercises the method. They must not be
/// allowed to drift.
#[test]
fn the_free_function_and_the_method_agree() {
    let mut registry = NativeMethodRegistry::new();
    registry.register("a/A", "m", "()I", stub_a);
    registry.register("a/A", "m", "()I", stub_b);
    registry.register("b/B", "n", "()V", stub_c);

    let via_method: Vec<ShadowedRegistration> = registry.shadowed_registrations();
    let via_function = shadowed_registrations_in(&registry.census());
    assert_eq!(via_method, via_function);
}
