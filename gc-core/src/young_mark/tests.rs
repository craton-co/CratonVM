#[test]
fn gcj_retire_object_start_copy_is_independent() {
    let bits = ObjectStartBits::new(0x1000, 4096);
    assert!(bits.insert(0x1100));
    let copy = bits.duplicate();
    assert!(copy.contains(0x1100));
    assert!(bits.insert(0x1200));
    assert!(!copy.contains(0x1200));
    assert!(copy.insert(0x1300));
    assert!(!bits.contains(0x1300));
}

#[test]
fn object_start_bits_match_a_hash_set_exactly() {
    const BASE: usize = 0x1_0000;
    const SPAN: usize = 4096;
    let bits = ObjectStartBits::new(BASE, SPAN);
    let mut set = std::collections::HashSet::new();
    for k in [0usize, 1, 2, 7, 63, 64, 65, 511] {
        let addr = BASE + k * 8;
        assert!(bits.insert(addr), "{addr:#x} is in-span and aligned");
        set.insert(addr);
    }
    // Every 8-byte slot in the span must agree with the reference set.
    for k in 0..(SPAN / 8) {
        let addr = BASE + k * 8;
        assert_eq!(
            bits.contains(addr),
            set.contains(&addr),
            "membership disagrees at {addr:#x}"
        );
    }
    assert_eq!(bits.len(), set.len());
}

/// gen r4w5/pinned5: `next_start_at_or_above` is the forward twin of
/// `nearest_start_at_or_below` — it must agree with a linear search over
/// a reference set at every address, across word boundaries, off the
/// grid, below the span and past its last start.
#[test]
fn next_start_at_or_above_matches_a_linear_search() {
    const BASE: usize = 0x2_0000;
    const SPAN: usize = 8192;
    let bits = ObjectStartBits::new(BASE, SPAN);
    let starts = [3usize, 63, 64, 65, 300, 511, 1000];
    for k in starts {
        assert!(bits.insert(BASE + k * 8));
    }
    let reference = |addr: usize| -> Option<usize> {
        starts
            .iter()
            .map(|k| BASE + k * 8)
            .filter(|&a| a >= addr)
            .min()
    };
    for addr in (BASE..BASE + SPAN).step_by(4) {
        assert_eq!(
            bits.next_start_at_or_above(addr),
            reference(addr),
            "disagrees at {addr:#x}"
        );
    }
    assert_eq!(
        bits.next_start_at_or_above(BASE - 64),
        Some(BASE + 3 * 8),
        "below the span answers the first start"
    );
    assert_eq!(bits.next_start_at_or_above(BASE + SPAN), None);
}

/// gengc-mark2 2026-09-20 — `retain_intersection` is the word-wise
/// substitute for `HashSet::retain(|a| other.contains(a))` that
/// `concurrent_mark::remark` uses to narrow its sweep-eligibility
/// snapshot. It must agree with that `retain` exactly.
#[test]
fn retain_intersection_matches_hash_set_retain() {
    const BASE: usize = 0x1_0000;
    const SPAN: usize = 4096;
    let mine = ObjectStartBits::new(BASE, SPAN);
    let theirs = ObjectStartBits::new(BASE, SPAN);
    let mut mine_set = std::collections::HashSet::new();
    let theirs_set: std::collections::HashSet<usize> = [0usize, 2, 63, 64, 200]
        .iter()
        .map(|k| BASE + k * 8)
        .collect();
    for k in [0usize, 1, 2, 64, 65, 200, 300] {
        let addr = BASE + k * 8;
        assert!(mine.insert(addr));
        mine_set.insert(addr);
    }
    for &addr in &theirs_set {
        assert!(theirs.insert(addr));
    }

    assert!(mine.retain_intersection(&theirs));
    mine_set.retain(|a| theirs_set.contains(a));

    for k in 0..(SPAN / 8) {
        let addr = BASE + k * 8;
        assert_eq!(
            mine.contains(addr),
            mine_set.contains(&addr),
            "intersection disagrees with HashSet::retain at {addr:#x}"
        );
    }
    assert_eq!(mine.len(), mine_set.len());
    assert!(!mine.is_empty());
}

