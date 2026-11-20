// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! JDWP event system — breakpoints, thread events, VM lifecycle.
//!
//! The debugger client registers *event requests* that describe what it wants
//! to be notified about.  The VM checks those requests at the appropriate
//! points (e.g. before executing a bytecode, when a thread starts, etc.) and
//! sends composite event packets back to the debugger.

use std::collections::HashMap;

use crate::debug::protocol::{JdwpPacket, PayloadWriter};

// ---------------------------------------------------------------------------
// Event kinds (wire values from the JDWP spec)
// ---------------------------------------------------------------------------

/// The kind of event that can be requested / delivered.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum EventKind {
    SingleStep = 1,
    Breakpoint = 2,
    FramePop = 3,
    Exception = 4,
    ThreadStart = 6,
    ThreadDeath = 7,
    ClassPrepare = 8,
    ClassUnload = 9,
    FieldAccess = 20,
    FieldModification = 21,
    /// A method was entered (interpreter round i1 wave 24): reported at the
    /// first bytecode of the method, location index 0, as HotSpot's back end
    /// reports it. JDI's `createMethodEntryRequest` (jdb `trace methods`, an
    /// IDE's method breakpoint).
    MethodEntry = 40,
    /// A method is about to return normally (wave 24): reported at its
    /// return bytecode. Not reported for a method left by an exception, as
    /// HotSpot's back end does not (`cbMethodExit`).
    MethodExit = 41,
    /// [`Self::MethodExit`] with the return value after the location (wave
    /// 24): what JDI sends for `createMethodExitRequest` to a JDWP 1.6+
    /// target.
    MethodExitWithReturnValue = 42,
    /// The monitor events (interpreter round i5 wave 103, lane L1;
    /// `canRequestMonitorEvents`): reported by the thread that contends for,
    /// wins, waits on or returns from waiting on a monitor, from the paths
    /// that post the C JVMTI ones (`debug::post_jdwp_monitor_event`). Each is
    /// `requestID, thread, object, location`, then a `MonitorWait`'s timeout
    /// or a `MonitorWaited`'s `timed_out`.
    MonitorContendedEnter = 43,
    MonitorContendedEntered = 44,
    MonitorWait = 45,
    MonitorWaited = 46,
    // NOTE: the JDWP spec uses 6 for VMDeath in the *event* kind, but some
    // implementations number it differently.  We follow the Oracle spec.
    VMStart = 90,
    VMDeath = 99,
}

impl EventKind {
    pub fn from_u8(v: u8) -> Option<Self> {
        match v {
            1 => Some(Self::SingleStep),
            2 => Some(Self::Breakpoint),
            3 => Some(Self::FramePop),
            4 => Some(Self::Exception),
            6 => Some(Self::ThreadStart),
            7 => Some(Self::ThreadDeath),
            8 => Some(Self::ClassPrepare),
            9 => Some(Self::ClassUnload),
            20 => Some(Self::FieldAccess),
            21 => Some(Self::FieldModification),
            40 => Some(Self::MethodEntry),
            41 => Some(Self::MethodExit),
            42 => Some(Self::MethodExitWithReturnValue),
            43 => Some(Self::MonitorContendedEnter),
            44 => Some(Self::MonitorContendedEntered),
            45 => Some(Self::MonitorWait),
            46 => Some(Self::MonitorWaited),
            90 => Some(Self::VMStart),
            99 => Some(Self::VMDeath),
            _ => None,
        }
    }

    /// Is this a method exit kind (41, or 42 with the return value)?
    pub fn is_method_exit(self) -> bool {
        matches!(self, Self::MethodExit | Self::MethodExitWithReturnValue)
    }

    /// Is this one of the monitor events (43-46, interpreter round i5 wave
    /// 103)?
    pub fn is_monitor(self) -> bool {
        matches!(
            self,
            Self::MonitorContendedEnter
                | Self::MonitorContendedEntered
                | Self::MonitorWait
                | Self::MonitorWaited
        )
    }
}

// ---------------------------------------------------------------------------
// Suspend policy
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum SuspendPolicy {
    None = 0,
    EventThread = 1,
    All = 2,
}

impl SuspendPolicy {
    pub fn from_u8(v: u8) -> Option<Self> {
        match v {
            0 => Some(Self::None),
            1 => Some(Self::EventThread),
            2 => Some(Self::All),
            _ => None,
        }
    }

    /// The policy an `EventRequest.Set` asks for, as HotSpot's back end
    /// applies it (interpreter round i1 wave 37, lane L1; measured on HotSpot
    /// 25.0.3 with `tools/probes/interp/L1/L1W37RawJdwpUnknownSuspendPolicy.java`):
    /// every byte is accepted, `NONE` (0) suspends nothing, `ALL` (2) the whole
    /// VM, and any other value -- 1, and also 3, 7, 255 -- the event thread.
    /// HotSpot's event set then carries the byte the request sent; here it
    /// carries `EVENT_THREAD` (1), the policy it applied.
    pub fn from_wire(v: u8) -> Self {
        match v {
            0 => Self::None,
            2 => Self::All,
            _ => Self::EventThread,
        }
    }
}

// ---------------------------------------------------------------------------
// Step size and depth constants (JDWP spec)
// ---------------------------------------------------------------------------

/// Step size — granularity of stepping.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u32)]
pub enum StepSize {
    /// Step by bytecode instruction.
    Min = 0,
    /// Step by source line.
    Line = 1,
}

impl StepSize {
    pub fn from_u32(v: u32) -> Option<Self> {
        match v {
            0 => Some(Self::Min),
            1 => Some(Self::Line),
            _ => None,
        }
    }
}

/// Step depth — controls whether to step into, over, or out of calls.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u32)]
pub enum StepDepth {
    /// Step into method calls.
    Into = 0,
    /// Step over method calls (stay at same or lower call depth).
    Over = 1,
    /// Step out of the current method.
    Out = 2,
    /// A depth JDWP does not define, which a raw client may send
    /// (interpreter round i4 wave 86, lane L1). The JDK back end stores it
    /// unchecked (`stepControl_beginStep`) and its `initEvents` switch
    /// enables single stepping for none of INTO / OVER / OUT (`default:
    /// JDI_ASSERT(JNI_FALSE)`, a no-op in a product build), nor does any
    /// frame-pop or catch handler re-enable it: such a step never reports.
    /// Never matched here either ([`step_reports`]).
    Undefined = u32::MAX,
}

impl StepDepth {
    pub fn from_u32(v: u32) -> Option<Self> {
        match v {
            0 => Some(Self::Into),
            1 => Some(Self::Over),
            2 => Some(Self::Out),
            _ => None,
        }
    }
}

// ---------------------------------------------------------------------------
// Event modifiers
// ---------------------------------------------------------------------------

/// Modifiers that narrow down when an event fires.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EventModifier {
    /// Only fire if the class matches.
    ClassOnly { class_id: u64 },
    /// Only fire at a specific bytecode location.
    LocationOnly {
        class_id: u64,
        method_id: u64,
        offset: u64,
    },
    /// Fire only after *count* occurrences (decrement each time).
    Count(i32),
    /// Only fire if the event thread matches.
    ThreadOnly { thread_id: u64 },
    /// Match class by name pattern (e.g. "java.lang.*").
    ClassMatch { pattern: String },
    /// Exclude class by name pattern.
    ClassExclude { pattern: String },
    /// Step modifier (JDWP modKind 10): thread + size + depth.
    Step {
        thread_id: u64,
        size: StepSize,
        depth: StepDepth,
    },
    /// FieldOnly modifier (JDWP modKind 9): restrict to a specific field.
    FieldOnly { class_id: u64, field_id: u64 },
    /// ConditionalFilter modifier (JDWP modKind 2): expression ID for
    /// conditional breakpoints.
    ConditionalFilter { expr_id: u32 },
    /// ExceptionOnly modifier (JDWP modKind 8, wave 10): an `Exception`
    /// request reports exceptions of class `class_id` or a subclass (`0`:
    /// every class), caught ones only with `caught`, uncaught ones only with
    /// `uncaught`.
    ExceptionOnly {
        class_id: u64,
        caught: bool,
        uncaught: bool,
    },
    /// InstanceOnly modifier (JDWP modKind 11, wave 10): the event's
    /// instance must be the object `object_id` names. Accepted on field
    /// watch requests only, where the instance is the object whose field is
    /// accessed (`commands::handle_er_set` refuses it elsewhere).
    InstanceOnly { object_id: u64 },
    /// SourceNameMatch modifier (JDWP modKind 12, interpreter round i1 wave
    /// 23): a `ClassPrepare` request reports only classes whose source name
    /// (their `SourceFile`; this VM keeps no `SourceDebugExtension`)
    /// matches `pattern` — exact, or with a leading or trailing `*`. JDWP
    /// allows it on `ClassPrepare` requests only (`commands::handle_er_set`
    /// refuses it elsewhere, as HotSpot does). JDI offers it to every JDWP
    /// 1.6+ target without asking a capability, so it was refused
    /// (`ILLEGAL_ARGUMENT`), and `ClassPrepareRequest.addSourceNameFilter`
    /// failed to enable.
    SourceNameMatch { pattern: String },
    /// PlatformThreadsOnly modifier (JDWP modKind 13, JDWP 21; interpreter
    /// round i1 wave 25): a `ThreadStart` / `ThreadDeath` request reports
    /// platform threads only, not virtual ones. JDWP allows it on those two
    /// kinds only (`commands::handle_er_set` refuses it elsewhere). JDI
    /// sends it (jdb's default thread requests) only to a JDWP 19+ target.
    PlatformThreadsOnly,
}

