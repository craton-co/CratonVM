#[allow(unused_imports)]
use super::*;


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
pub(in crate::debug) fn event_names_a_thread(kind: events::EventKind) -> bool {
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
pub(in crate::debug) const STATE_SUSPEND_PRUNE_FLOOR: usize = 64;

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
    pub(in crate::debug) suspend_all_count: u32,
    /// Per-thread adjustment of a thread's suspend count against
    /// `suspend_all_count` (`ThreadReference.Suspend` / `Resume`, a
    /// `SUSPEND_EVENT_THREAD` event). A thread's count is the sum, never
    /// below zero; ask [`DebugState::suspend_count`]. HotSpot keeps one
    /// counter per thread: a thread suspended n times runs after n resumes,
    /// and `VirtualMachine.Resume` takes one from every thread's count.
    /// Until wave 7 this was a set and a flag, so a thread suspended both on
    /// its own and by the VM ran after one resume.
    pub(in crate::debug) suspend_adjust: HashMap<u64, i64>,
    /// Threads a C JVMTI agent suspended (`SuspendThread`, interpreter round
    /// i1 wave 45, lane L1; `jvmti::native_env`). JVMTI's suspension is a
    /// flag, not a count: a second `SuspendThread` is refused
    /// (`JVMTI_ERROR_THREAD_SUSPENDED`) and one `ResumeThread` ends it. It
    /// parks the thread as a JDWP suspension does
    /// ([`Self::is_thread_suspended`]), and a debugger's detach
    /// ([`Self::resume_all`]) leaves it in force: it is the agent's.
    pub(in crate::debug) jvmti_suspended: HashSet<u64>,
    /// Short suspensions the C JVMTI table takes to read a running thread's
    /// stack (wave 45: HotSpot reads it through a handshake), counted per
    /// thread: the reader waits for the thread to stop, reads the listing it
    /// published, and ends its suspension ([`Self::end_handshake`]).
    pub(in crate::debug) jvmti_handshakes: HashMap<u64, u32>,
    /// The threads waiting out a suspension on their own word
    /// (`GcBlockState::debugger_suspend_word`; interpreter round i2 wave 63,
    /// lane L6, `CRATONVM_DEBUG_THREAD_SUSPEND_WORD`): enrolled by
    /// [`SuspendWordWait::enroll`] when such a wait starts, gone when it
    /// ends. Every change of what [`Self::suspend_word_flags`] answers for an
    /// enrolled thread rewrites its word and unparks it
    /// ([`Self::republish_suspend_word`]). Empty without the switch, and
    /// then every republication is one `is_empty` test.
    pub(in crate::debug) suspend_words: HashMap<u64, SuspendWordEntry>,
    /// The threads whose `GcBlockState::jvmti_state_suspended` a C JVMTI
    /// `GetThreadState` subscribed (interpreter round i7 wave 135, lane L1,
    /// `CRATONVM_JVMTI_STATE_SUSPEND_WORD`, default off; i64-L1 item 1):
    /// [`Self::republish_suspend_word`] / [`Self::republish_suspend_words`]
    /// rewrite each one's answer to [`Self::is_thread_suspended`] under the
    /// lock, enrolled or not. Empty without the switch: one `is_empty` test.
    pub(in crate::debug) state_suspend_words: HashMap<u64, Arc<crate::threading::jvm_thread::GcBlockState>>,
    /// The size at which [`Self::subscribe_state_suspend_word`] next drops
    /// the subscriptions of threads whose `JvmThread` is gone
    /// (`GcBlockState::owner_dropped`; interpreter round i7 wave 136, lane
    /// L1b): a `GetThreadState` made after the thread's `ThreadDeath`
    /// unsubscribed it (`forget_jvmti_suspension`) but before it left the
    /// registry subscribed it again, for good. Doubles after each sweep, so
    /// the sweeps cost O(1) amortised per subscription.
    pub(in crate::debug) state_suspend_prune_at: usize,
    /// [`Self::any_suspension`], kept current by every mutator of a suspend
    /// count, a JVMTI suspension or a stack-read handshake (each calls
    /// [`Self::republish_suspend_word`] or [`Self::republish_suspend_words`]
    /// before it returns, under the lock its caller holds), and readable
    /// without the debug-state lock through the handle the VM keeps
    /// (`DebugRealm::suspension_hint`; interpreter round i3 wave 72, lane L6,
    /// `CRATONVM_JVMTI_STATE_SUSPENSION_HINT`). Down: no thread is suspended,
    /// so C JVMTI `GetThreadState` adds no `SUSPENDED` bit without taking the
    /// lock. Up: the reader asks under the lock, as before.
    pub(in crate::debug) suspension_hint: Arc<std::sync::atomic::AtomicBool>,
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
    pub(in crate::debug) frame_epoch: u32,
    /// Per-thread part of the frame generation; see [`Self::frame_epoch`].
    pub(in crate::debug) frame_generations: HashMap<u64, u32>,
    /// Bumped when the debugger detaches ([`DebugState::resume_all`]), so an
    /// invocation still running then does not re-suspend threads for a
    /// session that is gone (wave 9, [`run_invocation_on_parked_thread`]).
    pub(in crate::debug) session: u64,
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
    pub(in crate::debug) vm_death_sent: bool,
    /// `ThreadReference.Stop` (interpreter round i1 wave 24): thread id → the
    /// object id of the throwable it must throw at its next interpreted
    /// bytecode ([`take_pending_stop`]). The id's collection is disabled
    /// until then, so the throwable stays alive when the debugger lets go
    /// of it.
    pub(in crate::debug) pending_stops: HashMap<u64, u64>,
    /// `ThreadReference.ForceEarlyReturn` (interpreter round i1 wave 43):
    /// the parks it is served at and the values recorded
    /// ([`early_return`]).
    pub(crate) early_returns: early_return::EarlyReturns,
    /// The threads the VM started as virtual threads (interpreter round i1
    /// wave 25): recorded at their `ThreadStart` ([`send_thread_event_of`]),
    /// where the VM knows it from the `Thread` object's class, before the
    /// virtual-thread table does. Read by `PlatformThreadsOnly` and
    /// `ThreadReference.IsVirtual`.
    pub(in crate::debug) virtual_threads: HashSet<u64>,
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
    pub(in crate::debug) class_prepare_reported: HashSet<u64>,
    /// Classes the server's poll found defined but not yet prepared (wave
    /// 10), as `(wire class id, JNI signature, type tag)` and when first
    /// seen: their `ClassPrepare` is left to the thread that prepares them,
    /// and reported by the poll only once they are prepared (by a path that
    /// did not report them) or [`CLASS_PREPARE_GRACE`] has passed.
    pub(in crate::debug) class_prepare_pending: Vec<((u64, String, u8), std::time::Instant)>,

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
    pub(in crate::debug) parked_threads: HashMap<u64, Vec<bool>>,
    /// Per level of [`Self::parked_threads`], innermost last: did an event
    /// suspend the thread for that park (interpreter round i1 wave 44, lane
    /// L1)? Only such a park takes a debugger invocation, as HotSpot's back
    /// end invokes only on a thread an event suspended (its invoker's
    /// `available` flag): one suspended by `VirtualMachine.Suspend`, a
    /// `ThreadReference.Suspend` or the re-suspension that ends another
    /// thread's invocation is `INVALID_THREAD`
    /// (`tools/probes/interp/L1/L1W43RawJdwpObjectErrorAnswers.java`,
    /// `invokeMethod(on running other)`: 10 on HotSpot 25.0.3).
    pub(in crate::debug) event_parks: HashMap<u64, Vec<bool>>,
    /// Threads an event set's suspend policy suspended
    /// ([`Self::apply_event_set_policy`]) that have not parked for it yet:
    /// the next park of each is an event park ([`Self::note_parked`]).
    pub(in crate::debug) event_park_pending: HashSet<u64>,
    /// Work for a parked thread, taken by that thread's park loop
    /// ([`DebugState::take_parked_task`]).
    pub(in crate::debug) parked_tasks: HashMap<u64, ParkedTask>,
    /// The heap service (wave 10, [`start_heap_service`]): a daemon thread
    /// attached to the VM that runs heap work — a heap command, an
    /// allocation — when no thread is parked at a suspend point. The job
    /// channel and the service's thread id, which the debugger never sees.
    pub(in crate::debug) heap_service: Option<(
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
pub(in crate::debug) fn bump_frame_generation(generations: &mut HashMap<u64, u32>, tid: u64, delta: u32) {
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
            include_virtual_threads: false,
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
            hide_unprepared_classes: true,
            class_prepare_watermark: None,
            destroy_java_vm_identity: true,
            destroy_java_vm_split: None,
            ended_main_handle: 0,
            heap_walk_from_roots: true,
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
            event_object_hold: true,
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
    pub(in crate::debug) fn is_suspended_by_jvmti(&self, tid: u64) -> bool {
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
    pub(in crate::debug) fn publish_state_suspend_word(
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
    pub(in crate::debug) fn unsubscribe_state_suspend_word(&mut self, tid: u64) {
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
    pub(in crate::debug) fn refresh_suspension_hint(&self) {
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
    pub(in crate::debug) fn enroll_suspend_word(
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
    pub(in crate::debug) fn leave_suspend_word(&mut self, tid: u64) {
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
    pub(in crate::debug) fn can_take_work(&self, tid: u64) -> bool {
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
    pub(in crate::debug) fn park_region_open(&self, tid: u64) -> bool {
        self.suspend_words.get(&tid).is_some_and(|entry| {
            entry
                .state
                .in_blocked_region
                .load(std::sync::atomic::Ordering::Acquire)
        })
    }

    /// The parked threads that can take work now ([`Self::can_take_work`]).
    pub(in crate::debug) fn idle_parked_threads(&self) -> impl Iterator<Item = u64> + '_ {
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
    pub(in crate::debug) fn heap_work_target(&self, outside_region: bool, service_ok: bool) -> HeapWorkTarget {
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
pub(in crate::debug) const SERVER_READ_WAIT: std::time::Duration = std::time::Duration::from_millis(2);

/// How long one write to a debugger that reads nothing may block before the
/// session is given up (the stall limit `protocol::write_packet_nonblocking`
/// applies to a non-blocking socket).
pub(in crate::debug) const SERVER_WRITE_WAIT: std::time::Duration = std::time::Duration::from_secs(60);

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
pub(in crate::debug) fn spawn_command_reader(
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
pub(in crate::debug) fn events_may_follow_reply_only(command_set: u8, command: u8) -> bool {
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
            true;
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
pub(in crate::debug) fn drain_interpreter_events(
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
pub(in crate::debug) fn await_event_set_settled(
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
pub(in crate::debug) fn note_vm_death_sent(shared: &crate::vm::SharedVm, set: &[events::Event]) {
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
pub(in crate::debug) fn event_set_policy_byte(policy: events::SuspendPolicy, wire: Option<u8>) -> u8 {
    match (policy, wire) {
        (events::SuspendPolicy::EventThread, Some(wire)) => wire,
        (policy, _) => policy as u8,
    }
}

/// Write one composite event packet of `set` with the suspend-policy byte
/// `policy` ([`event_set_policy_byte`]). `false` when the connection failed.
pub(in crate::debug) fn send_composite(
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
pub(in crate::debug) fn send_class_prepare_events(
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
pub(in crate::debug) fn send_class_unload_events(
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
pub(in crate::debug) fn class_unload_packets(shared: &crate::vm::SharedVm, report: bool) -> Vec<JdwpPacket> {
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
pub(in crate::debug) const CLASS_PREPARE_GRACE: std::time::Duration = std::time::Duration::from_secs(1);

/// The classes the server's poll reports now: of `added` (new since the last
/// poll) and the ones pending from earlier polls, those prepared (or further)
/// and those pending for [`CLASS_PREPARE_GRACE`]; the rest stay pending.
/// Takes the class-manager lock, then the debug-state lock.
pub(in crate::debug) fn classes_ready_for_prepare_events(
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
pub(in crate::debug) const METHOD_EVENTS_JDWP: u8 = 1;
/// [`DebuggerGates::method_events_armed`]'s bit for a JVMTI env listening for
/// `MethodEntry` or `MethodExit` (wave 27).
pub(in crate::debug) const METHOD_EVENTS_JVMTI: u8 = 2;
/// [`DebuggerGates::method_events_armed`]'s bit for a JDWP or JVMTI
/// breakpoint, or a JDWP step request, in force (interpreter round i1 wave
/// 28, lane L1): a Java method a registered native or an interpreter
/// intrinsic stands in for must run its bytecode, in a frame, when a
/// breakpoint sits in it or a step may enter it
/// ([`DebuggerGates::java_frames_armed`],
/// `interpreter::run_stood_in_java_method`).
pub(in crate::debug) const METHOD_EVENTS_FRAMES: u8 = 4;
/// [`DebuggerGates::method_events_armed`]'s bit for a JVMTI env of this VM
/// listening for `SingleStep` (interpreter round i1 wave 29, lane L1): a
/// stepping agent must step through a stood-in Java method's bytecode, as
/// HotSpot's interpreter-only mode does ([`DebuggerGates::jvmti_frames_armed`],
/// `interpreter::run_stood_in_java_method`). Set by the C JVMTI table only.
pub(in crate::debug) const METHOD_EVENTS_JVMTI_FRAMES: u8 = 8;
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
pub(in crate::debug) const METHOD_EVENTS_BLOCKING: u8 = 16;
/// [`DebuggerGates::native_hooks_armed`]'s bit for a C JVMTI env of this VM
/// listening for `NativeMethodBind` since `Agent_OnLoad` (interpreter round
/// i3 wave 83, lane L1): the native-call funnel posts the event the first
/// time it runs a registered (Rust) native, as HotSpot posts it when it links
/// one (`jvmti::native_env::post_rust_native_method_bind`). Not part of
/// [`DebuggerGates::method_events_armed`]: no method-event consumer acts on
/// it; only the funnel and its leaf twin read the whole word.
pub(in crate::debug) const METHOD_EVENTS_NATIVE_BIND: u8 = 32;

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
pub(in crate::debug) const RUNNING_NATIVE_SESSION: u8 = 1;
/// `DebuggerGates::running_native_record`: a C JVMTI env is bound and
/// `CRATONVM_JVMTI_C_RUNNING_NATIVE` is on (wave 61, lane L6).
pub(in crate::debug) const RUNNING_NATIVE_C_ENV: u8 = 2;
/// `DebuggerGates::running_native_record`: a C JVMTI env is bound and
/// `CRATONVM_JVMTI_C_RUNNING_NATIVE` is on, but the funnel's own callback
/// record is not kept, because the raised native-excuse word already carries
/// the callback (`CRATONVM_JVMTI_C_FUNNEL_RECORD_FROM_EXCUSE`; interpreter
/// round i6 wave 133, lane L1,
/// `docs/feature-designs/jvmti-delivery-threading.md`).
/// Leaf natives still take the full funnel (it raises the word), but
/// `interpreter::enter_running_native` records nothing for this bit
/// ([`DebuggerGates::running_native_armed`] ignores it).
pub(in crate::debug) const RUNNING_NATIVE_C_LEAF_ONLY: u8 = 4;

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
    pub(in crate::debug) generation: std::sync::atomic::AtomicU64,
    /// A step request or a suspension is in force: every method.
    pub(in crate::debug) all_methods: std::sync::atomic::AtomicBool,
    /// Bit `class_id % 64` of every class holding a breakpoint.
    pub(in crate::debug) breakpoint_classes: std::sync::atomic::AtomicU64,
    /// `(class id, jdwp_method_id)` of every method holding a breakpoint.
    pub(in crate::debug) breakpoint_methods: parking_lot::RwLock<HashSet<(u64, u64)>>,
    /// A `FieldAccess` / `FieldModification` request is in force (wave 10):
    /// the four field bytecodes run their JDWP hook
    /// (`interpreter::deliver_field_watch_if_armed`), behind the process-wide
    /// `jvmti::any_field_watchpoint_active` pre-filter this also raises
    /// (`jvmti::note_debugger_field_watch`), which already turns off every
    /// quickened field arm.
    pub(in crate::debug) field_watch: std::sync::atomic::AtomicBool,
    /// An `Exception` request is in force (wave 10): the interpreter's
    /// unwinder runs its JDWP hook (`interpreter::deliver_exception_event_if_armed`).
    pub(in crate::debug) exceptions: std::sync::atomic::AtomicBool,
    /// Every method must run interpreted, without the dispatch loop's
    /// per-bytecode suspend point (wave 10): a field watch or an exception
    /// request is in force, and only the interpreter posts those events. The
    /// JIT's doors ask [`Self::requires_interpreter`]. Wave 40 (lane L1): or
    /// a breakpoint sits in a method of a JDK class
    /// ([`WITHDRAWAL_BY_JDK_BREAKPOINT`]), which a caller compiled after it
    /// could expand as a call-site intrinsic.
    pub(in crate::debug) interpret_all: std::sync::atomic::AtomicBool,
    /// A `ClassPrepare` request is in force (wave 10): a class prepared by a
    /// thread of this VM is reported from that thread
    /// ([`class_prepared_on_thread`]).
    pub(in crate::debug) class_prepare: std::sync::atomic::AtomicBool,
    /// A debugger is attached (wave 10): a thread entering a blocking native
    /// region opens its frames to the debugger
    /// (`interpreter::open_blocked_inspection`, wave 12), so
    /// `ThreadReference.Frames` can answer for a thread suspended while it is
    /// blocked.
    pub(in crate::debug) session: std::sync::atomic::AtomicBool,
    /// This VM is configured with a JDWP server (`vm_init`, as it spawns
    /// `run_jdwp_server`; the i54-L1 retirement): a debugger may attach at
    /// any time, and a lambda call it
    /// finds in progress must have recorded its proxy method's frame
    /// ([`Self::c_lambda_frames_armed`]).
    pub(in crate::debug) jdwp_listening: std::sync::atomic::AtomicBool,
    /// A C JVMTI env bound after lambda proxies existed (the i54-L1
    /// retirement): a lambda call in progress then recorded no proxy frame,
    /// and the C table's listing infers it (`jvmti::native_env::with_inferred_proxy_rows`).
    pub(in crate::debug) proxy_rows_incomplete: std::sync::atomic::AtomicBool,
    /// A JDWP suspension is in force (wave 12; `DebugState::any_suspension`):
    /// a thread entering a blocking native region checks its own suspend
    /// count only while this is up (`interpreter::publish_blocked_frames`).
    pub(in crate::debug) suspension: std::sync::atomic::AtomicBool,
    /// Any suspension is counted, a thread listed ZOMBIE's own included
    /// (`DebugState::any_suspension`; interpreter round i5 wave 106, lane L1):
    /// the gate of the hold whose thread may be listed ended
    /// ([`hold_native_thread_while_suspended`]; its slow path then asks that
    /// thread's own count).
    /// [`Self::suspension`] leaves such a suspension out on purpose (wave 29:
    /// it must not stop every live thread), so a hold of an ended thread
    /// gated on it returned at once (lane L6, wave 105). Published with it.
    pub(in crate::debug) suspension_counted: std::sync::atomic::AtomicBool,
    /// A C JVMTI env of this VM has `ClassPrepare` enabled (interpreter
    /// round i1 wave 13): [`class_prepared_on_thread`] delivers it to the
    /// agent's callback (`jvmti::native_env::post_class_prepare`). Set by
    /// `native_env`, never by [`publish_debugger_gates`].
    pub(in crate::debug) native_class_prepare: std::sync::atomic::AtomicBool,
    /// A C JVMTI env of this VM has `ThreadStart` or `ThreadEnd` enabled
    /// (interpreter round i1 wave 52, lane L1): a platform thread posts them
    /// before its `run()` and after it (`jvmti::native_env::post_thread_event`).
    /// One load per thread start and end.
    pub(in crate::debug) native_thread_events: std::sync::atomic::AtomicBool,
    /// `main` has ended and the launcher's registry entry (VM thread 0) is
    /// the `DestroyJavaVM` thread (interpreter round i5 wave 104, lane L1;
    /// [`DebugState::destroy_java_vm_split`], whose copy this is for the
    /// paths that hold no debug-state lock: [`send_event_set`] sends an event
    /// of thread 0 as `ids::DESTROY_JAVA_VM_WIRE_ID`). Never cleared.
    pub(in crate::debug) destroy_java_vm_split: std::sync::atomic::AtomicBool,
    /// The events of the C JVMTI table's wave-63 set a C env of this VM has
    /// enabled, by [`NativeEvent`] bit (interpreter round i2 wave 63, lane
    /// L1; `jvmti::native_env::events_more::refresh_event_gates`): each
    /// posting site asks one relaxed load before anything else.
    pub(in crate::debug) native_events: std::sync::atomic::AtomicU32,
    /// The [`NativeEvent`] bits the JDWP server's requests arm (interpreter
    /// round i5 wave 103, lane L1): `Monitor` while a monitor event request
    /// (43-46) is in force, so the monitor paths that post the C JVMTI events
    /// reach `debug::post_jdwp_monitor_event` too. Published by
    /// [`publish_debugger_gates`]; [`Self::native_event_armed`] answers the
    /// union.
    pub(in crate::debug) jdwp_native_events: std::sync::atomic::AtomicU32,
    /// A C JVMTI env of this VM has `ClassLoad` enabled (interpreter round
    /// i2 wave 53, lane L1): a class's definition posts it on the defining
    /// thread (`jvmti::native_env::post_class_load`). One load per class
    /// defined.
    pub(in crate::debug) native_class_load: std::sync::atomic::AtomicBool,
    /// A C JVMTI env of this VM has `ClassFileLoadHook` enabled and the VM is
    /// in the live phase (wave 53, lane L1): the load path offers each class
    /// file to the agent's callback before the class is defined, inside the
    /// transformer chain `java.lang.instrument` uses
    /// (`runtime::instrument::load_hook_armed`,
    /// `jvmti::native_env::post_class_file_load_hook`).
    pub(in crate::debug) native_class_file_load_hook: std::sync::atomic::AtomicBool,
    /// A C JVMTI env is bound to this VM (interpreter round i1 wave 45, lane
    /// L1): a thread entering a blocking native region opens its inspection
    /// window (`interpreter::open_blocked_inspection`), so the table's stack
    /// functions can read it from another thread, as HotSpot reads a blocked
    /// thread's stack. Set once by `native_env` when an env binds, never
    /// cleared.
    pub(in crate::debug) native_env_present: std::sync::atomic::AtomicBool,
    /// The callbacks registered for [`BLOCKING_STANDINS`], in that order, 0
    /// for none (interpreter round i1 wave 46, lane L1): filled when
    /// [`METHOD_EVENTS_BLOCKING`] is raised ([`arm_blocking_standins`]).
    pub(in crate::debug) blocking_standins: [std::sync::atomic::AtomicUsize; BLOCKING_STANDINS.len()],
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
    pub(in crate::debug) jvmti_phase: std::sync::atomic::AtomicU8,
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
    pub(in crate::debug) c_jvmti_phases: bool,
    /// `CRATONVM_JVMTI_C_LAMBDA_CLASSES`, read once when this VM's gates are
    /// made (interpreter round i2 wave 55, lane L1;
    /// `docs/feature-designs/jvmti-delivery-threading.md`):
    /// the C JVMTI table lists the lambda proxy classes the VM mints outside
    /// the class store in `GetLoadedClasses` and answers their signature,
    /// modifiers, status, loader, interfaces and version
    /// (`jvmti::native_env::JvmtiClass::LambdaProxy`). Off (`=0`): it refuses them
    /// with `JVMTI_ERROR_INVALID_CLASS`, as before. On by default (wave 55's
    /// last commit). Plain data: never written after construction.
    pub(in crate::debug) c_lambda_classes: bool,
    /// `CRATONVM_JVMTI_C_LAMBDA_METHODS`, read once when this VM's gates are
    /// made (interpreter round i2 wave 56, lane L1;
    /// `docs/feature-designs/jvmti-delivery-threading.md`):
    /// the C JVMTI table's `GetClassMethods` lists a lambda proxy class's
    /// methods, and the method functions answer them, from the one record of
    /// its facts (`runtime::lambda_proxy::lambda_proxy_facts`). Off: they
    /// answer `JVMTI_ERROR_INVALID_CLASS` / `JVMTI_ERROR_INVALID_METHODID`,
    /// as before. On by default (wave 56's last commit). Plain data: never
    /// written after construction.
    pub(in crate::debug) c_lambda_methods: bool,
    /// `CRATONVM_JVMTI_C_LAMBDA_CODE`, read once when this VM's gates are
    /// made (interpreter round i2 wave 57, lane L1;
    /// `docs/feature-designs/jvmti-delivery-threading.md`):
    /// the C JVMTI table's `GetMethodLocation` and `GetBytecodes` answer a
    /// lambda proxy's method with the code `InnerClassLambdaMetafactory`
    /// spins (`runtime::lambda_proxy::lambda_proxy_method_code`). Off: they
    /// answer `JVMTI_ERROR_ABSENT_INFORMATION`, as before. Only with
    /// [`Self::c_lambda_methods`]. On by default (wave 57's last commit).
    /// Plain data: never written after construction.
    pub(in crate::debug) c_lambda_code: bool,
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
    pub(in crate::debug) c_lambda_frames: bool,
    /// `CRATONVM_JVMTI_C_LAMBDA_EVENTS`, read once when this VM's gates are
    /// made (interpreter round i2 wave 60, lane L1;
    /// `docs/feature-designs/jvmti-delivery-threading.md`):
    /// with [`Self::c_lambda_frames`] armed, the C JVMTI envs listening for
    /// `MethodEntry` / `MethodExit` get them for the lambda proxy method a
    /// call enters (`interpreter::lambda::dispatch_with_proxy_frame`), as
    /// HotSpot posts them for the spun class's method. Off: none, as before.
    /// Plain data: never written after construction.
    pub(in crate::debug) c_lambda_events: bool,
    /// `CRATONVM_JNI_LAMBDA_METHOD_IDS`, read once when this VM's gates are
    /// made (interpreter round i2 wave 60, lane L1; stage 2 of
    /// `docs/feature-designs/jvmti-delivery-threading.md`):
    /// while a C JVMTI env is bound, JNI's `GetMethodID` of a lambda's class
    /// answers a method the class declares with the proxy's own `jmethodID`
    /// (the one the C table lists it by), as HotSpot answers the hidden
    /// class's method. Off: the functional interface's method, as before.
    /// Only with [`Self::c_lambda_methods`]. Plain data: never written after
    /// construction.
    pub(in crate::debug) jni_lambda_method_ids: bool,
    /// `CRATONVM_JVMTI_C_PRIMITIVE_CLASSES`, read once when this VM's gates
    /// are made (interpreter round i2 wave 56, lane L1;
    /// `docs/feature-designs/jvmti-delivery-threading.md`):
    /// the C JVMTI table's class functions answer a primitive class's mirror
    /// (`int.class`, which has no `ClassId`) with HotSpot's facts
    /// (`jvmti::native_env::JvmtiClass::Primitive`). Off: they refuse it with
    /// `JVMTI_ERROR_INVALID_CLASS`, as before. On by default (wave 56's last
    /// commit). Plain data: never written after construction.
    pub(in crate::debug) c_primitive_classes: bool,
    /// A JDWP `MethodEntry` request is in force in this VM (interpreter
    /// round i1 wave 24). Kept for its edges, which move the process-wide
    /// pre-filter the frame pushes read ([`method_entry_requests_anywhere`]);
    /// since wave 95 (lane L1) a VM dropped with a request in force takes
    /// its unit back ([`RequestVmsEdge`]).
    pub(in crate::debug) method_entry: RequestVmsEdge,
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
    pub(in crate::debug) method_events: std::sync::atomic::AtomicU8,
    /// A `ThreadReference.Stop` is pending for some thread of this VM
    /// (interpreter round i1 wave 25): the interpreter's unwinder asks
    /// [`exception_or_pending_stop`] before it routes an exception, so a stop
    /// replaces the exception in flight — the `InterruptedException` of the
    /// blocking call the stop woke — as HotSpot's asynchronous exception
    /// replaces a pending one.
    pub(in crate::debug) stops: std::sync::atomic::AtomicBool,
    /// `-agentlib:jdwp`'s start deferred to a matching throw (interpreter
    /// round i7 wave 144, lane L1; [`Self::jdwp_deferred_start_armed`]).
    pub(in crate::debug) jdwp_deferred_start: std::sync::atomic::AtomicBool,
    /// A `StackFrame.SetValues` sent while its thread was blocked in a native
    /// waits to be applied by that thread (interpreter round i1 wave 41,
    /// lane L1; [`DebugState::deferred_frame_writes`]): the blocking region's
    /// exit asks `inspect::apply_deferred_frame_writes_if_any` only while
    /// this is up. Written under the debug-state lock only.
    pub(in crate::debug) frame_writes: std::sync::atomic::AtomicBool,
    /// A `StackFrame.SetValues` to a compiled activation waits for that
    /// activation's post-call exit (interpreter round i6 wave 123, lane L2;
    /// [`DebugState::compiled_home_writes`]): the deopt resume asks
    /// `inspect::apply_compiled_home_writes` only while this is up. Written
    /// under the debug-state lock only.
    pub(in crate::debug) compiled_home_writes: std::sync::atomic::AtomicBool,
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
    pub(in crate::debug) withdrawal: std::sync::atomic::AtomicU8,
    /// Until when (milliseconds on [`Self::linger_clock_origin`]'s clock) the
    /// tier-up strides stay closed after [`Self::withdrawal`] fell to `0`
    /// (wave 39, lane L1; [`WITHDRAWAL_LINGER_MS`]); `0` when not lingering.
    /// Read by [`Self::tier_up_held`] only while no source is armed.
    pub(in crate::debug) withdrawal_linger_until: std::sync::atomic::AtomicU64,
    /// The code cache's generation (`JitCache::generation`) the last
    /// whole-cache withdrawal left (wave 39): the next arm finds nothing
    /// compiled since while it still reads this, and skips its flush
    /// (`interpreter::note_every_method_needs_the_interpreter`). `0` before
    /// the first withdrawal, which no cache generation equals.
    pub(in crate::debug) withdrawal_generation: std::sync::atomic::AtomicU64,
    /// Arms of the withdrawal that found nothing compiled since the last one
    /// and so withdrew nothing (wave 39): a count for the tests and the
    /// stepping-session positive control.
    pub(in crate::debug) withdrawals_held: std::sync::atomic::AtomicU64,
    /// The origin of the linger's monotonic clock: this VM's gates' creation.
    pub(in crate::debug) linger_clock_origin: std::time::Instant,
    /// Bumped whenever [`Self::breakpoint_methods`] changes (interpreter
    /// round i1 wave 44, lane L1): the generation [`Self::caller_verdicts`]
    /// are judged at. Not [`Self::generation`], which every publication
    /// bumps (a suspension and its resume at each stop included).
    pub(in crate::debug) breakpoint_generation: std::sync::atomic::AtomicU64,
    /// `(class id, jdwp_method_id)` of a method the compile doors asked
    /// about -> `(breakpoint generation, does its bytecode invoke a method
    /// holding a breakpoint)` (wave 44, lane L1;
    /// `interpreter::breakpoint_bars_compiling`). A verdict of an older
    /// generation is stale; the map is emptied when the last breakpoint
    /// goes. Asked only at compile time and at the tier-up strides, while a
    /// breakpoint is set.
    pub(in crate::debug) caller_verdicts: parking_lot::Mutex<HashMap<(u32, u64), (u64, bool)>>,
    /// `CRATONVM_JDWP_SCOPED_METHOD_EVENTS`, read once when this VM's gates
    /// are made (interpreter round i4 wave 93, lane L1; default OFF; page
    /// `docs/feature-designs/jvmti-delivery-threading.md`):
    /// a JDWP `MethodEntry` / `MethodExit` request confined to a set of
    /// classes by a `ClassOnly` or exact `ClassMatch` modifier
    /// (`events::method_event_scope_of`) keeps only those classes' methods
    /// interpreted ([`Self::in_method_event_scope`]) and withdraws only their
    /// dependents' compiled code, instead of every method of the VM. Plain
    /// data: never written after construction.
    pub(in crate::debug) scoped_method_events: bool,
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
    pub(in crate::debug) exceptions_from_compiled: bool,
    /// An `Exception` request is in force that this VM serves from compiled
    /// code (wave 93, [`Self::exceptions_from_compiled`]). Kept for its
    /// edges, which move the process-wide pre-filter the compiled `athrow`
    /// helper reads ([`compiled_exception_requests_anywhere`]); since wave 95
    /// (lane L1) a VM dropped with such a request in force takes its unit
    /// back ([`RequestVmsEdge`]).
    pub(in crate::debug) compiled_exception_requests: RequestVmsEdge,
    /// The classes a JDWP field watch keeps interpreted under
    /// `CRATONVM_JDWP_SCOPED_FIELD_WATCHES` (interpreter round i4 wave 95,
    /// lane L1; default ON since the wave-129 triage, `=0` off;
    /// [`field_watch_scope`]).
    pub(in crate::debug) field_watch_scope: field_watch_scope::FieldWatchScope,
    /// Bit `class_id % 64` of every class in [`Self::method_event_classes`]
    /// (wave 93): the one load [`Self::in_method_event_scope`] answers `false`
    /// with for every other class.
    pub(in crate::debug) method_event_class_bits: std::sync::atomic::AtomicU64,
    /// The classes a class-scoped method event request in force concerns
    /// (wave 93; [`refresh_method_event_scope`], [`join_method_event_scope`]).
    pub(in crate::debug) method_event_classes: parking_lot::RwLock<HashSet<u32>>,
    /// The scopes [`Self::method_event_classes`] was resolved for (wave 93).
    /// [`publish_debugger_gates`] treats a scoped request whose scope is not
    /// here as concerning every method. Written only by
    /// [`refresh_method_event_scope`]; held for a clone or a compare, never
    /// across another lock.
    pub(in crate::debug) method_event_specs: parking_lot::RwLock<Vec<events::MethodEventScope>>,
    /// The class store's slot count [`refresh_method_event_scope`] last
    /// resolved through (wave 93): the classes defined since are resolved by
    /// the next refresh or, when prepared, by [`join_method_event_scope`].
    pub(in crate::debug) method_event_slots_seen: std::sync::atomic::AtomicUsize,
    /// A class a scope names cannot be kept interpreted alone (wave 93): a
    /// JDK class (the JIT expands some of their methods at a call site
    /// without a record), an interface or a class not in the store named by
    /// `ClassOnly`, or a class with a native method (a compiled caller calls
    /// a native without the funnel that reports its events). The scoped
    /// requests then concern every method, as before wave 93.
    pub(in crate::debug) method_event_degraded: std::sync::atomic::AtomicBool,
    /// The [`Self::method_event_degraded`] the last [`publish_debugger_gates`]
    /// read (interpreter round i4 wave 94, lane L1): a join that degraded the
    /// scope on a thread that could not publish (`debugger_hooks_suppressed`)
    /// leaves the two apart, and the server's next poll
    /// ([`refresh_method_event_scope`]) publishes. Without it the poll saw
    /// the flag already set and published nothing, and the degraded scope
    /// stayed unpublished until the next command.
    pub(in crate::debug) method_event_degraded_published: std::sync::atomic::AtomicBool,
    /// A scope is resolved (wave 93): a class prepared now is matched against
    /// it ([`join_method_event_scope`]). One load per class prepared.
    pub(in crate::debug) method_event_scope_armed: std::sync::atomic::AtomicBool,
    /// Bumped whenever [`Self::method_event_classes`] grows (wave 93).
    pub(in crate::debug) method_event_scope_generation: std::sync::atomic::AtomicU64,
    /// The [`Self::method_event_scope_generation`] the last
    /// [`publish_debugger_gates`] saw: a change sends running compiled loops
    /// to their exit polls, as a grown breakpoint set does.
    pub(in crate::debug) method_event_scope_published: std::sync::atomic::AtomicU64,
    /// `CRATONVM_DEBUG_ONE_FRAME_LISTING`, read once when this VM's gates
    /// are made (interpreter round i1 wave 47, lane L1; stage 1 of
    /// `docs/feature-designs/jvmti-delivery-threading.md`):
    /// the JDWP listing and the C JVMTI table's listing of a blocked thread
    /// also list the JNI natives in the middle of a thread's stack and its
    /// reflective calls' JDK frames (`interpreter::build_frame_listing`), as
    /// the current thread's JVMTI listing does. Plain data: never written
    /// after construction.
    pub(in crate::debug) one_frame_listing: bool,
    /// `CRATONVM_DEBUG_COMPATIBLE_BLOCKING_BYTECODE`, read once when this
    /// VM's gates are made (interpreter round i1 wave 48, lane L1; stage 1 of
    /// `docs/feature-designs/jvmti-delivery-threading.md`):
    /// while a debugger is attached ([`METHOD_EVENTS_BLOCKING`]), a
    /// `--compatible` call of a [`BLOCKING_STANDINS`] method runs its JDK
    /// bytecode as `--real-jdk` does (`jvmti_events::run_blocking_standin`),
    /// so the thread blocks under real `sleep` / `sleepNanos` / `wait`
    /// frames. Plain data: never written after construction.
    pub(in crate::debug) compatible_blocking_bytecode: bool,
    /// `CRATONVM_DEBUG_VIRTUAL_CONTINUATION_FRAMES`, read once when this
    /// VM's gates are made (interpreter round i1 wave 49, lane L1;
    /// `docs/feature-designs/jvmti-delivery-threading.md`):
    /// a suspended unmounted virtual thread blocked in a sleep or a park
    /// lists HotSpot's default continuation rows (`VirtualThread.yieldContinuation`,
    /// `VirtualThread.parkNanos`, ...) in place of the `BoundVirtualThread`
    /// platform branch that ran (`interpreter::continuation_head_of_unmounted`).
    /// Plain data: never written after construction.
    pub(in crate::debug) virtual_continuation_frames: bool,
    /// `CRATONVM_DEBUG_SUSPEND_HANDSHAKE`, read once when this VM's gates
    /// are made (interpreter round i1 wave 49, lane L1; stage 1 of
    /// `docs/feature-designs/jvmti-delivery-threading.md`):
    /// `VirtualMachine.Suspend`, `ThreadReference.Suspend` and a
    /// `SUSPEND_ALL` event set wait, bounded, for each suspended thread that
    /// was running Java to reach a safe place before the reply or the event
    /// goes out ([`await_suspension_settled`]), as HotSpot's handshake
    /// completes before its reply. Plain data: never written after
    /// construction.
    pub(in crate::debug) suspend_handshake: bool,
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
    pub(in crate::debug) suspend_event_settle: bool,
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
    pub(in crate::debug) invoke_reply_settle: bool,
    /// `CRATONVM_JVMTI_STATE_SUSPENSION_HINT`, read once when this VM's gates
    /// are made (interpreter round i3 wave 72, lane L6; page
    /// `interpreter-i64-L1-proposal-suspended-read-from-a-word-every-suspension-writes-KEPT-OFF-20261021`):
    /// C JVMTI `GetThreadState` reads the VM's suspension hint
    /// (`DebugRealm::suspension_hint`) first and takes the debug-state lock
    /// for the `SUSPENDED` bit only while some thread is suspended. Default
    /// OFF (`truthy_word`). Plain data: never written after construction.
    pub(in crate::debug) state_suspension_hint: bool,
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
    pub(in crate::debug) jvmti_thread_lookup_vm_only: bool,
    /// `CRATONVM_JVMTI_THREAD_STATE_ONE_READ`, read once when this VM's gates
    /// are made (interpreter round i6 wave 132, lane L1; i130-L1b's first
    /// "next step"): the C JVMTI `GetThreadState` reads the thread's block
    /// state, published wait kind and interrupted flag from one registry
    /// `threads` read (`ThreadRegistry::jvmti_state_facts`) instead of three,
    /// without cloning the flag's `Arc`. Performance switch, default ON since
    /// interpreter round i7 (i7-default-flip, A/B w144flip; `=0` off). Plain
    /// data: never written after construction.
    pub(in crate::debug) jvmti_thread_state_one_read: bool,
    /// `CRATONVM_JVMTI_STATE_SUSPEND_WORD`, read once when this VM's gates are
    /// made (interpreter round i7 wave 135, lane L1; i64-L1 item 1, the
    /// i130-L1b "per-thread suspended word" step): the C JVMTI
    /// `GetThreadState` reads the `SUSPENDED` bit from the thread's own
    /// `GcBlockState::jvmti_state_suspended`, which every suspension mutator
    /// rewrites under the debug-state lock once a first ask subscribed the
    /// thread (`DebugState::subscribe_state_suspend_word`), instead of taking
    /// that VM-global lock per call. Performance switch, default OFF
    /// (`truthy_word`). Plain data: never written after construction.
    pub(in crate::debug) jvmti_state_suspend_word: bool,
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
    pub(in crate::debug) suspend_native_ack: bool,
    /// `CRATONVM_DEBUG_MONITOR_EXIT_ON_SUSPEND`, read once when this VM's
    /// gates are made (interpreter round i2 wave 54, lane L6;
    /// `docs/feature-designs/jvmti-delivery-threading.md`):
    /// a thread that wins a contended monitor while the debugger (or a C
    /// JVMTI agent) holds it suspended gives the monitor back and waits for
    /// its resume, still blocked on the entry, as HotSpot's `ExitOnSuspend`
    /// does ([`exit_monitor_while_suspended`]). Off (`=0`): it keeps the
    /// monitor and parks holding it, as before. Plain data: never written
    /// after construction.
    pub(in crate::debug) monitor_exit_on_suspend: bool,
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
    pub(in crate::debug) native_frames_window: bool,
    /// `CRATONVM_DEBUG_NATIVE_WINDOW_CHAIN`, read once when this VM's gates
    /// are made (interpreter round i2 wave 58, lane L6;
    /// `docs/feature-designs/jvmti-delivery-threading.md`):
    /// a native frames window opened under compiled code records a copy of
    /// the thread's JIT entry chain (`conservative_roots::JitChainSnapshot`)
    /// and the reader walks the compiled activations from it, instead of the
    /// thread capturing the whole compiled view as it enters every native.
    /// On by default since wave 58 (lane L6); `=0` restores the capture.
    /// Plain data: never written after construction.
    pub(in crate::debug) native_window_chain: bool,
    /// `CRATONVM_DEBUG_SUSPEND_SETTLE_LONG`, read once when this VM's gates
    /// are made (interpreter round i2 wave 56, lane L6;
    /// `docs/feature-designs/jvmti-delivery-threading.md`):
    /// the suspension's settle wait ([`await_suspension_settled`]) waits up
    /// to [`SUSPEND_SETTLE_LONG_WAIT`] for a running thread on its way to
    /// its suspend point, not [`SUSPEND_POINT_WAIT`]. Plain data: never
    /// written after construction.
    pub(in crate::debug) suspend_settle_long: bool,
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
    pub(in crate::debug) park_condvar: bool,
    /// Notified (`notify_all`) by every [`publish_debugger_gates`] -- which
    /// follows every JDWP command, every C JVMTI suspension or resume and a
    /// detach -- by every task queued for a parked thread and by every
    /// arrival at a suspend point (interpreter round i2 wave 54, lane L6;
    /// [`Self::park_condvar`]). Waited on with the debug-state mutex.
    pub(in crate::debug) state_changed: parking_lot::Condvar,
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
    pub(in crate::debug) thread_suspend_word: bool,
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
    pub(in crate::debug) stw_unparks_parked: bool,
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
    pub(in crate::debug) park_gc_blocked: bool,
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
    pub(in crate::debug) heap_work_outside_region: bool,
    /// `CRATONVM_DEBUG_PARK_REGION_REUSES_DEPOSIT`, read once when this VM's
    /// gates are made (interpreter round i2 wave 66, lane L6;
    /// `docs/feature-designs/jvmti-delivery-threading.md`):
    /// with [`Self::park_gc_blocked`], a park re-entering its region after
    /// heap work or a relisting raises the flag over the deposit its last
    /// leave refreshed, when nothing it holds can have moved
    /// ([`ParkGcRegion`]'s standing key), instead of rebuilding it. Off
    /// (default): every entry deposits in full, as in wave 65. Plain data:
    /// never written after construction.
    pub(in crate::debug) park_region_reuses_deposit: bool,
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
    pub(in crate::debug) park_region_at_first_pause: bool,
    /// `CRATONVM_DEBUG_PARK_LEAVE_KEEPS_UNFOLDED_SNAPSHOT`, read once when
    /// this VM's gates are made (interpreter round i2 wave 68, lane L6;
    /// `docs/feature-designs/jvmti-delivery-threading.md`):
    /// with [`Self::park_region_reuses_deposit`], a park region left to run
    /// heap work ([`ParkGcRegion::leave_for_heap_work`]) keeps the root
    /// snapshot its entry deposited when no collection, fold or
    /// redefinition reached the region, instead of refreshing it from the
    /// frames and auditing them. Off (default): every leave refreshes, as in
    /// wave 66. Plain data: never written after construction.
    pub(in crate::debug) park_leave_keeps_unfolded_snapshot: bool,
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
    pub(in crate::debug) settle_arrival_count: bool,
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
    pub(in crate::debug) settle_pending_counter: bool,
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
    pub(in crate::debug) jdk_breakpoint_expansion_set: bool,
    /// `CRATONVM_JDWP_VISIBLE_CLASSES_INITIATED`, read once when this VM's
    /// gates are made (interpreter round i6 wave 118, lane L1; HotSpot
    /// parity, default ON since the i6 wave-126 flip, `=0` off;
    /// `i41-L1-proposal-one-initiating-loader-record-for-every-loader`):
    /// JDWP `ClassLoaderReference.VisibleClasses` answers the classes the
    /// loader is an initiating loader of (`instrument::initiated_class_ids`,
    /// the `getInitiatedClasses` / JVMTI `GetClassLoaderClasses` record), as
    /// HotSpot's back end answers it from `GetClassLoaderClasses`. Off: every
    /// class whose defining loader is the loader or an ancestor (wave 24).
    pub(in crate::debug) visible_classes_initiated: bool,
    /// `CRATONVM_JVMTI_C_LISTING_SHARED`, read once when this VM's gates are
    /// made (interpreter round i6 wave 119, lane L1; default ON since the i6
    /// wave-126 flip, `=0` off; i102-L1 item 1 / i46-L1 step 2): the C
    /// table's listing of the calling thread for `GetStackTrace`,
    /// `GetFrameCount` and `GetFrameLocation` (`native_env::listed_rows`
    /// without frames) comes from the one builder JDWP uses
    /// (`interpreter::listing_rows_of_current_thread`), as its other-thread
    /// reads already do, instead of its own stack-trace capture. Off: as
    /// before.
    pub(in crate::debug) c_listing_shared: bool,
    /// `CRATONVM_JDWP_INTRINSIC_SHADOW_METHOD_EVENTS`, read once when this
    /// VM's gates are made (interpreter round i6 wave 120, lane L1; default
    /// OFF until `L1W120JdiShadowedMethodEvents` agrees on the host; i40-L1):
    /// a Java method with bytecode that a reviewed `Intrinsic` answers
    /// without running it posts a JDWP `MethodEntry` at index 0 and a
    /// `MethodExit` at its last return, the method listed on top as an
    /// opaque row while the thread is suspended there
    /// (`jvmti_events::report_native_method_entry`), as HotSpot's interpreter
    /// reports the bytecode it runs. Off: no event, as before.
    pub(in crate::debug) intrinsic_shadow_method_events: bool,
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
    pub(in crate::debug) standin_level_locals: bool,
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
    pub(in crate::debug) blocked_dead_locals: bool,
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
    pub(in crate::debug) blocked_rooted_dead_locals: bool,
    /// `CRATONVM_JVMTI_C_AGENT_KEEPS_LOCALS_ALIVE`, read once when this VM's
    /// gates are made (interpreter round i6 wave 132, lane L1; HotSpot parity,
    /// default ON, `=0` off; i126-L1): a startup C agent that acquired
    /// `can_access_local_variables` in `Agent_OnLoad` makes every local of
    /// this VM a GC root, as a JDWP port does
    /// ([`Self::c_agent_observes_locals`], `jvmti::debugger_observes_locals`).
    pub(in crate::debug) c_agent_keeps_locals_alive: bool,
    /// Set once, by `native_env::bind_startup_envs` in `Vm::new` (before any
    /// Java frame exists), when a startup env holds `can_access_local_variables`
    /// and [`Self::c_agent_keeps_locals_alive`] is on (interpreter round i6
    /// wave 132, lane L1). Never cleared: HotSpot's capability is OnLoad-only
    /// and keeps every local alive for the VM's life
    /// (`JvmtiExport::can_access_local_variables`); a slot the VM stopped
    /// rooting could not be rooted again soundly.
    pub(in crate::debug) c_agent_observes_locals: std::sync::atomic::AtomicBool,
    /// t1 wave 2, lane M: set once, by `native_env::bind_startup_envs` in
    /// `Vm::new` (before any Java frame exists and before any compile), when
    /// a startup env acquired `can_get_owned_monitor_info`,
    /// `can_get_owned_monitor_stack_depth_info` or `can_access_local_variables`
    /// (all OnLoad-only). HotSpot's C2 annotates its safepoints for the escape
    /// barrier exactly then (`ConnectionGraph::compute_escape`), so an
    /// eliminated lock is relocked and listed by the owned-monitor answers;
    /// the JIT keeps such locks instead (`jvmti::monitor_observer_may_exist`,
    /// `CRATONVM_JIT_EA_MONITOR_AGENT_KEEPS_LOCKS`). Never cleared.
    pub(in crate::debug) c_agent_observes_monitors: std::sync::atomic::AtomicBool,
    /// t1 wave 3, lane M: set when a C agent env of this VM acquired
    /// `can_tag_objects` (by `native_env::bind_startup_envs` for a startup
    /// env, by `AddCapabilities` in the live phase). HotSpot's
    /// `JvmtiExport::can_walk_any_space`: C2 then annotates its safepoints so
    /// heap walks reallocate scalar-replaced objects; the JIT replaces none
    /// instead (`jvmti::object_observer_may_exist`). Never cleared (a
    /// relinquished capability leaves the conservative answer). Bodies
    /// compiled before a live-phase acquisition keep their replacement, as
    /// HotSpot's unannotated older frames do.
    pub(in crate::debug) c_agent_walks_heap: std::sync::atomic::AtomicBool,
    /// Arrivals and other changes of the debug state the suspension's waits
    /// look at: bumped by every [`Self::notify_state_changed`] -- every
    /// publication of these gates, every arrival at a suspend point, every
    /// queued task -- with [`Self::settle_arrival_count`] on (wave 64, lane
    /// L6). Per VM, as HotSpot's handshake counts its arrivals on the
    /// operation. Only compared for change, never for a value.
    pub(in crate::debug) arrivals: std::sync::atomic::AtomicU64,
    /// `CRATONVM_JVMTI_C_EXCEPTION_CATCH`, read once when this VM's gates
    /// are made (interpreter round i1 wave 52, lane L1; the last item of
    /// `docs/feature-designs/jvmti-delivery-threading.md`):
    /// the C JVMTI table delivers `ExceptionCatch`, with the caught object
    /// (`jvmti::native_env::deliver_exception_catch`); off (`=0`), enabling
    /// it answers `JVMTI_ERROR_NOT_AVAILABLE` as before. On by default. Plain
    /// data: never written after construction.
    pub(in crate::debug) c_exception_catch: bool,
    /// `CRATONVM_JVMTI_C_CLASS_HOOKS`, read once when this VM's gates are
    /// made (interpreter round i2 wave 53, lane L1;
    /// `docs/feature-designs/jvmti-delivery-threading.md`):
    /// the C JVMTI table delivers `ClassFileLoadHook` and `ClassLoad`; off
    /// (`=0`), enabling either answers `JVMTI_ERROR_NOT_AVAILABLE` as before.
    /// On by default. Plain
    /// data: never written after construction.
    pub(in crate::debug) c_class_hooks: bool,
    /// `CRATONVM_JVMTI_C_RETRANSFORM`, read once when this VM's gates are
    /// made ([`c_retransform_switch`]; interpreter round i2 wave 54, lane L1;
    /// `docs/feature-designs/jvmti-delivery-threading.md`):
    /// the C JVMTI table grants `can_retransform_classes`, answers
    /// `RetransformClasses` and posts `ClassFileLoadHook` at a
    /// retransformation; off (`=0`), the capability is not potential, as before. On by default.
    /// Plain data: never written after construction.
    pub(in crate::debug) c_retransform: bool,
    /// `CRATONVM_JVMTI_C_REDEFINE`, read once when this VM's gates are made
    /// ([`c_redefine_switch`]; interpreter round i2 wave 57, lane L1;
    /// `docs/feature-designs/jvmti-delivery-threading.md`):
    /// the C JVMTI table grants `can_redefine_classes` and answers
    /// `RedefineClasses` (`runtime::instrument::redefine_classes_for_jvmti`);
    /// off (`=0`), the capability is not potential, as before. On by default
    /// (wave 57's last commit). Plain data: never written after construction.
    pub(in crate::debug) c_redefine: bool,
    /// `CRATONVM_JVMTI_C_IN_NATIVE`, read once when this VM's gates are made
    /// (interpreter round i2 wave 58, lane L1;
    /// `docs/feature-designs/jvmti-delivery-threading.md`):
    /// with a C JVMTI env bound, a thread running a JNI native's C code
    /// publishes it (`GcBlockState::jvmti_in_native`) and the C table's
    /// `GetThreadState` answers `JVMTI_THREAD_STATE_IN_NATIVE` for it; off
    /// (`=0`), never, as before. On by default (wave 58's last commit). Plain
    /// data: never written after construction.
    pub(in crate::debug) c_in_native: bool,
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
    pub(in crate::debug) suspend_at_native_transition: std::sync::atomic::AtomicBool,
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
    pub(in crate::debug) c_frame_control: bool,
    /// `CRATONVM_JVMTI_C_CALLBACK_FRAME_CONTROL`, read once when this VM's
    /// gates are made (interpreter round i2 wave 62, lane L1; stage 1 of
    /// `docs/feature-designs/jvmti-delivery-threading.md`):
    /// with [`Self::c_frame_control`] on, a C agent's `PopFrame` and
    /// `ForceEarlyReturn*` of the calling thread made inside its
    /// `Breakpoint` or `SingleStep` callback act on the Java frame the event
    /// is about, taken as the callback returns, as HotSpot takes them; off,
    /// `OPAQUE_FRAME`, as in wave 61 (`=0`). On by default (wave 62's
    /// switch-on commit). Plain data: never written after construction.
    pub(in crate::debug) c_callback_frame_control: bool,
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
    pub(in crate::debug) c_running_native: bool,
    /// Who needs the native-call funnel to record the native a thread runs
    /// (wave 61, lane L6): [`RUNNING_NATIVE_SESSION`] while a debugger is
    /// attached, [`RUNNING_NATIVE_C_ENV`] once a C JVMTI env is bound with
    /// [`Self::c_running_native`] on. One load in the funnel
    /// ([`Self::running_native_armed`]), in place of the session flag's.
    /// Wave 133 (lane L1): [`RUNNING_NATIVE_C_LEAF_ONLY`] in place of
    /// [`RUNNING_NATIVE_C_ENV`] when [`Self::c_funnel_record_from_excuse`]
    /// is on and the VM's excuse word carries the callback.
    pub(in crate::debug) running_native_record: std::sync::atomic::AtomicU8,
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
    pub(in crate::debug) c_funnel_record_from_excuse: bool,
    /// `CRATONVM_JVMTI_C_START_PHASE_HOOK`, read once when this VM's gates
    /// are made (wave 54, lane L1): a C agent's `ClassFileLoadHook` is armed
    /// in JVMTI's start phase too, for the classes the bootstrap loader
    /// defines from `System.initPhase1` on
    /// (`runtime::instrument::start_phase_withholds`); off, from the live
    /// phase only, as in wave 53 (`=0`). On by default. Plain data: never written after
    /// construction.
    pub(in crate::debug) c_start_phase_hook: bool,
    /// `CRATONVM_JVMTI_BRIDGE_LISTENER_GATE`, read once when this VM's gates
    /// are made (wave 54, lane L1; option 1 of
    /// `docs/feature-designs/jvmti-delivery-threading.md`):
    /// the D14 bridge (`runtime::jvmti`) forwards an event into the older
    /// `jvmti::EventManager` only while that manager has it enabled
    /// ([`Self::legacy_jvmti_listens`]); off, every bridged event takes the
    /// VM's JVMTI lock and builds its record for no one, as before (`=0`). On by default. Plain
    /// data: never written after construction.
    pub(in crate::debug) bridge_listener_gate: bool,
    /// The older `jvmti::EventManager`'s enabled events
    /// (`EventManager::enabled_mask_handle`, shared when the VM is made;
    /// wave 54), read by [`Self::legacy_jvmti_listens`] without the VM's
    /// JVMTI lock.
    pub(in crate::debug) legacy_jvmti_events: std::sync::Arc<std::sync::atomic::AtomicU64>,
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
    true
}

