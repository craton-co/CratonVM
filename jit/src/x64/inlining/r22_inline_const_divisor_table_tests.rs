use super::inline_const_divisor;
use crate::ir::splice_division_const_divisor;

/// `(window, push pc, division pc)`: the dividend load, the push, the
/// division, a return.
fn windows() -> Vec<(Vec<u8>, usize, usize)> {
    let mut w = Vec::new();
    // iconst_m1..iconst_5 under idiv and irem.
    for push in 0x02u8..=0x08 {
        for div in [0x6c, 0x70] {
            w.push((vec![0x1a, push, div, 0xac], 1, 2));
        }
    }
    // bipush, positive, negative, zero, -1.
    for b in [31u8, 0x80, 0, 0xff] {
        w.push((vec![0x1a, 0x10, b, 0x6c, 0xac], 1, 3));
    }
    // sipush.
    for v in [-300i16, 0, 1000, -1] {
        let [hi, lo] = v.to_be_bytes();
        w.push((vec![0x1a, 0x11, hi, lo, 0x70, 0xac], 1, 4));
    }
    // ldc and ldc_w (int), ldc fp-tagged at pc 1 by the site below.
    w.push((vec![0x1a, 0x12, 0x05, 0x6c, 0xac], 1, 3));
    w.push((vec![0x1a, 0x13, 0x00, 0x05, 0x70, 0xac], 1, 4));
    // lconst_0 / lconst_1 under ldiv and lrem.
    for push in [0x09u8, 0x0a] {
        for div in [0x6d, 0x71] {
            w.push((vec![0x1e, push, div, 0xad], 1, 2));
        }
    }
    // ldc2_w.
    w.push((vec![0x1e, 0x14, 0x00, 0x05, 0x71, 0xad], 1, 4));
    // Width mismatches: an int push under ldiv, a long push under idiv.
    w.push((vec![0x1e, 0x10, 7, 0x6d, 0xad], 1, 3));
    w.push((vec![0x1a, 0x0a, 0x6c, 0xac], 1, 2));
    // A push not ending at the division.
    w.push((vec![0x10, 7, 0x1a, 0x6c, 0xac], 0, 3));
    // A local divisor.
    w.push((vec![0x1a, 0x1b, 0x6c, 0xac], 1, 2));
    // A division pc past `code_len` (the window is cut below).
    w.push((vec![0x1a, 0x10, 7], 1, 3));
    // A division that a branch lands on: iload_0; iload_1; ifeq -> 7;
    // bipush 7; idiv (7); ireturn.
    w.push((vec![0x1a, 0x1b, 0x99, 0x00, 0x05, 0x10, 7, 0x6c, 0xac], 5, 7));
    w
}

fn site(ldc: i64, ldc_fp: bool) -> crate::InlineSite {
    let mut s = crate::InlineSite::default();
    s.ldc_info = vec![(1, ldc)];
    s.ldc2w_info = vec![(1, 1_000_000_007)];
    if ldc_fp {
        s.ldc_fp_pcs = vec![1];
    }
    s
}

#[test]
fn the_adapter_agrees_with_the_ir_decoder_on_every_window() {
    for ldc in [1000i64, 0, -1, 1 << 32] {
        for fp in [false, true] {
            let s = site(ldc, fp);
            for (code, push, div) in windows() {
                let is_64bit = matches!(code.get(div), Some(0x6d | 0x71));
                let got = inline_const_divisor(&code, code.len(), push, div, is_64bit, &s);
                let op = code[push];
                let width_ok = if is_64bit {
                    matches!(op, 0x09 | 0x0a | 0x14)
                } else {
                    matches!(op, 0x02..=0x08 | 0x10..=0x13)
                };
                let adjacent = push + crate::bytecode_analysis::step(&code, push) == div;
                let ir = splice_division_const_divisor(&code, code.len(), div, |pc| {
                    if s.ldc_fp_pcs.contains(&pc) {
                        return None;
                    }
                    match code[pc] {
                        0x14 => s.ldc2w_info.iter().find(|(p, _)| *p == pc).map(|r| r.1),
                        // Cast: an int `ldc`, as the emitter reads it.
                        _ => s
                            .ldc_info
                            .iter()
                            .find(|(p, _)| *p == pc)
                            .map(|r| i64::from(r.1 as i32)),
                    }
                });
                let want = if width_ok && adjacent { ir } else { None };
                assert_eq!(
                    got, want,
                    "window {code:x?} push {push} div {div} ldc {ldc} fp {fp}"
                );
                // Never a zero divisor, at the division's width.
                if let Some(d) = got {
                    assert!(if is_64bit { d != 0 } else { d as i32 != 0 }, "{code:x?}");
                }
            }
        }
    }
}

/// The decoded values themselves, for the shapes the splice walk sees.
#[test]
fn the_adapter_decodes_the_reviewed_pushes() {
    let s = site(1000, false);
    let at = |code: &[u8], push, div, wide| {
        inline_const_divisor(code, code.len(), push, div, wide, &s)
    };
    assert_eq!(at(&[0x1a, 0x08, 0x6c, 0xac], 1, 2, false), Some(5));
    assert_eq!(at(&[0x1a, 0x02, 0x70, 0xac], 1, 2, false), Some(-1));
    assert_eq!(at(&[0x1a, 0x10, 0x80, 0x6c, 0xac], 1, 3, false), Some(-128));
    assert_eq!(at(&[0x1a, 0x12, 0x05, 0x6c, 0xac], 1, 3, false), Some(1000));
    assert_eq!(at(&[0x1e, 0x14, 0x00, 0x05, 0x71, 0xad], 1, 4, true), Some(1_000_000_007));
    assert_eq!(at(&[0x1e, 0x0a, 0x6d, 0xad], 1, 2, true), Some(1));
    // An int `ldc` whose low 32 bits are zero is a zero divisor.
    let s = site(1 << 32, false);
    assert_eq!(inline_const_divisor(&[0x1a, 0x12, 0x05, 0x6c, 0xac], 5, 1, 3, false, &s), None);
    // A branch onto the division, and a division past the window.
    let s = site(1000, false);
    let branch_onto_division = [0x1a, 0x1b, 0x99, 0x00, 0x05, 0x10, 7, 0x6c, 0xac];
    assert_eq!(
        inline_const_divisor(&branch_onto_division, 9, 5, 7, false, &s),
        None
    );
    assert_eq!(inline_const_divisor(&[0x1a, 0x10, 7, 0x6c], 3, 1, 3, false, &s), None);
}
