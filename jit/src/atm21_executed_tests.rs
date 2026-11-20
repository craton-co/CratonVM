// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! JIT round 21 proposals ATOMICS21 (page
//! `docs/feature-designs/jit-ir-optimizations.md`, wave 3,
//! tag `atm21`): the byte sequences every compiled `Unsafe` atomic shares,
//! EXECUTED.
//!
//! The older tests of these sequences compare their bytes with a hand-written
//! listing (`jr21_atomics_static_cell_tests`, `direct_helpers`' RMW byte
//! pins); a wrong operand size, a wrong register or a wrong displacement
//! encodes just as well in the listing as in the generator. Here the bytes run:
//!
//! * the static arm's screen (`unsafe_inline_static_cell_bytes`) against a fake
//!   per-VM table, a fake `Class` mirror word, a fake statics block and its
//!   base-pointer cell, followed by the shared locked instruction, so a hit is
//!   proven to land on the static's own payload word and nowhere else, and each
//!   decline (width key, mirror, empty or colliding entry, null block, wrong
//!   tag, band) to leave the cell untouched;
//! * every `Unsafe` RMW op at both widths (`unsafe_inline_rmw_bytes`) against a
//!   reference model, with garbage in the unused upper operand bits;
//! * the sub-word `LOCK CMPXCHG` (`unsafe_inline_subword_rmw_bytes`) with
//!   neighbouring bytes that must survive;
//! * the sub-word cell half's separation from the int/long half;
//! * atomicity of the locked sequences across threads.
//!
//! x86-64 only (the declaration in `direct_helpers.rs` is gated). Run with
//! `cargo test -p cratonvm-jit atm21`.

use super::*;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};

/// Status words the stubs write at `[R9 + 16]`.
const HIT: u64 = 0;
const DECLINED: u64 = 1;
const NOT_STATIC: u64 = 2;

/// The scratch block the generated code works on: `[operand, expected,
/// status]`. Its address travels in R9 (written by none of the sequences
/// under test).
#[repr(C)]
#[derive(Default, Clone, Copy)]
struct Args {
    operand: u64,
    expected: u64,
    status: u64,
}

/// 64 bytes of memory the generated code reads and writes.
#[repr(C, align(16))]
struct Mem(std::cell::UnsafeCell<[u8; 64]>);

impl Mem {
    fn new() -> Box<Self> {
        Box::new(Mem(std::cell::UnsafeCell::new([0; 64])))
    }

    fn addr(&self) -> u64 {
        self.0.get() as usize as u64 // Cast: the block's address
    }

    fn put(&self, at: usize, bytes: &[u8]) {
        assert!(at + bytes.len() <= 64, "inside the block");
        // SAFETY: `at .. at + len` lies inside the 64-byte block (asserted),
        // and no generated code runs while the fixture is written.
        unsafe {
            std::ptr::copy_nonoverlapping(
                bytes.as_ptr(),
                (self.0.get() as *mut u8).add(at),
                bytes.len(),
            )
        };
    }

    /// `width` (at most 8) little-endian bytes at `at`, zero-extended.
    fn word(&self, at: usize, width: usize) -> u64 {
        assert!(width <= 8 && at + width <= 64, "inside the block");
        let mut b = [0u8; 8];
        // SAFETY: in bounds (asserted); no generated code runs concurrently.
        unsafe {
            std::ptr::copy_nonoverlapping((self.0.get() as *const u8).add(at), b.as_mut_ptr(), width)
        };
        u64::from_le_bytes(b)
    }

    fn snapshot(&self) -> [u8; 64] {
        let mut out = [0u8; 64];
        for (k, byte) in out.iter_mut().enumerate() {
            *byte = self.word(k, 1) as u8; // Cast: one byte
        }
        out
    }
}

