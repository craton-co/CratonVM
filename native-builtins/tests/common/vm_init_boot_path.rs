// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! THE ONE MODEL OF `vm_init`'s REAL-JDK BOOT PATH, shared by every gate that
//! censuses it.
//!
//! Two integration-test binaries — `stub_ratchet.rs` and
//! `duplicate_registration_gate.rs` — each carried a complete copy of this
//! model: the registrar sequence, the replay, and a source witness over
//! `vm/src/vm/vm_init.rs`. The duplication was deliberate rather than
//! overlooked (two test binaries cannot share a module without a file like this
//! one), and its recorded cost was *redundant maintenance* rather than silent
//! disagreement, because two witnesses read the same source file. This is the
//! collapse prescribed by
//! docs/known-issues/jdk-only/W7-30-stub-ratchet-boot-path-scope.md §7: one
//! model, one witness, included by both targets with
//! `#[path = "common/vm_init_boot_path.rs"] mod boot_path;`.
//!
//! The witness below is compiled into BOTH binaries and therefore runs twice
//! per `cargo test -p cratonvm-native-builtins`. That is the intended cost of
//! sharing a module between integration-test targets, and it is cheap: it reads
//! one file and scans it once.
//!
//! # The witness was BLIND until 2026-08-12, and this is the fix
//!
//! Both copies located `vm_init`'s real-JDK arm with
//!
//! ```text
//! lines.iter().position(|l| l.contains("cfg(not(feature = \"synthetic-jdk\"))"))
//! ```
//!
//! The FIRST line of `vm_init.rs` matching that substring is a **comment**
//! inside the `#[cfg(feature = "synthetic-jdk")]` arm's own real-JDK `else`
//! branch — the W7-50 tombstone, which quotes the attribute in prose:
//! *"'The branch below' was read as the `#[cfg(not(feature = \"synthetic-jdk\"))]`
//! block, but the relevant fork is `if config.use_synthetic_jdk`"*. The real
//! attribute is 44 lines further down.
//!
//! So the brace scan isolated a 39-line window in the wrong arm, `observed`
//! held **8** registrars instead of **48**, every one of the 8 happened to be in
//! [`VM_INIT_SEQUENCE`] (they are the `jmx::*` calls, which both arms make in
//! the same relative order), `unmodelled` was **0**, and the order check ran
//! over 8 names it could not fail on. The witness printed a clean line and
//! passed — while asserting nothing about the arm it names.
//!
//! That is the third instance of this file's own species: the predicate was
//! fine and the POPULATION was the defect (W7-30 §9). It is also the second
//! time the *locator* was the thing that decayed rather than the model — with
//! the locator fixed, all 46 modelled names are observed, in order, and the two
//! unmodelled ones are exactly the pair
//! [`UNMODELLED_VM_CRATE_REGISTRARS`] allows. **The model was never stale; only
//! the instrument was.**
//!
//! The fix is to require the match on a line whose trimmed form STARTS with the
//! attribute, so a quotation of it in prose cannot be mistaken for it. A
//! comment can contain an attribute; a comment cannot start with one.
//!
//! # What this witness still does not assert
//!
//! Stated rather than left for a reader to assume, because two of the three
//! were found while fixing the locator:
//!
//! 1. ~~**That the replay CALLS every name in [`VM_INIT_SEQUENCE`].**~~ ASSERTED
//!    since 2026-09-11 by [`the_replay_calls_every_name_in_the_sequence`], which
//!    reads THIS file off disk exactly as [`vm_init_source`] reads `vm_init.rs`
//!    -- "Rust has no reflection over a function body" was true and beside the
//!    point. It passed as written, 46 of 46 in order, so the review had held;
//!    the assertion is here so the next merge does not need it to hold again.
//!    Two sibling lists were broken by clean `git merge`s the same week.
//! 2. **Registration entry points whose name does not begin with `register_`.**
//!    The scan takes bare `register_*` calls at statement start, so
//!    `vm_init`'s call to `init_service_loader_bootstrap` (`vm_init.rs`, a `pub
//!    fn` that wraps
//!    `cratonvm_native_builtins::service_loader::register_service_loader_natives`)
//!    is invisible to it and is NOT in the list. This one is benign for the
//!    KIND — that registrar states `SyntheticStub` explicitly, and its other
//!    caller `jdbc::register_jdbc_driver_natives` is on the replayed path — but
//!    it is a real hole in the *population*, and settling whether the replay
//!    holds those 11 triples at the same POSITION needs a registry dump, not a
//!    grep. Do not add it to the replay from arithmetic: that changes a
//!    slack-free count nobody has re-taken.
//! 3. **The individual `native_methods.register(...)` calls `vm_init` makes
//!    inline between the passes.** Eight of them in the arm, of which exactly
//!    one states `SyntheticStub`
//!    (`io/quarkus/bootstrap/runner/RunnerClassLoader.close()V`). That one is
//!    the reason `stub_ratchet.rs`'s baseline is a FLOOR BY ONE.
//!    [`the_inline_registrations_in_vm_init_are_enumerated`] now ratchets it
//!    instead of leaving it as prose. Closing it properly needs the gate to
//!    move to `vm/tests/`, which needs a registration-only helper extracted
//!    from `SharedVm::new` — see `vm/tests/stub_ratchet.rs`.

