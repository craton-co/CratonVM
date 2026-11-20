use super::{
    detect_batch_loop, detect_bulk_set_byte_stride_loop, detect_bulk_zero_byte_fill_loop,
    detect_byte_sieve_loop, detect_loops, find_bypassable_loop_headers,
    find_rotation_preheader_entries, unrotate_loop, unrotate_loop_in_place,
    unrotate_nested_loops,
};
use crate::x64::{
    bytecode_analysis, detect_int_array_element_wise_forms, detect_int_array_sum_forms,
    detect_matrix_dot_loop, find_induction_variable, SimdLoopBound,
};
use rustc_hash::{FxHashMap, FxHashSet};

/// `static void z(boolean[] a, int limit) { for (int i = 0; i <= limit; i++) a[i] = false; }`
fn ecj_zero_fill() -> Vec<u8> {
    vec![
        0x03, 0x3d, // 0: i = 0
        0xa7, 0x00, 0x0a, // 2: goto 12
        0x2a, 0x1c, 0x03, 0x54, // 5: a[i] = 0        ; header
        0x84, 0x02, 0x01, // 9: i++
        0x1c, 0x1b, // 12: iload i; iload limit     ; COND
        0xa4, 0xff, 0xf7, // 14: if_icmple 5
        0xb1, // 17: return
    ]
}

/// `static int s(int[] a) { int s = 0; int n = a.length; for (int i = 0; i < n; i++) s += a[i]; return s; }`
fn ecj_int_sum() -> Vec<u8> {
    vec![
        0x03, 0x3c, // 0: s = 0
        0x2a, 0xbe, 0x3d, // 2: n = a.length
        0x03, 0x3e, // 5: i = 0
        0xa7, 0x00, 0x0c, // 7: goto 19
        0x1b, 0x2a, 0x1d, 0x2e, 0x60, 0x3c, // 10: s += a[i]  ; header
        0x84, 0x03, 0x01, // 16: i++
        0x1d, 0x1c, // 19: iload i; iload n          ; COND
        0xa1, 0xff, 0xf5, // 21: if_icmplt 10
        0x1b, 0xac, // 24: return s
    ]
}

/// `static void add(int[] c, int[] a, int[] b) { for (int i = 0; i < c.length; i++) c[i] = a[i] + b[i]; }`
fn ecj_add_over_length() -> Vec<u8> {
    vec![
        0x03, 0x3e, // 0: i = 0
        0xa7, 0x00, 0x10, // 2: goto 18
        0x2a, 0x1d, 0x2b, 0x1d, 0x2e, 0x2c, 0x1d, 0x2e, 0x60,
        0x4f, // 5: c[i] = a[i] + b[i]
        0x84, 0x03, 0x01, // 15: i++
        0x1d, 0x2a, 0xbe, // 18: iload i; aload c; arraylength   ; COND
        0xa1, 0xff, 0xf0, // 21: if_icmplt 5
        0xb1, // 24: return
    ]
}

/// `CratonBench.matmul(int[][] a, int[][] b, int n)` as ecj compiles it.
fn ecj_matmul() -> Vec<u8> {
    vec![
        0x1c, 0x1c, 0xc5, 0x00, 0x34, 0x02, // 0: new int[n][n]
        0x4e, // 6: astore_3 (c)
        0x03, 0x36, 0x04, // 7: i = 0
        0xa7, 0x00, 0x44, // 10: goto 78
        0x03, 0x36, 0x05, // 13: j = 0
        0xa7, 0x00, 0x35, // 16: goto 69
        0x03, 0x36, 0x06, // 19: sum = 0
        0x03, 0x36, 0x07, // 22: k = 0
        0xa7, 0x00, 0x1a, // 25: goto 51
        0x15, 0x06, 0x2a, 0x15, 0x04, 0x32, 0x15, 0x07, 0x2e, // 28: sum, a[i][k]
        0x2b, 0x15, 0x07, 0x32, 0x15, 0x05, 0x2e, // 37: b[k][j]
        0x68, 0x60, 0x36, 0x06, // 44: sum += a[i][k] * b[k][j]
        0x84, 0x07, 0x01, // 48: k++
        0x15, 0x07, 0x1c, 0xa1, 0xff, 0xe6, // 51: if (k < n) goto 28
        0x2d, 0x15, 0x04, 0x32, 0x15, 0x05, 0x15, 0x06, 0x4f, // 57: c[i][j] = sum
        0x84, 0x05, 0x01, // 66: j++
        0x15, 0x05, 0x1c, 0xa1, 0xff, 0xcb, // 69: if (j < n) goto 19
        0x84, 0x04, 0x01, // 75: i++
        0x15, 0x04, 0x1c, 0xa1, 0xff, 0xbc, // 78: if (i < n) goto 13
        0x2d, 0xb0, // 84: return c
    ]
}

