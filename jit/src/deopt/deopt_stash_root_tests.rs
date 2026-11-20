use super::*;

#[test]
fn precise_native_proof_declines_a_borrowed_stash() {
    clear_stashes();
    assert!(deopt_stashes_are_empty());
    LAST_DEOPT.with(|cell| {
        let _borrow = cell.borrow_mut();
        assert!(!deopt_stashes_are_empty());
    });
    LAST_EXCEPTIONAL.with(|cell| {
        let _borrow = cell.borrow_mut();
        assert!(!deopt_stashes_are_empty());
    });
    assert!(deopt_stashes_are_empty());
}

/// Both stashes empty, whatever a previous test on this thread did.
fn clear_stashes() {
    let _ = take_last_deopt();
    let _ = take_exceptional_frame();
}

fn scanned_addresses() -> Vec<u64> {
    let mut out = Vec::new();
    for_each_stashed_deopt_object(|a| out.push(a));
    out.sort_unstable();
    out
}

/// A frame that puts a distinct object address in EVERY container that can
/// hold one, so a walk that forgets a family fails loudly rather than
/// silently under-rooting:
///
/// * `locals`                                    → `0x1000`
/// * `stack`                                     → `0x2000`
/// * `monitors[].object`                         → `0x3000`
/// * a scalar-replaced object's `field_values`   → `0x4000`
/// * the inlined `caller_frames` chain (locals)  → `0x5000`
/// * a nested virtual object inside that field   → `0x6000`
///
/// Interleaved with non-reference slots of every width, and with a null
/// (`Object(0)`) slot, both of which must be left strictly alone.
fn frame_with_one_object_per_container(base: u64) -> ReconstructedFrame {
    let nested = FrameValue::VirtualObject(VirtualObjectState {
        array_element_type: None,
        id: 2,
        class_id: 9,
        num_fields: 1,
        field_values: vec![FrameValue::Object(base + 0x6000)],
    });
    ReconstructedFrame {
        class_id: 0,
        method_key: "craton/probe/Stash.m:()V".to_string(),
        bci: 4,
        locals: vec![
            FrameValue::Object(base + 0x1000),
            FrameValue::Int(-7),
            FrameValue::Object(0),                  // null — never a root
            FrameValue::Long(base as i64 + 0x1000), // an int-typed lookalike
            FrameValue::Undefined,
        ],
        stack: vec![
            FrameValue::Double(0x4059_0000_0000_0000),
            FrameValue::Object(base + 0x2000),
            FrameValue::VirtualObject(VirtualObjectState {
                array_element_type: None,
                id: 1,
                class_id: 8,
                num_fields: 2,
                field_values: vec![FrameValue::Object(base + 0x4000), nested],
            }),
            FrameValue::VirtualObjectRef(1),
        ],
        monitors: vec![MonitorInfo {
            object: FrameValue::Object(base + 0x3000),
            lock_depth: 1,
            relock: true,
        }],
        semantics: ResumeSemantics::REEXECUTE,
        caller_frames: vec![ReconstructedFrame {
            class_id: 0,
            method_key: "craton/probe/Stash.caller:()V".to_string(),
            bci: 0,
            locals: vec![FrameValue::Object(base + 0x5000)],
            stack: Vec::new(),
            monitors: Vec::new(),
            semantics: ResumeSemantics::for_caller_scope(),
            caller_frames: Vec::new(),
        }],
    }
}

fn expected_addresses(base: u64) -> Vec<u64> {
    let mut v = vec![
        base + 0x1000,
        base + 0x2000,
        base + 0x3000,
        base + 0x4000,
        base + 0x5000,
        base + 0x6000,
    ];
    v.sort_unstable();
    v
}

/// Every container that can hold a raw heap address is reachable from the
/// scan half. This is the test that fails if a new `FrameValue` variant or
/// a new frame field starts carrying an address.
#[test]
fn every_object_slot_family_is_offered_to_a_root_scan() {
    clear_stashes();
    restash_last_deopt(frame_with_one_object_per_container(0));
    assert_eq!(scanned_addresses(), expected_addresses(0));
    clear_stashes();
}

