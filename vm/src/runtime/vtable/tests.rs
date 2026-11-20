use super::*;

/// JIT r24 (proxy-define page): the lazy key round-trips, and method 0 of
/// 4096 classes -- the define row's shape, one class per fresh loader --
/// spreads over the buckets of a table that size (Fx keeps the low key
/// bits in the low hash bits). With the class id in the HIGH half every
/// one of them hashed to bucket 0.
#[test]
fn r24_lazy_keys_of_many_classes_spread_over_the_buckets() {
    use std::hash::{BuildHasher, BuildHasherDefault};
    let key = lazy_key(0x1234_5678, 9);
    assert_eq!(lazy_key_class(key), 0x1234_5678);
    assert_eq!(lazy_key_method(key), 9);
    let build = BuildHasherDefault::<FxHasher>::default();
    let mask = 4095u64;
    let buckets: std::collections::HashSet<u64> = (1..=4096u64)
        .map(|c| build.hash_one(lazy_key(c, 0)) & mask)
        .collect();
    assert!(
        buckets.len() > 2000,
        "only {} distinct buckets",
        buckets.len()
    );
    let old: std::collections::HashSet<u64> = (1..=4096u64)
        .map(|c| build.hash_one((c << 32) | 0) & mask)
        .collect();
    assert_eq!(old.len(), 1, "the old layout put every class in one bucket");
}

#[test]
fn test_create_vtable_and_add_methods() {
    let mut vt = Vtable::new(1);
    let s0 = vt.add_method("toString", "()Ljava/lang/String;", 1, 0);
    let s1 = vt.add_method("hashCode", "()I", 1, 1);
    assert_eq!(s0, 0);
    assert_eq!(s1, 1);
    assert_eq!(vt.len(), 2);
}

#[test]
fn test_lookup_by_name() {
    let mut vt = Vtable::new(1);
    vt.add_method("foo", "(I)V", 1, 0);
    vt.add_method("bar", "()I", 1, 1);
    assert_eq!(vt.lookup_slot("foo", "(I)V"), Some(0));
    assert_eq!(vt.lookup_slot("bar", "()I"), Some(1));
    assert_eq!(vt.lookup_slot("baz", "()V"), None);
}

#[test]
fn test_get_entry_by_slot() {
    let mut vt = Vtable::new(10);
    vt.add_method("run", "()V", 10, 5);
    let entry = vt.get(0).unwrap();
    assert_eq!(entry.declaring_class_id, 10);
    assert_eq!(entry.method_index, 5);
    assert_eq!(&*entry.method_name, "run");
    assert!(entry.resolved);
}

#[test]
fn test_inherit_from_parent() {
    let mut parent = Vtable::new(1);
    parent.add_method("toString", "()Ljava/lang/String;", 1, 0);
    parent.add_method("hashCode", "()I", 1, 1);

    let child = Vtable::from_parent(2, &parent);
    assert_eq!(child.len(), 2);
    assert_eq!(
        child.lookup_slot("toString", "()Ljava/lang/String;"),
        Some(0)
    );
    let entry = child.get(0).unwrap();
    assert_eq!(entry.declaring_class_id, 1); // still declared by parent
}

#[test]
fn test_override_method_in_child() {
    let mut parent = Vtable::new(1);
    parent.add_method("toString", "()Ljava/lang/String;", 1, 0);

    let mut child = Vtable::from_parent(2, &parent);
    // add_method with same sig overrides the inherited slot
    let slot = child.add_method("toString", "()Ljava/lang/String;", 2, 7);
    assert_eq!(slot, 0); // same slot as parent
    let entry = child.get(0).unwrap();
    assert_eq!(entry.declaring_class_id, 2);
    assert_eq!(entry.method_index, 7);
}

#[test]
fn test_slot_numbers_stable_after_override() {
    let mut parent = Vtable::new(1);
    parent.add_method("a", "()V", 1, 0);
    parent.add_method("b", "()V", 1, 1);
    parent.add_method("c", "()V", 1, 2);

    let mut child = Vtable::from_parent(2, &parent);
    child.add_method("b", "()V", 2, 10); // override middle method
    child.add_method("d", "()V", 2, 11); // new method

    assert_eq!(child.lookup_slot("a", "()V"), Some(0));
    assert_eq!(child.lookup_slot("b", "()V"), Some(1));
    assert_eq!(child.lookup_slot("c", "()V"), Some(2));
    assert_eq!(child.lookup_slot("d", "()V"), Some(3));
    assert_eq!(child.len(), 4);
}

#[test]
fn test_override_method_direct() {
    let mut vt = Vtable::new(1);
    vt.add_method("foo", "()V", 1, 0);
    vt.override_method(0, 2, 99);
    let entry = vt.get(0).unwrap();
    assert_eq!(entry.declaring_class_id, 2);
    assert_eq!(entry.method_index, 99);
    assert!(entry.resolved);
}

#[test]
fn test_interface_dispatch_via_itable() {
    let mut it = Itable::new();
    it.register(100, "run", "()V", 3);
    assert_eq!(it.lookup(100, "run", "()V"), Some(3));
    assert_eq!(it.lookup(100, "run", "(I)V"), None);
    assert_eq!(it.lookup(999, "run", "()V"), None);
}

// -- 2026-07-26 arch pass: single-index + inline bucket ---------------

fn probe_entry(name: &str, descriptor: &str, declaring: u64, idx: u32) -> VtableEntry {
    VtableEntry {
        declaring_class_id: declaring,
        method_index: idx,
        method_name: Arc::from(name),
        descriptor: Arc::from(descriptor),
        resolved: true,
        resolved_method: None,
        lazy: None,
        is_native: false,
    }
}

#[test]
fn slot_bucket_promotes_to_many_only_on_collision() {
    // zs s3/fp3: the bucket is a tagged `u32` in the index.
    let mut ix = SigIndex::default();
    ix.push(77, 3);
    assert_eq!(ix.get(77), Some(&[3u32][..]));
    ix.push(77, 3); // idempotent — must not allocate a Vec
    assert!(ix.many.is_empty());
    assert_eq!(ix.get(77), Some(&[3u32][..]));
    ix.push(77, 9);
    assert_eq!(ix.get(77), Some(&[3u32, 9][..]));
    ix.push(77, 9);
    assert_eq!(ix.get(77), Some(&[3u32, 9][..]));
    ix.push(77, 4);
    assert_eq!(ix.get(77), Some(&[3u32, 9, 4][..]));
    assert_eq!(ix.many.len(), 1);
    // A second colliding key gets its own list; other keys stay inline.
    ix.push(5, 1);
    ix.push(5, 2);
    ix.push(6, 0);
    assert_eq!(ix.get(5), Some(&[1u32, 2][..]));
    assert_eq!(ix.get(6), Some(&[0u32][..]));
    assert_eq!(ix.get(77), Some(&[3u32, 9, 4][..]));
    assert_eq!(ix.get(8), None);
    assert_eq!(std::mem::size_of::<(u32, u32)>(), 8);
    // Two keys that share the low half share a bucket; both stay
    // candidates in insertion order (the caller verifies each).
    let mut shared = SigIndex::default();
    shared.push(0x1_0000_0042, 4);
    shared.push(0x2_0000_0042, 1);
    assert_eq!(shared.get(0x1_0000_0042), Some(&[4u32, 1][..]));
    assert_eq!(shared.get(0x3_0000_0042), Some(&[4u32, 1][..]));
}

#[test]
fn add_method_override_still_reuses_the_slot_without_name_to_slot() {
    // The `name_to_slot` map that used to answer this question is gone;
    // `lookup_slot` must give the identical answer.
    let mut vt = Vtable::new(1);
    assert_eq!(vt.add_method("m", "(I)V", 1, 0), 0);
    assert_eq!(vt.add_method("m", "(J)V", 1, 1), 1); // overload: new slot
    assert_eq!(vt.add_method("m", "(I)V", 2, 7), 0); // same sig: override
    assert_eq!(vt.len(), 2);
    let e = vt.get(0).unwrap();
    assert_eq!(e.declaring_class_id, 2);
    assert_eq!(e.method_index, 7);
    assert_eq!(vt.lookup_slot("m", "(I)V"), Some(0));
    assert_eq!(vt.lookup_slot("m", "(J)V"), Some(1));
}

#[test]
fn install_vtable_indexes_every_slot_and_lookups_verify_exactly() {
    let mut mgr = VtableManager::new();
    mgr.install_vtable(
        7,
        vec![
            Some(probe_entry("run", "()V", 7, 0)),
            None, // an intentionally empty slot must not break the index
            Some(probe_entry("run", "(I)V", 7, 1)),
            Some(probe_entry("stop", "()V", 7, 2)),
        ],
    );
    let vt = mgr.get_vtable(7).unwrap();
    assert_eq!(vt.lookup_slot("run", "()V"), Some(0));
    assert_eq!(vt.lookup_slot("run", "(I)V"), Some(2));
    assert_eq!(vt.lookup_slot("stop", "()V"), Some(3));
    assert_eq!(vt.lookup_slot("stop", "(I)V"), None);
    assert_eq!(vt.lookup_slot("nope", "()V"), None);
}