/// `CratonBench.sieve(boolean[] a, int limit)` as ecj compiles it.
fn ecj_sieve() -> Vec<u8> {
    vec![
        0x03, 0x3d, 0xa7, 0x00, 0x0a, // 0: i = 0; goto 12
        0x2a, 0x1c, 0x03, 0x54, 0x84, 0x02, 0x01, // 5: a[i] = false; i++
        0x1c, 0x1b, 0xa4, 0xff, 0xf7, // 12: if (i <= limit) goto 5
        0x03, 0x3d, 0x05, 0x3e, // 17: count = 0; i = 2
        0xa7, 0x00, 0x28, // 21: goto 61
        0x2a, 0x1d, 0x33, 0x9a, 0x00, 0x1f, // 24: if (a[i]) goto 58
        0x84, 0x02, 0x01, // 30: count++
        0x1d, 0x1d, 0x60, 0x36, 0x04, // 33: j = i + i
        0xa7, 0x00, 0x0e, // 38: goto 52
        0x2a, 0x15, 0x04, 0x04, 0x54, // 41: a[j] = true
        0x15, 0x04, 0x1d, 0x60, 0x36, 0x04, // 46: j += i
        0x15, 0x04, 0x1b, 0xa4, 0xff, 0xf2, // 52: if (j <= limit) goto 41
        0x84, 0x03, 0x01, // 58: i++
        0x1d, 0x1b, 0xa4, 0xff, 0xd9, // 61: if (i <= limit) goto 24
        0x1c, 0xac, // 66: return count
    ]
}

/// The driver's inputs for `code`: loops, bypassable headers, and the
/// rotation map as `CRATONVM_JIT_ROTATED_PREHEADER=1` computes it.
fn analyse(
    code: &[u8],
) -> (
    Vec<(usize, usize)>,
    FxHashSet<usize>,
    FxHashMap<usize, usize>,
) {
    let len = code.len();
    let loops = detect_loops(code, len);
    let bypassable = find_bypassable_loop_headers(code, len, &loops, &[]);
    let rotation = find_rotation_preheader_entries(code, len, &loops, &[]);
    (loops, bypassable, rotation)
}

#[test]
fn a_rotated_zero_fill_unrotates_to_the_javac_shape() {
    let code = ecj_zero_fill();
    let (loops, bypassable, rotation) = analyse(&code);
    assert_eq!(loops, vec![(5, 14)]);
    assert!(bypassable.contains(&5));
    assert_eq!(rotation.get(&5), Some(&2));
    let u = unrotate_loop(&code, code.len(), 5, 14).expect("the ecj loop unrotates");
    assert_eq!(u.header, 0);
    assert_eq!(u.back_edge, 12);
    assert_eq!(
        u.code,
        vec![
            0x1c, 0x1b, 0xa3, 0x00, 0x0d, // COND; if_icmpgt EXIT(15)
            0x2a, 0x1c, 0x03, 0x54, 0x84, 0x02, 0x01, // BODY
            0xa7, 0xff, 0xf4, // goto 0
            0xb1, 0x00, 0x00, // EXIT
        ],
        "exactly javac's top-tested spelling of the same loop"
    );
    // Through the driver's rule: detected, re-anchored at the real header.
    let fill = detect_batch_loop(
        &code,
        code.len(),
        5,
        14,
        &bypassable,
        &rotation,
        detect_bulk_zero_byte_fill_loop,
    )
    .expect("a relocated rotated zero fill is detected");
    assert_eq!(fill.header_pc, 5);
    assert_eq!(
        (fill.array_local, fill.iv_local, fill.bound_local),
        (0, 2, 1)
    );
    // Unarmed (no rotation map): the bypassable header keeps its veto.
    assert!(detect_batch_loop(
        &code,
        code.len(),
        5,
        14,
        &bypassable,
        &FxHashMap::default(),
        detect_bulk_zero_byte_fill_loop,
    )
    .is_none());
}

