use super::*;
use crate::threading::jvm_thread::SlotOrigin;

// Granule buckets (`(addr >> 20) & 255`): OLD* in 0x57, YOUNG* in 0x12.
const OLD: usize = 0x0570_0040;
const OLD_TO: usize = 0x0571_0040;
const YOUNG: usize = 0x0120_0040;
const YOUNG_TO: usize = 0x0128_0040;

fn fake(addr: usize) -> ObjectRef {
    // SAFETY: never dereferenced; a pointer-map key / value only.
    unsafe { ObjectRef::from_raw(addr as *mut u8) }
}

fn map(pairs: &[(usize, usize)]) -> cratonvm_types::PointerMap {
    let mut pm = cratonvm_types::PointerMap::default();
    for &(k, v) in pairs {
        pm.insert(k, v);
    }
    pm
}

/// A blocked entry holding `OLD` in its snapshot and one live slot origin.
fn blocked_entry(filter_on: bool, unmounted: bool) -> (ThreadRegistry, ThreadId) {
    let mut registry = ThreadRegistry::new();
    registry.set_vt_parked_fold_filter_for_test(filter_on);
    let t = ThreadId(1);
    registry.register(t, "vt-parked", None);
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

fn snapshot_of(registry: &ThreadRegistry, t: ThreadId) -> Vec<usize> {
    let threads = registry.threads.read();
    let e = threads.get(&t).unwrap();
    let words = e.root_snapshot.lock().iter().map(|r| r.as_ptr() as usize).collect();
    words
}

fn origin_cur(registry: &ThreadRegistry, t: ThreadId) -> Vec<usize> {
    let threads = registry.threads.read();
    let e = threads.get(&t).unwrap();
    let curs = e.gc_block_state.slot_origins.lock().iter().map(|so| so.cur).collect();
    curs
}

fn folds(registry: &ThreadRegistry, t: ThreadId) -> u64 {
    let threads = registry.threads.read();
    let e = threads.get(&t).unwrap();
    e.gc_block_state.blocked_folds.load(Ordering::Acquire)
}

fn cached(registry: &ThreadRegistry, t: ThreadId) -> Option<FoldAddrFilter> {
    let threads = registry.threads.read();
    let e = threads.get(&t).unwrap();
    e.parked_filter.cached(e.mount_epoch.load(Ordering::Acquire))
}

fn epoch(registry: &ThreadRegistry, t: ThreadId) -> u64 {
    let threads = registry.threads.read();
    threads.get(&t).unwrap().mount_epoch.load(Ordering::Acquire)
}

fn plant(registry: &ThreadRegistry, t: ThreadId, addr: usize) {
    let threads = registry.threads.read();
    threads.get(&t).unwrap().root_snapshot.lock().push(fake(addr));
}

/// The filter is a sound disjointness proof: a shared address always
/// shares a bit, a wide range saturates it, and the pause's filter covers
/// the vacated arena as well as the map's keys.
#[test]
fn gci_i12vp_fold_addr_filter_proves_disjointness() {
    let mut a = FoldAddrFilter::default();
    a.insert(OLD);
    let mut b = FoldAddrFilter::default();
    b.insert(YOUNG);
    assert!(!a.intersects(&b), "different granules: disjoint");
    b.insert(OLD + 8);
    assert!(a.intersects(&b), "one granule in common");
    // Modulo 256 MiB: the same bucket, so "may intersect" (never a false
    // "disjoint").
    let mut c = FoldAddrFilter::default();
    c.insert(OLD + (256 << 20));
    assert!(a.intersects(&c));
    let mut wide = FoldAddrFilter::default();
    wide.insert_range(0x1000_0000, 0x1000_0000 + (300 << 20));
    assert!(wide.is_full());
    let mut empty_range = FoldAddrFilter::default();
    empty_range.insert_range(0x2000, 0x2000);
    assert_eq!(empty_range, FoldAddrFilter::default());
    // Map keys, not values; and the vacated arena.
    let pm = map(&[(YOUNG, OLD_TO)]);
    let only_keys = pointer_map_fold_filter(&pm, (None, &[]));
    let mut young_only = FoldAddrFilter::default();
    young_only.insert(YOUNG);
    assert!(only_keys.intersects(&young_only));
    assert!(!only_keys.intersects(&a), "a map VALUE is not a lookup key");
    let spans = [(OLD, 64usize)];
    let with_arena = pointer_map_fold_filter(&pm, (Some((YOUNG, YOUNG + 64)), &spans));
    assert!(with_arena.intersects(&a), "the vacated spans are covered");
    // The cache answers only for the epoch it was stored for.
    let cell = ParkedFoldFilter::new();
    assert_eq!(cell.cached(0), None);
    cell.store(7, a);
    assert_eq!(cell.cached(7), Some(a));
    assert_eq!(cell.cached(8), None);
    cell.clear();
    assert_eq!(cell.cached(7), None);
}

/// The skip is exact and the epoch is what keeps it so. A word planted
/// WITHOUT an epoch bump (what the mount rule forbids) is not seen by a
/// skipped fold -- which proves the skip ran -- and a mount's epoch bump
/// makes the next fold walk the entry again and remap it.
#[test]
fn gci_i12vp_parked_fold_skips_only_a_provably_untouched_continuation() {
    cratonvm_gc::gc_quiescence::clear_peer_stack_slots();
    let (registry, t) = blocked_entry(true, true);
    let young = map(&[(YOUNG, YOUNG_TO)]);
    // Fold 1: no filter yet -- the full fold, which leaves one.
    registry.fold_pointer_map_into_blocked(&young);
    assert_eq!(folds(&registry, t), 1);
    assert_eq!(snapshot_of(&registry, t), vec![OLD]);
    if !cratonvm_gc::gc_quiescence::blocked_peer_stack_remap_enabled() {
        // The native slots are not walked, so no filter may be cached.
        assert_eq!(cached(&registry, t), None);
        return;
    }
    assert!(cached(&registry, t).is_some(), "an unmounted entry leaves its filter");
    // Plant a young word behind the epoch's back.
    plant(&registry, t, YOUNG);
    // Fold 2: the cached filter (old granule only) is disjoint from the
    // young map: skipped, so the planted word is NOT remapped.
    registry.fold_pointer_map_into_blocked(&young);
    assert_eq!(folds(&registry, t), 2, "a skipped fold still counts as a fold");
    assert_eq!(snapshot_of(&registry, t), vec![OLD, YOUNG], "the fold was skipped");
    // A mount and an unmount bump the epoch: fold 3 walks the entry.
    registry.set_mounted(t, true);
    registry.set_mounted(t, false);
    assert_eq!(cached(&registry, t), None, "a mount retires the filter");
    registry.fold_pointer_map_into_blocked(&young);
    assert_eq!(snapshot_of(&registry, t), vec![OLD, YOUNG_TO]);
    let refreshed = cached(&registry, t).expect("refreshed filter");
    let mut young_to = FoldAddrFilter::default();
    young_to.insert(YOUNG_TO);
    assert!(refreshed.intersects(&young_to), "the new word is summarised");
    // Fold 4: an old-granule key (a full / mixed collection) intersects:
    // the full fold runs and moves the old word in every set.
    registry.fold_pointer_map_into_blocked(&map(&[(OLD, OLD_TO)]));
    assert_eq!(snapshot_of(&registry, t), vec![OLD_TO, YOUNG_TO]);
    assert_eq!(origin_cur(&registry, t), vec![OLD_TO]);
    assert_eq!(folds(&registry, t), 4);
}

/// A mounted entry (every platform thread, and a continuation blocked
/// while mounted) is never filtered: it gets no filter and every fold
/// walks it.
#[test]
fn gci_i12vp_mounted_entries_are_never_filtered() {
    cratonvm_gc::gc_quiescence::clear_peer_stack_slots();
    let (registry, t) = blocked_entry(true, false);
    registry.fold_pointer_map_into_blocked(&map(&[(YOUNG, YOUNG_TO)]));
    assert_eq!(cached(&registry, t), None);
    plant(&registry, t, YOUNG);
    registry.fold_pointer_map_into_blocked(&map(&[(YOUNG, YOUNG_TO)]));
    assert_eq!(snapshot_of(&registry, t), vec![OLD, YOUNG_TO]);
}

/// With the switch off (the default) the fold is the old fold: nothing is
/// cached, nothing skipped.
#[test]
fn gci_i12vp_filter_off_is_the_old_fold() {
    cratonvm_gc::gc_quiescence::clear_peer_stack_slots();
    let (mut registry, t) = blocked_entry(false, true);
    // Both filters off (the gather filter is default ON since 2026-10-17
    // and keeps the fold's filter for its own gather).
    registry.set_vt_parked_gather_filter_for_test(false);
    let young = map(&[(YOUNG, YOUNG_TO)]);
    registry.fold_pointer_map_into_blocked(&young);
    assert_eq!(cached(&registry, t), None);
    plant(&registry, t, YOUNG);
    registry.fold_pointer_map_into_blocked(&young);
    assert_eq!(snapshot_of(&registry, t), vec![OLD, YOUNG_TO]);
}

/// Every `set_mounted`, and every replacement of the entry's snapshot or
/// block-state `Arc`, moves the epoch (and so retires a stored filter).
#[test]
fn gci_i12vp_mounts_and_replaced_state_move_the_epoch() {
    let (registry, t) = blocked_entry(true, true);
    let e0 = epoch(&registry, t);
    registry.set_mounted(t, true);
    let e1 = epoch(&registry, t);
    assert!(e1 > e0, "mount");
    registry.set_mounted(t, false);
    let e2 = epoch(&registry, t);
    assert!(e2 > e1, "unmount");
    registry.set_root_snapshot(t, Arc::new(Mutex::new(vec![fake(OLD)])));
    let e3 = epoch(&registry, t);
    assert!(e3 > e2, "new snapshot Arc");
    registry.set_gc_block_state(t, Arc::new(GcBlockState::new()));
    let e4 = epoch(&registry, t);
    assert!(e4 > e3, "new block-state Arc");
}

/// The census lines name what the brief asks to narrow with: parked
/// entries, their words, the skip count and the time.
#[test]
fn gci_i12vp_census_lines() {
    let mut g = ParkedGatherCensus::default();
    g.note(false, 10);
    g.note(true, 7);
    g.note(true, 5);
    assert_eq!(
        g.line(30, std::time::Duration::from_micros(42)),
        "[vt-parked-roots] gather alive=3 parked=2 words=22 parked_words=12 roots=30 us=42"
    );
    let f = ParkedFoldCensus {
        blocked: 4,
        parked: 3,
        cached: 2,
        skipped: 1,
        stored: 2,
        parked_words: 9,
        map_filter_nanos: 5_000,
    };
    assert_eq!(
        f.line(100, true, std::time::Duration::from_micros(77)),
        "[vt-parked-roots] fold filter=on map=100 blocked=4 parked=3 cached=2 skipped=1 \
         stored=2 parked_words=9 map_filter_us=5 us=77"
    );
}

fn read(rel: &str) -> String {
    let path = format!("{}/{rel}", env!("CARGO_MANIFEST_DIR"));
    std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("{path}: {e}"))
        .replace("\r\n", "\n")
}

