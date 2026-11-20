use super::*;

/// gcf f1/j: republishing the same pin set keeps the entry; a different
/// set replaces it; an empty one removes it.
#[test]
fn republishing_the_same_pin_set_keeps_the_entry() {
    let tid = std::thread::current().id();
    let entry = || recover(pinned_jit_map()).get(&tid).cloned();
    publish_pinned_jit_roots(&[0x1000, 0x2000, 0x1000]);
    let first = entry().expect("published");
    publish_pinned_jit_roots(&[0x2000, 0x1000]);
    assert_eq!(entry(), Some(first));
    publish_pinned_jit_roots(&[0x2000]);
    assert_eq!(entry(), Some([0x2000usize].into_iter().collect()));
    publish_pinned_jit_roots(&[0x2000, 0x3000]);
    assert_eq!(entry().map(|s| s.len()), Some(2));
    publish_pinned_jit_roots(&[]);
    assert_eq!(entry(), None);
}

/// gci i17/dk (i16/dp D3): an unchanged republication returns without the
/// process-global map lock and leaves the same entry; an add, a clear, an
/// empty publication or one past the compare bound drops the thread's
/// record, and the next publication takes the locked path again. Runs on
/// its own thread so its record starts empty.
#[test]
fn gci_i17dk_unchanged_republication_skips_the_map_lock() {
    const A: usize = 0x17d0_1000;
    const B: usize = 0x17d0_2000;
    const C: usize = 0x17d0_3000;
    std::thread::spawn(|| {
        let tid = std::thread::current().id();
        let entry = || recover(pinned_jit_map()).get(&tid).cloned();
        assert!(!published_pins_answer(&[A]), "no record before any publication");
        publish_pinned_jit_roots(&[A, B, A]);
        let first = entry().expect("published");
        assert!(published_pins_answer(&[B, A]));
        assert!(published_pins_answer(&[A, B, B]));
        assert!(!published_pins_answer(&[A]));
        assert!(!published_pins_answer(&[A, B, C]));

        // Another thread holds the map lock: the unchanged republication
        // must return while it is held.
        let (held_tx, held_rx) = std::sync::mpsc::channel::<()>();
        let (done_tx, done_rx) = std::sync::mpsc::channel::<()>();
        let holder = std::thread::spawn(move || {
            let map = recover(pinned_jit_map());
            held_tx.send(()).expect("held");
            let returned = done_rx
                .recv_timeout(std::time::Duration::from_secs(10))
                .is_ok();
            drop(map);
            returned
        });
        held_rx.recv().expect("the holder locked the map");
        publish_pinned_jit_roots(&[B, A]);
        let _ = done_tx.send(());
        assert!(
            holder.join().expect("holder"),
            "an unchanged republication waited for the map lock"
        );
        assert_eq!(entry(), Some(first.clone()), "same entry");

        // An add changes the entry: the record no longer answers.
        add_pinned_jit_root(C);
        assert!(!published_pins_answer(&[A, B]));
        assert_eq!(entry().map(|s| s.len()), Some(3));
        publish_pinned_jit_roots(&[A, B]);
        assert_eq!(entry(), Some(first.clone()), "the locked path replaced it");
        assert!(published_pins_answer(&[A, B]));

        // A clear removes the entry and the record.
        clear_pinned_jit_roots();
        assert!(!published_pins_answer(&[A, B]));
        assert_eq!(entry(), None);
        publish_pinned_jit_roots(&[A, B]);
        assert_eq!(entry(), Some(first.clone()), "republished after the clear");

        // Past the compare bound: published, never recorded.
        let wide: Vec<usize> = (0..=PIN_REPUBLISH_COMPARE_MAX)
            .map(|i| 0x17d1_0000 + 8 * i)
            .collect();
        publish_pinned_jit_roots(&wide);
        assert!(!published_pins_answer(&wide));
        assert_eq!(entry().map(|s| s.len()), Some(wide.len()));

        // An empty publication removes the entry and the record.
        publish_pinned_jit_roots(&[A]);
        assert!(published_pins_answer(&[A]));
        publish_pinned_jit_roots(&[]);
        assert!(!published_pins_answer(&[A]));
        assert_eq!(entry(), None);
        publish_pinned_jit_roots(&[A]);
        assert_eq!(entry(), Some([A].into_iter().collect()));
        publish_pinned_jit_roots(&[]);
    })
    .join()
    .expect("worker");
}

