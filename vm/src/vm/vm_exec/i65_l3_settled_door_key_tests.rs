use super::*;
use crate::runtime::stackwalker::DoorInFlight;

/// A settling door's hit writes the same record a note would (the key,
/// the thread's depths) and hands back the outer record, which the
/// door's return restores: the slot changes the lookup, not the record.
#[test]
fn a_kept_key_writes_the_record_a_note_writes() {
    let mut thread = Box::new(JvmThread::new(
        crate::threading::ThreadId(65),
        "i65-l3-door",
    ));
    let outer = DoorInFlight {
        key: Some((ClassId::new(5), 1)),
        interp_depth: 0,
        jit_depth: 0,
    };
    thread.served_door_in_flight = Some(outer);
    let count = cratonvm_classloading::class_redefinition_count();
    let slot = cratonvm_native_api::SettledDoorKey::new();
    slot.set(Some(cratonvm_native_api::SettledDoorKeyEntry {
        owner: ClassId::new(9),
        redefinitions: count,
        key: Some((ClassId::new(9), 4)),
    }));
    let key = slot
        .lookup(ClassId::new(9), count)
        .expect("kept for its owner at the count");
    let save: DoorInFlightSave = Some(door_in_flight_store(&mut thread, key));
    let written = thread.served_door_in_flight.expect("a record");
    assert_eq!(written.key, Some((ClassId::new(9), 4)));
    assert_eq!(
        written.interp_depth,
        u32::try_from(thread.frames.len()).unwrap()
    );
    door_in_flight_leave(&mut thread, save);
    assert_eq!(thread.served_door_in_flight, Some(outer));
    // Another owner's door, or a moved count, never takes the kept key.
    assert_eq!(slot.lookup(ClassId::new(10), count), None);
    assert_eq!(slot.lookup(ClassId::new(9), count.wrapping_add(1)), None);
}
