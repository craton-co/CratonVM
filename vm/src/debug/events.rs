// SPDX-License-Identifier: Apache-2.0
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
            90 => Some(Self::VMStart),
            99 => Some(Self::VMDeath),
            _ => None,
        }
    }

    /// Is this a method exit kind (41, or 42 with the return value)?
    pub fn is_method_exit(self) -> bool {
        matches!(self, Self::MethodExit | Self::MethodExitWithReturnValue)
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
}

impl EventManager {
    pub fn new() -> Self {
        Self {
            requests: HashMap::new(),
            next_request_id: 1,
            wire_suspend_policies: HashMap::new(),
            spent: Vec::new(),
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
            if req.kind != EventKind::Breakpoint {
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
    /// an event at `at`, as `(request id, suspend policy)` in request-id
    /// order. This is what the interpreter's suspend point asks before each
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
        let mut ids: Vec<u32> = self
            .requests
            .values()
            .filter(|r| r.kind == kind)
            .map(|r| r.id)
            .collect();
        ids.sort_unstable();
        let mut hits = Vec::new();
        for id in ids {
            let mut was_spent = true;
            if let Some(req) = self.requests.get_mut(&id) {
                was_spent = req.is_spent();
                if location_request_reports(req, at, class_is) {
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

    /// `(class id, method id)` of every method a `Breakpoint` request's
    /// `LocationOnly` modifier names: the methods the dispatch loop and the
    /// JIT must treat as debugged (`debug::DebuggerGates`, wave 7).
    pub fn breakpoint_locations(&self) -> std::collections::HashSet<(u64, u64)> {
        self.requests
            .values()
            .filter(|r| r.kind == EventKind::Breakpoint && r.arms_gates())
            .flat_map(|r| r.modifiers.iter())
            .filter_map(|m| match m {
                EventModifier::LocationOnly {
                    class_id,
                    method_id,
                    ..
                } => Some((*class_id, *method_id)),
                _ => None,
            })
            .collect()
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
        self.match_class_event(EventKind::ClassPrepare, class_name, source_names, thread, class_is)
    }

    /// Does a `ClassPrepare` request filter by source name (interpreter round
    /// i1 wave 43)? Only then are a prepared class's `SourceDebugExtension`
    /// names read ([`Self::match_class_prepare`]).
    pub fn has_source_name_filters(&self) -> bool {
        self.requests.values().any(|r| {
            r.kind == EventKind::ClassPrepare
                && r
                    .modifiers
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
                    EventModifier::SourceNameMatch { pattern } => {
                        source_names
                            .iter()
                            .any(|source| class_pattern_matches(pattern.as_str(), source))
                    }
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
        ids
    }

    /// Clear all event requests.
    pub fn clear_all(&mut self) {
        self.requests.clear();
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
                    && (after_entry
                        || step_reports(*size, *depth, initial_depth, initial_line, at))
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
            // Meaningless on a breakpoint or step (JDWP allows them on
            // `Exception` / field / `ClassPrepare` requests only), and never
            // registered on one.
            EventModifier::ExceptionOnly { .. }
            | EventModifier::InstanceOnly { .. }
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
    let on_step_thread = req.modifiers.iter().any(|m| {
        matches!(m, EventModifier::Step { thread_id, .. } if *thread_id == at.thread_id)
    });
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
                    EventModifier::InstanceOnly { object_id } => {
                        field_event && (subject.instance_is)(*object_id)
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
    if let Some(init) = initial_depth {
        match depth {
            StepDepth::Into => {}
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

/// JDWP `ClassMatch` / `ClassExclude`: `pattern` (dotted, e.g. `java.*`) is
/// an exact name or begins or ends with `*`; `class_name` is internal (slash)
/// form.
fn class_pattern_matches(pattern: &str, class_name: &str) -> bool {
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
mod tests {
    use super::*;

    /// Wave 25: `PlatformThreadsOnly` (JDWP 21, jdb's thread requests)
    /// suppresses a virtual thread's start and death, not a platform one's.
    #[test]
    fn platform_threads_only_suppresses_a_virtual_threads_event() {
        let mut mgr = EventManager::new();
        let id = mgr.set_event_request(
            EventKind::ThreadStart,
            SuspendPolicy::None,
            vec![EventModifier::PlatformThreadsOnly],
        );
        assert_eq!(
            mgr.match_thread_events_of(EventKind::ThreadStart, 3, false),
            vec![(id, SuspendPolicy::None)]
        );
        assert!(mgr.match_thread_events_of(EventKind::ThreadStart, 3, true).is_empty());
        assert!(mgr.match_thread_events_of(EventKind::ThreadDeath, 3, false).is_empty());
    }

    /// Wave 25: a `ClassUnload` request is matched by class name
    /// (`ClassMatch` / `ClassExclude` / `Count`), and its event is
    /// `requestID, signature`, with no thread id.
    #[test]
    fn class_unload_matches_by_name_and_carries_no_thread() {
        let mut mgr = EventManager::new();
        let all = mgr.set_event_request(EventKind::ClassUnload, SuspendPolicy::None, vec![]);
        let only = mgr.set_event_request(
            EventKind::ClassUnload,
            SuspendPolicy::None,
            vec![EventModifier::ClassMatch {
                pattern: "com.example.*".to_string(),
            }],
        );
        let prepare = mgr.set_event_request(EventKind::ClassPrepare, SuspendPolicy::None, vec![]);
        assert_ne!(prepare, all);
        assert_eq!(
            mgr.match_class_unload("com/example/Servlet"),
            vec![(all, SuspendPolicy::None), (only, SuspendPolicy::None)]
        );
        assert_eq!(
            mgr.match_class_unload("org/other/Thing"),
            vec![(all, SuspendPolicy::None)]
        );
        let mut extra = PayloadWriter::new();
        extra.put_string("Lcom/example/Servlet;");
        let event = Event {
            request_id: all,
            kind: EventKind::ClassUnload,
            thread_id: 77,
            extra: extra.into_bytes(),
        };
        let JdwpPacket::Command { data, .. } = compose_event_packet(SuspendPolicy::None, &[event])
        else {
            unreachable!("an event set is a command packet");
        };
        let mut want = PayloadWriter::new();
        want.put_u8(0); // suspend policy NONE
        want.put_u32_be(1);
        want.put_u8(EventKind::ClassUnload as u8);
        want.put_u32_be(all);
        want.put_string("Lcom/example/Servlet;");
        assert_eq!(data, want.into_bytes(), "no thread id after the request id");
    }

    #[test]
    fn set_and_clear_event_request() {
        let mut mgr = EventManager::new();
        let id = mgr.set_event_request(EventKind::Breakpoint, SuspendPolicy::All, vec![]);
        assert_eq!(id, 1);
        assert_eq!(mgr.request_count(), 1);
        assert!(mgr.clear_event_request(id));
        assert_eq!(mgr.request_count(), 0);
    }

    #[test]
    fn clear_nonexistent_returns_false() {
        let mut mgr = EventManager::new();
        assert!(!mgr.clear_event_request(999));
    }

    #[test]
    fn breakpoint_matching() {
        let mut mgr = EventManager::new();
        mgr.set_event_request(
            EventKind::Breakpoint,
            SuspendPolicy::All,
            vec![EventModifier::LocationOnly {
                class_id: 10,
                method_id: 20,
                offset: 30,
            }],
        );

        assert!(mgr.check_breakpoint(10, 20, 30).is_some());
        assert!(mgr.check_breakpoint(10, 20, 31).is_none());
        assert!(mgr.check_breakpoint(10, 21, 30).is_none());
        assert!(mgr.check_breakpoint(11, 20, 30).is_none());
    }

    #[test]
    fn thread_event_matching() {
        let mut mgr = EventManager::new();
        mgr.set_event_request(
            EventKind::ThreadStart,
            SuspendPolicy::EventThread,
            vec![EventModifier::ThreadOnly { thread_id: 5 }],
        );

        assert!(mgr.check_thread_event(EventKind::ThreadStart, 5).is_some());
        assert!(mgr.check_thread_event(EventKind::ThreadStart, 6).is_none());
        assert!(mgr.check_thread_event(EventKind::ThreadDeath, 5).is_none());
    }

    #[test]
    fn thread_event_no_modifier_matches_all() {
        let mut mgr = EventManager::new();
        mgr.set_event_request(EventKind::ThreadStart, SuspendPolicy::None, vec![]);
        assert!(mgr.check_thread_event(EventKind::ThreadStart, 42).is_some());
    }

    #[test]
    fn compose_event_packet_structure() {
        let events = vec![Event {
            request_id: 1,
            kind: EventKind::Breakpoint,
            thread_id: 100,
            extra: vec![],
        }];
        let pkt = compose_event_packet(SuspendPolicy::All, &events);
        match &pkt {
            JdwpPacket::Command {
                command_set,
                command,
                data,
                ..
            } => {
                assert_eq!(*command_set, 64);
                assert_eq!(*command, 100);
                // suspend_policy(1) + count(4) + kind(1) + req_id(4) + thread_id(8) = 18
                assert_eq!(data.len(), 18);
                assert_eq!(data[0], SuspendPolicy::All as u8); // suspend_policy
            }
            _ => panic!("expected Command packet"),
        }
    }

    #[test]
    fn event_kind_from_u8() {
        assert_eq!(EventKind::from_u8(2), Some(EventKind::Breakpoint));
        assert_eq!(EventKind::from_u8(99), Some(EventKind::VMDeath));
        assert_eq!(EventKind::from_u8(255), None);
    }

    #[test]
    fn suspend_policy_from_u8() {
        assert_eq!(SuspendPolicy::from_u8(0), Some(SuspendPolicy::None));
        assert_eq!(SuspendPolicy::from_u8(2), Some(SuspendPolicy::All));
        assert_eq!(SuspendPolicy::from_u8(3), None);
        // Wave 37: what `EventRequest.Set` applies, HotSpot 25.0.3's rows.
        assert_eq!(SuspendPolicy::from_wire(0), SuspendPolicy::None);
        assert_eq!(SuspendPolicy::from_wire(1), SuspendPolicy::EventThread);
        assert_eq!(SuspendPolicy::from_wire(2), SuspendPolicy::All);
        for other in [3u8, 7, 255] {
            assert_eq!(SuspendPolicy::from_wire(other), SuspendPolicy::EventThread);
        }
    }

    #[test]
    fn request_ids_increment() {
        let mut mgr = EventManager::new();
        let a = mgr.set_event_request(EventKind::Breakpoint, SuspendPolicy::All, vec![]);
        let b = mgr.set_event_request(EventKind::SingleStep, SuspendPolicy::EventThread, vec![]);
        assert_eq!(a, 1);
        assert_eq!(b, 2);
    }

    // -----------------------------------------------------------------------
    // T4.8.3 — Step depth differentiation tests
    // -----------------------------------------------------------------------

    #[test]
    fn step_size_from_u32() {
        assert_eq!(StepSize::from_u32(0), Some(StepSize::Min));
        assert_eq!(StepSize::from_u32(1), Some(StepSize::Line));
        assert_eq!(StepSize::from_u32(2), None);
    }

    #[test]
    fn step_depth_from_u32() {
        assert_eq!(StepDepth::from_u32(0), Some(StepDepth::Into));
        assert_eq!(StepDepth::from_u32(1), Some(StepDepth::Over));
        assert_eq!(StepDepth::from_u32(2), Some(StepDepth::Out));
        assert_eq!(StepDepth::from_u32(3), None);
    }

    #[test]
    fn step_into_fires_at_any_depth() {
        let mut mgr = EventManager::new();
        let id = mgr.set_event_request(
            EventKind::SingleStep,
            SuspendPolicy::All,
            vec![EventModifier::Step {
                thread_id: 1,
                size: StepSize::Min,
                depth: StepDepth::Into,
            }],
        );
        mgr.set_initial_frame_depth(id, 3);

        // Same depth
        assert!(mgr.check_single_step_with_depth(1, Some(3)).is_some());
        // Deeper (stepped into a call)
        assert!(mgr.check_single_step_with_depth(1, Some(5)).is_some());
        // Shallower (stepped out)
        assert!(mgr.check_single_step_with_depth(1, Some(1)).is_some());
    }

    #[test]
    fn step_over_fires_at_same_or_lower_depth() {
        let mut mgr = EventManager::new();
        let id = mgr.set_event_request(
            EventKind::SingleStep,
            SuspendPolicy::All,
            vec![EventModifier::Step {
                thread_id: 1,
                size: StepSize::Min,
                depth: StepDepth::Over,
            }],
        );
        mgr.set_initial_frame_depth(id, 3);

        // Same depth — fires
        assert!(mgr.check_single_step_with_depth(1, Some(3)).is_some());
        // Shallower — fires (returned from a call)
        assert!(mgr.check_single_step_with_depth(1, Some(2)).is_some());
        // Deeper — does NOT fire (inside a deeper call)
        assert!(mgr.check_single_step_with_depth(1, Some(4)).is_none());
    }

    #[test]
    fn step_out_fires_only_at_lower_depth() {
        let mut mgr = EventManager::new();
        let id = mgr.set_event_request(
            EventKind::SingleStep,
            SuspendPolicy::All,
            vec![EventModifier::Step {
                thread_id: 1,
                size: StepSize::Min,
                depth: StepDepth::Out,
            }],
        );
        mgr.set_initial_frame_depth(id, 3);

        // Same depth — does NOT fire
        assert!(mgr.check_single_step_with_depth(1, Some(3)).is_none());
        // Deeper — does NOT fire
        assert!(mgr.check_single_step_with_depth(1, Some(5)).is_none());
        // Shallower — fires (returned from the method)
        assert!(mgr.check_single_step_with_depth(1, Some(2)).is_some());
    }

    #[test]
    fn step_without_depth_info_always_fires() {
        let mut mgr = EventManager::new();
        let _id = mgr.set_event_request(
            EventKind::SingleStep,
            SuspendPolicy::All,
            vec![EventModifier::Step {
                thread_id: 1,
                size: StepSize::Line,
                depth: StepDepth::Out,
            }],
        );
        // No initial_frame_depth set, no current depth passed — fires anyway
        assert!(mgr.check_single_step(1).is_some());
        assert!(mgr.check_single_step_with_depth(1, None).is_some());
    }

    #[test]
    fn step_thread_filter_respects_step_modifier() {
        let mut mgr = EventManager::new();
        let id = mgr.set_event_request(
            EventKind::SingleStep,
            SuspendPolicy::All,
            vec![EventModifier::Step {
                thread_id: 42,
                size: StepSize::Min,
                depth: StepDepth::Into,
            }],
        );
        mgr.set_initial_frame_depth(id, 1);

        // Correct thread
        assert!(mgr.check_single_step_with_depth(42, Some(1)).is_some());
        // Wrong thread
        assert!(mgr.check_single_step_with_depth(99, Some(1)).is_none());
    }

    #[test]
    fn step_request_stores_depth_and_size() {
        let mut mgr = EventManager::new();
        let id = mgr.set_event_request(
            EventKind::SingleStep,
            SuspendPolicy::EventThread,
            vec![EventModifier::Step {
                thread_id: 5,
                size: StepSize::Line,
                depth: StepDepth::Over,
            }],
        );
        let req = mgr.get_request(id).unwrap();
        assert_eq!(req.step_depth, Some(StepDepth::Over));
        assert_eq!(req.step_size, Some(StepSize::Line));
        assert_eq!(req.initial_frame_depth, None); // not yet set
    }

    #[test]
    fn set_initial_frame_depth_updates_request() {
        let mut mgr = EventManager::new();
        let id = mgr.set_event_request(
            EventKind::SingleStep,
            SuspendPolicy::All,
            vec![EventModifier::Step {
                thread_id: 1,
                size: StepSize::Min,
                depth: StepDepth::Over,
            }],
        );
        assert_eq!(mgr.get_request(id).unwrap().initial_frame_depth, None);
        mgr.set_initial_frame_depth(id, 7);
        assert_eq!(mgr.get_request(id).unwrap().initial_frame_depth, Some(7));
    }

    // -----------------------------------------------------------------------
    // Interpreter round i1 wave 5 (lane L1) — location matching with modifiers
    // -----------------------------------------------------------------------

    fn at(
        offset: u64,
        thread_id: u64,
        frame_depth: usize,
        line_start: Option<bool>,
    ) -> EventLocation<'static> {
        EventLocation {
            class_id: 10,
            class_name: "com/example/App",
            method_id: 20,
            offset,
            thread_id,
            frame_depth,
            line_start,
            line: None,
        }
    }

    fn location(offset: u64) -> EventModifier {
        EventModifier::LocationOnly {
            class_id: 10,
            method_id: 20,
            offset,
        }
    }

    /// JDWP `Count`: the first count - 1 reports are suppressed, the count-th
    /// is reported, and none after it.
    #[test]
    fn a_count_modifier_reports_once_on_the_nth_hit() {
        let mut mgr = EventManager::new();
        mgr.set_event_request(
            EventKind::Breakpoint,
            SuspendPolicy::None,
            vec![location(5), EventModifier::Count(3)],
        );
        let hits: Vec<usize> = (0..6)
            .map(|_| {
                mgr.match_location_events(EventKind::Breakpoint, &at(5, 1, 1, None), &|_| false)
                    .len()
            })
            .collect();
        assert_eq!(hits, vec![0, 0, 1, 0, 0, 0]);
        // Other locations never reach the count.
        let mut mgr = EventManager::new();
        let id2 = mgr.set_event_request(
            EventKind::Breakpoint,
            SuspendPolicy::All,
            vec![location(5), EventModifier::Count(1)],
        );
        assert!(mgr
            .match_location_events(EventKind::Breakpoint, &at(6, 1, 1, None), &|_| false)
            .is_empty());
        assert_eq!(
            mgr.match_location_events(EventKind::Breakpoint, &at(5, 1, 1, None), &|_| false),
            vec![(id2, SuspendPolicy::All)]
        );
    }

    /// Interpreter round i1 wave 46 (lane L1): a request whose `Count` is
    /// spent stops arming the gates while it stays in the table
    /// (`SPENT_COUNT_RELEASES_GATES`), and the matcher that spent it says so
    /// once.
    #[test]
    fn a_spent_count_releases_the_gates_it_armed() {
        let mut mgr = EventManager::new();
        let entry = mgr.set_event_request(
            EventKind::MethodEntry,
            SuspendPolicy::None,
            vec![EventModifier::Count(2)],
        );
        let bp = mgr.set_event_request(
            EventKind::Breakpoint,
            SuspendPolicy::None,
            vec![location(5), EventModifier::Count(1)],
        );
        assert_eq!(mgr.method_events_requested(), (true, false));
        assert_eq!(mgr.breakpoint_locations().len(), 1);
        // The first entry is counted, not spent.
        assert!(mgr
            .match_location_events(EventKind::MethodEntry, &at(0, 1, 1, None), &|_| false)
            .is_empty());
        assert!(mgr.take_spent().is_empty());
        // The second is reported and spends it; the breakpoint's first hit
        // spends its own.
        assert_eq!(
            mgr.match_location_events(EventKind::MethodEntry, &at(0, 1, 1, None), &|_| false),
            vec![(entry, SuspendPolicy::None)]
        );
        assert_eq!(
            mgr.match_location_events(EventKind::Breakpoint, &at(5, 1, 1, None), &|_| false),
            vec![(bp, SuspendPolicy::None)]
        );
        let spent = mgr.take_spent();
        assert!(mgr.get_request(entry).is_some_and(|r| r.is_spent()));
        assert!(mgr.get_request(bp).is_some_and(|r| r.is_spent()));
        if SPENT_COUNT_RELEASES_GATES {
            assert_eq!(spent, vec![entry, bp]);
            assert_eq!(mgr.method_events_requested(), (false, false));
            assert!(mgr.breakpoint_locations().is_empty());
            assert!(!mgr.has_requests(EventKind::Breakpoint));
        } else {
            assert!(spent.is_empty());
            assert_eq!(mgr.method_events_requested(), (true, false));
            assert_eq!(mgr.breakpoint_locations().len(), 1);
        }
        // Spent for good, and said once.
        assert!(mgr
            .match_location_events(EventKind::MethodEntry, &at(0, 1, 1, None), &|_| false)
            .is_empty());
        assert!(mgr.take_spent().is_empty());
        // Still in the table: `EventRequest.Clear` finds it.
        assert!(mgr.clear_event_request(entry));
    }

    /// `ThreadOnly` restricts a breakpoint to one thread, and a breakpoint
    /// request with no `LocationOnly` never matches.
    #[test]
    fn thread_only_and_location_less_breakpoints() {
        let mut mgr = EventManager::new();
        mgr.set_event_request(
            EventKind::Breakpoint,
            SuspendPolicy::None,
            vec![location(5), EventModifier::ThreadOnly { thread_id: 7 }],
        );
        mgr.set_event_request(EventKind::Breakpoint, SuspendPolicy::None, vec![]);
        assert!(mgr
            .match_location_events(EventKind::Breakpoint, &at(5, 1, 1, None), &|_| false)
            .is_empty());
        assert_eq!(
            mgr.match_location_events(EventKind::Breakpoint, &at(5, 7, 1, None), &|_| false)
                .len(),
            1
        );
    }

    fn step(depth: StepDepth, size: StepSize) -> EventModifier {
        EventModifier::Step {
            thread_id: 1,
            size,
            depth,
        }
    }

    /// Step depth against the recorded starting depth, and LINE size: a LINE
    /// step reports at line starts, anywhere in a caller it returned into,
    /// and everywhere in a method without line information.
    #[test]
    fn step_requests_honour_depth_and_line_size() {
        let mut mgr = EventManager::new();
        let over = mgr.set_event_request(
            EventKind::SingleStep,
            SuspendPolicy::None,
            vec![step(StepDepth::Over, StepSize::Min)],
        );
        mgr.set_initial_frame_depth(over, 3);
        let reports = |mgr: &mut EventManager, loc: EventLocation<'static>| {
            !mgr.match_location_events(EventKind::SingleStep, &loc, &|_| false)
                .is_empty()
        };
        assert!(reports(&mut mgr, at(0, 1, 3, None)), "same depth");
        assert!(!reports(&mut mgr, at(0, 1, 4, None)), "inside a callee");
        assert!(reports(&mut mgr, at(0, 1, 2, None)), "returned");
        assert!(!reports(&mut mgr, at(0, 2, 3, None)), "another thread");
        assert!(mgr.clear_event_request(over));

        let line = mgr.set_event_request(
            EventKind::SingleStep,
            SuspendPolicy::None,
            vec![step(StepDepth::Into, StepSize::Line)],
        );
        mgr.set_initial_frame_depth(line, 3);
        assert!(reports(&mut mgr, at(4, 1, 3, Some(true))), "a line start");
        assert!(!reports(&mut mgr, at(5, 1, 3, Some(false))), "mid-line");
        assert!(reports(&mut mgr, at(5, 1, 4, None)), "no line table: MIN");
        assert!(
            reports(&mut mgr, at(9, 1, 2, Some(false))),
            "mid-line in the caller"
        );
    }

    /// Wave 6: a LINE step that knows its starting line completes, in its own
    /// frame, on the first bytecode of a different line — not at a line start
    /// of the starting line (a one-line loop's back edge), and also in the
    /// middle of another line. Other frames keep the line-start rule.
    #[test]
    fn a_line_step_compares_against_its_starting_line() {
        let mut mgr = EventManager::new();
        let id = mgr.set_event_request(
            EventKind::SingleStep,
            SuspendPolicy::None,
            vec![step(StepDepth::Over, StepSize::Line)],
        );
        mgr.set_initial_frame_depth(id, 3);
        mgr.set_initial_line(id, 10, 20, 7);
        let at_line = |offset: u64, depth: usize, start: bool, line: i32| EventLocation {
            line: Some(line),
            ..at(offset, 1, depth, Some(start))
        };
        let mut reports = |loc: EventLocation<'static>| {
            !mgr.match_location_events(EventKind::SingleStep, &loc, &|_| false)
                .is_empty()
        };
        assert!(
            !reports(at_line(0, 3, true, 7)),
            "back to the start of line 7"
        );
        assert!(!reports(at_line(2, 3, false, 7)), "still on line 7");
        assert!(reports(at_line(9, 3, false, 8)), "mid-line 8");
        assert!(reports(at_line(4, 3, true, 8)), "line 8 starts");
        let other_method = EventLocation {
            method_id: 21,
            ..at_line(0, 3, true, 7)
        };
        assert!(reports(other_method), "another method: its line start");
        assert!(reports(at_line(3, 2, false, 7)), "returned into the caller");
    }

    /// `ClassExclude` / `ClassMatch` patterns, as jdb's step requests send
    /// them (`java.*`, `sun.*`, ...).
    #[test]
    fn class_patterns_filter_step_events() {
        assert!(class_pattern_matches("java.*", "java/lang/String"));
        assert!(!class_pattern_matches("java.*", "javax/swing/JFrame"));
        assert!(class_pattern_matches("*.App", "com/example/App"));
        assert!(class_pattern_matches("com.example.App", "com/example/App"));
        let mut mgr = EventManager::new();
        mgr.set_event_request(
            EventKind::SingleStep,
            SuspendPolicy::None,
            vec![
                step(StepDepth::Into, StepSize::Min),
                EventModifier::ClassExclude {
                    pattern: "com.example.*".to_string(),
                },
            ],
        );
        assert!(mgr
            .match_location_events(EventKind::SingleStep, &at(0, 1, 1, None), &|_| false)
            .is_empty());
    }

    /// Wave 7: the breakpoint methods the dispatch loop and the JIT narrow
    /// their debugger handling to.
    #[test]
    fn breakpoint_locations_name_the_methods_holding_breakpoints() {
        let mut mgr = EventManager::new();
        assert!(mgr.breakpoint_locations().is_empty());
        mgr.set_event_request(
            EventKind::Breakpoint,
            SuspendPolicy::None,
            vec![location(5)],
        );
        mgr.set_event_request(
            EventKind::Breakpoint,
            SuspendPolicy::None,
            vec![location(9)],
        );
        mgr.set_event_request(
            EventKind::SingleStep,
            SuspendPolicy::None,
            vec![step(StepDepth::Into, StepSize::Min)],
        );
        let at = mgr.breakpoint_locations();
        assert_eq!(at.len(), 1, "one method, two offsets: {at:?}");
        assert!(at.contains(&(10, 20)));
    }

    /// Wave 7: `ClassPrepare` requests honour `ClassMatch`, `ClassExclude`
    /// and `Count`, and one that needs the preparing thread never reports.
    #[test]
    fn class_prepare_requests_filter_by_class_pattern() {
        let mut mgr = EventManager::new();
        let app = mgr.set_event_request(
            EventKind::ClassPrepare,
            SuspendPolicy::All,
            vec![EventModifier::ClassMatch {
                pattern: "com.example.*".to_string(),
            }],
        );
        let once = mgr.set_event_request(
            EventKind::ClassPrepare,
            SuspendPolicy::None,
            vec![
                EventModifier::ClassExclude {
                    pattern: "java.*".to_string(),
                },
                EventModifier::Count(1),
            ],
        );
        mgr.set_event_request(
            EventKind::ClassPrepare,
            SuspendPolicy::None,
            vec![EventModifier::ThreadOnly { thread_id: 1 }],
        );
        assert_eq!(
            mgr.match_class_prepare("java/lang/Foo", &[], &|_| false),
            Vec::<(u32, SuspendPolicy)>::new()
        );
        assert_eq!(
            mgr.match_class_prepare("com/example/App", &[], &|_| false),
            vec![(app, SuspendPolicy::All), (once, SuspendPolicy::None)]
        );
        assert_eq!(
            mgr.match_class_prepare("com/example/Other", &[], &|_| false),
            vec![(app, SuspendPolicy::All)],
            "the Count(1) request is spent"
        );
    }

    /// Wave 23: `SourceNameMatch` reports the classes whose source name
    /// matches (exact, `*` prefix or suffix); a class without a source name
    /// does not match.
    #[test]
    fn class_prepare_requests_filter_by_source_name() {
        let mut mgr = EventManager::new();
        let kotlin = mgr.set_event_request(
            EventKind::ClassPrepare,
            SuspendPolicy::None,
            vec![EventModifier::SourceNameMatch {
                pattern: "*.kt".to_string(),
            }],
        );
        let exact = mgr.set_event_request(
            EventKind::ClassPrepare,
            SuspendPolicy::None,
            vec![EventModifier::SourceNameMatch {
                pattern: "Main.java".to_string(),
            }],
        );
        assert_eq!(
            mgr.match_class_prepare("p/AppKt", &["App.kt"], &|_| false),
            vec![(kotlin, SuspendPolicy::None)]
        );
        assert_eq!(
            mgr.match_class_prepare("p/Main", &["Main.java"], &|_| false),
            vec![(exact, SuspendPolicy::None)]
        );
        assert!(mgr.match_class_prepare("p/Gen", &[], &|_| false).is_empty());
        // Wave 43: any of the names matches (a `SourceDebugExtension`'s
        // file names after the `SourceFile`).
        assert_eq!(
            mgr.match_class_prepare("p/HolderKt", &["Holder.java", "Inline.kt"], &|_| false),
            vec![(kotlin, SuspendPolicy::None)]
        );
        assert!(mgr.has_source_name_filters());
        assert!(!EventManager::new().has_source_name_filters());
        let mut thread_events = EventManager::new();
        thread_events.set_event_request(
            EventKind::ThreadStart,
            SuspendPolicy::None,
            vec![EventModifier::SourceNameMatch {
                pattern: "*".to_string(),
            }],
        );
        assert!(
            thread_events
                .match_thread_events(EventKind::ThreadStart, 1)
                .is_empty(),
            "meaningless outside ClassPrepare"
        );
    }

    // -----------------------------------------------------------------------
    // T6.4.4 — Field watchpoint event tests
    // -----------------------------------------------------------------------

    #[test]
    fn t644_field_access_event_kind_roundtrip() {
        assert_eq!(EventKind::from_u8(20), Some(EventKind::FieldAccess));
        assert_eq!(EventKind::FieldAccess as u8, 20);
    }

    #[test]
    fn t644_field_modification_event_kind_roundtrip() {
        assert_eq!(EventKind::from_u8(21), Some(EventKind::FieldModification));
        assert_eq!(EventKind::FieldModification as u8, 21);
    }

    #[test]
    fn t644_field_watchpoint_matching() {
        let mut mgr = EventManager::new();
        mgr.set_event_request(
            EventKind::FieldAccess,
            SuspendPolicy::All,
            vec![EventModifier::FieldOnly {
                class_id: 10,
                field_id: 3,
            }],
        );

        assert!(mgr
            .check_field_watchpoint(EventKind::FieldAccess, 10, 3)
            .is_some());
        assert!(mgr
            .check_field_watchpoint(EventKind::FieldAccess, 10, 4)
            .is_none());
        assert!(mgr
            .check_field_watchpoint(EventKind::FieldAccess, 11, 3)
            .is_none());
        // Wrong kind
        assert!(mgr
            .check_field_watchpoint(EventKind::FieldModification, 10, 3)
            .is_none());
    }

    #[test]
    fn t644_has_field_watchpoints() {
        let mut mgr = EventManager::new();
        assert!(!mgr.has_field_access_watchpoints());
        assert!(!mgr.has_field_modification_watchpoints());

        mgr.set_event_request(
            EventKind::FieldAccess,
            SuspendPolicy::All,
            vec![EventModifier::FieldOnly {
                class_id: 1,
                field_id: 1,
            }],
        );
        assert!(mgr.has_field_access_watchpoints());
        assert!(!mgr.has_field_modification_watchpoints());

        mgr.set_event_request(
            EventKind::FieldModification,
            SuspendPolicy::EventThread,
            vec![EventModifier::FieldOnly {
                class_id: 2,
                field_id: 2,
            }],
        );
        assert!(mgr.has_field_modification_watchpoints());
    }

    // -----------------------------------------------------------------------
    // Interpreter round i1 wave 24 (lane L1)
    // -----------------------------------------------------------------------

    /// A location in `class_name` (class id `class_id`) at `offset`, frame
    /// depth `depth`, on thread 1, with line information.
    fn in_class(
        class_id: u64,
        class_name: &'static str,
        offset: u64,
        depth: usize,
    ) -> EventLocation<'static> {
        EventLocation {
            class_id,
            class_name,
            method_id: 20 + class_id,
            offset,
            thread_id: 1,
            frame_depth: depth,
            line_start: Some(offset == 0),
            line: Some(1),
        }
    }

    /// `ClassOnly` is evaluated on location events: a step filtered to a
    /// class reports only in it or a subtype (`class_is`), and a breakpoint
    /// request with one only there.
    #[test]
    fn a_class_only_filter_restricts_steps_and_breakpoints() {
        let mut mgr = EventManager::new();
        let id = mgr.set_event_request(
            EventKind::SingleStep,
            SuspendPolicy::None,
            vec![
                step(StepDepth::Over, StepSize::Min),
                EventModifier::ClassOnly { class_id: 7 },
            ],
        );
        mgr.set_initial_frame_depth(id, 3);
        let base_or_sub = |id: u64| id == 7;
        let never = |_: u64| false;
        assert!(mgr
            .match_location_events(EventKind::SingleStep, &in_class(9, "p/Util", 4, 3), &never)
            .is_empty());
        assert_eq!(
            mgr.match_location_events(
                EventKind::SingleStep,
                &in_class(8, "p/Derived", 4, 3),
                &base_or_sub
            ),
            vec![(id, SuspendPolicy::None)]
        );
    }

    /// HotSpot's method-entry mode of a step INTO (`StepEntryMode`): a step
    /// into a method its class filters reject goes quiet until an accepted
    /// method is ENTERED, and completes at the first location after that
    /// entry — the method's second bytecode, whatever its line — as
    /// `L1W24JdiSurface`'s `Step at Derived.m:37@1` shows on HotSpot. A
    /// return into the stepping frame resumes plain stepping.
    #[test]
    fn a_step_into_through_a_filtered_method_completes_after_the_next_entry() {
        let mut mgr = EventManager::new();
        let id = mgr.set_event_request(
            EventKind::SingleStep,
            SuspendPolicy::All,
            vec![
                step(StepDepth::Into, StepSize::Line),
                EventModifier::ClassOnly { class_id: 7 },
                EventModifier::Count(1),
            ],
        );
        mgr.set_initial_frame_depth(id, 3);
        // Only `p/Derived` (id 8) is a subtype of the filter's class 7.
        let class_is = |filter: u64| filter == 7;
        let derived = |filter: u64| class_is(filter);
        let not_base = |_: u64| false;
        let mut reports = |at: EventLocation<'static>, is: &dyn Fn(u64) -> bool| {
            !mgr.match_location_events(EventKind::SingleStep, &at, is)
                .is_empty()
        };
        // The stepping frame itself (filtered): nothing, stepping goes on.
        assert!(!reports(in_class(1, "p/Main", 5, 3), &not_base));
        // Into `Util.route` (filtered): method-entry mode.
        assert!(!reports(in_class(9, "p/Util", 0, 4), &not_base));
        assert!(!reports(in_class(9, "p/Util", 3, 4), &not_base));
        // `Derived.m` entered: the entry itself is not reported...
        assert!(!reports(in_class(8, "p/Derived", 0, 5), &derived));
        // ...its next bytecode is, mid-line, with the Count spent there.
        assert!(reports(in_class(8, "p/Derived", 1, 5), &derived));
        assert!(!reports(in_class(8, "p/Derived", 2, 5), &derived), "count spent");

        // A filtered callee that returns: plain stepping in the frame again.
        let mut mgr = EventManager::new();
        let id = mgr.set_event_request(
            EventKind::SingleStep,
            SuspendPolicy::All,
            vec![
                step(StepDepth::Into, StepSize::Min),
                EventModifier::ClassExclude {
                    pattern: "java.*".to_string(),
                },
            ],
        );
        mgr.set_initial_frame_depth(id, 3);
        let mut reports = |at: EventLocation<'static>| {
            !mgr.match_location_events(EventKind::SingleStep, &at, &|_| false)
                .is_empty()
        };
        assert!(!reports(in_class(2, "java/util/List", 0, 4)), "excluded");
        assert!(!reports(in_class(2, "java/util/List", 7, 4)), "still in it");
        assert!(reports(in_class(1, "p/Main", 9, 3)), "back in the frame");
        // Unfiltered stepping into a method still stops at its first bytecode.
        assert!(reports(in_class(1, "p/Main", 0, 4)), "a plain step into");
    }

    /// `MethodEntry` / `MethodExit(WithReturnValue)` requests are matched
    /// like location events (class patterns, `ThreadOnly`, `Count`), and
    /// `method_events_requested` says which of them are in force.
    #[test]
    fn method_entry_and_exit_requests_match_like_location_events() {
        let mut mgr = EventManager::new();
        assert_eq!(mgr.method_events_requested(), (false, false));
        let entry = mgr.set_event_request(
            EventKind::MethodEntry,
            SuspendPolicy::None,
            vec![
                EventModifier::ThreadOnly { thread_id: 1 },
                EventModifier::ClassMatch {
                    pattern: "p.Main".to_string(),
                },
            ],
        );
        assert_eq!(mgr.method_events_requested(), (true, false));
        let exit = mgr.set_event_request(
            EventKind::MethodExitWithReturnValue,
            SuspendPolicy::EventThread,
            vec![EventModifier::Count(2)],
        );
        assert_eq!(mgr.method_events_requested(), (true, true));
        assert_eq!(EventKind::from_u8(40), Some(EventKind::MethodEntry));
        assert_eq!(EventKind::from_u8(41), Some(EventKind::MethodExit));
        assert_eq!(
            EventKind::from_u8(42),
            Some(EventKind::MethodExitWithReturnValue)
        );
        assert!(EventKind::MethodExitWithReturnValue.is_method_exit());
        let never = |_: u64| false;
        assert_eq!(
            mgr.match_location_events(EventKind::MethodEntry, &in_class(1, "p/Main", 0, 2), &never),
            vec![(entry, SuspendPolicy::None)]
        );
        assert!(mgr
            .match_location_events(EventKind::MethodEntry, &in_class(2, "p/Other", 0, 2), &never)
            .is_empty());
        let exits: Vec<usize> = (0..3)
            .map(|_| {
                mgr.match_location_events(
                    EventKind::MethodExitWithReturnValue,
                    &in_class(1, "p/Main", 4, 2),
                    &never,
                )
                .len()
            })
            .collect();
        assert_eq!(exits, vec![0, 1, 0], "Count(2): the second exit only");
        assert!(mgr.clear_event_request(exit));
        assert_eq!(mgr.method_events_requested(), (true, false));
    }

    /// `ClassOnly` on a `ClassPrepare` request: only the named class and
    /// its subtypes (the prepared class's supertypes, `class_is`).
    #[test]
    fn a_class_only_filter_restricts_class_prepare() {
        let mut mgr = EventManager::new();
        let id = mgr.set_event_request(
            EventKind::ClassPrepare,
            SuspendPolicy::None,
            vec![EventModifier::ClassOnly { class_id: 5 }],
        );
        assert_eq!(mgr.filter_class_ids(EventKind::ClassPrepare), vec![5]);
        assert!(mgr
            .match_class_prepare("p/Unrelated", &[], &|_| false)
            .is_empty());
        assert_eq!(
            mgr.match_class_prepare("p/Square", &[], &|filter| filter == 5),
            vec![(id, SuspendPolicy::None)]
        );
    }
}
