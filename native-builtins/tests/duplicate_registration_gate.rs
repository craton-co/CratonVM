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
//! # WHAT THIS CENSUS CANNOT SEE — read before quoting its number
//!
//! The census is **scoped, not total**. It observes exactly the registrations
//! made by the registrars [`vm_init_real_jdk_boot_path`] calls, minus the ones
//! `register` silently discards. A `0` here therefore means *"no duplicates
//! among the registrars this test calls, under this process's flags"* — never
//! *"no duplicates exist"*. Four separate blind spots, each demonstrated:
//!
//! 1. **The whole synthetic-JDK registration graph is unmeasured.**
//!    `register_builtins` -> `register_synthetic_overrides` -> the phase 50–72
//!    registrars run only when `config.use_synthetic_jdk` is true AT RUNTIME
//!    (`vm_init.rs`), and this file replays the other arm. A static scan of
//!    `native-builtins/src/lib.rs` alone finds **154** triples registered by
//!    BOTH `register_essential_natives_with_shims` and
//!    `register_synthetic_overrides`; not one of them can appear below.
//!
//! 2. **Registrars off the boot path are unmeasured even in real-JDK mode.**
//!    `classloader::register_classloader_natives` is never called here, and it
//!    holds live shadows: it re-registers `MethodHandles$Lookup.in` over
//!    `lang_invoke::register_p63_method_handles_lookup`'s copy, and
//!    `ClassLoader.defineClass0/1/2` over the copies in
//!    `register_essential_natives_with_shims` — where the SHADOWED body is the
//!    one carrying the hardened `checked_add` ByteBuffer decode. That is the
//!    exact defect this gate exists to catch, and it is invisible to it.
//!
//! 3. **A DROPPED registration leaves no row at all.** Every drop arm in
//!    `register` — the `JdkOnly` refusal, `drop_synthetic_stubs`,
//!    `real_net_sockets`, `real_forkjoinpool`, `drop_real_layout_synthetic` —
//!    `return`s BEFORE pushing to `registrations`. So a native that was
//!    silently discarded is not a shadowed row; it is no row. Measured
//!    instance: `register_forkjoin_quiescence` registered
//!    `ForkJoinPool.awaitQuiescence` on the ambient category, which falls back
//!    to `SyntheticStub`, which the real-ForkJoinPool arm drops — on the
//!    DEFAULT path, since `real_forkjoinpool` is on unless
//!    `CRATONVM_SYNTHETIC_FORKJOINPOOL` is set. The census showed one row and
//!    no shadow for a registration that was not happening.
//!
//! 4. **The number is flag-dependent.** Those same arms read
//!    `cratonvm_types::flags::flags()`, which latches on first read. Running
//!    this gate under `CRATONVM_SYNTHETIC_FORKJOINPOOL`, `CRATONVM_NO_STUBS`
//!    or `CRATONVM_REAL_NET_SOCKETS` measures a different registry. Baselines
//!    frozen below are for the unset default.
//!
//! 5. **The number is FEATURE-dependent, and that is a property of `vm_init`,
//!    not a wart of the replay.** `vm_init`'s real-JDK arm gates all ten
//!    `jmx::*` registrar calls with `#[cfg(feature = "management")]`
//!    (`vm/src/vm/vm_init.rs`, the block at :2728–:2784), and this crate's
//!    `jmx` module is itself `#[cfg(feature = "management")]`
//!    (`native-builtins/src/lib.rs:4128`). `cratonvm-vm` declares
//!    `default = ["awt", "management"]` and `management =
//!    ["cratonvm-native-builtins/management"]`, so **every shipping
//!    `cratonvm-cli` build has the feature**; this crate declares
//!    `default = []`, so a `-p cratonvm-native-builtins` resolve does not.
//!    The replay therefore carries the same `cfg` on the same ten calls, and
//!    the baselines are keyed per configuration — see [`MEASURED_CONFIG`].
//!
//!    Making the census feature-INDEPENDENT was considered and rejected. The
//!    only way to get one number is to drop those ten registrars from the
//!    model in *both* configurations, which would under-measure the build that
//!    actually ships. A gate that is confidently silent about the shipping
//!    registry is the exact species this file exists to catch.
//!
//! What it CAN see is worth stating too, because a prior lane predicted
//! otherwise: the `java/util/concurrent/locks/StampedLock` family **is** in
//! the census. `register_stamped_lock_natives` is called twice on the replayed
//! path — once from inside `register_essential_natives_with_shims`
//! (native-builtins/src/lib.rs) and once directly by `vm_init` — with the same
//! callbacks and the same stated `SyntheticStub` kind. Benign, and a good
//! worked example of why [`BASELINE_SHADOWED`] counts losers rather than
//! defects.
//!
//! **The "~21 shadowed StampedLock rows" figure that used to sit in this
//! paragraph was a PREDICTION, not a measurement, and it is deleted rather
//! than corrected.** A third caller was reported by one lane and a fourth
//! suspected in a `synthetic-jdk` build; a fourth lane recorded that the
//! `native-collections` twin was disabled at its call site in 2026-07 and has
//! since been deleted outright. Nobody has run the census. The number this
//! gate prints is the answer; arithmetic over registrar call counts is not.
//!
//! # Running it — the configuration is part of the answer
//!
//! ```text
//! # SHIPPING resolve — the number CI gates on:
//! cargo test -p cratonvm-native-builtins --features management \
//!     --test duplicate_registration_gate -- --nocapture
//!
//! # This crate's own default resolve — compiles the file with the ten jmx
//! # registrars absent. A NON-SHIPPING configuration; never quote its number.
//! cargo test -p cratonvm-native-builtins \
//!     --test duplicate_registration_gate -- --nocapture
//! ```
//!
//! Do not seed from a `cargo test --workspace` run. That resolve unifies
//! features across every member, so `management` arrives via `cratonvm-vm`'s
//! defaults and the number happens to match the shipping one — but nothing in
//! the command says so, and a member that later stops enabling `management`
//! would silently move the baseline. `-p` plus an explicit `--features` states
//! the configuration it measured.

