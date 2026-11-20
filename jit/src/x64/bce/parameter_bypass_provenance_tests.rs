use super::*;

/// `void fill(int[] a, int n, boolean all) { if (all) n = a.length;
///  for (int i = 0; i < n; i++) a[i] = 0; }` — locals a=0, n=1, all=2, i=3.
fn fill_with_param_bypass() -> Vec<u8> {
    vec![
        0x1c, //             0: iload_2        (all)
        0x99, 0x00, 0x06, // 1: ifeq -> 7      <- skips the store
        0x2a, //             4: aload_0
        0xbe, //             5: arraylength
        0x3c, //             6: istore_1       (n = a.length)
        0x00, //             7: nop            (join)
        0x03, //             8: iconst_0
        0x3e, //             9: istore_3       (i = 0)
        0x1d, //            10: iload_3        <- header
        0x1b, //            11: iload_1
        0xa2, 0x00, 0x0d, // 12: if_icmpge -> 25
        0x2a, //            15: aload_0
        0x1d, //            16: iload_3
        0x03, //            17: iconst_0
        0x4f, //            18: iastore
        0x84, 0x03, 0x01, // 19: iinc 3, 1
        0xa7, 0xff, 0xf4, // 22: goto -> 10
        0xb1, //            25: return
    ]
}

#[test]
fn a_store_a_branch_can_skip_is_not_the_bounds_provenance() {
    let code = fill_with_param_bypass();
    assert_eq!(
        find_bound_arraylength_provenance(&code, code.len(), 1),
        None,
        "`all == false` reaches the loop with the caller's `n`; naming \
         `a.length` as the limit deletes the store's bounds check"
    );
    // The twin with the branch replaced by `nop`s: the store now runs on
    // every path, and the provenance is real.
    let mut straight = code.clone();
    for b in straight.iter_mut().take(4) {
        *b = 0x00;
    }
    assert_eq!(
        find_bound_arraylength_provenance(&straight, straight.len(), 1),
        Some(0)
    );
}

/// End to end: the store at 18 must never be STATICALLY elided. It may be
/// elided behind a speculative guard, which re-checks `a.length >= n` at
/// run time.
#[test]
fn the_bypassable_bound_never_licenses_a_static_elision() {
    let code = fill_with_param_bypass();
    let (safe, guards) = analyze_bounds_elimination(&code, code.len(), &[(10, 22)]);
    let guarded = guards.iter().any(|g| g.covered_pcs.contains(&18));
    assert!(
        !safe.contains(&18) || guarded,
        "iastore at 18 elided with no guard: safe={safe:?}"
    );
}

/// With the exception table, a `catch` that can only run AFTER the store
/// keeps the proof; one whose range covers the `arraylength` (which can
/// throw before the store runs) does not. Without the table every
/// handler-only pc is a possible entry and the proof is refused.
#[test]
fn the_exception_table_decides_which_handlers_bypass_the_store() {
    #[rustfmt::skip]
    let code: Vec<u8> = vec![
        0x2a,             // 0: aload_0
        0xbe,             // 1: arraylength
        0x3c,             // 2: istore_1     (n = a.length)
        0xb8, 0x00, 0x09, // 3: invokestatic (may throw)
        0xa7, 0x00, 0x05, // 6: goto -> 11
        0x57,             // 9: pop          (handler entry)
        0x00,             // 10: nop
        0x1b,             // 11: iload_1     (join: reads n)
        0xac,             // 12: ireturn
    ];
    let n = code.len();
    assert_eq!(
        bound_arraylength_provenance_with(&code, n, 1, Some(&[(3, 6, 9)])),
        Some(0),
        "the handler covers only the call, which runs after the store"
    );
    assert_eq!(
        bound_arraylength_provenance_with(&code, n, 1, Some(&[(0, 6, 9)])),
        None,
        "the handler covers the arraylength, which can throw before the store"
    );
    assert_eq!(
        find_bound_arraylength_provenance(&code, n, 1),
        None,
        "without the table the handler-only code is a possible entry"
    );
}

/// A handler reachable around the store only THROUGH another handler is
/// still a way around it. Round 11 wave 2 made the dominance walk
/// incremental; listing the dependent handler first makes the answer need
/// a second round.
#[test]
fn a_handler_entered_only_from_another_handler_still_bypasses_the_store() {
    #[rustfmt::skip]
    let code: Vec<u8> = vec![
        0x2a,             // 0: aload_0
        0xbe,             // 1: arraylength   (may throw before the store)
        0x3c,             // 2: istore_1      (n = a.length)
        0xb8, 0x00, 0x09, // 3: invokestatic
        0xa7, 0x00, 0x09, // 6: goto -> 15
        0x57,             // 9: pop           (handler A)
        0xb8, 0x00, 0x09, // 10: invokestatic (protected by B)
        0xb1,             // 13: return
        0x57,             // 14: pop          (handler B)
        0x1b,             // 15: iload_1      (reads n)
        0xac,             // 16: ireturn
    ];
    let n = code.len();
    assert_eq!(
        bound_arraylength_provenance_with(&code, n, 1, Some(&[(10, 13, 14)])),
        Some(0),
        "B's range is reachable only after the store"
    );
    assert_eq!(
        bound_arraylength_provenance_with(&code, n, 1, Some(&[(10, 13, 14), (0, 2, 9)])),
        None,
        "A is entered before the store and leads into B's range, and B reads n"
    );
}

/// `void f(int i, boolean c) { if (c) i = 0; return i; }` — the IV-start
/// proof has the same hole: one store of `0`, skippable.
#[test]
fn a_skippable_constant_store_does_not_prove_a_non_negative_start() {
    // Locals: i=0, c=1.
    let code: Vec<u8> = vec![
        0x1b, //             0: iload_1
        0x99, 0x00, 0x05, // 1: ifeq -> 6
        0x03, //             4: iconst_0
        0x3b, //             5: istore_0   (i = 0)
        0x1a, //             6: iload_0    <- reached with the caller's i
        0xac, //             7: ireturn
    ];
    assert!(!find_iv_nonneg_start(&code, code.len(), 0));
    let mut straight = code.clone();
    for b in straight.iter_mut().take(4) {
        *b = 0x00;
    }
    assert!(find_iv_nonneg_start(&straight, straight.len(), 0));
}
