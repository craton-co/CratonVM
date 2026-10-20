// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! JIT review round 9, wave 8, lane `arr8`: single-pass array access.
//!
//! 1. `compiled-bastore-to-boolean-array-is-not-masked-20260918.md` (the
//!    single-pass half): `bastore` into a `boolean[]` must store `value & 1`,
//!    like the interpreter (`gc/src/heap.rs` `write_prim_element`) and HotSpot;
//!    into a `byte[]` it keeps the low byte. Compiled and RUN on synthetic
//!    arrays whose header carries the real `[Z` / `[B` kind byte, with the
//!    value computed (runtime test) and constant (`iconst_2`: runtime test;
//!    `iconst_1`: the test is elided and the answer must not change).
//! 2. `perf-single-pass-checked-array-loads-copy-operands-into-rax-rcx-20260918.md`:
//!    a load that keeps its null check but not its bounds check now tests the
//!    array in its own register. Checked here by RUNNING the counted-loop
//!    shape that produces it (`for (i = 0; i < n; i++) s += a[i]`, `n` a
//!    parameter) in the four register-home x operand-fold configurations
//!    against a Rust oracle.

#![cfg(target_arch = "x86_64")]

use cratonvm_jit::x64::{compile_with_request, BackendRequest};
use cratonvm_jit::CompiledMethod;
use cratonvm_jit_api::JitRuntimeHelpers;
use cratonvm_types::{ARRAY_DATA_OFFSET, ARRAY_LENGTH_OFFSET, KIND_TAGS_BYTE_OFFSET};
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

/// A synthetic primitive array of `elems.len()` elements of `size` bytes,
/// whose header carries the kind/element byte `kind_tag` (0 when the test
/// does not care).
struct JArray {
    words: Vec<u64>,
}

