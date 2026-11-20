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
const RECV: u64 = 0xCAFE_0000;

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

fn array(desc: &str, len: u32, payload: &[u8]) -> Vec<u64> {
    let data = cratonvm_types::ARRAY_DATA_OFFSET;
    let mut r = vec![0u8; data + payload.len() + 8];
    r[cratonvm_types::KIND_TAGS_BYTE_OFFSET] =
        cratonvm_types::primitive_array_kind_tags_byte(desc).expect("array tag");
    let len_at = cratonvm_types::ARRAY_LENGTH_OFFSET;
    r[len_at..len_at + 4].copy_from_slice(&len.to_le_bytes());
    r[data..data + payload.len()].copy_from_slice(payload);
    words(&r)
}

fn char_mark() -> i32 {
    use cratonvm_types::{ArrayElementType, ClassId, ObjectHeader, ObjectKind};
    let m = ObjectHeader::new(
        ClassId::new(0),
        ObjectKind::Array,
        ArrayElementType::Char,
        0,
        0,
    )
    .mark_word
    .load(std::sync::atomic::Ordering::Relaxed);
    i32::from_le_bytes(m.to_le_bytes())
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
        char_array_mark: char_mark(),
    }
}

#[repr(C)]
struct Args {
    rax: u64,
    rcx: u64,
    r10: u64,
    r9: u64,
    xmm2_sentinel: u128,
    xmm6_sentinel: u128,
}

/// Emit `body` as `extern "C" fn(rbp, &mut Args) -> i64`, loading `RAX`, `RCX`,
/// `R10`, `R9` from `Args`, seeding `XMM2` and `XMM6` with sentinels and
/// reading them back afterwards to prove only `XMM0`/`XMM1` are touched.
fn run_args(
    body: impl Fn(&mut ExecutableBuffer, &mut Vec<usize>),
    rbp: u64,
    args: &mut Args,
) -> i64 {
    let mut buf = ExecutableBuffer::new(1 << 16).expect("test buffer");
    buf.emit_byte(0x55); // PUSH RBP
    mov_r64_r64(&mut buf, 5, A0); // MOV RBP, a0
    mov_r64_r64(&mut buf, R8, A1); // R8 = &mut Args
    //PUSH R8 so we can verify XMM2 / XMM6 on exit (keep 16B alignment via SUB RSP, 8).
    buf.emit(&[0x41, 0x50]); // PUSH R8
    buf.emit(&[0x48, 0x83, 0xEC, 0x08]); // SUB RSP, 8
    // MOVDQU XMM2, [R8 + 32]; MOVDQU XMM6, [R8 + 48]
    buf.emit(&[0xF3, 0x41, 0x0F, 0x6F, 0x50, 0x20]);
    buf.emit(&[0xF3, 0x41, 0x0F, 0x6F, 0x70, 0x30]);
    // Load RAX, RCX, R10, R9 from [R8 + 0..32)
    mov_r64_mem(&mut buf, RAX, R8, 0);
    mov_r64_mem(&mut buf, RCX, R8, 8);
    mov_r64_mem(&mut buf, R10, R8, 16);
    mov_r64_mem(&mut buf, R9, R8, 24);

    let mut decline = Vec::new();
    body(&mut buf, &mut decline);
    let exit = jmp_patch(&mut buf);
    for p in decline {
        patch_here(&mut buf, p);
    }
    mov_imm32_sx(&mut buf, RAX, -1);
    patch_here(&mut buf, exit);
    buf.emit(&[0x48, 0x83, 0xC4, 0x08]); // ADD RSP, 8
    buf.emit(&[0x41, 0x58]); // POP R8
    // Store XMM2 -> [R8 + 32], XMM6 -> [R8 + 48]
    buf.emit(&[0xF3, 0x41, 0x0F, 0x7F, 0x50, 0x20]);
    buf.emit(&[0xF3, 0x41, 0x0F, 0x7F, 0x70, 0x30]);
    buf.emit(&[0x5D, 0xC3]); // POP RBP ; RET
    assert!(!buf.overflowed());
    crate::platform::make_executable(buf.as_ptr() as *mut u8, buf.capacity())
        .expect("RX test buffer");
    // SAFETY: leaf machine-code test snippet preserving RBP and callee-saved state.
    let f: extern "C" fn(u64, *mut Args) -> i64 = unsafe { std::mem::transmute(buf.as_ptr()) };
    f(rbp, args as *mut Args)
}

