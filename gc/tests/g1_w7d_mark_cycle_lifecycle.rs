// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Lane W7-D — the mark cycle that starts and never finishes, and the census
//! that can tell which of the two reasons it did not.
//!
//! # What these tests pin, and what they deliberately cannot
//!
//! The defect is a MISSING CALLER. `marking_complete` is set in exactly one
//! place — the tail of `G1Collector::cleanup` — which is reached from exactly
//! one VM function, `interpreter::g1_final_remark_cleanup`, whose callers
//! before this lane were `maybe_gc`'s epilogue, the OPT-IN JIT mark driver,
//! and the pre-OOM ladder. The allocation-failure pause (`maybe_gc_forced_at`),
//! which is where G1 takes most of its pauses, called none of them.
//!
//! That wiring lives in `vm/src/runtime/interpreter/gc_and_alloc.rs` and this
//! crate cannot see it. What this crate CAN pin is the property that makes a
//! missing caller fatal rather than merely slow, and it is the half that was
//! never written down as a test:
//!
//! **an open cycle is a LATCH.** Nothing in the collector closes it on its
//! own. `g1_is_marking_active()` stays true for as long as no caller runs the
//! finish half — including long after the background marker has drained to a
//! fixed point and `g1_concurrent_mark_finished()` answers `true` — and every
//! production start site is guarded on `!g1_is_marking_active()`, so one
//! unclosed cycle suppresses every later one for the life of the process.
//!
//! So the shape being tested is: *the marker finishing is not the cycle
//! finishing.* A test that only asserted "a cycle completes when you complete
//! it" would pass against the defect.
//!
//! # And the control
//!
//! Every assertion below has its opposite arm in the same test: the latch test
//! then RUNS the finish half and shows the latch opens, and the census test
//! asserts the untouched doors stayed at zero. README §5 rule 8 — a
//! verification step needs a control as much as an experiment does; "the
//! collector never completes a cycle" and "this test cannot observe a
//! completion" must not read alike.

use cratonvm_gc::collector::StopTheWorldToken;
use cratonvm_gc::g1::{
    mark_door_census, mark_door_report, mark_pause_totals, note_mark_door, MarkDoor,
    MarkDoorOutcome,
};
use cratonvm_gc::{GcBackend, VmHeap};

const HEAP_BYTES: usize = 8 * 1024 * 1024;

/// SAFETY: single-threaded test harness; no mutator is running.
fn stw() -> StopTheWorldToken {
    unsafe { StopTheWorldToken::new() }
}

fn g1_heap() -> VmHeap {
    VmHeap::new(GcBackend::G1, HEAP_BYTES)
}

/// Poll `g1_concurrent_mark_finished()` to a deadline.
///
/// Bounded rather than spun forever for the reason
/// `g1_concurrent::worker_stops_promptly_when_parked` gives: a hang is
/// unbounded, so any finite bound catches it, and a loose bound keeps the
/// assertion about behaviour rather than about how busy this host is.
fn wait_for_marker_to_drain(heap: &VmHeap) -> bool {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while std::time::Instant::now() < deadline {
        if heap.g1_concurrent_mark_finished() {
            return true;
        }
        std::thread::yield_now();
    }
    false
}

/// THE LATCH. A cycle whose background marker has drained is still an OPEN
/// cycle, and stays open until something runs the finish half.
///
/// This is the whole of `w6m-a-workload-that-mixes.md` §4's
/// `remark_pauses=1, cleanup_pauses=0` reading, at collector level: the
/// marking-active flag does not clear itself, `cleanup` does not run itself,
/// and `marking_complete` is therefore never set. Every production start site
/// is guarded on `!g1_is_marking_active()`, so this state is not "one cycle
/// delayed" — it is every future cycle refused.
#[test]
fn a_drained_marker_does_not_close_the_cycle() {
    let heap = g1_heap();
    assert!(
        !heap.g1_is_marking_active(),
        "fixture precondition: a fresh heap must not be mid-cycle",
    );

    heap.g1_start_concurrent_mark(&stw());
    assert!(
        heap.g1_is_marking_active(),
        "g1_start_concurrent_mark did not open a cycle — the rest of this test \
         would pass vacuously",
    );

    assert!(
        wait_for_marker_to_drain(&heap),
        "the background marker did not reach a fixed point within 5 s on an \
         empty heap; this test cannot say anything about the latch until it \
         does",
    );

    // THE ASSERTION. The marker is done. Nothing else happens.
    let (_, _, cleanup_before, _) = mark_pause_totals();
    for _ in 0..64 {
        std::thread::yield_now();
    }
    assert!(
        heap.g1_is_marking_active(),
        "the cycle closed itself — if this ever becomes true, the missing-caller \
         defect this lane fixed is no longer reachable and \
         CRATONVM_G1_ALLOC_MARK_DRIVE can be reconsidered",
    );
    let (_, _, cleanup_after, _) = mark_pause_totals();
    assert_eq!(
        cleanup_after, cleanup_before,
        "a cleanup pause ran without anyone calling the finish half (delta \
         asserted, not an absolute: this counter is process-global and the rest \
         of the suite shares it)",
    );

    // THE CONTROL, in the same test: run the finish half and the latch opens.
    // Without this arm, a `g1_start_concurrent_mark` that silently did nothing
    // would satisfy every assertion above.
    let completed = heap.g1_final_remark_and_cleanup(&stw(), &[], None);
    assert!(
        completed,
        "the finish half declined a cycle it had just been shown was active",
    );
    assert!(
        !heap.g1_is_marking_active(),
        "the finish half ran and the cycle is still open",
    );
    let (_, _, cleanup_final, _) = mark_pause_totals();
    assert!(
        cleanup_final > cleanup_after,
        "cleanup_pauses did not move across a completed cycle \
         ({cleanup_after} -> {cleanup_final}) — the engagement counter every \
         marking A/B in this round reads first would be lying",
    );
}

