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
