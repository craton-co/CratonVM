use super::*;

fn obj(addr: usize) -> ObjectRef {
    // SAFETY: a key only; the registry and the mock hash the address and
    // never dereference it.
    unsafe { ObjectRef::from_raw(addr as *mut u8) }
}

fn ctx_for(vm: usize) -> crate::test_utils::MockNativeContext {
    let c = crate::test_utils::mock_ctx();
    c.set_vm_identity(vm);
    c
}

fn slots_of(vm: usize) -> usize {
    lock_key_shards::lock_key_shards_in_use()
        .iter()
        .map(|shard| {
            shard
                .lock()
                .buckets
                .values()
                .map(|b| b.iter().filter(|s| s.vm == vm).count())
                .sum::<usize>()
        })
        .sum()
}

fn tombstones_of(vm: usize) -> usize {
    lock_key_shards::lock_key_shards_in_use()
        .iter()
        .map(|shard| {
            shard
                .lock()
                .tombstones
                .values()
                .filter(|owner| **owner == vm)
                .count()
        })
        .sum()
}

/// Two freed spans, as a slice hands them over: sorted, non-overlapping.
const SPANS: [(usize, usize); 2] = [(0x1_C261_0000, 0x100), (0x1_C261_4000, 0x40)];
/// The first byte and the last word of span 1, and the last word of span 2.
const INSIDE: [usize; 3] = [0x1_C261_0000, 0x1_C261_00F8, 0x1_C261_4038];
/// Just below span 1, one past its end, the gap, one past span 2's end.
const OUTSIDE: [usize; 4] = [0x1_C260_FFF8, 0x1_C261_0100, 0x1_C261_2000, 0x1_C261_4040];
/// A held (`gc_stable_lock_key`) slot inside span 1.
const HELD_INSIDE: usize = 0x1_C261_0040;

/// Mint VM `vm`'s weak slots at [`INSIDE`] and [`OUTSIDE`] and a held one
/// at [`HELD_INSIDE`]; answer `(key, address)` for each.
fn mint_layout(vm: usize) -> Vec<(usize, usize)> {
    let mut c = ctx_for(vm);
    let mut keys: Vec<(usize, usize)> = INSIDE
        .iter()
        .chain(OUTSIDE.iter())
        .map(|&a| (gc_stable_weak_lock_key(&c, obj(a)).unwrap(), a))
        .collect();
    keys.push((
        gc_stable_lock_key(&mut c, obj(HELD_INSIDE)).unwrap(),
        HELD_INSIDE,
    ));
    keys
}

#[test]
fn a_span_drop_frees_exactly_the_slots_inside_the_spans_of_its_vm() {
    const VM: usize = 0xC260_0001;
    const OTHER_VM: usize = 0xC260_0002;
    let keys = mint_layout(VM);
    let key_at = |addr: usize| keys.iter().find(|&&(_, a)| a == addr).unwrap().0;
    // Another VM's slot at an address inside a span: never judged.
    let foreign = gc_stable_weak_lock_key(&ctx_for(OTHER_VM), obj(INSIDE[0])).unwrap();
    for &(k, _) in &keys {
        crate::stamped_lock::stamped_init(k);
    }
    crate::stamped_lock::stamped_init(foreign);
    assert_eq!(
        (slots_of(VM), tombstones_of(VM)),
        (INSIDE.len() + OUTSIDE.len() + 1, 0)
    );

    assert_eq!(gc_drop_lock_keys_in_spans(VM, &SPANS), INSIDE.len());
    for &a in &INSIDE {
        let k = key_at(a);
        assert_eq!(
            lock_key_owner_vm(k as u64),
            None,
            "slot at {a:#x} survived its freed span"
        );
        assert!(
            !crate::stamped_lock::stamped_has_slot(k),
            "the freed key at {a:#x} was not fanned out"
        );
    }
    for &a in &OUTSIDE {
        let k = key_at(a);
        assert_eq!(
            lock_key_owner_vm(k as u64),
            Some(VM),
            "slot at {a:#x} dropped"
        );
        assert!(
            crate::stamped_lock::stamped_has_slot(k),
            "state at {a:#x} dropped"
        );
    }
    let held = key_at(HELD_INSIDE);
    assert_eq!(
        lock_key_owner_vm(held as u64),
        Some(VM),
        "held: tombstoned, not dropped"
    );
    assert_eq!((slots_of(VM), tombstones_of(VM)), (OUTSIDE.len(), 1));
    assert_eq!(
        lock_key_owner_vm(foreign as u64),
        Some(OTHER_VM),
        "another VM's slot was judged"
    );
    assert!(crate::stamped_lock::stamped_has_slot(foreign));
    // An object allocated onto a freed span later is a new object.
    let reborn = gc_stable_weak_lock_key(&ctx_for(VM), obj(INSIDE[0])).unwrap();
    assert!(keys.iter().all(|&(k, _)| k != reborn));

    forget_vm_lock_keys(VM);
    forget_vm_lock_keys(OTHER_VM);
    assert_eq!((slots_of(VM), tombstones_of(VM)), (0, 0));
    assert!(!crate::stamped_lock::stamped_has_slot(foreign));
}

