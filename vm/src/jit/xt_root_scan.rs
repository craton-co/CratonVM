// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! BUG-03 — cross-thread stop-the-world JIT conservative root scan.
//!
//! ## The gap this closes
//!
//! A stop-the-world GC initiator marks every *parked* mutator from that
//! thread's published `root_snapshot`. A thread that is executing
//! JIT-compiled code, however, never cooperatively reaches an interpreter
//! safepoint: JIT code does not poll the STW flag. Its live object roots
//! therefore live only in its CPU registers and JIT stack frames, which the
//! cross-thread collector cannot see (the thread-local conservative scanner
//! runs on the *collector's* stack, not the peer's). Its only coverage is
//! the snapshot it published at its last object-returning native call — and
//! if its JIT slots advanced past that snapshot, the collector is blind to
//! the new roots and the non-moving sweep reclaims a still-live object. The
//! peer then dereferences the freed slot → SIGSEGV (the classic truncated /
//! garbage pointer crash). See `conservative_roots::warn_cross_thread_jit_gap`.
//!
//! ## The fix (Windows)
//!
//! Instead of relying on the in-JIT peer to *cooperatively* arrive at a
//! safepoint (which it may never do while spinning in a JIT loop — the
//! `wait_for_all` hang), the collector forcibly stops it at the OS level:
//!
//!   1. Enumerate every other OS thread of this process.
//!   2. `SuspendThread` + `GetThreadContext` each one.
//!   3. If its `Rip` is inside a registered JIT code range, the thread is
//!      executing pure JIT instructions and holds **no** VM/Rust lock (every
//!      lock-taking helper is itself Rust code, so its `Rip` would be outside
//!      the JIT range). It is safe to keep it frozen across the collection.
//!      Conservatively scan its 16 integer registers and its used stack for
//!      heap object addresses (validated by the lock-free
//!      [`VmHeap::is_object_address`]) and add them as roots.
//!   4. Otherwise (the thread is in the interpreter / a native / already
//!      parked) resume it immediately and let it arrive cooperatively — it
//!      either reaches a safepoint and parks, or it is a blocking-native
//!      thread already excluded from the barrier. We must NOT keep such a
//!      thread frozen: it may hold a lock the collector needs (deadlock).
//!
//! The taken-over (frozen) peers are excluded from the barrier's `expected`
//! count (so `wait_for_all` cannot hang on them) and resumed once the
//! collection has completed.
//!
//! ## Why this is sound
//!
//!   * A frozen in-JIT peer keeps its JIT-entry guard live, so
//!     `gc_quiescence::is_active()` stays `true` for the whole collection and
//!     the heap performs a **non-moving** sweep. Conservatively-discovered
//!     roots are therefore never relocated — pinning is implicit and an
//!     interior / false-positive pointer can never be mis-rewritten.
//!   * `is_object_address` is lock-free and inclusive: a false positive only
//!     inflates retention; a real object is never missed.
//!   * Only threads whose `Rip` is in JIT code (lock-free instruction stream)
//!     are held across the mark/sweep, so the collector can never deadlock on
//!     a lock owned by a frozen thread.
//!
//! Gated behind `CRATONVM_XT_JIT_ROOT_SCAN=1` (default off) while it bakes;
//! flipping the default to on (opt-out) is a one-line change in [`enabled`]
//! once the app gauntlet has validated it.

use crate::types::ObjectRef;
use std::sync::atomic::{AtomicU64, Ordering};

/// Number of times a peer thread was taken over (suspended in JIT and
/// conservatively scanned). Exposed for tests / JFR / the validation gate.
pub static XT_THREADS_TAKEN_OVER: AtomicU64 = AtomicU64::new(0);
/// Number of conservative roots contributed by taken-over peers.
pub static XT_ROOTS_FOUND: AtomicU64 = AtomicU64::new(0);

/// Whether the cross-thread STW JIT root scan is enabled.
///
/// Default OFF; set `CRATONVM_XT_JIT_ROOT_SCAN=1` to enable. (When the
/// default is eventually flipped to on, this becomes an opt-OUT check.)
#[inline]
pub fn enabled() -> bool {
    static CACHE: AtomicU64 = AtomicU64::new(u64::MAX);
    let c = CACHE.load(Ordering::Relaxed);
    if c != u64::MAX {
        return c == 1;
    }
    let on = matches!(
        std::env::var("CRATONVM_XT_JIT_ROOT_SCAN").as_deref(),
        Ok("1") | Ok("true") | Ok("on")
    );
    CACHE.store(on as u64, Ordering::Relaxed);
    on
}