/// The exceptional stash is a second, independent thread-local. It is the
/// one with the shortest proven window (an allocating `sig.npe` /
/// `sig.aioobe` / `sig.arithmetic` arm sits between its publication and its
/// drain), so a scan that covered only `LAST_DEOPT` would close the wrong
/// half.
#[test]
fn the_exceptional_stash_is_scanned_too() {
    clear_stashes();
    restash_exceptional_frame(frame_with_one_object_per_container(0));
    assert_eq!(scanned_addresses(), expected_addresses(0));
    clear_stashes();
}

/// gci i9/ds: a peer's stashes, read through its published cell addresses
/// while it is parked (the take-over's "frozen"), yield exactly what its own
/// scan would; a zero pair reads nothing; a cell the owner holds mutably
/// borrowed is refused; reading writes nothing the owner can see.
#[test]
fn gci_i9ds_frozen_peer_stash_is_read_through_its_published_cells() {
    use std::sync::mpsc;
    let (to_main, from_peer) = mpsc::channel::<(usize, usize)>();
    let (to_peer, from_main) = mpsc::channel::<()>();
    let peer = std::thread::spawn(move || {
        clear_stashes();
        restash_last_deopt(frame_with_one_object_per_container(0));
        restash_exceptional_frame(frame_with_one_object_per_container(0x10_0000));
        let cells = stash_cell_addrs().expect("live TLS");
        let scans_before = STASH_ROOT_SCANS.with(|c| c.get());
        to_main.send(cells).unwrap();
        // "Frozen": runs nothing until the reader is done.
        from_main.recv().unwrap();
        // The reader bumped no scan count and left both stashes intact.
        assert_eq!(STASH_ROOT_SCANS.with(|c| c.get()), scans_before);
        assert_eq!(stashed_deopt_object_count(), 12);
        clear_stashes();
    });
    let (deopt, exceptional) = from_peer.recv().unwrap();
    assert_ne!(deopt, 0);
    assert_ne!(exceptional, 0);
    assert_ne!(Some((deopt, exceptional)), stash_cell_addrs(), "per-thread cells");
    let mut seen = Vec::new();
    // SAFETY: the peer is alive and parked on `from_main` for the call.
    let (frames, offered) = unsafe {
        for_each_stashed_deopt_object_of_frozen_peer(deopt, exceptional, |a| seen.push(a))
    };
    seen.sort_unstable();
    let mut expected = expected_addresses(0);
    expected.extend(expected_addresses(0x10_0000));
    expected.sort_unstable();
    assert_eq!(seen, expected);
    assert_eq!((frames, offered), (2, 12));
    // SAFETY: zero addresses are skipped without a dereference.
    let none = unsafe { for_each_stashed_deopt_object_of_frozen_peer(0, 0, |_| panic!()) };
    assert_eq!(none, (0, 0));
    to_peer.send(()).unwrap();
    peer.join().unwrap();

    // A cell borrowed mutably (only Rust stash code does that) is refused.
    clear_stashes();
    restash_last_deopt(frame_with_one_object_per_container(0));
    let (own_deopt, own_exc) = stash_cell_addrs().unwrap();
    LAST_DEOPT.with(|cell| {
        let _held = cell.borrow_mut();
        // SAFETY: this thread's own live cells, read on this thread.
        let got = unsafe {
            for_each_stashed_deopt_object_of_frozen_peer(own_deopt, own_exc, |_| {})
        };
        assert_eq!(got, (0, 0));
    });
    clear_stashes();
}

/// Both stashes can be occupied at once — a deopt frame belonging to a
/// callee standing while this method publishes an exceptional frame. The
/// scan must yield the union, not whichever it happens to look at first.
#[test]
fn both_stashes_are_scanned_when_both_are_occupied() {
    clear_stashes();
    restash_last_deopt(frame_with_one_object_per_container(0));
    restash_exceptional_frame(frame_with_one_object_per_container(0x10_0000));
    let mut expected = expected_addresses(0);
    expected.extend(expected_addresses(0x10_0000));
    expected.sort_unstable();
    assert_eq!(scanned_addresses(), expected);
    assert_eq!(stashed_deopt_object_count(), 12);
    clear_stashes();
}

