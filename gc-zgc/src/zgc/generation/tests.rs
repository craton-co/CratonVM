use super::*;

use crate::zgc::page::ZPageConfig;

/// The same miniature geometry `page.rs`'s own tests use: OpenJDK's shape
/// at 1/512 scale, so a test heap is 256 KiB instead of 256 MiB.
fn test_page_config() -> ZPageConfig {
    ZPageConfig {
        granule_size: 4096,
        small_page_size: 8192,
        medium_page_size: 65536,
        small_object_limit: 1024,
        medium_object_limit: 8192,
        max_capacity: 4096 * 64,
    }
}

fn allocator() -> Arc<ZPageAllocator> {
    Arc::new(ZPageAllocator::new(test_page_config()).expect("test geometry must validate"))
}

fn heap_with(promotion: ZPromotionPolicy) -> ZGenerationalHeap {
    let config = ZGenerationalConfig {
        promotion,
        ..ZGenerationalConfig::default()
    };
    ZGenerationalHeap::new(allocator(), config)
}

/// 512-byte objects: Small class, 16 to an 8 KiB page.
const OBJ: usize = 512;

// -- test markers -------------------------------------------------------

/// Marks nothing. Every page in scope is reclaimable.
struct MarkNothing;

impl ZGenerationMarker for MarkNothing {
    fn mark_from_roots(
        &self,
        _id: ZGenerationId,
        _roots: &[u64],
        _scope: &ZGenerationScope,
    ) -> ZMarkReport {
        ZMarkReport::new()
    }
}

/// Marks every page in scope fully live.
struct MarkAllInScope;

impl ZGenerationMarker for MarkAllInScope {
    fn mark_from_roots(
        &self,
        _id: ZGenerationId,
        _roots: &[u64],
        scope: &ZGenerationScope,
    ) -> ZMarkReport {
        let mut report = ZMarkReport::new();
        for entry in scope.entries() {
            let extent = entry.end.saturating_sub(entry.start);
            if extent > 0 {
                report.record(entry.page_id, extent);
                report.objects_marked += 1;
            }
        }
        report
    }
}

/// The instrument for the no-tracing-into-old claim.
///
/// Records every page the scope exposed, and probes a set of known
/// old-generation addresses through the scope, counting how many the scope
/// was willing to admit. The count must be zero.
struct CountingMarker {
    /// Addresses inside old pages, supplied by the test.
    old_probes: Vec<usize>,
    visited_pages: Mutex<Vec<u64>>,
    old_admitted: AtomicU64,
    whole_heap_scopes: AtomicU64,
    calls: AtomicU64,
}

impl CountingMarker {
    fn new(old_probes: Vec<usize>) -> Self {
        CountingMarker {
            old_probes,
            visited_pages: Mutex::new(Vec::new()),
            old_admitted: AtomicU64::new(0),
            whole_heap_scopes: AtomicU64::new(0),
            calls: AtomicU64::new(0),
        }
    }
}

impl ZGenerationMarker for CountingMarker {
    fn mark_from_roots(
        &self,
        _id: ZGenerationId,
        _roots: &[u64],
        scope: &ZGenerationScope,
    ) -> ZMarkReport {
        self.calls.fetch_add(1, Ordering::Relaxed);
        if scope.covers_whole_heap() {
            self.whole_heap_scopes.fetch_add(1, Ordering::Relaxed);
        }
        {
            let mut visited = self.visited_pages.lock();
            for entry in scope.entries() {
                visited.push(entry.page_id);
            }
        }
        for probe in self.old_probes.iter() {
            if scope.admits(*probe) {
                self.old_admitted.fetch_add(1, Ordering::Relaxed);
            }
        }
        ZMarkReport::new()
    }
}

/// A remembered set backed by a fixed list of young targets.
struct FixedRememberedSet {
    targets: Vec<u64>,
}

impl ZRememberedSetView for FixedRememberedSet {
    fn iterate_old_to_young(&self, f: &mut dyn FnMut(u64)) {
        for t in self.targets.iter() {
            f(*t);
        }
    }

    fn entry_count(&self) -> usize {
        self.targets.len()
    }
}

