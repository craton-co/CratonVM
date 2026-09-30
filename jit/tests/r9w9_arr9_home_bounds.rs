// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! JIT review round 9, wave 9, lane `arr9`: single-pass array loads that keep
//! their BOUNDS check now compare the operands in their own registers
//! (`perf-single-pass-checked-array-loads-copy-operands-into-rax-rcx-20260918.md`,
//! the half left after wave 8).
//!
//! The fast path is `cmp <index>d, [<array> + len] ; jae L_k`, and `L_k` is a
//! per-site cold prologue `mov rax, <array> ; mov rcx, <index> ; jmp <pad>`,
//! because the AIOOBE pad hands RAX/RCX to `jit_throw_aioobe`. What can go
//! wrong is therefore visible only on the FAILING path: a prologue that moved
//! the wrong registers reports the wrong index, length or array. So these
//! tests RUN compiled methods both in bounds (against a Rust oracle) and out
//! of bounds, and check the four arguments the pad passed to a recording
//! `throw_aioobe` stand-in, in the four register-home x operand-fold
//! configurations.

#![cfg(target_arch = "x86_64")]

use cratonvm_jit::x64::{compile_with_request, BackendRequest};
use cratonvm_jit::CompiledMethod;
use cratonvm_jit_api::JitRuntimeHelpers;
use cratonvm_types::{ARRAY_DATA_OFFSET, ARRAY_LENGTH_OFFSET};
use std::cell::Cell;
use std::collections::{HashMap, HashSet};

thread_local! {
    /// The `(index, length, array, bci)` of the last `throw_aioobe` call on
    /// this thread. JIT code calls the helper on the test's own thread.
    static AIOOBE: Cell<Option<(i64, i64, i64, i64)>> = const { Cell::new(None) };
}

