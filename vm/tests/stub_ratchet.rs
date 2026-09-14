// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! THE `vm`-CRATE HALF OF THE STUB RATCHET — the two registrars
//! `native-builtins/tests/stub_ratchet.rs` structurally cannot see.
//!
//! # What this file is, and what it deliberately is NOT
//!
//! It is **not** a second copy of the stub census.
//! `native-builtins/tests/stub_ratchet.rs` owns
//! `BASELINE_SYNTHETIC_STUBS_{MANAGEMENT,NO_MANAGEMENT}`, it is the one CI
//! runs, and two count-ratchets over "the same" VM is the configuration that
//! already cost this project weeks: `regression-suite/bridge-ratchet.sh`'s own
//! header records two gates disagreeing by 364 registrations because one was
//! looking at a fraction of the registry. A second baseline here would
//! reproduce that shape deliberately.
//!
//! It is the part of docs/known-issues/jdk-only/W7-30-stub-ratchet-boot-path-scope.md
//! §6 that needs the `vm` crate and can be settled **without a census**.
//!
//! # The residual it closes
//!
//! `vm_init`'s real-JDK arm calls 48 registrars. `native-builtins` replays 46;
//! it cannot replay `crate::runtime::instrument::register_instrumentation_natives`
//! or `register_self_attach_natives`, because `native-builtins` dev-depending
//! on `vm` is a dependency cycle. That was ratcheted as
//! `UNMODELLED_VM_CRATE_REGISTRARS = 2` and otherwise **admitted without a
//! bound**: "two registrars are outside the census" says nothing about what
//! they contribute to the number the census freezes.
//!
//! They contribute nothing, and that is now measured here rather than assumed.
//! It is not self-evident from the registrars: `register_instrumentation_natives`
//! makes fourteen plain `r.register(...)` calls that state no kind at all, and
//! `register_self_attach_natives` four more. A plain `register` takes the
//! AMBIENT category, and `NativeMethodRegistry::effective_category` is
//! `current_category.unwrap_or(NativeKind::SyntheticStub)` — every registrar in
//! `vm_init`'s sequence saves and restores its own category, so at `vm_init`'s
//! top level the ambient is the constructor's `None`. **Called bare, those
//! eighteen registrations would land as unscoped `SyntheticStub`, and
//! `--jdk-only` would refuse the whole `java.lang.instrument` and self-attach
//! surface by accident rather than by decision.**
//!
//! They do not, because `vm_init` wraps both calls in an explicit
//! `set_category(NativeKind::Bridge)` and restores it afterwards. That scope is
//! the load-bearing part, it lives in a file neither gate replays, and until
//! now nothing asserted it. [`the_instrument_registrars_run_under_a_bridge_scope`]
//! is the source witness for it; [`the_vm_crate_registrars_add_no_synthetic_stub`]
//! is the behavioural half.
//!
//! # What is still NOT closed, and what closing it needs
//!
//! The other half of §6: `vm_init` makes eight inline
//! `native_methods.register*` calls in that arm, of which exactly one states
//! `NativeKind::SyntheticStub` —
//! `io/quarkus/bootstrap/runner/RunnerClassLoader.close()V`. That one stub is
//! outside every census in this repository, so `BASELINE_SYNTHETIC_STUBS` is a
//! **floor by one**. `native-builtins/tests/common/vm_init_boot_path.rs`'s
//! `the_inline_registrations_in_vm_init_are_enumerated` ratchets it so a second
//! one cannot arrive unnoticed, but it cannot COUNT it.
//!
//! Counting it means running the arm, and the arm is 666 lines in the middle of
//! `SharedVm::new` — there is no registration-only entry point to call. So the
//! full "move the gate to `vm/tests/`" prescribed by W7-30 §6(a) is blocked on
//! extracting one from `vm/src/vm/vm_init.rs`:
//!
//! ```text
//! pub(crate) fn register_real_jdk_boot_natives(
//!     native_methods: &mut NativeMethodRegistry,
//!     shim_selection: cratonvm_native_builtins::app_shims::ShimSelection,
//! )
//! ```
//!
//! with `SharedVm::new`'s `#[cfg(not(feature = "synthetic-jdk"))]` arm reduced
//! to a call to it. That is a `vm/src` change with a real review surface (the
//! arm interleaves registration with VM state the extraction must not capture),
//! and it must land with a re-freeze of both baselines from one real run,
//! because the count grows by the inline registrations. Until then this file
//! closes the half that needs no census, and says so instead of implying the
//! move happened.
//!
//! # 2026-08-20 (H3-1) — two things about the sibling that a reader here needs
//!
//! 1. **`native-builtins/tests/stub_ratchet.rs` did not COMPILE between
//!    2026-08-19 and 2026-08-20.** Merge `26e4b5db4` spliced two versions of
//!    `synthetic_stub_count_does_not_regress`'s failure message together and
//!    left both argument lists, so the file was a parse error, not a red gate.
//!    That is worse than red in exactly the way `G89-1` §1 describes: a broken
//!    gate says nothing about the next change, and this one is BLOCKING in CI
//!    (`ci.yml`, both configurations) and is what the P0 *Residual synthetic
//!    native set* row cites as its evidence. Repaired in the same change as
//!    this note; `rustfmt --edition 2021 --check <file>` reproduces the
//!    original diagnosis (`unknown start of token: \`) and is the cheapest way
//!    to check a merge of a message-heavy `assert!` before paying for a build.
//! 2. **All four of that file's frozen constants carry
//!    `H3-1 REBASELINE REQUIRED` markers** and are deliberately left at their
//!    pre-change values. Seven `java.util.function` stubs were deleted from
//!    `native-builtins/src/phases_late/streams.rs`, so both columns are
//!    expected to fall by 7 in both configurations. The expected values are
//!    written down as PREDICTIONS beside the command that measures them; none
//!    of them has been run. Do not copy them into the constants — run the
//!    command.
//!
//! Neither touches this file's own assertions, which freeze no count.
//!
//! ```text
//! cargo test -p cratonvm-vm --test stub_ratchet -- --nocapture
//! ```

