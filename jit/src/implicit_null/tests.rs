use super::*;

/// A table with its own arrays, its own load factor and its own counters.
///
/// The process-global statics are shared by every test in the binary, so a
/// test that fills the table to prove the load factor would break every
/// test that ran after it. Everything that needs to see the table *full*,
/// or needs collisions without a search, uses one of these instead — and
/// runs the same [`Table`] methods the public functions run, because the
/// public functions are two-line wrappers over them.
struct OwnedTable {
    fault_pc: Vec<AtomicUsize>,
    recover_pc: Vec<AtomicUsize>,
    chain_next: Vec<AtomicU32>,
    bucket_head: Vec<AtomicU32>,
    chain_lock: Vec<parking_lot::Mutex<()>>,
    occupied: AtomicUsize,
    counters: [AtomicUsize; 5],
}

impl OwnedTable {
    fn new(cap: usize, buckets: usize) -> Self {
        assert!(cap.is_power_of_two() && buckets.is_power_of_two());
        let atomics = |n: usize| (0..n).map(|_| AtomicUsize::new(0)).collect::<Vec<_>>();
        let atomics32 = |n: usize| (0..n).map(|_| AtomicU32::new(0)).collect::<Vec<_>>();
        Self {
            fault_pc: atomics(cap),
            recover_pc: atomics(cap),
            chain_next: atomics32(cap),
            bucket_head: atomics32(buckets),
            // One stripe per bucket in a test table: the striping exists to
            // stop compile threads serialising on each other, and a test
            // has one thread.
            chain_lock: (0..buckets)
                .map(|_| parking_lot::Mutex::new(()))
                .collect::<Vec<_>>(),
            occupied: AtomicUsize::new(0),
            counters: [
                AtomicUsize::new(0),
                AtomicUsize::new(0),
                AtomicUsize::new(0),
                AtomicUsize::new(0),
                AtomicUsize::new(0),
            ],
        }
    }

    fn view(&self) -> Table<'_> {
        Table {
            fault_pc: &self.fault_pc,
            recover_pc: &self.recover_pc,
            chain_next: &self.chain_next,
            bucket_head: &self.bucket_head,
            chain_lock: &self.chain_lock,
            occupied: &self.occupied,
            counters: &self.counters,
        }
    }

    fn declined(&self) -> usize {
        self.counters[C_DECLINED].load(Ordering::Relaxed)
    }

    fn retired(&self) -> usize {
        self.counters[C_RETIRED].load(Ordering::Relaxed)
    }
}

/// `n` addresses that all hash to the same slot of a table of `cap` slots,
/// starting at `start` and stepping by one instruction's worth.
///
/// Collisions have to be constructed rather than assumed: the whole point
/// of [`mix`] is that nearby code addresses do NOT collide, so a test that
/// wants a probe run must go and find one.
fn colliding(start: usize, cap: usize, n: usize) -> Vec<usize> {
    let mask = cap - 1;
    let want = mix(start) & mask;
    let mut out = vec![start];
    let mut pc = start;
    let limit = start + (cap * n * 4096).max(1 << 20);
    while out.len() < n {
        pc += 0x10;
        assert!(
            pc < limit,
            "no {n} colliding addresses within {} bytes of {start:#x} -- the \
             hash changed and this helper's search budget has to change with it",
            limit - start
        );
        if mix(pc) & mask == want {
            out.push(pc);
        }
    }
    out
}

/// Hazard 4: the flag alone must not elide a check. `active` also needs a
/// recovering fault handler, which only the VM's installers note.
#[test]
fn elision_needs_a_recovering_fault_handler_as_well_as_the_flag() {
    if !enabled() {
        assert!(
            !active(),
            "the kill switch turns elision off whatever else holds"
        );
        return;
    }
    // Process-wide: another test may already have stood in for the
    // installer, in which case only the second half can be checked.
    if !fault_handler_installed() {
        assert!(!active(), "no handler installed, so no elision");
    }
    note_fault_handler_installed();
    assert!(fault_handler_installed());
    assert!(active(), "flag on and a handler installed: elide");
}

