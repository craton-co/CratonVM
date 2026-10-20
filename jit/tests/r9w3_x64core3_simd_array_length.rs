// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! JIT review round 9, wave 3, lane `x64core3`: executed fixtures for the
//! single-pass SIMD pre-headers over the idiomatic `i < a.length` header
//! (`simd-detectors-reject-the-array-length-loop-bound-20260918.md`).
//!
//! The `a.length` header is detected only under the vectorization opt-in
//! (`CRATONVM_JIT_VECTORIZE`), and `simd_sum_forms_enabled` LATCHES its first
//! read process-wide. Every compile in this binary therefore runs under the
//! same thread override (armed), so whichever test compiles first latches the
//! armed value; no test here wants it off.
//!
//! What each test pins:
//! * the value equals the scalar oracle for lengths around the 8-lane chunk
//!   (0, 1, 7, 8, 9, 1000);
//! * the vector pre-header was actually emitted (the `VPMOVSXDQ` / `VMOVDQU`
//!   encodings are in the body), so a silent scalar fallback cannot pass;
//! * a short OUT array under an `a.length` bound takes the self-guard's
//!   fallback, throws at `out.length`, and writes nothing past it.

#![cfg(target_arch = "x86_64")]

use cratonvm_jit::x64::{compile, has_avx2};
use cratonvm_jit::CompiledMethod;
use cratonvm_jit_api::JitRuntimeHelpers;
use cratonvm_types::{ARRAY_DATA_OFFSET, ARRAY_LENGTH_OFFSET};
use std::collections::{HashMap, HashSet};

/// What the AIOOBE stub was handed, `(index, length)`.
static AIOOBE_LOG: std::sync::Mutex<Vec<(i64, i64)>> = std::sync::Mutex::new(Vec::new());
/// Serialises the tests that read [`AIOOBE_LOG`].
static AIOOBE_GATE: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn helpers() -> JitRuntimeHelpers {
    unsafe extern "C" fn record_throw_bci(_bci: i64) {}
    unsafe extern "C" fn deopt_unserviceable(_vm: i64, _info: i64, _args: i64, _n: i64) -> i64 {
        i64::MIN
    }
    unsafe extern "C" fn throw_aioobe_stub(index: i64, length: i64, _array: i64, _pc: i64) -> i64 {
        if let Ok(mut log) = AIOOBE_LOG.lock() {
            log.push((index, length));
        }
        i64::MIN
    }
    JitRuntimeHelpers {
        set_throw_bci: record_throw_bci as *const () as usize,
        service_callee_deopt: deopt_unserviceable as *const () as usize,
        throw_aioobe: throw_aioobe_stub as *const () as usize,
        ..Default::default()
    }
}

/// Compile `code` with the vectorization opt-in armed (two padding bytes
/// appended, as the in-crate fixtures do).
fn compile_vectorized(code: &[u8], num_params: usize, max_locals: usize) -> CompiledMethod {
    let mut padded = code.to_vec();
    padded.extend_from_slice(&[0, 0]);
    cratonvm_types::flags::with_thread_overrides(&[("CRATONVM_JIT_VECTORIZE", Some("1"))], || {
        compile(
            &padded,
            code.len(),
            num_params,
            max_locals,
            false,
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            HashMap::new(),
            HashMap::new(),
            &helpers(),
            HashSet::new(),
            HashMap::new(),
            None,
        )
    })
    .expect("fixture compiles")
}

/// A synthetic `int[]`: length at `ARRAY_LENGTH_OFFSET`, elements from
/// `ARRAY_DATA_OFFSET`, then `GUARD` element slots that must stay zero.
struct IntArray {
    words: Vec<u64>,
    len: usize,
}

impl IntArray {
    const GUARD: usize = 16;

    fn new(values: &[i32]) -> Self {
        let bytes = ARRAY_DATA_OFFSET + (values.len() + Self::GUARD) * 4;
        let mut me = IntArray {
            words: vec![0u64; bytes.div_ceil(8)],
            len: values.len(),
        };
        let base = me.base();
        // SAFETY: the allocation holds the header, `len` elements and the
        // guard; every write below is inside it.
        unsafe {
            std::ptr::write_unaligned(
                base.add(ARRAY_LENGTH_OFFSET) as *mut i32,
                values.len() as i32,
            );
            for (i, v) in values.iter().enumerate() {
                std::ptr::write_unaligned(base.add(ARRAY_DATA_OFFSET + i * 4) as *mut i32, *v);
            }
        }
        me
    }

    fn base(&mut self) -> *mut u8 {
        self.words.as_mut_ptr() as *mut u8
    }

    fn handle(&mut self) -> i64 {
        self.base() as i64
    }

    fn get(&mut self, i: usize) -> i32 {
        // SAFETY: callers pass an index inside the element-plus-guard region.
        unsafe {
            std::ptr::read_unaligned(self.base().add(ARRAY_DATA_OFFSET + i * 4) as *const i32)
        }
    }

    fn guards_are_untouched(&mut self) -> bool {
        (self.len..self.len + Self::GUARD).all(|i| self.get(i) == 0)
    }
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|w| w == needle)
}