/// gcg g3/e H1 (gch h1/yp): the split puts the caller's own entry in
/// `own` and another thread's in `others`. Other tests may publish
/// concurrently, so `others` is checked by containment only.
#[test]
fn gcg_g3e_the_pin_snapshot_split_separates_the_callers_own_words() {
    const MINE: usize = 0x3e3e_1000;
    const PEER: usize = 0x3e3e_2000;
    publish_pinned_jit_roots(&[MINE]);
    let (ready_tx, ready_rx) = std::sync::mpsc::channel();
    let (done_tx, done_rx) = std::sync::mpsc::channel::<()>();
    let peer = std::thread::spawn(move || {
        publish_pinned_jit_roots(&[PEER]);
        ready_tx.send(()).expect("ready");
        let _ = done_rx.recv();
        publish_pinned_jit_roots(&[]);
    });
    ready_rx.recv().expect("the peer published");
    let (own, others) = pinned_jit_roots_snapshot_split();
    assert_eq!(own, vec![MINE], "only the caller's entry is its own");
    assert!(
        others.contains(&PEER),
        "a peer's word is never the caller's"
    );
    assert!(!others.contains(&MINE));
    let all = pinned_jit_roots_snapshot();
    assert!(
        all.contains(&MINE) && all.contains(&PEER),
        "the union is the snapshot"
    );
    done_tx.send(()).expect("done");
    peer.join().expect("peer");
    publish_pinned_jit_roots(&[]);
}

/// gc-common w2-c: the one take-over licence, as a truth table.
#[test]
fn takeover_licence_truth_table() {
    let none = TakeoverVerdict::NONE;
    assert_eq!(none.licence(false), MoveLicence::Move);
    assert_eq!(none.licence(true), MoveLicence::Move);

    let frozen_ok = TakeoverVerdict {
        frozen: 1,
        ..TakeoverVerdict::NONE
    };
    assert_eq!(frozen_ok.licence(true), MoveLicence::MovePinned);
    assert_eq!(
        frozen_ok.licence(false),
        MoveLicence::NonMoving,
        "a backend that cannot pin must not move under a frozen peer"
    );

    let exact_base_only = TakeoverVerdict {
        derived_pointers_resolved: false,
        ..frozen_ok
    };
    assert_eq!(
        exact_base_only.licence(true),
        MoveLicence::NonMoving,
        "an exact-base pin set leaves a cursor's array unpinned"
    );

    let partial = TakeoverVerdict {
        helper_windows: 2,
        frozen: 0,
        pins_complete: false,
        ..TakeoverVerdict::NONE
    };
    assert_eq!(partial.licence(true), MoveLicence::NonMoving);

    let unread = TakeoverVerdict {
        unreadable: 1,
        ..TakeoverVerdict::NONE
    };
    assert_eq!(
        unread.licence(true),
        MoveLicence::NonMoving,
        "a peer whose roots were never seen voids any pin set"
    );
}

/// gc-common w3-g: G1's consumer of the licence refuses exactly the
/// `NonMoving` pauses, and only with the lever on.
#[test]
fn g1_licence_refusal_follows_the_licence_and_the_lever() {
    let unread = TakeoverVerdict {
        unreadable: 1,
        ..TakeoverVerdict::NONE
    };
    assert_eq!(
        g1_licence_refusal(unread, true),
        Some(incomplete_reason::XT_TAKEOVER)
    );
    assert_eq!(g1_licence_refusal(unread, false), None, "opt-in");
    let pinned = TakeoverVerdict {
        frozen: 1,
        ..TakeoverVerdict::NONE
    };
    assert_eq!(
        g1_licence_refusal(pinned, true),
        None,
        "a complete, derived-resolving pin set is honoured by the region pins"
    );
    assert_eq!(g1_licence_refusal(TakeoverVerdict::NONE, true), None);
}

/// gc-common w3-g: a pause that opens without running a take-over must not
/// inherit the previous pause's verdict -- the collectors now read it.
///
/// The marker verdict is deliberately one whose licence is still `Move`
/// (nothing frozen, nobody unread): the global is read by the Generational
/// collector, and a concurrently running collector test must not see a
/// take-over it never had.
#[test]
fn opening_a_pause_clears_the_takeover_verdict() {
    let marker = TakeoverVerdict {
        pins_complete: false,
        ..TakeoverVerdict::NONE
    };
    assert_eq!(marker.licence(false), MoveLicence::Move);
    publish_takeover_verdict(marker);
    reset_peer_proven_jit_depth();
    assert_eq!(takeover_verdict(), TakeoverVerdict::NONE);
    assert!(!takeover_forbids_unpinnable_move());
}

