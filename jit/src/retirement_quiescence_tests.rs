use super::{
    classify_thread_quiescence, BlockedStackSummary, JitThreadQuiescence,
    ThreadQuiescenceEvidence,
};
use std::sync::atomic::{AtomicU32, AtomicU64, AtomicUsize};
use std::sync::Arc;

fn evidence(min_running_gen: u64, blocked: Vec<(u64, Vec<usize>)>) -> ThreadQuiescenceEvidence {
    ThreadQuiescenceEvidence {
        min_running_gen,
        min_running_ic_gen: min_running_gen,
        blocked,
    }
}

/// A thread record in a chosen state. `blocked` is `Some(buffers)` for a
/// thread parked inside compiled code whose stack scan COMPLETED.
fn thread_record(
    depth: usize,
    quiescent_gen: u64,
    blocked: Option<Vec<usize>>,
) -> Arc<JitThreadQuiescence> {
    let valid = blocked.is_some();
    Arc::new(JitThreadQuiescence {
        depth: AtomicUsize::new(depth),
        quiescent_gen: AtomicU64::new(quiescent_gen),
        ic_quiescent_gen: AtomicU64::new(0),
        blocked_nesting: AtomicU32::new(u32::from(valid)),
        blocked: parking_lot::Mutex::new(BlockedStackSummary {
            valid,
            buffers: blocked.unwrap_or_default(),
            pending: None,
        }),
    })
}

/// A thread inside a blocked window whose scan did NOT complete — the band
/// was larger than `collect_code_buffers_in_band` will walk — so the
/// partial buffer list is not a statement about its stack.
fn thread_record_with_unusable_scan(
    depth: usize,
    quiescent_gen: u64,
    partial: Vec<usize>,
) -> Arc<JitThreadQuiescence> {
    Arc::new(JitThreadQuiescence {
        depth: AtomicUsize::new(depth),
        quiescent_gen: AtomicU64::new(quiescent_gen),
        ic_quiescent_gen: AtomicU64::new(0),
        blocked_nesting: AtomicU32::new(1),
        blocked: parking_lot::Mutex::new(BlockedStackSummary {
            valid: false,
            buffers: partial,
            pending: None,
        }),
    })
}

/// nio21 wave 17 L3: a lazy summary's band is scanned by the drain, under
/// the summary lock, against the region snapshot captured at the
/// transition -- and answers what the eager scan answers for the same
/// band (sorted, deduplicated buffer starts; the last-region memo agrees
/// with the binary search).
#[test]
fn w17_l3_lazy_blocked_summary_is_resolved_by_the_drain() {
    let regions = Arc::new(vec![(0x1000usize, 0x2000usize), (0x5000, 0x6000)]);
    let band: [usize; 7] = [0x1004, 7, 0x5008, 0x1ff0, 0x3000, 0x5000, 0x1004];
    let lo = band.as_ptr() as usize;
    let hi = lo + std::mem::size_of_val(&band);
    let lazy = Arc::new(JitThreadQuiescence {
        depth: AtomicUsize::new(1),
        quiescent_gen: AtomicU64::new(3),
        ic_quiescent_gen: AtomicU64::new(0),
        blocked_nesting: AtomicU32::new(1),
        blocked: parking_lot::Mutex::new(BlockedStackSummary {
            valid: true,
            buffers: vec![0xdead],
            pending: Some(super::PendingBlockedBand {
                lo,
                hi,
                regions: Arc::clone(&regions),
            }),
        }),
    });
    let e = classify_thread_quiescence(0, std::slice::from_ref(&lazy)).expect("evidence");
    assert_eq!(e.blocked, vec![(3, vec![0x1000, 0x5000])]);
    assert_eq!(
        e.min_running_gen,
        u64::MAX,
        "a resolved band is blocked evidence"
    );
    // Resolved once: the band is gone and the answer stays.
    assert!(lazy.blocked.lock().pending.is_none());
    let again = classify_thread_quiescence(0, std::slice::from_ref(&lazy)).expect("evidence");
    assert_eq!(again.blocked, vec![(3, vec![0x1000, 0x5000])]);
    // The eager scan over the same band and snapshot agrees.
    let mut eager = Vec::new();
    // SAFETY: `band` is a live local array.
    assert!(unsafe { super::scan_code_buffers_in_band(lo, hi, &regions, &mut eager) });
    assert_eq!(eager, vec![0x1000, 0x5000]);
    std::hint::black_box(&band);

    // A band too large to scan leaves the thread counted as running.
    let too_large = Arc::new(JitThreadQuiescence {
        depth: AtomicUsize::new(1),
        quiescent_gen: AtomicU64::new(4),
        ic_quiescent_gen: AtomicU64::new(0),
        blocked_nesting: AtomicU32::new(1),
        blocked: parking_lot::Mutex::new(BlockedStackSummary {
            valid: true,
            buffers: Vec::new(),
            pending: Some(super::PendingBlockedBand {
                lo: 0x10_0000,
                hi: 0x10_0000 + super::MAX_BLOCKED_BAND_BYTES + 8,
                regions,
            }),
        }),
    });
    let e = classify_thread_quiescence(0, std::slice::from_ref(&too_large)).expect("evidence");
    assert!(e.blocked.is_empty());
    assert_eq!(e.min_running_gen, 4);
}

