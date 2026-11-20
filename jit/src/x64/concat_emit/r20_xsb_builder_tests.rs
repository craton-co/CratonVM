use super::*;

#[cfg(windows)]
const A0: u8 = 1; // RCX
#[cfg(windows)]
const A1: u8 = 2; // RDX
#[cfg(not(windows))]
const A0: u8 = 7; // RDI
#[cfg(not(windows))]
const A1: u8 = 6; // RSI

const STRING_CID: u32 = 77;
const TOTAL: i32 = 32;
const VALUE_OFF: i32 = 16;
const CODER: (i32, bool) = (24, true);

fn words(raw: &[u8]) -> Vec<u64> {
    let mut w = vec![0u64; raw.len().div_ceil(8)];
    for (i, b) in raw.iter().enumerate() {
        w[i / 8] |= u64::from(*b) << ((i % 8) * 8);
    }
    w
}

fn raw(w: &[u64]) -> Vec<u8> {
    w.iter().flat_map(|x| x.to_le_bytes()).collect()
}

/// A fake array of kind `desc` and header length `len`: `payload`, then
/// `0xEE` filler up to `len` bytes (and 8 more).
fn array(desc: &str, len: u32, payload: &[u8]) -> Vec<u64> {
    let data = cratonvm_types::ARRAY_DATA_OFFSET;
    let n = (len as usize).max(payload.len()); // Cast: a small count
    let mut r = vec![0xEEu8; data + n + 8];
    r[..data].fill(0);
    r[cratonvm_types::KIND_TAGS_BYTE_OFFSET] =
        cratonvm_types::primitive_array_kind_tags_byte(desc).expect("array tag");
    let len_at = cratonvm_types::ARRAY_LENGTH_OFFSET;
    r[len_at..len_at + 4].copy_from_slice(&len.to_le_bytes());
    r[data..data + payload.len()].copy_from_slice(payload);
    words(&r)
}

/// Run `append(int)` over `value` with `count` and `i` (RCX = value,
/// R11D = count, R10D = i), answering the new count (R9D) or -1 on a
/// decline.
fn append_int(value: &mut [u64], count: u32, i: i32) -> i64 {
    let mut buf = ExecutableBuffer::new(1 << 16).expect("test buffer");
    mov_r64_r64(&mut buf, RAX, A1); // RAX = count << 32 | (u32) i
    mov_r64_r64(&mut buf, RCX, A0);
    mov_r64_r64(&mut buf, R11, RAX);
    shr_r64_imm8(&mut buf, R11, 32);
    buf.emit(&[0x41, 0x89, 0xC2]); // MOV R10D,EAX
    let mut decline = Vec::new();
    emit_sb_append_int_latin1(&mut buf, &mut decline);
    buf.emit(&[0x4C, 0x89, 0xC8]); // MOV RAX,R9
    buf.emit_byte(0xC3); // RET
    for p in decline {
        patch_here(&mut buf, p);
    }
    mov_imm32_sx(&mut buf, RAX, -1);
    buf.emit_byte(0xC3); // RET
    assert!(!buf.overflowed());
    crate::platform::make_executable(buf.as_ptr() as *mut u8, buf.capacity())
        .expect("the test buffer must be flippable to RX");
    // SAFETY: a leaf snippet over the caller's buffer (alive across the
    // call) and caller-saved registers only.
    let f: extern "C" fn(u64, u64) -> i64 = unsafe { std::mem::transmute(buf.as_ptr()) };
    // Cast: the fake array's address; the packed operands.
    f(
        value.as_mut_ptr() as u64,
        (u64::from(count) << 32) | u64::from(i as u32),
    )
}

