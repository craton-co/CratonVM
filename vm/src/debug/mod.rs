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
pub(crate) mod field_watch_scope;
pub mod ids;
pub(crate) mod jit_expansion_set;
pub mod inspect;
pub(crate) mod pop_frames;
pub mod protocol;
pub mod transport;

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use commands::{FieldInfo, MethodInfo};
use cratonvm_native_api::NativeHeapAccess as _;
use events::EventManager;
use ids::IdManager;
use ids::ThreadId;
use ids::{ObjectExport, ObjectTable};
use protocol::JdwpPacket;
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

/// Does an event of `kind` carry a thread (every kind but `VMDeath` and
/// `ClassUnload`, as `events::compose_event_packet` writes them)? Wave 104.
fn event_names_a_thread(kind: events::EventKind) -> bool {
    !matches!(
        kind,
        events::EventKind::VMDeath | events::EventKind::ClassUnload
    )
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
    // Interpreter round i5 wave 104 (lane L1): once `main` has ended, an
    // event of VM thread 0 is the `DestroyJavaVM` thread's, under its own
    // wire id (`ids::DESTROY_JAVA_VM_WIRE_ID`, which `thread_to_wire` sends
    // unchanged). Rewritten here, as the event is raised, so an event raised
    // by `main` before it ended keeps `main`'s id however late it is sent.
    let split = shared.debug.debugger_gates.destroy_java_vm_split();
    for (i, event) in set.iter_mut().enumerate() {
        event.set_follows = i != last;
        if split && event.thread_id == 0 && event_names_a_thread(event.kind) {
            event.thread_id = ids::DESTROY_JAVA_VM_WIRE_ID;
        }
    }
    if let Ok(guard) = shared.debug.debug_event_tx.lock() {
        if let Some(ref tx) = *guard {
            for event in set {
                let _ = tx.send(event);
            }
        }
    }
    // Interpreter round i5 wave 105 (lane L1, `CRATONVM_JDWP_PROMPT_EVENTS`):
    // the server writes the set now, as HotSpot's event helper does, not
    // when its wait for a command ends. A `ClassPrepare` this thread queued
    // went out after the thread ran on: when the debugger's next command came
    // first (`L1W102JdiUnpreparedClassesHidden`, 3 runs in 24), its reply
    // reported state the thread reached after the event (`phase = 4`) while
    // JDI had not seen the class yet. After the event's own lock, so the
    // server never finds the wake before the events. One wake in flight at
    // a time (`jdwp_server_wake_pending`): the loop clears it before it
    // drains, so these events, queued before the flag was read, are drained
    // by the wake already on its way.
    if !shared
        .debug
        .jdwp_server_wake_pending
        .swap(true, std::sync::atomic::Ordering::AcqRel)
    {
        if let Ok(guard) = shared.debug.jdwp_server_wake.lock() {
            if let Some(ref wake) = *guard {
                let _ = wake.send(ServerInput::Wake);
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

/// The smallest `DebugState::state_suspend_prune_at` (interpreter round i7
/// wave 136, lane L1b): below this many C JVMTI state-word subscriptions no
/// sweep for gone threads is made.
const STATE_SUSPEND_PRUNE_FLOOR: usize = 64;

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
    /// The threads waiting out a suspension on their own word
    /// (`GcBlockState::debugger_suspend_word`; interpreter round i2 wave 63,
    /// lane L6, `CRATONVM_DEBUG_THREAD_SUSPEND_WORD`): enrolled by
    /// [`SuspendWordWait::enroll`] when such a wait starts, gone when it
    /// ends. Every change of what [`Self::suspend_word_flags`] answers for an
    /// enrolled thread rewrites its word and unparks it
    /// ([`Self::republish_suspend_word`]). Empty without the switch, and
    /// then every republication is one `is_empty` test.
    suspend_words: HashMap<u64, SuspendWordEntry>,
    /// The threads whose `GcBlockState::jvmti_state_suspended` a C JVMTI
    /// `GetThreadState` subscribed (interpreter round i7 wave 135, lane L1,
    /// `CRATONVM_JVMTI_STATE_SUSPEND_WORD`, default off; i64-L1 item 1):
    /// [`Self::republish_suspend_word`] / [`Self::republish_suspend_words`]
    /// rewrite each one's answer to [`Self::is_thread_suspended`] under the
    /// lock, enrolled or not. Empty without the switch: one `is_empty` test.
    state_suspend_words: HashMap<u64, Arc<crate::threading::jvm_thread::GcBlockState>>,
    /// The size at which [`Self::subscribe_state_suspend_word`] next drops
    /// the subscriptions of threads whose `JvmThread` is gone
    /// (`GcBlockState::owner_dropped`; interpreter round i7 wave 136, lane
    /// L1b): a `GetThreadState` made after the thread's `ThreadDeath`
    /// unsubscribed it (`forget_jvmti_suspension`) but before it left the
    /// registry subscribed it again, for good. Doubles after each sweep, so
    /// the sweeps cost O(1) amortised per subscription.
    state_suspend_prune_at: usize,
    /// [`Self::any_suspension`], kept current by every mutator of a suspend
    /// count, a JVMTI suspension or a stack-read handshake (each calls
    /// [`Self::republish_suspend_word`] or [`Self::republish_suspend_words`]
    /// before it returns, under the lock its caller holds), and readable
    /// without the debug-state lock through the handle the VM keeps
    /// (`DebugRealm::suspension_hint`; interpreter round i3 wave 72, lane L6,
    /// `CRATONVM_JVMTI_STATE_SUSPENSION_HINT`). Down: no thread is suspended,
    /// so C JVMTI `GetThreadState` adds no `SUSPENDED` bit without taking the
    /// lock. Up: the reader asks under the lock, as before.
    suspension_hint: Arc<std::sync::atomic::AtomicBool>,
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
    /// `CRATONVM_JDWP_INCLUDE_VIRTUAL_THREADS`, read once when the state is
    /// made (interpreter round i1 wave 49, lane L1): `AllThreads` and
    /// `ThreadGroupReference.Children` list the virtual threads too, as
    /// HotSpot's back end does under `includevirtualthreads=y`
    /// (`commands::listed_thread_ids`). Off: HotSpot's default.
    pub(crate) include_virtual_threads: bool,
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
    /// zt t18/df7 (x13 item 21a): the slots below [`Self::class_slots_seen`]
    /// skipped because their class was nameless (an unbound define, or a
    /// withdrawn one: `ClassStore::is_nameless`). Each poll looks at them
    /// again and describes the ones bound since; a withdrawn one stays here
    /// until its loader unloads it. Empty unless a define door's upcall was in
    /// flight at a poll.
    pub class_slots_nameless: Vec<u32>,
    /// The lambda proxy ids (`ClassRealm::next_lambda_id`) whose classes the
    /// tables above already describe, and those minted but not yet linked
    /// (`register_lambda_proxy_metadata`; the i54-L1 retirement).
    pub lambda_ids_seen: u32,
    pub lambda_ids_pending: Vec<u32>,
    /// Wire ref-type ID → JDWP `ClassStatus` bits (`jdwp_class_status`),
    /// refreshed from the class manager before the commands that answer a
    /// class's status (interpreter round i1 wave 15,
    /// `refresh_class_statuses`). A class missing here (a bare
    /// `DebugState`) answers VERIFIED | PREPARED, what every class answered
    /// before.
    pub class_statuses: HashMap<u64, u32>,
    /// The classes `AllClassesWithGeneric` and `ClassesBySignature` leave
    /// out (interpreter round i5 wave 103, lane L1): loaded but not
    /// prepared ([`jdwp_class_unprepared`]), as HotSpot's back end filters
    /// out a class whose JVMTI status has neither `PREPARED` nor `ARRAY`.
    /// Refreshed with [`Self::class_statuses`]; empty in a bare
    /// `DebugState` and with `CRATONVM_JDWP_HIDE_UNPREPARED_CLASSES=0`.
    pub(crate) unprepared_classes: HashSet<u64>,
    /// `CRATONVM_JDWP_HIDE_UNPREPARED_CLASSES` (default on; `=0` lists every
    /// loaded class and lets the server poll report a class still
    /// unprepared after [`CLASS_PREPARE_GRACE`], as before wave 103). Read
    /// once when the state is made.
    pub(crate) hide_unprepared_classes: bool,
    /// The class store's slot count when a JDWP `ClassPrepare` request
    /// came in force, while one stays in force (interpreter round i5 wave
    /// 104, lane L1; [`note_class_prepare_watermark`]). A class whose slot
    /// is at or past it was defined while `ClassPrepare` was observed, so
    /// `vm::class_linked_or_link_trivial` never let code use it unlinked:
    /// one still `Loaded` has run nothing and is hidden even when its link
    /// is trivial ([`jdwp_class_unprepared`]), as HotSpot hides a JDK class
    /// loaded by `Class.forName(n, false, null)`. `None` while no request is
    /// in force (class ids are never reused, so a slot orders definitions).
    pub(crate) class_prepare_watermark: Option<usize>,
    /// `CRATONVM_DEBUG_DESTROY_JAVA_VM_IDENTITY` (interpreter round i5 wave
    /// 104, lane L1; default on): `main`'s end posts JVMTI `ThreadEnd` and
    /// JDWP `ThreadDeath`, the `DestroyJavaVM` attach JVMTI and JDWP
    /// `ThreadStart`, and the two get their own JDWP ids, as HotSpot's
    /// launcher detaches `main` and attaches `DestroyJavaVM`
    /// ([`note_main_thread_ended`], [`note_destroy_java_vm_attached`]); `=0`
    /// posts none of them and keeps one wire id for both, as before. Read
    /// once when the state is made.
    pub(crate) destroy_java_vm_identity: bool,
    /// `main` has ended and VM thread 0 is the `DestroyJavaVM` thread
    /// (wave 104, [`note_main_thread_ended`]): the ended `main`'s name, which
    /// [`populate_thread_metadata`] lists under `ids::ENDED_MAIN_VM_ID` as a
    /// ZOMBIE; the wire ids of both follow `ids::thread_to_wire_split` /
    /// `thread_from_wire_split`. `None` before (and with the switch off).
    pub(crate) destroy_java_vm_split: Option<String>,
    /// The ended `main`'s `Thread`, as a weak JNI global reference (0: none),
    /// once [`Self::destroy_java_vm_split`] is set (wave 104): the object
    /// `main`'s id names when a command takes it as an object id
    /// ([`thread_object_for_id`]); it is no longer the registry entry's.
    pub(crate) ended_main_handle: u64,
    /// `CRATONVM_JDWP_HEAP_WALK_FROM_ROOTS` (interpreter round i5 wave 104,
    /// lane L1; default on): `InstanceCounts`, `Instances` and
    /// `ReferringObjects` answer the objects reachable from the heap roots,
    /// as HotSpot's `FollowReferences` walks; `=0` answers every heap object,
    /// garbage not yet collected included, as wave 103 did. Read once when
    /// the state is made.
    pub(crate) heap_walk_from_roots: bool,
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
    /// The published rows that are a lambda proxy method's frame, `(thread,
    /// frame id)` (interpreter round i7 wave 138, lane L1;
    /// `VmConfig::jdwp_lambda_proxy_steps`), with its locals (a parked
    /// thread's listing) or without (a blocked thread's, also in
    /// [`Self::opaque_frames`]). A step counts such a row as a frame either
    /// way, as the interpreter's suspend point does
    /// (`interpreter::debugger_step_depth`); the commands that find a frame
    /// among the thread's interpreter frames by position (`SetValues`,
    /// `PopFrames`) skip it, as it has no `Frame`. Withdrawn with the
    /// thread's snapshot.
    pub proxy_frames: HashSet<(u64, u64)>,
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
    /// A C JVMTI agent's `NotifyFramePop` / `ClearAllFramePops` of a thread
    /// suspended while blocked in a native (interpreter round i2 wave 61,
    /// lane L1; `inspect::defer_jvmti_frame_pop`): thread id → the requests,
    /// applied by the thread as it leaves the blocking region, where it
    /// applies [`Self::deferred_frame_writes`]. A C agent's, so a debugger's
    /// detach leaves them. [`DebuggerGates::frame_writes_pending`] is up while
    /// either map is not empty ([`Self::frame_writes_waiting`]).
    pub(crate) deferred_frame_pops: HashMap<u64, DeferredFramePops>,
    /// The published compiled activations whose locals were read from their
    /// home slots (`interpreter::publish_frame_snapshot`, `ListingRow::CompiledHome`;
    /// interpreter round i6 wave 123, lane L2, stage 3b of
    /// `i43-L1-proposal-deoptimize-a-compiled-frame-for-the-frame-commands`):
    /// `(thread, frame id) ->` the row's body, frame base, call bci and local
    /// count. Withdrawn with the thread's snapshot, so a `cm_ptr` here names a
    /// body whose activation is live on that (parked) thread.
    pub(crate) compiled_home_rows: HashMap<(u64, u64), CompiledHomeRowFacts>,
    /// `StackFrame.SetValues` writes to compiled activations, waiting for
    /// each activation's post-call exit to rebuild it as an interpreter frame
    /// (wave 123, lane L2; `inspect::set_compiled_home_values` records,
    /// `inspect::apply_compiled_home_writes` applies at the deopt resume):
    /// thread id -> the writes, in the order they were sent. An object value
    /// is held by its id, collection disabled and pinned, until then.
    /// [`DebuggerGates::compiled_home_writes_pending`] is up while this is
    /// not empty.
    pub(crate) compiled_home_writes: HashMap<u64, Vec<CompiledHomeWrite>>,
    /// Threads a suspension's settle wait ([`await_suspension_settled`],
    /// interpreter round i1 wave 49, lane L1) gave up on: suspended while
    /// running, they reached neither their suspend point nor a blocking
    /// region within [`SUSPEND_POINT_WAIT`] (a thread in a native that
    /// blocks outside a region, a service thread that runs no bytecode). No
    /// later wait of the session waits for them again. Cleared with the
    /// session.
    pub(crate) suspend_laggards: HashSet<u64>,
    /// `CRATONVM_DEBUG_SETTLE_PENDING_COUNTER` (interpreter round i6 wave 117,
    /// lane L1; part 3 of
    /// `i101-L1-proposal-per-vm-process-tables-and-a-settled-suspension-word`):
    /// the threads the settle wait in progress ([`await_suspension_settled`])
    /// found on their way, registered in the same critical section as its
    /// first walk. A thread's listing publication removes it and pushes it
    /// on [`Self::settle_arrived`] ([`note_settle_arrival`]). Empty outside a
    /// wait.
    pub(crate) settle_wanted: HashSet<u64>,
    /// The threads of [`Self::settle_wanted`] that published their listing
    /// since the wait last drained this (in arrival order).
    pub(crate) settle_arrived: Vec<u64>,
    /// `CRATONVM_JDWP_EVENT_OBJECT_HOLD` (interpreter round i6 wave 127, lane
    /// L1; parity fix, default ON, `=0` off; page
    /// `docs/feature-designs/jvmti-delivery-threading.md`),
    /// read once when the state is made: the object an `Exception` event
    /// names is held with its collection disabled until the debugger's
    /// first heap command naming it ([`hold_event_object`]). Plain data.
    pub(crate) event_object_hold: bool,
    /// The object ids [`hold_event_object`] holds, oldest first. Each goes at
    /// the first heap command that names it ([`release_event_hold_named`]),
    /// the oldest past [`EVENT_OBJECT_HOLD_CAP`], all of them with the
    /// session.
    pub(crate) event_object_holds: std::collections::VecDeque<u64>,

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

/// A C JVMTI agent's frame-pop requests of a blocked thread (interpreter
/// round i2 wave 61, lane L1; [`DebugState::deferred_frame_pops`]).
#[derive(Debug, Clone, Default)]
pub(crate) struct DeferredFramePops {
    /// `ClearAllFramePops`: the thread's requests are dropped first.
    pub(crate) clear: bool,
    /// `NotifyFramePop`s, each frame as a [`DeferredFrameWrite`] names it
    /// (its slot unused).
    pub(crate) frames: Vec<DeferredFrameWrite>,
    /// Interpreter round i6 wave 134, lane L1 (i133-L1): a `PopFrame` /
    /// `ForceEarlyReturn*` of the thread's top frame, made while it was
    /// suspended blocked entering a monitor
    /// (`early_return::defer_blocked_pop` / `defer_blocked_forced_return`),
    /// adopted at its wake
    /// (`early_return::adopt_blocked_frame_control`).
    pub(crate) control: Option<DeferredFrameControl>,
}

/// A C JVMTI agent's `PopFrame` / `ForceEarlyReturn*` of the top frame of a
/// thread suspended while blocked entering a monitor (interpreter round i6
/// wave 134, lane L1; [`DeferredFramePops::control`]).
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct DeferredFrameControl {
    /// The thread's interpreter frame count at the record: the frame is the
    /// top one, `frame_count - 1`.
    pub(crate) frame_count: usize,
    /// The top frame's class id and JDWP method id, checked again at the wake.
    pub(crate) class_id: u64,
    pub(crate) method_id: u64,
    /// `PopFrame`: whether the popped frame's invoke is an `invokestatic`.
    pub(crate) pop: Option<bool>,
    /// `ForceEarlyReturn*`: the value; a global reference the record owns.
    pub(crate) forced: Option<early_return::JvmtiForced>,
    /// The forced return was made after the `PopFrame` of this record: the
    /// popped frame's caller returns it, as at a park (`record_jvmti`).
    pub(crate) forced_after_pop: bool,
}

/// What a `SetValues` to a published compiled-home row needs of it
/// ([`DebugState::compiled_home_rows`]; interpreter round i6 wave 123, lane
/// L2).
#[derive(Debug, Clone, Copy)]
pub(crate) struct CompiledHomeRowFacts {
    /// The activation's body and frame base.
    pub(crate) body: crate::threading::jvm_thread::CompiledHomeBody,
    /// The bci the activation stands at: the call it is suspended in.
    pub(crate) call_bci: u32,
    /// How many locals its homes describe.
    pub(crate) locals: usize,
}

/// One `StackFrame.SetValues` write to a compiled activation, pending until
/// its post-call exit rebuilds it ([`DebugState::compiled_home_writes`];
/// interpreter round i6 wave 123, lane L2). Matched at the rebuild by the
/// frame base and the bci the exit resumes at, and by the body that left (or,
/// when the stash did not name it, by the frame's class and method).
#[derive(Debug, Clone, Copy)]
pub(crate) struct CompiledHomeWrite {
    /// The activation's body and frame base, as listed. `cm_ptr` is only
    /// compared here: the activation may be gone by the time a stale record
    /// is dropped.
    pub(crate) body: crate::threading::jvm_thread::CompiledHomeBody,
    /// The bci of the post-call exit's map (`DebuggerForceVerdict::Leaves`).
    pub(crate) exit_bci: u32,
    /// The frame's class id and JDWP method id.
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
    /// The object a JNI global reference names (interpreter round i1 wave
    /// 47, lane L1): a C JVMTI agent's `SetLocalObject` on a thread blocked
    /// in a native region (`inspect::defer_jvmti_object_write`). The
    /// reference (a root, remapped by every collection) is deleted when the
    /// write is applied or dropped.
    Global(u64),
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
            suspend_words: HashMap::new(),
            state_suspend_words: HashMap::new(),
            state_suspend_prune_at: STATE_SUSPEND_PRUNE_FLOOR,
            suspension_hint: Arc::new(std::sync::atomic::AtomicBool::new(false)),
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
            include_virtual_threads: cratonvm_types::flags::runtime_flag_on(
                "CRATONVM_JDWP_INCLUDE_VIRTUAL_THREADS",
            ),
            next_packet_id: 1,
            class_type_tags: HashMap::new(),
            class_signatures: HashMap::new(),
            class_source_files: HashMap::new(),
            class_fields: HashMap::new(),
            class_methods: HashMap::new(),
            class_slots_seen: 0,
            class_slots_nameless: Vec::new(),
            lambda_ids_seen: crate::vm::realms::class_realm::LAMBDA_PROXY_ID_BASE,
            lambda_ids_pending: Vec::new(),
            class_statuses: HashMap::new(),
            unprepared_classes: HashSet::new(),
            hide_unprepared_classes: cratonvm_types::flags::runtime_flag_default_on(
                "CRATONVM_JDWP_HIDE_UNPREPARED_CLASSES",
            ),
            class_prepare_watermark: None,
            destroy_java_vm_identity: cratonvm_types::flags::runtime_flag_default_on(
                "CRATONVM_DEBUG_DESTROY_JAVA_VM_IDENTITY",
            ),
            destroy_java_vm_split: None,
            ended_main_handle: 0,
            heap_walk_from_roots: cratonvm_types::flags::runtime_flag_default_on(
                "CRATONVM_JDWP_HEAP_WALK_FROM_ROOTS",
            ),
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
            proxy_frames: HashSet::new(),
            deferred_frame_writes: HashMap::new(),
            deferred_frame_pops: HashMap::new(),
            compiled_home_rows: HashMap::new(),
            compiled_home_writes: HashMap::new(),
            suspend_laggards: HashSet::new(),
            settle_wanted: HashSet::new(),
            settle_arrived: Vec::new(),
            event_object_hold: cratonvm_types::flags::runtime_flag_default_on(
                "CRATONVM_JDWP_EVENT_OBJECT_HOLD",
            ),
            event_object_holds: std::collections::VecDeque::new(),
            class_superclass: HashMap::new(),
            method_line_tables: HashMap::new(),
            method_variables: HashMap::new(),
            method_bytecodes: HashMap::new(),
            class_details: HashMap::new(),
            vm_bridge: None,
        }
    }

    /// Does a deferred frame write or frame-pop request wait for some
    /// blocked thread (wave 61)? What [`DebuggerGates::frame_writes_pending`]
    /// publishes.
    pub(crate) fn frame_writes_waiting(&self) -> bool {
        !self.deferred_frame_writes.is_empty() || !self.deferred_frame_pops.is_empty()
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

    /// [`Self::known_classes`] minus the loaded-but-unprepared ones
    /// ([`Self::unprepared_classes`]): what `AllClassesWithGeneric` lists
    /// (interpreter round i5 wave 103, lane L1; HotSpot's `allClasses1`
    /// "Filter out unprepared classes").
    pub fn listed_classes(&self) -> Vec<(u64, u8, &str)> {
        let mut classes = self.known_classes();
        if !self.unprepared_classes.is_empty() {
            classes.retain(|(id, _, _)| !self.unprepared_classes.contains(id));
        }
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

    /// Must thread `tid`, which has just won a contended monitor, give it back
    /// and wait (interpreter round i2 wave 54, lane L6;
    /// [`exit_monitor_while_suspended`])? Suspended by the debugger (JDWP) or
    /// by a C JVMTI agent's `SuspendThread` -- HotSpot's `ExitOnSuspend`
    /// covers both, which are one suspension there -- but not by a stack
    /// read's handshake, which holds the thread only while its stack is read.
    pub(crate) fn holds_off_monitor_entry(&self, tid: u64) -> bool {
        self.suspend_count(tid) > 0
            || (!self.jvmti_suspended.is_empty() && self.jvmti_suspended.contains(&tid))
    }

    /// Is thread `tid` suspended by a C JVMTI agent: its `SuspendThread`, or
    /// a stack read's handshake (wave 45)?
    fn is_suspended_by_jvmti(&self, tid: u64) -> bool {
        (!self.jvmti_suspended.is_empty() && self.jvmti_suspended.contains(&tid))
            || (!self.jvmti_handshakes.is_empty() && self.jvmti_handshakes.contains_key(&tid))
    }

    /// What thread `tid`'s suspend word says, without its change sequence
    /// (interpreter round i2 wave 63, lane L6;
    /// `GcBlockState::debugger_suspend_word`): [`SW_ENROLLED`], with
    /// [`SW_SUSPENDED`] while [`Self::is_thread_suspended`] holds,
    /// [`SW_HOLDS_OFF`] while [`Self::holds_off_monitor_entry`] holds and
    /// [`SW_TASK`] while work is queued for it.
    pub(crate) fn suspend_word_flags(&self, tid: u64) -> u32 {
        let mut flags = SW_ENROLLED;
        if self.is_thread_suspended(tid) {
            flags |= SW_SUSPENDED;
        }
        if self.holds_off_monitor_entry(tid) {
            flags |= SW_HOLDS_OFF;
        }
        if !self.parked_tasks.is_empty() && self.parked_tasks.contains_key(&tid) {
            flags |= SW_TASK;
        }
        flags
    }

    /// Rewrite enrolled thread `tid`'s suspend word from this state and wake
    /// it when the word moved (interpreter round i2 wave 63, lane L6). Every
    /// mutator of a suspend count, a JVMTI suspension or a parked task calls
    /// this (or [`Self::republish_suspend_words`]) before it returns, under
    /// the lock its caller holds, so the word is always what the lock would
    /// answer. One `is_empty` test while no thread is enrolled.
    #[inline]
    pub(crate) fn republish_suspend_word(&self, tid: u64) {
        self.refresh_suspension_hint();
        // Interpreter round i7 wave 135, lane L1: a subscribed thread's C
        // JVMTI state word (`CRATONVM_JVMTI_STATE_SUSPEND_WORD`).
        if !self.state_suspend_words.is_empty() {
            if let Some(state) = self.state_suspend_words.get(&tid) {
                self.publish_state_suspend_word(tid, state);
            }
        }
        if self.suspend_words.is_empty() {
            return;
        }
        if let Some(entry) = self.suspend_words.get(&tid) {
            entry.publish(self.suspend_word_flags(tid));
        }
    }

    /// [`Self::republish_suspend_word`] of every enrolled thread: a VM-wide
    /// suspension or resume (`VirtualMachine.Suspend` / `Resume`, a
    /// `SUSPEND_ALL` event set, a detach). HotSpot's handshake walks every
    /// thread too; this walks only the threads waiting on their word.
    pub(crate) fn republish_suspend_words(&self) {
        self.refresh_suspension_hint();
        // Wave 135 (lane L1): and every subscribed C JVMTI state word.
        for (&tid, state) in &self.state_suspend_words {
            self.publish_state_suspend_word(tid, state);
        }
        for (&tid, entry) in &self.suspend_words {
            entry.publish(self.suspend_word_flags(tid));
        }
    }

    /// Store [`Self::is_thread_suspended`] of thread `tid` in its C JVMTI
    /// state word (interpreter round i7 wave 135, lane L1;
    /// `GcBlockState::jvmti_state_suspended`). Under the debug-state lock,
    /// before the mutator's caller releases it, as the hint is stored.
    #[inline]
    fn publish_state_suspend_word(
        &self,
        tid: u64,
        state: &crate::threading::jvm_thread::GcBlockState,
    ) {
        use crate::threading::jvm_thread::{JVMTI_STATE_SUSPEND_NO, JVMTI_STATE_SUSPEND_YES};
        let word = if self.is_thread_suspended(tid) {
            JVMTI_STATE_SUSPEND_YES
        } else {
            JVMTI_STATE_SUSPEND_NO
        };
        state
            .jvmti_state_suspended
            .store(word, std::sync::atomic::Ordering::Release);
    }

    /// Subscribe thread `tid`, whose `GcBlockState` is `state`, to the C
    /// JVMTI state word (interpreter round i7 wave 135, lane L1,
    /// `CRATONVM_JVMTI_STATE_SUSPEND_WORD`): its word is written now and by
    /// every later suspension mutator, so the next `GetThreadState` of it
    /// reads one atomic. Answers [`Self::is_thread_suspended`] now.
    pub(crate) fn subscribe_state_suspend_word(
        &mut self,
        tid: u64,
        state: Arc<crate::threading::jvm_thread::GcBlockState>,
    ) -> bool {
        // Wave 136 (lane L1b): a thread that asked after its `ThreadDeath`
        // unsubscribed it is subscribed again and never unsubscribed; drop
        // the subscriptions of threads that are gone, at a doubling size.
        if self.state_suspend_words.len() >= self.state_suspend_prune_at {
            self.state_suspend_words.retain(|_, s| !s.owner_dropped());
            self.state_suspend_prune_at = self
                .state_suspend_words
                .len()
                .saturating_mul(2)
                .max(STATE_SUSPEND_PRUNE_FLOOR);
        }
        self.publish_state_suspend_word(tid, &state);
        self.state_suspend_words.insert(tid, state);
        self.is_thread_suspended(tid)
    }

    /// Forget thread `tid`'s C JVMTI state word (wave 135; the thread ends):
    /// the word goes back to unpublished, so a reader that still finds the
    /// thread alive asks the lock, as without the switch.
    fn unsubscribe_state_suspend_word(&mut self, tid: u64) {
        if let Some(state) = self.state_suspend_words.remove(&tid) {
            state.jvmti_state_suspended.store(
                crate::threading::jvm_thread::JVMTI_STATE_SUSPEND_UNPUBLISHED,
                std::sync::atomic::Ordering::Release,
            );
        }
    }

    /// Is thread `tid` subscribed to the C JVMTI state word (wave 135)?
    /// Tests only.
    #[cfg(test)]
    pub(crate) fn state_suspend_word_subscribed(&self, tid: u64) -> bool {
        self.state_suspend_words.contains_key(&tid)
    }

    /// Store [`Self::any_suspension`] in [`Self::suspension_hint`]
    /// (interpreter round i3 wave 72, lane L6). Under the debug-state lock,
    /// before the mutator's caller releases it, so a reader that saw any
    /// suspension through the lock and then reads the hint finds it up.
    /// Written whatever the reader's switch says: one store per mutation.
    #[inline]
    fn refresh_suspension_hint(&self) {
        self.suspension_hint
            .store(self.any_suspension(), std::sync::atomic::Ordering::Release);
    }

    /// The handle of [`Self::suspension_hint`], for the VM to keep beside
    /// this state's lock (`DebugRealm::suspension_hint`).
    pub(crate) fn suspension_hint_handle(&self) -> Arc<std::sync::atomic::AtomicBool> {
        Arc::clone(&self.suspension_hint)
    }

    /// Enroll the calling thread, `tid`, whose `GcBlockState` is `state`, as
    /// waiting on its own suspend word (wave 63; [`SuspendWordWait::enroll`]).
    /// Nests: a park inside a task an outer park runs enrolls again.
    fn enroll_suspend_word(
        &mut self,
        tid: u64,
        state: &Arc<crate::threading::jvm_thread::GcBlockState>,
    ) {
        let me = std::thread::current();
        self.suspend_words
            .entry(tid)
            .or_insert_with(|| SuspendWordEntry {
                state: Arc::clone(state),
                wakers: Vec::new(),
            })
            .wakers
            .push(me);
        // A fresh enrollment's word is 0, so this always writes it.
        self.republish_suspend_word(tid);
    }

    /// End the calling thread's innermost enrollment of `tid` (wave 63). The
    /// last one clears the word and forgets the thread.
    fn leave_suspend_word(&mut self, tid: u64) {
        let me = std::thread::current().id();
        let Some(entry) = self.suspend_words.get_mut(&tid) else {
            return;
        };
        if let Some(at) = entry.wakers.iter().rposition(|t| t.id() == me) {
            entry.wakers.remove(at);
        } else {
            entry.wakers.pop();
        }
        if entry.wakers.is_empty() {
            entry
                .state
                .debugger_suspend_word
                .store(0, std::sync::atomic::Ordering::Release);
            self.suspend_words.remove(&tid);
        }
    }

    /// Is thread `tid` enrolled on its suspend word (wave 63)? Tests only.
    #[cfg(test)]
    pub(crate) fn suspend_word_enrolled(&self, tid: u64) -> bool {
        self.suspend_words.contains_key(&tid)
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
        let inserted = self.jvmti_suspended.insert(tid);
        self.republish_suspend_word(tid);
        inserted
    }

    /// JVMTI `ResumeThread` of thread `tid` (wave 45): `false` when the agent
    /// had not suspended it. A thread that runs now takes a new frame
    /// generation, as after a JDWP resume.
    pub(crate) fn jvmti_resume(&mut self, tid: u64) -> bool {
        let removed = self.jvmti_suspended.remove(&tid);
        if removed && !self.is_thread_suspended(tid) {
            bump_frame_generation(&mut self.frame_generations, tid, 1);
        }
        self.republish_suspend_word(tid);
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
        self.republish_suspend_word(tid);
        // Interpreter round i7 wave 135, lane L1: the ending thread's C JVMTI
        // state word is dropped (back to unpublished: a JDWP suspension its
        // `ThreadDeath` event set takes is then read under the lock).
        self.unsubscribe_state_suspend_word(tid);
        agent || handshake
    }

    /// Suspend thread `tid` for a C JVMTI stack read (wave 45).
    pub(crate) fn begin_handshake(&mut self, tid: u64) {
        *self.jvmti_handshakes.entry(tid).or_insert(0) += 1;
        self.republish_suspend_word(tid);
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
        self.republish_suspend_word(tid);
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
        self.republish_suspend_word(tid);
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
        self.republish_suspend_word(tid);
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
        self.republish_suspend_word(tid);
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

    /// Is `tid` one of the VM's virtual threads, as recorded at its start
    /// or by the thread-table refresh (interpreter round i1 wave 49)?
    pub(crate) fn is_virtual_thread(&self, tid: u64) -> bool {
        self.virtual_threads.contains(&tid)
    }

    /// Suspend every thread (`VirtualMachine.Suspend`, a `SUSPEND_ALL`
    /// event): every count, including the counts of threads not yet known,
    /// goes up by one.
    pub fn suspend_all(&mut self) {
        self.suspend_all_count = self.suspend_all_count.saturating_add(1);
        self.republish_suspend_words();
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
        self.republish_suspend_words();
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
        self.republish_suspend_words();
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
            self.republish_suspend_word(tid);
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

    /// The wire id of VM thread `tid`, with the `DestroyJavaVM` split once
    /// `main` has ended (interpreter round i5 wave 104, lane L1;
    /// [`Self::destroy_java_vm_split`], `ids::thread_to_wire_split`).
    #[inline]
    pub(crate) fn wire_thread(&self, tid: u64) -> u64 {
        ids::thread_to_wire_split(self.destroy_java_vm_split.is_some(), tid)
    }

    /// The VM thread a wire thread id names, with the `DestroyJavaVM` split
    /// (wave 104, `ids::thread_from_wire_split`).
    #[inline]
    pub(crate) fn vm_thread(&self, wire: u64) -> u64 {
        ids::thread_from_wire_split(self.destroy_java_vm_split.is_some(), wire)
    }

    /// The ended `main`'s `Thread` ([`Self::ended_main_handle`], wave 104),
    /// `None` once collected or before the split. As [`object_for_id`]
    /// reads a weak referent: the SATB barrier keeps it for a concurrent
    /// cycle.
    pub(crate) fn ended_main_thread_obj(
        &self,
        shared: &crate::vm::SharedVm,
    ) -> Option<crate::types::ObjectRef> {
        if self.ended_main_handle == 0 {
            return None;
        }
        let obj = shared
            .natives
            .jni_global_refs
            .lock()
            .resolve(self.ended_main_handle);
        if let Some(o) = obj {
            shared
                .mem
                .heap
                .referent_keep_alive(crate::types::Value::Object(Some(o)));
        }
        obj
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

    /// Is thread `tid`, parked on its suspend word, inside a GC-blocked
    /// region now (interpreter round i2 wave 66, lane L6)? Its park's region
    /// (`ParkGcRegion`) is the only one an idle park can stand in. Read
    /// lock-free from the `GcBlockState` the word enrollment holds; a thread
    /// not enrolled on its word never opens a park region.
    fn park_region_open(&self, tid: u64) -> bool {
        self.suspend_words.get(&tid).is_some_and(|entry| {
            entry
                .state
                .in_blocked_region
                .load(std::sync::atomic::Ordering::Acquire)
        })
    }

    /// The parked threads that can take work now ([`Self::can_take_work`]).
    fn idle_parked_threads(&self) -> impl Iterator<Item = u64> + '_ {
        self.parked_threads
            .keys()
            .copied()
            .filter(move |&t| self.can_take_work(t))
    }

    /// Where heap work for any thread goes (a [`run_on_parked_thread`] with
    /// no thread named; wave 8, wave 10, wave 66). `service_ok`: the heap
    /// service may take it (never an invocation).
    ///
    /// Without `outside_region` (wave 65 and before): the lowest-numbered
    /// idle parked thread, else the heap service. With it
    /// ([`DebuggerGates::heap_work_outside_region`], wave 66): the
    /// lowest-numbered idle parked thread whose GC-blocked region is closed,
    /// which runs the work without leaving one; else the heap service, whose
    /// region holds no Java frame and so costs next to nothing to leave;
    /// only then a park inside its region, which pays a leave and a deposit
    /// of its whole stack to re-enter.
    fn heap_work_target(&self, outside_region: bool, service_ok: bool) -> HeapWorkTarget {
        if outside_region {
            if let Some(t) = self
                .idle_parked_threads()
                .filter(|&t| !self.park_region_open(t))
                .min()
            {
                return HeapWorkTarget::Parked(t);
            }
            if service_ok && self.heap_service.is_some() {
                return HeapWorkTarget::Service;
            }
        }
        match self.idle_parked_threads().min() {
            Some(t) => HeapWorkTarget::Parked(t),
            None if service_ok && self.heap_service.is_some() => HeapWorkTarget::Service,
            None => HeapWorkTarget::Nowhere,
        }
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
        self.republish_suspend_word(tid);
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
            self.unprepared_classes.remove(id);
            self.class_superclass.remove(id);
            self.class_details.remove(id);
            self.class_prepare_reported.remove(id);
        }
        self.class_prepare_pending
            .retain(|((id, _, _), _)| !gone.contains(id));
        self.method_line_tables
            .retain(|(id, _), _| !gone.contains(id));
        self.method_variables
            .retain(|(id, _), _| !gone.contains(id));
        self.method_bytecodes
            .retain(|(id, _), _| !gone.contains(id));
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
/// Read the session's commands on a thread of their own, with blocking reads
/// and no timeout, and hand each whole packet (or the error that ended the
/// stream) to the JDWP server's loop through the returned channel
/// (interpreter round i2 wave 58, lane L6). Windows only: a Winsock receive
/// that times out leaves the socket indeterminate, so the server's
/// `SERVER_READ_WAIT` read timeout ended sessions with `os error 997`. The
/// thread ends at the first error or end of stream (the server shuts the
/// socket down when the session ends), or when the server's end of the
/// channel is gone. `None` when the socket cannot be cloned or the thread
/// cannot be started (the server then reads as before).
///
/// Interpreter round i5 wave 105 (lane L1, `CRATONVM_JDWP_PROMPT_EVENTS`,
/// default on): on every platform, and the second value is a sender into the
/// same channel, which [`send_event_set`] wakes the loop by
/// ([`ServerInput::Wake`]), so an event goes out as soon as it is queued,
/// not when the next command or `SERVER_READ_WAIT` ends the server's wait.
fn spawn_command_reader(
    stream: &std::net::TcpStream,
) -> Option<(
    std::sync::mpsc::Receiver<ServerInput>,
    std::sync::mpsc::Sender<ServerInput>,
)> {
    let mut socket = stream.try_clone().ok()?;
    let (tx, rx) = std::sync::mpsc::channel();
    let wake = tx.clone();
    std::thread::Builder::new()
        .name("cratonvm-jdwp-reader".to_string())
        .spawn(move || {
            let mut incoming = protocol::PacketAssembler::new();
            loop {
                match incoming.poll(&mut socket) {
                    Ok(Some(packet)) => {
                        if tx.send(ServerInput::Packet(Ok(packet))).is_err() {
                            return;
                        }
                    }
                    // A blocking read with no timeout does not end empty; if
                    // it ever does, read again.
                    Ok(None) => {}
                    Err(e) => {
                        let _ = tx.send(ServerInput::Packet(Err(e)));
                        return;
                    }
                }
            }
        })
        .ok()?;
    Some((rx, wake))
}

/// Must the events this command causes follow its reply, so the server
/// writes no queued event or class-prepare poll between serving it and
/// replying (interpreter round i5 wave 105, second hand-back)?
/// `EventRequest.Set` (an event of the new request before the reply would
/// name a request id the debugger does not know yet), the resumes (the
/// events of the threads they release) and `Dispose`.
fn events_may_follow_reply_only(command_set: u8, command: u8) -> bool {
    matches!(
        (command_set, command),
        (commands::CS_EVENT_REQUEST, commands::CMD_ER_SET)
            | (commands::CS_VM, commands::CMD_VM_RESUME)
            | (commands::CS_VM, commands::CMD_VM_DISPOSE)
            | (commands::CS_THREAD_REF, commands::CMD_TR_RESUME)
    )
}

/// What the JDWP server's loop waits for when its commands are read by
/// [`spawn_command_reader`]'s thread (interpreter round i5 wave 105, lane
/// L1): a whole command packet (or the error that ended the stream), or a
/// wake from [`send_event_set`] saying an event was queued.
pub enum ServerInput {
    /// A packet the debugger sent, or the read error that ended the stream.
    Packet(std::io::Result<JdwpPacket>),
    /// An event was queued on the server's channel: go round the loop, which
    /// writes it.
    Wake,
}

pub fn run_jdwp_server(shared: &crate::vm::SharedVm, port: u16) {
    let listener = match std::net::TcpListener::bind(("127.0.0.1", port)) {
        Ok(l) => l,
        Err(e) => {
            tracing::error!("JDWP: failed to bind port {port}: {e}");
            return;
        }
    };
    tracing::info!("JDWP: listening on 127.0.0.1:{port}");
    run_jdwp_sessions(shared, listener.incoming());
}

/// The JDWP server's sessions, one per connection `streams` yields, until it
/// yields none (interpreter round i4 wave 85, lane L1: split out of
/// [`run_jdwp_server`] so `-agentlib:jdwp` can serve a listener the VM bound
/// at `VMInit`, accepting again after each detach as HotSpot's `server=y`
/// back end does, or the one connection `server=n` made).
pub(crate) fn run_jdwp_sessions(
    shared: &crate::vm::SharedVm,
    streams: impl Iterator<Item = std::io::Result<std::net::TcpStream>>,
) {
    // This thread is not a registered mutator: it never runs Java and touches
    // the heap only inside `GcBarrier::run_if_no_stw_requested` (wave 8: the
    // invocations and the heap commands run on a parked thread,
    // `run_on_parked_thread`). The interpreter's suspend point must never
    // park it all the same (`debugger_hooks_suppressed`).
    JDWP_SERVER_THREAD.with(|f| f.set(true));

    for stream in streams {
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
        //
        // Interpreter round i2 wave 58, lane L6
        // (`docs/feature-designs/jvmti-delivery-threading.md`):
        // on Windows a receive that times out leaves the socket in an
        // indeterminate state (Winsock's `SO_RCVTIMEO`), and a later read
        // failed with `os error 997` (`ERROR_IO_PENDING`), which ended the
        // session in about one run in six of `L6W56JdiSuspendInRunningNative`
        // under `--nojit`. There the commands are read by a thread of their
        // own with no timeout ([`spawn_command_reader`]); the loop waits on its
        // channel for at most `SERVER_READ_WAIT`.
        //
        // Interpreter round i5 wave 105 (lane L1): with
        // `CRATONVM_JDWP_PROMPT_EVENTS` (default on; `=0` is the old loop) the
        // commands are read by that thread on every platform, and an event
        // queued while the loop waits wakes it ([`ServerInput::Wake`],
        // `DebugRealm::jdwp_server_wake`), so it is written at once.
        let prompt_events =
            cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JDWP_PROMPT_EVENTS");
        let reader = if (cfg!(windows) || prompt_events)
            && stream.set_nonblocking(false).is_ok()
            && stream.set_read_timeout(None).is_ok()
            && stream.set_write_timeout(Some(SERVER_WRITE_WAIT)).is_ok()
        {
            spawn_command_reader(&stream)
        } else {
            None
        };
        let reader = reader.map(|(commands_rx, wake)| {
            if prompt_events {
                if let Ok(mut guard) = shared.debug.jdwp_server_wake.lock() {
                    *guard = Some(wake);
                }
                // A flag left set by an earlier session (or by sets queued
                // with no wake installed) would silence the first wake.
                shared
                    .debug
                    .jdwp_server_wake_pending
                    .store(false, std::sync::atomic::Ordering::Release);
            }
            commands_rx
        });
        let blocking_reads = reader.is_some()
            || (stream.set_nonblocking(false).is_ok()
                && stream.set_read_timeout(Some(SERVER_READ_WAIT)).is_ok()
                && stream.set_write_timeout(Some(SERVER_WRITE_WAIT)).is_ok());
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
            // --- Interpreter round i4 wave 93 (lane L1): the classes defined
            // since the last poll join a class-scoped method event request's
            // scope (`CRATONVM_JDWP_SCOPED_METHOD_EVENTS`; nothing without it) ---
            refresh_method_event_scope_and_publish(shared);

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
                // Interpreter round i5 wave 101 (lane L1,
                // `DebuggerGates::invoke_reply_settle`): an invocation's
                // closing re-suspension completes before its reply, as
                // HotSpot's `invoker_completeInvokeRequest` suspends (a
                // handshake) before it sends. The invoking thread published
                // its listing before it answered (`park_for_debugger`'s task
                // arm), so only the threads that ran during the call are
                // waited for; with `INVOKE_SINGLE_THREADED` none was.
                if answered.iter().any(|(_, result)| result.error_code == 0)
                    && shared.debug.debugger_gates.invoke_reply_settle()
                {
                    await_suspension_settled(shared, None);
                }
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
            let polled = match &reader {
                Some(commands_rx) => match commands_rx.recv_timeout(SERVER_READ_WAIT) {
                    Ok(ServerInput::Packet(read)) => read.map(Some),
                    // Wave 105: an event was queued; the loop's top writes it.
                    // Cleared first, so a set queued from now on wakes again.
                    Ok(ServerInput::Wake) => {
                        shared
                            .debug
                            .jdwp_server_wake_pending
                            .store(false, std::sync::atomic::Ordering::Release);
                        Ok(None)
                    }
                    Err(std::sync::mpsc::RecvTimeoutError::Timeout) => Ok(None),
                    Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                        Err(std::io::Error::from(std::io::ErrorKind::UnexpectedEof))
                    }
                },
                None => incoming.poll(&mut stream),
            };
            let packet = match polled {
                Ok(Some(p)) => p,
                Ok(None) => {
                    if !blocking_reads {
                        std::thread::sleep(std::time::Duration::from_millis(5));
                    }
                    continue;
                }
                Err(e) => {
                    // Interpreter round i2 wave 58, lane L6: a session that
                    // ends on anything but the debugger's close names why.
                    if e.kind() != std::io::ErrorKind::UnexpectedEof {
                        tracing::warn!(error = %e, "JDWP session ended: the command stream failed");
                    }
                    break;
                }
            };

            match packet {
                JdwpPacket::Command {
                    id,
                    command_set,
                    command,
                    mut data,
                    ..
                } => {
                    // Interpreter round i5 wave 105 (lane L1,
                    // `CRATONVM_JDWP_PROMPT_EVENTS`): the events queued before
                    // this command arrived go out before it is served, as if
                    // it had arrived a moment later. The wake
                    // (`ServerInput::Wake`) sits behind a command that was
                    // already in the reader's channel, and without this the
                    // command's reply could report state its program reached
                    // after an event the debugger had not yet received (a
                    // `ClassPrepare` queued before `Class.forName` returned,
                    // `L1W102JdiUnpreparedClassesHidden`). Events the command
                    // itself causes (a `Resume`) still follow its reply's
                    // serving, as before.
                    if prompt_events
                        && !shared.debug.debug_state.lock().events_held
                        && !drain_interpreter_events(&mut stream, shared, &rx)
                    {
                        break;
                    }
                    // The thread and class ids the command names, from their
                    // wire form to the VM's (wave 25: id 0 is JDWP's null;
                    // `ids::thread_from_wire`, `ids::class_from_wire`).
                    // Wave 104 (lane L1): with the `DestroyJavaVM` split.
                    ids_from_wire_split(
                        command_set,
                        command,
                        &mut data,
                        shared.debug.debugger_gates.destroy_java_vm_split(),
                    );
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
                    // The i54-L1 retirement: a lambda proxy class a listing
                    // or a reference names is described before any command
                    // asks about it.
                    register_lambda_proxy_metadata(shared);
                    if reads_class_status(command_set, command) {
                        refresh_class_statuses_for(shared, command_set, command, &data);
                    }
                    if reads_thread_frames(command_set, command) {
                        publish_blocked_frames_for(shared, &data);
                    }
                    let result =
                        if let Some(refused) = unknown_id_refusal(shared, command_set, &data) {
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

                    // Interpreter round i4 wave 93 (lane L1): a method event
                    // request the command set or cleared changes the class
                    // scopes; resolved before the publication reads them, and
                    // before the reply, as a request is in force once its
                    // `EventRequest.Set` is answered.
                    let _ = refresh_method_event_scope(shared);
                    // Wave 95 (lane L1): and a field watch's
                    // (`CRATONVM_JDWP_SCOPED_FIELD_WATCHES`).
                    let _ = field_watch_scope::refresh(shared);
                    // Republish the interpreter's gate and the per-method
                    // summary (`publish_debugger_gates`), and free the
                    // handles of object ids the command disposed of.
                    {
                        let mut ds = shared.debug.debug_state.lock();
                        publish_debugger_gates(shared, &ds);
                        release_disposed_objects(shared, &mut ds);
                    }
                    // Interpreter round i5 wave 104 (lane L1): a
                    // `ClassPrepare` request the command set or cleared
                    // moves the watermark of classes defined while one is
                    // in force (no lock held here).
                    note_class_prepare_watermark(shared);
                    // A barrier this thread ran (a weak-handle keep-alive, a
                    // store) buffered its SATB entry thread-locally, and this
                    // thread never reaches a safepoint that would flush it.
                    shared.mem.heap.flush_thread_satb();
                    // Wave 49 (lane L1): a suspension completes before its
                    // reply, as HotSpot's handshake does (the gates the
                    // threads park by were republished just above).
                    if result.error_code == 0 {
                        match (command_set, command) {
                            (commands::CS_VM, commands::CMD_VM_SUSPEND) => {
                                await_suspension_settled(shared, None);
                            }
                            (commands::CS_THREAD_REF, commands::CMD_TR_SUSPEND) => {
                                if let Ok(tid) = protocol::PayloadReader::new(&data).read_u64_be() {
                                    await_suspension_settled(shared, Some(tid));
                                }
                            }
                            _ => {}
                        }
                    }

                    // Interpreter round i5 wave 105, second hand-back (lane
                    // L1, `CRATONVM_JDWP_PROMPT_EVENTS`): every class the VM
                    // prepared and every event a thread queued BEFORE this
                    // command was served goes out before its reply, whichever
                    // path reports it (the preparing thread's
                    // `class_prepared_on_thread`, or this server's poll for a
                    // class prepared where nothing posted, `send_class_prepare_events`).
                    // The command read the VM's state after those threads
                    // queued their events, so a reply about that state (a
                    // `phase` the program wrote after `Class.forName`
                    // returned, `L1W105JdiClassPrepareBeforeAck`) can no
                    // longer overtake them: ordered by construction on the one
                    // writer, with no wait on the posting thread. Not for a
                    // command that itself releases or creates events (a
                    // resume, `Dispose`, `EventRequest.Set`, whose request id
                    // the debugger learns from this reply): their events
                    // follow the reply, at the loop's top, as before.
                    if prompt_events && !events_may_follow_reply_only(command_set, command) {
                        let held_now = shared.debug.debug_state.lock().events_held;
                        if !held_now {
                            if !send_class_prepare_events(&mut stream, shared) {
                                break;
                            }
                            if !drain_interpreter_events(&mut stream, shared, &rx) {
                                break;
                            }
                        }
                    }

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
        // Wave 58 (lane L6): a command reader blocked in its read ends with
        // the socket (it holds a clone of it).
        if reader.is_some() {
            let _ = stream.shutdown(std::net::Shutdown::Both);
        }

        // Tear down the event channel
        {
            if let Ok(mut guard) = shared.debug.debug_event_tx.lock() {
                *guard = None;
            }
            // Wave 105: and the loop's wake.
            if let Ok(mut guard) = shared.debug.jdwp_server_wake.lock() {
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
            // Wave 104 (lane L1): no request is in force any more, so a
            // class defined from now on may be used unlinked.
            ds.class_prepare_watermark = None;
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
            ds.proxy_frames.clear();
            // Wave 123 (L2): and the compiled-home rows with them. A write to
            // a compiled activation whose object is a session id dies with
            // the session (the id's hold goes with `release_all`); a
            // primitive one stays, as a blocked thread's does.
            ds.compiled_home_rows.clear();
            for writes in ds.compiled_home_writes.values_mut() {
                writes.retain(|w| !matches!(w.value, DeferredLocal::Object(_)));
            }
            ds.compiled_home_writes
                .retain(|_, writes| !writes.is_empty());
            shared
                .debug
                .debugger_gates
                .set_compiled_home_writes_pending(!ds.compiled_home_writes.is_empty());
            // Wave 41: writes a blocked thread has not applied yet whose
            // object is a session id die with the session; the ids' holds go
            // with `release_all`. Interpreter round i2 wave 61 (lane L1): the
            // others stay, a C agent's `SetLocalObject` (held by a JNI global
            // reference of its own, which `clear()` leaked, the write lost)
            // and a primitive write, which HotSpot made at once.
            for writes in ds.deferred_frame_writes.values_mut() {
                writes.retain(|w| !matches!(w.value, DeferredLocal::Object(_)));
            }
            ds.deferred_frame_writes
                .retain(|_, writes| !writes.is_empty());
            // Wave 49: the next session waits for every thread again.
            ds.suspend_laggards.clear();
            // Wave 117: no settle wait outlives its session.
            ds.settle_wanted.clear();
            ds.settle_arrived.clear();
            // Wave 61: a C agent's frame-pop requests stay.
            shared
                .debug
                .debugger_gates
                .set_frame_writes_pending(ds.frame_writes_waiting());
            // Wave 127 (lane L1): the event holds go with their ids.
            ds.event_object_holds.clear();
            ds.objects.release_all();
            release_disposed_objects(shared, &mut ds);
            // (Wave 11: the "exception already reported" handles live on
            // their threads, `JvmThread::reported_exception`, freed by the
            // thread's next catch or reported throw, or its exit, wave 12.)
            publish_debugger_gates(shared, &ds);
        }
        // Interpreter round i4 wave 93 (lane L1): the detached session's
        // class-scoped method event requests went with it.
        refresh_method_event_scope_and_publish(shared);
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
            // `VM_START` is `requestID, thread` too (wave 85).
            events::EventKind::ThreadStart
            | events::EventKind::ThreadDeath
            | events::EventKind::VMStart
            | events::EventKind::VMDeath => Vec::new(),
            // No location: its producer encoded the class (wave 10,
            // `class_prepared_on_thread`).
            events::EventKind::ClassPrepare => evt.extra.clone(),
            // Interpreter round i5 wave 103 (lane L1): a monitor event is
            // `thread, object, location[, timeout | timed_out]`: its
            // producer encoded the tagged object (9 bytes) and the trailer
            // (`post_jdwp_monitor_event`); the location goes between them.
            kind if kind.is_monitor() => {
                let split = evt.extra.len().min(9);
                let mut bytes = evt.extra[..split].to_vec();
                let mut location = build_location_extra(evt.class_id, evt.method_id, evt.offset);
                if evt.method_id != 0 {
                    location[0] = shared
                        .debug
                        .debug_state
                        .lock()
                        .location_type_tag(evt.class_id);
                }
                bytes.extend_from_slice(&location);
                bytes.extend_from_slice(&evt.extra[split..]);
                bytes
            }
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
        // Wave 49 (lane L1): a `SUSPEND_ALL` set goes out once the other
        // threads stopped (the event's own thread published before it sent).
        // Interpreter round i4 wave 94 (lane L1): an `EVENT_THREAD` set once
        // its thread stopped.
        await_event_set_settled(shared, set_policy, &set);
        if !send_composite(
            stream,
            shared,
            event_set_policy_byte(set_policy, set_wire),
            &set,
        ) {
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
    await_event_set_settled(shared, set_policy, &set);
    let sent = send_composite(
        stream,
        shared,
        event_set_policy_byte(set_policy, set_wire),
        &set,
    );
    if sent {
        note_vm_death_sent(shared, &set);
    }
    sent
}

/// Wait, bounded, for the threads an event set about to go out suspended
/// ([`await_suspension_settled`]): every thread for a `SUSPEND_ALL` set (wave
/// 49), and (interpreter round i4 wave 94, lane L1;
/// `DebuggerGates::suspend_event_settle`) the event threads of a
/// `SUSPEND_EVENT_THREAD` set. A breakpoint's, a step's or an exception's
/// thread published its frames before it sent, so that wait answers at once;
/// a `ClassPrepare` reported on its preparing thread or a `ThreadStart` is
/// sent while its thread is still on its way to its next suspend point, and
/// a debugger's `Frames` / `Status` right after the event raced it. HotSpot's
/// back end suspends the event thread before it writes the set. A thread
/// that is dying (`ThreadDeath`) or a set about the VM (`VMDeath`) reaches no
/// suspend point and is not waited for.
fn await_event_set_settled(
    shared: &crate::vm::SharedVm,
    policy: events::SuspendPolicy,
    set: &[events::Event],
) {
    match policy {
        events::SuspendPolicy::All => await_suspension_settled(shared, None),
        events::SuspendPolicy::EventThread if shared.debug.debugger_gates.suspend_event_settle() => {
            let mut threads: Vec<u64> = set
                .iter()
                .filter(|e| {
                    // Wave 104: the `DestroyJavaVM` thread is VM thread 0
                    // too, which no settle waits for.
                    e.thread_id != 0
                        && e.thread_id != ids::DESTROY_JAVA_VM_WIRE_ID
                        && !matches!(
                            e.kind,
                            events::EventKind::ThreadDeath | events::EventKind::VMDeath
                        )
                })
                .map(|e| e.thread_id)
                .collect();
            threads.sort_unstable();
            threads.dedup();
            for tid in threads {
                await_suspension_settled(shared, Some(tid));
            }
        }
        _ => {}
    }
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
    let mut suspended = false;
    {
        let mut ds = shared.debug.debug_state.lock();
        let thread_id = ds.thread_names.keys().copied().min().unwrap_or(0);
        // Wave 104 (lane L1): VM thread 0 goes out as the `DestroyJavaVM`
        // thread once `main` has ended (an identity id `thread_to_wire`
        // keeps).
        let thread_id = if thread_id == 0 && ds.destroy_java_vm_split.is_some() {
            ids::DESTROY_JAVA_VM_WIRE_ID
        } else {
            thread_id
        };
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
    // Interpreter round i4 wave 94 (lane L1, `DebuggerGates::suspend_event_settle`):
    // a set that suspended every thread goes out once they stopped, as a
    // `SUSPEND_ALL` set from the interpreter does (wave 49); it used to be
    // written while they still ran on to their suspend points. No lock held.
    if suspended && shared.debug.debugger_gates.suspend_event_settle() {
        await_suspension_settled(shared, None);
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
    let packets = class_unload_packets(shared, report);
    // Interpreter round i5 wave 101 (lane L1, under
    // `DebuggerGates::suspend_event_settle`): a `SUSPEND_ALL` `ClassUnload`
    // set goes out once the threads it suspended stopped, as the poll's
    // `ClassPrepare` set has since wave 94; it was written while they ran on
    // to their suspend points. HotSpot's back end suspends before it sends.
    let suspends_all = packets.iter().any(|pkt| {
        matches!(pkt, JdwpPacket::Command { data, .. }
            if data.first() == Some(&(events::SuspendPolicy::All as u8)))
    });
    if suspends_all && shared.debug.debugger_gates.suspend_event_settle() {
        await_suspension_settled(shared, None);
    }
    packets
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
                let class_id = crate::classloading::ClassId::new(raw);
                // A lambda proxy class (the i54-L1 retirement) is never in
                // the store: gone only once its call site is.
                cm.class_store.get(class_id).is_none()
                    && !shared.classes.is_lambda_proxy_class(class_id)
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
    let hide = ds.hide_unprepared_classes;
    for (class, since) in candidates {
        let stored = u32::try_from(class.0)
            .ok()
            .and_then(|raw| cm.class_store.get(crate::classloading::ClassId::new(raw)));
        // Interpreter round i5 wave 103 (lane L1): a class HotSpot would
        // not list ([`jdwp_class_unprepared`]: loaded and not linked, its
        // link not trivial) is never reported after the fact; it waits here
        // until it is prepared, as HotSpot posts `ClassPrepare` only then.
        let held = hide
            && stored.is_some_and(|c| {
                jdwp_class_unprepared(shared, &cm, c, ds.class_prepare_watermark)
            });
        let prepared = stored.is_none_or(|c| {
            !matches!(
                c.state,
                ClassState::Loading
                    | ClassState::Loaded
                    | ClassState::Verifying
                    | ClassState::Verified
                    | ClassState::Preparing
            )
        });
        if prepared || (!held && now.duration_since(since) >= CLASS_PREPARE_GRACE) {
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
    // Interpreter round i4 wave 93 (lane L1, `CRATONVM_JDWP_SCOPED_METHOD_EVENTS`):
    // a class a class-scoped method event request names joins its scope
    // before any of its code runs, on whatever thread prepares it. One load
    // with no such request.
    if shared.debug.debugger_gates.method_event_scope_armed() {
        join_method_event_scope(shared, class_id);
    }
    // Wave 95 (lane L1, `CRATONVM_JDWP_SCOPED_FIELD_WATCHES`): a class whose
    // constant pool names a watched field joins the field watch's scope
    // before any of its code runs. One load with no such watch.
    if shared.debug.debugger_gates.field_watch_scope.armed() {
        field_watch_scope::join(shared, class_id);
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

/// The internal name a lambda proxy class is matched by (`ClassMatch`,
/// `ClassExclude`): its host's name, `$$Lambda/0x` and the 16-digit id, which
/// a filter reads dotted as HotSpot's back end reads the hidden class's
/// signature (`Host$$Lambda.0x...`). `None` for any other class.
pub(crate) fn lambda_proxy_internal_name(
    shared: &crate::vm::SharedVm,
    proxy: crate::classloading::ClassId,
) -> Option<String> {
    let (_, host) = crate::runtime::interpreter::lambda_proxy_host(shared, proxy)?;
    Some(format!("{host}$$Lambda/0x{:016x}", proxy.as_u32()))
}

/// The wire ids a lambda proxy class answers a `ClassOnly` filter for
/// (the i54-L1 retirement): `java.lang.Object` and each of its interfaces,
/// with theirs.
pub(crate) fn lambda_proxy_supertypes(
    shared: &crate::vm::SharedVm,
    proxy: crate::classloading::ClassId,
) -> Vec<u64> {
    let interfaces = crate::runtime::lambda_proxy::lambda_proxy_facts(shared, proxy)
        .map(|facts| facts.interfaces)
        .unwrap_or_default();
    let cm = shared.classes.class_manager.read();
    let mut all: Vec<u64> = cm
        .resolve_fast_path_class_id("java/lang/Object")
        .map(|id| u64::from(id.as_u32()))
        .into_iter()
        .collect();
    let mut pending = interfaces;
    while let Some(at) = pending.pop() {
        let wire = u64::from(at.as_u32());
        if all.contains(&wire) {
            continue;
        }
        all.push(wire);
        if let Some(class) = cm.get_class(at) {
            pending.extend(class.interfaces.iter().copied());
        }
    }
    all
}

/// Lambda proxy class `proxy` of this VM was just minted and linked on VM
/// thread `thread_id` (`runtime::invokedynamic`, the i54-L1 retirement): HotSpot
/// spins and defines a hidden class there, and posts its `ClassPrepare`, then
/// its `ClassLoad`, to the C JVMTI envs that take them (MEASURED,
/// `tools/probes/interp/L1/L1K54JvmtiLambdaProxyEvents.java`; no
/// `ClassFileLoadHook`), and its JDWP `ClassPrepare` to the requests it
/// matches (`L1K54JdiLambdaProxyEvents`). The session's tables describe it
/// first ([`register_lambda_proxy_metadata`]). Behind three loads when
/// nothing listens.
pub(crate) fn lambda_proxy_minted(
    shared: &crate::vm::SharedVm,
    thread_id: u64,
    proxy: crate::classloading::ClassId,
) {
    let gates = &shared.debug.debugger_gates;
    if gates.native_class_prepare_armed() {
        crate::jvmti::native_env::post_class_prepare(shared, thread_id, proxy);
    }
    if gates.native_class_load_armed() {
        crate::jvmti::native_env::post_class_load(shared, thread_id, proxy);
    }
    if !gates.class_prepare_armed() || debugger_hooks_suppressed() {
        return;
    }
    register_lambda_proxy_metadata(shared);
    let Some(name) = lambda_proxy_internal_name(shared, proxy) else {
        return;
    };
    let Some(facts) = crate::runtime::lambda_proxy::lambda_proxy_facts(shared, proxy) else {
        return;
    };
    let supertypes = lambda_proxy_supertypes(shared, proxy);
    let queued = {
        let mut ds = shared.debug.debug_state.lock();
        let wire = u64::from(proxy.as_u32());
        if !ds.class_prepare_reported.insert(wire) {
            return;
        }
        let hits = ds
            .events
            .match_class_prepare_on(&name, &[], Some(thread_id), &|id| supertypes.contains(&id));
        let policy = strongest_suspend_policy(hits.iter().map(|&(_, p)| p));
        let suspended = ds.apply_event_set_policy(policy, thread_id);
        let mut queued = Vec::with_capacity(hits.len());
        for (request_id, suspend_policy) in hits {
            let mut extra = protocol::PayloadWriter::new();
            extra.put_u8(1); // CLASS
            extra.put_u64_be(ids::class_to_wire(wire));
            extra.put_string(&facts.signature(proxy));
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
/// [`DebuggerGates::native_hooks_armed`]'s bit for a C JVMTI env of this VM
/// listening for `NativeMethodBind` since `Agent_OnLoad` (interpreter round
/// i3 wave 83, lane L1): the native-call funnel posts the event the first
/// time it runs a registered (Rust) native, as HotSpot posts it when it links
/// one (`jvmti::native_env::post_rust_native_method_bind`). Not part of
/// [`DebuggerGates::method_events_armed`]: no method-event consumer acts on
/// it; only the funnel and its leaf twin read the whole word.
const METHOD_EVENTS_NATIVE_BIND: u8 = 32;

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
/// `docs/feature-designs/jvmti-delivery-threading.md`).
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
/// `DebuggerGates::running_native_record`: a debugger is attached.
const RUNNING_NATIVE_SESSION: u8 = 1;
/// `DebuggerGates::running_native_record`: a C JVMTI env is bound and
/// `CRATONVM_JVMTI_C_RUNNING_NATIVE` is on (wave 61, lane L6).
const RUNNING_NATIVE_C_ENV: u8 = 2;
/// `DebuggerGates::running_native_record`: a C JVMTI env is bound and
/// `CRATONVM_JVMTI_C_RUNNING_NATIVE` is on, but the funnel's own callback
/// record is not kept, because the raised native-excuse word already carries
/// the callback (`CRATONVM_JVMTI_C_FUNNEL_RECORD_FROM_EXCUSE`; interpreter
/// round i6 wave 133, lane L1,
/// `docs/feature-designs/jvmti-delivery-threading.md`).
/// Leaf natives still take the full funnel (it raises the word), but
/// `interpreter::enter_running_native` records nothing for this bit
/// ([`DebuggerGates::running_native_armed`] ignores it).
const RUNNING_NATIVE_C_LEAF_ONLY: u8 = 4;

/// The C JVMTI table's events posted from VM sites behind
/// [`DebuggerGates::native_event_armed`] (interpreter round i2 wave 63, lane
/// L1; `jvmti::native_env::events_more`). The value is the gate's bit.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u32)]
pub enum NativeEvent {
    /// `FieldAccess` / `FieldModification` of a watched field.
    FieldWatch = 1,
    /// `MonitorWait`, `MonitorWaited`, `MonitorContendedEnter`,
    /// `MonitorContendedEntered`.
    Monitor = 2,
    /// `VMObjectAlloc`.
    VmObjectAlloc = 4,
    /// `SampledObjectAlloc`.
    SampledAlloc = 8,
    /// `NativeMethodBind`.
    NativeBind = 16,
    /// `ResourceExhausted`.
    ResourceExhausted = 32,
    /// `VirtualThreadStart` / `End` and the mount extension events.
    VirtualThread = 64,
    /// The `ClassUnload` extension event.
    ClassUnload = 128,
    /// `CompiledMethodLoad` / `Unload`, `DynamicCodeGenerated`.
    CompiledCode = 256,
    /// `DataDumpRequest`.
    DataDump = 512,
    /// `GarbageCollectionStart` / `Finish`.
    Gc = 1024,
    /// `ObjectFree`.
    ObjectFree = 2048,
}

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
    /// This VM is configured with a JDWP server (`vm_init`, as it spawns
    /// `run_jdwp_server`; the i54-L1 retirement): a debugger may attach at
    /// any time, and a lambda call it
    /// finds in progress must have recorded its proxy method's frame
    /// ([`Self::c_lambda_frames_armed`]).
    jdwp_listening: std::sync::atomic::AtomicBool,
    /// A C JVMTI env bound after lambda proxies existed (the i54-L1
    /// retirement): a lambda call in progress then recorded no proxy frame,
    /// and the C table's listing infers it (`jvmti::native_env::with_inferred_proxy_rows`).
    proxy_rows_incomplete: std::sync::atomic::AtomicBool,
    /// A JDWP suspension is in force (wave 12; `DebugState::any_suspension`):
    /// a thread entering a blocking native region checks its own suspend
    /// count only while this is up (`interpreter::publish_blocked_frames`).
    suspension: std::sync::atomic::AtomicBool,
    /// Any suspension is counted, a thread listed ZOMBIE's own included
    /// (`DebugState::any_suspension`; interpreter round i5 wave 106, lane L1):
    /// the gate of the hold whose thread may be listed ended
    /// ([`hold_native_thread_while_suspended`]; its slow path then asks that
    /// thread's own count).
    /// [`Self::suspension`] leaves such a suspension out on purpose (wave 29:
    /// it must not stop every live thread), so a hold of an ended thread
    /// gated on it returned at once (lane L6, wave 105). Published with it.
    suspension_counted: std::sync::atomic::AtomicBool,
    /// A C JVMTI env of this VM has `ClassPrepare` enabled (interpreter
    /// round i1 wave 13): [`class_prepared_on_thread`] delivers it to the
    /// agent's callback (`jvmti::native_env::post_class_prepare`). Set by
    /// `native_env`, never by [`publish_debugger_gates`].
    native_class_prepare: std::sync::atomic::AtomicBool,
    /// A C JVMTI env of this VM has `ThreadStart` or `ThreadEnd` enabled
    /// (interpreter round i1 wave 52, lane L1): a platform thread posts them
    /// before its `run()` and after it (`jvmti::native_env::post_thread_event`).
    /// One load per thread start and end.
    native_thread_events: std::sync::atomic::AtomicBool,
    /// `main` has ended and the launcher's registry entry (VM thread 0) is
    /// the `DestroyJavaVM` thread (interpreter round i5 wave 104, lane L1;
    /// [`DebugState::destroy_java_vm_split`], whose copy this is for the
    /// paths that hold no debug-state lock: [`send_event_set`] sends an event
    /// of thread 0 as `ids::DESTROY_JAVA_VM_WIRE_ID`). Never cleared.
    destroy_java_vm_split: std::sync::atomic::AtomicBool,
    /// The events of the C JVMTI table's wave-63 set a C env of this VM has
    /// enabled, by [`NativeEvent`] bit (interpreter round i2 wave 63, lane
    /// L1; `jvmti::native_env::events_more::refresh_event_gates`): each
    /// posting site asks one relaxed load before anything else.
    native_events: std::sync::atomic::AtomicU32,
    /// The [`NativeEvent`] bits the JDWP server's requests arm (interpreter
    /// round i5 wave 103, lane L1): `Monitor` while a monitor event request
    /// (43-46) is in force, so the monitor paths that post the C JVMTI events
    /// reach `debug::post_jdwp_monitor_event` too. Published by
    /// [`publish_debugger_gates`]; [`Self::native_event_armed`] answers the
    /// union.
    jdwp_native_events: std::sync::atomic::AtomicU32,
    /// A C JVMTI env of this VM has `ClassLoad` enabled (interpreter round
    /// i2 wave 53, lane L1): a class's definition posts it on the defining
    /// thread (`jvmti::native_env::post_class_load`). One load per class
    /// defined.
    native_class_load: std::sync::atomic::AtomicBool,
    /// A C JVMTI env of this VM has `ClassFileLoadHook` enabled and the VM is
    /// in the live phase (wave 53, lane L1): the load path offers each class
    /// file to the agent's callback before the class is defined, inside the
    /// transformer chain `java.lang.instrument` uses
    /// (`runtime::instrument::load_hook_armed`,
    /// `jvmti::native_env::post_class_file_load_hook`).
    native_class_file_load_hook: std::sync::atomic::AtomicBool,
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
    /// This VM's JVMTI phase as its C JVMTI envs see it, one `jvmtiPhase`
    /// value ([`JVMTI_PHASE_PRIMORDIAL`], [`JVMTI_PHASE_START`],
    /// [`JVMTI_PHASE_LIVE`], [`JVMTI_PHASE_DEAD`]; interpreter round i2 wave
    /// 55, lane L1, which replaced wave 14's start-phase flag). Live from
    /// construction: a VM without a startup agent is never anything else
    /// before its death. `Vm::new` binding startup agents' envs moves it to
    /// primordial (with [`Self::c_jvmti_phases`]; start without), init level
    /// 2 (after `System.initPhase1`) to start, posting `VMStart`
    /// (`jvmti::native_env::enter_start_phase`), level 3 to live, posting
    /// `VMInit` (`jvmti::native_env::enter_live_phase`), and the VM's drop to
    /// dead after the `VMDeath` callbacks (with the switch). Written by
    /// `native_env` only.
    jvmti_phase: std::sync::atomic::AtomicU8,
    /// `CRATONVM_JVMTI_C_PHASES`, read once when this VM's gates are made
    /// ([`c_jvmti_phases_switch`]; interpreter round i2 wave 55, lane L1;
    /// `docs/feature-designs/jvmti-delivery-threading.md`):
    /// the C JVMTI table keeps HotSpot's primordial phase until
    /// `System.initPhase1` returns (no `ClassLoad`, `ClassPrepare` or
    /// `ClassFileLoadHook` then, `GetPhase` 2), delivers `VMStart` when the
    /// start phase begins, and answers the dead phase after `VMDeath`. Off
    /// (`=0`): the start phase from `Vm::new` on and `VMStart` not
    /// available, as before. On by default (wave 55's last commit). Plain
    /// data: never written after construction.
    c_jvmti_phases: bool,
    /// `CRATONVM_JVMTI_C_LAMBDA_CLASSES`, read once when this VM's gates are
    /// made (interpreter round i2 wave 55, lane L1;
    /// `docs/feature-designs/jvmti-delivery-threading.md`):
    /// the C JVMTI table lists the lambda proxy classes the VM mints outside
    /// the class store in `GetLoadedClasses` and answers their signature,
    /// modifiers, status, loader, interfaces and version
    /// (`jvmti::native_env::JvmtiClass::LambdaProxy`). Off (`=0`): it refuses them
    /// with `JVMTI_ERROR_INVALID_CLASS`, as before. On by default (wave 55's
    /// last commit). Plain data: never written after construction.
    c_lambda_classes: bool,
    /// `CRATONVM_JVMTI_C_LAMBDA_METHODS`, read once when this VM's gates are
    /// made (interpreter round i2 wave 56, lane L1;
    /// `docs/feature-designs/jvmti-delivery-threading.md`):
    /// the C JVMTI table's `GetClassMethods` lists a lambda proxy class's
    /// methods, and the method functions answer them, from the one record of
    /// its facts (`runtime::lambda_proxy::lambda_proxy_facts`). Off: they
    /// answer `JVMTI_ERROR_INVALID_CLASS` / `JVMTI_ERROR_INVALID_METHODID`,
    /// as before. On by default (wave 56's last commit). Plain data: never
    /// written after construction.
    c_lambda_methods: bool,
    /// `CRATONVM_JVMTI_C_LAMBDA_CODE`, read once when this VM's gates are
    /// made (interpreter round i2 wave 57, lane L1;
    /// `docs/feature-designs/jvmti-delivery-threading.md`):
    /// the C JVMTI table's `GetMethodLocation` and `GetBytecodes` answer a
    /// lambda proxy's method with the code `InnerClassLambdaMetafactory`
    /// spins (`runtime::lambda_proxy::lambda_proxy_method_code`). Off: they
    /// answer `JVMTI_ERROR_ABSENT_INFORMATION`, as before. Only with
    /// [`Self::c_lambda_methods`]. On by default (wave 57's last commit).
    /// Plain data: never written after construction.
    c_lambda_code: bool,
    /// `CRATONVM_JVMTI_C_LAMBDA_FRAMES`, read once when this VM's gates are
    /// made (interpreter round i2 wave 60, lane L1;
    /// `docs/feature-designs/jvmti-delivery-threading.md`):
    /// while a C JVMTI env is bound, the interpreter's lambda dispatch
    /// records the lambda proxy method a call enters
    /// (`jvmti::native_env::NativeFrameRow::enter_lambda_proxy`), and the C
    /// table's stack functions list that method's frame between the body and
    /// its caller, as HotSpot lists the spun class's frame. Off: no frame, as
    /// before. Only with [`Self::c_lambda_methods`]. Plain data: never
    /// written after construction.
    c_lambda_frames: bool,
    /// `CRATONVM_JVMTI_C_LAMBDA_EVENTS`, read once when this VM's gates are
    /// made (interpreter round i2 wave 60, lane L1;
    /// `docs/feature-designs/jvmti-delivery-threading.md`):
    /// with [`Self::c_lambda_frames`] armed, the C JVMTI envs listening for
    /// `MethodEntry` / `MethodExit` get them for the lambda proxy method a
    /// call enters (`interpreter::lambda::dispatch_with_proxy_frame`), as
    /// HotSpot posts them for the spun class's method. Off: none, as before.
    /// Plain data: never written after construction.
    c_lambda_events: bool,
    /// `CRATONVM_JNI_LAMBDA_METHOD_IDS`, read once when this VM's gates are
    /// made (interpreter round i2 wave 60, lane L1; stage 2 of
    /// `docs/feature-designs/jvmti-delivery-threading.md`):
    /// while a C JVMTI env is bound, JNI's `GetMethodID` of a lambda's class
    /// answers a method the class declares with the proxy's own `jmethodID`
    /// (the one the C table lists it by), as HotSpot answers the hidden
    /// class's method. Off: the functional interface's method, as before.
    /// Only with [`Self::c_lambda_methods`]. Plain data: never written after
    /// construction.
    jni_lambda_method_ids: bool,
    /// `CRATONVM_JVMTI_C_PRIMITIVE_CLASSES`, read once when this VM's gates
    /// are made (interpreter round i2 wave 56, lane L1;
    /// `docs/feature-designs/jvmti-delivery-threading.md`):
    /// the C JVMTI table's class functions answer a primitive class's mirror
    /// (`int.class`, which has no `ClassId`) with HotSpot's facts
    /// (`jvmti::native_env::JvmtiClass::Primitive`). Off: they refuse it with
    /// `JVMTI_ERROR_INVALID_CLASS`, as before. On by default (wave 56's last
    /// commit). Plain data: never written after construction.
    c_primitive_classes: bool,
    /// A JDWP `MethodEntry` request is in force in this VM (interpreter
    /// round i1 wave 24). Kept for its edges, which move the process-wide
    /// pre-filter the frame pushes read ([`method_entry_requests_anywhere`]);
    /// since wave 95 (lane L1) a VM dropped with a request in force takes
    /// its unit back ([`RequestVmsEdge`]).
    method_entry: RequestVmsEdge,
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
    /// `-agentlib:jdwp`'s start deferred to a matching throw (interpreter
    /// round i7 wave 144, lane L1; [`Self::jdwp_deferred_start_armed`]).
    jdwp_deferred_start: std::sync::atomic::AtomicBool,
    /// A `StackFrame.SetValues` sent while its thread was blocked in a native
    /// waits to be applied by that thread (interpreter round i1 wave 41,
    /// lane L1; [`DebugState::deferred_frame_writes`]): the blocking region's
    /// exit asks `inspect::apply_deferred_frame_writes_if_any` only while
    /// this is up. Written under the debug-state lock only.
    frame_writes: std::sync::atomic::AtomicBool,
    /// A `StackFrame.SetValues` to a compiled activation waits for that
    /// activation's post-call exit (interpreter round i6 wave 123, lane L2;
    /// [`DebugState::compiled_home_writes`]): the deopt resume asks
    /// `inspect::apply_compiled_home_writes` only while this is up. Written
    /// under the debug-state lock only.
    compiled_home_writes: std::sync::atomic::AtomicBool,
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
    /// `CRATONVM_JDWP_SCOPED_METHOD_EVENTS`, read once when this VM's gates
    /// are made (interpreter round i4 wave 93, lane L1; default OFF; page
    /// `docs/feature-designs/jvmti-delivery-threading.md`):
    /// a JDWP `MethodEntry` / `MethodExit` request confined to a set of
    /// classes by a `ClassOnly` or exact `ClassMatch` modifier
    /// (`events::method_event_scope_of`) keeps only those classes' methods
    /// interpreted ([`Self::in_method_event_scope`]) and withdraws only their
    /// dependents' compiled code, instead of every method of the VM. Plain
    /// data: never written after construction.
    scoped_method_events: bool,
    /// `CRATONVM_JVMTI_EXCEPTION_FROM_COMPILED` for JDWP, read once when this
    /// VM's gates are made (interpreter round i4 wave 93, lane L1; default
    /// OFF; page
    /// `docs/feature-designs/jvmti-delivery-threading.md`):
    /// an `Exception` request compiled code can serve
    /// (`events::EventManager::exception_requests_need_the_interpreter`
    /// false) no longer keeps every method interpreted; the compiled catch
    /// doors, the compiled `athrow`'s site and the catch prediction over
    /// compiled activations report it, as for JVMTI. Plain data: never
    /// written after construction.
    exceptions_from_compiled: bool,
    /// An `Exception` request is in force that this VM serves from compiled
    /// code (wave 93, [`Self::exceptions_from_compiled`]). Kept for its
    /// edges, which move the process-wide pre-filter the compiled `athrow`
    /// helper reads ([`compiled_exception_requests_anywhere`]); since wave 95
    /// (lane L1) a VM dropped with such a request in force takes its unit
    /// back ([`RequestVmsEdge`]).
    compiled_exception_requests: RequestVmsEdge,
    /// The classes a JDWP field watch keeps interpreted under
    /// `CRATONVM_JDWP_SCOPED_FIELD_WATCHES` (interpreter round i4 wave 95,
    /// lane L1; default ON since the wave-129 triage, `=0` off;
    /// [`field_watch_scope`]).
    field_watch_scope: field_watch_scope::FieldWatchScope,
    /// Bit `class_id % 64` of every class in [`Self::method_event_classes`]
    /// (wave 93): the one load [`Self::in_method_event_scope`] answers `false`
    /// with for every other class.
    method_event_class_bits: std::sync::atomic::AtomicU64,
    /// The classes a class-scoped method event request in force concerns
    /// (wave 93; [`refresh_method_event_scope`], [`join_method_event_scope`]).
    method_event_classes: parking_lot::RwLock<HashSet<u32>>,
    /// The scopes [`Self::method_event_classes`] was resolved for (wave 93).
    /// [`publish_debugger_gates`] treats a scoped request whose scope is not
    /// here as concerning every method. Written only by
    /// [`refresh_method_event_scope`]; held for a clone or a compare, never
    /// across another lock.
    method_event_specs: parking_lot::RwLock<Vec<events::MethodEventScope>>,
    /// The class store's slot count [`refresh_method_event_scope`] last
    /// resolved through (wave 93): the classes defined since are resolved by
    /// the next refresh or, when prepared, by [`join_method_event_scope`].
    method_event_slots_seen: std::sync::atomic::AtomicUsize,
    /// A class a scope names cannot be kept interpreted alone (wave 93): a
    /// JDK class (the JIT expands some of their methods at a call site
    /// without a record), an interface or a class not in the store named by
    /// `ClassOnly`, or a class with a native method (a compiled caller calls
    /// a native without the funnel that reports its events). The scoped
    /// requests then concern every method, as before wave 93.
    method_event_degraded: std::sync::atomic::AtomicBool,
    /// The [`Self::method_event_degraded`] the last [`publish_debugger_gates`]
    /// read (interpreter round i4 wave 94, lane L1): a join that degraded the
    /// scope on a thread that could not publish (`debugger_hooks_suppressed`)
    /// leaves the two apart, and the server's next poll
    /// ([`refresh_method_event_scope`]) publishes. Without it the poll saw
    /// the flag already set and published nothing, and the degraded scope
    /// stayed unpublished until the next command.
    method_event_degraded_published: std::sync::atomic::AtomicBool,
    /// A scope is resolved (wave 93): a class prepared now is matched against
    /// it ([`join_method_event_scope`]). One load per class prepared.
    method_event_scope_armed: std::sync::atomic::AtomicBool,
    /// Bumped whenever [`Self::method_event_classes`] grows (wave 93).
    method_event_scope_generation: std::sync::atomic::AtomicU64,
    /// The [`Self::method_event_scope_generation`] the last
    /// [`publish_debugger_gates`] saw: a change sends running compiled loops
    /// to their exit polls, as a grown breakpoint set does.
    method_event_scope_published: std::sync::atomic::AtomicU64,
    /// `CRATONVM_DEBUG_ONE_FRAME_LISTING`, read once when this VM's gates
    /// are made (interpreter round i1 wave 47, lane L1; stage 1 of
    /// `docs/feature-designs/jvmti-delivery-threading.md`):
    /// the JDWP listing and the C JVMTI table's listing of a blocked thread
    /// also list the JNI natives in the middle of a thread's stack and its
    /// reflective calls' JDK frames (`interpreter::build_frame_listing`), as
    /// the current thread's JVMTI listing does. Plain data: never written
    /// after construction.
    one_frame_listing: bool,
    /// `CRATONVM_DEBUG_COMPATIBLE_BLOCKING_BYTECODE`, read once when this
    /// VM's gates are made (interpreter round i1 wave 48, lane L1; stage 1 of
    /// `docs/feature-designs/jvmti-delivery-threading.md`):
    /// while a debugger is attached ([`METHOD_EVENTS_BLOCKING`]), a
    /// `--compatible` call of a [`BLOCKING_STANDINS`] method runs its JDK
    /// bytecode as `--jdk-only` does (`jvmti_events::run_blocking_standin`),
    /// so the thread blocks under real `sleep` / `sleepNanos` / `wait`
    /// frames. Plain data: never written after construction.
    compatible_blocking_bytecode: bool,
    /// `CRATONVM_DEBUG_VIRTUAL_CONTINUATION_FRAMES`, read once when this
    /// VM's gates are made (interpreter round i1 wave 49, lane L1;
    /// `docs/feature-designs/jvmti-delivery-threading.md`):
    /// a suspended unmounted virtual thread blocked in a sleep or a park
    /// lists HotSpot's default continuation rows (`VirtualThread.yieldContinuation`,
    /// `VirtualThread.parkNanos`, ...) in place of the `BoundVirtualThread`
    /// platform branch that ran (`interpreter::continuation_head_of_unmounted`).
    /// Plain data: never written after construction.
    virtual_continuation_frames: bool,
    /// `CRATONVM_DEBUG_SUSPEND_HANDSHAKE`, read once when this VM's gates
    /// are made (interpreter round i1 wave 49, lane L1; stage 1 of
    /// `docs/feature-designs/jvmti-delivery-threading.md`):
    /// `VirtualMachine.Suspend`, `ThreadReference.Suspend` and a
    /// `SUSPEND_ALL` event set wait, bounded, for each suspended thread that
    /// was running Java to reach a safe place before the reply or the event
    /// goes out ([`await_suspension_settled`]), as HotSpot's handshake
    /// completes before its reply. Plain data: never written after
    /// construction.
    suspend_handshake: bool,
    /// `CRATONVM_DEBUG_SUSPEND_EVENT_SETTLE`, read once when this VM's gates
    /// are made (interpreter round i4 wave 94, lane L1; the stage-2 remainder
    /// of `docs/feature-designs/jvmti-delivery-threading.md`;
    /// default ON, `=0` off). The two event suspensions the wave-49 wait did
    /// not cover wait too ([`await_event_set_settled`]): a
    /// `SUSPEND_EVENT_THREAD` set waits for its event thread (a `ClassPrepare`
    /// or `ThreadStart` thread counted suspended parks at its next suspend
    /// point, after the set went out), and the server poll's `ClassPrepare`
    /// set that suspends every thread waits for them before it is written
    /// (`send_class_prepare_events`). HotSpot's back end suspends before it
    /// sends either. Takes effect only with [`Self::suspend_handshake`].
    /// Plain data: never written after construction.
    suspend_event_settle: bool,
    /// `CRATONVM_DEBUG_INVOKE_REPLY_SETTLE`, read once when this VM's gates
    /// are made (interpreter round i5 wave 101, lane L1; the last suspension
    /// of `docs/feature-designs/jvmti-delivery-threading.md`
    /// that did not wait; default ON, `=0` off). The reply to a
    /// `ClassType.InvokeMethod` / `ObjectReference.InvokeMethod` /
    /// `ClassType.NewInstance` waits for the threads the invocation's closing
    /// re-suspension covered ([`await_suspension_settled`]): without
    /// `INVOKE_SINGLE_THREADED` every thread ran during the call and is only
    /// counted suspended again (`DebugState::resuspend_after_invocation`).
    /// HotSpot's `invoker_completeInvokeRequest` suspends them
    /// (`threadControl_suspendAll`, a JVMTI `SuspendThreadList`, which
    /// returns once they are stopped) before `outStream_sendReply`. Takes
    /// effect only with [`Self::suspend_handshake`]. Plain data: never
    /// written after construction.
    invoke_reply_settle: bool,
    /// `CRATONVM_JVMTI_STATE_SUSPENSION_HINT`, read once when this VM's gates
    /// are made (interpreter round i3 wave 72, lane L6; page
    /// `interpreter-i64-L1-proposal-suspended-read-from-a-word-every-suspension-writes-KEPT-OFF-20261021`):
    /// C JVMTI `GetThreadState` reads the VM's suspension hint
    /// (`DebugRealm::suspension_hint`) first and takes the debug-state lock
    /// for the `SUSPENDED` bit only while some thread is suspended. Default
    /// OFF (`truthy_word`). Plain data: never written after construction.
    state_suspension_hint: bool,
    /// `CRATONVM_JVMTI_THREAD_LOOKUP_VM_ONLY`, read once when this VM's gates
    /// are made (interpreter round i6 wave 130, lane L1b; page
    /// `docs/feature-designs/jvmti-delivery-threading.md`):
    /// the C JVMTI functions' `jthread` lookup (`native_env::lookup_thread`,
    /// so `GetThreadState` and every `thread_id_of` caller) leaves native
    /// through `ForeignJniEntry::enter_vm_only` (a quiet leave and an
    /// incremental re-entry) instead of `enter` (a full root deposit on the
    /// way back into native). Performance switch, default ON since
    /// interpreter round i7 (i7-default-flip, A/B w144flip; `=0` off). Plain
    /// data: never written after construction.
    jvmti_thread_lookup_vm_only: bool,
    /// `CRATONVM_JVMTI_THREAD_STATE_ONE_READ`, read once when this VM's gates
    /// are made (interpreter round i6 wave 132, lane L1; i130-L1b's first
    /// "next step"): the C JVMTI `GetThreadState` reads the thread's block
    /// state, published wait kind and interrupted flag from one registry
    /// `threads` read (`ThreadRegistry::jvmti_state_facts`) instead of three,
    /// without cloning the flag's `Arc`. Performance switch, default ON since
    /// interpreter round i7 (i7-default-flip, A/B w144flip; `=0` off). Plain
    /// data: never written after construction.
    jvmti_thread_state_one_read: bool,
    /// `CRATONVM_JVMTI_STATE_SUSPEND_WORD`, read once when this VM's gates are
    /// made (interpreter round i7 wave 135, lane L1; i64-L1 item 1, the
    /// i130-L1b "per-thread suspended word" step): the C JVMTI
    /// `GetThreadState` reads the `SUSPENDED` bit from the thread's own
    /// `GcBlockState::jvmti_state_suspended`, which every suspension mutator
    /// rewrites under the debug-state lock once a first ask subscribed the
    /// thread (`DebugState::subscribe_state_suspend_word`), instead of taking
    /// that VM-global lock per call. Performance switch, default OFF
    /// (`truthy_word`). Plain data: never written after construction.
    jvmti_state_suspend_word: bool,
    /// `CRATONVM_DEBUG_SUSPEND_NATIVE_ACK`, read once when this VM's gates
    /// are made (interpreter round i2 wave 53, lane L6; stage 2 of
    /// `docs/feature-designs/jvmti-delivery-threading.md`):
    /// while a debugger is attached, a thread running a native through the
    /// native-call funnel outside a blocking region, with no Java above it,
    /// says so in `GcBlockState::debugger_native_ack`
    /// (`interpreter::enter_running_native`), and the suspension's settle wait
    /// counts it stopped at once, as HotSpot's handshake counts a thread in
    /// native ([`await_suspension_settled`]); such a thread then parks at the
    /// native's return even when the native cannot be named
    /// (`interpreter::park_if_suspended_at_native_exit`). Off: it is waited
    /// for, a bounded while, and then a laggard, as in wave 49. Plain data:
    /// never written after construction.
    suspend_native_ack: bool,
    /// `CRATONVM_DEBUG_MONITOR_EXIT_ON_SUSPEND`, read once when this VM's
    /// gates are made (interpreter round i2 wave 54, lane L6;
    /// `docs/feature-designs/jvmti-delivery-threading.md`):
    /// a thread that wins a contended monitor while the debugger (or a C
    /// JVMTI agent) holds it suspended gives the monitor back and waits for
    /// its resume, still blocked on the entry, as HotSpot's `ExitOnSuspend`
    /// does ([`exit_monitor_while_suspended`]). Off (`=0`): it keeps the
    /// monitor and parks holding it, as before. Plain data: never written
    /// after construction.
    monitor_exit_on_suspend: bool,
    /// `CRATONVM_DEBUG_NATIVE_FRAMES_WINDOW`, read once when this VM's gates
    /// are made (interpreter round i2 wave 56, lane L6;
    /// `docs/feature-designs/jvmti-delivery-threading.md`):
    /// a thread that acknowledges a suspension in a native
    /// ([`Self::suspend_native_ack`]) opens a frames window as it enters the
    /// native (`interpreter::enter_running_native`), and a frame command on
    /// it reads the frames through it while the native runs
    /// (`interpreter::publish_frames_of_native_thread`), as HotSpot lists a
    /// thread in native at once. Off (`=0`): such a thread lists no frame
    /// until the native returns. On by default since wave 57 (lane L6), which
    /// made it serve (a moving-collection generation, the compiled view).
    /// Plain data: never written after construction.
    native_frames_window: bool,
    /// `CRATONVM_DEBUG_NATIVE_WINDOW_CHAIN`, read once when this VM's gates
    /// are made (interpreter round i2 wave 58, lane L6;
    /// `docs/feature-designs/jvmti-delivery-threading.md`):
    /// a native frames window opened under compiled code records a copy of
    /// the thread's JIT entry chain (`conservative_roots::JitChainSnapshot`)
    /// and the reader walks the compiled activations from it, instead of the
    /// thread capturing the whole compiled view as it enters every native.
    /// On by default since wave 58 (lane L6); `=0` restores the capture.
    /// Plain data: never written after construction.
    native_window_chain: bool,
    /// `CRATONVM_DEBUG_SUSPEND_SETTLE_LONG`, read once when this VM's gates
    /// are made (interpreter round i2 wave 56, lane L6;
    /// `docs/feature-designs/jvmti-delivery-threading.md`):
    /// the suspension's settle wait ([`await_suspension_settled`]) waits up
    /// to [`SUSPEND_SETTLE_LONG_WAIT`] for a running thread on its way to
    /// its suspend point, not [`SUSPEND_POINT_WAIT`]. Plain data: never
    /// written after construction.
    suspend_settle_long: bool,
    /// `CRATONVM_DEBUG_PARK_CONDVAR`, read once when this VM's gates are made
    /// (interpreter round i2 wave 54, lane L6;
    /// `docs/feature-designs/jvmti-delivery-threading.md`):
    /// the suspension machinery's waits -- a parked thread's
    /// (`interpreter::park_for_debugger_under`), a GC-blocked thread's
    /// ([`hold_native_thread_while_suspended`], [`exit_monitor_while_suspended`])
    /// and the JDWP server's settle and frame waits -- wait on
    /// [`Self::state_changed`], which every publication of these gates and
    /// every arrival at a suspend point notify, instead of sleeping 1 ms per
    /// round. On by default (wave 54's last commit); off (`=0`): the 1 ms
    /// poll. Plain data: never written after construction.
    park_condvar: bool,
    /// Notified (`notify_all`) by every [`publish_debugger_gates`] -- which
    /// follows every JDWP command, every C JVMTI suspension or resume and a
    /// detach -- by every task queued for a parked thread and by every
    /// arrival at a suspend point (interpreter round i2 wave 54, lane L6;
    /// [`Self::park_condvar`]). Waited on with the debug-state mutex.
    state_changed: parking_lot::Condvar,
    /// `CRATONVM_DEBUG_THREAD_SUSPEND_WORD`, read once when this VM's gates
    /// are made (interpreter round i2 wave 63, lane L6; stage 1 of
    /// `docs/feature-designs/jvmti-delivery-threading.md`):
    /// a thread parked at its suspend point
    /// (`interpreter::park_for_debugger_under`) or held GC-blocked by a
    /// suspension ([`wait_while_suspended`]) waits on its own suspend word
    /// (`GcBlockState::debugger_suspend_word`, [`SuspendWordWait`]) and its
    /// own parker, and takes the debug-state lock only when the word moved
    /// (a suspend count, a JVMTI suspension or its task queue changed) or
    /// every [`SUSPEND_WORD_BACKSTOP`] -- not once per poll as the 1 ms poll
    /// and the condvar do. Default ON since the wave-64 host run (`L6W63JdiSuspendAll...`:
    /// a status question 311 -> 183 ns, VM suspend+resume over 64 spinners 84 -> 29 ms, over
    /// 256 spinners 806 -> 178 ms; JDI transcripts HotSpot's); `=0`: the lock questions, as before.
    /// Plain data: never written after construction.
    thread_suspend_word: bool,
    /// `CRATONVM_DEBUG_STW_UNPARKS_PARKED`, read once when this VM's gates
    /// are made (interpreter round i2 wave 64, lane L6; step 2 of
    /// `docs/feature-designs/jvmti-delivery-threading.md`):
    /// with [`Self::thread_suspend_word`], a thread parked at its suspend
    /// point enrolls its OS thread with the GC barrier
    /// (`GcBarrier::enroll_stw_waker`), whose stop-the-world request unparks
    /// it, so its wait on the word is bounded by [`SUSPEND_WORD_BACKSTOP`]
    /// instead of the 1 ms safepoint poll ([`Self::parked_word_wait_bound`]).
    /// Off (default): the 1 ms poll, as in wave 63. Plain data: never written
    /// after construction.
    stw_unparks_parked: bool,
    /// `CRATONVM_DEBUG_PARK_GC_BLOCKED`, read once when this VM's gates are
    /// made (interpreter round i2 wave 65, lane L6;
    /// `docs/feature-designs/jvmti-delivery-threading.md`):
    /// with [`Self::thread_suspend_word`], a thread parked at its suspend
    /// point (`interpreter::park_for_debugger_under`) waits on its word
    /// inside a GC-blocked region ([`ParkGcRegion`]: roots deposited,
    /// `in_blocked_region` up, `Thread.getState()` RUNNABLE), so a
    /// stop-the-world pause neither wakes it nor waits for it -- HotSpot's
    /// suspended thread is safe for a safepoint the same way. It leaves the
    /// region before anything that touches its frames or the heap (a task, a
    /// relisting, its resume). Supersedes [`Self::stw_unparks_parked`] for
    /// those parks. Off (default): the park answers every pause, as in wave
    /// 64. Plain data: never written after construction.
    park_gc_blocked: bool,
    /// `CRATONVM_DEBUG_HEAP_WORK_OUTSIDE_REGION`, read once when this VM's
    /// gates are made (interpreter round i2 wave 66, lane L6;
    /// `docs/feature-designs/jvmti-delivery-threading.md`):
    /// with [`Self::park_gc_blocked`], heap work (a [`run_on_parked_thread`]
    /// with no thread named) goes to an idle parked thread whose GC-blocked
    /// region is closed, else to the heap service, and only then to a park
    /// inside its region ([`DebugState::heap_work_target`]); and a park
    /// whose word moved since its last locked round takes that round before
    /// it opens its region again. Off (default): the lowest-numbered idle
    /// parked thread, as in wave 65. Plain data: never written after
    /// construction.
    heap_work_outside_region: bool,
    /// `CRATONVM_DEBUG_PARK_REGION_REUSES_DEPOSIT`, read once when this VM's
    /// gates are made (interpreter round i2 wave 66, lane L6;
    /// `docs/feature-designs/jvmti-delivery-threading.md`):
    /// with [`Self::park_gc_blocked`], a park re-entering its region after
    /// heap work or a relisting raises the flag over the deposit its last
    /// leave refreshed, when nothing it holds can have moved
    /// ([`ParkGcRegion`]'s standing key), instead of rebuilding it. Off
    /// (default): every entry deposits in full, as in wave 65. Plain data:
    /// never written after construction.
    park_region_reuses_deposit: bool,
    /// `CRATONVM_DEBUG_PARK_REGION_AT_FIRST_PAUSE`, read once when this VM's
    /// gates are made (interpreter round i2 wave 68, lane L6;
    /// `docs/feature-designs/jvmti-delivery-threading.md`):
    /// with [`Self::park_gc_blocked`], a park at a suspend point waits
    /// outside its region, enrolled with the GC barrier
    /// (`GcBarrier::enroll_stw_waker`), and opens its region
    /// ([`ParkGcRegion::enter_at_pause`]) only when a stop-the-world request
    /// first wakes it -- arriving for that pause through the entry -- then
    /// waits GC-blocked for the rest of the hold ([`ParkRegionOpening`]). A
    /// hold that sees no pause deposits nothing. Off (default): the region
    /// opens after the first locked round, as in wave 65. Plain data: never
    /// written after construction.
    park_region_at_first_pause: bool,
    /// `CRATONVM_DEBUG_PARK_LEAVE_KEEPS_UNFOLDED_SNAPSHOT`, read once when
    /// this VM's gates are made (interpreter round i2 wave 68, lane L6;
    /// `docs/feature-designs/jvmti-delivery-threading.md`):
    /// with [`Self::park_region_reuses_deposit`], a park region left to run
    /// heap work ([`ParkGcRegion::leave_for_heap_work`]) keeps the root
    /// snapshot its entry deposited when no collection, fold or
    /// redefinition reached the region, instead of refreshing it from the
    /// frames and auditing them. Off (default): every leave refreshes, as in
    /// wave 66. Plain data: never written after construction.
    park_leave_keeps_unfolded_snapshot: bool,
    /// `CRATONVM_DEBUG_SETTLE_ARRIVAL_COUNT`, read once when this VM's gates
    /// are made (interpreter round i2 wave 64, lane L6; stage 1 of
    /// `docs/feature-designs/jvmti-delivery-threading.md`):
    /// [`await_suspension_settled`] walks the debug state under its lock only
    /// when [`Self::arrivals`] moved since its last walk (or
    /// [`SETTLE_REWALK_BACKSTOP`] passed), and then only the threads its last
    /// walk found still on their way -- not every thread the suspension
    /// covered, once per round. Off (default): every round walks every
    /// covered thread, as before. Plain data: never written after
    /// construction.
    settle_arrival_count: bool,
    /// `CRATONVM_DEBUG_SETTLE_PENDING_COUNTER`, read once when this VM's gates
    /// are made (interpreter round i6 wave 117, lane L1; performance, default
    /// ON since the wave-129 triage, `=0` off; part 3 of
    /// `i101-L1-proposal-per-vm-process-tables-and-a-settled-suspension-word`):
    /// [`await_suspension_settled`] walks the debug state for the threads it
    /// covers ONCE, registering those on their way
    /// ([`DebugState::settle_wanted`]); each round after only drains the
    /// ones whose listing was published since ([`DebugState::settle_arrived`],
    /// pushed by [`note_settle_arrival`]), so the wait's total debug-state
    /// work is the number of arrivals, not the pending threads times the
    /// rounds. A full re-walk of the pending threads still runs every
    /// [`SETTLE_REWALK_BACKSTOP`] (an event that publishes a listing, a death
    /// or a C JVMTI resume). Takes precedence over
    /// [`Self::settle_arrival_count`]. Plain data: never written after
    /// construction.
    settle_pending_counter: bool,
    /// `CRATONVM_JDWP_JDK_BREAKPOINT_EXPANSION_SET`, read once when this VM's
    /// gates are made (interpreter round i6 wave 117, lane L1; performance,
    /// default ON since the i6 wave-126 flip, `=0` off; step 1 of
    /// `i40-L1-proposal-call-site-intrinsics-that-yield-to-a-breakpoint`): a
    /// breakpoint in a JDK class keeps every method interpreted (wave 40)
    /// only when the JIT may expand a method of that class at a call site
    /// without a record ([`jit_expansion_set::may_expand`]); a breakpoint in
    /// any other JDK class takes the scoped, per-class withdrawal an
    /// application class's breakpoint takes. Off: every JDK class holds the
    /// whole VM, as before. Plain data: never written after construction.
    jdk_breakpoint_expansion_set: bool,
    /// `CRATONVM_JDWP_VISIBLE_CLASSES_INITIATED`, read once when this VM's
    /// gates are made (interpreter round i6 wave 118, lane L1; HotSpot
    /// parity, default ON since the i6 wave-126 flip, `=0` off;
    /// `i41-L1-proposal-one-initiating-loader-record-for-every-loader`):
    /// JDWP `ClassLoaderReference.VisibleClasses` answers the classes the
    /// loader is an initiating loader of (`instrument::initiated_class_ids`,
    /// the `getInitiatedClasses` / JVMTI `GetClassLoaderClasses` record), as
    /// HotSpot's back end answers it from `GetClassLoaderClasses`. Off: every
    /// class whose defining loader is the loader or an ancestor (wave 24).
    visible_classes_initiated: bool,
    /// `CRATONVM_JVMTI_C_LISTING_SHARED`, read once when this VM's gates are
    /// made (interpreter round i6 wave 119, lane L1; default ON since the i6
    /// wave-126 flip, `=0` off; i102-L1 item 1 / i46-L1 step 2): the C
    /// table's listing of the calling thread for `GetStackTrace`,
    /// `GetFrameCount` and `GetFrameLocation` (`native_env::listed_rows`
    /// without frames) comes from the one builder JDWP uses
    /// (`interpreter::listing_rows_of_current_thread`), as its other-thread
    /// reads already do, instead of its own stack-trace capture. Off: as
    /// before.
    c_listing_shared: bool,
    /// `CRATONVM_JDWP_INTRINSIC_SHADOW_METHOD_EVENTS`, read once when this
    /// VM's gates are made (interpreter round i6 wave 120, lane L1; default
    /// OFF until `L1W120JdiShadowedMethodEvents` agrees on the host; i40-L1):
    /// a Java method with bytecode that a reviewed `Intrinsic` answers
    /// without running it posts a JDWP `MethodEntry` at index 0 and a
    /// `MethodExit` at its last return, the method listed on top as an
    /// opaque row while the thread is suspended there
    /// (`jvmti_events::report_native_method_entry`), as HotSpot's interpreter
    /// reports the bytecode it runs. Off: no event, as before.
    intrinsic_shadow_method_events: bool,
    /// `CRATONVM_DEBUG_STANDIN_LEVEL_LOCALS`, read once when this VM's gates
    /// are made (interpreter round i6 wave 121, lane L1; default OFF until
    /// `L1W119RawJdwpNativeThisObject` agrees under
    /// `CRATONVM_DEBUG_COMPATIBLE_BLOCKING_BYTECODE=0 --compatible`; i119-L1):
    /// a blocking stand-in's `Object.wait` Java levels, which the stand-in did
    /// not run, answer their receiver (the monitor the thread waits on) and,
    /// for `wait(J)V` under `wait()V`, the `0L` timeout -- to JDWP
    /// (`ThisObject`, `GetValues`) and to the C table's reads of a blocked
    /// thread (`GetLocalInstance`, `GetLocalObject` / `GetLocalLong`). Off:
    /// `OPAQUE_FRAME`, as before. Since wave 129 (lane L1,
    /// `L1W129RawJdwpVirtualWaitThisObject`) also the `VirtualThread`
    /// instance rows an unmounted BOUND virtual thread lists
    /// (`CRATONVM_VT_CONTINUATIONS=0`; `yieldContinuation`, `parkNanos`,
    /// `sleepNanos`): JDWP `ThisObject` / `GetValues` slot 0 is the thread
    /// itself, as on HotSpot.
    standin_level_locals: bool,
    /// `CRATONVM_DEBUG_BLOCKED_DEAD_LOCALS`, read once when this VM's gates
    /// are made (interpreter round i6 wave 126, lane L1; HotSpot parity,
    /// default OFF since the wave-128 review: a dead slot's stale address can
    /// meet a new object after a collection, i126-L1; i47-L1): a reference local
    /// of a thread blocked in a native region (or of an unmounted virtual
    /// thread) that the frame's per-bci liveness calls dead is read through
    /// its blocking deposit's slot origin like a live one, screened as a
    /// running thread's dead local is (`screen_dead_local`: answered while
    /// the tracked address is still an object start). HotSpot's interpreter
    /// keeps every local, so `LockSupport.park(Object)`'s `blocker`, dead at
    /// its `Unsafe.park` call, reads as the blocker there
    /// (`L1W101JdiCompatibleParkFrames`). Off: such a slot reads as `null`.
    blocked_dead_locals: bool,
    /// `CRATONVM_DEBUG_BLOCKED_ROOTED_DEAD_LOCALS`, read once when this VM's
    /// gates are made (interpreter round i6 wave 130, lane L1a; HotSpot
    /// parity, default ON, `=0` off; i47-L1, i126-L1): [`Self::blocked_dead_locals`]'
    /// read, but only in a VM where every local is a GC root
    /// (`jvmti::debugger_observes_locals`: a JDWP port given at start, as
    /// HotSpot keeps every local alive under `can_access_local_variables`).
    /// There every root scan and blocking deposit roots a dead slot's
    /// referent too (`Frame::scan_local_objects_for_vm`), so it cannot die
    /// while the frame holds it, and every fold advances its tracked address
    /// exactly: the wave-128 hazard (a dead referent's stale address meeting
    /// a new object) has no dead referent to start from. Without such a port
    /// the dead slot still reads `null` (unless `blocked_dead_locals`).
    blocked_rooted_dead_locals: bool,
    /// `CRATONVM_JVMTI_C_AGENT_KEEPS_LOCALS_ALIVE`, read once when this VM's
    /// gates are made (interpreter round i6 wave 132, lane L1; HotSpot parity,
    /// default ON, `=0` off; i126-L1): a startup C agent that acquired
    /// `can_access_local_variables` in `Agent_OnLoad` makes every local of
    /// this VM a GC root, as a JDWP port does
    /// ([`Self::c_agent_observes_locals`], `jvmti::debugger_observes_locals`).
    c_agent_keeps_locals_alive: bool,
    /// Set once, by `native_env::bind_startup_envs` in `Vm::new` (before any
    /// Java frame exists), when a startup env holds `can_access_local_variables`
    /// and [`Self::c_agent_keeps_locals_alive`] is on (interpreter round i6
    /// wave 132, lane L1). Never cleared: HotSpot's capability is OnLoad-only
    /// and keeps every local alive for the VM's life
    /// (`JvmtiExport::can_access_local_variables`); a slot the VM stopped
    /// rooting could not be rooted again soundly.
    c_agent_observes_locals: std::sync::atomic::AtomicBool,
    /// t1 wave 2, lane M: set once, by `native_env::bind_startup_envs` in
    /// `Vm::new` (before any Java frame exists and before any compile), when
    /// a startup env acquired `can_get_owned_monitor_info`,
    /// `can_get_owned_monitor_stack_depth_info` or `can_access_local_variables`
    /// (all OnLoad-only). HotSpot's C2 annotates its safepoints for the escape
    /// barrier exactly then (`ConnectionGraph::compute_escape`), so an
    /// eliminated lock is relocked and listed by the owned-monitor answers;
    /// the JIT keeps such locks instead (`jvmti::monitor_observer_may_exist`,
    /// `CRATONVM_JIT_EA_MONITOR_AGENT_KEEPS_LOCKS`). Never cleared.
    c_agent_observes_monitors: std::sync::atomic::AtomicBool,
    /// t1 wave 3, lane M: set when a C agent env of this VM acquired
    /// `can_tag_objects` (by `native_env::bind_startup_envs` for a startup
    /// env, by `AddCapabilities` in the live phase). HotSpot's
    /// `JvmtiExport::can_walk_any_space`: C2 then annotates its safepoints so
    /// heap walks reallocate scalar-replaced objects; the JIT replaces none
    /// instead (`jvmti::object_observer_may_exist`). Never cleared (a
    /// relinquished capability leaves the conservative answer). Bodies
    /// compiled before a live-phase acquisition keep their replacement, as
    /// HotSpot's unannotated older frames do.
    c_agent_walks_heap: std::sync::atomic::AtomicBool,
    /// Arrivals and other changes of the debug state the suspension's waits
    /// look at: bumped by every [`Self::notify_state_changed`] -- every
    /// publication of these gates, every arrival at a suspend point, every
    /// queued task -- with [`Self::settle_arrival_count`] on (wave 64, lane
    /// L6). Per VM, as HotSpot's handshake counts its arrivals on the
    /// operation. Only compared for change, never for a value.
    arrivals: std::sync::atomic::AtomicU64,
    /// `CRATONVM_JVMTI_C_EXCEPTION_CATCH`, read once when this VM's gates
    /// are made (interpreter round i1 wave 52, lane L1; the last item of
    /// `docs/feature-designs/jvmti-delivery-threading.md`):
    /// the C JVMTI table delivers `ExceptionCatch`, with the caught object
    /// (`jvmti::native_env::deliver_exception_catch`); off (`=0`), enabling
    /// it answers `JVMTI_ERROR_NOT_AVAILABLE` as before. On by default. Plain
    /// data: never written after construction.
    c_exception_catch: bool,
    /// `CRATONVM_JVMTI_C_CLASS_HOOKS`, read once when this VM's gates are
    /// made (interpreter round i2 wave 53, lane L1;
    /// `docs/feature-designs/jvmti-delivery-threading.md`):
    /// the C JVMTI table delivers `ClassFileLoadHook` and `ClassLoad`; off
    /// (`=0`), enabling either answers `JVMTI_ERROR_NOT_AVAILABLE` as before.
    /// On by default. Plain
    /// data: never written after construction.
    c_class_hooks: bool,
    /// `CRATONVM_JVMTI_C_RETRANSFORM`, read once when this VM's gates are
    /// made ([`c_retransform_switch`]; interpreter round i2 wave 54, lane L1;
    /// `docs/feature-designs/jvmti-delivery-threading.md`):
    /// the C JVMTI table grants `can_retransform_classes`, answers
    /// `RetransformClasses` and posts `ClassFileLoadHook` at a
    /// retransformation; off (`=0`), the capability is not potential, as before. On by default.
    /// Plain data: never written after construction.
    c_retransform: bool,
    /// `CRATONVM_JVMTI_C_REDEFINE`, read once when this VM's gates are made
    /// ([`c_redefine_switch`]; interpreter round i2 wave 57, lane L1;
    /// `docs/feature-designs/jvmti-delivery-threading.md`):
    /// the C JVMTI table grants `can_redefine_classes` and answers
    /// `RedefineClasses` (`runtime::instrument::redefine_classes_for_jvmti`);
    /// off (`=0`), the capability is not potential, as before. On by default
    /// (wave 57's last commit). Plain data: never written after construction.
    c_redefine: bool,
    /// `CRATONVM_JVMTI_C_IN_NATIVE`, read once when this VM's gates are made
    /// (interpreter round i2 wave 58, lane L1;
    /// `docs/feature-designs/jvmti-delivery-threading.md`):
    /// with a C JVMTI env bound, a thread running a JNI native's C code
    /// publishes it (`GcBlockState::jvmti_in_native`) and the C table's
    /// `GetThreadState` answers `JVMTI_THREAD_STATE_IN_NATIVE` for it; off
    /// (`=0`), never, as before. On by default (wave 58's last commit). Plain
    /// data: never written after construction.
    c_in_native: bool,
    /// `CRATONVM_DEBUG_SUSPEND_AT_NATIVE_TRANSITION`, read once when this
    /// VM's gates are made (gci i16/sn;
    /// `gci-i15rb-vm-default-path-changes-review-findings-FIXED-20261017` finding
    /// 6): a thread a debugger (JDWP or a C JVMTI agent's `SuspendThread`)
    /// holds suspended while its JNI native's C code runs in native stops at
    /// its next native -> VM transition -- the JNIEnv / C JVMTI function it
    /// calls next (`native::jni::hold_in_native_while_suspended`) -- and
    /// waits there, still in native, until it is resumed, as HotSpot's
    /// `ThreadInVMfromNative` does ([`hold_at_native_transition_while_suspended`]).
    /// Off (the default): it keeps running those functions while suspended,
    /// as before. An atomic only so a unit test can arm it on a built VM;
    /// never written outside tests.
    suspend_at_native_transition: std::sync::atomic::AtomicBool,
    /// `CRATONVM_JVMTI_C_FRAME_CONTROL`, read once when this VM's gates are
    /// made ([`c_frame_control_switch`]; interpreter round i2 wave 61, lane
    /// L1;
    /// `docs/feature-designs/jvmti-delivery-threading.md`):
    /// the C JVMTI table grants `can_pop_frame` and `can_force_early_return`
    /// and answers `PopFrame`, `ForceEarlyReturn*` and `ClearAllFramePops`,
    /// and `NotifyFramePop` of a thread suspended while blocked in a native;
    /// off (`=0`), those slots answer `JVMTI_ERROR_NOT_AVAILABLE` as before.
    /// On by default (wave 61's last commit). Plain data: never written after
    /// construction.
    c_frame_control: bool,
    /// `CRATONVM_JVMTI_C_CALLBACK_FRAME_CONTROL`, read once when this VM's
    /// gates are made (interpreter round i2 wave 62, lane L1; stage 1 of
    /// `docs/feature-designs/jvmti-delivery-threading.md`):
    /// with [`Self::c_frame_control`] on, a C agent's `PopFrame` and
    /// `ForceEarlyReturn*` of the calling thread made inside its
    /// `Breakpoint` or `SingleStep` callback act on the Java frame the event
    /// is about, taken as the callback returns, as HotSpot takes them; off,
    /// `OPAQUE_FRAME`, as in wave 61 (`=0`). On by default (wave 62's
    /// switch-on commit). Plain data: never written after construction.
    c_callback_frame_control: bool,
    /// `CRATONVM_JVMTI_C_RUNNING_NATIVE`, read once when this VM's gates are
    /// made (interpreter round i2 wave 61, lane L6;
    /// `docs/feature-designs/jvmti-delivery-threading.md`):
    /// with a C JVMTI env bound, the native-call funnel records the callback
    /// a thread runs (`GcBlockState::funnel_native_callback`), and the C
    /// table's `GetThreadState` adds `JVMTI_THREAD_STATE_IN_NATIVE` for a
    /// thread running a registered native that stands in for a JDK `native`
    /// method outside a blocking region (HotSpot MEASURED `0x400005` inside
    /// `Deflater.deflateBytesBytes`); off (`=0`), never, as before. Plain
    /// data: never written after construction.
    c_running_native: bool,
    /// Who needs the native-call funnel to record the native a thread runs
    /// (wave 61, lane L6): [`RUNNING_NATIVE_SESSION`] while a debugger is
    /// attached, [`RUNNING_NATIVE_C_ENV`] once a C JVMTI env is bound with
    /// [`Self::c_running_native`] on. One load in the funnel
    /// ([`Self::running_native_armed`]), in place of the session flag's.
    /// Wave 133 (lane L1): [`RUNNING_NATIVE_C_LEAF_ONLY`] in place of
    /// [`RUNNING_NATIVE_C_ENV`] when [`Self::c_funnel_record_from_excuse`]
    /// is on and the VM's excuse word carries the callback.
    running_native_record: std::sync::atomic::AtomicU8,
    /// `CRATONVM_JVMTI_C_FUNNEL_RECORD_FROM_EXCUSE`, read once when this
    /// VM's gates are made (interpreter round i6 wave 133, lane L1; a
    /// performance switch, default OFF): with a C JVMTI env bound and the
    /// VM's native-excuse word carrying the callback
    /// (`CRATONVM_HANDSHAKE_NATIVE_EXCUSE` and
    /// `CRATONVM_HANDSHAKE_NATIVE_CALLBACK` both on, the defaults), the
    /// native-call funnel no longer keeps its second record of the same
    /// callback (`GcBlockState::funnel_native_callback`): the C table's
    /// `GetThreadState` reads the word's (`ThreadRegistry::running_funnel_callback`),
    /// which it already preferred. With `CRATONVM_HANDSHAKE_NATIVE_CALLBACK=0`
    /// the record is kept as before, so that kill switch keeps the C
    /// reader's `IN_NATIVE` answer. Plain data: never written after
    /// construction.
    c_funnel_record_from_excuse: bool,
    /// `CRATONVM_JVMTI_C_START_PHASE_HOOK`, read once when this VM's gates
    /// are made (wave 54, lane L1): a C agent's `ClassFileLoadHook` is armed
    /// in JVMTI's start phase too, for the classes the bootstrap loader
    /// defines from `System.initPhase1` on
    /// (`runtime::instrument::start_phase_withholds`); off, from the live
    /// phase only, as in wave 53 (`=0`). On by default. Plain data: never written after
    /// construction.
    c_start_phase_hook: bool,
    /// `CRATONVM_JVMTI_BRIDGE_LISTENER_GATE`, read once when this VM's gates
    /// are made (wave 54, lane L1; option 1 of
    /// `docs/feature-designs/jvmti-delivery-threading.md`):
    /// the D14 bridge (`runtime::jvmti`) forwards an event into the older
    /// `jvmti::EventManager` only while that manager has it enabled
    /// ([`Self::legacy_jvmti_listens`]); off, every bridged event takes the
    /// VM's JVMTI lock and builds its record for no one, as before (`=0`). On by default. Plain
    /// data: never written after construction.
    bridge_listener_gate: bool,
    /// The older `jvmti::EventManager`'s enabled events
    /// (`EventManager::enabled_mask_handle`, shared when the VM is made;
    /// wave 54), read by [`Self::legacy_jvmti_listens`] without the VM's
    /// JVMTI lock.
    legacy_jvmti_events: std::sync::Arc<std::sync::atomic::AtomicU64>,
}

/// `jvmtiPhase` values (`jvmti.h`) of `DebuggerGates`' `jvmti_phase`
/// (interpreter round i2 wave 55, lane L1).
pub const JVMTI_PHASE_PRIMORDIAL: u8 = 2;
pub const JVMTI_PHASE_START: u8 = 6;
pub const JVMTI_PHASE_LIVE: u8 = 4;
pub const JVMTI_PHASE_DEAD: u8 = 8;

/// `CRATONVM_JVMTI_C_PHASES` ([`DebuggerGates::c_jvmti_phases`], wave 55),
/// also asked by a startup agent's env in `Agent_OnLoad`, before its VM and
/// its gates exist (whether `VMStart` can be enabled).
pub(crate) fn c_jvmti_phases_switch() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JVMTI_C_PHASES")
}

/// `CRATONVM_JVMTI_C_CLASS_HOOKS` ([`DebuggerGates::c_class_hooks`]), also
/// asked by a startup agent's env in `Agent_OnLoad` (whether
/// `can_generate_all_class_hook_events` is potential; wave 55).
pub(crate) fn c_class_hooks_switch() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JVMTI_C_CLASS_HOOKS")
}

/// `CRATONVM_JVMTI_C_RETRANSFORM` ([`DebuggerGates::c_retransform`]), also
/// asked by a startup agent's env in `Agent_OnLoad`, before its VM and its
/// gates exist (`jvmti::native_env`'s potential capabilities).
pub(crate) fn c_retransform_switch() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JVMTI_C_RETRANSFORM")
}

/// `CRATONVM_JVMTI_C_REDEFINE` ([`DebuggerGates::c_redefine`], wave 57), also
/// asked by a startup agent's env in `Agent_OnLoad`, as
/// [`c_retransform_switch`] is.
pub(crate) fn c_redefine_switch() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JVMTI_C_REDEFINE")
}

/// `CRATONVM_JVMTI_C_FRAME_CONTROL` ([`DebuggerGates::c_frame_control`],
/// wave 61), also asked by a startup agent's env in `Agent_OnLoad`, as
/// [`c_retransform_switch`] is.
pub(crate) fn c_frame_control_switch() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JVMTI_C_FRAME_CONTROL")
}

impl DebuggerGates {
    /// Must the D14 bridge forward `event` into the older
    /// `jvmti::EventManager` of this VM (wave 54,
    /// [`Self::bridge_listener_gate`])? `true` while the gate is off; with it
    /// on, one relaxed load: whether that manager has `event` enabled.
    #[inline]
    pub fn legacy_jvmti_listens(&self, event: crate::jvmti::JvmtiEvent) -> bool {
        !self.bridge_listener_gate
            || self
                .legacy_jvmti_events
                .load(std::sync::atomic::Ordering::Relaxed)
                & crate::jvmti::events::event_mask_bit(event)
                != 0
    }

    /// These gates, reading the older `jvmti::EventManager`'s enabled events
    /// from `mask` (`EventManager::enabled_mask_handle`; wave 54). `Vm::new`
    /// makes a VM's gates with its manager's mask.
    pub fn with_legacy_jvmti_events(mask: std::sync::Arc<std::sync::atomic::AtomicU64>) -> Self {
        Self {
            legacy_jvmti_events: mask,
            ..Self::new()
        }
    }

    /// Does the C JVMTI table grant `can_retransform_classes`
    /// ([`Self::c_retransform`], wave 54)?
    #[inline]
    pub fn c_retransform(&self) -> bool {
        self.c_retransform
    }

    /// Does the C JVMTI table grant `can_redefine_classes`
    /// ([`Self::c_redefine`], wave 57)?
    #[inline]
    pub fn c_redefine(&self) -> bool {
        self.c_redefine
    }

    /// Does the C JVMTI table serve frame control: `PopFrame`,
    /// `ForceEarlyReturn*`, `ClearAllFramePops`, `NotifyFramePop` of a
    /// blocked thread ([`Self::c_frame_control`], wave 61)?
    #[inline]
    pub fn c_frame_control(&self) -> bool {
        self.c_frame_control
    }

    /// Does the C JVMTI table serve `PopFrame` / `ForceEarlyReturn*` of the
    /// calling thread from its `Breakpoint` / `SingleStep` callback
    /// ([`Self::c_callback_frame_control`], wave 62)? Only with frame control
    /// itself on.
    #[inline]
    pub fn c_callback_frame_control(&self) -> bool {
        self.c_frame_control && self.c_callback_frame_control
    }

    /// Is a C agent's `ClassFileLoadHook` armed in the start phase
    /// ([`Self::c_start_phase_hook`], wave 54)?
    #[inline]
    pub fn c_start_phase_hook(&self) -> bool {
        self.c_start_phase_hook
    }

    /// Does the C JVMTI table deliver `ClassFileLoadHook` and `ClassLoad`
    /// ([`Self::c_class_hooks`], wave 53)?
    #[inline]
    pub fn c_class_hooks(&self) -> bool {
        self.c_class_hooks
    }

    /// Does the C JVMTI table deliver `ExceptionCatch`
    /// ([`Self::c_exception_catch`], wave 52)?
    #[inline]
    pub fn c_exception_catch(&self) -> bool {
        self.c_exception_catch
    }

    /// Does C JVMTI `GetThreadState` read the suspension hint before the
    /// debug-state lock ([`Self::state_suspension_hint`], interpreter round
    /// i3 wave 72, lane L6)?
    #[inline]
    pub fn state_suspension_hint(&self) -> bool {
        self.state_suspension_hint
    }

    /// Does the C JVMTI `jthread` lookup leave native through the vm-only
    /// entry ([`Self::jvmti_thread_lookup_vm_only`], interpreter round i6
    /// wave 130, lane L1b)?
    #[inline]
    pub fn jvmti_thread_lookup_vm_only(&self) -> bool {
        self.jvmti_thread_lookup_vm_only
    }

    /// Does the C JVMTI `GetThreadState` read its registry facts in one
    /// `threads` read ([`Self::jvmti_thread_state_one_read`], interpreter
    /// round i6 wave 132, lane L1)?
    #[inline]
    pub(crate) fn jvmti_thread_state_one_read(&self) -> bool {
        self.jvmti_thread_state_one_read
    }

    /// Does the C JVMTI `GetThreadState` read the `SUSPENDED` bit from the
    /// thread's own word ([`Self::jvmti_state_suspend_word`], interpreter
    /// round i7 wave 135, lane L1)?
    #[inline]
    pub(crate) fn jvmti_state_suspend_word(&self) -> bool {
        self.jvmti_state_suspend_word
    }

    /// Does a JDWP suspension wait for its threads before its reply
    /// ([`Self::suspend_handshake`], wave 49)?
    #[inline]
    pub fn suspend_handshake(&self) -> bool {
        self.suspend_handshake
    }

    /// Do the event-thread and the poll's `ClassPrepare` suspensions wait for
    /// their threads before their event set goes out
    /// ([`Self::suspend_event_settle`], interpreter round i4 wave 94)?
    #[inline]
    pub fn suspend_event_settle(&self) -> bool {
        self.suspend_handshake && self.suspend_event_settle
    }

    /// Does an invocation's reply wait for the threads its closing
    /// re-suspension covered ([`Self::invoke_reply_settle`], interpreter
    /// round i5 wave 101)?
    #[inline]
    pub fn invoke_reply_settle(&self) -> bool {
        self.suspend_handshake && self.invoke_reply_settle
    }

    /// Does a thread in a native outside a blocking region acknowledge a
    /// JDWP suspension ([`Self::suspend_native_ack`], wave 53)?
    #[inline]
    pub fn suspend_native_ack(&self) -> bool {
        self.suspend_native_ack
    }

    /// Does a thread that wins a contended monitor while suspended give it
    /// back until its resume ([`Self::monitor_exit_on_suspend`], wave 54)?
    #[inline]
    pub fn monitor_exit_on_suspend(&self) -> bool {
        self.monitor_exit_on_suspend
    }

    /// Does a thread in a native list its frames through a window while the
    /// native runs ([`Self::native_frames_window`], wave 56)?
    #[inline]
    pub fn native_frames_window(&self) -> bool {
        self.native_frames_window
    }

    /// Does a native frames window under compiled code carry a copy of the
    /// thread's JIT entry chain for the reader to walk
    /// ([`Self::native_window_chain`], wave 58)?
    #[inline]
    pub fn native_window_chain(&self) -> bool {
        self.native_window_chain
    }

    /// Does the suspension's settle wait take the long bound
    /// ([`Self::suspend_settle_long`], wave 56)?
    #[inline]
    pub fn suspend_settle_long(&self) -> bool {
        self.suspend_settle_long
    }

    /// Do the suspension machinery's waits wait on [`Self::state_changed`]
    /// ([`Self::park_condvar`], wave 54)?
    #[inline]
    pub fn park_condvar(&self) -> bool {
        self.park_condvar
    }

    /// Do the suspension's waits wait on the thread's own suspend word
    /// ([`Self::thread_suspend_word`], wave 63)?
    #[inline]
    pub fn thread_suspend_word(&self) -> bool {
        self.thread_suspend_word
    }

    /// Does a stop-the-world request unpark the threads parked on their
    /// suspend word ([`Self::stw_unparks_parked`], wave 64)? Only with
    /// [`Self::thread_suspend_word`]: without it no park waits on its word.
    #[inline]
    pub fn stw_unparks_parked(&self) -> bool {
        self.thread_suspend_word && self.stw_unparks_parked
    }

    /// Does a park at a suspend point wait GC-blocked
    /// ([`Self::park_gc_blocked`], wave 65)? Only with
    /// [`Self::thread_suspend_word`]: the region's wait is the word's wait,
    /// which a pause does not need to end.
    #[inline]
    pub fn park_gc_blocked(&self) -> bool {
        self.thread_suspend_word && self.park_gc_blocked
    }

    /// Does heap work prefer a thread outside a GC-blocked park region
    /// ([`Self::heap_work_outside_region`], wave 66)? Only with
    /// [`Self::park_gc_blocked`]: without it no park waits in a region.
    #[inline]
    pub fn heap_work_outside_region(&self) -> bool {
        self.park_gc_blocked() && self.heap_work_outside_region
    }

    /// Does a park re-entering its region reuse its standing deposit
    /// ([`Self::park_region_reuses_deposit`], wave 66)? Only with
    /// [`Self::park_gc_blocked`].
    #[inline]
    pub fn park_region_reuses_deposit(&self) -> bool {
        self.park_gc_blocked() && self.park_region_reuses_deposit
    }

    /// Does a park open its GC-blocked region only at the first pause of its
    /// hold ([`Self::park_region_at_first_pause`], wave 68)? Only with
    /// [`Self::park_gc_blocked`]: without it no park waits in a region.
    #[inline]
    pub fn park_region_at_first_pause(&self) -> bool {
        self.park_gc_blocked() && self.park_region_at_first_pause
    }

    /// Does a park region left for heap work keep its entry's snapshot when
    /// nothing folded into it ([`Self::park_leave_keeps_unfolded_snapshot`],
    /// wave 68)? Only with [`Self::park_region_reuses_deposit`]: the kept
    /// snapshot is the one a re-entry then reuses.
    #[inline]
    pub fn park_leave_keeps_unfolded_snapshot(&self) -> bool {
        self.park_region_reuses_deposit() && self.park_leave_keeps_unfolded_snapshot
    }

    /// How long a park at a suspend point waits on its word between two
    /// looks at `stw_requested` (wave 64, lane L6): [`DEBUG_STATE_POLL`],
    /// unless `unparked_by_stw` -- the park enrolled with the GC barrier
    /// ([`Self::stw_unparks_parked`]), whose request wakes it -- when the
    /// word's own [`SUSPEND_WORD_BACKSTOP`] is the only bound.
    #[inline]
    pub fn parked_word_wait_bound(unparked_by_stw: bool) -> std::time::Duration {
        if unparked_by_stw {
            SUSPEND_WORD_BACKSTOP
        } else {
            DEBUG_STATE_POLL
        }
    }

    /// Wake every wait on the debug state ([`Self::state_changed`], wave
    /// 54): a suspend count, a queued task, an arrival changed. Nothing with
    /// [`Self::park_condvar`] off. Callable with or without the debug-state
    /// lock held.
    #[inline]
    pub(crate) fn notify_state_changed(&self) {
        // Wave 64 (lane L6): before the notification, so a waiter woken by
        // it reads the moved count.
        if self.settle_arrival_count {
            self.arrivals
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }
        if self.park_condvar {
            self.state_changed.notify_all();
        }
    }

    /// Does the C table list the calling thread from the shared builder
    /// ([`Self::c_listing_shared`], interpreter round i6 wave 119)?
    #[inline]
    pub(crate) fn c_listing_shared(&self) -> bool {
        self.c_listing_shared
    }

    /// Does a reviewed-`Intrinsic` shadow post frame-only JDWP method events
    /// ([`Self::intrinsic_shadow_method_events`], interpreter round i6 wave
    /// 120)?
    #[inline]
    pub(crate) fn intrinsic_shadow_method_events(&self) -> bool {
        self.intrinsic_shadow_method_events
    }

    /// Do a stand-in's `Object.wait` levels answer their locals
    /// ([`Self::standin_level_locals`], interpreter round i6 wave 121)?
    #[inline]
    pub(crate) fn standin_level_locals(&self) -> bool {
        self.standin_level_locals
    }

    /// Does a blocked thread's liveness-dead reference local read through
    /// its slot origin ([`Self::blocked_dead_locals`], interpreter round i6
    /// wave 126)?
    #[inline]
    pub(crate) fn blocked_dead_locals(&self) -> bool {
        self.blocked_dead_locals
    }

    /// Does a blocked thread's liveness-dead reference local read through
    /// its slot origin when every local is a root
    /// ([`Self::blocked_rooted_dead_locals`], interpreter round i6 wave 130)?
    #[inline]
    pub(crate) fn blocked_rooted_dead_locals(&self) -> bool {
        self.blocked_rooted_dead_locals
    }

    /// Does a startup C agent's `can_access_local_variables` keep every local
    /// alive ([`Self::c_agent_keeps_locals_alive`], interpreter round i6
    /// wave 132)?
    #[inline]
    pub(crate) fn c_agent_keeps_locals_alive(&self) -> bool {
        self.c_agent_keeps_locals_alive
    }

    /// Did a startup C agent of this VM acquire `can_access_local_variables`
    /// with [`Self::c_agent_keeps_locals_alive`] on
    /// ([`Self::c_agent_observes_locals`], interpreter round i6 wave 132)?
    /// One `Acquire` load; read by every root scan through
    /// `jvmti::debugger_observes_locals`.
    #[inline]
    pub(crate) fn c_agent_observes_locals(&self) -> bool {
        self.c_agent_observes_locals.load(std::sync::atomic::Ordering::Acquire)
    }

    /// `native_env::bind_startup_envs`: a startup env holds
    /// `can_access_local_variables` (interpreter round i6 wave 132, lane L1).
    /// A no-op with the switch off.
    pub(crate) fn note_c_agent_observes_locals(&self) {
        if self.c_agent_keeps_locals_alive {
            self.c_agent_observes_locals.store(true, std::sync::atomic::Ordering::Release);
        }
    }

    /// Did a startup C agent of this VM acquire a capability that observes
    /// owned monitors or locals ([`Self::c_agent_observes_monitors`], t1
    /// wave 2, lane M)? One `Acquire` load, asked by every compile door.
    #[inline]
    pub(crate) fn c_agent_observes_monitors(&self) -> bool {
        self.c_agent_observes_monitors.load(std::sync::atomic::Ordering::Acquire)
    }

    /// `native_env::bind_startup_envs`: a startup env holds a monitor/locals
    /// observing capability (t1 wave 2, lane M).
    pub(crate) fn note_c_agent_observes_monitors(&self) {
        self.c_agent_observes_monitors.store(true, std::sync::atomic::Ordering::Release);
    }

    /// Did a C agent env of this VM acquire `can_tag_objects`
    /// ([`Self::c_agent_walks_heap`], t1 wave 3, lane M)? One `Acquire` load.
    #[inline]
    pub(crate) fn c_agent_walks_heap(&self) -> bool {
        self.c_agent_walks_heap.load(std::sync::atomic::Ordering::Acquire)
    }

    /// A C agent env acquired `can_tag_objects` (t1 wave 3, lane M). Returns
    /// whether this call raised the gate (t1 wave 5: the caller then refuses
    /// the compiles already in flight, `JitCache::refuse_in_flight_for_object_observer`).
    pub(crate) fn note_c_agent_walks_heap(&self) -> bool {
        !self.c_agent_walks_heap.swap(true, std::sync::atomic::Ordering::AcqRel)
    }

    /// Does JDWP `VisibleClasses` answer the loader's initiated classes
    /// ([`Self::visible_classes_initiated`], interpreter round i6 wave 118)?
    #[inline]
    pub(crate) fn visible_classes_initiated(&self) -> bool {
        self.visible_classes_initiated
    }

    /// May a breakpoint in a method of the class `class_name` be missed by a
    /// caller compiled after it, which the JIT can expand the method into
    /// without a record (interpreter round i1 wave 40; wave 117's
    /// [`Self::jdk_breakpoint_expansion_set`] narrows the JDK classes to
    /// [`jit_expansion_set::may_expand`])? Such a breakpoint keeps every
    /// method interpreted and takes the whole-cache withdrawal.
    pub(crate) fn breakpoint_may_be_expanded(&self, class_name: &str) -> bool {
        crate::runtime::interpreter::jit_may_expand_a_method_of_without_a_record(class_name)
            && (!self.jdk_breakpoint_expansion_set || jit_expansion_set::may_expand(class_name))
    }

    /// Does the settle wait walk once and then drain only the arrivals
    /// ([`Self::settle_pending_counter`], interpreter round i6 wave 117)?
    #[inline]
    pub fn settle_pending_counter(&self) -> bool {
        self.settle_pending_counter
    }

    /// Does the settle wait walk only on an arrival
    /// ([`Self::settle_arrival_count`], wave 64)?
    #[inline]
    pub fn settle_arrival_count(&self) -> bool {
        self.settle_arrival_count
    }

    /// [`Self::arrivals`] now (wave 64, lane L6). Read BEFORE the walk it
    /// stamps: a change published after the read moves the count again.
    #[inline]
    pub(crate) fn arrivals(&self) -> u64 {
        self.arrivals.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// Wait for the next change of the debug state `ds` guards, for at most
    /// `bound`, the lock released meanwhile and held again on return
    /// (interpreter round i2 wave 54, lane L6). With [`Self::park_condvar`]
    /// off, the 1 ms poll every such wait made before: the lock released,
    /// then [`DEBUG_STATE_POLL`] of sleep, whatever `bound` says. Answers
    /// whether a notification (not the bound, nor the poll) ended the wait.
    pub(crate) fn await_state_change(
        &self,
        ds: &mut parking_lot::MutexGuard<'_, DebugState>,
        bound: std::time::Duration,
    ) -> bool {
        if self.park_condvar {
            !self.state_changed.wait_for(ds, bound).timed_out()
        } else {
            parking_lot::MutexGuard::unlocked(ds, || std::thread::sleep(DEBUG_STATE_POLL));
            false
        }
    }

    /// Does a suspended unmounted virtual thread list HotSpot's default
    /// continuation rows ([`Self::virtual_continuation_frames`], wave 49)?
    #[inline]
    pub fn virtual_continuation_frames(&self) -> bool {
        self.virtual_continuation_frames
    }

    /// Does this VM's debugger listing splice the JNI natives and the
    /// reflective calls' JDK frames in ([`Self::one_frame_listing`], wave 47)?
    #[inline]
    pub fn one_frame_listing(&self) -> bool {
        self.one_frame_listing
    }

    /// Does a `--compatible` blocking stand-in run its JDK bytecode while a
    /// debugger is attached ([`Self::compatible_blocking_bytecode`], wave 48)?
    #[inline]
    pub fn compatible_blocking_bytecode(&self) -> bool {
        self.compatible_blocking_bytecode
    }

    pub fn new() -> Self {
        Self {
            one_frame_listing: cratonvm_types::flags::runtime_flag_default_on(
                "CRATONVM_DEBUG_ONE_FRAME_LISTING",
            ),
            compatible_blocking_bytecode: cratonvm_types::flags::runtime_flag_default_on(
                "CRATONVM_DEBUG_COMPATIBLE_BLOCKING_BYTECODE",
            ),
            virtual_continuation_frames: cratonvm_types::flags::runtime_flag_default_on(
                "CRATONVM_DEBUG_VIRTUAL_CONTINUATION_FRAMES",
            ),
            suspend_handshake: cratonvm_types::flags::runtime_flag_default_on(
                "CRATONVM_DEBUG_SUSPEND_HANDSHAKE",
            ),
            suspend_event_settle: cratonvm_types::flags::runtime_flag_default_on(
                "CRATONVM_DEBUG_SUSPEND_EVENT_SETTLE",
            ),
            invoke_reply_settle: cratonvm_types::flags::runtime_flag_default_on(
                "CRATONVM_DEBUG_INVOKE_REPLY_SETTLE",
            ),
            state_suspension_hint: cratonvm_types::flags::runtime_flag_on(
                "CRATONVM_JVMTI_STATE_SUSPENSION_HINT",
            ),
            jvmti_thread_lookup_vm_only: cratonvm_types::flags::runtime_flag_default_on(
                "CRATONVM_JVMTI_THREAD_LOOKUP_VM_ONLY",
            ),
            jvmti_thread_state_one_read: cratonvm_types::flags::runtime_flag_default_on(
                "CRATONVM_JVMTI_THREAD_STATE_ONE_READ",
            ),
            jvmti_state_suspend_word: cratonvm_types::flags::runtime_flag_on(
                "CRATONVM_JVMTI_STATE_SUSPEND_WORD",
            ),
            suspend_native_ack: cratonvm_types::flags::runtime_flag_default_on(
                "CRATONVM_DEBUG_SUSPEND_NATIVE_ACK",
            ),
            monitor_exit_on_suspend: cratonvm_types::flags::runtime_flag_default_on(
                "CRATONVM_DEBUG_MONITOR_EXIT_ON_SUSPEND",
            ),
            native_frames_window: cratonvm_types::flags::runtime_flag_default_on(
                "CRATONVM_DEBUG_NATIVE_FRAMES_WINDOW",
            ),
            native_window_chain: cratonvm_types::flags::runtime_flag_default_on(
                "CRATONVM_DEBUG_NATIVE_WINDOW_CHAIN",
            ),
            suspend_settle_long: cratonvm_types::flags::runtime_flag_default_on(
                "CRATONVM_DEBUG_SUSPEND_SETTLE_LONG",
            ),
            park_condvar: cratonvm_types::flags::runtime_flag_default_on(
                "CRATONVM_DEBUG_PARK_CONDVAR",
            ),
            state_changed: parking_lot::Condvar::new(),
            thread_suspend_word: cratonvm_types::flags::runtime_flag_default_on(
                "CRATONVM_DEBUG_THREAD_SUSPEND_WORD",
            ),
            stw_unparks_parked: cratonvm_types::flags::runtime_flag_on(
                "CRATONVM_DEBUG_STW_UNPARKS_PARKED",
            ),
            park_gc_blocked: cratonvm_types::flags::runtime_flag_on(
                "CRATONVM_DEBUG_PARK_GC_BLOCKED",
            ),
            heap_work_outside_region: cratonvm_types::flags::runtime_flag_on(
                "CRATONVM_DEBUG_HEAP_WORK_OUTSIDE_REGION",
            ),
            park_region_reuses_deposit: cratonvm_types::flags::runtime_flag_on(
                "CRATONVM_DEBUG_PARK_REGION_REUSES_DEPOSIT",
            ),
            park_region_at_first_pause: cratonvm_types::flags::runtime_flag_on(
                "CRATONVM_DEBUG_PARK_REGION_AT_FIRST_PAUSE",
            ),
            park_leave_keeps_unfolded_snapshot: cratonvm_types::flags::runtime_flag_on(
                "CRATONVM_DEBUG_PARK_LEAVE_KEEPS_UNFOLDED_SNAPSHOT",
            ),
            settle_arrival_count: cratonvm_types::flags::runtime_flag_on(
                "CRATONVM_DEBUG_SETTLE_ARRIVAL_COUNT",
            ),
            settle_pending_counter: cratonvm_types::flags::runtime_flag_default_on(
                "CRATONVM_DEBUG_SETTLE_PENDING_COUNTER",
            ),
            jdk_breakpoint_expansion_set: cratonvm_types::flags::runtime_flag_default_on(
                "CRATONVM_JDWP_JDK_BREAKPOINT_EXPANSION_SET",
            ),
            visible_classes_initiated: cratonvm_types::flags::runtime_flag_default_on(
                "CRATONVM_JDWP_VISIBLE_CLASSES_INITIATED",
            ),
            c_listing_shared: cratonvm_types::flags::runtime_flag_default_on(
                "CRATONVM_JVMTI_C_LISTING_SHARED",
            ),
            intrinsic_shadow_method_events: cratonvm_types::flags::runtime_flag_on(
                "CRATONVM_JDWP_INTRINSIC_SHADOW_METHOD_EVENTS",
            ),
            standin_level_locals: cratonvm_types::flags::runtime_flag_on(
                "CRATONVM_DEBUG_STANDIN_LEVEL_LOCALS",
            ),
            blocked_dead_locals: cratonvm_types::flags::runtime_flag_on(
                "CRATONVM_DEBUG_BLOCKED_DEAD_LOCALS",
            ),
            blocked_rooted_dead_locals: cratonvm_types::flags::runtime_flag_default_on(
                "CRATONVM_DEBUG_BLOCKED_ROOTED_DEAD_LOCALS",
            ),
            c_agent_keeps_locals_alive: cratonvm_types::flags::runtime_flag_default_on(
                "CRATONVM_JVMTI_C_AGENT_KEEPS_LOCALS_ALIVE",
            ),
            c_agent_observes_locals: std::sync::atomic::AtomicBool::new(false),
            c_agent_observes_monitors: std::sync::atomic::AtomicBool::new(false),
            c_agent_walks_heap: std::sync::atomic::AtomicBool::new(false),
            arrivals: std::sync::atomic::AtomicU64::new(0),
            c_exception_catch: cratonvm_types::flags::runtime_flag_default_on(
                "CRATONVM_JVMTI_C_EXCEPTION_CATCH",
            ),
            c_class_hooks: c_class_hooks_switch(),
            c_retransform: c_retransform_switch(),
            c_redefine: c_redefine_switch(),
            c_frame_control: c_frame_control_switch(),
            c_callback_frame_control: cratonvm_types::flags::runtime_flag_default_on(
                "CRATONVM_JVMTI_C_CALLBACK_FRAME_CONTROL",
            ),
            c_in_native: cratonvm_types::flags::runtime_flag_default_on(
                "CRATONVM_JVMTI_C_IN_NATIVE",
            ),
            suspend_at_native_transition: std::sync::atomic::AtomicBool::new(
                cratonvm_types::flags::runtime_flag_on("CRATONVM_DEBUG_SUSPEND_AT_NATIVE_TRANSITION"),
            ),
            c_running_native: cratonvm_types::flags::runtime_flag_default_on(
                "CRATONVM_JVMTI_C_RUNNING_NATIVE",
            ),
            running_native_record: std::sync::atomic::AtomicU8::new(0),
            c_funnel_record_from_excuse: cratonvm_types::flags::runtime_flag_on(
                "CRATONVM_JVMTI_C_FUNNEL_RECORD_FROM_EXCUSE",
            ),
            c_start_phase_hook: cratonvm_types::flags::runtime_flag_default_on(
                "CRATONVM_JVMTI_C_START_PHASE_HOOK",
            ),
            bridge_listener_gate: cratonvm_types::flags::runtime_flag_default_on(
                "CRATONVM_JVMTI_BRIDGE_LISTENER_GATE",
            ),
            legacy_jvmti_events: std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0)),
            generation: std::sync::atomic::AtomicU64::new(0),
            all_methods: std::sync::atomic::AtomicBool::new(false),
            breakpoint_classes: std::sync::atomic::AtomicU64::new(0),
            breakpoint_methods: parking_lot::RwLock::new(HashSet::new()),
            field_watch: std::sync::atomic::AtomicBool::new(false),
            exceptions: std::sync::atomic::AtomicBool::new(false),
            interpret_all: std::sync::atomic::AtomicBool::new(false),
            class_prepare: std::sync::atomic::AtomicBool::new(false),
            session: std::sync::atomic::AtomicBool::new(false),
            jdwp_listening: std::sync::atomic::AtomicBool::new(false),
            proxy_rows_incomplete: std::sync::atomic::AtomicBool::new(false),
            suspension: std::sync::atomic::AtomicBool::new(false),
            suspension_counted: std::sync::atomic::AtomicBool::new(false),
            native_class_prepare: std::sync::atomic::AtomicBool::new(false),
            native_thread_events: std::sync::atomic::AtomicBool::new(false),
            destroy_java_vm_split: std::sync::atomic::AtomicBool::new(false),
            native_events: std::sync::atomic::AtomicU32::new(0),
            jdwp_native_events: std::sync::atomic::AtomicU32::new(0),
            native_class_load: std::sync::atomic::AtomicBool::new(false),
            native_class_file_load_hook: std::sync::atomic::AtomicBool::new(false),
            native_env_present: std::sync::atomic::AtomicBool::new(false),
            blocking_standins: std::array::from_fn(|_| std::sync::atomic::AtomicUsize::new(0)),
            jvmti_phase: std::sync::atomic::AtomicU8::new(JVMTI_PHASE_LIVE),
            c_jvmti_phases: c_jvmti_phases_switch(),
            c_lambda_classes: cratonvm_types::flags::runtime_flag_default_on(
                "CRATONVM_JVMTI_C_LAMBDA_CLASSES",
            ),
            c_lambda_methods: cratonvm_types::flags::runtime_flag_default_on(
                "CRATONVM_JVMTI_C_LAMBDA_METHODS",
            ),
            c_primitive_classes: cratonvm_types::flags::runtime_flag_default_on(
                "CRATONVM_JVMTI_C_PRIMITIVE_CLASSES",
            ),
            c_lambda_code: cratonvm_types::flags::runtime_flag_default_on(
                "CRATONVM_JVMTI_C_LAMBDA_CODE",
            ),
            c_lambda_frames: cratonvm_types::flags::runtime_flag_default_on(
                "CRATONVM_JVMTI_C_LAMBDA_FRAMES",
            ),
            c_lambda_events: cratonvm_types::flags::runtime_flag_default_on(
                "CRATONVM_JVMTI_C_LAMBDA_EVENTS",
            ),
            jni_lambda_method_ids: cratonvm_types::flags::runtime_flag_default_on(
                "CRATONVM_JNI_LAMBDA_METHOD_IDS",
            ),
            method_entry: RequestVmsEdge::new(METHOD_ENTRY_COUNT),
            method_events: std::sync::atomic::AtomicU8::new(0),
            stops: std::sync::atomic::AtomicBool::new(false),
            jdwp_deferred_start: std::sync::atomic::AtomicBool::new(false),
            frame_writes: std::sync::atomic::AtomicBool::new(false),
            compiled_home_writes: std::sync::atomic::AtomicBool::new(false),
            withdrawal: std::sync::atomic::AtomicU8::new(0),
            withdrawal_linger_until: std::sync::atomic::AtomicU64::new(0),
            withdrawal_generation: std::sync::atomic::AtomicU64::new(0),
            withdrawals_held: std::sync::atomic::AtomicU64::new(0),
            linger_clock_origin: std::time::Instant::now(),
            breakpoint_generation: std::sync::atomic::AtomicU64::new(0),
            caller_verdicts: parking_lot::Mutex::new(HashMap::new()),
            scoped_method_events: cratonvm_types::flags::runtime_flag_default_on(
                "CRATONVM_JDWP_SCOPED_METHOD_EVENTS",
            ),
            exceptions_from_compiled: cratonvm_types::flags::runtime_flag_default_on(
                "CRATONVM_JVMTI_EXCEPTION_FROM_COMPILED",
            ),
            compiled_exception_requests: RequestVmsEdge::new(COMPILED_EXCEPTION_COUNT),
            field_watch_scope: field_watch_scope::FieldWatchScope::new(),
            method_event_class_bits: std::sync::atomic::AtomicU64::new(0),
            method_event_classes: parking_lot::RwLock::new(HashSet::new()),
            method_event_specs: parking_lot::RwLock::new(Vec::new()),
            method_event_slots_seen: std::sync::atomic::AtomicUsize::new(0),
            method_event_degraded: std::sync::atomic::AtomicBool::new(false),
            method_event_degraded_published: std::sync::atomic::AtomicBool::new(false),
            method_event_scope_armed: std::sync::atomic::AtomicBool::new(false),
            method_event_scope_generation: std::sync::atomic::AtomicU64::new(0),
            method_event_scope_published: std::sync::atomic::AtomicU64::new(0),
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

    /// Is this VM's `-agentlib:jdwp` start deferred to a matching throw
    /// (`onthrow=` / `onuncaught=y`; interpreter round i7 wave 144, lane L1,
    /// [`start_deferred_jdwp_agent`])? One load, asked by the unwinder for
    /// every exception it routes.
    #[inline(always)]
    pub fn jdwp_deferred_start_armed(&self) -> bool {
        self.jdwp_deferred_start
            .load(std::sync::atomic::Ordering::Acquire)
    }

    /// Defer the agent's start to a matching throw (wave 144).
    pub(crate) fn arm_jdwp_deferred_start(&self) {
        self.jdwp_deferred_start
            .store(true, std::sync::atomic::Ordering::Release);
    }

    /// Claim the deferred start, once: answers whether this caller does it
    /// (two threads throwing at once start it once, as the back end's
    /// `initMonitor` serializes `initialize`).
    pub(crate) fn take_jdwp_deferred_start(&self) -> bool {
        self.jdwp_deferred_start
            .swap(false, std::sync::atomic::Ordering::AcqRel)
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

    /// Is a `SetValues` to a compiled activation waiting for its post-call
    /// exit in this VM (interpreter round i6 wave 123, lane L2;
    /// [`DebugState::compiled_home_writes`])? One load, asked by the deopt
    /// resume's frame build.
    #[inline(always)]
    pub(crate) fn compiled_home_writes_pending(&self) -> bool {
        self.compiled_home_writes
            .load(std::sync::atomic::Ordering::Acquire)
    }

    /// Raise or lower [`Self::compiled_home_writes_pending`]; under the
    /// debug-state lock, from whether [`DebugState::compiled_home_writes`] is
    /// empty.
    pub(crate) fn set_compiled_home_writes_pending(&self, pending: bool) {
        self.compiled_home_writes
            .store(pending, std::sync::atomic::Ordering::Release);
    }

    /// Is this VM before JVMTI's live phase for its C JVMTI envs: in the
    /// primordial or the start phase (wave 14; `jvmti_phase`)? One
    /// load, asked by `SharedVm::set_init_level` and the C table's phase
    /// checks; the functions JVMTI allows in the OnLoad and live phases only
    /// answer `JVMTI_ERROR_WRONG_PHASE` in both.
    #[inline(always)]
    pub fn jvmti_start_phase(&self) -> bool {
        matches!(
            self.jvmti_phase(),
            JVMTI_PHASE_PRIMORDIAL | JVMTI_PHASE_START
        )
    }

    /// Is this VM outside JVMTI's live phase for its C JVMTI envs: primordial,
    /// start or (wave 55) dead? The live-only and OnLoad-or-live functions of
    /// the C table answer `JVMTI_ERROR_WRONG_PHASE` then.
    #[inline(always)]
    pub fn jvmti_outside_live(&self) -> bool {
        self.jvmti_phase() != JVMTI_PHASE_LIVE
    }

    /// This VM's `jvmtiPhase` for its C JVMTI envs (the `jvmti_phase` field,
    /// wave 55).
    #[inline(always)]
    pub fn jvmti_phase(&self) -> u8 {
        self.jvmti_phase.load(std::sync::atomic::Ordering::Acquire)
    }

    /// Move the `jvmti_phase` field from `from` to `to`; answers whether this
    /// call made the move, so each transition happens once.
    pub(crate) fn advance_jvmti_phase(&self, from: u8, to: u8) -> bool {
        use std::sync::atomic::Ordering;
        self.jvmti_phase
            .compare_exchange(from, to, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    }

    /// Does the C JVMTI table know the lambda proxy classes
    /// ([`Self::c_lambda_classes`], wave 55)?
    #[inline]
    pub fn c_lambda_classes(&self) -> bool {
        self.c_lambda_classes
    }

    /// Does the C JVMTI table know a lambda proxy class's methods
    /// ([`Self::c_lambda_methods`], wave 56)? Only with the classes
    /// themselves ([`Self::c_lambda_classes`]).
    #[inline]
    pub fn c_lambda_methods(&self) -> bool {
        self.c_lambda_methods && self.c_lambda_classes
    }

    /// Does the C JVMTI table answer a lambda proxy method's spun code
    /// ([`Self::c_lambda_code`], wave 57)? Only with its methods
    /// ([`Self::c_lambda_methods`]).
    #[inline]
    pub fn c_lambda_code(&self) -> bool {
        self.c_lambda_code && self.c_lambda_methods()
    }

    /// Does a lambda dispatch record the proxy method it enters for the C
    /// table's stack functions ([`Self::c_lambda_frames`], wave 60) and, since
    /// the i54-L1 retirement, for a JDWP session's `ThreadReference.Frames`
    /// (`interpreter::debugger_listing_splices`) -- from the moment the JDWP
    /// server listens, so that a debugger attaching later finds the calls in
    /// progress listed? One plain test, then up to three loads (a C env is
    /// bound, a debugger is attached, the server listens), on a lambda call.
    #[inline(always)]
    pub fn c_lambda_frames_armed(&self) -> bool {
        self.c_lambda_frames
            && self.c_lambda_methods()
            && (self.native_env_present()
                || self.session_attached()
                || self
                    .jdwp_listening
                    .load(std::sync::atomic::Ordering::Acquire))
    }

    /// Does a lambda dispatch that records its proxy frame also post the
    /// proxy method's `MethodEntry` / `MethodExit` ([`Self::c_lambda_events`],
    /// wave 60)? Asked only once [`Self::c_lambda_frames_armed`] holds.
    #[inline]
    pub fn c_lambda_events(&self) -> bool {
        self.c_lambda_events
    }

    /// Does JNI's `GetMethodID` answer a lambda proxy's own method with the
    /// proxy's `jmethodID` ([`Self::jni_lambda_method_ids`], wave 60)?
    #[inline]
    pub fn jni_lambda_method_ids_armed(&self) -> bool {
        self.jni_lambda_method_ids && self.c_lambda_methods() && self.native_env_present()
    }

    /// Does the C JVMTI table answer the primitive classes
    /// ([`Self::c_primitive_classes`], wave 56)?
    #[inline]
    pub fn c_primitive_classes(&self) -> bool {
        self.c_primitive_classes
    }

    /// Does the C JVMTI table model the primordial and the dead phase and
    /// deliver `VMStart` ([`Self::c_jvmti_phases`], wave 55)?
    #[inline]
    pub fn c_jvmti_phases(&self) -> bool {
        self.c_jvmti_phases
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
            & !METHOD_EVENTS_NATIVE_BIND
            != 0
    }

    /// [`Self::method_events_armed`], or the funnel posts `NativeMethodBind`
    /// for the registered natives it runs ([`Self::native_bind_hook_armed`];
    /// interpreter round i3 wave 83, lane L1). The one relaxed load the
    /// native-call funnel and its leaf twin read before their debugger hook,
    /// as they read `method_events_armed` before; false without a debugger
    /// or an agent.
    #[inline(always)]
    pub fn native_hooks_armed(&self) -> bool {
        self.method_events
            .load(std::sync::atomic::Ordering::Relaxed)
            != 0
    }

    /// The bit of [`Self::native_hooks_armed`] for a C env's
    /// `NativeMethodBind` (wave 83, [`METHOD_EVENTS_NATIVE_BIND`]).
    #[inline]
    pub fn native_bind_hook_armed(&self) -> bool {
        self.method_events
            .load(std::sync::atomic::Ordering::Acquire)
            & METHOD_EVENTS_NATIVE_BIND
            != 0
    }

    /// The C JVMTI table's publication of [`Self::native_bind_hook_armed`]
    /// (wave 83; `jvmti::native_env::events_more::refresh_event_gates`).
    pub(crate) fn set_native_bind_hook(&self, armed: bool) {
        self.set_method_events_bit(METHOD_EVENTS_NATIVE_BIND, armed);
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

    /// Is any suspension counted, an ended (ZOMBIE-listed) thread's own
    /// included ([`Self::suspension_counted`])? One load; published with
    /// [`Self::suspension_in_force`], which it implies.
    #[inline(always)]
    pub fn suspension_counted(&self) -> bool {
        self.suspension_counted
            .load(std::sync::atomic::Ordering::Acquire)
    }

    /// Did a C env bind after lambda proxies existed ([`Self::proxy_rows_incomplete`])?
    #[inline]
    pub fn proxy_rows_incomplete(&self) -> bool {
        self.proxy_rows_incomplete
            .load(std::sync::atomic::Ordering::Acquire)
    }

    /// `native_env`'s publication of [`Self::proxy_rows_incomplete`].
    pub(crate) fn set_proxy_rows_incomplete(&self) {
        self.proxy_rows_incomplete
            .store(true, std::sync::atomic::Ordering::Release);
    }

    /// `vm_init`'s publication of [`Self::jdwp_listening`], before it spawns
    /// the JDWP server.
    pub(crate) fn set_jdwp_listening(&self) {
        self.jdwp_listening
            .store(true, std::sync::atomic::Ordering::Release);
    }

    /// A debugger attached (`true`) or detached (`false`); the JDWP server
    /// sets it.
    pub(crate) fn set_session_attached(&self, attached: bool) {
        use std::sync::atomic::Ordering;
        self.session.store(attached, Ordering::Release);
        if attached {
            self.running_native_record
                .fetch_or(RUNNING_NATIVE_SESSION, Ordering::AcqRel);
        } else {
            self.running_native_record
                .fetch_and(!RUNNING_NATIVE_SESSION, Ordering::AcqRel);
        }
    }

    /// Does the native-call funnel record the native a thread runs
    /// (`interpreter::enter_running_native`): a debugger is attached, or a C
    /// JVMTI env is bound with [`Self::c_running_native`] on (wave 61, lane
    /// L6)? One load, the funnel's only test of it.
    #[inline(always)]
    pub fn running_native_armed(&self) -> bool {
        self.running_native_record
            .load(std::sync::atomic::Ordering::Acquire)
            & (RUNNING_NATIVE_SESSION | RUNNING_NATIVE_C_ENV)
            != 0
    }

    /// Does a leaf native take the full native-call funnel
    /// (`vm_exec::safe_native_call_leaf_impl`'s debugger test): whenever
    /// [`Self::running_native_armed`], and also while a C JVMTI env counts
    /// running natives from the excuse word alone
    /// ([`RUNNING_NATIVE_C_LEAF_ONLY`], wave 133, lane L1), since only the
    /// full funnel raises that word. One load, as before.
    #[inline(always)]
    pub fn leaf_takes_full_funnel(&self) -> bool {
        self.running_native_record
            .load(std::sync::atomic::Ordering::Acquire)
            != 0
    }

    /// Does the C JVMTI table's `GetThreadState` count a thread running a
    /// funnel native that stands in for a JDK `native` method as in native
    /// ([`Self::c_running_native`] with a C JVMTI env bound; wave 61)?
    #[inline]
    pub fn c_running_native_armed(&self) -> bool {
        self.c_running_native && self.native_env_present()
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

    /// Does a thread entering a JNI native publish that it runs in native
    /// ([`Self::c_in_native`] with a C JVMTI env bound; wave 58)? One plain
    /// test, then one load.
    #[inline(always)]
    pub fn c_in_native_armed(&self) -> bool {
        self.c_in_native && self.native_env_present()
    }

    /// Does a suspended thread in native stop at its next native -> VM
    /// transition ([`Self::suspend_at_native_transition`], gci i16/sn)?
    #[inline(always)]
    pub fn suspend_at_native_transition(&self) -> bool {
        self.suspend_at_native_transition
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Arm or disarm [`Self::suspend_at_native_transition`] on a built VM
    /// (gci i16/sn): unit tests only.
    #[cfg(test)]
    pub(crate) fn set_suspend_at_native_transition(&self, on: bool) {
        self.suspend_at_native_transition
            .store(on, std::sync::atomic::Ordering::Relaxed);
    }

    /// `native_env`'s publication of [`Self::native_env_present`] (wave 45).
    ///
    /// `excuse_carries_callback`: the VM's native-excuse word records the
    /// callback of every funnel native it is raised for
    /// (`GcBarrier::native_excuse_enabled` and `excused_native_callback`).
    /// With [`Self::c_funnel_record_from_excuse`] on as well, the funnel
    /// keeps no record of its own for the C table (wave 133, lane L1).
    pub(crate) fn set_native_env_present(&self, excuse_carries_callback: bool) {
        self.native_env_present
            .store(true, std::sync::atomic::Ordering::Release);
        if self.c_running_native {
            let bit = if self.c_funnel_record_from_excuse && excuse_carries_callback {
                RUNNING_NATIVE_C_LEAF_ONLY
            } else {
                RUNNING_NATIVE_C_ENV
            };
            self.running_native_record
                .fetch_or(bit, std::sync::atomic::Ordering::AcqRel);
        }
    }

    /// `native_env`'s publication of [`Self::native_class_prepare_armed`].
    pub(crate) fn set_native_class_prepare(&self, armed: bool) {
        self.native_class_prepare
            .store(armed, std::sync::atomic::Ordering::Release);
    }

    /// Does a C JVMTI env listen for `ThreadStart` / `ThreadEnd`
    /// ([`Self::native_thread_events`], wave 52)?
    #[inline]
    pub fn native_thread_events_armed(&self) -> bool {
        self.native_thread_events
            .load(std::sync::atomic::Ordering::Acquire)
    }

    /// Has `main` ended, so that VM thread 0 is the `DestroyJavaVM` thread
    /// on the wire ([`Self::destroy_java_vm_split`], wave 104)? One load.
    #[inline]
    pub fn destroy_java_vm_split(&self) -> bool {
        self.destroy_java_vm_split
            .load(std::sync::atomic::Ordering::Acquire)
    }

    /// Does a C JVMTI env of this VM listen for wave-63 event `event`
    /// ([`Self::native_events`])? One relaxed load.
    #[inline(always)]
    pub fn native_event_armed(&self, event: NativeEvent) -> bool {
        (self.native_events.load(std::sync::atomic::Ordering::Relaxed)
            | self
                .jdwp_native_events
                .load(std::sync::atomic::Ordering::Relaxed))
            & event as u32
            != 0
    }

    /// Does a JDWP monitor event request (43-46) listen (interpreter round
    /// i5 wave 103, lane L1; [`Self::jdwp_native_events`])? One load.
    #[inline]
    pub fn jdwp_monitor_events_armed(&self) -> bool {
        self.jdwp_native_events
            .load(std::sync::atomic::Ordering::Acquire)
            & NativeEvent::Monitor as u32
            != 0
    }

    /// The address of [`Self::native_events`], stable for the VM's life (j27
    /// wave 4, lane CLONEIR): the inline array `clone()` of compiled code
    /// tests `cratonvm_jit::direct_helpers::JVMTI_ALLOC_EVENT_BITS` in it
    /// (`DirectHelperTable::jvmti_alloc_events_word`). Only the C table's
    /// word: the JDWP word ([`Self::jdwp_native_events`]) never carries an
    /// allocation event.
    pub(crate) fn native_events_addr(&self) -> usize {
        &self.native_events as *const std::sync::atomic::AtomicU32 as usize // Cast: a data address
    }

    /// `native_env`'s publication of [`Self::native_event_armed`].
    pub(crate) fn set_native_events(&self, mask: u32) {
        self.native_events
            .store(mask, std::sync::atomic::Ordering::Release);
    }

    /// `native_env`'s publication of [`Self::native_thread_events_armed`].
    pub(crate) fn set_native_thread_events(&self, armed: bool) {
        self.native_thread_events
            .store(armed, std::sync::atomic::Ordering::Release);
    }

    /// Does a C JVMTI env listen for `ClassLoad`
    /// ([`Self::native_class_load`], wave 53)? One load per class defined.
    #[inline]
    pub fn native_class_load_armed(&self) -> bool {
        self.native_class_load
            .load(std::sync::atomic::Ordering::Acquire)
    }

    /// Does a C JVMTI env take `ClassFileLoadHook`
    /// ([`Self::native_class_file_load_hook`], wave 53)? One load per class
    /// the load path offers to the transformer chain.
    #[inline]
    pub fn native_class_file_load_hook_armed(&self) -> bool {
        self.native_class_file_load_hook
            .load(std::sync::atomic::Ordering::Acquire)
    }

    /// `native_env`'s publication of [`Self::native_class_load_armed`] and
    /// [`Self::native_class_file_load_hook_armed`].
    pub(crate) fn set_native_class_hooks(&self, load: bool, file_load_hook: bool) {
        self.native_class_load
            .store(load, std::sync::atomic::Ordering::Release);
        self.native_class_file_load_hook
            .store(file_load_hook, std::sync::atomic::Ordering::Release);
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
        // Interpreter round i4 wave 93 (lane L1): or a class-scoped method
        // event request covers the method's class (one load otherwise).
        // Wave 95 (lane L1): or a class-scoped field watch covers it.
        self.holds_breakpoint(class_id, name, descriptor)
            || self.in_method_event_scope(class_id)
            || self.field_watch_scope.contains(class_id)
    }

    /// Must class `class_id`'s methods run interpreted for a JDWP field watch
    /// served by class (interpreter round i4 wave 95, lane L1;
    /// `CRATONVM_JDWP_SCOPED_FIELD_WATCHES`, [`field_watch_scope`])? One load
    /// for a class outside the scope's bits; the set is empty whenever no
    /// such watch is in force. Asked by the compile doors
    /// (`interpreter::breakpoint_bars_compiling`) beside
    /// [`Self::in_method_event_scope`].
    #[inline]
    pub fn in_field_watch_scope(&self, class_id: u32) -> bool {
        self.field_watch_scope.contains(class_id)
    }

    /// Does a class-scoped JDWP method event request in force concern every
    /// method of class `class_id` (interpreter round i4 wave 93, lane L1;
    /// [`Self::scoped_method_events`])? One load for a class outside the
    /// scope's bits, one more bit test and a read-locked lookup for a class
    /// sharing one. Asked whether or not `breakpoints_active` is set (the
    /// compile doors, `interpreter::breakpoint_bars_compiling`); the set is
    /// empty whenever no such request is in force.
    #[inline]
    pub fn in_method_event_scope(&self, class_id: u32) -> bool {
        let bit = 1u64 << (class_id % 64);
        if self
            .method_event_class_bits
            .load(std::sync::atomic::Ordering::Acquire)
            & bit
            == 0
        {
            return false;
        }
        self.method_event_classes.read().contains(&class_id)
    }

    /// Does this VM serve JDWP `Exception` requests from compiled code when
    /// they allow it ([`Self::exceptions_from_compiled`], wave 93)?
    #[inline]
    pub fn jdwp_exceptions_from_compiled(&self) -> bool {
        self.exceptions_from_compiled
    }

    /// Is a class-scoped method event scope resolved, so a class prepared now
    /// must be matched against it ([`join_method_event_scope`], wave 93)?
    #[inline(always)]
    pub fn method_event_scope_armed(&self) -> bool {
        self.method_event_scope_armed
            .load(std::sync::atomic::Ordering::Acquire)
    }

    /// Add `class_ids` to [`Self::method_event_classes`] (`replace`: they are
    /// the whole set, plus any id of a class defined at or after `carry_from`
    /// already there, which a [`join_method_event_scope`] may have added
    /// while the caller resolved), republishing the bits and bumping the
    /// generations. Answers the ids that were not in the set before (wave 93).
    fn store_method_event_classes(
        &self,
        class_ids: &[u32],
        replace: bool,
        carry_from: usize,
    ) -> Vec<u32> {
        use std::sync::atomic::Ordering;
        let mut set = self.method_event_classes.write();
        let gained: Vec<u32> = class_ids
            .iter()
            .copied()
            .filter(|id| !set.contains(id))
            .collect();
        if replace {
            let carried: Vec<u32> = set
                .iter()
                .copied()
                // Widening: a class id to a store slot index.
                .filter(|&id| id as usize >= carry_from)
                .collect();
            set.clear();
            set.extend(carried);
        }
        set.extend(class_ids.iter().copied());
        let bits = set
            .iter()
            .fold(0u64, |bits, &id| bits | (1u64 << (id % 64)));
        self.method_event_class_bits.store(bits, Ordering::Release);
        drop(set);
        self.method_event_scope_generation
            .fetch_add(1, Ordering::AcqRel);
        self.generation.fetch_add(1, Ordering::Release);
        gained
    }

    /// Forget every class-scoped method event scope (wave 93): no such
    /// request is in force.
    fn clear_method_event_scope(&self) {
        use std::sync::atomic::Ordering;
        self.method_event_scope_armed.store(false, Ordering::Release);
        self.method_event_specs.write().clear();
        self.method_event_classes.write().clear();
        self.method_event_class_bits.store(0, Ordering::Release);
        self.method_event_degraded.store(false, Ordering::Release);
        self.method_event_slots_seen.store(0, Ordering::Release);
        self.method_event_scope_generation
            .fetch_add(1, Ordering::AcqRel);
        self.generation.fetch_add(1, Ordering::Release);
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
    /// Interpreter round i4 wave 93 (lane L1): `interpret_all` takes the
    /// exception requests that need the interpreter
    /// (`exceptions_need_interpreter`), which is `exceptions` unless the VM
    /// serves them from compiled code.
    ///
    /// Wave 95 (lane L1): likewise the field watches that need the
    /// interpreter (`field_watch_needs_interpreter`), which is `field_watch`
    /// unless a class-scoped JDWP watch serves them
    /// ([`field_watch_scope`]).
    fn store_subject_flags(
        &self,
        field_watch: bool,
        field_watch_needs_interpreter: bool,
        exceptions: bool,
        exceptions_need_interpreter: bool,
        jdk_breakpoint: bool,
    ) -> (bool, bool) {
        use std::sync::atomic::Ordering;
        let interpret_all =
            field_watch_needs_interpreter || exceptions_need_interpreter || jdk_breakpoint;
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
        for (slot, (class, name, descriptor)) in
            gates.blocking_standins.iter().zip(BLOCKING_STANDINS)
        {
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
    // Wave 106 (lane L1): before `suspension`, so a reader of that bit never
    // finds this one down.
    gates.suspension_counted.store(
        suspension || ds.any_suspension(),
        std::sync::atomic::Ordering::Release,
    );
    gates
        .suspension
        .store(suspension, std::sync::atomic::Ordering::Release);
    // Interpreter round i3 wave 72, lane L6 (`CRATONVM_PARK_LAZY_SUSPEND_AWARE`
    // with `CRATONVM_PARK_LAZY_STW_WAKE`): a park's running phase re-reads
    // the gate now and, suspended, takes its blocked region. One load when off.
    if suspension && shared.threads.thread_registry.park_lazy_suspend_aware() {
        shared.threads.thread_registry.wake_lazy_park_phases();
    }
    // Wave 54 (lane L6): the waits on the debug state re-check it now (a
    // resume, a suspension, a detach); they take the lock our caller holds
    // once it lets it go.
    gates.notify_state_changed();
    // Wave 24: a method entry or exit request, like a step, concerns every
    // method — only the interpreter's suspend point reports them — and so
    // does a `ThreadReference.Stop` not yet delivered, which the thread
    // throws at its next interpreted bytecode.
    let (method_entries, method_exits) = ds.events.method_events_requested();
    // Interpreter round i4 wave 93 (lane L1, `CRATONVM_JDWP_SCOPED_METHOD_EVENTS`):
    // a method event request confined to classes the scope resolved
    // (`refresh_method_event_scope`) concerns only those classes' methods
    // (`DebuggerGates::in_method_event_scope`), not every method; the others
    // still do. Off: every method event request concerns every method.
    let (wide_entries, wide_exits, scoped_methods) = method_events_by_scope(gates, ds);
    let all_methods = ds.events.has_single_steps()
        || suspension
        || wide_entries
        || wide_exits
        || !ds.pending_stops.is_empty();
    gates.stops.store(
        !ds.pending_stops.is_empty(),
        std::sync::atomic::Ordering::Release,
    );
    // Wave 26: the native-call funnel's pre-filter (wave 27: its JDWP bit;
    // the C JVMTI table owns the other).
    gates.set_method_events_bit(METHOD_EVENTS_JDWP, method_entries || method_exits);
    gates.method_entry.set(method_entries);
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
    // Interpreter round i2 wave 63 (lane L1): or a C JVMTI env's field
    // watch, whose events the same field bytecodes post
    // (`jvmti::native_env::post_field_watch`).
    // Interpreter round i5 wave 103 (lane L1): a JDWP monitor event request
    // arms the monitor paths' event hook (`NativeEvent::Monitor`).
    let jdwp_monitor = ds.events.has_requests(EventKind::MonitorContendedEnter)
        || ds.events.has_requests(EventKind::MonitorContendedEntered)
        || ds.events.has_requests(EventKind::MonitorWait)
        || ds.events.has_requests(EventKind::MonitorWaited);
    gates.jdwp_native_events.store(
        if jdwp_monitor {
            NativeEvent::Monitor as u32
        } else {
            0
        },
        std::sync::atomic::Ordering::Release,
    );
    let jdwp_field_watch = ds.events.has_requests(EventKind::FieldAccess)
        || ds.events.has_requests(EventKind::FieldModification);
    let native_field_watch = gates.native_event_armed(NativeEvent::FieldWatch);
    let field_watch = jdwp_field_watch || native_field_watch;
    // Interpreter round i4 wave 95 (lane L1, `CRATONVM_JDWP_SCOPED_FIELD_WATCHES`):
    // JDWP watches whose fields the scope resolved, with no class of it
    // degrading it, keep only the classes that name a watched field
    // interpreted (`DebuggerGates::concerns_method`); a C JVMTI env's watch,
    // or any other JDWP watch, still keeps every method interpreted. The
    // field bytecodes' hook stays armed (`field_watch`) either way.
    let scoped_field_watch = jdwp_field_watch
        && gates.field_watch_scope.enabled()
        && gates
            .field_watch_scope
            .serves(&ds.events.watched_field_ids());
    let field_watch_needs_interpreter =
        native_field_watch || (jdwp_field_watch && !scoped_field_watch);
    let field_watch_scope_grew = gates.field_watch_scope.take_grew();
    let exceptions = ds.events.has_requests(EventKind::Exception);
    // Interpreter round i4 wave 93 (lane L1, `CRATONVM_JVMTI_EXCEPTION_FROM_COMPILED`):
    // a VM that serves `Exception` from compiled code keeps every method
    // interpreted only for a request compiled code cannot serve yet (one that
    // suspends, or filters by instance); the unwinder's hook stays armed
    // (`exceptions`) either way.
    let exceptions_need_interpreter = exceptions
        && (!gates.jdwp_exceptions_from_compiled()
            || ds.events.exception_requests_need_the_interpreter());
    gates
        .compiled_exception_requests
        .set(exceptions && !exceptions_need_interpreter);
    // Interpreter round i2 wave 63 (lane L1): a C JVMTI env's
    // `SampledObjectAlloc` samples in the interpreter's allocation funnels,
    // which compiled code's inline allocations bypass: every method runs
    // interpreted while it is enabled, as for a field watch.
    let sampling = gates.native_event_armed(NativeEvent::SampledAlloc);
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
    let active = all_methods
        || scoped_methods
        || !methods.is_empty()
        || field_watch
        || exceptions
        || sampling;
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
    let (was_watching, interpret_all_rose) = gates.store_subject_flags(
        field_watch,
        field_watch_needs_interpreter,
        exceptions,
        exceptions_need_interpreter,
        jdk_breakpoint || sampling,
    );
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
    // Interpreter round i2 wave 62 (lane L7): a dispatch loop that hoists the
    // observer gates (`VmConfig::interp_hoisted_observers`) re-reads this one,
    // and the per-method summary stored above, at its next top: send every
    // loop of this VM there, after the stores.
    // Wave 81 (lane L1): `CRATONVM_JVMTI_STEP_POLL` re-reads the same gates
    // there (a JDWP step request armed from a callback on this thread).
    if shared.config.interp_hoisted_observers || shared.config.jvmti_step_poll {
        shared.mem.gc_barrier.note_code_moved_on_every_loop();
    }
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
        // Wave 95 (lane L1): a class-scoped field watch withdraws only its
        // classes' dependents (`field_watch_scope::refresh`).
        ds.events.has_single_steps()
            || wide_entries
            || wide_exits
            || field_watch_needs_interpreter
            || sampling,
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
    // Wave 93 (lane L1): or the classes a scoped method event request
    // concerns grew (`refresh_method_event_scope` withdrew their dependents).
    let scope_generation = gates
        .method_event_scope_generation
        .load(std::sync::atomic::Ordering::Acquire);
    let scope_grew = gates
        .method_event_scope_published
        .swap(scope_generation, std::sync::atomic::Ordering::AcqRel)
        != scope_generation;
    let concerned_grew = !was_active
        || every_method_rose
        || interpret_all_rose
        || methods_changed
        || (scoped_methods && scope_grew)
        // Wave 95 (lane L1): or the classes a scoped field watch concerns.
        || (scoped_field_watch && field_watch_scope_grew);
    if active && concerned_grew {
        crate::runtime::interpreter::request_compiled_loop_exits(shared);
    }
}

/// `(entries, exits, scoped)` of the method event requests in force for
/// [`publish_debugger_gates`] (interpreter round i4 wave 93, lane L1): with
/// [`DebuggerGates::scoped_method_events`], a request whose class scope the
/// last [`refresh_method_event_scope`] resolved, while no class of it
/// degraded the scope, is `scoped`; every other one concerns every method.
/// Without the switch: [`events::EventManager::method_events_requested`] and
/// nothing scoped. Takes the resolved scopes' read lock for a compare (a
/// leaf: its writer holds no other lock while it writes).
fn method_events_by_scope(gates: &DebuggerGates, ds: &DebugState) -> (bool, bool, bool) {
    if !gates.scoped_method_events {
        let (entries, exits) = ds.events.method_events_requested();
        return (entries, exits, false);
    }
    // Interpreter round i4 wave 94 (lane L1): what this publication saw, for
    // the server's poll (`DebuggerGates::method_event_degraded_published`).
    let degraded = gates
        .method_event_degraded
        .load(std::sync::atomic::Ordering::Acquire);
    gates
        .method_event_degraded_published
        .store(degraded, std::sync::atomic::Ordering::Release);
    if degraded {
        let (entries, exits) = ds.events.method_events_requested();
        return (entries, exits, false);
    }
    let resolved = gates.method_event_specs.read();
    ds.events
        .method_events_split(&|scope| resolved.contains(scope))
}

/// Does `class` fall in one of `scopes` (interpreter round i4 wave 93, lane
/// L1)? `Below`: the class or a subclass of it; `Named`: its dotted name.
fn method_event_scope_matches(
    cm: &crate::classloading::ClassManager,
    class: &crate::classloading::Class,
    scopes: &[events::MethodEventScope],
) -> bool {
    scopes.iter().any(|scope| match scope {
        events::MethodEventScope::Below(id) => u32::try_from(*id)
            .is_ok_and(|raw| cm.is_subclass_of(class.id, crate::classloading::ClassId::new(raw))),
        events::MethodEventScope::Named(pattern) => {
            events::class_pattern_matches(pattern, &class.name)
        }
    })
}

/// Must a class in a method event scope take every method of the VM with it
/// (interpreter round i4 wave 93, lane L1; `DebuggerGates::method_event_degraded`)?
/// A JDK class, whose methods the JIT may expand at a call site without a
/// record the scoped withdrawal reads
/// (`interpreter::jit_may_expand_a_method_of_without_a_record`), or a class
/// with a native method, whose events the native-call funnel reports only
/// for an interpreted caller.
fn method_event_class_degrades(class: &crate::classloading::Class) -> bool {
    crate::runtime::interpreter::jit_may_expand_a_method_of_without_a_record(&class.name)
        || class.methods.iter().any(|m| m.is_native())
}

/// Resolve the class scopes of the JDWP method event requests in force to the
/// classes they concern (interpreter round i4 wave 93, lane L1;
/// `CRATONVM_JDWP_SCOPED_METHOD_EVENTS`, page
/// `docs/feature-designs/jvmti-delivery-threading.md`).
/// Called by the JDWP server, holding no lock, at each poll and before it
/// publishes the gates after a command; nothing with the switch off.
///
/// When the scopes changed, every class of the store is matched and the
/// result replaces the set; otherwise only the classes defined since the
/// last call are. The new scopes are published BEFORE the class store's slot
/// count is read, so a class defined after that read is prepared after it
/// and joins at its preparation ([`join_method_event_scope`]); one a join
/// added while this call resolved is carried over. On the edge where a class
/// joins, the compiled code that would run its methods without the
/// interpreter -- its own bodies and every body that inlined one of its
/// methods -- is withdrawn (`interpreter::note_breakpoint_classes_gained`,
/// the breakpoint's scoped withdrawal); the other classes stay compiled.
///
/// Lock order: the class manager is read with no debug-state lock held; the
/// set's and the scopes' locks are leaves.
///
/// Answers whether the scope changed (its scopes, its classes or its
/// degradation), so a caller no publication follows publishes the gates
/// ([`refresh_method_event_scope_and_publish`]).
pub(crate) fn refresh_method_event_scope(shared: &crate::vm::SharedVm) -> bool {
    use std::sync::atomic::Ordering;
    let gates = &shared.debug.debugger_gates;
    if !gates.scoped_method_events {
        return false;
    }
    let wanted = shared.debug.debug_state.lock().events.method_event_scopes();
    let changed = *gates.method_event_specs.read() != wanted;
    if wanted.is_empty() {
        if changed || gates.method_event_scope_armed() {
            gates.clear_method_event_scope();
            return true;
        }
        return false;
    }
    if changed {
        // Interpreter round i4 wave 94 (lane L1): the degradation restarts
        // with the new scopes, under the scopes' write lock, so a join that
        // matched the new scopes and degraded them (it swaps the flag while
        // holding the read lock, `join_method_event_scope`) is never undone
        // by this refresh's own verdict, which is OR-ed in below. Wave 93
        // stored the verdict over it.
        let mut specs = gates.method_event_specs.write();
        *specs = wanted.clone();
        gates.method_event_degraded.store(false, Ordering::Release);
    }
    gates.method_event_scope_armed.store(true, Ordering::SeqCst);
    let from = if changed {
        0
    } else {
        gates.method_event_slots_seen.load(Ordering::Acquire)
    };
    let (matched, degraded, slots) = {
        let cm = shared.classes.class_manager.read();
        let slots = cm.class_store.slot_count();
        if !changed && slots <= from {
            // Wave 94: a join that degraded the scope where it could not
            // publish left the publication behind.
            return gates.method_event_degraded.load(Ordering::Acquire)
                != gates.method_event_degraded_published.load(Ordering::Acquire);
        }
        // A `ClassOnly` naming an interface (its implementors), a JDK class or
        // a class the store does not hold (a lambda proxy's id) is not
        // scoped in stage 1.
        let mut degraded = changed
            && wanted.iter().any(|scope| match scope {
                events::MethodEventScope::Below(id) => !u32::try_from(*id).is_ok_and(|raw| {
                    cm.class_store
                        .get(crate::classloading::ClassId::new(raw))
                        .is_some_and(|c| {
                            !c.is_interface()
                                && !crate::runtime::interpreter::jit_may_expand_a_method_of_without_a_record(&c.name)
                        })
                }),
                events::MethodEventScope::Named(_) => false,
            });
        let mut matched: Vec<(u32, String)> = Vec::new();
        for slot in from..slots {
            let Ok(raw) = u32::try_from(slot) else {
                break;
            };
            let Some(class) = cm.class_store.get(crate::classloading::ClassId::new(raw)) else {
                continue;
            };
            if method_event_scope_matches(&cm, class, &wanted) {
                degraded |= method_event_class_degrades(class);
                matched.push((raw, class.name.to_string()));
            }
        }
        (matched, degraded, slots)
    };
    let ids: Vec<u32> = matched.iter().map(|&(id, _)| id).collect();
    let gained = gates.store_method_event_classes(&ids, changed, slots);
    gates.method_event_slots_seen.store(slots, Ordering::Release);
    if degraded {
        gates.method_event_degraded.store(true, Ordering::Release);
    }
    let degraded = gates.method_event_degraded.load(Ordering::Acquire);
    if crate::runtime::env_cache::dbg_jitc() && (changed || !gained.is_empty()) {
        eprintln!(
            "[cratonvm-jitc] method event scope: scopes={} classes={} gained={} degraded={degraded}",
            wanted.len(),
            gates.method_event_classes.read().len(),
            gained.len(),
        );
    }
    // Wave 94: measured against what the last publication saw, not against
    // the flag at entry, which a join may already have moved.
    let moved = changed
        || !gained.is_empty()
        || degraded != gates.method_event_degraded_published.load(Ordering::Acquire);
    if degraded || gained.is_empty() {
        // Degraded: the publication that follows withdraws every body.
        return moved;
    }
    let classes: Vec<(crate::classloading::ClassId, Option<String>)> = matched
        .into_iter()
        .filter(|(id, _)| gained.contains(id))
        .map(|(id, name)| (crate::classloading::ClassId::new(id), Some(name)))
        .collect();
    crate::runtime::interpreter::note_breakpoint_classes_gained(shared, &classes);
    moved
}

/// [`refresh_method_event_scope`], and publish the gates when the scope
/// changed (interpreter round i4 wave 93, lane L1): for the JDWP server's
/// poll and its detach, after which no other publication follows (a spent
/// `Count`, or the classes defined since the last poll, may change it).
///
/// Wave 95 (lane L1): the field watches' class scope too
/// ([`field_watch_scope::refresh`], `CRATONVM_JDWP_SCOPED_FIELD_WATCHES`).
pub(crate) fn refresh_method_event_scope_and_publish(shared: &crate::vm::SharedVm) {
    let methods_moved = refresh_method_event_scope(shared);
    let watches_moved = field_watch_scope::refresh(shared);
    if methods_moved || watches_moved {
        let ds = shared.debug.debug_state.lock();
        publish_debugger_gates(shared, &ds);
    }
}

/// Class `class_id` of this VM was just prepared while a class-scoped method
/// event scope is resolved (interpreter round i4 wave 93, lane L1;
/// [`DebuggerGates::method_event_scope_armed`]): if a scope names it, it
/// joins the scope before any of its code runs, so nothing of it is compiled
/// while the request stands. A class that degrades the scope
/// ([`method_event_class_degrades`]) republishes the gates, which then keep
/// every method interpreted. Called from [`class_prepared_on_thread`] with no
/// lock held.
fn join_method_event_scope(shared: &crate::vm::SharedVm, class_id: crate::classloading::ClassId) {
    let gates = &shared.debug.debugger_gates;
    let scopes = gates.method_event_specs.read().clone();
    if scopes.is_empty() {
        return;
    }
    let degrades = {
        let cm = shared.classes.class_manager.read();
        let Some(class) = cm.class_store.get(class_id) else {
            return;
        };
        if !method_event_scope_matches(&cm, class, &scopes) {
            return;
        }
        method_event_class_degrades(class)
    };
    if gates.in_method_event_scope(class_id.as_u32()) {
        return;
    }
    // Interpreter round i4 wave 94 (lane L1): store only while the scopes
    // matched above are still the resolved ones, holding their read lock
    // across the store (the set's lock is taken inside it; nothing takes
    // the two the other way round). Wave 93 stored unconditionally: a join
    // racing a `clear_method_event_scope` (the last scoped request cleared,
    // or a detach) re-added its class after the clear, and that class then
    // stayed out of every compile (`breakpoint_bars_compiling`) with no
    // request in force until another scoped request came. A mismatch means a
    // refresh published other scopes after this class was defined, and that
    // refresh's walk of the store covers it.
    let fresh_degradation = {
        let resolved = gates.method_event_specs.read();
        if *resolved != scopes {
            return;
        }
        // `carry_from` past every slot: nothing is replaced.
        let _ = gates.store_method_event_classes(&[class_id.as_u32()], false, usize::MAX);
        degrades
            && !gates
                .method_event_degraded
                .swap(true, std::sync::atomic::Ordering::AcqRel)
    };
    if crate::runtime::env_cache::dbg_jitc() {
        eprintln!(
            "[cratonvm-jitc] method event scope: class id={} joined at its preparation degrades={degrades}",
            class_id.as_u32(),
        );
    }
    // Not on the server or a thread running its heap work, which may hold
    // the debug-state lock: the server's next poll publishes
    // (`DebuggerGates::method_event_degraded_published`, wave 94).
    if fresh_degradation && !debugger_hooks_suppressed() {
        let ds = shared.debug.debug_state.lock();
        publish_debugger_gates(shared, &ds);
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
    // Wave 117 (`CRATONVM_JDWP_JDK_BREAKPOINT_EXPANSION_SET`): narrowed to the
    // JDK classes the JIT can expand ([`jit_expansion_set`]) with the switch.
    let gates = &shared.debug.debugger_gates;
    let expanded = |name: &str| gates.breakpoint_may_be_expanded(name);
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
///
/// Interpreter round i4 wave 93 (lane L1): two counts in one word, so the
/// per-VM-state ratchet's static count does not grow. The low 32 bits
/// ([`METHOD_ENTRY_COUNT`]) count the VMs above; the high 32 bits
/// ([`COMPILED_EXCEPTION_COUNT`]) count the VMs with a JDWP `Exception`
/// request they serve from compiled code
/// (`DebuggerGates::compiled_exception_requests`), the pre-filter of the
/// compiled `athrow` helper ([`compiled_exception_requests_anywhere`]).
static METHOD_ENTRY_REQUEST_VMS: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

/// [`METHOD_ENTRY_REQUEST_VMS`]'s method-entry count: one unit, and its mask.
const METHOD_ENTRY_COUNT: (u64, u64) = (1, 0xFFFF_FFFF_u64);
/// [`METHOD_ENTRY_REQUEST_VMS`]'s compiled-exception count (wave 93).
const COMPILED_EXCEPTION_COUNT: (u64, u64) = (1_u64 << 32, 0xFFFF_FFFF_u64 << 32);

/// Is a JDWP `MethodEntry` request in force in any VM of this process?
#[cfg_attr(not(feature = "experimental-debug"), allow(dead_code))]
#[inline(always)]
pub(crate) fn method_entry_requests_anywhere() -> bool {
    METHOD_ENTRY_REQUEST_VMS.load(std::sync::atomic::Ordering::Relaxed) & METHOD_ENTRY_COUNT.1 != 0
}

/// Does any VM of this process serve a JDWP `Exception` request from
/// compiled code (interpreter round i4 wave 93, lane L1)? The pre-filter the
/// compiled `athrow` helper reads before it records its throw site
/// (`interpreter::compiled_throw_sites_wanted`); one relaxed load.
#[inline(always)]
pub(crate) fn compiled_exception_requests_anywhere() -> bool {
    METHOD_ENTRY_REQUEST_VMS.load(std::sync::atomic::Ordering::Relaxed)
        & COMPILED_EXCEPTION_COUNT.1
        != 0
}

/// One of [`METHOD_ENTRY_REQUEST_VMS`]'s counts (`(unit, mask)`) moved by
/// one VM: up when `armed`, down (never below zero) otherwise.
fn note_request_vms(count: (u64, u64), armed: bool) {
    use std::sync::atomic::Ordering;
    let (unit, mask) = count;
    let _ = METHOD_ENTRY_REQUEST_VMS.try_update(Ordering::AcqRel, Ordering::Acquire, |n| {
        let own = n & mask;
        let rest = n & !mask;
        let own = if armed {
            own.wrapping_add(unit) & mask
        } else if own == 0 {
            0
        } else {
            own - unit
        };
        Some(rest | own)
    });
}

/// One VM's share of one of [`METHOD_ENTRY_REQUEST_VMS`]'s counts
/// (interpreter round i4 wave 95, lane L1): whether this VM holds a unit of
/// it. [`Self::set`] moves the count only on this VM's edges, so it stays
/// balanced per VM; dropping the VM's gates with the unit held (an embedder
/// dropping a VM while a debugger's request stands) gives it back, so the
/// process-wide pre-filter does not stay up for a VM that is gone.
///
/// The `MethodEntry` count (`DebuggerGates::method_entry`, wave 24) and the
/// compiled-exception count (`DebuggerGates::compiled_exception_requests`,
/// wave 93) each hold one. A field type with `Drop`, not a `Drop` for
/// `DebuggerGates`, so `DebuggerGates { .., ..DebuggerGates::new() }` stays
/// legal (a temporary's unused edge is unarmed and gives nothing back).
pub(crate) struct RequestVmsEdge {
    armed: std::sync::atomic::AtomicBool,
    count: (u64, u64),
}

impl RequestVmsEdge {
    /// An unarmed share of `count` (`(unit, mask)`).
    const fn new(count: (u64, u64)) -> Self {
        Self {
            armed: std::sync::atomic::AtomicBool::new(false),
            count,
        }
    }

    /// This VM's requests of the kind came into force (`armed`) or all went;
    /// only [`publish_debugger_gates`] calls this (under the debug-state
    /// lock). Moves the count on an edge only.
    fn set(&self, armed: bool) {
        let was = self
            .armed
            .swap(armed, std::sync::atomic::Ordering::AcqRel);
        if was != armed {
            note_request_vms(self.count, armed);
        }
    }
}

impl Drop for RequestVmsEdge {
    fn drop(&mut self) {
        if *self.armed.get_mut() {
            note_request_vms(self.count, false);
        }
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

/// The calling thread pushed a frame at `depth` that was NOT entered there (a
/// frame rebuilt from a compiled activation, `interpreter::push_resumed_frame`;
/// interpreter round i4 wave 93, lane L1): every record at or above `depth`
/// belongs to a frame that is gone. While a class-scoped method event request
/// keeps only some methods interpreted, an unconcerned frame never reaches
/// the suspend point that would consume its record, and the record outlived
/// it until the next push at its depth: a resumed frame of a concerned method
/// branching back to bytecode 0 then reported a `MethodEntry`.
#[cfg_attr(not(feature = "experimental-debug"), allow(dead_code))]
pub(crate) fn note_frame_resumed(depth: usize) {
    ENTERED_FRAMES.with(|frames| {
        if let Ok(mut frames) = frames.try_borrow_mut() {
            while frames.last().is_some_and(|&d| d >= depth) {
                frames.pop();
            }
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
// `-agentlib:jdwp` (interpreter round i4 wave 85, lane L1)
// ---------------------------------------------------------------------------

/// The socket transport's text for an I/O error, as `dbgsysGetLastIOError`
/// words it: the POSIX `strerror` text for the common socket errors on
/// every OS (MEASURED, Temurin 25.0.3 on Windows: a refused connect is
/// `connect failed: Connection refused`, not Winsock's message), else the
/// OS's message without Rust's ` (os error N)` suffix.
fn transport_error_text(error: &std::io::Error) -> String {
    use std::io::ErrorKind;
    let posix = match error.kind() {
        ErrorKind::ConnectionRefused => Some("Connection refused"),
        ErrorKind::AddrInUse => Some("Address already in use"),
        ErrorKind::AddrNotAvailable => Some("Cannot assign requested address"),
        ErrorKind::TimedOut => Some("Connection timed out"),
        ErrorKind::ConnectionReset => Some("Connection reset by peer"),
        ErrorKind::PermissionDenied => Some("Permission denied"),
        ErrorKind::HostUnreachable => Some("No route to host"),
        ErrorKind::NetworkUnreachable => Some("Network is unreachable"),
        _ => None,
    };
    if let Some(text) = posix {
        return text.to_string();
    }
    let text = error.to_string();
    match text.rfind(" (os error ") {
        Some(at) => text[..at].to_string(),
        None => text,
    }
}

/// The peers a `server=y` transport with `allow=` accepts (interpreter round
/// i7 wave 140, lane L1; [`parse_jdwp_allow`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum JdwpAllow {
    /// `allow=*`.
    All,
    /// IPv6 addresses (an IPv4 one as its IPv4-mapped form) and prefix
    /// lengths over the 128 bits.
    Peers(Vec<(u128, u8)>),
}

/// The socket transport's parse of `allow=<spec>` (`parseAllowedPeers`),
/// MEASURED on HotSpot 25.0.3 (`tools/probes/interp/L1/L1W140JdwpAllow.java`):
/// `*` alone accepts every peer, and a `*` with anything else is
/// `allow option '*' cannot be expanded` (the whole value shown); otherwise
/// `+`-separated entries (`;` is no separator), each a numeric IPv4
/// (dotted quad only: `127.1` is refused) or IPv6 address with an optional
/// `/prefix` of decimal digits, at most 32 for IPv4 and 128 for IPv6. The
/// error is `(shown, message)`: the text the `Error in allow option: '..'`
/// line quotes (the entry's address, or its prefix) and the transport
/// error's message. Pure.
pub(crate) fn parse_jdwp_allow(spec: &str) -> Result<JdwpAllow, (String, &'static str)> {
    if spec == "*" {
        return Ok(JdwpAllow::All);
    }
    if spec.contains('*') {
        return Err((spec.to_string(), "allow option '*' cannot be expanded"));
    }
    let mut peers = Vec::new();
    for entry in spec.split('+') {
        let (address, prefix) = match entry.split_once('/') {
            Some((address, prefix)) => (address, Some(prefix)),
            None => (entry, None),
        };
        let invalid_address = || (address.to_string(), "invalid IP address in allow option");
        let (bits, v4) = if let Ok(v4) = address.parse::<std::net::Ipv4Addr>() {
            (u128::from(v4.to_ipv6_mapped()), true)
        } else if let Ok(v6) = address.parse::<std::net::Ipv6Addr>() {
            (u128::from(v6), false)
        } else {
            return Err(invalid_address());
        };
        let max: u32 = if v4 { 32 } else { 128 };
        let length = match prefix {
            None => max,
            Some(text) => {
                let invalid_mask = || (text.to_string(), "invalid netmask in allow option");
                if text.is_empty() || !text.bytes().all(|b| b.is_ascii_digit()) {
                    return Err(invalid_mask());
                }
                match text.parse::<u32>() {
                    Ok(n) if n <= max => n,
                    _ => return Err(invalid_mask()),
                }
            }
        };
        // An IPv4 prefix counts from the mapped form's 96 leading bits.
        let length = if v4 { length + 96 } else { length };
        // Truncation: at most 128.
        peers.push((bits, length as u8));
    }
    Ok(JdwpAllow::Peers(peers))
}

/// Does `allow` accept a debugger connecting from `peer`? An IPv4 peer is
/// compared in its IPv4-mapped form, as the transport's dual-stack socket
/// sees it. Pure.
pub(crate) fn jdwp_allow_admits(allow: &JdwpAllow, peer: std::net::IpAddr) -> bool {
    let bits = match peer {
        std::net::IpAddr::V4(v4) => u128::from(v4.to_ipv6_mapped()),
        std::net::IpAddr::V6(v6) => u128::from(v6),
    };
    match allow {
        JdwpAllow::All => true,
        JdwpAllow::Peers(peers) => peers.iter().any(|&(address, length)| {
            let mask = if length == 0 {
                0
            } else {
                u128::MAX << (128 - u32::from(length.min(128)))
            };
            bits & mask == address & mask
        }),
    }
}

/// A refused peer as the transport's `Peer not allowed to connect` line
/// names it: an IPv4 peer of a socket bound to every interface (`address=*`,
/// HotSpot's dual-stack socket) in its IPv4-mapped form, as MEASURED
/// (`::ffff:127.0.0.1`), else as it connected (`127.0.0.1` on a socket
/// bound to the loopback address). Pure.
pub(crate) fn jdwp_peer_text(peer: std::net::IpAddr, every_interface: bool) -> String {
    match peer {
        std::net::IpAddr::V4(v4) if every_interface => format!("::ffff:{v4}"),
        other => other.to_string(),
    }
}

/// Where the JDK back end's `transport.c` names its `could not connect`
/// exit (wave 140, lane L1): MEASURED on Windows (Temurin 25.0.3); elsewhere
/// the source-relative path JDK builds use there (not measured, as for
/// `config::JDWP_NO_TRANSPORTS_AT`).
#[cfg(windows)]
const JDWP_ACCEPT_TIMEOUT_AT: &str = "s\\src\\jdk.jdwp.agent\\share\\native\\libjdwp\\transport.c:368";
/// See the Windows definition above.
#[cfg(not(windows))]
const JDWP_ACCEPT_TIMEOUT_AT: &str = "src/jdk.jdwp.agent/share/native/libjdwp/transport.c:368";

/// One accept of the agent's listener (wave 140, lane L1): blocking without
/// a `deadline`; with one, polled until it passes (`None` then). The stream
/// handed on blocks, whatever the listener's mode was.
fn accept_until(
    listener: &std::net::TcpListener,
    deadline: Option<std::time::Instant>,
) -> Option<std::io::Result<std::net::TcpStream>> {
    let Some(deadline) = deadline else {
        return Some(listener.accept().map(|(stream, _)| stream));
    };
    if let Err(e) = listener.set_nonblocking(true) {
        return Some(Err(e));
    }
    let answer = loop {
        match listener.accept() {
            Ok((stream, _)) => break Some(stream.set_nonblocking(false).map(|()| stream)),
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                if std::time::Instant::now() >= deadline {
                    break None;
                }
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            Err(e) => break Some(Err(e)),
        }
    };
    let _ = listener.set_nonblocking(false);
    answer
}

/// No debugger connected within `timeout=` (wave 140, lane L1): the back
/// end's two lines on stderr and the process ends with status 0, as
/// HotSpot 25.0.3 ends it (MEASURED: `ERROR: transport error 203: timed out
/// waiting for connection`, then `JDWP exit error JVMTI_ERROR_NONE(0): could
/// not connect, timeout or fatal error [...transport.c:368]`, exit status 0).
fn exit_on_accept_timeout(shared: &crate::vm::SharedVm) -> ! {
    use std::io::Write as _;
    // As `exit_for_debugger`: the VM's buffered streams first.
    let _ = shared.natives.fd_table.flush(1);
    let _ = shared.natives.fd_table.flush(2);
    let _ = std::io::stdout().flush();
    {
        let mut err = std::io::stderr().lock();
        let _ = writeln!(err, "ERROR: transport error 203: timed out waiting for connection");
        let _ = writeln!(
            err,
            "JDWP exit error JVMTI_ERROR_NONE(0): could not connect, timeout or fatal error [{JDWP_ACCEPT_TIMEOUT_AT}]"
        );
        let _ = err.flush();
    }
    cratonvm_native_api::process_exit::exit_process(0)
}

/// Start the JDWP server `-agentlib:jdwp` asked for (`VmConfig::jdwp_agent`)
/// where the JDK back end's `initialize` runs, at `VMInit` (interpreter
/// round i4 wave 85, lane L1). The launcher calls it on the main thread
/// (`thread`) after the VM's boot and before the startup Java agents and
/// the main class:
///
/// * `server=y`: bind `address` (the loopback interface without a host, as
///   JDK 9+ does; every interface for `*`; an ephemeral port without one),
///   print `Listening for transport dt_socket at address: <port>` on stdout
///   (`TTY_MESSAGE`; not with `quiet=y`), and serve each connection in turn,
///   accepting again after a detach;
/// * `server=n`: connect to `address` (within `timeout=` when given) and
///   serve that one connection;
/// * `suspend=y`: wait for the debugger's session, send `VM_START` (request
///   0, this thread, `SUSPEND_ALL`) with the VM suspended, and hold this
///   thread until the debugger resumes it; `suspend=n` with a connection made
///   here (`server=n`) sends `VM_START` with `SUSPEND_NONE`, and a listener
///   (`server=y`) sends none, the debugger attaching later.
///
/// A transport that does not start is the back end's `ERROR:` lines and exit
/// status 2 ([`crate::config::JdwpAgentFailure::transport_init`]), for the
/// launcher to print. `Ok` with nothing to do without the option.
pub fn start_jdwp_agent(
    shared: &Arc<crate::vm::SharedVm>,
    thread: &mut crate::threading::jvm_thread::JvmThread,
) -> Result<(), crate::config::JdwpAgentFailure> {
    let Some(agent) = shared.config.jdwp_agent.clone() else {
        return Ok(());
    };
    // Interpreter round i6 wave 130 (lane L1b,
    // `CRATONVM_JDWP_AGENT_INCLUDE_VIRTUAL_THREADS`, default ON, `=0` drops
    // the option as before): `includevirtualthreads=y` lists the virtual
    // threads in `AllThreads` (`commands::listed_thread_ids`), as the back
    // end's `gdata->includeVThreads`. Set before the server starts; `n`
    // leaves `CRATONVM_JDWP_INCLUDE_VIRTUAL_THREADS` in charge. Page:
    // docs/feature-designs/jvmti-delivery-threading.md.
    if agent.include_virtual_threads
        && cratonvm_types::flags::runtime_flag_default_on(
            "CRATONVM_JDWP_AGENT_INCLUDE_VIRTUAL_THREADS",
        )
    {
        shared.debug.debug_state.lock().include_virtual_threads = true;
    }
    // Interpreter round i7 wave 144, lane L1 (`CRATONVM_JDWP_AGENT_LAUNCH`,
    // parity, default ON; `=0` makes the launcher refuse the options):
    // `onthrow=` / `onuncaught=y` defer the back end's initialization, the
    // transport included, to the first throw that matches
    // ([`start_deferred_jdwp_agent`]), as `debugInit.c` defers it
    // (`initOnStartup` false). Nothing listens and nothing is printed until
    // then. Page:
    // docs/feature-designs/jvmti-delivery-threading.md.
    if agent.on_throw.is_some() || agent.on_uncaught {
        shared.debug.debugger_gates.arm_jdwp_deferred_start();
        return Ok(());
    }
    start_jdwp_transport(shared, thread, agent)
}

/// The deferred start's trigger (interpreter round i7 wave 144, lane L1):
/// exception `exc_class` is thrown on `thread`, `uncaught` when no frame on
/// the thread catches it. When this VM's `-agentlib:jdwp` deferred its start
/// (`onthrow=` / `onuncaught=y`) and the throw matches -- an exception of
/// exactly the `onthrow` class (the back end compares the class's signature,
/// so a subclass does not match), or any uncaught one with `onuncaught=y` --
/// the transport starts now, on the throwing thread, once: it listens (or
/// connects), runs `launch=`, and with `suspend=y` holds this thread until
/// the debugger resumes it, as the back end's `initialize(.., EI_EXCEPTION)`
/// does from its early `Exception` callback. A transport that fails ends the
/// process with the back end's lines, as at start. Answers whether the start
/// ran (the caller re-reads what a collection in the hold may have moved).
pub(crate) fn start_deferred_jdwp_agent(
    shared: &crate::vm::SharedVm,
    thread: &mut crate::threading::jvm_thread::JvmThread,
    exc_class: crate::classloading::ClassId,
    uncaught: bool,
) -> bool {
    let Some(agent) = shared.config.jdwp_agent.clone() else {
        return false;
    };
    let matches = (agent.on_uncaught && uncaught)
        || agent.on_throw.as_deref().is_some_and(|wanted| {
            let cm = shared.classes.class_manager.read();
            cm.get_class(exc_class)
                .is_some_and(|class| class.name.replace('/', ".") == wanted)
        });
    if !matches || !shared.debug.debugger_gates.take_jdwp_deferred_start() {
        return false;
    }
    let Some(owned) = shared.self_arc.read().as_ref().and_then(|w| w.upgrade()) else {
        return false;
    };
    // What the program printed before the throw goes out before the
    // transport's `Listening` line (HotSpot's streams are unbuffered).
    let _ = shared.natives.fd_table.flush(1);
    let _ = shared.natives.fd_table.flush(2);
    if let Err(failure) = start_jdwp_transport(&owned, thread, agent) {
        use std::io::Write as _;
        let _ = shared.natives.fd_table.flush(1);
        let _ = std::io::stdout().flush();
        let mut err = std::io::stderr().lock();
        for line in &failure.stderr_lines {
            let _ = writeln!(err, "{line}");
        }
        let _ = err.flush();
        drop(err);
        cratonvm_native_api::process_exit::exit_process(failure.exit_code);
    }
    true
}

/// `launch=<command>` (interpreter round i7 wave 144, lane L1): run
/// `<command> <transport> <address>` once the transport is up, as the back
/// end's `launch()` builds it and `dbgsysExec` runs it: on Windows the whole
/// line goes to `CreateProcess` (the first word names the program, the rest
/// is its command line as written, no shell), elsewhere it is split at
/// whitespace and `execvp`'d. The child inherits the VM's standard streams
/// and is not waited for. A command that cannot be started ends the start
/// with exit status 1 (PREDICTED text: `ERROR: JDWP unable to launch: <line>`).
fn launch_jdwp_debugger(command: &str, address: &str) -> Result<(), crate::config::JdwpAgentFailure> {
    let line = format!("{command} dt_socket {address}");
    let refused = || crate::config::JdwpAgentFailure {
        stderr_lines: vec![format!("ERROR: JDWP unable to launch: {line}")],
        exit_code: 1,
    };
    #[cfg(windows)]
    let spawned = {
        use std::os::windows::process::CommandExt as _;
        let trimmed = line.trim_start();
        let (program, rest) = if let Some(quoted) = trimmed.strip_prefix('"') {
            match quoted.split_once('"') {
                Some((program, rest)) => (program, rest),
                None => (quoted, ""),
            }
        } else {
            trimmed.split_once(char::is_whitespace).unwrap_or((trimmed, ""))
        };
        std::process::Command::new(program).raw_arg(rest.trim_start()).spawn()
    };
    #[cfg(not(windows))]
    let spawned = {
        let mut words = line.split_whitespace();
        match words.next() {
            Some(program) => std::process::Command::new(program).args(words).spawn(),
            None => Err(std::io::Error::from(std::io::ErrorKind::NotFound)),
        }
    };
    match spawned {
        Ok(_child) => Ok(()),
        Err(_) => Err(refused()),
    }
}

/// [`start_jdwp_agent`]'s transport: listen or connect, run `launch=`, and
/// with `suspend=y` hold `thread` until the debugger resumes it. Called at
/// `VMInit`, or from the deferred start's trigger
/// ([`start_deferred_jdwp_agent`], wave 144).
fn start_jdwp_transport(
    shared: &Arc<crate::vm::SharedVm>,
    thread: &mut crate::threading::jvm_thread::JvmThread,
    agent: crate::config::JdwpAgentConfig,
) -> Result<(), crate::config::JdwpAgentFailure> {
    use crate::config::JdwpAgentFailure;
    // Wave 144: the address `launch=` is given, as the transport answers it
    // (the port a listener bound, the connect address as written).
    let launch_address;
    let server_ended = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let owned = Arc::clone(shared);
    let ended = Arc::clone(&server_ended);
    if agent.server {
        // Interpreter round i7 wave 140, lane L1 (`CRATONVM_JDWP_AGENT_ALLOW`,
        // parity, default ON, `=0` drops the option as before): the
        // transport parses `allow=` before it listens, and refuses a bad list
        // with its `Error in allow option` line and the transport error
        // (MEASURED, `L1W140JdwpAllow`); each accepted peer outside the list
        // is closed with `Debugger failed to attach: ERROR: Peer not allowed
        // to connect: <peer>` on stderr, and the transport keeps listening.
        // Page: docs/feature-designs/jvmti-delivery-threading.md.
        let allow = match agent.allow.as_deref() {
            Some(spec)
                if cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JDWP_AGENT_ALLOW") =>
            {
                match parse_jdwp_allow(spec) {
                    Ok(allow) => Some(allow),
                    Err((shown, message)) => {
                        let mut failure = JdwpAgentFailure::transport_init_with(103, message);
                        failure
                            .stderr_lines
                            .insert(0, format!("Error in allow option: '{shown}'"));
                        return Err(failure);
                    }
                }
            }
            _ => None,
        };
        // Wave 140 (L1, `CRATONVM_JDWP_AGENT_RELISTEN_LINE`, parity, default
        // ON, `=0` prints the line once as before): the transport prints its
        // `Listening for transport` line again each time it listens anew
        // after a session ended (MEASURED, `L1W140JdwpAllow` cases B-F: the
        // line twice), not after a refused peer.
        // Wave 144 (L1): with `launch=` the back end prints no `Listening`
        // line at all, neither at the listen nor after a session (MEASURED,
        // HotSpot 25.0.3, `L1W144JdwpLaunchOnThrow` A-C and F: the TTY
        // message is printed only when no debugger is launched), the
        // deferred `onthrow` / `onuncaught` start included.
        let listening_lines = !agent.quiet && agent.launch.is_none();
        let relisten_line = listening_lines
            && cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JDWP_AGENT_RELISTEN_LINE");
        let every_interface = agent.host.as_deref() == Some("*");
        let host = match agent.host.as_deref() {
            None | Some("localhost") => "127.0.0.1",
            Some("*") => "0.0.0.0",
            Some(host) => host,
        };
        let listener = std::net::TcpListener::bind((host, agent.port)).map_err(|e| {
            JdwpAgentFailure::transport_init(&format!("bind failed: {}", transport_error_text(&e)))
        })?;
        let port = listener
            .local_addr()
            .map_or(agent.port, |address| address.port());
        launch_address = port.to_string();
        let print_listening = move || {
            use std::io::Write as _;
            let mut out = std::io::stdout().lock();
            let _ = writeln!(out, "Listening for transport dt_socket at address: {port}");
            let _ = out.flush();
        };
        if listening_lines {
            print_listening();
        }
        // Wave 140 (L1, `CRATONVM_JDWP_AGENT_ACCEPT_TIMEOUT`, parity, default
        // ON, `=0` waits for ever as before): `timeout=<ms>` (not 0) bounds
        // each listen of a `server=y` transport, the first and every one
        // after a session; when no debugger connects in time the back end
        // prints its two lines on stderr and ends the process with status 0
        // (MEASURED, HotSpot 25.0.3, `suspend=n` and `suspend=y` alike).
        let accept_timeout = agent
            .timeout_ms
            .filter(|&ms| ms > 0)
            .filter(|_| {
                cratonvm_types::flags::runtime_flag_default_on(
                    "CRATONVM_JDWP_AGENT_ACCEPT_TIMEOUT",
                )
            })
            .map(std::time::Duration::from_millis);
        std::thread::Builder::new()
            .name("JDWP-Server".into())
            .spawn(move || {
                let mut first = true;
                let streams = std::iter::from_fn(|| {
                    if !std::mem::replace(&mut first, false) && relisten_line {
                        print_listening();
                    }
                    let deadline = accept_timeout.map(|wait| std::time::Instant::now() + wait);
                    loop {
                        let accepted = match accept_until(&listener, deadline) {
                            Some(accepted) => accepted,
                            None => exit_on_accept_timeout(&owned),
                        };
                        let refused_peer = match (allow.as_ref(), &accepted) {
                            (Some(allow), Ok(stream)) => stream
                                .peer_addr()
                                .ok()
                                .filter(|peer| !jdwp_allow_admits(allow, peer.ip())),
                            _ => None,
                        };
                        let Some(peer) = refused_peer else {
                            return Some(accepted);
                        };
                        {
                            use std::io::Write as _;
                            let mut err = std::io::stderr().lock();
                            let _ = writeln!(
                                err,
                                "Debugger failed to attach: ERROR: Peer not allowed to connect: {}",
                                jdwp_peer_text(peer.ip(), every_interface)
                            );
                            let _ = err.flush();
                        }
                        // Closed before the handshake; the transport listens on.
                        drop(accepted);
                    }
                });
                run_jdwp_sessions(&owned, streams);
                ended.store(true, std::sync::atomic::Ordering::Release);
            })
            .map_err(|e| JdwpAgentFailure::transport_init(&transport_error_text(&e)))?;
    } else {
        let host = agent.host.as_deref().unwrap_or("localhost");
        let connect_failed = |e: &std::io::Error| {
            JdwpAgentFailure::transport_init(&format!("connect failed: {}", transport_error_text(e)))
        };
        let addresses: Vec<std::net::SocketAddr> =
            std::net::ToSocketAddrs::to_socket_addrs(&(host, agent.port))
                .map_err(|e| connect_failed(&e))?
                .collect();
        let mut last_error = std::io::Error::from(std::io::ErrorKind::AddrNotAvailable);
        let mut connected = None;
        for address in addresses {
            let attempt = match agent.timeout_ms.filter(|&ms| ms > 0) {
                Some(ms) => std::net::TcpStream::connect_timeout(
                    &address,
                    std::time::Duration::from_millis(ms),
                ),
                None => std::net::TcpStream::connect(address),
            };
            match attempt {
                Ok(stream) => {
                    connected = Some(stream);
                    break;
                }
                Err(e) => last_error = e,
            }
        }
        let stream = connected.ok_or_else(|| connect_failed(&last_error))?;
        std::thread::Builder::new()
            .name("JDWP-Server".into())
            .spawn(move || {
                run_jdwp_sessions(&owned, std::iter::once(Ok(stream)));
                ended.store(true, std::sync::atomic::Ordering::Release);
            })
            .map_err(|e| JdwpAgentFailure::transport_init(&transport_error_text(&e)))?;
        launch_address = match agent.host.as_deref() {
            Some(host) => format!("{host}:{}", agent.port),
            None => agent.port.to_string(),
        };
    }
    // Wave 144 (L1): `launch=`, once the transport is up.
    if let Some(command) = agent.launch.as_deref() {
        launch_jdwp_debugger(command, &launch_address)?;
    }
    shared.debug.debugger_gates.set_jdwp_listening();
    if !agent.suspend && agent.server {
        return Ok(());
    }
    let tid = thread.thread_id.0;
    let mut ctx = crate::vm::NativeContextImpl {
        shared: shared.as_ref(),
        thread,
    };
    cratonvm_native_api::NativeThreadAccess::begin_blocking_region(&mut ctx);
    let answer = hold_for_vm_start(shared, tid, agent.suspend, &server_ended);
    cratonvm_native_api::NativeThreadAccess::end_blocking_region(&mut ctx);
    answer
}

/// [`start_jdwp_agent`]'s wait at `VMInit` (wave 85), in a blocking region:
/// until the debugger's session is attached (a `server=n` connection whose
/// handshake fails ends the server: the back end's
/// `handshake failed - connection prematurally closed`), then `VM_START`,
/// and with `suspend` the hold until thread `tid` is resumed (or the
/// debugger detaches).
fn hold_for_vm_start(
    shared: &crate::vm::SharedVm,
    tid: u64,
    suspend: bool,
    server_ended: &std::sync::atomic::AtomicBool,
) -> Result<(), crate::config::JdwpAgentFailure> {
    let pause = || std::thread::sleep(std::time::Duration::from_millis(2));
    while !shared.debug.debugger_gates.session_attached() {
        if server_ended.load(std::sync::atomic::Ordering::Acquire) {
            return Err(crate::config::JdwpAgentFailure::transport_init(
                "handshake failed - connection prematurally closed",
            ));
        }
        pause();
    }
    let policy = if suspend {
        events::SuspendPolicy::All
    } else {
        events::SuspendPolicy::None
    };
    {
        let mut ds = shared.debug.debug_state.lock();
        if suspend {
            ds.suspend_all();
            publish_debugger_gates(shared, &ds);
        }
    }
    send_event_set(
        shared,
        vec![DebugEvent {
            kind: events::EventKind::VMStart,
            request_id: 0,
            suspend_policy: policy,
            thread_id: tid,
            class_id: 0,
            method_id: 0,
            offset: 0,
            extra: Vec::new(),
            set_follows: false,
        }],
    );
    while suspend
        && shared.debug.debugger_gates.session_attached()
        && shared.debug.debug_state.lock().is_thread_suspended(tid)
    {
        pause();
    }
    Ok(())
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
    let hits = ds.events.match_thread_events(events::EventKind::VMDeath, 0);
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
            && shared
                .debug
                .debug_state
                .lock()
                .is_thread_suspended(dying_tid)
        {
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
    };
    match dying {
        // gci i11/ja (`gci-i10al-jvmti-event-callbacks-borrow-the-registry-jvmthread-under-the-posters-borrow-FIXED-20261017`
        // item 2): already GC-safe -- the exit native opened its region on
        // its own context (`CRATONVM_EXIT_HOOK_GC_SAFE`) -- so just wait: no
        // `&mut` of a thread whose `&mut` that native's context holds.
        Some(thread) if dying_thread_already_gc_safe(thread) => wait(),
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

/// gci i11/ja: whether the dying thread `thread` (this OS thread's own, from
/// `current_jvm_thread_of`) is already in its blocked region. One atomic read
/// through a shared reference; nothing is written.
fn dying_thread_already_gc_safe(thread: *mut crate::threading::jvm_thread::JvmThread) -> bool {
    // SAFETY: this OS thread's own live `JvmThread` (see `hold_for_vm_death`);
    // only an atomic is read, through a shared reference.
    let view = unsafe { &*(thread as *const crate::threading::jvm_thread::JvmThread) };
    view.gc_block_state
        .in_blocked_region
        .load(std::sync::atomic::Ordering::Acquire)
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

/// [`debugger_local`] for a READ whose object answer becomes a handle (a
/// JDWP snapshot's pinned export, a JVMTI `GetLocalObject` reference). gcf
/// f2/f, `gcf-f1f-debugger-local-reads-export-liveness-dead-stale-locals-FIXED-20261012.md`.
///
/// A reference local the frame's per-bci liveness calls dead (`live`, from
/// `Frame::live_locals_mask_here`) is not a root (`Frame::scan_local_objects`),
/// so a collection may have freed or moved its referent and left the old
/// address in the slot, with its object tag. A handle to that address is a
/// root to garbage. Such a slot is answered as its object only while the
/// address is still an object start in the heap (the validator the
/// conservative stack scans use on stale words, which reads no header outside
/// a committed arena); otherwise as `null`. A live slot is answered as
/// before, and so is every slot when no collection has run since it died --
/// HotSpot keeps every local alive under a debugger and shows the object.
pub(crate) fn debugger_local_for_read(
    shared: &crate::vm::SharedVm,
    frame: &crate::runtime::frame::Frame,
    slot: u16,
    live: &crate::runtime::local_liveness::LiveMask,
) -> crate::types::Value {
    screen_dead_local(shared, live, slot, debugger_local(frame, slot))
}

/// The screen of [`debugger_local_for_read`] over a value already read (and,
/// for a blocked thread's window, already brought to the referent's current
/// address).
pub(crate) fn screen_dead_local(
    shared: &crate::vm::SharedVm,
    live: &crate::runtime::local_liveness::LiveMask,
    slot: u16,
    v: crate::types::Value,
) -> crate::types::Value {
    match v {
        crate::types::Value::Object(Some(r))
            if !live.is_live(usize::from(slot))
                // Cast: the reference's address, probed, never dereferenced here.
                && !shared.mem.heap.is_object_start(r.as_ptr() as usize) =>
        {
            crate::types::Value::Object(None)
        }
        other => other,
    }
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
                .referent_keep_alive(crate::types::Value::Object(Some(o)));
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
    // `ids::MAIN_THREAD_WIRE_ID`, and 0 is null. Wave 104 (lane L1): once
    // `main` has ended, `DESTROY_JAVA_VM_WIRE_ID` names VM thread 0, and
    // `main`'s id the ended `main`, whose `Thread` is no longer the entry's.
    let ended_main = ds.destroy_java_vm_split.is_some() && id == ids::MAIN_THREAD_WIRE_ID;
    let id = ds.vm_thread(id);
    if ended_main {
        return ds.ended_main_thread_obj(shared);
    }
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

// ---------------------------------------------------------------------------
// Event object holds (interpreter round i6 wave 127, lane L1)
// ---------------------------------------------------------------------------
//
// An object id is backed by a WEAK handle (wave 8), as HotSpot's back end
// keeps its `commonRef` nodes. An `Exception` event with `SUSPEND_NONE` lets
// the thread run on: the exception is caught, unreachable, and a collection
// that ends before the debugger's first `ObjectReference.ReferenceType`
// takes it -- JDI then throws `ObjectCollectedException` from
// `ExceptionEvent.exception().referenceType()` (`L1W93JdiExceptionFromCompiled`,
// `--compatible` with the JIT, 4 of 4 runs). HotSpot's `eventHelper` holds
// the event's objects strongly until the packet is written, and its
// collections do not land in the debugger's round trip after it; here one
// did. So the event's object is held with its collection disabled (the
// `DisableCollection` count, which nests with the debugger's own) until the
// first heap command that names it, which then sees it as HotSpot's debugger
// does and leaves it as weak as before. `CRATONVM_JDWP_EVENT_OBJECT_HOLD=0`
// restores the bare weak id. Page:
// docs/feature-designs/jvmti-delivery-threading.md.

/// At most this many event objects are held; past it the oldest hold goes
/// (a raw JDWP client that never looks at the objects must not pin every
/// exception the session reported).
pub(crate) const EVENT_OBJECT_HOLD_CAP: usize = 1024;

/// Hold `obj`, which an event being built exported just now (same
/// debug-state critical section), with its collection disabled until the
/// debugger's first heap command naming its id ([`release_event_hold_named`]).
/// No-op under `CRATONVM_JDWP_EVENT_OBJECT_HOLD=0`, for an object exported as
/// something else (a live thread goes out as its thread id), or an id already
/// held. Same calling rule as [`export_object`] (no collection can run).
pub(crate) fn hold_event_object(
    shared: &crate::vm::SharedVm,
    ds: &mut DebugState,
    obj: crate::types::ObjectRef,
) {
    if !ds.event_object_hold {
        return;
    }
    // Cast: the address index key. The export that just ran rekeyed the
    // index and confirmed (or minted) the entry at this address.
    let Some((id, _)) = ds.objects.candidate_for_addr(obj.as_ptr() as usize) else {
        return;
    };
    if ds.event_object_holds.contains(&id) || !set_collection_enabled(shared, ds, id, false) {
        return;
    }
    ds.event_object_holds.push_back(id);
    while ds.event_object_holds.len() > EVENT_OBJECT_HOLD_CAP {
        if let Some(oldest) = ds.event_object_holds.pop_front() {
            let _ = set_collection_enabled(shared, ds, oldest, true);
        }
    }
    // The weak handles the swaps replaced.
    release_disposed_objects(shared, ds);
}

/// After a heap command (`inspect::execute`, no collection able to run):
/// release the event hold of the object id the command names first, if it
/// is held ([`hold_event_object`]). The object-centred command sets
/// (`ObjectReference`, `StringReference`, `ArrayReference`,
/// `ClassLoaderReference`, `ClassObjectReference`) all start with it.
pub(crate) fn release_event_hold_named(
    shared: &crate::vm::SharedVm,
    ds: &mut DebugState,
    command_set: u8,
    data: &[u8],
) {
    if ds.event_object_holds.is_empty() {
        return;
    }
    if !matches!(
        command_set,
        commands::CS_OBJECT_REF
            | commands::CS_STRING_REF
            | commands::CS_ARRAY_REF
            | commands::CS_CLASSLOADER_REF
            | commands::CS_CLASS_OBJ_REF
    ) {
        return;
    }
    let Ok(id) = protocol::PayloadReader::new(data).read_u64_be() else {
        return;
    };
    let Some(at) = ds.event_object_holds.iter().position(|&held| held == id) else {
        return;
    };
    ds.event_object_holds.remove(at);
    // `false` for an id disposed of meanwhile: nothing left to release.
    let _ = set_collection_enabled(shared, ds, id, true);
    release_disposed_objects(shared, ds);
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
    /// Queued for any parked thread (no thread named: heap work, wave 66):
    /// it neither reads nor writes the frames of the thread that runs it, so
    /// a park's standing deposit outlives it ([`ParkGcRegion`]).
    any_thread: bool,
}

impl ParkedTask {
    /// Was this work queued for any parked thread (heap work), not for this
    /// one (an invocation, a frame write, a stack read)? Interpreter round
    /// i2 wave 66, lane L6.
    pub(crate) fn runs_on_any_thread(&self) -> bool {
        self.any_thread
    }
}

/// Where [`DebugState::heap_work_target`] sends heap work (wave 66).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HeapWorkTarget {
    /// This parked thread, idle.
    Parked(u64),
    /// The heap service ([`start_heap_service`]).
    Service,
    /// No thread can take it.
    Nowhere,
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
/// (docs/feature-designs/jvmti-delivery-threading.md).
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
            // Wave 66 (lane L6, `CRATONVM_DEBUG_HEAP_WORK_OUTSIDE_REGION`):
            // a park outside its GC-blocked region first, then the heap
            // service, then a park inside one.
            let outside_region = shared.debug.debugger_gates.heap_work_outside_region();
            match ds.heap_work_target(outside_region, invocation.is_none()) {
                HeapWorkTarget::Parked(t) => t,
                // No thread can take it (wave 10): heap work (a `None` target
                // is never an invocation) goes to the heap service, a
                // registered mutator of its own, when the session has one.
                HeapWorkTarget::Service => {
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
                HeapWorkTarget::Nowhere => return Err(ParkedError::NotParked),
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
            any_thread: tid.is_none(),
        },
    );
    // Wave 63 (lane L6): a thread parked on its own word sees the task bit.
    ds.republish_suspend_word(target);
    // Wave 54 (lane L6): the parked thread takes it now, not at its next
    // poll ([`DebuggerGates::park_condvar`]).
    shared.debug.debugger_gates.notify_state_changed();
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
    let _ = send_thread_event_suspending(shared, kind, thread_id, is_virtual);
}

/// A platform thread's JDWP `ThreadDeath` ([`send_thread_event`]), answering
/// whether the event set's own suspend policy suspended something
/// (`SUSPEND_EVENT_THREAD`: the thread; `SUSPEND_ALL`: the VM). Interpreter
/// round i5 wave 106, lane L1 (hand-back): only then does the dying thread
/// hold ([`hold_ending_thread_while_suspended`]); a suspension it did not
/// cause -- another thread's, a debugger invocation's closing re-suspension,
/// a `VirtualMachine.Suspend` that a `ThreadReference.Stop`'s thread ran
/// into -- never holds it (`L1W25JdiStopMonitors` hung on that).
pub(crate) fn send_thread_death_event(shared: &crate::vm::SharedVm, thread_id: u64) -> bool {
    let is_virtual = shared.threads.virtual_thread_manager.is_virtual(thread_id);
    send_thread_event_suspending(shared, events::EventKind::ThreadDeath, thread_id, is_virtual)
}

/// [`send_thread_event_of`], answering whether the set's policy suspended.
fn send_thread_event_suspending(
    shared: &crate::vm::SharedVm,
    kind: events::EventKind,
    thread_id: u64,
    is_virtual: bool,
) -> bool {
    let (hits, suspended) = {
        let mut ds = shared.debug.debug_state.lock();
        let is_virtual = is_virtual || ds.virtual_threads.contains(&thread_id);
        // Wave 45 (lane L1): a thread that ends leaves no C JVMTI suspension
        // behind.
        let forgot =
            matches!(kind, events::EventKind::ThreadDeath) && ds.forget_jvmti_suspension(thread_id);
        // Interpreter round i7 wave 136 (lane L1): nor a C agent's frame
        // control it never took (its object's global reference is released).
        if matches!(kind, events::EventKind::ThreadDeath) {
            early_return::forget_thread(shared, &mut ds, thread_id);
        }
        match kind {
            events::EventKind::ThreadStart if is_virtual => {
                ds.virtual_threads.insert(thread_id);
            }
            events::EventKind::ThreadDeath => {
                ds.virtual_threads.remove(&thread_id);
            }
            _ => {}
        }
        let hits = ds
            .events
            .match_thread_events_of(kind, thread_id, is_virtual);
        // One event set, suspended once with its strongest policy (wave 22).
        let policy = strongest_suspend_policy(hits.iter().map(|&(_, p)| p));
        let suspended = ds.apply_event_set_policy(policy, thread_id);
        // Wave 46 (lane L1): or a `Count` this match spent.
        let spent = take_spent_requests(&mut ds);
        if suspended || forgot || spent {
            publish_debugger_gates(shared, &ds);
        }
        (hits, suspended)
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
    suspended
}

/// `main` has ended its Java side on the launcher's thread, VM thread 0,
/// whose `Thread` is still `mirror` (interpreter round i5 wave 104, lane L1;
/// called by `vm_exec::end_main_thread_java_side` after `Thread.exit()`,
/// before the termination's `ensure_join`, where HotSpot's
/// `JavaThread::exit` posts `ThreadEnd`). With
/// `DebugState::destroy_java_vm_identity` and `destroy_java_vm` (the
/// launcher will list a `DestroyJavaVM` thread,
/// `VmConfig::destroy_java_vm_thread`): a C JVMTI agent's `ThreadEnd` for
/// `main`, the JDWP `ThreadDeath` of `main`'s id, and from then on the
/// `DestroyJavaVM` split ([`DebugState::destroy_java_vm_split`]): `main`'s
/// id names the ended `main` (`ids::ENDED_MAIN_VM_ID`, ZOMBIE) and VM
/// thread 0 goes out as `ids::DESTROY_JAVA_VM_WIRE_ID`.
///
/// The death's suspend policy is counted on the ended `main`, not on VM
/// thread 0: the debugger resumes the event's thread by `main`'s id, which
/// now names the ended `main`, so a suspension of thread 0 would never be
/// resumed. HotSpot holds `main` in its exit until then; here the launcher
/// goes on to list `DestroyJavaVM`.
pub(crate) fn note_main_thread_ended(
    shared: &crate::vm::SharedVm,
    name: &str,
    mirror: crate::types::ObjectRef,
    destroy_java_vm: bool,
) {
    if !destroy_java_vm || !shared.debug.debug_state.lock().destroy_java_vm_identity {
        return;
    }
    // The handle first: `mirror` is current only until this thread's next
    // safepoint, which the agent's callback below may reach.
    let handle = shared.natives.jni_global_refs.lock().add_weak(mirror);
    if shared.debug.debugger_gates.native_thread_events_armed() {
        crate::jvmti::native_env::post_thread_event(shared, 0, false);
    }
    let hits = {
        let mut ds = shared.debug.debug_state.lock();
        if ds.destroy_java_vm_split.is_some() {
            drop(ds);
            let _ = shared.natives.jni_global_refs.lock().remove(handle);
            return;
        }
        let hits = if shared.debug.debugger_gates.session_attached() {
            ds.events
                .match_thread_events_of(events::EventKind::ThreadDeath, 0, false)
        } else {
            Vec::new()
        };
        let policy = strongest_suspend_policy(hits.iter().map(|&(_, p)| p));
        let suspended = match policy {
            events::SuspendPolicy::EventThread => {
                ds.suspend_thread(ids::ENDED_MAIN_VM_ID);
                true
            }
            events::SuspendPolicy::All => {
                ds.suspend_all();
                true
            }
            events::SuspendPolicy::None => false,
        };
        let spent = take_spent_requests(&mut ds);
        // Interpreter round i5 wave 105 (lane L1): a `ThreadOnly` filter or a
        // step naming `main` (VM thread 0, which the launcher goes on to run
        // as `DestroyJavaVM`) now names the ended `main`, so it matches none
        // of `DestroyJavaVM`'s events: HotSpot's filter holds `main`'s
        // thread object. After `main`'s own `ThreadDeath` matched above.
        let renamed = ds.events.rename_thread_filters(0, ids::ENDED_MAIN_VM_ID) > 0;
        ds.destroy_java_vm_split = Some(name.to_string());
        ds.ended_main_handle = handle;
        ds.thread_names
            .insert(ids::ENDED_MAIN_VM_ID, name.to_string());
        ds.thread_statuses
            .insert(ids::ENDED_MAIN_VM_ID, commands::THREAD_STATUS_ZOMBIE);
        shared
            .debug
            .debugger_gates
            .destroy_java_vm_split
            .store(true, std::sync::atomic::Ordering::Release);
        if suspended || spent || renamed {
            publish_debugger_gates(shared, &ds);
        }
        hits
    };
    let set = hits
        .into_iter()
        .map(|(request_id, suspend_policy)| DebugEvent {
            kind: events::EventKind::ThreadDeath,
            request_id,
            suspend_policy,
            // `thread_to_wire` sends it unchanged: `main`'s wire id.
            thread_id: ids::ENDED_MAIN_VM_ID,
            class_id: 0,
            method_id: 0,
            offset: 0,
            extra: Vec::new(),
            set_follows: false,
        })
        .collect();
    send_event_set(shared, set);
}

/// The launcher's thread, VM thread 0, now carries the `DestroyJavaVM`
/// `Thread` (interpreter round i5 wave 104, lane L1; called by
/// `vm_exec::list_destroy_java_vm_thread` once the mirror is registered):
/// HotSpot's `jni_DestroyJavaVM` attaches it, which posts JVMTI and JDWP
/// `ThreadStart`. Posts both when [`note_main_thread_ended`] started the
/// split; the JDWP event goes out under `ids::DESTROY_JAVA_VM_WIRE_ID`
/// ([`send_event_set`]).
pub(crate) fn note_destroy_java_vm_attached(shared: &crate::vm::SharedVm) {
    if !shared.debug.debugger_gates.destroy_java_vm_split() {
        return;
    }
    if shared.debug.debugger_gates.native_thread_events_armed() {
        crate::jvmti::native_env::post_thread_event(shared, 0, true);
    }
    if shared.debug.debugger_gates.session_attached() {
        send_thread_event_of(shared, events::EventKind::ThreadStart, 0, false);
    }
}

/// Report JDWP monitor event `kind` (43-46) about monitor `obj` on thread
/// `tid`, the calling thread (interpreter round i5 wave 103, lane L1;
/// `canRequestMonitorEvents`). Called from
/// `jvmti::native_env::post_monitor_event`, which the monitor paths call
/// where they post the C JVMTI events, while
/// [`DebuggerGates::jdwp_monitor_events_armed`]. `trailer` is the event's
/// data after its location: a `MonitorWait`'s timeout (`long`), a
/// `MonitorWaited`'s `timed_out` (`boolean`), nothing for the contended ones.
///
/// As HotSpot's back end (`eventHandler.c`, `cbMonitor*`): the location is
/// the thread's top frame (`GetFrameLocation(thread, 0)`; here its top
/// interpreter frame, the null location without one), whose class the
/// `ClassOnly` / `ClassMatch` / `ClassExclude` filters test; `InstanceOnly`
/// tests the monitor object. A suspending policy counts the suspension here;
/// the thread parks at its next bytecode (a contended entry gives the
/// monitor back while suspended, `vm_exec::block_enter_suspension_aware`),
/// as `class_prepared_on_thread` does.
pub(crate) fn post_jdwp_monitor_event(
    shared: &crate::vm::SharedVm,
    tid: u64,
    kind: events::EventKind,
    obj: crate::types::ObjectRef,
    trailer: &[u8],
) {
    use events::{EventLocation, EventSubject};
    if debugger_hooks_suppressed() {
        return;
    }
    let (filter_classes, instance_ids) = {
        let ds = shared.debug.debug_state.lock();
        if !ds.events.has_requests(kind) {
            return;
        }
        (
            ds.events.filter_class_ids(kind),
            ds.events.instance_filter_ids(kind),
        )
    };
    // The calling thread's top interpreter frame.
    let top: Option<(crate::classloading::ClassId, String, u64, u64, usize)> =
        crate::native::jni::current_jvm_thread_of(shared).and_then(|current| {
            // SAFETY: the calling thread's own, live `JvmThread`, read only
            // here, while the thread runs this call.
            let thread = unsafe { &*current };
            let depth = thread.frames.len();
            thread.frames.last().map(|frame| {
                (
                    frame.class_id,
                    frame.class_name().to_string(),
                    frame_method_id(frame),
                    frame.last_instr_pc as u64, // Widening: bytecode index
                    depth,
                )
            })
        });
    // What needs the class manager, before the debug-state lock.
    let (location_supers, object_tag) = {
        let cm = shared.classes.class_manager.read();
        (
            top.as_ref().map_or_else(Vec::new, |(class, ..)| {
                supertypes_among(&cm, *class, &filter_classes)
            }),
            inspect::object_tag(shared, &cm, obj),
        )
    };
    let (class_id, class_name, method_id, offset, frame_depth) = match &top {
        Some((class, name, method, offset, depth)) => (
            u64::from(class.as_u32()),
            name.as_str(),
            *method,
            *offset,
            *depth,
        ),
        None => (0, "", 0, 0, 0),
    };
    let location = EventLocation {
        class_id,
        class_name,
        method_id,
        offset,
        thread_id: tid,
        frame_depth,
        line_start: None,
        line: None,
    };
    let set = {
        let mut ds = shared.debug.debug_state.lock();
        let instance_hits: Vec<u64> = instance_ids
            .into_iter()
            .filter(|&id| object_for_id(shared, &ds, id) == Some(obj))
            .collect();
        let subject = EventSubject {
            location_class_is: &|id| location_supers.contains(&id),
            field: None,
            instance_is: &|id| instance_hits.contains(&id),
            exception_is: &|_| false,
            caught: false,
        };
        let hits = ds.events.match_subject_events(kind, &location, &subject);
        let mut set = Vec::with_capacity(hits.len());
        for &(request_id, suspend_policy) in &hits {
            // The tagged monitor object, then the trailer; the drain puts the
            // location between them. Each event exports its own reference.
            let mut extra = protocol::PayloadWriter::new();
            inspect::put_tagged_with_tag(
                shared,
                &mut ds,
                &mut extra,
                b'L',
                crate::types::Value::Object(Some(obj)),
                Some(object_tag),
            );
            extra.put_bytes(trailer);
            set.push(DebugEvent {
                kind,
                request_id,
                suspend_policy,
                thread_id: tid,
                class_id,
                method_id,
                offset,
                extra: extra.into_bytes(),
                set_follows: false,
            });
        }
        // Interpreter round i7 wave 139, lane L1 (`CRATONVM_JDWP_EVENT_OBJECT_HOLD`,
        // the "not covered" part of i127-L1): the monitor lives until the
        // debugger's first look at it, as an `Exception` event's object does.
        if !set.is_empty() {
            hold_event_object(shared, &mut ds, obj);
        }
        let policy = strongest_suspend_policy(hits.iter().map(|&(_, p)| p));
        let suspended = ds.apply_event_set_policy(policy, tid);
        let spent = take_spent_requests(&mut ds);
        if suspended || spent {
            publish_debugger_gates(shared, &ds);
        }
        set
    };
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
        STARTUP_ARRAYS.iter().any(|name| {
            cm.loaded_class_under_exact_key(name, ClassLoaderId::Bootstrap)
                .is_none()
        })
    };
    if missing {
        let mut cm = shared.classes.class_manager_write_settling();
        for name in STARTUP_ARRAYS {
            let _ = cm.load_array_class_for_loader(name, ClassLoaderId::Bootstrap);
        }
    }
}

fn populate_class_metadata(shared: &crate::vm::SharedVm) -> Vec<(u64, String, u8)> {
    let added = populate_stored_class_metadata(shared);
    register_lambda_proxy_metadata(shared);
    added
}

/// Record debugger metadata for the lambda proxy classes minted since the
/// last call (the i54-L1 retirement,
/// `docs/feature-designs/jvmti-delivery-threading.md`):
/// the VM mints a lambda's class outside the class store, so the session's
/// tables never described one, and a listing that names its method's frame
/// (`interpreter::ListingSplice`) named a class no `ReferenceType` command
/// knew. Each is described from its facts record
/// (`runtime::lambda_proxy::lambda_proxy_facts`), as HotSpot's back end
/// answers the hidden class (MEASURED, `tools/probes/interp/L1/L1K54JdiLambdaProxyFrames.java`):
/// its signature, `0x1030`, `java.lang.Object` as its superclass, its
/// interfaces, methods (with their spun code's lengths) and `arg$N` fields,
/// no source file, class-file version 69, status VERIFIED | PREPARED |
/// INITIALIZED; `AllClasses` lists it. An id minted but not yet linked to
/// its call site is retried at the next call. Not reported by
/// `ClassPrepare`. Nothing but one lock and a compare when no proxy is new.
fn register_lambda_proxy_metadata(shared: &crate::vm::SharedVm) {
    let next = shared
        .classes
        .next_lambda_id
        .load(std::sync::atomic::Ordering::Acquire);
    let (from, mut ids) = {
        let ds = shared.debug.debug_state.lock();
        if ds.lambda_ids_seen >= next && ds.lambda_ids_pending.is_empty() {
            return;
        }
        (ds.lambda_ids_seen, ds.lambda_ids_pending.clone())
    };
    ids.extend(from..next);
    let object = shared
        .classes
        .class_manager
        .read()
        .resolve_fast_path_class_id("java/lang/Object")
        .map(|id| u64::from(id.as_u32()));
    let mut described = Vec::new();
    let mut pending = Vec::new();
    for raw in ids {
        let id = crate::classloading::ClassId::new(raw);
        let Some(facts) = crate::runtime::lambda_proxy::lambda_proxy_facts(shared, id) else {
            pending.push(raw);
            continue;
        };
        let lengths: Vec<Option<u64>> = (0..facts.methods.len())
            .map(|index| {
                crate::runtime::lambda_proxy::lambda_proxy_code(shared, id, index)
                    .and_then(|code| u64::try_from(code.len()).ok())
            })
            .collect();
        described.push((id, facts, lengths));
    }
    let mut ds = shared.debug.debug_state.lock();
    for (id, facts, lengths) in described {
        add_lambda_proxy_metadata(&mut ds, id, &facts, &lengths, object);
    }
    ds.lambda_ids_seen = ds.lambda_ids_seen.max(next);
    ds.lambda_ids_pending = pending;
}

/// JDWP `ClassStatus` of a lambda proxy class: VERIFIED | PREPARED |
/// INITIALIZED (it exists once its call site is linked; measured 7, as
/// JVMTI's `GetClassStatus` answers it).
const LAMBDA_PROXY_CLASS_STATUS: u32 = CLASS_STATUS_LINKED | CLASS_STATUS_INITIALIZED;

/// Record lambda proxy `id`'s metadata from its `facts`
/// ([`register_lambda_proxy_metadata`]); `lengths` the spun code length of
/// each method, `object` the wire id of `java.lang.Object`.
fn add_lambda_proxy_metadata(
    ds: &mut DebugState,
    id: crate::classloading::ClassId,
    facts: &crate::runtime::lambda_proxy::LambdaProxyFacts,
    lengths: &[Option<u64>],
    object: Option<u64>,
) {
    let class_id = u64::from(id.as_u32());
    ds.class_type_tags.insert(class_id, 1);
    ds.class_signatures.insert(class_id, facts.signature(id));
    if let Some(object) = object {
        ds.class_superclass.insert(class_id, object);
    }
    ds.class_statuses
        .insert(class_id, LAMBDA_PROXY_CLASS_STATUS);
    let mut details = ClassDetails {
        modifiers: u32::from(crate::runtime::lambda_proxy::LAMBDA_PROXY_CLASS_MODIFIERS),
        generic_signature: None,
        interfaces: facts
            .interfaces
            .iter()
            .map(|i| u64::from(i.as_u32()))
            .collect(),
        // The spun class's version: the running JDK's latest (measured 69).
        version: Some((69, 0)),
        ..ClassDetails::default()
    };
    let mut methods = Vec::with_capacity(facts.methods.len());
    for (index, m) in facts.methods.iter().enumerate() {
        let method_id = jdwp_method_id(&m.name, &m.descriptor);
        methods.push(commands::MethodInfo {
            method_id: ids::MethodId(method_id),
            name: m.name.to_string(),
            signature: m.descriptor.to_string(),
            // Never synthetic (measured).
            mod_bits: jdwp_member_modifiers(
                u32::from(m.access_flags),
                JVM_RECOGNIZED_METHOD_MODIFIERS,
                false,
            ),
        });
        if let Some(length) = lengths.get(index).copied().flatten() {
            details.code_lengths.insert(method_id, length);
        }
    }
    ds.class_methods.insert(class_id, methods);
    let fields = facts
        .fields
        .iter()
        .enumerate()
        .map(|(i, (name, descriptor))| commands::FieldInfo {
            field_id: ids::FieldId(jdwp_field_id(id, i)),
            name: name.clone(),
            signature: descriptor.clone(),
            mod_bits: u32::from(crate::runtime::lambda_proxy::LAMBDA_PROXY_FIELD_MODIFIERS),
        })
        .collect();
    ds.class_fields.insert(class_id, fields);
    ds.class_details.insert(class_id, details);
}

fn populate_stored_class_metadata(shared: &crate::vm::SharedVm) -> Vec<(u64, String, u8)> {
    let cm = shared.classes.class_manager.read();
    let slots = cm.class_store.slot_count();
    let mut ds = shared.debug.debug_state.lock();
    let from = ds.class_slots_seen;
    let mut added = Vec::new();
    // zt t18/df7 (x13 item 21a): a define whose name is not bound (its
    // `addClass` upcall in flight, or thrown) is in no dictionary, so no
    // `AllClasses` / `ClassesBySignature` lists it. Its slot is kept aside
    // and described once its name is bound; a slot whose class is gone
    // (unloaded) is dropped.
    if !ds.class_slots_nameless.is_empty() {
        let pending = std::mem::take(&mut ds.class_slots_nameless);
        for raw in pending {
            let id = crate::classloading::ClassId::new(raw);
            match cm.class_store.get(id) {
                Some(_) if cm.class_store.is_nameless(id) => ds.class_slots_nameless.push(raw),
                Some(class) => added.push(add_class_metadata(&mut ds, class)),
                None => {}
            }
        }
    }
    if slots <= from {
        return added;
    }
    for slot in from..slots {
        let Ok(raw) = u32::try_from(slot) else {
            break;
        };
        let id = crate::classloading::ClassId::new(raw);
        if let Some(class) = cm.class_store.get(id) {
            if cm.class_store.is_nameless(id) {
                ds.class_slots_nameless.push(raw);
                continue;
            }
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

/// Is `class` loaded but not prepared, so that HotSpot's back end would leave
/// it out of `AllClassesWithGeneric` and `ClassesBySignature` and post no
/// `ClassPrepare` for it (interpreter round i5 wave 103, lane L1)? An array
/// class never is (JVMTI's `ARRAY` status keeps it listed). A class being
/// defined (`Loading`), being linked or whose link failed (`Verifying`,
/// [`crate::vm::LINK_FAILED_STATE`]) is. A `Loaded` class is, unless linking
/// it is trivial ([`crate::vm::class_link_is_trivial`]): such a class (a
/// bootstrap JDK class) may be instantiated without ever being linked while
/// no `ClassPrepare` is observed (`vm::class_linked_or_link_trivial`), so
/// its code may run while it reads `Loaded`; it stays listed, as every class
/// was before wave 103. A class whose link is not trivial is never used
/// before its link moves it on.
///
/// Interpreter round i5 wave 104 (lane L1): a `Loaded` class with a trivial
/// link is unprepared too when it was defined while a JDWP `ClassPrepare`
/// request was in force (`watermark`, [`DebugState::class_prepare_watermark`],
/// and one is in force now): `vm::class_linked_or_link_trivial` answered no
/// for it all along, so no code used it unlinked, and HotSpot hides it (a JDK
/// class loaded by `Class.forName(n, false, null)`). A trivially linked class
/// defined before the request stays listed (it may have been used unlinked).
pub(crate) fn jdwp_class_unprepared(
    shared: &crate::vm::SharedVm,
    cm: &crate::classloading::ClassManager,
    class: &crate::classloading::Class,
    watermark: Option<usize>,
) -> bool {
    use crate::classloading::ClassState;
    if class.name.starts_with('[') {
        return false;
    }
    match class.state {
        ClassState::Loading | ClassState::Verifying => true,
        ClassState::Loaded => {
            !crate::vm::class_link_is_trivial(shared, cm, class)
                || defined_while_class_prepare_observed(shared, class, watermark)
                || never_used_while_class_prepare_observed(shared, class, watermark)
        }
        _ => false,
    }
}

/// Interpreter round i5 wave 106 (lane L1; the residue of
/// `interpreter-i102-L1-jdwp-lists-unprepared-classes-FIXED-20261016.md`): is `class`, a
/// `Loaded` class with a trivial link defined BEFORE the watermark, one that
/// no path has used unlinked ([`UsedUnlinkedClasses`]), with a JDWP
/// `ClassPrepare` request in force? Then it has run no code: the only paths
/// that use a class without linking it (`jit_bridge::jit_new_target_init_walk`,
/// `exceptions::stackless_exception_class`) record the use first, and while
/// the request is in force they refuse a never-linked class
/// (`vm::class_linked_or_link_trivial`), so a later use initializes it on a
/// Java thread, which prepares it and posts its `ClassPrepare`. HotSpot hides
/// such a class (`Class.forName(n, false, null)` before the attach).
/// `CRATONVM_JDWP_HIDE_UNUSED_UNLINKED_CLASSES=0`: never (listed, as before).
fn never_used_while_class_prepare_observed(
    shared: &crate::vm::SharedVm,
    class: &crate::classloading::Class,
    watermark: Option<usize>,
) -> bool {
    watermark.is_some()
        && !shared.debug.used_unlinked_classes.contains(class.id)
        && shared.debug.debugger_gates.class_prepare_armed()
}

/// The classes some path used while they were still never linked (`Loaded`,
/// or `Verifying` after a failed link) because linking them is trivial
/// (`vm::class_link_is_trivial`): one bit per class-store slot, set once and
/// never cleared (interpreter round i5 wave 106, lane L1). Only the JDWP
/// listing reads it ([`never_used_while_class_prepare_observed`]); a set bit
/// keeps the class listed, so an over-approximation (a bit set by a walk whose
/// answer then did not use the class, a slot reused by another class) only
/// lists what wave 104 listed. A slot past the capacity reads as used.
///
/// Per VM (`DebugRealm::used_unlinked_classes`), read-only after creation
/// apart from the bits; `CRATONVM_JDWP_HIDE_UNUSED_UNLINKED_CLASSES=0` makes
/// every class read as used and records nothing.
pub struct UsedUnlinkedClasses {
    /// `None` with the switch off.
    words: Option<Box<[std::sync::atomic::AtomicU64]>>,
}

/// Class-store slots [`UsedUnlinkedClasses`] keeps a bit for (16 KiB per VM).
const USED_UNLINKED_CAPACITY: usize = 1 << 17;

impl UsedUnlinkedClasses {
    /// The VM's set, empty; the switch read once.
    pub fn new() -> Self {
        let on = cratonvm_types::flags::runtime_flag_default_on(
            "CRATONVM_JDWP_HIDE_UNUSED_UNLINKED_CLASSES",
        );
        Self {
            words: on.then(|| {
                (0..USED_UNLINKED_CAPACITY / 64)
                    .map(|_| std::sync::atomic::AtomicU64::new(0))
                    .collect()
            }),
        }
    }

    /// Has `id` been used unlinked (or is it past the capacity, or the switch
    /// off)? One relaxed load.
    #[inline]
    pub fn contains(&self, id: crate::classloading::ClassId) -> bool {
        let Some(words) = self.words.as_deref() else {
            return true;
        };
        // Widening: a class id is a `u32` store slot.
        let slot = id.as_u32() as usize;
        match words.get(slot / 64) {
            Some(word) => {
                word.load(std::sync::atomic::Ordering::Relaxed) & (1u64 << (slot % 64)) != 0
            }
            None => true,
        }
    }

    /// Record `id`. Cold: once per class.
    #[cold]
    #[inline(never)]
    fn insert(&self, id: crate::classloading::ClassId) {
        let Some(words) = self.words.as_deref() else {
            return;
        };
        let slot = id.as_u32() as usize;
        if let Some(word) = words.get(slot / 64) {
            word.fetch_or(1u64 << (slot % 64), std::sync::atomic::Ordering::Relaxed);
        }
    }
}

impl Default for UsedUnlinkedClasses {
    fn default() -> Self {
        Self::new()
    }
}

/// `class` is about to be used although it was never linked, its link being
/// trivial (`vm::class_linked_or_link_trivial` answered yes for it): record
/// it ([`UsedUnlinkedClasses`]). A linked class is not recorded. One relaxed
/// load once recorded. Interpreter round i5 wave 106, lane L1; called by
/// `jit_bridge::jit_new_target_init_walk` for each class of the chain it lets
/// a resolved `new` row skip.
#[inline]
pub(crate) fn note_class_used_unlinked(
    shared: &crate::vm::SharedVm,
    class: &crate::classloading::Class,
) {
    use crate::classloading::ClassState;
    if matches!(class.state, ClassState::Loaded | ClassState::Verifying)
        && !shared.debug.used_unlinked_classes.contains(class.id)
    {
        shared.debug.used_unlinked_classes.insert(class.id);
    }
}

/// `leaf` and the never-linked classes above it are about to be used without
/// being initialized (`exceptions::stackless_exception_class`, a fast-throw
/// instance of a class not initialized): record each never-linked class of
/// the chain, and `leaf` itself, so the next call returns after ONE relaxed
/// load (the leaf's bit). The walk stops at a linked class (its supertypes
/// were linked with it). Interpreter round i5 wave 106, lane L1.
#[inline]
pub(crate) fn note_chain_used_unlinked(
    shared: &crate::vm::SharedVm,
    cm: &crate::classloading::ClassManager,
    leaf: crate::classloading::ClassId,
) {
    if shared.debug.used_unlinked_classes.contains(leaf) {
        return;
    }
    note_chain_used_unlinked_cold(shared, cm, leaf);
}

#[cold]
#[inline(never)]
fn note_chain_used_unlinked_cold(
    shared: &crate::vm::SharedVm,
    cm: &crate::classloading::ClassManager,
    leaf: crate::classloading::ClassId,
) {
    use crate::classloading::ClassState;
    // A hierarchy is shallow; a longer walk is a malformed graph.
    const MAX_WALK: usize = 256;
    let mut todo = vec![leaf];
    let mut walked = 0usize;
    while let Some(id) = todo.pop() {
        walked += 1;
        if walked > MAX_WALK {
            break;
        }
        let Some(class) = cm.get_class(id) else {
            continue;
        };
        if !matches!(class.state, ClassState::Loaded | ClassState::Verifying) {
            continue;
        }
        shared.debug.used_unlinked_classes.insert(id);
        todo.extend(class.superclass);
        todo.extend(class.interfaces.iter().copied());
    }
    // The leaf's bit is the fast path's answer, linked or not.
    shared.debug.used_unlinked_classes.insert(leaf);
}

/// Was `class` defined at or after `watermark` (a slot of the class store,
/// [`DebugState::class_prepare_watermark`]), with a JDWP `ClassPrepare`
/// request still in force (interpreter round i5 wave 104, lane L1)?
fn defined_while_class_prepare_observed(
    shared: &crate::vm::SharedVm,
    class: &crate::classloading::Class,
    watermark: Option<usize>,
) -> bool {
    // Widening: a class id is a `u32` store slot.
    watermark.is_some_and(|w| class.id.as_u32() as usize >= w)
        && shared.debug.debugger_gates.class_prepare_armed()
}

/// The exception classes HotSpot 25's `Threads::initialize_java_lang_classes`
/// initializes at boot, before any debugger can attach, so their chains
/// (`Throwable`, `Exception`, `RuntimeException`, `Error`, ...) are always
/// prepared there.
const HOTSPOT_BOOT_INITIALIZED_EXCEPTIONS: &[&str] = &[
    "java/lang/OutOfMemoryError",
    "java/lang/NullPointerException",
    "java/lang/ClassCastException",
    "java/lang/ArrayStoreException",
    "java/lang/ArithmeticException",
    "java/lang/ArrayIndexOutOfBoundsException",
    "java/lang/StackOverflowError",
    "java/lang/IllegalMonitorStateException",
    "java/lang/IllegalArgumentException",
    "java/lang/InternalError",
];

/// Interpreter round i7 (final JDI battery, `L1W25JdiStopMonitors --nojit`:
/// `classesByName("java.lang.RuntimeException")` was empty): this VM loads
/// [`HOTSPOT_BOOT_INITIALIZED_EXCEPTIONS`] at boot but leaves them `Loaded`
/// until a throw needs one. With the JIT on, a fast-throw or `new` row records
/// their chains ([`note_chain_used_unlinked`]) early; under `--nojit` nothing
/// did, so with a `ClassPrepare` request in force
/// [`never_used_while_class_prepare_observed`] hid them, where HotSpot lists
/// them. Record their chains as used when the watermark is first set.
fn note_hotspot_boot_initialized_classes_used(
    shared: &crate::vm::SharedVm,
    cm: &crate::classloading::ClassManager,
) {
    for name in HOTSPOT_BOOT_INITIALIZED_EXCEPTIONS {
        if let Some(id) =
            cm.find_class_by_name_for_loader(name, crate::classloading::ClassLoaderId::Bootstrap)
        {
            note_chain_used_unlinked(shared, cm, id);
        }
    }
}

/// Keep [`DebugState::class_prepare_watermark`] (interpreter round i5 wave
/// 104, lane L1): set to the class store's slot count when a JDWP
/// `ClassPrepare` request is first found in force, cleared once none is.
/// The slot count is read after the request was armed, so a class defined
/// between the two is counted as defined before it (listed: the safe side).
/// Called by the server after every command, with no lock held; one load and
/// one lock when nothing changes. Lock order: the class manager, released,
/// then the debug state.
fn note_class_prepare_watermark(shared: &crate::vm::SharedVm) {
    let armed = shared.debug.debugger_gates.class_prepare_armed();
    {
        let mut ds = shared.debug.debug_state.lock();
        if !armed {
            ds.class_prepare_watermark = None;
            return;
        }
        if ds.class_prepare_watermark.is_some() || !ds.hide_unprepared_classes {
            return;
        }
    }
    let slots = {
        let cm = shared.classes.class_manager.read();
        note_hotspot_boot_initialized_classes_used(shared, &cm);
        cm.class_store.slot_count()
    };
    let mut ds = shared.debug.debug_state.lock();
    if ds.class_prepare_watermark.is_none() && shared.debug.debugger_gates.class_prepare_armed() {
        ds.class_prepare_watermark = Some(slots);
    }
}

/// Refresh [`DebugState::class_statuses`] for every class the session knows
/// (wave 15). Before the commands that answer a class's status
/// ([`reads_class_status`]). Takes the class-manager lock, then the
/// debug-state lock. Wave 103: and [`DebugState::unprepared_classes`].
fn refresh_class_statuses(shared: &crate::vm::SharedVm) {
    let cm = shared.classes.class_manager.read();
    let mut ds = shared.debug.debug_state.lock();
    let hide = ds.hide_unprepared_classes;
    let watermark = ds.class_prepare_watermark;
    let mut unprepared: HashSet<u64> = HashSet::new();
    let statuses: HashMap<u64, u32> = ds
        .class_signatures
        .keys()
        .filter_map(|&id| {
            let raw = u32::try_from(id).ok()?;
            match cm.class_store.get(crate::classloading::ClassId::new(raw)) {
                Some(class) => {
                    if hide && jdwp_class_unprepared(shared, &cm, class, watermark) {
                        unprepared.insert(id);
                    }
                    Some((id, jdwp_class_status(class)))
                }
                // A lambda proxy class (the i54-L1 retirement) is never in
                // the store: initialized since its call site was linked.
                None if raw >= crate::vm::realms::class_realm::LAMBDA_PROXY_ID_BASE => {
                    Some((id, LAMBDA_PROXY_CLASS_STATUS))
                }
                None => None,
            }
        })
        .collect();
    ds.class_statuses = statuses;
    ds.unprepared_classes = unprepared;
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
    use commands::{
        CMD_RT_FIELDS, CMD_RT_FIELDS_WITH_GENERIC, CMD_RT_INTERFACES, CMD_RT_METHODS,
        CMD_RT_METHODS_WITH_GENERIC, CMD_RT_STATUS, CMD_VM_CLASSES_BY_SIGNATURE, CS_REF_TYPE,
        CS_VM,
    };
    let named: Vec<u64> = match (command_set, command) {
        // Wave 103 (interpreter round i5, lane L1): the member listings too,
        // which refuse an unprepared class (`commands::read_prepared_class`).
        (
            CS_REF_TYPE,
            CMD_RT_STATUS
            | CMD_RT_FIELDS
            | CMD_RT_FIELDS_WITH_GENERIC
            | CMD_RT_METHODS
            | CMD_RT_METHODS_WITH_GENERIC
            | CMD_RT_INTERFACES,
        ) => match protocol::PayloadReader::new(data).read_u64_be() {
            Ok(id) => vec![id],
            Err(_) => return,
        },
        (CS_VM, CMD_VM_CLASSES_BY_SIGNATURE) => {
            let Ok(signature) = protocol::PayloadReader::new(data).read_string() else {
                return;
            };
            // Interpreter round i5 wave 103 hand-back (lane L1;
            // `L1W103JdiInstances` on w103b: `classesByName("Item[]")` was
            // empty): this VM makes an array class only on demand, so an
            // array signature's classes are made first, as
            // `VirtualMachine.VisibleClasses` makes the primitive ones.
            if signature.starts_with('[') {
                inspect::materialize_array_classes(shared, &signature);
            }
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
    let hide = ds.hide_unprepared_classes;
    for id in named {
        let class = u32::try_from(id)
            .ok()
            .and_then(|raw| cm.class_store.get(crate::classloading::ClassId::new(raw)));
        if let Some(class) = class {
            ds.class_statuses.insert(id, jdwp_class_status(class));
            // Wave 103: `ClassesBySignature` leaves an unprepared class out.
            if hide && jdwp_class_unprepared(shared, &cm, class, ds.class_prepare_watermark) {
                ds.unprepared_classes.insert(id);
            } else {
                ds.unprepared_classes.remove(&id);
            }
        }
    }
}

/// The commands that answer a class's status, before which the server
/// refreshes it ([`refresh_class_statuses`], wave 15):
/// `VirtualMachine.ClassesBySignature` and `AllClassesWithGeneric`, and
/// (wave 23) `ReferenceType.Status`.
fn reads_class_status(command_set: u8, command: u8) -> bool {
    use commands::{
        CMD_RT_FIELDS, CMD_RT_FIELDS_WITH_GENERIC, CMD_RT_INTERFACES, CMD_RT_METHODS,
        CMD_RT_METHODS_WITH_GENERIC, CMD_RT_STATUS, CMD_VM_ALL_CLASSES_WITH_GENERIC,
        CMD_VM_CLASSES_BY_SIGNATURE, CS_REF_TYPE, CS_VM,
    };
    matches!(
        (command_set, command),
        (
            CS_VM,
            CMD_VM_CLASSES_BY_SIGNATURE | CMD_VM_ALL_CLASSES_WITH_GENERIC
        ) | (
            CS_REF_TYPE,
            CMD_RT_STATUS
                | CMD_RT_FIELDS
                | CMD_RT_FIELDS_WITH_GENERIC
                | CMD_RT_METHODS
                | CMD_RT_METHODS_WITH_GENERIC
                | CMD_RT_INTERFACES
        )
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
            let synthetic = flags & ACC_SYNTHETIC != 0 || has_synthetic_attribute(&m.attributes);
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
                            class.constant_pool.get_utf8(e.signature_index)?.to_string(),
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
pub(crate) const JVM_RECOGNIZED_FIELD_MODIFIERS: u32 = 0x50DF;
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
pub(crate) fn has_synthetic_attribute(
    attributes: &[cratonvm_reader::attribute::LazyAttribute],
) -> bool {
    attributes.iter().any(|a| {
        matches!(
            a.as_decoded(),
            Some(cratonvm_reader::attribute::Attribute::Synthetic)
        )
    })
}

/// A member's `Signature` attribute (its generic signature), if any.
pub(crate) fn signature_attribute(
    attributes: &[cratonvm_reader::attribute::LazyAttribute],
) -> Option<String> {
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
/// it answered `0x411`. Interpreter round i1 wave 52 (lane L1): an interface
/// has `ACC_SUPER` too, although its class file may not set it: HotSpot
/// 25.0.3's `GetClassModifiers` answers `0x621` for `Runnable` and `0x628`
/// for a nested `static interface` (measured,
/// `tools/probes/interp/L1/L1W52JvmtiThreadAndClassFacts.java`; HotSpot
/// treats every class as `ACC_SUPER`); it answered `0x601` / `0x608`.
pub(crate) fn jdwp_class_modifiers(class: &crate::classloading::Class) -> u32 {
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
    modifiers | ACC_SUPER
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
            .map(|(tid, _)| {
                (
                    tid.0,
                    jdwp_thread_status_with(registry, *tid, &invokes_sleep),
                )
            })
            .collect()
    };
    // Wave 49 (lane L1): the virtual threads, which `AllThreads` leaves out
    // (`commands::listed_thread_ids`), also those started before the
    // debugger attached (no `ThreadStart` recorded them). Read before the
    // debug-state lock, as the manager's lock is its own.
    let manager = &shared.threads.virtual_thread_manager;
    let virtual_ids: Vec<u64> = names
        .iter()
        .map(|(tid, _)| tid.0)
        .filter(|&tid| manager.is_virtual(tid))
        .collect();
    let mut ds = shared.debug.debug_state.lock();
    ds.virtual_threads.extend(virtual_ids);
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
    // Interpreter round i5 wave 104 (lane L1): once `main` has ended, its id
    // names the ended `main` (`ids::ENDED_MAIN_VM_ID`): its name, ZOMBIE,
    // which `AllThreads` leaves out, as HotSpot answers a terminated thread.
    if let Some(name) = ds.destroy_java_vm_split.clone() {
        ds.thread_names.insert(ids::ENDED_MAIN_VM_ID, name);
        ds.thread_statuses
            .insert(ids::ENDED_MAIN_VM_ID, commands::THREAD_STATUS_ZOMBIE);
    }
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
        // Interpreter round i2 wave 63, lane L1 (`CRATONVM_PUBLISHED_WAIT_KIND`):
        // the sleep's own region says whether it is a sleep, as HotSpot's
        // `JavaThreadSleepState` does, whatever frame entered it.
        3 if registry.published_wait_kind() => {
            if registry.published_sleeping(tid) == Some(true) {
                THREAD_STATUS_SLEEPING
            } else {
                THREAD_STATUS_WAIT
            }
        }
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
pub(crate) fn is_thread_sleep_frame(frame: &cratonvm_native_api::StackTraceEntry) -> bool {
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
pub(crate) fn frame_invokes_thread_sleep(
    cm: &crate::classloading::ClassManager,
    entry: &cratonvm_native_api::StackTraceEntry,
) -> bool {
    use cratonvm_reader::constant_pool::ConstantPoolEntry;
    let (Some(class_id), Ok(bci)) = (entry.class_id, usize::try_from(entry.byte_code_index)) else {
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
#[cfg_attr(not(test), allow(dead_code))]
fn ids_from_wire(command_set: u8, command: u8, data: &mut [u8]) {
    ids_from_wire_split(command_set, command, data, false);
}

/// [`ids_from_wire`] with the `DestroyJavaVM` split (`split`; interpreter
/// round i5 wave 104, lane L1): a thread id decodes through
/// `ids::thread_from_wire_split`. What the server runs.
fn ids_from_wire_split(command_set: u8, command: u8, data: &mut [u8], split: bool) {
    use commands::{
        CMD_CT_INVOKE_METHOD, CMD_CT_NEW_INSTANCE, CMD_IT_INVOKE_METHOD, CMD_OR_INVOKE_METHOD,
        CS_ARRAY_TYPE, CS_CLASS_TYPE, CS_INTERFACE_TYPE, CS_METHOD, CS_OBJECT_REF, CS_REF_TYPE,
        CS_STACK_FRAME, CS_THREAD_REF,
    };
    fn rewrite(data: &mut [u8], at: usize, decode: &dyn Fn(u64) -> u64) {
        let Some(bytes) = data.get_mut(at..at + 8) else {
            return;
        };
        let mut raw = [0u8; 8];
        raw.copy_from_slice(bytes);
        bytes.copy_from_slice(&decode(u64::from_be_bytes(raw)).to_be_bytes());
    }
    let thread = |wire: u64| ids::thread_from_wire_split(split, wire);
    match command_set {
        CS_THREAD_REF | CS_STACK_FRAME => rewrite(data, 0, &thread),
        CS_REF_TYPE | CS_CLASS_TYPE | CS_ARRAY_TYPE | CS_INTERFACE_TYPE | CS_METHOD => {
            rewrite(data, 0, &ids::class_from_wire)
        }
        _ => {}
    }
    match (command_set, command) {
        (CS_CLASS_TYPE, CMD_CT_INVOKE_METHOD | CMD_CT_NEW_INSTANCE)
        | (CS_INTERFACE_TYPE, CMD_IT_INVOKE_METHOD) => rewrite(data, 8, &thread),
        (CS_OBJECT_REF, CMD_OR_INVOKE_METHOD) => {
            rewrite(data, 8, &thread);
            rewrite(data, 16, &ids::class_from_wire);
        }
        _ => {}
    }
}

/// `INVALID_OBJECT` for a command whose leading thread or reference-type id
/// names nothing at all (interpreter round i1 wave 43, lane L1;
/// `docs/feature-designs/jvmti-delivery-threading.md`).
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
        // Wave 104 (lane L1): with the `DestroyJavaVM` split.
        CS_THREAD_REF | CS_STACK_FRAME => ids::thread_to_wire_split(
            shared.debug.debugger_gates.destroy_java_vm_split(),
            vm_id,
        ),
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
    let ds = shared.debug.debug_state.lock();
    // Wave 104 (lane L1): with the `DestroyJavaVM` split.
    let thread = ds.vm_thread(wire);
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
        // Interpreter round i5 wave 103 (lane L1).
        (commands::CS_REF_TYPE, commands::CMD_RT_CONSTANT_POOL) => {
            Some(constant_pool(shared, data))
        }
        // Interpreter round i5 wave 104 (lane L1): `canRedefineClasses`.
        (commands::CS_VM, commands::CMD_VM_REDEFINE_CLASSES) => {
            Some(redefine_classes(shared, data))
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
/// `docs/feature-designs/jvmti-delivery-threading.md`):
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
    let (spec, java_version, vm_name, vm_info) = {
        let props = shared.system_properties.read();
        let get = |key: &str| props.get(key).cloned();
        (
            get("java.specification.version"),
            get("java.version"),
            get("java.vm.name"),
            get("java.vm.info"),
        )
    };
    let (major, minor) = spec.as_deref().and_then(jdwp_version_of).unwrap_or((1, 8));
    let java_version = java_version.unwrap_or_else(|| "1.8.0".to_string());
    // Interpreter round i6 wave 118, lane L1 (i61-L1 stage 2): the identity
    // table's name without a property map, not a literal of its own.
    let vm_name = vm_name.unwrap_or_else(|| {
        cratonvm_types::vm_properties::vm_identity_value("java.vm.name")
            .unwrap_or_default()
            .to_string()
    });
    let vm_info = vm_info.unwrap_or_default();
    let mut pw = protocol::PayloadWriter::new();
    // Wave 118: the back end's own description (`VirtualMachineImpl.c`
    // `version()`, MEASURED on HotSpot 25.0.3, `L1W118VmIdentityAnswers`):
    // the JDWP version, the JVMTI version (JDK 9+ numbers it as the JDK,
    // the same pair) and `java.version (java.vm.name, java.vm.info)`. It
    // read "CratonVM JDWP Debug Server, ..." without the JVMTI line or
    // `java.vm.info`.
    pw.put_string(&format!(
        "Java Debug Wire Protocol (Reference Implementation) version {major}.{minor}\n\
         JVM Debug Interface version {major}.{minor}\n\
         JVM version {java_version} ({vm_name}, {vm_info})"
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
    let is_virtual = started_virtual || shared.threads.virtual_thread_manager.is_virtual(tid);
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
        drop(cm);
        return lambda_proxy_bytecodes(shared, crate::classloading::ClassId::new(raw), method_id);
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

/// [`method_bytecodes`] of a lambda proxy class's method (the i54-L1
/// retirement): the code `InnerClassLambdaMetafactory` spins for it, as the C
/// JVMTI table's `GetBytecodes` answers it
/// (`runtime::lambda_proxy::lambda_proxy_code`); `INVALID_CLASS` for an id
/// naming no proxy, `INVALID_METHODID` for one naming none of its methods.
fn lambda_proxy_bytecodes(
    shared: &crate::vm::SharedVm,
    id: crate::classloading::ClassId,
    method_id: u64,
) -> commands::CommandResult {
    use commands::{
        CommandResult, ERR_ABSENT_INFORMATION, ERR_INVALID_CLASS, ERR_INVALID_METHODID,
    };
    let Some(facts) = crate::runtime::lambda_proxy::lambda_proxy_facts(shared, id) else {
        return CommandResult::error(ERR_INVALID_CLASS);
    };
    let Some(index) = facts
        .methods
        .iter()
        .position(|m| jdwp_method_id(&m.name, &m.descriptor) == method_id)
    else {
        return CommandResult::error(ERR_INVALID_METHODID);
    };
    let Some(code) = crate::runtime::lambda_proxy::lambda_proxy_code(shared, id, index) else {
        return CommandResult::error(ERR_ABSENT_INFORMATION);
    };
    let mut pw = protocol::PayloadWriter::new();
    pw.put_u32_be(u32::try_from(code.len()).unwrap_or(0));
    pw.put_bytes(&code);
    CommandResult::ok(pw.into_bytes())
}

/// `ReferenceType.SourceDebugExtension` (2/12, interpreter round i1 wave 43,
/// lane L1; stage 1 of
/// `docs/feature-designs/jvmti-delivery-threading.md`):
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
            // A lambda proxy class has none (the i54-L1 retirement).
            if shared.classes.is_lambda_proxy_class(id) {
                return CommandResult::error(ERR_ABSENT_INFORMATION);
            }
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
pub(crate) fn defined_class_file(
    cm: &crate::classloading::ClassManager,
    id: crate::classloading::ClassId,
    name: &str,
) -> Option<Vec<u8>> {
    if let Some(cached) = cm.defined_class_bytes(id) {
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
pub(crate) fn source_debug_extension_of(class_file: &[u8]) -> Option<Vec<u8>> {
    use cratonvm_reader::attribute::{Attribute, LazyAttribute};
    let parsed = cratonvm_reader::class_reader::read_class(class_file).ok()?;
    let lazy = parsed
        .attributes
        .iter()
        .find(|a| a.name() == "SourceDebugExtension")?;
    match lazy {
        LazyAttribute::Raw(raw) => raw.source.get(raw.range.clone()).map(<[u8]>::to_vec),
        LazyAttribute::Decoded(Attribute::Unknown { data, .. }) => Some(data.to_vec()),
        LazyAttribute::Decoded(_) => None,
    }
}

/// The constant pool of class file `bytes`: its `constant_pool_count` and
/// its entries' bytes, in class-file order. `None` for bytes that are not a
/// well-formed pool. What JVMTI `GetConstantPool` and JDWP
/// `ReferenceType.ConstantPool` answer (HotSpot reconstitutes the pool of
/// the class file the class was defined from; MEASURED for a plain class,
/// `jvmti::native_env::agent_functions::get_constant_pool`).
pub(crate) fn class_file_constant_pool(bytes: &[u8]) -> Option<(u16, &[u8])> {
    let count = u16::from_be_bytes([*bytes.get(8)?, *bytes.get(9)?]);
    let start = 10;
    let mut pos = start;
    let mut index = 1;
    while index < count {
        let tag = *bytes.get(pos)?;
        let width = match tag {
            1 => {
                3 + usize::from(u16::from_be_bytes([
                    *bytes.get(pos + 1)?,
                    *bytes.get(pos + 2)?,
                ]))
            }
            3 | 4 | 9 | 10 | 11 | 12 | 17 | 18 => 5,
            5 | 6 => {
                index += 1;
                9
            }
            7 | 8 | 16 | 19 | 20 => 3,
            15 => 4,
            _ => return None,
        };
        pos += width;
        index += 1;
    }
    Some((count, bytes.get(start..pos)?))
}

/// `ReferenceType.ConstantPool` (2/18, interpreter round i5 wave 103, lane
/// L1): `count` (the class file's `constant_pool_count`) and the pool's
/// bytes, as HotSpot's back end answers it from JVMTI `GetConstantPool`;
/// `ABSENT_INFORMATION` for an array class and where this VM no longer has
/// the class file; a lambda proxy class's pool as its spun class holds it.
/// `canGetConstantPool` answers true since the same wave.
fn constant_pool(shared: &crate::vm::SharedVm, data: &[u8]) -> commands::CommandResult {
    use commands::{CommandResult, ERR_ABSENT_INFORMATION, ERR_INTERNAL, ERR_INVALID_CLASS};
    let Ok(class_id) = protocol::PayloadReader::new(data).read_u64_be() else {
        return CommandResult::error(ERR_INTERNAL);
    };
    let Ok(raw) = u32::try_from(class_id) else {
        return CommandResult::error(ERR_INVALID_CLASS);
    };
    let id = crate::classloading::ClassId::new(raw);
    let stored: Option<Option<(u16, Vec<u8>)>> = {
        let cm = shared.classes.class_manager.read();
        cm.class_store.get(id).map(|class| {
            if class.name.starts_with('[') {
                return None;
            }
            defined_class_file(&cm, id, &class.name).and_then(|file| {
                class_file_constant_pool(&file).map(|(count, pool)| (count, pool.to_vec()))
            })
        })
    };
    let pool = match stored {
        Some(pool) => pool,
        // A lambda proxy class (never in the store): its spun class's pool.
        None if shared.classes.is_lambda_proxy_class(id) => {
            crate::runtime::lambda_proxy::lambda_proxy_spun_class_of(shared, id)
                .map(|spun| spun.constant_pool())
        }
        None => return CommandResult::error(ERR_INVALID_CLASS),
    };
    let Some((count, bytes)) = pool else {
        return CommandResult::error(ERR_ABSENT_INFORMATION);
    };
    let mut pw = protocol::PayloadWriter::new();
    pw.put_u32_be(u32::from(count));
    pw.put_u32_be(u32::try_from(bytes.len()).unwrap_or(0));
    pw.put_bytes(&bytes);
    CommandResult::ok(pw.into_bytes())
}

/// The JDWP error HotSpot's back end answers for the JVMTI error `jvmti` of
/// a refused `RedefineClasses` (`util.c` `map2jdwpError`): the redefinition
/// errors 60-72 keep their numbers (JDWP `INVALID_CLASS_FORMAT` ..
/// `CLASS_ATTRIBUTE_CHANGE_NOT_IMPLEMENTED`), `UNSUPPORTED_OPERATION` (73)
/// and `UNMODIFIABLE_CLASS` (79) are `NOT_IMPLEMENTED`, anything else
/// `INTERNAL` (interpreter round i5 wave 104, lane L1; derived from the
/// back end's source, not measured).
fn jdwp_redefinition_error(jvmti: i32) -> u16 {
    match jvmti {
        // Cast: in 60..=72.
        60..=72 => jvmti as u16,
        73 | 79 => commands::ERR_NOT_IMPLEMENTED,
        _ => commands::ERR_INTERNAL,
    }
}

/// `VirtualMachine.RedefineClasses` (1/18, interpreter round i5 wave 104,
/// lane L1; `canRedefineClasses`, true since the same wave): `classes`, then
/// per class its `refType` and `classfile` (`int` length, bytes). As
/// HotSpot's back end (`VirtualMachineImpl.c` `redefineClasses`): no class is
/// success; a class id naming nothing is `INVALID_OBJECT`, naming no class
/// `INVALID_CLASS`; an empty class file `INVALID_CLASS_FORMAT`; then JVMTI
/// `RedefineClasses` of all of them at once, here this VM's (the C JVMTI
/// table's path, `instrument::redefine_classes_for_jvmti`: the class files
/// through the `ClassFileLoadHook` chain, then checked and installed
/// together), whose refusal answers its JDWP error
/// ([`jdwp_redefinition_error`]: a field added is
/// `SCHEMA_CHANGE_NOT_IMPLEMENTED`, a method added
/// `ADD_METHOD_NOT_IMPLEMENTED`; `canAddMethod` and
/// `canUnrestrictedlyRedefineClasses` stay false, as on HotSpot). Run on a
/// registered mutator (a parked thread, else the heap service,
/// [`run_on_parked_thread`]), as making and installing classes allocates;
/// HotSpot runs it on its JDWP thread with no thread suspended. A redefined
/// class's members, line and variable tables are described again
/// ([`add_class_metadata`]), as JDI re-reads them after
/// `noticeRedefineClass`.
fn redefine_classes(shared: &crate::vm::SharedVm, data: &[u8]) -> commands::CommandResult {
    use commands::{
        CommandResult, ERR_INTERNAL, ERR_INVALID_CLASS, ERR_INVALID_OBJECT, ERR_NOT_IMPLEMENTED,
    };
    /// JDWP `INVALID_CLASS_FORMAT`.
    const ERR_INVALID_CLASS_FORMAT: u16 = 60;
    let mut r = protocol::PayloadReader::new(data);
    let Ok(count) = r.read_u32_be() else {
        return CommandResult::error(ERR_INTERNAL);
    };
    // Cast: a JDWP int; a negative count names no class.
    let count = count as i32;
    if count <= 0 {
        return CommandResult::ok(Vec::new());
    }
    let mut classes: Vec<(crate::classloading::ClassId, Vec<u8>)> = Vec::new();
    for _ in 0..count {
        let Ok(wire) = r.read_u64_be() else {
            return CommandResult::error(ERR_INTERNAL);
        };
        let class = ids::class_from_wire(wire);
        let stored = u32::try_from(class).ok().map(crate::classloading::ClassId::new);
        let in_store = stored.is_some_and(|id| {
            shared
                .classes
                .class_manager
                .read()
                .class_store
                .get(id)
                .is_some()
        });
        let lambda = stored.is_some_and(|id| shared.classes.is_lambda_proxy_class(id));
        if !in_store && !lambda {
            // An id of another kind (a thread, an object) is `INVALID_CLASS`;
            // one naming nothing `INVALID_OBJECT` (`inStream_readClassRef`).
            let names_something = id_names_something(shared, wire);
            return CommandResult::error(if names_something {
                ERR_INVALID_CLASS
            } else {
                ERR_INVALID_OBJECT
            });
        }
        let Ok(length) = r.read_u32_be() else {
            return CommandResult::error(ERR_INTERNAL);
        };
        // Cast: a JDWP int.
        let length = length as i32;
        if length <= 0 {
            return CommandResult::error(ERR_INVALID_CLASS_FORMAT);
        }
        let Ok(bytes) = r.read_bytes(length as usize) else {
            return CommandResult::error(ERR_INTERNAL);
        };
        let Some(id) = stored else {
            return CommandResult::error(ERR_INTERNAL);
        };
        classes.push((id, bytes.to_vec()));
    }
    let ids: Vec<crate::classloading::ClassId> = classes.iter().map(|(id, _)| *id).collect();
    let outcome = run_on_parked_thread(shared, None, move |shared, thread| {
        let mut ctx = crate::vm::NativeContextImpl { shared, thread };
        if classes
            .iter()
            .any(|&(id, _)| !crate::runtime::instrument::class_id_is_modifiable(&ctx, id))
        {
            return Err(ERR_NOT_IMPLEMENTED);
        }
        match crate::runtime::instrument::redefine_classes_for_jvmti(&mut ctx, &classes) {
            None => Ok(()),
            Some(refusal) => {
                tracing::warn!("JDWP RedefineClasses: {}", refusal.detail);
                Err(jdwp_redefinition_error(
                    crate::jvmti::native_env::redefinition_refusal_error(&refusal),
                ))
            }
        }
    });
    match outcome {
        Ok(Ok(())) => {
            let cm = shared.classes.class_manager.read();
            let mut ds = shared.debug.debug_state.lock();
            for id in ids {
                if let Some(class) = cm.class_store.get(id) {
                    let _ = add_class_metadata(&mut ds, class);
                }
                // As the back end's `eventHandler_freeClassBreakpoints`: the
                // class's breakpoints go (JDI deletes its requests too). The
                // server republishes the gates after the command.
                let _ = ds.events.clear_class_breakpoints(u64::from(id.as_u32()));
            }
            CommandResult::ok(Vec::new())
        }
        Ok(Err(code)) => CommandResult::error(code),
        Err(_) => CommandResult::error(ERR_INTERNAL),
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
    let separator = separator
        .chars()
        .next()
        .unwrap_or(if cfg!(windows) { ';' } else { ':' });
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
    if !wanted {
        return;
    }
    if !crate::runtime::interpreter::blocked_frames_readable(shared, tid) {
        // Wave 48 (lane L1): an unmounted virtual thread (a continuation that
        // yielded in a sleep, a park or a keyed wait) has no window; its
        // frames sit in the virtual-thread manager.
        if shared.threads.virtual_thread_manager.is_virtual(tid)
            && run_on_parked_thread(shared, None, move |shared, _thread| {
                crate::runtime::interpreter::publish_frames_of_unmounted_virtual_thread(shared, tid)
            })
            .unwrap_or(false)
        {
            return;
        }
        // Wave 56 (lane L6): a thread suspended while it runs a native
        // outside a blocking region is read through its native frames
        // window, now or once it opens it on its way into the native.
        if publish_native_frames_for(shared, tid) {
            return;
        }
        await_suspend_point_frames(shared, tid);
        if publish_native_frames_for(shared, tid) {
            return;
        }
        // It blocked on the way, suspended (a blocking region's entry
        // publishes then only for a suspension it sees first).
        if !await_inspection_window(shared, tid) {
            return;
        }
    }
    let _ = run_on_parked_thread(shared, None, move |shared, _thread| {
        crate::runtime::interpreter::publish_frames_of_blocked_thread(shared, tid)
    });
}

/// Read suspended thread `tid`'s frames through its native frames window
/// on a registered mutator, when that window is open (interpreter round i2
/// wave 56, lane L6, [`DebuggerGates::native_frames_window`]); answers
/// whether a listing was published.
///
/// The i53 retirement: while a stop-the-world pause is requested, which waits
/// for the thread in its native and holds every registered mutator until it
/// returns, this thread reads the window itself first
/// (`interpreter::publish_frames_of_native_thread_in_stop`).
fn publish_native_frames_for(shared: &crate::vm::SharedVm, tid: u64) -> bool {
    if !crate::runtime::interpreter::native_frames_readable(shared, tid) {
        return false;
    }
    if shared.mem.gc_barrier.stop_requested()
        && crate::runtime::interpreter::publish_frames_of_native_thread_in_stop(shared, tid)
    {
        return true;
    }
    run_on_parked_thread(shared, None, move |shared, _thread| {
        crate::runtime::interpreter::publish_frames_of_native_thread(shared, tid)
    })
    .unwrap_or(false)
}

/// Whether suspended thread `tid`, which has no listing of this suspension,
/// can be read through its inspection window now; waits a bounded while
/// ([`SUSPEND_POINT_WAIT`]) for a thread that is on its way INTO a blocking
/// wait to open it (interpreter round i1 wave 50, lane L1).
///
/// `Object.wait` (and a blocking region) stores the thread's Java state --
/// what `ThreadReference.Status` answers (`WAIT`, `SLEEPING`) -- and marks it
/// blocked BEFORE it opens its window (`vm_exec` `monitor_wait_with_state`:
/// state, deposit, `enter_blocked`, then `open_blocked_inspection`). A
/// debugger that waited for `WAIT`, suspended the thread and asked for its
/// frames inside that gap found the window closed and the thread not running
/// (no suspend point to wait for), and the reply listed no frames at all: JDI
/// `frame(0)` threw. Seen once in 20 host runs of `L1W43JdiForceEarlyReturn`
/// under `--compatible` (its `== refused` section suspends a thread as soon as
/// it reads `WAIT`); HotSpot's `Frames` of a suspended waiting thread always
/// lists its `Object.wait` frame. `CRATONVM_FRAME_TRACE=1` prints
/// `[AWAIT_WINDOW] tid=<n> readable=<bool> waited_ms=<n>` when it waited.
fn await_inspection_window(shared: &crate::vm::SharedVm, tid: u64) -> bool {
    let start = std::time::Instant::now();
    let mut waited = false;
    let readable = loop {
        {
            let ds = shared.debug.debug_state.lock();
            if !ds.is_thread_suspended(tid) || ds.has_current_frames(tid) {
                break false;
            }
        }
        if crate::runtime::interpreter::blocked_frames_readable(shared, tid) {
            break true;
        }
        // Only a thread blocked, or about to block, opens a window: one that
        // runs Java parks at its suspend point instead (and was waited for by
        // `await_suspend_point_frames`), a dead one never opens one.
        let coming = shared
            .threads
            .thread_registry
            .is_blocked(crate::threading::ThreadId(tid))
            || matches!(
                commands::thread_status_of(&shared.debug.debug_state.lock(), tid),
                Some(
                    commands::THREAD_STATUS_WAIT
                        | commands::THREAD_STATUS_SLEEPING
                        | commands::THREAD_STATUS_MONITOR
                )
            );
        if !coming || start.elapsed() >= SUSPEND_POINT_WAIT {
            break false;
        }
        waited = true;
        await_debug_state_poll(shared);
    };
    if waited && crate::runtime::env_cache::frame_trace() {
        eprintln!(
            "[AWAIT_WINDOW] tid={tid} readable={readable} waited_ms={}",
            start.elapsed().as_millis()
        );
    }
    readable
}

/// Wait, a bounded while, for suspended thread `tid` -- running Java when it
/// was suspended, so neither blocked nor parked yet -- to reach its suspend
/// point and publish its frames (interpreter round i1 wave 48, lane L1).
/// `VirtualMachine.Suspend` / `ThreadReference.Suspend` only count the
/// suspension here; the thread parks at its next suspend point. HotSpot's
/// suspension completes (a handshake) before its reply, so a `Frames` sent
/// right after it lists the thread's frames; this VM's answered no frames
/// at all for the thread still on its way (JDI: `frame(0)` threw
/// `IndexOutOfBoundsException`; seen with `L1W48JdiCompatibleBlockingBytecode`,
/// whose sleeper is suspended between two sleeps). A thread blocked in a
/// region (its window closed) or one that never reaches a suspend point (a
/// native that does not block through a region) is waited for at most
/// [`SUSPEND_POINT_WAIT`]. `CRATONVM_FRAME_TRACE=1` prints
/// `[AWAIT_SUSPEND_POINT] tid=<n> published=<bool> waited_ms=<n>`.
fn await_suspend_point_frames(shared: &crate::vm::SharedVm, tid: u64) {
    let registry = &shared.threads.thread_registry;
    if registry.is_blocked(crate::threading::ThreadId(tid)) {
        return;
    }
    let start = std::time::Instant::now();
    let mut nudges = 0u32;
    let published = loop {
        {
            let ds = shared.debug.debug_state.lock();
            // Only a RUNNING thread is on its way to a suspend point (a dead
            // one never gets there; a sleeping or waiting one publishes
            // through its window or its deposit).
            if !ds.is_thread_suspended(tid)
                || commands::thread_status_of(&ds, tid) != Some(commands::THREAD_STATUS_RUNNING)
            {
                return;
            }
            if ds.has_current_frames(tid) {
                break true;
            }
        }
        if start.elapsed() >= SUSPEND_POINT_WAIT
            || registry.is_blocked(crate::threading::ThreadId(tid))
            // Wave 56 (lane L6): in a native, readable through its window.
            || crate::runtime::interpreter::native_frames_readable(shared, tid)
        {
            break false;
        }
        // Wave 53 (lane L6): as the suspension's own wait does.
        if suspend_nudge_due(start.elapsed(), nudges) {
            nudges += 1;
            crate::runtime::interpreter::request_compiled_loop_exits_again(shared);
        }
        await_debug_state_poll(shared);
    };
    if crate::runtime::env_cache::frame_trace() {
        eprintln!(
            "[AWAIT_SUSPEND_POINT] tid={tid} published={published} waited_ms={} nudges={nudges}",
            start.elapsed().as_millis()
        );
    }
}

/// Thread `tid` published its listing (under the debug-state lock `ds`):
/// if the settle wait in progress counts it
/// ([`DebuggerGates::settle_pending_counter`], interpreter round i6 wave
/// 117, lane L1), hand it to the wait's next round. One empty-set test
/// otherwise (the set is empty outside a wait and with the switch off).
#[inline]
pub(crate) fn note_settle_arrival(ds: &mut DebugState, tid: u64) {
    if !ds.settle_wanted.is_empty() && ds.settle_wanted.remove(&tid) {
        ds.settle_arrived.push(tid);
    }
}

/// [`await_suspension_settled`]'s state with
/// [`DebuggerGates::settle_pending_counter`] (interpreter round i6 wave 117,
/// lane L1): the threads still on their way, the laggards among those its
/// one walk found, and when the pending threads were last re-walked.
struct CountedSettle {
    pending: Vec<u64>,
    laggards: Vec<u64>,
    walked_at: std::time::Instant,
}

/// Is suspended thread `tid` still on its way to a safe place, as far as the
/// debug state tells (interpreter round i1 wave 49, lane L1;
/// [`await_suspension_settled`])? Not a dead one, and not one whose listing
/// of this suspension stands (it parked at its suspend point, or an event
/// published it). Whether it is blocked is the registry's answer, read fresh
/// by the caller.
///
/// Interpreter round i2 wave 53, lane L6: no longer "only a thread whose
/// status reads `RUNNING`". That status is the thread table's, refreshed
/// before the commands that name a thread ([`refreshes_thread_names`]) --
/// not before `VirtualMachine.Suspend` nor a `SUSPEND_ALL` event set, and
/// never during the wait -- so a thread that read `SLEEPING` or `WAIT` at the
/// last refresh and has woken since was taken for stopped, and ran Java after
/// the reply. HotSpot's handshake stops it.
fn suspension_on_its_way(ds: &DebugState, tid: u64) -> bool {
    ds.is_thread_suspended(tid)
        && commands::thread_status_of(ds, tid)
            .is_some_and(|status| status != commands::THREAD_STATUS_ZOMBIE)
        && !ds.has_current_frames(tid)
}

/// Wait, bounded, for the threads a JDWP suspension just covered -- `only`,
/// or every thread the server knows -- to be stopped where a debugger may
/// read them, before the suspension's reply or event set goes out
/// (interpreter round i1 wave 49, lane L1; stage 1 of
/// `docs/feature-designs/jvmti-delivery-threading.md`,
/// `CRATONVM_DEBUG_SUSPEND_HANDSHAKE`). HotSpot's suspension is a handshake:
/// when `VirtualMachine.Suspend` answers, every thread is stopped. Here the
/// commands only count it and each running thread parks at its next suspend
/// point, so a `Frames`, `Status`, `OwnedMonitors` or a field read right
/// after the reply raced the thread's arrival. A thread counts as stopped
/// once it published its listing at its suspend point, is blocked in a
/// region, is an unmounted virtual thread, or is dead
/// ([`suspension_on_its_way`]). A thread still on its way after
/// [`SUSPEND_POINT_WAIT`] (all of them together) becomes a laggard
/// (`DebugState::suspend_laggards`): later waits look at it but do not wait
/// for it, until one sees it stopped. Runs on the JDWP server thread after
/// the gates are republished (so the threads' suspend points are armed),
/// holding no lock between its polls. `CRATONVM_FRAME_TRACE=1` prints
/// `[SUSPEND_SETTLED] waited=<n> laggards=<n> waited_us=<n>` whenever it
/// found a thread on its way.
#[cold]
#[inline(never)]
fn await_suspension_settled(shared: &crate::vm::SharedVm, only: Option<u64>) {
    if !shared.debug.debugger_gates.suspend_handshake() {
        return;
    }
    // Wave 53 (lane L6): a suspension of every thread covers the threads
    // started since the table was last refreshed too (neither
    // `VirtualMachine.Suspend` nor a `SUSPEND_ALL` event set refreshes it,
    // `refreshes_thread_names`): they were not waited for at all.
    if only.is_none() {
        populate_thread_metadata(shared);
    }
    let registry = &shared.threads.thread_registry;
    let manager = &shared.threads.virtual_thread_manager;
    let start = std::time::Instant::now();
    let mut checked: Vec<u64> = Vec::new();
    let mut first: Option<usize> = None;
    let mut polls = 0u32;
    let mut nudges = 0u32;
    let native_ack = shared.debug.debugger_gates.suspend_native_ack();
    // Interpreter round i2 wave 56, lane L6
    // (`CRATONVM_DEBUG_SUSPEND_SETTLE_LONG`): HotSpot's handshake has no time
    // bound, and on an overloaded host a running thread can be descheduled
    // for longer than 250 ms on its way to its suspend point; the reply then
    // went out with it running (`L1W49JdiSuspendCompletesBeforeReply`, 3 of
    // 80 at load ~45). The long bound only matters for such a thread: one
    // that is blocked, in an acknowledged native, deposited or dead is not
    // waited for at all.
    let settle_bound = if shared.debug.debugger_gates.suspend_settle_long() {
        SUSPEND_SETTLE_LONG_WAIT
    } else {
        SUSPEND_POINT_WAIT
    };
    let mut acknowledged: Vec<u64> = Vec::new();
    // Interpreter round i2 wave 64, lane L6
    // (`CRATONVM_DEBUG_SETTLE_ARRIVAL_COUNT`): the last walk's answer, the
    // arrival count it was stamped with, and when it ran. A round with no
    // arrival since reuses it; a walk looks only at the threads the last
    // walk found on their way (a thread seen stopped stays stopped for this
    // suspension: only a resume -- the server's own next command, or a C
    // JVMTI resume, which moves the count -- unlists it).
    let arrival_count = shared.debug.debugger_gates.settle_arrival_count();
    let mut last_walk: Option<SettleWalk> = None;
    let mut walks = 0u32;
    // Interpreter round i6 wave 117, lane L1
    // (`CRATONVM_DEBUG_SETTLE_PENDING_COUNTER`): one walk, then the arrivals.
    let pending_counter = shared.debug.debugger_gates.settle_pending_counter();
    let mut counted: Option<CountedSettle> = None;
    let pending = loop {
        acknowledged.clear();
        let arrivals_now = shared.debug.debugger_gates.arrivals();
        let reuse = arrival_count
            && last_walk
                .as_ref()
                .is_some_and(|walk| !walk.due(arrivals_now));
        let (mut pending, laggards): (Vec<u64>, Vec<u64>) = if pending_counter {
            let mut ds = shared.debug.debug_state.lock();
            match counted.as_mut() {
                None => {
                    checked = match only {
                        Some(tid) => vec![tid],
                        None => commands::known_thread_ids(&ds),
                    };
                    let pending: Vec<u64> = checked
                        .iter()
                        .copied()
                        .filter(|&tid| suspension_on_its_way(&ds, tid))
                        .collect();
                    let laggards: Vec<u64> = pending
                        .iter()
                        .copied()
                        .filter(|tid| ds.suspend_laggards.contains(tid))
                        .collect();
                    // Registered in the walk's own critical section: a
                    // listing published after it is an arrival.
                    ds.settle_wanted = pending.iter().copied().collect();
                    ds.settle_arrived.clear();
                    walks += 1;
                    counted = Some(CountedSettle {
                        pending: pending.clone(),
                        laggards: laggards.clone(),
                        walked_at: std::time::Instant::now(),
                    });
                    (pending, laggards)
                }
                Some(c) => {
                    if !ds.settle_arrived.is_empty() {
                        let arrived: HashSet<u64> = ds.settle_arrived.drain(..).collect();
                        c.pending.retain(|tid| !arrived.contains(tid));
                    }
                    // The backstop: a change no listing publication
                    // reports (a death, a C JVMTI resume).
                    if c.walked_at.elapsed() >= SETTLE_REWALK_BACKSTOP {
                        c.pending.retain(|&tid| suspension_on_its_way(&ds, tid));
                        c.walked_at = std::time::Instant::now();
                        walks += 1;
                    }
                    (c.pending.clone(), c.laggards.clone())
                }
            }
        } else if reuse {
            let walk = last_walk.as_ref().expect("reuse needs a walk");
            (walk.pending.clone(), walk.laggards.clone())
        } else {
            let ds = shared.debug.debug_state.lock();
            if checked.is_empty() {
                checked = match only {
                    Some(tid) => vec![tid],
                    None => commands::known_thread_ids(&ds),
                };
            }
            let source: &[u64] = match (&last_walk, arrival_count) {
                (Some(walk), true) => &walk.pending,
                _ => &checked,
            };
            let pending: Vec<u64> = source
                .iter()
                .copied()
                .filter(|&tid| suspension_on_its_way(&ds, tid))
                .collect();
            let laggards: Vec<u64> = pending
                .iter()
                .copied()
                .filter(|tid| ds.suspend_laggards.contains(tid))
                .collect();
            walks += 1;
            if arrival_count {
                last_walk = Some(SettleWalk {
                    pending: pending.clone(),
                    laggards: laggards.clone(),
                    arrivals: arrivals_now,
                    at: std::time::Instant::now(),
                });
            }
            (pending, laggards)
        };
        // Outside the debug-state lock: the registry's and the manager's
        // own locks.
        pending.retain(|&tid| {
            let id = crate::threading::ThreadId(tid);
            if !registry.is_alive(id) || registry.is_blocked(id) || manager.is_deposited(tid) {
                return false;
            }
            // Wave 53 (lane L6): in a native, outside a region, with no Java
            // above it: it parks at the native's return.
            if native_ack && in_native_acknowledged(registry, id) {
                acknowledged.push(tid);
                return false;
            }
            true
        });
        // Wave 117 (`CRATONVM_DEBUG_SETTLE_PENDING_COUNTER`): a thread seen
        // stopped stays stopped for this suspension, as the arrival-count
        // walk treats it.
        if let Some(c) = counted.as_mut() {
            if c.pending.len() != pending.len() {
                let still: HashSet<u64> = pending.iter().copied().collect();
                c.pending.retain(|tid| still.contains(tid));
            }
        }
        first.get_or_insert(pending.len());
        let waiting = pending.iter().any(|tid| !laggards.contains(tid));
        if !waiting || start.elapsed() >= settle_bound {
            break pending;
        }
        // Wave 53 (lane L6): a thread still running compiled code is sent to
        // its poll again ([`suspend_nudge_due`]).
        if suspend_nudge_due(start.elapsed(), nudges) {
            nudges += 1;
            crate::runtime::interpreter::request_compiled_loop_exits_again(shared);
        }
        // A thread running bytecode arrives within microseconds: yield
        // first, sleep (a coarse quantum on some hosts) only after.
        polls += 1;
        if polls < 64 {
            std::thread::yield_now();
        } else {
            await_debug_state_poll(shared);
        }
    };
    {
        let mut ds = shared.debug.debug_state.lock();
        // Wave 117: the wait is over; no listing is an arrival any more.
        if pending_counter {
            ds.settle_wanted.clear();
            ds.settle_arrived.clear();
        }
        // A laggard seen stopped is waited for again next time.
        ds.suspend_laggards
            .retain(|tid| !checked.contains(tid) || pending.contains(tid));
        ds.suspend_laggards.extend(pending.iter().copied());
    }
    // Wave 53 (lane L6): a laggard still running compiled code is asked once
    // more, so it parks soon after the reply instead of running suspended
    // until something else pauses the VM.
    if !pending.is_empty() {
        nudges += 1;
        crate::runtime::interpreter::request_compiled_loop_exits_again(shared);
    }
    if crate::runtime::env_cache::frame_trace() && first.is_some_and(|n| n > 0) {
        // Wave 58 (lane L6): each laggard's state as the wait gave up on it
        // -- parked at a suspend point, its listing's generation against
        // its current one.
        let states: Vec<String> = {
            let ds = shared.debug.debug_state.lock();
            pending
                .iter()
                .map(|&tid| {
                    let listed = ds
                        .thread_frames
                        .get(&tid)
                        .map(|f| (f.len(), f.first().map(|e| e.frame_id >> 32)));
                    format!(
                        "{tid}:parked={} listed={listed:?} gen={}",
                        ds.is_parked(tid),
                        ds.frame_generation(tid)
                    )
                })
                .collect()
        };
        eprintln!(
            "[SUSPEND_SETTLED] waited={} laggards={} waited_us={} nudges={nudges} in_native={} \
             walks={walks} polls={polls} laggard_tids={pending:?} laggard_states={states:?}",
            first.unwrap_or(0),
            pending.len(),
            start.elapsed().as_micros(),
            acknowledged.len()
        );
    }
}

/// Has thread `id` acknowledged a JDWP suspension by being in a native
/// outside a blocking region, with no Java above it
/// (`GcBlockState::debugger_native_ack`, interpreter round i2 wave 53, lane
/// L6; stage 2 of the suspension handshake, `CRATONVM_DEBUG_SUSPEND_NATIVE_ACK`)?
/// HotSpot's handshake counts a thread in native stopped: it touches no Java
/// state until its transition back, where it blocks. Here the native-call
/// funnel parks it at the native's return
/// (`interpreter::park_if_suspended_at_native_exit`, which with the switch on
/// parks a native it cannot name too), before any bytecode of its caller.
///
/// Interpreter round i5 wave 101, lane L1: also the C JVMTI table's
/// `SuspendThread` / `SuspendThreadList` / `SuspendAllVirtualThreads` stop
/// rule (`jvmti::native_env::thread_stopped`), which waited up to its 2 s
/// bound for such a thread (HotSpot's `SuspendThread` returns at once for a
/// thread in native).
pub(crate) fn in_native_acknowledged(
    registry: &crate::threading::thread_registry::ThreadRegistry,
    id: crate::threading::ThreadId,
) -> bool {
    registry.gc_block_state_of(id).is_some_and(|state| {
        state
            .debugger_native_ack
            .load(std::sync::atomic::Ordering::Acquire)
    })
}

/// One walk of [`await_suspension_settled`] over the debug state, kept for
/// the rounds after it with [`DebuggerGates::settle_arrival_count`]
/// (interpreter round i2 wave 64, lane L6).
struct SettleWalk {
    /// The threads it found on their way, as far as the debug state tells.
    pending: Vec<u64>,
    /// Those of them that are laggards of an earlier suspension.
    laggards: Vec<u64>,
    /// [`DebuggerGates::arrivals`] read before it took the lock.
    arrivals: u64,
    /// When it ran.
    at: std::time::Instant,
}

impl SettleWalk {
    /// Must the next round walk again: something arrived (or changed) since
    /// this walk's count was read, or [`SETTLE_REWALK_BACKSTOP`] passed?
    fn due(&self, arrivals_now: u64) -> bool {
        arrivals_now != self.arrivals || self.at.elapsed() >= SETTLE_REWALK_BACKSTOP
    }
}

/// How long [`await_suspension_settled`] reuses a walk with no arrival
/// counted since ([`DebuggerGates::settle_arrival_count`], wave 64): the
/// bound of a change that notifies nothing (a listing an event published
/// without parking). A backstop, not a poll: the arrivals it covers are
/// rare, and the registry's answers are read every round regardless.
const SETTLE_REWALK_BACKSTOP: std::time::Duration = std::time::Duration::from_millis(10);

/// When [`await_suspension_settled`] asks the compiled loops to leave again
/// (`interpreter::request_compiled_loop_exits_again`, interpreter round i2
/// wave 53, lane L6): after [`SUSPEND_NUDGE_FIRST`] of waiting, then each
/// time the wait doubles (4, 8, 16, ... ms: at most six times within
/// [`SUSPEND_POINT_WAIT`]). A thread that runs bytecode or polls arrives
/// well within the first slice, so a settled suspension never asks; one the
/// suspension's own loop-exit pause missed is asked within a few
/// milliseconds.
fn suspend_nudge_due(elapsed: std::time::Duration, nudges: u32) -> bool {
    elapsed >= SUSPEND_NUDGE_FIRST.saturating_mul(1u32 << nudges.min(16))
}

/// One round of the JDWP server's waits for a thread's arrival
/// ([`await_suspension_settled`], [`await_suspend_point_frames`],
/// [`await_inspection_window`]): at most [`DEBUG_STATE_POLL`], ended early by
/// an arrival at a suspend point with [`DebuggerGates::park_condvar`] on
/// (interpreter round i2 wave 54, lane L6). A window's opening does not
/// notify, so the bound stays the old poll.
fn await_debug_state_poll(shared: &crate::vm::SharedVm) {
    let mut ds = shared.debug.debug_state.lock();
    shared
        .debug
        .debugger_gates
        .await_state_change(&mut ds, DEBUG_STATE_POLL);
}

/// The first slice of [`suspend_nudge_due`].
const SUSPEND_NUDGE_FIRST: std::time::Duration = std::time::Duration::from_millis(4);

/// How long [`await_suspend_point_frames`] waits for a suspended thread to
/// park at its suspend point.
const SUSPEND_POINT_WAIT: std::time::Duration = std::time::Duration::from_millis(250);

/// The settle wait's bound with [`DebuggerGates::suspend_settle_long`]
/// (interpreter round i2 wave 56, lane L6): long enough for a thread an
/// overloaded host deschedules, short enough that a thread that never
/// reaches a suspend point (a compiled loop that does not poll, a native
/// with no acknowledgement) delays one reply, not the session -- it becomes
/// a laggard, which later waits do not wait for.
const SUSPEND_SETTLE_LONG_WAIT: std::time::Duration = std::time::Duration::from_secs(10);

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
///
/// Interpreter round i2 wave 55, lane L6
/// (`docs/feature-designs/jvmti-delivery-threading.md`):
/// a C JVMTI agent's `SuspendThread` holds it too, with no JDWP session --
/// JVMTI and JDWP suspensions are one suspension in HotSpot. Until wave 55
/// the gate also required a JDWP session, so with an agent alone a suspended
/// I/O loop kept dispatching.
///
/// Interpreter round i5 wave 106 (lane L1; the audit lane L6's wave-105
/// finding asked for): gated on [`DebuggerGates::suspension_counted`], not
/// [`DebuggerGates::suspension_in_force`], which leaves out the suspension of
/// a thread the last listing saw ended (ZOMBIE): a hold of such a thread
/// returned at once whatever its suspend count. The slow path asks the debug
/// state itself ([`DebugState::holds_off_monitor_entry`]), as before.
#[inline]
pub(crate) fn hold_native_thread_while_suspended(shared: &crate::vm::SharedVm, tid: u64) {
    if shared.debug.debugger_gates.suspension_counted() {
        hold_native_thread_while_suspended_slow(shared, tid);
    }
}

/// A thread whose `ThreadDeath` event suspended it (`SUSPEND_EVENT_THREAD`
/// or `SUSPEND_ALL`; [`send_thread_death_event`] answered true -- the
/// caller's only gate, never a bare "some suspension is counted" bit)
/// waits here until the debugger resumes it or detaches,
/// before its termination: `threadStatus` TERMINATED, `isAlive()` false and
/// the joiners' wake-up (interpreter round i5 wave 106, lane L1). HotSpot's
/// `JavaThread::exit` posts `ThreadEnd` -- the back end's handler suspends the
/// event thread inside the callback -- before `ensure_join`, so a
/// `thread.join()` elsewhere stays blocked while the debugger holds the
/// event set. Lane L6 did this for `main` in wave 105
/// (`vm_exec::end_main_thread_java_side`); every other platform thread posted
/// the event and went on, so its joiners woke while it was held. The caller
/// is in a blocked region (GC-safe) for the whole wait.
#[cold]
#[inline(never)]
pub(crate) fn hold_ending_thread_while_suspended(shared: &crate::vm::SharedVm, tid: u64) {
    let gates = &shared.debug.debugger_gates;
    let mut ds = shared.debug.debug_state.lock();
    while ds.holds_off_monitor_entry(tid) {
        // Bounded, so a resume that publishes no change is still seen.
        gates.await_state_change(&mut ds, BLOCKED_WAIT_BOUND);
    }
}

/// [`hold_native_thread_while_suspended`] while a suspension is in force: the
/// park loop of `interpreter::park_for_debugger` without its task queue (a
/// native thread runs no invocation) and without safepoint polls (it is
/// GC-blocked). Held while the debugger's suspend count or an agent's
/// `SuspendThread` stands ([`DebugState::holds_off_monitor_entry`]), not for
/// a stack read's handshake, which holds a thread only while its stack is
/// read. Ends on the resume, or on a detach (which resumes every JDWP
/// suspension: `DebugState::resume_all`).
#[cold]
#[inline(never)]
fn hold_native_thread_while_suspended_slow(shared: &crate::vm::SharedVm, tid: u64) {
    wait_while_suspended(shared, tid, DebugState::holds_off_monitor_entry);
}

/// Must a thread in native that is about to leave native for a JNIEnv or C
/// JVMTI function look at its suspension (gci i16/sn,
/// `CRATONVM_DEBUG_SUSPEND_AT_NATIVE_TRANSITION`)? A suspension of some
/// thread is in force and the switch is on: two loads, the first of which
/// is down whenever no debugger or agent suspended anything.
#[inline(always)]
pub(crate) fn native_transition_hold_armed(shared: &crate::vm::SharedVm) -> bool {
    let gates = &shared.debug.debugger_gates;
    gates.suspension_in_force() && gates.suspend_at_native_transition()
}

/// A thread whose JNI native's C code runs in native, about to leave native
/// for a JNIEnv or C JVMTI function, waits here while a debugger or a C
/// JVMTI agent holds it suspended (gci i16/sn;
/// `gci-i15rb-vm-default-path-changes-review-findings-FIXED-20261017` finding 6).
/// Answers whether it waited.
///
/// HotSpot's native -> VM transition (`ThreadInVMfromNative`) processes the
/// thread's pending suspend handshake and blocks there, so a suspended
/// thread's C code runs on only until it calls into the VM. Here the thread
/// ran every such function while suspended (`jvmti::native_env`
/// `thread_stopped` counts it stopped at once: it is in a blocked region).
///
/// The caller is still IN NATIVE (`native::jni`, before its in-native leave):
/// its blocked region stands, its deposited roots are its roots, and every
/// pause excludes it, so a collection never waits for it here, and the
/// in-native leave's fairness rule (`GcBarrier::leave_in_native_if`, gci
/// i15/hx) never sees it as a leaver: it leaves only after the resume. Its
/// frames read through its window as before, so the listing is unchanged by
/// the hold. Held as [`hold_native_thread_while_suspended`] holds: by the
/// debugger's suspend count or an agent's `SuspendThread`
/// ([`DebugState::holds_off_monitor_entry`]), not by a stack read's
/// handshake, which holds a thread only while its stack is read. Never on
/// the JDWP server thread or for work the debugger runs
/// ([`debugger_hooks_suppressed`]), nor on a thread parked at a suspend
/// point (the work its park runs: an invocation's own natives).
/// `CRATONVM_FRAME_TRACE=1` prints `[NATIVE_TRANSITION_SUSPEND] tid=<n>
/// held_us=<n>` after a hold.
#[cold]
#[inline(never)]
pub(crate) fn hold_at_native_transition_while_suspended(
    shared: &crate::vm::SharedVm,
    tid: u64,
) -> bool {
    if debugger_hooks_suppressed() {
        return false;
    }
    {
        let ds = shared.debug.debug_state.lock();
        if !ds.holds_off_monitor_entry(tid) || ds.is_parked(tid) {
            return false;
        }
    }
    let began = std::time::Instant::now();
    wait_while_suspended(shared, tid, DebugState::holds_off_monitor_entry);
    if crate::runtime::env_cache::frame_trace() {
        eprintln!(
            "[NATIVE_TRANSITION_SUSPEND] tid={tid} held_us={}",
            began.elapsed().as_micros()
        );
    }
    true
}

/// Must thread `tid`, which has just won a contended monitor, give it back
/// because the debugger (or a C JVMTI agent) holds it suspended (interpreter
/// round i2 wave 54, lane L6; [`exit_monitor_while_suspended`])? One load
/// while no suspension is in force.
#[inline]
pub(crate) fn monitor_entry_held_off(
    shared: &crate::vm::SharedVm,
    tid: crate::threading::ThreadId,
) -> bool {
    let gates = &shared.debug.debugger_gates;
    gates.suspension_in_force()
        && gates.monitor_exit_on_suspend()
        && monitor_entry_held_off_slow(shared, tid.0)
}

/// [`monitor_entry_held_off`] while a suspension is in force.
#[cold]
#[inline(never)]
fn monitor_entry_held_off_slow(shared: &crate::vm::SharedVm, tid: u64) -> bool {
    shared.debug.debug_state.lock().holds_off_monitor_entry(tid)
}

/// A thread that won the contended `monitor` in its blocked entry
/// (`vm_exec::monitor_enter_blocking`, still GC-blocked, its inspection
/// window still open) while the debugger or a C JVMTI agent holds it
/// suspended gives the monitor back, waits for its resume, and enters again
/// (interpreter round i2 wave 54, lane L6;
/// `docs/feature-designs/jvmti-delivery-threading.md`,
/// `CRATONVM_DEBUG_MONITOR_EXIT_ON_SUSPEND`).
///
/// HotSpot's contended entry does this (`ObjectMonitor::enter` with
/// `ExitOnSuspend`): a debugger's suspension of a blocked thread never takes
/// a lock away from the rest of the program, and the thread still reads
/// `MONITOR` (`BLOCKED`) while it waits. Here it won the monitor the moment
/// its holder released it and parked holding it at its next suspend point,
/// so every other thread that needed the lock waited for the debugger's
/// resume (`L6W53JdiSuspendedThreadAndContendedMonitor`).
///
/// The acquisition `block_enter` made is fresh (entry count 1: a re-entrant
/// one never contends), so one `Monitor::exit` undoes it and wakes the next
/// entrant as a release does. The exit is by the monitor, never by the
/// object, whose address a collection may have moved meanwhile. The thread
/// stays in its blocked region throughout (Java state `BLOCKED`, roots
/// deposited, its JMX contention still published), so a collection never
/// waits for it here. `CRATONVM_FRAME_TRACE=1` prints
/// `[MONITOR_EXIT_ON_SUSPEND] tid=<n> given_back=<n>` when it gave one back.
#[inline]
pub(crate) fn exit_monitor_while_suspended(
    shared: &crate::vm::SharedVm,
    tid: crate::threading::ThreadId,
    monitor: &crate::threading::monitor::Monitor,
) {
    if shared.debug.debugger_gates.suspension_in_force() {
        exit_monitor_while_suspended_slow(shared, tid, monitor);
    }
}

/// [`exit_monitor_while_suspended`] while a suspension is in force.
#[cold]
#[inline(never)]
fn exit_monitor_while_suspended_slow(
    shared: &crate::vm::SharedVm,
    tid: crate::threading::ThreadId,
    monitor: &crate::threading::monitor::Monitor,
) {
    let mut given_back = 0u32;
    while monitor_entry_held_off(shared, tid) {
        if monitor.exit(tid).is_err() {
            // Not ours after all: nothing to give back (never expected).
            break;
        }
        given_back += 1;
        wait_while_suspended(shared, tid.0, DebugState::holds_off_monitor_entry);
        monitor.block_enter(tid);
    }
    if given_back > 0 && crate::runtime::env_cache::frame_trace() {
        eprintln!(
            "[MONITOR_EXIT_ON_SUSPEND] tid={} given_back={given_back}",
            tid.0
        );
    }
}

/// The VM's [`crate::threading::monitor::EntryHold`] (interpreter round i2
/// wave 56, lane L6, `CRATONVM_MONITOR_SUSPENSION_AWARE_ENTRY`): a debugger
/// or a C JVMTI agent holding the thread suspended
/// ([`monitor_entry_held_off`], one load without a suspension), waited out as
/// [`exit_monitor_while_suspended`] waits. The loop that uses it lives in the
/// monitor (`Monitor::give_back_fresh_hold_while_held_off`); this is the
/// whole of the debugger's part.
pub(crate) struct DebuggerEntryHold<'a> {
    pub(crate) shared: &'a crate::vm::SharedVm,
}

impl crate::threading::monitor::EntryHold for DebuggerEntryHold<'_> {
    #[inline]
    fn held_off(&self, thread_id: crate::threading::ThreadId) -> bool {
        monitor_entry_held_off(self.shared, thread_id)
    }

    #[cold]
    #[inline(never)]
    fn wait_until_released(&self, thread_id: crate::threading::ThreadId) {
        wait_while_suspended(
            self.shared,
            thread_id.0,
            DebugState::holds_off_monitor_entry,
        );
    }
}

/// Wait, GC-blocked, while `held(debug state, tid)` stays true (interpreter
/// round i2 wave 54, lane L6): the loop of
/// [`hold_native_thread_while_suspended_slow`], woken by the resume's
/// publication with [`DebuggerGates::park_condvar`], a 1 ms poll without.
/// The caller is in a blocked region, so it answers no safepoint.
fn wait_while_suspended(
    shared: &crate::vm::SharedVm,
    tid: u64,
    held: fn(&DebugState, u64) -> bool,
) {
    let gates = &shared.debug.debugger_gates;
    // Wave 63 (lane L6): the thread's own word, looked up before the
    // debug-state lock (the registry lock is never taken under it).
    let state = if gates.thread_suspend_word() {
        shared
            .threads
            .thread_registry
            .gc_block_state_of(crate::threading::ThreadId(tid))
    } else {
        None
    };
    let mut ds = shared.debug.debug_state.lock();
    let Some(state) = state else {
        while held(&ds, tid) {
            gates.await_state_change(&mut ds, BLOCKED_WAIT_BOUND);
        }
        return;
    };
    if !held(&ds, tid) {
        return;
    }
    let mut word = SuspendWordWait::enroll(gates, &mut ds, tid, &state);
    loop {
        word.note_locked_round();
        if !held(&ds, tid) {
            break;
        }
        // The lock released while the word stands: a resume rewrites it and
        // unparks this thread.
        parking_lot::MutexGuard::unlocked(&mut ds, || {
            while word.wait_unchanged(BLOCKED_WAIT_BOUND) {}
        });
    }
    word.leave(&mut ds, tid);
}

/// [`DebugState::suspend_word_flags`]: the word is live (its thread is
/// enrolled; interpreter round i2 wave 63, lane L6).
pub(crate) const SW_ENROLLED: u32 = 1;
/// [`DebugState::suspend_word_flags`]: [`DebugState::is_thread_suspended`].
pub(crate) const SW_SUSPENDED: u32 = 2;
/// [`DebugState::suspend_word_flags`]: [`DebugState::holds_off_monitor_entry`].
pub(crate) const SW_HOLDS_OFF: u32 = 4;
/// [`DebugState::suspend_word_flags`]: work is queued for the parked thread.
pub(crate) const SW_TASK: u32 = 8;
/// The flag bits of a suspend word; the bits above count its changes.
const SW_FLAGS: u32 = 0xF;
/// Where a suspend word's change sequence starts.
const SW_SEQ_SHIFT: u32 = 4;

/// How long a thread waiting on its suspend word goes without a locked round
/// of the debug state (interpreter round i2 wave 63, lane L6): the bound of a
/// change the word does not carry (a listing withdrawn by another thread, a
/// missed wake-up). Every change of a suspend count, a JVMTI suspension or a
/// parked task moves the word, so this is a backstop, not a poll.
pub(crate) const SUSPEND_WORD_BACKSTOP: std::time::Duration = std::time::Duration::from_millis(50);

/// One enrolled thread of [`DebugState::suspend_words`] (wave 63): where its
/// word lives, and the OS threads to unpark when the word moves (one per
/// nested wait; normally all the same thread).
struct SuspendWordEntry {
    state: Arc<crate::threading::jvm_thread::GcBlockState>,
    wakers: Vec<std::thread::Thread>,
}

impl SuspendWordEntry {
    /// Store `flags` with the next change sequence and unpark the waiters,
    /// when they differ from the word's flags (or the word is still 0: a
    /// fresh enrollment). Called under the debug-state lock, so writes never
    /// race each other.
    fn publish(&self, flags: u32) {
        use std::sync::atomic::Ordering;
        let word = &self.state.debugger_suspend_word;
        let old = word.load(Ordering::Relaxed);
        if old != 0 && old & SW_FLAGS == flags {
            return;
        }
        let seq = (old >> SW_SEQ_SHIFT).wrapping_add(1);
        word.store(seq.wrapping_shl(SW_SEQ_SHIFT) | flags, Ordering::Release);
        for waker in &self.wakers {
            waker.unpark();
        }
    }
}

/// A wait that asks the waiting thread's own suspend word instead of the
/// debug-state lock (interpreter round i2 wave 63, lane L6;
/// [`DebuggerGates::thread_suspend_word`]). The waiter takes a locked round
/// (reads the debug state under its lock and records the word it saw with
/// [`Self::note_locked_round`]), then waits without the lock
/// ([`Self::wait_unchanged`]) until the word moves -- every writer of what
/// the round read moves it, under that lock, and unparks the waiter -- or
/// [`SUSPEND_WORD_BACKSTOP`] passes. Inactive (switch off): every
/// [`Self::wait_unchanged`] answers `false`, so the caller's loop is the
/// lock loop it was.
pub(crate) struct SuspendWordWait {
    state: Option<Arc<crate::threading::jvm_thread::GcBlockState>>,
    /// The word the last locked round saw; `None` before the first.
    seen: Option<u32>,
    /// When the last locked round was.
    locked_at: std::time::Instant,
    /// Locked rounds taken (the `[DEBUG_PARK_WORD]` trace).
    pub(crate) locked_rounds: u64,
    /// Lock-free waits a moved word ended (or found moved).
    pub(crate) word_wakes: u64,
}

impl SuspendWordWait {
    /// Enroll thread `tid` (the calling thread, `GcBlockState` `state`) on
    /// its word under the debug-state lock `ds`, when `gates` say so; an
    /// inactive wait otherwise.
    pub(crate) fn enroll(
        gates: &DebuggerGates,
        ds: &mut DebugState,
        tid: u64,
        state: &Arc<crate::threading::jvm_thread::GcBlockState>,
    ) -> Self {
        let active = gates.thread_suspend_word();
        if active {
            ds.enroll_suspend_word(tid, state);
        }
        Self {
            state: active.then(|| Arc::clone(state)),
            seen: None,
            locked_at: std::time::Instant::now(),
            locked_rounds: 0,
            word_wakes: 0,
        }
    }

    /// Does this wait ask the word (the switch was on at its enrollment)?
    #[inline]
    pub(crate) fn active(&self) -> bool {
        self.state.is_some()
    }

    /// Has a locked round recorded the word yet (wave 65): may the next
    /// [`Self::wait_unchanged`] wait at all?
    #[inline]
    pub(crate) fn has_seen(&self) -> bool {
        self.seen.is_some()
    }

    /// Without the lock and without waiting: has the word moved since the
    /// last locked round (interpreter round i2 wave 66, lane L6)? `false`
    /// before the first locked round and when inactive.
    #[inline]
    pub(crate) fn moved(&self) -> bool {
        match (&self.state, self.seen) {
            (Some(state), Some(seen)) => {
                state
                    .debugger_suspend_word
                    .load(std::sync::atomic::Ordering::Acquire)
                    != seen
            }
            _ => false,
        }
    }

    /// Record, under the debug-state lock, the word a locked round saw.
    pub(crate) fn note_locked_round(&mut self) {
        if let Some(state) = &self.state {
            self.seen = Some(
                state
                    .debugger_suspend_word
                    .load(std::sync::atomic::Ordering::Acquire),
            );
            self.locked_at = std::time::Instant::now();
            self.locked_rounds += 1;
        }
    }

    /// Without the debug-state lock: may the caller skip its next locked
    /// round? `true` after waiting up to `bound` on this thread's parker
    /// with the word still what the last locked round saw; `false` at once
    /// when the word moved, when the backstop is due, before the first
    /// locked round, and always when inactive.
    pub(crate) fn wait_unchanged(&mut self, bound: std::time::Duration) -> bool {
        use std::sync::atomic::Ordering;
        let (Some(state), Some(seen)) = (&self.state, self.seen) else {
            return false;
        };
        let word = &state.debugger_suspend_word;
        if word.load(Ordering::Acquire) != seen {
            self.word_wakes += 1;
            return false;
        }
        if self.locked_at.elapsed() >= SUSPEND_WORD_BACKSTOP {
            return false;
        }
        std::thread::park_timeout(bound);
        if word.load(Ordering::Acquire) != seen {
            self.word_wakes += 1;
            return false;
        }
        true
    }

    /// End the wait under the debug-state lock `ds` (thread `tid`).
    pub(crate) fn leave(&mut self, ds: &mut DebugState, tid: u64) {
        if self.state.take().is_some() {
            ds.leave_suspend_word(tid);
        }
    }
}

/// When a park at a suspend point opens its GC-blocked region (interpreter
/// round i2 wave 68, lane L6; [`DebuggerGates::park_region_at_first_pause`],
/// `docs/feature-designs/jvmti-delivery-threading.md`).
///
/// Eager (switch off, wave 65): after the park's first locked round, and
/// again after every task or relisting that closed it. Lazy (switch on):
/// the park waits outside its region, a counted mutator enrolled with the
/// barrier, until a stop-the-world request first wakes it; it then opens
/// the region instead of arriving at the safepoint
/// ([`ParkGcRegion::enter_at_pause`], whose entry arrives for that pause),
/// and from then on behaves as an eager park for the rest of the hold. A
/// hold that sees no pause -- a step, a breakpoint looked at, a
/// suspend+resume round trip -- deposits nothing and refreshes nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ParkRegionOpening {
    /// The switch, as the park read it.
    lazy: bool,
    /// A pause has opened the region during this hold.
    paused: bool,
}

impl ParkRegionOpening {
    /// For one hold; `lazy` from [`DebuggerGates::park_region_at_first_pause`].
    pub(crate) const fn new(lazy: bool) -> Self {
        Self {
            lazy,
            paused: false,
        }
    }

    /// Does this park wait outside its region until a pause comes (and so
    /// need the barrier to wake it)?
    #[inline]
    pub(crate) fn lazy(&self) -> bool {
        self.lazy
    }

    /// May the park open its region on its own, at a quiet word, without a
    /// pause asking: always when eager, once a pause has opened it when lazy.
    #[inline]
    pub(crate) fn enters_eagerly(&self) -> bool {
        !self.lazy || self.paused
    }

    /// Should the park, finding a stop-the-world request with its region
    /// closed, open the region (arriving through the entry) rather than
    /// arrive at the safepoint? Only when lazy, once a locked round has seen
    /// its word (`word_seen`: its listing stands), and not while the word
    /// carries news for the locked round (`news`, wave 66's
    /// `CRATONVM_DEBUG_HEAP_WORK_OUTSIDE_REGION`): a task would close the
    /// region again at once.
    #[inline]
    pub(crate) fn opens_at_pause(&self, word_seen: bool, news: bool) -> bool {
        self.lazy && word_seen && !news
    }

    /// The region was opened at a pause.
    #[inline]
    pub(crate) fn note_opened_at_pause(&mut self) {
        self.paused = true;
    }
}

/// The GC-blocked region a park at a suspend point waits in between its
/// locked rounds (interpreter round i2 wave 65, lane L6;
/// [`DebuggerGates::park_gc_blocked`],
/// `docs/feature-designs/jvmti-delivery-threading.md`).
///
/// The `Object.wait` shape, by the same calls
/// (`NativeContextImpl::begin_blocking_region_with_state` /
/// `end_blocking_region`): the TLAB retired, the roots deposited
/// (`deposit_root_snapshot`, which raises `in_blocked_region`), the barrier's
/// blocked count entered -- arriving once for a pause already requested --
/// and, on the way out, the blocked count left (waiting out a pause in
/// progress) and the composed fixup of the collections slept through applied
/// to the frames and every off-frame reference (`check_post_block_gc`, which
/// also moves frames a redefinition made obsolete). In between a pause
/// excludes the thread from its census and remaps it through its deposit:
/// it neither wakes it nor waits for it.
///
/// While open the thread's frames hold the addresses of its last deposit, so
/// nothing may read or write them, nor the heap, from this thread: the park
/// closes the region before a task (an invocation, a heap read, a frame
/// write), before it relists its frames and before it leaves. The listing
/// it published stays readable meanwhile: its object ids are pinned handles,
/// which every collection remaps.
///
/// The thread reads RUNNABLE inside (`GcBlockState::java_state` 0, so
/// `Thread.getState()`, JDWP `ThreadReference.Status` and JVMTI
/// `GetThreadState` answer as for a thread suspended while running, as
/// HotSpot's do); the value it had is put back when the region closes.
///
/// Wave 66 (lane L6, [`DebuggerGates::park_region_reuses_deposit`]): the
/// leave ends with a full refresh of the root snapshot from the frames
/// (`check_post_block_gc`'s no-flag deposit). When neither the region nor
/// that refresh saw a collection or a redefinition, the leave records a
/// [`StandingDeposit`] key. A re-entry whose key still matches -- no pause,
/// no other root publish, the same frames at the same pc, and only work
/// that never touches this thread's frames ran meanwhile (heap work, a
/// relisting; the park calls [`Self::forget_standing`] after any other
/// task) -- raises the flag over that standing snapshot
/// ([`Self::reuse_standing`]) instead of depositing again.
pub(crate) struct ParkGcRegion {
    /// `GcBlockState::java_state` as it was before the region; `Some` while
    /// the region is open.
    saved_java_state: Option<u8>,
    /// Regions entered (the `[DEBUG_PARK_WORD]` trace).
    pub(crate) entries: u64,
    /// Wave 66: may a re-entry reuse the standing deposit?
    reuse: bool,
    /// Wave 66: `CRATONVM_DBG_BLOCKGC` -- check every reused entry's
    /// snapshot against the frames' live object slots.
    audit: bool,
    /// Wave 66: the key the open region's deposit was made under (read
    /// right after it), compared at the leave.
    entered: Option<StandingDeposit>,
    /// Wave 66: the key of the snapshot the last leave refreshed, while a
    /// re-entry may still reuse it.
    standing: Option<StandingDeposit>,
    /// Wave 66: entries that reused the standing deposit (the trace).
    pub(crate) reuses: u64,
    /// Wave 66: time spent opening regions (deposit and blocked-count
    /// entry, without a pause's wait) and closing them (the blocked-count
    /// leave, the fixup and the refresh), in ns (the trace).
    pub(crate) enter_ns: u64,
    pub(crate) leave_ns: u64,
    /// Wave 68: entries made at a pause that woke the park
    /// ([`Self::enter_at_pause`], [`ParkRegionOpening`]; the trace).
    pub(crate) pause_entries: u64,
    /// Wave 68: may a leave for heap work keep the entry's snapshot
    /// ([`DebuggerGates::park_leave_keeps_unfolded_snapshot`])?
    keep_unfolded: bool,
    /// Wave 68: leaves that kept the entry's snapshot (the trace).
    pub(crate) keeps: u64,
    /// Wave 68: the root snapshot's length right after the last full build
    /// of it on this thread (an entry's deposit, a leave's refresh). A reuse
    /// cuts the snapshot back to it before it appends the off-frame and JNI
    /// roots again, so a run of kept leaves and reused entries does not grow
    /// the snapshot by one copy of those roots per cycle.
    base_len: Option<usize>,
}

/// What a park region's standing root snapshot was made from (interpreter
/// round i2 wave 66, lane L6): equal keys mean no collection completed, no
/// class was redefined, no other root publish replaced the snapshot and the
/// frame stack has the same depth and top pc.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct StandingDeposit {
    generation: u64,
    collections: u64,
    publish_seq: u64,
    frames: usize,
    top_pc: usize,
    redefinitions: u64,
}

impl StandingDeposit {
    /// The key as it stands now for `thread`.
    fn read(
        shared: &crate::vm::SharedVm,
        thread: &crate::threading::jvm_thread::JvmThread,
    ) -> Self {
        use std::sync::atomic::Ordering;
        Self {
            generation: shared.mem.gc_barrier.gc_generation.load(Ordering::Acquire),
            collections: shared.mem.heap.collection_count(),
            publish_seq: thread.root_publish_seq.load(Ordering::Acquire),
            frames: thread.frames.len(),
            top_pc: thread.frames.last().map_or(0, |f| f.pc),
            redefinitions: cratonvm_classloading::class_redefinition_count(),
        }
    }

    /// The same moves-and-frames as `other`: everything but the publish
    /// sequence, which every deposit (the leave's refresh too) bumps.
    fn same_heap_and_frames(&self, other: &Self) -> bool {
        self.generation == other.generation
            && self.collections == other.collections
            && self.frames == other.frames
            && self.top_pc == other.top_pc
            && self.redefinitions == other.redefinitions
    }
}

impl ParkGcRegion {
    /// No region open yet.
    pub(crate) const fn closed() -> Self {
        Self {
            saved_java_state: None,
            entries: 0,
            reuse: false,
            audit: false,
            entered: None,
            standing: None,
            reuses: 0,
            enter_ns: 0,
            leave_ns: 0,
            pause_entries: 0,
            keep_unfolded: false,
            keeps: 0,
            base_len: None,
        }
    }

    /// Wave 68: let a leave for heap work keep the entry's snapshot when
    /// nothing folded into the region (`on`, from
    /// [`DebuggerGates::park_leave_keeps_unfolded_snapshot`]). Only with
    /// deposit reuse ([`Self::with_deposit_reuse`], called first): the leave
    /// needs the entry's key, which only a reusing region records.
    pub(crate) fn with_unfolded_keep(mut self, on: bool) -> Self {
        self.keep_unfolded = on && self.reuse;
        self
    }

    /// Wave 66: let a re-entry reuse the standing deposit (`on`, from
    /// [`DebuggerGates::park_region_reuses_deposit`]), auditing each reuse
    /// when `audit`.
    pub(crate) fn with_deposit_reuse(mut self, on: bool, audit: bool) -> Self {
        self.reuse = on;
        self.audit = on && audit;
        self
    }

    /// Wave 66: work that may have read or written this thread's frames or
    /// run Java on it ran since the last leave: the next entry deposits in
    /// full.
    pub(crate) fn forget_standing(&mut self) {
        self.standing = None;
    }

    /// Wave 66: raise the GC-blocked flag over the snapshot the last leave
    /// refreshed, if its key still holds; `false` (nothing done) otherwise.
    /// Called by [`Self::enter`] with `java_state` stored and the TLAB
    /// retired, where the full deposit would run.
    ///
    /// The standing snapshot holds every root the full deposit would find
    /// in the frames, the class owners, the compiled band and the shadow
    /// stack, at their current addresses: nothing moved since it was taken
    /// (no collection), the frames are the ones it scanned (no task touched
    /// them, no publish replaced it). What the work run meanwhile may have
    /// changed -- the thread's off-frame references and JNI local
    /// references -- is scanned again and appended (a duplicate only
    /// over-retains). The parts only the flag-raising deposit makes are
    /// made here as it makes them: the SATB flush, the publish sequence,
    /// the per-slot origins the wake writes back, and the frame-class
    /// census. The frame trace and its JMX attribution the region's first
    /// deposit captured still stand: the frames did not change. The thread
    /// status the full deposit would write is the RUNNABLE the leave wrote.
    fn reuse_standing(
        &mut self,
        shared: &crate::vm::SharedVm,
        thread: &mut crate::threading::jvm_thread::JvmThread,
    ) -> bool {
        use std::sync::atomic::Ordering;
        let Some(standing) = self.standing.take() else {
            return false;
        };
        if standing != StandingDeposit::read(shared, thread) {
            return false;
        }
        // The refresh skipped the compiled band (a measurement lever): it
        // does not hold the band's roots.
        if crate::runtime::env_cache::noflag_deposit_skips_jit_scan() {
            return false;
        }
        // A blocked-monitor proof only a full deposit settles.
        if thread
            .gc_block_state
            .monitor_block_proven_jit_depth
            .load(Ordering::Relaxed)
            == crate::threading::jvm_thread::MONITOR_BLOCK_PROOF_ARMED
        {
            return false;
        }
        // Pure memos, cleared before every flag-raising deposit.
        thread.jit_hashmap_string_node_cache.clear();
        thread.string_case_cache.clear();
        let thread: &crate::threading::jvm_thread::JvmThread = &*thread;
        thread.root_publish_seq.fetch_add(1, Ordering::Relaxed);
        crate::memory::reclaim_guard::note_root_publish(shared, thread);
        shared.mem.heap.flush_thread_satb();
        {
            let mut snapshot = thread.root_snapshot.lock();
            // Wave 68: what earlier reuses appended goes before this one
            // appends again (a kept leave leaves it in place).
            if let Some(len) = self.base_len {
                if len <= snapshot.len() {
                    snapshot.truncate(len);
                }
            }
            crate::memory::roots::push_off_frame_thread_roots(thread, &mut snapshot);
            crate::native::jni::collect_local_ref_roots(&mut snapshot);
        }
        {
            let mut origins = thread.gc_block_state.slot_origins.lock();
            origins.clear();
            for (fi, fr) in thread.frames.iter().enumerate() {
                let live_mask = fr.live_locals_mask_here();
                for li in 0..fr.locals_len() {
                    if let crate::types::Value::Object(Some(o)) = fr.get_local(li as u16) {
                        let a = o.as_ptr() as usize;
                        origins.push(crate::threading::jvm_thread::SlotOrigin {
                            frame: fi as u32,
                            idx: li as u32,
                            is_stack: false,
                            live: live_mask.is_live(li),
                            orig: a,
                            cur: a,
                        });
                    }
                }
                for si in 0..fr.stack.len() {
                    if let crate::types::Value::Object(Some(o)) = fr.stack.peek_at(si) {
                        let a = o.as_ptr() as usize;
                        origins.push(crate::threading::jvm_thread::SlotOrigin {
                            frame: fi as u32,
                            idx: si as u32,
                            is_stack: true,
                            live: true,
                            orig: a,
                            cur: a,
                        });
                    }
                }
            }
        }
        crate::runtime::interpreter::obsolete_frames::publish_blocked_frame_classes(thread);
        if self.audit {
            Self::audit_reused(shared, thread);
        }
        thread
            .gc_block_state
            .in_blocked_region
            .store(true, Ordering::Release);
        self.reuses += 1;
        true
    }

    /// Wave 66, `CRATONVM_DBG_BLOCKGC`: every live object slot of the frames
    /// is in the reused snapshot (as the full deposit's own
    /// `UNPUBLISHED-LIVE-SLOT` check asks of a fresh one).
    fn audit_reused(
        shared: &crate::vm::SharedVm,
        thread: &crate::threading::jvm_thread::JvmThread,
    ) {
        let snapshot = thread.root_snapshot.lock();
        let published: HashSet<usize> = snapshot.iter().map(|r| r.as_ptr() as usize).collect();
        let len = snapshot.len();
        drop(snapshot);
        let heap = &shared.mem.heap;
        let mut missing = 0usize;
        let mut check = |a: usize| {
            if !published.contains(&a) && heap.is_heap_addr(a).is_some() {
                missing += 1;
            }
        };
        for fr in thread.frames.iter() {
            let live_mask = fr.live_locals_mask_here();
            for li in 0..fr.locals_len() {
                if !live_mask.is_live(li) {
                    continue;
                }
                if let crate::types::Value::Object(Some(o)) = fr.get_local(li as u16) {
                    check(o.as_ptr() as usize);
                }
            }
            for si in 0..fr.stack.len() {
                if let crate::types::Value::Object(Some(o)) = fr.stack.peek_at(si) {
                    check(o.as_ptr() as usize);
                }
            }
        }
        eprintln!(
            "[DEBUG_PARK_REUSE] tid={} frames={} snapshot={len} missing={missing}",
            thread.thread_id.0,
            thread.frames.len()
        );
    }

    /// Is the region open: is the thread GC-blocked by it?
    #[inline]
    pub(crate) fn is_open(&self) -> bool {
        self.saved_java_state.is_some()
    }

    /// Open the region for `thread`, the calling thread's own, at its
    /// suspend point. Called with no debug-state lock held: the entry may
    /// arrive at a pause already requested and wait for its end. Answers
    /// whether it opened it -- not when it is already open, nor for a thread
    /// already GC-blocked (its own blocked region stands, and the park then
    /// answers no pause either way).
    pub(crate) fn enter(
        &mut self,
        shared: &crate::vm::SharedVm,
        thread: &mut crate::threading::jvm_thread::JvmThread,
    ) -> bool {
        use std::sync::atomic::Ordering;
        if self.is_open()
            || thread
                .gc_block_state
                .in_blocked_region
                .load(Ordering::Acquire)
        {
            return false;
        }
        // RUNNABLE while blocked here; stored before the deposit raises the
        // flag, so the flag never shows over a stale kind (a `Thread.sleep`
        // the thread left long ago).
        let saved = thread.gc_block_state.java_state.swap(0, Ordering::AcqRel);
        // As `begin_blocking_region_with_state`: a pause slept through may
        // grow the young arena and free the buffer a kept TLAB points into.
        thread.tlab.retire();
        let tid = thread.thread_id;
        let started = std::time::Instant::now();
        // Wave 66: the key this region's roots are taken under, read while
        // the thread is still a counted mutator (no pause can complete
        // before the flag goes up), so a collection that folds into the
        // region shows as a moved generation at the leave.
        let entered = StandingDeposit::read(shared, thread);
        // Wave 66: the standing deposit, when its key still holds; else the
        // full deposit.
        let reused = self.reuse && self.reuse_standing(shared, thread);
        self.standing = None;
        if !reused {
            let mut ctx = crate::vm::vm_exec::NativeContextImpl {
                shared,
                thread: &mut *thread,
            };
            crate::jit::conservative_roots::with_frozen_band_memo(None, || {
                ctx.deposit_root_snapshot();
            });
            self.base_len = Some(thread.root_snapshot.lock().len());
        }
        self.entered = self.reuse.then_some(entered);
        self.enter_ns = self
            .enter_ns
            .saturating_add(u64::try_from(started.elapsed().as_nanos()).unwrap_or(u64::MAX));
        if shared.mem.gc_barrier.mark_blocked_region_enter() {
            // A pause was already requested and may have counted this
            // thread: arrive for it once (the deposit raised the flag first,
            // hence `_auto`).
            let _ = shared.mem.gc_barrier.arrive_and_wait_auto(tid);
        }
        self.saved_java_state = Some(saved);
        self.entries += 1;
        true
    }

    /// Wave 68 ([`ParkRegionOpening`]): open the region at a stop-the-world
    /// request that found the park outside it, in place of the park's
    /// safepoint arrival. [`Self::enter`] as it is: the thread is still a
    /// counted mutator the request is waiting for, so the deposit runs
    /// before the pause can start, and the entry's
    /// `mark_blocked_region_enter` reports the request and arrives for it
    /// once (`arrive_and_wait_auto`), as a native that blocks during a pause
    /// does (`begin_blocking_region_with_state`). The caller drops its
    /// barrier waker first: once inside, a later request neither wakes the
    /// thread nor waits for it. Answers whether it opened the region.
    pub(crate) fn enter_at_pause(
        &mut self,
        shared: &crate::vm::SharedVm,
        thread: &mut crate::threading::jvm_thread::JvmThread,
    ) -> bool {
        let opened = self.enter(shared, thread);
        if opened {
            self.pause_entries += 1;
        }
        opened
    }

    /// Close the region, if open: leave the blocked count (waiting out a
    /// pause in progress), apply the collections' fixup to `thread`'s frames
    /// and off-frame references, and put its `java_state` back. Called with
    /// no debug-state lock held. Afterwards the thread is a running mutator
    /// again, its frames current.
    pub(crate) fn leave(
        &mut self,
        shared: &crate::vm::SharedVm,
        thread: &mut crate::threading::jvm_thread::JvmThread,
    ) {
        self.leave_inner(shared, thread, false);
    }

    /// Wave 68 ([`DebuggerGates::park_leave_keeps_unfolded_snapshot`],
    /// `docs/feature-designs/jvmti-delivery-threading.md`):
    /// [`Self::leave`] before heap work queued for any thread -- work that
    /// neither reads nor writes this thread's frames. With the switch, when
    /// no collection, fold or redefinition reached the region since its
    /// entry, the leave keeps the snapshot the entry deposited (or reused)
    /// instead of refreshing it from the frames, and skips the wake audit:
    /// the frames did not move and the thread ran nothing, so the snapshot
    /// is still exact (`NativeContextImpl::check_post_block_gc_keeping`).
    ///
    /// Sound until the thread is next GC-blocked: in between it is a counted
    /// mutator running Rust heap work, so a pause cannot complete without its
    /// arrival, and every arrival (`safepoint_check`) republishes the
    /// snapshot from the frames, which also bumps the publish sequence and
    /// so refuses the next entry's reuse. Without a pause, the next entry
    /// reuses the kept snapshot ([`Self::reuse_standing`], which cuts what
    /// earlier reuses appended before it appends again).
    pub(crate) fn leave_for_heap_work(
        &mut self,
        shared: &crate::vm::SharedVm,
        thread: &mut crate::threading::jvm_thread::JvmThread,
    ) {
        let keep = self.keep_unfolded;
        self.leave_inner(shared, thread, keep);
    }

    /// [`Self::leave`], keeping the entry's snapshot when `keep` and nothing
    /// folded into the region (wave 68).
    fn leave_inner(
        &mut self,
        shared: &crate::vm::SharedVm,
        thread: &mut crate::threading::jvm_thread::JvmThread,
        keep: bool,
    ) {
        let Some(saved) = self.saved_java_state.take() else {
            return;
        };
        let started = std::time::Instant::now();
        shared.mem.gc_barrier.mark_blocked_region_leave();
        let entered = self.entered.take();
        // Wave 68: what the region was entered under; the wake keeps the
        // snapshot only when the heap still stands as it did then.
        let keep_if = if keep {
            entered.map(|e| crate::vm::vm_exec::UnfoldedSince {
                gc_generation: e.generation,
                collections: e.collections,
                redefinitions: e.redefinitions,
            })
        } else {
            None
        };
        let kept = crate::vm::vm_exec::NativeContextImpl {
            shared,
            thread: &mut *thread,
        }
        .check_post_block_gc_keeping(keep_if);
        if kept {
            self.keeps += 1;
        } else {
            // Wave 68: the refresh is the snapshot's new full build.
            self.base_len = Some(thread.root_snapshot.lock().len());
        }
        // Wave 66: the refresh `check_post_block_gc` ended with stands for
        // the next entry when no collection and no redefinition reached the
        // region (no fixup was applied, no frame converted) and the frames
        // are the ones the region was entered with. Read with the flag
        // down: the thread is counted, so no pause completes after it.
        // Wave 68: so does a kept snapshot, under the same test.
        self.standing = entered.and_then(|entered| {
            let now = StandingDeposit::read(shared, thread);
            now.same_heap_and_frames(&entered).then_some(now)
        });
        self.leave_ns = self
            .leave_ns
            .saturating_add(u64::try_from(started.elapsed().as_nanos()).unwrap_or(u64::MAX));
        thread
            .gc_block_state
            .java_state
            .store(saved, std::sync::atomic::Ordering::Release);
    }
}

/// The 1 ms poll of every wait on the debug state with
/// [`DebuggerGates::park_condvar`] off ([`DebuggerGates::await_state_change`]).
pub(crate) const DEBUG_STATE_POLL: std::time::Duration = std::time::Duration::from_millis(1);

/// How long a GC-blocked thread held by a suspension
/// ([`hold_native_thread_while_suspended`], [`exit_monitor_while_suspended`])
/// waits for a notification before it looks again, with
/// [`DebuggerGates::park_condvar`] on (interpreter round i2 wave 54, lane
/// L6). Every resume notifies, so this bounds only a missed one; such a
/// thread answers no safepoint, so nothing else needs it awake.
const BLOCKED_WAIT_BOUND: std::time::Duration = std::time::Duration::from_millis(50);

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
        let split = self.shared.debug.debugger_gates.destroy_java_vm_split();
        if id_names_something(&self.shared, ids::thread_to_wire_split(split, tid)) {
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
        let (how, receiver_id) =
            if is_static && matches!(how, InvokeHow::Virtual | InvokeHow::NonVirtual) {
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
        Some((
            hit.name.clone(),
            hit.signature.clone(),
            hit.mod_bits & 0x0008 != 0,
        ))
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

    /// Interpreter round i3 wave 72, lane L6
    /// (`CRATONVM_JVMTI_STATE_SUSPENSION_HINT`): every suspension mutator
    /// leaves the lock-free hint equal to `any_suspension`, with no thread
    /// enrolled on a suspend word (the case the wave-63 word did not cover).
    #[test]
    fn w72_l6_the_suspension_hint_follows_every_suspension_mutator() {
        use std::sync::atomic::Ordering;
        let mut ds = DebugState::new();
        let hint = ds.suspension_hint_handle();
        let read = |h: &Arc<std::sync::atomic::AtomicBool>| h.load(Ordering::Acquire);
        assert!(!read(&hint));
        ds.suspend_thread(5);
        assert!(read(&hint), "ThreadReference.Suspend");
        ds.resume_thread(5);
        assert!(!read(&hint), "ThreadReference.Resume");
        ds.suspend_all();
        assert!(read(&hint), "VirtualMachine.Suspend");
        ds.resume_vm();
        assert!(!read(&hint), "VirtualMachine.Resume");
        assert!(ds.jvmti_suspend(6));
        assert!(read(&hint), "JVMTI SuspendThread");
        assert!(ds.jvmti_resume(6));
        assert!(!read(&hint), "JVMTI ResumeThread");
        ds.begin_handshake(7);
        assert!(read(&hint), "stack-read handshake");
        ds.end_handshake(7);
        assert!(!read(&hint));
        ds.suspend_thread(8);
        ds.suspend_all();
        ds.resume_all();
        assert!(!read(&hint), "detach");
        for tid in [5, 6, 7, 8] {
            assert_eq!(read(&hint), ds.is_thread_suspended(tid));
        }
    }

    /// Interpreter round i7 wave 135, lane L1 (`CRATONVM_JVMTI_STATE_SUSPEND_WORD`,
    /// i64-L1 item 1): a subscribed thread's C JVMTI state word follows every
    /// suspension mutator, whether it is suspended alone, VM-wide, by a JVMTI
    /// agent or by a stack read, and goes back to unpublished when the thread
    /// ends.
    #[test]
    fn i135_l1_the_state_suspend_word_follows_every_suspension_mutator() {
        use crate::threading::jvm_thread::{
            GcBlockState, JVMTI_STATE_SUSPEND_NO, JVMTI_STATE_SUSPEND_UNPUBLISHED,
            JVMTI_STATE_SUSPEND_YES,
        };
        use std::sync::atomic::Ordering;
        let mut ds = DebugState::new();
        let state = Arc::new(GcBlockState::new());
        let other = Arc::new(GcBlockState::new());
        let word = |s: &Arc<GcBlockState>| s.jvmti_state_suspended.load(Ordering::Acquire);
        assert_eq!(word(&state), JVMTI_STATE_SUSPEND_UNPUBLISHED);
        assert!(!ds.subscribe_state_suspend_word(5, Arc::clone(&state)));
        assert!(ds.state_suspend_word_subscribed(5));
        assert_eq!(word(&state), JVMTI_STATE_SUSPEND_NO, "published at the subscription");
        ds.suspend_thread(5);
        assert_eq!(word(&state), JVMTI_STATE_SUSPEND_YES, "ThreadReference.Suspend");
        ds.resume_thread(5);
        assert_eq!(word(&state), JVMTI_STATE_SUSPEND_NO, "ThreadReference.Resume");
        ds.suspend_all();
        assert_eq!(word(&state), JVMTI_STATE_SUSPEND_YES, "VirtualMachine.Suspend");
        ds.resume_vm();
        assert_eq!(word(&state), JVMTI_STATE_SUSPEND_NO, "VirtualMachine.Resume");
        assert!(ds.jvmti_suspend(5));
        assert_eq!(word(&state), JVMTI_STATE_SUSPEND_YES, "JVMTI SuspendThread");
        assert!(ds.jvmti_resume(5));
        assert_eq!(word(&state), JVMTI_STATE_SUSPEND_NO, "JVMTI ResumeThread");
        ds.begin_handshake(5);
        assert_eq!(word(&state), JVMTI_STATE_SUSPEND_YES, "stack-read handshake");
        ds.end_handshake(5);
        assert_eq!(word(&state), JVMTI_STATE_SUSPEND_NO);
        // Another thread's suspension leaves this word alone; a subscription
        // made while suspended publishes YES.
        assert!(ds.jvmti_suspend(6));
        assert_eq!(word(&state), JVMTI_STATE_SUSPEND_NO);
        assert!(ds.subscribe_state_suspend_word(6, Arc::clone(&other)));
        assert_eq!(word(&other), JVMTI_STATE_SUSPEND_YES);
        for tid in [5, 6] {
            let s = if tid == 5 { &state } else { &other };
            assert_eq!(word(s) == JVMTI_STATE_SUSPEND_YES, ds.is_thread_suspended(tid));
        }
        // The thread ends: unpublished, unsubscribed.
        assert!(ds.forget_jvmti_suspension(6));
        assert!(!ds.state_suspend_word_subscribed(6));
        assert_eq!(word(&other), JVMTI_STATE_SUSPEND_UNPUBLISHED);
        ds.suspend_all();
        assert_eq!(word(&other), JVMTI_STATE_SUSPEND_UNPUBLISHED, "no longer written");
        assert_eq!(word(&state), JVMTI_STATE_SUSPEND_YES);
    }

    /// Interpreter round i7 wave 136, lane L1b: a thread subscribed again
    /// after its `ThreadDeath` unsubscribed it (a `GetThreadState` before it
    /// left the registry) is dropped by a later subscription's sweep once its
    /// `JvmThread` is gone; a live one stays.
    #[test]
    fn i136_l1b_subscriptions_of_gone_threads_are_swept() {
        use crate::threading::jvm_thread::{GcBlockState, GcBlockStateDropNotice};
        let mut ds = DebugState::new();
        let live = Arc::new(GcBlockState::new());
        assert!(!ds.subscribe_state_suspend_word(1, Arc::clone(&live)));
        for tid in 2..(STATE_SUSPEND_PRUNE_FLOOR as u64 + 1) {
            let gone = Arc::new(GcBlockState::new());
            ds.subscribe_state_suspend_word(tid, Arc::clone(&gone));
            drop(GcBlockStateDropNotice::new(&gone));
        }
        assert_eq!(ds.state_suspend_words.len(), STATE_SUSPEND_PRUNE_FLOOR);
        let next = Arc::new(GcBlockState::new());
        ds.subscribe_state_suspend_word(10_000, Arc::clone(&next));
        assert_eq!(ds.state_suspend_words.len(), 2, "the gone threads are swept");
        assert!(ds.state_suspend_word_subscribed(1));
        assert!(ds.state_suspend_word_subscribed(10_000));
        assert_eq!(ds.state_suspend_prune_at, STATE_SUSPEND_PRUNE_FLOOR);
    }

    /// Interpreter round i2 wave 54, lane L1 (option 1 of
    /// `i53-L1-proposal-retire-the-d14-bridge-into-the-unused-jvmti-event-manager`):
    /// with the bridge's listener gate on, `legacy_jvmti_listens` follows the
    /// older `jvmti::EventManager`'s enables through the shared mask; with it
    /// off it always answers `true`, the bridge as before.
    #[test]
    fn the_bridge_listener_gate_follows_the_older_managers_enables() {
        use crate::jvmti::{EventManager, JvmtiEvent};
        let mut manager = EventManager::new();
        let gated = DebuggerGates {
            bridge_listener_gate: true,
            ..DebuggerGates::with_legacy_jvmti_events(manager.enabled_mask_handle())
        };
        let open = DebuggerGates {
            bridge_listener_gate: false,
            ..DebuggerGates::with_legacy_jvmti_events(manager.enabled_mask_handle())
        };
        assert!(!gated.legacy_jvmti_listens(JvmtiEvent::ClassLoad));
        assert!(open.legacy_jvmti_listens(JvmtiEvent::ClassLoad));
        manager.set_event_notification_mode(JvmtiEvent::ClassLoad, true);
        assert!(gated.legacy_jvmti_listens(JvmtiEvent::ClassLoad));
        assert!(!gated.legacy_jvmti_listens(JvmtiEvent::ClassPrepare));
        assert!(!gated.legacy_jvmti_listens(JvmtiEvent::ObjectFree));
        manager.set_event_notification_mode(JvmtiEvent::ObjectFree, true);
        assert!(gated.legacy_jvmti_listens(JvmtiEvent::ObjectFree));
        manager.set_event_notification_mode(JvmtiEvent::ClassLoad, false);
        assert!(!gated.legacy_jvmti_listens(JvmtiEvent::ClassLoad));
        assert!(open.legacy_jvmti_listens(JvmtiEvent::ClassPrepare));
    }

    /// Interpreter round i6 wave 133, lane L1
    /// (`CRATONVM_JVMTI_C_FUNNEL_RECORD_FROM_EXCUSE`,
    /// `i62-L6-proposal-retire-the-c-agents-funnel-callback-record`): with
    /// the switch on and the excuse word carrying the callback, a bound C env
    /// keeps leaf natives on the full funnel but arms no funnel record; with
    /// the switch off, or the word not carrying the callback
    /// (`CRATONVM_HANDSHAKE_NATIVE_CALLBACK=0`), the record is armed as in
    /// wave 61. A debugger session arms it either way.
    #[test]
    fn w133_l1_a_c_env_skips_the_funnel_record_only_when_the_excuse_word_carries_it() {
        let gates = |switch: bool| DebuggerGates {
            c_running_native: true,
            c_funnel_record_from_excuse: switch,
            ..DebuggerGates::new()
        };
        // Switch on, word carries the callback: leaves only.
        let skip = gates(true);
        assert!(!skip.running_native_armed() && !skip.leaf_takes_full_funnel());
        skip.set_native_env_present(true);
        assert!(skip.native_env_present() && skip.c_running_native_armed());
        assert!(!skip.running_native_armed(), "no funnel record of its own");
        assert!(skip.leaf_takes_full_funnel(), "leaves still raise the word");
        skip.set_session_attached(true);
        assert!(skip.running_native_armed(), "a debugger still records");
        skip.set_session_attached(false);
        assert!(!skip.running_native_armed() && skip.leaf_takes_full_funnel());
        // Switch on, word does not carry it (the kill switch): wave 61.
        let kill = gates(true);
        kill.set_native_env_present(false);
        assert!(kill.running_native_armed() && kill.leaf_takes_full_funnel());
        // Switch off: wave 61, whatever the word carries.
        let off = gates(false);
        off.set_native_env_present(true);
        assert!(off.running_native_armed() && off.leaf_takes_full_funnel());
        // `CRATONVM_JVMTI_C_RUNNING_NATIVE=0`: nothing for the C env.
        let none = DebuggerGates {
            c_running_native: false,
            c_funnel_record_from_excuse: true,
            ..DebuggerGates::new()
        };
        none.set_native_env_present(true);
        assert!(!none.running_native_armed() && !none.leaf_takes_full_funnel());
    }

    /// Wave 133, lane L1: the same through a VM's funnel. With the switch on
    /// (the VM's excuse word carries the callback by default), a C env bound
    /// leaves `GcBlockState::funnel_native_callback` untouched and
    /// `enter_running_native` records nothing; off, the record is kept and
    /// restored as `i61_l6_the_funnel_native_callback_is_recorded_and_restored`
    /// pins.
    #[test]
    fn w133_l1_the_funnel_keeps_no_c_record_while_the_excuse_word_carries_the_callback() {
        use std::sync::atomic::Ordering;
        for switch in [true, false] {
            let shared = std::sync::Arc::new(cratonvm_types::flags::with_thread_overrides(
                &[(
                    "CRATONVM_JVMTI_C_FUNNEL_RECORD_FROM_EXCUSE",
                    Some(if switch { "1" } else { "0" }),
                )],
                || crate::vm::SharedVm::new(crate::config::VmConfig::default()),
            ));
            let barrier = &shared.mem.gc_barrier;
            let carries = barrier.native_excuse_enabled() && barrier.excused_native_callback();
            let gates = &shared.debug.debugger_gates;
            gates.set_native_env_present(carries);
            let mut thread = crate::threading::jvm_thread::JvmThread::new(
                crate::threading::jvm_thread::ThreadId(0x0133),
                "w133-funnel",
            );
            let prior =
                crate::runtime::interpreter::enter_running_native(&shared, &mut thread, 0x1000);
            let cell = thread
                .gc_block_state
                .funnel_native_callback
                .load(Ordering::Acquire);
            if !gates.c_running_native_armed() {
                // `CRATONVM_JVMTI_C_RUNNING_NATIVE=0` in the environment.
                assert_eq!(prior, None);
            } else if switch && carries {
                assert_eq!(prior, None, "switch on: no record for the C env");
                assert_eq!(cell, 0);
                assert!(gates.leaf_takes_full_funnel());
            } else {
                assert!(prior.is_some(), "switch off or word bare: wave 61's record");
                assert_eq!(cell, 0x1000);
            }
            crate::runtime::interpreter::leave_running_native(&mut thread, prior);
            assert_eq!(
                thread
                    .gc_block_state
                    .funnel_native_callback
                    .load(Ordering::Acquire),
                0
            );
        }
    }

    /// gcf f2/f: a debugger read of a liveness-dead reference local answers
    /// the object while its address is still an object start, and `null` for
    /// an address that names none (a referent the collector freed); a live
    /// local and a non-reference value pass unchanged.
    #[test]
    fn gcf_f2f_a_dead_reference_local_is_answered_only_while_it_names_an_object() {
        std::thread::spawn(|| {
            use crate::types::Value;
            let shared =
                std::sync::Arc::new(crate::vm::SharedVm::new(crate::config::VmConfig::default()));
            let o = shared
                .mem
                .heap
                .alloc_object(cratonvm_types::ClassId::new(7), 0);
            // SAFETY: never dereferenced by the code under test (it is only
            // probed); non-null and 8-aligned as `from_raw` requires.
            let stale = unsafe { cratonvm_types::ObjectRef::from_raw(0x1000 as *mut u8) };
            // `return` at pc 0: every slot but 0 is dead.
            let code: std::sync::Arc<[u8]> = std::sync::Arc::from(vec![0xb1u8]);
            let dead = crate::runtime::local_liveness::live_locals_mask(&code, &[], 3, [0, 0]);
            assert!(dead.is_live(0) && !dead.is_live(1));
            let all = crate::runtime::local_liveness::LiveMask::all_live();
            let screen = |live, slot, v| screen_dead_local(&shared, live, slot, v);
            assert_eq!(
                screen(&dead, 1, Value::Object(Some(stale))),
                Value::Object(None),
                "a dead slot naming no object is null"
            );
            assert_eq!(
                screen(&dead, 1, Value::Object(Some(o))),
                Value::Object(Some(o))
            );
            assert_eq!(
                screen(&all, 1, Value::Object(Some(stale))),
                Value::Object(Some(stale)),
                "a live slot is not screened"
            );
            assert_eq!(screen(&dead, 2, Value::Int(0x1000)), Value::Int(0x1000));
            assert_eq!(screen(&dead, 1, Value::Object(None)), Value::Object(None));
        })
        .join()
        .expect("test thread");
    }

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
        let two =
            b"SMAP\nH.kt\nKotlin\n*S Kotlin\n*F\n+ 1 H.kt\np/H.kt\n+ 2 Inline.kt\np/Inline.kt\n\
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
        assert_eq!(
            refusal(CS_THREAD_REF, 0x7fff_fff0),
            Some(ERR_INVALID_OBJECT)
        );
        assert_eq!(
            refusal(CS_STACK_FRAME, 0x7fff_fff0),
            Some(ERR_INVALID_OBJECT)
        );
        assert_eq!(refusal(CS_REF_TYPE, 0x7fff_fff0), Some(ERR_INVALID_OBJECT));
        assert_eq!(
            refusal(CS_THREAD_REF, ids::NO_VM_ID),
            None,
            "null is the handler's"
        );
        assert_eq!(
            refusal(CS_THREAD_REF, commands::SYSTEM_THREAD_GROUP_ID),
            None
        );
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

    /// Interpreter round i6 wave 127 (lane L1,
    /// `CRATONVM_JDWP_EVENT_OBJECT_HOLD`): an event's object is held strong
    /// (held once however often it is offered) until the first object
    /// command naming it; a command naming another id, or of another set,
    /// leaves it held; then its id is weak again, as before the event.
    #[test]
    fn an_event_object_is_held_until_the_first_command_naming_it() {
        use crate::native::jni::JniGlobalKind;
        let shared = Arc::new(crate::vm::SharedVm::new(crate::config::VmConfig::default()));
        let Some(arr) = shared.mem.heap.try_alloc_array_full(
            crate::classloading::ClassId::new(0),
            cratonvm_types::ArrayElementType::Int,
            4,
        ) else {
            return; // no room in a stripped test heap
        };
        // Takes the debug-state lock, then the JNI table's: never call it
        // with the debug state locked.
        let kind = |id: u64| {
            let handle = shared.debug.debug_state.lock().objects.handle_of(id);
            handle.and_then(|h| shared.natives.jni_global_refs.lock().kind(h))
        };
        let id = {
            let mut ds = shared.debug.debug_state.lock();
            ds.event_object_hold = true;
            let id = export_object(&shared, &mut ds, arr, ObjectExport::Sent);
            hold_event_object(&shared, &mut ds, arr);
            hold_event_object(&shared, &mut ds, arr);
            assert_eq!(ds.event_object_holds.len(), 1, "held once");
            id
        };
        assert_eq!(kind(id), Some(JniGlobalKind::Strong));
        {
            let mut ds = shared.debug.debug_state.lock();
            let other = (id + 1).to_be_bytes();
            release_event_hold_named(&shared, &mut ds, commands::CS_OBJECT_REF, &other);
            release_event_hold_named(&shared, &mut ds, commands::CS_REF_TYPE, &id.to_be_bytes());
        }
        assert_eq!(kind(id), Some(JniGlobalKind::Strong), "still held");
        {
            let mut ds = shared.debug.debug_state.lock();
            release_event_hold_named(&shared, &mut ds, commands::CS_OBJECT_REF, &id.to_be_bytes());
            assert!(ds.event_object_holds.is_empty());
        }
        assert_eq!(kind(id), Some(JniGlobalKind::Weak), "weak again");
        {
            let mut ds = shared.debug.debug_state.lock();
            ds.event_object_hold = false;
            hold_event_object(&shared, &mut ds, arr);
            assert!(ds.event_object_holds.is_empty(), "=0: no hold");
            ds.objects.release_all();
            release_disposed_objects(&shared, &mut ds);
        }
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

    /// Interpreter round i1 wave 49, lane L1: a suspension's settle wait
    /// waits only for a suspended RUNNING thread with no listing of this
    /// suspension; it returns at once for a thread the VM does not know (it
    /// is not alive), and records nothing when the switch is off.
    #[test]
    fn a_suspension_waits_only_for_running_threads_without_a_listing() {
        let mut ds = DebugState::new();
        ds.thread_names.insert(3, "spinner".to_string());
        assert!(!suspension_on_its_way(&ds, 3), "not suspended");
        ds.suspend_thread(3);
        assert!(suspension_on_its_way(&ds, 3));
        // Wave 53 (lane L6): a status read at the last refresh says nothing
        // about now (the registry's fresh answer decides); a dead thread is
        // never waited for.
        ds.thread_statuses
            .insert(3, commands::THREAD_STATUS_SLEEPING);
        assert!(suspension_on_its_way(&ds, 3), "it may have woken since");
        ds.thread_statuses.insert(3, commands::THREAD_STATUS_ZOMBIE);
        assert!(!suspension_on_its_way(&ds, 3), "a dead thread is stopped");
        ds.thread_statuses
            .insert(3, commands::THREAD_STATUS_RUNNING);
        ds.thread_frames.insert(3, Vec::new());
        assert!(!suspension_on_its_way(&ds, 3), "it published its listing");
        let shared = Arc::new(crate::vm::SharedVm::new(crate::config::VmConfig::default()));
        {
            let mut ds = shared.debug.debug_state.lock();
            ds.thread_names.insert(0x7fff_fff4, "ghost".to_string());
            ds.suspend_all();
        }
        let start = std::time::Instant::now();
        await_suspension_settled(&shared, None);
        await_suspension_settled(&shared, Some(0x7fff_fff4));
        assert!(start.elapsed() < SUSPEND_POINT_WAIT);
        assert!(shared.debug.debug_state.lock().suspend_laggards.is_empty());
    }

    /// Interpreter round i2 wave 53, lane L6: the settle wait asks the
    /// compiled loops to leave again only once a thread is late (4 ms), then
    /// each time the wait doubles: at most six times within the bound.
    #[test]
    fn a_late_suspension_asks_the_compiled_loops_again_on_a_doubling_schedule() {
        let ms = std::time::Duration::from_millis;
        assert!(!suspend_nudge_due(ms(0), 0));
        assert!(!suspend_nudge_due(ms(3), 0));
        assert!(suspend_nudge_due(ms(4), 0));
        assert!(!suspend_nudge_due(ms(7), 1));
        assert!(suspend_nudge_due(ms(8), 1));
        assert!(suspend_nudge_due(ms(16), 2));
        let mut nudges = 0u32;
        let mut at = ms(0);
        while at < SUSPEND_POINT_WAIT {
            if suspend_nudge_due(at, nudges) {
                nudges += 1;
            }
            at += ms(1);
        }
        assert_eq!(nudges, 6);
        // No shift overflow however often it asked.
        assert!(!suspend_nudge_due(ms(250), u32::MAX));
        assert!(suspend_nudge_due(ms(u64::from(u32::MAX)), u32::MAX));
    }

    /// Interpreter round i1 wave 38, lane L1: an event set applied as
    /// `EVENT_THREAD` reports the byte its first such event's request sent
    /// when JDWP does not define it; every other set reports its policy.
    #[test]
    fn an_event_set_echoes_an_undefined_suspend_policy_byte() {
        use events::SuspendPolicy;
        assert_eq!(
            event_set_policy_byte(SuspendPolicy::EventThread, Some(7)),
            7
        );
        assert_eq!(
            event_set_policy_byte(SuspendPolicy::EventThread, Some(1)),
            1
        );
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
        ds.class_signatures
            .insert(81, "Ljava/lang/Math;".to_string());

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
        assert!(
            !gates.concerns_method(82, "other", "()V"),
            "no suspend point"
        );
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

    /// Interpreter round i6 wave 117, lane L1
    /// (`CRATONVM_DEBUG_SETTLE_PENDING_COUNTER`): a listing publication hands
    /// a thread the settle wait counts to its next round exactly once, and
    /// one it does not count (or any, outside a wait) to nobody.
    #[test]
    fn i117_l1_a_counted_arrival_is_handed_over_once() {
        let mut st = DebugState::new();
        note_settle_arrival(&mut st, 7);
        assert!(st.settle_arrived.is_empty(), "no wait in progress");
        st.settle_wanted = [7u64, 9].into_iter().collect();
        note_settle_arrival(&mut st, 7);
        note_settle_arrival(&mut st, 7);
        note_settle_arrival(&mut st, 8);
        assert_eq!(st.settle_arrived, vec![7], "once, and only a counted thread");
        assert!(st.settle_wanted.contains(&9) && !st.settle_wanted.contains(&7));
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
    /// or a VM-wide one, or (wave 55) a C JVMTI agent's with no session — and
    /// released by the resume or by a detach; with no suspension it passes
    /// straight through.
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
        // The detach's own order (`run_jdwp_server`): the session lowered,
        // then every JDWP suspension resumed and published.
        let detach = || {
            shared.debug.debugger_gates.set_session_attached(false);
            let mut ds = shared.debug.debug_state.lock();
            ds.resume_all();
            publish_debugger_gates(&shared, &ds);
        };
        assert!(
            held_until(&detach, Duration::from_secs(10)),
            "held under VirtualMachine.Suspend, released by the detach"
        );
        // Wave 55 (lane L6): a C JVMTI agent's `SuspendThread` holds it with
        // no session attached, and its `ResumeThread` releases it.
        {
            let mut ds = shared.debug.debug_state.lock();
            assert!(ds.jvmti_suspend(TID));
            publish_debugger_gates(&shared, &ds);
        }
        let jvmti_resume = || {
            let mut ds = shared.debug.debug_state.lock();
            assert!(ds.jvmti_resume(TID));
            publish_debugger_gates(&shared, &ds);
        };
        assert!(
            held_until(&jvmti_resume, Duration::from_secs(10)),
            "held under an agent's SuspendThread with no session, released by its ResumeThread"
        );
        {
            let mut ds = shared.debug.debug_state.lock();
            ds.resume_vm();
            publish_debugger_gates(&shared, &ds);
        }
        // Interpreter round i5 wave 106 (lane L1): a thread the last listing
        // saw ended (ZOMBIE) is held by its own suspension too, although the
        // live-thread gate (`suspension_in_force`) leaves it out.
        {
            let mut ds = shared.debug.debug_state.lock();
            ds.thread_statuses
                .insert(TID, commands::THREAD_STATUS_ZOMBIE);
            ds.suspend_thread(TID);
            publish_debugger_gates(&shared, &ds);
        }
        assert!(!shared.debug.debugger_gates.suspension_in_force());
        assert!(shared.debug.debugger_gates.suspension_counted());
        let resume_ended = || {
            let mut ds = shared.debug.debug_state.lock();
            ds.resume_thread(TID);
            publish_debugger_gates(&shared, &ds);
        };
        assert!(
            held_until(&resume_ended, Duration::from_secs(10)),
            "an ended thread held by its own suspension, released by the resume"
        );
        assert!(!shared.debug.debugger_gates.suspension_counted());
        shared.debug.debug_state.lock().thread_statuses.remove(&TID);
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

    /// gci i16/sn (`CRATONVM_DEBUG_SUSPEND_AT_NATIVE_TRANSITION`): the hold at
    /// a native -> VM transition is armed only with the switch on and a
    /// suspension in force; it holds the thread under an agent's
    /// `SuspendThread` (released by its `ResumeThread`) and under the
    /// debugger's own suspension (released by `ThreadReference.Resume`), and
    /// passes straight through for a stack read's handshake alone, for
    /// another thread's suspension and for a thread parked at a suspend point.
    #[test]
    fn gci_i16sn_the_native_transition_hold_follows_the_suspension() {
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::time::Duration;
        const TID: u64 = 0x0016_5A01;
        let _lock = crate::runtime::jvmti::jvmti_registry_test_lock();
        let shared = Arc::new(crate::vm::SharedVm::new(crate::config::VmConfig::default()));
        let gates = &shared.debug.debugger_gates;
        gates.set_suspend_at_native_transition(false);
        assert!(!native_transition_hold_armed(&shared), "nothing in force");
        {
            let mut ds = shared.debug.debug_state.lock();
            assert!(ds.jvmti_suspend(TID));
            publish_debugger_gates(&shared, &ds);
        }
        assert!(
            !native_transition_hold_armed(&shared),
            "the switch off: never armed, whatever is suspended"
        );
        gates.set_suspend_at_native_transition(true);
        assert!(native_transition_hold_armed(&shared));

        // Returns whether the hold was still waiting after 30 ms and ended
        // within `wait` of `release`.
        let held_until = |release: &dyn Fn(), wait: Duration| -> bool {
            let done = Arc::new(AtomicBool::new(false));
            let worker = {
                let shared = Arc::clone(&shared);
                let done = Arc::clone(&done);
                std::thread::spawn(move || {
                    let held = hold_at_native_transition_while_suspended(&shared, TID);
                    done.store(true, Ordering::Release);
                    held
                })
            };
            std::thread::sleep(Duration::from_millis(30));
            let waiting = !done.load(Ordering::Acquire);
            release();
            let deadline = std::time::Instant::now() + wait;
            while !done.load(Ordering::Acquire) && std::time::Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(1));
            }
            let released = done.load(Ordering::Acquire);
            let answered_held = worker.join().unwrap_or(false);
            waiting && released && answered_held
        };

        // An agent's SuspendThread (marked above), lifted by its ResumeThread.
        let jvmti_resume = || {
            let mut ds = shared.debug.debug_state.lock();
            assert!(ds.jvmti_resume(TID));
            publish_debugger_gates(&shared, &ds);
        };
        assert!(
            held_until(&jvmti_resume, Duration::from_secs(10)),
            "held under the agent's SuspendThread, released by its ResumeThread"
        );
        assert!(!native_transition_hold_armed(&shared), "resumed: disarmed");

        // The debugger's own suspension, lifted by ThreadReference.Resume.
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
            "held under the debugger's suspension, released by its resume"
        );

        // A stack read's handshake alone: armed, but never held.
        {
            let mut ds = shared.debug.debug_state.lock();
            ds.begin_handshake(TID);
            publish_debugger_gates(&shared, &ds);
        }
        assert!(native_transition_hold_armed(&shared));
        assert!(
            !hold_at_native_transition_while_suspended(&shared, TID),
            "a handshake does not hold a thread at its transition"
        );
        {
            let mut ds = shared.debug.debug_state.lock();
            ds.end_handshake(TID);
            publish_debugger_gates(&shared, &ds);
        }

        // Another thread's suspension does not hold this one.
        {
            let mut ds = shared.debug.debug_state.lock();
            assert!(ds.jvmti_suspend(TID + 1));
            publish_debugger_gates(&shared, &ds);
        }
        assert!(!hold_at_native_transition_while_suspended(&shared, TID));
        {
            let mut ds = shared.debug.debug_state.lock();
            assert!(ds.jvmti_resume(TID + 1));
            publish_debugger_gates(&shared, &ds);
        }

        // A thread parked at a suspend point (its park's own work) is not held.
        {
            let mut ds = shared.debug.debug_state.lock();
            assert!(ds.jvmti_suspend(TID));
            ds.note_parked(TID, true);
            publish_debugger_gates(&shared, &ds);
        }
        assert!(!hold_at_native_transition_while_suspended(&shared, TID));
        {
            let mut ds = shared.debug.debug_state.lock();
            ds.note_parked(TID, false);
            assert!(ds.jvmti_resume(TID));
            publish_debugger_gates(&shared, &ds);
        }
        gates.set_suspend_at_native_transition(false);
    }

    /// Interpreter round i2 wave 54, lane L6 (`CRATONVM_DEBUG_PARK_CONDVAR`):
    /// with the condvar a wait on the debug state ends at the notification,
    /// not at its bound, and says so; without it, the wait is the 1 ms poll
    /// and no notification is reported.
    #[test]
    fn a_wait_on_the_debug_state_ends_at_its_notification() {
        use std::time::{Duration, Instant};
        let state = Arc::new(parking_lot::Mutex::new(DebugState::new()));
        let gates = Arc::new(DebuggerGates {
            park_condvar: true,
            ..DebuggerGates::new()
        });
        state.lock().suspend_thread(7);
        let waiting = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let waiter = {
            let (state, gates) = (Arc::clone(&state), Arc::clone(&gates));
            let waiting = Arc::clone(&waiting);
            std::thread::spawn(move || {
                let mut ds = state.lock();
                let start = Instant::now();
                // Raised under the lock, which only the wait lets go: once
                // the main thread holds the lock after seeing it, this waits.
                waiting.store(true, std::sync::atomic::Ordering::Release);
                while ds.suspend_count(7) > 0 {
                    if !gates.await_state_change(&mut ds, Duration::from_secs(30)) {
                        return (false, start.elapsed());
                    }
                }
                (true, start.elapsed())
            })
        };
        while !waiting.load(std::sync::atomic::Ordering::Acquire) {
            std::thread::yield_now();
        }
        {
            let mut ds = state.lock();
            ds.resume_thread(7);
            gates.notify_state_changed();
        }
        let (notified, waited) = waiter.join().expect("waiter");
        assert!(notified, "a notification ended the wait");
        assert!(
            waited < Duration::from_secs(10),
            "not the 30 s bound: {waited:?}"
        );

        // Off: the poll, never reported as a notification.
        let polled = DebuggerGates {
            park_condvar: false,
            ..DebuggerGates::new()
        };
        let mut ds = state.lock();
        let start = Instant::now();
        assert!(!polled.await_state_change(&mut ds, Duration::from_secs(30)));
        assert!(
            start.elapsed() < Duration::from_secs(10),
            "the poll, not the bound"
        );
    }

    /// Interpreter round i2 wave 63, lane L6
    /// (`CRATONVM_DEBUG_THREAD_SUSPEND_WORD`): an enrolled thread's suspend
    /// word says what `is_thread_suspended`, `holds_off_monitor_entry` and
    /// the task queue would answer, after every kind of suspension and
    /// resume, and its change sequence moves exactly when the answer does.
    #[test]
    fn an_enrolled_threads_suspend_word_follows_every_suspension() {
        use std::sync::atomic::Ordering;
        const TID: u64 = 0x00F6_3001;
        let gates = DebuggerGates {
            thread_suspend_word: true,
            ..DebuggerGates::new()
        };
        type Gbs = crate::threading::jvm_thread::GcBlockState;
        let state = Arc::new(Gbs::new());
        let mut ds = DebugState::new();
        let word = |s: &Arc<Gbs>| s.debugger_suspend_word.load(Ordering::Acquire);
        let flags = |w: u32| w & SW_FLAGS;
        let check = |ds: &DebugState, s: &Arc<Gbs>, what: &str| {
            assert_eq!(flags(word(s)), ds.suspend_word_flags(TID), "{what}");
            assert_eq!(
                word(s) & SW_SUSPENDED != 0,
                ds.is_thread_suspended(TID),
                "{what}: suspended"
            );
            assert_eq!(
                word(s) & SW_HOLDS_OFF != 0,
                ds.holds_off_monitor_entry(TID),
                "{what}: holds off"
            );
        };

        assert_eq!(word(&state), 0, "not enrolled: 0");
        ds.suspend_thread(TID);
        assert_eq!(
            word(&state),
            0,
            "a change before the enrollment writes nothing"
        );
        let mut wait = SuspendWordWait::enroll(&gates, &mut ds, TID, &state);
        assert!(wait.active() && ds.suspend_word_enrolled(TID));
        check(&ds, &state, "enrolled while suspended");
        assert_ne!(word(&state) & SW_ENROLLED, 0);

        let before = word(&state);
        ds.suspend_thread(TID);
        assert_eq!(
            word(&state),
            before,
            "count 1 -> 2: the answer stands, the word too"
        );
        ds.resume_thread(TID);
        assert_eq!(word(&state), before, "2 -> 1");
        ds.resume_thread(TID);
        check(&ds, &state, "resumed");
        assert_ne!(word(&state), before, "1 -> 0 moves the word");

        ds.suspend_all();
        check(&ds, &state, "VirtualMachine.Suspend");
        ds.resume_thread(TID);
        check(&ds, &state, "its own resume under the VM-wide suspension");
        ds.suspend_thread(TID);
        ds.resume_vm();
        check(&ds, &state, "VirtualMachine.Resume");
        ds.suspend_all();
        ds.suspend_thread(TID);
        ds.resume_all();
        check(&ds, &state, "detach");

        assert!(ds.jvmti_suspend(TID));
        check(&ds, &state, "JVMTI SuspendThread");
        assert!(ds.jvmti_resume(TID));
        check(&ds, &state, "JVMTI ResumeThread");
        ds.begin_handshake(TID);
        check(
            &ds,
            &state,
            "a stack read's handshake (suspended, not holding off)",
        );
        assert_eq!(word(&state) & SW_HOLDS_OFF, 0);
        ds.end_handshake(TID);
        check(&ds, &state, "handshake ended");
        assert!(ds.jvmti_suspend(TID));
        assert!(ds.forget_jvmti_suspension(TID));
        check(&ds, &state, "a dead thread's JVMTI suspension forgotten");

        // The task bit: queued, taken.
        ds.suspend_thread(TID);
        ds.note_parked(TID, true);
        let (tx, _rx) = std::sync::mpsc::channel::<()>();
        ds.parked_tasks.insert(
            TID,
            ParkedTask {
                run: Box::new(move |_: &crate::vm::SharedVm, _: &mut ParkedThread| {
                    ParkedReply(Box::new(move || {
                        let _ = tx.send(());
                    }))
                }),
                invocation: false,
                any_thread: false,
            },
        );
        ds.republish_suspend_word(TID);
        assert_ne!(word(&state) & SW_TASK, 0, "a queued task");
        assert!(ds.take_parked_task(TID).is_some());
        assert_eq!(word(&state) & SW_TASK, 0, "taken");
        ds.note_task_done(TID);
        ds.note_parked(TID, false);
        check(&ds, &state, "left the park");

        // Another thread's suspension does not move this word.
        let before = word(&state);
        ds.suspend_thread(TID + 1);
        ds.resume_thread(TID + 1);
        assert_eq!(word(&state), before, "another thread");

        wait.leave(&mut ds, TID);
        assert!(!ds.suspend_word_enrolled(TID));
        assert_eq!(word(&state), 0, "left: 0 again");
        ds.resume_thread(TID);
        assert_eq!(word(&state), 0, "a change after the leave writes nothing");
    }

    /// Wave 63, lane L6: enrollments nest (a park inside a task an outer park
    /// runs) and only the last leave clears the word; with the switch off
    /// nothing enrolls and the wait never skips a locked round.
    #[test]
    fn suspend_word_enrollments_nest_and_the_switch_off_enrolls_nothing() {
        use std::sync::atomic::Ordering;
        const TID: u64 = 0x00F6_3002;
        let on = DebuggerGates {
            thread_suspend_word: true,
            ..DebuggerGates::new()
        };
        let off = DebuggerGates {
            thread_suspend_word: false,
            ..DebuggerGates::new()
        };
        let state = Arc::new(crate::threading::jvm_thread::GcBlockState::new());
        let mut ds = DebugState::new();
        ds.suspend_thread(TID);

        let mut inactive = SuspendWordWait::enroll(&off, &mut ds, TID, &state);
        assert!(!inactive.active() && !ds.suspend_word_enrolled(TID));
        inactive.note_locked_round();
        assert!(
            !inactive.wait_unchanged(std::time::Duration::from_secs(30)),
            "inactive: the caller takes its locked round"
        );
        inactive.leave(&mut ds, TID);
        assert_eq!(state.debugger_suspend_word.load(Ordering::Acquire), 0);

        let mut outer = SuspendWordWait::enroll(&on, &mut ds, TID, &state);
        let mut inner = SuspendWordWait::enroll(&on, &mut ds, TID, &state);
        assert!(
            !inner.wait_unchanged(std::time::Duration::from_secs(30)),
            "before its first round"
        );
        inner.leave(&mut ds, TID);
        assert!(
            ds.suspend_word_enrolled(TID),
            "the outer level stays enrolled"
        );
        assert_ne!(state.debugger_suspend_word.load(Ordering::Acquire), 0);
        ds.resume_thread(TID);
        assert_eq!(
            state.debugger_suspend_word.load(Ordering::Acquire) & SW_SUSPENDED,
            0,
            "still republished for the outer level"
        );
        outer.leave(&mut ds, TID);
        assert!(!ds.suspend_word_enrolled(TID));
        assert_eq!(state.debugger_suspend_word.load(Ordering::Acquire), 0);
    }

    /// Wave 63, lane L6: a thread waiting on its word without the debug-state
    /// lock is woken by the resume (its own parker, not the 30 s bound), and
    /// meanwhile the lock stays free: the waiter takes it once per locked
    /// round, not once per poll.
    #[test]
    fn a_wait_on_the_suspend_word_ends_at_the_resume_without_the_lock() {
        use std::time::{Duration, Instant};
        const TID: u64 = 0x00F6_3003;
        let ds = Arc::new(parking_lot::Mutex::new(DebugState::new()));
        let gates = Arc::new(DebuggerGates {
            thread_suspend_word: true,
            ..DebuggerGates::new()
        });
        ds.lock().suspend_thread(TID);
        let enrolled = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let waiter = {
            let (ds, gates, enrolled) =
                (Arc::clone(&ds), Arc::clone(&gates), Arc::clone(&enrolled));
            std::thread::spawn(move || {
                let state = Arc::new(crate::threading::jvm_thread::GcBlockState::new());
                let start = Instant::now();
                let mut guard = ds.lock();
                let mut word = SuspendWordWait::enroll(&gates, &mut guard, TID, &state);
                loop {
                    word.note_locked_round();
                    if !guard.is_thread_suspended(TID) {
                        break;
                    }
                    enrolled.store(true, std::sync::atomic::Ordering::Release);
                    parking_lot::MutexGuard::unlocked(&mut guard, || {
                        while word.wait_unchanged(Duration::from_secs(30)) {}
                    });
                }
                word.leave(&mut guard, TID);
                (start.elapsed(), word.locked_rounds, word.word_wakes)
            })
        };
        while !enrolled.load(std::sync::atomic::Ordering::Acquire) {
            std::thread::yield_now();
        }
        // The waiter does not hold the lock while it waits.
        std::thread::sleep(Duration::from_millis(20));
        assert!(
            ds.try_lock_for(Duration::from_secs(5)).is_some(),
            "the lock is free"
        );
        ds.lock().resume_thread(TID);
        let (waited, locked, wakes) = waiter.join().expect("waiter");
        assert!(
            waited < Duration::from_secs(10),
            "woken by the resume: {waited:?}"
        );
        assert!(wakes >= 1, "the word ended the wait");
        // The first round, the backstop's (one per 50 ms) and the last.
        assert!(locked < 1_000, "locked rounds: {locked}");
        assert!(!ds.lock().suspend_word_enrolled(TID));
    }

    /// Interpreter round i2 wave 64, lane L6
    /// (`CRATONVM_DEBUG_STW_UNPARKS_PARKED`): the switch means nothing
    /// without the suspend word, and a park enrolled with the barrier waits
    /// on its word up to the word's backstop, not the 1 ms poll.
    #[test]
    fn w64_l6_the_stw_unpark_switch_needs_the_suspend_word() {
        let both = DebuggerGates {
            thread_suspend_word: true,
            stw_unparks_parked: true,
            ..DebuggerGates::new()
        };
        assert!(both.stw_unparks_parked());
        let alone = DebuggerGates {
            thread_suspend_word: false,
            stw_unparks_parked: true,
            ..DebuggerGates::new()
        };
        assert!(
            !alone.stw_unparks_parked(),
            "no word, no word-parked thread"
        );
        let off = DebuggerGates {
            thread_suspend_word: true,
            stw_unparks_parked: false,
            ..DebuggerGates::new()
        };
        assert!(!off.stw_unparks_parked());
        assert_eq!(
            DebuggerGates::parked_word_wait_bound(false),
            DEBUG_STATE_POLL
        );
        assert_eq!(
            DebuggerGates::parked_word_wait_bound(true),
            SUSPEND_WORD_BACKSTOP
        );
    }

    /// Wave 64, lane L6: the park loop of `interpreter::park_for_debugger_under`
    /// with both switches -- a thread waiting on its suspend word with the
    /// backstop bound, enrolled with a GC barrier -- answers that barrier's
    /// stop-the-world request at once, and takes far fewer rounds than one a
    /// millisecond while it waits.
    #[test]
    fn w64_l6_a_word_parked_thread_answers_a_request_without_polling() {
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::time::{Duration, Instant};
        const TID: u64 = 0x00F6_4001;
        let ds = Arc::new(parking_lot::Mutex::new(DebugState::new()));
        let gates = Arc::new(DebuggerGates {
            thread_suspend_word: true,
            stw_unparks_parked: true,
            ..DebuggerGates::new()
        });
        let barrier = Arc::new(crate::threading::gc_barrier::GcBarrier::new());
        ds.lock().suspend_thread(TID);
        let enrolled = Arc::new(AtomicBool::new(false));
        let parked = {
            let (ds, gates, barrier, enrolled) = (
                Arc::clone(&ds),
                Arc::clone(&gates),
                Arc::clone(&barrier),
                Arc::clone(&enrolled),
            );
            std::thread::spawn(move || {
                let state = Arc::new(crate::threading::jvm_thread::GcBlockState::new());
                let mut word = {
                    let mut guard = ds.lock();
                    SuspendWordWait::enroll(&gates, &mut guard, TID, &state)
                };
                let unparks = gates.stw_unparks_parked();
                let waker = unparks.then(|| barrier.enroll_stw_waker());
                let bound = DebuggerGates::parked_word_wait_bound(unparks);
                enrolled.store(true, Ordering::Release);
                let mut rounds = 0u64;
                let answered_at = loop {
                    if barrier.stw_requested.load(Ordering::Acquire) {
                        break Instant::now();
                    }
                    rounds += 1;
                    if word.wait_unchanged(bound) {
                        continue;
                    }
                    let guard = ds.lock();
                    word.note_locked_round();
                    assert!(guard.is_thread_suspended(TID));
                };
                drop(waker);
                word.leave(&mut ds.lock(), TID);
                (answered_at, rounds)
            })
        };
        while !enrolled.load(Ordering::Acquire) {
            std::thread::yield_now();
        }
        std::thread::sleep(Duration::from_millis(300));
        let requested_at = Instant::now();
        let census = crate::threading::gc_barrier::StwCensus {
            alive: 1,
            initiator_counted: Some(true),
            ..Default::default()
        };
        assert!(barrier.request_stw_opening_cycle(crate::threading::ThreadId(1), || census, || {}));
        let (answered_at, rounds) = parked.join().expect("parked thread");
        barrier.complete_gc(cratonvm_types::PointerMap::default());
        let latency = answered_at.saturating_duration_since(requested_at);
        assert!(
            latency < Duration::from_secs(5),
            "woken by the request: {latency:?}"
        );
        // About two rounds per 50 ms backstop; the 1 ms poll takes ~300.
        assert!(rounds < 100, "rounds while parked: {rounds}");
    }

    /// Interpreter round i2 wave 64, lane L6
    /// (`CRATONVM_DEBUG_SETTLE_ARRIVAL_COUNT`): every notification of the
    /// debug state's waiters moves the arrival count with the switch, none
    /// without; a settle walk is reused only while the count stands and the
    /// rewalk backstop has not passed.
    #[test]
    fn w64_l6_arrivals_move_with_every_notification_and_a_walk_goes_stale() {
        use std::time::{Duration, Instant};
        let counted = DebuggerGates {
            settle_arrival_count: true,
            ..DebuggerGates::new()
        };
        assert!(counted.settle_arrival_count());
        let before = counted.arrivals();
        counted.notify_state_changed();
        counted.notify_state_changed();
        assert_eq!(counted.arrivals(), before + 2);
        let uncounted = DebuggerGates {
            settle_arrival_count: false,
            ..DebuggerGates::new()
        };
        uncounted.notify_state_changed();
        assert_eq!(uncounted.arrivals(), 0, "the switch off counts nothing");

        let fresh = SettleWalk {
            pending: vec![1, 2],
            laggards: vec![],
            arrivals: 7,
            at: Instant::now(),
        };
        assert!(!fresh.due(7), "nothing arrived: reuse");
        assert!(fresh.due(8), "an arrival: walk again");
        let old = SettleWalk {
            at: Instant::now()
                .checked_sub(SETTLE_REWALK_BACKSTOP + Duration::from_millis(5))
                .unwrap_or_else(Instant::now),
            ..fresh
        };
        if old.at.elapsed() >= SETTLE_REWALK_BACKSTOP {
            assert!(old.due(7), "the backstop: walk again");
        }
    }

    /// Wave 64, lane L6: the park's arrival notification (as
    /// `interpreter::park_for_debugger_under` sends it) wakes a settle-style
    /// waiter on the condvar with the count already moved, so the waiter's
    /// next round walks.
    #[test]
    fn w64_l6_an_arrival_moves_the_count_before_it_wakes_the_waiter() {
        use std::time::Duration;
        let gates = Arc::new(DebuggerGates {
            settle_arrival_count: true,
            park_condvar: true,
            ..DebuggerGates::new()
        });
        let ds = Arc::new(parking_lot::Mutex::new(DebugState::new()));
        let seen = gates.arrivals();
        let waiter = {
            let (gates, ds) = (Arc::clone(&gates), Arc::clone(&ds));
            std::thread::spawn(move || {
                let mut guard = ds.lock();
                let mut rounds = 0u32;
                while gates.arrivals() == seen && rounds < 30_000 {
                    gates.await_state_change(&mut guard, Duration::from_millis(1));
                    rounds += 1;
                }
                gates.arrivals()
            })
        };
        std::thread::sleep(Duration::from_millis(20));
        {
            let _published = ds.lock();
        }
        gates.notify_state_changed();
        let after = waiter.join().expect("waiter");
        assert_ne!(after, seen, "the arrival moved the count");
    }

    /// Interpreter round i2 wave 65, lane L6 (`CRATONVM_DEBUG_PARK_GC_BLOCKED`):
    /// the switch means nothing without the suspend word, and a park that
    /// waits GC-blocked waits on its word up to the word's backstop (no
    /// pause needs to end its wait).
    #[test]
    fn w65_l6_the_gc_blocked_park_switch_needs_the_suspend_word() {
        let both = DebuggerGates {
            thread_suspend_word: true,
            park_gc_blocked: true,
            ..DebuggerGates::new()
        };
        assert!(both.park_gc_blocked());
        let alone = DebuggerGates {
            thread_suspend_word: false,
            park_gc_blocked: true,
            ..DebuggerGates::new()
        };
        assert!(!alone.park_gc_blocked(), "no word, no word-parked thread");
        let off = DebuggerGates {
            thread_suspend_word: true,
            park_gc_blocked: false,
            ..DebuggerGates::new()
        };
        assert!(!off.park_gc_blocked());
        // `park_for_debugger_under` bounds its word wait as an unparked park
        // does.
        assert_eq!(
            DebuggerGates::parked_word_wait_bound(true),
            SUSPEND_WORD_BACKSTOP
        );
    }

    /// A thread of `shared`, registered as the park's thread is, with one
    /// frame holding `obj` (wave 65).
    fn w65_parked_thread(
        shared: &crate::vm::SharedVm,
        tid: crate::threading::jvm_thread::ThreadId,
        obj: crate::types::ObjectRef,
    ) -> crate::threading::jvm_thread::JvmThread {
        let registry = &shared.threads.thread_registry;
        registry.register(tid, "w65-parked", None);
        let thread = crate::threading::jvm_thread::JvmThread::new(tid, "w65-parked");
        registry.set_root_snapshot(tid, thread.root_snapshot.clone());
        registry.set_gc_block_state(tid, thread.gc_block_state.clone());
        let mut thread = thread;
        thread.frames.push(crate::runtime::frame::Frame::new(
            crate::classloading::ClassId::new(0),
            "W65Parked".to_string(),
            "spin".to_string(),
            "()V".to_string(),
            None,
            vec![],
            vec![],
            4,
            2,
            &[
                crate::types::Value::Object(Some(obj)),
                crate::types::Value::Int(7),
            ],
        ));
        thread
    }

    /// Wave 65, lane L6: a park's region deposits the thread's roots, makes
    /// it GC-blocked (the census excludes it) while it reads RUNNABLE --
    /// whatever kind a blocking region left in `java_state` -- and its leave
    /// puts that kind back with the flag down.
    #[test]
    fn w65_l6_a_park_region_is_gc_blocked_reads_runnable_and_restores() {
        use std::sync::atomic::Ordering;
        let shared = Arc::new(crate::vm::SharedVm::new(crate::config::VmConfig::default()));
        *shared.self_arc.write() = Some(Arc::downgrade(&shared));
        let tid = crate::threading::jvm_thread::ThreadId(0x00F6_5001);
        let obj = shared
            .mem
            .heap
            .alloc_object(crate::classloading::ClassId::new(0), 0);
        let mut thread = w65_parked_thread(&shared, tid, obj);
        let registry = &shared.threads.thread_registry;
        // A TIMED_WAITING kind a sleep left behind (read only with the flag up).
        thread.gc_block_state.java_state.store(3, Ordering::Release);
        assert!(!registry.is_blocked(tid));

        let mut region = ParkGcRegion::closed();
        assert!(!region.is_open());
        assert!(region.enter(&shared, &mut thread));
        assert!(region.is_open());
        assert!(!region.enter(&shared, &mut thread), "already open");
        assert!(registry.is_blocked(tid), "GC-blocked inside");
        assert_eq!(registry.java_block_state(tid), 0, "RUNNABLE inside");
        assert_eq!(
            jdwp_thread_status(registry, tid),
            commands::THREAD_STATUS_RUNNING,
            "as a thread suspended while running"
        );
        assert!(
            thread.root_snapshot.lock().contains(&obj),
            "the frame's reference is deposited"
        );
        assert_eq!(region.entries, 1);

        region.leave(&shared, &mut thread);
        assert!(!region.is_open());
        assert!(!registry.is_blocked(tid));
        assert_eq!(
            thread.gc_block_state.java_state.load(Ordering::Acquire),
            3,
            "the kind it had is back"
        );
        // A second leave is nothing.
        region.leave(&shared, &mut thread);
        assert!(!registry.is_blocked(tid));

        // A thread already GC-blocked is left in its own region.
        thread
            .gc_block_state
            .in_blocked_region
            .store(true, Ordering::Release);
        assert!(!region.enter(&shared, &mut thread));
        assert!(!region.is_open());
        thread
            .gc_block_state
            .in_blocked_region
            .store(false, Ordering::Release);
    }

    /// Wave 65, lane L6: a pause requested while the region is open excludes
    /// the parked thread (it completes without its arrival), and closing the
    /// region waits for that pause's end, as every blocked region's leave
    /// does, so the frames are never used while a collection may move what
    /// they name.
    #[test]
    fn w65_l6_a_park_region_sleeps_through_a_pause_and_its_leave_waits_it_out() {
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::time::Duration;
        let shared = Arc::new(crate::vm::SharedVm::new(crate::config::VmConfig::default()));
        *shared.self_arc.write() = Some(Arc::downgrade(&shared));
        let tid = crate::threading::jvm_thread::ThreadId(0x00F6_5002);
        let obj = shared
            .mem
            .heap
            .alloc_object(crate::classloading::ClassId::new(0), 0);
        let mut thread = w65_parked_thread(&shared, tid, obj);
        let mut region = ParkGcRegion::closed();
        assert!(region.enter(&shared, &mut thread));
        let barrier = &shared.mem.gc_barrier;
        let requested = AtomicBool::new(false);
        let completed = AtomicBool::new(false);
        std::thread::scope(|s| {
            // The pause, requested and completed on a thread of its own (the
            // pause ledger binds to the requesting thread).
            let pauser = s.spawn(|| {
                let census = crate::threading::gc_barrier::StwCensus {
                    alive: 2,
                    blocked: 1,
                    blocked_tids: vec![tid.0],
                    initiator_counted: Some(true),
                    ..Default::default()
                };
                let won = barrier.request_stw_opening_cycle(
                    crate::threading::ThreadId(1),
                    || census,
                    || {},
                );
                let all_arrived = barrier.wait_for_all_timeout(Duration::from_secs(5));
                requested.store(true, Ordering::Release);
                std::thread::sleep(Duration::from_millis(50));
                completed.store(true, Ordering::Release);
                barrier.complete_gc(cratonvm_types::PointerMap::default());
                (won, all_arrived)
            });
            while !requested.load(Ordering::Acquire) {
                std::thread::yield_now();
            }
            region.leave(&shared, &mut thread);
            assert!(
                completed.load(Ordering::Acquire),
                "the leave waited for the pause's end"
            );
            let (won, all_arrived) = pauser.join().expect("pauser");
            assert!(won, "the pause was requested");
            assert!(all_arrived, "the parked thread is not waited for");
        });
        assert!(!shared.threads.thread_registry.is_blocked(tid));
    }

    /// Interpreter round i2 wave 66, lane L6: both wave-66 switches mean
    /// nothing without the GC-blocked park (and so without the suspend
    /// word), and a word wait tells, without waiting, whether its word moved
    /// since its last locked round.
    #[test]
    fn w66_l6_the_region_switches_need_the_gc_blocked_park_and_the_word_tells_news() {
        let all = DebuggerGates {
            thread_suspend_word: true,
            park_gc_blocked: true,
            heap_work_outside_region: true,
            park_region_reuses_deposit: true,
            ..DebuggerGates::new()
        };
        assert!(all.heap_work_outside_region());
        assert!(all.park_region_reuses_deposit());
        let no_region = DebuggerGates {
            park_gc_blocked: false,
            ..all
        };
        assert!(!no_region.heap_work_outside_region());
        assert!(!no_region.park_region_reuses_deposit());
        let no_word = DebuggerGates {
            thread_suspend_word: false,
            park_gc_blocked: true,
            heap_work_outside_region: true,
            park_region_reuses_deposit: true,
            ..DebuggerGates::new()
        };
        assert!(!no_word.heap_work_outside_region());
        assert!(!no_word.park_region_reuses_deposit());
        let off = DebuggerGates {
            thread_suspend_word: true,
            park_gc_blocked: true,
            heap_work_outside_region: false,
            park_region_reuses_deposit: false,
            ..DebuggerGates::new()
        };
        assert!(!off.heap_work_outside_region());
        assert!(!off.park_region_reuses_deposit());

        use std::sync::atomic::Ordering;
        const TID: u64 = 0x00F6_6000;
        let on = DebuggerGates {
            thread_suspend_word: true,
            ..DebuggerGates::new()
        };
        let state = Arc::new(crate::threading::jvm_thread::GcBlockState::new());
        let mut ds = DebugState::new();
        ds.suspend_thread(TID);
        let mut wait = SuspendWordWait::enroll(&on, &mut ds, TID, &state);
        assert!(!wait.moved(), "no locked round yet");
        wait.note_locked_round();
        assert!(!wait.moved(), "as the round saw it");
        state
            .debugger_suspend_word
            .fetch_add(1u32 << SW_SEQ_SHIFT, Ordering::AcqRel);
        assert!(wait.moved(), "a writer moved it");
        wait.note_locked_round();
        assert!(!wait.moved());
        wait.leave(&mut ds, TID);
    }

    /// Wave 66, lane L6 (`CRATONVM_DEBUG_HEAP_WORK_OUTSIDE_REGION`): heap
    /// work goes to the lowest idle park outside its GC-blocked region, then
    /// to the heap service, then to a park inside its region; without the
    /// switch, to the lowest idle park as in wave 65. An invocation never
    /// goes to the service, and a busy park takes nothing.
    #[test]
    fn w66_l6_heap_work_prefers_a_park_outside_its_region() {
        use std::sync::atomic::Ordering;
        const LOW: u64 = 0x00F6_6001;
        const HIGH: u64 = 0x00F6_6002;
        let low = Arc::new(crate::threading::jvm_thread::GcBlockState::new());
        let high = Arc::new(crate::threading::jvm_thread::GcBlockState::new());
        let mut ds = DebugState::new();
        assert_eq!(ds.heap_work_target(true, true), HeapWorkTarget::Nowhere);
        for (t, state) in [(LOW, &low), (HIGH, &high)] {
            ds.suspend_thread(t);
            ds.note_parked(t, true);
            ds.enroll_suspend_word(t, state);
        }
        assert_eq!(
            ds.heap_work_target(false, true),
            HeapWorkTarget::Parked(LOW)
        );
        assert_eq!(ds.heap_work_target(true, true), HeapWorkTarget::Parked(LOW));

        low.in_blocked_region.store(true, Ordering::Release);
        assert_eq!(
            ds.heap_work_target(false, true),
            HeapWorkTarget::Parked(LOW),
            "switch off: the lowest, region or not"
        );
        assert_eq!(
            ds.heap_work_target(true, true),
            HeapWorkTarget::Parked(HIGH),
            "the park outside its region"
        );

        high.in_blocked_region.store(true, Ordering::Release);
        assert_eq!(
            ds.heap_work_target(true, true),
            HeapWorkTarget::Parked(LOW),
            "no service: a park inside its region"
        );
        let (service, _jobs) = std::sync::mpsc::channel::<crate::native::jni::AttachedServiceJob>();
        ds.heap_service = Some((service, 0x00F6_6003));
        assert_eq!(
            ds.heap_work_target(true, true),
            HeapWorkTarget::Service,
            "every park in its region: the service"
        );
        assert_eq!(
            ds.heap_work_target(true, false),
            HeapWorkTarget::Parked(LOW),
            "never the service when it may not take the work"
        );
        assert_eq!(
            ds.heap_work_target(false, true),
            HeapWorkTarget::Parked(LOW)
        );

        // A busy park takes nothing, inside its region or not.
        high.in_blocked_region.store(false, Ordering::Release);
        if let Some(busy) = ds.parked_threads.get_mut(&HIGH).and_then(|l| l.last_mut()) {
            *busy = true;
        }
        assert_eq!(ds.heap_work_target(true, true), HeapWorkTarget::Service);
        low.in_blocked_region.store(false, Ordering::Release);
        assert_eq!(ds.heap_work_target(true, true), HeapWorkTarget::Parked(LOW));
    }

    /// Wave 66, lane L6 (`CRATONVM_DEBUG_PARK_REGION_REUSES_DEPOSIT`): a
    /// park region re-entered with nothing changed raises the flag over the
    /// deposit its leave refreshed -- the frame's reference still in the
    /// snapshot, its slot origin recorded again -- and deposits in full
    /// after a task that may have touched the frames, after another root
    /// publish, after a frame push, and always with the switch off.
    #[test]
    fn w66_l6_a_park_region_reuses_its_standing_deposit_only_while_its_key_holds() {
        let shared = Arc::new(crate::vm::SharedVm::new(crate::config::VmConfig::default()));
        *shared.self_arc.write() = Some(Arc::downgrade(&shared));
        let tid = crate::threading::jvm_thread::ThreadId(0x00F6_6011);
        let obj = shared
            .mem
            .heap
            .alloc_object(crate::classloading::ClassId::new(0), 0);
        let mut thread = w65_parked_thread(&shared, tid, obj);
        let registry = &shared.threads.thread_registry;
        let addr = obj.as_ptr() as usize;
        let origin_of_local0 = |thread: &crate::threading::jvm_thread::JvmThread| {
            thread
                .gc_block_state
                .slot_origins
                .lock()
                .iter()
                .filter(|o| !o.is_stack && o.frame == 0 && o.idx == 0 && o.orig == addr)
                .count()
        };

        let mut region = ParkGcRegion::closed().with_deposit_reuse(true, true);
        assert!(region.enter(&shared, &mut thread));
        assert_eq!(region.reuses, 0, "the first entry deposits");
        region.leave(&shared, &mut thread);
        assert!(!registry.is_blocked(tid));

        // Heap work ran elsewhere (or a relisting): nothing moved.
        assert!(region.enter(&shared, &mut thread));
        assert_eq!(region.reuses, 1, "the standing deposit is reused");
        assert!(
            registry.is_blocked(tid),
            "GC-blocked as after a full deposit"
        );
        assert!(
            thread.root_snapshot.lock().contains(&obj),
            "the frame's reference is still deposited"
        );
        assert_eq!(
            origin_of_local0(&thread),
            1,
            "its slot origin recorded again"
        );
        region.leave(&shared, &mut thread);
        assert!(!registry.is_blocked(tid));

        // A task named this thread (a frame write, an invocation).
        region.forget_standing();
        assert!(region.enter(&shared, &mut thread));
        assert_eq!(region.reuses, 1, "after a task on this thread: in full");
        region.leave(&shared, &mut thread);

        // Another root publish replaced the snapshot.
        thread
            .root_publish_seq
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        assert!(region.enter(&shared, &mut thread));
        assert_eq!(region.reuses, 1, "after another publish: in full");
        region.leave(&shared, &mut thread);

        // Reused again, then a frame pushed.
        assert!(region.enter(&shared, &mut thread));
        assert_eq!(region.reuses, 2);
        region.leave(&shared, &mut thread);
        thread.frames.push(crate::runtime::frame::Frame::new(
            crate::classloading::ClassId::new(0),
            "W66Parked".to_string(),
            "deeper".to_string(),
            "()V".to_string(),
            None,
            vec![],
            vec![],
            4,
            2,
            &[
                crate::types::Value::Object(Some(obj)),
                crate::types::Value::Int(7),
            ],
        ));
        assert!(region.enter(&shared, &mut thread));
        assert_eq!(region.reuses, 2, "after a frame push: in full");
        region.leave(&shared, &mut thread);
        assert!(!registry.is_blocked(tid));

        // Switch off: every entry in full.
        let mut off = ParkGcRegion::closed();
        assert!(off.enter(&shared, &mut thread));
        off.leave(&shared, &mut thread);
        assert!(off.enter(&shared, &mut thread));
        assert_eq!(off.reuses, 0);
        off.leave(&shared, &mut thread);
        assert!(!registry.is_blocked(tid));
    }

    /// Interpreter round i2 wave 68, lane L6: the region-at-first-pause
    /// switch means nothing without the GC-blocked park, the kept-snapshot
    /// switch nothing without deposit reuse; a lazy opening waits for a pause
    /// (and a seen word with no news) before it opens, then turns eager.
    #[test]
    fn w68_l6_the_wave_68_switches_need_their_parents_and_a_lazy_park_waits_for_a_pause() {
        let all = DebuggerGates {
            thread_suspend_word: true,
            park_gc_blocked: true,
            park_region_reuses_deposit: true,
            park_region_at_first_pause: true,
            park_leave_keeps_unfolded_snapshot: true,
            ..DebuggerGates::new()
        };
        assert!(all.park_region_at_first_pause());
        assert!(all.park_leave_keeps_unfolded_snapshot());
        let no_region = DebuggerGates {
            thread_suspend_word: true,
            park_gc_blocked: false,
            park_region_reuses_deposit: true,
            park_region_at_first_pause: true,
            park_leave_keeps_unfolded_snapshot: true,
            ..DebuggerGates::new()
        };
        assert!(!no_region.park_region_at_first_pause());
        assert!(!no_region.park_leave_keeps_unfolded_snapshot());
        let no_reuse = DebuggerGates {
            thread_suspend_word: true,
            park_gc_blocked: true,
            park_region_reuses_deposit: false,
            park_region_at_first_pause: true,
            park_leave_keeps_unfolded_snapshot: true,
            ..DebuggerGates::new()
        };
        assert!(no_reuse.park_region_at_first_pause());
        assert!(
            !no_reuse.park_leave_keeps_unfolded_snapshot(),
            "nothing to reuse"
        );
        let off = DebuggerGates {
            thread_suspend_word: true,
            park_gc_blocked: true,
            park_region_reuses_deposit: true,
            park_region_at_first_pause: false,
            park_leave_keeps_unfolded_snapshot: false,
            ..DebuggerGates::new()
        };
        assert!(!off.park_region_at_first_pause());
        assert!(!off.park_leave_keeps_unfolded_snapshot());

        let eager = ParkRegionOpening::new(false);
        assert!(!eager.lazy());
        assert!(
            eager.enters_eagerly(),
            "wave 65: after the first locked round"
        );
        assert!(
            !eager.opens_at_pause(true, false),
            "an eager park arrives at the pause"
        );

        let mut lazy = ParkRegionOpening::new(true);
        assert!(lazy.lazy());
        assert!(!lazy.enters_eagerly(), "no region before a pause");
        assert!(
            !lazy.opens_at_pause(false, false),
            "no locked round yet: arrive"
        );
        assert!(
            !lazy.opens_at_pause(true, true),
            "news for the locked round: arrive"
        );
        assert!(lazy.opens_at_pause(true, false));
        lazy.note_opened_at_pause();
        assert!(lazy.enters_eagerly(), "after the first pause: as wave 65");
        assert!(lazy.lazy());
    }

    /// Wave 68, lane L6 (`CRATONVM_DEBUG_PARK_REGION_AT_FIRST_PAUSE`): a park
    /// counted by a pause already requested opens its region in place of its
    /// safepoint arrival -- the pause gets the one arrival it waits for, the
    /// entry returns once the pause is over, and the thread is GC-blocked
    /// for the next one.
    #[test]
    fn w68_l6_a_lazy_park_opens_its_region_inside_the_pause_that_woke_it() {
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::time::Duration;
        let shared = Arc::new(crate::vm::SharedVm::new(crate::config::VmConfig::default()));
        *shared.self_arc.write() = Some(Arc::downgrade(&shared));
        let tid = crate::threading::jvm_thread::ThreadId(0x00F6_8001);
        let obj = shared
            .mem
            .heap
            .alloc_object(crate::classloading::ClassId::new(0), 0);
        let mut thread = w65_parked_thread(&shared, tid, obj);
        let registry = &shared.threads.thread_registry;
        let barrier = &shared.mem.gc_barrier;
        let requested = AtomicBool::new(false);
        let completed = AtomicBool::new(false);
        let mut region = ParkGcRegion::closed();
        std::thread::scope(|s| {
            let pauser = s.spawn(|| {
                // The parked thread is a counted mutator: the pause expects it.
                let census = crate::threading::gc_barrier::StwCensus {
                    alive: 2,
                    initiator_counted: Some(true),
                    ..Default::default()
                };
                let won = barrier.request_stw_opening_cycle(
                    crate::threading::ThreadId(1),
                    || census,
                    || {},
                );
                requested.store(true, Ordering::Release);
                let all_arrived = barrier.wait_for_all_timeout(Duration::from_secs(5));
                std::thread::sleep(Duration::from_millis(20));
                completed.store(true, Ordering::Release);
                barrier.complete_gc(cratonvm_types::PointerMap::default());
                (won, all_arrived)
            });
            while !requested.load(Ordering::Acquire) {
                std::thread::yield_now();
            }
            assert!(region.enter_at_pause(&shared, &mut thread));
            assert!(
                completed.load(Ordering::Acquire),
                "the entry arrived for the pause and waited for its end"
            );
            let (won, all_arrived) = pauser.join().expect("pauser");
            assert!(won, "the pause was requested");
            assert!(
                all_arrived,
                "the entry was the arrival the pause waited for"
            );
        });
        assert!(region.is_open());
        assert_eq!(region.pause_entries, 1);
        assert_eq!(region.entries, 1);
        assert!(
            registry.is_blocked(tid),
            "GC-blocked for the rest of the hold"
        );
        assert!(thread.root_snapshot.lock().contains(&obj));
        region.leave(&shared, &mut thread);
        assert!(!registry.is_blocked(tid));
    }

    /// Wave 68, lane L6 (`CRATONVM_DEBUG_PARK_LEAVE_KEEPS_UNFOLDED_SNAPSHOT`):
    /// a region left for heap work with nothing folded into it keeps its
    /// entry's snapshot, which the next entry reuses -- and a run of such
    /// cycles does not grow the snapshot; a pause completed inside the
    /// region refreshes instead, as does every plain leave and every leave
    /// with the switch off.
    #[test]
    fn w68_l6_a_leave_for_heap_work_keeps_an_unfolded_snapshot() {
        use std::sync::atomic::Ordering;
        let shared = Arc::new(crate::vm::SharedVm::new(crate::config::VmConfig::default()));
        *shared.self_arc.write() = Some(Arc::downgrade(&shared));
        let tid = crate::threading::jvm_thread::ThreadId(0x00F6_8011);
        let obj = shared
            .mem
            .heap
            .alloc_object(crate::classloading::ClassId::new(0), 0);
        let mut thread = w65_parked_thread(&shared, tid, obj);
        let registry = &shared.threads.thread_registry;
        let seq = |thread: &crate::threading::jvm_thread::JvmThread| {
            thread.root_publish_seq.load(Ordering::Acquire)
        };

        let mut region = ParkGcRegion::closed()
            .with_deposit_reuse(true, true)
            .with_unfolded_keep(true);
        assert!(region.enter(&shared, &mut thread));
        let after_entry = seq(&thread);
        region.leave_for_heap_work(&shared, &mut thread);
        assert_eq!(region.keeps, 1, "nothing folded: kept");
        assert_eq!(seq(&thread), after_entry, "no refresh published");
        assert!(!registry.is_blocked(tid), "a running mutator again");
        assert!(
            thread.root_snapshot.lock().contains(&obj),
            "the entry's roots stand"
        );

        // The next entry reuses the kept snapshot.
        assert!(region.enter(&shared, &mut thread));
        assert_eq!(region.reuses, 1);
        let len = thread.root_snapshot.lock().len();
        for cycle in 2..6u64 {
            region.leave_for_heap_work(&shared, &mut thread);
            assert_eq!(region.keeps, cycle);
            assert!(region.enter(&shared, &mut thread));
            assert_eq!(region.reuses, cycle);
            assert_eq!(
                thread.root_snapshot.lock().len(),
                len,
                "no growth per cycle"
            );
        }
        assert!(thread.root_snapshot.lock().contains(&obj));

        // A pause completed while the region stood: refresh, no reuse after.
        shared
            .mem
            .gc_barrier
            .gc_generation
            .fetch_add(1, Ordering::Release);
        let before = seq(&thread);
        region.leave_for_heap_work(&shared, &mut thread);
        assert_eq!(region.keeps, 5, "a moved generation: refreshed");
        assert_ne!(seq(&thread), before, "the refresh published");
        assert!(region.enter(&shared, &mut thread));
        assert_eq!(region.reuses, 5, "the key moved: in full");

        // A plain leave (an invocation, a frame write, the resume) refreshes.
        region.leave(&shared, &mut thread);
        assert_eq!(region.keeps, 5);
        assert!(!registry.is_blocked(tid));

        // Switch off: the heap-work leave is the plain one.
        let mut off = ParkGcRegion::closed()
            .with_deposit_reuse(true, false)
            .with_unfolded_keep(false);
        assert!(off.enter(&shared, &mut thread));
        off.leave_for_heap_work(&shared, &mut thread);
        assert_eq!(off.keeps, 0);
        // And never without deposit reuse.
        let mut no_reuse = ParkGcRegion::closed().with_unfolded_keep(true);
        assert!(no_reuse.enter(&shared, &mut thread));
        no_reuse.leave_for_heap_work(&shared, &mut thread);
        assert_eq!(no_reuse.keeps, 0);
        assert!(!registry.is_blocked(tid));
    }

    /// Interpreter round i2 wave 54, lane L6: a thread that won a contended
    /// monitor while suspended gives it back -- another thread takes it --
    /// and owns it again only after its resume; one not suspended, or held
    /// only by a stack read's handshake, keeps it.
    #[test]
    fn a_suspended_entrant_gives_the_monitor_back_until_its_resume() {
        use crate::threading::ThreadId;
        use std::time::{Duration, Instant};
        const TID: u64 = 0x00F5_4001;
        const OTHER: u64 = 0x00F5_4002;
        let _lock = crate::runtime::jvmti::jvmti_registry_test_lock();
        let shared = Arc::new(crate::vm::SharedVm::new(crate::config::VmConfig::default()));
        let monitor = Arc::new(crate::threading::monitor::Monitor::new());
        let set = |f: &dyn Fn(&mut DebugState)| {
            let mut ds = shared.debug.debug_state.lock();
            f(&mut ds);
            publish_debugger_gates(&shared, &ds);
        };

        // Not suspended: the monitor stays with its winner.
        monitor.block_enter(ThreadId(TID));
        exit_monitor_while_suspended(&shared, ThreadId(TID), &monitor);
        assert_eq!(monitor.current_owner(), Some(ThreadId(TID)));
        // A stack read's handshake is no suspension for this.
        set(&|ds| {
            ds.jvmti_handshakes.insert(TID, 1);
        });
        assert!(!monitor_entry_held_off(&shared, ThreadId(TID)));
        set(&|ds| {
            ds.jvmti_handshakes.remove(&TID);
        });
        monitor.exit(ThreadId(TID)).expect("owned");

        // Suspended: the winner -- blocked behind OTHER, then handed the
        // monitor by OTHER's release -- gives it back, OTHER takes it again,
        // and the winner owns it once resumed.
        set(&|ds| ds.suspend_thread(TID));
        assert!(monitor_entry_held_off(&shared, ThreadId(TID)));
        monitor.block_enter(ThreadId(OTHER));
        let worker = {
            let shared = Arc::clone(&shared);
            let monitor = Arc::clone(&monitor);
            std::thread::spawn(move || {
                monitor.block_enter(ThreadId(TID));
                exit_monitor_while_suspended(&shared, ThreadId(TID), &monitor);
                let owner = monitor.current_owner();
                monitor.exit(ThreadId(TID)).expect("owned after the resume");
                owner
            })
        };
        // Let the worker park behind OTHER, then hand it the monitor.
        std::thread::sleep(Duration::from_millis(30));
        monitor.exit(ThreadId(OTHER)).expect("OTHER owns it");
        // The worker woke, won and gave it back: free, and the worker waits.
        let deadline = Instant::now() + Duration::from_secs(10);
        while monitor.current_owner().is_some() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(1));
        }
        std::thread::sleep(Duration::from_millis(50));
        assert_eq!(
            monitor.current_owner(),
            None,
            "the suspended winner gave it back"
        );
        assert!(!worker.is_finished(), "still waiting for its resume");
        // Free, so this returns at once.
        monitor.block_enter(ThreadId(OTHER));
        monitor.exit(ThreadId(OTHER)).expect("OTHER owns it");
        set(&|ds| ds.resume_thread(TID));
        assert_eq!(worker.join().expect("worker"), Some(ThreadId(TID)));
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
        // The gate without `CRATONVM_JVMTI_EXCEPTION_FROM_COMPILED` (default on
        // since the round i4 wave-94 triage), where every exception request
        // keeps the interpreter; with it a `SUSPEND_NONE` one is served from
        // compiled code.
        let shared = Arc::new(cratonvm_types::flags::with_thread_overrides(
            &[("CRATONVM_JVMTI_EXCEPTION_FROM_COMPILED", Some("0"))],
            || crate::vm::SharedVm::new(crate::config::VmConfig::default()),
        ));
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

        // The frame-reading path; `CRATONVM_PUBLISHED_WAIT_KIND` is on by
        // default since wave 64, and its path has its own test below.
        let mut registry = ThreadRegistry::new();
        registry.set_published_wait_kind_for_test(false);
        // `CRATONVM_PUBLISHED_ALL_WAIT_KINDS` implies the word too, and is
        // default on since the wave-67 triage.
        registry.set_published_all_wait_kinds_for_test(false);
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

    /// Interpreter round i2 wave 63 (lane L1, `CRATONVM_PUBLISHED_WAIT_KIND`):
    /// with the switch on, a TIMED_WAITING thread is `SLEEPING` exactly when
    /// its region published the sleep kind, whatever its deposited top frame
    /// says; off, the frames decide as before.
    #[test]
    fn a_thread_status_reads_the_published_sleep_kind() {
        use crate::threading::jvm_thread::{
            GcBlockState, JAVA_WAIT_KIND_NONE, JAVA_WAIT_KIND_SLEEPING,
        };
        use crate::threading::thread_registry::ThreadRegistry;
        use commands::{THREAD_STATUS_SLEEPING, THREAD_STATUS_WAIT};
        use std::sync::atomic::Ordering;

        let mut registry = ThreadRegistry::new();
        registry.set_published_wait_kind_for_test(false);
        // `CRATONVM_PUBLISHED_ALL_WAIT_KINDS` implies the word too, and is
        // default on since the wave-67 triage.
        registry.set_published_all_wait_kinds_for_test(false);
        let tid = crate::threading::ThreadId(1);
        registry.register(tid, "sleeper", None);
        let block = Arc::new(GcBlockState::new());
        registry.set_gc_block_state(tid, block.clone());
        block
            .java_wait_kind
            .store(JAVA_WAIT_KIND_SLEEPING, Ordering::Release);
        block.java_state.store(3, Ordering::Release);
        block.in_blocked_region.store(true, Ordering::Release);
        let calls_sleep = |_: &cratonvm_native_api::StackTraceEntry| true;
        // Off: no frame, so no sleep; the frame test still answers.
        assert_eq!(jdwp_thread_status(&registry, tid), THREAD_STATUS_WAIT);
        assert_eq!(
            jdwp_thread_status_with(&registry, tid, &calls_sleep),
            THREAD_STATUS_WAIT,
            "no deposited frame at all"
        );

        registry.set_published_wait_kind_for_test(true);
        assert_eq!(
            jdwp_thread_status(&registry, tid),
            THREAD_STATUS_SLEEPING,
            "the published kind, with no frame to read"
        );
        block
            .java_wait_kind
            .store(JAVA_WAIT_KIND_NONE, Ordering::Release);
        let frame = cratonvm_native_api::StackTraceEntry {
            class_name: Arc::from("java/lang/Thread"),
            method_name: Arc::from("sleepNanos"),
            method_descriptor: None,
            source_file: None,
            line_number: -1,
            byte_code_index: 0,
            class_id: None,
            method_index: None,
        };
        registry.set_frame_trace(tid, Arc::new(parking_lot::Mutex::new(vec![frame])));
        assert_eq!(
            jdwp_thread_status_with(&registry, tid, &calls_sleep),
            THREAD_STATUS_WAIT,
            "on: the frames no longer decide"
        );
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
            // Wave 103 (interpreter round i5, lane L1): a class being linked
            // is left out of the listings, a linked one is listed (a `Loaded`
            // one depends on whether its link is trivial).
            let hidden = shared
                .debug
                .debug_state
                .lock()
                .unprepared_classes
                .contains(&wire);
            if state != ClassState::Loaded {
                assert_eq!(hidden, state == ClassState::Verifying, "{state}");
            }
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

    /// Interpreter round i5 wave 106 (lane L1, the i102 residue): the
    /// used-unlinked set records a never-linked class, never a linked one, and
    /// reads a slot past its capacity as used (listed).
    #[test]
    fn w106_l1_used_unlinked_records_never_linked_classes_only() {
        use crate::classloading::ClassState;
        let shared = Arc::new(crate::vm::SharedVm::new(crate::config::VmConfig::default()));
        let class_id = shared
            .classes
            .class_manager
            .write()
            .try_ensure_synthetic_class("cratonvm/test/W106Unlinked", 0)
            .expect("Compatible mode fabricates");
        if shared.debug.used_unlinked_classes.words.is_none() {
            // `CRATONVM_JDWP_HIDE_UNUSED_UNLINKED_CLASSES=0` in this process.
            return;
        }
        let set_state = |state: ClassState| {
            shared
                .classes
                .class_manager_write()
                .get_class_mut(class_id)
                .expect("registered")
                .state = state;
        };
        let note = || {
            let cm = shared.classes.class_manager.read();
            note_class_used_unlinked(&shared, cm.get_class(class_id).expect("registered"));
        };
        set_state(ClassState::Verified);
        note();
        assert!(!shared.debug.used_unlinked_classes.contains(class_id));
        set_state(ClassState::Loaded);
        note();
        assert!(shared.debug.used_unlinked_classes.contains(class_id));
        let past = crate::classloading::ClassId::new(
            u32::try_from(USED_UNLINKED_CAPACITY).expect("fits"),
        );
        assert!(shared.debug.used_unlinked_classes.contains(past));
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
        assert_ne!(
            dispatch(CS_THREAD_REF, CMD_TR_NAME, &data, &mut ds).error_code,
            ERR_NONE
        );

        // `ClassType.Superclass` of a class whose superclass is class 0.
        ds.class_signatures.insert(5, "LFive;".to_string());
        ds.class_signatures
            .insert(0, "Ljava/lang/Object;".to_string());
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
        for id in [
            MAIN_THREAD_WIRE_ID,
            MAIN_THREAD_WIRE_ID,
            CLASS_ZERO_WIRE_ID,
            9,
        ] {
            data.extend_from_slice(&id.to_be_bytes());
        }
        ids_from_wire(CS_OBJECT_REF, CMD_OR_INVOKE_METHOD, &mut data);
        let mut want = Vec::new();
        for id in [MAIN_THREAD_WIRE_ID, 0, 0, 9] {
            want.extend_from_slice(&id.to_be_bytes());
        }
        assert_eq!(
            data, want,
            "the receiver is an object id, decoded where it is read"
        );

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
        shared
            .threads
            .thread_registry
            .register(crate::threading::ThreadId(0), "main", Some(obj));
        let ds = shared.debug.debug_state.lock();
        assert_eq!(
            object_for_id(&shared, &ds, ids::MAIN_THREAD_WIRE_ID),
            Some(obj)
        );
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
        shared
            .threads
            .thread_registry
            .register(crate::threading::ThreadId(5), "worker", Some(obj));
        let mut ds = shared.debug.debug_state.lock();
        // Wave 24: a registered thread resolves before the server's thread
        // table lists it (it was `INVALID_OBJECT` until the next refresh).
        assert_eq!(
            object_for_id(&shared, &ds, 5),
            Some(obj),
            "registered, not listed yet"
        );
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
        assert_eq!(
            object_for_id(&shared, &ds, 5),
            None,
            "the heap service is hidden"
        );
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
            dispatch, CMD_M_LINE_TABLE, CMD_M_VARIABLE_TABLE_WITH_GENERIC,
            CMD_RT_CLASS_FILE_VERSION, CMD_RT_FIELDS_WITH_GENERIC, CMD_RT_INTERFACES,
            CMD_RT_METHODS_WITH_GENERIC, CMD_RT_MODIFIERS, CMD_RT_SIGNATURE_WITH_GENERIC,
            CMD_RT_SOURCE_FILE, CS_METHOD, CS_REF_TYPE, ERR_ABSENT_INFORMATION, ERR_INVALID_CLASS,
            ERR_INVALID_METHODID, ERR_NATIVE_METHOD, ERR_NONE,
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
            LazyAttribute::Decoded(Attribute::Code(Box::new(CodeAttribute {
                max_stack: 1,
                max_locals: 2,
                code: cratonvm_reader::ByteView::from_vec(vec![0; len]),
                exception_table: vec![],
                attributes,
            })))
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
                ConstantPoolEntry::Utf8("n".into()),
                ConstantPoolEntry::Utf8("I".into()),
                ConstantPoolEntry::Utf8("tags".into()),
                ConstantPoolEntry::Utf8("Ljava/util/List;".into()),
                ConstantPoolEntry::Utf8(list_generic.into()),
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
        let named = |n: &str| methods.iter().find(|m| m.1 == n).cloned().expect("listed");
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
        assert_eq!(
            r.read_u64_be().expect("end"),
            5,
            "not the last line's start, 3"
        );
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
                shared
                    .debug
                    .debugger_gates
                    .requires_interpreter_everywhere(),
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
        assert!(!shared
            .debug
            .debugger_gates
            .requires_interpreter_everywhere());
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
        assert!(!shared
            .debug
            .debug_state
            .lock()
            .class_signatures
            .contains_key(&gone));
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
                    attributes: vec![LazyAttribute::Decoded(Attribute::Code(Box::new(
                        CodeAttribute {
                            max_stack: 1,
                            max_locals: 1,
                            code: cratonvm_reader::ByteView::from_vec(vec![0x03, 0x3b, 0xb1]),
                            exception_table: vec![],
                            attributes: vec![],
                        },
                    )))],
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
        assert_eq!(
            ask(id, jdwp_method_id("nat", "()V")).error_code,
            ERR_NATIVE_METHOD
        );
        assert_eq!(
            ask(id, jdwp_method_id("gone", "()V")).error_code,
            ERR_INVALID_METHODID
        );
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
        assert_eq!(
            set.iter().map(|e| e.request_id).collect::<Vec<_>>(),
            vec![0]
        );
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

    /// Interpreter round i3 wave 83, lane L1: the funnel's `NativeMethodBind`
    /// bit arms the funnel's hook (`native_hooks_armed`) without arming any
    /// method-event consumer (`method_events_armed`), and the JVMTI method
    /// events still arm both.
    #[test]
    fn the_native_bind_bit_arms_only_the_funnel_hook() {
        let gates = DebuggerGates::new();
        assert!(!gates.native_hooks_armed() && !gates.method_events_armed());
        gates.set_native_bind_hook(true);
        assert!(gates.native_bind_hook_armed() && gates.native_hooks_armed());
        assert!(
            !gates.method_events_armed(),
            "no method-event consumer acts on it"
        );
        gates.set_jvmti_method_events(true);
        assert!(gates.method_events_armed() && gates.native_hooks_armed());
        gates.set_native_bind_hook(false);
        assert!(!gates.native_bind_hook_armed());
        assert!(gates.method_events_armed() && gates.native_hooks_armed());
        gates.set_jvmti_method_events(false);
        assert!(!gates.native_hooks_armed());
    }
}

#[cfg(test)]
mod gci_i11ja_vm_death_hold_tests {
    //! gci i11/ja (`gci-i10al-jvmti-event-callbacks-borrow-the-registry-jvmthread-under-the-posters-borrow-FIXED-20261017`
    //! item 2): the `VMDeath` hold opens no region of its own on a thread the
    //! exit native already made GC-safe.
    use super::*;
    use crate::threading::jvm_thread::{JvmThread, ThreadId};

    #[test]
    fn gci_i11ja_vm_death_hold_reads_the_blocked_flag_only() {
        let mut thread = JvmThread::new(ThreadId(3), "dying");
        let ptr = &mut thread as *mut JvmThread;
        assert!(!dying_thread_already_gc_safe(ptr), "a running thread");
        thread
            .gc_block_state
            .in_blocked_region
            .store(true, std::sync::atomic::Ordering::Release);
        let ptr = &mut thread as *mut JvmThread;
        assert!(dying_thread_already_gc_safe(ptr), "inside the exit native's region");
    }

    /// Source pin: the already-blocked arm comes first and only waits.
    #[test]
    fn gci_i11ja_vm_death_hold_skips_its_region_when_blocked() {
        let src = include_str!("mod.rs").replace("\r\n", "\n");
        let start = src.find("\nfn hold_for_vm_death(").expect("hold_for_vm_death");
        let rest = &src[start + 1..];
        let body = &rest[..rest.find("\n}\n").expect("its end")];
        let skip = body
            .find("Some(thread) if dying_thread_already_gc_safe(thread) => wait(),")
            .expect("the blocked arm");
        let region = body.find("begin_blocking_region(&mut ctx)").expect("the own region");
        assert!(skip < region);
    }
}

/// Interpreter round i4 wave 93, lane L1: the class-scoped method event set
/// of `DebuggerGates` (`CRATONVM_JDWP_SCOPED_METHOD_EVENTS`).
#[cfg(test)]
mod i93_l1_method_event_scope_tests {
    use super::*;

    fn scoped_gates() -> DebuggerGates {
        DebuggerGates {
            scoped_method_events: true,
            ..DebuggerGates::new()
        }
    }

    #[test]
    fn a_scoped_class_concerns_every_method_of_it_and_no_other_class() {
        let gates = scoped_gates();
        assert!(!gates.in_method_event_scope(70));
        let before = gates.generation();
        let gained = gates.store_method_event_classes(&[70], true, usize::MAX);
        assert_eq!(gained, vec![70]);
        assert!(gates.generation() > before, "the dispatch loops re-ask");
        assert!(gates.in_method_event_scope(70));
        assert!(gates.concerns_method(70, "anything", "()V"));
        assert!(!gates.in_method_event_scope(70 + 64), "same bit");
        assert!(!gates.concerns_method(71, "other", "()V"));
        assert!(gates.requires_interpreter(70, "hot", "(I)I"));
        assert!(
            !gates.requires_interpreter_everywhere(),
            "the other classes stay compiled"
        );
        assert!(gates.store_method_event_classes(&[70], false, usize::MAX).is_empty());
        gates.clear_method_event_scope();
        assert!(!gates.in_method_event_scope(70));
        assert!(!gates.concerns_method(70, "anything", "()V"));
    }

    /// A replacing store keeps a class defined at or after the resolved
    /// slot count (one a preparation joined while the refresh resolved) and
    /// drops the others the new scopes no longer name.
    #[test]
    fn a_replacing_store_carries_the_classes_joined_meanwhile() {
        let gates = scoped_gates();
        let _ = gates.store_method_event_classes(&[5, 120], false, usize::MAX);
        let gained = gates.store_method_event_classes(&[9], true, 100);
        assert_eq!(gained, vec![9]);
        assert!(gates.in_method_event_scope(9));
        assert!(gates.in_method_event_scope(120), "joined meanwhile");
        assert!(!gates.in_method_event_scope(5), "no longer named");
    }

    /// The frame-entry records a resumed frame invalidates.
    #[test]
    fn a_resumed_frame_drops_the_records_at_its_depth() {
        note_frame_entered(3);
        note_frame_resumed(3);
        assert!(!take_frame_entry(3));
        note_frame_entered(2);
        note_frame_resumed(3);
        assert!(take_frame_entry(2), "a shallower record stays");
    }
}

/// Interpreter round i5 wave 105, lane L1: the JDWP server loop's wake
/// (`CRATONVM_JDWP_PROMPT_EVENTS`, [`ServerInput::Wake`]).
#[cfg(test)]
mod i105_l1_server_wake_tests {
    use super::*;

    fn thread_start(thread_id: u64) -> DebugEvent {
        DebugEvent {
            kind: events::EventKind::ThreadStart,
            request_id: 1,
            suspend_policy: events::SuspendPolicy::None,
            thread_id,
            class_id: 0,
            method_id: 0,
            offset: 0,
            extra: Vec::new(),
            set_follows: false,
        }
    }

    #[test]
    fn a_burst_of_event_sets_wakes_the_server_once() {
        let shared = Arc::new(crate::vm::SharedVm::new(crate::config::VmConfig::default()));
        let (tx, rx) = std::sync::mpsc::channel::<DebugEvent>();
        *shared.debug.debug_event_tx.lock().expect("event channel") = Some(tx);
        let (wake_tx, wake_rx) = std::sync::mpsc::channel::<ServerInput>();
        *shared.debug.jdwp_server_wake.lock().expect("wake") = Some(wake_tx);

        send_event_set(&shared, vec![thread_start(5)]);
        send_event_set(&shared, vec![thread_start(6)]);
        assert_eq!(rx.try_iter().count(), 2, "both sets queued");
        let wakes = wake_rx
            .try_iter()
            .filter(|w| matches!(w, ServerInput::Wake))
            .count();
        assert_eq!(wakes, 1, "one wake in flight");

        // The loop took it (and cleared the flag before draining): the next
        // set wakes it again.
        shared
            .debug
            .jdwp_server_wake_pending
            .store(false, std::sync::atomic::Ordering::Release);
        send_event_set(&shared, vec![thread_start(7)]);
        assert_eq!(rx.try_iter().count(), 1);
        assert_eq!(wake_rx.try_iter().count(), 1);
    }
}

/// Interpreter round i7 wave 140, lane L1: `-agentlib:jdwp`'s `allow=`, as
/// HotSpot 25.0.3's socket transport parses and applies it (MEASURED,
/// `tools/probes/interp/L1/L1W140JdwpAllow.java`).
#[cfg(test)]
mod i140_l1_jdwp_allow_tests {
    use super::{jdwp_allow_admits, jdwp_peer_text, parse_jdwp_allow, JdwpAllow};
    use std::net::IpAddr;

    fn ip(text: &str) -> IpAddr {
        text.parse().expect("an address")
    }

    fn admits(spec: &str, peer: &str) -> bool {
        jdwp_allow_admits(&parse_jdwp_allow(spec).expect("a valid list"), ip(peer))
    }

    #[test]
    fn i140_l1_the_lists_hotspot_accepts() {
        assert_eq!(parse_jdwp_allow("*"), Ok(JdwpAllow::All));
        assert!(!admits("127.0.0.2", "127.0.0.1"));
        assert!(admits("127.0.0.1", "127.0.0.1"));
        assert!(admits("127.0.0.0/8", "127.0.0.1"));
        assert!(admits("10.1.2.3+127.0.0.1", "127.0.0.1"));
        assert!(admits("127.0.0.1/32+10.0.0.0/8", "127.0.0.1"));
        assert!(admits("127.0.0.2/0", "127.0.0.1"));
        assert!(!admits("::1", "127.0.0.1"));
        assert!(admits("::ffff:127.0.0.1", "127.0.0.1"));
        assert!(admits("::1", "::1"));
        assert!(!admits("10.0.0.0/8", "11.0.0.1"));
    }

    #[test]
    fn i140_l1_the_lists_hotspot_refuses_with_its_texts() {
        let ip_error = "invalid IP address in allow option";
        let mask_error = "invalid netmask in allow option";
        let refused = |spec: &str| parse_jdwp_allow(spec).unwrap_err();
        assert_eq!(refused("10.1.2.3;127.0.0.1"), ("10.1.2.3;127.0.0.1".to_string(), ip_error));
        assert_eq!(refused("bogus"), ("bogus".to_string(), ip_error));
        assert_eq!(refused("localhost"), ("localhost".to_string(), ip_error));
        assert_eq!(refused("127.1"), ("127.1".to_string(), ip_error));
        assert_eq!(refused("10.0.0.0/8+bogus"), ("bogus".to_string(), ip_error));
        assert_eq!(refused("127.0.0.1+"), (String::new(), ip_error));
        assert_eq!(refused("127.0.0.1/40"), ("40".to_string(), mask_error));
        assert_eq!(refused("127.0.0.1/"), (String::new(), mask_error));
        assert_eq!(refused("127.0.0.1/abc"), ("abc".to_string(), mask_error));
        assert_eq!(refused("::1/129"), ("129".to_string(), mask_error));
        assert_eq!(
            refused("*+127.0.0.1"),
            ("*+127.0.0.1".to_string(), "allow option '*' cannot be expanded")
        );
    }

    #[test]
    fn i140_l1_a_refused_peer_is_named_as_the_transport_names_it() {
        assert_eq!(jdwp_peer_text(ip("127.0.0.1"), false), "127.0.0.1");
        assert_eq!(jdwp_peer_text(ip("127.0.0.1"), true), "::ffff:127.0.0.1");
    }
}
