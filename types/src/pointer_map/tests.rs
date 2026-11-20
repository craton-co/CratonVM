use super::*;

#[test]
fn it_behaves_like_the_hash_map_it_replaced() {
    let mut m = PointerMap::with_capacity(100);
    assert!(m.is_empty());
    for i in 0..1000usize {
        assert_eq!(m.insert(i * 8, i * 8 + 4096), None);
    }
    assert_eq!(m.len(), 1000);
    assert_eq!(m.get(&(8 * 7)), Some(&(56 + 4096)));
    assert_eq!(m[&0], 4096);
    assert!(m.contains_key(&(8 * 999)));
    assert!(!m.contains_key(&(8 * 1000)));
    assert_eq!(m.insert(8, 1), Some(8 + 4096));
    *m.entry(16).or_insert(0) = 2;
    assert_eq!(m.get(&16), Some(&2));
    assert_eq!(m.remove(&16), Some(2));
    assert_eq!(m.iter().count(), 999);
    assert_eq!(m.keys().count(), 999);
    assert_eq!(m.values().filter(|&&v| v == 1).count(), 1);
    for v in m.values_mut() {
        *v += 1;
    }
    assert_eq!(m.get(&8), Some(&2));
    let owned: Vec<(usize, usize)> = m.clone().into_iter().collect();
    assert_eq!(owned.len(), 999);
    let back: PointerMap = owned.into_iter().collect();
    assert_eq!(back, m);
    m.retain(|&k, _| k < 80);
    assert_eq!(m.len(), 9);
    m.clear();
    assert!(m.is_empty());
}

/// g1sz: an indexed lookup finds exactly what the hashed form finds, for
/// keys in, between and outside the index's buckets.
#[test]
fn g1sz_the_bucket_index_answers_what_the_binary_search_did() {
    // Clustered keys (two "regions" far apart) and isolated ones, with
    // probes inside, between and outside every bucket.
    let mut pairs: Vec<(usize, usize)> = Vec::new();
    for i in 0..5000usize {
        pairs.push((0x7000_0000 + i * 24, 0x9000_0000 + i * 24));
        pairs.push((0x7800_0000 + i * 40, 0x9800_0000 + i * 40));
    }
    pairs.push((0x10, 0x20));
    pairs.push((0xffff_fff8, 0x30));
    let hashed: PointerMap = pairs.iter().copied().collect();
    let sorted = PointerMap::from_pairs_sorted(pairs.clone());
    assert!(!sorted.index.starts.is_empty());
    assert!(sorted.index.starts.len() <= sorted.len() + 1);
    for &(k, v) in &pairs {
        assert_eq!(sorted.get(&k), Some(&v), "key {k:#x}");
        for probe in [k.wrapping_sub(8), k + 8, k + 4] {
            assert_eq!(sorted.get(&probe), hashed.get(&probe), "probe {probe:#x}");
        }
    }
    for probe in [0usize, 8, 0x18, 0x7000_0000 - 8, usize::MAX - 7, 0x1_0000_0000] {
        assert_eq!(sorted.get(&probe), hashed.get(&probe), "probe {probe:#x}");
    }
    let one = PointerMap::from_sorted_unique(vec![(0x40, 0x80)]);
    assert_eq!(one.get(&0x40), Some(&0x80));
    assert_eq!(one.get(&0x48), None);
    assert_eq!(one.get(&0x38), None);
}

/// g1sz: `compose_after` answers the same in both forms of `next`.
#[test]
fn g1sz_compose_after_is_form_blind() {
    // acc: a young pause (some targets the full compaction then moved,
    // one identity entry the compaction also moved); next: the compaction.
    let acc_pairs = vec![(0x100, 0x5000), (0x108, 0x5010), (0x110, 0x110), (0x118, 0x6000)];
    let next_pairs = vec![(0x5000, 0x4000), (0x110, 0x3000), (0x7000, 0x2000), (0x5010, 0x5010)];
    let want = {
        let mut m: PointerMap = acc_pairs.iter().copied().collect();
        let next: PointerMap = next_pairs.iter().copied().collect();
        for v in m.values_mut() {
            if let Some(&nv) = next.get(v) {
                *v = nv;
            }
        }
        for (&k, &v) in &next {
            m.entry(k).or_insert(v);
        }
        m
    };
    for acc_sorted in [false, true] {
        for next_sorted in [false, true] {
            let acc = if acc_sorted {
                PointerMap::from_pairs_sorted(acc_pairs.clone())
            } else {
                acc_pairs.iter().copied().collect()
            };
            let next = if next_sorted {
                PointerMap::from_pairs_sorted(next_pairs.clone())
            } else {
                next_pairs.iter().copied().collect()
            };
            let mut got = acc;
            got.compose_after(&next);
            assert_eq!(got, want, "acc_sorted={acc_sorted} next_sorted={next_sorted}");
        }
    }
    assert_eq!(want.get(&0x100), Some(&0x4000));
    assert_eq!(want.get(&0x110), Some(&0x3000));
    assert_eq!(want.get(&0x7000), Some(&0x2000));
}

