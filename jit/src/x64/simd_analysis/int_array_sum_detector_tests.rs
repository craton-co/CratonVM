use super::*;

/// `int sum(int[] a, int n) { int s = 0; for (int i = 0; i < n; i++) s += a[i]; return s; }`
///
/// Locals: 0 = a, 1 = n, 2 = s, 3 = i. This is `javac`'s spelling of
/// `s += a[i]`: the accumulator is loaded *before* the element.
fn int_plus_equals() -> Vec<u8> {
    vec![
        0x03, //  0: iconst_0
        0x3d, //  1: istore_2        s = 0
        0x03, //  2: iconst_0
        0x3e, //  3: istore_3        i = 0
        0x1d, //  4: iload_3         <- header
        0x1b, //  5: iload_1
        0xa2, 0x00, 0x0f, //  6: if_icmpge +15 -> 21
        0x1c, //  9: iload_2         s
        0x2a, // 10: aload_0         a
        0x1d, // 11: iload_3         i
        0x2e, // 12: iaload
        0x60, // 13: iadd
        0x3d, // 14: istore_2
        0x84, 0x03, 0x01, // 15: iinc 3, 1
        0xa7, 0xff, 0xf2, // 18: goto -14 -> 4
        0x1c, // 21: iload_2
        0xac, // 22: ireturn
    ]
}

/// `long s = 0; for (int i = 0; i < n; i++) s += a[i];`
///
/// Locals: 0 = a, 1 = n, 2-3 = s, 4 = i.
fn long_plus_equals() -> Vec<u8> {
    vec![
        0x09, //  0: lconst_0
        0x41, //  1: lstore_2
        0x03, //  2: iconst_0
        0x36, 0x04, //  3: istore 4
        0x15, 0x04, //  5: iload 4       <- header
        0x1b, //  7: iload_1
        0xa2, 0x00, 0x11, //  8: if_icmpge +17 -> 25
        0x20, // 11: lload_2        s
        0x2a, // 12: aload_0        a
        0x15, 0x04, // 13: iload 4
        0x2e, // 15: iaload
        0x85, // 16: i2l
        0x61, // 17: ladd
        0x41, // 18: lstore_2
        0x84, 0x04, 0x01, // 19: iinc 4, 1
        0xa7, 0xff, 0xef, // 22: goto -17 -> 5
        0x20, // 25: lload_2
        0xad, // 26: lreturn
    ]
}

/// The historical `s = a[i] + s` long form — matched with or without the
/// switch.
fn long_element_first() -> Vec<u8> {
    let mut code = long_plus_equals();
    // 11..=17 becomes: aload_0 ; iload 4 ; iaload ; i2l ; lload_2 ; ladd
    code[11..18].copy_from_slice(&[0x2a, 0x15, 0x04, 0x2e, 0x85, 0x20, 0x61]);
    code
}

#[test]
fn the_historical_long_form_is_matched_with_the_switch_off() {
    let code = long_element_first();
    let info = detect_int_array_sum_forms(&code, 5, 22, 4, false).expect("historical form");
    assert_eq!(
        (
            info.acc_local,
            info.array_local,
            info.bound_local,
            info.acc_is_long
        ),
        (2, 0, 1, true)
    );
    assert!(detect_int_array_sum_forms(&code, 5, 22, 4, true).is_some());
}

#[test]
fn the_javac_plus_equals_forms_need_the_switch() {
    let code = long_plus_equals();
    assert!(
        detect_int_array_sum_forms(&code, 5, 22, 4, false).is_none(),
        "default-off: the historical detector's verdict is unchanged"
    );
    let info = detect_int_array_sum_forms(&code, 5, 22, 4, true).expect("long s += a[i]");
    assert_eq!(
        (
            info.acc_local,
            info.array_local,
            info.bound_local,
            info.acc_is_long
        ),
        (2, 0, 1, true)
    );

    let code = int_plus_equals();
    assert!(detect_int_array_sum_forms(&code, 4, 18, 3, false).is_none());
    let info = detect_int_array_sum_forms(&code, 4, 18, 3, true).expect("int s += a[i]");
    assert_eq!(
        (
            info.acc_local,
            info.array_local,
            info.bound_local,
            info.acc_is_long
        ),
        (2, 0, 1, false)
    );
}

#[test]
fn the_int_element_first_form_is_matched_under_the_switch() {
    let mut code = int_plus_equals();
    // 9..=13 becomes: aload_0 ; iload_3 ; iaload ; iload_2 ; iadd
    code[9..14].copy_from_slice(&[0x2a, 0x1d, 0x2e, 0x1c, 0x60]);
    assert!(detect_int_array_sum_forms(&code, 4, 18, 3, false).is_none());
    let info = detect_int_array_sum_forms(&code, 4, 18, 3, true).expect("s = a[i] + s");
    assert_eq!((info.acc_local, info.acc_is_long), (2, false));
}