// -- 1. `permits_stamp` as a pure predicate --------------------------

/// Arm (a): the running bound is a veto, not a vote. A body queued after
/// some running thread's last return to depth 0 is held whatever the
/// blocked evidence says — including blocked evidence that would permit it
/// on its own. Catches a rewrite of the `&&` into an `||`.
#[test]
fn a_body_retired_after_a_running_threads_last_return_is_never_released() {
    assert!(!evidence(4, Vec::new()).permits_stamp(5, 0x1000));
    assert!(!evidence(4, vec![(9, vec![0x2000])]).permits_stamp(5, 0x1000));
    // Equality releases: the thread stood at depth 0 AT the stamp, which
    // is after the caller unpublished the body.
    assert!(evidence(5, Vec::new()).permits_stamp(5, 0x1000));
}

/// Arm (b), the "it has been out since" arm: a blocked thread whose
/// `quiescent_gen` is at or past the body's stamp permits it without the
/// buffer list being consulted at all — here the list names the body and
/// it is released anyway. Without this arm a parked thread would pin every
/// body whose address a scan older than the retirement still mentions.
#[test]
fn a_blocked_thread_out_since_the_stamp_permits_whatever_its_stack_holds() {
    assert!(evidence(u64::MAX, vec![(7, vec![0x1000])]).permits_stamp(7, 0x1000));
    // One generation earlier and the list decides instead.
    assert!(!evidence(u64::MAX, vec![(6, vec![0x1000])]).permits_stamp(7, 0x1000));
}

/// Arm (c), and the boundaries `binary_search` really has.
///
/// `0x1001` is the important one: one byte past a start that IS on the
/// list. It must PERMIT, because the list holds identities and not ranges.
/// A future rewrite that reintroduced `[entry, entry + len)` containment
/// here — the design `NOTES-runtime.md` RT-3 assumed — would answer "held"
/// and fail this line.
#[test]
fn a_blocked_threads_buffer_list_is_searched_by_exact_start() {
    let e = evidence(
        u64::MAX,
        vec![(1, vec![0x1000usize, 0x3000, 0x5000, 0x9000])],
    );
    for present in [0x1000usize, 0x5000, 0x9000] {
        assert!(
            !e.permits_stamp(9, present),
            "{present:#x} is on the blocked thread's list and must pin the body"
        );
    }
    for absent in [0x0fffusize, 0x1001, 0x4000, 0x9001, usize::MAX] {
        assert!(
            e.permits_stamp(9, absent),
            "{absent:#x} is not a recorded buffer START, so nothing on that \
             stack can return into it"
        );
    }
}

/// An empty buffer list permits. A thread can be inside compiled code by
/// depth and have no word of its scanned band point into any executable
/// buffer — parked in a helper, say. `binary_search` on an empty slice is
/// `Err(0)`, which is the answer this relies on, and a rewrite to
/// `!buffers.is_empty() && ..` would invert it and pin the whole queue.
#[test]
fn a_blocked_thread_whose_scan_found_no_code_permits() {
    assert!(evidence(u64::MAX, vec![(1, Vec::new())]).permits_stamp(9, 0x1000));
}

