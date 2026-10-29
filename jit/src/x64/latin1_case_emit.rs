// SPDX-License-Identifier: Apache-2.0
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
    patch_here(buf, not_two);
}

#[cfg(all(test, target_arch = "x86_64"))]
mod jr21_strings2_latin1_case_emit_tests {
    use super::*;

    /// `MOV <dst>, <first argument>` for this platform's C ABI.
    fn mov_from_a0(dst_rex_b: bool, dst_low3: u8) -> Vec<u8> {
        // MOV r/m64 (dst), r64 (src): REX.W(+B) 89 /r; src RCX (Win64) / RDI.
        #[cfg(windows)]
        let src = 1u8;
        #[cfg(not(windows))]
        let src = 7u8;
        let modrm = 0xC0 | (src << 3) | dst_low3;
        vec![0x48 | u8::from(dst_rex_b), 0x89, modrm]
    }

    /// Emit `prologue`, `body`, `epilogue` + `RET` as `extern "C" fn(a0) -> i64`,
    /// every `decline` edge answering `-1`, and run it.
    fn run(
        prologue: &[u8],
        body: impl Fn(&mut ExecutableBuffer, &mut Vec<usize>),
        epilogue: &[u8],
        a0: u64,
    ) -> i32 {
        let mut buf = ExecutableBuffer::new(1 << 16).expect("test buffer");
        buf.emit(prologue);
        let mut decline = Vec::new();
        body(&mut buf, &mut decline);
        buf.emit(epilogue);
        buf.emit_byte(0xC3); // RET
        for p in decline {
            patch_here(&mut buf, p);
        }
        buf.emit_byte(0xB8); // MOV EAX, -1
        buf.emit(&(-1i32).to_le_bytes());
        buf.emit_byte(0xC3);
        assert!(!buf.overflowed());
        crate::platform::make_executable(buf.as_ptr() as *mut u8, buf.capacity())
            .expect("the test buffer must be flippable to RX");
        // SAFETY: a leaf snippet over the caller's buffers (alive across the
        // call) touching caller-saved registers only.
        let f: extern "C" fn(u64, u64) -> i64 = unsafe { std::mem::transmute(buf.as_ptr()) };
        f(a0, 0) as i32 // Cast: the EAX answer
    }

    fn words(raw: &[u8]) -> Vec<u64> {
        let mut w = vec![0u64; raw.len().div_ceil(8)];
        for (i, b) in raw.iter().enumerate() {
            w[i / 8] |= u64::from(*b) << ((i % 8) * 8);
        }
        w
    }

    fn bytes_of(w: &[u64]) -> Vec<u8> {
        w.iter().flat_map(|x| x.to_le_bytes()).collect()
    }

    /// A fake `byte[]` holding `bytes`.
    fn byte_array(bytes: &[u8]) -> Vec<u64> {
        let data = cratonvm_types::ARRAY_DATA_OFFSET;
        let mut raw = vec![0u8; data + bytes.len() + 8];
        raw[cratonvm_types::KIND_TAGS_BYTE_OFFSET] =
            cratonvm_types::primitive_array_kind_tags_byte("[B").expect("byte[] tag");
        let len_at = cratonvm_types::ARRAY_LENGTH_OFFSET;
        let len = u32::try_from(bytes.len()).expect("small");
        raw[len_at..len_at + 4].copy_from_slice(&len.to_le_bytes());
        raw[data..data + bytes.len()].copy_from_slice(bytes);
        words(&raw)
    }

    fn payload(a: &[u64], n: usize) -> Vec<u8> {
        let data = cratonvm_types::ARRAY_DATA_OFFSET;
        bytes_of(a)[data..data + n].to_vec()
    }

    // ---- the JDK 25 bodies, written out from `jdk25src` ----------------------

    /// `CharacterDataLatin1.toLowerCase(int)`.
    fn jdk_to_lower(ch: u32) -> u32 {
        if ch < u32::from(b'A') {
            return ch;
        }
        let lower = ch | 0x20;
        if lower <= u32::from(b'z') || ((0xE0..=0xFE).contains(&lower) && lower != 0xF7) {
            return lower;
        }
        ch
    }

    /// `CharacterDataLatin1.isUpperCase(int)` over Latin-1 (the `B` table's
    /// bit 2: `Lu`, `A-Z` and `0xC0..=0xDE` except `0xD7`).
    fn jdk_is_upper(ch: u32) -> bool {
        (0x41..=0x5A).contains(&ch) || ((0xC0..=0xDE).contains(&ch) && ch != 0xD7)
    }

    /// `CharacterDataLatin1.toUpperCase(int)` (== `toUpperCaseEx` on every byte
    /// but `0xDF`, which `toUpperCaseEx` sends to `Character.ERROR`).
    fn jdk_to_upper(ch: u32) -> u32 {
        if ch < u32::from(b'a') {
            return ch;
        }
        let upper = ch & 0xDF;
        if upper <= u32::from(b'Z') || ((0xC0..=0xDE).contains(&upper) && upper != 0xD7) {
            return upper;
        }
        match ch {
            0xFF => 0x178,
            0xB5 => 0x39C,
            _ => ch,
        }
    }