/// A registered site round-trips, and only for a null-page address.
#[test]
fn a_registered_site_recovers_only_inside_the_null_page() {
    let fault = 0x4000_0000_1000usize;
    let recover = 0x4000_0000_2000usize;
    assert!(register(fault, recover));

    assert_eq!(recover_at(fault, 0), Some(recover));
    assert_eq!(recover_at(fault, 15), Some(recover));
    assert_eq!(
        recover_at(fault, NULL_PAGE_LIMIT),
        None,
        "an address outside the null page is a wild pointer, not a null \
         receiver -- resuming from one would convert a diagnosable crash \
         into arbitrary control flow"
    );
    assert_eq!(
        recover_at(fault + 1, 0),
        None,
        "the PC must match EXACTLY; being inside some compiled method is \
         not enough"
    );
    unregister_range(fault, 1);
}

/// Retiring a range makes its PCs unrecoverable — the property that keeps
/// a freed-and-reused buffer from resuming at a stale address.
#[test]
fn a_retired_range_stops_recovering_and_its_slot_is_never_reused() {
    let base = 0x5000_0000_0000usize;
    assert!(register(base + 0x10, base + 0x80));
    assert!(register(base + 0x20, base + 0x90));
    assert_eq!(recover_at(base + 0x10, 0), Some(base + 0x80));

    let before = OCCUPIED.load(Ordering::Relaxed);
    unregister_range(base, 0x100);
    assert_eq!(recover_at(base + 0x10, 0), None);
    assert_eq!(recover_at(base + 0x20, 0), None);

    // The slots are TOMBSTONED, not recycled. Putting a live key back into
    // one would let a reader that already matched `fault_pc` read a
    // `recover_pc` a concurrent re-registration had overwritten, so a fresh
    // registration must claim a slot that has never been claimed before.
    assert!(register(base + 0x30, base + 0xA0));
    assert!(
        OCCUPIED.load(Ordering::Relaxed) > before,
        "registration after a retirement must claim a FRESH slot"
    );
    unregister_range(base, 0x100);
}

/// [`stale_duplicates_retired`] and [`unlinked_chain_nodes`] through the
/// PUBLIC entry points and the process-global table -- the two counters
/// this round found had no caller anywhere in the workspace (see
/// `docs/feature-designs/jit-ir-optimizations.md`).
/// The module's own doc calls the first "should read zero forever" and the
/// second the signal that the chain splice (2026-09-16) has not
/// regressed; this at least keeps both call-reachable and moving in the
/// direction their contract promises, even though the still-open gap is
/// that nothing outside this test module reads them in production.
///
/// Deltas rather than absolutes throughout: both counters are on the
/// process-global table every test in this binary shares, so only
/// "moved in the direction this operation implies" is safe to assert.
#[test]
fn stale_duplicates_retired_and_unlinked_chain_nodes_are_reachable_and_move_correctly() {
    // A live duplicate registration (same key, different recovery
    // address) is the exact condition `STALE_DUPLICATES_RETIRED` exists to
    // count -- see `Table::insert`'s duplicate arm. The address is unique
    // to this test, so no concurrently-running test can also retire it.
    let pc = 0x5A00_0000_0010usize;
    assert!(register(pc, 0x5B00_0000_0001));
    let stale_before = stale_duplicates_retired();
    assert!(
        register(pc, 0x5B00_0000_0002),
        "a live key registered again must still succeed, by retiring the \
         stale entry and claiming a fresh slot"
    );
    // `>` rather than `== + 1`: `STALE_DUPLICATES_RETIRED` is one counter
    // shared by the whole test binary, so a `>=`/`>` bound is the only one
    // safe against another test's stale duplicate landing between the two
    // reads. What matters is that the public function moved at all.
    assert!(
        stale_duplicates_retired() > stale_before,
        "the public counter must see the stale duplicate this test just \
         created"
    );
    assert_eq!(recover(pc, 0), Some(0x5B00_0000_0002));
    unregister_range(pc, 1);

    // Retiring a page with a live entry on it unlinks the node once the
    // entry is gone -- `unlinked_chain_nodes` must not stay flat forever.
    let page = 0x5C00_0000_0000usize;
    let unlinked_before = unlinked_chain_nodes();
    assert!(register(page + 0x10, 0x5D00_0000_0000));
    unregister_range(page, 1 << PAGE_SHIFT);
    assert!(
        unlinked_chain_nodes() > unlinked_before,
        "retiring the only entry on a page's chain must splice it out, \
         which is exactly what this counter exists to make visible"
    );
}

