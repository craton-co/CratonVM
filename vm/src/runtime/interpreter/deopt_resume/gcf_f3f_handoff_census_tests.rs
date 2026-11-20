use super::{door_rerun_census, note_door_rerun, owned_args_handoff_census, DoorRerunCause};

/// A hand-off is counted in its own pair, by verdict, and stays out of
/// the door re-run census (whose rows and warnings are unchanged).
#[test]
fn gcf_f3f_a_handoff_is_counted_apart_from_the_door_reruns() {
    let (e0, u0) = owned_args_handoff_census();
    let rows0 = door_rerun_census();
    note_door_rerun(DoorRerunCause::OwnedArgsHandoff, true);
    note_door_rerun(DoorRerunCause::OwnedArgsHandoff, false);
    let (e1, u1) = owned_args_handoff_census();
    // Other tests may count concurrently: at least ours.
    assert!(e1 > e0 && u1 > u0);
    let rows1 = door_rerun_census();
    assert_eq!(rows0.len(), rows1.len());
    assert!(rows1.iter().all(|(why, _, _)| *why != "owned-args-handoff"));
}
