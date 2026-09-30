// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! JIT review round 9, wave 4, lane `x64core4`: executed fixtures for the
//! batch pre-headers (bulk zero fill, SIMD sum, SIMD element-wise) on
//! ROTATED loops — the `goto COND; BODY; COND: if<cc> BODY` shape ecj,
//! kotlinc and scalac emit
//! (`rotated-loops-lose-every-preheader-transform-20260918.md`).
//!
//! Under `CRATONVM_JIT_ROTATED_PREHEADER=1` the driver detects such a loop on
//! its unrotated copy (`escape_analysis::detect_batch_loop`) and the walk
//! emits the pre-header at the entry `goto`; unarmed, the header is
//! bypassable and every batch pre-header is refused. Each test compiles the
//! exact ecj 3.45 bytecode both ways and pins:
//! * the value (or the stored bytes) equals the scalar oracle, armed and
//!   unarmed, across lengths around the batch widths and the zero-trip case;
//! * the batch pre-header was emitted only when armed (its signature bytes),
//!   so a silent scalar fallback cannot pass.
//!
//! `CRATONVM_JIT_VECTORIZE` is armed through the thread override for every
//! compile here (`simd_sum_forms_enabled` latches its first read
//! process-wide; since wave 4 it is default-on anyway).

#![cfg(target_arch = "x86_64")]

use cratonvm_jit::x64::{compile, has_avx2};
use cratonvm_jit::CompiledMethod;
use cratonvm_jit_api::JitRuntimeHelpers;
use cratonvm_types::{ARRAY_DATA_OFFSET, ARRAY_LENGTH_OFFSET};
use std::collections::{HashMap, HashSet};

fn helpers() -> JitRuntimeHelpers {
    unsafe extern "C" fn record_throw_bci(_bci: i64) {}
    unsafe extern "C" fn deopt_unserviceable(_vm: i64, _info: i64, _args: i64, _n: i64) -> i64 {
        i64::MIN
    }
    JitRuntimeHelpers {
        set_throw_bci: record_throw_bci as *const () as usize,
        service_callee_deopt: deopt_unserviceable as *const () as usize,
        ..Default::default()
    }
}

/// Compile `code` with the rotated pre-header relocation `armed` or not
/// (vectorization on either way; two padding bytes appended, as the in-crate
/// fixtures do).
fn compile_rotated(
    code: &[u8],
    num_params: usize,
    max_locals: usize,
    armed: bool,
) -> CompiledMethod {
    let mut padded = code.to_vec();
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
        },
    )
    .expect("rotated fixture compiles")
}

/// A synthetic primitive array: length at `ARRAY_LENGTH_OFFSET`, `len`
/// elements of `elem` bytes from `ARRAY_DATA_OFFSET`, then `GUARD` bytes that
/// must keep their fill.
struct PrimArray {
    words: Vec<u64>,
    len: usize,
    elem: usize,
}

impl PrimArray {
    const GUARD: usize = 64;
    const GUARD_FILL: u8 = 0x5A;

    fn new(len: usize, elem: usize) -> Self {
        let bytes = ARRAY_DATA_OFFSET + len * elem + Self::GUARD;
        let mut me = PrimArray {
            words: vec![0u64; bytes.div_ceil(8)],
            len,
            elem,
        };
        let base = me.base();
        // SAFETY: the allocation holds the header, the elements and the
        // guard; every write below is inside it.
        unsafe {
            std::ptr::write_unaligned(base.add(ARRAY_LENGTH_OFFSET) as *mut i32, len as i32);
            for g in 0..Self::GUARD {
                *base.add(ARRAY_DATA_OFFSET + len * elem + g) = Self::GUARD_FILL;
            }
        }
        me
    }

    fn ints(values: &[i32]) -> Self {
        let mut me = Self::new(values.len(), 4);
        for (i, v) in values.iter().enumerate() {
            me.set_int(i, *v);
        }
        me
    }

    fn base(&mut self) -> *mut u8 {
        self.words.as_mut_ptr() as *mut u8
    }

    fn handle(&mut self) -> i64 {
        self.base() as i64
    }

    fn byte(&mut self, i: usize) -> u8 {
        assert!(i < self.len && self.elem == 1);
        // SAFETY: `i` is an element index of this byte array.
        unsafe { *self.base().add(ARRAY_DATA_OFFSET + i) }
    }

