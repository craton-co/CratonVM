use super::inline_const_divisor;

fn site() -> crate::InlineSite {
    crate::InlineSite::default()
}

/// The constant pushes an integer division may take its divisor from, and
/// only when the push ends exactly at the division.
#[test]
fn int_constant_pushes_are_decoded() {
    let s = site();
    // iload_0; bipush 31; irem; ireturn
    let code = [0x1a, 0x10, 31, 0x70, 0xac];
    assert_eq!(
        inline_const_divisor(&code, code.len(), 1, 3, false, &s),
        Some(31)
    );
    // iload_0; sipush -300; idiv
    let [hi, lo] = (-300i16).to_be_bytes();
    let code = [0x1a, 0x11, hi, lo, 0x6c, 0xac];
    assert_eq!(
        inline_const_divisor(&code, code.len(), 1, 4, false, &s),
        Some(-300)
    );
    // iconst_m1 decodes to -1, which the arm lowers as a negation (round
    // 11 wave 3); iconst_0 is no divisor at all (JIT r22 INLINE22-6: the
    // shared decoder answers only a non-zero one).
    let code = [0x1a, 0x02, 0x6c, 0xac];
    assert_eq!(
        inline_const_divisor(&code, code.len(), 1, 2, false, &s),
        Some(-1)
    );
    let code = [0x1a, 0x03, 0x6c, 0xac];
    assert_eq!(
        inline_const_divisor(&code, code.len(), 1, 2, false, &s),
        None
    );
    // bipush -1 is the same hazard spelled differently.
    let code = [0x1a, 0x10, 0xff, 0x6c, 0xac];
    assert_eq!(
        inline_const_divisor(&code, code.len(), 1, 3, false, &s),
        Some(-1)
    );
}

/// A push that does not end at the division, a non-constant push, and a
/// width mismatch are all `None` — the arm then refuses the splice.
#[test]
fn anything_else_is_not_a_constant_divisor() {
    let s = site();
    // iload_0; iload_1; idiv — the divisor is a local.
    let code = [0x1a, 0x1b, 0x6c, 0xac];
    assert_eq!(
        inline_const_divisor(&code, code.len(), 1, 2, false, &s),
        None
    );
    // bipush 7 whose end is NOT the division pc.
    let code = [0x10, 7, 0x1a, 0x6c, 0xac];
    assert_eq!(
        inline_const_divisor(&code, code.len(), 0, 3, false, &s),
        None
    );
    // An int push under a LONG division is not the long divisor.
    let code = [0x1e, 0x10, 7, 0x6d, 0xad];
    assert_eq!(
        inline_const_divisor(&code, code.len(), 1, 3, true, &s),
        None
    );
    // lconst_1 under ldiv is.
    let code = [0x1e, 0x0a, 0x6d, 0xad];
    assert_eq!(
        inline_const_divisor(&code, code.len(), 1, 2, true, &s),
        Some(1)
    );
    // An `ldc` the resolver did not record is refused, not read as 0.
    let code = [0x1a, 0x12, 0x05, 0x6c, 0xac];
    assert_eq!(
        inline_const_divisor(&code, code.len(), 1, 3, false, &s),
        None
    );
}

/// `ldc`/`ldc2_w` use the resolver's table, and an entry tagged
/// floating-point is refused.
#[test]
fn ldc_divisors_come_from_the_resolver_tables() {
    let mut s = site();
    s.ldc_info = vec![(1, 1000)];
    s.ldc2w_info = vec![(1, 1_000_000_007)];
    let code = [0x1a, 0x12, 0x05, 0x70, 0xac];
    assert_eq!(
        inline_const_divisor(&code, code.len(), 1, 3, false, &s),
        Some(1000)
    );
    let code = [0x1e, 0x14, 0x00, 0x05, 0x71, 0xad];
    assert_eq!(
        inline_const_divisor(&code, code.len(), 1, 4, true, &s),
        Some(1_000_000_007)
    );
    s.ldc_fp_pcs = vec![1];
    let code = [0x1a, 0x12, 0x05, 0x70, 0xac];
    assert_eq!(
        inline_const_divisor(&code, code.len(), 1, 3, false, &s),
        None
    );
}

/// The two splice-walk division arms must not come back on the guarded
/// helper: its reason-3 stub is stamped with a CALLEE pc.
#[test]
fn the_splice_walk_does_not_call_the_guarded_divide() {
    let src = include_str!("../inlining.rs");
    let needle = ["emit_safe", "_idiv("].concat();
    assert_eq!(
        src.matches(needle.as_str()).count(),
        0,
        "a guarded divide inside a splice publishes a deopt stub under the \
         callee's pc; see the division arm in `try_emit_inline_body`"
    );
}
