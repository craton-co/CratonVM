// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! JIT round 21, lane strings2 (STRINGS21-1,
//! `CRATONVM_JIT_IR_LATIN1_CASE_MAP_PREFIX`): the machine code of the
//! optimizing tier's inline `StringLatin1.toLowerCase` / `toUpperCase(String,
//! byte[], Locale)` prefix (`ir_lower::Lowerer::try_emit_ir_latin1_case_map_prefix`),
//! split out so each pass is executed by a unit test on its own.
//!
//! The JDK 25 bodies (`jdk25src` `StringLatin1.java` 416 / 483):
//!
//! * `toLowerCase`: a null `locale` throws; `first` = the first byte with
//!   `CharacterDataLatin1.isUpperCase` (over Latin-1: `A-Z`, `0xC0..=0xDE`
//!   except `0xD7`); none answers `str` itself; a language `== "tr"`, `"az"`
//!   or `"lt"` takes `toLowerCaseEx`; else a fresh `byte[len]` holds every
//!   byte through `CharacterDataLatin1.toLowerCase` (exactly those bytes `| 0x20`,
//!   every other byte itself) and `new String(result, LATIN1)` answers.
//! * `toUpperCase`: the same shape with `notUpperCaseEx` (`a-z`, `0xB5`,
//!   `0xDF..=0xFF` except `0xF7`) and `toUpperCaseEx`, whose answer for `0xB5`
//!   (`0x39C`), `0xFF` (`0x178`) and `0xDF` (`"SS"`) leaves Latin-1 and takes
//!   `toUpperCaseEx(.., false)`: those three bytes decline here unless the
//!   `sharp_s` (`0xDF`, LATIN1 "SS") / `wide` (`0xB5` / `0xFF`, a UTF16
//!   result, JIT round 21 lane latin1up) arms answer them.
//!
//! The locale-dependent arms differ from the plain fold on Latin-1 input only
//! at `I` (`tr` / `az`: dotless `ı`), `0xCC` / `0xCD` (`lt`: `i̇̀` / `i̇́`) for
//! the lower case and `i` (`tr` / `az`: `İ`) for the upper case
//! (`ConditionalSpecialCasing.entry`; the other `lt` rules need a combining
//! mark, which Latin-1 has none of). So the language is asked only when one of
//! those bytes changes, and a language the screen cannot read declines.

use super::concat_emit::{jcc_patch, jmp_patch, patch_here};
use crate::ExecutableBuffer;

// Every pass bakes the array header: length at `+8`, payload at `+16`.
const _: () = assert!(
    cratonvm_types::ARRAY_LENGTH_OFFSET == 8 && cratonvm_types::ARRAY_DATA_OFFSET == 16,
    "the Latin-1 case passes bake ARRAY_LENGTH_OFFSET 8 / ARRAY_DATA_OFFSET 16"
);

/// `CMP EAX, imm32`.
fn cmp_eax_imm32(buf: &mut ExecutableBuffer, imm: u32) {
    buf.emit_byte(0x3D);
    buf.emit(&imm.to_le_bytes());
}

/// A backward `JMP rel32` to `top`.
fn jmp_back(buf: &mut ExecutableBuffer, top: usize) {
    let p = jmp_patch(buf);
    let rel = top as i32 - (p as i32 + 4); // Cast: rel32 displacement
    let _ = buf.try_patch_i32(p, rel);
}

/// `(low end of the ASCII letters, low end of the 31 accented letters, the
/// one sign inside that span)` of the bytes the fold changes.
fn ranges(lower: bool) -> (u8, u8, u8) {
    if lower {
        (0x41, 0xC0, 0xD7)
    } else {
        (0x61, 0xE0, 0xF7)
    }
}

/// The bytes whose fold a `tr` / `az` / `lt` language changes.
fn locale_sensitive(lower: bool) -> &'static [u8] {
    if lower {
        &[0x49, 0xCC, 0xCD]
    } else {
        &[0x69]
    }
}

/// The scan. On entry RCX = a non-null `byte[]` (payload at `+16`) and R10D =
/// its length. On the fall-through R8D bit 0 says some byte changes and bit 1
/// that a locale-sensitive one does ([`locale_sensitive`]). The upper case
/// pushes a `0xB5` / `0xDF` / `0xFF` byte onto `decline` (the JDK answers it
/// outside Latin-1). Clobbers RAX, RDX, R8, R11 and the flags; RCX and R10
/// are kept.
pub(crate) fn emit_latin1_case_scan(
    buf: &mut ExecutableBuffer,
    lower: bool,
    decline: &mut Vec<usize>,
) {
    emit_latin1_case_scan_scalar(buf, lower, false, false, decline);
}