fn with_tlab(tlab_words: usize, go: impl FnOnce(u64) -> i64) -> (i64, Vec<u8>, u64, u64) {
    let mut tlab = vec![0u64; tlab_words];
    let base = tlab.as_mut_ptr() as u64;
    let mut thread = vec![0u64; 4];
    thread[1] = base;
    thread[2] = base + 8 * tlab_words as u64;
    let frame = [thread.as_ptr() as u64, 0];
    // SAFETY: one past `frame[0]`; emitted code addresses `[rbp - 8]`.
    let rbp = unsafe { frame.as_ptr().add(1) } as u64;
    let r = go(rbp);
    let moved = thread[1] - base;
    (r, raw(&tlab), base, moved)
}

fn decode_allocated_latin1(tlab: &[u8], base: u64, str_ptr: i64) -> Vec<u8> {
    assert!(str_ptr > 0, "expected allocated String pointer, got {str_ptr}");
    let str_off = usize::try_from(str_ptr as u64 - base).expect("in TLAB");
    let cid = u32::from_le_bytes(tlab[str_off..str_off + 4].try_into().unwrap());
    assert_eq!(cid, STRING_CID);
    assert_eq!(tlab[str_off + CODER.0 as usize], 0, "LATIN1 coder");
    let val_ptr = u64::from_le_bytes(
        tlab[str_off + VALUE_OFF as usize..str_off + VALUE_OFF as usize + 8]
            .try_into()
            .unwrap(),
    );
    let arr_off = usize::try_from(val_ptr - base).expect("byte[] in TLAB");
    assert_eq!(
        tlab[arr_off + cratonvm_types::KIND_TAGS_BYTE_OFFSET],
        cratonvm_types::primitive_array_kind_tags_byte("[B").unwrap()
    );
    let len_at = arr_off + cratonvm_types::ARRAY_LENGTH_OFFSET;
    let len = u32::from_le_bytes(tlab[len_at..len_at + 4].try_into().unwrap()) as usize;
    let data_at = arr_off + cratonvm_types::ARRAY_DATA_OFFSET;
    tlab[data_at..data_at + len].to_vec()
}

#[test]
fn replace_char_unchanged_cases_do_not_allocate() {
    let flag = 1u8;
    let guard = 7u32;
    let s = site(&flag, &guard, 7);

    // 1. old == new on String.replace returns receiver even when char is present;
    //    on StringLatin1.replace (`unchanged_zero = true`) allocates a copy when present.
    let src = array("[B", 3, b"a.b");
    let mut args = Args {
        rax: RECV,
        rcx: src.as_ptr() as u64,
        r10: u64::from(b'.') | 0xDEAD_0000,
        r9: u64::from(b'.') | 0xBEEF_0000,
        xmm2_sentinel: 0x0123_4567_89AB_CDEF_FEDC_BA98_7654_3210,
        xmm6_sentinel: 0xAAAA_5555_BBBB_6666_CCCC_7777_DDDD_8888,
    };
    let (r, _, _, moved) = with_tlab(64, |rbp| {
        run_args(|b, d| emit_ir_latin1_replace_char(b, &s, d, false), rbp, &mut args)
    });
    assert_eq!(r, RECV as i64);
    assert_eq!(moved, 0);
    assert_eq!(args.xmm2_sentinel, 0x0123_4567_89AB_CDEF_FEDC_BA98_7654_3210);
    assert_eq!(args.xmm6_sentinel, 0xAAAA_5555_BBBB_6666_CCCC_7777_DDDD_8888);

    let (r_l1, tlab_l1, base_l1, moved_l1) = with_tlab(64, |rbp| {
        run_args(|b, d| emit_ir_latin1_replace_char(b, &s, d, true), rbp, &mut args)
    });
    assert!(moved_l1 > 0);
    assert_eq!(decode_allocated_latin1(&tlab_l1, base_l1, r_l1), b"a.b");

    // 2. old > 0xFF (including 0x012E whose low byte equals '.') never matches Latin-1.
    for &unchanged_zero in &[false, true] {
        let want = if unchanged_zero { 0 } else { RECV as i64 };
        let mut a = Args {
            rax: RECV,
            rcx: src.as_ptr() as u64,
            r10: 0x012E,
            r9: u64::from(b'/'),
            xmm2_sentinel: 11,
            xmm6_sentinel: 22,
        };
        let (r, _, _, moved) = with_tlab(64, |rbp| {
            run_args(
                |b, d| emit_ir_latin1_replace_char(b, &s, d, unchanged_zero),
                rbp,
                &mut a,
            )
        });
        assert_eq!(r, want);
        assert_eq!(moved, 0);
    }

    // 3. Absent needle across empty, short, 16B, and 62B strings, even with wide newChar.
    let inputs: [&[u8]; 4] = [
        b"",
        b"quarkus",
        b"0123456789abcdef",
        b"io/quarkus/commons/benchmarks/BenchmarkClassnameToResourceName",
    ];
    for text in inputs {
        let arr = array("[B", text.len() as u32, text);
        for &unchanged_zero in &[false, true] {
            for &new_ch in &[u64::from(b'_'), 0x03B1] {
                let mut a = Args {
                    rax: RECV,
                    rcx: arr.as_ptr() as u64,
                    r10: u64::from(b'.'),
                    r9: new_ch,
                    xmm2_sentinel: 33,
                    xmm6_sentinel: 44,
                };
                let (r, _, _, moved) = with_tlab(64, |rbp| {
                    run_args(
                        |b, d| emit_ir_latin1_replace_char(b, &s, d, unchanged_zero),
                        rbp,
                        &mut a,
                    )
                });
                let want = if unchanged_zero { 0 } else { RECV as i64 };
                assert_eq!(r, want, "len={} z={unchanged_zero} new={new_ch:#x}", text.len());
                assert_eq!(moved, 0);
            }
        }
    }
}

