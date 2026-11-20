// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

#![deny(
    clippy::missing_safety_doc,
    clippy::not_unsafe_ptr_arg_deref,
    clippy::undocumented_unsafe_blocks
)]

//! # cratonvm-jfr — Java Flight Recorder support for CratonVM
//!
//! ## Per-thread event rings
//!
//! High-throughput emit goes through [`push_to_thread_ring`], which writes to
//! a thread-local ring (capacity [`DEFAULT_THREAD_RING_CAPACITY`] = 1024).
//! Dump-time consumers call [`global_ring_registry`]`().drain_all()` to harvest
//! all threads' events.
//!
//! This avoids global `Mutex` contention on the hot emit path: every producer
//! thread owns its own shard (registered once on first emit), and the registry
//! mutex is only taken at registration time and during drainage.
//!
//! See [`ThreadEventRing`] for the local (non-registered) ring type, and
//! [`ThreadRingRegistry`] for the multi-thread aggregator used by the dumper.
//!
//! ---------------------------------------------------------------------------
//! # LIVENESS — observability audit, 2026-07-26
//! ---------------------------------------------------------------------------
//!
//! **No production code path starts a recording, so JFR captures nothing on a
//! default run.** The emit surface is genuinely wired — ~30 `emit_*` call
//! sites across the interpreter, the JIT, `vm_exec`, `vm_util` and `vm_init`
//! cover GC, class load, thread lifecycle, monitors, compilation,
//! deoptimization, file I/O and virtual-thread pinning — and a
//! `FlightRecorder` is constructed for every VM
//! (`SharedVm::debug.flight_recorder`). What is missing is the trigger:
//!
//!  * There is **no `-XX:StartFlightRecording` command-line option**. Nothing
//!    in `vm-cli` parses one.
//!  * `FlightRecorder::new_recording` / `start_recording` have **zero callers
//!    outside `#[cfg(test)]`**, so [`set_enabled`] is never flipped and
//!    [`is_enabled`] is permanently `false`.
//!
//!    **CORRECTION (2026-08-13): that bullet is stale, and it was already stale
//!    when the audit was written.** `NativeContext::jfr_begin_java_recording`
//!    (`vm/src/vm/vm_exec.rs`) calls both, and it is reached from real
//!    application bytecode — `jdk.jfr.Recording.start()` and
//!    `jdk.jfr.consumer.RecordingStream.startAsync()`/`start()`, both wired in
//!    `native_builtins::jfr`. So on a run that opens a `Recording` or a
//!    `RecordingStream`, `refresh_running_ids` DOES flip the gate, every
//!    `emit_*` above goes live, and `FlightRecorder::dump_recording` writes a
//!    real file. That is observable: a `Recording` opened around a few lines of
//!    Java dumps the VM's own `jdk.ClassLoad` events alongside the
//!    application's. The rest of this block — no CLI option, no jcmd host — is
//!    still accurate, and remains why a run that touches no `jdk.jfr` class
//!    records nothing.
//!  * The `JFR.start` / `JFR.stop` / `JFR.dump` jcmd verbs in
//!    `vm/src/runtime/serviceability.rs` never touched the recorder (they
//!    returned canned success strings; the audit replaced them with honest
//!    failures), and in any case nothing constructs the `JcmdProcessor` that
//!    hosts them, nor opens an attach socket.
//!  * [`dump_to_file`] has no caller outside this crate.
//!
//! Because every `emit_*` checks [`is_enabled`] first, the standing cost is
//! one relaxed atomic load per call site — the wiring is not a performance
//! problem, it is simply unreachable. The two deliberate exceptions that
//! bypass the gate (`emit_physical_memory_event` and
//! `emit_initial_environment_variable_event`, both one-shot from `vm_init` on
//! the main thread) push into that thread's bounded ring and stay there.
//!
//! To make JFR real, in dependency order: add the CLI option and/or bind the
//! jcmd verbs to `SharedVm::debug.flight_recorder`; call `new_recording` +
//! `start_recording`; and call `FlightRecorder::dump_recording` on stop.
//!
//! The produced file **is** JMC / `jfr print` loadable as of 2026-08-13:
//! `dump_recording` writes the JDK's own chunk format ([`jdk_chunk`]), verified
//! against `RecordingFile`, `jfr summary` and `jfr print`. It used not to be,
//! and the FORMAT-FIDELITY GAP block on `write_metadata_section` in [`dump`]
//! describes the format that is still what [`dump::dump_to_file`] writes and
//! what [`read_events`] reads. What remains true is that **no event carries a
//! real captured stack trace**, and that a JDK-format chunk from this writer
//! declares no `eventThread`/`stackTrace` field at all.
//!
//! ## Memory bounds (audited)
//!
//!  * Per-thread emit ring: [`DEFAULT_THREAD_RING_CAPACITY`] = 1024 entries,
//!    fixed, oldest-dropped on overflow.
//!  * Per-recording repository: `EventRepository::default()` = 100_000 events
//!    with ring eviction. Bounded.
//!  * Dead shards: [`ThreadRingRegistry`] reclaims retired+empty shards, but
//!    only from inside `drain_all`. Since `drain_all` runs at dump time and no
//!    dump ever happens, reclamation never runs in production today. This is
//!    currently harmless *only* because the gate keeps ordinary threads from
//!    registering a shard at all. It becomes a real per-thread leak the moment
//!    a long-lived recording is started on a workload that churns threads —
//!    fix reclamation (or drive a periodic drain) as part of wiring the
//!    trigger, not after.
//!  * `RecordingSettings::max_age` / `max_size` are **not enforced anywhere**;
//!    see their declarations in [`recording`].
//!
//! ## Phase accounting
//!
//! [`phase`] partitions each thread's wall clock into named categories plus an
//! explicit unattributed remainder, and is the answer to the C2 review's
//! "separate startup, compilation, execution, and GC time" lane. Like
//! [`jdk_only`] it shares none of the machinery above: it takes no event ring,
//! registers no built-in event type, has its own flag, and does not consult
//! [`is_enabled`] — a flight recording and a wall-clock partition are
//! different questions. Its JFR sink is standalone (it builds its own registry
//! and calls [`jdk_chunk::dump_to_file`] directly), so a phase report can be
//! produced by a run that never started a recording — which, per the LIVENESS
//! block above, is every run that touches no `jdk.jfr` class.
//!
//! ## Real-JDK mode telemetry
//!
//! [`jdk_only`] holds the aggregate counter set of
//! `docs/book/src/user-guide/real-jdk-mode.md` — seven `_total` counters folded, at
//! report time, out of censuses the VM already keeps. It lives in this crate
//! because this is where the repository puts "measurements of a run that an
//! operator may ask to have written out", not because it emits JFR events: it
//! shares none of the machinery above, takes no ring, registers no event type,
//! and does not consult [`is_enabled`]. It has its own gate (off by default,
//! opened only by the Real-JDK artefact flags), because a flight recording and
//! a policy census are different questions and an operator asking for one must
//! not silently get the other.

