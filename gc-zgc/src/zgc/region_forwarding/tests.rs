use super::*;
use std::sync::{Arc, Barrier};

const KIB: usize = 1024;
/// A synthetic, 8-aligned arena base well above zero; nothing is mapped
/// there -- every test copies through its own word array.
const BASE: usize = 0x4000_0000;

fn grid(regions: usize, region_bytes: usize) -> ZRegionGrid {
    ZRegionGrid::new(BASE, BASE + regions * region_bytes, region_bytes).unwrap()
}

#[test]
fn region_size_is_the_logical_page_size() {
    assert_eq!(
        ZRELOC_REGION_BYTES,
        crate::zgc::ZgcRealHeap::Z_LOGICAL_PAGE_BYTES
    );
    assert!(
        ZRegionGrid::new(BASE, BASE + 4 * ZRELOC_REGION_BYTES, ZRELOC_REGION_BYTES).is_some()
    );
}

#[test]
fn grid_rejects_unusable_geometry() {
    assert!(ZRegionGrid::new(BASE, BASE + 100, 3 * KIB).is_none());
    assert!(ZRegionGrid::new(BASE, BASE + 100, 8).is_none());
    assert!(ZRegionGrid::new(BASE, BASE + 100, 64 * 1024 * KIB).is_none());
    assert!(ZRegionGrid::new(BASE + 4, BASE + 100, 64 * KIB).is_none());
    assert!(ZRegionGrid::new(BASE, BASE - 8, 64 * KIB).is_none());
    // 32 MiB is the largest region the from-offset field keys.
    assert!(ZRegionGrid::new(BASE, BASE + 100, 32 * 1024 * KIB).is_some());
}

#[test]
fn grid_maps_addresses_and_used_extent() {
    let g = ZRegionGrid::new(BASE, BASE + 2 * 64 * KIB + 100, 64 * KIB).unwrap();
    assert_eq!(g.region_count(), 3);
    assert_eq!(g.region_of(BASE), Some(0));
    assert_eq!(g.region_of(BASE + 64 * KIB - 1), Some(0));
    assert_eq!(g.region_of(BASE + 64 * KIB), Some(1));
    assert_eq!(g.region_of(BASE - 1), None);
    assert_eq!(g.region_of(g.end()), None);
    assert_eq!(g.used_in(0), 64 * KIB);
    assert_eq!(g.used_in(2), 100);
    assert_eq!(g.used_in(3), 0);
}

#[test]
fn liveness_books_objects_to_their_start_region_and_records_straddles() {
    let g = grid(4, 64 * KIB);
    let objs = [
        (BASE, 32),
        (BASE + 64, 48),
        // Starts in region 1, reaches 16 bytes into region 2.
        (BASE + 2 * 64 * KIB - 16, 32),
        // Outside the grid (the high end).
        (BASE + 10 * 64 * KIB, 64),
        // Corrupt size.
        (BASE + 3 * 64 * KIB, 8),
    ];
    let l = region_liveness(&g, objs);
    assert_eq!(l.live_bytes, vec![80, 32, 0, 0]);
    assert_eq!(l.live_objects, vec![2, 1, 0, 0]);
    assert_eq!(l.straddle_in_bytes, vec![0, 0, 16, 0]);
    assert_eq!(l.outside, 1);
    assert_eq!(l.unsizable, 1);
}

#[test]
fn a_multi_region_object_straddles_every_region_it_covers() {
    let g = grid(4, 64 * KIB);
    // Starts 8 bytes before region 1 and ends exactly at region 3's base:
    // regions 1 and 2 are covered whole, region 3 not at all.
    let l = region_liveness(&g, [(BASE + 64 * KIB - 8, 2 * 64 * KIB + 8)]);
    assert_eq!(l.straddle_in_bytes, vec![0, 64 * KIB, 64 * KIB, 0]);
    let l = region_liveness(&g, [(BASE + 64 * KIB - 8, 2 * 64 * KIB + 16)]);
    assert_eq!(l.straddle_in_bytes, vec![0, 64 * KIB, 64 * KIB, 8]);
    assert_eq!(l.live_bytes[0], 2 * 64 * KIB + 16);
}

