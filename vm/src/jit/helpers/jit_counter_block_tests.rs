use super::*;

/// The tallies moved off process-global `fetch_add` and into per-thread
/// blocks to stop paying a `lock xadd` per dispatch. The whole claim is
/// that they stayed EXACT while doing it, so assert exactly that: every
/// increment from every thread, including threads that have since exited,
/// must be visible to the reader.
///
/// Asserted on a DELTA rather than an absolute, because these are
/// process-global counters and the rest of this binary's tests dispatch
/// natives of their own.
#[test]
fn every_thread_s_increments_survive_and_are_summed() {
    const THREADS: usize = 4;
    const PER_THREAD: u64 = 10_000;

    let before_walks = membership_walks_by_site()
        .into_iter()
        .find(|(n, _)| *n == MEMBERSHIP_WALK_SITE_NAMES[1])
        .map(|(_, v)| v)
        .unwrap_or(0);
    let before_leaf = leaf_native_hit_count();

    let handles: Vec<_> = (0..THREADS)
        .map(|_| {
            std::thread::spawn(|| {
                for _ in 0..PER_THREAD {
                    note_membership_walk(1);
                    note_leaf_native_hit();
                }
            })
        })
        .collect();
    for h in handles {
        h.join().expect("worker panicked");
    }
    // Every worker has EXITED by now. Its block is still registered, which
    // is what a whole-run census needs.
    let after_walks = membership_walks_by_site()
        .into_iter()
        .find(|(n, _)| *n == MEMBERSHIP_WALK_SITE_NAMES[1])
        .map(|(_, v)| v)
        .unwrap_or(0);
    let after_leaf = leaf_native_hit_count();

    assert_eq!(
        after_walks - before_walks,
        PER_THREAD * THREADS as u64,
        "membership-walk tally lost increments across threads"
    );
    assert_eq!(
        after_leaf - before_leaf,
        PER_THREAD * THREADS as u64,
        "leaf-hit tally lost increments across threads"
    );
}

/// A site index past the end of the block is ignored, exactly as the
/// `MEMBERSHIP_WALK_BY_SITE.get(site)` it replaced ignored it — this is
/// the arm that kept an out-of-range site from panicking a release build.
///
/// Read through THIS THREAD'S OWN block, not through
/// `leaf_native_hit_count()`, which sums every registered block in the
/// process. `cargo test` runs tests concurrently and several of them reach
/// compiled-dispatch code, so the global sum moves under this test for
/// reasons that have nothing to do with the out-of-range site — it failed
/// that way at 8121 against an expected 8072. A thread's own counter is the
/// only reading no other test can perturb, and it is the reading this test
/// always meant: the question is whether a bad index lands on a
/// NEIGHBOURING FIELD of the same block.
#[test]
fn an_out_of_range_site_is_ignored_rather_than_panicking() {
    fn my_leaf_hits() -> u64 {
        JIT_COUNTERS.with(|block| {
            block
                .leaf_native_hits
                .load(std::sync::atomic::Ordering::Relaxed)
        })
    }
    let before = my_leaf_hits();
    note_membership_walk(MEMBERSHIP_WALK_SITE_NAMES.len());
    note_membership_walk(usize::MAX);
    assert_eq!(
        my_leaf_hits(),
        before,
        "an out-of-range site must not land on a neighbouring counter"
    );
}

/// Every name has a counter behind it. The two arrays are index-parallel
/// by contract and nothing else checks it now that the counters live in a
/// struct rather than beside the names.
#[test]
fn the_block_has_one_counter_per_site_name() {
    let block = JitCounterBlock::default();
    assert_eq!(
        block.membership_walk.len(),
        MEMBERSHIP_WALK_SITE_NAMES.len(),
        "site names and per-thread counters must stay index-parallel"
    );
}

/// The `VarHandle` direct-helper pairs moved off six process-global
/// `lock xadd` counters (r9-vmside). Same claim as above: still EXACT,
/// summed across threads that have exited, and one kind never lands on
/// another's counter. Read on THIS thread's block for the cross-kind check,
/// because other tests in the binary may drive real VarHandle helpers.
#[test]
fn varhandle_direct_counters_are_per_thread_and_exact() {
    const THREADS: usize = 3;
    const PER_THREAD: u64 = 5_000;
    let before = vh_direct_counts(VH_DIRECT_CAS);
    let before_bypass = jit_funnel_bypass_count();
    let handles: Vec<_> = (0..THREADS)
        .map(|_| {
            std::thread::spawn(|| {
                for _ in 0..PER_THREAD {
                    note_vh_direct(VH_DIRECT_CAS, true);
                    note_funnel_bypass_hit();
                }
                note_vh_direct(VH_DIRECT_CAS, false);
            })
        })
        .collect();
    for h in handles {
        h.join().expect("worker panicked");
    }
    // `>=`, not `==`: these are process-wide sums and a concurrently
    // running test may drive a real VarHandle helper. A LOST increment —
    // the failure this guards — still reads short.
    let after = vh_direct_counts(VH_DIRECT_CAS);
    assert!(after.0 - before.0 >= PER_THREAD * THREADS as u64);
    assert!(after.1 - before.1 >= THREADS as u64);
    assert!(jit_funnel_bypass_count() - before_bypass >= PER_THREAD * THREADS as u64);

    // Kinds are separate, and an out-of-range kind is ignored.
    let mine = |kind: usize| {
        JIT_COUNTERS
            .with(|b| b.vh_direct_served[kind].load(std::sync::atomic::Ordering::Relaxed))
    };
    let (r0, w0, c0) = (
        mine(VH_DIRECT_READ),
        mine(VH_DIRECT_WRITE),
        mine(VH_DIRECT_CAS),
    );
    note_vh_direct(VH_DIRECT_WRITE, true);
    note_vh_direct(usize::MAX, true);
    assert_eq!(mine(VH_DIRECT_READ), r0);
    assert_eq!(mine(VH_DIRECT_WRITE), w0 + 1);
    assert_eq!(mine(VH_DIRECT_CAS), c0);
}
