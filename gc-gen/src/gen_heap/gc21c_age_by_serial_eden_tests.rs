//! gc wave 9, lane gcC (`gcf-f5c-...`): `CRATONVM_GEN_AGE_BY_SERIAL_EDEN`
//! -- survivors age once per HotSpot Serial eden of young allocation.
use super::{
    cycle_age_step, serial_eden_age_epoch, serial_eden_bytes, should_tenure,
    sweep_survivor_mark_by, GenerationalHeap, NON_AGING_PROMOTION_AGE,
};
use cratonvm_gc_core::heap::{ObjectHeader, GC_FLAG_MARKED};

const M: usize = 1 << 20;

/// The epoch ages when the carried credit plus this interval's
/// allocation reaches an eden, and carries the remainder; an unarmed
/// eden (0) always ages.
#[test]
fn gc21c_the_epoch_ages_once_per_eden_and_carries_the_rest() {
    assert_eq!(serial_eden_age_epoch(0, 40, 100), (false, 40));
    assert_eq!(serial_eden_age_epoch(40, 40, 100), (false, 80));
    assert_eq!(serial_eden_age_epoch(80, 40, 100), (true, 20));
    assert_eq!(serial_eden_age_epoch(0, 100, 100), (true, 0));
    assert_eq!(serial_eden_age_epoch(7, 0, 0), (true, 0), "unarmed");
    assert!(serial_eden_age_epoch(usize::MAX, 5, 100).0, "saturates");
}

/// The RATE is HotSpot's: at the vh4-fl geometry (`-Xmx256m`: eden
/// 71 630 848 bytes since gci i30/gv, the default arm's cycles every 32 MiB), 640 MB of
/// allocation ages a survivor 9 times -- under the tenuring ceiling of
/// 15 -- where one age per cycle would be 20 (`OldGenFinalizeProbe`).
#[test]
fn gc21c_the_finalize_probes_churn_ages_a_survivor_under_fifteen() {
    let eden = serial_eden_bytes(256 * M / 3);
    assert_eq!(eden, 71_630_848);
    let (mut credit, mut ages, mut cycles) = (0usize, 0u32, 0u32);
    let mut allocated = 0usize;
    while allocated < 640 * 1_000_000 {
        let (a, c) = serial_eden_age_epoch(credit, 32 * M, eden);
        credit = c;
        ages += u32::from(a);
        cycles += 1;
        allocated += 32 * M;
    }
    assert_eq!(cycles, 20);
    assert_eq!(ages, 9);
}

/// A non-aging cycle tenures by force only and ages nothing; every other
/// promotion age ages by one, as before.
#[test]
fn gc21c_a_held_cycle_promotes_by_force_only_and_ages_nothing() {
    assert_eq!(cycle_age_step(NON_AGING_PROMOTION_AGE), 0);
    for age in [0u8, 1, 3, 15, 16] {
        assert_eq!(cycle_age_step(age), 1);
    }
    for age in 0..=cratonvm_types::MAX_GC_AGE {
        assert!(
            !should_tenure(age, false, NON_AGING_PROMOTION_AGE),
            "age {age}"
        );
        assert!(
            should_tenure(age, true, NON_AGING_PROMOTION_AGE),
            "forced {age}"
        );
    }
    let marked =
        ObjectHeader::mark_with_gc_age(ObjectHeader::mark_with_gc_flags(0, GC_FLAG_MARKED), 3);
    let (held, held_age) = sweep_survivor_mark_by(marked, 0);
    assert_eq!(held_age, 3);
    assert_eq!(ObjectHeader::gc_age_of(held), 3);
    assert_eq!(
        held & ObjectHeader::mark_with_gc_flags(0, GC_FLAG_MARKED),
        0,
        "mark cleared"
    );
    let (aged, aged_age) = sweep_survivor_mark_by(marked, 1);
    assert_eq!((ObjectHeader::gc_age_of(aged), aged_age), (4, 4));
}

// Per heap, default ON since w9c: `=0` leaves the heap unarmed (every
// cycle ages, the census line unchanged); unset arms the epoch from
// Serial's eden WITHOUT the Serial-shaped trigger, sizes the tenuring
// target from that eden, and an empty interval holds the cycle's aging
// until it closes; a heap pinned to the fixed promotion age stays out.