// ---------------------------------------------------------------------------
// Event request
// ---------------------------------------------------------------------------

/// Does a request whose `Count` modifier is spent stop arming the debugger
/// gates (interpreter round i1 wave 46, lane L1;
/// `interpreter-L1-proposal-a-spent-count-filter-releases-its-request-FIXED-20261010`)? A spent
/// `Count` reports nothing ever again (JDWP: "subsequent events are never
/// reported for this request"; HotSpot's back end deletes such a request,
/// `eventFilter.c`), but the request stays in the table until the debugger
/// clears it, and until wave 46 it kept every gate it armed up: a spent
/// `MethodEntry` / `MethodExit` or step request kept every method
/// interpreted and every compiled body withdrawn, a spent breakpoint its
/// method interpreted. With the switch the gate questions
/// ([`EventManager::method_events_requested`],
/// [`EventManager::has_single_steps`], [`EventManager::breakpoint_locations`],
/// [`EventManager::has_requests`], [`EventManager::location_filter_class_ids`])
/// skip spent requests, and the matcher that spends one records it
/// ([`EventManager::take_spent`]) so the gates are republished
/// (`debug::republish_gates_if_spent`). The request stays in the table, so a
/// later `EventRequest.Clear` of its id answers as before. Off: the gates
/// count every request, as before wave 46.
pub(crate) const SPENT_COUNT_RELEASES_GATES: bool = true;

/// A registered event request from the debugger client.
#[derive(Debug, Clone)]
pub struct EventRequest {
    pub id: u32,
    pub kind: EventKind,
    pub suspend_policy: SuspendPolicy,
    pub modifiers: Vec<EventModifier>,
    /// For SingleStep requests: the step depth mode.
    pub step_depth: Option<StepDepth>,
    /// For SingleStep requests: the step size.
    pub step_size: Option<StepSize>,
    /// For SingleStep requests: the frame depth when stepping was initiated.
    pub initial_frame_depth: Option<usize>,
    /// For SingleStep requests: where stepping was initiated, as `(class id,
    /// method id, source line)` of the stepping thread's top frame, when that
    /// method has line information. A LINE step in that same frame completes
    /// on the first bytecode of a DIFFERENT line (wave 6).
    pub initial_line: Option<(u64, u64, i32)>,
    /// For a SingleStep INTO request: where its method-entry mode stands
    /// (interpreter round i1 wave 24, [`StepEntryMode`]).
    pub step_entry: StepEntryMode,
}

impl EventRequest {
    /// Is this request spent: one of its `Count` modifiers reached zero, so
    /// it reports no event ever again (interpreter round i1 wave 46, lane
    /// L1; [`SPENT_COUNT_RELEASES_GATES`])? `EventRequest.Set` refuses a
    /// count below one (`commands::event_request_refusal`), so a count is
    /// zero only once it has been reached that many times.
    pub fn is_spent(&self) -> bool {
        self.modifiers
            .iter()
            .any(|m| matches!(m, EventModifier::Count(n) if *n <= 0))
    }

    /// Does this request arm the debugger gates: not spent, or spent
    /// requests still arm them ([`SPENT_COUNT_RELEASES_GATES`] off)?
    fn arms_gates(&self) -> bool {
        !SPENT_COUNT_RELEASES_GATES || !self.is_spent()
    }
}

/// HotSpot's method-entry mode of a step INTO (interpreter round i1 wave 24;
/// the reference back end's `stepControl.c`): when the step reaches a method
/// deeper than the frame it started in whose class the request's class
/// filters (`ClassOnly`, `ClassMatch`, `ClassExclude`) reject, single
/// stepping is switched off and the step waits for the next method ENTRY in
/// a class they accept (`handleMethodEnterEvent`); stepping is switched on
/// there, so the step completes at the first location single stepping sees
/// after the entry — the method's SECOND bytecode — whatever its line. A
/// return into the step's own frame (or above it) switches stepping back on
/// (`handleFramePopEvent`). A debugger sees it: a step into
/// `Service.work()` filtered to `Service` through a call in `Util` stops at
/// `work@1`, not `work@0`, on HotSpot
/// (`tools/probes/interp/L1/L1W24JdiSurface.java`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum StepEntryMode {
    /// Stepping normally.
    #[default]
    Stepping,
    /// In a filtered method deeper than the step's frame: waiting for the
    /// entry of a method whose class the filters accept.
    AwaitingEntry,
    /// Such a method was just entered: the next location the filters accept
    /// completes the step.
    Entered,
}

// ---------------------------------------------------------------------------
// Concrete event (ready to send)
// ---------------------------------------------------------------------------

/// A single event instance ready to be sent to the debugger.
#[derive(Debug, Clone)]
pub struct Event {
    pub request_id: u32,
    pub kind: EventKind,
    pub thread_id: u64,
    /// Opaque, event-specific payload bytes (already serialized).
    pub extra: Vec<u8>,
}

// ---------------------------------------------------------------------------
// EventManager
// ---------------------------------------------------------------------------

/// Manages active event requests and matches them against VM events.
pub struct EventManager {
    requests: HashMap<u32, EventRequest>,
    next_request_id: u32,
    /// The suspend-policy byte an `EventRequest.Set` sent, for the requests
    /// whose byte JDWP does not define (3 to 255), by request id
    /// (interpreter round i1 wave 38, lane L1). Such a request is applied as
    /// `EVENT_THREAD` ([`SuspendPolicy::from_wire`]), and the event sets it
    /// reports carry the byte it sent, as HotSpot's do
    /// ([`Self::wire_suspend_policy`]). Kept after the request is cleared or
    /// its `Count` expires it, since an event it matched may still be on the
    /// server's channel; emptied by [`Self::clear_all`]. One entry per such
    /// request, and only a raw client sends one.
    wire_suspend_policies: HashMap<u32, u8>,
    /// The requests a matcher spent (their `Count` reached zero) since the
    /// last [`Self::take_spent`] (interpreter round i1 wave 46, lane L1):
    /// the gates they armed must be republished. Recorded only with
    /// [`SPENT_COUNT_RELEASES_GATES`] on.
    spent: Vec<u32>,
    /// The `(class, method, offset)` breakpoint locations a redefinition of
    /// their class cleared while a request stood there
    /// ([`Self::disarm_class_breakpoints`], wave 119): no request there fires
    /// until the last one is deleted.
    cleared_locations: std::collections::HashSet<(u64, u64, u64)>,
}

/// The `(class, method, offset)` of a `Breakpoint` request's `LocationOnly`
/// modifier. Wave 119.
fn breakpoint_location(req: &EventRequest) -> Option<(u64, u64, u64)> {
    if req.kind != EventKind::Breakpoint {
        return None;
    }
    req.modifiers.iter().find_map(|m| match m {
        EventModifier::LocationOnly {
            class_id,
            method_id,
            offset,
        } => Some((*class_id, *method_id, *offset)),
        _ => None,
    })
}

impl EventManager {
    pub fn new() -> Self {
        Self {
            requests: HashMap::new(),
            next_request_id: 1,
            wire_suspend_policies: HashMap::new(),
            spent: Vec::new(),
            cleared_locations: std::collections::HashSet::new(),
        }
    }

    /// The requests matchers spent since the last call (wave 46; see
    /// [`SPENT_COUNT_RELEASES_GATES`]), taken: the caller republishes the
    /// gates when there is one. Empty, and allocation-free, in the common
    /// case.
    pub fn take_spent(&mut self) -> Vec<u32> {
        std::mem::take(&mut self.spent)
    }

    /// Record that request `id` was just spent when the switch is on and it
    /// was not before this match (`was_spent`).
    fn note_spent(&mut self, id: u32, was_spent: bool) {
        if !SPENT_COUNT_RELEASES_GATES || was_spent {
            return;
        }
        if self.requests.get(&id).is_some_and(EventRequest::is_spent) {
            self.spent.push(id);
        }
    }

    /// Record the suspend-policy byte request `id`'s `EventRequest.Set` sent,
    /// when JDWP does not define it (wave 38; see `wire_suspend_policies`).
    /// A defined byte (0, 1, 2) is its policy's own and is not recorded.
    pub fn note_wire_suspend_policy(&mut self, id: u32, wire: u8) {
        if SuspendPolicy::from_u8(wire).is_none() {
            self.wire_suspend_policies.insert(id, wire);
        }
    }