#[test]
fn append_int_writes_integer_to_string_in_place() {
    let data = cratonvm_types::ARRAY_DATA_OFFSET;
    let ints: [i32; 12] = [
        0,
        7,
        -7,
        42,
        -10,
        99,
        1_000_000_000,
        -1_000_000_000,
        999_999_999,
        i32::MAX,
        i32::MIN,
        -2_147_483_647,
    ];
    for i in ints {
        let text = i.to_string();
        let prefix = b"ab=";
        // Cast (both): small counts.
        let count = prefix.len() as u32;
        let exact = count + text.len() as u32;
        // Room to spare, and room exactly.
        for cap in [exact + 5, exact] {
            let mut v = array("[B", cap, prefix);
            let r = append_int(&mut v, count, i);
            assert_eq!(r, i64::from(exact), "append({i}) cap {cap}: the new count");
            let bytes = raw(&v);
            let at = data + prefix.len();
            assert_eq!(&bytes[data..at], prefix, "the old content is kept");
            assert_eq!(&bytes[at..at + text.len()], text.as_bytes(), "append({i})");
            let end = data + cap as usize; // Cast: a small count
            assert!(
                bytes[at + text.len()..end].iter().all(|&b| b == 0xEE),
                "append({i}): nothing written past the digits"
            );
        }
        // One byte short: the growth is the call's, nothing stored.
        let mut v = array("[B", exact - 1, prefix);
        let before = raw(&v);
        assert_eq!(append_int(&mut v, count, i), -1, "append({i}) needs growth");
        assert_eq!(
            raw(&v),
            before,
            "append({i}): the declined array is untouched"
        );
    }
    // A count past the array (corrupt) declines too.
    let mut v = array("[B", 4, b"abcd");
    assert_eq!(append_int(&mut v, 9, 1), -1);
}

fn site(flag: &u8, guard: &u32, guard_value: u32) -> CharsIrSite {
    CharsIrSite {
        alloc: SbToStringPlan::for_test(
            STRING_CID,
            TOTAL,
            VALUE_OFF,
            // Cast: the guard word's address.
            (guard as *const u32 as usize, guard_value),
        ),
        gc: SbToStringGc {
            zgc_start_bits: false,
            g1_same_region: false,
        },
        thread: ConcatThread::Frame(8),
        cursor_off: 8,
        end_off: 16,
        g1_table: 0,
        class_id_off: 0,
        // Cast: the flag's address.
        compact_flag_addr: flag as *const u8 as usize,
        young_bounds_addr: 0,
        coder: CODER,
        kind_d8: u8::try_from(cratonvm_types::KIND_TAGS_BYTE_OFFSET).expect("disp8"),
        byte_array_tag: cratonvm_types::primitive_array_kind_tags_byte("[B").expect("[B"),
        char_array_tag: cratonvm_types::primitive_array_kind_tags_byte("[C").expect("[C"),
        char_array_mark: 0,
    }
}

/// `toString()` over a TLAB of `tlab_words` with RDX = `value` and ECX =
/// `count`: answers the tail's RAX (-1 on a decline), the TLAB's bytes,
/// its base and how far the cursor moved.
fn to_string(
    s: &CharsIrSite,
    value: u64,
    count: u32,
    tlab_words: usize,
) -> (i64, Vec<u8>, u64, u64) {
    let mut tlab = vec![0u64; tlab_words];
    // Cast: addresses as words.
    let base = tlab.as_mut_ptr() as u64;
    let mut thread = vec![0u64; 4];
    thread[1] = base;
    // Cast: a small count.
    thread[2] = base + 8 * tlab_words as u64;
    // Cast: the thread's address as the frame word.
    let frame = [thread.as_ptr() as u64, 0];
    // SAFETY: one past `frame[0]`, inside the array.
    // SAFETY: one-past-the-end of `frame` (or an element of it): a valid pointer value the
    // emitted code only addresses below, never dereferences at the pointer itself.
    let rbp = unsafe { frame.as_ptr().add(1) } as u64;
    let mut buf = ExecutableBuffer::new(1 << 16).expect("test buffer");
    buf.emit_byte(0x55); // PUSH RBP
    mov_r64_r64(&mut buf, 5, A0); // MOV RBP, a0
    let mut decline = Vec::new();
    // Cast: the fake array's address.
    mov_imm64_full(&mut buf, RDX, value as i64);
    buf.emit_byte(0xB9); // MOV ECX, imm32 (zero-extends)
    buf.emit(&count.to_le_bytes());
    emit_ir_sb_to_string(&mut buf, s, &mut decline);
    buf.emit(&[0x5D, 0xC3]); // POP RBP ; RET
    for p in decline {
        patch_here(&mut buf, p);
    }
    mov_imm32_sx(&mut buf, RAX, -1);
    buf.emit(&[0x5D, 0xC3]); // POP RBP ; RET
    assert!(!buf.overflowed());
    crate::platform::make_executable(buf.as_ptr() as *mut u8, buf.capacity())
        .expect("the test buffer must be flippable to RX");
    // SAFETY: a leaf snippet over the caller's buffers (alive across the
    // call) and caller-saved registers; RBP is saved and restored.
    let f: extern "C" fn(u64, u64) -> i64 = unsafe { std::mem::transmute(buf.as_ptr()) };
    let r = f(rbp, 0);
    let moved = thread[1] - base;
    (r, raw(&tlab), base, moved)
}