#[inline]
fn dbg() -> bool {
    std::env::var_os("CRATONVM_DBG_XT_JIT_ROOT_SCAN").is_some()
}

/// Handles of peer threads that were suspended in JIT code and must be
/// resumed once the collection completes. Resuming is mandatory for liveness
/// (a leaked suspend wedges the peer forever), so this is `#[must_use]`.
#[must_use = "suspended peers must be resumed via resume()"]
#[derive(Default)]
pub struct TakenOver {
    handles: Vec<isize>,
    tids: Vec<u32>,
}

impl TakenOver {
    /// Number of peers currently held suspended.
    pub fn count(&self) -> usize {
        self.handles.len()
    }
    fn contains(&self, tid: u32) -> bool {
        self.tids.iter().any(|&t| t == tid)
    }
}

// ---------------------------------------------------------------------------
// Windows implementation
// ---------------------------------------------------------------------------

#[cfg(windows)]
mod imp {
    use super::*;
    use core::ffi::c_void;

    #[link(name = "kernel32")]
    extern "system" {
        fn GetCurrentThreadId() -> u32;
        fn GetCurrentProcessId() -> u32;
        fn OpenThread(access: u32, inherit: i32, tid: u32) -> isize;
        fn SuspendThread(h: isize) -> u32;
        fn ResumeThread(h: isize) -> u32;
        fn GetThreadContext(h: isize, ctx: *mut u8) -> i32;
        fn CloseHandle(h: isize) -> i32;
        fn CreateToolhelp32Snapshot(flags: u32, pid: u32) -> isize;
        fn Thread32First(snap: isize, entry: *mut ThreadEntry32) -> i32;
        fn Thread32Next(snap: isize, entry: *mut ThreadEntry32) -> i32;
    }

    // THREADENTRY32 (28 bytes). MUST stay structurally identical to the
    // declaration in `stwhang_watch.rs` — both reach `Thread32First/Next`,
    // so `clashing_extern_declarations` compares them.
    #[repr(C)]
    struct ThreadEntry32 {
        dw_size: u32,
        cnt_usage: u32,
        th32_thread_id: u32,
        th32_owner_process_id: u32,
        tp_base_pri: i32,
        tp_delta_pri: i32,
        dw_flags: u32,
    }
    const TH32CS_SNAPTHREAD: u32 = 0x0000_0004;

    // VirtualQuery — MUST stay byte-for-byte structurally identical to the
    // declarations in `memwatch.rs` / `crash_handler.rs` (same symbol →
    // `clashing_extern_declarations` deny-lint compares them).
    #[repr(C)]
    struct MemoryBasicInformation {
        base_address: *mut c_void,
        allocation_base: *mut c_void,
        allocation_protect: u32,
        partition_id: u16,
        _pad: u16,
        region_size: usize,
        state: u32,
        protect: u32,
        type_: u32,
    }
    extern "system" {
        fn VirtualQuery(
            lp_address: *const c_void,
            lp_buffer: *mut MemoryBasicInformation,
            dw_length: usize,
        ) -> usize;
    }
    const MEM_COMMIT: u32 = 0x1000;
    const PAGE_NOACCESS: u32 = 0x01;
    const PAGE_GUARD: u32 = 0x100;

    const THREAD_GET_CONTEXT: u32 = 0x0008;
    const THREAD_SUSPEND_RESUME: u32 = 0x0002;
    const THREAD_QUERY_INFORMATION: u32 = 0x0040;

    // x64 CONTEXT: 1232 bytes, 16-byte aligned.
    const CTX_SIZE: usize = 1232;
    const OFF_FLAGS: usize = 0x30;
    const OFF_RSP: usize = 0x98;
    const OFF_RIP: usize = 0xF8;
    // Integer GPR block: Rax(0x78) .. R15(0xF0), 16 contiguous u64 slots.
    const OFF_GPR_LO: usize = 0x78;
    const OFF_GPR_HI: usize = 0xF0;
    // CONTEXT_CONTROL | CONTEXT_INTEGER for AMD64.
    const CONTEXT_CONTROL_INTEGER: u32 = 0x0010_0001 | 0x0010_0002;

