use super::*;

const GB: u64 = 1024 * 1024 * 1024;
const MB: u64 = 1024 * 1024;
/// VM A's heap: 256 MiB at 256 GiB, far above the headroom.
const A: (u64, u64) = (256 * GB, 256 * GB + 256 * MB);

const NARROW_GEN: NarrowOopRequest = NarrowOopRequest {
    wants_narrow: true,
    backend_audited: true,
};
const WIDE_GEN: NarrowOopRequest = NarrowOopRequest {
    wants_narrow: false,
    backend_audited: true,
};
const NARROW_UNAUDITED: NarrowOopRequest = NarrowOopRequest {
    wants_narrow: true,
    backend_audited: false,
};
const WIDE_UNAUDITED: NarrowOopRequest = NarrowOopRequest {
    wants_narrow: false,
    backend_audited: false,
};

/// The state after VM A fixed its window: returns it and the window.
fn after_first_vm() -> (WidthState, u64, u8) {
    let AdmissionPlan::Enable { base, shift } = plan(WidthState::default(), &[A], NARROW_GEN)
    else {
        panic!("a fitting first heap that asked must enable narrow oops");
    };
    (
        WidthState {
            narrow: Some((base, shift)),
            wide_admitted: false,
        },
        base,
        shift,
    )
}

fn refused(p: &AdmissionPlan) -> bool {
    matches!(
        p,
        AdmissionPlan::Answer {
            admission: NarrowOopAdmission::Refuse(_),
            commits_wide: false,
        }
    )
}

#[test]
fn the_first_vm_fixes_the_window_below_its_own_heap() {
    let (_, base, shift) = after_first_vm();
    assert_eq!(shift, 3);
    assert_eq!(base, (A.0 - NARROW_OOP_HEADROOM) & !0xfff);
    assert!(check_fits(base, shift, &[A]).is_ok());
}

#[test]
fn a_second_vm_whose_heap_fits_joins_the_window_without_refixing_it() {
    let (state, base, shift) = after_first_vm();
    // VM B: another 512 MiB, 1 GiB above A -- inside the 32 GiB window.
    let b = (A.1 + GB, A.1 + GB + 512 * MB);
    assert_eq!(
        plan(state, &[b], NARROW_GEN),
        AdmissionPlan::Answer {
            admission: NarrowOopAdmission::Narrow {
                base,
                shift,
                fixed_by_this_vm: false,
            },
            commits_wide: false,
        }
    );
}

#[test]
fn a_second_vm_whose_heap_is_outside_the_window_is_refused_not_handed_it() {
    let (state, base, shift) = after_first_vm();
    let limit = base + ((u32::MAX as u64) << shift);
    // Above the limit (the w7-f scenario: an unrelated mapping 100 GiB up).
    let above = (A.0 + 100 * GB, A.0 + 100 * GB + 64 * MB);
    assert!(above.1 > limit);
    assert!(refused(&plan(state, &[above], NARROW_GEN)));
    // Straddling the limit: its base fits, its end does not.
    let straddle = (limit - 16 * MB, limit + 16 * MB);
    assert!(refused(&plan(state, &[straddle], NARROW_GEN)));
    // Below the base, and exactly ON it (the base encodes to null).
    let below = (base - GB, base - GB + 64 * MB);
    assert!(refused(&plan(state, &[below], NARROW_GEN)));
    let on_base = (base, base + 64 * MB);
    assert!(refused(&plan(state, &[on_base], NARROW_GEN)));
    // One good span does not excuse a bad one.
    let good = (A.1 + GB, A.1 + GB + 64 * MB);
    assert!(refused(&plan(state, &[good, above], NARROW_GEN)));
    // A heap that published nothing cannot be checked, so it cannot join.
    assert!(refused(&plan(state, &[], NARROW_GEN)));
}

#[test]
fn a_second_vm_that_did_not_ask_or_cannot_run_narrow_is_refused() {
    let (state, _, _) = after_first_vm();
    let inside = (A.1 + GB, A.1 + GB + 64 * MB);
    // Did not ask (the default flags; the w7-f failure scenario's VM B).
    assert!(refused(&plan(state, &[inside], WIDE_GEN)));
    // G1 / ZGC, asked or not: not audited for 4-byte slots.
    assert!(refused(&plan(state, &[inside], NARROW_UNAUDITED)));
    assert!(refused(&plan(state, &[inside], WIDE_UNAUDITED)));
}

#[test]
fn once_a_wide_vm_exists_a_later_vm_cannot_switch_the_process_to_narrow() {
    // VM A wide (default flags).
    let first = plan(WidthState::default(), &[A], WIDE_GEN);
    assert_eq!(
        first,
        AdmissionPlan::Answer {
            admission: NarrowOopAdmission::Wide,
            commits_wide: true,
        }
    );
    let state = WidthState {
        narrow: None,
        wide_admitted: true,
    };
    // VM B asks, with a heap that would fit on its own: declined, runs wide.
    let b = (A.1 + GB, A.1 + GB + 64 * MB);
    match plan(state, &[b], NARROW_GEN) {
        AdmissionPlan::Answer {
            admission: NarrowOopAdmission::WideDeclined(why),
            commits_wide: true,
        } => assert!(why.contains("earlier VM"), "{why}"),
        other => panic!("a later VM must not flip the width under a wide VM: {other:?}"),
    }
}

#[test]
fn a_first_vm_that_cannot_run_narrow_falls_back_to_wide_and_commits_the_process() {
    // Unaudited backend.
    assert!(matches!(
        plan(WidthState::default(), &[A], NARROW_UNAUDITED),
        AdmissionPlan::Answer {
            admission: NarrowOopAdmission::WideDeclined(_),
            commits_wide: true,
        }
    ));
    // Unusable geometry: below the headroom, no spans, misaligned.
    for spans in [
        vec![(4 * GB, 4 * GB + 64 * MB)],
        vec![],
        vec![(A.0 + 4, A.1)],
    ] {
        assert!(
            matches!(
                plan(WidthState::default(), &spans, NARROW_GEN),
                AdmissionPlan::Answer {
                    admission: NarrowOopAdmission::WideDeclined(_),
                    commits_wide: true,
                }
            ),
            "{spans:?}"
        );
    }
}

/// `check_fits` and the runtime tripwire `assert_region_encodable` must
/// agree about the window's two edges, or a heap admitted here panics on
/// its first region republish (or one refused here would have been fine).
#[test]
fn the_fit_check_uses_the_runtime_tripwire_bounds() {
    let (_, base, shift) = after_first_vm();
    let limit = base + ((u32::MAX as u64) << shift);
    assert!(check_fits(base, shift, &[(base + 8, limit)]).is_ok());
    assert!(check_fits(base, shift, &[(base, limit)]).is_err());
    assert!(check_fits(base, shift, &[(base + 8, limit + 8)]).is_err());
}
