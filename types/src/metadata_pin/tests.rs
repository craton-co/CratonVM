use super::*;

/// g1sz: the key filter never hides a key -- after an add, a wholesale
/// replace, a remap that moves keys, and a forget -- and the lookup it
/// guards still answers the rows.
#[test]
fn g1sz_the_key_filter_never_hides_a_registered_loader() {
    // The registry is process-global and this test's rows (and its
    // remap) would show up in the other tests' latch and row checks.
    let _g = TEST_LOCK.lock();
    // A VM id and addresses no other test of this process uses.
    let vm = 0x6151_0000;
    let (a, b, c) = (0x6151_1000usize, 0x6151_2000usize, 0x6151_3000usize);
    add_metadata_pin(vm, a, 0x6151_a000);
    assert_eq!(roots_for_loader(a), Some(vec![0x6151_a000]));
    assert!(key_filter_may_contain(a));
    replace_metadata_pins(vm, &[(a, 0x6151_a008), (b, 0x6151_b000)]);
    assert_eq!(roots_for_loader(b), Some(vec![0x6151_b000]));
    assert!(key_filter_may_contain(a) && key_filter_may_contain(b));
    remap_metadata_pins(|x| Some(if x == b { c } else { x }));
    assert!(key_filter_may_contain(c), "a moved key is in the filter");
    assert_eq!(roots_for_loader(c), Some(vec![0x6151_b000]));
    assert_eq!(roots_for_loader(b), None);
    forget_vm_metadata_pins(vm);
    assert_eq!(roots_for_loader(a), None);
    assert_eq!(roots_for_loader(c), None);
}

/// gcd d10/r: the additive publish keeps a VM's rows, and the remap
/// rewrites keys and values (merging keys that land on one address,
/// dropping what died).
#[test]
fn d10r_add_keeps_rows_and_remap_follows_the_forwarding_map() {
    const A: usize = 0xA20;
    const B: usize = 0xB20;
    let _g = TEST_LOCK.lock();
    forget_vm_metadata_pins(A);
    forget_vm_metadata_pins(B);
    replace_metadata_pins(A, &[(0x100, 0x200)]);
    add_metadata_pins(A, &[(0x100, 0x201), (0x300, 0x400)]);
    add_metadata_pin(B, 0x500, 0x200);
    assert_eq!(roots_for_loader(0x100), Some(vec![0x200, 0x201]));
    let mut addrs = vm_row_addresses(A);
    addrs.sort_unstable();
    assert_eq!(addrs, vec![0x100, 0x200, 0x201, 0x300, 0x400]);

    let gen = generation();
    remap_metadata_pins(Some);
    assert_eq!(generation(), gen, "nothing moved: no tick");

    // 0x100 -> 0x300 merges with the existing 0x300 row; 0x200 -> 0x210
    // (B's 0x200 too: a forwarding map is per heap, not per VM); 0x400
    // died; B's key 0x500 is untouched.
    remap_metadata_pins(|a| match a {
        0x100 => Some(0x300),
        0x200 => Some(0x210),
        0x400 => None,
        other => Some(other),
    });
    assert!(generation() > gen);
    assert_eq!(roots_for_loader(0x100), None);
    let mut row = roots_for_loader(0x300).expect("merged row");
    row.sort_unstable();
    assert_eq!(row, vec![0x201, 0x210]);
    assert_eq!(roots_for_loader(0x500), Some(vec![0x210]));
    // A dead key drops its row.
    remap_metadata_pins(|a| if a == 0x500 { None } else { Some(a) });
    assert_eq!(roots_for_loader(0x500), None);
    forget_vm_metadata_pins(A);
    forget_vm_metadata_pins(B);
}

/// **The `NON_EMPTY` latch must never say `false` while a row exists.**
///
/// A latch on a per-object fast path is a correctness device, not an
/// optimisation: a stale `false` makes [`roots_for_loader`] return `None`
/// for a loader that DOES own metadata roots, and the marker then sweeps a
/// static reference field's target while its defining loader is live. Every
/// mutator has to maintain it, and the one that is easy to get wrong is
/// `replace_metadata_pins` -- its `retain` drops one VM's rows while another
/// VM's survive, so "I just cleared mine" is not "the registry is empty".
#[test]
fn the_non_empty_latch_tracks_the_registry_through_every_mutator() {
    const A: usize = 0xA10;
    const B: usize = 0xB10;
    let _g = TEST_LOCK.lock();
    forget_vm_metadata_pins(A);
    forget_vm_metadata_pins(B);
    assert!(!NON_EMPTY.load(Ordering::Relaxed), "both VMs cleared");

    add_metadata_pin(A, 0x1000, 0x2000);
    assert!(NON_EMPTY.load(Ordering::Relaxed));
    assert!(
        roots_for_loader(0x1000).is_some(),
        "and the read path agrees"
    );

    // TWO VMs. Clearing A must NOT lower the latch, because B still has a
    // row -- and B's row is exactly what a stale `false` would hide.
    add_metadata_pin(B, 0x3000, 0x4000);
    replace_metadata_pins(A, &[]);
    assert!(
        NON_EMPTY.load(Ordering::Relaxed),
        "VM B still owns a row; a lowered latch here drops its roots"
    );
    assert!(roots_for_loader(0x3000).is_some());

    forget_vm_metadata_pins(B);
    assert!(!NON_EMPTY.load(Ordering::Relaxed), "now it is really empty");
    assert!(roots_for_loader(0x3000).is_none());
}

