use super::*;

/// Two locals, three bci — the shape every other test perturbs.
fn consistent() -> (Vec<i32>, Vec<u64>, Vec<Option<u8>>, Vec<Option<u8>>, usize) {
    (
        vec![-1, 0x10, -1],
        vec![0, 0, 0],
        vec![Some(3), None],
        vec![None, None],
        2,
    )
}

#[test]
fn consistent_metadata_passes() {
    let (p, d, l, x, n) = consistent();
    assert_eq!(check(&p, &d, &l, &x, n), Ok(()));
}

/// The unsound one. A dead mask shorter than the entry table makes
/// `can_osr_enter_with` read `unwrap_or(0)` — "no dead locals" — for every
/// bci in the tail, admitting entries that must be refused.
///
/// The exact edit that trips it: size `osr_dead_mask` from the OUTPUT code
/// length while `osr_pc_to_native` is rebuilt to the ORIGINAL one, which is
/// what a bytecode transform makes easy to do by accident.
#[test]
fn a_short_dead_mask_is_refused() {
    let (p, _, l, x, n) = consistent();
    let short = vec![0u64; p.len() - 1];
    assert_eq!(
        check(&p, &short, &l, &x, n),
        Err(OsrContractViolation::BciVectorLengthMismatch {
            pc_to_native: 3,
            dead_mask: 2,
        })
    );
}

/// And the other direction, which is merely wrong rather than unsound —
/// checked because "equal length" is the invariant, not "long enough".
#[test]
fn a_long_dead_mask_is_refused_too() {
    let (p, _, l, x, n) = consistent();
    let long = vec![0u64; p.len() + 1];
    assert!(matches!(
        check(&p, &long, &l, &x, n),
        Err(OsrContractViolation::BciVectorLengthMismatch { .. })
    ));
}

#[test]
fn assignment_vectors_must_cover_every_seeded_local() {
    let (p, d, l, x, n) = consistent();
    assert_eq!(
        check(&p, &d, &l[..1], &x, n),
        Err(OsrContractViolation::LocalAssignmentsTooShort {
            len: 1,
            num_locals: 2,
        })
    );
    assert_eq!(
        check(&p, &d, &l, &x[..1], n),
        Err(OsrContractViolation::XmmAssignmentsTooShort {
            len: 1,
            num_locals: 2,
        })
    );
    // Longer than `num_locals` is fine: the trampoline stops at
    // `num_locals`, and a category-2 high half can push the vector past it.
    let mut longer = l.clone();
    longer.push(None);
    assert_eq!(check(&p, &d, &longer, &x, n), Ok(()));
}

/// A register home past local 63 is outside what the 64-bit dead mask can
/// protect, so it is refused — in either register file — while a home at
/// 63 and a homeless local past 63 are both fine.
#[test]
fn a_register_home_past_the_dead_mask_width_is_refused() {
    let n = 70;
    let p = vec![-1i32; 4];
    let d = vec![0u64; 4];
    let mut gpr = vec![None; n];
    let mut xmm = vec![None; n];
    gpr[63] = Some(12);
    xmm[10] = Some(9);
    assert_eq!(check(&p, &d, &gpr, &xmm, n), Ok(()));

    let mut bad_gpr = gpr.clone();
    bad_gpr[64] = Some(13);
    assert_eq!(
        check(&p, &d, &bad_gpr, &xmm, n),
        Err(OsrContractViolation::RegisterHomeBeyondDeadMask {
            local: 64,
            xmm: false,
        })
    );

    let mut bad_xmm = xmm.clone();
    bad_xmm[69] = Some(8);
    assert_eq!(
        check(&p, &d, &gpr, &bad_xmm, n),
        Err(OsrContractViolation::RegisterHomeBeyondDeadMask {
            local: 69,
            xmm: true,
        })
    );

    // A home in the tail the trampoline never reads is not a violation.
    let mut tail = gpr.clone();
    tail.push(Some(14));
    assert_eq!(check(&p, &d, &tail, &xmm, n), Ok(()));
}

/// The check is a pure function of the vectors, so the empty case — a
/// method with no locals and no entries — is consistent, not a violation.
/// Stated because "everything empty" is what a stubbed-out producer yields
/// and a checker that rejected it would refuse every artifact.
#[test]
fn empty_metadata_is_consistent() {
    assert_eq!(check(&[], &[], &[], &[], 0), Ok(()));
}

/// The refusal sentinel is preserved verbatim — the checker never edits.
///
/// It is a *predicate*, not a repair: the publication site already enforces
/// `-1` at every bci the transform refuses, and a checker that silently
/// fixed a disagreement instead of reporting it would hide exactly the bug
/// it exists to surface.
#[test]
fn the_checker_does_not_repair() {
    let (p, d, l, x, n) = consistent();
    let before = p.clone();
    assert_eq!(check(&p, &d, &l, &x, n), Ok(()));
    assert_eq!(p, before, "check() must not mutate its inputs");
}