/// [`emit_latin1_case_scan`]; with `sharp_s` (JIT round 21, lane ci, upper
/// case only) a `0xDF` sets R8D bits 0 and 2 instead of declining; with
/// `wide` (JIT round 21, lane latin1up, upper case only) a `0xB5` / `0xFF`
/// sets R8D bits 0 and 3 instead of declining.
fn emit_latin1_case_scan_scalar(
    buf: &mut ExecutableBuffer,
    lower: bool,
    sharp_s: bool,
    wide: bool,
    decline: &mut Vec<usize>,
) {
    let (lo1, lo2, sign) = ranges(lower);
    let sharp_s = sharp_s && !lower;
    let wide = wide && !lower;
    buf.emit(&[0x31, 0xD2]); // XOR EDX, EDX: i
    buf.emit(&[0x45, 0x31, 0xC0]); // XOR R8D, R8D: the flags
    let top = buf.pos();
    buf.emit(&[0x44, 0x39, 0xD2]); // CMP EDX, R10D
    let done = jcc_patch(buf, 0x83); // JAE
    buf.emit(&[0x0F, 0xB6, 0x44, 0x11, 0x10]); // MOVZX EAX, BYTE [RCX+RDX+16]
    let mut sharp_seen: Option<usize> = None;
    let mut wide_seen: Vec<usize> = Vec::new();
    if !lower {
        for out_of_latin1 in [0xB5u32, 0xDF, 0xFF] {
            if sharp_s && out_of_latin1 == 0xDF {
                continue;
            }
            if wide && out_of_latin1 != 0xDF {
                cmp_eax_imm32(buf, out_of_latin1);
                let other = jcc_patch(buf, 0x85); // JNE
                buf.emit(&[0x41, 0x83, 0xC8, 0x09]); // OR R8D, 9: changes, and a UTF16 unit
                wide_seen.push(jmp_patch(buf));
                patch_here(buf, other);
                continue;
            }
            cmp_eax_imm32(buf, out_of_latin1);
            decline.push(jcc_patch(buf, 0x84)); // JE
        }
        if sharp_s {
            cmp_eax_imm32(buf, 0xDF);
            let other = jcc_patch(buf, 0x85); // JNE
            buf.emit(&[0x41, 0x83, 0xC8, 0x05]); // OR R8D, 5: changes, and an 0xDF
            sharp_seen = Some(jmp_patch(buf));
            patch_here(buf, other);
        }
    }
    buf.emit(&[0x44, 0x8D, 0x58, lo1.wrapping_neg()]); // LEA R11D, [RAX - lo1]
    buf.emit(&[0x41, 0x83, 0xFB, 26]); // CMP R11D, 26
    let ascii = jcc_patch(buf, 0x82); // JB: a letter the fold changes
    buf.emit(&[0x44, 0x8D, 0x98]); // LEA R11D, [RAX + disp32]
    buf.emit(&(-i32::from(lo2)).to_le_bytes());
    buf.emit(&[0x41, 0x83, 0xFB, 31]); // CMP R11D, 31
    let unchanged = jcc_patch(buf, 0x83); // JAE
    cmp_eax_imm32(buf, u32::from(sign));
    let is_sign = jcc_patch(buf, 0x84); // JE: the sign stays
    patch_here(buf, ascii);
    buf.emit(&[0x41, 0x83, 0xC8, 0x01]); // OR R8D, 1
    let mut sensitive = Vec::new();
    for &b in locale_sensitive(lower) {
        cmp_eax_imm32(buf, u32::from(b));
        sensitive.push(jcc_patch(buf, 0x84)); // JE
    }
    let plain = jmp_patch(buf);
    for p in sensitive {
        patch_here(buf, p);
    }
    buf.emit(&[0x41, 0x83, 0xC8, 0x02]); // OR R8D, 2
    patch_here(buf, plain);
    patch_here(buf, unchanged);
    patch_here(buf, is_sign);
    if let Some(p) = sharp_seen {
        patch_here(buf, p);
    }
    for p in wide_seen {
        patch_here(buf, p);
    }
    buf.emit(&[0xFF, 0xC2]); // INC EDX
    jmp_back(buf, top);
    patch_here(buf, done);
}

/// The fold, in place over the PRIVATE `byte[]` in R11 (the fresh result, its
/// header length the count): lower `b | 0x20`, upper `b & !0x20`, for exactly
/// the bytes [`emit_latin1_case_scan`] counts as changing; every other byte is
/// left. No safepoint, no reference store. Clobbers RCX, RDX, R8, R10 and the
/// flags; RAX, R9 and R11 are kept.
pub(crate) fn emit_latin1_case_map_in_place(buf: &mut ExecutableBuffer, lower: bool) {
    let (lo1, lo2, sign) = ranges(lower);
    buf.emit(&[0x41, 0x8B, 0x4B, 0x08]); // MOV ECX, [R11+8]: the length
    buf.emit(&[0x31, 0xD2]); // XOR EDX, EDX
    let top = buf.pos();
    buf.emit(&[0x39, 0xCA]); // CMP EDX, ECX
    let done = jcc_patch(buf, 0x83); // JAE
    buf.emit(&[0x45, 0x0F, 0xB6, 0x44, 0x13, 0x10]); // MOVZX R8D, BYTE [R11+RDX+16]
    buf.emit(&[0x45, 0x8D, 0x50, lo1.wrapping_neg()]); // LEA R10D, [R8 - lo1]
    buf.emit(&[0x41, 0x83, 0xFA, 26]); // CMP R10D, 26
    let ascii = jcc_patch(buf, 0x82); // JB
    buf.emit(&[0x45, 0x8D, 0x90]); // LEA R10D, [R8 + disp32]
    buf.emit(&(-i32::from(lo2)).to_le_bytes());
    buf.emit(&[0x41, 0x83, 0xFA, 31]); // CMP R10D, 31
    let unchanged = jcc_patch(buf, 0x83); // JAE
    buf.emit(&[0x41, 0x81, 0xF8]); // CMP R8D, imm32
    buf.emit(&u32::from(sign).to_le_bytes());
    let is_sign = jcc_patch(buf, 0x84); // JE
    patch_here(buf, ascii);
    if lower {
        buf.emit(&[0x41, 0x83, 0xC8, 0x20]); // OR R8D, 0x20
    } else {
        buf.emit(&[0x41, 0x83, 0xE0, 0xDF]); // AND R8D, -0x21: clear bit 5
    }
    buf.emit(&[0x45, 0x88, 0x44, 0x13, 0x10]); // MOV BYTE [R11+RDX+16], R8B
    patch_here(buf, unchanged);
    patch_here(buf, is_sign);
    buf.emit(&[0xFF, 0xC2]); // INC EDX
    jmp_back(buf, top);
    patch_here(buf, done);
}