/// The generated function `extern "C" fn(base, offset, &mut Args) -> RAX`:
/// RAX = `base`, R11 = `offset`, R9 = the args; then the optional screen
/// (decline edges go to a stub that stores `DECLINED` in the status, the
/// not-static edge to one that stores `NOT_STATIC`); then, as the compiled
/// prefixes do AFTER their screens, RDX = `operand` and R10 = `expected`;
/// then `tail` (the locked sequence) and `RET` with the sequence's RAX.
fn build(
    screen: Option<(Vec<u8>, Vec<usize>, Option<usize>)>,
    tail: &[u8],
) -> crate::ExecutableBuffer {
    let mut body: Vec<u8> = Vec::new();
    if cfg!(windows) {
        body.extend_from_slice(&[0x48, 0x89, 0xC8]); // MOV RAX, RCX
        body.extend_from_slice(&[0x49, 0x89, 0xD3]); // MOV R11, RDX
        body.extend_from_slice(&[0x4D, 0x89, 0xC1]); // MOV R9, R8
    } else {
        body.extend_from_slice(&[0x48, 0x89, 0xF8]); // MOV RAX, RDI
        body.extend_from_slice(&[0x49, 0x89, 0xF3]); // MOV R11, RSI
        body.extend_from_slice(&[0x49, 0x89, 0xD1]); // MOV R9, RDX
    }
    let screen_at = body.len();
    let mut decline_edges: Vec<usize> = Vec::new();
    let mut not_static_edge: Option<usize> = None;
    if let Some((bytes, declines, not_static)) = &screen {
        body.extend_from_slice(bytes);
        decline_edges = declines.iter().map(|e| screen_at + e).collect();
        not_static_edge = not_static.map(|e| screen_at + e);
    }
    body.extend_from_slice(&[0x49, 0x8B, 0x11]); // MOV RDX, [R9]
    body.extend_from_slice(&[0x4D, 0x8B, 0x51, 0x08]); // MOV R10, [R9 + 8]
    body.extend_from_slice(tail);
    body.push(0xC3); // RET
    fn stub(body: &mut Vec<u8>, status: u32) -> usize {
        let at = body.len();
        body.extend_from_slice(&[0x49, 0xC7, 0x41, 0x10]); // MOV QWORD [R9 + 16], imm32
        body.extend_from_slice(&status.to_le_bytes());
        body.extend_from_slice(&[0x31, 0xC0, 0xC3]); // XOR EAX, EAX ; RET
        at
    }
    fn patch(body: &mut [u8], operand: usize, target: usize) {
        let rel = i32::try_from(target).expect("small") - i32::try_from(operand + 4).expect("small");
        body[operand..operand + 4].copy_from_slice(&rel.to_le_bytes());
    }
    let declined_at = stub(&mut body, DECLINED as u32); // Cast: a small status
    let not_static_at = stub(&mut body, NOT_STATIC as u32); // Cast: a small status
    for e in decline_edges {
        patch(&mut body, e, declined_at);
    }
    if let Some(e) = not_static_edge {
        patch(&mut body, e, not_static_at);
    }
    let mut buf = crate::ExecutableBuffer::new(4096).expect("executable buffer");
    buf.emit(&body);
    assert!(!buf.overflowed());
    buf.finalize();
    buf
}

type ProgFn = extern "C" fn(u64, u64, *mut Args) -> u64;

fn run(buf: &crate::ExecutableBuffer, base: u64, offset: u64, args: &mut Args) -> u64 {
    // SAFETY: a complete `extern "C" fn(u64, u64, *mut Args) -> u64` built by
    // `build`; it reads and writes only the fixtures the caller keeps alive
    // and clobbers RAX, RCX, RDX, R9, R10 and R11 (caller-saved on both ABIs).
    let f = unsafe { std::mem::transmute::<*const u8, ProgFn>(buf.as_ptr()) };
    f(base, offset, args as *mut Args)
}

fn rmw_prog(width: u8, op: UnsafeRmwOp) -> crate::ExecutableBuffer {
    build(None, &unsafe_inline_rmw_bytes(UnsafeInlineRmw { width, op }))
}

fn sign_extend(v: u64, width: u8) -> u64 {
    if width == 4 {
        v as u32 as i32 as i64 as u64 // Cast: the sign extension under test
    } else {
        v
    }
}

fn mask_of(width: u8) -> u64 {
    if width == 8 {
        u64::MAX
    } else {
        0xFFFF_FFFF
    }
}

