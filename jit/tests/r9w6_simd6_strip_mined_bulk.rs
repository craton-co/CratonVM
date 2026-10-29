// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! JIT review round 9, wave 6, lane `simd6`: executed fixtures for the
//! strip-mined bulk byte pre-headers
//! (`simd-preheaders-skip-arrays-longer-than-the-poll-free-cap-20260918.md`).
//!
//! Before wave 6 the zero fill, the strided store and the byte sieve skipped
//! their pre-header outright once the span passed `MAX_BULK_BYTE_LOOP_SPAN`
//! (2^20), so a `boolean[]` one element over the cap was filled entirely by
//! the scalar loop. Each now clamps one pass to the cap (the sieve: to a work
//! budget) and publishes the loop state at that point; the scalar loop then
//! continues from it. Each test compiles javac-shaped bytecode and pins:
//!
//! * the stored bytes (and the sieve's prime count) equal a Rust oracle, for
//!   spans below, at and well past the cap, with guard bytes past the array
//!   untouched;
//! * the clamping code was emitted (its signature bytes), so a silent
//!   fall-back to the whole-scalar loop cannot pass.
//!
//! The fixture helpers leave `safepoint_flag_addr` at 0, so no poll is
//! emitted: these runs cover the arithmetic of the clamp and the hand-off to
//! the scalar loop, not the GC-under-allocation run the page prescribes.

#![cfg(target_arch = "x86_64")]

use cratonvm_jit::x64::compile;
use cratonvm_jit::CompiledMethod;
use cratonvm_jit_api::JitRuntimeHelpers;
use cratonvm_types::{ARRAY_DATA_OFFSET, ARRAY_LENGTH_OFFSET};
use std::collections::{HashMap, HashSet};

/// `MAX_BULK_BYTE_LOOP_SPAN` (`jit/src/x64/escape_analysis.rs`).
const CAP: usize = 1 << 20;

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

/// Compile `code` (two padding bytes appended, as the in-crate fixtures do).
fn compile_fixture(code: &[u8], num_params: usize, max_locals: usize) -> CompiledMethod {
    let mut padded = code.to_vec();
    padded.extend_from_slice(&[0, 0]);
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
    .expect("fixture compiles")
}

/// A synthetic `boolean[]`: length at `ARRAY_LENGTH_OFFSET`, `len` bytes from
/// `ARRAY_DATA_OFFSET`, then `GUARD` bytes that must keep their fill.
struct Bools {
    words: Vec<u64>,
    len: usize,
}

impl Bools {
    const GUARD: usize = 64;
    const GUARD_FILL: u8 = 0x5A;

    fn new(len: usize, fill: u8) -> Self {
        let bytes = ARRAY_DATA_OFFSET + len + Self::GUARD;
        let mut me = Bools {
            words: vec![0u64; bytes.div_ceil(8)],
            len,
        };
        let base = me.base();
        // SAFETY: the allocation holds the header, the elements and the guard.
        unsafe {
            std::ptr::write_unaligned(base.add(ARRAY_LENGTH_OFFSET) as *mut i32, len as i32);
            std::ptr::write_bytes(base.add(ARRAY_DATA_OFFSET), fill, len);
            std::ptr::write_bytes(
                base.add(ARRAY_DATA_OFFSET + len),
                Self::GUARD_FILL,
                Self::GUARD,
            );
        }
        me
    }

    fn base(&mut self) -> *mut u8 {
        self.words.as_mut_ptr() as *mut u8
    }

    fn handle(&mut self) -> i64 {
        self.base() as i64
    }

    fn elements(&mut self) -> Vec<u8> {
        let len = self.len;
        let base = self.base();
        // SAFETY: `len` element bytes start at the data offset.
        unsafe { std::slice::from_raw_parts(base.add(ARRAY_DATA_OFFSET), len).to_vec() }
    }

    fn guard_is_untouched(&mut self) -> bool {
        let start = ARRAY_DATA_OFFSET + self.len;
        let base = self.base();
        // SAFETY: the guard region is inside the allocation.
        (0..Self::GUARD).all(|g| unsafe { *base.add(start + g) } == Self::GUARD_FILL)
    }
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|w| w == needle)
}

