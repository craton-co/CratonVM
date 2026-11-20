// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

use super::prim_arraycopy::emit_prim_copy_bytes;
use super::*;
use crate::ir::{IrBuilder, IrType};
use crate::ir_schedule;
use cratonvm_types::{
    ArrayElementType, ClassId, ObjectHeader, ObjectKind, ARRAY_DATA_OFFSET, ARRAY_LENGTH_OFFSET,
    GC_FLAG_COMPACT, KIND_TAGS_BYTE_OFFSET,
};
use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};

const DESC: &str = "(Ljava/lang/Object;ILjava/lang/Object;II)V";

#[derive(Default)]
struct DispatchCounter {
    calls: AtomicUsize,
}

extern "C" fn test_dispatch(
    ctx: *mut DispatchCounter,
    _info: usize,
    _args: *const i64,
    _n: usize,
) -> i64 {
    if !ctx.is_null() {
        // SAFETY: `ctx` points to a live `DispatchCounter` owned by the test frame.
        unsafe { (*ctx).calls.fetch_add(1, Ordering::Relaxed) };
    }
    0
}

extern "C" fn guard_ok(_vm: i64, _ctor_receiver: i64) -> i64 {
    0
}

unsafe extern "C" fn record_throw_bci(_bci: i64) {}

unsafe extern "C" fn deopt_unserviceable_stub(_vm: i64, _info: i64, _args: i64, _n: i64) -> i64 {
    i64::MIN
}

fn test_helpers() -> JitRuntimeHelpers {
    let mut h = JitRuntimeHelpers::default();
    h.invoke_dispatch = test_dispatch as *const () as usize;
    h.self_call_stack_guard = guard_ok as *const () as usize;
    h.set_throw_bci = record_throw_bci as *const () as usize;
    h.service_callee_deopt = deopt_unserviceable_stub as *const () as usize;
    h
}

/// Build a synthetic 8-byte-aligned heap array with `elem` and `length`, filled
/// from `payload` starting at `ARRAY_DATA_OFFSET`.
fn make_array(elem: ArrayElementType, length: u32, payload: &[u8]) -> Vec<u64> {
    let total_bytes = ARRAY_DATA_OFFSET + payload.len();
    let words_len = total_bytes.div_ceil(8).max(2);
    let mut words = vec![0u64; words_len];
    let header = ObjectHeader::new(ClassId::new(0), ObjectKind::Array, elem, length, length);
    // SAFETY: `words` is 8-byte aligned and at least `ARRAY_DATA_OFFSET + payload.len()` bytes.
    unsafe {
        let base = words.as_mut_ptr() as *mut u8;
        std::ptr::write_unaligned(base as *mut u64, header.header_word());
        std::ptr::write_unaligned(base.add(ARRAY_LENGTH_OFFSET) as *mut u32, length);
        std::ptr::copy_nonoverlapping(payload.as_ptr(), base.add(ARRAY_DATA_OFFSET), payload.len());
    }
    words
}

fn make_plain_object(hashed: bool) -> Vec<u64> {
    let mut words = vec![0u64; 4];
    let header = ObjectHeader::new(
        ClassId::new(42),
        ObjectKind::Object,
        ArrayElementType::Reference,
        0,
        1,
    );
    // SAFETY: `words` is 32 bytes, 8-byte aligned.
    unsafe {
        let base = words.as_mut_ptr() as *mut u8;
        std::ptr::write_unaligned(base as *mut u64, header.header_word());
        if hashed {
            // Set compact flag and non-zero identity hash bits in `KIND_TAGS_BYTE_OFFSET`
            // (preserving low kind bits = 0 for plain Object).
            *base.add(cratonvm_types::GC_FLAGS_BYTE_OFFSET) |= GC_FLAG_COMPACT;
            *base.add(KIND_TAGS_BYTE_OFFSET) |= 0x3C;
        }
    }
    words
}

fn array_payload(words: &[u64], byte_len: usize) -> &[u8] {
    // SAFETY: `words` holds at least `ARRAY_DATA_OFFSET + byte_len` bytes.
    unsafe {
        let base = words.as_ptr() as *const u8;
        std::slice::from_raw_parts(base.add(ARRAY_DATA_OFFSET), byte_len)
    }
}

