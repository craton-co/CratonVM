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
    assert!(mgr
        .match_thread_events_of(EventKind::ThreadStart, 3, true)
        .is_empty());
    assert!(mgr
        .match_thread_events_of(EventKind::ThreadDeath, 3, false)
        .is_empty());
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

/// Interpreter round i5 wave 105 (lane L1): `main`'s end renames its
/// thread filters, so they no longer match VM thread 0 (`DestroyJavaVM`).
#[test]
fn renamed_thread_filters_stop_matching_the_old_thread() {
    let mut mgr = EventManager::new();
    mgr.set_event_request(
        EventKind::ThreadDeath,
        SuspendPolicy::None,
        vec![EventModifier::ThreadOnly { thread_id: 0 }],
    );
    mgr.set_event_request(
        EventKind::ThreadStart,
        SuspendPolicy::None,
        vec![EventModifier::ThreadOnly { thread_id: 9 }],
    );
    assert!(mgr.check_thread_event(EventKind::ThreadDeath, 0).is_some());
    assert_eq!(mgr.rename_thread_filters(0, 77), 1);
    assert!(mgr.check_thread_event(EventKind::ThreadDeath, 0).is_none());
    assert!(mgr.check_thread_event(EventKind::ThreadDeath, 77).is_some());
    assert!(mgr.check_thread_event(EventKind::ThreadStart, 9).is_some(), "others kept");
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

/// Interpreter round i6 wave 119, lane L3: HotSpot's answers after a
/// redefinition cleared a breakpoint (MEASURED, HotSpot 25.0.3,
/// `L3W119RetransformClearsBreakpoints`): the old request and a new one
/// at the same location never fire; deleting all but the last succeeds,
/// the last answers `NOT_FOUND`; then a new request fires.
#[test]
fn i119_l3_a_redefinition_clears_the_breakpoints_of_its_class() {
    let mut mgr = EventManager::new();
    let hits = |mgr: &mut EventManager| {
        mgr.match_location_events(EventKind::Breakpoint, &at(5, 1, 1, None), &|_| false)
            .len()
    };
    let first = mgr.set_event_request(EventKind::Breakpoint, SuspendPolicy::All, vec![location(5)]);
    assert_eq!(hits(&mut mgr), 1);
    assert_eq!(mgr.disarm_class_breakpoints(10), 1);
    assert_eq!(mgr.disarm_class_breakpoints(11), 0, "another class: nothing");
    assert_eq!(hits(&mut mgr), 0);
    assert!(mgr.check_breakpoint(10, 20, 5).is_none());
    assert!(mgr.breakpoint_locations().is_empty());
    let second =
        mgr.set_event_request(EventKind::Breakpoint, SuspendPolicy::All, vec![location(5)]);
    assert_eq!(hits(&mut mgr), 0, "a new request at a cleared location");
    assert!(mgr.clear_event_request_answer(first));
    assert!(!mgr.clear_event_request_answer(second), "the last answers NOT_FOUND");
    let third = mgr.set_event_request(EventKind::Breakpoint, SuspendPolicy::All, vec![location(5)]);
    assert_eq!(hits(&mut mgr), 1);
    assert!(mgr.clear_event_request_answer(third));
    assert!(mgr.clear_event_request_answer(999), "an unknown id is success");
}

/// JDWP `Count`: the first count - 1 reports are suppressed, the count-th
/// is reported, and none after it.
/// Interpreter round i4 wave 85 (lane L1): `InstanceOnly` on a
/// breakpoint reports only when the frame's `this` is the object, and a
/// frame with no `this` (`instance_is` true for every id) passes it, as
/// the back end's NULL `eventInstance` does; a `Count` after it counts
/// only the passing hits (modifiers in order).
#[test]
fn an_instance_filter_on_a_breakpoint_reports_for_its_object_only() {
    let mut mgr = EventManager::new();
    let id = mgr.set_event_request(
        EventKind::Breakpoint,
        SuspendPolicy::None,
        vec![
            location(5),
            EventModifier::InstanceOnly { object_id: 9 },
            EventModifier::Count(2),
        ],
    );
    assert_eq!(mgr.location_instance_filter_ids(), vec![9]);
    let mut hit = |this: Option<u64>| {
        mgr.match_location_events_with_instance(
            EventKind::Breakpoint,
            &at(5, 1, 1, None),
            &|_| false,
            &|object| this.is_none_or(|t| t == object),
        )
    };
    assert!(hit(Some(8)).is_empty(), "another object");
    assert!(hit(Some(9)).is_empty(), "first of a count of 2");
    assert_eq!(hit(None), vec![(id, SuspendPolicy::None)], "no this passes");
    // The plain matcher knows no instance and passes it too.
    let mut mgr = EventManager::new();
    mgr.set_event_request(
        EventKind::Breakpoint,
        SuspendPolicy::None,
        vec![location(5), EventModifier::InstanceOnly { object_id: 9 }],
    );
    assert_eq!(
        mgr.match_location_events(EventKind::Breakpoint, &at(5, 1, 1, None), &|_| false)
            .len(),
        1
    );
}

/// Wave 85 (lane L1; MEASURED on HotSpot 25.0.3, `L1W85EventSetOrder`):
/// co-located requests of one kind report the newest first.
#[test]
fn co_located_requests_report_the_newest_first() {
    let mut mgr = EventManager::new();
    let first = mgr.set_event_request(EventKind::Breakpoint, SuspendPolicy::All, vec![location(5)]);
    let second =
        mgr.set_event_request(EventKind::Breakpoint, SuspendPolicy::All, vec![location(5)]);
    assert_eq!(
        mgr.match_location_events(EventKind::Breakpoint, &at(5, 1, 1, None), &|_| false),
        vec![(second, SuspendPolicy::All), (first, SuspendPolicy::All)]
    );
}

/// Wave 86 (lane L1): a step whose depth JDWP does not define never
/// reports, as the JDK back end never enables single stepping for it.
#[test]
fn a_step_of_an_undefined_depth_never_reports() {
    let mut mgr = EventManager::new();
    mgr.set_event_request(
        EventKind::SingleStep,
        SuspendPolicy::None,
        vec![EventModifier::Step {
            thread_id: 1,
            size: StepSize::Min,
            depth: StepDepth::Undefined,
        }],
    );
    for offset in 0..4 {
        assert!(mgr
            .match_location_events(EventKind::SingleStep, &at(offset, 1, 1, None), &|_| false)
            .is_empty());
    }
}

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
    assert!(
        !reports(in_class(8, "p/Derived", 2, 5), &derived),
        "count spent"
    );

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
        .match_location_events(
            EventKind::MethodEntry,
            &in_class(2, "p/Other", 0, 2),
            &never
        )
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