/// With no blocked threads the decision is `min_running_gen` alone —
/// `all` over an empty iterator is `true`. Pinned because it is what makes
/// the first conjunct load-bearing.
#[test]
fn no_blocked_threads_leaves_the_running_bound_in_charge() {
    assert!(evidence(9, Vec::new()).permits_stamp(9, 0x1000));
    assert!(!evidence(8, Vec::new()).permits_stamp(9, 0x1000));
}

/// `all`, not `any`. Two blocked threads, the first permitting on every
/// ground and the second holding the buffer: the body stays. An `any`
/// would pass every single-thread case above and free bodies under the
/// second thread here, which is the shape this test exists to catch.
#[test]
fn one_blocked_thread_that_holds_the_body_is_enough_to_keep_it() {
    let e = evidence(u64::MAX, vec![(99, vec![0x1000]), (1, vec![0x1000])]);
    assert!(!e.permits_stamp(9, 0x1000));
}

/// Staleness (the note's item 2, restated against this predicate): a
/// summary five generations older than the body may not release it on the
/// strength of its generation. Only genuine absence from the list may.
#[test]
fn a_summary_older_than_the_body_releases_only_on_absence() {
    let e = evidence(u64::MAX, vec![(2, vec![0x1000])]);
    assert!(!e.permits_stamp(7, 0x1000), "present on a stale list: held");
    assert!(
        e.permits_stamp(7, 0x2000),
        "absent from that same list: free"
    );
}

// -- 2. the fail-closed arm, one function up -------------------------

/// The fail-closed edge, and the one a test can get wrong in the
/// safe-looking direction. `None` and "evidence with nothing in it" are
/// OPPOSITES: the latter has `min_running_gen == u64::MAX` and no blocked
/// threads, so it permits every body in the queue. This asserts both — the
/// untracked arm answers `None`, and the value it would otherwise have
/// returned releases everything.
#[test]
fn an_untracked_jit_execution_yields_no_evidence_at_all() {
    assert!(classify_thread_quiescence(1, &[]).is_none());
    let running = thread_record(1, 3, None);
    assert!(classify_thread_quiescence(1, std::slice::from_ref(&running)).is_none());

    let empty = classify_thread_quiescence(0, &[])
        .expect("zero untracked executions must yield evidence");
    assert!(
        empty.permits_stamp(u64::MAX, 0x1000),
        "a process with no thread inside compiled code releases everything — \
         which is exactly why the untracked arm must answer `None` and not \
         this value"
    );
}

/// A thread at depth 0 vouches for itself and contributes nothing. Its
/// `quiescent_gen` must NOT be folded into `min_running_gen`: one
/// long-idle thread would otherwise hold the entire queue for the life of
/// the process, which is the bug the per-thread records were added for.
#[test]
fn a_thread_at_depth_zero_contributes_nothing() {
    let idle = thread_record(0, 0, None);
    let e = classify_thread_quiescence(0, std::slice::from_ref(&idle)).unwrap();
    assert_eq!(e.min_running_gen, u64::MAX);
    assert!(e.blocked.is_empty());
    assert!(e.permits_stamp(9, 0x1000));
}

/// The second inversion. `valid == false` means the scan did not complete,
/// so the partial buffer list says nothing about this thread's stack. It
/// must be routed into `min_running_gen`, where it BOUNDS releases, and
/// never onto `blocked`, where the same thread would instead permit every
/// body its partial list happens to miss. `0x2000` below is exactly such a
/// body: the blocked arm would have released it.
#[test]
fn a_blocked_window_with_an_unusable_scan_counts_as_running() {
    let torn = thread_record_with_unusable_scan(1, 4, vec![0x1000]);
    let e = classify_thread_quiescence(0, std::slice::from_ref(&torn)).unwrap();
    assert!(
        e.blocked.is_empty(),
        "an incomplete scan is not evidence of quiescence"
    );
    assert_eq!(e.min_running_gen, 4);
    assert!(!e.permits_stamp(5, 0x2000));
    assert!(e.permits_stamp(4, 0x2000));
}

/// A valid blocked summary is carried through with its own generation, and
/// does not bound the running side. Both halves matter: folding a blocked
/// thread into `min_running_gen` would be safe but would give back the
/// whole benefit, and dropping its generation would lose arm (b).
#[test]
fn a_valid_blocked_summary_is_carried_through_with_its_generation() {
    let parked = thread_record(1, 4, Some(vec![0x1000, 0x3000]));
    let e = classify_thread_quiescence(0, std::slice::from_ref(&parked)).unwrap();
    assert_eq!(
        e.min_running_gen,
        u64::MAX,
        "a blocked thread must not bound the running side"
    );
    assert_eq!(e.blocked, vec![(4u64, vec![0x1000usize, 0x3000])]);
}

