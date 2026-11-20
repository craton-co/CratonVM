use super::*;
use crate::config::VmConfig;
use crate::vm::Vm;

#[test]
fn a_display_name_is_exactly_the_dotted_internal_name() {
    for (internal, display, want) in [
        ("java/lang/String", "java.lang.String", true),
        ("java/lang/String", "java.lang.Strin", false),
        ("java/lang/String", "java/lang/String", false),
        ("Main$Inner", "Main$Inner", true),
        ("a/b", "a.c", false),
        ("", "", true),
    ] {
        assert_eq!(
            internal_name_spells_display(internal, display),
            internal.replace('/', ".") == display,
            "{internal} {display}"
        );
        assert_eq!(internal_name_spells_display(internal, display), want);
    }
}

/// Only an NPE in the constructor's state (`extendedMessageState == 1`,
/// no messages) takes the mark, only a marked one answers a deferred
/// message, and once the JDK has stored an answer (state 2) the native
/// answers as before.
#[test]
fn only_a_marked_npe_answers_a_deferred_message() {
    let mut vm = Vm::new(VmConfig::default());
    // The mark first: its allocation then moves nothing this test holds.
    if try_create_java_string(&vm.shared, DEFERRED_NPE_MESSAGE_MARK).is_none() {
        return;
    }
    let Ok(npe) = create_exception_object(
        &vm.shared,
        &mut vm.main_thread,
        "java/lang/NullPointerException",
        None,
    ) else {
        return;
    };
    let shared = &vm.shared;
    let Some((extended_slot, state_slot)) =
        npe_extended_message_slots(shared, shared.mem.heap.class_id_of(npe))
    else {
        // A class library without the JDK's fields: nothing to defer.
        return;
    };
    assert_eq!(deferred_npe_message(shared, npe), None);
    shared.mem.heap.set_field(npe, state_slot, Value::Int(1));
    shared
        .mem
        .heap
        .set_field(npe, extended_slot, Value::Object(None));
    assert!(mark_npe_message_deferred(shared, npe));
    assert!(deferred_npe_message(shared, npe).is_some());
    // Marked twice is refused: the slot is no longer empty.
    assert!(!mark_npe_message_deferred(shared, npe));
    shared
        .mem
        .heap
        .set_field(npe, extended_slot, Value::Object(None));
    shared.mem.heap.set_field(npe, state_slot, Value::Int(2));
    assert!(!mark_npe_message_deferred(shared, npe));
    assert_eq!(deferred_npe_message(shared, npe), None);
}