/// The snapshot may be SHORTER than the later walk — the old generation
/// grows while a cycle is open — but never longer, and never at a
/// different base. Both refusals must change nothing, because the caller
/// fails closed on them.
#[test]
fn retain_intersection_refuses_mismatched_geometry() {
    const BASE: usize = 0x1_0000;
    let short = ObjectStartBits::new(BASE, 512);
    let long = ObjectStartBits::new(BASE, 4096);
    let elsewhere = ObjectStartBits::new(BASE + 4096, 512);
    assert!(short.insert(BASE));
    assert!(long.insert(BASE));

    // Shorter snapshot against a longer walk: allowed.
    assert!(short.retain_intersection(&long));
    assert!(short.contains(BASE));

    // Longer against shorter, and a different base: refused, unchanged.
    assert!(!long.retain_intersection(&short));
    assert!(long.contains(BASE), "a refusal must not modify the bitmap");
    assert!(!long.retain_intersection(&elsewhere));
    assert!(long.contains(BASE));
}

/// `is_empty` must agree with `len() == 0` on both sides.
#[test]
fn object_start_bits_is_empty_agrees_with_len() {
    const BASE: usize = 0x1_0000;
    let bits = ObjectStartBits::new(BASE, 4096);
    assert!(bits.is_empty());
    assert_eq!(bits.len(), 0);
    assert!(bits.insert(BASE + 64 * 8));
    assert!(!bits.is_empty());
    assert_eq!(bits.len(), 1);
    // A zero-span bitmap has no words at all and must still be empty.
    let none = ObjectStartBits::new(BASE, 0);
    assert!(none.is_empty());
    assert_eq!(none.len(), 0);
}

#[test]
fn object_start_bits_reject_out_of_span_and_unaligned() {
    const BASE: usize = 0x1_0000;
    let bits = ObjectStartBits::new(BASE, 4096);
    // Below the base, at the end of the span, and past it.
    assert!(!bits.insert(BASE - 8));
    assert!(!bits.insert(BASE + 4096));
    assert!(!bits.contains(BASE - 8));
    assert!(!bits.contains(BASE + 4096));
    // An 8-byte-misaligned start would alias its neighbour's bit, so it is
    // refused rather than recorded: the caller diverts to the non-moving
    // sweep. Recording it would make `contains(BASE + 8)` true.
    assert!(!bits.insert(BASE + 12));
    assert!(!bits.contains(BASE + 12));
    assert!(!bits.contains(BASE + 8));
    assert_eq!(bits.len(), 0);
}

/// gc-genpause F1: the word-batching cursor must record exactly the bits
/// per-address `insert` would, and must have published them by the time it
/// is dropped.
///
/// The batching is the whole point of the cursor and also its only risk:
/// bits live in a register-held mask until the walk moves past the word,
/// so a missed flush loses up to 63 object starts silently -- and a start
/// this bitmap does not have is one `forward_object` refuses to relocate.
#[test]
fn the_batching_cursor_records_exactly_what_insert_would() {
    const BASE: usize = 0x40_0000;
    const SPAN: usize = 4096;
    // Addresses crossing several word boundaries (one word = 512 bytes),
    // several within one word, and revisits in DESCENDING order to prove
    // the cursor does not assume ascending input.
    let addrs: Vec<usize> = vec![
        BASE,
        BASE + 8,
        BASE + 16,
        BASE + 504,
        BASE + 512,
        BASE + 520,
        BASE + 1024,
        BASE + 1032,
        BASE + 2040,
        BASE + 2048,
        BASE + 4088,
        BASE + 512,
        BASE + 8,
        BASE + 3000,
    ];

    let direct = ObjectStartBits::new(BASE, SPAN);
    for &a in &addrs {
        assert!(direct.insert(a), "{a:#x} is in-span and aligned");
    }

    let batched = ObjectStartBits::new(BASE, SPAN);
    {
        let mut run = batched.run();
        for &a in &addrs {
            assert!(run.insert(a), "{a:#x} is in-span and aligned");
        }
        // Deliberately NOT flushed by hand: the drop below must do it.
    }

    assert_eq!(batched.len(), direct.len(), "start counts must agree");
    for off in (0..SPAN).step_by(8) {
        assert_eq!(
            batched.contains(BASE + off),
            direct.contains(BASE + off),
            "membership disagrees at +{off}"
        );
    }
}

