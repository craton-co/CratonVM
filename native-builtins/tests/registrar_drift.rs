// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! REGISTRAR MODE-DRIFT GATE — a `(class, name, descriptor)` triple may not
//! newly acquire two implementations, one per compatibility mode.
//!
//! # The species
//!
//! `NativeMethodRegistry::register` is last-write-wins with no unregister
//! API, and `register_builtins` runs `register_essential_natives` and *then*
//! `register_synthetic_overrides` — which is `#[cfg(feature = "synthetic-jdk")]`
//! and in no crate's default feature set. So when one triple is registered by
//! BOTH a pass that only `register_synthetic_overrides` can reach and a pass
//! the shipping binary reaches:
//!
//! * in synthetic-JDK mode the synthetic-only body is registered LAST and wins;
//! * in the shipping modes (`--jdk-only` included) the synthetic-only pass is
//!   not compiled in at all, so the shipping body is the only one there.
//!
//! **The two modes run different code for that triple**, and every test built
//! with `--features synthetic-jdk` measures the copy that does not ship. That
//! is the `register_pe_panama` / `structLayout` defect generalised: two files
//! carried two different layout objects behind one descriptor, and which one a
//! caller got was decided by which mode you booted. `phases_late/collections.rs`
//! and `native-collections/src/lib.rs` still carry that exact shape for
//! `TreeMap.ceilingEntry` today (see [`MUST_DRIFT`]).
//!
//! `registrar_reachability.rs` gates the *population* — which passes are
//! synthetic-only. It says nothing about which triples they share with the
//! shipping side. This file is that second gate, and F34-1 §8.1 is the request
//! for it.
//!
//! # What is new here, relative to `registrar_reachability.rs`
//!
//! 1. **Triple granularity.** Reachability is answered per pass; drift has to
//!    be answered per `(class, name, descriptor)`. F34-1 §8.2 records the gap
//!    this closes: a family whose classes are all also registered by a shipping
//!    pass, but where the shipping pass registers *fewer methods*, is invisible
//!    at class granularity.
//! 2. **Five crates, not one.** `native-builtins`, `native-collections`,
//!    `native-io`, `native-awt`, `vm`, plus the two `native-builtins-*`
//!    satellites. F34-1 §8.3 records that ~17 synthetic-only registrars live
//!    outside `native-builtins`, and — more importantly for drift — that most
//!    shipping TWINS do (`register_io_natives`, `register_tree_map_natives`,
//!    `channel_register_native` are all outside this crate). A one-crate scan
//!    manufactures false drift *and* misses real drift.
//! 3. **`for` loops are expanded.** F34-1 warned that four rows come out of a
//!    `for` loop so a `registry.register(` grep undercounts. It is not four:
//!    in this tree 695 register sites sit inside a `for x in ["a", "b"] { .. }`
//!    and expand to more than one triple each. A scan that cannot expand them
//!    silently drops whole classes.
//!
//! # What this scanner CANNOT see, stated up front
//!
//! Never trust the number without this list. It is deliberately identical to
//! the list in `docs/known-issues/jdk-only/G3-1-…-20260816.md` §3.
//!
//! * **Descriptors built with `format!`** — 18 sites. W7-5 §0 records a 60%
//!   over-statement that came from exactly this blind spot, which is why this
//!   file counts them (the `unresolved` map) instead of pretending they do
//!   not exist.
//! * **Registrars parameterised by class name** (`fn register_x(r, class:
//!   &str)`) — the class arrives from the call site, and this scanner does not
//!   propagate it. Their sites land in `unresolved`.
//! * **`for (name, desc) in [(..), (..)]`** — tuple-destructuring loops.
//! * **Iterating a named array const** (`for c in MAP_VIEW_CARRIERS`).
//! * **`NativeKind`.** This is the big one. `--jdk-only` REFUSES any
//!   registration whose kind is `SyntheticStub` (`native-api/src/registry.rs`
//!   `allowed_in`), so "the shipping copy wins in `--jdk-only`" is only true
//!   when the shipping copy is a `Bridge` or an `Intrinsic`. Kind is ambient
//!   registry state (`set_category` / `with_category`), threaded through call
//!   chains, and a source scan cannot resolve it. `java/time/Instant` is the
//!   worked counter-example: its shipping twin is tagged `SyntheticStub`, so in
//!   `--jdk-only` NEITHER copy is registered and the JDK's own bytecode runs.
//!   **A row in the baseline is a claim that two registrations exist, not a
//!   claim about which body a `--jdk-only` process ends up dispatching.** Only
//!   `--dump-native-registry` can answer that.
//!
//! # Why the numbers are a snapshot and not an invariant
//!
//! [`DRIFT_TRIPLES`] is **measured**, at 2026-08-17, on the working tree -- it
//! is not a specification and nothing derives it. It exists so this gate
//! ratchets instead of failing on day one against 1,244 pre-existing rows.
//! Every row is a debt, not a permission.
//!
//! # What changed on 2026-08-17, and what it cost
//!
//! The 2026-08-16 version of this file ratcheted on a per-pass **count**, and
//! its own mutation table named the hole (M11): *a drifting triple swapped for
//! another inside one pass, at the same count, is invisible.* It also shipped
//! the baseline as a CEILING rather than an equality, because it had never been
//! compiled and did not know the number its own resolver would produce.
//!
//! Both are addressed here:
//!
//! * the baseline is now the **set** of `(synthetic-only pass, triple)` pairs,
//!   not a count, so a swap fails (M11);
//! * the ratchet is **two-sided** -- a pair that stops drifting is a fix and
//!   must be recorded, exactly as `registrar_reachability.rs` treats a stale
//!   allow-list entry (N7);
//! * every failure prints the whole table, regenerated from that run, in
//!   paste-ready Rust (see `retake`), so re-taking it is one paste rather than
//!   a re-derivation.
//!
//! The cost is honest and large: ~1,900 lines of table, and a re-take needed
//! whenever the resolver legitimately improves. The 2026-08-16 record argued
//! against exactly this trade. It is being reversed because a gate that cannot
//! see a swap is not measuring the thing it names.
//!
//! # 2026-08-17: it has now been compiled and run
//!
//! Four consecutive lanes recorded "`cargo` was not available, this file has
//! never been compiled" as their single largest caveat. It was never a `cargo`
//! problem. This file and `registrar_reachability.rs` depend on nothing but
//! `std`, so
//!
//! ```text
//! CARGO_MANIFEST_DIR=<abs path to native-builtins> \
//!   rustc --edition 2021 --test -O -o drift_gate.exe tests/registrar_drift.rs
//! ./drift_gate.exe --test-threads=1 --nocapture
//! ```
//!
//! builds and runs the whole gate in about a second, with no workspace build
//! and no feature resolution. That is how the G54-1 lane checked every number
//! below. **It compiled clean on the first attempt** — none of the risks the
//! G41-1 record listed (the match-ergonomic destructuring in `retake`,
//! `baseline_by_pass`'s `entry(pass).or_default()`, the `&&String` bind in the
//! M12 filter) was real.
//!
//! What the first run found was not a compile error but a *fourth* independent
//! confirmation of the numbers: the Rust resolver produced 1,232 drifting
//! triples and 1,368 pairs, matching the arithmetic G50-1 predicted, and
//! `retake`'s regenerated table differed from the hand-edited one by exactly
//! the `register_byte_array_output_stream` entry and nothing else.
//!
//! [`DRIFT_TRIPLES`] says where the numbers came from and how to tell a
//! resolver disagreement from a real regression on a future run.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

// ===========================================================================
// SECTION 1 — configuration and the measured baseline
// ===========================================================================

/// Crates scanned, relative to the workspace root. Every one of them either
/// defines a synthetic-only registrar or defines a shipping twin of one; a
/// census that drops any of them reports a wrong number in both directions.
const CRATES: &[&str] = &[
    "native-builtins",
    "native-collections",
    "native-io",
    "native-awt",
    "vm",
    "native-builtins-crypto",
    "native-builtins-security",
];

/// The population is keyed on the ARGUMENT, not on the name: a registration
/// pass need not be called `register_*`. If this type is ever renamed, every
/// floor in [`the_drift_scanner_is_not_vacuous`] fires at once — which is the
/// point, and is the hole F34-1 §M4a found in its own gate.
const KEY_TYPE: &str = "NativeMethodRegistry";

/// The pass every synthetic-only chain runs through.
const SYNTHETIC_OVERRIDES: &str = "register_synthetic_overrides";

/// The cargo feature that gates it.
const SYNTHETIC_FEATURE: &str = "synthetic-jdk";

// --- measured floors -------------------------------------------------------
// Every one is a MEASURED value at 2026-08-17 minus generous slack. A floor
// with no measurement behind it is decoration; a floor that tracks the
// measurement exactly is a maintenance tax that gets relaxed under pressure.
//
// The 2026-08-16 measurements are kept beside the 2026-08-17 ones where they
// differ, because the delta is the honest record of how fast this tree moves
// while a parallel campaign is running: several crates gained commits between
// the two, and `native-io/src/lib.rs` -- which holds shipping twins -- was one
// of them. That is why the RATCHET below is a set and not an equality on
// counts.

/// `.rs` files parsed across all seven crate `src` trees (measured 361; 361).
const MIN_FILES: usize = 250;
/// `fn` definitions parsed (measured 34,062; was 33,710).
const MIN_FN_DEFS: usize = 20_000;
/// Definitions whose signature mentions `NativeMethodRegistry` (measured 845).
const MIN_PASSES: usize = 600;
/// `.register(` call sites found, of any arity (measured 14,088; was 14,063).
const MIN_REGISTER_SITES: usize = 9_000;
/// Sites whose first three arguments all resolved (measured 12,623).
const MIN_RESOLVED_SITES: usize = 8_000;
/// Distinct `(class, name, descriptor)` triples recovered (measured 11,458).
const MIN_TRIPLES: usize = 7_000;
/// Passes reachable from the shipping side (measured 509; was 521).
const MIN_SHIPPING: usize = 350;
/// Synthetic-only passes (measured 280; `registrar_reachability.rs` says 284
/// for `native-builtins` alone -- this scan sees the other crates' shipping
/// call sites, so a few names it calls synthetic-only are not).
const MIN_SYNTHETIC_ONLY: usize = 200;
/// Direct synthetic-only children of `register_synthetic_overrides`
/// (measured 73, exactly `registrar_reachability.rs`'s allow-list length).
const MIN_DIRECT_SYNTHETIC_ONLY: usize = 60;
/// Bytes of `register_synthetic_overrides`' extracted body (measured 104,469).
/// F34-1 §M4b: when the locator broke, ONLY a body-size floor noticed.
const MIN_SYNTHETIC_OVERRIDES_BODY: usize = 60_000;
/// Drifting triples (measured 1,244 after the `p62` fix recorded below; 1,268
/// before it). This is the anti-vacuity floor: a scanner that has stopped
/// resolving descriptors reports a small, clean, entirely fictional number, and
/// every other assertion here passes.
const MIN_TOTAL_DRIFT: usize = 900;
/// Sites inside an expanded `for` loop (measured 696). Without loop expansion
/// whole classes vanish from the census with no other symptom.
const MIN_LOOP_EXPANDED_SITES: usize = 400;

/// **M12, bounded.** Ceiling on register sites this scanner cannot resolve and
/// which therefore *could* hide drift: everything in `Analysis::unresolved`
/// except `arity<4`, which is the 499 proven non-registry `register` methods
/// (`thread_registry`, `SubstitutionRegistry`, `self` in `tck.rs`) classified by
/// receiver.
///
/// Measured 2026-08-17: 950 = 892 `unbound-identifier` + 18 `format!` +
/// 15 `no-enclosing-fn` + 14 `no-registry-owner` + 11 `expression`.
///
/// The 2026-08-16 record's admission was that new drift arriving through one of
/// these forms is invisible to the gate. It still is -- this constant does not
/// make it visible. What it does is **bound** it: the blind region cannot GROW
/// without failing, so a change that starts building descriptors with `format!`,
/// or moves a family behind a class-parameterised registrar, is caught as a loss
/// of coverage even though its drift is not caught as drift. That is a strictly
/// weaker guarantee than seeing the drift, and it is stated that way on purpose.
const MAX_BLIND_SITES: usize = 1_000;

/// Distinct drifting triples in [`DRIFT_TRIPLES`]. A cross-check, not a second
/// source of truth: if the table is edited by hand and this is not re-taken,
/// `the_baseline_is_well_formed` fails. Both are regenerated together by the
/// printer in `retake`.
///
/// **Re-taken 2026-08-20, +1: `javax/net/ssl/SSLContext.getProvider
/// ()Ljava/security/Provider;`.** That method had no registration at all until
/// then, so the real bytecode answered it -- `return provider;`, slot 0, which
/// is the PROTOCOL on every synthetic `SSLContext` layout, and `getProvider()`
/// handed back a `java.lang.String`. It is now registered in all three sets
/// (`tls::register_ssl_context`, `phases_late::ssl_security::register_p68_ssl`,
/// `net_phase_e::register_re6_ssl_context`), which is what makes it drift.
///
/// This row answers the gate's first question -- "do the two bodies agree?" --
/// with YES, structurally rather than by inspection: all three registrations
/// are the SAME free function, `jca::ssl_context_spi::ssl_context_provider`,
/// forwarded verbatim. There is one body; last-write-wins picks between three
/// pointers to it. See `jca/ssl_context_spi.rs` for why the guarded
/// `SSLContext` surface is deliberately registered three times over.
/// **Re-taken 2026-09-10, +51 distinct / +54 pairs, and NONE of it is new
/// drift.** The scanner was widened; these pairs were always there.
///
/// This scan required the byte after `register` to be `(`, so
/// `register_with_kind(class, name, descriptor, body, kind)` was skipped --
/// the old code said so in a comment and treated it as noise alongside
/// `registered_by`. It is not noise. **757 sites tree-wide** spell the call
/// that way, and they are not a uniform sample of the registry: they are
/// precisely the sites whose kind was ADJUDICATED. Converting
/// `register` -> `register_with_kind` is the standard remedy for a contract
/// 1.4 shadow, so every adjudication silently deleted the SHIPPING half of any
/// drift pair the triple belonged to, and this gate reported the deletion as
/// `STALE BASELINE -- recorded drift pair(s) no longer drift`. Good news,
/// wearing a defect's clothes, once per adjudication.
///
/// Found 2026-09-10 by tagging `java/lang/Class.getName` an `Intrinsic`. The
/// gate went red claiming two pairs had stopped drifting; only ONE of them
/// had, and not for the reason the message implied:
///
/// * `Class.getName` -- both registrations resolve to the SAME function,
///   `lang_class::native_class_get_name` (`use lang_class::*` at
///   `native-builtins/src/lib.rs:4785` makes the synthetic site's bare name
///   the qualified one). One body, two pointers, exactly the `SSLContext
///   .getProvider` case recorded below.
/// * `Class.getModule` -- two DIFFERENT closures, `lib.rs` versus
///   `phases_late/reflect_invoke.rs`. Still two implementations, one per mode.
///   It had been a recorded row here since before the tag; the tag hid it.
///
/// So the fix is the scanner, not the baseline. Both triples are back in the
/// table below where they always belonged, and the 54 pairs are what the blind
/// spot had been covering. They are recorded rather than adjudicated because
/// each needs its own "do the two bodies agree?" answer, and several are not
/// cosmetic -- `ClassLoader.defineClass0/1/2`, `Class.getSuperclass`,
/// `Class.isInstance`, `Class.isAssignableFrom`,
/// `ObjectStreamClass.hasStaticInitializer`. Routed to lanes L0, L3, L4 and L7
/// by `docs/contributing/jdk-only-lanes/`.
///
/// `registrar_reachability.rs`'s `FAMILY_DRIFT_EXPOSURE` was re-taken in the
/// same commit, which is what its own panic prescribes when this file moved:
/// six families rose (`register_classloader_natives` 81 -> 84,
/// `register_enterprise_final_natives` 118 -> 135,
/// `register_java_lang_extras_natives` 27 -> 28, `register_phase69_natives`
/// 8 -> 11, `register_serialization_natives` 2 -> 4,
/// `register_unsafe_define_class` 1 -> 2). Both numbers here came from this
/// gate's own paste-ready output, not from arithmetic.
const BASELINE_TOTAL_DRIFT: usize = 1274;

/// `(synthetic-only pass, triple)` PAIRS in [`DRIFT_TRIPLES`] -- larger than
/// [`BASELINE_TOTAL_DRIFT`] because one triple can be registered by several
/// synthetic-only passes (`AtomicBoolean.get` has two).
const BASELINE_TOTAL_PAIRS: usize = 1410;

/// Two triples that pin BOTH answers.
///
/// A one-sided control is worthless here: a scanner that has stopped
/// discriminating answers "drift" for everything and sails past a positive-only
/// control, and one that has stopped resolving answers "no drift" for
/// everything and sails past a negative-only one.
///
/// **POSITIVE** -- `ClassLoader.loadClass` MUST drift. Three synthetic-only
/// passes register it (`register_classloader_natives`,
/// `register_java_lang_extras_natives`, `register_p61_classloader`) against the
/// shipping `register_classloader_real_natives`. Chosen over the previous
/// positive control (`TreeMap.ceilingEntry`) because that one is now FIXED --
/// see [`FIXED_NOT_DRIFTING`] -- and because no nomination in the G41-1 record
/// proposes touching this family, so the control should not need re-pointing on
/// the next fix. MEASURED on a registry dump, both modes: `kind = bridge`,
/// `owns_slot = true`, `registered_by =
/// native-builtins/src/classloader_real.rs:841`.
///
/// **NEGATIVE** -- `MemoryLayout.structLayout` MUST NOT drift. It is F34-1's
/// worked example, and it was FIXED by F16: `panama.rs::register_pe2_struct_layouts`
/// deleted its whole group-layout family (the comment at `panama.rs:5829`
/// records why), leaving `phases_late/foreign_ffm.rs::register_p67_foreign_memory`
/// as the sole registrant. If this control ever reports drift, the twin came
/// back -- which is the single most likely way this defect recurs.
const CONTROL_POSITIVE: (&str, &str, &str) = (
    "java/lang/ClassLoader",
    "loadClass",
    "(Ljava/lang/String;)Ljava/lang/Class;",
);
const CONTROL_NEGATIVE: (&str, &str, &str) = (
    "java/lang/foreign/MemoryLayout",
    "structLayout",
    "([Ljava/lang/foreign/MemoryLayout;)Ljava/lang/foreign/StructLayout;",
);

/// Triples that must be present in the census at all, drifting or not.
///
/// Distinct from the controls: these guard the RESOLVER rather than the drift
/// verdict. Each needs a different resolution path to be recovered, so if any
/// one of them goes missing the corresponding path has broken silently.
const RESOLVER_WITNESSES: &[(&str, &str, &str, &str)] = &[
    (
        "java/io/ByteArrayOutputStream",
        "toByteArray",
        "()[B",
        "a `let <name> = \"...\"` binding read out of the enclosing fn. Until 2026-08-17 this \
         witnessed serialization.rs's `let cls = \"...\";`; that pass is deleted, so it now \
         witnesses native-io's `let baos = \"java/io/ByteArrayOutputStream\";` block. Same \
         resolution path, same assertion — presence in the census, drifting or not",
    ),
    (
        "java/util/concurrent/atomic/AtomicBoolean",
        "compareAndSet",
        "(ZZ)Z",
        "a `let c = \"...\"` binding inside a `with_category` closure",
    ),
    (
        "java/lang/foreign/MemoryLayout$PathElement",
        "groupElement",
        "(Ljava/lang/String;)Ljava/lang/foreign/MemoryLayout$PathElement;",
        "a plain three-literal call in panama.rs",
    ),
];

/// Triples whose two implementations were READ and found to differ materially.
///
/// These are the *live* rows: the shipping body and the synthetic-only body do
/// different things, so a `--features synthetic-jdk` test measuring one says
/// nothing about the other. Each must still be drifting -- if one stops, either
/// it was fixed (move it to [`FIXED_NOT_DRIFTING`] and say so in the record) or
/// the scanner stopped seeing it, which is worse.
///
/// The `TreeMap`/`TreeSet` rows that used to head this list are gone: they were
/// fixed, not lost, and they now live in [`FIXED_NOT_DRIFTING`].
///
/// Where a row says MEASURED, the claim is from `--dump-native-registry` in both
/// modes, not from reading source.
const MUST_DRIFT: &[(&str, &str, &str, &str)] = &[
    (
        "java/time/Instant",
        "getEpochSecond",
        "()J",
        "native_inst_get_epoch_second returns the raw slot; native_synthetic_instant_* \
         coerce Int<->Long. MEASURED: the shipping twin is kind=synthetic-stub \
         (native-builtins/src/lib.rs:41202) and the triple is ABSENT from the --jdk-only \
         registry dump entirely -- so in --jdk-only NEITHER is registered and real JDK \
         bytecode runs. The standing counter-example to 'the shipping copy wins'",
    ),
    (
        "java/util/concurrent/atomic/AtomicBoolean",
        "get",
        "()Z",
        "BENIGN BY IDENTITY, LIVE BY KIND: both sides name the same callback, but \
         lib.rs:8521 registers it inside with_category(SyntheticStub) and \
         util_concurrent_ext.rs sets Bridge. MEASURED: kind=synthetic-stub in the \
         compatible dump, ABSENT from the --jdk-only dump. This row is why a \
         same-callback drift row is not automatically nothing",
    ),
    (
        "org/slf4j/Logger",
        "debug",
        "(Ljava/lang/String;)V",
        "slf4j_log_msg (synthetic-only register_slf4j_natives) against slf4j_debug_msg \
         (shipping register_slf4j_binder_stubs_pub) -- the names alone say the level is \
         decided differently. MEASURED: kind=synthetic-stub in the compatible dump \
         (logging_shims.rs:2567), ABSENT from the --jdk-only dump",
    ),
];

/// Triples whose twin was COLLAPSED onto the shipping implementation, pinned so
/// the collapse cannot silently come back.
///
/// This is the two-sided control applied to a FIX rather than to the scanner:
/// each row must be **absent from the drift set** *and* **present in the
/// census**. Without the second half the check passes for the wrong reason the
/// moment the resolver loses the triple -- the "confident, vacuous zero" F34-1
/// §2.1 recorded twice.
///
/// # Provenance of the 12 `java/io/ByteArrayOutputStream` rows
///
/// `serialization.rs::register_byte_array_output_stream` bound all 12 to a
/// two-slot synthetic layout, with `flush`/`close` as `|_ctx, _args| Ok(None)`
/// no-ops. The G50-1 lane deleted the registrations and the five helpers they
/// exclusively owned (245 lines) after a two-mode registry dump:
/// `native-io/src/lib.rs` owns **13** `ByteArrayOutputStream` rows — a strict
/// superset of these 12, the extra being `write([B)V` — with `kind = bridge`,
/// `owns_slot = true` and `overwrote = null`, identically in compatible mode
/// and under `--jdk-only`, and **zero** registry rows in either mode name
/// `serialization.rs`. Five of the 13 carried non-zero `invocations` on an
/// `RSerial` run, which is positive proof the native-io bodies are the ones
/// that answer.
///
/// The deleted `close`/`flush` no-ops were the live half: `native_baos_close`
/// dispatches `BaosEvent::Close` and runs `process_pipe_output_close`, and
/// `native_baos_flush` dispatches `BaosEvent::Flush` and then
/// `ctx.fd_table().flush(fd)` — the machinery behind a `Process`'s stdin pipe,
/// which this VM models as a `ByteArrayOutputStream`.
///
/// These 12 are also the reason `registrar_reachability.rs` now carries
/// [`the_two_gates_agree_on_the_synthetic_only_population`]'s counterpart: that
/// file recorded this family as `0/12 triples also registered by a shipping
/// pass` while this one recorded 12/12, and the dump agreed with this one. See
/// `docs/known-issues/jdk-only/G54-1-…-20260817.md` §2.
///
/// # Provenance of the 24 navigation rows
///
/// `register_p62_navigable_expansion`
/// (`native-builtins/src/phases_late/collections.rs`) bound all 24 to local
/// `p62_tm_*` / `p62_ts_*` comparator-blind linear scans. The G41-1 lane deleted
/// the registrations and the bodies after checking F34-1 §5's trap **against a
/// registry dump rather than against source**:
///
/// * compatible mode: all 24 present, `kind = bridge`, `owns_slot = true`,
///   `overwrote = null`, owner `native-collections/src/lib.rs` (:48609-48651,
///   :48821-48863, :48980-48998, :49116-49134);
/// * `--jdk-only`: the identical 24 rows, same kind, same owner, and a
///   whole-registry `synthetic-stub` count of 0.
///
/// `Bridge` is `allowed_in(JdkOnly)`, so the shipping bodies serve all 24 in
/// both modes -- including the `NavigableMap`/`NavigableSet` *interface* rows,
/// which are the ones §5's trap is actually about.
///
/// # Provenance of the 8 ThreadMXBean rows and the 4 MemorySegment rows
///
/// 2026-08-24. Both families became visible only when `registrar_drift.rs`
/// learned one-level call-site parameter binding (`ffe741b6f`): each shipping
/// twin is a CLASS-PARAMETERISED registrar, which is the form the resolver
/// could not follow, so neither pair had ever been reported by any gate.
///
/// **ThreadMXBean (8).** `phases_late/management.rs::register_p59_management`
/// carried a whole `java/lang/management/ThreadMXBean` block, all 8 triples of
/// which `jmx.rs::register_thread_mxbean_for` also registers -- on a SUPERSET
/// of the class names, since it is called for `com/sun/management/ThreadMXBean`
/// too. The synthetic-only block is deleted.
///
/// Reading BOTH bodies mattered, because the better one was not the same one
/// twice:
///
/// * `getThreadCount` / `getTotalStartedThreadCount` / `getDaemonThreadCount`
///   were LIVE on the deleted side and frozen `<init>`-time slot reads on the
///   surviving side. `ManagementFactory.getThreadMXBean()` hands back one
///   cached bean, so a shipping binary answered the thread count as of whenever
///   that bean was first built, forever. `getPeakThreadCount`'s own comment
///   diagnoses exactly this and had been applied to one counter of four. The
///   other three are now live too.
/// * `isThreadContentionMonitoringSupported` was `false` on the deleted side
///   and `true` on the surviving one -- opposite answers, each under a comment
///   arguing for `false`. The comments are STALE:
///   `ThreadJmxSnapshot::blocked_time_ms`/`waited_time_ms` are real, filled by
///   `vm_exec.rs` from the registry and read back into `ThreadInfo.blockedTime`,
///   and `native_set_thread_contention_monitoring_enabled` resets them. The
///   capability exists; the prose describing its absence outlived it in two
///   files. So synthetic-JDK mode had been denying a feature this VM has.
///
/// **MemorySegment (4).** `panama.rs::register_pe2_string_marshaling_on` and
/// `phases_late/foreign_ffm.rs::register_p67_foreign_memory` each registered
/// `getUtf8String(J)` and `reinterpret(J)` on both `PE_SEGMENT_INTERFACE` and
/// `CRATON_SEGMENT_CLASS`. Here the SHIPPING copy was the weaker one twice:
///
/// * `getUtf8String` read `get_field(this, 0)` as the base address, which is
///   right only for the synthetic six-slot carrier -- on a real JDK-loaded
///   segment slot 0 is the byte LENGTH. That is the confusion
///   `panama_libffi::segment_address` exists to end; its comment records
///   `ofArray(new byte[16])` faulting at `address 0x10`, and 0x10 == 16 == that
///   array's length. It now points at `p67_segment_get_string`, the body that
///   already served the JDK-22 spelling `getString` and already reads through
///   `segment_address`/`segment_byte_size`.
/// * `reinterpret` shipped with NO native-access check, while the synthetic-only
///   twin refused unless `native_access_enabled()` -- calling the operation the
///   second half of the arbitrary-memory primitive, which is also how real JDK
///   25 treats it. The gated body survives, lifted out of its closure into
///   `panama::pe_segment_reinterpret` so the shipping pass can name it.
///
/// MEASURED, not read off the source, on `--dump-native-registry` from the
/// PATCHED tree in both modes (`RStrings`, debug binary 2026-08-24 18:59). All
/// 12 triples: `kind = bridge`, `owns_slot = true`, **`overwrote = null`**, one
/// owner each -- `jmx.rs` for the eight, `phases_late/foreign_ffm.rs` for the
/// four -- and byte-identical rows in compatible mode and under `--jdk-only`.
/// `overwrote = null` is the load-bearing field: it is positive evidence that
/// nothing registered these triples ahead of the survivor, i.e. the duplicate
/// really is gone rather than merely losing the race. `Bridge` is
/// `allowed_in(JdkOnly)`, and the strict dump reports `synthetic-stub: 0`
/// whole-registry, so the surviving bodies serve both modes.
/// **`AtomicReference.compareAndSet` (1), 2026-08-29.** The synthetic twin in
/// `util_concurrent_ext.rs` was deleted deliberately by the JUC VarHandle
/// composition lane, whose comment at that site records why: the real body is
/// one line -- `return VALUE.compareAndSet(this, expectedValue, newValue);` --
/// and `VarHandle.compareAndSet` is now thin-direct-bound, so the stub bought
/// nothing. `register_phase54_atomics` is the sole remaining registrar, which
/// is what "no longer drifts" means.
///
/// EVIDENCE, and it is source-side plus a negative runtime one rather than the
/// usual both-modes dump, because the surviving registrar is not reached in
/// either mode of the run used here. `--dump-native-registry` lists SIX
/// `AtomicReference` rows in compatible mode (`<init>` x2, `get`, `getAndSet`,
/// `lazySet`, `set`, `toString`, all `synthetic-stub` from
/// `util_concurrent_ext.rs`) and **no `compareAndSet` among them**, and no
/// `AtomicReference` rows at all under `--jdk-only` where the stubs are
/// dropped. So there is no duplicate registration in either mode -- which is
/// the property this bucket asserts -- and the sibling triples still in
/// `DRIFT_TRIPLES` are the control: they are still listed, so the scanner has
/// not simply lost sight of the class.
///
/// Moved by lane L3 while landing, because a stale baseline row fails this gate
/// for every lane and the deletion that made it stale is not L3's.
const FIXED_NOT_DRIFTING: &[(&str, &str, &str)] = &[
    (
        "java/util/concurrent/atomic/AtomicReference",
        "compareAndSet",
        "(Ljava/lang/Object;Ljava/lang/Object;)Z",
    ),
    (
        "java/lang/management/ThreadMXBean",
        "getAllThreadIds",
        "()[J",
    ),
    (
        "java/lang/management/ThreadMXBean",
        "getDaemonThreadCount",
        "()I",
    ),
    (
        "java/lang/management/ThreadMXBean",
        "getPeakThreadCount",
        "()I",
    ),
    ("java/lang/management/ThreadMXBean", "getThreadCount", "()I"),
    (
        "java/lang/management/ThreadMXBean",
        "getTotalStartedThreadCount",
        "()J",
    ),
    (
        "java/lang/management/ThreadMXBean",
        "isThreadContentionMonitoringEnabled",
        "()Z",
    ),
    (
        "java/lang/management/ThreadMXBean",
        "isThreadContentionMonitoringSupported",
        "()Z",
    ),
    (
        "java/lang/management/ThreadMXBean",
        "isThreadCpuTimeSupported",
        "()Z",
    ),
    (
        "java/lang/foreign/MemorySegment",
        "getUtf8String",
        "(J)Ljava/lang/String;",
    ),
    (
        "java/lang/foreign/MemorySegment",
        "reinterpret",
        "(J)Ljava/lang/foreign/MemorySegment;",
    ),
    (
        "cratonvm/internal/foreign/MemorySegmentImpl",
        "getUtf8String",
        "(J)Ljava/lang/String;",
    ),
    (
        "cratonvm/internal/foreign/MemorySegmentImpl",
        "reinterpret",
        "(J)Ljava/lang/foreign/MemorySegment;",
    ),
    // **AtomicReference.compareAndSet (1), 2026-08-29.** Collapsed by DELETION
    // of the shipping twin, not by a merge: `7c90ec930` de-registered
    // `util_concurrent_ext`'s copy because the method is one line on a real JDK
    // -- `VALUE.compareAndSet(this, expectedValue, newValue)` -- and the stub
    // had become the SLOWER of the two once the VarHandle reference CAS
    // underneath it was bound. The baseline was not re-taken in that commit, so
    // this row went stale and `the_drift_baseline_has_no_stale_rows` went red on
    // `dev`.
    //
    // MEASURED on `--dump-native-registry`, not read off the source, on a
    // `--features synthetic-jdk` debug binary (2026-08-29 18:34):
    //
    //   compareAndSet (Ljava/lang/Object;Ljava/lang/Object;)Z
    //     registered_by = native-builtins/src/phases_early.rs:20828
    //     owns_slot = true   kind = intrinsic   overwrote = null
    //
    // `overwrote = null` is the load-bearing field this list's header names: it
    // is positive evidence that nothing registers the triple ahead of the
    // survivor, so the duplicate is gone rather than merely losing the race.
    // Every OTHER AtomicReference method still shows the three-row
    // synthetic-stub / intrinsic / synthetic-stub chain and still drifts, which
    // is the negative control sitting in the same dump.
    //
    // ON "BOTH MODES", which for this triple is not the usual shape.
    // `register_phase54_atomics` is reached only through
    // `register_synthetic_overrides`, so the survivor exists ONLY in
    // synthetic-JDK mode. Compatible-mode and `--jdk-only` dumps from the same
    // tree carry SEVEN AtomicReference rows and `compareAndSet` is not among
    // them: there the JDK's own body runs, which is exactly what `7c90ec930`
    // intended and measured as faster. So no mode is served by a drifting pair,
    // which is what this list asserts -- one mode by the surviving native, the
    // other two by real bytecode.
    (
        "java/util/concurrent/atomic/AtomicReference",
        "compareAndSet",
        "(Ljava/lang/Object;Ljava/lang/Object;)Z",
    ),
    ("java/io/ByteArrayOutputStream", "<init>", "()V"),
    ("java/io/ByteArrayOutputStream", "<init>", "(I)V"),
    ("java/io/ByteArrayOutputStream", "close", "()V"),
    ("java/io/ByteArrayOutputStream", "flush", "()V"),
    ("java/io/ByteArrayOutputStream", "reset", "()V"),
    ("java/io/ByteArrayOutputStream", "size", "()I"),
    ("java/io/ByteArrayOutputStream", "toByteArray", "()[B"),
    (
        "java/io/ByteArrayOutputStream",
        "toString",
        "()Ljava/lang/String;",
    ),
    (
        "java/io/ByteArrayOutputStream",
        "toString",
        "(Ljava/lang/String;)Ljava/lang/String;",
    ),
    (
        "java/io/ByteArrayOutputStream",
        "toString",
        "(Ljava/nio/charset/Charset;)Ljava/lang/String;",
    ),
    ("java/io/ByteArrayOutputStream", "write", "(I)V"),
    ("java/io/ByteArrayOutputStream", "write", "([BII)V"),
    (
        "java/util/NavigableMap",
        "ceilingEntry",
        "(Ljava/lang/Object;)Ljava/util/Map$Entry;",
    ),
    (
        "java/util/NavigableMap",
        "ceilingKey",
        "(Ljava/lang/Object;)Ljava/lang/Object;",
    ),
    (
        "java/util/NavigableMap",
        "floorEntry",
        "(Ljava/lang/Object;)Ljava/util/Map$Entry;",
    ),
    (
        "java/util/NavigableMap",
        "floorKey",
        "(Ljava/lang/Object;)Ljava/lang/Object;",
    ),
    (
        "java/util/NavigableMap",
        "higherEntry",
        "(Ljava/lang/Object;)Ljava/util/Map$Entry;",
    ),
    (
        "java/util/NavigableMap",
        "higherKey",
        "(Ljava/lang/Object;)Ljava/lang/Object;",
    ),
    (
        "java/util/NavigableMap",
        "lowerEntry",
        "(Ljava/lang/Object;)Ljava/util/Map$Entry;",
    ),
    (
        "java/util/NavigableMap",
        "lowerKey",
        "(Ljava/lang/Object;)Ljava/lang/Object;",
    ),
    (
        "java/util/NavigableSet",
        "ceiling",
        "(Ljava/lang/Object;)Ljava/lang/Object;",
    ),
    (
        "java/util/NavigableSet",
        "floor",
        "(Ljava/lang/Object;)Ljava/lang/Object;",
    ),
    (
        "java/util/NavigableSet",
        "higher",
        "(Ljava/lang/Object;)Ljava/lang/Object;",
    ),
    (
        "java/util/NavigableSet",
        "lower",
        "(Ljava/lang/Object;)Ljava/lang/Object;",
    ),
    (
        "java/util/TreeMap",
        "ceilingEntry",
        "(Ljava/lang/Object;)Ljava/util/Map$Entry;",
    ),
    (
        "java/util/TreeMap",
        "ceilingKey",
        "(Ljava/lang/Object;)Ljava/lang/Object;",
    ),
    (
        "java/util/TreeMap",
        "floorEntry",
        "(Ljava/lang/Object;)Ljava/util/Map$Entry;",
    ),
    (
        "java/util/TreeMap",
        "floorKey",
        "(Ljava/lang/Object;)Ljava/lang/Object;",
    ),
    (
        "java/util/TreeMap",
        "higherEntry",
        "(Ljava/lang/Object;)Ljava/util/Map$Entry;",
    ),
    (
        "java/util/TreeMap",
        "higherKey",
        "(Ljava/lang/Object;)Ljava/lang/Object;",
    ),
    (
        "java/util/TreeMap",
        "lowerEntry",
        "(Ljava/lang/Object;)Ljava/util/Map$Entry;",
    ),
    (
        "java/util/TreeMap",
        "lowerKey",
        "(Ljava/lang/Object;)Ljava/lang/Object;",
    ),
    (
        "java/util/TreeSet",
        "ceiling",
        "(Ljava/lang/Object;)Ljava/lang/Object;",
    ),
    (
        "java/util/TreeSet",
        "floor",
        "(Ljava/lang/Object;)Ljava/lang/Object;",
    ),
    (
        "java/util/TreeSet",
        "higher",
        "(Ljava/lang/Object;)Ljava/lang/Object;",
    ),
    (
        "java/util/TreeSet",
        "lower",
        "(Ljava/lang/Object;)Ljava/lang/Object;",
    ),
];