#[test]
fn selection_frees_empty_regions_keeps_dense_and_excluded_ones_and_picks_sparse() {
    let g = grid(6, 64 * KIB);
    let r = |i: usize| BASE + i * 64 * KIB;
    let objs = vec![
        // 0: dense (one 60 KiB object): occupancy 0.94, never selected.
        (r(0), 60 * KIB),
        // 1: sparse: 1 KiB live of 64 KiB.
        (r(1), KIB),
        // 2: empty (no live object).
        // 3: sparse but excluded by the caller (a raw holder).
        (r(3), KIB),
        // 4: tail of an object from 3 reaches in -> straddled.
        (r(4) - 16, 32),
        // 5: sparse.
        (r(5) + 128, 2 * KIB),
    ];
    let l = region_liveness(&g, objs);
    let sel = select_relocation_regions(&g, &l, &ZRelocationPolicy::default(), |i| i == 3);
    assert!(sel.trustworthy());
    assert_eq!(sel.empty, vec![2]);
    assert_eq!(sel.excluded, 1);
    assert_eq!(sel.straddled, 1);
    let mut ids = sel.set.page_ids();
    ids.sort_unstable();
    assert_eq!(ids, vec![1, 5]);
    // Most garbage per byte first: region 1 (63/64) before region 5 (62/64).
    assert_eq!(sel.set.pages()[0].page_id, 1);
    assert_eq!(sel.set.total_live_bytes(), 3 * KIB);
}

#[test]
fn selection_honours_the_policy_budget_and_reports_corrupt_sizes() {
    let g = grid(3, 64 * KIB);
    let r = |i: usize| BASE + i * 64 * KIB;
    let l = region_liveness(&g, [(r(0), KIB), (r(1), 2 * KIB), (r(2), 4), (r(2), KIB)]);
    let policy = ZRelocationPolicy {
        max_evacuation_bytes: KIB,
        ..ZRelocationPolicy::default()
    };
    let sel = select_relocation_regions(&g, &l, &policy, |_| false);
    assert_eq!(sel.set.page_ids(), vec![0]);
    assert!(!sel.trustworthy());
}

#[test]
fn encoding_round_trips_from_the_arena_base_and_refuses_below_it() {
    let g = grid(2, 64 * KIB);
    let f = ZRegionForwarding::new(&g, 1, 4, 64);
    // Offset 0 of the arena is a legal destination and must not encode to 0.
    let p = f.encode_to(BASE).unwrap();
    assert!(!p.is_null());
    assert_eq!(f.decode_to(p), BASE);
    assert_eq!(f.decode_to(f.encode_to(BASE + 4096).unwrap()), BASE + 4096);
    assert!(f.encode_to(BASE - 8).is_none());
    assert!(f.encode_to(BASE + 4).is_none());
    // A Linux-shaped arena base still encodes: the payload is relative.
    let hi =
        ZRegionGrid::new(0x7f00_0000_0000, 0x7f00_0000_0000 + 128 * KIB, 64 * KIB).unwrap();
    let fh = ZRegionForwarding::new(&hi, 0, 1, 32);
    let to = 0x7f00_0000_0000 + 64 * KIB + 8;
    assert_eq!(fh.decode_to(fh.encode_to(to).unwrap()), to);
}

/// A word-addressed fake heap: `copy` moves `size / 8` words.
struct FakeHeap {
    words: Vec<AtomicU64>,
    bump: AtomicUsize,
}

impl FakeHeap {
    fn new(bytes: usize, to_space_from: usize) -> Self {
        Self {
            words: (0..bytes / 8).map(|_| AtomicU64::new(0)).collect(),
            bump: AtomicUsize::new(BASE + to_space_from),
        }
    }
    fn alloc(&self, size: usize) -> Option<usize> {
        let at = self.bump.fetch_add(size, Ordering::Relaxed);
        (at + size <= BASE + self.words.len() * 8).then_some(at)
    }
    fn word(&self, addr: usize) -> &AtomicU64 {
        &self.words[(addr - BASE) / 8]
    }
    fn copy(&self, from: usize, to: usize, size: usize) {
        for k in 0..size / 8 {
            let v = self.word(from + k * 8).load(Ordering::Relaxed);
            self.word(to + k * 8).store(v, Ordering::Relaxed);
        }
    }
}

#[test]
fn first_copier_relocates_later_ones_are_forwarded_without_a_copy() {
    let g = grid(2, 64 * KIB);
    let heap = FakeHeap::new(2 * 64 * KIB, 64 * KIB);
    let f = ZRegionForwarding::new(&g, 0, 2, 64);
    let obj = BASE + 128;
    heap.word(obj).store(0xfeed, Ordering::Relaxed);
    let a = f
        .relocate_or_forward(obj, 32, |s| heap.alloc(s), |x, y, s| heap.copy(x, y, s))
        .unwrap();
    let ZRegionRelocated::Relocated(to) = a else {
        panic!("first copier must win: {a:?}")
    };
    assert_eq!(heap.word(to).load(Ordering::Relaxed), 0xfeed);
    let b = f
        .relocate_or_forward(
            obj,
            32,
            |_| panic!("no allocation on a hit"),
            |_, _, _| panic!("no copy on a hit"),
        )
        .unwrap();
    assert_eq!(b, ZRegionRelocated::Forwarded(to));
    assert_eq!(f.forwarded(obj), Some(to));
    assert_eq!(f.forwarded(obj + 32), None);
    assert_eq!(f.stats(), (1, 32, 0, 0));
    assert_eq!(f.in_flight(), 0);
}