// -- allocation routing -------------------------------------------------

#[test]
fn young_allocation_routes_to_young_pages() {
    let heap = heap_with(ZPromotionPolicy::default());
    let mut addrs: Vec<usize> = Vec::new();
    for _ in 0..40 {
        let a = heap.allocate_young(OBJ, 8).expect("young allocation");
        addrs.push(a.address);
    }

    assert!(heap.young().page_count() >= 2, "40 x 512 B spans >1 page");
    assert_eq!(heap.old().page_count(), 0, "nothing may reach old yet");

    for addr in addrs.iter() {
        assert_eq!(
            heap.generation_of_address(*addr),
            Some(ZGeneration::Young),
            "address {addr:#x} did not resolve to the young generation",
        );
    }
    let stats = heap.stats();
    assert!(stats.young_used >= 40 * OBJ);
    assert_eq!(stats.old_used, 0);
    assert_eq!(stats.promoted_pages, 0);
}

#[test]
fn old_allocation_uses_pages_young_never_owns() {
    let heap = heap_with(ZPromotionPolicy::default());
    for _ in 0..20 {
        heap.allocate_young(OBJ, 8).expect("young allocation");
    }
    for _ in 0..20 {
        heap.allocate_old(OBJ, 8).expect("old allocation");
    }

    let young: FxHashSet<u64> = heap.young().snapshot().iter().map(|p| p.id()).collect();
    let old: FxHashSet<u64> = heap.old().snapshot().iter().map(|p| p.id()).collect();
    assert!(!young.is_empty());
    assert!(!old.is_empty());
    assert!(
        young.intersection(&old).next().is_none(),
        "a page is owned by both generations: young={young:?} old={old:?}",
    );
    heap.debug_assert_generations_disjoint();
}

// -- THE central test ---------------------------------------------------

/// A minor cycle must free dead young pages and must not be able to see the
/// old generation at all.
#[test]
fn minor_cycle_frees_dead_young_pages_and_never_visits_old() {
    let heap = heap_with(ZPromotionPolicy::default());

    for _ in 0..32 {
        heap.allocate_young(OBJ, 8).expect("young allocation");
    }
    let mut old_addrs: Vec<usize> = Vec::new();
    for _ in 0..8 {
        old_addrs.push(heap.allocate_old(OBJ, 8).expect("old allocation").address);
    }

    let young_ids: FxHashSet<u64> = heap.young().snapshot().iter().map(|p| p.id()).collect();
    let old_pages = heap.old().snapshot();
    let old_ids: FxHashSet<u64> = old_pages.iter().map(|p| p.id()).collect();
    assert!(!young_ids.is_empty());
    assert!(!old_ids.is_empty());

    // Sentinel: if the minor cycle touched old at all, this moves.
    for page in old_pages.iter() {
        page.set_live_bytes(1234);
    }
    let old_page_count_before = heap.old().page_count();

    let marker = CountingMarker::new(old_addrs.clone());
    let report = heap.collect_young(&[], &ZEmptyRememberedSet, &marker);

    // The marker was offered young pages only.
    let visited: FxHashSet<u64> = marker.visited_pages.lock().iter().copied().collect();
    assert!(
        visited.intersection(&old_ids).next().is_none(),
        "the minor cycle exposed old pages {:?} to the marker",
        visited.intersection(&old_ids).collect::<Vec<_>>(),
    );
    assert_eq!(
        visited, young_ids,
        "the scope must be exactly the young set"
    );

    // Every probe of a real old address was refused.
    assert_eq!(
        marker.old_admitted.load(Ordering::Relaxed),
        0,
        "the young scope admitted an old-generation address",
    );
    assert_eq!(
        marker.whole_heap_scopes.load(Ordering::Relaxed),
        0,
        "a minor cycle handed out a whole-heap scope",
    );
    assert_eq!(marker.calls.load(Ordering::Relaxed), 1);
    assert_eq!(
        report.scope_rejected,
        old_addrs.len() as u64,
        "every out-of-scope probe should have been counted as a rejection",
    );

    // Old is untouched: same pages, same live bytes.
    assert_eq!(heap.old().page_count(), old_page_count_before);
    for page in old_pages.iter() {
        assert_eq!(
            page.live_bytes(),
            1234,
            "the minor cycle rewrote an old page's live bytes",
        );
    }

    // Young is reclaimed.
    assert_eq!(heap.young().page_count(), 0, "all young pages were dead");
    assert_eq!(report.pages_examined, young_ids.len());
    assert_eq!(report.pages_freed, young_ids.len());
    assert_eq!(report.pages_promoted, 0);
    assert!(report.bytes_freed >= 32 * OBJ);
    assert_eq!(report.young_live_bytes, 0);
}

