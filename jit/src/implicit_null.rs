// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Implicit null checks: the receiver dereference *is* the check.
//!
//! A `getfield` on a null receiver must raise `NullPointerException`. The
//! ordinary way to guarantee that is to emit `TEST RAX, RAX; JZ slow` before
//! the load. The implicit way is to emit nothing, let the load fault on the
//! null page, and translate the `SIGSEGV` / `EXCEPTION_ACCESS_VIOLATION` back
//! into the slow path that raises the NPE.
//!
//! This module owns the faulting-PC → recovery-PC map that makes that
//! translation possible, and the three hazards it has to close.
//!
//! # Hazard 1 — the lookup runs in a signal handler
//!
//! It must be async-signal-safe, which rules out the `Mutex` that
//! [`crate::lookup_jit_method_name`] uses. That function is only ever called
//! while the process is already dying, so a `try_lock` there is acceptable;
//! here the process is expected to *survive*, and a handler that blocks on a
//! lock its own interrupted thread holds would deadlock.
//!
//! So the table is a fixed-capacity array of atomics, and the reader does
//! nothing but relaxed/acquire loads. There is no allocation, no lock, and no
//! call into anything that takes one.
//!
//! # Hazard 2 — a JIT code buffer is freed and its address REUSED
//!
//! This is the one that makes the feature hard, and the reason it did not land
//! with the rest of the round. `CompiledMethod::drop` unmaps the buffer it
//! owns, and — in that function's own words — "the address is then reusable by
//! the next `alloc_executable`". An entry left behind would eventually match a
//! PC belonging to *different* code, and the handler would resume execution at
//! a stale recovery address inside a live method. That is not a crash; it is
//! silent, arbitrary control flow.
//!
//! Two things close it:
//!
//! * `CompiledMethod::drop` calls [`unregister_range`] for its own code range,
//!   beside the `unregister_jit_method_name` call that exists for exactly the
//!   same reason.
//! * **Slots are never reused.** `NEXT` only ever increments; retiring an
//!   entry stores `0` into its `fault_pc` and leaks the slot. Reuse would open
//!   a genuine race — a reader that has already matched `fault_pc` could read
//!   a `recover_pc` that a concurrent re-registration has since overwritten,
//!   and jump into the wrong method. Leaking is the cheap way to make that
//!   unrepresentable rather than merely unlikely.
//!
//! [`unregister_range`] and [`register`] agree on the base address only because
//! **the method entry IS the buffer base**. `CompiledMethod::drop` retires
//! `[entry, entry + buffer.pos())` while registration keys sites off
//! `cm.entry + fault_off`; `x64::driver` establishes the equality in as many
//! words (`let entry_offset = 0; // prologue starts at offset 0`), and the
//! OSR-trampoline purge in that same `Drop` already leans on it. Give the
//! prologue a non-zero offset and every site below the new entry silently stops
//! being retired — which is exactly the stale-entry hazard this design exists
//! to prevent, arriving through the one door nobody would think to check.
//!
//! Retirement cannot race a fault in the method being retired, and the reason
//! is an invariant this file borrows rather than establishes: a
//! `CompiledMethod` is only dropped when **no frame of it is live** — the same
//! sentence `release_compile_id` and `unregister_jit_method_name` rely on in
//! the very same `Drop`. A thread cannot be executing at a PC inside a method
//! that is being unmapped, so it cannot be faulting at one either. What the
//! retirement protects against is not that race but the LATER one: a fault in
//! whatever code `alloc_executable` puts at the same address next.
//!
//! Exhaustion is therefore possible in a long run with heavy recompilation.
//! It is handled by *declining* — [`register`] returns `false`, the compiler
//! emits the explicit check for that site, and `DECLINED` counts it. The
//! feature turns itself off instead of turning unsound. A seqlock per slot
//! would allow reuse and is the obvious upgrade if the decline count ever
//! becomes non-trivial.
//!
//! # Hazard 3 — recovering from a fault that is NOT an implicit null check
//!
//! A genuine JIT bug also produces a `SIGSEGV` inside compiled code, and
//! silently resuming from one would convert a diagnosable crash into corrupted
//! state. Every one of these must hold before [`recover`] answers:
//!
//! * the signal is a memory-access fault (checked by the caller);
//! * `si_code` says a real hardware fault, so `si_addr` is an address at all
//!   rather than a union member left over from a `kill -SEGV`;
//! * the faulting address is inside the **null page** (`< 4096`) — a null
//!   receiver plus a small field offset, and nothing else;
//! * the faulting PC is an **exact** registered entry, not merely inside some
//!   compiled method's range.
//!
//! The last two are what separate "a null receiver reached a load we chose not
//! to guard" from "compiled code dereferenced garbage". A wild pointer does
//! not land in the first page, and a PC that is not the exact instruction we
//! registered is not ours.
//!
//! # What the compiler must guarantee, and does
//!
//! The registered PC has to be an instruction that (a) faults whenever the
//! receiver is null, and (b) has no side effect before it faults. The
//! compiler verifies the emitted bytes rather than trusting the source: see
//! `x64::Compiler::bind_implicit_null_recovery`, which decodes the instruction
//! at the recorded offset and **fails the compile** if it is not the expected
//! `MOV r32, [RAX + disp32]` with `disp32 < 4096`. A failed compile falls back
//! to the interpreter, so the fail-closed direction never leaves unguarded
//! code running.

