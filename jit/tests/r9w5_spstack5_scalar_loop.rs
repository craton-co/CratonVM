// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! JIT review round 9, wave 5, lane `spstack5`: executed fixtures for the
//! single-pass operand folding
//! (`perf-single-pass-scalar-loop-round-trips-every-operand-through-a-scratch-register-20260918.md`).
//!
//! Each method is javac-shaped bytecode for a scalar loop, compiled by the
//! single-pass backend in four configurations and RUN:
//!
//! * `kernel_reg_homes` on (the pure-kernel path: int/long locals in
//!   callee-saved registers, operands cached in R8/R9 — where the folding
//!   does the most) and off (frame-homed locals, frame-word operands);
//! * the folding on (the default) and off (`CRATONVM_JIT_NO_OPERAND_FOLD=1`,
//!   the pre-wave-5 lowering).
//!
//! Every answer is compared with the same loop written in Rust with Java's
//! wrapping semantics. The shapes cover what the folding touches: in-place
//! ALU ops on cached operands, `x = x OP y` stores fused into the local's
//! home (left and right operand), constant ALU/shift arms, `i2l`, variable
//! shifts, forward `if_icmp*`/`if*` with their operands as the whole stack,
//! a ternary that keeps an operand live across a merge, and an ecj-rotated
//! loop whose test is a BACKWARD `if_icmplt`.

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
    unsafe extern "C" fn throw_aioobe_stub(_index: i64, _length: i64, _array: i64, _pc: i64) -> i64 {
        i64::MIN
    }
    JitRuntimeHelpers {
        set_throw_bci: record_throw_bci as *const () as usize,
        service_callee_deopt: deopt_unserviceable as *const () as usize,
        throw_aioobe: throw_aioobe_stub as *const () as usize,
        ..Default::default()
    }
}

/// One compile configuration.
#[derive(Clone, Copy, Debug)]
struct Config {
    kernel: bool,
    fold: bool,
}

const CONFIGS: [Config; 4] = [
    Config { kernel: true, fold: true },
    Config { kernel: true, fold: false },
    Config { kernel: false, fold: true },
    Config { kernel: false, fold: false },
];

/// Compile `code` (two padding bytes appended, as the in-crate fixtures do)
/// under `cfg`.
fn compile_as(code: &[u8], num_params: usize, max_locals: usize, cfg: Config) -> CompiledMethod {
    let mut padded = code.to_vec();
    padded.extend_from_slice(&[0, 0]);
    let fold_edit = if cfg.fold { None } else { Some("1") };
    cratonvm_types::flags::with_thread_overrides(&[("CRATONVM_JIT_NO_OPERAND_FOLD", fold_edit)], || {
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
    })
    .unwrap_or_else(|| panic!("fixture compiles under {cfg:?}"))
}

fn call(m: &CompiledMethod, args: &[i64]) -> i64 {
    // SAFETY: every fixture is a self-contained static method over int/long
    // arguments and synthetic arrays that stay alive for the call; it reaches
    // no runtime helper on these inputs (every index is in bounds).
    unsafe { m.try_call(args).expect("test JIT call") }
}

/// A synthetic primitive array: length at `ARRAY_LENGTH_OFFSET`, elements
/// from `ARRAY_DATA_OFFSET`.
struct JArray {
    words: Vec<u64>,
}

impl JArray {
    fn new(elem_size: usize, len: usize, write: impl Fn(*mut u8, usize)) -> Self {
        let bytes = ARRAY_DATA_OFFSET + (len + 8) * elem_size;
        let mut me = JArray {
            words: vec![0u64; bytes.div_ceil(8)],
        };
        let base = me.words.as_mut_ptr() as *mut u8;
        // SAFETY: the allocation holds the header and `len + 8` elements.
        unsafe {
            std::ptr::write_unaligned(base.add(ARRAY_LENGTH_OFFSET) as *mut i32, len as i32);
        }
        for i in 0..len {
            // SAFETY: element `i < len` is inside the allocation.
            write(unsafe { base.add(ARRAY_DATA_OFFSET + i * elem_size) }, i);
        }
        me
    }

    fn bytes(v: &[i8]) -> Self {
        // SAFETY (closure): `p` points at element `i` of a byte array.
        Self::new(1, v.len(), |p, i| unsafe { std::ptr::write_unaligned(p as *mut i8, v[i]) })
    }

    fn ints(v: &[i32]) -> Self {
        // SAFETY (closure): `p` points at element `i` of an int array.
        Self::new(4, v.len(), |p, i| unsafe { std::ptr::write_unaligned(p as *mut i32, v[i]) })
    }

