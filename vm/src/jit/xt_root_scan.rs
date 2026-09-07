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
//!   * The conservative roots contributed here are never relocated, so an
//!     interior / false-positive pointer can never be mis-rewritten.
//!
//!     arch-2026-07-26 (`moving-young-precise-roots`): this used to be stated
//!     as a *consequence* — "a frozen in-JIT peer keeps its JIT-entry guard
//!     live, so `gc_quiescence::is_active()` stays true for the whole
//!     collection and the heap performs a non-moving sweep". That inference
//!     held only while `is_active()` unconditionally forced the non-moving
//!     sweep. Under a moving young generation it does not: `is_active()` is
//!     precisely the condition moving-young is designed to run *through*. Each
//!     pass below therefore asserts the obligation itself, calling
//!     `gc_quiescence::mark_moving_young_coverage_incomplete_because` whenever
//!     it actually contributes a peer's conservative roots — this collection
//!     cannot rewrite a frozen peer's registers, so it must not move.
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

/// Helper windows DISCHARGED by pinning the peer's conservative roots, and
/// those that still refused the collection.
///
/// The pair is the point: `pinned` alone cannot say whether the refusal is
/// gone, and `refused` alone cannot say whether the pass ever ran. Zero in both
/// means no peer was caught inside a helper.
pub static XT_HELPER_WINDOWS_PINNED: AtomicU64 = AtomicU64::new(0);
pub static XT_HELPER_WINDOWS_REFUSED: AtomicU64 = AtomicU64::new(0);

/// Helper windows THIS CYCLE that could not be pinned (a partial scan, or the
/// pin switch off). Reset at the start of every pass.
///
/// Per-cycle, unlike the two lifetime totals above, because the discharge
/// decision is per-cycle: a collection may relocate only if EVERY window it saw
/// is covered. The Windows arm never pins, so it leaves this nonzero and never
/// discharges.
pub static XT_HELPER_WINDOWS_UNPINNED_CYCLE: AtomicU64 = AtomicU64::new(0);

/// May this cycle's helper windows be discharged instead of refusing?
///
/// True only when the pass pinned every window it saw. Read by
/// `interpreter::gc_and_alloc`, which raises the SECOND (unlabelled) refusal.
pub fn helper_windows_all_pinned_this_cycle() -> bool {
    XT_HELPER_WINDOWS_UNPINNED_CYCLE.load(Ordering::Acquire) == 0
}

/// `CRATONVM_XT_HELPER_WINDOW_DISCHARGE=1` -- let a fully-pinned helper window
/// stop refusing the collection. **Default OFF.**
///
/// Off by default because it is a behaviour change on the relocation gate and
/// the first attempt at it (722de9a33) was wrong in two ways at once: it
/// discharged only the LABELLED refusal, leaving the unlabelled one in
/// `interpreter::gc_and_alloc` to refuse anyway, and it pinned a root set that
/// could not be complete because the probe was `is_object_address` (exact bases
/// only), so a peer's derived pointer left its base unpinned.
///
/// Both are addressed here: this flag implies the interior-resolving probe, and
/// it gates BOTH sites off the same per-cycle condition. The widening that
/// implies was measured at **+25 % conservative roots per window** on
/// `TestMultiThread` (111 -> 139), which is what makes it affordable.
///
/// Turn it on with `CRATONVM_GC_STATS=1` and read `relocation_on_proven_jit`:
/// a zero still voids the run.
/// Engagement census for [`scan_peer_shadow_window`].
///
/// Without these a clean result cannot be told apart from a scan that never
/// ran: a peer whose shadow stack is legitimately empty and a peer whose window
/// was never read both contribute zero roots. `WINDOWS` is the denominator,
/// `SLOTS` says whether the windows had anything in them, and `UNTRUSTED`
/// counts the windows that refused the pin rather than claim coverage.
pub static XT_PEER_SHADOW_WINDOWS: AtomicU64 = AtomicU64::new(0);
pub static XT_PEER_SHADOW_SLOTS: AtomicU64 = AtomicU64::new(0);
pub static XT_PEER_SHADOW_ROOTS: AtomicU64 = AtomicU64::new(0);
pub static XT_PEER_SHADOW_UNTRUSTED: AtomicU64 = AtomicU64::new(0);

/// Scan a frozen blocked peer's SHADOW STACK, appending every heap address it
/// names to `out`.
///
/// The window is `[base, top)` of the `ShadowStack` the peer published the
/// address of (`gc_quiescence::publish_self_shadow_addr`). Reading it is sound
/// only for a peer that cannot run: `mark_blocked_region_leave` waits out an
/// active pause, so a blocked peer's `top` is stable for the whole STW.
///
/// Every field is validated before use. The address came from another thread
/// and a stale or torn one would be dereferenced here -- the same shape of
/// mistake that SIGSEGV'd the band verifier on a `base` of `0x5555_0000_0004`.
/// Returns the number of slots scanned, or `None` if the window could not be
/// trusted (which must keep the cycle refusing rather than claim coverage).
pub fn scan_peer_shadow_window<F>(os_tid: u32, is_obj: &F, out: &mut Vec<ObjectRef>) -> Option<usize>
where
    F: Fn(usize) -> Option<ObjectRef>,
{
    let Some((ss, pub_base, pub_end)) = cratonvm_gc::gc_quiescence::shadow_window_of_tid(os_tid)
    else {
        // The peer never published. That is UNKNOWN coverage, not empty
        // coverage, so it must refuse.
        XT_PEER_SHADOW_UNTRUSTED.fetch_add(1, Ordering::Relaxed);
        return None;
    };
    if ss == 0 || ss & 0x7 != 0 {
        XT_PEER_SHADOW_UNTRUSTED.fetch_add(1, Ordering::Relaxed);
        return None;
    }
    // SAFETY: `ss` is the `#[repr(C)] ShadowStack` address the owning thread
    // published. The thread is in `blocked_os_tids` (alive), it is blocked so
    // it is not mutating, its `JvmThread` cannot have moved while it holds live
    // compiled frames (the JIT caches `*mut JvmThread` per frame and reaches
    // the shadow stack through it), and the entry is removed when the thread
    // exits. Fields are `top`, `end`, `base` at 0, 8, 16 -- asserted by
    // `layout_offsets_match_jit_contract`.
    //
    // RESIDUAL, stated plainly: this read happens BEFORE the identity check
    // below can reject a stale address, so a thread that died between the
    // blocked-tid snapshot and here would be read after free. The window is
    // narrow and the same shape the existing `shadow_window_from_frame` lives
    // with; the identity check is what stops a stale read from being ACTED on.
    let (top, end, base) = unsafe {
        let p = ss as *const usize;
        (p.read(), p.add(1).read(), p.add(2).read())
    };
    // IDENTITY CHECK, and the reason reading `ss` is defensible: the struct's
    // own `base`/`end` must be exactly what this thread published. They point
    // into the heap `Box` and never change after `ensure_allocated`, so a
    // struct that moved (or was freed) does not match, and a stale address is
    // rejected instead of dereferenced further.
    if base != pub_base || end != pub_end {
        XT_PEER_SHADOW_UNTRUSTED.fetch_add(1, Ordering::Relaxed);
        return None;
    }
    // The `#[repr(C)]` invariant. `top` is the only field compiled code writes,
    // so it is the only one that still needs checking.
    if top & 0x7 != 0 || top < base || top > end {
        XT_PEER_SHADOW_UNTRUSTED.fetch_add(1, Ordering::Relaxed);
        return None;
    }
    let span = top - base;
    let slots = span / 8;
    XT_PEER_SHADOW_WINDOWS.fetch_add(1, Ordering::Relaxed);
    XT_PEER_SHADOW_SLOTS.fetch_add(slots as u64, Ordering::Relaxed);
    let found_before = out.len();
    for i in 0..slots {
        // SAFETY: `[base, top)` is inside the validated window, which the
        // blocked peer is not mutating.
        let v = unsafe { ((base + i * 8) as *const usize).read() };
        if let Some(o) = is_obj(v) {
            out.push(o);
        }
    }
    XT_PEER_SHADOW_ROOTS.fetch_add((out.len() - found_before) as u64, Ordering::Relaxed);
    Some(slots)
}

