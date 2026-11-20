use super::*;

#[test]
fn zt7tg_the_timer_is_off_without_an_interval() {
    assert_eq!(plan(None, Duration::from_secs(3600), false, false), ZTick::Off);
    assert_eq!(parse_collection_interval("0"), None);
    assert_eq!(parse_collection_interval("-1"), None);
    assert_eq!(parse_collection_interval("x"), None);
    assert_eq!(parse_collection_interval("1"), Some(Duration::from_secs(1)));
    assert_eq!(
        parse_collection_interval("0.5"),
        Some(Duration::from_millis(500))
    );
}

#[test]
fn zt7tg_the_timer_fires_once_the_interval_has_elapsed() {
    let iv = Some(Duration::from_secs(1));
    assert_eq!(
        plan(iv, Duration::from_millis(1000), false, false),
        ZTick::Collect(ZTickCause::Timer)
    );
    match plan(iv, Duration::from_millis(400), false, false) {
        ZTick::Wait(w) => {
            assert!(w.abs_diff(Duration::from_millis(600)) < Duration::from_micros(1))
        }
        other => panic!("expected a wait, got {other:?}"),
    }
    // The wait is clamped to the tick bounds.
    assert_eq!(
        plan(Some(Duration::from_secs(60)), Duration::ZERO, false, false),
        ZTick::Wait(TICK_MAX_WAIT)
    );
    // Timer before proactive, as `make_major_gc_decision` asks.
    assert_eq!(
        plan(iv, Duration::from_secs(2), true, true),
        ZTick::Collect(ZTickCause::Timer)
    );
    // The proactive answer counts only when its tick is armed.
    assert_eq!(
        plan(None, Duration::ZERO, true, false),
        ZTick::Off,
        "control: proactive_due without the tick"
    );
    assert_eq!(
        plan(None, Duration::ZERO, true, true),
        ZTick::Collect(ZTickCause::Proactive)
    );
    assert_eq!(
        plan(None, Duration::ZERO, false, true),
        ZTick::Wait(PROACTIVE_TICK)
    );
}

/// zt t11/tg4 (`t11-x8-report.md` T1): the in-flight count the exit
/// path waits on, and its floor at zero.
#[test]
fn zt11tg4_a_tick_pause_is_in_flight_between_enter_and_leave() {
    let t = ZTimer::new();
    assert!(!t.pause_in_flight());
    t.enter_pause();
    assert!(t.pause_in_flight());
    t.stop();
    assert!(t.is_stopped());
    assert!(t.pause_in_flight(), "the stop does not clear it");
    t.leave_pause();
    assert!(!t.pause_in_flight());
    t.leave_pause();
    assert!(!t.pause_in_flight(), "an extra leave does not wrap");
    t.enter_pause();
    assert!(t.pause_in_flight(), "floor at zero: one enter counts again");
}

/// zt t12/x9: the guard form leaves on drop, also when the pause
/// unwinds.
#[test]
fn zt12x9_the_pause_guard_leaves_on_drop_and_on_unwind() {
    let t = ZTimer::new();
    {
        let _g = t.pause_guard();
        assert!(t.pause_in_flight());
    }
    assert!(!t.pause_in_flight(), "left at the drop");
    let unwound = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _g = t.pause_guard();
        assert!(t.pause_in_flight());
        panic!("a pause that unwinds");
    }));
    assert!(unwound.is_err());
    assert!(!t.pause_in_flight(), "left on unwind");
}

#[test]
fn zt7tg_a_minor_does_not_reset_the_major_clock() {
    let t = ZTimer::new();
    t.set_interval(Some(Duration::from_secs(1)));
    let start = t.epoch;
    // Before any cycle: time since construction (HotSpot: since VM start).
    assert!(!t.timer_due(start + Duration::from_millis(999)));
    assert!(t.timer_due(start + Duration::from_millis(1000)));
    // A whole-heap cycle ends at 1.0 s: the clock restarts.
    t.note_cycle_end(start + Duration::from_secs(1), 0);
    assert!(!t.timer_due(start + Duration::from_millis(1500)));
    assert!(t.timer_due(start + Duration::from_millis(2000)));
    // A minor (young count moved 0 -> 1) at 1.8 s does not restart it.
    t.note_cycle_end(start + Duration::from_millis(1800), 1);
    assert!(t.timer_due(start + Duration::from_millis(2000)));
    // A major after it (count unchanged) does.
    t.note_cycle_end(start + Duration::from_millis(1900), 1);
    assert!(!t.timer_due(start + Duration::from_millis(2000)));
    // Off: never due.
    t.set_interval(None);
    assert!(!t.timer_due(start + Duration::from_secs(3600)));
}

