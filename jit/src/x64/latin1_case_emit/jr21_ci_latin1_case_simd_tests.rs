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