#[test]
fn a_lost_install_returns_the_winner_and_names_the_abandoned_copy() {
    let g = grid(2, 64 * KIB);
    let heap = FakeHeap::new(2 * 64 * KIB, 64 * KIB);
    let f = ZRegionForwarding::new(&g, 0, 1, 32);
    let obj = BASE + 64;
    // Another copier installs between our allocation and our install.
    let mut theirs = 0usize;
    let r = f
        .relocate_or_forward(
            obj,
            32,
            |s| heap.alloc(s),
            |x, y, s| {
                heap.copy(x, y, s);
                let inner = f
                    .relocate_or_forward(
                        x,
                        s,
                        |s2| heap.alloc(s2),
                        |a, b, n| heap.copy(a, b, n),
                    )
                    .unwrap();
                theirs = inner.address();
            },
        )
        .unwrap();
    match r {
        ZRegionRelocated::LostRace { winner, abandoned } => {
            assert_eq!(winner, theirs);
            assert_ne!(abandoned, theirs);
        }
        other => panic!("expected a lost race: {other:?}"),
    }
    assert_eq!(f.forwarded(obj), Some(theirs));
    assert_eq!(f.stats(), (1, 32, 1, 32));
}

#[test]
fn bad_objects_and_failed_allocations_are_refused() {
    let g = grid(2, 64 * KIB);
    let f = ZRegionForwarding::new(&g, 1, 1, 32);
    let in_region = BASE + 64 * KIB + 64;
    let none = |_: usize| -> Option<usize> { None };
    let nocopy = |_: usize, _: usize, _: usize| {};
    // Outside the region, misaligned, too small.
    for (from, size) in [(BASE + 64, 32), (in_region + 4, 32), (in_region, 8)] {
        assert_eq!(
            f.relocate_or_forward(from, size, none, nocopy),
            Err(ZRegionRelocateError::BadObject)
        );
    }
    assert_eq!(
        f.relocate_or_forward(in_region, 32, none, nocopy),
        Err(ZRegionRelocateError::AllocFailed)
    );
    assert_eq!(
        f.relocate_or_forward(in_region, 32, |_| Some(BASE - 64), nocopy),
        Err(ZRegionRelocateError::Unencodable)
    );
    // Every refusal released its hold.
    assert_eq!(f.in_flight(), 0);
}

#[test]
fn seal_refuses_new_copiers_and_quiescence_waits_for_the_last_guard() {
    let g = grid(1, 64 * KIB);
    let heap = FakeHeap::new(2 * 64 * KIB, 64 * KIB);
    let f = ZRegionForwarding::new(&g, 0, 2, 64);
    let a = BASE + 64;
    let b = BASE + 128;
    let moved_a = f
        .relocate_or_forward(a, 32, |s| heap.alloc(s), |x, y, s| heap.copy(x, y, s))
        .unwrap()
        .address();
    let held = f.retain().expect("unsealed region admits a copier");
    assert_eq!(f.in_flight(), 1);
    assert_eq!(f.seal(), 1);
    assert!(f.is_sealed());
    assert!(!f.is_quiescent());
    assert!(f.retain().is_none());
    // A sealed region answers its entries and refuses to copy a new one.
    assert_eq!(
        f.relocate_or_forward(a, 32, |_| None, |_, _, _| {}),
        Ok(ZRegionRelocated::Forwarded(moved_a))
    );
    assert_eq!(
        f.relocate_or_forward(b, 32, |s| heap.alloc(s), |_, _, _| {}),
        Err(ZRegionRelocateError::SealedWithoutEntry)
    );
    drop(held);
    assert!(f.is_quiescent());
}

#[test]
fn a_panicking_copier_still_releases_its_hold() {
    let g = grid(1, 64 * KIB);
    let f = ZRegionForwarding::new(&g, 0, 1, 32);
    let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _ = f.relocate_or_forward(
            BASE + 64,
            32,
            |_| Some(BASE + 32 * KIB),
            |_, _, _| panic!("copy fault"),
        );
    }));
    assert!(r.is_err());
    assert_eq!(f.in_flight(), 0);
    assert_eq!(f.seal(), 0);
    assert!(f.is_quiescent());
}