/// `CRATONVM_XT_HELPER_WINDOW_PIN_RESOLVE=1` -- resolve a frozen peer's words
/// with `resolve_interior_for_pin` rather than `is_heap_addr`.
///
/// The difference is the two cases `is_heap_addr` drops and a frozen peer's
/// registers hold: a MISALIGNED interior pointer and a ONE-PAST-THE-END cursor.
/// Both leave an object unpinned, and relocation then moves it out from under
/// the register that names it.
pub fn helper_window_pin_resolve_enabled() -> bool {
    cratonvm_types::flags::runtime_var_os("CRATONVM_XT_HELPER_WINDOW_PIN_RESOLVE").is_some()
}

pub fn helper_window_discharge_enabled() -> bool {
    // DEFAULT ON since 2026-09-04. A helper window whose peer is completely
    // pinned -- register file, whole `[rsp, stack_base)` band, and the peer's
    // shadow stack -- no longer refuses the collection.
    //
    // Measured on `org.h2.test.jdbc.TestCachedQueryResults` with the arena
    // commit fix in: 5 runs, 0 SIGSEGV, ZERO ref-array OOM, 99953-99978,
    // completing in 555-728 s, compaction intact at 25 cycles / 545893 objects.
    // Against 98304 with 1497 OOMs in ~1519 s before. Regression suite 88/88.
    //
    // `CRATONVM_XT_HELPER_WINDOW_DISCHARGE=0` is the kill switch: it restores
    // the blanket refusal, which costs ~6264 OOMs on that class and does not
    // complete.
    !matches!(
        cratonvm_types::flags::runtime_var("CRATONVM_XT_HELPER_WINDOW_DISCHARGE").as_deref(),
        Ok("0") | Ok("false") | Ok("off") | Ok("no")
    )
}

/// Peers the STW cross-thread scan could NOT classify: it signalled them and
/// they did not reach the handler before the deadline (`STATE_CANCELLED`), or
/// no slot was free to arm. Such a peer is neither parked nor proven
/// interpreter-side, so its JIT-frame oops are absent from the root set while
/// it KEEPS RUNNING — and the non-moving sweep then frees on `GC_FLAG_MARKED`
/// alone. This is the counter that says whether that happened.
pub static XT_PEERS_UNCLASSIFIED: AtomicU64 = AtomicU64::new(0);
/// Collections during which at least one peer went unclassified.
pub static XT_CYCLES_WITH_UNCLASSIFIED: AtomicU64 = AtomicU64::new(0);

/// H2-CID0 (2026-08-05) — takeover signals re-sent because a peer had not
/// reached the handler within one per-attempt deadline.
///
/// Non-zero here is the normal, healthy state on a loaded box; it is the
/// counter that says the retry is doing work rather than being decorative.
pub static XT_PEER_RESIGNALS: AtomicU64 = AtomicU64::new(0);
/// H2-CID0 (2026-08-05) — peers that answered only AFTER a re-signal.
///
/// Every one of these would have been an UNCLASSIFIED peer before the retry
/// landed: a running thread whose JIT-frame oops were absent from the mark set
/// while the non-moving sweep freed on `GC_FLAG_MARKED` alone.
pub static XT_PEERS_CLASSIFIED_AFTER_RETRY: AtomicU64 = AtomicU64::new(0);

/// Deadline for a signalled peer to reach the takeover handler.
///
/// `CRATONVM_XT_PEER_DEADLINE_MS` (default 20). The default is a scheduling
/// bet: a runnable peer under heavy CPU contention can simply not be
/// scheduled within it.
pub fn peer_deadline_ms() -> u64 {
    use std::sync::OnceLock;
    static G: OnceLock<u64> = OnceLock::new();
    *G.get_or_init(|| {
        cratonvm_types::flags::runtime_var("CRATONVM_XT_PEER_DEADLINE_MS")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .filter(|v| *v > 0)
            .unwrap_or(20)
    })
}

/// H2-CID0 (2026-08-05) — TOTAL time a peer gets to answer, across re-signals.
///
/// [`peer_deadline_ms`] is one scheduling bet. Losing it means the peer was not
/// placed on a CPU in that window, which on a loaded host says nothing about
/// whether it would ever answer — and the cost of concluding "unclassified" is
/// that the non-moving sweep marks from a root set provably missing a RUNNING
/// thread's JIT frames, then frees on `GC_FLAG_MARKED` alone. Measured 16 such
/// peers across 15 cycles on the H2 `ClassId(0)` reproducer.
///
/// The takeover is signal-based, so a peer need not reach a safepoint to
/// answer; it only needs to be scheduled. Retrying therefore converges for any
/// peer that is genuinely running, which is exactly the population that matters
/// here. `CRATONVM_XT_PEER_TOTAL_MS` (default 1000); set it equal to
/// `CRATONVM_XT_PEER_DEADLINE_MS` to restore the old one-shot behaviour.
pub fn peer_total_deadline_ms() -> u64 {
    use std::sync::OnceLock;
    static G: OnceLock<u64> = OnceLock::new();
    *G.get_or_init(|| {
        let total = cratonvm_types::flags::runtime_var("CRATONVM_XT_PEER_TOTAL_MS")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .filter(|v| *v > 0)
            .unwrap_or(1000);
        // A total below one attempt would silently shorten the per-attempt
        // deadline instead of extending it.
        total.max(peer_deadline_ms())
    })
}

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
        cratonvm_types::flags::runtime_var("CRATONVM_XT_JIT_ROOT_SCAN").as_deref(),
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
        cratonvm_types::flags::runtime_var("CRATONVM_XT_HELPER_WINDOW_SCAN").as_deref(),
        Ok("0") | Ok("false") | Ok("off")
    );
    CACHE.store(on as u64, Ordering::Relaxed);
    on
}