#[test]
fn reinstall_refreshes_inherited_entries_without_touching_overrides() {
    let mut mgr = VtableManager::new();
    mgr.install_vtable(
        1,
        vec![
            Some(probe_entry("inherited", "()I", 1, 0)),
            Some(probe_entry("overridden", "()I", 1, 1)),
        ],
    );
    mgr.install_vtable(
        2,
        vec![
            Some(probe_entry("inherited", "()I", 1, 0)),
            Some(probe_entry("overridden", "()I", 2, 9)),
        ],
    );

    // Reinstalling an existing class id is the vtable side of JVMTI
    // redefine. The child's inherited snapshot must move to the new
    // method metadata while its own override remains untouched.
    mgr.install_vtable(
        1,
        vec![
            Some(probe_entry("inherited", "()I", 1, 7)),
            Some(probe_entry("overridden", "()I", 1, 8)),
        ],
    );

    let inherited = mgr.resolve_virtual(2, "inherited", "()I").unwrap();
    assert_eq!(inherited.declaring_class_id, 1);
    assert_eq!(inherited.method_index, 7);

    let overridden = mgr.resolve_virtual(2, "overridden", "()I").unwrap();
    assert_eq!(overridden.declaring_class_id, 2);
    assert_eq!(overridden.method_index, 9);
}

#[test]
fn from_parent_inherits_the_signature_index() {
    let mut parent = Vtable::new(1);
    parent.add_method("a", "()V", 1, 0);
    parent.add_method("b", "()V", 1, 1);
    let mut child = Vtable::from_parent(2, &parent);
    assert_eq!(child.lookup_slot("b", "()V"), Some(1));
    // Overriding through the inherited index must land on the same slot.
    assert_eq!(child.add_method("b", "()V", 2, 42), 1);
    assert_eq!(child.get(1).unwrap().declaring_class_id, 2);
    // The parent's own table is untouched.
    assert_eq!(parent.get(1).unwrap().declaring_class_id, 1);
}

#[test]
fn unload_classes_matches_a_loop_of_unload_class() {
    let build = || {
        let mut mgr = VtableManager::new();
        // Class 1 declares `a`; classes 2 and 3 inherit that entry.
        for cid in [1u64, 2, 3] {
            mgr.install_vtable(
                cid,
                vec![
                    Some(probe_entry("a", "()V", 1, 0)),
                    Some(probe_entry("b", "()V", cid, 1)),
                ],
            );
        }
        mgr
    };

    let mut looped = build();
    looped.unload_class(1);
    let mut batched = build();
    batched.unload_classes(&[1]);

    assert!(looped.get_vtable(1).is_none());
    assert!(batched.get_vtable(1).is_none());
    for cid in [2u64, 3] {
        // The inherited `a` entry (declared by the unloaded class 1) is
        // invalidated in both; the class's own `b` stays resolved.
        assert!(looped.vtable_entry_ref(cid, 0).is_none());
        assert!(batched.vtable_entry_ref(cid, 0).is_none());
        assert!(looped.vtable_entry_ref(cid, 1).is_some());
        assert!(batched.vtable_entry_ref(cid, 1).is_some());
    }
}

#[test]
fn unload_classes_is_a_noop_on_an_empty_batch() {
    let mut mgr = VtableManager::new();
    mgr.install_vtable(1, vec![Some(probe_entry("a", "()V", 1, 0))]);
    mgr.unload_classes(&[]);
    assert!(mgr.vtable_entry_ref(1, 0).is_some());
}

#[test]
fn test_resolve_virtual_on_leaf_class() {
    let mut mgr = VtableManager::new();
    let vt = mgr.create_vtable(1, None);
    vt.add_method("compute", "(II)I", 1, 0);

    let entry = mgr.resolve_virtual(1, "compute", "(II)I").unwrap();
    assert_eq!(entry.declaring_class_id, 1);
    assert_eq!(entry.method_index, 0);
}

#[test]
fn test_resolve_interface_method() {
    let mut mgr = VtableManager::new();

    // Class 10 has a vtable with "run" at slot 2
    let vt = mgr.create_vtable(10, None);
    vt.add_method("toString", "()Ljava/lang/String;", 10, 0);
    vt.add_method("hashCode", "()I", 10, 1);
    vt.add_method("run", "()V", 10, 2);

    // Register interface 50 mapping for class 10
    let it = mgr.create_itable(10);
    it.register(50, "run", "()V", 2);

    let entry = mgr.resolve_interface(10, 50, "run", "()V").unwrap();
    assert_eq!(entry.declaring_class_id, 10);
    assert_eq!(entry.method_index, 2);
}

#[test]
fn test_invalidate_slot_marks_unresolved() {
    let mut vt = Vtable::new(1);
    vt.add_method("foo", "()V", 1, 0);
    assert!(vt.get(0).unwrap().resolved);

    vt.invalidate_slot(0);
    assert!(!vt.get(0).unwrap().resolved);
}

#[test]
fn test_invalidate_class_across_vtables() {
    let mut mgr = VtableManager::new();

    // Parent class 1
    let vt = mgr.create_vtable(1, None);
    vt.add_method("foo", "()V", 1, 0);
    vt.add_method("bar", "()V", 1, 1);

    // Child class 2 inherits from 1, overrides bar
    let vt2 = mgr.create_vtable(2, Some(1));
    vt2.add_method("bar", "()V", 2, 5);

    // Invalidate class 1 -- should mark foo unresolved in both vtables,
    // bar unresolved only in class 1's vtable (class 2 overrode it).
    mgr.invalidate_class(1);

    let vt1 = mgr.get_vtable(1).unwrap();
    assert!(!vt1.get(0).unwrap().resolved); // foo in class 1
    assert!(!vt1.get(1).unwrap().resolved); // bar in class 1

    let vt2 = mgr.get_vtable(2).unwrap();
    assert!(!vt2.get(0).unwrap().resolved); // foo inherited from class 1
    assert!(vt2.get(1).unwrap().resolved); // bar overridden by class 2
}

#[test]
fn test_empty_vtable() {
    let vt = Vtable::new(99);
    assert_eq!(vt.len(), 0);
    assert!(vt.is_empty());
    assert_eq!(vt.get(0), None);
    assert_eq!(vt.lookup_slot("any", "()V"), None);
    assert_eq!(vt.class_id(), 99);
}

#[test]
fn test_multiple_interfaces() {
    let mut mgr = VtableManager::new();
    let vt = mgr.create_vtable(10, None);
    vt.add_method("run", "()V", 10, 0);
    vt.add_method("call", "()Ljava/lang/Object;", 10, 1);
    vt.add_method("compare", "(Ljava/lang/Object;Ljava/lang/Object;)I", 10, 2);

    let it = mgr.create_itable(10);
    it.register(50, "run", "()V", 0); // Runnable
    it.register(51, "call", "()Ljava/lang/Object;", 1); // Callable
    it.register(52, "compare", "(Ljava/lang/Object;Ljava/lang/Object;)I", 2); // Comparator

    assert!(mgr.resolve_interface(10, 50, "run", "()V").is_some());
    assert!(mgr
        .resolve_interface(10, 51, "call", "()Ljava/lang/Object;")
        .is_some());
    assert!(mgr
        .resolve_interface(10, 52, "compare", "(Ljava/lang/Object;Ljava/lang/Object;)I")
        .is_some());
    assert!(mgr.resolve_interface(10, 99, "run", "()V").is_none());
}

#[test]
fn test_diamond_inheritance() {
    // A (class 1) -> B (class 2), A -> C (class 3), D (class 4) -> B + C
    let mut mgr = VtableManager::new();

    // A: base class with method "foo"
    let vt_a = mgr.create_vtable(1, None);
    vt_a.add_method("foo", "()V", 1, 0);

    // B: inherits from A, overrides foo
    let vt_b = mgr.create_vtable(2, Some(1));
    vt_b.add_method("foo", "()V", 2, 10);

    // C: inherits from A, adds bar
    let vt_c = mgr.create_vtable(3, Some(1));
    vt_c.add_method("bar", "()V", 3, 20);

    // D: inherits from B (in JVM single-inheritance, pick one parent for vtable).
    // Then manually add C's interface methods via itable.
    let vt_d = mgr.create_vtable(4, Some(2));
    vt_d.add_method("bar", "()V", 4, 30); // D implements bar itself

    // D's itable maps interface C's bar
    let it_d = mgr.create_itable(4);
    it_d.register(3, "bar", "()V", 1); // slot 1 in D's vtable

    let entry_foo = mgr.resolve_virtual(4, "foo", "()V").unwrap();
    assert_eq!(entry_foo.declaring_class_id, 2); // inherited from B

    let entry_bar = mgr.resolve_interface(4, 3, "bar", "()V").unwrap();
    assert_eq!(entry_bar.declaring_class_id, 4); // D's own impl
}

#[test]
fn test_lookup_nonexistent_method() {
    let mut mgr = VtableManager::new();
    mgr.create_vtable(1, None);
    assert!(mgr.resolve_virtual(1, "nope", "()V").is_none());
    assert!(mgr.resolve_virtual(999, "nope", "()V").is_none());
    assert!(mgr.resolve_interface(1, 50, "nope", "()V").is_none());
}