use cratonvm_native_api::registry::ShadowedRegistration;
use cratonvm_native_api::NativeMethodRegistry;

// ===========================================================================
// BASELINES
// ===========================================================================

/// Which configuration this build measures. Printed on every line this gate
/// emits and named in every failure message.
///
/// The census is not the same number in the two of them — blind spot 5 in the
/// header — so a number quoted without its configuration is not a measurement.
///
/// `management` ON is the SHIPPING resolve: `cratonvm-vm` declares
/// `default = ["awt", "management"]` and forwards it to this crate, so every
/// `cratonvm-cli` build has the ten `jmx::*` registrars. `management` OFF is a
/// `-p cratonvm-native-builtins` resolve, which **no product build produces**.
/// That arm exists so this file compiles and runs in this crate's own default
/// feature set — a guard against exactly the breakage described below — and
/// its number must never be quoted as "the" duplicate count.
///
/// # Why the OFF arm has to exist at all
///
/// Until this change the ten `jmx::*` calls in [`vm_init_real_jdk_boot_path`]
/// carried no `cfg`, so `cargo test -p cratonvm-native-builtins --test
/// duplicate_registration_gate` failed to COMPILE with ten `E0433`s: the `jmx`
/// module is `#[cfg(feature = "management")]`. The gate looked green under
/// `cargo check --workspace --all-targets`, where feature unification supplies
/// `management`, and was uncompilable on its own. `.github/workflows/ci.yml`'s
/// `cargo check --all-targets -p cratonvm-native-builtins --features
/// app-stubs,synthetic-quarkus-arc,legacy-synthetic-crypto` step is a `-p`
/// resolve with `management` off, so that break was live in CI, not
/// hypothetical.
#[cfg(feature = "management")]
const MEASURED_CONFIG: &str = "management (SHIPPING resolve)";
#[cfg(not(feature = "management"))]
const MEASURED_CONFIG: &str = "no-management (NON-SHIPPING: ten jmx registrars absent)";

