use super::*;
use std::sync::Arc;

/// A peer's deposit lands in the ledger of ITS VM's pause, and an
/// initiator reads only its own VM's ledger -- the property the
/// process-static ledger could not have: VM B's peers can no longer
/// inflate VM A's proven depth, whatever the coverage slot did.
#[test]
fn a_deposit_is_credited_to_its_own_vms_pause_only() {
    let a = Arc::new(PauseLedger::new());
    let b = Arc::new(PauseLedger::new());
    // A's initiator: bound for its pause, which opens its ledger.
    bind_pause_ledger(&a);
    reset_peer_proven_jit_depth();
    // A peer of VM B parks and deposits (another thread, B's binding).
    let b2 = Arc::clone(&b);
    std::thread::spawn(move || with_pause_ledger(&b2, || add_peer_proven_jit_depth(5)))
        .join()
        .expect("B's peer");
    assert_eq!(b.proven_jit_depth(), 5, "B's deposit is B's");
    assert_eq!(
        peer_proven_jit_depth(),
        0,
        "A's initiator must not be credited with B's peer"
    );
    // A peer of VM A deposits on its own thread.
    let a2 = Arc::clone(&a);
    std::thread::spawn(move || with_pause_ledger(&a2, || add_peer_proven_jit_depth(3)))
        .join()
        .expect("A's peer");
    assert_eq!(peer_proven_jit_depth(), 3);
    // B's next request opens B's ledger only.
    let b3 = Arc::clone(&b);
    std::thread::spawn(move || {
        bind_pause_ledger(&b3);
        reset_peer_proven_jit_depth();
        unbind_pause_ledger(&b3);
    })
    .join()
    .expect("B's initiator");
    assert_eq!(b.proven_jit_depth(), 0);
    assert_eq!(
        peer_proven_jit_depth(),
        3,
        "B's reset must not erase A's ledger"
    );
    unbind_pause_ledger(&a);
    assert!(!pause_ledger_bound(&a));
    assert_eq!(
        peer_proven_jit_depth(),
        0,
        "an unbound thread reads its own fallback"
    );
}

/// `with_pause_ledger` restores the previous binding, also on unwind, and
/// `unbind_pause_ledger` removes only the ledger it names.
#[test]
fn a_scoped_binding_restores_the_previous_one() {
    let a = Arc::new(PauseLedger::new());
    let b = Arc::new(PauseLedger::new());
    bind_pause_ledger(&a);
    with_pause_ledger(&b, || {
        assert!(pause_ledger_bound(&b));
        add_peer_proven_jit_depth(2);
    });
    assert!(pause_ledger_bound(&a), "the initiator's binding is back");
    assert_eq!(b.proven_jit_depth(), 2);
    assert_eq!(a.proven_jit_depth(), 0);
    fn unwinding_deposit() {
        panic!("a deposit that unwinds");
    }
    let unwound = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        with_pause_ledger(&b, unwinding_deposit)
    }));
    assert!(unwound.is_err());
    assert!(pause_ledger_bound(&a), "restored on unwind too");
    unbind_pause_ledger(&b);
    assert!(
        pause_ledger_bound(&a),
        "unbinding another VM's ledger is a no-op"
    );
    unbind_pause_ledger(&a);
    assert!(!pause_ledger_bound(&a));
}

/// gc-common w18-c: the guard form restores the previous binding when it
/// drops, and nests like `with_pause_ledger`.
#[test]
fn a_scoped_ledger_guard_restores_the_previous_binding() {
    let a = Arc::new(PauseLedger::new());
    let b = Arc::new(PauseLedger::new());
    bind_pause_ledger(&a);
    {
        let _scope = scoped_pause_ledger(&b);
        assert!(pause_ledger_bound(&b));
        add_peer_proven_jit_depth(3);
        {
            let _inner = scoped_pause_ledger(&a);
            assert!(pause_ledger_bound(&a));
        }
        assert!(
            pause_ledger_bound(&b),
            "the inner guard restores the outer one"
        );
    }
    assert!(pause_ledger_bound(&a), "the initiator's binding is back");
    assert_eq!((a.proven_jit_depth(), b.proven_jit_depth()), (0, 3));
    unbind_pause_ledger(&a);
    {
        let _scope = scoped_pause_ledger(&b);
        assert!(pause_ledger_bound(&b));
    }
    assert!(
        !pause_ledger_bound(&b),
        "an unbound thread goes back to unbound"
    );
}

