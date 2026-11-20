use super::*;

fn caches_of(
    slot: &std::sync::OnceLock<Box<dyn std::any::Any + Send + Sync>>,
) -> &ReflectDoorCaches {
    (**slot.get_or_init(new_reflect_door_caches))
        .downcast_ref::<ReflectDoorCaches>()
        .expect("door caches")
}

/// Four distinct bits: each bridge under each user-loader state.
#[test]
fn each_bridge_and_loader_state_has_its_own_bit() {
    let bits = [
        name_bridge_verdict_bit(false, false),
        name_bridge_verdict_bit(true, false),
        name_bridge_verdict_bit(false, true),
        name_bridge_verdict_bit(true, true),
    ];
    assert_eq!(bits, [0b0001, 0b0010, 0b0100, 0b1000]);
}

/// A pass recorded for one bridge / state is not read by another; a forget
/// drops the class; the unload sweep drops dead class ids only.
#[test]
fn verdicts_are_per_bridge_per_state_and_pruned_on_unload() {
    let slot: std::sync::OnceLock<Box<dyn std::any::Any + Send + Sync>> =
        std::sync::OnceLock::new();
    let caches = caches_of(&slot);
    let memo = &caches.name_bridge_verdicts;
    let (a, b) = (ClassId::new(7), ClassId::new(8));
    let simple = name_bridge_verdict_bit(false, false);
    let canonical_loaders = name_bridge_verdict_bit(true, true);
    assert!(!name_bridge_verdicts_hold(memo, a, simple));
    name_bridge_verdicts_insert(memo, a, simple);
    assert!(name_bridge_verdicts_hold(memo, a, simple));
    assert!(!name_bridge_verdicts_hold(memo, a, canonical_loaders));
    name_bridge_verdicts_insert(memo, a, canonical_loaders);
    assert!(name_bridge_verdicts_hold(memo, a, simple));
    assert!(name_bridge_verdicts_hold(memo, a, canonical_loaders));
    name_bridge_verdicts_insert(memo, b, simple);
    forget_unloaded_reflect_door_rows(&slot, &|id| id == 7);
    assert!(!name_bridge_verdicts_hold(memo, a, simple));
    assert!(name_bridge_verdicts_hold(memo, b, simple));
    name_bridge_verdicts_forget(memo, b);
    assert!(memo.read().is_empty());
}


