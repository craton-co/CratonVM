use super::*;

const BASE: usize = 0x7000_0000;
const END: usize = 0x7010_0000;

fn deposit(
    l: &YoungPinLedger,
    slot: &std::sync::Arc<YoungPinSlot>,
    words: &[usize],
    depth: usize,
) {
    let mut d = l.begin_deposit(std::sync::Arc::clone(slot));
    for &w in words {
        d.note(w);
    }
    d.commit(depth);
}

/// gcj-j1pn-pinned-cycle-review-findings item 1: `read_tagged` names
/// every layout-free / register word (a word deposited from a compiled
/// band first and a register later counts as layout-free), every peer
/// register word, and no compiled-band-only word; a torn slot's tags are
/// dropped with its words.
#[test]
fn gcj1pn_cycle_item1_read_tagged_names_layout_free_words() {
    let l = YoungPinLedger::new();
    l.open_pause();
    let slot = l.register();
    let mut d = l.begin_deposit(std::sync::Arc::clone(&slot));
    d.note(BASE + 0x10); // compiled band only
    d.note_native(BASE + 0x20); // layout-free band
    d.note(BASE + 0x30);
    d.note_native(BASE + 0x30); // compiled, then a register
    d.note_native(BASE + 0x20); // a repeat changes nothing
    d.commit(1);
    l.note_peer_reg_word(BASE + 0x40);
    let (mut out, mut lf) = (Vec::new(), Vec::new());
    let r = l.read_tagged(1, &mut out, &mut lf);
    assert!(r.complete);
    out.sort_unstable();
    lf.sort_unstable();
    assert_eq!(out, vec![BASE + 0x10, BASE + 0x20, BASE + 0x30, BASE + 0x40]);
    assert_eq!(lf, vec![BASE + 0x20, BASE + 0x30, BASE + 0x40]);
    assert_eq!(r.native, 3, "the upgraded word is counted once as native");
    // The untagged read is the same words.
    let mut plain = Vec::new();
    assert_eq!(l.read(1, &mut plain).words, out.len());
    // A fresh deposit forgets the previous pause's tags.
    l.open_pause();
    deposit(&l, &slot, &[BASE + 0x20], 1);
    let (mut out, mut lf) = (Vec::new(), Vec::new());
    assert!(l.read_tagged(1, &mut out, &mut lf).complete);
    assert_eq!(out, vec![BASE + 0x20]);
    assert!(lf.is_empty(), "compiled-band word in a reused cell: {lf:x?}");
}

/// THE prerequisite. A peer deposits for pause N; the initiator then runs
/// every per-pause reset it runs AFTER the peers parked — the coverage
/// cycle open and the root gatherer's clears — and the deposit is still
/// read as this pause's. Only the NEXT pause's opening makes it stale.
#[test]
fn the_ledger_survives_the_initiators_reset() {
    // A pause opens (the barrier's `request_stw`).
    reset_peer_proven_jit_depth();
    let l = young_pin_ledger();
    let peer = l.register();
    let initiator = l.register();
    deposit(l, &peer, &[BASE + 0x40], 2);

    // The initiator's resets.
    begin_moving_young_coverage_cycle();
    clear_pinned_jit_roots();
    clear_movable_jit_roots();
    clear_unrewritable_jit_roots();
    clear_unregistered_jit_frame_on_stack();
    deposit(l, &initiator, &[], 1);

    let mut out = Vec::new();
    let r = l.read(3, &mut out);
    assert!(
        r.complete,
        "a peer deposit must survive the initiator's resets: {r:?}"
    );
    assert_eq!(out, vec![BASE + 0x40]);
    assert_eq!(r.deposits, 2);

    // The next pause opens: both deposits are now stale.
    reset_peer_proven_jit_depth();
    let mut out = Vec::new();
    let r = l.read(3, &mut out);
    assert!(
        !r.complete,
        "last pause's deposits must not satisfy this one"
    );
    assert!(out.is_empty());
}