    /// The suspend-policy byte an event set reports for an event of request
    /// `request_id` applied as `EVENT_THREAD`: the byte its request sent when
    /// JDWP does not define it (wave 38), `None` for every other request.
    pub fn wire_suspend_policy(&self, request_id: u32) -> Option<u8> {
        self.wire_suspend_policies.get(&request_id).copied()
    }

    /// Register a new event request; returns the assigned request ID.
    pub fn set_event_request(
        &mut self,
        kind: EventKind,
        suspend_policy: SuspendPolicy,
        modifiers: Vec<EventModifier>,
    ) -> u32 {
        let id = self.next_request_id;
        self.next_request_id += 1;

        // Extract step depth/size from the Step modifier if present.
        let (step_depth, step_size) = modifiers
            .iter()
            .find_map(|m| {
                if let EventModifier::Step { depth, size, .. } = m {
                    Some((Some(*depth), Some(*size)))
                } else {
                    None
                }
            })
            .unwrap_or((None, None));

        self.requests.insert(
            id,
            EventRequest {
                id,
                kind,
                suspend_policy,
                modifiers,
                step_depth,
                step_size,
                initial_frame_depth: None, // set by the VM when stepping begins
                initial_line: None,
                step_entry: StepEntryMode::Stepping,
            },
        );
        id
    }

    /// Set the initial frame depth for a step request (called by the VM
    /// when it begins stepping a thread).
    pub fn set_initial_frame_depth(&mut self, request_id: u32, depth: usize) {
        if let Some(req) = self.requests.get_mut(&request_id) {
            req.initial_frame_depth = Some(depth);
        }
    }

    /// Record the source line a step request starts on: `line` of method
    /// `method_id` in class `class_id` (see [`EventRequest::initial_line`]).
    pub fn set_initial_line(&mut self, request_id: u32, class_id: u64, method_id: u64, line: i32) {
        if let Some(req) = self.requests.get_mut(&request_id) {
            req.initial_line = Some((class_id, method_id, line));
        }
    }

    /// Make every request's thread filter (`ThreadOnly`, and a `Step`'s
    /// thread) that names VM thread `from` name `to` instead (interpreter
    /// round i5 wave 105, lane L1). Used when `main` ends and the launcher's
    /// VM thread 0 becomes `DestroyJavaVM` (`debug::note_main_thread_ended`):
    /// HotSpot's filter holds `main`'s thread object, so it never matches
    /// the `DestroyJavaVM` thread's events. Returns how many were renamed.
    pub fn rename_thread_filters(&mut self, from: u64, to: u64) -> usize {
        let mut renamed = 0;
        for req in self.requests.values_mut() {
            for m in &mut req.modifiers {
                match m {
                    EventModifier::ThreadOnly { thread_id }
                    | EventModifier::Step { thread_id, .. }
                        if *thread_id == from =>
                    {
                        *thread_id = to;
                        renamed += 1;
                    }
                    _ => {}
                }
            }
        }
        renamed
    }

    /// Remove an event request by ID.  Returns `true` if something was removed.
    pub fn clear_event_request(&mut self, id: u32) -> bool {
        self.requests.remove(&id).is_some()
    }

    /// Look up a request by ID.
    pub fn get_request(&self, id: u32) -> Option<&EventRequest> {
        self.requests.get(&id)
    }

    /// Return how many requests are currently active.
    pub fn request_count(&self) -> usize {
        self.requests.len()
    }

    /// Check whether any breakpoint request matches the given location.
    pub fn check_breakpoint(
        &self,
        class_id: u64,
        method_id: u64,
        offset: u64,
    ) -> Option<&EventRequest> {
        for req in self.requests.values() {
            if req.kind != EventKind::Breakpoint || self.on_cleared_location(req) {
                continue;
            }
            for m in &req.modifiers {
                if let EventModifier::LocationOnly {
                    class_id: cid,
                    method_id: mid,
                    offset: off,
                } = m
                {
                    if *cid == class_id && *mid == method_id && *off == offset {
                        return Some(req);
                    }
                }
            }
        }
        None
    }

    /// The `kind` requests (`Breakpoint`, `SingleStep`, and since wave 24
    /// `MethodEntry` / `MethodExit` / `MethodExitWithReturnValue`) that report
    /// an event at `at`, as `(request id, suspend policy)`, the newest
    /// request first (wave 85, measured; see
    /// [`Self::match_location_events_with_instance`]). This is what the
    /// interpreter's suspend point asks before each
    /// bytecode while the debugger gate is armed.
    ///
    /// Every modifier is applied, in the order the debugger sent it, as JDWP
    /// specifies (see [`location_request_reports`]); a `Count` modifier is
    /// consumed as it is reached, so this takes `&mut self`. A `Breakpoint`
    /// request without a `LocationOnly` modifier never matches.
    ///
    /// `class_is` answers `ClassOnly` (interpreter round i1 wave 24): is the
    /// location's class the class of that id, or a subtype? The caller
    /// computes it before taking the debug-state lock (it needs the class
    /// manager), for the ids [`Self::location_filter_class_ids`] names.
    pub fn match_location_events(
        &mut self,
        kind: EventKind,
        at: &EventLocation<'_>,
        class_is: &dyn Fn(u64) -> bool,
    ) -> Vec<(u32, SuspendPolicy)> {
        self.match_location_events_with_instance(kind, at, class_is, &|_| true)
    }

    /// [`Self::match_location_events`] with the `InstanceOnly` answer
    /// (interpreter round i4 wave 85, lane L1): `instance_is(id)` -- is the
    /// object id `id` names the location frame's active `this` -- answering
    /// true for every id when the frame has none (a static method, a native
    /// frame, a slot 0 that holds no object), as the JDK back end's
    /// `eventFilter.c` lets an event whose `eventInstance` is NULL through.
    /// [`Self::match_location_events`] passes every `InstanceOnly`: its
    /// callers (lambda proxy and native-method events) know no instance.
    pub fn match_location_events_with_instance(
        &mut self,
        kind: EventKind,
        at: &EventLocation<'_>,
        class_is: &dyn Fn(u64) -> bool,
        instance_is: &dyn Fn(u64) -> bool,
    ) -> Vec<(u32, SuspendPolicy)> {
        let mut ids: Vec<u32> = self
            .requests
            .values()
            .filter(|r| r.kind == kind && !self.on_cleared_location(r))
            .map(|r| r.id)
            .collect();
        // Newest request first (interpreter round i4 wave 85, lane L1;
        // MEASURED, HotSpot 25.0.3, `tools/probes/interp/L1/L1W85EventSetOrder.java`:
        // two breakpoints at one location report `bp2, bp1`): the JDK back
        // end's `eventHandler.c` `insert()` puts a new request at the head of
        // its kind's handler chain, which `filterAndHandleEvent` walks.
        ids.sort_unstable_by(|a, b| b.cmp(a));
        let mut hits = Vec::new();
        for id in ids {
            let mut was_spent = true;
            if let Some(req) = self.requests.get_mut(&id) {
                was_spent = req.is_spent();
                if location_request_reports(req, at, class_is, instance_is) {
                    hits.push((id, req.suspend_policy));
                }
            }
            self.note_spent(id, was_spent);
        }
        hits
    }

    /// The class ids the `ClassOnly` modifiers of the location-scoped
    /// requests (breakpoints, steps, method entries and exits) name
    /// (interpreter round i1 wave 24): the subtype questions the `class_is`
    /// of [`Self::match_location_events`] must answer. Empty — the common
    /// case — when no such request has one, so the suspend point asks the
    /// class manager nothing.
    pub fn location_filter_class_ids(&self) -> Vec<u64> {
        let mut ids: Vec<u64> = self
            .requests
            .values()
            .filter(|r| {
                matches!(
                    r.kind,
                    EventKind::Breakpoint
                        | EventKind::SingleStep
                        | EventKind::MethodEntry
                        | EventKind::MethodExit
                        | EventKind::MethodExitWithReturnValue
                ) && r.arms_gates()
            })
            .flat_map(|r| r.modifiers.iter())
            .filter_map(|m| match m {
                EventModifier::ClassOnly { class_id } => Some(*class_id),
                _ => None,
            })
            .collect();
        ids.sort_unstable();
        ids.dedup();
        ids
    }

    /// Which method events the requests in force ask for (interpreter round
    /// i1 wave 24): `(entries, exits)`, an exit being `MethodExit` or
    /// `MethodExitWithReturnValue`. The suspend point reports a method entry
    /// or exit only while one is asked for, and the gates arm every method
    /// then (`debug::publish_debugger_gates`).
    pub fn method_events_requested(&self) -> (bool, bool) {
        let mut entries = false;
        let mut exits = false;
        for r in self.requests.values().filter(|r| r.arms_gates()) {
            entries |= r.kind == EventKind::MethodEntry;
            exits |= r.kind.is_method_exit();
        }
        (entries, exits)
    }