/// Frozen upper bound on the number of SHADOWED registrations — registrations
/// a later `register*` of the identical triple displaced — in the real-JDK boot
/// registry, for the `management` (shipping) configuration.
///
/// One row per LOSER, not per triple: a triple registered four times
/// contributes three. That is deliberate, so that removing one of the four
/// competing `SSLContext.getInstance` registrations moves the number and a
/// partial fix can be scored.
///
/// # `None` means UNSEEDED, and that is a distinct state from `Some(0)`
///
/// This used to be `usize = 0`, which conflated "measured, and it really is
/// zero" with "nobody has run it yet". The conflation was not academic: at
/// `0`, [`no_new_shadowed_registrations`] asserts `shadowed <= 0` while
/// [`the_gate_measures_a_populated_registry`] asserts `census > slots`, i.e.
/// `shadowed >= 1`. **The two could not both pass**, so the target was red in
/// every configuration in which it compiled — including under `cargo test
/// --workspace`, which runs it. An `Option` cannot express that contradiction.
///
/// While `None` the ratchets report and do not fail; see [`adjudicate`] and
/// [`UNSEEDED_GRACE_ENDS_UNIX`], which stops that state from becoming
/// permanent.
///
/// # Seeding
///
/// Never seed a guess. A baseline set above the true count is a gate that
/// silently tolerates every duplicate below it — the failure mode this species
/// already has. Run the command in the header for the configuration you are
/// seeding and paste the `const ... = Some(N);` line the run prints; it names
/// the configuration-specific constant, so a paste into the wrong one is a
/// visible mistake rather than a silent cross-configuration baseline.
///
/// One-way ratchets afterwards: removing a duplicate is welcome and requires
/// lowering the number in the same change to lock the improvement in.
///
/// A **static** source scan of the boot-reachable registrars found 483 triples
/// registered from more than one call site (445 of them cross-file). Treat that
/// as an order-of-magnitude sanity check, not as the seed: it cannot resolve
/// class names held in variables, cannot see registrations behind runtime
/// flags, and counts triples rather than losers.
///
/// # This number is SCOPED — see "WHAT THIS CENSUS CANNOT SEE" above
///
/// It counts losers among the registrars [`vm_init_real_jdk_boot_path`] calls,
/// with this process's flags, in this feature configuration, excluding anything
/// `register` dropped. It is a ratchet on a subset, and a low value is not a
/// clean bill of health for the tree. Do not cite it as a total.
const BASELINE_SHADOWED_MANAGEMENT: Option<usize> = Some(1206);

/// [`BASELINE_SHADOWED_MANAGEMENT`] for the non-shipping `-p`-only resolve.
/// Seeded and read independently; the two are different registries.
const BASELINE_SHADOWED_NO_MANAGEMENT: Option<usize> = Some(1153);

/// Frozen upper bound on the SHADOWED registrations where the winner and the
/// loser disagree about `NativeKind` — the high-signal subset — for the
/// `management` (shipping) configuration.
///
/// This is instance 1 exactly: a placeholder `SyntheticStub` displacing a real
/// `Bridge`. It is never cosmetic, because `NativeKind` decides three separate
/// things: `CompatibilityMode::JdkOnly` refuses a `SyntheticStub`,
/// `CRATONVM_NO_STUBS` drops one, and
/// `synthetic_stub_kind_should_yield_to_real_bytecode` arbitrates for one. A
/// bridge displaced this way stops dispatching under `--jdk-only` while its
/// stated original would have been allowed.
///
/// `None` for the same reason as [`BASELINE_SHADOWED_MANAGEMENT`]. `register`'s
/// downgrade rule (an unchosen re-registration preserves a prior *chosen* kind)
/// already suppresses the benign majority, so this number should be small — if
/// the first run prints something large, that is itself the finding.
const BASELINE_KIND_DISAGREEMENTS_MANAGEMENT: Option<usize> = Some(52);

/// [`BASELINE_KIND_DISAGREEMENTS_MANAGEMENT`] for the non-shipping resolve.
const BASELINE_KIND_DISAGREEMENTS_NO_MANAGEMENT: Option<usize> = Some(52);

// The four constants above are all compiled in both configurations, on
// purpose: a reader seeding one can see the other, and neither can be edited
// by accident while invisible to the compiler. The `cfg` picks which pair this
// build ADJUDICATES against.

#[cfg(feature = "management")]
const BASELINE_SHADOWED: Option<usize> = BASELINE_SHADOWED_MANAGEMENT;
#[cfg(not(feature = "management"))]
const BASELINE_SHADOWED: Option<usize> = BASELINE_SHADOWED_NO_MANAGEMENT;

#[cfg(feature = "management")]
const BASELINE_KIND_DISAGREEMENTS: Option<usize> = BASELINE_KIND_DISAGREEMENTS_MANAGEMENT;
#[cfg(not(feature = "management"))]
const BASELINE_KIND_DISAGREEMENTS: Option<usize> = BASELINE_KIND_DISAGREEMENTS_NO_MANAGEMENT;

