use super::*;

const MIB: usize = 1 << 20;

#[test]
fn only_a_restored_heap_waits_before_its_first_pass() {
    assert_eq!(start_delay(false), std::time::Duration::from_secs(1));
    assert!(start_delay(true) >= std::time::Duration::from_millis(50));
}

#[test]
fn the_window_defaults_on_linux_only_and_parses_mib() {
    assert_eq!(prefault_ahead_from(None, true), 8 * MIB);
    assert_eq!(prefault_ahead_from(None, false), 0);
    assert_eq!(prefault_ahead_from(Some("16"), false), 16 * MIB);
    assert_eq!(prefault_ahead_from(Some(" 8 "), true), 8 * MIB);
    for off in ["0", "off", "false", "no", "OFF"] {
        assert_eq!(prefault_ahead_from(Some(off), true), 0, "{off}");
    }
    assert_eq!(prefault_ahead_from(Some("lots"), true), 8 * MIB);
}

#[test]
fn the_next_span_starts_at_the_later_of_cursor_and_done() {
    // Fresh: from the cursor, one step.
    assert_eq!(
        next_span(100, 0, 64 * MIB, 2 * MIB, 1 << 40),
        Some((100, 100 + 2 * MIB))
    );
    // Already populated past the cursor: continue from there.
    assert_eq!(
        next_span(100, 10 * MIB, 64 * MIB, 2 * MIB, 1 << 40),
        Some((10 * MIB, 12 * MIB))
    );
    // The window is covered.
    assert_eq!(next_span(0, 64 * MIB, 64 * MIB, 2 * MIB, 1 << 40), None);
    // The last step is clipped to the window...
    assert_eq!(
        next_span(0, 63 * MIB, 64 * MIB, 2 * MIB, 1 << 40),
        Some((63 * MIB, 64 * MIB))
    );
    // ... and to the bump region's end.
    assert_eq!(next_span(0, 0, 64 * MIB, 2 * MIB, MIB), Some((0, MIB)));
    assert_eq!(next_span(5 * MIB, 0, 64 * MIB, 2 * MIB, 5 * MIB), None);
    // No window, nothing to do.
    assert_eq!(next_span(0, 0, 0, 2 * MIB, 1 << 40), None);
}

#[test]
fn the_committed_run_stops_at_the_first_uncommitted_granule() {
    // Granules 0..3 committed, 3 not, 4 committed.
    let bits: Vec<AtomicU64> = vec![AtomicU64::new(0b10111)];
    let g = GRANULE;
    assert_eq!(committed_run_end(&bits, 0, 10 * g), 3 * g);
    assert_eq!(committed_run_end(&bits, g + 5, 2 * g), 2 * g);
    assert_eq!(committed_run_end(&bits, 3 * g, 5 * g), 3 * g);
    assert_eq!(committed_run_end(&bits, 4 * g, 5 * g), 5 * g);
    // Past the map: nothing is committed.
    assert_eq!(committed_run_end(&bits, 64 * g, 65 * g), 64 * g);
}

/// The behaviour a user sees: on a reserved heap with a startup commit, a
/// carve wakes nothing synchronously, but a pass populates the committed
/// window past the cursor, stops at the window, and the bytes stay zero.
#[test]
fn a_pass_populates_the_committed_window_ahead_of_the_cursor() {
    let heap = ZgcRealHeap::with_capacity_and_initial(256 * MIB, 64 * MIB);
    heap.counters
        .fp
        .prefault
        .ahead
        .store(8 * MIB, Ordering::Relaxed);
    let cursor = heap.arena.lock().used_low_for_compaction();
    let got = heap.prefault_pass();
    assert!(got > 0, "populated nothing");
    assert!(got <= 8 * MIB + GRANULE, "populated past the window: {got}");
    assert_eq!(heap.prefaulted_bytes(), got);
    let upto = heap.counters.fp.prefault.upto.load(Ordering::Relaxed);
    assert!(
        upto >= cursor + 8 * MIB,
        "window not covered: {upto} from {cursor}"
    );
    // Covered: a second pass has nothing to do.
    assert_eq!(heap.prefault_pass(), 0);
    // Contents unchanged: the populated bytes still read zero.
    let base = heap.arena_base as *const u8;
    for off in (cursor..upto).step_by(4096) {
        // SAFETY: committed (the startup commit) and inside the arena.
        assert_eq!(unsafe { *base.add(off) }, 0, "byte at {off}");
    }
}