/// The reference model: `(RAX result, memory after)` of one RMW.
fn model(op: UnsafeRmwOp, width: u8, mem: u64, operand: u64, expected: u64) -> (u64, u64) {
    let mask = mask_of(width);
    let (m, o, e) = (mem & mask, operand & mask, expected & mask);
    match op {
        UnsafeRmwOp::Cas => {
            if m == e {
                (1, o)
            } else {
                (0, m)
            }
        }
        UnsafeRmwOp::Cae => (sign_extend(m, width), if m == e { o } else { m }),
        UnsafeRmwOp::GetAndAdd => (sign_extend(m, width), m.wrapping_add(o) & mask),
        UnsafeRmwOp::GetAndSet => (sign_extend(m, width), o),
        UnsafeRmwOp::GetAndBitwise(UnsafeBitOp::Or) => (sign_extend(m, width), m | o),
        UnsafeRmwOp::GetAndBitwise(UnsafeBitOp::And) => (sign_extend(m, width), m & o),
        UnsafeRmwOp::GetAndBitwise(UnsafeBitOp::Xor) => (sign_extend(m, width), m ^ o),
    }
}

/// Every RMW op at both widths agrees with the model: the result in RAX
/// (`0` / `1` for a CAS, the previous value sign-extended for an `int`), the
/// word after, and the bytes around the word untouched, with garbage in the
/// unused upper half of every 32-bit operand and of `expected`.
#[test]
fn atm21_every_rmw_op_executes_as_the_model_says() {
    let ops = [
        UnsafeRmwOp::Cas,
        UnsafeRmwOp::Cae,
        UnsafeRmwOp::GetAndAdd,
        UnsafeRmwOp::GetAndSet,
        UnsafeRmwOp::GetAndBitwise(UnsafeBitOp::Or),
        UnsafeRmwOp::GetAndBitwise(UnsafeBitOp::And),
        UnsafeRmwOp::GetAndBitwise(UnsafeBitOp::Xor),
    ];
    let values: [u64; 6] = [0, 1, 0x7FFF_FFFF, 0x8000_0000, 0xFFFF_FFFF, 0x1234_5678_9ABC_DEF0];
    let disp = 16usize;
    for op in ops {
        for width in [4u8, 8u8] {
            let buf = rmw_prog(width, op);
            let garbage = if width == 4 { 0xDEAD_BEEF_0000_0000u64 } else { 0 };
            for &m in &values {
                for &o in &values {
                    for matching in [true, false] {
                        let mem = Mem::new();
                        mem.put(0, &[0xA5; 64]);
                        mem.put(disp, &m.to_le_bytes()[..usize::from(width)]);
                        let e = if matching { m } else { m ^ 1 };
                        let mut a = Args {
                            operand: o | garbage,
                            expected: e | garbage,
                            status: 0,
                        };
                        let r = run(&buf, mem.addr(), disp as u64, &mut a); // Cast: a small offset
                        let (want_r, want_m) = model(op, width, m, o, e);
                        let what = format!("{op:?} w{width} mem={m:#x} operand={o:#x} exp={e:#x}");
                        assert_eq!(a.status, HIT, "{what}");
                        assert_eq!(r, want_r, "result of {what}");
                        assert_eq!(
                            mem.word(disp, usize::from(width)),
                            want_m & mask_of(width),
                            "memory of {what}"
                        );
                        for k in 0..64usize {
                            if k < disp || k >= disp + usize::from(width) {
                                assert_eq!(mem.word(k, 1), 0xA5, "byte {k} of {what}");
                            }
                        }
                    }
                }
            }
        }
    }
}