/// `static int sum(int[] a) { int s = 0; for (int i = 0; i < a.length; i++) s += a[i]; return s; }`
const SUM_OVER_LENGTH: [u8; 24] = [
    0x03, // 0: iconst_0
    0x3c, // 1: istore_1            ; s = 0
    0x03, // 2: iconst_0
    0x3d, // 3: istore_2            ; i = 0
    0x1c, // 4: iload_2             ; header
    0x2a, // 5: aload_0
    0xbe, // 6: arraylength
    0xa2, 0x00, 0x0f, // 7: if_icmpge +15 -> 22
    0x1b, // 10: iload_1
    0x2a, // 11: aload_0
    0x1c, // 12: iload_2
    0x2e, // 13: iaload
    0x60, // 14: iadd
    0x3c, // 15: istore_1
    0x84, 0x02, 0x01, // 16: iinc 2, 1
    0xa7, 0xff, 0xf1, // 19: goto -15 -> 4
    0x1b, // 22: iload_1
    0xac, // 23: ireturn
];

/// `static void f(int[] out, int[] a, int[] b) { for (int i = 0; i < a.length; i++) out[i] = a[i] + b[i]; }`
const ADD_OVER_LENGTH: [u8; 25] = [
    0x03, // 0: iconst_0
    0x3e, // 1: istore_3            ; i = 0
    0x1d, // 2: iload_3             ; header
    0x2b, // 3: aload_1
    0xbe, // 4: arraylength
    0xa2, 0x00, 0x13, // 5: if_icmpge +19 -> 24
    0x2a, // 8: aload_0
    0x1d, // 9: iload_3
    0x2b, // 10: aload_1
    0x1d, // 11: iload_3
    0x2e, // 12: iaload
    0x2c, // 13: aload_2
    0x1d, // 14: iload_3
    0x2e, // 15: iaload
    0x60, // 16: iadd
    0x4f, // 17: iastore
    0x84, 0x03, 0x01, // 18: iinc 3, 1
    0xa7, 0xff, 0xed, // 21: goto -19 -> 2
    0xb1, // 24: return
];

/// `VPMOVSXDQ ymm1, [rax]` — the sum pre-header's widening load.
const VPMOVSXDQ_YMM1_RAX: [u8; 5] = [0xC4, 0xE2, 0x7D, 0x25, 0x08];

#[test]
fn an_int_sum_over_a_length_is_vectorised_and_exact() {
    if !has_avx2() {
        return;
    }
    let m = compile_vectorized(&SUM_OVER_LENGTH, 1, 3);
    assert!(
        contains(m.code_bytes(), &VPMOVSXDQ_YMM1_RAX),
        "the `a.length` header must reach the AVX2 sum pre-header under the opt-in \
         (driver gate: `simd_sum_covered`)"
    );
    for len in [0usize, 1, 7, 8, 9, 15, 16, 17, 1000] {
        let values: Vec<i32> = (0..len as i32).map(|i| i.wrapping_mul(7919) ^ -3).collect();
        let want = values.iter().fold(0i32, |s, v| s.wrapping_add(*v));
        let mut a = IntArray::new(&values);
        // SAFETY: a static `([I)I` body with no runtime helper on the
        // non-throwing path; `a` is a laid-out array fixture.
        let got = unsafe { m.try_call(&[a.handle()]).expect("call") } as i32;
        assert_eq!(got, want, "len = {len}");
    }
    // Wrapping: the int accumulator must wrap exactly like `iadd`.
    let values = vec![i32::MAX; 19];
    let want = values.iter().fold(0i32, |s, v| s.wrapping_add(*v));
    let mut a = IntArray::new(&values);
    // SAFETY: as above.
    let got = unsafe { m.try_call(&[a.handle()]).expect("call") } as i32;
    assert_eq!(got, want, "wrapping sum");
}

#[test]
fn an_element_wise_add_over_a_length_is_vectorised_and_exact() {
    if !has_avx2() {
        return;
    }
    let m = compile_vectorized(&ADD_OVER_LENGTH, 3, 4);
    for len in [0usize, 1, 7, 8, 9, 17, 1000] {
        let av: Vec<i32> = (0..len as i32).map(|i| i * 3 - 5).collect();
        let bv: Vec<i32> = (0..len as i32).map(|i| i ^ 0x55).collect();
        let mut out = IntArray::new(&vec![0; len]);
        let mut a = IntArray::new(&av);
        let mut b = IntArray::new(&bv);
        // SAFETY: a static `([I[I[I)V` body; all three fixtures are `len` long,
        // so no throw path is reached.
        unsafe {
            m.try_call(&[out.handle(), a.handle(), b.handle()])
                .expect("call");
        }
        for i in 0..len {
            assert_eq!(
                out.get(i),
                av[i].wrapping_add(bv[i]),
                "len = {len}, i = {i}"
            );
        }
        assert!(out.guards_are_untouched(), "len = {len}: wrote past OUT");
    }
}