    /// Backstop on how far above `Rsp` we scan a peer's stack (matches the
    /// thread-local conservative scanner's `MAX_SCAN_BYTES`).
    const MAX_STACK_SCAN: usize = 8 * 1024 * 1024;

    /// Upper bound (exclusive) of the committed, readable region containing
    /// `rsp`. The used stack grows downward, so `[rsp, end)` is exactly the
    /// committed stack above the current top; beyond `end` is the guard page
    /// or reserved/uncommitted space, which we must not touch.
    unsafe fn committed_region_end(rsp: usize) -> usize {
        let mut mbi = core::mem::MaybeUninit::<MemoryBasicInformation>::uninit();
        let n = VirtualQuery(
            rsp as *const c_void,
            mbi.as_mut_ptr(),
            core::mem::size_of::<MemoryBasicInformation>(),
        );
        if n == 0 {
            return rsp; // query failed — scan nothing on the stack, registers only
        }
        let mbi = mbi.assume_init();
        let readable = mbi.state == MEM_COMMIT
            && mbi.protect & PAGE_NOACCESS == 0
            && mbi.protect & PAGE_GUARD == 0;
        if !readable {
            return rsp;
        }
        let base = mbi.base_address as usize;
        let end = base.saturating_add(mbi.region_size);
        // Clamp to the backstop window above rsp.
        end.min(rsp.saturating_add(MAX_STACK_SCAN))
    }

    /// Conservatively scan a frozen peer's integer registers and used stack,
    /// pushing every value that resolves to a live object onto `roots`.
    unsafe fn scan_context<F>(ctx: &[u8; CTX_SIZE], is_obj: &F, roots: &mut Vec<ObjectRef>) -> usize
    where
        F: Fn(usize) -> Option<ObjectRef>,
    {
        let mut found = 0usize;
        // Integer registers (covers oops that live only in a register — the
        // truncated-r10 SIGSEGV signature).
        let mut off = OFF_GPR_LO;
        while off <= OFF_GPR_HI {
            let v = *(ctx.as_ptr().add(off) as *const u64) as usize;
            if let Some(o) = is_obj(v) {
                roots.push(o);
                found += 1;
            }
            off += 8;
        }
        // Used stack: [rsp, committed-region-end).
        let rsp = *(ctx.as_ptr().add(OFF_RSP) as *const u64) as usize;
        if rsp == 0 || rsp & 0x7 != 0 {
            return found;
        }
        let end = committed_region_end(rsp);
        let mut p = rsp;
        while p + 8 <= end {
            // SAFETY: VirtualQuery confirmed [rsp, end) is committed+readable
            // and the peer is frozen, so the words are stable for this read.
            let w = *(p as *const usize);
            if let Some(o) = is_obj(w) {
                roots.push(o);
                found += 1;
            }
            p += 8;
        }
        found
    }

