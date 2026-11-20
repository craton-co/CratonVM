use super::*;
use crate::concat_inline_plan::JitConcatInlinePlan;

#[cfg(windows)]
const A0: u8 = 1; // RCX
#[cfg(windows)]
const A1: u8 = 2; // RDX
#[cfg(not(windows))]
const A0: u8 = 7; // RDI
#[cfg(not(windows))]
const A1: u8 = 6; // RSI

const STRING_CID: u32 = 77;
const CLASS_ID_OFF: i32 = 0;

/// `emit_copy` copies exactly `n` bytes for every `n` across the qword
/// loop and the 4/2/1 tail, writes nothing past them, and leaves R8 at the
/// destination's end.
#[test]
fn emit_copy_copies_exactly_n_bytes() {
    for n in 0..=20i32 {
        let mut buf = ExecutableBuffer::new(1 << 12).expect("test buffer");
        mov_r64_r64(&mut buf, R8, A1);
        mov_r64_r64(&mut buf, RDX, A0);
        mov_imm32_sx(&mut buf, RCX, n);
        emit_copy(&mut buf);
        mov_r64_r64(&mut buf, RAX, R8);
        buf.emit_byte(0xC3); // RET
        assert!(!buf.overflowed());
        crate::platform::make_executable(buf.as_ptr() as *mut u8, buf.capacity())
            .expect("the test buffer must be flippable to RX");
        let src: Vec<u8> = (1..=40).collect();
        let mut dst = vec![0xEEu8; 40];
        // SAFETY: a leaf snippet copying `n` (< 40) bytes between the two
        // live vectors through caller-saved registers only.
        let f: extern "C" fn(u64, u64) -> u64 = unsafe { std::mem::transmute(buf.as_ptr()) };
        // Casts (all): the vectors' addresses and a small length.
        let end = f(src.as_ptr() as u64, dst.as_mut_ptr() as u64);
        let n = n as usize;
        assert_eq!(end, dst.as_ptr() as u64 + n as u64, "R8 ends past the copy (n={n})");
        assert_eq!(&dst[..n], &src[..n], "n={n}");
        assert!(dst[n..].iter().all(|&b| b == 0xEE), "nothing past n={n}");
    }
}

pub(super) fn layout() -> crate::StringFieldLayout {
    crate::StringFieldLayout {
        value_field_index: 0,
        value_compact_offset: 16,
        value_compact_is_narrow: false,
        value_legacy_offset: 40,
        hash_field_index: 1,
        hash_compact_offset: 28,
        hash_legacy_offset: 56,
        has_coder: true,
        coder_field_index: 2,
        coder_compact_offset: 24,
        coder_legacy_offset: 48,
        coder_compact_is_byte: true,
        string_class_id: STRING_CID,
        builder: None,
        array_list: None,
        hash_is_zero_compact_offset: None,
        locale_language: None,
    }
}

fn units(s: &str) -> Vec<u16> {
    s.encode_utf16().collect()
}

fn plan(recipe: &str, desc: &str) -> &'static JitConcatInlinePlan {
    Box::leak(Box::new(
        JitConcatInlinePlan::from_recipe(&units(recipe), &[], desc).expect("admitted"),
    ))
}

/// A fake `byte[]` holding `bytes` (64-bit words, 8-aligned).
fn byte_array(bytes: &[u8]) -> Vec<u64> {
    let data = cratonvm_types::ARRAY_DATA_OFFSET;
    let mut raw = vec![0u8; data + bytes.len() + 8];
    raw[cratonvm_types::KIND_TAGS_BYTE_OFFSET] =
        cratonvm_types::primitive_array_kind_tags_byte("[B").expect("byte[] tag");
    let len_at = cratonvm_types::ARRAY_LENGTH_OFFSET;
    // Cast: a small test length.
    raw[len_at..len_at + 4].copy_from_slice(&(bytes.len() as u32).to_le_bytes());
    raw[data..data + bytes.len()].copy_from_slice(bytes);
    words(&raw)
}

fn words(raw: &[u8]) -> Vec<u64> {
    let mut w = vec![0u64; raw.len().div_ceil(8)];
    for (i, b) in raw.iter().enumerate() {
        w[i / 8] |= u64::from(*b) << ((i % 8) * 8);
    }
    w
}