use std::sync::atomic::{AtomicUsize, Ordering};

/// Faults at or above this address are never an implicit null check. One page
/// covers a null receiver plus any field offset the compiler will fold into
/// the addressing mode; the emitter's own verification caps the displacement
/// at the same number, so the two agree by construction.
pub const NULL_PAGE_LIMIT: usize = 4096;

/// Registered sites. 32,768 × two words = 512 KiB of zero-initialised BSS.
const CAP: usize = 1 << 15;

/// `0` means "empty, or retired". A real faulting PC is never 0.
///
/// The inline `const` block is a fresh `AtomicUsize` PER ELEMENT. It replaced a
/// named `const ZERO`, which worked for the same reason -- a `const` is copied
/// at each use -- but is the shape `clippy::declare_interior_mutable_const`
/// warns about, because the same name read anywhere else would silently
/// produce a temporary to mutate rather than shared state.
static FAULT_PC: [AtomicUsize; CAP] = [const { AtomicUsize::new(0) }; CAP];
static RECOVER_PC: [AtomicUsize; CAP] = [const { AtomicUsize::new(0) }; CAP];
/// Monotonic high-water mark. Never decreases — see hazard 2.
static NEXT: AtomicUsize = AtomicUsize::new(0);

static REGISTERED: AtomicUsize = AtomicUsize::new(0);
static RETIRED: AtomicUsize = AtomicUsize::new(0);
static RECOVERED: AtomicUsize = AtomicUsize::new(0);
static DECLINED: AtomicUsize = AtomicUsize::new(0);

/// Is the implicit null check enabled? **Default ON** since 2026-09-02; opt
/// out with `CRATONVM_JIT_IMPLICIT_NULL_CHECK=0`.
///
/// # What the off arm restores, exactly
///
/// `emit_trusted_oop_receiver_check` at both `getfield` arms, unconditionally
/// — the behaviour of every binary before this feature existed. Nothing
/// registers, so [`recover`] scans an empty table and answers `None` on the
/// first load, and a fault in compiled code reaches the crash reporter exactly
/// as it always did. There is no degraded middle state.
///
/// # Read this before deciding the flag is unnecessary
///
/// This switch guards the only mechanism in this backend whose wrong arm is
/// **silent**. Every other one produces a wrong answer, which a test catches;
/// a stale or mis-shaped entry here resumes execution at an address the table
/// chose, which nothing catches. That is why the kill switch exists and why it
/// should keep existing even though the default moved: the first thing anyone
/// debugging an unexplained crash in compiled code should be able to do is
/// take this out of the picture in one run, on the same binary.
///
/// It was default-off through its soak (see `docs/JIT_OPTIMIZATION.md`): about
/// an hour of continuous execution plus two full regression-suite passes, with
/// 11,663 of 11,719 null dereferences recovered as translated hardware faults
/// under GC pressure, every checksum matching HotSpot, and 87/87 twice.
pub fn enabled() -> bool {
    use std::sync::OnceLock;
    static G: OnceLock<bool> = OnceLock::new();
    *G.get_or_init(|| {
        !matches!(
            cratonvm_types::flags::runtime_var("CRATONVM_JIT_IMPLICIT_NULL_CHECK").as_deref(),
            Ok("0") | Ok("false") | Ok("off") | Ok("no")
        )
    })
}

/// Register one site. `false` means the table is full and the caller must emit
/// an explicit check instead.
///
/// Publication order matters: `recover_pc` is stored **before** `fault_pc`,
/// and `fault_pc` is stored with `Release`. A reader that loads a matching
/// `fault_pc` with `Acquire` therefore always sees the paired `recover_pc`.
pub fn register(fault_pc: usize, recover_pc: usize) -> bool {
    if fault_pc == 0 || recover_pc == 0 {
        return false;
    }
    let i = NEXT.fetch_add(1, Ordering::Relaxed);
    if i >= CAP {
        DECLINED.fetch_add(1, Ordering::Relaxed);
        return false;
    }
    RECOVER_PC[i].store(recover_pc, Ordering::Relaxed);
    FAULT_PC[i].store(fault_pc, Ordering::Release);
    REGISTERED.fetch_add(1, Ordering::Relaxed);
    true
}

