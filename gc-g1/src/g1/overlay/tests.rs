use super::*;
use cratonvm_gc_core::external_roots::{ExternalRootProvider, OwnerPredicate};
use std::collections::HashSet;

// -- a test overlay provider ---------------------------------------------
//
// Rows are keyed by addresses in each test's own heap. Heaps that are
// alive at the same time never share addresses, and every row naming a
// heap is removed by the fixture's `Drop`, so a concurrently running test
// (in this module or another collector's) never finds a row naming its
// objects.

/// One test row: a stable `key`, the owner's current address, its refs.
#[derive(Clone)]
struct TRow {
    key: usize,
    owner: usize,
    refs: Vec<usize>,
}
static ROWS: std::sync::Mutex<Vec<TRow>> = std::sync::Mutex::new(Vec::new());
/// Keys written since the last take (the provider-side record).
static DIRTY: std::sync::Mutex<Vec<usize>> = std::sync::Mutex::new(Vec::new());
static NEXT_KEY: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(1);

fn rows() -> std::sync::MutexGuard<'static, Vec<TRow>> {
    ROWS.lock().unwrap_or_else(|e| e.into_inner())
}
fn dirty() -> std::sync::MutexGuard<'static, Vec<usize>> {
    DIRTY.lock().unwrap_or_else(|e| e.into_inner())
}
fn obj(addr: usize) -> ObjectRef {
    unsafe { ObjectRef::from_raw(addr as *mut u8) }
}
fn t_scan(out: &mut Vec<ObjectRef>) {
    for row in rows().iter() {
        out.extend(row.refs.iter().map(|&r| obj(r)));
    }
}
fn t_owner_addrs() -> Option<HashSet<usize>> {
    Some(rows().iter().map(|r| r.owner).collect())
}
fn t_roots_for_owner(owner: usize, _class: Option<u32>) -> Vec<ObjectRef> {
    rows()
        .iter()
        .filter(|r| r.owner == owner)
        .flat_map(|r| r.refs.iter().map(|&x| obj(x)))
        .collect()
}
fn t_matching(pred: &OwnerPredicate<'_>) -> Vec<ObjectRef> {
    let snapshot: Vec<TRow> = rows().clone();
    snapshot
        .into_iter()
        .filter(|r| pred(r.owner))
        .flat_map(|r| r.refs.into_iter().map(obj))
        .collect()
}
fn t_remap(map: &cratonvm_types::PointerMap) {
    for row in rows().iter_mut() {
        if let Some(&n) = map.get(&row.owner) {
            row.owner = n;
        }
        for r in row.refs.iter_mut() {
            if let Some(&n) = map.get(r) {
                *r = n;
            }
        }
    }
}
std::thread_local! {
    /// The arenas of the [`Fixture`]s alive on THIS thread. `ROWS` holds
    /// every running test's rows, and `prune` fans out to every provider
    /// from every test in the binary -- including
    /// `external_roots::tests::prune_condemns_only_the_collecting_heaps_owners`,
    /// whose deliberately unscreened predicate (`owned = None`) answers
    /// "dead" for every address but one constant. Judging all rows let
    /// that test delete a concurrently running fixture's row mid-pause
    /// (`a_rooted_cset_owner_keeps_its_row_parallel`, ~1 run in 20 of the
    /// full suite). A real overlay provider is per VM; this one is per
    /// test thread: a prune judges only the rows its own fixtures own.
    static MY_ARENAS: std::cell::RefCell<Vec<(usize, usize)>> =
        const { std::cell::RefCell::new(Vec::new()) };
}
fn owned_by_this_thread(owner: usize) -> bool {
    MY_ARENAS.with(|a| a.borrow().iter().any(|&(lo, hi)| owner >= lo && owner < hi))
}
fn t_prune(is_live: &OwnerPredicate<'_>) {
    let snapshot: Vec<TRow> = rows().clone();
    let dead: HashSet<usize> = snapshot
        .iter()
        .filter(|r| owned_by_this_thread(r.owner) && !is_live(r.owner))
        .map(|r| r.key)
        .collect();
    rows().retain(|r| !dead.contains(&r.key));
}
fn as_owner_row(r: &TRow) -> cratonvm_gc_core::external_roots::OwnerRow {
    cratonvm_gc_core::external_roots::OwnerRow {
        owner: r.owner,
        key: r.key,
        refs: r.refs.iter().map(|&x| obj(x)).collect(),
    }
}
fn t_all_rows(pred: &OwnerPredicate<'_>) -> Vec<cratonvm_gc_core::external_roots::OwnerRow> {
    let snapshot: Vec<TRow> = rows().clone();
    snapshot
        .iter()
        .filter(|r| pred(r.owner))
        .map(as_owner_row)
        .collect()
}
fn t_rows_for_keys(keys: &[usize]) -> Vec<cratonvm_gc_core::external_roots::OwnerRow> {
    let snapshot: Vec<TRow> = rows().clone();
    snapshot
        .iter()
        .filter(|r| keys.contains(&r.key))
        .map(as_owner_row)
        .collect()
}
fn t_take_dirty(pred: &OwnerPredicate<'_>) -> Vec<usize> {
    let snapshot: Vec<TRow> = rows().clone();
    let owner_of = |k: usize| snapshot.iter().find(|r| r.key == k).map(|r| r.owner);
    let mut d = dirty();
    let (mine, theirs): (Vec<usize>, Vec<usize>) = d
        .drain(..)
        .filter(|&k| owner_of(k).is_some())
        .partition(|&k| owner_of(k).is_some_and(|o| pred(o)));
    d.extend(theirs);
    mine
}
fn t_enable() {}