fn header_u32(raw: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(raw[at..at + 4].try_into().expect("4 bytes"))
}

#[test]
fn to_string_builds_a_fresh_latin1_string_of_count_bytes() {
    let flag: u8 = 1;
    let guard: u32 = 5;
    let s = site(&flag, &guard, 5);
    let data = cratonvm_types::ARRAY_DATA_OFFSET;
    let len_at = cratonvm_types::ARRAY_LENGTH_OFFSET;
    let text: &[u8] = b"GET /index.html HTTP/1.1 h\xe9llo -2147483648";
    // Capacity past the count: only `count` bytes are copied.
    for count in [1usize, 7, 8, 9, text.len()] {
        // Cast (both): small counts.
        let value = array("[B", text.len() as u32 + 6, text);
        let (r, t, base, moved) = to_string(&s, value.as_ptr() as u64, count as u32, 64);
        assert_ne!(r, -1, "count {count}: not declined");
        assert_eq!(header_u32(&t, len_at) as usize, count, "a byte[count]");
        assert_eq!(
            &t[data..data + count],
            &text[..count],
            "count {count}: the payload"
        );
        let string_at = data + count.div_ceil(8) * 8;
        // Cast: the answer back to an offset into the TLAB.
        assert_eq!(r as u64 - base, string_at as u64, "RAX = the String");
        assert_eq!(header_u32(&t, string_at), STRING_CID);
        let vo = string_at + VALUE_OFF as usize; // Cast: a small offset
        assert_eq!(
            u64::from_le_bytes(t[vo..vo + 8].try_into().expect("8 bytes")),
            base,
            "String.value is the fresh byte[]"
        );
        let co = string_at + CODER.0 as usize; // Cast: a small offset
        assert_eq!(t[co], 0, "coder LATIN1");
        // Cast: small offsets.
        assert_eq!(
            moved as usize,
            string_at + TOTAL as usize,
            "one bump, committed"
        );
    }
    // Declines, each with the cursor untouched: an empty builder, a count
    // past the array, a null value, a char[] value, a replaced String
    // layout, compact strings off, a full TLAB.
    let value = array("[B", 4, b"abcd");
    let chars = array("[C", 4, b"a\0b\0c\0d\0");
    let off: u8 = 0;
    let replaced = site(&flag, &guard, 6);
    let compact_off = site(&off, &guard, 5);
    // Cast: the fake arrays' addresses.
    let (v, c) = (value.as_ptr() as u64, chars.as_ptr() as u64);
    let declines: [(&CharsIrSite, u64, u32, usize, &str); 7] = [
        (&s, v, 0, 64, "empty builder"),
        (&s, v, 5, 64, "count past the array"),
        (&s, 0, 2, 64, "null value"),
        (&s, c, 2, 64, "char[] value"),
        (&replaced, v, 2, 64, "replaced layout"),
        (&compact_off, v, 2, 64, "compact strings off"),
        (&s, v, 4, 4, "full TLAB"),
    ];
    for (site, value, count, tlab_words, what) in declines {
        let (r, _, _, moved) = to_string(site, value, count, tlab_words);
        assert_eq!((r, moved), (-1, 0), "{what}");
    }
}

