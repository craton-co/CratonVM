use super::super::{detect_loops, find_arith_loop_hoists, find_loop_hoists};
use super::*;

#[test]
fn osr_entries_are_the_backward_branch_targets_and_only_inner_ones_count() {
    // `test_licm_nested_loops`' shape: outer header 0, inner header 6.
    let code: Vec<u8> = vec![
        0x15, 0x05, 0x1c, 0xa2, 0x00, 0x17, 0x15, 0x07, 0x1c, 0xa2, 0x00, 0x0a, 0x2a, 0x15,
        0x04, 0x32, 0x57, 0x84, 0x07, 0x01, 0xa7, 0xff, 0xf2, 0x84, 0x05, 0x01, 0xa7, 0xff,
        0xe6, 0, 0,
    ];
    let mut pcs = osr_entry_pcs(&code, 29);
    pcs.sort_unstable();
    assert_eq!(pcs, vec![0, 6]);
    assert!(
        span_holds_inner_osr_entry(&pcs, 0, 29),
        "the inner header is inside the outer loop"
    );
    assert!(
        !span_holds_inner_osr_entry(&pcs, 6, 23),
        "a loop's own header is not an inner entry"
    );
}

/// The `aaload` hoist of an invariant `a[i]` inside a nested loop goes to
/// the INNER loop: on the outer one the walk would publish the inner header
/// OSR-ineligible, which measured 28.9 s against 0.23 s for a method that
/// runs a long inner loop once (`OsrHoistCliff.aaload`).
#[test]
fn a_nested_aaload_hoist_is_attached_to_the_inner_loop() {
    let code: Vec<u8> = vec![
        0x15, 0x05, // 0: iload 5 (j)      outer header
        0x1c, // 2: iload_2 (n)
        0xa2, 0x00, 0x17, // 3: if_icmpge -> 26
        0x15, 0x07, // 6: iload 7 (k)      inner header
        0x1c, // 8: iload_2
        0xa2, 0x00, 0x0a, // 9: if_icmpge -> 19
        0x2a, // 12: aload_0           invariant a[i]
        0x15, 0x04, // 13: iload 4
        0x32, // 15: aaload
        0x57, // 16: pop
        0x84, 0x07, 0x01, // 17: iinc 7, 1
        0xa7, 0xff, 0xf2, // 20: goto -> 6
        0x84, 0x05, 0x01, // 23: iinc 5, 1
        0xa7, 0xff, 0xe6, // 26: goto -> 0
        0, 0,
    ];
    let loops = detect_loops(&code, 29);
    assert_eq!(loops.len(), 2, "{loops:?}");
    let hoists = find_loop_hoists(&code, 29, &loops);
    assert_eq!(hoists.len(), 1);
    assert_eq!(
        hoists[0].loop_header, 6,
        "the inner loop, not the outer one"
    );
    assert_eq!(hoists[0].seq_start, 12);
    assert_eq!(hoists[0].array_local, 0);
    assert_eq!(hoists[0].index_local, 4);
}

/// The same for an invariant arithmetic run: `s += base * 3 + 1` in the
/// inner loop is hoisted to the inner header (`OsrHoistCliff.arith`, 25.2 s
/// against 0.20 s with the outer attribution).
#[test]
fn a_nested_arith_hoist_is_attached_to_the_inner_loop() {
    // locals: 0=base 1=n 2=m 3=s 4=i 5=j
    let code: Vec<u8> = vec![
        0x15, 0x04, // 0: iload 4 (i)      outer header
        0x1b, // 2: iload_1
        0xa2, 0x00, 0x20, // 3: if_icmpge -> 35
        0x03, // 6: iconst_0
        0x36, 0x05, // 7: istore 5
        0x15, 0x05, // 9: iload 5 (j)      inner header
        0x1c, // 11: iload_2
        0xa2, 0x00, 0x11, // 12: if_icmpge -> 29
        0x1d, // 15: iload_3
        0x1a, // 16: iload_0           run start
        0x06, // 17: iconst_3
        0x68, // 18: imul
        0x04, // 19: iconst_1
        0x60, // 20: iadd              run end (21)
        0x60, // 21: iadd
        0x3e, // 22: istore_3
        0x84, 0x05, 0x01, // 23: iinc 5, 1
        0xa7, 0xff, 0xef, // 26: goto -> 9
        0x84, 0x04, 0x01, // 29: iinc 4, 1
        0xa7, 0xff, 0xe0, // 32: goto -> 0
        0xb1, // 35: return
    ];
    let loops = detect_loops(&code, code.len());
    assert_eq!(loops.len(), 2, "{loops:?}");
    let hoists = find_arith_loop_hoists(&code, code.len(), &loops);
    assert_eq!(hoists.len(), 1, "one run");
    assert_eq!(
        hoists[0].loop_header, 9,
        "the inner loop, not the outer one"
    );
    assert_eq!((hoists[0].seq_start, hoists[0].seq_end), (16, 21));
}

/// An outer loop's `i < rows.length` stays in the body when an inner loop
/// sits inside it (`OsrHoistCliff.len`, 18.2 s against 0.21 s hoisted);
/// with the inner back edge `nop`ed out the same site is hoisted.
#[test]
fn an_outer_arraylength_is_not_hoisted_over_an_inner_loop() {
    // locals: 0=rows 1=m 2=s 3=i 4=j
    let nested: Vec<u8> = vec![
        0x1d, // 0: iload_3            outer header
        0x2a, // 1: aload_0
        0xbe, // 2: arraylength
        0xa2, 0x00, 0x1b, // 3: if_icmpge -> 30
        0x03, // 6: iconst_0
        0x36, 0x04, // 7: istore 4
        0x15, 0x04, // 9: iload 4          inner header
        0x1b, // 11: iload_1
        0xa2, 0x00, 0x0c, // 12: if_icmpge -> 24
        0x84, 0x02, 0x01, // 15: iinc 2, 1
        0x84, 0x04, 0x01, // 18: iinc 4, 1
        0xa7, 0xff, 0xf4, // 21: goto -> 9
        0x84, 0x03, 0x01, // 24: iinc 3, 1
        0xa7, 0xff, 0xe5, // 27: goto -> 0
        0xb1, // 30: return
    ];
    let loops = detect_loops(&nested, nested.len());
    assert_eq!(loops.len(), 2, "{loops:?}");
    assert!(
        find_array_len_hoists(&nested, nested.len(), &loops).is_empty(),
        "the outer header's arraylength must stay in the body"
    );

    let mut flat = nested.clone();
    flat[21] = 0x00;
    flat[22] = 0x00;
    flat[23] = 0x00;
    let flat_loops = detect_loops(&flat, flat.len());
    assert_eq!(flat_loops, vec![(0, 27)]);
    let hoists = find_array_len_hoists(&flat, flat.len(), &flat_loops);
    assert_eq!(hoists.len(), 1, "{hoists:?}");
    assert_eq!(hoists[0].loop_header, 0);
    assert_eq!(hoists[0].sites, vec![(1, 3)]);
}