#[test]
fn a_short_out_array_falls_back_to_the_checked_loop() {
    if !has_avx2() {
        return;
    }
    let m = compile_vectorized(&ADD_OVER_LENGTH, 3, 4);
    let _gate = AIOOBE_GATE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    AIOOBE_LOG.lock().expect("log").clear();
    let len = 64usize;
    let short = 20usize;
    let av: Vec<i32> = (0..len as i32).collect();
    let bv: Vec<i32> = (0..len as i32).map(|i| 1000 + i).collect();
    let mut out = IntArray::new(&vec![0; short]);
    let mut a = IntArray::new(&av);
    let mut b = IntArray::new(&bv);
    let dummy_vm = [0u8; 64];
    // SAFETY: the only helper the throw path reaches is the AIOOBE stub.
    let _ = unsafe {
        m.try_call_with_context(
            dummy_vm.as_ptr() as i64,
            &[out.handle(), a.handle(), b.handle()],
        )
    };
    let log = AIOOBE_LOG.lock().expect("log").clone();
    assert_eq!(
        log,
        vec![(short as i64, short as i64)],
        "OUT is shorter than the `a.length` bound: the pre-header's `bound <= out.length` \
         self-guard must refuse, and the scalar loop must throw at out.length"
    );
    assert!(
        out.guards_are_untouched(),
        "the vector loop stored past the end of a short OUT array"
    );
    for i in 0..short {
        assert_eq!(out.get(i), av[i] + bv[i], "out[{i}] before the throw");
    }
}

/// ecj/kotlinc-style ROTATED `for (i = 0; i < a.length; i++) s += a[i]`
/// (`goto COND; BODY; COND: iload i; aload a; arraylength; if_icmplt BODY`).
/// Under `CRATONVM_JIT_ROTATED_PREHEADER=1` its `arraylength` is hoisted to
/// the entry `goto` (proved against COND's prefix,
/// `find_array_len_hoists_with_rotation`); by default the header is
/// bypassable and nothing is hoisted
/// (`rotated-loops-lose-every-preheader-transform-20260918.md`).
const ROTATED_SUM: [u8; 24] = [
    0x03, // 0: iconst_0
    0x3c, // 1: istore_1
    0x03, // 2: iconst_0
    0x3d, // 3: istore_2
    0xa7, 0x00, 0x0c, // 4: goto +12 -> 16
    0x1b, // 7: iload_1        ; header (BODY)
    0x2a, // 8: aload_0
    0x1c, // 9: iload_2
    0x2e, // 10: iaload
    0x60, // 11: iadd
    0x3c, // 12: istore_1
    0x84, 0x02, 0x01, // 13: iinc 2, 1
    0x1c, // 16: iload_2        ; COND
    0x2a, // 17: aload_0
    0xbe, // 18: arraylength
    0xa1, 0xff, 0xf4, // 19: if_icmplt -12 -> 7
    0x1b, // 22: iload_1
    0xac, // 23: ireturn
];

#[test]
fn a_rotated_array_length_loop_is_exact_armed_and_unarmed() {
    let compile_with = |armed: bool| {
        let mut padded = ROTATED_SUM.to_vec();
        padded.extend_from_slice(&[0, 0]);
        cratonvm_types::flags::with_thread_overrides(
            &[
                ("CRATONVM_JIT_VECTORIZE", Some("1")),
                (
                    "CRATONVM_JIT_ROTATED_PREHEADER",
                    Some(if armed { "1" } else { "0" }),
                ),
            ],
            || {
                compile(
                    &padded,
                    ROTATED_SUM.len(),
                    1,
                    3,
                    false,
                    Vec::new(),
                    Vec::new(),
                    Vec::new(),
                    Vec::new(),
                    Vec::new(),
                    Vec::new(),
                    Vec::new(),
                    Vec::new(),
                    Vec::new(),
                    Vec::new(),
                    Vec::new(),
                    Vec::new(),
                    HashMap::new(),
                    HashMap::new(),
                    &helpers(),
                    HashSet::new(),
                    HashMap::new(),
                    None,
                )
            },
        )
        .expect("rotated loop compiles")
    };
    let off = compile_with(false);
    let on = compile_with(true);
    for len in [0usize, 1, 2, 7, 64, 1000] {
        let values: Vec<i32> = (0..len as i32).map(|i| i * 31 - 7).collect();
        let want = values.iter().fold(0i32, |s, v| s.wrapping_add(*v));
        let mut a = IntArray::new(&values);
        // SAFETY: a static `([I)I` body; `a` is a laid-out, non-null array,
        // so no throw path is reached.
        let got_off = unsafe { off.try_call(&[a.handle()]).expect("call") } as i32;
        let got_on = unsafe { on.try_call(&[a.handle()]).expect("call") } as i32;
        assert_eq!(got_off, want, "default: len = {len}");
        assert_eq!(got_on, want, "armed: len = {len}");
    }
    assert_ne!(
        off.code_bytes(),
        on.code_bytes(),
        "arming must relocate the rotated header's pre-header (arraylength hoist \
         at the entry goto)"
    );
}
