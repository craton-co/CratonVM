// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! STUB-RATCHET GATE — a one-way compatibility ratchet on synthetic-stub natives.
//!
//! Project rule (see `docs/synthetic-vs-real-explained.md` and
//! `feedback_no_synthetic_stubs`): **no NEW synthetic stubs**. The native
//! overlay tags every registration with a [`NativeKind`]:
//!
//!   * `Intrinsic` — a correct fast-path for a hot method (kept forever).
//!   * `Bridge`    — a native the VM genuinely needs (OS syscalls, `sun.*`
//!                   internals, classes with no real bytecode). It *is* the
//!                   real behavior.
//!   * `SyntheticStub` — a fake: placeholder / approximate / wrong return
//!                   values, fabricated objects, or "fake main" launcher
//!                   short-circuits. These shadow correct real bytecode and
//!                   are the removal target.
//!
//! This test builds the **default native registry the way the VM does** (via
//! the public `register_essential_natives` entrypoint — the same function
//! `vm/src/vm/vm_init.rs` calls unconditionally on the real-JDK boot path),
//! censuses how many registrations are tagged `SyntheticStub`, and asserts the
//! count has not RISEN above a frozen [`BASELINE_SYNTHETIC_STUBS`] constant.
//!
//! It is a *ratchet*: a change that ADDS a synthetic stub pushes the count over
//! the baseline and fails CI; a change that REMOVES one is welcome and only
//! requires lowering the baseline (see `stub-ratchet.md`).
//!
//! Wire into CI with:
//!
//! ```text
//! cargo test -p cratonvm-native-builtins --test stub_ratchet
//! ```

use cratonvm_native_api::{NativeKind, NativeMethodRegistry};
use cratonvm_native_builtins::register_essential_natives;
use cratonvm_types::compat::CompatibilityMode;

/// Frozen upper bound on the number of `SyntheticStub`-tagged registrations in
/// the default (real-JDK / `register_essential_natives`) registry.
///
/// This is the exact current observed count. The ratchet has zero slack: adding
/// one synthetic stub fails, while removing one requires lowering the baseline
/// in the same change to lock in the improvement.
///
/// ## How to (re)compute the baseline
///
/// The exact count is produced at runtime by this very test. Run it once and
/// read the observed value off the assertion / stdout line:
///
/// ```text
/// cargo test -p cratonvm-native-builtins --test stub_ratchet -- --nocapture
/// ```
///
/// The test prints
///
/// ```text
/// stub-ratchet: <N> SyntheticStub registrations (baseline <BASELINE>)
/// ```
///
/// Set this constant to `<N>` and keep [`SLACK`] at zero. See
/// `docs/contributing/stub-ratchet.md`.
///
/// # 157 → 165, 2026-08-05 (JDK-only wave 2, lane L7 item 4)
///
/// The ratchet moved **up**, and the explanation its own failure message asks
/// for is that this change added no fake: it re-labelled eight that were
/// already there and were hidden from this count by the wrong tag.
///
/// The eight are `java/util/function/Function.{identity,compose,andThen}`,
/// `UnaryOperator.identity`, and `Function$Identity.{apply,andThen,compose}`.
/// Every one of them fabricates a `Function$Identity` / `Function$AndThen` /
/// `Function$Compose` stand-in, and **no JDK declares any of those names** —
/// real `Function.identity()` is one line of invokedynamic returning `t -> t`,
/// and `compose`/`andThen` are default methods that return a lambda. They were
/// tagged `Bridge`, which asserts "no working real-bytecode fallback exists".
/// There is one, in `java.base`, and `--jdk-only` now runs it: the
/// `JdkOnlyBreadthProbe` `lambdas` line is byte-identical to HotSpot 25.
///
/// So the count rising is this gate becoming *more* honest, not less: the
/// backlog it exists to measure was under-reported by eight. The direction to
/// be suspicious of is a `SyntheticStub` quietly becoming a `Bridge`, which
/// lowers this number while changing nothing — and which is exactly the shape
/// L6's unadjudicated-`Bridge` ratchet is being built to catch.
const BASELINE_SYNTHETIC_STUBS: usize = 165;

/// Slack added on top of the observed count when (re)freezing the baseline.
/// Documented here so the recount instructions and the constant stay in sync.
const SLACK: usize = 0;