fn compile_arraycopy(const_args: Option<(i32, i32, i32)>) -> CompiledMethod {
    let info: &'static JitInvokeInfo = Box::leak(Box::new(JitInvokeInfo {
        class_name: "java/lang/System",
        method_name: "arraycopy",
        descriptor: DESC,
        num_jit_args: 5,
        return_type: b'V',
        invoke_kind: 3,
        declaring_class_id: 0,
        owner_class_id: 0,
    }));
    let info_ptr = info as *const JitInvokeInfo as usize;
    let (code, invoke_pc) = match const_args {
        Some((sp, dp, l)) => {
            let [sp_hi, sp_lo] = i16::try_from(sp).expect("sp fits i16").to_be_bytes();
            let [dp_hi, dp_lo] = i16::try_from(dp).expect("dp fits i16").to_be_bytes();
            let [l_hi, l_lo] = i16::try_from(l).expect("len fits i16").to_be_bytes();
            (
                vec![
                    0x2a, // aload_0 (src)
                    0x11, sp_hi, sp_lo, // sipush srcPos
                    0x2c,  // aload_2 (dst)
                    0x11, dp_hi, dp_lo, // sipush dstPos
                    0x11, l_hi, l_lo, // sipush len
                    0xb8, 0x00, 0x02, // invokestatic #2
                    0xb1, // return
                ],
                11usize,
            )
        }
        None => (
            vec![
                0x2a, // aload_0 (src)
                0x1b, // iload_1 (srcPos)
                0x2c, // aload_2 (dst)
                0x1d, // iload_3 (dstPos)
                0x15, 0x04, // iload 4 (len)
                0xb8, 0x00, 0x02, // invokestatic #2
                0xb1, // return
            ],
            6usize,
        ),
    };
    let params = [
        IrType::Ref,
        IrType::Int,
        IrType::Ref,
        IrType::Int,
        IrType::Int,
    ];
    let mut builder = IrBuilder::new(5, 5);
    builder.set_param_types(&params);
    builder.set_invoke_info(HashMap::from([(invoke_pc, (info_ptr, 5usize, b'V'))]));
    let graph = builder
        .build(&code, code.len())
        .expect("System.arraycopy IR builds");
    let schedule = ir_schedule::schedule(&graph);
    clear_lower_bail();
    lower(&graph, &schedule, 5, 5, &test_helpers())
        .unwrap_or_else(|| panic!("System.arraycopy graph lowers: {:?}", last_lower_bail()))
}

#[test]
fn prim_copy_bytes_covers_0_to_128_odd_alignments_and_self_overlap() {
    let mut buf = ExecutableBuffer::new(1024).expect("buffer");
    // Move C args (src, dst, byte_len) to R10, RCX, R11.
    #[cfg(target_os = "windows")]
    buf.emit(&[0x49, 0x89, 0xCA, 0x48, 0x89, 0xD1, 0x4D, 0x89, 0xC3]); // RCX, RDX, R8
    #[cfg(not(target_os = "windows"))]
    buf.emit(&[0x49, 0x89, 0xFA, 0x48, 0x89, 0xF1, 0x49, 0x89, 0xD3]); // RDI, RSI, RDX
    emit_prim_copy_bytes(&mut buf);
    buf.emit(&[0xC3]); // RET
    assert!(!buf.overflowed());
    buf.finalize();
    // SAFETY: `buf` holds a leaf function reading `[src..src+len)` and writing
    // `[dst..dst+len)` using only caller-saved registers.
    let f: extern "C" fn(*const u8, *mut u8, i64) = unsafe { std::mem::transmute(buf.as_ptr()) };

    // 1. Distinct buffers, lengths 0..=128, odd alignments.
    for len in 0..=128usize {
        for (src_align, dst_align) in [(0, 0), (1, 3), (7, 5), (15, 9)] {
            let src: Vec<u8> = (0..192u8)
                .map(|i| i.wrapping_mul(13).wrapping_add(5))
                .collect();
            let mut dst = vec![0xA5u8; 192];
            // SAFETY: pointers stay within `src` and `dst`.
            unsafe {
                f(
                    src.as_ptr().add(src_align),
                    dst.as_mut_ptr().add(dst_align),
                    len as i64,
                );
            }
            assert_eq!(
                &dst[dst_align..dst_align + len],
                &src[src_align..src_align + len],
                "distinct len={len} aligns=({src_align},{dst_align})"
            );
            assert!(
                dst[..dst_align].iter().all(|&b| b == 0xA5),
                "len={len}: clobbered before dst"
            );
            assert!(
                dst[dst_align + len..].iter().all(|&b| b == 0xA5),
                "len={len}: clobbered past dst"
            );
        }
    }

    // 2. Self-array overlapping copies (forward, backward, and in-place).
    let shifts = [0usize, 1, 2, 3, 4, 7, 8, 15, 16, 17, 31, 32, 33, 64];
    for len in 0..=128usize {
        for &shift in &shifts {
            for backward in [false, true] {
                let base = 16usize;
                let (src_off, dst_off) = if backward {
                    (base, base + shift)
                } else {
                    (base + shift, base)
                };
                let init: Vec<u8> = (0..=255u8)
                    .map(|i| i.wrapping_mul(29).wrapping_add(3))
                    .collect();
                let mut got = init.clone();
                let mut want = init;
                want.copy_within(src_off..src_off + len, dst_off);
                // SAFETY: `src_off + len` and `dst_off + len` are < 256.
                unsafe {
                    f(
                        got.as_ptr().add(src_off),
                        got.as_mut_ptr().add(dst_off),
                        len as i64,
                    );
                }
                assert_eq!(
                    got, want,
                    "overlap len={len} shift={shift} backward={backward}"
                );
            }
        }
    }
}

