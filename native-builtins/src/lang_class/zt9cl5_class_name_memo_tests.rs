use super::*;

fn slot_with_rows(
    rows: &[(u32, usize)],
) -> std::sync::OnceLock<Box<dyn std::any::Any + Send + Sync>> {
    let slot: std::sync::OnceLock<Box<dyn std::any::Any + Send + Sync>> =
        std::sync::OnceLock::new();
    let caches = fresh_reflect_door_caches();
    {
        let mut memo = caches.class_names.write();
        for &(cid, handle) in rows {
            memo.insert(cid, handle);
        }
    }
    let boxed: Box<dyn std::any::Any + Send + Sync> = Box::new(caches);
    assert!(slot.set(boxed).is_ok());
    slot
}

fn remaining(slot: &std::sync::OnceLock<Box<dyn std::any::Any + Send + Sync>>) -> Vec<u32> {
    let any: &(dyn std::any::Any + Send + Sync) = &**slot.get().expect("made");
    let caches = any.downcast_ref::<ReflectDoorCaches>().expect("caches");
    let mut ids: Vec<u32> = caches.class_names.read().keys().copied().collect();
    ids.sort_unstable();
    ids
}

/// The rows of the unloaded ids go (their handles are answered for the
/// caller to release); a live class's row stays; an id with no row is
/// ignored.
#[test]
fn zt9cl5_unloaded_classes_lose_their_get_name_memo_rows() {
    let slot = slot_with_rows(&[(7, 0x70), (8, 0x80), (9, 0x90)]);
    let mut taken = take_unloaded_class_name_memo_rows(&slot, &[7, 9, 11]);
    taken.sort_unstable();
    assert_eq!(taken, vec![0x70, 0x90]);
    assert_eq!(remaining(&slot), vec![8], "the live class keeps its row");
    assert!(
        take_unloaded_class_name_memo_rows(&slot, &[7, 9]).is_empty(),
        "a second take finds nothing"
    );
}

/// No caches made yet (no `getName()` ever ran through the memo), or no
/// ids: nothing to take, and the slot is not filled.
#[test]
fn zt9cl5_an_unmade_slot_or_no_ids_takes_nothing() {
    let empty: std::sync::OnceLock<Box<dyn std::any::Any + Send + Sync>> =
        std::sync::OnceLock::new();
    assert!(take_unloaded_class_name_memo_rows(&empty, &[1, 2]).is_empty());
    assert!(empty.get().is_none(), "the take must not make the caches");
    let slot = slot_with_rows(&[(3, 0x30)]);
    assert!(take_unloaded_class_name_memo_rows(&slot, &[]).is_empty());
    assert_eq!(remaining(&slot), vec![3]);
}