/// An open cycle blocks the NEXT one, which is why one missing call costs a
/// whole process rather than one cycle.
///
/// The conjunction every production start site uses is
/// `g1_should_start_marking() && !g1_is_marking_active()`
/// (`interpreter::g1_drive_mark_cycle`, and before it three hand-written
/// copies). This test pins the second conjunct's behaviour over a cycle's
/// lifetime, since it is the term that never becomes true again on a build
/// with no finish-half caller.
#[test]
fn an_open_cycle_suppresses_the_start_guard_until_cleanup_runs() {
    let heap = g1_heap();

    // Before: the guard admits.
    assert!(
        !heap.g1_is_marking_active(),
        "fixture precondition: nothing open",
    );

    heap.g1_start_concurrent_mark(&stw());
    assert!(
        wait_for_marker_to_drain(&heap),
        "marker did not drain; the state under test was never reached",
    );

    // During: the guard refuses, and it refuses for a reason that has nothing
    // to do with occupancy — a heap that crossed IHOP a thousand times over
    // would still be refused here.
    assert!(
        heap.g1_is_marking_active(),
        "the start guard's blocking term cleared itself",
    );

    // After: the guard admits again, and ONLY because cleanup ran.
    assert!(heap.g1_final_remark_and_cleanup(&stw(), &[], None));
    assert!(
        !heap.g1_is_marking_active(),
        "cleanup ran and the start guard is still blocked",
    );
}

/// The door census counts what it says it counts, and prints the zeroes.
///
/// Delta-asserted (house rule 8): `MARK_DOORS` is process-global and the rest
/// of this binary's tests run as threads in the same process. The assertion is
/// that the door written to moved by one in exactly one column, and that the
/// DIFFERENCE between two reads carries nothing else — not that any absolute
/// value is a particular number.
#[test]
fn the_door_census_moves_one_column_of_one_door() {
    let before = mark_door_census();
    note_mark_door(MarkDoor::AllocFail, MarkDoorOutcome::Waiting);
    let after = mark_door_census();

    assert_eq!(
        before.len(),
        5,
        "the census must report every door, including the ones a run never \
         reaches — a door that was never visited IS the finding",
    );
    assert_eq!(after.len(), before.len());

    for (b, a) in before.iter().zip(after.iter()) {
        assert_eq!(b.0, a.0, "door order is not stable between reads");
        if a.0 == "alloc_fail" {
            assert_eq!(a.1 - b.1, 1, "visits did not move for the door written to");
            assert_eq!(a.4 - b.4, 1, "waiting did not move");
            assert_eq!(a.2 - b.2, 0, "idle moved and should not have");
            assert_eq!(a.3 - b.3, 0, "started moved and should not have");
            assert_eq!(a.5 - b.5, 0, "finished moved and should not have");
            assert_eq!(a.6 - b.6, 0, "lost_stw moved and should not have");
        }
        // No `else` arm asserting the other doors are unchanged: a concurrent
        // test in this binary may legitimately drive one. What must hold is
        // that `visits` is the sum of the five outcome columns, for EVERY row,
        // at every instant — that is the invariant a miscounted door breaks.
        assert_eq!(
            a.1,
            a.2 + a.3 + a.4 + a.5 + a.6,
            "door {} reports visits={} against outcomes summing to {}",
            a.0,
            a.1,
            a.2 + a.3 + a.4 + a.5 + a.6,
        );
    }
}

/// The report line names every door and every column, so a reader who has
/// never seen this code can tell "no driver ran" from "a driver ran and was
/// refused" without reading the source.
///
/// The control is the part that matters: this asserts a token that has been in
/// the report since the report existed (`visits=`) alongside the new ones, so
/// a formatting change that emptied the line cannot pass as "the doors were
/// all zero". That is README §5 rule 8 applied to a string check — the
/// `strings -a` failure it is drawn from returned zero for `UseG1GC` too.
#[test]
fn the_door_report_carries_every_door_and_every_column() {
    let report = mark_door_report();
    for door in [
        "maybe_gc",
        "alloc_fail",
        "jit_driver",
        "force_full",
        "system_gc",
    ] {
        assert!(
            report.contains(&format!("door={door} ")),
            "mark-door report is missing door={door}:\n{report}",
        );
    }
    for column in [
        "visits=",
        "idle=",
        "started=",
        "waiting=",
        "finished=",
        "lost_stw=",
    ] {
        assert!(
            report.contains(column),
            "mark-door report is missing column {column}:\n{report}",
        );
    }
    assert_eq!(
        report.lines().count(),
        5,
        "one line per door, no more and no fewer:\n{report}",
    );
}
