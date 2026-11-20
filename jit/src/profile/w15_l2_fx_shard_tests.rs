use super::*;

/// The owned-key and borrowed-key paths must agree on the shard, or a
/// borrowed lookup would miss a slot the owned path inserted.
#[test]
fn fx_shard_paths_agree() {
    let mut seen = [false; PROFILE_SHARDS];
    for cid in 0..64u32 {
        for (n, d) in [
            ("lock", "()V"),
            ("tryRelease", "(I)Z"),
            ("get", "()Ljava/lang/Object;"),
        ] {
            let key = MethodKey {
                class_id: cid,
                method_name: Arc::from(n),
                descriptor: Arc::from(d),
            };
            let (shard, _) = shard_and_fingerprint_for_borrowed(cid, n, d);
            assert_eq!(shard_for_key(&key), shard, "{cid} {n}{d}");
            seen[shard] = true;
        }
    }
    // 192 triples spread over more than half of the 16 shards.
    assert!(seen.iter().filter(|s| **s).count() > PROFILE_SHARDS / 2);
}

/// A slot inserted through the borrowed path is found again through it,
/// and the collision counter stays at zero.
#[test]
fn borrowed_lookup_round_trips_under_fx() {
    let store = ProfileStore::new();
    let n: Arc<str> = Arc::from("m");
    let d: Arc<str> = Arc::from("()V");
    let a = store.get_or_insert_borrowed(7, &n, &d);
    let b = store.get_or_insert_borrowed(7, &n, &d);
    assert!(Arc::ptr_eq(&a, &b));
    let c = store.get_or_insert_borrowed(8, &n, &d);
    assert!(!Arc::ptr_eq(&a, &c));
    assert_eq!(store.name_index_collisions(), 0);
}