/// The measured drift set, **per synthetic-only pass**, 2026-08-17.
///
/// # Why this is a SET and no longer a count
///
/// The 2026-08-16 version of this table was `&[(&str, usize)]` -- a per-pass
/// ceiling. Its own §5.2 named the hole and called it M11: *one drifting triple
/// swapped for another inside one pass is invisible, because the ceiling is a
/// count, not a set.* It also argued the trade was worth it, on the grounds
/// that pinning the triples exactly "would redden the gate on any harmless
/// resolver improvement".
///
/// That argument is reversed here, and the reversal has a cost worth stating:
/// this table is ~1,900 lines and it WILL need re-taking whenever the resolver
/// improves. In exchange, the ratchet now catches:
///
/// * a swap at constant count inside one pass (M11) -- the hole;
/// * a triple moving from one synthetic-only pass to another, which the global
///   ceiling could also net out;
/// * a new triple in a pass that already has a row, which the per-pass ceiling
///   caught only if the count went up.
///
/// The re-taking cost is paid down by `retake`, which prints this whole table in
/// paste-ready form on any failure of the ratchet. Re-taking it is one paste,
/// not a re-derivation.
///
/// # Provenance, stated plainly
///
/// **These rows did not come from this file.** `cargo` could not be run in the
/// lane that took them. They came from a line-by-line transliteration of
/// SECTION 2 and SECTION 3 below into Python -- not a second reading of the
/// spec, a port of this source -- run over the same working tree. Its totals
/// agreed with the 2026-08-16 Python mirror to within the commits that landed
/// between them (files 361/361, loop-expanded 696/695, direct synthetic-only
/// children 73/73, baseline rows 112/112 before the `p62` fix, total drift
/// 1,268/1,266).
///
/// Independently, 1,128 of those 1,268 pre-fix drifting triples were confirmed
/// present in a real `--dump-native-registry` -- the first time any number in
/// this family has been checked against a running VM.
///
/// **On 2026-08-17 this table WAS produced by the Rust code in this file**, and
/// the transliteration's numbers survived contact with it. Compiled with
/// `rustc --edition 2021 --test` (see the module header) against the tree at
/// `107efe18a`, the scanner reported 1,232 drifting triples over 110 passes and
/// 1,368 `(pass, triple)` pairs, and `retake`'s regenerated table was
/// byte-identical to the table above. Three sources now agree — the Python
/// port, the arithmetic in the G50-1 record, and this file's own scanner.
///
/// The tree does move underneath it: at that commit the scan saw 849 passes and
/// 34,197 `fn` defs where G41-1's port saw 843 and 34,059, and 512
/// shipping-reachable passes where it saw 507. **None of that moved the drift
/// set**, which is the useful observation: the census is far more stable than
/// its inputs.
///
/// If a future run is red on `no_new_mode_drift`, read the printed table before
/// assuming a regression: a handful of rows is the resolver, and re-taking is
/// one paste. A disagreement of hundreds is not, and
/// `the_drift_scanner_is_not_vacuous` should be read first. If
/// `the_two_gates_agree_on_the_synthetic_only_population` is red at the same
/// time, read THAT first — a shifted population moves every number here, and it
/// is what happened to `registrar_reachability.rs` on 2026-08-17.
///
/// A pass absent from this table has an allowance of ZERO triples -- so a pass
/// that starts drifting fails even though nothing else about it changed.
const DRIFT_TRIPLES: &[(&str, &[(&str, &str, &str)])] = &[
    (
        "register_aot_natives",
        &[
            ("java/lang/reflect/Method", "isAnnotationPresent", "(Ljava/lang/Class;)Z"),
        ],
    ),
    (
        "register_atomic_boolean_natives",
        &[
            ("java/util/concurrent/atomic/AtomicBoolean", "<init>", "()V"),
            ("java/util/concurrent/atomic/AtomicBoolean", "<init>", "(Z)V"),
            ("java/util/concurrent/atomic/AtomicBoolean", "compareAndSet", "(ZZ)Z"),
            ("java/util/concurrent/atomic/AtomicBoolean", "get", "()Z"),
            ("java/util/concurrent/atomic/AtomicBoolean", "getAndSet", "(Z)Z"),
            ("java/util/concurrent/atomic/AtomicBoolean", "lazySet", "(Z)V"),
            ("java/util/concurrent/atomic/AtomicBoolean", "set", "(Z)V"),
            ("java/util/concurrent/atomic/AtomicBoolean", "toString", "()Ljava/lang/String;"),
        ],
    ),
    (
        "register_bigdecimal_natives",
        &[
            ("java/math/BigDecimal", "<init>", "(D)V"),
            ("java/math/BigDecimal", "add", "(Ljava/math/BigDecimal;)Ljava/math/BigDecimal;"),
            ("java/math/BigDecimal", "doubleValue", "()D"),
            ("java/math/BigDecimal", "intValue", "()I"),
            ("java/math/BigDecimal", "longValue", "()J"),
            ("java/math/BigDecimal", "multiply", "(Ljava/math/BigDecimal;)Ljava/math/BigDecimal;"),
            ("java/math/BigDecimal", "negate", "()Ljava/math/BigDecimal;"),
            ("java/math/BigDecimal", "precision", "()I"),
            ("java/math/BigDecimal", "scale", "()I"),
            ("java/math/BigDecimal", "setScale", "(I)Ljava/math/BigDecimal;"),
            ("java/math/BigDecimal", "setScale", "(II)Ljava/math/BigDecimal;"),
            ("java/math/BigDecimal", "signum", "()I"),
            ("java/math/BigDecimal", "subtract", "(Ljava/math/BigDecimal;)Ljava/math/BigDecimal;"),
            ("java/math/BigDecimal", "toBigInteger", "()Ljava/math/BigInteger;"),
            ("java/math/BigDecimal", "toPlainString", "()Ljava/lang/String;"),
            ("java/math/BigDecimal", "toString", "()Ljava/lang/String;"),
            ("java/math/BigDecimal", "valueOf", "(D)Ljava/math/BigDecimal;"),
            ("java/math/BigDecimal", "valueOf", "(J)Ljava/math/BigDecimal;"),
        ],
    ),
    (
        "register_biginteger_natives",
        &[
            ("java/math/BigInteger", "add", "(Ljava/math/BigInteger;)Ljava/math/BigInteger;"),
            ("java/math/BigInteger", "compareTo", "(Ljava/math/BigInteger;)I"),
            ("java/math/BigInteger", "divide", "(Ljava/math/BigInteger;)Ljava/math/BigInteger;"),
            ("java/math/BigInteger", "equals", "(Ljava/lang/Object;)Z"),
            ("java/math/BigInteger", "gcd", "(Ljava/math/BigInteger;)Ljava/math/BigInteger;"),
            ("java/math/BigInteger", "intValue", "()I"),
            ("java/math/BigInteger", "isProbablePrime", "(I)Z"),
            ("java/math/BigInteger", "longValue", "()J"),
            ("java/math/BigInteger", "mod", "(Ljava/math/BigInteger;)Ljava/math/BigInteger;"),
            ("java/math/BigInteger", "modInverse", "(Ljava/math/BigInteger;)Ljava/math/BigInteger;"),
            ("java/math/BigInteger", "modPow", "(Ljava/math/BigInteger;Ljava/math/BigInteger;)Ljava/math/BigInteger;"),
            ("java/math/BigInteger", "multiply", "(Ljava/math/BigInteger;)Ljava/math/BigInteger;"),
            ("java/math/BigInteger", "negate", "()Ljava/math/BigInteger;"),
            ("java/math/BigInteger", "pow", "(I)Ljava/math/BigInteger;"),
            ("java/math/BigInteger", "remainder", "(Ljava/math/BigInteger;)Ljava/math/BigInteger;"),
            ("java/math/BigInteger", "signum", "()I"),
            ("java/math/BigInteger", "subtract", "(Ljava/math/BigInteger;)Ljava/math/BigInteger;"),
            ("java/math/BigInteger", "toString", "()Ljava/lang/String;"),
            ("java/math/BigInteger", "valueOf", "(J)Ljava/math/BigInteger;"),
        ],
    ),
    (
        "register_body_handlers",
        &[
            ("java/net/http/HttpResponse$BodyHandlers", "discarding", "()Ljava/net/http/HttpResponse$BodyHandler;"),
            ("java/net/http/HttpResponse$BodyHandlers", "ofByteArray", "()Ljava/net/http/HttpResponse$BodyHandler;"),
            ("java/net/http/HttpResponse$BodyHandlers", "ofString", "()Ljava/net/http/HttpResponse$BodyHandler;"),
        ],
    ),
    (
        "register_body_publisher",
        &[
            ("java/net/http/HttpRequest$BodyPublisher", "contentLength", "()J"),
            ("java/net/http/HttpRequest$BodyPublishers", "noBody", "()Ljava/net/http/HttpRequest$BodyPublisher;"),
            ("java/net/http/HttpRequest$BodyPublishers", "ofByteArray", "([B)Ljava/net/http/HttpRequest$BodyPublisher;"),
            ("java/net/http/HttpRequest$BodyPublishers", "ofString", "(Ljava/lang/String;)Ljava/net/http/HttpRequest$BodyPublisher;"),
        ],
    ),
    (
        "register_classloader_define_class",
        &[
            ("java/lang/ClassLoader", "defineClass0", "(Ljava/lang/ClassLoader;Ljava/lang/Class;Ljava/lang/String;[BIILjava/security/ProtectionDomain;ZILjava/lang/Object;)Ljava/lang/Class;"),
            ("java/lang/ClassLoader", "defineClass1", "(Ljava/lang/ClassLoader;Ljava/lang/String;[BIILjava/security/ProtectionDomain;Ljava/lang/String;)Ljava/lang/Class;"),
            ("java/lang/ClassLoader", "defineClass2", "(Ljava/lang/ClassLoader;Ljava/lang/String;Ljava/nio/ByteBuffer;IILjava/security/ProtectionDomain;Ljava/lang/String;)Ljava/lang/Class;"),
            ("java/lang/System$1", "defineClass", "(Ljava/lang/ClassLoader;Ljava/lang/Class;Ljava/lang/String;[BLjava/security/ProtectionDomain;ZILjava/lang/Object;)Ljava/lang/Class;"),
        ],
    ),
    (
        "register_classloader_natives",
        &[
            ("java/io/BufferedInputStream", "<init>", "(Ljava/io/InputStream;)V"),
            ("java/io/BufferedInputStream", "<init>", "(Ljava/io/InputStream;I)V"),
            ("java/io/BufferedInputStream", "available", "()I"),
            ("java/io/BufferedInputStream", "close", "()V"),
            ("java/io/BufferedInputStream", "mark", "(I)V"),
            ("java/io/BufferedInputStream", "markSupported", "()Z"),
            ("java/io/BufferedInputStream", "read", "()I"),
            ("java/io/BufferedInputStream", "read", "([BII)I"),
            ("java/io/BufferedInputStream", "reset", "()V"),
            ("java/io/ByteArrayInputStream", "<init>", "([B)V"),
            ("java/io/ByteArrayInputStream", "<init>", "([BII)V"),
            ("java/io/ByteArrayInputStream", "available", "()I"),
            ("java/io/ByteArrayInputStream", "read", "()I"),
            ("java/io/ByteArrayInputStream", "read", "([BII)I"),
            ("java/io/ByteArrayInputStream", "reset", "()V"),
            ("java/io/ByteArrayInputStream", "skip", "(J)J"),
            ("java/io/DataInputStream", "available", "()I"),
            ("java/io/DataInputStream", "close", "()V"),
            ("java/io/DataInputStream", "read", "()I"),
            ("java/io/DataInputStream", "read", "([BII)I"),
            ("java/io/DataInputStream", "readBoolean", "()Z"),
            ("java/io/DataInputStream", "readByte", "()B"),
            ("java/io/DataInputStream", "readChar", "()C"),
            ("java/io/DataInputStream", "readDouble", "()D"),
            ("java/io/DataInputStream", "readFloat", "()F"),
            ("java/io/DataInputStream", "readFully", "([B)V"),
            ("java/io/DataInputStream", "readFully", "([BII)V"),
            ("java/io/DataInputStream", "readInt", "()I"),
            ("java/io/DataInputStream", "readLong", "()J"),
            ("java/io/DataInputStream", "readShort", "()S"),
            ("java/io/DataInputStream", "readUTF", "()Ljava/lang/String;"),
            ("java/io/DataInputStream", "readUnsignedByte", "()I"),
            ("java/io/DataInputStream", "readUnsignedShort", "()I"),
            ("java/io/DataInputStream", "skipBytes", "(I)I"),
            ("java/io/FilterInputStream", "<init>", "(Ljava/io/InputStream;)V"),
            ("java/lang/ClassLoader", "<init>", "()V"),
            ("java/lang/ClassLoader", "<init>", "(Ljava/lang/ClassLoader;)V"),
            ("java/lang/ClassLoader", "<init>", "(Ljava/lang/String;Ljava/lang/ClassLoader;)V"),
            ("java/lang/ClassLoader", "findLoadedClass", "(Ljava/lang/String;)Ljava/lang/Class;"),
            ("java/lang/ClassLoader", "getDefinedPackage", "(Ljava/lang/String;)Ljava/lang/Package;"),
            ("java/lang/ClassLoader", "getDefinedPackages", "()[Ljava/lang/Package;"),
            ("java/lang/ClassLoader", "getName", "()Ljava/lang/String;"),
            ("java/lang/ClassLoader", "getPackages", "()[Ljava/lang/Package;"),
            ("java/lang/ClassLoader", "getParent", "()Ljava/lang/ClassLoader;"),
            ("java/lang/ClassLoader", "getPlatformClassLoader", "()Ljava/lang/ClassLoader;"),
            ("java/lang/ClassLoader", "getResource", "(Ljava/lang/String;)Ljava/net/URL;"),
            ("java/lang/ClassLoader", "getResourceAsStream", "(Ljava/lang/String;)Ljava/io/InputStream;"),
            ("java/lang/ClassLoader", "getResources", "(Ljava/lang/String;)Ljava/util/Enumeration;"),
            ("java/lang/ClassLoader", "getSystemClassLoader", "()Ljava/lang/ClassLoader;"),
            ("java/lang/ClassLoader", "getSystemResources", "(Ljava/lang/String;)Ljava/util/Enumeration;"),
            ("java/lang/ClassLoader", "loadClass", "(Ljava/lang/String;)Ljava/lang/Class;"),
            ("java/lang/ClassLoader", "loadClass", "(Ljava/lang/String;Z)Ljava/lang/Class;"),
            ("java/lang/ClassLoader", "registerAsParallelCapable", "()Z"),
            ("java/lang/ClassLoader", "setDefaultAssertionStatus", "(Z)V"),
            ("java/lang/invoke/MethodHandles$Lookup", "defineClass", "([B)Ljava/lang/Class;"),
            ("java/lang/invoke/MethodHandles$Lookup", "defineHiddenClass", "([BZ[Ljava/lang/invoke/MethodHandles$Lookup$ClassOption;)Ljava/lang/invoke/MethodHandles$Lookup;"),
            ("java/lang/invoke/MethodHandles$Lookup", "ensureInitialized", "(Ljava/lang/Class;)Ljava/lang/Class;"),
            ("java/lang/invoke/MethodHandles$Lookup", "in", "(Ljava/lang/Class;)Ljava/lang/invoke/MethodHandles$Lookup;"),
            ("java/lang/invoke/MethodHandles$Lookup", "lookupClass", "()Ljava/lang/Class;"),
            ("java/lang/invoke/MethodHandles$Lookup", "lookupModes", "()I"),
            ("java/lang/invoke/MethodHandles$Lookup", "unreflect", "(Ljava/lang/reflect/Method;)Ljava/lang/invoke/MethodHandle;"),
            ("java/lang/invoke/MethodHandles$Lookup", "unreflectSpecial", "(Ljava/lang/reflect/Method;Ljava/lang/Class;)Ljava/lang/invoke/MethodHandle;"),
            ("java/net/URLClassLoader", "<init>", "([Ljava/net/URL;)V"),
            ("java/net/URLClassLoader", "<init>", "([Ljava/net/URL;Ljava/lang/ClassLoader;)V"),
            ("java/net/URLClassLoader", "<init>", "([Ljava/net/URL;Ljava/lang/ClassLoader;Ljava/net/URLStreamHandlerFactory;)V"),
            ("java/net/URLClassLoader", "addURL", "(Ljava/net/URL;)V"),
            ("java/net/URLClassLoader", "close", "()V"),
            ("java/net/URLClassLoader", "findClass", "(Ljava/lang/String;)Ljava/lang/Class;"),
            ("java/net/URLClassLoader", "findResource", "(Ljava/lang/String;)Ljava/net/URL;"),
            ("java/net/URLClassLoader", "findResources", "(Ljava/lang/String;)Ljava/util/Enumeration;"),
            ("java/net/URLClassLoader", "getResourceAsStream", "(Ljava/lang/String;)Ljava/io/InputStream;"),
            ("java/net/URLClassLoader", "getURLs", "()[Ljava/net/URL;"),
            ("java/security/CodeSource", "getCertificates", "()[Ljava/security/cert/Certificate;"),
            ("java/security/CodeSource", "getLocation", "()Ljava/net/URL;"),
            ("java/security/ProtectionDomain", "getCodeSource", "()Ljava/security/CodeSource;"),
            ("java/security/ProtectionDomain", "implies", "(Ljava/security/Permission;)Z"),
            ("jdk/internal/loader/BuiltinClassLoader", "getResourceAsStream", "(Ljava/lang/String;)Ljava/io/InputStream;"),
            ("jdk/internal/loader/ClassLoaders$AppClassLoader", "getResourceAsStream", "(Ljava/lang/String;)Ljava/io/InputStream;"),
            ("jdk/internal/loader/ClassLoaders$PlatformClassLoader", "getResourceAsStream", "(Ljava/lang/String;)Ljava/io/InputStream;"),
            ("jdk/internal/misc/Unsafe", "defineClass", "(Ljava/lang/String;[BIILjava/lang/ClassLoader;Ljava/security/ProtectionDomain;)Ljava/lang/Class;"),
        ],
    ),
    (
        "register_completable_future_natives",
        &[
            ("java/util/concurrent/CompletableFuture", "complete", "(Ljava/lang/Object;)Z"),
            ("java/util/concurrent/CompletableFuture", "thenAccept", "(Ljava/util/function/Consumer;)Ljava/util/concurrent/CompletableFuture;"),
            ("java/util/concurrent/CompletableFuture", "thenApply", "(Ljava/util/function/Function;)Ljava/util/concurrent/CompletableFuture;"),
        ],
    ),
    (
        "register_core_stdlib_extras",
        &[
            ("java/io/FileDescriptor", "registerNatives", "()V"),
            ("java/io/FileInputStream", "registerNatives", "()V"),
            ("java/io/FileOutputStream", "registerNatives", "()V"),
            ("java/lang/Boolean", "toString", "()Ljava/lang/String;"),
            ("java/lang/Byte", "toString", "()Ljava/lang/String;"),
            ("java/lang/Character", "toString", "()Ljava/lang/String;"),
            ("java/lang/ClassLoader", "registerNatives", "()V"),
            ("java/lang/Double", "toString", "()Ljava/lang/String;"),
            ("java/lang/Float", "toString", "()Ljava/lang/String;"),
            ("java/lang/Integer", "compare", "(II)I"),
            ("java/lang/Integer", "toHexString", "(I)Ljava/lang/String;"),
            ("java/lang/Integer", "toString", "()Ljava/lang/String;"),
            ("java/lang/Long", "compare", "(JJ)I"),
            ("java/lang/Long", "toString", "()Ljava/lang/String;"),
            ("java/lang/Short", "toString", "()Ljava/lang/String;"),
            ("java/lang/String", "replace", "(Ljava/lang/CharSequence;Ljava/lang/CharSequence;)Ljava/lang/String;"),
            ("java/lang/invoke/MethodHandleNatives", "registerNatives", "()V"),
            ("java/nio/charset/Charset", "defaultCharset", "()Ljava/nio/charset/Charset;"),
            ("java/nio/charset/Charset", "displayName", "()Ljava/lang/String;"),
            ("java/nio/charset/Charset", "forName", "(Ljava/lang/String;)Ljava/nio/charset/Charset;"),
            ("java/nio/charset/Charset", "name", "()Ljava/lang/String;"),
            ("java/nio/charset/StandardCharsets", "<clinit>", "()V"),
            ("java/util/Arrays", "asList", "([Ljava/lang/Object;)Ljava/util/List;"),
            ("java/util/Arrays", "binarySearch", "([II)I"),
            ("java/util/Arrays", "copyOf", "([BI)[B"),
            ("java/util/Arrays", "copyOf", "([II)[I"),
            ("java/util/Arrays", "copyOf", "([Ljava/lang/Object;I)[Ljava/lang/Object;"),
            ("java/util/Arrays", "copyOf", "([Ljava/lang/Object;ILjava/lang/Class;)[Ljava/lang/Object;"),
            ("java/util/Arrays", "copyOfRange", "([BII)[B"),
            ("java/util/Arrays", "copyOfRange", "([Ljava/lang/Object;II)[Ljava/lang/Object;"),
            ("java/util/Arrays", "equals", "([I[I)Z"),
            ("java/util/Arrays", "fill", "([II)V"),
            ("java/util/Arrays", "fill", "([Ljava/lang/Object;Ljava/lang/Object;)V"),
            ("java/util/Arrays", "hashCode", "([B)I"),
            ("java/util/Arrays", "hashCode", "([Ljava/lang/Object;)I"),
            ("java/util/Arrays", "sort", "([I)V"),
            ("java/util/Arrays", "toString", "([Ljava/lang/Object;)Ljava/lang/String;"),
            ("java/util/Collections", "emptyEnumeration", "()Ljava/util/Enumeration;"),
            ("java/util/Collections", "emptyIterator", "()Ljava/util/Iterator;"),
            ("java/util/Collections", "emptyList", "()Ljava/util/List;"),
            ("java/util/Collections", "emptyMap", "()Ljava/util/Map;"),
            ("java/util/Collections", "emptySet", "()Ljava/util/Set;"),
            ("java/util/Collections", "reverse", "(Ljava/util/List;)V"),
            ("java/util/Collections", "shuffle", "(Ljava/util/List;)V"),
            ("java/util/Collections", "singleton", "(Ljava/lang/Object;)Ljava/util/Set;"),
            ("java/util/Collections", "singletonList", "(Ljava/lang/Object;)Ljava/util/List;"),
            ("java/util/Collections", "singletonMap", "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/util/Map;"),
            ("java/util/Collections", "synchronizedList", "(Ljava/util/List;)Ljava/util/List;"),
            ("java/util/Collections", "synchronizedMap", "(Ljava/util/Map;)Ljava/util/Map;"),
            ("java/util/Collections", "synchronizedSet", "(Ljava/util/Set;)Ljava/util/Set;"),
            ("java/util/Collections", "unmodifiableList", "(Ljava/util/List;)Ljava/util/List;"),
            ("java/util/Collections", "unmodifiableMap", "(Ljava/util/Map;)Ljava/util/Map;"),
            ("java/util/Collections", "unmodifiableSet", "(Ljava/util/Set;)Ljava/util/Set;"),
            ("java/util/HashMap", "forEach", "(Ljava/util/function/BiConsumer;)V"),
            ("java/util/HashMap", "getOrDefault", "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;"),
            ("java/util/HashMap", "putIfAbsent", "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;"),
            ("java/util/List", "copyOf", "(Ljava/util/Collection;)Ljava/util/List;"),
            ("java/util/Map", "copyOf", "(Ljava/util/Map;)Ljava/util/Map;"),
            ("java/util/Optional", "empty", "()Ljava/util/Optional;"),
            ("java/util/Optional", "get", "()Ljava/lang/Object;"),
            ("java/util/Optional", "ifPresent", "(Ljava/util/function/Consumer;)V"),
            ("java/util/Optional", "isEmpty", "()Z"),
            ("java/util/Optional", "isPresent", "()Z"),
            ("java/util/Optional", "map", "(Ljava/util/function/Function;)Ljava/util/Optional;"),
            ("java/util/Optional", "of", "(Ljava/lang/Object;)Ljava/util/Optional;"),
            ("java/util/Optional", "ofNullable", "(Ljava/lang/Object;)Ljava/util/Optional;"),
            ("java/util/Optional", "orElse", "(Ljava/lang/Object;)Ljava/lang/Object;"),
            ("java/util/Optional", "orElseGet", "(Ljava/util/function/Supplier;)Ljava/lang/Object;"),
            ("java/util/Optional", "toString", "()Ljava/lang/String;"),
            ("java/util/Properties", "<init>", "()V"),
            ("java/util/Properties", "containsKey", "(Ljava/lang/Object;)Z"),
            ("java/util/Properties", "getProperty", "(Ljava/lang/String;)Ljava/lang/String;"),
            ("java/util/Properties", "getProperty", "(Ljava/lang/String;Ljava/lang/String;)Ljava/lang/String;"),
            ("java/util/Properties", "setProperty", "(Ljava/lang/String;Ljava/lang/String;)Ljava/lang/Object;"),
            ("java/util/Properties", "size", "()I"),
            ("java/util/Properties", "stringPropertyNames", "()Ljava/util/Set;"),
            ("java/util/Set", "copyOf", "(Ljava/util/Collection;)Ljava/util/Set;"),
            ("java/util/StringJoiner", "<init>", "(Ljava/lang/CharSequence;)V"),
            ("java/util/StringJoiner", "<init>", "(Ljava/lang/CharSequence;Ljava/lang/CharSequence;Ljava/lang/CharSequence;)V"),
            ("java/util/StringJoiner", "add", "(Ljava/lang/CharSequence;)Ljava/util/StringJoiner;"),
            ("java/util/StringJoiner", "length", "()I"),
            ("java/util/StringJoiner", "toString", "()Ljava/lang/String;"),
            ("java/util/stream/Stream", "toList", "()Ljava/util/List;"),
            ("jdk/internal/misc/CDS", "defineArchivedModules", "(Ljava/lang/ClassLoader;Ljava/lang/ClassLoader;)V"),
            ("jdk/internal/misc/CDS", "dumpClassList", "(Ljava/lang/String;)V"),
            ("jdk/internal/misc/CDS", "dumpDynamicArchive", "(Ljava/lang/String;)V"),
            ("jdk/internal/misc/CDS", "getRandomSeedForDumping", "()J"),
            ("jdk/internal/misc/CDS", "initializeFromArchive", "(Ljava/lang/Class;)V"),
            ("jdk/internal/misc/CDS", "isDumpingArchive0", "()Z"),
            ("jdk/internal/misc/CDS", "isDumpingClassList0", "()Z"),
            ("jdk/internal/misc/CDS", "isSharingEnabled0", "()Z"),
            ("jdk/internal/misc/CDS", "logLambdaFormInvoker", "(Ljava/lang/String;)V"),
            ("jdk/internal/misc/ScopedMemoryAccess", "registerNatives", "()V"),
            ("jdk/internal/misc/Unsafe", "ensureClassInitialized0", "(Ljava/lang/Class;)V"),
            ("jdk/internal/misc/Unsafe", "fullFence", "()V"),
            ("jdk/internal/misc/Unsafe", "loadFence", "()V"),
            ("jdk/internal/misc/Unsafe", "storeFence", "()V"),
            ("jdk/internal/misc/VM", "awaitInitLevel", "(I)V"),
            ("jdk/internal/misc/VM", "getSavedProperty", "(Ljava/lang/String;)Ljava/lang/String;"),
            ("jdk/internal/misc/VM", "initLevel", "()I"),
            ("jdk/internal/misc/VM", "initialize", "()V"),
        ],
    ),
    (
        "register_crypto_impl_natives",
        &[
            ("java/security/SecureRandom", "generateSeed", "(I)[B"),
            ("java/security/SecureRandom", "nextBytes", "([B)V"),
        ],
    ),
    (
        "register_enterprise_final_natives",
        &[
            ("java/lang/Class", "asSubclass", "(Ljava/lang/Class;)Ljava/lang/Class;"),
            ("java/lang/Class", "cast", "(Ljava/lang/Object;)Ljava/lang/Object;"),
            ("java/lang/Class", "componentType", "()Ljava/lang/Class;"),
            ("java/lang/Class", "descriptorString", "()Ljava/lang/String;"),
            ("java/lang/Class", "desiredAssertionStatus", "()Z"),
            ("java/lang/Class", "getCanonicalName", "()Ljava/lang/String;"),
            ("java/lang/Class", "getClassLoader", "()Ljava/lang/ClassLoader;"),
            ("java/lang/Class", "getComponentType", "()Ljava/lang/Class;"),
            ("java/lang/Class", "getEnclosingClass", "()Ljava/lang/Class;"),
            ("java/lang/Class", "getEnclosingConstructor", "()Ljava/lang/reflect/Constructor;"),
            ("java/lang/Class", "getEnclosingMethod", "()Ljava/lang/reflect/Method;"),
            ("java/lang/Class", "getEnumConstants", "()[Ljava/lang/Object;"),
            ("java/lang/Class", "getEnumConstantsShared", "()[Ljava/lang/Object;"),
            ("java/lang/Class", "getPackage", "()Ljava/lang/Package;"),
            ("java/lang/Class", "getPackageName", "()Ljava/lang/String;"),
            ("java/lang/Class", "getTypeName", "()Ljava/lang/String;"),
            ("java/lang/Class", "isEnum", "()Z"),
            ("java/lang/Class", "isHidden", "()Z"),
        ],
    ),
    (
        "register_enum_map_natives",
        &[
            ("java/util/EnumMap", "<init>", "(Ljava/lang/Class;)V"),
        ],
    ),
    (
        "register_forkjoin_extras",
        &[
            ("java/util/concurrent/ForkJoinPool", "execute", "(Ljava/lang/Runnable;)V"),
            ("java/util/concurrent/ForkJoinPool", "execute", "(Ljava/util/concurrent/ForkJoinTask;)V"),
            ("java/util/concurrent/ForkJoinPool", "externalSubmit", "(Ljava/util/concurrent/ForkJoinTask;)Ljava/util/concurrent/ForkJoinTask;"),
        ],
    ),
    (
        "register_forkjoin_natives",
        &[
            ("java/util/concurrent/ForkJoinPool", "commonPool", "()Ljava/util/concurrent/ForkJoinPool;"),
            ("java/util/concurrent/ForkJoinPool", "getActiveThreadCount", "()I"),
            ("java/util/concurrent/ForkJoinPool", "getCommonPoolParallelism", "()I"),
            ("java/util/concurrent/ForkJoinPool", "getParallelism", "()I"),
            ("java/util/concurrent/ForkJoinPool", "invoke", "(Ljava/util/concurrent/ForkJoinTask;)Ljava/lang/Object;"),
            ("java/util/concurrent/ForkJoinPool", "submit", "(Ljava/lang/Runnable;)Ljava/util/concurrent/ForkJoinTask;"),
            ("java/util/concurrent/ForkJoinPool", "submit", "(Ljava/util/concurrent/ForkJoinTask;)Ljava/util/concurrent/ForkJoinTask;"),
            ("java/util/concurrent/ForkJoinTask", "cancel", "(Z)Z"),
            ("java/util/concurrent/ForkJoinTask", "complete", "(Ljava/lang/Object;)V"),
            ("java/util/concurrent/ForkJoinTask", "completeExceptionally", "(Ljava/lang/Throwable;)V"),
            ("java/util/concurrent/ForkJoinTask", "fork", "()Ljava/util/concurrent/ForkJoinTask;"),
            ("java/util/concurrent/ForkJoinTask", "get", "()Ljava/lang/Object;"),
            ("java/util/concurrent/ForkJoinTask", "invoke", "()Ljava/lang/Object;"),
            ("java/util/concurrent/ForkJoinTask", "isCancelled", "()Z"),
            ("java/util/concurrent/ForkJoinTask", "isCompletedAbnormally", "()Z"),
            ("java/util/concurrent/ForkJoinTask", "isCompletedNormally", "()Z"),
            ("java/util/concurrent/ForkJoinTask", "isDone", "()Z"),
            ("java/util/concurrent/ForkJoinTask", "join", "()Ljava/lang/Object;"),
            ("java/util/concurrent/RecursiveAction", "cancel", "(Z)Z"),
            ("java/util/concurrent/RecursiveAction", "completeExceptionally", "(Ljava/lang/Throwable;)V"),
            ("java/util/concurrent/RecursiveAction", "fork", "()Ljava/util/concurrent/ForkJoinTask;"),
            ("java/util/concurrent/RecursiveAction", "getRawResult", "()Ljava/lang/Object;"),
            ("java/util/concurrent/RecursiveAction", "invoke", "()Ljava/lang/Object;"),
            ("java/util/concurrent/RecursiveAction", "isDone", "()Z"),
            ("java/util/concurrent/RecursiveAction", "join", "()Ljava/lang/Object;"),
            ("java/util/concurrent/RecursiveTask", "cancel", "(Z)Z"),
            ("java/util/concurrent/RecursiveTask", "complete", "(Ljava/lang/Object;)V"),
            ("java/util/concurrent/RecursiveTask", "completeExceptionally", "(Ljava/lang/Throwable;)V"),
            ("java/util/concurrent/RecursiveTask", "fork", "()Ljava/util/concurrent/ForkJoinTask;"),
            ("java/util/concurrent/RecursiveTask", "get", "()Ljava/lang/Object;"),
            ("java/util/concurrent/RecursiveTask", "getRawResult", "()Ljava/lang/Object;"),
            ("java/util/concurrent/RecursiveTask", "invoke", "()Ljava/lang/Object;"),
            ("java/util/concurrent/RecursiveTask", "isDone", "()Z"),
            ("java/util/concurrent/RecursiveTask", "join", "()Ljava/lang/Object;"),
            ("java/util/concurrent/RecursiveTask", "setRawResult", "(Ljava/lang/Object;)V"),
        ],
    ),
    (
        "register_formatter_natives",
        &[
            ("java/util/Formatter", "<init>", "()V"),
            ("java/util/Formatter", "<init>", "(Ljava/lang/Appendable;)V"),
            ("java/util/Formatter", "<init>", "(Ljava/util/Locale;)V"),
            ("java/util/Formatter", "close", "()V"),
            ("java/util/Formatter", "flush", "()V"),
            ("java/util/Formatter", "format", "(Ljava/lang/String;[Ljava/lang/Object;)Ljava/util/Formatter;"),
            ("java/util/Formatter", "out", "()Ljava/lang/Appendable;"),
            ("java/util/Formatter", "toString", "()Ljava/lang/String;"),
        ],
    ),
    (
        "register_http_client",
        &[
            ("java/net/http/HttpClient", "followRedirects", "()Ljava/net/http/HttpClient$Redirect;"),
            ("java/net/http/HttpClient", "newBuilder", "()Ljava/net/http/HttpClient$Builder;"),
            ("java/net/http/HttpClient", "newHttpClient", "()Ljava/net/http/HttpClient;"),
            ("java/net/http/HttpClient", "send", "(Ljava/net/http/HttpRequest;Ljava/net/http/HttpResponse$BodyHandler;)Ljava/net/http/HttpResponse;"),
            ("java/net/http/HttpClient", "sendAsync", "(Ljava/net/http/HttpRequest;Ljava/net/http/HttpResponse$BodyHandler;)Ljava/util/concurrent/CompletableFuture;"),
            ("java/net/http/HttpClient", "sendAsync", "(Ljava/net/http/HttpRequest;Ljava/net/http/HttpResponse$BodyHandler;Ljava/net/http/HttpResponse$PushPromiseHandler;)Ljava/util/concurrent/CompletableFuture;"),
            ("java/net/http/HttpClient", "sslContext", "()Ljavax/net/ssl/SSLContext;"),
            ("java/net/http/HttpClient", "sslParameters", "()Ljavax/net/ssl/SSLParameters;"),
            ("java/net/http/HttpClient", "version", "()Ljava/net/http/HttpClient$Version;"),
        ],
    ),
    (
        "register_http_client_builder",
        &[
            ("java/net/http/HttpClient$Builder", "build", "()Ljava/net/http/HttpClient;"),
            ("java/net/http/HttpClient$Builder", "connectTimeout", "(Ljava/time/Duration;)Ljava/net/http/HttpClient$Builder;"),
            ("java/net/http/HttpClient$Builder", "followRedirects", "(Ljava/net/http/HttpClient$Redirect;)Ljava/net/http/HttpClient$Builder;"),
        ],
    ),
    (
        "register_http_headers",
        &[
            ("java/net/http/HttpHeaders", "allValues", "(Ljava/lang/String;)Ljava/util/List;"),
            ("java/net/http/HttpHeaders", "firstValue", "(Ljava/lang/String;)Ljava/util/Optional;"),
            ("java/net/http/HttpHeaders", "firstValueAsLong", "(Ljava/lang/String;)Ljava/util/OptionalLong;"),
            ("java/net/http/HttpHeaders", "map", "()Ljava/util/Map;"),
        ],
    ),
    (
        "register_http_request",
        &[
            ("java/net/http/HttpRequest", "bodyPublisher", "()Ljava/util/Optional;"),
            ("java/net/http/HttpRequest", "expectContinue", "()Z"),
            ("java/net/http/HttpRequest", "headers", "()Ljava/net/http/HttpHeaders;"),
            ("java/net/http/HttpRequest", "method", "()Ljava/lang/String;"),
            ("java/net/http/HttpRequest", "newBuilder", "()Ljava/net/http/HttpRequest$Builder;"),
            ("java/net/http/HttpRequest", "newBuilder", "(Ljava/net/URI;)Ljava/net/http/HttpRequest$Builder;"),
            ("java/net/http/HttpRequest", "timeout", "()Ljava/util/Optional;"),
            ("java/net/http/HttpRequest", "uri", "()Ljava/net/URI;"),
            ("java/net/http/HttpRequest", "version", "()Ljava/util/Optional;"),
        ],
    ),
    (
        "register_http_request_builder",
        &[
            ("java/net/http/HttpRequest$Builder", "DELETE", "()Ljava/net/http/HttpRequest$Builder;"),
            ("java/net/http/HttpRequest$Builder", "GET", "()Ljava/net/http/HttpRequest$Builder;"),
            ("java/net/http/HttpRequest$Builder", "POST", "(Ljava/net/http/HttpRequest$BodyPublisher;)Ljava/net/http/HttpRequest$Builder;"),
            ("java/net/http/HttpRequest$Builder", "PUT", "(Ljava/net/http/HttpRequest$BodyPublisher;)Ljava/net/http/HttpRequest$Builder;"),
            ("java/net/http/HttpRequest$Builder", "build", "()Ljava/net/http/HttpRequest;"),
            ("java/net/http/HttpRequest$Builder", "expectContinue", "(Z)Ljava/net/http/HttpRequest$Builder;"),
            ("java/net/http/HttpRequest$Builder", "header", "(Ljava/lang/String;Ljava/lang/String;)Ljava/net/http/HttpRequest$Builder;"),
            ("java/net/http/HttpRequest$Builder", "headers", "([Ljava/lang/String;)Ljava/net/http/HttpRequest$Builder;"),
            ("java/net/http/HttpRequest$Builder", "method", "(Ljava/lang/String;Ljava/net/http/HttpRequest$BodyPublisher;)Ljava/net/http/HttpRequest$Builder;"),
            ("java/net/http/HttpRequest$Builder", "setHeader", "(Ljava/lang/String;Ljava/lang/String;)Ljava/net/http/HttpRequest$Builder;"),
            ("java/net/http/HttpRequest$Builder", "timeout", "(Ljava/time/Duration;)Ljava/net/http/HttpRequest$Builder;"),
            ("java/net/http/HttpRequest$Builder", "uri", "(Ljava/net/URI;)Ljava/net/http/HttpRequest$Builder;"),
            ("java/net/http/HttpRequest$Builder", "version", "(Ljava/net/http/HttpClient$Version;)Ljava/net/http/HttpRequest$Builder;"),
        ],
    ),
    (
        "register_http_response",
        &[
            ("java/net/http/HttpResponse", "body", "()Ljava/lang/Object;"),
            ("java/net/http/HttpResponse", "headers", "()Ljava/net/http/HttpHeaders;"),
            ("java/net/http/HttpResponse", "statusCode", "()I"),
        ],
    ),
    (
        "register_java_lang_extras_natives",
        &[
            ("java/lang/Byte", "doubleValue", "()D"),
            ("java/lang/Byte", "floatValue", "()F"),
            ("java/lang/Byte", "intValue", "()I"),
            ("java/lang/Byte", "longValue", "()J"),
            ("java/lang/ClassLoader", "getSystemClassLoader", "()Ljava/lang/ClassLoader;"),
            ("java/lang/ClassLoader", "loadClass", "(Ljava/lang/String;)Ljava/lang/Class;"),
            ("java/lang/Double", "doubleValue", "()D"),
            ("java/lang/Double", "floatValue", "()F"),
            ("java/lang/Double", "intValue", "()I"),
            ("java/lang/Double", "longValue", "()J"),
            ("java/lang/Float", "doubleValue", "()D"),
            ("java/lang/Float", "floatValue", "()F"),
            ("java/lang/Float", "intValue", "()I"),
            ("java/lang/Float", "longValue", "()J"),
            ("java/lang/Integer", "doubleValue", "()D"),
            ("java/lang/Integer", "floatValue", "()F"),
            ("java/lang/Integer", "intValue", "()I"),
            ("java/lang/Integer", "longValue", "()J"),
            ("java/lang/Long", "doubleValue", "()D"),
            ("java/lang/Long", "floatValue", "()F"),
            ("java/lang/Long", "intValue", "()I"),
            ("java/lang/Long", "longValue", "()J"),
            ("java/lang/Short", "doubleValue", "()D"),
            ("java/lang/Short", "floatValue", "()F"),
            ("java/lang/Short", "intValue", "()I"),
            ("java/lang/Short", "longValue", "()J"),
            ("java/lang/System", "gc", "()V"),
            ("java/lang/Thread", "holdsLock", "(Ljava/lang/Object;)Z"),
        ],
    ),
    (
        "register_key_manager_factory",
        &[
            ("javax/net/ssl/KeyManagerFactory", "getDefaultAlgorithm", "()Ljava/lang/String;"),
            ("javax/net/ssl/KeyManagerFactory", "getInstance", "(Ljava/lang/String;)Ljavax/net/ssl/KeyManagerFactory;"),
            ("javax/net/ssl/KeyManagerFactory", "getKeyManagers", "()[Ljavax/net/ssl/KeyManager;"),
            ("javax/net/ssl/KeyManagerFactory", "init", "(Ljava/security/KeyStore;[C)V"),
        ],
    ),
    (
        "register_logging_natives",
        &[
            ("java/util/logging/Level", "<clinit>", "()V"),
            ("java/util/logging/Level", "<init>", "(Ljava/lang/String;I)V"),
            ("java/util/logging/Level", "<init>", "(Ljava/lang/String;ILjava/lang/String;)V"),
            ("java/util/logging/Level", "getName", "()Ljava/lang/String;"),
            ("java/util/logging/Level", "intValue", "()I"),
            ("java/util/logging/Level", "toString", "()Ljava/lang/String;"),
            ("java/util/logging/Logger", "addHandler", "(Ljava/util/logging/Handler;)V"),
            ("java/util/logging/Logger", "config", "(Ljava/lang/String;)V"),
            ("java/util/logging/Logger", "fine", "(Ljava/lang/String;)V"),
            ("java/util/logging/Logger", "finer", "(Ljava/lang/String;)V"),
            ("java/util/logging/Logger", "finest", "(Ljava/lang/String;)V"),
            ("java/util/logging/Logger", "getLevel", "()Ljava/util/logging/Level;"),
            ("java/util/logging/Logger", "getLogger", "(Ljava/lang/String;)Ljava/util/logging/Logger;"),
            ("java/util/logging/Logger", "getName", "()Ljava/lang/String;"),
            ("java/util/logging/Logger", "info", "(Ljava/lang/String;)V"),
            ("java/util/logging/Logger", "isLoggable", "(Ljava/util/logging/Level;)Z"),
            ("java/util/logging/Logger", "log", "(Ljava/util/logging/Level;Ljava/lang/String;)V"),
            ("java/util/logging/Logger", "setLevel", "(Ljava/util/logging/Level;)V"),
            ("java/util/logging/Logger", "severe", "(Ljava/lang/String;)V"),
            ("java/util/logging/Logger", "warning", "(Ljava/lang/String;)V"),
        ],
    ),
    (
        "register_m18_concurrent_fixes",
        &[
            ("java/util/concurrent/ArrayBlockingQueue", "<init>", "(I)V"),
            ("java/util/concurrent/ArrayBlockingQueue", "<init>", "(IZ)V"),
            ("java/util/concurrent/ArrayBlockingQueue", "add", "(Ljava/lang/Object;)Z"),
            ("java/util/concurrent/ArrayBlockingQueue", "clear", "()V"),
            ("java/util/concurrent/ArrayBlockingQueue", "contains", "(Ljava/lang/Object;)Z"),
            ("java/util/concurrent/ArrayBlockingQueue", "isEmpty", "()Z"),
            ("java/util/concurrent/ArrayBlockingQueue", "offer", "(Ljava/lang/Object;)Z"),
            ("java/util/concurrent/ArrayBlockingQueue", "peek", "()Ljava/lang/Object;"),
            ("java/util/concurrent/ArrayBlockingQueue", "poll", "()Ljava/lang/Object;"),
            ("java/util/concurrent/ArrayBlockingQueue", "put", "(Ljava/lang/Object;)V"),
            ("java/util/concurrent/ArrayBlockingQueue", "remainingCapacity", "()I"),
            ("java/util/concurrent/ArrayBlockingQueue", "size", "()I"),
            ("java/util/concurrent/ArrayBlockingQueue", "take", "()Ljava/lang/Object;"),
            ("java/util/concurrent/ArrayBlockingQueue", "toArray", "()[Ljava/lang/Object;"),
            ("java/util/concurrent/ConcurrentHashMap", "compute", "(Ljava/lang/Object;Ljava/util/function/BiFunction;)Ljava/lang/Object;"),
            ("java/util/concurrent/ConcurrentHashMap", "computeIfAbsent", "(Ljava/lang/Object;Ljava/util/function/Function;)Ljava/lang/Object;"),
            ("java/util/concurrent/ConcurrentHashMap", "forEach", "(Ljava/util/function/BiConsumer;)V"),
            ("java/util/concurrent/ConcurrentHashMap", "getOrDefault", "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;"),
            ("java/util/concurrent/ConcurrentHashMap", "merge", "(Ljava/lang/Object;Ljava/lang/Object;Ljava/util/function/BiFunction;)Ljava/lang/Object;"),
            ("java/util/concurrent/ConcurrentHashMap", "putIfAbsent", "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;"),
            ("java/util/concurrent/LinkedBlockingQueue", "<init>", "()V"),
            ("java/util/concurrent/LinkedBlockingQueue", "<init>", "(I)V"),
            ("java/util/concurrent/LinkedBlockingQueue", "add", "(Ljava/lang/Object;)Z"),
            ("java/util/concurrent/LinkedBlockingQueue", "clear", "()V"),
            ("java/util/concurrent/LinkedBlockingQueue", "contains", "(Ljava/lang/Object;)Z"),
            ("java/util/concurrent/LinkedBlockingQueue", "isEmpty", "()Z"),
            ("java/util/concurrent/LinkedBlockingQueue", "iterator", "()Ljava/util/Iterator;"),
            ("java/util/concurrent/LinkedBlockingQueue", "offer", "(Ljava/lang/Object;)Z"),
            ("java/util/concurrent/LinkedBlockingQueue", "peek", "()Ljava/lang/Object;"),
            ("java/util/concurrent/LinkedBlockingQueue", "poll", "()Ljava/lang/Object;"),
            ("java/util/concurrent/LinkedBlockingQueue", "put", "(Ljava/lang/Object;)V"),
            ("java/util/concurrent/LinkedBlockingQueue", "remainingCapacity", "()I"),
            ("java/util/concurrent/LinkedBlockingQueue", "size", "()I"),
            ("java/util/concurrent/LinkedBlockingQueue", "take", "()Ljava/lang/Object;"),
            ("java/util/concurrent/LinkedBlockingQueue", "toArray", "()[Ljava/lang/Object;"),
        ],
    ),
    (
        "register_object_input_stream",
        &[
            ("java/io/ObjectInputStream", "resolveProxyClass", "([Ljava/lang/String;)Ljava/lang/Class;"),
        ],
    ),
    (
        "register_object_stream_class",
        &[
            ("java/io/ObjectStreamClass", "hasStaticInitializer", "(Ljava/lang/Class;)Z"),
            ("java/io/ObjectStreamClass", "hasStaticInitializer", "(Ljava/lang/Class;Z)Z"),
            ("java/io/ObjectStreamClass", "initNative", "()V"),
        ],
    ),
    (
        "register_p58_completable_future",
        &[
            ("java/util/concurrent/CompletableFuture", "allOf", "([Ljava/util/concurrent/CompletableFuture;)Ljava/util/concurrent/CompletableFuture;"),
            ("java/util/concurrent/CompletableFuture", "anyOf", "([Ljava/util/concurrent/CompletableFuture;)Ljava/util/concurrent/CompletableFuture;"),
            ("java/util/concurrent/CompletableFuture", "complete", "(Ljava/lang/Object;)Z"),
            ("java/util/concurrent/CompletableFuture", "completeExceptionally", "(Ljava/lang/Throwable;)Z"),
            ("java/util/concurrent/CompletableFuture", "exceptionally", "(Ljava/util/function/Function;)Ljava/util/concurrent/CompletableFuture;"),
            ("java/util/concurrent/CompletableFuture", "handle", "(Ljava/util/function/BiFunction;)Ljava/util/concurrent/CompletableFuture;"),
            ("java/util/concurrent/CompletableFuture", "isCompletedExceptionally", "()Z"),
            ("java/util/concurrent/CompletableFuture", "thenAccept", "(Ljava/util/function/Consumer;)Ljava/util/concurrent/CompletableFuture;"),
            ("java/util/concurrent/CompletableFuture", "thenApply", "(Ljava/util/function/Function;)Ljava/util/concurrent/CompletableFuture;"),
            ("java/util/concurrent/CompletableFuture", "thenCombine", "(Ljava/util/concurrent/CompletionStage;Ljava/util/function/BiFunction;)Ljava/util/concurrent/CompletableFuture;"),
            ("java/util/concurrent/CompletableFuture", "thenCompose", "(Ljava/util/function/Function;)Ljava/util/concurrent/CompletableFuture;"),
            ("java/util/concurrent/CompletableFuture", "thenRun", "(Ljava/lang/Runnable;)Ljava/util/concurrent/CompletableFuture;"),
            ("java/util/concurrent/CompletableFuture", "whenComplete", "(Ljava/util/function/BiConsumer;)Ljava/util/concurrent/CompletableFuture;"),
            ("java/util/concurrent/CompletionStage", "thenAccept", "(Ljava/util/function/Consumer;)Ljava/util/concurrent/CompletionStage;"),
            ("java/util/concurrent/CompletionStage", "thenApply", "(Ljava/util/function/Function;)Ljava/util/concurrent/CompletionStage;"),
            ("java/util/concurrent/CompletionStage", "thenCompose", "(Ljava/util/function/Function;)Ljava/util/concurrent/CompletionStage;"),
        ],
    ),
    (
        "register_p58_nio_channels",
        &[
            ("java/nio/channels/SelectableChannel", "register", "(Ljava/nio/channels/Selector;I)Ljava/nio/channels/SelectionKey;"),
            ("java/nio/channels/Selector", "isOpen", "()Z"),
            ("java/nio/channels/Selector", "open", "()Ljava/nio/channels/Selector;"),
            ("java/nio/channels/Selector", "select", "(J)I"),
        ],
    ),
    (
        "register_p59_management",
        &[
            ("java/lang/management/ClassLoadingMXBean", "getLoadedClassCount", "()I"),
            ("java/lang/management/ClassLoadingMXBean", "getTotalLoadedClassCount", "()J"),
            ("java/lang/management/ClassLoadingMXBean", "getUnloadedClassCount", "()J"),
            ("java/lang/management/CompilationMXBean", "getName", "()Ljava/lang/String;"),
            ("java/lang/management/CompilationMXBean", "getTotalCompilationTime", "()J"),
            ("java/lang/management/CompilationMXBean", "isCompilationTimeMonitoringSupported", "()Z"),
            ("java/lang/management/ManagementFactory", "getClassLoadingMXBean", "()Ljava/lang/management/ClassLoadingMXBean;"),
            ("java/lang/management/ManagementFactory", "getCompilationMXBean", "()Ljava/lang/management/CompilationMXBean;"),
            ("java/lang/management/ManagementFactory", "getMemoryMXBean", "()Ljava/lang/management/MemoryMXBean;"),
            ("java/lang/management/ManagementFactory", "getOperatingSystemMXBean", "()Ljava/lang/management/OperatingSystemMXBean;"),
            ("java/lang/management/ManagementFactory", "getRuntimeMXBean", "()Ljava/lang/management/RuntimeMXBean;"),
            ("java/lang/management/ManagementFactory", "getThreadMXBean", "()Ljava/lang/management/ThreadMXBean;"),
            ("java/lang/management/MemoryMXBean", "getHeapMemoryUsage", "()Ljava/lang/management/MemoryUsage;"),
            ("java/lang/management/MemoryMXBean", "getNonHeapMemoryUsage", "()Ljava/lang/management/MemoryUsage;"),
            ("java/lang/management/MemoryMXBean", "getObjectPendingFinalizationCount", "()I"),
            ("java/lang/management/MemoryMXBean", "isVerbose", "()Z"),
            ("java/lang/management/MemoryUsage", "getCommitted", "()J"),
            ("java/lang/management/MemoryUsage", "getInit", "()J"),
            ("java/lang/management/MemoryUsage", "getMax", "()J"),
            ("java/lang/management/MemoryUsage", "getUsed", "()J"),
            ("java/lang/management/OperatingSystemMXBean", "getArch", "()Ljava/lang/String;"),
            ("java/lang/management/OperatingSystemMXBean", "getAvailableProcessors", "()I"),
            ("java/lang/management/OperatingSystemMXBean", "getName", "()Ljava/lang/String;"),
            ("java/lang/management/OperatingSystemMXBean", "getSystemLoadAverage", "()D"),
            ("java/lang/management/OperatingSystemMXBean", "getVersion", "()Ljava/lang/String;"),
            ("java/lang/management/RuntimeMXBean", "getName", "()Ljava/lang/String;"),
            ("java/lang/management/RuntimeMXBean", "getSpecVersion", "()Ljava/lang/String;"),
            ("java/lang/management/RuntimeMXBean", "getStartTime", "()J"),
            ("java/lang/management/RuntimeMXBean", "getUptime", "()J"),
            ("java/lang/management/RuntimeMXBean", "getVmName", "()Ljava/lang/String;"),
            ("java/lang/management/RuntimeMXBean", "getVmVersion", "()Ljava/lang/String;"),
        ],
    ),
    (
        "register_p59_module",
        &[
            ("java/lang/Class", "getModule", "()Ljava/lang/Module;"),
            ("java/lang/Module", "addExports", "(Ljava/lang/String;Ljava/lang/Module;)Ljava/lang/Module;"),
            ("java/lang/Module", "addOpens", "(Ljava/lang/String;Ljava/lang/Module;)Ljava/lang/Module;"),
            ("java/lang/Module", "canRead", "(Ljava/lang/Module;)Z"),
            ("java/lang/Module", "getDescriptor", "()Ljava/lang/module/ModuleDescriptor;"),
            ("java/lang/Module", "getLayer", "()Ljava/lang/ModuleLayer;"),
            ("java/lang/Module", "getName", "()Ljava/lang/String;"),
            ("java/lang/Module", "getPackages", "()Ljava/util/Set;"),
            ("java/lang/Module", "isExported", "(Ljava/lang/String;)Z"),
            ("java/lang/Module", "isExported", "(Ljava/lang/String;Ljava/lang/Module;)Z"),
            ("java/lang/Module", "isOpen", "(Ljava/lang/String;)Z"),
            ("java/lang/Module", "isOpen", "(Ljava/lang/String;Ljava/lang/Module;)Z"),
            ("java/lang/ModuleLayer", "boot", "()Ljava/lang/ModuleLayer;"),
            ("java/lang/ModuleLayer", "findModule", "(Ljava/lang/String;)Ljava/util/Optional;"),
            ("java/lang/ModuleLayer", "modules", "()Ljava/util/Set;"),
            ("java/lang/module/ModuleDescriptor", "isAutomatic", "()Z"),
            ("java/lang/module/ModuleDescriptor", "isOpen", "()Z"),
            ("java/lang/module/ModuleDescriptor", "name", "()Ljava/lang/String;"),
        ],
    ),
    (
        "register_p59_package",
        &[
            ("java/lang/Package", "getPackages", "()[Ljava/lang/Package;"),
        ],
    ),
    (
        "register_p59_spliterator",
        &[
            ("java/util/stream/DoubleStream", "spliterator", "()Ljava/util/Spliterator$OfDouble;"),
            ("java/util/stream/IntStream", "spliterator", "()Ljava/util/Spliterator$OfInt;"),
            ("java/util/stream/LongStream", "spliterator", "()Ljava/util/Spliterator$OfLong;"),
            ("java/util/stream/StreamSupport", "stream", "(Ljava/util/Spliterator;Z)Ljava/util/stream/Stream;"),
        ],
    ),
    (
        "register_p59_varhandle",
        &[
            ("java/lang/invoke/MethodHandles", "byteArrayViewVarHandle", "(Ljava/lang/Class;Ljava/nio/ByteOrder;)Ljava/lang/invoke/VarHandle;"),
            ("java/lang/invoke/MethodHandles$Lookup", "findStaticVarHandle", "(Ljava/lang/Class;Ljava/lang/String;Ljava/lang/Class;)Ljava/lang/invoke/VarHandle;"),
            ("java/lang/invoke/MethodHandles$Lookup", "findVarHandle", "(Ljava/lang/Class;Ljava/lang/String;Ljava/lang/Class;)Ljava/lang/invoke/VarHandle;"),
        ],
    ),
    (
        "register_p60_flow",
        &[
            ("java/util/concurrent/Flow$Subscription", "cancel", "()V"),
            ("java/util/concurrent/Flow$Subscription", "request", "(J)V"),
        ],
    ),
    (
        "register_p60_http_client",
        &[
            ("java/net/http/HttpClient", "newBuilder", "()Ljava/net/http/HttpClient$Builder;"),
            ("java/net/http/HttpClient", "newHttpClient", "()Ljava/net/http/HttpClient;"),
            ("java/net/http/HttpClient", "send", "(Ljava/net/http/HttpRequest;Ljava/net/http/HttpResponse$BodyHandler;)Ljava/net/http/HttpResponse;"),
            ("java/net/http/HttpClient", "sendAsync", "(Ljava/net/http/HttpRequest;Ljava/net/http/HttpResponse$BodyHandler;)Ljava/util/concurrent/CompletableFuture;"),
            ("java/net/http/HttpClient", "version", "()Ljava/net/http/HttpClient$Version;"),
            ("java/net/http/HttpClient$Builder", "build", "()Ljava/net/http/HttpClient;"),
            ("java/net/http/HttpClient$Builder", "connectTimeout", "(Ljava/time/Duration;)Ljava/net/http/HttpClient$Builder;"),
            ("java/net/http/HttpClient$Builder", "followRedirects", "(Ljava/net/http/HttpClient$Redirect;)Ljava/net/http/HttpClient$Builder;"),
            ("java/net/http/HttpRequest", "method", "()Ljava/lang/String;"),
            ("java/net/http/HttpRequest", "newBuilder", "()Ljava/net/http/HttpRequest$Builder;"),
            ("java/net/http/HttpRequest", "newBuilder", "(Ljava/net/URI;)Ljava/net/http/HttpRequest$Builder;"),
            ("java/net/http/HttpRequest", "uri", "()Ljava/net/URI;"),
            ("java/net/http/HttpRequest$BodyPublishers", "noBody", "()Ljava/net/http/HttpRequest$BodyPublisher;"),
            ("java/net/http/HttpRequest$BodyPublishers", "ofString", "(Ljava/lang/String;)Ljava/net/http/HttpRequest$BodyPublisher;"),
            ("java/net/http/HttpRequest$Builder", "DELETE", "()Ljava/net/http/HttpRequest$Builder;"),
            ("java/net/http/HttpRequest$Builder", "GET", "()Ljava/net/http/HttpRequest$Builder;"),
            ("java/net/http/HttpRequest$Builder", "POST", "(Ljava/net/http/HttpRequest$BodyPublisher;)Ljava/net/http/HttpRequest$Builder;"),
            ("java/net/http/HttpRequest$Builder", "PUT", "(Ljava/net/http/HttpRequest$BodyPublisher;)Ljava/net/http/HttpRequest$Builder;"),
            ("java/net/http/HttpRequest$Builder", "build", "()Ljava/net/http/HttpRequest;"),
            ("java/net/http/HttpRequest$Builder", "header", "(Ljava/lang/String;Ljava/lang/String;)Ljava/net/http/HttpRequest$Builder;"),
            ("java/net/http/HttpRequest$Builder", "timeout", "(Ljava/time/Duration;)Ljava/net/http/HttpRequest$Builder;"),
            ("java/net/http/HttpRequest$Builder", "uri", "(Ljava/net/URI;)Ljava/net/http/HttpRequest$Builder;"),
            ("java/net/http/HttpResponse", "body", "()Ljava/lang/Object;"),
            ("java/net/http/HttpResponse", "statusCode", "()I"),
            ("java/net/http/HttpResponse$BodyHandlers", "discarding", "()Ljava/net/http/HttpResponse$BodyHandler;"),
            ("java/net/http/HttpResponse$BodyHandlers", "ofString", "()Ljava/net/http/HttpResponse$BodyHandler;"),
        ],
    ),
    (
        "register_p61_charset",
        &[
            ("java/nio/charset/Charset", "aliases", "()Ljava/util/Set;"),
            ("java/nio/charset/Charset", "contains", "(Ljava/nio/charset/Charset;)Z"),
            ("java/nio/charset/Charset", "displayName", "()Ljava/lang/String;"),
            ("java/nio/charset/Charset", "name", "()Ljava/lang/String;"),
            ("java/nio/charset/Charset", "toString", "()Ljava/lang/String;"),
        ],
    ),
    (
        "register_p61_classloader",
        &[
            ("java/lang/ClassLoader", "findLoadedClass", "(Ljava/lang/String;)Ljava/lang/Class;"),
            ("java/lang/ClassLoader", "getName", "()Ljava/lang/String;"),
            ("java/lang/ClassLoader", "getPlatformClassLoader", "()Ljava/lang/ClassLoader;"),
            ("java/lang/ClassLoader", "getResource", "(Ljava/lang/String;)Ljava/net/URL;"),
            ("java/lang/ClassLoader", "getResourceAsStream", "(Ljava/lang/String;)Ljava/io/InputStream;"),
            ("java/lang/ClassLoader", "getResources", "(Ljava/lang/String;)Ljava/util/Enumeration;"),
            ("java/lang/ClassLoader", "loadClass", "(Ljava/lang/String;)Ljava/lang/Class;"),
            ("java/lang/ClassLoader", "loadClass", "(Ljava/lang/String;Z)Ljava/lang/Class;"),
        ],
    ),
    (
        "register_p61_files_path",
        &[
            ("java/nio/file/Files", "exists", "(Ljava/nio/file/Path;[Ljava/nio/file/LinkOption;)Z"),
            ("java/nio/file/Files", "isDirectory", "(Ljava/nio/file/Path;[Ljava/nio/file/LinkOption;)Z"),
            ("java/nio/file/Files", "isReadable", "(Ljava/nio/file/Path;)Z"),
            ("java/nio/file/Files", "isRegularFile", "(Ljava/nio/file/Path;[Ljava/nio/file/LinkOption;)Z"),
            ("java/nio/file/Files", "isWritable", "(Ljava/nio/file/Path;)Z"),
            ("java/nio/file/Files", "notExists", "(Ljava/nio/file/Path;[Ljava/nio/file/LinkOption;)Z"),
            ("java/nio/file/Files", "size", "(Ljava/nio/file/Path;)J"),
        ],
    ),
    (
        "register_p61_logging",
        &[
            ("java/util/logging/LogManager", "getLogManager", "()Ljava/util/logging/LogManager;"),
            ("java/util/logging/LogManager", "getLoggerNames", "()Ljava/util/Enumeration;"),
            ("java/util/logging/LogManager", "reset", "()V"),
            ("java/util/logging/Logger", "addHandler", "(Ljava/util/logging/Handler;)V"),
            ("java/util/logging/Logger", "getHandlers", "()[Ljava/util/logging/Handler;"),
            ("java/util/logging/Logger", "getParent", "()Ljava/util/logging/Logger;"),
            ("java/util/logging/Logger", "getUseParentHandlers", "()Z"),
            ("java/util/logging/Logger", "setParent", "(Ljava/util/logging/Logger;)V"),
            ("java/util/logging/Logger", "setUseParentHandlers", "(Z)V"),
        ],
    ),
    (
        "register_p61_net",
        &[
            ("java/net/NetworkInterface", "getHardwareAddress", "()[B"),
            ("java/net/NetworkInterface", "getMTU", "()I"),
        ],
    ),
    (
        "register_p61_reflect",
        &[
            ("java/lang/reflect/Field", "getAnnotation", "(Ljava/lang/Class;)Ljava/lang/annotation/Annotation;"),
            ("java/lang/reflect/Field", "getAnnotations", "()[Ljava/lang/annotation/Annotation;"),
            ("java/lang/reflect/Field", "isAnnotationPresent", "(Ljava/lang/Class;)Z"),
            ("java/lang/reflect/Method", "getAnnotation", "(Ljava/lang/Class;)Ljava/lang/annotation/Annotation;"),
            ("java/lang/reflect/Method", "getAnnotations", "()[Ljava/lang/annotation/Annotation;"),
            ("java/lang/reflect/Method", "getParameterAnnotations", "()[[Ljava/lang/annotation/Annotation;"),
            ("java/lang/reflect/Method", "isAnnotationPresent", "(Ljava/lang/Class;)Z"),
        ],
    ),
    (
        "register_p61_text_formatting",
        &[
            ("java/text/Normalizer", "isNormalized", "(Ljava/lang/CharSequence;Ljava/text/Normalizer$Form;)Z"),
            ("java/text/Normalizer", "normalize", "(Ljava/lang/CharSequence;Ljava/text/Normalizer$Form;)Ljava/lang/String;"),
        ],
    ),
    (
        "register_p62_abstract_map_entries",
        &[
            ("java/util/AbstractMap$SimpleEntry", "getKey", "()Ljava/lang/Object;"),
            ("java/util/AbstractMap$SimpleEntry", "getValue", "()Ljava/lang/Object;"),
            ("java/util/AbstractMap$SimpleEntry", "setValue", "(Ljava/lang/Object;)Ljava/lang/Object;"),
        ],
    ),
    (
        "register_p63_enumeration",
        &[
            ("java/util/Collections", "emptyEnumeration", "()Ljava/util/Enumeration;"),
            ("java/util/Collections$EmptyEnumeration", "hasMoreElements", "()Z"),
            ("java/util/Collections$EmptyEnumeration", "nextElement", "()Ljava/lang/Object;"),
            ("java/util/Enumeration", "hasMoreElements", "()Z"),
            ("java/util/Enumeration", "nextElement", "()Ljava/lang/Object;"),
        ],
    ),
    (
        "register_p63_resource_bundle",
        &[
            ("java/util/ResourceBundle", "containsKey", "(Ljava/lang/String;)Z"),
            ("java/util/ResourceBundle", "getBundle", "(Ljava/lang/String;)Ljava/util/ResourceBundle;"),
            ("java/util/ResourceBundle", "getBundle", "(Ljava/lang/String;Ljava/util/Locale;)Ljava/util/ResourceBundle;"),
            ("java/util/ResourceBundle", "getBundle", "(Ljava/lang/String;Ljava/util/Locale;Ljava/lang/ClassLoader;)Ljava/util/ResourceBundle;"),
            ("java/util/ResourceBundle", "getKeys", "()Ljava/util/Enumeration;"),
            ("java/util/ResourceBundle", "getLocale", "()Ljava/util/Locale;"),
            ("java/util/ResourceBundle", "getObject", "(Ljava/lang/String;)Ljava/lang/Object;"),
            ("java/util/ResourceBundle", "getString", "(Ljava/lang/String;)Ljava/lang/String;"),
            ("java/util/ResourceBundle", "keySet", "()Ljava/util/Set;"),
        ],
    ),
    (
        "register_p63_scheduled_executor",
        &[
            ("java/util/concurrent/Executors", "newScheduledThreadPool", "(I)Ljava/util/concurrent/ScheduledExecutorService;"),
            ("java/util/concurrent/Executors", "newSingleThreadScheduledExecutor", "()Ljava/util/concurrent/ScheduledExecutorService;"),
            ("java/util/concurrent/ScheduledExecutorService", "schedule", "(Ljava/lang/Runnable;JLjava/util/concurrent/TimeUnit;)Ljava/util/concurrent/ScheduledFuture;"),
            ("java/util/concurrent/ScheduledExecutorService", "shutdown", "()V"),
            ("java/util/concurrent/ScheduledFuture", "cancel", "(Z)Z"),
            ("java/util/concurrent/ScheduledFuture", "isCancelled", "()Z"),
            ("java/util/concurrent/ScheduledFuture", "isDone", "()Z"),
            ("java/util/concurrent/ScheduledThreadPoolExecutor", "<init>", "(I)V"),
            ("java/util/concurrent/ScheduledThreadPoolExecutor", "<init>", "(ILjava/util/concurrent/ThreadFactory;)V"),
            ("java/util/concurrent/ScheduledThreadPoolExecutor", "getCorePoolSize", "()I"),
            ("java/util/concurrent/ScheduledThreadPoolExecutor", "getPoolSize", "()I"),
            ("java/util/concurrent/ScheduledThreadPoolExecutor", "isShutdown", "()Z"),
            ("java/util/concurrent/ScheduledThreadPoolExecutor", "isTerminated", "()Z"),
            ("java/util/concurrent/ScheduledThreadPoolExecutor", "schedule", "(Ljava/lang/Runnable;JLjava/util/concurrent/TimeUnit;)Ljava/util/concurrent/ScheduledFuture;"),
            ("java/util/concurrent/ScheduledThreadPoolExecutor", "scheduleAtFixedRate", "(Ljava/lang/Runnable;JJLjava/util/concurrent/TimeUnit;)Ljava/util/concurrent/ScheduledFuture;"),
            ("java/util/concurrent/ScheduledThreadPoolExecutor", "scheduleWithFixedDelay", "(Ljava/lang/Runnable;JJLjava/util/concurrent/TimeUnit;)Ljava/util/concurrent/ScheduledFuture;"),
            ("java/util/concurrent/ScheduledThreadPoolExecutor", "shutdown", "()V"),
            ("java/util/concurrent/ScheduledThreadPoolExecutor", "shutdownNow", "()Ljava/util/List;"),
        ],
    ),
    (
        "register_p63_service_loader",
        &[
            ("java/util/ServiceLoader", "findFirst", "()Ljava/util/Optional;"),
            ("java/util/ServiceLoader", "iterator", "()Ljava/util/Iterator;"),
            ("java/util/ServiceLoader", "load", "(Ljava/lang/Class;)Ljava/util/ServiceLoader;"),
            ("java/util/ServiceLoader", "load", "(Ljava/lang/Class;Ljava/lang/ClassLoader;)Ljava/util/ServiceLoader;"),
            ("java/util/ServiceLoader", "reload", "()V"),
            ("java/util/ServiceLoader", "stream", "()Ljava/util/stream/Stream;"),
        ],
    ),
    (
        "register_p64_collectors_teeing",
        &[
            ("java/util/stream/Collectors", "teeing", "(Ljava/util/stream/Collector;Ljava/util/stream/Collector;Ljava/util/function/BiFunction;)Ljava/util/stream/Collector;"),
        ],
    ),
    (
        "register_p64_sequenced_collections",
        &[
            ("java/util/LinkedHashMap", "reversed", "()Ljava/util/SequencedMap;"),
            ("java/util/LinkedList", "getFirst", "()Ljava/lang/Object;"),
            ("java/util/LinkedList", "getLast", "()Ljava/lang/Object;"),
        ],
    ),
    (
        "register_p64_stream_modern",
        &[
            ("java/util/stream/Stream", "concat", "(Ljava/util/stream/Stream;Ljava/util/stream/Stream;)Ljava/util/stream/Stream;"),
            ("java/util/stream/Stream", "toList", "()Ljava/util/List;"),
        ],
    ),
    (
        "register_p65_checked_collections",
        &[
            ("java/util/Collections", "checkedList", "(Ljava/util/List;Ljava/lang/Class;)Ljava/util/List;"),
            ("java/util/Collections", "checkedMap", "(Ljava/util/Map;Ljava/lang/Class;Ljava/lang/Class;)Ljava/util/Map;"),
            ("java/util/Collections", "checkedSet", "(Ljava/util/Set;Ljava/lang/Class;)Ljava/util/Set;"),
        ],
    ),
    (
        "register_p65_priority_blocking_queue",
        &[
            ("java/util/concurrent/PriorityBlockingQueue", "<init>", "()V"),
            ("java/util/concurrent/PriorityBlockingQueue", "isEmpty", "()Z"),
            ("java/util/concurrent/PriorityBlockingQueue", "offer", "(Ljava/lang/Object;)Z"),
            ("java/util/concurrent/PriorityBlockingQueue", "peek", "()Ljava/lang/Object;"),
            ("java/util/concurrent/PriorityBlockingQueue", "poll", "()Ljava/lang/Object;"),
            ("java/util/concurrent/PriorityBlockingQueue", "put", "(Ljava/lang/Object;)V"),
            ("java/util/concurrent/PriorityBlockingQueue", "size", "()I"),
            ("java/util/concurrent/PriorityBlockingQueue", "take", "()Ljava/lang/Object;"),
        ],
    ),
    (
        "register_p67_misc",
        &[
            ("java/lang/StackWalker", "forEach", "(Ljava/util/function/Consumer;)V"),
            ("java/lang/StackWalker", "getCallerClass", "()Ljava/lang/Class;"),
            ("java/lang/StackWalker", "getInstance", "()Ljava/lang/StackWalker;"),
            ("java/lang/StackWalker", "getInstance", "(Ljava/lang/StackWalker$Option;)Ljava/lang/StackWalker;"),
            ("java/lang/StackWalker", "walk", "(Ljava/util/function/Function;)Ljava/lang/Object;"),
            ("java/lang/System", "getLogger", "(Ljava/lang/String;)Ljava/lang/System$Logger;"),
            ("java/lang/System", "getLogger", "(Ljava/lang/String;Ljava/util/ResourceBundle;)Ljava/lang/System$Logger;"),
            ("java/lang/reflect/GenericArrayType", "getGenericComponentType", "()Ljava/lang/reflect/Type;"),
            ("java/lang/reflect/ParameterizedType", "getActualTypeArguments", "()[Ljava/lang/reflect/Type;"),
            ("java/lang/reflect/ParameterizedType", "getOwnerType", "()Ljava/lang/reflect/Type;"),
            ("java/lang/reflect/ParameterizedType", "getRawType", "()Ljava/lang/reflect/Type;"),
            ("java/lang/reflect/TypeVariable", "getBounds", "()[Ljava/lang/reflect/Type;"),
            ("java/lang/reflect/TypeVariable", "getGenericDeclaration", "()Ljava/lang/reflect/GenericDeclaration;"),
            ("java/lang/reflect/TypeVariable", "getName", "()Ljava/lang/String;"),
            ("java/lang/reflect/WildcardType", "getLowerBounds", "()[Ljava/lang/reflect/Type;"),
            ("java/lang/reflect/WildcardType", "getUpperBounds", "()[Ljava/lang/reflect/Type;"),
        ],
    ),
    (
        "register_p69_misc",
        &[
            ("java/util/List", "copyOf", "(Ljava/util/Collection;)Ljava/util/List;"),
            ("java/util/Map", "copyOf", "(Ljava/util/Map;)Ljava/util/Map;"),
            ("java/util/Set", "copyOf", "(Ljava/util/Collection;)Ljava/util/Set;"),
        ],
    ),
    (
        "register_p69_spliterator",
        &[
            ("cratonvm/internal/StreamCollector", "accept", "(Ljava/lang/Object;)V"),
            ("java/util/Spliterator", "characteristics", "()I"),
            ("java/util/Spliterator", "estimateSize", "()J"),
            ("java/util/Spliterator", "forEachRemaining", "(Ljava/util/function/Consumer;)V"),
            ("java/util/Spliterator", "tryAdvance", "(Ljava/util/function/Consumer;)Z"),
            ("java/util/Spliterator", "trySplit", "()Ljava/util/Spliterator;"),
            ("java/util/Spliterators", "emptySpliterator", "()Ljava/util/Spliterator;"),
            ("java/util/stream/StreamSupport", "stream", "(Ljava/util/Spliterator;Z)Ljava/util/stream/Stream;"),
        ],
    ),
    (
        "register_p70_atomic_accumulators",
        &[
            ("java/util/concurrent/atomic/DoubleAdder", "<init>", "()V"),
            ("java/util/concurrent/atomic/DoubleAdder", "add", "(D)V"),
            ("java/util/concurrent/atomic/DoubleAdder", "doubleValue", "()D"),
            ("java/util/concurrent/atomic/DoubleAdder", "reset", "()V"),
            ("java/util/concurrent/atomic/DoubleAdder", "sum", "()D"),
        ],
    ),
    (
        "register_p70_misc",
        &[
            ("java/util/EnumSet", "complementOf", "(Ljava/util/EnumSet;)Ljava/util/EnumSet;"),
            ("java/util/EnumSet", "range", "(Ljava/lang/Enum;Ljava/lang/Enum;)Ljava/util/EnumSet;"),
        ],
    ),
    (
        "register_p71_files_bridge",
        &[
            ("java/nio/file/Files", "getOwner", "(Ljava/nio/file/Path;[Ljava/nio/file/LinkOption;)Ljava/nio/file/attribute/UserPrincipal;"),
            ("java/nio/file/Files", "newInputStream", "(Ljava/nio/file/Path;[Ljava/nio/file/OpenOption;)Ljava/io/InputStream;"),
            ("java/nio/file/Files", "readAllBytes", "(Ljava/nio/file/Path;)[B"),
            ("java/nio/file/Files", "readSymbolicLink", "(Ljava/nio/file/Path;)Ljava/nio/file/Path;"),
            ("java/nio/file/Files", "write", "(Ljava/nio/file/Path;[B[Ljava/nio/file/OpenOption;)Ljava/nio/file/Path;"),
        ],
    ),
    (
        "register_p71_logging_extras",
        &[
            ("java/util/logging/LogRecord", "<init>", "(Ljava/util/logging/Level;Ljava/lang/String;)V"),
            ("java/util/logging/LogRecord", "getLevel", "()Ljava/util/logging/Level;"),
            ("java/util/logging/LogRecord", "getLoggerName", "()Ljava/lang/String;"),
            ("java/util/logging/LogRecord", "getMessage", "()Ljava/lang/String;"),
            ("java/util/logging/LogRecord", "getMillis", "()J"),
            ("java/util/logging/LogRecord", "getParameters", "()[Ljava/lang/Object;"),
            ("java/util/logging/LogRecord", "getSequenceNumber", "()J"),
            ("java/util/logging/LogRecord", "getThrown", "()Ljava/lang/Throwable;"),
            ("java/util/logging/LogRecord", "setLoggerName", "(Ljava/lang/String;)V"),
            ("java/util/logging/LogRecord", "setMessage", "(Ljava/lang/String;)V"),
            ("java/util/logging/LogRecord", "setMillis", "(J)V"),
            ("java/util/logging/LogRecord", "setParameters", "([Ljava/lang/Object;)V"),
            ("java/util/logging/LogRecord", "setSequenceNumber", "(J)V"),
            ("java/util/logging/LogRecord", "setThrown", "(Ljava/lang/Throwable;)V"),
            ("java/util/logging/Logger", "entering", "(Ljava/lang/String;Ljava/lang/String;)V"),
            ("java/util/logging/Logger", "exiting", "(Ljava/lang/String;Ljava/lang/String;)V"),
            ("java/util/logging/Logger", "throwing", "(Ljava/lang/String;Ljava/lang/String;Ljava/lang/Throwable;)V"),
        ],
    ),
    (
        "register_p72_datagram",
        &[
            ("java/net/MulticastSocket", "<init>", "()V"),
            ("java/net/MulticastSocket", "<init>", "(I)V"),
            ("java/net/MulticastSocket", "close", "()V"),
            ("java/net/MulticastSocket", "getTimeToLive", "()I"),
            ("java/net/MulticastSocket", "receive", "(Ljava/net/DatagramPacket;)V"),
            ("java/net/MulticastSocket", "send", "(Ljava/net/DatagramPacket;)V"),
            ("java/net/MulticastSocket", "setSoTimeout", "(I)V"),
            ("java/net/MulticastSocket", "setTimeToLive", "(I)V"),
        ],
    ),
    (
        "register_p72_naming",
        &[
            ("javax/naming/InitialContext", "<init>", "()V"),
            ("javax/naming/InitialContext", "bind", "(Ljava/lang/String;Ljava/lang/Object;)V"),
            ("javax/naming/InitialContext", "close", "()V"),
            ("javax/naming/InitialContext", "createSubcontext", "(Ljava/lang/String;)Ljavax/naming/Context;"),
            ("javax/naming/InitialContext", "destroySubcontext", "(Ljava/lang/String;)V"),
            ("javax/naming/InitialContext", "getEnvironment", "()Ljava/util/Hashtable;"),
            ("javax/naming/InitialContext", "lookup", "(Ljava/lang/String;)Ljava/lang/Object;"),
            ("javax/naming/InitialContext", "rebind", "(Ljava/lang/String;Ljava/lang/Object;)V"),
            ("javax/naming/InitialContext", "unbind", "(Ljava/lang/String;)V"),
        ],
    ),
    (
        "register_p72_server_socket",
        &[
            ("java/net/ServerSocket", "<init>", "(IILjava/net/InetAddress;)V"),
            ("java/net/ServerSocket", "accept", "()Ljava/net/Socket;"),
            ("java/net/ServerSocket", "bind", "(Ljava/net/SocketAddress;)V"),
            ("java/net/ServerSocket", "bind", "(Ljava/net/SocketAddress;I)V"),
            ("java/net/ServerSocket", "getInetAddress", "()Ljava/net/InetAddress;"),
            ("java/net/ServerSocket", "getLocalPort", "()I"),
            ("java/net/ServerSocket", "getLocalSocketAddress", "()Ljava/net/SocketAddress;"),
            ("java/net/ServerSocket", "isBound", "()Z"),
        ],
    ),
    (
        "register_pe_arena",
        &[
            ("java/lang/foreign/Arena", "allocate", "(J)Ljava/lang/foreign/MemorySegment;"),
            ("java/lang/foreign/Arena", "allocate", "(JJ)Ljava/lang/foreign/MemorySegment;"),
            ("java/lang/foreign/Arena", "close", "()V"),
            ("java/lang/foreign/Arena", "global", "()Ljava/lang/foreign/Arena;"),
            ("java/lang/foreign/Arena", "ofAuto", "()Ljava/lang/foreign/Arena;"),
            ("java/lang/foreign/Arena", "ofConfined", "()Ljava/lang/foreign/Arena;"),
            ("java/lang/foreign/Arena", "ofShared", "()Ljava/lang/foreign/Arena;"),
        ],
    ),
    (
        "register_pe_function_descriptor",
        &[
            ("java/lang/foreign/FunctionDescriptor", "of", "(Ljava/lang/foreign/MemoryLayout;[Ljava/lang/foreign/MemoryLayout;)Ljava/lang/foreign/FunctionDescriptor;"),
            ("java/lang/foreign/FunctionDescriptor", "ofVoid", "([Ljava/lang/foreign/MemoryLayout;)Ljava/lang/foreign/FunctionDescriptor;"),
            ("java/lang/foreign/FunctionDescriptor", "returnLayout", "()Ljava/util/Optional;"),
        ],
    ),
    (
        "register_pe_linker",
        &[
            ("java/lang/foreign/Linker", "downcallHandle", "(Ljava/lang/foreign/MemorySegment;Ljava/lang/foreign/FunctionDescriptor;[Ljava/lang/foreign/Linker$Option;)Ljava/lang/invoke/MethodHandle;"),
            ("java/lang/foreign/Linker", "nativeLinker", "()Ljava/lang/foreign/Linker;"),
        ],
    ),
    (
        "register_phase52_byte_order",
        &[
            ("java/nio/ByteOrder", "BIG_ENDIAN", "Ljava/nio/ByteOrder;"),
            ("java/nio/ByteOrder", "LITTLE_ENDIAN", "Ljava/nio/ByteOrder;"),
            ("java/nio/ByteOrder", "equals", "(Ljava/lang/Object;)Z"),
            ("java/nio/ByteOrder", "nativeOrder", "()Ljava/nio/ByteOrder;"),
            ("java/nio/ByteOrder", "toString", "()Ljava/lang/String;"),
        ],
    ),
    (
        "register_phase52_function_extras",
        &[
            ("java/util/function/BinaryOperator", "apply", "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;"),
            ("java/util/function/DoubleBinaryOperator", "applyAsDouble", "(DD)D"),
            ("java/util/function/DoubleFunction", "apply", "(D)Ljava/lang/Object;"),
            ("java/util/function/DoubleToIntFunction", "applyAsInt", "(D)I"),
            ("java/util/function/DoubleToLongFunction", "applyAsLong", "(D)J"),
            ("java/util/function/IntBinaryOperator", "applyAsInt", "(II)I"),
            ("java/util/function/IntFunction", "apply", "(I)Ljava/lang/Object;"),
            ("java/util/function/IntToDoubleFunction", "applyAsDouble", "(I)D"),
            ("java/util/function/IntToLongFunction", "applyAsLong", "(I)J"),
            ("java/util/function/LongBinaryOperator", "applyAsLong", "(JJ)J"),
            ("java/util/function/LongFunction", "apply", "(J)Ljava/lang/Object;"),
            ("java/util/function/LongToDoubleFunction", "applyAsDouble", "(J)D"),
            ("java/util/function/LongToIntFunction", "applyAsInt", "(J)I"),
            ("java/util/function/ObjDoubleConsumer", "accept", "(Ljava/lang/Object;D)V"),
            ("java/util/function/ObjIntConsumer", "accept", "(Ljava/lang/Object;I)V"),
            ("java/util/function/ObjLongConsumer", "accept", "(Ljava/lang/Object;J)V"),
            ("java/util/function/ToDoubleFunction", "applyAsDouble", "(Ljava/lang/Object;)D"),
            ("java/util/function/ToIntFunction", "applyAsInt", "(Ljava/lang/Object;)I"),
            ("java/util/function/ToLongFunction", "applyAsLong", "(Ljava/lang/Object;)J"),
            ("java/util/function/UnaryOperator", "apply", "(Ljava/lang/Object;)Ljava/lang/Object;"),
        ],
    ),
    (
        "register_phase52_objects_extras",
        &[
            ("java/util/Objects", "isNull", "(Ljava/lang/Object;)Z"),
            ("java/util/Objects", "nonNull", "(Ljava/lang/Object;)Z"),
            ("java/util/Objects", "requireNonNullElse", "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;"),
            ("java/util/Objects", "requireNonNullElseGet", "(Ljava/lang/Object;Ljava/util/function/Supplier;)Ljava/lang/Object;"),
        ],
    ),
    (
        "register_phase52_url_encoding",
        &[
            ("java/net/URLDecoder", "decode", "(Ljava/lang/String;)Ljava/lang/String;"),
            ("java/net/URLDecoder", "decode", "(Ljava/lang/String;Ljava/lang/String;)Ljava/lang/String;"),
            ("java/net/URLDecoder", "decode", "(Ljava/lang/String;Ljava/nio/charset/Charset;)Ljava/lang/String;"),
            ("java/net/URLEncoder", "encode", "(Ljava/lang/String;)Ljava/lang/String;"),
            ("java/net/URLEncoder", "encode", "(Ljava/lang/String;Ljava/lang/String;)Ljava/lang/String;"),
            ("java/net/URLEncoder", "encode", "(Ljava/lang/String;Ljava/nio/charset/Charset;)Ljava/lang/String;"),
        ],
    ),
    (
        "register_phase53_crypto",
        &[
            ("javax/crypto/Cipher", "doFinal", "()[B"),
            ("javax/crypto/Cipher", "doFinal", "([B)[B"),
            ("javax/crypto/Cipher", "getAlgorithm", "()Ljava/lang/String;"),
            ("javax/crypto/Cipher", "getInstance", "(Ljava/lang/String;)Ljavax/crypto/Cipher;"),
            ("javax/crypto/Cipher", "getInstance", "(Ljava/lang/String;Ljava/lang/String;)Ljavax/crypto/Cipher;"),
            ("javax/crypto/Cipher", "getOutputSize", "(I)I"),
            ("javax/crypto/Cipher", "init", "(ILjava/security/Key;)V"),
            ("javax/crypto/Cipher", "init", "(ILjava/security/Key;Ljava/security/spec/AlgorithmParameterSpec;)V"),
            ("javax/crypto/Cipher", "init", "(ILjava/security/Key;Ljava/security/spec/AlgorithmParameterSpec;Ljava/security/SecureRandom;)V"),
            ("javax/crypto/Cipher", "update", "([B)[B"),
            ("javax/crypto/Cipher", "updateAAD", "([B)V"),
            ("javax/crypto/SecretKeyFactory", "generateSecret", "(Ljava/security/spec/KeySpec;)Ljavax/crypto/SecretKey;"),
            ("javax/crypto/SecretKeyFactory", "getAlgorithm", "()Ljava/lang/String;"),
            ("javax/crypto/SecretKeyFactory", "getInstance", "(Ljava/lang/String;)Ljavax/crypto/SecretKeyFactory;"),
            ("javax/crypto/SecretKeyFactory", "getInstance", "(Ljava/lang/String;Ljava/lang/String;)Ljavax/crypto/SecretKeyFactory;"),
            ("javax/crypto/SecretKeyFactory", "getProvider", "()Ljava/security/Provider;"),
        ],
    ),
    (
        "register_phase53_security",
        &[
            ("java/security/KeyPair", "getPrivate", "()Ljava/security/PrivateKey;"),
            ("java/security/KeyPair", "getPublic", "()Ljava/security/PublicKey;"),
            ("java/security/Provider", "getInfo", "()Ljava/lang/String;"),
            ("java/security/Provider", "getName", "()Ljava/lang/String;"),
            ("java/security/Provider", "getVersion", "()D"),
            ("java/security/Provider", "toString", "()Ljava/lang/String;"),
            ("java/security/Security", "addProvider", "(Ljava/security/Provider;)I"),
            ("java/security/Security", "getAlgorithms", "(Ljava/lang/String;)Ljava/util/Set;"),
            ("java/security/Security", "getProperty", "(Ljava/lang/String;)Ljava/lang/String;"),
            ("java/security/Security", "getProvider", "(Ljava/lang/String;)Ljava/security/Provider;"),
            ("java/security/Security", "getProviders", "()[Ljava/security/Provider;"),
            ("java/security/Security", "insertProviderAt", "(Ljava/security/Provider;I)I"),
            ("java/security/Security", "removeProvider", "(Ljava/lang/String;)V"),
            ("java/security/Security", "setProperty", "(Ljava/lang/String;Ljava/lang/String;)V"),
            ("java/security/Signature", "getAlgorithm", "()Ljava/lang/String;"),
            ("java/security/Signature", "getInstance", "(Ljava/lang/String;)Ljava/security/Signature;"),
            ("java/security/Signature", "getInstance", "(Ljava/lang/String;Ljava/lang/String;)Ljava/security/Signature;"),
            ("java/security/Signature", "initSign", "(Ljava/security/PrivateKey;)V"),
            ("java/security/Signature", "initSign", "(Ljava/security/PrivateKey;Ljava/security/SecureRandom;)V"),
            ("java/security/Signature", "initVerify", "(Ljava/security/PublicKey;)V"),
            ("java/security/Signature", "sign", "()[B"),
            ("java/security/Signature", "sign", "([BII)I"),
            ("java/security/Signature", "update", "(B)V"),
            ("java/security/Signature", "update", "([B)V"),
            ("java/security/Signature", "update", "([BII)V"),
            ("java/security/Signature", "verify", "([B)Z"),
            ("java/security/Signature", "verify", "([BII)Z"),
            ("java/util/IteratorEnumeration", "hasMoreElements", "()Z"),
            ("java/util/IteratorEnumeration", "nextElement", "()Ljava/lang/Object;"),
        ],
    ),
    (
        "register_phase54_atomics",
        &[
            ("java/util/concurrent/atomic/AtomicBoolean", "<init>", "()V"),
            ("java/util/concurrent/atomic/AtomicBoolean", "<init>", "(Z)V"),
            ("java/util/concurrent/atomic/AtomicBoolean", "compareAndSet", "(ZZ)Z"),
            ("java/util/concurrent/atomic/AtomicBoolean", "get", "()Z"),
            ("java/util/concurrent/atomic/AtomicBoolean", "getAndSet", "(Z)Z"),
            ("java/util/concurrent/atomic/AtomicBoolean", "lazySet", "(Z)V"),
            ("java/util/concurrent/atomic/AtomicBoolean", "set", "(Z)V"),
            ("java/util/concurrent/atomic/AtomicBoolean", "toString", "()Ljava/lang/String;"),
            ("java/util/concurrent/atomic/AtomicInteger", "<init>", "()V"),
            ("java/util/concurrent/atomic/AtomicInteger", "<init>", "(I)V"),
            ("java/util/concurrent/atomic/AtomicInteger", "addAndGet", "(I)I"),
            ("java/util/concurrent/atomic/AtomicInteger", "compareAndSet", "(II)Z"),
            ("java/util/concurrent/atomic/AtomicInteger", "decrementAndGet", "()I"),
            ("java/util/concurrent/atomic/AtomicInteger", "get", "()I"),
            ("java/util/concurrent/atomic/AtomicInteger", "getAndAdd", "(I)I"),
            ("java/util/concurrent/atomic/AtomicInteger", "getAndDecrement", "()I"),
            ("java/util/concurrent/atomic/AtomicInteger", "getAndIncrement", "()I"),
            ("java/util/concurrent/atomic/AtomicInteger", "getAndSet", "(I)I"),
            ("java/util/concurrent/atomic/AtomicInteger", "incrementAndGet", "()I"),
            ("java/util/concurrent/atomic/AtomicInteger", "intValue", "()I"),
            ("java/util/concurrent/atomic/AtomicInteger", "lazySet", "(I)V"),
            ("java/util/concurrent/atomic/AtomicInteger", "longValue", "()J"),
            ("java/util/concurrent/atomic/AtomicInteger", "set", "(I)V"),
            ("java/util/concurrent/atomic/AtomicInteger", "toString", "()Ljava/lang/String;"),
            ("java/util/concurrent/atomic/AtomicInteger", "weakCompareAndSet", "(II)Z"),
            ("java/util/concurrent/atomic/AtomicIntegerArray", "<init>", "(I)V"),
            ("java/util/concurrent/atomic/AtomicIntegerArray", "compareAndSet", "(III)Z"),
            ("java/util/concurrent/atomic/AtomicIntegerArray", "get", "(I)I"),
            ("java/util/concurrent/atomic/AtomicIntegerArray", "getAndAdd", "(II)I"),
            ("java/util/concurrent/atomic/AtomicIntegerArray", "getAndSet", "(II)I"),
            ("java/util/concurrent/atomic/AtomicIntegerArray", "incrementAndGet", "(I)I"),
            ("java/util/concurrent/atomic/AtomicIntegerArray", "length", "()I"),
            ("java/util/concurrent/atomic/AtomicIntegerArray", "set", "(II)V"),
            ("java/util/concurrent/atomic/AtomicLong", "<init>", "()V"),
            ("java/util/concurrent/atomic/AtomicLong", "<init>", "(J)V"),
            ("java/util/concurrent/atomic/AtomicLong", "VMSupportsCS8", "()Z"),
            ("java/util/concurrent/atomic/AtomicLong", "addAndGet", "(J)J"),
            ("java/util/concurrent/atomic/AtomicLong", "compareAndSet", "(JJ)Z"),
            ("java/util/concurrent/atomic/AtomicLong", "decrementAndGet", "()J"),
            ("java/util/concurrent/atomic/AtomicLong", "get", "()J"),
            ("java/util/concurrent/atomic/AtomicLong", "getAndAdd", "(J)J"),
            ("java/util/concurrent/atomic/AtomicLong", "getAndDecrement", "()J"),
            ("java/util/concurrent/atomic/AtomicLong", "getAndIncrement", "()J"),
            ("java/util/concurrent/atomic/AtomicLong", "getAndSet", "(J)J"),
            ("java/util/concurrent/atomic/AtomicLong", "incrementAndGet", "()J"),
            ("java/util/concurrent/atomic/AtomicLong", "intValue", "()I"),
            ("java/util/concurrent/atomic/AtomicLong", "lazySet", "(J)V"),
            ("java/util/concurrent/atomic/AtomicLong", "longValue", "()J"),
            ("java/util/concurrent/atomic/AtomicLong", "set", "(J)V"),
            ("java/util/concurrent/atomic/AtomicLong", "weakCompareAndSet", "(JJ)Z"),
            ("java/util/concurrent/atomic/AtomicLongArray", "<init>", "(I)V"),
            ("java/util/concurrent/atomic/AtomicLongArray", "get", "(I)J"),
            ("java/util/concurrent/atomic/AtomicLongArray", "getAndAdd", "(IJ)J"),
            ("java/util/concurrent/atomic/AtomicLongArray", "incrementAndGet", "(I)J"),
            ("java/util/concurrent/atomic/AtomicLongArray", "length", "()I"),
            ("java/util/concurrent/atomic/AtomicLongArray", "set", "(IJ)V"),
            ("java/util/concurrent/atomic/AtomicReference", "<init>", "()V"),
            ("java/util/concurrent/atomic/AtomicReference", "<init>", "(Ljava/lang/Object;)V"),
            ("java/util/concurrent/atomic/AtomicReference", "get", "()Ljava/lang/Object;"),
            ("java/util/concurrent/atomic/AtomicReference", "getAndSet", "(Ljava/lang/Object;)Ljava/lang/Object;"),
            ("java/util/concurrent/atomic/AtomicReference", "lazySet", "(Ljava/lang/Object;)V"),
            ("java/util/concurrent/atomic/AtomicReference", "set", "(Ljava/lang/Object;)V"),
            ("java/util/concurrent/atomic/AtomicReference", "toString", "()Ljava/lang/String;"),
            ("java/util/concurrent/atomic/DoubleAdder", "<init>", "()V"),
            ("java/util/concurrent/atomic/DoubleAdder", "add", "(D)V"),
            ("java/util/concurrent/atomic/DoubleAdder", "doubleValue", "()D"),
            ("java/util/concurrent/atomic/DoubleAdder", "reset", "()V"),
            ("java/util/concurrent/atomic/DoubleAdder", "sum", "()D"),
            ("java/util/concurrent/atomic/LongAdder", "<init>", "()V"),
            ("java/util/concurrent/atomic/LongAdder", "add", "(J)V"),
            ("java/util/concurrent/atomic/LongAdder", "decrement", "()V"),
            ("java/util/concurrent/atomic/LongAdder", "increment", "()V"),
            ("java/util/concurrent/atomic/LongAdder", "intValue", "()I"),
            ("java/util/concurrent/atomic/LongAdder", "longValue", "()J"),
            ("java/util/concurrent/atomic/LongAdder", "reset", "()V"),
            ("java/util/concurrent/atomic/LongAdder", "sum", "()J"),
            ("java/util/concurrent/atomic/LongAdder", "sumThenReset", "()J"),
            ("java/util/concurrent/atomic/LongAdder", "toString", "()Ljava/lang/String;"),
        ],
    ),
    (
        "register_phase54_net_extras",
        &[
            ("java/net/HttpURLConnection", "connect", "()V"),
            ("java/net/HttpURLConnection", "disconnect", "()V"),
            ("java/net/HttpURLConnection", "getContentLength", "()I"),
            ("java/net/HttpURLConnection", "getContentLengthLong", "()J"),
            ("java/net/HttpURLConnection", "getErrorStream", "()Ljava/io/InputStream;"),
            ("java/net/HttpURLConnection", "getHeaderField", "(Ljava/lang/String;)Ljava/lang/String;"),
            ("java/net/HttpURLConnection", "getInputStream", "()Ljava/io/InputStream;"),
            ("java/net/HttpURLConnection", "getInstanceFollowRedirects", "()Z"),
            ("java/net/HttpURLConnection", "getOutputStream", "()Ljava/io/OutputStream;"),
            ("java/net/HttpURLConnection", "getResponseMessage", "()Ljava/lang/String;"),
            ("java/net/HttpURLConnection", "setChunkedStreamingMode", "(I)V"),
            ("java/net/HttpURLConnection", "setConnectTimeout", "(I)V"),
            ("java/net/HttpURLConnection", "setDoInput", "(Z)V"),
            ("java/net/HttpURLConnection", "setDoOutput", "(Z)V"),
            ("java/net/HttpURLConnection", "setFixedLengthStreamingMode", "(I)V"),
            ("java/net/HttpURLConnection", "setFixedLengthStreamingMode", "(J)V"),
            ("java/net/HttpURLConnection", "setInstanceFollowRedirects", "(Z)V"),
            ("java/net/HttpURLConnection", "setReadTimeout", "(I)V"),
            ("java/net/InetAddress", "getHostAddress", "()Ljava/lang/String;"),
            ("java/net/InetAddress", "getHostName", "()Ljava/lang/String;"),
            ("java/net/InetAddress", "toString", "()Ljava/lang/String;"),
            ("java/net/URI", "<init>", "(Ljava/lang/String;)V"),
            ("java/net/URI", "create", "(Ljava/lang/String;)Ljava/net/URI;"),
            ("java/net/URI", "equals", "(Ljava/lang/Object;)Z"),
            ("java/net/URI", "getFragment", "()Ljava/lang/String;"),
            ("java/net/URI", "getHost", "()Ljava/lang/String;"),
            ("java/net/URI", "getPath", "()Ljava/lang/String;"),
            ("java/net/URI", "getPort", "()I"),
            ("java/net/URI", "getQuery", "()Ljava/lang/String;"),
            ("java/net/URI", "getScheme", "()Ljava/lang/String;"),
            ("java/net/URI", "hashCode", "()I"),
            ("java/net/URI", "toString", "()Ljava/lang/String;"),
        ],
    ),
    (
        "register_phase55_charset",
        &[
            ("java/nio/charset/Charset", "defaultCharset", "()Ljava/nio/charset/Charset;"),
            ("java/nio/charset/Charset", "displayName", "()Ljava/lang/String;"),
            ("java/nio/charset/Charset", "equals", "(Ljava/lang/Object;)Z"),
            ("java/nio/charset/Charset", "forName", "(Ljava/lang/String;)Ljava/nio/charset/Charset;"),
            ("java/nio/charset/Charset", "hashCode", "()I"),
            ("java/nio/charset/Charset", "isSupported", "(Ljava/lang/String;)Z"),
            ("java/nio/charset/Charset", "name", "()Ljava/lang/String;"),
            ("java/nio/charset/Charset", "toString", "()Ljava/lang/String;"),
        ],
    ),
    (
        "register_phase55_collection_extras",
        &[
            ("java/util/Collections", "checkedList", "(Ljava/util/List;Ljava/lang/Class;)Ljava/util/List;"),
            ("java/util/Collections", "checkedMap", "(Ljava/util/Map;Ljava/lang/Class;Ljava/lang/Class;)Ljava/util/Map;"),
            ("java/util/Collections", "checkedSet", "(Ljava/util/Set;Ljava/lang/Class;)Ljava/util/Set;"),
            ("java/util/Collections", "frequency", "(Ljava/util/Collection;Ljava/lang/Object;)I"),
            ("java/util/Collections", "nCopies", "(ILjava/lang/Object;)Ljava/util/List;"),
            ("java/util/Collections", "singleton", "(Ljava/lang/Object;)Ljava/util/Set;"),
            ("java/util/Collections", "singletonList", "(Ljava/lang/Object;)Ljava/util/List;"),
            ("java/util/Collections", "singletonMap", "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/util/Map;"),
            ("java/util/Collections", "synchronizedCollection", "(Ljava/util/Collection;)Ljava/util/Collection;"),
            ("java/util/Collections", "synchronizedList", "(Ljava/util/List;)Ljava/util/List;"),
            ("java/util/Collections", "synchronizedMap", "(Ljava/util/Map;)Ljava/util/Map;"),
            ("java/util/Collections", "synchronizedSet", "(Ljava/util/Set;)Ljava/util/Set;"),
        ],
    ),
    (
        "register_phase55_executors",
        &[
            ("java/util/concurrent/CompletableFuture", "allOf", "([Ljava/util/concurrent/CompletableFuture;)Ljava/util/concurrent/CompletableFuture;"),
            ("java/util/concurrent/CompletableFuture", "anyOf", "([Ljava/util/concurrent/CompletableFuture;)Ljava/util/concurrent/CompletableFuture;"),
            ("java/util/concurrent/CompletableFuture", "complete", "(Ljava/lang/Object;)Z"),
            ("java/util/concurrent/CompletableFuture", "completeExceptionally", "(Ljava/lang/Throwable;)Z"),
            ("java/util/concurrent/CompletableFuture", "exceptionally", "(Ljava/util/function/Function;)Ljava/util/concurrent/CompletableFuture;"),
            ("java/util/concurrent/CompletableFuture", "handle", "(Ljava/util/function/BiFunction;)Ljava/util/concurrent/CompletableFuture;"),
            ("java/util/concurrent/CompletableFuture", "isCompletedExceptionally", "()Z"),
            ("java/util/concurrent/CompletableFuture", "thenCompose", "(Ljava/util/function/Function;)Ljava/util/concurrent/CompletableFuture;"),
            ("java/util/concurrent/Executors", "newCachedThreadPool", "()Ljava/util/concurrent/ExecutorService;"),
            ("java/util/concurrent/Executors", "newCachedThreadPool", "(Ljava/util/concurrent/ThreadFactory;)Ljava/util/concurrent/ExecutorService;"),
            ("java/util/concurrent/Executors", "newFixedThreadPool", "(I)Ljava/util/concurrent/ExecutorService;"),
            ("java/util/concurrent/Executors", "newScheduledThreadPool", "(I)Ljava/util/concurrent/ScheduledExecutorService;"),
            ("java/util/concurrent/Executors", "newSingleThreadExecutor", "()Ljava/util/concurrent/ExecutorService;"),
        ],
    ),
    (
        "register_phase56_collectors_extras",
        &[
            ("java/util/stream/Collectors", "averagingDouble", "(Ljava/util/function/ToDoubleFunction;)Ljava/util/stream/Collector;"),
            ("java/util/stream/Collectors", "averagingInt", "(Ljava/util/function/ToIntFunction;)Ljava/util/stream/Collector;"),
            ("java/util/stream/Collectors", "averagingLong", "(Ljava/util/function/ToLongFunction;)Ljava/util/stream/Collector;"),
            ("java/util/stream/Collectors", "collectingAndThen", "(Ljava/util/stream/Collector;Ljava/util/function/Function;)Ljava/util/stream/Collector;"),
            ("java/util/stream/Collectors", "filtering", "(Ljava/util/function/Predicate;Ljava/util/stream/Collector;)Ljava/util/stream/Collector;"),
            ("java/util/stream/Collectors", "mapping", "(Ljava/util/function/Function;Ljava/util/stream/Collector;)Ljava/util/stream/Collector;"),
            ("java/util/stream/Collectors", "maxBy", "(Ljava/util/Comparator;)Ljava/util/stream/Collector;"),
            ("java/util/stream/Collectors", "minBy", "(Ljava/util/Comparator;)Ljava/util/stream/Collector;"),
            ("java/util/stream/Collectors", "summarizingDouble", "(Ljava/util/function/ToDoubleFunction;)Ljava/util/stream/Collector;"),
            ("java/util/stream/Collectors", "summarizingInt", "(Ljava/util/function/ToIntFunction;)Ljava/util/stream/Collector;"),
            ("java/util/stream/Collectors", "summarizingLong", "(Ljava/util/function/ToLongFunction;)Ljava/util/stream/Collector;"),
            ("java/util/stream/Collectors", "summingDouble", "(Ljava/util/function/ToDoubleFunction;)Ljava/util/stream/Collector;"),
            ("java/util/stream/Collectors", "summingInt", "(Ljava/util/function/ToIntFunction;)Ljava/util/stream/Collector;"),
            ("java/util/stream/Collectors", "summingLong", "(Ljava/util/function/ToLongFunction;)Ljava/util/stream/Collector;"),
            ("java/util/stream/Collectors", "toUnmodifiableList", "()Ljava/util/stream/Collector;"),
            ("java/util/stream/Collectors", "toUnmodifiableSet", "()Ljava/util/stream/Collector;"),
        ],
    ),
    (
        "register_phase56_stream_extras",
        &[
            ("java/util/stream/BaseStream", "iterator", "()Ljava/util/Iterator;"),
            ("java/util/stream/DoubleStream", "boxed", "()Ljava/util/stream/Stream;"),
            ("java/util/stream/DoubleStream", "mapToObj", "(Ljava/util/function/DoubleFunction;)Ljava/util/stream/Stream;"),
            ("java/util/stream/DoubleStream", "peek", "(Ljava/util/function/DoubleConsumer;)Ljava/util/stream/DoubleStream;"),
            ("java/util/stream/IntStream", "asDoubleStream", "()Ljava/util/stream/DoubleStream;"),
            ("java/util/stream/IntStream", "asLongStream", "()Ljava/util/stream/LongStream;"),
            ("java/util/stream/IntStream", "boxed", "()Ljava/util/stream/Stream;"),
            ("java/util/stream/IntStream", "peek", "(Ljava/util/function/IntConsumer;)Ljava/util/stream/IntStream;"),
            ("java/util/stream/IntStream", "sorted", "()Ljava/util/stream/IntStream;"),
            ("java/util/stream/LongStream", "asDoubleStream", "()Ljava/util/stream/DoubleStream;"),
            ("java/util/stream/LongStream", "boxed", "()Ljava/util/stream/Stream;"),
            ("java/util/stream/LongStream", "mapToObj", "(Ljava/util/function/LongFunction;)Ljava/util/stream/Stream;"),
            ("java/util/stream/LongStream", "peek", "(Ljava/util/function/LongConsumer;)Ljava/util/stream/LongStream;"),
            ("java/util/stream/Stream", "concat", "(Ljava/util/stream/Stream;Ljava/util/stream/Stream;)Ljava/util/stream/Stream;"),
            ("java/util/stream/Stream", "flatMapToDouble", "(Ljava/util/function/Function;)Ljava/util/stream/DoubleStream;"),
            ("java/util/stream/Stream", "flatMapToInt", "(Ljava/util/function/Function;)Ljava/util/stream/IntStream;"),
            ("java/util/stream/Stream", "flatMapToLong", "(Ljava/util/function/Function;)Ljava/util/stream/LongStream;"),
            ("java/util/stream/Stream", "forEachOrdered", "(Ljava/util/function/Consumer;)V"),
            ("java/util/stream/Stream", "isParallel", "()Z"),
            ("java/util/stream/Stream", "iterator", "()Ljava/util/Iterator;"),
            ("java/util/stream/Stream", "mapToDouble", "(Ljava/util/function/ToDoubleFunction;)Ljava/util/stream/DoubleStream;"),
            ("java/util/stream/Stream", "mapToInt", "(Ljava/util/function/ToIntFunction;)Ljava/util/stream/IntStream;"),
            ("java/util/stream/Stream", "mapToLong", "(Ljava/util/function/ToLongFunction;)Ljava/util/stream/LongStream;"),
            ("java/util/stream/Stream", "peek", "(Ljava/util/function/Consumer;)Ljava/util/stream/Stream;"),
        ],
    ),
    (
        "register_phase57_file_channel",
        &[
            ("java/nio/channels/FileChannel", "close", "()V"),
            ("java/nio/channels/FileChannel", "force", "(Z)V"),
            ("java/nio/channels/FileChannel", "isOpen", "()Z"),
            ("java/nio/channels/FileChannel", "position", "()J"),
            ("java/nio/channels/FileChannel", "position", "(J)Ljava/nio/channels/FileChannel;"),
            ("java/nio/channels/FileChannel", "read", "(Ljava/nio/ByteBuffer;)I"),
            ("java/nio/channels/FileChannel", "size", "()J"),
            ("java/nio/channels/FileChannel", "truncate", "(J)Ljava/nio/channels/FileChannel;"),
            ("java/nio/channels/FileChannel", "write", "(Ljava/nio/ByteBuffer;)I"),
        ],
    ),
    (
        "register_phase57_random_access_file",
        &[
            ("java/io/RandomAccessFile", "<init>", "(Ljava/io/File;Ljava/lang/String;)V"),
            ("java/io/RandomAccessFile", "<init>", "(Ljava/lang/String;Ljava/lang/String;)V"),
            ("java/io/RandomAccessFile", "close", "()V"),
            ("java/io/RandomAccessFile", "getFilePointer", "()J"),
            ("java/io/RandomAccessFile", "length", "()J"),
            ("java/io/RandomAccessFile", "read", "()I"),
            ("java/io/RandomAccessFile", "read", "([BII)I"),
            ("java/io/RandomAccessFile", "readFully", "([B)V"),
            ("java/io/RandomAccessFile", "readInt", "()I"),
            ("java/io/RandomAccessFile", "readLine", "()Ljava/lang/String;"),
            ("java/io/RandomAccessFile", "readLong", "()J"),
            ("java/io/RandomAccessFile", "readUTF", "()Ljava/lang/String;"),
            ("java/io/RandomAccessFile", "seek", "(J)V"),
            ("java/io/RandomAccessFile", "write", "(I)V"),
            ("java/io/RandomAccessFile", "write", "([BII)V"),
            ("java/io/RandomAccessFile", "writeInt", "(I)V"),
            ("java/io/RandomAccessFile", "writeLong", "(J)V"),
            ("java/io/RandomAccessFile", "writeUTF", "(Ljava/lang/String;)V"),
        ],
    ),
    (
        "register_s1_classloading",
        &[
            ("java/net/URLClassLoader", "<init>", "([Ljava/net/URL;)V"),
            ("java/net/URLClassLoader", "<init>", "([Ljava/net/URL;Ljava/lang/ClassLoader;)V"),
            ("java/net/URLClassLoader", "<init>", "([Ljava/net/URL;Ljava/lang/ClassLoader;Ljava/net/URLStreamHandlerFactory;)V"),
            ("java/net/URLClassLoader", "addURL", "(Ljava/net/URL;)V"),
            ("java/net/URLClassLoader", "close", "()V"),
            ("java/net/URLClassLoader", "findClass", "(Ljava/lang/String;)Ljava/lang/Class;"),
            ("java/net/URLClassLoader", "getResourceAsStream", "(Ljava/lang/String;)Ljava/io/InputStream;"),
            ("java/net/URLClassLoader", "getURLs", "()[Ljava/net/URL;"),
            ("java/util/ServiceLoader", "findFirst", "()Ljava/util/Optional;"),
            ("java/util/ServiceLoader", "iterator", "()Ljava/util/Iterator;"),
            ("java/util/ServiceLoader", "load", "(Ljava/lang/Class;)Ljava/util/ServiceLoader;"),
            ("java/util/ServiceLoader", "load", "(Ljava/lang/Class;Ljava/lang/ClassLoader;)Ljava/util/ServiceLoader;"),
            ("java/util/ServiceLoader", "loadInstalled", "(Ljava/lang/Class;)Ljava/util/ServiceLoader;"),
            ("java/util/ServiceLoader", "reload", "()V"),
            ("java/util/ServiceLoader", "stream", "()Ljava/util/stream/Stream;"),
            ("java/util/ServiceLoader$Itr", "hasNext", "()Z"),
            ("java/util/ServiceLoader$Itr", "next", "()Ljava/lang/Object;"),
        ],
    ),
    (
        "register_s2_selector",
        &[
            ("java/nio/channels/SelectableChannel", "register", "(Ljava/nio/channels/Selector;I)Ljava/nio/channels/SelectionKey;"),
            ("java/nio/channels/SelectableChannel", "register", "(Ljava/nio/channels/Selector;ILjava/lang/Object;)Ljava/nio/channels/SelectionKey;"),
            ("java/nio/channels/Selector", "isOpen", "()Z"),
            ("java/nio/channels/Selector", "open", "()Ljava/nio/channels/Selector;"),
            ("java/nio/channels/Selector", "select", "(J)I"),
        ],
    ),
    (
        "register_s2_server_socket_channel",
        &[
            ("java/nio/channels/ServerSocketChannel", "register", "(Ljava/nio/channels/Selector;I)Ljava/nio/channels/SelectionKey;"),
            ("java/nio/channels/ServerSocketChannel", "register", "(Ljava/nio/channels/Selector;ILjava/lang/Object;)Ljava/nio/channels/SelectionKey;"),
        ],
    ),
    (
        "register_s2_socket_channel",
        &[
            ("java/nio/channels/SocketChannel", "register", "(Ljava/nio/channels/Selector;I)Ljava/nio/channels/SelectionKey;"),
            ("java/nio/channels/SocketChannel", "register", "(Ljava/nio/channels/Selector;ILjava/lang/Object;)Ljava/nio/channels/SelectionKey;"),
        ],
    ),
    (
        "register_s3_http_client",
        &[
            ("java/net/http/HttpClient", "send", "(Ljava/net/http/HttpRequest;Ljava/net/http/HttpResponse$BodyHandler;)Ljava/net/http/HttpResponse;"),
            ("java/net/http/HttpClient", "sendAsync", "(Ljava/net/http/HttpRequest;Ljava/net/http/HttpResponse$BodyHandler;)Ljava/util/concurrent/CompletableFuture;"),
            ("java/net/http/HttpRequest$Builder", "POST", "(Ljava/net/http/HttpRequest$BodyPublisher;)Ljava/net/http/HttpRequest$Builder;"),
            ("java/net/http/HttpRequest$Builder", "PUT", "(Ljava/net/http/HttpRequest$BodyPublisher;)Ljava/net/http/HttpRequest$Builder;"),
        ],
    ),
    (
        "register_security_natives",
        &[
            ("java/security/MessageDigest", "digest", "()[B"),
            ("java/security/MessageDigest", "digest", "([B)[B"),
            ("java/security/MessageDigest", "digest", "([BII)I"),
            ("java/security/MessageDigest", "getAlgorithm", "()Ljava/lang/String;"),
            ("java/security/MessageDigest", "getDigestLength", "()I"),
            ("java/security/MessageDigest", "getInstance", "(Ljava/lang/String;)Ljava/security/MessageDigest;"),
            ("java/security/MessageDigest", "reset", "()V"),
            ("java/security/MessageDigest", "update", "(B)V"),
            ("java/security/MessageDigest", "update", "([B)V"),
            ("java/security/MessageDigest", "update", "([BII)V"),
        ],
    ),
    (
        "register_slf4j_natives",
        &[
            ("java/util/logging/LogManager", "getLogManager", "()Ljava/util/logging/LogManager;"),
            ("java/util/logging/LogManager", "getLogger", "(Ljava/lang/String;)Ljava/util/logging/Logger;"),
            ("java/util/logging/Logger", "addHandler", "(Ljava/util/logging/Handler;)V"),
            ("java/util/logging/Logger", "config", "(Ljava/lang/String;)V"),
            ("java/util/logging/Logger", "fine", "(Ljava/lang/String;)V"),
            ("java/util/logging/Logger", "finer", "(Ljava/lang/String;)V"),
            ("java/util/logging/Logger", "finest", "(Ljava/lang/String;)V"),
            ("java/util/logging/Logger", "getLevel", "()Ljava/util/logging/Level;"),
            ("java/util/logging/Logger", "getLogger", "(Ljava/lang/String;)Ljava/util/logging/Logger;"),
            ("java/util/logging/Logger", "getName", "()Ljava/lang/String;"),
            ("java/util/logging/Logger", "info", "(Ljava/lang/String;)V"),
            ("java/util/logging/Logger", "isLoggable", "(Ljava/util/logging/Level;)Z"),
            ("java/util/logging/Logger", "log", "(Ljava/util/logging/Level;Ljava/lang/String;)V"),
            ("java/util/logging/Logger", "removeHandler", "(Ljava/util/logging/Handler;)V"),
            ("java/util/logging/Logger", "setLevel", "(Ljava/util/logging/Level;)V"),
            ("java/util/logging/Logger", "severe", "(Ljava/lang/String;)V"),
            ("java/util/logging/Logger", "warning", "(Ljava/lang/String;)V"),
            ("org/slf4j/Logger", "debug", "(Ljava/lang/String;)V"),
            ("org/slf4j/Logger", "debug", "(Ljava/lang/String;Ljava/lang/Object;)V"),
            ("org/slf4j/Logger", "debug", "(Ljava/lang/String;Ljava/lang/Object;Ljava/lang/Object;)V"),
            ("org/slf4j/Logger", "debug", "(Ljava/lang/String;[Ljava/lang/Object;)V"),
            ("org/slf4j/Logger", "error", "(Ljava/lang/String;)V"),
            ("org/slf4j/Logger", "error", "(Ljava/lang/String;Ljava/lang/Object;)V"),
            ("org/slf4j/Logger", "error", "(Ljava/lang/String;Ljava/lang/Object;Ljava/lang/Object;)V"),
            ("org/slf4j/Logger", "error", "(Ljava/lang/String;Ljava/lang/Throwable;)V"),
            ("org/slf4j/Logger", "error", "(Ljava/lang/String;[Ljava/lang/Object;)V"),
            ("org/slf4j/Logger", "getName", "()Ljava/lang/String;"),
            ("org/slf4j/Logger", "info", "(Ljava/lang/String;)V"),
            ("org/slf4j/Logger", "info", "(Ljava/lang/String;Ljava/lang/Object;)V"),
            ("org/slf4j/Logger", "info", "(Ljava/lang/String;Ljava/lang/Object;Ljava/lang/Object;)V"),
            ("org/slf4j/Logger", "info", "(Ljava/lang/String;[Ljava/lang/Object;)V"),
            ("org/slf4j/Logger", "isDebugEnabled", "()Z"),
            ("org/slf4j/Logger", "isErrorEnabled", "()Z"),
            ("org/slf4j/Logger", "isInfoEnabled", "()Z"),
            ("org/slf4j/Logger", "isTraceEnabled", "()Z"),
            ("org/slf4j/Logger", "isWarnEnabled", "()Z"),
            ("org/slf4j/Logger", "trace", "(Ljava/lang/String;)V"),
            ("org/slf4j/Logger", "trace", "(Ljava/lang/String;Ljava/lang/Object;)V"),
            ("org/slf4j/Logger", "trace", "(Ljava/lang/String;Ljava/lang/Object;Ljava/lang/Object;)V"),
            ("org/slf4j/Logger", "trace", "(Ljava/lang/String;[Ljava/lang/Object;)V"),
            ("org/slf4j/Logger", "warn", "(Ljava/lang/String;)V"),
            ("org/slf4j/Logger", "warn", "(Ljava/lang/String;Ljava/lang/Object;)V"),
            ("org/slf4j/Logger", "warn", "(Ljava/lang/String;Ljava/lang/Object;Ljava/lang/Object;)V"),
            ("org/slf4j/Logger", "warn", "(Ljava/lang/String;Ljava/lang/Throwable;)V"),
            ("org/slf4j/Logger", "warn", "(Ljava/lang/String;[Ljava/lang/Object;)V"),
        ],
    ),
    (
        "register_ssl_context",
        &[
            ("javax/net/ssl/SSLContext", "createSSLEngine", "()Ljavax/net/ssl/SSLEngine;"),
            ("javax/net/ssl/SSLContext", "createSSLEngine", "(Ljava/lang/String;I)Ljavax/net/ssl/SSLEngine;"),
            ("javax/net/ssl/SSLContext", "getDefault", "()Ljavax/net/ssl/SSLContext;"),
            ("javax/net/ssl/SSLContext", "getDefaultSSLParameters", "()Ljavax/net/ssl/SSLParameters;"),
            ("javax/net/ssl/SSLContext", "getInstance", "(Ljava/lang/String;)Ljavax/net/ssl/SSLContext;"),
            ("javax/net/ssl/SSLContext", "getProtocol", "()Ljava/lang/String;"),
            ("javax/net/ssl/SSLContext", "getProvider", "()Ljava/security/Provider;"),
            ("javax/net/ssl/SSLContext", "getServerSocketFactory", "()Ljavax/net/ssl/SSLServerSocketFactory;"),
            ("javax/net/ssl/SSLContext", "getSocketFactory", "()Ljavax/net/ssl/SSLSocketFactory;"),
            ("javax/net/ssl/SSLContext", "getSupportedSSLParameters", "()Ljavax/net/ssl/SSLParameters;"),
            ("javax/net/ssl/SSLContext", "init", "([Ljavax/net/ssl/KeyManager;[Ljavax/net/ssl/TrustManager;Ljava/security/SecureRandom;)V"),
        ],
    ),
    (
        "register_ssl_engine",
        &[
            ("javax/net/ssl/SSLEngine", "beginHandshake", "()V"),
            ("javax/net/ssl/SSLEngine", "closeInbound", "()V"),
            ("javax/net/ssl/SSLEngine", "closeOutbound", "()V"),
            ("javax/net/ssl/SSLEngine", "getHandshakeStatus", "()Ljavax/net/ssl/SSLEngineResult$HandshakeStatus;"),
            ("javax/net/ssl/SSLEngine", "getSession", "()Ljavax/net/ssl/SSLSession;"),
            ("javax/net/ssl/SSLEngine", "getUseClientMode", "()Z"),
            ("javax/net/ssl/SSLEngine", "isInboundDone", "()Z"),
            ("javax/net/ssl/SSLEngine", "isOutboundDone", "()Z"),
            ("javax/net/ssl/SSLEngine", "setEnabledCipherSuites", "([Ljava/lang/String;)V"),
            ("javax/net/ssl/SSLEngine", "setEnabledProtocols", "([Ljava/lang/String;)V"),
            ("javax/net/ssl/SSLEngine", "setUseClientMode", "(Z)V"),
        ],
    ),
    (
        "register_ssl_engine_result",
        &[
            ("javax/net/ssl/SSLEngineResult", "bytesConsumed", "()I"),
            ("javax/net/ssl/SSLEngineResult", "bytesProduced", "()I"),
            ("javax/net/ssl/SSLEngineResult", "getHandshakeStatus", "()Ljavax/net/ssl/SSLEngineResult$HandshakeStatus;"),
            ("javax/net/ssl/SSLEngineResult", "getStatus", "()Ljavax/net/ssl/SSLEngineResult$Status;"),
        ],
    ),
    (
        "register_ssl_parameters",
        &[
            ("javax/net/ssl/SSLParameters", "getApplicationProtocols", "()[Ljava/lang/String;"),
            ("javax/net/ssl/SSLParameters", "setApplicationProtocols", "([Ljava/lang/String;)V"),
        ],
    ),
    (
        "register_ssl_session",
        &[
            ("javax/net/ssl/SSLSession", "getApplicationBufferSize", "()I"),
            ("javax/net/ssl/SSLSession", "getCipherSuite", "()Ljava/lang/String;"),
            ("javax/net/ssl/SSLSession", "getCreationTime", "()J"),
            ("javax/net/ssl/SSLSession", "getId", "()[B"),
            ("javax/net/ssl/SSLSession", "getLastAccessedTime", "()J"),
            ("javax/net/ssl/SSLSession", "getPacketBufferSize", "()I"),
            ("javax/net/ssl/SSLSession", "getPeerHost", "()Ljava/lang/String;"),
            ("javax/net/ssl/SSLSession", "getPeerPort", "()I"),
            ("javax/net/ssl/SSLSession", "getProtocol", "()Ljava/lang/String;"),
            ("javax/net/ssl/SSLSession", "invalidate", "()V"),
            ("javax/net/ssl/SSLSession", "isValid", "()Z"),
        ],
    ),
    (
        "register_ssl_socket_factory",
        &[
            ("javax/net/ssl/SSLSocketFactory", "createSocket", "(Ljava/lang/String;I)Ljava/net/Socket;"),
            ("javax/net/ssl/SSLSocketFactory", "createSocket", "(Ljava/net/Socket;Ljava/lang/String;IZ)Ljava/net/Socket;"),
            ("javax/net/ssl/SSLSocketFactory", "getDefaultCipherSuites", "()[Ljava/lang/String;"),
            ("javax/net/ssl/SSLSocketFactory", "getSupportedCipherSuites", "()[Ljava/lang/String;"),
        ],
    ),
    (
        "register_synthetic_overrides",
        &[
            ("java/io/PrintStream", "close", "()V"),
            ("java/io/PrintStream", "flush", "()V"),
            ("java/io/PrintStream", "format", "(Ljava/lang/String;[Ljava/lang/Object;)Ljava/io/PrintStream;"),
            ("java/io/PrintStream", "print", "(C)V"),
            ("java/io/PrintStream", "print", "(D)V"),
            ("java/io/PrintStream", "print", "(F)V"),
            ("java/io/PrintStream", "print", "(I)V"),
            ("java/io/PrintStream", "print", "(J)V"),
            ("java/io/PrintStream", "print", "(Ljava/lang/Object;)V"),
            ("java/io/PrintStream", "print", "(Ljava/lang/String;)V"),
            ("java/io/PrintStream", "print", "(Z)V"),
            ("java/io/PrintStream", "printf", "(Ljava/lang/String;[Ljava/lang/Object;)Ljava/io/PrintStream;"),
            ("java/io/PrintStream", "println", "()V"),
            ("java/io/PrintStream", "println", "(C)V"),
            ("java/io/PrintStream", "println", "(D)V"),
            ("java/io/PrintStream", "println", "(F)V"),
            ("java/io/PrintStream", "println", "(I)V"),
            ("java/io/PrintStream", "println", "(J)V"),
            ("java/io/PrintStream", "println", "(Ljava/lang/Object;)V"),
            ("java/io/PrintStream", "println", "(Ljava/lang/String;)V"),
            ("java/io/PrintStream", "println", "(Z)V"),
            ("java/io/PrintStream", "write", "(I)V"),
            ("java/io/PrintStream", "write", "(Ljava/lang/String;)V"),
            ("java/io/PrintStream", "write", "(Ljava/lang/String;II)V"),
            ("java/io/PrintStream", "write", "([BII)V"),
            ("java/io/PrintWriter", "println", "()V"),
            ("java/io/PrintWriter", "println", "(Ljava/lang/String;)V"),
            ("java/lang/Class", "desiredAssertionStatus0", "(Ljava/lang/Class;)Z"),
            ("java/lang/Class", "forName", "(Ljava/lang/Module;Ljava/lang/String;)Ljava/lang/Class;"),
            ("java/lang/Class", "forName", "(Ljava/lang/String;)Ljava/lang/Class;"),
            ("java/lang/Class", "forName", "(Ljava/lang/String;ZLjava/lang/ClassLoader;)Ljava/lang/Class;"),
            ("java/lang/Class", "getConstructor", "([Ljava/lang/Class;)Ljava/lang/reflect/Constructor;"),
            ("java/lang/Class", "getConstructors", "()[Ljava/lang/reflect/Constructor;"),
            ("java/lang/Class", "getDeclaredConstructor", "([Ljava/lang/Class;)Ljava/lang/reflect/Constructor;"),
            ("java/lang/Class", "getDeclaredConstructors", "()[Ljava/lang/reflect/Constructor;"),
            ("java/lang/Class", "getDeclaredField", "(Ljava/lang/String;)Ljava/lang/reflect/Field;"),
            ("java/lang/Class", "getDeclaredFields", "()[Ljava/lang/reflect/Field;"),
            ("java/lang/Class", "getDeclaredMethod", "(Ljava/lang/String;[Ljava/lang/Class;)Ljava/lang/reflect/Method;"),
            ("java/lang/Class", "getDeclaredMethods", "()[Ljava/lang/reflect/Method;"),
            ("java/lang/Class", "getField", "(Ljava/lang/String;)Ljava/lang/reflect/Field;"),
            ("java/lang/Class", "getFields", "()[Ljava/lang/reflect/Field;"),
            ("java/lang/Class", "getGenericInterfaces", "()[Ljava/lang/reflect/Type;"),
            ("java/lang/Class", "getGenericSuperclass", "()Ljava/lang/reflect/Type;"),
            ("java/lang/Class", "getMethod", "(Ljava/lang/String;[Ljava/lang/Class;)Ljava/lang/reflect/Method;"),
            ("java/lang/Class", "getMethods", "()[Ljava/lang/reflect/Method;"),
            ("java/lang/Class", "getModifiers", "()I"),
            ("java/lang/Class", "getName", "()Ljava/lang/String;"),
            ("java/lang/Class", "getPrimitiveClass", "(Ljava/lang/String;)Ljava/lang/Class;"),
            ("java/lang/Class", "getSimpleName", "()Ljava/lang/String;"),
            ("java/lang/Class", "getSuperclass", "()Ljava/lang/Class;"),
            ("java/lang/Class", "getTypeParameters", "()[Ljava/lang/reflect/TypeVariable;"),
            ("java/lang/Class", "isArray", "()Z"),
            ("java/lang/Class", "isAssignableFrom", "(Ljava/lang/Class;)Z"),
            ("java/lang/Class", "isEnum", "()Z"),
            ("java/lang/Class", "isInstance", "(Ljava/lang/Object;)Z"),
            ("java/lang/Class", "isInterface", "()Z"),
            ("java/lang/Class", "isPrimitive", "()Z"),
            ("java/lang/Class", "newInstance", "()Ljava/lang/Object;"),
            ("java/lang/Class", "registerNatives", "()V"),
            ("java/lang/Double", "doubleToRawLongBits", "(D)J"),
            ("java/lang/Double", "longBitsToDouble", "(J)D"),
            ("java/lang/Float", "floatToRawIntBits", "(F)I"),
            ("java/lang/Object", "clone", "()Ljava/lang/Object;"),
            ("java/lang/Object", "equals", "(Ljava/lang/Object;)Z"),
            ("java/lang/Object", "finalize", "()V"),
            ("java/lang/Object", "getClass", "()Ljava/lang/Class;"),
            ("java/lang/Object", "hashCode", "()I"),
            ("java/lang/Object", "notify", "()V"),
            ("java/lang/Object", "notifyAll", "()V"),
            ("java/lang/Object", "toString", "()Ljava/lang/String;"),
            ("java/lang/Object", "wait", "()V"),
            ("java/lang/Object", "wait", "(J)V"),
            ("java/lang/String", "<init>", "([B)V"),
            ("java/lang/String", "<init>", "([BII)V"),
            ("java/lang/String", "charAt", "(I)C"),
            ("java/lang/String", "chars", "()Ljava/util/stream/IntStream;"),
            ("java/lang/String", "codePoints", "()Ljava/util/stream/IntStream;"),
            ("java/lang/String", "contains", "(Ljava/lang/CharSequence;)Z"),
            ("java/lang/String", "endsWith", "(Ljava/lang/String;)Z"),
            ("java/lang/String", "equals", "(Ljava/lang/Object;)Z"),
            ("java/lang/String", "formatted", "([Ljava/lang/Object;)Ljava/lang/String;"),
            ("java/lang/String", "getBytes", "()[B"),
            ("java/lang/String", "hashCode", "()I"),
            ("java/lang/String", "indent", "(I)Ljava/lang/String;"),
            ("java/lang/String", "indexOf", "(I)I"),
            ("java/lang/String", "indexOf", "(II)I"),
            ("java/lang/String", "indexOf", "(Ljava/lang/String;)I"),
            ("java/lang/String", "intern", "()Ljava/lang/String;"),
            ("java/lang/String", "isBlank", "()Z"),
            ("java/lang/String", "isEmpty", "()Z"),
            ("java/lang/String", "join", "(Ljava/lang/CharSequence;[Ljava/lang/CharSequence;)Ljava/lang/String;"),
            ("java/lang/String", "lastIndexOf", "(I)I"),
            ("java/lang/String", "lastIndexOf", "(II)I"),
            ("java/lang/String", "lastIndexOf", "(Ljava/lang/String;)I"),
            ("java/lang/String", "length", "()I"),
            ("java/lang/String", "lines", "()Ljava/util/stream/Stream;"),
            ("java/lang/String", "matches", "(Ljava/lang/String;)Z"),
            ("java/lang/String", "regionMatches", "(ILjava/lang/String;II)Z"),
            ("java/lang/String", "regionMatches", "(ZILjava/lang/String;II)Z"),
            ("java/lang/String", "repeat", "(I)Ljava/lang/String;"),
            ("java/lang/String", "replace", "(CC)Ljava/lang/String;"),
            ("java/lang/String", "replaceAll", "(Ljava/lang/String;Ljava/lang/String;)Ljava/lang/String;"),
            ("java/lang/String", "replaceFirst", "(Ljava/lang/String;Ljava/lang/String;)Ljava/lang/String;"),
            ("java/lang/String", "startsWith", "(Ljava/lang/String;)Z"),
            ("java/lang/String", "startsWith", "(Ljava/lang/String;I)Z"),
            ("java/lang/String", "substring", "(I)Ljava/lang/String;"),
            ("java/lang/String", "substring", "(II)Ljava/lang/String;"),
            ("java/lang/String", "toLowerCase", "()Ljava/lang/String;"),
            ("java/lang/String", "toUpperCase", "()Ljava/lang/String;"),
            ("java/lang/String", "trim", "()Ljava/lang/String;"),
            ("java/lang/String", "valueOf", "(C)Ljava/lang/String;"),
            ("java/lang/String", "valueOf", "(D)Ljava/lang/String;"),
            ("java/lang/String", "valueOf", "(F)Ljava/lang/String;"),
            ("java/lang/String", "valueOf", "(I)Ljava/lang/String;"),
            ("java/lang/String", "valueOf", "(J)Ljava/lang/String;"),
            ("java/lang/String", "valueOf", "(Ljava/lang/Object;)Ljava/lang/String;"),
            ("java/lang/String", "valueOf", "(Z)Ljava/lang/String;"),
            ("java/lang/System", "arraycopy", "(Ljava/lang/Object;ILjava/lang/Object;II)V"),
            ("java/lang/System", "currentTimeMillis", "()J"),
            ("java/lang/System", "exit", "(I)V"),
            ("java/lang/System", "gc", "()V"),
            ("java/lang/System", "getenv", "()Ljava/util/Map;"),
            ("java/lang/System", "getenv", "(Ljava/lang/String;)Ljava/lang/String;"),
            ("java/lang/System", "identityHashCode", "(Ljava/lang/Object;)I"),
            ("java/lang/System", "lineSeparator", "()Ljava/lang/String;"),
            ("java/lang/System", "nanoTime", "()J"),
            ("java/lang/System", "registerNatives", "()V"),
            ("java/lang/System", "setProperty", "(Ljava/lang/String;Ljava/lang/String;)Ljava/lang/String;"),
            ("java/lang/Thread", "<init>", "()V"),
            ("java/lang/Thread", "<init>", "(Ljava/lang/Runnable;)V"),
            ("java/lang/Thread", "<init>", "(Ljava/lang/Runnable;Ljava/lang/String;)V"),
            ("java/lang/Thread", "<init>", "(Ljava/lang/String;)V"),
            ("java/lang/Thread", "<init>", "(Ljava/lang/ThreadGroup;Ljava/lang/Runnable;)V"),
            ("java/lang/Thread", "<init>", "(Ljava/lang/ThreadGroup;Ljava/lang/Runnable;Ljava/lang/String;)V"),
            ("java/lang/Thread", "<init>", "(Ljava/lang/ThreadGroup;Ljava/lang/Runnable;Ljava/lang/String;J)V"),
            ("java/lang/Thread", "<init>", "(Ljava/lang/ThreadGroup;Ljava/lang/Runnable;Ljava/lang/String;JZ)V"),
            ("java/lang/Thread", "<init>", "(Ljava/lang/ThreadGroup;Ljava/lang/String;)V"),
            ("java/lang/Thread", "currentThread", "()Ljava/lang/Thread;"),
            ("java/lang/Thread", "getName", "()Ljava/lang/String;"),
            ("java/lang/Thread", "getPriority", "()I"),
            ("java/lang/Thread", "getState", "()Ljava/lang/Thread$State;"),
            ("java/lang/Thread", "interrupt", "()V"),
            ("java/lang/Thread", "interrupted", "()Z"),
            ("java/lang/Thread", "isAlive", "()Z"),
            ("java/lang/Thread", "isDaemon", "()Z"),
            ("java/lang/Thread", "isInterrupted", "()Z"),
            ("java/lang/Thread", "registerNatives", "()V"),
            ("java/lang/Thread", "run", "()V"),
            ("java/lang/Thread", "setDaemon", "(Z)V"),
            ("java/lang/Thread", "setName", "(Ljava/lang/String;)V"),
            ("java/lang/Thread", "sleep", "(J)V"),
            ("java/lang/Thread", "start", "()V"),
            ("java/lang/Thread", "start0", "()V"),
            ("java/lang/Thread", "threadState", "()Ljava/lang/Thread$State;"),
            ("java/lang/Throwable", "fillInStackTrace", "(I)Ljava/lang/Throwable;"),
            ("java/lang/Throwable", "getStackTraceDepth", "()I"),
            ("java/lang/Throwable", "getStackTraceElement", "(I)Ljava/lang/StackTraceElement;"),
            ("java/lang/Throwable", "initCause", "(Ljava/lang/Throwable;)Ljava/lang/Throwable;"),
            ("java/lang/reflect/AccessibleObject", "setAccessible", "(Z)V"),
            ("java/lang/reflect/Constructor", "getDeclaringClass", "()Ljava/lang/Class;"),
            ("java/lang/reflect/Constructor", "getModifiers", "()I"),
            ("java/lang/reflect/Constructor", "getParameterCount", "()I"),
            ("java/lang/reflect/Constructor", "getParameterTypes", "()[Ljava/lang/Class;"),
            ("java/lang/reflect/Constructor", "newInstance", "([Ljava/lang/Object;)Ljava/lang/Object;"),
            ("java/lang/reflect/Constructor", "setAccessible", "(Z)V"),
            ("java/lang/reflect/Field", "get", "(Ljava/lang/Object;)Ljava/lang/Object;"),
            ("java/lang/reflect/Field", "getBoolean", "(Ljava/lang/Object;)Z"),
            ("java/lang/reflect/Field", "getByte", "(Ljava/lang/Object;)B"),
            ("java/lang/reflect/Field", "getChar", "(Ljava/lang/Object;)C"),
            ("java/lang/reflect/Field", "getDeclaringClass", "()Ljava/lang/Class;"),
            ("java/lang/reflect/Field", "getDouble", "(Ljava/lang/Object;)D"),
            ("java/lang/reflect/Field", "getFloat", "(Ljava/lang/Object;)F"),
            ("java/lang/reflect/Field", "getGenericType", "()Ljava/lang/reflect/Type;"),
            ("java/lang/reflect/Field", "getInt", "(Ljava/lang/Object;)I"),
            ("java/lang/reflect/Field", "getLong", "(Ljava/lang/Object;)J"),
            ("java/lang/reflect/Field", "getModifiers", "()I"),
            ("java/lang/reflect/Field", "getName", "()Ljava/lang/String;"),
            ("java/lang/reflect/Field", "getShort", "(Ljava/lang/Object;)S"),
            ("java/lang/reflect/Field", "getType", "()Ljava/lang/Class;"),
            ("java/lang/reflect/Field", "set", "(Ljava/lang/Object;Ljava/lang/Object;)V"),
            ("java/lang/reflect/Field", "setAccessible", "(Z)V"),
            ("java/lang/reflect/Field", "setBoolean", "(Ljava/lang/Object;Z)V"),
            ("java/lang/reflect/Field", "setByte", "(Ljava/lang/Object;B)V"),
            ("java/lang/reflect/Field", "setChar", "(Ljava/lang/Object;C)V"),
            ("java/lang/reflect/Field", "setDouble", "(Ljava/lang/Object;D)V"),
            ("java/lang/reflect/Field", "setFloat", "(Ljava/lang/Object;F)V"),
            ("java/lang/reflect/Field", "setInt", "(Ljava/lang/Object;I)V"),
            ("java/lang/reflect/Field", "setLong", "(Ljava/lang/Object;J)V"),
            ("java/lang/reflect/Field", "setShort", "(Ljava/lang/Object;S)V"),
            ("java/lang/reflect/Method", "getDeclaringClass", "()Ljava/lang/Class;"),
            ("java/lang/reflect/Method", "getGenericParameterTypes", "()[Ljava/lang/reflect/Type;"),
            ("java/lang/reflect/Method", "getGenericReturnType", "()Ljava/lang/reflect/Type;"),
            ("java/lang/reflect/Method", "getModifiers", "()I"),
            ("java/lang/reflect/Method", "getName", "()Ljava/lang/String;"),
            ("java/lang/reflect/Method", "getParameterCount", "()I"),
            ("java/lang/reflect/Method", "getParameterTypes", "()[Ljava/lang/Class;"),
            ("java/lang/reflect/Method", "getReturnType", "()Ljava/lang/Class;"),
            ("java/lang/reflect/Method", "getTypeParameters", "()[Ljava/lang/reflect/TypeVariable;"),
            ("java/lang/reflect/Method", "invoke", "(Ljava/lang/Object;[Ljava/lang/Object;)Ljava/lang/Object;"),
            ("java/lang/reflect/Method", "setAccessible", "(Z)V"),
        ],
    ),
    (
        "register_t25_natives",
        &[
            ("java/time/ZoneId", "systemDefault", "()Ljava/time/ZoneId;"),
        ],
    ),
    (
        "register_t310_scripting",
        &[
            ("javax/script/ScriptEngineManager", "getEngineByExtension", "(Ljava/lang/String;)Ljavax/script/ScriptEngine;"),
            ("javax/script/ScriptEngineManager", "getEngineByName", "(Ljava/lang/String;)Ljavax/script/ScriptEngine;"),
        ],
    ),
    (
        "register_t311_i18n",
        &[
            ("java/util/Locale", "getDefault", "()Ljava/util/Locale;"),
        ],
    ),
    (
        "register_t31_concurrent_extras",
        &[
            ("java/util/concurrent/Flow$Subscription", "cancel", "()V"),
            ("java/util/concurrent/Flow$Subscription", "request", "(J)V"),
        ],
    ),
    (
        "register_t38_jndi",
        &[
            ("javax/naming/InitialContext", "<init>", "()V"),
            ("javax/naming/InitialContext", "bind", "(Ljava/lang/String;Ljava/lang/Object;)V"),
            ("javax/naming/InitialContext", "close", "()V"),
            ("javax/naming/InitialContext", "getEnvironment", "()Ljava/util/Hashtable;"),
            ("javax/naming/InitialContext", "lookup", "(Ljava/lang/String;)Ljava/lang/Object;"),
            ("javax/naming/InitialContext", "rebind", "(Ljava/lang/String;Ljava/lang/Object;)V"),
            ("javax/naming/InitialContext", "unbind", "(Ljava/lang/String;)V"),
        ],
    ),
    (
        "register_t39_stax",
        &[
            ("javax/xml/stream/XMLInputFactory", "createXMLEventReader", "(Ljava/io/InputStream;)Ljavax/xml/stream/XMLEventReader;"),
            ("javax/xml/stream/XMLInputFactory", "createXMLEventReader", "(Ljava/io/Reader;)Ljavax/xml/stream/XMLEventReader;"),
            ("javax/xml/stream/XMLInputFactory", "createXMLStreamReader", "(Ljava/io/InputStream;)Ljavax/xml/stream/XMLStreamReader;"),
            ("javax/xml/stream/XMLInputFactory", "newFactory", "()Ljavax/xml/stream/XMLInputFactory;"),
            ("javax/xml/stream/XMLInputFactory", "newInstance", "()Ljavax/xml/stream/XMLInputFactory;"),
            ("javax/xml/stream/XMLStreamReader", "close", "()V"),
            ("javax/xml/stream/XMLStreamReader", "getEventType", "()I"),
            ("javax/xml/stream/XMLStreamReader", "getLocalName", "()Ljava/lang/String;"),
            ("javax/xml/stream/XMLStreamReader", "getText", "()Ljava/lang/String;"),
            ("javax/xml/stream/XMLStreamReader", "hasNext", "()Z"),
            ("javax/xml/stream/XMLStreamReader", "next", "()I"),
        ],
    ),
    (
        "register_time_extras_natives",
        &[
            ("java/time/ZoneId", "systemDefault", "()Ljava/time/ZoneId;"),
        ],
    ),
    (
        "register_time_natives",
        &[
            ("java/time/Instant", "equals", "(Ljava/lang/Object;)Z"),
            ("java/time/Instant", "getEpochSecond", "()J"),
            ("java/time/Instant", "getNano", "()I"),
            ("java/time/Instant", "hashCode", "()I"),
            ("java/time/Instant", "isAfter", "(Ljava/time/Instant;)Z"),
            ("java/time/Instant", "isBefore", "(Ljava/time/Instant;)Z"),
            ("java/time/Instant", "minusSeconds", "(J)Ljava/time/Instant;"),
            ("java/time/Instant", "now", "()Ljava/time/Instant;"),
            ("java/time/Instant", "ofEpochMilli", "(J)Ljava/time/Instant;"),
            ("java/time/Instant", "ofEpochSecond", "(J)Ljava/time/Instant;"),
            ("java/time/Instant", "ofEpochSecond", "(JJ)Ljava/time/Instant;"),
            ("java/time/Instant", "plusMillis", "(J)Ljava/time/Instant;"),
            ("java/time/Instant", "plusNanos", "(J)Ljava/time/Instant;"),
            ("java/time/Instant", "plusSeconds", "(J)Ljava/time/Instant;"),
            ("java/time/Instant", "toEpochMilli", "()J"),
            ("java/time/Instant", "toString", "()Ljava/lang/String;"),
        ],
    ),
    (
        "register_timeunit_natives",
        &[
            ("java/util/concurrent/TimeUnit", "convert", "(JLjava/util/concurrent/TimeUnit;)J"),
            ("java/util/concurrent/TimeUnit", "sleep", "(J)V"),
            ("java/util/concurrent/TimeUnit", "toDays", "(J)J"),
            ("java/util/concurrent/TimeUnit", "toHours", "(J)J"),
            ("java/util/concurrent/TimeUnit", "toMicros", "(J)J"),
            ("java/util/concurrent/TimeUnit", "toMillis", "(J)J"),
            ("java/util/concurrent/TimeUnit", "toMinutes", "(J)J"),
            ("java/util/concurrent/TimeUnit", "toNanos", "(J)J"),
            ("java/util/concurrent/TimeUnit", "toSeconds", "(J)J"),
        ],
    ),
    (
        "register_trust_manager_factory",
        &[
            ("javax/net/ssl/TrustManagerFactory", "getDefaultAlgorithm", "()Ljava/lang/String;"),
            ("javax/net/ssl/TrustManagerFactory", "getInstance", "(Ljava/lang/String;)Ljavax/net/ssl/TrustManagerFactory;"),
            ("javax/net/ssl/TrustManagerFactory", "getTrustManagers", "()[Ljavax/net/ssl/TrustManager;"),
            ("javax/net/ssl/TrustManagerFactory", "init", "(Ljava/security/KeyStore;)V"),
        ],
    ),
    (
        "register_unsafe_define_class",
        &[
            ("jdk/internal/misc/Unsafe", "defineClass", "(Ljava/lang/String;[BIILjava/lang/ClassLoader;Ljava/security/ProtectionDomain;)Ljava/lang/Class;"),
            ("jdk/internal/misc/Unsafe", "defineClass0", "(Ljava/lang/String;[BIILjava/lang/ClassLoader;Ljava/security/ProtectionDomain;)Ljava/lang/Class;"),
        ],
    ),
];