/// The cursor must reject the same addresses `insert` rejects -- an
/// unaligned or out-of-span start cannot be represented, and the caller
/// treats that as a walk that did not complete.
#[test]
fn the_batching_cursor_rejects_what_insert_rejects() {
    const BASE: usize = 0x40_0000;
    let bits = ObjectStartBits::new(BASE, 4096);
    let mut run = bits.run();
    assert!(!run.insert(BASE + 4), "unaligned");
    assert!(!run.insert(BASE + 4096), "past the span");
    assert!(!run.insert(BASE - 8), "before the span");
    assert!(run.insert(BASE + 4088), "the last aligned slot is in-span");
}

#[test]
fn object_start_bits_handle_an_empty_span() {
    let bits = ObjectStartBits::new(0x1_0000, 0);
    assert!(!bits.insert(0x1_0000));
    assert!(!bits.contains(0x1_0000));
    assert_eq!(bits.len(), 0);
}

use super::*;
use std::sync::atomic::AtomicUsize;

#[test]
fn bitmap_claims_each_address_once() {
    let bits = YoungMarkBits::new(0x1000, 4096);
    assert!(bits.try_mark(0x1000));
    assert!(!bits.try_mark(0x1000));
    assert!(bits.contains(0x1000));
    assert!(!bits.contains(0x1008));
    assert!(bits.try_mark(0x1008));
    assert!(bits.contains(0x1008));
}

#[test]
fn bitmap_ignores_out_of_range() {
    let bits = YoungMarkBits::new(0x1000, 64);
    assert!(!bits.try_mark(0x0fff));
    assert!(!bits.try_mark(0x1040));
    assert!(!bits.contains(0x1040));
    assert!(bits.try_mark(0x1038));
}

#[test]
fn bitmap_zero_span_is_inert() {
    let bits = YoungMarkBits::new(0x1000, 0);
    assert!(!bits.try_mark(0x1000));
    assert!(bits.collect_marked().is_empty());
}

#[test]
fn collect_marked_is_ascending_and_complete() {
    let bits = YoungMarkBits::new(0, 8192);
    let want: Vec<usize> = vec![0, 8, 512, 4088, 4096, 8184];
    for &a in &want {
        assert!(bits.try_mark(a));
    }
    assert_eq!(bits.collect_marked(), want);
}

#[test]
fn parallel_drain_visits_every_node_exactly_once() {
    // Synthetic graph: node i (0..N) points to 2i+1 and 2i+2.
    const N: usize = 50_000;
    let visits: Vec<AtomicUsize> = (0..N).map(|_| AtomicUsize::new(0)).collect();
    for threads in [1usize, 2, 4, 8] {
        for v in &visits {
            v.store(0, Ordering::Relaxed);
        }
        let bits = YoungMarkBits::new(0, N * 8);
        assert!(bits.try_mark(0));
        drain_parallel(vec![0], threads, |addr, work| {
            let i = addr / 8;
            visits[i].fetch_add(1, Ordering::Relaxed);
            for child in [2 * i + 1, 2 * i + 2] {
                if child < N && bits.try_mark(child * 8) {
                    work.push(child * 8);
                }
            }
        });
        assert_eq!(bits.collect_marked().len(), N, "threads={threads}");
        for (i, v) in visits.iter().enumerate() {
            assert_eq!(v.load(Ordering::Relaxed), 1, "node {i}, threads={threads}");
        }
    }
}

/// gengc-mark 2026-09-20 — `unmark` must refuse a MISALIGNED address.
///
/// `locate` floors an address into its containing 8-byte slot, which is the
/// safe direction for `try_mark` and the unsafe one for `unmark`: clearing
/// through `base + 3` would retire `base`'s bit, and `base` may be a
/// genuinely marked live object start. Losing it from `collect_marked`
/// loses it from `side_sorted`, which is the sweep's live set.
#[test]
fn unmark_refuses_a_misaligned_address() {
    let bits = YoungMarkBits::new(0x1000, 4096);
    assert!(bits.try_mark(0x1000));
    // A misaligned clear must not touch the aligned neighbour's bit.
    assert!(!bits.unmark(0x1001));
    assert!(!bits.unmark(0x1007));
    assert!(
        bits.contains(0x1000),
        "a misaligned unmark retired a live object's mark"
    );
    // The aligned clear still works, which is the only case the caller
    // (the late base-resolution pass) can actually produce.
    assert!(bits.unmark(0x1000));
    assert!(!bits.contains(0x1000));
}

