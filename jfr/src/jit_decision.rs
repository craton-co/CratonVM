// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! # `cratonvm.JitCompileDecision` — why the JIT took, or refused, a method
//!
//! ## What this exists to answer
//!
//! The compiler already *computes* a precise, human-readable reason for every
//! compile decision. `cratonvm_jit`'s admission chain builds a verdict string
//! ("`optimize=false — the C1/fast tier was requested, not C2`",
//! "`value shape not admitted (category2=true fp=false …)`", …) and
//! `note_jit_bail_site` / `note_jit_bail_site_at` record a named bail site plus
//! the bytecode pc and opcode that caused it. All of that reaches **stderr**,
//! **only** under `CRATONVM_DBG_JITC` / `CRATONVM_DBG_IR_COMPILES`, and
//! therefore only on a run somebody thought to re-launch with the flag set.
//!
//! The 2026-09-01 audit is the argument for this event. `String.charAt` in a
//! counted loop cost a flat 186-196 ns/char while a byte-identical body reached
//! 3.0 ns/char elsewhere in the same binary — 90x. Five hypotheses for what
//! selected the fast body were each refuted by measurement without converging
//! on an answer, because the one instrument that could name the decision was
//! reachable only by rebuilding, re-running under a debug flag, and grepping
//! stderr — and even then it omitted a term of the real conjunction: which
//! *door* asked for the compile.
//!
//! As a JFR event the same information is something an operator captures from a
//! production run they already have, and "why is this method slow?" stops being
//! an archaeology exercise.
//!
//! ## Turning it on
//!
//! **Off by default**, as a diagnostic event should be. It is armed only when a
//! *running* recording names it explicitly:
//!
//! ```java
//! var r = new jdk.jfr.Recording();
//! r.enable("cratonvm.JitCompileDecision");
//! r.start();
//! ```
//!
//! `Recording.enable(name)` lands in
//! [`RecordingSettings::enabled_event_names`](crate::recording::RecordingSettings::enabled_event_names),
//! and [`sync_jit_decision_gate`] reads exactly that set. A recording that
//! installs **no** name filter keeps every event it sees, but it does not arm
//! this one: "keep whatever arrives" is not the same as "pay the producer cost
//! of a diagnostic", and the whole point of the gate is that the producer can
//! skip building the verdict string. An operator who wants this event asks for
//! it by name.
//!
//! ## Cost when disabled
//!
//! [`jit_decision_enabled`] is a single atomic load and a branch. The producer
//! side **must** call it *before* formatting anything:
//!
//! ```ignore
//! if cratonvm_jfr::jit_decision::jit_decision_enabled() {
//!     let verdict = /* ... the String ... */;
//!     cratonvm_jfr::jit_decision::record_jit_compile_decision(&decision);
//! }
//! ```
//!
//! The natural way to get this wrong is to format the reason first and then
//! discover nobody wanted it, which is why the predicate is public and separate
//! from [`record_jit_compile_decision`] rather than hidden inside it.
//! `record_jit_compile_decision` re-checks the gate — so a careless caller is
//! still correct, just not free — but that check cannot un-allocate a `String`
//! the caller has already built.
//!
//! ## Why a sink instead of a direct call
//!
//! `cratonvm-jit` already depends on `cratonvm-jfr` (for `phase`), so there is
//! no crate cycle to route around. What `cratonvm-jit` does *not* have is a
//! [`FlightRecorder`]: the recorder lives in `cratonvm-vm`'s
//! `SharedVm::debug.flight_recorder`, and `cratonvm-jit` cannot depend on
//! `cratonvm-vm` — that edge would cycle. So this mirrors the pattern
//! `cratonvm-gc` already uses for its JVMTI hooks (`gc::install_gc_start_hook`,
//! `gc::install_class_info_hook`): the VM installs a sink at boot, and the
//! producer fires it behind an atomic that is false until it is installed.
//!
//! The sink is a boxed closure rather than a bare `fn` pointer — unlike the GC
//! hooks it has to *capture* the recorder handle, and a `fn` cannot.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::OnceLock;

use crate::event::EventValue;
use crate::recording::FlightRecorder;

/// The JFR event name. Also the string an operator passes to
/// `jdk.jfr.Recording.enable(...)`, and the key [`sync_jit_decision_gate`]
/// looks for in a running recording's name filter.
pub const JIT_COMPILE_DECISION_EVENT: &str = "cratonvm.JitCompileDecision";