/// Name of the constant a run of THIS build should be pasted into. Emitted as
/// part of the paste line so the seed cannot land in the other configuration's
/// slot.
#[cfg(feature = "management")]
const SHADOWED_CONST: &str = "BASELINE_SHADOWED_MANAGEMENT";
#[cfg(not(feature = "management"))]
const SHADOWED_CONST: &str = "BASELINE_SHADOWED_NO_MANAGEMENT";

#[cfg(feature = "management")]
const KIND_CONST: &str = "BASELINE_KIND_DISAGREEMENTS_MANAGEMENT";
#[cfg(not(feature = "management"))]
const KIND_CONST: &str = "BASELINE_KIND_DISAGREEMENTS_NO_MANAGEMENT";

/// **2026-09-07T00:00:00Z.** Unix seconds after which an UNSEEDED baseline is
/// itself a failure.
///
/// A gate whose baselines are `None` reports and does not block. That is the
/// honest state for a number nobody has taken — but "reports and does not
/// block" is also the permanent-soft-gate shape this campaign exists to
/// excise, and the only difference between the two is whether anyone comes
/// back. This constant is what makes someone come back.
///
/// It is NOT the same species as the fixed wall-clock bounds this repository
/// spent a wave removing from `synthetic-jdk`. Those asserted that an
/// *operation* finished inside a duration, so they went red under load, at
/// random, and taught people to ignore CI. This reads the calendar and nothing
/// else: it is deterministic, it fires exactly once, and its failure text says
/// precisely what to do.
///
/// Two legitimate resolutions, and only two:
///
///   1. Seed the baselines from a real run (preferred — the gate becomes a
///      ratchet and this deadline stops being reachable), or
///   2. move this date forward **in the same commit as a written reason** for
///      why the measurement still has not been taken.
///
/// Deleting the deadline without doing (1) converts this file back into a
/// decorative guard.
const UNSEEDED_GRACE_ENDS_UNIX: u64 = 1_788_739_200;

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
///
/// **Deliberately NOT `cfg`-gated, unlike the replay.** The witness reads
/// `vm_init.rs` as TEXT, and the ten `jmx::*` calls are textually present in
/// that file whether or not `management` is enabled here. Gating this list on
/// the feature would make the witness report ten unmodelled registrars in the
/// default build and blow past `UNMODELLED_VM_CRATE_REGISTRARS`. The list
/// describes the source; the replay describes this build.
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

    // ╔══ `#[cfg(feature = "management")]` — COPIED FROM `vm_init`, not added ═╗
    // Every one of the ten calls below carries this exact `cfg` at its
    // `vm_init` call site (vm/src/vm/vm_init.rs:2728–:2784), and `nb::jmx` is
    // itself `#[cfg(feature = "management")]` (native-builtins/src/lib.rs:4128).
    // Replaying them unconditionally therefore did two wrong things at once: it
    // modelled a registry no build produces, and it made this test target
    // uncompilable in this crate's own default feature set (ten `E0433`s), so
    // the gate could only be built where feature unification happened to supply
    // `management`.
    //
    // The consequence for the census is stated rather than hidden: WITHOUT this
    // feature the number is ten registrars short of the shipping registry, and
    // the baseline is a different constant. See `MEASURED_CONFIG`.
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

fn shadowed() -> Vec<ShadowedRegistration> {
    let mut registry = NativeMethodRegistry::new();
    vm_init_real_jdk_boot_path(&mut registry);
    registry.shadowed_registrations()
}

/// One census row rendered as the three facts a reader needs: the triple, the
/// registration that LOST with its `file:line` and kind, and the one that WINS
/// with its `file:line` and kind.
fn render(row: &ShadowedRegistration) -> String {
    format!(
        "  {}\n      lost at {}  [{}]\n      WINS at {}  [{}]",
        row.triple(),
        row.shadowed_at.as_deref().unwrap_or("<unknown>"),
        row.shadowed_kind.as_str(),
        row.winner_at.as_deref().unwrap_or("<unknown>"),
        row.winner_kind.as_str(),
    )
}