/// A fake `String` over `value`: compact (offsets 16/24) or legacy
/// (40/48), `coder`, class `cid`.
fn string_obj(value: &[u64], coder: u8, compact: bool, cid: u32) -> Vec<u64> {
    let mut raw = vec![0u8; 64];
    raw[0..4].copy_from_slice(&cid.to_le_bytes());
    if compact {
        raw[cratonvm_types::GC_FLAGS_BYTE_OFFSET] = cratonvm_types::GC_FLAG_COMPACT;
    }
    let (v_off, c_off) = if compact { (16, 24) } else { (40, 48) };
    // Cast: the value array's address as a word.
    let addr = value.as_ptr() as u64;
    raw[v_off..v_off + 8].copy_from_slice(&addr.to_le_bytes());
    raw[c_off] = coder;
    words(&raw)
}

fn shape(p: &'static JitConcatInlinePlan, flag: &u8, n: usize) -> ConcatPassShape {
    ConcatPassShape {
        plan: p,
        outer: layout(),
        // Cast: the flag's address.
        compact_flag_addr: flag as *const u8 as usize,
        // Cast: small offsets.
        arg_offsets: (0..n).map(|i| 8 * (i as i32 + 1)).collect(),
        class_id_off: CLASS_ID_OFF,
        len_d8: u8::try_from(cratonvm_types::ARRAY_LENGTH_OFFSET).expect("disp8"),
        data_d8: u8::try_from(cratonvm_types::ARRAY_DATA_OFFSET).expect("disp8"),
        max_count: 4096,
        boxed_int: None,
        boxed_layouts: ConcatBoxedLayouts::default(),
    }
}

/// Emit `body` as `extern "C" fn(rbp, a1) -> i64` (RBP = `rbp`, R9 = `a1`
/// on entry), with every `decline` edge answering `-1`, and run it.
fn run(body: impl Fn(&mut ExecutableBuffer, &mut Vec<usize>), rbp: u64, a1: u64) -> i64 {
    let mut buf = ExecutableBuffer::new(1 << 16).expect("test buffer");
    buf.emit_byte(0x55); // PUSH RBP
    mov_r64_r64(&mut buf, 5, A0); // MOV RBP, a0
    mov_r64_r64(&mut buf, R9, A1);
    let mut decline = Vec::new();
    body(&mut buf, &mut decline);
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
    f(rbp, a1)
}

/// The two passes over `args` (frame words, operand 0 first): the text
/// written, or `None` when the length pass declined. The length pass's
/// ECX is checked against the bytes the fill wrote.
fn render(p: &'static JitConcatInlinePlan, args: &[u64], compact_on: bool) -> Option<Vec<u8>> {
    let flag: u8 = u8::from(compact_on);
    let s = shape(p, &flag, args.len());
    // frame[n - 1 - i] = operand i, RBP one past the end: [rbp - 8(i+1)].
    let mut frame = vec![0u64; args.len() + 1];
    for (i, a) in args.iter().enumerate() {
        frame[args.len() - 1 - i] = *a;
    }
    let data = cratonvm_types::ARRAY_DATA_OFFSET;
    let mut dst = vec![0xAAAA_AAAA_AAAA_AAAAu64; 512];
    // Cast: the frame top and the destination array as words.
    // SAFETY: the frame has args.len() + 1 elements, so this offset is in bounds.
    let rbp = unsafe { frame.as_ptr().add(args.len()) } as u64;
    let dst_addr = dst.as_mut_ptr() as u64;
    let written = run(
        |b, d| {
            emit_length_pass(b, &s, d);
            // The array "length" := ECX, then the fill into R11 = dst.
            b.emit(&[0x41, 0x89, 0x49, s.len_d8]); // MOV [R9+len],ECX
            mov_r64_r64(b, R11, R9);
            emit_fill_pass(b, &s);
            mov_r64_r64(b, RAX, R8);
            b.emit(&[0x4C, 0x29, 0xD8]); // SUB RAX,R11
        },
        rbp,
        dst_addr,
    );
    if written < 0 {
        return None;
    }
    // Cast: a small non-negative count.
    let n = written as usize - data;
    let raw: Vec<u8> = dst.iter().flat_map(|w| w.to_le_bytes()).collect();
    let len_at = cratonvm_types::ARRAY_LENGTH_OFFSET;
    let ecx = u32::from_le_bytes(raw[len_at..len_at + 4].try_into().expect("4 bytes"));
    assert_eq!(
        ecx as usize, n,
        "the length pass's count is the bytes the fill wrote"
    );
    assert!(
        raw[data + n..data + n + 16].iter().all(|&b| b == 0xAA),
        "wrote past the result"
    );
    Some(raw[data..data + n].to_vec())
}

