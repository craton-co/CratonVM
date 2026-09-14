// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! WIRING RATCHET — natives that MUST be reachable from the real-JDK boot path,
//! because there is no bytecode underneath them to fall back to.
//!
//! # The species
//!
//! `--features synthetic-jdk` is a BUILD decision; `--jdk-only` / `--real-jdk`
//! are RUNTIME modes. `register_builtins` -> `register_synthetic_overrides` is
//! `#[cfg(feature = "synthetic-jdk")]`, and `synthetic-jdk` is in no crate's
//! default feature set — so a registrar reachable only from there is not
//! "present and declined by policy" in a shipping `cratonvm-cli` build. **It is
//! not in the binary at all.** A reader who sees `register(...)` calls for
//! `java/util/stream/IntStream` reasonably concludes the surface is covered and
//! that strict mode merely chooses whether to use it. It is not there to
//! choose. See docs/architecture/natives-over-real-jdk-classes.md §2, and
//! docs/known-issues/jdk-only/W7-5-registrars-that-never-shipped.md for the
//! census: 301 registrar functions absent from the default build, carrying
//! 4,264 `register(...)` call sites.
//!
//! Most of those 301 are correctly gated, and this file deliberately does NOT
//! assert about them: for a real JDK class with working bytecode, wiring the
//! registrar in does not add coverage, it REPLACES working JDK code with a
//! partial Rust reimplementation (W7-5 §3.1, and `Integer.toString(II)` for
//! what that buys). The rows here are the one bucket where the gate is wrong:
//!
//! > **CratonVM mints instances of the real class itself, the declaration on
//! > that class is ABSTRACT, so a missing registration is an
//! > `AbstractMethodError: … has no Code attribute` with no bytecode fallback.**
//!
//! `native-collections`' `make_int_stream` allocates through
//! `try_alloc_synthetic(ctx, "java/util/stream/IntStream", ..)`, so the
//! receiver's runtime class IS the real JDK interface. There is no
//! `AbstractPipeline` underneath it. That is how
//! `IntStream.rangeClosed(1,5).summaryStatistics()` came to kill whole probe
//! runs under `--real-jdk` while the registration "existed".
//!
//! # Why this file exists rather than a comment
//!
//! W7-5 §6.3 specified this test and it was never written, so the regression
//! that produced the record could recur silently. It already recurred once in a
//! worse shape: `Stream.forEachOrdered` was not fixed by registering the
//! native, it was fixed by adding a hardcoded method-name special case to the
//! interpreter's `!has_code` fallback (`vm/src/runtime/interpreter.rs`). **A
//! dead registrar pushed a workaround into the dispatch core.** Five further
//! live sites carry a comment naming a synthetic-only registrar as the reason
//! they exist, each one method hand-copied out for the one workload that
//! reached it (W7-5 §4.1).
//!
//! The shape of the guard is not new either — `reflect_annotations.rs`'s
//! `module_can_read_essential_tests` is the same assertion for
//! `java.lang.Module`, and it existed for that one class and nothing else.
//!
//! ```text
//! cargo test -p cratonvm-native-builtins --test essential_wiring_ratchet -- --nocapture
//! ```

use cratonvm_native_api::{NativeKind, NativeMethodRegistry};
use cratonvm_native_builtins::register_essential_natives;

/// The one model of `vm_init`'s real-JDK boot path, shared with
/// `stub_ratchet.rs` and `duplicate_registration_gate.rs`. Used by
/// [`the_terminals_survive_the_whole_real_jdk_boot`], which is the assertion
/// that catches the failure mode the registrar's own doc comment warns about:
/// a triple `native-collections` also registers makes the essentials
/// registration silently inert, because `register_collections_natives` runs
/// LATER and `register()` is last-write-wins.
#[path = "common/vm_init_boot_path.rs"]
mod boot_path;