    /// [`Self::method_events_requested`] split by scope (interpreter round i4
    /// wave 93, lane L1; `CRATONVM_JDWP_SCOPED_METHOD_EVENTS`): `(entries,
    /// exits, scoped)`, where `entries` / `exits` count only the requests that
    /// concern EVERY method -- those without a class scope
    /// ([`method_event_scope_of`]) and those whose scope `covered` does not
    /// answer for -- and `scoped` says whether a covered class-scoped request
    /// is in force. `debug::publish_debugger_gates` keeps every method
    /// interpreted only for the first two.
    pub fn method_events_split(
        &self,
        covered: &dyn Fn(&MethodEventScope) -> bool,
    ) -> (bool, bool, bool) {
        let mut entries = false;
        let mut exits = false;
        let mut scoped = false;
        for r in self.requests.values().filter(|r| r.arms_gates()) {
            let entry = r.kind == EventKind::MethodEntry;
            let exit = r.kind.is_method_exit();
            if !(entry || exit) {
                continue;
            }
            if method_event_scope_of(&r.modifiers).is_some_and(|scope| covered(&scope)) {
                scoped = true;
            } else {
                entries |= entry;
                exits |= exit;
            }
        }
        (entries, exits, scoped)
    }

    /// The class scopes of the `MethodEntry` / `MethodExit(WithReturnValue)`
    /// requests in force that have one ([`method_event_scope_of`]), sorted
    /// and deduplicated (interpreter round i4 wave 93, lane L1): what
    /// `debug::refresh_method_event_scope` resolves to class ids.
    pub fn method_event_scopes(&self) -> Vec<MethodEventScope> {
        let mut scopes: Vec<MethodEventScope> = self
            .requests
            .values()
            .filter(|r| {
                r.arms_gates() && (r.kind == EventKind::MethodEntry || r.kind.is_method_exit())
            })
            .filter_map(|r| method_event_scope_of(&r.modifiers))
            .collect();
        scopes.sort_unstable();
        scopes.dedup();
        scopes
    }

    /// The JDWP field ids the `FieldOnly` modifiers of the `FieldAccess` /
    /// `FieldModification` requests in force name, sorted and deduplicated
    /// (interpreter round i4 wave 95, lane L1): what
    /// `debug::field_watch_scope::refresh` resolves to the classes that may
    /// access them. A watch request without `FieldOnly` never reports
    /// ([`Self::match_subject_events`]), so it concerns no class.
    pub fn watched_field_ids(&self) -> Vec<u64> {
        let mut ids: Vec<u64> = self
            .requests
            .values()
            .filter(|r| {
                r.arms_gates()
                    && matches!(r.kind, EventKind::FieldAccess | EventKind::FieldModification)
            })
            .flat_map(|r| r.modifiers.iter())
            .filter_map(|m| match m {
                EventModifier::FieldOnly { field_id, .. } => Some(*field_id),
                _ => None,
            })
            .collect();
        ids.sort_unstable();
        ids.dedup();
        ids
    }

    /// `(class id, method id)` of every method a `Breakpoint` request's
    /// `LocationOnly` modifier names: the methods the dispatch loop and the
    /// JIT must treat as debugged (`debug::DebuggerGates`, wave 7).
    pub fn breakpoint_locations(&self) -> std::collections::HashSet<(u64, u64)> {
        self.requests
            .values()
            .filter(|r| r.kind == EventKind::Breakpoint && r.arms_gates())
            .flat_map(|r| r.modifiers.iter())
            .filter_map(|m| match m {
                // A location whose breakpoint a redefinition cleared arms
                // nothing (wave 119, [`Self::disarm_class_breakpoints`]).
                EventModifier::LocationOnly {
                    class_id,
                    method_id,
                    offset,
                } if !self
                    .cleared_locations
                    .contains(&(*class_id, *method_id, *offset)) =>
                {
                    Some((*class_id, *method_id))                }
                _ => None,
            })
            .collect()    }

    /// What HotSpot's `VM_RedefineClasses` does to the breakpoints in a
    /// class it redefines, on every route, identical bytes included
    /// (`JvmtiBreakpoints::clearall_in_class_at_safepoint`): the JVMTI
    /// breakpoint at each location goes, and the JDWP back end, which keeps
    /// one JVMTI breakpoint per location for all its requests there, is not
    /// told. Interpreter round i6 wave 119, lane L3. MEASURED, HotSpot 25.0.3
    /// (JDI, `L3W119RetransformClearsBreakpoints.java`),
    /// after two identical retransforms of the class:
    ///
    /// * the request set before them never fires again;
    /// * a NEW request at the same location does not fire either (the back end
    ///   sees the location's breakpoint as set);
    /// * deleting the requests there succeeds for all but the last, whose
    ///   delete answers `NOT_FOUND` (41): the back end's `ClearBreakpoint` of
    ///   the cleared breakpoint ([`Self::clear_event_request_answer`]);
    /// * once the last is gone, a new request there fires.
    ///
    /// So each location of a `Breakpoint` request in class `class_id` is
    /// recorded as cleared until no request stands there. Answers how many
    /// locations were recorded. The JDWP route's own `RedefineClasses` also
    /// frees the class's requests ([`Self::clear_class_breakpoints`]).
    pub fn disarm_class_breakpoints(&mut self, class_id: u64) -> usize {
        let locations: Vec<(u64, u64, u64)> = self
            .requests
            .values()
            .filter_map(breakpoint_location)
            .filter(|&(c, _, _)| c == class_id)
            .collect();
        let mut recorded = 0;
        for location in locations {
            if self.cleared_locations.insert(location) {
                recorded += 1;
            }
        }
        recorded
    }

    /// Is `req` a breakpoint at a location a redefinition cleared
    /// ([`Self::disarm_class_breakpoints`])? Such a request never fires.
    fn on_cleared_location(&self, req: &EventRequest) -> bool {
        !self.cleared_locations.is_empty()
            && breakpoint_location(req).is_some_and(|at| self.cleared_locations.contains(&at))
    }

    /// `EventRequest.Clear` of request `id`, as HotSpot's back end answers it:
    /// `false` (JDWP `NOT_FOUND`, 41) when it was the last request at a
    /// location whose breakpoint a redefinition cleared (the back end's
    /// `ClearBreakpoint` finds none); the request goes either way, and the
    /// location is live again. `true` otherwise. Wave 119.
    pub fn clear_event_request_answer(&mut self, id: u32) -> bool {
        let Some(removed) = self.requests.remove(&id) else {
            return true;
        };
        let Some(at) = breakpoint_location(&removed) else {
            return true;
        };
        if !self.cleared_locations.contains(&at) {
            return true;
        }
        let still_there = self
            .requests
            .values()
            .any(|r| breakpoint_location(r) == Some(at));
        if still_there {
            return true;
        }
        self.cleared_locations.remove(&at);
        false
    }

    /// The `ClassPrepare` requests that report the class `class_name`
    /// (internal, slash form), as `(request id, suspend policy)` in
    /// request-id order (wave 7). Modifiers apply in order, as for location
    /// events: `ClassMatch` / `ClassExclude` must (not) match, `Count` is
    /// consumed as it is reached, `ClassOnly` must name the prepared class or
    /// one of its supertypes (`class_is`, interpreter round i1 wave 24: it was
    /// not evaluated, so a request filtered to the subtypes of one interface
    /// reported every class the VM prepared), and anything that needs the
    /// preparing thread or a location (`ThreadOnly`, `Step`, `LocationOnly`,
    /// `FieldOnly`) suppresses the event: the server that reports it does not
    /// know the thread.
    ///
    /// `source_names` are the class's source names, which a
    /// `SourceNameMatch` modifier matches when any one does: its `SourceFile`
    /// (wave 23) and (wave 43) the file names its `SourceDebugExtension`'s
    /// strata list, as HotSpot matches them
    /// (`tools/probes/interp/L1/L1W43JdiSourceDebugExtension.java`); a class
    /// with none does not match.
    pub fn match_class_prepare(
        &mut self,
        class_name: &str,
        source_names: &[&str],
        class_is: &dyn Fn(u64) -> bool,
    ) -> Vec<(u32, SuspendPolicy)> {
        self.match_class_prepare_on(class_name, source_names, None, class_is)
    }

    /// [`Self::match_class_prepare`] for a class prepared on thread
    /// `thread` when that thread is known (wave 10: the preparing thread
    /// reports it, `debug::class_prepared_on_thread`): a `ThreadOnly`
    /// modifier is then evaluated instead of suppressing the event.
    pub fn match_class_prepare_on(
        &mut self,
        class_name: &str,
        source_names: &[&str],
        thread: Option<u64>,
        class_is: &dyn Fn(u64) -> bool,
    ) -> Vec<(u32, SuspendPolicy)> {
        self.match_class_event(
            EventKind::ClassPrepare,
            class_name,
            source_names,
            thread,
            class_is,
        )
    }

    /// Does a `ClassPrepare` request filter by source name (interpreter round
    /// i1 wave 43)? Only then are a prepared class's `SourceDebugExtension`
    /// names read ([`Self::match_class_prepare`]).
    pub fn has_source_name_filters(&self) -> bool {
        self.requests.values().any(|r| {
            r.kind == EventKind::ClassPrepare
                && r.modifiers
                    .iter()
                    .any(|m| matches!(m, EventModifier::SourceNameMatch { .. }))
        })
    }