/// gcj-j1pn-pinned-cycle-review-findings item 1: the register words a
/// deposit noted first, and the peer register words, read back as the
/// pause's register subset; a register word noted after a stack word, a
/// duplicate, and a stale pause's registers are not in it.
#[test]
fn gcj1pn_register_words_are_read_back_as_their_own_subset() {
    let l = YoungPinLedger::new();
    l.open_pause();
    let slot = l.register();
    let mut d = l.begin_deposit(std::sync::Arc::clone(&slot));
    d.note_register(BASE + 0x10);
    d.note_register(BASE + 0x10);
    d.note_register(BASE + 0x20);
    d.note(BASE + 0x30);
    d.note_register(BASE + 0x40);
    d.note_native(BASE + 0x50);
    d.commit(1);
    l.note_peer_reg_word(BASE + 0x60);
    l.note_peer_band(&[BASE + 0x70, BASE + 0x80], Some(0), true, false);
    l.note_peer_band_registers(&[BASE + 0x70]);
    let mut regs = Vec::new();
    l.read_register_words(&mut regs);
    assert_eq!(
        regs,
        vec![BASE + 0x10, BASE + 0x20, BASE + 0x60, BASE + 0x70]
    );
    let mut all = Vec::new();
    let r = l.read(1, &mut all);
    assert_eq!(all.len(), 8, "{all:x?}");
    assert_eq!(r.native, 7, "registers count as layout-free words: {r:?}");
    // The next pause: nothing is this pause's.
    l.open_pause();
    let mut regs = Vec::new();
    l.read_register_words(&mut regs);
    assert!(regs.is_empty());
}

/// A thread inside compiled code that did not deposit leaves a shortfall.
#[test]
fn a_missing_peer_deposit_returns_false() {
    let l = YoungPinLedger::new();
    l.open_pause();
    let initiator = l.register();
    let peer = l.register();
    deposit(&l, &initiator, &[], 1);
    let mut out = Vec::new();
    assert!(
        !l.read(3, &mut out).complete,
        "depth 3 in the process, 1 deposited: incomplete"
    );
    // Registered is not deposited: the peer's slot is still at stamp 0.
    assert!(out.is_empty());
    deposit(&l, &peer, &[], 2);
    let mut out = Vec::new();
    let r = l.read(3, &mut out);
    assert!(
        r.complete && r.words == 0,
        "every entry accounted, nothing young: {r:?}"
    );
}

/// A deposit from an earlier pause contributes neither words nor depth.
#[test]
fn a_stale_stamp_deposit_is_ignored() {
    let l = YoungPinLedger::new();
    let peer = l.register();
    deposit(&l, &peer, &[BASE + 8, BASE + 16], 2);
    l.open_pause();
    let mut out = Vec::new();
    let r = l.read(2, &mut out);
    assert!(!r.complete, "a stale deposit must not account for depth");
    assert!(
        out.is_empty(),
        "a stale deposit's words must not be reported"
    );
    // With no JIT depth left to account for, the stale words are still not
    // read back as this pause's.
    let mut out = Vec::new();
    let r = l.read(0, &mut out);
    assert!(r.complete && out.is_empty(), "{r:?} {out:?}");
    // An unfinished deposit (begun, never committed) reads as unpublished.
    let open = l.begin_deposit(std::sync::Arc::clone(&peer));
    drop(open);
    let mut out = Vec::new();
    assert!(!l.read(1, &mut out).complete);
}

