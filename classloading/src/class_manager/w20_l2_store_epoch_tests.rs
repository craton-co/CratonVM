use super::*;

/// A definition moves its own store's slot and the process counters; a
/// store in another slot is a different cell (interpreter round i1 wave
/// 20, lane L2).
#[test]
fn a_definition_moves_its_own_stores_slot_and_the_process_counters() {
    let mut a = ClassManager::new(&[], &[], &[]);
    let domain = a.class_store().layout_domain();
    let slot = store_epochs(domain);
    let (slot_def, slot_name) = (slot.class_definition_epoch(), slot.class_name_generation());
    let (process_def, process_name) = (class_definition_epoch(), class_name_generation());
    let name = "w20l2/StoreEpochProbe";
    a.register_class_name(ClassLoaderId::Application, name, ClassId::new(900_201));
    assert!(slot.class_definition_epoch() > slot_def);
    assert!(class_definition_epoch() > process_def);
    // A second id under the same name changes an existing answer.
    a.register_class_name(ClassLoaderId::Bootstrap, name, ClassId::new(900_202));
    assert!(slot.class_name_generation() > slot_name);
    assert!(class_name_generation() > process_name);
}

/// The slot is the layout domain masked: domains one table apart share a
/// cell, neighbours do not, and every slot index is in range.
#[test]
fn store_slots_are_the_layout_domain_masked() {
    let d = 7u32;
    assert_eq!(
        store_epoch_slot(d),
        store_epoch_slot(d + STORE_EPOCH_SLOTS as u32)
    );
    assert_ne!(store_epoch_slot(d), store_epoch_slot(d + 1));
    assert!(std::ptr::eq(
        store_epochs(d),
        store_epochs_at(store_epoch_slot(d))
    ));
    assert!(!std::ptr::eq(store_epochs(d), store_epochs(d + 1)));
    assert!(std::ptr::eq(
        store_epochs_at(STORE_EPOCH_SLOTS + 3),
        store_epochs_at(3)
    ));
}

/// The fan-out bump reaches every slot.
#[test]
fn the_every_store_bump_moves_every_slot() {
    let before: Vec<u64> = (0..STORE_EPOCH_SLOTS)
        .map(|s| store_epochs_at(s).resolution_epoch())
        .collect();
    let before_global: Vec<u64> = (0..STORE_EPOCH_SLOTS)
        .map(|s| store_epochs_at(s).resolution_global_epoch())
        .collect();
    bump_every_store_resolution_epoch();
    for (s, b) in before.iter().enumerate() {
        assert!(store_epochs_at(s).resolution_epoch() > *b, "slot {s}");
    }
    // t1 INTV (`CRATONVM_SITE_GLOBAL_RESOLUTION_STAMP`): the fan-out also
    // retires every global-mode site entry.
    for (s, b) in before_global.iter().enumerate() {
        assert!(
            store_epochs_at(s).resolution_global_epoch() > *b,
            "slot {s}: global counter"
        );
    }
}

/// t1 INTV: the ordinary bump moves BOTH resolution counters (the safe
/// default), and only the loader-row bump leaves the global one alone.
/// Asserted on a private `StoreEpochs`, so concurrent tests that bump the
/// shared slots cannot move the cells under the comparison.
#[test]
fn t1_intv_the_default_resolution_bump_moves_the_global_counter_too() {
    let e = StoreEpochs::new();
    assert_eq!((e.resolution_epoch(), e.resolution_global_epoch()), (0, 0));
    e.bump_resolution_epoch();
    assert_eq!((e.resolution_epoch(), e.resolution_global_epoch()), (1, 1));
    e.bump_resolution_epoch_loader_row_only();
    e.bump_resolution_epoch_loader_row_only();
    assert_eq!(
        (e.resolution_epoch(), e.resolution_global_epoch()),
        (3, 1),
        "a loader row moves the ordinary counter only"
    );
    e.bump_resolution_epoch();
    assert_eq!((e.resolution_epoch(), e.resolution_global_epoch()), (4, 2));
}