    /// The `ClassUnload` requests that report the unloading of the class
    /// `class_name` (internal, slash form), as `(request id, suspend policy)`
    /// in request-id order (interpreter round i1 wave 25). JDWP allows
    /// `Count`, `ClassMatch` and `ClassExclude` on them, evaluated as for
    /// `ClassPrepare` ([`Self::match_class_prepare`]); an unloaded class has
    /// no thread, no source name and no type to test, so any other modifier
    /// suppresses the event.
    pub fn match_class_unload(&mut self, class_name: &str) -> Vec<(u32, SuspendPolicy)> {
        self.match_class_event(EventKind::ClassUnload, class_name, &[], None, &|_| false)
    }

    /// The requests of `kind` (`ClassPrepare` or, wave 25, `ClassUnload`)
    /// that report class `class_name`; see [`Self::match_class_prepare_on`].
    fn match_class_event(
        &mut self,
        kind: EventKind,
        class_name: &str,
        source_names: &[&str],
        thread: Option<u64>,
        class_is: &dyn Fn(u64) -> bool,
    ) -> Vec<(u32, SuspendPolicy)> {
        let mut ids: Vec<u32> = self
            .requests
            .values()
            .filter(|r| r.kind == kind)
            .map(|r| r.id)
            .collect();
        ids.sort_unstable();
        let mut hits = Vec::new();
        for id in ids {
            let Some(req) = self.requests.get_mut(&id) else {
                continue;
            };
            let was_spent = req.is_spent();
            let mut reports = true;
            for m in req.modifiers.iter_mut() {
                let passes = match m {
                    EventModifier::ClassMatch { pattern } => {
                        class_pattern_matches(pattern, class_name)
                    }
                    EventModifier::ClassExclude { pattern } => {
                        !class_pattern_matches(pattern, class_name)
                    }
                    EventModifier::SourceNameMatch { pattern } => source_names
                        .iter()
                        .any(|source| class_pattern_matches(pattern.as_str(), source)),
                    EventModifier::Count(n) => {
                        if *n <= 0 {
                            false
                        } else {
                            *n -= 1;
                            *n == 0
                        }
                    }
                    EventModifier::ClassOnly { class_id } => class_is(*class_id),
                    EventModifier::ConditionalFilter { .. } => true,
                    EventModifier::ThreadOnly { thread_id } => thread == Some(*thread_id),
                    EventModifier::PlatformThreadsOnly
                    | EventModifier::Step { .. }
                    | EventModifier::LocationOnly { .. }
                    | EventModifier::FieldOnly { .. }
                    | EventModifier::ExceptionOnly { .. }
                    | EventModifier::InstanceOnly { .. } => false,
                };
                if !passes {
                    reports = false;
                    break;
                }
            }
            if reports {
                hits.push((id, req.suspend_policy));
            }
            self.note_spent(id, was_spent);
        }
        hits
    }

    /// The `ThreadStart` / `ThreadDeath` requests (`kind`) that report thread
    /// `thread_id`, as `(request id, suspend policy)` in request-id order
    /// (wave 9). Modifiers apply in order, as for location events:
    /// `ThreadOnly` must match, `Count` is consumed as it is reached, the
    /// reserved `ConditionalFilter` is not evaluated, and a modifier JDWP does
    /// not allow on a thread event (a class, location, field or step filter)
    /// suppresses it. `check_thread_event` (now test-only), which the JDWP server used
    /// until wave 9, answered only the first matching request and ignored
    /// `Count`.
    pub fn match_thread_events(
        &mut self,
        kind: EventKind,
        thread_id: u64,
    ) -> Vec<(u32, SuspendPolicy)> {
        self.match_thread_events_of(kind, thread_id, false)
    }

    /// [`Self::match_thread_events`] for a thread known to be virtual or not
    /// (interpreter round i1 wave 25): a `PlatformThreadsOnly` modifier
    /// suppresses a virtual thread's event.
    pub fn match_thread_events_of(
        &mut self,
        kind: EventKind,
        thread_id: u64,
        thread_is_virtual: bool,
    ) -> Vec<(u32, SuspendPolicy)> {
        let mut ids: Vec<u32> = self
            .requests
            .values()
            .filter(|r| r.kind == kind)
            .map(|r| r.id)
            .collect();
        ids.sort_unstable();
        let mut hits = Vec::new();
        for id in ids {
            let Some(req) = self.requests.get_mut(&id) else {
                continue;
            };
            let was_spent = req.is_spent();
            let mut reports = true;
            for m in req.modifiers.iter_mut() {
                let passes = match m {
                    EventModifier::ThreadOnly { thread_id: only } => *only == thread_id,
                    EventModifier::PlatformThreadsOnly => !thread_is_virtual,
                    EventModifier::Count(n) => {
                        if *n <= 0 {
                            false
                        } else {
                            *n -= 1;
                            *n == 0
                        }
                    }
                    EventModifier::ConditionalFilter { .. } => true,
                    EventModifier::ClassOnly { .. }
                    | EventModifier::ClassMatch { .. }
                    | EventModifier::ClassExclude { .. }
                    | EventModifier::Step { .. }
                    | EventModifier::LocationOnly { .. }
                    | EventModifier::FieldOnly { .. }
                    | EventModifier::ExceptionOnly { .. }
                    | EventModifier::InstanceOnly { .. }
                    | EventModifier::SourceNameMatch { .. } => false,
                };
                if !passes {
                    reports = false;
                    break;
                }
            }
            if reports {
                hits.push((id, req.suspend_policy));
            }
            self.note_spent(id, was_spent);
        }
        hits
    }

    /// Whether any single-step requests are currently active.
    pub fn has_single_steps(&self) -> bool {
        self.requests
            .values()
            .any(|r| r.kind == EventKind::SingleStep && r.arms_gates())
    }

    /// Remove every `Breakpoint` request (JDWP
    /// `EventRequest.ClearAllBreakpoints`), answering their ids.
    pub fn clear_breakpoints(&mut self) -> Vec<u32> {
        let ids: Vec<u32> = self
            .requests
            .values()
            .filter(|r| r.kind == EventKind::Breakpoint)
            .map(|r| r.id)
            .collect();
        for id in &ids {
            self.requests.remove(id);
        }
        // Wave 119: no request stands at a cleared location any more.
        self.cleared_locations.clear();
        ids
    }

    /// Did a redefinition clear the breakpoint at a location some request
    /// stands at ([`Self::disarm_class_breakpoints`])? What HotSpot's back end
    /// answers `ClearAllBreakpoints` with then: `NOT_FOUND` (41), MEASURED
    /// (`L3W119RetransformClearsBreakpoints`, JDI's `deleteAllBreakpoints`).
    /// Wave 119.
    pub fn has_cleared_breakpoint_locations(&self) -> bool {
        !self.cleared_locations.is_empty()
    }

    /// Remove every `Breakpoint` request whose location is in class
    /// `class_id`, answering their ids (interpreter round i5 wave 104, lane
    /// L1): what HotSpot's back end does after a successful
    /// `VirtualMachine.RedefineClasses` (`eventHandler_freeClassBreakpoints`).
    pub fn clear_class_breakpoints(&mut self, class_id: u64) -> Vec<u32> {
        let ids: Vec<u32> = self
            .requests
            .values()
            .filter(|r| {
                r.kind == EventKind::Breakpoint
                    && r.modifiers.iter().any(|m| {
                        matches!(m, EventModifier::LocationOnly { class_id: c, .. } if *c == class_id)
                    })
            })
            .map(|r| r.id)
            .collect();
        for id in &ids {
            self.requests.remove(id);
        }
        // Wave 119: the back end frees them with their (cleared) breakpoints.
        self.cleared_locations.retain(|&(c, _, _)| c != class_id);
        ids
    }

    /// Clear all event requests.
    pub fn clear_all(&mut self) {
        self.requests.clear();
        self.cleared_locations.clear();
        self.wire_suspend_policies.clear();
        self.spent.clear();
    }

    /// Check whether any single-step request matches the given thread and
    /// current frame depth.  The step depth mode determines when the event
    /// fires:
    ///
    /// - **INTO** (or no depth set): always fire
    /// - **OVER**: fire only when `current_frame_depth <= initial_frame_depth`
    /// - **OUT**: fire only when `current_frame_depth < initial_frame_depth`
    ///
    /// If you don't have frame depth info, pass `None` for `current_frame_depth`
    /// and depth filtering will be skipped (backwards compatible).
    pub fn check_single_step(&self, thread_id: u64) -> Option<&EventRequest> {
        self.check_single_step_with_depth(thread_id, None)
    }

