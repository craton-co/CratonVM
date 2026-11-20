use super::*;

/// Serialises the tests in this module.
///
/// The verdict table is per-THREAD but the epoch is a process-global,
/// and `a_verdict_is_scoped_to_its_carrier_and_epoch` bumps it. Without
/// this, that bump lands in the middle of a sibling running on another
/// thread and invalidates verdicts it just published — which is exactly
/// how the two carrier tests below failed in the suite and passed when
/// run alone.
static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Take the lock and start from an empty table.
fn guard() -> std::sync::MutexGuard<'static, ()> {
    let g = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
    forget_validated();
    g
}

/// The per-thread consult tally (round 11) loses nothing the reporting
/// thread counted: `fast_path_counts` flushes the caller's own batch, and
/// other threads can only add.
#[test]
fn batched_consults_are_all_visible_to_the_counting_thread() {
    let (h0, m0) = fast_path_counts();
    for _ in 0..(CONSULT_FLUSH_EVERY + 5) {
        note_fast_consult(true);
    }
    for _ in 0..3 {
        note_fast_consult(false);
    }
    let (h1, m1) = fast_path_counts();
    assert!(h1 >= h0 + CONSULT_FLUSH_EVERY + 5, "hits {h0} -> {h1}");
    assert!(m1 >= m0 + 3, "misses {m0} -> {m1}");
}

#[test]
fn a_verdict_is_scoped_to_its_carrier_and_epoch() {
    let _g = guard();
    note_validated(0x1000, false);
    assert!(is_validated(0x1000, false), "the published carrier hits");
    assert!(
        !is_validated(0x2000, false),
        "a different carrier must never hit a single-slot memo"
    );
    assert!(
        !is_validated(0x1000, true),
        "a read verdict must not answer a write question"
    );

    // Anything that could free the block or move the carrier invalidates.
    bump_epoch();
    assert!(
        !is_validated(0x1000, false),
        "an epoch bump must retire every published verdict"
    );
}

#[test]
fn a_write_verdict_merges_rather_than_replacing_the_read_one() {
    let _g = guard();
    note_validated(0x1000, false);
    note_validated(0x1000, true);
    assert!(is_validated(0x1000, false));
    assert!(
        is_validated(0x1000, true),
        "the write verdict must be retained"
    );
    // And the merge must not manufacture a write verdict for a carrier that
    // only ever passed the read checks.
    note_validated(0x3000, false);
    assert!(!is_validated(0x3000, true));
}

/// The defect the census found: two carriers used alternately, which is
/// what kfusion's integration stage does (the TSDF volume and the images
/// it reads). With one slot each switch evicted the other and republished.
///
/// Verified able to fail: with `CRATONVM_FFM_VERDICT_WAYS=1` — the
/// single-slot behaviour this replaced — the second assertion fails on
/// the first alternation.
#[test]
fn two_carriers_used_alternately_both_stay_validated() {
    let _g = guard();
    let a = 0x1_0000u64;
    let b = 0x2_0000u64;
    note_validated(a, false);
    note_validated(b, false);
    for _ in 0..8 {
        assert!(is_validated(a, false), "carrier A was evicted by B");
        assert!(is_validated(b, false), "carrier B was evicted by A");
    }
}

/// Four is the width, so a fifth carrier must cost one of the others —
/// but only one, and the survivors must stay valid. A cache that dropped
/// everything on an overflow would be the single slot again with extra
/// steps.
#[test]
fn a_fifth_carrier_evicts_one_way_not_the_table() {
    let _g = guard();
    for i in 1..=(VERDICT_WAYS as u64 + 1) {
        note_validated(i * 0x1000, false);
    }
    let live = (1..=(VERDICT_WAYS as u64 + 1))
        .filter(|i| is_validated(i * 0x1000, false))
        .count();
    assert_eq!(
        live, VERDICT_WAYS,
        "expected exactly {VERDICT_WAYS} carriers to survive, got {live}"
    );
    assert!(
        is_validated((VERDICT_WAYS as u64 + 1) * 0x1000, false),
        "the most recent carrier must always be the one that is kept"
    );
}

