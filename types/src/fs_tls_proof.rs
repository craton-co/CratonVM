// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Proof that a Rust `thread_local!` cell is reachable as `fs:[disp]` on
//! EVERY thread, not only on the thread that derived `disp`.
//!
//! Compiled code reads (and in one case `LOCK INC`s through) several
//! thread-local mirrors with a single `mov reg, fs:[disp32]`: the JIT's
//! innermost-RBP, compile-id and `JIT_THREAD` mirrors (`jit/src/x64/licm.rs`)
//! and the FFM shared-session access-word mirror
//! (`native-builtins/src/ffm_fast.rs`). The displacement is derived once per
//! process as `&cell - fs_base` on the first thread that asks.
//!
//! That offset is thread-invariant only when the cell lives in the STATIC TLS
//! block (the initial-exec / local-exec models: the `cratonvm` executable, or
//! a shared object loaded as `DT_NEEDED`). A crate that ends up in DYNAMIC TLS
//! (general-dynamic model: e.g. `libcratonvm.so` `dlopen`ed by an embedder
//! once the static TLS surplus is exhausted) gets a separately allocated
//! block per thread, and `fs_base + disp` then names an unrelated address on
//! every thread but the one that measured it. A sentinel round trip on the
//! measuring thread cannot see that: there `fs_base + disp == &cell` holds by
//! construction. The old per-mirror probes did exactly that and so proved
//! nothing about the other threads (JIT round 20, lane tls20).
//!
//! [`prove_fs_disp`] makes the proof sound:
//!
//! 1. the offset must be negative (x86-64 uses TLS variant II: the static
//!    block lies BELOW the thread pointer, so a non-negative offset is not a
//!    static-TLS cell);
//! 2. the sentinel round trip must succeed on the calling thread; and
//! 3. a freshly spawned thread must measure the SAME offset for its own
//!    instance of the cell and pass its own sentinel round trip.
//!
//! A fresh thread is the case dynamic TLS gets wrong (its block is allocated
//! lazily on first access, independently of the TCB), so agreement there is
//! what static TLS guarantees and dynamic TLS does not. Any failure, including
//! a failed spawn, answers `None`: every caller already has a non-mirror path
//! (the helper call, or the deopt the FFM fold keeps), so an unproven offset
//! costs speed, never memory.

/// The `fs:`-relative displacement of the thread-local cell whose address
/// `cell_addr` returns on the calling thread, proved on this thread AND on a
/// freshly spawned one; `None` when it cannot be proved (see the module docs).
///
/// `cell_addr` must return the address of the calling thread's instance of an
/// 8-byte, 8-aligned, drop-free thread-local cell (`Cell<usize>`), or `None`
/// when the thread-local is unavailable (being destroyed). It must not call
/// back into whatever caches this function's answer: it runs on a second
/// thread while the caller is still computing. `sentinel` is written into the
/// cell for the duration of one load on each thread and then the old value is
/// put back; pick a value no real mirror content can equal.
///
/// Cold: it spawns and joins one thread. Callers cache the answer per process.
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
pub fn prove_fs_disp(cell_addr: fn() -> Option<usize>, sentinel: usize) -> Option<i32> {
    let here = measure_on_this_thread(cell_addr, sentinel)?;
    let there = std::thread::Builder::new()
        .name("cvm-tls-proof".to_string())
        .spawn(move || measure_on_this_thread(cell_addr, sentinel))
        .ok()?
        .join()
        .ok()??;
    (here == there).then_some(here)
}

/// No `fs:` mirrors elsewhere.
#[cfg(not(all(target_os = "linux", target_arch = "x86_64")))]
pub fn prove_fs_disp(_cell_addr: fn() -> Option<usize>, _sentinel: usize) -> Option<i32> {
    None
}

/// The single-thread half of [`prove_fs_disp`]: derive `&cell - fs_base` on
/// the calling thread and check it with a sentinel round trip. On its own this
/// proves only that `fs:[0]` is the TCB self pointer; the cross-thread
/// comparison in [`prove_fs_disp`] is what proves thread invariance.
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
fn measure_on_this_thread(cell_addr: fn() -> Option<usize>, sentinel: usize) -> Option<i32> {
    let addr = cell_addr()?;
    if addr == 0 || !addr.is_multiple_of(8) {
        return None;
    }
    // SAFETY: `fs:[0]` is the x86-64 SysV TCB self pointer, mapped on every
    // thread; if the convention did not hold this is a wrong number, never a
    // wild dereference -- only the sentinel comparison below acts on it.
    let fs_base = unsafe { read_fs_qword(0) };
    let delta = i32::try_from((addr as i128) - (fs_base as i128)).ok()?;
    // Variant II: static TLS lives below the thread pointer.
    if delta >= 0 {
        return None;
    }
    let cell = addr as *mut usize;
    // SAFETY: `cell_addr`'s contract: `addr` is this thread's own live,
    // aligned, drop-free `Cell<usize>`; no other thread touches it and no
    // reference into it is held across these accesses (a `Cell` is interior
    // mutable, so writing through a pointer derived from `&Cell` is allowed).
    let old = unsafe { cell.read_volatile() };
    // SAFETY: as above.
    unsafe { cell.write_volatile(sentinel) };
    // SAFETY: `delta` is `addr - fs_base` for this thread, so `fs:[delta]` is
    // `addr` -- 8 aligned, mapped bytes -- iff the `fs:[0]` convention holds;
    // the comparison below decides that. Read-only.
    let probed = unsafe { read_fs_qword(delta as isize) };
    // SAFETY: as for the write above.
    unsafe { cell.write_volatile(old) };
    (probed == sentinel).then_some(delta)
}