    fn longs(v: &[i64]) -> Self {
        // SAFETY (closure): `p` points at element `i` of a long array.
        Self::new(8, v.len(), |p, i| unsafe { std::ptr::write_unaligned(p as *mut i64, v[i]) })
    }

    fn handle(&mut self) -> i64 {
        self.words.as_mut_ptr() as i64
    }
}

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

const LENGTHS: [usize; 8] = [0, 1, 2, 7, 8, 9, 31, 1000];

/// `static int bsum(byte[] a) { int s = 0; for (int i = 0; i < a.length; i++) s += a[i]; return s; }`
const BSUM: [u8; 24] = [
    0x03, 0x3c, 0x03, 0x3d, // s = 0; i = 0
    0x1c, 0x2a, 0xbe, // 4: iload_2; aload_0; arraylength
    0xa2, 0x00, 0x0f, // 7: if_icmpge -> 22
    0x1b, 0x2a, 0x1c, 0x33, // 10: iload_1; aload_0; iload_2; baload
    0x60, 0x3c, // 14: iadd; istore_1
    0x84, 0x02, 0x01, // 16: iinc 2, 1
    0xa7, 0xff, 0xf1, // 19: goto -> 4
    0x1b, 0xac, // 22: iload_1; ireturn
];

/// The same loop as ecj emits it: `goto COND; BODY; COND: ... if_icmplt BODY`
/// — the loop test is a BACKWARD compare, which keeps the flush-first order.
const BSUM_ROTATED: [u8; 24] = [
    0x03, 0x3c, 0x03, 0x3d, // s = 0; i = 0
    0xa7, 0x00, 0x0c, // 4: goto -> 16
    0x1b, 0x2a, 0x1c, 0x33, // 7: iload_1; aload_0; iload_2; baload
    0x60, 0x3c, // 11: iadd; istore_1
    0x84, 0x02, 0x01, // 13: iinc 2, 1
    0x1c, 0x2a, 0xbe, // 16: iload_2; aload_0; arraylength
    0xa1, 0xff, 0xf4, // 19: if_icmplt -> 7
    0x1b, 0xac, // 22: iload_1; ireturn
];

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

/// `static long isum(int[] a) { long s = 0; for (int i = 0; i < a.length; i++) s += a[i]; return s; }`
/// — the page's own loop (`iaload; i2l; ladd; lstore`).
const ISUM_LONG: [u8; 25] = [
    0x09, 0x40, 0x03, 0x3e, // s = 0L; i = 0
    0x1d, 0x2a, 0xbe, // 4: iload_3; aload_0; arraylength
    0xa2, 0x00, 0x10, // 7: if_icmpge -> 23
    0x1f, 0x2a, 0x1d, 0x2e, // 10: lload_1; aload_0; iload_3; iaload
    0x85, 0x61, 0x40, // 14: i2l; ladd; lstore_1
    0x84, 0x03, 0x01, // 17: iinc 3, 1
    0xa7, 0xff, 0xf0, // 20: goto -> 4
    0x1f, 0xad, // 23: lload_1; lreturn
];

/// `static int recur(int n) { int h = 17; for (int i = 0; i < n; i++) { h = h * 31 + (i ^ (h >>> 7)); h -= i; } return h; }`
const RECUR: [u8; 34] = [
    0x10, 0x11, 0x3c, // 0: bipush 17; istore_1
    0x03, 0x3d, // 3: iconst_0; istore_2
    0x1c, 0x1a, // 5: iload_2; iload_0
    0xa2, 0x00, 0x19, // 7: if_icmpge -> 32
    0x1b, 0x10, 0x1f, 0x68, // 10: iload_1; bipush 31; imul
    0x1c, 0x1b, 0x10, 0x07, 0x7c, // 14: iload_2; iload_1; bipush 7; iushr
    0x82, 0x60, 0x3c, // 19: ixor; iadd; istore_1
    0x1b, 0x1c, 0x64, 0x3c, // 22: iload_1; iload_2; isub; istore_1
    0x84, 0x02, 0x01, // 26: iinc 2, 1
    0xa7, 0xff, 0xe8, // 29: goto -> 5
    0x1b, 0xac, // 32: iload_1; ireturn
];

