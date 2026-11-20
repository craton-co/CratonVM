use cratonvm_types::ClassId;

/// The row whose VALUE was unloaded must go: handing that `ClassId` back
/// out of the cache is what allocated an instance against a class the
/// class manager had already removed.
#[test]
fn an_unloaded_generated_proxy_class_is_dropped_from_the_cache() {
    const VM: usize = 0xC0FFEE;
    {
        let mut guard = super::PROXY_CLASS_CACHE.write();
        let map = guard.get_or_insert_with(rustc_hash::FxHashMap::default);
        map.insert((VM, 7, vec![ClassId::new(100)]), ClassId::new(3003));
        map.insert((VM, 7, vec![ClassId::new(101)]), ClassId::new(3005));
    }
    super::forget_unloaded_proxy_classes(VM, &[3003]);
    let guard = super::PROXY_CLASS_CACHE.read();
    let map = guard.as_ref().expect("cache populated above");
    assert!(map.get(&(VM, 7, vec![ClassId::new(100)])).is_none());
    assert_eq!(
        map.get(&(VM, 7, vec![ClassId::new(101)])),
        Some(&ClassId::new(3005)),
        "a live row must survive the purge",
    );
}

/// And the row whose KEY names an unloaded interface: it can only be
/// matched again through `ClassId` reuse, and matching by reuse is the
/// same defect one step removed.
#[test]
fn a_row_keyed_on_an_unloaded_interface_is_dropped_too() {
    const VM: usize = 0xC0FFEF;
    {
        let mut guard = super::PROXY_CLASS_CACHE.write();
        let map = guard.get_or_insert_with(rustc_hash::FxHashMap::default);
        map.insert(
            (VM, 9, vec![ClassId::new(200), ClassId::new(201)]),
            ClassId::new(4000),
        );
    }
    super::forget_unloaded_proxy_classes(VM, &[201]);
    let guard = super::PROXY_CLASS_CACHE.read();
    let map = guard.as_ref().expect("cache populated above");
    assert!(map
        .get(&(VM, 9, vec![ClassId::new(200), ClassId::new(201)]))
        .is_none());
}

/// Another VM's rows are none of this VM's business — the same partition
/// the `vm_identity` key component exists to keep.
#[test]
fn another_vms_rows_are_untouched() {
    const MINE: usize = 0xD0D0;
    const THEIRS: usize = 0xE0E0;
    {
        let mut guard = super::PROXY_CLASS_CACHE.write();
        let map = guard.get_or_insert_with(rustc_hash::FxHashMap::default);
        map.insert((THEIRS, 1, vec![ClassId::new(300)]), ClassId::new(5000));
    }
    super::forget_unloaded_proxy_classes(MINE, &[5000, 300]);
    let guard = super::PROXY_CLASS_CACHE.read();
    let map = guard.as_ref().expect("cache populated above");
    assert_eq!(
        map.get(&(THEIRS, 1, vec![ClassId::new(300)])),
        Some(&ClassId::new(5000)),
    );
}
