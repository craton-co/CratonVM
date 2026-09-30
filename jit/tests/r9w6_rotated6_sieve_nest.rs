// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! JIT review round 9, wave 6, lane `rotated6`: the ecj-compiled byte-sieve
//! NEST reaches the sieve pre-header
//! (`rotated-loops-lose-every-preheader-transform-20260918.md`, its last open
//! case).
//!
//! ecj rotates both the outer `i` loop and the inner `j` marking loop. Wave 4
//! unrotated the outer loop for the batch detectors, but the nest detector
//! wants javac's top-tested INNER loop as well; wave 6's
//! `escape_analysis::unrotate_nested_loops` rewrites the inner loop of the
//! copy in place. The test compiles the exact ecj 3.45 bytecode with the
//! rotated pre-header relocation armed and unarmed and pins:
//! * the count and every array byte equal a scalar oracle, both arms, for
//!   limits around the pre-header's word scan and its `i >= 2` guard;
//! * the sieve pre-header (its `MOV RSI, 0x8080..80` word-scan constant) is
//!   emitted only when armed, so a silent scalar fallback cannot pass.

#![cfg(target_arch = "x86_64")]

use cratonvm_jit::x64::compile;
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

fn compile_rotated(
    code: &[u8],
    num_params: usize,
    max_locals: usize,
    armed: bool,
) -> CompiledMethod {
    let mut padded = code.to_vec();
    padded.extend_from_slice(&[0, 0]);
    cratonvm_types::flags::with_thread_overrides(
        &[(
            "CRATONVM_JIT_ROTATED_PREHEADER",
            Some(if armed { "1" } else { "0" }),
        )],
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
    .expect("rotated sieve compiles")
}

/// A synthetic `boolean[]`: length at `ARRAY_LENGTH_OFFSET`, `len` bytes from
/// `ARRAY_DATA_OFFSET`, then `GUARD` bytes that must keep their fill.
struct ByteArray {
    words: Vec<u64>,
    len: usize,
}

impl ByteArray {
    const GUARD: usize = 64;
    const GUARD_FILL: u8 = 0x5A;

    fn new(len: usize) -> Self {
        let bytes = ARRAY_DATA_OFFSET + len + Self::GUARD;
        let mut me = ByteArray {
            words: vec![0u64; bytes.div_ceil(8)],
            len,
        };
        let base = me.base();
        // SAFETY: the allocation holds the header, the elements and the
        // guard; every write below is inside it.
        unsafe {
            std::ptr::write_unaligned(base.add(ARRAY_LENGTH_OFFSET) as *mut i32, len as i32);
            for g in 0..Self::GUARD {
                *base.add(ARRAY_DATA_OFFSET + len + g) = Self::GUARD_FILL;
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

    fn get(&mut self, i: usize) -> u8 {
        assert!(i < self.len);
        // SAFETY: `i` is an element index of this array.
        unsafe { *self.base().add(ARRAY_DATA_OFFSET + i) }
    }

    fn set(&mut self, i: usize, v: u8) {
        assert!(i < self.len);
        // SAFETY: as above.
        unsafe { *self.base().add(ARRAY_DATA_OFFSET + i) = v }
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

/// ecj 3.45 for
/// ```java
/// static int sieve(boolean[] a, int limit) {
///     for (int i = 0; i <= limit; i++) a[i] = false;
///     int count = 0;
///     for (int i = 2; i <= limit; i++) {
///         if (!a[i]) { count++; for (int j = i + i; j <= limit; j += i) a[j] = true; }
///     }
///     return count;
/// }
/// ```
const ECJ_SIEVE: [u8; 68] = [
    0x03, 0x3d, 0xa7, 0x00, 0x0a, // 0: i = 0; goto 12
    0x2a, 0x1c, 0x03, 0x54, 0x84, 0x02, 0x01, // 5: a[i] = false; i++
    0x1c, 0x1b, 0xa4, 0xff, 0xf7, // 12: if (i <= limit) goto 5
    0x03, 0x3d, 0x05, 0x3e, // 17: count = 0; i = 2
    0xa7, 0x00, 0x28, // 21: goto 61
    0x2a, 0x1d, 0x33, 0x9a, 0x00, 0x1f, // 24: if (a[i]) goto 58
    0x84, 0x02, 0x01, // 30: count++
    0x1d, 0x1d, 0x60, 0x36, 0x04, // 33: j = i + i
    0xa7, 0x00, 0x0e, // 38: goto 52
    0x2a, 0x15, 0x04, 0x04, 0x54, // 41: a[j] = true
    0x15, 0x04, 0x1d, 0x60, 0x36, 0x04, // 46: j += i
    0x15, 0x04, 0x1b, 0xa4, 0xff, 0xf2, // 52: if (j <= limit) goto 41
    0x84, 0x03, 0x01, // 58: i++
    0x1d, 0x1b, 0xa4, 0xff, 0xd9, // 61: if (i <= limit) goto 24
    0x1c, 0xac, // 66: return count
];

/// `MOV RSI, 0x8080_8080_8080_8080` — the sieve pre-header's word-scan
/// constant (`emit_byte_sieve_preheader`).
fn sieve_word_scan() -> Vec<u8> {
    let mut sig = vec![0x48, 0xBE];
    sig.extend_from_slice(&[0x80; 8]);
    sig
}

/// The Java semantics, on a plain byte vector.
fn oracle(a: &mut [u8], limit: usize) -> i32 {
    for x in a.iter_mut().take(limit + 1) {
        *x = 0;
    }
    let mut count = 0;
    for i in 2..=limit {
        if a[i] == 0 {
            count += 1;
            let mut j = i + i;
            while j <= limit {
                a[j] = 1;
                j += i;
            }
        }
    }
    count
}

#[test]
fn the_rotated_ecj_sieve_nest_reaches_the_sieve_preheader_and_is_exact() {
    let off = compile_rotated(&ECJ_SIEVE, 2, 5, false);
    let on = compile_rotated(&ECJ_SIEVE, 2, 5, true);
    assert!(
        contains(on.code_bytes(), &sieve_word_scan()),
        "armed: the rotated ecj sieve nest must reach the sieve pre-header at its entry goto"
    );
    assert!(
        !contains(off.code_bytes(), &sieve_word_scan()),
        "unarmed: a rotated header is bypassable and keeps no batch pre-header"
    );
    for (len, limit) in [
        (1usize, 0usize),
        (2, 1),
        (3, 2),
        (8, 7),
        (9, 8),
        (17, 16),
        (100, 99),
        (100, 50),
        (1000, 999),
        (4099, 4097),
    ] {
        for (m, which) in [(&off, "unarmed"), (&on, "armed")] {
            let mut a = ByteArray::new(len);
            let mut want = vec![1u8; len];
            for i in 0..len {
                a.set(i, 1);
            }
            let want_count = oracle(&mut want, limit);
            // SAFETY: a static `([ZI)I` body; `limit < len`, so the method
            // never reaches a throw path.
            let got = unsafe { m.try_call(&[a.handle(), limit as i64]).expect("call") } as i32;
            assert_eq!(got, want_count, "{which}: len {len}, limit {limit}: count");
            for (i, w) in want.iter().enumerate() {
                assert_eq!(a.get(i), *w, "{which}: len {len}, limit {limit}, a[{i}]");
            }
            assert!(
                a.guard_is_untouched(),
                "{which}: len {len}: wrote past the array"
            );
        }
    }
}