#![allow(dead_code)]

use cratonvm_native_api::NativeMethodRegistry;

/// The registrar sequence of `vm_init.rs`'s `#[cfg(not(feature =
/// "synthetic-jdk"))]` arm, in its order, by bare function name.
///
/// Read by [`the_replayed_sequence_matches_vm_init`] and replayed by
/// [`vm_init_real_jdk_boot_path`]. The two must not drift: ORDER is the entire
/// content of a "which registration wins" answer, because `register()` is
/// last-write-wins.
///
/// **Deliberately NOT `cfg`-gated, unlike the replay.** The witness reads
/// `vm_init.rs` as TEXT, and the ten `jmx::*` calls are textually present in
/// that file whether or not `management` is enabled here. Gating this list on
/// the feature would make the witness report ten unmodelled registrars in the
/// default resolve and blow past [`UNMODELLED_VM_CRATE_REGISTRARS`]. The list
/// describes the source; the replay describes this build.
pub const VM_INIT_SEQUENCE: &[&str] = &[
    "register_essential_natives_with_shims",
    "register_concurrent_natives",
    "register_forkjoin_quiescence",
    "register_stamped_lock_natives",
    "register_p61_file_handler",
    "register_url_classloader_close_bridge",
    "register_io_natives",
    "register_p60_process_handle",
    "register_classvalue_natives",
    // ╔══ LAST-WRITE-WINS BOUNDARY — do not reorder ═══════════════════════╗
    // `vm_init` carries this banner verbatim. The two calls that follow
    // `register_collections_natives` exist BECAUSE it overwrites them.
    "register_collections_natives",
    "register_random_and_securerandom_natives",
    "register_properties_sidetable",
    // ╚═══════════════════════════════════════════════════════════════════╝
    "register_t12_unsafe_natives",
    "register_t14_system_bootstrap",
    "register_boot_loader_natives",
    "register_phase57_nio_file",
    "register_phase57_file",
    "register_p59_jar",
    "register_p59_bulk_stream_transfer",
    "register_p59_zip_output_primitives",
    "register_spring_boot_logback_apply",
    "register_url_codec",
    "register_charset_natives_pub",
    "register_p58_charset_coder",
    "register_real_charset_natives",
    "register_deprecated_internal_natives",
    "register_arrays_support_natives",
    "register_string_latin1_natives",
    "register_classloader_real_natives",
    "register_phase54_method_handle",
    "register_p63_method_handles_lookup",
    "register_t4_method_handle_invoke",
    "register_t28_method_handle_completeness",
    "register_p68_invoke_extras",
    "register_reflect_proxy_natives",
    "register_vm_management_impl",
    "register_jmx_natives",
    "register_thread_impl",
    "register_class_loading_impl",
    "register_garbage_collector_impl",
    "register_memory_pool_impl",
    "register_memory_manager_impl",
    "register_operating_system_impl",
    "register_hotspot_diagnostic",
    "register_flag_impl",
    "register_slf4j_binder_stubs_pub",
];

