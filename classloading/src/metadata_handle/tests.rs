use super::*;

const VM_A: VmId = VmId::from_raw(1);
const VM_B: VmId = VmId::from_raw(2);

/// Resolve against a table of `slots` slots whose named slot is live.
fn resolve_live(
    realm: &MetadataRealm,
    handle: ClassHandle,
    slots: u32,
) -> Result<ClassId, StaleMetadata> {
    realm.resolve_class(handle, slots, true)
}

#[test]
fn live_handle_round_trips() {
    let realm = MetadataRealm::for_vm(VM_A);
    let handle = realm.mint_class(ClassId::new(7)).expect("mint");
    assert_eq!(handle.vm(), VM_A);
    assert_eq!(handle.generation(), 0);
    assert_eq!(handle.class_id_unchecked(), ClassId::new(7));
    assert_eq!(resolve_live(&realm, handle, 16), Ok(ClassId::new(7)));
}

#[test]
fn handle_from_vm_a_does_not_resolve_in_vm_b() {
    let realm_a = MetadataRealm::for_vm(VM_A);
    let realm_b = MetadataRealm::for_vm(VM_B);

    // Both VMs have a class at the same numeric id — the whole point of
    // Fact 1 in `docs/architecture/per-vm-state.md`.
    let handle_a = realm_a.mint_class(ClassId::new(7)).expect("mint");
    let handle_b = realm_b.mint_class(ClassId::new(7)).expect("mint");
    assert_ne!(
        handle_a, handle_b,
        "same ClassId in two VMs must not compare equal"
    );

    // In its own realm each resolves.
    assert!(resolve_live(&realm_a, handle_a, 16).is_ok());
    assert!(resolve_live(&realm_b, handle_b, 16).is_ok());

    // Crossed, both fail, and they fail by *name* rather than by returning
    // the other VM's class.
    let err =
        resolve_live(&realm_b, handle_a, 16).expect_err("VM A handle must not resolve in VM B");
    assert_eq!(err.kind(), StaleMetadataKind::WrongVm);
    assert_eq!(
        err,
        StaleMetadata::WrongVm {
            handle_vm: VM_A,
            realm_vm: VM_B,
            class: ClassId::new(7),
        }
    );
    assert_eq!(
        resolve_live(&realm_a, handle_b, 16).map_err(|e| e.kind()),
        Err(StaleMetadataKind::WrongVm)
    );
}

#[test]
fn handle_stale_by_one_generation_is_rejected_and_says_so() {
    let realm = MetadataRealm::for_vm(VM_A);
    let id = ClassId::new(3);
    let handle = realm.mint_class(id).expect("mint");

    assert_eq!(realm.bump_generation(id), 1);

    let err = resolve_live(&realm, handle, 8).expect_err("stale handle must be rejected");
    assert_eq!(err.kind(), StaleMetadataKind::StaleGeneration);
    assert_eq!(
        err,
        StaleMetadata::StaleGeneration {
            class: id,
            handle_generation: 0,
            live_generation: 1,
        }
    );
    // The message names the class and both generations — this is what a
    // dependency-invalidation sweep logs.
    let rendered = err.to_string();
    assert!(rendered.contains("stale-generation"), "{rendered}");
    assert!(rendered.contains("generation 0"), "{rendered}");

    // A freshly minted handle at the new generation works again.
    let fresh = realm.mint_class(id).expect("re-mint");
    assert_eq!(fresh.generation(), 1);
    assert_eq!(resolve_live(&realm, fresh, 8), Ok(id));
}

#[test]
fn out_of_range_index_is_rejected() {
    let realm = MetadataRealm::for_vm(VM_A);
    let handle = realm.mint_class(ClassId::new(9)).expect("mint");
    let err =
        resolve_live(&realm, handle, 9).expect_err("index 9 with 9 slots is past the end");
    assert_eq!(err.kind(), StaleMetadataKind::OutOfRange);
    assert_eq!(
        err,
        StaleMetadata::OutOfRange {
            kind: MetadataKind::Class,
            class: ClassId::new(9),
            index: 9,
            len: 9,
        }
    );
    // An empty table rejects everything, including id 0.
    assert_eq!(
        realm
            .resolve_class(realm.mint_class(ClassId::new(0)).unwrap(), 0, true)
            .map_err(|e| e.kind()),
        Err(StaleMetadataKind::OutOfRange)
    );
}