/// gc-common w18-c: the peer native-stack captures and the register
/// pairing capture (`PEER_STACK_SLOTS` / `PEER_REG_CAPTURE` process statics
/// until then) are the bound ledger's. VM B's pause opening and B's
/// captures leave VM A's alone; A's drain returns A's words only; an
/// unbound thread drains its own empty fallback; opening A's next pause
/// discards what A never folded.
#[test]
fn the_peer_captures_are_the_bound_vms_only() {
    let a = Arc::new(PauseLedger::new());
    let b = Arc::new(PauseLedger::new());
    let remap_on = blocked_peer_stack_remap_enabled();
    with_pause_ledger(&a, || {
        record_peer_stack_slot(0xA, 0x5a00_0000, 0x5a00_1000);
        record_takeover_stack_slot(0xA, 0x5a00_0008, 0x5a00_2000);
        // The pairing capture fills only under its debug flag; seed it
        // directly so the routing is tested either way.
        a.reg_capture
            .lock()
            .push((0xA, PEER_REG_STACK_HELPER, 0x5a00_3000));
    });
    let b2 = Arc::clone(&b);
    std::thread::spawn(move || {
        with_pause_ledger(&b2, || {
            // B's request opens B's pause; B's opener clears B's captures.
            b2.open();
            clear_peer_stack_slots();
            clear_peer_reg_capture();
            record_peer_stack_slot(0xB, 0x5b00_0000, 0x5b00_1000);
            assert!(
                take_peer_reg_capture().is_empty(),
                "A's pairing words are not B's"
            );
        });
    })
    .join()
    .expect("B's initiator");
    let expected_a: Vec<(u32, usize, usize)> = if remap_on {
        vec![
            (0xA, 0x5a00_0000, 0x5a00_1000),
            (0xA, 0x5a00_0008, 0x5a00_2000),
        ]
    } else {
        Vec::new()
    };
    with_pause_ledger(&a, || {
        let mut values = Vec::new();
        peer_stack_slot_values_into(&mut values, |v| v != 0x5a00_2000);
        let want: Vec<usize> = if remap_on {
            vec![0x5a00_1000]
        } else {
            Vec::new()
        };
        assert_eq!(values, want, "the filter applies, over A's words only");
        assert!(!peer_stack_slots_saturated());
        assert_eq!(
            take_peer_stack_slots(),
            expected_a,
            "B's pause neither cleared nor joined A's"
        );
        assert!(
            take_peer_stack_slots().is_empty(),
            "a drain empties the buffer"
        );
        assert_eq!(
            take_peer_reg_capture(),
            vec![(0xA, PEER_REG_STACK_HELPER, 0x5a00_3000)]
        );
    });
    assert_eq!(
        b.stack_slots.lock().len(),
        usize::from(remap_on),
        "B's capture is still B's to fold"
    );
    assert!(
        std::thread::spawn(take_peer_stack_slots)
            .join()
            .expect("unbound drain")
            .is_empty(),
        "an unbound thread drains its own empty fallback"
    );
    // A capture a pause never folded is discarded when the next one opens.
    with_pause_ledger(&a, || record_peer_stack_slot(0xA, 0x5a00_0010, 0x5a00_4000));
    a.open();
    assert!(
        a.stack_slots.lock().is_empty(),
        "a pause opens with no captures"
    );
}

/// Opening a pause zeroes every row of the bound ledger, including the
/// take-over times -- which only a take-over (`reset_xt_cycle`) used to
/// zero, so a pause without one reported the previous take-over's times.
#[test]
fn opening_a_pause_zeroes_the_whole_ledger() {
    let l = Arc::new(PauseLedger::new());
    with_pause_ledger(&l, || {
        add_peer_proven_jit_depth(4);
        add_xt_cycle_pinned_jit_depth(Some(6));
        publish_xt_post_quota_ns(700);
        add_xt_pass_ns(90);
        assert_eq!(peer_proven_jit_depth(), 4);
        assert_eq!(xt_cycle_pinned_jit_depth(), 6);
        assert_eq!((xt_post_quota_ns(), xt_pass_ns()), (700, 90));
        add_xt_cycle_pinned_jit_depth(None);
        assert_eq!(xt_cycle_pinned_jit_depth(), 0, "inexact voids the credit");
        reset_peer_proven_jit_depth();
        assert_eq!(peer_proven_jit_depth(), 0);
        assert_eq!(xt_cycle_pinned_jit_depth(), 0);
        add_xt_cycle_pinned_jit_depth(Some(1));
        assert_eq!(xt_cycle_pinned_jit_depth(), 1, "the poison is lifted");
        assert_eq!((xt_post_quota_ns(), xt_pass_ns()), (0, 0));
    });
}