// ===========================================================================
// SECTION 2 — the scanner
// ===========================================================================

mod scan {
    /// One parsed file. `nc` has comments blanked but keeps string CONTENT;
    /// `full` additionally blanks string content. Both preserve byte offsets
    /// and newlines, so one offset indexes both.
    ///
    /// Two buffers are needed because the two jobs conflict: structure (braces,
    /// `fn` headers, call graph) must not see text inside literals, and the
    /// registered class/name/descriptor ARE text inside literals.
    pub struct FileSrc {
        pub rel: String,
        pub nc: Vec<u8>,
        pub full: Vec<u8>,
    }

    /// One `fn` definition. `body` is the offset of its `{`, `end` one past the
    /// matching `}`.
    pub struct FnDef {
        pub file: usize,
        pub name: String,
        pub line: usize,
        pub start: usize,
        pub body: usize,
        pub end: usize,
        pub takes_registry: bool,
        pub syn_gated: bool,
        pub testish: bool,
        pub parent: Option<usize>,
        /// Parameter names, in declaration order.
        ///
        /// Needed only by the one-level call-site binding in the resolver: a
        /// registrar helper takes the class as a PARAMETER, so the name is
        /// unbound inside its own body and the value lives at the call site.
        pub params: Vec<String>,
    }

    #[inline]
    pub fn is_ident_start(b: u8) -> bool {
        b.is_ascii_alphabetic() || b == b'_'
    }
    #[inline]
    pub fn is_ident(b: u8) -> bool {
        b.is_ascii_alphanumeric() || b == b'_'
    }

