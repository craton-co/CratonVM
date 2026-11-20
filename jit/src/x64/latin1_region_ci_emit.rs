// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! JIT round 21, lane str21b (STRINGS2-21-4,
//! `CRATONVM_JIT_IR_LATIN1_REGION_MATCHES_CI_PREFIX`, opt-in): the machine code
//! of the optimizing tier's inline `StringLatin1.regionMatchesCI(byte[], int,
//! byte[], int, int)` prefix (`ir_lower::Lowerer::try_emit_ir_latin1_region_matches_ci_prefix`),
//! split out so the pass is executed by a unit test on its own.
//!
//! The JDK body (`String.equalsIgnoreCase` ends in it, through
//! `String.regionMatches(true, ..)`, for two LATIN1 strings): walk `len` bytes
//! from `toffset` / `ooffset`; equal bytes continue; unequal bytes continue iff
//! `CharacterDataLatin1.equalsIgnoreCase(b1, b2)`, which JDK 21+ writes as
//!
//! ```text
//! upper = b1 & 0xDF;                       // ASCII and Latin-1 were designed for this
//! if (upper < 'A') return false;           // low ASCII
//! return (upper <= 'Z' || (upper >= 0xC0 && upper <= 0xDE && upper != 0xD7))
//!     && upper == (b2 & 0xDF);
//! ```
//!
//! and the older body spelled as `toUpperCase` then `toLowerCase` of the
//! chars. The two agree over all 65 536 byte pairs (the test
//! `str21b_the_two_jdk_spellings_agree_on_every_byte_pair` pins it): only the
//! letters `A-Z` / `a-z` and `0xC0..=0xDE` / `0xE0..=0xFE` without `0xD7` /
//! `0xF7` pair with their other case; `0xB5` (upper `0x39C`), `0xDF` and `0xFF`
//! (upper `0x178`) pair with nothing in Latin-1.
//!
//! The caller (`String.regionMatches`) has checked the bounds, but this static
//! is public to the JDK and the prefix must not widen what the call accepts:
//! every offset or length that is not a plain in-range non-negative `int`
//! pair DECLINES to the call, which then raises (or answers `true` for an
//! empty range) exactly as before.

use super::concat_emit::{jcc_patch, jmp_patch, patch_here};
use crate::ExecutableBuffer;

// The pass bakes the array header: length at `+8`, payload at `+16`.
const _: () = assert!(
    cratonvm_types::ARRAY_LENGTH_OFFSET == 8 && cratonvm_types::ARRAY_DATA_OFFSET == 16,
    "the regionMatchesCI pass bakes ARRAY_LENGTH_OFFSET 8 / ARRAY_DATA_OFFSET 16"
);

/// A backward `JMP rel32` to `top`.
fn jmp_back(buf: &mut ExecutableBuffer, top: usize) {
    let p = jmp_patch(buf);
    let rel = top as i32 - (p as i32 + 4); // Cast: rel32 displacement
    let _ = buf.try_patch_i32(p, rel);
}