#[test]
fn racing_copiers_agree_on_one_address_per_object() {
    const THREADS: usize = 8;
    const OBJECTS: usize = 256;
    const SIZE: usize = 32;
    let region = 64 * KIB;
    let g = grid(1, region);
    let heap = Arc::new(FakeHeap::new(region + THREADS * OBJECTS * SIZE, region));
    for i in 0..OBJECTS {
        heap.word(BASE + i * SIZE)
            .store(i as u64 + 1, Ordering::Relaxed);
    }
    let f = Arc::new(ZRegionForwarding::new(&g, 0, OBJECTS, OBJECTS * SIZE));
    let start = Arc::new(Barrier::new(THREADS));
    let handles: Vec<_> = (0..THREADS)
        .map(|t| {
            let (heap, f, start) = (Arc::clone(&heap), Arc::clone(&f), Arc::clone(&start));
            std::thread::spawn(move || {
                start.wait();
                let mut seen = vec![0usize; OBJECTS];
                let mut wins = 0usize;
                for k in 0..OBJECTS {
                    // Each thread walks the objects from a different start.
                    let i = (k + t * 31) % OBJECTS;
                    let r = f
                        .relocate_or_forward(
                            BASE + i * SIZE,
                            SIZE,
                            |s| heap.alloc(s),
                            |x, y, s| heap.copy(x, y, s),
                        )
                        .unwrap();
                    if matches!(r, ZRegionRelocated::Relocated(_)) {
                        wins += 1;
                    }
                    seen[i] = r.address();
                }
                (seen, wins)
            })
        })
        .collect();
    let results: Vec<(Vec<usize>, usize)> =
        handles.into_iter().map(|h| h.join().unwrap()).collect();
    let total_wins: usize = results.iter().map(|r| r.1).sum();
    assert_eq!(total_wins, OBJECTS, "exactly one install per object");
    for i in 0..OBJECTS {
        let a = results[0].0[i];
        assert!(results.iter().all(|r| r.0[i] == a), "object {i} split");
        assert_eq!(f.forwarded(BASE + i * SIZE), Some(a));
        assert_eq!(heap.word(a).load(Ordering::Relaxed), i as u64 + 1);
    }
    let (won, won_bytes, lost, abandoned) = f.stats();
    assert_eq!((won, won_bytes), (OBJECTS, OBJECTS * SIZE));
    assert_eq!(abandoned, lost * SIZE);
    assert_eq!(f.in_flight(), 0);
}

#[test]
fn the_cycle_maps_addresses_to_selected_regions_only() {
    let g = grid(4, 64 * KIB);
    let r = |i: usize| BASE + i * 64 * KIB;
    let l = region_liveness(
        &g,
        [
            (r(0), 60 * KIB),
            (r(1), KIB),
            (r(1) + KIB, KIB),
            (r(3), 2 * KIB),
        ],
    );
    let sel = select_relocation_regions(&g, &l, &ZRelocationPolicy::default(), |_| false);
    let cycle = ZRelocationCycle::build(&g, &l, &sel.set);
    assert_eq!(cycle.regions().len(), 2);
    assert!(cycle.forwarding_for(r(0) + 8).is_none());
    assert!(cycle.forwarding_for(r(2)).is_none());
    assert!(cycle.forwarding_for(BASE - 8).is_none());
    assert!(cycle.forwarding_for(r(4)).is_none());
    let f1 = cycle.forwarding_for(r(1) + 100).unwrap();
    assert_eq!(f1.region(), 1);
    assert_eq!(f1.live(), (2, 2 * KIB));
    assert_eq!(f1.span(), r(1)..r(2));
    assert_eq!(cycle.forwarding_for(r(3)).unwrap().region(), 3);
    assert_eq!(cycle.forwarded(r(1)), None);
}

#[test]
fn workers_claim_each_region_once_and_the_cycle_is_quiescent_when_all_seal() {
    let g = grid(4, 64 * KIB);
    let r = |i: usize| BASE + i * 64 * KIB;
    let l = region_liveness(&g, [(r(0), KIB), (r(1), KIB), (r(2), KIB), (r(3), KIB)]);
    let sel = select_relocation_regions(&g, &l, &ZRelocationPolicy::default(), |_| false);
    let cycle = ZRelocationCycle::build(&g, &l, &sel.set);
    assert_eq!(cycle.regions().len(), 4);
    let mut claimed = Vec::new();
    while let Some(f) = cycle.claim_next() {
        claimed.push(f.region());
    }
    claimed.sort_unstable();
    assert_eq!(claimed, vec![0, 1, 2, 3]);
    assert!(!cycle.all_quiescent());
    for f in cycle.regions() {
        assert!(f.is_claimed());
        assert_eq!(f.seal(), 0);
    }
    assert!(cycle.all_quiescent());
}