    /// `StringLatin1.toUpperCase`'s `notUpperCaseEx` scan predicate.
    fn jdk_not_upper_case_ex(cp: u32) -> bool {
        cp >= u32::from(b'a') && (cp <= u32::from(b'z') || cp == 0xB5 || (cp >= 0xDF && cp != 0xF7))
    }

    fn scan(lower: bool, bytes: &[u8]) -> i32 {
        let a = byte_array(bytes);
        let mut pro = mov_from_a0(false, 1); // MOV RCX, a0
        pro.extend_from_slice(&[0x44, 0x8B, 0x51, 0x08]); // MOV R10D, [RCX+8]
        run(
            &pro,
            |buf, decline| emit_latin1_case_scan(buf, lower, decline),
            &[0x44, 0x89, 0xC0], // MOV EAX, R8D
            a.as_ptr() as u64,   // Cast: object address as a word
        )
    }

    /// Every Latin-1 byte, alone and between unchanged neighbours, against the
    /// JDK predicates: bit 0 is "the JDK copies a changed byte" (lower: the
    /// `isUpperCase` scan, which equals "`toLowerCase` changes it"), bit 1 the
    /// locale-sensitive bytes, and the upper case's three out-of-Latin-1 bytes
    /// decline.
    #[test]
    fn the_scan_finds_exactly_the_bytes_the_jdk_changes() {
        for b in 0..=255u8 {
            let ch = u32::from(b);
            // The two definitions of "changes" the JDK body mixes agree.
            assert_eq!(jdk_is_upper(ch), jdk_to_lower(ch) != ch, "lower {b:#x}");
            let lower_want =
                i32::from(jdk_is_upper(ch)) | (i32::from(matches!(b, 0x49 | 0xCC | 0xCD)) << 1);
            let long: Vec<u8> = vec![b'q'; 9].into_iter().chain([b]).collect();
            for text in [vec![b], vec![b'1', b, b'.'], long] {
                assert_eq!(scan(true, &text), lower_want, "lower scan of {b:#x}");
            }
            let upper_want = if matches!(b, 0xB5 | 0xDF | 0xFF) {
                -1
            } else {
                assert_eq!(
                    jdk_not_upper_case_ex(ch),
                    jdk_to_upper(ch) != ch,
                    "upper {b:#x}"
                );
                i32::from(jdk_not_upper_case_ex(ch)) | (i32::from(b == b'i') << 1)
            };
            assert_eq!(
                scan(false, &[b'1', b, b'.']),
                upper_want,
                "upper scan of {b:#x}"
            );
        }
        assert_eq!(scan(true, b""), 0);
        assert_eq!(scan(true, b"content-type: \xe0\xff"), 0);
        assert_eq!(scan(true, b"Content-Type"), 1);
        assert_eq!(scan(true, b"content-Iype"), 3);
        assert_eq!(scan(false, b"CONTENT-TYPE \xc0\xd7"), 0);
        assert_eq!(scan(false, b"CONTENT-tYPE"), 1);
        assert_eq!(scan(false, b"CONTENT-TYPi"), 3);
        assert_eq!(scan(false, b"CONTENT-TYPE\xff"), -1);
    }

    /// The in-place fold over all 256 bytes (the upper case without its three
    /// declined bytes) against `CharacterDataLatin1`.
    #[test]
    fn the_fold_in_place_is_the_jdk_fold() {
        let all: Vec<u8> = (0..=255u8).collect();
        for lower in [true, false] {
            let text: Vec<u8> = all
                .iter()
                .copied()
                .filter(|b| lower || !matches!(b, 0xB5 | 0xDF | 0xFF))
                .collect();
            let a = byte_array(&text);
            let pro = mov_from_a0(true, 3); // MOV R11, a0
            let r = run(
                &pro,
                |buf, _| emit_latin1_case_map_in_place(buf, lower),
                &[0xB8, 7, 0, 0, 0], // MOV EAX, 7
                a.as_ptr() as u64,   // Cast: object address as a word
            );
            assert_eq!(r, 7);
            let got = payload(&a, text.len());
            for (i, &b) in text.iter().enumerate() {
                let want = if lower {
                    jdk_to_lower(u32::from(b))
                } else {
                    jdk_to_upper(u32::from(b))
                };
                assert_eq!(u32::from(got[i]), want, "lower={lower} byte {b:#x}");
            }
        }
    }

    const LOCALE_ID: u32 = 0x0007_1001;
    const BASE_ID: u32 = 0x0007_1002;
    const STRING_ID: u32 = 0x0007_1003;

    fn loc_layout() -> crate::LocaleLanguageLayout {
        crate::LocaleLanguageLayout {
            locale_class_id: LOCALE_ID,
            base_locale_offset: 16,
            base_locale_class_id: BASE_ID,
            language_offset: 24,
        }
    }

    fn string_layout() -> crate::StringFieldLayout {
        crate::StringFieldLayout {
            value_compact_offset: 16,
            value_legacy_offset: 40,
            string_class_id: STRING_ID,
            ..crate::tests::synthetic_string_layout(STRING_ID)
        }
    }