/// Registrars `vm_init`'s real-JDK arm calls that this file CANNOT replay,
/// because they live in the `vm` crate and `native-builtins` must not
/// dev-depend on it (that would be a dependency cycle).
///
/// Both are `crate::runtime::instrument::*`
/// (`register_instrumentation_natives`, `register_self_attach_natives`).
/// Anything else appearing in `vm_init` and not in [`VM_INIT_SEQUENCE`] means
/// this model has gone stale — hence a ratcheted `<=` rather than a comment
/// nobody re-reads.
///
/// **What that omission costs the stub count is now MEASURED rather than
/// assumed: nothing.** `vm/tests/stub_ratchet.rs` replays exactly those two
/// under the `set_category(NativeKind::Bridge)` scope `vm_init` wraps them in
/// and asserts they contribute zero `SyntheticStub` and zero unscoped
/// registrations. Before that test existed, "two registrars are outside the
/// census" was an unbounded admission.
pub const UNMODELLED_VM_CRATE_REGISTRARS: usize = 2;

/// Inline `native_methods.register*` calls `vm_init`'s real-JDK arm makes
/// between the registrar passes, which no replay in this crate can reach.
///
/// Printed, not asserted: a new inline `Bridge` registration is ordinary work
/// and should not redden a gate. The number that matters is
/// [`INLINE_SYNTHETIC_STUBS_IN_VM_INIT`].
pub const INLINE_REGISTRATIONS_IN_VM_INIT: usize = 8;

/// Inline registrations in that arm that state — or scope — `SyntheticStub`.
///
/// **One**, and it is the reason `stub_ratchet.rs`'s baseline is a floor rather
/// than a count: `vm_init` registers
/// `io/quarkus/bootstrap/runner/RunnerClassLoader.close()V` with an explicit
/// `NativeKind::SyntheticStub` inside the real-JDK arm. `register_boot_path`'s
/// comment used to predict this case in the future tense ("a `SyntheticStub`
/// added there WOULD slip past"); the conditional was already false when it was
/// written, and W7-30 §6 recorded that as prose. This is the same fact as an
/// assertion, so a SECOND one cannot arrive unnoticed.
pub const INLINE_SYNTHETIC_STUBS_IN_VM_INIT: usize = 1;