/// gch h1/yp: the conservatively deposited snapshot words are one pause's
/// gather; the next pause opens without them.
#[test]
fn gch_h1yp_a_pause_opens_without_the_previous_conservative_deposit_words() {
    let l = Arc::new(PauseLedger::new());
    with_pause_ledger(&l, || {
        publish_conservative_deposit_words(vec![0x7000_0010, 0x7000_0200]);
        let mut words = Vec::new();
        conservative_deposit_words_into(&mut words, |v| v != 0x7000_0200);
        assert_eq!(words, vec![0x7000_0010], "the filter applies");
        reset_peer_proven_jit_depth();
        words.clear();
        conservative_deposit_words_into(&mut words, |_| true);
        assert!(words.is_empty(), "a pause opens with no deposit words");
    });
}

/// gc-common w9-f: the six take-over counts (`XT_*_LAST_CYCLE` process
/// statics until then) are the bound ledger's. VM B's take-over, on B's
/// initiator, neither adds to VM A's counts nor zeroes them -- the zeroing
/// is what could erase A's `unclassified` between A's passes and A's
/// verdict. Opening a pause zeroes them, and an unbound thread reads its
/// own (empty) fallback.
///
/// Clears through the ledger itself rather than `reset_xt_cycle` (which
/// was a process-wide verdict reset until gc-common w10-g).
#[test]
fn the_takeover_counts_are_the_bound_vms_only() {
    let a = Arc::new(PauseLedger::new());
    let b = Arc::new(PauseLedger::new());
    with_pause_ledger(&a, || {
        publish_xt_pass(2, 1, 7);
        publish_xt_helper_window(3, 4);
        assert_eq!(xt_cycle_coverage(), (1, 2, 1, 7, 3, 4));
    });
    let b2 = Arc::clone(&b);
    std::thread::spawn(move || {
        with_pause_ledger(&b2, || {
            publish_xt_pass(5, 0, 9);
            assert_eq!(xt_cycle_coverage(), (1, 5, 0, 9, 0, 0), "B's pass is B's");
            // B's next take-over opens its cycle.
            b2.xt.clear();
            assert_eq!(xt_cycle_coverage(), (0, 0, 0, 0, 0, 0));
        });
    })
    .join()
    .expect("B's initiator");
    with_pause_ledger(&a, || {
        assert_eq!(
            xt_cycle_coverage(),
            (1, 2, 1, 7, 3, 4),
            "B's take-over must neither add to nor zero A's counts"
        );
    });
    assert_eq!(
        std::thread::spawn(xt_cycle_coverage)
            .join()
            .expect("unbound reader"),
        (0, 0, 0, 0, 0, 0),
        "an unbound thread reads its own empty fallback"
    );
    a.open();
    assert_eq!(a.xt.snapshot(), (0, 0, 0, 0, 0, 0), "a pause opens at zero");
}

/// gc-common w10-g: the take-over verdict (`TAKEOVER_VERDICT`, a process
/// static until then) is the bound ledger's. VM B's pause opening, and VM
/// B's take-over, on B's initiator, leave VM A's verdict alone -- the
/// opening used to reset it to `NONE`, i.e. licence `Move` for the peer A
/// froze. Opening A's own pause clears it, and an unbound thread reads its
/// own fallback.
#[test]
fn a_frozen_native_peer_has_an_explicit_vm_guard_without_a_jit_entry() {
    std::thread::spawn(|| {
        let a = Arc::new(PauseLedger::new());
        let b = Arc::new(PauseLedger::new());
        assert!(!compiled_frames_live());
        with_pause_ledger(&a, || {
            publish_takeover_verdict(TakeoverVerdict {
                frozen: 1,
                ..TakeoverVerdict::NONE
            });
            assert!(
                frozen_peers_live(),
                "a frozen native peer needs an explicit collector guard"
            );
            assert!(
                !compiled_frames_live(),
                "claim protection must not manufacture a JIT entry"
            );
            with_pause_ledger(&b, || {
                assert!(
                    !frozen_peers_live(),
                    "another VM must not inherit A's refusal"
                );
            });
            assert!(frozen_peers_live(), "A's guard must survive B's scope");
            publish_takeover_verdict(TakeoverVerdict {
                helper_windows: 1,
                ..TakeoverVerdict::NONE
            });
            assert!(!frozen_peers_live());
            assert!(
                conservative_peers_live(),
                "a pinned A5 band has no JIT entry but still needs its pins honoured"
            );
            a.open();
            assert!(
                !frozen_peers_live(),
                "the next pause must not inherit a stale guard"
            );
        });
    })
    .join()
    .unwrap();
}