/// ```java
/// static long lrecur(int n) {
///     long h = 1;
///     for (int i = 0; i < n; i++) {
///         h = (h << (i & 7)) ^ ((h >>> i) - (long) i);
///         h = -h * h + h;
///     }
///     return h;
/// }
/// ```
const LRECUR: [u8; 38] = [
    0x0a, 0x40, 0x03, 0x3e, // h = 1L (locals 1-2); i = 0 (local 3)
    0x1d, 0x1a, // 4: iload_3; iload_0
    0xa2, 0x00, 0x1e, // 6: if_icmpge -> 36
    0x1f, 0x1d, 0x10, 0x07, 0x7e, 0x79, // 9: lload_1; iload_3; bipush 7; iand; lshl
    0x1f, 0x1d, 0x7d, // 15: lload_1; iload_3; lushr
    0x1d, 0x85, 0x65, // 18: iload_3; i2l; lsub
    0x83, 0x40, // 21: lxor; lstore_1
    0x1f, 0x75, 0x1f, 0x69, // 23: lload_1; lneg; lload_1; lmul
    0x1f, 0x61, 0x40, // 27: lload_1; ladd; lstore_1   (h is the RIGHT operand)
    0x84, 0x03, 0x01, // 30: iinc 3, 1
    0xa7, 0xff, 0xe3, // 33: goto -> 4
    0x1f, 0xad, // 36: lload_1; lreturn
];

/// `static int cnt(int[] a, int k) { int c = 0; for (int i = 0; i < a.length; i++) { if (a[i] > k) c++; } return c; }`
const CNT: [u8; 28] = [
    0x03, 0x3d, 0x03, 0x3e, // c = 0 (local 2); i = 0 (local 3)
    0x1d, 0x2a, 0xbe, // 4: iload_3; aload_0; arraylength
    0xa2, 0x00, 0x13, // 7: if_icmpge -> 26
    0x2a, 0x1d, 0x2e, 0x1b, // 10: aload_0; iload_3; iaload; iload_1
    0xa4, 0x00, 0x06, // 14: if_icmple -> 20
    0x84, 0x02, 0x01, // 17: iinc 2, 1
    0x84, 0x03, 0x01, // 20: iinc 3, 1
    0xa7, 0xff, 0xed, // 23: goto -> 4
    0x1c, 0xac, // 26: iload_2; ireturn
];

/// `static int nonZero(int[] a) { int c = 0; for (int i = 0; i < a.length; i++) { if (a[i] != 0) c++; } return c; }`
/// — a forward `ifeq` whose operand is the whole stack.
const NON_ZERO: [u8; 27] = [
    0x03, 0x3c, 0x03, 0x3d, // c = 0; i = 0
    0x1c, 0x2a, 0xbe, // 4: iload_2; aload_0; arraylength
    0xa2, 0x00, 0x12, // 7: if_icmpge -> 25
    0x2a, 0x1c, 0x2e, // 10: aload_0; iload_2; iaload
    0x99, 0x00, 0x06, // 13: ifeq -> 19
    0x84, 0x01, 0x01, // 16: iinc 1, 1
    0x84, 0x02, 0x01, // 19: iinc 2, 1
    0xa7, 0xff, 0xee, // 22: goto -> 4
    0x1b, 0xac, // 25: iload_1; ireturn
];

/// `static int absSum(int[] a) { int s = 0; for (int i = 0; i < a.length; i++) { int x = a[i]; s += x > 0 ? x : -x; } return s; }`
/// — `s` stays on the stack across the ternary's merge.
const ABS_SUM: [u8; 35] = [
    0x03, 0x3c, 0x03, 0x3d, // s = 0; i = 0
    0x1c, 0x2a, 0xbe, // 4: iload_2; aload_0; arraylength
    0xa2, 0x00, 0x1a, // 7: if_icmpge -> 33
    0x2a, 0x1c, 0x2e, 0x3e, // 10: aload_0; iload_2; iaload; istore_3
    0x1b, 0x1d, // 14: iload_1; iload_3
    0x9e, 0x00, 0x07, // 16: ifle -> 23
    0x1d, // 19: iload_3
    0xa7, 0x00, 0x05, // 20: goto -> 25
    0x1d, 0x74, // 23: iload_3; ineg
    0x60, 0x3c, // 25: iadd; istore_1
    0x84, 0x02, 0x01, // 27: iinc 2, 1
    0xa7, 0xff, 0xe6, // 30: goto -> 4
    0x1b, 0xac, // 33: iload_1; ireturn
];

fn recur_ref(n: i32) -> i32 {
    let mut h: i32 = 17;
    for i in 0..n {
        // Cast: `>>>` on the int's bits.
        let ushr = ((h as u32) >> 7) as i32;
        h = h.wrapping_mul(31).wrapping_add(i ^ ushr);
        h = h.wrapping_sub(i);
    }
    h
}

fn lrecur_ref(n: i32) -> i64 {
    let mut h: i64 = 1;
    for i in 0..n {
        // Cast: Java masks a long shift count to its low six bits.
        let shl = (i & 7) as u32;
        let ushr = ((h as u64) >> ((i & 63) as u32)) as i64;
        h = h.wrapping_shl(shl) ^ ushr.wrapping_sub(i64::from(i));
        h = h.wrapping_neg().wrapping_mul(h).wrapping_add(h);
    }
    h
}