/// gc-common w2-c: take-over captures stop at half the buffer, so a deep
/// frozen stack (captured first in every cycle) can never crowd out the
/// helper-window captures the blocked-wake remap depends on. Exercised on a
/// local buffer: the real one is process-global and other tests use it.
#[test]
fn takeover_captures_leave_half_the_buffer_to_the_repair() {
    let mut buf = Vec::new();
    let mut admitted = 0usize;
    for i in 0..PEER_STACK_SLOTS_CAP {
        if push_capped(&mut buf, (1, i * 8, i), TAKEOVER_STACK_SLOTS_CAP) {
            admitted += 1;
        }
    }
    assert_eq!(admitted, TAKEOVER_STACK_SLOTS_CAP);
    assert!(TAKEOVER_STACK_SLOTS_CAP < PEER_STACK_SLOTS_CAP);
    // The helper-window capture after a saturating take-over still lands.
    assert!(push_capped(
        &mut buf,
        (2, 0x1000, 0x2000),
        PEER_STACK_SLOTS_CAP
    ));
    assert_eq!(buf.last(), Some(&(2, 0x1000, 0x2000)));
    // And the hard cap still holds for everyone.
    while push_capped(&mut buf, (2, 0, 0), PEER_STACK_SLOTS_CAP) {}
    assert_eq!(buf.len(), PEER_STACK_SLOTS_CAP);
}

/// gc-common 2026-09-23 (lane A) — the empty-publication fast path must
/// never leave a stale entry behind: every transition a thread can make
/// through the three writers (`publish` non-empty / empty, `add`,
/// `clear`) has to be visible in the union the collectors read.
///
/// Runs on its own thread so the thread-local "may own" hint starts from
/// its initial state, and uses addresses no other test publishes (the map
/// is process-global and other tests write to it concurrently).
#[test]
fn empty_publication_fast_path_never_strands_an_entry() {
    std::thread::spawn(|| {
        const A: usize = 0x7EAD_0000_1000;
        const B: usize = 0x7EAD_0000_2000;
        let has = |a: usize| pinned_jit_roots_snapshot().contains(&a);

        // Nothing owned: the empty publication takes the fast path.
        publish_pinned_jit_roots(&[]);
        assert!(!has(A) && !has(B));

        publish_pinned_jit_roots(&[A]);
        assert!(has(A), "a non-empty publication must be visible");
        publish_pinned_jit_roots(&[]);
        assert!(!has(A), "an empty publication must withdraw the entry");

        add_pinned_jit_root(B);
        assert!(has(B), "add_pinned_jit_root must be visible");
        publish_pinned_jit_roots(&[]);
        assert!(!has(B), "an empty publication after `add` must withdraw it");

        publish_pinned_jit_roots(&[A, B]);
        clear_pinned_jit_roots();
        assert!(!has(A) && !has(B), "clear must withdraw the entry");
        publish_pinned_jit_roots(&[]);
        assert!(!has(A) && !has(B));

        add_pinned_jit_root(A);
        clear_pinned_jit_roots();
        add_pinned_jit_root(B);
        assert!(has(B) && !has(A));
        publish_pinned_jit_roots(&[]);
        assert!(!has(B));
    })
    .join()
    .expect("fast-path test thread panicked");
}

/// Provenance is wanted only between a collection's `clear_pinned_jit_roots`
/// and the same thread's next publication. Per thread, so a fresh thread
/// isolates the test from every other one.
#[test]
fn provenance_is_wanted_only_during_the_collections_own_scan() {
    std::thread::spawn(|| {
        assert!(!jit_root_provenance_wanted(), "a thread starts unarmed");
        // A deposit or a native call's snapshot: never armed.
        publish_pinned_jit_roots(&[]);
        assert!(!jit_root_provenance_wanted());
        // The collection's root gatherer.
        clear_pinned_jit_roots();
        assert!(
            jit_root_provenance_wanted(),
            "the collection's scan records"
        );
        publish_pinned_jit_roots(&[]);
        assert!(
            !jit_root_provenance_wanted(),
            "its publication ends the scan"
        );
    })
    .join()
    .expect("provenance arming test thread panicked");
}

/// The vacated ledger is process-global and its gate is a process-global
/// byte, so the tests that arm it must not run beside each other.
static VACATED_TEST_LOCK: parking_lot::Mutex<()> = parking_lot::Mutex::new(());

fn pointer_map_of(pairs: &[(usize, usize)]) -> cratonvm_types::PointerMap {
    let mut m = cratonvm_types::PointerMap::default();
    for (k, v) in pairs {
        m.insert(*k, *v);
    }
    m
}

