use super::*;
use crate::recording::RecordingSettings;

#[test]
fn door_and_outcome_names_are_stable() {
    assert_eq!(CompileDoor::MethodEntry.name(), "MethodEntry");
    assert_eq!(CompileDoor::EagerFirstCall.name(), "EagerFirstCall");
    assert_eq!(CompileDoor::Osr.name(), "Osr");
    assert_eq!(CompileOutcome::SinglePass.name(), "SinglePass");
    assert_eq!(CompileOutcome::Optimizing.name(), "Optimizing");
    assert_eq!(CompileOutcome::Refused.name(), "Refused");
}

#[test]
fn static_reason_does_not_allocate_an_arc() {
    // `Str` is the zero-allocation variant and `String` is not; that is the
    // property the `DecisionText` split exists to preserve.
    assert!(matches!(
        DecisionText::Static("scan/wide-iinc").to_event_value(),
        EventValue::Str("scan/wide-iinc")
    ));
    let owned = format!("verdict {}", 1);
    assert!(matches!(
        DecisionText::Borrowed(&owned).to_event_value(),
        EventValue::String(_)
    ));
}

#[test]
fn event_is_registered_and_declares_seven_fields() {
    let fr = crate::create_flight_recorder();
    let id = fr
        .type_registry
        .find_by_name(JIT_COMPILE_DECISION_EVENT)
        .expect("cratonvm.JitCompileDecision must be a registered built-in");
    let et = fr.type_registry.get(id).unwrap();
    assert_eq!(et.fields.len(), 7);
    assert!(et.fields.len() <= crate::event::EVENT_FIELD_INLINE);
    assert_eq!(et.fields[0].name, "method");
    assert_eq!(et.fields[1].name, "door");
    assert_eq!(et.fields[2].name, "outcome");
    assert_eq!(et.fields[3].name, "reason");
}

#[test]
fn a_recording_that_names_no_events_does_not_arm_the_gate() {
    // The default-off contract: "keep everything" is not "arm the
    // diagnostic". Only an explicit `enable(name)` counts.
    let _g = crate::repository::jfr_test_guard();
    let mut fr = crate::create_flight_recorder();
    let rid = fr.new_recording(RecordingSettings::new("no-name-filter"));
    fr.start_recording(rid);
    assert!(!fr.any_running_recording_names_event(JIT_COMPILE_DECISION_EVENT));
    fr.stop_recording(rid);
}

#[test]
fn a_recording_that_names_the_event_is_seen() {
    let _g = crate::repository::jfr_test_guard();
    let mut fr = crate::create_flight_recorder();
    let rid = fr.new_recording(RecordingSettings::new("named"));
    fr.get_recording_mut(rid)
        .unwrap()
        .settings
        .enabled_event_names = Some(
        [JIT_COMPILE_DECISION_EVENT.to_string()]
            .into_iter()
            .collect(),
    );
    fr.start_recording(rid);
    assert!(fr.any_running_recording_names_event(JIT_COMPILE_DECISION_EVENT));
    fr.stop_recording(rid);
    assert!(!fr.any_running_recording_names_event(JIT_COMPILE_DECISION_EVENT));
}