    /// A fake object: class id, compact flag, one reference at `at`.
    fn obj(class_id: u32, compact: bool, at: usize, refr: u64) -> Vec<u64> {
        let mut raw = vec![0u8; 64];
        raw[0..4].copy_from_slice(&class_id.to_le_bytes());
        if compact {
            raw[cratonvm_types::GC_FLAGS_BYTE_OFFSET] = cratonvm_types::GC_FLAG_COMPACT;
        }
        raw[at..at + 8].copy_from_slice(&refr.to_le_bytes());
        words(&raw)
    }

    /// `screen(Locale(BaseLocale(String(byte[lang]))))`: 1 = provably plain,
    /// -1 = declined.
    fn screen(lang: &[u8], string_compact: bool, locale_compact: bool, base_null: bool) -> i32 {
        let value = byte_array(lang);
        let s = obj(
            STRING_ID,
            string_compact,
            if string_compact { 16 } else { 40 },
            value.as_ptr() as u64,
        ); // Cast: address
        let base = obj(BASE_ID, true, 24, s.as_ptr() as u64); // Cast: address
        let base_ref = if base_null { 0 } else { base.as_ptr() as u64 }; // Cast: address
        let locale = obj(LOCALE_ID, locale_compact, 16, base_ref);
        let pro = mov_from_a0(true, 0); // MOV R8, a0
        let (l, st) = (loc_layout(), string_layout());
        run(
            &pro,
            |buf, decline| emit_locale_language_screen(buf, &l, &st, decline),
            &[0xB8, 1, 0, 0, 0],    // MOV EAX, 1
            locale.as_ptr() as u64, // Cast: object address as a word
        )
    }

    /// The three locale-dependent languages decline by either String layout;
    /// any other language (`""` is `Locale.ROOT`'s) falls through; an
    /// unreadable hop declines.
    #[test]
    fn the_language_screen_declines_tr_az_lt_and_every_unreadable_hop() {
        for compact in [true, false] {
            for lang in [&b"tr"[..], b"az", b"lt"] {
                assert_eq!(screen(lang, compact, true, false), -1, "{lang:?}");
            }
            for lang in [&b""[..], b"en", b"de", b"t", b"trr", b"rt", b"za", b"tl"] {
                assert_eq!(screen(lang, compact, true, false), 1, "{lang:?}");
            }
            // t1 wave 8: a 4-byte payload (a UTF16-coded "tr") declines.
            assert_eq!(screen(b"t\0r\0", compact, true, false), -1, "UTF16 tr");
        }
        assert_eq!(screen(b"en", true, false, false), -1, "a legacy Locale");
        assert_eq!(screen(b"en", true, true, true), -1, "a null baseLocale");
        // A foreign class at the first hop.
        let other = obj(0x0007_1009, true, 16, 0);
        let (l, st) = (loc_layout(), string_layout());
        let r = run(
            &mov_from_a0(true, 0),
            |buf, decline| emit_locale_language_screen(buf, &l, &st, decline),
            &[0xB8, 1, 0, 0, 0],
            other.as_ptr() as u64, // Cast: object address as a word
        );
        assert_eq!(r, -1, "not a Locale");
    }
}

#[cfg(all(test, target_arch = "x86_64"))]
mod jr21_ci_latin1_case_simd_tests {
    //! JIT round 21, lane ci: the SSE2 scan / fold and the `0xDF` passes,
    //! EXECUTED over every Latin-1 byte at every position of every length
    //! class (byte loop below 8, the 8..=15 step, one or more 16-byte steps
    //! with the clamped last step) against Rust transcriptions of the JDK 25
    //! bodies, plus the registers each pass promises to keep.
    use super::*;

    /// One emitted snippet, run as `extern "C" fn(a0, a1) -> i64` (EAX), every
    /// `decline` edge answering `-1`.
    struct Snippet {
        buf: ExecutableBuffer,
    }

    impl Snippet {
        fn new(
            prologue: &[u8],
            body: impl FnOnce(&mut ExecutableBuffer, &mut Vec<usize>),
            epilogue: &[u8],
        ) -> Self {
            let mut buf = ExecutableBuffer::new(1 << 16).expect("test buffer");
            buf.emit(prologue);
            let mut decline = Vec::new();
            body(&mut buf, &mut decline);
            buf.emit(epilogue);
            buf.emit_byte(0xC3); // RET
            for p in decline {
                patch_here(&mut buf, p);
            }
            buf.emit_byte(0xB8); // MOV EAX, -1
            buf.emit(&(-1i32).to_le_bytes());
            buf.emit_byte(0xC3);
            assert!(!buf.overflowed());
            crate::platform::make_executable(buf.as_ptr() as *mut u8, buf.capacity())
                .expect("the test buffer must be flippable to RX");
            Self { buf }
        }