/// The span pass decides exactly what the predicate path (the full sweep
/// with a "not in a span" verdict, which is what this call site used before
/// w26-c) decides on the same layout: the same slots freed, the same held
/// slot tombstoned, the same slots kept. Both feed one fan-out list in
/// `sweep_lock_keys`, so the same freed keys fan out.
#[test]
fn a_span_drop_frees_the_same_slots_the_full_sweep_would() {
    const SPAN_VM: usize = 0xC260_0011;
    const FULL_VM: usize = 0xC260_0012;
    let span_keys = mint_layout(SPAN_VM);
    let full_keys = mint_layout(FULL_VM);
    let freed_addrs = |keys: &[(usize, usize)], freed: &[usize]| -> Vec<usize> {
        let mut v: Vec<usize> = freed
            .iter()
            .map(|k| {
                keys.iter()
                    .find(|(kk, _)| kk == k)
                    .expect("a key this test minted")
                    .1
            })
            .collect();
        v.sort_unstable();
        v
    };

    let (span_dropped, span_freed) = drop_lock_keys_in_spans_locked(SPAN_VM, &SPANS);
    let (full_dropped, full_freed) =
        judge_lock_keys_by_predicate(FULL_VM, &|a| !lock_key_in_spans(a, &SPANS), None);

    assert_eq!(span_dropped, full_dropped);
    assert_eq!(span_dropped, INSIDE.len());
    let mut expected = INSIDE.to_vec();
    expected.sort_unstable();
    assert_eq!(freed_addrs(&span_keys, &span_freed), expected);
    assert_eq!(freed_addrs(&full_keys, &full_freed), expected);
    assert_eq!(
        (slots_of(SPAN_VM), tombstones_of(SPAN_VM)),
        (slots_of(FULL_VM), tombstones_of(FULL_VM))
    );
    assert_eq!(tombstones_of(SPAN_VM), 1);
    forget_vm_lock_keys(SPAN_VM);
    forget_vm_lock_keys(FULL_VM);
}

#[test]
fn an_empty_span_list_touches_nothing() {
    const VM: usize = 0xC260_0021;
    let mut c = ctx_for(VM);
    let weak = gc_stable_weak_lock_key(&c, obj(INSIDE[0])).unwrap();
    let held = gc_stable_lock_key(&mut c, obj(HELD_INSIDE)).unwrap();
    crate::stamped_lock::stamped_init(weak);

    assert_eq!(gc_drop_lock_keys_in_spans(VM, &[]), 0);
    assert_eq!((slots_of(VM), tombstones_of(VM)), (2, 0));
    assert_eq!(lock_key_owner_vm(weak as u64), Some(VM));
    assert_eq!(lock_key_owner_vm(held as u64), Some(VM));
    assert!(crate::stamped_lock::stamped_has_slot(weak));
    // Spans that hold none of its slots: nothing either.
    assert_eq!(
        gc_drop_lock_keys_in_spans(VM, &[(0x1_C26F_0000, 0x1000)]),
        0
    );
    assert_eq!((slots_of(VM), tombstones_of(VM)), (2, 0));
    assert!(crate::stamped_lock::stamped_has_slot(weak));
    forget_vm_lock_keys(VM);
}

