//! gen r4w5/thrash5: the mutator-progress half of the GC-overhead limit.
use super::*;
use crate::config::{GcAlgorithm, VmConfig};
use std::sync::atomic::Ordering;

/// The measured heap: `-Xmx128m` Generational, one 32 MiB semi-space
/// plus a 64 MiB old generation.
const CAP: usize = 100_663_296;

fn window(
    allocated: u64,
    prev_live_after: usize,
    live_after: usize,
) -> Option<MutatorProgress> {
    Some(MutatorProgress {
        allocated,
        prev_live_after,
        live_after,
    })
}

/// HIB-GCOVERHEAD-HALFFULL.1 is untouched: with room in the old
/// generation nothing is ever unproductive, progress or not.
#[test]
fn a_heap_with_old_gen_room_is_never_unproductive() {
    assert!(!gc_cycle_is_unproductive(0, CAP, false, None));
    assert!(!gc_cycle_is_unproductive(0, CAP, false, window(0, 10, 10)));
}

/// With no window (first forced collection, or the switch off) the verdict
/// is the historical freed-bytes one, byte for byte.
#[test]
fn without_a_window_the_verdict_is_the_freed_bytes_one() {
    let two_percent = CAP / 50;
    assert!(gc_cycle_is_unproductive(two_percent - 8, CAP, true, None));
    assert!(!gc_cycle_is_unproductive(two_percent + 8, CAP, true, None));
}

/// The measured thrash (`GenR4W4HeapFullThrashProbe` step `threads`):
/// 13 carves (3 407 872 bytes, 3.4 %) of filler freed per cycle, a few
/// kilobytes linked, live flat at 13.5 MB young + 64 MiB old.
#[test]
fn filler_freed_without_progress_is_unproductive() {
    let live = 13_565_952 + 67_108_856;
    assert!(gc_cycle_is_unproductive(
        3_407_872,
        CAP,
        true,
        window(20_000, live, live)
    ));
    // The live set creeping up (the few hundred linked bytes) is still flat.
    assert!(gc_cycle_is_unproductive(
        3_407_872,
        CAP,
        true,
        window(20_000, live, live + 65_536)
    ));
    // ...and so is a fall of less than 2 % of capacity.
    assert!(gc_cycle_is_unproductive(
        3_407_872,
        CAP,
        true,
        window(20_000, live, live - CAP / 50 + 8),
    ));
}

/// A program that allocated 2 % of capacity or more since the previous
/// forced collection made progress, however full the heap. (2 % of `CAP`
/// is 2 013 265.92 bytes, so `CAP / 50` is still under it.)
#[test]
fn real_allocation_is_progress() {
    let live = 90_000_000;
    let two_percent = (CAP / 50) as u64;
    assert!(!gc_cycle_is_unproductive(
        3_407_872,
        CAP,
        true,
        window(two_percent + 1, live, live)
    ));
    assert!(gc_cycle_is_unproductive(
        3_407_872,
        CAP,
        true,
        window(two_percent, live, live)
    ));
}

/// A program that caught the error and DROPPED its data has allocated
/// almost nothing since, but its live set fell: not this arm's loop.
#[test]
fn a_program_that_dropped_its_data_is_not_caught() {
    assert!(!gc_cycle_is_unproductive(
        30_000_000,
        CAP,
        true,
        window(1_000, 95_000_000, 66_000_000),
    ));
    // A fall of just over 2 % of capacity counts as a fall.
    assert!(!gc_cycle_is_unproductive(
        3_407_872,
        CAP,
        true,
        window(1_000, 90_000_000, 90_000_000 - CAP / 50 - 1),
    ));
}

/// The window is per VM, opens at the first forced collection, and
/// measures the larger of the two allocation counters' deltas.
#[test]
fn the_progress_window_is_per_vm_and_takes_the_larger_counter() {
    let shared = crate::vm::SharedVm::new(VmConfig {
        gc_algorithm: GcAlgorithm::Generational,
        ..VmConfig::default()
    });
    assert_eq!(
        sample_mutator_progress(&shared, 1_000),
        None,
        "no window before the first"
    );
    shared
        .mem
        .bytes_allocated_total
        .fetch_add(4_096, Ordering::Relaxed);
    shared
        .mem
        .thread_allocated_total
        .fetch_add(10_000, Ordering::Relaxed);
    assert_eq!(
        sample_mutator_progress(&shared, 2_000),
        Some(MutatorProgress {
            allocated: 10_000,
            prev_live_after: 1_000,
            live_after: 2_000,
        })
    );
    assert_eq!(
        sample_mutator_progress(&shared, 2_000).map(|p| p.allocated),
        Some(0),
        "each collection closes its window"
    );
    let other = crate::vm::SharedVm::new(VmConfig::default());
    assert!(!other.mem.gc_progress.armed.load(Ordering::Relaxed));
}