#[test]
fn minor_cycle_takes_remembered_set_targets_as_roots_and_filters_stale_ones() {
    let heap = heap_with(ZPromotionPolicy::default());
    let mut young_addrs: Vec<u64> = Vec::new();
    for _ in 0..16 {
        young_addrs.push(heap.allocate_young(OBJ, 8).expect("young").address as u64);
    }
    let old_addr = heap.allocate_old(OBJ, 8).expect("old").address as u64;

    // One live young target, one address that is not young at all (an old
    // address standing in for a stale card).
    let rset = FixedRememberedSet {
        targets: vec![young_addrs[0], old_addr],
    };
    let report = heap.collect_young(&[], &rset, &MarkAllInScope);

    assert_eq!(report.remembered_entries, 2);
    assert_eq!(
        report.roots_scanned, 1,
        "only the young target may become a root; the old address is a stale card",
    );
}

// -- promotion ----------------------------------------------------------

#[test]
fn pages_promote_after_surviving_n_minor_cycles() {
    let heap = heap_with(ZPromotionPolicy::with_age(2));
    for _ in 0..20 {
        heap.allocate_young(OBJ, 8).expect("young allocation");
    }
    let young_before: FxHashSet<u64> = heap.young().snapshot().iter().map(|p| p.id()).collect();
    assert_eq!(
        young_before.len(),
        2,
        "20 x 512 B fills 1 page and starts a 2nd"
    );

    // Cycle 1: everything survives, age 0 -> 1, nothing promoted.
    let first = heap.collect_young(&[], &ZEmptyRememberedSet, &MarkAllInScope);
    assert_eq!(first.pages_promoted, 0);
    assert_eq!(first.pages_freed, 0);
    assert_eq!(heap.young().page_count(), 2);
    assert_eq!(heap.old().page_count(), 0);

    // Cycle 2: age 1 -> 2 >= promotion_age, both pages promote.
    let second = heap.collect_young(&[], &ZEmptyRememberedSet, &MarkAllInScope);
    assert_eq!(second.pages_promoted, 2);
    assert_eq!(heap.young().page_count(), 0);
    assert_eq!(heap.old().page_count(), 2);

    let young_after: FxHashSet<u64> = heap.old().snapshot().iter().map(|p| p.id()).collect();
    assert_eq!(
        young_after, young_before,
        "promotion must move the same pages, not copies",
    );
    let stats = heap.stats();
    assert_eq!(stats.promoted_pages, 2);
    assert_eq!(stats.minor_cycles, 2);
    heap.debug_assert_generations_disjoint();
}

#[test]
fn promotion_age_zero_promotes_every_first_cycle_survivor() {
    let heap = heap_with(ZPromotionPolicy::with_age(0));
    for _ in 0..8 {
        heap.allocate_young(OBJ, 8).expect("young allocation");
    }
    let report = heap.collect_young(&[], &ZEmptyRememberedSet, &MarkAllInScope);
    assert_eq!(report.pages_promoted, 1);
    assert_eq!(heap.old().page_count(), 1);
}

