// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Observability, diagnostics and the JVMTI/JDWP debug plumbing.
//!
//! Extracted verbatim from the former monolithic `SharedVm` struct.
//! Field types, lock types and lock levels are unchanged; only the
//! owning struct differs. Access paths are `shared.debug.<field>`.

use crate::vm::vm_init::MissingNativeEntry;
use std::sync::atomic::{AtomicI32, AtomicU32, AtomicUsize, Ordering};

/// Observability, diagnostics and the JVMTI/JDWP debug plumbing.
pub struct DebugRealm {
    /// T1.7.7 — set to `true` after the first HPROF dump-on-OOM so a
    /// tight allocation loop doesn't write thousands of dumps.
    pub oom_dump_written: std::sync::atomic::AtomicBool,

    /// Structured audit log of ACC_NATIVE methods that were invoked but had no
    /// Rust implementation. Populated when `config.audit_missing_natives` is
    /// true. Entries are deduped by `(class, method, descriptor)`; the first
    /// occurrence records the caller frame as a sample call-site which is
    /// useful for diagnosing which Java code transitively reaches the missing
    /// native.
    ///
    /// Schema written to disk by [`Self::dump_missing_natives_json`]:
    ///
    /// ```json
    /// {
    ///   "missing_natives": [
    ///     {
    ///       "class": "java/lang/Foo",
    ///       "name": "bar",
    ///       "descriptor": "(I)V",
    ///       "sample_call_site": "com/example/Main.main([Ljava/lang/String;)V"
    ///     }
    ///   ]
    /// }
    /// ```
    ///
    /// The ordering is the order of first occurrence. Two consecutive runs on
    /// the same program produce byte-identical JSON, which makes the file
    /// suitable as a committed baseline that can be diffed against new
    /// releases.
    pub missing_natives_log: parking_lot::Mutex<Vec<MissingNativeEntry>>,

    /// Java Flight Recorder — records VM events (GC, thread, class loading, compilation).
    pub flight_recorder: parking_lot::Mutex<cratonvm_jfr::FlightRecorder>,

    /// obsaudit D12 (2026-07-26) — `(recording_id, filename)` for the
    /// `-XX:StartFlightRecording` recording that should be dumped when the
    /// VM exits, if `dumponexit` was not explicitly set to `false`. `None`
    /// when no such recording exists (the flag was never passed) or when
    /// `dumponexit=false` was requested. Read by the pre-exit hook installed
    /// in `vm-cli/src/main.rs`'s `run()`.
    pub jfr_dump_on_exit: parking_lot::Mutex<Option<(u64, String)>>,

    /// The active recording created by the real-JDK jdk.jfr.Event bridge.
    /// Kept separate from -XX:StartFlightRecording, which may coexist.
    pub jfr_java_recording: parking_lot::Mutex<Option<u64>>,

    /// Whether that Java-owned recording is currently accepting events.
    pub jfr_java_recording_running: std::sync::atomic::AtomicBool,

    /// Last output path supplied by the Java JFR recorder.
    pub jfr_java_output: parking_lot::Mutex<Option<String>>,

    /// obsaudit D15 (2026-07-26) — the real attach-API jcmd processor,
    /// constructed in `Vm::new` (not here — it needs an `Arc<SharedVm>` for
    /// `VmDiagnosticState`, which does not exist yet during `SharedVm::new`)
    /// via `JcmdProcessor::new_with_vm_state`. Must be kept alive for the
    /// VM's lifetime: dropping it would not by itself close the listener's
    /// background thread or unlink the socket, but there is no reason to
    /// drop it early, and holding it here ties its lifetime to the most
    /// natural owner. `None` until `Vm::new` fills it in.
    pub jcmd_processor: parking_lot::Mutex<Option<crate::runtime::serviceability::JcmdProcessor>>,

    /// JVMTI debug state — breakpoints, step requests, and event callbacks.
    #[cfg(feature = "experimental-debug")]
    pub debug_state: parking_lot::Mutex<crate::debug::DebugState>,

