//! gcd d9/b: the young trigger's floor below the cap on a wedged old
//! generation (`young_trigger_floor_judged`), and the two census lines.
use super::*;

const CAP: usize = 1600; // ceiling 1440, capacity / 16 = 100

fn never() -> Option<usize> {
    panic!("the old generation must not be consulted here")
}

/// The rule as it stood before gcd d9/b, spelled out.
fn historical(live: usize, threshold: usize, wedged: bool) -> usize {
    let ceiling = CAP * NON_MOVING_YOUNG_GC_THRESHOLD_PERCENT / 100;
    let rearm = live.saturating_add(CAP / 16);
    if rearm > ceiling && wedged {
        YOUNG_TRIGGER_DEFERRED
    } else if live >= threshold {
        rearm.min(ceiling)
    } else {
        0
    }
}

/// Off, the judged floor is the historical rule on every input, and the
/// old generation is consulted exactly where it was (only past the cap).
#[test]
fn off_the_judged_floor_is_the_historical_rule() {
    for live in [0, 100, 799, 800, 900, 1339, 1340, 1341, 1400, 1500, 2000] {
        for threshold in [400, 800, 1440, 1600, usize::MAX] {
            for wedged in [false, true] {
                let want = historical(live, threshold, wedged);
                let old = young_trigger_floor_after_collection(live, threshold, CAP, || wedged);
                let (judged, verdict) =
                    young_trigger_floor_judged(live, threshold, CAP, false, || {
                        wedged.then_some(usize::MAX)
                    });
                assert_eq!(
                    old, want,
                    "live={live} threshold={threshold} wedged={wedged}"
                );
                assert_eq!(
                    judged, want,
                    "live={live} threshold={threshold} wedged={wedged}"
                );
                assert_ne!(verdict, WedgedTriggerVerdict::Raised);
            }
        }
    }
    // Below the cap the off arm never asks.
    assert_eq!(young_trigger_floor_judged(100, 800, CAP, false, never).0, 0);
    assert_eq!(
        young_trigger_floor_judged(900, 800, CAP, false, never).0,
        1000
    );
}

/// On, below the cap, wedged: the floor rises to the non-moving trigger,
/// whether `live` sits under the moving trigger (floor 0) or over it
/// (floor `live + capacity/16`).
#[test]
fn a_wedged_old_gen_raises_the_floor_to_the_cap() {
    let wedged = || Some(usize::MAX);
    assert_eq!(
        young_trigger_floor_judged(100, 800, CAP, true, wedged),
        (1440, WedgedTriggerVerdict::Raised)
    );
    assert_eq!(
        young_trigger_floor_judged(900, 800, CAP, true, wedged),
        (1440, WedgedTriggerVerdict::Raised)
    );
    // Room in the old generation: the ordinary floor.
    assert_eq!(
        young_trigger_floor_judged(100, 800, CAP, true, || None),
        (0, WedgedTriggerVerdict::Ordinary)
    );
    assert_eq!(
        young_trigger_floor_judged(900, 800, CAP, true, || None),
        (1000, WedgedTriggerVerdict::Ordinary)
    );
    // Past the cap the deferral is unchanged, on or off.
    assert_eq!(
        young_trigger_floor_judged(1400, 800, CAP, true, wedged),
        (YOUNG_TRIGGER_DEFERRED, WedgedTriggerVerdict::Deferred)
    );
    // A trigger already at the cap (the non-moving 90 % one) has nothing
    // to raise, and the old generation is not asked.
    assert_eq!(
        young_trigger_floor_judged(100, 1440, CAP, true, never),
        (0, WedgedTriggerVerdict::Ordinary)
    );
}

/// A heap whose old generation borrowed young budget
/// (`CRATONVM_GC_OLD_BORROW_YOUNG`) raises no further than its cap, and
/// not at all when the cap is under the ordinary level.
#[test]
fn the_raise_obeys_the_borrowed_young_cap() {
    assert_eq!(
        young_trigger_floor_judged(100, 800, CAP, true, || Some(1200)),
        (1200, WedgedTriggerVerdict::Raised)
    );
    assert_eq!(
        young_trigger_floor_judged(100, 800, CAP, true, || Some(700)),
        (0, WedgedTriggerVerdict::Ordinary)
    );
}

