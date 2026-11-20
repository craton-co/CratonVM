use super::super::detect_loops;
use super::*;

/// `static int f(int[] a, int k, boolean c)
///  { int s = 0; for (int i = 0; i < a.length; i = (c ? k : i) + 1) s += a[i]; return s; }`
///
/// Locals: 0=a, 1=k, 2=c, 3=s, 4=i. The update's `iconst_1` at 29 is the
/// join `M` of the ternary, so the stored value is `k + 1` on one path and
/// the `iaload` at 16 must keep its check.
#[rustfmt::skip]
fn ternary_update() -> Vec<u8> {
    vec![
        0x03,             //  0: iconst_0
        0x3e,             //  1: istore_3        ; s = 0
        0x03,             //  2: iconst_0
        0x36, 0x04,       //  3: istore 4        ; i = 0
        0x15, 0x04,       //  5: iload 4         ; header
        0x2a,             //  7: aload_0
        0xbe,             //  8: arraylength
        0xa2, 0x00, 0x1b, //  9: if_icmpge +27 -> 36
        0x1d,             // 12: iload_3
        0x2a,             // 13: aload_0
        0x15, 0x04,       // 14: iload 4
        0x2e,             // 16: iaload
        0x60,             // 17: iadd
        0x3e,             // 18: istore_3
        0x1c,             // 19: iload_2         ; c
        0x99, 0x00, 0x07, // 20: ifeq +7 -> 27
        0x1b,             // 23: iload_1         ; k
        0xa7, 0x00, 0x05, // 24: goto +5 -> 29
        0x15, 0x04,       // 27: iload 4         ; L
        0x04,             // 29: iconst_1        ; M (join)
        0x60,             // 30: iadd
        0x36, 0x04,       // 31: istore 4
        0xa7, 0xff, 0xe4, // 33: goto -28 -> 5
        0x1d,             // 36: iload_3
        0xac,             // 37: ireturn
    ]
}

#[test]
fn a_join_inside_the_iv_update_keeps_the_check() {
    let code = ternary_update();
    assert!(iv_store_advance_is_entered(&code, code.len(), 5, 36, 4));
    let loops = detect_loops(&code, code.len());
    let (safe, _) = analyze_bounds_elimination(&code, code.len(), &loops);
    assert!(
        !safe.contains(&16),
        "`i = (c ? k : i) + 1` is not `i++`; eliding a[i] reads below the array for k < -1"
    );
}

/// Positive control: `i = i + 1` spelled with `istore` still has no join.
#[test]
fn a_plain_store_advance_is_not_entered() {
    let mut code = ternary_update();
    // 19..=28 -> nops, 27..=28 back to `iload 4`: `iload 4; iconst_1; iadd; istore 4`.
    for b in &mut code[19..27] {
        *b = 0x00;
    }
    assert!(!iv_store_advance_is_entered(&code, code.len(), 5, 36, 4));
}