use cratonvm_native_api::{NativeKind, NativeMethodRegistry};
use cratonvm_vm::runtime::instrument::{
    register_instrumentation_natives, register_self_attach_natives,
};

/// Replay the two `vm`-crate registrars exactly as `vm_init` calls them:
/// inside `set_category(NativeKind::Bridge)`, restored afterwards.
///
/// The scope is copied from `vm_init`, not invented here — see
/// [`the_instrument_registrars_run_under_a_bridge_scope`], which fails if
/// `vm_init` stops doing it. A replay that silently improved on the source
/// would make this whole file a statement about a registry no build produces.
fn instrument_registrars(r: &mut NativeMethodRegistry) {
    let prev = r.current_category();
    r.set_category(NativeKind::Bridge);
    register_instrumentation_natives(r);
    register_self_attach_natives(r);
    r.set_category(prev);
}

/// The two registrars outside `native-builtins`' census contribute **zero**
/// `SyntheticStub` rows and **zero** unscoped registrations.
///
/// This is what makes `UNMODELLED_VM_CRATE_REGISTRARS = 2` a bounded admission
/// rather than an open one. Both properties are asserted, because they fail
/// independently:
///
///   * a `SyntheticStub` here is a stub outside `BASELINE_SYNTHETIC_STUBS`,
///     i.e. the same floor-by-N defect W7-30 §6 records for the inline
///     `RunnerClassLoader.close` registration;
///   * an UNSCOPED registration is worse than a wrong tag, because it is the
///     absence of one: `native-builtins/tests/stub_ratchet.rs`'s
///     `no_registration_runs_on_the_ambient_default` asserts that count is zero
///     across the whole boot, and it cannot see these eighteen rows.
///
/// Note what this does not claim. `Bridge` is asserted as the OUTCOME of the
/// scope `vm_init` applies, not as an adjudication that each of these eighteen
/// natives is a §1.5 bridge. That question belongs to
/// `scripts/baselines/jdk-only-kind-map-25-linux.tsv`, which adjudicates
/// registrations against a real JDK image; a `cargo test` has none.
#[test]
fn the_vm_crate_registrars_add_no_synthetic_stub() {
    let mut registry = NativeMethodRegistry::new();
    instrument_registrars(&mut registry);

    let rows = registry.census();
    let stubs: Vec<String> = rows
        .iter()
        .filter(|r| r.kind == NativeKind::SyntheticStub)
        .map(|r| format!("{}.{}{}", r.class, r.name, r.descriptor))
        .collect();
    let unscoped: Vec<String> = rows
        .iter()
        .filter(|r| !r.kind_chosen)
        .map(|r| format!("{}.{}{}", r.class, r.name, r.descriptor))
        .collect();

    println!(
        "vm-crate registrars: {} registrations, {} SyntheticStub, {} unscoped",
        rows.len(),
        stubs.len(),
        unscoped.len()
    );

    assert!(
        !rows.is_empty(),
        "the two `crate::runtime::instrument::*` registrars registered NOTHING. \
         Either they were emptied or this replay stopped calling them — and a \
         zero-row registry makes both assertions below pass vacuously, which is \
         the exact species (a gate that reports green while measuring nothing) \
         that W7-30 is about."
    );

    assert!(
        stubs.is_empty(),
        "{} registration(s) made by `register_instrumentation_natives` / \
         `register_self_attach_natives` are `SyntheticStub`:\n  {}\n\
         Those registrars live in the `vm` crate, so `native-builtins`' \
         stub-ratchet census cannot replay them and its zero-slack \
         `BASELINE_SYNTHETIC_STUBS` does not count these rows. A stub here is a \
         stub outside the ratchet — the same floor-by-N defect W7-30 §6 records \
         for `RunnerClassLoader.close`. Either tag it `Bridge` at the \
         registration, or move the gate to a total census.",
        stubs.len(),
        stubs.join("\n  ")
    );

    assert!(
        unscoped.is_empty(),
        "{} registration(s) here were made with NO category scope in effect:\n  {}\n\
         `effective_category` is `current_category.unwrap_or(SyntheticStub)`, so \
         an unscoped registration is not a tag, it is the absence of one — and \
         under `--jdk-only` that is the difference between a native being \
         registered and being refused at the door. `vm_init` wraps both \
         registrars in `set_category(NativeKind::Bridge)`; if that scope is \
         still present (the sibling witness in this file checks), then a \
         registration here escaped it, most likely by nesting a registrar that \
         sets and restores its own category around the wrong span.",
        unscoped.len(),
        unscoped.join("\n  ")
    );
}