/// Value written to `bailBci` / `bailOpcode` when the decision is not
/// attributable to one bytecode.
///
/// `-1` rather than `0`, because `0` is a legal bci *and* a legal opcode
/// (`nop`): `note_jit_bail_site` records `(0, 0)` for the sites that are not a
/// single bytecode's fault, which is otherwise indistinguishable from a real
/// refusal on the first instruction. Producers must map that `(0, 0)` onto this
/// sentinel rather than passing it through.
pub const NO_BAIL_SITE: i32 = -1;

// ---------------------------------------------------------------------------
// Vocabulary
// ---------------------------------------------------------------------------

/// Which compile door asked.
///
/// A structural mirror of `cratonvm_jit::compile_gate::CompileDoor`, redeclared
/// here because the dependency runs jit -> jfr and cannot run back. Keeping the
/// spellings identical is deliberate: the two enums name the same three doors,
/// and a reader comparing a JFR dump against `compile_gate`'s own diagnostics
/// must not have to translate.
///
/// This term is load-bearing and no stderr line carries it reliably today.
/// `compile_gate` exists *because* patching one door and shipping was a
/// repeated failure mode here — a method can be refused at `MethodEntry` and
/// accepted at `Osr` in the same run, so a verdict that does not say which door
/// asked cannot distinguish "the compiler refuses this method" from "the
/// compiler was never asked through the door that matters".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompileDoor {
    /// `try_compile_with_invokespecial_resolver` — the ordinary tiering door.
    MethodEntry,
    /// The interpreter's eager first-call single-pass compile.
    EagerFirstCall,
    /// `compile_osr_artifact` — the on-stack-replacement door.
    Osr,
}

impl CompileDoor {
    /// Stable wire name, written into the event's `door` field verbatim — so
    /// renaming a variant must not rename this.
    pub const fn name(self) -> &'static str {
        match self {
            CompileDoor::MethodEntry => "MethodEntry",
            CompileDoor::EagerFirstCall => "EagerFirstCall",
            CompileDoor::Osr => "Osr",
        }
    }
}

/// What the compiler did with the request.
///
/// Deliberately three-valued rather than a `succeeded: boolean`. The question
/// the audit could not answer was not "did it compile" but *which body ran*: a
/// method that reaches [`Self::SinglePass`] is compiled, is fast-ish, and never
/// shows up in a bail list, yet it has silently lost every optimization the IR
/// tier would have applied — which is exactly the shape of the `String.charAt`
/// finding.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompileOutcome {
    /// The single-pass (C1-equivalent) backend produced the body.
    SinglePass,
    /// The optimizing IR tier (C2-equivalent) produced the body.
    Optimizing,
    /// No body was produced; the method stays interpreted.
    Refused,
}

impl CompileOutcome {
    /// Stable wire name, written into the event's `outcome` field verbatim.
    pub const fn name(self) -> &'static str {
        match self {
            CompileOutcome::SinglePass => "SinglePass",
            CompileOutcome::Optimizing => "Optimizing",
            CompileOutcome::Refused => "Refused",
        }
    }
}

/// The `reason` payload, kept as a borrow so the producer never allocates for a
/// decision nobody records.
///
/// This crate has no per-value string pool — [`crate::dump`] and
/// [`crate::jdk_chunk`] both write string field values inline — so the closest
/// thing to pooling is [`EventValue::Str`], which stores a `&'static str`
/// directly and skips the per-event `Arc::from`. The reason set is small and
/// repeats heavily, and the commonest producer case is already `&'static str`:
/// every `note_jit_bail_site` / `note_jit_bail_site_at` site name is a literal.
/// [`Self::Static`] carries those for free. [`Self::Borrowed`] is for the
/// admission verdict, which is genuinely a freshly formatted `String`.
#[derive(Debug, Clone, Copy)]
pub enum DecisionText<'a> {
    /// A literal — becomes [`EventValue::Str`], no allocation.
    Static(&'static str),
    /// A borrowed run-time string — copied into an `Arc<str>` at emit time.
    Borrowed(&'a str),
}

impl DecisionText<'_> {
    /// Convert to the field value the event carries. Both variants declare as
    /// `"string"`, so neither can desync the chunk.
    pub fn to_event_value(self) -> EventValue {
        match self {
            DecisionText::Static(s) => EventValue::Str(s),
            DecisionText::Borrowed(s) => EventValue::String(std::sync::Arc::from(s)),
        }
    }
}