/// JIT r21 wave 3, lane strings (ZCP-3): [`emit_ir_latin1_to_lower_string`]
/// over the same fake thread / TLAB: a fresh LATIN1 String whose bytes are
/// `CharacterDataLatin1.toLowerCase` of each source byte -- checked for
/// EVERY Latin-1 byte against Rust's `char::to_lowercase` (single-char
/// results; all 256 are) -- and the same declines as `toString()`.
#[test]
fn to_lower_string_maps_every_latin1_byte() {
    let flag: u8 = 1;
    let guard: u32 = 5;
    let s = site(&flag, &guard, 5);
    let data = cratonvm_types::ARRAY_DATA_OFFSET;
    let all: Vec<u8> = (0..=255u8).collect();
    let want: Vec<u8> = all
        .iter()
        .map(|&b| {
            let mut it = char::from(b).to_lowercase();
            match (it.next(), it.next()) {
                // Cast: a Latin-1 lower case is Latin-1.
                (Some(c), None) if (c as u32) < 0x100 => c as u8,
                _ => b,
            }
        })
        .collect();
    for (from, to) in [(0usize, 256usize), (0x40, 0x5C), (0xBE, 0xE1), (0, 1), (0x41, 0x42)] {
        let text = &all[from..to];
        // Cast (both): small counts.
        let value = array("[B", text.len() as u32, text);
        let (r, t, base, _) = to_lower(&s, value.as_ptr() as u64, text.len() as u32, 128);
        assert_ne!(r, -1, "{from:#x}..{to:#x}: not declined");
        assert_eq!(&t[data..data + text.len()], &want[from..to], "{from:#x}..{to:#x}");
        let string_at = data + text.len().div_ceil(8) * 8;
        // Cast: the answer back to an offset into the TLAB.
        assert_eq!(r as u64 - base, string_at as u64, "RAX = the String");
        let co = string_at + CODER.0 as usize; // Cast: a small offset
        assert_eq!(t[co], 0, "coder LATIN1");
    }
    let value = array("[B", 4, b"ABCD");
    let off: u8 = 0;
    let compact_off = site(&off, &guard, 5);
    // Cast: the fake array's address.
    let v = value.as_ptr() as u64;
    for (site, count, tlab_words, what) in [
        (&s, 4u32, 4usize, "full TLAB"),
        (&compact_off, 4, 64, "compact strings off"),
        (&s, 5, 64, "count past the array"),
    ] {
        let (r, _, _, moved) = to_lower(site, v, count, tlab_words);
        assert_eq!((r, moved), (-1, 0), "{what}");
    }
}

/// Round rx27, lane splitfast: [`emit_ir_latin1_substring`] over `value`
/// with RAX = `recv`, R10D = `begin`, R9D = `end` (the prefix's entry
/// contract): answers RAX (-1 on a decline), the TLAB's bytes, its base and
/// how far the cursor moved.
fn substring(
    s: &CharsIrSite,
    recv: u64,
    value: u64,
    begin: i32,
    end: i32,
    tlab_words: usize,
) -> (i64, Vec<u8>, u64, u64) {
    let mut tlab = vec![0u64; tlab_words];
    // Cast: addresses as words.
    let base = tlab.as_mut_ptr() as u64;
    let mut thread = vec![0u64; 4];
    thread[1] = base;
    // Cast: a small count.
    thread[2] = base + 8 * tlab_words as u64;
    // Cast: the thread's address as the frame word.
    let frame = [thread.as_ptr() as u64, 0];
    // SAFETY: one past `frame[0]`, inside the array.
    // SAFETY: one-past-the-end of `frame` (or an element of it): a valid pointer value the
    // emitted code only addresses below, never dereferences at the pointer itself.
    let rbp = unsafe { frame.as_ptr().add(1) } as u64;
    let mut buf = ExecutableBuffer::new(1 << 16).expect("test buffer");
    buf.emit_byte(0x55); // PUSH RBP
    mov_r64_r64(&mut buf, 5, A0); // MOV RBP, a0
    let mut decline = Vec::new();
    // Cast (all): the fake objects' addresses and the int operands' bits.
    mov_imm64_full(&mut buf, RAX, recv as i64);
    mov_imm64_full(&mut buf, RCX, value as i64);
    mov_imm64_full(&mut buf, R10, i64::from(begin));
    mov_imm64_full(&mut buf, R9, i64::from(end));
    emit_ir_latin1_substring(&mut buf, s, &mut decline);
    buf.emit(&[0x5D, 0xC3]); // POP RBP ; RET
    for p in decline {
        patch_here(&mut buf, p);
    }
    mov_imm32_sx(&mut buf, RAX, -1);
    buf.emit(&[0x5D, 0xC3]); // POP RBP ; RET
    assert!(!buf.overflowed());
    crate::platform::make_executable(buf.as_ptr() as *mut u8, buf.capacity())
        .expect("the test buffer must be flippable to RX");
    // SAFETY: a leaf snippet over the caller's buffers (alive across the
    // call) and caller-saved registers; RBP is saved and restored.
    let f: extern "C" fn(u64, u64) -> i64 = unsafe { std::mem::transmute(buf.as_ptr()) };
    let r = f(rbp, 0);
    let moved = thread[1] - base;
    (r, raw(&tlab), base, moved)
}