/// Build the default native registry exactly as the VM's real-JDK boot path
/// does, and return `(synthetic_stub_count, total_registrations)`.
///
/// We deliberately use the *public* registration entrypoint rather than poking
/// at registry internals so this test exercises the same surface the VM ships.
/// `register_essential_natives` is the unconditional real-JDK registrar (see
/// `vm/src/vm/vm_init.rs`); the synthetic-override block is feature-gated
/// (`synthetic-jdk`, off by default) and intentionally NOT counted here — the
/// ratchet guards the default build.
fn census() -> (usize, usize) {
    let mut registry = NativeMethodRegistry::new();
    register_essential_natives(&mut registry);

    // `dump_registrations()` is the registry's public census API: it yields one
    // `(class, method, descriptor, NativeKind)` row per registration, in
    // registration order. Count the `SyntheticStub` rows.
    let rows = registry.dump_registrations();
    let synthetic = rows
        .iter()
        .filter(|(_, _, _, kind)| *kind == NativeKind::SyntheticStub)
        .count();
    (synthetic, rows.len())
}

/// THE GATE: the synthetic-stub count must not exceed the frozen baseline.
///
/// A failure here means a change ADDED one or more synthetic stubs to the
/// default registry. The fix is to make the new native a real `Bridge`/
/// `Intrinsic` (i.e. correct behavior) — NOT to bump the baseline. Only raise
/// the baseline when a stub is genuinely, unavoidably needed; prefer fixing the
/// underlying VM gap so real bytecode runs (see `feedback_no_synthetic_stubs`).
#[test]
fn synthetic_stub_count_does_not_regress() {
    let (synthetic, total) = census();

    // Always surface the live number (visible with `-- --nocapture`) so the
    // baseline can be (re)frozen to `synthetic + SLACK` without guessing.
    println!(
        "stub-ratchet: {synthetic} SyntheticStub registrations \
         out of {total} total (baseline {BASELINE_SYNTHETIC_STUBS}, slack {SLACK})"
    );

    assert!(
        synthetic <= BASELINE_SYNTHETIC_STUBS,
        "STUB-RATCHET REGRESSION: {synthetic} SyntheticStub natives now registered, \
         exceeding the frozen baseline of {BASELINE_SYNTHETIC_STUBS}. A change added a \
         NEW synthetic stub. Make the new native a real Bridge/Intrinsic (correct \
         behavior) instead of a fake — do NOT just raise the baseline. If the stub is \
         genuinely, unavoidably needed, re-freeze BASELINE_SYNTHETIC_STUBS to \
         {synthetic} + SLACK ({}) and explain why in the PR. See \
         stub-ratchet.md.",
        synthetic + SLACK,
    );
}

/// Sanity guard for the census itself: if `register_essential_natives` ever
/// stops registering anything (a wiring break), the ratchet would pass
/// vacuously. Assert the registry is non-trivially populated so a "0 stubs
/// because 0 registrations" never masquerades as a green ratchet.
#[test]
fn essential_registry_is_populated() {
    let (_synthetic, total) = census();
    // `> 100` was too weak to be a vacuity guard. The ratchet is a ratio
    // argument — "157 of 9,320" — and the denominator was never asserted, so
    // a wiring break that dropped 9,000 registrations would still leave
    // `total` above 100 and the ratchet green with far fewer stubs. This floor
    // sits well below the live count (9,320 as of 2026-07-30) so ordinary
    // churn does not trip it, but a collapse of the surface does.
    const MIN_TOTAL_REGISTRATIONS: usize = 8_000;
    assert!(
        total >= MIN_TOTAL_REGISTRATIONS,
        "register_essential_natives produced only {total} registrations, below the \
         {MIN_TOTAL_REGISTRATIONS} floor — the census entrypoint or a whole \
         registration module looks broken, which would make the stub-ratchet pass \
         vacuously. Expected the real-JDK boot path to register ~9,300 natives."
    );
}

// ---------------------------------------------------------------------------
// STRICT (JDK-only) CENSUS — docs/feature-designs/jdk-only-mode.md §4 and §11
//
// Strictness is a *runtime policy* on the registry, not a build feature: the VM
// calls `set_compatibility_mode(CompatibilityMode::JdkOnly)` once at init,
// BEFORE any `register_*` pass (contract §8), and `register()` then refuses a
// `NativeKind::SyntheticStub` outright, recording a
// `JdkOnlyViolation::SyntheticNativeRegistered` so the run can name what went
// missing.
//
// The three tests below measure that same `register_essential_natives` surface
// through the strict policy. They are purely additive: the compatibility-mode
// baseline above is untouched, and nothing here tightens
// `BASELINE_SYNTHETIC_STUBS`. Wave 1 is measurement, not deletion (contract
// §10) — the point of these tests is to make the size and shape of the backlog
// a number CI prints on every run, not to delete anything yet.
//
// The same zero-stub invariant is asserted against a hand-built synthetic mix
// in `native-api/tests/jdk_only_registry.rs`; here it is asserted against the
// real boot-path registrar, which is the one that has 165 stubs in it.
// ---------------------------------------------------------------------------