/// The primitive-stream TERMINALS that are ABSTRACT on the real JDK 25
/// interfaces, taken from `phases_late::register_phase56_primitive_stream_terminals`
/// — the narrowed registrar `reflect_annotations::register_annotation_overrides`
/// wires into the real-JDK essentials path.
///
/// Six, not the five W7-5 §6.3 listed. That record's list was written before
/// the narrowed registrar existed and predicted its contents; two rows differ
/// and both differences are deliberate:
///
///  * **`java/util/stream/Stream.forEachOrdered(Consumer)V` is NOT here.** It is
///    equally abstract, and it is the one masked by the interpreter's hardcoded
///    `!has_code` special case. Registering it is what would let that special
///    case be deleted, but it is not registered today, so asserting it would
///    freeze a state the tree is not in. Recorded as the next row to add, not
///    as a passing assertion.
///  * **`{Long,Double}Stream.forEachOrdered` ARE here.** The record listed only
///    `IntStream.forEachOrdered` ("unmasked, live defect"); the registrar that
///    landed covers all three primitive widths.
///
/// A triple added here that is not registered turns this gate red, which is the
/// point. A triple removed from the registrar without being removed here does
/// the same.
const ABSTRACT_PRIMITIVE_STREAM_TERMINALS: &[(&str, &str, &str)] = &[
    (
        "java/util/stream/IntStream",
        "forEachOrdered",
        "(Ljava/util/function/IntConsumer;)V",
    ),
    (
        "java/util/stream/IntStream",
        "summaryStatistics",
        "()Ljava/util/IntSummaryStatistics;",
    ),
    (
        "java/util/stream/LongStream",
        "forEachOrdered",
        "(Ljava/util/function/LongConsumer;)V",
    ),
    (
        "java/util/stream/LongStream",
        "summaryStatistics",
        "()Ljava/util/LongSummaryStatistics;",
    ),
    (
        "java/util/stream/DoubleStream",
        "forEachOrdered",
        "(Ljava/util/function/DoubleConsumer;)V",
    ),
    (
        "java/util/stream/DoubleStream",
        "summaryStatistics",
        "()Ljava/util/DoubleSummaryStatistics;",
    ),
];

/// They must be reachable from `register_essential_natives` — the real-JDK boot
/// path — and not only from the `synthetic-jdk`-gated
/// `register_phase56_stream_extras`.
///
/// `register_essential_natives` is the entry point, not
/// `register_synthetic_overrides`: that is the whole distinction this file
/// measures, so the registry is built from the shipping entry point and nothing
/// else.
#[test]
fn essentials_cover_the_abstract_primitive_stream_terminals() {
    let mut registry = NativeMethodRegistry::new();
    register_essential_natives(&mut registry);

    let missing: Vec<String> = ABSTRACT_PRIMITIVE_STREAM_TERMINALS
        .iter()
        .filter(|(c, m, d)| registry.find(c, m, d).is_none())
        .map(|(c, m, d)| format!("{c}.{m}{d}"))
        .collect();

    println!(
        "essential-wiring: {} of {} abstract primitive-stream terminals \
         registered on the real-JDK essentials path",
        ABSTRACT_PRIMITIVE_STREAM_TERMINALS.len() - missing.len(),
        ABSTRACT_PRIMITIVE_STREAM_TERMINALS.len()
    );

    assert!(
        missing.is_empty(),
        "{} triple(s) are NOT registered on the real-JDK essentials path:\n  {}\n\
         The receiver is a VM-minted instance of the interface ITSELF \
         (`try_alloc_synthetic(ctx, \"java/util/stream/IntStream\", ..)` in \
         native-collections' `make_int_stream`), and the declaration is \
         ABSTRACT on JDK 25, so a missing registration is an \
         `AbstractMethodError: … has no Code attribute` — there is no bytecode \
         to fall back to. Do NOT fix this by wiring \
         `register_phase56_stream_extras` wholesale: it also carries 22 triples \
         native-collections already serves, STATIC interface methods that keep \
         the native check in real-JDK mode, and a 1-field stream layout that \
         disagrees with `make_stream`'s `STREAM_NUM_FIELDS`. Add the triple to \
         the narrowed `register_phase56_primitive_stream_terminals` instead. \
         See docs/known-issues/jdk-only/W7-5-registrars-that-never-shipped.md §4.2.",
        missing.len(),
        missing.join("\n  ")
    );
}

