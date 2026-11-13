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
mod str21b_region_matches_ci_emit_tests {
    use super::*;

    /// The Latin-1 byte as a char, `Character.toUpperCase(char)`.
    fn char_upper(c: u32) -> u32 {
        match c {
            0xB5 => 0x39C,
            0xFF => 0x178,
            0x61..=0x7A => c - 0x20,
            0xE0..=0xFE if c != 0xF7 => c - 0x20,
            _ => c,
        }
    }

    /// `Character.toLowerCase(char)` over the images of [`char_upper`].
    fn char_lower(c: u32) -> u32 {
        match c {
            0x39C => 0x3BC,
            0x178 => 0xFF,
            0x41..=0x5A => c + 0x20,
            0xC0..=0xDE if c != 0xD7 => c + 0x20,
            _ => c,
        }
    }

    /// The JDK 17 body: `Character.toUpperCase` then `toLowerCase` of the chars.
    fn old_spelling(b1: u8, b2: u8) -> bool {
        let (c1, c2) = (u32::from(b1), u32::from(b2));
        if c1 == c2 {
            return true;
        }
        let (u1, u2) = (char_upper(c1), char_upper(c2));
        u1 == u2 || char_lower(u1) == char_lower(u2)
    }

    /// `CharacterDataLatin1.equalsIgnoreCase(byte, byte)`, JDK 21+.
    fn new_spelling(b1: u8, b2: u8) -> bool {
        if b1 == b2 {
            return true;
        }
        let upper = u32::from(b1) & 0xDF;
        if upper < u32::from(b'A') {
            return false;
        }
        (upper <= u32::from(b'Z') || ((0xC0..=0xDE).contains(&upper) && upper != 0xD7))
            && upper == (u32::from(b2) & 0xDF)
    }

    /// `MOV <reg>, imm64` for RAX (0), R10 (10) or R11 (11).
    fn mov_imm64(reg: u8, value: u64) -> Vec<u8> {
        let mut v = match reg {
            0 => vec![0x48, 0xB8],
            10 => vec![0x49, 0xBA],
            _ => vec![0x49, 0xBB],
        };
        v.extend_from_slice(&value.to_le_bytes());
        v
    }

    /// An executable `extern "C" fn(value, other) -> i64` running the pass on
    /// the baked offsets, every decline answering `-1`.
    struct Snippet {
        buf: ExecutableBuffer,
    }

    impl Snippet {
        fn new(toffset: u64, ooffset: u64, len: u64) -> Self {
            let mut buf = ExecutableBuffer::new(1 << 14).expect("test buffer");
            // The two array arguments into RCX / RDX (SysV: RDI / RSI; Win64:
            // already there).
            #[cfg(not(windows))]
            {
                buf.emit(&[0x48, 0x89, 0xF9]); // MOV RCX, RDI
                buf.emit(&[0x48, 0x89, 0xF2]); // MOV RDX, RSI
            }
            buf.emit(&mov_imm64(11, toffset));
            buf.emit(&mov_imm64(0, ooffset));
            buf.emit(&mov_imm64(10, len));
            let mut decline = Vec::new();
            emit_latin1_region_matches_ci(&mut buf, &mut decline);
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

        fn run(&self, value: &[u64], other: &[u64]) -> i32 {
            // SAFETY: a leaf snippet reading the two caller-owned fake arrays
            // (alive across the call) and touching caller-saved registers only.
            let f: extern "C" fn(u64, u64) -> i64 =
                unsafe { std::mem::transmute(self.buf.as_ptr()) };
            f(value.as_ptr() as u64, other.as_ptr() as u64) as i32 // Cast: the EAX answer
        }
    }

    fn words(raw: &[u8]) -> Vec<u64> {
        let mut w = vec![0u64; raw.len().div_ceil(8)];
        for (i, b) in raw.iter().enumerate() {
            w[i / 8] |= u64::from(*b) << ((i % 8) * 8);
        }
        w
    }

    /// A fake `byte[]` holding `bytes`, with 8 slack bytes of `0xEE` past the
    /// end so an over-read could not look like a match.
    fn byte_array(bytes: &[u8]) -> Vec<u64> {
        let data = cratonvm_types::ARRAY_DATA_OFFSET;
        let mut raw = vec![0xEEu8; data + bytes.len() + 8];
        raw[..data].fill(0);
        raw[cratonvm_types::KIND_TAGS_BYTE_OFFSET] =
            cratonvm_types::primitive_array_kind_tags_byte("[B").expect("byte[] tag");
        let len_at = cratonvm_types::ARRAY_LENGTH_OFFSET;
        let len = u32::try_from(bytes.len()).expect("small");
        raw[len_at..len_at + 4].copy_from_slice(&len.to_le_bytes());
        raw[data..data + bytes.len()].copy_from_slice(bytes);
        words(&raw)
    }

    /// The expected answer, written from the JDK loop plus the pass's bounds
    /// contract (`-1`: decline).
    fn want(value: &[u8], toffset: u32, other: &[u8], ooffset: u32, len: u32) -> i32 {
        let (vlen, olen) = (value.len() as u32, other.len() as u32); // Cast: small
        if toffset > vlen || len > vlen - toffset || ooffset > olen || len > olen - ooffset {
            return -1;
        }
        let ok = (0..len as usize)
            .all(|i| new_spelling(value[toffset as usize + i], other[ooffset as usize + i]));
        i32::from(ok)
    }

