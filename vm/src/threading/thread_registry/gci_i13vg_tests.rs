use super::*;
use crate::threading::jvm_thread::SlotOrigin;

// Granules (`(addr >> 20) & 255`): OLD* in 0x57; YOUNG*, MIRROR in 0x12.
const OLD: usize = 0x0570_0040;
const OLD_TO: usize = 0x0571_0040;
const YOUNG: usize = 0x0120_0040;
const YOUNG_TO: usize = 0x0128_0040;
const MIRROR: usize = 0x0125_0080;
/// The collector's young set: granules 0x10-0x12 and 0x20-0x22.
const YOUNG_SET: [(usize, usize); 2] =
    [(0x0100_0000, 0x0130_0000), (0x0200_0000, 0x0230_0000)];

fn fake(addr: usize) -> ObjectRef {
    // SAFETY: never dereferenced; an address only.
    unsafe { ObjectRef::from_raw(addr as *mut u8) }
}

fn map(pairs: &[(usize, usize)]) -> cratonvm_types::PointerMap {
    let mut pm = cratonvm_types::PointerMap::default();
    for &(k, v) in pairs {
        pm.insert(k, v);
    }
    pm
}

fn addrs(v: &[ObjectRef]) -> Vec<usize> {
    v.iter().map(|r| r.as_ptr() as usize).collect()
}

/// A blocked entry with a (young) mirror, `OLD` in its snapshot and one
/// live slot origin; `gather` / `fold` are the two switches.
fn parked_entry(gather: bool, fold: bool, unmounted: bool) -> (ThreadRegistry, ThreadId) {
    let mut registry = ThreadRegistry::new();
    registry.set_vt_parked_gather_filter_for_test(gather);
    registry.set_vt_parked_fold_filter_for_test(fold);
    let t = ThreadId(1);
    registry.register(t, "vt-parked", Some(fake(MIRROR)));
    if unmounted {
        registry.set_mounted(t, false);
    }
    {
        let threads = registry.threads.read();
        let e = threads.get(&t).unwrap();
        e.root_snapshot.lock().push(fake(OLD));
        e.gc_block_state.slot_origins.lock().push(SlotOrigin {
            frame: 0,
            idx: 0,
            is_stack: false,
            live: true,
            orig: OLD,
            cur: OLD,
        });
        e.gc_block_state
            .in_blocked_region
            .store(true, Ordering::Release);
    }
    (registry, t)
}

fn plant(registry: &ThreadRegistry, t: ThreadId, addr: usize) {
    let threads = registry.threads.read();
    threads.get(&t).unwrap().root_snapshot.lock().push(fake(addr));
}

fn snapshot_of(registry: &ThreadRegistry, t: ThreadId) -> Vec<usize> {
    let threads = registry.threads.read();
    let words = addrs(&threads.get(&t).unwrap().root_snapshot.lock());
    words
}

fn has_filter(registry: &ThreadRegistry, t: ThreadId) -> bool {
    let threads = registry.threads.read();
    let e = threads.get(&t).unwrap();
    e.parked_filter
        .cached(e.mount_epoch.load(Ordering::Acquire))
        .is_some()
}