/// Everything one compile decision has to say.
///
/// Borrowed throughout and `Copy`, so a producer that has already decided to
/// record builds it on the stack with no allocation at all; the only
/// allocations on the whole path are the `method` string the emitter joins and,
/// for a [`DecisionText::Borrowed`] reason, one `Arc<str>`.
#[derive(Debug, Clone, Copy)]
pub struct JitCompileDecision<'a> {
    /// Internal (slash-separated) class name, e.g. `java/lang/String`.
    pub class_name: &'a str,
    /// Method name, e.g. `charAt`.
    pub method_name: &'a str,
    /// Method descriptor, e.g. `(I)C`.
    pub method_descriptor: &'a str,
    /// Which door asked for this compile.
    pub door: CompileDoor,
    /// Which body, if any, the request produced.
    pub outcome: CompileOutcome,
    /// The admission verdict, or the bail-site name.
    pub reason: DecisionText<'a>,
    /// Bytecode index the refusal is attributable to, or [`NO_BAIL_SITE`].
    pub bail_bci: i32,
    /// Opcode the refusal is attributable to, or [`NO_BAIL_SITE`].
    pub bail_opcode: i32,
    /// Length of the method's bytecode, in bytes. Size-keyed refusals are
    /// common enough that digging them out of the reason string is a chore.
    pub bytecode_size: i32,
    /// Event start, nanos since epoch.
    pub start_time_ns: u64,
    /// Compile duration if it is already measured at the call site, else `0`.
    /// Carried in the event's `end_time` rather than as a field, which is how
    /// every other `EventPeriod::BeginEnd` built-in here reports duration.
    pub duration_ns: u64,
}

// ---------------------------------------------------------------------------
// The gate
// ---------------------------------------------------------------------------

/// Set once the VM installs a sink. Without one there is nowhere to send a
/// decision, so the producer must not build one.
static SINK_INSTALLED: AtomicBool = AtomicBool::new(false);

/// Set while some running recording names [`JIT_COMPILE_DECISION_EVENT`].
static RECORDING_ENABLED: AtomicBool = AtomicBool::new(false);

/// `SINK_INSTALLED && RECORDING_ENABLED`, precomputed so the producer's check
/// is one load rather than two.
static ARMED: AtomicBool = AtomicBool::new(false);

/// Is anybody going to read a `cratonvm.JitCompileDecision` right now?
///
/// **Call this before constructing any `String`.** One atomic load and a
/// branch; on a default run — no recording, or a recording that never named the
/// event — it is false forever and the compile path pays nothing else. This is
/// the whole reason the predicate is public: the producer's `reason` is a
/// formatted `String`, and a gate that could only be consulted *after* it was
/// built would defeat itself.
///
/// `Acquire` rather than `Relaxed`, matching [`crate::is_enabled`] and for the
/// same reason: the arming path writes registry state — the event type's id is
/// assigned by `register_builtin_events` — before it flips this flag, and a
/// producer that observed the flip without the pairing could emit against a
/// stale registry. On x86-64 an `Acquire` load of a `bool` is the same
/// instruction as a `Relaxed` one, so this costs nothing here and is correct on
/// AArch64, which this VM also targets.
///
/// ## What this flag is, and is not
///
/// It is a **producer-side cost gate**, not the filter. The authority on
/// whether an event is kept is still the per-recording name filter
/// ([`crate::recording::RecordingSettings::name_filter_admits`]) applied at
/// drain time. That distinction matters because the flag is process-global
/// while a [`FlightRecorder`] is not: in the `cratonvm-vm` test binary, which
/// builds a `SharedVm` (and therefore a recorder) per test, the last recorder to
/// call [`sync_jit_decision_gate`] wins. A spuriously-`true` flag costs one sink
/// call whose event the uninterested recording then drops; a spuriously-`false`
/// flag loses diagnostic events in a concurrent test. Both are acceptable for a
/// diagnostic and neither can corrupt a chunk. Production has exactly one
/// recorder per process, where the flag is exact.
#[inline(always)]
pub fn jit_decision_enabled() -> bool {
    ARMED.load(Ordering::Acquire)
}