#[test]
fn every_operand_kind_renders_as_hotspot_spells_it() {
    let p = plan(
        "i=\u{1} s=\u{1} j=\u{1} c=\u{1} z=\u{1} n=\u{1} f=\u{1}.",
        "(ILjava/lang/String;JCZLjava/lang/String;Z)Ljava/lang/String;",
    );
    let hello = byte_array(b"h\xe9llo world!");
    let s = string_obj(&hello, 0, true, STRING_CID);
    for (i, j) in [
        (0i32, 0i64),
        (-1, -1),
        (i32::MIN, i64::MIN),
        (i32::MAX, i64::MAX),
        (1_000_000_000, 10_000_000_000),
    ] {
        let args = [
            // Cast: an int operand's bits under garbage in the high half.
            (0xDEAD_0000_0000_0000u64) | u64::from(i as u32),
            s.as_ptr() as u64,
            // Cast: the long's bits.
            j as u64,
            0xFFFF_0000_0000_0041u64, // 'A' under garbage
            0x5A5A_0000_0000_0001u64, // true
            0,                        // null
            0x5A5A_0000_0000_0000u64, // false (low word zero)
        ];
        let got = render(p, &args, true).expect("not declined");
        // The result is Latin-1 bytes (the 0xE9 stays one byte).
        let mut want = format!("i={i} s=").into_bytes();
        want.extend_from_slice(b"h\xe9llo world!");
        want.extend(format!(" j={j} c=A z=true n=null f=false.").bytes());
        assert_eq!(got, want, "{i} {j}");
    }
}

#[test]
fn a_legacy_layout_string_is_read_through_its_legacy_offsets() {
    let p = plan("<\u{1}>", "(Ljava/lang/String;)Ljava/lang/String;");
    let v = byte_array(b"legacy");
    let s = string_obj(&v, 0, false, STRING_CID);
    assert_eq!(
        render(p, &[s.as_ptr() as u64], true),
        Some(b"<legacy>".to_vec())
    );
}

#[test]
fn the_length_pass_declines_what_the_fill_cannot_write() {
    let p = plan("x\u{1}", "(Ljava/lang/String;)Ljava/lang/String;");
    let v = byte_array(b"abc");
    // UTF16 coder, a foreign class id, a null value: the bridge.
    let utf16 = string_obj(&v, 1, true, STRING_CID);
    let other = string_obj(&v, 0, true, STRING_CID + 1);
    let no_value = {
        let mut s = string_obj(&v, 0, true, STRING_CID);
        s[2] = 0;
        s
    };
    for (what, s) in [("utf16", &utf16), ("class", &other), ("value", &no_value)] {
        assert_eq!(render(p, &[s.as_ptr() as u64], true), None, "{what}");
    }
    // A non-byte[] value (a char[] tag).
    let mut chars = byte_array(b"abc");
    let tag_word = cratonvm_types::KIND_TAGS_BYTE_OFFSET / 8;
    let tag_shift = (cratonvm_types::KIND_TAGS_BYTE_OFFSET % 8) * 8;
    chars[tag_word] ^= 0x01 << tag_shift;
    let s = string_obj(&chars, 0, true, STRING_CID);
    assert_eq!(render(p, &[s.as_ptr() as u64], true), None, "kind tag");
    // Compact strings off: always the bridge.
    let ok = string_obj(&v, 0, true, STRING_CID);
    assert_eq!(render(p, &[ok.as_ptr() as u64], false), None, "compact off");
    assert_eq!(
        render(p, &[ok.as_ptr() as u64], true),
        Some(b"xabc".to_vec())
    );
    // A char above 0xFF.
    let pc = plan("\u{1}!", "(C)Ljava/lang/String;");
    assert_eq!(render(pc, &[0x100], true), None, "wide char");
    assert_eq!(render(pc, &[0xFF], true), Some(vec![0xFF, b'!']));
    // An empty result (no literal text, an empty String operand).
    let pe = plan("\u{1}", "(Ljava/lang/String;)Ljava/lang/String;");
    let empty = byte_array(b"");
    let es = string_obj(&empty, 0, true, STRING_CID);
    assert_eq!(render(pe, &[es.as_ptr() as u64], true), None, "empty");
}