#[test]
fn ir_lower_all_eight_primitive_types_and_self_overlap() {
    let cm = compile_arraycopy(None);
    let types = [
        (ArrayElementType::Boolean, 1usize),
        (ArrayElementType::Byte, 1),
        (ArrayElementType::Char, 2),
        (ArrayElementType::Short, 2),
        (ArrayElementType::Int, 4),
        (ArrayElementType::Float, 4),
        (ArrayElementType::Long, 8),
        (ArrayElementType::Double, 8),
    ];
    let lengths = [
        0usize, 1, 2, 3, 4, 5, 7, 8, 9, 15, 16, 17, 31, 32, 33, 63, 64, 65, 96, 127, 128,
    ];

    for &(elem, width) in &types {
        for &len in &lengths {
            let arr_elems = len + 11;
            let total_bytes = arr_elems * width;
            let src_bytes: Vec<u8> = (0..total_bytes)
                .map(|i| (i as u8).wrapping_mul(17).wrapping_add(1))
                .collect();
            let dst_init = vec![0x5Au8; total_bytes];
            let src_arr = make_array(elem, arr_elems as u32, &src_bytes);
            let mut dst_arr = make_array(elem, arr_elems as u32, &dst_init);

            let src_pos = 3usize;
            let dst_pos = 5usize;
            let ctx = DispatchCounter::default();
            let args = [
                src_arr.as_ptr() as i64,
                src_pos as i64,
                dst_arr.as_mut_ptr() as i64,
                dst_pos as i64,
                len as i64,
            ];
            // SAFETY: arrays are valid synthetic heap arrays outliving the call.
            unsafe { cm.try_call_with_context(&ctx as *const _ as i64, &args) }.expect("call");
            assert_eq!(
                ctx.calls.load(Ordering::Relaxed),
                0,
                "{elem:?} len={len} declined unexpectedly"
            );

            let got = array_payload(&dst_arr, total_bytes);
            let s_off = src_pos * width;
            let d_off = dst_pos * width;
            let copy_bytes = len * width;
            assert_eq!(
                &got[d_off..d_off + copy_bytes],
                &src_bytes[s_off..s_off + copy_bytes],
                "{elem:?} len={len} payload mismatch"
            );
            assert!(
                got[..d_off].iter().all(|&b| b == 0x5A),
                "{elem:?} len={len} clobbered before dstPos"
            );
            assert!(
                got[d_off + copy_bytes..].iter().all(|&b| b == 0x5A),
                "{elem:?} len={len} clobbered past dstPos+len"
            );
        }

        // Self-array overlap: forward, backward, and in-place.
        for &len in &[0usize, 1, 3, 4, 8, 16, 17, 33, 64] {
            for (src_pos, dst_pos) in [(5usize, 2usize), (2, 5), (3, 3)] {
                let arr_elems = len + 12;
                let total_bytes = arr_elems * width;
                let init: Vec<u8> = (0..total_bytes)
                    .map(|i| (i as u8).wrapping_mul(31).wrapping_add(9))
                    .collect();
                let mut arr = make_array(elem, arr_elems as u32, &init);
                let mut want = init;
                want.copy_within(src_pos * width..(src_pos + len) * width, dst_pos * width);

                let ctx = DispatchCounter::default();
                let ptr = arr.as_mut_ptr() as i64;
                let args = [ptr, src_pos as i64, ptr, dst_pos as i64, len as i64];
                // SAFETY: `arr` is a valid synthetic array outliving the call.
                unsafe { cm.try_call_with_context(&ctx as *const _ as i64, &args) }.expect("call");
                assert_eq!(
                    ctx.calls.load(Ordering::Relaxed),
                    0,
                    "self-overlap {elem:?} len={len} ({src_pos}->{dst_pos}) declined"
                );
                assert_eq!(
                    array_payload(&arr, total_bytes),
                    want.as_slice(),
                    "self-overlap {elem:?} len={len} ({src_pos}->{dst_pos}) mismatch"
                );
            }
        }
    }
}

