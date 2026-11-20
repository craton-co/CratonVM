use super::*;

/// Interpreter round i3 wave 77, lane L3: an off timer charges nothing
/// and is not recorded.
#[test]
fn an_off_timer_records_nothing() {
    let stats = DoorPhaseStats::new();
    let mut t = PhaseTimer::new(false);
    assert!(!t.is_on());
    t.mark(PH_ENTER);
    t.mark(PH_CALL);
    assert_eq!(t.charged(PH_ENTER), 0);
    assert_eq!(t.charged(PH_CALL), 0);
    assert!(stats.record(KIND_INVOKE, &t).is_none());
    assert_eq!(stats.kinds[KIND_INVOKE].calls.load(Ordering::Relaxed), 0);
}

/// A running timer charges each mark to its phase, and the window prints
/// one line naming every phase when it fills, then resets.
#[test]
fn a_full_window_prints_every_phase_and_resets() {
    let stats = DoorPhaseStats::new();
    let mut last = None;
    for _ in 0..WINDOW {
        let mut t = PhaseTimer::new(true);
        assert!(t.is_on());
        for phase in 0..N_PHASES {
            t.mark(phase);
        }
        last = stats.record(KIND_CTOR, &t);
    }
    let line = last.expect("the window's last call prints");
    assert!(
        line.starts_with("reflect-door-phases kind=ctor calls=65536 total="),
        "{line}"
    );
    for name in PHASE_NAMES {
        assert!(line.contains(&format!(" {name}=")), "{line}");
    }
    assert!(line.contains(" gap="), "{line}");
    for split in [
        " gap_split_n=",
        " gap_out=",
        " gap_java=",
        " gap_in=",
        " gap_in_split_n=",
        " gap_in_helper=",
        " gap_in_funnel=",
        " call_split_n=",
        " call_door=",
        " call_exec=",
        " call_body=",
        " exec_split_n=",
        " call_exec_prologue=",
        " call_exec_gate=",
        " body_split_n=",
        " call_body_run=",
        " call_body_tail=",
        " door_split_n=",
        " call_door_alloc=",
        " call_door_rest=",
    ] {
        assert!(line.contains(split), "{line}");
    }
    assert!(line.ends_with("(ns/call)"), "{line}");
    assert_eq!(stats.kinds[KIND_CTOR].calls.load(Ordering::Relaxed), 0);
    assert_eq!(stats.kinds[KIND_INVOKE].calls.load(Ordering::Relaxed), 0);
}

/// Interpreter round i3 wave 81, lane L3: the helper's return is kept only
/// once per door exit, and `execute`'s stamps only in order after an
/// arming mark.
#[test]
fn the_gate_stamps_keep_the_first_return_and_an_ordered_reentry() {
    use super::ReflectDoorGatePoint as P;
    let g = GateStamps::EMPTY.stamp(P::HelperExit, 5);
    assert_eq!(g.helper_exit, 0, "not armed: no door exited yet");
    let g = g
        .door_exited()
        .stamp(P::HelperExit, 10)
        .stamp(P::HelperExit, 20);
    assert_eq!(g.helper_exit, 10, "the first return after the exit");
    let g = g.stamp(P::HelperEntry, 30).stamp(P::HelperEntry, 40);
    assert_eq!(g.helper_entry, 40, "the latest entry");
    // `execute` is ignored until a mark arms it, and its body only after
    // its entry.
    let g = g.stamp(P::ExecuteBody, 50).stamp(P::ExecuteEntry, 55);
    assert_eq!((g.exec_state, g.exec_entry, g.exec_body), (EXEC_IDLE, 0, 0));
    let armed = GateStamps {
        exec_state: EXEC_ARMED,
        ..g
    };
    let g = armed
        .stamp(P::ExecuteEntry, 60)
        .stamp(P::ExecuteEntry, 61)
        .stamp(P::ExecuteBody, 70)
        .stamp(P::ExecuteBody, 71);
    assert_eq!(
        (g.exec_state, g.exec_entry, g.exec_body),
        (EXEC_BODY, 60, 70)
    );
}