/// gci i48/zl3 (`zx48-collector-scope-audit.md` A9): the `System.gc()`
/// door's major promotes every young survivor whatever its age (default
/// ON, as on Gen and G1); `=0` kills it; without the door's request, and on
/// a young cycle even with it, the age rule stands.
#[test]
fn gci_i48_zl3_system_gc_major_tenures_every_young_survivor() {
    use cratonvm_gc_core::gc_quiescence::{request_system_gc_tenure, take_system_gc_tenure_request};
    // (pages promoted, young pages after, old pages after)
    let run = |door: bool, major: bool| {
        {
            let heap = heap_with(ZPromotionPolicy::with_age(15));
            for _ in 0..8 {
                heap.allocate_young(OBJ, 8).expect("young allocation");
            }
            let _ = take_system_gc_tenure_request();
            if door {
                request_system_gc_tenure();
            }
            let promoted = if major {
                let report = heap.collect_major(&[], &MarkAllInScope);
                report.young.pages_promoted
            } else {
                let report = heap.collect_young(&[], &ZEmptyRememberedSet, &MarkAllInScope);
                report.pages_promoted
            };
            let _ = take_system_gc_tenure_request();
            heap.debug_assert_generations_disjoint();
            (promoted, heap.young().page_count(), heap.old().page_count())            }        };
    assert_eq!(run(true, true), (1, 0, 1), "default ON: all tenured");
    assert_eq!(run(false, true), (0, 1, 0), "no door: the age rule");
    assert_eq!(run(true, false), (0, 1, 0), "a young cycle ages only");    }

// -- trigger policy -----------------------------------------------------

fn pressure_stats(young_used: usize, old_used: usize) -> ZGenerationalStats {
    ZGenerationalStats {
        young_used,
        young_capacity: 4 * 1024 * 1024,
        old_used,
        old_capacity: 12 * 1024 * 1024,
        ..ZGenerationalStats::default()
    }
}

#[test]
fn young_pressure_fires_a_minor_before_a_major() {
    let policy = ZGenerationalPolicy::default();
    // Young 75% (> 0.50), old 10% (< 0.80).
    let stats = pressure_stats(3 * 1024 * 1024, 1024 * 1024);
    assert_eq!(
        policy.should_collect(&stats),
        Some(ZGenerationalTrigger::Minor),
    );
}

#[test]
fn old_pressure_outranks_young_pressure() {
    let policy = ZGenerationalPolicy::default();
    // Both over threshold: the major branch is tested first.
    let stats = pressure_stats(3 * 1024 * 1024, 11 * 1024 * 1024);
    assert_eq!(
        policy.should_collect(&stats),
        Some(ZGenerationalTrigger::Major),
    );
}

#[test]
fn an_idle_heap_collects_nothing() {
    let policy = ZGenerationalPolicy::default();
    let stats = pressure_stats(1024 * 1024, 1024 * 1024);
    assert_eq!(policy.should_collect(&stats), None);
}

/// Regression test for the documented `ZgcRealHeap` GC storm: a live set
/// parked above the static threshold must not be able to fire a collection
/// on every poll.
#[test]
fn rearm_floor_prevents_back_to_back_collection() {
    let policy = ZGenerationalPolicy::default();
    let young_capacity = 4 * 1024 * 1024;
    let live_after = 3 * 1024 * 1024; // 75% — permanently above the 50% trigger

    // Before any collection the trigger is armed.
    let stats = pressure_stats(live_after, 0);
    assert_eq!(
        policy.should_collect(&stats),
        Some(ZGenerationalTrigger::Minor),
    );

    // A minor runs and reclaims nothing: occupancy is still 75%.
    policy.note_minor_complete(live_after, young_capacity);

    // The next poll must NOT fire — this is the whole point.
    assert_eq!(
        policy.should_collect(&stats),
        None,
        "a live set above the threshold re-fired with no fresh allocation: \
         this is the per-allocation GC-storm livelock",
    );
    // Nor may a thousand more polls.
    for _ in 0..1000 {
        assert_eq!(policy.should_collect(&stats), None);
    }

    // The floor is `live + max(headroom/4, 64 KiB)` = 3 MiB + 256 KiB.
    let headroom = young_capacity - live_after;
    let expected_floor = live_after + (headroom / 4).max(Z_REARM_MIN_BYTES);
    assert_eq!(policy.young_rearm_floor(), expected_floor);

    // One byte short of the floor: still silent.
    let almost = pressure_stats(expected_floor - 1, 0);
    assert_eq!(policy.should_collect(&almost), None);

    // At the floor, fresh allocation has re-armed it.
    let armed = pressure_stats(expected_floor, 0);
    assert_eq!(
        policy.should_collect(&armed),
        Some(ZGenerationalTrigger::Minor),
    );

    // The allocation-failure path bypasses the floor by design, so a heap
    // whose headroom is genuinely exhausted can still collect.
    assert_eq!(
        policy.should_collect_ignoring_rearm(&stats),
        Some(ZGenerationalTrigger::Minor),
    );
}