/// A zero on either side is refused rather than stored, because `0` is the
/// sentinel this table uses for "never claimed".
#[test]
fn a_zero_pc_is_refused_because_zero_is_the_empty_sentinel() {
    assert!(!register(0, 0x1234));
    assert!(!register(0x1234, 0));
}

/// The two non-zero sentinels are refused too, and — the reason it matters
/// — cannot be used as a lookup key to match a reserved or retired slot.
#[test]
fn the_sentinel_values_are_refused_as_keys() {
    assert!(!register(RESERVED, 0x1234));
    assert!(!register(TOMBSTONE, 0x1234));
    assert_eq!(recover(RESERVED, 0), None);
    assert_eq!(recover(TOMBSTONE, 0), None);
}

/// A long probe run: many sites that all hash to one slot, each of which
/// must still find its OWN recovery address.
///
/// This is the property the old linear array had for free and that linear
/// probing has to earn. A bug in the probe walk — an index that does not
/// advance, a comparison against the slot rather than the key — shows up
/// here as one site recovering another site's target, which in production
/// is a jump into the wrong method's slow path.
#[test]
fn colliding_sites_each_recover_their_own_target() {
    let t = OwnedTable::new(64, 16);
    let v = t.view();
    let keys = colliding(0x4100_0000_0000, 64, 12);
    for (n, &k) in keys.iter().enumerate() {
        assert!(v.insert(k, 0x7000_0000_0000 + n), "insert {n} of 12");
    }
    for (n, &k) in keys.iter().enumerate() {
        assert_eq!(
            v.lookup(k),
            Some(0x7000_0000_0000 + n),
            "site {n} recovered somebody else's address"
        );
    }
    assert_eq!(t.declined(), 0, "12 sites at 3/4 of 64 must all fit");
}

/// A retired slot is skipped by insertion, not recycled by it.
///
/// Register A, retire A, then register a B chosen to hash to A's slot. B
/// must land somewhere else, A must stay unrecoverable, and B must recover
/// its own address — which is the whole tombstone argument in one test.
#[test]
fn a_tombstoned_slot_is_skipped_by_the_next_registration() {
    let t = OwnedTable::new(64, 16);
    let v = t.view();
    let keys = colliding(0x4200_0000_0000, 64, 2);
    let (a, b) = (keys[0], keys[1]);

    assert!(v.insert(a, 0x7100_0000_0000));
    let slot_a = mix(a) & (64 - 1);
    assert_eq!(v.fault_pc[slot_a].load(Ordering::Relaxed), a);

    v.retire_range(a, a + 1);
    assert_eq!(t.retired(), 1);
    assert_eq!(
        v.fault_pc[slot_a].load(Ordering::Relaxed),
        TOMBSTONE,
        "retirement must leave a TOMBSTONE, not an empty slot -- an empty \
         slot would both end probes early and be claimable again"
    );
    assert_eq!(v.lookup(a), None);

    assert!(v.insert(b, 0x7100_0000_1000));
    assert_eq!(
        v.fault_pc[slot_a].load(Ordering::Relaxed),
        TOMBSTONE,
        "the new registration must NOT have taken the retired slot"
    );
    assert_eq!(v.lookup(a), None, "a retired site stays retired");
    assert_eq!(v.lookup(b), Some(0x7100_0000_1000));
}

/// The length of bucket `b`'s retirement chain.
fn chain_len(v: &Table<'_>, b: usize) -> usize {
    let mut n = 0usize;
    let mut cur = v.bucket_head[b].load(Ordering::Acquire);
    while cur != 0 && n <= v.cap() {
        n += 1;
        cur = v.chain_next[(cur - 1) as usize].load(Ordering::Acquire);
    }
    n
}