/// The optimizing tier's whole tail, executed: one bump for the `byte[]`
/// and the String, both headers, the String's zeroed body and `value`, the
/// text, the commit, and RAX = the String; a full TLAB, a moved replace
/// count and a null thread decline with the cursor untouched.
#[test]
fn the_ir_tail_builds_both_objects_in_one_bump() {
    let p = plan("v=\u{1}/\u{1}", "(ILjava/lang/String;)Ljava/lang/String;");
    let flag: u8 = 1;
    let guard_word: u32 = 5;
    let total = 32i32;
    let value_off = 16i32;
    let run_tail = |tlab_words: usize, guard_value: u32, thread_null: bool| {
        let mut tlab = vec![0u64; tlab_words];
        // A fake thread: cursor at 8, end at 16.
        let mut thread = vec![0u64; 4];
        // Cast: addresses as words.
        let base = tlab.as_mut_ptr() as u64;
        thread[1] = base;
        thread[2] = base + 8 * tlab_words as u64;
        let v = byte_array(b"xyz");
        let s = string_obj(&v, 0, true, STRING_CID);
        let mut frame = vec![0u64; 4];
        // [rbp-8] = operand 0, [rbp-16] = operand 1, [rbp-24] = thread.
        frame[3] = 42;
        frame[2] = s.as_ptr() as u64;
        frame[1] = if thread_null {
            0
        } else {
            thread.as_ptr() as u64
        };
        // SAFETY: one-past-the-end of `frame` (or an element of it): a valid pointer value the
        // emitted code only addresses below, never dereferences at the pointer itself.
        let rbp = unsafe { frame.as_ptr().add(4) } as u64;
        let site = ConcatIrSite {
            shape: shape(p, &flag, 2),
            alloc: SbToStringPlan::for_test(
                STRING_CID,
                total,
                value_off,
                (&guard_word as *const u32 as usize, guard_value),
            ),
            gc: SbToStringGc {
                zgc_start_bits: false,
                g1_same_region: false,
            },
            thread: ConcatThread::Frame(24),
            cursor_off: 8,
            end_off: 16,
            g1_table: 0,
        };
        let r = run(|b, d| emit_ir_tail(b, &site, d), rbp, 0);
        (r, tlab, thread[1] - base)
    };
    let (r, tlab, advanced) = run_tail(64, 5, false);
    assert!(r > 0, "not declined");
    let raw: Vec<u8> = tlab.iter().flat_map(|w| w.to_le_bytes()).collect();
    let data = cratonvm_types::ARRAY_DATA_OFFSET;
    // The byte[8] "v=42/xyz" at the cursor, then the String at data + 8.
    let text = b"v=42/xyz";
    let len_at = cratonvm_types::ARRAY_LENGTH_OFFSET;
    assert_eq!(&raw[len_at..len_at + 4], &(text.len() as u32).to_le_bytes());
    assert_eq!(&raw[data..data + text.len()], text);
    let string_at = data + 8;
    // Cast: the answer back to an offset into the TLAB.
    assert_eq!(r as u64 - tlab.as_ptr() as u64, string_at as u64);
    assert_eq!(&raw[string_at..string_at + 4], &STRING_CID.to_le_bytes());
    let flags = raw[string_at + cratonvm_types::GC_FLAGS_BYTE_OFFSET];
    assert_eq!(
        flags,
        cratonvm_types::GC_FLAG_COMPACT | cratonvm_types::GC_FLAG_HEADER
    );
    let vo = string_at + value_off as usize;
    assert_eq!(
        u64::from_le_bytes(raw[vo..vo + 8].try_into().expect("8 bytes")),
        tlab.as_ptr() as u64,
        "String.value is the fresh byte[]"
    );
    assert_eq!(
        advanced,
        (string_at + total as usize) as u64,
        "one bump, committed"
    );
    // A full TLAB, a moved layout and no thread: declined, cursor untouched.
    for (words, guard, null_thread) in [(4usize, 5u32, false), (64, 6, false), (64, 5, true)] {
        let (r, _, advanced) = run_tail(words, guard, null_thread);
        assert_eq!((r, advanced), (-1, 0), "{words} {guard} {null_thread}");
    }
}


/// `"" + b` is the one unary shape HotSpot answers with a literal: the IR
/// twin declines it; a boolean beside literal text, or a unary `int`,
/// stays admitted.
#[test]
fn the_unary_boolean_concat_keeps_the_bridge() {
    let frame = ConcatIrFrame {
        tls_disp: 0,
        thread_slot_off: 24,
        cursor_off: 8,
        end_off: 16,
        class_id_off: 0,
        g1_table: 0,
        region_bounds_addr: 0,
    };
    let unary = plan("\u{1}", "(Z)Ljava/lang/String;");
    assert!(is_unary_boolean(unary));
    let why = concat_ir_site(Some(unary), Some(layout()), Some(1), vec![8], frame, ConcatBoxedLayouts::default())
        .expect_err("declined");
    assert!(why.contains("unary boolean"), "{why}");
    assert!(!is_unary_boolean(plan("b=\u{1}", "(Z)Ljava/lang/String;")));
    assert!(!is_unary_boolean(plan("\u{1}", "(I)Ljava/lang/String;")));
    assert!(!is_unary_boolean(plan(
        "\u{1}\u{1}",
        "(ZZ)Ljava/lang/String;"
    )));
}