#[test]
fn the_takeover_verdict_is_the_bound_vms_only() {
    let a = Arc::new(PauseLedger::new());
    let b = Arc::new(PauseLedger::new());
    let frozen = TakeoverVerdict {
        frozen: 1,
        pins_complete: false,
        ..TakeoverVerdict::NONE
    };
    assert_eq!(frozen.licence(true), MoveLicence::NonMoving);
    // Opened through the ledger, not `reset_peer_proven_jit_depth`, which
    // also clears the helper-window pins (per VM since gc-common w21-e,
    // and this thread's own under `cfg(test)`).
    a.open();
    with_pause_ledger(&a, || {
        publish_takeover_verdict(frozen);
        assert_eq!(takeover_verdict(), frozen);
        assert!(takeover_forbids_unpinnable_move());
    });
    let b2 = Arc::clone(&b);
    std::thread::spawn(move || {
        with_pause_ledger(&b2, || {
            // B's request opens B's pause, B's take-over runs and reads
            // nothing frozen.
            b2.open();
            reset_xt_cycle();
            publish_takeover_verdict(TakeoverVerdict::NONE);
            assert_eq!(takeover_verdict(), TakeoverVerdict::NONE);
        });
    })
    .join()
    .expect("B's initiator");
    with_pause_ledger(&a, || {
        assert_eq!(
            takeover_verdict(),
            frozen,
            "B's pause must neither reset nor overwrite A's verdict"
        );
        assert_eq!(
            may_move_under_takeover(true),
            MoveLicence::NonMoving,
            "A's collector still reads A's frozen peer"
        );
    });
    assert_eq!(
        std::thread::spawn(takeover_verdict)
            .join()
            .expect("unbound reader"),
        TakeoverVerdict::NONE,
        "an unbound thread reads its own fallback"
    );
    a.open();
    with_pause_ledger(&a, || {
        assert_eq!(
            takeover_verdict(),
            TakeoverVerdict::NONE,
            "A's next pause opens clear"
        );
    });
    assert_eq!(
        TakeoverVerdict::UNKNOWN.licence(true),
        MoveLicence::NonMoving
    );
    assert_eq!(
        TakeoverVerdict::UNKNOWN.licence(false),
        MoveLicence::NonMoving
    );
}

/// gc-common w21-e: the moving-young coverage rows (the verdict, its
/// reasons, the un-rewritable flag, the scan count and the helper-window
/// pins; six process statics until then) are the bound ledger's. VM B's
/// coverage open, on B's initiator, leaves VM A's rows alone -- it used to
/// erase A's "incomplete" and A's pins, i.e. licence a move for a peer A
/// froze -- and B's marks, scans and pins never reach A's collector.
#[test]
fn the_coverage_rows_are_the_bound_vms_only() {
    const PIN_A: usize = 0x7a21_0000;
    const PIN_B: usize = 0x7b21_0000;
    let a = Arc::new(PauseLedger::new());
    let b = Arc::new(PauseLedger::new());
    with_pause_ledger(&a, || {
        begin_moving_young_coverage_cycle();
        mark_moving_young_coverage_incomplete_because(incomplete_reason::XT_TAKEOVER);
        note_conservative_jit_scan();
        add_xt_cycle_pinned_jit_roots(&[PIN_A, PIN_A]);
    });
    let b2 = Arc::clone(&b);
    std::thread::spawn(move || {
        with_pause_ledger(&b2, || {
            // B's request opens B's coverage cycle...
            begin_moving_young_coverage_cycle();
            assert!(
                !moving_young_coverage_incomplete(),
                "A's verdict is not B's"
            );
            assert!(!unrewritable_peer_state());
            assert_eq!(conservative_jit_scans(), 0, "A's scan is not B's");
            assert!(!pinned_jit_roots_snapshot().contains(&PIN_A));
            // ...and B's pause records its own.
            mark_moving_young_coverage_incomplete_because(
                incomplete_reason::UNPUBLISHED_FRAME_OOP,
            );
            add_xt_cycle_pinned_jit_roots(&[PIN_B]);
        });
    })
    .join()
    .expect("B's initiator");
    with_pause_ledger(&a, || {
        assert!(
            moving_young_coverage_incomplete(),
            "B's coverage open must not erase A's verdict"
        );
        assert_eq!(
            moving_young_incomplete_reason(),
            incomplete_reason::XT_TAKEOVER
        );
        assert_eq!(
            moving_young_incomplete_reason_mask(),
            1usize << incomplete_reason::XT_TAKEOVER,
            "B's reason is not A's"
        );
        assert!(unrewritable_peer_state());
        assert_eq!(conservative_jit_scans(), 1);
        let pins = pinned_jit_roots_snapshot();
        assert!(
            pins.contains(&PIN_A) && !pins.contains(&PIN_B),
            "A's collector pins A's helper windows, all of them, and none of B's"
        );
        assert_eq!(xt_cycle_pinned_jit_root_count(), 1, "a pin is kept once");
    });
    assert_eq!(
        b.coverage_snapshot(),
        CoverageSnapshot {
            incomplete: true,
            reason: incomplete_reason::UNPUBLISHED_FRAME_OOP,
            reason_mask: 1usize << incomplete_reason::UNPUBLISHED_FRAME_OOP,
            unrewritable: false,
            scans: 0,
            xt_pins: vec![PIN_B],
        }
    );
    assert!(
        !std::thread::spawn(moving_young_coverage_incomplete)
            .join()
            .expect("unbound reader"),
        "an unbound thread reads neither VM's rows"
    );
    // A's next collection opens A's rows afresh; clearing B's pins (what
    // every pause's open does) leaves B's verdict to B's next collection.
    with_pause_ledger(&a, begin_moving_young_coverage_cycle);
    assert_eq!(a.coverage_snapshot(), CoverageSnapshot::default());
    with_pause_ledger(&b, clear_xt_cycle_pinned_jit_roots);
    let b_now = b.coverage_snapshot();
    assert!(b_now.xt_pins.is_empty() && b_now.incomplete);
}