#[test]
fn a_rotated_int_sum_is_detected_with_its_real_header() {
    let code = ecj_int_sum();
    let (loops, bypassable, rotation) = analyse(&code);
    assert_eq!(loops, vec![(10, 21)]);
    assert_eq!(rotation.get(&10), Some(&7));
    let sum = detect_batch_loop(
        &code,
        code.len(),
        10,
        21,
        &bypassable,
        &rotation,
        |c, _, h, b| {
            let iv = find_induction_variable(c, h, b + bytecode_analysis::step(c, b))?;
            detect_int_array_sum_forms(c, h, b, iv, true)
        },
    )
    .expect("the rotated `s += a[i]` loop is a SIMD sum");
    assert_eq!((sum.header_pc, sum.back_edge_pc), (10, 21));
    assert_eq!((sum.iv_local, sum.acc_local, sum.array_local), (3, 1, 0));
    assert_eq!(sum.bound, SimdLoopBound::Local(2));
    assert!(!sum.acc_is_long);
}

#[test]
fn a_rotated_element_wise_loop_over_a_length_is_detected() {
    let code = ecj_add_over_length();
    let (_, bypassable, rotation) = analyse(&code);
    assert_eq!(rotation.get(&5), Some(&2));
    let e = detect_batch_loop(
        &code,
        code.len(),
        5,
        21,
        &bypassable,
        &rotation,
        |c, _, h, b| {
            let iv = find_induction_variable(c, h, b + bytecode_analysis::step(c, b))?;
            detect_int_array_element_wise_forms(c, h, b, iv, true)
        },
    )
    .expect("the rotated `c[i] = a[i] + b[i]` loop is element-wise");
    assert_eq!(e.header_pc, 5);
    assert_eq!((e.out_local, e.a_local, e.b_local), (0, 1, 2));
    assert_eq!(e.bound, SimdLoopBound::ArrayLength(0));
}

#[test]
fn the_rotated_matrix_dot_inner_loop_is_detected() {
    let code = ecj_matmul();
    let (loops, bypassable, rotation) = analyse(&code);
    assert!(loops.contains(&(28, 54)), "{loops:?}");
    assert_eq!(rotation.get(&28), Some(&25));
    let dot = detect_batch_loop(
        &code,
        code.len(),
        28,
        54,
        &bypassable,
        &rotation,
        |c, _, h, b| {
            let iv = find_induction_variable(c, h, b + bytecode_analysis::step(c, b))?;
            detect_matrix_dot_loop(c, h, b, iv)
        },
    )
    .expect("the rotated k-loop is the matrix dot");
    assert_eq!((dot.header_pc, dot.back_edge_pc), (28, 54));
    assert_eq!(
        (dot.iv_local, dot.bound_local, dot.acc_local),
        (7, 2, 6),
        "k, n, sum"
    );
    assert_eq!(
        (
            dot.a_outer_local,
            dot.a_row_local,
            dot.b_outer_local,
            dot.b_column_local
        ),
        (0, 4, 1, 5)
    );
}

