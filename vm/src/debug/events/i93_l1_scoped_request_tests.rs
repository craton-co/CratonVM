use super::*;

fn class_match(pattern: &str) -> EventModifier {
    EventModifier::ClassMatch {
        pattern: pattern.to_string(),
    }
}

#[test]
fn a_class_filter_scopes_a_method_event_request() {
    assert_eq!(
        method_event_scope_of(&[EventModifier::ClassOnly { class_id: 9 }]),
        Some(MethodEventScope::Below(9))
    );
    assert_eq!(
        method_event_scope_of(&[
            EventModifier::ThreadOnly { thread_id: 1 },
            class_match("p.Hot"),
        ]),
        Some(MethodEventScope::Named("p.Hot".to_string()))
    );
    // A wildcard does not scope, but a later `ClassOnly` does.
    assert_eq!(method_event_scope_of(&[class_match("p.*")]), None);
    assert_eq!(
        method_event_scope_of(&[class_match("*.Hot"), EventModifier::ClassOnly { class_id: 4 }]),
        Some(MethodEventScope::Below(4))
    );
    assert_eq!(method_event_scope_of(&[class_match("p.Hot$$Lambda.0x1")]), None);
    assert_eq!(method_event_scope_of(&[]), None);
}

/// A `Count` before the class filter counts the events of every class.
#[test]
fn a_count_before_the_class_filter_keeps_the_request_unscoped() {
    assert_eq!(
        method_event_scope_of(&[EventModifier::Count(3), class_match("p.Hot")]),
        None
    );
    assert_eq!(
        method_event_scope_of(&[class_match("p.Hot"), EventModifier::Count(3)]),
        Some(MethodEventScope::Named("p.Hot".to_string()))
    );
}

#[test]
fn only_covered_scopes_leave_every_method_unconcerned() {
    let mut mgr = EventManager::new();
    let entry = mgr.set_event_request(
        EventKind::MethodEntry,
        SuspendPolicy::None,
        vec![class_match("p.Hot")],
    );
    let hot = MethodEventScope::Named("p.Hot".to_string());
    assert_eq!(mgr.method_event_scopes(), vec![hot.clone()]);
    assert_eq!(mgr.method_events_split(&|_| false), (true, false, false));
    assert_eq!(mgr.method_events_split(&|s| *s == hot), (false, false, true));
    let exit = mgr.set_event_request(EventKind::MethodExit, SuspendPolicy::None, vec![]);
    assert_eq!(mgr.method_events_split(&|s| *s == hot), (false, true, true));
    assert_eq!(mgr.method_events_requested(), (true, true));
    assert!(mgr.clear_event_request(exit));
    assert!(mgr.clear_event_request(entry));
    assert!(mgr.method_event_scopes().is_empty());
    assert_eq!(mgr.method_events_split(&|_| true), (false, false, false));
}

#[test]
fn only_a_quiet_exception_request_is_served_from_compiled_code() {
    let mut mgr = EventManager::new();
    let quiet = mgr.set_event_request(EventKind::Exception, SuspendPolicy::None, vec![]);
    assert!(!mgr.exception_requests_need_the_interpreter());
    let suspending =
        mgr.set_event_request(EventKind::Exception, SuspendPolicy::All, vec![]);
    assert!(mgr.exception_requests_need_the_interpreter());
    assert!(mgr.clear_event_request(suspending));
    let by_instance = mgr.set_event_request(
        EventKind::Exception,
        SuspendPolicy::None,
        vec![EventModifier::InstanceOnly { object_id: 7 }],
    );
    assert!(mgr.exception_requests_need_the_interpreter());
    assert!(mgr.clear_event_request(by_instance));
    assert!(mgr.clear_event_request(quiet));
    assert!(!mgr.exception_requests_need_the_interpreter());
}