/// zt t27/gn5 (`rule_minor_timer`): the minor timer's plan comes after
/// the major plan (a major request or a mark-end close wins), collects
/// once `ZCollectionIntervalMinor` passed since the last collection,
/// shortens a longer wait, and leaves the major plan alone when it is
/// off. Every cycle end restarts its clock -- a major's and a stall's
/// too, unlike the major timer's.
#[test]
fn zt27gn5_the_minor_timer_plans_after_the_major_rules() {
    let iv = Some(Duration::from_millis(200));
    let wait = ZTick::Wait(Duration::from_secs(1));
    assert_eq!(plan_minor(wait, None, Duration::from_secs(9)), wait, "off");
    assert_eq!(
        plan_minor(ZTick::Off, None, Duration::from_secs(9)),
        ZTick::Off
    );
    assert_eq!(
        plan_minor(wait, iv, Duration::from_millis(200)),
        ZTick::Collect(ZTickCause::MinorTimer)
    );
    assert_eq!(
        plan_minor(ZTick::Off, iv, Duration::from_millis(250)),
        ZTick::Collect(ZTickCause::MinorTimer)
    );
    let major = ZTick::Collect(ZTickCause::Timer);
    assert_eq!(
        plan_minor(major, iv, Duration::from_secs(9)),
        major,
        "major first"
    );
    assert_eq!(
        plan_minor(ZTick::CloseMark, iv, Duration::from_secs(9)),
        ZTick::CloseMark
    );
    match plan_minor(wait, iv, Duration::from_millis(50)) {
        ZTick::Wait(w) => {
            assert!(w.abs_diff(Duration::from_millis(150)) < Duration::from_micros(1))
        }
        other => panic!("expected a wait, got {other:?}"),
    }
    assert_eq!(
        plan_minor(ZTick::Off, Some(Duration::from_secs(60)), Duration::ZERO),
        ZTick::Wait(TICK_MAX_WAIT),
        "clamped"
    );
    // The clock: since construction, then since the last cycle end of
    // any kind (a minor, a major, a stall's).
    let t = ZTimer::new();
    t.set_minor_interval(iv);
    let start = t.epoch;
    assert!(!t.minor_timer_due(start + Duration::from_millis(199)));
    assert!(t.minor_timer_due(start + Duration::from_millis(200)));
    t.note_cycle_end(start + Duration::from_millis(300), 0); // a major
    assert!(!t.minor_timer_due(start + Duration::from_millis(450)));
    assert!(t.minor_timer_due(start + Duration::from_millis(500)));
    t.note_cycle_end(start + Duration::from_millis(500), 1); // a minor
    assert!(!t.minor_timer_due(start + Duration::from_millis(650)));
    t.note_cycle_end_as(start + Duration::from_millis(600), 1, true); // a stall's
    assert!(!t.minor_timer_due(start + Duration::from_millis(750)));
    assert!(t.minor_timer_due(start + Duration::from_millis(800)));
    // The minor clock never restarted the major one (last major end at
    // 0.3 s; the minor and the stall's left it alone).
    t.set_interval(Some(Duration::from_millis(400)));
    assert!(!t.timer_due(start + Duration::from_millis(650)));
    assert!(t.timer_due(start + Duration::from_millis(700)));
    t.note_request(ZTickCause::MinorTimer);
    assert_eq!(t.minor_timer_requests(), 1);
    assert_eq!(t.counts(), (0, 0, 0), "the major counts are separate");
    t.set_minor_interval(None);
    assert!(!t.minor_timer_due(start + Duration::from_secs(3600)));
}

/// zt t12/tg5 (TG3-O4): an allocation stall's cycle does not restart
/// the major clock (HotSpot's stall runs a minor), so a program that
/// stalls every 100 ms still gets its `Timer` cycle each interval.
/// Control: the same cycles as non-stall ones keep the timer from ever
/// coming due (the t11 behaviour). A stall cycle still takes the young
/// count in, so the next whole-heap cycle restarts the clock.
#[test]
fn zt12tg5_a_stall_cycle_does_not_restart_the_timer() {
    let iv = Some(Duration::from_secs(1));
    for stall in [true, false] {
        let t = ZTimer::new();
        t.set_interval(iv);
        let start = t.epoch;
        for step in 1..=25u64 {
            t.note_cycle_end_as(start + Duration::from_millis(step * 100), 0, stall);
        }
        let at = start + Duration::from_millis(2600);
        assert_eq!(
            t.timer_due(at),
            stall,
            "stall={stall}: due at 2.6 s only when the stalls left the clock alone"
        );
    }
    // A stall that was a young-only cycle (count 0 -> 1) is taken in:
    // the major after it (count unchanged) restarts the clock.
    let t = ZTimer::new();
    t.set_interval(iv);
    let start = t.epoch;
    t.note_cycle_end_as(start + Duration::from_millis(500), 1, true);
    assert!(t.timer_due(start + Duration::from_millis(1000)));
    t.note_cycle_end_as(start + Duration::from_millis(900), 1, false);
    assert!(!t.timer_due(start + Duration::from_millis(1000)));
    assert!(t.timer_due(start + Duration::from_millis(1900)));
}