fn register_provider() {
    cratonvm_gc_core::external_roots::register_external_root_provider(ExternalRootProvider {
        name: "g1-overlay-test-rows",
        scan: t_scan,
        owner_addrs: t_owner_addrs,
        roots_for_owner: t_roots_for_owner,
        roots_for_matching_owners: t_matching,
        remap: t_remap,
        prune: t_prune,
        gate_stats: None,
        owner_rows: Some(cratonvm_gc_core::external_roots::OwnerRowCallbacks {
            all_rows: t_all_rows,
            rows_for_keys: t_rows_for_keys,
            take_dirty_keys: t_take_dirty,
            enable_dirty_tracking: t_enable,
        }),
    });
}

/// One test's heap; every row naming it is dropped at the end.
struct Fixture {
    gc: G1Collector,
}
impl Fixture {
    fn new() -> Self {
        register_provider();
        let config = G1CollectorConfig {
            heap_size: 8 * 1024 * 1024,
            initial_heap_size: 8 * 1024 * 1024,
            region_size: 1024 * 1024,
            max_gc_pause_ms: 200,
            ihop_percent: 45,
            promotion_age: 3,
            gc_worker_threads: 1,
            string_dedup_enabled: false,
            mixed_gc_count_target: 8,
            old_cset_region_threshold_percent: 10,
            mixed_gc_live_threshold_percent: 85,
            heap_waste_percent: 0,
        };
        let gc = G1Collector::new(config);
        MY_ARENAS.with(|a| a.borrow_mut().push((gc.arena_base, gc.arena_end)));
        Self { gc }
    }
    /// Write `owner`'s row (keeping its key if it has one) and record the
    /// write, as a mutator's overlay store does.
    fn set_row(&self, owner: ObjectRef, refs: &[ObjectRef]) {
        let owner = owner.as_ptr() as usize;
        let refs: Vec<usize> = refs.iter().map(|r| r.as_ptr() as usize).collect();
        let mut rows = rows();
        let key = match rows.iter_mut().find(|r| r.owner == owner) {
            Some(row) => {
                row.refs = refs;
                row.key
            }
            None => {
                let key = NEXT_KEY.fetch_add(1, Ordering::Relaxed);
                rows.push(TRow { key, owner, refs });
                key
            }
        };
        dirty().push(key);
    }
    fn row(&self, owner: usize) -> Option<Vec<usize>> {
        rows()
            .iter()
            .find(|r| r.owner == owner)
            .map(|r| r.refs.clone())
    }
    /// The VM's post-pause half: prune by the collector's verdict
    /// (forwarded, or in a live region and not condemned), then remap.
    fn after_pause(&self, result: &GcResult) {
        let condemned = self.gc.take_overlay_condemned_owners();
        let span = Some((self.gc.arena_base, self.gc.arena_end));
        cratonvm_gc_core::external_roots::prune_external_roots_owned(span, &|owner| {
            !condemned.contains(&owner)
                && (result.pointer_map.contains_key(&owner)
                    || self.gc.is_addr_in_live_region(owner))
        });
        cratonvm_gc_core::external_roots::remap_external_roots(&result.pointer_map);
    }
    fn marked(&self, addr: usize) -> bool {
        let regions = self.gc.regions.read();
        let idx = self
            .gc
            .region_for_ptr(&regions, addr as *mut u8)
            .expect("in heap");
        regions[idx].mark_bitmap.is_marked(addr)
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let (lo, hi) = (self.gc.arena_base, self.gc.arena_end);
        rows().retain(|r| !(r.owner >= lo && r.owner < hi));
        MY_ARENAS.with(|a| a.borrow_mut().retain(|&span| span != (lo, hi)));
    }
}

struct NoMonitors;
impl MonitorCleanup for NoMonitors {
    fn remap_after_gc(&self, _pointer_map: &cratonvm_types::PointerMap) {}
}
fn stw() -> cratonvm_gc_core::collector::StopTheWorldToken {
    // SAFETY: single-threaded fixture.
    unsafe { cratonvm_gc_core::collector::StopTheWorldToken::new() }
}

