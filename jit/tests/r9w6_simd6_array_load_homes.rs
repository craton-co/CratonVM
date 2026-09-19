// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! JIT review round 9, wave 6, lane `simd6`: array element loads addressed
//! through the operands' own registers — fix 3 of
//! `perf-single-pass-scalar-loop-round-trips-every-operand-through-a-scratch-register-20260918.md`.
//!
//! When a load needs neither its null check (the dataflow proves the array
//! local non-null) nor its bounds check (BCE), `op_array.rs` now emits
//! `movsx r8, word [r14 + r12*2 + d]` instead of copying the array into RAX
//! and the index into RCX first and the result out of RAX afterwards.
//!
//! Each counted `for (i = 0; i < a.length; i++) s += a[i]` loop below is
//! compiled in the four configurations of `r9w5_spstack5_scalar_loop.rs`
//! (pure-kernel register homes on/off x `CRATONVM_JIT_NO_OPERAND_FOLD` off/on)
//! and RUN against a Rust oracle. With register homes and the folding on,
//! the RAX/RCX form of the load must be gone; with the folding off (the kill
//! switch) it must be back, byte for byte.

#![cfg(target_arch = "x86_64")]

use cratonvm_jit::x64::{compile_with_request, BackendRequest};
use cratonvm_jit::CompiledMethod;
use cratonvm_jit_api::JitRuntimeHelpers;
use cratonvm_types::{ARRAY_DATA_OFFSET, ARRAY_LENGTH_OFFSET};
use std::collections::{HashMap, HashSet};

fn helpers() -> JitRuntimeHelpers {
    unsafe extern "C" fn record_throw_bci(_bci: i64) {}
    unsafe extern "C" fn deopt_unserviceable(_vm: i64, _info: i64, _args: i64, _n: i64) -> i64 {
        i64::MIN
    }
    unsafe extern "C" fn throw_aioobe_stub(
        _index: i64,
        _length: i64,
        _array: i64,
        _pc: i64,
    ) -> i64 {
        i64::MIN
    }
    JitRuntimeHelpers {
        set_throw_bci: record_throw_bci as *const () as usize,
        service_callee_deopt: deopt_unserviceable as *const () as usize,
        throw_aioobe: throw_aioobe_stub as *const () as usize,
        ..Default::default()
    }
}

#[derive(Clone, Copy, Debug)]
struct Config {
    kernel: bool,
    fold: bool,
}

const CONFIGS: [Config; 4] = [
    Config {
        kernel: true,
        fold: true,
    },
    Config {
        kernel: true,
        fold: false,
    },
    Config {
        kernel: false,
        fold: true,
    },
    Config {
        kernel: false,
        fold: false,
    },
];

fn compile_as(code: &[u8], num_params: usize, max_locals: usize, cfg: Config) -> CompiledMethod {
    let mut padded = code.to_vec();
    padded.extend_from_slice(&[0, 0]);
    let fold_edit = if cfg.fold { None } else { Some("1") };
    cratonvm_types::flags::with_thread_overrides(
        &[("CRATONVM_JIT_NO_OPERAND_FOLD", fold_edit)],
        || {
            compile_with_request(
                BackendRequest {
                    kernel_reg_homes: cfg.kernel,
                    ..BackendRequest::default()
                },
                Vec::new(), // compact_field_info
                &padded,
                code.len(),
                num_params,
                max_locals,
                false,      // needs_heap
                Vec::new(), // multianewarray_info
                Vec::new(), // field_info
                Vec::new(), // typecheck_info
                Vec::new(), // static_field_info
                Vec::new(), // new_info
                Vec::new(), // anewarray_info
                Vec::new(), // invoke_info
                Vec::new(), // direct_calls
                Vec::new(), // mic_slots
                Vec::new(), // pic_slots
                Vec::new(), // ldc_info
                Vec::new(), // ldc2w_info
                HashMap::new(),
                HashMap::new(),
                &helpers(),
                HashSet::new(),
                HashMap::new(),
                None,
            )
        },
    )
    .unwrap_or_else(|| panic!("fixture compiles under {cfg:?}"))
}

/// A synthetic primitive array of `elems.len()` elements of `size` bytes.
struct JArray {
    words: Vec<u64>,
}

impl JArray {
    fn new(size: usize, elems: &[i64]) -> Self {
        let bytes = ARRAY_DATA_OFFSET + (elems.len() + 8) * size;
        let mut me = JArray {
            words: vec![0u64; bytes.div_ceil(8)],
        };
        let base = me.words.as_mut_ptr() as *mut u8;
        // SAFETY: the allocation holds the header and every element; each
        // element takes the low `size` bytes of its little-endian value.
        unsafe {
            std::ptr::write_unaligned(
                base.add(ARRAY_LENGTH_OFFSET) as *mut i32,
                elems.len() as i32,
            );
            for (i, v) in elems.iter().enumerate() {
                let le = v.to_le_bytes();
                std::ptr::copy_nonoverlapping(
                    le.as_ptr(),
                    base.add(ARRAY_DATA_OFFSET + i * size),
                    size,
                );
            }
        }
        me
    }

