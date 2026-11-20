use super::*;

/// ecj/kotlinc-style ROTATED `for (i = 0; i < a.length; i++) s += a[i]`:
/// `goto COND; BODY (header 7); COND (16): iload i; aload a; arraylength;
/// if_icmplt BODY`.
const ROTATED_SUM: [u8; 24] = [
    0x03, // 0: iconst_0
    0x3c, // 1: istore_1
    0x03, // 2: iconst_0
    0x3d, // 3: istore_2
    0xa7, 0x00, 0x0c, // 4: goto +12 -> 16
    0x1b, // 7: iload_1        ; header (BODY)
    0x2a, // 8: aload_0
    0x1c, // 9: iload_2
    0x2e, // 10: iaload
    0x60, // 11: iadd
    0x3c, // 12: istore_1
    0x84, 0x02, 0x01, // 13: iinc 2, 1
    0x1c, // 16: iload_2        ; COND
    0x2a, // 17: aload_0
    0xbe, // 18: arraylength
    0xa1, 0xff, 0xf4, // 19: if_icmplt -12 -> 7
    0x1b, // 22: iload_1
    0xac, // 23: ireturn
];

fn rotation() -> FxHashMap<usize, usize> {
    let mut m = FxHashMap::default();
    m.insert(7usize, 4usize);
    m
}

#[test]
fn the_cond_site_opens_a_hoist_only_when_the_loop_is_relocated() {
    let loops = vec![(7usize, 19usize)];
    // Unrelocated: COND is not on the HEADER's straight-line prefix (the
    // body's `iaload` is in the way), so nothing is hoisted.
    assert!(find_array_len_hoists(&ROTATED_SUM, ROTATED_SUM.len(), &loops).is_empty());
    let hoists = find_array_len_hoists_with_rotation(
        &ROTATED_SUM,
        ROTATED_SUM.len(),
        &loops,
        &rotation(),
    );
    assert_eq!(hoists.len(), 1, "{hoists:?}");
    assert_eq!(hoists[0].loop_header, 7);
    assert_eq!(hoists[0].array_local, 0);
    assert_eq!(hoists[0].sites, vec![(17, 19)]);
    assert_eq!(hoists[0].trap_bci, 18, "the COND arraylength traps first");
}

#[test]
fn a_body_only_site_never_opens_a_rotated_hoist() {
    // `goto COND; BODY: aload_0; arraylength; istore_1; iinc 2 1;
    //  COND: iload_2; iload_3; if_icmplt BODY; return` -- a zero-trip
    // rotated loop never evaluates the body's `a.length`.
    let code: [u8; 18] = [
        0xa7, 0x00, 0x09, // 0: goto +9 -> 9
        0x2a, // 3: aload_0          ; header
        0xbe, // 4: arraylength
        0x3c, // 5: istore_1
        0x84, 0x02, 0x01, // 6: iinc 2, 1
        0x1c, // 9: iload_2          ; COND
        0x1d, // 10: iload_3
        0xa1, 0xff, 0xf8, // 11: if_icmplt -8 -> 3
        0xb1, // 14: return
        0x00, 0x00, 0x00, // padding
    ];
    let loops = vec![(3usize, 11usize)];
    let mut rotation = FxHashMap::default();
    rotation.insert(3usize, 0usize);
    assert!(
        find_array_len_hoists_with_rotation(&code, 15, &loops, &rotation).is_empty(),
        "the body site is on the HEADER's prefix, which a rotated loop may skip"
    );
    // The same site IS hoistable for a top-tested reading of the header,
    // which is what the unrelocated finder answers (the driver then drops
    // it at a bypassable header).
    let top = find_array_len_hoists(&code, 15, &loops);
    assert_eq!(top.len(), 1);
    assert_eq!(top[0].trap_bci, 4);
}

#[test]
fn a_top_tested_loop_is_unchanged_by_an_empty_rotation_map() {
    // `iload_1; aload_0; arraylength; if_icmpge +10; iinc 1 1; goto -9`
    let code: [u8; 14] = [
        0x1b, // 0: iload_1   ; header
        0x2a, // 1: aload_0
        0xbe, // 2: arraylength
        0xa2, 0x00, 0x09, // 3: if_icmpge +9 -> 12
        0x84, 0x01, 0x01, // 6: iinc 1, 1
        0xa7, 0xff, 0xf7, // 9: goto -9 -> 0
        0xb1, // 12: return
        0x00,
    ];
    let loops = vec![(0usize, 9usize)];
    let plain = find_array_len_hoists(&code, 13, &loops);
    let with = find_array_len_hoists_with_rotation(&code, 13, &loops, &FxHashMap::default());
    assert_eq!(plain.len(), 1);
    assert_eq!(with.len(), 1);
    assert_eq!(plain[0].sites, with[0].sites);
    assert_eq!(plain[0].trap_bci, 2);
    assert_eq!(with[0].trap_bci, 2);
}