/// A running thread with a miss-handler stamp (round 14 wave 2, lane mic,
/// M8-1). `blocked` as in [`thread_record`].
fn thread_record_with_ic_stamp(
    depth: usize,
    quiescent_gen: u64,
    ic_quiescent_gen: u64,
    blocked: Option<Vec<usize>>,
) -> Arc<JitThreadQuiescence> {
    let record = thread_record(depth, quiescent_gen, blocked);
    record
        .ic_quiescent_gen
        .store(ic_quiescent_gen, std::sync::atomic::Ordering::Relaxed);
    record
}

/// M8-1's two minima must never be confused. A thread spinning in compiled
/// code that last returned to depth 0 at 3 and entered a miss handler at 9
/// bounds the inline-cache grace at 9 and code release at 3: a body retired
/// at 5 may still be on its stack, so `permits_stamp` (the release
/// decision, which reads `min_running_gen` only) must hold it.
#[test]
fn a_miss_handler_stamp_bounds_the_ic_grace_but_never_code_release() {
    let spinner = thread_record_with_ic_stamp(1, 3, 9, None);
    let e = classify_thread_quiescence(0, std::slice::from_ref(&spinner)).unwrap();
    assert_eq!(e.min_running_gen, 3);
    assert_eq!(e.min_running_ic_gen, 9);
    assert!(
        !e.permits_stamp(5, 0x1000),
        "a miss-handler stamp is not evidence the thread left the body"
    );
}

/// The later stamp wins in either order, and the minimum is over threads:
/// one thread with no miss-handler stamp at all bounds the grace at its
/// depth-0 stamp, which is the pre-M8-1 value (what the kill switch
/// `CRATONVM_JIT_IC_QUIESCENT_STAMP=0` leaves: every stamp `0`).
#[test]
fn the_ic_bound_is_the_minimum_over_threads_of_the_later_stamp() {
    let stale_ic = thread_record_with_ic_stamp(1, 7, 2, None);
    let e = classify_thread_quiescence(0, std::slice::from_ref(&stale_ic)).unwrap();
    assert_eq!(e.min_running_ic_gen, 7);

    let fresh = thread_record_with_ic_stamp(1, 1, 12, None);
    let unstamped = thread_record(2, 4, None);
    let e = classify_thread_quiescence(0, &[fresh, unstamped]).unwrap();
    assert_eq!(e.min_running_gen, 1);
    assert_eq!(e.min_running_ic_gen, 4);

    let never = thread_record(1, 6, None);
    let e = classify_thread_quiescence(0, std::slice::from_ref(&never)).unwrap();
    assert_eq!(e.min_running_ic_gen, e.min_running_gen);
}

/// Blocked and idle threads bound neither minimum, whatever their
/// miss-handler stamp says.
#[test]
fn blocked_and_idle_threads_leave_the_ic_bound_alone() {
    let parked = thread_record_with_ic_stamp(1, 2, 3, Some(vec![0x1000]));
    let idle = thread_record_with_ic_stamp(0, 0, 0, None);
    let e = classify_thread_quiescence(0, &[parked, idle]).unwrap();
    assert_eq!(e.min_running_gen, u64::MAX);
    assert_eq!(e.min_running_ic_gen, u64::MAX);
}

/// The whole reason the per-thread proof exists, end to end over the two
/// functions: a pool worker parked forever inside compiled code releases
/// every body it is not parked inside. Before these records,
/// `ACTIVE_JIT_EXECUTIONS` stayed non-zero and the queue never drained
/// until the code-cache cap refused every compile.
#[test]
fn a_worker_parked_inside_compiled_code_does_not_pin_unrelated_bodies() {
    let parked = thread_record(1, 2, Some(vec![0xAAAA_0000]));
    let e = classify_thread_quiescence(0, std::slice::from_ref(&parked)).unwrap();
    assert!(
        e.permits_stamp(7, 0xBBBB_0000),
        "an unrelated body must be releasable while the worker stays parked"
    );
    assert!(
        !e.permits_stamp(7, 0xAAAA_0000),
        "the body it is parked inside must not be"
    );
}