/// # Safety
///
/// `offset` must name 8 readable bytes relative to this thread's FS base
/// (`0`, the TCB self pointer, always does).
//
// SAFETY: one 8-byte FS-relative load and nothing else; `nostack`,
// `preserves_flags` and `readonly` hold for a bare `mov`.
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
#[inline]
unsafe fn read_fs_qword(offset: isize) -> usize {
    let out: usize;
    core::arch::asm!(
        "mov {out}, qword ptr fs:[{addr}]",
        addr = in(reg) offset,
        out = out(reg) out,
        options(nostack, preserves_flags, readonly),
    );
    out
}

#[cfg(all(test, target_os = "linux", target_arch = "x86_64"))]
mod tests {
    use super::*;
    use std::cell::Cell;

    thread_local! {
        static CELL_A: Cell<usize> = const { Cell::new(0) };
        static CELL_B: Cell<usize> = const { Cell::new(0) };
        /// Set only on the test's own thread: steers [`a_here_b_elsewhere`].
        static IS_MEASURING_THREAD: Cell<bool> = const { Cell::new(false) };
    }

    fn addr_a() -> Option<usize> {
        CELL_A.try_with(|c| c as *const Cell<usize> as usize).ok()
    }

    /// A cell whose offset differs between the measuring thread and every
    /// other thread: what dynamic TLS looks like to a one-thread probe.
    fn a_here_b_elsewhere() -> Option<usize> {
        if IS_MEASURING_THREAD.with(Cell::get) {
            CELL_A.try_with(|c| c as *const Cell<usize> as usize).ok()
        } else {
            CELL_B.try_with(|c| c as *const Cell<usize> as usize).ok()
        }
    }

    const SENTINEL: usize = 0x5446_5354_4C53_3230;

    #[test]
    fn a_static_tls_cell_is_proved_on_two_threads_and_reads_through_fs_on_a_third() {
        // The test binary links this crate statically: static TLS, so the
        // proof must succeed (a `None` here would silently disable every
        // mirror in the shipped launcher too).
        let disp = prove_fs_disp(addr_a, SENTINEL).expect("static TLS offset proved");
        assert!(disp < 0, "variant II offset is negative, got {disp}");
        assert_eq!(CELL_A.with(Cell::get), 0, "the proof restores the cell");
        // The proved offset reaches each thread's own cell.
        let seen = std::thread::spawn(move || {
            CELL_A.with(|c| c.set(0x1234_5678));
            // SAFETY: `disp` was proved thread-invariant just above.
            unsafe { read_fs_qword(disp as isize) }
        })
        .join()
        .unwrap();
        assert_eq!(seen, 0x1234_5678);
        CELL_A.with(|c| c.set(0x0BAD_F00D));
        // SAFETY: as above, on this thread.
        assert_eq!(unsafe { read_fs_qword(disp as isize) }, 0x0BAD_F00D);
        CELL_A.with(|c| c.set(0));
    }

    #[test]
    fn an_offset_that_differs_on_another_thread_is_refused() {
        IS_MEASURING_THREAD.with(|f| f.set(true));
        // Positive control: the one-thread probe (what the mirrors used to
        // run) accepts it...
        assert!(measure_on_this_thread(a_here_b_elsewhere, SENTINEL).is_some());
        // ...and the two-thread proof does not.
        assert_eq!(prove_fs_disp(a_here_b_elsewhere, SENTINEL), None);
        IS_MEASURING_THREAD.with(|f| f.set(false));
    }

    #[test]
    fn a_non_tls_address_is_refused() {
        fn heap() -> Option<usize> {
            // A per-call heap cell: never at a fixed fs offset.
            Some(Box::into_raw(Box::new(Cell::new(0usize))) as usize)
        }
        assert_eq!(prove_fs_disp(heap, SENTINEL), None);
        assert_eq!(prove_fs_disp(|| None, SENTINEL), None);
        assert_eq!(prove_fs_disp(|| Some(0), SENTINEL), None);
    }
}