/// SOURCE WITNESS — `vm_init` must still wrap those two calls in
/// `set_category(NativeKind::Bridge)`.
///
/// [`the_vm_crate_registrars_add_no_synthetic_stub`] is only a statement about
/// the shipping registry while this holds. Delete the scope in `vm_init` and
/// that test goes on passing — it replays the scope itself — while the shipping
/// boot silently registers eighteen unscoped `SyntheticStub` rows and
/// `--jdk-only` refuses the whole `java.lang.instrument` surface. A replay is
/// evidence about the source only when something checks it against the source.
///
/// Reads the working tree, not a frozen copy, and skips rather than fails if
/// `vm_init.rs` is not on disk.
#[test]
fn the_instrument_registrars_run_under_a_bridge_scope() {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("src")
        .join("vm")
        .join("vm_init.rs");
    let Ok(src) = std::fs::read_to_string(&path) else {
        println!("vm_init.rs not on disk at {path:?}; witness skipped");
        return;
    };
    let lines: Vec<&str> = src.lines().collect();

    // Isolate the `#[cfg(not(feature = "synthetic-jdk"))]` arm — the DEFAULT
    // `cratonvm-cli` build. `starts_with` on the TRIMMED line, never
    // `contains`: the first line of this file containing that attribute text is
    // a COMMENT quoting it (the W7-50 tombstone), 44 lines above the attribute
    // and inside the SIBLING synthetic arm. Both witnesses in
    // `native-builtins/tests/` used `contains` and were blind for it until
    // 2026-08-12 — they scanned 39 lines of the wrong arm and passed.
    let arm_start = lines
        .iter()
        .position(|l| {
            l.trim_start()
                .starts_with("#[cfg(not(feature = \"synthetic-jdk\"))]")
        })
        .expect("vm_init.rs must still have a real-JDK-only arm");

    let call_at = |needle: &str| -> Option<usize> {
        lines
            .iter()
            .enumerate()
            .skip(arm_start)
            .find(|(_, l)| {
                let t = l.trim_start();
                !t.starts_with("//") && t.contains(needle)
            })
            .map(|(i, _)| i)
    };

    let instrumentation = call_at("register_instrumentation_natives(")
        .expect("vm_init's real-JDK arm must still call register_instrumentation_natives");
    let self_attach = call_at("register_self_attach_natives(")
        .expect("vm_init's real-JDK arm must still call register_self_attach_natives");
    let first_call = instrumentation.min(self_attach);
    let last_call = instrumentation.max(self_attach);

    // The nearest `set_category` at or above the first call must name `Bridge`,
    // and nothing may change the category between the two calls.
    let opening = (arm_start..first_call)
        .rev()
        .find(|i| lines[*i].contains("set_category("));
    let opening = opening.expect(
        "no `set_category(...)` precedes vm_init's `register_instrumentation_natives` \
         call inside the real-JDK arm",
    );

    println!(
        "vm-crate scope witness: set_category at line {}, \
         register_instrumentation_natives at {}, register_self_attach_natives at {}",
        opening + 1,
        instrumentation + 1,
        self_attach + 1
    );

    assert!(
        lines[opening].contains("NativeKind::Bridge"),
        "vm_init's `register_instrumentation_natives` / \
         `register_self_attach_natives` calls are covered by the category scope \
         opened at line {} — `{}` — which does not name `NativeKind::Bridge`. \
         Those eighteen registrations state no kind of their own, so the scope \
         IS their kind. With no scope at all they take \
         `effective_category`'s conservative `SyntheticStub` default, and \
         `--jdk-only` then refuses the whole `java.lang.instrument` and \
         self-attach surface before any javaagent premain runs. Neither this \
         crate's gates nor `native-builtins`' can count those rows, so nothing \
         else would say so.",
        opening + 1,
        lines[opening].trim()
    );

    let interference: Vec<usize> = ((first_call + 1)..last_call)
        .filter(|i| {
            let t = lines[*i].trim_start();
            !t.starts_with("//") && t.contains("set_category(")
        })
        .map(|i| i + 1)
        .collect();
    assert!(
        interference.is_empty(),
        "a `set_category(...)` at line(s) {:?} sits BETWEEN vm_init's two \
         instrument-registrar calls, so the two no longer share one scope and \
         the second is registered under a kind this witness has not checked. \
         Re-derive the scope, and re-check \
         `the_vm_crate_registrars_add_no_synthetic_stub`'s replay against it.",
        interference
    );
}
