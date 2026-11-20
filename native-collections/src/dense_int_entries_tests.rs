use super::*;
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};

fn object(address: usize) -> ObjectRef {
    // The entry store treats refs as opaque values; these aligned sentinels
    // are never dereferenced by this unit test.
    unsafe { ObjectRef::from_raw(address as *mut u8) }
}

/// r13w6 hashtree: the overlay's iteration order is computed over the
/// table the map's first insert allocates. Three keys that share a bucket
/// of a 4-bucket table (`new HashMap<>(4)`) iterate in insertion order
/// there, and in bucket order over the historical 16.
#[test]
fn r13_hashtree_overlay_order_starts_from_the_maps_own_capacity() {
    let mut entries = DenseIntEntries::default();
    for k in [-11, -7, -3] {
        assert_eq!(entries.insert(k, (object(0x1000), Value::Int(k))), None);
    }
    // spread(-11) & 15 = 10, spread(-7) & 15 = 6, spread(-3) & 15 = 2.
    assert_eq!(entries.keys_in_java_hashmap_order(0), vec![-3, -7, -11]);
    assert_eq!(entries.keys_in_java_hashmap_order(16), vec![-3, -7, -11]);
    // ... and all three are bucket 2 of 4.
    assert_eq!(entries.keys_in_java_hashmap_order(4), vec![-11, -7, -3]);
}

/// r13w10 compat5 (`CRATONVM_COMPAT_OVERLAY_REMAP_ORDER`): a
/// compute-family insert links the new key at its bucket's HEAD. HotSpot:
/// `m.put(1, ..); m.computeIfAbsent(17, ..)` iterates `{17, 1}`, and a
/// later `put(33)` (same bucket) goes to the tail, a later
/// `computeIfAbsent(49)` to the head again.
#[test]
fn r13_compat5_overlay_remap_insert_heads_its_bucket() {
    let mut entries = DenseIntEntries::default();
    assert_eq!(entries.insert(1, (object(0x1000), Value::Int(1))), None);
    assert_eq!(
        entries.insert_remap(17, (object(0x1008), Value::Int(2)), 16),
        None
    );
    assert_eq!(entries.keys_in_java_hashmap_order(16), vec![17, 1]);
    assert_eq!(entries.insert(33, (object(0x1010), Value::Int(3))), None);
    assert_eq!(
        entries.insert_remap(49, (object(0x1018), Value::Int(4)), 16),
        None
    );
    assert_eq!(entries.keys_in_java_hashmap_order(16), vec![49, 17, 1, 33]);
    // An update through the remap door moves nothing.
    assert_eq!(
        entries.insert_remap(1, (object(0x1020), Value::Int(5)), 16),
        Some((object(0x1000), Value::Int(1)))
    );
    assert_eq!(entries.keys_in_java_hashmap_order(16), vec![49, 17, 1, 33]);
}

/// r13w10 compat5: the table grows on a compute-family insert's ENTRY
/// only. HotSpot: twelve `put(i)` then `merge(17, ..)` keeps 16 buckets
/// (`size > threshold` is `12 > 12` on entry), so 17 heads bucket 1; the
/// next `put(18)` takes the size to 14 and doubles the table, whose split
/// keeps each chain's order.
#[test]
fn r13_compat5_overlay_remap_insert_grows_on_entry_only() {
    let mut entries = DenseIntEntries::default();
    for k in 0..12 {
        assert_eq!(entries.insert(k, (object(0x2000), Value::Int(k))), None);
    }
    assert_eq!(
        entries.insert_remap(17, (object(0x3000), Value::Int(17)), 16),
        None
    );
    let mut want: Vec<i32> = vec![0, 17];
    want.extend(1..12);
    assert_eq!(entries.keys_in_java_hashmap_order(16), want);
    assert_eq!(entries.insert(18, (object(0x3008), Value::Int(18))), None);
    let mut want: Vec<i32> = (0..12).collect();
    want.extend([17, 18]);
    assert_eq!(entries.keys_in_java_hashmap_order(16), want);
    // Size 14 is under 32's threshold of 24, so the table stays; 33 heads
    // bucket 1.
    assert_eq!(
        entries.insert_remap(33, (object(0x3010), Value::Int(33)), 16),
        None
    );
    let mut want: Vec<i32> = vec![0, 33, 1];
    want.extend(2..12);
    want.extend([17, 18]);
    assert_eq!(entries.keys_in_java_hashmap_order(16), want);
}

#[test]
fn dense_and_sparse_integer_keys_preserve_map_semantics() {
    let mut entries = DenseIntEntries::default();
    assert_eq!(entries.insert(0, (object(0x1000), Value::Int(10))), None);
    assert_eq!(entries.insert(1024, (object(0x2000), Value::Int(20))), None);
    assert_eq!(entries.insert(-7, (object(0x3000), Value::Int(30))), None);
    assert_eq!(entries.len(), 3);
    assert_eq!(
        entries.get(&1024).map(|(_, value)| value),
        Some(Value::Int(20))
    );
    assert_eq!(
        entries.insert(1024, (object(0x4000), Value::Int(21))),
        Some((object(0x2000), Value::Int(20)))
    );
    assert_eq!(entries.len(), 3);
    assert!(entries.contains_key(&-7));
    assert_eq!(entries.remove(&-7), Some((object(0x3000), Value::Int(30))));
    assert_eq!(entries.len(), 2);
    let mut keys: Vec<_> = entries.keys().collect();
    keys.sort_unstable();
    assert_eq!(keys, vec![0, 1024]);
}

#[test]
fn insertion_sequence_survives_updates_migration_and_reinsert() {
    let mut entries = DenseIntEntries::default();
    // 2048 starts sparse because it is beyond the initial dense frontier.
    assert_eq!(entries.insert(2048, (object(0x1000), Value::Int(1))), None);
    assert_eq!(entries.insert(0, (object(0x2000), Value::Int(2))), None);
    for key in 1..=1024 {
        assert_eq!(
            entries.insert(key, (object(0x3000 + key as usize * 8), Value::Int(key))),
            None
        );
    }
    let original_seq = entries.insertion_seq(&2048).expect("sparse entry");
    // The now-nearby key migrates into the dense vector and a value update
    // must retain its original position within a bucket.
    assert_eq!(
        entries.insert(2048, (object(0x9000), Value::Int(3))),
        Some((object(0x1000), Value::Int(1)))
    );
    assert_eq!(
        entries.insertion_seq(&2048).expect("migrated entry"),
        original_seq
    );
    assert_eq!(
        entries.insert(2048, (object(0xA000), Value::Int(4))),
        Some((object(0x9000), Value::Int(3)))
    );
    assert_eq!(
        entries.insertion_seq(&2048).expect("updated entry"),
        original_seq
    );

    assert_eq!(entries.remove(&2048), Some((object(0xA000), Value::Int(4))));
    assert_eq!(entries.insert(2048, (object(0xB000), Value::Int(5))), None);
    assert!(
        entries.insertion_seq(&2048).expect("reinserted entry") > original_seq,
        "remove followed by reinsert must create a new bucket-chain position"
    );
}
