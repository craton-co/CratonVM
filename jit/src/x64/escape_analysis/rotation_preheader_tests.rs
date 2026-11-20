use super::{detect_loops, find_bypassable_loop_headers, find_rotation_preheader_entries};

/// `s = 0; i = 0; goto COND; BODY: s += x*3 + 11; i++; COND: if (i < n)
/// goto BODY; return s` — locals 0=x, 1=n, 2=s, 3=i.
fn rotated() -> Vec<u8> {
    vec![
        0x03, // 0: iconst_0
        0x3d, // 1: istore_2
        0x03, // 2: iconst_0
        0x3e, // 3: istore_3
        0xa7, 0x00, 0x0f, // 4: goto +15 -> 19
        0x1c, // 7: iload_2             ; header
        0x1a, // 8: iload_0
        0x06, // 9: iconst_3
        0x68, // 10: imul
        0x10, 0x0b, // 11: bipush 11
        0x60, // 13: iadd
        0x60, // 14: iadd
        0x3d, // 15: istore_2
        0x84, 0x03, 0x01, // 16: iinc 3, 1
        0x1d, // 19: iload_3            ; COND
        0x1b, // 20: iload_1
        0xa1, 0xff, 0xf2, // 21: if_icmplt -14 -> 7
        0x1c, // 24: iload_2
        0xac, // 25: ireturn
    ]
}

#[test]
fn the_rotation_goto_is_the_preheader_site_of_a_bypassable_header() {
    let code = rotated();
    let len = code.len();
    let loops = detect_loops(&code, len);
    assert_eq!(loops, vec![(7, 21)]);
    assert!(
        find_bypassable_loop_headers(&code, len, &loops, &[]).contains(&7),
        "the rotated header is bypassable for the header-anchored pre-header"
    );
    let entries = find_rotation_preheader_entries(&code, len, &loops, &[]);
    assert_eq!(entries.get(&7), Some(&4));
    assert_eq!(entries.len(), 1);
}

#[test]
fn a_second_entry_into_the_loop_disqualifies_the_rotation() {
    // Prefix `iload_0; ifne +N` jumping straight into BODY past the goto.
    let mut code = vec![0x1a, 0x9a, 0x00, 0x00]; // 0: iload_0; 1: ifne -> patched
    let base = code.len();
    code.extend(rotated());
    // BODY is at base + 7; the `ifne` at pc 1 targets it.
    let off = (base + 7 - 1) as i16;
    code[2..4].copy_from_slice(&off.to_be_bytes());
    let len = code.len();
    let loops = detect_loops(&code, len);
    assert_eq!(loops, vec![(base + 7, base + 21)]);
    assert!(find_rotation_preheader_entries(&code, len, &loops, &[]).is_empty());
}

#[test]
fn a_branch_to_the_goto_itself_disqualifies_the_rotation() {
    // `iload_0; ifne +3` lands on the goto; the relocated pre-header would
    // sit before `pc_to_native[goto]` and that edge would skip it.
    let mut code = vec![0x1a, 0x9a, 0x00, 0x00];
    let base = code.len();
    code.extend(rotated());
    let goto_pc = base + 4;
    let off = (goto_pc - 1) as i16;
    code[2..4].copy_from_slice(&off.to_be_bytes());
    let len = code.len();
    let loops = detect_loops(&code, len);
    assert!(find_rotation_preheader_entries(&code, len, &loops, &[]).is_empty());
}

#[test]
fn a_handler_at_the_goto_or_into_the_loop_disqualifies_the_rotation() {
    let code = rotated();
    let len = code.len();
    let loops = detect_loops(&code, len);
    // A handler starting at the goto.
    assert!(find_rotation_preheader_entries(&code, len, &loops, &[(0, 4, 4)]).is_empty());
    // A handler inside the loop whose protected range starts before it.
    assert!(find_rotation_preheader_entries(&code, len, &loops, &[(0, 10, 19)]).is_empty());
    // A range wholly inside the loop is fine.
    assert_eq!(
        find_rotation_preheader_entries(&code, len, &loops, &[(7, 16, 19)]).get(&7),
        Some(&4)
    );
}

#[test]
fn a_top_tested_loop_is_not_a_rotation() {
    // javac: `i = 0; H: if (i >= n) exit; i++; goto H` — the header falls
    // through from the code before it; no rotation entry.
    let code = vec![
        0x03, // 0: iconst_0
        0x3c, // 1: istore_1
        0x1b, // 2: iload_1            ; header
        0x1a, // 3: iload_0
        0xa2, 0x00, 0x09, // 4: if_icmpge +9 -> 13
        0x84, 0x01, 0x01, // 7: iinc 1, 1
        0xa7, 0xff, 0xf8, // 10: goto -8 -> 2
        0xb1, // 13: return
    ];
    let len = code.len();
    let loops = detect_loops(&code, len);
    assert!(find_rotation_preheader_entries(&code, len, &loops, &[]).is_empty());
}
