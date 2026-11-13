// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! JIT tier2 wave 2, page agent `atm20`
//! (`docs/feature-designs/jit-ir-optimizations.md`).
//!
//! Two things the in-crate tests of the atomic lowerings did not do:
//!
//! 1. **Execute the shared read-modify-write bytes.** Every `Unsafe` RMW
//!    prefix (array element, instance field, static field, reference field)
//!    and the constant-`VarHandle` array arm (AT-2) end in
//!    `direct_helpers::unsafe_inline_rmw_bytes`. Its existing tests compare the
//!    bytes with a literal; nothing ran them. The tests here wrap those bytes
//!    in the register contract both tiers' prefixes honour (RAX = base,
//!    R11 = offset, RDX = operand, R10 = expected) and EXECUTE every
//!    `(width, op)` row on a fake cell, single-threaded for the result and
//!    widening rules, and from four threads for atomicity. An `int` row must
//!    leave the other half of its 8-byte word alone, return a sign-extended
//!    result, and compare only 32 bits of `expected`.
//!
//! 2. **Pin the armed-ZGC decline of every reference atomic** (AT-7): a
//!    source ratchet that each reference CAS / volatile-store / constant
//!    `VarHandle` emitter, in the optimizing tier and the single-pass tier,
//!    asks `zgc_read_barrier_blocks_inline_fields()` before its first byte,
//!    and that the reference-store gate screen precedes the locked store.
//!
//! x86-64 only (the bytes are machine code). Run with
//! `cargo test -p cratonvm-jit --test atm20_atomic_lowerings`.

#![cfg(all(target_arch = "x86_64", any(unix, windows)))]

use cratonvm_jit::direct_helpers::{
    unsafe_inline_rmw_bytes, UnsafeBitOp, UnsafeInlineRmw, UnsafeRmwOp,
};
use cratonvm_jit::ExecutableBuffer;
use std::sync::atomic::{AtomicU64, Ordering};

type RmwFn = extern "C" fn(u64, u64, u64, u64) -> u64;

/// The shared RMW bytes behind the register contract of the prefixes: the
/// function takes `(base, offset, operand, expected)`.
struct Rmw {
    buf: ExecutableBuffer,
}

impl Rmw {
    fn new(width: u8, op: UnsafeRmwOp) -> Rmw {
        let mut body: Vec<u8> = Vec::new();
        if cfg!(windows) {
            body.extend_from_slice(&[0x48, 0x89, 0xC8]); // MOV RAX, RCX  (base)
            body.extend_from_slice(&[0x49, 0x89, 0xD3]); // MOV R11, RDX  (offset)
            body.extend_from_slice(&[0x4C, 0x89, 0xC2]); // MOV RDX, R8   (operand)
            body.extend_from_slice(&[0x4D, 0x89, 0xCA]); // MOV R10, R9   (expected)
        } else {
            // SysV: RDX is already the operand (argument 3).
            body.extend_from_slice(&[0x48, 0x89, 0xF8]); // MOV RAX, RDI  (base)
            body.extend_from_slice(&[0x49, 0x89, 0xF3]); // MOV R11, RSI  (offset)
            body.extend_from_slice(&[0x49, 0x89, 0xCA]); // MOV R10, RCX  (expected)
        }
        body.extend_from_slice(&unsafe_inline_rmw_bytes(UnsafeInlineRmw { width, op }));
        body.push(0xC3); // RET
        let mut buf = ExecutableBuffer::new(4096).expect("executable buffer");
        buf.emit(&body);
        assert!(!buf.overflowed());
        buf.finalize();
        Rmw { buf }
    }

    fn call(&self, base: usize, offset: u64, operand: u64, expected: u64) -> u64 {
        // SAFETY: a complete `extern "C" fn(u64, u64, u64, u64) -> u64` whose
        // only memory access is the locked instruction at `base + offset`,
        // which every caller points into a live `[AtomicU64; 4]`; it writes
        // RAX, RCX, RDX, R10 and R11, all caller-saved on both ABIs.
        let f = unsafe { std::mem::transmute::<*const u8, RmwFn>(self.buf.as_ptr()) };
        f(base as u64, offset, operand, expected)
    }
}

const SENT0: u64 = 0x0123_4567_89AB_CDEF;
const SENT2: u64 = 0xFEDC_BA98_7654_3210;

/// `[SENT0, mid, SENT2, 0]`: the cell under test is index 1, at offsets 8
/// (low half) and 12 (high half); its neighbours must never change.
fn fixture(mid: u64) -> [AtomicU64; 4] {
    [
        AtomicU64::new(SENT0),
        AtomicU64::new(mid),
        AtomicU64::new(SENT2),
        AtomicU64::new(0),
    ]
}