/// zt t1/rl: the sorted form answers exactly what the hashed form built
/// from the same pairs in order answers (last pair for a key wins), and
/// every mutation converts it without losing an entry.
#[test]
fn zt1_rl_the_sorted_form_equals_the_hashed_form() {
    let pairs: Vec<(usize, usize)> = (0..3000usize)
        .map(|i| (((i * 7919) % 2000) * 8, i))
        .collect();
    let hashed: PointerMap = pairs.iter().copied().collect();
    let sorted = PointerMap::from_pairs_sorted(pairs.clone());
    assert!(sorted.is_sorted_form());
    assert_eq!(sorted.len(), hashed.len());
    assert_eq!(sorted, hashed);
    assert_eq!(hashed, sorted);
    for k in (0..2100usize).map(|i| i * 8) {
        assert_eq!(sorted.get(&k), hashed.get(&k), "key {k}");
        assert_eq!(sorted.contains_key(&k), hashed.contains_key(&k));
    }
    assert!(!sorted.contains_key(&4));
    assert_eq!(sorted.iter().count(), hashed.len());
    assert_eq!(sorted.keys().count(), hashed.len());
    assert_eq!(
        sorted.values().sum::<usize>(),
        hashed.values().sum::<usize>()
    );
    let owned: Vec<(usize, usize)> = sorted.clone().into_iter().collect();
    assert_eq!(owned.len(), hashed.len());
    assert!(sorted.heap_bytes() >= sorted.len() * 16);
    // A mutation keeps every entry: an insert replaces a sorted key in
    // place (gcj pcp), a new key joins the overlay, `retain` thaws.
    let mut m = sorted.clone();
    assert_eq!(m.insert(8, 1), hashed.get(&8).copied());
    assert!(m.is_sorted_form(), "a sorted key is replaced in place");
    assert_eq!(m.insert(0x10_0000, 2), None);
    assert!(!m.is_sorted_form(), "a new key is the overlay's");
    assert_eq!(m.len(), hashed.len() + 1);
    assert_eq!(m.get(&8), Some(&1));
    assert_eq!(m.get(&0x10_0000), Some(&2));
    let mut r = sorted.clone();
    r.retain(|&k, _| k < 80);
    assert_eq!(r.len(), 10);
    let mut c = sorted;
    c.clear();
    assert!(c.is_empty());
    assert!(PointerMap::from_pairs_sorted(Vec::new()).is_empty());
}