/// An unmounted, blocked continuation whose filter (left by the last fold,
/// valid for its epoch) is disjoint from the young set goes to the
/// old-only section; its young mirror stays a root. No filter, a retired
/// filter (a mount), or a young word: the whole snapshot stays in `roots`.
#[test]
fn gci_i13vg_split_moves_only_a_provably_old_parked_snapshot() {
    cratonvm_gc::gc_quiescence::clear_peer_stack_slots();
    let (registry, t) = parked_entry(true, false, true);
    let (roots, old) = registry.collect_all_root_snapshots_split(&YOUNG_SET);
    assert!(old.is_empty(), "no filter yet");
    assert_eq!(addrs(&roots), vec![OLD, MIRROR]);
    registry.fold_pointer_map_into_blocked(&map(&[(YOUNG, YOUNG_TO)]));
    if !cratonvm_gc::gc_quiescence::blocked_peer_stack_remap_enabled() {
        // The fold leaves no filter without the native-slot walk.
        assert!(!has_filter(&registry, t));
        return;
    }
    assert!(has_filter(&registry, t), "the gather switch alone keeps the filter");
    let (roots, old) = registry.collect_all_root_snapshots_split(&YOUNG_SET);
    assert_eq!(addrs(&old), vec![OLD], "an old-only parked snapshot");
    assert_eq!(addrs(&roots), vec![MIRROR], "the mirror is a root of every pause");
    // A mount and an unmount retire the filter: back in `roots`.
    registry.set_mounted(t, true);
    registry.set_mounted(t, false);
    let (roots, old) = registry.collect_all_root_snapshots_split(&YOUNG_SET);
    assert!(old.is_empty());
    assert_eq!(addrs(&roots), vec![OLD, MIRROR]);
    // A young word (deposited before the next fold): the fold's new filter
    // names its granule, so the entry stays in `roots`.
    plant(&registry, t, YOUNG);
    registry.fold_pointer_map_into_blocked(&map(&[(YOUNG, YOUNG_TO)]));
    let (roots, old) = registry.collect_all_root_snapshots_split(&YOUNG_SET);
    assert!(old.is_empty(), "a young word keeps the snapshot in the young roots");
    assert_eq!(addrs(&roots), vec![OLD, YOUNG_TO, MIRROR]);
    // A major moves the young word into old space: old-only again.
    registry.fold_pointer_map_into_blocked(&map(&[(YOUNG_TO, OLD_TO)]));
    let (roots, old) = registry.collect_all_root_snapshots_split(&YOUNG_SET);
    assert_eq!(addrs(&old), vec![OLD, OLD_TO]);
    assert_eq!(addrs(&roots), vec![MIRROR]);
}

/// A mounted entry gets no filter and an unblocked one is never split,
/// even with a filter in its cell.
#[test]
fn gci_i13vg_mounted_or_unblocked_entries_are_never_split() {
    cratonvm_gc::gc_quiescence::clear_peer_stack_slots();
    let (registry, t) = parked_entry(true, true, false);
    registry.fold_pointer_map_into_blocked(&map(&[(YOUNG, YOUNG_TO)]));
    let (roots, old) = registry.collect_all_root_snapshots_split(&YOUNG_SET);
    assert!(old.is_empty(), "mounted");
    assert_eq!(addrs(&roots), vec![OLD, MIRROR]);

    let (registry, t2) = parked_entry(true, true, true);
    {
        let threads = registry.threads.read();
        let e = threads.get(&t2).unwrap();
        let mut f = FoldAddrFilter::default();
        f.insert(OLD);
        e.parked_filter
            .store(e.mount_epoch.load(Ordering::Acquire), f);
        e.gc_block_state
            .in_blocked_region
            .store(false, Ordering::Release);
    }
    let (roots, old) = registry.collect_all_root_snapshots_split(&YOUNG_SET);
    assert!(old.is_empty(), "not in its blocked region");
    assert_eq!(addrs(&roots), vec![OLD, MIRROR]);
    let _ = t;
}

/// The gather switch alone keeps the filters fresh but never skips a
/// fold: a word planted behind the epoch's back IS remapped (with the
/// fold switch on, i12/vp's test shows the same fold skipped).
#[test]
fn gci_i13vg_the_gather_switch_never_skips_a_fold() {
    cratonvm_gc::gc_quiescence::clear_peer_stack_slots();
    let (registry, t) = parked_entry(true, false, true);
    let young = map(&[(YOUNG, YOUNG_TO)]);
    registry.fold_pointer_map_into_blocked(&young);
    plant(&registry, t, YOUNG);
    registry.fold_pointer_map_into_blocked(&young);
    assert_eq!(snapshot_of(&registry, t), vec![OLD, YOUNG_TO], "the fold ran");
    // Both switches off: no filter is ever left.
    let (registry, t) = parked_entry(false, false, true);
    registry.fold_pointer_map_into_blocked(&young);
    assert!(!has_filter(&registry, t));
}

