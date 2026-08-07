// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! DUPLICATE-REGISTRATION GATE — the shadowed-native ratchet.
//!
//! `NativeMethodRegistry::register` is last-write-wins and **updates the
//! existing slot in place**. A correct, guarded native therefore loses silently
//! to an unguarded twin registered later, and every symptom points at the wrong
//! source file. Four instances each cost a full measure-fix-rebuild cycle
//! before anyone went looking for the pattern:
//!
//!   1. `MethodHandles$Lookup.defineHiddenClass` — a placeholder registered late
//!      shadowed the real implementation; the returned `Lookup`'s slot 0 was
//!      never written, so `lookupClass()` answered null. Broke `RJdkHidden`
//!      AND `RJdkStrict`.
//!   2. `Files.copy(Path,Path,CopyOption...)` — the winner never read its
//!      options argument, so a copy onto an existing file overwrote instead of
//!      throwing `FileAlreadyExistsException`.
//!   3. `Module.getResourceAsStream` — registered twice ~9k lines apart; the
//!      wave-2 `opens` gate was dead code and an unguarded classpath resolver
//!      served resources from any package.
//!   4. `SSLContext.getInstance` — four competing registrations; the live one
//!      threw a `java.io.IOException` whose message said
//!      "NoSuchAlgorithmException", so the caller's catch did not match.
//!
//! The misdirection that made all four expensive is worth stating once:
//! **forcing "the native" over real bytecode does not help when the slot holds
//! a DIFFERENT native.** Only provenance separates the two.
//!
//! # Why this is now mechanical
//!
//! Nothing here is inferred or scraped. `register` is `#[track_caller]`, so
//! every accepted registration records its `Location`, and `registrations` is
//! append-only — so the LOSER's census row survives with `owns_slot == false`.
//! `cratonvm_native_api::registry::shadowed_registrations_in` pairs each loser
//! with the winner that displaced it. The lever existed; nothing consumed it.
//!
//! # What this gate replays, and why not the shorter list
//!
//! It replays the registrar sequence of **`vm_init.rs`'s real-JDK arm** — the
//! `#[cfg(not(feature = "synthetic-jdk"))]` block, which is the default
//! `cratonvm-cli` build — not the six-call `register_boot_path` helper in
//! `stub_ratchet.rs`.
//!
//! That distinction is the whole ballgame for THIS gate, and it is not a
//! refinement. `register_boot_path` stops at `register_collections_natives`.
//! `vm_init` does not: immediately after that call sits a block commented
//! "LAST-WRITE-WINS BOUNDARY — do not reorder", which re-registers
//! `securerandom` and `properties_sidetable` **because**
//! `register_collections_natives` overwrites them with layout-wrong versions.
//! A duplicate census taken over the six-call helper therefore reports
//! `native-collections` as the winner for ~23 `java/util/Properties` triples
//! and the whole `java/util/Random` family — the exact opposite of what the
//! shipping VM does, and precisely the kind of confidently-wrong winner column
//! that made this species expensive in the first place.
//!
//! So [`vm_init_real_jdk_boot_path`] mirrors `vm_init` call-for-call, and
//! [`the_replayed_sequence_matches_vm_init`] is a source-witness test that
//! fails if `vm_init` grows a registrar this file does not replay, or reorders
//! two that it does.
//!
//! Run with:
//!
//! ```text
//! cargo test -p cratonvm-native-builtins --test duplicate_registration_gate -- --nocapture
//! ```

use cratonvm_native_api::registry::ShadowedRegistration;
use cratonvm_native_api::NativeMethodRegistry;

// ===========================================================================
// BASELINES
// ===========================================================================

/// Frozen upper bound on the number of SHADOWED registrations — registrations
/// a later `register*` of the identical triple displaced — in the real-JDK boot
/// registry.
///
/// One row per LOSER, not per triple: a triple registered four times
/// contributes three. That is deliberate, so that removing one of the four
/// competing `SSLContext.getInstance` registrations moves the number and a
/// partial fix can be scored.
///
/// # SEEDING — this constant is `0` and the gate is therefore RED until the
/// first run pastes the real number in.
///
/// That is on purpose and it is the same procedure `stub_ratchet.rs` documents
/// for its own baseline. The alternative — seeding a guess — is strictly worse
/// here: a baseline set above the true count is a gate that silently tolerates
/// every duplicate below it, which is exactly the failure mode this species
/// already has. A run prints
///
/// ```text
/// dup-registration-gate: <N> shadowed registrations (baseline <B>)
/// ```
///
/// Paste `<N>` here, and `<M>` from the sibling line into
/// [`BASELINE_KIND_DISAGREEMENTS`]. Both are one-way ratchets afterwards:
/// removing a duplicate is welcome and requires lowering the number in the
/// same change to lock the improvement in.
///
/// A **static** source scan of the boot-reachable registrars found 483 triples
/// registered from more than one call site (445 of them cross-file). Treat that
/// as an order-of-magnitude sanity check on `<N>`, not as the seed: it cannot
/// resolve class names held in variables, cannot see registrations behind
/// runtime flags, and counts triples rather than losers.
const BASELINE_SHADOWED: usize = 0;

