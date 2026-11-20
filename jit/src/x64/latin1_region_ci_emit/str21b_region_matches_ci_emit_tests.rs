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