/// Retiring a page SPLICES its dead nodes out; it does not merely
/// tombstone them.
///
/// # The failure this pins
///
/// Slots are never reused, so before the splice existed a page that was
/// compiled, dropped and recompiled grew a chain of tombstoned nodes that
/// every later retirement of that page walked in full. The chain is bounded
/// only by `CAP`, so the worst case is the whole table — which is exactly
/// the linear sweep the chains were introduced to replace. A run with heavy
/// same-address recompilation would have degraded back to O(table) per
/// code-buffer free with no counter saying so.
#[test]
fn retiring_a_page_unlinks_its_dead_nodes() {
    let t = OwnedTable::new(64, 16);
    let v = t.view();
    // Three sites on one code page, so one bucket, then retire the page.
    let page = 0x4400_0000_0000usize;
    let sites = [page + 0x10, page + 0x40, page + 0x80];
    for (k, site) in sites.iter().enumerate() {
        assert!(v.insert(*site, 0x7300_0000_0000 + k));
    }
    let b = v.bucket_of(page);
    assert_eq!(chain_len(&v, b), 3, "all three are on one page's chain");

    v.retire_range(page, page + (1 << PAGE_SHIFT));
    assert_eq!(
        chain_len(&v, b),
        0,
        "every node was dead, so the chain is empty — not three tombstones"
    );
    assert_eq!(
        t.counters[C_UNLINKED].load(Ordering::Relaxed),
        3,
        "and the splice is counted, so a chain that starts growing again \
         is visible rather than silent"
    );
    for site in sites {
        assert_eq!(v.lookup(site), None);
    }
}

/// A live node between two dead ones survives the splice, and the chain
/// stays connected through it.
///
/// The predecessor bookkeeping is the whole content of the splice: after
/// removing a node, the next node's predecessor is still the node BEFORE
/// the removed one, so `prev` must not advance. Getting that wrong either
/// drops the tail (losing live sites, which turns a recoverable NPE into a
/// crash) or leaves a cycle.
#[test]
fn a_live_node_survives_a_splice_on_both_sides_of_it() {
    let t = OwnedTable::new(64, 16);
    let v = t.view();
    // Two pages that share a bucket would confuse the test, so use one
    // page and retire two disjoint sub-ranges of it.
    let page = 0x4500_0000_0000usize;
    let (dead_a, live, dead_b) = (page + 0x10, page + 0x40, page + 0x80);
    assert!(v.insert(dead_a, 0x7400_0000_0001));
    assert!(v.insert(live, 0x7400_0000_0002));
    assert!(v.insert(dead_b, 0x7400_0000_0003));
    let b = v.bucket_of(page);
    assert_eq!(chain_len(&v, b), 3);

    // Retire the two dead ones in one walk, leaving the middle one live.
    v.retire_range(dead_a, dead_a + 1);
    v.retire_range(dead_b, dead_b + 1);

    assert_eq!(
        chain_len(&v, b),
        1,
        "both dead nodes are spliced out and the live one remains"
    );
    assert_eq!(
        v.lookup(live),
        Some(0x7400_0000_0002),
        "the survivor is still findable — the splice must not drop the tail"
    );
    assert_eq!(v.lookup(dead_a), None);
    assert_eq!(v.lookup(dead_b), None);
}

/// Recompiling the same page repeatedly does not grow its chain.
///
/// The end-to-end form of the property: `CAP` registrations against one
/// page, retired between each, and the chain never holds more than the
/// sites currently live. Before the splice this chain would have been as
/// long as the number of registrations ever made.
#[test]
fn repeated_recompilation_of_one_page_does_not_grow_its_chain() {
    let t = OwnedTable::new(64, 16);
    let v = t.view();
    let page = 0x4600_0000_0000usize;
    let b = v.bucket_of(page);
    // Bounded well below the load factor so no round can decline: the
    // point is the chain length, not exhaustion.
    for round in 0..8usize {
        let site = page + 0x10 + round;
        assert!(v.insert(site, 0x7500_0000_0000 + round), "round {round}");
        assert_eq!(chain_len(&v, b), 1, "round {round}: one live site");
        v.retire_range(site, site + 1);
        assert_eq!(chain_len(&v, b), 0, "round {round}: spliced back to empty");
    }
}

/// A probe must not stop at a tombstone.
///
/// A and B collide, so B sits past A in the probe run. Retire A and B must
/// still be found. If [`Table::lookup`] treated a tombstone as the end of
/// the run — the single easiest mistake in an open-addressed table — B
/// would vanish, and in production that is an NPE turning into a process
/// crash for no visible reason.
#[test]
fn a_probe_does_not_stop_at_a_tombstone() {
    let t = OwnedTable::new(64, 16);
    let v = t.view();
    let keys = colliding(0x4300_0000_0000, 64, 2);
    let (a, b) = (keys[0], keys[1]);
    assert!(v.insert(a, 0x7200_0000_0000));
    assert!(v.insert(b, 0x7200_0000_1000));

    v.retire_range(a, a + 1);
    assert_eq!(v.lookup(a), None);
    assert_eq!(
        v.lookup(b),
        Some(0x7200_0000_1000),
        "B hashes to A's slot and lives past it; a probe that stopped at \
         A's tombstone would lose B silently"
    );
}

