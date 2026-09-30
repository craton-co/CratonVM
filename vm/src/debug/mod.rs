// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! JDWP (Java Debug Wire Protocol) foundation.
//!
//! This module implements the wire protocol that IDE debuggers (IntelliJ,
//! Eclipse, VS Code) use to attach to a running JVM.  It provides:
//!
//! - **Transport** — TCP listener with JDWP handshake (`transport.rs`)
//! - **Protocol** — Packet serialization/deserialization (`protocol.rs`)
//! - **Commands** — Handlers for the standard JDWP command sets (`commands.rs`)
//! - **Inspect** — The commands that read or write the heap (`inspect.rs`)
//! - **Events** — Breakpoints, thread events, VM lifecycle (`events.rs`)
//! - **IDs** — Wire-level ID management (`ids.rs`)

pub mod commands;
pub(crate) mod early_return;
pub mod events;
pub mod ids;
pub mod inspect;
pub(crate) mod pop_frames;
pub mod protocol;
pub mod transport;

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use commands::{FieldInfo, MethodInfo};
use events::EventManager;
use ids::IdManager;
use ids::ThreadId;
use ids::{ObjectExport, ObjectTable};
use protocol::JdwpPacket;
use cratonvm_native_api::NativeHeapAccess as _;
use transport::JdwpConnection;

// ---------------------------------------------------------------------------
// LocalValue — debugger-visible local variable values
// ---------------------------------------------------------------------------

/// A debugger-visible local variable value.
#[derive(Debug, Clone)]
pub enum LocalValue {
    Int(i32),
    Long(i64),
    Float(f32),
    Double(f64),
    ObjectRef(u64), // object ID (0 = null)
}

impl LocalValue {
    pub fn as_int(&self) -> Option<i32> {
        match self {
            LocalValue::Int(v) => Some(*v),
            _ => None,
        }
    }
    pub fn as_long(&self) -> Option<i64> {
        match self {
            LocalValue::Long(v) => Some(*v),
            _ => None,
        }
    }
    pub fn as_float_bits(&self) -> Option<u32> {
        match self {
            LocalValue::Float(v) => Some(v.to_bits()),
            _ => None,
        }
    }
    pub fn as_double_bits(&self) -> Option<u64> {
        match self {
            LocalValue::Double(v) => Some(v.to_bits()),
            _ => None,
        }
    }
    pub fn as_object_id(&self) -> Option<u64> {
        match self {
            LocalValue::ObjectRef(v) => Some(*v),
            _ => None,
        }
    }
}

// ---------------------------------------------------------------------------
// DebugEvent — queued from interpreter threads to the JDWP server
// ---------------------------------------------------------------------------

/// An event sent from an interpreter thread to the JDWP server thread.
#[derive(Debug, Clone)]
pub struct DebugEvent {
    /// The event kind (Breakpoint, SingleStep, ThreadStart, ThreadDeath, etc.).
    pub kind: events::EventKind,
    /// The matched request ID (0 for unsolicited VM events).
    pub request_id: u32,
    /// The suspend policy from the matching request.
    pub suspend_policy: events::SuspendPolicy,
    /// Wire-level thread ID.
    pub thread_id: u64,
    /// Location: class_id, method_id, bytecode offset.
    pub class_id: u64,
    pub method_id: u64,
    pub offset: u64,
    /// The event's data after its location, already encoded (wave 10): an
    /// `Exception` event's tagged exception and catch location, a field
    /// event's field, object and (modification) new value. Empty for the
    /// other kinds.
    pub extra: Vec<u8>,
    /// More events of the same JDWP event set follow this one on the channel
    /// (interpreter round i1 wave 22, lane L1). The events one hook reports
    /// at one point — every request a breakpoint, a step landing on it, an
    /// exception or a field access matches — are one composite packet with
    /// the strongest of their suspend policies, and the thread is suspended
    /// ONCE for it, as HotSpot's back end reports an event bag
    /// (`eventHelper_reportEvents`, co-located events included). A producer
    /// sends a set contiguously, under the channel's mutex
    /// ([`send_event_set`]); the drain composes up to the event with this
    /// `false`.
    pub set_follows: bool,
}

/// The strongest of `policies` (`SUSPEND_ALL` over `SUSPEND_EVENT_THREAD`
/// over `SUSPEND_NONE`): the policy of a JDWP event set, which the thread
/// that reports it applies once (interpreter round i1 wave 22, lane L1; JDWP
/// `Event.Composite`: "the suspend policy of the composite event is the
/// strongest of the policies of its events"). `SUSPEND_NONE` for no events.
pub(crate) fn strongest_suspend_policy(
    policies: impl IntoIterator<Item = events::SuspendPolicy>,
) -> events::SuspendPolicy {
    policies
        .into_iter()
        .max_by_key(|&p| p as u8)
        .unwrap_or(events::SuspendPolicy::None)
}

/// Queue one JDWP event set on the server's channel: contiguously, under
/// the channel's mutex, with [`DebugEvent::set_follows`] on every event but
/// the last, so the drain sends them as one composite packet (wave 22).
/// Dropped when no debugger is attached. Called without the debug-state
/// lock.
pub(crate) fn send_event_set(shared: &crate::vm::SharedVm, mut set: Vec<DebugEvent>) {
    let Some(last) = set.len().checked_sub(1) else {
        return;
    };
    for (i, event) in set.iter_mut().enumerate() {
        event.set_follows = i != last;
    }
    if let Ok(guard) = shared.debug.debug_event_tx.lock() {
        if let Some(ref tx) = *guard {
            for event in set {
                let _ = tx.send(event);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// T6.5 — DebuggerValue and DebuggerVmBridge
// ---------------------------------------------------------------------------

/// A JDWP-typed value crossing the debugger/VM boundary.
///
/// Parsed from JDWP `value` byte strings on the wire and produced by
/// method-invocation bridges when returning a result to the debugger.
#[derive(Debug, Clone, PartialEq)]
pub enum DebuggerValue {
    /// JDWP tag `V` — `void` return.
    Void,
    /// JDWP tag `Z` — boolean (0/1).
    Boolean(u8),
    /// JDWP tag `B` — signed byte.
    Byte(i8),
    /// JDWP tag `C` — UTF-16 char.
    Char(u16),
    /// JDWP tag `S` — signed short.
    Short(i16),
    /// JDWP tag `I` — int.
    Int(i32),
    /// JDWP tag `J` — long.
    Long(i64),
    /// JDWP tag `F` — float (raw bit pattern).
    Float(u32),
    /// JDWP tag `D` — double (raw bit pattern).
    Double(u64),
    /// JDWP tag `L` — object reference (0 = null).
    Object(u64),
    /// JDWP tag `[` — array reference (0 = null).
    Array(u64),
    /// JDWP tag `s` — string reference (0 = null).
    String(u64),
    /// Thread reference (`t`).
    Thread(u64),
    /// ThreadGroup reference (`g`).
    ThreadGroup(u64),
    /// ClassLoader reference (`l`).
    ClassLoader(u64),
    /// ClassObject reference (`c`).
    ClassObject(u64),
}

impl DebuggerValue {
    /// The JDWP type tag byte for this value.
    pub fn tag(&self) -> u8 {
        match self {
            DebuggerValue::Void => b'V',
            DebuggerValue::Boolean(_) => b'Z',
            DebuggerValue::Byte(_) => b'B',
            DebuggerValue::Char(_) => b'C',
            DebuggerValue::Short(_) => b'S',
            DebuggerValue::Int(_) => b'I',
            DebuggerValue::Long(_) => b'J',
            DebuggerValue::Float(_) => b'F',
            DebuggerValue::Double(_) => b'D',
            DebuggerValue::Object(_) => b'L',
            DebuggerValue::Array(_) => b'[',
            DebuggerValue::String(_) => b's',
            DebuggerValue::Thread(_) => b't',
            DebuggerValue::ThreadGroup(_) => b'g',
            DebuggerValue::ClassLoader(_) => b'l',
            DebuggerValue::ClassObject(_) => b'c',
        }
    }

    /// A null object reference (tagged `L`, ID 0).
    pub fn null() -> Self {
        DebuggerValue::Object(0)
    }
}

/// Outcome of a debugger-initiated method invocation.
///
/// Exactly one of `return_value` or `exception` is meaningful: if the method
/// throws, `exception` holds the wire ID of the thrown Throwable and
/// `return_value` is `Void` (tag `V`).  Otherwise `exception` is `0`.
#[derive(Debug, Clone)]
pub struct InvokeOutcome {
    pub return_value: DebuggerValue,
    /// Wire ID of the thrown exception (0 = no exception).
    pub exception: u64,
}

impl InvokeOutcome {
    pub fn returned(value: DebuggerValue) -> Self {
        Self {
            return_value: value,
            exception: 0,
        }
    }

    pub fn threw(exception_id: u64) -> Self {
        Self {
            return_value: DebuggerValue::null(),
            exception: exception_id,
        }
    }
}

/// Error returned when a debugger bridge call cannot be dispatched (e.g.
/// the thread ID is unknown).  This surfaces as a JDWP error code in the
/// reply packet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BridgeError {
    /// The target thread ID did not resolve to a live thread.
    InvalidThread,
    /// The target thread is not suspended at a point where the VM can run
    /// work on it (wave 8: an invocation runs on its own thread, parked at an
    /// interpreter suspend point), or no thread is (an allocation).
    ThreadNotSuspended,
    /// The target class ID was invalid.
    InvalidClass,
    /// The method ID names no method of the class (wave 8: this answered
    /// `InvalidClass`).
    InvalidMethod,
    /// The target object ID was invalid.
    InvalidObject,
    /// The arguments do not fit the method (count), or a length is negative.
    IllegalArgument,
    /// The thread is already running an invocation (wave 9; JDWP
    /// `ALREADY_INVOKING`).
    AlreadyInvoking,
    /// Any other internal error from the VM.
    Internal,
}

/// Live-VM hooks needed by the JDWP command handlers to execute code in the
/// target VM.
///
/// Implementors forward `invoke_static`/`invoke_virtual`/`invoke_special`
/// onto the live `SharedVm` and expose array allocation.  The bridge is
/// installed on [`DebugState`] when the VM accepts a debugger connection
/// and removed on disconnect.
///
/// The bridge is kept behind a trait (rather than a direct `SharedVm`
/// reference) to avoid a hard module cycle: `debug/commands.rs` does not
/// need to depend on `vm::SharedVm` at compile time, which keeps unit
/// tests on the debug subtree lightweight and lets us test the handler
/// code path with a mock bridge.
pub trait DebuggerVmBridge: Send + Sync {
    /// Invoke a static method on the given class in the context of `thread_id`.
    ///
    /// `single_threaded` is JDWP's `INVOKE_SINGLE_THREADED` (wave 9): only
    /// `thread_id` runs for the call; without it every suspended thread is
    /// resumed for the call and suspended again after it.
    fn invoke_static(
        &self,
        class_id: u64,
        method_id: u64,
        thread_id: u64,
        args: &[DebuggerValue],
        return_sig: &str,
        single_threaded: bool,
    ) -> Result<InvokeOutcome, BridgeError>;

    /// Invoke an instance method on a receiver in the context of `thread_id`.
    ///
    /// If `non_virtual` is true, dispatch is resolved statically against
    /// `class_id` (used for `ObjectReference.InvokeMethod` with option 2,
    /// "invoke non-virtual"). `single_threaded` as for
    /// [`Self::invoke_static`].
    #[allow(clippy::too_many_arguments)]
    fn invoke_instance(
        &self,
        receiver_id: u64,
        class_id: u64,
        method_id: u64,
        thread_id: u64,
        args: &[DebuggerValue],
        return_sig: &str,
        non_virtual: bool,
        single_threaded: bool,
    ) -> Result<InvokeOutcome, BridgeError>;

    /// Allocate a new array of the given element type and length.
    /// Returns the wire ID of the newly allocated array.
    fn new_array(&self, array_type_id: u64, length: i32) -> Result<u64, BridgeError>;

    /// `ClassType.NewInstance` (interpreter round i1 wave 24): allocate an
    /// instance of `class_id` and run its constructor `method_id` on thread
    /// `thread_id`, as an invocation (the same thread, suspension and
    /// `single_threaded` rules as [`Self::invoke_static`]). The outcome's
    /// value is the new object; `null` when the constructor threw, with the
    /// exception. A bridge that cannot allocate refuses it.
    fn new_instance(
        &self,
        class_id: u64,
        method_id: u64,
        thread_id: u64,
        args: &[DebuggerValue],
        single_threaded: bool,
    ) -> Result<InvokeOutcome, BridgeError> {
        let _ = (class_id, method_id, thread_id, args, single_threaded);
        Err(BridgeError::Internal)
    }
}

// ---------------------------------------------------------------------------
// DebugState
// ---------------------------------------------------------------------------

/// Central debug state combining ID management, event tracking, class
/// metadata, and connection bookkeeping.
pub struct DebugState {
    /// Wire-level ID manager.
    pub ids: IdManager,
    /// JDWP object ids for heap objects (wave 7): stable across collections.
    /// See [`export_object`] / [`object_for_id`].
    pub objects: ObjectTable,
    /// Event request manager.
    pub events: EventManager,

    // -- Suspension (JDWP suspend counts, wave 7) ------------------------------
    /// VM-wide suspensions in force (`VirtualMachine.Suspend`, `SUSPEND_ALL`
    /// events): every thread's suspend count includes it.
    suspend_all_count: u32,
    /// Per-thread adjustment of a thread's suspend count against
    /// `suspend_all_count` (`ThreadReference.Suspend` / `Resume`, a
    /// `SUSPEND_EVENT_THREAD` event). A thread's count is the sum, never
    /// below zero; ask [`DebugState::suspend_count`]. HotSpot keeps one
    /// counter per thread: a thread suspended n times runs after n resumes,
    /// and `VirtualMachine.Resume` takes one from every thread's count.
    /// Until wave 7 this was a set and a flag, so a thread suspended both on
    /// its own and by the VM ran after one resume.
    suspend_adjust: HashMap<u64, i64>,
    /// Threads a C JVMTI agent suspended (`SuspendThread`, interpreter round
    /// i1 wave 45, lane L1; `jvmti::native_env`). JVMTI's suspension is a
    /// flag, not a count: a second `SuspendThread` is refused
    /// (`JVMTI_ERROR_THREAD_SUSPENDED`) and one `ResumeThread` ends it. It
    /// parks the thread as a JDWP suspension does
    /// ([`Self::is_thread_suspended`]), and a debugger's detach
    /// ([`Self::resume_all`]) leaves it in force: it is the agent's.
    jvmti_suspended: HashSet<u64>,
    /// Short suspensions the C JVMTI table takes to read a running thread's
    /// stack (wave 45: HotSpot reads it through a handshake), counted per
    /// thread: the reader waits for the thread to stop, reads the listing it
    /// published, and ends its suspension ([`Self::end_handshake`]).
    jvmti_handshakes: HashMap<u64, u32>,
    /// JDWP frame generations (interpreter round i1 wave 28, lane L1): a
    /// thread's generation is `frame_epoch + frame_generations[tid]`
    /// (wrapping), and it moves exactly when the thread's suspend count
    /// reaches zero — HotSpot's `frameGeneration`, incremented on every
    /// resume (`threadControl.c` `resumeThreadByNode`). A published frame id
    /// is `generation << 32 | position` ([`DebugState::frame_id`]), so an id
    /// from before a resume is in no later listing and a frame command
    /// refuses it with `INVALID_FRAMEID` (`commands::frame_refusal`); until
    /// wave 28 ids were bare positions and a stale one named whatever frame
    /// stood there after the next suspension.
    ///
    /// `frame_epoch` covers the threads `VirtualMachine.Resume` takes to zero
    /// all at once (those with no `suspend_adjust` entry, which this state
    /// cannot enumerate); a thread that stays suspended through that resume
    /// has its own part lowered by one, so its generation, and the ids of a
    /// listing republished within the same suspension, do not move.
    frame_epoch: u32,
    /// Per-thread part of the frame generation; see [`Self::frame_epoch`].
    frame_generations: HashMap<u64, u32>,
    /// Bumped when the debugger detaches ([`DebugState::resume_all`]), so an
    /// invocation still running then does not re-suspend threads for a
    /// session that is gone (wave 9, [`run_invocation_on_parked_thread`]).
    session: u64,
    /// Whether the debug session has been disposed.
    pub disposed: bool,
    /// `VirtualMachine.HoldEvents` is in force (interpreter round i1 wave
    /// 23): the server sends no event until `ReleaseEvents`. The events are
    /// held, not dropped — they wait on the event channel, and the classes
    /// defined meanwhile are picked up by the first poll after the release.
    /// JDI sends the pair itself when its event queue backs up.
    pub events_held: bool,
    /// If `Some`, the VM was asked to exit with this code.
    pub exit_code: Option<i32>,
    /// The session's `VM_DEATH` event set has been written (interpreter
    /// round i1 wave 24): [`report_vm_death`] queues it and waits for this.
    vm_death_sent: bool,
    /// `ThreadReference.Stop` (interpreter round i1 wave 24): thread id → the
    /// object id of the throwable it must throw at its next interpreted
    /// bytecode ([`take_pending_stop`]). The id's collection is disabled
    /// until then, so the throwable stays alive when the debugger lets go
    /// of it.
    pending_stops: HashMap<u64, u64>,
    /// `ThreadReference.ForceEarlyReturn` (interpreter round i1 wave 43):
    /// the parks it is served at and the values recorded
    /// ([`early_return`]).
    pub(crate) early_returns: early_return::EarlyReturns,
    /// The threads the VM started as virtual threads (interpreter round i1
    /// wave 25): recorded at their `ThreadStart` ([`send_thread_event_of`]),
    /// where the VM knows it from the `Thread` object's class, before the
    /// virtual-thread table does. Read by `PlatformThreadsOnly` and
    /// `ThreadReference.IsVirtual`.
    virtual_threads: HashSet<u64>,
    /// Next packet ID for packets sent by the VM.
    pub next_packet_id: u32,

    // -- Class metadata (populated by the VM) ------------------------------
    /// Wire ref-type ID → JDWP type tag (1 class, 2 interface, 3 array).
    /// Keyed by class (interpreter round i1 wave 15): it was a map from the
    /// signature, which kept one class per name, so a class that two loaders
    /// define was listed once and `ClassesBySignature` found only the last
    /// one defined.
    pub class_type_tags: HashMap<u64, u8>,
    /// Wire ref-type ID → JNI signature.
    pub class_signatures: HashMap<u64, String>,
    /// Wire ref-type ID → source file name.
    pub class_source_files: HashMap<u64, String>,
    /// Wire ref-type ID → field list.
    pub class_fields: HashMap<u64, Vec<FieldInfo>>,
    /// Wire ref-type ID → method list.
    pub class_methods: HashMap<u64, Vec<MethodInfo>>,
    /// Class-store slots whose classes the metadata above already covers
    /// (`populate_class_metadata`, wave 7).
    pub class_slots_seen: usize,
    /// Wire ref-type ID → JDWP `ClassStatus` bits (`jdwp_class_status`),
    /// refreshed from the class manager before the commands that answer a
    /// class's status (interpreter round i1 wave 15,
    /// `refresh_class_statuses`). A class missing here (a bare
    /// `DebugState`) answers VERIFIED | PREPARED, what every class answered
    /// before.
    pub class_statuses: HashMap<u64, u32>,
    /// Classes a `ClassPrepare` event has been considered for this session
    /// (wave 10): by the thread that prepared the class
    /// ([`class_prepared_on_thread`]) or, after the fact, by the server's
    /// poll (`send_class_prepare_events`) — whichever came first; the other
    /// skips it.
    class_prepare_reported: HashSet<u64>,
    /// Classes the server's poll found defined but not yet prepared (wave
    /// 10), as `(wire class id, JNI signature, type tag)` and when first
    /// seen: their `ClassPrepare` is left to the thread that prepares them,
    /// and reported by the poll only once they are prepared (by a path that
    /// did not report them) or [`CLASS_PREPARE_GRACE`] has passed.
    class_prepare_pending: Vec<((u64, String, u8), std::time::Instant)>,

    // -- Thread metadata ---------------------------------------------------
    /// Wire thread ID → human-readable name.
    pub thread_names: HashMap<u64, String>,
    /// Wire thread ID → JDWP `ThreadStatus` (`commands::THREAD_STATUS_*`),
    /// refreshed with `thread_names` from the thread registry (wave 11,
    /// [`populate_thread_metadata`]). A dead thread the registry still keeps
    /// is `THREAD_STATUS_ZOMBIE`: named, answered by `Status`, not listed.
    pub thread_statuses: HashMap<u64, u32>,
    /// Wire thread ID → the JDWP `ThreadStatus` of a thread parked at the
    /// return of a native method it was suspended in (interpreter round i1
    /// wave 45, lane L1; `interpreter::park_if_suspended_at_native_exit`),
    /// for a native HotSpot still reports in its own state there:
    /// `Object.wait` stays `WAIT` (measured on HotSpot 25.0.3,
    /// `tools/probes/interp/L1/L1W45JdiSuspendedInNative.java`). The thread
    /// is a running mutator while parked, so the registry reads `RUNNING`;
    /// [`populate_thread_metadata`] takes this instead.
    pub(crate) native_exit_statuses: HashMap<u64, u32>,

    // -- Work handed to parked threads (wave 8) -----------------------------
    /// Threads parked at an interpreter suspend point
    /// (`interpreter::park_for_debugger`): registered mutators that the
    /// collector stops and scans, which is where the JDWP server has Java run
    /// and the heap read ([`run_on_parked_thread`]).
    ///
    /// One entry per nesting level of the thread's park, innermost last,
    /// `true` while that level runs a task (wave 9): a stop inside a debugger
    /// invocation parks the thread again, above the invocation's frames, and
    /// only the innermost level, idle, can take work. It was a set, which a
    /// nested park's exit emptied while the outer park still stood.
    parked_threads: HashMap<u64, Vec<bool>>,
    /// Per level of [`Self::parked_threads`], innermost last: did an event
    /// suspend the thread for that park (interpreter round i1 wave 44, lane
    /// L1)? Only such a park takes a debugger invocation, as HotSpot's back
    /// end invokes only on a thread an event suspended (its invoker's
    /// `available` flag): one suspended by `VirtualMachine.Suspend`, a
    /// `ThreadReference.Suspend` or the re-suspension that ends another
    /// thread's invocation is `INVALID_THREAD`
    /// (`tools/probes/interp/L1/L1W43RawJdwpObjectErrorAnswers.java`,
    /// `invokeMethod(on running other)`: 10 on HotSpot 25.0.3).
    event_parks: HashMap<u64, Vec<bool>>,
    /// Threads an event set's suspend policy suspended
    /// ([`Self::apply_event_set_policy`]) that have not parked for it yet:
    /// the next park of each is an event park ([`Self::note_parked`]).
    event_park_pending: HashSet<u64>,
    /// Work for a parked thread, taken by that thread's park loop
    /// ([`DebugState::take_parked_task`]).
    parked_tasks: HashMap<u64, ParkedTask>,
    /// The heap service (wave 10, [`start_heap_service`]): a daemon thread
    /// attached to the VM that runs heap work — a heap command, an
    /// allocation — when no thread is parked at a suspend point. The job
    /// channel and the service's thread id, which the debugger never sees.
    heap_service: Option<(
        std::sync::mpsc::Sender<crate::native::jni::AttachedServiceJob>,
        u64,
    )>,

    // -- Frame local variable snapshots ---------------------------------------
    /// Frame local variable snapshots: (thread_id, frame_id) -> Vec<LocalValue>
    pub frame_locals: HashMap<(u64, u64), Vec<LocalValue>>,

    // -- Frame stack snapshots (for TR_FRAMES / TR_FRAME_COUNT) --------------
    /// Per-thread frame stack snapshots: thread_id -> list of frame entries.
    /// Populated when a thread is suspended (breakpoint/single-step).
    pub thread_frames: HashMap<u64, Vec<FrameEntry>>,
    /// The published frames that have no locals to read (interpreter round
    /// i1 wave 21, lane L3): `(thread, frame id) -> is a compiled activation`.
    /// `true` for a compiled (or inlined) activation a blocked thread stands
    /// under, which has no interpreter frame at all; `false` for an
    /// interpreter frame whose body runs compiled, whose `Frame` locals are
    /// the values at the compiled entry. `GetValues`, `SetValues` and
    /// `ThisObject` answer `OPAQUE_FRAME` for both, as HotSpot does for a
    /// frame it cannot describe; a step counts only interpreter frames.
    /// Withdrawn with the thread's snapshot.
    pub opaque_frames: HashMap<(u64, u64), bool>,
    /// `StackFrame.SetValues` writes to the frames of a thread suspended
    /// while it was blocked in a native (interpreter round i1 wave 41, lane
    /// L1): thread id → the writes, in the order they were sent. Recorded
    /// against the snapshot published through the thread's inspection window
    /// (`inspect::defer_blocked_frame_write`, which also refreshes that
    /// snapshot), and applied by the thread itself as it leaves the blocking
    /// region (`inspect::apply_deferred_frame_writes_if_any`), after the
    /// collections it slept through are folded into its frames. An object
    /// value is held by its id, collection disabled and pinned, until then.
    /// [`DebuggerGates::frame_writes_pending`] is up while this is not empty.
    pub(crate) deferred_frame_writes: HashMap<u64, Vec<DeferredFrameWrite>>,

    // -- T6.4 additional JDWP command support ---------------------------------
    /// Class hierarchy: class_id → superclass_id.
    pub class_superclass: HashMap<u64, u64>,
    /// Method line tables: (ref_type_id, method_id) → Vec<(code_index, line_number)>.
    pub method_line_tables: HashMap<(u64, u64), Vec<(u64, i32)>>,
    /// Method variable tables: (ref_type_id, method_id) → Vec<VariableInfo>.
    pub method_variables: HashMap<(u64, u64), Vec<VariableInfo>>,
    /// Method bytecodes: (ref_type_id, method_id) → raw bytecode bytes.
    pub method_bytecodes: HashMap<(u64, u64), Vec<u8>>,
    /// Wire ref-type ID → what the `ReferenceType` and `Method` commands a
    /// JDI session sends need beyond the names and members above
    /// (interpreter round i1 wave 23, [`ClassDetails`]). A class missing here
    /// (a bare `DebugState`) answers what it answered before wave 23.
    pub class_details: HashMap<u64, ClassDetails>,
    // Wave 8: the side tables `object_class_map`, `array_lengths`,
    // `array_elements`, `array_type_tags`, `class_object_to_ref_type` and the
    // `CreateString` text table are gone. The object commands read the heap
    // (`inspect`); nothing filled those tables, so every real object answered
    // class 0, `null` fields, empty arrays and `""`.

    // Wave 10: `field_access_watchpoints` / `field_modification_watchpoints`
    // are gone. They copied the watch requests the request table already
    // holds, and nothing delivered from them.

    // Wave 11: `exceptions_seen` (the per-thread "exception already
    // reported" handles) moved onto the thread, `JvmThread::reported_exception`,
    // shared by the JDWP and JVMTI `Exception` events.

    // Wave 11: `breakpoint_conditions` is gone. It copied each breakpoint's
    // `Count` modifier with a different meaning (fire on exactly the n-th
    // hit, ignoring the modifiers before it), and nothing read it; the
    // request's own modifier (`events::location_request_reports`) is the
    // one source of truth.

    // -- T6.5 Debugger-initiated method invocation ---------------------------
    /// Optional live-VM bridge, installed when a debugger attaches.
    ///
    /// When present, `ClassType.InvokeMethod`, `ObjectReference.InvokeMethod`
    /// and `ArrayType.NewInstance` forward to the running VM.  When absent
    /// (e.g. in unit tests) those commands fail with `ERR_VM_DEAD`.
    pub vm_bridge: Option<Arc<dyn DebuggerVmBridge>>,
}

/// Local variable metadata for debugger inspection.
#[derive(Debug, Clone)]
pub struct VariableInfo {
    /// Start of scope (bytecode index).
    pub code_index: u64,
    /// Variable name.
    pub name: String,
    /// JNI type signature (e.g., "I", "Ljava/lang/String;").
    pub signature: String,
    /// Length of scope in bytecodes.
    pub length: usize,
    /// Slot index in the local variable table.
    pub slot: usize,
}

/// One class's metadata for the JDWP commands a JDI session sends beyond
/// `Signature` / `Fields` / `Methods` (interpreter round i1 wave 23): JDI asks
/// a JDWP 1.5+ target for `SignatureWithGeneric`, `FieldsWithGeneric` and
/// `MethodsWithGeneric` (2/13-15), and for `Modifiers` (2/3) and `Interfaces`
/// (2/10) as soon as it lists a class's methods, so until wave 23 no line
/// breakpoint could be set from JDI (`jdb`, every IDE). Filled by
/// `add_class_metadata`.
#[derive(Debug, Clone, Default)]
pub struct ClassDetails {
    /// `ReferenceType.Modifiers`, as HotSpot's JVMTI `GetClassModifiers`
    /// answers it (`jdwp_class_modifiers`).
    pub modifiers: u32,
    /// The class's `Signature` attribute (its generic signature), if any.
    pub generic_signature: Option<String>,
    /// Wire ref-type IDs of the direct superinterfaces, in declaration
    /// order; none for an array class (JVMTI `GetImplementedInterfaces`).
    pub interfaces: Vec<u64>,
    /// `(major, minor)` class-file version; `None` for an array class
    /// (`ClassFileVersion` answers `ABSENT_INFORMATION` for it).
    pub version: Option<(u16, u16)>,
    /// Method ID → the method's `Signature` attribute.
    pub method_generics: HashMap<u64, String>,
    /// Field ID → the field's `Signature` attribute.
    pub field_generics: HashMap<u64, String>,
    /// Method ID → the length of the method's bytecode, for every method
    /// that has code: `Method.LineTable` answers `start` 0 and `end`
    /// `length - 1`, the range JDI checks every location of the method
    /// against.
    pub code_lengths: HashMap<u64, u64>,
    /// Method ID → the method's `LocalVariableTypeTable` as `(slot, scope
    /// start, generic signature)`: `VariableTableWithGeneric`'s generic
    /// signatures.
    pub variable_generics: HashMap<u64, Vec<(usize, u64, String)>>,
}

/// Move thread `tid`'s own part of its JDWP frame generation by `delta`
/// (wrapping; `u32::MAX` takes one off). See `DebugState::frame_epoch`.
fn bump_frame_generation(generations: &mut HashMap<u64, u32>, tid: u64, delta: u32) {
    let own = generations.entry(tid).or_insert(0);
    *own = own.wrapping_add(delta);
}

/// The [`FrameEntry::offset`] of a native method's frame: JDWP location
/// index -1, as HotSpot reports a native frame (interpreter round i1 wave 15,
/// `interpreter::read_blocked_frames`).
pub const NATIVE_FRAME_LOCATION: u64 = u64::MAX;

/// One deferred `StackFrame.SetValues` write (interpreter round i1 wave 41,
/// lane L1; [`DebugState::deferred_frame_writes`]).
#[derive(Debug, Clone, Copy)]
pub(crate) struct DeferredFrameWrite {
    /// The frame's index among the thread's interpreter frames, bottom first.
    pub(crate) frame_index: usize,
    /// How many interpreter frames the thread had when the write was
    /// recorded: a thread whose stack changed since applies nothing.
    pub(crate) frame_count: usize,
    /// The frame's class id and JDWP method id, checked again when applied.
    pub(crate) class_id: u64,
    pub(crate) method_id: u64,
    /// The local's slot.
    pub(crate) slot: u16,
    /// The value to store.
    pub(crate) value: DeferredLocal,
}

/// The value of a [`DeferredFrameWrite`].
#[derive(Debug, Clone, Copy)]
pub(crate) enum DeferredLocal {
    /// A primitive, or `null`.
    Plain(crate::types::Value),
    /// The object an id names; its collection is disabled and the id pinned
    /// until the write is applied or dropped.
    Object(u64),
}

/// A single stack frame entry for debugger frame inspection.
#[derive(Debug, Clone)]
pub struct FrameEntry {
    /// Wire-level frame ID: the thread's frame generation over the frame's
    /// position in the listing (wave 28, [`DebugState::frame_id`]).
    pub frame_id: u64,
    /// Class ID (from ClassId::as_u32()).
    pub class_id: u64,
    /// Method ID (`jdwp_method_id` of the method's name and descriptor).
    pub method_id: u64,
    /// Current bytecode offset.
    pub offset: u64,
}

impl DebugState {
    pub fn new() -> Self {
        Self {
            ids: IdManager::new(),
            objects: ObjectTable::new(),
            events: EventManager::new(),
            suspend_all_count: 0,
            suspend_adjust: HashMap::new(),
            jvmti_suspended: HashSet::new(),
            jvmti_handshakes: HashMap::new(),
            frame_epoch: 0,
            frame_generations: HashMap::new(),
            session: 0,
            disposed: false,
            events_held: false,
            exit_code: None,
            vm_death_sent: false,
            pending_stops: HashMap::new(),
            early_returns: early_return::EarlyReturns::default(),
            virtual_threads: HashSet::new(),
            next_packet_id: 1,
            class_type_tags: HashMap::new(),
            class_signatures: HashMap::new(),
            class_source_files: HashMap::new(),
            class_fields: HashMap::new(),
            class_methods: HashMap::new(),
            class_slots_seen: 0,
            class_statuses: HashMap::new(),
            class_prepare_reported: HashSet::new(),
            class_prepare_pending: Vec::new(),
            thread_names: HashMap::new(),
            thread_statuses: HashMap::new(),
            native_exit_statuses: HashMap::new(),
            parked_threads: HashMap::new(),
            event_parks: HashMap::new(),
            event_park_pending: HashSet::new(),
            parked_tasks: HashMap::new(),
            heap_service: None,
            frame_locals: HashMap::new(),
            thread_frames: HashMap::new(),
            opaque_frames: HashMap::new(),
            deferred_frame_writes: HashMap::new(),
            class_superclass: HashMap::new(),
            method_line_tables: HashMap::new(),
            method_variables: HashMap::new(),
            method_bytecodes: HashMap::new(),
            class_details: HashMap::new(),
            vm_bridge: None,
        }
    }

    /// Look up frame local variable snapshot for the given thread and frame.
    pub fn get_frame_locals(&self, thread_id: ThreadId, frame_id: u64) -> Option<&Vec<LocalValue>> {
        self.frame_locals.get(&(thread_id.0, frame_id))
    }

    /// Store a snapshot of local variables for a given thread/frame
    /// (typically called when the VM suspends a thread).
    pub fn update_frame_locals(&mut self, thread_id: u64, frame_id: u64, locals: Vec<LocalValue>) {
        self.frame_locals.insert((thread_id, frame_id), locals);
    }

    /// The JDWP type tag a location in class `class_id` carries
    /// (interpreter round i1 wave 15): the class's own tag — `INTERFACE` (2)
    /// for a static or default method of an interface, as HotSpot's back end
    /// writes it (`referenceTypeTag`) — else `CLASS` (1), which every location
    /// carried. JDI creates the location's `ReferenceType` from this tag, so a
    /// frame in an interface method made an interface a `ClassType` mirror.
    pub fn location_type_tag(&self, class_id: u64) -> u8 {
        self.class_type_tags.get(&class_id).copied().unwrap_or(1)
    }

    /// Every class the session knows, as `(wire ref-type ID, type tag, JNI
    /// signature)`, in id order (wave 15; `AllClassesWithGeneric`,
    /// `ClassLoaderReference.VisibleClasses`).
    pub fn known_classes(&self) -> Vec<(u64, u8, &str)> {
        let mut classes: Vec<(u64, u8, &str)> = self
            .class_signatures
            .iter()
            .map(|(&id, sig)| (id, self.location_type_tag(id), sig.as_str()))
            .collect();
        classes.sort_unstable_by_key(|&(id, _, _)| id);
        classes
    }

    /// Thread `tid`'s JDWP suspend count (`ThreadReference.SuspendCount`):
    /// the VM-wide suspensions in force plus its own, never below zero.
    pub fn suspend_count(&self, tid: u64) -> u32 {
        let own = self.suspend_adjust.get(&tid).copied().unwrap_or(0);
        let count = (i64::from(self.suspend_all_count) + own).max(0);
        u32::try_from(count).unwrap_or(u32::MAX)
    }

    /// Is thread `tid` suspended by the debugger (its suspend count is not
    /// zero)? The one answer the interpreter's suspend point parks on and
    /// `ThreadReference.Status` reports. Wave 45 (lane L1): or by a C JVMTI
    /// agent, for good or for a stack read ([`Self::jvmti_suspended`],
    /// [`Self::jvmti_handshakes`]).
    pub fn is_thread_suspended(&self, tid: u64) -> bool {
        self.suspend_count(tid) > 0 || self.is_suspended_by_jvmti(tid)
    }

    /// Is thread `tid` suspended by a C JVMTI agent: its `SuspendThread`, or
    /// a stack read's handshake (wave 45)?
    fn is_suspended_by_jvmti(&self, tid: u64) -> bool {
        (!self.jvmti_suspended.is_empty() && self.jvmti_suspended.contains(&tid))
            || (!self.jvmti_handshakes.is_empty() && self.jvmti_handshakes.contains_key(&tid))
    }

    /// Is any thread suspended, or a VM-wide suspension in force? While one
    /// is, every method runs the interpreter's debugger hooks
    /// ([`DebuggerGates`]), so every interpreter thread reaches a suspend
    /// point. Wave 45: a C JVMTI agent's suspensions count.
    pub fn any_suspension(&self) -> bool {
        self.suspend_all_count > 0
            || self.suspend_adjust.values().any(|&own| own > 0)
            || !self.jvmti_suspended.is_empty()
            || !self.jvmti_handshakes.is_empty()
    }

    /// JVMTI `SuspendThread` of thread `tid` (interpreter round i1 wave 45,
    /// lane L1): `false` when the agent had suspended it already.
    pub(crate) fn jvmti_suspend(&mut self, tid: u64) -> bool {
        self.jvmti_suspended.insert(tid)
    }

    /// JVMTI `ResumeThread` of thread `tid` (wave 45): `false` when the agent
    /// had not suspended it. A thread that runs now takes a new frame
    /// generation, as after a JDWP resume.
    pub(crate) fn jvmti_resume(&mut self, tid: u64) -> bool {
        let removed = self.jvmti_suspended.remove(&tid);
        if removed && !self.is_thread_suspended(tid) {
            bump_frame_generation(&mut self.frame_generations, tid, 1);
        }
        removed
    }

    /// Has a C JVMTI agent suspended thread `tid` (`SuspendThread`, wave 45)?
    pub(crate) fn is_jvmti_suspended(&self, tid: u64) -> bool {
        self.jvmti_suspended.contains(&tid)
    }

    /// Forget a dead thread `tid`'s C JVMTI suspensions (wave 45): a thread
    /// that ends suspended would otherwise keep every method interpreted
    /// (`publish_debugger_gates`). Answers whether there was one.
    pub(crate) fn forget_jvmti_suspension(&mut self, tid: u64) -> bool {
        let agent = self.jvmti_suspended.remove(&tid);
        let handshake = self.jvmti_handshakes.remove(&tid).is_some();
        agent || handshake
    }

    /// Suspend thread `tid` for a C JVMTI stack read (wave 45).
    pub(crate) fn begin_handshake(&mut self, tid: u64) {
        *self.jvmti_handshakes.entry(tid).or_insert(0) += 1;
    }

    /// End a suspension [`Self::begin_handshake`] took (wave 45).
    pub(crate) fn end_handshake(&mut self, tid: u64) {
        let Some(count) = self.jvmti_handshakes.get_mut(&tid) else {
            return;
        };
        *count = count.saturating_sub(1);
        if *count == 0 {
            self.jvmti_handshakes.remove(&tid);
            if !self.is_thread_suspended(tid) {
                bump_frame_generation(&mut self.frame_generations, tid, 1);
            }
        }
    }

    /// [`Self::any_suspension`] without the suspensions of threads the last
    /// thread listing saw terminated (interpreter round i1 wave 29). A dead
    /// thread never reaches a suspend point, so its counted suspension (JDWP
    /// counts it, as HotSpot does: `ThreadReference.SuspendCount` answers 1)
    /// must not hold every live thread in the interpreter's suspend point
    /// with the JIT stood down: this is what arms [`DebuggerGates`].
    pub fn any_suspension_of_a_live_thread(&self) -> bool {
        if !self.any_suspension() {
            return false;
        }
        self.suspend_all_count > 0
            || self.suspend_adjust.iter().any(|(tid, &own)| {
                own > 0 && self.thread_statuses.get(tid) != Some(&commands::THREAD_STATUS_ZOMBIE)
            })
            // Wave 45: a C JVMTI agent suspends only live threads.
            || !self.jvmti_suspended.is_empty()
            || !self.jvmti_handshakes.is_empty()
    }

    /// Apply the suspend policy of one JDWP event set reported on thread
    /// `tid` (wave 22): `SUSPEND_ALL` suspends the VM once, `SUSPEND_EVENT_THREAD`
    /// the thread once, however many events the set carries, as HotSpot's
    /// back end does (`suspendWithInvokeEnabled` per composite). Answers
    /// whether anything was suspended. Until wave 22 every matching request
    /// counted its own suspension, so a breakpoint two requests watched, or
    /// a step landing on a breakpoint, left the thread suspended after the
    /// debugger resumed the one event set it saw.
    pub fn apply_event_set_policy(&mut self, policy: events::SuspendPolicy, tid: u64) -> bool {
        match policy {
            events::SuspendPolicy::EventThread => {
                self.suspend_thread(tid);
                self.event_park_pending.insert(tid);
                true
            }
            events::SuspendPolicy::All => {
                self.suspend_all();
                self.event_park_pending.insert(tid);
                true
            }
            events::SuspendPolicy::None => false,
        }
    }

    /// Suspend one thread (`ThreadReference.Suspend`, a
    /// `SUSPEND_EVENT_THREAD` event): its count goes up by one.
    pub fn suspend_thread(&mut self, tid: u64) {
        *self.suspend_adjust.entry(tid).or_insert(0) += 1;
    }

    /// A terminated thread's JDWP suspend count: its own `ThreadReference.
    /// Suspend`s only, never a VM-wide one, which covers live threads
    /// (interpreter round i1 wave 29; HotSpot 25 answers 1 after one
    /// `Suspend` of a dead thread, `L1/L1W29RawJdwpThreadAndFrameErrors`).
    pub fn own_suspend_count(&self, tid: u64) -> u32 {
        let own = self.suspend_adjust.get(&tid).copied().unwrap_or(0).max(0);
        u32::try_from(own).unwrap_or(u32::MAX)
    }

    /// `ThreadReference.Resume` of a terminated thread: one off its own
    /// suspensions ([`Self::own_suspend_count`]), nothing else.
    pub fn resume_own_suspension(&mut self, tid: u64) {
        if let Some(own) = self.suspend_adjust.get_mut(&tid) {
            if *own > 0 {
                *own -= 1;
            }
        }
        self.suspend_adjust.retain(|_, own| *own != 0);
    }

    /// Resume one thread (`ThreadReference.Resume`): its count goes down by
    /// one, if it is suspended at all. A thread suspended by the VM and on
    /// its own needs two resumes, as in HotSpot; one only lifts the VM-wide
    /// suspension FOR IT (the next `VirtualMachine.Suspend` stops it again).
    pub fn resume_thread(&mut self, tid: u64) {
        if self.suspend_count(tid) > 0 {
            *self.suspend_adjust.entry(tid).or_insert(0) -= 1;
            if self.suspend_count(tid) == 0 {
                bump_frame_generation(&mut self.frame_generations, tid, 1);
            }
        }
        self.suspend_adjust.retain(|_, own| *own != 0);
    }

    /// Move thread `tid`'s JDWP frame generation while it stays suspended
    /// (interpreter round i1 wave 44, lane L1): a `StackFrame.PopFrames`
    /// changed its frames, so no id of the listing before it may name a
    /// frame of the one after it (JDI: "All StackFrame objects for this
    /// thread are invalidated"). The listing then no longer counts as
    /// current ([`Self::has_current_frames`]), and the park republishes it.
    pub(crate) fn invalidate_frame_ids(&mut self, tid: u64) {
        bump_frame_generation(&mut self.frame_generations, tid, 1);
    }

    /// Thread `tid`'s JDWP frame generation (interpreter round i1 wave 28,
    /// lane L1; [`Self::frame_epoch`]): moves each time the thread is
    /// resumed, never while it stays suspended.
    pub fn frame_generation(&self, tid: u64) -> u32 {
        let own = self.frame_generations.get(&tid).copied().unwrap_or(0);
        self.frame_epoch.wrapping_add(own)
    }

    /// The wire frame id of the frame at `position` (0 = top) of thread
    /// `tid`'s listing in its current suspension: the generation in the high
    /// half, as HotSpot's back end mints it (`createFrameID`). A first
    /// suspension's ids are the bare positions.
    pub fn frame_id(&self, tid: u64, position: u64) -> u64 {
        (u64::from(self.frame_generation(tid)) << 32) | (position & 0xFFFF_FFFF)
    }

    /// Does thread `tid` have a published listing minted in its current
    /// suspension (wave 28)? A listing that outlived a resume — a thread that
    /// stayed blocked in one native region while it was resumed and
    /// suspended again, or a parked thread's listing across a debugger
    /// invocation — is not current: the paths that publish only when no
    /// listing stands republish it, so its ids move to the new generation
    /// as HotSpot's do. An empty listing has no ids and counts as current.
    pub(crate) fn has_current_frames(&self, tid: u64) -> bool {
        let generation = u64::from(self.frame_generation(tid));
        self.thread_frames.get(&tid).is_some_and(|frames| {
            frames
                .first()
                .is_none_or(|e| e.frame_id >> 32 == generation)
        })
    }

    /// Suspend every thread (`VirtualMachine.Suspend`, a `SUSPEND_ALL`
    /// event): every count, including the counts of threads not yet known,
    /// goes up by one.
    pub fn suspend_all(&mut self) {
        self.suspend_all_count = self.suspend_all_count.saturating_add(1);
    }

    /// `VirtualMachine.Resume`: every suspended thread's count goes down by
    /// one (a thread at zero stays at zero).
    pub fn resume_vm(&mut self) {
        if self.suspend_all_count > 0 {
            let all_before = i64::from(self.suspend_all_count);
            self.suspend_all_count -= 1;
            let all_after = all_before - 1;
            // Wave 28: every thread with no adjustment of its own runs now.
            if all_after == 0 {
                self.frame_epoch = self.frame_epoch.wrapping_add(1);
            }
            // A thread whose count was already zero must not go negative.
            for (&tid, own) in self.suspend_adjust.iter_mut() {
                if all_before + *own <= 0 {
                    *own += 1;
                } else if all_after + *own > 0 {
                    // Still suspended: its generation must not move with
                    // the epoch.
                    if all_after == 0 {
                        bump_frame_generation(&mut self.frame_generations, tid, u32::MAX);
                    }
                } else if all_after != 0 {
                    // Resumed (count 1 -> 0) while the epoch stands.
                    bump_frame_generation(&mut self.frame_generations, tid, 1);
                }
            }
        } else {
            for (&tid, own) in self.suspend_adjust.iter_mut() {
                if *own > 0 {
                    *own -= 1;
                    if *own == 0 {
                        bump_frame_generation(&mut self.frame_generations, tid, 1);
                    }
                }
            }
        }
        self.suspend_adjust.retain(|_, own| *own != 0);
    }

    /// Resume everything and forget every count (the debugger detached).
    /// Also ends the session (wave 9): an invocation that returns after this
    /// re-suspends nothing. Every thread runs, so every frame generation
    /// moves (wave 28).
    pub fn resume_all(&mut self) {
        self.suspend_all_count = 0;
        self.suspend_adjust.clear();
        self.frame_epoch = self.frame_epoch.wrapping_add(1);
        self.session = self.session.wrapping_add(1);
    }

    /// The implicit resume a debugger invocation on thread `tid` starts with
    /// (wave 9; JDWP `ClassType.InvokeMethod`): with `INVOKE_SINGLE_THREADED`
    /// one `ThreadReference.Resume` of `tid`, else one `VirtualMachine.Resume`
    /// — every suspended thread runs for the call, so an invoked method that
    /// needs a monitor or a hand-off from another suspended thread gets it. A
    /// thread suspended more than once stays suspended, as the JDWP spec says.
    pub(crate) fn release_for_invocation(&mut self, tid: u64, single_threaded: bool) {
        if single_threaded {
            self.resume_thread(tid);
        } else {
            self.resume_vm();
        }
    }

    /// The suspension a debugger invocation on thread `tid` ends with (wave 9):
    /// `tid` alone with `INVOKE_SINGLE_THREADED`, else every thread — "all
    /// threads in the target VM are suspended, regardless of their state
    /// before the invocation" (JDWP; HotSpot's `invoker_completeInvokeRequest`).
    pub(crate) fn resuspend_after_invocation(&mut self, tid: u64, single_threaded: bool) {
        if single_threaded {
            self.suspend_thread(tid);
        } else {
            self.suspend_all();
        }
    }

    /// Thread `tid` parks at an interpreter suspend point (`true`) or leaves
    /// it (`false`). Only `interpreter::park_for_debugger` calls this. A park
    /// inside a task an outer park runs (a stop inside a debugger invocation)
    /// nests; leaving it uncovers the outer level, which is busy with that
    /// task.
    pub(crate) fn note_parked(&mut self, tid: u64, parked: bool) {
        if parked {
            self.parked_threads.entry(tid).or_default().push(false);
            // Wave 44: whether an event suspended the thread for this park.
            let by_event = self.event_park_pending.remove(&tid);
            self.event_parks.entry(tid).or_default().push(by_event);
        } else {
            if let Some(levels) = self.parked_threads.get_mut(&tid) {
                levels.pop();
                if levels.is_empty() {
                    self.parked_threads.remove(&tid);
                }
            }
            if let Some(levels) = self.event_parks.get_mut(&tid) {
                levels.pop();
                if levels.is_empty() {
                    self.event_parks.remove(&tid);
                }
            }
            // Nothing can be queued for a level that is leaving (the park
            // loop leaves only with no task queued, under this lock), but a
            // task left behind would never be answered: drop it, which
            // closes its reply channel and fails the waiting server call.
            self.parked_tasks.remove(&tid);
        }
    }

    /// Did an event suspend thread `tid` for its innermost park (wave 44;
    /// [`Self::event_parks`])? A debugger invocation runs only there.
    pub(crate) fn parked_by_event(&self, tid: u64) -> bool {
        self.event_parks
            .get(&tid)
            .and_then(|levels| levels.last())
            .copied()
            .unwrap_or(false)
    }

    /// Does thread `tid` hold a suspension of its own (a
    /// `ThreadReference.Suspend` or an event that suspended it), not only
    /// the VM-wide count every thread shares (wave 44)?
    pub(crate) fn has_own_suspension(&self, tid: u64) -> bool {
        self.suspend_adjust.get(&tid).is_some_and(|&own| own > 0)
    }

    /// Is thread `tid` parked at an interpreter suspend point (at any level,
    /// busy or not)? Interpreter round i1 wave 46, lane L1: no longer
    /// `#[cfg(test)]`; the C JVMTI table asks it of another thread
    /// (`jvmti::native_env::thread_stopped`, `stopped_rows`).
    pub fn is_parked(&self, tid: u64) -> bool {
        self.parked_threads.contains_key(&tid)
    }

    /// Can parked thread `tid` take work now: its innermost park level is
    /// idle and nothing is queued for it?
    fn can_take_work(&self, tid: u64) -> bool {
        self.parked_threads
            .get(&tid)
            .and_then(|levels| levels.last())
            == Some(&false)
            && !self.parked_tasks.contains_key(&tid)
    }

    /// The work queued for parked thread `tid`, if any (its park loop runs
    /// it). Its innermost park level is busy until [`Self::note_task_done`].
    pub(crate) fn take_parked_task(&mut self, tid: u64) -> Option<ParkedTask> {
        let task = self.parked_tasks.remove(&tid)?;
        if let Some(busy) = self
            .parked_threads
            .get_mut(&tid)
            .and_then(|levels| levels.last_mut())
        {
            *busy = true;
        }
        Some(task)
    }

    /// Is a `ThreadReference.Stop` waiting for thread `tid` (wave 24,
    /// [`take_pending_stop`])?
    #[cfg_attr(not(feature = "experimental-debug"), allow(dead_code))]
    pub(crate) fn has_pending_stop(&self, tid: u64) -> bool {
        !self.pending_stops.is_empty() && self.pending_stops.contains_key(&tid)
    }

    /// The heap service's thread id, while it runs ([`start_heap_service`]).
    pub(crate) fn heap_service_thread(&self) -> Option<u64> {
        self.heap_service.as_ref().map(|(_, tid)| *tid)
    }

    /// The task [`Self::take_parked_task`] handed thread `tid`'s innermost
    /// park level is done.
    pub(crate) fn note_task_done(&mut self, tid: u64) {
        if let Some(busy) = self
            .parked_threads
            .get_mut(&tid)
            .and_then(|levels| levels.last_mut())
        {
            *busy = false;
        }
    }

    /// Allocate the next outgoing packet ID.
    pub fn next_id(&mut self) -> u32 {
        let id = self.next_packet_id;
        self.next_packet_id += 1;
        id
    }

    /// Forget every class of `gone`, which the VM unloaded (interpreter round
    /// i1 wave 25, [`class_unload_packets`]): its metadata, status, and
    /// class-prepare bookkeeping, so the commands that name it answer
    /// `INVALID_CLASS` and `ClassesBySignature` / `AllClassesWithGeneric` no
    /// longer list it. Answers `(id, JNI signature)` of each one the session
    /// knew, in id order.
    pub(crate) fn forget_classes(&mut self, gone: &HashSet<u64>) -> Vec<(u64, String)> {
        let mut forgotten: Vec<(u64, String)> = gone
            .iter()
            .filter_map(|id| Some((*id, self.class_signatures.remove(id)?)))
            .collect();
        forgotten.sort_unstable_by_key(|&(id, _)| id);
        for id in gone {
            self.class_type_tags.remove(id);
            self.class_source_files.remove(id);
            self.class_fields.remove(id);
            self.class_methods.remove(id);
            self.class_statuses.remove(id);
            self.class_superclass.remove(id);
            self.class_details.remove(id);
            self.class_prepare_reported.remove(id);
        }
        self.class_prepare_pending.retain(|((id, _, _), _)| !gone.contains(id));
        self.method_line_tables.retain(|(id, _), _| !gone.contains(id));
        self.method_variables.retain(|(id, _), _| !gone.contains(id));
        self.method_bytecodes.retain(|(id, _), _| !gone.contains(id));
        forgotten
    }

    // -- Exception events (wave 10) ------------------------------------------
    //
    // Wave 10: `fire_field_access_event` / `fire_field_modification_event`
    // are gone. Only unit tests called them, and the packets they built
    // stopped after the field id: a JDWP field event carries the accessed
    // object (and, for a modification, the new value) after it, so a
    // debugger would have mis-parsed every event. The interpreter's hooks
    // build the events now (`interpreter::deliver_field_watch_if_armed`).
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Start the JDWP debug server on the given port.
///
/// This spawns a background listener thread and returns a [`DebugState`]
/// immediately.  The caller should periodically call
/// [`handle_debug_commands`] to process incoming JDWP commands.
pub fn start_debug_server(
    port: u16,
) -> std::io::Result<(
    DebugState,
    std::sync::mpsc::Receiver<std::io::Result<JdwpConnection>>,
)> {
    let rx = transport::JdwpTransport::start_listener(port)?;
    Ok((DebugState::new(), rx))
}

/// Process one pending JDWP command from `conn`, dispatching it through
/// the command handler and writing the reply back.
///
/// Returns `Ok(true)` if a command was processed, `Ok(false)` if the
/// connection was cleanly closed, or `Err` on I/O failure.
pub fn handle_one_command(
    conn: &mut JdwpConnection,
    state: &mut DebugState,
) -> std::io::Result<bool> {
    let packet = match conn.read_packet() {
        Ok(p) => p,
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(false),
        Err(e) => return Err(e),
    };

    match packet {
        JdwpPacket::Command {
            id,
            command_set,
            command,
            data,
            ..
        } => {
            let result = commands::dispatch(command_set, command, &data, state);
            let reply = JdwpPacket::Reply {
                id,
                error_code: result.error_code,
                data: result.data,
            };
            conn.write_packet(&reply)?;
        }
        JdwpPacket::Reply { .. } => {
            // We don't expect replies from the debugger; ignore.
        }
    }

    Ok(!state.disposed)
}

/// Process JDWP commands in a loop until the session ends.
pub fn handle_debug_commands(
    conn: &mut JdwpConnection,
    state: &mut DebugState,
) -> std::io::Result<()> {
    loop {
        match handle_one_command(conn, state)? {
            true => {}
            false => return Ok(()),
        }
    }
}

// ---------------------------------------------------------------------------
// Live VM integration — runs in a background thread
// ---------------------------------------------------------------------------

/// How long the server's read of the debugger's socket waits for a command
/// before it goes round its loop again (the events the interpreter queued,
/// invocation replies, class prepares and unloads): the most an event waits
/// while the debugger is silent (interpreter round i1 wave 25, L1d).
const SERVER_READ_WAIT: std::time::Duration = std::time::Duration::from_millis(2);

/// How long one write to a debugger that reads nothing may block before the
/// session is given up (the stall limit `protocol::write_packet_nonblocking`
/// applies to a non-blocking socket).
const SERVER_WRITE_WAIT: std::time::Duration = std::time::Duration::from_secs(60);

/// Run the JDWP server loop.  Blocks forever: listens for a debugger
/// connection, processes commands, and loops back to accept a new
/// connection after the previous one closes.
pub fn run_jdwp_server(shared: &crate::vm::SharedVm, port: u16) {
    // This thread is not a registered mutator: it never runs Java and touches
    // the heap only inside `GcBarrier::run_if_no_stw_requested` (wave 8: the
    // invocations and the heap commands run on a parked thread,
    // `run_on_parked_thread`). The interpreter's suspend point must never
    // park it all the same (`debugger_hooks_suppressed`).
    JDWP_SERVER_THREAD.with(|f| f.set(true));
    let listener = match std::net::TcpListener::bind(("127.0.0.1", port)) {
        Ok(l) => l,
        Err(e) => {
            tracing::error!("JDWP: failed to bind port {port}: {e}");
            return;
        }
    };
    tracing::info!("JDWP: listening on 127.0.0.1:{port}");

    for stream in listener.incoming() {
        let mut stream = match stream {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!("JDWP: accept failed: {e}");
                continue;
            }
        };

        // JDWP handshake: 14-byte string "JDWP-Handshake"
        let mut handshake_buf = [0u8; 14];
        if std::io::Read::read_exact(&mut stream, &mut handshake_buf).is_err() {
            continue;
        }
        if &handshake_buf != b"JDWP-Handshake" {
            continue;
        }
        if std::io::Write::write_all(&mut stream, b"JDWP-Handshake").is_err() {
            continue;
        }

        tracing::info!("JDWP: debugger attached");

        // Create the debug event channel and install in SharedVm
        let (tx, rx) = std::sync::mpsc::channel::<DebugEvent>();
        {
            if let Ok(mut guard) = shared.debug.debug_event_tx.lock() {
                *guard = Some(tx);
            }
        }
        shared.debug.debugger_gates.set_session_attached(true);
        // Wave 46 (lane L1): blocking stand-ins run their bytecode.
        arm_blocking_standins(shared, true);

        // T6.5 — install the live-VM bridge so `ClassType.InvokeMethod`,
        // `ObjectReference.InvokeMethod` and `ArrayType.NewInstance` can
        // forward to the running VM.  The bridge is removed on disconnect.
        let self_arc = shared.self_arc.read().as_ref().and_then(|w| w.upgrade());
        if let Some(shared_arc) = self_arc {
            // Wave 10: heap work (`CreateString`, `ArrayType.NewInstance`,
            // the heap commands) no longer needs a thread parked at a
            // suspend point.
            if !start_heap_service(&shared_arc) {
                tracing::warn!("JDWP: the heap service thread could not be started");
            }
            let bridge: Arc<dyn DebuggerVmBridge> = Arc::new(SharedVmBridge::new(shared_arc));
            shared.debug.debug_state.lock().vm_bridge = Some(bridge);
        } else {
            tracing::warn!(
                "JDWP: self_arc unavailable — debugger-initiated invocations \
                 will return VM_DEAD until the VM is fully wired up"
            );
        }

        // Wave 44 (lane L1): the array classes HotSpot makes at start-up.
        ensure_startup_array_classes(shared);
        // Populate class metadata from current VM state (the classes defined
        // later are picked up by every poll below, `send_class_prepare_events`)
        let _ = populate_class_metadata(shared);

        // Populate thread metadata
        populate_thread_metadata(shared);

        // No `VMStart` event (interpreter round i1 wave 23): HotSpot's back
        // end sends one only while the VM initialises (`suspend=y`, or a
        // connection made before the VM started), never to a debugger that
        // attaches to a running VM, which is the only way this server is
        // reached (`--jdwp-suspend` is not implemented). It sent one to every
        // debugger, naming thread 0, which no thread has.

        // Set stream to non-blocking so we can interleave command processing
        // with event draining. Wave 24: so it is read through an assembler
        // and written whole (`protocol::write_packet_nonblocking`).
        //
        // Wave 25 (L1d): reads block for at most `SERVER_READ_WAIT` instead
        // of a non-blocking socket polled every 5 ms, so a command is read the
        // moment it arrives. The 5 ms sleep delayed every command a JDI
        // session sends by up to 5 ms, and opened a window in which the
        // program ran on ahead of the debugger: `L1W24JdiSurface` sets its
        // `attached` flag and then sends `VirtualMachine.Resume`; when the
        // program reached its first `SUSPEND_ALL` breakpoint before the
        // server read the (late) `Resume`, that `Resume` released the
        // breakpoint's suspension and the program ran past the next
        // breakpoint the debugger was about to set. A socket that cannot
        // take a read timeout keeps the old non-blocking poll.
        let blocking_reads = stream.set_nonblocking(false).is_ok()
            && stream.set_read_timeout(Some(SERVER_READ_WAIT)).is_ok()
            && stream.set_write_timeout(Some(SERVER_WRITE_WAIT)).is_ok();
        if !blocking_reads {
            let _ = stream.set_nonblocking(true);
        }
        let mut incoming = protocol::PacketAssembler::new();

        // Invocations in flight (wave 9): each runs on a waiter thread, which
        // sends `(packet id, reply)` here when the invoked method returns.
        let (invoke_tx, invoke_rx) = std::sync::mpsc::channel::<(u32, commands::CommandResult)>();
        // The VM's class-unload count at the last poll (wave 25,
        // `send_class_unload_events`).
        let mut unloads_seen: Option<u64> = None;

        // Process commands until the connection closes
        loop {
            // While the debugger holds events (wave 23, `HoldEvents`), none
            // goes out: they wait on the channel, and the classes defined
            // meanwhile are picked up by the first poll after the release.
            let held = shared.debug.debug_state.lock().events_held;

            // --- Classes defined since the last poll (wave 7): metadata, and
            // a `ClassPrepare` event for each one a request asks about ---
            if !held && !send_class_prepare_events(&mut stream, shared) {
                break;
            }
            // --- Classes unloaded since the last poll (wave 25) ---
            if !held && !send_class_unload_events(&mut stream, shared, &mut unloads_seen) {
                break;
            }

            // --- Invocation replies, then the interpreter's events, then the
            // replies go out. An invoking thread queues every event it raises
            // (a stop inside the invoked method) before it answers, so taking
            // the answers first writes such an event before the reply to the
            // invocation, as JDI requires. ---
            let answered: Vec<(u32, commands::CommandResult)> = invoke_rx.try_iter().collect();
            if !held && !drain_interpreter_events(&mut stream, shared, &rx) {
                break;
            }
            if !answered.is_empty() {
                {
                    let mut ds = shared.debug.debug_state.lock();
                    publish_debugger_gates(shared, &ds);
                    release_disposed_objects(shared, &mut ds);
                }
                shared.mem.heap.flush_thread_satb();
                let written = answered.into_iter().all(|(id, result)| {
                    let reply = JdwpPacket::Reply {
                        id,
                        error_code: result.error_code,
                        data: result.data,
                    };
                    protocol::write_packet_nonblocking(&mut stream, &reply).is_ok()
                });
                if !written {
                    break;
                }
            }

            // --- Process one incoming command (non-blocking) ---
            // Assembled from whatever the socket has (wave 24): a packet that
            // arrives in pieces is kept, not half-read and lost.
            let packet = match incoming.poll(&mut stream) {
                Ok(Some(p)) => p,
                Ok(None) => {
                    if !blocking_reads {
                        std::thread::sleep(std::time::Duration::from_millis(5));
                    }
                    continue;
                }
                Err(_) => break,
            };

            match packet {
                JdwpPacket::Command {
                    id,
                    command_set,
                    command,
                    mut data,
                    ..
                } => {
                    // The thread and class ids the command names, from their
                    // wire form to the VM's (wave 25: id 0 is JDWP's null;
                    // `ids::thread_from_wire`, `ids::class_from_wire`).
                    ids_from_wire(command_set, command, &mut data);
                    // A command that runs Java or allocates through the VM
                    // bridge (`ClassType.InvokeMethod`,
                    // `ObjectReference.InvokeMethod`, `ArrayType.NewInstance`)
                    // is prepared under the debug-state lock and RUN WITHOUT
                    // IT (wave 7). It used to run under it: the bridge takes
                    // that (non-reentrant) lock itself, so each of the three
                    // commands deadlocked the server; and Java run under it
                    // would stall every interpreter thread's suspend point,
                    // and with it any collection the Java triggered.
                    //
                    // Wave 8: the commands that read or write the heap
                    // (`inspect`) and the bridge commands run on a thread
                    // parked at an interpreter suspend point, never here.
                    //
                    // Wave 9: an invocation is answered when the invoked
                    // method returns, from a waiter thread; the server keeps
                    // serving commands and events meanwhile. It used to wait
                    // here, so the debugger could not resume a stop inside
                    // the invoked method, nor a thread the method waited for.
                    if refreshes_thread_names(command_set, command) {
                        populate_thread_metadata(shared);
                    }
                    if reads_class_status(command_set, command) {
                        refresh_class_statuses_for(shared, command_set, command, &data);
                    }
                    if reads_thread_frames(command_set, command) {
                        publish_blocked_frames_for(shared, &data);
                    }
                    let result = if let Some(refused) =
                        unknown_id_refusal(shared, command_set, &data)
                    {
                        refused
                    } else if let Some(served) =
                        serve_with_the_vm(shared, command_set, command, &data)
                    {
                        served
                    } else if inspect::served_with_the_heap(command_set, command) {
                        inspect::run_heap_command(shared, command_set, command, &data)
                    } else {
                        let prepared = {
                            let ds = shared.debug.debug_state.lock();
                            commands::prepare_bridge_command(command_set, command, &data, &ds)
                        };
                        match prepared {
                            Some(Ok(call)) if call.runs_java() => {
                                let tx = invoke_tx.clone();
                                let waiter = std::thread::Builder::new()
                                    .name("cratonvm-jdwp-invoke".to_string())
                                    .spawn(move || {
                                        let _ = tx.send((id, call.run()));
                                    });
                                match waiter {
                                    Ok(_) => continue,
                                    Err(_) => {
                                        commands::CommandResult::error(commands::ERR_INTERNAL)
                                    }
                                }
                            }
                            Some(Ok(call)) => call.run(),
                            Some(Err(refused)) => refused,
                            None => {
                                let mut ds = shared.debug.debug_state.lock();
                                commands::dispatch(command_set, command, &data, &mut ds)
                            }
                        }
                    };

                    // Republish the interpreter's gate and the per-method
                    // summary (`publish_debugger_gates`), and free the
                    // handles of object ids the command disposed of.
                    {
                        let mut ds = shared.debug.debug_state.lock();
                        publish_debugger_gates(shared, &ds);
                        release_disposed_objects(shared, &mut ds);
                    }
                    // A barrier this thread ran (a weak-handle keep-alive, a
                    // store) buffered its SATB entry thread-locally, and this
                    // thread never reaches a safepoint that would flush it.
                    shared.mem.heap.flush_thread_satb();

                    let reply = JdwpPacket::Reply {
                        id,
                        error_code: result.error_code,
                        data: result.data,
                    };
                    if protocol::write_packet_nonblocking(&mut stream, &reply).is_err() {
                        break;
                    }

                    // Check if session was disposed, or the debugger asked
                    // the VM to exit
                    let (disposed, exit_code) = {
                        let ds = shared.debug.debug_state.lock();
                        (ds.disposed, ds.exit_code)
                    };
                    if let Some(code) = exit_code {
                        exit_for_debugger(shared, code);
                    }
                    if disposed {
                        break;
                    }
                }
                JdwpPacket::Reply { .. } => {}
            }
        }

        tracing::info!("JDWP: debugger disconnected");

        // Tear down the event channel
        {
            if let Ok(mut guard) = shared.debug.debug_event_tx.lock() {
                *guard = None;
            }
        }
        shared.debug.debugger_gates.set_session_attached(false);
        arm_blocking_standins(shared, false);
        stop_heap_service(shared);

        // Clear all breakpoints/step requests and resume all threads
        {
            let mut ds = shared.debug.debug_state.lock();
            ds.events.clear_all();
            ds.resume_all();
            ds.disposed = false;
            ds.events_held = false;
            ds.class_prepare_reported.clear();
            ds.class_prepare_pending.clear();
            // A stop nobody delivered dies with the session (wave 24); its
            // id's pin goes with `release_all` below.
            ds.pending_stops.clear();
            // So does a forced return not yet taken (wave 43).
            ds.early_returns.clear_pending();
            ds.vm_death_sent = false;
            // T6.5 — tear down the VM bridge so subsequent connect cycles
            // install a fresh one (avoids stale Arc<SharedVm> references).
            ds.vm_bridge = None;
            // The detached debugger's object ids die with the session; their
            // handles stop keeping the objects alive. So do the frame
            // snapshots naming them (a thread still blocked in native code
            // withdraws its own when it leaves; wave 12: whatever the
            // session).
            ds.thread_frames.clear();
            ds.frame_locals.clear();
            ds.opaque_frames.clear();
            // Wave 41: writes a blocked thread has not applied yet die with
            // the session too; their ids' holds go with `release_all`.
            ds.deferred_frame_writes.clear();
            shared.debug.debugger_gates.set_frame_writes_pending(false);
            ds.objects.release_all();
            release_disposed_objects(shared, &mut ds);
            // (Wave 11: the "exception already reported" handles live on
            // their threads, `JvmThread::reported_exception`, freed by the
            // thread's next catch or reported throw, or its exit, wave 12.)
            publish_debugger_gates(shared, &ds);
        }
    }
}

/// Send every event the interpreter queued since the last drain (breakpoints,
/// steps, exceptions, field watches, thread events). `false` when the
/// connection failed. Until wave 9 a failed write only ended this drain, and
/// the loop went on to read from the dead connection; and a thread event
/// carried a location trailer JDWP does not define for it.
fn drain_interpreter_events(
    stream: &mut std::net::TcpStream,
    shared: &crate::vm::SharedVm,
    rx: &std::sync::mpsc::Receiver<DebugEvent>,
) -> bool {
    // The event set being gathered (wave 22): its events and the strongest
    // of their policies. A producer sends a set contiguously, so it ends at
    // the first event with `set_follows` false; a set cut short (the channel
    // emptied mid-set, which a producer holding the mutex cannot cause) is
    // sent as it stands.
    let mut set: Vec<events::Event> = Vec::new();
    let mut set_policy = events::SuspendPolicy::None;
    // Wave 38 (lane L1): the byte the set reports when it is applied as
    // `EVENT_THREAD`: that of its first event so applied, which is the byte
    // its request sent when JDWP does not define it
    // (`EventManager::wire_suspend_policy`), as HotSpot reports it.
    let mut set_wire: Option<u8> = None;
    while let Ok(evt) = rx.try_recv() {
        // A thread event is `requestID, thread` and nothing after; the
        // location trailer belongs to the location events only (wave 9: every
        // event carried one). An exception or field event continues after
        // its location with the data its producer encoded (wave 10).
        let extra = match evt.kind {
            // `VM_DEATH` is `requestID` alone (wave 24).
            events::EventKind::ThreadStart
            | events::EventKind::ThreadDeath
            | events::EventKind::VMDeath => Vec::new(),
            // No location: its producer encoded the class (wave 10,
            // `class_prepared_on_thread`).
            events::EventKind::ClassPrepare => evt.extra.clone(),
            _ => {
                let mut bytes = build_location_extra(evt.class_id, evt.method_id, evt.offset);
                // The class's own type tag (wave 15): an interface method's
                // location was sent as a CLASS one.
                bytes[0] = shared
                    .debug
                    .debug_state
                    .lock()
                    .location_type_tag(evt.class_id);
                bytes.extend_from_slice(&evt.extra);
                bytes
            }
        };
        set.push(events::Event {
            request_id: evt.request_id,
            kind: evt.kind,
            thread_id: evt.thread_id,
            extra,
        });
        set_policy = strongest_suspend_policy([set_policy, evt.suspend_policy]);
        if set_wire.is_none() && evt.suspend_policy == events::SuspendPolicy::EventThread {
            set_wire = Some(
                shared
                    .debug
                    .debug_state
                    .lock()
                    .events
                    .wire_suspend_policy(evt.request_id)
                    .unwrap_or(events::SuspendPolicy::EventThread as u8),
            );
        }
        if evt.set_follows {
            continue;
        }
        if !send_composite(stream, shared, event_set_policy_byte(set_policy, set_wire), &set) {
            return false;
        }
        note_vm_death_sent(shared, &set);
        set.clear();
        set_policy = events::SuspendPolicy::None;
        set_wire = None;
    }
    if set.is_empty() {
        return true;
    }
    let sent = send_composite(stream, shared, event_set_policy_byte(set_policy, set_wire), &set);
    if sent {
        note_vm_death_sent(shared, &set);
    }
    sent
}

/// After `set` went out: if it was the `VM_DEATH` set, tell
/// [`report_vm_death`], which waits for it (wave 24).
fn note_vm_death_sent(shared: &crate::vm::SharedVm, set: &[events::Event]) {
    if set.iter().any(|e| e.kind == events::EventKind::VMDeath) {
        shared.debug.debug_state.lock().vm_death_sent = true;
    }
}

/// The suspend-policy byte an event set applied as `policy` reports
/// (interpreter round i1 wave 38, lane L1): `wire`, the byte of its first
/// event applied as `EVENT_THREAD` (see `drain_interpreter_events`), when the
/// set is applied as `EVENT_THREAD`; the policy's own byte otherwise. HotSpot
/// 25 echoes a request's undefined byte (3, 7, 255) in the event set it
/// reports and suspends the event thread for it
/// (`tools/probes/interp/L1/L1W37RawJdwpUnknownSuspendPolicy.java`); a set
/// mixing such a request with an `ALL` one reports `ALL` here.
fn event_set_policy_byte(policy: events::SuspendPolicy, wire: Option<u8>) -> u8 {
    match (policy, wire) {
        (events::SuspendPolicy::EventThread, Some(wire)) => wire,
        (policy, _) => policy as u8,
    }
}

/// Write one composite event packet of `set` with the suspend-policy byte
/// `policy` ([`event_set_policy_byte`]). `false` when the connection failed.
fn send_composite(
    stream: &mut std::net::TcpStream,
    shared: &crate::vm::SharedVm,
    policy: u8,
    set: &[events::Event],
) -> bool {
    let pkt_id = shared.debug.debug_state.lock().next_id();
    let mut pkt = events::compose_event_packet_with_policy_byte(policy, set);
    if let JdwpPacket::Command { ref mut id, .. } = pkt {
        *id = pkt_id;
    }
    protocol::write_packet_nonblocking(stream, &pkt).is_ok()
}

/// Record metadata for the classes defined since the last poll and send a
/// `ClassPrepare` event for each one a `ClassPrepare` request reports
/// (`EventManager::match_class_prepare`: `ClassMatch`, `ClassExclude`,
/// `Count`). `false` when the connection failed.
///
/// The event is sent from the server, after the fact, so the defining thread
/// cannot be the one a `SUSPEND_EVENT_THREAD` request stops: such an event is
/// reported with policy `NONE` (nothing was suspended, and the debugger must
/// not resume anything for it). A `SUSPEND_ALL` request suspends the VM
/// before the event goes out, and every interpreter thread parks at its next
/// gated point. The event names the lowest known thread id.
///
/// Wave 10: this is the fallback. A class prepared by a VM thread is reported
/// by that thread ([`class_prepared_on_thread`]), which it names and stops as
/// the request says, before any of the class's code runs; so the poll leaves
/// a class that is defined but not yet prepared pending, and reports it only
/// once it is prepared by a path that did not report it, or after
/// [`CLASS_PREPARE_GRACE`]. A class is considered once, by whichever came
/// first (`DebugState::class_prepare_reported`).
fn send_class_prepare_events(
    stream: &mut std::net::TcpStream,
    shared: &crate::vm::SharedVm,
) -> bool {
    let added = populate_class_metadata(shared);
    let ready = classes_ready_for_prepare_events(shared, added, std::time::Instant::now());
    if ready.is_empty() {
        return true;
    }
    // `ClassOnly` (wave 24): each class's supertypes among the ids the
    // requests name, from the class manager, before the debug-state lock.
    let (filter_classes, wants_smap) = {
        let ds = shared.debug.debug_state.lock();
        (
            ds.events.filter_class_ids(events::EventKind::ClassPrepare),
            ds.events.has_source_name_filters(),
        )
    };
    // Wave 43: each class's SMAP file names, for `SourceNameMatch`, from the
    // class manager before the debug-state lock.
    let smap: Vec<Vec<String>> = if wants_smap {
        let cm = shared.classes.class_manager.read();
        ready
            .iter()
            .map(|(class_id, _, _)| {
                u32::try_from(*class_id)
                    .ok()
                    .and_then(|raw| cm.class_store.get(crate::classloading::ClassId::new(raw)))
                    .map_or_else(Vec::new, |class| smap_source_names(&cm, class))
            })
            .collect()
    } else {
        Vec::new()
    };
    let supers: Vec<Vec<u64>> = if filter_classes.is_empty() {
        Vec::new()
    } else {
        let cm = shared.classes.class_manager.read();
        ready
            .iter()
            .map(|(class_id, _, _)| {
                u32::try_from(*class_id).map_or_else(
                    |_| Vec::new(),
                    |raw| {
                        supertypes_among(
                            &cm,
                            crate::classloading::ClassId::new(raw),
                            &filter_classes,
                        )
                    },
                )
            })
            .collect()
    };
    let mut packets = Vec::new();
    {
        let mut ds = shared.debug.debug_state.lock();
        let thread_id = ds.thread_names.keys().copied().min().unwrap_or(0);
        let mut suspended = false;
        for (index, (class_id, sig, tag)) in ready.iter().enumerate() {
            if !ds.class_prepare_reported.insert(*class_id) {
                continue;
            }
            // An array class is never prepared (JVMTI posts no
            // `ClassPrepare` for one, and HotSpot's back end reports none);
            // this poll reported each one after the grace period (wave 24).
            if *tag == 3 {
                continue;
            }
            let name = sig
                .strip_prefix('L')
                .and_then(|s| s.strip_suffix(';'))
                .unwrap_or(sig.as_str());
            let source = ds.class_source_files.get(class_id).cloned();
            let mut source_names: Vec<&str> = source.as_deref().into_iter().collect();
            if let Some(extra) = smap.get(index) {
                source_names.extend(extra.iter().map(String::as_str));
            }
            let class_supers = supers.get(index).map_or(&[][..], Vec::as_slice);
            let class_is = |id: u64| class_supers.contains(&id);
            let hits = ds
                .events
                .match_class_prepare(name, &source_names, &class_is);
            if hits.is_empty() {
                continue;
            }
            // One event set per class, the VM suspended once when a request
            // asks for it (wave 22); a thread policy is reported as NONE, as
            // before, since no thread was stopped.
            let reported = match strongest_suspend_policy(hits.iter().map(|&(_, p)| p)) {
                events::SuspendPolicy::All => {
                    ds.suspend_all();
                    suspended = true;
                    events::SuspendPolicy::All
                }
                _ => events::SuspendPolicy::None,
            };
            let set: Vec<events::Event> = hits
                .into_iter()
                .map(|(request_id, _)| {
                    let mut extra = protocol::PayloadWriter::new();
                    extra.put_u8(*tag);
                    extra.put_u64_be(ids::class_to_wire(*class_id));
                    extra.put_string(sig);
                    extra.put_u32_be(3); // status: VERIFIED | PREPARED
                    events::Event {
                        request_id,
                        kind: events::EventKind::ClassPrepare,
                        thread_id,
                        extra: extra.into_bytes(),
                    }
                })
                .collect();
            let mut pkt = events::compose_event_packet(reported, &set);
            let pkt_id = ds.next_id();
            if let JdwpPacket::Command { ref mut id, .. } = pkt {
                *id = pkt_id;
            }
            packets.push(pkt);
        }
        if suspended {
            publish_debugger_gates(shared, &ds);
        }
    }
    packets
        .iter()
        .all(|pkt| protocol::write_packet_nonblocking(stream, pkt).is_ok())
}

/// Classes unloaded since the server's last poll (interpreter round i1 wave
/// 25): `ClassUnload` events, and the classes forgotten
/// ([`class_unload_packets`]). `seen` is the VM's unload count
/// (`diagnostic_counters.classes_unloaded`, which the unload transaction,
/// `memory::gc::unload_dead_class_metadata`, bumps after it removed its
/// classes from the store) at the last poll; the classes are swept only when
/// it moved, so an idle poll costs one load. `None` at the start of a
/// session: the first sweep forgets what an earlier session's tables still
/// hold about classes unloaded since, and reports nothing (this debugger
/// never knew them). `false` when the connection failed.
fn send_class_unload_events(
    stream: &mut std::net::TcpStream,
    shared: &crate::vm::SharedVm,
    seen: &mut Option<u64>,
) -> bool {
    let now = shared
        .debug
        .diagnostic_counters
        .classes_unloaded
        .load(std::sync::atomic::Ordering::Acquire);
    if *seen == Some(now) {
        return true;
    }
    let report = seen.is_some();
    *seen = Some(now);
    class_unload_packets(shared, report)
        .iter()
        .all(|pkt| protocol::write_packet_nonblocking(stream, pkt).is_ok())
}

/// Forget the classes the session knows that are no longer in the class
/// store — the VM unloaded them (class ids are never reused: the store keeps
/// a tombstone) — and, when `report`, one `ClassUnload` event set per class
/// for the requests that match it (`EventManager::match_class_unload`; JDI
/// holds one of its own, which is how it drops the class's `ReferenceType`),
/// as HotSpot's back end reports a class unload after the collection
/// (`classTrack.c`). Each set carries its requests' strongest suspend
/// policy; `SUSPEND_ALL` suspends the VM before the set goes out, as the
/// class-prepare poll does; a thread policy is reported as `NONE` (an unload
/// has no thread). Until wave 25 an unloaded class stayed listed and its
/// `ReferenceType` commands answered from the stale tables.
fn class_unload_packets(shared: &crate::vm::SharedVm, report: bool) -> Vec<JdwpPacket> {
    let cm = shared.classes.class_manager.read();
    let mut ds = shared.debug.debug_state.lock();
    let gone: HashSet<u64> = ds
        .class_signatures
        .keys()
        .copied()
        .filter(|&id| {
            u32::try_from(id).map_or(true, |raw| {
                cm.class_store
                    .get(crate::classloading::ClassId::new(raw))
                    .is_none()
            })
        })
        .collect();
    drop(cm);
    if gone.is_empty() {
        return Vec::new();
    }
    let forgotten = ds.forget_classes(&gone);
    if !report {
        return Vec::new();
    }
    let mut packets = Vec::new();
    let mut suspended = false;
    for (_, sig) in &forgotten {
        let name = sig
            .strip_prefix('L')
            .and_then(|s| s.strip_suffix(';'))
            .unwrap_or(sig.as_str());
        let hits = ds.events.match_class_unload(name);
        if hits.is_empty() {
            continue;
        }
        let reported = match strongest_suspend_policy(hits.iter().map(|&(_, p)| p)) {
            events::SuspendPolicy::All => {
                ds.suspend_all();
                suspended = true;
                events::SuspendPolicy::All
            }
            _ => events::SuspendPolicy::None,
        };
        let set: Vec<events::Event> = hits
            .into_iter()
            .map(|(request_id, _)| {
                let mut extra = protocol::PayloadWriter::new();
                extra.put_string(sig);
                events::Event {
                    request_id,
                    kind: events::EventKind::ClassUnload,
                    thread_id: 0,
                    extra: extra.into_bytes(),
                }
            })
            .collect();
        let mut pkt = events::compose_event_packet(reported, &set);
        let pkt_id = ds.next_id();
        if let JdwpPacket::Command { ref mut id, .. } = pkt {
            *id = pkt_id;
        }
        packets.push(pkt);
    }
    if suspended {
        publish_debugger_gates(shared, &ds);
    }
    packets
}

/// How long the server's poll leaves a defined, unprepared class to the
/// thread that will prepare it before reporting it after the fact
/// ([`send_class_prepare_events`]).
const CLASS_PREPARE_GRACE: std::time::Duration = std::time::Duration::from_secs(1);

/// The classes the server's poll reports now: of `added` (new since the last
/// poll) and the ones pending from earlier polls, those prepared (or further)
/// and those pending for [`CLASS_PREPARE_GRACE`]; the rest stay pending.
/// Takes the class-manager lock, then the debug-state lock.
fn classes_ready_for_prepare_events(
    shared: &crate::vm::SharedVm,
    added: Vec<(u64, String, u8)>,
    now: std::time::Instant,
) -> Vec<(u64, String, u8)> {
    use crate::classloading::ClassState;
    let cm = shared.classes.class_manager.read();
    let mut ds = shared.debug.debug_state.lock();
    if added.is_empty() && ds.class_prepare_pending.is_empty() {
        return Vec::new();
    }
    let mut candidates = std::mem::take(&mut ds.class_prepare_pending);
    candidates.extend(added.into_iter().map(|class| (class, now)));
    let mut ready = Vec::new();
    for (class, since) in candidates {
        let prepared = u32::try_from(class.0)
            .ok()
            .and_then(|raw| cm.class_store.get(crate::classloading::ClassId::new(raw)))
            .is_none_or(|c| {
                !matches!(
                    c.state,
                    ClassState::Loading
                        | ClassState::Loaded
                        | ClassState::Verifying
                        | ClassState::Verified
                        | ClassState::Preparing
                )
            });
        if prepared || now.duration_since(since) >= CLASS_PREPARE_GRACE {
            ready.push(class);
        } else {
            ds.class_prepare_pending.push((class, since));
        }
    }
    ready
}

/// A class of this VM was just prepared on VM thread `thread_id`
/// (interpreter round i1 wave 10): report it to the `ClassPrepare` requests
/// from that thread. Called by class initialization (JVMS §5.5) once the
/// class is `Prepared` — which, since wave 15, is part of its linking, before
/// its superclass, its superinterfaces and its `<clinit>` are initialized or
/// run — behind one load when no request is in force.
///
/// The event names the preparing thread and applies each request's policy
/// to it — `SUSPEND_EVENT_THREAD` suspends this thread, `SUSPEND_ALL` the VM
/// — and a `ThreadOnly` modifier is evaluated. The thread is only counted
/// suspended here, never parked: it holds the class's initialization claim
/// and possibly class-manager state. It parks at its next interpreter
/// bytecode — a suspension concerns every method, and the dispatch loop
/// re-reads its gate at the frame switch into the code that follows (the
/// `<clinit>`, or the caller's next bytecode) — so no bytecode of the class
/// runs before the debugger has seen the event, which is what JDI's deferred
/// breakpoints (`stop at Foo:12` before `Foo` loads) need. Until wave 10 the
/// server reported every class after the fact, with no thread, and a
/// `SUSPEND_EVENT_THREAD` request suspended nothing.
///
/// Not on the JDWP server or a thread running the debugger's heap work
/// ([`debugger_hooks_suppressed`]); the server's poll reports those classes.
pub(crate) fn class_prepared_on_thread(
    shared: &crate::vm::SharedVm,
    thread_id: u64,
    class_id: crate::classloading::ClassId,
) {
    // Wave 13: a C JVMTI agent's `ClassPrepare` callback, on this thread,
    // before any code of the class runs.
    if shared.debug.debugger_gates.native_class_prepare_armed() {
        crate::jvmti::native_env::post_class_prepare(shared, thread_id, class_id);
    }
    if !shared.debug.debugger_gates.class_prepare_armed() || debugger_hooks_suppressed() {
        return;
    }
    let wants_smap = shared
        .debug
        .debug_state
        .lock()
        .events
        .has_source_name_filters();
    let queued = {
        let cm = shared.classes.class_manager.read();
        let Some(class) = cm.class_store.get(class_id) else {
            return;
        };
        // Wave 43: the SMAP's file names, read before the debug-state lock
        // (it may read the class file).
        let smap_names = if wants_smap {
            smap_source_names(&cm, class)
        } else {
            Vec::new()
        };
        let mut source_names: Vec<&str> = class.source_file.as_deref().into_iter().collect();
        source_names.extend(smap_names.iter().map(String::as_str));
        let mut ds = shared.debug.debug_state.lock();
        if !ds
            .class_prepare_reported
            .insert(u64::from(class_id.as_u32()))
        {
            return;
        }
        let (class_wire, sig, tag) = add_class_metadata(&mut ds, class);
        // `ClassOnly` (wave 24): the class manager is held already.
        let class_is = |id: u64| {
            u32::try_from(id).is_ok_and(|raw| {
                cm.is_subclass_of(class_id, crate::classloading::ClassId::new(raw))
            })
        };
        let hits = ds.events.match_class_prepare_on(
            &class.name,
            &source_names,
            Some(thread_id),
            &class_is,
        );
        // Released before the gates are published: that flushes the JIT's
        // inline caches, whose lock a compiling thread may hold while it
        // waits for the class manager.
        drop(cm);
        // One event set, suspended once with its strongest policy (wave 22).
        let policy = strongest_suspend_policy(hits.iter().map(|&(_, p)| p));
        let suspended = ds.apply_event_set_policy(policy, thread_id);
        let mut queued = Vec::with_capacity(hits.len());
        for (request_id, suspend_policy) in hits {
            let mut extra = protocol::PayloadWriter::new();
            extra.put_u8(tag);
            extra.put_u64_be(ids::class_to_wire(class_wire));
            extra.put_string(&sig);
            extra.put_u32_be(3); // status: VERIFIED | PREPARED
            queued.push(DebugEvent {
                kind: events::EventKind::ClassPrepare,
                request_id,
                suspend_policy,
                thread_id,
                class_id: 0,
                method_id: 0,
                offset: 0,
                extra: extra.into_bytes(),
                set_follows: false,
            });
        }
        // Wave 46 (lane L1): or a `Count` this match spent.
        let spent = take_spent_requests(&mut ds);
        if suspended || spent {
            publish_debugger_gates(shared, &ds);
        }
        queued
    };
    send_event_set(shared, queued);
}

// ---------------------------------------------------------------------------
// DebuggerGates — what the interpreter and the JIT read without the lock
// ---------------------------------------------------------------------------

/// [`DebuggerGates::method_events_armed`]'s bit for a JDWP method entry or
/// exit request in force (interpreter round i1 wave 27, lane L1).
const METHOD_EVENTS_JDWP: u8 = 1;
/// [`DebuggerGates::method_events_armed`]'s bit for a JVMTI env listening for
/// `MethodEntry` or `MethodExit` (wave 27).
const METHOD_EVENTS_JVMTI: u8 = 2;
/// [`DebuggerGates::method_events_armed`]'s bit for a JDWP or JVMTI
/// breakpoint, or a JDWP step request, in force (interpreter round i1 wave
/// 28, lane L1): a Java method a registered native or an interpreter
/// intrinsic stands in for must run its bytecode, in a frame, when a
/// breakpoint sits in it or a step may enter it
/// ([`DebuggerGates::java_frames_armed`],
/// `interpreter::run_stood_in_java_method`).
const METHOD_EVENTS_FRAMES: u8 = 4;
/// [`DebuggerGates::method_events_armed`]'s bit for a JVMTI env of this VM
/// listening for `SingleStep` (interpreter round i1 wave 29, lane L1): a
/// stepping agent must step through a stood-in Java method's bytecode, as
/// HotSpot's interpreter-only mode does ([`DebuggerGates::jvmti_frames_armed`],
/// `interpreter::run_stood_in_java_method`). Set by the C JVMTI table only.
const METHOD_EVENTS_JVMTI_FRAMES: u8 = 8;
/// [`DebuggerGates::method_events_armed`]'s bit for a JDWP session on this VM
/// (interpreter round i1 wave 46, lane L1): a Java
/// method that BLOCKS the thread, served by a registered native standing in
/// for it (`Thread.sleep`, `Object.wait`: [`BLOCKING_STANDINS`]), runs its
/// bytecode instead, so a thread blocked or suspended inside it lists
/// HotSpot's frames -- `Thread.sleepNanos0` (native) under `sleepNanos` and
/// `sleep`, `Object.wait0` under `wait` -- and the commands HotSpot refuses
/// on a native frame (`ForceEarlyReturn`, `PopFrames`) are refused
/// (`interpreter::run_stood_in_java_method`). Only the stand-ins' own
/// callbacks take the hook ([`DebuggerGates::blocking_standin_matches`]).
const METHOD_EVENTS_BLOCKING: u8 = 16;

/// The Java methods whose registered stand-in natives block the calling
/// thread, and whose JDK bytecode reaches a genuine `native` leaf
/// ([`METHOD_EVENTS_BLOCKING`]): JDK 25's `Thread.sleep(long)` and
/// `sleep(long, int)` (`sleepNanos` -> `sleepNanos0`), `sleep(Duration)`,
/// and `Object.wait()` / `wait(long)` / `wait(long, int)` (`wait0`). The same
/// family `stackwalker`'s stand-in census rebuilds a throwable's frames for.
pub(crate) const BLOCKING_STANDINS: [(&str, &str, &str); 6] = [
    ("java/lang/Thread", "sleep", "(J)V"),
    ("java/lang/Thread", "sleep", "(JI)V"),
    ("java/lang/Thread", "sleep", "(Ljava/time/Duration;)V"),
    ("java/lang/Object", "wait", "()V"),
    ("java/lang/Object", "wait", "(J)V"),
    ("java/lang/Object", "wait", "(JI)V"),
];

/// [`DebuggerGates::every_body_withdrawn`]'s bit for a JDWP request that
/// needs every method interpreted, including the ones already running
/// compiled: a step request, or a `MethodEntry` / `MethodExit` request
/// (interpreter round i1 wave 37, lane L1), or since wave 38 a
/// `FieldAccess` / `FieldModification` request. Set by
/// [`publish_debugger_gates`].
pub(crate) const WITHDRAWAL_BY_JDWP: u8 = 1;
/// [`DebuggerGates::every_body_withdrawn`]'s bit for a JVMTI env of this VM
/// listening for `MethodEntry`, `MethodExit`, `SingleStep` or `FramePop`
/// (wave 37). Set by `jvmti::publish_union_listener_flags`.
pub(crate) const WITHDRAWAL_BY_JVMTI: u8 = 2;
/// [`DebuggerGates::every_body_withdrawn`]'s bit for a JDWP or JVMTI
/// breakpoint in a method of a JDK class, which the JIT's call-site
/// intrinsics may expand in a caller without asking any VM resolver
/// (interpreter round i1 wave 40, lane L1;
/// `interpreter::jit_may_expand_a_method_of_without_a_record`). Set by
/// [`publish_debugger_gates`], which also keeps every method interpreted
/// while it is up ([`DebuggerGates::requires_interpreter`]).
pub(crate) const WITHDRAWAL_BY_JDK_BREAKPOINT: u8 = 4;

/// How long the interpreter's tier-up strides stay closed after the last
/// source of [`DebuggerGates::every_body_withdrawn`] goes (interpreter round
/// i1 wave 39, lane L1; the linger of
/// `docs/internal/fixed-bugs/interpreter-L1-proposal-keep-the-withdrawal-across-a-stepping-session-FIXED-20261003.md`).
/// A debugger creates and deletes a step request per step, so without the
/// linger every step reopened the strides, the hot methods were compiled
/// again, and the next step withdrew them again: a whole-cache flush per
/// step. An IDE's steps come faster than this. `0` is the kill switch: the
/// strides reopen at once, as in waves 37-38 (the next arm still skips its
/// flush when nothing was compiled since the last one,
/// `interpreter::note_every_method_needs_the_interpreter`).
pub(crate) const WITHDRAWAL_LINGER_MS: u64 = 5_000;

/// Which methods the JDWP requests in force concern, readable without the
/// `debug_state` lock (interpreter round i1 wave 7). Lives in
/// `shared.debug.debugger_gates`; written only by [`publish_debugger_gates`].
///
/// A breakpoint concerns the method it sits in; a step request or a
/// suspension concerns every method (a step may enter any callee, and every
/// thread must reach a suspend point). Two readers, both consulted only
/// while `shared.debug.breakpoints_active` is set:
///
/// * the dispatch loop's debugger gate (`interpreter::breakpoints_armed`,
///   refreshed at frame switches and back edges): the per-bytecode suspend
///   point and the fusion veto run only in methods the requests concern. They
///   used to run in every method while any breakpoint anywhere was set, each
///   bytecode taking the debug-state lock twice;
/// * the JIT's interpreter→compiled doors
///   (`jit_bridge::jvmti_requires_interpreter_for`), which used to stand down
///   for every method while any breakpoint anywhere was set.
///
/// The per-method answer is one load when everything or nothing is
/// concerned, and one more load and a bit test (class id modulo 64) before
/// the exact lookup under a read lock, which only a class holding a
/// breakpoint (or sharing its bit) pays.
///
/// Wave 10: the VM's JVMTI breakpoints are folded into the per-method set;
/// field watches and exception requests keep every method interpreted for
/// the JIT ([`Self::requires_interpreter`]) without arming the per-bytecode
/// suspend point, and arm their own hooks ([`Self::field_watch_armed`],
/// [`Self::exception_events_armed`]); `ClassPrepare` requests and an attached
/// session have flags of their own.
pub struct DebuggerGates {
    /// Bumped after every publication, so a cached per-method answer can
    /// tell that it is stale.
    generation: std::sync::atomic::AtomicU64,
    /// A step request or a suspension is in force: every method.
    all_methods: std::sync::atomic::AtomicBool,
    /// Bit `class_id % 64` of every class holding a breakpoint.
    breakpoint_classes: std::sync::atomic::AtomicU64,
    /// `(class id, jdwp_method_id)` of every method holding a breakpoint.
    breakpoint_methods: parking_lot::RwLock<HashSet<(u64, u64)>>,
    /// A `FieldAccess` / `FieldModification` request is in force (wave 10):
    /// the four field bytecodes run their JDWP hook
    /// (`interpreter::deliver_field_watch_if_armed`), behind the process-wide
    /// `jvmti::any_field_watchpoint_active` pre-filter this also raises
    /// (`jvmti::note_debugger_field_watch`), which already turns off every
    /// quickened field arm.
    field_watch: std::sync::atomic::AtomicBool,
    /// An `Exception` request is in force (wave 10): the interpreter's
    /// unwinder runs its JDWP hook (`interpreter::deliver_exception_event_if_armed`).
    exceptions: std::sync::atomic::AtomicBool,
    /// Every method must run interpreted, without the dispatch loop's
    /// per-bytecode suspend point (wave 10): a field watch or an exception
    /// request is in force, and only the interpreter posts those events. The
    /// JIT's doors ask [`Self::requires_interpreter`]. Wave 40 (lane L1): or
    /// a breakpoint sits in a method of a JDK class
    /// ([`WITHDRAWAL_BY_JDK_BREAKPOINT`]), which a caller compiled after it
    /// could expand as a call-site intrinsic.
    interpret_all: std::sync::atomic::AtomicBool,
    /// A `ClassPrepare` request is in force (wave 10): a class prepared by a
    /// thread of this VM is reported from that thread
    /// ([`class_prepared_on_thread`]).
    class_prepare: std::sync::atomic::AtomicBool,
    /// A debugger is attached (wave 10): a thread entering a blocking native
    /// region opens its frames to the debugger
    /// (`interpreter::open_blocked_inspection`, wave 12), so
    /// `ThreadReference.Frames` can answer for a thread suspended while it is
    /// blocked.
    session: std::sync::atomic::AtomicBool,
    /// A JDWP suspension is in force (wave 12; `DebugState::any_suspension`):
    /// a thread entering a blocking native region checks its own suspend
    /// count only while this is up (`interpreter::publish_blocked_frames`).
    suspension: std::sync::atomic::AtomicBool,
    /// A C JVMTI env of this VM has `ClassPrepare` enabled (interpreter
    /// round i1 wave 13): [`class_prepared_on_thread`] delivers it to the
    /// agent's callback (`jvmti::native_env::post_class_prepare`). Set by
    /// `native_env`, never by [`publish_debugger_gates`].
    native_class_prepare: std::sync::atomic::AtomicBool,
    /// A C JVMTI env is bound to this VM (interpreter round i1 wave 45, lane
    /// L1): a thread entering a blocking native region opens its inspection
    /// window (`interpreter::open_blocked_inspection`), so the table's stack
    /// functions can read it from another thread, as HotSpot reads a blocked
    /// thread's stack. Set once by `native_env` when an env binds, never
    /// cleared.
    native_env_present: std::sync::atomic::AtomicBool,
    /// The callbacks registered for [`BLOCKING_STANDINS`], in that order, 0
    /// for none (interpreter round i1 wave 46, lane L1): filled when
    /// [`METHOD_EVENTS_BLOCKING`] is raised ([`arm_blocking_standins`]).
    blocking_standins: [std::sync::atomic::AtomicUsize; BLOCKING_STANDINS.len()],
    /// This VM's C JVMTI envs are in the start phase (wave 14): `Vm::new`
    /// bound startup agents' envs and the VM has not reached the live phase
    /// (`SharedVm::set_init_level(3)`, after `System.initPhase2`), where
    /// `jvmti::native_env::enter_live_phase` lowers it and posts `VMInit`.
    /// Set and cleared by `native_env` only.
    jvmti_start_phase: std::sync::atomic::AtomicBool,
    /// A JDWP `MethodEntry` request is in force in this VM (interpreter
    /// round i1 wave 24). Kept for its edges, which move the process-wide
    /// pre-filter the frame pushes read ([`method_entry_requests_anywhere`]).
    method_entry: std::sync::atomic::AtomicBool,
    /// Who wants the method events of native methods in this VM, as bits: a
    /// JDWP `MethodEntry`, `MethodExit` or `MethodExitWithReturnValue`
    /// request is in force ([`METHOD_EVENTS_JDWP`], interpreter round i1
    /// wave 26, lane L1), or a JVMTI env of this VM listens for `MethodEntry`
    /// or `MethodExit` ([`METHOD_EVENTS_JVMTI`], wave 27) or `SingleStep`
    /// ([`METHOD_EVENTS_JVMTI_FRAMES`], wave 29), or a breakpoint or
    /// a step request is in force ([`METHOD_EVENTS_FRAMES`], wave 28). The one load the
    /// native-call funnel (`vm_exec::safe_native_call_impl`) and its leaf
    /// twin read before they report a native method's entry and exit
    /// (`interpreter::report_native_method_entry`). Each writer sets or
    /// clears its own bit (the JDWP server under the debug-state lock, the C
    /// JVMTI table under the VM's JVMTI lock), so the two cannot overwrite
    /// each other's answer.
    method_events: std::sync::atomic::AtomicU8,
    /// A `ThreadReference.Stop` is pending for some thread of this VM
    /// (interpreter round i1 wave 25): the interpreter's unwinder asks
    /// [`exception_or_pending_stop`] before it routes an exception, so a stop
    /// replaces the exception in flight — the `InterruptedException` of the
    /// blocking call the stop woke — as HotSpot's asynchronous exception
    /// replaces a pending one.
    stops: std::sync::atomic::AtomicBool,
    /// A `StackFrame.SetValues` sent while its thread was blocked in a native
    /// waits to be applied by that thread (interpreter round i1 wave 41,
    /// lane L1; [`DebugState::deferred_frame_writes`]): the blocking region's
    /// exit asks `inspect::apply_deferred_frame_writes_if_any` only while
    /// this is up. Written under the debug-state lock only.
    frame_writes: std::sync::atomic::AtomicBool,
    /// Who holds this VM's compiled code withdrawn, as bits (interpreter
    /// round i1 wave 37, lane L1): [`WITHDRAWAL_BY_JDWP`], a JDWP step,
    /// method event or (wave 38) field watch request, and
    /// [`WITHDRAWAL_BY_JVMTI`], a JVMTI listener
    /// for an event HotSpot posts from the interpreter only. On the edge
    /// where it leaves `0` every compiled body is withdrawn and made not
    /// entrant, and the running ones leave at their next exit-capable point
    /// (`interpreter::note_every_method_needs_the_interpreter`); while it is
    /// not `0` the interpreter's tier-up strides offer nothing to compile.
    /// Each writer sets or clears its own bit.
    withdrawal: std::sync::atomic::AtomicU8,
    /// Until when (milliseconds on [`Self::linger_clock_origin`]'s clock) the
    /// tier-up strides stay closed after [`Self::withdrawal`] fell to `0`
    /// (wave 39, lane L1; [`WITHDRAWAL_LINGER_MS`]); `0` when not lingering.
    /// Read by [`Self::tier_up_held`] only while no source is armed.
    withdrawal_linger_until: std::sync::atomic::AtomicU64,
    /// The code cache's generation (`JitCache::generation`) the last
    /// whole-cache withdrawal left (wave 39): the next arm finds nothing
    /// compiled since while it still reads this, and skips its flush
    /// (`interpreter::note_every_method_needs_the_interpreter`). `0` before
    /// the first withdrawal, which no cache generation equals.
    withdrawal_generation: std::sync::atomic::AtomicU64,
    /// Arms of the withdrawal that found nothing compiled since the last one
    /// and so withdrew nothing (wave 39): a count for the tests and the
    /// stepping-session positive control.
    withdrawals_held: std::sync::atomic::AtomicU64,
    /// The origin of the linger's monotonic clock: this VM's gates' creation.
    linger_clock_origin: std::time::Instant,
    /// Bumped whenever [`Self::breakpoint_methods`] changes (interpreter
    /// round i1 wave 44, lane L1): the generation [`Self::caller_verdicts`]
    /// are judged at. Not [`Self::generation`], which every publication
    /// bumps (a suspension and its resume at each stop included).
    breakpoint_generation: std::sync::atomic::AtomicU64,
    /// `(class id, jdwp_method_id)` of a method the compile doors asked
    /// about -> `(breakpoint generation, does its bytecode invoke a method
    /// holding a breakpoint)` (wave 44, lane L1;
    /// `interpreter::breakpoint_bars_compiling`). A verdict of an older
    /// generation is stale; the map is emptied when the last breakpoint
    /// goes. Asked only at compile time and at the tier-up strides, while a
    /// breakpoint is set.
    caller_verdicts: parking_lot::Mutex<HashMap<(u32, u64), (u64, bool)>>,
}

impl DebuggerGates {
    pub fn new() -> Self {
        Self {
            generation: std::sync::atomic::AtomicU64::new(0),
            all_methods: std::sync::atomic::AtomicBool::new(false),
            breakpoint_classes: std::sync::atomic::AtomicU64::new(0),
            breakpoint_methods: parking_lot::RwLock::new(HashSet::new()),
            field_watch: std::sync::atomic::AtomicBool::new(false),
            exceptions: std::sync::atomic::AtomicBool::new(false),
            interpret_all: std::sync::atomic::AtomicBool::new(false),
            class_prepare: std::sync::atomic::AtomicBool::new(false),
            session: std::sync::atomic::AtomicBool::new(false),
            suspension: std::sync::atomic::AtomicBool::new(false),
            native_class_prepare: std::sync::atomic::AtomicBool::new(false),
            native_env_present: std::sync::atomic::AtomicBool::new(false),
            blocking_standins: std::array::from_fn(|_| std::sync::atomic::AtomicUsize::new(0)),
            jvmti_start_phase: std::sync::atomic::AtomicBool::new(false),
            method_entry: std::sync::atomic::AtomicBool::new(false),
            method_events: std::sync::atomic::AtomicU8::new(0),
            stops: std::sync::atomic::AtomicBool::new(false),
            frame_writes: std::sync::atomic::AtomicBool::new(false),
            withdrawal: std::sync::atomic::AtomicU8::new(0),
            withdrawal_linger_until: std::sync::atomic::AtomicU64::new(0),
            withdrawal_generation: std::sync::atomic::AtomicU64::new(0),
            withdrawals_held: std::sync::atomic::AtomicU64::new(0),
            linger_clock_origin: std::time::Instant::now(),
            breakpoint_generation: std::sync::atomic::AtomicU64::new(0),
            caller_verdicts: parking_lot::Mutex::new(HashMap::new()),
        }
    }

    /// Is this VM's compiled code held withdrawn for a debugger or an agent
    /// that needs every method interpreted (wave 37)? One relaxed load,
    /// asked at the interpreter's tier-up strides (never per call); `false`
    /// without a debugger or an agent.
    #[inline(always)]
    pub fn every_body_withdrawn(&self) -> bool {
        self.withdrawal.load(std::sync::atomic::Ordering::Relaxed) != 0
    }

    /// Raise (`armed`) or lower one writer's bit of
    /// [`Self::every_body_withdrawn`]; answers the bits as they were before.
    pub(crate) fn set_withdrawal_source(&self, bit: u8, armed: bool) -> u8 {
        use std::sync::atomic::Ordering;
        if armed {
            self.withdrawal.fetch_or(bit, Ordering::AcqRel)
        } else {
            self.withdrawal.fetch_and(!bit, Ordering::AcqRel)
        }
    }

    /// Do the interpreter's tier-up strides offer nothing to compile (wave
    /// 39, lane L1)? While a source holds the code withdrawn
    /// ([`Self::every_body_withdrawn`]), and for [`WITHDRAWAL_LINGER_MS`]
    /// after the last one went, so a debugger's next step request finds
    /// nothing compiled and takes no second flush. One relaxed load while a
    /// source is armed or nothing lingers; a second one, and a clock read,
    /// only during a linger. Asked at the tier-up strides only, never per
    /// call.
    #[inline(always)]
    pub fn tier_up_held(&self) -> bool {
        if self.every_body_withdrawn() {
            return true;
        }
        let until = self
            .withdrawal_linger_until
            .load(std::sync::atomic::Ordering::Relaxed);
        until != 0 && self.linger_still_running(until)
    }

    /// The linger's clock read: `true` before `until`; after it, the linger
    /// is forgotten, so the next stride pays one load again.
    #[cold]
    #[inline(never)]
    fn linger_still_running(&self, until: u64) -> bool {
        if self.linger_clock_ms() < until {
            return true;
        }
        let _ = self.withdrawal_linger_until.compare_exchange(
            until,
            0,
            std::sync::atomic::Ordering::Relaxed,
            std::sync::atomic::Ordering::Relaxed,
        );
        false
    }

    /// Milliseconds since this VM's gates were created (monotonic).
    fn linger_clock_ms(&self) -> u64 {
        u64::try_from(self.linger_clock_origin.elapsed().as_millis()).unwrap_or(u64::MAX)
    }

    /// The last source of [`Self::every_body_withdrawn`] just went: keep the
    /// tier-up strides closed for [`WITHDRAWAL_LINGER_MS`] more (wave 39).
    pub(crate) fn begin_withdrawal_linger(&self) {
        if WITHDRAWAL_LINGER_MS == 0 {
            return;
        }
        let until = self
            .linger_clock_ms()
            .saturating_add(WITHDRAWAL_LINGER_MS)
            .max(1);
        self.withdrawal_linger_until
            .store(until, std::sync::atomic::Ordering::Relaxed);
    }

    /// A source armed again: the linger, if one ran, is over (the armed bit
    /// holds the strides now). Answers whether one was still running.
    pub(crate) fn end_withdrawal_linger(&self) -> bool {
        let until = self
            .withdrawal_linger_until
            .swap(0, std::sync::atomic::Ordering::Relaxed);
        until != 0 && self.linger_clock_ms() < until
    }

    /// The code cache's generation the last whole-cache withdrawal left
    /// (wave 39); `0` before the first.
    pub(crate) fn withdrawal_generation(&self) -> u64 {
        self.withdrawal_generation
            .load(std::sync::atomic::Ordering::Acquire)
    }

    /// Record the code cache's generation a whole-cache withdrawal just left.
    pub(crate) fn note_withdrawal_generation(&self, generation: u64) {
        self.withdrawal_generation
            .store(generation, std::sync::atomic::Ordering::Release);
    }

    /// Count an arm that withdrew nothing (wave 39).
    pub(crate) fn note_withdrawal_held(&self) {
        self.withdrawals_held
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }

    /// Arms that withdrew nothing so far (wave 39).
    pub(crate) fn withdrawals_held(&self) -> u64 {
        self.withdrawals_held
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Is a `ThreadReference.Stop` pending for a thread of this VM (wave
    /// 25)? One load, asked by the unwinder for every exception it routes in
    /// a build with a JDWP server.
    #[inline(always)]
    pub fn stop_pending(&self) -> bool {
        self.stops.load(std::sync::atomic::Ordering::Acquire)
    }

    /// Is a deferred `StackFrame.SetValues` waiting for some thread of this
    /// VM (wave 41, [`DebugState::deferred_frame_writes`])? One load, asked
    /// at every blocking region's exit in a build with a JDWP server.
    #[inline(always)]
    pub(crate) fn frame_writes_pending(&self) -> bool {
        self.frame_writes.load(std::sync::atomic::Ordering::Acquire)
    }

    /// Raise or lower [`Self::frame_writes_pending`]; under the debug-state
    /// lock, from whether [`DebugState::deferred_frame_writes`] is empty.
    pub(crate) fn set_frame_writes_pending(&self, pending: bool) {
        self.frame_writes
            .store(pending, std::sync::atomic::Ordering::Release);
    }

    /// Are this VM's C JVMTI envs in the start phase (wave 14)? One load,
    /// asked by `SharedVm::set_init_level` and the C table's phase checks.
    #[inline(always)]
    pub fn jvmti_start_phase(&self) -> bool {
        self.jvmti_start_phase
            .load(std::sync::atomic::Ordering::Acquire)
    }

    /// `native_env`'s entry into the start phase (`true`) or into the live
    /// phase (`false`); answers whether the VM was in the start phase, so the
    /// live transition happens once.
    pub(crate) fn swap_jvmti_start_phase(&self, start: bool) -> bool {
        self.jvmti_start_phase
            .swap(start, std::sync::atomic::Ordering::AcqRel)
    }

    /// Is a debugger attached to this VM? One load, asked by every blocking
    /// native transition (`interpreter::publish_blocked_frames`).
    #[inline(always)]
    pub fn session_attached(&self) -> bool {
        self.session.load(std::sync::atomic::Ordering::Acquire)
    }

    /// Is a JDWP method entry or exit request in force in this VM (wave 26),
    /// or does a JVMTI env of it listen for `MethodEntry` / `MethodExit`
    /// (wave 27) or `SingleStep` (wave 29, [`Self::jvmti_frames_armed`]), or
    /// is a breakpoint or a step request in force (wave 28,
    /// [`Self::java_frames_armed`])? One relaxed load, asked by the native-call funnel for every
    /// native call in a build with a JDWP server; false without a debugger
    /// or an agent.
    #[inline(always)]
    pub fn method_events_armed(&self) -> bool {
        self.method_events
            .load(std::sync::atomic::Ordering::Relaxed)
            != 0
    }

    /// The JDWP half of [`Self::method_events_armed`] (wave 27): a JDWP
    /// method entry or exit request is in force.
    #[inline]
    pub fn jdwp_method_events_armed(&self) -> bool {
        self.method_events
            .load(std::sync::atomic::Ordering::Acquire)
            & METHOD_EVENTS_JDWP
            != 0
    }

    /// The JVMTI half of [`Self::method_events_armed`] (wave 27): an env of
    /// this VM listens for `MethodEntry` or `MethodExit`.
    #[inline]
    pub fn jvmti_method_events_armed(&self) -> bool {
        self.method_events
            .load(std::sync::atomic::Ordering::Acquire)
            & METHOD_EVENTS_JVMTI
            != 0
    }

    /// The bit of [`Self::method_events_armed`] for a breakpoint or a step
    /// request (wave 28): the native-call funnel asks whether the Java
    /// method a native stands in for is concerned
    /// ([`Self::concerns_method`]) and, if so, runs its bytecode instead
    /// (`interpreter::run_stood_in_java_method`).
    #[inline]
    pub fn java_frames_armed(&self) -> bool {
        self.method_events
            .load(std::sync::atomic::Ordering::Acquire)
            & METHOD_EVENTS_FRAMES
            != 0
    }

    /// The bit of [`Self::method_events_armed`] for a JVMTI `SingleStep`
    /// listener (wave 29): every stood-in Java method the funnel's hook
    /// accepts runs its bytecode, in a frame, so the agent's steps enter it
    /// (`interpreter::run_stood_in_java_method`). JVMTI breakpoints ride
    /// [`Self::java_frames_armed`] through [`publish_debugger_gates`].
    #[inline]
    pub fn jvmti_frames_armed(&self) -> bool {
        self.method_events
            .load(std::sync::atomic::Ordering::Acquire)
            & METHOD_EVENTS_JVMTI_FRAMES
            != 0
    }

    /// The bit of [`Self::method_events_armed`] for a JDWP session (wave 46,
    /// [`METHOD_EVENTS_BLOCKING`]).
    #[inline]
    pub fn blocking_standins_armed(&self) -> bool {
        self.method_events
            .load(std::sync::atomic::Ordering::Acquire)
            & METHOD_EVENTS_BLOCKING
            != 0
    }

    /// Which of [`BLOCKING_STANDINS`] `callback` is the registered native of
    /// (wave 46): bit `i` for entry `i`, 0 for none. A few loads; asked by the
    /// native-call funnel's debugger hook only while
    /// [`Self::blocking_standins_armed`].
    #[inline]
    pub fn blocking_standin_matches(&self, callback: usize) -> u8 {
        if callback == 0 {
            return 0;
        }
        let mut matches = 0u8;
        for (i, slot) in self.blocking_standins.iter().enumerate() {
            if slot.load(std::sync::atomic::Ordering::Acquire) == callback {
                matches |= 1 << i;
            }
        }
        matches
    }

    /// Raise or lower one writer's bit of [`Self::method_events_armed`].
    fn set_method_events_bit(&self, bit: u8, armed: bool) {
        use std::sync::atomic::Ordering;
        if armed {
            self.method_events.fetch_or(bit, Ordering::AcqRel);
        } else {
            self.method_events.fetch_and(!bit, Ordering::AcqRel);
        }
    }

    /// The C JVMTI table's publication of its half of
    /// [`Self::method_events_armed`] (wave 27): whether this VM's JVMTI
    /// manager has a `MethodEntry` or `MethodExit` listener
    /// (`jvmti::native_env::refresh_method_events_gate`).
    pub(crate) fn set_jvmti_method_events(&self, armed: bool) {
        self.set_method_events_bit(METHOD_EVENTS_JVMTI, armed);
    }

    /// The C JVMTI table's publication of [`Self::jvmti_frames_armed`]
    /// (wave 29): whether this VM's JVMTI manager has a `SingleStep`
    /// listener (`jvmti::native_env::refresh_method_events_gate`).
    pub(crate) fn set_jvmti_single_step(&self, armed: bool) {
        self.set_method_events_bit(METHOD_EVENTS_JVMTI_FRAMES, armed);
    }

    /// Is a JDWP suspension (of any thread, or VM-wide) in force? One load;
    /// published with the other gates by [`publish_debugger_gates`].
    #[inline(always)]
    pub fn suspension_in_force(&self) -> bool {
        self.suspension.load(std::sync::atomic::Ordering::Acquire)
    }

    /// A debugger attached (`true`) or detached (`false`); the JDWP server
    /// sets it.
    pub(crate) fn set_session_attached(&self, attached: bool) {
        self.session
            .store(attached, std::sync::atomic::Ordering::Release);
    }

    /// Is a JDWP `ClassPrepare` request in force in this VM? One load, asked
    /// by class initialization when it prepares a class.
    #[inline(always)]
    pub fn class_prepare_armed(&self) -> bool {
        self.class_prepare
            .load(std::sync::atomic::Ordering::Acquire)
    }

    /// Does a C JVMTI env of this VM take `ClassPrepare` (wave 13)? One
    /// load, asked by class initialization when it prepares a class.
    #[inline(always)]
    pub fn native_class_prepare_armed(&self) -> bool {
        self.native_class_prepare
            .load(std::sync::atomic::Ordering::Acquire)
    }

    /// Is a C JVMTI env bound to this VM (wave 45)? One load, asked by a
    /// thread opening its blocking region's inspection window.
    #[inline(always)]
    pub fn native_env_present(&self) -> bool {
        self.native_env_present
            .load(std::sync::atomic::Ordering::Acquire)
    }

    /// `native_env`'s publication of [`Self::native_env_present`] (wave 45).
    pub(crate) fn set_native_env_present(&self) {
        self.native_env_present
            .store(true, std::sync::atomic::Ordering::Release);
    }

    /// `native_env`'s publication of [`Self::native_class_prepare_armed`].
    pub(crate) fn set_native_class_prepare(&self, armed: bool) {
        self.native_class_prepare
            .store(armed, std::sync::atomic::Ordering::Release);
    }

    /// Is a JDWP field watch in force in this VM? One load; the field
    /// bytecodes ask it only after the process-wide pre-filter.
    #[inline(always)]
    pub fn field_watch_armed(&self) -> bool {
        self.field_watch.load(std::sync::atomic::Ordering::Acquire)
    }

    /// Is a JDWP `Exception` request in force in this VM? One load, asked by
    /// the interpreter's unwinder for every exception it routes.
    #[inline(always)]
    pub fn exception_events_armed(&self) -> bool {
        self.exceptions.load(std::sync::atomic::Ordering::Acquire)
    }

    /// Must the method `name` + `descriptor` of class `class_id` run
    /// interpreted for the debugger? The JIT doors' question (wave 10): the
    /// method concerns the requests ([`Self::concerns_method`]), or a field
    /// watch or exception request makes every method interpreted. Meaningful
    /// only while `breakpoints_active` is set.
    #[inline]
    pub fn requires_interpreter(&self, class_id: u32, name: &str, descriptor: &str) -> bool {
        self.interpret_all
            .load(std::sync::atomic::Ordering::Acquire)
            || self.concerns_method(class_id, name, descriptor)
    }

    /// Must EVERY method run interpreted for the debugger — a step request, a
    /// suspension, a field watch or an exception request in force, or (wave
    /// 40) a breakpoint in a JDK class's method? The answer
    /// of [`Self::requires_interpreter`] with no method in hand (interpreter
    /// round i1 wave 15: a method-entry body's loop-exit verdict,
    /// `interpreter::every_compiled_frame_may_leave`). Meaningful only while
    /// `breakpoints_active` is set.
    #[inline]
    pub fn requires_interpreter_everywhere(&self) -> bool {
        self.interpret_all
            .load(std::sync::atomic::Ordering::Acquire)
            || self.all_methods.load(std::sync::atomic::Ordering::Acquire)
    }

    /// The publication count; changes whenever an answer may have.
    #[inline]
    pub fn generation(&self) -> u64 {
        self.generation.load(std::sync::atomic::Ordering::Acquire)
    }

    /// Do the requests in force concern the method `name` + `descriptor` of
    /// class `class_id`? Meaningful only while `breakpoints_active` is set.
    #[inline]
    pub fn concerns_method(&self, class_id: u32, name: &str, descriptor: &str) -> bool {
        if self.all_methods.load(std::sync::atomic::Ordering::Acquire) {
            return true;
        }
        self.holds_breakpoint(class_id, name, descriptor)
    }

    /// Does a JDWP or JVMTI breakpoint sit in the method `name` +
    /// `descriptor` of class `class_id` (its declaring class)? The per-method
    /// set alone, whatever else is in force (interpreter round i1 wave 39,
    /// lane L1): the compile doors ask it so that no compile binds or splices
    /// such a method (`interpreter::breakpoint_bars_compiling`). One load
    /// while no breakpoint is set, and one more bit test before the exact
    /// lookup under a read lock, which only a class holding a breakpoint (or
    /// sharing its bit) pays.
    #[inline]
    pub fn holds_breakpoint(&self, class_id: u32, name: &str, descriptor: &str) -> bool {
        let bit = 1u64 << (class_id % 64);
        if self
            .breakpoint_classes
            .load(std::sync::atomic::Ordering::Acquire)
            & bit
            == 0
        {
            return false;
        }
        self.breakpoint_methods
            .read()
            .contains(&(u64::from(class_id), jdwp_method_id(name, descriptor)))
    }

    /// Does any JDWP or JVMTI breakpoint stand in this VM (interpreter round
    /// i1 wave 44, lane L1)? One load.
    #[inline]
    pub fn any_breakpoint(&self) -> bool {
        self.breakpoint_classes
            .load(std::sync::atomic::Ordering::Acquire)
            != 0
    }

    /// The generation of the breakpoint set (wave 44): changes whenever a
    /// method gains or loses its last breakpoint.
    #[inline]
    pub fn breakpoint_generation(&self) -> u64 {
        self.breakpoint_generation
            .load(std::sync::atomic::Ordering::Acquire)
    }

    /// The `jdwp_method_id` of every method holding a breakpoint, whatever
    /// its class (wave 44): what a caller's invoke instructions are matched
    /// against by name and descriptor
    /// (`interpreter::breakpoint_bars_compiling`).
    pub fn breakpoint_method_ids(&self) -> Vec<u64> {
        let mut ids: Vec<u64> = self
            .breakpoint_methods
            .read()
            .iter()
            .map(|&(_, method_id)| method_id)
            .collect();
        ids.sort_unstable();
        ids.dedup();
        ids
    }

    /// The verdict recorded for the method `(class_id, method_id)` at
    /// breakpoint generation `generation`, if any (wave 44).
    pub fn caller_verdict(&self, class_id: u32, method_id: u64, generation: u64) -> Option<bool> {
        self.caller_verdicts
            .lock()
            .get(&(class_id, method_id))
            .filter(|&&(at, _)| at == generation)
            .map(|&(_, answer)| answer)
    }

    /// Record a verdict of [`Self::caller_verdict`]; answers whether it is
    /// new at this generation (the positive control prints each new `true`
    /// once).
    pub fn record_caller_verdict(
        &self,
        class_id: u32,
        method_id: u64,
        generation: u64,
        answer: bool,
    ) -> bool {
        let previous = self
            .caller_verdicts
            .lock()
            .insert((class_id, method_id), (generation, answer));
        previous != Some((generation, answer))
    }

    fn store(&self, all_methods: bool, methods: HashSet<(u64, u64)>) {
        use std::sync::atomic::Ordering;
        let bits = methods.iter().fold(0u64, |bits, &(class_id, _)| {
            bits | (1u64 << (class_id % 64))
        });
        // The exact set first: a reader that sees a class bit finds its methods.
        {
            let mut set = self.breakpoint_methods.write();
            if *set != methods {
                let none_left = methods.is_empty();
                *set = methods;
                // Wave 44: every caller verdict judged against the old set is
                // stale.
                self.breakpoint_generation.fetch_add(1, Ordering::AcqRel);
                if none_left {
                    self.caller_verdicts.lock().clear();
                }
            }
        }
        self.breakpoint_classes.store(bits, Ordering::Release);
        self.all_methods.store(all_methods, Ordering::Release);
        self.generation.fetch_add(1, Ordering::Release);
    }

    /// Store the field-watch and exception flags (and `interpret_all`, their
    /// union with `jdk_breakpoint`, wave 40), answering the field-watch
    /// flag's previous value and whether `interpret_all` just rose.
    fn store_subject_flags(
        &self,
        field_watch: bool,
        exceptions: bool,
        jdk_breakpoint: bool,
    ) -> (bool, bool) {
        use std::sync::atomic::Ordering;
        let interpret_all = field_watch || exceptions || jdk_breakpoint;
        let was_interpret_all = self.interpret_all.swap(interpret_all, Ordering::AcqRel);
        self.exceptions.store(exceptions, Ordering::Release);
        let was_watching = self.field_watch.swap(field_watch, Ordering::AcqRel);
        (was_watching, interpret_all && !was_interpret_all)
    }
}

impl Default for DebuggerGates {
    fn default() -> Self {
        Self::new()
    }
}

/// Did a matcher spend a request since the last look (a `Count` modifier
/// reached zero; interpreter round i1 wave 46, lane L1,
/// [`events::SPENT_COUNT_RELEASES_GATES`])? The caller, holding the
/// debug-state lock, then republishes the gates ([`publish_debugger_gates`])
/// so the spent request's stop arming them. `CRATONVM_FRAME_TRACE=1` prints
/// `[JDWP_COUNT_SPENT] request=<id>` per spent request: the positive
/// control. One empty-`Vec` take in the common case.
pub(crate) fn take_spent_requests(ds: &mut DebugState) -> bool {
    let spent = ds.events.take_spent();
    if spent.is_empty() {
        return false;
    }
    if crate::runtime::env_cache::frame_trace() {
        for id in &spent {
            eprintln!("[JDWP_COUNT_SPENT] request={id} gates republished");
        }
    }
    true
}

/// [`take_spent_requests`], and republish the gates when a request was
/// spent (wave 46).
pub(crate) fn republish_gates_if_spent(shared: &crate::vm::SharedVm, ds: &mut DebugState) {
    if take_spent_requests(ds) {
        publish_debugger_gates(shared, ds);
    }
}

/// Raise (`armed`) or lower [`METHOD_EVENTS_BLOCKING`] for `shared`
/// (interpreter round i1 wave 46, lane L1): raised when a debugger attaches,
/// lowered when it detaches (kept while a C JVMTI env is bound, which never
/// unbinds; the C table does not raise it itself). The stand-ins' callbacks
/// are looked up in the VM's registry when it is raised.
pub(crate) fn arm_blocking_standins(shared: &crate::vm::SharedVm, armed: bool) {
    use std::sync::atomic::Ordering;
    let gates = &shared.debug.debugger_gates;
    let armed = armed || gates.native_env_present();
    if armed {
        for (slot, (class, name, descriptor)) in gates.blocking_standins.iter().zip(BLOCKING_STANDINS) {
            let callback = shared
                .natives
                .native_methods
                .find(class, name, descriptor)
                .map_or(0, |callback| callback as usize); // Cast: fn address
            slot.store(callback, Ordering::Release);
        }
    }
    gates.set_method_events_bit(METHOD_EVENTS_BLOCKING, armed);
}

/// Publish what the requests and suspensions in `ds` ask of the interpreter
/// and the JIT: the per-method summary ([`DebuggerGates`]) and then the one
/// VM-wide gate, `breakpoints_active` (armed while any breakpoint, step
/// request or suspension is in force). Called with the debug-state lock held
/// by whoever changed them: the JDWP server after every command and at
/// detach, the interpreter's suspend point after an event suspended threads.
pub(crate) fn publish_debugger_gates(shared: &crate::vm::SharedVm, ds: &DebugState) {
    use events::EventKind;
    let gates = &shared.debug.debugger_gates;
    let suspension = ds.any_suspension_of_a_live_thread();
    gates
        .suspension
        .store(suspension, std::sync::atomic::Ordering::Release);
    // Wave 24: a method entry or exit request, like a step, concerns every
    // method — only the interpreter's suspend point reports them — and so
    // does a `ThreadReference.Stop` not yet delivered, which the thread
    // throws at its next interpreted bytecode.
    let (method_entries, method_exits) = ds.events.method_events_requested();
    let all_methods = ds.events.has_single_steps()
        || suspension
        || method_entries
        || method_exits
        || !ds.pending_stops.is_empty();
    gates.stops.store(
        !ds.pending_stops.is_empty(),
        std::sync::atomic::Ordering::Release,
    );
    // Wave 26: the native-call funnel's pre-filter (wave 27: its JDWP bit;
    // the C JVMTI table owns the other).
    gates.set_method_events_bit(METHOD_EVENTS_JDWP, method_entries || method_exits);
    let was_entries = gates
        .method_entry
        .swap(method_entries, std::sync::atomic::Ordering::AcqRel);
    if was_entries != method_entries {
        note_method_entry_requests(method_entries);
    }
    let mut methods = ds.events.breakpoint_locations();
    // Wave 10: the VM's JVMTI breakpoints ride the same per-method gate
    // (`jvmti::set_breakpoint_for_vm`); the suspend point delivers them.
    methods.extend(crate::runtime::jvmti::breakpoint_methods_for_vm(
        shared.vm_identity,
    ));
    // Wave 28: a breakpoint or a step routes the native-call funnel through
    // its debugger hook, which runs a stood-in Java method's bytecode when
    // the breakpoint sits in it or the step may enter it.
    gates.set_method_events_bit(
        METHOD_EVENTS_FRAMES,
        !methods.is_empty() || ds.events.has_single_steps(),
    );
    // Wave 10: field watches and exception requests. Only the interpreter
    // posts their events, so while one is in force every method runs
    // interpreted (`DebuggerGates::requires_interpreter`), but the dispatch
    // loop's per-bytecode suspend point stays off: their hooks sit in the
    // field bytecodes and the unwinder.
    let field_watch = ds.events.has_requests(EventKind::FieldAccess)
        || ds.events.has_requests(EventKind::FieldModification);
    let exceptions = ds.events.has_requests(EventKind::Exception);
    // Wave 40 (lane L1): a breakpoint in a method of a JDK class. The JIT's
    // call-site intrinsics (`try_resolve_*_intrinsic` in `jit/src/lib.rs`:
    // `String.length`, `Math.max`, the boxing methods, ...) expand such a
    // method in a caller from a fixed table, asking no VM resolver, so the
    // compile doors' per-method refusal (`breakpoint_bars_compiling`) cannot
    // keep a caller compiled AFTER the breakpoint from running the method
    // inline and missing it. While one is in force every method runs
    // interpreted (`interpret_all`) and nothing is compiled (the withdrawal
    // bit below), as for a field watch.
    let jdk_breakpoint = crate::runtime::interpreter::JDK_BREAKPOINT_INTERPRETER_ONLY_ENABLED
        && methods.iter().any(|&(class_id, _)| {
            breakpoint_class_is_expanded_without_a_record(shared, ds, class_id)
        });
    let active = all_methods || !methods.is_empty() || field_watch || exceptions;
    let was_all_methods = gates.all_methods.load(std::sync::atomic::Ordering::Acquire);
    // Wave 38 (lane L1): the classes one of whose methods gains a breakpoint
    // at this publication, for the scoped withdrawal below. Nothing is
    // allocated unless the set grew.
    let (methods_changed, mut gained_classes) = {
        let before = gates.breakpoint_methods.read();
        let gained: Vec<u64> = methods
            .iter()
            .filter(|method| !before.contains(*method))
            .map(|&(class_id, _)| class_id)
            .collect();
        (*before != methods, gained)
    };
    gained_classes.sort_unstable();
    gained_classes.dedup();
    gates.store(all_methods, methods);
    let (was_watching, interpret_all_rose) =
        gates.store_subject_flags(field_watch, exceptions, jdk_breakpoint);
    gates.class_prepare.store(
        ds.events.has_requests(EventKind::ClassPrepare),
        std::sync::atomic::Ordering::Release,
    );
    if field_watch != was_watching {
        // The process-wide pre-filter the field bytecodes (and every
        // quickened field arm) test before anything else.
        crate::runtime::jvmti::note_debugger_field_watch(field_watch);
    }
    let was_active = shared
        .debug
        .breakpoints_active
        .swap(active, std::sync::atomic::Ordering::AcqRel);
    // The compiled-code helpers stand down on this VM-wide flag, but a
    // compiled frame already running reaches them only on an inline-cache
    // miss: flush the VM's MIC/PIC slots on the false→true edge, after the
    // store (interpreter round i1 wave 9, lane L5; the JVMTI twin is
    // `jvmti::publish_union_listener_flags`). Nothing is retired.
    //
    // Wave 10: also when the set of methods that must run interpreted grows
    // to every method (a step, a suspension, a field watch or an exception
    // request) while the flag was already up for a breakpoint: a compiled
    // caller would otherwise keep calling its compiled callees through the
    // caches it filled before.
    let every_method_rose = all_methods && !was_all_methods;
    if active && (!was_active || every_method_rose || interpret_all_rose) {
        shared.jit.jit_cache.clear_inline_caches();
    }
    // Wave 37 (lane L1): a step or a method event request needs every method
    // interpreted, including a compiled caller's baked call into a compiled
    // callee and a frame already running compiled, which neither the doors
    // nor the loop-exit pause below reach. On the edge where the first such
    // request comes into force every compiled body is withdrawn and made not
    // entrant, and the running ones leave at their next exit-capable point;
    // compiling resumes when the last one goes. Not for a suspension, a
    // breakpoint or an exception request: a suspension is taken and released
    // on every stop, and a flush per stop would recompile everything each
    // time; a breakpoint withdraws its own class's dependents (below); the
    // compiled catch doors post an exception event themselves. Before the
    // loop-exit pause, so a frame polling in it reads the withdrawal.
    //
    // Wave 38 (lane L1): and a field watch. Compiled code posts no field
    // event, so a frame running compiled, or a compiled callee a baked call
    // reaches, read and wrote the watched field unseen until it returned to
    // the interpreter (`L1W38JdiFieldWatchInCompiledLoop`; HotSpot reports
    // the compiled loop's access). The doors already refuse every compiled
    // entry while a watch is in force (`interpret_all`), so withdrawing costs
    // only the bodies already running.
    crate::runtime::interpreter::note_every_method_needs_the_interpreter(
        shared,
        WITHDRAWAL_BY_JDWP,
        ds.events.has_single_steps() || method_entries || method_exits || field_watch,
    );
    // Wave 40 (lane L1): a breakpoint in a JDK class's method withdraws every
    // body (the whole-cache path the scoped withdrawal below already took for
    // a JDK class) and, unlike that one-shot withdrawal, keeps the tier-up
    // strides closed while it stands, so no caller is compiled that expands
    // the method as a call-site intrinsic. Its own bit, so its positive
    // control names it (`source=jdk-breakpoint`). Before the scoped
    // withdrawal, which then finds every body withdrawn and does nothing.
    crate::runtime::interpreter::note_every_method_needs_the_interpreter(
        shared,
        WITHDRAWAL_BY_JDK_BREAKPOINT,
        jdk_breakpoint,
    );
    // Wave 38 (lane L1): a breakpoint concerns one method, so it does not take
    // the whole-cache withdrawal above, but a compiled caller's baked call
    // into that method's compiled body, a caller that inlined it, and a frame
    // running one of those on a thread no pause reaches never let it be hit.
    // On the edge where a method gains a breakpoint, its class's dependents
    // are withdrawn (`interpreter::note_breakpoint_classes_gained`). The class
    // name comes from the session's own class table, or from the name a JVMTI
    // breakpoint recorded: the class manager is never taken under the
    // debug-state lock (the class-unload poll takes the two in the other
    // order).
    if !gained_classes.is_empty() {
        let classes: Vec<(crate::classloading::ClassId, Option<String>)> = gained_classes
            .iter()
            .filter_map(|&id| {
                let raw = u32::try_from(id).ok()?;
                let name = ds
                    .class_signatures
                    .get(&id)
                    .and_then(|sig| sig.strip_prefix('L')?.strip_suffix(';'))
                    .map(str::to_string)
                    .or_else(|| {
                        crate::runtime::jvmti::breakpoint_class_name_for_vm(shared.vm_identity, id)
                            .map(|name| name.to_string())
                    });
                Some((crate::classloading::ClassId::new(raw), name))
            })
            .collect();
        crate::runtime::interpreter::note_breakpoint_classes_gained(shared, &classes);
    }
    // Wave 12 (lane L4): a loop already running in an OSR body of a method the
    // requests now concern leaves it at its next back-edge poll. Asked when
    // what the gates concern grew — an edge of the flags above, or a changed
    // per-method set (a breakpoint in one more method is not a flag edge);
    // nothing is paused unless an OSR body of this VM is running, and the
    // pause runs on its own thread (this caller holds the debug-state lock).
    // Lane L1, wave 12: it was asked on every publication while active, and
    // the server publishes after every command, so with an unconcerned OSR
    // loop running each command of a session cost a thread spawn and a
    // stop-the-world pause.
    let concerned_grew = !was_active || every_method_rose || interpret_all_rose || methods_changed;
    if active && concerned_grew {
        crate::runtime::interpreter::request_compiled_loop_exits(shared);
    }
}

/// Is the class `class_id`, which holds a breakpoint, one whose methods the
/// JIT may expand at a call site without a record the compile doors read
/// (interpreter round i1 wave 40, lane L1;
/// `interpreter::jit_may_expand_a_method_of_without_a_record`: a JDK class)?
/// The name comes from the session's class table or from the name a JVMTI
/// breakpoint recorded beside it, as for the scoped withdrawal: the class
/// manager is not taken under the debug-state lock. A class neither names is
/// answered `false`: every class a debugger can set a breakpoint in reached
/// it through a reply that recorded its signature
/// ([`populate_class_metadata`]), and `jvmti::set_breakpoint_for_vm` records
/// the name before it publishes.
fn breakpoint_class_is_expanded_without_a_record(
    shared: &crate::vm::SharedVm,
    ds: &DebugState,
    class_id: u64,
) -> bool {
    use crate::runtime::interpreter::jit_may_expand_a_method_of_without_a_record as expanded;
    if let Some(name) = ds
        .class_signatures
        .get(&class_id)
        .and_then(|sig| sig.strip_prefix('L')?.strip_suffix(';'))
    {
        return expanded(name);
    }
    crate::runtime::jvmti::breakpoint_class_name_for_vm(shared.vm_identity, class_id)
        .is_some_and(|name| expanded(&name))
}

/// The ids in `filter_classes` that name `class` or one of its supertypes
/// (superclasses and superinterfaces): the answers a `ClassOnly` modifier
/// needs about `class` (interpreter round i1 wave 24), as HotSpot's back end
/// evaluates it (`IsAssignableFrom`).
pub(crate) fn supertypes_among(
    cm: &crate::classloading::ClassManager,
    class: crate::classloading::ClassId,
    filter_classes: &[u64],
) -> Vec<u64> {
    filter_classes
        .iter()
        .copied()
        .filter(|&id| {
            u32::try_from(id)
                .is_ok_and(|raw| cm.is_subclass_of(class, crate::classloading::ClassId::new(raw)))
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Method entries (interpreter round i1 wave 24)
// ---------------------------------------------------------------------------

/// How many VMs of this process have a JDWP `MethodEntry` request in force:
/// the one-load pre-filter every interpreter frame push reads
/// (`interpreter::fire_method_entry_after_push`) before it records the new
/// frame for the suspend point ([`note_frame_entered`]). A union over VMs,
/// like JVMTI's listener mirrors: a push in a VM without a request records a
/// depth nobody consumes, which the next push at that depth discards.
static METHOD_ENTRY_REQUEST_VMS: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);

/// Is a JDWP `MethodEntry` request in force in any VM of this process?
#[cfg_attr(not(feature = "experimental-debug"), allow(dead_code))]
#[inline(always)]
pub(crate) fn method_entry_requests_anywhere() -> bool {
    METHOD_ENTRY_REQUEST_VMS.load(std::sync::atomic::Ordering::Relaxed) != 0
}

/// A VM's `MethodEntry` requests came into force (`armed`) or all went; only
/// [`publish_debugger_gates`] calls this, on the edges of
/// `DebuggerGates::method_entry`, so the count stays balanced per VM.
fn note_method_entry_requests(armed: bool) {
    use std::sync::atomic::Ordering;
    if armed {
        METHOD_ENTRY_REQUEST_VMS.fetch_add(1, Ordering::AcqRel);
    } else {
        let _ = METHOD_ENTRY_REQUEST_VMS.fetch_update(Ordering::AcqRel, Ordering::Acquire, |n| {
            Some(n.saturating_sub(1))
        });
    }
}

thread_local! {
    /// The depths of the interpreter frames this thread pushed while a
    /// `MethodEntry` request was in force whose first bytecode the suspend
    /// point has not reached yet, innermost last (wave 24). The suspend
    /// point reports a method entry at bytecode 0 of a frame only when its
    /// depth is here ([`take_frame_entry`]); a backward branch to bytecode 0
    /// (`while (true)` compiles to `0: goto 0`) is not an entry.
    static ENTERED_FRAMES: std::cell::RefCell<Vec<usize>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

/// The calling thread just pushed an interpreter frame at `depth` (its frame
/// count after the push) while a `MethodEntry` request is in force somewhere
/// ([`method_entry_requests_anywhere`]). A depth at or above `depth` still
/// recorded belongs to a frame that is gone.
#[cfg_attr(not(feature = "experimental-debug"), allow(dead_code))]
pub(crate) fn note_frame_entered(depth: usize) {
    ENTERED_FRAMES.with(|frames| {
        if let Ok(mut frames) = frames.try_borrow_mut() {
            while frames.last().is_some_and(|&d| d >= depth) {
                frames.pop();
            }
            frames.push(depth);
        }
    });
}

/// Is bytecode 0 of the frame at `depth` on the calling thread the method's
/// entry — pushed, and not yet reached by the suspend point? Consumes the
/// record (and any deeper, stale one).
#[cfg_attr(not(feature = "experimental-debug"), allow(dead_code))]
pub(crate) fn take_frame_entry(depth: usize) -> bool {
    ENTERED_FRAMES.with(|frames| {
        let Ok(mut frames) = frames.try_borrow_mut() else {
            return false;
        };
        while frames.last().is_some_and(|&d| d > depth) {
            frames.pop();
        }
        if frames.last() == Some(&depth) {
            frames.pop();
            true
        } else {
            false
        }
    })
}

// ---------------------------------------------------------------------------
// `ThreadReference.Stop` (interpreter round i1 wave 24)
// ---------------------------------------------------------------------------

/// Record `ThreadReference.Stop(thread, throwable)`: `tid` throws the object
/// id `throwable_id` names at its next interpreted bytecode
/// ([`take_pending_stop`]). The id's collection is disabled until then, so
/// the throwable survives a debugger that lets go of it; a stop already
/// pending for the thread is replaced (HotSpot installs the latest one).
/// `false` for an id naming no live object (`INVALID_OBJECT`). Called where
/// no collection can run (a mutator between polls, or the server quiesced),
/// with the debug-state lock held; the caller republishes the gates, which
/// arm every method while a stop is pending.
pub(crate) fn set_pending_stop(
    shared: &crate::vm::SharedVm,
    ds: &mut DebugState,
    tid: u64,
    throwable_id: u64,
) -> bool {
    if throwable_id == 0 || !set_collection_enabled(shared, ds, throwable_id, false) {
        return false;
    }
    // Pinned as well: a debugger that disposes of the id (JDI does once its
    // mirror is unreachable) must not free the handle before delivery.
    ds.objects.note_export(throwable_id, ObjectExport::Pinned);
    if let Some(previous) = ds.pending_stops.insert(tid, throwable_id) {
        release_pending_stop(shared, ds, previous);
    }
    true
}

/// Undo what [`set_pending_stop`] did to the id of a stop that is delivered
/// or replaced: collection enabled again, the pin dropped.
fn release_pending_stop(shared: &crate::vm::SharedVm, ds: &mut DebugState, throwable_id: u64) {
    let _ = set_collection_enabled(shared, ds, throwable_id, true);
    ds.objects.unpin(throwable_id);
}

/// The throwable a `ThreadReference.Stop` left for thread `tid`, if any: the
/// interpreter's suspend point throws it at the bytecode it is about to run,
/// as HotSpot throws an asynchronous exception at the thread's current
/// bytecode. Its id's collection is enabled again (the object stays where it
/// is until the caller's next safepoint, and the caller throws it before
/// one). `None` — one lock and one map probe — in the common case.
#[cfg_attr(not(feature = "experimental-debug"), allow(dead_code))]
pub(crate) fn take_pending_stop(
    shared: &crate::vm::SharedVm,
    tid: u64,
) -> Option<crate::types::ObjectRef> {
    let mut ds = shared.debug.debug_state.lock();
    let id = ds.pending_stops.remove(&tid)?;
    let throwable = object_for_id(shared, &ds, id);
    release_pending_stop(shared, &mut ds, id);
    publish_debugger_gates(shared, &ds);
    release_disposed_objects(shared, &mut ds);
    throwable
}

/// The exception the interpreter's unwinder is about to route for thread
/// `tid`, or in its place the throwable a `ThreadReference.Stop` left for the
/// thread (interpreter round i1 wave 25). HotSpot installs a stop as an
/// asynchronous exception that REPLACES a pending one
/// (`JavaThread::handle_async_exception` → `set_pending_exception`); the
/// case that matters is the `InterruptedException` of a blocking call the
/// stop woke ([`inspect`]'s `stop_thread`), which must not reach the
/// program's handlers: the stop must, and it must be matched against the
/// handlers the in-flight exception would have been, not thrown later at
/// the first bytecode of one of them (which lies outside its own protected
/// range). Not on a thread whose debugger hooks are suppressed (heap work the
/// debugger runs on a parked thread). The unwinder asks only while
/// `DebuggerGates::stop_pending` is up.
pub(crate) fn exception_or_pending_stop(
    shared: &crate::vm::SharedVm,
    tid: u64,
    exc: crate::types::ObjectRef,
) -> crate::types::ObjectRef {
    take_stop_for_blocking_call(shared, tid).unwrap_or(exc)
}

/// The pending `ThreadReference.Stop` of thread `tid`, taken, for the code
/// that is about to throw in its place (interpreter round i1 wave 25): the
/// unwinder ([`exception_or_pending_stop`]), and a blocking native woken by
/// the stop's interrupt, which throws it instead of its
/// `InterruptedException` (`NativeContext::take_debugger_stop`: the sleep
/// natives, and `exceptions::throw_runtime_error` for an interrupted
/// `Object.wait` / `Thread.join`), so the stop is what the program's
/// handlers see whether the frame that catches runs interpreted or compiled.
/// `None` on a thread whose debugger hooks are suppressed (heap work the
/// debugger runs on a parked thread) or with no stop pending.
pub(crate) fn take_stop_for_blocking_call(
    shared: &crate::vm::SharedVm,
    tid: u64,
) -> Option<crate::types::ObjectRef> {
    if debugger_hooks_suppressed() {
        return None;
    }
    take_pending_stop(shared, tid)
}

// ---------------------------------------------------------------------------
// `VM_DEATH` (interpreter round i1 wave 24)
// ---------------------------------------------------------------------------

/// How long [`report_vm_death`] waits for the server to write the event.
const VM_DEATH_WAIT: std::time::Duration = std::time::Duration::from_secs(2);

/// The VM is ending (interpreter round i1 wave 24): report `VM_DEATH` to an
/// attached debugger and wait, at most [`VM_DEATH_WAIT`], until the JDWP
/// server has written it. HotSpot's back end sends the event — one per
/// `VMDeath` request, then request id 0, always, whatever the requests say
/// (wave 26: that order, measured; request 0 went first), in one set with
/// the strongest of their policies — before the transport closes,
/// so JDI delivers a `VMDeathEvent` before the `VMDisconnectEvent`
/// (jdb: "The application exited"). It was never sent: the debugger saw the
/// connection drop.
///
/// A `VMDeath` request's `SUSPEND_ALL` (interpreter round i1 wave 26, lane
/// L1) suspends the VM before the set is sent and holds the dying thread
/// until the debugger resumes it (or detaches), so the debugger can inspect
/// the VM before it dies, as on HotSpot (`VMDeathRequest`'s one use; JDI
/// creates one only since `canRequestVMDeathEvent` answers true). The hold
/// runs in a blocking region: a debugger command served meanwhile may
/// collect, and a GC-blocked thread is one the collector does not wait for.
/// `SUSPEND_EVENT_THREAD` suspends and holds the dying thread alone
/// ([`vm_death_event_set`] has HotSpot's measured behaviour).
///
/// Called by the launcher on both of its exit arms (`Java main` returned,
/// and `System.exit` through its pre-exit hook), after the shutdown hooks,
/// where HotSpot posts JVMTI `VMDeath` (`before_exit`). A no-op without an
/// attached debugger, and after the first call.
pub fn report_vm_death(shared: &crate::vm::SharedVm) {
    if !shared.debug.debugger_gates.session_attached() {
        return;
    }
    // The dying thread, when this VM knows the calling OS thread: it is held
    // by its own suspend count, else by the VM-wide one (an id no thread has).
    let dying = crate::native::jni::current_jvm_thread_of(shared);
    // SAFETY: `current_jvm_thread_of` answers the live `JvmThread` running
    // this call on this OS thread; `thread_id` is an immutable `Copy` field.
    let dying_tid = dying.map_or(u64::MAX, |thread| unsafe { (*thread).thread_id.0 });
    let (set, held) = {
        let mut ds = shared.debug.debug_state.lock();
        if ds.vm_death_sent {
            return;
        }
        let (set, held) = vm_death_event_set(&mut ds, dying.map(|_| dying_tid));
        if held {
            publish_debugger_gates(shared, &ds);
        }
        (set, held)
    };
    send_event_set(shared, set);
    if held {
        hold_for_vm_death(shared, dying, dying_tid);
        return;
    }
    let until = std::time::Instant::now() + VM_DEATH_WAIT;
    while std::time::Instant::now() < until && shared.debug.debugger_gates.session_attached() {
        if shared.debug.debug_state.lock().vm_death_sent {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(2));
    }
}

/// The `VM_DEATH` event set [`report_vm_death`] sends, with its suspension
/// applied to `ds` (wave 26): one event per `VMDeath` request, then the
/// unsolicited one (request 0), all with the strongest of the requests'
/// policies — the order HotSpot's back end sends them in (measured with JDI:
/// the `EventSet` iterates the `VMDeathRequest`'s event before the one with
/// no request). `SUSPEND_ALL` suspends the VM; `SUSPEND_EVENT_THREAD`
/// suspends `dying`, the thread reporting the death, when it is known — the
/// event names no thread, but HotSpot's back end suspends the thread that
/// posts it (measured: JDI's `EventSet.resume()` then throws "Inconsistent
/// suspend policy" and the VM stays held until a `VirtualMachine.Resume` or
/// a detach). Answers the set and whether a suspension was applied (the
/// caller then holds the dying thread).
fn vm_death_event_set(ds: &mut DebugState, dying: Option<u64>) -> (Vec<DebugEvent>, bool) {
    let hits = ds
        .events
        .match_thread_events(events::EventKind::VMDeath, 0);
    let policy = strongest_suspend_policy(hits.iter().map(|&(_, p)| p));
    let held = match (policy, dying) {
        (events::SuspendPolicy::All, _) => {
            ds.suspend_all();
            true
        }
        (events::SuspendPolicy::EventThread, Some(tid)) => {
            ds.suspend_thread(tid);
            true
        }
        _ => false,
    };
    let set = hits
        .into_iter()
        .map(|(request_id, _)| request_id)
        .chain(std::iter::once(0u32))
        .map(|request_id| DebugEvent {
            kind: events::EventKind::VMDeath,
            request_id,
            suspend_policy: policy,
            thread_id: 0,
            class_id: 0,
            method_id: 0,
            offset: 0,
            extra: Vec::new(),
            set_follows: false,
        })
        .collect();
    (set, held)
}

/// [`report_vm_death`]'s hold for a suspending `VMDeath` request (wave
/// 26): wait, GC-blocked when `dying` is this VM's thread, until thread
/// `dying_tid` is no longer suspended (the debugger resumed the set, or
/// detached, which resumes everything) or the session ends.
fn hold_for_vm_death(
    shared: &crate::vm::SharedVm,
    dying: Option<*mut crate::threading::jvm_thread::JvmThread>,
    dying_tid: u64,
) {
    let wait = || {
        while shared.debug.debugger_gates.session_attached()
            && shared.debug.debug_state.lock().is_thread_suspended(dying_tid)
        {
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
    };
    match dying {
        Some(thread) => {
            // SAFETY: `current_jvm_thread_of` answered this OS thread's own
            // live `JvmThread`, which only this thread uses while it runs
            // this call (the launcher's exit arm, or the `System.exit`
            // native through its pre-exit hook, whose own context it
            // stands in for until the region ends).
            let mut ctx = crate::vm::NativeContextImpl {
                shared,
                thread: unsafe { &mut *thread },
            };
            cratonvm_native_api::NativeThreadAccess::begin_blocking_region(&mut ctx);
            wait();
            cratonvm_native_api::NativeThreadAccess::end_blocking_region(&mut ctx);
        }
        // Not a thread of this VM: nothing waits for it.
        None => wait(),
    }
}

// ---------------------------------------------------------------------------
// Object ids (wave 7): the VM half of `ids::ObjectTable`
// ---------------------------------------------------------------------------

/// The VM's collection count, which [`ids::ObjectTable`]'s address index is
/// keyed by: `GcBarrier::gc_generation`, bumped once at the end of every
/// pause, after the pause's remap of the JNI global references.
fn gc_epoch(shared: &crate::vm::SharedVm) -> u64 {
    shared
        .mem
        .gc_barrier
        .gc_generation
        .load(std::sync::atomic::Ordering::Acquire)
}

/// The JDWP object id of `obj`, minting one on first export. The same object
/// always gets the same id while the debugger holds it, however far a
/// collection moved it; ids never collide with thread ids (see
/// [`ids::HEAP_OBJECT_ID_BASE`]).
///
/// The id is backed by a JNI WEAK global reference (wave 8; a strong one
/// while `DisableCollection` is in force, [`set_collection_enabled`]): every
/// collection remaps it or clears it (`jni::sweep_weak_global_refs` and the
/// concurrent-cycle sweeps), so no new side table exists, and an id no
/// longer keeps its object alive, as in HotSpot. Lock order: debug state,
/// then the JNI global-reference table.
///
/// Call only where no collection can run concurrently: on a mutator between
/// two safepoint polls (a parked thread, the suspend point), or on the
/// server inside `GcBarrier::run_if_no_stw_requested`.
pub(crate) fn export_object(
    shared: &crate::vm::SharedVm,
    ds: &mut DebugState,
    obj: crate::types::ObjectRef,
    how: ObjectExport,
) -> u64 {
    let epoch = gc_epoch(shared);
    if ds.objects.needs_rekey(epoch) {
        let globals = shared.natives.jni_global_refs.lock();
        ds.objects.rekey(epoch, |handle| {
            // Cast: a referent's address is the index key.
            globals.resolve(handle).map(|r| r.as_ptr() as usize)
        });
    }
    let addr = obj.as_ptr() as usize; // Cast: the index key
    if let Some((id, handle)) = ds.objects.candidate_for_addr(addr) {
        // A weak referent can die — and its address go to a new object —
        // between two rekeys: a concurrent cycle clears weak handles outside
        // a pause. The handle, not the index, says whose address it is.
        let current = shared.natives.jni_global_refs.lock().resolve(handle);
        if current.is_some_and(|r| r.as_ptr() as usize == addr) {
            ds.objects.note_export(id, how);
            return id;
        }
        ds.objects.forget_addr(id);
    }
    let handle = shared.natives.jni_global_refs.lock().add_weak(obj);
    let id = ds.ids.fresh_object_id().0;
    ds.objects.insert(id, handle, addr, how);
    id
}

/// A frame local as a suspended thread's published snapshot records it
/// (`interpreter::publish_debugger_frames`, `StackFrame.SetValues`): a
/// reference as its object id, pinned while the snapshot stands (the
/// snapshot's withdrawal unpins it). Same calling rule as [`export_object`].
pub(crate) fn snapshot_local(
    shared: &crate::vm::SharedVm,
    ds: &mut DebugState,
    v: crate::types::Value,
) -> LocalValue {
    use crate::types::Value;
    match v {
        Value::Int(i) => LocalValue::Int(i),
        Value::Long(l) => LocalValue::Long(l),
        Value::Float(f) => LocalValue::Float(f),
        Value::Double(d) => LocalValue::Double(d),
        Value::Object(Some(r)) => {
            LocalValue::ObjectRef(export_object(shared, ds, r, ObjectExport::Pinned))
        }
        // A slot never written, or a `long`'s / `double`'s second slot, is no
        // reference (interpreter round i1 wave 42): HotSpot types it as an
        // `int`, so asking it for an object is `TYPE_MISMATCH`
        // (`commands::local_read_refusal`). It read as a null reference.
        Value::Uninitialized => LocalValue::Int(0),
        _ => LocalValue::ObjectRef(0),
    }
}

/// Local `slot` of `frame` as the debugger reads it (interpreter round i1
/// wave 15). `Frame::get_local` decodes a slot context-free except for a
/// `double`; a `long` is stored as its verbatim bits, so it came back as a
/// `double` with those bits (or, for a few bit patterns, as another type),
/// and `StackFrame.GetValues` answered 0 for every `long` local (the snapshot
/// held no `Long`), while `SetValues` of a `long` local of a class compiled
/// without `-g` was refused as a type mismatch. The slot's kind mark says
/// which it is.
pub(crate) fn debugger_local(
    frame: &crate::runtime::frame::Frame,
    slot: u16,
) -> crate::types::Value {
    let i = usize::from(slot);
    if i < frame.locals_len() && frame.get_local_tag(i) == crate::types::VTAG_LONG {
        // Cast: the long's verbatim bits.
        return crate::types::Value::Long(frame.get_local_raw(i) as i64);
    }
    frame.get_local(slot)
}

/// The object a JDWP object id names now, wherever a collection moved it;
/// `None` for an id this session never handed out, has disposed of, or whose
/// object was collected (JDWP `INVALID_OBJECT`).
///
/// Resolving a weak handle is a keep-alive, as `jni::jobject_to_obj` makes
/// it: a concurrent mark may not have reached the referent (a weak handle is
/// not a root), and whatever the debugger does with it next — store it into a
/// field, pass it to an invocation — must not leave it unmarked. Same calling
/// rule as [`export_object`].
pub(crate) fn object_for_id(
    shared: &crate::vm::SharedVm,
    ds: &DebugState,
    id: u64,
) -> Option<crate::types::ObjectRef> {
    let Some(handle) = ds.objects.handle_of(id) else {
        return thread_object_for_id(shared, ds, id);
    };
    let (obj, weak) = {
        let globals = shared.natives.jni_global_refs.lock();
        let obj = globals.resolve(handle);
        (obj, obj.is_some() && globals.is_weak(handle))
    };
    if weak {
        if let Some(o) = obj {
            shared
                .mem
                .heap
                .satb_barrier(crate::types::Value::Object(Some(o)));
        }
    }
    obj
}

/// The `java.lang.Thread` of the live thread a JDWP THREAD id names, when a
/// command takes it as an object id (interpreter round i1 wave 23).
///
/// In HotSpot a `threadID` IS the thread object's `objectID`: JDI's
/// `ThreadReference` is an `ObjectReference`, so `referenceType()`,
/// `toString()`, `getValue(field)` or an invocation on a thread send the
/// `ObjectReference` commands with the thread id — jdb's `threads` prints
/// every thread through `referenceType()` (`Env.description`). This server's
/// thread ids are the VM's own thread ids, which the object table never
/// hands out (its ids start at [`ids::HEAP_OBJECT_ID_BASE`], so the two
/// cannot collide), and those commands answered `INVALID_OBJECT`, which JDI
/// turns into `ObjectCollectedException`: jdb's `threads` failed. The thread
/// must be one the server lists (`thread_names`); its object comes from the
/// thread registry, current wherever a collection moved it (the callers run
/// with no collection possible, as for any id).
///
/// Lock order: the debug state, then the registry's thread map, read-only
/// and innermost (no registry method calls into the debugger).
///
/// Interpreter round i1 wave 24: a thread the registry knows counts even
/// before the server's thread table has caught up with it (that table is
/// refreshed only before the commands that list or target threads,
/// [`refreshes_thread_names`]): a thread started after the last refresh — a
/// `ThreadStart` event's, or one a thread value names (`t`,
/// `inspect::thread_value`) — answered `INVALID_OBJECT`, which JDI turns
/// into `ObjectCollectedException`, for its `referenceType()`. The heap
/// service, the back end's own thread, stays hidden.
fn thread_object_for_id(
    shared: &crate::vm::SharedVm,
    ds: &DebugState,
    id: u64,
) -> Option<crate::types::ObjectRef> {
    // The id is a wire id (wave 25): the main thread's is
    // `ids::MAIN_THREAD_WIRE_ID`, and 0 is null.
    let id = ids::thread_from_wire(id);
    if id >= ids::HEAP_OBJECT_ID_BASE || ds.heap_service_thread() == Some(id) {
        return None;
    }
    shared
        .threads
        .thread_registry
        .java_thread_obj(crate::threading::ThreadId(id))
}

/// Is the object behind a live id gone? `None` for an id the table does not
/// know (JDWP `INVALID_OBJECT`). `ObjectReference.IsCollected`. A live
/// thread's id (wave 23, [`thread_object_for_id`]) is never collected.
pub(crate) fn object_collected(
    shared: &crate::vm::SharedVm,
    ds: &DebugState,
    id: u64,
) -> Option<bool> {
    let Some(handle) = ds.objects.handle_of(id) else {
        return thread_object_for_id(shared, ds, id).map(|_| false);
    };
    Some(
        shared
            .natives
            .jni_global_refs
            .lock()
            .resolve(handle)
            .is_none(),
    )
}

/// `ObjectReference.DisableCollection` (`enable == false`) /
/// `EnableCollection` (`true`), which JDWP nests: the first disable swaps the
/// id's weak handle for a strong one, the last enable swaps it back. `false`
/// for an unknown id or one whose object is already gone (`INVALID_OBJECT`).
/// The replaced handle is freed by [`release_disposed_objects`].
pub(crate) fn set_collection_enabled(
    shared: &crate::vm::SharedVm,
    ds: &mut DebugState,
    id: u64,
    enable: bool,
) -> bool {
    let Some(handle) = ds.objects.handle_of(id) else {
        // A live thread's id (wave 23, [`thread_object_for_id`]): its
        // object is reachable while the thread runs, so there is nothing to
        // pin, and the command succeeds as HotSpot's does.
        return thread_object_for_id(shared, ds, id).is_some();
    };
    let Some(obj) = shared.natives.jni_global_refs.lock().resolve(handle) else {
        // Enabling collection of a collected object is harmless (HotSpot
        // accepts it); disabling it is too late.
        if enable {
            let _ = ds.objects.enable_collection(id);
        }
        return enable;
    };
    let count = if enable {
        ds.objects.enable_collection(id)
    } else {
        ds.objects.disable_collection(id)
    };
    let swap_to_strong = !enable && count == Some(1);
    let is_weak = shared.natives.jni_global_refs.lock().is_weak(handle);
    let swap_to_weak = enable && count == Some(0) && !is_weak;
    if swap_to_strong || swap_to_weak {
        let fresh = {
            let mut globals = shared.natives.jni_global_refs.lock();
            if swap_to_strong {
                globals.add(obj)
            } else {
                globals.add_weak(obj)
            }
        };
        ds.objects.replace_handle(id, fresh);
    }
    true
}

/// Free the JNI global references behind the object ids that are gone
/// (disposed of, unpinned, or the session ended).
pub(crate) fn release_disposed_objects(shared: &crate::vm::SharedVm, ds: &mut DebugState) {
    let handles = ds.objects.take_released();
    if handles.is_empty() {
        return;
    }
    let mut globals = shared.natives.jni_global_refs.lock();
    for handle in handles {
        let _ = globals.remove(handle);
    }
}

thread_local! {
    /// Set on the thread running [`run_jdwp_server`]; see
    /// [`debugger_hooks_suppressed`].
    static JDWP_SERVER_THREAD: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    /// How many heap-work [`ParkedTask`]s the calling thread is running
    /// (wave 8; wave 9: invocations no longer count); see
    /// [`debugger_hooks_suppressed`].
    static DEBUGGER_WORK_DEPTH: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
}

/// Must the interpreter's JDWP suspend point leave the calling thread alone?
/// True on the JDWP server thread, and on a parked thread while it runs heap
/// work for the debugger (a command that reads or writes the heap, which the
/// server waits for): stopping there would wait on the server, which is
/// waiting for that very work.
///
/// Not while a parked thread runs a debugger INVOCATION (wave 9): the server
/// answers an invocation when it returns and serves commands meanwhile
/// (`run_jdwp_server`), so a breakpoint or step inside the invoked method is
/// reported and parks the thread again, above the invocation's frames, as in
/// HotSpot. Until wave 9 those events were dropped.
pub(crate) fn debugger_hooks_suppressed() -> bool {
    JDWP_SERVER_THREAD.with(|f| f.get()) || DEBUGGER_WORK_DEPTH.with(|d| d.get()) > 0
}

// ---------------------------------------------------------------------------
// Work the JDWP server hands to a parked thread (wave 8)
// ---------------------------------------------------------------------------

/// Work the JDWP server runs on a thread parked at an interpreter suspend
/// point ([`run_on_parked_thread`], [`run_invocation_on_parked_thread`]). It
/// answers through a channel it captured, when the park loop sends its
/// [`ParkedReply`].
pub(crate) struct ParkedTask {
    run: ParkedRun,
    /// A debugger invocation (wave 9): runs Java whose events are reported,
    /// and may park the thread again at a stop inside it. Heap work keeps the
    /// suspend point suppressed ([`debugger_hooks_suppressed`]).
    invocation: bool,
}

/// The thread a [`ParkedTask`] runs on.
type ParkedThread = crate::threading::jvm_thread::JvmThread;

/// The work of a [`ParkedTask`], run on the parked thread.
type ParkedRun = Box<dyn FnOnce(&crate::vm::SharedVm, &mut ParkedThread) -> ParkedReply + Send>;

/// The answer of a [`ParkedTask`], held back until the parked thread has
/// settled its park (wave 9): its park level is idle again and, after a stop
/// inside an invocation, its frame snapshot is republished
/// (`interpreter::park_for_debugger`). Sent any earlier, the server could
/// reply to the debugger and hand the thread its next command while the
/// thread still looked busy (`ALREADY_INVOKING` / `THREAD_NOT_SUSPENDED`
/// for a thread about to be idle) or published no frames at all. Dropping it
/// unsent fails the waiting call (`ParkedError::Lost`).
pub(crate) struct ParkedReply(Box<dyn FnOnce() + Send>);

impl ParkedReply {
    /// Hand the answer to the waiting call.
    pub(crate) fn send(self) {
        (self.0)();
    }
}

/// Why [`run_on_parked_thread`] could not run its work.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ParkedError {
    /// The thread named (or, for any thread, every thread) is not parked at
    /// an interpreter suspend point: running, blocked in native code or
    /// compiled code, or not a thread at all.
    NotParked,
    /// The thread named is parked but busy: it runs an invocation (and has
    /// not stopped inside it), or work is already queued for it (wave 9;
    /// JDWP `ALREADY_INVOKING` for an invocation).
    Busy,
    /// The thread left without answering (its park ended, or the work
    /// panicked).
    Lost,
}

/// Run `work` on thread `tid` — or, for `None`, on the lowest-numbered
/// parked thread that is idle — while it is parked at an interpreter suspend
/// point, and wait for its answer. The JDWP server's way to touch the heap
/// (interpreter round i1 wave 8); invocations go through
/// [`run_invocation_on_parked_thread`] (wave 9).
///
/// Why a parked thread: it is a registered mutator. The collector counts it,
/// waits for it at a safepoint and scans and remaps its frames, and no
/// collection can run while it executes Rust code between two polls. The
/// server thread is none of that: until wave 8 it ran debugger invocations
/// itself, on an unregistered `JvmThread` borrowing the suspended thread's
/// id, so a collection triggered by another thread moved the objects in the
/// ephemeral frames under the running Java
/// (docs/internal/fixed-bugs/interpreter-L1-jdwp-invocations-run-java-on-an-unregistered-thread-FIXED-20260924.md).
///
/// Called with no debug-state lock held, by the thread running
/// [`run_jdwp_server`], an invocation's waiter thread, or a test standing in
/// for them; it blocks until the work is done. The work runs with the
/// suspend point suppressed and must not run Java.
pub(crate) fn run_on_parked_thread<R, F>(
    shared: &crate::vm::SharedVm,
    tid: Option<u64>,
    work: F,
) -> Result<R, ParkedError>
where
    R: Send + 'static,
    F: FnOnce(&crate::vm::SharedVm, &mut crate::threading::jvm_thread::JvmThread) -> R
        + Send
        + 'static,
{
    queue_on_parked_thread(shared, tid, None, work)?
        .recv()
        .map_err(|_| ParkedError::Lost)
}

/// Run a debugger invocation's `work` on parked thread `tid` and wait for
/// its answer (wave 9). Around the work, on that thread and under the
/// debug-state lock, the JDWP invocation protocol applies
/// ([`DebugState::release_for_invocation`] /
/// [`DebugState::resuspend_after_invocation`]): without
/// `INVOKE_SINGLE_THREADED` every suspended thread runs while the method
/// does (until wave 9 none did, so a method that needed another suspended
/// thread's monitor or hand-off hung the session), and afterwards every
/// thread — or, single-threaded, `tid` alone — is suspended again. The
/// re-suspension happens before the answer is sent, so the thread never
/// sees itself resumed between the two and leaves its park.
///
/// The work runs with the suspend point live: a stop inside it reports its
/// event and parks the thread again (see [`debugger_hooks_suppressed`]). So
/// the caller must not be the JDWP server loop itself, which has to keep
/// serving commands until the answer comes (`run_jdwp_server` runs each
/// invocation on a waiter thread).
pub(crate) fn run_invocation_on_parked_thread<R, F>(
    shared: &crate::vm::SharedVm,
    tid: u64,
    single_threaded: bool,
    work: F,
) -> Result<R, ParkedError>
where
    R: Send + 'static,
    F: FnOnce(&crate::vm::SharedVm, &mut crate::threading::jvm_thread::JvmThread) -> R
        + Send
        + 'static,
{
    queue_on_parked_thread(shared, Some(tid), Some(single_threaded), work)?
        .recv()
        .map_err(|_| ParkedError::Lost)
}

/// Queue `work` for a parked thread (see [`run_on_parked_thread`]);
/// `invocation` is `Some(single_threaded)` for a debugger invocation
/// ([`run_invocation_on_parked_thread`]). Answers the channel the work's
/// result arrives on.
fn queue_on_parked_thread<R, F>(
    shared: &crate::vm::SharedVm,
    tid: Option<u64>,
    invocation: Option<bool>,
    work: F,
) -> Result<std::sync::mpsc::Receiver<R>, ParkedError>
where
    R: Send + 'static,
    F: FnOnce(&crate::vm::SharedVm, &mut crate::threading::jvm_thread::JvmThread) -> R
        + Send
        + 'static,
{
    let (tx, rx) = std::sync::mpsc::sync_channel::<R>(1);
    let mut ds = shared.debug.debug_state.lock();
    let target = match tid {
        Some(t) if !ds.parked_threads.contains_key(&t) => return Err(ParkedError::NotParked),
        // Wave 44: an invocation runs only on a thread an event suspended
        // (`DebugState::event_parks`); the refusal is `INVALID_THREAD`, the
        // not-parked answer (`SharedVmBridge::not_parked`).
        Some(t) if invocation.is_some() && !ds.parked_by_event(t) => {
            return Err(ParkedError::NotParked)
        }
        // Running an invocation it has not stopped in, or work already
        // queued (a second invocation on the same thread: JDWP
        // `ALREADY_INVOKING`).
        Some(t) if !ds.can_take_work(t) => return Err(ParkedError::Busy),
        Some(t) => t,
        None => {
            let idle = ds
                .parked_threads
                .keys()
                .copied()
                .filter(|&t| ds.can_take_work(t))
                .min();
            match idle {
                Some(t) => t,
                // No thread can take it (wave 10): heap work (a `None` target
                // is never an invocation) goes to the heap service, a
                // registered mutator of its own, when the session has one.
                None if invocation.is_none() => {
                    let Some((service, _)) = ds.heap_service.as_ref() else {
                        return Err(ParkedError::NotParked);
                    };
                    let job: crate::native::jni::AttachedServiceJob = Box::new(
                        move |shared: &crate::vm::SharedVm, thread: &mut ParkedThread| {
                            let _depth = HeapWorkDepth::enter();
                            let _ = tx.send(work(shared, thread));
                        },
                    );
                    return service
                        .send(job)
                        .map(|()| rx)
                        .map_err(|_| ParkedError::NotParked);
                }
                None => return Err(ParkedError::NotParked),
            }
        }
    };
    let reply = move |answer: R| {
        ParkedReply(Box::new(move || {
            let _ = tx.send(answer);
        }))
    };
    let run: ParkedRun = match invocation {
        None => Box::new(
            move |shared: &crate::vm::SharedVm, thread: &mut ParkedThread| {
                reply(work(shared, thread))
            },
        ),
        Some(single_threaded) => {
            // Captured now, under the lock the task is queued under: a detach
            // after this point ends the session the invocation belongs to.
            let session = ds.session;
            Box::new(
                move |shared: &crate::vm::SharedVm, thread: &mut ParkedThread| {
                    {
                        let mut ds = shared.debug.debug_state.lock();
                        if ds.session == session {
                            ds.release_for_invocation(target, single_threaded);
                            publish_debugger_gates(shared, &ds);
                        }
                    }
                    let answer = work(shared, thread);
                    {
                        let mut ds = shared.debug.debug_state.lock();
                        if ds.session == session {
                            ds.resuspend_after_invocation(target, single_threaded);
                            publish_debugger_gates(shared, &ds);
                        }
                    }
                    reply(answer)
                },
            )
        }
    };
    ds.parked_tasks.insert(
        target,
        ParkedTask {
            run,
            invocation: invocation.is_some(),
        },
    );
    Ok(rx)
}

/// Run a [`ParkedTask`] on the calling (parked) thread; heap work runs with
/// the suspend point suppressed for its duration
/// ([`debugger_hooks_suppressed`]), an invocation does not (wave 9).
/// `interpreter::park_for_debugger`'s poll loop calls this, marks its park
/// level idle ([`DebugState::note_task_done`]) and then sends the reply.
pub(crate) fn run_parked_task(
    shared: &crate::vm::SharedVm,
    thread: &mut crate::threading::jvm_thread::JvmThread,
    task: ParkedTask,
) -> ParkedReply {
    if task.invocation {
        return (task.run)(shared, thread);
    }
    let _depth = HeapWorkDepth::enter();
    (task.run)(shared, thread)
}

/// Heap work in progress on the calling thread: the debugger's suspend point
/// stays suppressed while one lives ([`debugger_hooks_suppressed`]).
struct HeapWorkDepth;

impl HeapWorkDepth {
    fn enter() -> Self {
        DEBUGGER_WORK_DEPTH.with(|d| d.set(d.get() + 1));
        HeapWorkDepth
    }
}

impl Drop for HeapWorkDepth {
    fn drop(&mut self) {
        DEBUGGER_WORK_DEPTH.with(|d| d.set(d.get().saturating_sub(1)));
    }
}

/// Start the JDWP heap service for `shared` (interpreter round i1 wave 10):
/// a daemon thread attached to the VM (`jni::run_attached_service`) that runs
/// heap work when no thread is parked at a suspend point — `CreateString`
/// and `ArrayType.NewInstance`, which HotSpot serves with no thread
/// suspended, and the other heap commands, which then run on a registered
/// mutator rather than quiesced on the server thread. It never runs Java,
/// never parks, is not a thread the debugger lists (`populate_thread_metadata`)
/// and takes no invocation. Idempotent; `false` if the thread could not be
/// started. Started at attach by `run_jdwp_server`, stopped at detach
/// ([`stop_heap_service`]).
pub(crate) fn start_heap_service(shared: &Arc<crate::vm::SharedVm>) -> bool {
    if shared.debug.debug_state.lock().heap_service.is_some() {
        return true;
    }
    let (jobs_tx, jobs_rx) = std::sync::mpsc::channel::<crate::native::jni::AttachedServiceJob>();
    let (started_tx, started_rx) = std::sync::mpsc::sync_channel::<u64>(1);
    let owned = Arc::clone(shared);
    let spawned = std::thread::Builder::new()
        .name("cratonvm-jdwp-heap".to_string())
        .spawn(move || {
            crate::native::jni::run_attached_service(
                owned,
                "cratonvm-jdwp-heap",
                started_tx,
                jobs_rx,
            )
        });
    if spawned.is_err() {
        return false;
    }
    let Ok(tid) = started_rx.recv() else {
        return false;
    };
    shared.debug.debug_state.lock().heap_service = Some((jobs_tx, tid));
    true
}

/// Stop the heap service: the thread detaches once the work queued before
/// this has run.
pub(crate) fn stop_heap_service(shared: &crate::vm::SharedVm) {
    let service = shared.debug.debug_state.lock().heap_service.take();
    drop(service);
}

/// Build JDWP location extra bytes: tag(1) + classID(8) + methodID(8) + index(8).
/// `class_id` is the VM's; it goes out in its wire form (wave 25,
/// `ids::class_to_wire`) — except in the null location `(0, 0, 0)` an
/// exception event writes for an uncaught exception, which stays all zeros
/// (a method id is never 0 for a real method: `jdwp_method_id`).
pub fn build_location_extra(class_id: u64, method_id: u64, offset: u64) -> Vec<u8> {
    let class_wire = if method_id == 0 {
        class_id
    } else {
        ids::class_to_wire(class_id)
    };
    let mut extra = Vec::with_capacity(25);
    extra.push(1u8); // TypeTag: CLASS
    extra.extend_from_slice(&class_wire.to_be_bytes());
    extra.extend_from_slice(&method_id.to_be_bytes());
    extra.extend_from_slice(&offset.to_be_bytes());
    extra
}

/// Send a thread event (ThreadStart or ThreadDeath) to the debugger if
/// there is an active event channel: one event per matching request
/// ([`events::EventManager::match_thread_events`]).
///
/// Wave 9: each request's suspend policy is applied before the event is
/// queued, as the interpreter's suspend point does for a breakpoint —
/// `SUSPEND_EVENT_THREAD` counts one suspension of the thread, `SUSPEND_ALL`
/// one of the VM — so a started thread parks at its first gated bytecode and
/// the debugger's resume of the event set balances a real suspension. The
/// policy used to be reported and not applied, and only the first matching
/// request reported. Wave 22: the matching requests' events are one event
/// set, suspended once with its strongest policy ([`send_event_set`]).
pub fn send_thread_event(shared: &crate::vm::SharedVm, kind: events::EventKind, thread_id: u64) {
    // Wave 25: a `PlatformThreadsOnly` modifier asks this (before the
    // debug-state lock: the table has a lock of its own).
    let is_virtual = shared.threads.virtual_thread_manager.is_virtual(thread_id);
    send_thread_event_of(shared, kind, thread_id, is_virtual);
}

/// [`send_thread_event`] for a thread whose caller knows whether it is a
/// virtual thread (interpreter round i1 wave 25: the VM's `Thread.start`
/// reads it from the `Thread` object's class, `BaseVirtualThread`, before the
/// virtual-thread table registers the thread — so `PlatformThreadsOnly`, which
/// asked that table, let a virtual thread's `ThreadStart` through,
/// `L1W25JdiVersion`). A virtual thread is remembered from its start for its
/// death and for `ThreadReference.IsVirtual`.
pub fn send_thread_event_of(
    shared: &crate::vm::SharedVm,
    kind: events::EventKind,
    thread_id: u64,
    is_virtual: bool,
) {
    let hits = {
        let mut ds = shared.debug.debug_state.lock();
        let is_virtual = is_virtual || ds.virtual_threads.contains(&thread_id);
        // Wave 45 (lane L1): a thread that ends leaves no C JVMTI suspension
        // behind.
        let forgot = matches!(kind, events::EventKind::ThreadDeath)
            && ds.forget_jvmti_suspension(thread_id);
        match kind {
            events::EventKind::ThreadStart if is_virtual => {
                ds.virtual_threads.insert(thread_id);
            }
            events::EventKind::ThreadDeath => {
                ds.virtual_threads.remove(&thread_id);
            }
            _ => {}
        }
        let hits = ds.events.match_thread_events_of(kind, thread_id, is_virtual);
        // One event set, suspended once with its strongest policy (wave 22).
        let policy = strongest_suspend_policy(hits.iter().map(|&(_, p)| p));
        let suspended = ds.apply_event_set_policy(policy, thread_id);
        // Wave 46 (lane L1): or a `Count` this match spent.
        let spent = take_spent_requests(&mut ds);
        if suspended || forgot || spent {
            publish_debugger_gates(shared, &ds);
        }
        hits
    };
    let set = hits
        .into_iter()
        .map(|(request_id, suspend_policy)| DebugEvent {
            kind,
            request_id,
            suspend_policy,
            thread_id,
            class_id: 0,
            method_id: 0,
            offset: 0,
            extra: Vec::new(),
            set_follows: false,
        })
        .collect();
    send_event_set(shared, set);
}

/// The JDWP `methodID` this server hands out for the method `name` +
/// `descriptor`: a 64-bit FNV-1a hash of `name`, a `0` separator byte (which
/// neither a method name nor a descriptor can contain) and `descriptor`. One
/// definition for the id the debugger learns (`populate_class_metadata`), the
/// id the interpreter matches a breakpoint location against, and the ids in a
/// suspended thread's frame snapshot.
///
/// Until wave 4 only the name was hashed, so every overload of a method
/// shared one id: a breakpoint set in `f(I)V` also fired at the same bytecode
/// index of `f(J)V`, and `ClassType.InvokeMethod` resolved the id to whichever
/// overload the method table listed first.
///
/// Since interpreter round i1 wave 42 the hash lives with the JVMTI native
/// rows (`jvmti::native_env::native_row_method_hash`, compiled in every
/// build), which key a running native by the same id.
#[inline]
pub(crate) fn jdwp_method_id(name: &str, descriptor: &str) -> u64 {
    crate::jvmti::native_env::native_row_method_hash(name, descriptor)
}

/// The JDWP `methodID` of a location in an obsolete method: HotSpot's back
/// end writes a null method id for a method JVMTI `IsMethodObsolete` answers
/// true for (`writeCodeLocation`), which JDI turns into an
/// `ObsoleteMethodImpl`, and `Method.IsObsolete` answers true for it.
pub(crate) const OBSOLETE_METHOD_ID: u64 = 0;

/// The JDWP `methodID` of the method `frame` runs: [`OBSOLETE_METHOD_ID`]
/// for a frame moved onto a body its class's redefinition replaced
/// (`Frame::runs_obsolete_method`), else [`jdwp_method_id`] (interpreter
/// round i1 wave 19, lane L3).
pub(crate) fn frame_method_id(frame: &crate::runtime::frame::Frame) -> u64 {
    if frame.runs_obsolete_method() {
        OBSOLETE_METHOD_ID
    } else {
        jdwp_method_id(frame.method_name(), frame.method_descriptor())
    }
}

/// The JDWP `fieldID` of field `index` of class `class_id`'s own `fields`
/// table: the class id in the high 32 bits, the index in the low 32
/// (wave 8). [`field_of_id`] is the inverse.
pub(crate) fn jdwp_field_id(class_id: crate::classloading::ClassId, index: usize) -> u64 {
    // Cast: a class's field count is bounded by the u16 `fields_count`.
    (u64::from(class_id.as_u32()) << 32) | (index as u64 & 0xFFFF_FFFF)
}

/// The `(declaring class, index into its fields table)` a JDWP `fieldID`
/// names ([`jdwp_field_id`]).
pub(crate) fn field_of_id(field_id: u64) -> (crate::classloading::ClassId, usize) {
    // Cast: the high half is a u32 class id; the low half fits usize.
    (
        crate::classloading::ClassId::new((field_id >> 32) as u32),
        (field_id & 0xFFFF_FFFF) as usize,
    )
}

/// Record debugger metadata for every class defined since the last call —
/// all of them at attach — and return the new classes as `(wire class id,
/// JNI signature, type tag)`.
///
/// Wave 7: classes defined after the debugger attached used to have no
/// metadata at all until a re-attach (metadata was read once, at attach), so
/// `VirtualMachine.ClassesBySignature` could not find them, a breakpoint could
/// not be set in them and a step in them fell back to the line-start rule.
/// The JDWP server now calls this on every poll; class ids are never reused,
/// so the classes new since the last call are the store slots past
/// [`DebugState::class_slots_seen`], and an unchanged store costs one
/// comparison. Takes the class-manager lock, then the debug-state lock (the
/// order `interpreter::deliver_breakpoint_if_set` respects).
/// Make the array classes HotSpot creates at start-up, when a debugger
/// attaches (interpreter round i1 wave 44, lane L1): the eight primitive
/// arrays (`Universe::genesis`), `Object[]`, and `String[]` (the launcher
/// resolves `main(String[])`). This VM makes an array class when Java code
/// first names it; `main`'s arguments are allocated with the element class
/// alone (`vm_exec`'s `GetInitiatedClasses` list makes the same ten, for the
/// same reason). JDI lists every class once, at its first
/// `AllClasses`, and learns of no array class after it (no JVMTI
/// `ClassPrepare` is posted for one), so `classesByName("java.lang.String[]")`
/// found nothing where HotSpot finds the class
/// (`tools/probes/interp/L1/L1W43JdiSourceDebugExtension.java`, `== an array
/// class`: `no String[]`). Once per attach, before the class listing; an
/// array class already made is left as it is.
fn ensure_startup_array_classes(shared: &crate::vm::SharedVm) {
    use cratonvm_types::ClassLoaderId;
    const STARTUP_ARRAYS: [&str; 10] = [
        "[Z",
        "[C",
        "[F",
        "[D",
        "[B",
        "[S",
        "[I",
        "[J",
        "[Ljava/lang/Object;",
        "[Ljava/lang/String;",
    ];
    let missing = {
        let cm = shared.classes.class_manager.read();
        STARTUP_ARRAYS
            .iter()
            .any(|name| cm.loaded_class_under_exact_key(name, ClassLoaderId::Bootstrap).is_none())
    };
    if missing {
        let mut cm = shared.classes.class_manager.write();
        for name in STARTUP_ARRAYS {
            let _ = cm.load_array_class_for_loader(name, ClassLoaderId::Bootstrap);
        }
    }
}

fn populate_class_metadata(shared: &crate::vm::SharedVm) -> Vec<(u64, String, u8)> {
    let cm = shared.classes.class_manager.read();
    let slots = cm.class_store.slot_count();
    let mut ds = shared.debug.debug_state.lock();
    let from = ds.class_slots_seen;
    let mut added = Vec::new();
    if slots <= from {
        return added;
    }
    for slot in from..slots {
        let Ok(raw) = u32::try_from(slot) else {
            break;
        };
        if let Some(class) = cm.class_store.get(crate::classloading::ClassId::new(raw)) {
            added.push(add_class_metadata(&mut ds, class));
        }
    }
    ds.class_slots_seen = slots;
    added
}

/// JDWP `ClassStatus` bits: `VERIFIED` (1), `PREPARED` (2), `INITIALIZED`
/// (4), `ERROR` (8).
const CLASS_STATUS_LINKED: u32 = 1 | 2;
const CLASS_STATUS_INITIALIZED: u32 = 4;
const CLASS_STATUS_ERROR: u32 = 8;

/// The JDWP `ClassStatus` of `class` (interpreter round i1 wave 15), as
/// HotSpot's back end maps `InstanceKlass::jvmti_class_status`: a linked
/// class is VERIFIED | PREPARED, an initialized one INITIALIZED too, an
/// erroneous one (linked, its initialization failed) ERROR too; a class not
/// linked yet, or whose linking failed, has no bit; an array class has none
/// either (JVMTI's `ARRAY` status, which JDWP does not carry). Every class
/// answered VERIFIED | PREPARED.
pub(crate) fn jdwp_class_status(class: &crate::classloading::Class) -> u32 {
    use crate::classloading::ClassState;
    if class.name.starts_with('[') {
        return 0;
    }
    match class.state {
        ClassState::Loading | ClassState::Loaded | ClassState::Verifying => 0,
        ClassState::Verified
        | ClassState::Preparing
        | ClassState::Prepared
        | ClassState::Initializing => CLASS_STATUS_LINKED,
        ClassState::Initialized => CLASS_STATUS_LINKED | CLASS_STATUS_INITIALIZED,
        ClassState::InitializationError => CLASS_STATUS_LINKED | CLASS_STATUS_ERROR,
    }
}

/// Refresh [`DebugState::class_statuses`] for every class the session knows
/// (wave 15). Before the commands that answer a class's status
/// ([`reads_class_status`]). Takes the class-manager lock, then the
/// debug-state lock.
fn refresh_class_statuses(shared: &crate::vm::SharedVm) {
    let cm = shared.classes.class_manager.read();
    let mut ds = shared.debug.debug_state.lock();
    let statuses: HashMap<u64, u32> = ds
        .class_signatures
        .keys()
        .filter_map(|&id| {
            let class = cm
                .class_store
                .get(crate::classloading::ClassId::new(u32::try_from(id).ok()?))?;
            Some((id, jdwp_class_status(class)))
        })
        .collect();
    ds.class_statuses = statuses;
}

/// Refresh the statuses the command in hand answers (interpreter round i1
/// wave 24): `ReferenceType.Status` the one class it names,
/// `ClassesBySignature` the classes of that signature, and only
/// `AllClassesWithGeneric` every class ([`refresh_class_statuses`]). Every
/// one of them refreshed every class the session knows — thousands of
/// class-store reads and a map rebuild for each `isPrepared()` a debugger
/// asks, and JDI asks it of every class it lists by name.
fn refresh_class_statuses_for(
    shared: &crate::vm::SharedVm,
    command_set: u8,
    command: u8,
    data: &[u8],
) {
    use commands::{CMD_RT_STATUS, CMD_VM_CLASSES_BY_SIGNATURE, CS_REF_TYPE, CS_VM};
    let named: Vec<u64> = match (command_set, command) {
        (CS_REF_TYPE, CMD_RT_STATUS) => match protocol::PayloadReader::new(data).read_u64_be() {
            Ok(id) => vec![id],
            Err(_) => return,
        },
        (CS_VM, CMD_VM_CLASSES_BY_SIGNATURE) => {
            let Ok(signature) = protocol::PayloadReader::new(data).read_string() else {
                return;
            };
            let ds = shared.debug.debug_state.lock();
            ds.class_signatures
                .iter()
                .filter(|(_, sig)| **sig == signature)
                .map(|(&id, _)| id)
                .collect()
        }
        _ => return refresh_class_statuses(shared),
    };
    let cm = shared.classes.class_manager.read();
    let mut ds = shared.debug.debug_state.lock();
    for id in named {
        let status = u32::try_from(id)
            .ok()
            .and_then(|raw| cm.class_store.get(crate::classloading::ClassId::new(raw)))
            .map(jdwp_class_status);
        if let Some(status) = status {
            ds.class_statuses.insert(id, status);
        }
    }
}

/// The commands that answer a class's status, before which the server
/// refreshes it ([`refresh_class_statuses`], wave 15):
/// `VirtualMachine.ClassesBySignature` and `AllClassesWithGeneric`, and
/// (wave 23) `ReferenceType.Status`.
fn reads_class_status(command_set: u8, command: u8) -> bool {
    use commands::{
        CMD_RT_STATUS, CMD_VM_ALL_CLASSES_WITH_GENERIC, CMD_VM_CLASSES_BY_SIGNATURE, CS_REF_TYPE,
        CS_VM,
    };
    matches!(
        (command_set, command),
        (
            CS_VM,
            CMD_VM_CLASSES_BY_SIGNATURE | CMD_VM_ALL_CLASSES_WITH_GENERIC
        ) | (CS_REF_TYPE, CMD_RT_STATUS)
    )
}

/// Record one class's debugger metadata (see [`populate_class_metadata`]).
fn add_class_metadata(
    ds: &mut DebugState,
    class: &crate::classloading::Class,
) -> (u64, String, u8) {
    {
        let class_id = class.id.as_u32() as u64;
        // An array class's name is already its JNI signature (`[I`,
        // `[Ljava/lang/String;`) and its JDWP type tag is ARRAY (wave 8: it
        // was registered as `L[I;`, a CLASS, so `ClassesBySignature("[I")`
        // found nothing and an array's `ReferenceType` named a class no
        // debugger could describe).
        let is_array = class.name.starts_with('[');
        let type_tag = if is_array {
            3u8
        } else if class.is_interface() {
            2u8
        } else {
            1u8
        };
        let jni_sig = if is_array {
            class.name.to_string()
        } else {
            format!("L{};", class.name)
        };

        ds.class_type_tags.insert(class_id, type_tag);
        ds.class_signatures.insert(class_id, jni_sig.clone());

        if let Some(ref sf) = class.source_file {
            ds.class_source_files.insert(class_id, sf.to_string());
        }
        // `ClassType.Superclass` read this map, and nothing filled it: every
        // class answered "no superclass". An interface has none (interpreter
        // round i1 wave 43: its class file's `java/lang/Object` was
        // answered; HotSpot answers null, as JVMTI `GetSuperclass` does).
        if let Some(sup) = class.superclass.filter(|_| !class.is_interface()) {
            ds.class_superclass
                .insert(class_id, u64::from(sup.as_u32()));
        }

        // Wave 23: the modifiers, generic signatures, superinterfaces,
        // class-file version and code lengths the JDI session's other
        // `ReferenceType` / `Method` commands answer from.
        let mut details = ClassDetails {
            modifiers: jdwp_class_modifiers(class),
            generic_signature: class.signature.clone(),
            interfaces: if is_array {
                Vec::new()
            } else {
                class
                    .interfaces
                    .iter()
                    .map(|i| u64::from(i.as_u32()))
                    .collect()
            },
            version: (!is_array).then_some((class.version.major, class.version.minor)),
            ..ClassDetails::default()
        };

        // Populate methods
        let mut methods = Vec::new();
        for (_i, m) in class.methods.iter().enumerate() {
            let method_id = jdwp_method_id(&m.name, &m.descriptor);
            let flags = u32::from(m.access_flags.bits());
            // Wave 23: the modifiers HotSpot's back end writes — JVMTI's
            // recognised method modifiers, and `MOD_SYNTHETIC` for a
            // synthetic method (`canGetSyntheticAttribute` is advertised).
            // The raw flags went out, and no method was ever synthetic to JDI.
            let synthetic = flags & ACC_SYNTHETIC != 0
                || has_synthetic_attribute(&m.attributes);
            methods.push(commands::MethodInfo {
                method_id: ids::MethodId(method_id),
                name: m.name.to_string(),
                signature: m.descriptor.to_string(),
                mod_bits: jdwp_member_modifiers(flags, JVM_RECOGNIZED_METHOD_MODIFIERS, synthetic),
            });
            if let Some(generic) = signature_attribute(&m.attributes) {
                details.method_generics.insert(method_id, generic);
            }
            // `Method.LineTable` (interpreter round i1 wave 5, lane L1): the
            // table jdb resolves `stop at Class:line` through, and that a LINE
            // step is shown against. It was declared and read here but never
            // filled, so every method answered an empty table.
            if let Some(code) = m.code() {
                // Widening: a method's code is shorter than 64 KiB.
                details
                    .code_lengths
                    .insert(method_id, code.code.len() as u64);
                let generic_vars: Vec<(usize, u64, String)> = code
                    .attributes
                    .iter()
                    .filter_map(|a| match a {
                        cratonvm_reader::attribute::Attribute::LocalVariableTypeTable(entries) => {
                            Some(entries)
                        }
                        _ => None,
                    })
                    .flatten()
                    .filter_map(|e| {
                        Some((
                            usize::from(e.index),
                            u64::from(e.start_pc),
                            class
                                .constant_pool
                                .get_utf8(e.signature_index)?
                                .to_string(),
                        ))
                    })
                    .collect();
                if !generic_vars.is_empty() {
                    details.variable_generics.insert(method_id, generic_vars);
                }
                let mut lines: Vec<(u64, i32)> = code
                    .attributes
                    .iter()
                    .filter_map(|a| match a {
                        cratonvm_reader::attribute::Attribute::LineNumberTable(entries) => {
                            Some(entries)
                        }
                        _ => None,
                    })
                    .flatten()
                    .map(|e| (u64::from(e.start_pc), i32::from(e.line_number)))
                    .collect();
                if !lines.is_empty() {
                    lines.sort_by_key(|l| l.0);
                    ds.method_line_tables.insert((class_id, method_id), lines);
                }
                // `Method.VariableTable` (wave 6): read by the command handler
                // and, like the line tables were, never filled, so jdb's
                // `locals` / `print x` answered "Local variable information
                // not available" for every method compiled with `-g`.
                let vars: Vec<VariableInfo> = code
                    .attributes
                    .iter()
                    .filter_map(|a| match a {
                        cratonvm_reader::attribute::Attribute::LocalVariableTable(entries) => {
                            Some(entries)
                        }
                        _ => None,
                    })
                    .flatten()
                    .filter_map(|e| {
                        Some(VariableInfo {
                            code_index: u64::from(e.start_pc),
                            name: class.constant_pool.get_utf8(e.name_index)?.to_string(),
                            signature: class
                                .constant_pool
                                .get_utf8(e.descriptor_index)?
                                .to_string(),
                            length: usize::from(e.length),
                            slot: usize::from(e.index),
                        })
                    })
                    .collect();
                if !vars.is_empty() {
                    ds.method_variables.insert((class_id, method_id), vars);
                }
            }
        }
        ds.class_methods.insert(class_id, methods);

        // Populate fields. A field id names the field VM-wide
        // ([`jdwp_field_id`], wave 8): it was the field's index in its class,
        // which every class shares.
        let mut fields = Vec::new();
        for (i, f) in class.fields.iter().enumerate() {
            let field_id = jdwp_field_id(class.id, i);
            let flags = u32::from(f.access_flags.bits());
            let synthetic = flags & ACC_SYNTHETIC != 0 || has_synthetic_attribute(&f.attributes);
            fields.push(commands::FieldInfo {
                field_id: ids::FieldId(field_id),
                name: f.name.to_string(),
                signature: f.descriptor.to_string(),
                mod_bits: jdwp_member_modifiers(flags, JVM_RECOGNIZED_FIELD_MODIFIERS, synthetic),
            });
            if let Some(generic) = signature_attribute(&f.attributes) {
                details.field_generics.insert(field_id, generic);
            }
        }
        ds.class_fields.insert(class_id, fields);
        ds.class_details.insert(class_id, details);
        (class_id, jni_sig, type_tag)
    }
}

/// Class-file access flags the JDWP modifiers below are built from.
const ACC_PUBLIC: u32 = 0x0001;
const ACC_FINAL: u32 = 0x0010;
const ACC_SUPER: u32 = 0x0020;
const ACC_ABSTRACT: u32 = 0x0400;
const ACC_SYNTHETIC: u32 = 0x1000;
/// HotSpot's `JVM_RECOGNIZED_METHOD_MODIFIERS` / `JVM_RECOGNIZED_FIELD_MODIFIERS`
/// / `JVM_ACC_WRITTEN_FLAGS`: what JVMTI's `GetMethodModifiers`,
/// `GetFieldModifiers` and `GetClassModifiers` keep of the class file's flags.
const JVM_RECOGNIZED_METHOD_MODIFIERS: u32 = 0x1DFF;
const JVM_RECOGNIZED_FIELD_MODIFIERS: u32 = 0x50DF;
const JVM_ACC_WRITTEN_FLAGS: u32 = 0x7FFF;
/// JDWP's synthetic marker in a field's or method's `modBits` (HotSpot's
/// back end `MOD_SYNTHETIC`; JDI `VMModifiers.SYNTHETIC`).
pub(crate) const MOD_SYNTHETIC: u32 = 0xF000_0000;

/// A field's or method's JDWP `modBits` (interpreter round i1 wave 23): the
/// class file's `flags` that JVMTI recognises (`recognized`), with
/// [`MOD_SYNTHETIC`] for a synthetic member, as HotSpot's back end writes
/// them (`writeMethodInfo` / `writeFieldInfo`).
fn jdwp_member_modifiers(flags: u32, recognized: u32, synthetic: bool) -> u32 {
    let modifiers = flags & recognized;
    if synthetic {
        modifiers | MOD_SYNTHETIC
    } else {
        modifiers
    }
}

/// Does a member carry the `Synthetic` attribute (JVMS §4.7.8)? HotSpot's
/// class-file parser turns it into `ACC_SYNTHETIC`.
fn has_synthetic_attribute(attributes: &[cratonvm_reader::attribute::LazyAttribute]) -> bool {
    attributes.iter().any(|a| {
        matches!(
            a.as_decoded(),
            Some(cratonvm_reader::attribute::Attribute::Synthetic)
        )
    })
}

/// A member's `Signature` attribute (its generic signature), if any.
fn signature_attribute(attributes: &[cratonvm_reader::attribute::LazyAttribute]) -> Option<String> {
    attributes.iter().find_map(|a| match a.as_decoded() {
        Some(cratonvm_reader::attribute::Attribute::Signature(s)) => Some(s.to_string()),
        _ => None,
    })
}

/// `ReferenceType.Modifiers` of `class` (interpreter round i1 wave 23), as
/// HotSpot's JVMTI `GetClassModifiers` answers it: the flags of the class's
/// own `InnerClasses` entry when it has one (a member, local or anonymous
/// class: `static`, `private`, ... live there), else the class file's
/// flags, limited to the written flags, with `ACC_SUPER` as the class file
/// has it; an array class is `public final abstract` (HotSpot takes the
/// visibility of a reference array's element class; this answers `public`
/// for every array). It was not served at all (`NOT_IMPLEMENTED`), which JDI
/// asks as soon as it lists a class's methods. Wave 42 (lane L1): an array
/// class has `ACC_SUPER` too, `0x431` for `String[]` and `int[]` alike on
/// HotSpot 25.0.3 (`tools/probes/interp/L1/L1W42RawJdwpErrorAnswers.java`);
/// it answered `0x411`.
fn jdwp_class_modifiers(class: &crate::classloading::Class) -> u32 {
    if class.name.starts_with('[') {
        return ACC_PUBLIC | ACC_FINAL | ACC_SUPER | ACC_ABSTRACT;
    }
    let own = u32::from(class.access_flags.bits());
    let access = class
        .inner_classes
        .iter()
        .find(|e| e.inner_class.as_str() == &*class.name)
        .map_or(own, |e| u32::from(e.access_flags));
    let modifiers = access & !ACC_SUPER & JVM_ACC_WRITTEN_FLAGS;
    modifiers | (own & ACC_SUPER)
}

/// Replace DebugState's thread table with the thread registry's live threads.
///
/// Wave 8: it was filled once, at attach, so a thread started later had no
/// name and was missing from `AllThreads`, and one that had died stayed
/// listed. The server now refreshes it before every command that lists,
/// names or targets a thread ([`refreshes_thread_names`]). Takes the registry
/// lock, then the debug-state lock.
fn populate_thread_metadata(shared: &crate::vm::SharedVm) {
    let registry = &shared.threads.thread_registry;
    let names = registry.all_thread_names();
    // Wave 11: each thread's JDWP status, read before the debug-state lock
    // (the registry lock is taken per thread, never under the debug state).
    // Wave 25: with the class manager, which tells a sleep entered through
    // the registered `Thread.sleep` native (no `Thread` frame is deposited)
    // from another timed wait ([`frame_invokes_thread_sleep`]).
    let statuses: Vec<(u64, u32)> = {
        let cm = shared.classes.class_manager.read();
        let invokes_sleep =
            |entry: &cratonvm_native_api::StackTraceEntry| frame_invokes_thread_sleep(&cm, entry);
        names
            .iter()
            .map(|(tid, _)| (tid.0, jdwp_thread_status_with(registry, *tid, &invokes_sleep)))
            .collect()
    };
    let mut ds = shared.debug.debug_state.lock();
    // The heap service is the back end's own thread (wave 10), as HotSpot's
    // back end hides its own: never listed, so never suspended or named.
    let hidden = ds.heap_service_thread();
    ds.thread_names = names
        .into_iter()
        .filter(|(tid, _)| Some(tid.0) != hidden)
        .map(|(tid, name)| (tid.0, name))
        .collect();
    // Wave 45 (lane L1): a thread parked at a native's return keeps the
    // status HotSpot keeps there (`native_exit_statuses`).
    let statuses: HashMap<u64, u32> = statuses
        .into_iter()
        .filter(|(tid, _)| Some(*tid) != hidden)
        .map(|(tid, status)| match ds.native_exit_statuses.get(&tid) {
            Some(&parked) if status == commands::THREAD_STATUS_RUNNING => (tid, parked),
            _ => (tid, status),
        })
        .collect();
    ds.thread_statuses = statuses;
}

/// The JDWP `ThreadStatus` of a registered thread (wave 11), mapped as
/// HotSpot's back end maps JVMTI thread state (`map2jdwpThreadStatus`): a
/// dead thread is `ZOMBIE`; a thread blocked on monitor entry `MONITOR`; one
/// in `Thread.sleep` `SLEEPING`; any other wait, park or join, timed or not,
/// `WAIT`; everything else `RUNNING`. The blocking kind is the one
/// `Thread.getState()` reads (`ThreadRegistry::java_block_state`: 1 waiting,
/// 2 blocked, 3 timed). A timed block is a sleep when the frames the thread
/// deposited on entering it (`ThreadRegistry::frame_trace_of`) end in
/// `java.lang.Thread.sleep*`: the blocking region does not say which native
/// entered it.
#[cfg(test)]
fn jdwp_thread_status(
    registry: &crate::threading::thread_registry::ThreadRegistry,
    tid: crate::threading::ThreadId,
) -> u32 {
    jdwp_thread_status_with(registry, tid, &|_| false)
}

/// [`jdwp_thread_status`], where `invokes_sleep` answers whether the
/// deposited top frame is calling `Thread.sleep*` (interpreter round i1 wave
/// 25): this VM registers `Thread.sleep(long)` as a native, so a sleeping
/// thread deposits no `Thread` frame at all — its top frame is the caller
/// (`sleepQuietly`), and the status read `WAIT` where HotSpot says
/// `SLEEPING` (`L1W24JdiThreads`, `L1W24JdiSurface`).
fn jdwp_thread_status_with(
    registry: &crate::threading::thread_registry::ThreadRegistry,
    tid: crate::threading::ThreadId,
    invokes_sleep: &dyn Fn(&cratonvm_native_api::StackTraceEntry) -> bool,
) -> u32 {
    use commands::{
        THREAD_STATUS_MONITOR, THREAD_STATUS_RUNNING, THREAD_STATUS_SLEEPING, THREAD_STATUS_WAIT,
        THREAD_STATUS_ZOMBIE,
    };
    if !registry.is_alive(tid) {
        return THREAD_STATUS_ZOMBIE;
    }
    match registry.java_block_state(tid) {
        1 => THREAD_STATUS_WAIT,
        2 => THREAD_STATUS_MONITOR,
        3 => {
            // The deposited trace is outermost-first
            // (`stackwalker::capture_frames_no_lines`): the frame that called
            // the blocking native is the LAST one. Wave 12: this read the
            // first, the thread's bottom frame (`main`, `Thread.run`), so a
            // sleeping thread was never `SLEEPING`.
            let trace = registry.frame_trace_of(tid);
            if trace
                .last()
                .is_some_and(|top| is_thread_sleep_frame(top) || invokes_sleep(top))
            {
                THREAD_STATUS_SLEEPING
            } else {
                THREAD_STATUS_WAIT
            }
        }
        _ => THREAD_STATUS_RUNNING,
    }
}

/// Is `frame` one of `java.lang.Thread`'s sleep methods (`sleep`,
/// `sleepNanos`, `sleep0`, `sleepNanos0`)? The class name is accepted in
/// either spelling the frame capture may use.
fn is_thread_sleep_frame(frame: &cratonvm_native_api::StackTraceEntry) -> bool {
    matches!(&*frame.class_name, "java.lang.Thread" | "java/lang/Thread")
        && frame.method_name.starts_with("sleep")
}

/// Is the deposited frame `entry` stopped at an invoke of
/// `java.lang.Thread.sleep*` (interpreter round i1 wave 25)? Read from the
/// method's bytecode at the entry's `byte_code_index` (the invoke the
/// blocking native was entered from: the dispatch loop sets
/// `last_instr_pc` at every instruction) and the constant pool's method
/// reference. The deposit keeps no descriptor, so every method of that name
/// is asked; `false` when the class or the code is not there.
fn frame_invokes_thread_sleep(
    cm: &crate::classloading::ClassManager,
    entry: &cratonvm_native_api::StackTraceEntry,
) -> bool {
    use cratonvm_reader::constant_pool::ConstantPoolEntry;
    let (Some(class_id), Ok(bci)) = (entry.class_id, usize::try_from(entry.byte_code_index))
    else {
        return false;
    };
    let Some(class) = cm.get_class(class_id) else {
        return false;
    };
    class
        .methods
        .iter()
        .filter(|m| &*m.name == &*entry.method_name)
        .any(|m| {
            let Some(code) = m.code() else {
                return false;
            };
            let code: &[u8] = &code.code[..];
            // invokevirtual, invokespecial, invokestatic
            if !matches!(code.get(bci), Some(0xb6..=0xb8)) {
                return false;
            }
            let (Some(&hi), Some(&lo)) = (code.get(bci + 1), code.get(bci + 2)) else {
                return false;
            };
            let Some(ConstantPoolEntry::MethodReference {
                class_index,
                name_and_type_index,
            }) = class.constant_pool.get(u16::from_be_bytes([hi, lo]))
            else {
                return false;
            };
            class.constant_pool.get_class_name(*class_index) == Some("java/lang/Thread")
                && class
                    .constant_pool
                    .get_name_and_type(*name_and_type_index)
                    .is_some_and(|(name, _)| name.starts_with("sleep"))
        })
}

/// The commands before which the server refreshes its thread table
/// ([`populate_thread_metadata`]): `VirtualMachine.AllThreads`,
/// `ThreadGroupReference.Children`, `ThreadReference.Name` and the commands
/// whose target must be a live thread: the two invocations and (wave 9)
/// `StackFrame.SetValues`; (wave 11) `ThreadReference.Status`,
/// `ThreadGroup`, `Suspend`, `Frames` and `FrameCount`, which answer from, or
/// check the target against, the refreshed thread states; (wave 24)
/// `ClassType.NewInstance`, `InterfaceType.InvokeMethod`,
/// `ThreadReference.Interrupt` / `Stop` and `TopLevelThreadGroups`, which
/// read the thread groups of the listed threads; (wave 25) the owned- and
/// contended-monitor commands and `IsVirtual`.
fn refreshes_thread_names(command_set: u8, command: u8) -> bool {
    use commands::{
        CMD_CT_INVOKE_METHOD, CMD_CT_NEW_INSTANCE, CMD_IT_INVOKE_METHOD, CMD_OR_INVOKE_METHOD,
        CMD_SF_SET_VALUES, CMD_TGR_CHILDREN, CMD_TR_FRAMES, CMD_TR_FRAME_COUNT, CMD_TR_INTERRUPT,
        CMD_TR_NAME, CMD_TR_STATUS, CMD_TR_STOP, CMD_TR_SUSPEND, CMD_TR_THREAD_GROUP,
        CMD_VM_ALL_THREADS, CMD_VM_TOP_LEVEL_THREAD_GROUPS, CS_CLASS_TYPE, CS_INTERFACE_TYPE,
        CS_OBJECT_REF, CS_STACK_FRAME, CS_THREAD_GROUP_REF, CS_THREAD_REF, CS_VM,
    };
    matches!(
        (command_set, command),
        (CS_VM, CMD_VM_ALL_THREADS | CMD_VM_TOP_LEVEL_THREAD_GROUPS)
            | (CS_THREAD_GROUP_REF, CMD_TGR_CHILDREN)
            | (CS_THREAD_REF, CMD_TR_NAME)
            | (
                CS_THREAD_REF,
                CMD_TR_STATUS
                    | CMD_TR_THREAD_GROUP
                    | CMD_TR_SUSPEND
                    | CMD_TR_FRAMES
                    | CMD_TR_FRAME_COUNT
                    | CMD_TR_INTERRUPT
                    | CMD_TR_STOP
            )
            | (CS_CLASS_TYPE, CMD_CT_INVOKE_METHOD | CMD_CT_NEW_INSTANCE)
            | (CS_INTERFACE_TYPE, CMD_IT_INVOKE_METHOD)
            | (CS_OBJECT_REF, CMD_OR_INVOKE_METHOD)
            | (CS_STACK_FRAME, CMD_SF_SET_VALUES)
    ) || inspect::is_monitor_command(command_set, command)
        || (command_set, command) == (CS_THREAD_REF, commands::CMD_TR_IS_VIRTUAL)
}

/// Rewrite, in place, the thread and class ids at the head of a command's
/// payload from their wire form to the VM's (interpreter round i1 wave 25;
/// see `ids::thread_to_wire`): the thread every `ThreadReference` and
/// `StackFrame` command names first, the class every `ReferenceType`,
/// `ClassType`, `ArrayType`, `InterfaceType` and `Method` command names
/// first, and the thread (and, for `ObjectReference.InvokeMethod`, the class)
/// an invocation names after its receiver or class. Ids further in (event
/// modifiers, tagged values, object ids that name threads) are decoded where
/// they are read (`commands::handle_er_set`, `object_for_id`). A payload too
/// short for an id is left alone: its handler refuses it.
fn ids_from_wire(command_set: u8, command: u8, data: &mut [u8]) {
    use commands::{
        CMD_CT_INVOKE_METHOD, CMD_CT_NEW_INSTANCE, CMD_IT_INVOKE_METHOD, CMD_OR_INVOKE_METHOD,
        CS_ARRAY_TYPE, CS_CLASS_TYPE, CS_INTERFACE_TYPE, CS_METHOD, CS_OBJECT_REF,
        CS_REF_TYPE, CS_STACK_FRAME, CS_THREAD_REF,
    };
    fn rewrite(data: &mut [u8], at: usize, decode: fn(u64) -> u64) {
        let Some(bytes) = data.get_mut(at..at + 8) else {
            return;
        };
        let mut raw = [0u8; 8];
        raw.copy_from_slice(bytes);
        bytes.copy_from_slice(&decode(u64::from_be_bytes(raw)).to_be_bytes());
    }
    match command_set {
        CS_THREAD_REF | CS_STACK_FRAME => rewrite(data, 0, ids::thread_from_wire),
        CS_REF_TYPE | CS_CLASS_TYPE | CS_ARRAY_TYPE | CS_INTERFACE_TYPE | CS_METHOD => {
            rewrite(data, 0, ids::class_from_wire)
        }
        _ => {}
    }
    match (command_set, command) {
        (CS_CLASS_TYPE, CMD_CT_INVOKE_METHOD | CMD_CT_NEW_INSTANCE)
        | (CS_INTERFACE_TYPE, CMD_IT_INVOKE_METHOD) => rewrite(data, 8, ids::thread_from_wire),
        (CS_OBJECT_REF, CMD_OR_INVOKE_METHOD) => {
            rewrite(data, 8, ids::thread_from_wire);
            rewrite(data, 16, ids::class_from_wire);
        }
        _ => {}
    }
}

/// `INVALID_OBJECT` for a command whose leading thread or reference-type id
/// names nothing at all (interpreter round i1 wave 43, lane L1;
/// `docs/internal/fixed-bugs/interpreter-L1-jdwp-ids-naming-nothing-are-not-invalid-object-FIXED-20261007.md`).
///
/// HotSpot's back end reads a `threadID` / `referenceTypeID` as an object id
/// first: an id its object table does not hold is `INVALID_OBJECT` (20),
/// which JDI turns into `ObjectCollectedException`; only an id naming an
/// object of the wrong kind is `INVALID_THREAD` (10) / `INVALID_CLASS` (21)
/// (`tools/probes/interp/L1/L1W42RawJdwpErrorAnswers.java`, measured on
/// HotSpot 25.0.3). The handlers answered 10 / 21 for both. `None` (the
/// handler answers) when the id names anything the server knows under any
/// of its id spaces: a thread (the server's tables or the registry), a class
/// (the class store, read here so a class defined since the last poll
/// counts), an exported object, or the thread-group id; and for JDWP's null
/// (wire 0), whose answer is the handler's. `data` holds the VM ids
/// ([`ids_from_wire`] ran): the wire id is rebuilt so each table is asked
/// in its own decoding (a thread's wire id sent to a `ReferenceType` command
/// still names that thread, and is `INVALID_CLASS`).
///
/// Lock order: the class manager, released, then the debug state, then the
/// registry's thread map innermost (as [`thread_object_for_id`]).
fn unknown_id_refusal(
    shared: &crate::vm::SharedVm,
    command_set: u8,
    data: &[u8],
) -> Option<commands::CommandResult> {
    use commands::{
        CS_ARRAY_TYPE, CS_CLASS_TYPE, CS_INTERFACE_TYPE, CS_METHOD, CS_REF_TYPE, CS_STACK_FRAME,
        CS_THREAD_REF,
    };
    let vm_id = protocol::PayloadReader::new(data).read_u64_be().ok()?;
    if vm_id == ids::NO_VM_ID {
        return None;
    }
    let wire = match command_set {
        CS_THREAD_REF | CS_STACK_FRAME => ids::thread_to_wire(vm_id),
        CS_REF_TYPE | CS_CLASS_TYPE | CS_ARRAY_TYPE | CS_INTERFACE_TYPE | CS_METHOD => {
            ids::class_to_wire(vm_id)
        }
        _ => return None,
    };
    if id_names_something(shared, wire) {
        None
    } else {
        Some(commands::CommandResult::error(commands::ERR_INVALID_OBJECT))
    }
}

/// Does wire id `wire` name a thread, a class, an exported object or the
/// thread group ([`unknown_id_refusal`])?
fn id_names_something(shared: &crate::vm::SharedVm, wire: u64) -> bool {
    if wire == commands::SYSTEM_THREAD_GROUP_ID {
        return true;
    }
    let class = ids::class_from_wire(wire);
    let in_class_store = u32::try_from(class).is_ok_and(|raw| {
        shared
            .classes
            .class_manager
            .read()
            .class_store
            .get(crate::classloading::ClassId::new(raw))
            .is_some()
    });
    if in_class_store {
        return true;
    }
    let thread = ids::thread_from_wire(wire);
    let ds = shared.debug.debug_state.lock();
    if ds.objects.handle_of(wire).is_some()
        || ds.class_signatures.contains_key(&class)
        || ds.class_methods.contains_key(&class)
        || commands::thread_status_of(&ds, thread).is_some()
    {
        return true;
    }
    thread < ids::HEAP_OBJECT_ID_BASE
        && shared
            .threads
            .thread_registry
            .java_thread_obj(crate::threading::ThreadId(thread))
            .is_some()
}

/// The commands that read a thread's published frames (wave 12):
/// `ThreadReference.Frames` / `FrameCount` and `StackFrame.GetValues` /
/// `ThisObject`; and (wave 25) the owned-monitor commands, whose stack
/// depths are places in that listing (`inspect::is_monitor_command`). Each
/// names the thread first.
fn reads_thread_frames(command_set: u8, command: u8) -> bool {
    use commands::{
        CMD_SF_GET_VALUES, CMD_SF_THIS_OBJECT, CMD_TR_FRAMES, CMD_TR_FRAME_COUNT, CS_STACK_FRAME,
        CS_THREAD_REF,
    };
    matches!(
        (command_set, command),
        (CS_THREAD_REF, CMD_TR_FRAMES | CMD_TR_FRAME_COUNT)
            | (CS_STACK_FRAME, CMD_SF_GET_VALUES | CMD_SF_THIS_OBJECT)
    ) || inspect::is_monitor_command(command_set, command)
}

/// The commands the server answers from the VM's own state, with neither the
/// heap nor a thread (interpreter round i1 wave 24): `VirtualMachine.ClassPaths`
/// and `Method.Bytecodes`; (wave 25) `VirtualMachine.Version` and
/// `ThreadReference.IsVirtual`.
/// `None` for every other command.
fn serve_with_the_vm(
    shared: &crate::vm::SharedVm,
    command_set: u8,
    command: u8,
    data: &[u8],
) -> Option<commands::CommandResult> {
    match (command_set, command) {
        (commands::CS_VM, commands::CMD_VM_VERSION) => Some(vm_version(shared)),
        (commands::CS_VM, commands::CMD_VM_CLASS_PATHS) => Some(class_paths(shared)),
        (commands::CS_METHOD, commands::CMD_M_BYTECODES) => Some(method_bytecodes(shared, data)),
        (commands::CS_REF_TYPE, commands::CMD_RT_SOURCE_DEBUG_EXTENSION) => {
            Some(source_debug_extension(shared, data))
        }
        (commands::CS_THREAD_REF, commands::CMD_TR_IS_VIRTUAL) => {
            Some(thread_is_virtual(shared, data))
        }
        (commands::CS_THREAD_REF, commands::CMD_TR_FORCE_EARLY_RETURN) => {
            Some(early_return::force_early_return(shared, data))
        }
        // Wave 44: on the frame's parked thread (`debug::pop_frames`).
        (commands::CS_STACK_FRAME, commands::CMD_SF_POP_FRAMES) => {
            Some(pop_frames::pop_frames(shared, data))
        }
        _ => None,
    }
}

/// `VirtualMachine.Version` (1/1), as the JDK the VM runs answers it
/// (interpreter round i1 wave 25; proposal
/// `docs/internal/fixed-bugs/interpreter-L1-proposal-answer-jdwp-as-the-jdk-it-runs-FIXED-20260930.md`):
/// the JDWP version is the JDK's (`java.specification.version`: `25` is
/// JDWP 25.0, `1.8` is 1.8), `vmVersion` its `java.version` and `vmName` its
/// `java.vm.name`, as HotSpot's back end answers (`VirtualMachine.c`
/// `version`); the description names this server. The server answered JDWP
/// 1.8 and `vmVersion` "1.8.0" whatever the JDK, so JDI took a JDK 25 program
/// for a JDK 8 one: `canGetModuleInfo()` was false (`ReferenceType.module()`
/// and `allModules()` threw) and JDI never asked whether a thread is virtual.
/// The commands JDI sends only to a 9+ / 19+ target — `ReferenceType.Module`,
/// `VirtualMachine.AllModules`, `ModuleReference.Name` / `ClassLoader`,
/// `ThreadReference.IsVirtual` and the `PlatformThreadsOnly` modifier (read
/// off a HotSpot session, `tools/probes/interp/L1/L1W25JdiVersion.java`) —
/// are served since the same wave. Without the properties (a bare VM), 1.8.
fn vm_version(shared: &crate::vm::SharedVm) -> commands::CommandResult {
    let (spec, java_version, vm_name) = {
        let props = shared.system_properties.read();
        let get = |key: &str| props.get(key).cloned();
        (
            get("java.specification.version"),
            get("java.version"),
            get("java.vm.name"),
        )
    };
    let (major, minor) = spec.as_deref().and_then(jdwp_version_of).unwrap_or((1, 8));
    let java_version = java_version.unwrap_or_else(|| "1.8.0".to_string());
    let vm_name = vm_name.unwrap_or_else(|| "CratonVM".to_string());
    let mut pw = protocol::PayloadWriter::new();
    pw.put_string(&format!(
        "CratonVM JDWP Debug Server, Java Debug Wire Protocol version {major}.{minor}\n\
         JVM version {java_version} ({vm_name})"
    ));
    pw.put_u32_be(major);
    pw.put_u32_be(minor);
    pw.put_string(&java_version);
    pw.put_string(&vm_name);
    commands::CommandResult::ok(pw.into_bytes())
}

/// The JDWP `(major, minor)` of a JDK whose `java.specification.version` is
/// `spec`: `(25, 0)` for `25`, `(1, 8)` for `1.8` (wave 25).
fn jdwp_version_of(spec: &str) -> Option<(u32, u32)> {
    let mut parts = spec.trim().split('.');
    let major: u32 = parts.next()?.parse().ok()?;
    let minor: u32 = match parts.next() {
        Some(m) => m.parse().ok()?,
        None => 0,
    };
    Some((major, minor))
}

/// `ThreadReference.IsVirtual` (11/15, JDWP 21; interpreter round i1 wave
/// 25): whether the thread is one of the VM's virtual threads
/// (`VirtualThreadManager::is_virtual`). JDI asks it only of a JDWP 19+
/// target, and jdb asks it of every thread event's thread.
/// `INVALID_THREAD` for an id naming no thread the session knows.
fn thread_is_virtual(shared: &crate::vm::SharedVm, data: &[u8]) -> commands::CommandResult {
    let Ok(tid) = protocol::PayloadReader::new(data).read_u64_be() else {
        return commands::CommandResult::error(commands::ERR_INTERNAL);
    };
    let (known, started_virtual) = {
        let ds = shared.debug.debug_state.lock();
        (
            commands::thread_status_of(&ds, tid).is_some(),
            ds.virtual_threads.contains(&tid),
        )
    };
    if !known {
        return commands::CommandResult::error(commands::ERR_INVALID_THREAD);
    }
    let is_virtual =
        started_virtual || shared.threads.virtual_thread_manager.is_virtual(tid);
    commands::CommandResult::ok(vec![u8::from(is_virtual)])
}

/// `Method.Bytecodes` (6/3, interpreter round i1 wave 24): the method's
/// class-file bytecode, as JVMTI `GetBytecodes` answers it — `NATIVE_METHOD`
/// for a native method, no bytes for an abstract one; `INVALID_CLASS` /
/// `INVALID_METHODID` for ids naming none. It answered zero bytes for every
/// method (from a table nothing filled), so `canGetBytecodes` answered false,
/// which turns off what a debugger builds on the bytecode (IntelliJ's smart
/// step into, method-breakpoint emulation); HotSpot answers true.
fn method_bytecodes(shared: &crate::vm::SharedVm, data: &[u8]) -> commands::CommandResult {
    use commands::{CommandResult, ERR_INTERNAL, ERR_INVALID_CLASS, ERR_INVALID_METHODID};
    let mut r = protocol::PayloadReader::new(data);
    let (Ok(class_id), Ok(method_id)) = (r.read_u64_be(), r.read_u64_be()) else {
        return CommandResult::error(ERR_INTERNAL);
    };
    let Ok(raw) = u32::try_from(class_id) else {
        return CommandResult::error(ERR_INVALID_CLASS);
    };
    let cm = shared.classes.class_manager.read();
    let Some(class) = cm.class_store.get(crate::classloading::ClassId::new(raw)) else {
        return CommandResult::error(ERR_INVALID_CLASS);
    };
    let Some(method) = class
        .methods
        .iter()
        .find(|m| jdwp_method_id(&m.name, &m.descriptor) == method_id)
    else {
        return CommandResult::error(ERR_INVALID_METHODID);
    };
    if method.is_native() {
        return CommandResult::error(commands::ERR_NATIVE_METHOD);
    }
    let code: &[u8] = method.code().map_or(&[][..], |c| &c.code[..]);
    let mut pw = protocol::PayloadWriter::new();
    pw.put_u32_be(u32::try_from(code.len()).unwrap_or(0));
    pw.put_bytes(code);
    CommandResult::ok(pw.into_bytes())
}

/// `ReferenceType.SourceDebugExtension` (2/12, interpreter round i1 wave 43,
/// lane L1; stage 1 of
/// `docs/internal/fixed-bugs/interpreter-L1-proposal-pop-frames-force-early-return-and-source-debug-extension-FIXED-20261008.md`):
/// the class's `SourceDebugExtension` attribute (JVMS 4.7.11, a JSR-45 SMAP
/// for Kotlin inline functions, JSP and the like), as JVMTI
/// `GetSourceDebugExtension` answers it; `ABSENT_INFORMATION` for a class
/// without one and for an array class; `INVALID_CLASS` for an id naming no
/// class.
///
/// `Class` does not keep the attribute (adding a field means every one of
/// its ~70 struct literals), so the class file is read again, only when a
/// debugger asks, from where a retransformation reads it
/// (`vm/src/runtime/instrument.rs`, `try_original_class_bytes`): the class
/// manager's class-bytes cache, filled by every define, else the built-in
/// delegation chain's copy when it is byte for byte the file the class was
/// defined from (`class_bytes_match_base`). A class whose file is in
/// neither (a user loader's class the FIFO evicted) answers
/// `ABSENT_INFORMATION`, which JDI reads as "no SMAP": the class keeps the
/// base stratum, as before this command was served. For a class a load-time
/// transformer changed, the cache holds the file before the transform (its
/// retransformation base); transformers do not change this attribute in
/// practice.
///
/// JDI asks it of every class whose lines it maps once the capability is
/// true (`ReferenceTypeImpl.sourceDebugExtensionInfo`, cached per class), so
/// a class without the attribute must answer `ABSENT_INFORMATION`, never an
/// error JDI would throw.
fn source_debug_extension(shared: &crate::vm::SharedVm, data: &[u8]) -> commands::CommandResult {
    use commands::{CommandResult, ERR_ABSENT_INFORMATION, ERR_INTERNAL, ERR_INVALID_CLASS};
    let Ok(class_id) = protocol::PayloadReader::new(data).read_u64_be() else {
        return CommandResult::error(ERR_INTERNAL);
    };
    let Ok(raw) = u32::try_from(class_id) else {
        return CommandResult::error(ERR_INVALID_CLASS);
    };
    let bytes: Option<Vec<u8>> = {
        let cm = shared.classes.class_manager.read();
        let id = crate::classloading::ClassId::new(raw);
        let Some(class) = cm.class_store.get(id) else {
            return CommandResult::error(ERR_INVALID_CLASS);
        };
        if class.name.starts_with('[') {
            return CommandResult::error(ERR_ABSENT_INFORMATION);
        }
        defined_class_file(&cm, id, &class.name)
    };
    match bytes.as_deref().and_then(source_debug_extension_of) {
        Some(smap) => {
            // A JDWP string is its length and its (modified) UTF-8 bytes,
            // which is what the attribute holds.
            let mut pw = protocol::PayloadWriter::new();
            pw.put_u32_be(u32::try_from(smap.len()).unwrap_or(0));
            pw.put_bytes(&smap);
            CommandResult::ok(pw.into_bytes())
        }
        None => CommandResult::error(ERR_ABSENT_INFORMATION),
    }
}

/// The class file class `id` (named `name`) was defined from, for the
/// debugger's reads of attributes `Class` does not keep
/// ([`source_debug_extension`]): the class-bytes cache, else the delegation
/// chain's copy when it is byte for byte the defined file. `None` when
/// neither has it.
fn defined_class_file(
    cm: &crate::classloading::ClassManager,
    id: crate::classloading::ClassId,
    name: &str,
) -> Option<Vec<u8>> {
    if let Some(cached) = cm.class_bytes_cache.get(&id) {
        return Some(cached.to_vec());
    }
    match cm.find_class_bytes_for_transform(name) {
        Ok((found, _)) if cm.class_bytes_match_base(id, &found) == Some(true) => Some(found),
        _ => None,
    }
}

/// The source file names the strata of `class`'s `SourceDebugExtension`
/// list (interpreter round i1 wave 43, lane L1): what a `ClassPrepare`
/// request's `SourceNameMatch` matches besides the `SourceFile`, as HotSpot
/// matches them (`tools/probes/interp/L1/L1W43JdiSourceDebugExtension.java`:
/// a filter on `Inline.kt`, a name only the SMAP lists, reports the class).
/// Empty without the attribute. Read only while such a filter is in force
/// (`EventManager::has_source_name_filters`).
fn smap_source_names(
    cm: &crate::classloading::ClassManager,
    class: &crate::classloading::Class,
) -> Vec<String> {
    if class.name.starts_with('[') {
        return Vec::new();
    }
    defined_class_file(cm, class.id, &class.name)
        .as_deref()
        .and_then(source_debug_extension_of)
        .map_or_else(Vec::new, |smap| smap_file_names(&smap))
}

/// The file names of every file section (`*F`) of JSR-45 SMAP `smap`: each
/// entry is `<id> <name>`, or `+ <id> <name>` followed by a line with the
/// file's path. Each name once, in order.
fn smap_file_names(smap: &[u8]) -> Vec<String> {
    let text = String::from_utf8_lossy(smap);
    let mut names: Vec<String> = Vec::new();
    let mut in_files = false;
    let mut path_follows = false;
    for line in text.lines() {
        let line = line.trim_end_matches('\r');
        if line.starts_with('*') {
            in_files = line.trim_end() == "*F";
            path_follows = false;
            continue;
        }
        if !in_files {
            continue;
        }
        if path_follows {
            path_follows = false;
            continue;
        }
        let (plus, entry) = match line.strip_prefix('+') {
            Some(rest) => (true, rest.trim_start()),
            None => (false, line.trim_start()),
        };
        path_follows = plus;
        if let Some((_, name)) = entry.split_once(' ') {
            let name = name.trim();
            if !name.is_empty() && !names.iter().any(|n| n == name) {
                names.push(name.to_string());
            }
        }
    }
    names
}

/// The bytes of class file `class_file`'s `SourceDebugExtension` attribute
/// ([`source_debug_extension`]); `None` without one or for a file that does
/// not parse. The reader keeps the attribute as raw bytes (it has no decoded
/// form), so it is read from its range without decoding.
fn source_debug_extension_of(class_file: &[u8]) -> Option<Vec<u8>> {
    use cratonvm_reader::attribute::{Attribute, LazyAttribute};
    let parsed = cratonvm_reader::class_reader::read_class(class_file).ok()?;
    let lazy = parsed
        .attributes
        .iter()
        .find(|a| a.name() == "SourceDebugExtension")?;
    match lazy {
        LazyAttribute::Raw { source, range, .. } => source.get(range.clone()).map(<[u8]>::to_vec),
        LazyAttribute::Decoded(Attribute::Unknown { data, .. }) => Some(data.to_vec()),
        LazyAttribute::Decoded(_) => None,
    }
}

/// `VirtualMachine.ClassPaths` (1/13, interpreter round i1 wave 24): as
/// HotSpot's back end answers it (`classPaths`) — `baseDir` is `user.dir`,
/// the class paths are `java.class.path` split at `path.separator`, and the
/// boot class path is empty (JDK 9+ has none). JDI's
/// `PathSearchingVirtualMachine.classPath()` (jdb `classpath`) sends it
/// without a capability check; it answered `NOT_IMPLEMENTED`.
fn class_paths(shared: &crate::vm::SharedVm) -> commands::CommandResult {
    let (base, class_path, separator) = {
        let props = shared.system_properties.read();
        let get = |key: &str| props.get(key).cloned().unwrap_or_default();
        (
            get("user.dir"),
            get("java.class.path"),
            get("path.separator"),
        )
    };
    let separator = separator.chars().next().unwrap_or(if cfg!(windows) { ';' } else { ':' });
    let entries: Vec<&str> = if class_path.is_empty() {
        Vec::new()
    } else {
        class_path.split(separator).collect()
    };
    let mut pw = protocol::PayloadWriter::new();
    pw.put_string(&base);
    pw.put_u32_be(u32::try_from(entries.len()).unwrap_or(u32::MAX));
    for entry in entries {
        pw.put_string(entry);
    }
    pw.put_u32_be(0); // bootclasspaths
    commands::CommandResult::ok(pw.into_bytes())
}

/// Before a command that reads a thread's frames (wave 12): a thread the
/// debugger suspended while it was blocked in a native region has published
/// none (a blocked thread publishes on entry only when it is already
/// suspended, `interpreter::publish_blocked_frames`), so read them now through
/// its inspection window, on a registered mutator (a parked thread or the
/// heap service, [`run_on_parked_thread`]). Nothing happens for a thread that
/// is not suspended (the command refuses it), already has a snapshot, or is
/// not blocked in such a region (a running thread publishes at its suspend
/// point).
fn publish_blocked_frames_for(shared: &crate::vm::SharedVm, data: &[u8]) {
    let Some(tid) = data
        .get(..8)
        .and_then(|bytes| <[u8; 8]>::try_from(bytes).ok())
        .map(u64::from_be_bytes)
    else {
        return;
    };
    let wanted = {
        let ds = shared.debug.debug_state.lock();
        // Wave 28: a listing from before a resume is read again.
        ds.is_thread_suspended(tid) && !ds.has_current_frames(tid)
    };
    if !wanted || !crate::runtime::interpreter::blocked_frames_readable(shared, tid) {
        return;
    }
    let _ = run_on_parked_thread(shared, None, move |shared, _thread| {
        crate::runtime::interpreter::publish_frames_of_blocked_thread(shared, tid)
    });
}

/// A registered native thread (`NativeContext::register_native_thread`: the
/// XNIO I/O loop, which blocks through `VmNativeThreadBlocker`) returning from
/// its blocking call while the debugger holds it suspended waits here until
/// it is resumed, still GC-blocked (interpreter round i1 wave 18, lane L3).
///
/// Such a thread runs no bytecode, so it never reaches the interpreter's
/// suspend point: it was counted suspended and kept dispatching I/O. HotSpot's
/// rule for a thread in native code is that a suspension lets it run on in
/// native and stops it at its next transition back into the VM; the end of a
/// blocking call is this thread's only such transition. Its frames stay what
/// they were (none: it has no interpreter frame, which is what HotSpot answers
/// for an attached thread that has not called Java). One load while no
/// suspension is in force; `leave_blocked` runs before the thread leaves the
/// blocked region, so a collection never waits for it here.
#[inline]
pub(crate) fn hold_native_thread_while_suspended(shared: &crate::vm::SharedVm, tid: u64) {
    let gates = &shared.debug.debugger_gates;
    if gates.suspension_in_force() && gates.session_attached() {
        hold_native_thread_while_suspended_slow(shared, tid);
    }
}

/// [`hold_native_thread_while_suspended`] while a suspension is in force: the
/// park loop of `interpreter::park_for_debugger` without its task queue (a
/// native thread runs no invocation) and without safepoint polls (it is
/// GC-blocked). Ends on the resume or when the session detaches (a detach
/// resumes every thread).
#[cold]
#[inline(never)]
fn hold_native_thread_while_suspended_slow(shared: &crate::vm::SharedVm, tid: u64) {
    while shared.debug.debugger_gates.session_attached()
        && shared.debug.debug_state.lock().is_thread_suspended(tid)
    {
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
}

/// `VirtualMachine.Exit` (interpreter round i1 wave 23): once its reply has
/// gone out, end the process with the debugger's exit code, as HotSpot's back
/// end does (`exitWithCode` sends the reply, then `forceExit`: the transport
/// closed and a C `exit`, no Java shutdown hooks). The command only recorded
/// the code, which nothing read, so the debuggee ran on and JDI's
/// `VirtualMachine.exit` (jdb's `exit` on a launched VM) waited for a
/// disconnect that never came. The VM's buffered stdout and stderr are
/// flushed first: HotSpot's are unbuffered, so what the program printed
/// before the exit is delivered there.
fn exit_for_debugger(shared: &crate::vm::SharedVm, code: i32) -> ! {
    tracing::info!("JDWP: VirtualMachine.Exit({code})");
    let _ = shared.natives.fd_table.flush(1);
    let _ = shared.natives.fd_table.flush(2);
    cratonvm_native_api::process_exit::exit_process(code)
}

// ---------------------------------------------------------------------------
// T6.5 — SharedVm bridge: live VM access for debugger-initiated invocation
// ---------------------------------------------------------------------------

/// Concrete [`DebuggerVmBridge`] that forwards to a live [`SharedVm`].
///
/// Wave 8: every call runs on a thread parked at an interpreter suspend
/// point ([`run_on_parked_thread`]). An invocation runs on the very thread
/// the command names, as JDWP specifies — `Thread.currentThread()`, its
/// thread-locals and the monitors it holds are that thread's — with its
/// frames pushed above the parked one, and is refused
/// (`THREAD_NOT_SUSPENDED`) when that thread is not parked at a suspend
/// point. An allocation runs on any parked thread. Until wave 8 each call
/// ran on the JDWP server thread, under an ephemeral `JvmThread` that
/// borrowed the target's id and that no collection counted, waited for or
/// scanned (the arguments and the running frames could be moved under it).
///
/// Wave 9: the invocation options are honoured
/// ([`run_invocation_on_parked_thread`]). Without `INVOKE_SINGLE_THREADED`
/// every suspended thread runs for the call and all are suspended again
/// after it; until wave 9 only the target thread ever ran, so an invoked
/// method that waited for another suspended thread (a monitor it holds, a
/// queue it fills) hung the session. And the JDWP server serves commands
/// and events while the call runs (`run_jdwp_server`), so a stop inside the
/// invoked method is reported and can be resumed.
pub struct SharedVmBridge {
    pub shared: Arc<crate::vm::SharedVm>,
}

/// How an invocation selects its method.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InvokeHow {
    /// `ClassType.InvokeMethod`.
    Static,
    /// `ObjectReference.InvokeMethod`: selected on the receiver's class, so
    /// `toString` named by `java.lang.Object`'s method id runs the override
    /// (until wave 8 the declaring class's method ran, whatever the receiver).
    Virtual,
    /// `ObjectReference.InvokeMethod` with `INVOKE_NONVIRTUAL`.
    NonVirtual,
    /// `ClassType.NewInstance` (wave 24): allocate, then run the constructor.
    Construct,
}

/// One invocation, as the parked thread runs it ([`invoke_on_parked_thread`]).
struct ParkedInvocation {
    class_id: crate::classloading::ClassId,
    method_name: String,
    descriptor: String,
    /// 0 for a static method.
    receiver_id: u64,
    args: Vec<DebuggerValue>,
    return_sig: String,
    how: InvokeHow,
    /// `ClassType.NewInstance` of an abstract class or an interface (wave
    /// 25): the class's Java name, which the `InstantiationException` the
    /// invocation throws instead of constructing carries.
    not_instantiable: Option<String>,
}

impl SharedVmBridge {
    pub fn new(shared: Arc<crate::vm::SharedVm>) -> Self {
        Self { shared }
    }

    /// The class a wire class id names, if it is loaded.
    fn loaded_class(&self, class_id: u64) -> Option<crate::classloading::ClassId> {
        let raw = u32::try_from(class_id).ok()?;
        let cid = crate::classloading::ClassId::new(raw);
        let cm = self.shared.classes.class_manager.read();
        cm.class_store.get(cid).map(|_| cid)
    }

    /// The refusal for an invocation on thread `tid`, which is not parked at
    /// a suspend point: `INVALID_THREAD`, as HotSpot's back end refuses an
    /// invocation on a thread that no event suspended (its invoker's
    /// `available` flag), which JDI turns into
    /// `IncompatibleThreadStateException`; `INVALID_OBJECT` for an id that
    /// names nothing at all (interpreter round i1 wave 43, lane L1:
    /// `tools/probes/interp/L1/L1W43RawJdwpObjectErrorAnswers.java`, measured
    /// on HotSpot 25.0.3). It answered `THREAD_NOT_SUSPENDED` for a live
    /// thread, which JDI turns into an `InternalException`, and
    /// `INVALID_THREAD` for an id naming nothing.
    fn not_parked(&self, tid: u64) -> BridgeError {
        if id_names_something(&self.shared, ids::thread_to_wire(tid)) {
            BridgeError::InvalidThread
        } else {
            BridgeError::InvalidObject
        }
    }

    /// Check an invocation and run it on its thread.
    #[allow(clippy::too_many_arguments)]
    fn invoke(
        &self,
        receiver_id: u64,
        class_id: u64,
        method_id: u64,
        thread_id: u64,
        args: &[DebuggerValue],
        return_sig: &str,
        how: InvokeHow,
        single_threaded: bool,
    ) -> Result<InvokeOutcome, BridgeError> {
        let class = self
            .loaded_class(class_id)
            .ok_or(BridgeError::InvalidClass)?;
        let (method_name, descriptor, is_static) = self
            .method_sig_for(class_id, method_id)
            .ok_or(BridgeError::InvalidMethod)?;
        if matches!(how, InvokeHow::Virtual | InvokeHow::NonVirtual) && receiver_id == 0 {
            return Err(BridgeError::InvalidObject);
        }
        // `ObjectReference.InvokeMethod` of a static method (interpreter
        // round i1 wave 43, lane L1): the method runs as a static one, the
        // object checked and then ignored, as HotSpot 25.0.3 answers it
        // (`tools/probes/interp/L1/L1W43RawJdwpObjectErrorAnswers.java`:
        // `add(4, 5)` on an instance is 9). The receiver was passed as the
        // method's first argument, which shifted every argument by one.
        let (how, receiver_id) = if is_static
            && matches!(how, InvokeHow::Virtual | InvokeHow::NonVirtual)
        {
            let known = {
                let ds = self.shared.debug.debug_state.lock();
                object_collected(&self.shared, &ds, receiver_id) == Some(false)
            };
            if !known {
                return Err(BridgeError::InvalidObject);
            }
            (InvokeHow::Static, 0)
        } else {
            (how, receiver_id)
        };
        let mut not_instantiable = None;
        if how == InvokeHow::Construct {
            // Only a constructor constructs. An abstract class or an
            // interface has no instances: HotSpot's back end runs JNI
            // `NewObjectA`, which throws `InstantiationException` naming the
            // class (`InstanceKlass::check_valid_for_instantiation`), and the
            // debugger gets it as the invocation's exception (wave 25,
            // `tools/probes/interp/L1/L1W25JdiStopMonitors.java`). Wave 24
            // refused the command (`INVALID_CLASS`) instead.
            if method_name != "<init>" {
                return Err(BridgeError::InvalidMethod);
            }
            let cm = self.shared.classes.class_manager.read();
            match cm.class_store.get(class) {
                None => return Err(BridgeError::InvalidClass),
                Some(c) if c.name.starts_with('[') => return Err(BridgeError::InvalidClass),
                Some(c) if c.is_interface() || c.is_abstract() => {
                    not_instantiable = Some(c.name.replace('/', "."));
                }
                Some(_) => {}
            }
        }
        if descriptor_arg_count(&descriptor) != Some(args.len()) {
            return Err(BridgeError::IllegalArgument);
        }
        let call = ParkedInvocation {
            class_id: class,
            method_name,
            descriptor,
            receiver_id,
            args: args.to_vec(),
            return_sig: return_sig.to_string(),
            how,
            not_instantiable,
        };
        match run_invocation_on_parked_thread(
            &self.shared,
            thread_id,
            single_threaded,
            move |shared, thread| invoke_on_parked_thread(shared, thread, call),
        ) {
            Ok(outcome) => outcome,
            Err(ParkedError::NotParked) => Err(self.not_parked(thread_id)),
            Err(ParkedError::Busy) => Err(BridgeError::AlreadyInvoking),
            Err(ParkedError::Lost) => Err(BridgeError::Internal),
        }
    }

    /// Look up a (method_name, descriptor) by the debugger-side methodID,
    /// which is `jdwp_method_id` of the name and descriptor, so overloads
    /// have distinct ids.
    /// The name, descriptor and staticness of the method `method_id` names
    /// in class `class_id`.
    fn method_sig_for(&self, class_id: u64, method_id: u64) -> Option<(String, String, bool)> {
        let ds = self.shared.debug.debug_state.lock();
        let methods = ds.class_methods.get(&class_id)?;
        let hit = methods.iter().find(|m| m.method_id.0 == method_id)?;
        // ACC_STATIC.
        Some((hit.name.clone(), hit.signature.clone(), hit.mod_bits & 0x0008 != 0))
    }
}

/// The refusal for work that needs thread `tid` parked at a suspend point
/// when it is not: `THREAD_NOT_SUSPENDED` for a live (or suspended) thread,
/// `INVALID_THREAD` for an id that names none.
pub(crate) fn unparked_thread_refusal(ds: &DebugState, tid: u64) -> BridgeError {
    if ds.thread_names.contains_key(&tid) || ds.is_thread_suspended(tid) {
        BridgeError::ThreadNotSuspended
    } else {
        BridgeError::InvalidThread
    }
}

/// The number of arguments a method descriptor declares (one per parameter,
/// `long` and `double` included), or `None` for a malformed descriptor.
fn descriptor_arg_count(descriptor: &str) -> Option<usize> {
    let params = descriptor.strip_prefix('(')?.split(')').next()?;
    let mut count = 0usize;
    let mut chars = params.chars();
    while let Some(c) = chars.next() {
        let mut elem = c;
        while elem == '[' {
            elem = chars.next()?;
        }
        if elem == 'L' {
            chars.by_ref().find(|&ch| ch == ';')?;
        }
        count += 1;
    }
    Some(count)
}

/// Run one debugger invocation on the parked thread `thread` (wave 8; see
/// [`SharedVmBridge`]). The ids are resolved here, on the registered thread,
/// and the object arguments are pinned for the call (the debugger's handles
/// are weak); the result or the thrown exception is exported before this
/// thread reaches another safepoint, so it cannot move in between.
fn invoke_on_parked_thread(
    shared: &crate::vm::SharedVm,
    thread: &mut crate::threading::jvm_thread::JvmThread,
    call: ParkedInvocation,
) -> Result<InvokeOutcome, BridgeError> {
    use crate::error::MethodCallFailed;
    use crate::types::Value;
    let mut vm_args = Vec::with_capacity(call.args.len() + 1);
    let receiver = {
        let ds = shared.debug.debug_state.lock();
        let receiver = if call.receiver_id == 0 {
            None
        } else {
            Some(object_for_id(shared, &ds, call.receiver_id).ok_or(BridgeError::InvalidObject)?)
        };
        if let Some(r) = receiver {
            vm_args.push(Value::Object(Some(r)));
        }
        for a in &call.args {
            vm_args.push(inspect::debugger_value_to_vm(shared, &ds, a)?);
        }
        receiver
    };
    let target = match (call.how, receiver) {
        (InvokeHow::Virtual, Some(r)) => shared.mem.heap.class_id_of(r),
        _ => call.class_id,
    };
    let pin_base = thread.native_pin_roots.len();
    for v in &vm_args {
        if let Value::Object(Some(o)) = v {
            thread.native_pin_roots.push(*o);
        }
    }
    let result = match call.how {
        InvokeHow::NonVirtual => crate::vm::invoke_on_class_shared_no_retarget(
            shared,
            thread,
            target,
            &call.method_name,
            &call.descriptor,
            &vm_args,
        ),
        // Wave 24: `ClassType.NewInstance` allocates and runs the
        // constructor as JNI `NewObjectA` does (class initialization, the
        // finalizer registration, the new object pinned across `<init>`),
        // on this thread. The answer is the object, wherever `<init>` left it.
        // Wave 25: an abstract class or an interface: JNI `NewObjectA`'s
        // `InstantiationException`, thrown by the invocation.
        InvokeHow::Construct if call.not_instantiable.is_some() => {
            let name = call.not_instantiable.as_deref();
            match crate::runtime::exceptions::create_exception_object(
                shared,
                thread,
                "java/lang/InstantiationException",
                name,
            ) {
                Ok(exc) => Err(MethodCallFailed::ExceptionThrown(exc)),
                Err(failed) => Err(failed),
            }
        }
        InvokeHow::Construct => {
            use cratonvm_native_api::NativeContext as _;
            let mut ctx = crate::vm::NativeContextImpl {
                shared,
                thread: &mut *thread,
            };
            ctx.new_object_initialized_with_class_id(call.class_id, &call.descriptor, &vm_args)
        }
        InvokeHow::Static | InvokeHow::Virtual => crate::vm::invoke_on_class_shared(
            shared,
            thread,
            target,
            &call.method_name,
            &call.descriptor,
            &vm_args,
        ),
    };
    thread.native_pin_roots.truncate(pin_base);
    // No safepoint from here to the export: the result stays where it is.
    let cm = shared.classes.class_manager.read();
    let mut ds = shared.debug.debug_state.lock();
    match result {
        Ok(None) => Ok(InvokeOutcome::returned(DebuggerValue::Void)),
        Ok(Some(v)) => Ok(InvokeOutcome::returned(inspect::vm_value_to_debugger(
            shared,
            &cm,
            &mut ds,
            v,
            &call.return_sig,
        ))),
        Err(MethodCallFailed::ExceptionThrown(exc)) => Ok(InvokeOutcome::threw(export_object(
            shared,
            &mut ds,
            exc,
            ObjectExport::Sent,
        ))),
        // Until wave 8 this answered "threw object 1", an id naming nothing
        // (or a thread).
        Err(MethodCallFailed::InternalError(_)) => Err(BridgeError::Internal),
    }
}

impl DebuggerVmBridge for SharedVmBridge {
    fn invoke_static(
        &self,
        class_id: u64,
        method_id: u64,
        thread_id: u64,
        args: &[DebuggerValue],
        return_sig: &str,
        single_threaded: bool,
    ) -> Result<InvokeOutcome, BridgeError> {
        self.invoke(
            0,
            class_id,
            method_id,
            thread_id,
            args,
            return_sig,
            InvokeHow::Static,
            single_threaded,
        )
    }

    fn invoke_instance(
        &self,
        receiver_id: u64,
        class_id: u64,
        method_id: u64,
        thread_id: u64,
        args: &[DebuggerValue],
        return_sig: &str,
        non_virtual: bool,
        single_threaded: bool,
    ) -> Result<InvokeOutcome, BridgeError> {
        let how = if non_virtual {
            InvokeHow::NonVirtual
        } else {
            InvokeHow::Virtual
        };
        self.invoke(
            receiver_id,
            class_id,
            method_id,
            thread_id,
            args,
            return_sig,
            how,
            single_threaded,
        )
    }

    /// `ClassType.NewInstance` (wave 24): an invocation of the constructor
    /// on its thread whose answer is the new object, tagged by its class.
    fn new_instance(
        &self,
        class_id: u64,
        method_id: u64,
        thread_id: u64,
        args: &[DebuggerValue],
        single_threaded: bool,
    ) -> Result<InvokeOutcome, BridgeError> {
        self.invoke(
            0,
            class_id,
            method_id,
            thread_id,
            args,
            "Ljava/lang/Object;",
            InvokeHow::Construct,
            single_threaded,
        )
    }

    /// `ArrayType.NewInstance`. Wave 8: the element type comes from the
    /// array class the id names (it came from a table nothing filled, so
    /// every array was an `Object[]` whatever its type said, headed by class 0
    /// when the id was unknown), and the allocation runs on a parked thread
    /// through the collecting allocator — it was a raw `alloc_array` on the
    /// server thread, which no collection waited for and which aborts the VM
    /// on an exhausted heap.
    fn new_array(&self, array_type_id: u64, length: i32) -> Result<u64, BridgeError> {
        use crate::memory::heap::ArrayElementType as E;
        let class = self
            .loaded_class(array_type_id)
            .ok_or(BridgeError::InvalidClass)?;
        let element_type = {
            let cm = self.shared.classes.class_manager.read();
            let name = cm
                .class_store
                .get(class)
                .map(|c| c.name.to_string())
                .unwrap_or_default();
            match name.as_bytes() {
                [b'[', b'Z', ..] => E::Boolean,
                [b'[', b'B', ..] => E::Byte,
                [b'[', b'C', ..] => E::Char,
                [b'[', b'S', ..] => E::Short,
                [b'[', b'I', ..] => E::Int,
                [b'[', b'J', ..] => E::Long,
                [b'[', b'F', ..] => E::Float,
                [b'[', b'D', ..] => E::Double,
                [b'[', b'L' | b'[', ..] => E::Reference,
                _ => return Err(BridgeError::InvalidClass),
            }
        };
        let length = usize::try_from(length).map_err(|_| BridgeError::IllegalArgument)?;
        let allocated = run_on_parked_thread(
            &self.shared,
            None,
            move |shared, thread| -> Result<u64, BridgeError> {
                let array = crate::runtime::interpreter::gc_alloc_array(
                    shared,
                    thread,
                    class,
                    element_type,
                    length,
                )
                .map_err(|_| BridgeError::Internal)?;
                // No safepoint between the allocation's return and the export.
                let mut ds = shared.debug.debug_state.lock();
                Ok(export_object(shared, &mut ds, array, ObjectExport::Sent))
            },
        );
        match allocated {
            Ok(id) => id,
            // `Busy` needs a thread named; any idle thread would have done.
            Err(ParkedError::NotParked | ParkedError::Busy) => Err(BridgeError::ThreadNotSuspended),
            Err(ParkedError::Lost) => Err(BridgeError::Internal),
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// Interpreter round i1 wave 43, lane L1: the `SourceDebugExtension` of a
    /// class file is read back byte for byte, and a file without one (or
    /// one that does not parse) has none.
    #[test]
    fn the_source_debug_extension_is_read_from_the_class_file() {
        fn utf8(out: &mut Vec<u8>, s: &str) {
            out.push(1);
            out.extend_from_slice(&u16::try_from(s.len()).unwrap_or(0).to_be_bytes());
            out.extend_from_slice(s.as_bytes());
        }
        let class_file = |smap: Option<&[u8]>| {
            let mut c = vec![0xCA, 0xFE, 0xBA, 0xBE, 0, 0, 0, 52];
            c.extend_from_slice(&7u16.to_be_bytes()); // constant_pool_count
            utf8(&mut c, "H");
            c.extend_from_slice(&[7, 0, 1]);
            utf8(&mut c, "java/lang/Object");
            c.extend_from_slice(&[7, 0, 3]);
            utf8(&mut c, "SourceDebugExtension");
            utf8(&mut c, "Unused");
            c.extend_from_slice(&[0x00, 0x21, 0, 2, 0, 4, 0, 0, 0, 0, 0, 0]);
            match smap {
                Some(bytes) => {
                    c.extend_from_slice(&[0, 1, 0, 5]);
                    c.extend_from_slice(&u32::try_from(bytes.len()).unwrap_or(0).to_be_bytes());
                    c.extend_from_slice(bytes);
                }
                None => c.extend_from_slice(&[0, 0]),
            }
            c
        };
        let smap = b"SMAP\nH.kt\nKotlin\n*S Kotlin\n*F\n+ 1 H.kt\nH.kt\n*L\n1#1,2:1\n*E\n";
        assert_eq!(
            source_debug_extension_of(&class_file(Some(smap))).as_deref(),
            Some(&smap[..])
        );
        assert_eq!(source_debug_extension_of(&class_file(None)), None);
        assert_eq!(source_debug_extension_of(&[0xCA, 0xFE]), None);
        // The file names of every stratum's file section, each once.
        let two = b"SMAP\nH.kt\nKotlin\n*S Kotlin\n*F\n+ 1 H.kt\np/H.kt\n+ 2 Inline.kt\np/Inline.kt\n\
            *L\n1#1,2:1\n*S KotlinDebug\n*F\n+ 1 H.kt\np/H.kt\n3 Other.kt\n*L\n1#1:1\n*E\n";
        assert_eq!(smap_file_names(two), vec!["H.kt", "Inline.kt", "Other.kt"]);
        assert!(smap_file_names(b"SMAP\nH.kt\nKotlin\n*E\n").is_empty());
    }

    /// Interpreter round i1 wave 43, lane L1: a leading thread or class id
    /// that names nothing in any of the server's id spaces is
    /// `INVALID_OBJECT`, as HotSpot answers; one that names something of
    /// another kind is left to the handler (`INVALID_THREAD` /
    /// `INVALID_CLASS`).
    #[test]
    fn an_id_naming_nothing_is_invalid_object() {
        use commands::{CS_REF_TYPE, CS_STACK_FRAME, CS_THREAD_REF, ERR_INVALID_OBJECT};
        let shared = Arc::new(crate::vm::SharedVm::new(crate::config::VmConfig::default()));
        let refusal = |set: u8, vm_id: u64| {
            unknown_id_refusal(&shared, set, &vm_id.to_be_bytes()).map(|r| r.error_code)
        };
        assert_eq!(refusal(CS_THREAD_REF, 0x7fff_fff0), Some(ERR_INVALID_OBJECT));
        assert_eq!(refusal(CS_STACK_FRAME, 0x7fff_fff0), Some(ERR_INVALID_OBJECT));
        assert_eq!(refusal(CS_REF_TYPE, 0x7fff_fff0), Some(ERR_INVALID_OBJECT));
        assert_eq!(refusal(CS_THREAD_REF, ids::NO_VM_ID), None, "null is the handler's");
        assert_eq!(refusal(CS_THREAD_REF, commands::SYSTEM_THREAD_GROUP_ID), None);
        assert_eq!(refusal(commands::CS_VM, 0x7fff_fff0), None, "no leading id");
        {
            let mut ds = shared.debug.debug_state.lock();
            ds.thread_names.insert(0x7fff_fff1, "t".to_string());
            ds.class_signatures.insert(0x7fff_fff2, "LX;".to_string());
        }
        // A thread's id sent to a class command, and a class's to a thread
        // command, name something: the handler refuses them by kind.
        assert_eq!(refusal(CS_THREAD_REF, 0x7fff_fff1), None);
        assert_eq!(refusal(CS_REF_TYPE, 0x7fff_fff1), None);
        assert_eq!(refusal(CS_THREAD_REF, 0x7fff_fff2), None);
        assert_eq!(refusal(CS_REF_TYPE, 0x7fff_fff2), None);
        assert_eq!(refusal(CS_REF_TYPE, 0x7fff_fff3), Some(ERR_INVALID_OBJECT));
    }

    /// Wave 6: a forged object id is `INVALID_OBJECT`, never a wild
    /// reference. Wave 7: an id is a handle-table entry, not an address — it
    /// follows its object through a moving collection, the moved object keeps
    /// its id, an object that later lands on the old address gets another,
    /// and a released id is `INVALID_OBJECT` with its handle freed. Wave 8:
    /// the handle is a WEAK global — the weak sweep moves it, a collection
    /// that finds the object dead clears it, and the id then reads as
    /// collected rather than keeping the object alive.
    #[test]
    fn debugger_object_ids_follow_their_object_through_a_move() {
        let shared = Arc::new(crate::vm::SharedVm::new(crate::config::VmConfig::default()));
        let resolve = |id: u64| {
            let ds = shared.debug.debug_state.lock();
            object_for_id(&shared, &ds, id)
        };
        let to_vm = |v: DebuggerValue| {
            let ds = shared.debug.debug_state.lock();
            inspect::debugger_value_to_vm(&shared, &ds, &v)
        };
        assert_eq!(resolve(0x1000), None);
        assert_eq!(
            to_vm(DebuggerValue::Object(0x7ff0_0000_1000)),
            Err(BridgeError::InvalidObject)
        );
        assert_eq!(
            to_vm(DebuggerValue::Object(0)),
            Ok(crate::types::Value::Object(None)),
            "0 is the null object"
        );
        let alloc = || {
            shared.mem.heap.try_alloc_array_full(
                crate::classloading::ClassId::new(0),
                cratonvm_types::ArrayElementType::Int,
                4,
            )
        };
        let (Some(arr), Some(elsewhere)) = (alloc(), alloc()) else {
            return; // no room in a stripped test heap: nothing to resolve
        };
        let strong_before = shared.natives.jni_global_refs.lock().count();
        let id = {
            let mut ds = shared.debug.debug_state.lock();
            let id = export_object(&shared, &mut ds, arr, ObjectExport::Sent);
            let again = export_object(&shared, &mut ds, arr, ObjectExport::Sent);
            assert_eq!(id, again, "one object, one id");
            id
        };
        assert!(id >= ids::HEAP_OBJECT_ID_BASE, "never a thread id: {id:#x}");
        assert_eq!(resolve(id), Some(arr));
        let handle = shared.debug.debug_state.lock().objects.handle_of(id);
        let kind = |h: Option<u64>| h.and_then(|h| shared.natives.jni_global_refs.lock().kind(h));
        assert_eq!(
            kind(handle),
            Some(crate::native::jni::JniGlobalKind::Weak),
            "an id does not keep its object alive"
        );
        assert_eq!(
            shared.natives.jni_global_refs.lock().count(),
            strong_before,
            "no strong global reference"
        );
        // A collection moves `arr` to `elsewhere`'s address: the weak-global
        // sweep the pause applies, then the pause's generation bump.
        let mut moved = cratonvm_types::PointerMap::new();
        // Cast: addresses are the pointer map's keys and values.
        moved.insert(arr.as_ptr() as usize, elsewhere.as_ptr() as usize);
        let _ = crate::native::jni::sweep_weak_global_refs(&shared, &moved);
        shared
            .mem
            .gc_barrier
            .gc_generation
            .fetch_add(1, std::sync::atomic::Ordering::AcqRel);
        assert_eq!(resolve(id), Some(elsewhere), "the id follows");
        let other = {
            let mut ds = shared.debug.debug_state.lock();
            assert_eq!(
                export_object(&shared, &mut ds, elsewhere, ObjectExport::Sent),
                id,
                "the moved object keeps its id"
            );
            let other = export_object(&shared, &mut ds, arr, ObjectExport::Sent);
            assert_ne!(
                other, id,
                "whatever is at the old address now is another object"
            );
            other
        };
        // A collection finds the moved object dead: its id reads as
        // collected (`IsCollected`), resolves to nothing (`INVALID_OBJECT`),
        // and stays until disposed.
        // Cast: the address the mark verdict is asked about.
        let dead = elsewhere.as_ptr() as usize;
        let marked = |a: usize| a != dead;
        let _ = crate::native::jni::clear_weak_global_refs_unmarked(&shared, &marked, &[]);
        {
            let ds = shared.debug.debug_state.lock();
            assert_eq!(object_for_id(&shared, &ds, id), None);
            assert_eq!(object_collected(&shared, &ds, id), Some(true));
            assert_eq!(object_collected(&shared, &ds, other), Some(false));
            assert_eq!(object_collected(&shared, &ds, 0x1000), None);
        }
        {
            let mut ds = shared.debug.debug_state.lock();
            assert!(
                !set_collection_enabled(&shared, &mut ds, id, false),
                "too late to keep a collected object"
            );
            ds.objects.release_all();
            release_disposed_objects(&shared, &mut ds);
        }
        assert_eq!(resolve(id), None);
        assert_eq!(kind(handle), None, "released ids free their handles");
    }

    /// Wave 8: an invocation checks its argument count against the method,
    /// and is refused — never run on the server thread — when its thread is
    /// not parked at a suspend point: since wave 43 `INVALID_THREAD` for a
    /// live thread and `INVALID_OBJECT` for an id naming nothing, as HotSpot
    /// answers.
    #[test]
    fn invocations_are_refused_for_threads_not_parked() {
        assert_eq!(descriptor_arg_count("()V"), Some(0));
        assert_eq!(descriptor_arg_count("(IJ[[Ljava/lang/String;D)V"), Some(4));
        assert_eq!(descriptor_arg_count("(Ljava/lang/Object;[I)Z"), Some(2));
        assert_eq!(descriptor_arg_count("(Ljava/lang/Object"), None);
        assert_eq!(descriptor_arg_count("V"), None);
        let shared = Arc::new(crate::vm::SharedVm::new(crate::config::VmConfig::default()));
        assert_eq!(
            run_on_parked_thread(&shared, None, |_, _| ()),
            Err(ParkedError::NotParked)
        );
        assert_eq!(
            run_on_parked_thread(&shared, Some(3), |_, _| ()),
            Err(ParkedError::NotParked)
        );
        let bridge = SharedVmBridge::new(Arc::clone(&shared));
        assert_eq!(bridge.not_parked(0x7fff_fff3), BridgeError::InvalidObject);
        shared
            .debug
            .debug_state
            .lock()
            .thread_names
            .insert(0x7fff_fff3, "worker".to_string());
        assert_eq!(bridge.not_parked(0x7fff_fff3), BridgeError::InvalidThread);
        assert!(!debugger_hooks_suppressed());
    }

    /// Interpreter round i1 wave 38, lane L1: an event set applied as
    /// `EVENT_THREAD` reports the byte its first such event's request sent
    /// when JDWP does not define it; every other set reports its policy.
    #[test]
    fn an_event_set_echoes_an_undefined_suspend_policy_byte() {
        use events::SuspendPolicy;
        assert_eq!(event_set_policy_byte(SuspendPolicy::EventThread, Some(7)), 7);
        assert_eq!(event_set_policy_byte(SuspendPolicy::EventThread, Some(1)), 1);
        assert_eq!(event_set_policy_byte(SuspendPolicy::EventThread, None), 1);
        assert_eq!(event_set_policy_byte(SuspendPolicy::All, Some(255)), 2);
        assert_eq!(event_set_policy_byte(SuspendPolicy::None, None), 0);
        let mut mgr = events::EventManager::new();
        let odd = mgr.set_event_request(
            events::EventKind::MethodEntry,
            SuspendPolicy::from_wire(255),
            vec![],
        );
        mgr.note_wire_suspend_policy(odd, 255);
        let plain = mgr.set_event_request(
            events::EventKind::MethodEntry,
            SuspendPolicy::from_wire(1),
            vec![],
        );
        mgr.note_wire_suspend_policy(plain, 1);
        assert_eq!(mgr.wire_suspend_policy(odd), Some(255));
        assert_eq!(mgr.wire_suspend_policy(plain), None);
        // Kept after the request goes: an event it matched may be in flight.
        assert!(mgr.clear_event_request(odd));
        assert_eq!(mgr.wire_suspend_policy(odd), Some(255));
        mgr.clear_all();
        assert_eq!(mgr.wire_suspend_policy(odd), None);
    }

    /// Interpreter round i1 wave 22, lane L1: the events one hook reports at
    /// one point are one JDWP event set. The thread is suspended once, with
    /// the strongest policy of the set, and the set crosses the channel
    /// contiguously, every event but the last marked `set_follows`, so the
    /// drain sends one composite packet.
    #[test]
    fn one_event_set_suspends_once_and_crosses_the_channel_as_one_set() {
        use events::{EventKind, SuspendPolicy};
        let shared = crate::vm::SharedVm::new(crate::config::VmConfig::default());
        let (tx, rx) = std::sync::mpsc::channel();
        *shared.debug.debug_event_tx.lock().unwrap() = Some(tx);
        assert_eq!(
            strongest_suspend_policy(std::iter::empty()),
            SuspendPolicy::None
        );
        assert_eq!(
            strongest_suspend_policy([SuspendPolicy::EventThread, SuspendPolicy::None]),
            SuspendPolicy::EventThread
        );
        assert_eq!(
            strongest_suspend_policy([
                SuspendPolicy::EventThread,
                SuspendPolicy::All,
                SuspendPolicy::None
            ]),
            SuspendPolicy::All
        );
        {
            let mut ds = shared.debug.debug_state.lock();
            assert!(ds.apply_event_set_policy(SuspendPolicy::EventThread, 5));
            assert_eq!(ds.suspend_count(5), 1, "once, however many events");
            assert!(!ds.apply_event_set_policy(SuspendPolicy::None, 5));
            assert_eq!(ds.suspend_count(5), 1);
            ds.resume_all();
        }
        let event = |request_id: u32, kind: EventKind| DebugEvent {
            kind,
            request_id,
            suspend_policy: SuspendPolicy::EventThread,
            thread_id: 5,
            class_id: 1,
            method_id: 2,
            offset: 3,
            extra: Vec::new(),
            set_follows: false,
        };
        send_event_set(
            &shared,
            vec![
                event(1, EventKind::Breakpoint),
                event(2, EventKind::Breakpoint),
                event(3, EventKind::SingleStep),
            ],
        );
        send_event_set(&shared, Vec::new());
        send_event_set(&shared, vec![event(4, EventKind::Breakpoint)]);
        let got: Vec<(u32, bool)> = rx
            .try_iter()
            .map(|e| (e.request_id, e.set_follows))
            .collect();
        assert_eq!(got, vec![(1, true), (2, true), (3, false), (4, false)]);
    }

    /// Wave 9: a thread event reports once per matching request, honours
    /// `Count` and `ThreadOnly`, and applies its suspend policy before it is
    /// queued (it was reported and not applied, and only the first matching
    /// request reported).
    #[test]
    fn thread_events_apply_their_suspend_policy() {
        use events::{EventKind, EventModifier, SuspendPolicy};
        let shared = crate::vm::SharedVm::new(crate::config::VmConfig::default());
        let (tx, rx) = std::sync::mpsc::channel();
        *shared.debug.debug_event_tx.lock().unwrap() = Some(tx);
        {
            let mut ds = shared.debug.debug_state.lock();
            ds.events
                .set_event_request(EventKind::ThreadStart, SuspendPolicy::EventThread, vec![]);
            ds.events.set_event_request(
                EventKind::ThreadStart,
                SuspendPolicy::None,
                vec![EventModifier::Count(2)],
            );
            ds.events.set_event_request(
                EventKind::ThreadStart,
                SuspendPolicy::All,
                vec![EventModifier::ThreadOnly { thread_id: 9 }],
            );
        }
        send_thread_event(&shared, EventKind::ThreadStart, 7);
        {
            let ds = shared.debug.debug_state.lock();
            assert_eq!(ds.suspend_count(7), 1, "SUSPEND_EVENT_THREAD applied");
            assert_eq!(
                ds.suspend_count(8),
                0,
                "the SUSPEND_ALL request is for thread 9"
            );
        }
        assert!(
            shared
                .debug
                .breakpoints_active
                .load(std::sync::atomic::Ordering::Relaxed),
            "a suspension arms the interpreter's gate"
        );
        send_thread_event(&shared, EventKind::ThreadStart, 8);
        let got: Vec<(u32, SuspendPolicy, u64)> = rx
            .try_iter()
            .map(|e| (e.request_id, e.suspend_policy, e.thread_id))
            .collect();
        assert_eq!(
            got,
            vec![
                (1, SuspendPolicy::EventThread, 7),
                (1, SuspendPolicy::EventThread, 8),
                (2, SuspendPolicy::None, 8),
            ],
            "every matching request, the Count one on its second hit"
        );
        send_thread_event(&shared, EventKind::ThreadDeath, 7);
        assert!(rx.try_recv().is_err(), "no ThreadDeath request");
    }

    /// Wave 9: the JDWP invocation protocol on the suspend counts. Without
    /// `INVOKE_SINGLE_THREADED` every thread is resumed once (a thread
    /// suspended twice stays suspended) and all are suspended after the call
    /// whatever they were before; with it, only the invoking thread.
    #[test]
    fn invocations_release_and_resuspend_like_hotspot() {
        let mut ds = DebugState::new();
        ds.suspend_thread(1); // stopped by a SUSPEND_EVENT_THREAD breakpoint
        ds.suspend_thread(2); // ThreadReference.Suspend
        ds.release_for_invocation(1, false);
        assert!(!ds.any_suspension(), "every thread runs for the call");
        ds.resuspend_after_invocation(1, false);
        assert_eq!(
            (
                ds.suspend_count(1),
                ds.suspend_count(2),
                ds.suspend_count(3)
            ),
            (1, 1, 1),
            "all suspended after it, a thread that was running too"
        );
        ds.release_for_invocation(1, true);
        assert_eq!((ds.suspend_count(1), ds.suspend_count(2)), (0, 1));
        ds.resuspend_after_invocation(1, true);
        assert_eq!((ds.suspend_count(1), ds.suspend_count(2)), (1, 1));
        ds.suspend_thread(2);
        ds.release_for_invocation(1, false);
        assert_eq!(
            (ds.suspend_count(1), ds.suspend_count(2)),
            (0, 1),
            "the implicit resume is one resume"
        );
    }

    /// Wave 9: work handed to a parked thread, with a stand-in for
    /// `interpreter::park_for_debugger` (no bytecode). An invocation runs with
    /// the suspend point live and the JDWP protocol applied on the invoking
    /// thread — single-threaded: only it runs; by default: every thread runs,
    /// and all are suspended again before the answer. While it runs the
    /// thread is busy (`ALREADY_INVOKING`); a stop inside it parks the thread
    /// again one level deeper, where it takes work and is resumed, and the
    /// outer park stands. An invocation still running when the debugger
    /// detaches re-suspends nothing.
    #[test]
    fn parked_work_nests_and_invocations_resume_and_resuspend() {
        use crate::threading::jvm_thread::{JvmThread, ThreadId};
        use std::sync::mpsc;
        use std::time::{Duration, Instant};

        fn fake_park(shared: &crate::vm::SharedVm, thread: &mut JvmThread) {
            let tid = thread.thread_id.0;
            // A failing test must not hang: the park gives up at a deadline.
            let deadline = Instant::now() + Duration::from_secs(30);
            shared.debug.debug_state.lock().note_parked(tid, true);
            loop {
                let task = {
                    let mut ds = shared.debug.debug_state.lock();
                    match ds.take_parked_task(tid) {
                        Some(task) => Some(task),
                        None if !ds.is_thread_suspended(tid) || Instant::now() > deadline => {
                            ds.note_parked(tid, false);
                            break;
                        }
                        None => None,
                    }
                };
                match task {
                    Some(task) => {
                        let reply = run_parked_task(shared, thread, task);
                        shared.debug.debug_state.lock().note_task_done(tid);
                        reply.send();
                    }
                    None => std::thread::sleep(Duration::from_millis(1)),
                }
            }
        }

        let shared = Arc::new(crate::vm::SharedVm::new(crate::config::VmConfig::default()));
        let sh: &crate::vm::SharedVm = &shared;
        let counts = || {
            let ds = sh.debug.debug_state.lock();
            (ds.suspend_count(5), ds.suspend_count(6))
        };
        {
            let mut ds = sh.debug.debug_state.lock();
            // Stopped by a SUSPEND_EVENT_THREAD event (wave 44: only an event
            // park takes an invocation).
            ds.apply_event_set_policy(events::SuspendPolicy::EventThread, 5);
            ds.suspend_thread(6); // ThreadReference.Suspend; not parked
        }
        std::thread::scope(|s| {
            let vm_thread =
                s.spawn(move || fake_park(sh, &mut JvmThread::new(ThreadId(5), "w9-parked")));
            let deadline = Instant::now() + Duration::from_secs(30);
            while !sh.debug.debug_state.lock().is_parked(5) {
                assert!(Instant::now() < deadline, "thread 5 never parked");
                std::thread::sleep(Duration::from_millis(1));
            }
            assert_eq!(
                run_on_parked_thread(sh, Some(6), |_, _| ()),
                Err(ParkedError::NotParked)
            );

            // INVOKE_SINGLE_THREADED: only the invoking thread runs, and its
            // events are live.
            let during = run_invocation_on_parked_thread(sh, 5, true, |shared, _| {
                let ds = shared.debug.debug_state.lock();
                (
                    ds.suspend_count(5),
                    ds.suspend_count(6),
                    debugger_hooks_suppressed(),
                )
            });
            assert_eq!(during, Ok((0, 1, false)));
            assert_eq!(counts(), (1, 1));

            // The default, with a stop inside the invoked method.
            let (running_tx, running_rx) = mpsc::channel::<()>();
            let (checked_tx, checked_rx) = mpsc::channel::<()>();
            let debugger = s.spawn(move || {
                let deadline = Instant::now() + Duration::from_secs(30);
                let _ = running_rx.recv_timeout(Duration::from_secs(30));
                let busy = run_on_parked_thread(sh, Some(5), |_, _| ());
                let _ = checked_tx.send(());
                let nested = loop {
                    match run_on_parked_thread(sh, Some(5), |_, thread| thread.thread_id.0) {
                        Err(ParkedError::Busy) if Instant::now() < deadline => {
                            std::thread::sleep(Duration::from_millis(1));
                        }
                        other => break other,
                    }
                };
                sh.debug.debug_state.lock().resume_thread(5);
                (busy, nested)
            });
            let answer = run_invocation_on_parked_thread(sh, 5, false, move |shared, thread| {
                let during = {
                    let ds = shared.debug.debug_state.lock();
                    (ds.suspend_count(5), ds.suspend_count(6))
                };
                let _ = running_tx.send(());
                let _ = checked_rx.recv_timeout(Duration::from_secs(30));
                // A breakpoint with SUSPEND_EVENT_THREAD inside the method.
                shared.debug.debug_state.lock().suspend_thread(5);
                fake_park(shared, thread);
                during
            });
            let (busy, nested) = debugger.join().expect("the debugger thread");
            assert_eq!(answer, Ok((0, 0)), "every thread ran for the call");
            assert_eq!(busy, Err(ParkedError::Busy), "invoking: ALREADY_INVOKING");
            assert_eq!(nested, Ok(5), "the nested stop takes work");
            assert_eq!(counts(), (1, 1), "all suspended again");
            assert!(
                sh.debug.debug_state.lock().is_parked(5),
                "the outer park stands"
            );
            assert_eq!(
                run_on_parked_thread(sh, None, |_, thread| thread.thread_id.0),
                Ok(5)
            );

            // The debugger detaches while an invocation runs.
            let detached = run_invocation_on_parked_thread(sh, 5, false, |shared, _| {
                shared.debug.debug_state.lock().resume_all();
            });
            assert_eq!(detached, Ok(()));
            assert_eq!(counts(), (0, 0), "nothing re-suspended for a gone session");
            vm_thread.join().expect("the parked thread");
        });
        assert!(!sh.debug.debug_state.lock().is_parked(5), "left on resume");
    }

    /// Wave 7: `publish_debugger_gates` arms the per-method summary — a
    /// breakpoint concerns its own method only, a step request or a
    /// suspension every method — and the VM-wide gate with it.
    #[test]
    fn debugger_gates_narrow_to_the_methods_holding_breakpoints() {
        use std::sync::atomic::Ordering;
        let shared = crate::vm::SharedVm::new(crate::config::VmConfig::default());
        let gates = &shared.debug.debugger_gates;
        let bp_method = jdwp_method_id("hot", "()V");
        let mut ds = DebugState::new();
        publish_debugger_gates(&shared, &ds);
        assert!(!shared.debug.breakpoints_active.load(Ordering::Relaxed));
        let g0 = gates.generation();
        ds.events.set_event_request(
            events::EventKind::Breakpoint,
            events::SuspendPolicy::None,
            vec![events::EventModifier::LocationOnly {
                class_id: 70,
                method_id: bp_method,
                offset: 0,
            }],
        );
        publish_debugger_gates(&shared, &ds);
        assert!(gates.generation() > g0);
        assert!(shared.debug.breakpoints_active.load(Ordering::Relaxed));
        assert!(gates.concerns_method(70, "hot", "()V"));
        assert!(!gates.concerns_method(70, "cold", "()V"), "same class");
        assert!(!gates.concerns_method(70 + 64, "hot", "()V"), "same bit");
        assert!(!gates.concerns_method(71, "other", "()V"));
        // Wave 39: the compile doors' question (no body, splice or bind of a
        // method holding a breakpoint).
        let (hot_class, other_class) = (
            crate::classloading::ClassId::new(70),
            crate::classloading::ClassId::new(71),
        );
        assert!(crate::runtime::interpreter::breakpoint_bars_compiling(
            &shared, hot_class, "hot", "()V"
        ));
        assert!(!crate::runtime::interpreter::breakpoint_bars_compiling(
            &shared, hot_class, "cold", "()V"
        ));
        ds.suspend_thread(5);
        publish_debugger_gates(&shared, &ds);
        assert!(gates.concerns_method(71, "other", "()V"), "a suspension");
        assert!(
            !gates.holds_breakpoint(71, "other", "()V"),
            "a suspension holds no breakpoint"
        );
        assert!(!crate::runtime::interpreter::breakpoint_bars_compiling(
            &shared,
            other_class,
            "other",
            "()V"
        ));
        ds.resume_all();
        ds.events.clear_all();
        publish_debugger_gates(&shared, &ds);
        assert!(!shared.debug.breakpoints_active.load(Ordering::Relaxed));
        assert!(!gates.concerns_method(70, "hot", "()V"));
        assert!(!crate::runtime::interpreter::breakpoint_bars_compiling(
            &shared, hot_class, "hot", "()V"
        ));
    }

    /// Wave 7: the JIT's interpreter→compiled doors stand down for a method
    /// holding a breakpoint, not for every method of the VM.
    #[test]
    fn the_jit_stands_down_only_for_methods_the_debugger_concerns() {
        use crate::runtime::interpreter::jit_bridge::jvmti_requires_interpreter_for;
        let shared = crate::vm::SharedVm::new(crate::config::VmConfig::default());
        if crate::runtime::jvmti::interp_only_events_active_for_vm(shared.vm_identity) {
            return; // a JVMTI listener already keeps everything interpreted
        }
        let cached = |name: &str| {
            cratonvm_jit_api::CachedBytecodeMethod::from_parts(
                cratonvm_jit_api::CachedMethodParts {
                    declaring_class_id: crate::classloading::ClassId::new(70),
                    class_name: Arc::from("cratonvm/test/Debugged"),
                    method_name: Arc::from(name),
                    method_descriptor: Arc::from("()V"),
                    source_file: None,
                    code: Arc::from(vec![0xb1u8, 0, 0]),
                    exception_table: Arc::from(Vec::new()),
                    max_stack: 0,
                    max_locals: 0,
                    num_params: 0,
                    is_synchronized: false,
                    is_static: true,
                },
            )
        };
        let (hot, cold) = (cached("hot"), cached("cold"));
        let mut ds = DebugState::new();
        ds.events.set_event_request(
            events::EventKind::Breakpoint,
            events::SuspendPolicy::None,
            vec![events::EventModifier::LocationOnly {
                class_id: 70,
                method_id: jdwp_method_id("hot", "()V"),
                offset: 0,
            }],
        );
        publish_debugger_gates(&shared, &ds);
        assert!(jvmti_requires_interpreter_for(&shared, &hot));
        assert!(!jvmti_requires_interpreter_for(&shared, &cold));
        ds.suspend_all();
        publish_debugger_gates(&shared, &ds);
        assert!(
            jvmti_requires_interpreter_for(&shared, &cold),
            "a suspension"
        );
    }

    /// Wave 40 (lane L1): a breakpoint in a JDK class's method keeps every
    /// method interpreted and the tier-up strides closed while it stands,
    /// because a caller compiled after it could expand the method as a JIT
    /// call-site intrinsic; a breakpoint in a program's class concerns its
    /// own method only, as before.
    #[test]
    fn a_breakpoint_in_a_jdk_class_keeps_every_method_interpreted() {
        let shared = crate::vm::SharedVm::new(crate::config::VmConfig::default());
        let gates = &shared.debug.debugger_gates;
        let max = jdwp_method_id("max", "(JJ)J");
        let bp = |class_id: u64, method_id: u64| {
            vec![events::EventModifier::LocationOnly {
                class_id,
                method_id,
                offset: 0,
            }]
        };
        let mut ds = DebugState::new();
        ds.class_signatures.insert(80, "Lpkg/Hot;".to_string());
        ds.class_signatures.insert(81, "Ljava/lang/Math;".to_string());

        // A program's class: only its method.
        ds.events.set_event_request(
            events::EventKind::Breakpoint,
            events::SuspendPolicy::None,
            bp(80, max),
        );
        publish_debugger_gates(&shared, &ds);
        assert!(gates.requires_interpreter(80, "max", "(JJ)J"));
        assert!(!gates.requires_interpreter(82, "other", "()V"));
        assert!(!gates.every_body_withdrawn());

        // A JDK class: every method, and the strides are held.
        ds.events.set_event_request(
            events::EventKind::Breakpoint,
            events::SuspendPolicy::None,
            bp(81, max),
        );
        publish_debugger_gates(&shared, &ds);
        assert!(gates.requires_interpreter(82, "other", "()V"));
        assert!(!gates.concerns_method(82, "other", "()V"), "no suspend point");
        assert!(gates.every_body_withdrawn());
        assert!(gates.tier_up_held());

        // It goes: the other methods may run compiled again; the strides
        // reopen after the linger.
        ds.events.clear_all();
        publish_debugger_gates(&shared, &ds);
        assert!(!gates.requires_interpreter(82, "other", "()V"));
        assert!(!gates.every_body_withdrawn());
    }

    #[test]
    fn debug_state_initial_values() {
        let st = DebugState::new();
        assert!(!st.any_suspension());
        assert!(!st.disposed);
        assert!(st.exit_code.is_none());
        assert_eq!(st.next_packet_id, 1);
    }

    #[test]
    fn debug_state_next_id_increments() {
        let mut st = DebugState::new();
        assert_eq!(st.next_id(), 1);
        assert_eq!(st.next_id(), 2);
        assert_eq!(st.next_id(), 3);
    }

    // --- Session 39: JDWP event system tests ---

    #[test]
    fn s39_debug_event_struct_captures_breakpoint() {
        let evt = DebugEvent {
            kind: events::EventKind::Breakpoint,
            request_id: 1,
            suspend_policy: events::SuspendPolicy::All,
            thread_id: 42,
            class_id: 10,
            method_id: 20,
            offset: 30,
            extra: Vec::new(),
            set_follows: false,
        };
        assert_eq!(evt.kind, events::EventKind::Breakpoint);
        assert_eq!(evt.request_id, 1);
        assert_eq!(evt.thread_id, 42);
        assert_eq!(evt.class_id, 10);
        assert_eq!(evt.method_id, 20);
        assert_eq!(evt.offset, 30);
    }

    #[test]
    fn s39_debug_event_struct_captures_single_step() {
        let evt = DebugEvent {
            kind: events::EventKind::SingleStep,
            request_id: 5,
            suspend_policy: events::SuspendPolicy::EventThread,
            thread_id: 7,
            class_id: 100,
            method_id: 200,
            offset: 42,
            extra: Vec::new(),
            set_follows: false,
        };
        assert_eq!(evt.kind, events::EventKind::SingleStep);
        assert_eq!(evt.suspend_policy, events::SuspendPolicy::EventThread);
    }

    #[test]
    fn s39_build_location_extra_format() {
        let extra = build_location_extra(10, 20, 30);
        assert_eq!(extra.len(), 25);
        assert_eq!(extra[0], 1); // TypeTag = CLASS
                                 // class_id = 10 in BE
        assert_eq!(u64::from_be_bytes(extra[1..9].try_into().unwrap()), 10);
        // method_id = 20 in BE
        assert_eq!(u64::from_be_bytes(extra[9..17].try_into().unwrap()), 20);
        // offset = 30 in BE
        assert_eq!(u64::from_be_bytes(extra[17..25].try_into().unwrap()), 30);
    }

    #[test]
    fn s39_frame_entry_construction() {
        let entry = FrameEntry {
            frame_id: 0,
            class_id: 42,
            method_id: 99,
            offset: 10,
        };
        assert_eq!(entry.frame_id, 0);
        assert_eq!(entry.class_id, 42);
    }

    #[test]
    fn s39_thread_frames_stored_and_retrieved() {
        let mut st = DebugState::new();
        assert!(st.thread_frames.is_empty());

        let frames = vec![
            FrameEntry {
                frame_id: 0,
                class_id: 1,
                method_id: 100,
                offset: 5,
            },
            FrameEntry {
                frame_id: 1,
                class_id: 2,
                method_id: 200,
                offset: 10,
            },
        ];
        st.thread_frames.insert(42, frames);

        assert_eq!(st.thread_frames.get(&42).unwrap().len(), 2);
        assert_eq!(st.thread_frames.get(&42).unwrap()[0].class_id, 1);
        assert!(st.thread_frames.get(&99).is_none());
    }

    #[test]
    fn s39_event_channel_send_receive() {
        let (tx, rx) = std::sync::mpsc::channel::<DebugEvent>();
        tx.send(DebugEvent {
            kind: events::EventKind::Breakpoint,
            request_id: 1,
            suspend_policy: events::SuspendPolicy::All,
            thread_id: 5,
            class_id: 10,
            method_id: 20,
            offset: 0,
            extra: Vec::new(),
            set_follows: false,
        })
        .unwrap();

        let evt = rx.recv().unwrap();
        assert_eq!(evt.kind, events::EventKind::Breakpoint);
        assert_eq!(evt.thread_id, 5);
    }

    #[test]
    fn s39_event_channel_multiple_events() {
        let (tx, rx) = std::sync::mpsc::channel::<DebugEvent>();
        for i in 0..5 {
            tx.send(DebugEvent {
                kind: events::EventKind::SingleStep,
                request_id: i,
                suspend_policy: events::SuspendPolicy::EventThread,
                thread_id: 1,
                class_id: 0,
                method_id: 0,
                offset: i as u64,
                extra: Vec::new(),
                set_follows: false,
            })
            .unwrap();
        }

        let mut received = 0;
        while let Ok(evt) = rx.try_recv() {
            assert_eq!(evt.kind, events::EventKind::SingleStep);
            assert_eq!(evt.offset, received as u64);
            received += 1;
        }
        assert_eq!(received, 5);
    }

    #[test]
    fn s39_compose_breakpoint_event_with_location() {
        let extra = build_location_extra(10, 20, 30);
        let event = events::Event {
            request_id: 1,
            kind: events::EventKind::Breakpoint,
            thread_id: 42,
            extra,
        };
        let pkt = events::compose_event_packet(events::SuspendPolicy::All, &[event]);
        match &pkt {
            JdwpPacket::Command {
                command_set,
                command,
                data,
                ..
            } => {
                assert_eq!(*command_set, 64);
                assert_eq!(*command, 100);
                // suspend_policy(1) + count(4) + kind(1) + req_id(4) + thread_id(8) + location(25) = 43
                assert_eq!(data.len(), 43);
            }
            _ => panic!("expected Command"),
        }
    }

    #[test]
    fn s39_compose_single_step_event_with_location() {
        let extra = build_location_extra(5, 10, 15);
        let event = events::Event {
            request_id: 2,
            kind: events::EventKind::SingleStep,
            thread_id: 7,
            extra,
        };
        let pkt = events::compose_event_packet(events::SuspendPolicy::EventThread, &[event]);
        match &pkt {
            JdwpPacket::Command { data, .. } => {
                assert_eq!(data[0], events::SuspendPolicy::EventThread as u8);
                // 1 + 4 + 1 + 4 + 8 + 25 = 43
                assert_eq!(data.len(), 43);
            }
            _ => panic!("expected Command"),
        }
    }

    #[test]
    fn s39_compose_thread_start_event() {
        let event = events::Event {
            request_id: 3,
            kind: events::EventKind::ThreadStart,
            thread_id: 99,
            extra: vec![],
        };
        let pkt = events::compose_event_packet(events::SuspendPolicy::None, &[event]);
        match &pkt {
            JdwpPacket::Command { data, .. } => {
                // suspend_policy(1) + count(4) + kind(1) + req_id(4) + thread_id(8) = 18
                assert_eq!(data.len(), 18);
                assert_eq!(data[0], 0); // NONE suspend policy
            }
            _ => panic!("expected Command"),
        }
    }

    #[test]
    fn s39_compose_thread_death_event() {
        let event = events::Event {
            request_id: 4,
            kind: events::EventKind::ThreadDeath,
            thread_id: 55,
            extra: vec![],
        };
        let pkt = events::compose_event_packet(events::SuspendPolicy::All, &[event]);
        match &pkt {
            JdwpPacket::Command { data, .. } => {
                assert_eq!(data.len(), 18);
            }
            _ => panic!("expected Command"),
        }
    }

    #[test]
    fn s39_tr_frame_count_with_frames() {
        let mut st = DebugState::new();
        st.thread_frames.insert(
            1,
            vec![
                FrameEntry {
                    frame_id: 0,
                    class_id: 10,
                    method_id: 100,
                    offset: 0,
                },
                FrameEntry {
                    frame_id: 1,
                    class_id: 20,
                    method_id: 200,
                    offset: 5,
                },
                FrameEntry {
                    frame_id: 2,
                    class_id: 30,
                    method_id: 300,
                    offset: 10,
                },
            ],
        );
        let count = st.thread_frames.get(&1).map_or(0, |f| f.len());
        assert_eq!(count, 3);
    }

    #[test]
    fn s39_tr_frame_count_no_thread() {
        let st = DebugState::new();
        let count = st.thread_frames.get(&999).map_or(0, |f| f.len());
        assert_eq!(count, 0);
    }

    #[test]
    fn s39_debug_state_thread_frames_cleared_on_new() {
        let st = DebugState::new();
        assert!(st.thread_frames.is_empty());
    }

    // -----------------------------------------------------------------------
    // Field watches and exception requests (wave 10). The T6.4.4 watchpoint
    // tables and their packet builders are gone; the request table is
    // matched by `EventManager::match_subject_events`.
    // -----------------------------------------------------------------------

    fn subject_location(class_name: &str) -> events::EventLocation<'_> {
        events::EventLocation {
            class_id: 10,
            class_name,
            method_id: 100,
            offset: 5,
            thread_id: 42,
            frame_depth: 1,
            line_start: None,
            line: None,
        }
    }

    /// A watch reports only its field; `Count` counts only the accesses the
    /// modifiers before it let through; `InstanceOnly` needs the accessed
    /// object; a watch without `FieldOnly` never reports.
    #[test]
    fn field_watch_requests_match_their_field_instance_and_count() {
        use events::{EventKind, EventModifier, EventSubject, SuspendPolicy};
        let mut st = DebugState::new();
        let counted = st.events.set_event_request(
            EventKind::FieldAccess,
            SuspendPolicy::All,
            vec![
                EventModifier::FieldOnly {
                    class_id: 10,
                    field_id: 3,
                },
                EventModifier::Count(2),
            ],
        );
        let instance = st.events.set_event_request(
            EventKind::FieldAccess,
            SuspendPolicy::None,
            vec![
                EventModifier::FieldOnly {
                    class_id: 10,
                    field_id: 3,
                },
                EventModifier::InstanceOnly { object_id: 77 },
            ],
        );
        let _everywhere =
            st.events
                .set_event_request(EventKind::FieldAccess, SuspendPolicy::All, vec![]);
        let at = subject_location("p/C");
        let hits = |st: &mut DebugState, field: (u64, u64), object: u64| {
            let subject = EventSubject {
                location_class_is: &|_| false,
                field: Some(field),
                instance_is: &|id| id == object,
                exception_is: &|_| false,
                caught: false,
            };
            st.events
                .match_subject_events(EventKind::FieldAccess, &at, &subject)
        };
        assert!(hits(&mut st, (10, 4), 77).is_empty(), "another field");
        assert!(hits(&mut st, (10, 3), 1).is_empty(), "first counted access");
        assert_eq!(
            hits(&mut st, (10, 3), 1),
            vec![(counted, SuspendPolicy::All)],
            "second counted access"
        );
        assert_eq!(
            hits(&mut st, (10, 3), 77),
            vec![(instance, SuspendPolicy::None)],
            "the watched object; the count is spent"
        );
        assert!(st.events.has_requests(EventKind::FieldAccess));
        assert!(!st.events.has_requests(EventKind::FieldModification));
        assert_eq!(
            st.events.instance_filter_ids(EventKind::FieldAccess),
            vec![77]
        );
    }

    /// `ExceptionOnly` filters by class (0: any) and by caught / uncaught;
    /// `ClassOnly` by the location's class; a field modifier suppresses an
    /// exception request.
    #[test]
    fn exception_requests_apply_exception_only_and_class_only() {
        use events::{EventKind, EventModifier, EventSubject, SuspendPolicy};
        let mut st = DebugState::new();
        let uncaught_io = st.events.set_event_request(
            EventKind::Exception,
            SuspendPolicy::All,
            vec![EventModifier::ExceptionOnly {
                class_id: 5,
                caught: false,
                uncaught: true,
            }],
        );
        let in_class_9 = st.events.set_event_request(
            EventKind::Exception,
            SuspendPolicy::EventThread,
            vec![
                EventModifier::ExceptionOnly {
                    class_id: 0,
                    caught: true,
                    uncaught: true,
                },
                EventModifier::ClassOnly { class_id: 9 },
            ],
        );
        let _field = st.events.set_event_request(
            EventKind::Exception,
            SuspendPolicy::All,
            vec![EventModifier::FieldOnly {
                class_id: 1,
                field_id: 1,
            }],
        );
        assert_eq!(st.events.filter_class_ids(EventKind::Exception), vec![5, 9]);
        let at = subject_location("p/Thrower");
        let hits = |st: &mut DebugState, exc_is_5: bool, in_9: bool, caught: bool| {
            let subject = EventSubject {
                location_class_is: &|id| in_9 && id == 9,
                field: None,
                instance_is: &|_| false,
                exception_is: &|id| exc_is_5 && id == 5,
                caught,
            };
            st.events
                .match_subject_events(EventKind::Exception, &at, &subject)
        };
        assert_eq!(
            hits(&mut st, true, false, false),
            vec![(uncaught_io, SuspendPolicy::All)]
        );
        assert!(
            hits(&mut st, true, false, true).is_empty(),
            "caught: not asked for"
        );
        assert!(
            hits(&mut st, false, false, false).is_empty(),
            "another class"
        );
        assert_eq!(
            hits(&mut st, false, true, true),
            vec![(in_class_9, SuspendPolicy::EventThread)]
        );
    }

    /// Wave 10: with no thread parked at a suspend point, heap work runs on
    /// the session's heap service — a registered thread of the VM, with the
    /// suspend point suppressed — so `CreateString` is served instead of
    /// answering `THREAD_NOT_SUSPENDED`; the service is not a thread the
    /// debugger lists; stopping it restores the refusal.
    #[test]
    fn heap_work_runs_on_the_heap_service_when_no_thread_is_parked() {
        use commands::{
            CMD_SR_VALUE, CMD_VM_CREATE_STRING, CS_STRING_REF, CS_VM, ERR_THREAD_NOT_SUSPENDED,
        };
        use protocol::PayloadWriter;
        let shared = Arc::new(crate::vm::SharedVm::new(crate::config::VmConfig::default()));
        let mut body = PayloadWriter::new();
        body.put_string("abc");
        let create = body.into_bytes();
        let res = inspect::run_heap_command(&shared, CS_VM, CMD_VM_CREATE_STRING, &create);
        assert_eq!(res.error_code, ERR_THREAD_NOT_SUSPENDED, "no service yet");

        assert!(start_heap_service(&shared), "the service starts");
        assert!(start_heap_service(&shared), "idempotent");
        let service = shared
            .debug
            .debug_state
            .lock()
            .heap_service_thread()
            .expect("the service's thread id");
        let seen = run_on_parked_thread(&shared, None, |_, thread| {
            (thread.thread_id.0, debugger_hooks_suppressed())
        });
        assert_eq!(seen, Ok((service, true)), "on the service, suppressed");
        assert!(!debugger_hooks_suppressed(), "only there");

        let res = inspect::run_heap_command(&shared, CS_VM, CMD_VM_CREATE_STRING, &create);
        assert_ne!(res.error_code, ERR_THREAD_NOT_SUSPENDED);
        if res.error_code == 0 {
            // A stripped test VM may lack `java.lang.String`; with it, the id
            // names a real string.
            let res = inspect::run_heap_command(&shared, CS_STRING_REF, CMD_SR_VALUE, &res.data);
            assert_eq!(res.error_code, 0);
            let mut expected = PayloadWriter::new();
            expected.put_string("abc");
            assert_eq!(res.data, expected.into_bytes());
        }

        populate_thread_metadata(&shared);
        assert!(
            !shared
                .debug
                .debug_state
                .lock()
                .thread_names
                .contains_key(&service),
            "the service is not listed"
        );

        stop_heap_service(&shared);
        assert_eq!(
            run_on_parked_thread(&shared, None, |_, _| ()),
            Err(ParkedError::NotParked)
        );
        let mut ds = shared.debug.debug_state.lock();
        ds.objects.release_all();
        release_disposed_objects(&shared, &mut ds);
    }

    /// Wave 13 (the suspension page's compiled-loop stage): a JDWP
    /// suspension is, for every method, the verdict a single-pass OSR body's
    /// back-edge poll reads (`interpreter::compiled_loop_must_leave`, the OSR
    /// door's own question), so the loop leaves through its OSR-exit map and
    /// the interpreter parks it at its next backward branch; resuming
    /// withdraws the verdict.
    #[test]
    fn a_suspension_makes_a_compiled_loop_leave_at_its_back_edge_poll() {
        let _lock = crate::runtime::jvmti::jvmti_registry_test_lock();
        let shared = Arc::new(crate::vm::SharedVm::new(crate::config::VmConfig::default()));
        let mut thread = crate::threading::jvm_thread::JvmThread::new(
            crate::threading::jvm_thread::ThreadId(3),
            "w13-osr-loop",
        );
        thread.frames.push(crate::runtime::frame::Frame::new(
            crate::classloading::ClassId::new(0x00F1_3001),
            "cratonvm/test/W13Loop".to_string(),
            "spin".to_string(),
            "()V".to_string(),
            None,
            vec![0xb1],
            vec![],
            4,
            2,
            &[],
        ));
        let must_leave = |thread: &crate::threading::jvm_thread::JvmThread| {
            crate::runtime::interpreter::compiled_loop_must_leave(&shared, thread)
        };
        assert!(!must_leave(&thread), "no request, no suspension");
        {
            let mut ds = shared.debug.debug_state.lock();
            ds.suspend_all();
            publish_debugger_gates(&shared, &ds);
        }
        assert!(shared.debug.debugger_gates.suspension_in_force());
        assert!(must_leave(&thread), "suspended: every method leaves");
        {
            let mut ds = shared.debug.debug_state.lock();
            ds.resume_vm();
            publish_debugger_gates(&shared, &ds);
        }
        assert!(!shared.debug.debugger_gates.suspension_in_force());
        assert!(!must_leave(&thread), "resumed");
    }

    /// Wave 18, lane L3 (`i9-L1-jdwp-suspension-does-not-reach-compiled-or-native-code`,
    /// item 4): a registered native thread returning from its blocking call
    /// is held while the debugger holds it suspended — by its own suspension
    /// or a VM-wide one — and released by the resume or by a detach; with no
    /// suspension, or no session, it passes straight through.
    #[test]
    fn a_suspended_native_thread_is_held_at_the_end_of_its_blocking_call() {
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::time::Duration;
        const TID: u64 = 0x00F1_8003;
        let _lock = crate::runtime::jvmti::jvmti_registry_test_lock();
        let shared = Arc::new(crate::vm::SharedVm::new(crate::config::VmConfig::default()));
        // Not attached, not suspended: nothing to wait for.
        hold_native_thread_while_suspended(&shared, TID);
        shared.debug.debugger_gates.set_session_attached(true);
        hold_native_thread_while_suspended(&shared, TID);

        // Returns whether the hold ended within `wait` after it began.
        let held_until = |release: &dyn Fn(), wait: Duration| -> bool {
            let done = Arc::new(AtomicBool::new(false));
            let worker = {
                let shared = Arc::clone(&shared);
                let done = Arc::clone(&done);
                std::thread::spawn(move || {
                    hold_native_thread_while_suspended(&shared, TID);
                    done.store(true, Ordering::Release);
                })
            };
            std::thread::sleep(Duration::from_millis(30));
            let held = !done.load(Ordering::Acquire);
            release();
            let deadline = std::time::Instant::now() + wait;
            while !done.load(Ordering::Acquire) && std::time::Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(1));
            }
            let released = done.load(Ordering::Acquire);
            let _ = worker.join();
            held && released
        };

        // Its own suspension, lifted by `ThreadReference.Resume`.
        {
            let mut ds = shared.debug.debug_state.lock();
            ds.suspend_thread(TID);
            publish_debugger_gates(&shared, &ds);
        }
        let resume_thread = || {
            let mut ds = shared.debug.debug_state.lock();
            ds.resume_thread(TID);
            publish_debugger_gates(&shared, &ds);
        };
        assert!(
            held_until(&resume_thread, Duration::from_secs(10)),
            "held while suspended, released by the resume"
        );

        // A VM-wide suspension, lifted by a detach.
        {
            let mut ds = shared.debug.debug_state.lock();
            ds.suspend_all();
            publish_debugger_gates(&shared, &ds);
        }
        let detach = || shared.debug.debugger_gates.set_session_attached(false);
        assert!(
            held_until(&detach, Duration::from_secs(10)),
            "held under VirtualMachine.Suspend, released by the detach"
        );
        {
            let mut ds = shared.debug.debug_state.lock();
            ds.resume_vm();
            publish_debugger_gates(&shared, &ds);
        }
        // Another thread's suspension does not hold this one.
        shared.debug.debugger_gates.set_session_attached(true);
        {
            let mut ds = shared.debug.debug_state.lock();
            ds.suspend_thread(TID + 1);
            publish_debugger_gates(&shared, &ds);
        }
        hold_native_thread_while_suspended(&shared, TID);
        {
            let mut ds = shared.debug.debug_state.lock();
            ds.resume_thread(TID + 1);
            publish_debugger_gates(&shared, &ds);
        }
    }

    /// Wave 10: a class prepared on a VM thread is reported from that
    /// thread — naming it, applying `SUSPEND_EVENT_THREAD` to it and
    /// evaluating `ThreadOnly` — once; the server's poll then skips it, and
    /// leaves a class that is defined but not yet prepared pending.
    #[test]
    fn class_prepare_is_reported_by_the_preparing_thread() {
        use events::{EventKind, EventModifier, SuspendPolicy};
        let shared = Arc::new(crate::vm::SharedVm::new(crate::config::VmConfig::default()));
        let name = "cratonvm/test/W10Prepared";
        let class_id = shared
            .classes
            .class_manager
            .write()
            .try_ensure_synthetic_class(name, 0)
            .expect("Compatible mode fabricates");
        let (stops, other_thread) = {
            let mut ds = shared.debug.debug_state.lock();
            let stops = ds.events.set_event_request(
                EventKind::ClassPrepare,
                SuspendPolicy::EventThread,
                vec![EventModifier::ClassMatch {
                    pattern: "cratonvm.test.W10*".to_string(),
                }],
            );
            let other_thread = ds.events.set_event_request(
                EventKind::ClassPrepare,
                SuspendPolicy::None,
                vec![EventModifier::ThreadOnly { thread_id: 8 }],
            );
            publish_debugger_gates(&shared, &ds);
            (stops, other_thread)
        };
        assert!(shared.debug.debugger_gates.class_prepare_armed());
        let (tx, rx) = std::sync::mpsc::channel();
        *shared.debug.debug_event_tx.lock().expect("event channel") = Some(tx);

        class_prepared_on_thread(&shared, 7, class_id);
        let got: Vec<DebugEvent> = rx.try_iter().collect();
        assert_eq!(got.len(), 1, "the ThreadOnly(8) request does not report");
        let evt = &got[0];
        assert_eq!(
            (evt.kind, evt.request_id, evt.thread_id, evt.suspend_policy),
            (
                EventKind::ClassPrepare,
                stops,
                7,
                SuspendPolicy::EventThread
            )
        );
        let wire = u64::from(class_id.as_u32());
        let mut want = protocol::PayloadWriter::new();
        want.put_u8(1);
        want.put_u64_be(wire);
        want.put_string(&format!("L{name};"));
        want.put_u32_be(3);
        assert_eq!(
            evt.extra,
            want.into_bytes(),
            "tag, class, signature, status"
        );
        {
            let ds = shared.debug.debug_state.lock();
            assert!(
                ds.is_thread_suspended(7),
                "the preparing thread is suspended"
            );
            assert!(!ds.is_thread_suspended(8));
        }
        assert!(
            shared
                .debug
                .debugger_gates
                .concerns_method(1, "anything", "()V"),
            "a suspension concerns every method: the thread parks at its next bytecode"
        );

        // Once: neither the thread path again nor the server's poll.
        class_prepared_on_thread(&shared, 8, class_id);
        assert_eq!(rx.try_iter().count(), 0);
        let ready = classes_ready_for_prepare_events(
            &shared,
            vec![(wire, format!("L{name};"), 1)],
            std::time::Instant::now(),
        );
        let (reported, pending) = {
            let ds = shared.debug.debug_state.lock();
            (
                ds.class_prepare_reported.contains(&wire),
                ds.class_prepare_pending.len(),
            )
        };
        assert!(reported, "the poll skips a class already considered");
        assert_eq!(
            ready.len() + pending,
            1,
            "the poll's candidate is ready, or pending until it is prepared"
        );
        let _ = other_thread;
    }

    /// The per-VM gates (wave 10): a field watch or an exception request
    /// makes every method run interpreted without turning on the loop's
    /// per-bytecode suspend point, and arms its own hook only.
    #[test]
    fn watch_and_exception_requests_publish_their_gates() {
        use events::{EventKind, EventModifier, SuspendPolicy};
        // The JDWP watch raises the process-wide JVMTI watch pre-filter, which
        // the JVMTI tests assert on under this lock.
        let _lock = crate::runtime::jvmti::jvmti_registry_test_lock();
        let shared = Arc::new(crate::vm::SharedVm::new(crate::config::VmConfig::default()));
        let gates = &shared.debug.debugger_gates;
        let ids = {
            let mut ds = shared.debug.debug_state.lock();
            let exc =
                ds.events
                    .set_event_request(EventKind::Exception, SuspendPolicy::None, vec![]);
            publish_debugger_gates(&shared, &ds);
            assert!(gates.exception_events_armed() && !gates.field_watch_armed());
            // Wave 38: an exception request withdraws nothing (the compiled
            // catch doors post the event).
            assert!(!gates.every_body_withdrawn());
            assert!(gates.requires_interpreter(3, "m", "()V"));
            assert!(
                !gates.concerns_method(3, "m", "()V"),
                "no per-bytecode suspend point"
            );
            assert!(shared
                .debug
                .breakpoints_active
                .load(std::sync::atomic::Ordering::Acquire));
            let watch = ds.events.set_event_request(
                EventKind::FieldModification,
                SuspendPolicy::None,
                vec![EventModifier::FieldOnly {
                    class_id: 3,
                    field_id: 3 << 32,
                }],
            );
            publish_debugger_gates(&shared, &ds);
            assert!(gates.field_watch_armed());
            assert!(crate::runtime::jvmti::any_field_watchpoint_active());
            // Wave 38: a field watch withdraws every compiled body (compiled
            // code posts no field event).
            assert_eq!(
                gates.every_body_withdrawn(),
                crate::runtime::interpreter::EVERY_BODY_WITHDRAWAL_ENABLED
            );
            (exc, watch)
        };
        {
            let mut ds = shared.debug.debug_state.lock();
            ds.events.clear_event_request(ids.0);
            ds.events.clear_event_request(ids.1);
            publish_debugger_gates(&shared, &ds);
        }
        assert!(!gates.field_watch_armed() && !gates.exception_events_armed());
        assert!(!gates.every_body_withdrawn(), "compiling resumes");
        assert!(!gates.requires_interpreter(3, "m", "()V"));
        assert!(!shared
            .debug
            .breakpoints_active
            .load(std::sync::atomic::Ordering::Acquire));
    }

    /// Wave 11: a thread's JDWP status comes from the registry's state — the
    /// blocking kind `Thread.getState()` reads, the deposited frames for a
    /// sleep, liveness for a zombie. `ThreadReference.Status` answered
    /// RUNNING for every thread.
    #[test]
    fn a_thread_status_follows_the_registry_state() {
        use crate::threading::jvm_thread::GcBlockState;
        use crate::threading::thread_registry::ThreadRegistry;
        use commands::{
            THREAD_STATUS_MONITOR, THREAD_STATUS_RUNNING, THREAD_STATUS_SLEEPING,
            THREAD_STATUS_WAIT, THREAD_STATUS_ZOMBIE,
        };
        use std::sync::atomic::Ordering;

        let registry = ThreadRegistry::new();
        let tid = crate::threading::ThreadId(1);
        registry.register(tid, "worker", None);
        let block = Arc::new(GcBlockState::new());
        registry.set_gc_block_state(tid, block.clone());
        assert_eq!(jdwp_thread_status(&registry, tid), THREAD_STATUS_RUNNING);

        block.in_blocked_region.store(true, Ordering::Release);
        block.java_state.store(1, Ordering::Release);
        assert_eq!(jdwp_thread_status(&registry, tid), THREAD_STATUS_WAIT);
        block.java_state.store(2, Ordering::Release);
        assert_eq!(jdwp_thread_status(&registry, tid), THREAD_STATUS_MONITOR);
        block.java_state.store(3, Ordering::Release);
        assert_eq!(
            jdwp_thread_status(&registry, tid),
            THREAD_STATUS_WAIT,
            "a timed wait that is not a sleep"
        );
        let frame = |class: &str, method: &str| cratonvm_native_api::StackTraceEntry {
            class_name: Arc::from(class),
            method_name: Arc::from(method),
            method_descriptor: None,
            source_file: None,
            line_number: -1,
            byte_code_index: 0,
            class_id: None,
            method_index: None,
        };
        // Outermost-first, as the blocking deposit captures it (wave 12: the
        // sleep frame was put first here, the order the status wrongly read).
        registry.set_frame_trace(
            tid,
            Arc::new(parking_lot::Mutex::new(vec![
                frame("java/lang/Thread", "sleepNanos"),
                frame("Main", "main"),
            ])),
        );
        assert_eq!(
            jdwp_thread_status(&registry, tid),
            THREAD_STATUS_WAIT,
            "a sleep frame at the bottom is not where the thread blocked"
        );
        registry.set_frame_trace(
            tid,
            Arc::new(parking_lot::Mutex::new(vec![
                frame("Main", "main"),
                frame("java/lang/Thread", "sleep"),
                frame("java/lang/Thread", "sleepNanos"),
            ])),
        );
        assert_eq!(jdwp_thread_status(&registry, tid), THREAD_STATUS_SLEEPING);
        // Wave 25: `Thread.sleep(long)` is a registered native here, so a
        // sleeping thread deposits no `Thread` frame; its top frame is the
        // caller, stopped at the invoke of `Thread.sleep`.
        registry.set_frame_trace(
            tid,
            Arc::new(parking_lot::Mutex::new(vec![
                frame("Main", "main"),
                frame("Main", "sleepQuietly"),
            ])),
        );
        assert_eq!(jdwp_thread_status(&registry, tid), THREAD_STATUS_WAIT);
        let sleeps = |e: &cratonvm_native_api::StackTraceEntry| &*e.method_name == "sleepQuietly";
        assert_eq!(
            jdwp_thread_status_with(&registry, tid, &sleeps),
            THREAD_STATUS_SLEEPING
        );
        block.in_blocked_region.store(false, Ordering::Release);
        assert_eq!(
            jdwp_thread_status(&registry, tid),
            THREAD_STATUS_RUNNING,
            "the kind counts only inside a blocking region"
        );

        registry.mark_dead(tid);
        assert_eq!(jdwp_thread_status(&registry, tid), THREAD_STATUS_ZOMBIE);
    }

    /// Wave 15: a class's JDWP status follows its state — no bit until it is
    /// linked, VERIFIED | PREPARED once linked, INITIALIZED once initialized,
    /// ERROR for an erroneous class — and the server's refresh records it for
    /// every class the session knows. Every class answered 3.
    #[test]
    fn a_class_status_follows_the_class_state() {
        use crate::classloading::ClassState;
        let shared = Arc::new(crate::vm::SharedVm::new(crate::config::VmConfig::default()));
        let class_id = shared
            .classes
            .class_manager
            .write()
            .try_ensure_synthetic_class("cratonvm/test/W15Status", 0)
            .expect("Compatible mode fabricates");
        let _ = populate_class_metadata(&shared);
        let wire = u64::from(class_id.as_u32());
        for (state, bits) in [
            (ClassState::Loaded, 0),
            (ClassState::Verifying, 0),
            (ClassState::Verified, 3),
            (ClassState::Prepared, 3),
            (ClassState::Initializing, 3),
            (ClassState::Initialized, 7),
            (ClassState::InitializationError, 11),
        ] {
            shared
                .classes
                .class_manager_write()
                .get_class_mut(class_id)
                .expect("registered")
                .state = state;
            refresh_class_statuses(&shared);
            let refreshed = shared
                .debug
                .debug_state
                .lock()
                .class_statuses
                .get(&wire)
                .copied();
            assert_eq!(refreshed, Some(bits), "{state}");
        }
        assert!(reads_class_status(
            commands::CS_VM,
            commands::CMD_VM_CLASSES_BY_SIGNATURE
        ));
        assert!(reads_class_status(
            commands::CS_VM,
            commands::CMD_VM_ALL_CLASSES_WITH_GENERIC
        ));
        assert!(!reads_class_status(
            commands::CS_VM,
            commands::CMD_VM_VERSION
        ));
        assert!(reads_class_status(
            commands::CS_REF_TYPE,
            commands::CMD_RT_STATUS
        ));
    }

    /// Wave 25 (the conformance runner's three NullPointerExceptions): the
    /// VM's thread 0 (main) and class 0 (`java.lang.Object`) go out as ids JDI
    /// does not read as null — in the `AllThreads` reply, a thread value, an
    /// event's thread, a location and the `Superclass` reply — and come back
    /// in as 0; a wire 0 names nothing.
    #[test]
    fn the_main_thread_and_class_zero_are_not_null_on_the_wire() {
        use commands::{
            dispatch, CMD_CT_SUPERCLASS, CMD_OR_INVOKE_METHOD, CMD_TR_NAME, CMD_VM_ALL_THREADS,
            CS_CLASS_TYPE, CS_OBJECT_REF, CS_THREAD_REF, CS_VM, ERR_NONE,
        };
        use ids::{
            class_from_wire, class_to_wire, thread_from_wire, thread_to_wire, CLASS_ZERO_WIRE_ID,
            MAIN_THREAD_WIRE_ID, NO_VM_ID,
        };
        assert_eq!(thread_to_wire(0), MAIN_THREAD_WIRE_ID);
        assert_eq!(thread_from_wire(MAIN_THREAD_WIRE_ID), 0);
        assert_eq!((thread_to_wire(7), thread_from_wire(7)), (7, 7));
        assert_eq!(thread_from_wire(0), NO_VM_ID);
        assert_eq!(class_to_wire(0), CLASS_ZERO_WIRE_ID);
        assert_eq!(class_from_wire(CLASS_ZERO_WIRE_ID), 0);
        assert_eq!(class_from_wire(0), NO_VM_ID);
        assert!(u32::try_from(NO_VM_ID).is_err(), "names no class");

        let mut ds = DebugState::new();
        ds.thread_names.insert(0, "main".to_string());
        ds.thread_names.insert(7, "worker".to_string());
        // `VirtualMachine.AllThreads`: the main thread is not 0.
        let res = dispatch(CS_VM, CMD_VM_ALL_THREADS, &[], &mut ds);
        assert_eq!(res.error_code, ERR_NONE);
        let mut want = protocol::PayloadWriter::new();
        want.put_u32_be(2);
        want.put_u64_be(MAIN_THREAD_WIRE_ID);
        want.put_u64_be(7);
        assert_eq!(res.data, want.into_bytes());
        // A command naming it by that id reaches thread 0.
        let mut data = MAIN_THREAD_WIRE_ID.to_be_bytes().to_vec();
        ids_from_wire(CS_THREAD_REF, CMD_TR_NAME, &mut data);
        assert_eq!(data, 0u64.to_be_bytes().to_vec());
        let res = dispatch(CS_THREAD_REF, CMD_TR_NAME, &data, &mut ds);
        assert_eq!(res.error_code, ERR_NONE);
        let mut name = protocol::PayloadWriter::new();
        name.put_string("main");
        assert_eq!(res.data, name.into_bytes());
        // A wire 0 (null) does not.
        let mut data = 0u64.to_be_bytes().to_vec();
        ids_from_wire(CS_THREAD_REF, CMD_TR_NAME, &mut data);
        assert_ne!(dispatch(CS_THREAD_REF, CMD_TR_NAME, &data, &mut ds).error_code, ERR_NONE);

        // `ClassType.Superclass` of a class whose superclass is class 0.
        ds.class_signatures.insert(5, "LFive;".to_string());
        ds.class_signatures.insert(0, "Ljava/lang/Object;".to_string());
        ds.class_superclass.insert(5, 0);
        let mut data = 5u64.to_be_bytes().to_vec();
        ids_from_wire(CS_CLASS_TYPE, CMD_CT_SUPERCLASS, &mut data);
        let res = dispatch(CS_CLASS_TYPE, CMD_CT_SUPERCLASS, &data, &mut ds);
        assert_eq!(res.data, CLASS_ZERO_WIRE_ID.to_be_bytes().to_vec());
        // ... and of class 0 itself, named by its wire id: null.
        let mut data = CLASS_ZERO_WIRE_ID.to_be_bytes().to_vec();
        ids_from_wire(CS_CLASS_TYPE, CMD_CT_SUPERCLASS, &mut data);
        assert_eq!(data, 0u64.to_be_bytes().to_vec());
        let res = dispatch(CS_CLASS_TYPE, CMD_CT_SUPERCLASS, &data, &mut ds);
        assert_eq!(res.data, 0u64.to_be_bytes().to_vec());

        // `ObjectReference.InvokeMethod`: object, thread, class, method.
        let mut data = Vec::new();
        for id in [MAIN_THREAD_WIRE_ID, MAIN_THREAD_WIRE_ID, CLASS_ZERO_WIRE_ID, 9] {
            data.extend_from_slice(&id.to_be_bytes());
        }
        ids_from_wire(CS_OBJECT_REF, CMD_OR_INVOKE_METHOD, &mut data);
        let mut want = Vec::new();
        for id in [MAIN_THREAD_WIRE_ID, 0, 0, 9] {
            want.extend_from_slice(&id.to_be_bytes());
        }
        assert_eq!(data, want, "the receiver is an object id, decoded where it is read");

        // An event on the main thread at a location in class 0; a null
        // location stays zeros.
        let event = events::Event {
            request_id: 1,
            kind: events::EventKind::Breakpoint,
            thread_id: 0,
            extra: build_location_extra(0, 9, 3),
        };
        let JdwpPacket::Command { data, .. } =
            events::compose_event_packet(events::SuspendPolicy::None, &[event])
        else {
            unreachable!("an event set is a command packet");
        };
        // policy(1) count(4) kind(1) request(4), then the thread.
        assert_eq!(&data[10..18], &MAIN_THREAD_WIRE_ID.to_be_bytes()[..]);
        assert_eq!(&data[19..27], &CLASS_ZERO_WIRE_ID.to_be_bytes()[..]);
        let mut null_location = vec![0u8; 25];
        null_location[0] = 1; // CLASS
        assert_eq!(build_location_extra(0, 0, 0), null_location);
    }

    /// Wave 25: the main thread's wire id names its `Thread` object, and its
    /// `Thread` object goes out as that id (`inspect::thread_value`).
    #[test]
    fn the_main_thread_wire_id_names_its_thread_object() {
        let shared = Arc::new(crate::vm::SharedVm::new(crate::config::VmConfig::default()));
        let Some(obj) = shared.mem.heap.try_alloc_array_full(
            crate::classloading::ClassId::new(0),
            crate::memory::heap::ArrayElementType::Int,
            1,
        ) else {
            return; // no room in a stripped test heap
        };
        shared.threads.thread_registry.register(
            crate::threading::ThreadId(0),
            "main",
            Some(obj),
        );
        let ds = shared.debug.debug_state.lock();
        assert_eq!(object_for_id(&shared, &ds, ids::MAIN_THREAD_WIRE_ID), Some(obj));
        assert_eq!(object_for_id(&shared, &ds, 0), None, "0 is null");
        assert_eq!(
            inspect::thread_value(&shared, &ds, obj),
            Some((b't', ids::MAIN_THREAD_WIRE_ID))
        );
    }

    /// Interpreter round i1 wave 23: a JDWP thread id names the thread's
    /// `java.lang.Thread` where a command takes an object id, as in HotSpot
    /// (a JDI `ThreadReference` is an `ObjectReference`; jdb's `threads`
    /// asks each thread's `referenceType()`); an id naming no listed thread
    /// stays `INVALID_OBJECT`.
    #[test]
    fn a_thread_id_names_the_thread_object() {
        let shared = Arc::new(crate::vm::SharedVm::new(crate::config::VmConfig::default()));
        let Some(obj) = shared.mem.heap.try_alloc_array_full(
            crate::classloading::ClassId::new(0),
            crate::memory::heap::ArrayElementType::Int,
            1,
        ) else {
            return; // no room in a stripped test heap
        };
        shared.threads.thread_registry.register(
            crate::threading::ThreadId(5),
            "worker",
            Some(obj),
        );
        let mut ds = shared.debug.debug_state.lock();
        // Wave 24: a registered thread resolves before the server's thread
        // table lists it (it was `INVALID_OBJECT` until the next refresh).
        assert_eq!(object_for_id(&shared, &ds, 5), Some(obj), "registered, not listed yet");
        ds.thread_names.insert(5, "worker".to_string());
        assert_eq!(object_for_id(&shared, &ds, 5), Some(obj));
        assert_eq!(object_collected(&shared, &ds, 5), Some(false));
        assert!(set_collection_enabled(&shared, &mut ds, 5, false));
        assert!(set_collection_enabled(&shared, &mut ds, 5, true));
        assert_eq!(object_for_id(&shared, &ds, 6), None, "no such thread");
        assert_eq!(object_collected(&shared, &ds, 6), None);
        // The back end's own heap service is never a debugger's thread.
        let (jobs, _rx) = std::sync::mpsc::channel::<crate::native::jni::AttachedServiceJob>();
        ds.heap_service = Some((jobs, 5));
        assert_eq!(object_for_id(&shared, &ds, 5), None, "the heap service is hidden");
        ds.heap_service = None;
    }

    /// Interpreter round i1 wave 23: what a JDI session asks of a class
    /// (`ReferenceType` 2/3, 7, 10, 13-15, 17 and `Method` 6/1, 6/5) is
    /// answered from the class as HotSpot's back end answers it — a member
    /// class's modifiers from its `InnerClasses` entry, the generic
    /// signatures, the synthetic marker, the line table's code range, and the
    /// `NATIVE_METHOD` / `ABSENT_INFORMATION` refusals. Until wave 23 the
    /// `*WithGeneric` commands were not served, so JDI could set no line
    /// breakpoint.
    #[test]
    fn a_jdi_session_reads_a_class_as_hotspot_answers_it() {
        use commands::{
            dispatch, CMD_M_LINE_TABLE, CMD_M_VARIABLE_TABLE_WITH_GENERIC, CMD_RT_CLASS_FILE_VERSION,
            CMD_RT_FIELDS_WITH_GENERIC, CMD_RT_INTERFACES, CMD_RT_METHODS_WITH_GENERIC,
            CMD_RT_MODIFIERS, CMD_RT_SIGNATURE_WITH_GENERIC, CMD_RT_SOURCE_FILE, CS_METHOD,
            CS_REF_TYPE, ERR_ABSENT_INFORMATION, ERR_INVALID_CLASS, ERR_INVALID_METHODID,
            ERR_NATIVE_METHOD, ERR_NONE,
        };
        use cratonvm_reader::attribute::{
            Attribute, CodeAttribute, LazyAttribute, LineNumberEntry, LocalVariableEntry,
            LocalVariableTypeEntry,
        };
        use cratonvm_reader::class_access_flags::{
            ClassAccessFlags, FieldAccessFlags, MethodAccessFlags as M,
        };
        use cratonvm_reader::constant_pool::{ConstantPool, ConstantPoolEntry};
        use protocol::{PayloadReader, PayloadWriter};

        let shared = Arc::new(crate::vm::SharedVm::new(crate::config::VmConfig::default()));
        let name = "cratonvm/test/W23Outer$Pair";
        let list_generic = "Ljava/util/List<Ljava/lang/String;>;";
        let mut cm = shared.classes.class_manager.write();
        let iface = cm
            .try_ensure_synthetic_class("cratonvm/test/W23Iface", 0)
            .expect("Compatible mode fabricates");
        let id = cm
            .try_ensure_synthetic_class(name, 0)
            .expect("Compatible mode fabricates");
        let method = |name: &str, desc: &str, flags: M, attributes: Vec<LazyAttribute>| {
            cratonvm_reader::method::ClassFileMethod {
                access_flags: flags,
                name: Arc::from(name),
                descriptor: Arc::from(desc),
                attributes,
            }
        };
        let code = |len: usize, attributes: Vec<Attribute>| {
            LazyAttribute::Decoded(Attribute::Code(CodeAttribute {
                max_stack: 1,
                max_locals: 2,
                code: cratonvm_reader::ByteView::from_vec(vec![0; len]),
                exception_table: vec![],
                attributes,
            }))
        };
        // Through the store, which keeps the implementor index in step
        // (`class::tests::no_raw_write_to_a_stored_class_interfaces_bypasses_set_interfaces`).
        cm.set_interfaces(id, vec![iface]);
        {
            let class = cm.get_class_mut(id).expect("just added");
            // `final` + `ACC_SUPER` in the class file; `static final` in its
            // `InnerClasses` entry, which HotSpot answers (with `ACC_SUPER`).
            class.access_flags = ClassAccessFlags::FINAL | ClassAccessFlags::SUPER;
            class.inner_classes[0].access_flags = 0x0018;
            class.signature = Some("Ljava/lang/Object;Lcratonvm/test/W23Iface;".to_string());
            class.source_file = None;
            class.constant_pool = ConstantPool::new(vec![
                ConstantPoolEntry::Tombstone,
                ConstantPoolEntry::Utf8(Arc::from("n")),
                ConstantPoolEntry::Utf8(Arc::from("I")),
                ConstantPoolEntry::Utf8(Arc::from("tags")),
                ConstantPoolEntry::Utf8(Arc::from("Ljava/util/List;")),
                ConstantPoolEntry::Utf8(Arc::from(list_generic)),
            ]);
            class.methods = vec![
                method(
                    "run",
                    "(ILjava/util/List;)V",
                    M::STATIC,
                    vec![
                        LazyAttribute::Decoded(Attribute::Signature(Arc::from(
                            "(ILjava/util/List<Ljava/lang/String;>;)V",
                        ))),
                        code(
                            6,
                            vec![
                                Attribute::LineNumberTable(vec![
                                    LineNumberEntry {
                                        start_pc: 0,
                                        line_number: 10,
                                    },
                                    LineNumberEntry {
                                        start_pc: 3,
                                        line_number: 11,
                                    },
                                ]),
                                Attribute::LocalVariableTable(vec![
                                    LocalVariableEntry {
                                        start_pc: 0,
                                        length: 6,
                                        name_index: 1,
                                        descriptor_index: 2,
                                        index: 0,
                                    },
                                    LocalVariableEntry {
                                        start_pc: 0,
                                        length: 6,
                                        name_index: 3,
                                        descriptor_index: 4,
                                        index: 1,
                                    },
                                ]),
                                Attribute::LocalVariableTypeTable(vec![LocalVariableTypeEntry {
                                    start_pc: 0,
                                    length: 6,
                                    name_index: 3,
                                    signature_index: 5,
                                    index: 1,
                                }]),
                            ],
                        ),
                    ],
                ),
                method(
                    "bridge",
                    "()Ljava/lang/Object;",
                    M::PUBLIC | M::BRIDGE | M::SYNTHETIC,
                    vec![code(1, vec![])],
                ),
                method("nat", "()V", M::NATIVE, vec![]),
                method("abs", "()V", M::ABSTRACT, vec![]),
            ];
            class.fields = vec![
                cratonvm_reader::field::ClassFileField {
                    access_flags: FieldAccessFlags::PRIVATE,
                    name: Arc::from("tags"),
                    descriptor: Arc::from("Ljava/util/List;"),
                    attributes: vec![LazyAttribute::Decoded(Attribute::Signature(Arc::from(
                        list_generic,
                    )))],
                },
                cratonvm_reader::field::ClassFileField {
                    access_flags: FieldAccessFlags::FINAL | FieldAccessFlags::SYNTHETIC,
                    name: Arc::from("this$0"),
                    descriptor: Arc::from("Lcratonvm/test/W23Outer;"),
                    attributes: vec![],
                },
            ];
        }
        let version = {
            let class = cm.class_store.get(id).expect("just added");
            let mut ds = shared.debug.debug_state.lock();
            let _ = add_class_metadata(&mut ds, class);
            (class.version.major, class.version.minor)
        };
        drop(cm);

        let wire = u64::from(id.as_u32());
        let mut ds = shared.debug.debug_state.lock();
        let mut ask = |set: u8, cmd: u8, ids: &[u64]| {
            let mut pw = PayloadWriter::new();
            for &v in ids {
                pw.put_u64_be(v);
            }
            dispatch(set, cmd, &pw.into_bytes(), &mut ds)
        };
        let ok = |res: commands::CommandResult| {
            assert_eq!(res.error_code, ERR_NONE);
            res.data
        };

        let data = ok(ask(CS_REF_TYPE, CMD_RT_MODIFIERS, &[wire]));
        assert_eq!(data, 0x38u32.to_be_bytes().to_vec(), "static final + super");
        let data = ok(ask(CS_REF_TYPE, CMD_RT_SIGNATURE_WITH_GENERIC, &[wire]));
        let mut r = PayloadReader::new(&data);
        assert_eq!(r.read_string().expect("sig"), format!("L{name};"));
        assert_eq!(
            r.read_string().expect("generic"),
            "Ljava/lang/Object;Lcratonvm/test/W23Iface;"
        );
        let data = ok(ask(CS_REF_TYPE, CMD_RT_INTERFACES, &[wire]));
        let mut want = 1u32.to_be_bytes().to_vec();
        want.extend(u64::from(iface.as_u32()).to_be_bytes());
        assert_eq!(data, want);
        let data = ok(ask(CS_REF_TYPE, CMD_RT_CLASS_FILE_VERSION, &[wire]));
        let mut want = u32::from(version.0).to_be_bytes().to_vec();
        want.extend(u32::from(version.1).to_be_bytes());
        assert_eq!(data, want);
        assert_eq!(
            ask(CS_REF_TYPE, CMD_RT_SOURCE_FILE, &[wire]).error_code,
            ERR_ABSENT_INFORMATION
        );
        assert_eq!(
            ask(CS_REF_TYPE, CMD_RT_MODIFIERS, &[0x7fff_fff0]).error_code,
            ERR_INVALID_CLASS
        );

        // MethodsWithGeneric: id, name, signature, generic, modifiers.
        let data = ok(ask(CS_REF_TYPE, CMD_RT_METHODS_WITH_GENERIC, &[wire]));
        let mut r = PayloadReader::new(&data);
        let mut methods = Vec::new();
        for _ in 0..r.read_u32_be().expect("count") {
            let id = r.read_u64_be().expect("id");
            let name = r.read_string().expect("name");
            let _sig = r.read_string().expect("signature");
            let generic = r.read_string().expect("generic");
            methods.push((id, name, generic, r.read_u32_be().expect("modifiers")));
        }
        let named = |n: &str| {
            methods
                .iter()
                .find(|m| m.1 == n)
                .cloned()
                .expect("listed")
        };
        assert_eq!(
            methods.iter().map(|m| m.1.as_str()).collect::<Vec<_>>(),
            vec!["run", "bridge", "nat", "abs"],
            "declaration order"
        );
        assert_eq!(named("run").2, "(ILjava/util/List<Ljava/lang/String;>;)V");
        assert_eq!(named("run").3, 0x0008);
        assert_eq!(named("bridge").2, "");
        assert_eq!(named("bridge").3, 0xF000_1041, "synthetic marker");
        assert_eq!(named("nat").3, 0x0100);

        // FieldsWithGeneric.
        let data = ok(ask(CS_REF_TYPE, CMD_RT_FIELDS_WITH_GENERIC, &[wire]));
        let mut r = PayloadReader::new(&data);
        assert_eq!(r.read_u32_be().expect("count"), 2);
        let _ = r.read_u64_be().expect("id");
        assert_eq!(r.read_string().expect("name"), "tags");
        let _ = r.read_string().expect("signature");
        assert_eq!(r.read_string().expect("generic"), list_generic);
        assert_eq!(r.read_u32_be().expect("modifiers"), 0x0002);
        let _ = r.read_u64_be().expect("id");
        assert_eq!(r.read_string().expect("name"), "this$0");
        let _ = r.read_string().expect("signature");
        assert_eq!(r.read_string().expect("generic"), "");
        assert_eq!(r.read_u32_be().expect("modifiers"), 0xF000_1010);

        // LineTable: start 0, end = the last bytecode's index.
        let data = ok(ask(CS_METHOD, CMD_M_LINE_TABLE, &[wire, named("run").0]));
        let mut r = PayloadReader::new(&data);
        assert_eq!(r.read_u64_be().expect("start"), 0);
        assert_eq!(r.read_u64_be().expect("end"), 5, "not the last line's start, 3");
        assert_eq!(r.read_u32_be().expect("lines"), 2);
        let data = ok(ask(CS_METHOD, CMD_M_LINE_TABLE, &[wire, named("bridge").0]));
        let mut want = vec![0; 16];
        want.extend(0u32.to_be_bytes());
        assert_eq!(data, want, "no line table: 0..0, no entries");
        let data = ok(ask(CS_METHOD, CMD_M_LINE_TABLE, &[wire, named("abs").0]));
        let mut want = vec![0xFF; 16];
        want.extend(0u32.to_be_bytes());
        assert_eq!(data, want, "abstract: -1..-1");
        assert_eq!(
            ask(CS_METHOD, CMD_M_LINE_TABLE, &[wire, named("nat").0]).error_code,
            ERR_NATIVE_METHOD
        );
        assert_eq!(
            ask(CS_METHOD, CMD_M_LINE_TABLE, &[wire, 0x1234]).error_code,
            ERR_INVALID_METHODID
        );

        // VariableTableWithGeneric: argCnt 2 (static, int + List), the
        // generic signature from the `LocalVariableTypeTable`.
        let data = ok(ask(
            CS_METHOD,
            CMD_M_VARIABLE_TABLE_WITH_GENERIC,
            &[wire, named("run").0],
        ));
        let mut r = PayloadReader::new(&data);
        assert_eq!(r.read_u32_be().expect("argCnt"), 2);
        assert_eq!(r.read_u32_be().expect("slots"), 2);
        let mut generics = Vec::new();
        for _ in 0..2 {
            let _ = r.read_u64_be().expect("codeIndex");
            let name = r.read_string().expect("name");
            let _ = r.read_string().expect("signature");
            generics.push((name, r.read_string().expect("generic")));
            let _ = r.read_u32_be().expect("length");
            let _ = r.read_u32_be().expect("slot");
        }
        assert_eq!(
            generics,
            vec![
                ("n".to_string(), String::new()),
                ("tags".to_string(), list_generic.to_string())
            ]
        );
        assert_eq!(
            ask(
                CS_METHOD,
                CMD_M_VARIABLE_TABLE_WITH_GENERIC,
                &[wire, named("bridge").0]
            )
            .error_code,
            ERR_ABSENT_INFORMATION,
            "no LocalVariableTable"
        );
        assert_eq!(
            ask(
                CS_METHOD,
                CMD_M_VARIABLE_TABLE_WITH_GENERIC,
                &[wire, named("nat").0]
            )
            .error_code,
            ERR_NATIVE_METHOD
        );
    }

    /// Interpreter round i1 wave 24: a frame pushed while a `MethodEntry`
    /// request is in force is an entry at its first bytecode once; a
    /// backward branch to bytecode 0 of the same frame is not, and a deeper
    /// record left by a frame that went away is dropped.
    #[test]
    fn a_frame_entry_is_reported_once_per_push() {
        // Whatever this thread recorded before (the records are per thread).
        let _ = take_frame_entry(0);
        note_frame_entered(3);
        assert!(take_frame_entry(3), "the first bytecode of the new frame");
        assert!(!take_frame_entry(3), "a branch back to bytecode 0");
        note_frame_entered(3);
        note_frame_entered(4);
        note_frame_entered(5);
        // Frame 5 and 4 returned before reaching bytecode 0 (a stale record);
        // frame 3's own entry is still pending.
        assert!(take_frame_entry(3));
        assert!(!take_frame_entry(4), "dropped with the deeper records");
        // A new push at a depth drops the records at or above it.
        note_frame_entered(2);
        note_frame_entered(2);
        assert!(take_frame_entry(2));
        assert!(!take_frame_entry(2), "one record per depth");
    }

    /// Interpreter round i1 wave 24: `VirtualMachine.ClassPaths` answers
    /// `user.dir`, `java.class.path` split at `path.separator`, and an empty
    /// boot class path, as HotSpot's back end does. It answered
    /// `NOT_IMPLEMENTED`.
    #[test]
    fn class_paths_answers_the_class_path_properties() {
        use commands::{CMD_VM_CLASS_PATHS, CS_VM};
        let shared = crate::vm::SharedVm::new(crate::config::VmConfig::default());
        {
            let mut props = shared.system_properties.write();
            props.insert("user.dir".to_string(), "/work".to_string());
            props.insert("java.class.path".to_string(), "out:lib/a.jar".to_string());
            props.insert("path.separator".to_string(), ":".to_string());
        }
        let res = serve_with_the_vm(&shared, CS_VM, CMD_VM_CLASS_PATHS, &[]).expect("served");
        assert_eq!(res.error_code, commands::ERR_NONE);
        let mut r = protocol::PayloadReader::new(&res.data);
        assert_eq!(r.read_string().expect("baseDir"), "/work");
        assert_eq!(r.read_u32_be().expect("classpaths"), 2);
        assert_eq!(r.read_string().expect("entry"), "out");
        assert_eq!(r.read_string().expect("entry"), "lib/a.jar");
        assert_eq!(r.read_u32_be().expect("bootclasspaths"), 0);
        // Wave 25: `Version` is served with the VM too (the JDK it runs);
        // a command that needs neither the VM nor its heap is not.
        assert!(serve_with_the_vm(&shared, CS_VM, commands::CMD_VM_VERSION, &[]).is_some());
        assert!(serve_with_the_vm(&shared, CS_VM, commands::CMD_VM_ID_SIZES, &[]).is_none());
    }

    /// Interpreter round i1 wave 24: a `ThreadReference.Stop` keeps its
    /// throwable alive and its id valid until the thread takes it — through
    /// the debugger's `DisposeObjects` of the id — and a pending stop arms
    /// every method; taking it gives the object and releases the id.
    #[test]
    fn a_pending_stop_survives_its_disposal_until_taken() {
        let shared = Arc::new(crate::vm::SharedVm::new(crate::config::VmConfig::default()));
        let Some(obj) = shared.mem.heap.try_alloc_array_full(
            crate::classloading::ClassId::new(0),
            crate::memory::heap::ArrayElementType::Int,
            1,
        ) else {
            return; // no room in a stripped test heap
        };
        if shared
            .mem
            .heap
            .is_object_address(obj.as_ptr() as usize) // Cast: address probe
            .is_none()
        {
            return; // a backend that cannot answer the liveness probe
        }
        let id = {
            let mut ds = shared.debug.debug_state.lock();
            let id = export_object(&shared, &mut ds, obj, ObjectExport::Sent);
            assert!(set_pending_stop(&shared, &mut ds, 5, id));
            assert!(ds.has_pending_stop(5) && !ds.has_pending_stop(6));
            publish_debugger_gates(&shared, &ds);
            assert!(
                shared.debug.debugger_gates.requires_interpreter_everywhere(),
                "a pending stop concerns every method"
            );
            // Wave 25: and arms the unwinder's replacement.
            assert!(shared.debug.debugger_gates.stop_pending());
            // The debugger lets go of the id before the thread takes it.
            ds.objects.dispose(id, 1);
            assert!(ds.objects.handle_of(id).is_some(), "still pinned");
            id
        };
        assert_eq!(take_pending_stop(&shared, 5), Some(obj));
        assert_eq!(take_pending_stop(&shared, 5), None, "taken once");
        let ds = shared.debug.debug_state.lock();
        assert!(ds.objects.handle_of(id).is_none(), "released once taken");
        assert!(!shared.debug.debugger_gates.requires_interpreter_everywhere());
        assert!(!shared.debug.debugger_gates.stop_pending());
    }

    /// Wave 25: the unwinder's replacement — the exception in flight gives
    /// way to the thread's pending stop, once; another thread's exception is
    /// its own.
    #[test]
    fn a_pending_stop_replaces_the_exception_in_flight_once() {
        let shared = Arc::new(crate::vm::SharedVm::new(crate::config::VmConfig::default()));
        let heap = &shared.mem.heap;
        let alloc = || {
            heap.try_alloc_array_full(
                crate::classloading::ClassId::new(0),
                crate::memory::heap::ArrayElementType::Int,
                1,
            )
        };
        let (Some(stop), Some(in_flight)) = (alloc(), alloc()) else {
            return; // no room in a stripped test heap
        };
        if heap.is_object_address(stop.as_ptr() as usize).is_none() {
            return; // a backend that cannot answer the liveness probe
        }
        {
            let mut ds = shared.debug.debug_state.lock();
            let id = export_object(&shared, &mut ds, stop, ObjectExport::Sent);
            assert!(set_pending_stop(&shared, &mut ds, 5, id));
            publish_debugger_gates(&shared, &ds);
        }
        assert_eq!(exception_or_pending_stop(&shared, 6, in_flight), in_flight);
        assert_eq!(exception_or_pending_stop(&shared, 5, in_flight), stop);
        assert_eq!(exception_or_pending_stop(&shared, 5, in_flight), in_flight);
        assert!(!shared.debug.debugger_gates.stop_pending());
    }

    /// Wave 25: a class the session knows that is no longer in the class
    /// store is forgotten — `ClassesBySignature` no longer lists it and
    /// `ReferenceType.Signature` answers `INVALID_CLASS` — and one
    /// `ClassUnload` event set (request id, signature, no thread) goes to
    /// each matching request; the first sweep of a session only forgets.
    #[test]
    fn an_unloaded_class_is_forgotten_and_reported() {
        let shared = Arc::new(crate::vm::SharedVm::new(crate::config::VmConfig::default()));
        // A class id the store never held (ids are never reused), known to
        // the session as if an earlier poll had listed it.
        let gone: u64 = 0x00ff_fff0;
        let sig = "Lcom/example/Servlet;";
        let request = {
            let mut ds = shared.debug.debug_state.lock();
            ds.class_signatures.insert(gone, sig.to_string());
            ds.class_type_tags.insert(gone, 1);
            ds.class_methods.insert(gone, Vec::new());
            ds.method_line_tables.insert((gone, 7), vec![(0, 1)]);
            ds.events.set_event_request(
                events::EventKind::ClassUnload,
                events::SuspendPolicy::None,
                vec![],
            )
        };
        let packets = class_unload_packets(&shared, true);
        assert_eq!(packets.len(), 1, "one event set for the one class");
        let JdwpPacket::Command {
            command_set,
            command,
            ref data,
            ..
        } = packets[0]
        else {
            unreachable!("an event set is a command packet");
        };
        assert_eq!((command_set, command), (64, 100));
        let mut want = protocol::PayloadWriter::new();
        want.put_u8(0);
        want.put_u32_be(1);
        want.put_u8(events::EventKind::ClassUnload as u8);
        want.put_u32_be(request);
        want.put_string(sig);
        assert_eq!(*data, want.into_bytes());
        {
            let mut ds = shared.debug.debug_state.lock();
            assert!(!ds.class_signatures.contains_key(&gone));
            assert!(!ds.class_methods.contains_key(&gone));
            assert!(ds.method_line_tables.is_empty());
            let mut by_sig = protocol::PayloadWriter::new();
            by_sig.put_string(sig);
            let res = commands::dispatch(
                commands::CS_VM,
                commands::CMD_VM_CLASSES_BY_SIGNATURE,
                &by_sig.into_bytes(),
                &mut ds,
            );
            assert_eq!(res.data, vec![0, 0, 0, 0], "no class listed");
            let res = commands::dispatch(
                commands::CS_REF_TYPE,
                commands::CMD_RT_SIGNATURE,
                &gone.to_be_bytes(),
                &mut ds,
            );
            assert_eq!(res.error_code, commands::ERR_INVALID_CLASS);
            // A session's first sweep forgets without reporting.
            ds.class_signatures.insert(gone, sig.to_string());
        }
        assert!(class_unload_packets(&shared, false).is_empty());
        assert!(!shared.debug.debug_state.lock().class_signatures.contains_key(&gone));
    }

    /// Wave 25: `VirtualMachine.Version` answers the JDK the VM runs (JDWP
    /// 25.0, `java.version`, `java.vm.name`), 1.8 without the properties;
    /// `IsVirtual` refuses an id naming no thread.
    #[test]
    fn version_answers_the_jdk_the_vm_runs() {
        assert_eq!(jdwp_version_of("25"), Some((25, 0)));
        assert_eq!(jdwp_version_of("1.8"), Some((1, 8)));
        assert_eq!(jdwp_version_of("twenty"), None);
        let shared = Arc::new(crate::vm::SharedVm::new(crate::config::VmConfig::default()));
        {
            let mut props = shared.system_properties.write();
            props.insert("java.specification.version".to_string(), "25".to_string());
            props.insert("java.version".to_string(), "25.0.3".to_string());
            props.insert("java.vm.name".to_string(), "CratonVM".to_string());
        }
        let res = vm_version(&shared);
        assert_eq!(res.error_code, commands::ERR_NONE);
        let mut r = protocol::PayloadReader::new(&res.data);
        assert!(r.read_string().is_ok_and(|d| d.contains("25.0.3")));
        assert_eq!(r.read_u32_be().ok(), Some(25));
        assert_eq!(r.read_u32_be().ok(), Some(0));
        assert_eq!(r.read_string().ok().as_deref(), Some("25.0.3"));
        assert_eq!(r.read_string().ok().as_deref(), Some("CratonVM"));
        let res = thread_is_virtual(&shared, &0x7fff_0000u64.to_be_bytes());
        assert_eq!(res.error_code, commands::ERR_INVALID_THREAD);
    }

    /// Interpreter round i1 wave 24: `Method.Bytecodes` answers the method's
    /// class-file code (`NATIVE_METHOD` for a native one, unknown ids
    /// refused); it answered zero bytes for every method.
    #[test]
    fn method_bytecodes_answers_the_class_file_code() {
        use commands::{
            CMD_M_BYTECODES, CS_METHOD, ERR_INVALID_CLASS, ERR_INVALID_METHODID, ERR_NATIVE_METHOD,
            ERR_NONE,
        };
        use cratonvm_reader::attribute::{Attribute, CodeAttribute, LazyAttribute};
        use cratonvm_reader::class_access_flags::MethodAccessFlags as M;
        let shared = crate::vm::SharedVm::new(crate::config::VmConfig::default());
        let id = {
            let mut cm = shared.classes.class_manager.write();
            let id = cm
                .try_ensure_synthetic_class("cratonvm/test/W24Code", 0)
                .expect("Compatible mode fabricates");
            let class = cm.get_class_mut(id).expect("just added");
            class.methods = vec![
                cratonvm_reader::method::ClassFileMethod {
                    access_flags: M::STATIC,
                    name: Arc::from("run"),
                    descriptor: Arc::from("()V"),
                    attributes: vec![LazyAttribute::Decoded(Attribute::Code(CodeAttribute {
                        max_stack: 1,
                        max_locals: 1,
                        code: cratonvm_reader::ByteView::from_vec(vec![0x03, 0x3b, 0xb1]),
                        exception_table: vec![],
                        attributes: vec![],
                    }))],
                },
                cratonvm_reader::method::ClassFileMethod {
                    access_flags: M::NATIVE,
                    name: Arc::from("nat"),
                    descriptor: Arc::from("()V"),
                    attributes: vec![],
                },
            ];
            u64::from(id.as_u32())
        };
        let ask = |class: u64, method: u64| {
            let mut pw = protocol::PayloadWriter::new();
            pw.put_u64_be(class);
            pw.put_u64_be(method);
            serve_with_the_vm(&shared, CS_METHOD, CMD_M_BYTECODES, &pw.into_bytes())
                .expect("served by the VM")
        };
        let res = ask(id, jdwp_method_id("run", "()V"));
        assert_eq!(res.error_code, ERR_NONE);
        assert_eq!(res.data, vec![0, 0, 0, 3, 0x03, 0x3b, 0xb1]);
        assert_eq!(ask(id, jdwp_method_id("nat", "()V")).error_code, ERR_NATIVE_METHOD);
        assert_eq!(ask(id, jdwp_method_id("gone", "()V")).error_code, ERR_INVALID_METHODID);
        assert_eq!(ask(u64::from(u32::MAX), 1).error_code, ERR_INVALID_CLASS);
    }

    /// Interpreter round i1 wave 24: the `VM_DEATH` event set is
    /// `requestID` 0 and nothing else after the kind, with `SUSPEND_NONE`
    /// (HotSpot 25: `00 00000001 63 00000000`).
    #[test]
    fn vm_death_is_request_zero_with_no_thread() {
        let set = [events::Event {
            request_id: 0,
            kind: events::EventKind::VMDeath,
            thread_id: 0,
            extra: Vec::new(),
        }];
        let JdwpPacket::Command { data, .. } =
            events::compose_event_packet(events::SuspendPolicy::None, &set)
        else {
            panic!("an event is a command packet");
        };
        assert_eq!(data, vec![0, 0, 0, 0, 1, 99, 0, 0, 0, 0]);
    }

    /// Interpreter round i1 wave 26: the `VM_DEATH` set lists the requests'
    /// events before the unsolicited one, as HotSpot 25 sends them, and a
    /// request's policy is applied — `SUSPEND_ALL` to the VM,
    /// `SUSPEND_EVENT_THREAD` to the dying thread when it is known — so the
    /// reporter holds the dying thread until the debugger resumes it.
    #[test]
    fn vm_death_set_lists_the_requests_first_and_applies_their_policy() {
        let mut ds = DebugState::new();
        let (set, held) = vm_death_event_set(&mut ds, Some(7));
        assert_eq!(set.iter().map(|e| e.request_id).collect::<Vec<_>>(), vec![0]);
        assert!(!held, "no request: nothing is suspended");

        let all = ds.events.set_event_request(
            events::EventKind::VMDeath,
            events::SuspendPolicy::All,
            Vec::new(),
        );
        let (set, held) = vm_death_event_set(&mut ds, None);
        assert_eq!(
            set.iter().map(|e| e.request_id).collect::<Vec<_>>(),
            vec![all, 0],
            "the request's event first"
        );
        assert!(set
            .iter()
            .all(|e| e.suspend_policy == events::SuspendPolicy::All));
        assert!(held);
        assert!(ds.is_thread_suspended(u64::MAX), "the VM is suspended");
        ds.resume_vm();
        assert!(!ds.is_thread_suspended(u64::MAX));

        assert!(ds.events.clear_event_request(all));
        ds.events.set_event_request(
            events::EventKind::VMDeath,
            events::SuspendPolicy::EventThread,
            Vec::new(),
        );
        let (_, held) = vm_death_event_set(&mut ds, None);
        assert!(!held, "no thread to suspend");
        let (_, held) = vm_death_event_set(&mut ds, Some(7));
        assert!(held);
        assert!(ds.is_thread_suspended(7), "the dying thread is suspended");
        assert!(!ds.is_thread_suspended(8), "and no other");
    }
}