    fn set_byte(&mut self, i: usize, v: u8) {
        assert!(i < self.len && self.elem == 1);
        // SAFETY: as above.
        unsafe { *self.base().add(ARRAY_DATA_OFFSET + i) = v }
    }

    fn int(&mut self, i: usize) -> i32 {
        assert!(i < self.len && self.elem == 4);
        // SAFETY: `i` is an element index of this int array.
        unsafe {
            std::ptr::read_unaligned(self.base().add(ARRAY_DATA_OFFSET + i * 4) as *const i32)
        }
    }

    fn set_int(&mut self, i: usize, v: i32) {
        assert!(i < self.len && self.elem == 4);
        // SAFETY: as above.
        unsafe {
            std::ptr::write_unaligned(self.base().add(ARRAY_DATA_OFFSET + i * 4) as *mut i32, v)
        }
    }

    fn guard_is_untouched(&mut self) -> bool {
        let start = ARRAY_DATA_OFFSET + self.len * self.elem;
        let base = self.base();
        // SAFETY: the guard region is inside the allocation.
        (0..Self::GUARD).all(|g| unsafe { *base.add(start + g) } == Self::GUARD_FILL)
    }
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|w| w == needle)
}

/// ecj: `static void z(boolean[] a, int limit) { for (int i = 0; i <= limit; i++) a[i] = false; }`
const ECJ_ZERO_FILL: [u8; 18] = [
    0x03, 0x3d, // 0: i = 0
    0xa7, 0x00, 0x0a, // 2: goto 12
    0x2a, 0x1c, 0x03, 0x54, // 5: a[i] = 0        ; header
    0x84, 0x02, 0x01, // 9: i++
    0x1c, 0x1b, // 12: iload i; iload limit     ; COND
    0xa4, 0xff, 0xf7, // 14: if_icmple 5
    0xb1, // 17: return
];

/// ecj: `static int s(int[] a) { int s = 0; int n = a.length; for (int i = 0; i < n; i++) s += a[i]; return s; }`
const ECJ_INT_SUM: [u8; 26] = [
    0x03, 0x3c, // 0: s = 0
    0x2a, 0xbe, 0x3d, // 2: n = a.length
    0x03, 0x3e, // 5: i = 0
    0xa7, 0x00, 0x0c, // 7: goto 19
    0x1b, 0x2a, 0x1d, 0x2e, 0x60, 0x3c, // 10: s += a[i]  ; header
    0x84, 0x03, 0x01, // 16: i++
    0x1d, 0x1c, // 19: iload i; iload n          ; COND
    0xa1, 0xff, 0xf5, // 21: if_icmplt 10
    0x1b, 0xac, // 24: return s
];

/// ecj: `static void add(int[] c, int[] a, int[] b) { for (int i = 0; i < c.length; i++) c[i] = a[i] + b[i]; }`
const ECJ_ADD_OVER_LENGTH: [u8; 25] = [
    0x03, 0x3e, // 0: i = 0
    0xa7, 0x00, 0x10, // 2: goto 18
    0x2a, 0x1d, 0x2b, 0x1d, 0x2e, 0x2c, 0x1d, 0x2e, 0x60, 0x4f, // 5: c[i] = a[i] + b[i]
    0x84, 0x03, 0x01, // 15: i++
    0x1d, 0x2a, 0xbe, // 18: iload i; aload c; arraylength   ; COND
    0xa1, 0xff, 0xf0, // 21: if_icmplt 5
    0xb1, // 24: return
];

/// `REP STOSB` — the bulk zero-fill pre-header's store.
const REP_STOSB: [u8; 2] = [0xF3, 0xAA];
/// `VPMOVSXDQ ymm1, [rax]` — the SIMD sum pre-header's widening load.
const VPMOVSXDQ_YMM1_RAX: [u8; 5] = [0xC4, 0xE2, 0x7D, 0x25, 0x08];
/// `VZEROUPPER` — ends every AVX2 pre-header.
const VZEROUPPER: [u8; 3] = [0xC5, 0xF8, 0x77];