/// The VM-installed delivery closure.
///
/// Boxed rather than a bare `fn` because it has to capture the recorder handle
/// — `cratonvm-jit` has no route to `SharedVm::debug.flight_recorder`, and that
/// capture is the entire point of the indirection.
pub type JitDecisionSink = Box<dyn Fn(&JitCompileDecision<'_>) + Send + Sync + 'static>;

static SINK: OnceLock<JitDecisionSink> = OnceLock::new();

/// Install the delivery closure. Idempotent — only the first installation wins,
/// and a later call is dropped rather than replacing a live sink out from under
/// a concurrent producer.
///
/// The VM calls this once at boot with a closure that locks
/// `SharedVm::debug.flight_recorder` and forwards to
/// [`crate::builtin::emit_jit_compile_decision_event`].
pub fn install_jit_decision_sink(sink: JitDecisionSink) {
    let _ = SINK.set(sink);
    SINK_INSTALLED.store(SINK.get().is_some(), Ordering::Release);
    rearm();
}

/// Recompute the gate from a recorder's current recordings.
///
/// Called from `FlightRecorder::refresh_running_ids`, so start and stop
/// transitions arm and disarm the event on their own. It must **also** be called
/// by anyone who mutates a *running* recording's
/// [`enabled_event_names`](crate::recording::RecordingSettings::enabled_event_names)
/// after the fact — which is what the `jdk.jfr` Java boundary does, because
/// `Recording.enable(...)` may be called at any point in a recording's life and
/// changes no recording's *state*, so nothing else would notice.
pub fn sync_jit_decision_gate(recorder: &FlightRecorder) {
    let wanted = recorder.any_running_recording_names_event(JIT_COMPILE_DECISION_EVENT);
    RECORDING_ENABLED.store(wanted, Ordering::Release);
    rearm();
}

fn rearm() {
    let armed = SINK_INSTALLED.load(Ordering::Acquire) && RECORDING_ENABLED.load(Ordering::Acquire);
    ARMED.store(armed, Ordering::Release);
}

/// Hand one compile decision to the installed sink.
///
/// Cheap and correct when nothing is listening, but **not** a substitute for
/// checking [`jit_decision_enabled`] first: by the time this returns early the
/// caller has already paid for whatever it formatted to build `decision`.
#[inline]
pub fn record_jit_compile_decision(decision: &JitCompileDecision<'_>) {
    if !jit_decision_enabled() {
        return;
    }
    if let Some(sink) = SINK.get() {
        sink(decision);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::recording::RecordingSettings;

    #[test]
    fn door_and_outcome_names_are_stable() {
        assert_eq!(CompileDoor::MethodEntry.name(), "MethodEntry");
        assert_eq!(CompileDoor::EagerFirstCall.name(), "EagerFirstCall");
        assert_eq!(CompileDoor::Osr.name(), "Osr");
        assert_eq!(CompileOutcome::SinglePass.name(), "SinglePass");
        assert_eq!(CompileOutcome::Optimizing.name(), "Optimizing");
        assert_eq!(CompileOutcome::Refused.name(), "Refused");
    }

    #[test]
    fn static_reason_does_not_allocate_an_arc() {
        // `Str` is the zero-allocation variant and `String` is not; that is the
        // property the `DecisionText` split exists to preserve.
        assert!(matches!(
            DecisionText::Static("scan/wide-iinc").to_event_value(),
            EventValue::Str("scan/wide-iinc")
        ));
        let owned = format!("verdict {}", 1);
        assert!(matches!(
            DecisionText::Borrowed(&owned).to_event_value(),
            EventValue::String(_)
        ));
    }

    #[test]
    fn event_is_registered_and_declares_seven_fields() {
        let fr = crate::create_flight_recorder();
        let id = fr
            .type_registry
            .find_by_name(JIT_COMPILE_DECISION_EVENT)
            .expect("cratonvm.JitCompileDecision must be a registered built-in");
        let et = fr.type_registry.get(id).unwrap();
        assert_eq!(et.fields.len(), 7);
        assert!(et.fields.len() <= crate::event::EVENT_FIELD_INLINE);
        assert_eq!(et.fields[0].name, "method");
        assert_eq!(et.fields[1].name, "door");
        assert_eq!(et.fields[2].name, "outcome");
        assert_eq!(et.fields[3].name, "reason");
    }

    #[test]
    fn a_recording_that_names_no_events_does_not_arm_the_gate() {
        // The default-off contract: "keep everything" is not "arm the
        // diagnostic". Only an explicit `enable(name)` counts.
        let _g = crate::repository::jfr_test_guard();
        let mut fr = crate::create_flight_recorder();
        let rid = fr.new_recording(RecordingSettings::new("no-name-filter"));
        fr.start_recording(rid);
        assert!(!fr.any_running_recording_names_event(JIT_COMPILE_DECISION_EVENT));
        fr.stop_recording(rid);
    }

    #[test]
    fn a_recording_that_names_the_event_is_seen() {
        let _g = crate::repository::jfr_test_guard();
        let mut fr = crate::create_flight_recorder();
        let rid = fr.new_recording(RecordingSettings::new("named"));
        fr.get_recording_mut(rid)
            .unwrap()
            .settings
            .enabled_event_names = Some(
            [JIT_COMPILE_DECISION_EVENT.to_string()]
                .into_iter()
                .collect(),
        );
        fr.start_recording(rid);
        assert!(fr.any_running_recording_names_event(JIT_COMPILE_DECISION_EVENT));
        fr.stop_recording(rid);
        assert!(!fr.any_running_recording_names_event(JIT_COMPILE_DECISION_EVENT));
    }
}
