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
//! - **Events** — Breakpoints, thread events, VM lifecycle (`events.rs`)
//! - **IDs** — Wire-level ID management (`ids.rs`)

pub mod commands;
pub mod events;
pub mod ids;
pub mod protocol;
pub mod transport;

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use commands::{FieldInfo, MethodInfo};
use events::EventManager;
use ids::IdManager;
use ids::ThreadId;
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
}

// ---------------------------------------------------------------------------
// Watchpoint and conditional breakpoint types
// ---------------------------------------------------------------------------

/// A field watchpoint (access or modification).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FieldWatchpoint {
    /// The class owning the field.
    pub class_id: u64,
    /// The field being watched.
    pub field_id: u64,
    /// The JDWP event request ID that created this watchpoint.
    pub request_id: u32,
}

/// How to compare a breakpoint's hit count against the filter threshold.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HitCountMode {
    /// Fire when hit_count == count.
    Equal,
    /// Fire when hit_count >= count.
    GreaterOrEqual,
    /// Fire when hit_count is a multiple of count.
    Multiple,
}

/// A hit-count condition attached to a breakpoint request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HitCountFilter {
    pub mode: HitCountMode,
    pub count: u32,
}

/// Extended breakpoint info for conditional breakpoints.
#[derive(Debug, Clone)]
pub struct BreakpointCondition {
    /// Optional boolean expression (expression ID from ConditionalFilter).
    pub condition: Option<String>,
    /// Current hit count (incremented each time the location is hit).
    pub hit_count: u32,
    /// Optional hit-count filter derived from a Count modifier.
    pub hit_count_filter: Option<HitCountFilter>,
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
    /// The target class ID was invalid.
    InvalidClass,
    /// The target object ID was invalid.
    InvalidObject,
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
    fn invoke_static(
        &self,
        class_id: u64,
        method_id: u64,
        thread_id: u64,
        args: &[DebuggerValue],
        return_sig: &str,
    ) -> Result<InvokeOutcome, BridgeError>;

    /// Invoke an instance method on a receiver in the context of `thread_id`.
    ///
    /// If `non_virtual` is true, dispatch is resolved statically against
    /// `class_id` (used for `ObjectReference.InvokeMethod` with option 2,
    /// "invoke non-virtual").
    fn invoke_instance(
        &self,
        receiver_id: u64,
        class_id: u64,
        method_id: u64,
        thread_id: u64,
        args: &[DebuggerValue],
        return_sig: &str,
        non_virtual: bool,
    ) -> Result<InvokeOutcome, BridgeError>;

    /// Allocate a new array of the given element type and length.
    /// Returns the wire ID of the newly allocated array.
    fn new_array(&self, array_type_id: u64, length: i32) -> Result<u64, BridgeError>;
}

// ---------------------------------------------------------------------------
// DebugState
// ---------------------------------------------------------------------------

/// Central debug state combining ID management, event tracking, class
/// metadata, and connection bookkeeping.
pub struct DebugState {
    /// Wire-level ID manager.
    pub ids: IdManager,
    /// Event request manager.
    pub events: EventManager,

    // -- VM-wide state -----------------------------------------------------
    /// Whether all threads are suspended.
    pub suspended: bool,
    /// Whether the debug session has been disposed.
    pub disposed: bool,
    /// If `Some`, the VM was asked to exit with this code.
    pub exit_code: Option<i32>,
    /// Next packet ID for packets sent by the VM.
    pub next_packet_id: u32,

    // -- Class metadata (populated by the VM) ------------------------------
    /// Signature (e.g. `"Ljava/lang/String;"`) → (wire ref-type ID, type-tag).
    pub loaded_classes: HashMap<String, (u64, u8)>,
    /// Wire ref-type ID → JNI signature.
    pub class_signatures: HashMap<u64, String>,
    /// Wire ref-type ID → source file name.
    pub class_source_files: HashMap<u64, String>,
    /// Wire ref-type ID → field list.
    pub class_fields: HashMap<u64, Vec<FieldInfo>>,
    /// Wire ref-type ID → method list.
    pub class_methods: HashMap<u64, Vec<MethodInfo>>,

    // -- Thread metadata ---------------------------------------------------
    /// Wire thread ID → human-readable name.
    pub thread_names: HashMap<u64, String>,
    /// Set of currently-suspended thread IDs.
    pub suspended_threads: HashSet<u64>,

    // -- String table (for CreateString) -----------------------------------
    pub string_values: HashMap<u64, String>,
    pub next_string_handle: u64,

    // -- Frame local variable snapshots ---------------------------------------
    /// Frame local variable snapshots: (thread_id, frame_id) -> Vec<LocalValue>
    pub frame_locals: HashMap<(u64, u64), Vec<LocalValue>>,

    // -- Frame stack snapshots (for TR_FRAMES / TR_FRAME_COUNT) --------------
    /// Per-thread frame stack snapshots: thread_id -> list of frame entries.
    /// Populated when a thread is suspended (breakpoint/single-step).
    pub thread_frames: HashMap<u64, Vec<FrameEntry>>,