fn helpers() -> JitRuntimeHelpers {
    unsafe extern "C" fn record_throw_bci(_bci: i64) {}
    unsafe extern "C" fn deopt_unserviceable(_vm: i64, _info: i64, _args: i64, _n: i64) -> i64 {
        i64::MIN
    }
    unsafe extern "C" fn throw_aioobe_recording(
        index: i64,
        length: i64,
        array: i64,
        pc: i64,
    ) -> i64 {
        AIOOBE.with(|c| c.set(Some((index, length, array, pc))));
        i64::MIN
    }
    JitRuntimeHelpers {
        set_throw_bci: record_throw_bci as *const () as usize,
        service_callee_deopt: deopt_unserviceable as *const () as usize,
        throw_aioobe: throw_aioobe_recording as *const () as usize,
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

/// A synthetic `int[]` holding `elems`.
struct IntArray {
    words: Vec<u64>,
}

impl IntArray {
    fn new(elems: &[i32]) -> Self {
        let bytes = ARRAY_DATA_OFFSET + (elems.len() + 8) * 4;
        let mut me = IntArray {
            words: vec![0u64; bytes.div_ceil(8)],
        };
        let base = me.words.as_mut_ptr() as *mut u8;
        // SAFETY: the allocation holds the header and every element.
        unsafe {
            std::ptr::write_unaligned(
                base.add(ARRAY_LENGTH_OFFSET) as *mut i32,
                elems.len() as i32,
            );
            for (i, v) in elems.iter().enumerate() {
                std::ptr::write_unaligned(base.add(ARRAY_DATA_OFFSET + i * 4) as *mut i32, *v);
            }
        }
        me
    }

    fn handle(&mut self) -> i64 {
        self.words.as_mut_ptr() as i64
    }
}

/// Call `m` and return its result and the AIOOBE the pad reported, if any.
fn call(m: &CompiledMethod, args: &[i64]) -> (i64, Option<(i64, i64, i64, i64)>) {
    AIOOBE.with(|c| c.set(None));
    // SAFETY: every array argument is a live synthetic array that outlives
    // the call; an out-of-bounds index reaches the recording helper, never
    // memory.
    let got = unsafe { m.try_call(args).expect("test JIT call") };
    (got, AIOOBE.with(Cell::get))
}

/// `static int get(int[] a, int i) { return a[i]; }`
const GET: [u8; 4] = [0x2a, 0x1b, 0x2e, 0xac];

#[test]
fn a_single_checked_load_reports_its_own_operands_when_out_of_bounds() {
    let vals = [10, -20, 30, i32::MIN, i32::MAX];
    for cfg in CONFIGS {
        let m = compile_as(&GET, 2, 2, cfg);
        let mut a = IntArray::new(&vals);
        let h = a.handle();
        for (i, &v) in vals.iter().enumerate() {
            let (got, thrown) = call(&m, &[h, i as i64]);
            assert_eq!(thrown, None, "{cfg:?} a[{i}] is in bounds");
            assert_eq!(got as i32, v, "{cfg:?} a[{i}]");
        }
        for bad in [5i64, 6, 1 << 20, -1, i64::from(i32::MIN)] {
            let (got, thrown) = call(&m, &[h, bad]);
            assert_eq!(got, i64::MIN, "{cfg:?} a[{bad}] returns the sentinel");
            let (index, length, array, pc) =
                thrown.unwrap_or_else(|| panic!("{cfg:?} a[{bad}] must reach the AIOOBE pad"));
            assert_eq!(index as i32, bad as i32, "{cfg:?} a[{bad}]: the index");
            assert_eq!(length as i32, 5, "{cfg:?} a[{bad}]: the length");
            assert_eq!(array, h, "{cfg:?} a[{bad}]: the array");
            assert_eq!(pc, 2, "{cfg:?} a[{bad}]: the bci of the iaload");
        }
    }
}

/// `static int gather(int[] a, int[] idx, int n) {
///     int s = 0; for (int i = 0; i < n; i++) s += a[idx[i]]; return s; }`
///
/// `a[idx[i]]` can never have its bounds check eliminated, and inside a
/// loop both operands of that load are in registers: the shape this fix is
/// for. `idx[i]` is parameter-bounded (speculative pre-header BCE at best).
const GATHER: [u8; 28] = [
    0x03, 0x3e, // 0: iconst_0; istore_3            (s = 0)
    0x03, 0x36, 0x04, // 2: iconst_0; istore 4      (i = 0)
    0x15, 0x04, // 5: iload 4
    0x1c, // 7: iload_2
    0xa2, 0x00, 0x12, // 8: if_icmpge -> 26
    0x1d, // 11: iload_3
    0x2a, // 12: aload_0
    0x2b, // 13: aload_1
    0x15, 0x04, // 14: iload 4
    0x2e, // 16: iaload                           (idx[i])
    0x2e, // 17: iaload                           (a[idx[i]])
    0x60, // 18: iadd
    0x3e, // 19: istore_3
    0x84, 0x04, 0x01, // 20: iinc 4, 1
    0xa7, 0xff, 0xee, // 23: goto -> 5
    0x1d, // 26: iload_3
    0xac, // 27: ireturn
];

#[test]
fn a_gather_loop_sums_in_bounds_and_reports_the_bad_index() {
    let a_vals: Vec<i32> = (0..37).map(|v| v * 7 - 100).collect();
    for cfg in CONFIGS {
        let m = compile_as(&GATHER, 3, 5, cfg);
        let mut a = IntArray::new(&a_vals);
        let ha = a.handle();
        // In bounds: every index, scrambled.
        let idx_vals: Vec<i32> = (0..200).map(|k| (k * 13 + 5) % 37).collect();
        let mut idx = IntArray::new(&idx_vals);
        let hi = idx.handle();
        for n in [0usize, 1, 2, 17, 200] {
            let want = idx_vals[..n]
                .iter()
                .fold(0i32, |s, &k| s.wrapping_add(a_vals[k as usize]));
            let (got, thrown) = call(&m, &[ha, hi, n as i64]);
            assert_eq!(thrown, None, "{cfg:?} n {n}: all in bounds");
            assert_eq!(got as i32, want, "{cfg:?} n {n}");
        }
        // Out of bounds at position 3: one past the end, then negative.
        for bad in [37i32, 38, -1, i32::MIN] {
            let mut bad_idx = idx_vals.clone();
            bad_idx[3] = bad;
            let mut idx2 = IntArray::new(&bad_idx);
            let hi2 = idx2.handle();
            let (got, thrown) = call(&m, &[ha, hi2, 10]);
            assert_eq!(got, i64::MIN, "{cfg:?} bad {bad}: the sentinel");
            let (index, length, array, pc) =
                thrown.unwrap_or_else(|| panic!("{cfg:?} bad {bad} must reach the AIOOBE pad"));
            assert_eq!(index as i32, bad, "{cfg:?} bad {bad}: the index");
            assert_eq!(length as i32, 37, "{cfg:?} bad {bad}: the length of a");
            assert_eq!(array, ha, "{cfg:?} bad {bad}: the array is a, not idx");
            assert_eq!(pc, 17, "{cfg:?} bad {bad}: the bci of the outer iaload");
        }
    }
}