#[test]
fn test_large_vtable_100_plus_methods() {
    let mut vt = Vtable::new(1);
    for i in 0..150u32 {
        let name = format!("method_{}", i);
        vt.add_method(&name, "()V", 1, i);
    }
    assert_eq!(vt.len(), 150);

    // Spot check
    assert_eq!(vt.lookup_slot("method_0", "()V"), Some(0));
    assert_eq!(vt.lookup_slot("method_149", "()V"), Some(149));
    let entry = vt.get(75).unwrap();
    assert_eq!(entry.method_index, 75);
    assert_eq!(&*entry.method_name, "method_75");
}

#[test]
fn test_vtable_manager_lifecycle() {
    let mut mgr = VtableManager::new();

    // No vtables initially
    assert!(mgr.get_vtable(1).is_none());
    assert!(mgr.get_vtable_mut(1).is_none());

    // Create root
    let vt = mgr.create_vtable(1, None);
    vt.add_method("init", "()V", 1, 0);

    // Create child
    let vt2 = mgr.create_vtable(2, Some(1));
    vt2.add_method("doWork", "()V", 2, 1);

    // Both exist
    assert!(mgr.get_vtable(1).is_some());
    assert!(mgr.get_vtable(2).is_some());

    // Child has inherited method plus its own
    let child_vt = mgr.get_vtable(2).unwrap();
    assert_eq!(child_vt.len(), 2);
    assert!(child_vt.lookup_slot("init", "()V").is_some());
    assert!(child_vt.lookup_slot("doWork", "()V").is_some());

    // Create itable
    assert!(mgr.get_itable(2).is_none());
    mgr.create_itable(2);
    assert!(mgr.get_itable(2).is_some());
}

#[test]
fn test_resolve_virtual_unresolved_returns_none() {
    let mut mgr = VtableManager::new();
    let vt = mgr.create_vtable(1, None);
    vt.add_method("foo", "()V", 1, 0);

    // Invalidate it
    mgr.get_vtable_mut(1).unwrap().invalidate_slot(0);

    // Should return None because entry is unresolved
    assert!(mgr.resolve_virtual(1, "foo", "()V").is_none());
}

#[test]
fn test_resolve_interface_unresolved_returns_none() {
    let mut mgr = VtableManager::new();
    let vt = mgr.create_vtable(10, None);
    vt.add_method("run", "()V", 10, 0);

    let it = mgr.create_itable(10);
    it.register(50, "run", "()V", 0);

    mgr.get_vtable_mut(10).unwrap().invalidate_slot(0);

    assert!(mgr.resolve_interface(10, 50, "run", "()V").is_none());
}

#[test]
fn test_itable_default() {
    let it = Itable::default();
    assert!(it.is_empty());
    assert_eq!(it.len(), 0);
}

#[test]
fn test_vtable_manager_default() {
    let mgr = VtableManager::default();
    assert!(mgr.get_vtable(1).is_none());
}

// ---------------------------------------------------------------
// T10.5 — install_vtable + resolve_virtual_slot + CHA invalidate
// ---------------------------------------------------------------

#[test]
fn test_install_vtable_and_resolve_slot() {
    let mut mgr = VtableManager::new();
    let entries = vec![
        Some(VtableEntry {
            declaring_class_id: 42,
            method_index: 0,
            method_name: Arc::<str>::from("toString"),
            descriptor: Arc::<str>::from("()Ljava/lang/String;"),
            resolved: true,
            resolved_method: None,
            lazy: None,
            is_native: false,
        }),
        Some(VtableEntry {
            declaring_class_id: 42,
            method_index: 1,
            method_name: Arc::<str>::from("hashCode"),
            descriptor: Arc::<str>::from("()I"),
            resolved: true,
            resolved_method: None,
            lazy: None,
            is_native: false,
        }),
    ];
    mgr.install_vtable(42, entries);

    let e0 = mgr.resolve_virtual_slot(42, 0).unwrap();
    assert_eq!(e0.declaring_class_id, 42);
    assert_eq!(e0.method_index, 0);
    assert_eq!(&*e0.method_name, "toString");

    let e1 = mgr.resolve_virtual_slot(42, 1).unwrap();
    assert_eq!(e1.method_index, 1);

    // name-based lookup still works because install_vtable rebuilt the index
    let by_name = mgr.resolve_virtual(42, "hashCode", "()I").unwrap();
    assert_eq!(by_name.method_index, 1);

    // unknown class / slot → None
    assert!(mgr.resolve_virtual_slot(999, 0).is_none());
    assert!(mgr.resolve_virtual_slot(42, 99).is_none());
}

#[test]
fn test_install_vtable_replaces_previous_entries() {
    let mut mgr = VtableManager::new();

    let first = vec![Some(VtableEntry {
        declaring_class_id: 1,
        method_index: 0,
        method_name: Arc::<str>::from("foo"),
        descriptor: Arc::<str>::from("()V"),
        resolved: true,
        resolved_method: None,
        lazy: None,
        is_native: false,
    })];
    mgr.install_vtable(1, first);

    let second = vec![Some(VtableEntry {
        declaring_class_id: 1,
        method_index: 7,
        method_name: Arc::<str>::from("bar"),
        descriptor: Arc::<str>::from("()V"),
        resolved: true,
        resolved_method: None,
        lazy: None,
        is_native: false,
    })];
    mgr.install_vtable(1, second);

    // old "foo" is gone, new "bar" is installed at slot 0
    assert!(mgr.resolve_virtual(1, "foo", "()V").is_none());
    let e = mgr.resolve_virtual_slot(1, 0).unwrap();
    assert_eq!(e.method_index, 7);
    assert_eq!(&*e.method_name, "bar");
}

#[test]
fn test_invalidate_for_override_single_slot() {
    let mut mgr = VtableManager::new();
    let entries = vec![
        Some(VtableEntry {
            declaring_class_id: 1,
            method_index: 0,
            method_name: Arc::<str>::from("foo"),
            descriptor: Arc::<str>::from("()V"),
            resolved: true,
            resolved_method: None,
            lazy: None,
            is_native: false,
        }),
        Some(VtableEntry {
            declaring_class_id: 1,
            method_index: 1,
            method_name: Arc::<str>::from("bar"),
            descriptor: Arc::<str>::from("()V"),
            resolved: true,
            resolved_method: None,
            lazy: None,
            is_native: false,
        }),
    ];
    mgr.install_vtable(1, entries);

    // CHA signal: a new subclass overrode slot 0 of class 1.
    mgr.invalidate_for_override(1, 0);

    // slot 0 is now unresolved → resolve_virtual_slot returns None
    assert!(mgr.resolve_virtual_slot(1, 0).is_none());
    // slot 1 is still resolved
    assert!(mgr.resolve_virtual_slot(1, 1).is_some());

    // invalidating an unknown class is a no-op
    mgr.invalidate_for_override(999, 0);
}

#[test]
fn test_vtable_entry_ref_borrow_no_clone() {
    let mut mgr = VtableManager::new();
    let entries = vec![Some(VtableEntry {
        declaring_class_id: 5,
        method_index: 3,
        method_name: Arc::<str>::from("run"),
        descriptor: Arc::<str>::from("()V"),
        resolved: true,
        resolved_method: None,
        lazy: None,
        is_native: false,
    })];
    mgr.install_vtable(5, entries);

    let eref = mgr
        .vtable_entry_ref(5, 0)
        .expect("slot should be populated");
    assert_eq!(eref.method_index, 3);
    assert_eq!(eref.declaring_class_id, 5);

    // invalidate → vtable_entry_ref returns None even though the entry exists
    mgr.invalidate_for_override(5, 0);
    assert!(mgr.vtable_entry_ref(5, 0).is_none());
}

// -----------------------------------------------------------------
// T10.5 task-required tests
// -----------------------------------------------------------------

/// A populated vtable must be queryable via resolve_virtual_slot
/// immediately after `install_vtable`.
#[test]
fn t10_vtable_populated_at_link_time() {
    let mut mgr = VtableManager::new();
    let entries = vec![Some(VtableEntry {
        declaring_class_id: 7,
        method_index: 2,
        method_name: Arc::<str>::from("foo"),
        descriptor: Arc::<str>::from("()V"),
        resolved: true,
        resolved_method: None,
        lazy: None,
        is_native: false,
    })];
    mgr.install_vtable(7, entries);

    let entry = mgr
        .resolve_virtual_slot(7, 0)
        .expect("vtable should be populated at link time");
    assert_eq!(entry.declaring_class_id, 7);
    assert_eq!(entry.method_index, 2);
}

