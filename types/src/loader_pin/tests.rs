use super::*;

/// One process-global map and one process-global [`GENERATION`], so these
/// run one at a time even though their VM ids differ: a generation
/// assertion is about the WHOLE table, and a concurrently-running test that
/// registers its own row ticks it. Mirrors `metadata_pin`'s own test lock.
static TEST_LOCK: parking_lot::Mutex<()> = parking_lot::Mutex::new(());

#[test]
fn teardown_drops_only_the_torn_down_vms_rows() {
    const VM_A: usize = 0xA00;
    const VM_B: usize = 0xB00;
    let _g = TEST_LOCK.lock();
    forget_vm_loader_pins(VM_A);
    forget_vm_loader_pins(VM_B);

    set_loader_pin(VM_A, 1, 0x1000);
    set_loader_pin(VM_B, 2, 0x2000);
    assert_eq!(loader_pin_addr(1), Some(0x1000));
    assert_eq!(loader_pin_addr(2), Some(0x2000));

    // The wipe this replaced ran here, at VM creation, and took both rows.
    forget_vm_loader_pins(VM_A);
    assert_eq!(loader_pin_addr(1), None);
    assert_eq!(
        loader_pin_addr(2),
        Some(0x2000),
        "a concurrently-live VM keeps its pin"
    );

    // One VM's post-GC re-sync must not touch the other's addresses.
    set_loader_pin(VM_A, 3, 0x3000);
    replace_loader_pins(VM_A, &[(3, 0x3100)]);
    assert_eq!(loader_pin_addr(3), Some(0x3100));
    assert_eq!(loader_pin_addr(2), Some(0x2000));

    // Unload of a class id another VM has since claimed leaves it alone.
    set_loader_pin(VM_B, 3, 0x3200);
    remove_loader_pin(VM_A, 3);
    assert_eq!(loader_pin_addr(3), Some(0x3200));

    forget_vm_loader_pins(VM_A);
    forget_vm_loader_pins(VM_B);
}

/// **Every mutation must tick the generation, and a non-mutation must not.**
///
/// A concurrent marker treats "the generation did not move" as a proof that
/// its snapshot of the key set is still exact (see [`GENERATION`]). A
/// mutator that forgets to tick therefore does not make the filter slower,
/// it makes it *wrong*: the snapshot answers "this class has no pinned
/// loader" for a class that has just acquired one, and the loader is swept
/// while live. The other direction only costs a discarded snapshot, but a
/// tick for an identical re-registration would discard one on every
/// post-GC reconciliation, so it is asserted too.
#[test]
fn the_generation_ticks_for_every_real_mutation_and_only_those() {
    const VM_A: usize = 0xA02;
    const VM_B: usize = 0xB02;
    let _g = TEST_LOCK.lock();
    forget_vm_loader_pins(VM_A);
    forget_vm_loader_pins(VM_B);

    let g0 = generation();
    set_loader_pin(VM_A, 4001, 0x4000);
    let g1 = generation();
    assert!(g1 > g0, "a new pin must tick");

    // Idempotent re-registration: same VM, same class, same address.
    set_loader_pin(VM_A, 4001, 0x4000);
    assert_eq!(
        generation(),
        g1,
        "an identical re-registration is no change"
    );

    set_loader_pin(VM_A, 4001, 0x4100);
    let g2 = generation();
    assert!(g2 > g1, "a moved loader address must tick");

    // A removal aimed at a row another VM owns changes nothing.
    set_loader_pin(VM_B, 4002, 0x4200);
    let g3 = generation();
    remove_loader_pin(VM_A, 4002);
    assert_eq!(generation(), g3, "a refused removal is no change");
    remove_loader_pin(VM_B, 4002);
    let g4 = generation();
    assert!(g4 > g3, "a real removal must tick");

    // A teardown that drops nothing is not a change; one that drops a row is.
    forget_vm_loader_pins(VM_B);
    assert_eq!(generation(), g4, "VM B has no rows left");
    forget_vm_loader_pins(VM_A);
    assert!(generation() > g4);
}