/// gengc-mark 2026-09-20 — a `scan` that panics must not hang the drain.
///
/// Termination used to be `idle == threads`, which assumed every worker
/// reaches the idle check. A worker that unwinds out of `scan` never does,
/// so the survivors parked on the condvar forever and `thread::scope`
/// waited on them: a hang inside a GC pause instead of a panic. The drain
/// must now propagate the panic promptly.
///
/// A regression here manifests as this test HANGING, not failing.
#[test]
fn a_panicking_scan_terminates_the_drain_instead_of_hanging() {
    const POISON: usize = 777 * 8;
    let seed: Vec<usize> = (0..1000usize).map(|i| i * 8).collect();
    let prev = std::panic::take_hook();
    // The worker's panic is expected; don't spam the test output with it.
    std::panic::set_hook(Box::new(|_| {}));
    let caught = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        drain_parallel(seed, 4, |addr, _work| {
            assert_ne!(addr, POISON, "poisoned object");
        });
    }));
    std::panic::set_hook(prev);
    assert!(
        caught.is_err(),
        "the drain must propagate the worker's panic, not swallow it"
    );
}

/// gengc-mark 2026-09-20 — the early-spill path must not lose or duplicate
/// work.
///
/// A worker now publishes surplus as soon as a peer is parked
/// (`SPILL_WHEN_PEER_IDLE`), not only at `SPILL_HIGH`. The graph below is
/// shaped to cross that threshold repeatedly -- a spine of hub nodes each
/// fanning out to 200 leaves -- so the drain takes the early-spill branch
/// many times per run. Every node must still be scanned exactly once.
#[test]
fn early_spill_visits_every_node_exactly_once() {
    const HUBS: usize = 40;
    const FANOUT: usize = 200;
    const N: usize = HUBS * (FANOUT + 1);
    for threads in [1usize, 2, 4, 8] {
        let visits: Vec<AtomicUsize> = (0..N).map(|_| AtomicUsize::new(0)).collect();
        let bits = YoungMarkBits::new(0, N * 8);
        assert!(bits.try_mark(0));
        drain_parallel(vec![0], threads, |addr, work| {
            let i = addr / 8;
            visits[i].fetch_add(1, Ordering::Relaxed);
            // Hub `h` lives at index `h * (FANOUT + 1)`; it points at its
            // FANOUT leaves and at the next hub.
            if i % (FANOUT + 1) != 0 {
                return; // leaf
            }
            for k in 1..=FANOUT {
                let child = i + k;
                if child < N && bits.try_mark(child * 8) {
                    work.push(child * 8);
                }
            }
            let next_hub = i + FANOUT + 1;
            if next_hub < N && bits.try_mark(next_hub * 8) {
                work.push(next_hub * 8);
            }
        });
        assert_eq!(bits.collect_marked().len(), N, "threads={threads}");
        for (i, v) in visits.iter().enumerate() {
            assert_eq!(v.load(Ordering::Relaxed), 1, "node {i}, threads={threads}");
        }
    }
}