pub mod builtin;
pub mod dump;
pub mod event;
pub mod jdk_chunk;
pub mod jdk_only;
pub mod jit_decision;
pub mod phase;
pub mod recording;
pub mod repository;
pub mod stream;

pub use dump::{dump_to_file, read_events, read_jfr_header, JfrDumpError, JfrFileHeader};
pub use event::*;
// The JDK-format chunk writer. Reached through `jdk_chunk::` rather than
// re-exported flat, because its `dump_to_file` is deliberately the same shape
// as [`dump::dump_to_file`] and the two write DIFFERENT formats — a flat
// re-export would make the two indistinguishable at a call site, which is
// exactly the confusion the module docs warn about.
pub use jdk_chunk::{JDK_HEADER_SIZE, JDK_MAJOR, JDK_MINOR};
// Real-JDK counters. Named re-exports rather than a glob: the module's public
// surface is mostly `const` label vocabularies whose names (`NATIVE_KINDS`,
// `GENERATORS`) are generic enough to collide with a future JFR export, and a
// vocabulary should be reached through `jdk_only::` so the reader knows which
// contract's spelling they are looking at.
pub use jdk_only::{
    ContractCounts, JdkOnlyCounters, JdkOnlyTelemetry, NativeCensusSample,
    JDK_ONLY_TELEMETRY_SCHEMA_VERSION,
};
// Phase accounting. Named re-exports for the same reason as `jdk_only`'s: the
// module's own vocabulary (`enabled`, `level`, `report`, `enter`) is generic
// enough to collide with the JFR recording surface above, and a caller should
// reach those through `phase::` so it is obvious which subsystem is being
// asked. The types are re-exported because a consumer that stores a
// `PhaseReport` should not have to name the module to spell its own field.
pub use phase::{
    Anomalies, Category, Level, PhaseReport, PhaseSpan, ThreadPhases,
    PHASE_ACCOUNTING_SCHEMA_VERSION,
};
// The JIT compile-decision diagnostic event. Named re-exports rather than a
// glob, for the same reason as `jdk_only`'s and `phase`'s below: the module's
// vocabulary (`CompileDoor`, `CompileOutcome`, `install_jit_decision_sink`) is
// generic enough that a reader should be able to see which subsystem's contract
// a name belongs to at the use site. `jit_decision_enabled` is re-exported flat
// because it is the one item a producer calls on its hot path and the whole
// point of it is to be cheap to reach.
pub use jit_decision::{
    install_jit_decision_sink, jit_decision_enabled, record_jit_compile_decision,
    sync_jit_decision_gate, CompileDoor, CompileOutcome, DecisionText, JitCompileDecision,
    JitDecisionSink, JIT_COMPILE_DECISION_EVENT, NO_BAIL_SITE,
};
pub use recording::*;
pub use repository::*;
pub use stream::EventStream;