/// zr z2/cp (rv3): on ZGC a wedged window counts only when the live set
/// kept at least 1/16 of it. The crawl (40 % kept) counts at any size; a
/// steady program (live flat, or noise) does not, however small its
/// windows; an empty window never does.
#[test]
fn zr2cp_the_zgc_crawl_rule_separates_retaining_from_steady_windows() {
    let w = |allocated: u64, prev: usize, after: usize| {
        Some(MutatorProgress {
            allocated,
            prev_live_after: prev,
            live_after: after,
        })
    };
    let live = 66_439_000usize;
    // `grow`: 48 KB window, ~40 % retained.
    assert!(zgc_window_is_crawl(w(48 * 1024, live, live + 19_660)));
    // A large window that retained 40 % is still the crawl.
    assert!(zgc_window_is_crawl(w(4 << 20, live, live + (1_677_721))));
    // NearFullSteady 975 at 64m (debug): live identical every cycle.
    assert!(!zgc_window_is_crawl(w(514_800, live, live)));
    assert!(!zgc_window_is_crawl(w(257_920, live, live)));
    // The live set fell: a program dropping data.
    assert!(!zgc_window_is_crawl(w(514_800, live, live - 4096)));
    // Noise below 1/16 of the window.
    let tip = 1_200_000 / 16;
    assert!(!zgc_window_is_crawl(w(1_200_000, live, live + tip - 1)));
    assert!(zgc_window_is_crawl(w(1_200_000, live, live + tip)));
    // Nothing allocated in between (several refused threads, a boundary
    // relief): never the crawl.
    assert!(!zgc_window_is_crawl(w(0, live, live + 8)));
    assert!(!zgc_window_is_crawl(None));
}

/// zr z2/cp: the clear-all soft pass on ZGC picks exactly the condemned
/// referents a conservative compiled-frame word names; a referent no word
/// names (kept by a real heap path) and a null referent are left to the
/// ordinary `is_marked` verdict.
#[test]
fn zr2cp_only_conservatively_named_condemned_referents_are_forced_dead() {
    // addr-keyed: test-local fabricated words, never a live heap.

    // addr-keyed: test data (literal words, no heap).
    let words: std::collections::HashSet<usize> = [0x1000, 0x2000].into_iter().collect();
    let condemned = [(0x10, 0x1000), (0x20, 0x3000), (0x30, 0)];
    assert_eq!(soft_referents_named_by(&condemned, &words), vec![0x1000]);
    assert!(soft_referents_named_by(&[], &words).is_empty());
    assert!(soft_referents_named_by(&condemned, &Default::default()).is_empty());
}

/// zr z7/sp: a condemned referent that a PRECISE root of the initiator
/// names directly (a static whose value is the referent) is claimed even
/// when a conservative word holds the same address; one named only inside
/// the conservative frame scan, only by the initiator's own earlier
/// deposit (step 11, which carries that scan's conservative words: the
/// `SoftRefBeforeOome` regression a first cut had), or only by a peer's
/// snapshot root (past `initiator_end`), is not; nothing is claimed
/// without recorded ranges (another VM's scan).
#[test]
fn zr7sp_precise_root_values_are_claimed_by_provenance() {
    // SAFETY: the values are only compared, never dereferenced.
    let r = |a: usize| unsafe { ObjectRef::from_raw(a as *mut u8) };
    // [0] static 0x1000 | [1] own deposit 0x5000 | [2..4) frame words
    // 0x1000, 0x2000 | [4] shadow 0x4000 | [5] peer 0x3000
    let roots = [
        r(0x1000),
        r(0x5000),
        r(0x1000),
        r(0x2000),
        r(0x4000),
        r(0x3000),
    ];
    let cands = [0x1000, 0x2000, 0x3000, 0x4000, 0x5000, 0];
    let ranges = Some([(1, 2), (2, 4)]);
    assert_eq!(
        soft_precise_root_named(&cands, &roots, 5, ranges),
        vec![0x1000, 0x4000]
    );
    // The collector's value-only split defers every root a conservative
    // word also names, the static's 0x1000 included (the defect): with
    // the static itself inside an excluded range it is not claimed.
    assert!(
        !soft_precise_root_named(&cands, &roots, 5, Some([(0, 2), (2, 4)])).contains(&0x1000)
    );
    assert!(soft_precise_root_named(&cands, &roots, 5, None).is_empty());
    assert!(soft_precise_root_named(&[], &roots, 5, ranges).is_empty());
    // No range recorded (a precise-only scan, no deposit): every
    // initiator root is precise.
    assert_eq!(
        soft_precise_root_named(&cands, &roots, 5, Some([(0, 0); 2])),
        vec![0x1000, 0x2000, 0x4000, 0x5000]
    );
}