    /// One enumeration pass: suspend each not-yet-taken peer thread, and for
    /// those whose `Rip` is in JIT code, scan + keep them frozen. Returns the
    /// number of peers newly taken over this pass.
    pub fn take_over_pass<F>(taken: &mut TakenOver, is_obj: &F, roots: &mut Vec<ObjectRef>) -> usize
    where
        F: Fn(usize) -> Option<ObjectRef>,
    {
        let self_tid = unsafe { GetCurrentThreadId() };
        let pid = unsafe { GetCurrentProcessId() };
        // BUG-03 deadlock avoidance: snapshot the JIT code ranges BEFORE
        // suspending any peer. Classifying a frozen peer's Rip via the
        // lock-taking `lookup_jit_code_range` would deadlock if that peer was
        // suspended while holding the code-range lock (mid-registration). The
        // local snapshot lets us classify with a lock-free range check.
        let ranges = crate::jit::jit_code_ranges_snapshot();
        let snap = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0) };
        if snap == -1 || snap == 0 {
            return 0;
        }
        let mut newly = 0usize;
        let mut dbg_suspended = 0usize;
        let mut e: ThreadEntry32 = unsafe { core::mem::zeroed() };
        e.dw_size = core::mem::size_of::<ThreadEntry32>() as u32;
        let mut ok = unsafe { Thread32First(snap, &mut e) };
        while ok != 0 {
            let tid = e.th32_thread_id;
            if e.th32_owner_process_id == pid && tid != self_tid && !taken.contains(tid) {
                dbg_suspended += 1;
                if let Some((kept, found)) = unsafe { try_take(tid, &ranges, is_obj, roots) } {
                    if kept != 0 {
                        taken.handles.push(kept);
                        taken.tids.push(tid);
                        newly += 1;
                        XT_THREADS_TAKEN_OVER.fetch_add(1, Ordering::Relaxed);
                        XT_ROOTS_FOUND.fetch_add(found as u64, Ordering::Relaxed);
                        if dbg() {
                            eprintln!(
                                "[xt-jit-roots] took over tid={tid} (Rip in JIT): {found} conservative roots"
                            );
                        }
                    }
                }
            }
            e.dw_size = core::mem::size_of::<ThreadEntry32>() as u32;
            ok = unsafe { Thread32Next(snap, &mut e) };
        }
        unsafe { CloseHandle(snap) };
        if dbg() {
            eprintln!(
                "[xt-jit-roots] pass: examined {dbg_suspended} peer(s), {newly} newly taken over (Rip in JIT); {} code ranges; any_thread_in_jit={} jit_gate={}",
                ranges.len(),
                crate::jit::conservative_roots::any_thread_in_jit(),
                cratonvm_jit::xt_jit_root_scan_enabled(),
            );
        }
        newly
    }

    /// Suspend `tid`, read its context. If its `Rip` is in a JIT code range,
    /// scan it and return `Some((handle_kept_suspended, roots_found))`. If it
    /// is not in JIT code, resume + close it and return `Some((0, 0))`. On any
    /// OS failure return `None` (nothing held).
    unsafe fn try_take<F>(
        tid: u32,
        ranges: &[(usize, usize)],
        is_obj: &F,
        roots: &mut Vec<ObjectRef>,
    ) -> Option<(isize, usize)>
    where
        F: Fn(usize) -> Option<ObjectRef>,
    {
        let h = OpenThread(
            THREAD_GET_CONTEXT | THREAD_SUSPEND_RESUME | THREAD_QUERY_INFORMATION,
            0,
            tid,
        );
        if h == 0 {
            return None;
        }
        // SuspendThread returns (prev suspend count) or u32::MAX on failure.
        if SuspendThread(h) == u32::MAX {
            CloseHandle(h);
            return None;
        }
        #[repr(C, align(16))]
        struct Ctx([u8; CTX_SIZE]);
        let mut ctx = Ctx([0u8; CTX_SIZE]);
        *(ctx.0.as_mut_ptr().add(OFF_FLAGS) as *mut u32) = CONTEXT_CONTROL_INTEGER;
        if GetThreadContext(h, ctx.0.as_mut_ptr()) == 0 {
            ResumeThread(h);
            CloseHandle(h);
            return None;
        }
        let rip = *(ctx.0.as_ptr().add(OFF_RIP) as *const u64) as usize;
        // Lock-free classification against the pre-suspend snapshot (see
        // `take_over_pass`): never call the lock-taking `lookup_jit_code_range`
        // on a frozen peer.
        let in_jit = ranges.iter().any(|&(e, end)| rip >= e && rip < end);
        if in_jit {
            // Pure JIT instruction stream → holds no VM lock → safe to freeze.
            let found = scan_context(&ctx.0, is_obj, roots);
            Some((h, found)) // keep suspended; caller records the handle
        } else {
            // Interpreter / native / already parked → let it arrive cooperatively.
            ResumeThread(h);
            CloseHandle(h);
            Some((0, 0))
        }
    }

    /// Resume + close every taken-over peer. Called after the collection has
    /// completed and the heap is consistent again.
    pub fn resume(taken: TakenOver) {
        for &h in &taken.handles {
            unsafe {
                ResumeThread(h);
                CloseHandle(h);
            }
        }
        if dbg() && !taken.handles.is_empty() {
            eprintln!("[xt-jit-roots] resumed {} taken-over peer(s)", taken.handles.len());
        }
    }
}

#[cfg(windows)]
pub use imp::{resume, take_over_pass};

// ---------------------------------------------------------------------------
// Non-Windows stubs (the OS-suspend primitive is Windows-only here)
// ---------------------------------------------------------------------------

#[cfg(not(windows))]
pub fn take_over_pass<F>(_taken: &mut TakenOver, _is_obj: &F, _roots: &mut Vec<ObjectRef>) -> usize
where
    F: Fn(usize) -> Option<ObjectRef>,
{
    0
}

#[cfg(not(windows))]
pub fn resume(_taken: TakenOver) {}