#[test]
fn the_rotated_sieve_stride_loop_and_the_nest_are_detected() {
    let code = ecj_sieve();
    let (loops, bypassable, rotation) = analyse(&code);
    for l in [(5, 14), (41, 55), (24, 63)] {
        assert!(loops.contains(&l), "{l:?} in {loops:?}");
    }
    assert_eq!(rotation.get(&41), Some(&38));
    assert_eq!(rotation.get(&24), Some(&21));
    let stride = detect_batch_loop(
        &code,
        code.len(),
        41,
        55,
        &bypassable,
        &rotation,
        detect_bulk_set_byte_stride_loop,
    )
    .expect("the rotated `j += i` marking loop is a stride store");
    assert_eq!(stride.header_pc, 41);
    assert_eq!(
        (
            stride.array_local,
            stride.iv_local,
            stride.bound_local,
            stride.step_local
        ),
        (0, 4, 1, 3)
    );
    // The outer loop unrotates (its body's `ifne` and the inner loop's
    // `goto` / back edge are re-based). The inner loop is still rotated
    // in that copy; `unrotate_nested_loops` (wave 6) rewrites it in place
    // so the javac-shaped nest detector matches.
    let outer = unrotate_loop(&code, code.len(), 24, 63).expect("the outer loop unrotates");
    // COND (2 bytes) + negated test (3) = body base 5; the `ifne` at real
    // 27 (body offset 3) sits at synthetic 8 and targets real 58 (body
    // offset 34), synthetic 39: +31.
    assert_eq!(&outer.code[8..11], &[0x9a, 0x00, 0x1f]);
    // The inner `goto 52` at real 38 (offset 14 -> synthetic 19) targets
    // real 52 (offset 28 -> synthetic 33): +14, unchanged.
    assert_eq!(&outer.code[19..22], &[0xa7, 0x00, 0x0e]);
    // On the outer-only copy the nest detector does not match...
    assert!(
        detect_byte_sieve_loop(&outer.code, outer.code.len(), 0, outer.back_edge).is_none()
    );
    // ...and through the driver's rule (nested loops unrotated) it does,
    // anchored at the real outer header.
    let sieve = detect_batch_loop(
        &code,
        code.len(),
        24,
        63,
        &bypassable,
        &rotation,
        detect_byte_sieve_loop,
    )
    .expect("the rotated ecj sieve nest is matched");
    assert_eq!(sieve.header_pc, 24);
    assert_eq!(
        (
            sieve.array_local,
            sieve.outer_iv_local,
            sieve.bound_local,
            sieve.count_local,
            sieve.inner_iv_local
        ),
        (0, 3, 1, 2, 4),
        "a, i, limit, count, j"
    );
    // Unarmed (no rotation map): still refused.
    assert!(detect_batch_loop(
        &code,
        code.len(),
        24,
        63,
        &bypassable,
        &FxHashMap::default(),
        detect_byte_sieve_loop,
    )
    .is_none());
}

/// Round 9 wave 6 (lane `rotated6`): the nested rewrite of the ecj sieve
/// produces exactly javac's nest spelling, pc for pc.
#[test]
fn the_nested_rotated_sieve_loop_unrotates_in_place() {
    let code = ecj_sieve();
    let mut outer = unrotate_loop(&code, code.len(), 24, 63).expect("the outer loop unrotates");
    let before_len = outer.code.len();
    unrotate_nested_loops(&mut outer);
    assert_eq!(
        outer.code.len(),
        before_len,
        "an in-place rewrite keeps the length"
    );
    assert_eq!((outer.header, outer.back_edge), (0, 42));
    assert_eq!(
        outer.code,
        vec![
            0x1d, 0x1b, 0xa3, 0x00, 0x2b, // 0: if (i > limit) goto 45
            0x2a, 0x1d, 0x33, 0x9a, 0x00, 0x1f, // 5: if (a[i]) goto 39
            0x84, 0x02, 0x01, // 11: count++
            0x1d, 0x1d, 0x60, 0x36, 0x04, // 14: j = i + i
            0x15, 0x04, 0x1b, 0xa3, 0x00, 0x11, // 19: if (j > limit) goto 39
            0x2a, 0x15, 0x04, 0x04, 0x54, // 25: a[j] = true
            0x15, 0x04, 0x1d, 0x60, 0x36, 0x04, // 30: j += i
            0xa7, 0xff, 0xef, // 36: goto 19
            0x84, 0x03, 0x01, // 39: i++
            0xa7, 0xff, 0xd6, // 42: goto 0
            0xb1, 0x00, 0x00, // 45: EXIT
        ]
    );
    // Idempotent: nothing rotated is left.
    let again = outer.code.clone();
    unrotate_nested_loops(&mut outer);
    assert_eq!(outer.code, again);
}