/// zr z2/cp: off ZGC, or without the clear-all rule, or with the kill
/// switch, the pass forces nothing.
#[test]
fn zr2cp_the_conservative_soft_pass_is_zgc_clear_all_only() {
    let g1 = crate::vm::SharedVm::new(VmConfig {
        gc_algorithm: GcAlgorithm::G1,
        ..VmConfig::default()
    });
    let rp = g1.mem.ref_processor.lock();
    assert!(zgc_soft_clear_all_conservative_referents(&g1, &rp, true).is_empty());
    drop(rp);
    #[cfg(feature = "zgc")]
    {
        let z = crate::vm::SharedVm::new(VmConfig {
            gc_algorithm: GcAlgorithm::Zgc,
            ..VmConfig::default()
        });
        let rp = z.mem.ref_processor.lock();
        assert!(zgc_soft_clear_all_conservative_referents(&z, &rp, false).is_empty());
        // Nothing condemned: nothing forced, rule or not.
        assert!(zgc_soft_clear_all_conservative_referents(&z, &rp, true).is_empty());
    }
}

/// gen r4w6/review6: the progress half is Generational-only, the backend
/// its evidence came from. On G1 the marks are still recorded, but no
/// window is ever handed to the verdict, so it stays the historical
/// freed-bytes one.
#[test]
fn the_progress_window_is_generational_only() {
    let g1 = crate::vm::SharedVm::new(VmConfig {
        gc_algorithm: GcAlgorithm::G1,
        ..VmConfig::default()
    });
    assert_eq!(sample_mutator_progress(&g1, 1_000), None);
    g1.mem
        .bytes_allocated_total
        .fetch_add(4_096, Ordering::Relaxed);
    assert_eq!(sample_mutator_progress(&g1, 1_000), None, "no window on G1");
    assert!(
        g1.mem.gc_progress.armed.load(Ordering::Relaxed),
        "the marks are kept"
    );
}

/// gci i2/oo (`gci-i1pp-overhead-verdict-bookkeeping-runs-after-the-release`
/// item 1): a window closed on counters sealed inside the pause credits
/// nothing the released mutators allocated after the seal to the window
/// that closed; the next window starts at the seal and gets it.
#[test]
fn gci_i2oo_a_sealed_window_credits_nothing_allocated_after_the_seal() {
    let shared = crate::vm::SharedVm::new(VmConfig {
        gc_algorithm: GcAlgorithm::Generational,
        ..VmConfig::default()
    });
    assert_eq!(sample_mutator_progress_at(&shared, 1_000, Some((0, 0))), None);
    // The mutators allocate 4 KiB, the pause seals, then the released
    // mutators' refills publish another MiB before the verdict runs.
    shared
        .mem
        .bytes_allocated_total
        .fetch_add(4_096, Ordering::Relaxed);
    let sealed = (
        shared.mem.bytes_allocated_total.load(Ordering::Relaxed),
        shared.mem.thread_allocated_total.load(Ordering::Relaxed),
    );
    shared
        .mem
        .bytes_allocated_total
        .fetch_add(1 << 20, Ordering::Relaxed);
    assert_eq!(
        sample_mutator_progress_at(&shared, 1_000, Some(sealed)).map(|p| p.allocated),
        Some(4_096),
        "the post-release burst is not this window's"
    );
    assert_eq!(
        sample_mutator_progress(&shared, 1_000).map(|p| p.allocated),
        Some(1 << 20),
        "it opens the next window"
    );
}

/// gci i2/oo item 3: only a pause newer than the newest one judged is
/// judged.
#[test]
fn gci_i2oo_only_a_newer_pause_is_judged() {
    assert!(sealed_verdict_is_newer(0, 1));
    assert!(sealed_verdict_is_newer(4, 5));
    assert!(!sealed_verdict_is_newer(5, 5));
    assert!(!sealed_verdict_is_newer(5, 4));    }