/// `emit_inclusive_strip_clamp`: `CMP EDX, cap-1 ; JBE +7 ; LEA R11D, [R10 + cap-1]`.
const INCLUSIVE_CLAMP: [u8; 15] = [
    0x81, 0xFA, 0xFF, 0xFF, 0x0F, 0x00, 0x76, 0x07, 0x45, 0x8D, 0x9A, 0xFF, 0xFF, 0x0F, 0x00,
];
/// The budgeted sieve nest's prologue: `PUSH RBX ; MOV EBX, 2 * cap`.
const SIEVE_BUDGET: [u8; 6] = [0x53, 0xBB, 0x00, 0x00, 0x20, 0x00];

/// javac: `static void z(boolean[] a, int limit) { for (int i = 0; i <= limit; i++) a[i] = false; }`
const ZERO_FILL: [u8; 18] = [
    0x03, 0x3d, // 0: i = 0
    0x1c, 0x1b, // 2: iload i; iload limit        ; header
    0xa3, 0x00, 0x0d, // 4: if_icmpgt 17
    0x2a, 0x1c, 0x03, 0x54, // 7: a[i] = 0
    0x84, 0x02, 0x01, // 11: i++
    0xa7, 0xff, 0xf4, // 14: goto 2
    0xb1, // 17: return
];

/// javac: `static void st(boolean[] a, int start, int step, int limit)
/// { for (int i = start; i <= limit; i += step) a[i] = true; }`
const STRIDE: [u8; 24] = [
    0x1b, 0x36, 0x04, // 0: i = start
    0x15, 0x04, 0x1d, // 3: iload i; iload limit     ; header
    0xa3, 0x00, 0x11, // 6: if_icmpgt 23
    0x2a, 0x15, 0x04, 0x04, 0x54, // 9: a[i] = 1
    0x15, 0x04, 0x1c, 0x60, 0x36, 0x04, // 14: i = i + step
    0xa7, 0xff, 0xef, // 20: goto 3
    0xb1, // 23: return
];

/// javac:
/// ```java
/// static int sieve(boolean[] c, int limit) {
///     int count = 0;
///     for (int i = 2; i <= limit; i++) {
///         if (!c[i]) {
///             count++;
///             for (int j = i + i; j <= limit; j += i) c[j] = true;
///         }
///     }
///     return count;
/// }
/// ```
const SIEVE: [u8; 51] = [
    0x03, 0x3d, // 0: count = 0
    0x05, 0x3e, // 2: i = 2
    0x1d, 0x1b, // 4: iload i; iload limit      ; outer header
    0xa3, 0x00, 0x2b, // 6: if_icmpgt 49
    0x2a, 0x1d, 0x33, // 9: c[i]
    0x9a, 0x00, 0x1f, // 12: ifne 43
    0x84, 0x02, 0x01, // 15: count++
    0x1d, 0x1d, 0x60, 0x36, 0x04, // 18: j = i + i
    0x15, 0x04, 0x1b, // 23: iload j; iload limit   ; inner header
    0xa3, 0x00, 0x11, // 26: if_icmpgt 43
    0x2a, 0x15, 0x04, 0x04, 0x54, // 29: c[j] = true
    0x15, 0x04, 0x1d, 0x60, 0x36, 0x04, // 34: j += i
    0xa7, 0xff, 0xef, // 40: goto 23
    0x84, 0x03, 0x01, // 43: i++
    0xa7, 0xff, 0xd6, // 46: goto 4
    0x1c, 0xac, // 49: return count
];

fn call(m: &CompiledMethod, args: &[i64]) -> i64 {
    // SAFETY: every fixture is a static method over one synthetic `boolean[]`
    // that outlives the call and int arguments that keep every index in
    // bounds, so no throw path or runtime helper is reached.
    unsafe { m.try_call(args).expect("fixture call") }
}