/// **Two VMs on one class id keep one row each** (gc-common w5-b,
/// `common-w4b-loader-pin-collision-unroots-another-vms-statics-FIXED-20261010.md`).
///
/// The per-VM lookup a root deferral uses must answer with the asking VM's
/// own loader whatever order the VMs wrote in, and `None` for a VM with no
/// row. The VM-less marker answer is the most recent writer, and falls
/// back to the surviving VM's row when that writer unloads -- the single-
/// row map lost it for good.
#[test]
fn colliding_class_ids_keep_one_row_per_vm() {
    const VM_A: usize = 0xA04;
    const VM_B: usize = 0xB04;
    const VM_C: usize = 0xC04;
    const CID: u32 = 4201;
    let _g = TEST_LOCK.lock();
    forget_vm_loader_pins(VM_A);
    forget_vm_loader_pins(VM_B);

    set_loader_pin(VM_A, CID, 0xa000);
    set_loader_pin(VM_B, CID, 0xb000);
    assert_eq!(loader_pin_addr_for_vm(VM_A, CID), Some(0xa000));
    assert_eq!(loader_pin_addr_for_vm(VM_B, CID), Some(0xb000));
    assert_eq!(
        loader_pin_addr_for_vm(VM_C, CID),
        None,
        "a VM with no user-loader class at this id must root, not defer"
    );
    assert_eq!(loader_pin_addr(CID), Some(0xb000), "most recent writer");
    let mut all = all_pinned_loaders();
    all.retain(|&a| a == 0xa000 || a == 0xb000);
    all.sort_unstable();
    assert_eq!(all, vec![0xa000, 0xb000], "every VM's loader is listed");

    // A's post-GC re-sync moves A's loader and makes A the latest writer;
    // B's row is untouched.
    replace_loader_pins(VM_A, &[(CID, 0xa100)]);
    assert_eq!(loader_pin_addr_for_vm(VM_A, CID), Some(0xa100));
    assert_eq!(loader_pin_addr_for_vm(VM_B, CID), Some(0xb000));
    assert_eq!(loader_pin_addr(CID), Some(0xa100));

    // A unloads the class: B's row becomes the VM-less answer again.
    remove_loader_pin(VM_A, CID);
    assert_eq!(loader_pin_addr_for_vm(VM_A, CID), None);
    assert_eq!(
        loader_pin_addr(CID),
        Some(0xb000),
        "the surviving VM's live loader keeps its marker edge"
    );
    assert_eq!(snapshot().and_then(|s| s.get(&CID).copied()), Some(0xb000));

    forget_vm_loader_pins(VM_B);
    assert_eq!(loader_pin_addr(CID), None);
    assert!(!pinned_class_ids().contains(&CID));
}

/// **A marker that can recognise its own heap follows its own VM's loader**
/// (gc-common w6-a, the marker half of
/// `common-w4b-loader-pin-collision-unroots-another-vms-statics-FIXED-20261010.md`).
///
/// Two VMs' user-loader classes on one id: the VM-less answer names the
/// most recent writer, so the other VM's marker used to be handed a
/// foreign address and its own loader lost the instance->loader edge.
/// `loader_pin_addr_where` / `snapshot_where` pick the row the caller's
/// heap test accepts, whatever the write order, and consult that test
/// only when there is a choice to make.
#[test]
fn a_marker_picks_the_row_inside_its_own_heap() {
    const VM_A: usize = 0xA05;
    const VM_B: usize = 0xB05;
    const CID: u32 = 4301;
    const SOLO: u32 = 4302;
    let _g = TEST_LOCK.lock();
    forget_vm_loader_pins(VM_A);
    forget_vm_loader_pins(VM_B);
    // "Heap A" is [0xa000, 0xb000), "heap B" is [0xb000, 0xc000).
    let in_a = |addr: usize| (0xa000..0xb000).contains(&addr);
    let in_b = |addr: usize| (0xb000..0xc000).contains(&addr);

    set_loader_pin(VM_A, CID, 0xa100);
    set_loader_pin(VM_B, CID, 0xb100);
    assert_eq!(loader_pin_addr(CID), Some(0xb100), "VM-less: last writer");
    assert_eq!(loader_pin_addr_where(CID, in_a), Some(0xa100));
    assert_eq!(loader_pin_addr_where(CID, in_b), Some(0xb100));
    // Write order does not matter.
    replace_loader_pins(VM_A, &[(CID, 0xa200)]);
    assert_eq!(loader_pin_addr(CID), Some(0xa200));
    assert_eq!(loader_pin_addr_where(CID, in_b), Some(0xb100));
    assert_eq!(loader_pin_addr_where(CID, in_a), Some(0xa200));
    // A heap that owns neither row gets the VM-less answer back (its own
    // bounds check rejects it, exactly as before).
    assert_eq!(loader_pin_addr_where(CID, |_| false), Some(0xa200));

    let snap_b = snapshot_where(in_b).expect("rows exist");
    assert_eq!(snap_b.get(&CID).copied(), Some(0xb100));
    let snap_a = snapshot_where(in_a).expect("rows exist");
    assert_eq!(snap_a.get(&CID).copied(), Some(0xa200));

    // One row: returned without asking -- the per-object common case must
    // not pay for a heap test.
    set_loader_pin(VM_A, SOLO, 0xa300);
    let asked = std::cell::Cell::new(0u32);
    let counting = |_: usize| {
        asked.set(asked.get() + 1);
        false
    };
    assert_eq!(loader_pin_addr_where(SOLO, counting), Some(0xa300));
    assert_eq!(asked.get(), 0, "a single row needs no heap test");
    assert_eq!(loader_pin_addr_where(9_999_999, in_a), None);

    forget_vm_loader_pins(VM_A);
    forget_vm_loader_pins(VM_B);
    assert_eq!(loader_pin_addr_where(CID, in_a), None);
}

