use super::*;

const BASE: usize = 0x7000_0000;

/// A band read whole credits its depth, and its young words are read back
/// raw, deduplicated, with the pause's other words.
#[test]
fn a_whole_band_credits_depth_and_its_words_are_read() {
    let l = YoungPinLedger::new();
    l.note_peer_band(&[BASE + 16, BASE + 8, BASE + 16], Some(2), true, true);
    l.note_peer_band(&[BASE + 8, BASE + 0x2000 + 3], Some(1), true, false);
    let mut out = Vec::new();
    let r = l.read(3, &mut out);
    assert!(r.complete, "two bands account for depth 3: {r:?}");
    assert_eq!(r.depth, 3, "{r:?}");
    assert_eq!(r.peer_bands, 3, "deduplicated across bands: {r:?}");
    assert_eq!(r.words, 3, "{r:?}");
    assert_eq!(r.native, 3, "band words are layout-free words: {r:?}");
    assert_eq!(out, vec![BASE + 8, BASE + 16, BASE + 0x2000 + 3]);
    assert_eq!(
        l.peer_band_windows(),
        1,
        "only the helper-window band counts as a window"
    );
    // One more JIT entry than the bands cover: a shortfall.
    assert!(!l.read(4, &mut Vec::new()).complete);
}

/// A band that was not read whole, or whose peer has no published depth,
/// fails the pause and credits nothing; the next pause is clean.
#[test]
fn a_partial_or_depthless_band_fails_only_its_pause() {
    let l = YoungPinLedger::new();
    l.note_peer_band(&[BASE + 8], None, true, true);
    let r = l.read(0, &mut Vec::new());
    assert!(!r.complete && r.depth == 0, "{r:?}");
    assert_eq!(l.peer_band_windows(), 0);

    l.open_pause();
    l.note_peer_band(&[BASE + 8], Some(1), false, false);
    let r = l.read(0, &mut Vec::new());
    assert!(!r.complete && r.depth == 0, "{r:?}");

    l.open_pause();
    l.mark_peer_band_incomplete();
    assert!(!l.read(0, &mut Vec::new()).complete);

    l.open_pause();
    let mut out = Vec::new();
    let r = l.read(0, &mut out);
    assert!(r.complete && out.is_empty() && r.peer_bands == 0, "{r:?}");
}

/// More distinct band words than the cap is incompleteness, not
/// truncation; duplicates never consume the cap.
#[test]
fn a_band_word_overflow_is_incompleteness() {
    let l = YoungPinLedger::new();
    let full: Vec<usize> = (0..PEER_BAND_PIN_WORDS_CAP).map(|i| BASE + 8 * i).collect();
    l.note_peer_band(&full, Some(1), true, true);
    l.note_peer_band(&full[..10], Some(0), true, false);
    let mut out = Vec::new();
    let r = l.read(1, &mut out);
    assert!(r.complete, "exactly the cap is complete: {r:?}");
    assert_eq!(out.len(), PEER_BAND_PIN_WORDS_CAP);
    l.note_peer_band(&[BASE - 8], Some(0), true, false);
    let r = l.read(1, &mut Vec::new());
    assert!(!r.complete, "one distinct word past the cap: {r:?}");
}

/// Last pause's bands are neither words nor depth of this one.
#[test]
fn a_stale_band_is_ignored() {
    let l = YoungPinLedger::new();
    l.note_peer_band(&[BASE + 8], Some(2), true, true);
    l.open_pause();
    let mut out = Vec::new();
    let r = l.read(2, &mut out);
    assert!(!r.complete && out.is_empty(), "{r:?}");
    assert_eq!(l.peer_band_windows(), 0);
}

/// The take-over arm's admission: only the licence a pinning backend gets,
/// with nobody frozen and at least one window, and only once every window
/// is in the ledger.
#[test]
fn the_takeover_admission_rules() {
    let hw = TakeoverVerdict {
        helper_windows: 2,
        ..TakeoverVerdict::NONE
    };
    assert_eq!(hw.licence(true), MoveLicence::MovePinned);
    assert!(takeover_verdict_admits_pinned_copy(hw));
    assert!(!takeover_admits_band_ledger(hw, 1), "one window missing");
    assert!(takeover_admits_band_ledger(hw, 2));

    assert!(
        !takeover_verdict_admits_pinned_copy(TakeoverVerdict::NONE),
        "no window: term 3 did not fire through the take-over"
    );
    let frozen = TakeoverVerdict { frozen: 1, ..hw };
    assert!(
        !takeover_verdict_admits_pinned_copy(frozen),
        "a frozen peer"
    );
    let unread = TakeoverVerdict {
        unreadable: 1,
        ..hw
    };
    assert!(
        !takeover_verdict_admits_pinned_copy(unread),
        "an unread peer"
    );
    let unpinned = TakeoverVerdict {
        pins_complete: false,
        ..hw
    };
    assert!(
        !takeover_verdict_admits_pinned_copy(unpinned),
        "an unpinned window"
    );
    let derived = TakeoverVerdict {
        derived_pointers_resolved: false,
        ..hw
    };
    assert!(
        !takeover_verdict_admits_pinned_copy(derived),
        "unresolved derived pointers"
    );
    assert!(!takeover_verdict_admits_pinned_copy(
        TakeoverVerdict::UNKNOWN
    ));
    assert!(!takeover_admits_band_ledger(unread, 9));
}

/// The proven blocked-monitor stash is the pause's own row: drained once,
/// cleared by a take-over start and by a pause open, and not another VM's.
#[test]
fn the_proven_monitor_stash_is_per_pause_and_per_vm() {
    let a = std::sync::Arc::new(PauseLedger::new());
    let b = std::sync::Arc::new(PauseLedger::new());
    with_pause_ledger(&a, || {
        stash_proven_monitor_peer_for_ledger(11, 2);
        stash_proven_monitor_peer_for_ledger(12, 1);
    });
    with_pause_ledger(&b, || stash_proven_monitor_peer_for_ledger(21, 3));
    assert_eq!(
        with_pause_ledger(&a, take_proven_monitor_peers_for_ledger),
        vec![(11, 2), (12, 1)]
    );
    assert!(with_pause_ledger(&a, take_proven_monitor_peers_for_ledger).is_empty());
    with_pause_ledger(&b, reset_xt_cycle);
    assert!(
        with_pause_ledger(&b, take_proven_monitor_peers_for_ledger).is_empty(),
        "a take-over start clears the stash"
    );
    with_pause_ledger(&a, || stash_proven_monitor_peer_for_ledger(13, 1));
    with_pause_ledger(&a, reset_peer_proven_jit_depth);
    assert!(
        with_pause_ledger(&a, take_proven_monitor_peers_for_ledger).is_empty(),
        "a pause open clears the stash"
    );
}