fn base(c: &[AtomicU64; 4]) -> usize {
    c[0].as_ptr() as usize // Cast: the cells' address, handed to machine code
}

fn mid(c: &[AtomicU64; 4]) -> u64 {
    c[1].load(Ordering::SeqCst)
}

fn neighbours_intact(c: &[AtomicU64; 4]) {
    assert_eq!(c[0].load(Ordering::SeqCst), SENT0, "the cell before");
    assert_eq!(c[2].load(Ordering::SeqCst), SENT2, "the cell after");
    assert_eq!(c[3].load(Ordering::SeqCst), 0, "the cell after that");
}

/// An `int` result as the prefix leaves it in RAX: sign-extended.
fn sx(v: u32) -> u64 {
    i64::from(v as i32) as u64 // Cast: reinterpret then sign-extend, as MOVSXD does
}

const LO: u64 = 8;
const HI: u64 = 12;

#[test]
fn atm20_cas_int_swaps_one_word_compares_32_bits_and_answers_zero_or_one() {
    let cas = Rmw::new(4, UnsafeRmwOp::Cas);
    let c = fixture(0xDEAD_BEEF_0000_0005);
    // `expected` with garbage above bit 31: CMPXCHG EAX compares 32 bits.
    assert_eq!(cas.call(base(&c), LO, 9, 0xFFFF_FFFF_0000_0005), 1);
    assert_eq!(mid(&c), 0xDEAD_BEEF_0000_0009);
    assert_eq!(cas.call(base(&c), LO, 11, 5), 0, "a stale expected fails");
    assert_eq!(
        mid(&c),
        0xDEAD_BEEF_0000_0009,
        "a failed CAS stores nothing"
    );
    // The high word is a separate int cell.
    assert_eq!(cas.call(base(&c), HI, 0x1234_5678, 0xDEAD_BEEF), 1);
    assert_eq!(mid(&c), 0x1234_5678_0000_0009);
    neighbours_intact(&c);
}

#[test]
fn atm20_cas_long_swaps_the_whole_word() {
    let cas = Rmw::new(8, UnsafeRmwOp::Cas);
    let c = fixture(u64::MAX);
    assert_eq!(cas.call(base(&c), LO, 7, u64::MAX), 1);
    assert_eq!(mid(&c), 7);
    // 32 matching bits are not enough for a long.
    assert_eq!(cas.call(base(&c), LO, 9, 0xFFFF_FFFF_0000_0007), 0);
    assert_eq!(mid(&c), 7);
    assert_eq!(cas.call(base(&c), LO, 9, 7), 1);
    assert_eq!(mid(&c), 9);
    neighbours_intact(&c);
}

#[test]
fn atm20_compare_and_exchange_keeps_the_witness_and_sign_extends_an_int() {
    let cae4 = Rmw::new(4, UnsafeRmwOp::Cae);
    let c = fixture(0xAAAA_BBBB_8000_0000);
    // Success: the witness is `expected` (here a negative int).
    assert_eq!(cae4.call(base(&c), LO, 7, 0x8000_0000), sx(0x8000_0000));
    assert_eq!(mid(&c), 0xAAAA_BBBB_0000_0007);
    // Failure: the witness is the cell's value, nothing is stored.
    assert_eq!(cae4.call(base(&c), LO, 3, 1), 7);
    assert_eq!(mid(&c), 0xAAAA_BBBB_0000_0007);
    // A negative witness on failure is sign-extended too.
    c[1].store(0xAAAA_BBBB_8000_0000, Ordering::SeqCst);
    assert_eq!(cae4.call(base(&c), LO, 3, 0), sx(0x8000_0000));
    assert_eq!(mid(&c), 0xAAAA_BBBB_8000_0000);
    neighbours_intact(&c);

    let cae8 = Rmw::new(8, UnsafeRmwOp::Cae);
    let c = fixture(0x8000_0000_0000_0001);
    assert_eq!(
        cae8.call(base(&c), LO, 5, 0x8000_0000_0000_0001),
        0x8000_0000_0000_0001
    );
    assert_eq!(mid(&c), 5);
    assert_eq!(cae8.call(base(&c), LO, 6, 4), 5, "the witness of a failure");
    assert_eq!(mid(&c), 5);
    neighbours_intact(&c);
}