/// Words are stored RAW: an interior word is reported as itself, never as
/// the base of the object it points into. And the range keeps the
/// one-past-end and low-slack words the design asks for.
#[test]
fn an_interior_word_is_kept_raw() {
    let range = YoungPinRange::from_regions([(BASE, END), (0, 0)]);
    let interior = BASE + 0x1000 + 24;
    for w in [BASE, interior, END, BASE - YOUNG_PIN_LOW_SLACK] {
        assert!(range.contains(w), "0x{w:x} must count as young");
    }
    for w in [END + 8, BASE - YOUNG_PIN_LOW_SLACK - 8, 0, 8] {
        assert!(!range.contains(w), "0x{w:x} must not count as young");
    }
    assert!(YoungPinRange::from_regions([(0, 0), (0, 0)]).is_empty());
    // A header read is only licensed inside the region proper: the
    // one-past-end and low-slack words are young, but not readable.
    assert!(range.contains_span(BASE, 16));
    assert!(range.contains_span(END - 16, 16));
    for w in [END, END - 8, BASE - 8, BASE - YOUNG_PIN_LOW_SLACK] {
        assert!(!range.contains_span(w, 16), "0x{w:x} must not be read");
    }
    assert!(!range.contains_span(usize::MAX - 4, 16));

    let l = YoungPinLedger::new();
    let t = l.register();
    deposit(&l, &t, &[interior, interior], 1);
    let mut out = Vec::new();
    let r = l.read(1, &mut out);
    assert!(r.complete);
    assert_eq!(
        out,
        vec![interior],
        "raw word, deduplicated, not resolved to 0x{:x}",
        BASE + 0x1000
    );
}

/// Overflow is incompleteness, not truncation.
#[test]
fn overflow_returns_false() {
    let l = YoungPinLedger::new();
    let t = l.register();
    let full: Vec<usize> = (0..YOUNG_PIN_WORDS_PER_THREAD)
        .map(|i| BASE + 8 * i)
        .collect();
    deposit(&l, &t, &full, 1);
    let mut out = Vec::new();
    let r = l.read(1, &mut out);
    assert!(r.complete, "exactly the cap is complete");
    assert_eq!(out.len(), YOUNG_PIN_WORDS_PER_THREAD);

    let mut over = full.clone();
    over.push(END);
    deposit(&l, &t, &over, 1);
    let mut out = Vec::new();
    assert!(
        !l.read(1, &mut out).complete,
        "one word past the cap must read incomplete"
    );

    // Duplicates do not consume the cap.
    let mut dups = full;
    dups.extend_from_slice(&[BASE, BASE + 8]);
    deposit(&l, &t, &dups, 1);
    let mut out = Vec::new();
    assert!(l.read(1, &mut out).complete);

    // A scan that could not read its whole band is incomplete too.
    let mut d = l.begin_deposit(std::sync::Arc::clone(&t));
    d.mark_incomplete();
    d.commit(1);
    let mut out = Vec::new();
    assert!(!l.read(1, &mut out).complete);
}

/// A departed thread's slot is gone, so it can neither satisfy nor spoil
/// a later read.
#[test]
fn an_unregistered_slot_is_not_read() {
    let l = YoungPinLedger::new();
    let t = l.register();
    deposit(&l, &t, &[BASE], 1);
    l.unregister(&t);
    let mut out = Vec::new();
    let r = l.read(0, &mut out);
    assert!(r.complete && out.is_empty() && r.deposits == 0, "{r:?}");
}

/// Option B's rule and its census: it clears only when enabled AND
/// complete AND empty, and the census counts every evaluation either way.
#[test]
fn the_term4_verdict_and_its_census() {
    let empty = YoungPinRead {
        complete: true,
        ..YoungPinRead::default()
    };
    let words = YoungPinRead {
        complete: true,
        words: 3,
        native: 1,
        deposits: 2,
        depth: 2,
        peer_regs: 0,
        peer_bands: 0,
        deposit_incomplete: false,
        depth_short: false,
    };
    let partial = YoungPinRead {
        complete: false,
        ..YoungPinRead::default()
    };
    assert!(young_pin_ledger_verdict(empty, true));
    assert!(
        !young_pin_ledger_verdict(empty, false),
        "the flag is opt-in"
    );
    assert!(
        !young_pin_ledger_verdict(words, true),
        "a young word keeps the divert"
    );
    assert!(
        !young_pin_ledger_verdict(partial, true),
        "incomplete keeps the divert"
    );

    let c = YoungPinCensus::new();
    c.note(empty, true, 0);
    c.note(words, false, 2);
    c.note(partial, false, 0);
    assert_eq!(c.snapshot(), [3, 1, 1, 3, 3, 1, 1, 2]);
}