// ---- JIT round 21, lane ci (STRINGS2-21-2, `CRATONVM_JIT_IR_LATIN1_CASE_MAP_SIMD`) ----
//
// 16 bytes per SSE2 step, branch-free: the byte loops above take a
// data-dependent branch per byte, which a mixed-case word mispredicts often.
// Only XMM0 / XMM1 are scratch at a call prefix (XMM2-XMM7 are the IR
// register cache, XMM8-XMM15 the vector region), so every constant is splat
// from a GP register and the data is re-read from memory per test; an
// unsigned byte range `[lo, lo + n)` is one `PADDB` (x + 0x80 - lo, i.e.
// `(x - lo) ^ 0x80`) and one signed `PCMPGTB` against `n ^ 0x80`. A string of
// 16 or more bytes steps 16 at a time with the last step clamped back to
// `len - 16` (overlapping bytes are seen twice: the scan ORs, the fold is
// idempotent -- a folded byte lies in no range); 8..=15 bytes take one step
// over `[0, 8) ++ [len - 8, len)` (`MOVQ` + `MOVHPS`, never past the array);
// fewer than 8 take the byte loops.

/// The unsigned byte ranges `(lo, n)` whose bytes the fold changes: lower
/// `A-Z`, `0xC0..=0xD6`, `0xD8..=0xDE`; upper `a-z`, `0xE0..=0xF6`,
/// `0xF8..=0xFE` (the sign `0xD7` / `0xF7` split out).
fn simd_ranges(lower: bool) -> [(u8, u8); 3] {
    if lower {
        [(0x41, 26), (0xC0, 23), (0xD8, 7)]
    } else {
        [(0x61, 26), (0xE0, 23), (0xF8, 7)]
    }
}

/// `MOV r32, imm32` (`gp` 0 = EAX, 1 = ECX), `MOVD xmm, r32`, `PSHUFD xmm,
/// xmm, 0`: sixteen copies of `b` in `xmm` (0 or 1).
fn splat(buf: &mut ExecutableBuffer, xmm: u8, gp: u8, b: u8) {
    buf.emit_byte(0xB8 + gp); // MOV r32, imm32
    buf.emit(&u32::from_le_bytes([b; 4]).to_le_bytes());
    buf.emit(&[0x66, 0x0F, 0x6E, 0xC0 | (xmm << 3) | gp]); // MOVD xmm, r32
    buf.emit(&[0x66, 0x0F, 0x70, 0xC0 | (xmm << 3) | xmm, 0x00]); // PSHUFD xmm, xmm, 0
}

/// Where a step's 16 bytes are: `long` = `[base + RDX + 16, +16)`, else the
/// 8..=15-byte form `[base + 16, +8) ++ [base + R10 + 8, +8)`. `r11` picks
/// the base: R11 (the fold's private array) or RCX (the scan's `value`).
#[derive(Clone, Copy)]
struct Step {
    long: bool,
    r11: bool,
}

impl Step {
    fn load(self, buf: &mut ExecutableBuffer, xmm: u8) {
        let x = xmm << 3;
        match (self.long, self.r11) {
            // MOVDQU xmm, [base + RDX + 16]
            (true, false) => buf.emit(&[0xF3, 0x0F, 0x6F, 0x44 | x, 0x11, 0x10]),
            (true, true) => buf.emit(&[0xF3, 0x41, 0x0F, 0x6F, 0x44 | x, 0x13, 0x10]),
            (false, false) => {
                buf.emit(&[0xF3, 0x0F, 0x7E, 0x41 | x, 0x10]); // MOVQ xmm, [RCX+16]
                buf.emit(&[0x42, 0x0F, 0x16, 0x44 | x, 0x11, 0x08]); // MOVHPS xmm, [RCX+R10+8]
            }
            (false, true) => {
                buf.emit(&[0xF3, 0x41, 0x0F, 0x7E, 0x43 | x, 0x10]); // MOVQ xmm, [R11+16]
                buf.emit(&[0x43, 0x0F, 0x16, 0x44 | x, 0x13, 0x08]); // MOVHPS xmm, [R11+R10+8]
            }
        }
    }