// ---- zt t23/zc5: R1 (census) and R2 (the stop-the-world drain) --------

#[test]
fn zt23_build_regions_keeps_the_given_order_and_skips_repeats_and_strays() {
    let g = grid(4, 64 * KIB);
    let r = |i: usize| BASE + i * 64 * KIB;
    // Region 1: two objects, 32 + 64 = 96 bytes; region 3: one, 64 bytes.
    let l = region_liveness(&g, [(r(1), 32), (r(1) + 64, 64), (r(3), 64)]);
    // 3 first, then 1; the repeated 3 and the stray 9 (past the grid's
    // four regions) are skipped.
    let cycle = ZRelocationCycle::build_regions(&g, &l, &[3, 1, 3, 9]);
    let ids: Vec<u64> = cycle.regions().iter().map(|f| f.region()).collect();
    assert_eq!(ids, vec![3, 1]);
    assert_eq!(cycle.regions()[1].live(), (2, 96));
    assert_eq!(cycle.regions()[0].live(), (1, 64));
    assert_eq!(cycle.claim_next().map(|f| f.region()), Some(3));
    assert_eq!(cycle.claim_next().map(|f| f.region()), Some(1));
    assert!(cycle.claim_next().is_none());
    assert!(cycle.forwarding_for(r(2)).is_none());
}

#[test]
fn zt23_the_census_selects_from_survivors_and_lists_empty_regions_apart() {
    // The fixture of `selection_frees_empty_regions_...`: region 0 dense
    // (60 of 64 KiB live), 1 and 5 sparse, 2 empty, 3 excluded by the
    // caller, 4 straddled by region 3's last object (16 bytes reach in).
    let g = grid(6, 64 * KIB);
    let r = |i: usize| BASE + i * 64 * KIB;
    let objs = vec![
        (r(0), 60 * KIB),
        (r(1), KIB),
        (r(3), KIB),
        (r(4) - 16, 32),
        (r(5) + 128, 2 * KIB),
    ];
    let c = reloc_select_census(&g, objs, &ZRelocationPolicy::default(), |i| i == 3);
    assert_eq!(c.regions, 6);
    assert_eq!(c.set, vec![1, 5]);
    // 1 KiB (region 1) + 2 KiB (region 5).
    assert_eq!(c.set_live_bytes, 3 * KIB);
    assert_eq!(c.empty, vec![2]);
    assert_eq!(
        (c.straddled, c.excluded, c.unsizable, c.outside),
        (1, 1, 0, 0)
    );
    assert!(c.slide.is_none());
    assert_eq!(c.agreement(), ZRelocSelectAgreement::default());
}

#[test]
fn zt23_agreement_is_counted_region_by_region() {
    // R1 reclaims regions 1 and 5 (set) and 2 (empty) of six.
    let r1 = ZRelocSelectCensus {
        regions: 6,
        set: vec![1, 5],
        empty: vec![2],
        ..ZRelocSelectCensus::default()
    };
    // The slide's grid stops at 5 regions (the sweep retracted the cursor
    // below region 5); it selected 1, 2, 4 and a stray 7.
    //   0: neither -> agree      1: both -> agree     2: both -> agree
    //   3: neither -> agree      4: slide only        5: R1 set, retracted
    //   (reclaimed by the slide too) -> agree;  stray 7 -> slide only.
    let c = ZRelocSelectCensus {
        slide: Some((vec![1, 2, 4, 7], 5)),
        ..r1.clone()
    };
    assert_eq!(
        c.agreement(),
        ZRelocSelectAgreement {
            compared: true,
            agree: 5,
            r1_only: 0,
            slide_only: 2,
            retracted: 1,
        }
    );
    // The slide selected nothing over a grid as tall as R1's: R1's three
    // reclaimed regions (1, 2, 5) are R1-only, the other three agree.
    let c = ZRelocSelectCensus {
        slide: Some((vec![], 6)),
        ..r1.clone()
    };
    assert_eq!(
        c.agreement(),
        ZRelocSelectAgreement {
            compared: true,
            agree: 3,
            r1_only: 3,
            slide_only: 0,
            retracted: 0,
        }
    );
    let line = c.line(7);
    assert!(line.starts_with("[GC] zgc-rsel: rsel_cycle=0 rsel_regions=6 rsel_set=2 "));
    assert!(line.contains(" rsel_compared=1 rsel_slide_set=0 rsel_agree=3 rsel_r1_only=3 "));
    assert!(line.ends_with(" rsel_young_skipped=7"));
    // 17 keys, each `rsel_`-prefixed and printed once.
    let keys: Vec<&str> = line
        .split_whitespace()
        .skip(2)
        .map(|kv| kv.split('=').next().unwrap_or(""))
        .collect();
    assert_eq!(keys.len(), 17);
    assert!(keys.iter().all(|k| k.starts_with("rsel_")));
    let mut unique = keys.clone();
    unique.sort_unstable();
    unique.dedup();
    assert_eq!(unique.len(), keys.len());
    // No slide side: not compared, counts zero.
    assert!(r1
        .line(0)
        .contains(" rsel_compared=0 rsel_slide_set=0 rsel_agree=0 "));
}