/// Round rx27, lane splitfast: every in-range `(begin, end)` of a LATIN1
/// value answers a fresh String of exactly `value[begin..end]` (one bump),
/// the whole range answers the receiver with nothing allocated, and every
/// case JDK 25's `substring` throws for, the empty range (`""`) and the
/// allocation's own declines leave RAX = -1 and the cursor untouched.
#[test]
fn rx27_substring_copies_exactly_the_range_or_declines() {
    const RECV: u64 = 0x5EC0_0000_1234_5678;
    let flag: u8 = 1;
    let guard: u32 = 5;
    let s = site(&flag, &guard, 5);
    let data = cratonvm_types::ARRAY_DATA_OFFSET;
    let len_at = cratonvm_types::ARRAY_LENGTH_OFFSET;
    let text: &[u8] = b"alpha,beta,gamma,delta \xe9\xff!";
    let n = text.len();
    // Cast: a small count.
    let value = array("[B", n as u32, text);
    // Cast: the fake array's address.
    let v = value.as_ptr() as u64;
    for begin in 0..=n {
        for end in begin..=n {
            // Cast (both): small indices.
            let (b, e) = (begin as i32, end as i32);
            let (r, t, base, moved) = substring(&s, RECV, v, b, e, 64);
            if begin == 0 && end == n {
                assert_eq!((r as u64, moved), (RECV, 0), "whole range: `this`");
                continue;
            }
            if begin == end {
                assert_eq!((r, moved), (-1, 0), "({begin}, {end}): \"\" is the call's");
                continue;
            }
            let count = end - begin;
            assert_ne!(r, -1, "({begin}, {end}): not declined");
            assert_eq!(
                header_u32(&t, len_at) as usize,
                count,
                "a byte[end - begin]"
            );
            assert_eq!(
                &t[data..data + count],
                &text[begin..end],
                "({begin}, {end}): the payload"
            );
            assert!(
                t[data + count..data + count.div_ceil(8) * 8]
                    .iter()
                    .all(|&x| x == 0),
                "({begin}, {end}): nothing copied past the range"
            );
            let string_at = data + count.div_ceil(8) * 8;
            // Cast: the answer back to an offset into the TLAB.
            assert_eq!(r as u64 - base, string_at as u64, "RAX = the String");
            assert_eq!(header_u32(&t, string_at), STRING_CID);
            let vo = string_at + VALUE_OFF as usize; // Cast: a small offset
            assert_eq!(
                u64::from_le_bytes(t[vo..vo + 8].try_into().expect("8 bytes")),
                base,
                "String.value is the fresh byte[]"
            );
            let co = string_at + CODER.0 as usize; // Cast: a small offset
            assert_eq!(t[co], 0, "coder LATIN1");
            // Cast: small offsets.
            assert_eq!(moved as usize, string_at + TOTAL as usize, "one bump");
        }
    }
    // Every throwing range, and the allocation's declines.
    let chars = array("[C", 4, b"a\0b\0c\0d\0");
    let off: u8 = 0;
    let replaced = site(&flag, &guard, 6);
    let compact_off = site(&off, &guard, 5);
    // Cast: the fake array's address, and small counts.
    let (c, ni) = (chars.as_ptr() as u64, n as i32);
    let declines: [(&CharsIrSite, u64, i32, i32, usize, &str); 12] = [
        (&s, v, -1, 2, 64, "begin < 0"),
        (&s, v, 0, ni + 1, 64, "end > length"),
        (&s, v, 1, -1, 64, "end < 0"),
        (&s, v, 3, 2, 64, "begin > end"),
        (&s, v, i32::MIN, 2, 64, "begin MIN_VALUE"),
        (&s, v, 1, i32::MAX, 64, "end MAX_VALUE"),
        (&s, 0, 0, 1, 64, "null value"),
        (&s, c, 0, 1, 64, "char[] value"),
        (&replaced, v, 1, 3, 64, "replaced layout"),
        (&compact_off, v, 1, 3, 64, "compact strings off"),
        (&s, v, 1, 3, 4, "full TLAB"),
        (&s, v, ni, ni, 64, "empty at the end"),
    ];
    for (site, value, b, e, tlab_words, what) in declines {
        let (r, _, _, moved) = substring(site, RECV, value, b, e, tlab_words);
        assert_eq!((r, moved), (-1, 0), "{what}");
    }
}