#[test]
fn replace_char_transforms_all_lengths_and_positions_in_one_tlab_bump() {
    let flag = 1u8;
    let guard = 7u32;
    let s = site(&flag, &guard, 7);

    let mut cases: Vec<Vec<u8>> = vec![
        b".".to_vec(),
        b"a.b".to_vec(),
        b"....".to_vec(),
        b"io.quarkus.commons.benchmarks.BenchmarkClassnameToResourceName".to_vec(),
    ];
    for len in [1usize, 7, 8, 15, 16, 17, 31, 32, 33, 48, 62, 65] {
        // Match only at first, only at middle, only at last byte.
        for pos in [0, len / 2, len - 1] {
            let mut v = vec![b'x'; len];
            v[pos] = b'.';
            cases.push(v);
        }
    }
    // High Latin-1 bytes (0x80..=0xFF) too.
    cases.push(vec![0xE9, b'.', 0xFF, 0x80, b'.', 0xDF]);

    for input in cases {
        let expected: Vec<u8> = input
            .iter()
            .map(|&b| if b == b'.' { b'/' } else { b })
            .collect();
        let arr = array("[B", input.len() as u32, &input);
        let before = arr.clone();
        let mut args = Args {
            rax: RECV,
            rcx: arr.as_ptr() as u64,
            r10: u64::from(b'.') | 0xABCD_0000,
            r9: u64::from(b'/') | 0x1234_0000,
            xmm2_sentinel: 0xDEAD_BEEF_CAFE_BABE_0102_0304_0506_0708,
            xmm6_sentinel: 0x1122_3344_5566_7788_99AA_BBCC_DDEE_FF00,
        };
        let (r, tlab, base, moved) = with_tlab(64, |rbp| {
            run_args(|b, d| emit_ir_latin1_replace_char(b, &s, d, false), rbp, &mut args)
        });
        assert!(moved > 0, "len={}", input.len());
        assert_eq!(decode_allocated_latin1(&tlab, base, r), expected);
        assert_eq!(arr, before, "source array must remain untouched");
        assert_eq!(args.xmm2_sentinel, 0xDEAD_BEEF_CAFE_BABE_0102_0304_0506_0708);
        assert_eq!(args.xmm6_sentinel, 0x1122_3344_5566_7788_99AA_BBCC_DDEE_FF00);
    }
}