    fn wipe(out: &mut [u8], from: usize, to: usize) {
        let to = to.min(out.len());
        if from >= to {
            return;
        }
        for b in out[from..to].iter_mut() {
            if *b != b'\n' {
                *b = b' ';
            }
        }
    }

    /// Blank comments (into both buffers) and string/char literal CONTENT
    /// (into `full` only). Delimiters survive in both, so `full` still shows
    /// `""` where a literal was and the argument parser can tell a literal from
    /// an identifier.
    ///
    /// Region boundaries are ASCII and whole regions are blanked, so a
    /// multi-byte character inside a comment becomes N spaces rather than a
    /// broken code unit; the result is still valid UTF-8.
    pub fn blank(src: &[u8]) -> (Vec<u8>, Vec<u8>) {
        let n = src.len();
        let mut nc = src.to_vec();
        let mut full = src.to_vec();
        let mut i = 0usize;
        while i < n {
            let c = src[i];
            if c == b'/' && i + 1 < n && src[i + 1] == b'/' {
                let j = src[i..]
                    .iter()
                    .position(|&b| b == b'\n')
                    .map_or(n, |p| i + p);
                wipe(&mut nc, i, j);
                wipe(&mut full, i, j);
                i = j;
            } else if c == b'/' && i + 1 < n && src[i + 1] == b'*' {
                // Rust block comments nest.
                let mut depth = 0usize;
                let mut j = i;
                while j < n {
                    if src[j] == b'/' && j + 1 < n && src[j + 1] == b'*' {
                        depth += 1;
                        j += 2;
                    } else if src[j] == b'*' && j + 1 < n && src[j + 1] == b'/' {
                        depth -= 1;
                        j += 2;
                        if depth == 0 {
                            break;
                        }
                    } else {
                        j += 1;
                    }
                }
                wipe(&mut nc, i, j);
                wipe(&mut full, i, j);
                i = j;
            } else if (c == b'r' || c == b'b')
                && (i == 0 || !is_ident(src[i - 1]))
                && raw_string_open(src, i).is_some()
            {
                let (content, end) = raw_string_open(src, i).expect("checked on the line above");
                wipe(&mut full, content, end);
                i = end;
                // step past the closing delimiter too
                while i < n && (src[i] == b'"' || src[i] == b'#') {
                    i += 1;
                }
            } else if c == b'"' {
                let mut j = i + 1;
                while j < n {
                    if src[j] == b'\\' {
                        j += 2;
                        continue;
                    }
                    if src[j] == b'"' {
                        break;
                    }
                    j += 1;
                }
                wipe(&mut full, i + 1, j);
                i = (j + 1).min(n);
            } else if c == b'\'' {
                // Char literal or lifetime. A lifetime (`'a`, `'static`) must
                // not be treated as an unterminated literal, and `'\\'` must
                // not be walked with a "skip the char after a backslash" loop:
                // that swallows the literal's own closing quote and blanks
                // real code — braces included — until the next `'` in the
                // file. The brace-balance self-check in `parse_file` exists
                // because that bug produced a plausible-looking wrong answer.
                if i + 2 < n && src[i + 1] == b'\\' {
                    if let Some(p) = src[i + 2..].iter().take(8).position(|&b| b == b'\'') {
                        let end = i + 3 + p;
                        wipe(&mut full, i + 1, end - 1);
                        i = end;
                        continue;
                    }
                    i += 1;
                } else if i + 2 < n && src[i + 2] == b'\'' {
                    wipe(&mut full, i + 1, i + 2);
                    i += 3;
                } else {
                    i += 1;
                }
            } else {
                i += 1;
            }
        }
        (nc, full)
    }