/// The sub-word `LOCK CMPXCHG` compares and stores exactly its own width: the
/// bytes around the field survive, the upper bits of the operands are ignored,
/// and a compareAndExchange answers the witness sign-extended from the
/// field's width (a compareAndSet `0` / `1`).
#[test]
fn atm21_the_subword_cmpxchg_touches_only_its_own_bytes_and_widens_the_witness() {
    let pattern: [u8; 16] = [
        0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0xFF,
        0x01,
    ];
    for (width, disp) in [(1u8, 3usize), (1, 7), (1, 8), (2, 2), (2, 6), (2, 8)] {
        for cae in [true, false] {
            let buf = build(
                None,
                &unsafe_inline_subword_rmw_bytes(UnsafeSubwordRmw { width, cae }),
            );
            let w = usize::from(width);
            let field_mask: u64 = if width == 1 { 0xFF } else { 0xFFFF };
            let garbage: u64 = !field_mask;
            for matching in [true, false] {
                let mem = Mem::new();
                mem.put(0, &pattern);
                let cur = mem.word(disp, w);
                let new = !cur & field_mask;
                let e = if matching { cur } else { cur ^ 1 };
                let mut a = Args {
                    operand: new | garbage,
                    expected: e | garbage,
                    status: 0,
                };
                let r = run(&buf, mem.addr(), disp as u64, &mut a); // Cast: a small offset
                let what = format!("w{width} cae={cae} disp={disp} matching={matching}");
                assert_eq!(a.status, HIT, "{what}");
                let witness = if width == 1 {
                    cur as u8 as i8 as i64 as u64 // Cast: the sign extension under test
                } else {
                    cur as u16 as i16 as i64 as u64 // Cast: the sign extension under test
                };
                let want = if cae { witness } else { u64::from(matching) };
                assert_eq!(r, want, "result of {what}");
                let mut expect_bytes = pattern;
                if matching {
                    expect_bytes[disp..disp + w].copy_from_slice(&new.to_le_bytes()[..w]);
                }
                assert_eq!(mem.snapshot()[..16], expect_bytes, "memory of {what}");
            }
        }
    }
}

const MIRROR: u64 = 0x0000_5555_AAAA_1000;
/// Static offsets in the synthetic band; their entries are 7 and 18.
const OFF_I: usize = 0x2000_0007;
const OFF_J: usize = 0x2000_0010;
/// Same entry as `OFF_I` (the index reads bits 0..9 only), a different key.
const OFF_COLLIDES: usize = OFF_I + 0x400;
/// A different entry from `OFF_I`, never published.
const OFF_OTHER: usize = OFF_I + 1;

/// A fake statics block (cells of 16 bytes, tag at 0, payload at 4 / 8), the
/// base-pointer cell naming it, and the per-VM table with the entries for an
/// `int` static (field 0) and a `long` static (field 2).
struct StaticFixture {
    table: Vec<u64>,
    block: Box<Mem>,
    base_cell: Box<AtomicU64>,
}

impl StaticFixture {
    fn tag_bytes(tag: u8) -> [u8; 4] {
        u32::from(tag).to_le_bytes()
    }

    fn new() -> Self {
        let block = Mem::new();
        // Field 0: an int static (41). Field 1: an int neighbour. Field 2: a
        // long static (5 << 32). Field 3: a long neighbour.
        block.put(0, &Self::tag_bytes(UNSAFE_STATIC_TAG_INT));
        block.put(4, &41u32.to_le_bytes());
        block.put(16, &Self::tag_bytes(UNSAFE_STATIC_TAG_INT));
        block.put(20, &0x1111_1111u32.to_le_bytes());
        block.put(32, &Self::tag_bytes(UNSAFE_STATIC_TAG_LONG));
        block.put(40, &(5u64 << 32).to_le_bytes());
        block.put(48, &Self::tag_bytes(UNSAFE_STATIC_TAG_LONG));
        block.put(56, &0x2222_2222_2222_2222u64.to_le_bytes());
        let base_cell = Box::new(AtomicU64::new(block.addr()));
        let base_cell_addr = base_cell.as_ptr() as usize as u64; // Cast: the cell's address
        let mut table =
            vec![0u64; UNSAFE_STATIC_CELL_BASE / 8 + UNSAFE_STATIC_CELL_COUNT * UNSAFE_STATIC_CELL_WORDS];
        let mut publish = |offset: usize, wide: bool, field_index: usize| {
            let i = UNSAFE_STATIC_CELL_BASE / 8
                + UNSAFE_STATIC_CELL_WORDS * unsafe_static_cell_index(offset);
            table[i] = unsafe_static_cell_key(offset, wide);
            table[i + 1] = MIRROR;
            table[i + 2] = base_cell_addr;
            table[i + 3] = (9u64 << 32) | (field_index as u64 * 16); // Cast: a small index
        };
        publish(OFF_I, false, 0);
        publish(OFF_J, true, 2);
        Self {
            table,
            block,
            base_cell,
        }
    }

    fn table_addr(&self) -> usize {
        self.table.as_ptr() as usize // Cast: the fake table's address
    }