#[test]
fn zt23_the_drain_copies_in_address_order_into_to_space() {
    let g = grid(4, 64 * KIB);
    let r = |i: usize| BASE + i * 64 * KIB;
    // To-space starts at the grid's end, as the arena's window opens at
    // the old low cursor.
    let heap = FakeHeap::new(8 * 64 * KIB, 4 * 64 * KIB);
    let t = BASE + 4 * 64 * KIB;
    let survivors = [
        (r(0) + 64, Some(32)),
        (r(0) + 256, Some(48)),
        (r(1) + 8, Some(32)),
        (r(3), Some(64)),
    ];
    for (k, &(from, _)) in survivors.iter().enumerate() {
        heap.word(from).store(k as u64 + 1, Ordering::Relaxed);
    }
    let d = drain_stw(
        &g,
        &survivors,
        false,
        usize::MAX,
        |s| heap.alloc(s),
        |f, to, s| heap.copy(f, to, s),
    );
    // Bumped in survivor order: 32, 48, 32 then 64 bytes, so the
    // destinations are t, t+32, t+80 and t+112.
    assert_eq!(
        d.pairs,
        vec![
            (r(0) + 64, t),
            (r(0) + 256, t + 32),
            (r(1) + 8, t + 80),
            (r(3), t + 112)
        ]
    );
    assert_eq!(
        d.moves,
        vec![
            (r(0) + 64, 32),
            (r(0) + 256, 48),
            (r(1) + 8, 32),
            (r(3), 64)
        ]
    );
    // 32 + 48 + 32 + 64.
    assert_eq!(d.moved_bytes, 176);
    // Regions 0, 1 and 3 hold survivors; region 2 gets no table.
    assert_eq!(d.regions, 3);
    assert!(d.stays.is_empty());
    assert!(!d.halted);
    assert_eq!((d.alloc_stops, d.refusals, d.xcheck_bad), (0, 0, 0));
    for (k, &(_, to)) in d.pairs.iter().enumerate() {
        assert_eq!(heap.word(to).load(Ordering::Relaxed), k as u64 + 1);
    }
}

#[test]
fn zt23_the_drain_keeps_the_slides_stay_rules() {
    let g = grid(4, 64 * KIB);
    let r = |i: usize| BASE + i * 64 * KIB;
    let heap = FakeHeap::new(8 * 64 * KIB, 4 * 64 * KIB);
    let t = BASE + 4 * 64 * KIB;
    // The grid's end is the old cursor, where to-space begins.
    let end = g.end();
    assert_eq!(end, t);
    let survivors = [
        // 48 bytes is above the 40-byte extent limit: stays, no halt.
        (r(0) + 64, Some(48)),
        // Copied to the first to-space byte.
        (r(0) + 128, Some(32)),
        // Unsizable: stays, bounded by the grid's end, and halts.
        (r(0) + 256, None),
        // After the halt: stays.
        (r(1), Some(32)),
        // After the halt and unsizable: stays to the grid's end.
        (r(2), None),
    ];
    let d = drain_stw(
        &g,
        &survivors,
        false,
        40,
        |s| heap.alloc(s),
        |f, to, s| heap.copy(f, to, s),
    );
    assert_eq!(d.pairs, vec![(r(0) + 128, t)]);
    assert_eq!(d.moves, vec![(r(0) + 128, 32)]);
    // 64 + 48 = 112 for the first stay.
    assert_eq!(
        d.stays,
        vec![
            (r(0) + 64, r(0) + 112),
            (r(0) + 256, end),
            (r(1), r(1) + 32),
            (r(2), end)
        ]
    );
    assert!(d.halted);
    // Regions 0, 1, 2 each have a survivor (region 2's is unsizable, so
    // its table is sized for zero entries and receives none).
    assert_eq!(d.regions, 3);
    assert_eq!((d.alloc_stops, d.refusals), (0, 0));
}