    /// If a raw string starts at `i`, return `(content_start, content_end)`.
    fn raw_string_open(src: &[u8], i: usize) -> Option<(usize, usize)> {
        let n = src.len();
        let mut p = i;
        if src[p] == b'b' {
            p += 1;
            if p >= n || src[p] != b'r' {
                return None;
            }
        }
        if p >= n || src[p] != b'r' {
            return None;
        }
        p += 1;
        let hash_start = p;
        while p < n && src[p] == b'#' {
            p += 1;
        }
        let hashes = p - hash_start;
        if p >= n || src[p] != b'"' {
            return None;
        }
        let content = p + 1;
        let mut j = content;
        while j < n {
            if src[j] == b'"'
                && src[j + 1..]
                    .iter()
                    .take(hashes)
                    .filter(|&&b| b == b'#')
                    .count()
                    == hashes
            {
                return Some((content, j));
            }
            j += 1;
        }
        Some((content, n))
    }

    /// Index one past the `}` matching the `{` at `open`.
    pub fn match_brace(b: &[u8], open: usize) -> usize {
        let mut d = 0i64;
        let mut k = open;
        while k < b.len() {
            match b[k] {
                b'{' => d += 1,
                b'}' => {
                    d -= 1;
                    if d == 0 {
                        return k + 1;
                    }
                }
                _ => {}
            }
            k += 1;
        }
        b.len()
    }