    fn handle(&mut self) -> i64 {
        self.words.as_mut_ptr() as i64
    }
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|w| w == needle)
}

/// `static int sum(T[] a) { int s = 0; for (int i = 0; i < a.length; i++) s += a[i]; return s; }`
/// with the element load `load` (`baload`/`caload`/`saload`/`iaload`).
fn int_sum(load: u8) -> [u8; 24] {
    [
        0x03, 0x3c, 0x03, 0x3d, // s = 0; i = 0
        0x1c, 0x2a, 0xbe, // 4: iload_2; aload_0; arraylength
        0xa2, 0x00, 0x0f, // 7: if_icmpge -> 22
        0x1b, 0x2a, 0x1c, load, // 10: iload_1; aload_0; iload_2; <load>
        0x60, 0x3c, // 14: iadd; istore_1
        0x84, 0x02, 0x01, // 16: iinc 2, 1
        0xa7, 0xff, 0xf1, // 19: goto -> 4
        0x1b, 0xac, // 22: iload_1; ireturn
    ]
}

/// `static long lsum(long[] a) { long s = 0; for (int i = 0; i < a.length; i++) s += a[i]; return s; }`
const LSUM: [u8; 24] = [
    0x09, 0x40, 0x03, 0x3e, // s = 0L (locals 1-2); i = 0 (local 3)
    0x1d, 0x2a, 0xbe, // 4: iload_3; aload_0; arraylength
    0xa2, 0x00, 0x0f, // 7: if_icmpge -> 22
    0x1f, 0x2a, 0x1d, 0x2f, // 10: lload_1; aload_0; iload_3; laload
    0x61, 0x40, // 14: ladd; lstore_1
    0x84, 0x03, 0x01, // 16: iinc 3, 1
    0xa7, 0xff, 0xf1, // 19: goto -> 4
    0x1f, 0xad, // 22: lload_1; lreturn
];

/// Deterministic, sign-mixed test data.
fn data(len: usize, seed: u64) -> Vec<i64> {
    let mut x = seed | 1;
    (0..len)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x as i64
        })
        .collect()
}

const LENGTHS: [usize; 7] = [0, 1, 2, 7, 9, 31, 1000];

fn call(m: &CompiledMethod, args: &[i64]) -> i64 {
    // SAFETY: a static method over one synthetic array that outlives the
    // call; every index is in bounds, so no helper is reached.
    unsafe { m.try_call(args).expect("test JIT call") }
}

#[test]
fn narrow_sums_load_through_the_homes_and_match_java() {
    let d = ARRAY_DATA_OFFSET as u8;
    // (opcode, element size, Java's widening of the element, the RAX/RCX
    // form of the load the general arm emits)
    type Widen = fn(i64) -> i32;
    let kinds: [(u8, usize, Widen, Vec<u8>); 4] = [
        (
            0x33,
            1,
            |v| v as i8 as i32,
            vec![0x48, 0x0F, 0xBE, 0x44, 0x08, d],
        ),
        (
            0x34,
            2,
            |v| v as u16 as i32,
            vec![0x0F, 0xB7, 0x44, 0x48, d],
        ),
        (
            0x35,
            2,
            |v| v as i16 as i32,
            vec![0x48, 0x0F, 0xBF, 0x44, 0x48, d],
        ),
        (0x2e, 4, |v| v as i32, vec![0x48, 0x63, 0x44, 0x88, d]),
    ];
    for (op, size, widen, general_form) in kinds {
        let code = int_sum(op);
        for cfg in CONFIGS {
            let m = compile_as(&code, 1, 3, cfg);
            // The signature is pinned for the narrow loads only: an `int[]`
            // sum also gets the SIMD pre-header, whose bytes are not this
            // test's business.
            if op != 0x2e && cfg.kernel {
                assert_eq!(
                    contains(m.code_bytes(), &general_form),
                    !cfg.fold,
                    "{op:#x} {cfg:?}: the RAX/RCX load must be gone with the folding on \
                     and back under CRATONVM_JIT_NO_OPERAND_FOLD=1"
                );
            }
            for (k, &len) in LENGTHS.iter().enumerate() {
                let vals = data(len, 0x9E37_79B9 + k as u64 + u64::from(op));
                let want = vals.iter().fold(0i32, |s, &v| s.wrapping_add(widen(v)));
                let mut a = JArray::new(size, &vals);
                let got = call(&m, &[a.handle()]) as i32;
                assert_eq!(got, want, "{op:#x} {cfg:?} len {len}");
            }
        }
    }
}

#[test]
fn a_long_sum_loads_through_the_homes_and_matches_java() {
    for cfg in CONFIGS {
        let m = compile_as(&LSUM, 1, 4, cfg);
        for (k, &len) in LENGTHS.iter().enumerate() {
            let vals = data(len, 0x51ED_2701 + k as u64);
            let want = vals.iter().fold(0i64, |s, &v| s.wrapping_add(v));
            let mut a = JArray::new(8, &vals);
            assert_eq!(call(&m, &[a.handle()]), want, "{cfg:?} len {len}");
        }
    }
}