/// Round rx27 wave 3, lane splitfast: [`emit_ir_byte_copy_of_range`] with
/// RDX = `value`, R10D = `from`, R9D = `to`: answers RAX (-1 on a
/// decline), the TLAB's bytes, its base and how far the cursor moved.
fn copy_of_range(
    s: &CharsIrSite,
    value: u64,
    from: i32,
    to: i32,
    tlab_words: usize,
) -> (i64, Vec<u8>, u64, u64) {
    let mut tlab = vec![0u64; tlab_words];
    // Cast: addresses as words.
    let base = tlab.as_mut_ptr() as u64;
    let mut thread = vec![0u64; 4];
    thread[1] = base;
    // Cast: a small count.
    thread[2] = base + 8 * tlab_words as u64;
    // Cast: the thread's address as the frame word.
    let frame = [thread.as_ptr() as u64, 0];
    // SAFETY: one past `frame[0]`, inside the array.
    // SAFETY: one-past-the-end of `frame` (or an element of it): a valid pointer value the
    // emitted code only addresses below, never dereferences at the pointer itself.
    let rbp = unsafe { frame.as_ptr().add(1) } as u64;
    let mut buf = ExecutableBuffer::new(1 << 16).expect("test buffer");
    buf.emit_byte(0x55); // PUSH RBP
    mov_r64_r64(&mut buf, 5, A0); // MOV RBP, a0
    let mut decline = Vec::new();
    // Cast (all): the fake array's address and the int operands' bits.
    mov_imm64_full(&mut buf, RDX, value as i64);
    mov_imm64_full(&mut buf, R10, i64::from(from));
    mov_imm64_full(&mut buf, R9, i64::from(to));
    emit_ir_byte_copy_of_range(&mut buf, s, &mut decline);
    buf.emit(&[0x5D, 0xC3]); // POP RBP ; RET
    for p in decline {
        patch_here(&mut buf, p);
    }
    mov_imm32_sx(&mut buf, RAX, -1);
    buf.emit(&[0x5D, 0xC3]); // POP RBP ; RET
    assert!(!buf.overflowed());
    crate::platform::make_executable(buf.as_ptr() as *mut u8, buf.capacity())
        .expect("the test buffer must be flippable to RX");
    // SAFETY: a leaf snippet over the caller's buffers (alive across the
    // call) and caller-saved registers; RBP is saved and restored.
    let f: extern "C" fn(u64, u64) -> i64 = unsafe { std::mem::transmute(buf.as_ptr()) };
    let r = f(rbp, 0);
    let moved = thread[1] - base;
    (r, raw(&tlab), base, moved)
}