/// gcj-j1pn-young-pin-ledger-review-findings item 7: the hash-indexed
/// dedup keeps exactly the distinct words, in first-seen order, across
/// colliding home cells; a second deposit into the same slot starts from
/// an empty index; the 257th distinct word still marks the deposit
/// incomplete.
#[test]
fn gcj1pn_the_deposit_dedup_keeps_each_distinct_word_once() {
    let l = YoungPinLedger::new();
    let slot = l.register();
    // Words 4 KiB apart share low bits; a run of them stresses probing.
    let distinct: Vec<usize> = (0..200).map(|i| BASE + i * 0x1000).collect();
    let mut d = l.begin_deposit(std::sync::Arc::clone(&slot));
    for round in 0..3 {
        for &w in &distinct {
            d.note(w);
        }
        assert_eq!(d.len(), distinct.len(), "round {round}");
    }
    d.commit(1);
    let mut out = Vec::new();
    let r = l.read(1, &mut out);
    assert!(r.complete);
    assert_eq!(out, distinct);

    // A fresh deposit: the old index must not hide its words.
    let mut d = l.begin_deposit(std::sync::Arc::clone(&slot));
    d.note(distinct[7]);
    assert_eq!(d.len(), 1);
    for i in 0..YOUNG_PIN_WORDS_PER_THREAD {
        d.note(END + i * 8);
    }
    assert!(d.is_incomplete(), "past the cap");
    d.commit(1);
    let mut out = Vec::new();
    assert!(!l.read(1, &mut out).complete);
}


/// The production predicate records one evaluation per call and, with the
/// flag unset, never clears.
#[test]
fn the_term4_helper_is_opt_in_and_counted() {
    let before = young_pin_ledger_census();
    assert!(!young_pin_ledger_clears_term4(None) || gc_flags().gen_young_pin_ledger_term4);
    let after = young_pin_ledger_census();
    assert_eq!(after[0], before[0] + 1);
}

/// gen r4w6/pinstale6: a peer REGISTER word the cross-thread scan captured
/// is read back with the pause's deposits, raw and deduplicated, credits no
/// JIT depth, and goes stale when the next pause opens.
#[test]
fn peer_register_words_join_the_pause_and_go_stale_with_it() {
    let l = YoungPinLedger::new();
    let t = l.register();
    deposit(&l, &t, &[BASE + 8], 1);
    let interior = BASE + 0x2000 + 24;
    l.note_peer_reg_word(interior);
    l.note_peer_reg_word(interior);
    l.note_peer_reg_word(BASE + 0x3000);
    let mut out = Vec::new();
    let r = l.read(1, &mut out);
    assert!(r.complete, "{r:?}");
    assert_eq!(r.peer_regs, 2, "deduplicated: {r:?}");
    assert_eq!(r.words, 3, "{r:?}");
    assert_eq!(r.native, 2, "register words are layout-free words: {r:?}");
    assert_eq!(out, vec![BASE + 8, interior, BASE + 0x3000]);

    // Register words credit no depth: a JIT entry nobody deposited for
    // still leaves the ledger incomplete.
    l.open_pause();
    l.note_peer_reg_word(BASE + 0x40);
    let mut out = Vec::new();
    assert!(!l.read(1, &mut out).complete);

    // The next pause opens: last pause's register words are not read.
    l.open_pause();
    let mut out = Vec::new();
    let r = l.read(0, &mut out);
    assert!(
        r.complete && out.is_empty() && r.peer_regs == 0,
        "{r:?} {out:?}"
    );
}