/// Build the registry the way `vm_init.rs`'s real-JDK arm does.
///
/// `set_drop_real_layout_synthetic(true)` is first and load-bearing, exactly as
/// in `vm_init`: it drops the synthetic `java/util/StringJoiner` natives whose
/// fake 5-field layout corrupts the real 7-field object. Setting it after a
/// pass would leave them in — and, for the duplicate census specifically, would
/// invent duplicate rows for registrations the shipping VM never accepts.
///
/// `ShimSelection::ALL` where the VM computes a selection from a classpath
/// probe: the widest set, which is the right direction for an upper-bound gate
/// and is what `register_essential_natives` itself passes.
///
/// # This replay is the fix for a blind spot TWICE over
///
/// It began, in `stub_ratchet.rs`, as `register_essential_natives` and nothing
/// else, under a doc comment claiming it built "the default native registry
/// exactly as the VM's real-JDK boot path does". That was false and the gap was
/// large. The 2026-08-05 fix named six registrars and stopped; `vm_init` calls
/// 48, so forty registrars' worth of registrations sat outside a zero-slack
/// assertion. Both gaps were found by DISAGREEMENT with another gate, never by
/// reading this function — which is why the scope is now witnessed.
pub fn vm_init_real_jdk_boot_path(r: &mut NativeMethodRegistry) {
    use cratonvm_native_builtins as nb;

    r.set_drop_real_layout_synthetic(true);

    nb::register_essential_natives_with_shims(r, nb::app_shims::ShimSelection::ALL);
    nb::register_concurrent_natives(r);
    // MUST follow `register_concurrent_natives` — same last-write-wins ordering
    // constraint `vm_init` documents at its own call site.
    nb::register_forkjoin_quiescence(r);
    nb::register_stamped_lock_natives(r);
    nb::phases_late::register_p61_file_handler(r);
    nb::servlet::register_url_classloader_close_bridge(r);

    cratonvm_native_io::register_io_natives(r);
    nb::phases_late::register_p60_process_handle(r);
    nb::phases_late::register_classvalue_natives(r);

    // ╔══ LAST-WRITE-WINS BOUNDARY — do not reorder ═══════════════════════╗
    // Verbatim from `vm_init`. The two calls after this one exist *because*
    // `register_collections_natives` overwrites earlier, correct
    // implementations: `securerandom` (collections re-registers every
    // `java/util/Random` method against a synthetic 2-field layout, so a seeded
    // `Random` returned all zeroes) and `properties_sidetable` (collections
    // re-registers `Properties` against the legacy HashMap layout, breaking
    // Surefire's load -> stringPropertyNames -> getProperty round-trip).
    //
    // This boundary is why stopping at `register_collections_natives` was not
    // merely a narrow census but a WRONG one: the two overwritten families
    // would be counted at `native-collections`' kind rather than at the kind
    // the shipping VM dispatches.
    cratonvm_native_collections::register_collections_natives(r);
    nb::securerandom::register_random_and_securerandom_natives(r);
    nb::properties_sidetable::register_properties_sidetable(r);
    // ╚═══════════════════════════════════════════════════════════════════╝

    nb::unsafe_jdk25::register_t12_unsafe_natives(r);
    nb::system_bootstrap::register_t14_system_bootstrap(r);
    nb::boot_loader::register_boot_loader_natives(r);
    nb::phases_late::register_phase57_nio_file(r);
    nb::phases_late::register_phase57_file(r);
    nb::phases_late::register_p59_jar(r);
    nb::phases_late::register_p59_bulk_stream_transfer(r);
    nb::phases_late::register_p59_zip_output_primitives(r);
    nb::register_spring_boot_logback_apply(r);
    nb::deprecated_io_util::register_url_codec(r);
    nb::register_charset_natives_pub(r);
    nb::phases_late::register_p58_charset_coder(r);
    nb::charset::register_real_charset_natives(r);
    nb::deprecated_internal::register_deprecated_internal_natives(r);
    nb::phases_early::register_arrays_support_natives(r);
    nb::phases_early::register_string_latin1_natives(r);
    nb::classloader_real::register_classloader_real_natives(r);
    nb::lang_invoke::register_phase54_method_handle(r);
    nb::lang_invoke::register_p63_method_handles_lookup(r);
    nb::lang_invoke::register_t4_method_handle_invoke(r);
    nb::lang_invoke::register_t28_method_handle_completeness(r);
    nb::lang_invoke::register_p68_invoke_extras(r);
    nb::register_reflect_proxy_natives(r);
    // `crate::runtime::instrument::register_instrumentation_natives` and
    // `register_self_attach_natives` sit here in `vm_init`, wrapped in an
    // explicit `set_category(NativeKind::Bridge)`. They live in the `vm` crate;
    // see `UNMODELLED_VM_CRATE_REGISTRARS` and `vm/tests/stub_ratchet.rs`.

    // ╔══ `#[cfg(feature = "management")]` — COPIED FROM `vm_init`, not added ═╗
    // Every one of the ten calls below carries this exact `cfg` at its
    // `vm_init` call site, and `nb::jmx` is itself
    // `#[cfg(feature = "management")]`. Replaying them unconditionally would do
    // two wrong things at once: model a registry no build produces, and make
    // both test targets uncompilable in this crate's own default feature set
    // (ten `E0433`s).
    //
    // The consequence is stated rather than hidden: these censuses are
    // FEATURE-dependent, which is a property of `vm_init` and not of the
    // replay, so every baseline over them is keyed per configuration.
    #[cfg(feature = "management")]
    {
        nb::jmx::register_vm_management_impl(r);
        nb::jmx::register_jmx_natives(r);
        nb::jmx::register_thread_impl(r);
        nb::jmx::register_class_loading_impl(r);
        nb::jmx::register_garbage_collector_impl(r);
        nb::jmx::register_memory_pool_impl(r);
        nb::jmx::register_memory_manager_impl(r);
        nb::jmx::register_operating_system_impl(r);
        nb::jmx::register_hotspot_diagnostic(r);
        nb::jmx::register_flag_impl(r);
    }
    // ╚═══════════════════════════════════════════════════════════════════════╝

    nb::register_slf4j_binder_stubs_pub(r);
}