#[test]
fn ir_lower_const_len_fast_paths_work() {
    for &c_len in &[0i32, 1, 4, 16] {
        let cm = compile_arraycopy(Some((0, 0, c_len)));
        for &(elem, width) in &[
            (ArrayElementType::Boolean, 1usize),
            (ArrayElementType::Byte, 1),
            (ArrayElementType::Short, 2),
            (ArrayElementType::Int, 4),
            (ArrayElementType::Long, 8),
        ] {
            let total_elems = (c_len as usize) + 4;
            let total_bytes = total_elems * width;
            let src_bytes: Vec<u8> = (0..total_bytes)
                .map(|i| (i as u8).wrapping_mul(23).wrapping_add(7))
                .collect();
            let dst_init = vec![0x3Cu8; total_bytes];
            let src_arr = make_array(elem, total_elems as u32, &src_bytes);
            let mut dst_arr = make_array(elem, total_elems as u32, &dst_init);

            let ctx = DispatchCounter::default();
            let args = [
                src_arr.as_ptr() as i64,
                0,
                dst_arr.as_mut_ptr() as i64,
                0,
                i64::from(c_len),
            ];
            // SAFETY: synthetic arrays outlive the call.
            unsafe { cm.try_call_with_context(&ctx as *const _ as i64, &args) }.expect("call");
            assert_eq!(
                ctx.calls.load(Ordering::Relaxed),
                0,
                "const len={c_len} {elem:?} declined"
            );
            let copy_bytes = (c_len as usize) * width;
            let got = array_payload(&dst_arr, total_bytes);
            assert_eq!(&got[..copy_bytes], &src_bytes[..copy_bytes]);
            assert!(got[copy_bytes..].iter().all(|&b| b == 0x3C));
        }
    }
}

