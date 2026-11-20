use super::*;
use std::sync::atomic::AtomicU8;

#[test]
fn t1_w7_g2_effective_workers_scale_with_the_walked_old_gen() {
    assert_eq!(effective_workers(0, 10_000_000, 0), 0);
    assert_eq!(effective_workers(4, 1000, PAR_MARK_MIN_OBJECTS), 0);
    assert_eq!(effective_workers(4, PAR_MARK_MIN_OBJECTS, PAR_MARK_MIN_OBJECTS), 2);
    assert_eq!(effective_workers(4, 600_000, PAR_MARK_MIN_OBJECTS), 2);
    assert_eq!(effective_workers(4, 800_000, PAR_MARK_MIN_OBJECTS), 3);
    assert_eq!(effective_workers(4, 4_000_000, PAR_MARK_MIN_OBJECTS), 4);
    assert_eq!(effective_workers(7, 10, 0), 2);
}

#[test]
fn workers_from_value() {
    assert_eq!(par_mark_workers_from(None, 8), 4);
    assert_eq!(par_mark_workers_from(Some("0"), 8), 0);
    assert_eq!(par_mark_workers_from(Some("off"), 8), 0);
    assert_eq!(par_mark_workers_from(Some("1"), 8), 4);
    assert_eq!(par_mark_workers_from(Some("on"), 2), 2);
    assert_eq!(par_mark_workers_from(Some("7"), 2), 7);
    assert_eq!(par_mark_workers_from(Some("99"), 2), PAR_MARK_MAX_WORKERS);
    assert_eq!(par_mark_workers_from(Some("junk"), 8), 0);
}

/// A random graph over `n` nodes (index-encoded as fake addresses
/// `(i + 1) * 8`): the drain on 1, 2, 3 and 7 workers marks exactly the
/// nodes reachable from the seed, scans each exactly once, and the
/// per-worker states hold every scanned node exactly once.
#[test]
fn t1_g2_the_parallel_drain_reaches_exactly_the_serial_closure_once_each() {
    let n = 20_000usize;
    let mut rng = 0x9E37_79B9_7F4A_7C15u64;
    let mut next = || {
        rng ^= rng << 13;
        rng ^= rng >> 7;
        rng ^= rng << 17;
        rng
    };
    let edges: Vec<Vec<usize>> = (0..n)
        .map(|i| {
            let k = (next() % 4) as usize;
            // Node 0 is a wide "array"; others have 0-3 random edges, and
            // every 7th node is unreachable garbage (no one points at it
            // except other garbage).
            if i == 0 {
                (1..(PAR_ARRAY_CHUNK * 2 + 77).min(n)).collect()
            } else {
                (0..k)
                    .map(|_| (next() as usize) % n)
                    .filter(|&t| t % 7 != 3 || i % 7 == 3)
                    .collect()
            }
        })
        .collect();
    // Serial reference closure.
    let mut want = vec![false; n];
    let mut st = vec![0usize];
    want[0] = true;
    while let Some(i) = st.pop() {
        for &t in &edges[i] {
            if !want[t] {
                want[t] = true;
                st.push(t);
            }
        }
    }
    for threads in [1usize, 2, 3, 7] {
        let marks: Vec<AtomicU8> = (0..n).map(|_| AtomicU8::new(0)).collect();
        let scans: Vec<AtomicU8> = (0..n).map(|_| AtomicU8::new(0)).collect();
        let chunk_slots = AtomicUsize::new(0);
        marks[0].store(1, Ordering::Relaxed);
        let addr = |i: usize| ((i + 1) * 8) as *mut u8;
        let idx = |p: *mut u8| p as usize / 8 - 1;
        let (states, counts) = drain(
            vec![addr(0)],
            Vec::new(),
            threads,
            true,
            Vec::<usize>::new,
            |p, stack, mine: &mut Vec<usize>| {
                let i = idx(p);
                scans[i].fetch_add(1, Ordering::Relaxed);
                mine.push(i);
                // Node 0 (the wide one) is split into slot ranges.
                if i == 0 {
                    return edges[0].len();
                }
                for &t in &edges[i] {
                    if marks[t]
                        .compare_exchange(0, 1, Ordering::AcqRel, Ordering::Acquire)
                        .is_ok()
                    {
                        stack.push(addr(t));
                    }
                }
                0
            },
            |p, s, e, stack, _mine: &mut Vec<usize>| {
                assert_eq!(idx(p), 0, "only node 0 is split");
                chunk_slots.fetch_add(e - s, Ordering::Relaxed);
                for &t in &edges[0][s..e] {
                    if marks[t]
                        .compare_exchange(0, 1, Ordering::AcqRel, Ordering::Acquire)
                        .is_ok()
                    {
                        stack.push(addr(t));
                    }
                }
            },
        );
        assert_eq!(
            chunk_slots.load(Ordering::Relaxed),
            edges[0].len(),
            "threads={threads}: every slot range of the split array ran once"
        );
        assert_eq!(states.len(), threads.max(1));
        let got: Vec<bool> = marks
            .iter()
            .map(|m| m.load(Ordering::Relaxed) == 1)
            .collect();
        assert_eq!(got, want, "threads={threads}: marked set differs");
        for i in 0..n {
            let s = scans[i].load(Ordering::Relaxed);
            assert_eq!(
                s, want[i] as u8,
                "threads={threads}: node {i} scanned {s} times"
            );
        }
        let mut all: Vec<usize> = states.into_iter().flatten().collect();
        all.sort_unstable();
        let reach: Vec<usize> = (0..n).filter(|&i| want[i]).collect();
        assert_eq!(all, reach, "threads={threads}: per-worker outputs");
        assert_eq!(counts.scanned, reach.len());
    }
}

/// An empty seed terminates at once on every width.
#[test]
fn t1_g2_an_empty_drain_terminates() {
    for threads in [1usize, 2, 5] {
        let (states, counts) = drain(
            Vec::new(),
            Vec::new(),
            threads,
            false,
            || 0u8,
            |_, _, _| 0,
            |_, _, _, _, _| {},
        );
        assert_eq!(states.len(), threads.max(1));
        assert_eq!(counts.scanned, 0);
    }
}