impl JArray {
    fn new(size: usize, elems: &[i64], kind_tag: u8) -> Self {
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
            base.add(KIND_TAGS_BYTE_OFFSET).write(kind_tag);
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

    fn byte(&self, i: usize) -> u8 {
        // SAFETY: callers ask only for in-bounds element indices.
        unsafe { *(self.words.as_ptr() as *const u8).add(ARRAY_DATA_OFFSET + i) }
    }
}

fn call(m: &CompiledMethod, args: &[i64]) -> i64 {
    // SAFETY: every array argument is a live synthetic array that outlives
    // the call and every index is in bounds, so no helper is reached.
    unsafe { m.try_call(args).expect("test JIT call") }
}

fn tag(desc: &str) -> u8 {
    cratonvm_types::primitive_array_kind_tags_byte(desc).expect("a 1-D primitive array descriptor")
}

/// `static int f(boolean[]/byte[] a, int v) { a[1] = v; return a[1]; }`
const STORE_VAR: [u8; 8] = [
    0x2a, 0x04, 0x1b, 0x54, // aload_0; iconst_1; iload_1; bastore
    0x2a, 0x04, 0x33, 0xac, // aload_0; iconst_1; baload; ireturn
];

/// The page's own shape: `a[1] = 2` (`iconst_2; bastore`), then read back.
const STORE_TWO: [u8; 8] = [
    0x2a, 0x04, 0x05, 0x54, // aload_0; iconst_1; iconst_2; bastore
    0x2a, 0x04, 0x33, 0xac, // aload_0; iconst_1; baload; ireturn
];

/// `a[1] = true` (`iconst_1; bastore`): the value proof elides the test.
const STORE_ONE: [u8; 8] = [
    0x2a, 0x04, 0x04, 0x54, // aload_0; iconst_1; iconst_1; bastore
    0x2a, 0x04, 0x33, 0xac, // aload_0; iconst_1; baload; ireturn
];

#[test]
fn bastore_into_a_boolean_array_stores_value_and_one() {
    let (z, b) = (tag("[Z"), tag("[B"));
    for cfg in CONFIGS {
        let m = compile_as(&STORE_VAR, 2, 2, cfg);
        for v in [
            0i64,
            1,
            2,
            3,
            0x7F,
            0x80,
            0xFF,
            0x100,
            0x101,
            -1,
            -2,
            i32::MIN as i64,
        ] {
            // boolean[]: JVMS §6.5 `value & 1`.
            let mut za = JArray::new(1, &[0x55, 0x55, 0x55], z);
            let got = call(&m, &[za.handle(), v]);
            assert_eq!(got, v & 1, "{cfg:?} boolean[] value {v:#x}");
            assert_eq!(za.byte(0), 0x55, "{cfg:?}: the neighbours are untouched");
            assert_eq!(za.byte(2), 0x55, "{cfg:?}: the neighbours are untouched");
            // byte[]: the low byte, sign-extended by baload.
            let mut ba = JArray::new(1, &[0x55, 0x55, 0x55], b);
            let got = call(&m, &[ba.handle(), v]);
            assert_eq!(got, i64::from(v as i8), "{cfg:?} byte[] value {v:#x}");
        }
    }
}

#[test]
fn a_constant_two_is_masked_and_a_constant_one_is_not_disturbed() {
    let (z, b) = (tag("[Z"), tag("[B"));
    for cfg in CONFIGS {
        let two = compile_as(&STORE_TWO, 1, 1, cfg);
        let mut za = JArray::new(1, &[7, 7, 7], z);
        assert_eq!(
            call(&two, &[za.handle()]),
            0,
            "{cfg:?}: boolean[] a[1] = 2 reads false"
        );
        let mut ba = JArray::new(1, &[7, 7, 7], b);
        assert_eq!(
            call(&two, &[ba.handle()]),
            2,
            "{cfg:?}: byte[] a[1] = 2 reads 2"
        );

        let one = compile_as(&STORE_ONE, 1, 1, cfg);
        for t in [z, b] {
            let mut a = JArray::new(1, &[0, 0, 0], t);
            assert_eq!(call(&one, &[a.handle()]), 1, "{cfg:?} tag {t:#x}: a[1] = 1");
        }
    }
}

/// `static int sum(T[] a, int n) { int s = 0; for (int i = 0; i < n; i++) s += a[i]; return s; }`
/// with the element load `load`. The bound is a parameter, so any bounds-check
/// elimination is the speculative pre-header guard and the null-check dataflow
/// has no proof for `a` at the load: the shape whose null check now tests the
/// array's home register.
fn param_bound_sum(load: u8) -> [u8; 23] {
    [
        0x03, 0x3d, 0x03, 0x3e, // s = 0 (local 2); i = 0 (local 3)
        0x1d, 0x1b, // 4: iload_3; iload_1
        0xa2, 0x00, 0x0f, // 6: if_icmpge -> 21
        0x1c, 0x2a, 0x1d, load, // 9: iload_2; aload_0; iload_3; <load>
        0x60, 0x3d, // 13: iadd; istore_2
        0x84, 0x03, 0x01, // 15: iinc 3, 1
        0xa7, 0xff, 0xf2, // 18: goto -> 4
        0x1c, 0xac, // 21: iload_2; ireturn
    ]
}

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

#[test]
fn parameter_bounded_sums_match_java_in_every_configuration() {
    type Widen = fn(i64) -> i32;
    let kinds: [(u8, usize, Widen); 4] = [
        (0x33, 1, |v| v as i8 as i32),
        (0x34, 2, |v| v as u16 as i32),
        (0x35, 2, |v| v as i16 as i32),
        (0x2e, 4, |v| v as i32),
    ];
    for (op, size, widen) in kinds {
        let code = param_bound_sum(op);
        for cfg in CONFIGS {
            let m = compile_as(&code, 2, 4, cfg);
            for (k, len) in [0usize, 1, 2, 7, 9, 31, 300].into_iter().enumerate() {
                let vals = data(len, 0xA11C_E5ED + k as u64 + u64::from(op));
                let mut a = JArray::new(size, &vals, 0);
                // Every prefix length up to the array's own, so the loop runs
                // with `n` both equal to and short of `a.length`.
                for n in [0, len / 2, len] {
                    let want = vals[..n]
                        .iter()
                        .fold(0i32, |s, &v| s.wrapping_add(widen(v)));
                    let got = call(&m, &[a.handle(), n as i64]) as i32;
                    assert_eq!(got, want, "{op:#x} {cfg:?} len {len} n {n}");
                }
            }
        }
    }
}