/// The measured crawl in its own numbers: a 32 MiB semi-space, the moving
/// 50 % trigger, 16 494 720 live young bytes after a `moving+major` cycle
/// on a full old generation. Off: floor 0, the next cycle one TLAB carve
/// away. On: the 90 % trigger.
#[test]
fn the_measured_crawl_waits_for_the_non_moving_trigger() {
    let semi = 33_554_432;
    let live = 16_494_720;
    let ceiling = semi * NON_MOVING_YOUNG_GC_THRESHOLD_PERCENT / 100;
    assert_eq!(
        young_trigger_floor_after_collection(live, semi / 2, semi, || true),
        0
    );
    assert_eq!(
        young_trigger_floor_judged(live, semi / 2, semi, true, || Some(usize::MAX)),
        (ceiling, WedgedTriggerVerdict::Raised)
    );
}

/// The heap-level half: a completed collection, a young live set under
/// the moving trigger and a wedged old generation raise the floor and
/// count it; the switch off (or room in the old generation) leaves the
/// historical floor and counts nothing. Asserted on the floor, because
/// `needs_gc()`'s threshold follows process-global JIT quiescence.
#[test]
fn needs_gc_raises_the_floor_on_a_wedged_old_gen() {
    if super::young_pause_goal_ms_from_env() == 0 {
        // No goal: a non-moving cycle would use the 90 % trigger itself.
        return;
    }
    for (switch_on, wedge_old) in [(true, true), (false, true), (true, false)] {
        let mut heap = GenerationalHeap::with_sizes(4 * 1024, 8 * 1024);
        heap.young_wedged_trigger = switch_on;
        let capacity = heap.young_semi_capacity();
        let ceiling = capacity * NON_MOVING_YOUNG_GC_THRESHOLD_PERCENT / 100;
        heap.young_gc_threshold
            .store(capacity / 2, Ordering::Relaxed);
        let keep = heap.alloc_object(ClassId::new(1), 1);
        assert!(
            heap.young_from_used() < capacity / 4,
            "fixture: live well under the trigger"
        );
        if wedge_old {
            // Raw reservations: nothing walks the old generation here.
            let mut old = heap.old_gen.lock();
            while old.alloc(64, 8).is_some() {}
            while old.alloc(8, 8).is_some() {}
        }
        heap.stats.minor_gc_count.fetch_add(1, Ordering::Relaxed);
        let _ = heap.needs_gc();
        let floor = heap.young_trigger_floor.load(Ordering::Relaxed);
        let raised = heap.young_wedged_census[0].load(Ordering::Relaxed);
        if switch_on && wedge_old {
            assert_eq!(
                floor, ceiling,
                "wedged: the floor waits for the 90 % trigger"
            );
            assert_eq!(raised, 1, "and the sample is counted");
            let line = heap.young_trigger_wedged_census_line();
            assert!(
                line.contains("ytw_raise_enabled=true ytw_raised=1 ytw_deferred=0"),
                "{line}"
            );
        } else {
            assert_eq!(
                floor, 0,
                "switch={switch_on} wedged={wedge_old}: the ordinary floor"
            );
            assert_eq!(raised, 0);
        }
        let _ = keep;
    }
}