#[test]
fn tombstoned_slot_reports_unloaded_not_out_of_range() {
    let realm = MetadataRealm::for_vm(VM_A);
    let id = ClassId::new(2);
    let handle = realm.mint_class(id).expect("mint");
    let err = realm
        .resolve_class(handle, 8, false)
        .expect_err("a tombstoned slot must not resolve");
    assert_eq!(err.kind(), StaleMetadataKind::Unloaded);
    assert_eq!(err, StaleMetadata::Unloaded { class: id });
}

#[test]
fn the_four_failure_kinds_are_distinguishable() {
    let realm_a = MetadataRealm::for_vm(VM_A);
    let realm_b = MetadataRealm::for_vm(VM_B);
    let id = ClassId::new(1);

    let foreign = realm_b.mint_class(id).expect("mint");
    let live = realm_a.mint_class(id).expect("mint");
    realm_a.bump_generation(id);

    let wrong_vm = realm_a.resolve_class(foreign, 8, true).unwrap_err();
    let out_of_range = realm_a
        .resolve_class(realm_a.mint_class(ClassId::new(99)).unwrap(), 8, true)
        .unwrap_err();
    let unloaded = realm_a
        .resolve_class(realm_a.mint_class(id).unwrap(), 8, false)
        .unwrap_err();
    let stale = realm_a.resolve_class(live, 8, true).unwrap_err();

    let kinds = [
        wrong_vm.kind(),
        out_of_range.kind(),
        unloaded.kind(),
        stale.kind(),
    ];
    assert_eq!(
        kinds,
        [
            StaleMetadataKind::WrongVm,
            StaleMetadataKind::OutOfRange,
            StaleMetadataKind::Unloaded,
            StaleMetadataKind::StaleGeneration,
        ]
    );
    // Pairwise distinct — the property the acceptance criterion asks for.
    for i in 0..kinds.len() {
        for j in (i + 1)..kinds.len() {
            assert_ne!(kinds[i], kinds[j], "failure kinds {i} and {j} collide");
        }
    }
    // And the rendered messages are distinct too, so a log line is enough
    // to tell them apart without a debugger.
    let rendered = [
        wrong_vm.to_string(),
        out_of_range.to_string(),
        unloaded.to_string(),
        stale.to_string(),
    ];
    for i in 0..rendered.len() {
        for j in (i + 1)..rendered.len() {
            assert_ne!(rendered[i], rendered[j]);
        }
    }
}

#[test]
fn unbound_realm_binds_once_and_rejects_a_second_vm() {
    let realm = MetadataRealm::new();
    assert!(!realm.is_bound());
    assert_eq!(realm.vm(), UNBOUND_VM);

    assert_eq!(realm.bind_vm(VM_A), Ok(()));
    assert!(realm.is_bound());
    assert_eq!(realm.vm(), VM_A);
    // Idempotent for the same VM.
    assert_eq!(realm.bind_vm(VM_A), Ok(()));
    // A second, different VM is a hard error, not a silent rebind.
    assert_eq!(realm.bind_vm(VM_B), Err(VM_A));
    assert_eq!(realm.vm(), VM_A);
}

#[test]
fn handles_minted_before_binding_fail_closed_after_it() {
    let realm = MetadataRealm::new();
    let early = realm.mint_class(ClassId::new(4)).expect("mint");
    assert_eq!(early.vm(), UNBOUND_VM);
    // Valid while the realm is still unbound...
    assert!(resolve_live(&realm, early, 8).is_ok());
    // ...and rejected the moment it acquires an identity. Fail-closed is
    // the only safe direction: the alternative is a handle that outlives
    // the anonymity it was minted under.
    realm.bind_vm(VM_A).expect("bind");
    assert_eq!(
        resolve_live(&realm, early, 8).map_err(|e| e.kind()),
        Err(StaleMetadataKind::WrongVm)
    );
}

#[test]
fn generation_map_is_untouched_until_the_first_bump() {
    let realm = MetadataRealm::for_vm(VM_A);
    assert_eq!(realm.tracked_classes(), 0);
    for i in 0..64u32 {
        assert_eq!(realm.generation(ClassId::new(i)), 0);
    }
    assert_eq!(
        realm.tracked_classes(),
        0,
        "reads must not populate the map"
    );

    realm.bump_generation(ClassId::new(5));
    assert_eq!(realm.tracked_classes(), 1);
    assert_eq!(realm.generation(ClassId::new(5)), 1);
    assert_eq!(realm.generation(ClassId::new(6)), 0);
}