/// `CRATONVM_XT_HELPER_WINDOW_PIN=0` -- stop publishing a frozen helper-window
/// peer's conservative roots as pins.
///
/// Default ON, and it does NOT discharge `incomplete_reason::XT_HELPER_WINDOW`;
/// see the note at that refusal. The pins are additive protection and a
/// measurement: on `org.h2.test.jdbc.TestCachedQueryResults` the helper-window
/// refusal is **219 of 227**, so `hw_pinned` is the size of what a future
/// discharge has to cover.
///
/// The scan itself is complete in what it READS -- the published register file
/// and every readable word from `rsp` up -- and `classify_slot_helper_window`
/// reports `complete` so a partial one is never pinned. What it cannot do is
/// resolve a DERIVED pointer to its base, because it probes with
/// `is_object_address` (exact bases). That is the gap that keeps the refusal.
fn helper_window_pin_enabled() -> bool {
    !matches!(
        cratonvm_types::flags::runtime_var("CRATONVM_XT_HELPER_WINDOW_PIN").as_deref(),
        Ok("0") | Ok("false") | Ok("off") | Ok("no")
    )
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
    cratonvm_types::flags::runtime_var_os("CRATONVM_DBG_XT_JIT_ROOT_SCAN").is_some()
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
    unsafe fn scan_context<F>(
        ctx: &[u8; CTX_SIZE],
        is_obj: &F,
        roots: &mut Vec<ObjectRef>,
        os_tid: u32,
    ) -> usize
    where
        F: Fn(usize) -> Option<ObjectRef>,
    {
        let mut found = 0usize;
        // Integer registers (covers oops that live only in a register — the
        // truncated-r10 SIGSEGV signature).
        let mut off = OFF_GPR_LO;
        while off <= OFF_GPR_HI {
            let v = *(ctx.as_ptr().add(off) as *const u64) as usize;
            // Pairing capture: EVERY word, before any root filter. These
            // registers are not heap and not frame-band memory, and a Cheney
            // copy never rewrites them, so if one names an object this cycle
            // relocates, the peer resumes holding a vacated address.
            cratonvm_gc::gc_quiescence::record_peer_reg(
                os_tid,
                ((off - OFF_GPR_LO) / 8) as u8,
                v,
            );
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
                // Pairing capture, STACK side -- the JIT spill slots the
                // `CompiledUninterruptible` comment names alongside registers.
                // Gated on `is_obj` rather than taking every word: a
                // `pointer_map` key is by construction an object this cycle
                // relocated, so the collector's own object test accepts it,
                // and the alternative is millions of words per peer. Register
                // 0xff marks the stack side.
                cratonvm_gc::gc_quiescence::record_peer_reg(os_tid, 0xff, w);
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
                        // The roots we just contributed came from a FROZEN
                        // peer's register file and raw stack — conservatively
                        // discovered and, critically, not rewritable: this
                        // collection never applies its pointer map to a frozen
                        // peer (it is excused from the barrier and resumes
                        // straight back into compiled code). The module's
                        // soundness argument used to lean on "a frozen in-JIT
                        // peer keeps its JIT-entry guard live, so `is_active()`
                        // stays true and the heap performs a non-moving sweep" —
                        // which stopped being automatic the moment moving-young
                        // could run under `is_active()`. State the obligation
                        // directly instead of inheriting it. (Also asserted at
                        // the call site; kept here so any future caller of this
                        // pass inherits the guarantee.)
                        cratonvm_gc::gc_quiescence::mark_moving_young_coverage_incomplete_because(
                            cratonvm_gc::gc_quiescence::incomplete_reason::XT_TAKEOVER,
                        );
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
            let found = scan_context(&ctx.0, is_obj, roots, tid);
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
        blocked_os_tids: &[u32],
    ) -> (usize, usize)
    where
        F: Fn(usize) -> Option<ObjectRef>,
    {
        let ranges = crate::jit::jit_code_ranges_snapshot();
        if ranges.is_empty() || blocked_os_tids.is_empty() {
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
        let mut pinned_windows = 0usize;
        let mut unpinned_windows = 0usize;
        // Per-cycle, so reset before the pass rather than accumulated.
        super::XT_HELPER_WINDOWS_UNPINNED_CYCLE.store(0, Ordering::Release);
        let mut found_total = 0usize;
        let mut e: ThreadEntry32 = unsafe { core::mem::zeroed() };
        e.dw_size = core::mem::size_of::<ThreadEntry32>() as u32;
        let mut ok = unsafe { Thread32First(snap, &mut e) };
        while ok != 0 {
            let tid = e.th32_thread_id;
            // xt-hardening follow-up (2026-07-03): this pass exists ONLY to
            // close the BLOCKED-thread coverage gap (deposit_root_snapshot
            // never scans the JIT band) — a cooperatively-arrived mutator
            // already published its JIT roots via update_root_snapshot
            // before parking at the barrier. Scanning it again is pure
            // redundant over-retention risk (a false-positive conservative
            // candidate can only ever help correctness for a REAL gap;
            // widening the candidate volume for threads that need no help
            // just adds corruption surface). Skip any peer not in the
            // blocked-tid snapshot — this also skips the suspend/resume
            // round-trip entirely for the (large majority) non-blocked case.
            if e.th32_owner_process_id == pid
                && tid != self_tid
                && !taken.contains(tid)
                && blocked_os_tids.contains(&tid)
            {
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
                        // Pairing capture -- see the take-over path.
                        cratonvm_gc::gc_quiescence::record_peer_reg(
                            tid,
                            ((off - OFF_GPR_LO) / 8) as u8,
                            v,
                        );
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
                    // Pairing capture, helper-window stack side. Same gate and
                    // same 0xff marker as the take-over path.
                    if cratonvm_gc::gc_quiescence::peer_reg_pairing_enabled() {
                        for i in 0..band_len / 8 {
                            let w = unsafe {
                                (band.as_ptr().add(i * 8) as *const usize).read_unaligned()
                            };
                            if is_obj(w).is_some() {
                                cratonvm_gc::gc_quiescence::record_peer_reg(tid, 0xff, w);
                            }
                        }
                    }
                    has_jit |=
                        classify_helper_window_words(words, &ranges, is_obj, &mut candidates);
                    if has_jit {
                        windows += 1;
                        // A JIT frame's oops live in the SHADOW STACK, which
                        // is not the machine stack and so is invisible to
                        // everything above. Scan it too, or the pin is
                        // incomplete and any coverage credited on it is a lie.
                        // An untrusted window refuses the pin rather than
                        // claiming coverage it does not have.
                        let shadow_ok = if crate::jit::conservative_roots::xt_peer_shadow_scan_enabled()
                        {
                            super::scan_peer_shadow_window(tid, is_obj, &mut candidates).is_some()
                        } else {
                            true
                        };
                        found_total += candidates.len();
                        // PIN, exactly as the Linux arm does. A window is only
                        // COUNTED here when `snapshot_peer` returned `Some`,
                        // and that means the whole GPR range and the whole
                        // band `[rsp, committed_region_end)` were captured --
                        // so a counted window is complete by construction and
                        // needs no separate `complete` flag.
                        if super::helper_window_pin_enabled() && shadow_ok {
                            let addrs: Vec<usize> =
                                candidates.iter().map(|o| o.as_ptr() as usize).collect();
                            cratonvm_gc::gc_quiescence::add_xt_cycle_pinned_jit_roots(&addrs);
                            pinned_windows += 1;
                            // A pinned window covers the peer's WHOLE stack
                            // (`[rsp, stack_base)`) plus its register file, so
                            // every JIT frame it holds is immobile -- which is
                            // what the cross-thread coverage account wants to
                            // hear, and it wants to hear it as a DEPTH.
                            //
                            // Reading the peer's published depth after it has
                            // resumed is still exact: `mark_blocked_region_leave`
                            // waits out an active pause, so a blocked peer
                            // cannot run Java (and so cannot mutate its chain)
                            // between the block and the end of this STW.
                            //
                            // `None` -- a peer that never registered a slot --
                            // poisons the ledger rather than crediting zero.
                            if crate::jit::conservative_roots::xt_pinned_peer_depth_enabled() {
                                cratonvm_gc::gc_quiescence::add_xt_cycle_pinned_jit_depth(
                                    cratonvm_gc::gc_quiescence::jit_depth_of_tid(tid),
                                );
                            }
                        } else {
                            unpinned_windows += 1;
                        }
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
        super::XT_HELPER_WINDOWS_PINNED.fetch_add(pinned_windows as u64, Ordering::Relaxed);
        super::XT_HELPER_WINDOWS_REFUSED.fetch_add(unpinned_windows as u64, Ordering::Relaxed);
        super::XT_HELPER_WINDOWS_UNPINNED_CYCLE.store(unpinned_windows as u64, Ordering::Release);
        // Helper-window roots are a blocked peer's register file + raw stack:
        // conservative and un-rewritable, so without a pin this collection must
        // not relocate. WITH one -- and with the interior-resolving probe the
        // discharge implies, so a derived pointer resolves to the base that
        // must stay still -- the objects the peer can reach are held in place
        // and the rest of the heap may move. See the Linux arm for the full
        // argument and for why the pins alone are not sufficient without the
        // second site in `interpreter::gc_and_alloc` agreeing.
        if windows > 0
            && !(super::helper_window_discharge_enabled() && unpinned_windows == 0)
        {
            cratonvm_gc::gc_quiescence::mark_moving_young_coverage_incomplete_because(
                cratonvm_gc::gc_quiescence::incomplete_reason::XT_HELPER_WINDOW,
            );
        }
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
// Linux x86-64 implementation (signal rendezvous)
// ---------------------------------------------------------------------------

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
mod imp {
    use super::*;
    use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU8, AtomicUsize, Ordering};
    use std::sync::Once;
    use std::time::{Duration, Instant};

    const TAKEOVER_SIGNAL: i32 = libc::SIGUSR2;
    const MAX_SLOTS: usize = 512;
    const MAX_RANGES: usize = 65_536;
    const MAX_STACK_SCAN: usize = 8 * 1024 * 1024;
    const REG_COUNT: usize = 17;

    use super::{
        peer_deadline_ms, peer_total_deadline_ms, XT_CYCLES_WITH_UNCLASSIFIED,
        XT_PEERS_CLASSIFIED_AFTER_RETRY, XT_PEERS_UNCLASSIFIED, XT_PEER_RESIGNALS,
    };

    const STATE_EMPTY: u8 = 0;
    const STATE_ARMED: u8 = 1;
    const STATE_PARKED: u8 = 2;
    const STATE_NOT_JIT: u8 = 3;
    const STATE_DONE: u8 = 4;
    const STATE_CANCELLED: u8 = 5;

    struct LinuxSlot {
        tid: AtomicU32,
        state: AtomicU8,
        resume: AtomicU8,
        rip: AtomicUsize,
        rsp: AtomicUsize,
        regs: [AtomicUsize; REG_COUNT],
    }

    impl LinuxSlot {
        const fn new() -> Self {
            Self {
                tid: AtomicU32::new(0),
                state: AtomicU8::new(STATE_EMPTY),
                resume: AtomicU8::new(0),
                rip: AtomicUsize::new(0),
                rsp: AtomicUsize::new(0),
                regs: [const { AtomicUsize::new(0) }; REG_COUNT],
            }
        }

        fn arm(&self, tid: u32) {
            self.tid.store(tid, Ordering::Release);
            self.resume.store(0, Ordering::Release);
            self.rip.store(0, Ordering::Release);
            self.rsp.store(0, Ordering::Release);
            for reg in &self.regs {
                reg.store(0, Ordering::Release);
            }
            self.state.store(STATE_ARMED, Ordering::Release);
        }

        fn clear(&self) {
            self.resume.store(1, Ordering::Release);
            self.rip.store(0, Ordering::Release);
            self.rsp.store(0, Ordering::Release);
            self.tid.store(0, Ordering::Release);
            self.state.store(STATE_EMPTY, Ordering::Release);
        }
    }

    struct AtomicRange {
        lo: AtomicUsize,
        hi: AtomicUsize,
    }

    impl AtomicRange {
        const fn new() -> Self {
            Self {
                lo: AtomicUsize::new(0),
                hi: AtomicUsize::new(0),
            }
        }
    }

    static INSTALL: Once = Once::new();
    static ACTIVE: AtomicBool = AtomicBool::new(false);
    static HELPER_MODE: AtomicBool = AtomicBool::new(false);
    static SLOTS: [LinuxSlot; MAX_SLOTS] = [const { LinuxSlot::new() }; MAX_SLOTS];
    static RANGES: [AtomicRange; MAX_RANGES] = [const { AtomicRange::new() }; MAX_RANGES];
    static RANGES_LEN: AtomicUsize = AtomicUsize::new(0);

    #[inline]
    fn gettid() -> u32 {
        unsafe { libc::syscall(libc::SYS_gettid) as u32 }
    }

    #[inline]
    fn publish_ranges(ranges: &[(usize, usize)]) {
        let n = ranges.len().min(MAX_RANGES);
        for (i, &(lo, hi)) in ranges.iter().take(n).enumerate() {
            RANGES[i].lo.store(lo, Ordering::Release);
            RANGES[i].hi.store(hi, Ordering::Release);
        }
        RANGES_LEN.store(n, Ordering::Release);
        if dbg() && ranges.len() > MAX_RANGES {
            eprintln!(
                "[xt-jit-roots] linux range table capped: {} -> {} ranges",
                ranges.len(),
                MAX_RANGES
            );
        }
    }

    #[inline]
    fn find_slot(tid: u32) -> Option<&'static LinuxSlot> {
        SLOTS.iter().find(|slot| {
            slot.tid.load(Ordering::Acquire) == tid
                && slot.state.load(Ordering::Acquire) != STATE_EMPTY
        })
    }

    fn arm_slot(tid: u32) -> Option<&'static LinuxSlot> {
        for slot in &SLOTS {
            if slot.state.load(Ordering::Acquire) == STATE_EMPTY {
                slot.arm(tid);
                return Some(slot);
            }
        }
        None
    }

    unsafe extern "C" fn takeover_signal_handler(
        _sig: i32,
        _info: *mut libc::siginfo_t,
        ucontext: *mut libc::c_void,
    ) {
        if !ACTIVE.load(Ordering::Acquire) || ucontext.is_null() {
            return;
        }
        let tid = gettid();
        let Some(slot) = find_slot(tid) else {
            return;
        };
        if slot.state.load(Ordering::Acquire) != STATE_ARMED {
            return;
        }

        let uc = &*(ucontext as *const libc::ucontext_t);
        let g = &uc.uc_mcontext.gregs;
        let read_reg = |idx: i32| -> usize { g[idx as usize] as usize };
        let rip = read_reg(libc::REG_RIP);
        let rsp = read_reg(libc::REG_RSP);

        let mut in_jit = false;
        let len = RANGES_LEN.load(Ordering::Acquire);
        let mut i = 0usize;
        while i < len {
            let lo = RANGES[i].lo.load(Ordering::Acquire);
            let hi = RANGES[i].hi.load(Ordering::Acquire);
            if rip >= lo && rip < hi {
                in_jit = true;
                break;
            }
            i += 1;
        }
        if !in_jit && !HELPER_MODE.load(Ordering::Acquire) {
            slot.state.store(STATE_NOT_JIT, Ordering::Release);
            return;
        }

        let regs = [
            libc::REG_R8,
            libc::REG_R9,
            libc::REG_R10,
            libc::REG_R11,
            libc::REG_R12,
            libc::REG_R13,
            libc::REG_R14,
            libc::REG_R15,
            libc::REG_RDI,
            libc::REG_RSI,
            libc::REG_RBP,
            libc::REG_RBX,
            libc::REG_RDX,
            libc::REG_RAX,
            libc::REG_RCX,
            libc::REG_RSP,
            libc::REG_RIP,
        ];
        slot.rip.store(rip, Ordering::Release);
        slot.rsp.store(rsp, Ordering::Release);
        for (i, reg) in regs.iter().enumerate() {
            slot.regs[i].store(read_reg(*reg), Ordering::Release);
        }
        slot.state.store(STATE_PARKED, Ordering::Release);

        while slot.resume.load(Ordering::Acquire) == 0 && ACTIVE.load(Ordering::Acquire) {
            core::hint::spin_loop();
        }
        slot.state.store(STATE_DONE, Ordering::Release);
    }

    fn install_handler() {
        INSTALL.call_once(|| unsafe {
            let mut sa: libc::sigaction = std::mem::zeroed();
            sa.sa_flags = libc::SA_SIGINFO | libc::SA_RESTART;
            sa.sa_sigaction = takeover_signal_handler as usize;
            libc::sigemptyset(&mut sa.sa_mask);
            let rc = libc::sigaction(TAKEOVER_SIGNAL, &sa, std::ptr::null_mut());
            if rc != 0 {
                eprintln!(
                    "[xt-jit-roots] failed to install linux takeover signal handler: errno={}",
                    *libc::__errno_location()
                );
            }
        });
    }

    fn list_thread_tids() -> Vec<u32> {
        let mut tids = Vec::new();
        let Ok(entries) = std::fs::read_dir("/proc/self/task") else {
            return tids;
        };
        for entry in entries.flatten() {
            let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
                continue;
            };
            if let Ok(tid) = name.parse::<u32>() {
                tids.push(tid);
            }
        }
        tids
    }

    fn send_takeover_signal(tid: u32) -> bool {
        let rc = unsafe {
            libc::syscall(
                libc::SYS_tgkill,
                libc::getpid(),
                tid as libc::pid_t,
                TAKEOVER_SIGNAL,
            )
        };
        rc == 0
    }

    /// Poll for at most `timeout`, WITHOUT cancelling when it expires.
    ///
    /// Split out of `wait_for_response` so an expired attempt can be retried:
    /// cancelling is a decision about the whole budget, not about one attempt,
    /// and `STATE_CANCELLED` is the state the handler reads to bail out.
    fn poll_for_response(slot: &LinuxSlot, timeout: Duration) -> Option<u8> {
        let start = Instant::now();
        loop {
            let state = slot.state.load(Ordering::Acquire);
            if state != STATE_ARMED {
                return Some(state);
            }
            if start.elapsed() >= timeout {
                return None;
            }
            std::thread::yield_now();
        }
    }

    /// H2-CID0 (2026-08-05) — wait for a peer across re-signals.
    ///
    /// Replaces a single [`peer_deadline_ms`] bet. Missing that deadline means
    /// the peer was not scheduled inside the window; because the takeover is
    /// signal-based rather than poll-based, that is a statement about the
    /// scheduler and not about whether the peer can answer. Concluding
    /// "unclassified" instead costs a root set that provably omits a RUNNING
    /// thread's JIT-frame oops, which the non-moving sweep then frees on
    /// `GC_FLAG_MARKED` alone.
    ///
    /// Re-signalling an already-`STATE_ARMED` slot is safe and idempotent: the
    /// handler re-checks `ACTIVE`, re-finds the slot and early-returns unless
    /// the state is still `STATE_ARMED`, so a redundant delivery (including one
    /// racing with the peer parking) does no work.
    ///
    /// Returns `STATE_EMPTY` if the peer exited mid-wait — nothing to scan and
    /// nothing running, which is not a coverage hole and must not be counted as
    /// one.
    fn wait_for_response_retrying(slot: &LinuxSlot, tid: u32) -> u8 {
        let attempt = Duration::from_millis(peer_deadline_ms());
        let budget = Duration::from_millis(peer_total_deadline_ms());
        let start = Instant::now();
        loop {
            if let Some(state) = poll_for_response(slot, attempt) {
                return state;
            }
            if start.elapsed() >= budget {
                // Out of budget. This IS a coverage hole; the caller counts it.
                slot.resume.store(1, Ordering::Release);
                slot.state.store(STATE_CANCELLED, Ordering::Release);
                return STATE_CANCELLED;
            }
            if !send_takeover_signal(tid) {
                // ESRCH: exited while we waited.
                slot.resume.store(1, Ordering::Release);
                slot.state.store(STATE_CANCELLED, Ordering::Release);
                slot.clear();
                return STATE_EMPTY;
            }
            XT_PEER_RESIGNALS.fetch_add(1, Ordering::Relaxed);
        }
    }

    fn readable_regions() -> Vec<(usize, usize)> {
        let mut regions = Vec::new();
        let Ok(maps) = std::fs::read_to_string("/proc/self/maps") else {
            return regions;
        };
        for line in maps.lines() {
            let mut parts = line.split_whitespace();
            let Some(range) = parts.next() else { continue };
            let Some(perms) = parts.next() else { continue };
            if !perms.starts_with('r') {
                continue;
            }
            let Some((lo, hi)) = range.split_once('-') else {
                continue;
            };
            let Ok(lo) = usize::from_str_radix(lo, 16) else {
                continue;
            };
            let Ok(hi) = usize::from_str_radix(hi, 16) else {
                continue;
            };
            regions.push((lo, hi));
        }
        regions
    }

    fn readable_region_end_from_regions(addr: usize, regions: &[(usize, usize)]) -> Option<usize> {
        for &(lo, hi) in regions {
            if addr >= lo && addr < hi {
                return Some(hi.min(addr.saturating_add(MAX_STACK_SCAN)));
            }
        }
        None
    }

    fn scan_slot<F>(slot: &LinuxSlot, is_obj: &F, roots: &mut Vec<ObjectRef>) -> usize
    where
        F: Fn(usize) -> Option<ObjectRef>,
    {
        let regions = readable_regions();
        scan_slot_with_regions(slot, &regions, is_obj, roots)
    }

    fn scan_slot_with_regions<F>(
        slot: &LinuxSlot,
        regions: &[(usize, usize)],
        is_obj: &F,
        roots: &mut Vec<ObjectRef>,
    ) -> usize
    where
        F: Fn(usize) -> Option<ObjectRef>,
    {
        let mut found = 0usize;
        let pair_tid = slot.tid.load(Ordering::Acquire);
        for (ri, reg) in slot.regs.iter().enumerate() {
            let v = reg.load(Ordering::Acquire);
            if let Some(o) = is_obj(v) {
                // LINUX arm of `CRATONVM_DBG_PEER_REG_PAIRING` (§11 built the
                // Windows one). Cast: REG_COUNT is 17.
                cratonvm_gc::gc_quiescence::record_peer_reg(pair_tid, ri as u8, v);
                roots.push(o);
                found += 1;
            }
        }

        let rsp = slot.rsp.load(Ordering::Acquire);
        if rsp == 0 || rsp & 0x7 != 0 {
            return found;
        }
        let Some(end) = readable_region_end_from_regions(rsp, regions) else {
            return found;
        };
        let mut p = rsp;
        while p + 8 <= end {
            let w = unsafe { (p as *const usize).read_unaligned() };
            if let Some(o) = is_obj(w) {
                cratonvm_gc::gc_quiescence::record_peer_reg(pair_tid, 0xff, w);
                roots.push(o);
                found += 1;
            }
            p += 8;
        }
        found
    }

    fn release_slot(slot: &LinuxSlot) {
        slot.resume.store(1, Ordering::Release);
        let start = Instant::now();
        while slot.state.load(Ordering::Acquire) == STATE_PARKED
            && start.elapsed() < Duration::from_millis(100)
        {
            std::thread::yield_now();
        }
        slot.clear();
    }

    /// Returns `(has_jit, complete)`.
    ///
    /// `complete` is the half that licenses PINNING instead of refusing the
    /// cycle: it says this peer's conservative root set is the WHOLE of what it
    /// can reach -- its register file and every readable word of its stack from
    /// `rsp` up. Pinning a partial set helps nothing, because what was missed
    /// is unrewritable too, so the two early returns below report `false` and
    /// the caller keeps refusing.
    fn classify_slot_helper_window<F>(
        slot: &LinuxSlot,
        regions: &[(usize, usize)],
        ranges: &[(usize, usize)],
        is_obj: &F,
        candidates: &mut Vec<ObjectRef>,
    ) -> (bool, bool)
    where
        F: Fn(usize) -> Option<ObjectRef>,
    {
        let mut has_jit = false;
        let pair_tid_hw = slot.tid.load(Ordering::Acquire);
        for (ri_hw, reg) in slot.regs.iter().enumerate() {
            let v = reg.load(Ordering::Acquire);
            if !has_jit && ranges.iter().any(|&(lo, hi)| v >= lo && v < hi) {
                has_jit = true;
            }
            if let Some(o) = is_obj(v) {
                // Cast: REG_COUNT is 17.
                cratonvm_gc::gc_quiescence::record_peer_reg(pair_tid_hw, ri_hw as u8, v);
                candidates.push(o);
            }
        }

        let rsp = slot.rsp.load(Ordering::Acquire);
        if rsp == 0 || rsp & 0x7 != 0 {
            return (has_jit, false);
        }
        let Some(end) = readable_region_end_from_regions(rsp, regions) else {
            return (has_jit, false);
        };
        let mut p = rsp;
        while p + 8 <= end {
            let w = unsafe { (p as *const usize).read_unaligned() };
            if !has_jit && ranges.iter().any(|&(lo, hi)| w >= lo && w < hi) {
                has_jit = true;
            }
            if let Some(o) = is_obj(w) {
                cratonvm_gc::gc_quiescence::record_peer_reg(pair_tid_hw, 0xff, w);
                candidates.push(o);
            }
            p += 8;
        }
        (has_jit, true)
    }

    /// Linux implementation of the cross-thread JIT root scan. We cannot use
    /// Windows' SuspendThread/GetThreadContext primitives, so the collector sends
    /// a private signal to peer threads. The handler only parks peers interrupted
    /// inside registered JIT code; interpreter/native peers return immediately
    /// and remain cooperative barrier participants.
    pub fn take_over_pass<F>(taken: &mut TakenOver, is_obj: &F, roots: &mut Vec<ObjectRef>) -> usize
    where
        F: Fn(usize) -> Option<ObjectRef>,
    {
        install_handler();
        let ranges = crate::jit::jit_code_ranges_snapshot();
        if ranges.is_empty() {
            return 0;
        }
        publish_ranges(&ranges);
        ACTIVE.store(true, Ordering::Release);

        let self_tid = gettid();
        let mut newly = 0usize;
        let mut examined = 0usize;
        let mut unclassified = 0usize;
        let mut roots_this_pass = 0usize;
        for tid in list_thread_tids() {
            if tid == self_tid || taken.contains(tid) {
                continue;
            }
            let Some(slot) = arm_slot(tid) else {
                // No free slot: every remaining peer goes unscanned.
                XT_PEERS_UNCLASSIFIED.fetch_add(1, Ordering::Relaxed);
                unclassified += 1;
                break;
            };
            examined += 1;
            if !send_takeover_signal(tid) {
                // ESRCH: the thread exited between listing and signalling.
                // Nothing to scan and nothing running — not a coverage hole.
                slot.clear();
                continue;
            }
            let resignals_before = XT_PEER_RESIGNALS.load(Ordering::Relaxed);
            let answer = wait_for_response_retrying(slot, tid);
            // Any DEFINITIVE answer that needed a re-signal would have been an
            // UNCLASSIFIED peer before the retry — `STATE_NOT_JIT` counts, it
            // means the peer reached the handler with its `Rip` outside JIT
            // code and is a cooperative barrier participant publishing its own
            // roots. Counting only `STATE_PARKED` reported zero on a run where
            // the retry had just converted sixteen.
            if answer != STATE_CANCELLED
                && XT_PEER_RESIGNALS.load(Ordering::Relaxed) != resignals_before
            {
                XT_PEERS_CLASSIFIED_AFTER_RETRY.fetch_add(1, Ordering::Relaxed);
            }
            match answer {
                STATE_PARKED => {
                    let found = scan_slot(slot, is_obj, roots);
                    taken.handles.push(0);
                    taken.tids.push(tid);
                    newly += 1;
                    roots_this_pass += found;
                    XT_THREADS_TAKEN_OVER.fetch_add(1, Ordering::Relaxed);
                    XT_ROOTS_FOUND.fetch_add(found as u64, Ordering::Relaxed);
                    // See the Windows arm: a frozen peer's conservatively
                    // scanned registers/stack are not rewritable by this
                    // collection, so it must not be a moving one.
                    cratonvm_gc::gc_quiescence::mark_moving_young_coverage_incomplete_because(
                        cratonvm_gc::gc_quiescence::incomplete_reason::XT_TAKEOVER,
                    );
                    if dbg() {
                        eprintln!(
                            "[xt-jit-roots] linux took over tid={tid} rip=0x{:x}: {found} conservative roots",
                            slot.rip.load(Ordering::Acquire)
                        );
                    }
                }
                // `STATE_NOT_JIT` is a real answer: the peer reached the
                // handler and its `Rip` was outside JIT code, so it is a
                // cooperative barrier participant publishing its own precise
                // roots. `STATE_CANCELLED` is NOT an answer — the peer never
                // reached the handler, is STILL RUNNING, and its JIT-frame
                // oops are in no root set.
                STATE_CANCELLED => {
                    XT_PEERS_UNCLASSIFIED.fetch_add(1, Ordering::Relaxed);
                    unclassified += 1;
                    slot.clear();
                }
                _ => slot.clear(),
            }
        }
        if unclassified > 0 {
            XT_CYCLES_WITH_UNCLASSIFIED.fetch_add(1, Ordering::Relaxed);
        }
        // Publish this cycle's coverage where the SWEEP can read it. A looping
        // reproduction never reaches the shutdown summary these counters were
        // previously only visible in, and the question "did this sweep mark
        // from a complete root set?" has to be answerable per sweep.
        cratonvm_gc::gc_quiescence::publish_xt_pass(
            newly as u64,
            unclassified as u64,
            roots_this_pass as u64,
        );
        if taken.tids.is_empty() {
            ACTIVE.store(false, Ordering::Release);
            RANGES_LEN.store(0, Ordering::Release);
        }
        if dbg() {
            eprintln!(
                "[xt-jit-roots] linux pass: signaled {examined} peer(s), {newly} newly taken over; {} code ranges; any_thread_in_jit={} jit_gate={}",
                ranges.len(),
                crate::jit::conservative_roots::any_thread_in_jit(),
                cratonvm_jit::xt_jit_root_scan_enabled(),
            );
        }
        newly
    }

    pub fn resume(taken: TakenOver) {
        for tid in taken.tids {
            if let Some(slot) = find_slot(tid) {
                release_slot(slot);
            }
        }
        ACTIVE.store(false, Ordering::Release);
        HELPER_MODE.store(false, Ordering::Release);
        RANGES_LEN.store(0, Ordering::Release);
    }

    pub fn helper_window_pass<F>(
        taken: &TakenOver,
        is_obj: &F,
        roots: &mut Vec<ObjectRef>,
        blocked_os_tids: &[u32],
    ) -> (usize, usize)
    where
        F: Fn(usize) -> Option<ObjectRef>,
    {
        let ranges = crate::jit::jit_code_ranges_snapshot();
        if ranges.is_empty() || blocked_os_tids.is_empty() {
            return (0, 0);
        }
        install_handler();
        publish_ranges(&ranges);
        let regions = readable_regions();
        let had_taken = taken.count() > 0;
        ACTIVE.store(true, Ordering::Release);
        HELPER_MODE.store(true, Ordering::Release);

        let self_tid = gettid();
        let mut candidates: Vec<ObjectRef> = Vec::new();
        let mut windows = 0usize;
        let mut pinned_windows = 0usize;
        let mut unpinned_windows = 0usize;
        // Per-cycle, so reset before the pass rather than accumulated.
        XT_HELPER_WINDOWS_UNPINNED_CYCLE.store(0, Ordering::Release);
        let mut found_total = 0usize;
        let mut examined = 0usize;
        let mut unclassified = 0usize;
        for &tid in blocked_os_tids {
            if tid == self_tid || taken.contains(tid) {
                continue;
            }
            let Some(slot) = arm_slot(tid) else {
                // No free slot: every remaining peer goes unscanned.
                XT_PEERS_UNCLASSIFIED.fetch_add(1, Ordering::Relaxed);
                unclassified += 1;
                break;
            };
            examined += 1;
            if !send_takeover_signal(tid) {
                // ESRCH: the thread exited between listing and signalling.
                // Nothing to scan and nothing running — not a coverage hole.
                slot.clear();
                continue;
            }
            let resignals_before = XT_PEER_RESIGNALS.load(Ordering::Relaxed);
            let answer = wait_for_response_retrying(slot, tid);
            // Any DEFINITIVE answer that needed a re-signal would have been an
            // UNCLASSIFIED peer before the retry — `STATE_NOT_JIT` counts, it
            // means the peer reached the handler with its `Rip` outside JIT
            // code and is a cooperative barrier participant publishing its own
            // roots. Counting only `STATE_PARKED` reported zero on a run where
            // the retry had just converted sixteen.
            if answer != STATE_CANCELLED
                && XT_PEER_RESIGNALS.load(Ordering::Relaxed) != resignals_before
            {
                XT_PEERS_CLASSIFIED_AFTER_RETRY.fetch_add(1, Ordering::Relaxed);
            }
            match answer {
                STATE_PARKED => {
                    candidates.clear();
                    let (has_jit, complete) = classify_slot_helper_window(
                        slot,
                        &regions,
                        &ranges,
                        is_obj,
                        &mut candidates,
                    );
                    if has_jit {
                        windows += 1;
                        // A JIT frame's oops live in the SHADOW STACK, which
                        // is not the machine stack and so is invisible to
                        // everything above. Scan it too, or the pin is
                        // incomplete and any coverage credited on it is a lie.
                        // An untrusted window refuses the pin rather than
                        // claiming coverage it does not have.
                        let shadow_ok = if crate::jit::conservative_roots::xt_peer_shadow_scan_enabled()
                        {
                            super::scan_peer_shadow_window(tid, is_obj, &mut candidates).is_some()
                        } else {
                            true
                        };
                        found_total += candidates.len();
                        let roots_this_window = candidates.len();
                        // PIN, rather than refuse the whole cycle.
                        //
                        // This peer's conservative root set is COMPLETE -- the
                        // classifier read its register file and every readable
                        // word of its stack -- so nothing it can reach is
                        // missing from `candidates`. Pinning exactly those
                        // addresses is the same contract the cooperatively
                        // parked threads already get through
                        // `publish_pinned_jit_roots`: the objects do not move,
                        // their FIELDS are still rewritten through the pointer
                        // map, and everything else in the heap may relocate.
                        //
                        // The peer could not publish for itself because it was
                        // interrupted by our signal inside a Rust helper,
                        // reaching neither a safepoint arrival nor a
                        // blocking-region entry -- the only two deposit points.
                        // The scan runs on the COLLECTOR's thread, so it cannot
                        // publish under the peer's `ThreadId` either; hence a
                        // per-cycle set.
                        if complete && shadow_ok && helper_window_pin_enabled() {
                            let addrs: Vec<usize> =
                                candidates.iter().map(|o| o.as_ptr() as usize).collect();
                            cratonvm_gc::gc_quiescence::add_xt_cycle_pinned_jit_roots(&addrs);
                            pinned_windows += 1;
                            // A pinned window covers the peer's WHOLE stack
                            // (`[rsp, stack_base)`) plus its register file, so
                            // every JIT frame it holds is immobile -- which is
                            // what the cross-thread coverage account wants to
                            // hear, and it wants to hear it as a DEPTH.
                            //
                            // Reading the peer's published depth after it has
                            // resumed is still exact: `mark_blocked_region_leave`
                            // waits out an active pause, so a blocked peer
                            // cannot run Java (and so cannot mutate its chain)
                            // between the block and the end of this STW.
                            //
                            // `None` -- a peer that never registered a slot --
                            // poisons the ledger rather than crediting zero.
                            if crate::jit::conservative_roots::xt_pinned_peer_depth_enabled() {
                                cratonvm_gc::gc_quiescence::add_xt_cycle_pinned_jit_depth(
                                    cratonvm_gc::gc_quiescence::jit_depth_of_tid(tid),
                                );
                            }
                        } else {
                            unpinned_windows += 1;
                        }
                        roots.append(&mut candidates);
                        if dbg() {
                            eprintln!(
                                "[xt-jit-roots] linux helper-window tid={tid}: JIT frames on native stack (Rip outside JIT), {roots_this_window} conservative roots"
                            );
                        }
                    }
                    release_slot(slot);
                }
                // `STATE_NOT_JIT` is a real answer: the peer reached the
                // handler and its `Rip` was outside JIT code, so it is a
                // cooperative barrier participant publishing its own precise
                // roots. `STATE_CANCELLED` is NOT an answer — the peer never
                // reached the handler, is STILL RUNNING, and its JIT-frame
                // oops are in no root set.
                STATE_CANCELLED => {
                    XT_PEERS_UNCLASSIFIED.fetch_add(1, Ordering::Relaxed);
                    unclassified += 1;
                    slot.clear();
                }
                _ => slot.clear(),
            }
        }
        if unclassified > 0 {
            XT_CYCLES_WITH_UNCLASSIFIED.fetch_add(1, Ordering::Relaxed);
        }

        HELPER_MODE.store(false, Ordering::Release);
        if !had_taken {
            ACTIVE.store(false, Ordering::Release);
            RANGES_LEN.store(0, Ordering::Release);
        }
        XT_HELPER_WINDOWS_SCANNED.fetch_add(windows as u64, Ordering::Relaxed);
        XT_HELPER_WINDOW_ROOTS.fetch_add(found_total as u64, Ordering::Relaxed);
        // EVERY window still refuses, and the pins above do NOT discharge it.
        //
        // This was wrong in the first cut of this change, which suppressed the
        // refusal for a pinned window. The pins are real and the scan is
        // complete in the sense that matters for what it FINDS -- register file
        // plus the whole readable stack band -- but the predicate it finds with
        // is `VmHeap::is_object_address`, which is `registry.contains(addr)`:
        // EXACT OBJECT BASES ONLY. A frozen peer holding a derived or interior
        // pointer (a compiled loop's pointer into an array body is the ordinary
        // case) contributes no candidate at all, so its base object is not
        // pinned, and relocating it strands the peer on resume. The comment at
        // the second refusal site in `interpreter::gc_and_alloc` says exactly
        // this -- "a frozen peer's registers can hold only a derived/interior
        // pointer whose base would otherwise be evacuated from under it" -- and
        // it is the reason that site marks the cycle incomplete too.
        //
        // Discharging this properly needs the interior-resolving predicate,
        // `is_heap_addr`, which is now affordable (one backwards bit scan plus
        // one header dereference via `nearest_base_at_or_below`, not the old
        // O(live) registry iteration) and which `vm_heap.rs` already feeds
        // per-slot conservative scanning. The cost of adopting it here is a
        // WIDER conservative root set -- every long that happens to land inside
        // a live object's extent becomes a root -- and that trade has not been
        // measured. Until it is, the refusal stands.
        //
        // The pins are still published, because they are strictly additive
        // (they can only keep a page out of one CSet) and because they are what
        // made the shortfall measurable: `hw_pinned`/`hw_refused` on the
        // `[GC] xt_peer_scan` line say how many windows a future discharge
        // would have to cover.
        XT_HELPER_WINDOWS_UNPINNED_CYCLE.store(unpinned_windows as u64, Ordering::Release);
        if windows > 0
            && !(helper_window_discharge_enabled() && unpinned_windows == 0)
        {
            cratonvm_gc::gc_quiescence::mark_moving_young_coverage_incomplete_because(
                cratonvm_gc::gc_quiescence::incomplete_reason::XT_HELPER_WINDOW,
            );
        }
        XT_HELPER_WINDOWS_PINNED.fetch_add(pinned_windows as u64, Ordering::Relaxed);
        XT_HELPER_WINDOWS_REFUSED.fetch_add(unpinned_windows as u64, Ordering::Relaxed);
        cratonvm_gc::gc_quiescence::publish_xt_helper_window(windows as u64, found_total as u64);
        if dbg() {
            eprintln!(
                "[xt-jit-roots] linux helper-window pass: examined {examined} blocked peer(s), {windows} window(s), {found_total} conservative root(s)"
            );
        }
        (windows, found_total)
    }
}

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
pub use imp::{helper_window_pass, resume, take_over_pass};

// ---------------------------------------------------------------------------
// Non-Windows stubs (the OS-suspend primitive is Windows-only here)
// ---------------------------------------------------------------------------

#[cfg(not(any(windows, all(target_os = "linux", target_arch = "x86_64"))))]
pub fn take_over_pass<F>(_taken: &mut TakenOver, _is_obj: &F, _roots: &mut Vec<ObjectRef>) -> usize
where
    F: Fn(usize) -> Option<ObjectRef>,
{
    0
}

#[cfg(not(any(windows, all(target_os = "linux", target_arch = "x86_64"))))]
pub fn resume(_taken: TakenOver) {}

#[cfg(not(any(windows, all(target_os = "linux", target_arch = "x86_64"))))]
pub fn helper_window_pass<F>(
    _taken: &TakenOver,
    _is_obj: &F,
    _roots: &mut Vec<ObjectRef>,
    _blocked_os_tids: &[u32],
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