fn young(f: &Fixture, roots: &mut [ObjectRef], parallel: bool) -> GcResult {
    f.gc.overlay_collection_begin();
    if parallel {
        f.gc.young_collection_parallel(roots, &NoMonitors)
    } else {
        f.gc.young_collection_serial(roots, &NoMonitors)
    }
}

const OWNER_CLASS: u32 = 0x00C0_FFE1;
const ELEM_CLASS: u32 = 0x00C0_FFE2;

/// A live owner in the CSet keeps an element held ONLY by its row; the
/// row is re-addressed to the copies.
fn a_rooted_cset_owner_keeps_its_row(parallel: bool) {
    let f = Fixture::new();
    let owner = f.gc.alloc_object(ClassId::new(OWNER_CLASS), 1);
    let elem = f.gc.alloc_object(ClassId::new(ELEM_CLASS), 1);
    f.gc.set_field(elem, 0, Value::Int(77));
    f.set_row(owner, &[elem]);
    let mut roots = vec![owner];
    let result = young(&f, &mut roots, parallel);
    let new_owner = *result
        .pointer_map
        .get(&(owner.as_ptr() as usize))
        .expect("the rooted owner is evacuated");
    let new_elem = *result
        .pointer_map
        .get(&(elem.as_ptr() as usize))
        .expect("an element held only by a live owner's row must survive");
    f.after_pause(&result);
    assert_eq!(f.row(new_owner), Some(vec![new_elem]));
    assert_eq!(f.gc.get_field(obj(new_elem), 0).as_int(), Some(77));
}

/// The fixture's own isolation, which the parallel variant above depended
/// on without having it: `prune` fans out to every provider in the binary,
/// and another test's span-less, all-"dead" prune (as
/// `external_roots::tests::prune_condemns_only_the_collecting_heaps_owners`
/// issues) must not delete this thread's rows -- that deleted a row
/// between a fixture's `set_row` and its pause, ~1 full-suite run in 20.
#[test]
fn a_prune_from_another_test_thread_leaves_this_threads_rows() {
    let f = Fixture::new();
    let owner = f.gc.alloc_object(ClassId::new(OWNER_CLASS), 1);
    let elem = f.gc.alloc_object(ClassId::new(ELEM_CLASS), 1);
    f.set_row(owner, &[elem]);
    std::thread::spawn(|| cratonvm_gc_core::external_roots::prune_external_roots_owned(None, &|_| false))
        .join()
        .expect("the foreign prune ran");
    assert_eq!(
        f.row(owner.as_ptr() as usize),
        Some(vec![elem.as_ptr() as usize]),
        "a prune on a thread with no fixture must not condemn this thread's row"
    );
    // ...while this thread's own prune still judges it.
    cratonvm_gc_core::external_roots::prune_external_roots_owned(None, &|_| false);
    assert_eq!(
        f.row(owner.as_ptr() as usize),
        None,
        "the owning thread's prune applies"
    );
}

#[test]
fn a_rooted_cset_owner_keeps_its_row_serial() {
    a_rooted_cset_owner_keeps_its_row(false);
}

#[test]
fn a_rooted_cset_owner_keeps_its_row_parallel() {
    a_rooted_cset_owner_keeps_its_row(true);
}

/// The defect this module closes: a dead owner whose element points back
/// at it is collected, and its row is condemned rather than left holding
/// a reference into a reclaimed region.
fn a_self_referencing_dead_owner_is_collected(parallel: bool) {
    let f = Fixture::new();
    let owner = f.gc.alloc_object(ClassId::new(OWNER_CLASS), 1);
    let elem = f.gc.alloc_object(ClassId::new(ELEM_CLASS), 1);
    f.gc.set_field(elem, 0, Value::Object(Some(owner)));
    f.gc.set_field(owner, 0, Value::Int(1));
    f.set_row(owner, &[elem]);
    let owner_addr = owner.as_ptr() as usize;
    let mut roots: Vec<ObjectRef> = vec![];
    let result = young(&f, &mut roots, parallel);
    assert!(
        !result.pointer_map.contains_key(&owner_addr),
        "nothing but its own element names the owner: it is dead"
    );
    assert!(
        !result.pointer_map.contains_key(&(elem.as_ptr() as usize)),
        "the element is reachable only through a dead owner's row"
    );
    let condemned = f.gc.take_overlay_condemned_owners();
    assert!(
        condemned.contains(&owner_addr),
        "the dead owner is reported"
    );
    // Re-arm for the fixture's own prune.
    f.gc.overlay_condemned_owners.lock().extend(condemned);
    f.after_pause(&result);
    assert_eq!(f.row(owner_addr), None, "the dead owner's row is pruned");
}

#[test]
fn a_self_referencing_dead_owner_is_collected_serial() {
    a_self_referencing_dead_owner_is_collected(false);
}

#[test]
fn a_self_referencing_dead_owner_is_collected_parallel() {
    a_self_referencing_dead_owner_is_collected(true);
}