/// More distinct register words than the cap is incompleteness for that
/// pause only; duplicates never consume the cap.
#[test]
fn a_register_word_overflow_is_incompleteness() {
    let l = YoungPinLedger::new();
    for i in 0..PEER_REG_PIN_WORDS_CAP {
        l.note_peer_reg_word(BASE + 8 * i);
    }
    let mut out = Vec::new();
    assert!(l.read(0, &mut out).complete, "exactly the cap is complete");
    assert_eq!(out.len(), PEER_REG_PIN_WORDS_CAP);
    l.note_peer_reg_word(BASE);
    assert!(
        l.read(0, &mut Vec::new()).complete,
        "a duplicate is not an overflow"
    );
    l.note_peer_reg_word(END);
    assert!(
        !l.read(0, &mut Vec::new()).complete,
        "one distinct word past the cap must read incomplete"
    );
    l.open_pause();
    assert!(
        l.read(0, &mut Vec::new()).complete,
        "the overflow was that pause's"
    );
}

/// gci i1/pn: the register words are kept sorted (the dedup is a binary
/// search now). Words noted in any order, with repeats, read back as the
/// same distinct set, ascending; a repeat at the cap is still not an
/// overflow.
#[test]
fn gci_i1pn_peer_register_words_dedup_in_any_order() {
    let l = YoungPinLedger::new();
    for &w in &[BASE + 0x300, BASE + 0x100, BASE + 0x300, BASE + 0x200, BASE + 0x100] {
        l.note_peer_reg_word(w);
    }
    let mut out = Vec::new();
    let r = l.read(0, &mut out);
    assert!(r.complete, "{r:?}");
    assert_eq!(r.peer_regs, 3, "{r:?}");
    assert_eq!(out, vec![BASE + 0x100, BASE + 0x200, BASE + 0x300]);

    // Fill to the cap in DESCENDING order, then repeat the lowest and the
    // highest: still complete. One new word: incomplete.
    l.open_pause();
    for i in (0..PEER_REG_PIN_WORDS_CAP).rev() {
        l.note_peer_reg_word(BASE + 8 * i);
    }
    l.note_peer_reg_word(BASE);
    l.note_peer_reg_word(BASE + 8 * (PEER_REG_PIN_WORDS_CAP - 1));
    let mut out = Vec::new();
    assert!(l.read(0, &mut out).complete, "repeats at the cap are not an overflow");
    assert_eq!(out.len(), PEER_REG_PIN_WORDS_CAP);
    assert!(out.windows(2).all(|w| w[0] < w[1]), "sorted and distinct");
    l.note_peer_reg_word(END);
    assert!(!l.read(0, &mut Vec::new()).complete, "one distinct word past the cap");
}

/// Option B must not stand term 4 down over a young word a peer holds in a
/// register: the word is in the ledger, so the read is not empty.
#[test]
fn option_b_keeps_the_divert_over_a_peer_register_word() {
    let l = YoungPinLedger::new();
    l.note_peer_reg_word(BASE + 16);
    let r = l.read(0, &mut Vec::new());
    assert!(r.complete && r.words == 1, "{r:?}");
    assert!(!young_pin_ledger_verdict(r, true));
}

/// Deposit `words` at `depth` through the production door, on the calling
/// thread, into whatever ledger it is bound to.
fn deposit_self(words: &[usize], depth: usize) {
    let mut d = begin_self_young_pin_deposit().expect("a bound or test thread has a ledger");
    for &w in words {
        d.note(w);
    }
    d.commit(depth);
}

