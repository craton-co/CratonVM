use super::{SettledDoorKey, SettledDoorKeyEntry};
use cratonvm_types::ClassId;

/// The slot answers only for the owner and redefinition count it was
/// filled at; a `None` key (a selection the keyed families do not answer)
/// is remembered as such.
#[test]
fn the_slot_answers_for_its_owner_and_count_only() {
    let slot = SettledDoorKey::new();
    assert_eq!(slot.lookup(ClassId::new(3), 0), None);
    slot.set(Some(SettledDoorKeyEntry {
        owner: ClassId::new(3),
        redefinitions: 5,
        key: Some((ClassId::new(4), 9)),
    }));
    assert_eq!(
        slot.lookup(ClassId::new(3), 5),
        Some(Some((ClassId::new(4), 9)))
    );
    assert_eq!(slot.lookup(ClassId::new(3), 6), None);
    assert_eq!(slot.lookup(ClassId::new(8), 5), None);
    slot.set(Some(SettledDoorKeyEntry {
        owner: ClassId::new(3),
        redefinitions: 6,
        key: None,
    }));
    assert_eq!(slot.lookup(ClassId::new(3), 6), Some(None));
    slot.set(None);
    assert_eq!(slot.get(), None);
}