#[test]
fn zt23_a_refused_allocation_halts_the_drain_and_a_halted_entry_moves_nothing() {
    let g = grid(2, 64 * KIB);
    let r = |i: usize| BASE + i * 64 * KIB;
    let heap = FakeHeap::new(4 * 64 * KIB, 2 * 64 * KIB);
    let t = BASE + 2 * 64 * KIB;
    let survivors = [
        (r(0) + 64, Some(32)),
        (r(0) + 128, Some(32)),
        (r(1), Some(32)),
    ];
    // The first allocation succeeds, the second is refused.
    let calls = std::cell::Cell::new(0usize);
    let d = drain_stw(
        &g,
        &survivors,
        false,
        usize::MAX,
        |s| {
            calls.set(calls.get() + 1);
            if calls.get() == 1 {
                heap.alloc(s)
            } else {
                None
            }
        },
        |f, to, s| heap.copy(f, to, s),
    );
    assert_eq!(d.pairs, vec![(r(0) + 64, t)]);
    // 128 + 32 = 160.
    assert_eq!(d.stays, vec![(r(0) + 128, r(0) + 160), (r(1), r(1) + 32)]);
    assert!(d.halted);
    assert_eq!((d.alloc_stops, d.refusals), (1, 0));
    assert_eq!(calls.get(), 2, "no allocation after the halt");
    // Halted on entry (the to-space window did not open): every survivor
    // stays and nothing is allocated or copied; both regions still get a
    // (sealed, empty) table.
    let d = drain_stw(
        &g,
        &survivors,
        true,
        usize::MAX,
        |_| panic!("no allocation on a halted drain"),
        |_, _, _| panic!("no copy on a halted drain"),
    );
    assert!(d.pairs.is_empty() && d.moves.is_empty());
    assert_eq!(
        d.stays,
        vec![
            (r(0) + 64, r(0) + 96),
            (r(0) + 128, r(0) + 160),
            (r(1), r(1) + 32)
        ]
    );
    assert_eq!(d.regions, 2);
    assert!(d.halted);
}

#[test]
fn zt23_a_survivor_outside_the_grid_is_refused_and_stays() {
    let g = grid(2, 64 * KIB);
    let r = |i: usize| BASE + i * 64 * KIB;
    let heap = FakeHeap::new(4 * 64 * KIB, 3 * 64 * KIB);
    // r(2) + 8 is past the grid's end (r(2)): no region, no table.
    let survivors = [(r(1), Some(32)), (r(2) + 8, Some(32))];
    let d = drain_stw(
        &g,
        &survivors,
        false,
        usize::MAX,
        |s| heap.alloc(s),
        |f, to, s| heap.copy(f, to, s),
    );
    assert_eq!(d.pairs, vec![(r(1), BASE + 3 * 64 * KIB)]);
    // 8 + 32 = 40.
    assert_eq!(d.stays, vec![(r(2) + 8, r(2) + 40)]);
    assert_eq!((d.regions, d.refusals), (1, 1));
    assert!(!d.halted);
}

// -----------------------------------------------------------------------
// zt t23/zc7: the cycle after the drain (R2's readers, R3's kept cycle)
// -----------------------------------------------------------------------

/// The drain of `zt23_the_drain_copies_in_address_order_into_to_space`,
/// shared by the zc7 tests: survivors at r(0)+64 (32 B), r(0)+256 (48 B),
/// r(1)+8 (32 B) and r(3) (64 B) of a 4 x 64 KiB grid, to-space from
/// t = r(4).
fn zc7_survivors() -> [(usize, Option<usize>); 4] {
    let r = |i: usize| BASE + i * 64 * KIB;
    [
        (r(0) + 64, Some(32)),
        (r(0) + 256, Some(48)),
        (r(1) + 8, Some(32)),
        (r(3), Some(64)),
    ]
}

fn zc7_drain() -> (ZRegionDrain, ZRelocationCycle) {
    let g = grid(4, 64 * KIB);
    let heap = FakeHeap::new(8 * 64 * KIB, 4 * 64 * KIB);
    drain_stw_keep(
        &g,
        &zc7_survivors(),
        false,
        usize::MAX,
        |s| heap.alloc(s),
        |f, to, s| heap.copy(f, to, s),
    )
}