/// The whole pass. On entry RCX = a non-null `byte[]` (`value`), RDX = a
/// non-null `byte[]` (`other`), R11 = `toffset`, RAX = `ooffset`, R10 = `len`
/// (each an `int` in the low 32 bits; the upper halves are ignored). On the
/// fall-through EAX = 1 (the regions match ignoring case) or 0. Every decline
/// -- an offset or the length negative, or a region past either array's end --
/// is pushed onto `decline` before anything is read past a header; the pass
/// stores nothing at all. Clobbers RAX, RCX, RDX, R8, R9, R10, R11 and the
/// flags.
pub(crate) fn emit_latin1_region_matches_ci(buf: &mut ExecutableBuffer, decline: &mut Vec<usize>) {
    // The three ints, zero-extended: a negative one is then above every
    // array length and fails the unsigned bounds tests below.
    buf.emit(&[0x45, 0x89, 0xD2]); // MOV R10D, R10D
    buf.emit(&[0x45, 0x89, 0xDB]); // MOV R11D, R11D
    buf.emit(&[0x89, 0xC0]); // MOV EAX, EAX
                             // toffset in [0, value.length] and len <= value.length - toffset.
    buf.emit(&[0x44, 0x3B, 0x59, 0x08]); // CMP R11D, [RCX+8]
    decline.push(jcc_patch(buf, 0x87)); // JA
    buf.emit(&[0x44, 0x8B, 0x41, 0x08]); // MOV R8D, [RCX+8]
    buf.emit(&[0x45, 0x29, 0xD8]); // SUB R8D, R11D
    buf.emit(&[0x45, 0x39, 0xC2]); // CMP R10D, R8D
    decline.push(jcc_patch(buf, 0x87)); // JA
    buf.emit(&[0x4A, 0x8D, 0x0C, 0x19]); // LEA RCX, [RCX+R11]
                                         // ooffset in [0, other.length] and len <= other.length - ooffset.
    buf.emit(&[0x3B, 0x42, 0x08]); // CMP EAX, [RDX+8]
    decline.push(jcc_patch(buf, 0x87)); // JA
    buf.emit(&[0x44, 0x8B, 0x42, 0x08]); // MOV R8D, [RDX+8]
    buf.emit(&[0x41, 0x29, 0xC0]); // SUB R8D, EAX
    buf.emit(&[0x45, 0x39, 0xC2]); // CMP R10D, R8D
    decline.push(jcc_patch(buf, 0x87)); // JA
    buf.emit(&[0x48, 0x8D, 0x14, 0x02]); // LEA RDX, [RDX+RAX]
                                         // No decline past this point. RCX / RDX address element 0 of each
                                         // region minus the 16-byte header.
    buf.emit(&[0x31, 0xC0]); // XOR EAX, EAX: i
    let top = buf.pos();
    buf.emit(&[0x44, 0x39, 0xD0]); // CMP EAX, R10D
    let all_equal = jcc_patch(buf, 0x83); // JAE: every byte matched
                                          // Eight equal bytes at a time while eight remain.
    buf.emit(&[0x45, 0x89, 0xD0]); // MOV R8D, R10D
    buf.emit(&[0x41, 0x29, 0xC0]); // SUB R8D, EAX
    buf.emit(&[0x41, 0x83, 0xF8, 0x08]); // CMP R8D, 8
    let to_byte = jcc_patch(buf, 0x82); // JB
    buf.emit(&[0x4C, 0x8B, 0x44, 0x01, 0x10]); // MOV R8, [RCX+RAX+16]
    buf.emit(&[0x4C, 0x3B, 0x44, 0x02, 0x10]); // CMP R8, [RDX+RAX+16]
    let block_differs = jcc_patch(buf, 0x85); // JNE
    buf.emit(&[0x83, 0xC0, 0x08]); // ADD EAX, 8
    jmp_back(buf, top);
    patch_here(buf, to_byte);
    patch_here(buf, block_differs);
    // One byte: equal bytes step on; unequal ones need the case test.
    buf.emit(&[0x44, 0x0F, 0xB6, 0x44, 0x01, 0x10]); // MOVZX R8D, BYTE [RCX+RAX+16]
    buf.emit(&[0x44, 0x0F, 0xB6, 0x4C, 0x02, 0x10]); // MOVZX R9D, BYTE [RDX+RAX+16]
    buf.emit(&[0x45, 0x39, 0xC8]); // CMP R8D, R9D
    let same = jcc_patch(buf, 0x84); // JE
    let mut not_equal: Vec<usize> = Vec::new();
    buf.emit(&[0x41, 0x83, 0xE0, 0xDF]); // AND R8D, 0xDF: b1's upper case
    buf.emit(&[0x41, 0x83, 0xF8, 0x41]); // CMP R8D, 'A'
    not_equal.push(jcc_patch(buf, 0x82)); // JB: low ASCII
    buf.emit(&[0x41, 0x83, 0xF8, 0x5A]); // CMP R8D, 'Z'
    let letter = jcc_patch(buf, 0x86); // JBE: A-Z
    buf.emit(&[0x41, 0x81, 0xF8, 0xC0, 0x00, 0x00, 0x00]); // CMP R8D, 0xC0
    not_equal.push(jcc_patch(buf, 0x82)); // JB: `[` .. 0xBF
    buf.emit(&[0x41, 0x81, 0xF8, 0xDE, 0x00, 0x00, 0x00]); // CMP R8D, 0xDE
    not_equal.push(jcc_patch(buf, 0x87)); // JA: 0xDF
    buf.emit(&[0x41, 0x81, 0xF8, 0xD7, 0x00, 0x00, 0x00]); // CMP R8D, 0xD7
    not_equal.push(jcc_patch(buf, 0x84)); // JE: the multiplication sign
    patch_here(buf, letter);
    buf.emit(&[0x41, 0x83, 0xE1, 0xDF]); // AND R9D, 0xDF: b2's upper case
    buf.emit(&[0x45, 0x39, 0xC8]); // CMP R8D, R9D
    not_equal.push(jcc_patch(buf, 0x85)); // JNE: the other case of another letter
    patch_here(buf, same);
    buf.emit(&[0xFF, 0xC0]); // INC EAX
    jmp_back(buf, top);
    // The answers.
    patch_here(buf, all_equal);
    buf.emit(&[0xB8, 0x01, 0x00, 0x00, 0x00]); // MOV EAX, 1
    let done = jmp_patch(buf);
    for p in not_equal {
        patch_here(buf, p);
    }
    buf.emit(&[0x31, 0xC0]); // XOR EAX, EAX
    patch_here(buf, done);
}

#[cfg(all(test, target_arch = "x86_64"))]
mod str21b_region_matches_ci_emit_tests;
