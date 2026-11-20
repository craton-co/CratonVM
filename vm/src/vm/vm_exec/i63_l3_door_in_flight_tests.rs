use super::*;
use crate::runtime::stackwalker::DoorInFlight;

fn record(slot: u32, depth: u32) -> DoorInFlight {
    DoorInFlight {
        key: Some((ClassId::new(7), slot)),
        interp_depth: depth,
        jit_depth: 0,
    }
}

/// A door inside a door restores the outer record on return; a door that
/// noted nothing (switch off, application class) leaves it alone.
#[test]
fn a_door_restores_the_outer_record() {
    let mut thread = Box::new(JvmThread::new(
        crate::threading::ThreadId(63),
        "i63-l3-door",
    ));
    assert_eq!(thread.served_door_in_flight, None);
    let outer = record(1, 2);
    let save_outer: DoorInFlightSave = Some(thread.served_door_in_flight.replace(outer));
    let inner = record(2, 2);
    let save_inner: DoorInFlightSave = Some(thread.served_door_in_flight.replace(inner));
    assert_eq!(thread.served_door_in_flight, Some(inner));
    // An application door in between: nothing noted, nothing restored.
    door_in_flight_leave(&mut thread, None);
    assert_eq!(thread.served_door_in_flight, Some(inner));
    door_in_flight_leave(&mut thread, save_inner);
    assert_eq!(thread.served_door_in_flight, Some(outer));
    door_in_flight_leave(&mut thread, save_outer);
    assert_eq!(thread.served_door_in_flight, None);
}