/// gcj pcp: a sorted map takes inserts without thawing. Every answer --
/// lookups, `get_mut`, `entry`, `remove`, iteration, `range`, equality,
/// `compose_after` -- is the hashed map's built from the same inserts in
/// the same order, while the keys stay disjoint between the two sides.
#[test]
fn gcj_pcp_inserts_into_a_sorted_map_answer_as_the_hashed_map_does() {
    let base: Vec<(usize, usize)> = (0..500usize).map(|i| (0x1000 + i * 16, 0x9_0000 + i)).collect();
    // Replacements of sorted keys, then keys between and past them.
    let later: Vec<(usize, usize)> = (0..200usize)
        .map(|i| match i % 3 {
            0 => (0x1000 + (i * 7 % 500) * 16, 0x7_0000 + i),
            1 => (0x1008 + i * 16, 0x6_0000 + i),
            _ => (0x10_0000 + i * 8, 0x5_0000 + i),
        })
        .collect();
    let mut overlay = PointerMap::from_pairs_sorted(base.clone());
    let mut hashed: PointerMap = base.iter().copied().collect();
    for &(k, v) in &later {
        assert_eq!(overlay.insert(k, v), hashed.insert(k, v), "insert {k:#x}");
    }
    assert!(!overlay.sorted.is_empty(), "no thaw");
    assert!(
        overlay.sorted.iter().all(|&(k, _)| !overlay.shards[shard_of(k)].contains_key(&k)),
        "no key on both sides"
    );
    assert_eq!(overlay, hashed);
    assert_eq!(overlay.len(), hashed.len());
    assert_eq!(overlay.iter().count(), hashed.len());
    for k in (0x0f00..0x10_0800usize).step_by(8) {
        assert_eq!(overlay.get(&k), hashed.get(&k), "get {k:#x}");
        assert_eq!(overlay.contains_key(&k), hashed.contains_key(&k));
    }
    let range = |m: &PointerMap| {
        let mut v: Vec<(usize, usize)> = m.range(0x1100, 0x10_0100).map(|(&k, &v)| (k, v)).collect();
        v.sort_unstable();
        v
    };
    assert_eq!(range(&overlay), range(&hashed));
    // `get_mut` on either side.
    for k in [0x1000usize, 0x1018, 0x10_0010] {
        *overlay.get_mut(&k).expect("present") += 1;
        *hashed.get_mut(&k).expect("present") += 1;
    }
    assert_eq!(overlay, hashed);
    // A vacant `entry` is the overlay's; an occupied sorted one thaws.
    *overlay.entry(0x20_0000).or_insert(1) += 0;
    *hashed.entry(0x20_0000).or_insert(1) += 0;
    assert!(!overlay.sorted.is_empty());
    *overlay.entry(0x1010).or_insert(3) += 5;
    *hashed.entry(0x1010).or_insert(3) += 5;
    assert_eq!(overlay, hashed);
    // `compose_after` merges both sides in key order.
    let next = PointerMap::from_pairs_sorted(vec![(0x9_0001, 0xa_0000), (0x30_0000, 0xb_0000)]);
    let mut a = PointerMap::from_pairs_sorted(base.clone());
    a.insert(0x10_0008, 0x9_0001);
    let mut b: PointerMap = base.iter().copied().collect();
    b.insert(0x10_0008, 0x9_0001);
    a.compose_after(&next);
    b.compose_after(&next);
    assert_eq!(a, b);
    assert!(a.is_sorted_form());
    assert_eq!(overlay.remove(&0x1000), hashed.remove(&0x1000));
    assert_eq!(overlay, hashed);
}

/// One shard's keys agree in bits 0..7, so plain FxHash (`key * K`) gave
/// them all the same low seven hash bits: one probe start in 128 buckets.
/// [`AddrHasher`]'s low bits must spread them.
#[test]
fn a_shards_keys_spread_over_the_low_hash_bits() {
    use std::hash::BuildHasher;
    let b = BuildHasherDefault::<AddrHasher>::default();
    let fx = BuildHasherDefault::<FxHasher>::default();
    let keys: Vec<usize> = (0..4096usize).map(|k| 0x7f00_0000_0000 + k * 128 + 40).collect();
    let distinct = |f: &dyn Fn(usize) -> u64| {
        let mut seen = std::collections::HashSet::new();
        for &k in &keys {
            seen.insert(f(k) & 1023);
        }
        seen.len()
    };
    assert!(distinct(&|k| fx.hash_one(k)) <= 8, "the defect this replaces");
    assert!(distinct(&|k| b.hash_one(k)) > 900);
    assert!(keys.iter().all(|&k| shard_of(k) == shard_of(keys[0])));
}

#[test]
fn every_shard_is_used_by_adjacent_objects() {
    let mut m = PointerMap::new();
    for i in 0..SHARDS {
        m.insert(i * 8, 1);
    }
    assert!(m.shards.iter().all(|s| s.len() == 1));
}