    /// Index one past the `)` matching the `(` at `open`.
    pub fn match_paren(b: &[u8], open: usize) -> usize {
        let mut d = 0i64;
        let mut k = open;
        while k < b.len() {
            match b[k] {
                b'(' => d += 1,
                b')' => {
                    d -= 1;
                    if d == 0 {
                        return k + 1;
                    }
                }
                _ => {}
            }
            k += 1;
        }
        b.len()
    }

    /// Index one past the `]` matching the `[` at `open`.
    pub fn match_bracket(b: &[u8], open: usize) -> usize {
        let mut d = 0i64;
        let mut k = open;
        while k < b.len() {
            match b[k] {
                b'[' => d += 1,
                b']' => {
                    d -= 1;
                    if d == 0 {
                        return k + 1;
                    }
                }
                _ => {}
            }
            k += 1;
        }
        b.len()
    }

    /// True when `at` begins the keyword `kw` as a whole token.
    pub fn word_at(b: &[u8], at: usize, kw: &[u8]) -> bool {
        if !b[at..].starts_with(kw) {
            return false;
        }
        if at > 0 && is_ident(b[at - 1]) {
            return false;
        }
        let after = at + kw.len();
        after >= b.len() || !is_ident(b[after])
    }

    pub fn skip_ws(b: &[u8], mut p: usize) -> usize {
        while p < b.len() && (b[p] == b' ' || b[p] == b'\t' || b[p] == b'\r' || b[p] == b'\n') {
            p += 1;
        }
        p
    }

    /// Every `.rs` file under `dir`, sorted, skipping test/build trees.
    pub fn rs_files(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
        let Ok(rd) = std::fs::read_dir(dir) else {
            return;
        };
        let mut entries: Vec<std::path::PathBuf> = rd.flatten().map(|e| e.path()).collect();
        entries.sort();
        for p in entries {
            let name = p
                .file_name()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_default();
            if p.is_dir() {
                // `.claude` holds sibling worktrees — whole extra copies of
                // this repo. Descending into one doubles every count and makes
                // the answer depend on which lanes are running.
                if matches!(
                    name.as_str(),
                    ".git"
                        | "target"
                        | "node_modules"
                        | ".claude"
                        | "tests"
                        | "benches"
                        | "examples"
                        | "fuzz"
                ) {
                    continue;
                }
                rs_files(&p, out);
            } else if name.ends_with(".rs") {
                out.push(p);
            }
        }
    }
}

use scan::{
    is_ident, is_ident_start, match_brace, match_bracket, match_paren, skip_ws, word_at, FileSrc,
    FnDef,
};

/// A `(class, name, descriptor)` triple.
type Triple = (String, String, String);

/// Split a comma-separated argument list at depth 0. Returns `(full, nc)`
/// slices as owned, trimmed strings — parallel views of the same bytes.
/// Parameter names of a `fn` signature, in declaration order.
///
/// Deliberately conservative: anything that is not a plain `name: Type` pair
/// yields an EMPTY name in that position, so the slot still counts for
/// arity — a caller's Nth argument has to line up with the Nth parameter —
/// while never binding a name the resolver could then trust wrongly. `self`
/// is kept as a slot for the same reason.
fn param_names(sig: &str) -> Vec<String> {
    let Some(o) = sig.find('(') else {
        return Vec::new();
    };
    let bytes = sig.as_bytes();
    let mut depth = 0i64;
    let mut close = None;
    for (i, b) in bytes.iter().enumerate().skip(o) {
        match b {
            b'(' => depth += 1,
            b')' => {
                depth -= 1;
                if depth == 0 {
                    close = Some(i);
                    break;
                }
            }
            _ => {}
        }
    }
    let Some(c) = close else {
        return Vec::new();
    };
    let inner = &sig[o + 1..c];
    let ib = inner.as_bytes();
    let mut out = Vec::new();
    let mut d = 0i64;
    let mut start = 0usize;
    let mut push = |a: usize, b: usize, out: &mut Vec<String>| {
        let piece = inner[a..b].trim();
        if piece.is_empty() {
            return;
        }
        let name = match piece.split_once(':') {
            Some((n, _)) => n.trim(),
            None => piece,
        };
        let name = name.trim_start_matches("mut ").trim();
        if !name.is_empty()
            && name.bytes().all(|x| is_ident(x))
            && !name.starts_with(|ch: char| ch.is_ascii_digit())
        {
            out.push(name.to_string());
        } else {
            out.push(String::new());
        }
    };
    for (i, b) in ib.iter().enumerate() {
        match b {
            b'(' | b'[' | b'<' => d += 1,
            b')' | b']' | b'>' => d -= 1,
            b',' if d == 0 => {
                push(start, i, &mut out);
                start = i + 1;
            }
            _ => {}
        }
    }
    push(start, inner.len(), &mut out);
    out
}

fn split_args(full: &[u8], nc: &[u8]) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut depth = 0i64;
    let mut start = 0usize;
    let push = |a: usize, b: usize, out: &mut Vec<(String, String)>| {
        let f = String::from_utf8_lossy(&full[a..b]).trim().to_string();
        let c = String::from_utf8_lossy(&nc[a..b]).trim().to_string();
        if !f.is_empty() {
            out.push((f, c));
        }
    };
    for (i, &b) in full.iter().enumerate() {
        match b {
            b'(' | b'[' | b'{' => depth += 1,
            b')' | b']' | b'}' => depth -= 1,
            b',' if depth == 0 => {
                push(start, i, &mut out);
                start = i + 1;
            }
            _ => {}
        }
    }
    push(start, full.len(), &mut out);
    out
}

/// Undo the Rust escapes this scanner can encounter in a class/method/descriptor
/// literal. `$` and `/` need none; `\\` appears in a handful of Windows path
/// descriptors and `\"` in none, but both are handled so a future one is not a
/// silent corruption.
fn unescape(lit: &str) -> String {
    let inner = lit
        .strip_prefix('"')
        .and_then(|s| s.strip_suffix('"'))
        .unwrap_or(lit);
    let mut out = String::with_capacity(inner.len());
    let mut it = inner.chars();
    while let Some(c) = it.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match it.next() {
            Some('n') => out.push('\n'),
            Some('t') => out.push('\t'),
            Some('r') => out.push('\r'),
            Some('0') => out.push('\0'),
            Some(other) => out.push(other),
            None => out.push('\\'),
        }
    }
    out
}

/// Is this argument a single string literal? Checked on the BLANKED text, so
/// the content cannot fake a quote, then read from the unblanked text.
fn as_literal(full: &str, nc: &str) -> Option<String> {
    let f = full.trim();
    if f.len() >= 2
        && f.starts_with('"')
        && f.ends_with('"')
        && f.bytes().filter(|&b| b == b'"').count() == 2
    {
        return Some(unescape(nc.trim()));
    }
    None
}

/// Strip a leading `&` and a trailing `.as_str()`-style adaptor, so
/// `&CLS_FOO.as_str()` resolves the same as `CLS_FOO`.
///
/// The two inputs are byte-parallel views of the same source range — blanking
/// preserves length — so ONE pair of offsets can be applied to both. Trimming
/// them independently would desynchronise them the moment a literal's content
/// happened to be whitespace.
fn strip_adaptors(full: &str, nc: &str) -> (String, String) {
    let fb = full.as_bytes();
    let mut lo = 0usize;
    let mut hi = fb.len();
    let ws = |b: u8| b == b' ' || b == b'\t' || b == b'\r' || b == b'\n';
    while lo < hi && ws(fb[lo]) {
        lo += 1;
    }
    while hi > lo && ws(fb[hi - 1]) {
        hi -= 1;
    }
    while lo < hi && fb[lo] == b'&' {
        lo += 1;
        while lo < hi && ws(fb[lo]) {
            lo += 1;
        }
    }
    for tail in [".as_str()", ".as_ref()", ".to_string()", ".clone()"] {
        if hi >= lo + tail.len() && &full[hi - tail.len()..hi] == tail {
            hi -= tail.len();
            while hi > lo && ws(fb[hi - 1]) {
                hi -= 1;
            }
            break;
        }
    }
    let f = full.get(lo..hi).unwrap_or("").to_string();
    let c = nc.get(lo..hi).unwrap_or("").to_string();
    (f, c)
}

fn is_plain_ident(s: &str) -> bool {
    !s.is_empty()
        && s.bytes().enumerate().all(|(i, b)| {
            if i == 0 {
                is_ident_start(b)
            } else {
                is_ident(b)
            }
        })
}

/// Last segment of a `a::b::CONST` path, or `None` if it is not such a path.
fn path_tail(s: &str) -> Option<&str> {
    let tail = s.rsplit("::").next()?;
    if s.contains("::") && is_plain_ident(tail) {
        Some(tail)
    } else {
        None
    }
}

// ===========================================================================
// SECTION 3 — the analysis
// ===========================================================================

/// Everything the assertions read, computed once per test process.
struct Analysis {
    files: usize,
    fn_defs: usize,
    passes: usize,
    register_sites: usize,
    resolved_sites: usize,
    loop_expanded_sites: usize,
    unresolved: BTreeMap<String, usize>,
    /// `reason|file|enclosing-fn` -> count, so the blind region can NAME
    /// the registrars to teach the resolver about next.
    unresolved_where: BTreeMap<String, usize>,
    triples: usize,
    shipping: usize,
    synthetic_only: BTreeSet<String>,
    direct_synthetic_only: usize,
    /// The direct synthetic-only children by NAME, not just counted.
    ///
    /// `registrar_reachability.rs` pins the same set in
    /// `DELIBERATE_SYNTHETIC_ONLY_FAMILIES`, and
    /// [`the_two_gates_agree_on_the_synthetic_only_population`] compares them.
    /// Two independently written scanners over the same tree agreeing on 73
    /// names is the strongest evidence either file has that its reachability
    /// half is right; a count cannot deliver it.
    direct_synthetic_only_set: BTreeSet<String>,
    synthetic_overrides_body: usize,
    brace_imbalanced_files: Vec<String>,
    /// Every triple registered anywhere -> the passes that register it.
    registrants: BTreeMap<Triple, BTreeSet<String>>,
    /// Drifting triples -> (synthetic-only registrants, shipping registrants).
    drift: BTreeMap<Triple, (BTreeSet<String>, BTreeSet<String>)>,
    /// Drift count per synthetic-only pass.
    per_pass: BTreeMap<String, usize>,
    /// `pass -> "file:line"`, for legible failure messages.
    where_defined: BTreeMap<String, String>,
    /// `triple -> "file:line"` sites, for the same reason.
    sites_of: BTreeMap<Triple, Vec<String>>,
}

fn analysis() -> &'static Analysis {
    static A: OnceLock<Analysis> = OnceLock::new();
    A.get_or_init(build_analysis)
}

/// One file's `fn` definitions, with parent links, plus the file's own
/// brace-balance self-check.
fn parse_file(idx: usize, src: &FileSrc, raw: &str) -> (Vec<FnDef>, bool) {
    let t = &src.full;
    let n = t.len();
    let raw_lines: Vec<&str> = raw.lines().collect();
    // line index for offsets
    let mut line_starts: Vec<usize> = vec![0];
    for (i, &b) in t.iter().enumerate() {
        if b == b'\n' {
            line_starts.push(i + 1);
        }
    }
    let line_of = |off: usize| -> usize {
        match line_starts.binary_search(&off) {
            Ok(i) => i + 1,
            Err(i) => i,
        }
    };

    let mut out: Vec<FnDef> = Vec::new();
    let mut i = 0usize;
    while i < n {
        if !word_at(t, i, b"fn") {
            i += 1;
            continue;
        }
        let kw = i;
        let mut p = skip_ws(t, i + 2);
        if p >= n || !is_ident_start(t[p]) {
            i += 2;
            continue;
        }
        let name_start = p;
        while p < n && is_ident(t[p]) {
            p += 1;
        }
        let name = String::from_utf8_lossy(&t[name_start..p]).into_owned();
        let mut q = skip_ws(t, p);
        // generic parameter list
        if q < n && t[q] == b'<' {
            let mut d = 0i64;
            while q < n {
                match t[q] {
                    b'<' => d += 1,
                    b'>' => {
                        d -= 1;
                        if d == 0 {
                            q += 1;
                            break;
                        }
                    }
                    b'{' | b';' => break,
                    _ => {}
                }
                q += 1;
            }
            q = skip_ws(t, q);
        }
        if q >= n || t[q] != b'(' {
            i = p;
            continue;
        }
        let pe = match_paren(t, q);
        let mut r = pe;
        while r < n && t[r] != b'{' && t[r] != b';' {
            r += 1;
        }
        if r >= n || t[r] == b';' {
            // a bodyless trait declaration
            i = pe;
            continue;
        }
        let body = r;
        let end = match_brace(t, body);
        let sig = String::from_utf8_lossy(&t[kw..body]).into_owned();
        let line = line_of(kw);
        // Attributes are the `#[...]` lines directly above, skipping doc
        // comments and blanks. Matching `registrar_reachability.rs` on purpose:
        // two gates that disagree about what a `#[cfg(test)]` covers would
        // disagree about the population for reasons nobody could see.
        let mut attrs: Vec<String> = Vec::new();
        let mut a = line as i64 - 2;
        while a >= 0 && (a as usize) < raw_lines.len() {
            let txt = raw_lines[a as usize].trim();
            if txt.starts_with("#[") || txt.starts_with("#![") {
                attrs.push(txt.to_string());
            } else if txt.starts_with("//") || txt.is_empty() || txt.starts_with(')') {
                // keep walking
            } else {
                break;
            }
            a -= 1;
        }
        let testish = attrs.iter().any(|s| {
            s.starts_with("#[test]") || s.contains("cfg(test") || s.contains("cfg(all(test")
        });
        let syn_gated = attrs.iter().any(|s| s.contains(SYNTHETIC_FEATURE));
        out.push(FnDef {
            file: idx,
            name,
            line,
            start: kw,
            body,
            end,
            takes_registry: sig.contains(KEY_TYPE),
            syn_gated,
            testish,
            parent: None,
            params: param_names(&sig),
        });
        i = body + 1;
    }

    out.sort_by(|a, b| a.start.cmp(&b.start).then(b.end.cmp(&a.end)));
    // parent links, by a nesting stack over start order
    let mut stack: Vec<usize> = Vec::new();
    for k in 0..out.len() {
        // `.copied()` on purpose: the `while let` scrutinee must not hold a
        // borrow of `stack` across the `pop()` inside the body.
        while let Some(top) = stack.last().copied() {
            if out[top].end <= out[k].start {
                stack.pop();
            } else {
                break;
            }
        }
        out[k].parent = stack.last().copied();
        stack.push(k);
    }
    // `#[cfg(test)] mod` bodies, and downward propagation of testish
    let mut test_spans: Vec<(usize, usize)> = Vec::new();
    let mut j = 0usize;
    while j + 12 <= n {
        if t[j..].starts_with(b"#[cfg(test)]") {
            let mut k = j + 12;
            let mut saw_mod = false;
            while k < n && t[k] != b'{' {
                if word_at(t, k, b"mod") {
                    saw_mod = true;
                }
                k += 1;
            }
            if saw_mod && k < n {
                let e = match_brace(t, k);
                test_spans.push((k, e));
                j = e;
                continue;
            }
        }
        j += 1;
    }
    for k in 0..out.len() {
        // Computed into a local first, so no immutable borrow of `out` is
        // anywhere near the assignment that follows.
        let inside = test_spans
            .iter()
            .any(|&(s, e)| s <= out[k].start && out[k].end <= e);
        if inside {
            out[k].testish = true;
        }
    }
    for k in 0..out.len() {
        let mut p = out[k].parent;
        while let Some(pi) = p {
            if out[pi].testish {
                out[k].testish = true;
                break;
            }
            p = out[pi].parent;
        }
    }

    // A well-formed Rust file's braces balance after blanking. They will not if
    // a literal ate code, and the resulting census is confidently wrong rather
    // than obviously broken — so it is reported, never ignored.
    let opens = t.iter().filter(|&&b| b == b'{').count();
    let closes = t.iter().filter(|&&b| b == b'}').count();
    (out, opens == closes)
}