#[test]
fn major_is_scheduled_periodically_but_only_under_pressure() {
    let policy = ZGenerationalPolicy::new(
        Z_YOUNG_OCCUPANCY_MINOR_THRESHOLD,
        Z_OLD_OCCUPANCY_MAJOR_THRESHOLD,
        2, // minors_per_major
        Z_REARM_MIN_BYTES,
        Z_REARM_HEADROOM_DIVISOR,
    );

    let mut under_pressure = pressure_stats(3 * 1024 * 1024, 1024 * 1024);

    under_pressure.minors_since_major = 1;
    assert_eq!(
        policy.should_collect(&under_pressure),
        Some(ZGenerationalTrigger::Minor),
    );

    under_pressure.minors_since_major = 2;
    assert_eq!(
        policy.should_collect(&under_pressure),
        Some(ZGenerationalTrigger::Major),
        "the minor budget is spent, so the next pressure-driven cycle upgrades",
    );

    // And the periodic rule must never be a trigger of its own: an idle
    // heap collects nothing however many minors have elapsed. A standalone
    // "N minors have passed" rule would be a second unfloored latch.
    let mut idle = pressure_stats(1024 * 1024, 1024 * 1024);
    idle.minors_since_major = 500;
    assert_eq!(policy.should_collect(&idle), None);
}

// -- major cycle --------------------------------------------------------

#[test]
fn major_cycle_uses_one_whole_heap_scope_and_frees_dead_old_pages() {
    let heap = heap_with(ZPromotionPolicy::default());
    for _ in 0..16 {
        heap.allocate_young(OBJ, 8).expect("young allocation");
    }
    for _ in 0..16 {
        heap.allocate_old(OBJ, 8).expect("old allocation");
    }
    let young_ids: FxHashSet<u64> = heap.young().snapshot().iter().map(|p| p.id()).collect();
    let old_ids: FxHashSet<u64> = heap.old().snapshot().iter().map(|p| p.id()).collect();
    assert!(!young_ids.is_empty() && !old_ids.is_empty());

    let marker = CountingMarker::new(Vec::new());
    let report = heap.collect_major(&[], &marker);

    // The major's scope spans BOTH generations — that is the deviation from
    // the simulation's minor-then-old-pass shape.
    assert_eq!(marker.whole_heap_scopes.load(Ordering::Relaxed), 1);
    let visited: FxHashSet<u64> = marker.visited_pages.lock().iter().copied().collect();
    let expected: FxHashSet<u64> = young_ids.union(&old_ids).copied().collect();
    assert_eq!(visited, expected);

    // Nothing was marked, so both generations are reclaimed.
    assert_eq!(heap.young().page_count(), 0);
    assert_eq!(heap.old().page_count(), 0);
    assert_eq!(report.old_pages_examined, old_ids.len());
    assert_eq!(report.old_pages_freed, old_ids.len());
    assert_eq!(report.young.pages_freed, young_ids.len());
    assert_eq!(heap.stats().minors_since_major, 0);
}

#[test]
fn major_cycle_keeps_marked_old_pages() {
    let heap = heap_with(ZPromotionPolicy::default());
    for _ in 0..16 {
        heap.allocate_old(OBJ, 8).expect("old allocation");
    }
    let old_before = heap.old().page_count();
    assert!(old_before > 0);

    let report = heap.collect_major(&[], &MarkAllInScope);
    assert_eq!(heap.old().page_count(), old_before);
    assert_eq!(report.old_pages_freed, 0);
    assert!(report.old_live_bytes > 0);
}

// -- scope mechanics ----------------------------------------------------