/// Round 12 wave 2 (lane rt): a verdict whose checks began before an epoch
/// bump (a free on another thread, a collection) is never published, so it
/// cannot vouch for a block freed while the checks ran.
#[test]
fn a_verdict_whose_checks_straddle_an_epoch_bump_is_not_published() {
    let _g = guard();
    let observed = epoch();
    // The free (or collection) lands after the checks read the scope live.
    bump_epoch();
    note_validated_at(0x5000, true, observed);
    assert!(
        !is_validated(0x5000, false),
        "a verdict from before the bump vouched for a freed block"
    );
    // Checks that began after the bump publish as usual.
    note_validated_at(0x5000, true, epoch());
    assert!(is_validated(0x5000, true));
}

/// Round 12 wave 3 (lane ffm): the default arena model closes a session
/// without freeing anything, so before the close bumped the epoch a
/// thread's verdict survived `arena.close()` and its compiled accesses
/// kept hitting — the hung worker of `R12RtSharedArenaClose`.
#[test]
fn a_close_retires_every_verdict_even_without_a_free() {
    let _g = guard();
    note_validated(0x6000, true);
    assert!(is_validated(0x6000, true));
    let in_flight = close_handshake_begin(false);
    assert!(in_flight.is_empty(), "a confined close scans nothing");
    assert!(
        !is_validated(0x6000, false),
        "a verdict outlived the close of its scope"
    );
}

/// A shared close retires verdicts on every thread (the bump), not only on
/// the closer: the hung worker held its verdict on ANOTHER thread.
#[test]
fn a_shared_close_retires_verdicts_published_on_another_thread() {
    let _g = guard();
    let (published_tx, published_rx) = std::sync::mpsc::channel::<()>();
    let (closed_tx, closed_rx) = std::sync::mpsc::channel::<()>();
    let worker = std::thread::spawn(move || {
        note_validated(0x7000, false);
        let before = is_validated(0x7000, false);
        let _ = published_tx.send(());
        let _ = closed_rx.recv();
        (before, is_validated(0x7000, false))
    });
    let _ = published_rx.recv();
    close_handshake_begin(true).wait();
    let _ = closed_tx.send(());
    let (before, after) = worker.join().unwrap_or((false, true));
    assert!(before, "the worker's verdict was never published");
    assert!(!after, "the worker's verdict survived a shared close");
}

/// The accessor half: a close that probes while another thread is inside
/// a window waits until that window closes, and not before.
#[test]
fn a_close_waits_for_an_access_window_that_was_open() {
    use std::sync::mpsc;
    let _g = guard();
    let (opened_tx, opened_rx) = mpsc::channel::<()>();
    let (release_tx, release_rx) = mpsc::channel::<()>();
    let accessor = std::thread::spawn(move || {
        let outer = AccessWindow::open();
        assert!(outer.admits_fast_path());
        // Nested: only the outermost window toggles the word.
        let inner = AccessWindow::open();
        drop(inner);
        let _ = opened_tx.send(());
        let _ = release_rx.recv();
        drop(outer);
    });
    let _ = opened_rx.recv();
    let in_flight = close_handshake_begin(true);
    assert!(
        !in_flight.is_empty(),
        "the open window (still open after its nested window closed) was not probed"
    );
    let done = std::sync::Arc::new(AtomicBool::new(false));
    let waiter = {
        let done = std::sync::Arc::clone(&done);
        std::thread::spawn(move || {
            in_flight.wait();
            done.store(true, Ordering::SeqCst);
        })
    };
    std::thread::sleep(std::time::Duration::from_millis(50));
    assert!(
        !done.load(Ordering::SeqCst),
        "the close stopped waiting while the access was still in flight"
    );
    let _ = release_tx.send(());
    let _ = accessor.join();
    let _ = waiter.join();
    assert!(done.load(Ordering::SeqCst));
}