/// **A per-collection snapshot answers every class id exactly as the
/// per-object lookup does** (gci i10/lp,
/// `gci-i9yl-moving-copy-takes-the-loader-pin-lock-per-copied-object-RETIRED-UNFIXED-20261018`).
///
/// Two VMs share one id (the collision case), each also owns a solo id,
/// and an id nobody registered is asked too. For both heaps' predicates,
/// `loader_pin_addr_where_in(Some(&snapshot_where(p)), id, p)` must equal
/// `loader_pin_addr_where_in(None, id, p)` (the live lookup) for every id.
#[test]
fn a_snapshot_lookup_matches_the_live_lookup_for_every_id() {
    const VM_A: usize = 0xA06;
    const VM_B: usize = 0xB06;
    const SHARED: u32 = 4401;
    const SOLO_A: u32 = 4402;
    const SOLO_B: u32 = 4403;
    const ABSENT: u32 = 4499;
    let _g = TEST_LOCK.lock();
    forget_vm_loader_pins(VM_A);
    forget_vm_loader_pins(VM_B);
    let in_a = |addr: usize| (0xa000..0xb000).contains(&addr);
    let in_b = |addr: usize| (0xb000..0xc000).contains(&addr);

    set_loader_pin(VM_A, SHARED, 0xa100);
    set_loader_pin(VM_B, SHARED, 0xb100);
    set_loader_pin(VM_A, SOLO_A, 0xa200);
    set_loader_pin(VM_B, SOLO_B, 0xb200);

    let snap_a = snapshot_where(in_a).expect("rows exist");
    let snap_b = snapshot_where(in_b).expect("rows exist");
    let mut ids = pinned_class_ids();
    ids.push(ABSENT);
    for id in ids {
        assert_eq!(
            loader_pin_addr_where_in(Some(&snap_a), id, in_a),
            loader_pin_addr_where_in(None, id, in_a),
            "heap A, class id {id}"
        );
        assert_eq!(
            loader_pin_addr_where_in(Some(&snap_b), id, in_b),
            loader_pin_addr_where_in(None, id, in_b),
            "heap B, class id {id}"
        );
    }
    // The collision case picks each heap's own row through the snapshot.
    assert_eq!(loader_pin_addr_where_in(Some(&snap_a), SHARED, in_a), Some(0xa100));
    assert_eq!(loader_pin_addr_where_in(Some(&snap_b), SHARED, in_b), Some(0xb100));
    assert_eq!(loader_pin_addr_where_in(Some(&snap_a), ABSENT, in_a), None);

    // Another VM's write after the snapshot does not change heap A's
    // answer for an id it owns a row for.
    set_loader_pin(VM_B, SOLO_A, 0xb300);
    assert_eq!(
        loader_pin_addr_where_in(Some(&snap_a), SOLO_A, in_a),
        loader_pin_addr_where_in(None, SOLO_A, in_a),
    );

    forget_vm_loader_pins(VM_A);
    forget_vm_loader_pins(VM_B);
    assert_eq!(loader_pin_addr_where_in(None, SHARED, in_a), None);
}

/// The paired reader must hand back a key set and a generation that were
/// taken under one lock acquisition -- the property the marker's proof
/// rests on.
#[test]
fn the_paired_reader_agrees_with_the_unpaired_ones() {
    const VM: usize = 0xA03;
    let _g = TEST_LOCK.lock();
    forget_vm_loader_pins(VM);
    set_loader_pin(VM, 4101, 0x4100);
    let (ids, gen) = pinned_class_ids_with_generation();
    assert_eq!(gen, generation());
    assert!(ids.contains(&4101));
    let mut a = ids;
    let mut b = pinned_class_ids();
    a.sort_unstable();
    b.sort_unstable();
    assert_eq!(a, b);
    forget_vm_loader_pins(VM);
}
