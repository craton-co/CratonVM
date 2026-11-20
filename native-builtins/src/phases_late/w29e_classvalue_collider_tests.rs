use super::*;
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeHeapAccess, NativeInvokeAccess,
    NativeSystemAccess, NativeThreadAccess,
};

fn at(addr: usize) -> ObjectRef {
    // SAFETY: never dereferenced; the mock hashes the address.
    unsafe { ObjectRef::from_raw(addr as *mut u8) }
}

struct Teardown(usize);
impl Drop for Teardown {
    fn drop(&mut self) {
        forget_vm_classvalue_cache(self.0);
        crate::forget_vm_lock_keys(self.0);
    }
}

fn entry(cls: ObjectRef, value: ObjectRef) -> ClassValueCacheEntry {
    ClassValueCacheEntry {
        owner_class_id: None,
        mirror_addr: cls.as_ptr() as usize,
        value,
    }
}

/// Two ClassValues with one hash, and two Class mirrors with one hash:
/// every `(ClassValue, Class)` pair answers its own value, `remove` drops
/// only its own, and a moved mirror is still found after the remap.
#[test]
fn same_hash_class_values_and_classes_keep_their_own_values() {
    const VM: usize = 0x29E0_1701;
    let _t = Teardown(VM);
    let ctx = crate::test_utils::mock_ctx();
    ctx.set_vm_identity(VM);
    let (cv_a, cv_b) = (at(0x1_29E0_1700), at(0x2_29E0_1700));
    let (cls_1, cls_2) = (at(0x1_29E0_1780), at(0x2_29E0_1780));
    assert_eq!(ctx.identity_hash_code(cv_a), ctx.identity_hash_code(cv_b));
    assert_eq!(ctx.identity_hash_code(cls_1), ctx.identity_hash_code(cls_2));
    let (v_a1, v_a2, v_b1) = (at(0x29E0_1710), at(0x29E0_1720), at(0x29E0_1730));

    let key_a = classvalue_key(&ctx, cv_a, cls_1).expect("minted");
    assert_eq!(
        existing_classvalue_key(&ctx, cv_b, cls_1),
        None,
        "B never keyed"
    );
    let key_b = classvalue_key(&ctx, cv_b, cls_1).expect("minted");
    assert_ne!(key_a, key_b);
    assert_eq!(classvalue_key(&ctx, cv_a, cls_2), Some(key_a), "one bucket");

    classvalue_publish(VM, key_a, entry(cls_1, v_a1));
    assert_eq!(classvalue_cached(VM, key_b, cls_1), None);
    assert_eq!(classvalue_cached(VM, key_a, cls_2), None);
    classvalue_publish(VM, key_a, entry(cls_2, v_a2));
    classvalue_publish(VM, key_b, entry(cls_1, v_b1));
    assert_eq!(classvalue_cached(VM, key_a, cls_1), Some(v_a1));
    assert_eq!(classvalue_cached(VM, key_a, cls_2), Some(v_a2));
    assert_eq!(classvalue_cached(VM, key_b, cls_1), Some(v_b1));

    // A real move keeps the identity hash: 4 GiB up keeps the mock's.
    let moved_to = cls_2.as_ptr() as usize + 0x1_0000_0000;
    let mut map = cratonvm_types::PointerMap::new();
    map.insert(cls_2.as_ptr() as usize, moved_to);
    gc_update_classvalue_cache_refs(VM, &map);
    assert_eq!(classvalue_cached(VM, key_a, at(moved_to)), Some(v_a2));
    assert_eq!(classvalue_cached(VM, key_a, cls_2), None);

    classvalue_remove(VM, key_a, cls_1);
    assert_eq!(classvalue_cached(VM, key_a, cls_1), None);
    assert_eq!(classvalue_cached(VM, key_a, at(moved_to)), Some(v_a2));
    assert_eq!(classvalue_cached(VM, key_b, cls_1), Some(v_b1));
}

/// A dead ClassValue's rows go when the lock-key sweep frees its key (they
/// used to stay rooted until the class unloaded); a live collider's stay.
#[test]
fn a_dead_class_values_rows_go_with_its_key() {
    const VM: usize = 0x29E0_1702;
    let _t = Teardown(VM);
    let ctx = crate::test_utils::mock_ctx();
    ctx.set_vm_identity(VM);
    let (dead, live) = (at(0x1_29E0_1708), at(0x2_29E0_1708));
    let cls = at(0x29E0_1788);
    let key_dead = classvalue_key(&ctx, dead, cls).expect("minted");
    let key_live = classvalue_key(&ctx, live, cls).expect("minted");
    classvalue_publish(VM, key_dead, entry(cls, at(0x29E0_1740)));
    classvalue_publish(VM, key_live, entry(cls, at(0x29E0_1748)));

    let live_addr = live.as_ptr() as usize;
    crate::gc_sweep_lock_keys(VM, &|x| x == live_addr);
    assert_eq!(existing_classvalue_key(&ctx, dead, cls), None);
    assert_eq!(classvalue_cached(VM, key_dead, cls), None);
    assert_eq!(classvalue_cached(VM, key_live, cls), Some(at(0x29E0_1748)));
    let mut roots = Vec::new();
    gc_scan_classvalue_cache_roots(VM, &mut roots, &|_| false, &|cid| {
        cratonvm_types::loader_pin::loader_pin_addr_for_vm(VM, cid)
    });
    assert_eq!(
        roots,
        vec![at(0x29E0_1748)],
        "the dead one's value is no root"
    );
}

/// gcd d2/g (`gengc-r5w4-conc8-native-root-deferrals-ignore-the-unload-classification`):
/// a deferrable value is pinned to the owner `deferral_owner` names, and
/// ROOTED when it names none (a class the unload never removes), even
/// though the class has a loader.
#[test]
fn a_class_value_defers_only_to_the_owner_the_vm_names() {
    const VM: usize = 0x29E0_1703;
    let _t = Teardown(VM);
    struct Pins;
    impl Drop for Pins {
        fn drop(&mut self) {
            cratonvm_types::metadata_pin::forget_vm_metadata_pins(VM);
        }
    }
    let _p = Pins;
    let ctx = crate::test_utils::mock_ctx();
    ctx.set_vm_identity(VM);
    let (cv, cls, value) = (at(0x1_29E0_1710), at(0x29E0_1790), at(0x29E0_1750));
    let loader: usize = 0x29E0_17C0;
    let key = classvalue_key(&ctx, cv, cls).expect("minted");
    classvalue_publish(
        VM,
        key,
        ClassValueCacheEntry {
            owner_class_id: Some(0x7ff0_6d01),
            mirror_addr: cls.as_ptr() as usize,
            value,
        },
    );
    let pinned = || {
        cratonvm_types::metadata_pin::roots_for_loader(loader)
            .is_some_and(|p| p.contains(&(value.as_ptr() as usize)))
    };

    let mut roots = Vec::new();
    gc_scan_classvalue_cache_roots(VM, &mut roots, &|_| true, &|_| None);
    assert_eq!(roots, vec![value], "no deferral owner: rooted");
    assert!(!pinned());

    let mut roots = Vec::new();
    gc_scan_classvalue_cache_roots(VM, &mut roots, &|_| true, &|cid| {
        (cid == 0x7ff0_6d01).then_some(loader)
    });
    assert!(
        roots.is_empty(),
        "an owner the VM names: deferred, not rooted"
    );
    assert!(pinned(), "...and pinned to that loader");
}