/// An owner outside the collection set (here: in a pinned region) is live
/// for the pause, so its row's young element is a root.
#[test]
fn a_non_cset_owner_roots_its_row() {
    let f = Fixture::new();
    let owner = f.gc.alloc_object(ClassId::new(OWNER_CLASS), 1);
    let region_of = |o: ObjectRef| {
        let regions = f.gc.regions.read();
        f.gc.region_for_ptr(&regions, o.as_ptr()).unwrap()
    };
    let owner_region = region_of(owner);
    let mut elem = f.gc.alloc_object(ClassId::new(ELEM_CLASS), 1);
    while region_of(elem) == owner_region {
        elem = f.gc.alloc_object(ClassId::new(ELEM_CLASS), 1);
    }
    f.gc.set_field(elem, 0, Value::Int(5));
    f.set_row(owner, &[elem]);
    f.gc.pin_region(owner_region);
    let mut roots: Vec<ObjectRef> = vec![];
    let result = young(&f, &mut roots, false);
    let new_elem = *result
        .pointer_map
        .get(&(elem.as_ptr() as usize))
        .expect("a non-CSet owner's row is a root");
    f.after_pause(&result);
    assert_eq!(f.row(owner.as_ptr() as usize), Some(vec![new_elem]));
    f.gc.unpin_region(owner_region);
}

/// Owner -> row -> owner -> row: the fixpoint follows an owner that only
/// becomes live through another owner's row.
fn nested_owners_reach_a_fixpoint(parallel: bool) {
    let f = Fixture::new();
    let outer = f.gc.alloc_object(ClassId::new(OWNER_CLASS), 1);
    let inner = f.gc.alloc_object(ClassId::new(OWNER_CLASS), 1);
    let elem = f.gc.alloc_object(ClassId::new(ELEM_CLASS), 1);
    f.set_row(outer, &[inner]);
    f.set_row(inner, &[elem]);
    let mut roots = vec![outer];
    let result = young(&f, &mut roots, parallel);
    for (what, o) in [("inner", inner), ("elem", elem)] {
        assert!(
            result.pointer_map.contains_key(&(o.as_ptr() as usize)),
            "{what} is live through the chain of rows"
        );
    }
    assert!(f.gc.take_overlay_condemned_owners().is_empty());
}

#[test]
fn nested_owners_reach_a_fixpoint_serial() {
    nested_owners_reach_a_fixpoint(false);
}

#[test]
fn nested_owners_reach_a_fixpoint_parallel() {
    nested_owners_reach_a_fixpoint(true);
}

/// Marking follows a live owner's row and not a dead one's, and the
/// post-mark prune drops exactly the dead owner's row.
#[test]
fn marking_follows_live_owner_rows_only_and_prunes_the_dead() {
    let f = Fixture::new();
    let live_owner = f.gc.alloc_object(ClassId::new(OWNER_CLASS), 1);
    let live_elem = f.gc.alloc_object(ClassId::new(ELEM_CLASS), 1);
    let dead_owner = f.gc.alloc_object(ClassId::new(OWNER_CLASS), 1);
    let dead_elem = f.gc.alloc_object(ClassId::new(ELEM_CLASS), 1);
    f.gc.set_field(dead_elem, 0, Value::Object(Some(dead_owner)));
    f.set_row(live_owner, &[live_elem]);
    f.set_row(dead_owner, &[dead_elem]);

    f.gc.start_concurrent_mark(&stw());
    f.gc.remark(&stw(), &[live_owner]);
    while !f.gc.concurrent_mark_step(usize::MAX) {}

    assert!(f.marked(live_owner.as_ptr() as usize));
    assert!(
        f.marked(live_elem.as_ptr() as usize),
        "the marker follows a scanned owner's row"
    );
    assert!(!f.marked(dead_owner.as_ptr() as usize));
    assert!(
        !f.marked(dead_elem.as_ptr() as usize),
        "a dead owner's row is not a root: the self-referencing pair is garbage"
    );

    f.gc.prune_overlays_after_mark();
    assert!(f.row(live_owner.as_ptr() as usize).is_some());
    assert_eq!(f.row(dead_owner.as_ptr() as usize), None);
    f.gc.cleanup(&stw());
    assert_eq!(f.gc.overlay_snapshot_len.load(Ordering::Relaxed), 0);
}

/// SATB: an element removed from a row after the snapshot is still marked
/// on behalf of its (live) owner — the mutator may have stored it into an
/// object the marker already scanned.
#[test]
fn marking_uses_the_snapshot_row_not_the_current_one() {
    let f = Fixture::new();
    let owner = f.gc.alloc_object(ClassId::new(OWNER_CLASS), 1);
    let elem = f.gc.alloc_object(ClassId::new(ELEM_CLASS), 1);
    f.set_row(owner, &[elem]);
    f.gc.start_concurrent_mark(&stw());
    // The mutator empties the row after the snapshot.
    f.set_row(owner, &[]);
    f.gc.remark(&stw(), &[owner]);
    while !f.gc.concurrent_mark_step(usize::MAX) {}
    assert!(
        f.marked(elem.as_ptr() as usize),
        "a snapshot-live element is marked even though the row no longer holds it"
    );
    f.gc.cleanup(&stw());
}

