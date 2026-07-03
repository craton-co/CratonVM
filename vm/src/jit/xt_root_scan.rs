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
//! Default ON (opt-out): set `CRATONVM_XT_JIT_ROOT_SCAN=0` to disable. Flipped
//! from default-off after it was validated to suppress the
//! multi-thread-in-JIT-under-STW root gap on the Tomcat suite (see [`enabled`]).

use crate::types::ObjectRef;
use std::sync::atomic::{AtomicU64, Ordering};

/// Number of times a peer thread was taken over (suspended in JIT and
/// conservatively scanned). Exposed for tests / JFR / the validation gate.
pub static XT_THREADS_TAKEN_OVER: AtomicU64 = AtomicU64::new(0);
/// Number of conservative roots contributed by taken-over peers.
pub static XT_ROOTS_FOUND: AtomicU64 = AtomicU64::new(0);
/// A4 (fork6-fjp) — number of helper-window peers scanned: threads whose `Rip`
/// was OUTSIDE JIT code at the STW but whose native stack still held JIT
/// frames (a blocked FJP worker under compiled `runWorker`/`doExec`, or a peer
/// inside a Rust runtime helper called from JIT).
pub static XT_HELPER_WINDOWS_SCANNED: AtomicU64 = AtomicU64::new(0);
/// A4 (fork6-fjp) — conservative roots contributed by helper-window peers.
pub static XT_HELPER_WINDOW_ROOTS: AtomicU64 = AtomicU64::new(0);

/// Whether the cross-thread STW JIT root scan is enabled.
///
/// Default ON (opt-OUT): set `CRATONVM_XT_JIT_ROOT_SCAN=0` (or `false`/`off`)
/// to disable. Flipped to default-on after the cross-thread STW JIT root scan
/// (BUG-03) was validated to suppress the multi-thread-in-JIT-under-STW root
/// gap — e.g. it lets the Tomcat `TestHttpServletDoHead*` (HTTP/2) classes
/// complete instead of dying with the `cross_thread_jit_gap` warning.
#[inline]
pub fn enabled() -> bool {
    static CACHE: AtomicU64 = AtomicU64::new(u64::MAX);
    let c = CACHE.load(Ordering::Relaxed);
    if c != u64::MAX {
        return c == 1;
    }
    // On unless explicitly disabled.
    let on = !matches!(
        std::env::var("CRATONVM_XT_JIT_ROOT_SCAN").as_deref(),
        Ok("0") | Ok("false") | Ok("off")
    );
    CACHE.store(on as u64, Ordering::Relaxed);
    on
}

/// A4 (fork6-fjp) — whether the post-barrier helper-window scan is enabled.
///
/// The takeover pass above only freezes+scans peers whose `Rip` is INSIDE a
/// registered JIT code range. A peer that is *blocked* (or parked) with its
/// `Rip` in Rust/native code can still have live JIT frames on its native
/// stack — e.g. an FJP worker blocked in `join()`/park under JIT-compiled
/// `ForkJoinPool.runWorker`/`ForkJoinTask.doExec` frames that are the sole
/// holders of a forked subtask. Such a thread is excluded from the barrier
/// (`threads_blocked`), and its `deposit_root_snapshot` covers only
/// interpreter frames + native pins — never the JIT band — so the non-moving
/// sweep reclaims the JIT-band oops (the Fork6 stale all-zero receivers).
/// The helper-window pass closes that gap by conservatively scanning the
/// register file + used stack of every remaining peer whose stack contains a
/// JIT return address, once per collection, after the barrier is satisfied.
///
/// Default ON (opt-out): set `CRATONVM_XT_HELPER_WINDOW_SCAN=0` to disable
/// (the A/B kill switch for validating the fix on one binary).
#[inline]
pub fn helper_window_scan_enabled() -> bool {
    static CACHE: AtomicU64 = AtomicU64::new(u64::MAX);
    let c = CACHE.load(Ordering::Relaxed);
    if c != u64::MAX {
        return c == 1;
    }
    let on = !matches!(
        std::env::var("CRATONVM_XT_HELPER_WINDOW_SCAN").as_deref(),
        Ok("0") | Ok("false") | Ok("off")
    );
    CACHE.store(on as u64, Ordering::Relaxed);
    on
}

