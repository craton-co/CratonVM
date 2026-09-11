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
///
/// # Independently corroborated
///
/// A separate session on `dev` measured this surface at the same time and
/// seeded a single-baseline `BASELINE_SHADOWED = 1206` — the same count this
/// configuration reports, arrived at from a different working tree. That
/// agreement is why the number is trusted. Its companion kind-disagreement
/// count differed by one (53 there, 52 here), which is exactly what the
/// per-configuration split below exists to keep legible rather than average
/// away.
const BASELINE_SHADOWED_MANAGEMENT: Option<usize> = Some(1201);

/// [`BASELINE_SHADOWED_MANAGEMENT`] for the non-shipping `-p`-only resolve.
/// Seeded and read independently; the two are different registries.
///
/// **Re-seeded 1148 → 1150 on 2026-08-11, from a real run**, after the
/// `VM_MINTED_STAND_IN_RECEIVERS` → `VM_SERVICE_RECEIVERS` retag moved nine
/// registrations `synthetic-stub` → `bridge` and changed which rows this census
/// counts. The gate did its job: it named the delta rather than absorbing it.
///
/// The run reported 1152. Two of those four were a genuine duplicate and were
/// **deleted** rather than baselined — `cratonvm/internal/ArrayListSubList`'s
/// `toArray(T[])`/`toArray(IntFunction)` were registered twice in one function,
/// byte-identically, by two independent regression fixes (JUnit Platform /
/// ES-FAIL-04 and hibernate-smoke) neither of which noticed the other.
///
/// The remaining two are **legitimate and must stay shadowed**:
/// `cratonvm/internal/UnmodifiableEntrySet`'s `iterator()` and `forEach` are a
/// deliberate override of the plain-`Set` pair the shared loop registers for
/// many classes, so each yielded `Map.Entry` is wrapped in
/// `UnmodifiableMapEntry` and `setValue()` throws instead of silently mutating
/// the backing map through a "locked" view. Deleting either loser would
/// reintroduce the Tomcat parameter-map defect that override was written for.
/// This is the "raise it WITH a written justification" arm, and this is the
/// justification.
const BASELINE_SHADOWED_NO_MANAGEMENT: Option<usize> = Some(1150);

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
const BASELINE_KIND_DISAGREEMENTS_MANAGEMENT: Option<usize> = Some(51);

/// [`BASELINE_KIND_DISAGREEMENTS_MANAGEMENT`] for the non-shipping resolve.
const BASELINE_KIND_DISAGREEMENTS_NO_MANAGEMENT: Option<usize> = Some(51);

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

// ===========================================================================
// THE BOOT PATH — one model, shared with `stub_ratchet.rs`
// ===========================================================================

/// `VM_INIT_SEQUENCE`, the replay, and the source witness over
/// `vm/src/vm/vm_init.rs`.
///
/// This file and `stub_ratchet.rs` each carried a near-identical copy. The
/// duplication was deliberate — two integration-test binaries cannot share a
/// module without a file like this one — and its recorded failure mode was
/// *redundant maintenance* rather than silent disagreement, because two
/// witnesses read the same source file. On 2026-08-12 the maintenance came due:
/// **both witnesses located the arm with a `contains` match that hit a COMMENT
/// quoting `#[cfg(not(feature = "synthetic-jdk"))]` 44 lines above the
/// attribute**, so both scanned 39 lines of the SIBLING synthetic arm, both
/// observed 8 registrars instead of 48, and both passed while asserting nothing
/// about the arm they name. One locator defect, two files — which is the whole
/// argument for collapsing them.
///
/// Collapsed per
/// docs/known-issues/jdk-only/W7-30-stub-ratchet-boot-path-scope.md §7. The
/// witness is compiled into both binaries and runs once per binary.
#[path = "common/vm_init_boot_path.rs"]
mod boot_path;

use boot_path::vm_init_real_jdk_boot_path;

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