/// A young pause inside a mark cycle keeps the snapshot's values alive and
/// re-addresses the snapshot, so the marker still finds the edge.
#[test]
fn a_young_pause_carries_the_mark_snapshot() {
    let f = Fixture::new();
    let owner = f.gc.alloc_object(ClassId::new(OWNER_CLASS), 1);
    let elem = f.gc.alloc_object(ClassId::new(ELEM_CLASS), 1);
    f.set_row(owner, &[elem]);
    f.gc.start_concurrent_mark(&stw());
    // Removed from the row after the snapshot, so only the snapshot keeps
    // it alive across the pause.
    f.set_row(owner, &[]);
    let mut roots = vec![owner];
    let result = young(&f, &mut roots, false);
    let new_owner = *result.pointer_map.get(&(owner.as_ptr() as usize)).unwrap();
    let new_elem = *result
        .pointer_map
        .get(&(elem.as_ptr() as usize))
        .expect("a snapshot value survives a pause inside the cycle");
    f.after_pause(&result);
    assert_eq!(
        f.gc.overlay_snapshot.read().values_of(new_owner),
        Some(vec![new_elem]),
        "the snapshot is re-keyed and re-addressed through the pause"
    );
    f.gc.remark(&stw(), &[obj(new_owner)]);
    while !f.gc.concurrent_mark_step(usize::MAX) {}
    assert!(f.marked(new_elem));
    f.gc.cleanup(&stw());
}

fn key_of(owner: usize) -> usize {
    rows()
        .iter()
        .find(|r| r.owner == owner)
        .map(|r| r.key)
        .unwrap()
}

/// An owner outside every collection set: allocated, then its region
/// pinned. Returns the owner and its region.
fn pinned_owner(f: &Fixture) -> (ObjectRef, usize) {
    let owner = f.gc.alloc_object(ClassId::new(OWNER_CLASS), 1);
    let region = {
        let regions = f.gc.regions.read();
        f.gc.region_for_ptr(&regions, owner.as_ptr()).unwrap()
    };
    // Move the allocation cursor off the pinned region so later objects
    // land in collectable Eden.
    loop {
        let probe = f.gc.alloc_object(ClassId::new(ELEM_CLASS), 1);
        let r = {
            let regions = f.gc.regions.read();
            f.gc.region_for_ptr(&regions, probe.as_ptr()).unwrap()
        };
        if r != region {
            break;
        }
    }
    f.gc.pin_region(region);
    (owner, region)
}

/// The remembered-set path end to end on a young pause: a row written
/// after the full read is found through the provider's written-key record;
/// the young reference it then holds is found again by the next pause with
/// no write at all (the remembered set); once that reference is old, the
/// row is forgotten.
#[test]
fn young_pauses_read_written_and_remembered_rows() {
    let f = Fixture::new();
    let (owner, region) = pinned_owner(&f);
    let owner_addr = owner.as_ptr() as usize;
    f.set_row(owner, &[]);
    let key = key_of(owner_addr);

    // Pause 1: the full read that makes the remembered set valid.
    let r1 = young(&f, &mut [], false);
    f.after_pause(&r1);
    assert!(
        f.gc.overlay_rset.lock().valid,
        "a full read leaves a valid set"
    );

    // A mutator stores a young object into the OLD owner's row.
    let elem = f.gc.alloc_object(ClassId::new(ELEM_CLASS), 1);
    f.gc.set_field(elem, 0, Value::Int(9));
    f.set_row(owner, &[elem]);
    let r2 = young(&f, &mut [], false);
    let e2 = *r2
        .pointer_map
        .get(&(elem.as_ptr() as usize))
        .expect("a row written since the last pause is read");
    f.after_pause(&r2);
    assert!(
        f.gc.overlay_rset.lock().keys.iter().any(|&(_, k)| k == key),
        "a row holding a young reference is remembered"
    );

    // No write this time: only the remembered set can find it.
    let r3 = young(&f, &mut [], false);
    let e3 = *r3
        .pointer_map
        .get(&e2)
        .expect("a remembered row's young reference survives a write-free pause");
    f.after_pause(&r3);
    assert_eq!(f.row(owner_addr), Some(vec![e3]));
    assert_eq!(f.gc.get_field(obj(e3), 0).as_int(), Some(9));

    // The owner sits in a pinned EDEN region, so it is young itself and
    // the row stays remembered for that reason alone. Unpin it, keep it
    // live through a root, and pause until owner and reference are both
    // promoted: then the row is forgotten.
    f.gc.unpin_region(region);
    let mut owner_now = owner_addr;
    let mut now = e3;
    let mut both_old = false;
    for _ in 0..12 {
        let mut roots = [obj(owner_now)];
        let r = young(&f, &mut roots, false);
        owner_now = roots[0].as_ptr() as usize;
        if let Some(&n) = r.pointer_map.get(&now) {
            now = n;
        }
        f.after_pause(&r);
        both_old = {
            let regions = f.gc.regions.read();
            !f.gc.overlay_is_young(&regions, now) && !f.gc.overlay_is_young(&regions, owner_now)
        };
        if both_old {
            break;
        }
    }
    assert!(
        both_old,
        "owner and reference are promoted within the tenuring threshold"
    );
    assert_eq!(f.row(owner_now), Some(vec![now]));
    assert_eq!(f.gc.get_field(obj(now), 0).as_int(), Some(9));
    assert!(
        !f.gc.overlay_rset.lock().keys.iter().any(|&(_, k)| k == key),
        "a row whose owner and references are all old is not remembered"
    );
}