/// Frozen upper bound on the SHADOWED registrations where the winner and the
/// loser disagree about [`NativeKind`] — the high-signal subset.
///
/// This is instance 1 exactly: a placeholder `SyntheticStub` displacing a real
/// `Bridge`. It is never cosmetic, because `NativeKind` decides three separate
/// things: `CompatibilityMode::JdkOnly` refuses a `SyntheticStub`,
/// `CRATONVM_NO_STUBS` drops one, and
/// `synthetic_stub_kind_should_yield_to_real_bytecode` arbitrates for one. A
/// bridge displaced this way stops dispatching under `--jdk-only` while its
/// stated original would have been allowed.
///
/// Seeded `0` for the same reason as [`BASELINE_SHADOWED`]. `register`'s
/// downgrade rule (an unchosen re-registration preserves a prior *chosen* kind)
/// already suppresses the benign majority, so this number should be small — if
/// the first run prints something large, that is itself the finding.
const BASELINE_KIND_DISAGREEMENTS: usize = 0;

/// Registrars `vm_init`'s real-JDK arm calls that this file CANNOT replay,
/// because they live in the `vm` crate and `native-builtins` must not
/// dev-depend on it (that is a dependency cycle).
///
/// Both are `crate::runtime::instrument::*`. Anything else appearing in
/// `vm_init` and not in [`VM_INIT_SEQUENCE`] means the model has gone stale and
/// the winner column can no longer be trusted — hence the `<=` below rather
/// than a comment.
const UNMODELLED_VM_CRATE_REGISTRARS: usize = 2;

// ===========================================================================
// THE BOOT PATH
// ===========================================================================