    fn prog(&self, width: u8, op: UnsafeRmwOp) -> crate::ExecutableBuffer {
        let (bytes, declines, not_static) = unsafe_inline_static_cell_bytes(self.table_addr(), width);
        build(
            Some((bytes, declines, Some(not_static))),
            &unsafe_inline_rmw_bytes(UnsafeInlineRmw { width, op }),
        )
    }

    /// `(RAX result, status)` of one run against mirror `base` at `offset`.
    fn go(
        &self,
        prog: &crate::ExecutableBuffer,
        base: u64,
        offset: u64,
        operand: u64,
        expected: u64,
    ) -> (u64, u64) {
        let mut a = Args {
            operand,
            expected,
            status: 0,
        };
        let r = run(prog, base, offset, &mut a);
        (r, a.status)
    }
}

/// The static arm, executed: its screen finds the entry, proves the mirror,
/// loads the statics block through the base cell, checks the cell's tag, and
/// the shared locked instruction then changes the static's payload word and
/// nothing else; every other shape declines (or leaves the band) with the
/// block untouched.
#[test]
fn atm21_the_static_arm_executes_hit_decline_and_not_static() {
    assert_eq!(unsafe_static_cell_index(OFF_I), 7);
    assert_eq!(unsafe_static_cell_index(OFF_J), 18);
    assert_eq!(
        unsafe_static_cell_index(OFF_COLLIDES),
        unsafe_static_cell_index(OFF_I)
    );
    assert_ne!(unsafe_static_cell_index(OFF_OTHER), unsafe_static_cell_index(OFF_I));
    let f = StaticFixture::new();
    let add4 = f.prog(4, UnsafeRmwOp::GetAndAdd);
    let set4 = f.prog(4, UnsafeRmwOp::GetAndSet);
    let cas4 = f.prog(4, UnsafeRmwOp::Cas);
    let cae4 = f.prog(4, UnsafeRmwOp::Cae);
    let add8 = f.prog(8, UnsafeRmwOp::GetAndAdd);
    let int_off = OFF_I as u64; // Cast: a band offset
    let long_off = OFF_J as u64; // Cast: a band offset

    // A getAndAdd of the int static: the previous value, payload + 5, the
    // tag word and every other byte untouched.
    let before = f.block.snapshot();
    assert_eq!(f.go(&add4, MIRROR, int_off, 5, 0), (41, HIT));
    assert_eq!(f.block.word(4, 4), 46);
    let mut want = before;
    want[4..8].copy_from_slice(&46u32.to_le_bytes());
    assert_eq!(f.block.snapshot(), want, "only the payload word changed");

    // getAndSet answers the previous value sign-extended and stores 32 bits
    // whatever the upper operand bits are.
    assert_eq!(
        f.go(&set4, MIRROR, int_off, 0xDEAD_BEEF_FFFF_FFFF, 0),
        (46, HIT)
    );
    assert_eq!(f.block.word(4, 4), 0xFFFF_FFFF);
    // CAS / compareAndExchange on it.
    assert_eq!(f.go(&cas4, MIRROR, int_off, 7, 0xFFFF_FFFF), (1, HIT));
    assert_eq!(f.block.word(4, 4), 7);
    assert_eq!(f.go(&cas4, MIRROR, int_off, 8, 0), (0, HIT));
    assert_eq!(f.go(&cae4, MIRROR, int_off, 9, 0), (7, HIT));
    assert_eq!(f.block.word(4, 4), 7, "a failed exchange stores nothing");
    assert_eq!(f.go(&cae4, MIRROR, int_off, 9, 7), (7, HIT));
    assert_eq!(f.block.word(4, 4), 9);
    assert_eq!(f.block.word(0, 4), u64::from(UNSAFE_STATIC_TAG_INT), "tag intact");

    // The long static: payload64 at +8 of its own cell (field 2 = byte 32).
    let before = f.block.snapshot();
    assert_eq!(f.go(&add8, MIRROR, long_off, 3, 0), (5u64 << 32, HIT));
    assert_eq!(f.block.word(40, 8), (5u64 << 32) + 3);
    let mut want = before;
    want[40..48].copy_from_slice(&((5u64 << 32) + 3).to_le_bytes());
    assert_eq!(f.block.snapshot(), want, "only the long payload changed");

    // Declines: the block must not change.
    let frozen = f.block.snapshot();
    let declined = |what: &str, got: (u64, u64)| {
        assert_eq!(got.1, DECLINED, "{what}");
        assert_eq!(f.block.snapshot(), frozen, "{what}: the block changed");
    };
    declined("the wide key on an int entry", f.go(&add8, MIRROR, int_off, 1, 0));
    declined("the narrow key on a long entry", f.go(&add4, MIRROR, long_off, 1, 0));
    declined("another class's mirror", f.go(&add4, MIRROR + 8, int_off, 1, 0));
    declined("a null base", f.go(&add4, 0, int_off, 1, 0));
    declined(
        "an in-band offset nobody published",
        f.go(&add4, MIRROR, OFF_OTHER as u64, 1, 0), // Cast: a band offset
    );
    declined(
        "an offset whose entry another key owns",
        f.go(&add4, MIRROR, OFF_COLLIDES as u64, 1, 0), // Cast: a band offset
    );
    f.base_cell.store(0, Ordering::Relaxed);
    declined("a class with no statics block", f.go(&add4, MIRROR, int_off, 1, 0));
    f.base_cell.store(f.block.addr(), Ordering::Relaxed);
    // A cell holding another kind of value (a reset block, a retyped static).
    f.block.put(0, &StaticFixture::tag_bytes(UNSAFE_STATIC_TAG_LONG));
    let frozen = f.block.snapshot();
    assert_eq!(f.go(&add4, MIRROR, int_off, 1, 0), (0, DECLINED), "a Long-tagged cell");
    assert_eq!(f.block.snapshot(), frozen);
    f.block.put(0, &StaticFixture::tag_bytes(2));
    assert_eq!(f.go(&add4, MIRROR, int_off, 1, 0).1, DECLINED, "a Float-tagged cell");
    f.block.put(0, &StaticFixture::tag_bytes(UNSAFE_STATIC_TAG_INT));
    f.block.put(32, &StaticFixture::tag_bytes(UNSAFE_STATIC_TAG_INT));
    assert_eq!(f.go(&add8, MIRROR, long_off, 1, 0).1, DECLINED, "an Int-tagged long cell");
    f.block.put(32, &StaticFixture::tag_bytes(UNSAFE_STATIC_TAG_LONG));

    // Outside the band the screen leaves RAX / R11 for the field screen.
    for off in [
        0x1FFF_FFFFu64,
        0x4000_0000,
        16,
        0,
        u64::MAX - 7,
    ] {
        assert_eq!(f.go(&add4, MIRROR, off, 1, 0).1, NOT_STATIC, "offset {off:#x}");
    }
    // And a repaired fixture hits again (the declines changed no state).
    assert_eq!(f.go(&add4, MIRROR, int_off, 1, 0).1, HIT);
}