    #[test]
    fn str21b_the_two_jdk_spellings_agree_on_every_byte_pair() {
        for b1 in 0..=255u8 {
            for b2 in 0..=255u8 {
                assert_eq!(
                    old_spelling(b1, b2),
                    new_spelling(b1, b2),
                    "{b1:#x} {b2:#x}"
                );
            }
        }
        // The four Latin-1 specials that pair with nothing but themselves.
        for special in [0xB5u8, 0xDF, 0xFF, 0xD7, 0xF7] {
            for other in 0..=255u8 {
                assert_eq!(
                    new_spelling(special, other),
                    special == other,
                    "{special:#x}"
                );
                assert_eq!(
                    new_spelling(other, special),
                    special == other,
                    "{special:#x}"
                );
            }
        }
        assert!(new_spelling(b'a', b'A') && new_spelling(0xE9, 0xC9) && !new_spelling(b'@', b'`'));
    }

    /// Every byte pair through the emitted code, as a one-byte region.
    #[test]
    fn str21b_the_emitted_loop_answers_every_byte_pair_as_the_jdk() {
        let all: Vec<u8> = (0..=255u8).collect();
        let value = byte_array(&all);
        for b1 in 0..=255u32 {
            let code = Snippet::new(u64::from(b1), 0, 1);
            for b2 in 0..=255u8 {
                let other = byte_array(&[b2]);
                let want = i32::from(new_spelling(b1 as u8, b2)); // Cast: a byte
                assert_eq!(code.run(&value, &other), want, "{b1:#x} vs {b2:#x}");
            }
        }
    }

    /// Lengths 0..=40 (the 8-byte block, its tail, the byte step), offsets in
    /// and out of range, a differing byte at every position, dirty upper
    /// halves in the three ints, against the JDK loop.
    #[test]
    fn str21b_the_emitted_loop_matches_the_jdk_over_offsets_and_lengths() {
        // A fixed word and its other-cased twin, with the specials mixed in.
        let base: Vec<u8> = (0..48u32)
            .map(|i| match i % 11 {
                0 => b'A' + (i % 26) as u8,
                1 => 0xC0 + (i % 31) as u8,
                2 => 0xB5,
                3 => 0xDF,
                4 => 0xFF,
                5 => b'a' + (i % 26) as u8,
                6 => b'-',
                7 => 0xE0 + (i % 31) as u8,
                8 => b'7',
                9 => 0xD7,
                _ => 0xF7,
            })
            .collect();
        let swap = |b: u8| -> u8 {
            // The other case where Latin-1 has one.
            match b {
                b'A'..=b'Z' | b'a'..=b'z' | 0xC0..=0xDE | 0xE0..=0xFE if b != 0xD7 && b != 0xF7 => {
                    b ^ 0x20
                }
                _ => b,
            }
        };
        let twin: Vec<u8> = base.iter().map(|&b| swap(b)).collect();
        let dirty = |v: u32, garbage: u64| (garbage << 32) | u64::from(v);
        for (vlen, olen) in [(48usize, 48usize), (20, 48), (48, 9), (0, 0), (1, 1)] {
            let (v, o) = (&twin[..vlen], &base[..olen]);
            let (va, oa) = (byte_array(v), byte_array(o));
            for toffset in [0u32, 1, 3, 8, 17, 47, 48, 49, u32::MAX, 0x8000_0000] {
                for ooffset in [0u32, 1, 5, 40, 48, 49, u32::MAX] {
                    for len in [0u32, 1, 2, 7, 8, 9, 15, 16, 17, 40, 48, 49, u32::MAX] {
                        let code = Snippet::new(
                            dirty(toffset, 0xDEAD_BEEF),
                            dirty(ooffset, 0x1234_5678),
                            dirty(len, 0xFFFF_FFFF),
                        );
                        assert_eq!(
                            code.run(&va, &oa),
                            want(v, toffset, o, ooffset, len),
                            "vlen {vlen} olen {olen} toffset {toffset} ooffset {ooffset} len {len}"
                        );
                    }
                }
            }
        }
    }

    /// One byte flipped to a non-twin at each position of a 33-byte word: the
    /// answer is `false` exactly when the flip lies inside the region.
    #[test]
    fn str21b_a_single_mismatch_is_found_at_every_position() {
        let word: Vec<u8> = b"Content-Type: TEXT/plain; Charset=\xC9t\xE9".to_vec();
        let twin: Vec<u8> = word
            .iter()
            .map(|&b| {
                if b.is_ascii_alphabetic() {
                    b ^ 0x20
                } else if b == 0xC9 {
                    0xE9
                } else if b == 0xE9 {
                    0xC9
                } else {
                    b
                }
            })
            .collect();
        let value = byte_array(&twin);
        let n = word.len() as u64; // Cast: small
        let all = Snippet::new(0, 0, n);
        assert_eq!(
            all.run(&value, &byte_array(&word)),
            1,
            "the case twin matches whole"
        );
        for at in 0..word.len() {
            let mut bad = word.clone();
            // `0x80` can never be the other case of an ASCII letter or digit.
            bad[at] = 0x80;
            assert_eq!(all.run(&value, &byte_array(&bad)), 0, "mismatch at {at}");
            // A region that stops before the flip still matches.
            let short = Snippet::new(0, 0, at as u64);
            assert_eq!(
                short.run(&value, &byte_array(&bad)),
                1,
                "region ends before {at}"
            );
        }
    }
}