/// `vm/src/vm/vm_init.rs` from the working tree, or `None` when it is not on
/// disk (a packaged crate has no sibling `vm/`).
///
/// Reads the working tree rather than a frozen copy, so it measures the source
/// as it is now — the property that makes this a witness rather than a second
/// hand-maintained list. Source-witness tests that read a frozen copy have gone
/// stale in this campaign; ones that read a FIXED LINE BAND have gone stale
/// twice.
fn vm_init_source() -> Option<String> {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("vm")
        .join("src")
        .join("vm")
        .join("vm_init.rs");
    match std::fs::read_to_string(&path) {
        Ok(src) => Some(src),
        Err(_) => {
            println!("vm_init.rs not on disk at {path:?}; witness skipped");
            None
        }
    }
}

/// The `[start, end)` line range of `vm_init`'s `#[cfg(not(feature =
/// "synthetic-jdk"))]` arm — the DEFAULT `cratonvm-cli` boot.
///
/// # The locator, and why it is `starts_with` and not `contains`
///
/// `contains` matched a COMMENT 44 lines above the attribute and isolated a
/// 39-line window in the sibling `#[cfg(feature = "synthetic-jdk")]` arm. The
/// module header has the full account. Requiring the trimmed line to START with
/// the attribute is what distinguishes the attribute from a quotation of it:
/// a comment can contain an attribute, but a comment cannot start with one.
///
/// The sibling arm must stay excluded for a second reason as well: the phase
/// registrars there run only when `config.use_synthetic_jdk` is true AT
/// RUNTIME, and that arm has its own real-JDK `else` branch. Scanning the whole
/// file would splice the two into one imaginary sequence — and would, for
/// instance, count the SECOND `register_p60_process_handle` call site as a
/// third registrar.
fn real_jdk_arm_range(lines: &[&str]) -> (usize, usize) {
    let start = lines
        .iter()
        .position(|l| {
            l.trim_start()
                .starts_with("#[cfg(not(feature = \"synthetic-jdk\"))]")
        })
        .expect(
            "vm_init.rs must still have a real-JDK-only arm whose \
             `#[cfg(not(feature = \"synthetic-jdk\"))]` attribute begins a line",
        );
    let mut depth: i32 = 0;
    let mut opened = false;
    let mut end = lines.len();
    for (i, line) in lines.iter().enumerate().skip(start + 1) {
        depth += line.matches('{').count() as i32;
        if !opened && depth > 0 {
            opened = true;
        }
        depth -= line.matches('}').count() as i32;
        if opened && depth <= 0 {
            end = i;
            break;
        }
    }
    (start, end)
}

/// Bare `register_*` calls at the start of a statement, in source order.
///
/// Deliberately not a general call matcher: nested
/// `native_methods.register(...)` / `register_with_kind(...)` calls inside the
/// arm are REGISTRATIONS, not registrars, and are accounted for by
/// [`the_inline_registrations_in_vm_init_are_enumerated`] instead.
fn observed_registrars(lines: &[&str]) -> Vec<String> {
    let mut observed: Vec<String> = Vec::new();
    for line in lines {
        let trimmed = line.trim_start();
        if trimmed.starts_with("//") {
            continue;
        }
        let Some(open) = trimmed.find('(') else {
            continue;
        };
        let head = &trimmed[..open];
        if !head
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == ':')
        {
            continue;
        }
        let name = head.rsplit("::").next().unwrap_or(head);
        if name.starts_with("register_") {
            observed.push(name.to_string());
        }
    }
    observed
}

