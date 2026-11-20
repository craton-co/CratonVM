use super::{HeldMonitors, RecordProducer};
use crate::types::ObjectRef;

const P: RecordProducer = RecordProducer::Unnamed;

fn obj(addr: usize) -> ObjectRef {
    // SAFETY: never dereferenced; the record only compares and remaps.
    unsafe { ObjectRef::from_raw(addr as *mut u8) }
}

#[test]
fn an_untrusted_clear_marks_only_a_frame_that_may_hold_a_lock() {
    let mut h = HeldMonitors::new();
    h.clear_untrusted(false, P);
    assert!(!h.is_understated(), "no entry, no monitor bytecode");
    h.clear_untrusted(true, P);
    assert!(h.is_understated(), "monitor bytecode");
    assert!(h.is_empty());
    assert_eq!(h.len(), 0);
    assert!(h.as_slice().is_empty());
    assert_eq!(h.pop(), None, "the mark is never popped");
    let mut held = HeldMonitors::new();
    held.push(obj(0x1000));
    held.clear_untrusted(false, P);
    assert!(held.is_understated(), "it held an entry");
    assert!(held.is_empty());
}

/// Interpreter round i6 wave 121, lane L6: the mark names its producer,
/// with or without the understated bit, and a trusted seed's provenance
/// replaces an untrusted mark (the record is exact again).
#[test]
fn the_mark_names_its_producer() {
    for producer in RecordProducer::ALL {
        let mut h = HeldMonitors::new();
        assert_eq!(h.producer(), None, "an interpreter-entered frame");
        h.mark_understated_by(producer);
        assert_eq!(h.producer(), Some(producer));
        assert!(h.is_understated());
        h.push(obj(0x1000));
        h.set_provenance(producer);
        assert_eq!(h.producer(), Some(producer));
        assert!(!h.is_understated(), "a trusted seed is exact");
        assert_eq!(h.as_slice(), &[obj(0x1000)]);
        h.clear_untrusted(false, producer);
        assert!(h.is_understated(), "it held an entry");
        assert_eq!(h.producer(), Some(producer));
    }
    let mut h = HeldMonitors::new();
    h.set_provenance(RecordProducer::DeoptBuild);
    h.mark_understated();
    assert_eq!(h.producer(), Some(RecordProducer::DeoptBuild), "kept");
}

#[test]
fn the_mark_is_invisible_to_entries_and_remaps() {
    let mut h = HeldMonitors::new();
    h.mark_understated();
    h.mark_understated();
    h.push(obj(0x1000));
    h.push(obj(0x2000));
    assert_eq!(h.len(), 2);
    assert_eq!(h.as_slice(), &[obj(0x1000), obj(0x2000)]);
    assert_eq!(h.count_of(obj(0x1000)), 1);
    h.remap_with(|a| (a == 0x1000).then_some(0x3000));
    assert_eq!(h.as_slice(), &[obj(0x3000), obj(0x2000)]);
    h.remove_at(0);
    assert_eq!(h.as_slice(), &[obj(0x2000)]);
    assert!(h.remove_newest(obj(0x2000)));
    assert!(!h.remove_newest(obj(0x2000)));
    assert!(h.is_empty() && h.is_understated());
    h.clear();
    assert!(!h.is_understated(), "frame reuse forgets the mark");
}