/// Instance 5, and the reason this file gained a per-triple test: an EMPTY
/// STRING where the text should be.
///
/// `java/nio/CharBuffer.toString()Ljava/lang/String;` was registered twice in
/// `register_p62_char_buffer`, ~340 lines apart. The later one read only the
/// backing `char[]` through `cb_read_hb` and returned `""` when there was none
/// — and a real `java.nio.StringCharBuffer` has none: it holds the wrapped
/// sequence in `str`. Being later, it WON.
///
/// The symptom split by CALL ROUTE, which is what made it expensive. A direct
/// bytecode `cb.toString()` resolves the EXACT class and finds
/// `StringCharBuffer.toString()` (a separate registration, correct). Anything
/// that resolves the inherited declaration instead — `Method.invoke`,
/// `NativeContext::invoke_virtual`, and through them `String.valueOf(Object)`,
/// string concatenation and `StringBuilder.append` — landed on
/// `java/nio/CharBuffer.toString()` and got `""`. The investigation that
/// preceded this test ruled out buffer state, the bytecode chain,
/// abstract-override dispatch and the force-native gate before reaching the
/// registry, because nothing pointed at a second registrar.
///
/// The whole-workspace ratchet above already counted this row — it went 1207 to
/// 1206 when the duplicate was deleted — but a baseline that large cannot fail
/// for one triple coming back. This one can.
#[test]
fn char_buffer_to_string_is_not_shadowed() {
    const TRIPLE: &str = "java/nio/CharBuffer.toString()Ljava/lang/String;";
    let offenders: Vec<String> = shadowed()
        .iter()
        .filter(|row| row.triple() == TRIPLE)
        .map(|row| {
            format!(
                "lost at {} [{}] -> WINS at {} [{}]",
                row.shadowed_at.as_deref().unwrap_or("<unknown>"),
                row.shadowed_kind.as_str(),
                row.winner_at.as_deref().unwrap_or("<unknown>"),
                row.winner_kind.as_str(),
            )
        })
        .collect();
    assert!(
        offenders.is_empty(),
        "{TRIPLE} is registered more than once, so one of the callbacks can \
         never be dispatched:\n  {}\nThe last registration wins. If the winner \
         cannot read a `StringCharBuffer` (no `hb`, text in `str`), every \
         declaring-class route answers an empty string while a direct \
         `cb.toString()` stays correct — see \
         stringcharbuffer-tostring-empty-via-native-invoke-FIXED.md.",
        offenders.join("\n  ")
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

// SOURCE WITNESS — `the_replayed_sequence_matches_vm_init` moved to
// `tests/common/vm_init_boot_path.rs`, alongside the model it checks, and is
// compiled into this binary through the `mod boot_path;` above. It still runs
// under `cargo test -p cratonvm-native-builtins --test duplicate_registration_gate`.
//
// It is the test that keeps the two ratchets above honest: a duplicate census
// answers "which registration wins", and that answer is ENTIRELY a function of
// ORDER. It was also BLIND until 2026-08-12 — its arm locator matched a comment
// quoting the `cfg` attribute, so it scanned the sibling synthetic arm and
// observed 8 registrars instead of 48. The shared copy fixes the locator and
// adds the assertion that would have caught it: every name in
// `VM_INIT_SEQUENCE` must actually be OBSERVED in the scanned arm.

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

/// `java/util/jar/JarFile.<init>` has TWO producers, and which one wins is a
/// fact lane 1 §10 item 5 depends on.
///
/// The count-based ratchets above tolerate 1,201 shadowed rows, so a duplicate
/// is invisible individually — and this one is load-bearing. `JarFile`'s
/// constructors are registered by
/// `native-io/src/zip_real_jar.rs::register_jar_natives` (which mirrors the
/// whole surface onto `java/util/zip/ZipFile` as well) and again by
/// `native-builtins/src/phases_late/jar_manifest.rs::register_p59_jar`. The
/// two build DIFFERENT objects, and `register` is last-write-wins, so a reader
/// pricing a `JarFile` retirement from the wrong source file prices the wrong
/// native. This lane has already paid for that species once, on the
/// `ZoneInfoFile` duplicate whose note in `native-builtins/src/lib.rs` ends
/// "the later call always wins silently".
///
/// This test does not judge which producer SHOULD win — it pins that a
/// duplicate exists and prints both `file:line`s, so the next reader starts
/// from the measurement instead of a grep. If the duplicate is ever resolved,
/// the assert fires and the resolution gets recorded here.
#[test]
fn the_jarfile_constructor_has_two_producers_and_this_says_which_wins() {
    let census = shadowed();
    let ctors: Vec<&ShadowedRegistration> = census
        .iter()
        .filter(|r| r.triple().starts_with("java/util/jar/JarFile.<init>"))
        .collect();
    assert!(
        !ctors.is_empty(),
        "`java/util/jar/JarFile.<init>` reports no shadowed registration, so \
         either the second producer is gone — in which case delete this test \
         and record in lane 1 §10 item 5 which one survived — or the replay no \
         longer reaches one of the two registrars, which would make the \
         ratchets above vacuous for the whole `java/util/jar/` family."
    );
    let rendered: Vec<String> = ctors.iter().map(|r| render(r)).collect();
    // Printed unconditionally: the value of this test is the two locations,
    // and a passing test that hides them is the "reports and does not block"
    // shape this file's own `UNSEEDED_GRACE_ENDS_UNIX` note argues against.
    println!(
        "java/util/jar/JarFile.<init> — {} shadowed registration(s):\n{}",
        ctors.len(),
        rendered.join("\n")
    );
}

// TEMPORARY (wave 6, 2026-09-11): the census this wave's retirement tables are
// transcribed FROM. Deleted in the same wave that reads it — a table is only
// as good as the dump it came from, and a dump that stays becomes a second
// place to keep in step. Run with `--nocapture`.
#[test]
fn w6_print_candidate_triples() {
    let mut registry = NativeMethodRegistry::new();
    vm_init_real_jdk_boot_path(&mut registry);
    let dump = registry.dump_registrations();
    for prefix in [
        "java/text/BreakIterator",
        "java/util/Date",
        "java/util/TimeZone",
        "sun/util/calendar/",
        "sun/util/resources/",
        "sun/util/locale/provider/",
    ] {
        let mut rows: Vec<String> = dump
            .iter()
            .filter(|(c, _, _, _)| *c == prefix || c.starts_with(prefix))
            .map(|(c, m, d, k)| format!("    (\"{c}\", \"{m}\", \"{d}\"),  // {}", k.as_str()))
            .collect();
        rows.sort();
        println!("W6TRIPLES prefix={prefix} count={}", rows.len());
        for r in &rows {
            println!("W6ROW {r}");
        }
    }
}
