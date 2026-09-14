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
            90 => Some(Self::VMStart),
            99 => Some(Self::VMDeath),
            _ => None,
        }
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
    /// FieldOnly modifier (JDWP modKind 8): restrict to a specific field.
    FieldOnly { class_id: u64, field_id: u64 },
    /// ConditionalFilter modifier (JDWP modKind 7): expression ID for
    /// conditional breakpoints.
    ConditionalFilter { expr_id: u32 },
}

// ---------------------------------------------------------------------------
// Event request
// ---------------------------------------------------------------------------

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
}

impl EventManager {
    pub fn new() -> Self {
        Self {
            requests: HashMap::new(),
            next_request_id: 1,
        }
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

    /// Whether any breakpoint requests are currently active.
    pub fn has_breakpoints(&self) -> bool {
        self.requests
            .values()
            .any(|r| r.kind == EventKind::Breakpoint)
    }

    /// Whether any single-step requests are currently active.
    pub fn has_single_steps(&self) -> bool {
        self.requests
            .values()
            .any(|r| r.kind == EventKind::SingleStep)
    }

    /// Clear all event requests.
    pub fn clear_all(&mut self) {
        self.requests.clear();
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
    let mut pw = PayloadWriter::new();
    pw.put_u8(suspend_policy as u8);
    pw.put_u32_be(events.len() as u32);
    for evt in events {
        pw.put_u8(evt.kind as u8);
        pw.put_u32_be(evt.request_id);
        // Most event kinds carry a thread ID right after request_id.
        match evt.kind {
            EventKind::VMDeath => {
                // VMDeath has no thread ID in the spec.
            }
            _ => {
                pw.put_u64_be(evt.thread_id);
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
}
