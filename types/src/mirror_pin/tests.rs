use super::*;

/// One process-global map and one process-global [`GENERATION`]: a
/// generation assertion is about the WHOLE table, so a concurrently-running
/// test that registers its own row would tick it. Mirrors `metadata_pin`'s
/// own test lock.
static TEST_LOCK: parking_lot::Mutex<()> = parking_lot::Mutex::new(());

#[test]
fn teardown_drops_only_the_torn_down_vms_rows() {
    const VM_A: usize = 0xA00;
    const VM_B: usize = 0xB00;
    let _g = TEST_LOCK.lock();
    forget_vm_mirror_pins(VM_A);
    forget_vm_mirror_pins(VM_B);

    add_mirror_pin(VM_A, 0x1000, 0x1008);
    add_mirror_pin(VM_B, 0x2000, 0x2008);

    forget_vm_mirror_pins(VM_A);
    assert_eq!(mirrors_for_loader(0x1000), None);
    assert_eq!(
        mirrors_for_loader(0x2000),
        Some(vec![0x2008]),
        "a concurrently-live VM keeps its mirrors"
    );

    // One VM's post-GC re-sync must leave the other's rows in place.
    replace_mirror_pins(VM_A, &[(0x1100, 0x1108)]);
    assert_eq!(mirrors_for_loader(0x1100), Some(vec![0x1108]));
    assert_eq!(mirrors_for_loader(0x2000), Some(vec![0x2008]));

    forget_vm_mirror_pins(VM_A);
    forget_vm_mirror_pins(VM_B);
}

/// **A registration a concurrent marker cannot see must be a registration
/// it can DETECT.**
///
/// The extra-root filter snapshots the owner key set and then answers "this
/// object owns no mirrors" from it without a lock. A mutator that adds a
/// row without ticking [`GENERATION`] leaves that answer looking valid
/// while it is false, and the mirror is swept with its class still live.
/// Adding a mirror to an owner that is ALREADY in the table ticks it too:
/// the filter's key set is unchanged there, but a consumer that cached the
/// values is stale, and the cheap conservative rule is one counter for the
/// contents.
#[test]
fn the_generation_ticks_for_every_real_mutation_and_only_those() {
    const VM_A: usize = 0xA02;
    const VM_B: usize = 0xB02;
    let _g = TEST_LOCK.lock();
    forget_vm_mirror_pins(VM_A);
    forget_vm_mirror_pins(VM_B);

    let g0 = generation();
    add_mirror_pin(VM_A, 0x5000, 0x5008);
    let g1 = generation();
    assert!(g1 > g0, "a new owner must tick");

    add_mirror_pin(VM_A, 0x5000, 0x5010);
    let g2 = generation();
    assert!(
        g2 > g1,
        "a new mirror under an existing owner must tick too"
    );

    replace_mirror_pins(VM_A, &[(0x5100, 0x5108)]);
    let g3 = generation();
    assert!(g3 > g2, "a post-GC wholesale replacement must tick");

    // A teardown that drops nothing is not a change.
    forget_vm_mirror_pins(VM_B);
    assert_eq!(generation(), g3, "VM B never had a row");
    forget_vm_mirror_pins(VM_A);
    assert!(generation() > g3, "a real teardown must tick");
}

/// gc-common w18-d: a non-strong hidden class's mirror is never pinned to
/// its loader -- not by the row the mirror's creation already added, not
/// by a later `add_mirror_pin`, and not by the post-GC rebuild -- while
/// the loader's OTHER mirrors and another VM's rows are untouched. The
/// exclusion follows the re-published addresses and dies with the VM.
#[test]
fn an_excluded_mirror_is_never_pinned_to_its_loader() {
    const VM_A: usize = 0xA04;
    const VM_B: usize = 0xB04;
    let _g = TEST_LOCK.lock();
    forget_vm_mirror_pins(VM_A);
    forget_vm_mirror_pins(VM_B);

    const LOADER: usize = 0x6000;
    const HIDDEN: usize = 0x6008;
    const PLAIN: usize = 0x6010;
    add_mirror_pin(VM_A, LOADER, HIDDEN);
    add_mirror_pin(VM_A, LOADER, PLAIN);
    // Another VM's row naming the same address must survive A's exclusion.
    add_mirror_pin(VM_B, 0x7000, HIDDEN);

    let g0 = generation();
    exclude_mirror(VM_A, HIDDEN, Some(LOADER));
    assert!(
        generation() > g0,
        "dropping a mirror from a row is a content change"
    );
    assert_eq!(mirrors_for_loader(LOADER), Some(vec![PLAIN]));
    assert_eq!(
        mirrors_for_loader(0x7000),
        Some(vec![HIDDEN]),
        "VM B untouched"
    );

    add_mirror_pin(VM_A, LOADER, HIDDEN);
    assert_eq!(
        mirrors_for_loader(LOADER),
        Some(vec![PLAIN]),
        "a later add is refused"
    );

    replace_mirror_pins(VM_A, &[(LOADER, HIDDEN), (LOADER, PLAIN)]);
    assert_eq!(
        mirrors_for_loader(LOADER),
        Some(vec![PLAIN]),
        "the post-GC rebuild skips it too"
    );

    // A move: the owner re-publishes the new address.
    const MOVED: usize = 0x6108;
    replace_excluded_mirrors(VM_A, &[MOVED]);
    replace_mirror_pins(VM_A, &[(LOADER, MOVED), (LOADER, HIDDEN), (LOADER, PLAIN)]);
    let mut got = mirrors_for_loader(LOADER).unwrap_or_default();
    got.sort_unstable();
    assert_eq!(
        got,
        vec![HIDDEN, PLAIN],
        "the new address is excluded and the old one no longer is"
    );

    forget_vm_mirror_pins(VM_A);
    add_mirror_pin(VM_A, LOADER, MOVED);
    assert_eq!(
        mirrors_for_loader(LOADER),
        Some(vec![MOVED]),
        "teardown drops the VM's exclusions with its rows"
    );
    forget_vm_mirror_pins(VM_A);
    forget_vm_mirror_pins(VM_B);
}

/// The paired reader must hand back a key set and a generation taken under
/// one lock acquisition -- the property the marker's proof rests on.
#[test]
fn the_paired_reader_agrees_with_the_unpaired_one() {
    const VM: usize = 0xA03;
    let _g = TEST_LOCK.lock();
    forget_vm_mirror_pins(VM);
    add_mirror_pin(VM, 0x5200, 0x5208);
    let (owners, gen) = pinned_owner_addrs_with_generation();
    assert_eq!(gen, generation());
    assert!(owners.contains(&0x5200));
    let mut a = owners;
    let mut b = pinned_owner_addrs();
    a.sort_unstable();
    b.sort_unstable();
    assert_eq!(a, b);
    forget_vm_mirror_pins(VM);
}
