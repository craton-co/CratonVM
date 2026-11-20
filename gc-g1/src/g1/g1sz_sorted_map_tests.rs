use super::*;

/// A `run` that hands the jobs to real threads, in reverse order, so the
/// passes cannot lean on job order.
fn threaded(jobs: usize, job: &(dyn Fn(usize) + Sync)) {
    std::thread::scope(|s| {
        for i in (0..jobs).rev() {
            s.spawn(move || job(i));
        }
    });
}

#[test]
fn the_region_bucket_sort_answers_what_the_hashed_fold_does() {
    let base = 0x4000_0000usize;
    let shift = 20u32;
    let regions = 64usize;
    // Copy-order forwards over many regions, with repeated keys whose LAST
    // pair must win (as `insert` in order keeps it).
    let mut forwards: Vec<(usize, usize)> = Vec::new();
    let mut x = 0x2545_f491_4f6c_dd1du64;
    for i in 0..20_000usize {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        let r = (x as usize) % regions;
        let off = ((x >> 32) as usize % ((1 << shift) / 8)) * 8;
        forwards.push((base + (r << shift) + off, 0x9000_0000 + i * 8));
    }
    let dup = forwards[17].0;
    forwards.push((dup, 0xdead_0000));
    let hashed: cratonvm_types::PointerMap = forwards.iter().copied().collect();
    // g1ep4: every build after the first reuses the previous one's buffers.
    let kept = Arc::new(cratonvm_types::pointer_map::SortedBufferPool::new(64 << 20));
    for threads in [1usize, 3, 8, 8] {
        let sorted = sorted_pointer_map_by_region(
            &[&forwards],
            base,
            shift,
            regions,
            threads,
            &threaded,
            Some(&kept),
        );
        assert_eq!(sorted, hashed, "threads={threads}");
        assert_eq!(sorted.get(&dup), Some(&0xdead_0000));
        assert!(sorted.is_sorted_form());
    }
    // A key outside the arena falls back to the serial build, same answer.
    let mut stray = forwards.clone();
    stray.push((0x10, 0x20));
    let expect: cratonvm_types::PointerMap = stray.iter().copied().collect();
    assert_eq!(
        sorted_pointer_map_by_region(&[&stray], base, shift, regions, 4, &threaded, None),
        expect
    );
    // g1ep3: the same pairs cut into uneven sources (one empty) build the
    // concatenation's map, a key repeated across sources keeping its last
    // pair.
    let mut tail = vec![(dup, 0xbeef_0000)];
    tail.extend_from_slice(&forwards[..5]);
    let mut all = forwards.clone();
    all.extend_from_slice(&tail);
    let expect: cratonvm_types::PointerMap = all.iter().copied().collect();
    let sources: [&[(usize, usize)]; 4] =
        [&forwards[..7001], &[], &forwards[7001..], &tail];
    for threads in [3usize, 8, 40] {
        let sorted =
            sorted_pointer_map_by_region(
                &sources, base, shift, regions, threads, &threaded, None,
            );
        assert_eq!(sorted, expect, "threads={threads}");
        assert_eq!(sorted.get(&dup), Some(&0xbeef_0000));
    }
}