    /// The fold's write-back (R11 only).
    fn store(self, buf: &mut ExecutableBuffer, xmm: u8) {
        let x = xmm << 3;
        if self.long {
            // MOVDQU [R11 + RDX + 16], xmm
            buf.emit(&[0xF3, 0x41, 0x0F, 0x7F, 0x44 | x, 0x13, 0x10]);
        } else {
            buf.emit(&[0x66, 0x41, 0x0F, 0xD6, 0x43 | x, 0x10]); // MOVQ [R11+16], xmm
            buf.emit(&[0x43, 0x0F, 0x17, 0x44 | x, 0x13, 0x08]); // MOVHPS [R11+R10+8], xmm
        }
    }
}

/// XMM0 = the in-range mask (0xFF / 0x00 per byte) of `[lo, lo + n)` over the
/// step's bytes. Splats through `gp`. Clobbers XMM1.
fn emit_range_mask(buf: &mut ExecutableBuffer, step: Step, gp: u8, lo: u8, n: u8) {
    step.load(buf, 0);
    splat(buf, 1, gp, 0x80u8.wrapping_sub(lo));
    buf.emit(&[0x66, 0x0F, 0xFC, 0xC8]); // PADDB XMM1, XMM0: (x - lo) ^ 0x80
    splat(buf, 0, gp, n ^ 0x80);
    buf.emit(&[0x66, 0x0F, 0x64, 0xC1]); // PCMPGTB XMM0, XMM1: (n ^ 0x80) > that
}

/// One scan step: OR the step's 16-bit `PMOVMSKB` masks into R8 -- bits 0-15
/// a byte the fold changes, 16-31 a locale-sensitive one, 32-47 a byte the
/// prefix declines (upper `0xB5` / `0xFF`, and `0xDF` without `sharp_s`),
/// 48-63 an `0xDF` (`sharp_s`). Clobbers RAX, XMM0, XMM1.
fn emit_scan_step(buf: &mut ExecutableBuffer, step: Step, lower: bool, sharp_s: bool) {
    fn or_into_r8(buf: &mut ExecutableBuffer, shift: u8) {
        // RAX = the zero-extended 16-bit mask (`PMOVMSKB` writes EAX).
        if shift != 0 {
            buf.emit(&[0x48, 0xC1, 0xE0, shift]); // SHL RAX, shift
        }
        // 64-bit: a 32-bit OR would clear R8's upper half.
        buf.emit(&[0x49, 0x09, 0xC0]); // OR R8, RAX
    }
    fn eq(buf: &mut ExecutableBuffer, step: Step, v: u8, shift: u8) {
        step.load(buf, 0);
        splat(buf, 1, 0, v);
        buf.emit(&[0x66, 0x0F, 0x74, 0xC8]); // PCMPEQB XMM1, XMM0
        buf.emit(&[0x66, 0x0F, 0xD7, 0xC1]); // PMOVMSKB EAX, XMM1
        or_into_r8(buf, shift);
    }
    for (lo, n) in simd_ranges(lower) {
        emit_range_mask(buf, step, 0, lo, n);
        buf.emit(&[0x66, 0x0F, 0xD7, 0xC0]); // PMOVMSKB EAX, XMM0
        or_into_r8(buf, 0);
    }
    if lower {
        eq(buf, step, 0x49, 16);
        emit_range_mask(buf, step, 0, 0xCC, 2);
        buf.emit(&[0x66, 0x0F, 0xD7, 0xC0]); // PMOVMSKB EAX, XMM0
        or_into_r8(buf, 16);
    } else {
        eq(buf, step, 0x69, 16);
        eq(buf, step, 0xB5, 32);
        eq(buf, step, 0xFF, 32);
        eq(buf, step, 0xDF, if sharp_s { 48 } else { 32 });
    }
}

/// [`emit_latin1_case_scan`] with `simd` (16 bytes per step, see above) and
/// `sharp_s` (upper only: an `0xDF` sets R8D bit 2 -- and bit 0 -- instead of
/// declining). Same entry (RCX = a non-null `byte[]`, R10D = its length),
/// the same R8D bits 0 / 1 on the fall-through; clobbers RAX, RDX, R8, R11,
/// XMM0, XMM1 and the flags; keeps RCX, R9, R10.
pub(crate) fn emit_latin1_case_scan_with(
    buf: &mut ExecutableBuffer,
    lower: bool,
    simd: bool,
    sharp_s: bool,
    decline: &mut Vec<usize>,
) {
    emit_latin1_case_scan_wide(buf, lower, simd, sharp_s, false, decline);
}

