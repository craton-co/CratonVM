use super::*;

/// The kill switch keeps the legacy 4096 steps whatever the band.
#[test]
fn the_kill_switch_keeps_the_legacy_guard() {
    assert_eq!(compiled_walk_step_guard(false, 0x1000, 0x1000), 4096);
    assert_eq!(
        compiled_walk_step_guard(false, 0x1000, 0x10_0000_1000),
        4096
    );
}

/// On, the guard admits every strictly increasing 8-aligned step a band can
/// hold (a chain of minimal 8-byte steps from the band's bottom to its top),
/// so it never ends a well-formed chain early, and it is not 4096 for a
/// deep band.
#[test]
fn the_whole_entry_guard_admits_every_step_of_the_band() {
    let bands: [(usize, usize); 4] = [
        (0x1000, 0x1000),
        (0x1000, 0x1010),
        (0x1000, 0x2000),
        (0x7000_0000, 0x7080_0000),
    ];
    for (lo, hi) in bands {
        let guard = compiled_walk_step_guard(true, lo, hi);
        let most_steps = (hi - lo) / 8;
        assert!(
            guard > most_steps,
            "{lo:x}..{hi:x}: guard {guard} steps {most_steps}"
        );
    }
    // An 8 MiB band holds far more than 4096 frames.
    assert!(compiled_walk_step_guard(true, 0x7000_0000, 0x7080_0000) > 4096 * 16);
    // A band that is not one (entry below the scanner) still walks nothing
    // wild: the loop's own bound checks refuse the first step.
    assert_eq!(compiled_walk_step_guard(true, 0x2000, 0x1000), 1);
}