    /// JVMTI environment — exposes the JVMTI interface to attached agents.
    #[cfg(feature = "experimental-debug")]
    pub jvmti_env: parking_lot::Mutex<crate::jvmti::JvmtiEnv>,

    /// Fast-path flag: true when at least one breakpoint is active.
    /// Checked on every interpreted instruction to skip the debug_state lock
    /// when no breakpoints are set.
    #[cfg(feature = "experimental-debug")]
    pub breakpoints_active: std::sync::atomic::AtomicBool,

    /// Event channel: interpreter threads send debug events here; the JDWP
    /// server thread drains and forwards them as composite event packets.
    #[cfg(feature = "experimental-debug")]
    pub debug_event_tx: std::sync::Mutex<Option<std::sync::mpsc::Sender<crate::debug::DebugEvent>>>,
    #[cfg(feature = "experimental-debug")]
    pub debug_event_rx:
        std::sync::Mutex<Option<std::sync::mpsc::Receiver<crate::debug::DebugEvent>>>,

    /// Diagnostic counters — atomic counters for bytecodes, GC, classes, etc.
    pub diagnostic_counters: crate::runtime::diagnostics::DiagnosticCounters,

    /// B6: Counter of silent swallows during class init / invokedynamic / native calls.
    /// Incremented whenever an error is swallowed (converted to a WARN) so the CLI
    /// can surface a summary after main() completes silently. Visible via tracing
    /// at WARN level. Setting CRATONVM_STRICT_SWALLOWS=1 escalates swallows to panics.
    pub swallow_counter: std::sync::atomic::AtomicU64,

    /// T19.H1 — cross-thread stack-dump-on-timeout flag.
    ///
    /// Set by the CLI watchdog thread (see `--stack-dump-on-timeout`) after
    /// the configured deadline elapses. Interpreter threads poll this flag
    /// on every dispatch loop iteration with a cheap `Ordering::Relaxed`
    /// load; when set, each thread writes its frame chain to stderr via
    /// [`Self::dump_current_thread_frames`] and increments
    /// [`stack_dump_ack_count`] so the watchdog can confirm all runtime
    /// threads responded before calling `std::process::abort`.
    ///
    /// This is a diagnostic-only mechanism — the flag is one-shot and is
    /// never cleared; the process is expected to abort soon after.
    pub stack_dump_requested: std::sync::atomic::AtomicBool,

    /// T19.H1 — count of threads that have flushed their frame dump to
    /// stderr in response to [`stack_dump_requested`]. The watchdog uses
    /// this to decide how long to wait for interpreter threads to respond
    /// before aborting the process.
    pub stack_dump_ack_count: std::sync::atomic::AtomicU32,

    /// T19.H1 — the thread ids behind [`stack_dump_ack_count`], in ack order.
    ///
    /// The count alone cannot answer the question the summary needs: *which*
    /// running threads stayed silent. A thread that is RUNNING and did not ack
    /// is not in the interpreter at all — it is in JIT-compiled code or a long
    /// native call — and saying so is the difference between a five-minute
    /// diagnosis and the wrong one. See
    /// [`crate::threading::thread_registry::ThreadRegistry::render_thread_summary_for`].
    pub stack_dump_acked_tids: parking_lot::Mutex<Vec<u64>>,

    /// Sampling mode for [`Self::stack_dump_requested`] (see `--stack-sample-ms`).
    ///
    /// The watchdog's dump is one-shot: the flag is never cleared and each
    /// `execute()` invocation dumps at most once, so what it produces is one
    /// record per *nested interpreter entry* — a call-count trace wearing a
    /// profile's clothes, in which a method entered 100k times cheaply
    /// outranks the one that actually burned the wall clock.
    ///
    /// With this flag set the interpreter clears the request after dumping,
    /// so a sampler thread re-arming it every N ms yields one dump per
    /// interval per running thread: a time-weighted Java-level profile.
    /// Diagnostic-only, and off unless `--stack-sample-ms` is passed — when
    /// off the hot loop pays exactly the `stack_dump_pending()` load it
    /// already paid.
    pub stack_sample_mode: std::sync::atomic::AtomicBool,
}