/// gci i18/nr (gci i17/rv finding 9): `vm_snapshot` hands out exactly one
/// VM's rows, and `None` for a VM with none, while another VM's rows
/// stay in the whole-process `snapshot`.
#[test]
fn gci_i18_nr_vm_snapshot_holds_only_that_vms_rows() {
    const A: usize = 0xA30;
    const B: usize = 0xB30;
    const C: usize = 0xC30;
    let _g = TEST_LOCK.lock();
    forget_vm_metadata_pins(A);
    forget_vm_metadata_pins(B);
    forget_vm_metadata_pins(C);
    add_metadata_pins(A, &[(0x7100, 0x7108), (0x7100, 0x7110), (0x7200, 0x7208)]);
    add_metadata_pin(B, 0x7300, 0x7308);

    let a = vm_snapshot(A).expect("A owns rows");
    assert_eq!(a.len(), 2);
    assert_eq!(a.get(&0x7100), Some(&vec![0x7108, 0x7110]));
    assert_eq!(a.get(&0x7200), Some(&vec![0x7208]));
    assert!(!a.contains_key(&0x7300), "another VM's row is not handed out");
    let b = vm_snapshot(B).expect("B owns a row");
    assert_eq!(b.len(), 1);
    assert_eq!(b.get(&0x7300), Some(&vec![0x7308]));
    assert!(vm_snapshot(C).is_none(), "a VM with no row");
    let all = snapshot().expect("non-empty");
    assert!(all.contains_key(&0x7100) && all.contains_key(&0x7300));

    forget_vm_metadata_pins(A);
    assert!(vm_snapshot(A).is_none());
    assert!(vm_snapshot(B).is_some(), "B's rows survive A's teardown");
    forget_vm_metadata_pins(B);
}

/// The store and [`GENERATION`] are process-wide, so these run one at a
/// time even though their VM ids differ.
static TEST_LOCK: parking_lot::Mutex<()> = parking_lot::Mutex::new(());

#[test]
fn loader_owned_roots_are_replaced_and_deduplicated() {
    const VM: usize = 0xA00;
    let _g = TEST_LOCK.lock();
    forget_vm_metadata_pins(VM);
    set_metadata_weak_mode(VM, true);
    replace_metadata_pins(VM, &[(10, 100), (10, 100), (10, 101)]);
    assert_eq!(roots_for_loader(10), Some(vec![100, 101]));
    add_metadata_pin(VM, 10, 102);
    assert_eq!(roots_for_loader(10), Some(vec![100, 101, 102]));
    replace_metadata_pins(VM, &[(20, 200)]);
    assert!(roots_for_loader(10).is_none());
    forget_vm_metadata_pins(VM);
}

#[test]
fn one_vms_reset_leaves_a_concurrently_live_vms_roots_alone() {
    const VM_A: usize = 0xA01;
    const VM_B: usize = 0xB01;
    let _g = TEST_LOCK.lock();
    forget_vm_metadata_pins(VM_A);
    forget_vm_metadata_pins(VM_B);
    add_metadata_pin(VM_A, 0x1000, 0x1008);
    add_metadata_pin(VM_B, 0x2000, 0x2008);

    // Both of the wipes this replaced — VM creation, and weak mode going
    // off — took the whole table.
    replace_metadata_pins(VM_A, &[]);
    set_metadata_weak_mode(VM_A, false);
    assert_eq!(roots_for_loader(0x1000), None);
    assert_eq!(roots_for_loader(0x2000), Some(vec![0x2008]));

    forget_vm_metadata_pins(VM_B);
}