/// When a subclass doesn't override a slot, the installed vtable
/// carries the superclass's entry for that slot.
#[test]
fn t10_vtable_inherits_from_super() {
    let super_entry = VtableEntry {
        declaring_class_id: 1,
        method_index: 0,
        method_name: Arc::<str>::from("greet"),
        descriptor: Arc::<str>::from("()V"),
        resolved: true,
        resolved_method: None,
        lazy: None,
        is_native: false,
    };

    // Subclass didn't override — slot 0 still carries the super's
    // declaring_class_id.
    let subclass_entries = vec![Some(super_entry.clone())];

    let mut mgr = VtableManager::new();
    mgr.install_vtable(1, vec![Some(super_entry.clone())]);
    mgr.install_vtable(2, subclass_entries);

    let inherited = mgr.resolve_virtual_slot(2, 0).unwrap();
    assert_eq!(
        inherited.declaring_class_id, 1,
        "subclass slot must still point at super's declaring class",
    );
    assert_eq!(inherited.method_index, 0);
    assert_eq!(&*inherited.method_name, "greet");
}

/// When the subclass overrides a method, its own entry occupies the
/// inherited slot (same slot number, new declaring_class_id).
#[test]
fn t10_vtable_override_replaces_super() {
    let super_entries = vec![Some(VtableEntry {
        declaring_class_id: 1,
        method_index: 0,
        method_name: Arc::<str>::from("greet"),
        descriptor: Arc::<str>::from("()V"),
        resolved: true,
        resolved_method: None,
        lazy: None,
        is_native: false,
    })];

    let subclass_entries = vec![Some(VtableEntry {
        declaring_class_id: 2,
        method_index: 5,
        method_name: Arc::<str>::from("greet"),
        descriptor: Arc::<str>::from("()V"),
        resolved: true,
        resolved_method: None,
        lazy: None,
        is_native: false,
    })];

    let mut mgr = VtableManager::new();
    mgr.install_vtable(1, super_entries);
    mgr.install_vtable(2, subclass_entries);

    let e = mgr.resolve_virtual_slot(2, 0).unwrap();
    assert_eq!(
        e.declaring_class_id, 2,
        "override must replace the super's declaring_class_id",
    );
    assert_eq!(e.method_index, 5);
}

/// 4 threads reading the same vtable concurrently must all succeed
/// without deadlock. `resolve_virtual_slot` must be read-lock only.
#[test]
fn t10_vtable_resolve_concurrent() {
    use std::sync::Arc;
    use std::thread;

    let mgr = Arc::new(parking_lot::RwLock::new(VtableManager::new()));
    {
        let mut w = mgr.write();
        let entries = vec![
            Some(VtableEntry {
                declaring_class_id: 10,
                method_index: 0,
                method_name: Arc::<str>::from("a"),
                descriptor: Arc::<str>::from("()V"),
                resolved: true,
                resolved_method: None,
                lazy: None,
                is_native: false,
            }),
            Some(VtableEntry {
                declaring_class_id: 10,
                method_index: 1,
                method_name: Arc::<str>::from("b"),
                descriptor: Arc::<str>::from("()V"),
                resolved: true,
                resolved_method: None,
                lazy: None,
                is_native: false,
            }),
            Some(VtableEntry {
                declaring_class_id: 10,
                method_index: 2,
                method_name: Arc::<str>::from("c"),
                descriptor: Arc::<str>::from("()V"),
                resolved: true,
                resolved_method: None,
                lazy: None,
                is_native: false,
            }),
        ];
        w.install_vtable(10, entries);
    }

    let mut handles = Vec::new();
    for t in 0..4 {
        let mgr = Arc::clone(&mgr);
        handles.push(thread::spawn(move || {
            let mut hits = 0usize;
            for _ in 0..2000 {
                let guard = mgr.read();
                let slot = t % 3;
                let e = guard
                    .resolve_virtual_slot(10, slot)
                    .expect("concurrent reader missed a populated slot");
                assert_eq!(e.declaring_class_id, 10);
                assert_eq!(e.method_index as usize, slot);
                hits += 1;
            }
            hits
        }));
    }
    let totals: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
    for t in totals {
        assert_eq!(t, 2000);
    }
}

// ------------------------------------------------------------------
// T10.9.A tests — vtable dispatch integration
//
// These tests exercise the `VtableEntry::resolved_method` payload,
// `vtable_install_adapter` conversion, and the `invalidate_for_override`
// CHA path without booting a full `SharedVm`.  The vm-level
// integration test `t10_9_a_vtable_fast_path_hits_without_class_manager_lock`
// lives in `vm.rs` alongside other synthetic-jdk integration tests.
// ------------------------------------------------------------------

/// Helper — build a minimal `CachedBytecodeMethod` Arc for a given
/// slot so the `resolved_method` field can be exercised without a
/// real class file.
fn make_dispatch_stub(
    declaring_class_id: u32,
    method_name: &str,
    descriptor: &str,
) -> Arc<cratonvm_jit_api::CachedBytecodeMethod> {
    Arc::new(cratonvm_jit_api::CachedBytecodeMethod::from_parts(
        cratonvm_jit_api::CachedMethodParts {
            declaring_class_id: cratonvm_types::ClassId::new(declaring_class_id),
            class_name: Arc::<str>::from("Stub"),
            method_name: Arc::<str>::from(method_name),
            method_descriptor: Arc::<str>::from(descriptor),
            source_file: None,
            // 0xb1 = `return` — safe single-op bytecode.
            code: Arc::<[u8]>::from(vec![0xb1u8, 0x00, 0x00].into_boxed_slice()),
            exception_table: Arc::<[_]>::from(Vec::new().into_boxed_slice()),
            max_stack: 0,
            max_locals: 1,
            num_params: 0,
            is_synchronized: false,
            is_static: false,
        },
    ))
}

/// T10.9.A.1 — `VtableEntry` must carry a pre-built `Arc<CachedBytecodeMethod>`
/// after `vtable_install_adapter` converts a
/// `VtableSlotDescriptor` that included a dispatch snapshot.
#[test]
fn t10_9_a_vtable_entry_carries_resolved_method() {
    let mut mgr = VtableManager::new();
    let entries = vec![Some(VtableEntry {
        declaring_class_id: 100,
        method_index: 0,
        method_name: Arc::<str>::from("run"),
        descriptor: Arc::<str>::from("()V"),
        resolved: true,
        resolved_method: Some(make_dispatch_stub(100, "run", "()V")),
        lazy: None,
        is_native: false,
    })];
    mgr.install_vtable(100, entries);

    let entry = mgr
        .resolve_virtual_slot(100, 0)
        .expect("slot 0 should be populated");
    assert_eq!(entry.declaring_class_id, 100);
    assert!(
        entry.resolved_method.is_some(),
        "vtable entry must carry Arc<CachedBytecodeMethod> for fast-path dispatch",
    );
    let cached = entry.resolved_method.unwrap();
    assert_eq!(&*cached.method_name, "run");
    assert_eq!(&*cached.method_descriptor, "()V");
}

/// T10.9.A.4 — when a subclass overrides a superclass method,
/// `invalidate_for_override` is fired against the super's slot and
/// the entry is marked unresolved. Subsequent `resolve_virtual_slot`
/// returns `None` so stale cached dispatch entries get re-resolved.
#[test]
fn t10_9_a_vtable_override_invalidation() {
    let mut mgr = VtableManager::new();

    // Super class Animal — slot 0 = speak()V
    let super_entries = vec![Some(VtableEntry {
        declaring_class_id: 1,
        method_index: 0,
        method_name: Arc::<str>::from("speak"),
        descriptor: Arc::<str>::from("()V"),
        resolved: true,
        resolved_method: Some(make_dispatch_stub(1, "speak", "()V")),
        lazy: None,
        is_native: false,
    })];
    mgr.install_vtable(1, super_entries);

    // Before the override, Animal's slot 0 resolves.
    assert!(mgr.resolve_virtual_slot(1, 0).is_some());
    // And carries a dispatch snapshot.
    assert!(mgr
        .resolve_virtual_slot(1, 0)
        .unwrap()
        .resolved_method
        .is_some());

    // Subclass Dog overrides speak() — the link-time hook calls
    // `vtable_override_adapter(1, 0)` which hits this API.
    mgr.invalidate_for_override(1, 0);

    // Now Animal's slot 0 is unresolved.
    assert!(
        mgr.resolve_virtual_slot(1, 0).is_none(),
        "override invalidation must mark super's slot unresolved",
    );
    // `vtable_entry_ref` also returns None because the entry is
    // unresolved (the is_resolved check gates both accessors).
    assert!(mgr.vtable_entry_ref(1, 0).is_none());
}

/// T10.9.A.3 — a class with no vtable entry at the requested slot
/// returns `None` from `resolve_virtual_slot` — the fast-path
/// caller must fall through to the slow path without panic. Also
/// exercises bounds-check safety: slot way past the end, class_id
/// out of range.
#[test]
fn t10_9_a_vtable_miss_falls_through() {
    let mut mgr = VtableManager::new();

    // Empty vtable for class 50 — no slots at all.
    mgr.install_vtable(50, vec![]);
    assert!(mgr.resolve_virtual_slot(50, 0).is_none());
    assert!(mgr.resolve_virtual_slot(50, 99).is_none());

    // Sparse vtable: slot 0 = Some, slot 1 = None (reserved).
    let entries = vec![
        Some(VtableEntry {
            declaring_class_id: 51,
            method_index: 0,
            method_name: Arc::<str>::from("foo"),
            descriptor: Arc::<str>::from("()V"),
            resolved: true,
            resolved_method: None,
            lazy: None,
            is_native: false,
        }),
        None,
    ];
    mgr.install_vtable(51, entries);

    assert!(mgr.resolve_virtual_slot(51, 0).is_some());
    // `None` slot → lookup returns None, no panic.
    assert!(mgr.resolve_virtual_slot(51, 1).is_none());
    // Out-of-range slot → None.
    assert!(mgr.resolve_virtual_slot(51, 1_000_000).is_none());
    // Unknown class_id → None.
    assert!(mgr.resolve_virtual_slot(999_999, 0).is_none());
}