/// `Object(0)` is `null`. Offering it as a root would hand the collector a
/// zero address to resolve; callers are entitled to assume every value they
/// receive is a live reference.
#[test]
fn null_object_slots_are_not_offered_as_roots() {
    clear_stashes();
    restash_last_deopt(ReconstructedFrame {
        class_id: 0,
        method_key: String::new(),
        bci: 0,
        locals: vec![FrameValue::Object(0), FrameValue::Object(0)],
        stack: vec![FrameValue::Object(0)],
        monitors: vec![MonitorInfo {
            object: FrameValue::Object(0),
            lock_depth: 1,
            relock: true,
        }],
        semantics: ResumeSemantics::REEXECUTE,
        caller_frames: Vec::new(),
    });
    assert!(scanned_addresses().is_empty());
    assert_eq!(stashed_deopt_object_count(), 0);
    clear_stashes();
}

#[test]
fn nothing_is_offered_when_no_frame_is_stashed() {
    clear_stashes();
    assert!(scanned_addresses().is_empty());
    assert_eq!(stashed_deopt_object_count(), 0);
}

/// The real shape: a moving collection relocates every object the frame
/// names, and the drained frame must read the POST-move addresses. A frame
/// that kept its pre-move addresses hands the interpreter from-space
/// pointers — the failure this whole mechanism exists to prevent.
#[test]
fn a_moving_collection_rewrites_every_stashed_object_slot() {
    clear_stashes();
    restash_last_deopt(frame_with_one_object_per_container(0));
    restash_exceptional_frame(frame_with_one_object_per_container(0x10_0000));

    // The scan half runs first, exactly as it does in a real collection.
    let scanned = scanned_addresses();
    assert_eq!(scanned.len(), 12);

    // Every scanned object moved by +0x8000_0000.
    const DELTA: u64 = 0x8000_0000;
    remap_stashed_deopt_objects(|a| Some(a + DELTA));

    let deopt = take_last_deopt().expect("still stashed");
    let exceptional = take_exceptional_frame().expect("still stashed");

    let mut after: Vec<u64> = Vec::new();
    deopt.for_each_object_address(&mut |a: u64| after.push(a));
    exceptional.for_each_object_address(&mut |a: u64| after.push(a));
    after.sort_unstable();
    let expected: Vec<u64> = scanned.iter().map(|a| a + DELTA).collect();
    assert_eq!(after, expected, "every slot family must follow the move");

    // Spot-check the two hardest containers by hand, so a walk that
    // "visits" them without writing back cannot pass.
    assert_eq!(deopt.monitors[0].object, FrameValue::Object(0x3000 + DELTA));
    assert_eq!(
        deopt.caller_frames[0].locals[0],
        FrameValue::Object(0x5000 + DELTA)
    );
    clear_stashes();
}

/// A pointer map that does not mention an object means it did not move.
/// Perturbing such a slot would be a corruption of its own.
#[test]
fn remap_leaves_addresses_the_map_does_not_mention_alone() {
    clear_stashes();
    restash_last_deopt(frame_with_one_object_per_container(0));
    let _ = scanned_addresses();
    // Only the local at 0x1000 moved.
    remap_stashed_deopt_objects(|a| if a == 0x1000 { Some(0xABCD) } else { None });
    let frame = take_last_deopt().expect("still stashed");
    assert_eq!(frame.locals[0], FrameValue::Object(0xABCD));
    assert_eq!(frame.stack[1], FrameValue::Object(0x2000));
    assert_eq!(frame.monitors[0].object, FrameValue::Object(0x3000));
    assert_eq!(frame.caller_frames[0].locals[0], FrameValue::Object(0x5000));
    clear_stashes();
}