/// Every shard's address index names exactly its buckets' slots.
fn assert_index_is_exact() {
    for shard in lock_key_shards::lock_key_shards_in_use() {
        let reg = shard.lock();
        let slots: std::collections::BTreeSet<(usize, usize, u32)> = reg
            .buckets
            .iter()
            .flat_map(|(&h, b)| b.iter().map(move |s| (s.last_ptr, s.vm, h)))
            .collect();
        assert_eq!(slots, reg.by_addr, "address index drifted from the slots");
    }
}

/// common-w26c item 1: mint, a moving sweep (re-address plus a drop), a
/// span drop at the NEW addresses and a teardown each leave the index
/// exact, and the span drop finds a slot only where it now lives.
#[test]
fn the_address_index_follows_every_registry_change() {
    const VM: usize = 0xC260_0031;
    let base = 0x1_C262_0000usize;
    let at = |i: usize| base + i * 0x40;
    let mut c = ctx_for(VM);
    for i in 0..64 {
        if i % 4 == 0 {
            gc_stable_lock_key(&mut c, obj(at(i))).unwrap();
        } else {
            gc_stable_weak_lock_key(&c, obj(at(i))).unwrap();
        }
    }
    assert_index_is_exact();
    // Slots 0..32 move up 0x10_0000; slot 63 dies.
    let mut moved = cratonvm_types::PointerMap::default();
    for i in 0..32 {
        moved.insert(at(i), at(i) + 0x10_0000);
    }
    let dead = at(63);
    gc_sweep_and_remap_lock_keys(VM, &|a| a != dead, &moved);
    assert_index_is_exact();
    // The old span holds only the 32 unmoved survivors minus the dead one.
    assert_eq!(
        gc_drop_lock_keys_in_spans(VM, &[(at(0), 32 * 0x40)]),
        0,
        "nothing is left at the vacated addresses"
    );
    // 8 of the moved slots are held (i % 4 == 0): tombstoned, 24 dropped.
    assert_eq!(
        gc_drop_lock_keys_in_spans(VM, &[(at(0) + 0x10_0000, 32 * 0x40)]),
        24
    );
    assert_index_is_exact();
    forget_vm_lock_keys(VM);
    assert_index_is_exact();
    assert_eq!((slots_of(VM), tombstones_of(VM)), (0, 0));
}

/// `lock_key_in_spans` answers what `addr_keyed::in_spans` answers (its
/// `in_spans_boundaries` cases), plus the bounds pre-check's edges.
#[test]
fn the_span_test_matches_addr_keyed_in_spans() {
    let spans = [(0x1000usize, 0x40usize), (0x2000, 0x10)];
    for (addr, want) in [
        (0x1000usize, true),
        (0x103F, true),
        (0x1040, false),
        (0x0FFF, false),
        (0x1800, false),
        (0x2000, true),
        (0x200F, true),
        (0x2010, false),
        (0, false),
        (usize::MAX, false),
    ] {
        assert_eq!(lock_key_in_spans(addr, &spans), want, "{addr:#x}");
    }
    assert!(!lock_key_in_spans(0x1000, &[]));
    // A span that reaches the top of the address space does not overflow.
    assert!(lock_key_in_spans(
        usize::MAX - 8,
        &[(usize::MAX - 0xF, 0x10)]
    ));
}