/// **THIS TEST CANNOT FAIL, and its name overstates what it covers.**
///
/// The property the name claims — "when `vtable_install_adapter` sees a
/// `VtableSlotDescriptor` with `dispatch: None`, the resulting
/// `VtableEntry` has `resolved_method: None`" — is NOT asserted here.
/// The body constructs a descriptor and then asserts `dispatch.is_none()`
/// and `method_index == 3`, which are the two fields the four lines above
/// just set. `vtable_install_adapter` is never called. Make the adapter
/// hand a `dispatch: None` descriptor a `resolved_method: Some(..)` —
/// i.e. dispatch an abstract method to a body — and this test does not
/// move.
///
/// It is left in place rather than deleted because what it constructs is
/// still a live compile-time check that the `VtableSlotDescriptor` shape
/// the adapter consumes has not drifted. The behavioural half is
/// `an_abstract_slot_installs_without_a_body` below, which calls the
/// conversion (`install_vtable_descriptors`, lifted out of the adapter in
/// wave 19; the global manager it once needed is gone since wave 21).
#[test]
fn t10_9_a_adapter_preserves_empty_dispatch() {
    // Shape-only: this is a compile-time check on `VtableSlotDescriptor`,
    // NOT a check on the adapter. See the doc comment above.
    let desc = cratonvm_classloading::VtableSlotDescriptor::new(
        cratonvm_classloading::VtableSlotData {
            declaring_class_id: 77,
            method_index: 3,
            method_name: Arc::<str>::from("abstr"),
            descriptor: Arc::<str>::from("()I"),
            dispatch: None, // abstract method — no snapshot
        },
    );
    assert!(desc.dispatch.is_none());
    assert_eq!(desc.method_index, 3);
}

/// The behavioural half of the test above, now that the conversion is a
/// callable function (`install_vtable_descriptors`, interpreter round i1
/// wave 19): a `dispatch: None` descriptor (an abstract method) installs
/// a resolved entry with no bytecode body, into the manager it is given.
#[test]
fn an_abstract_slot_installs_without_a_body() {
    let manager = RwLock::new(VtableManager::new());
    let desc = cratonvm_classloading::VtableSlotDescriptor::new(
        cratonvm_classloading::VtableSlotData {
            declaring_class_id: 77,
            method_index: 3,
            method_name: Arc::<str>::from("abstr"),
            descriptor: Arc::<str>::from("()I"),
            dispatch: None,
        },
    );
    install_vtable_descriptors(&manager, 77, vec![Some(desc)]);
    let entry = manager
        .read()
        .resolve_virtual_slot(77, 0)
        .expect("slot installed");
    assert_eq!(entry.method_index, 3);
    assert!(
        entry.resolved_method.is_none(),
        "no body for an abstract method"
    );
    assert!(!entry.is_native);
}

/// T10.9.A — a native `VtableMethodSnapshot` routes into a
/// `VtableEntry` with `is_native == true` and no bytecode Arc. The
/// interpreter's fast-path returns `CacheMiss` for these so the
/// native registry takes over.
#[test]
fn t10_9_a_native_snapshot_flags_is_native() {
    let mut mgr = VtableManager::new();
    let entries = vec![Some(VtableEntry {
        declaring_class_id: 88,
        method_index: 0,
        method_name: Arc::<str>::from("getClass"),
        descriptor: Arc::<str>::from("()Ljava/lang/Class;"),
        resolved: true,
        resolved_method: None,
        lazy: None,
        is_native: true,
    })];
    mgr.install_vtable(88, entries);

    let e = mgr.resolve_virtual_slot(88, 0).expect("slot populated");
    assert!(e.is_native, "native methods must carry is_native=true");
    assert!(
        e.resolved_method.is_none(),
        "native methods carry no bytecode Arc",
    );
}

/// Round-2 VM §8 HIGH — `resolve_virtual_slot_method` returns the
/// same `Arc<CachedBytecodeMethod>` payload the full
/// `resolve_virtual_slot` would carry, but as a bare Arc clone so
/// the hot path skips two needless refcount bumps on
/// `method_name` / `descriptor`.
#[test]
fn resolve_virtual_slot_method_returns_cached_arc() {
    let mut mgr = VtableManager::new();
    let stub = make_dispatch_stub(42, "tick", "()V");
    let stub_weak = Arc::downgrade(&stub);
    mgr.install_vtable(
        42,
        vec![Some(VtableEntry {
            declaring_class_id: 42,
            method_index: 0,
            method_name: Arc::<str>::from("tick"),
            descriptor: Arc::<str>::from("()V"),
            resolved: true,
            resolved_method: Some(stub),
            lazy: None,
            is_native: false,
        })],
    );

    // Slot-indexed helper returns just the bytecode Arc.
    let got = mgr
        .resolve_virtual_slot_method(42, 0)
        .expect("slot populated and resolved");
    assert!(
        stub_weak.upgrade().is_some(),
        "vtable should still own the installed Arc",
    );
    assert_eq!(&*got.method_name, "tick");
    assert_eq!(&*got.method_descriptor, "()V");

    // Name-based helper does the same for the slow path.
    let got_named = mgr
        .resolve_virtual_method(42, "tick", "()V")
        .expect("named lookup hits");
    assert!(Arc::ptr_eq(&got, &got_named));

    // Unresolved → None for both variants.
    mgr.invalidate_for_override(42, 0);
    assert!(mgr.resolve_virtual_slot_method(42, 0).is_none());
    assert!(mgr.resolve_virtual_method(42, "tick", "()V").is_none());
}

/// Round-2 VM §8 HIGH — a vtable entry with no cached bytecode
/// (abstract / native) returns `None` from the `_method` helpers
/// even when the slot itself is resolved.
#[test]
fn resolve_virtual_method_none_for_native_slot() {
    let mut mgr = VtableManager::new();
    mgr.install_vtable(
        88,
        vec![Some(VtableEntry {
            declaring_class_id: 88,
            method_index: 0,
            method_name: Arc::<str>::from("getClass"),
            descriptor: Arc::<str>::from("()Ljava/lang/Class;"),
            resolved: true,
            resolved_method: None, // native — no bytecode
            lazy: None,
            is_native: true,
        })],
    );
    assert!(mgr.resolve_virtual_slot_method(88, 0).is_none());
    assert!(mgr
        .resolve_virtual_method(88, "getClass", "()Ljava/lang/Class;")
        .is_none(),);
    // The full-entry variant still returns Some(..) because the
    // slot is resolved — callers that need to detect the native
    // case rely on `is_native` from the full entry.
    let full = mgr.resolve_virtual_slot(88, 0).unwrap();
    assert!(full.is_native);
    assert!(full.resolved_method.is_none());
}

/// T10.9.A — installing a vtable with a dispatch-bearing entry
/// twice (e.g. class redefine) replaces the Arc cleanly; the old
/// Arc is dropped when the install completes.
#[test]
fn t10_9_a_install_twice_drops_prior_arc() {
    let mut mgr = VtableManager::new();
    let first_arc = make_dispatch_stub(10, "foo", "()V");
    let second_arc = make_dispatch_stub(10, "foo", "()V");
    let first_weak = Arc::downgrade(&first_arc);

    mgr.install_vtable(
        10,
        vec![Some(VtableEntry {
            declaring_class_id: 10,
            method_index: 0,
            method_name: Arc::<str>::from("foo"),
            descriptor: Arc::<str>::from("()V"),
            resolved: true,
            resolved_method: Some(first_arc),
            lazy: None,
            is_native: false,
        })],
    );

    mgr.install_vtable(
        10,
        vec![Some(VtableEntry {
            declaring_class_id: 10,
            method_index: 0,
            method_name: Arc::<str>::from("foo"),
            descriptor: Arc::<str>::from("()V"),
            resolved: true,
            resolved_method: Some(second_arc),
            lazy: None,
            is_native: false,
        })],
    );

    // The weak reference to the first Arc should now be unable to
    // upgrade — the vtable no longer holds it and we dropped our
    // local handle.
    assert!(
        first_weak.upgrade().is_none(),
        "installing a second vtable must drop the prior resolved_method Arc",
    );
}