/// An evacuation failure invalidates the remembered set, and the next
/// young pause reads every row.
#[test]
fn an_invalidated_remembered_set_forces_a_full_read() {
    let f = Fixture::new();
    let (owner, region) = pinned_owner(&f);
    f.set_row(owner, &[]);
    let r1 = young(&f, &mut [], false);
    f.after_pause(&r1);
    assert!(f.gc.overlay_rset.lock().valid);
    f.gc.overlay_rset_invalidate();
    // A row written with NO record (as if the write predated tracking):
    // only a full read can find it.
    let elem = f.gc.alloc_object(ClassId::new(ELEM_CLASS), 1);
    {
        let owner_addr = owner.as_ptr() as usize;
        let mut rows = rows();
        let row = rows.iter_mut().find(|r| r.owner == owner_addr).unwrap();
        row.refs = vec![elem.as_ptr() as usize];
    }
    let r2 = young(&f, &mut [], false);
    assert!(
        r2.pointer_map.contains_key(&(elem.as_ptr() as usize)),
        "an invalid remembered set is never trusted"
    );
    f.after_pause(&r2);
    f.gc.unpin_region(region);
}

/// The parallel driver expands a rooted CSet owner's rows INSIDE its
/// closure (the workers copy the row's graph), not only in the serial
/// fixpoint after it: the row's element and the element's own field
/// referent are both evacuated, and the expansion counter moves.
#[test]
fn the_parallel_closure_expands_owner_rows() {
    let f = Fixture::new();
    let owner = f.gc.alloc_object(ClassId::new(OWNER_CLASS), 1);
    let elem = f.gc.alloc_object(ClassId::new(ELEM_CLASS), 1);
    let leaf = f.gc.alloc_object(ClassId::new(ELEM_CLASS), 1);
    f.gc.set_field(leaf, 0, Value::Int(31));
    f.gc.set_field(elem, 0, Value::Object(Some(leaf)));
    f.set_row(owner, &[elem]);
    let before = g1_overlay_census().parallel_expanded;
    let mut roots = vec![owner];
    let result = young(&f, &mut roots, true);
    assert!(
        g1_overlay_census().parallel_expanded > before,
        "a worker expanded the owner's rows"
    );
    let new_elem = *result
        .pointer_map
        .get(&(elem.as_ptr() as usize))
        .expect("the row's element is evacuated");
    let Value::Object(Some(new_leaf)) = f.gc.get_field(obj(new_elem), 0) else {
        panic!("the element's field still names an object");
    };
    assert_ne!(
        new_leaf.as_ptr(),
        leaf.as_ptr(),
        "the element's referent moved with it"
    );
    assert_eq!(f.gc.get_field(new_leaf, 0).as_int(), Some(31));
    f.after_pause(&result);
}

/// A humongous array named by NOTHING but a live owner's overlay row (a
/// large `TreeMap`'s backing `data`) survives the eager humongous
/// reclaim, which otherwise judges spans by heap slots and roots alone.
fn an_overlay_held_humongous_span_is_not_eagerly_reclaimed(parallel: bool) {
    let f = Fixture::new();
    let owner = f.gc.alloc_object(ClassId::new(OWNER_CLASS), 1);
    let big =
        f.gc.alloc_array(ClassId::new(0), ArrayElementType::Long, 200_000);
    let big_region = {
        let regions = f.gc.regions.read();
        f.gc.region_for_ptr(&regions, big.as_ptr()).unwrap()
    };
    assert_eq!(
        f.gc.regions.read()[big_region].region_type(),
        RegionType::HumongousStart
    );
    f.set_row(owner, &[big]);
    let mut roots = vec![owner];
    let result = young(&f, &mut roots, parallel);
    assert_eq!(
        f.gc.regions.read()[big_region].region_type(),
        RegionType::HumongousStart,
        "the overlay-held humongous span is still allocated"
    );
    f.after_pause(&result);
    assert_eq!(
        f.row(roots[0].as_ptr() as usize),
        Some(vec![big.as_ptr() as usize])
    );
}

