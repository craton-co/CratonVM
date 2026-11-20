use super::super::detect_loops;
use super::*;

/// The exact javac output for
///
/// ```java
/// static void f(int[] a, int n) {
///     int i = 0;
///     while (i < n) { i++; a[i] = i; }
/// }
/// ```
///
/// The `iastore` at pc 13 reads `i` AFTER the `iinc` at pc 7, so its index
/// is `iv + 1`. A guard proving `a.length >= n` says nothing about index
/// `n`, so this access must KEEP its per-element check.
fn advance_then_store() -> Vec<u8> {
    vec![
        0x03, // 0: iconst_0
        0x3d, // 1: istore_2
        0x1c, // 2: iload_2      <- header
        0x1b, // 3: iload_1
        0xa2, 0x00, 0x0d, // 4: if_icmpge +13 -> 17
        0x84, 0x02, 0x01, // 7: iinc 2, 1
        0x2a, // 10: aload_0
        0x1c, // 11: iload_2     <- the index, already advanced
        0x1c, // 12: iload_2
        0x4f, // 13: iastore
        0xa7, 0xff, 0xf4, // 14: goto -12 -> 2
        0xb1, // 17: return
    ]
}

/// The `i < a.length` spelling, which reached `BoundsProof::Static` — the
/// check vanished with no guard at all, which is strictly worse than the
/// guarded form.
fn advance_then_store_against_length() -> Vec<u8> {
    vec![
        0x03, // 0: iconst_0
        0x3c, // 1: istore_1
        0x1b, // 2: iload_1      <- header
        0x2a, // 3: aload_0
        0xbe, // 4: arraylength
        0xa2, 0x00, 0x0d, // 5: if_icmpge +13 -> 18
        0x84, 0x01, 0x01, // 8: iinc 1, 1
        0x2a, // 11: aload_0
        0x1b, // 12: iload_1     <- the index, already advanced
        0x1b, // 13: iload_1
        0x4f, // 14: iastore
        0xa7, 0xff, 0xf3, // 15: goto -13 -> 2
        0xb1, // 18: return
    ]
}

fn elided(code: &[u8]) -> Vec<usize> {
    let loops = detect_loops(code, code.len());
    let (safe, _guards) = analyze_bounds_elimination(code, code.len(), &loops);
    let mut v: Vec<usize> = safe.into_iter().collect();
    v.sort_unstable();
    v
}

#[test]
fn an_index_read_after_an_iinc_keeps_its_check() {
    let code = advance_then_store();
    assert!(
        !elided(&code).contains(&13),
        "the `iastore` at pc 13 indexes `i + 1` while the guard bounds `i`; eliding its \
         check stores one past the end on the final iteration"
    );
}

#[test]
fn an_index_read_after_an_iinc_keeps_its_check_against_arraylength() {
    let code = advance_then_store_against_length();
    assert!(
        !elided(&code).contains(&14),
        "the `iastore` at pc 14 indexes `i + 1` while the exit test bounds `i` by \
         `a.length`; this spelling reached BoundsProof::Static, so the check vanished with \
         no guard at all"
    );
}

/// The ordinary javac counted-loop shape — `a[i]` then `i++` — must still
/// be elided, or the fix has cost the optimization the pass exists for.
#[test]
fn the_ordinary_counted_loop_is_still_elided() {
    // 0: iconst_0 ; 1: istore_1
    // 2: iload_1 ; 3: aload_0 ; 4: arraylength ; 5: if_icmpge +13 -> 18
    // 8: aload_0 ; 9: iload_1 ; 10: iload_1 ; 11: iastore
    // 12: iinc 1,1 ; 15: goto -13 -> 2 ; 18: return
    let code = vec![
        0x03, 0x3c, 0x1b, 0x2a, 0xbe, 0xa2, 0x00, 0x0d, 0x2a, 0x1b, 0x1b, 0x4f, 0x84, 0x01,
        0x01, 0xa7, 0xff, 0xf3, 0xb1,
    ];
    assert!(
        elided(&code).contains(&11),
        "`for (i = 0; i < a.length; i++) a[i] = i;` must keep its bounds-check elision"
    );
}
