use super::*;

fn fake(addr: usize) -> ObjectRef {
    // SAFETY: never dereferenced; only compared by address.
    unsafe { ObjectRef::from_raw(addr as *mut u8) }
}

fn trace_named(addr: usize, method: &str) -> ThrowableStackTrace {
    let frames: Vec<StackTraceEntry> = vec![StackTraceEntry {
        class_name: Arc::from("craton/probe/T"),
        method_name: Arc::from(method),
        method_descriptor: None,
        source_file: None,
        line_number: -1,
        byte_code_index: -1,
        class_id: None,
        method_index: None,
    }];
    ThrowableStackTrace {
        throwable: fake(addr),
        frames: frames.into_iter().map(BacktraceFrame::Entry).collect(),
        captured_at: 0,
    }
}

fn shards() -> Vec<RwLock<crate::vm::realms::thread_realm::ThrowableStackShard>> {
    (0..crate::vm::realms::thread_realm::THROWABLE_STACK_SHARDS)
        .map(|_| RwLock::new(Default::default()))
        .collect()
}

fn insert(
    s: &[RwLock<crate::vm::realms::thread_realm::ThrowableStackShard>],
    t: ThrowableStackTrace,
) {
    let addr = t.throwable.as_ptr() as usize;
    s[throwable_stack_shard(addr)].write().push(addr, t);
}

/// `(method name of the trace, the throwable address it records)` keyed
/// at `addr`, or `None`.
fn at(
    s: &[RwLock<crate::vm::realms::thread_realm::ThrowableStackShard>],
    addr: usize,
) -> Option<(String, usize)> {
    let mut shard = s[throwable_stack_shard(addr)].write();
    shard.flush();
    shard.get(&addr).map(|t| {
        (
            t.frames[0].to_entry().method_name.to_string(),
            t.throwable.as_ptr() as usize,
        )
    })
}

fn len(s: &[RwLock<crate::vm::realms::thread_realm::ThrowableStackShard>]) -> usize {
    s.iter().map(|sh| sh.read().len()).sum()
}

/// Three 8-aligned addresses `a < b < c` in one sliding run with
/// `shard(a) < shard(b)`, so the one-pass loop visited `a`'s shard first
/// and re-inserted into `b`'s shard before draining it.
fn chained_addresses() -> (usize, usize, usize) {
    for base in (0x4_0000usize..0x4_4000).step_by(8) {
        let (a, b, c) = (base, base + 0x80, base + 0x100);
        if throwable_stack_shard(a) < throwable_stack_shard(b) {
            return (a, b, c);
        }
    }
    unreachable!("32 shards over 2048 candidates always yields an ordered pair");
}

/// **A CHAINED MAP IS APPLIED ONCE.** `T1` at `A` slides to `B`, `T2` at
/// `B` slides to `C`: `T1`'s trace must end at `B` and `T2`'s at `C`,
/// whatever order the shards are visited in.
#[test]
fn a_chained_map_moves_each_trace_exactly_once() {
    let s = shards();
    let (a, b, c) = chained_addresses();
    insert(&s, trace_named(a, "t1"));
    insert(&s, trace_named(b, "t2"));
    let map = cratonvm_types::PointerMap::from_iter([(a, b), (b, c)]);

    remap_and_sweep_throwable_shards(&s, &map, &|_| true);

    assert_eq!(
        at(&s, b),
        Some(("t1".to_string(), b)),
        "T1 must be keyed at B"
    );
    assert_eq!(
        at(&s, c),
        Some(("t2".to_string(), c)),
        "T2 must be keyed at C"
    );
    assert_eq!(at(&s, a), None);
    assert_eq!(len(&s), 2);
}

/// A survivor that moved onto a dead Throwable's address wins it, even
/// though the address now parses as a live object.
#[test]
fn a_relocated_survivor_wins_a_dead_entrys_address() {
    let s = shards();
    let (a, b, _) = chained_addresses();
    insert(&s, trace_named(a, "survivor"));
    insert(&s, trace_named(b, "dead"));
    let map = cratonvm_types::PointerMap::from_iter([(a, b)]);

    remap_and_sweep_throwable_shards(&s, &map, &|_| true);

    assert_eq!(at(&s, b), Some(("survivor".to_string(), b)));
    assert_eq!(len(&s), 1);
}

/// Non-moving cycle: dead rows go, live rows stay where they are.
#[test]
fn a_non_moving_sweep_filters_in_place() {
    let s = shards();
    let (a, b, c) = chained_addresses();
    insert(&s, trace_named(a, "live"));
    insert(&s, trace_named(b, "dead"));
    insert(&s, trace_named(c, "live2"));

    remap_and_sweep_throwable_shards(&s, &cratonvm_types::PointerMap::default(), &|addr| {
        addr != b
    });

    assert_eq!(at(&s, a), Some(("live".to_string(), a)));
    assert_eq!(at(&s, b), None);
    assert_eq!(at(&s, c), Some(("live2".to_string(), c)));
}