#[test]
fn an_overlay_held_humongous_span_is_not_eagerly_reclaimed_serial() {
    an_overlay_held_humongous_span_is_not_eagerly_reclaimed(false);
}

#[test]
fn an_overlay_held_humongous_span_is_not_eagerly_reclaimed_parallel() {
    an_overlay_held_humongous_span_is_not_eagerly_reclaimed(true);
}

/// gci i44/cd -- the remembered-set predicate: a young object always pins
/// its row for the next young pause, a humongous one only on the switch
/// arm (`CRATONVM_G1_OVERLAY_REMEMBER_HUMONGOUS`, default ON), Old and Free
/// never.
#[test]
fn cd44_a_humongous_reference_pins_the_row_for_young_pauses() {
    for on in [false, true] {
        assert!(overlay_kind_pins_young_pause(RegionType::Eden, on));
        assert!(overlay_kind_pins_young_pause(RegionType::Survivor, on));
        assert_eq!(overlay_kind_pins_young_pause(RegionType::HumongousStart, on), on);
        assert_eq!(
            overlay_kind_pins_young_pause(RegionType::HumongousContinuation, on),
            on
        );
        assert!(!overlay_kind_pins_young_pause(RegionType::Old, on));
        assert!(!overlay_kind_pins_young_pause(RegionType::Free, on));
    }
}

/// gci i44/cd -- an OLD owner whose row holds a humongous array and is
/// not written again after the first pause. Promotes the owner (kept by a
/// root) and then runs `pauses_after` write-free young pauses; returns
/// `(alive, remembered)`: whether the humongous span is still allocated,
/// and whether the row is in the overlay remembered set after the last
/// pause. `remember_humongous` forces the per-collector latch
/// (`OverlayRemSet::remember_humongous`) before the first pause, so the
/// kill arm is reached through the field, not the environment.
///
/// Before the fix the row left the overlay remembered set the pause its
/// owner was promoted (neither the owner nor the value was young), the
/// next young pause read the rows incrementally and did not read it, and
/// the eager humongous reclaim -- which hears of an overlay-held span only
/// through the rows a pause read -- freed the span under the row.
fn old_owner_humongous_row(
    parallel: bool,
    pauses_after: usize,
    remember_humongous: bool,
) -> (bool, bool) {
    let f = Fixture::new();
    f.gc.overlay_rset.lock().remember_humongous = Some(remember_humongous);
    let owner = f.gc.alloc_object(ClassId::new(OWNER_CLASS), 1);
    let big = f.gc.alloc_array(ClassId::new(0), ArrayElementType::Long, 200_000);
    let big_addr = big.as_ptr() as usize;
    let big_region = {
        let regions = f.gc.regions.read();
        f.gc.region_for_ptr(&regions, big.as_ptr()).unwrap()
    };
    assert_eq!(
        f.gc.regions.read()[big_region].region_type(),
        RegionType::HumongousStart
    );
    f.set_row(owner, &[big]);
    let key = key_of(owner.as_ptr() as usize);
    let mut owner_now = owner.as_ptr() as usize;
    let mut promoted = false;
    for _ in 0..12 {
        let mut roots = [obj(owner_now)];
        let r = young(&f, &mut roots, parallel);
        owner_now = roots[0].as_ptr() as usize;
        f.after_pause(&r);
        promoted = {
            let regions = f.gc.regions.read();
            !f.gc.overlay_is_young(&regions, owner_now)
        };
        if promoted {
            break;
        }
    }
    assert!(promoted, "the rooted owner is promoted within the tenuring threshold");
    assert_eq!(
        f.gc.regions.read()[big_region].region_type(),
        RegionType::HumongousStart,
        "while the owner is young its row is read every pause"
    );
    for _ in 0..pauses_after {
        let mut roots = [obj(owner_now)];
        let r = young(&f, &mut roots, parallel);
        owner_now = roots[0].as_ptr() as usize;
        f.after_pause(&r);
    }
    let alive = f.gc.regions.read()[big_region].region_type() == RegionType::HumongousStart;
    if alive {
        assert_eq!(f.row(owner_now), Some(vec![big_addr]));
    }
    let remembered = f
        .gc
        .overlay_rset
        .lock()
        .keys
        .iter()
        .any(|&(_, k)| k == key);
    (alive, remembered)
}

#[test]
fn cd44_an_old_owners_humongous_row_value_survives_write_free_young_pauses_serial() {
    let (alive, remembered) = old_owner_humongous_row(false, 3, true);
    assert!(
        alive,
        "a humongous array held only by an old owner's overlay row was eagerly reclaimed"
    );
    assert!(remembered, "the row holding a humongous value stays remembered");
}

#[test]
fn cd44_an_old_owners_humongous_row_value_survives_write_free_young_pauses_parallel() {
    let (alive, remembered) = old_owner_humongous_row(true, 3, true);
    assert!(
        alive,
        "a humongous array held only by an old owner's overlay row was eagerly reclaimed"
    );
    assert!(remembered, "the row holding a humongous value stays remembered");
}