        fn call(&self, a0: u64, a1: u64) -> i32 {
            // SAFETY: a leaf snippet over the caller's buffers (alive across
            // the call) touching caller-saved registers only.
            let f: extern "C" fn(u64, u64) -> i64 =
                unsafe { std::mem::transmute(self.buf.as_ptr()) };
            f(a0, a1) as i32 // Cast: the EAX answer
        }
    }

    /// `MOV dst, src` (64-bit).
    fn mov(dst: u8, src: u8) -> Vec<u8> {
        vec![
            0x48 | (u8::from(src >= 8) << 2) | u8::from(dst >= 8),
            0x89,
            0xC0 | ((src & 7) << 3) | (dst & 7),
        ]
    }

    /// The platform C ABI's first two integer argument registers.
    fn args() -> (u8, u8) {
        if cfg!(windows) {
            (1, 2) // RCX, RDX
        } else {
            (7, 6) // RDI, RSI
        }
    }

    /// A fake `byte[]` of length `cap` starting with `bytes`, followed by
    /// sentinel bytes (`0xC5`, a byte both folds would change).
    fn byte_array(bytes: &[u8], cap: usize) -> Vec<u64> {
        let data = cratonvm_types::ARRAY_DATA_OFFSET;
        // Whole words: every byte the `Vec<u64>` holds past the array is a
        // sentinel (a ragged tail would read back as zero padding).
        let mut raw = vec![0u8; (data + cap.max(bytes.len()) + 16).next_multiple_of(8)];
        raw[cratonvm_types::KIND_TAGS_BYTE_OFFSET] =
            cratonvm_types::primitive_array_kind_tags_byte("[B").expect("byte[] tag");
        let len_at = cratonvm_types::ARRAY_LENGTH_OFFSET;
        let len = u32::try_from(cap).expect("small");
        raw[len_at..len_at + 4].copy_from_slice(&len.to_le_bytes());
        raw[data..data + bytes.len()].copy_from_slice(bytes);
        for b in raw.iter_mut().skip(data + cap) {
            *b = 0xC5;
        }
        let mut w = vec![0u64; raw.len().div_ceil(8)];
        for (i, b) in raw.iter().enumerate() {
            w[i / 8] |= u64::from(*b) << ((i % 8) * 8);
        }
        w
    }

    fn raw_bytes(a: &[u64]) -> Vec<u8> {
        a.iter().flat_map(|x| x.to_le_bytes()).collect()
    }

    fn payload(a: &[u64], n: usize) -> Vec<u8> {
        let data = cratonvm_types::ARRAY_DATA_OFFSET;
        raw_bytes(a)[data..data + n].to_vec()
    }

    // ---- the JDK 25 bodies, written out from `jdk25src` ----------------------

    /// `CharacterDataLatin1.toLowerCase(int)`.
    fn jdk_to_lower(ch: u32) -> u32 {
        if ch < u32::from(b'A') {
            return ch;
        }
        let lower = ch | 0x20;
        if lower <= u32::from(b'z') || ((0xE0..=0xFE).contains(&lower) && lower != 0xF7) {
            return lower;
        }
        ch
    }

    /// `CharacterDataLatin1.isUpperCase(int)` over Latin-1.
    fn jdk_is_upper(ch: u32) -> bool {
        (0x41..=0x5A).contains(&ch) || ((0xC0..=0xDE).contains(&ch) && ch != 0xD7)
    }

    /// `CharacterDataLatin1.toUpperCase(int)` over the bytes that stay in
    /// Latin-1 (every byte but `0xB5` / `0xFF`; `0xDF` maps to itself here,
    /// its "SS" is `toUpperCaseCharArray`).
    fn jdk_to_upper(ch: u32) -> u32 {
        if ch < u32::from(b'a') {
            return ch;
        }
        let upper = ch & 0xDF;
        if upper <= u32::from(b'Z') || ((0xC0..=0xDE).contains(&upper) && upper != 0xD7) {
            return upper;
        }
        ch
    }

    /// `StringLatin1.toUpperCase`'s `notUpperCaseEx` scan predicate.
    fn jdk_not_upper_case_ex(cp: u32) -> bool {
        cp >= u32::from(b'a') && (cp <= u32::from(b'z') || cp == 0xB5 || (cp >= 0xDF && cp != 0xF7))
    }

    /// What the scan must answer: `-1` declines, else bit 0 changes, bit 1 a
    /// locale-sensitive byte, bit 2 an `0xDF` (`sharp_s`).
    fn want_scan(lower: bool, sharp_s: bool, s: &[u8]) -> i32 {
        let has = |b: u8| s.contains(&b);
        if lower {
            let chg = s.iter().any(|&b| jdk_is_upper(u32::from(b)));
            return i32::from(chg) | (i32::from(has(0x49) || has(0xCC) || has(0xCD)) << 1);
        }
        if has(0xB5) || has(0xFF) || (!sharp_s && has(0xDF)) {
            return -1;
        }
        let chg = s.iter().any(|&b| jdk_not_upper_case_ex(u32::from(b)));
        i32::from(chg) | (i32::from(has(b'i')) << 1) | (i32::from(sharp_s && has(0xDF)) << 2)
    }

    const R9_SENTINEL: u32 = 0x1357_9BDF;

