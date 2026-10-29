// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! JIT review round 9, wave 7, lane `bulkplace7`: the bulk byte pre-headers
//! (zero fill, strided store, byte sieve) are re-entered from the loop's back
//! edge (`simd-preheaders-skip-arrays-longer-than-the-poll-free-cap-20260918.md`).
//!
//! Wave 6 made each of them strip-mined (one pass covers at most
//! `MAX_BULK_BYTE_LOOP_SPAN`, the sieve a work budget, then hands over to the
//! scalar loop), but they were still emitted BEFORE `pc_to_native[header]`, so
//! the back edge skipped them and only the first strip was bulk. Wave 7 emits
//! them from `emit_strip_mined_preheaders`, at `pc_to_native[header]`, where
//! the back edge lands: every strip is bulk.
//!
//! Each test pins two things:
//!
//! * LAYOUT: some backward `JMP rel32` / `Jcc rel32` located after the
//!   pre-header's signature bytes lands BEFORE them. That is the loop's back
//!   edge landing at (or before) the pre-header. With the pre-wave-7 layout
//!   every back edge landed past the whole pre-header, so no such jump exists
//!   (the pre-headers' own internal loops start after their signatures).
//! * RESULT: over-cap spans (several re-entries) still equal a Rust oracle,
//!   with the guard bytes past the array untouched.
//!
//! The fixture helpers leave `safepoint_flag_addr` at 0, so the back edge
//! carries no poll here; the GC-under-allocation run is a VM-level check.

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

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

/// Is there a `JMP rel32` (E9) or `Jcc rel32` (0F 80..8F) at an offset past
/// `after` whose target lies in `[0, before)`?
fn has_backward_jump_before(code: &[u8], after: usize, before: usize) -> bool {
    let rel_at = |at: usize| -> Option<i64> {
        let bytes: [u8; 4] = code.get(at..at + 4)?.try_into().ok()?;
        Some(i64::from(i32::from_le_bytes(bytes)))
    };
    (after..code.len()).any(|q| {
        let target = match code[q] {
            0xE9 => rel_at(q + 1).map(|rel| q as i64 + 5 + rel),
            0x0F if code.get(q + 1).is_some_and(|b| (0x80..=0x8F).contains(b)) => {
                rel_at(q + 2).map(|rel| q as i64 + 6 + rel)
            }
            _ => None,
        };
        target.is_some_and(|t| t >= 0 && t < before as i64)
    })
}

/// Assert the back edge re-enters the pre-header whose signature is `sig`.
fn assert_back_edge_reenters(m: &CompiledMethod, sig: &[u8], what: &str) {
    let code = m.code_bytes();
    let at = find(code, sig).unwrap_or_else(|| panic!("{what}: pre-header signature missing"));
    assert!(
        has_backward_jump_before(code, at + sig.len(), at),
        "{what}: no back edge lands before the pre-header (it is still fall-through-only)"
    );
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

/// javac: `static int sieve(boolean[] c, int limit)`, the classic nest
/// (`count`, outer `i` from 2, inner `j = i + i; j <= limit; j += i`).
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
fn the_zero_fill_pre_header_is_reentered_from_the_back_edge() {
    let m = compile_fixture(&ZERO_FILL, 2, 3);
    assert_back_edge_reenters(&m, &INCLUSIVE_CLAMP, "zero fill");
    // Five strips and a partial one: re-entered five times.
    for (len, limit) in [(5 * CAP + 9, 5 * CAP + 8), (5 * CAP + 9, 2 * CAP)] {
        let mut a = Bools::new(len, 1);
        call(&m, &[a.handle(), limit as i64]);
        let got = a.elements();
        let first_bad = got
            .iter()
            .enumerate()
            .position(|(i, &b)| b != u8::from(i > limit));
        assert_eq!(
            first_bad, None,
            "len {len}, limit {limit}: first wrong element"
        );
        assert!(a.guard_is_untouched(), "len {len}: wrote past the array");
    }
}

#[test]
fn the_strided_store_pre_header_is_reentered_from_the_back_edge() {
    let m = compile_fixture(&STRIDE, 4, 5);
    assert_back_edge_reenters(&m, &INCLUSIVE_CLAMP, "strided store");
    for (len, start, step, limit) in [
        (4 * CAP + 3, 0usize, 1usize, 4 * CAP + 2),
        (4 * CAP + 3, 7, 5, 4 * CAP - 1),
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

#[test]
fn the_sieve_pre_header_is_reentered_from_the_back_edge() {
    let m = compile_fixture(&SIEVE, 2, 5);
    assert_back_edge_reenters(&m, &SIEVE_BUDGET, "byte sieve");
    for limit in [3_000_000usize, 4 * CAP + 1] {
        let mut c = Bools::new(limit + 1, 0);
        let got_count = call(&m, &[c.handle(), limit as i64]) as i32 as i64;
        let mut want = vec![0u8; limit + 1];
        let mut want_count = 0i64;
        for i in 2..=limit {
            if want[i] == 0 {
                want_count += 1;
                let mut j = i + i;
                while j <= limit {
                    want[j] = 1;
                    j += i;
                }
            }
        }
        assert_eq!(got_count, want_count, "limit {limit}: prime count");
        let got = c.elements();
        assert!(
            got == want,
            "limit {limit}: first difference at {:?}",
            got.iter().zip(&want).position(|(g, w)| g != w)
        );
        assert!(
            c.guard_is_untouched(),
            "limit {limit}: wrote past the array"
        );
    }
}