/// The door's protocol: an armed request is consumed by exactly the next
/// `collect_all_root_snapshots`, which leaves the old-only section for
/// `take_parked_old_only`; unarmed gathers are the historical one.
#[test]
fn gci_i13vg_an_armed_gather_is_consumed_once() {
    cratonvm_gc::gc_quiescence::clear_peer_stack_slots();
    let (registry, _t) = parked_entry(true, false, true);
    registry.fold_pointer_map_into_blocked(&map(&[(YOUNG, YOUNG_TO)]));
    if !cratonvm_gc::gc_quiescence::blocked_peer_stack_remap_enabled() {
        return;
    }
    registry.arm_parked_split_gather(&YOUNG_SET);
    let roots = registry.collect_all_root_snapshots();
    assert_eq!(addrs(&roots), vec![MIRROR]);
    assert_eq!(addrs(&registry.take_parked_old_only()), vec![OLD]);
    assert!(registry.take_parked_old_only().is_empty(), "taken once");
    // Not armed: everything in one section.
    let roots = registry.collect_all_root_snapshots();
    assert_eq!(addrs(&roots), vec![OLD, MIRROR]);
    assert!(registry.take_parked_old_only().is_empty());
    // Armed then taken without a gather: disarmed.
    registry.arm_parked_split_gather(&YOUNG_SET);
    assert!(registry.take_parked_old_only().is_empty());
    let roots = registry.collect_all_root_snapshots();
    assert_eq!(addrs(&roots), vec![OLD, MIRROR], "the take disarmed it");
}

fn read(rel: &str) -> String {
    if rel == "src/runtime/interpreter/gc_and_alloc.rs" {
        return crate::runtime::interpreter::gc_and_alloc::read_gc_and_alloc_src_from_disk()
            .replace("\r\n", "\n");
    }
    let path = format!("{}/{rel}", env!("CARGO_MANIFEST_DIR"));
    std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("{path}: {e}"))
        .replace("\r\n", "\n")
}

fn at(text: &str, needle: &str) -> usize {
    text.find(needle)
        .unwrap_or_else(|| panic!("`{needle}` not found"))
}

/// The collection door arms before its one registry gather, stages the
/// section after it, and both before the collection.
#[test]
fn gci_i13vg_the_door_arms_before_and_stages_after_its_gather() {
    let src = read("src/runtime/interpreter/gc_and_alloc.rs");
    let start = at(
        &src,
        "\npub(in crate::runtime::interpreter::gc_and_alloc) fn run_collection_pause(",
    );
    let rest = &src[start..];
    let body = &rest[..at(rest, "\n}\n")];
    let arm = at(body, "arm_parked_old_only_gather(shared);");
    let gather = at(body, "thread_registry.collect_all_root_snapshots()");
    let stage = at(body, "stage_parked_old_only_roots(shared);");
    let collect = at(body, ".collect_garbage_with_finalizers(");
    assert!(arm < gather && gather < stage && stage < collect, "{arm} {gather} {stage} {collect}");
    assert_eq!(
        body.matches("thread_registry.collect_all_root_snapshots()").count(),
        1,
        "one gather between the arm and the stage"
    );
    // The arm is a disarm-on-drop guard, and the concurrent cycles' own
    // gathers (initial mark / remark doors) never arm: they always see
    // every root.
    assert!(body.contains("let _parked_split_arm = arm_parked_old_only_gather(shared);"));
    for door in [
        "\npub(in crate::runtime::interpreter) fn maybe_concurrent_gc_at(",
        "\npub(in crate::runtime::interpreter) fn g1_concurrent_mark_cycle(",
    ] {
        let s = at(&src, door);
        let rest = &src[s..];
        let b = &rest[..at(rest, "\n}\n")];
        assert!(b.contains("collect_all_root_snapshots()"), "{door}: gathers its own roots");
        assert!(!b.contains("arm_parked"), "{door}: must not split");
    }
}