#[test]
fn the_parallel_fold_equals_the_sequential_one_and_last_pair_wins() {
    let sources: Vec<Vec<(usize, usize)>> = (0..8)
        .map(|w| {
            (0..5000usize)
                .map(|i| ((i * 8) ^ (w * 40), i + w))
                .collect()
        })
        .collect();
    let mut seq = PointerMap::new();
    for s in &sources {
        seq.extend(s.iter().copied());
    }
    for threads in [1, 2, 3, 8, 16, 64] {
        let mut par = PointerMap::new();
        par.par_extend_pairs(&sources, threads);
        assert_eq!(par, seq, "threads={threads}");
    }
    let dup = vec![vec![(8usize, 1usize)], vec![(8, 2)]];
    let mut m = PointerMap::new();
    m.par_extend_pairs(&dup, 4);
    assert_eq!(m.get(&8), Some(&2));
}

/// gce e1/y: the caller-driven fold equals the sequential one, whether the
/// runner runs every job on its own thread, skips half of them, or runs
/// each twice; the last pair for a key still wins.
#[test]
fn the_runner_fold_equals_the_sequential_one_even_when_the_runner_skips_jobs() {
    let sources: Vec<Vec<(usize, usize)>> = (0..8)
        .map(|w| {
            (0..5000usize)
                .map(|i| ((i * 8) ^ (w * 40), i + w))
                .collect()
        })
        .collect();
    let mut seq = PointerMap::new();
    for s in &sources {
        seq.extend(s.iter().copied());
    }
    for threads in [1, 2, 3, 8, 16] {
        let mut all = PointerMap::new();
        all.par_extend_pairs_with(&sources, threads, |n, job| {
            std::thread::scope(|s| {
                for i in 0..n {
                    s.spawn(move || job(i));
                }
            })
        });
        assert_eq!(all, seq, "threads={threads}");
        let mut skipping = PointerMap::new();
        skipping.par_extend_pairs_with(&sources, threads, |n, job| {
            (0..n).step_by(2).for_each(job);
        });
        assert_eq!(
            skipping, seq,
            "threads={threads}: skipped jobs are folded after"
        );
        let mut twice = PointerMap::new();
        twice.par_extend_pairs_with(&sources, threads, |n, job| {
            (0..n).chain(0..n).for_each(job);
        });
        assert_eq!(twice, seq, "threads={threads}: a repeated job is a no-op");
    }
    let dup = vec![vec![(8usize, 1usize)], vec![(8, 2)]];
    let mut m = PointerMap::new();
    m.par_extend_pairs_with(&dup, 4, |n, job| (0..n).rev().for_each(job));
    assert_eq!(m.get(&8), Some(&2));
}

/// g1ep7: [`PointerMap::compose_after_par`] builds the map
/// [`PointerMap::compose_after`] builds, sorted form and bucket index
/// included, for every job count, on maps past the parallel threshold.
#[test]
fn g1ep7_the_parallel_compose_answers_the_serial_one() {
    let n = PAR_INDEX_MIN + 777;
    // Pass 1: moves and self-forwards; pass 2 moves some pass-1
    // destinations and adds keys, some of them colliding with pass 1's.
    let first: Vec<(usize, usize)> = (0..n)
        .map(|i| {
            let k = 0x1000_0000 + i * 64;
            (k, if i % 7 == 0 { k } else { 0x8000_0000 + i * 48 })
        })
        .collect();
    let second: Vec<(usize, usize)> = {
        let mut v: Vec<(usize, usize)> = (0..n)
            .filter(|i| i % 3 != 1)
            .map(|i| {
                let k = if i % 5 == 0 { 0x1000_0000 + i * 64 } else { 0x8000_0000 + i * 48 };
                (k, 0x2_0000_0000 + i * 32)
            })
            .collect();
        v.sort_unstable();
        v.dedup_by_key(|p| p.0);
        v
    };
    let mut expect = PointerMap::from_pairs_sorted(first.clone());
    let next = PointerMap::from_pairs_sorted(second);
    expect.compose_after(&next);
    for jobs in [1usize, 2, 3, 8, 16] {
        let mut got = PointerMap::from_pairs_sorted(first.clone());
        got.compose_after_par(&next, jobs, &|n, job| {
            std::thread::scope(|s| {
                for i in 0..n {
                    s.spawn(move || job(i));
                }
            })
        });
        assert!(got.is_sorted_form(), "jobs={jobs}");
        assert_eq!(got, expect, "jobs={jobs}");
        for &(k, _) in first.iter().step_by(97) {
            assert_eq!(got.get(&k), expect.get(&k), "jobs={jobs} key {k:#x}");
        }
    }
}