#[test]
fn atm20_get_and_add_wraps_in_its_width_and_answers_the_old_value() {
    let add4 = Rmw::new(4, UnsafeRmwOp::GetAndAdd);
    let c = fixture(0xDEAD_BEEF_7FFF_FFFF);
    assert_eq!(add4.call(base(&c), LO, 1, 0), 0x7FFF_FFFF);
    // The int wrapped to MIN_VALUE and the carry did NOT reach the high word.
    assert_eq!(mid(&c), 0xDEAD_BEEF_8000_0000);
    // A Java `-1` arrives sign-extended; the old value (negative) comes back
    // sign-extended.
    assert_eq!(add4.call(base(&c), LO, u64::MAX, 0), sx(0x8000_0000));
    assert_eq!(mid(&c), 0xDEAD_BEEF_7FFF_FFFF);
    // The high word is its own int.
    assert_eq!(add4.call(base(&c), HI, 0x2111_1111, 0), sx(0xDEAD_BEEF));
    // 0xDEAD_BEEF + 0x2111_1111 = 0xFFBE_D000 in the high word.
    assert_eq!(mid(&c), 0xFFBE_D000_7FFF_FFFF);
    neighbours_intact(&c);

    let add8 = Rmw::new(8, UnsafeRmwOp::GetAndAdd);
    let c = fixture(u64::MAX);
    assert_eq!(add8.call(base(&c), LO, 1, 0), u64::MAX);
    assert_eq!(mid(&c), 0, "a long wraps");
    assert_eq!(add8.call(base(&c), LO, u64::MAX, 0), 0);
    assert_eq!(mid(&c), u64::MAX);
    neighbours_intact(&c);
}

#[test]
fn atm20_get_and_set_exchanges_and_answers_the_old_value() {
    let set4 = Rmw::new(4, UnsafeRmwOp::GetAndSet);
    let c = fixture(0xDEAD_BEEF_8000_0001);
    assert_eq!(set4.call(base(&c), LO, 0x7FFF_FFFF, 0), sx(0x8000_0001));
    assert_eq!(mid(&c), 0xDEAD_BEEF_7FFF_FFFF, "the high word is untouched");
    assert_eq!(set4.call(base(&c), HI, sx(0x8000_0000), 0), sx(0xDEAD_BEEF));
    assert_eq!(mid(&c), 0x8000_0000_7FFF_FFFF);
    neighbours_intact(&c);

    let set8 = Rmw::new(8, UnsafeRmwOp::GetAndSet);
    let c = fixture(0x0123_4567_89AB_CDEF);
    assert_eq!(set8.call(base(&c), LO, u64::MAX, 0), 0x0123_4567_89AB_CDEF);
    assert_eq!(mid(&c), u64::MAX);
    neighbours_intact(&c);
}

fn apply(op: UnsafeBitOp, a: u64, b: u64) -> u64 {
    match op {
        UnsafeBitOp::Or => a | b,
        UnsafeBitOp::And => a & b,
        UnsafeBitOp::Xor => a ^ b,
    }
}

#[test]
fn atm20_get_and_bitwise_loops_to_the_old_value_in_both_widths() {
    for op in [UnsafeBitOp::Or, UnsafeBitOp::And, UnsafeBitOp::Xor] {
        // int: a negative old value, a mask with its sign bit clear.
        let rmw4 = Rmw::new(4, UnsafeRmwOp::GetAndBitwise(op));
        let old_low: u32 = 0xF0F0_F0F0;
        let mask: u32 = 0x3C3C_3C3C;
        let c = fixture(0x1357_9BDF_0000_0000 | u64::from(old_low));
        assert_eq!(
            rmw4.call(base(&c), LO, sx(mask), 0),
            sx(old_low),
            "{op:?} int old"
        );
        let new_low = apply(op, u64::from(old_low), u64::from(mask)) & 0xFFFF_FFFF;
        assert_eq!(
            mid(&c),
            0x1357_9BDF_0000_0000 | new_low,
            "{op:?} int new, high word kept"
        );
        neighbours_intact(&c);

        // long.
        let rmw8 = Rmw::new(8, UnsafeRmwOp::GetAndBitwise(op));
        let old: u64 = 0xF0F0_F0F0_A5A5_A5A5;
        let mask: u64 = 0x0FF0_0FF0_3C3C_3C3C;
        let c = fixture(old);
        assert_eq!(rmw8.call(base(&c), LO, mask, 0), old, "{op:?} long old");
        assert_eq!(mid(&c), apply(op, old, mask), "{op:?} long new");
        neighbours_intact(&c);
    }
}