/// `istore i` / `istore n` are well-typed bytecode for an `int`
/// accumulator. The pre-header reads `iv`/`bound` once and writes back
/// only `acc` and `iv`, so either alias would be a miscompile.
#[test]
fn an_accumulator_that_is_the_iv_or_the_bound_is_refused() {
    let mut code = int_plus_equals();
    code[9] = 0x1d; // iload_3  (i)
    code[14] = 0x3e; // istore_3 (i)
    assert!(detect_int_array_sum_forms(&code, 4, 18, 3, true).is_none());

    let mut code = int_plus_equals();
    code[9] = 0x1b; // iload_1  (n)
    code[14] = 0x3c; // istore_1 (n)
    assert!(detect_int_array_sum_forms(&code, 4, 18, 3, true).is_none());
}

#[test]
fn a_store_to_a_different_local_is_refused() {
    let mut code = int_plus_equals();
    code[14] = 0x3b; // istore_0 — not the accumulator that was read
    assert!(detect_int_array_sum_forms(&code, 4, 18, 3, true).is_none());
}

/// `int sum(int[] a) { int s = 0; for (int i = 0; i < a.length; i++) s += a[i]; return s; }`
///
/// Locals: 0 = a, 1 = s, 2 = i. The idiomatic header: the bound is
/// re-read from the array every iteration and never sits in a local.
fn int_sum_over_length() -> Vec<u8> {
    vec![
        0x03, //  0: iconst_0
        0x3c, //  1: istore_1        s = 0
        0x03, //  2: iconst_0
        0x3d, //  3: istore_2        i = 0
        0x1c, //  4: iload_2         <- header
        0x2a, //  5: aload_0
        0xbe, //  6: arraylength
        0xa2, 0x00, 0x0f, //  7: if_icmpge +15 -> 22
        0x1b, // 10: iload_1         s
        0x2a, // 11: aload_0         a
        0x1c, // 12: iload_2         i
        0x2e, // 13: iaload
        0x60, // 14: iadd
        0x3c, // 15: istore_1
        0x84, 0x02, 0x01, // 16: iinc 2, 1
        0xa7, 0xff, 0xf1, // 19: goto -15 -> 4
        0x1b, // 22: iload_1
        0xac, // 23: ireturn
    ]
}

#[test]
fn the_array_length_header_is_matched_under_the_switch() {
    let code = int_sum_over_length();
    assert!(
        detect_int_array_sum_forms(&code, 4, 19, 2, false).is_none(),
        "default-off: the `a.length` header stays refused without the opt-in"
    );
    let info = detect_int_array_sum_forms(&code, 4, 19, 2, true).expect("a.length header");
    assert_eq!(info.bound, SimdLoopBound::ArrayLength(0));
    // The walk loads `bound_local` into R11: for this header that must be
    // the ARRAY, which the pre-header turns into its length.
    assert_eq!(info.bound_local, 0);
    assert_eq!(
        (info.array_local, info.acc_local, info.acc_is_long),
        (0, 1, false)
    );
}

/// The `iload bound` header keeps reporting a `Local` bound, so the
/// driver's coverage proof for it is unchanged.
#[test]
fn an_iload_header_reports_a_local_bound() {
    let code = int_plus_equals();
    let info = detect_int_array_sum_forms(&code, 4, 18, 3, true).expect("int s += a[i]");
    assert_eq!(info.bound, SimdLoopBound::Local(1));
    assert_eq!(info.bound_local, 1);
}

/// `aload a` NOT followed by `arraylength` is not a bound.
#[test]
fn an_aload_without_arraylength_is_not_a_bound() {
    let mut code = int_sum_over_length();
    code[6] = 0x00; // nop instead of arraylength
    assert!(detect_int_array_sum_forms(&code, 4, 19, 2, true).is_none());
}

/// `static void add(int[] out, int[] a, int[] b) { for (int i = 0; i < a.length; i++) out[i] = a[i] + b[i]; }`
///
/// Locals: 0 = out, 1 = a, 2 = b, 3 = i.
fn element_wise_over_length() -> Vec<u8> {
    vec![
        0x03, //  0: iconst_0
        0x3e, //  1: istore_3        i = 0
        0x1d, //  2: iload_3         <- header
        0x2b, //  3: aload_1
        0xbe, //  4: arraylength
        0xa2, 0x00, 0x13, //  5: if_icmpge +19 -> 24
        0x2a, //  8: aload_0         out
        0x1d, //  9: iload_3
        0x2b, // 10: aload_1         a
        0x1d, // 11: iload_3
        0x2e, // 12: iaload
        0x2c, // 13: aload_2         b
        0x1d, // 14: iload_3
        0x2e, // 15: iaload
        0x60, // 16: iadd
        0x4f, // 17: iastore
        0x84, 0x03, 0x01, // 18: iinc 3, 1
        0xa7, 0xff, 0xed, // 21: goto -19 -> 2
        0xb1, // 24: return
    ]
}

#[test]
fn the_element_wise_array_length_header_is_matched_under_the_switch() {
    let code = element_wise_over_length();
    assert!(detect_int_array_element_wise_forms(&code, 2, 21, 3, false).is_none());
    let e =
        detect_int_array_element_wise_forms(&code, 2, 21, 3, true).expect("a.length header");
    assert_eq!(e.bound, SimdLoopBound::ArrayLength(1));
    assert_eq!(e.bound_local, 1);
    assert_eq!((e.out_local, e.a_local, e.b_local), (0, 1, 2));
    assert_eq!(e.op, ElementWiseOp::Add);
}