#[test]
fn scope_admits_only_allocated_extents_of_its_own_pages() {
    let heap = heap_with(ZPromotionPolicy::default());
    let a = heap.allocate_young(OBJ, 8).expect("young allocation");
    let old = heap.allocate_old(OBJ, 8).expect("old allocation");

    let young_pages = heap.young().snapshot();
    let scope = ZGenerationScope::from_pages(ZGeneration::Young, 1, &young_pages, false);

    assert!(scope.admits(a.address));
    assert!(
        !scope.admits(old.address),
        "an old address must never be admitted"
    );
    assert!(scope.contains_page(a.page_id));
    assert!(!scope.contains_page(old.page_id));

    // Above the page's allocation extent is untouched reservation, never an
    // object — see `ZPageReal::walk_bounds`.
    let page = heap.allocator().page_for(a.address).expect("page");
    let (_, end) = page.walk_bounds();
    assert!(!scope.admits(end));
    assert!(!scope.admits(page.end() - 1));

    assert_eq!(scope.admitted(), 1);
    assert!(scope.rejected() >= 3);
    assert!(!scope.covers_whole_heap());
    assert_eq!(scope.generation(), ZGeneration::Young);
}

#[test]
fn an_empty_scope_admits_nothing() {
    let scope = ZGenerationScope::from_pages(ZGeneration::Young, 7, &[], false);
    assert_eq!(scope.page_count(), 0);
    assert!(!scope.admits(0));
    assert!(!scope.admits(usize::MAX));
    assert_eq!(scope.admitted(), 0);
    assert_eq!(scope.rejected(), 2);
    assert_eq!(scope.cycle(), 7);
}

// -- policy arithmetic --------------------------------------------------

#[test]
fn promotion_policy_matches_the_simulations_rule() {
    let policy = ZPromotionPolicy::with_age(3);
    // The simulation's predicate was `age + 1 >= promotion_age`, i.e. the
    // age AFTER this cycle's increment.
    assert!(!policy.should_promote(1));
    assert!(!policy.should_promote(2));
    assert!(policy.should_promote(3));
    assert!(policy.should_promote(4));
    // The survival-pressure rule is off by default.
    assert!(!policy.promote_all_survivors(10, 10));
}

#[test]
fn survival_pressure_rule_fires_only_when_configured() {
    let policy = ZPromotionPolicy {
        promotion_age: 100,
        promote_all_on_survival_above: Some(0.75),
    };
    assert!(policy.promote_all_survivors(9, 10));
    assert!(!policy.promote_all_survivors(7, 10));
    assert!(!policy.promote_all_survivors(0, 0));
}

#[test]
fn occupancy_is_zero_for_an_unsized_generation() {
    let stats = ZGenerationalStats::default();
    assert_eq!(stats.young_occupancy(), 0.0);
    assert_eq!(stats.old_occupancy(), 0.0);
}

#[test]
fn generation_split_follows_the_configured_fraction() {
    let alloc = allocator();
    let total = alloc.max_capacity();
    let heap = ZGenerationalHeap::with_defaults(alloc);
    let expected_young = ((total as f64) * Z_DEFAULT_YOUNG_FRACTION) as usize;
    assert_eq!(heap.young().capacity(), expected_young);
    assert_eq!(heap.old().capacity(), total - expected_young);
}

#[test]
fn a_freed_young_page_can_be_recycled_into_young_again() {
    // The allocator's page cache hands a freed page back with the SAME id,
    // so the generation map must be keyed on current ownership. If
    // `invalidate_hot_pages` were skipped, the stale hot-page id would
    // suppress the re-registration and the page would be invisible to the
    // next cycle.
    let heap = heap_with(ZPromotionPolicy::default());
    for _ in 0..16 {
        heap.allocate_young(OBJ, 8).expect("young allocation");
    }
    let first: FxHashSet<u64> = heap.young().snapshot().iter().map(|p| p.id()).collect();
    heap.collect_young(&[], &ZEmptyRememberedSet, &MarkNothing);
    assert_eq!(heap.young().page_count(), 0);

    for _ in 0..16 {
        heap.allocate_young(OBJ, 8).expect("young allocation");
    }
    assert!(heap.young().page_count() >= 1);
    let second: FxHashSet<u64> = heap.young().snapshot().iter().map(|p| p.id()).collect();
    assert!(
        second.intersection(&first).next().is_some(),
        "the allocator should have recycled a cached page; if it did not, this \
         test no longer exercises the id-reuse path",
    );

    // And the recycled page is collectable again.
    let report = heap.collect_young(&[], &ZEmptyRememberedSet, &MarkNothing);
    assert_eq!(report.pages_examined, second.len());
    assert_eq!(heap.young().page_count(), 0);
}