/// Run `f(thread index)` on `threads` threads and join them.
fn hammer(threads: usize, f: impl Fn(usize) + Sync) {
    std::thread::scope(|s| {
        for t in 0..threads {
            let f = &f;
            s.spawn(move || f(t));
        }
    });
}

const THREADS: usize = 4;
const ROUNDS: u64 = 20_000;

#[test]
fn atm20_contended_get_and_add_loses_no_update_and_keeps_the_halves_apart() {
    // Two threads per half of one 8-byte word, int-wide: a locked 32-bit XADD
    // must not read-modify-write its neighbour.
    let add4 = Rmw::new(4, UnsafeRmwOp::GetAndAdd);
    let c = fixture(0);
    let b = base(&c);
    hammer(THREADS, |t| {
        let at = if t % 2 == 0 { LO } else { HI };
        for _ in 0..ROUNDS {
            add4.call(b, at, 1, 0);
        }
    });
    let each = (THREADS as u64 / 2) * ROUNDS;
    assert_eq!(mid(&c), (each << 32) | each);
    neighbours_intact(&c);

    let add8 = Rmw::new(8, UnsafeRmwOp::GetAndAdd);
    let c = fixture(0);
    let b = base(&c);
    hammer(THREADS, |_| {
        for _ in 0..ROUNDS {
            add8.call(b, LO, 1, 0);
        }
    });
    assert_eq!(mid(&c), THREADS as u64 * ROUNDS);
    neighbours_intact(&c);
}

#[test]
fn atm20_contended_cas_loop_counts_exactly() {
    for (width, op_bytes) in [(4u8, 4u64), (8u8, 8u64)] {
        let cas = Rmw::new(width, UnsafeRmwOp::Cas);
        let c = fixture(0);
        let b = base(&c);
        hammer(THREADS, |_| {
            for _ in 0..ROUNDS {
                loop {
                    // SAFETY: `b + LO` is the live cell `c[1]`, read with the
                    // width the CAS uses.
                    let cur = unsafe {
                        if op_bytes == 4 {
                            u64::from(std::ptr::read_volatile((b + LO as usize) as *const u32))
                        } else {
                            std::ptr::read_volatile((b + LO as usize) as *const u64)
                        }
                    };
                    if cas.call(b, LO, cur.wrapping_add(1), cur) == 1 {
                        break;
                    }
                }
            }
        });
        let want = THREADS as u64 * ROUNDS;
        // For the int row the high word is untouched (0), so the cell is the count.
        assert_eq!(mid(&c), want, "width {width}");
        neighbours_intact(&c);
    }
}

#[test]
fn atm20_contended_bitwise_xor_toggles_cancel_exactly() {
    // Each thread toggles its own bit an even number of times while the
    // others toggle theirs: a bitwise loop that loses a competing update
    // leaves a stray bit. Bit 31 is set up front and must survive.
    for width in [4u8, 8u8] {
        let xor = Rmw::new(width, UnsafeRmwOp::GetAndBitwise(UnsafeBitOp::Xor));
        let c = fixture(0x8000_0000);
        let b = base(&c);
        hammer(THREADS, |t| {
            let mask = 1u64 << t;
            for _ in 0..ROUNDS {
                xor.call(b, LO, mask, 0);
                xor.call(b, LO, mask, 0);
            }
        });
        assert_eq!(mid(&c), 0x8000_0000, "width {width}");
        neighbours_intact(&c);
    }
}

// ---------------------------------------------------------------------------
// AT-7: the armed-ZGC decline of every reference atomic, pinned by text.
// ---------------------------------------------------------------------------

const IR_LOWER: &str = include_str!("../src/ir_lower.rs");
const OP_INVOKE: &str = include_str!("../src/x64/op_invoke.rs");

const ZGC_GUARD: &str = "zgc_read_barrier_blocks_inline_fields()";

/// The text of the method `fn name(` of an `impl` (4-space indent), up to the
/// next item at that indent.
fn method_body<'a>(src: &'a str, name: &str) -> &'a str {
    let needle = format!("    fn {name}(");
    let start = src
        .find(&needle)
        .unwrap_or_else(|| panic!("`{name}` is gone: update this ratchet with its new name"));
    let rest = &src[start + needle.len()..];
    let end = [
        "\n    ///",
        "\n    fn ",
        "\n    pub fn ",
        "\n    pub(crate) fn ",
        "\n    #[",
    ]
    .iter()
    .filter_map(|marker| rest.find(marker))
    .min()
    .unwrap_or(rest.len());
    &rest[..end]
}