/// Nothing past the commit: a heap whose startup commit ends below the
/// window populates up to the commit and stops there.
#[test]
fn a_pass_never_crosses_into_uncommitted_granules() {
    let heap = ZgcRealHeap::with_capacity_and_initial(256 * MIB, 4 * MIB);
    heap.counters
        .fp
        .prefault
        .ahead
        .store(64 * MIB, Ordering::Relaxed);
    let committed_before = heap.os_committed_bytes();
    // `commit_initial_zgc` splits a 4 MiB `-Xms` into 2 MiB at the low
    // end and 2 MiB at the high end: the pass covers the low part only.
    let got = heap.prefault_pass();
    assert!(got > 0, "populated nothing of the low startup commit");
    let upto = heap.counters.fp.prefault.upto.load(Ordering::Relaxed);
    assert_eq!(upto, 2 * MIB, "stopped short of, or past, the low startup commit");
    assert_eq!(
        heap.os_committed_bytes(),
        committed_before,
        "the worker committed memory"
    );
}

/// A collection holding the gate makes the pass yield at once.
#[test]
fn a_pass_yields_to_a_held_uncommit_gate() {
    let heap = ZgcRealHeap::with_capacity_and_initial(256 * MIB, 64 * MIB);
    heap.counters
        .fp
        .prefault
        .ahead
        .store(8 * MIB, Ordering::Relaxed);
    let gate = heap.counters.fp.uncommit_gate.lock();
    assert_eq!(heap.prefault_pass(), 0);
    drop(gate);
    assert!(heap.prefault_pass() > 0);
}

/// The carve hook requests a wake only when the window has fallen below
/// half its size ahead of the cursor, and only once until the worker runs.
#[test]
fn the_carve_hook_requests_once_per_half_window() {
    let heap = ZgcRealHeap::with_capacity_and_initial(256 * MIB, 64 * MIB);
    let pf = &heap.counters.fp.prefault;
    pf.ahead.store(8 * MIB, Ordering::Relaxed);
    pf.upto.store(10 * MIB, Ordering::Relaxed);
    heap.note_low_cursor(MIB);
    assert!(
        !pf.requested.load(Ordering::Relaxed),
        "9 MiB ahead is above half"
    );
    // 7 MiB ahead: short of the full window, still above half.
    heap.note_low_cursor(3 * MIB);
    assert!(
        !pf.requested.load(Ordering::Relaxed),
        "7 MiB ahead is above half"
    );
    heap.note_low_cursor(7 * MIB);
    assert!(
        pf.requested.load(Ordering::Relaxed),
        "3 MiB ahead is below half"
    );
    // Off: never requests.
    pf.requested.store(false, Ordering::Relaxed);
    pf.ahead.store(0, Ordering::Relaxed);
    heap.note_low_cursor(9 * MIB);
    assert!(!pf.requested.load(Ordering::Relaxed));
}

/// The worker itself: started, it populates the window without being
/// asked by a carve (its first pass), and stops on request.
#[test]
fn the_worker_populates_and_stops() {
    let heap = Arc::new(ZgcRealHeap::with_capacity_and_initial(256 * MIB, 64 * MIB));
    heap.counters
        .fp
        .prefault
        .ahead
        .store(8 * MIB, Ordering::Relaxed);
    if !heap.start_prefaulter() {
        // A store without a commit map (no reservation on this host).
        return;
    }
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while heap.prefaulted_bytes() == 0 && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    assert!(heap.prefaulted_bytes() > 0, "the worker populated nothing");
    heap.stop_prefaulter();
    assert!(heap.counters.fp.prefault.handle.lock().is_none());
}