/// Vacuity floor for the *strict* registry, mirroring `MIN_TOTAL_REGISTRATIONS`
/// in [`essential_registry_is_populated`].
///
/// This is a collapse detector, not a measurement. Strict mode is expected to
/// shed the stub registrations plus whatever aliases hang off them (see
/// [`strict_registry_drops_only_the_stubs`] for why that fallout is real), so
/// the floor sits well below the compatibility-mode floor. If the strict
/// registry ever drops under it, `set_compatibility_mode` is refusing far more
/// than the stubs, and "zero synthetic stubs" would be true only because the
/// registry is empty.
const STRICT_MIN_TOTAL_REGISTRATIONS: usize = 7_500;

/// Build the default native registry the way `--jdk-only` does: set the
/// VM-scoped strict policy *first*, then run the same public
/// `register_essential_natives` entrypoint `vm/src/vm/vm_init.rs` calls on the
/// real-JDK boot path.
///
/// Ordering is load-bearing and mirrors contract §8: a mode set *after*
/// registration would leave every stub already in the table and make this whole
/// section pass for the wrong reason.
///
/// Returns `(synthetic_stub_count, total_registrations, refused_registrations)`.
fn strict_census() -> (usize, usize, usize) {
    let mut registry = NativeMethodRegistry::new();
    registry.set_compatibility_mode(CompatibilityMode::JdkOnly);
    register_essential_natives(&mut registry);

    let rows = registry.dump_registrations();
    let synthetic = rows
        .iter()
        .filter(|(_, _, _, kind)| *kind == NativeKind::SyntheticStub)
        .count();
    (synthetic, rows.len(), registry.refused_registrations().len())
}

/// Acceptance criterion (contract §11): **the final native registry in strict
/// mode contains zero `SyntheticStub` entries.**
///
/// This passes *today*, and it is worth being precise about why: not because
/// the 165 stubs are gone, but because `register()` refuses them at the door
/// under `JdkOnly`. That is exactly the property CI's zero-stub census asserts
/// against a booted VM, so it is worth pinning here too — it is the cheap,
/// hermetic version of the same check, with no JDK image and no subprocess.
///
/// A failure means a `SyntheticStub` reached the live table despite the strict
/// policy: either a registrar bypasses `register()`, or the mode is being set
/// after registration rather than before it.
#[test]
fn strict_registry_has_zero_synthetic_stubs() {
    let (strict_stubs, strict_total, refused) = strict_census();

    println!(
        "stub-ratchet(strict): {strict_stubs} SyntheticStub registrations out of \
         {strict_total} total; {refused} registrations refused by JdkOnly"
    );

    assert_eq!(
        strict_stubs, 0,
        "acceptance criterion (contract §11) violated: {strict_stubs} SyntheticStub \
         natives are in the STRICT registry. Under CompatibilityMode::JdkOnly, \
         register() must refuse every SyntheticStub, so a non-zero count means a \
         registrar reached the slot table without going through register(), or \
         set_compatibility_mode was applied after registration instead of before it."
    );

    assert!(
        strict_total >= STRICT_MIN_TOTAL_REGISTRATIONS,
        "the strict registry holds only {strict_total} registrations, below the \
         {STRICT_MIN_TOTAL_REGISTRATIONS} floor. Zero synthetic stubs is then a \
         statement about an empty registry, not about the stub backlog."
    );
}