#[test]
fn replace_char_declines_wide_new_char_and_bad_inputs_without_moving_cursor() {
    let flag = 1u8;
    let guard = 7u32;
    let s = site(&flag, &guard, 7);
    let arr = array("[B", 62, b"io.quarkus.commons.benchmarks.BenchmarkClassnameToResourceName");
    let char_arr = array("[C", 4, b"a\0.\0b\0c\0");

    // Wide newChar when oldChar is present declines before allocating.
    let mut wide = Args {
        rax: RECV,
        rcx: arr.as_ptr() as u64,
        r10: u64::from(b'.'),
        r9: 0x0100,
        xmm2_sentinel: 1,
        xmm6_sentinel: 2,
    };
    let (r, _, _, moved) = with_tlab(64, |rbp| {
        run_args(|b, d| emit_ir_latin1_replace_char(b, &s, d, false), rbp, &mut wide)
    });
    assert_eq!((r, moved), (-1, 0));

    // Null value, wrong array tag, full TLAB.
    for (val_ptr, words_cap) in [(0u64, 64usize), (char_arr.as_ptr() as u64, 64), (arr.as_ptr() as u64, 2)] {
        let mut a = Args {
            rax: RECV,
            rcx: val_ptr,
            r10: u64::from(b'.'),
            r9: u64::from(b'/'),
            xmm2_sentinel: 1,
            xmm6_sentinel: 2,
        };
        let (r, _, _, moved) = with_tlab(words_cap, |rbp| {
            run_args(|b, d| emit_ir_latin1_replace_char(b, &s, d, false), rbp, &mut a)
        });
        assert_eq!((r, moved), (-1, 0));
    }
}

#[test]
fn latin1_concat_handles_empty_short_and_quarkus_68b_in_one_tlab_bump() {
    let flag = 1u8;
    let guard = 7u32;
    let s = site(&flag, &guard, 7);

    let empty = array("[B", 0, b"");
    let suffix = array("[B", 6, b".class");
    let qname = array(
        "[B",
        62,
        b"io/quarkus/commons/benchmarks/BenchmarkClassnameToResourceName",
    );

    // 1. arg.isEmpty() returns receiver RAX without allocating (even for empty receiver).
    for recv_arr in [&empty, &qname] {
        let mut a = Args {
            rax: RECV,
            rcx: recv_arr.as_ptr() as u64,
            r10: empty.as_ptr() as u64,
            r9: 0,
            xmm2_sentinel: 99,
            xmm6_sentinel: 88,
        };
        let (r, _, _, moved) = with_tlab(64, |rbp| {
            run_args(|b, d| emit_ir_latin1_concat(b, &s, d), rbp, &mut a)
        });
        assert_eq!((r, moved), (RECV as i64, 0));
        assert_eq!((a.xmm2_sentinel, a.xmm6_sentinel), (99, 88));
    }

    // 2. "".concat(".class") allocates a FRESH String (matching StringConcatHelper.doConcat).
    let mut empty_recv = Args {
        rax: RECV,
        rcx: empty.as_ptr() as u64,
        r10: suffix.as_ptr() as u64,
        r9: 0,
        xmm2_sentinel: 77,
        xmm6_sentinel: 66,
    };
    let (r, tlab, base, moved) = with_tlab(64, |rbp| {
        run_args(|b, d| emit_ir_latin1_concat(b, &s, d), rbp, &mut empty_recv)
    });
    assert_ne!(r, RECV as i64);
    assert!(moved > 0);
    assert_eq!(decode_allocated_latin1(&tlab, base, r), b".class");

    // 3. 62B + 6B = 68B Quarkus resource name in one TLAB bump.
    let mut q = Args {
        rax: RECV,
        rcx: qname.as_ptr() as u64,
        r10: suffix.as_ptr() as u64,
        r9: 0,
        xmm2_sentinel: 0x5555_AAAA,
        xmm6_sentinel: 0xBBBB_4444,
    };
    let (r, tlab, base, moved) = with_tlab(64, |rbp| {
        run_args(|b, d| emit_ir_latin1_concat(b, &s, d), rbp, &mut q)
    });
    assert!(moved > 0);
    assert_eq!(
        decode_allocated_latin1(&tlab, base, r),
        b"io/quarkus/commons/benchmarks/BenchmarkClassnameToResourceName.class"
    );
    assert_eq!((q.xmm2_sentinel, q.xmm6_sentinel), (0x5555_AAAA, 0xBBBB_4444));

    // 4. Declines on null arrays, bad tag, overflow, or full TLAB without moving cursor.
    let char_arr = array("[C", 2, b"a\0b\0");
    let huge = array("[B", i32::MAX as u32, b"x");
    for (p1, p2, cap) in [
        (0u64, suffix.as_ptr() as u64, 64usize),
        (qname.as_ptr() as u64, 0u64, 64),
        (char_arr.as_ptr() as u64, suffix.as_ptr() as u64, 64),
        (qname.as_ptr() as u64, char_arr.as_ptr() as u64, 64),
        (huge.as_ptr() as u64, suffix.as_ptr() as u64, 64),
        (qname.as_ptr() as u64, suffix.as_ptr() as u64, 2),
    ] {
        let mut bad = Args {
            rax: RECV,
            rcx: p1,
            r10: p2,
            r9: 0,
            xmm2_sentinel: 1,
            xmm6_sentinel: 2,
        };
        let (r, _, _, moved) = with_tlab(cap, |rbp| {
            run_args(|b, d| emit_ir_latin1_concat(b, &s, d), rbp, &mut bad)
        });
        assert_eq!((r, moved), (-1, 0));
    }
}