#[test]
fn ir_lower_decline_cases_fall_through_without_mutating_dst() {
    let cm = compile_arraycopy(None);
    let int_payload = vec![0x11u8; 64];
    let int_src = make_array(ArrayElementType::Int, 16, &int_payload);
    let bool_src = make_array(ArrayElementType::Boolean, 16, &int_payload);
    let byte_src = make_array(ArrayElementType::Byte, 16, &int_payload);
    let char_src = make_array(ArrayElementType::Char, 16, &int_payload);
    let short_src = make_array(ArrayElementType::Short, 16, &int_payload);
    let float_src = make_array(ArrayElementType::Float, 16, &int_payload);
    let long_src = make_array(ArrayElementType::Long, 16, &int_payload);
    let double_src = make_array(ArrayElementType::Double, 16, &int_payload);
    let ref_arr = make_array(ArrayElementType::Reference, 8, &vec![0u8; 64]);
    let plain_unhashed = make_plain_object(false);
    let plain_hashed = make_plain_object(true);

    let invoke_expect_decline = |src: i64, sp: i64, dst: i64, dp: i64, len: i64, label: &str| {
        let ctx = DispatchCounter::default();
        let args = [src, sp, dst, dp, len];
        // SAFETY: all non-zero pointers refer to live synthetic objects.
        unsafe { cm.try_call_with_context(&ctx as *const _ as i64, &args) }.expect("call");
        assert_eq!(
            ctx.calls.load(Ordering::Relaxed),
            1,
            "{label}: expected decline to dispatch"
        );
    };

    let dst_init = vec![0x7Eu8; 64];
    let mut dst_int = make_array(ArrayElementType::Int, 16, &dst_init);
    let dst_int_ptr = dst_int.as_mut_ptr() as i64;

    // Null pointers.
    invoke_expect_decline(0, 0, dst_int_ptr, 0, 4, "src null");
    invoke_expect_decline(int_src.as_ptr() as i64, 0, 0, 0, 4, "dst null");
    invoke_expect_decline(0, 0, 0, 0, 0, "both null len 0");

    // Plain objects (unhashed & hashed) and reference arrays.
    invoke_expect_decline(
        plain_unhashed.as_ptr() as i64,
        0,
        dst_int_ptr,
        0,
        0,
        "src plain",
    );
    invoke_expect_decline(
        plain_hashed.as_ptr() as i64,
        0,
        dst_int_ptr,
        0,
        0,
        "src hashed",
    );
    invoke_expect_decline(
        int_src.as_ptr() as i64,
        0,
        plain_hashed.as_ptr() as i64,
        0,
        0,
        "dst hashed",
    );
    invoke_expect_decline(ref_arr.as_ptr() as i64, 0, dst_int_ptr, 0, 0, "src ref[]");
    invoke_expect_decline(
        int_src.as_ptr() as i64,
        0,
        ref_arr.as_ptr() as i64,
        0,
        0,
        "dst ref[]",
    );

    // Mismatched primitive array types (including same-width pairs like [Z/[B).
    let mismatches = [
        (&bool_src, &byte_src, "[Z vs [B"),
        (&byte_src, &bool_src, "[B vs [Z"),
        (&char_src, &short_src, "[C vs [S"),
        (&short_src, &char_src, "[S vs [C"),
        (&int_src, &float_src, "[I vs [F"),
        (&float_src, &int_src, "[F vs [I"),
        (&long_src, &double_src, "[J vs [D"),
        (&double_src, &long_src, "[D vs [J"),
        (&byte_src, &int_src, "[B vs [I"),
        (&int_src, &long_src, "[I vs [J"),
    ];
    for (s, d, label) in mismatches {
        let mut d_copy = d.clone();
        invoke_expect_decline(
            s.as_ptr() as i64,
            0,
            d_copy.as_mut_ptr() as i64,
            0,
            4,
            label,
        );
        assert_eq!(array_payload(&d_copy, 64), &int_payload[..]);
    }

    // Negative pos/len & out-of-bounds (including i32::MAX overflow traps).
    let src_p = int_src.as_ptr() as i64;
    let max = i64::from(i32::MAX);
    for (sp, dp, len, label) in [
        (-1, 0, 0, "srcPos < 0"),
        (0, -1, 0, "dstPos < 0"),
        (0, 0, -1, "len < 0"),
        (17, 0, 0, "srcPos > src.length (len 0)"),
        (0, 17, 0, "dstPos > dst.length (len 0)"),
        (10, 0, 7, "srcPos + len > src.length"),
        (0, 10, 7, "dstPos + len > dst.length"),
        (max, 0, 1, "srcPos i32::MAX + 1 wrap"),
        (0, max, 1, "dstPos i32::MAX + 1 wrap"),
        (1, 0, max, "srcPos + len i32::MAX wrap"),
        (0, 1, max, "dstPos + len i32::MAX wrap"),
    ] {
        invoke_expect_decline(src_p, sp, dst_int_ptr, dp, len, label);
    }
    assert_eq!(array_payload(&dst_int, 64), &dst_init[..]);

    // Compile-time negative constant len also declines cleanly.
    let cm_neg = compile_arraycopy(Some((0, 0, -1)));
    let ctx = DispatchCounter::default();
    let args = [src_p, 0, dst_int_ptr, 0, -1];
    // SAFETY: synthetic arrays outlive the call.
    unsafe { cm_neg.try_call_with_context(&ctx as *const _ as i64, &args) }.expect("call");
    assert_eq!(ctx.calls.load(Ordering::Relaxed), 1);
    assert_eq!(array_payload(&dst_int, 64), &dst_init[..]);
}
