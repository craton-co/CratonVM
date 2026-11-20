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