#[test]
fn bulk_bump_invalidates_every_named_class() {
    let realm = MetadataRealm::for_vm(VM_A);
    let ids: Vec<ClassId> = (0..4).map(ClassId::new).collect();
    let handles: Vec<ClassHandle> = ids
        .iter()
        .map(|id| realm.mint_class(*id).unwrap())
        .collect();

    realm.bump_generations(ids.iter().copied());

    for handle in &handles {
        assert_eq!(
            resolve_live(&realm, *handle, 8).map_err(|e| e.kind()),
            Err(StaleMetadataKind::StaleGeneration),
            "{handle} should have been invalidated"
        );
    }
    // A class outside the bumped set is unaffected.
    let untouched = realm.mint_class(ClassId::new(4)).unwrap();
    assert!(resolve_live(&realm, untouched, 8).is_ok());
}

#[test]
fn an_exhausted_generation_is_poison_in_both_directions() {
    let realm = MetadataRealm::for_vm(VM_A);
    let id = ClassId::new(0);
    let handle = realm.mint_class(id).expect("mint");

    // Drive the counter to the ceiling directly — 2^32 real bumps is not a
    // test we can run.
    realm
        .generations
        .write()
        .insert(id, EXHAUSTED_GENERATION - 1);
    realm.tracked.store(1, Ordering::Release);
    assert_eq!(realm.bump_generation(id), EXHAUSTED_GENERATION);
    // Saturates rather than wrapping back to 0, which would revalidate the
    // generation-0 handle minted above.
    assert_eq!(realm.bump_generation(id), EXHAUSTED_GENERATION);

    assert_eq!(realm.mint_class(id), None, "no new handle at the ceiling");
    assert_eq!(
        resolve_live(&realm, handle, 8).map_err(|e| e.kind()),
        Err(StaleMetadataKind::StaleGeneration)
    );
    // And a fabricated handle *at* the ceiling is rejected too, so the
    // saturation cannot be used to mint a permanently-valid handle.
    let forged = ClassHandle::from_parts(VM_A, EXHAUSTED_GENERATION, id);
    assert_eq!(
        resolve_live(&realm, forged, 8).map_err(|e| e.kind()),
        Err(StaleMetadataKind::StaleGeneration)
    );
}

#[test]
fn method_and_field_handles_carry_the_class_generation() {
    let realm = MetadataRealm::for_vm(VM_A);
    let id = ClassId::new(11);
    let class = realm.mint_class(id).expect("mint");
    let method = MethodHandle::from_parts(class, 3);
    let field = FieldHandle::from_parts(class, 1);

    assert_eq!(method.class(), class);
    assert_eq!(method.index_unchecked(), 3);
    assert_eq!(field.class(), class);
    assert_eq!(field.index_unchecked(), 1);

    realm.bump_generation(id);
    // Both are invalidated through the class handle they embed — the
    // redefine that bumped the generation is exactly what renumbers the
    // methods vec.
    assert_eq!(
        resolve_live(&realm, method.class(), 16).map_err(|e| e.kind()),
        Err(StaleMetadataKind::StaleGeneration)
    );
    assert_eq!(
        resolve_live(&realm, field.class(), 16).map_err(|e| e.kind()),
        Err(StaleMetadataKind::StaleGeneration)
    );
}

#[test]
fn handles_are_usable_as_map_keys_without_aliasing_across_vms() {
    use std::collections::HashMap;

    let realm_a = MetadataRealm::for_vm(VM_A);
    let realm_b = MetadataRealm::for_vm(VM_B);
    let mut cache: HashMap<ClassHandle, &'static str> = HashMap::new();

    cache.insert(realm_a.mint_class(ClassId::new(7)).unwrap(), "A's class 7");
    cache.insert(realm_b.mint_class(ClassId::new(7)).unwrap(), "B's class 7");

    assert_eq!(cache.len(), 2, "the two VMs' class 7 must be distinct keys");
    assert_eq!(
        cache.get(&realm_a.mint_class(ClassId::new(7)).unwrap()),
        Some(&"A's class 7")
    );
}

#[test]
fn display_renders_all_three_components() {
    let realm = MetadataRealm::for_vm(VM_A);
    let class = realm.mint_class(ClassId::new(7)).unwrap();
    assert_eq!(class.to_string(), "vm#0x1/c7@g0");
    let method = MethodHandle::from_parts(class, 2);
    let field = FieldHandle::from_parts(class, 2);
    assert_eq!(method.to_string(), "vm#0x1/c7@g0#m2");
    assert_eq!(field.to_string(), "vm#0x1/c7@g0#f2");
}