#[test]
fn a_zero_fill_past_the_cap_is_strip_mined_and_exact() {
    let m = compile_fixture(&ZERO_FILL, 2, 3);
    assert!(
        contains(m.code_bytes(), &INCLUSIVE_CLAMP),
        "the zero fill's pre-header must clamp an over-cap span, not skip it"
    );
    for (len, limit) in [
        (8usize, 7usize),
        (1000, 999),
        (CAP - 1, CAP - 2),
        (CAP, CAP - 1),
        (CAP + 1, CAP),
        (CAP + 4097, CAP + 4096),
        (3 * CAP + 5, 3 * CAP + 4),
        (3 * CAP + 5, 2 * CAP + 17),
    ] {
        let mut a = Bools::new(len, 1);
        call(&m, &[a.handle(), limit as i64]);
        let got = a.elements();
        for (i, &b) in got.iter().enumerate() {
            let want = if i <= limit { 0 } else { 1 };
            assert_eq!(b, want, "len {len}, limit {limit}: a[{i}]");
        }
        assert!(a.guard_is_untouched(), "len {len}: wrote past the array");
    }
}

#[test]
fn a_strided_store_past_the_cap_is_strip_mined_and_exact() {
    let m = compile_fixture(&STRIDE, 4, 5);
    assert!(
        contains(m.code_bytes(), &INCLUSIVE_CLAMP),
        "the strided store's pre-header must clamp an over-cap span, not skip it"
    );
    for (len, start, step, limit) in [
        (100usize, 1usize, 7usize, 99usize),
        (CAP + 10, 0, 2, CAP + 9),
        (CAP + 10, 3, 1, CAP + 9),
        (3 * CAP + 1, 0, 1, 3 * CAP),
        (3 * CAP + 1, 5, 3, 3 * CAP),
        (3 * CAP + 1, 11, 4099, 3 * CAP - 7),
    ] {
        let mut a = Bools::new(len, 0);
        call(&m, &[a.handle(), start as i64, step as i64, limit as i64]);
        let got = a.elements();
        let mut want = vec![0u8; len];
        let mut i = start;
        while i <= limit {
            want[i] = 1;
            i += step;
        }
        assert!(
            got == want,
            "len {len}, start {start}, step {step}, limit {limit}: first difference at {:?}",
            got.iter().zip(&want).position(|(g, w)| g != w)
        );
        assert!(a.guard_is_untouched(), "len {len}: wrote past the array");
    }
}

fn sieve_oracle(limit: usize) -> (i64, Vec<u8>) {
    let mut c = vec![0u8; limit + 1];
    let mut count = 0i64;
    for i in 2..=limit {
        if c[i] == 0 {
            count += 1;
            let mut j = i + i;
            while j <= limit {
                c[j] = 1;
                j += i;
            }
        }
    }
    (count, c)
}

#[test]
fn a_sieve_past_the_cap_runs_the_budgeted_nest_and_is_exact() {
    let m = compile_fixture(&SIEVE, 2, 5);
    assert!(
        contains(m.code_bytes(), &SIEVE_BUDGET),
        "the sieve's pre-header must carry the budgeted nest for over-cap spans"
    );
    // 100_000: the unbudgeted nest (CratonBench's size). 3_000_000: the
    // budgeted nest marks from 2 and 3, runs out of budget and hands over.
    // 5_000_000: prime 2 alone would exceed the budget, so the budgeted nest
    // stops before it and the scalar loop does all of it. CAP + 1 and
    // 2 * CAP: either side of the per-prime ceiling for prime 2.
    for limit in [100_000usize, CAP + 1, 2 * CAP, 3_000_000, 5_000_000] {
        let (want_count, want) = sieve_oracle(limit);
        let mut c = Bools::new(limit + 1, 0);
        let got_count = call(&m, &[c.handle(), limit as i64]) as i32 as i64;
        assert_eq!(got_count, want_count, "limit {limit}: prime count");
        let got = c.elements();
        assert!(
            got == want,
            "limit {limit}: first flag difference at {:?}",
            got.iter().zip(&want).position(|(g, w)| g != w)
        );
        assert!(
            c.guard_is_untouched(),
            "limit {limit}: wrote past the array"
        );
    }
}