/// The sub-word cell half is its own array: a byte cell and a short cell of
/// one `(class, slot)` are served to their own width only, and an int/long
/// cell never answers a sub-word screen (nor the reverse).
#[test]
fn atm21_the_subword_cell_half_is_separate_from_the_int_long_half() {
    const CLASS: u32 = 0x123;
    let epoch = AtomicU32::new(7);
    let epoch_addr = epoch.as_ptr() as usize; // Cast: the epoch cell's address
    let mut table = vec![0u64; 1 + 3 * UNSAFE_FIELD_CELL_COUNT];
    table[0] = 7;
    let sub_half = 1 + 2 * UNSAFE_FIELD_CELL_COUNT;
    let mut put = |half: usize, slot: usize, wide: bool, disp: usize| {
        table[half + unsafe_field_cell_index(CLASS, slot, wide)] =
            unsafe_field_cell_word(CLASS, slot, wide, disp).expect("fits a word");
    };
    put(sub_half, 3, false, 28); // a byte / boolean field
    put(sub_half, 3, true, 30); // a short / char field
    put(1, 5, false, 40); // an int field, int/long half
    put(sub_half, 6, false, 44); // a byte field
    let tbl = table.as_ptr() as usize; // Cast: the fake table's address
    let screen = |bytes: (Vec<u8>, Vec<usize>)| {
        // The hit leaves the displacement in R11; return it in RAX.
        build(Some((bytes.0, bytes.1, None)), &[0x4C, 0x89, 0xD8]) // MOV RAX, R11
    };
    let byte = screen(unsafe_inline_subword_field_cell_bytes(tbl, epoch_addr, 1));
    let short = screen(unsafe_inline_subword_field_cell_bytes(tbl, epoch_addr, 2));
    let int4 = screen(unsafe_inline_field_cell_bytes(tbl, epoch_addr, 4));
    let obj = Mem::new();
    obj.put(0, &CLASS.to_le_bytes());
    obj.put(
        cratonvm_types::GC_FLAGS_BYTE_OFFSET,
        &[cratonvm_types::GC_FLAG_COMPACT],
    );
    let go = |prog: &crate::ExecutableBuffer, slot: u64| {
        let mut a = Args::default();
        let r = run(prog, obj.addr(), slot, &mut a);
        (r, a.status)
    };
    assert_eq!(go(&byte, 3), (28, HIT), "the byte cell");
    assert_eq!(go(&short, 3), (30, HIT), "the short cell");
    assert_eq!(go(&byte, 6), (44, HIT));
    assert_eq!(go(&short, 6).1, DECLINED, "a byte cell is not a short cell");
    assert_eq!(go(&byte, 4).1, DECLINED, "an unpublished slot");
    assert_eq!(go(&byte, 5).1, DECLINED, "an int cell is not a sub-word cell");
    assert_eq!(go(&int4, 5), (40, HIT), "the int cell");
    assert_eq!(go(&int4, 3).1, DECLINED, "a sub-word cell is not an int cell");
    assert_eq!(go(&int4, 6).1, DECLINED, "a sub-word cell is not an int cell");
}