/// g1ep: [`PointerMap::compose_after`] answers what the hashed compose
/// loop does (chase each first-pass value one hop, add a second-pass key
/// only when absent), in each of the four form pairings, and leaves the
/// map in the sorted form.
#[test]
fn g1ep_compose_after_answers_the_hashed_compose_loop() {
    // Pass 1: moves, identity self-forwards, and keys pass 2 reuses.
    let first: Vec<(usize, usize)> = (0..3000usize)
        .map(|i| {
            let k = 0x10_0000 + i * 64;
            let v = if i % 7 == 0 { k } else { 0x80_0000 + i * 48 };
            (k, v)
        })
        .collect();
    // Pass 2: moves some pass-1 destinations (and self-forwards), and
    // names fresh keys -- some colliding with pass-1 keys.
    let second: Vec<(usize, usize)> = (0..3000usize)
        .filter(|i| i % 3 != 1)
        .map(|i| {
            let k = if i % 5 == 0 { 0x10_0000 + i * 64 } else { 0x80_0000 + i * 48 };
            (k, 0x200_0000 + i * 32)
        })
        .chain((0..500usize).map(|i| (0x400_0000 + i * 16, 0x500_0000 + i * 16)))
        .collect();
    let expect = {
        let mut acc: PointerMap = first.iter().copied().collect();
        let next: PointerMap = second.iter().copied().collect();
        for v in acc.values_mut() {
            if let Some(&nv) = next.get(v) {
                *v = nv;
            }
        }
        for (&k, &v) in &next {
            acc.entry(k).or_insert(v);
        }
        acc
    };
    for (sorted_first, sorted_second) in [(false, false), (true, false), (false, true), (true, true)] {
        let build = |pairs: &[(usize, usize)], sorted: bool| {
            if sorted {
                PointerMap::from_pairs_sorted(pairs.to_vec())
            } else {
                pairs.iter().copied().collect::<PointerMap>()
            }
        };
        let mut acc = build(&first, sorted_first);
        let next = build(&second, sorted_second);
        acc.compose_after(&next);
        assert!(acc.is_sorted_form(), "{sorted_first}/{sorted_second}: sorted form");
        assert_eq!(acc, expect, "{sorted_first}/{sorted_second}");
    }
    // An empty second pass changes nothing, form included.
    let mut acc: PointerMap = first.iter().copied().collect();
    acc.compose_after(&PointerMap::new());
    assert!(!acc.is_sorted_form());
    assert_eq!(acc.len(), first.len());
}

/// g1pin: `range(lo, hi)` answers exactly the entries whose key lies in
/// `[lo, hi)`, in either form, at the edges and outside every key.
#[test]
fn g1pin_range_answers_the_keys_in_the_interval_in_either_form() {
    let pairs: Vec<(usize, usize)> = (1..=40).map(|i| (i * 8, i * 1000)).collect();
    let sorted = PointerMap::from_pairs_sorted(pairs.clone());
    let mut hashed = PointerMap::new();
    for &(k, v) in &pairs {
        hashed.insert(k, v);
    }
    assert!(sorted.is_sorted_form() && !hashed.is_sorted_form());
    for (lo, hi) in [(0, 8), (8, 16), (40, 41), (40, 200), (0, usize::MAX), (400, 4000), (320, 328), (200, 40)] {
        let want: Vec<(usize, usize)> =
            pairs.iter().copied().filter(|&(k, _)| k >= lo && k < hi).collect();
        for map in [&sorted, &hashed] {
            let mut got: Vec<(usize, usize)> = map.range(lo, hi).map(|(&k, &v)| (k, v)).collect();
            got.sort_unstable();
            assert_eq!(got, want, "[{lo}, {hi}) sorted_form={}", map.is_sorted_form());
        }
    }
}