    /// The scan with R9 set to a sentinel; the answer is R8D, plus 0x100 if R9
    /// changed and 0x200 if R10D no longer is the array's length.
    fn scan_snippet(lower: bool, simd: bool, sharp_s: bool) -> Snippet {
        let mut pro = mov(1, args().0); // MOV RCX, a0
        pro.extend_from_slice(&[0x44, 0x8B, 0x51, 0x08]); // MOV R10D, [RCX+8]
        pro.extend_from_slice(&[0x41, 0xB9]); // MOV R9D, imm32
        pro.extend_from_slice(&R9_SENTINEL.to_le_bytes());
        let mut epi = vec![0x44, 0x89, 0xC0]; // MOV EAX, R8D
        epi.extend_from_slice(&[0x45, 0x89, 0xCB]); // MOV R11D, R9D
        epi.extend_from_slice(&[0x41, 0x81, 0xF3]); // XOR R11D, imm32
        epi.extend_from_slice(&R9_SENTINEL.to_le_bytes());
        epi.extend_from_slice(&[0x41, 0xF7, 0xDB, 0x45, 0x19, 0xDB]); // NEG R11D; SBB R11D, R11D
        epi.extend_from_slice(&[0x41, 0x81, 0xE3, 0x00, 0x01, 0x00, 0x00]); // AND R11D, 0x100
        epi.extend_from_slice(&[0x44, 0x09, 0xD8]); // OR EAX, R11D
        epi.extend_from_slice(&[0x45, 0x89, 0xD3]); // MOV R11D, R10D
        epi.extend_from_slice(&[0x44, 0x2B, 0x59, 0x08]); // SUB R11D, [RCX+8]
        epi.extend_from_slice(&[0x41, 0xF7, 0xDB, 0x45, 0x19, 0xDB]); // NEG R11D; SBB R11D, R11D
        epi.extend_from_slice(&[0x41, 0x81, 0xE3, 0x00, 0x02, 0x00, 0x00]); // AND R11D, 0x200
        epi.extend_from_slice(&[0x44, 0x09, 0xD8]); // OR EAX, R11D
        Snippet::new(
            &pro,
            |buf, decline| emit_latin1_case_scan_with(buf, lower, simd, sharp_s, decline),
            &epi,
        )
    }

    /// A small deterministic generator.
    struct Lcg(u64);

    impl Lcg {
        fn next(&mut self) -> u64 {
            self.0 = self
                .0
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            self.0 >> 33
        }
    }

    const LENGTHS: [usize; 14] = [0, 1, 7, 8, 9, 15, 16, 17, 24, 31, 32, 33, 47, 70];

    /// Every byte at every position of every length class, between unchanged
    /// neighbours, and random mixed strings, through all eight scans.
    #[test]
    fn the_simd_scan_answers_as_the_jdk_scan_for_every_byte_and_length() {
        for lower in [true, false] {
            for simd in [true, false] {
                for sharp_s in [false, true] {
                    let snip = scan_snippet(lower, simd, sharp_s);
                    let check = |s: &[u8]| {
                        let a = byte_array(s, s.len());
                        let got = snip.call(a.as_ptr() as u64, 0); // Cast: address
                        let want = want_scan(lower, sharp_s && !lower, s);
                        assert_eq!(
                            got, want,
                            "lower={lower} simd={simd} sharp_s={sharp_s} {s:x?}"
                        );
                    };
                    for len in LENGTHS {
                        check(&vec![b'1'; len]);
                        for pos in 0..len {
                            for b in 0..=255u8 {
                                let mut s = vec![b'1'; len];
                                s[pos] = b;
                                check(&s);
                            }
                        }
                    }
                    let mut g = Lcg(0x5EED);
                    for _ in 0..4000 {
                        let len = (g.next() % 72) as usize; // Cast: small
                        let s: Vec<u8> = (0..len)
                            .map(|_| {
                                let r = g.next();
                                if r % 4 == 0 {
                                    (r >> 8) as u8 // Cast: any byte
                                } else {
                                    [b'a', b'Z', b'-', 0xC9, 0xE9, b'q'][(r >> 8) as usize % 6]
                                }
                            })
                            .collect();
                        check(&s);
                    }
                }
            }
        }
    }