/// The same property through the public entry points and the global table,
/// so that the wrappers are covered too and not just [`Table`].
#[test]
fn the_public_path_skips_a_tombstone_and_still_finds_the_next_site() {
    let keys = colliding(0x6000_0000_0000, CAP, 2);
    let (a, b) = (keys[0], keys[1]);
    assert!(register(a, a + 0x40));
    assert!(register(b, b + 0x40));
    assert_eq!(recover(a, 0), Some(a + 0x40));
    assert_eq!(recover(b, 0), Some(b + 0x40));

    unregister_range(a, 1);
    assert_eq!(recover(a, 0), None, "the retired site is gone");
    assert_eq!(
        recover(b, 0),
        Some(b + 0x40),
        "the site behind the tombstone is not"
    );
    unregister_range(b, 1);
    assert_eq!(recover(b, 0), None);
}

/// Past the load factor, registration declines and says so.
///
/// Run on a private table on purpose: filling the process-global one to
/// prove this would leave it exhausted for every other test in the binary,
/// which is the same failure mode as the production exhaustion this test
/// is about.
#[test]
fn the_table_declines_past_its_load_factor_and_counts_it() {
    let t = OwnedTable::new(64, 16);
    let v = t.view();
    assert_eq!(v.load_cap(), 48, "3/4 of 64");

    let mut pc = 0x4400_0000_0000usize;
    for n in 0..48 {
        assert!(v.insert(pc, 0x7300_0000_0000 + n), "site {n} of 48 fits");
        pc += 0x40;
    }
    assert_eq!(t.declined(), 0, "nothing below the cap may decline");

    let declined_before = t.declined();
    assert!(
        !v.insert(pc, 0x7300_0000_9999),
        "the 49th site is past 3/4 load and must be DECLINED, not squeezed \
         in -- an open-addressed table run to capacity has probe runs as \
         long as the scan this design replaced"
    );
    assert_eq!(
        t.declined(),
        declined_before + 1,
        "a decline that is not counted is a feature switching itself off \
         invisibly, which is exactly what `counts` reports DECLINED for"
    );
    assert_eq!(
        v.occupied.load(Ordering::Relaxed),
        48,
        "a declined registration must give its reservation back"
    );
    // And the decline is stable: the table does not spin or eventually
    // relent.
    assert!(!v.insert(pc, 0x7300_0000_9999));
    assert_eq!(t.declined(), declined_before + 2);
}

/// Hazard 5: elision stops `headroom_of(cap)` slots BEFORE the load cap,
/// so the compiles already past their elision decision when the gate
/// closes still find a slot, instead of declining after their unchecked
/// load is emitted.
#[test]
fn elision_stops_before_the_table_can_decline() {
    let t = OwnedTable::new(64, 16);
    let v = t.view();
    // Load cap 48, headroom 8: the gate closes at 40 claimed slots.
    assert_eq!(v.load_cap(), 48);
    assert_eq!(headroom_of(64), 8);
    let mut pc = 0x4B00_0000_0000usize;
    for n in 0..40usize {
        assert!(v.has_headroom(), "slot {n}: there is still room to elide");
        assert!(v.insert(pc, 0x7800_0000_0000 + n));
        pc += 0x40;
    }
    assert!(
        !v.has_headroom(),
        "40 of 48 claimed: new compiles must take explicit checks"
    );
    // The in-flight registrations the headroom exists for all fit.
    for n in 40..48usize {
        assert!(v.insert(pc, 0x7800_0000_0000 + n), "in-flight site {n}");
        pc += 0x40;
    }
    assert_eq!(t.declined(), 0);
}