/// Strict mode drops the stubs — and, modulo aliasing, *only* the stubs.
///
/// The tempting assertion is exact subtraction:
/// `strict_total == compat_total - compat_stubs`. It is wrong, in a way worth
/// writing down because it will look like a bug to the next reader.
///
/// `NativeMethodRegistry::alias_class` (used by the JDBC, JBoss-MSC and
/// `net_phase_e` registrars) copies a class's natives to a second name by
/// walking the **live registration log** and re-`register()`ing each row it
/// finds. Under `JdkOnly` a refused stub never enters that log, so the alias
/// pass finds nothing to copy and the alias disappears too — one refusal can
/// remove more than one row. Hence:
///
/// * `compat_total - strict_total >= compat_stubs` — every stub is gone, plus
///   any aliases derived from one.
/// * `refused <= compat_stubs` — the mirror image. An alias copy that was
///   itself stub-tagged in compatible mode is counted in `compat_stubs`, but in
///   strict mode `alias_class` never attempts it, so no refusal is recorded for
///   it. `refused` counts refusals, not stubs.
///
/// Both bounds are one-sided on purpose. Pinning the alias fallout to an exact
/// number would make this test fail every time a registrar adds or removes an
/// `alias_class` call, which is churn, not regression.
#[test]
fn strict_registry_drops_only_the_stubs() {
    let (compat_stubs, compat_total) = census();
    let (_strict_stubs, strict_total, refused) = strict_census();

    let dropped = compat_total.saturating_sub(strict_total);

    println!(
        "stub-ratchet(strict): compatible {compat_total} rows ({compat_stubs} stubs) \
         -> strict {strict_total} rows; {dropped} rows dropped, {refused} refusals recorded"
    );

    assert!(
        strict_total <= compat_total,
        "strict mode registered MORE than compatible mode ({strict_total} > \
         {compat_total}). JdkOnly only ever refuses; it must never add a registration."
    );

    assert!(
        dropped >= compat_stubs,
        "strict mode dropped only {dropped} registrations but compatible mode has \
         {compat_stubs} SyntheticStub rows. Every stub row must be absent from the \
         strict registry (plus possibly some alias_class fallout, which is why this \
         is a lower bound and not equality). A shortfall means some stubs survive \
         the strict policy."
    );

    assert!(
        refused <= compat_stubs,
        "{refused} registrations were refused but compatible mode only has \
         {compat_stubs} SyntheticStub rows. A refusal with no corresponding stub \
         means JdkOnly is rejecting a Bridge or an Intrinsic — SyntheticStub is the \
         only kind it may reject (contract §4)."
    );

    assert!(
        strict_total >= STRICT_MIN_TOTAL_REGISTRATIONS,
        "the strict registry holds only {strict_total} registrations, below the \
         {STRICT_MIN_TOTAL_REGISTRATIONS} floor — strict mode is shedding whole \
         registration modules, not just stubs."
    );
}

/// THE END-STATE GATE, deliberately `#[ignore]`d.
///
/// [`strict_registry_has_zero_synthetic_stubs`] passes today for a weak reason:
/// `register()` refuses the stubs at the door. The 165 registrations still
/// exist in `native-builtins/src/`, still run on every boot, and are still what
/// an ordinary `--real-jdk` run dispatches into. **Refused is not retired.**
///
/// This test asserts the strong property — strict mode has *nothing to refuse*
/// — and stays ignored until all three of the following have landed:
///
/// 1. **Reclassify or delete the 165 `SyntheticStub` registrations** in
///    `native-builtins/src/`, subsystem by subsystem: each one becomes a real
///    `Bridge`/`Intrinsic` because it genuinely crosses a VM boundary, or it
///    goes away so the real JDK bytecode runs. This is explicitly *not* wave 1
///    work (contract §8: "do not edit `native-builtins/src/lib.rs`; the
///    165-stub reclassification is a separate wave with its own
///    subsystem-per-PR discipline").
/// 2. **Drive [`BASELINE_SYNTHETIC_STUBS`] to 0 in the same change** that
///    removes the last one. The ratchet is slack-free by design; leaving the
///    baseline at 165 after the stubs are gone would silently re-admit 165 new
///    ones.
/// 3. **Un-ignore this test** (delete the `#[ignore]`) so the zero is held,
///    and promote the CI `jdk-only` job from advisory to blocking, which is the
///    posture contract §11 calls for.
///
/// Until then it is run on demand:
/// `cargo test -p cratonvm-native-builtins --test stub_ratchet -- --ignored --nocapture`
#[test]
#[ignore = "wave 1 is measurement: the 165 stubs are refused at registration, not yet retired"]
fn strict_mode_refuses_nothing() {
    let (_strict_stubs, _strict_total, refused) = strict_census();

    assert_eq!(
        refused, 0,
        "{refused} SyntheticStub registrations still have to be refused at VM init. \
         Zero refusals is the real end state: it means the stubs were reclassified \
         or deleted at the source, not merely filtered out of the table on the way \
         in. See this test's doc comment for the three steps that must land first."
    );
}