#[test]
fn latin1_hash_unrolled4_matches_jls_across_lengths_and_high_bytes() {
    for len in 0..=70usize {
        let payload: Vec<u8> = (0..len)
            .map(|i| ((i.wrapping_mul(73).wrapping_add(0x80 * (i & 1))) & 0xFF) as u8)
            .collect();
        let want = payload
            .iter()
            .fold(0i32, |h, &b| h.wrapping_mul(31).wrapping_add(i32::from(b)));
        let arr = array("[B", len as u32, &payload);
        let mut a = Args {
            rax: 0x1234_5678,
            rcx: arr.as_ptr() as u64,
            r10: 0,
            r9: 0,
            xmm2_sentinel: 42,
            xmm6_sentinel: 84,
        };
        let got = run_args(
            |b, _| {
                let len_d8 = cratonvm_types::ARRAY_LENGTH_OFFSET as u8;
                let data_d8 = cratonvm_types::ARRAY_DATA_OFFSET as u8;
                b.emit(&[0x44, 0x8B, 0x59, len_d8]); // MOV R11D, [RCX + len_d8]
                emit_latin1_hash_unrolled4(b, data_d8);
                b.emit(&[0x48, 0x63, 0xC0]); // MOVSXD RAX, EAX
            },
            0,
            &mut a,
        );
        assert_eq!(got as i32, want, "len={len}");
        assert_eq!((a.xmm2_sentinel, a.xmm6_sentinel), (42, 84));
    }
}

#[test]
fn kind_facts_and_naming_cover_replace_and_concat() {
    use crate::{
        ir_splice_nested_case_prefix_stays_call, ir_string_prefix_site_stays_a_call,
        ir_string_search_prefix_kind, IrStringSearchKind as K,
    };

    assert_eq!(
        ir_string_search_prefix_kind("java/lang/String", "replace", "(CC)Ljava/lang/String;"),
        Some(K::ReplaceChar)
    );
    assert_eq!(
        ir_string_search_prefix_kind(
            "java/lang/StringLatin1",
            "replace",
            "([BCC)Ljava/lang/String;"
        ),
        Some(K::Latin1ReplaceChar)
    );
    assert_eq!(
        ir_string_search_prefix_kind(
            "java/lang/String",
            "replace",
            "(Ljava/lang/CharSequence;Ljava/lang/CharSequence;)Ljava/lang/String;"
        ),
        Some(K::ReplaceCharSeq)
    );
    assert_eq!(
        ir_string_search_prefix_kind(
            "java/lang/String",
            "concat",
            "(Ljava/lang/String;)Ljava/lang/String;"
        ),
        Some(K::StringConcat)
    );

    for (k, params, static_site) in [
        (K::ReplaceChar, 2usize, false),
        (K::Latin1ReplaceChar, 2, true),
        (K::ReplaceCharSeq, 2, false),
        (K::StringConcat, 1, false),
    ] {
        assert_eq!(k.num_params(), params, "{k:?}");
        assert!(ir_string_prefix_site_stays_a_call(k), "{k:?}");
        if static_site {
            assert!(k.admits_invoke_kind(3) && !k.admits_invoke_kind(0), "{k:?}");
            assert!(k.admits_opcode(0xb8) && !k.admits_opcode(0xb6), "{k:?}");
        } else {
            assert!(
                k.admits_invoke_kind(0) && k.admits_invoke_kind(1) && !k.admits_invoke_kind(3),
                "{k:?}"
            );
            assert!(k.admits_opcode(0xb6) && !k.admits_opcode(0xb8), "{k:?}");
        }
    }
    assert!(ir_splice_nested_case_prefix_stays_call(
        "java/lang/StringLatin1",
        "replace",
        "([BCC)Ljava/lang/String;",
        3,
    ));
}