    /// Like [`check_single_step`] but with explicit frame depth for
    /// step-over / step-out filtering.
    pub fn check_single_step_with_depth(
        &self,
        thread_id: u64,
        current_frame_depth: Option<usize>,
    ) -> Option<&EventRequest> {
        for req in self.requests.values() {
            if req.kind != EventKind::SingleStep {
                continue;
            }

            // Thread filter: check ThreadOnly modifiers *and* Step modifier thread.
            let mut thread_ids: Vec<u64> = req
                .modifiers
                .iter()
                .filter_map(|m| match m {
                    EventModifier::ThreadOnly { thread_id: tid } => Some(*tid),
                    EventModifier::Step { thread_id: tid, .. } => Some(*tid),
                    _ => None,
                })
                .collect();
            thread_ids.dedup();

            if !thread_ids.is_empty() && !thread_ids.contains(&thread_id) {
                continue;
            }

            // Depth filter: only applies when we have both current depth and
            // the request recorded its initial depth.
            if let (Some(cur), Some(init)) = (current_frame_depth, req.initial_frame_depth) {
                match req.step_depth {
                    Some(StepDepth::Over) => {
                        if cur > init {
                            continue; // inside a deeper call — skip
                        }
                    }
                    Some(StepDepth::Out) => {
                        if cur >= init {
                            continue; // not yet returned — skip
                        }
                    }
                    // Wave 86: never reports (see `StepDepth::Undefined`).
                    Some(StepDepth::Undefined) => continue,
                    // INTO or None — always fire
                    _ => {}
                }
            }

            return Some(req);
        }
        None
    }

    /// Check whether any field-access or field-modification request matches
    /// the given class/field.
    pub fn check_field_watchpoint(
        &self,
        kind: EventKind,
        class_id: u64,
        field_id: u64,
    ) -> Option<&EventRequest> {
        for req in self.requests.values() {
            if req.kind != kind {
                continue;
            }
            for m in &req.modifiers {
                if let EventModifier::FieldOnly {
                    class_id: cid,
                    field_id: fid,
                } = m
                {
                    if *cid == class_id && *fid == field_id {
                        return Some(req);
                    }
                }
            }
        }
        None
    }

    /// Whether any field-access watchpoints are currently active.
    pub fn has_field_access_watchpoints(&self) -> bool {
        self.requests
            .values()
            .any(|r| r.kind == EventKind::FieldAccess)
    }

    /// Whether any field-modification watchpoints are currently active.
    pub fn has_field_modification_watchpoints(&self) -> bool {
        self.requests
            .values()
            .any(|r| r.kind == EventKind::FieldModification)
    }

    /// Check whether any request matches a thread event of the given kind.
    /// Test-only since wave 9: the server uses [`Self::match_thread_events`].
    #[cfg(test)]
    pub fn check_thread_event(&self, kind: EventKind, thread_id: u64) -> Option<&EventRequest> {
        for req in self.requests.values() {
            if req.kind != kind {
                continue;
            }
            // If there are ThreadOnly modifiers, make sure one matches.
            let thread_mods: Vec<_> = req
                .modifiers
                .iter()
                .filter_map(|m| {
                    if let EventModifier::ThreadOnly { thread_id: tid } = m {
                        Some(*tid)
                    } else {
                        None
                    }
                })
                .collect();
            if thread_mods.is_empty() || thread_mods.contains(&thread_id) {
                return Some(req);
            }
        }
        None
    }
}

// ---------------------------------------------------------------------------
// Location-scoped requests (breakpoints, single steps)
// ---------------------------------------------------------------------------

/// Where an interpreter thread is about to execute a bytecode: what a
/// `Breakpoint` or `SingleStep` request is matched against
/// ([`EventManager::match_location_events`]).
#[derive(Debug, Clone, Copy)]
pub struct EventLocation<'a> {
    /// Wire class id (`ClassId::as_u32`).
    pub class_id: u64,
    /// The class's internal (slash) name, e.g. `java/lang/String`.
    pub class_name: &'a str,
    /// `debug::jdwp_method_id` of the executing method.
    pub method_id: u64,
    /// Bytecode index about to execute.
    pub offset: u64,
    /// Wire thread id.
    pub thread_id: u64,
    /// Interpreter frames on the thread, the executing one included — the
    /// depth a step request's `initial_frame_depth` was recorded in.
    pub frame_depth: usize,
    /// Whether `offset` starts a `LineNumberTable` entry of the method;
    /// `None` when the method has no line information.
    pub line_start: Option<bool>,
    /// The source line `offset` belongs to (the entry with the greatest
    /// `start_pc <= offset`); `None` without line information, or when no
    /// entry covers it.
    pub line: Option<i32>,
}

/// Does `req` report an event at `at`? Its modifiers are applied in order,
/// as JDWP specifies, and the first that suppresses the event ends the walk:
///
/// * `LocationOnly`, `ThreadOnly`: must match.
/// * `ClassMatch` / `ClassExclude`: the class name must (not) match the
///   pattern ([`class_pattern_matches`]).
/// * `Step`: its thread must match, then the step depth and size
///   ([`step_reports`]).
/// * `Count(n)`: JDWP "the event is not reported the first count - 1 times
///   this filter is reached ... subsequent events are never reported for
///   this request": each time it is reached the count drops by one, the
///   walk continues only when it reaches zero, and a spent count (`<= 0`)
///   suppresses for good. Because the walk stops at the first suppressing
///   modifier, a `Count` counts only the events every modifier BEFORE it let
///   through, which is the JDWP rule.
/// * `ClassOnly`: the location's class is the filter's class or a subtype
///   (`class_is`, interpreter round i1 wave 24; it was not evaluated, so a
///   step filtered to one class stopped in any class);
/// * `FieldOnly` and the reserved `ConditionalFilter`: not evaluated.
///
/// A step INTO also follows HotSpot's method-entry mode ([`StepEntryMode`],
/// wave 24) before any of this.
fn location_request_reports(
    req: &mut EventRequest,
    at: &EventLocation<'_>,
    class_is: &dyn Fn(u64) -> bool,
    instance_is: &dyn Fn(u64) -> bool,
) -> bool {
    if req.kind == EventKind::Breakpoint
        && !req
            .modifiers
            .iter()
            .any(|m| matches!(m, EventModifier::LocationOnly { .. }))
    {
        return false;
    }
    // A step completing right after a method entry reports wherever the
    // filters accept, whatever the line (HotSpot completes a step into a new
    // method at the first location it sees there).
    let after_entry = match step_entry_gate(req, at, class_is) {
        EntryGate::Suppress => return false,
        EntryGate::AfterEntry => true,
        EntryGate::Normal => false,
    };
    let initial_depth = req.initial_frame_depth;
    let initial_line = req.initial_line;
    for m in req.modifiers.iter_mut() {
        let passes = match m {
            EventModifier::LocationOnly {
                class_id,
                method_id,
                offset,
            } => *class_id == at.class_id && *method_id == at.method_id && *offset == at.offset,
            EventModifier::ThreadOnly { thread_id } => *thread_id == at.thread_id,
            EventModifier::ClassMatch { pattern } => class_pattern_matches(pattern, at.class_name),
            EventModifier::ClassExclude { pattern } => {
                !class_pattern_matches(pattern, at.class_name)
            }
            EventModifier::ClassOnly { class_id } => class_is(*class_id),
            EventModifier::Step {
                thread_id,
                size,
                depth,
            } => {
                *thread_id == at.thread_id
                    && (after_entry || step_reports(*size, *depth, initial_depth, initial_line, at))
            }
            EventModifier::Count(n) => {
                if *n <= 0 {
                    false
                } else {
                    *n -= 1;
                    *n == 0
                }
            }
            EventModifier::FieldOnly { .. } | EventModifier::ConditionalFilter { .. } => true,
            // The frame's active `this` (wave 85; see
            // `EventManager::match_location_events_with_instance`).
            EventModifier::InstanceOnly { object_id } => instance_is(*object_id),
            // Meaningless on a breakpoint or step (JDWP allows them on
            // `Exception` / field / `ClassPrepare` requests only), and never
            // registered on one.
            EventModifier::ExceptionOnly { .. }
            | EventModifier::SourceNameMatch { .. }
            | EventModifier::PlatformThreadsOnly => false,
        };
        if !passes {
            return false;
        }
    }
    true
}

/// What a step INTO's method-entry mode ([`StepEntryMode`]) makes of a
/// location, before the request's modifiers are walked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EntryGate {
    /// Evaluate the modifiers as usual.
    Normal,
    /// Not reported: single stepping is off (or this is the entry itself).
    Suppress,
    /// The first location after an accepted entry: the modifiers decide, and
    /// the step's depth and line rules do not.
    AfterEntry,
}

/// Do the class filters of `modifiers` (`ClassMatch`, `ClassExclude`,
/// `ClassOnly`) accept the class of `at`? What HotSpot's back end predicts
/// (`eventFilter_predictFiltering`) when a step reaches a new method. No
/// `Count` is consumed.
fn class_filters_accept(
    modifiers: &[EventModifier],
    at: &EventLocation<'_>,
    class_is: &dyn Fn(u64) -> bool,
) -> bool {
    modifiers.iter().all(|m| match m {
        EventModifier::ClassMatch { pattern } => class_pattern_matches(pattern, at.class_name),
        EventModifier::ClassExclude { pattern } => !class_pattern_matches(pattern, at.class_name),
        EventModifier::ClassOnly { class_id } => class_is(*class_id),
        _ => true,
    })
}