/// The registrar sequence of `vm_init.rs`'s `#[cfg(not(feature =
/// "synthetic-jdk"))]` arm, in its order, by bare function name.
///
/// Read by [`the_replayed_sequence_matches_vm_init`] and replayed by
/// [`vm_init_real_jdk_boot_path`]. The two must not drift, because ORDER is the
/// entire content of a "which registration wins" answer.
const VM_INIT_SEQUENCE: &[&str] = &[
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

/// Build the registry the way `vm_init.rs`'s real-JDK arm does.
///
/// `set_drop_real_layout_synthetic(true)` is first and load-bearing, exactly as
/// in `vm_init` (:2138): it drops the synthetic `java/util/StringJoiner`
/// natives whose fake 5-field layout corrupts the real 7-field object. Setting
/// it after a pass would leave them in — and, for this gate specifically, would
/// invent duplicate rows for registrations the shipping VM never accepts.
///
/// `ShimSelection::ALL` matches `stub_ratchet.rs`'s choice. `vm_init` derives
/// its selection from config; ALL is the superset, so this over-covers rather
/// than under-covers, which is the safe direction for a ratchet.
fn vm_init_real_jdk_boot_path(r: &mut NativeMethodRegistry) {
    use cratonvm_native_builtins as nb;

    r.set_drop_real_layout_synthetic(true);

    nb::register_essential_natives_with_shims(r, nb::app_shims::ShimSelection::ALL);
    nb::register_concurrent_natives(r);
    // MUST follow `register_concurrent_natives` — same last-write-wins ordering
    // constraint `vm_init` documents at its own call site (:2163).
    nb::register_forkjoin_quiescence(r);
    nb::register_stamped_lock_natives(r);
    nb::phases_late::register_p61_file_handler(r);
    nb::servlet::register_url_classloader_close_bridge(r);

    cratonvm_native_io::register_io_natives(r);
    nb::phases_late::register_p60_process_handle(r);
    nb::phases_late::register_classvalue_natives(r);

    // ╔══ LAST-WRITE-WINS BOUNDARY — do not reorder ═══════════════════════╗
    // Verbatim from `vm_init` (:2360). The two calls after this one exist
    // *because* `register_collections_natives` overwrites earlier, correct
    // implementations: `securerandom` (collections re-registers every
    // `java/util/Random` method against a synthetic 2-field layout, so a seeded
    // `Random` returned all zeroes) and `properties_sidetable` (collections
    // re-registers `Properties` against the legacy HashMap layout, breaking
    // Surefire's load -> stringPropertyNames -> getProperty round-trip).
    //
    // This is the reason the gate cannot be built on `stub_ratchet.rs`'s
    // `register_boot_path`, which stops here.
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
    // `register_self_attach_natives` sit here in `vm_init`. They live in the
    // `vm` crate; see `UNMODELLED_VM_CRATE_REGISTRARS`.
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
    nb::register_slf4j_binder_stubs_pub(r);
}

fn shadowed() -> Vec<ShadowedRegistration> {
    let mut registry = NativeMethodRegistry::new();
    vm_init_real_jdk_boot_path(&mut registry);
    registry.shadowed_registrations()
}

fn report(rows: &[ShadowedRegistration], limit: usize) {
    for row in rows.iter().take(limit) {
        println!(
            "  {}\n      lost at {}  [{}]\n      WINS at {}  [{}]",
            row.triple(),
            row.shadowed_at.as_deref().unwrap_or("<unknown>"),
            row.shadowed_kind.as_str(),
            row.winner_at.as_deref().unwrap_or("<unknown>"),
            row.winner_kind.as_str(),
        );
    }
    if rows.len() > limit {
        println!("  ... and {} more", rows.len() - limit);
    }
}

// ===========================================================================
// THE GATE
// ===========================================================================

/// THE RATCHET. A new duplicate registration fails CI instead of costing a
/// measure-fix-rebuild cycle.
///
/// Prints the full shadow census with both provenance sites, so the failure
/// message is the diagnosis rather than a pointer to one.
#[test]
fn no_new_shadowed_registrations() {
    let rows = shadowed();

    println!(
        "dup-registration-gate: {} shadowed registrations (baseline {})",
        rows.len(),
        BASELINE_SHADOWED
    );
    println!("--- shadow census (loser -> winner) ---");
    report(&rows, 4000);
    println!("--- baseline paste line ---");
    println!("const BASELINE_SHADOWED: usize = {};", rows.len());

    assert!(
        rows.len() <= BASELINE_SHADOWED,
        "{} registrations are shadowed by a later registration of the same \
         triple, above the frozen baseline of {}. Each one is a callback that \
         can never be dispatched — and if it is the CORRECT one, the symptom \
         will point at the winner's file, not at yours. Either remove the \
         duplicate, or raise the baseline WITH a written justification for the \
         new entry in \
         docs/known-issues/jdk-only/W6-4-duplicate-registration-gate.md.",
        rows.len(),
        BASELINE_SHADOWED
    );
}

/// The high-signal subset: the winner and the loser disagree about what the
/// native IS. Instance 1 was a placeholder stub displacing a real bridge.
#[test]
fn no_new_kind_disagreements_between_a_winner_and_the_native_it_shadows() {
    let rows: Vec<_> = shadowed()
        .into_iter()
        .filter(|r| r.kind_disagreement())
        .collect();

    println!(
        "dup-registration-gate: {} kind disagreements (baseline {})",
        rows.len(),
        BASELINE_KIND_DISAGREEMENTS
    );
    report(&rows, 400);
    println!(
        "const BASELINE_KIND_DISAGREEMENTS: usize = {};",
        rows.len()
    );

    assert!(
        rows.len() <= BASELINE_KIND_DISAGREEMENTS,
        "{} registrations were displaced by a twin of a DIFFERENT NativeKind, \
         above the frozen baseline of {}. This is never cosmetic: JdkOnly \
         refuses a SyntheticStub, CRATONVM_NO_STUBS drops one, and \
         synthetic_stub_kind_should_yield_to_real_bytecode arbitrates for one.",
        rows.len(),
        BASELINE_KIND_DISAGREEMENTS
    );
}

/// SOURCE WITNESS — the replayed sequence must still be `vm_init`'s.
///
/// This is the test that keeps the other two honest, and unlike them it needs
/// no measurement and is green today. A duplicate census answers "which
/// registration wins", and that answer is **entirely** a function of ORDER. If
/// `vm_init` grows a registrar this file does not replay, or runs two in the
/// other order, every winner column downstream of the change is wrong — and
/// wrong in the confident, plausible way that made this species cost four build
/// cycles.
///
/// Two separate failures, because they need different fixes:
///
///   * an **order inversion** — two registrars this file replays, run by
///     `vm_init` in the opposite relative order — is a hard failure with no
///     baseline, because it means the model is actively lying.
///   * an **unmodelled registrar** ratchets against
///     [`UNMODELLED_VM_CRATE_REGISTRARS`], which is 2 (both in the `vm` crate,
///     which this crate cannot depend on).
///
/// Reads the working tree rather than a frozen copy, so it measures the source
/// as it is now. It is skipped, not failed, if `vm_init.rs` is not on disk —
/// a packaged crate has no sibling `vm/`.
#[test]
fn the_replayed_sequence_matches_vm_init() {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("vm")
        .join("src")
        .join("vm")
        .join("vm_init.rs");
    let Ok(src) = std::fs::read_to_string(&path) else {
        println!("vm_init.rs not on disk at {path:?}; witness skipped");
        return;
    };

    // Isolate the `#[cfg(not(feature = "synthetic-jdk"))]` arm — the DEFAULT
    // `cratonvm-cli` build. The sibling `#[cfg(feature = "synthetic-jdk")]`
    // block has its own real-JDK arm, and a prior lane established that the
    // phase registrars there run only when `config.use_synthetic_jdk` is true
    // AT RUNTIME, not merely when the Cargo feature is on. Scanning the whole
    // file would splice the two into one imaginary sequence.
    let lines: Vec<&str> = src.lines().collect();
    let start = lines
        .iter()
        .position(|l| l.contains("cfg(not(feature = \"synthetic-jdk\"))"))
        .expect("vm_init.rs must still have a real-JDK-only arm");
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

    // Bare `register_*` calls at the start of a statement. Deliberately not a
    // general call matcher: nested `registry.register(...)` calls inside the
    // arm are registrations, not registrars, and belong to whichever registrar
    // encloses them.
    let mut observed: Vec<String> = Vec::new();
    for line in &lines[start..end] {
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

    let unmodelled: Vec<&String> = observed
        .iter()
        .filter(|n| !VM_INIT_SEQUENCE.contains(&n.as_str()))
        .collect();
    println!(
        "vm_init real-JDK arm: {} registrar calls, {} replayed here, \
         {} unmodelled",
        observed.len(),
        observed.len() - unmodelled.len(),
        unmodelled.len()
    );
    for n in &unmodelled {
        println!("  UNMODELLED: {n}");
    }

    // Order inversion: for every pair this file replays, the relative order in
    // `vm_init` must match. This is the assertion that protects the winner
    // column, and it carries no baseline on purpose.
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
                    "vm_init runs `{name}` in an order VM_INIT_SEQUENCE does \
                     not allow. Registration is last-write-wins, so this makes \
                     every 'which registration wins' answer in this gate \
                     WRONG. Re-derive VM_INIT_SEQUENCE and \
                     `vm_init_real_jdk_boot_path` from vm_init.rs."
                ),
            }
        }
    }

    assert!(
        unmodelled.len() <= UNMODELLED_VM_CRATE_REGISTRARS,
        "vm_init's real-JDK arm calls {} registrars this gate does not replay \
         (allowed: {}, the two `crate::runtime::instrument::*` ones that live \
         in the `vm` crate). Every registration made by an unmodelled \
         registrar is invisible to the shadow census, and one that runs LATE \
         can make this gate name the wrong winner. Add it to \
         VM_INIT_SEQUENCE and to `vm_init_real_jdk_boot_path`, in position.",
        unmodelled.len(),
        UNMODELLED_VM_CRATE_REGISTRARS
    );
}

/// The gate must be measuring a real registry, not an empty one.
///
/// A negative control for the two ratchets above: if a refactor made
/// `vm_init_real_jdk_boot_path` register nothing, both `<=` assertions would
/// pass at zero and the gate would silently become a test of nothing. That is
/// the failure mode `stub_ratchet.rs` shipped for months (it measured about
/// four fifths of the registry while claiming to measure all of it).
#[test]
fn the_gate_measures_a_populated_registry() {
    let mut registry = NativeMethodRegistry::new();
    vm_init_real_jdk_boot_path(&mut registry);
    let census = registry.census();
    println!(
        "dup-registration-gate: {} registrations, {} slots",
        census.len(),
        registry.len()
    );
    assert!(
        census.len() > 5_000,
        "the real-JDK boot path registers thousands of natives; {} means the \
         replay is broken and both ratchets are vacuous",
        census.len()
    );
    // And the two are not equal — the gap IS the shadowed set.
    assert!(
        census.len() > registry.len(),
        "every registration owns a distinct slot, so nothing is shadowed. \
         Either the analysis broke or `register` stopped being \
         last-write-wins; both invalidate this gate."
    );
}