/// UTF-16LE bytes of a Rust string (the expected result payload).
fn u16le(text: &str) -> Vec<u8> {
    text.encode_utf16().flat_map(|u| u.to_le_bytes()).collect()
}

/// STRGAP4-4: a wide plan's two passes write UTF-16LE units with the same
/// spellings as the Latin-1 passes -- a LATIN1 String inflated, a UTF16
/// String copied, a `char` above 0xFF inline, `null`, booleans, the int
/// edges -- and the length pass's ECX is the byte count the fill wrote.
#[test]
fn the_wide_passes_render_utf16_as_hotspot_spells_it() {
    let p = plan(
        "\u{3a9}=\u{1} s=\u{1} w=\u{1} c=\u{1} z=\u{1} n=\u{1}.",
        "(ILjava/lang/String;Ljava/lang/String;CZLjava/lang/String;)Ljava/lang/String;",
    );
    assert!(p.wide);
    let latin = byte_array(b"h\xe9llo world!");
    let ls = string_obj(&latin, 0, true, STRING_CID);
    let wide_bytes = u16le("\u{3a9}mega\u{4e2d}");
    let wide = byte_array(&wide_bytes);
    let ws = string_obj(&wide, 1, true, STRING_CID);
    let legacy_wide = string_obj(&wide, 1, false, STRING_CID);
    for (i, w) in [
        (0i32, &ws),
        (-1, &legacy_wide),
        (i32::MIN, &ws),
        (i32::MAX, &ws),
        (1_000_000_000, &legacy_wide),
    ] {
        let args = [
            // Cast: an int operand's bits under garbage in the high half.
            0xDEAD_0000_0000_0000u64 | u64::from(i as u32),
            ls.as_ptr() as u64,
            w.as_ptr() as u64,
            0xFFFF_0000_0000_03A9u64, // the char U+03A9 under garbage
            0x5A5A_0000_0000_0001u64, // true
            0,                        // null
        ];
        let got = render(p, &args, true).expect("not declined");
        let want = u16le(&format!(
            "\u{3a9}={i} s=h\u{e9}llo world! w=\u{3a9}mega\u{4e2d} c=\u{3a9} z=true n=null."
        ));
        assert_eq!(got, want, "{i}");
    }
    // A long operand's edges and a false boolean.
    let pl = plan("\u{3a9}\u{1}|\u{1}", "(JZ)Ljava/lang/String;");
    for j in [0i64, -7, i64::MIN, i64::MAX] {
        // Cast: the long's bits.
        let got = render(pl, &[j as u64, 0x5A5A_0000_0000_0000], true).expect("not declined");
        assert_eq!(got, u16le(&format!("\u{3a9}{j}|false")), "{j}");
    }
}

/// STRGAP4-4: what the wide length pass still refuses, before any store:
/// compact strings off, a coder that is neither LATIN1 nor UTF16, a
/// foreign class, a null value, a result past the cap (in units).
#[test]
fn the_wide_length_pass_declines_what_it_cannot_write() {
    let p = plan("\u{3a9}\u{1}", "(Ljava/lang/String;)Ljava/lang/String;");
    let v = byte_array(b"abcd");
    let ok = string_obj(&v, 0, true, STRING_CID);
    assert_eq!(render(p, &[ok.as_ptr() as u64], false), None, "compact off");
    let bad_coder = string_obj(&v, 2, true, STRING_CID);
    let other = string_obj(&v, 0, true, STRING_CID + 1);
    let no_value = {
        let mut s = string_obj(&v, 0, true, STRING_CID);
        s[2] = 0;
        s
    };
    for (what, s) in [
        ("coder", &bad_coder),
        ("class", &other),
        ("value", &no_value),
    ] {
        assert_eq!(render(p, &[s.as_ptr() as u64], true), None, "{what}");
    }
    assert_eq!(
        render(p, &[ok.as_ptr() as u64], true),
        Some(u16le("\u{3a9}abcd"))
    );
    // 2049 units are 4098 bytes, past the 4096-byte cap (declined before
    // the fill, so the test's 4 KiB destination is never written).
    let over = byte_array(&vec![b'a'; 2048]);
    let os = string_obj(&over, 0, true, STRING_CID);
    assert_eq!(render(p, &[os.as_ptr() as u64], true), None, "past the cap");
}