/// Every emitter of a reference CAS / reference volatile access / constant
/// reference `VarHandle` access, by tier. `reference_atomic_emitters_decline_
/// while_the_zgc_load_barrier_is_armed` is the invariant AT-7 stated: while
/// `ZgcRealHeap::set_barrier_color` has armed the colour barrier (the opt-in
/// `CRATONVM_ZGC_LOAD_BARRIER`), a reference slot may hold a coloured word
/// that an uncoloured `expected` would fail against, so no inline
/// `LOCK CMPXCHG` / `XCHG` on a reference slot may be compiled; the call
/// reaches `VmHeap::cas_ref_slot_barriered` (heal-then-CAS) instead.
const REFERENCE_ATOMIC_EMITTERS: &[(&str, &str, &str)] = &[
    ("ir_lower.rs", "try_emit_ir_unsafe_ref_cas_prefix", "ir"),
    (
        "ir_lower.rs",
        "try_emit_ir_unsafe_ref_volatile_prefix",
        "ir",
    ),
    ("ir_lower.rs", "try_emit_ir_vh_const_prefix", "ir"),
    ("op_invoke.rs", "emit_sp_unsafe_ref_cas_core", "sp"),
    ("op_invoke.rs", "emit_sp_unsafe_ref_volatile_core", "sp"),
    ("op_invoke.rs", "emit_sp_vh_const_prefix", "sp"),
];

fn source_of(file: &str) -> &'static str {
    match file {
        "ir_lower.rs" => IR_LOWER,
        "op_invoke.rs" => OP_INVOKE,
        other => panic!("unknown source {other}"),
    }
}

#[test]
fn atm20_reference_atomic_emitters_decline_while_the_zgc_load_barrier_is_armed() {
    for (file, name, tier) in REFERENCE_ATOMIC_EMITTERS {
        let body = method_body(source_of(file), name);
        assert!(
            body.contains(ZGC_GUARD),
            "{tier}-tier `{name}` ({file}) no longer asks {ZGC_GUARD}: an inline reference \
             atomic would race a coloured slot (AT-7)"
        );
    }
}

#[test]
fn atm20_the_constant_varhandle_array_arms_sit_behind_the_zgc_guard() {
    // The array-element arms (primitive and reference, AT-2) are reached
    // only through the constant-`VarHandle` prefixes above, from a point
    // AFTER the guard.
    for (file, name, delegate) in [
        (
            "ir_lower.rs",
            "try_emit_ir_vh_const_prefix",
            "self.try_emit_ir_vh_const_array_prefix(",
        ),
        (
            "op_invoke.rs",
            "emit_sp_vh_const_prefix",
            "self.emit_sp_vh_const_array_prefix(",
        ),
    ] {
        let body = method_body(source_of(file), name);
        let guard = body.find(ZGC_GUARD).unwrap_or(usize::MAX);
        let call = body
            .find(delegate)
            .unwrap_or_else(|| panic!("`{name}` no longer delegates to the array arm"));
        assert!(
            guard < call,
            "`{name}`: the ZGC guard must precede the array arm"
        );
    }
}

#[test]
fn atm20_reference_cas_runs_the_store_gate_screen_before_its_locked_store() {
    // Both tiers: every gate screen is emitted before the CAS bytes, and
    // there is at least one (the SATB pre-gate and the card gate are decided
    // BEFORE the store; a CAS has no later point to log the overwritten
    // value).
    for (file, name, gate, locked) in [
        (
            "ir_lower.rs",
            "try_emit_ir_unsafe_ref_cas_prefix",
            "vh_ref_store_gate_screen_bytes(gates)",
            "unsafe_inline_rmw_bytes(",
        ),
        (
            "op_invoke.rs",
            "emit_sp_unsafe_ref_cas_core",
            "vh_ref_store_gate_screen_bytes(gates)",
            "unsafe_inline_rmw_bytes(",
        ),
        (
            "ir_lower.rs",
            "try_emit_ir_unsafe_ref_volatile_prefix",
            "vh_ref_store_gate_screen_bytes(g)",
            "unsafe_ref_volatile_access_bytes(access)",
        ),
    ] {
        let body = method_body(source_of(file), name);
        let last_gate = body
            .rfind(gate)
            .unwrap_or_else(|| panic!("`{name}` has no `{gate}`"));
        let store = body
            .find(locked)
            .unwrap_or_else(|| panic!("`{name}` has no `{locked}`"));
        assert!(
            last_gate < store,
            "`{name}`: a gate screen is emitted after the store bytes"
        );
    }
}