/// A live key registered again with a DIFFERENT recovery address is a
/// stale entry from a buffer that was freed without retiring its sites.
/// Declining left that entry to answer for the new code's unchecked load
/// — a jump into the dead buffer's recovery address. It is retired and
/// the new site registered instead. The identical mapping, by contrast, is
/// accepted in place without touching the slot.
#[test]
fn a_stale_duplicate_is_retired_not_left_to_answer_for_the_new_code() {
    let t = OwnedTable::new(64, 16);
    let v = t.view();
    let pc = 0x4C00_0000_0010usize;
    assert!(v.insert(pc, 0x7900_0000_0001));
    assert!(
        v.insert(pc, 0x7900_0000_0002),
        "the new site must be registered"
    );
    assert_eq!(
        v.lookup(pc),
        Some(0x7900_0000_0002),
        "the fault must resume at THIS method's recovery address, never \
         the freed buffer's"
    );
    assert_eq!(t.retired(), 1);
    assert_eq!(t.declined(), 0);

    let before = v.occupied.load(Ordering::Relaxed);
    assert!(v.insert(pc, 0x7900_0000_0002));
    assert_eq!(
        v.occupied.load(Ordering::Relaxed),
        before,
        "a repeated identical registration claims nothing"
    );
    assert_eq!(v.lookup(pc), Some(0x7900_0000_0002));
}

/// Tombstones count against the load factor, because they cost a probe.
#[test]
fn retired_entries_still_occupy_their_share_of_the_load_factor() {
    let t = OwnedTable::new(64, 16);
    let v = t.view();
    let base = 0x4500_0000_0000usize;
    for n in 0..48 {
        assert!(v.insert(base + n * 0x40, 0x7400_0000_0000 + n));
    }
    v.retire_range(base, base + 48 * 0x40);
    assert_eq!(t.retired(), 48);
    assert!(
        !v.insert(0x4600_0000_0000, 0x7400_0000_9999),
        "retiring does not give capacity back -- a tombstone is not a free \
         slot, and pretending it is would be the slot reuse hazard 2 rules \
         out"
    );
}

/// Retirement walks the chains of the pages a range covers, so a range on
/// an unrelated page must not disturb it.
#[test]
fn retirement_touches_only_the_range_it_was_given() {
    let t = OwnedTable::new(64, 16);
    let v = t.view();
    let keep = 0x4700_0000_0000usize;
    let doomed = 0x4700_0100_0000usize; // 16 MiB away: a different page.
    assert!(v.insert(keep, 0x7500_0000_0000));
    assert!(v.insert(doomed, 0x7500_0000_1000));

    v.retire_range(doomed, doomed + 0x1000);
    assert_eq!(v.lookup(doomed), None);
    assert_eq!(
        v.lookup(keep),
        Some(0x7500_0000_0000),
        "a page-chain walk must not retire entries outside the range"
    );
}

/// A range too wide for the chain walk falls back to a sweep, and the
/// sweep has to retire exactly the same entries.
#[test]
fn an_absurdly_wide_range_falls_back_to_a_sweep_with_the_same_result() {
    let t = OwnedTable::new(64, 16);
    let v = t.view();
    let base = 0x4800_0000_0000usize;
    let inside = base + 0x10;
    let outside = base + ((MAX_RANGE_PAGES + 8) << PAGE_SHIFT);
    assert!(v.insert(inside, 0x7600_0000_0000));
    assert!(v.insert(outside, 0x7600_0000_1000));

    let span = (MAX_RANGE_PAGES + 4) << PAGE_SHIFT;
    assert!(
        (span >> PAGE_SHIFT) > MAX_RANGE_PAGES,
        "this test is only meaningful if it takes the sweep path"
    );
    v.retire_range(base, base + span);
    assert_eq!(v.lookup(inside), None);
    assert_eq!(
        v.lookup(outside),
        Some(0x7600_0000_1000),
        "the sweep must respect the range bounds just as the walk does"
    );
}

/// A zero-length range is a no-op rather than a sweep of everything.
#[test]
fn a_zero_length_range_retires_nothing() {
    let t = OwnedTable::new(64, 16);
    let v = t.view();
    let pc = 0x4900_0000_0000usize;
    assert!(v.insert(pc, 0x7700_0000_0000));
    v.retire_range(pc, pc);
    assert_eq!(v.lookup(pc), Some(0x7700_0000_0000));
    assert_eq!(t.retired(), 0);
}