/// Advance a step INTO's method-entry mode at `at` ([`StepEntryMode`],
/// interpreter round i1 wave 24) and say what it makes of the location.
/// Every other request, a step whose starting depth is unknown, and a
/// location on another thread are [`EntryGate::Normal`].
fn step_entry_gate(
    req: &mut EventRequest,
    at: &EventLocation<'_>,
    class_is: &dyn Fn(u64) -> bool,
) -> EntryGate {
    if req.kind != EventKind::SingleStep || req.step_depth != Some(StepDepth::Into) {
        return EntryGate::Normal;
    }
    let Some(initial_depth) = req.initial_frame_depth else {
        return EntryGate::Normal;
    };
    let on_step_thread = req
        .modifiers
        .iter()
        .any(|m| matches!(m, EventModifier::Step { thread_id, .. } if *thread_id == at.thread_id));
    if !on_step_thread {
        return EntryGate::Normal;
    }
    let deeper = at.frame_depth > initial_depth;
    if !deeper {
        // In the step's own frame or above it: stepping is on again
        // (`handleFramePopEvent`), and the usual rules apply.
        req.step_entry = StepEntryMode::Stepping;
        return EntryGate::Normal;
    }
    let accepted = class_filters_accept(&req.modifiers, at, class_is);
    match req.step_entry {
        StepEntryMode::Stepping if accepted => EntryGate::Normal,
        StepEntryMode::Stepping => {
            // A new method the filters reject: stepping goes off until an
            // accepted method is entered.
            req.step_entry = StepEntryMode::AwaitingEntry;
            EntryGate::Suppress
        }
        StepEntryMode::AwaitingEntry => {
            if accepted && at.offset == 0 {
                // The entry itself is seen by the method-entry handler, not by
                // single stepping, which it switches back on.
                req.step_entry = StepEntryMode::Entered;
            }
            EntryGate::Suppress
        }
        StepEntryMode::Entered if accepted => {
            req.step_entry = StepEntryMode::Stepping;
            EntryGate::AfterEntry
        }
        StepEntryMode::Entered => {
            // Stepping reached a filtered method again first (the entered
            // method's first bytecode called one, or returned into one).
            req.step_entry = StepEntryMode::AwaitingEntry;
            EntryGate::Suppress
        }
    }
}

/// What a field-watch or exception event is matched against beyond its
/// location ([`EventManager::match_subject_events`], interpreter round i1
/// wave 10). The class and instance questions are answered by the caller,
/// which computed them before taking the debug-state lock (they need the
/// class manager, which must not be locked inside it).
pub struct EventSubject<'a> {
    /// `ClassOnly`: is the location's class the class `id`, or a subtype?
    pub location_class_is: &'a dyn Fn(u64) -> bool,
    /// A field event's field: `(declaring class id, debug::jdwp_field_id)`.
    pub field: Option<(u64, u64)>,
    /// `InstanceOnly` (field events): does the id name the accessed object?
    pub instance_is: &'a dyn Fn(u64) -> bool,
    /// An exception event: is the exception an instance of class `id`?
    pub exception_is: &'a dyn Fn(u64) -> bool,
    /// An exception event: does a handler on the thread's stack catch it?
    pub caught: bool,
}

impl EventManager {
    /// Is any request of `kind` in force? One scan of the request table;
    /// the interpreter's field-watch and exception hooks ask it first.
    pub fn has_requests(&self, kind: EventKind) -> bool {
        self.requests
            .values()
            .any(|r| r.kind == kind && r.arms_gates())
    }

    /// Is an `Exception` request in force that compiled code cannot serve yet
    /// (interpreter round i4 wave 93, lane L1; `CRATONVM_JVMTI_EXCEPTION_FROM_COMPILED`
    /// for JDWP): one whose policy suspends a thread -- the park must happen
    /// with the throwing frame on top, and a compiled throw is reported only
    /// after its frames unwound -- or one with an `InstanceOnly` modifier,
    /// which needs the throwing frame's `this`, which a compiled throw site
    /// does not keep. A `SUSPEND_NONE` request without one (an IDE's logging
    /// exception breakpoint) is served from compiled code.
    pub fn exception_requests_need_the_interpreter(&self) -> bool {
        self.requests.values().any(|r| {
            r.kind == EventKind::Exception
                && r.arms_gates()
                && (r.suspend_policy != SuspendPolicy::None
                    || r
                        .modifiers
                        .iter()
                        .any(|m| matches!(m, EventModifier::InstanceOnly { .. })))
        })
    }

    /// The class ids the `kind` requests' `ClassOnly` and `ExceptionOnly`
    /// modifiers name (wave 10): the subtype questions an
    /// [`EventSubject`] must be able to answer. `0` (every class) is left
    /// out.
    pub fn filter_class_ids(&self, kind: EventKind) -> Vec<u64> {
        let mut ids: Vec<u64> = self
            .requests
            .values()
            .filter(|r| r.kind == kind)
            .flat_map(|r| r.modifiers.iter())
            .filter_map(|m| match m {
                EventModifier::ClassOnly { class_id }
                | EventModifier::ExceptionOnly { class_id, .. } => Some(*class_id),
                _ => None,
            })
            .filter(|&id| id != 0)
            .collect();
        ids.sort_unstable();
        ids.dedup();
        ids
    }

    /// The object ids the `InstanceOnly` modifiers of the location-scoped
    /// requests (breakpoints, steps, method entries and exits) name
    /// (interpreter round i4 wave 85, lane L1): the identity questions the
    /// `instance_is` of [`Self::match_location_events_with_instance`] must
    /// answer. Empty -- the common case -- when no such request has one, so
    /// the suspend point reads no `this`.
    pub fn location_instance_filter_ids(&self) -> Vec<u64> {
        let mut ids: Vec<u64> = self
            .requests
            .values()
            .filter(|r| {
                matches!(
                    r.kind,
                    EventKind::Breakpoint
                        | EventKind::SingleStep
                        | EventKind::MethodEntry
                        | EventKind::MethodExit
                        | EventKind::MethodExitWithReturnValue
                )
            })
            .flat_map(|r| r.modifiers.iter())
            .filter_map(|m| match m {
                EventModifier::InstanceOnly { object_id } => Some(*object_id),
                _ => None,
            })
            .collect();
        ids.sort_unstable();
        ids.dedup();
        ids
    }

    /// The object ids the `kind` requests' `InstanceOnly` modifiers name.
    pub fn instance_filter_ids(&self, kind: EventKind) -> Vec<u64> {
        let mut ids: Vec<u64> = self
            .requests
            .values()
            .filter(|r| r.kind == kind)
            .flat_map(|r| r.modifiers.iter())
            .filter_map(|m| match m {
                EventModifier::InstanceOnly { object_id } => Some(*object_id),
                _ => None,
            })
            .collect();
        ids.sort_unstable();
        ids.dedup();
        ids
    }

    /// The `FieldAccess` / `FieldModification` / `Exception` requests
    /// (`kind`) that report an event at `at` about `subject`, as `(request
    /// id, suspend policy)` in request-id order (wave 10). Modifiers apply in
    /// order, as for location events:
    ///
    /// * `Count` is consumed as it is reached; `ThreadOnly`, `LocationOnly`,
    ///   `ClassMatch` / `ClassExclude` (the location's class name) as for a
    ///   breakpoint;
    /// * `ClassOnly`: the location's class or a subtype
    ///   ([`EventSubject::location_class_is`]);
    /// * `FieldOnly`: a field event's field; `InstanceOnly`: its object;
    /// * `ExceptionOnly`: an exception event whose exception is of the class
    ///   or a subclass (`0`: any), and whose caught / uncaught flag the
    ///   request asks for;
    /// * the reserved `ConditionalFilter` is not evaluated; a `Step`, or a
    ///   field modifier on an exception (or the reverse), suppresses.
    ///
    /// A `FieldAccess` / `FieldModification` request without a `FieldOnly`
    /// never reports (JDI always sends one; a watch on every field of the
    /// VM is not a request this server accepts as meaningful).
    pub fn match_subject_events(
        &mut self,
        kind: EventKind,
        at: &EventLocation<'_>,
        subject: &EventSubject<'_>,
    ) -> Vec<(u32, SuspendPolicy)> {
        let mut ids: Vec<u32> = self
            .requests
            .values()
            .filter(|r| r.kind == kind)
            .map(|r| r.id)
            .collect();
        ids.sort_unstable();
        let field_event = matches!(kind, EventKind::FieldAccess | EventKind::FieldModification);
        let mut hits = Vec::new();
        for id in ids {
            let Some(req) = self.requests.get_mut(&id) else {
                continue;
            };
            if field_event
                && !req
                    .modifiers
                    .iter()
                    .any(|m| matches!(m, EventModifier::FieldOnly { .. }))
            {
                continue;
            }
            let was_spent = req.is_spent();
            let mut reports = true;
            for m in req.modifiers.iter_mut() {
                let passes = match m {
                    EventModifier::Count(n) => {
                        if *n <= 0 {
                            false
                        } else {
                            *n -= 1;
                            *n == 0
                        }
                    }
                    EventModifier::ConditionalFilter { .. } => true,
                    EventModifier::ThreadOnly { thread_id } => *thread_id == at.thread_id,
                    EventModifier::LocationOnly {
                        class_id,
                        method_id,
                        offset,
                    } => {
                        *class_id == at.class_id
                            && *method_id == at.method_id
                            && *offset == at.offset
                    }
                    EventModifier::ClassMatch { pattern } => {
                        class_pattern_matches(pattern, at.class_name)
                    }
                    EventModifier::ClassExclude { pattern } => {
                        !class_pattern_matches(pattern, at.class_name)
                    }
                    EventModifier::ClassOnly { class_id } => (subject.location_class_is)(*class_id),
                    EventModifier::FieldOnly { class_id, field_id } => {
                        field_event && subject.field == Some((*class_id, *field_id))
                    }
                    // A field event's object, an exception event's throwing
                    // frame's `this` (wave 85); `instance_is` answers true
                    // where there is none, as the back end's `eventInstance`
                    // NULL passes the filter.
                    // Wave 103: a monitor event's monitor object.
                    EventModifier::InstanceOnly { object_id } => {
                        (field_event || kind == EventKind::Exception || kind.is_monitor())
                            && (subject.instance_is)(*object_id)
                    }
                    EventModifier::ExceptionOnly {
                        class_id,
                        caught,
                        uncaught,
                    } => {
                        let asked = if subject.caught { *caught } else { *uncaught };
                        kind == EventKind::Exception
                            && asked
                            && (*class_id == 0 || (subject.exception_is)(*class_id))
                    }
                    EventModifier::Step { .. }
                    | EventModifier::SourceNameMatch { .. }
                    | EventModifier::PlatformThreadsOnly => false,
                };
                if !passes {
                    reports = false;
                    break;
                }
            }
            if reports {
                hits.push((id, req.suspend_policy));
            }
            self.note_spent(id, was_spent);
        }
        hits
    }
}