/// An inherited slot shares the declaring class's snapshot, and only when
/// every identifying field agrees; every mismatch answers `None` so the
/// install builds a fresh snapshot exactly as it did before.
#[test]
fn inherited_dispatch_shares_only_a_provably_identical_snapshot() {
    let mut mgr = VtableManager::new();
    let declared = make_dispatch_stub(40, "run", "()V");
    let entry = |resolved_method, is_native| VtableEntry {
        declaring_class_id: 40,
        method_index: 0,
        method_name: Arc::<str>::from("run"),
        descriptor: Arc::<str>::from("()V"),
        resolved: true,
        resolved_method,
        lazy: None,
        is_native,
    };
    mgr.install_vtable(40, vec![Some(entry(Some(Arc::clone(&declared)), false))]);

    let shared = mgr
        .inherited_dispatch(40, 0, "Stub", "run", "()V")
        .expect("identical inherited slot");
    assert!(Arc::ptr_eq(&shared, &declared), "must be the same Arc");

    // A CHA-invalidated slot still describes the same method body.
    mgr.invalidate_for_override(40, 0);
    assert!(mgr
        .inherited_dispatch(40, 0, "Stub", "run", "()V")
        .is_some());

    assert!(mgr
        .inherited_dispatch(40, 1, "Stub", "run", "()V")
        .is_none());
    assert!(mgr
        .inherited_dispatch(41, 0, "Stub", "run", "()V")
        .is_none());
    assert!(mgr
        .inherited_dispatch(40, 0, "Other", "run", "()V")
        .is_none());
    assert!(mgr
        .inherited_dispatch(40, 0, "Stub", "walk", "()V")
        .is_none());
    assert!(mgr
        .inherited_dispatch(40, 0, "Stub", "run", "(I)V")
        .is_none());

    mgr.install_vtable(40, vec![Some(entry(None, true))]);
    assert!(
        mgr.inherited_dispatch(40, 0, "Stub", "run", "()V")
            .is_none(),
        "a native slot has no snapshot to share"
    );
}

/// Interpreter round i1 wave 22, lane L4
/// (`OVERRIDE_SIGNAL_CLEARS_SUPER_ENTRY`): a subclass that overrides a
/// slot leaves the super's own entry dispatchable — a receiver of the
/// super's class still selects the super's method — and the subclass's
/// table answers the override.
#[test]
fn a_subclass_override_leaves_the_supers_own_entry_dispatchable() {
    use cratonvm_classloading::metadata_handle::ClassHandle;
    use cratonvm_native_api::VmId;
    if OVERRIDE_SIGNAL_CLEARS_SUPER_ENTRY {
        return;
    }
    // A VM identity no real VM in the test process holds.
    let vm_identity = 0x4C34_0022_usize;
    let manager = Arc::new(RwLock::new(VtableManager::new()));
    register_vm_vtable_manager(vm_identity, &manager);
    let entry = |declaring: u64| VtableEntry {
        declaring_class_id: declaring,
        method_index: 0,
        method_name: Arc::<str>::from("hashCode"),
        descriptor: Arc::<str>::from("()I"),
        resolved: true,
        resolved_method: None,
        lazy: None,
        is_native: false,
    };
    let (sup, sub) = (0x0022_0001_u32, 0x0022_0002_u32);
    manager
        .write()
        .install_vtable(sup as u64, vec![Some(entry(sup as u64))]);
    manager
        .write()
        .install_vtable(sub as u64, vec![Some(entry(sub as u64))]);

    vtable_override_adapter_scoped(
        ClassHandle::from_parts(
            VmId::from_raw(vm_identity),
            0,
            crate::classloading::ClassId::new(sup),
        ),
        0,
    );

    let mgr = manager.read();
    assert_eq!(
        mgr.resolve_virtual_slot(sup as u64, 0)
            .map(|e| e.declaring_class_id),
        Some(sup as u64),
        "a receiver of the super's own class still selects its method"
    );
    assert_eq!(
        mgr.resolve_virtual_slot(sub as u64, 0)
            .map(|e| e.declaring_class_id),
        Some(sub as u64)
    );
}

// -- zs s3/fp3 (2026-10-14): shared entry blocks and signature indices --

fn zs3fp3_slot(
    declaring: u32,
    index: u32,
    name: &str,
    desc: &str,
    body: Option<(&str, bool)>,
) -> cratonvm_classloading::VtableSlotDescriptor {
    cratonvm_classloading::VtableSlotDescriptor::new(cratonvm_classloading::VtableSlotData {
        declaring_class_id: declaring,
        method_index: index,
        method_name: Arc::<str>::from(name),
        descriptor: Arc::<str>::from(desc),
        dispatch: body.map(|(class_name, is_native)| {
            Arc::new(cratonvm_classloading::VtableMethodSnapshot {
                class_name: Arc::from(class_name),
                source_file: None,
                code: cratonvm_reader::ByteView::from_vec(vec![0xb1_u8, index as u8]),
                exception_table: vec![],
                max_stack: 1,
                max_locals: 1,
                num_params: 0,
                is_synchronized: false,
                is_static: false,
                is_native,
            })
        }),
    })
}

fn zs3fp3_block(mgr: &VtableManager, class_id: u32, slot: usize) -> Arc<VtableEntry> {
    Arc::clone(
        mgr.tables[&(class_id as u64)].entries[slot]
            .as_ref()
            .expect("slot populated"),
    )
}

/// An inherited slot shares its declaring class's entry block, an
/// override-only class shares its ancestor's signature index, and every
/// answer (lookups, entries, invalidation, redefinition, unloading) is
/// the one private copies gave.
#[test]
fn zs3fp3_inherited_vtable_entries_share_the_declaring_block() {
    const P: u32 = 73_101; // declares run (bytecode), nat (native), abs (abstract)
    const C: u32 = 73_102; // inherits all three, appends extra
    const O: u32 = 73_103; // overrides run only: same layout as P
    const D: u32 = 73_104; // linked after P's invalidation
    // The eager arm explicitly (zt t2/fp): this test reads `resolved_method`.
    let manager = RwLock::new(VtableManager::with_lazy_dispatch(false));
    let run = zs3fp3_slot(P, 0, "run", "()V", Some(("pkg/P", false)));
    let nat = zs3fp3_slot(P, 1, "nat", "()V", Some(("pkg/P", true)));
    let abs = zs3fp3_slot(P, 2, "abs", "()I", None);
    install_vtable_descriptors(
        &manager,
        P,
        vec![Some(run.clone()), Some(nat.clone()), Some(abs.clone())],
    );
    install_vtable_descriptors(
        &manager,
        C,
        vec![
            Some(run.clone()),
            Some(nat.clone()),
            Some(abs.clone()),
            Some(zs3fp3_slot(C, 0, "extra", "()V", Some(("pkg/C", false)))),
        ],
    );
    install_vtable_descriptors(
        &manager,
        O,
        vec![
            Some(zs3fp3_slot(O, 0, "run", "()V", Some(("pkg/O", false)))),
            Some(nat.clone()),
            Some(abs.clone()),
        ],
    );
    {
        let mgr = manager.read();
        for slot in 0..3 {
            assert!(
                Arc::ptr_eq(&zs3fp3_block(&mgr, P, slot), &zs3fp3_block(&mgr, C, slot)),
                "C slot {slot} shares P's block"
            );
        }
        assert!(!Arc::ptr_eq(
            &zs3fp3_block(&mgr, P, 0),
            &zs3fp3_block(&mgr, O, 0)
        ));
        assert!(Arc::ptr_eq(
            &zs3fp3_block(&mgr, P, 1),
            &zs3fp3_block(&mgr, O, 1)
        ));
        // The index: O (override only) shares P's; C (one more slot) has its own.
        let ix = |c: u32| Arc::as_ptr(&mgr.tables[&(c as u64)].fast_lookup);
        assert_eq!(ix(O), ix(P));
        assert_ne!(ix(C), ix(P));
        // Same answers as a private build of each table.
        let three: &[(&str, &str)] = &[("run", "()V"), ("nat", "()V"), ("abs", "()I")];
        let four: &[(&str, &str)] = &[
            ("run", "()V"),
            ("nat", "()V"),
            ("abs", "()I"),
            ("extra", "()V"),
        ];
        for (c, sigs) in [(P, three), (C, four), (O, three)] {
            let vt = mgr.get_vtable(c as u64).unwrap();
            let own = SigIndex::build(&vt.entries);
            for (slot, (n, d)) in sigs.iter().enumerate() {
                assert_eq!(vt.lookup_slot(n, d), Some(slot), "{c} {n}{d}");
                assert_eq!(own.get(fast_lookup_key(n, d)), Some(&[slot as u32][..]));
            }
            assert_eq!(vt.lookup_slot("extra", "()I"), None);
        }
        assert_eq!(
            mgr.get_vtable(O as u64)
                .unwrap()
                .lookup_slot("extra", "()V"),
            None
        );
        let c0 = mgr.resolve_virtual_slot(C as u64, 0).unwrap();
        assert_eq!(c0.declaring_class_id, P as u64);
        assert!(c0.resolved_method.is_some());
        // Two snapshots with no handlers share one empty exception table.
        let c3 = mgr.resolve_virtual_slot(C as u64, 3).unwrap();
        assert!(Arc::ptr_eq(
            &c0.resolved_method.as_ref().unwrap().exception_table,
            &c3.resolved_method.as_ref().unwrap().exception_table
        ));
        assert!(c3
            .resolved_method
            .as_ref()
            .unwrap()
            .exception_table
            .is_empty());
        assert!(mgr.resolve_virtual_slot(C as u64, 1).unwrap().is_native);
        assert!(mgr
            .resolve_virtual_slot(C as u64, 2)
            .unwrap()
            .resolved_method
            .is_none());
        assert_eq!(
            mgr.resolve_virtual_slot(O as u64, 0)
                .unwrap()
                .declaring_class_id,
            O as u64
        );
    }

    // A write to one table's shared block copies it: only that table sees it.
    manager
        .write()
        .get_vtable_mut(C as u64)
        .unwrap()
        .invalidate_slot(1);
    {
        let mgr = manager.read();
        assert!(mgr.vtable_entry_ref(C as u64, 1).is_none());
        assert!(mgr.vtable_entry_ref(P as u64, 1).is_some());
        assert!(mgr.vtable_entry_ref(O as u64, 1).is_some());
        assert!(!Arc::ptr_eq(
            &zs3fp3_block(&mgr, P, 1),
            &zs3fp3_block(&mgr, C, 1)
        ));
    }

    // Redefinition: the subclasses' inherited slots follow P's new block.
    let run2 = zs3fp3_slot(P, 5, "run", "()V", Some(("pkg/P", false)));
    install_vtable_descriptors(
        &manager,
        P,
        vec![Some(run2), Some(nat.clone()), Some(abs.clone())],
    );
    {
        let mgr = manager.read();
        assert_eq!(
            mgr.resolve_virtual_slot(C as u64, 0).unwrap().method_index,
            5
        );
        assert!(Arc::ptr_eq(
            &zs3fp3_block(&mgr, P, 0),
            &zs3fp3_block(&mgr, C, 0)
        ));
        assert_eq!(
            mgr.resolve_virtual_slot(O as u64, 0)
                .unwrap()
                .declaring_class_id,
            O as u64
        );
    }

    // invalidate_class clears every table's copy; a class linked after it
    // gets a resolved block of its own, not the cleared shared one.
    manager.write().invalidate_class(P as u64);
    install_vtable_descriptors(
        &manager,
        D,
        vec![Some(run.clone()), Some(nat.clone()), Some(abs.clone())],
    );
    {
        let mgr = manager.read();
        for c in [P, C, O] {
            assert!(
                mgr.vtable_entry_ref(c as u64, 2).is_none(),
                "{c} abs cleared"
            );
        }
        assert!(
            mgr.vtable_entry_ref(O as u64, 0).is_some(),
            "O's own override stays"
        );
        for slot in 0..3 {
            assert!(
                mgr.vtable_entry_ref(D as u64, slot).is_some(),
                "D slot {slot}"
            );
        }
    }

    // Unloading P clears the inherited slots of C and D alike.
    manager.write().unload_classes(&[P as u64]);
    let mgr = manager.read();
    assert!(mgr.get_vtable(P as u64).is_none());
    for c in [C, D] {
        for slot in 0..3 {
            assert!(mgr.vtable_entry_ref(c as u64, slot).is_none(), "{c} {slot}");
        }
    }
    assert!(mgr.vtable_entry_ref(C as u64, 3).is_some());
    assert!(mgr.vtable_entry_ref(O as u64, 0).is_some());
    let census: std::collections::BTreeMap<_, _> = mgr.footprint_census().into_iter().collect();
    assert_eq!(census["tables"], 3);
    assert!(census["entry_blocks"] > 0 && census["sig_index"] > 0);
}