/// The claim the whole instrument rests on: an address the allocator has
/// re-issued is not evidence of anything.
///
/// Until 2026-09-08 the only callers of `note_allocated` were ZGC's, so on
/// `--XX:UseGc Generational` nothing ever removed an entry and the mutator
/// bump-allocated straight back into the semispace the previous cycle had
/// vacated. Every detector then reported freshly allocated young objects as
/// stale references -- the eight `stack[0]` "the frame remap did not reach
/// this slot" reports the BindableTests moving-collector page was written
/// around, whose producer backtrace is `Anewarray` pushing the array
/// `gc_alloc_array` had returned two statements earlier.
#[test]
fn vacated_ledger_forgets_a_re_issued_address() {
    let _g = VACATED_TEST_LOCK.lock();
    set_vacated_frames_enabled_for_test(true);
    reset_vacated_ledger_for_test();

    record_vacated(&pointer_map_of(&[(0x1000, 0x9000)]), 7);
    assert_eq!(
        was_vacated_on(0x1000),
        Some((0x9000, 7)),
        "a moved-from address must be in the ledger with the cycle that moved it"
    );

    note_allocated(&[0x1000]);
    assert_eq!(
        was_vacated(0x1000),
        None,
        "an address the allocator re-issued is no longer evidence of a stale reference"
    );

    reset_vacated_ledger_for_test();
    set_vacated_frames_enabled_for_test(false);
}

/// A TLAB chunk is bump-allocated from without any further call into the
/// heap, so the per-object door cannot see the objects inside it. The
/// range door is the only one that can, and `VmHeap::refill_tlab` is the
/// single chokepoint every backend's TLAB comes through.
#[test]
fn vacated_ledger_forgets_a_whole_re_issued_tlab_chunk() {
    let _g = VACATED_TEST_LOCK.lock();
    set_vacated_frames_enabled_for_test(true);
    reset_vacated_ledger_for_test();

    record_vacated(
        &pointer_map_of(&[(0x2000, 0xa000), (0x2100, 0xa100), (0x3000, 0xb000)]),
        11,
    );
    note_allocated_range(0x2000, 0x2800);

    assert_eq!(was_vacated(0x2000), None, "chunk start must be forgotten");
    assert_eq!(
        was_vacated(0x2100),
        None,
        "chunk interior must be forgotten"
    );
    assert_eq!(
        was_vacated_on(0x3000),
        Some((0xb000, 11)),
        "an address OUTSIDE the chunk must survive -- the purge is a range, not a clear"
    );

    reset_vacated_ledger_for_test();
    set_vacated_frames_enabled_for_test(false);
}

/// gen r5w6/pin10: the range purge on the ordered ledger is half-open,
/// `[lo, hi)`, exactly as the `retain` it replaced: `lo` goes, `hi` and
/// `lo - 8` stay.
#[test]
fn vacated_ledger_range_purge_is_half_open() {
    let _g = VACATED_TEST_LOCK.lock();
    set_vacated_frames_enabled_for_test(true);
    reset_vacated_ledger_for_test();

    record_vacated(
        &pointer_map_of(&[
            (0x6ff8, 0xe000),
            (0x7000, 0xe100),
            (0x7ff8, 0xe200),
            (0x8000, 0xe300),
        ]),
        5,
    );
    note_allocated_range(0x7000, 0x8000);

    assert_eq!(was_vacated_on(0x6ff8), Some((0xe000, 5)), "below the range");
    assert_eq!(was_vacated(0x7000), None, "lo is inside");
    assert_eq!(was_vacated(0x7ff8), None, "the last word inside");
    assert_eq!(was_vacated_on(0x8000), Some((0xe300, 5)), "hi is outside");

    reset_vacated_ledger_for_test();
    set_vacated_frames_enabled_for_test(false);
}

/// The ledger accumulates across cycles on purpose (a stale reference is
/// not necessarily consumed before the next collection), so "vacated" alone
/// carries no date. `was_vacated_on` is what lets a report compare the
/// vacating cycle against the thread's `last_heal_collection` instead of
/// against the current collection count, which at a safepoint is always
/// equal to it and therefore proves nothing.
#[test]
fn vacated_ledger_dates_each_entry_by_its_own_cycle() {
    let _g = VACATED_TEST_LOCK.lock();
    set_vacated_frames_enabled_for_test(true);
    reset_vacated_ledger_for_test();

    record_vacated(&pointer_map_of(&[(0x4000, 0xc000)]), 3);
    record_vacated(&pointer_map_of(&[(0x5000, 0xd000)]), 900);

    assert_eq!(was_vacated_on(0x4000), Some((0xc000, 3)));
    assert_eq!(was_vacated_on(0x5000), Some((0xd000, 900)));

    reset_vacated_ledger_for_test();
    set_vacated_frames_enabled_for_test(false);
}