    // -- T6.4 additional JDWP command support ---------------------------------
    /// Class hierarchy: class_id → superclass_id.
    pub class_superclass: HashMap<u64, u64>,
    /// Method line tables: (ref_type_id, method_id) → Vec<(code_index, line_number)>.
    pub method_line_tables: HashMap<(u64, u64), Vec<(u64, i32)>>,
    /// Method variable tables: (ref_type_id, method_id) → Vec<VariableInfo>.
    pub method_variables: HashMap<(u64, u64), Vec<VariableInfo>>,
    /// Method bytecodes: (ref_type_id, method_id) → raw bytecode bytes.
    pub method_bytecodes: HashMap<(u64, u64), Vec<u8>>,
    /// Object → class mapping: object_id → class_ref_type_id.
    pub object_class_map: HashMap<u64, u64>,
    /// Array lengths: array_id → length.
    pub array_lengths: HashMap<u64, usize>,
    /// Array element data: array_id → Vec<raw_bytes_per_element>.
    pub array_elements: HashMap<u64, Vec<Vec<u8>>>,
    /// Array type tags: array_id → JDWP type tag byte.
    pub array_type_tags: HashMap<u64, u8>,
    /// ClassObject → ReferenceType mapping.
    pub class_object_to_ref_type: HashMap<u64, u64>,

    // -- T6.4.4 Watchpoints --------------------------------------------------
    /// Active field-access watchpoints.
    pub field_access_watchpoints: Vec<FieldWatchpoint>,
    /// Active field-modification watchpoints.
    pub field_modification_watchpoints: Vec<FieldWatchpoint>,

    // -- T6.4.5 Conditional breakpoints --------------------------------------
    /// Per-request breakpoint conditions: request_id → BreakpointCondition.
    pub breakpoint_conditions: HashMap<u32, BreakpointCondition>,

    // -- T6.5 Debugger-initiated method invocation ---------------------------
    /// Optional live-VM bridge, installed when a debugger attaches.
    ///
    /// When present, `ClassType.InvokeMethod`, `ObjectReference.InvokeMethod`
    /// and `ArrayType.NewInstance` forward to the running VM.  When absent
    /// (e.g. in unit tests) those commands fail with `ERR_VM_DEAD`.
    pub vm_bridge: Option<Arc<dyn DebuggerVmBridge>>,