    /// The SSE2 fold in place (and the byte fold it falls back to) over every
    /// byte at every position, never writing past the array (the sentinel
    /// after it is a byte both folds would change), keeping RAX and R9.
    #[test]
    fn the_simd_fold_is_the_jdk_fold_and_stays_inside_the_array() {
        for lower in [true, false] {
            for simd in [true, false] {
                let mut pro = mov(11, args().0); // MOV R11, a0
                pro.extend_from_slice(&[0xB8, 0x11, 0, 0, 0]); // MOV EAX, 0x11
                pro.extend_from_slice(&[0x41, 0xB9, 0x22, 0, 0, 0]); // MOV R9D, 0x22
                let snip = Snippet::new(
                    &pro,
                    |buf, _| emit_latin1_case_map_in_place_with(buf, lower, simd),
                    &[0x44, 0x01, 0xC8], // ADD EAX, R9D
                );
                let fold = |b: u8| -> u8 {
                    let ch = u32::from(b);
                    let r = if lower {
                        jdk_to_lower(ch)
                    } else {
                        jdk_to_upper(ch)
                    };
                    u8::try_from(r).expect("Latin-1")
                };
                let check = |s: &[u8]| {
                    let a = byte_array(s, s.len());
                    let kept = snip.call(a.as_ptr() as u64, 0); // Cast: address
                    assert_eq!(kept, 0x33, "RAX / R9 kept");
                    let want: Vec<u8> = s.iter().map(|&b| fold(b)).collect();
                    assert_eq!(
                        payload(&a, s.len()),
                        want,
                        "lower={lower} simd={simd} {s:x?}"
                    );
                    let end = cratonvm_types::ARRAY_DATA_OFFSET + s.len();
                    assert!(
                        raw_bytes(&a)[end..].iter().all(|&b| b == 0xC5),
                        "wrote past the array"
                    );
                };
                let all: Vec<u8> = (0..=255u8)
                    .filter(|b| lower || !matches!(b, 0xB5 | 0xDF | 0xFF))
                    .collect();
                check(&all);
                for len in LENGTHS {
                    for pos in 0..len {
                        for &b in &all {
                            let mut s = vec![b'1'; len];
                            s[pos] = b;
                            check(&s);
                        }
                    }
                }
                let mut g = Lcg(0xF01D);
                for _ in 0..4000 {
                    let len = (g.next() % 72) as usize; // Cast: small
                    let s: Vec<u8> = (0..len)
                        .map(|_| all[(g.next() as usize) % all.len()]) // Cast: index
                        .collect();
                    check(&s);
                }
            }
        }
    }

    /// `0xDF` counted, then copied as "SS" and the rest folded: the JDK's
    /// `toUpperCaseEx(.., false)` + `StringUTF16.newString` (compressed back
    /// to LATIN1) for every string without `0xB5` / `0xFF`.
    #[test]
    fn sharp_s_expands_to_ss_and_the_rest_folds_upper() {
        let count = {
            let mut pro = mov(1, args().0); // MOV RCX, a0
            pro.extend_from_slice(&[0x44, 0x8B, 0x51, 0x08]); // MOV R10D, [RCX+8]
            Snippet::new(&pro, |buf, _| emit_count_sharp_s(buf), &[])
        };
        for simd in [true, false] {
            let mut pro = mov(11, args().0); // MOV R11, a0: the result
            pro.extend(mov(2, args().1)); // MOV RDX, a1: the source
            pro.extend_from_slice(&[0xB8, 0x77, 0, 0, 0]); // MOV EAX, 0x77
            let snip = Snippet::new(
                &pro,
                |buf, _| {
                    emit_copy_expanding_sharp_s(buf);
                    emit_latin1_case_map_in_place_with(buf, false, simd);
                },
                &[],
            );
            let mut g = Lcg(0xD15C);
            let pool: Vec<u8> = (0..=255u8).filter(|b| !matches!(b, 0xB5 | 0xFF)).collect();
            let mut cases: Vec<Vec<u8>> = vec![
                vec![0xDF],
                b"stra\xdfe".to_vec(),
                vec![0xDF; 20],
                b"abc".to_vec(),
            ];
            for _ in 0..3000 {
                let len = 1 + (g.next() % 40) as usize; // Cast: small
                cases.push(
                    (0..len)
                        .map(|_| {
                            let r = g.next();
                            if r % 5 == 0 {
                                0xDF
                            } else {
                                pool[(r >> 8) as usize % pool.len()] // Cast: index
                            }
                        })
                        .collect(),
                );
            }
            for s in cases {
                let n = s.iter().filter(|&&b| b == 0xDF).count();
                let src = byte_array(&s, s.len());
                let counted = count.call(src.as_ptr() as u64, 0); // Cast: address
                assert_eq!(counted, i32::try_from(n).expect("small"));
                let dst = byte_array(&[], s.len() + n);
                // Cast: addresses
                let kept = snip.call(dst.as_ptr() as u64, src.as_ptr() as u64);
                assert_eq!(kept, 0x77, "RAX kept");
                let mut want = Vec::new();
                for &b in &s {
                    if b == 0xDF {
                        want.extend_from_slice(b"SS");
                    } else {
                        want.push(u8::try_from(jdk_to_upper(u32::from(b))).expect("Latin-1"));
                    }
                }
                assert_eq!(payload(&dst, s.len() + n), want, "simd={simd} {s:x?}");
                assert_eq!(payload(&src, s.len()), s, "the source is not written");
            }
        }
    }
}

#[cfg(all(test, target_arch = "x86_64"))]
mod jr21_latin1up_tests {
    //! JIT round 21, lane latin1up (`CRATONVM_JIT_IR_LATIN1_UPPER_UTF16`): the
    //! `wide` scan (`0xB5` / `0xFF` report bit 3 instead of declining) and the
    //! UTF16 payload pass, EXECUTED over every Latin-1 byte at every position
    //! of every length class against Rust's own `char::to_uppercase` (which
    //! over Latin-1 is the JDK's `toUpperCaseEx` + `toUpperCaseCharArray`).
    use super::*;