/// g1ep3: the parallel index fill builds the serial index, for dense and
/// sparse keys, every job count (more jobs than pairs included), and jobs
/// run in reverse order.
#[test]
fn g1ep3_the_parallel_index_is_the_serial_index() {
    let reverse = |jobs: usize, job: &(dyn Fn(usize) + Sync)| (0..jobs).rev().for_each(job);
    let mut x = 0x9e37_79b9_7f4a_7c15u64;
    let mut next = move || {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        x as usize
    };
    for (n, stride) in [(1usize, 8usize), (2, 8), (70_000, 8), (70_000, 4096), (90_001, 24)] {
        let mut keys: Vec<usize> = Vec::with_capacity(n);
        let mut k = 0x4000_0000usize;
        for _ in 0..n {
            k += 8 + (next() % stride) / 8 * 8;
            keys.push(k);
        }
        let pairs: Vec<(usize, usize)> = keys.iter().map(|&k| (k, k ^ 0x5555_0000)).collect();
        let serial = SortedIndex::build(&pairs, Vec::new());
        for jobs in [1usize, 2, 3, 7, 16, n + 5] {
            let par = SortedIndex::build_par(&pairs, jobs, &reverse, Vec::new());
            assert_eq!((par.base, par.shift), (serial.base, serial.shift), "n={n} jobs={jobs}");
            assert_eq!(par.starts, serial.starts, "n={n} jobs={jobs}");
        }
        let m = PointerMap::from_sorted_unique_par(pairs.clone(), 5, &reverse);
        for &(k, v) in pairs.iter().step_by(97) {
            assert_eq!(m.get(&k), Some(&v));
            assert_eq!(m.get(&(k + 4)), None);
        }
        assert_eq!(m.len(), n);
    }
}

/// g1ep4: a pooled map is the unpooled map; dropping it hands both
/// buffers back, the next build reuses them (stale contents and all), and
/// a pooled composition is the unpooled one.
#[test]
fn g1ep4_a_pooled_map_reuses_its_buffers_and_answers_the_same() {
    let reverse = |jobs: usize, job: &(dyn Fn(usize) + Sync)| (0..jobs).rev().for_each(job);
    let pool = std::sync::Arc::new(SortedBufferPool::new(64 << 20));
    let pairs_of = |n: usize, stride: usize, tag: usize| -> Vec<(usize, usize)> {
        (0..n).map(|i| (0x4000_0000 + i * stride, tag + i)).collect()
    };
    let a = pairs_of(PAR_INDEX_MIN + 77, 24, 1);
    let mut buf = pool.take_pairs(a.len());
    buf.extend_from_slice(&a);
    let m = PointerMap::from_sorted_unique_par_in(buf, 4, &reverse, Some(&pool));
    assert_eq!(m, PointerMap::from_sorted_unique(a.clone()));
    let (pairs_ptr, held) = (m.sorted.as_ptr(), m.heap_bytes());
    assert_eq!(pool.held_bytes(), 0);
    drop(m);
    assert_eq!(pool.held_bytes(), held, "both buffers kept");
    // A smaller, sparser map into the same buffers.
    let b = pairs_of(PAR_INDEX_MIN + 5, 4096, 9);
    let mut buf = pool.take_pairs(b.len());
    assert_eq!(buf.as_ptr(), pairs_ptr, "the kept pair buffer");
    buf.extend_from_slice(&b);
    let m = PointerMap::from_sorted_unique_par_in(buf, 3, &reverse, Some(&pool));
    assert_eq!(m.index.starts, SortedIndex::build(&b, Vec::new()).starts);
    for &(k, v) in b.iter().step_by(31) {
        assert_eq!(m.get(&k), Some(&v));
        assert_eq!(m.get(&(k + 8)), None);
    }
    // Serial build, composition, thaw.
    let c = pairs_of(500, 16, 3);
    let mut buf = pool.take_pairs(c.len());
    buf.extend_from_slice(&c);
    let mut pooled = PointerMap::from_sorted_unique_in(buf, Some(&pool));
    let mut plain = PointerMap::from_sorted_unique(c.clone());
    let next = PointerMap::from_pairs_sorted(pairs_of(300, 16, 7));
    pooled.compose_after(&next);
    plain.compose_after(&next);
    assert_eq!(pooled, plain);
    assert!(pooled.buffer_pool().is_some(), "the composition keeps the pool");
    pooled.insert(1, 2);
    plain.insert(1, 2);
    assert_eq!(pooled, plain);
    drop((m, pooled));
    assert!(pool.held_bytes() > 0);
    // Nothing over the cap is kept.
    let tiny = std::sync::Arc::new(SortedBufferPool::new(1024));
    drop(PointerMap::from_sorted_unique_in(pairs_of(1000, 8, 0), Some(&tiny)));
    assert_eq!(tiny.held_bytes(), 0);
}