/// None of them may be a `SyntheticStub`, or `--jdk-only` refuses it at
/// registration and the strict arm gets the `AbstractMethodError` back.
///
/// This is the half a `find(..).is_some()` cannot see. `register()` refuses a
/// `SyntheticStub` outright under `CompatibilityMode::JdkOnly`, so for a triple
/// whose only implementation is the native, the KIND decides whether strict
/// mode has an implementation at all. The narrowed registrar states `Bridge`
/// for exactly this reason — these six are §1.5 bridges in the strong sense:
/// there is no real bytecode they could shadow.
#[test]
fn none_of_the_terminals_is_a_synthetic_stub() {
    let mut registry = NativeMethodRegistry::new();
    register_essential_natives(&mut registry);

    let stubs: Vec<String> = ABSTRACT_PRIMITIVE_STREAM_TERMINALS
        .iter()
        .filter(|(c, m, d)| registry.kind_of(c, m, d) == Some(NativeKind::SyntheticStub))
        .map(|(c, m, d)| format!("{c}.{m}{d}"))
        .collect();

    assert!(
        stubs.is_empty(),
        "{} abstract primitive-stream terminal(s) are tagged \
         `NativeKind::SyntheticStub`:\n  {}\n\
         `register()` refuses a SyntheticStub under `CompatibilityMode::JdkOnly`, \
         and these triples have NO bytecode underneath them, so a refusal here \
         does not hand the method back to the JDK — it hands it back to \
         `AbstractMethodError`. `register_phase56_primitive_stream_terminals` \
         states `Bridge`; something re-registered one of these with a chosen \
         stub kind afterwards.",
        stubs.len(),
        stubs.join("\n  ")
    );
}

/// They must still be registered at the END of the whole real-JDK boot, not
/// merely at the end of `register_essential_natives`.
///
/// **This is the assertion that catches the failure the registrar's own doc
/// comment warns about**, and it cannot be phrased against the essentials
/// registry alone:
///
/// > *"ORDERING: safe to call from the real-JDK essentials path, which runs
/// > BEFORE `register_collections_natives` and would therefore be overwritten
/// > by it. Every triple below is one native-collections does NOT register …
/// > Adding a triple that native-collections also registers makes this
/// > registrar silently inert — check before extending it."*
///
/// "Check before extending it" is an instruction to a human. This is the same
/// check as a test: `register()` is last-write-wins and
/// `register_collections_natives` runs after essentials in `vm_init`, so a
/// future `native-collections` registration of any of these six would take the
/// slot, and the essentials call would become a no-op that still looks wired.
///
/// It asserts presence and non-stub-ness, NOT which registrar owns the slot: a
/// deliberate `native-collections` implementation of one of these is a fine
/// outcome, and only silence is not.
#[test]
fn the_terminals_survive_the_whole_real_jdk_boot() {
    let mut registry = NativeMethodRegistry::new();
    boot_path::vm_init_real_jdk_boot_path(&mut registry);

    let lost: Vec<String> = ABSTRACT_PRIMITIVE_STREAM_TERMINALS
        .iter()
        .filter(|(c, m, d)| {
            registry.find(c, m, d).is_none()
                || registry.kind_of(c, m, d) == Some(NativeKind::SyntheticStub)
        })
        .map(|(c, m, d)| format!("{c}.{m}{d}"))
        .collect();

    println!(
        "essential-wiring(boot): {} of {} terminals survive the full \
         vm_init real-JDK sequence as non-stubs",
        ABSTRACT_PRIMITIVE_STREAM_TERMINALS.len() - lost.len(),
        ABSTRACT_PRIMITIVE_STREAM_TERMINALS.len()
    );

    assert!(
        lost.is_empty(),
        "{} terminal(s) are registered by `register_essential_natives` but are \
         absent — or tagged `SyntheticStub` — after the full `vm_init` real-JDK \
         sequence:\n  {}\n\
         `register()` is last-write-wins and `register_collections_natives` runs \
         AFTER essentials, so a later registration of the same triple takes the \
         slot and the essentials call becomes a no-op that still reads as wired. \
         That is the exact hazard `register_phase56_primitive_stream_terminals`' \
         doc comment asks a human to check for by hand.",
        lost.len(),
        lost.join("\n  ")
    );
}