/// A step request's depth and size rules at `at`, given the frame depth the
/// step began in (`None`: unknown, so every depth passes).
///
/// * Depth `OVER` reports at the starting depth or shallower, `OUT` only
///   shallower, `INTO` anywhere.
/// * Size `LINE`, in the frame the step began in (same depth, same method,
///   `initial_line` known): reports at the first bytecode whose line differs
///   from the starting line, as the reference back end does (wave 6) — so a
///   backward jump to the start of the starting line (a one-line loop) does
///   not report that line again, and a jump into the middle of another line
///   does report. Anywhere else: at the first bytecode of a line
///   (`EventLocation::line_start`), or anywhere in a caller the step
///   returned into — a debugger shows a step that finishes a method in the
///   middle of the calling line. JDWP: a LINE step in a method without line
///   information "is done as a MIN step instead". Size `MIN` reports at
///   every bytecode.
fn step_reports(
    size: StepSize,
    depth: StepDepth,
    initial_depth: Option<usize>,
    initial_line: Option<(u64, u64, i32)>,
    at: &EventLocation<'_>,
) -> bool {
    if depth == StepDepth::Undefined {
        // Wave 86: the back end never enables stepping for it.
        return false;
    }
    if let Some(init) = initial_depth {
        match depth {
            StepDepth::Into | StepDepth::Undefined => {}
            StepDepth::Over => {
                if at.frame_depth > init {
                    return false;
                }
            }
            StepDepth::Out => {
                if at.frame_depth >= init {
                    return false;
                }
            }
        }
    }
    match size {
        StepSize::Min => true,
        StepSize::Line => {
            let returned = initial_depth.is_some_and(|init| at.frame_depth < init);
            if returned {
                return true;
            }
            if let (Some(init), Some((class_id, method_id, from_line))) =
                (initial_depth, initial_line)
            {
                if at.frame_depth == init && at.class_id == class_id && at.method_id == method_id {
                    return at.line.is_none_or(|line| line != from_line);
                }
            }
            at.line_start != Some(false)
        }
    }
}

/// The classes a method event request is confined to (interpreter round i4
/// wave 93, lane L1; `CRATONVM_JDWP_SCOPED_METHOD_EVENTS`, page
/// `docs/feature-designs/jvmti-delivery-threading.md`):
/// a `MethodEntry` / `MethodExit` event's location is the method's declaring
/// class, so a request whose class filter names a set of classes reports
/// nothing from any other class's methods, which may then stay compiled.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum MethodEventScope {
    /// `ClassOnly`: the class of this id and its subclasses.
    Below(u64),
    /// `ClassMatch` without a wildcard: the classes of this dotted name.
    Named(String),
}

/// The [`MethodEventScope`] of a method event request's `modifiers`, or
/// `None` when it may report from any class (interpreter round i4 wave 93,
/// lane L1). The first `ClassOnly`, or `ClassMatch` naming one class exactly,
/// scopes it, unless a `Count` comes before it: modifiers apply in order
/// ([`location_request_reports`]), so such a `Count` counts the events of
/// every class and must see them all. A `ClassMatch` with a wildcard does not
/// scope (stage 1: a prefix pattern also matches lambda proxy classes, which
/// the class store does not hold), nor does a pattern naming a lambda proxy.
pub fn method_event_scope_of(modifiers: &[EventModifier]) -> Option<MethodEventScope> {
    for m in modifiers {
        match m {
            EventModifier::Count(_) => return None,
            EventModifier::ClassOnly { class_id } => return Some(MethodEventScope::Below(*class_id)),
            EventModifier::ClassMatch { pattern }
                if !pattern.is_empty() && !pattern.contains('*') && !pattern.contains("$$Lambda") =>
            {
                return Some(MethodEventScope::Named(pattern.clone()));
            }
            _ => {}
        }
    }
    None
}

/// JDWP `ClassMatch` / `ClassExclude`: `pattern` (dotted, e.g. `java.*`) is
/// an exact name or begins or ends with `*`; `class_name` is internal (slash)
/// form.
pub(crate) fn class_pattern_matches(pattern: &str, class_name: &str) -> bool {
    let dotted = class_name.replace('/', ".");
    if let Some(prefix) = pattern.strip_suffix('*') {
        dotted.starts_with(prefix)
    } else if let Some(suffix) = pattern.strip_prefix('*') {
        dotted.ends_with(suffix)
    } else {
        dotted == pattern
    }
}

// ---------------------------------------------------------------------------
// Composite event packet
// ---------------------------------------------------------------------------

/// Build a JDWP *composite event* command packet (command set 64, command 100).
///
/// The wire format is:
///   suspend_policy (1 byte)
///   event_count    (4 bytes BE)
///   for each event:
///     event_kind   (1 byte)
///     request_id   (4 bytes BE)
///     thread_id    (8 bytes BE)   — for most event kinds
///     ...extra...
pub fn compose_event_packet(suspend_policy: SuspendPolicy, events: &[Event]) -> JdwpPacket {
    compose_event_packet_with_policy_byte(suspend_policy as u8, events)
}

/// [`compose_event_packet`] with the suspend-policy byte as the event set
/// reports it (interpreter round i1 wave 38, lane L1): the byte a request
/// sent that JDWP does not define (`EventManager::wire_suspend_policy`) where
/// the set was applied as `EVENT_THREAD` for it, as HotSpot's back end
/// reports it (`tools/probes/interp/L1/L1W37RawJdwpUnknownSuspendPolicy.java`).
pub fn compose_event_packet_with_policy_byte(suspend_policy: u8, events: &[Event]) -> JdwpPacket {
    let mut pw = PayloadWriter::new();
    pw.put_u8(suspend_policy);
    pw.put_u32_be(events.len() as u32);
    for evt in events {
        pw.put_u8(evt.kind as u8);
        pw.put_u32_be(evt.request_id);
        // Most event kinds carry a thread ID right after request_id.
        match evt.kind {
            // VMDeath has no thread ID in the spec; nor has ClassUnload
            // (`requestID, signature`: interpreter round i1 wave 25, its
            // first producer).
            EventKind::VMDeath | EventKind::ClassUnload => {}
            // The VM's thread id in its wire form (wave 25): the main
            // thread's is not 0, which JDI reads as a null thread.
            _ => {
                pw.put_u64_be(crate::debug::ids::thread_to_wire(evt.thread_id));
            }
        }
        pw.put_bytes(&evt.extra);
    }

    // Composite events are sent as a *command* from the VM to the debugger:
    //   command_set = 64 (Event), command = 100 (Composite)
    JdwpPacket::Command {
        id: 0, // Caller should assign a proper packet ID.
        flags: 0,
        command_set: 64,
        command: 100,
        data: pw.into_bytes(),
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests;

/// Interpreter round i4 wave 93, lane L1: the class scope of a method event
/// request (`CRATONVM_JDWP_SCOPED_METHOD_EVENTS`) and the exception requests
/// compiled code serves (`CRATONVM_JVMTI_EXCEPTION_FROM_COMPILED` for JDWP).
#[cfg(test)]
mod i93_l1_scoped_request_tests;