/// gcf f2/y (kept by gcg g4/yg when the spin-before-park policy was
/// deleted): the park-at-once drain with its fair share scans every node
/// exactly once on both graph shapes above, with scoped threads and on a
/// persistent pool, over repeated drains (a lost wake-up or a premature
/// termination would show as a missing node or a hang).
#[test]
fn f2y_spinning_drain_visits_every_node_exactly_once() {
    const HUBS: usize = 40;
    const FANOUT: usize = 200;
    const N: usize = HUBS * (FANOUT + 1);
    let pool = crate::evac_pool::EvacPool::new(1);
    for threads in [2usize, 4, 8] {
        for use_pool in [false, true] {
            for round in 0..4 {
                let visits: Vec<AtomicUsize> = (0..N).map(|_| AtomicUsize::new(0)).collect();
                let bits = YoungMarkBits::new(0, N * 8);
                assert!(bits.try_mark(0));
                let scan = |addr: usize, work: &mut Vec<usize>| {
                    let i = addr / 8;
                    visits[i].fetch_add(1, Ordering::Relaxed);
                    if round % 2 == 0 {
                        // Binary tree: a narrow frontier at first.
                        for child in [2 * i + 1, 2 * i + 2] {
                            if child < N && bits.try_mark(child * 8) {
                                work.push(child * 8);
                            }
                        }
                        return;
                    }
                    // Hubs and leaves: repeated early spills.
                    if i % (FANOUT + 1) != 0 {
                        return;
                    }
                    for k in 1..=FANOUT + 1 {
                        let child = i + k;
                        if child < N && bits.try_mark(child * 8) {
                            work.push(child * 8);
                        }
                    }
                };
                let p = if use_pool { Some(&pool) } else { None };
                drain_parallel_on(vec![0], threads, p, scan);
                assert_eq!(
                    bits.collect_marked().len(),
                    N,
                    "threads={threads} pool={use_pool} round={round}"
                );
                for (i, v) in visits.iter().enumerate() {
                    assert_eq!(
                        v.load(Ordering::Relaxed),
                        1,
                        "node {i}, threads={threads} pool={use_pool} round={round}"
                    );
                }
            }
        }
    }
}

/// gcf f2/y: a panicking scan still terminates the drain (the exit
/// guard wakes every parked peer, so none waits on a count that cannot
/// be reached). A regression HANGS.
#[test]
fn f2y_a_panicking_scan_terminates_a_spinning_drain() {
    const POISON: usize = 777 * 8;
    let seed: Vec<usize> = (0..1000usize).map(|i| i * 8).collect();
    let prev = std::panic::take_hook();
    std::panic::set_hook(Box::new(|_| {}));
    let caught = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        drain_parallel_on(seed, 4, None, |addr, _work| {
            assert_ne!(addr, POISON, "poisoned object");
        });
    }));
    std::panic::set_hook(prev);
    assert!(caught.is_err(), "the drain swallowed the worker's panic");
}

#[test]
fn zero_spans_parallel_clears_every_span() {
    let mut buf = vec![0xAAu8; 64 * 1024];
    let base = buf.as_mut_ptr() as usize;
    let spans: Vec<(usize, usize)> = (0..32).map(|i| (i * 2048, 1024)).collect();
    for threads in [1usize, 2, 4, 8] {
        for b in buf.iter_mut() {
            *b = 0xAA;
        }
        // SAFETY: spans are disjoint sub-ranges of `buf`.
        unsafe { zero_spans_parallel(base, &spans, threads) };
        for (off, sz) in &spans {
            assert!(buf[*off..*off + *sz].iter().all(|&b| b == 0));
            assert!(buf[*off + *sz..*off + 2048].iter().all(|&b| b == 0xAA));
        }
    }
}

/// gen r5w3/evac7: a FEW HUGE spans (the pinned in-place cycle's shape)
/// past the serial cut-off. The spans are cut into per-worker pieces now;
/// every byte of every span must still be zeroed, and not one byte beside
/// them, at every worker count — including more workers than spans.
#[test]
fn zero_spans_parallel_splits_a_few_huge_spans_exactly() {
    const MIB: usize = 1 << 20;
    let mut buf = vec![0xAAu8; 3 * MIB + 8192];
    let base = buf.as_mut_ptr() as usize;
    let spans: Vec<(usize, usize)> = vec![(8, 2 * MIB + 24), (2 * MIB + 4096, MIB - 8)];
    for threads in [1usize, 2, 3, 8] {
        for b in buf.iter_mut() {
            *b = 0xAA;
        }
        // SAFETY: the two spans are disjoint sub-ranges of `buf`.
        unsafe { zero_spans_parallel(base, &spans, threads) };
        let mut expect = vec![0xAAu8; buf.len()];
        for &(off, sz) in &spans {
            expect[off..off + sz].iter_mut().for_each(|b| *b = 0);
        }
        assert!(buf == expect, "threads={threads}: zeroed bytes differ");
    }
}