/// A4 (fork6-fjp) — classify one peer's already-copied stack/register words.
///
/// Returns `true` if any word is a return address into a registered JIT code
/// range (i.e. the peer has JIT frames on its native stack — the
/// helper/blocked window), and pushes every word that resolves to a live
/// object onto `candidates`. The caller commits `candidates` as roots only
/// when the band is classified as a helper window, so a pure-native thread
/// (no JIT frames anywhere on its stack) contributes nothing.
pub(crate) fn classify_helper_window_words<F>(
    words: impl Iterator<Item = usize>,
    ranges: &[(usize, usize)],
    is_obj: &F,
    candidates: &mut Vec<ObjectRef>,
) -> bool
where
    F: Fn(usize) -> Option<ObjectRef>,
{
    let mut has_jit = false;
    for w in words {
        if !has_jit && ranges.iter().any(|&(lo, hi)| w >= lo && w < hi) {
            has_jit = true;
        }
        if let Some(o) = is_obj(w) {
            candidates.push(o);
        }
    }
    has_jit
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
    /// OS tids of the frozen peers, parallel to `handles`. `pub(crate)` so
    /// the initiator's identity-based barrier excusal (xt-hardening
    /// 2026-07-03) can check each newly-frozen tid against the counted-set
    /// snapshot.
    pub(crate) tids: Vec<u32>,
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

    /// A4 (fork6-fjp) — post-barrier helper-window pass: conservatively scan
    /// the register file + used stack of every remaining peer (not the
    /// initiator, not already taken over) whose native stack contains a JIT
    /// return address, appending the discovered roots to `roots`. Returns
    /// `(helper_windows_found, roots_found)`.
    ///
    /// ## When this runs and why it is complete
    ///
    /// Called AFTER the STW barrier is satisfied. At that point every alive
    /// peer is exactly one of:
    ///   * arrived cooperatively — published `update_root_snapshot` (which
    ///     includes `scan_active_jit_frames`) and is parked at the barrier;
    ///   * taken over — frozen in JIT and scanned by `take_over_pass`;
    ///   * BLOCKED (`threads_blocked`-excluded) — covered ONLY by its
    ///     `deposit_root_snapshot`, which never scans the JIT band on its
    ///     native stack. THIS is the gap this pass closes.
    ///
    /// ## Why the reads are sound without keeping the peer frozen for GC
    ///
    /// The peer is suspended only long enough to read its context and copy
    /// its used stack into a pre-allocated local buffer (both allocation-free
    /// operations — a peer suspended mid-`malloc` inside a Rust helper could
    /// otherwise deadlock us on the allocator lock). It is resumed before any
    /// classification/scanning happens. The copied band stays a faithful root
    /// source for this collection because a blocked thread cannot unwind past
    /// its blocked-region leave while the pause is active
    /// (`BlockedGuard::drop` / `mark_blocked_region_leave` wait out the STW
    /// before decrementing), and blocked-region code does not touch the Java
    /// heap — so the JIT frames above the blocking point are stable and no
    /// NEW heap references can appear on that stack during the collection.
    ///
    /// ## Why over-retention is bounded (vs the reverted 2026-06-19b deposit
    /// scan)
    ///
    /// The reverted attempt scanned + pinned the full blocked stack at EVERY
    /// deposit (every blocking native call, program-wide, persisting in the
    /// snapshot). This pass runs once per collection, only when a blocked
    /// thread exists while some thread holds live JIT frames, contributes
    /// roots only for stacks that actually carry a JIT return address, and
    /// the roots live only for the one collection that scanned them.
    pub fn helper_window_pass<F>(
        taken: &TakenOver,
        is_obj: &F,
        roots: &mut Vec<ObjectRef>,
    ) -> (usize, usize)
    where
        F: Fn(usize) -> Option<ObjectRef>,
    {
        let ranges = crate::jit::jit_code_ranges_snapshot();
        if ranges.is_empty() {
            return (0, 0);
        }
        let self_tid = unsafe { GetCurrentThreadId() };
        let pid = unsafe { GetCurrentProcessId() };
        let snap = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0) };
        if snap == -1 || snap == 0 {
            return (0, 0);
        }
        // Reusable copy buffer for each peer's used stack. Pre-sized so the
        // common case never allocates while a peer is frozen; grown (with the
        // peer running) when a band is larger.
        let mut band: Vec<u8> = Vec::with_capacity(256 * 1024);
        let mut candidates: Vec<ObjectRef> = Vec::new();
        let mut windows = 0usize;
        let mut found_total = 0usize;
        let mut e: ThreadEntry32 = unsafe { core::mem::zeroed() };
        e.dw_size = core::mem::size_of::<ThreadEntry32>() as u32;
        let mut ok = unsafe { Thread32First(snap, &mut e) };
        while ok != 0 {
            let tid = e.th32_thread_id;
            if e.th32_owner_process_id == pid && tid != self_tid && !taken.contains(tid) {
                if let Some((ctx, band_len)) = unsafe { snapshot_peer(tid, &mut band) } {
                    candidates.clear();
                    // Integer registers: a callee-saved register can still
                    // hold a JIT-frame oop that no Rust callee spilled.
                    let mut off = OFF_GPR_LO;
                    let mut has_jit = false;
                    while off <= OFF_GPR_HI {
                        // SAFETY: `ctx` is a fully-initialized CONTEXT copy;
                        // read_unaligned because the by-value array is only
                        // byte-aligned.
                        let v = unsafe { (ctx.as_ptr().add(off) as *const u64).read_unaligned() }
                            as usize;
                        if !has_jit && ranges.iter().any(|&(lo, hi)| v >= lo && v < hi) {
                            has_jit = true;
                        }
                        if let Some(o) = is_obj(v) {
                            candidates.push(o);
                        }
                        off += 8;
                    }
                    // SAFETY: `band[..band_len]` was copied while the peer was
                    // frozen; reading the copy is unconditionally safe.
                    let words = (0..band_len / 8).map(|i| unsafe {
                        (band.as_ptr().add(i * 8) as *const usize).read_unaligned()
                    });
                    has_jit |=
                        classify_helper_window_words(words, &ranges, is_obj, &mut candidates);
                    if has_jit {
                        windows += 1;
                        found_total += candidates.len();
                        roots.append(&mut candidates);
                        if dbg() {
                            eprintln!(
                                "[xt-jit-roots] helper-window tid={tid}: JIT frames on native stack (Rip outside JIT), band={band_len}B"
                            );
                        }
                    }
                }
            }
            e.dw_size = core::mem::size_of::<ThreadEntry32>() as u32;
            ok = unsafe { Thread32Next(snap, &mut e) };
        }
        unsafe { CloseHandle(snap) };
        XT_HELPER_WINDOWS_SCANNED.fetch_add(windows as u64, Ordering::Relaxed);
        XT_HELPER_WINDOW_ROOTS.fetch_add(found_total as u64, Ordering::Relaxed);
        if dbg() {
            eprintln!(
                "[xt-jit-roots] helper-window pass: {windows} window(s), {found_total} conservative root(s)"
            );
        }
        (windows, found_total)
    }

    /// Suspend `tid` just long enough to read its thread context and copy its
    /// used stack `[rsp, committed_region_end)` into `band` (growing `band`
    /// only while the peer is RUNNING — never allocate while it is frozen; it
    /// may be suspended mid-`malloc` inside a Rust helper and re-entering the
    /// allocator would deadlock). Returns the captured context and the number
    /// of valid bytes copied into `band`, or `None` on any OS failure.
    unsafe fn snapshot_peer(tid: u32, band: &mut Vec<u8>) -> Option<([u8; CTX_SIZE], usize)> {
        band.clear();
        let h = OpenThread(
            THREAD_GET_CONTEXT | THREAD_SUSPEND_RESUME | THREAD_QUERY_INFORMATION,
            0,
            tid,
        );
        if h == 0 {
            return None;
        }
        #[repr(C, align(16))]
        struct Ctx([u8; CTX_SIZE]);
        let mut ctx = Ctx([0u8; CTX_SIZE]);
        // Up to 4 attempts: each failed attempt resumes the peer, grows the
        // buffer to the band size just observed, and retries (the peer's rsp
        // may move between attempts, so the bounds are recomputed each time).
        for _ in 0..4 {
            if SuspendThread(h) == u32::MAX {
                CloseHandle(h);
                return None;
            }
            ctx.0 = [0u8; CTX_SIZE];
            *(ctx.0.as_mut_ptr().add(OFF_FLAGS) as *mut u32) = CONTEXT_CONTROL_INTEGER;
            if GetThreadContext(h, ctx.0.as_mut_ptr()) == 0 {
                ResumeThread(h);
                CloseHandle(h);
                return None;
            }
            let rsp = *(ctx.0.as_ptr().add(OFF_RSP) as *const u64) as usize;
            if rsp == 0 || rsp & 0x7 != 0 {
                ResumeThread(h);
                CloseHandle(h);
                return None;
            }
            // VirtualQuery is a pure syscall (no user-mode lock) — safe while
            // the peer is frozen.
            let end = committed_region_end(rsp);
            let len = end.saturating_sub(rsp);
            if len == 0 {
                ResumeThread(h);
                CloseHandle(h);
                return Some((ctx.0, 0));
            }
            if len <= band.capacity() {
                // SAFETY: [rsp, end) is committed+readable and the peer is
                // frozen, so the copy reads stable memory; the destination
                // capacity was checked above.
                core::ptr::copy_nonoverlapping(rsp as *const u8, band.as_mut_ptr(), len);
                ResumeThread(h);
                CloseHandle(h);
                // SAFETY: `len` bytes were just initialized above.
                band.set_len(len);
                return Some((ctx.0, len));
            }
            // Buffer too small: resume the peer FIRST, then grow.
            ResumeThread(h);
            band.reserve(len);
        }
        CloseHandle(h);
        None
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
            eprintln!(
                "[xt-jit-roots] resumed {} taken-over peer(s)",
                taken.handles.len()
            );
        }
    }
}

