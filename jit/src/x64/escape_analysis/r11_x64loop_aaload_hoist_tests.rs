use super::{detect_loops, find_loop_hoists};

/// `static int f(Object[][] a, Object[][] b, boolean c, int r, int n)
///  { int s = 0; for (int k = 0; k < n; k++) s += (c ? a : b)[r].length; return s; }`
///
/// Locals: 0=a, 1=b, 2=c, 3=r, 4=n, 5=s, 6=k.
#[rustfmt::skip]
fn ternary_row() -> Vec<u8> {
    vec![
        0x03, 0x36, 0x05,       //  0: s = 0
        0x03, 0x36, 0x06,       //  3: k = 0
        0x15, 0x06,             //  6: iload k        ; header
        0x15, 0x04,             //  8: iload n
        0xa2, 0x00, 0x1c,       // 10: if_icmpge +28 -> 38
        0x1c,                   // 13: iload_2 (c)
        0x99, 0x00, 0x07,       // 14: ifeq +7 -> 21
        0x2a,                   // 17: aload_0 (a)
        0xa7, 0x00, 0x04,       // 18: goto +4 -> 22
        0x2b,                   // 21: aload_1 (b)    ; L1
        0x1d,                   // 22: iload_3 (r)    ; L2 -- entered from 18
        0x32,                   // 23: aaload
        0xbe,                   // 24: arraylength
        0x15, 0x05,             // 25: iload s
        0x60,                   // 27: iadd
        0x36, 0x05,             // 28: istore s
        0x84, 0x06, 0x01,       // 30: iinc k, 1
        0xa7, 0xff, 0xe5,       // 33: goto -27 -> 6
        0x00, 0x00,             // 36: nop; nop
        0x15, 0x05,             // 38: iload s
        0xac,                   // 40: ireturn
    ]
}

/// A branch into the middle of `aload b; iload r; aaload` must refuse the
/// hoist: the walk would point pc 22 past the slot load that replaced the
/// whole sequence, and the `c == true` path would arrive there holding
/// `a` itself as the row.
#[test]
fn an_aaload_sequence_entered_in_the_middle_is_not_hoisted() {
    let code = ternary_row();
    let loops = detect_loops(&code, code.len());
    assert!(
        loops.iter().any(|&(h, b)| h == 6 && b == 33),
        "the counted loop must be found: {loops:?}"
    );
    let hoists = find_loop_hoists(&code, code.len(), &loops);
    assert!(
        hoists.iter().all(|h| h.seq_start != 21),
        "`aload b; iload r; aaload` at 21 is entered at 22 by the goto at 18"
    );
}

/// Positive control: the same loop reading `b[r]` directly still hoists.
#[test]
fn the_same_sequence_without_the_join_is_still_hoisted() {
    let mut code = ternary_row();
    // 13..=20 -> nops: `c` is not read and nothing jumps to 22.
    for b in &mut code[13..21] {
        *b = 0x00;
    }
    let loops = detect_loops(&code, code.len());
    let hoists = find_loop_hoists(&code, code.len(), &loops);
    assert!(
        hoists
            .iter()
            .any(|h| h.seq_start == 21 && h.array_local == 1 && h.index_local == 3),
        "{} hoist(s), none of `b[r]` at 21",
        hoists.len()
    );
}