/// zt t2/fp (`CRATONVM_VTABLE_LAZY_DISPATCH`): no snapshot is built at
/// link; the first request builds one per METHOD (an inherited slot gets
/// its declaring method's), with the eager arm's contents; a redefinition
/// gets a fresh cell, a replay of the same snapshot keeps the built one,
/// an inherited slot carrying another snapshot is built at link as
/// before, and an unload forgets the class's cells.
#[test]
fn zt2fp_lazy_dispatch_builds_one_snapshot_per_method_on_first_use() {
    const P: u32 = 74_201; // declares run (bytecode), nat (native), abs (abstract)
    const C: u32 = 74_202; // inherits all three, appends extra
    const O: u32 = 74_203; // overrides run only
    const Q: u32 = 74_204; // inherits run through another snapshot block
    let run = zs3fp3_slot(P, 0, "run", "()V", Some(("pkg/P", false)));
    let nat = zs3fp3_slot(P, 1, "nat", "()V", Some(("pkg/P", true)));
    let abs = zs3fp3_slot(P, 2, "abs", "()I", None);
    let extra = zs3fp3_slot(C, 0, "extra", "()V", Some(("pkg/C", false)));
    let o_run = zs3fp3_slot(O, 0, "run", "()V", Some(("pkg/O", false)));
    let install_all = |manager: &RwLock<VtableManager>| {
        install_vtable_descriptors(
            manager,
            P,
            vec![Some(run.clone()), Some(nat.clone()), Some(abs.clone())],
        );
        install_vtable_descriptors(
            manager,
            C,
            vec![
                Some(run.clone()),
                Some(nat.clone()),
                Some(abs.clone()),
                Some(extra.clone()),
            ],
        );
        install_vtable_descriptors(
            manager,
            O,
            vec![Some(o_run.clone()), Some(nat.clone()), Some(abs.clone())],
        );
    };
    let lazy = RwLock::new(VtableManager::with_lazy_dispatch(true));
    let eager = RwLock::new(VtableManager::with_lazy_dispatch(false));
    install_all(&lazy);
    install_all(&eager);

    {
        let mgr = lazy.read();
        // Nothing built at link: every bytecode slot is deferred.
        for (c, slot) in [(P, 0), (C, 0), (C, 3), (O, 0)] {
            assert!(
                zs3fp3_block(&mgr, c, slot).resolved_method.is_none(),
                "{c} slot {slot} deferred"
            );
        }
        assert_eq!(mgr.lazy.len(), 3, "run, extra and O.run");
        let census: std::collections::BTreeMap<_, _> =
            mgr.footprint_census().into_iter().collect();
        assert_eq!(census["dispatch_snapshots"], 0);
        assert_eq!(census["padded_code"], 0);
        assert!(census["lazy_cells"] > 0);
        // The inherited slot still shares the declaring block.
        assert!(Arc::ptr_eq(
            &zs3fp3_block(&mgr, P, 0),
            &zs3fp3_block(&mgr, C, 0)
        ));
        // First use: one snapshot for the method, whichever slot asks.
        let via_c = mgr.resolve_virtual_slot_method(C as u64, 0).unwrap();
        let via_p = mgr.resolve_virtual_slot_method(P as u64, 0).unwrap();
        let by_name = mgr.resolve_virtual_method(P as u64, "run", "()V").unwrap();
        assert!(Arc::ptr_eq(&via_c, &via_p) && Arc::ptr_eq(&via_p, &by_name));
        let via_o = mgr.resolve_virtual_slot_method(O as u64, 0).unwrap();
        assert!(!Arc::ptr_eq(&via_o, &via_p), "O.run is its own method");
        // Abstract and native slots have none, as on the eager arm.
        assert!(mgr.resolve_virtual_slot_method(C as u64, 1).is_none());
        assert!(mgr.resolve_virtual_slot_method(C as u64, 2).is_none());
        // Same contents as the eager arm's link-time snapshot.
        let em = eager.read();
        for (c, slot) in [(P, 0), (C, 0), (C, 3), (O, 0)] {
            let l = mgr.resolve_virtual_slot_method(c as u64, slot).unwrap();
            let e = em.resolve_virtual_slot_method(c as u64, slot).unwrap();
            assert_eq!(l.declaring_class_id, e.declaring_class_id, "{c} {slot}");
            assert_eq!(&*l.class_name, &*e.class_name);
            assert_eq!(&*l.method_name, &*e.method_name);
            assert_eq!(&*l.method_descriptor, &*e.method_descriptor);
            assert_eq!(&*l.code, &*e.code);
            assert_eq!(l.max_stack, e.max_stack);
            assert_eq!(l.max_locals, e.max_locals);
        }
        let census: std::collections::BTreeMap<_, _> =
            mgr.footprint_census().into_iter().collect();
        assert!(census["dispatch_snapshots"] > 0, "built ones are counted");
    }

    // A replay of the same descriptors keeps the built snapshot.
    let before = lazy.read().resolve_virtual_slot_method(P as u64, 0).unwrap();
    install_vtable_descriptors(
        &lazy,
        P,
        vec![Some(run.clone()), Some(nat.clone()), Some(abs.clone())],
    );
    let after = lazy.read().resolve_virtual_slot_method(C as u64, 0).unwrap();
    assert!(Arc::ptr_eq(&before, &after), "replay keeps the cell");

    // A redefinition (a new snapshot) gets a fresh cell: the subclass
    // follows the new body.
    let run2 = zs3fp3_slot(P, 5, "run", "()V", Some(("pkg/P", false)));
    install_vtable_descriptors(
        &lazy,
        P,
        vec![Some(run2), Some(nat.clone()), Some(abs.clone())],
    );
    {
        let mgr = lazy.read();
        let redefined = mgr.resolve_virtual_slot_method(C as u64, 0).unwrap();
        assert!(!Arc::ptr_eq(&redefined, &before));
        assert_eq!(redefined.code[1], 5, "the new body");
        assert!(Arc::ptr_eq(
            &redefined,
            &mgr.resolve_virtual_slot_method(P as u64, 0).unwrap()
        ));
    }

    // zt t3/x2: a redefinition that KEEPS the method index replaces the
    // recorded cell itself. (The arm above moved the index from 0 to 5,
    // so it recorded a second key and never reached the replacement: the
    // subclass followed the refresh sweep's new block, whatever
    // `defer_dispatch` did with the old cell.)
    let built = lazy.read().resolve_virtual_slot_method(P as u64, 0).unwrap();
    let keys = lazy.read().lazy.len();
    let run3 = zs3fp3_slot(P, 5, "run", "()V", Some(("pkg/P", false)));
    install_vtable_descriptors(
        &lazy,
        P,
        vec![Some(run3), Some(nat.clone()), Some(abs.clone())],
    );
    {
        let mgr = lazy.read();
        assert_eq!(mgr.lazy.len(), keys, "same key: replaced, not added");
        let renewed = mgr.resolve_virtual_slot_method(C as u64, 0).unwrap();
        assert!(!Arc::ptr_eq(&renewed, &built), "a fresh cell builds anew");
        assert!(Arc::ptr_eq(
            &renewed,
            &mgr.resolve_virtual_slot_method(P as u64, 0).unwrap()
        ));
    }

    // An inherited slot carrying a snapshot block other than the one
    // recorded for its method is built at link, as on the eager arm.
    let other_run = zs3fp3_slot(P, 5, "run", "()V", Some(("pkg/P", false)));
    install_vtable_descriptors(&lazy, Q, vec![Some(other_run)]);
    assert!(zs3fp3_block(&lazy.read(), Q, 0).resolved_method.is_some());

    // Unloading P forgets its cells, and only its.
    lazy.write().unload_classes(&[P as u64]);
    {
        let mgr = lazy.read();
        assert!(mgr.lazy.keys().all(|&key| lazy_key_class(key) != P as u64));
        assert_eq!(mgr.lazy.len(), 2, "extra and O.run stay");
        assert!(mgr.resolve_virtual_slot_method(C as u64, 0).is_none());
        assert!(mgr.resolve_virtual_slot_method(C as u64, 3).is_some());
    }
    lazy.write().unload_class(O as u64);
    assert_eq!(lazy.read().lazy.len(), 1, "extra stays");
}