/// The in-place rewrite refuses a nested loop with a second entry (a
/// branch from outside into its body) and a body branch into COND's
/// middle.
#[test]
fn a_nested_loop_with_a_second_entry_is_not_unrotated() {
    let code = ecj_sieve();
    let outer = unrotate_loop(&code, code.len(), 24, 63).expect("the outer loop unrotates");
    // Inner loop in the copy: goto at 19, header 22, COND 33, back edge 36.
    assert!(unrotate_loop_in_place(&outer.code, 22, 36).is_some());
    // Retarget the outer `ifne` (at 8) from 39 to the inner body (22):
    // a second entry into the inner loop.
    let mut second_entry = outer.code.clone();
    second_entry[9..11].copy_from_slice(&(22i16 - 8).to_be_bytes());
    assert!(unrotate_loop_in_place(&second_entry, 22, 36).is_none());
    // Retarget it at the inner COND's second instruction (35): refused
    // too (the middle of COND moves).
    let mut into_cond = outer.code.clone();
    into_cond[9..11].copy_from_slice(&(35i16 - 8).to_be_bytes());
    assert!(unrotate_loop_in_place(&into_cond, 22, 36).is_none());
    // Retarget it at the inner entry `goto` (19): allowed, it becomes
    // COND's first instruction.
    let mut to_entry = outer.code.clone();
    to_entry[9..11].copy_from_slice(&(19i16 - 8).to_be_bytes());
    assert!(unrotate_loop_in_place(&to_entry, 22, 36).is_some());
    // Not a rotated loop: the header 19 is not preceded by a `goto`.
    assert!(unrotate_loop_in_place(&outer.code, 19, 36).is_none());
}

#[test]
fn shapes_that_do_not_unrotate_are_refused() {
    // Top-tested loop (javac): no `goto` before the header.
    let javac_fill = [
        0x03, 0x3d, // 0: i = 0
        0x1c, 0x1b, 0xa3, 0x00, 0x0d, // 2: if (i > limit) goto 17
        0x2a, 0x1c, 0x03, 0x54, // 7: a[i] = 0
        0x84, 0x02, 0x01, 0xa7, 0xff, 0xf4, // 11: i++; goto 2
        0xb1, // 17
    ];
    assert!(unrotate_loop(&javac_fill, javac_fill.len(), 2, 14).is_none());

    // A body branch that leaves the loop anywhere but its fall-through
    // exit: `a[i] = 0` (4 bytes at 5) becomes `goto 18; nop`, and 17/18
    // are `nop; return`.
    let mut escaping = ecj_zero_fill();
    escaping.truncate(17);
    escaping.push(0x00); // 17: nop (the loop's fall-through exit)
    escaping.push(0xb1); // 18: return
    escaping[5..9].copy_from_slice(&[0xa7, 0x00, 0x0d, 0x00]);
    assert!(unrotate_loop(&escaping, escaping.len(), 5, 14).is_none());

    // A back edge that is not a conditional branch.
    let mut goto_back = ecj_zero_fill();
    goto_back[14] = 0xa7;
    assert!(unrotate_loop(&goto_back, goto_back.len(), 5, 14).is_none());
}