#[cfg(windows)]
pub use imp::{helper_window_pass, resume, take_over_pass};

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

#[cfg(not(windows))]
pub fn helper_window_pass<F>(
    _taken: &TakenOver,
    _is_obj: &F,
    _roots: &mut Vec<ObjectRef>,
) -> (usize, usize)
where
    F: Fn(usize) -> Option<ObjectRef>,
{
    (0, 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fake_obj(addr: usize) -> Option<ObjectRef> {
        // 8-aligned non-null address — the validator contract the real
        // `is_object_address` provides.
        if addr != 0 && addr % 8 == 0 {
            // SAFETY: non-null, 8-aligned; test-only value never dereferenced.
            Some(unsafe { ObjectRef::from_raw(addr as *mut u8) })
        } else {
            None
        }
    }

    /// A stack band with a JIT return address is classified as a helper
    /// window and its object-resolving words become candidate roots.
    #[test]
    fn helper_window_classifier_detects_jit_band_and_collects_candidates() {
        let ranges = [(0x7000_0000usize, 0x7000_4000usize)];
        // Two heap-shaped words; the validator accepts only 0x10_0008.
        let obj = 0x10_0008usize;
        let words = [
            0usize,
            0x7000_0123, // return address into the JIT range
            obj,
            0xdead_beef,
        ];
        let is_obj = |a: usize| if a == obj { fake_obj(a) } else { None };
        let mut candidates = Vec::new();
        let has_jit =
            classify_helper_window_words(words.iter().copied(), &ranges, &is_obj, &mut candidates);
        assert!(
            has_jit,
            "JIT return address on the band must classify as a helper window"
        );
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].as_ptr() as usize, obj);
    }

    /// A pure-native stack (no JIT return address anywhere) is NOT a helper
    /// window — the caller must discard its candidates (no blanket
    /// over-retention for threads that never ran JIT code).
    #[test]
    fn helper_window_classifier_rejects_band_without_jit_frames() {
        let ranges = [(0x7000_0000usize, 0x7000_4000usize)];
        let obj = 0x10_0008usize;
        let words = [obj, 0x12345usize, 0usize];
        let is_obj = |a: usize| if a == obj { fake_obj(a) } else { None };
        let mut candidates = Vec::new();
        let has_jit =
            classify_helper_window_words(words.iter().copied(), &ranges, &is_obj, &mut candidates);
        assert!(!has_jit);
        // Candidates were still collected — the caller's `has_jit` gate is
        // what discards them.
        assert_eq!(candidates.len(), 1);
    }

    /// Empty range set never classifies anything (JIT never emitted code).
    #[test]
    fn helper_window_classifier_empty_ranges() {
        let words = [0x7000_0123usize];
        let mut candidates = Vec::new();
        let has_jit =
            classify_helper_window_words(words.iter().copied(), &[], &|_| None, &mut candidates);
        assert!(!has_jit);
        assert!(candidates.is_empty());
    }
}