/// zt t6/rv3 (`zgc-zt3x2-*` item 3): a redefinition that moves a
/// method's index drops the old index's cell instead of keeping it, with
/// its snapshot, until the class unloads; a method still declared at its
/// index keeps its cell and built snapshot, and another class's cells are
/// untouched.
#[test]
fn zt6rv3_an_index_moving_redefinition_drops_the_old_cell() {
    const P: u32 = 74_401;
    const C: u32 = 74_402;
    let run = zs3fp3_slot(P, 0, "run", "()V", Some(("pkg/P", false)));
    let keep = zs3fp3_slot(P, 1, "keep", "()V", Some(("pkg/P", false)));
    let extra = zs3fp3_slot(C, 0, "extra", "()V", Some(("pkg/C", false)));
    let lazy = RwLock::new(VtableManager::with_lazy_dispatch(true));
    install_vtable_descriptors(&lazy, P, vec![Some(run.clone()), Some(keep.clone())]);
    install_vtable_descriptors(
        &lazy,
        C,
        vec![Some(run.clone()), Some(keep.clone()), Some(extra)],
    );
    assert_eq!(lazy.read().lazy.len(), 3, "P.run, P.keep, C.extra");
    let kept = lazy.read().resolve_virtual_slot_method(P as u64, 1).unwrap();
    let run5 = zs3fp3_slot(P, 5, "run", "()V", Some(("pkg/P", false)));
    install_vtable_descriptors(&lazy, P, vec![Some(run5), Some(keep.clone())]);
    let mgr = lazy.read();
    assert_eq!(mgr.lazy.len(), 3, "the index-0 cell is dropped, not kept beside 5");
    assert!(!mgr.lazy.contains_key(&lazy_key(P as u64, 0)));
    assert!(mgr.lazy.contains_key(&lazy_key(P as u64, 5)));
    assert!(mgr.lazy.contains_key(&lazy_key(C as u64, 0)), "C's cell stays");
    assert!(
        Arc::ptr_eq(&kept, &mgr.resolve_virtual_slot_method(P as u64, 1).unwrap()),
        "a method still declared at its index keeps its built snapshot"
    );
    assert_eq!(
        mgr.resolve_virtual_slot_method(P as u64, 0).unwrap().code[1],
        5,
        "slot 0 dispatches to the redefined body"
    );
}

/// zt t6/rv3 (`zgc-zt3x2-*` item 4): with both
/// `CRATONVM_VTABLE_LAZY_DISPATCH` and
/// `CRATONVM_DEFINE_VTABLE_INHERIT_TEMPLATE`, an all-inherited class whose
/// declaring slot is deferred takes the template (it never matched: the
/// bytecode arm needed a built snapshot). A snapshot block other than the
/// recorded one still refuses (negative control).
#[test]
fn zt6rv3_the_inherit_template_accepts_a_deferred_declaring_slot() {
    const P: u32 = 74_601;
    const C: u32 = 74_602;
    let run = zs3fp3_slot(P, 0, "run", "()V", Some(("pkg/P", false)));
    let nat = zs3fp3_slot(P, 1, "nat", "()V", Some(("pkg/P", true)));
    let lazy = RwLock::new(VtableManager::with_lazy_dispatch(true));
    lazy.write().inherit_template = true;
    install_vtable_descriptors(&lazy, P, vec![Some(run.clone()), Some(nat.clone())]);
    {
        let mgr = lazy.read();
        assert!(zs3fp3_block(&mgr, P, 0).resolved_method.is_none(), "P.run deferred");
        let (table, _) = mgr
            .inherited_template(C as u64, &[Some(run.clone()), Some(nat.clone())])
            .expect("the template matches a deferred declaring slot");
        assert!(Arc::ptr_eq(table[0].as_ref().unwrap(), &zs3fp3_block(&mgr, P, 0)));
        let other = zs3fp3_slot(P, 0, "run", "()V", Some(("pkg/P", false)));
        assert!(
            mgr.inherited_template(C as u64, &[Some(other), Some(nat.clone())]).is_none(),
            "another snapshot block is not the recorded one"
        );
    }
    install_vtable_descriptors(&lazy, C, vec![Some(run), Some(nat)]);
    let mgr = lazy.read();
    let via_c = mgr.resolve_virtual_slot_method(C as u64, 0).unwrap();
    let via_p = mgr.resolve_virtual_slot_method(P as u64, 0).unwrap();
    assert!(Arc::ptr_eq(&via_c, &via_p), "one snapshot for the method");
}

/// zt t6/rv3 (`zgc-zt3x2-*` item 5): the line reports the manager it is
/// given: its own latch and its own counters, so two VMs in one process
/// do not sum.
#[test]
fn zt6rv3_the_lazy_dispatch_line_reads_its_manager() {
    const P: u32 = 74_501;
    let run = zs3fp3_slot(P, 0, "run", "()V", Some(("pkg/P", false)));
    let on = RwLock::new(VtableManager::with_lazy_dispatch(true));
    let off = RwLock::new(VtableManager::with_lazy_dispatch(false));
    install_vtable_descriptors(&on, P, vec![Some(run.clone())]);
    install_vtable_descriptors(&off, P, vec![Some(run)]);
    assert_eq!(
        lazy_dispatch_line(Some(&on.read())),
        "[vm] vtable-lazy-dispatch: on=1 deferred=1 built=0"
    );
    on.read().resolve_virtual_slot_method(P as u64, 0).unwrap();
    assert_eq!(
        lazy_dispatch_line(Some(&on.read())),
        "[vm] vtable-lazy-dispatch: on=1 deferred=1 built=1"
    );
    assert_eq!(
        lazy_dispatch_line(Some(&off.read())),
        "[vm] vtable-lazy-dispatch: on=0 deferred=0 built=0"
    );
    assert_eq!(
        lazy_dispatch_line(None),
        "[vm] vtable-lazy-dispatch: on=0 deferred=0 built=0"
    );
}

/// zt t2/fp: the switch off defers nothing and keeps no cell.
#[test]
fn zt2fp_lazy_dispatch_off_builds_at_link() {
    const P: u32 = 74_301;
    let manager = RwLock::new(VtableManager::with_lazy_dispatch(false));
    let run = zs3fp3_slot(P, 0, "run", "()V", Some(("pkg/P", false)));
    install_vtable_descriptors(&manager, P, vec![Some(run)]);
    let mgr = manager.read();
    assert!(mgr.lazy.is_empty());
    assert!(zs3fp3_block(&mgr, P, 0).resolved_method.is_some());
    let census: std::collections::BTreeMap<_, _> = mgr.footprint_census().into_iter().collect();
    assert_eq!(census["lazy_cells"], 0);
}