/// The runtime half of the entry-is-base invariant.
#[test]
fn note_registration_base_accepts_an_entry_that_is_the_base() {
    assert!(note_registration_base(0x4A00_0000_0000, 0x4A00_0000_0000));
    // The mismatching arm trips a `debug_assert`, which is the point of it,
    // so it is deliberately not exercised here: a test that asserted the
    // `false` return would have to be `#[cfg(not(debug_assertions))]` and
    // would therefore never run where it matters. The compile-time test
    // below is the one that guards the invariant.
    // Through the PUBLIC getter, not `BASE_MISMATCHES.load(..)` as this
    // assertion used to be. The static and the getter are trivially the
    // same read, so the assertion is unchanged -- but the counter's whole
    // problem, until round 10, was that it had no reader that was not a
    // direct static access inside this file. Asserting through
    // `base_mismatches()` keeps the public surface exercised by the same
    // test that pins the value, so a getter deleted as "unused" fails here
    // rather than silently re-orphaning the counter.
    assert_eq!(
        base_mismatches(),
        0,
        "nothing in this binary should be registering with a moved entry"
    );
}

/// The compile-time half: `x64::driver` must still put the prologue at
/// offset 0.
///
/// `register` files a site under `entry + fault_off`, and two separate
/// mechanisms downstream assume `entry` is the buffer base: retirement,
/// and the OSR-trampoline purge in `CompiledMethod::drop`.
///
/// The retirement half has WEAKENED since this test was written, and
/// saying so here is better than letting the test keep asserting on an
/// argument it no longer has. Retirement moved to `ExecutableBuffer::drop`
/// on 2026-09-21 (`33957575f`) and now sweeps `[ptr, ptr + capacity)` —
/// the whole mapping — so a moved prologue would leave its sites inside
/// the retired range anyway. The purge half has not weakened; it reads
/// `entry` directly. And `note_registration_base` compares against the
/// MAPPING base (`cm.code_bytes().as_ptr()`), so a moved entry now refuses
/// every implicit-null compile outright.
///
/// None of that is visible at the call site, and none of it would fail a
/// functional test; a purge that misses would fail as an unreproducible
/// jump into the wrong method, months later. So the invariant is read out
/// of the source and asserted here. Deleting this test because the
/// retirement half stopped needing it is the edit this paragraph exists to
/// argue against.
///
/// The needles are assembled at runtime, in the house style of
/// `jit/tests/single_bytecode_decoder_ratchet.rs`, so that neither this
/// file nor a future ratchet scanning `jit/src` can match on the literal.
#[test]
fn the_method_entry_is_still_the_buffer_base() {
    const DRIVER: &str = include_str!("../x64/driver.rs");
    let decl = format!("let {}_{} ", "entry", "offset");
    let zero = format!("{}= {};", decl, 0);

    let decls: Vec<&str> = DRIVER
        .lines()
        .map(str::trim)
        .filter(|line| line.starts_with(decl.as_str()))
        .collect();

    assert_eq!(
        decls.len(),
        1,
        "expected exactly one `{}` declaration in x64/driver.rs, found {}: \
         {decls:?} -- this test reads that declaration to prove the method \
         entry is still the buffer base, and it cannot do that if it cannot \
         find it",
        decl.trim(),
        decls.len()
    );
    assert!(
        decls[0].starts_with(zero.as_str()),
        "x64/driver.rs now says `{}`, not `{}`.\n\n\
         The method entry is no longer the buffer base, and \
         `implicit_null::register` keys every site off `entry + fault_off`.\n\n\
         What this no longer breaks, so that nobody chases it: implicit-null \
         RETIREMENT. It moved to `ExecutableBuffer::drop` (2026-09-21, \
         `33957575f`) and sweeps the whole mapping, `[ptr, ptr + capacity)`, \
         so sites at a shifted entry are still inside the retired range. The \
         pre-move version of this message said they were not; that was true \
         of `[entry, entry + pos)` and is no longer the mechanism.\n\n\
         What this DOES break: the OSR-trampoline purge in \
         `CompiledMethod::drop`, which keys off `entry`; and \
         `note_registration_base`, which is handed the mapping base and \
         would now refuse EVERY compile carrying an implicit-null site \
         (`implicit-null-entry-not-base`) -- turning the feature off \
         wholesale rather than turning it wrong. That is the fail-closed \
         direction, and it is still not a state to reach by accident.\n\n\
         If the prologue really must move, move `note_registration_base`'s \
         contract with it -- assert the entry lies INSIDE the mapping \
         rather than AT its base -- and audit the OSR purge in the same \
         change.",
        decls[0],
        zero
    );
}

fn recover_at(pc: usize, addr: usize) -> Option<usize> {
    recover(pc, addr)
}