#[test]
fn a_rotated_zero_fill_runs_rep_stosb_at_its_entry_goto_and_is_exact() {
    let off = compile_rotated(&ECJ_ZERO_FILL, 2, 3, false);
    let on = compile_rotated(&ECJ_ZERO_FILL, 2, 3, true);
    assert!(
        contains(on.code_bytes(), &REP_STOSB),
        "armed: the rotated zero fill must reach the bulk pre-header at its entry goto"
    );
    assert!(
        !contains(off.code_bytes(), &REP_STOSB),
        "unarmed: a rotated header is bypassable and keeps no batch pre-header"
    );
    for (len, limit) in [
        (1usize, 0i64),
        (2, 1),
        (7, 6),
        (64, 63),
        (1000, 999),
        (1000, 500),
        (8, -1),
    ] {
        for (m, which) in [(&off, "unarmed"), (&on, "armed")] {
            let mut a = PrimArray::new(len, 1);
            for i in 0..len {
                a.set_byte(i, 1);
            }
            // SAFETY: a static `([ZI)V` body; `limit < len`, so the loop
            // never reaches its throw path.
            unsafe { m.try_call(&[a.handle(), limit]).expect("call") };
            for i in 0..len {
                let want = if (i as i64) <= limit { 0 } else { 1 };
                assert_eq!(a.byte(i), want, "{which}: len {len}, limit {limit}, a[{i}]");
            }
            assert!(
                a.guard_is_untouched(),
                "{which}: len {len}: wrote past the array"
            );
        }
    }
}

#[test]
fn a_rotated_int_sum_is_vectorised_at_its_entry_goto_and_exact() {
    if !has_avx2() {
        return;
    }
    let off = compile_rotated(&ECJ_INT_SUM, 1, 4, false);
    let on = compile_rotated(&ECJ_INT_SUM, 1, 4, true);
    assert!(
        contains(on.code_bytes(), &VPMOVSXDQ_YMM1_RAX),
        "armed: the rotated `s += a[i]` loop must reach the AVX2 sum pre-header"
    );
    assert!(
        !contains(off.code_bytes(), &VPMOVSXDQ_YMM1_RAX),
        "unarmed: a rotated header keeps no SIMD pre-header"
    );
    let mut cases: Vec<Vec<i32>> = [0usize, 1, 7, 8, 9, 15, 16, 17, 1000]
        .iter()
        .map(|&len| (0..len as i32).map(|i| i.wrapping_mul(7919) ^ -3).collect())
        .collect();
    cases.push(vec![i32::MAX; 19]); // the int accumulator must wrap like `iadd`
    for values in cases {
        let want = values.iter().fold(0i32, |s, v| s.wrapping_add(*v));
        let mut a = PrimArray::ints(&values);
        // SAFETY: a static `([I)I` body; `a` is a laid-out, non-null array.
        let got_off = unsafe { off.try_call(&[a.handle()]).expect("call") } as i32;
        let got_on = unsafe { on.try_call(&[a.handle()]).expect("call") } as i32;
        assert_eq!(got_off, want, "unarmed: len {}", values.len());
        assert_eq!(got_on, want, "armed: len {}", values.len());
    }
}

#[test]
fn a_rotated_element_wise_add_over_a_length_is_vectorised_and_exact() {
    if !has_avx2() {
        return;
    }
    let off = compile_rotated(&ECJ_ADD_OVER_LENGTH, 3, 4, false);
    let on = compile_rotated(&ECJ_ADD_OVER_LENGTH, 3, 4, true);
    assert!(
        contains(on.code_bytes(), &VZEROUPPER),
        "armed: the rotated element-wise loop must reach the AVX2 pre-header"
    );
    assert!(
        !contains(off.code_bytes(), &VZEROUPPER),
        "unarmed: a rotated header keeps no SIMD pre-header"
    );
    for len in [0usize, 1, 7, 8, 9, 17, 1000] {
        let av: Vec<i32> = (0..len as i32).map(|i| i * 3 - 5).collect();
        let bv: Vec<i32> = (0..len as i32).map(|i| i ^ 0x55).collect();
        for (m, which) in [(&off, "unarmed"), (&on, "armed")] {
            let mut c = PrimArray::ints(&vec![0; len]);
            let mut a = PrimArray::ints(&av);
            let mut b = PrimArray::ints(&bv);
            // SAFETY: a static `([I[I[I)V` body; all three fixtures are `len`
            // long, so no throw path is reached.
            unsafe {
                m.try_call(&[c.handle(), a.handle(), b.handle()])
                    .expect("call");
            }
            for i in 0..len {
                assert_eq!(
                    c.int(i),
                    av[i].wrapping_add(bv[i]),
                    "{which}: len {len}, c[{i}]"
                );
            }
            assert!(c.guard_is_untouched(), "{which}: len {len}: wrote past c");
        }
    }
}