/// The census as one diff-stable block.
///
/// Sorted by [`ShadowedRegistration::key`] rather than left in registration
/// order, so two runs can be diffed directly and a row that moved within its
/// file does not shuffle the whole listing. `key` is the triple plus the two
/// FILES with no line numbers, which is exactly the granularity a human
/// compares at.
fn census_block(rows: &[ShadowedRegistration], limit: usize) -> String {
    let mut sorted: Vec<&ShadowedRegistration> = rows.iter().collect();
    sorted.sort_by_key(|r| r.key());

    let mut out = String::new();
    for row in sorted.iter().take(limit) {
        out.push_str(&render(row));
        out.push('\n');
    }
    if sorted.len() > limit {
        out.push_str(&format!(
            "  ... and {} more (the full sorted census is in this test's \
             captured stdout above)\n",
            sorted.len() - limit
        ));
    }
    out
}

/// How many rows go into a PANIC message, as opposed to stdout.
///
/// The identity is the point — a count says something changed, the pair of
/// sites says what — so this is generous rather than a token head. It is
/// bounded at all only because a panic payload is assembled in memory and
/// duplicated into the test harness summary.
const PANIC_ROWS: usize = 200;

/// Wall-clock seconds since the Unix epoch, or `None` if the host clock is
/// before the epoch.
///
/// A clock that cannot be read is not a reason to fail an unseeded gate, so the
/// caller treats `None` as "grace period still open".
fn now_unix() -> Option<u64> {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .map(|d| d.as_secs())
}