/// gc-common w21-e: a coverage write from a thread no binding covers is not
/// lost. It lands in the orphan rows (here, under `cfg(test)`, the thread's
/// own fallback's), which a bound read ORs in -- the bound ledger's reason
/// first -- and which the next coverage open clears with the ledger's.
#[test]
fn an_unbound_coverage_write_is_seen_by_a_bound_read_and_cleared_by_the_open() {
    const PIN: usize = 0x7c21_0000;
    let a = Arc::new(PauseLedger::new());
    with_pause_ledger(&a, begin_moving_young_coverage_cycle);
    let before = coverage_orphan_writes();
    mark_moving_young_coverage_incomplete_because(incomplete_reason::UNREGISTERED_JIT_FRAME);
    note_conservative_jit_scan();
    add_xt_cycle_pinned_jit_roots(&[PIN]);
    assert!(
        coverage_orphan_writes() >= before + 3,
        "each unbound write is counted"
    );
    assert_eq!(
        a.coverage_snapshot(),
        CoverageSnapshot::default(),
        "an unbound write is not the ledger's own"
    );
    with_pause_ledger(&a, || {
        assert!(
            moving_young_coverage_incomplete(),
            "a bound read sees the orphan rows"
        );
        assert_eq!(
            moving_young_incomplete_reason(),
            incomplete_reason::UNREGISTERED_JIT_FRAME
        );
        assert_eq!(conservative_jit_scans(), 1);
        assert!(pinned_jit_roots_snapshot().contains(&PIN));
        // A bound write goes to the ledger, and its reason comes first.
        mark_moving_young_coverage_incomplete_because(incomplete_reason::XT_HELPER_WINDOW);
        assert_eq!(
            moving_young_incomplete_reason(),
            incomplete_reason::XT_HELPER_WINDOW
        );
        assert_eq!(
            moving_young_incomplete_reason_mask(),
            (1usize << incomplete_reason::XT_HELPER_WINDOW)
                | (1usize << incomplete_reason::UNREGISTERED_JIT_FRAME)
        );
    });
    assert!(a.coverage_snapshot().unrewritable);
    assert!(
        !unrewritable_peer_state(),
        "an unbound read sees the orphan rows only"
    );
    // The next open clears the ledger's rows and the orphan rows.
    with_pause_ledger(&a, begin_moving_young_coverage_cycle);
    assert_eq!(a.coverage_snapshot(), CoverageSnapshot::default());
    assert!(!moving_young_coverage_incomplete());
    assert_eq!(conservative_jit_scans(), 0);
    assert_eq!(xt_cycle_pinned_jit_root_count(), 0);
}