/// The coverage predicate the driver and the IR-tier veto share.
///
/// An `a.length` loop is covered by its own pre-header guards; an
/// `iload n` loop over a parameter `n` is covered only by a matching
/// speculative guard (or the static proof, which a parameter bound cannot
/// have).
#[test]
fn the_shared_coverage_predicate_matches_the_driver_rule() {
    let code = int_sum_over_length();
    let s = detect_int_array_sum_forms(&code, 4, 19, 2, true).expect("a.length header");
    assert!(simd_sum_covered(&s, &code, code.len(), &[]));

    let code = int_plus_equals();
    let s = detect_int_array_sum_forms(&code, 4, 18, 3, true).expect("int s += a[i]");
    assert!(
        !simd_sum_covered(&s, &code, code.len(), &[]),
        "a parameter bound with no guard is not covered: the driver emits it scalar, \
         so the veto must not fire for it either"
    );
    let guard = SpeculativeBCEGuard {
        loop_header: 4,
        array_local: 0,
        bound_local: 1,
        iv_local: 3,
        covered_pcs: vec![12],
        inclusive: false,
        step_local: None,
        array_field_hoist: None,
        bound_field_hoist: None,
        limit: BceGuardLimit::Bound,
    };
    assert!(simd_sum_covered(
        &s,
        &code,
        code.len(),
        std::slice::from_ref(&guard)
    ));
    let wrong_array = SpeculativeBCEGuard {
        array_local: 5,
        ..guard.clone()
    };
    assert!(!simd_sum_covered(&s, &code, code.len(), &[wrong_array]));

    let code = element_wise_over_length();
    let e =
        detect_int_array_element_wise_forms(&code, 2, 21, 3, true).expect("a.length header");
    assert!(simd_element_wise_covered(&e, &code, code.len(), &[]));
}

/// `int twice(int[] a) { int n = a.length; int s = 0;
///  for (int i = 0; i < n; i++) s += a[i];
///  for (int i = 0; i < n; i++) s += a[i]; return s; }`
///
/// Locals: 0 = a, 1 = n, 2 = s, 3 = i (both loops: javac reuses the slot).
fn sibling_sums_sharing_a_slot() -> Vec<u8> {
    vec![
        0x2a, //  0: aload_0
        0xbe, //  1: arraylength
        0x3c, //  2: istore_1        n = a.length
        0x03, //  3: iconst_0
        0x3d, //  4: istore_2        s = 0
        0x03, //  5: iconst_0
        0x3e, //  6: istore_3        i = 0
        0x1d, //  7: iload_3         <- header 1
        0x1b, //  8: iload_1
        0xa2, 0x00, 0x0f, //  9: if_icmpge -> 24
        0x1c, 0x2a, 0x1d, 0x2e, 0x60, 0x3d, // 12: s += a[i]
        0x84, 0x03, 0x01, // 18: iinc 3, 1
        0xa7, 0xff, 0xf2, // 21: goto -> 7
        0x03, // 24: iconst_0
        0x3e, // 25: istore_3        i = 0 (same slot)
        0x1d, // 26: iload_3         <- header 2
        0x1b, // 27: iload_1
        0xa2, 0x00, 0x0f, // 28: if_icmpge -> 43
        0x1c, 0x2a, 0x1d, 0x2e, 0x60, 0x3d, // 31: s += a[i]
        0x84, 0x03, 0x01, // 37: iinc 3, 1
        0xa7, 0xff, 0xf2, // 40: goto -> 26
        0x1c, // 43: iload_2
        0xac, // 44: ireturn
    ]
}

/// Round 11 wave 2 (`r11-iropt-bce-iv-start-refuses-a-reused-slot`): the
/// static coverage proof used to demand ONE store to the IV slot in the
/// whole method, so neither sibling loop qualified. Each loop's own entry
/// value is what the batch needs.
#[test]
fn sibling_loops_sharing_the_iv_slot_are_each_statically_covered() {
    let code = sibling_sums_sharing_a_slot();
    assert_eq!(code.len(), 45);
    let first = detect_int_array_sum_forms(&code, 7, 21, 3, true).expect("first loop");
    let second = detect_int_array_sum_forms(&code, 26, 40, 3, true).expect("second loop");
    assert_eq!(first.bound, SimdLoopBound::Local(1));
    assert!(simd_sum_covered(&first, &code, code.len(), &[]));
    assert!(simd_sum_covered(&second, &code, code.len(), &[]));

    // MUST REFUSE twin: the second loop starts at -1.
    let mut negative = code.clone();
    negative[24] = 0x02; // iconst_m1
    let second = detect_int_array_sum_forms(&negative, 26, 40, 3, true).expect("second loop");
    assert!(!simd_sum_covered(&second, &negative, negative.len(), &[]));
    let first = detect_int_array_sum_forms(&negative, 7, 21, 3, true).expect("first loop");
    assert!(simd_sum_covered(&first, &negative, negative.len(), &[]));
}
