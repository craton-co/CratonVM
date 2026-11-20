use super::*;
use crate::runtime::stackwalker::{DoorInFlight, ReflectiveCallRow};

fn record(slot: u32, depth: u32) -> DoorInFlight {
    DoorInFlight {
        key: Some((ClassId::new(11), slot)),
        interp_depth: depth,
        jit_depth: 0,
    }
}

fn row(depth: u32, door_save: DoorInFlightSave) -> ReflectiveCallRow {
    ReflectiveCallRow {
        interp_depth: depth,
        jit_depth: 0,
        kind: cratonvm_native_api::ReflectiveCallKind::MethodInvoke,
        check: None,
        raised: None,
        door_save,
        adapter_pin: None,
    }
}

/// The served `Method.invoke`'s door, noted and withdrawn the way
/// `note_reflective_target` does it: the outer static door's record comes
/// back after the call's door; a call left with its door still noted
/// gives back the record outside the OUTERMOST noted call.
#[test]
fn the_reflective_door_restores_the_record_outside_it() {
    let mut thread = Box::new(JvmThread::new(
        crate::threading::ThreadId(64),
        "i64-l3-door",
    ));
    let outer = record(1, 2);
    thread.served_door_in_flight = Some(outer);
    // `Some` note: the call's own record, the outer one saved on the row.
    let own = record(2, 4);
    let saved = thread.served_door_in_flight.replace(own);
    thread.reflective_calls.push(row(4, Some(saved)));
    assert_eq!(thread.served_door_in_flight, Some(own));
    // `None` note: the save taken back off the row.
    let save = thread
        .reflective_calls
        .last_mut()
        .and_then(|call| call.door_save.take());
    door_in_flight_leave(&mut thread, save);
    assert_eq!(thread.served_door_in_flight, Some(outer));
    assert_eq!(
        thread
            .reflective_calls
            .last()
            .and_then(|call| call.door_save),
        None
    );

    // Two nested calls left without their `None` notes.
    let rows = [
        row(4, Some(Some(outer))),
        row(6, None),
        row(6, Some(Some(own))),
    ];
    assert_eq!(outermost_door_save(&rows), Some(Some(outer)));
    assert_eq!(outermost_door_save(&rows[1..]), Some(Some(own)));
    assert_eq!(outermost_door_save(&rows[1..2]), None);
    assert_eq!(outermost_door_save(&[row(4, Some(None))]), Some(None));
}