/// gcd d9/c's blocked drain (cross-lane request 1, folded in by d9/b)
/// stands the trigger down like a wedged old generation, with room in the
/// old generation, and is counted as such; a drain that is not blocked
/// leaves nothing to stand down for. BUILD DEPENDENCY: d9/c's
/// `last_cycle_drain_blocked` / `young_drain_blocked` (`02847aedd`).
#[test]
fn a_blocked_drain_stands_the_trigger_down_like_a_wedged_old_gen() {
    let heap = GenerationalHeap::with_sizes(4 * 1024, 8 * 1024);
    let capacity = heap.young_semi_capacity();
    assert_eq!(
        heap.young_drain_stood_down_cap(capacity),
        None,
        "room, and a drain"
    );
    heap.last_cycle_drain_blocked.store(true, Ordering::Relaxed);
    if !heap.young_drain_blocked() {
        // `CRATONVM_GC_YOUNG_DRAIN_BLOCKED=0` in this process: never blocked.
        return;
    }
    assert_eq!(heap.young_drain_stood_down_cap(capacity), Some(usize::MAX));
    assert_eq!(heap.young_wedged_census[2].load(Ordering::Relaxed), 1);
    let line = heap.young_trigger_wedged_census_line();
    assert!(line.ends_with("ytw_drain_blocked=1"), "{line}");
}

/// gcj j1yg-thrash: a wedged old generation starves young the way a
/// gated-off promotion walk does. `GcfF3oOomeShapeProbe` (`-Xmx256m`
/// Serial) filled young to its last byte with arrays old gen had no room
/// for; every forced cycle promoted the previous iteration's 64 bytes into
/// an old hole and the next object fitted, so the futile-young verdict,
/// which asked only about the gated walk, never fired: 10-21 K cycles.
#[test]
fn a_wedged_old_gen_starves_young_like_a_blocked_drain() {
    let heap = GenerationalHeap::with_sizes(4 * 1024, 8 * 1024);
    assert!(
        !heap.young_starved_behind_blocked_drain(usize::MAX),
        "room in old gen, and a drain"
    );
    {
        // Raw reservations: nothing walks the old generation here.
        let mut old = heap.old_gen.lock();
        while old.alloc(64, 8).is_some() {}
        while old.alloc(8, 8).is_some() {}
    }
    assert!(
        heap.young_starved_behind_blocked_drain(usize::MAX),
        "wedged: no room reaches usize::MAX"
    );
    assert!(
        !heap.young_starved_behind_blocked_drain(0),
        "every room is at least 0 bytes"
    );
}

/// The ladder census: slots add up per heap, an unknown slot is ignored,
/// and the line divides by the OOME count (0.0 before the first).
#[test]
fn the_oome_ladder_census_line_reports_work_per_oome() {
    let heap = GenerationalHeap::with_sizes(4 * 1024, 8 * 1024);
    let line = heap.oome_ladder_census_line();
    assert!(
        line.starts_with("[GC] oome_ladder: ladder_oomes=0 "),
        "{line}"
    );
    assert!(line.contains("ladder_forced_per_oome=0.0"), "{line}");
    heap.note_oome_ladder(oome_ladder::OOMES, 2);
    heap.note_oome_ladder(oome_ladder::FORCED, 25);
    heap.note_oome_ladder(oome_ladder::FORCED_US, 7_500_000);
    heap.note_oome_ladder(oome_ladder::MAJORS, 9);
    heap.note_oome_ladder(oome_ladder::THRESHOLD_WEDGED, 3);
    heap.note_oome_ladder(oome_ladder::LEN, 1);
    let c = heap.oome_ladder_census();
    assert_eq!(c[oome_ladder::OOMES], 2);
    assert_eq!(c.iter().sum::<u64>(), 2 + 25 + 7_500_000 + 9 + 3);
    let line = heap.oome_ladder_census_line();
    assert!(
        line.contains("ladder_forced=25 ladder_forced_ms=7500 "),
        "{line}"
    );
    assert!(line.contains("ladder_forced_per_oome=12.5"), "{line}");
    assert!(line.contains("ladder_majors_per_oome=4.5"), "{line}");
    assert!(
        line.contains("ladder_threshold_wedged_per_oome=1.5"),
        "{line}"
    );
    let other = GenerationalHeap::with_sizes(4 * 1024, 8 * 1024);
    assert_eq!(
        other.oome_ladder_census()[oome_ladder::OOMES],
        0,
        "per heap"
    );
}