/// gce e1/y: the mark drain on the heap's persistent pool visits every
/// node exactly once, cycle after cycle on the SAME pool (its threads and
/// their thread-locals outlive a drain), at widths below, at and above
/// the pool's initial width (`phase_width` grows it).
#[test]
fn e1y_pool_drain_visits_every_node_exactly_once_across_cycles() {
    const N: usize = 30_000;
    let pool = crate::evac_pool::EvacPool::new(1);
    for round in 0..3 {
        for threads in [1usize, 2, 4, 8] {
            let visits: Vec<AtomicUsize> = (0..N).map(|_| AtomicUsize::new(0)).collect();
            let bits = YoungMarkBits::new(0, N * 8);
            assert!(bits.try_mark(0));
            drain_parallel_on(vec![0], threads, Some(&pool), |addr, work| {
                let i = addr / 8;
                visits[i].fetch_add(1, Ordering::Relaxed);
                for child in [2 * i + 1, 2 * i + 2] {
                    if child < N && bits.try_mark(child * 8) {
                        work.push(child * 8);
                    }
                }
            });
            assert_eq!(
                bits.collect_marked().len(),
                N,
                "round={round} threads={threads}"
            );
            assert!(
                visits.iter().all(|v| v.load(Ordering::Relaxed) == 1),
                "round={round} threads={threads}: a node was scanned twice or never"
            );
        }
    }
    // The width the drain asked for is the width it ran at (the pool grew).
    assert!(
        pool.helpers() >= 7,
        "phase_width grows the pool to the request"
    );
}

/// gce e1/y: `phase_width` never promises more workers than the pool has,
/// which is what the drain's `live` count rests on.
#[test]
fn e1y_phase_width_is_what_the_pool_can_supply() {
    assert_eq!(phase_width(0, None), 1);
    assert_eq!(phase_width(1, None), 1);
    assert_eq!(phase_width(6, None), 6);
    let pool = crate::evac_pool::EvacPool::new(0);
    assert_eq!(phase_width(1, Some(&pool)), 1, "one worker needs no pool");
    let w = phase_width(4, Some(&pool));
    assert!(w >= 1 && w <= 4);
    assert_eq!(
        w,
        pool.helpers().min(3) + 1,
        "width = driver + helpers that exist"
    );
}

/// gce e1/y: a panicking scan on the pool still ends the drain with the
/// panic (the pool re-raises the helper's payload after its barrier), and
/// the pool stays usable for the next drain.
#[test]
fn e1y_a_panicking_scan_on_the_pool_propagates_and_the_pool_survives() {
    const POISON: usize = 777 * 8;
    let pool = crate::evac_pool::EvacPool::new(3);
    let seed: Vec<usize> = (0..1000usize).map(|i| i * 8).collect();
    let prev = std::panic::take_hook();
    std::panic::set_hook(Box::new(|_| {}));
    let caught = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        drain_parallel_on(seed, 4, Some(&pool), |addr, _work| {
            assert_ne!(addr, POISON, "poisoned object");
        });
    }));
    std::panic::set_hook(prev);
    assert!(
        caught.is_err(),
        "the drain must propagate the worker's panic"
    );
    let seen = AtomicUsize::new(0);
    let seed: Vec<usize> = (0..1000usize).map(|i| i * 8).collect();
    drain_parallel_on(seed, 4, Some(&pool), |_addr, _work| {
        seen.fetch_add(1, Ordering::Relaxed);
    });
    assert_eq!(seen.load(Ordering::Relaxed), 1000);
}

/// gce e1/y: the span zeroing on the pool zeroes exactly the spans, at
/// every width, including a few huge spans cut into pieces.
#[test]
fn e1y_zero_spans_on_the_pool_zero_exactly_the_spans() {
    const MIB: usize = 1 << 20;
    let pool = crate::evac_pool::EvacPool::new(2);
    let mut buf = vec![0xAAu8; 3 * MIB + 8192];
    let base = buf.as_mut_ptr() as usize;
    let spans: Vec<(usize, usize)> = vec![
        (8, 2 * MIB + 24),
        (2 * MIB + 4096, 4096),
        (2 * MIB + 16384, MIB - 16384),
    ];
    for threads in [1usize, 2, 3, 8] {
        for b in buf.iter_mut() {
            *b = 0xAA;
        }
        // SAFETY: the spans are disjoint sub-ranges of `buf`; no dispatch
        // of `pool` is in progress.
        unsafe { zero_spans_parallel_on(base, &spans, threads, Some(&pool)) };
        let mut expect = vec![0xAAu8; buf.len()];
        for &(off, sz) in &spans {
            expect[off..off + sz].iter_mut().for_each(|b| *b = 0);
        }
        assert!(buf == expect, "threads={threads}: zeroed bytes differ");
    }
}