/// The gap splits only when its stamps sit in order inside it, and the
/// direct arm's stamp cuts `gap_in` only when it falls inside it.
#[test]
fn the_gap_splits_at_ordered_stamps_only() {
    let g = GateStamps {
        helper_exit: 110,
        helper_entry: 400,
        call: 450,
        ..GateStamps::EMPTY
    };
    let s = split_gap(100, 500, &g);
    assert!(s.split && s.in_split);
    assert_eq!((s.out, s.java, s.gate_in), (10, 290, 100));
    assert_eq!((s.in_helper, s.in_funnel), (50, 50));
    assert_eq!(
        s.out + s.java + s.gate_in,
        400,
        "the parts add up to the gap"
    );
    // A stale direct-arm stamp (another route served the call): no cut.
    let stale = GateStamps { call: 50, ..g };
    let s = split_gap(100, 500, &stale);
    assert!(s.split && !s.in_split);
    assert_eq!(s.gate_in, 100);
    // No helper return after the exit (an interpreted caller), no previous
    // exit, or a gap past the cap: no split.
    assert!(
        !split_gap(
            100,
            500,
            &GateStamps {
                helper_exit: 0,
                ..g
            }
        )
        .split
    );
    assert!(!split_gap(0, 500, &g).split);
    assert!(!split_gap(100, 100 + GAP_CAP_TICKS, &g).split);
}

/// `call` splits only when `execute`'s entry and body stamps were both
/// seen, in order, inside the phase.
#[test]
fn the_call_phase_splits_at_executes_stamps() {
    let g = GateStamps {
        exec_state: EXEC_BODY,
        exec_entry: 120,
        exec_body: 300,
        ..GateStamps::EMPTY
    };
    assert_eq!(split_call(100, 400, &g), Some([20, 180, 100]));
    assert_eq!(split_call(130, 400, &g), None, "entry before the phase");
    assert_eq!(
        split_call(
            100,
            400,
            &GateStamps {
                exec_state: EXEC_ENTERED,
                ..g
            }
        ),
        None
    );
}

/// Interpreter round i3 wave 83, lane L3: the gate, return and allocation
/// stamps are kept in order only (the gate the first after the entry, the
/// return the latest while the body runs, the allocation the latest while
/// armed), and an arming mark clears them.
#[test]
fn the_wave_83_stamps_keep_their_ordered_points() {
    use super::ReflectDoorGatePoint as P;
    let g = GateStamps::EMPTY.armed();
    let g = g.stamp(P::ExecuteGate, 5).stamp(P::ExecuteReturned, 6);
    assert_eq!((g.exec_gate, g.exec_returned), (0, 0), "no entry yet");
    let g = g.stamp(P::DoorAllocated, 7).stamp(P::DoorAllocated, 8);
    assert_eq!(g.door_alloc, 8, "the latest while armed");
    let g = g
        .stamp(P::ExecuteEntry, 10)
        .stamp(P::DoorAllocated, 11)
        .stamp(P::ExecuteGate, 20)
        .stamp(P::ExecuteGate, 25)
        .stamp(P::ExecuteReturned, 26)
        .stamp(P::ExecuteBody, 30)
        .stamp(P::ExecuteReturned, 40)
        .stamp(P::ExecuteReturned, 50);
    assert_eq!(g.door_alloc, 8, "not after the entry");
    assert_eq!(g.exec_gate, 20, "the first after the entry");
    assert_eq!(g.exec_returned, 50, "the latest while the body runs");
    let cleared = g.armed();
    assert_eq!(
        (cleared.exec_gate, cleared.exec_returned, cleared.door_alloc),
        (0, 0, 0)
    );
}

/// The finer cuts add up to the part they cut, and a stamp outside its
/// part leaves that part uncut.
#[test]
fn the_call_parts_cut_inside_their_part_only() {
    let g = GateStamps {
        exec_state: EXEC_BODY,
        exec_entry: 120,
        exec_body: 300,
        exec_gate: 200,
        exec_returned: 350,
        door_alloc: 110,
        ..GateStamps::EMPTY
    };
    let parts = split_call_parts(100, 400, &g);
    assert_eq!(parts.exec, Some([80, 100]));
    assert_eq!(parts.body, Some([50, 50]));
    assert_eq!(parts.door, Some([10, 10]));
    let [door, exec, body] = split_call(100, 400, &g).expect("split");
    assert_eq!(parts.exec.map(|[a, b]| a + b), Some(exec));
    assert_eq!(parts.body.map(|[a, b]| a + b), Some(body));
    assert_eq!(parts.door.map(|[a, b]| a + b), Some(door));
    let none = GateStamps {
        exec_gate: 0,
        exec_returned: 450,
        door_alloc: 50,
        ..g
    };
    assert_eq!(split_call_parts(100, 400, &none), CallParts::default());
    // No `call` split: no parts.
    let unsplit = GateStamps {
        exec_state: EXEC_ENTERED,
        ..g
    };
    assert_eq!(split_call_parts(100, 400, &unsplit), CallParts::default());    }