/// Retire every site inside `[base, base + len)`.
///
/// Called from `CompiledMethod::drop` before the buffer is unmapped. After
/// this returns, no PC in that range can be recovered — which is the whole
/// point, because the address is about to belong to someone else.
pub fn unregister_range(base: usize, len: usize) {
    let end = base.saturating_add(len);
    let hi = NEXT.load(Ordering::Relaxed).min(CAP);
    for slot in FAULT_PC.iter().take(hi) {
        let pc = slot.load(Ordering::Relaxed);
        if pc >= base && pc < end && slot.swap(0, Ordering::Release) != 0 {
            RETIRED.fetch_add(1, Ordering::Relaxed);
        }
    }
}

/// The recovery PC for a fault, or `None` to let the crash reporter have it.
///
/// **Async-signal-safe**: atomic loads only. Callers must already have
/// established that this was a hardware memory-access fault; this function
/// enforces the null-page and exact-PC conditions.
pub fn recover(fault_pc: usize, fault_addr: usize) -> Option<usize> {
    if fault_pc == 0 || fault_addr >= NULL_PAGE_LIMIT {
        return None;
    }
    let hi = NEXT.load(Ordering::Relaxed).min(CAP);
    for i in 0..hi {
        if FAULT_PC[i].load(Ordering::Acquire) == fault_pc {
            let target = RECOVER_PC[i].load(Ordering::Relaxed);
            if target != 0 {
                RECOVERED.fetch_add(1, Ordering::Relaxed);
                return Some(target);
            }
        }
    }
    None
}

/// `(registered, retired, recovered, declined)`.
///
/// All four, never a subset. `registered` alone cannot distinguish "the
/// feature is off" from "it is on and no site qualified"; `recovered` alone
/// cannot distinguish "no null receiver occurred" from "the table never had
/// the entry"; and a rising `declined` is the one reading that means the
/// feature has quietly stopped applying.
pub fn counts() -> (usize, usize, usize, usize) {
    (
        REGISTERED.load(Ordering::Relaxed),
        RETIRED.load(Ordering::Relaxed),
        RECOVERED.load(Ordering::Relaxed),
        DECLINED.load(Ordering::Relaxed),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A registered site round-trips, and only for a null-page address.
    #[test]
    fn a_registered_site_recovers_only_inside_the_null_page() {
        let fault = 0x4000_0000_1000usize;
        let recover = 0x4000_0000_2000usize;
        assert!(register(fault, recover));

        assert_eq!(recover_at(fault, 0), Some(recover));
        assert_eq!(recover_at(fault, 15), Some(recover));
        assert_eq!(
            recover_at(fault, NULL_PAGE_LIMIT),
            None,
            "an address outside the null page is a wild pointer, not a null \
             receiver -- resuming from one would convert a diagnosable crash \
             into arbitrary control flow"
        );
        assert_eq!(
            recover_at(fault + 1, 0),
            None,
            "the PC must match EXACTLY; being inside some compiled method is \
             not enough"
        );
        unregister_range(fault, 1);
    }

    /// Retiring a range makes its PCs unrecoverable — the property that keeps
    /// a freed-and-reused buffer from resuming at a stale address.
    #[test]
    fn a_retired_range_stops_recovering_and_the_slot_is_not_reused() {
        let base = 0x5000_0000_0000usize;
        assert!(register(base + 0x10, base + 0x80));
        assert!(register(base + 0x20, base + 0x90));
        assert_eq!(recover_at(base + 0x10, 0), Some(base + 0x80));

        let before = NEXT.load(Ordering::Relaxed);
        unregister_range(base, 0x100);
        assert_eq!(recover_at(base + 0x10, 0), None);
        assert_eq!(recover_at(base + 0x20, 0), None);

        // The slots are LEAKED, not recycled. Reuse would let a reader that
        // already matched `fault_pc` read a `recover_pc` a concurrent
        // re-registration had overwritten.
        assert!(register(base + 0x30, base + 0xA0));
        assert!(
            NEXT.load(Ordering::Relaxed) > before,
            "registration after a retirement must consume a FRESH slot"
        );
        unregister_range(base, 0x100);
    }

    /// A zero on either side is refused rather than stored, because `0` is the
    /// sentinel this table uses for "retired".
    #[test]
    fn a_zero_pc_is_refused_because_zero_is_the_retired_sentinel() {
        assert!(!register(0, 0x1234));
        assert!(!register(0x1234, 0));
    }

    fn recover_at(pc: usize, addr: usize) -> Option<usize> {
        recover(pc, addr)
    }
}
