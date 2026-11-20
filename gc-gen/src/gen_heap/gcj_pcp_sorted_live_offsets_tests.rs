use super::*;

/// gcj pcp: the sliced sort and merge answer exactly what the rebuild's
/// old sort, dead-pinned retain and offset map answered -- serially, and on
/// a pool at more slices than it has helpers.
#[test]
fn the_sliced_sort_answers_what_the_serial_sort_retain_and_map_did() {
    let base = 0x7000_0000usize;
    // Interleaved ascending runs, as the workers' claimed buffers leave
    // them: 300 runs of 400 distinct 16-byte survivors.
    let mut live: Vec<(usize, usize)> = Vec::new();
    for run in 0..300usize {
        for j in 0..400usize {
            live.push((base + (j * 300 + run) * 16, 16));
        }
    }
    let mut dead: Vec<usize> = live.iter().step_by(97).map(|&(a, _)| a).collect();
    dead.sort_unstable();
    let mut want = live.clone();
    want.sort();
    want.retain(|&(a, _)| dead.binary_search(&a).is_err());
    let want: Vec<(usize, usize)> = want.iter().map(|&(a, s)| (a - base, s)).collect();
    assert!(live.len() >= PAR_LIVE_SORT_MIN, "the fixture must take the pool");
    let pool = cratonvm_gc_core::evac_pool::EvacPool::new(3);
    for (threads, pool) in [(1, None), (4, Some(&pool)), (9, Some(&pool))] {
        let mut l = live.clone();
        assert_eq!(sorted_live_offsets(&mut l, base, &dead, threads, pool), want, "threads={threads}");
    }
    let mut none_dead = live.clone();
    assert_eq!(
        sorted_live_offsets(&mut none_dead, base, &[], 4, Some(&pool)).len(),
        live.len()
    );
}