    struct Snippet {
        buf: ExecutableBuffer,
    }

    impl Snippet {
        fn new(
            prologue: &[u8],
            body: impl FnOnce(&mut ExecutableBuffer, &mut Vec<usize>),
            epilogue: &[u8],
        ) -> Self {
            let mut buf = ExecutableBuffer::new(1 << 16).expect("test buffer");
            buf.emit(prologue);
            let mut decline = Vec::new();
            body(&mut buf, &mut decline);
            buf.emit(epilogue);
            buf.emit_byte(0xC3); // RET
            for p in decline {
                patch_here(&mut buf, p);
            }
            buf.emit_byte(0xB8); // MOV EAX, -1
            buf.emit(&(-1i32).to_le_bytes());
            buf.emit_byte(0xC3);
            assert!(!buf.overflowed());
            crate::platform::make_executable(buf.as_ptr() as *mut u8, buf.capacity())
                .expect("the test buffer must be flippable to RX");
            Self { buf }
        }

        fn call(&self, a0: u64, a1: u64) -> i32 {
            // SAFETY: a leaf snippet over the caller's buffers (alive across
            // the call) touching caller-saved registers only.
            let f: extern "C" fn(u64, u64) -> i64 =
                unsafe { std::mem::transmute(self.buf.as_ptr()) };
            f(a0, a1) as i32 // Cast: the EAX answer
        }
    }

    /// `MOV dst, src` (64-bit).
    fn mov(dst: u8, src: u8) -> Vec<u8> {
        vec![
            0x48 | (u8::from(src >= 8) << 2) | u8::from(dst >= 8),
            0x89,
            0xC0 | ((src & 7) << 3) | (dst & 7),
        ]
    }

    fn args() -> (u8, u8) {
        if cfg!(windows) {
            (1, 2) // RCX, RDX
        } else {
            (7, 6) // RDI, RSI
        }
    }

    /// A fake `byte[]` of length `cap` starting with `bytes`, then `0xC5`
    /// sentinels.
    fn byte_array(bytes: &[u8], cap: usize) -> Vec<u64> {
        let data = cratonvm_types::ARRAY_DATA_OFFSET;
        let mut raw = vec![0u8; (data + cap.max(bytes.len()) + 16).next_multiple_of(8)];
        raw[cratonvm_types::KIND_TAGS_BYTE_OFFSET] =
            cratonvm_types::primitive_array_kind_tags_byte("[B").expect("byte[] tag");
        let len_at = cratonvm_types::ARRAY_LENGTH_OFFSET;
        let len = u32::try_from(cap).expect("small");
        raw[len_at..len_at + 4].copy_from_slice(&len.to_le_bytes());
        raw[data..data + bytes.len()].copy_from_slice(bytes);
        for b in raw.iter_mut().skip(data + cap) {
            *b = 0xC5;
        }
        let mut w = vec![0u64; raw.len().div_ceil(8)];
        for (i, b) in raw.iter().enumerate() {
            w[i / 8] |= u64::from(*b) << ((i % 8) * 8);
        }
        w
    }

    fn raw_bytes(a: &[u64]) -> Vec<u8> {
        a.iter().flat_map(|x| x.to_le_bytes()).collect()
    }

    /// The JDK's UTF16 units of `toUpperCase(Locale.ROOT)` of one Latin-1
    /// byte, from Rust's Unicode tables.
    fn upper_units(b: u8) -> Vec<u16> {
        let mut out = Vec::new();
        for c in char::from(b).to_uppercase() {
            let mut two = [0u16; 2];
            out.extend_from_slice(c.encode_utf16(&mut two));
        }
        out
    }

    /// `StringLatin1.toUpperCase`'s `notUpperCaseEx` scan predicate.
    fn changes(b: u8) -> bool {
        b >= b'a' && (b <= b'z' || b == 0xB5 || (b >= 0xDF && b != 0xF7))
    }

    /// The `wide` scan's R8D: bit 0 a change, 1 an `i`, 2 an `0xDF`, 3 an
    /// `0xB5` / `0xFF`; never a decline.
    fn want_wide_scan(s: &[u8]) -> i32 {
        let has = |b: u8| s.contains(&b);
        i32::from(s.iter().any(|&b| changes(b)))
            | (i32::from(has(b'i')) << 1)
            | (i32::from(has(0xDF)) << 2)
            | (i32::from(has(0xB5) || has(0xFF)) << 3)
    }

    const LENGTHS: [usize; 14] = [0, 1, 7, 8, 9, 15, 16, 17, 24, 31, 32, 33, 47, 70];

    #[test]
    fn jr21_latin1up_oracle_pins_the_three_out_of_latin1_bytes() {
        assert_eq!(upper_units(0xB5), vec![0x39C]);
        assert_eq!(upper_units(0xFF), vec![0x178]);
        assert_eq!(upper_units(0xDF), vec![0x53, 0x53]);
        assert_eq!(upper_units(0xF7), vec![0xF7]);
        assert_eq!(upper_units(0xAA), vec![0xAA]);
        for b in 0..=255u8 {
            let u = upper_units(b);
            assert_eq!(changes(b), u != vec![u16::from(b)], "{b:#x}");
        }
    }

