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
