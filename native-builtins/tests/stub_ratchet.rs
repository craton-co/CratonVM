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
//! requires lowering the baseline (see `docs/internal/stub-ratchet.md`).
//!
//! Wire into CI with:
//!
//! ```text
//! cargo test -p cratonvm-native-builtins --test stub_ratchet
//! ```

use cratonvm_native_api::{NativeKind, NativeMethodRegistry};
use cratonvm_native_builtins::register_essential_natives;

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
const BASELINE_SYNTHETIC_STUBS: usize = 157;

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
         docs/internal/stub-ratchet.md.",
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