#[test]
fn enter_leave_round_trip() {
    let d0 = depth();
    let d1 = enter();
    assert_eq!(d1, d0 + 1);
    let d2 = leave();
    assert_eq!(d2, d0);
}

#[test]
fn is_active_reflects_depth() {
    let d0 = depth();
    if d0 == 0 {
        assert!(!is_active());
    }
    let _ = enter();
    assert!(is_active());
    let _ = leave();
}

/// The collector must follow the value the codegen side publishes, not its
/// own read of `gc_flags()`. This interlock is what makes a default flip on
/// the codegen side safe — and what makes a flip of
/// `cratonvm_types::flags::DEFAULT_MOVING_YOUNG` *alone* harmless rather
/// than corrupting, while `jit/src/x64.rs` still parses the raw variable.
#[test]
fn published_gate_wins_over_the_flags_default() {
    // Fresh test thread: unpublished, so the flags value applies.
    assert_eq!(moving_young_enabled(), gc_flags().moving_young);
    publish_moving_young_enabled(true);
    assert!(
        moving_young_enabled(),
        "the collector must honour a codegen-published moving-young decision",
    );
    publish_moving_young_enabled(false);
    assert!(
        !moving_young_enabled(),
        "and it must honour a codegen-published REFUSAL even if the typed \
         config says moving-young is on — the codegen is the side that has \
         to emit the rewritable root map",
    );
}

/// An explicit `System.gc()` takes `divert_non_moving`'s `explicit_full_gc`
/// arm, which names no moving-young condition — so the side-table follow
/// must hold on BOTH published values of the gate.
///
/// This is the regression test for the third recurrence of
/// `TestDefaultInstanceManager.testClassUnloading`: the predicate used to
/// factor `!moving_young_enabled()` across this arm too, so flipping
/// `DEFAULT_MOVING_YOUNG` to `true` disarmed
/// `VmHeap::mirror_pin_deferrable`'s young-mirror deferral everywhere at
/// once, with no test failing and the fix still sitting in the tree. Asserting
/// both gate values is the point — a one-sided assertion would have passed
/// before the flip and after it.
#[test]
fn explicit_full_gc_follows_side_tables_on_either_moving_young_gate() {
    for on in [false, true] {
        publish_moving_young_enabled(on);
        assert!(
            !young_marker_follows_side_tables(),
            "no System.gc() pending and no JIT frame: with moving-young={on} \
             this thread cannot promise the non-moving young marker",
        );
        request_major_gc();
        assert!(
            young_marker_follows_side_tables(),
            "an explicit System.gc() diverts to the non-moving young cycle on \
             its own (`divert_non_moving`'s `explicit_full_gc` term), so a \
             young class mirror is reachable through `mirror_pin` and must not \
             be rooted unconditionally — moving-young={on} is irrelevant here",
        );
        assert!(take_major_gc_request());
    }
}

/// The other usable arm, and the one that DOES carry the guard: a live JIT
/// frame forces the non-moving sweep only while moving-young is off
/// (`has_conservative_roots && !moving_young`).
#[test]
fn conservative_jit_roots_follow_side_tables_only_without_moving_young() {
    let _ = enter();
    assert!(is_active());

    publish_moving_young_enabled(false);
    assert!(
        young_marker_follows_side_tables(),
        "conservative JIT roots divert to the non-moving sweep when \
         moving-young is off",
    );

    publish_moving_young_enabled(true);
    assert!(
        !young_marker_follows_side_tables(),
        "with moving-young on, a live JIT frame no longer forces the \
         non-moving sweep — the cycle may relocate, and the moving closure \
         seeds strictly from the direct root set",
    );

    let _ = leave();
}

#[test]
fn coverage_cycle_resets_verdict_and_reason() {
    begin_moving_young_coverage_cycle();
    assert!(!moving_young_coverage_incomplete());
    assert_eq!(moving_young_incomplete_reason(), incomplete_reason::NONE);

    mark_moving_young_coverage_incomplete_because(incomplete_reason::UNREGISTERED_JIT_FRAME);
    assert!(moving_young_coverage_incomplete());
    assert_eq!(
        moving_young_incomplete_reason(),
        incomplete_reason::UNREGISTERED_JIT_FRAME
    );

    // First reason of a cycle wins — later ones are consequences of it.
    mark_moving_young_coverage_incomplete_because(incomplete_reason::XT_TAKEOVER);
    assert_eq!(
        moving_young_incomplete_reason(),
        incomplete_reason::UNREGISTERED_JIT_FRAME
    );

    begin_moving_young_coverage_cycle();
    assert!(!moving_young_coverage_incomplete());
    assert_eq!(moving_young_incomplete_reason(), incomplete_reason::NONE);
}