/// Adjudicate one census against one baseline. The whole policy of this gate
/// lives here, so the two ratchets below cannot drift apart.
///
/// Three outcomes, and exactly one of them passes in silence:
///
///   * **seeded, within budget** — pass, having printed the number and the
///     configuration it belongs to;
///   * **seeded, over budget** — panic, naming every offending triple with
///     BOTH registration sites and which one WINS. A count tells you something
///     changed; this campaign needed the identity every single time;
///   * **unseeded** (`None`) — print the measurement and the paste line, and do
///     NOT assert. That is honest for a number nobody has taken, and
///     [`UNSEEDED_GRACE_ENDS_UNIX`] stops it from being permanent.
fn adjudicate(
    what: &str,
    why: &str,
    const_name: &str,
    baseline: Option<usize>,
    rows: &[ShadowedRegistration],
) {
    let n = rows.len();

    println!("dup-registration-gate [{MEASURED_CONFIG}]");
    match baseline {
        Some(b) => println!("  {n} {what} (baseline {b})"),
        None => println!("  {n} {what} (baseline UNSEEDED)"),
    }
    println!("--- {what}: census (loser -> winner), sorted by triple+files ---");
    print!("{}", census_block(rows, 4000));
    println!("--- paste line for the {MEASURED_CONFIG} configuration ---");
    println!("const {const_name}: Option<usize> = Some({n});");

    let Some(b) = baseline else {
        // UNSEEDED. Report loudly, block on nothing — except the deadline.
        println!(
            "::warning::dup-registration-gate: {const_name} is UNSEEDED, so the \
             {what} ratchet adjudicated NOTHING this run. It measured {n}. \
             Paste the const line above to turn this into a ratchet."
        );
        if let Some(now) = now_unix() {
            assert!(
                now < UNSEEDED_GRACE_ENDS_UNIX,
                "GRACE PERIOD EXPIRED. `{const_name}` is still `None`, so the \
                 {what} ratchet in duplicate_registration_gate.rs has been \
                 reporting and gating nothing since it was wired into CI. It \
                 measured {n} on this run, in the {MEASURED_CONFIG} \
                 configuration; the line to paste was printed immediately \
                 above.\n\n\
                 This deadline exists because 'reports, does not block' is the \
                 decorative-guard shape this campaign is excising, and the only \
                 thing separating it from a real gate is whether anyone comes \
                 back. Two legitimate fixes, and only two: seed the constant \
                 from a real run, or move UNSEEDED_GRACE_ENDS_UNIX forward IN \
                 THE SAME COMMIT as a written reason why the measurement still \
                 has not been taken. Deleting the deadline is not one of them."
            );
        }
        return;
    };

    if n <= b {
        return;
    }

    // Over budget. A COUNT baseline cannot say which rows are new — that is a
    // real limitation and it is stated rather than papered over, because a
    // reader who assumes otherwise will go hunting for the wrong row.
    panic!(
        "dup-registration-gate FIRED in the {MEASURED_CONFIG} configuration: \
         {n} {what}, above the frozen baseline of {b}.\n\n\
         {why}\n\n\
         Each row below is a callback that can never be dispatched. If it is \
         the CORRECT one, every symptom will point at the WINNER's file and \
         not at yours — that misdirection is what made this species cost four \
         measure-fix-rebuild cycles. Forcing 'the native' over real bytecode \
         does not help when the slot holds a DIFFERENT native.\n\n\
         `{const_name}` is a COUNT, so it cannot name which of these rows is \
         new. Diff this sorted block against the previous run's; the key is \
         triple + both FILES, so a registration that merely moved within its \
         file does not appear as a change.\n\n\
         Either remove the duplicate, or raise `{const_name}` WITH a written \
         justification for the new entry in \
         docs/known-issues/jdk-only/W6-4-duplicate-registration-gate.md.\n\n\
         {}",
        census_block(rows, PANIC_ROWS)
    );
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

    // The key set, printed BEFORE adjudication so it survives the panic. `key()`
    // is the triple plus both FILES with no line numbers — a stable identity for
    // one duplicate pair.
    //
    // It is here so that promoting this gate from a COUNT to a SET is one paste
    // rather than a redesign. A count cannot distinguish "one duplicate removed
    // and one added" from "nothing happened", and this campaign wanted the
    // identity every single time. Nobody should seed a set from a run they did
    // not take, so this prints and asserts nothing.
    let mut keys: Vec<String> = rows.iter().map(|r| r.key()).collect();
    keys.sort();
    println!(
        "--- key set: {} rows, for a future set-valued ratchet ---",
        keys.len()
    );
    for k in &keys {
        println!("{k}");
    }

    adjudicate(
        "shadowed registrations",
        "A shadowed registration is a callback the dispatcher can never \
         reach: `register` is last-write-wins and rewrites the slot in place, \
         so the LOSER's body is inert code that still reads as live.",
        SHADOWED_CONST,
        BASELINE_SHADOWED,
        &rows,
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

    adjudicate(
        "kind disagreements",
        "The winner and the loser disagree about what the native IS, which is \
         never cosmetic: `NativeKind` decides three separate things — \
         `CompatibilityMode::JdkOnly` refuses a `SyntheticStub`, \
         `CRATONVM_NO_STUBS` drops one, and \
         `synthetic_stub_kind_should_yield_to_real_bytecode` arbitrates for \
         one. A `Bridge` displaced by a `SyntheticStub` stops dispatching \
         under `--jdk-only` while its stated original would have been allowed. \
         That is instance 1 of this species exactly.",
        KIND_CONST,
        BASELINE_KIND_DISAGREEMENTS,
        &rows,
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
///
/// # This test is also the arithmetic identity the baselines rest on
///
/// `census.len()` counts REGISTRATIONS and `registry.len()` counts SLOTS
/// (distinct triples), and `register` rewrites a slot in place, so
///
/// ```text
/// shadowed_registrations().len() == census.len() - registry.len()
/// ```
///
/// exactly. The second assertion below therefore states `shadowed >= 1` — which
/// is why a placeholder `BASELINE_SHADOWED = 0` was not merely unseeded but
/// **contradictory**: it demanded `shadowed <= 0` from the same registry this
/// test demands `shadowed >= 1` from. One of the two was guaranteed to fail in
/// every configuration where the target compiled, `cargo test --workspace`
/// included. `Option::None` is what removed the contradiction; keep the two in
/// mind together before seeding a literal `Some(0)`.
#[test]
fn the_gate_measures_a_populated_registry() {
    let mut registry = NativeMethodRegistry::new();
    vm_init_real_jdk_boot_path(&mut registry);
    let census = registry.census();
    println!(
        "dup-registration-gate [{}]: {} registrations, {} slots, {} shadowed",
        MEASURED_CONFIG,
        census.len(),
        registry.len(),
        census.len().saturating_sub(registry.len())
    );
    assert!(
        census.len() > 5_000,
        "the real-JDK boot path registers thousands of natives; {} in the {} \
         configuration means the replay is broken and both ratchets are vacuous",
        census.len(),
        MEASURED_CONFIG
    );
    // And the two are not equal — the gap IS the shadowed set.
    assert!(
        census.len() > registry.len(),
        "{} registrations own {} distinct slots, so nothing is shadowed. Either \
         the analysis broke or `register` stopped being last-write-wins; both \
         invalidate this gate. (If this is genuinely a clean registry, it is \
         also the strongest possible result for the ratchets above — but it \
         must be adjudicated, not asserted away.)",
        census.len(),
        registry.len()
    );
}
