use super::*;

/// How much of the parallel marker's per-cycle handicap is the POOL
/// ITSELF: spawning `n` OS threads at `begin`, and stopping and joining
/// them at `drop`.
///
/// Asked before building a persistent pool, because the arithmetic decides
/// whether one is worth building. The 2026-09-02 re-measurement put the
/// parallel marker ~2 ms/cycle behind the serial loop at four workers and
/// ~6 ms behind at one; if construction is a small fraction of that, a
/// persistent pool cannot close it and the cost is in the coordination
/// protocol instead.
#[test]
#[ignore = "timing measurement; wants --release and a quiet box"]
fn measure_the_pool_construction_cost() {
    const ROUNDS: usize = 20;
    let mut graph = rustc_hash::FxHashMap::default();
    graph.insert(1u64, vec![2u64]);
    graph.insert(2u64, vec![]);
    let ctx: Arc<dyn ZMarkContext> = Arc::new(TestMarkContext::new(graph));

    for workers in [1usize, 2, 4, 8] {
        let start = std::time::Instant::now();
        for _ in 0..ROUNDS {
            let pool = ZMarkCoordinator::new(Arc::clone(&ctx), workers);
            pool.begin_cycle();
            pool.end_cycle();
            drop(pool);
        }
        let per = start.elapsed() / ROUNDS as u32;
        eprintln!("[pool-cost] workers={workers} construct+begin+end+drop = {per:?} per cycle");
    }
}