/// Nothing but an `Object` slot is an address. An `Int`/`Long`/`Float`/
/// `Double` whose bits happen to look like a pointer, an unresolved
/// `StackSlotRef` (a frame OFFSET, not an address), a `VirtualObjectRef` (an
/// intra-frame id) and `Undefined` must all survive a remap untouched — a
/// visitor that widened to "anything 64-bit" would silently rewrite live
/// primitive data.
#[test]
fn non_reference_slots_are_never_offered_or_rewritten() {
    clear_stashes();
    let originals = vec![
        FrameValue::Int(0x1000),
        FrameValue::Long(0x1000),
        FrameValue::Float(0x1000),
        FrameValue::Double(0x1000),
        FrameValue::StackSlotRef(-16),
        FrameValue::RegisterRef(3),
        FrameValue::StackSlot(-8),
        FrameValue::VirtualObjectRef(1),
        FrameValue::Undefined,
        FrameValue::Unsupported,
        FrameValue::MaterializationRequired(EliminatedValue::unknown(
            EliminationCause::Unclassified,
        )),
    ];
    restash_last_deopt(ReconstructedFrame {
        class_id: 0,
        method_key: String::new(),
        bci: 0,
        locals: originals.clone(),
        stack: Vec::new(),
        monitors: Vec::new(),
        semantics: ResumeSemantics::REEXECUTE,
        caller_frames: Vec::new(),
    });
    assert!(scanned_addresses().is_empty());
    remap_stashed_deopt_objects(|_| Some(0xDEAD_BEEF));
    let frame = take_last_deopt().expect("still stashed");
    assert_eq!(frame.locals, originals);
    clear_stashes();
}

/// The visitors must not disturb the stash itself: a scan is a peek, and a
/// remap rewrites in place. A consumer that runs after a collection must
/// still find its frame, with its identity intact.
#[test]
fn scanning_and_remapping_leave_the_stash_in_place() {
    clear_stashes();
    restash_last_deopt(frame_with_one_object_per_container(0));
    assert!(has_last_deopt());
    let _ = scanned_addresses();
    assert!(has_last_deopt(), "a root scan must not consume the stash");
    remap_stashed_deopt_objects(|_| None);
    assert!(has_last_deopt(), "a remap must not consume the stash");
    assert_eq!(
        peek_last_deopt_identity(),
        Some(("craton/probe/Stash.m:()V".to_string(), 4))
    );
    clear_stashes();
}

/// A remap on a thread that never offered its stashes to a root scan is the
/// half-wired state: the reference gets faithfully rewritten to a slot the
/// collector was free to reclaim. Debug builds must refuse it. (An EMPTY
/// stash is not half-wiring — there is nothing to keep alive — so the
/// unscanned no-op below must stay quiet.)
#[test]
fn remapping_an_empty_stash_without_a_scan_is_not_an_error() {
    clear_stashes();
    remap_stashed_deopt_objects(|a| Some(a));
    assert_eq!(stashed_deopt_object_count(), 0);
}

#[test]
#[cfg(debug_assertions)]
fn remapping_a_populated_stash_without_a_scan_trips_the_wiring_check() {
    // Deliberately on a FRESH thread rather than via `#[should_panic]`:
    // both the stashes and the scan counter are thread-local, and
    // `--test-threads=1` runs every test on one thread, where an earlier
    // test's scan would satisfy the check and make this one vacuous.
    let outcome = std::thread::spawn(|| {
        restash_last_deopt(frame_with_one_object_per_container(0));
        // No `for_each_stashed_deopt_object` — this is the half-wiring.
        remap_stashed_deopt_objects(|a| Some(a + 1));
    })
    .join();
    let payload = outcome.expect_err("a remap without a scan must be refused");
    let msg = payload
        .downcast_ref::<String>()
        .map(String::as_str)
        .or_else(|| payload.downcast_ref::<&str>().copied())
        .unwrap_or("");
    assert!(
        msg.contains("half-wired"),
        "the panic must name the half-wiring, got: {msg}"
    );
}