/// The two passes over `args` with an optional CONCAT21-4 boxed-`Integer`
/// shape and the CONCAT21-3 FP prologue / restore around them (every
/// prologue decline through a restore stub, as both tiers wire it). R9 is
/// re-derived after the prologue: its leaf call clobbers it.
fn render_full(
    p: &'static JitConcatInlinePlan,
    args: &[u64],
    boxed: Option<ConcatBoxedInt>,
) -> Option<Vec<u8>> {
    let flag: u8 = 1;
    let mut s = shape(p, &flag, args.len());
    s.boxed_int = boxed;
    let mut frame = vec![0u64; args.len() + 1];
    for (i, a) in args.iter().enumerate() {
        frame[args.len() - 1 - i] = *a;
    }
    let data = cratonvm_types::ARRAY_DATA_OFFSET;
    let mut dst = vec![0xAAAA_AAAA_AAAA_AAAAu64; 512];
    // Cast: the frame top and the destination array as words.
    // SAFETY: the frame has args.len() + 1 elements, so this offset is in bounds.
    let rbp = unsafe { frame.as_ptr().add(args.len()) } as u64;
    let dst_addr = dst.as_mut_ptr() as u64;
    let written = run(
        |b, d| {
            let mut inner = Vec::new();
            emit_fp_prologue(b, &s, &mut inner);
            // Cast: the destination's address as an imm64.
            mov_imm64_full(b, R9, dst_addr as i64);
            emit_length_pass(b, &s, &mut inner);
            b.emit(&[0x41, 0x89, 0x49, s.len_d8]); // MOV [R9+len],ECX
            mov_r64_r64(b, R11, R9);
            emit_fill_pass(b, &s);
            mov_r64_r64(b, RAX, R8);
            b.emit(&[0x4C, 0x29, 0xD8]); // SUB RAX,R11
            emit_fp_restore(b, &s);
            let done = jmp_patch(b);
            for e in inner {
                patch_here(b, e);
            }
            emit_fp_restore(b, &s);
            d.push(jmp_patch(b));
            patch_here(b, done);
        },
        rbp,
        dst_addr,
    );
    if written < 0 {
        return None;
    }
    // Cast: a small non-negative count.
    let n = written as usize - data;
    let raw: Vec<u8> = dst.iter().flat_map(|w| w.to_le_bytes()).collect();
    let len_at = cratonvm_types::ARRAY_LENGTH_OFFSET;
    let ecx = u32::from_le_bytes(raw[len_at..len_at + 4].try_into().expect("4 bytes"));
    assert_eq!(ecx as usize, n, "the length pass's count is the bytes written");
    assert!(
        raw[data + n..data + n + 16].iter().all(|&b| b == 0xAA),
        "wrote past the result"
    );
    Some(raw[data..data + n].to_vec())
}

/// CONCAT21-3 (`CRATONVM_JIT_CONCAT_FP_INLINE`): float / double operands
/// render through the leaf into the scratch area and are copied (LATIN1)
/// or inflated (wide) as the shared formatter spells them; RSP comes back
/// (the harness's own `RET` proves it) on the hit path.
#[test]
fn r21_fp_operands_render_through_the_leaf() {
    let p = plan("f=\u{1} d=\u{1} i=\u{1};", "(FDI)Ljava/lang/String;");
    assert!(p.has_fp());
    for (f, dv) in [
        (1.5f32, 1.0e-5f64),
        (f32::NAN, f64::NEG_INFINITY),
        (-0.0, 0.1 + 0.2),
        (1e10, f64::MIN_POSITIVE),
        (f32::MIN_POSITIVE, 5e-324),
    ] {
        let args = [
            // A float is the slot's low 32 bits, under garbage.
            0x5A5A_5A5A_0000_0000u64 | u64::from(f.to_bits()),
            dv.to_bits(),
            0xDEAD_0000_FFFF_FFF9u64, // int -7 under garbage
        ];
        let want = format!(
            "f={} d={} i=-7;",
            cratonvm_types::java_float_to_string(f),
            cratonvm_types::java_double_to_string(dv)
        );
        assert_eq!(render_full(p, &args, None), Some(want.into_bytes()), "{f} {dv}");
    }
    // The wide passes inflate the same text.
    let pw = plan("\u{3a9}\u{1}", "(D)Ljava/lang/String;");
    assert!(pw.wide && pw.has_fp());
    assert_eq!(
        render_full(pw, &[1e7f64.to_bits()], None),
        Some(u16le("\u{3a9}1.0E7"))
    );
}