/// The text of the item starting at `sig`, up to the next `end`.
fn body(src: &str, sig: &str, end: &str) -> String {
    let start = src.find(sig).unwrap_or_else(|| panic!("`{sig}` not found"));
    let rest = &src[start..];
    let stop = rest
        .find(end)
        .unwrap_or_else(|| panic!("end of `{sig}` not found"));
    rest[..stop].to_string()
}

fn at(text: &str, needle: &str, what: &str) -> usize {
    text.find(needle)
        .unwrap_or_else(|| panic!("{what}: `{needle}` not found"))
}

/// The epoch rule's premise, on both mount paths: the mount bumps the
/// epoch (`set_mounted(tid, true)`) BEFORE the remount's drain rewrites
/// anything, and the unmount bumps it BEFORE the yield's deposit raises
/// the flag over new words.
#[test]
fn gci_i12vp_mounts_bump_the_epoch_before_the_words_change() {
    let jdk = read("src/vm/jdk_continuation.rs");
    let slice = body(&jdk, "fn run_slice(", "\n}\n");
    let mount = at(&slice, "registry.set_mounted(tid, true);", "run_slice mount");
    let drain = at(&slice, ".check_post_block_gc();", "run_slice drain");
    assert!(mount < drain, "run_slice: mount@{mount} drain@{drain}");
    let arm = &slice[at(
        &slice,
        "VmError::ContinuationYield { .. })) = &result",
        "run_slice yield arm",
    )..];
    let unmount = at(
        arm,
        "registry.set_mounted(tid, false);",
        "run_slice unmount",
    );
    let deposit = at(arm, ".deposit_root_snapshot();", "run_slice deposit");
    assert!(
        unmount < deposit,
        "run_slice: unmount@{unmount} deposit@{deposit}"
    );

    let exec = read("src/vm/vm_exec/diagnose_sync_on_value_based_class.rs");
    let resume = body(&exec, "fn resume_virtual_continuation(", "\n}\n");
    let mount = at(&resume, ".set_mounted(tid, true);", "bound mount");
    let drain = at(&resume, ".check_post_block_gc();", "bound drain");
    assert!(mount < drain, "resume_virtual_continuation: mount@{mount} drain@{drain}");
    let arm = &resume[at(
        &resume,
        "VmError::ContinuationYield { wake_after_nanos })) =",
        "bound yield arm",
    )..];
    let unmount = at(arm, ".set_mounted(tid, false);", "bound unmount");
    let deposit = at(arm, ".deposit_root_snapshot();", "bound deposit");
    assert!(
        unmount < deposit,
        "bound: unmount@{unmount} deposit@{deposit}"
    );
    // And `set_mounted` bumps before it stores.
    let reg = read("src/threading/thread_registry/deposit.rs");
    let set = body(&reg, "    pub fn set_mounted(", "\n    }\n");
    let bump = at(&set, "mount_epoch.fetch_add(1", "epoch bump");
    let store = at(&set, "entry.mounted.store(mounted", "mounted store");
    assert!(bump < store, "set_mounted: bump@{bump} store@{store}");
}
