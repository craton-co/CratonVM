use super::{OomeKind, PreallocatedOome};
use crate::types::ObjectRef;

fn fake(addr: usize) -> ObjectRef {
    // SAFETY: non-null, 8-byte aligned, never dereferenced.
    unsafe { ObjectRef::from_raw(addr as *mut u8) }
}

/// Each kind answers exactly HotSpot's message; `Java heap space` (with
/// or without a site detail) is the singleton's, not a kind.
#[test]
fn a_kind_is_found_by_its_exact_message_only() {
    for kind in OomeKind::ALL {
        assert_eq!(OomeKind::for_message(kind.message()), Some(kind));
    }
    assert_eq!(
        OomeKind::for_message("Requested array size exceeds VM limit"),
        Some(OomeKind::ArraySize)
    );
    assert_eq!(
        OomeKind::for_message("Metaspace"),
        Some(OomeKind::Metaspace)
    );
    for other in [
        "Java heap space",
        "Java heap space (alloc_array length 8)",
        "",
        "metaspace",
    ] {
        assert_eq!(OomeKind::for_message(other), None, "{other:?}");
    }
}

/// One slot per kind; the root scan and the remap see every built one.
#[test]
fn every_built_default_is_scanned_and_remapped() {
    let mut table = PreallocatedOome::default();
    assert!(table.values().next().is_none());
    assert_eq!(table.get(OomeKind::Metaspace), None);
    table.set(OomeKind::Metaspace, fake(0x2000));
    assert_eq!(table.get(OomeKind::Metaspace), Some(fake(0x2000)));
    assert_eq!(table.get(OomeKind::ArraySize), None);
    table.set(OomeKind::ArraySize, fake(0x1000));
    assert_eq!(table.values().count(), 2);
    for slot in table.values_mut() {
        if *slot == fake(0x1000) {
            *slot = fake(0x4_0000_1000);
        }
    }
    assert_eq!(table.get(OomeKind::ArraySize), Some(fake(0x4_0000_1000)));
    assert_eq!(table.get(OomeKind::Metaspace), Some(fake(0x2000)));
}