/// CONCAT21-4 (`CRATONVM_JIT_CONCAT_VALUE_OF_FUSION`): a `String` operand
/// holding an exact Integer renders its `int` (compact and legacy
/// layouts, both passes, the wide twin too); a real String still copies,
/// an `Integer[]` (same class id, an array kind tag) declines, and
/// without the shape any non-String declines as before.
#[test]
fn r21_a_fused_integer_operand_renders_its_int() {
    const INT_CID: u32 = 91;
    let boxed = ConcatBoxedInt {
        class_id: INT_CID,
        compact_off: 16,
        legacy_off: 40,
    };
    let integer = |v: i32, compact: bool| -> Vec<u64> {
        let mut raw = vec![0u8; 64];
        raw[0..4].copy_from_slice(&INT_CID.to_le_bytes());
        if compact {
            raw[cratonvm_types::GC_FLAGS_BYTE_OFFSET] = cratonvm_types::GC_FLAG_COMPACT;
        }
        let at = if compact { 16 } else { 40 };
        raw[at..at + 4].copy_from_slice(&v.to_le_bytes());
        words(&raw)
    };
    let p = plan("<\u{1}|\u{1}>", "(Ljava/lang/String;Ljava/lang/String;)Ljava/lang/String;");
    let hello = byte_array(b"hi");
    let s = string_obj(&hello, 0, true, STRING_CID);
    for (v, compact) in [(42, true), (-7, false), (i32::MIN, true), (0, false)] {
        let i = integer(v, compact);
        assert_eq!(
            render_full(p, &[i.as_ptr() as u64, s.as_ptr() as u64], Some(boxed)),
            Some(format!("<{v}|hi>").into_bytes()),
            "{v} {compact}"
        );
    }
    // Without the shape: the bridge, as before.
    let i = integer(5, true);
    assert_eq!(render_full(p, &[i.as_ptr() as u64, s.as_ptr() as u64], None), None);
    // An Integer[] carries the component's id: declines.
    let mut arr = integer(5, true);
    let tag_word = cratonvm_types::KIND_TAGS_BYTE_OFFSET / 8;
    let tag_shift = (cratonvm_types::KIND_TAGS_BYTE_OFFSET % 8) * 8;
    arr[tag_word] |= u64::from(cratonvm_types::KIND_TAG_BYTE_MASK) << tag_shift;
    assert_eq!(
        render_full(p, &[arr.as_ptr() as u64, s.as_ptr() as u64], Some(boxed)),
        None
    );
    // The wide passes render it as units.
    let pw = plan("\u{3a9}\u{1}", "(Ljava/lang/String;)Ljava/lang/String;");
    let i = integer(-12, true);
    assert_eq!(
        render_full(pw, &[i.as_ptr() as u64], Some(boxed)),
        Some(u16le("\u{3a9}-12"))
    );
}

/// The marker the promoted (wide) build adds to its written count.
const PROMOTED: i64 = 1 << 40;

/// CONCAT21-1: the LATIN1 passes with their UTF16 edges on the wide
/// twin's passes, executed as both tiers wire them: `Some((text, wide))`,
/// `wide` when the twin built it, `None` when either build declined.
fn render_promoting(
    p: &'static JitConcatInlinePlan,
    args: &[u64],
) -> Option<(Vec<u8>, bool)> {
    let twin = p.promotion_twin().expect("a twin");
    let flag: u8 = 1;
    let s = shape(p, &flag, args.len());
    let ws = shape(twin, &flag, args.len());
    let mut frame = vec![0u64; args.len() + 1];
    for (i, a) in args.iter().enumerate() {
        frame[args.len() - 1 - i] = *a;
    }
    let data = cratonvm_types::ARRAY_DATA_OFFSET;
    let mut dst = vec![0xAAAA_AAAA_AAAA_AAAAu64; 512];
    // Cast: the frame top and the destination array as words.
    // SAFETY: the frame has args.len() + 1 elements, so this offset is in bounds.
    let rbp = unsafe { frame.as_ptr().add(args.len()) } as u64;
    let dst_addr = dst.as_mut_ptr() as u64;
    let build = |b: &mut ExecutableBuffer, sh: &ConcatPassShape| {
        b.emit(&[0x41, 0x89, 0x49, sh.len_d8]); // MOV [R9+len],ECX
        mov_r64_r64(b, R11, R9);
        emit_fill_pass(b, sh);
        mov_r64_r64(b, RAX, R8);
        b.emit(&[0x4C, 0x29, 0xD8]); // SUB RAX,R11
    };
    let written = run(
        |b, d| {
            let mut promote = Vec::new();
            emit_length_pass_promoting(b, &s, d, &mut promote);
            build(b, &s);
            let done = jmp_patch(b);
            assert!(!promote.is_empty(), "a String / char operand has a UTF16 edge");
            for e in promote {
                patch_here(b, e);
            }
            emit_length_pass(b, &ws, d);
            build(b, &ws);
            mov_imm64_full(b, R10, PROMOTED);
            b.emit(&[0x4C, 0x01, 0xD0]); // ADD RAX,R10
            patch_here(b, done);
        },
        rbp,
        dst_addr,
    );
    if written < 0 {
        return None;
    }
    let wide = written & PROMOTED != 0;
    // Cast: a small non-negative count.
    let n = (written & !PROMOTED) as usize - data;
    let raw: Vec<u8> = dst.iter().flat_map(|w| w.to_le_bytes()).collect();
    let len_at = cratonvm_types::ARRAY_LENGTH_OFFSET;
    let ecx = u32::from_le_bytes(raw[len_at..len_at + 4].try_into().expect("4 bytes"));
    assert_eq!(ecx as usize, n, "the length pass's count is the bytes written");
    assert!(
        raw[data + n..data + n + 16].iter().all(|&b| b == 0xAA),
        "wrote past the result"
    );
    Some((raw[data..data + n].to_vec(), wide))
}