// -- seam 4, decision 3: the colour tripwire ----------------------------

/// A colored word offered as a root is COUNTED, not silently filtered.
///
/// This is the fault `adapters.rs`'s seam-4 header calls the highest-risk
/// unassigned responsibility at the remembered/generation seam: nobody
/// strips the colour and adds the heap base, `admits` answers `false` for
/// a bare 42-bit offset, and every cross-generation edge disappears with
/// no error anywhere. The counter is what turns "the remembered set is
/// empty" into "the conversion did not run".
#[test]
fn a_colored_word_offered_as_a_root_is_counted_not_silently_filtered() {
    let alloc = allocator();
    let page = alloc
        .alloc_page(ZPageSizeClass::Small, OBJ)
        .expect("a small page");
    let addr = page.alloc(OBJ, 8).expect("bump");
    let outside = alloc.end() + 0x1000;
    let scope = ZGenerationScope::from_pages(ZGeneration::Young, 1, &[page], false);

    // An ordinary in-scope address: admitted, and not colored.
    assert!(scope.admits(addr));
    assert_eq!(scope.colored_rejected(), 0);

    // An ordinary OUT-of-scope address: rejected, and still not colored.
    // The tripwire shares the reject path with every genuine
    // cross-generation pointer and must not fire on one.
    assert!(!scope.admits(outside));
    assert_eq!(scope.rejected(), 1);
    assert_eq!(scope.colored_rejected(), 0);

    // The same shape as a COLORED WORD: a 42-bit offset plus the tag.
    // This is what a real ZGC reference slot holds and what the adapter
    // hands over when decision 3 is skipped.
    let colored = crate::zgc::vaddr::color(0x1234, crate::zgc::vaddr::ZColor::Remapped);
    assert!(crate::zgc::vaddr::is_colored_word(colored));
    assert!(!scope.admits(colored as usize));
    assert_eq!(
        scope.colored_rejected(),
        1,
        "a colored word must be distinguishable from an out-of-generation \
         pointer; without that the only symptom of a skipped conversion is an \
         empty remembered set, which reads exactly like a heap with no \
         cross-generation edges",
    );
}

/// The driver-side refusal: a cycle that saw a colored root must not be
/// allowed to report success.
#[test]
#[should_panic(expected = "colored words were offered")]
fn assert_no_colored_roots_fails_the_cycle() {
    let alloc = allocator();
    let page = alloc
        .alloc_page(ZPageSizeClass::Small, OBJ)
        .expect("a small page");
    let scope = ZGenerationScope::from_pages(ZGeneration::Young, 1, &[page], false);
    let colored = crate::zgc::vaddr::color(0x40, crate::zgc::vaddr::ZColor::Marked0);
    assert!(!scope.admits(colored as usize));
    scope.assert_no_colored_roots();
}

/// ...and a clean cycle passes it, so the assertion is not vacuous.
#[test]
fn assert_no_colored_roots_passes_a_clean_cycle() {
    let alloc = allocator();
    let page = alloc
        .alloc_page(ZPageSizeClass::Small, OBJ)
        .expect("a small page");
    let outside = alloc.end() + 0x1000;
    let scope = ZGenerationScope::from_pages(ZGeneration::Young, 1, &[page], false);
    assert!(!scope.admits(outside));
    scope.assert_no_colored_roots();
}

// -- the nursery survivor census ----------------------------------------

const MIB: usize = 1024 * 1024;

fn nursery(floor_pages: usize, span_pages: usize) -> ZNurseryGeometry {
    ZNurseryGeometry {
        base: 0x1000_0000,
        page_bytes: 2 * MIB,
        floor: 0x1000_0000 + floor_pages * 2 * MIB,
        end: 0x1000_0000 + (floor_pages + span_pages) * 2 * MIB,
    }
}