/// SOURCE WITNESS — the modelled scope must still be `vm_init`'s boot path.
///
/// **This assertion is worth more than the numbers it protects.** Every gate
/// built on [`vm_init_real_jdk_boot_path`] is a count over a population, and
/// until 2026-08-11 the population was a hand-maintained list of six registrar
/// calls with nothing checking it against the thing it claimed to model. A
/// registration outside that list could be added, deleted or retagged
/// `Bridge` <-> `SyntheticStub` with no effect on any number — which is
/// precisely what a ratchet exists to prevent, and precisely what happened: a
/// `ProcessHandle` retag moved 18 registrations and the stub gate reported no
/// change at all.
///
/// A count baseline cannot detect that, by construction. Only the scope can,
/// and a scope is only checkable against a source of truth. So this test reads
/// `vm_init.rs` and asserts two separate things, because they need different
/// fixes:
///
///   * an **unmodelled registrar** — `vm_init` calls it, [`VM_INIT_SEQUENCE`]
///     does not name it — ratchets against [`UNMODELLED_VM_CRATE_REGISTRARS`].
///     Every registration such a registrar makes is invisible to every census
///     built on the replay.
///   * an **order inversion** — two modelled registrars run by `vm_init` in the
///     opposite relative order — is a hard failure with no baseline.
///     Registration is last-write-wins, so an inversion means the census counts
///     the kind of the row the shipping VM *discards*.
///
/// It also asserts the direction nobody had checked: that every name in
/// [`VM_INIT_SEQUENCE`] is actually OBSERVED. Without that, the locator defect
/// this module header describes reads as a pass — 8 observed names, all
/// modelled, zero unmodelled, and 38 modelled names that appear nowhere in the
/// window being scanned.
#[test]
fn the_replayed_sequence_matches_vm_init() {
    let Some(src) = vm_init_source() else {
        return;
    };
    let lines: Vec<&str> = src.lines().collect();
    let (start, end) = real_jdk_arm_range(&lines);
    let observed = observed_registrars(&lines[start..end]);

    let unmodelled: Vec<&String> = observed
        .iter()
        .filter(|n| !VM_INIT_SEQUENCE.contains(&n.as_str()))
        .collect();
    println!(
        "boot-path(scope): vm_init real-JDK arm (lines {}..{}) calls {} registrars, \
         {} modelled here, {} unmodelled",
        start + 1,
        end + 1,
        observed.len(),
        observed.len() - unmodelled.len(),
        unmodelled.len()
    );
    for n in &unmodelled {
        println!("  UNMODELLED: {n}");
    }

    let modelled: Vec<&String> = observed
        .iter()
        .filter(|n| VM_INIT_SEQUENCE.contains(&n.as_str()))
        .collect();
    let mut expected = VM_INIT_SEQUENCE.iter().peekable();
    for name in &modelled {
        loop {
            match expected.peek() {
                Some(e) if **e == name.as_str() => {
                    expected.next();
                    break;
                }
                Some(_) => {
                    expected.next();
                }
                None => panic!(
                    "vm_init runs `{name}` in an order VM_INIT_SEQUENCE does not \
                     allow. Registration is last-write-wins, so an inversion makes \
                     every census over this replay count the KIND OF THE ROW THE VM \
                     DISCARDS — the `register_collections_natives` / `securerandom` \
                     / `properties_sidetable` boundary is one such pair and it is \
                     load-bearing. Re-derive VM_INIT_SEQUENCE and \
                     `vm_init_real_jdk_boot_path` from vm_init.rs."
                ),
            }
        }
    }

    let never_observed: Vec<&str> = VM_INIT_SEQUENCE
        .iter()
        .copied()
        .filter(|n| !observed.iter().any(|o| o.as_str() == *n))
        .collect();
    assert!(
        never_observed.is_empty(),
        "{} name(s) in VM_INIT_SEQUENCE do not appear in the arm this witness \
         scanned: {:?}. Either `vm_init` dropped a registrar the replay still \
         calls — in which case the census models a registry no build produces — \
         or the ARM LOCATOR is picking the wrong block, which is exactly the \
         defect fixed on 2026-08-12 (a `contains` match on a comment quoting the \
         attribute isolated 39 lines of the sibling synthetic arm, so this \
         witness observed 8 registrars instead of 48 and passed).",
        never_observed.len(),
        never_observed
    );

    assert!(
        unmodelled.len() <= UNMODELLED_VM_CRATE_REGISTRARS,
        "vm_init's real-JDK arm calls {} registrars this model does not replay \
         (allowed: {}, the two `crate::runtime::instrument::*` ones that live in \
         the `vm` crate). EVERY registration an unmodelled registrar makes is \
         outside every baseline taken over this replay, so it can be added, \
         deleted or retagged `Bridge` <-> `SyntheticStub` and no gate will move \
         by a single row — a zero-slack assertion over a population it does not \
         enumerate. That is the exact defect this witness was added to close \
         (18 registrations, `register_p60_process_handle` and \
         `register_classvalue_natives`). Add the registrar to VM_INIT_SEQUENCE \
         and to `vm_init_real_jdk_boot_path`, in position, and re-freeze the \
         baselines from a real run.",
        unmodelled.len(),
        UNMODELLED_VM_CRATE_REGISTRARS
    );
}