/// JIT r20 wave 19 (lane yFfmShared, MEM20-2): the access-word mirror
/// names the thread's leased word once it leased one, and a window opened
/// through it the way compiled code does (`LOCK INC` the word, `INC` it
/// back) is one a shared close probes and waits for.
#[test]
fn the_access_word_mirror_names_a_word_a_close_waits_on() {
    use std::sync::mpsc;
    let _g = guard();
    if access_word_tls_disp().is_none() {
        // No mirror on this target / configuration: the fold keeps its
        // deopt, and there is nothing to check.
        return;
    }
    let (opened_tx, opened_rx) = mpsc::channel::<()>();
    let (release_tx, release_rx) = mpsc::channel::<()>();
    let accessor = std::thread::spawn(move || {
        assert_eq!(
            access_word_mirror_read(),
            Some(0),
            "a fresh thread has no word"
        );
        drop(AccessWindow::open());
        let addr = access_word_mirror_read().unwrap_or(0);
        assert_ne!(addr, 0, "the lease did not publish the mirror");
        // SAFETY: the mirror holds this thread's leased, leaked word.
        let word = unsafe { &*(addr as *const AtomicU64) };
        assert_eq!(word.load(Ordering::Relaxed) & 1, 0, "no window is open");
        word.fetch_add(1, Ordering::SeqCst); // the compiled open
        let _ = opened_tx.send(());
        let _ = release_rx.recv();
        let seq = word.load(Ordering::Relaxed);
        word.store(seq.wrapping_add(1), Ordering::Release); // the compiled close
    });
    let _ = opened_rx.recv();
    let in_flight = close_handshake_begin(true);
    assert!(!in_flight.is_empty(), "the compiled window was not probed");
    let _ = release_tx.send(());
    in_flight.wait();
    let _ = accessor.join();
}

/// A close made from inside this thread's own window (a close action run
/// from an access, however unlikely) must not wait for itself.
#[test]
fn a_close_never_waits_for_the_closing_thread() {
    let _g = guard();
    let window = AccessWindow::open();
    close_handshake_begin(true).wait();
    drop(window);
}

/// Round 12 wave 5 (lane ffm2): a layout verdict is scoped to its address
/// and its epoch, carries the layout's alignment mask, and is never
/// published for a checks-straddling-a-bump read or a bogus alignment.
#[test]
fn a_layout_verdict_is_scoped_to_its_address_and_epoch() {
    let _g = guard();
    assert_eq!(layout_align_mask(0x9_0000), None, "nothing published yet");
    assert_eq!(layout_align_mask(0), None, "null is never validated");
    // The epoch is process-wide and other test modules bump it (a shared
    // close handshake does), so each positive check is retried until one
    // attempt saw a stable epoch; the negative checks hold regardless.
    let stable = |check: &dyn Fn(u64) -> bool| {
        (0..32).any(|attempt| {
            let now = epoch();
            let ok = check(0x9_1000 + attempt * 0x100);
            epoch() == now && ok
        })
    };
    assert!(
        stable(&|layout: u64| {
            note_layout_validated_at(layout, 4, epoch());
            layout_align_mask(layout) == Some(3) && layout_align_mask(layout + 8).is_none()
        }),
        "a published layout must hit with its mask, and only at its address"
    );
    assert!(
        stable(&|layout: u64| {
            note_layout_validated_at(layout, 1, epoch());
            layout_align_mask(layout) == Some(0)
        }),
        "an unaligned layout's mask admits every address"
    );

    let layout = 0x9_8000u64;
    note_layout_validated_at(layout, 4, epoch());
    bump_epoch();
    assert_eq!(
        layout_align_mask(layout),
        None,
        "a collection (epoch bump) must retire a layout verdict"
    );
    let observed = epoch();
    bump_epoch();
    note_layout_validated_at(layout, 8, observed);
    assert_eq!(
        layout_align_mask(layout),
        None,
        "a verdict whose reads straddled a bump must not be published"
    );
    note_layout_validated_at(layout, 3, epoch());
    assert_eq!(layout_align_mask(layout), None, "3 is not an alignment");
}

/// More value layouts than ways: the newest one always survives and the
/// table keeps exactly `LAYOUT_WAYS` live.
#[test]
fn layout_verdicts_evict_one_way_at_a_time() {
    let _g = guard();
    let n = LAYOUT_WAYS as u64 + 3;
    // The epoch is process-wide and other test modules bump it (a shared
    // close handshake does), so retry until one pass saw a stable epoch.
    for _attempt in 0..32 {
        let now = epoch();
        for i in 1..=n {
            note_layout_validated_at(0xA_0000 + i * 0x100, 8, now);
        }
        let live = (1..=n)
            .filter(|i| layout_align_mask(0xA_0000 + i * 0x100).is_some())
            .count();
        let newest = layout_align_mask(0xA_0000 + n * 0x100);
        if epoch() != now {
            continue;
        }
        assert_eq!(live, LAYOUT_WAYS);
        assert_eq!(newest, Some(7), "the newest layout must always be kept");
        return;
    }
}

#[test]
fn a_zero_carrier_is_never_validated() {
    let _g = guard();
    note_validated(0, true);
    assert!(!is_validated(0, false), "null must not be fast-pathed");
}