/// HIB-GCOVERHEAD-HALFFULL.1 regression.
///
/// `unrewritable_peer_state` must stay STRICTLY narrower than
/// `moving_young_coverage_incomplete`. The two were the same flag when the
/// promotion gate was written; once moving-young became the default, the
/// wide verdict was set on essentially every JIT-active cycle, and reading
/// it as "un-rewritable peer state" switched off selective promotion
/// VM-wide — the young generation lost its only drain and the VM raised
/// `OutOfMemoryError` on a 49%-full heap.
///
/// Asserted as a DECISION, not a side effect: an end-to-end "does the heap
/// still OOM" test cannot distinguish this from any other allocation defect,
/// and would pass again the moment some unrelated change made young big
/// enough to hide it.
#[test]
fn unrewritable_peer_state_is_narrower_than_the_coverage_verdict() {
    // The ordinary case, and the one that regressed: a compiled frame on
    // THIS thread whose oop map is unproven. Relocation is off; promotion
    // must not be, because the conservative scan still covers that frame and
    // selective promotion pins its slot values.
    for reason in [
        incomplete_reason::UNPUBLISHED_FRAME_OOP,
        incomplete_reason::MISSING_EXACT_RBP,
        incomplete_reason::ACTIVE_FRAME_MAP,
        incomplete_reason::PARENT_FRAME_MAP,
        incomplete_reason::NO_PRECISE_MAP,
        incomplete_reason::UNREGISTERED_JIT_FRAME,
        incomplete_reason::OSR_SHADOW,
        incomplete_reason::UNBOUNDED_FRAME_BAND,
        incomplete_reason::FOREIGN_INNERMOST_RBP,
        incomplete_reason::JIT_RELOCATION_UNSUPPORTED,
        // "some peer is in compiled code" — a cooperatively parked peer,
        // covered by its own deposited root snapshot.
        incomplete_reason::CROSS_THREAD_JIT_PEER,
    ] {
        begin_moving_young_coverage_cycle();
        mark_moving_young_coverage_incomplete_because(reason);
        assert!(
            moving_young_coverage_incomplete(),
            "{} must still divert the COPYING collector",
            incomplete_reason::label(reason),
        );
        assert!(
            !unrewritable_peer_state(),
            "{} must NOT disable selective promotion — it describes a frame \
             the conservative scan covers, not state no one can rewrite. \
             Widening this gate is HIB-GCOVERHEAD-HALFFULL.1: the young \
             generation loses its only drain and the VM OOMs on a half-empty \
             heap.",
            incomplete_reason::label(reason),
        );
    }

    // The two the 2026-07-03 gate was actually written for: a peer excused
    // from the STW barrier, which therefore never re-reads its own
    // registers.
    for reason in [
        incomplete_reason::XT_TAKEOVER,
        incomplete_reason::XT_HELPER_WINDOW,
    ] {
        begin_moving_young_coverage_cycle();
        mark_moving_young_coverage_incomplete_because(reason);
        assert!(
            unrewritable_peer_state(),
            "{} MUST disable selective promotion: a frozen peer's register \
             can hold only a derived/interior pointer, which pin-by-value \
             does not protect",
            incomplete_reason::label(reason),
        );
    }

    // Set by a later reason even when an earlier one already claimed the
    // first-wins reason slot.
    begin_moving_young_coverage_cycle();
    mark_moving_young_coverage_incomplete_because(incomplete_reason::UNPUBLISHED_FRAME_OOP);
    assert!(!unrewritable_peer_state());
    mark_moving_young_coverage_incomplete_because(incomplete_reason::XT_TAKEOVER);
    assert_eq!(
        moving_young_incomplete_reason(),
        incomplete_reason::UNPUBLISHED_FRAME_OOP,
        "the recorded reason stays first-wins",
    );
    assert!(
        unrewritable_peer_state(),
        "…but the peer-state verdict must not be first-wins: it is a safety \
         gate, so any cross-thread obligation in the cycle arms it",
    );

    // And the standalone marker (the xt-takeover call site) plus the reset.
    begin_moving_young_coverage_cycle();
    assert!(!unrewritable_peer_state());
    mark_unrewritable_peer_state();
    assert!(unrewritable_peer_state());
    begin_moving_young_coverage_cycle();
    assert!(
        !unrewritable_peer_state(),
        "the verdict is per-cycle and must be cleared with the others",
    );
}