/// SOURCE WITNESS — the inline registrations `vm_init` makes between the passes
/// are ENUMERATED, and at most one of them is a synthetic stub.
///
/// `register_boot_path`'s comment used to say of them: *"They are `Bridge`, and
/// a `SyntheticStub` added there would slip past."* **The conditional was
/// already false when it was written** — `vm_init` registers
/// `io/quarkus/bootstrap/runner/RunnerClassLoader.close()V` with an explicit
/// `NativeKind::SyntheticStub` inside the real-JDK arm — and W7-30 §6 recorded
/// that by changing the comment to the present tense and calling the stub
/// baseline "a floor by one".
///
/// A floor by one that nothing checks becomes a floor by two. This is the same
/// fact as a ratchet: the predicate is textual (`NativeKind::SyntheticStub`
/// appearing anywhere in the arm, which also catches an inline
/// `set_category`), so it fires on a second one whether it is stated at the
/// call site or scoped around it.
///
/// The TOTAL inline count is printed and not asserted: a new inline `Bridge`
/// registration is ordinary work, and a gate that fires on it is friction.
/// Only the stub direction is load-bearing.
#[test]
fn the_inline_registrations_in_vm_init_are_enumerated() {
    let Some(src) = vm_init_source() else {
        return;
    };
    let lines: Vec<&str> = src.lines().collect();
    let (start, end) = real_jdk_arm_range(&lines);
    let arm = &lines[start..end];

    let inline: Vec<usize> = arm
        .iter()
        .enumerate()
        .filter(|(_, l)| {
            let t = l.trim_start();
            !t.starts_with("//") && t.contains("native_methods.register")
        })
        .map(|(i, _)| start + i + 1)
        .collect();
    let stub_stated: Vec<usize> = arm
        .iter()
        .enumerate()
        .filter(|(_, l)| {
            let t = l.trim_start();
            !t.starts_with("//") && t.contains("NativeKind::SyntheticStub")
        })
        .map(|(i, _)| start + i + 1)
        .collect();

    println!(
        "boot-path(inline): {} inline native_methods.register* call(s) in \
         vm_init's real-JDK arm at lines {:?} (modelled: {}); {} state \
         SyntheticStub at lines {:?} (allowed: {})",
        inline.len(),
        inline,
        INLINE_REGISTRATIONS_IN_VM_INIT,
        stub_stated.len(),
        stub_stated,
        INLINE_SYNTHETIC_STUBS_IN_VM_INIT
    );

    assert!(
        stub_stated.len() <= INLINE_SYNTHETIC_STUBS_IN_VM_INIT,
        "{} inline registration(s) in vm_init's real-JDK arm state or scope \
         `NativeKind::SyntheticStub` (lines {:?}), above the {} this model \
         accounts for. Every one of them is a synthetic stub OUTSIDE the \
         stub-ratchet census, because `native-builtins` cannot dev-depend on \
         `vm`: the baseline is a floor by exactly this number. Either move the \
         registration into a registrar the replay calls, or move the gate to \
         `vm/tests/` (which needs a registration-only helper extracted from \
         `SharedVm::new`) — do not raise this constant to make the message go \
         away. See docs/known-issues/jdk-only/W7-30-stub-ratchet-boot-path-scope.md §6.",
        stub_stated.len(),
        stub_stated,
        INLINE_SYNTHETIC_STUBS_IN_VM_INIT
    );
}

/// This file, off disk — the same trick [`vm_init_source`] plays on `vm_init.rs`.
///
/// Reading the working tree rather than a frozen copy is what makes the test
/// below a witness instead of a third hand-maintained list.
fn boot_path_file_source() -> Option<String> {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("common")
        .join("vm_init_boot_path.rs");
    match std::fs::read_to_string(&path) {
        Ok(src) => Some(src),
        Err(_) => {
            println!("vm_init_boot_path.rs not on disk at {path:?}; witness skipped");
            None
        }
    }
}