/// gci i44/cd (lane ch's H1) -- the enable decision: the first row read,
/// and every read after the provider table moved past the generation the
/// remembered set was rebuilt under.
#[test]
fn cd44_a_provider_table_change_re_enables_dirty_tracking() {
    assert!(overlay_tracking_needs_enable(false, 0, 0));
    assert!(overlay_tracking_needs_enable(false, 7, 7));
    assert!(!overlay_tracking_needs_enable(true, 7, 7));
    assert!(overlay_tracking_needs_enable(true, 7, 8));
}

static LATE_ENABLES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
fn late_scan(_: &mut Vec<ObjectRef>) {}
fn late_owner_addrs() -> Option<HashSet<usize>> {
    Some(HashSet::new())
}
fn late_roots_for_owner(_: usize, _: Option<u32>) -> Vec<ObjectRef> {
    Vec::new()
}
fn late_matching(_: &OwnerPredicate<'_>) -> Vec<ObjectRef> {
    Vec::new()
}
fn late_remap(_: &cratonvm_types::PointerMap) {}
fn late_prune(_: &OwnerPredicate<'_>) {}
fn late_all_rows(_: &OwnerPredicate<'_>) -> Vec<cratonvm_gc_core::external_roots::OwnerRow> {
    Vec::new()
}
fn late_rows_for_keys(_: &[usize]) -> Vec<cratonvm_gc_core::external_roots::OwnerRow> {
    Vec::new()
}
fn late_take_dirty(_: &OwnerPredicate<'_>) -> Vec<usize> {
    Vec::new()
}
fn late_enable() {
    LATE_ENABLES.fetch_add(1, Ordering::Relaxed);
}

/// gci i44/cd (lane ch's H1) -- a row provider registered after this
/// collector's first overlay pause (and after a rebuild under the old
/// table generation) is told to record its written keys by the next row
/// read. The provider has no rows, so registering it changes nothing for
/// the other tests in the binary but forcing one full read. (Another test's
/// first pause can also enable it, so this can only pass spuriously, never
/// fail spuriously; the pure test above pins the decision itself.)
#[test]
fn cd44_a_late_row_provider_is_enabled_by_the_next_row_read() {
    let f = Fixture::new();
    // A full pause and its rebuild: tracking on, the set valid under the
    // current generation. The heap must hold something: a young pause over
    // an empty heap returns before its overlay phase (the first version of
    // this test paused an empty heap and never reached the row read).
    let owner = f.gc.alloc_object(ClassId::new(OWNER_CLASS), 1);
    let elem = f.gc.alloc_object(ClassId::new(ELEM_CLASS), 1);
    f.set_row(owner, &[elem]);
    let mut roots = vec![owner];
    let r = young(&f, &mut roots, false);
    f.after_pause(&r);
    assert!(
        f.gc.overlay_rset.lock().tracking,
        "the pause read the rows through the row API"
    );
    cratonvm_gc_core::external_roots::register_external_root_provider(ExternalRootProvider {
        name: "g1-overlay-test-late-rows",
        scan: late_scan,
        owner_addrs: late_owner_addrs,
        roots_for_owner: late_roots_for_owner,
        roots_for_matching_owners: late_matching,
        remap: late_remap,
        prune: late_prune,
        gate_stats: None,
        owner_rows: Some(cratonvm_gc_core::external_roots::OwnerRowCallbacks {
            all_rows: late_all_rows,
            rows_for_keys: late_rows_for_keys,
            take_dirty_keys: late_take_dirty,
            enable_dirty_tracking: late_enable,
        }),
    });
    let before = LATE_ENABLES.load(Ordering::Relaxed);
    let (_, _, incremental) = f.gc.overlay_read_rows(&RegionSet::new(), true);
    assert!(!incremental, "a changed provider table forces a full read");
    assert!(
        LATE_ENABLES.load(Ordering::Relaxed) > before,
        "the late provider's written-key record was never switched on"
    );
}

/// gci i44/cd -- the kill arm, forced through the latch field (not the
/// environment): once the owner is promoted the row is NOT in the
/// remembered set, which is the defect's precondition (the next
/// incremental young pause does not read it). Deterministic: no pause runs
/// after the promotion, so nothing is reclaimed under the row. Whether the
/// reclaim then actually fires depends on whether a concurrently running
/// test moved the provider-table generation (a full read reads the row),
/// which is why the reclaim itself is not asserted on this arm.
#[test]
fn cd44_the_kill_arm_drops_the_old_owners_humongous_row_from_the_set() {
    let (alive, remembered) = old_owner_humongous_row(false, 0, false);
    assert!(alive, "no pause ran after the promotion");
    assert!(
        !remembered,
        "with the latch off only young owners / values keep a row remembered"
    );
}