/// gci i2/st: run `f` on its own thread and FAIL (rather than hang the
/// test binary) when it has not returned within `secs`. A lost wake-up or
/// an unreachable termination count in the drain or the pool shows up as
/// this timeout; a panic inside `f` is reported as a panic.
fn st2_within<F: FnOnce() + Send + 'static>(secs: u64, what: &str, f: F) {
    let (tx, rx) = std::sync::mpsc::channel::<()>();
    std::thread::spawn(move || {
        f();
        let _ = tx.send(());
    });
    match rx.recv_timeout(std::time::Duration::from_secs(secs)) {
        Ok(()) => {}
        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
            panic!("{what}: not done within {secs} s (lost wake-up or unreachable count)")
        }
        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
            panic!("{what}: panicked")
        }
    }
}

/// gci i2/st -- the late-helper interleaving of the stalled-pause page.
/// An EMPTY seed makes the driver reach the idle check first on almost
/// every round and park; the helpers take the job later (they are woken
/// by the dispatch, not by the drain) and the LAST of them to arrive must
/// complete the closure and wake the parked driver. Repeated on one pool
/// at every width, so each worker is late on some round.
#[test]
fn st2_a_drain_whose_helpers_arrive_after_the_driver_parked_terminates() {
    st2_within(120, "late-helper drain", || {
        let pool = crate::evac_pool::EvacPool::new(1);
        for round in 0..400usize {
            let threads = 2 + round % 7;
            drain_parallel_on(Vec::new(), threads, Some(&pool), |_addr, _work| {
                unreachable!("an empty seed scans nothing")
            });
            // ...and a one-node seed, which exactly one worker takes while
            // every other one parks behind it.
            let seen = AtomicUsize::new(0);
            drain_parallel_on(vec![8], threads, Some(&pool), |_addr, _work| {
                seen.fetch_add(1, Ordering::Relaxed);
            });
            assert_eq!(seen.load(Ordering::Relaxed), 1, "round={round}");
        }
    });
}

/// gci i2/st -- work that appears only after every peer has parked must
/// reach a peer: the root's scan publishes 4096 children while the other
/// workers wait on the drain's condvar, and each child's scan refuses to
/// return until a SECOND thread has scanned a child. If the spill's
/// `notify_all` were lost, no peer would ever wake and the scans would
/// give up after 10 s (the `starved` flag); every node must still be
/// scanned exactly once, on the pool and on scoped threads.
#[test]
fn st2_work_published_after_every_peer_parked_wakes_a_peer() {
    st2_within(120, "late-work drain", || {
        const FAN: usize = 4096;
        let pool = crate::evac_pool::EvacPool::new(3);
        for use_pool in [true, false] {
            let visits: Vec<AtomicUsize> = (0..=FAN).map(|_| AtomicUsize::new(0)).collect();
            let bits = YoungMarkBits::new(0, (FAN + 1) * 8);
            assert!(bits.try_mark(0));
            let scanners: Mutex<Vec<std::thread::ThreadId>> = Mutex::new(Vec::new());
            let starved = std::sync::atomic::AtomicBool::new(false);
            let scan = |addr: usize, work: &mut Vec<usize>| {
                let i = addr / 8;
                visits[i].fetch_add(1, Ordering::Relaxed);
                if i == 0 {
                    // Long enough that every peer reaches `cv.wait` first.
                    std::thread::sleep(std::time::Duration::from_millis(30));
                    for child in 1..=FAN {
                        if bits.try_mark(child * 8) {
                            work.push(child * 8);
                        }
                    }
                    return;
                }
                let me = std::thread::current().id();
                let distinct = {
                    let mut s = scanners.lock().unwrap_or_else(|e| e.into_inner());
                    if !s.contains(&me) {
                        s.push(me);
                    }
                    s.len()
                };
                if distinct >= 2 {
                    return;
                }
                let t0 = std::time::Instant::now();
                loop {
                    if scanners.lock().unwrap_or_else(|e| e.into_inner()).len() >= 2 {
                        return;
                    }
                    if t0.elapsed() > std::time::Duration::from_secs(10) {
                        starved.store(true, Ordering::Relaxed);
                        return;
                    }
                    std::thread::yield_now();
                }
            };
            let p = if use_pool { Some(&pool) } else { None };
            drain_parallel_on(vec![0], 4, p, scan);
            assert!(
                !starved.load(Ordering::Relaxed),
                "pool={use_pool}: no peer woke for the published work"
            );
            for (i, v) in visits.iter().enumerate() {
                assert_eq!(v.load(Ordering::Relaxed), 1, "node {i}, pool={use_pool}");
            }
        }
    });
}