/// CONCAT21-1 (`CRATONVM_JIT_CONCAT_CODER_PROMOTE`): a LATIN1 site whose
/// operand is UTF16 at run time -- a `char` above `0xFF`, a UTF16 String,
/// anywhere in the chain -- is built by the wide twin as UTF-16LE (the
/// LATIN1 operands before it inflated), exactly HotSpot's text; LATIN1
/// operands keep the LATIN1 build; an empty twin result and every other
/// refusal still decline to the bridge.
#[test]
fn r21_a_utf16_operand_promotes_to_the_wide_twin() {
    let pc = plan("x=\u{1};", "(C)Ljava/lang/String;");
    assert_eq!(render_promoting(pc, &[0x41]), Some((b"x=A;".to_vec(), false)));
    assert_eq!(
        render_promoting(pc, &[0xFFFF_0000_0000_03A9]),
        Some((u16le("x=\u{3a9};"), true))
    );
    assert_eq!(render_promoting(pc, &[0xFF]), Some((vec![b'x', b'=', 0xFF, b';'], false)));
    // A UTF16 String after a LATIN1 one and an int: the chain promotes
    // at its third operand, the first two re-rendered as units.
    let p = plan(
        "\u{1}+\u{1}+\u{1}\u{e9}",
        "(Ljava/lang/String;ILjava/lang/String;)Ljava/lang/String;",
    );
    let latin = byte_array(b"ab");
    let ls = string_obj(&latin, 0, true, STRING_CID);
    let wbytes = u16le("\u{3a9}z");
    let wv = byte_array(&wbytes);
    let ws = string_obj(&wv, 1, true, STRING_CID);
    assert_eq!(
        render_promoting(p, &[ls.as_ptr() as u64, 7, ws.as_ptr() as u64]),
        Some((u16le("ab+7+\u{3a9}z\u{e9}"), true))
    );
    assert_eq!(
        render_promoting(p, &[ls.as_ptr() as u64, 7, ls.as_ptr() as u64]),
        Some((b"ab+7+ab\xe9".to_vec(), false))
    );
    // null stays "null" on both builds.
    assert_eq!(
        render_promoting(p, &[0, 7, ws.as_ptr() as u64]),
        Some((u16le("null+7+\u{3a9}z\u{e9}"), true))
    );
    // `s1 + s2` has no literal text: an empty twin result declines.
    let pe = plan("\u{1}\u{1}", "(Ljava/lang/String;Ljava/lang/String;)Ljava/lang/String;");
    let empty = byte_array(b"");
    let ew = string_obj(&empty, 1, true, STRING_CID);
    assert_eq!(render_promoting(pe, &[ew.as_ptr() as u64, ew.as_ptr() as u64]), None);
    assert_eq!(
        render_promoting(pe, &[ls.as_ptr() as u64, ws.as_ptr() as u64]),
        Some((u16le("ab\u{3a9}z"), true))
    );
    // A foreign class still declines (the twin re-screens it).
    let other = string_obj(&wv, 1, true, STRING_CID + 1);
    assert_eq!(render_promoting(pe, &[ls.as_ptr() as u64, other.as_ptr() as u64]), None);
}
