use super::*;

#[test]
fn a_hit_needs_the_same_vm_generation_and_address() {
    let calls = Cell::new(0);
    let ask = |vm: usize, generation: u64, mirror: usize| {
        lookup_or_fill(vm, generation, mirror, || {
            calls.set(calls.get() + 1);
            Some(ClassId::new(7))
        })
    };
    assert_eq!(ask(11, 3, 0x1000), Some(ClassId::new(7)));
    assert_eq!(ask(11, 3, 0x1000), Some(ClassId::new(7)));
    // Another generation, VM or address: a miss each.
    assert!(ask(11, 4, 0x1000).is_some());
    assert!(ask(12, 4, 0x1000).is_some());
    assert!(ask(12, 4, 0x2000).is_some());
    assert_eq!(calls.get(), 4);
}

#[test]
fn a_miss_is_never_remembered_and_identity_zero_is_never_memoised() {
    let calls = Cell::new(0);
    for _ in 0..2 {
        let got = lookup_or_fill(21, 1, 0x3000, || {
            calls.set(calls.get() + 1);
            None
        });
        assert_eq!(got, None);
    }
    for _ in 0..2 {
        let got = lookup_or_fill(0, 1, 0x3000, || {
            calls.set(calls.get() + 1);
            Some(ClassId::new(1))
        });
        assert_eq!(got, Some(ClassId::new(1)));
    }
    assert_eq!(calls.get(), 4);
}

#[test]
fn table_hits_only_on_the_same_generation_and_address() {
    let t = MirrorIdTable::new(true);
    let calls = Cell::new(0);
    let ask = |generation: u64, mirror: usize, id: u32| {
        t.lookup_or_fill(generation, mirror, || {
            calls.set(calls.get() + 1);
            Some(ClassId::new(id))
        })
    };
    assert_eq!(ask(3, 0x1000, 7), Some(ClassId::new(7)));
    assert_eq!(
        ask(3, 0x1000, 99),
        Some(ClassId::new(7)),
        "a hit keeps the filled answer"
    );
    assert_eq!(calls.get(), 1);
    // A new generation: the same address may name another mirror now.
    assert_eq!(ask(4, 0x1000, 9), Some(ClassId::new(9)));
    assert_eq!(t.get(4, 0x1000), Some(ClassId::new(9)));
    assert_eq!(t.get(3, 0x1000), None);
    assert_eq!(t.get(4, 0x2000), None);
    assert_eq!(calls.get(), 2);
}

#[test]
fn table_never_stores_a_miss_and_disabled_never_hits() {
    let t = MirrorIdTable::new(true);
    let calls = Cell::new(0);
    for _ in 0..2 {
        let got = t.lookup_or_fill(1, 0x3000, || {
            calls.set(calls.get() + 1);
            None
        });
        assert_eq!(got, None);
    }
    assert_eq!(calls.get(), 2);
    let off = MirrorIdTable::default();
    assert!(!off.is_enabled());
    for _ in 0..2 {
        assert_eq!(
            off.lookup_or_fill(1, 0x3000, || {
                calls.set(calls.get() + 1);
                Some(ClassId::new(4))
            }),
            Some(ClassId::new(4))
        );
    }
    assert_eq!(calls.get(), 4);
    assert_eq!(off.get(1, 0x3000), None);
}

#[test]
fn table_is_shared_across_threads() {
    let t = std::sync::Arc::new(MirrorIdTable::new(true));
    t.lookup_or_fill(8, 0x5000, || Some(ClassId::new(12)));
    let t2 = std::sync::Arc::clone(&t);
    let seen = std::thread::spawn(move || t2.get(8, 0x5000))
        .join()
        .unwrap();
    assert_eq!(seen, Some(ClassId::new(12)));
}


#[test]
fn a_stale_generation_entry_is_replaced_by_the_new_answer() {
    lookup_or_fill(31, 1, 0x4000, || Some(ClassId::new(5)));
    // The same address now holds another mirror (after a pause).
    assert_eq!(
        lookup_or_fill(31, 2, 0x4000, || Some(ClassId::new(9))),
        Some(ClassId::new(9))
    );
    assert_eq!(
        lookup_or_fill(31, 2, 0x4000, || panic!("must hit")),
        Some(ClassId::new(9))
    );
}