/// gci i2/st -- a tree-shaped closure (a complete binary tree of 4096
/// nodes, each scan busy for 50 us so the peers have long parked). At the
/// default peer-idle threshold (64) the depth-first local stack never
/// exceeds the tree depth + 1 = 13, so the worker that took the root
/// marks the whole tree alone: EXACTLY one scanning thread, every time. At
/// the early threshold (8, `CRATONVM_GC_YOUNG_MARK_SHARE_EARLY`) the stack
/// passes 8 within the first levels and a parked peer is handed work: at
/// least two scanning threads. Both arms scan every node exactly once.
#[test]
fn st2_a_tree_mark_reaches_a_peer_only_with_early_sharing() {
    st2_within(120, "tree drain", || {
        const N: usize = (1 << 12) - 1;
        let pool = crate::evac_pool::EvacPool::new(3);
        for threshold in [SPILL_WHEN_PEER_IDLE, SPILL_WHEN_PEER_IDLE_EARLY] {
            let visits: Vec<AtomicUsize> = (0..N).map(|_| AtomicUsize::new(0)).collect();
            let bits = YoungMarkBits::new(0, N * 8);
            assert!(bits.try_mark(0));
            let scanners: Mutex<Vec<std::thread::ThreadId>> = Mutex::new(Vec::new());
            let scan = |addr: usize, work: &mut Vec<usize>| {
                let i = addr / 8;
                visits[i].fetch_add(1, Ordering::Relaxed);
                {
                    let me = std::thread::current().id();
                    let mut s = scanners.lock().unwrap_or_else(|e| e.into_inner());
                    if !s.contains(&me) {
                        s.push(me);
                    }
                }
                let t0 = std::time::Instant::now();
                while t0.elapsed() < std::time::Duration::from_micros(50) {
                    std::hint::spin_loop();
                }
                for child in [2 * i + 1, 2 * i + 2] {
                    if child < N && bits.try_mark(child * 8) {
                        work.push(child * 8);
                    }
                }
            };
            drain_parallel_on_with(vec![0], 4, Some(&pool), threshold, scan);
            let distinct = scanners.lock().unwrap_or_else(|e| e.into_inner()).len();
            if threshold == SPILL_WHEN_PEER_IDLE {
                assert_eq!(distinct, 1, "at 64 a tree never reaches a peer");
            } else {
                assert!(distinct >= 2, "at 8 a parked peer must be handed work");
            }
            for (i, v) in visits.iter().enumerate() {
                assert_eq!(v.load(Ordering::Relaxed), 1, "node {i}, threshold {threshold}");
            }
        }
    });
}

/// gci i2/st: `collect_marked` is sized by its counting pass -- one
/// allocation, no regrowth on the collecting thread -- and still returns
/// every mark, ascending.
#[test]
fn st2_collect_marked_allocates_once_for_exactly_the_marks() {
    let bits = YoungMarkBits::new(0x10_0000, 1 << 20);
    let want: Vec<usize> = (0..(1usize << 17)).step_by(3).map(|i| 0x10_0000 + i * 8).collect();
    for &a in &want {
        assert!(bits.try_mark(a));
    }
    let got = bits.collect_marked();
    assert_eq!(got, want);
    assert_eq!(got.capacity(), want.len(), "sized by the count, not regrown");
    assert!(YoungMarkBits::new(0x10_0000, 1 << 20).collect_marked().is_empty());
}