/// [`emit_latin1_case_scan_with`]; with `wide` (JIT round 21, lane latin1up,
/// `CRATONVM_JIT_IR_LATIN1_UPPER_UTF16`, upper case only) a `0xB5` / `0xFF`
/// -- whose `toUpperCaseEx` (`0x39C` / `0x178`) leaves Latin-1 -- sets R8D
/// bit 3 (and bit 0) instead of declining. `wide` scans as `sharp_s` does
/// too (an `0xDF` sets bit 2, never declines): a caller without the `0xDF`
/// arm declines bit 2 itself. Same entry, exits and clobbers.
pub(crate) fn emit_latin1_case_scan_wide(
    buf: &mut ExecutableBuffer,
    lower: bool,
    simd: bool,
    sharp_s: bool,
    wide: bool,
    decline: &mut Vec<usize>,
) {
    let sharp_s = (sharp_s || wide) && !lower;
    let wide = wide && !lower;
    if !simd {
        if sharp_s || wide {
            emit_latin1_case_scan_scalar(buf, lower, sharp_s, wide, decline);
        } else {
            emit_latin1_case_scan(buf, lower, decline);
        }
        return;
    }
    buf.emit(&[0x45, 0x31, 0xC0]); // XOR R8D, R8D: the masks
    buf.emit(&[0x41, 0x83, 0xFA, 0x10]); // CMP R10D, 16
    let short = jcc_patch(buf, 0x82); // JB
    buf.emit(&[0x31, 0xD2]); // XOR EDX, EDX: i
    let top = buf.pos();
    buf.emit(&[0x45, 0x8D, 0x5A, 0xF0]); // LEA R11D, [R10-16]
    buf.emit(&[0x44, 0x39, 0xDA]); // CMP EDX, R11D
    buf.emit(&[0x41, 0x0F, 0x47, 0xD3]); // CMOVA EDX, R11D: the last step ends at len
    emit_scan_step(
        buf,
        Step {
            long: true,
            r11: false,
        },
        lower,
        sharp_s,
    );
    buf.emit(&[0x83, 0xC2, 0x10]); // ADD EDX, 16
    buf.emit(&[0x44, 0x39, 0xD2]); // CMP EDX, R10D
    let again = jcc_patch(buf, 0x82); // JB
    let _ = buf.try_patch_i32(again, top as i32 - (again as i32 + 4)); // Cast: rel32
    let long_done = jmp_patch(buf);
    patch_here(buf, short);
    buf.emit(&[0x41, 0x83, 0xFA, 0x08]); // CMP R10D, 8
    let bytes = jcc_patch(buf, 0x82); // JB: the byte loop
    emit_scan_step(
        buf,
        Step {
            long: false,
            r11: false,
        },
        lower,
        sharp_s,
    );
    patch_here(buf, long_done);
    // The masks -> the byte loop's R8D bits (0 changes, 1 sensitive, 2 0xDF,
    // 3 a UTF16 unit).
    if wide {
        // Bits 32-47 hold only `0xB5` / `0xFF` here: `wide` scans with
        // `sharp_s` (the caller's contract), so `0xDF` sits in bits 48-63.
        buf.emit(&[0x45, 0x31, 0xDB]); // XOR R11D, R11D
        buf.emit(&[0x4C, 0x89, 0xC0]); // MOV RAX, R8
        buf.emit(&[0x48, 0xC1, 0xE8, 0x20]); // SHR RAX, 32
        buf.emit(&[0x66, 0x85, 0xC0]); // TEST AX, AX
        let narrow = jcc_patch(buf, 0x84); // JZ: no 0xB5 / 0xFF
        buf.emit(&[0x41, 0x83, 0xCB, 0x09]); // OR R11D, 9
        patch_here(buf, narrow);
    } else if !lower {
        buf.emit(&[0x4C, 0x89, 0xC0]); // MOV RAX, R8
        buf.emit(&[0x48, 0xC1, 0xE8, 0x20]); // SHR RAX, 32
        buf.emit(&[0x66, 0x85, 0xC0]); // TEST AX, AX
        decline.push(jcc_patch(buf, 0x85)); // JNZ: 0xB5 / 0xFF (/ 0xDF)
    }
    buf.emit(&[0x4C, 0x89, 0xC2]); // MOV RDX, R8
    buf.emit(&[0x48, 0xC1, 0xEA, 0x30]); // SHR RDX, 48: the 0xDF mask
    buf.emit(&[0x44, 0x89, 0xC0]); // MOV EAX, R8D
    buf.emit(&[0xC1, 0xE8, 0x10]); // SHR EAX, 16: the sensitive mask
    buf.emit(&[0x45, 0x0F, 0xB7, 0xC0]); // MOVZX R8D, R8W: the change mask
    if !wide {
        buf.emit(&[0x45, 0x31, 0xDB]); // XOR R11D, R11D
    }
    for (test, bits) in [
        ([0x45u8, 0x85, 0xC0], 1u8),
        ([0x40, 0x85, 0xC0], 2),
        ([0x40, 0x85, 0xD2], 5),
    ] {
        buf.emit(&test); // TEST R8D,R8D / TEST EAX,EAX / TEST EDX,EDX
        let none = jcc_patch(buf, 0x84); // JZ
        buf.emit(&[0x41, 0x83, 0xCB, bits]); // OR R11D, bits
        patch_here(buf, none);
    }
    buf.emit(&[0x45, 0x89, 0xD8]); // MOV R8D, R11D
    let done = jmp_patch(buf);
    patch_here(buf, bytes);
    emit_latin1_case_scan_scalar(buf, lower, sharp_s, wide, decline);
    patch_here(buf, done);
}

