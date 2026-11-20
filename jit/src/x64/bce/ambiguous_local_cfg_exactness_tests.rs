use super::*;

/// `AmbiguousLocalKinds::cfg_is_exact` is the whole soundness condition
/// behind reading `Ambiguous` as "the verifier types this local `top`".
/// Both of the constructs that break the CFG equality must clear it, and
/// the ordinary method must set it — a predicate that answered `true`
/// everywhere would silently license dropping live locals, and one that
/// answered `false` everywhere would look like a working gate while
/// buying nothing.
///
/// The bytecode below reuses slot 0 as an `int` (`istore_0`) on the taken
/// side of a branch and as a `double` (`dstore_0`) on the other, so slot 0
/// really is `Ambiguous` for the method and the pass has something to
/// track. Verified by BREAKING it: dropping the `exception_ranges`
/// conjunct makes the second case read `true` and this test fails.
#[test]
fn cfg_exactness_tracks_handlers_and_jsr() {
    // Slot 0 is an `int` on one arm of a branch and a `double` on the
    // other, so the whole-method classifier must call it `Ambiguous` and
    // the refinement has something to track:
    //   0: iconst_0  1: ifeq ->9  4: iconst_1  5: istore_0
    //   6: goto ->11  9: dconst_0 10: dstore_0 11: return
    let code: &[u8] = &[
        0x03, 0x99, 0x00, 0x08, 0x04, 0x3b, 0xa7, 0x00, 0x05, 0x0e, 0x47, 0xb1,
    ];
    // The same method with the trailing `return` replaced by `jsr; return`.
    let jsr_code: &[u8] = &[
        0x03, 0x99, 0x00, 0x08, 0x04, 0x3b, 0xa7, 0x00, 0x05, 0x0e, 0x47, 0xa8, 0x00, 0x03,
        0xb1,
    ];
    let kinds = classify_local_kinds(code, code.len(), 4);
    assert!(
        matches!(kinds[0], LocalKind::Ambiguous),
        "the fixture must actually produce an ambiguous slot, or every \
           assertion below passes vacuously"
    );
    let clean = refine_ambiguous_local_kinds(code, code.len(), &kinds, &[]);
    let with_handler = refine_ambiguous_local_kinds(code, code.len(), &kinds, &[(0, 4, 11)]);
    let jsr_kinds = classify_local_kinds(jsr_code, jsr_code.len(), 4);
    let with_jsr = refine_ambiguous_local_kinds(jsr_code, jsr_code.len(), &jsr_kinds, &[]);

    assert!(
        clean.cfg_is_exact && !clean.has_jsr,
        "an exception-free, jsr-free method's CFG IS the verifier's, and the \
           gate must say so or the relaxation it guards is dead code"
    );
    assert!(
        !with_handler.cfg_is_exact,
        "a handler entry merges in- AND out-states of the protected range, \
           which is WIDER than the verifier's in-state merge -- `Ambiguous` there does not imply \
           `top` and must not license dropping the slot"
    );
    assert!(
        !with_handler.has_jsr,
        "`has_jsr` is the half a caller may check ALONE before trusting a \
           SETTLED kind; folding the handler half into it would refuse every \
           method that merely has a try block"
    );
    assert!(
        with_jsr.has_jsr && !with_jsr.cfg_is_exact,
        "`ret` is given no successors at all, which makes the graph NARROWER \
           than the verifier's, and that is the half a settled kind cannot \
           survive"
    );
}

/// A handler is typed by the pcs it protects, not by TOP: slot 1 is an
/// `int` then a `float` for the method, `float` throughout the protected
/// loop, so it is `float` at the handler and back at the loop head.
#[test]
fn refine_types_a_handler_from_its_protected_range() {
    //  0 iconst_0  1 istore_1  2 fconst_0  3 fstore_1
    //  4 fload_1   5 pop       6 goto 4
    //  9 astore_2 (handler)   10 goto 4   13 return
    let code: &[u8] = &[
        0x03, 0x3c, 0x0b, 0x44, 0x23, 0x57, 0xa7, 0xff, 0xfe, 0x4d, 0xa7, 0xff, 0xfa, 0xb1,
    ];
    let kinds = classify_local_kinds(code, code.len(), 3);
    assert!(matches!(kinds[1], LocalKind::Ambiguous));
    let refined = refine_ambiguous_local_kinds(code, code.len(), &kinds, &[(4, 6, 9)]);
    assert_eq!(refined.kind_at(9, 1), Some(LocalKind::Float));
    assert_eq!(refined.kind_at(4, 1), Some(LocalKind::Float));
    assert!(
        !refined.cfg_is_exact,
        "an exception range still clears exactness"
    );
}