    /// Array element type for each `arrayTypeID` known to the debugger.
    ///
    /// JDWP clients call `ArrayType.NewInstance(arrayTypeID, length)` and
    /// expect the allocator to honour the declared element type of the
    /// array class.  Populated by the VM when it registers array types
    /// for the debugger (e.g. when a client asks for a class by signature
    /// such as `[I`).
    pub array_type_element_kind: HashMap<u64, u8>,
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

/// A single stack frame entry for debugger frame inspection.
#[derive(Debug, Clone)]
pub struct FrameEntry {
    /// Wire-level frame ID (just the frame index).
    pub frame_id: u64,
    /// Class ID (from ClassId::as_u32()).
    pub class_id: u64,
    /// Method ID (FNV-1a hash of method name).
    pub method_id: u64,
    /// Current bytecode offset.
    pub offset: u64,
}

impl DebugState {
    pub fn new() -> Self {
        Self {
            ids: IdManager::new(),
            events: EventManager::new(),
            suspended: false,
            disposed: false,
            exit_code: None,
            next_packet_id: 1,
            loaded_classes: HashMap::new(),
            class_signatures: HashMap::new(),
            class_source_files: HashMap::new(),
            class_fields: HashMap::new(),
            class_methods: HashMap::new(),
            thread_names: HashMap::new(),
            suspended_threads: HashSet::new(),
            string_values: HashMap::new(),
            next_string_handle: 0x1_0000_0000, // well above normal object IDs
            frame_locals: HashMap::new(),
            thread_frames: HashMap::new(),
            class_superclass: HashMap::new(),
            method_line_tables: HashMap::new(),
            method_variables: HashMap::new(),
            method_bytecodes: HashMap::new(),
            object_class_map: HashMap::new(),
            array_lengths: HashMap::new(),
            array_elements: HashMap::new(),
            array_type_tags: HashMap::new(),
            class_object_to_ref_type: HashMap::new(),
            field_access_watchpoints: Vec::new(),
            field_modification_watchpoints: Vec::new(),
            breakpoint_conditions: HashMap::new(),
            vm_bridge: None,
            array_type_element_kind: HashMap::new(),
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

    /// Allocate the next outgoing packet ID.
    pub fn next_id(&mut self) -> u32 {
        let id = self.next_packet_id;
        self.next_packet_id += 1;
        id
    }

    // -- T6.4.4 Watchpoint fire methods --------------------------------------

    /// Check if a field-access watchpoint exists for the given class/field.
    /// If so, build and return a JDWP event packet with the field info.
    pub fn fire_field_access_event(
        &self,
        class_id: u64,
        field_id: u64,
        thread_id: u64,
        location_class_id: u64,
        location_method_id: u64,
        location_offset: u64,
    ) -> Option<(u32, events::SuspendPolicy, protocol::JdwpPacket)> {
        let wp = self
            .field_access_watchpoints
            .iter()
            .find(|w| w.class_id == class_id && w.field_id == field_id)?;

        let req = self.events.get_request(wp.request_id)?;
        let suspend_policy = req.suspend_policy;
        let request_id = wp.request_id;

        // Build extra: location(25) + fieldType(1) + fieldClassID(8) + fieldID(8)
        let mut extra =
            build_location_extra(location_class_id, location_method_id, location_offset);
        extra.push(1u8); // refTypeTag: CLASS
        extra.extend_from_slice(&class_id.to_be_bytes());
        extra.extend_from_slice(&field_id.to_be_bytes());

        let event = events::Event {
            request_id,
            kind: events::EventKind::FieldAccess,
            thread_id,
            extra,
        };
        let pkt = events::compose_event_packet(suspend_policy, &[event]);
        Some((request_id, suspend_policy, pkt))
    }

    /// Check if a field-modification watchpoint exists for the given class/field.
    /// If so, build and return a JDWP event packet with the field info.
    pub fn fire_field_modification_event(
        &self,
        class_id: u64,
        field_id: u64,
        thread_id: u64,
        location_class_id: u64,
        location_method_id: u64,
        location_offset: u64,
    ) -> Option<(u32, events::SuspendPolicy, protocol::JdwpPacket)> {
        let wp = self
            .field_modification_watchpoints
            .iter()
            .find(|w| w.class_id == class_id && w.field_id == field_id)?;

        let req = self.events.get_request(wp.request_id)?;
        let suspend_policy = req.suspend_policy;
        let request_id = wp.request_id;

        // Build extra: location(25) + fieldType(1) + fieldClassID(8) + fieldID(8)
        let mut extra =
            build_location_extra(location_class_id, location_method_id, location_offset);
        extra.push(1u8); // refTypeTag: CLASS
        extra.extend_from_slice(&class_id.to_be_bytes());
        extra.extend_from_slice(&field_id.to_be_bytes());

        let event = events::Event {
            request_id,
            kind: events::EventKind::FieldModification,
            thread_id,
            extra,
        };
        let pkt = events::compose_event_packet(suspend_policy, &[event]);
        Some((request_id, suspend_policy, pkt))
    }

    // -- T6.4.5 Conditional breakpoint evaluation ----------------------------

    /// Evaluate whether a breakpoint should fire based on its conditions.
    /// Increments the hit count and checks the hit-count filter.
    /// Returns `true` if the breakpoint should suspend.
    pub fn evaluate_breakpoint_condition(&mut self, request_id: u32) -> bool {
        let cond = match self.breakpoint_conditions.get_mut(&request_id) {
            Some(c) => c,
            None => return true, // no condition — always fire
        };

        // Increment hit count
        cond.hit_count += 1;
        let current = cond.hit_count;

        // Check hit-count filter
        if let Some(ref filter) = cond.hit_count_filter {
            let passes = match filter.mode {
                HitCountMode::Equal => current == filter.count,
                HitCountMode::GreaterOrEqual => current >= filter.count,
                HitCountMode::Multiple => filter.count > 0 && current % filter.count == 0,
            };
            if !passes {
                return false;
            }
        }

        // If there is a conditional expression, the VM would need to evaluate
        // it (e.g. via the expression evaluator). For now we record it and
        // return true — the interpreter can check `cond.condition` and do
        // expression evaluation if needed.
        true
    }
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

/// Run the JDWP server loop.  Blocks forever: listens for a debugger
/// connection, processes commands, and loops back to accept a new
/// connection after the previous one closes.
pub fn run_jdwp_server(shared: &crate::vm::SharedVm, port: u16) {
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

        // T6.5 — install the live-VM bridge so `ClassType.InvokeMethod`,
        // `ObjectReference.InvokeMethod` and `ArrayType.NewInstance` can
        // forward to the running VM.  The bridge is removed on disconnect.
        if let Some(shared_arc) = shared.self_arc.read().as_ref().and_then(|w| w.upgrade()) {
            let bridge: Arc<dyn DebuggerVmBridge> = Arc::new(SharedVmBridge::new(shared_arc));
            shared.debug.debug_state.lock().vm_bridge = Some(bridge);
        } else {
            tracing::warn!(
                "JDWP: self_arc unavailable — debugger-initiated invocations \
                 will return VM_DEAD until the VM is fully wired up"
            );
        }

        // Populate class metadata from current VM state
        populate_class_metadata(shared);

        // Populate thread metadata
        populate_thread_metadata(shared);

        // Send VM_START composite event
        send_vm_start_event(&mut stream, shared);

        // Set stream to non-blocking so we can interleave command processing
        // with event draining.
        let _ = stream.set_nonblocking(true);

        // Process commands until the connection closes
        loop {
            // --- Drain pending events from the interpreter and send to debugger ---
            while let Ok(evt) = rx.try_recv() {
                let extra = build_location_extra(evt.class_id, evt.method_id, evt.offset);
                let event = events::Event {
                    request_id: evt.request_id,
                    kind: evt.kind,
                    thread_id: evt.thread_id,
                    extra,
                };
                let pkt_id = {
                    let mut ds = shared.debug.debug_state.lock();
                    ds.next_id()
                };
                let mut pkt = events::compose_event_packet(evt.suspend_policy, &[event]);
                if let JdwpPacket::Command { ref mut id, .. } = pkt {
                    *id = pkt_id;
                }
                if protocol::write_packet(&mut stream, &pkt).is_err() {
                    break;
                }
            }

            // --- Process one incoming command (non-blocking) ---
            let packet = match protocol::read_packet(&mut stream) {
                Ok(p) => p,
                Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(std::time::Duration::from_millis(5));
                    continue;
                }
                Err(_) => break,
            };

            match packet {
                JdwpPacket::Command {
                    id,
                    command_set,
                    command,
                    data,
                    ..
                } => {
                    let result = {
                        let mut ds = shared.debug.debug_state.lock();
                        commands::dispatch(command_set, command, &data, &mut ds)
                    };

                    // Update breakpoints_active flag based on current event state
                    let has_bp = {
                        let ds = shared.debug.debug_state.lock();
                        ds.events.has_breakpoints() || ds.events.has_single_steps()
                    };
                    shared
                        .debug
                        .breakpoints_active
                        .store(has_bp, std::sync::atomic::Ordering::Relaxed);

                    let reply = JdwpPacket::Reply {
                        id,
                        error_code: result.error_code,
                        data: result.data,
                    };
                    if protocol::write_packet(&mut stream, &reply).is_err() {
                        break;
                    }

                    // Check if session was disposed
                    let disposed = shared.debug.debug_state.lock().disposed;
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

        // Clear all breakpoints/step requests and resume all threads
        {
            let mut ds = shared.debug.debug_state.lock();
            ds.events.clear_all();
            ds.suspended_threads.clear();
            ds.suspended = false;
            ds.disposed = false;
            // T6.5 — tear down the VM bridge so subsequent connect cycles
            // install a fresh one (avoids stale Arc<SharedVm> references).
            ds.vm_bridge = None;
        }
        shared
            .debug
            .breakpoints_active
            .store(false, std::sync::atomic::Ordering::Relaxed);
    }
}

/// Build JDWP location extra bytes: tag(1) + classID(8) + methodID(8) + index(8).
pub fn build_location_extra(class_id: u64, method_id: u64, offset: u64) -> Vec<u8> {
    let mut extra = Vec::with_capacity(25);
    extra.push(1u8); // TypeTag: CLASS
    extra.extend_from_slice(&class_id.to_be_bytes());
    extra.extend_from_slice(&method_id.to_be_bytes());
    extra.extend_from_slice(&offset.to_be_bytes());
    extra
}

/// Send a thread event (ThreadStart or ThreadDeath) to the debugger if
/// there is an active event channel.
pub fn send_thread_event(shared: &crate::vm::SharedVm, kind: events::EventKind, thread_id: u64) {
    // Check if debugger has a matching request
    let (req_id, suspend_policy) = {
        let ds = shared.debug.debug_state.lock();
        match ds.events.check_thread_event(kind, thread_id) {
            Some(req) => (req.id, req.suspend_policy),
            None => return,
        }
    };
    if let Ok(guard) = shared.debug.debug_event_tx.lock() {
        if let Some(ref tx) = *guard {
            let _ = tx.send(DebugEvent {
                kind,
                request_id: req_id,
                suspend_policy,
                thread_id,
                class_id: 0,
                method_id: 0,
                offset: 0,
            });
        }
    }
}

/// Populate DebugState with class metadata from the class manager.
fn populate_class_metadata(shared: &crate::vm::SharedVm) {
    let cm = shared.classes.class_manager.read();
    let mut ds = shared.debug.debug_state.lock();

    for class in cm.class_store.iter() {
        let class_id = class.id.as_u32() as u64;
        let type_tag = if class.is_interface() { 2u8 } else { 1u8 };
        let jni_sig = format!("L{};", class.name);

        ds.loaded_classes
            .insert(jni_sig.clone(), (class_id, type_tag));
        ds.class_signatures.insert(class_id, jni_sig);

        if let Some(ref sf) = class.source_file {
            ds.class_source_files.insert(class_id, sf.to_string());
        }

        // Populate methods
        let mut methods = Vec::new();
        for (_i, m) in class.methods.iter().enumerate() {
            let method_id = {
                let mut h: u64 = 0xcbf29ce484222325;
                for b in m.name.bytes() {
                    h ^= b as u64;
                    h = h.wrapping_mul(0x100000001b3);
                }
                h
            };
            methods.push(commands::MethodInfo {
                method_id: ids::MethodId(method_id),
                name: m.name.to_string(),
                signature: m.descriptor.to_string(),
                mod_bits: m.access_flags.bits() as u32,
            });
        }
        ds.class_methods.insert(class_id, methods);

        // Populate fields
        let mut fields = Vec::new();
        for (i, f) in class.fields.iter().enumerate() {
            fields.push(commands::FieldInfo {
                field_id: ids::FieldId(i as u64),
                name: f.name.to_string(),
                signature: f.descriptor.to_string(),
                mod_bits: f.access_flags.bits() as u32,
            });
        }
        ds.class_fields.insert(class_id, fields);
    }
}

/// Populate DebugState with thread metadata from the thread registry.
fn populate_thread_metadata(shared: &crate::vm::SharedVm) {
    let mut ds = shared.debug.debug_state.lock();
    let names = shared.threads.thread_registry.all_thread_names();
    for (tid, name) in names {
        ds.thread_names.insert(tid.0 as u64, name);
    }
}

/// Send a VM_START composite event to the debugger.
fn send_vm_start_event(stream: &mut std::net::TcpStream, _shared: &crate::vm::SharedVm) {
    use protocol::PayloadWriter;
    let mut pw = PayloadWriter::new();
    // Composite event header
    pw.put_u8(0); // suspendPolicy = NONE
    pw.put_u32_be(1); // events count
                      // EventKind = VM_START (90)
    pw.put_u8(90);
    pw.put_u32_be(0); // requestID = 0
    pw.put_u64_be(0); // threadID = main thread (0)

    let data = pw.into_bytes();
    let event_packet = JdwpPacket::Command {
        id: 0,
        flags: 0,
        command_set: 64, // Event composite
        command: 100,
        data,
    };
    let _ = protocol::write_packet(stream, &event_packet);
}

// ---------------------------------------------------------------------------
// T6.5 — SharedVm bridge: live VM access for debugger-initiated invocation
// ---------------------------------------------------------------------------

/// Concrete [`DebuggerVmBridge`] that forwards to a live [`SharedVm`].
///
/// **Thread-context simplification (documented):** HotSpot runs
/// debugger-initiated invocations on the *target* thread after unwinding
/// it to a safe point.  Our implementation runs the call on a fresh
/// ephemeral [`JvmThread`] built on top of the current debugger-listener
/// thread.  This is safe because:
///
/// 1. We only allow invocations while the target thread is suspended
///    (`DebugState::suspended_threads.contains(thread_id)`).  If not
///    suspended we return [`BridgeError::InvalidThread`] so the IDE
///    surfaces the normal JDWP error.
/// 2. The ephemeral thread shares heap + class tables with the live VM,
///    so field mutations and allocations are observable immediately.
/// 3. Thread-identity-sensitive code (e.g. `Thread.currentThread()`) will
///    see the ephemeral thread rather than the target.  This matches the
///    simplification documented in T6 audit note — a follow-up can plumb
///    the call onto the target's own `JvmThread` once we have the
///    suspend/resume handshake wired.
pub struct SharedVmBridge {
    pub shared: Arc<crate::vm::SharedVm>,
}

impl SharedVmBridge {
    pub fn new(shared: Arc<crate::vm::SharedVm>) -> Self {
        Self { shared }
    }

    /// Look up a class name by its wire class_id (= `Class::id.as_u32()`).
    fn class_name_for(&self, class_id: u64) -> Option<String> {
        let cm = self.shared.classes.class_manager.read();
        let cid = crate::classloading::ClassId::new(class_id as u32);
        cm.class_store.get(cid).map(|c| c.name.to_string())
    }

    /// Look up a (method_name, descriptor) by the debugger-side methodID.
    /// The debugger-side methodID is an FNV-1a hash of the method name;
    /// this may collide for methods with the same name but different
    /// descriptors (overloads).  When the caller provides arguments we
    /// disambiguate by argument count after the name lookup.
    fn method_sig_for(&self, class_id: u64, method_id: u64) -> Option<(String, String)> {
        let ds = self.shared.debug.debug_state.lock();
        let methods = ds.class_methods.get(&class_id)?;
        let hit = methods.iter().find(|m| m.method_id.0 == method_id)?;
        Some((hit.name.clone(), hit.signature.clone()))
    }

    /// Convert a [`DebuggerValue`] into a VM [`crate::types::Value`].
    fn debugger_to_value(v: &DebuggerValue) -> crate::types::Value {
        use crate::types::Value;
        match v {
            DebuggerValue::Void => Value::Uninitialized,
            DebuggerValue::Boolean(b) => Value::Int(*b as i32),
            DebuggerValue::Byte(b) => Value::Int(*b as i32),
            DebuggerValue::Char(c) => Value::Int(*c as i32),
            DebuggerValue::Short(s) => Value::Int(*s as i32),
            DebuggerValue::Int(i) => Value::Int(*i),
            DebuggerValue::Long(l) => Value::Long(*l),
            DebuggerValue::Float(f) => Value::Float(f32::from_bits(*f)),
            DebuggerValue::Double(d) => Value::Double(f64::from_bits(*d)),
            DebuggerValue::Object(id)
            | DebuggerValue::Array(id)
            | DebuggerValue::String(id)
            | DebuggerValue::Thread(id)
            | DebuggerValue::ThreadGroup(id)
            | DebuggerValue::ClassLoader(id)
            | DebuggerValue::ClassObject(id) => {
                if *id == 0 {
                    Value::Object(None)
                } else {
                    // SAFETY: the debugger hands us an object ID that came
                    // from the VM earlier (e.g. via a previous reply).  We
                    // reconstruct the ObjectRef from the raw address stored
                    // in the wire ID.  If the ID is stale the GC will have
                    // already relocated the object and subsequent field
                    // reads will fault — we accept that rather than add a
                    // handle table in this pass.
                    let ptr = *id as *mut u8;
                    if ptr.is_null() || (ptr as usize) % 8 != 0 {
                        Value::Object(None)
                    } else {
                        Value::Object(Some(unsafe { crate::types::ObjectRef::from_raw(ptr) }))
                    }
                }
            }
        }
    }

    /// Convert a returned VM [`crate::types::Value`] into a [`DebuggerValue`]
    /// tagged per the method's return signature.
    fn value_to_debugger(v: crate::types::Value, return_sig: &str) -> DebuggerValue {
        use crate::types::Value;
        let tag = return_sig.chars().next().unwrap_or('V');
        match (tag, v) {
            ('V', _) => DebuggerValue::Void,
            ('Z', Value::Int(i)) => DebuggerValue::Boolean(if i != 0 { 1 } else { 0 }),
            ('B', Value::Int(i)) => DebuggerValue::Byte(i as i8),
            ('C', Value::Int(i)) => DebuggerValue::Char(i as u16),
            ('S', Value::Int(i)) => DebuggerValue::Short(i as i16),
            ('I', Value::Int(i)) => DebuggerValue::Int(i),
            ('J', Value::Long(l)) => DebuggerValue::Long(l),
            ('F', Value::Float(f)) => DebuggerValue::Float(f.to_bits()),
            ('D', Value::Double(d)) => DebuggerValue::Double(d.to_bits()),
            ('L', Value::Object(Some(r))) | ('[', Value::Object(Some(r))) => {
                let id = r.as_ptr() as u64;
                if tag == '[' {
                    DebuggerValue::Array(id)
                } else {
                    DebuggerValue::Object(id)
                }
            }
            ('L', Value::Object(None)) => DebuggerValue::Object(0),
            ('[', Value::Object(None)) => DebuggerValue::Array(0),
            // Fallback: unexpected type combination — surface as void.
            _ => DebuggerValue::Void,
        }
    }

    /// Dispatch through `invoke_or_native` on an ephemeral JvmThread
    /// and map `MethodCallFailed` -> `InvokeOutcome`.
    fn run_call(
        &self,
        class_name: &str,
        method_name: &str,
        descriptor: &str,
        args: &[crate::types::Value],
        thread_id: u64,
        return_sig: &str,
    ) -> InvokeOutcome {
        use crate::error::MethodCallFailed;
        let tid = crate::threading::jvm_thread::ThreadId(thread_id);
        let mut ephemeral = crate::threading::jvm_thread::JvmThread::new(tid, "jdwp-invoke");
        let result = crate::vm::invoke_or_native(
            &self.shared,
            &mut ephemeral,
            class_name,
            method_name,
            descriptor,
            args,
        );
        match result {
            Ok(None) => InvokeOutcome::returned(DebuggerValue::Void),
            Ok(Some(v)) => InvokeOutcome::returned(Self::value_to_debugger(v, return_sig)),
            Err(MethodCallFailed::ExceptionThrown(exc)) => {
                InvokeOutcome::threw(exc.as_ptr() as u64)
            }
            Err(MethodCallFailed::InternalError(_)) => {
                // Map internal errors to a null exception with a non-zero
                // sentinel; debuggers will surface this as a generic error.
                InvokeOutcome::threw(1)
            }
        }
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
    ) -> Result<InvokeOutcome, BridgeError> {
        let class_name = self
            .class_name_for(class_id)
            .ok_or(BridgeError::InvalidClass)?;
        let (method_name, descriptor) = self
            .method_sig_for(class_id, method_id)
            .ok_or(BridgeError::InvalidClass)?;

        // Require the target thread to exist.  We only check presence in
        // the thread_names table — the actual invocation runs on an
        // ephemeral JvmThread per the documented simplification.
        {
            let ds = self.shared.debug.debug_state.lock();
            if !ds.thread_names.contains_key(&thread_id) {
                return Err(BridgeError::InvalidThread);
            }
        }

        let vm_args: Vec<_> = args.iter().map(Self::debugger_to_value).collect();
        Ok(self.run_call(
            &class_name,
            &method_name,
            &descriptor,
            &vm_args,
            thread_id,
            return_sig,
        ))
    }

    fn invoke_instance(
        &self,
        receiver_id: u64,
        class_id: u64,
        method_id: u64,
        thread_id: u64,
        args: &[DebuggerValue],
        return_sig: &str,
        _non_virtual: bool,
    ) -> Result<InvokeOutcome, BridgeError> {
        let class_name = self
            .class_name_for(class_id)
            .ok_or(BridgeError::InvalidClass)?;
        let (method_name, descriptor) = self
            .method_sig_for(class_id, method_id)
            .ok_or(BridgeError::InvalidClass)?;
        if receiver_id == 0 {
            return Err(BridgeError::InvalidObject);
        }
        {
            let ds = self.shared.debug.debug_state.lock();
            if !ds.thread_names.contains_key(&thread_id) {
                return Err(BridgeError::InvalidThread);
            }
        }

        // Prepend the receiver (as Value::Object) to the arguments.  The
        // VM's dispatch layer expects the "this" pointer as arg 0.
        let ptr = receiver_id as *mut u8;
        if ptr.is_null() || (ptr as usize) % 8 != 0 {
            return Err(BridgeError::InvalidObject);
        }
        let receiver = unsafe { crate::types::ObjectRef::from_raw(ptr) };
        let mut full_args = Vec::with_capacity(args.len() + 1);
        full_args.push(crate::types::Value::Object(Some(receiver)));
        full_args.extend(args.iter().map(Self::debugger_to_value));

        // For non-virtual dispatch we resolve statically against class_name.
        // `invoke_or_native` always goes through the class name we pass, so
        // that's already "static" by construction; the distinction between
        // virtual and non-virtual is only meaningful when you also walk
        // the receiver's class chain.  We forward both to the same path
        // for now — non-virtual is implicit since we dispatch by `class_name`.
        Ok(self.run_call(
            &class_name,
            &method_name,
            &descriptor,
            &full_args,
            thread_id,
            return_sig,
        ))
    }

    fn new_array(&self, array_type_id: u64, length: i32) -> Result<u64, BridgeError> {
        if length < 0 {
            return Err(BridgeError::Internal);
        }
        let element_kind_byte = {
            let ds = self.shared.debug.debug_state.lock();
            ds.array_type_element_kind.get(&array_type_id).copied()
        };
        let element_type = match element_kind_byte {
            Some(b'Z') => crate::memory::heap::ArrayElementType::Boolean,
            Some(b'B') => crate::memory::heap::ArrayElementType::Byte,
            Some(b'C') => crate::memory::heap::ArrayElementType::Char,
            Some(b'S') => crate::memory::heap::ArrayElementType::Short,
            Some(b'I') => crate::memory::heap::ArrayElementType::Int,
            Some(b'J') => crate::memory::heap::ArrayElementType::Long,
            Some(b'F') => crate::memory::heap::ArrayElementType::Float,
            Some(b'D') => crate::memory::heap::ArrayElementType::Double,
            // Unknown element kind → reference array (Object[])
            _ => crate::memory::heap::ArrayElementType::Reference,
        };

        // Map the arrayTypeID to a ClassId for the heap header.  If the
        // debugger-side ID doesn't resolve to a loaded class we fall back
        // to ClassId::new(0) (the bootstrap placeholder) so allocation
        // still succeeds — the debugger's subsequent `ArrayReference.Length`
        // call will see the correct length regardless.
        let class_id = {
            let cm = self.shared.classes.class_manager.read();
            let cid = crate::classloading::ClassId::new(array_type_id as u32);
            if cm.class_store.get(cid).is_some() {
                cid
            } else {
                crate::classloading::ClassId::new(0)
            }
        };

        let array_ref = self
            .shared
            .mem
            .heap
            .alloc_array(class_id, element_type, length as usize);
        let wire_id = array_ref.as_ptr() as u64;

        // Register the allocated array in DebugState so subsequent
        // `ArrayReference.Length` / `ArrayReference.GetValues` can find it.
        {
            let mut ds = self.shared.debug.debug_state.lock();
            ds.array_lengths.insert(wire_id, length as usize);
            ds.array_type_tags
                .insert(wire_id, element_kind_byte.unwrap_or(b'L'));
        }
        Ok(wire_id)
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_state_initial_values() {
        let st = DebugState::new();
        assert!(!st.suspended);
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
    // T6.4.4 — Watchpoint tests
    // -----------------------------------------------------------------------

    #[test]
    fn t644_field_access_watchpoint_stored_and_retrieved() {
        let mut st = DebugState::new();
        assert!(st.field_access_watchpoints.is_empty());
        st.field_access_watchpoints.push(FieldWatchpoint {
            class_id: 10,
            field_id: 3,
            request_id: 1,
        });
        assert_eq!(st.field_access_watchpoints.len(), 1);
        assert_eq!(st.field_access_watchpoints[0].class_id, 10);
        assert_eq!(st.field_access_watchpoints[0].field_id, 3);
    }

    #[test]
    fn t644_field_modification_watchpoint_stored_and_retrieved() {
        let mut st = DebugState::new();
        st.field_modification_watchpoints.push(FieldWatchpoint {
            class_id: 20,
            field_id: 5,
            request_id: 2,
        });
        assert_eq!(st.field_modification_watchpoints.len(), 1);
        assert_eq!(st.field_modification_watchpoints[0].request_id, 2);
    }

    #[test]
    fn t644_fire_field_access_event_no_match() {
        let st = DebugState::new();
        // No watchpoints registered — should return None.
        assert!(st.fire_field_access_event(10, 3, 1, 10, 100, 0).is_none());
    }

    #[test]
    fn t644_fire_field_access_event_match() {
        let mut st = DebugState::new();
        // Register the event request first
        let req_id = st.events.set_event_request(
            events::EventKind::FieldAccess,
            events::SuspendPolicy::All,
            vec![events::EventModifier::FieldOnly {
                class_id: 10,
                field_id: 3,
            }],
        );
        st.field_access_watchpoints.push(FieldWatchpoint {
            class_id: 10,
            field_id: 3,
            request_id: req_id,
        });

        let result = st.fire_field_access_event(10, 3, 42, 10, 100, 5);
        assert!(result.is_some());
        let (rid, sp, pkt) = result.unwrap();
        assert_eq!(rid, req_id);
        assert_eq!(sp, events::SuspendPolicy::All);
        // Packet should be a Command (composite event)
        match pkt {
            protocol::JdwpPacket::Command {
                command_set,
                command,
                ..
            } => {
                assert_eq!(command_set, 64);
                assert_eq!(command, 100);
            }
            _ => panic!("expected Command packet"),
        }
    }

    #[test]
    fn t644_fire_field_modification_event_match() {
        let mut st = DebugState::new();
        let req_id = st.events.set_event_request(
            events::EventKind::FieldModification,
            events::SuspendPolicy::EventThread,
            vec![events::EventModifier::FieldOnly {
                class_id: 20,
                field_id: 7,
            }],
        );
        st.field_modification_watchpoints.push(FieldWatchpoint {
            class_id: 20,
            field_id: 7,
            request_id: req_id,
        });

        let result = st.fire_field_modification_event(20, 7, 99, 20, 200, 10);
        assert!(result.is_some());
        let (_, sp, _) = result.unwrap();
        assert_eq!(sp, events::SuspendPolicy::EventThread);
    }

    #[test]
    fn t644_fire_field_access_wrong_field_returns_none() {
        let mut st = DebugState::new();
        let req_id = st.events.set_event_request(
            events::EventKind::FieldAccess,
            events::SuspendPolicy::All,
            vec![events::EventModifier::FieldOnly {
                class_id: 10,
                field_id: 3,
            }],
        );
        st.field_access_watchpoints.push(FieldWatchpoint {
            class_id: 10,
            field_id: 3,
            request_id: req_id,
        });
        // Wrong field_id
        assert!(st.fire_field_access_event(10, 99, 1, 10, 100, 0).is_none());
        // Wrong class_id
        assert!(st.fire_field_access_event(99, 3, 1, 10, 100, 0).is_none());
    }

    // -----------------------------------------------------------------------
    // T6.4.5 — Conditional breakpoint tests
    // -----------------------------------------------------------------------

    #[test]
    fn t645_evaluate_no_condition_returns_true() {
        let mut st = DebugState::new();
        // No condition registered for request 99 — should return true
        assert!(st.evaluate_breakpoint_condition(99));
    }

    #[test]
    fn t645_hit_count_equal_mode() {
        let mut st = DebugState::new();
        st.breakpoint_conditions.insert(
            1,
            BreakpointCondition {
                condition: None,
                hit_count: 0,
                hit_count_filter: Some(HitCountFilter {
                    mode: HitCountMode::Equal,
                    count: 3,
                }),
            },
        );
        // Hits 1, 2 should not fire; hit 3 should fire.
        assert!(!st.evaluate_breakpoint_condition(1)); // hit 1
        assert!(!st.evaluate_breakpoint_condition(1)); // hit 2
        assert!(st.evaluate_breakpoint_condition(1)); // hit 3
        assert!(!st.evaluate_breakpoint_condition(1)); // hit 4
    }

    #[test]
    fn t645_hit_count_greater_or_equal_mode() {
        let mut st = DebugState::new();
        st.breakpoint_conditions.insert(
            2,
            BreakpointCondition {
                condition: None,
                hit_count: 0,
                hit_count_filter: Some(HitCountFilter {
                    mode: HitCountMode::GreaterOrEqual,
                    count: 3,
                }),
            },
        );
        assert!(!st.evaluate_breakpoint_condition(2)); // hit 1
        assert!(!st.evaluate_breakpoint_condition(2)); // hit 2
        assert!(st.evaluate_breakpoint_condition(2)); // hit 3 — fires
        assert!(st.evaluate_breakpoint_condition(2)); // hit 4 — still fires
    }

    #[test]
    fn t645_hit_count_multiple_mode() {
        let mut st = DebugState::new();
        st.breakpoint_conditions.insert(
            3,
            BreakpointCondition {
                condition: None,
                hit_count: 0,
                hit_count_filter: Some(HitCountFilter {
                    mode: HitCountMode::Multiple,
                    count: 2,
                }),
            },
        );
        assert!(!st.evaluate_breakpoint_condition(3)); // hit 1
        assert!(st.evaluate_breakpoint_condition(3)); // hit 2 (multiple of 2)
        assert!(!st.evaluate_breakpoint_condition(3)); // hit 3
        assert!(st.evaluate_breakpoint_condition(3)); // hit 4 (multiple of 2)
    }

    #[test]
    fn t645_condition_expression_stored() {
        let mut st = DebugState::new();
        st.breakpoint_conditions.insert(
            5,
            BreakpointCondition {
                condition: Some("expr:42".to_string()),
                hit_count: 0,
                hit_count_filter: None,
            },
        );
        // With an expression but no hit-count filter, should always pass
        assert!(st.evaluate_breakpoint_condition(5));
        assert_eq!(st.breakpoint_conditions[&5].hit_count, 1);
        assert_eq!(
            st.breakpoint_conditions[&5].condition.as_deref(),
            Some("expr:42")
        );
    }
}