/// A pending store is the newest: it wins over the indexed row of its
/// address at the flush, and the later of two pending stores wins.
#[test]
fn the_newest_store_of_an_address_wins_across_pending_and_map() {
    let s = shards();
    let (a, b, _) = chained_addresses();
    insert(&s, trace_named(a, "first"));
    assert_eq!(
        at(&s, a),
        Some(("first".to_string(), a)),
        "indexed by the read"
    );
    insert(&s, trace_named(a, "second"));
    insert(&s, trace_named(b, "other"));
    insert(&s, trace_named(a, "third"));
    assert_eq!(at(&s, a), Some(("third".to_string(), a)));
    assert_eq!(at(&s, b), Some(("other".to_string(), b)));
    assert_eq!(len(&s), 2);
}

/// A refill loop does not grow the pending list: one throwable stored over
/// and over keeps one row, and many alternating stores of a few
/// throwables are compacted to one row each.
#[test]
fn repeated_stores_of_a_few_throwables_stay_bounded() {
    let s = shards();
    let (a, _, _) = chained_addresses();
    for _ in 0..10_000 {
        insert(&s, trace_named(a, "refill"));
    }
    assert_eq!(len(&s), 1);
    let shard = throwable_stack_shard(a);
    let same_shard: Vec<usize> = (0..)
        .map(|i| a + i * 8)
        .filter(|&x| throwable_stack_shard(x) == shard)
        .take(3)
        .collect();
    for round in 0..20_000 {
        let addr = same_shard[round % 3];
        insert(
            &s,
            trace_named(addr, if round >= 19_997 { "last" } else { "old" }),
        );
    }
    assert!(len(&s) <= 4096, "pending grew to {}", len(&s));
    for &addr in &same_shard {
        assert_eq!(at(&s, addr), Some(("last".to_string(), addr)));
    }
}

/// A sweep drops a dead pending row without indexing it, keeps a live
/// one, and moves a moved one -- the same answers the map gave.
#[test]
fn a_sweep_settles_pending_rows_like_indexed_ones() {
    let s = shards();
    let (a, b, c) = chained_addresses();
    let far = 0x9_0000usize;
    insert(&s, trace_named(a, "moves"));
    insert(&s, trace_named(b, "stays"));
    insert(&s, trace_named(c, "dies"));
    let map = cratonvm_types::PointerMap::from_iter([(a, far)]);
    remap_and_sweep_throwable_shards(&s, &map, &|addr| addr == b);
    assert_eq!(at(&s, far), Some(("moves".to_string(), far)));
    assert_eq!(at(&s, b), Some(("stays".to_string(), b)));
    assert_eq!(at(&s, c), None);
    assert_eq!(len(&s), 2);
    // Non-moving: pending rows filtered in place.
    insert(&s, trace_named(c, "dies-again"));
    remap_and_sweep_throwable_shards(&s, &cratonvm_types::PointerMap::default(), &|addr| {
        addr != c
    });
    assert_eq!(at(&s, c), None);
    assert_eq!(len(&s), 2);
}

/// The keys of one shard share their low eight bits (the shard is chosen
/// by bits 3..8); their hashes must still spread over a table's low-bit
/// bucket indices. Fx left every one at the same index modulo 256.
#[test]
fn one_shards_keys_spread_over_bucket_indices() {
    use std::hash::BuildHasher;
    let build = std::hash::BuildHasherDefault::<
        crate::vm::realms::thread_realm::ThrowableAddrHasher,
    >::default();
    let shard = throwable_stack_shard(0x7f00_0000_1000);
    let keys: Vec<usize> = (0..1_000_000usize)
        .map(|i| 0x7f00_0000_0000 + i * 8)
        .filter(|&a| throwable_stack_shard(a) == shard)
        .take(4096)
        .collect();
    assert_eq!(keys.len(), 4096);
    let indices: std::collections::HashSet<u64> =
        keys.iter().map(|&k| build.hash_one(k) & 4095).collect();
    assert!(
        indices.len() > 2048,
        "only {} of 4096 bucket indices used",
        indices.len()
    );
    // Fx, for the record: one index per 256.
    let fx = std::hash::BuildHasherDefault::<rustc_hash::FxHasher>::default();
    let fx_indices: std::collections::HashSet<u64> =
        keys.iter().map(|&k| fx.hash_one(k) & 4095).collect();
    assert!(
        fx_indices.len() <= 16,
        "Fx used {} indices",
        fx_indices.len()
    );
}

/// Mixed: moved, stayed, died, in one relocating cycle.
#[test]
fn a_relocating_cycle_sorts_each_entry() {
    let s = shards();
    let (a, b, c) = chained_addresses();
    let far = 0x9_0000usize;
    insert(&s, trace_named(a, "moves"));
    insert(&s, trace_named(b, "stays"));
    insert(&s, trace_named(c, "dies"));
    let map = cratonvm_types::PointerMap::from_iter([(a, far)]);

    remap_and_sweep_throwable_shards(&s, &map, &|addr| addr == b);

    assert_eq!(at(&s, far), Some(("moves".to_string(), far)));
    assert_eq!(at(&s, b), Some(("stays".to_string(), b)));
    assert_eq!(at(&s, c), None);
    assert_eq!(at(&s, a), None);
    assert_eq!(len(&s), 2);
}