/// The text of [`vm_init_real_jdk_boot_path`]'s body, comment lines removed.
///
/// Comments are stripped because this file MENTIONS registrar names in prose on
/// purpose, and the 2026-08-12 locator defect (module header) was a comment
/// being read as source. A call is a call only if it is code.
fn replay_body_code(src: &str) -> Option<String> {
    let start = src.find("pub fn vm_init_real_jdk_boot_path(")?;
    let rest = &src[start..];
    let end = rest.find("\n}\n")?;
    Some(
        rest[..end]
            .lines()
            .filter(|l| !l.trim_start().starts_with("//"))
            .collect::<Vec<_>>()
            .join("\n"),
    )
}

/// Find `name(` where `name` starts at an identifier boundary.
fn call_position(code: &str, name: &str) -> Option<usize> {
    let needle = format!("{name}(");
    let mut from = 0usize;
    while let Some(rel) = code[from..].find(&needle) {
        let at = from + rel;
        let ok = at == 0
            || !code[..at]
                .chars()
                .next_back()
                .is_some_and(|c| c.is_alphanumeric() || c == '_');
        if ok {
            return Some(at);
        }
        from = at + needle.len();
    }
    None
}

/// SOURCE WITNESS — the replay must CALL every name in [`VM_INIT_SEQUENCE`], in
/// that order.
///
/// The module header lists this first under "What this witness still does not
/// assert": the list is checked against `vm_init.rs`, and *"the replay is
/// checked against the list by review."* Review is not a gate, and what it
/// costs when it is wrong is not a wrong number but a silent one — a registrar
/// named in the sequence and missing from the replay leaves whatever registered
/// the triple EARLIER in place, so every census over
/// [`vm_init_real_jdk_boot_path`] reports the kind of a row the shipping VM
/// overwrites. That is §1 of `W7-30-stub-ratchet-boot-path-scope.md` exactly,
/// and that page records the species being found twice by a disagreeing second
/// measurement and never by the gate.
///
/// "Rust has no reflection over a function body" is true and beside the point.
/// [`the_replayed_sequence_matches_vm_init`] already reads `vm_init.rs` off
/// disk; this file is on disk in the same way.
///
/// ORDER is asserted for the reason the sibling test gives: registration is
/// last-write-wins, so a replay that makes the right calls in the wrong order
/// is a replay of a different VM. Positions are first occurrences, which is
/// exact here because no name is called twice; a future second call would need
/// this to compare spans instead.
///
/// Feature-independent by construction: it reads source text, so the calls
/// inside the `#[cfg(feature = "management")]` block count as present whether
/// or not that feature is on. That is correct for a witness about the MODEL —
/// whether a given build compiles them is what `MEASURED_CONFIG` is for.
#[test]
fn the_replay_calls_every_name_in_the_sequence() {
    let Some(src) = boot_path_file_source() else {
        return;
    };
    let code = replay_body_code(&src)
        .expect("`vm_init_real_jdk_boot_path`'s body is in this file and ends at a bare `}`");

    let mut missing: Vec<&str> = Vec::new();
    let mut positions: Vec<(usize, &str)> = Vec::new();
    for name in VM_INIT_SEQUENCE {
        match call_position(&code, name) {
            Some(at) => positions.push((at, *name)),
            None => missing.push(*name),
        }
    }

    assert!(
        missing.is_empty(),
        "{} name(s) in VM_INIT_SEQUENCE are never called by \
         `vm_init_real_jdk_boot_path`: {missing:?}. Every registration such a \
         registrar makes is missing from this replay's registry, or — worse, \
         because it is silent — present at the kind of whatever registered the \
         triple earlier and was meant to be overwritten. Add the call where \
         `vm_init` makes it.",
        missing.len()
    );

    let mut last: usize = 0;
    let mut last_name = "";
    for (at, name) in &positions {
        assert!(
            *at >= last,
            "the replay calls `{name}` before `{last_name}`, and VM_INIT_SEQUENCE \
             — which is itself checked against vm_init.rs — has them the other way \
             round. Registration is last-write-wins, so every census over this \
             replay would count the kind of the row the shipping VM discards."
        );
        last = *at;
        last_name = name;
    }

    println!(
        "boot-path replay: all {} VM_INIT_SEQUENCE names called, in order",
        positions.len()
    );
}