/// gci i3/tk (X2 of `gci-i1jn-takeover-...`): the pinned door records the
/// reason and the incomplete verdict but never the un-rewritable bit; the
/// peer-state reason stays visible to `peer_state_reason_recorded`, and
/// the `_because` door is unchanged.
#[test]
fn gci_i3tk_the_pinned_door_keeps_the_reason_and_not_the_unrewritable_bit() {
    for reason in [
        incomplete_reason::XT_TAKEOVER,
        incomplete_reason::XT_HELPER_WINDOW,
    ] {
        begin_moving_young_coverage_cycle();
        assert!(!peer_state_reason_recorded());
        mark_moving_young_coverage_incomplete_pinned_because(reason);
        assert!(moving_young_coverage_incomplete(), "no Cheney copy");
        assert_eq!(moving_young_incomplete_reason(), reason);
        assert_eq!(moving_young_incomplete_reason_mask(), 1usize << reason);
        assert!(!unrewritable_peer_state(), "selective promotion may run");
        assert!(peer_state_reason_recorded());
        begin_moving_young_coverage_cycle();
        mark_moving_young_coverage_incomplete_because(reason);
        assert!(unrewritable_peer_state(), "the old door still gates promotion");
        assert!(peer_state_reason_recorded());
    }
    begin_moving_young_coverage_cycle();
    mark_moving_young_coverage_incomplete_because(incomplete_reason::UNPUBLISHED_FRAME_OOP);
    assert!(!peer_state_reason_recorded(), "a frame reason is not peer state");
    begin_moving_young_coverage_cycle();
    assert!(!peer_state_reason_recorded());
}

/// The discriminator `gen_heap` and `zgc` branch on, and the mixed unit its
/// name used to deny.
///
/// Two separate properties are pinned here because two separate defects
/// live in them:
///
/// 1. **An EMPTY publication still counts.** "This thread looked and found
///    nothing" and "this thread never looked" are opposite conclusions for
///    a pin-by-value consumer, and the pin set itself cannot hold the
///    difference. If this regressed, a collector would read an empty set as
///    a licence to relocate everything.
/// 2. **The two publication paths carry different units.**
///    `publish_pinned_jit_roots` bumps once per CALL, `add_pinned_jit_root`
///    once per ADDRESS. The accessor was documented as "how many THREADS
///    have published", which is true of neither path taken together. Only
///    non-zero-ness is well defined, and that is what both consumers use.
#[test]
fn conservative_jit_scan_count_is_per_publication_not_per_thread() {
    begin_moving_young_coverage_cycle();
    assert_eq!(
        conservative_jit_scans(),
        0,
        "a freshly opened coverage cycle has no publications",
    );

    publish_pinned_jit_roots(&[]);
    assert_eq!(
        conservative_jit_scans(),
        1,
        "a publication of an EMPTY vector is still a look, and the pin set \
         cannot record that it happened",
    );

    publish_pinned_jit_roots(&[0x1000, 0x2000, 0x3000]);
    assert_eq!(
        conservative_jit_scans(),
        2,
        "publish_pinned_jit_roots counts CALLS, whatever the slice length",
    );

    add_pinned_jit_root(0x4000);
    add_pinned_jit_root(0x5000);
    assert_eq!(
        conservative_jit_scans(),
        4,
        "add_pinned_jit_root counts ADDRESSES — the magnitude mixes two \
         units and may not be read as a thread or scan count",
    );

    // Per-CYCLE: the count describes the pause being opened, not the run.
    begin_moving_young_coverage_cycle();
    assert_eq!(conservative_jit_scans(), 0);

    clear_pinned_jit_roots();
}

/// CHARACTERIZATION TEST of the FUNCTIONS: `CONSERVATIVE_JIT_SCANS` is
/// cleared only by [`begin_moving_young_coverage_cycle`], not by
/// [`reset_peer_proven_jit_depth`], so an open that runs AFTER a peer's
/// deposit discards the peer's "I looked".
///
/// That used to be the production order -- the initiator opened its cycle
/// during root gathering, after its peers had parked -- and it was a hole:
/// with `g1_only_jit_pins()` on a non-G1 heap the initiator publishes
/// nothing, the count read zero while peers held conservative roots, and
/// `unrewritable_conservative_jit_roots` went false
/// (`docs/feature-designs/generative-gc-architecture.md`).
///
/// It is no longer the production order (re-audited gc-common w8-d): since
/// w2-a the open IS the winning request's `open_cycle`
/// (`GcBarrier::request_stw_opening_cycle`), under the barrier lock and
/// before `stw_requested` is visible, so no peer of the pause can have
/// deposited yet. The test stays because the function-level fact is what
/// makes that ordering load-bearing: a door that opened its cycle late
/// again would reopen the hole, and ZGC refuses on `== 0` (`zgc.rs`).
#[test]
fn conservative_jit_scans_are_erased_by_the_coverage_cycle_open() {
    begin_moving_young_coverage_cycle();

    // Stand in for a peer that parked on `stw_requested` and deposited.
    publish_pinned_jit_roots(&[0x1000]);
    assert_eq!(conservative_jit_scans(), 1, "the peer's look was recorded");

    // Clearing the sibling ledgers — what `request_stw` does — leaves this
    // one alone. Stated because adding the clear THERE looks like the fix.
    reset_peer_proven_jit_depth();
    assert_eq!(
        conservative_jit_scans(),
        1,
        "reset_peer_proven_jit_depth does not touch this counter; adding it \
         there would not be sufficient either, because the initiator's \
         begin_moving_young_coverage_cycle still runs later",
    );

    // An open AFTER the deposit discards it -- which is why every door
    // opens its cycle at the request, before any peer can deposit.
    begin_moving_young_coverage_cycle();
    assert_eq!(
        conservative_jit_scans(),
        0,
        "an open after a deposit discards it; see this test's doc comment \
         before changing this expectation",
    );

    clear_pinned_jit_roots();
}