/// gc-common w36-c: the ledger is per VM. Two VMs' pauses overlap (the
/// coverage slot's give-up allows it): B's open does not stale A's
/// deposit, B's deposit is not read by A (neither its words nor its
/// depth), and a thread's slot follows the VM it deposits for.
#[test]
fn the_young_pin_ledger_is_the_bound_vms_only() {
    let a = std::sync::Arc::new(PauseLedger::new());
    let b = std::sync::Arc::new(PauseLedger::new());

    // A's pause opens; a thread of A deposits one young word, depth 1.
    with_pause_ledger(&a, reset_peer_proven_jit_depth);
    let a_stamp = a.young_pin_stamp();
    assert_eq!(with_pause_ledger(&a, young_pin_pause_stamp), a_stamp);
    with_pause_ledger(&a, || deposit_self(&[BASE + 8], 1));

    // B's pause opens and a thread of B deposits, inside A's pause.
    let b_thread = std::sync::Arc::clone(&b);
    std::thread::spawn(move || {
        let b = b_thread;
        with_pause_ledger(&b, reset_peer_proven_jit_depth);
        with_pause_ledger(&b, || deposit_self(&[BASE + 16], 2));
        let mut out = Vec::new();
        let r = b.young_pins.read(2, &mut out);
        assert!(r.complete, "B's own deposit accounts for B: {r:?}");
        assert_eq!(out, vec![BASE + 16], "B reads only B's words");
    })
    .join()
    .expect("B's thread");
    // The thread has exited (a native join waits for its thread-local
    // destructors): its slot left B's ledger.
    assert!(b.young_pins.slots.lock().is_empty());

    assert_eq!(a.young_pin_stamp(), a_stamp, "B's open moved A's stamp");
    let mut out = Vec::new();
    let r = a.young_pins.read(1, &mut out);
    assert!(r.complete, "A's deposit survives B's pause: {r:?}");
    assert_eq!(out, vec![BASE + 8], "A reads only A's words");
    let mut out = Vec::new();
    assert!(
        !a.young_pins.read(3, &mut out).complete,
        "B's depth does not make up A's shortfall"
    );

    // The same thread now deposits for B: its slot moves to B's ledger.
    with_pause_ledger(&b, || deposit_self(&[BASE + 24], 0));
    assert!(
        a.young_pins.slots.lock().is_empty(),
        "A's slot was released"
    );
    assert_eq!(b.young_pins.slots.lock().len(), 1);

    // An unbound thread (under `cfg(test)`) uses its own ledger, which
    // neither VM reads.
    deposit_self(&[BASE + 32], 0);
    assert_eq!(
        b.young_pins.slots.lock().len(),
        0,
        "the slot followed the thread"
    );
    let mut out = Vec::new();
    assert!(b.young_pins.read(0, &mut out).complete && out.is_empty());
    assert!(a.young_pins.slots.lock().is_empty());
}

/// An orphan write (an unbound deposit or register word in production)
/// fails the read of every ledger whose pause was open at the time, until
/// that ledger's next open.
#[test]
fn an_orphan_write_fails_the_open_pauses_read() {
    let l = YoungPinLedger::new();
    l.open_pause();
    assert!(!l.orphaned_since_open());
    let before = young_pin_orphan_writes();
    let coverage_before = coverage_orphan_writes();
    note_young_pin_orphan_write();
    assert!(young_pin_orphan_writes() > before);
    assert!(coverage_orphan_writes() > coverage_before);
    assert!(l.orphaned_since_open());
    l.open_pause();
    // Another test's orphan may land between these two lines; retry the
    // open until none does (bounded: orphans are rare in this binary).
    let mut clean = !l.orphaned_since_open();
    for _ in 0..100 {
        if clean {
            break;
        }
        l.open_pause();
        clean = !l.orphaned_since_open();
    }
    assert!(clean, "the next open clears the verdict");
    // A ledger never opened since an orphan fails closed too.
    let fresh = YoungPinLedger::new();
    assert!(fresh.orphaned_since_open());
}

/// `record_peer_reg`'s index split: registers feed the ledger, the two
/// stack populations do not.
#[test]
fn only_register_indices_are_registers() {
    for reg in [0u8, 3, 15, 16] {
        assert!(peer_reg_is_register(reg), "r{reg}");
    }
    assert!(!peer_reg_is_register(PEER_REG_STACK_TAKEOVER));
    assert!(!peer_reg_is_register(PEER_REG_STACK_HELPER));
}