/// [`emit_latin1_case_map_in_place`] with `simd`: per range, the step's bytes
/// XOR `0x20` where in range, written back (ranges are disjoint from every
/// folded byte, so range by range in memory is the whole fold). Same entry
/// (R11 = the private `byte[]`); clobbers RCX, RDX, R8, R10, XMM0, XMM1 and
/// the flags; keeps RAX, R9, R11.
pub(crate) fn emit_latin1_case_map_in_place_with(
    buf: &mut ExecutableBuffer,
    lower: bool,
    simd: bool,
) {
    if !simd {
        emit_latin1_case_map_in_place(buf, lower);
        return;
    }
    fn step_fold(buf: &mut ExecutableBuffer, step: Step, lower: bool) {
        for (lo, n) in simd_ranges(lower) {
            emit_range_mask(buf, step, 1, lo, n);
            splat(buf, 1, 1, 0x20);
            buf.emit(&[0x66, 0x0F, 0xDB, 0xC1]); // PAND XMM0, XMM1
            step.load(buf, 1);
            buf.emit(&[0x66, 0x0F, 0xEF, 0xC8]); // PXOR XMM1, XMM0
            step.store(buf, 1);
        }
    }
    buf.emit(&[0x45, 0x8B, 0x53, 0x08]); // MOV R10D, [R11+8]: the length
    buf.emit(&[0x41, 0x83, 0xFA, 0x10]); // CMP R10D, 16
    let short = jcc_patch(buf, 0x82); // JB
    buf.emit(&[0x31, 0xD2]); // XOR EDX, EDX
    let top = buf.pos();
    buf.emit(&[0x45, 0x8D, 0x42, 0xF0]); // LEA R8D, [R10-16]
    buf.emit(&[0x44, 0x39, 0xC2]); // CMP EDX, R8D
    buf.emit(&[0x41, 0x0F, 0x47, 0xD0]); // CMOVA EDX, R8D
    step_fold(
        buf,
        Step {
            long: true,
            r11: true,
        },
        lower,
    );
    buf.emit(&[0x83, 0xC2, 0x10]); // ADD EDX, 16
    buf.emit(&[0x44, 0x39, 0xD2]); // CMP EDX, R10D
    let again = jcc_patch(buf, 0x82); // JB
    let _ = buf.try_patch_i32(again, top as i32 - (again as i32 + 4)); // Cast: rel32
    let long_done = jmp_patch(buf);
    patch_here(buf, short);
    buf.emit(&[0x41, 0x83, 0xFA, 0x08]); // CMP R10D, 8
    let bytes = jcc_patch(buf, 0x82); // JB
    step_fold(
        buf,
        Step {
            long: false,
            r11: true,
        },
        lower,
    );
    let done = jmp_patch(buf);
    patch_here(buf, bytes);
    emit_latin1_case_map_in_place(buf, lower);
    patch_here(buf, done);
    patch_here(buf, long_done);
}

/// JIT round 21, lane ci (`CRATONVM_JIT_IR_LATIN1_UPPER_SHARP_S`): EAX = the
/// number of `0xDF` bytes of the `byte[]` in RCX (length R10D). Clobbers RAX,
/// RDX and the flags.
pub(crate) fn emit_count_sharp_s(buf: &mut ExecutableBuffer) {
    buf.emit(&[0x31, 0xC0]); // XOR EAX, EAX
    buf.emit(&[0x31, 0xD2]); // XOR EDX, EDX
    let top = buf.pos();
    buf.emit(&[0x44, 0x39, 0xD2]); // CMP EDX, R10D
    let done = jcc_patch(buf, 0x83); // JAE
    buf.emit(&[0x80, 0x7C, 0x11, 0x10, 0xDF]); // CMP BYTE [RCX+RDX+16], 0xDF
    let other = jcc_patch(buf, 0x85); // JNE
    buf.emit(&[0xFF, 0xC0]); // INC EAX
    patch_here(buf, other);
    buf.emit(&[0xFF, 0xC2]); // INC EDX
    jmp_back(buf, top);
    patch_here(buf, done);
}