/// gen r4w3/young2: `note_conservative_jit_scan` is the counter half of a
/// publication and nothing else — it bumps exactly as an empty
/// `publish_pinned_jit_roots` does and leaves every published pin alone.
#[test]
fn noting_a_scan_bumps_the_count_and_leaves_the_pins_alone() {
    begin_moving_young_coverage_cycle();
    clear_pinned_jit_roots();
    publish_pinned_jit_roots(&[0x2000]);
    assert_eq!(conservative_jit_scans(), 1);

    note_conservative_jit_scan();
    assert_eq!(conservative_jit_scans(), 2, "one bump per noted scan");
    assert!(
        pinned_jit_roots_snapshot().contains(&0x2000),
        "noting a scan must not drop a pin this thread already published",
    );

    begin_moving_young_coverage_cycle();
    assert_eq!(conservative_jit_scans(), 0, "still per-cycle");
    clear_pinned_jit_roots();
}

#[test]
fn every_incomplete_reason_has_a_label() {
    for code in incomplete_reason::NONE..incomplete_reason::COUNT {
        assert_ne!(
            incomplete_reason::label(code),
            "unknown",
            "reason code {code} needs a label for the warn-level fallback diagnostic — \
             and `incomplete_reason::COUNT` must match the highest defined code + 1, \
             because it sizes the per-reason fallback histogram",
        );
    }
    assert_eq!(
        incomplete_reason::label(incomplete_reason::COUNT),
        "unknown",
        "COUNT must be one PAST the last defined reason",
    );
}

/// A repeat of the 2026-07-01 "validated" run — which declared moving-young
/// working after executing ZERO moving cycles — must be impossible to
/// reproduce silently. The pair (cycle count, per-reason fallback
/// histogram) is what makes that so, and the histogram must attribute the
/// fallback to the obligation that actually forced it.
#[test]
fn fallback_histogram_attributes_the_blocking_obligation() {
    let before = moving_young_fallback_reason_counts();
    assert_eq!(moving_young_cycle_count(), 0, "fresh test thread");

    begin_moving_young_coverage_cycle();
    mark_moving_young_coverage_incomplete_because(incomplete_reason::UNPUBLISHED_FRAME_OOP);
    // A later, consequential reason must NOT steal the attribution.
    mark_moving_young_coverage_incomplete_because(incomplete_reason::CROSS_THREAD_JIT_PEER);
    record_moving_young_coverage_fallback();

    let after = moving_young_fallback_reason_counts();
    assert_eq!(
        after[incomplete_reason::UNPUBLISHED_FRAME_OOP],
        before[incomplete_reason::UNPUBLISHED_FRAME_OOP] + 1,
    );
    assert_eq!(
        after[incomplete_reason::CROSS_THREAD_JIT_PEER],
        before[incomplete_reason::CROSS_THREAD_JIT_PEER],
        "only the FIRST reason of a cycle is the one that forced the decision",
    );
    assert_eq!(
        moving_young_cycle_count(),
        0,
        "a fallback is the OPPOSITE of a moving cycle; the two counters must \
         never both be bumped for one collection",
    );
}

#[test]
fn leave_saturates_at_zero() {
    // Force the counter to a known state, then over-leave.
    // We can't reset reliably from a test (other tests may share the
    // counter via parallel execution), so we just confirm the monotone
    // invariant: after one extra leave, depth never goes below 0.
    let _ = leave();
    let _ = leave();
    // Cannot assert == 0 because other concurrent tests may have
    // pushed; only assert the counter is well-formed (no UB / wrap).
    let d = depth();
    assert!(d <= usize::MAX / 2);
}