/// `const NAME: &str = "...";` / `static NAME: &str = "...";` in one file.
fn str_consts(src: &FileSrc) -> BTreeMap<String, String> {
    let t = &src.full;
    let nc = &src.nc;
    let n = t.len();
    let mut out = BTreeMap::new();
    let mut i = 0usize;
    while i < n {
        let is_const = word_at(t, i, b"const");
        let is_static = word_at(t, i, b"static");
        if !is_const && !is_static {
            i += 1;
            continue;
        }
        let mut p = skip_ws(t, i + if is_const { 5 } else { 6 });
        if p < n && word_at(t, p, b"mut") {
            p = skip_ws(t, p + 3);
        }
        if p >= n || !is_ident_start(t[p]) {
            i += 1;
            continue;
        }
        let ns = p;
        while p < n && is_ident(t[p]) {
            p += 1;
        }
        let name = String::from_utf8_lossy(&t[ns..p]).into_owned();
        let p2 = skip_ws(t, p);
        if p2 >= n || t[p2] != b':' {
            i = p;
            continue;
        }
        // The type must be `&str` / `&'static str` and the value one literal.
        // Deliberately NOT stopping at a newline: `const X: &str =` with the
        // literal on the next line is common in this tree, and stopping at the
        // line end drops those consts, which drops every triple that names
        // them — silently, and only in the gate, not in the mirror.
        let mut e = p2;
        while e < n && t[e] != b'=' && t[e] != b';' {
            e += 1;
        }
        if e >= n || t[e] != b'=' {
            i = p;
            continue;
        }
        let ty = String::from_utf8_lossy(&t[p2 + 1..e]);
        if !(ty.contains("str") && !ty.contains('[')) {
            i = e;
            continue;
        }
        let vs = skip_ws(t, e + 1);
        let mut ve = vs;
        while ve < n && t[ve] != b';' {
            ve += 1;
        }
        let full_v = String::from_utf8_lossy(&t[vs..ve.min(n)]).into_owned();
        let nc_v = String::from_utf8_lossy(&nc[vs..ve.min(n)]).into_owned();
        if let Some(v) = as_literal(full_v.trim(), nc_v.trim()) {
            out.insert(name, v);
        }
        i = ve.max(p);
    }
    out
}

/// `let x = "lit";` / `let x = SOME_CONST;` inside one fn body. FIRST binding
/// wins, deterministically; a shadowed rebinding is not modelled.
fn let_bindings(
    src: &FileSrc,
    from: usize,
    to: usize,
    file_consts: &BTreeMap<String, String>,
    global: &BTreeMap<String, String>,
    ambiguous: &BTreeSet<String>,
) -> BTreeMap<String, Option<String>> {
    let t = &src.full;
    let nc = &src.nc;
    let mut out: BTreeMap<String, Option<String>> = BTreeMap::new();
    let mut i = from;
    while i < to {
        if !word_at(t, i, b"let") {
            i += 1;
            continue;
        }
        let mut p = skip_ws(t, i + 3);
        if p < to && word_at(t, p, b"mut") {
            p = skip_ws(t, p + 3);
        }
        if p >= to || !is_ident_start(t[p]) {
            i += 3;
            continue;
        }
        let ns = p;
        while p < to && is_ident(t[p]) {
            p += 1;
        }
        let name = String::from_utf8_lossy(&t[ns..p]).into_owned();
        let mut e = p;
        while e < to && t[e] != b'=' && t[e] != b';' {
            e += 1;
        }
        if e >= to || t[e] != b'=' {
            i = p;
            continue;
        }
        let vs = skip_ws(t, e + 1);
        let mut ve = vs;
        while ve < to && t[ve] != b';' {
            ve += 1;
        }
        let fv = String::from_utf8_lossy(&t[vs..ve.min(to)]).into_owned();
        let cv = String::from_utf8_lossy(&nc[vs..ve.min(to)]).into_owned();
        let value = resolve_simple(&fv, &cv, file_consts, global, ambiguous);
        out.entry(name).or_insert(value);
        i = ve.max(p);
    }
    out
}

/// Literal, or `&str` const, and nothing else. Used for `let` right-hand sides
/// and `for`-loop array elements, where a local environment does not apply.
fn resolve_simple(
    full: &str,
    nc: &str,
    file_consts: &BTreeMap<String, String>,
    global: &BTreeMap<String, String>,
    ambiguous: &BTreeSet<String>,
) -> Option<String> {
    let (f, c) = strip_adaptors(full, nc);
    if let Some(v) = as_literal(&f, &c) {
        return Some(v);
    }
    let key: &str = if is_plain_ident(&f) {
        f.as_str()
    } else {
        path_tail(&f)?
    };
    if let Some(v) = file_consts.get(key) {
        return Some(v.clone());
    }
    if ambiguous.contains(key) {
        return None;
    }
    global.get(key).cloned()
}

/// A `for x in [ .. ] { .. }` loop over literal/const elements: the body span,
/// the bound variable, and the values it takes.
struct Loop {
    body: usize,
    end: usize,
    var: String,
    values: Option<Vec<String>>,
}

fn parse_loops(
    src: &FileSrc,
    file_consts: &BTreeMap<String, String>,
    global: &BTreeMap<String, String>,
    ambiguous: &BTreeSet<String>,
) -> Vec<Loop> {
    let t = &src.full;
    let nc = &src.nc;
    let n = t.len();
    let mut out = Vec::new();
    let mut i = 0usize;
    while i < n {
        if !word_at(t, i, b"for") {
            i += 1;
            continue;
        }
        let mut p = skip_ws(t, i + 3);
        if p >= n || !is_ident_start(t[p]) {
            i += 3;
            continue;
        }
        let vs = p;
        while p < n && is_ident(t[p]) {
            p += 1;
        }
        let var = String::from_utf8_lossy(&t[vs..p]).into_owned();
        let mut q = skip_ws(t, p);
        if !(q < n && word_at(t, q, b"in")) {
            i = p;
            continue;
        }
        q = skip_ws(t, q + 2);
        if q < n && t[q] == b'&' {
            q = skip_ws(t, q + 1);
        }
        if q >= n || t[q] != b'[' {
            i = p;
            continue;
        }
        let be = match_bracket(t, q);
        let mut bs = be;
        while bs < n && t[bs] != b'{' && t[bs] != b';' {
            bs += 1;
        }
        if bs >= n || t[bs] != b'{' {
            i = be;
            continue;
        }
        let end = match_brace(t, bs);
        let elems = split_args(&t[q + 1..be - 1], &nc[q + 1..be - 1]);
        let mut vals = Vec::new();
        let mut ok = !elems.is_empty();
        for (ef, en) in &elems {
            match resolve_simple(ef, en, file_consts, global, ambiguous) {
                Some(v) => vals.push(v),
                None => {
                    ok = false;
                    break;
                }
            }
        }
        out.push(Loop {
            body: bs,
            end,
            var,
            values: if ok { Some(vals) } else { None },
        });
        i = be;
    }
    out.sort_by_key(|l| l.body);
    out
}

fn build_analysis() -> Analysis {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let workspace: &Path = manifest
        .parent()
        .expect("native-builtins has a parent directory (the workspace root)");

    // --- 1. read and blank -------------------------------------------------
    let mut paths: Vec<PathBuf> = Vec::new();
    for c in CRATES {
        let dir = workspace.join(c).join("src");
        if dir.is_dir() {
            scan::rs_files(&dir, &mut paths);
        }
    }
    paths.sort();

    let mut files: Vec<FileSrc> = Vec::with_capacity(paths.len());
    let mut fns: Vec<FnDef> = Vec::new();
    let mut fn_ranges: Vec<(usize, usize)> = Vec::new(); // per file: [lo, hi)
    let mut brace_imbalanced: Vec<String> = Vec::new();
    for p in &paths {
        let raw = std::fs::read_to_string(p)
            .unwrap_or_else(|e| panic!("cannot read {}: {e}", p.display()));
        let (nc, full) = scan::blank(raw.as_bytes());
        let rel = p
            .strip_prefix(workspace)
            .unwrap_or(p)
            .to_string_lossy()
            .replace('\\', "/");
        let idx = files.len();
        files.push(FileSrc { rel, nc, full });
        let (mut defs, balanced) = parse_file(idx, &files[idx], &raw);
        if !balanced {
            brace_imbalanced.push(files[idx].rel.clone());
        }
        let lo = fns.len();
        // parent indices are file-local; rebase them onto the global vector
        for d in defs.iter_mut() {
            d.parent = d.parent.map(|k| k + lo);
        }
        fns.append(&mut defs);
        fn_ranges.push((lo, fns.len()));
    }

    // --- 2. the population -------------------------------------------------
    let mut pass_names: BTreeSet<String> = BTreeSet::new();
    let mut syn_gated: BTreeSet<String> = BTreeSet::new();
    let mut where_defined: BTreeMap<String, String> = BTreeMap::new();
    for f in &fns {
        if !f.takes_registry {
            continue;
        }
        pass_names.insert(f.name.clone());
        where_defined
            .entry(f.name.clone())
            .or_insert_with(|| format!("{}:{}", files[f.file].rel, f.line));
        if f.syn_gated {
            syn_gated.insert(f.name.clone());
        }
    }

    // --- 3. call graph -----------------------------------------------------
    // Identifiers in CALL position only. `r.register(..)` is a method call on
    // the registry; counting it as a reference to the several
    // `fn register(r: &mut NativeMethodRegistry)` definitions in `jca/*` wires
    // every registrar in the tree to every jca pass, inflates the synthetic
    // closure by ~70 names, and moves whole families across the shipping line.
    let mut calls_from: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    let mut non_pass_called: BTreeSet<String> = BTreeSet::new();
    let mut children: Vec<Vec<usize>> = vec![Vec::new(); fns.len()];
    for k in 0..fns.len() {
        if let Some(p) = fns[k].parent {
            children[p].push(k);
        }
    }
    let mut synthetic_overrides_body = 0usize;
    for k in 0..fns.len() {
        if fns[k].testish {
            continue;
        }
        if fns[k].name == SYNTHETIC_OVERRIDES {
            synthetic_overrides_body = synthetic_overrides_body.max(fns[k].end - fns[k].body);
        }
        // this fn's own text, minus its nested fn bodies
        let t = &files[fns[k].file].full;
        let mut segments: Vec<(usize, usize)> = Vec::new();
        let mut pos = fns[k].body;
        let mut kids: Vec<usize> = children[k].clone();
        kids.sort_by_key(|&c| fns[c].start);
        for c in kids {
            if fns[c].start >= pos {
                segments.push((pos, fns[c].start));
            }
            pos = pos.max(fns[c].end);
        }
        segments.push((pos, fns[k].end.min(t.len())));

        let mut hits: BTreeSet<String> = BTreeSet::new();
        for (a, b) in segments {
            let mut i = a;
            while i < b {
                if !is_ident_start(t[i]) {
                    i += 1;
                    continue;
                }
                let s = i;
                while i < b && is_ident(t[i]) {
                    i += 1;
                }
                if s > 0 && (t[s - 1] == b'.' || is_ident(t[s - 1])) {
                    continue;
                }
                let j = skip_ws(t, i);
                if j >= t.len() || t[j] != b'(' {
                    continue;
                }
                // `fn name(` is a definition, not a call
                let mut back = s;
                while back > 0 && (t[back - 1] == b' ' || t[back - 1] == b'\n') {
                    back -= 1;
                }
                if back >= 2 && &t[back - 2..back] == b"fn" {
                    continue;
                }
                let id = String::from_utf8_lossy(&t[s..i]).into_owned();
                if pass_names.contains(&id) && id != fns[k].name {
                    hits.insert(id);
                }
            }
        }
        if fns[k].takes_registry {
            calls_from
                .entry(fns[k].name.clone())
                .or_default()
                .extend(hits);
        } else {
            if !fns[k].syn_gated {
                non_pass_called.extend(hits);
            }
        }
    }
    // Module-level references (a pass named in a `static` table, a `use`
    // re-export) count as non-pass references too.
    for (fi, f) in files.iter().enumerate() {
        let (lo, hi) = fn_ranges[fi];
        let mut tops: Vec<(usize, usize)> = Vec::new();
        for k in lo..hi {
            if fns[k].parent.is_none() {
                tops.push((fns[k].start, fns[k].end));
            }
        }
        tops.sort();
        let mut segments: Vec<(usize, usize)> = Vec::new();
        let mut pos = 0usize;
        for (a, b) in tops {
            if a >= pos {
                segments.push((pos, a));
            }
            pos = pos.max(b);
        }
        segments.push((pos, f.full.len()));
        for (a, b) in segments {
            let t = &f.full;
            let mut i = a;
            while i < b {
                if !is_ident_start(t[i]) {
                    i += 1;
                    continue;
                }
                let s = i;
                while i < b && is_ident(t[i]) {
                    i += 1;
                }
                // Same `.`-exclusion as the in-fn scan: a method name is not a
                // reference to a free function of the same name.
                if s > 0 && t[s - 1] == b'.' {
                    continue;
                }
                let id = String::from_utf8_lossy(&t[s..i]).into_owned();
                if pass_names.contains(&id) {
                    non_pass_called.insert(id);
                }
            }
        }
    }

    // --- 4. reachability ---------------------------------------------------
    let reach = |seeds: &BTreeSet<String>, block: &BTreeSet<String>| -> BTreeSet<String> {
        let mut seen: BTreeSet<String> = BTreeSet::new();
        let mut stack: Vec<String> = Vec::new();
        for s in seeds {
            if !block.contains(s) {
                seen.insert(s.clone());
                stack.push(s.clone());
            }
        }
        while let Some(n) = stack.pop() {
            if let Some(cs) = calls_from.get(&n) {
                for c in cs {
                    if block.contains(c) || seen.contains(c) {
                        continue;
                    }
                    seen.insert(c.clone());
                    stack.push(c.clone());
                }
            }
        }
        seen
    };

    let syn_seed: BTreeSet<String> = [SYNTHETIC_OVERRIDES.to_string()].into_iter().collect();
    let syn_reach = reach(&syn_seed, &BTreeSet::new());
    // `register_builtins` is ITSELF `#[cfg(feature = "synthetic-jdk")]`, is
    // `pub`, and is referenced from `vm/src/native/builtins.rs`. Taken as a
    // shipping root it reaches `register_synthetic_overrides` and drags every
    // synthetic-reachable name into the shipping set, and this gate reports a
    // clean, confident zero. That is not hypothetical — F34-1 §2.1 records it
    // happening.
    let mut roots: BTreeSet<String> = non_pass_called.clone();
    roots.remove(SYNTHETIC_OVERRIDES);
    for g in &syn_gated {
        roots.remove(g);
    }
    let shipping = reach(&roots, &syn_gated);
    let synthetic_only: BTreeSet<String> = syn_reach.difference(&shipping).cloned().collect();
    let direct_synthetic_only_set: BTreeSet<String> = calls_from
        .get(SYNTHETIC_OVERRIDES)
        .map(|d| {
            d.iter()
                .filter(|n| synthetic_only.contains(*n))
                .cloned()
                .collect()
        })
        .unwrap_or_default();
    let direct_synthetic_only = direct_synthetic_only_set.len();

    // --- 5. constants ------------------------------------------------------
    let mut per_file_consts: Vec<BTreeMap<String, String>> = Vec::with_capacity(files.len());
    let mut global: BTreeMap<String, String> = BTreeMap::new();
    let mut ambiguous: BTreeSet<String> = BTreeSet::new();
    for f in &files {
        let m = str_consts(f);
        for (k, v) in &m {
            match global.get(k) {
                Some(prev) if prev != v => {
                    ambiguous.insert(k.clone());
                }
                _ => {}
            }
            global.insert(k.clone(), v.clone());
        }
        per_file_consts.push(m);
    }

    // --- 6. registration sites --------------------------------------------
    let mut registrants: BTreeMap<Triple, BTreeSet<String>> = BTreeMap::new();
    let mut sites_of: BTreeMap<Triple, Vec<String>> = BTreeMap::new();
    let mut unresolved: BTreeMap<String, usize> = BTreeMap::new();
    let mut register_sites = 0usize;
    let mut resolved_sites = 0usize;
    let mut loop_expanded_sites = 0usize;
    let mut param_bound_sites = 0usize;
    let mut unresolved_where: BTreeMap<String, usize> = BTreeMap::new();
    let bump = |m: &mut BTreeMap<String, usize>, k: &str| {
        *m.entry(k.to_string()).or_insert(0) += 1;
    };

    for (fi, f) in files.iter().enumerate() {
        let t = &f.full;
        let nc = &f.nc;
        let n = t.len();
        let (lo, hi) = fn_ranges[fi];
        let loops = parse_loops(f, &per_file_consts[fi], &global, &ambiguous);
        let mut let_cache: BTreeMap<usize, BTreeMap<String, Option<String>>> = BTreeMap::new();
        // Line numbers are only for failure messages, but counting newlines
        // from 0 at every one of ~14,000 sites is quadratic over ~80 MB of
        // source and turns a two-second test into a two-minute one.
        let mut line_starts: Vec<usize> = vec![0];
        for (bi, &b) in t.iter().enumerate() {
            if b == b'\n' {
                line_starts.push(bi + 1);
            }
        }

        // ---- one level of call-site parameter binding -------------------
        //
        // A registrar helper takes the class as a PARAMETER:
        //
        //     for ms in [PE_SEGMENT_INTERFACE, CRATON_SEGMENT_CLASS] {
        //         register_pe2_string_marshaling_on(r, ms);
        //     }
        //     fn register_pe2_string_marshaling_on(r: &mut .., ms: &str) {
        //         r.register(ms, "getUtf8String", ..)
        //
        // The loop expansion below only sees loops whose span CONTAINS the
        // register site, so `ms` was `unbound-identifier` and every row in such
        // a helper fell into the blind region this file's vacuity control
        // measures. That is the form `panama.rs` introduced on 2026-08-22 and
        // the reason three gates went red at once.
        //
        // SAME-FILE callers only, and that is a real limit rather than an
        // oversight: a cross-file caller needs a whole-tree call graph, and the
        // conservative direction here is to leave such a site UNRESOLVED (in
        // the blind region, where the ceiling can see it) rather than to bind
        // it from a caller this pass cannot prove is the only one.
        let mut callsites: BTreeMap<String, Vec<(usize, Vec<(String, String)>)>> = BTreeMap::new();
        {
            let mut j = 0usize;
            while j < n {
                if !is_ident_start(t[j]) || (j > 0 && is_ident(t[j - 1])) {
                    j += 1;
                    continue;
                }
                let mut k = j;
                while k < n && is_ident(t[k]) {
                    k += 1;
                }
                let name = String::from_utf8_lossy(&t[j..k]).into_owned();
                let paren = skip_ws(t, k);
                // NOT the definition. `fn name(r: &mut .., c: &str)` matches
                // `name(` just as a call does, and its "arguments" are the
                // PARAMETER LIST, which never resolves — so including it made
                // the all-sites-must-resolve rule refuse every real binding.
                // Measured: this alone was 41 sites on
                // `register_al_sublist_natives_on` and 41 more on
                // `register_pe_memory_segment_on`.
                let mut b = j;
                while b > 0 && (t[b - 1] == b' ' || t[b - 1] == b'\t') {
                    b -= 1;
                }
                let is_def = b >= 2 && &t[b - 2..b] == b"fn";
                if paren < n && t[paren] == b'(' && !is_def && name.starts_with("register") {
                    let cend = match_paren(t, paren);
                    if cend > paren + 1 {
                        let a = split_args(&t[paren + 1..cend - 1], &nc[paren + 1..cend - 1]);
                        callsites.entry(name).or_default().push((paren, a));
                    }
                }
                j = k;
            }
        }

        // innermost-enclosing-fn sweep, in increasing site order
        let mut next_fn = lo;
        let mut stack: Vec<usize> = Vec::new();

        let mut i = 0usize;
        while i < n {
            if t[i] != b'.' {
                i += 1;
                continue;
            }
            let p = skip_ws(t, i + 1);
            if !(p < n && t[p..].starts_with(b"register")) {
                i += 1;
                continue;
            }
            let after = p + 8;
            let mut q = skip_ws(t, after);
            // `register_with_kind(class, name, desc, body, kind)` is a
            // registration like any other, and this scan used to skip it
            // because the byte after `register` is not `(`. That blind spot
            // covered 757 sites tree-wide -- and it is not a uniform sample of
            // the registry, it is exactly the sites whose kind was ADJUDICATED.
            // Converting `register` -> `register_with_kind` is this campaign's
            // standard remedy for a contract-1.4 shadow, so every such
            // conversion silently removed the shipping half of a drift pair and
            // this gate reported the erasure as "no longer drifts". Measured
            // 2026-09-10 on `Class.getModule`, whose two bodies (lib.rs closure
            // vs `phases_late/reflect_invoke.rs` closure) still differ.
            // The first three arguments are in the same positions, so
            // everything downstream is unchanged.
            if !(q < n && t[q] == b'(') && t[after..].starts_with(b"_with_kind") {
                q = skip_ws(t, after + 10);
            }
            if q >= n || t[q] != b'(' {
                // `registered_by`, `register_all`, `registers`, …
                i += 1;
                continue;
            }
            register_sites += 1;
            let op = q;
            let ce = match_paren(t, op);
            i = op + 1;
            if ce <= op + 1 {
                bump(&mut unresolved, "empty-arg-list");
                continue;
            }
            let args = split_args(&t[op + 1..ce - 1], &nc[op + 1..ce - 1]);
            if args.len() < 4 {
                // `thread_registry.register(..)`, `self.register(..)`,
                // `SubstitutionRegistry::register(a, b, c)` and friends: real
                // methods, wrong registry. Counted so the number is visible.
                bump(&mut unresolved, "arity<4");
                continue;
            }

            // advance the nesting stack to `op`
            while next_fn < hi && fns[next_fn].start <= op {
                while let Some(top) = stack.last().copied() {
                    if fns[top].end <= fns[next_fn].start {
                        stack.pop();
                    } else {
                        break;
                    }
                }
                stack.push(next_fn);
                next_fn += 1;
            }
            while let Some(top) = stack.last().copied() {
                if fns[top].end <= op {
                    stack.pop();
                } else {
                    break;
                }
            }
            let Some(encl) = stack.last().copied() else {
                bump(&mut unresolved, "no-enclosing-fn");
                continue;
            };
            if fns[encl].testish {
                continue;
            }
            let mut owner = Some(encl);
            while let Some(o) = owner {
                if fns[o].takes_registry {
                    break;
                }
                owner = fns[o].parent;
            }
            let Some(owner) = owner else {
                bump(&mut unresolved, "no-registry-owner");
                continue;
            };
            let owner_name = fns[owner].name.clone();

            let lets = let_cache.entry(encl).or_insert_with(|| {
                let_bindings(
                    f,
                    fns[encl].body,
                    fns[encl].end,
                    &per_file_consts[fi],
                    &global,
                    &ambiguous,
                )
            });

            // enclosing `for` loops
            let mut envs: Vec<BTreeMap<String, String>> = vec![BTreeMap::new()];
            let mut unparsed_loop = false;
            for l in loops.iter() {
                if !(l.body <= op && op < l.end) {
                    continue;
                }
                match &l.values {
                    None => unparsed_loop = true,
                    Some(vals) => {
                        let mut next: Vec<BTreeMap<String, String>> = Vec::new();
                        for e in &envs {
                            for v in vals {
                                let mut d = e.clone();
                                d.insert(l.var.clone(), v.clone());
                                next.push(d);
                            }
                        }
                        if next.len() <= 512 {
                            envs = next;
                        }
                    }
                }
            }
            // Bind this helper's own parameters from its same-file call
            // sites, one level deep. A parameter is bound only when EVERY call
            // site resolves it; one unresolvable caller leaves the name unbound
            // and the site stays in the blind region, which is the direction
            // that keeps the ceiling meaningful.
            if !fns[encl].params.is_empty() {
                if let Some(sites) = callsites.get(&fns[encl].name) {
                    for (pi, pname) in fns[encl].params.iter().enumerate() {
                        if pname.is_empty() {
                            continue;
                        }
                        let mut vals: BTreeSet<String> = BTreeSet::new();
                        let mut all = !sites.is_empty();
                        for (coff, cargs) in sites {
                            let Some((af, an)) = cargs.get(pi) else {
                                all = false;
                                break;
                            };
                            // the CALLER's loops, not the callee's
                            let mut cvals: Vec<String> = Vec::new();
                            if let Some(v) =
                                resolve_simple(af, an, &per_file_consts[fi], &global, &ambiguous)
                            {
                                cvals.push(v);
                            } else {
                                let (sf, sn) = strip_adaptors(af, an);
                                let key: Option<&str> = if is_plain_ident(&sf) {
                                    Some(sf.as_str())
                                } else {
                                    path_tail(&sf)
                                };
                                if let Some(key) = key {
                                    for l in loops.iter() {
                                        if l.var == key && l.body <= *coff && *coff < l.end {
                                            if let Some(vs) = &l.values {
                                                cvals.extend(vs.iter().cloned());
                                            }
                                        }
                                    }
                                }
                            }
                            if cvals.is_empty() {
                                all = false;
                                break;
                            }
                            vals.extend(cvals);
                        }
                        if all && !vals.is_empty() && vals.len() <= 32 {
                            let mut next: Vec<BTreeMap<String, String>> = Vec::new();
                            for e in &envs {
                                for v in &vals {
                                    let mut d = e.clone();
                                    d.insert(pname.clone(), v.clone());
                                    next.push(d);
                                }
                            }
                            if next.len() <= 512 {
                                envs = next;
                                param_bound_sites += 1;
                            }
                        }
                    }
                }
            }

            if envs.len() > 1 {
                loop_expanded_sites += 1;
            }

            let mut any = false;
            let mut why = if unparsed_loop {
                "for-loop-unparsed".to_string()
            } else {
                "unresolved-argument".to_string()
            };
            for env in &envs {
                let mut vals: Vec<String> = Vec::with_capacity(3);
                for (af, an) in args.iter().take(3) {
                    let (sf, sn) = strip_adaptors(af, an);
                    if let Some(v) = as_literal(&sf, &sn) {
                        vals.push(v);
                        continue;
                    }
                    if sf.contains("format!") {
                        why = "format!".to_string();
                        break;
                    }
                    let key: Option<&str> = if is_plain_ident(&sf) {
                        Some(sf.as_str())
                    } else {
                        path_tail(&sf)
                    };
                    let Some(key) = key else {
                        why = "expression".to_string();
                        break;
                    };
                    if let Some(v) = env.get(key) {
                        vals.push(v.clone());
                    } else if let Some(Some(v)) = lets.get(key) {
                        vals.push(v.clone());
                    } else if let Some(v) = per_file_consts[fi].get(key) {
                        vals.push(v.clone());
                    } else if !ambiguous.contains(key) {
                        match global.get(key) {
                            Some(v) => vals.push(v.clone()),
                            None => {
                                why = "unbound-identifier".to_string();
                                break;
                            }
                        }
                    } else {
                        why = "ambiguous-const".to_string();
                        break;
                    }
                }
                if vals.len() != 3 {
                    continue;
                }
                any = true;
                let key: Triple = (vals[0].clone(), vals[1].clone(), vals[2].clone());
                let line = match line_starts.binary_search(&op) {
                    Ok(k) => k + 1,
                    Err(k) => k,
                };
                registrants
                    .entry(key.clone())
                    .or_default()
                    .insert(owner_name.clone());
                let sites = sites_of.entry(key).or_default();
                if sites.len() < 8 {
                    sites.push(format!("{} @ {}:{}", owner_name, f.rel, line));
                }
            }
            if any {
                resolved_sites += 1;
            } else {
                bump(&mut unresolved, &why);
                // WHERE, not just how many. The category alone says a form is
                // unresolvable; it does not say which registrar to teach the
                // resolver about next, and this gate's remedy is explicitly to
                // teach a form rather than to raise the ceiling.
                *unresolved_where
                    .entry(format!("{why}|{}|{}", f.rel, fns[encl].name))
                    .or_insert(0usize) += 1;
            }
        }
    }

    // --- 7. drift ----------------------------------------------------------
    let mut drift: BTreeMap<Triple, (BTreeSet<String>, BTreeSet<String>)> = BTreeMap::new();
    let mut per_pass: BTreeMap<String, usize> = BTreeMap::new();
    for (tri, regs) in &registrants {
        let so: BTreeSet<String> = regs.intersection(&synthetic_only).cloned().collect();
        let sh: BTreeSet<String> = regs.intersection(&shipping).cloned().collect();
        if so.is_empty() || sh.is_empty() {
            continue;
        }
        for p in &so {
            *per_pass.entry(p.clone()).or_insert(0) += 1;
        }
        drift.insert(tri.clone(), (so, sh));
    }

    Analysis {
        files: files.len(),
        fn_defs: fns.len(),
        passes: pass_names.len(),
        register_sites,
        resolved_sites,
        loop_expanded_sites,
        unresolved,
        unresolved_where,
        triples: registrants.len(),
        shipping: shipping.len(),
        synthetic_only,
        direct_synthetic_only,
        direct_synthetic_only_set,
        synthetic_overrides_body,
        brace_imbalanced_files: brace_imbalanced,
        registrants,
        drift,
        per_pass,
        where_defined,
        sites_of,
    }
}

fn triple(t: (&str, &str, &str)) -> Triple {
    (t.0.to_string(), t.1.to_string(), t.2.to_string())
}

fn show(a: &Analysis, t: &Triple) -> String {
    let regs = a
        .registrants
        .get(t)
        .map(|s| s.iter().cloned().collect::<Vec<_>>().join(", "))
        .unwrap_or_else(|| "<not registered anywhere this scan can see>".to_string());
    let sites = a
        .sites_of
        .get(t)
        .map(|v| v.join("\n            "))
        .unwrap_or_else(|| "<no site>".to_string());
    format!(
        "{}.{}{}\n        registered by: {regs}\n        sites:\n            {sites}",
        t.0, t.1, t.2
    )
}

// ===========================================================================
// SECTION 4 — the assertions
// ===========================================================================

/// The scanner must be measuring something, and must discriminate.
///
/// # What this catches
///
/// * the directory walk finding nothing (`MIN_FILES`);
/// * the `fn` parser breaking (`MIN_FN_DEFS`);
/// * `NativeMethodRegistry` renamed, so the population empties
///   (`MIN_PASSES`) — F34-1 §M4a's mutation;
/// * `register_synthetic_overrides` ceasing to be findable, which leaves
///   `families`-style set checks green and is caught here ONLY by the body-size
///   floor — F34-1 §M4b's mutation, reproduced deliberately;
/// * the descriptor resolver silently degrading (`MIN_RESOLVED_SITES`,
///   `MIN_TRIPLES`), which would otherwise report a small tidy fictional drift
///   number;
/// * loop expansion regressing (`MIN_LOOP_EXPANDED_SITES`);
/// * the shipping closure collapsing, which makes everything look
///   synthetic-only (`MIN_SHIPPING`);
/// * a literal running away and eating code (`brace_imbalanced_files`) — the
///   failure mode that produced this file's first, wrong, census.
#[test]
fn the_drift_scanner_is_not_vacuous() {
    let a = analysis();

    println!(
        "registrar-drift: {} files, {} fn defs, {} passes, {} register sites \
         ({} resolved, {} loop-expanded), {} distinct triples, {} shipping-reachable, \
         {} synthetic-only ({} direct), {} DRIFTING triples",
        a.files,
        a.fn_defs,
        a.passes,
        a.register_sites,
        a.resolved_sites,
        a.loop_expanded_sites,
        a.triples,
        a.shipping,
        a.synthetic_only.len(),
        a.direct_synthetic_only,
        a.drift.len(),
    );
    println!("registrar-drift: unresolved by reason: {:?}", a.unresolved);

    assert!(
        a.brace_imbalanced_files.is_empty(),
        "after blanking, these files' braces do not balance: {:?}\n\
         That means a string or char literal was not terminated where the scanner thought, so \
         it blanked real code — including `{{` and `}}` — until the next delimiter. Every fn \
         span, every call-graph edge and every triple in such a file is unreliable. Fix the \
         blanker; do NOT relax this assertion.",
        a.brace_imbalanced_files
    );
    assert!(
        a.files >= MIN_FILES,
        "found only {} .rs files across {CRATES:?} (floor {MIN_FILES}); the directory walk is \
         broken and every count below it is meaningless",
        a.files
    );
    assert!(
        a.fn_defs >= MIN_FN_DEFS,
        "parsed only {} fn definitions (floor {MIN_FN_DEFS}); the header parser is broken",
        a.fn_defs
    );
    assert!(
        a.passes >= MIN_PASSES,
        "found only {} registration passes (floor {MIN_PASSES}); the population is keyed on a \
         signature mentioning `{KEY_TYPE}` — if that type was renamed, re-point KEY_TYPE rather \
         than lowering this floor",
        a.passes
    );
    assert!(
        a.synthetic_overrides_body >= MIN_SYNTHETIC_OVERRIDES_BODY,
        "`{SYNTHETIC_OVERRIDES}`'s body extracted as {} bytes (floor \
         {MIN_SYNTHETIC_OVERRIDES_BODY}); the locator or the brace scan is broken, so the \
         synthetic closure is a fragment and every drift verdict below is understated. This is \
         the ONLY check that noticed F34-1's M4b mutation.",
        a.synthetic_overrides_body
    );
    assert!(
        a.register_sites >= MIN_REGISTER_SITES,
        "found only {} `.register(` sites (floor {MIN_REGISTER_SITES})",
        a.register_sites
    );
    assert!(
        a.resolved_sites >= MIN_RESOLVED_SITES,
        "resolved only {} of {} `.register(` sites (floor {MIN_RESOLVED_SITES}); the \
         literal/const resolver has degraded, and a census that cannot read descriptors reports \
         a small, clean, entirely fictional drift number. Unresolved by reason: {:?}",
        a.resolved_sites,
        a.register_sites,
        a.unresolved
    );
    assert!(
        a.triples >= MIN_TRIPLES,
        "recovered only {} distinct (class, name, descriptor) triples (floor {MIN_TRIPLES})",
        a.triples
    );
    assert!(
        a.loop_expanded_sites >= MIN_LOOP_EXPANDED_SITES,
        "only {} register sites expanded through a `for x in [..]` loop (floor \
         {MIN_LOOP_EXPANDED_SITES}); F34-1 warned that loop-emitted rows are invisible to a \
         site-counting scan, and without expansion whole classes vanish with no other symptom",
        a.loop_expanded_sites
    );
    assert!(
        a.shipping >= MIN_SHIPPING,
        "only {} passes are shipping-reachable (floor {MIN_SHIPPING}); the shipping closure \
         collapsed, which makes everything look synthetic-only and INFLATES drift",
        a.shipping
    );
    assert!(
        a.synthetic_only.len() >= MIN_SYNTHETIC_ONLY,
        "only {} synthetic-only passes (floor {MIN_SYNTHETIC_ONLY}); if the synthetic closure \
         collapsed, drift reads zero and this gate is decoration. F34-1 §2.1 records exactly \
         this happening when `register_builtins` was taken as a shipping root.",
        a.synthetic_only.len()
    );
    assert!(
        a.direct_synthetic_only >= MIN_DIRECT_SYNTHETIC_ONLY,
        "only {} synthetic-only DIRECT children of `{SYNTHETIC_OVERRIDES}` (floor \
         {MIN_DIRECT_SYNTHETIC_ONLY}); `registrar_reachability.rs` allow-lists 73",
        a.direct_synthetic_only
    );
    assert!(
        a.drift.len() >= MIN_TOTAL_DRIFT,
        "only {} drifting triples (floor {MIN_TOTAL_DRIFT}). A gate that measures nothing \
         passes loudly: this is the check that says the scan still finds the 1,244 rows that \
         were there on 2026-08-17 (1,268 before the `p62` navigable fix). If drift really was \
         reduced below the floor, that is very good news, and the floor is what to re-take — \
         after `the_drift_baseline_has_no_stale_rows` has told you which rows went.",
        a.drift.len()
    );

    // --- M12: the blind region is bounded even though it is not visible ----
    let blind: usize = a
        .unresolved
        .iter()
        .filter(|(reason, _)| reason.as_str() != "arity<4")
        .map(|(_, n)| *n)
        .sum();
    assert!(
        blind <= MAX_BLIND_SITES,
        "{blind} register sites could not be resolved and are NOT the proven-benign \
         `arity<4` receivers (ceiling {MAX_BLIND_SITES}, measured 950 on 2026-08-17). \
         Breakdown: {:?}.\n\
         Drift arriving through one of these forms -- a `format!`-built descriptor, a \
         class-parameterised registrar, a tuple `for` loop, an unbound identifier -- is \
         INVISIBLE to `no_new_mode_drift`. This assertion cannot see that drift either; \
         all it says is that the region where it could hide has not grown. If a change \
         needs to grow it, the honest move is to teach the resolver the new form, not to \
         raise this number.

\n         WHERE THE BLIND REGION IS, worst first -- the registrars to teach it \n         about next:
{}",
        a.unresolved,
        {
            let mut v: Vec<(&String, &usize)> = a
                .unresolved_where
                .iter()
                .filter(|(k, _)| !k.starts_with("arity<4|"))
                .collect();
            v.sort_by(|x, y| y.1.cmp(x.1).then(x.0.cmp(y.0)));
            v.into_iter()
                .take(15)
                .map(|(k, n)| {
                    let mut it = k.split('|');
                    let why = it.next().unwrap_or("");
                    let file = it.next().unwrap_or("");
                    let f = it.next().unwrap_or("");
                    format!("           {n:>5}  {why:<20} {f}  ({file})")
                })
                .collect::<Vec<_>>()
                .join("
")
        }
    );

    // --- the analysis's own two views of drift must agree ------------------
    // `per_pass` is accumulated while the drift map is built; `drift` is the
    // map. If a future edit changes one and not the other, every per-pass
    // message in this file starts pointing at the wrong pass while the totals
    // stay plausible — the failure mode that is hardest to notice.
    let pairs_from_drift: usize = a.drift.values().map(|(so, _)| so.len()).sum();
    let pairs_from_per_pass: usize = a.per_pass.values().sum();
    assert_eq!(
        pairs_from_drift, pairs_from_per_pass,
        "the scanner's two accounts of (synthetic-only pass, triple) pairs disagree: \
         {pairs_from_drift} counted from `drift`, {pairs_from_per_pass} from `per_pass`. \
         They are built in the same loop; one of them has been edited alone."
    );
    let mut worst: Vec<(&usize, &String)> = a.per_pass.iter().map(|(k, v)| (v, k)).collect();
    worst.sort();
    worst.reverse();
    println!(
        "registrar-drift: worst drifting passes: {:?}",
        worst.iter().take(12).collect::<Vec<_>>()
    );

    // --- the two-sided control -------------------------------------------
    let pos = triple(CONTROL_POSITIVE);
    let neg = triple(CONTROL_NEGATIVE);
    assert!(
        a.drift.contains_key(&pos),
        "POSITIVE CONTROL FAILED: `{}.{}{}` is not reported as drifting.\n\
         Either the twin was collapsed onto one implementation — in which case say so in the \
         record, move its rows to FIXED_NOT_DRIFTING and re-point this control at another \
         instance that is still drifting (pick one from DRIFT_TRIPLES, and prefer a family \
         no open nomination proposes to touch) — or the scanner has stopped discriminating \
         and every other assertion in this file is worthless.\n    {}",
        pos.0,
        pos.1,
        pos.2,
        show(a, &pos)
    );
    assert!(
        !a.drift.contains_key(&neg),
        "NEGATIVE CONTROL FAILED: `{}.{}{}` is reported as drifting.\n\
         This is F34-1's worked example AFTER its fix: F16 deleted the whole group-layout \
         family from `panama.rs::register_pe2_struct_layouts` so \
         `phases_late/foreign_ffm.rs::register_p67_foreign_memory` is the sole registrant. If \
         this fires, a synthetic-only twin came back, and the two files carry two different \
         layout objects behind one descriptor — read the comment at `panama.rs:5829` before \
         doing anything else.\n    {}",
        neg.0,
        neg.1,
        neg.2,
        show(a, &neg)
    );
    assert!(
        a.registrants.contains_key(&neg),
        "NEGATIVE CONTROL IS VACUOUS: `{}.{}{}` is not registered by ANY pass this scan can \
         see. The control passes for the wrong reason — the resolver lost the triple rather \
         than the drift being absent. Repair the resolver before trusting any number here.",
        neg.0,
        neg.1,
        neg.2
    );

    for &(c, n, d, how) in RESOLVER_WITNESSES {
        let t = triple((c, n, d));
        assert!(
            a.registrants.contains_key(&t),
            "RESOLVER WITNESS MISSING: `{c}.{n}{d}` was not recovered. It is recovered through \
             {how}; if it is gone, that resolution path has broken silently and the drift \
             number is understated by an unknown amount."
        );
    }
}

/// [`DRIFT_TRIPLES`] as `pass -> set of triples`, which is the shape every
/// assertion below wants. Built once per call; the table is a few thousand
/// short `&str`s and the tests that use it run four times at most.
fn baseline_by_pass() -> BTreeMap<&'static str, BTreeSet<Triple>> {
    let mut m: BTreeMap<&'static str, BTreeSet<Triple>> = BTreeMap::new();
    for &(pass, rows) in DRIFT_TRIPLES {
        let e = m.entry(pass).or_default();
        for &(c, n, d) in rows {
            e.insert(triple((c, n, d)));
        }
    }
    m
}

