use super::super::detect_loops;
use super::*;

/// ```java
/// static void f(int[] a) {
///     for (int i = 0; i < 1; ) {
///         i++;
///         for (int j = 0; j < i; j++) a[j] = 42;
///     }
/// }
/// ```
///
/// The outer exit test only ever sees `i == 0`, but the inner loop runs
/// with `i == 1`: one `a[0] = 42`, which must throw for `new int[0]`.
/// Binding `i` to the exit test's `[0, 0]` made the inner span empty, and
/// scev answered an empty span with `BoundsProof::Static` (since round 11
/// wave 2 it refuses one under a bound local; the placement check here is
/// what still covers a shifted non-empty span).
#[rustfmt::skip]
fn advance_before_inner_loop() -> Vec<u8> {
    vec![
        0x03,             //  0: iconst_0
        0x3c,             //  1: istore_1        ; i = 0
        0x1b,             //  2: iload_1         ; outer header
        0x04,             //  3: iconst_1
        0xa2, 0x00, 0x1b, //  4: if_icmpge +27 -> 31
        0x84, 0x01, 0x01, //  7: iinc 1, 1       ; i++ BEFORE the inner loop
        0x03,             // 10: iconst_0
        0x3d,             // 11: istore_2        ; j = 0
        0x1c,             // 12: iload_2         ; inner header
        0x1b,             // 13: iload_1
        0xa2, 0x00, 0x0e, // 14: if_icmpge +14 -> 28
        0x2a,             // 17: aload_0
        0x1c,             // 18: iload_2
        0x10, 0x2a,       // 19: bipush 42
        0x4f,             // 21: iastore
        0x84, 0x02, 0x01, // 22: iinc 2, 1
        0xa7, 0xff, 0xf3, // 25: goto -13 -> 12
        0xa7, 0xff, 0xe6, // 28: goto -26 -> 2
        0xb1,             // 31: return
    ]
}

#[test]
fn an_inner_access_is_not_statically_elided_on_a_stale_outer_range() {
    let code = advance_before_inner_loop();
    let loops = detect_loops(&code, code.len());
    let (safe, guards) = analyze_bounds_elimination(&code, code.len(), &loops);
    let guarded = guards.iter().any(|g| g.covered_pcs.contains(&21));
    assert!(
        !safe.contains(&21) || guarded,
        "the `iastore` at 21 runs once with j == 0 < i == 1; a STATIC elision (no \
         guard covers it) writes past an empty array"
    );
}
