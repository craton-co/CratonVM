// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
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
    prove_fs_disp_with(&cell_addr, sentinel)
}

/// [`prove_fs_disp`] for a cell address that needs captured state (the
/// `dlopen`ed mirror shim's handle). Same contract, same two-thread proof.
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
pub fn prove_fs_disp_with(
    cell_addr: &(dyn Fn() -> Option<usize> + Sync),
    sentinel: usize,
) -> Option<i32> {
    let here = measure_on_this_thread(cell_addr, sentinel)?;
    let there = std::thread::scope(|s| {
        std::thread::Builder::new()
            .name("cvm-tls-proof".to_string())
            .spawn_scoped(s, || measure_on_this_thread(cell_addr, sentinel))
            .ok()?
            .join()
            .ok()?
    })?;
    (here == there).then_some(here)
}

/// This thread's FS base (the TCB self pointer, `fs:[0]`).
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
pub fn fs_base() -> usize {
    // SAFETY: `fs:[0]` is mapped on every x86-64 SysV thread.
    unsafe { read_fs_qword(0) }
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
fn measure_on_this_thread(
    cell_addr: &(dyn Fn() -> Option<usize> + Sync),
    sentinel: usize,
) -> Option<i32> {
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
mod tests;