/// [`DRIFT_TRIPLES`] and its two totals, regenerated from what THIS RUN
/// measured, in paste-ready Rust.
///
/// This exists because of the way the 2026-08-16 gate was written: its baseline
/// came from a Python mirror, it had never been compiled, and its own §9 had to
/// warn the first runner to "re-take the baseline from the gate's own printed
/// output before assuming a regression". That warning is worth nothing without
/// the output. Here it is.
///
/// `cargo test` prints a failing test's captured stdout, so calling this from a
/// `println!` immediately before the assertion puts the replacement table in
/// front of whoever is reading the failure, without making the panic message
/// itself two thousand lines long.
fn retake(a: &Analysis) -> String {
    let mut by_pass: BTreeMap<&str, BTreeSet<&Triple>> = BTreeMap::new();
    for (t, (so, _)) in &a.drift {
        for p in so {
            by_pass.entry(p.as_str()).or_default().insert(t);
        }
    }
    let pairs: usize = by_pass.values().map(|v| v.len()).sum();
    let mut s = String::new();
    s.push_str("\n// ======== PASTE-READY, regenerated from this run ========\n");
    s.push_str("// Replace BASELINE_TOTAL_DRIFT, BASELINE_TOTAL_PAIRS and DRIFT_TRIPLES\n");
    s.push_str("// with everything between these markers, then say in the record WHICH\n");
    s.push_str("// rows moved and why. A re-take with no explanation is how a ratchet\n");
    s.push_str("// becomes a rubber stamp.\n");
    s.push_str(&format!(
        "const BASELINE_TOTAL_DRIFT: usize = {};\n",
        a.drift.len()
    ));
    s.push_str(&format!("const BASELINE_TOTAL_PAIRS: usize = {pairs};\n"));
    s.push_str("const DRIFT_TRIPLES: &[(&str, &[(&str, &str, &str)])] = &[\n");
    for (p, ts) in &by_pass {
        s.push_str(&format!("    (\n        \"{p}\",\n        &[\n"));
        for t in ts {
            s.push_str(&format!(
                "            (\"{}\", \"{}\", \"{}\"),\n",
                t.0, t.1, t.2
            ));
        }
        s.push_str("        ],\n    ),\n");
    }
    s.push_str("];\n// ======== END PASTE-READY ========\n");
    s
}

/// The baseline must stay a table of measured debts, not a wish list.
///
/// Everything here is checked against the TABLE, not against the tree, except
/// the last block — so a table edited into nonsense fails even if the scanner
/// is broken as well.
#[test]
fn the_baseline_is_well_formed() {
    let mut seen_pass: BTreeSet<&str> = BTreeSet::new();
    let mut distinct: BTreeSet<Triple> = BTreeSet::new();
    let mut pairs = 0usize;
    for &(pass, rows) in DRIFT_TRIPLES {
        assert!(
            seen_pass.insert(pass),
            "`{pass}` appears twice in DRIFT_TRIPLES. The second block is silently ignored \
             by every lookup below, so half its rows would read as NEW drift."
        );
        assert!(
            !rows.is_empty(),
            "`{pass}` has an EMPTY row list in DRIFT_TRIPLES. An empty list is \
             indistinguishable from absence and gives the false impression that the pass was \
             examined; delete the entry instead."
        );
        let mut here: BTreeSet<Triple> = BTreeSet::new();
        for &(c, n, d) in rows {
            let t = triple((c, n, d));
            assert!(
                here.insert(t.clone()),
                "`{pass}` lists `{c}.{n}{d}` twice; BASELINE_TOTAL_PAIRS counts rows, so a \
                 duplicate row silently buys an allowance that no measurement backs."
            );
            distinct.insert(t);
        }
        pairs += rows.len();
    }
    assert_eq!(
        pairs, BASELINE_TOTAL_PAIRS,
        "DRIFT_TRIPLES holds {pairs} (pass, triple) pairs but BASELINE_TOTAL_PAIRS says \
         {BASELINE_TOTAL_PAIRS}. One was edited without re-taking the other — which is the \
         exact failure the 2026-08-16 gate's two independent numbers (a per-pass table and a \
         global total) could not detect."
    );
    assert_eq!(
        distinct.len(),
        BASELINE_TOTAL_DRIFT,
        "DRIFT_TRIPLES holds {} distinct triples but BASELINE_TOTAL_DRIFT says \
         {BASELINE_TOTAL_DRIFT}.",
        distinct.len()
    );
    assert!(
        BASELINE_TOTAL_DRIFT > MIN_TOTAL_DRIFT,
        "MIN_TOTAL_DRIFT ({MIN_TOTAL_DRIFT}) must sit BELOW BASELINE_TOTAL_DRIFT \
         ({BASELINE_TOTAL_DRIFT}); a floor at or above the ceiling can never both hold"
    );

    // A triple cannot be both a recorded debt and a recorded fix.
    for &(c, n, d) in FIXED_NOT_DRIFTING {
        let t = triple((c, n, d));
        assert!(
            !distinct.contains(&t),
            "`{c}.{n}{d}` is in BOTH FIXED_NOT_DRIFTING and DRIFT_TRIPLES. \
             `the_fixed_twins_stay_fixed` and `no_new_mode_drift` then assert opposite \
             things about it and one of them is guaranteed to be wrong."
        );
    }
    // A row read and found LIVE must be a row the baseline knows about, or the
    // two tables disagree about what is being permitted.
    for &(c, n, d, _) in MUST_DRIFT {
        let t = triple((c, n, d));
        assert!(
            distinct.contains(&t),
            "`{c}.{n}{d}` is pinned in MUST_DRIFT but is absent from DRIFT_TRIPLES. \
             `the_known_live_twins_still_drift` demands it drift; `no_new_mode_drift` \
             reports it as NEW drift the moment it does."
        );
    }
    let pos = triple(CONTROL_POSITIVE);
    assert!(
        distinct.contains(&pos),
        "CONTROL_POSITIVE `{}.{}{}` is absent from DRIFT_TRIPLES, so the positive control \
         and the ratchet contradict each other: the control demands it drift and the ratchet \
         calls that drift new.",
        pos.0,
        pos.1,
        pos.2
    );
    let neg = triple(CONTROL_NEGATIVE);
    assert!(
        !distinct.contains(&neg),
        "CONTROL_NEGATIVE `{}.{}{}` appears in DRIFT_TRIPLES, so the baseline permits the \
         very row the negative control forbids.",
        neg.0,
        neg.1,
        neg.2
    );

    let a = analysis();
    // A baseline row naming a pass that is no longer synthetic-only is a stale
    // exemption, and a stale exemption is a hole: the allowance would then be
    // granted to a pass that can never drift, quietly covering for a different
    // one.
    let stale: Vec<&str> = DRIFT_TRIPLES
        .iter()
        .map(|&(n, _)| n)
        .filter(|n| !a.synthetic_only.contains(*n))
        .collect();
    assert!(
        stale.is_empty(),
        "these DRIFT_TRIPLES entries name passes that are NOT synthetic-only any more: \
         {stale:?}\n\
         Good news if they were promoted onto the shipping path — delete the entries and \
         re-take the totals. Bad news if the reachability scan broke, in which case fix that \
         first: `registrar_reachability.rs` is the gate that owns that question."
    );
}

/// The ratchet, forward direction. **New drift fails.**
///
/// # What this catches
///
/// * a NEW `(class, name, descriptor)` registered by both a synthetic-only pass
///   and a shipping pass — the species this file exists for;
/// * a synthetic-only pass that starts drifting at all (a pass absent from
///   [`DRIFT_TRIPLES`] has an allowance of zero);
/// * an existing family drifting on a triple it did not drift on before;
/// * **a drifting triple swapped for another inside one pass at the same
///   count** — M11 in the 2026-08-16 mutation table, where it was listed as
///   catching NOTHING. The baseline is a set now, so the substitute is new;
/// * a triple whose drift moves from one synthetic-only pass to another.
///
/// # What this provably does NOT catch
///
/// * **Anything the resolver cannot see** — `format!` descriptors,
///   class-parameterised registrars, tuple `for` loops, array-const iteration.
///   New drift arriving through one of those is invisible here. What
///   [`MAX_BLIND_SITES`] adds is only that the blind region cannot grow; it
///   still cannot see into it.
/// * **Kind drift** (M13). Two registrations of one triple with the SAME body
///   and different `NativeKind` behave differently under `--jdk-only`, because
///   `SyntheticStub` is refused there (`native-api/src/registry.rs`
///   `allowed_in`). Kind is ambient registry state threaded through call
///   chains; no source scan resolves it, and this one does not try. It is not
///   hypothetical: `AtomicBoolean.get` and `Instant.getEpochSecond` are both in
///   [`MUST_DRIFT`], both were confirmed `kind = synthetic-stub` in a
///   compatible-mode registry dump, and both are ABSENT from the `--jdk-only`
///   dump — so for those, `--jdk-only` runs neither copy and real JDK bytecode
///   serves the call. **A row in this baseline is a claim that two
///   registrations exist. It is not a claim about which body runs.**
/// * **Which body wins when both are registered.** That is registration ORDER
///   at runtime, and only `--dump-native-registry` answers it (`owns_slot`).
///   Note also that `invocations == 0` in that dump does NOT prove a body dead:
///   the counter misses intrinsic-cached and JIT direct-call dispatch, and is
///   exact only under `--nojit` with `CRATONVM_DISABLE_INTRINSICS=1`.
#[test]
fn no_new_mode_drift() {
    let a = analysis();
    let base = baseline_by_pass();

    let mut new_rows: Vec<String> = Vec::new();
    for (t, (so, sh)) in &a.drift {
        for p in so {
            let known = base.get(p.as_str()).map_or(false, |s| s.contains(t));
            if known {
                continue;
            }
            if new_rows.len() < 40 {
                new_rows.push(format!(
                    "  {} (defined at {})\n      {}.{}{}\n      synthetic-only: {:?}\n      \
                     shipping: {:?}",
                    p,
                    a.where_defined
                        .get(p)
                        .cloned()
                        .unwrap_or_else(|| "<unknown>".to_string()),
                    t.0,
                    t.1,
                    t.2,
                    so.iter().collect::<Vec<_>>(),
                    sh.iter().collect::<Vec<_>>()
                ));
            } else {
                new_rows.push(String::new());
            }
        }
    }
    let shown = new_rows.iter().filter(|s| !s.is_empty()).count();
    let total_new = new_rows.len();
    if total_new > 0 {
        println!("{}", retake(a));
    }

    assert!(
        new_rows.is_empty(),
        "NEW MODE DRIFT — {total_new} (pass, triple) pair(s), showing {shown}.\n\n{}\n\n\
         Each line is a `(class, name, descriptor)` registered by BOTH a synthetic-only \
         pass and a shipping pass, which the 2026-08-17 baseline did not record. \
         `register()` is last-write-wins and `register_synthetic_overrides` runs last, so \
         synthetic-JDK mode gets one body and the shipping modes get the other — and every \
         test built with `--features synthetic-jdk` measures the copy that does not ship.\n\n\
         Three questions, in order:\n\
         1. Do the two bodies agree? Read both. If they are the same free function under \
            two names, say so in the record; if they differ, the shipping one is the one \
            nobody tested. `--dump-native-registry` is the instrument: `owns_slot` says \
            which body holds the slot, and it is trustworthy.\n\
         2. If they differ, which should survive? Deleting the synthetic-only copy is NOT \
            automatically right — F34-1 §5 records that dropping `register_pe_panama` \
            would have taken 14 triples with it that its twin does not register. Prove the \
            shipping body serves the triples in BOTH modes from a dump before removing \
            anything.\n\
         3. If both must stay, paste the regenerated DRIFT_TRIPLES printed above and say \
            in the record which rows you added and why.\n\n\
         If this is the FIRST run of this file: the baseline came from a Python \
         transliteration of this scanner, never from this scanner. A handful of unexpected \
         rows is that disagreement and the right move is to re-take from the block above. \
         Hundreds of rows is not, and `the_drift_scanner_is_not_vacuous` should be read \
         first.",
        new_rows
            .iter()
            .filter(|s| !s.is_empty())
            .cloned()
            .collect::<Vec<_>>()
            .join("\n")
    );
}

/// The ratchet, reverse direction. **A debt that has been paid must be
/// recorded.**
///
/// `registrar_reachability.rs` ratchets both ways on purpose: a name leaving
/// its allow-list is good news and still fails, because a stale exemption is a
/// hole. The 2026-08-16 version of this file deliberately did not, on the
/// grounds that it had never been compiled and an equality would be a coin flip
/// on the first run. That reason is still partly true — see
/// [`DRIFT_TRIPLES`] — but the fix for it is the paste-ready printer, not a
/// permanently one-sided gate. Its own N7 asked for this.
///
/// A row here going stale means one of three things, and they are not
/// interchangeable:
///
/// 1. **the drift was fixed** — move the triple to [`FIXED_NOT_DRIFTING`] with
///    the dump evidence, exactly as the 24 `TreeMap`/`TreeSet` navigation rows
///    were on 2026-08-17;
/// 2. **the pass was promoted onto the shipping path** —
///    `the_baseline_is_well_formed` catches that separately and says so;
/// 3. **the scanner stopped seeing it**, which is not good news at all and is
///    what the floors in [`the_drift_scanner_is_not_vacuous`] exist to
///    separate out.
#[test]
fn the_drift_baseline_has_no_stale_rows() {
    let a = analysis();
    let base = baseline_by_pass();

    let mut stale: Vec<String> = Vec::new();
    for (&pass, rows) in &base {
        for t in rows {
            let still = a.drift.get(t).map_or(false, |(so, _)| so.contains(pass));
            if still {
                continue;
            }
            if stale.len() < 40 {
                stale.push(format!(
                    "  {pass}\n      {}.{}{}\n    {}",
                    t.0,
                    t.1,
                    t.2,
                    show(a, t)
                ));
            } else {
                stale.push(String::new());
            }
        }
    }
    let total = stale.len();
    if total > 0 {
        println!("{}", retake(a));
    }
    assert!(
        stale.is_empty(),
        "STALE BASELINE — {total} recorded drift pair(s) no longer drift.\n\n{}\n\n\
         This is usually good news and it still fails, for the same reason \
         `registrar_reachability.rs` fails on a stale allow-list entry: an allowance nobody \
         is using covers for the next row that needs one, and nothing else would ever \
         report it.\n\n\
         If a twin was collapsed onto one implementation, move its triples to \
         FIXED_NOT_DRIFTING — with the `--dump-native-registry` evidence that the surviving \
         body serves them in BOTH modes — and paste the regenerated DRIFT_TRIPLES printed \
         above, in the same commit as the source change.\n\n\
         If instead the triple has gone missing from the census entirely, the `show()` block \
         above says `<not registered anywhere this scan can see>` and this is a RESOLVER \
         failure wearing a fix's clothes. Read `the_drift_scanner_is_not_vacuous` first.",
        stale
            .iter()
            .filter(|s| !s.is_empty())
            .cloned()
            .collect::<Vec<_>>()
            .join("\n")
    );
}

/// The rows whose two implementations were READ and found to differ.
///
/// A set ratchet still cannot tell "these two bodies disagree" from "these two
/// names point at one function". These four were diffed by hand and three of
/// them were then checked against a registry dump, so they are pinned
/// individually: if one stops drifting, either it was fixed — move it to
/// [`FIXED_NOT_DRIFTING`] and say so — or the scanner stopped seeing it, which
/// is worse.
#[test]
fn the_known_live_twins_still_drift() {
    let a = analysis();
    let mut missing: Vec<String> = Vec::new();
    for &(c, n, d, why) in MUST_DRIFT {
        let t = triple((c, n, d));
        if !a.drift.contains_key(&t) {
            missing.push(format!(
                "  {c}.{n}{d}\n      why it mattered: {why}\n    {}",
                show(a, &t)
            ));
        }
    }
    assert!(
        missing.is_empty(),
        "these triples were measured as LIVE mode drift (two implementations that behave \
         differently) and are no longer reported as drifting:\n{}\n\n\
         If the twin was genuinely collapsed onto one implementation, move the row to \
         FIXED_NOT_DRIFTING and re-take DRIFT_TRIPLES in the same commit. If it was not, \
         this scanner has stopped resolving something and the whole number is understated.",
        missing.join("\n")
    );
}

/// The rows whose twin was COLLAPSED, pinned so the collapse cannot come back.
///
/// Two assertions, and the second is the one that matters:
///
/// * each triple must be ABSENT from the drift set — the fix held;
/// * each triple must be PRESENT in the census — the fix is being observed, not
///   merely unobservable. Without this half the test passes for the wrong
///   reason the moment the resolver loses the triple, which is the "confident,
///   vacuous zero" F34-1 §2.1 recorded twice and §5.1 of the 2026-08-16 record
///   guarded the negative control against.
#[test]
fn the_fixed_twins_stay_fixed() {
    let a = analysis();
    let mut vanished: Vec<String> = Vec::new();
    let mut returned: Vec<String> = Vec::new();
    for &(c, n, d) in FIXED_NOT_DRIFTING {
        let t = triple((c, n, d));
        if !a.registrants.contains_key(&t) {
            vanished.push(format!("  {c}.{n}{d}"));
        } else if a.drift.contains_key(&t) {
            returned.push(format!("  {c}.{n}{d}\n    {}", show(a, &t)));
        }
    }
    assert!(
        vanished.is_empty(),
        "THIS TEST IS VACUOUS: these triples are not registered by ANY pass this scan can \
         see, so 'they no longer drift' is true for the wrong reason — the resolver lost \
         them.\n{}\n\n\
         The 24 navigation rows are registered by `native-collections/src/lib.rs` \
         (`register_tree_map_natives` / `register_tree_set_natives`) and the 12 \
         `ByteArrayOutputStream` rows by `native-io/src/lib.rs` (`register_io_natives`, \
         which binds 13 — these 12 plus `write([B)V`). All 36 were confirmed present with \
         `kind = bridge` and `owns_slot = true` in BOTH a compatible-mode and a \
         `--jdk-only` `--dump-native-registry`. If the scan cannot see them, repair the \
         resolver before trusting any number in this file.",
        vanished.join("\n")
    );
    assert!(
        returned.is_empty(),
        "A COLLAPSED TWIN CAME BACK:\n{}\n\n\
         These triples were registered by BOTH \
         `native-builtins/src/phases_late/collections.rs::register_p62_navigable_expansion` \
         (synthetic-only, comparator-blind linear scans over the slot-0 interleaved array) \
         and `native-collections/src/lib.rs` (`native_tm_*` / `native_ts_*`, which honour a \
         user Comparator, sync native state, take a BTree fast path, and refresh `data` \
         after a comparator call because that call can move the heap). The synthetic-only \
         arms were deleted on 2026-08-17 after a registry dump showed the shipping bodies \
         own all 24 slots in both modes.\n\n\
         If a second registrant is back, read \
         `native-builtins/src/phases_late/collections.rs::register_p62_navigable_expansion` \
         — the deleted family and the reason are documented on it — before assuming the new \
         one is the good copy. For a `ByteArrayOutputStream` row, read \
         `native-builtins/src/serialization.rs::register_byte_array_output_stream`, which \
         is deliberately empty for the same kind of reason: a returning `close`/`flush` \
         no-op would silently skip `process_pipe_output_close` and the fd-table flush.",
        returned.join("\n")
    );
}

// ===========================================================================
// SECTION 5 — the cross-check against `registrar_reachability.rs`
// ===========================================================================

/// Path to the reachability gate, read at run time by
/// [`the_two_gates_agree_on_the_synthetic_only_population`].
const REACHABILITY_GATE: &str = "tests/registrar_reachability.rs";

/// Names this file calls synthetic-only and the other gate does not, or the
/// reverse, each with the reason the two scanners legitimately differ.
///
/// The two are not the same program. This one scans SEVEN crates' `src` trees
/// and roots its shipping closure on module-level references; the other scans
/// `native-builtins/src` for definitions and roots its shipping closure on
/// pass names mentioned anywhere else in the workspace. So an exact match is
/// not the right assertion — an exactly ENUMERATED difference is.
///
/// Every row below was checked by hand on 2026-08-17 and in every one of them
/// the OTHER gate is right and this one over-reads `shipping`:
///
/// * the five `use`-import rows: SECTION 3's module-level scan counts a pass
///   named in a `use crate::…::register_x;` item as a non-pass reference, and
///   therefore as a shipping root. But an import is not a call. All five are
///   called from exactly one place — `register_phase52_natives`,
///   `register_phase53_natives`, `register_phase60_natives` (twice) or
///   `register_phase65_natives` — every one of which is itself a direct
///   synthetic-only child of `register_synthetic_overrides`. Their real call
///   sites are already captured by the in-function scan, so the `use` rule
///   buys nothing and costs five false shipping roots. Consequence: their
///   triples are excluded from the drift census, so [`BASELINE_TOTAL_DRIFT`]
///   is an UNDER-count by whatever they share with a shipping pass.
/// * `register_synthetic_socket_stubs`: this scan reads `vm/src/vm/tests.rs`,
///   which is `#[cfg(all(test, feature = "synthetic-jdk"))] mod tests;` at
///   `vm/src/vm.rs:59-60`. Its line 582 is the pass's only reference outside
///   `native-builtins/src`, and a test — especially one compiled only under
///   `synthetic-jdk` — is not a shipping call site. The other gate closed this
///   on 2026-08-17; this one has not, because doing so moves the census and a
///   census move is a re-take, not a cross-check.
/// * `register_synthetic_overrides` itself: structural, not a defect. This
///   scan seeds the synthetic closure WITH the root and keeps it; the other
///   removes it. It registers triples directly in its own body, which is why
///   it appears in [`DRIFT_TRIPLES`] as a pass in its own right.
///
/// Repairing the first two is a re-take of [`DRIFT_TRIPLES`] and belongs in
/// its own commit — see `docs/known-issues/jdk-only/G54-1-…-20260817.md` N1.
/// Until then this table is the honest statement of where the two disagree,
/// and a SIXTH name appearing on either side fails.
const KNOWN_POPULATION_DIVERGENCE: &[(&str, &str)] = &[
    (
        "register_p60_callsite",
        "this scan only: `use` import at phases_late.rs:49 read as a shipping root; sole \
         caller is register_phase60_natives (synthetic-only)",
    ),
    (
        "register_p60_record",
        "this scan only: `use` import at phases_late.rs:52 read as a shipping root; sole \
         caller is register_phase60_natives (synthetic-only)",
    ),
    (
        "register_p65_method_handles_extra",
        "this scan only: `use` import at phases_late.rs:49 read as a shipping root; sole \
         caller is register_phase65_natives (synthetic-only)",
    ),
    (
        "register_phase52_string_buffer",
        "this scan only: `use` import at phases_early.rs:47 read as a shipping root; sole \
         caller is register_phase52_natives (synthetic-only)",
    ),
    (
        "register_phase53_record",
        "this scan only: `use` import at phases_early.rs:44 read as a shipping root; sole \
         caller is register_phase53_natives (synthetic-only)",
    ),
    (
        "register_synthetic_socket_stubs",
        "this scan only: vm/src/vm/tests.rs:582 read as a shipping root, but that file is \
         `#[cfg(all(test, feature = \"synthetic-jdk\"))] mod tests;`",
    ),
    (
        "register_synthetic_overrides",
        "STRUCTURAL, not a defect: this scan keeps the closure's root in the set and the \
         other removes it. It registers triples in its own body.",
    ),
];

/// Pull a `const NAME: &[&str] = &[ "a", "b", … ];` list out of Rust source.
///
/// Deliberately dumb, and floored by the caller so that dumbness cannot pass
/// as agreement: it takes every string literal between `= &[` and the matching
/// `];`. Comments are skipped so a commented-out row does not count.
fn str_list_after(src: &str, decl: &str) -> Vec<String> {
    let Some(start) = src.find(decl) else {
        return Vec::new();
    };
    let Some(open) = src[start..].find("= &[").map(|o| start + o + 4) else {
        return Vec::new();
    };
    let b = src.as_bytes();
    let mut out = Vec::new();
    let mut depth = 1usize;
    let mut i = open;
    while i < b.len() && depth > 0 {
        match b[i] {
            b'/' if i + 1 < b.len() && b[i + 1] == b'/' => {
                while i < b.len() && b[i] != b'\n' {
                    i += 1;
                }
            }
            b'[' => {
                depth += 1;
                i += 1;
            }
            b']' => {
                depth -= 1;
                i += 1;
            }
            b'"' => {
                let mut j = i + 1;
                let mut lit = String::new();
                while j < b.len() {
                    if b[j] == b'\\' {
                        if j + 1 < b.len() {
                            lit.push(b[j + 1] as char);
                        }
                        j += 2;
                        continue;
                    }
                    if b[j] == b'"' {
                        break;
                    }
                    lit.push(b[j] as char);
                    j += 1;
                }
                out.push(lit);
                i = j + 1;
            }
            _ => i += 1,
        }
    }
    out
}

/// **The two gates must agree about the POPULATION, and until 2026-08-17
/// nobody had ever asked.**
///
/// `registrar_reachability.rs` decides which passes are synthetic-only; this
/// file decides which triples those passes share with a shipping pass. The
/// second question is meaningless if the two disagree about the first, and
/// nothing compared them — so when reachability's own census went wrong, this
/// gate stayed green and said nothing.
///
/// It had gone wrong. On 2026-08-17 an untracked `scratch/` directory holding
/// a stray copy of `phases_late.rs` was being walked by that file's
/// whole-workspace reference scan, injecting 203 spurious shipping roots; it
/// reported 54 synthetic-only families where this scan reports 73, and 169
/// synthetic-only passes where it pins 284. All three of its ratchets were red
/// and this gate was entirely green, because nothing here reads that file.
///
/// Two assertions:
///
/// 1. **The 73 direct families must match exactly.** This is not a formality:
///    two independently written scanners, different crate scopes, different
///    shipping-root rules, agreeing name-for-name is the strongest evidence
///    either file has that its reachability half is right. MEASURED
///    2026-08-17: identical, 73/73.
/// 2. **The transitive closures may differ only by
///    [`KNOWN_POPULATION_DIVERGENCE`]**, which enumerates all seven names and
///    says which scanner is wrong about each. A drift in either direction that
///    is not on that list fails.
///
/// The reverse direction lives in that file, as
/// `the_drift_gate_agrees_about_family_drift_exposure`: it re-derives each
/// family's drift exposure from [`DRIFT_TRIPLES`]. Between them, neither
/// gate's table can be re-taken without the other noticing.
#[test]
fn the_two_gates_agree_on_the_synthetic_only_population() {
    let a = analysis();
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(REACHABILITY_GATE);
    let src = std::fs::read_to_string(&path).unwrap_or_else(|e| {
        panic!(
            "cannot read the reachability gate at {}: {e}. This cross-check is the only \
             thing comparing the two gates; if the file moved, re-point REACHABILITY_GATE \
             rather than deleting the test.",
            path.display()
        )
    });

    let families = str_list_after(&src, "const DELIBERATE_SYNTHETIC_ONLY_FAMILIES");
    // That list is `(name, reason)` pairs, so the pass names are the even
    // elements; a reason never starts with `register_`.
    let their_families: BTreeSet<&str> = families
        .iter()
        .map(String::as_str)
        .filter(|s| s.starts_with("register_"))
        .collect();
    let their_closure: BTreeSet<&str> = str_list_after(&src, "const SYNTHETIC_ONLY_CLOSURE")
        .iter()
        .map(|s| Box::leak(s.clone().into_boxed_str()) as &str)
        .collect();

    // --- non-vacuity: a parse that finds nothing agrees with everything ----
    assert!(
        their_families.len() >= 50 && their_closure.len() >= 200,
        "parsed only {} families / {} closure entries out of {REACHABILITY_GATE}. Those \
         lists held 73 and 285 on 2026-08-17, so this parse is broken and both assertions \
         below would pass by finding nothing.",
        their_families.len(),
        their_closure.len()
    );

    // --- 1. the direct families -------------------------------------------
    let ours: BTreeSet<&str> = a
        .direct_synthetic_only_set
        .iter()
        .map(String::as_str)
        .collect();
    let only_here: Vec<&str> = ours.difference(&their_families).copied().collect();
    let only_there: Vec<&str> = their_families.difference(&ours).copied().collect();
    assert!(
        only_here.is_empty() && only_there.is_empty(),
        "the two gates disagree about the DIRECT synthetic-only children of \
         `{SYNTHETIC_OVERRIDES}`.\n  \
         this scan sees, {REACHABILITY_GATE} does not: {only_here:?}\n  \
         {REACHABILITY_GATE} pins, this scan does not: {only_there:?}\n\n\
         These two scanners are independent, so a disagreement is a real signal and not a \
         formatting difference. Read `the_scanner_is_not_vacuous` in BOTH files before \
         editing either list: on 2026-08-17 the cause was an untracked `scratch/` directory \
         being walked as workspace source by the other gate, which reported 54 families \
         instead of 73 while this gate stayed green."
    );

    // --- 2. the transitive closure ----------------------------------------
    let allowed: BTreeSet<&str> = KNOWN_POPULATION_DIVERGENCE
        .iter()
        .map(|&(n, _)| n)
        .collect();
    let ours_all: BTreeSet<&str> = a.synthetic_only.iter().map(String::as_str).collect();
    let mut unexplained: Vec<String> = Vec::new();
    for n in ours_all.difference(&their_closure) {
        if !allowed.contains(n) {
            unexplained.push(format!(
                "  {n}: synthetic-only HERE, absent from {REACHABILITY_GATE}'s closure \
                 (defined at {})",
                a.where_defined
                    .get(*n)
                    .map(String::as_str)
                    .unwrap_or("unknown")
            ));
        }
    }
    for n in their_closure.difference(&ours_all) {
        if !allowed.contains(n) {
            unexplained.push(format!(
                "  {n}: pinned synthetic-only by {REACHABILITY_GATE}, SHIPPING-reachable here"
            ));
        }
    }
    assert!(
        unexplained.is_empty(),
        "the two gates' synthetic-only closures diverge on names that \
         KNOWN_POPULATION_DIVERGENCE does not explain:\n{}\n\n\
         Every existing row on that list is a case where THIS scan over-reads `shipping` — \
         five `use`-import roots, one test-file root, and the closure root itself. A new \
         name is not automatically the same species. Decide which scanner is right, say so \
         in the record, and add the row with its reason; an unexplained row here is an \
         exemption nobody measured.",
        unexplained.join("\n")
    );

    // A stale divergence row is a hole in exactly the way a stale allow-list
    // entry is: it pre-authorises a disagreement that is no longer happening,
    // and would silently cover a different one arriving under the same name.
    let live: BTreeSet<&str> = ours_all
        .symmetric_difference(&their_closure)
        .copied()
        .collect();
    let stale: Vec<&str> = allowed.difference(&live).copied().collect();
    assert!(
        stale.is_empty(),
        "KNOWN_POPULATION_DIVERGENCE lists {stale:?}, but the two gates now AGREE about \
         them. Good news that still has to be recorded: delete the rows, and if the repair \
         was to this file's shipping-root rules, re-take DRIFT_TRIPLES in the same commit \
         because the census moved."
    );
}