/// Round rx27 wave 3, lane splitfast: every `0 <= from <= to <= length`
/// (the whole array and empty ranges included) answers a fresh `byte[to -
/// from]` of exactly `value[from..to]` in one bump; every input JDK 25's
/// body throws for, the zero-padded `to > length` and the allocation's own
/// declines leave RAX = -1 and the cursor untouched.
#[test]
fn rx27_copy_of_range_copies_exactly_the_range_or_declines() {
    let flag: u8 = 1;
    let guard: u32 = 5;
    let s = site(&flag, &guard, 5);
    let data = cratonvm_types::ARRAY_DATA_OFFSET;
    let len_at = cratonvm_types::ARRAY_LENGTH_OFFSET;
    let bytes: Vec<u8> = (0..21u8)
        .map(|i| i.wrapping_mul(37).wrapping_add(0x90))
        .collect();
    let n = bytes.len();
    // Cast: a small count.
    let value = array("[B", n as u32, &bytes);
    // Cast: the fake array's address.
    let v = value.as_ptr() as u64;
    for from in 0..=n {
        for to in from..=n {
            // Cast (both): small indices.
            let (r, t, base, moved) = copy_of_range(&s, v, from as i32, to as i32, 64);
            let count = to - from;
            assert_ne!(r, -1, "({from}, {to}): not declined");
            // Cast: the answer back to an offset into the TLAB.
            assert_eq!(r as u64, base, "({from}, {to}): RAX = the fresh array");
            assert_eq!(header_u32(&t, len_at) as usize, count, "a byte[to - from]");
            assert_eq!(
                &t[data..data + count],
                &bytes[from..to],
                "({from}, {to}): the payload"
            );
            // Cast: a small offset.
            assert_eq!(moved as usize, data + count.div_ceil(8) * 8, "one bump");
        }
    }
    let chars = array("[C", 4, b"a\0b\0c\0d\0");
    // Cast: the fake array's address, and a small count.
    let (c, ni) = (chars.as_ptr() as u64, n as i32);
    let declines: [(u64, i32, i32, usize, &str); 9] = [
        (v, 3, 2, 64, "from > to"),
        (v, 0, -1, 64, "to < 0 = from > to"),
        (v, -1, 2, 64, "from < 0"),
        (v, -1, -1, 64, "from < 0, empty"),
        (v, 2, ni + 1, 64, "to > length: zero padding"),
        (v, ni + 1, ni + 2, 64, "from > length"),
        (0, 0, 1, 64, "null"),
        (c, 0, 1, 64, "char[]"),
        (v, 0, ni, 4, "full TLAB"),
    ];
    for (value, from, to, tlab_words, what) in declines {
        let (r, _, _, moved) = copy_of_range(&s, value, from, to, tlab_words);
        assert_eq!((r, moved), (-1, 0), "{what}");
    }
}

/// [`to_string`] with the lower-case emitter.
fn to_lower(
    s: &CharsIrSite,
    value: u64,
    count: u32,
    tlab_words: usize,
) -> (i64, Vec<u8>, u64, u64) {
    let mut tlab = vec![0u64; tlab_words];
    // Cast: addresses as words.
    let base = tlab.as_mut_ptr() as u64;
    let mut thread = vec![0u64; 4];
    thread[1] = base;
    // Cast: a small count.
    thread[2] = base + 8 * tlab_words as u64;
    // Cast: the thread's address as the frame word.
    let frame = [thread.as_ptr() as u64, 0];
    // SAFETY: one past `frame[0]`, inside the array.
    // SAFETY: one-past-the-end of `frame` (or an element of it): a valid pointer value the
    // emitted code only addresses below, never dereferences at the pointer itself.
    let rbp = unsafe { frame.as_ptr().add(1) } as u64;
    let mut buf = ExecutableBuffer::new(1 << 16).expect("test buffer");
    buf.emit_byte(0x55); // PUSH RBP
    mov_r64_r64(&mut buf, 5, A0); // MOV RBP, a0
    let mut decline = Vec::new();
    // Cast: the fake array's address.
    mov_imm64_full(&mut buf, RDX, value as i64);
    buf.emit_byte(0xB9); // MOV ECX, imm32 (zero-extends)
    buf.emit(&count.to_le_bytes());
    emit_ir_latin1_to_lower_string(&mut buf, s, &mut decline);
    buf.emit(&[0x5D, 0xC3]); // POP RBP ; RET
    for p in decline {
        patch_here(&mut buf, p);
    }
    mov_imm32_sx(&mut buf, RAX, -1);
    buf.emit(&[0x5D, 0xC3]); // POP RBP ; RET
    assert!(!buf.overflowed());
    crate::platform::make_executable(buf.as_ptr() as *mut u8, buf.capacity())
        .expect("the test buffer must be flippable to RX");
    // SAFETY: a leaf snippet over the caller's buffers (alive across the
    // call) and caller-saved registers; RBP is saved and restored.
    let f: extern "C" fn(u64, u64) -> i64 = unsafe { std::mem::transmute(buf.as_ptr()) };
    let r = f(rbp, 0);
    let moved = thread[1] - base;
    (r, raw(&tlab), base, moved)
}