/// JIT round 21, lane ci: copy the source `byte[]` in RDX into the PRIVATE
/// `byte[]` in R11 (`value.length + count(0xDF)` bytes), every `0xDF` as
/// `"SS"` -- `toUpperCaseEx`'s `toUpperCaseCharArray(0xDF)`; the upper fold in
/// place then maps every other byte (and leaves `S`). Clobbers RCX, R8, R9,
/// R10 and the flags; keeps RAX, RDX, R11.
pub(crate) fn emit_copy_expanding_sharp_s(buf: &mut ExecutableBuffer) {
    buf.emit(&[0x8B, 0x4A, 0x08]); // MOV ECX, [RDX+8]: the source length
    buf.emit(&[0x45, 0x31, 0xC0]); // XOR R8D, R8D: i
    buf.emit(&[0x45, 0x31, 0xD2]); // XOR R10D, R10D: j
    let top = buf.pos();
    buf.emit(&[0x41, 0x39, 0xC8]); // CMP R8D, ECX
    let done = jcc_patch(buf, 0x83); // JAE
    buf.emit(&[0x46, 0x0F, 0xB6, 0x4C, 0x02, 0x10]); // MOVZX R9D, BYTE [RDX+R8+16]
    buf.emit(&[0x41, 0xFF, 0xC0]); // INC R8D
    buf.emit(&[0x41, 0x81, 0xF9]); // CMP R9D, imm32
    buf.emit(&0xDFu32.to_le_bytes());
    let plain = jcc_patch(buf, 0x85); // JNE
    buf.emit(&[0x66, 0x43, 0xC7, 0x44, 0x13, 0x10, b'S', b'S']); // MOV WORD [R11+R10+16], "SS"
    buf.emit(&[0x41, 0x83, 0xC2, 0x02]); // ADD R10D, 2
    jmp_back(buf, top);
    patch_here(buf, plain);
    buf.emit(&[0x47, 0x88, 0x4C, 0x13, 0x10]); // MOV BYTE [R11+R10+16], R9B
    buf.emit(&[0x41, 0xFF, 0xC2]); // INC R10D
    jmp_back(buf, top);
    patch_here(buf, done);
}

/// JIT round 21, lane latin1up (`CRATONVM_JIT_IR_LATIN1_UPPER_UTF16`): the
/// whole UTF16 payload of `StringLatin1.toUpperCase`'s `toUpperCaseEx(..,
/// false)` + `StringUTF16.newString` for a source holding `0xB5` / `0xFF`
/// (so nothing compresses it back): every source byte of the `byte[]` in RDX
/// becomes one little-endian char of the PRIVATE `byte[]` in R11 (`2 *
/// (value.length + count(0xDF))` bytes) -- `CharacterDataLatin1.toUpperCaseEx`:
/// `a-z` and `0xE0..=0xFE` but `0xF7` minus `0x20`, `0xB5` -> `0x39C`, `0xFF`
/// -> `0x178`, every other byte itself -- and an `0xDF` two, "SS"
/// (`toUpperCaseCharArray`). The prefix of unchanged bytes the JDK copies
/// with `putChar` is the same map. Clobbers RCX, R8, R9, R10 and the flags;
/// keeps RAX, RDX, R11.
pub(crate) fn emit_copy_upper_utf16(buf: &mut ExecutableBuffer) {
    buf.emit(&[0x45, 0x31, 0xC0]); // XOR R8D, R8D: i
    buf.emit(&[0x45, 0x31, 0xD2]); // XOR R10D, R10D: the byte offset written
    let top = buf.pos();
    buf.emit(&[0x44, 0x3B, 0x42, 0x08]); // CMP R8D, [RDX+8]: the source length
    let done = jcc_patch(buf, 0x83); // JAE
    buf.emit(&[0x46, 0x0F, 0xB6, 0x4C, 0x02, 0x10]); // MOVZX R9D, BYTE [RDX+R8+16]
    buf.emit(&[0x41, 0xFF, 0xC0]); // INC R8D
    buf.emit(&[0x41, 0x81, 0xF9]); // CMP R9D, imm32
    buf.emit(&0xDFu32.to_le_bytes());
    let not_sharp = jcc_patch(buf, 0x85); // JNE
    let ss = [b'S', 0, b'S', 0]; // two UTF16LE 'S' units
    buf.emit(&[0x43, 0xC7, 0x44, 0x13, 0x10]); // MOV DWORD [R11+R10+16], imm32
    buf.emit(&ss);
    buf.emit(&[0x41, 0x83, 0xC2, 0x04]); // ADD R10D, 4
    jmp_back(buf, top);
    patch_here(buf, not_sharp);
    let mut store = Vec::new();
    for (byte, unit) in [(0xB5u32, 0x39Cu32), (0xFF, 0x178)] {
        buf.emit(&[0x41, 0x81, 0xF9]); // CMP R9D, imm32
        buf.emit(&byte.to_le_bytes());
        let other = jcc_patch(buf, 0x85); // JNE
        buf.emit(&[0x41, 0xB9]); // MOV R9D, imm32
        buf.emit(&unit.to_le_bytes());
        store.push(jmp_patch(buf));
        patch_here(buf, other);
    }
    buf.emit(&[0x41, 0x8D, 0x49, 0x9F]); // LEA ECX, [R9 - 0x61]
    buf.emit(&[0x83, 0xF9, 26]); // CMP ECX, 26
    let ascii = jcc_patch(buf, 0x82); // JB: a-z
    buf.emit(&[0x41, 0x8D, 0x89]); // LEA ECX, [R9 + disp32]
    buf.emit(&(-0xE0i32).to_le_bytes());
    buf.emit(&[0x83, 0xF9, 31]); // CMP ECX, 31
    store.push(jcc_patch(buf, 0x83)); // JAE: unchanged
    buf.emit(&[0x41, 0x81, 0xF9]); // CMP R9D, imm32
    buf.emit(&0xF7u32.to_le_bytes());
    store.push(jcc_patch(buf, 0x84)); // JE: the sign stays
    patch_here(buf, ascii);
    buf.emit(&[0x41, 0x83, 0xE9, 0x20]); // SUB R9D, 0x20
    for p in store {
        patch_here(buf, p);
    }
    buf.emit(&[0x66, 0x47, 0x89, 0x4C, 0x13, 0x10]); // MOV WORD [R11+R10+16], R9W
    buf.emit(&[0x41, 0x83, 0xC2, 0x02]); // ADD R10D, 2
    jmp_back(buf, top);
    patch_here(buf, done);
}