/// The refuting shape: a survivor on every nursery page. This is the
/// observation that closes `proposal-d-real-young-space.md` for the cost
/// of one counter — a page-backed young generation can only release a page
/// that holds none, and here there are none such.
#[test]
fn a_survivor_on_every_page_refutes_the_young_space() {
    let g = nursery(4, 10);
    let census = ZNurserySurvivorCensus::new();
    for _ in 0..ZNurserySurvivorCensus::MIN_CYCLES {
        let survivors: Vec<(usize, usize)> =
            (0..10).map(|i| (g.floor + i * 2 * MIB + 64, 48)).collect();
        census.observe_cycle(g, survivors);
    }
    let report = census.report();
    assert_eq!(report.pages, 30);
    assert_eq!(report.pages_with_survivor, 30);
    assert_eq!(report.survivor_page_percent(), 100);
    assert_eq!(report.verdict(), ZYoungSpaceVerdict::Refuted);
}

/// The supporting shape: survivors clustered on a minority of pages, so
/// the rest could have been released without being visited.
#[test]
fn survivors_clustered_on_a_few_pages_support_the_young_space() {
    let g = nursery(0, 10);
    let census = ZNurserySurvivorCensus::new();
    for _ in 0..ZNurserySurvivorCensus::MIN_CYCLES {
        // Three pages of ten hold every survivor.
        let survivors: Vec<(usize, usize)> = (0..30)
            .map(|i| (g.floor + (i % 3) * 2 * MIB + 8 * i, 32))
            .collect();
        census.observe_cycle(g, survivors);
    }
    let report = census.report();
    assert_eq!(report.pages_with_survivor, 9);
    assert_eq!(report.survivor_page_percent(), 30);
    assert_eq!(report.verdict(), ZYoungSpaceVerdict::Supported);
}

/// Objects below the floor are OLD, not nursery survivors, and must not be
/// counted — counting them would bias the census toward `Refuted` on
/// exactly the heaps where the floor has stopped moving, which is the
/// documented inert state of the current nursery.
#[test]
fn objects_below_the_floor_are_not_nursery_survivors() {
    let g = nursery(4, 4);
    let census = ZNurserySurvivorCensus::new();
    let below: Vec<(usize, usize)> = (0..4).map(|i| (g.base + i * 2 * MIB, 64)).collect();
    for _ in 0..ZNurserySurvivorCensus::MIN_CYCLES {
        census.observe_cycle(g, below.clone());
    }
    let report = census.report();
    assert_eq!(report.cycles, ZNurserySurvivorCensus::MIN_CYCLES);
    assert_eq!(report.pages, 12);
    assert_eq!(report.pages_with_survivor, 0);
    assert_eq!(report.survivor_bytes, 0);
    assert_eq!(report.verdict(), ZYoungSpaceVerdict::Supported);
}

/// An empty nursery is not evidence, and is not counted as a cycle:
/// counting it would drag the percentage toward zero, i.e. toward
/// `Supported`, which is the one direction an instrument must never drift
/// on its own.
#[test]
fn an_empty_nursery_is_not_evidence() {
    let census = ZNurserySurvivorCensus::new();
    let empty = ZNurseryGeometry {
        base: 0x1000_0000,
        page_bytes: 2 * MIB,
        floor: 0x1000_0000,
        end: 0x1000_0000,
    };
    for _ in 0..10 {
        census.observe_cycle(empty, Vec::new());
    }
    let report = census.report();
    assert_eq!(report.cycles, 0);
    assert_eq!(report.pages, 0);
    assert_eq!(report.verdict(), ZYoungSpaceVerdict::NotEnoughData);
}

/// Below `MIN_CYCLES` the census refuses to answer, however clean the
/// numbers look: the first young cycle after a whole-heap collection sees
/// a nursery that has only just started filling.
#[test]
fn one_cycle_is_never_enough_to_decide() {
    let g = nursery(0, 8);
    let census = ZNurserySurvivorCensus::new();
    census.observe_cycle(g, [(g.floor + 16, 24)]);
    assert_eq!(census.report().verdict(), ZYoungSpaceVerdict::NotEnoughData);
}
