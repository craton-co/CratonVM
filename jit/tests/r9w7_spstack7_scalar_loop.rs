// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! JIT review round 9, wave 7, lane `spstack7`: executed fixtures for the
//! remaining single-pass scalar-loop folds of
//! `perf-single-pass-scalar-loop-round-trips-every-operand-through-a-scratch-register-20260918.md`:
//!
//! * an `i2l` right after an int array load or an int ALU binop is consumed
//!   (`skip_redundant_i2l`) — but never one that is a branch target;
//! * a binop whose result is stored to a register-homed local that is NOT
//!   an operand (or is the right operand of `isub`) moves the result there
//!   directly (`emit_gpr_binop` step 4);
//! * `x * K` for a K with no strength-reduced form is one three-operand IMUL;
//! * `lshl`/`lshr`/`lushr` by an int constant is one immediate shift.
//!
//! Each method is javac-shaped bytecode compiled by the single-pass backend in
//! four configurations (pure-kernel register homes on/off x folding on/off,
//! the latter `CRATONVM_JIT_NO_OPERAND_FOLD=1`) and RUN; every answer is
//! compared with the same loop in Rust with Java's wrapping semantics.

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

/// Compile `code` (two padding bytes appended, as the in-crate fixtures do)
/// under `cfg`.
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
        Self::new(1, v.len(), |p, i| unsafe {
            std::ptr::write_unaligned(p as *mut i8, v[i])
        })
    }

    fn shorts(v: &[i16]) -> Self {
        // SAFETY (closure): `p` points at element `i` of a short/char array.
        Self::new(2, v.len(), |p, i| unsafe {
            std::ptr::write_unaligned(p as *mut i16, v[i])
        })
    }

    fn ints(v: &[i32]) -> Self {
        // SAFETY (closure): `p` points at element `i` of an int array.
        Self::new(4, v.len(), |p, i| unsafe {
            std::ptr::write_unaligned(p as *mut i32, v[i])
        })
    }

    fn longs(v: &[i64]) -> Self {
        // SAFETY (closure): `p` points at element `i` of a long array.
        Self::new(8, v.len(), |p, i| unsafe {
            std::ptr::write_unaligned(p as *mut i64, v[i])
        })
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

/// `static long xsum(T[] a) { long s = 0; for (int i = 0; i < a.length; i++) s += a[i]; return s; }`
/// with the element load at byte 13 (`iaload` here; the tests patch in
/// `baload`/`caload`/`saload`) and its `i2l` right after it.
const XSUM_LONG: [u8; 25] = [
    0x09, 0x40, 0x03, 0x3e, // s = 0L; i = 0
    0x1d, 0x2a, 0xbe, // 4: iload_3; aload_0; arraylength
    0xa2, 0x00, 0x10, // 7: if_icmpge -> 23
    0x1f, 0x2a, 0x1d, 0x2e, // 10: lload_1; aload_0; iload_3; iaload
    0x85, 0x61, 0x40, // 14: i2l; ladd; lstore_1
    0x84, 0x03, 0x01, // 17: iinc 3, 1
    0xa7, 0xff, 0xf0, // 20: goto -> 4
    0x1f, 0xad, // 23: lload_1; lreturn
];
const XSUM_LOAD_AT: usize = 13;

/// `static long orsum(int[] a) { long s = 0; for (int i = 0; i < a.length; i++) s += a[i] | i; return s; }`
/// — `ior; i2l`.
const OR_SUM_LONG: [u8; 27] = [
    0x09, 0x40, 0x03, 0x3e, // s = 0L; i = 0
    0x1d, 0x2a, 0xbe, // 4: iload_3; aload_0; arraylength
    0xa2, 0x00, 0x12, // 7: if_icmpge -> 25
    0x1f, 0x2a, 0x1d, 0x2e, // 10: lload_1; aload_0; iload_3; iaload
    0x1d, 0x80, 0x85, // 14: iload_3; ior; i2l
    0x61, 0x40, // 17: ladd; lstore_1
    0x84, 0x03, 0x01, // 19: iinc 3, 1
    0xa7, 0xff, 0xee, // 22: goto -> 4
    0x1f, 0xad, // 25: lload_1; lreturn
];

/// `static int sub(int[] a) { int s = 0; for (int i = 0; i < a.length; i++) s = a[i] - s; return s; }`
/// — the store's local is the RIGHT operand of `isub`.
const SUB_RIGHT: [u8; 24] = [
    0x03, 0x3c, 0x03, 0x3d, // s = 0; i = 0
    0x1c, 0x2a, 0xbe, // 4: iload_2; aload_0; arraylength
    0xa2, 0x00, 0x0f, // 7: if_icmpge -> 22
    0x2a, 0x1c, 0x2e, 0x1b, // 10: aload_0; iload_2; iaload; iload_1
    0x64, 0x3c, // 14: isub; istore_1
    0x84, 0x02, 0x01, // 16: iinc 2, 1
    0xa7, 0xff, 0xf1, // 19: goto -> 4
    0x1b, 0xac, // 22: iload_1; ireturn
];

/// ```java
/// static long shifts(long[] a) {
///     long s = 0;
///     for (int i = 0; i < a.length; i++) {
///         long x = a[i];
///         s += (x >> 3) ^ (x << 7) ^ (x >>> 61) ^ (x << 64) ^ (x >> -1);
///     }
///     return s;
/// }
/// ```
/// `x << 64` is `x` (count 0 after masking) and `x >> -1` is `x >> 63`.
const LONG_SHIFTS: [u8; 53] = [
    0x09, 0x40, 0x03, 0x3e, // s = 0L (1-2); i = 0 (3)
    0x1d, 0x2a, 0xbe, // 4: iload_3; aload_0; arraylength
    0xa2, 0x00, 0x2c, // 7: if_icmpge -> 51
    0x2a, 0x1d, 0x2f, 0x37, 0x04, // 10: aload_0; iload_3; laload; lstore 4
    0x1f, // 15: lload_1
    0x16, 0x04, 0x06, 0x7b, // 16: lload 4; iconst_3; lshr
    0x16, 0x04, 0x10, 0x07, 0x79, // 20: lload 4; bipush 7; lshl
    0x83, // 25: lxor
    0x16, 0x04, 0x10, 0x3d, 0x7d, // 26: lload 4; bipush 61; lushr
    0x83, // 31: lxor
    0x16, 0x04, 0x10, 0x40, 0x79, // 32: lload 4; bipush 64; lshl
    0x83, // 37: lxor
    0x16, 0x04, 0x02, 0x7b, // 38: lload 4; iconst_m1; lshr
    0x83, // 42: lxor
    0x61, 0x40, // 43: ladd; lstore_1
    0x84, 0x03, 0x01, // 45: iinc 3, 1
    0xa7, 0xff, 0xd4, // 48: goto -> 4
    0x1f, 0xad, // 51: lload_1; lreturn
];

/// ```java
/// static int muls(int n) {
///     int h = 1;
///     for (int i = 0; i < n; i++) h = (h * -7 + i * 1000 + h * 30001) ^ (i * 6);
///     return h;
/// }
/// ```
/// Four constant multiplies with no strength-reduced form, and a store to
/// `h` whose final binop (`ixor`) has no `h` operand.
const MULS: [u8; 39] = [
    0x04, 0x3c, 0x03, 0x3d, // h = 1; i = 0
    0x1c, 0x1a, // 4: iload_2; iload_0
    0xa2, 0x00, 0x1f, // 6: if_icmpge -> 37
    0x1b, 0x10, 0xf9, 0x68, // 9: iload_1; bipush -7; imul
    0x1c, 0x11, 0x03, 0xe8, 0x68, // 13: iload_2; sipush 1000; imul
    0x60, // 18: iadd
    0x1b, 0x11, 0x75, 0x31, 0x68, // 19: iload_1; sipush 30001; imul
    0x60, // 24: iadd
    0x1c, 0x10, 0x06, 0x68, // 25: iload_2; bipush 6; imul
    0x82, 0x3c, // 29: ixor; istore_1
    0x84, 0x02, 0x01, // 31: iinc 2, 1
    0xa7, 0xff, 0xe2, // 34: goto -> 4
    0x1b, 0xac, // 37: iload_1; ireturn
];

/// ```java
/// static long tern(int[] a, int k) {
///     long s = 0;
///     for (int i = 0; i < a.length; i++) s += a[i] > 0 ? a[i] + k : k - a[i];
///     return s;
/// }
/// ```
/// The `i2l` after the ternary's `isub` is a MERGE point (the `goto` of the
/// other arm lands on it), so it must not be consumed.
const TERNARY_I2L: [u8; 46] = [
    0x09, 0x41, 0x03, 0x36, 0x04, // s = 0L (2-3); i = 0 (4)
    0x15, 0x04, 0x2a, 0xbe, // 5: iload 4; aload_0; arraylength
    0xa2, 0x00, 0x23, // 9: if_icmpge -> 44
    0x20, // 12: lload_2
    0x2a, 0x15, 0x04, 0x2e, // 13: aload_0; iload 4; iaload
    0x9e, 0x00, 0x0c, // 17: ifle -> 29
    0x2a, 0x15, 0x04, 0x2e, 0x1b, 0x60, // 20: aload_0; iload 4; iaload; iload_1; iadd
    0xa7, 0x00, 0x09, // 26: goto -> 35
    0x1b, 0x2a, 0x15, 0x04, 0x2e, 0x64, // 29: iload_1; aload_0; iload 4; iaload; isub
    0x85, 0x61, 0x41, // 35: i2l; ladd; lstore_2
    0x84, 0x04, 0x01, // 38: iinc 4, 1
    0xa7, 0xff, 0xdc, // 41: goto -> 5
    0x20, 0xad, // 44: lload_2; lreturn
];

fn with_load(op: u8) -> [u8; 25] {
    let mut code = XSUM_LONG;
    code[XSUM_LOAD_AT] = op;
    code
}

#[test]
fn an_i2l_after_every_int_array_load_matches_the_host() {
    for cfg in CONFIGS {
        let isum = compile_as(&with_load(0x2e), 1, 4, cfg);
        let bsum = compile_as(&with_load(0x33), 1, 4, cfg);
        let csum = compile_as(&with_load(0x34), 1, 4, cfg);
        let ssum = compile_as(&with_load(0x35), 1, 4, cfg);
        for len in LENGTHS {
            let raw = data(len, 29);
            // Cast: the low half of the data, signed.
            let iv: Vec<i32> = raw.iter().map(|&x| x as i32).collect();
            let mut ia = JArray::ints(&iv);
            let want = iv.iter().fold(0i64, |s, &x| s.wrapping_add(i64::from(x)));
            assert_eq!(
                call(&isum, &[ia.handle()]),
                want,
                "iaload {cfg:?} len={len}"
            );

            // Cast: the low byte, signed.
            let bv: Vec<i8> = raw.iter().map(|&x| x as i8).collect();
            let mut ba = JArray::bytes(&bv);
            let want = bv.iter().fold(0i64, |s, &x| s.wrapping_add(i64::from(x)));
            assert_eq!(
                call(&bsum, &[ba.handle()]),
                want,
                "baload {cfg:?} len={len}"
            );

            // Cast: the low 16 bits; negative as a short, large as a char.
            let sv: Vec<i16> = raw.iter().map(|&x| x as i16).collect();
            let mut sa = JArray::shorts(&sv);
            // Cast: a char is the same bits, unsigned.
            let want = sv
                .iter()
                .fold(0i64, |s, &x| s.wrapping_add(i64::from(x as u16)));
            assert_eq!(
                call(&csum, &[sa.handle()]),
                want,
                "caload {cfg:?} len={len}"
            );
            let want = sv.iter().fold(0i64, |s, &x| s.wrapping_add(i64::from(x)));
            assert_eq!(
                call(&ssum, &[sa.handle()]),
                want,
                "saload {cfg:?} len={len}"
            );
        }
    }
}

#[test]
fn an_i2l_after_an_int_binop_matches_the_host() {
    for cfg in CONFIGS {
        let m = compile_as(&OR_SUM_LONG, 1, 4, cfg);
        for len in LENGTHS {
            // Cast: the low half of the data, signed.
            let v: Vec<i32> = data(len, 31).iter().map(|&x| x as i32).collect();
            let mut a = JArray::ints(&v);
            let want = v
                .iter()
                .enumerate()
                // Cast: `i` is an int loop index.
                .fold(0i64, |s, (i, &x)| s.wrapping_add(i64::from(x | i as i32)));
            assert_eq!(call(&m, &[a.handle()]), want, "orsum {cfg:?} len={len}");
        }
    }
}

#[test]
fn an_i2l_at_a_merge_point_is_kept_and_matches_the_host() {
    for cfg in CONFIGS {
        let m = compile_as(&TERNARY_I2L, 2, 5, cfg);
        for len in LENGTHS {
            // Cast: the low half, some of it non-positive.
            let v: Vec<i32> = data(len, 37).iter().map(|&x| x as i32).collect();
            let mut a = JArray::ints(&v);
            for k in [i32::MIN, -3, 0, 7, i32::MAX] {
                let want = v.iter().fold(0i64, |s, &x| {
                    let t = if x > 0 {
                        x.wrapping_add(k)
                    } else {
                        k.wrapping_sub(x)
                    };
                    s.wrapping_add(i64::from(t))
                });
                assert_eq!(
                    call(&m, &[a.handle(), i64::from(k)]),
                    want,
                    "tern {cfg:?} len={len} k={k}"
                );
            }
        }
    }
}

#[test]
fn stores_to_a_home_that_is_not_the_left_operand_match_the_host() {
    for cfg in CONFIGS {
        let sub = compile_as(&SUB_RIGHT, 1, 3, cfg);
        let muls = compile_as(&MULS, 1, 3, cfg);
        for len in LENGTHS {
            // Cast: the low half of the data, signed.
            let v: Vec<i32> = data(len, 41).iter().map(|&x| x as i32).collect();
            let mut a = JArray::ints(&v);
            let want = v.iter().fold(0i32, |s, &x| x.wrapping_sub(s));
            assert_eq!(
                call(&sub, &[a.handle()]),
                i64::from(want),
                "sub {cfg:?} len={len}"
            );
        }
        for n in [0i32, 1, 2, 3, 7, 64, 4097] {
            let mut h: i32 = 1;
            for i in 0..n {
                h = h
                    .wrapping_mul(-7)
                    .wrapping_add(i.wrapping_mul(1000))
                    .wrapping_add(h.wrapping_mul(30001))
                    ^ i.wrapping_mul(6);
            }
            assert_eq!(
                call(&muls, &[i64::from(n)]),
                i64::from(h),
                "muls {cfg:?} n={n}"
            );
        }
    }
}

#[test]
fn constant_long_shifts_match_the_host() {
    for cfg in CONFIGS {
        let m = compile_as(&LONG_SHIFTS, 1, 6, cfg);
        for len in LENGTHS {
            let v = data(len, 43);
            let mut a = JArray::longs(&v);
            let want = v.iter().fold(0i64, |s, &x| {
                // Cast: `>>>` on the long's bits.
                let ushr = ((x as u64) >> 61) as i64;
                s.wrapping_add((x >> 3) ^ (x << 7) ^ ushr ^ x ^ (x >> 63))
            });
            assert_eq!(call(&m, &[a.handle()]), want, "shifts {cfg:?} len={len}");
        }
    }
}
