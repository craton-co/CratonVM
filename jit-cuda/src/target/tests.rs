use super::*;

/// The regression this module exists for. Every one of these
/// rendered `.version 7.5` before 2026-09-02, and every one of them
/// failed to load.
#[test]
fn modern_targets_require_more_than_isa_7_5() {
    assert_eq!(min_isa_for_target(8, 9), (7, 8), "Ada");
    assert_eq!(min_isa_for_target(9, 0), (7, 8), "Hopper");
    assert_eq!(min_isa_for_target(10, 0), (8, 7), "Blackwell datacenter");
    assert_eq!(min_isa_for_target(12, 0), (8, 7), "Blackwell RTX");
}

/// The architectures the hardcoded literal happened to be right
/// for — these must not regress while fixing the ones it was wrong
/// for.
#[test]
fn pre_ada_targets_still_fit_under_isa_7_5() {
    for (maj, min) in [(7, 0), (7, 5), (8, 0), (8, 6), (8, 7)] {
        assert!(
            min_isa_for_target(maj, min) <= (7, 5),
            "sm_{maj}{min} used to load under .version 7.5 and must still"
        );
    }
}

/// The header this crate has always emitted must not move for any
/// target that already loaded. Every PTX-text assertion in the test
/// suite and every measurement in `docs/gpu/` depends on this.
#[test]
fn the_baseline_holds_every_pre_ada_target_at_7_5() {
    for (maj, min) in [(7, 0), (7, 2), (7, 5), (8, 0), (8, 6), (8, 7)] {
        assert_eq!(
            isa_for_target(maj, min),
            (7, 5),
            "sm_{maj}{min} must still render `.version 7.5`"
        );
    }
}

/// …and must move for exactly the targets that could not load.
#[test]
fn the_target_floor_wins_where_it_has_to() {
    assert_eq!(isa_for_target(8, 9), (7, 8), "Ada");
    assert_eq!(isa_for_target(9, 0), (7, 8), "Hopper");
    assert_eq!(isa_for_target(10, 0), (8, 7), "Blackwell datacenter");
    assert_eq!(isa_for_target(12, 0), (8, 7), "Blackwell RTX");
}

/// The property that makes the whole module correct: whatever
/// `.version` we render admits whatever `.target` we render beside
/// it. Stated over the full cross product rather than the handful
/// of named cases above.
#[test]
fn every_rendered_header_is_self_consistent() {
    for maj in 7..=13u32 {
        for min in 0..=9u32 {
            if (maj, min) > NEWEST_KNOWN_TARGET {
                // Past the table there is nothing to be consistent
                // WITH: no ISA we know admits an architecture we do
                // not. `min_isa_for_target` saturates and the driver
                // reports an unknown target, which is a clear error
                // rather than a version mismatch. Pinned separately
                // by `unknown_future_capability_saturates_*`.
                continue;
            }
            let isa = isa_for_target(maj, min);
            assert!(
                max_target_for_isa(isa) >= (maj, min),
                "sm_{maj}{min} would render `.version {}.{}`, which tops out at sm_{}{}",
                isa.0,
                isa.1,
                max_target_for_isa(isa).0,
                max_target_for_isa(isa).1
            );
        }
    }
}

/// The specific hole the exhaustive test above found on its first
/// run: `sm_88` is not a real architecture, but the original
/// round-DOWN lookup resolved it to the `sm_87` row and rendered
/// `.version 7.5` — a version whose target set stops at `sm_87`.
/// Rounding up is what makes an unknown capability inside a known
/// range safe.
#[test]
fn a_capability_between_two_known_rows_rounds_up() {
    assert_eq!(
        min_isa_for_target(8, 8),
        (7, 8),
        "sm_88 sits between sm_87 and sm_89"
    );
    assert_eq!(
        min_isa_for_target(10, 2),
        (8, 8),
        "sm_102 sits between sm_101 and sm_103"
    );
    assert_eq!(
        min_isa_for_target(11, 0),
        (8, 7),
        "sm_110 sits between sm_103 and sm_120"
    );
}

#[test]
fn floor_and_ceiling_compose() {
    // Every target we can name must be admitted by the ISA its own
    // floor reports — i.e. the two tables agree.
    for (maj, min) in [
        (7, 0),
        (7, 2),
        (7, 5),
        (8, 0),
        (8, 6),
        (8, 7),
        (8, 9),
        (9, 0),
        (10, 0),
        (12, 0),
    ] {
        let isa = min_isa_for_target(maj, min);
        assert!(
            max_target_for_isa(isa) >= (maj, min),
            "sm_{maj}{min} floors at {isa:?} but that ISA tops out at {:?}",
            max_target_for_isa(isa)
        );
    }
}

#[test]
fn unknown_future_capability_saturates_rather_than_extrapolating() {
    // A capability past every row must not resolve to something
    // invented; it takes the newest row we actually know.
    assert_eq!(min_isa_for_target(99, 9), (8, 8));
}

#[test]
fn driver_version_maps_to_the_isa_that_toolkit_published() {
    assert_eq!(max_isa_for_cuda_version(11050), (7, 5), "CUDA 11.5");
    assert_eq!(max_isa_for_cuda_version(11080), (7, 8), "CUDA 11.8");
    assert_eq!(max_isa_for_cuda_version(12000), (8, 0), "CUDA 12.0");
    assert_eq!(max_isa_for_cuda_version(12080), (8, 7), "CUDA 12.8");
    assert_eq!(max_isa_for_cuda_version(13030), (9, 3), "CUDA 13.3");
    // A driver newer than the last row saturates to it.
    assert_eq!(max_isa_for_cuda_version(99000), (9, 3));
    // A driver older than every row falls to the floor.
    assert_eq!(max_isa_for_cuda_version(8000), (6, 0));
}

#[test]
fn an_old_driver_lowers_the_target_instead_of_emitting_an_unloadable_header() {
    // Hopper card, CUDA 11.5 driver: sm_90 needs ISA 7.8, the
    // driver tops out at 7.5, so target the newest thing 7.5 knows.
    let driver = max_isa_for_cuda_version(11050);
    assert_eq!(clamp_target_to_isa((9, 0), driver), (8, 7));
    // And the clamped target's own floor now fits.
    assert!(min_isa_for_target(8, 7) <= driver);
}

#[test]
fn a_current_driver_leaves_the_target_alone() {
    let driver = max_isa_for_cuda_version(12080);
    for t in [(7, 5), (8, 0), (8, 9), (9, 0), (12, 0)] {
        assert_eq!(clamp_target_to_isa(t, driver), t, "sm_{}{}", t.0, t.1);
    }
}

#[test]
fn clamping_never_raises_the_target() {
    // A brand-new driver must not promote an old card's target:
    // the module has to run on the device, not on the driver.
    let driver = max_isa_for_cuda_version(13030);
    assert_eq!(clamp_target_to_isa((7, 5), driver), (7, 5));
}