    #[test]
    fn jr21_latin1up_wide_scan_reports_utf16_bytes_instead_of_declining() {
        for simd in [true, false] {
            for sharp_s in [false, true] {
                let mut pro = mov(1, args().0); // MOV RCX, a0
                pro.extend_from_slice(&[0x44, 0x8B, 0x51, 0x08]); // MOV R10D, [RCX+8]
                let upper = Snippet::new(
                    &pro,
                    |buf, decline| {
                        emit_latin1_case_scan_wide(buf, false, simd, sharp_s, true, decline)
                    },
                    &[0x44, 0x89, 0xC0], // MOV EAX, R8D
                );
                // `wide` never reaches the lower case: the plain scan's answer.
                let lower_wide = Snippet::new(
                    &pro,
                    |buf, decline| {
                        emit_latin1_case_scan_wide(buf, true, simd, sharp_s, true, decline)
                    },
                    &[0x44, 0x89, 0xC0],
                );
                let lower_plain = Snippet::new(
                    &pro,
                    |buf, decline| emit_latin1_case_scan_with(buf, true, simd, false, decline),
                    &[0x44, 0x89, 0xC0],
                );
                let check = |s: &[u8]| {
                    let a = byte_array(s, s.len());
                    let p = a.as_ptr() as u64; // Cast: address
                    let what = format!("simd={simd} sharp_s={sharp_s} {s:x?}");
                    assert_eq!(upper.call(p, 0), want_wide_scan(s), "upper {what}");
                    assert_eq!(
                        lower_wide.call(p, 0),
                        lower_plain.call(p, 0),
                        "lower {what}"
                    );
                };
                for len in LENGTHS {
                    check(&vec![b'1'; len]);
                    for pos in 0..len {
                        for b in 0..=255u8 {
                            let mut s = vec![b'1'; len];
                            s[pos] = b;
                            check(&s);
                        }
                    }
                }
                let pool = [b'a', b'Z', 0xB5, 0xDF, 0xFF, 0xE9, b'i', b'-'];
                let mut seed = 0x1A71_u64;
                let mut next = || {
                    seed = seed
                        .wrapping_mul(6_364_136_223_846_793_005)
                        .wrapping_add(1_442_695_040_888_963_407);
                    seed >> 33
                };
                for _ in 0..3000 {
                    let len = (next() % 72) as usize; // Cast: small
                    let s: Vec<u8> = (0..len)
                        .map(|_| pool[(next() % 8) as usize]) // Cast: index
                        .collect();
                    check(&s);
                }
            }
        }
    }

    #[test]
    fn jr21_latin1up_utf16_payload_is_the_jdk_upper_case() {
        let mut pro = mov(11, args().0); // MOV R11, a0: the result
        pro.extend(mov(2, args().1)); // MOV RDX, a1: the source
        pro.extend_from_slice(&[0xB8, 0x77, 0, 0, 0]); // MOV EAX, 0x77
        let snip = Snippet::new(
            &pro,
            |buf, _| emit_copy_upper_utf16(buf),
            // EAX + R11D - EDX: RAX, R11 and RDX kept.
            &[0x44, 0x01, 0xD8, 0x29, 0xD0],
        );
        let all: Vec<u8> = (0..=255u8).collect();
        let mut cases: Vec<Vec<u8>> = vec![
            all.clone(),
            vec![0xB5],
            vec![0xFF],
            vec![0xDF, 0xB5],
            b"Content-Type-\xb5\xe9\xdf\xff abc".to_vec(),
        ];
        for len in LENGTHS {
            for pos in 0..len {
                for &b in &all {
                    let mut s = vec![b'q'; len];
                    s[pos] = b;
                    cases.push(s);
                }
            }
        }
        let data = cratonvm_types::ARRAY_DATA_OFFSET;
        for s in cases {
            let want: Vec<u8> = s
                .iter()
                .flat_map(|&b| upper_units(b))
                .flat_map(u16::to_le_bytes)
                .collect();
            let src = byte_array(&s, s.len());
            let dst = byte_array(&[], want.len());
            // Cast: the addresses; then their low halves, as the snippet sums them.
            let (d, p) = (dst.as_ptr() as u64, src.as_ptr() as u64);
            let kept = 0x77i32
                .wrapping_add(d as u32 as i32)
                .wrapping_sub(p as u32 as i32);
            assert_eq!(snip.call(d, p), kept, "RAX / RDX / R11 kept");
            let got = raw_bytes(&dst);
            assert_eq!(&got[data..data + want.len()], &want[..], "{s:x?}");
            assert!(
                got[data + want.len()..].iter().all(|&b| b == 0xC5),
                "wrote past the array: {s:x?}"
            );
            assert_eq!(
                &raw_bytes(&src)[data..data + s.len()],
                &s[..],
                "the source is kept"
            );
        }
    }
}