/// The language screen: on entry R8 = a non-null `java/util/Locale`. Falls
/// through when `locale.getLanguage()` is provably none of `"tr"`, `"az"`,
/// `"lt"` (a language whose payload is not two bytes, or two other bytes);
/// pushes onto `decline` when it is one of them -- compared by content, a
/// superset of the JDK's identity test, so a non-interned twin only costs the
/// call -- and whenever a hop cannot be read: a LEGACY `Locale` / `BaseLocale`,
/// a class id other than the layout's (both classes are final), a null
/// `baseLocale` / `language` / `value`, a non-String language. The language
/// String is read by either layout, as `emit_str_value_and_coder` reads one.
/// Clobbers RAX, R8 and the flags.
pub(crate) fn emit_locale_language_screen(
    buf: &mut ExecutableBuffer,
    loc: &crate::LocaleLanguageLayout,
    string: &crate::StringFieldLayout,
    decline: &mut Vec<usize>,
) {
    fn class_is(buf: &mut ExecutableBuffer, decline: &mut Vec<usize>, class_id: u32) {
        buf.emit(&[0x41, 0x81, 0x78, 0x00]); // CMP DWORD [R8], imm32
        buf.emit(&class_id.to_le_bytes());
        decline.push(jcc_patch(buf, 0x85)); // JNE
    }
    fn test_compact(buf: &mut ExecutableBuffer) {
        const _: () = assert!(cratonvm_types::GC_FLAGS_BYTE_OFFSET <= 127);
        // Cast: x86-64 disp8 (7).
        let flags_d8 = cratonvm_types::GC_FLAGS_BYTE_OFFSET as u8;
        // TEST BYTE [R8+flags], COMPACT
        buf.emit(&[0x41, 0xF6, 0x40, flags_d8, cratonvm_types::GC_FLAG_COMPACT]);
    }
    fn load_r8(buf: &mut ExecutableBuffer, disp: i32) {
        buf.emit(&[0x4D, 0x8B, 0x80]); // MOV R8, [R8 + disp32]
        buf.emit(&disp.to_le_bytes());
    }
    fn non_null(buf: &mut ExecutableBuffer, decline: &mut Vec<usize>) {
        buf.emit(&[0x4D, 0x85, 0xC0]); // TEST R8, R8
        decline.push(jcc_patch(buf, 0x84)); // JZ
    }
    // Locale -> BaseLocale -> language: COMPACT instances of the exact class.
    for (class_id, field) in [
        (loc.locale_class_id, loc.base_locale_offset),
        (loc.base_locale_class_id, loc.language_offset),
    ] {
        test_compact(buf);
        decline.push(jcc_patch(buf, 0x84)); // JZ: legacy
        class_is(buf, decline, class_id);
        load_r8(buf, field);
        non_null(buf, decline);
    }
    // language.value, by either String layout.
    class_is(buf, decline, string.string_class_id);
    test_compact(buf);
    let legacy = jcc_patch(buf, 0x84); // JZ
    load_r8(buf, string.value_compact_offset);
    let have = jmp_patch(buf);
    patch_here(buf, legacy);
    load_r8(buf, string.value_legacy_offset);
    patch_here(buf, have);
    non_null(buf, decline);
    // t1 wave 8 (RL-STR review): a 4-byte payload may be a UTF16-coded
    // two-letter language (the screen does not read the coder); no real
    // language is four Latin-1 letters long, so it declines.
    buf.emit(&[0x41, 0x83, 0x78, 0x08, 0x04]); // CMP DWORD [R8+8], 4
    decline.push(jcc_patch(buf, 0x84)); // JE
    buf.emit(&[0x41, 0x83, 0x78, 0x08, 0x02]); // CMP DWORD [R8+8], 2
    let not_two = jcc_patch(buf, 0x85); // JNE: no two-letter language
    buf.emit(&[0x41, 0x0F, 0xB7, 0x40, 0x10]); // MOVZX EAX, WORD [R8+16]
    for lang in [*b"tr", *b"az", *b"lt"] {
        cmp_eax_imm32(buf, u32::from(u16::from_le_bytes(lang)));
        decline.push(jcc_patch(buf, 0x84)); // JE    
}
    patch_here(buf, not_two);}

#[cfg(all(test, target_arch = "x86_64"))]
mod jr21_strings2_latin1_case_emit_tests;

#[cfg(all(test, target_arch = "x86_64"))]
mod jr21_ci_latin1_case_simd_tests;

#[cfg(all(test, target_arch = "x86_64"))]
mod jr21_latin1up_tests;