#[test]
fn byte_sums_match_the_host_in_every_configuration() {
    for cfg in CONFIGS {
        for code in [&BSUM, &BSUM_ROTATED] {
            let m = compile_as(code, 1, 3, cfg);
            for len in LENGTHS {
                // Cast: the low byte of the data, signed.
                let v: Vec<i8> = data(len, 7).iter().map(|&x| x as i8).collect();
                let mut arr = JArray::bytes(&v);
                let want = v.iter().fold(0i32, |s, &b| s.wrapping_add(i32::from(b)));
                assert_eq!(call(&m, &[arr.handle()]), i64::from(want), "{cfg:?} len={len}");
            }
        }
    }
}

#[test]
fn long_sums_match_the_host_in_every_configuration() {
    for cfg in CONFIGS {
        let m = compile_as(&LSUM, 1, 4, cfg);
        let n = compile_as(&ISUM_LONG, 1, 4, cfg);
        for len in LENGTHS {
            let lv = data(len, 11);
            let mut la = JArray::longs(&lv);
            let want = lv.iter().fold(0i64, |s, &x| s.wrapping_add(x));
            assert_eq!(call(&m, &[la.handle()]), want, "lsum {cfg:?} len={len}");

            // Cast: the low half of the data, signed.
            let iv: Vec<i32> = data(len, 13).iter().map(|&x| x as i32).collect();
            let mut ia = JArray::ints(&iv);
            let want = iv.iter().fold(0i64, |s, &x| s.wrapping_add(i64::from(x)));
            assert_eq!(call(&n, &[ia.handle()]), want, "isum {cfg:?} len={len}");
        }
    }
}

#[test]
fn recurrences_match_the_host_in_every_configuration() {
    for cfg in CONFIGS {
        let m = compile_as(&RECUR, 1, 3, cfg);
        let l = compile_as(&LRECUR, 1, 4, cfg);
        for n in [0i32, 1, 2, 3, 7, 64, 4097] {
            assert_eq!(call(&m, &[i64::from(n)]), i64::from(recur_ref(n)), "recur {cfg:?} n={n}");
            assert_eq!(call(&l, &[i64::from(n)]), lrecur_ref(n), "lrecur {cfg:?} n={n}");
        }
    }
}

#[test]
fn branchy_loops_match_the_host_in_every_configuration() {
    for cfg in CONFIGS {
        let cnt = compile_as(&CNT, 2, 4, cfg);
        let nz = compile_as(&NON_ZERO, 1, 3, cfg);
        let abs = compile_as(&ABS_SUM, 1, 4, cfg);
        for len in LENGTHS {
            // Cast: the low half, with some zeros mixed in.
            let v: Vec<i32> = data(len, 17)
                .iter()
                .enumerate()
                .map(|(i, &x)| if i % 3 == 0 { 0 } else { x as i32 })
                .collect();
            let mut a = JArray::ints(&v);
            for k in [i32::MIN, -5, 0, 1 << 20, i32::MAX] {
                let want = v.iter().filter(|&&x| x > k).count() as i64;
                assert_eq!(call(&cnt, &[a.handle(), i64::from(k)]), want, "cnt {cfg:?} len={len} k={k}");
            }
            let want = v.iter().filter(|&&x| x != 0).count() as i64;
            assert_eq!(call(&nz, &[a.handle()]), want, "nonZero {cfg:?} len={len}");
            let want = v
                .iter()
                .fold(0i32, |s, &x| s.wrapping_add(if x > 0 { x } else { x.wrapping_neg() }));
            assert_eq!(call(&abs, &[a.handle()]), i64::from(want), "absSum {cfg:?} len={len}");
        }
    }
}

/// The folding must actually engage on the pure-kernel path: the same method
/// compiles SHORTER with it than under the kill switch. (Every result above is
/// already checked; this pins that the fold arm is the one being checked.)
#[test]
fn the_fold_shortens_the_pure_kernel_bodies() {
    for (name, code, params, locals) in [
        ("recur", &RECUR[..], 1, 3),
        ("lrecur", &LRECUR[..], 1, 4),
        ("bsum", &BSUM[..], 1, 3),
        ("lsum", &LSUM[..], 1, 4),
    ] {
        let fold = compile_as(code, params, locals, Config { kernel: true, fold: true });
        let old = compile_as(code, params, locals, Config { kernel: true, fold: false });
        assert!(
            fold.code_len() < old.code_len(),
            "{name}: folded body is {} bytes, unfolded {} — the fold did not engage",
            fold.code_len(),
            old.code_len()
        );
    }
}