// Explicit re-exports of the per-thread ring API. These are also covered by
// the blanket `pub use repository::*;` above, but listing them here documents
// the public surface and guards against accidental removal from the glob.
pub use repository::{
    global_ring_registry, push_to_thread_ring, ThreadEventRing, ThreadRingRegistry,
    DEFAULT_THREAD_RING_CAPACITY,
};

use std::sync::atomic::{AtomicBool, Ordering};

/// Global JFR enabled flag — set when any recording starts, cleared when the
/// last one stops. Every `emit_*` function in `builtin.rs` checks this with a
/// single relaxed load + branch to skip the disabled-path work entirely.
///
/// This converts the disabled-path cost from ~30-100 ns/call (HashMap probe +
/// Vec alloc) to ~2-3 ns (one relaxed load + branch).
static JFR_ENABLED: AtomicBool = AtomicBool::new(false);

/// Returns whether any flight recording is currently running.
///
/// This is the fast-path check used by every `emit_*` function.
///
/// Round-5 CRIT-fix (Bug 2, 2026-05-17): uses `Acquire` ordering so it
/// correctly pairs with the `Release` store in `set_enabled`. Previously
/// this was `Relaxed`, which paired with nothing — on a weakly-ordered
/// architecture (ARM64, POWER) a producer could observe
/// `is_enabled() == true` while still seeing stale values for
/// type-registry slots, `cached_event_id` caches, or `running_ids` snapshots
/// that the recording-start path wrote *before* flipping the flag. The
/// observable bug was first-batch events on a fresh recording silently
/// dropping (or recording with an INVALID `type_id`) until the producer
/// happened to re-fetch the registry.
///
/// On x86/x86_64 (TSO) `Acquire` is identical in cost to `Relaxed` for a
/// simple load. ARM64 pays one LDAR — still cheap, and the correctness
/// pairing matters far more than the nanosecond difference per emit.
#[inline(always)]
pub fn is_enabled() -> bool {
    JFR_ENABLED.load(Ordering::Acquire)
}

/// Set the global JFR-enabled flag. Called by `FlightRecorder::start_recording`
/// and `stop_recording`. Uses `Release` ordering so writes that precede the
/// store (e.g. registry inserts during recording setup) are visible to readers
/// that subsequently observe `is_enabled() == true` via the matching
/// `Acquire` load in [`is_enabled`].
pub fn set_enabled(v: bool) {
    JFR_ENABLED.store(v, Ordering::Release);
}

static REAL_BACKEND_ACTIVE: AtomicBool = AtomicBool::new(false);

/// SBR5 lane `jfr`: the real (`jdk.jfr`-driven) backend has a recording running.
/// Set by the VM at each collection's start; it arms the same phase stash
/// [`is_enabled`] does, without turning the legacy recorder's emitters on.
pub fn set_real_backend_active(v: bool) {
    REAL_BACKEND_ACTIVE.store(v, Ordering::Relaxed);
}

/// Whether collectors should stash their pause-phase marks.
#[inline]
pub fn phases_wanted() -> bool {
    is_enabled() || REAL_BACKEND_ACTIVE.load(Ordering::Relaxed)
}

/// Number of running recordings summed over every [`FlightRecorder`] in the
/// process. [`is_enabled`] is `true` exactly while this is non-zero.
static RUNNING_RECORDINGS: std::sync::atomic::AtomicIsize = std::sync::atomic::AtomicIsize::new(0);

/// Publish a recorder's change in running-recording count.
///
/// `JFR_ENABLED` is process-global but a recorder's running set is not, so
/// storing `!running_ids.is_empty()` directly — as `refresh_running_ids` used
/// to — lets whichever recorder transitioned last decide the flag for all of
/// them. With one recorder per process that is invisible; with several it is
/// not, and `cratonvm-vm`'s test binary builds a `SharedVm`, and therefore a
/// `FlightRecorder`, per test. A recorder with no recordings would call
/// `set_enabled(false)` and silently switch JFR off underneath a concurrent
/// test that had just started one, so every `emit_*` on that thread returned
/// at its `is_enabled()` gate and the recording came back empty.
///
/// Tracking the total instead makes the flag mean what it says: some recording
/// somewhere is running. Recorders report their own delta, so they compose.
pub(crate) fn publish_running_delta(prev: usize, now: usize) {
    let delta = now as isize - prev as isize;
    let total = if delta == 0 {
        RUNNING_RECORDINGS.load(Ordering::Acquire)
    } else {
        RUNNING_RECORDINGS.fetch_add(delta, Ordering::AcqRel) + delta
    };
    debug_assert!(total >= 0, "running-recording count went negative: {total}");
    set_enabled(total > 0);
}

/// Create a new FlightRecorder with all built-in events registered.
pub fn create_flight_recorder() -> FlightRecorder {
    let mut fr = FlightRecorder::new();
    builtin::register_builtin_events(&mut fr.type_registry);
    fr
}

#[cfg(test)]
mod tests;