/// **The generation is the concurrent marker's only lock-free way to learn
/// that its snapshot stopped being a proof.**
///
/// The extra-root filter snapshots this table's owner key set and then
/// answers "that object owns no loader metadata" from a bloom, per marked
/// object, without a lock. A mutator that changes the table without ticking
/// leaves that `false` looking valid while it is wrong -- a static
/// reference field's target swept with its defining loader live, which is
/// the same failure `the_non_empty_latch_tracks_the_registry_through_every_mutator`
/// guards from the other side.
#[test]
fn the_generation_ticks_for_every_real_mutation() {
    const VM_A: usize = 0xA02;
    const VM_B: usize = 0xB02;
    let _g = TEST_LOCK.lock();
    forget_vm_metadata_pins(VM_A);
    forget_vm_metadata_pins(VM_B);

    let g0 = generation();
    add_metadata_pin(VM_A, 0x6000, 0x6008);
    let g1 = generation();
    assert!(g1 > g0, "a new owner must tick");

    add_metadata_pin(VM_A, 0x6000, 0x6010);
    let g2 = generation();
    assert!(g2 > g1, "a new root under an existing owner must tick");

    // The de-duplicating arm records nothing, so it changes nothing.
    add_metadata_pin(VM_A, 0x6000, 0x6010);
    assert_eq!(generation(), g2, "a duplicate is not a change");

    replace_metadata_pins(VM_A, &[(0x6100, 0x6108)]);
    let g3 = generation();
    assert!(g3 > g2, "a wholesale replacement must tick");

    // Weak mode gates whether these roots are conditional at all.
    set_metadata_weak_mode(VM_A, true);
    let g4 = generation();
    assert!(g4 > g3, "a weak-mode change must tick");

    forget_vm_metadata_pins(VM_B);
    assert_eq!(generation(), g4, "VM B never had a row");
    forget_vm_metadata_pins(VM_A);
    assert!(generation() > g4, "a real teardown must tick");
    // Turning weak mode off drops VM A's rows and still ticks.
    add_metadata_pin(VM_A, 0x6300, 0x6308);
    let g5 = generation();
    set_metadata_weak_mode(VM_A, false);
    assert!(generation() > g5, "weak mode going off must tick");
    assert!(roots_for_loader(0x6300).is_none(), "and drop the VM's rows");
}

/// gc-common w8-e: a row past [`LINEAR_DEDUP_MAX`] switches to its hash
/// index. It must still record each object once, keep insertion order, and
/// tick the generation only for a new object, on both write paths.
#[test]
fn a_large_row_deduplicates_through_its_index_in_insertion_order() {
    const VM: usize = 0xA04;
    const LOADER: usize = 0x9000;
    let _g = TEST_LOCK.lock();
    forget_vm_metadata_pins(VM);

    let n = LINEAR_DEDUP_MAX * 4 + 3;
    let objects: Vec<usize> = (0..n).map(|i| 0x10_0000 + i * 16).collect();
    for &o in &objects {
        add_metadata_pin(VM, LOADER, o);
    }
    let g = generation();
    // Every object again, both before and after the switch point.
    for &o in objects.iter().rev() {
        add_metadata_pin(VM, LOADER, o);
    }
    assert_eq!(generation(), g, "re-adding known objects is not a change");
    assert_eq!(roots_for_loader(LOADER), Some(objects.clone()));

    add_metadata_pin(VM, LOADER, 0x20_0000);
    assert!(generation() > g, "a new object under a big row must tick");
    let mut expected = objects.clone();
    expected.push(0x20_0000);
    assert_eq!(roots_for_loader(LOADER), Some(expected));

    // The wholesale path, with every object listed twice.
    let entries: Vec<(usize, usize)> = objects
        .iter()
        .chain(objects.iter())
        .map(|&o| (LOADER, o))
        .collect();
    replace_metadata_pins(VM, &entries);
    assert_eq!(roots_for_loader(LOADER), Some(objects.clone()));
    let snap = snapshot().expect("a row exists");
    assert_eq!(snap.get(&LOADER), Some(&objects));

    forget_vm_metadata_pins(VM);
    assert!(roots_for_loader(LOADER).is_none());
}

/// The key-set reader must agree with [`snapshot`]'s keys and carry a
/// generation taken under the same lock acquisition.
#[test]
fn the_paired_key_set_reader_agrees_with_the_values_snapshot() {
    const VM: usize = 0xA03;
    let _g = TEST_LOCK.lock();
    forget_vm_metadata_pins(VM);
    add_metadata_pin(VM, 0x6200, 0x6208);
    let (owners, gen) = pinned_owner_addrs_with_generation();
    assert_eq!(gen, generation());
    let values = snapshot().expect("just registered a row");
    let mut a = owners;
    let mut b: Vec<usize> = values.keys().copied().collect();
    a.sort_unstable();
    b.sort_unstable();
    assert_eq!(a, b);
    forget_vm_metadata_pins(VM);
}