/// The locked sequences are atomic against one another across threads: 4
/// threads of `LOCK XADD` add exactly, a CAS retry loop counts exactly, and a
/// toggle of a private bit of a shared word by `LOCK CMPXCHG` loops leaves no
/// bit flipped (a lost update would).
#[test]
fn atm21_the_locked_sequences_are_atomic_across_threads() {
    const THREADS: usize = 4;
    const N: u64 = 20_000; // even: each thread toggles its bit an even number of times
    let xadd = rmw_prog(8, UnsafeRmwOp::GetAndAdd);
    let cas = rmw_prog(8, UnsafeRmwOp::Cas);
    let xor4 = rmw_prog(4, UnsafeRmwOp::GetAndBitwise(UnsafeBitOp::Xor));
    let cell = Mem::new();
    let base = cell.addr();
    let (xa, ca, xo) = (
        xadd.as_ptr() as usize, // Cast: code addresses, handed to the threads
        cas.as_ptr() as usize,
        xor4.as_ptr() as usize,
    );
    std::thread::scope(|s| {
        for t in 0..THREADS {
            s.spawn(move || {
                // SAFETY: the three buffers outlive the scope; each address is
                // the start of a complete `ProgFn`.
                let (fx, fc, fo) = unsafe {
                    (
                        std::mem::transmute::<usize, ProgFn>(xa),
                        std::mem::transmute::<usize, ProgFn>(ca),
                        std::mem::transmute::<usize, ProgFn>(xo),
                    )
                };
                for _ in 0..N {
                    let mut a = Args {
                        operand: 1,
                        expected: 0,
                        status: 0,
                    };
                    fx(base, 0, &mut a);
                    loop {
                        // SAFETY: `base + 8` is inside the 64-byte cell block,
                        // 8-aligned; only locked instructions write it.
                        let cur = unsafe { std::ptr::read_volatile((base + 8) as usize as *const u64) }; // Cast: an address
                        let mut a = Args {
                            operand: cur + 1,
                            expected: cur,
                            status: 0,
                        };
                        if fc(base, 8, &mut a) == 1 {
                            break;
                        }
                    }
                    let mut a = Args {
                        operand: 1 << t,
                        expected: 0,
                        status: 0,
                    };
                    fo(base, 16, &mut a);
                    fo(base, 16, &mut a);
                }
            });
        }
    });
    let total = THREADS as u64 * N; // Cast: small counts
    assert_eq!(cell.word(0, 8), total, "LOCK XADD lost an addition");
    assert_eq!(cell.word(8, 8), total, "the CAS loop lost an increment");
    assert_eq!(cell.word(16, 4), 0, "a LOCK CMPXCHG toggle lost an update");
}
