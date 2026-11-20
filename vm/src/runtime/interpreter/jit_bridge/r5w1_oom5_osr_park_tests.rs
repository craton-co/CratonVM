use super::park_osr_entry_reference_locals;
use crate::config::VmConfig;
use crate::runtime::frame::Frame;
use crate::types::Value;
use crate::vm::SharedVm;
use cratonvm_types::ClassId;

fn frame_with(args: &[Value]) -> Frame {
    Frame::new(
        ClassId::new(0),
        "cratonvm/test/OsrPark".to_string(),
        "m".to_string(),
        "(Ljava/lang/Object;I)V".to_string(),
        None,
        vec![0xb1],
        vec![],
        1,
        3,
        args,
    )
}

/// Only the non-null reference locals are taken out, and a declined
/// entry (nothing ran, no collection since) puts them back exactly.
#[test]
fn parking_takes_only_references_and_a_declined_entry_restores_them() {
    let shared = SharedVm::new(VmConfig::default());
    let obj = shared.mem.heap.alloc_object(ClassId::new(0), 0);
    let mut frame = frame_with(&[Value::Object(Some(obj)), Value::Int(7)]);
    let parked = park_osr_entry_reference_locals(&shared, &mut frame, |_| false)
        .expect("one reference local to park");
    assert_eq!(frame.get_local_unchecked(0), Value::Object(None));
    assert_eq!(frame.get_local_unchecked(1), Value::Int(7));
    assert!(parked.restore_after_declined_entry(&shared, &mut frame));
    assert_eq!(frame.get_local_unchecked(0), Value::Object(Some(obj)));
    assert_eq!(frame.get_local_unchecked(1), Value::Int(7));
}

/// A frame with no reference local parks nothing, so the entry's replay
/// arms keep their old answers.
#[test]
fn a_frame_without_references_parks_nothing() {
    let shared = SharedVm::new(VmConfig::default());
    let mut frame = frame_with(&[Value::Object(None), Value::Int(3)]);
    assert!(park_osr_entry_reference_locals(&shared, &mut frame, |_| false).is_none());
    assert_eq!(frame.get_local_unchecked(0), Value::Object(None));
}

/// gcf f2/f (`gcf-f1f-osr-entry-park-voids-the-entry-holds-snapshot`): a
/// slot the entry's lock snapshot reads back stays in place, and the rest
/// are parked as before.
#[test]
fn gcf_f2f_parking_leaves_the_slots_the_lock_snapshot_reads() {
    let shared = SharedVm::new(VmConfig::default());
    let lock = shared.mem.heap.alloc_object(ClassId::new(0), 0);
    let mut frame = frame_with(&[Value::Object(Some(lock)), Value::Int(7)]);
    assert!(
        park_osr_entry_reference_locals(&shared, &mut frame, |i| i == 0).is_none(),
        "the one reference is the snapshot's: nothing to park"
    );
    assert_eq!(frame.get_local_unchecked(0), Value::Object(Some(lock)));
    // Slot 2 is a second reference the snapshot does not read.
    let other = shared.mem.heap.alloc_object(ClassId::new(0), 0);
    frame.set_local_unchecked(2, Value::Object(Some(other)));
    let parked = park_osr_entry_reference_locals(&shared, &mut frame, |i| i == 0)
        .expect("the other reference is parked");
    assert_eq!(frame.get_local_unchecked(0), Value::Object(Some(lock)));
    assert_eq!(frame.get_local_unchecked(2), Value::Object(None));
    assert!(parked.restore_after_declined_entry(&shared, &mut frame));
    assert_eq!(frame.get_local_unchecked(2), Value::Object(Some(other)));
}