#[test]
fn zt23zc7_the_kept_cycle_enumerates_its_installs_and_sizes_its_tables() {
    let (d, cycle) = zc7_drain();
    let r = |i: usize| BASE + i * 64 * KIB;
    let t = r(4);
    // As in the zc5 test: bumped 32, 48, 32, 64 -> t, t+32, t+80, t+112.
    assert_eq!(
        d.pairs,
        vec![
            (r(0) + 64, t),
            (r(0) + 256, t + 32),
            (r(1) + 8, t + 80),
            (r(3), t + 112)
        ]
    );
    assert_eq!(cycle.installed(), 4);
    assert!(cycle.all_quiescent());
    let mut seen: Vec<(usize, usize)> = Vec::new();
    cycle.for_each_pair(|f, to| seen.push((f, to)));
    seen.sort_unstable();
    assert_eq!(seen, d.pairs, "the tables hold exactly the drained pairs");
    // Tables: region 0 holds 2 survivors, regions 1 and 3 one each.
    // `zfwd_capacity_for(2)` = next_pow2(4) = 4 and `zfwd_capacity_for(1)`
    // = next_pow2(2) = 2, both clamped up to ZFWD_MIN_CAPACITY = 8 slots:
    // 3 tables x 8 slots x 8 bytes = 192, plus the region map, 4 grid
    // regions x 4 bytes = 16. 208 bytes.
    assert_eq!(cycle.table_bytes(), 208);
    // `drain_stw` is the same drain without the cycle.
    let g = grid(4, 64 * KIB);
    let heap = FakeHeap::new(8 * 64 * KIB, 4 * 64 * KIB);
    let again = drain_stw(
        &g,
        &zc7_survivors(),
        false,
        usize::MAX,
        |s| heap.alloc(s),
        |f, to, s| heap.copy(f, to, s),
    );
    assert_eq!(again, d);
}

/// The slide's forward index through the cycle answers exactly what the
/// sorted index over the same pair list answers, while holding only the
/// pairs the cycle does not vouch for.
#[test]
fn zt23zc7_the_index_through_the_cycle_answers_as_the_pair_index() {
    use crate::zgc::relocate::ZForwardIndex;
    let (d, cycle) = zc7_drain();
    let r = |i: usize| BASE + i * 64 * KIB;
    let t = r(4);
    // Two pairs the cycle does not hold: one from region 2 (no survivor
    // there, so no table) and one above the grid (the large-object end's
    // shape).
    let h = BASE + 7 * 64 * KIB;
    let mut all = d.pairs.clone();
    all.push((h + 4096, h + 8192));
    all.push((r(2) + 16, BASE + 6 * 64 * KIB));
    let plain = ZForwardIndex::from_pairs(&all);
    let cycle = Arc::new(cycle);
    let with = ZForwardIndex::from_pairs_with_cycle(&all, Arc::clone(&cycle));
    assert_eq!(
        with.entries(),
        &[(r(2) + 16, BASE + 6 * 64 * KIB), (h + 4096, h + 8192)][..],
        "only the two pairs the cycle does not answer are listed"
    );
    // 2 listed + 4 installs: the same 6 moves the plain index lists.
    assert_eq!((with.len(), plain.len()), (6, 6));
    let mut probes: Vec<usize> =
        vec![BASE - 8, BASE, r(0), r(0) + 72, r(1), r(2), r(3) + 8, t, h];
    for &(f, to) in &all {
        probes.extend([f, to, f + 8, f + 3]);
    }
    for a in probes {
        assert_eq!(with.get(a), plain.get(a), "address {a:#x}");
        assert_eq!(with.resolve(a), plain.resolve(a), "address {a:#x}");
    }
    assert_eq!(with.get(r(1) + 8), Some(t + 80));
    assert_eq!(with.get(r(2) + 16), Some(BASE + 6 * 64 * KIB));
    assert_eq!(with.get(r(0) + 72), None, "an interior word did not move");
    // Every cycle region installed something; region 3 ends at the grid's
    // end, r(4).
    assert_eq!(
        with.cycle_source_spans(),
        vec![(r(0), r(1)), (r(1), r(2)), (r(3), r(4))]
    );
    assert!(plain.cycle_source_spans().is_empty());
    // `for_slide` picks the constructor.
    let none = ZForwardIndex::for_slide(&all, None);
    assert!(none.cycle().is_none());
    assert_eq!(none.entries().len(), 6);
    let some = ZForwardIndex::for_slide(&all, Some(&cycle));
    assert!(some.cycle().is_some());
    assert_eq!(some.entries().len(), 2);    }