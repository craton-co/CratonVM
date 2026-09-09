// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! NEW-1.5 — conservative root scanning for active JIT frames.
//!
//! ## Why this exists
//!
//! Until we land precise oop maps for JIT frames (the original A1.1 wording),
//! the GC has no way to know which i64 spill slots in a JIT-compiled method's
//! stack frame contain object references. A copying / compacting GC therefore
//! cannot safely walk a JIT call stack: an object whose only live reference
//! lives in a JIT spill slot would be reclaimed (or worse, silently relocated
//! while the spill slot still pointed at the old address).
//!
//! Before this module existed, the workaround was to forbid JIT compilation
//! of any method that could trigger GC while a finalizer was reachable
//! (`FinalizerTest` blanket ban in [`crate::jit::skip_list`]).
//!
//! ## What this module guarantees
//!
//! 1. **Per-thread active JIT entry chain.** [`push_jit_entry_at`] is called
//!    immediately before transferring control to JIT-compiled code; it captures
//!    the current native stack pointer (an upper bound on the spill region) and
//!    pushes it onto a thread-local stack. [`pop_jit_entry`] restores the prior
//!    state when the JIT call returns. Re-entrant interpreter↔JIT calls compose
//!    because the chain is a stack, not a single slot.
//!
//! 2. **Conservative scanner.** [`scan_active_jit_frames`] walks each entry on
//!    the chain from the *current* `RSP` up to the captured entry `RSP`,
//!    treating every 8-byte aligned qword as a *possible* heap pointer. A
//!    candidate is reported as a root only if [`VmHeap::is_object_address`]
//!    confirms it lands on a live object header in either the from-space or
//!    the to-space arena. False positives are filtered, false negatives are
//!    impossible (every real reference is at an 8-byte aligned spill slot —
//!    enforced by the JIT calling convention).
//!
//! 3. **GC-quiescence flag.** [`any_thread_in_jit`] returns `true` whenever any
//!    thread anywhere in the process holds at least one active JIT entry. The
//!    semispace copying collector consults this flag and *defers compaction*
//!    while it is set: marking still happens (so freshly unreachable objects
//!    are still found), but objects are not relocated. This keeps the
//!    conservative roots consistent — a stack qword that *coincidentally*
//!    equals an object address never gets rewritten because the object never
//!    moves while a JIT frame is active.
//!
//! ## What this module does NOT do
//!
//! - It does not produce a precise oop map. A spill slot containing an `i64`
//!   that happens to fall within the heap arena is reported as a root and the
//!   target is therefore pinned for that GC cycle. This is a *false positive*
//!   that wastes a small amount of heap but cannot cause incorrect behavior.
//! - It does not allow compaction while a JIT frame is active. Defragmentation
//!   resumes naturally as soon as every JIT call has returned.
//! - It does not replace the precise oop maps required by ZGC / Shenandoah-style
//!   concurrent relocators. Those remain a tracked future item; this module is
//!   the production-safe stop-gap that closes the user-visible blocker
//!   (`FinalizerTest` could not be JIT-compiled).
//!
//! ## Safety
//!
//! All public functions are safe to call from Rust code. The internals capture
//! the native stack pointer via [`std::ptr::null::<u8>`] arithmetic — the
//! captured value is treated as an opaque address, never dereferenced as a
//! Rust reference. The scanner reads memory through `unsafe { *.read() }` and
//! validates the resulting candidate with `VmHeap::is_object_address` before
//! treating it as a root, so an unaligned / stale / spurious value can never
//! cause a use-after-free or out-of-bounds read.

use std::cell::RefCell;
use std::sync::atomic::{AtomicUsize, Ordering};

use cratonvm_types::ObjectRef;

use crate::memory::vm_heap::VmHeap;
use crate::threading::thread_state::{self, ThreadExecState};

// ---------------------------------------------------------------------------
// Thread-local active JIT entry chain
// ---------------------------------------------------------------------------

/// NEW-12: one entry in the per-thread JIT call chain.
///
/// Every active JIT call pushes one of these at entry and pops it at
/// exit. The `entry_sp` field is the stack pointer captured at the
/// moment of the push — the conservative fallback scanner walks the
/// native stack between the current SP and this value to find any
/// qword that looks like a heap address.
///
/// The `precise` field is `Some(PreciseFrameInfo)` when the caller
/// registered a compiled method that has populated precise oop maps
/// (NEW-12). In that case the root walker can enumerate oops exactly
/// via [`JitFrameChainEntry::precise_oops`] instead of doing a blind
/// range scan. When `precise` is `None`, the entry is conservative:
/// the walker still reads every qword in the spill region and
/// validates via `heap.is_object_address`.
///
/// Both modes produce a **superset** of the real oops (false positives
/// are filtered at heap validation time) so GC correctness is
/// guaranteed regardless of which path runs.
#[derive(Clone, Copy)]
pub(crate) struct JitFrameChainEntry {
    /// Stack pointer captured at JIT entry. Used as the upper bound of
    /// the conservative scan for this frame.
    pub entry_sp: usize,
    /// Optional precise-frame metadata. When `Some`, the walker uses
    /// the compiled method's oop map at the current native PC to
    /// enumerate oops directly from frame slots.
    pub precise: Option<PreciseFrameInfo>,
    /// How many *interpreter* frames this thread had when the entry was
    /// pushed — i.e. where this compiled frame sits in the Java call stack.
    ///
    /// A compiled method executes without pushing a `runtime::frame::Frame`,
    /// so `Thread.getStackTrace()` (and every throwable's trace) used to skip
    /// it entirely: once H2's query chain warmed up, an ALIAS function saw a
    /// 6-frame stack where the interpreter showed 21, and
    /// `org.h2.test.db.TestIndex.testFunctionIndex` — whose whole assertion is
    /// that an `org.h2.command.query.Select` frame is visible from inside the
    /// function — failed. `capture_full_trace` interleaves the chain back in
    /// using this number.
    ///
    /// [`u32::MAX`] means "inherit the enclosing entry's": a JIT→JIT dispatch
    /// (`try_call_compiled_entry_reentrant`) is called *from* compiled code and
    /// has no `JvmThread` to ask, but no interpreter frame can have been pushed
    /// since the entry it nests inside, so the enclosing depth is exact.
    pub interp_depth: u32,
}

/// Sentinel for [`JitFrameChainEntry::interp_depth`]: resolve at push time from
/// the entry this one nests inside.
pub(crate) const INTERP_DEPTH_INHERIT: u32 = u32::MAX;

/// NEW-12: metadata needed to walk a JIT frame with precise oop maps.
///
/// Stored inline in [`JitFrameChainEntry`] when a caller opts into
/// precise root enumeration via [`JitEntryGuard::enter_with_compiled`].
#[derive(Clone, Copy)]
pub(crate) struct PreciseFrameInfo {
    /// Raw pointer to the [`cratonvm_jit::CompiledMethod`] whose code
    /// is currently executing in this frame. The pointer is borrowed
    /// — callers guarantee the CompiledMethod outlives the JIT call,
    /// which holds trivially because the guard is scoped to a single
    /// call and the caller owns an `&CompiledMethod` for its duration.
    ///
    /// Dereferencing this pointer at GC time is safe because:
    ///   1. The JIT cache holds an owning `Arc<CompiledMethod>` for
    ///      the duration of every compiled method's registration, so
    ///      the CM cannot be dropped while a call is in flight.
    ///   2. The chain entry is popped the moment the JIT call returns
    ///      or unwinds — there is no stale-pointer window.
    pub compiled_method: *const cratonvm_jit::CompiledMethod,
    /// Base address of the frame (the RBP value captured at the
    /// start of the prologue). Oop-map slot offsets are added to
    /// this value to obtain the absolute address of each oop slot.
    ///
    /// Also captured via [`current_stack_pointer`] like `entry_sp`;
    /// in practice the two are within a handful of bytes of each
    /// other because the guard is constructed immediately before the
    /// call transferring control to compiled code. The walker uses
    /// `frame_base` for oop-slot computation and `entry_sp` for
    /// bounding the conservative fallback when no map matches.
    pub frame_base: usize,
    /// Base address of the compiled method's entry point, cached so
    /// the walker can compute `current_pc - entry_ptr = offset` and
    /// look up the matching oop map entry.
    pub entry_ptr: *const u8,
    /// Stage 3/5 (precise relocation) — the EXACT RBP of the *innermost*
    /// active JIT frame, recorded by [`set_top_frame_base`] from the deepest
    /// prologue's `frame_record` helper. Used by precise slot scans for OSR
    /// boundary frames and as the start of the `remap_active_jit_frames`
    /// RBP-chain walk. It is kept SEPARATE from
    /// `frame_base` (the Rust-guard SP captured at entry) because the marking
    /// path [`scan_one_frame_precise`] uses `frame_base` as the UPPER bound of
    /// its conservative sweep `[scanner_sp, frame_base)`: clobbering it with
    /// the (low) innermost RBP shrank that sweep to just the innermost frame,
    /// dropping every ancestor frame's spilled oops → live objects reclaimed →
    /// heap corruption. `0` until the prologue records it (gate off).
    pub exact_rbp: usize,
    /// The compile id published by the frame standing at [`Self::exact_rbp`],
    /// captured in the SAME breath as that RBP.
    ///
    /// Both halves are written together by generated code, but they are only a
    /// matched pair at the instant they are read off the mirrors. Reading the
    /// id later — at scan time — is wrong twice over: the owning thread may
    /// have entered and left other compiled frames since, and the scan often
    /// runs on the COLLECTOR's thread, whose mirrors describe its own stack and
    /// not the one being walked. (Measured: reading it at scan time left the
    /// obligation in 785 of 786 cycles, i.e. it never once resolved.) So it is
    /// snapshotted here, beside the RBP it belongs to, and every consumer takes
    /// it from this struct. `0` = nothing published for that frame.
    pub exact_cm_id: u32,
}

// SAFETY: the raw pointers in JitFrameChainEntry are not dereferenced
// without additional validation (CompiledMethod is Arc-owned by the JIT
// cache; the chain is thread-local so no cross-thread access). Marking
// the struct Send+Sync enables storage in the thread-local RefCell,
// which cargo clippy otherwise flags.
unsafe impl Send for JitFrameChainEntry {}
unsafe impl Sync for JitFrameChainEntry {}

thread_local! {
    /// Stack of entries captured at each active JIT call. The top of
    /// the stack is the *innermost* JIT call (most recent).
    ///
    /// Under the NEW-12 refactor the chain holds [`JitFrameChainEntry`]
    /// structs instead of raw stack pointers. Entries whose `precise`
    /// field is `None` retain the NEW-1 conservative-scan semantics;
    /// entries with `precise = Some` are enumerated via the compiled
    /// method's oop maps at GC time.
    static JIT_ENTRY_CHAIN: RefCell<Vec<JitFrameChainEntry>> =
        const { RefCell::new(Vec::new()) };
}

thread_local! {
    /// Memoizes how deep the "unregistered JIT frame above the tracked chain"
    /// scan (see the A5-fix block in `scan_active_jit_frames`) has already
    /// verified clean: `(verified_lo, code_range_count_at_verification)`.
    ///
    /// That scan checks `[search_lo, stack_high)` for a stray JIT return
    /// address on every `update_root_snapshot` call. `search_lo` tracks the
    /// current (thread-local) native stack pointer, which only DECREASES as
    /// this thread recurses deeper — so once a range `[verified_lo,
    /// stack_high)` has been scanned and found clean, any LATER call whose
    /// `search_lo >= verified_lo` needs only a subset of that same range:
    /// nothing above our current stack pointer can change while we are
    /// nested below it (that memory belongs to still-waiting caller frames),
    /// so the "clean" verdict still holds and the scan can be skipped.
    ///
    /// The one way the verdict COULD go stale is a method compiling (OSR or
    /// otherwise) between the two checks: a stack slot that held a plain
    /// (non-JIT) value at the first scan could later be sitting where a
    /// *new* JIT code range now claims to start. Guard against that by also
    /// storing `jit_code_range_count()` at verification time and requiring
    /// it be unchanged — any new compilation invalidates the memo and forces
    /// a fresh scan. Reset to `(usize::MAX, 0)` so the very first check
    /// always scans.
    ///
    /// 2026-07-17: the same "nothing above our current stack pointer can
    /// change while nested below it" argument also covers the opposite
    /// direction — recursing DEEPER (`search_lo < verified_lo`) with an
    /// unchanged `code_range_count`. The once-verified `[verified_lo,
    /// stack_high)` band is unaffected by descending further below it, so
    /// `scan_active_jit_frames` only needs to scan the new incremental band
    /// `[search_lo, verified_lo)` in that case, not the whole
    /// `[search_lo, stack_high)` range again. See the caller for the
    /// rationale and the throughput evidence that motivated it (a
    /// perpetually-growing recursion depth, as in Hibernate/JUnit5's nested
    /// call chains, previously paid a full stack rescan on every single
    /// per-native-call root snapshot).
    static UNREG_JIT_MEMO: std::cell::Cell<UnregMemo> =
        const { std::cell::Cell::new(UnregMemo::new()) };

    /// H2-CID0 (2026-08-06) — is the NEXT unregistered-frame scan on this thread
    /// one a collector will actually consume?
    ///
    /// Set by `invalidate_scan_cache_for_gc` (the four GC-authoritative sites)
    /// and cleared by the scan that follows. Only the audit reads it, and only
    /// to attribute a suppression to the path where suppression is a defect
    /// rather than the intended optimisation.
    static UNREG_JIT_AUTHORITATIVE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };


}

thread_local! {
    /// Perf mirror of the CURRENT top chain entry's `exact_rbp`. The hot
    /// per-invocation `set_top_frame_base` (called once per JIT method entry —
    /// ~1.8B times for fib44) writes ONLY this `Cell` (a single TLS store, no
    /// `RefCell` borrow / `Vec::last_mut` / Option matching), cutting the
    /// per-call cost. It is synced with the chain at the rare push/pop/retain
    /// boundaries and flushed into the top entry before GC marking/remap reads
    /// `exact_rbp`. Invariant:
    /// `TOP_RBP` == the live `exact_rbp` of `JIT_ENTRY_CHAIN.last()` (or 0 when
    /// the chain is empty / the top is not yet precise).
    static TOP_RBP: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// Step 1 (`docs/feature-designs/precise-jit-maps-default.md`, inline
/// frame-record) — read/write the innermost-RBP mirror.
///
/// When inline frame-record is active (`cratonvm_jit::x64::inline_rbp_tls_disp()
/// != 0`), generated code stores RBP directly into an OS TLS slot with one
/// segment-relative instruction (`gs:` on Windows, `fs:` on Linux); these
/// accessors read/write that SAME slot so cold push/pop/prune/remap paths
/// observe the inlined writes. When unavailable, they fall back to the Rust
/// `thread_local! TOP_RBP` exactly as before. The displacement and mirror
/// accessors are shared with JIT codegen, so the two cannot disagree on the
/// slot.
#[inline]
fn top_rbp_get() -> usize {
    #[cfg(windows)]
    {
        let disp = cratonvm_jit::x64::inline_rbp_tls_disp();
        if disp != 0 {
            // SAFETY: `disp` was validated by the startup sentinel probe in
            // `inline_rbp_tls_disp()` to be a live, 8-byte-aligned TEB TLS slot
            // present on every thread.
            return unsafe { read_gs_qword(disp) };
        }
    }
    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    if let Some(value) = cratonvm_jit::x64::inline_rbp_tls_mirror_read() {
        return value;
    }
    TOP_RBP.with(|c| c.get())
}

/// Step 1 self-check (`CRATONVM_DBG_VERIFY_INLINE_FRAME_RECORD`) — public read
/// of the innermost-RBP mirror, used by `jit_verify_inline_frame_record` to
/// confirm the inlined store landed in the slot the GC reads.
pub fn top_rbp_mirror_read() -> usize {
    top_rbp_get()
}

/// Save/restore hook for Rust-side compiled-entry calls
/// (`try_call_compiled_entry` in `vm/src/jit/helpers.rs`): a compiled callee's
/// prologue publishes ITS rbp into the mirror and nothing on the Rust dispatch
/// path restores the JIT caller's value when the callee returns. A GC
/// triggered from any later helper call would then start its precise remap
/// walk at the DEAD callee frame (whose stack memory has been reused by
/// subsequent Rust frames); a garbage "parent rbp"/"return address" pair that
/// happens to resolve into registered JIT code gets its "oop slots" rewritten
/// with relocated pointers — observed as heap addresses appearing inside
/// double[] elements during Arrays.sort (ES SortingDigestTests, -Jit on).
/// The dispatch helpers snapshot the mirror before the raw entry call and
/// write it back afterwards. The JIT→JIT direct-call analogue is
/// `emit_post_call_rbp_republish` in `jit/src/x64.rs`.
pub fn top_rbp_mirror_write(v: usize) {
    top_rbp_set(v);
}

/// Save/restore of the IDENTITY half of the frame record, for the same bracket
/// as [`top_rbp_mirror_write`].
///
/// The two mirrors are written together by generated code, and that pairing is
/// what lets a conservative scan name the method owning the innermost RBP. A
/// bracket that restored only the RBP would leave the pair naming two different
/// frames — the caller's rbp beside the callee's identity — and the scan cannot
/// detect that, because both halves would still read consistently out of the
/// mirrors. So this is not an optimisation: restoring one without the other is
/// how the identity becomes actively wrong rather than merely absent.
pub fn top_cm_id_mirror_read() -> u32 {
    published_compile_id()
}

/// See [`top_cm_id_mirror_read`]. A no-op when no identity slot is active.
pub fn top_cm_id_mirror_write(id: u32) {
    #[cfg(windows)]
    {
        let disp = cratonvm_jit::x64::inline_cm_tls_disp();
        if disp != 0 {
            // SAFETY: `disp` was validated by the startup sentinel probe in
            // `inline_cm_tls_disp()` — a live, 8-byte-aligned TEB TLS slot on
            // every thread, exactly as for the RBP mirror.
            unsafe { write_gs_qword(disp, id as usize) };
            return;
        }
    }
    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    {
        if cratonvm_jit::x64::inline_cm_tls_mirror_write(id) {
            return;
        }
    }
    let _ = id;
}

#[inline]
fn top_rbp_set(v: usize) {
    #[cfg(windows)]
    {
        let disp = cratonvm_jit::x64::inline_rbp_tls_disp();
        if disp != 0 {
            // SAFETY: see `top_rbp_get`.
            unsafe { write_gs_qword(disp, v) };
            return;
        }
    }
    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    if cratonvm_jit::x64::inline_rbp_tls_mirror_write(v) {
        return;
    }
    TOP_RBP.with(|c| c.set(v));
}

/// Read the 8-byte value at `gs:[disp]` (Windows TEB-relative TLS slot).
#[cfg(windows)]
#[inline]
unsafe fn read_gs_qword(disp: usize) -> usize {
    let val: usize;
    core::arch::asm!(
        "mov {out}, qword ptr gs:[{addr}]",
        out = out(reg) val,
        addr = in(reg) disp,
        options(nostack, preserves_flags, readonly),
    );
    val
}

/// Write an 8-byte value to `gs:[disp]` (Windows TEB-relative TLS slot).
#[cfg(windows)]
#[inline]
unsafe fn write_gs_qword(disp: usize, val: usize) {
    core::arch::asm!(
        "mov qword ptr gs:[{addr}], {val}",
        addr = in(reg) disp,
        val = in(reg) val,
        options(nostack, preserves_flags),
    );
}

/// Process-wide counter of active JIT entries across all threads. Lets the GC
/// quickly answer "is anyone in JIT?" without crossing thread boundaries.
///
/// Striped per thread — it is written twice per interpreter/JIT boundary
/// crossing and read only by the GC, so as one shared `AtomicUsize` it was a
/// contended cache line on the hottest path in the VM. See
/// [`cratonvm_types::striped_counter`].
static GLOBAL_JIT_DEPTH: cratonvm_types::striped_counter::StripedCounter =
    cratonvm_types::striped_counter::StripedCounter::new();

/// `CRATONVM_XT_PINNED_PEER_DEPTH=1` -- credit a frozen peer's JIT depth to the
/// cross-thread coverage account when this cycle PINNED that peer's entire
/// stack, instead of refusing the cycle because the peer never parked to prove
/// its own frames rewritable.
///
/// Default OFF, so the two behaviours are one binary apart and the comparison
/// is an A/B rather than a rebuild. See
/// [`refresh_moving_young_coverage_for_collection`] for the argument, and
/// `cratonvm_gc::gc_quiescence::add_xt_cycle_pinned_jit_depth` for why the
/// per-thread depth is published from here rather than from the (much colder)
/// blocked-region transition.
pub fn xt_pinned_peer_depth_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    // DEFAULT ON since 2026-09-04 -- see `helper_window_discharge_enabled` for
    // the measurement. `CRATONVM_XT_PINNED_PEER_DEPTH=0` is the kill switch.
    //
    // This credit was suspected of causing the SIGSEGV on that class for most
    // of its life, and it did not: the crash was the compaction slide writing
    // into a decommitted granule (`Arena::ensure_committed_span`). The credit
    // only ever raised the amount of compaction, and compaction is what runs
    // slides.
    *ON.get_or_init(|| {
        !matches!(
            cratonvm_types::flags::runtime_var("CRATONVM_XT_PINNED_PEER_DEPTH").as_deref(),
            Ok("0") | Ok("false") | Ok("off") | Ok("no")
        )
    })
}

/// `CRATONVM_XT_PINNED_PEER_PUBLISH_ONLY=1` -- publish per-thread depths and
/// deposit them, but credit NOTHING to the coverage account.
///
/// The bisect lever for the segfault this feature produced on
/// `TestCachedQueryResults`: with one flag gating both the publisher (a TLS
/// `Arc` written on every JIT chain mutation) and the decision (relocating on
/// cycles that used to refuse), a crash cannot be attributed to either. This
/// arm runs the whole mechanism EXCEPT the decision, so a crash here indicts
/// the publisher and a crash only without it indicts the relocation.
pub fn xt_pinned_peer_publish_only() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        cratonvm_types::flags::runtime_var_os("CRATONVM_XT_PINNED_PEER_PUBLISH_ONLY").is_some()
    })
}

/// `CRATONVM_XT_PEER_SHADOW_SCAN=1` -- scan and pin a frozen blocked peer's
/// SHADOW STACK, not just its registers and machine stack.
///
/// A JIT frame's oops live in the shadow stack, a per-thread heap allocation
/// the helper-window scan cannot see. Without this the pinned-peer depth credit
/// claims coverage it does not have, and the class SIGSEGVs.
pub fn xt_peer_shadow_scan_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    // DEFAULT ON since 2026-09-04. Without it the pin is incomplete by
    // construction: a JIT frame's oops live in the shadow stack, a per-thread
    // HEAP allocation that the register+stack scan cannot see, and a blocked
    // peer's is scanned by nothing and remapped by nothing. Measured at 2090
    // heap refs recovered on a 216 s run that registers and stack missed
    // entirely. `CRATONVM_XT_PEER_SHADOW_SCAN=0` is the kill switch.
    *ON.get_or_init(|| {
        !matches!(
            cratonvm_types::flags::runtime_var("CRATONVM_XT_PEER_SHADOW_SCAN").as_deref(),
            Ok("0") | Ok("false") | Ok("off") | Ok("no")
        )
    })
}

thread_local! {
    static SHADOW_ADDR_PUBLISHED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Publish this thread's shadow-stack address once. Called from
/// `set_jit_thread`, which runs on every interpreter->JIT entry, so the
/// repeat path is one TLS bool.
pub fn publish_self_shadow_addr_once(addr: usize, base: usize, end: usize) {
    // `base == 0` means `ensure_allocated` has not run yet; publishing that
    // would register a window with no buffer. Stay unpublished (the reader
    // treats absent as UNKNOWN and refuses) and try again on the next entry.
    if base == 0 {
        return;
    }
    let _ = SHADOW_ADDR_PUBLISHED.try_with(|c| {
        if c.get() {
            return;
        }
        cratonvm_gc::gc_quiescence::publish_self_shadow_addr(self_os_tid(), addr, base, end);
        c.set(true);
    });
}

/// This thread's OS tid, in the same namespace `blocked_os_tids` reports and
/// `helper_window_pass` enumerates -- the key the initiator will look this
/// thread's depth up by.
#[cfg(windows)]
fn self_os_tid() -> u32 {
    unsafe extern "system" {
        fn GetCurrentThreadId() -> u32;
    }
    unsafe { GetCurrentThreadId() }
}

#[cfg(all(unix, target_os = "linux"))]
fn self_os_tid() -> u32 {
    // SAFETY: `SYS_gettid` takes no arguments and cannot fail.
    unsafe { libc::syscall(libc::SYS_gettid) as u32 }
}

#[cfg(not(any(windows, all(unix, target_os = "linux"))))]
fn self_os_tid() -> u32 {
    // No helper-window scanner on this platform, so nothing ever reads the
    // slot; 0 keeps every thread on the never-registered path.
    0
}

/// Owns this thread's registry entry so it is REMOVED when the thread exits.
///
/// A leftover entry is readable under a tid the OS will recycle, and a recycled
/// thread that never enters JIT never overwrites it -- so the initiator would
/// credit depth nobody holds. `register_self_jit_depth_slot` also resets on
/// re-registration, which covers the threads whose destructors never run.
struct JitDepthSlot {
    os_tid: u32,
    cell: std::sync::Arc<std::sync::atomic::AtomicUsize>,
}

impl Drop for JitDepthSlot {
    fn drop(&mut self) {
        cratonvm_gc::gc_quiescence::unregister_jit_depth_slot(self.os_tid);
    }
}

thread_local! {
    /// This thread's published JIT-depth cell, resolved once. `None` until the
    /// first chain mutation with the credit enabled.
    static SELF_JIT_DEPTH_SLOT: std::cell::RefCell<Option<JitDepthSlot>> =
        const { std::cell::RefCell::new(None) };
}

/// Publish this thread's current `JIT_ENTRY_CHAIN` length so a GC initiator
/// that freezes this thread can attribute a depth to it.
///
/// One TLS read and one relaxed store on a path that already does both; the
/// registry lock is taken only on this thread's FIRST call. Gated so the
/// default arm pays a single cached-bool branch.
#[inline]
fn publish_self_jit_depth(depth: usize) {
    if !xt_pinned_peer_depth_enabled() {
        return;
    }
    // `try_with`, not `with`: `pop_jit_entry` can run while this thread is
    // tearing down, and `with` on an already-destroyed thread-local PANICS.
    // A skipped publish only under-credits.
    let _ = SELF_JIT_DEPTH_SLOT.try_with(|c| {
        // `try_borrow_mut`: this runs on the JIT entry/exit path, which a panic
        // unwind can re-enter. Skipping a publish is safe (the initiator then
        // reads a stale-SMALLER depth and under-credits, refusing a cycle it
        // could have run); a double-borrow panic here would not be.
        let Ok(mut slot) = c.try_borrow_mut() else {
            return;
        };
        let held = slot.get_or_insert_with(|| {
            let os_tid = self_os_tid();
            JitDepthSlot {
                os_tid,
                cell: cratonvm_gc::gc_quiescence::register_self_jit_depth_slot(os_tid),
            }
        });
        held.cell.store(depth, std::sync::atomic::Ordering::Release);
    });
}

/// Re-export the GC-side quiescence flag so VM call sites have a single
/// canonical entry point. The flag itself lives in the gc crate (see
/// `gc::gc_quiescence`) because the GC must consult it from inside its own
/// collection cycles, which would create a circular dependency if the flag
/// lived in the vm crate.
pub use cratonvm_gc::gc_quiescence::is_active as gc_must_defer;

/// Whether the JIT **shadow-stack** precise-roots mechanism is enabled
/// (`CRATONVM_SHADOW_STACK`). Cached on first read.
///
/// When on, JIT codegen pushes live oops onto each thread's
/// [`cratonvm_gc::shadow_stack::ShadowStack`] around GC-capable safepoints, the
/// marking root scan folds those values into the root set, the post-move remap
/// rewrites them in place, and the young-gen collector is permitted to run the
/// *moving* (Cheney) cycle even while JIT frames are live (see
/// `gen_heap.rs` quiescence gate). The standalone shadow-stack knob remains
/// opt-in; the default moving-young configuration enables the mechanism.
///
/// The original 2026-06-22 standalone experiment was incomplete. Moving-young
/// now publishes complete oop homes and uses a per-cycle coverage proof; an
/// incomplete frame diverts the collection to the non-moving sweep.
#[inline]
pub fn shadow_stack_enabled() -> bool {
    use std::sync::OnceLock;
    static ENABLED: OnceLock<bool> = OnceLock::new();
    // `CRATONVM_MOVING_YOUNG` implies the shadow-stack root scan + remap: the
    // moving young gen relies on the complete precise map the shadow stack now
    // publishes (see `moving_young_enabled`).
    //
    // This formula MUST stay identical to the emission side,
    // `jit::x64::shadow_stack_maps_enabled`, or the collector walks a shadow
    // stack the codegen never pushed to. Both were tried scoped to
    // `JIT_PUBLISHES_RELOCATION_CONTRACT` and both were reverted together: the
    // scoping measured as no-change (see that function's comment), and moving
    // one side of this agreement is not worth doing for an unmeasurable win.
    // `CRATONVM_JIT_MY_SHADOW_EMISSION=0` drops the moving-young implication on
    // BOTH sides at once; see `jit::x64::shadow_emission_moving_implication_enabled`.
    // This expression must stay character-for-character equivalent to the
    // emission side's.
    //
    // 2026-07-31 — the `moving_young_enabled() &&` term is REMOVED, on BOTH
    // sides in the same change. Publication is how a JIT frame's live
    // references become visible to the collector at all; that is not a
    // property of which young collector runs. Keyed on moving-young, the
    // documented opt-out `CRATONVM_NO_MOVING_YOUNG=1` withdrew it, and that
    // lane faulted on a zeroed heap slot within seconds of real work.
    // Restoring publication with `CRATONVM_SHADOW_STACK=1` and changing
    // nothing else made the same runs clean. See
    // `jit-no-moving-young-opt-out-unpublishes-roots-CLOSED-20260803.md`.
    *ENABLED.get_or_init(|| {
        cratonvm_types::flags::runtime_var_os("CRATONVM_SHADOW_STACK").is_some()
            || match cratonvm_types::flags::runtime_var("CRATONVM_JIT_MY_SHADOW_EMISSION") {
                Ok(v) => v != "0" && !v.eq_ignore_ascii_case("false"),
                Err(_) => true,
            }
    })
}

/// Whether the **moving / compacting young generation** is in effect.
///
/// When on: (1) the JIT publishes a *complete* rewritable precise root map at
/// each safepoint (see `cratonvm_jit::x64::moving_young_enabled`), (2) the
/// conservative JIT-frame scan in `roots.rs` is SUPPRESSED (a fully-precise frame
/// needs no conservative backstop, and mixing a conservatively-marked slot with a
/// precisely-relocated object would corrupt), and (3) `gen_heap` runs the moving
/// (Cheney) young collection even while JIT frames are live instead of diverting
/// to the non-moving sweep.
///
/// # This is an INTERLOCK, not an independent policy decision (arch-2026-07-26)
///
/// This used to be its own `cratonvm_types::flags::runtime_var_os("CRATONVM_MOVING_YOUNG")` read —
/// the second of **three** copies of the same predicate, alongside
/// `cratonvm_jit::x64::moving_young_enabled` (codegen) and
/// `cratonvm_gc::gc_quiescence::moving_young_enabled` (collector). Three
/// independent reads of one safety-critical switch mean the default cannot be
/// flipped safely: flipping the collector without the codegen relocates objects
/// whose only home is a JIT register that no shadow push ever recorded, and
/// flipping the root gatherer without the codegen suppresses the conservative
/// backstop with nothing precise replacing it.
///
/// The effective gate is the **conjunction** of two questions that are answered
/// in different crates:
///
///   * *Can* we move? — `cratonvm_jit::x64::moving_young_enabled()`. Only the
///     codegen can physically emit the shadow push/reload and the
///     per-safepoint `moving_young_coverage_complete` bit. If it says no,
///     nothing anywhere else can make relocation safe.
///   * *Should* we move? — `cratonvm_types::flags().gc.moving_young`, the typed
///     config (opt-OUT `CRATONVM_NO_MOVING_YOUNG` over
///     `flags::DEFAULT_MOVING_YOUNG`).
///
/// AND-ing them is fail-safe in both directions. The centralized x64 projection
/// and this check make `CRATONVM_NO_MOVING_YOUNG` authoritative even when a
/// stale `CRATONVM_MOVING_YOUNG` compatibility variable is also present.
///
/// The result is then published to the GC crate, which cannot call into the JIT
/// crate (`cratonvm-gc` has no `cratonvm-jit` dependency, only a
/// dev-dependency). So the collector always relocates against the same answer
/// the codegen compiled for, and no skew between the three layers is
/// representable. The shipped default is now the single `true` constant in
/// `cratonvm_types::flags::DEFAULT_MOVING_YOUNG`.
#[inline]
pub fn moving_young_enabled() -> bool {
    use std::sync::OnceLock;
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| {
        // `x64::moving_young_enabled` is itself a `OnceLock`, and `flags()`
        // latches on first use, so this resolves once for the process.
        let on =
            cratonvm_jit::x64::moving_young_enabled() && cratonvm_types::flags().gc.moving_young;
        // Hand the collector the same answer. Before this publish the GC uses
        // `gc_flags().moving_young` on its own, which is only reachable in a
        // process with no JIT at all — where there are no JIT frames and moving
        // is unconditionally safe.
        cratonvm_gc::gc_quiescence::publish_moving_young_enabled(on);
        on
    })
}

/// Re-publish the moving-young decision to the GC crate.
///
/// Cheap (one store behind an already-resolved `OnceLock`) and called from the
/// root gatherer so the collector's view is refreshed on the path of every
/// collection, even if the first `moving_young_enabled()` call happened on a
/// different thread than the one that will collect.
#[inline]
pub fn publish_moving_young_gate() {
    let _ = moving_young_enabled();
}

/// Returns true when moving-young must fall back to the conservative
/// JIT-frame scan + non-moving sweep because an active OSR artifact cannot
/// prove rewritable shadow coverage for this collection.
pub fn moving_young_osr_shadow_fallback_needed() -> bool {
    if !moving_young_enabled() {
        return false;
    }
    let debug_shadow_disabled = cratonvm_types::flags::runtime_var_os("CRATONVM_SHADOW_NOPUSH")
        .is_some()
        || cratonvm_types::flags::runtime_var_os("CRATONVM_SHADOW_NORELOAD").is_some();
    JIT_ENTRY_CHAIN.with(|c| {
        let mut chain = c.borrow_mut();
        flush_top_rbp_cache_to_chain(chain.as_mut_slice());
        chain.iter().any(|entry| {
            let Some(info) = entry.precise else {
                return false;
            };
            // SAFETY: the chain entry stores a CompiledMethod pointer borrowed
            // from the live JIT cache entry; it remains valid while the guard is
            // on the stack.
            let cm: &cratonvm_jit::CompiledMethod = unsafe { &*info.compiled_method };
            moving_young_osr_method_needs_fallback(cm, info.exact_rbp, debug_shadow_disabled)
        })
    })
}

/// Per-disjunct breakdown of why [`moving_young_osr_method_needs_fallback`]
/// returned true, so the single `osr-shadow-coverage-unproven` reason code the
/// collector sees can be split apart without a debugger. Filed 2026-08-21:
/// `bug-h2-testkillprocess-zgc-oom-at-97-percent-free-20260821-FIXED-20260829.md`'s own
/// measurement found this reason blocking 234/263 collections and could not
/// say which of the (then three) disjuncts was responsible, only that "H2's
/// MVStore loops are OSR-compiled constantly." Attribution is by the same
/// short-circuit priority the boolean uses, so exactly one counter increments
/// per call that returns true: a frame with a genuinely broken shadow layout
/// is not ALSO double-counted under the map-coverage bucket just because it
/// would have failed that check too.
pub mod osr_fallback_reason {
    use std::sync::atomic::AtomicUsize;

    /// `!shadow_layout_ok` — the shadow-stack prologue slots were never
    /// allocated for this compilation at all. A codegen gap: this method was
    /// compiled by a path that does not emit shadow-stack bookkeeping.
    pub static BAD_SHADOW_LAYOUT: AtomicUsize = AtomicUsize::new(0);
    /// `debug_shadow_disabled` — `CRATONVM_SHADOW_NOPUSH` / `_NORELOAD` forced
    /// it. Not a production path; present so a debug run does not silently
    /// fall through to a different bucket and misattribute.
    pub static DEBUG_DISABLED: AtomicUsize = AtomicUsize::new(0);
    /// Shadow layout is fine, precise maps exist, but this safepoint's map is
    /// not `fully_oop_covered`.
    pub static BAD_MAP_COVERAGE: AtomicUsize = AtomicUsize::new(0);
    /// Shadow layout is fine, the map is fully covered, but the chain entry
    /// carries no exact RBP for this frame — the map cannot be located
    /// without one, even though it would answer the question if it could.
    pub static MISSING_EXACT_RBP: AtomicUsize = AtomicUsize::new(0);

    pub fn snapshot() -> (usize, usize, usize, usize) {
        use std::sync::atomic::Ordering::Relaxed;
        (
            BAD_SHADOW_LAYOUT.load(Relaxed),
            DEBUG_DISABLED.load(Relaxed),
            BAD_MAP_COVERAGE.load(Relaxed),
            MISSING_EXACT_RBP.load(Relaxed),
        )
    }
}

/// Does the OSR coverage check read the SHADOW aggregate
/// (`CompiledMethod::fully_shadow_covered`) rather than the frame-slot subset
/// (`fully_oop_covered`)?
///
/// Default ON, so a KILL SWITCH: `CRATONVM_OSR_COVERAGE_SHADOW=0` (or
/// `off`/`false`/`no`) restores the frame-slot reading, which is the bisect for
/// anything that appears with this on. Read per call rather than latched, so a
/// test can set it without deciding the answer for every later test in the
/// binary; the call site runs once per live chain entry per collection.
fn osr_coverage_uses_shadow_aggregate() -> bool {
    match cratonvm_types::flags::runtime_var_os("CRATONVM_OSR_COVERAGE_SHADOW") {
        Some(raw) => {
            let v = raw.to_string_lossy().trim().to_ascii_lowercase();
            !matches!(v.as_str(), "0" | "off" | "false" | "no")
        }
        None => true,
    }
}

fn moving_young_osr_method_needs_fallback(
    cm: &cratonvm_jit::CompiledMethod,
    exact_rbp: usize,
    debug_shadow_disabled: bool,
) -> bool {
    if !cm.compiled_via_osr {
        return false;
    }
    let shadow_layout_ok = cm.shadow_thread_slot_off != 0
        && cm.shadow_savetop_slot_off != 0
        && cm.shadow_off_in_thread != 0;
    // THE SHADOW AGGREGATE, NOT THE FRAME-SLOT SUBSET (2026-08-23).
    //
    // This function is `moving_young_osr_shadow_fallback_needed`'s per-method
    // half and its whole subject is whether an OSR artifact can prove
    // **rewritable shadow coverage**. It asked `fully_oop_covered`, which on
    // the fast tier — the only tier that produces OSR artifacts, since
    // `ir_lower` publishes no `osr_pc_to_native` — is a different question:
    // `safepoint_pcs ⊆ mapped_safepoint_pcs`, i.e. "every live oop is named by
    // a FRAME SLOT".
    //
    // A direct JIT→JIT call with a reference argument can never satisfy that.
    // The argument is popped off the operand stack and marshalled into the
    // outgoing-ABI area, which no frame-slot map can name, so all three direct
    // -call arms raise `pending_staged_args_unmapped` and the safepoint's pc is
    // withheld from `mapped_safepoint_pcs` — permanently, for the whole method.
    // Measured on `TestKillProcessWhileWriting` with `CRATONVM_DBG_OOPCOV=1`:
    // 439 of 449 coverage failures are that one shape, and through this term
    // they refused relocation on 725 of 759 collections. That is the
    // `bug-h2-testkillprocess-zgc-oom-at-97-percent-free-20260821-FIXED-20260829.md` residual,
    // and the page's own guess (a cross-thread peer) was measured at 0 of 759.
    //
    // The staged argument is not the caller's live value any more — it is the
    // CALLEE's parameter, covered by the callee's own locals map, and the
    // caller never re-reads the outgoing area after the call. Nothing the
    // moving cycle must rewrite is lost by not naming it, which is why the
    // per-safepoint SHADOW verdict is the honest question here.
    //
    // Narrowing this term does not weaken the proof, because it does not stand
    // alone: `refresh_moving_young_coverage_for_collection` — which the OSR
    // fallback SHORT-CIRCUITS PAST when it fires — then runs
    // `moving_young_frame_coverage_complete` on the active frame and every
    // parent (the same `moving_young_coverage_complete` flag, resolved through
    // the live safepoint id, so per-frame rather than per-method) and the band
    // verifier's empirical walk for young-resident unpublished words. Those are
    // strictly sharper than a method-wide bit; the effect of this change is
    // that they get to run.
    //
    // `CRATONVM_OSR_COVERAGE_SHADOW=0` restores `fully_oop_covered` on the same
    // binary.
    let covered = if osr_coverage_uses_shadow_aggregate() {
        cm.fully_shadow_covered
    } else {
        cm.fully_oop_covered
    };
    let precise_map_ok = !cm.has_precise_oop_maps() || (covered && exact_rbp != 0);
    use std::sync::atomic::Ordering::Relaxed;
    if !shadow_layout_ok {
        osr_fallback_reason::BAD_SHADOW_LAYOUT.fetch_add(1, Relaxed);
        return true;
    }
    if debug_shadow_disabled {
        osr_fallback_reason::DEBUG_DISABLED.fetch_add(1, Relaxed);
        return true;
    }
    if !precise_map_ok {
        if cm.has_precise_oop_maps() && !covered {
            osr_fallback_reason::BAD_MAP_COVERAGE.fetch_add(1, Relaxed);
        } else {
            osr_fallback_reason::MISSING_EXACT_RBP.fetch_add(1, Relaxed);
        }
        return true;
    }
    false
}

/// spring-bug-10 experiment (`CRATONVM_SHADOW_PIN`): when set, the shadow-stack
/// marking publish (`memory/roots.rs`) treats shadow oops as PINNED rather than
/// MOVABLE — keeping them alive without the evacuate→remap→reload path. Cached.
#[inline]
pub fn shadow_pin_roots() -> bool {
    use std::sync::OnceLock;
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| cratonvm_types::flags::runtime_var_os("CRATONVM_SHADOW_PIN").is_some())
}

/// Capture the current native stack pointer.
///
/// Implemented as the address of a probe variable that **must** live in the
/// caller's frame, not in a separate callee frame that immediately gets torn
/// down. Hence `#[inline(always)]`: when the function is inlined, `probe`
/// becomes a local of the caller and `&probe` is a pointer into the caller's
/// frame. If we forbade inlining, the probe would live in *this* function's
/// frame, the function would return, the frame would be deallocated, and the
/// recorded SP would point into freed stack memory — which is exactly the bug
/// we're trying to avoid.
#[inline(always)]
pub fn current_stack_pointer() -> usize {
    let probe: u8 = 0;
    // `&probe` forces `probe` to take an address, which forces it onto the
    // (caller's, after inlining) stack rather than living in a register.
    &probe as *const u8 as usize
}

/// The current thread's stack HIGH limit (one past the highest usable stack
/// address). Bounds the A5 unregistered-JIT-frame scan and the
/// `CRATONVM_DBG_FULLSTACK_SCAN` diagnostic.
///
/// Windows: Win32 `GetCurrentThreadStackLimits`. Linux (A5 port, GC audit
/// 2026-07-10 INT-3 follow-up): `pthread_getattr_np` + `pthread_attr_getstack`
/// — `stack_addr` is the LOWEST address, so high = addr + size. glibc answers
/// for the initial thread too (it derives the main stack extent from
/// /proc/self/maps + RLIMIT_STACK), but that derivation is not free, so the
/// result is memoized per thread — a thread's stack top never changes.
/// Returns 0 on failure; callers treat 0 as "unknown" and skip the scan
/// (safe degradation, matching the pre-port non-Windows behaviour).
#[cfg(any(target_os = "windows", target_os = "linux"))]
fn current_thread_stack_high() -> usize {
    thread_local! {
        static CACHED_HIGH: std::cell::Cell<usize> = const { std::cell::Cell::new(usize::MAX) };
    }
    let cached = CACHED_HIGH.with(std::cell::Cell::get);
    if cached != usize::MAX {
        return cached;
    }
    let high = current_thread_stack_high_uncached();
    CACHED_HIGH.with(|c| c.set(high));
    high
}

#[cfg(target_os = "windows")]
fn current_thread_stack_high_uncached() -> usize {
    #[link(name = "kernel32")]
    extern "system" {
        fn GetCurrentThreadStackLimits(low_limit: *mut usize, high_limit: *mut usize);
    }
    let mut low: usize = 0;
    let mut high: usize = 0;
    // SAFETY: passes two valid out-pointers to a well-known Win32 API that
    // only writes the thread's stack bounds; no other effect.
    unsafe { GetCurrentThreadStackLimits(&mut low, &mut high) };
    high
}

#[cfg(target_os = "linux")]
fn current_thread_stack_high_uncached() -> usize {
    // SAFETY: standard glibc stack-introspection sequence. `attr` is
    // initialized by pthread_getattr_np before any read, and destroyed on
    // every successful-init path. The out-pointers are valid locals.
    unsafe {
        let mut attr: libc::pthread_attr_t = std::mem::zeroed();
        if libc::pthread_getattr_np(libc::pthread_self(), &mut attr) != 0 {
            return 0;
        }
        let mut lo: *mut libc::c_void = std::ptr::null_mut();
        let mut size: libc::size_t = 0;
        let ok = libc::pthread_attr_getstack(&attr, &mut lo, &mut size) == 0;
        libc::pthread_attr_destroy(&mut attr);
        if !ok || lo.is_null() || size == 0 {
            return 0;
        }
        (lo as usize).saturating_add(size)
    }
}

thread_local! {
    /// Highest `entry_sp` among the JIT entries that have RETURNED on this
    /// thread -- an upper bound on the stack addresses whose current contents
    /// may be a returned frame's leftovers rather than anything live.
    ///
    /// A compiled frame writes only below its own `entry_sp`, so once it
    /// returns, every return address it left into JIT code lies below that
    /// mark. `scan_active_jit_frames`'s unregistered-frame probe uses this to
    /// tell residue from a genuinely live guardless frame; see its call site.
    ///
    /// Monotonic. A frame entered at `sp` writes only below `sp`, so it never
    /// overwrites residue at or above `sp`; resetting the mark on a push would
    /// discard exactly the higher-addressed leftovers of an earlier, shallower
    /// frame. The one live guardless frame the probe exists for is the process
    /// entry point, which sits above every JIT entry the run ever makes.
    static JIT_RESIDUE_HI: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// Record that a JIT entry with this `entry_sp` has returned.
fn note_jit_residue(entry_sp: usize) {
    JIT_RESIDUE_HI.with(|c| c.set(c.get().max(entry_sp)));
}

/// Upper bound on this thread's returned-JIT-frame residue, or 0 when no JIT
/// frame has returned yet.
fn jit_residue_hi() -> usize {
    JIT_RESIDUE_HI.with(|c| c.get())
}

/// Record that JIT execution is about to begin on the current thread.
///
/// Captures the current stack pointer at the instant of the call and pushes
/// it onto the per-thread chain. Returns the depth *after* the push (1-based)
/// purely as a debugging convenience.
///
/// The caller must pair every `push_jit_entry` with exactly one
/// [`pop_jit_entry`] when the JIT call returns or unwinds. The pairing is
/// stack-discipline (LIFO).
/// Internal: push a *given* stack pointer onto the JIT entry chain. Used by
/// the inline entry-point macros so the captured SP belongs to the caller's
/// frame, not to ours. Direct callers should generally prefer
/// [`JitEntryGuard::enter`] which handles the SP capture and pop pairing.
pub fn push_jit_entry_at(sp: usize) -> usize {
    push_entry_full(JitFrameChainEntry {
        entry_sp: sp,
        precise: None,
        interp_depth: INTERP_DEPTH_INHERIT,
    })
}

/// NEW-12: push a fully-specified chain entry. Used by
/// [`JitEntryGuard::enter_with_compiled`] to register both the stack
/// pointer and the precise-frame metadata in one atomic step.
pub(crate) fn push_entry_full(entry: JitFrameChainEntry) -> usize {
    // Chain mutation = JIT boundary: invalidate the per-thread scan cache.
    note_jit_boundary();
    // Every transfer of control into compiled code passes through here, so this
    // is the run's interpreter->JIT entry count. Divided into the JIT's measured
    // CPU delta it gives the per-entry cost, which is the number that decides
    // whether "compiling short methods an interpreted caller invokes" is what
    // makes the JIT a net negative on call-dense classes.
    scan_prof::bump(&scan_prof::JIT_ENTRIES);
    let depth = JIT_ENTRY_CHAIN.with(|c| {
        let mut v = c.borrow_mut();
        // Resolve the "same place in the Java stack as my caller" sentinel
        // while the chain is in hand. An empty chain means the entry is the
        // outermost compiled frame reached from a site that could not name its
        // interpreter depth; 0 places it below every interpreter frame, which
        // is the only safe guess and never reorders the frames that DO know.
        let mut entry = entry;
        if entry.interp_depth == INTERP_DEPTH_INHERIT {
            entry.interp_depth = v.last().map_or(0, |e| e.interp_depth);
        }
        // Finalize the outgoing top entry's `exact_rbp` from the cache before it
        // becomes non-top (each entry's `exact_rbp` is read by the relocation
        // walk). The incoming entry starts unrecorded → reset the cache to 0.
        if let Some(old_top) = v.last_mut() {
            if let Some(info) = old_top.precise.as_mut() {
                info.exact_rbp = top_rbp_get();
                info.exact_cm_id = published_compile_id();
            }
        }
        v.push(entry);
        let n = v.len();
        top_rbp_set(0);
        // The incoming entry starts unrecorded in BOTH mirrors. Zeroing only
        // the RBP left the identity naming the entry we just pushed BELOW us,
        // so the pair described two different frames from the first instruction
        // of the new entry. See `reload_top_rbp_cache` for what that cost.
        if cm_id_pairing_enabled() {
            top_cm_id_mirror_write(0);
        }
        n
    });
    GLOBAL_JIT_DEPTH.inc();
    publish_self_jit_depth(depth);
    // P1 shadow record (`docs/threading/thread-transition-states.md` §7.2):
    // this is the ONLY point at which a thread becomes
    // `CompiledUninterruptible`. A nested entry re-records the same state,
    // which the recorder treats as the counting event it is (self-edges are
    // legal) rather than a transition.
    thread_state::record_transition(
        ThreadExecState::CompiledUninterruptible,
        "jit::conservative_roots::push_entry_full",
    );
    cratonvm_jit::jit_execution_enter();
    // Mirror into the GC-side quiescence flag so the GC can defer
    // compaction whenever any thread is inside a JIT call. NEW-12's
    // precise root walk removes false positives from the root set,
    // but compaction still requires precise oop-map coverage across
    // the *entire* active chain — and today the JIT compiler itself
    // does not yet populate maps from its simulated-stack type
    // tracker, so conservative fallback entries remain possible. The
    // defer guard can only be lifted once every entry in flight is
    // guaranteed precise (a later follow-up under NEW-12).
    cratonvm_gc::gc_quiescence::enter();
    depth
}

/// Capture the current SP at the call site and push it onto the JIT entry
/// chain. **Must be inlined** so the captured SP belongs to the caller's
/// frame; calling this from a function that immediately returns would record
/// a stale SP pointing into freed stack memory.
///
/// `#[cfg(test)]`: every production entry now captures its own SP and calls
/// `push_jit_entry_at` directly, because the SP that matters is the one at
/// the transfer site and not this wrapper's. Only the tests still want the
/// convenience form, and `no_test_only_public_api` counts a `pub` item whose
/// sole real caller is a test -- the ratchet was one over its 299 baseline on
/// pristine dev because of this one.
#[cfg(test)]
#[inline(always)]
pub fn push_jit_entry() -> usize {
    let sp = current_stack_pointer();
    push_jit_entry_at(sp)
}

/// P1 shadow record — the state a thread leaving one JIT entry lands in, given
/// the chain length that REMAINS after the pop.
///
/// An empty chain means the thread is back in the interpreter
/// (`CompiledUninterruptible -> JavaRunning`, the tabled edge for
/// `pop_jit_entry` / `prune_returned_jit_entries`). A non-empty one means an
/// outer compiled frame is still live, so the thread stays
/// `CompiledUninterruptible` (a legal self-edge).
///
/// The `Deoptimizing` case is the exception, and it is why this is a function
/// rather than a bare `if`: a deopt trap raised the signal from *inside*
/// compiled code, and `Deoptimizing`'s only tabled successors are
/// `JavaRunning` / `VmRunning` — re-recording `CompiledUninterruptible` for a
/// nested pop would manufacture a violation the code is not actually
/// committing. The window is closed at the pop instead, which is the last
/// point this module can observe; the interpreter's own resume sites
/// (`resume_from_ir_deopt`, `real_frame_deopt_resume_and_despeculate`) then
/// re-record `JavaRunning` as a no-op self-edge.
#[inline]
fn leaving_compiled_state(remaining: usize) -> ThreadExecState {
    if remaining == 0 || thread_state::current_state() == ThreadExecState::Deoptimizing {
        ThreadExecState::JavaRunning
    } else {
        ThreadExecState::CompiledUninterruptible
    }
}

/// Pop the topmost entry off the JIT entry chain.
///
/// Should be called immediately after a JIT call returns, regardless of
/// success / failure / panic unwind. Returns the popped entry SP for
/// diagnostic purposes.
pub fn pop_jit_entry() -> Option<usize> {
    // Chain mutation = JIT boundary: invalidate the per-thread scan cache.
    note_jit_boundary();
    let (popped, remaining) = JIT_ENTRY_CHAIN.with(|c| {
        let mut v = c.borrow_mut();
        let p = v.pop();
        // Mirror now tracks the entry that became top again.
        reload_top_rbp_cache(&v);
        (p, v.len())
    });
    if let Some(entry) = popped {
        GLOBAL_JIT_DEPTH.dec();
        publish_self_jit_depth(remaining);
        thread_state::record_transition(
            leaving_compiled_state(remaining),
            "jit::conservative_roots::pop_jit_entry",
        );
        cratonvm_gc::gc_quiescence::leave();
        cratonvm_jit::jit_execution_leave();
        // P1 code-cache retirement: leaving a compiled frame is one of the two
        // moments `GLOBAL_JIT_DEPTH` can reach zero, and therefore one of the
        // two moments an unpublished body can become reclaimable. The sweep
        // asks the quiescence question itself (with the retirement queue lock
        // held — see `code_cache_lifecycle`'s §1.2); all this site owes it is
        // the wake-up. The gate is one relaxed load, and with nothing queued —
        // the overwhelmingly common case — that is the whole cost.
        if crate::jit::code_cache_lifecycle::pending_retirements() != 0 {
            crate::jit::code_cache_lifecycle::sweep_if_quiescent();
        }
        note_jit_residue(entry.entry_sp);
        Some(entry.entry_sp)
    } else {
        None
    }
}

/// Round-7 corruption fix: self-heal a LEAKED `JitEntryGuard`.
///
/// When a JIT call's RAII `Drop`/[`pop_jit_entry`] is bypassed (an
/// abandoned compiled-callee frame — e.g. a non-local exit through the
/// JIT return path), a stale entry is left on the chain **and** the
/// `gc_quiescence` counter stays permanently elevated. A wedged
/// quiescence forces the GC onto the *non-moving* young sweep on every
/// collection, which is the heap corruptor (verified round 7:
/// `CRATONVM_DBG_FORCE_MOVING=1` → 0 corruption vs a deterministic 211
/// under `CRATONVM_DBG_GC_STRESS`).
///
/// This prunes every chain entry that has *provably returned*. The
/// conservative scanner's invariant (see [`scan_active_jit_frames`]) is
/// that a **live** JIT spill region lies at or above the scanner's
/// current SP — the native stack grows downward, so a live ancestor
/// frame's `entry_sp` is always `>= scanner_sp`. An entry whose captured
/// `entry_sp` is strictly **below** `scanner_sp` therefore cannot belong
/// to any live frame: its call has returned without popping. Removing
/// such entries (and matching each one with a `GLOBAL_JIT_DEPTH`
/// decrement + `gc_quiescence::leave()`) lets quiescence fall back to the
/// true live count, so the moving collector resumes the moment no JIT
/// frame is genuinely live.
///
/// Soundness: the predicate **never** removes a live frame (a live frame
/// always satisfies `entry_sp >= scanner_sp`), so genuine JIT activity
/// still correctly keeps quiescence active and the non-moving sweep
/// engaged. Returns the number of stale entries reclaimed.
pub fn prune_returned_jit_entries(scanner_sp: usize) -> usize {
    let mut remaining = 0usize;
    let pruned = JIT_ENTRY_CHAIN.with(|c| {
        let mut v = c.borrow_mut();
        let before = v.len();
        // Keep only entries that could still be live (spill region at or
        // above the scanner SP). Entries below it have provably returned.
        for e in v.iter().filter(|e| e.entry_sp < scanner_sp) {
            note_jit_residue(e.entry_sp);
        }
        v.retain(|e| e.entry_sp >= scanner_sp);
        let pruned = before - v.len();
        // Mirror tracks whatever entry is top after pruning — but ONLY when
        // pruning actually changed the top.
        //
        // This reload used to be unconditional, and that single line is what
        // kept the moving young generation switched off. The mirror is written
        // by each compiled prologue (`mov gs:[disp], rbp`, or the
        // `jit_frame_record` helper); `PreciseFrameInfo::exact_rbp` is only a
        // SNAPSHOT of it, taken in `push_entry_full` when an entry stops being
        // top. For the entry that is *currently* top nothing has ever written
        // that field, so it still holds the `0` from `enter_with_compiled`.
        //
        // `refresh_moving_young_coverage_for_current_thread` calls this
        // function first and then reads the mirror. With the unconditional
        // reload, a no-op prune overwrote the live RBP with that `0` on the way
        // in, and the coverage proof then failed itself with MISSING_EXACT_RBP
        // — measured as 100% of fallbacks (67/67 on `BinTreesClassic 18` at
        // `-Xmx512m`, 58/58 at depth 16, 2/2 at depth 14) with `top_rbp=0x0`
        // logged for a frame that had 7 oop maps and a live sp-id slot. Nothing
        // was wrong with the frame; the verifier had erased its own input.
        if pruned > 0 {
            reload_top_rbp_cache(&v);
        }
        remaining = v.len();
        pruned
    });
    if pruned > 0 {
        // Chain mutation = JIT boundary: invalidate the per-thread scan cache.
        note_jit_boundary();
        // P1 shadow record: the self-heal is a real (if belated) observation
        // that those frames have returned — same edge `pop_jit_entry` records,
        // once for the whole batch.
        thread_state::record_transition(
            leaving_compiled_state(remaining),
            "jit::conservative_roots::prune_returned_jit_entries",
        );
    }
    for _ in 0..pruned {
        GLOBAL_JIT_DEPTH.dec();
        cratonvm_gc::gc_quiescence::leave();
        cratonvm_jit::jit_execution_leave();
    }
    if pruned > 0 {
        publish_self_jit_depth(remaining);
    }
    if pruned > 0 {
        // `info!`, not `debug!`. Under `release_max_level_info` a
        // `debug!` is compiled out of every release binary, so a count of
        // LEAKED JIT entries -- an anomaly worth knowing about -- could
        // never reach anyone running a released VM.
        //
        // Promoted only after measuring the rate, because the objection to
        // promoting an anomaly line is that it might flood. It does not:
        // across the 88-vector regression suite, `GcStress` under ZGC /
        // Generational / G1 at -Xmx96m (3, 16 and several collections
        // respectively), and the GPU runtime-stress and residency-gc gates,
        // this fired ZERO times. That measures "does not flood"; it does
        // not prove the line can fire, which is true of any rare-anomaly
        // report and is still strictly better than invisible-by-
        // construction.
        tracing::info!(
            "pruned {} leaked JIT entry/entries (returned frames below scanner \
             SP {:#x}); quiescence healed to live count",
            pruned,
            scanner_sp,
        );
        // P1 code-cache retirement: the self-heal is the OTHER way
        // `GLOBAL_JIT_DEPTH` reaches zero. Without this wake-up a leaked
        // `JitEntryGuard` would wedge the retirement queue exactly as it used
        // to wedge the moving collector — and because retention is the
        // fail-safe, that would show up as unbounded code-cache growth rather
        // than as a crash. See `code_cache_lifecycle`'s §1.3.
        if crate::jit::code_cache_lifecycle::pending_retirements() != 0 {
            crate::jit::code_cache_lifecycle::sweep_if_quiescent();
        }
    }
    pruned
}

/// RAII guard that pairs `push_jit_entry` with `pop_jit_entry` on drop.
///
/// Use this at every JIT call site so a panic unwinding through the
/// transition still cleans up the entry chain:
///
/// ```ignore
/// let _guard = JitEntryGuard::enter();
/// let result = std::panic::catch_unwind(|| unsafe { compiled.try_call(args) });
/// // _guard drops here, popping the entry whether result is Ok or Err
/// ```
pub struct JitEntryGuard {
    /// Depth at the moment of construction; used as a sanity check on drop.
    depth_at_push: usize,
    active_class_id: Option<cratonvm_types::jit_activation::Activation>,
    /// Native-allocation unwind permission suspended for the duration of this
    /// compiled frame, restored on drop. A JIT frame carries no unwind
    /// information, so a panic raised beneath one would terminate the process
    /// instead of reaching the `catch_unwind` that would have converted it into
    /// a Java exception (the same constraint that makes `jit_throw_aioobe`
    /// signal through a thread-local instead of panicking). `0` — the case
    /// where no native call is in flight — means nothing was written and
    /// nothing needs restoring. See `crate::runtime::native_oom`.
    saved_native_unwind: u32,
}

impl JitEntryGuard {
    /// Push a new conservative JIT entry and return a guard that will
    /// pop it on drop. The entry has `precise = None` so the GC root
    /// walker uses the conservative range scan for this frame.
    ///
    /// **Inlined intentionally**: the SP capture must resolve to a probe in
    /// the caller's frame, not in this function's frame, otherwise the SP
    /// recorded in the chain would point into freed stack memory the moment
    /// `enter` returns.
    #[inline(always)]
    pub fn enter() -> Self {
        let sp = current_stack_pointer();
        let depth_at_push = push_jit_entry_at(sp);
        Self {
            depth_at_push,
            active_class_id: None,
            saved_native_unwind: crate::runtime::native_oom::suspend_for_jit(),
        }
    }

    /// NEW-12: push a JIT entry that carries precise-frame metadata.
    ///
    /// When the root walker encounters an entry of this shape it uses
    /// the compiled method's oop maps to enumerate oops exactly where
    /// available. Map-empty methods retain the same metadata so their
    /// conservative fallback can scan a bounded compiled-frame band.
    ///
    /// **Safety**: the caller must hold a live borrow of `cm` for the
    /// duration of the returned guard. In practice this is trivial:
    /// the interpreter's JIT call site owns `&CompiledMethod` and the
    /// guard is dropped immediately after the call returns. The
    /// compiled method itself is kept alive by the JIT cache's Arc
    /// holding, so even after the borrow ends the pointer remains
    /// valid for any in-flight GC walker.
    #[inline(always)]
    pub fn enter_with_compiled(cm: &cratonvm_jit::CompiledMethod) -> Self {
        Self::enter_with_compiled_at(cm, None)
    }

    /// [`Self::enter_with_compiled`] plus the interpreter depth this compiled
    /// frame sits at, so Java-level stack walks can place it (see
    /// [`JitFrameChainEntry::interp_depth`]). `None` inherits the enclosing
    /// entry's depth, which is what a JIT→JIT dispatch wants.
    #[inline(always)]
    pub fn enter_with_compiled_at(
        cm: &cratonvm_jit::CompiledMethod,
        interp_depth: Option<usize>,
    ) -> Self {
        // Retain frame metadata even when this method has no oop-map entries.
        // The prologue still records its RBP whenever precise maps are enabled,
        // and the conservative fallback can then scan this compiled frame's
        // bounded spill band instead of every intervening interpreter/Rust
        // frame. Map-empty entries simply contribute no exact slots.
        let sp = current_stack_pointer();
        let entry = JitFrameChainEntry {
            entry_sp: sp,
            interp_depth: match interp_depth {
                Some(d) => u32::try_from(d).unwrap_or(u32::MAX - 1),
                None => INTERP_DEPTH_INHERIT,
            },
            precise: Some(PreciseFrameInfo {
                compiled_method: cm as *const cratonvm_jit::CompiledMethod,
                frame_base: sp,
                entry_ptr: cm.entry_ptr(),
                exact_rbp: 0,
                exact_cm_id: 0,
            }),
        };
        let depth_at_push = push_entry_full(entry);
        Self {
            depth_at_push,
            // The artifact itself carries its declaring class, so marking it
            // active is a per-thread slot write — no map, no global lock.
            active_class_id: cratonvm_types::jit_activation::enter(cm.owner_class_id),
            saved_native_unwind: crate::runtime::native_oom::suspend_for_jit(),
        }
    }
}

impl Drop for JitEntryGuard {
    fn drop(&mut self) {
        // A compiled OSR body can leave through a non-local deopt/exception
        // path after entering another compiled body.  That nested bridge does
        // not always get a Rust frame to run its own guard's Drop, so merely
        // popping once here would remove the leaked child and strand *this*
        // guard on the chain.  Restore the depth that this guard established
        // first, then remove the guard itself.  The chain is thread-local and
        // stack-disciplined, so entries above `depth_at_push` can only be
        // abandoned descendants of this invocation; never touch outer frames.
        while current_thread_jit_depth() > self.depth_at_push {
            let _ = pop_jit_entry();
        }
        let popped = pop_jit_entry();
        debug_assert!(
            popped.is_some(),
            "JitEntryGuard::drop: chain underflow (was depth {})",
            self.depth_at_push
        );
        if let Some(activation) = self.active_class_id.take() {
            cratonvm_types::jit_activation::exit(activation);
        }
        // Restore the native-allocation unwind permission this compiled frame
        // suspended. `0` means the entry wrote nothing (no native call was in
        // flight), so the common pure-JIT path skips the write entirely.
        if self.saved_native_unwind != 0 {
            crate::runtime::native_oom::restore(self.saved_native_unwind);
        }
    }
}

// ---------------------------------------------------------------------------
// WS1 (kafka JIT throughput): per-thread JIT-scan cache
// ---------------------------------------------------------------------------
//
// `update_root_snapshot` runs on EVERY object-returning native call (twice:
// `safe_native_call` + `native_return_pushed_to_stack`) and folds in
// `scan_active_jit_frames`. The conservative range for a chain entry is
// `[scanner_sp, entry_sp]` — when a long-running JIT frame sits near the
// stack bottom (e.g. a compiled JUnit `withInterceptedStreams` lambda that
// transitively runs the whole test plan), that range spans the ENTIRE
// interpreter recursion above it, so every native call paid an O(megabytes)
// word-by-word stack scan. Measured: a 6 s interpreted kafka test class ran
// 60-100 s with one such frame live — cdb sampling put all wall time under
// `scan_one_frame` / `is_object_address`. This was the dominant mechanism
// behind "JIT-on is slower than the interpreter" on call-heavy suites.
//
// The cache: JIT spill slots can only change while compiled code executes.
// Control re-enters compiled code exclusively through (a) a JIT entry
// (`JitEntryGuard` push) or (b) a JIT runtime helper RETURNING — and before
// any subsequent snapshot can happen, Rust must first be re-entered through
// another helper call or the entry's drop. So a generation counter bumped at
// every chain mutation AND at every JIT runtime-helper entry
// (`note_jit_boundary`) precisely tracks "spills may have changed".
// Between bumps, the previous scan's roots are reused verbatim.
//
// Soundness of reuse across differing `scanner_sp`: all live spill slots lie
// within `[innermost-helper-entry SP, entry_sp]`, and the cached scan's range
// covered that band (its scanner_sp was at or below the helper frame). Words
// outside the band are interpreter/Rust junk — including or omitting them
// only perturbs conservative over-retention, never drops a real JIT root.
//
// ⚠ CAVEAT (the words "outside the band" are NOT always junk): the scanned band
// also covers the interpreter / native / Rust stack of any callee a JIT method
// invoked, which mutates while that callee runs WITHOUT a boundary bump and can
// hold the only live reference to a freshly-allocated object (a reflection
// `Field[]`, a `StringBuilder` char[] in a native's Rust local, ...). Reusing
// the snapshot across such mutation therefore CAN drop a real root. That is
// tolerable for the cache's hot, best-effort purpose, but every
// GC-AUTHORITATIVE root scan must first discard the snapshot via
// [`invalidate_scan_cache_for_gc`] so it re-scans fresh. See that function's
// doc for the corruption this prevents.
// Address stability: while any JIT frame is live, `gc_quiescence` forces the
// NON-MOVING young sweep, so cached `ObjectRef` addresses cannot be
// relocated. The two diagnostic modes that lift that guarantee
// (`CRATONVM_DBG_FORCE_MOVING`, `CRATONVM_SHADOW_STACK`) disable the cache,
// as does `CRATONVM_NO_JIT_SCAN_CACHE` (bisection).

thread_local! {
    /// Monotonic count of Rust↔JIT boundary crossings on this thread.
    static JIT_BOUNDARY_GEN: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
    static JIT_SCAN_CACHE: std::cell::RefCell<JitScanCache> =
        const { std::cell::RefCell::new(JitScanCache::empty()) };
}

struct JitScanCache {
    /// Generation the cached roots were scanned at (`u64::MAX` = never).
    filled_gen: u64,
    chain_len: usize,
    /// Identity of the heap the cached `roots` were filtered against —
    /// `heap as *const VmHeap as usize`, the same key
    /// `memory/smuggled_longs.rs` uses and for the same reason (a raw heap
    /// address is only meaningful against the heap that produced it, and this
    /// scan path has a `&VmHeap` and nothing else in scope).
    ///
    /// Without it this thread-local was the one cache in `vm/src/jit/` that
    /// holds raw heap addresses with no VM key at all. A thread that reaches
    /// two VMs (JNI `AttachCurrentThread`, or a test thread reused across
    /// `SharedVm`s) fills the cache from `heap_a.is_object_address` and, if
    /// `(filled_gen, chain_len, collection_count)` happen to match on the
    /// other side, hands VM A's object addresses to VM B's collector as
    /// roots — the `oscache` failure mode from
    /// `vm-process-global-state.md`, but pointed at the mark
    /// phase. `collection_count` cannot stand in for this: it is
    /// `heap.collection_count()`, a *different* counter per heap, so two young
    /// heaps trivially agree on it.
    ///
    /// `usize::MAX` = never filled.
    heap_id: usize,
    /// Heap collection count the roots were scanned at. The boundary
    /// generation tracks JIT *spill* mutation, but the cached `roots` are raw
    /// object ADDRESSES — a garbage collection (which does NOT bump the
    /// boundary generation) can free or relocate them, leaving the cache
    /// pointing at reclaimed slots. Several young sweeps run at the SAME
    /// generation during one interpreted callee (the boundary only bumps when
    /// compiled code re-executes), so without this key the cache republishes
    /// freed addresses into the root snapshot; feeding those back as roots
    /// makes the next mark phase traverse garbage headers → the "implausible
    /// object size" sweep abort. Invalidate whenever a collection has occurred.
    collection_count: u64,
    roots: Vec<ObjectRef>,
}

impl JitScanCache {
    const fn empty() -> Self {
        Self {
            filled_gen: u64::MAX,
            chain_len: usize::MAX,
            heap_id: usize::MAX,
            collection_count: u64::MAX,
            roots: Vec::new(),
        }
    }

    /// The complete cache-hit predicate. Every component must match; in
    /// particular `heap_id`, without which this thread-local republishes one
    /// heap's addresses into another heap's root set. Factored out of
    /// `scan_active_jit_frames` so the keying is unit-testable without a live
    /// VM, heap or JIT frame.
    #[inline]
    fn matches(&self, gen: u64, chain_len: usize, heap_id: usize, collection_count: u64) -> bool {
        self.filled_gen == gen
            && self.chain_len == chain_len
            && self.heap_id == heap_id
            && self.collection_count == collection_count
    }
}

/// Identity of the heap a [`JitScanCache`] fill was filtered against.
///
/// `heap as *const VmHeap as usize` — the `heap_id` convention introduced by
/// `memory/smuggled_longs.rs` in the round-1 process-global sweep. `VmHeap` is
/// a by-value field of `HeapRealm` inside `Arc<SharedVm>`, so its address is
/// stable for the VM's life and distinct from every other *live* heap's. The
/// documented residual is the same one: an allocator that reuses a dropped
/// `VmHeap`'s address for a new one. That is harmless here in a way it is not
/// there — the recycled address can only produce a hit if `filled_gen`,
/// `chain_len` AND `collection_count` also match, and a fresh heap's
/// `collection_count` starts at 0 while the cache is only ever filled from a
/// thread with a non-empty JIT chain.
#[inline]
fn jit_scan_cache_heap_id(heap: &VmHeap) -> usize {
    heap as *const VmHeap as usize
}

/// Record a Rust↔JIT boundary crossing: called at every JIT runtime-helper
/// entry and at every JIT entry-chain mutation. Invalidates the JIT-scan
/// cache (next `scan_active_jit_frames` rescans).
#[inline]
pub fn note_jit_boundary() {
    JIT_BOUNDARY_GEN.with(|g| g.set(g.get().wrapping_add(1)));
}

/// Force the next [`scan_active_jit_frames`] on this thread to re-scan rather
/// than reuse the cached root set. **Must be called before every GC-authoritative
/// root collection** (the current thread's `collect_roots`, and a parked thread's
/// pre-STW `update_root_snapshot` publish).
///
/// ## Why this is required (the JIT-scan-cache soundness gap)
///
/// The cache (see the `JIT_SCAN_CACHE` module comment) reuses the previous
/// conservative scan's roots whenever the boundary generation is unchanged,
/// on the premise that "JIT spill slots can only change while compiled code
/// executes" — i.e. only at a [`note_jit_boundary`] crossing.
///
/// That premise is incomplete. The conservative scanner walks the WHOLE band
/// `[scanner_sp, entry_sp]`, which also covers the **interpreter / native /
/// Rust stack BELOW the JIT frame** (an interpreted callee a JIT method invoked,
/// and the native helpers it in turn calls). That region mutates continuously
/// while interpreted/native code runs — *without* any boundary bump — and it can
/// hold the only live reference to a freshly-allocated object (e.g. a reflection
/// `Field[]` or a `StringBuilder` char[] held in a callee's frame / a native's
/// Rust local before it is stored into a tracked slot). `update_root_snapshot`
/// runs on every object-returning native call and fills the cache at that same
/// generation, so a subsequent GC root scan at the same generation reuses a
/// snapshot that PREDATES those new references and silently drops them. With the
/// non-moving young sweep (forced while any JIT frame is live), a dropped live
/// root is reclaimed and its slot reused — heap corruption (observed as the
/// "inconsistent header / implausible object size" sweep abort on reflection-
/// heavy JIT workloads under GC stress).
///
/// The cache stays correct for its hot purpose (cheap repeated snapshots between
/// native calls); we only need the *authoritative* GC root scans to be fresh.
/// Bumping the generation here discards the stale snapshot so the immediately
/// following `scan_active_jit_frames` performs a full, current scan.
///
/// ## H2-CID0 (2026-08-05) — there are TWO caches here, and this used to reset one
///
/// [`UNREG_JIT_MEMO`] memoizes the *other* per-native-call scan: the detection
/// of a JIT frame that is live without having pushed an entry guard. It is not
/// keyed on the boundary generation, so bumping the generation left it intact —
/// and it is the cache that decides whether such a frame's oops are marked at
/// all. Miss the frame and the young non-moving sweep frees objects it alone
/// holds.
///
/// The argument above applies to it verbatim, and more sharply: a stale root
/// snapshot drops references, while a stale unregistered-frame verdict drops
/// an entire frame's worth of them AND leaves the collector believing it may
/// relocate. Reset it here so the authoritative scan cannot inherit a verdict
/// about a stack that has since been rewritten.
///
/// Cost is one full band rescan per GC-authoritative root collection, not per
/// native call — which is exactly the trade this function already documents.
/// The `UnregMemo::hiwater` rule still does the between-collections tightening;
/// it cannot replace this, because it only reacts to stack-pointer rises it
/// happens to OBSERVE, and a return-and-re-descend that falls entirely between
/// two snapshots is invisible to it (measured: 972 suppressed detections in one
/// run with hi-water enabled).
///
/// `CRATONVM_JIT_UNREG_MEMO_GC_RESET=0` restores the pre-fix behaviour (the
/// memo surviving authoritative scans) so the fix can be A/B'd against the
/// failure in one binary. Without a switch this would be the only change here
/// that could never be shown to work.
#[inline]
pub fn invalidate_scan_cache_for_gc() {
    note_jit_boundary();
    // Mark the following scan as one a collector will consume, so the audit can
    // attribute a suppression to the path where it is a defect. Set even when
    // the reset is disabled — that is exactly the arm whose count we want.
    UNREG_JIT_AUTHORITATIVE.with(|c| c.set(true));
    if unreg_memo_gc_reset_enabled() {
        UNREG_JIT_MEMO.with(|c| c.set(UnregMemo::new()));
    }
}

/// `CRATONVM_JIT_UNREG_MEMO_GC_RESET=0` — kill switch for the authoritative
/// reset above.
fn unreg_memo_gc_reset_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        !matches!(
            cratonvm_types::flags::runtime_var("CRATONVM_JIT_UNREG_MEMO_GC_RESET").as_deref(),
            Ok("0") | Ok("false") | Ok("off")
        )
    })
}

/// `CRATONVM_DBG_JIT_SCAN_PROF` — exit-time tally for the JIT root scan.
///
/// [`scan_active_jit_frames`] runs on **every object-returning native call**
/// (through `update_root_snapshot`), and whenever any chain entry is
/// conservative it blind-scans the whole band `[scanner_sp, max entry_sp)`.
/// That band spans the *interpreted callee tree below* the compiled frame, so
/// its width tracks Java stack depth rather than the compiled method's own
/// frame — a JUnit stack is 35-70 frames deep where a microbenchmark's is
/// three, which is exactly the difference between the two workloads where the
/// JIT wins and where it loses.
///
/// Nothing reported how often that path runs or how many words it reads, so
/// "the JIT costs +83% CPU on `ZipContentTests`" could be neither attributed
/// to it nor cleared of it. These counters are that attribution: `band_words`
/// against the run's CPU time is the whole question, and `cache_hits` against
/// `scans` says whether the boundary-generation key — bumped by
/// [`note_jit_boundary`] at *every* Rust↔JIT crossing — leaves the cache able
/// to hit at all.
///
/// **`cache_hits` MEASURED 2026-08-12: 0.0%, in every run.** Three netty
/// `io.netty.buffer` classes on dev `6d1bfd531` — 1,022 scans, 41,894 scans and
/// 60,163 scans respectively — and **zero** hits in all three. The
/// boundary-generation key is bumped from `push_entry_full`, which ran 826
/// million times in one of those runs, so the generation never survives long
/// enough for a second scan to match it. The answer is 0%, not "small".
///
/// That is not the cost on those classes — `band_words` was **0**, i.e. no band
/// scanning fired at all — so this is recorded as a fact about the cache rather
/// than as a lead. Anything that reworks the key should know it starts from
/// zero. See `docs/known-issues/netty/adaptive-bytebuf-allocator-throughput-20260812.md`.
///
/// Off by default and read through one cached bool, so a default run pays a
/// predictable branch per scan and nothing else.
pub mod scan_prof {
    use std::sync::atomic::{AtomicU64, Ordering};

    pub static SCANS: AtomicU64 = AtomicU64::new(0);
    pub static CACHE_HITS: AtomicU64 = AtomicU64::new(0);
    pub static BAND_SCANS: AtomicU64 = AtomicU64::new(0);
    pub static BAND_WORDS: AtomicU64 = AtomicU64::new(0);
    pub static BAND_MAX_BYTES: AtomicU64 = AtomicU64::new(0);
    pub static PRECISE_FRAMES: AtomicU64 = AtomicU64::new(0);
    /// Transfers of control into compiled code (`push_entry_full`). The run's
    /// interpreter→JIT entry count; the JIT's CPU delta divided by this is the
    /// per-entry cost.
    ///
    /// **MEASURED 2026-08-12** on netty `io.netty.buffer`, dev `6d1bfd531`, and
    /// the answer to the question this counter was added for. On
    /// `AdaptiveByteBufAllocatorTest`: **826,764,658 entries in a 551 s run**
    /// (1.5 M/s), against 1.59 M tracked method invocations. A flat `perf`
    /// profile of the same workload puts the entry/exit bookkeeping — this
    /// function, `pop_jit_entry`, `pin_jit_code_range_owner`,
    /// `validate_code_ptr`, `gc_quiescence::{enter,leave}`,
    /// `record_transition`, `jit_execution_{enter,leave}` — at **~15% of CPU**,
    /// i.e. **~200 ns of pure bookkeeping per entry**, with another ~14% in the
    /// dispatch that reaches it and 3.3% in the interpreter itself.
    ///
    /// So yes: on call-dense code the entry machinery is a first-order cost,
    /// and it is paid per *transfer*, not per compiled method. Wall time tracks
    /// entry count across the family — 116.8 M entries/35.3 s, 120.4 M/62.3 s,
    /// 826.8 M/551 s. Full write-up, including the two hypotheses this refuted,
    /// in `docs/known-issues/netty/adaptive-bytebuf-allocator-throughput-20260812.md`.
    pub static JIT_ENTRIES: AtomicU64 = AtomicU64::new(0);

    pub fn enabled() -> bool {
        static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        *ON.get_or_init(|| {
            cratonvm_types::flags::runtime_var_os("CRATONVM_DBG_JIT_SCAN_PROF").is_some()
        })
    }

    #[inline]
    pub fn bump(ctr: &'static AtomicU64) {
        if enabled() {
            ctr.fetch_add(1, Ordering::Relaxed);
        }
    }

    #[inline]
    pub fn add(ctr: &'static AtomicU64, n: u64) {
        if enabled() {
            ctr.fetch_add(n, Ordering::Relaxed);
        }
    }

    #[inline]
    pub fn observe_max(ctr: &'static AtomicU64, n: u64) {
        if enabled() {
            ctr.fetch_max(n, Ordering::Relaxed);
        }
    }

    /// Exit-time line. Prints nothing unless the flag is set, so a run that
    /// never asks stays silent rather than reporting zeros that read as a
    /// measured absence.
    pub fn dump() {
        if !enabled() {
            return;
        }
        let g = |c: &AtomicU64| c.load(Ordering::Relaxed);
        let scans = g(&SCANS);
        let hits = g(&CACHE_HITS);
        let hit_pct = if scans == 0 {
            0.0
        } else {
            100.0 * hits as f64 / scans as f64
        };
        eprintln!(
            "[cratonvm] JIT scan prof: scans={scans} cache_hits={hits} ({hit_pct:.1}%) \
             band_scans={band} band_words={words} band_max_bytes={maxb} \
             precise_frames={precise} jit_entries={entries}",
            entries = g(&JIT_ENTRIES),
            band = g(&BAND_SCANS),
            words = g(&BAND_WORDS),
            maxb = g(&BAND_MAX_BYTES),
            precise = g(&PRECISE_FRAMES),
        );
    }
}

fn jit_scan_cache_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        // Cache ON by default. (An earlier change here disabled it as a
        // supposed fix for the `ReflRepro` GC corruption — that was a
        // MISDIAGNOSIS: disabling the cache only perturbed GC timing enough to
        // mask the crash on one base, and does not fix it on current dev. The
        // real bug is a register-resident JIT root that conservative scanning —
        // cached, fresh, or even whole-stack (`CRATONVM_DBG_FULLSTACK_SCAN`) —
        // cannot see; it needs precise oop maps / the shadow stack. See
        // `bug-06b-jit-scan-cache-unsound.md`.) So the
        // cache stays enabled for its perf benefit; `collection_count` keying
        // (see `JitScanCache`) keeps it from republishing freed addresses across
        // a GC. `CRATONVM_NO_JIT_SCAN_CACHE` force-disables it for bisection.
        cratonvm_types::flags::runtime_var_os("CRATONVM_NO_JIT_SCAN_CACHE").is_none()
            && cratonvm_types::flags::runtime_var_os("CRATONVM_DBG_FORCE_MOVING").is_none()
            && cratonvm_types::flags::runtime_var_os("CRATONVM_SHADOW_STACK").is_none()
    })
}

/// Cached `CRATONVM_DBG_NO_PRUNE` gate. `scan_active_jit_frames` runs on every
/// object-returning native call (via `update_root_snapshot`); the previous
/// uncached `env::var_os` was a `GetEnvironmentVariableW` syscall per native
/// call — a kernel transition on the hottest dispatch path. Cached once.
fn dbg_no_prune() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| cratonvm_types::flags::runtime_var_os("CRATONVM_DBG_NO_PRUNE").is_some())
}

/// H2-CID0 (2026-08-05) — times the unregistered-JIT-frame memo said "clean"
/// while a real scan of the same range found a frame.
///
/// Only counted under `CRATONVM_DBG_UNREG_MEMO_AUDIT=1`. Non-zero means the
/// memo suppressed a detection that was true: that frame's oops went
/// unmarked and the collector was never told to avoid moving them.
pub static UNREG_MEMO_SUPPRESSED: AtomicUsize = AtomicUsize::new(0);
/// Times the memo short-circuited a scan at all (the audit's denominator — a
/// zero numerator is only meaningful beside a non-zero denominator).
pub static UNREG_MEMO_SHORTCIRCUITS: AtomicUsize = AtomicUsize::new(0);

/// ENGAGEMENT census for the A5 band probe, printed at exit under
/// `CRATONVM_DBG_A5_ENGAGEMENT=1`.
///
/// Four earlier attempts to make this probe cheaper were judged inert from a
/// profile that did not move. That is an inference, not a measurement: a memo
/// that never engages and a memo that engages but saves nothing look identical
/// in a flat profile. These say which. Counted per CALL of the coverage probe.
pub static A5_PROBE_CALLS: AtomicUsize = AtomicUsize::new(0);
/// Calls where the memo would have answered "already clean" (no scan needed).
pub static A5_PROBE_MEMO_CLEAN: AtomicUsize = AtomicUsize::new(0);
/// Calls where the memo could bound the scan to an incremental band.
pub static A5_PROBE_MEMO_BANDED: AtomicUsize = AtomicUsize::new(0);
/// Calls that fell through to a FULL scan because the code-range set changed.
pub static A5_PROBE_FULL_RESCAN: AtomicUsize = AtomicUsize::new(0);
/// Total band words the probe was asked to scan (the quantity a memo shrinks).
pub static A5_PROBE_WORDS: AtomicUsize = AtomicUsize::new(0);

pub fn a5_engagement_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        cratonvm_types::flags::runtime_var_os("CRATONVM_DBG_A5_ENGAGEMENT").is_some()
    })
}

/// Print the A5 FALLBACK census — how many moving-young cycles the A5 probe
/// diverted, and how many of those it diverted on a word that was not a live
/// frame at all.
///
/// Deliberately NOT folded into [`report_a5_engagement`], which answers a
/// different question ("does the memo engage") on a different flag. And
/// deliberately called from BOTH VM exit arms rather than only the `Ok(())`
/// one: the run this census exists to explain is a run whose test FAILED, and
/// on that arm `report_a5_engagement` never executes. A census that prints
/// only when nothing went wrong is a census of the uninteresting population.
/// `Once`-guarded, so having it on both arms is correct.
pub fn report_a5_fallback_census() {
    #[cfg(any(target_os = "windows", target_os = "linux"))]
    {
        static ONCE: std::sync::Once = std::sync::Once::new();
        ONCE.call_once(|| {
            if !a5_fallback_dbg() {
                return;
            }
            let (hits, residue, shapeless, filtered) = a5_fallback_census();
            eprintln!(
                "[a5-fallback] TOTALS hits={hits} residue={residue} shapeless={shapeless} \
                 filtered={filtered} residue_filter={} shape_filter={}",
                a5_residue_filter_enabled(),
                a5_shape_filter_enabled(),
            );
        });
    }
}

/// Print the A5 engagement census. Called from the VM's shutdown path.
pub fn report_a5_engagement() {
    if !a5_engagement_enabled() {
        return;
    }
    let calls = A5_PROBE_CALLS.load(Ordering::Relaxed);
    if calls == 0 {
        eprintln!("[a5-engagement] calls=0 (probe never ran)");
        return;
    }
    eprintln!(
        "[a5-engagement] calls={calls} memo_clean={} memo_banded={} full_rescan={} words={}",
        A5_PROBE_MEMO_CLEAN.load(Ordering::Relaxed),
        A5_PROBE_MEMO_BANDED.load(Ordering::Relaxed),
        A5_PROBE_FULL_RESCAN.load(Ordering::Relaxed),
        A5_PROBE_WORDS.load(Ordering::Relaxed),
    );
}
/// H2-CID0 (2026-08-06) — suppressions on a scan a collector CONSUMES.
///
/// The number that judges the fix. Total suppressions are dominated by
/// ordinary per-native-call snapshots, which the memo is supposed to
/// short-circuit and which no collector marks from; a suppression there is the
/// optimisation working. A suppression HERE is a live JIT frame's oops missing
/// from the root set a collection is about to use.
///
/// Expected exactly 0 with the authoritative reset enabled, and that is a
/// measurement of "impossible by construction", not a restatement of it.
pub static UNREG_MEMO_SUPPRESSED_AUTHORITATIVE: AtomicUsize = AtomicUsize::new(0);

/// `CRATONVM_DBG_UNREG_MEMO_AUDIT=1` — verify the memo instead of trusting it.
///
/// Runs the detection scan even on the fast path and only COUNTS the
/// disagreement. It marks nothing and changes no collector decision, so unlike
/// most instruments on this bug it cannot perturb the thing it measures; the
/// only cost is the scan itself.
fn dbg_unreg_memo_audit() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        cratonvm_types::flags::runtime_var_os("CRATONVM_DBG_UNREG_MEMO_AUDIT").is_some()
    })
}

/// H2-CID0 (2026-08-05) — the unregistered-JIT-frame memo, as a value.
///
/// Stack addresses grow DOWN, so "deeper" is numerically smaller and the
/// verified region is `[floor, stack_high)`.
///
/// * `verified_lo` — the depth at which a scan last came back clean.
/// * `hiwater` — the SHALLOWEST stack pointer observed since that scan.
///
/// The memo's justification is "nothing above our current stack pointer can
/// change while we are nested below it". Rising to `hiwater` pops every frame
/// below it, so only `[hiwater, stack_high)` is provably untouched; combined
/// with the scan's own reach the clean floor is `max(verified_lo, hiwater)`.
///
/// Using `verified_lo` alone — the pre-fix rule — claims the verdict holds for
/// the whole life of the thread, because `verified_lo` only ever moves deeper.
/// A thread that returns above it, enters an already-compiled method (no new
/// code range, no entry guard, so neither existing staleness check fires) and
/// descends again never scans that band again.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct UnregMemo {
    verified_lo: usize,
    verified_ranges: usize,
    hiwater: usize,
}

/// What the memo says to do for a root scan at `search_lo`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum UnregScan {
    /// The range is covered by a still-valid earlier verdict.
    AlreadyClean,
    /// Detect over `[search_lo, hi)`; `hi` is `stack_high` for a full rescan.
    Detect { hi: Option<usize> },
}

impl UnregMemo {
    const fn new() -> Self {
        Self {
            verified_lo: usize::MAX,
            verified_ranges: 0,
            hiwater: 0,
        }
    }

    /// Clean floor: the deepest address the memo still vouches for.
    fn floor(&self, hiwater_on: bool) -> usize {
        if hiwater_on {
            self.verified_lo.max(self.hiwater)
        } else {
            self.verified_lo
        }
    }

    /// Record this observation and decide what to scan.
    ///
    /// `Detect { hi: Some(floor) }` is the incremental band — only the stack
    /// that has appeared (or been rewritten) since the last clean verdict.
    fn observe(&mut self, search_lo: usize, code_ranges: usize, hiwater_on: bool) -> UnregScan {
        if hiwater_on {
            self.hiwater = self.hiwater.max(search_lo);
        }
        let floor = self.floor(hiwater_on);
        if code_ranges == self.verified_ranges {
            if search_lo >= floor {
                return UnregScan::AlreadyClean;
            }
            return UnregScan::Detect { hi: Some(floor) };
        }
        // A new compilation invalidates the verdict outright: a slot that held
        // a plain value at the last scan can now sit where a new JIT code range
        // claims to start.
        //
        // MEASURED 2026-08-11, and left alone deliberately. This rule can be
        // argued away — the band is frozen (nothing above the current stack
        // pointer changes while the thread is nested below it) and a genuine
        // return address into range R requires R to have existed when the CALL
        // wrote it, so a range registered after the verdict can only produce a
        // FALSE positive. Removing the check was implemented, unit-tested and
        // measured on `DefaultCatalogAndSchemaTest`, which compiles
        // continuously across 132 SessionFactory bootstraps and so keeps this
        // memo permanently cold: `native_stack_has_jit_frame` read 35.2 BILLION
        // stack words with the check and 35.2 billion without it — byte for
        // byte no change, because the probes that dominate this workload come
        // from `refresh_moving_young_coverage_for_current_thread`, which
        // consults no memo at all. So the rule is a real inefficiency and it is
        // NOT the binding one; it stays until something measures it binding,
        // rather than trading heap-safety-critical behaviour for nothing.
        UnregScan::Detect { hi: None }
    }

    /// A scan starting at `search_lo` came back clean.
    fn mark_clean(&mut self, search_lo: usize, code_ranges: usize) {
        self.verified_lo = search_lo;
        self.verified_ranges = code_ranges;
        // The verdict is fresh as of here, so the peak restarts from this
        // depth — otherwise an old peak would force rescans forever.
        self.hiwater = search_lo;
    }
}

/// `CRATONVM_JIT_UNREG_MEMO_HIWATER=0` — restore the pre-fix memo (kill switch
/// for A/B-ing the fix in one binary).
fn unreg_memo_hiwater_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        !matches!(
            cratonvm_types::flags::runtime_var("CRATONVM_JIT_UNREG_MEMO_HIWATER").as_deref(),
            Ok("0") | Ok("false") | Ok("off")
        )
    })
}

thread_local! {
    /// Is the running `scan_active_jit_frames` the COLLECTION's own root pass,
    /// rather than one of the per-native-call snapshot publishers?
    ///
    /// The above-chain conservative band is affordable once per collection and
    /// not once per native call, and only the collection's pass is what the
    /// collector marks from — so the two need telling apart. Set by
    /// [`scan_active_jit_frames_for_collection`], which is the entry point
    /// `memory::roots::collect_roots` uses.
    static GC_ROOT_PASS: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// True while the collection's own root pass is running on this thread.
#[inline]
pub fn gc_root_pass_active() -> bool {
    GC_ROOT_PASS.with(|c| c.get())
}

/// [`scan_active_jit_frames`] as the COLLECTION's root pass — the entry point
/// `memory::roots::collect_roots` calls, and the only one that scans the band
/// above the JIT entry chain. Restores the previous value rather than clearing,
/// so a nested call (there is none today) cannot silently downgrade the outer
/// pass.
pub fn scan_active_jit_frames_for_collection(heap: &VmHeap, out: &mut Vec<ObjectRef>) {
    let prev = GC_ROOT_PASS.with(|c| c.replace(true));
    scan_active_jit_frames(heap, out);
    GC_ROOT_PASS.with(|c| c.set(prev));
}

/// Engagement census for the above-chain conservative band.
///
/// A band that is never scanned and a band that is scanned and finds nothing
/// read the same way in a passing run, and this repair exists because a page
/// spent a day on an instrument that could not tell those apart. `passes` is
/// the denominator, `roots` what the band contributed, `bytes` how much stack
/// it had to read to get them.
pub mod above_chain {
    use std::sync::atomic::{AtomicU64, Ordering};

    pub static PASSES: AtomicU64 = AtomicU64::new(0);
    pub static ROOTS: AtomicU64 = AtomicU64::new(0);
    pub static BYTES: AtomicU64 = AtomicU64::new(0);

    pub(super) fn note(roots: usize, bytes: usize) {
        PASSES.fetch_add(1, Ordering::Relaxed);
        ROOTS.fetch_add(roots as u64, Ordering::Relaxed);
        BYTES.fetch_add(bytes as u64, Ordering::Relaxed);
    }

    /// `(passes, roots, bytes)` — for the shutdown census.
    pub fn census() -> (u64, u64, u64) {
        (
            PASSES.load(Ordering::Relaxed),
            ROOTS.load(Ordering::Relaxed),
            BYTES.load(Ordering::Relaxed),
        )
    }
}

/// OPT-IN gate for the above-chain conservative band
/// (`CRATONVM_JIT_ABOVE_CHAIN_SCAN=1`), and it is opt-in because it was
/// MEASURED not to be the fix it was written as.
///
/// The hypothesis was that the BindableTests ByteBuddy reclaim's missed root
/// lives in `[max(entry_sp), stack_high)` — the VM's own Rust frames between
/// the outermost interpreter entry and the compiled call — because
/// `CRATONVM_DBG_FULLSTACK_SCAN=1` cures that failure and its only difference
/// from the normal path is scanning the whole stack. It is not, or not only:
/// with this band walked on every collection (4978 passes, 3 056 492 roots,
/// 1.0 GB of stack read) the failure is unchanged, and so is it with the
/// widest possible CHAIN band (`CRATONVM_NO_PRECISE_JIT_MAPS=1`) and with the
/// A5 filter's veto removed (`CRATONVM_JIT_UNREG_ACCEPT_RESIDUE=1`). What is
/// left of the fullstack diagnostic's difference is that it also scans on the
/// per-native-call snapshot publishers — see [`above_chain_all_paths`].
///
/// Kept, default off, as the lever that reading measures rather than a fix:
/// a default-on gigabyte of stack reads per run buys nothing demonstrated.
/// See
/// `docs/internal/springboot/bindabletests-bytebuddy-receiver-reclaimed-under-gc-stress-20260908.md`.
#[cfg(any(target_os = "windows", target_os = "linux"))]
fn above_chain_scan_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        cratonvm_types::flags::runtime_var_os("CRATONVM_JIT_ABOVE_CHAIN_SCAN").is_some()
    })
}

/// `CRATONVM_JIT_ABOVE_CHAIN_ALL_PATHS=1` — also walk the band on the
/// per-native-call snapshot publishers, not only on the collection's own root
/// pass.
///
/// This is the difference between the band repair and
/// `CRATONVM_DBG_FULLSTACK_SCAN`, and it is a flag rather than a default
/// because it is the expensive half: `update_root_snapshot` runs on every
/// object-returning native call, and paying a live-stack walk there is what
/// makes the fullstack diagnostic 2-3x slower than the run it is diagnosing.
/// Kept so the two can be told apart by measurement instead of argument.
#[cfg(any(target_os = "windows", target_os = "linux"))]
fn above_chain_all_paths() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        cratonvm_types::flags::runtime_var_os("CRATONVM_JIT_ABOVE_CHAIN_ALL_PATHS").is_some()
    })
}

/// `CRATONVM_JIT_ABOVE_CHAIN_FROM_SP=1` — start the band at the SCANNER's SP
/// rather than at the top of the JIT entry chain.
///
/// The chain scan is supposed to cover `[scanner_sp, max(entry_sp))` already,
/// but only for entries with no precise oop map: a PRECISE entry contributes
/// its map's slots and no band at all. This makes the two hypotheses
/// separable without also changing which paths scan.
#[cfg(any(target_os = "windows", target_os = "linux"))]
fn above_chain_from_sp() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        cratonvm_types::flags::runtime_var_os("CRATONVM_JIT_ABOVE_CHAIN_FROM_SP").is_some()
    })
}

/// `CRATONVM_DBG_ABOVE_CHAIN_KB=<n>` — cap the above-chain band at `n` KiB.
///
/// Diagnostic only, and the reason it exists: the band's upper bound is the
/// thread's stack top, so "the fix works" says nothing about WHERE the missed
/// root was. Bisecting `n` until the failure returns names the depth, which is
/// the first step in replacing this conservative band with a precise root.
#[cfg(any(target_os = "windows", target_os = "linux"))]
fn above_chain_scan_cap_bytes() -> Option<usize> {
    static CAP: std::sync::OnceLock<Option<usize>> = std::sync::OnceLock::new();
    *CAP.get_or_init(|| {
        cratonvm_types::flags::runtime_var("CRATONVM_DBG_ABOVE_CHAIN_KB")
            .ok()
            .and_then(|v| v.parse::<usize>().ok())
            .filter(|&kb| kb > 0)
            .map(|kb| kb * 1024)
    })
}

/// Cached `CRATONVM_DBG_FULLSTACK_SCAN` gate, same per-native-call hot-path
/// rationale as [`dbg_no_prune`].
///
/// Its doc comment and a `#[cfg(any(windows, linux))]` used to sit ~200 lines
/// above, orphaned where the function had been before it moved — so the
/// attribute landed on [`UNREG_MEMO_SUPPRESSED`] instead, cfg-gating a counter
/// that `vm-cli` reads unconditionally. Both are reunited with the function
/// here; the gate itself needs no cfg (the flag is read on every target, only
/// its CONSUMER in `scan_active_jit_frames` is windows/linux).
fn dbg_fullstack_scan() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        cratonvm_types::flags::runtime_var_os("CRATONVM_DBG_FULLSTACK_SCAN").is_some()
    })
}

/// A5 fix — scan this thread's native stack band `[lo, hi)` for any word that is
/// an address strictly *inside* a live JIT code range
/// (`cratonvm_jit::lookup_jit_code_range`), i.e. a return address into compiled
/// code. A hit proves a JIT method's frame
/// is on the stack even if it pushed no `JitEntryGuard`. Early-exits on the
/// first hit. Bounded like [`scan_one_frame`] so a stale `hi` cannot run into
/// unmapped pages.
///
/// Returns the `(slot address, word)` of the first hit so the caller's
/// diagnostic can name the actual evidence — "some word somewhere looked like
/// compiled code" is not something a reader can act on, and this probe's own
/// doc comment names false-positive reduction as the follow-up work.
#[cfg(any(target_os = "windows", target_os = "linux"))]
fn native_stack_has_jit_frame(lo: usize, hi: usize) -> Option<(usize, usize)> {
    // PERF (TC0622 startup): snapshot the JIT code ranges ONCE (single table
    // lock) into a reusable thread-local buffer, then binary-search each stack
    // word lock-free. The previous code called `lookup_jit_code_range` per word,
    // locking a global `Mutex` up to ~1M times per scan — and this runs on the
    // per-native-call root-snapshot path, so it dominated Tomcat `start()`
    // (~66% of CPU). The hit/no-hit result is identical: ranges are disjoint,
    // so `start <= w < end` for the greatest `start <= w` is exact.
    // Opt-out safety net for the gauntlet soak: `CRATONVM_JIT_RANGE_SCAN_LEGACY=1`
    // restores the original per-word `lookup_jit_code_range` path (one global
    // Mutex lock per stack word). The default (fast) path is behavior-identical
    // — disjoint ranges make the binary-search membership test exact — so this
    // gate exists purely so a soak can A/B and revert without a rebuild.
    if jit_range_scan_legacy() {
        let mut addr = (lo + 7) & !7usize;
        const MAX_SCAN_BYTES: usize = 8 * 1024 * 1024;
        let hi = hi.min(addr.saturating_add(MAX_SCAN_BYTES));
        while addr + 8 <= hi {
            // SAFETY: see the fast path below.
            let w = unsafe { (addr as *const usize).read() };
            if let Some(cm_ptr) = cratonvm_jit::lookup_jit_code_range(w) {
                // A raw compiled-entry function pointer is routinely kept in
                // Rust helper locals.  It is data, not a native return address,
                // and treating it as a frame forces every precise moving-GC
                // cycle into the conservative fallback.  A genuine return PC
                // is always after the method entry instruction.
                let cm = unsafe { &*(cm_ptr as *const cratonvm_jit::CompiledMethod) };
                let entry = cm.entry_ptr() as usize;
                if w != entry && is_plausible_return_pc(w) {
                    return Some((addr, w));
                }
            }
            addr += 8;
        }
        return None;
    }
    // PERF (2026-07-15, round 2 of the RequestMappingMessageConversionIntegrationTests
    // bootstrap-slowness investigation): the thread-local buffer below used to be
    // rebuilt (full table lock + Vec copy + `sort_unstable()` over every
    // registered JIT code range) on EVERY call to this function, even though
    // nothing had changed since the previous call — a write-only "cache" in
    // name only. This function runs on the per-native-call root-snapshot path,
    // so that cost was paid on every native call, and it grew as the process
    // JIT-compiled more code (the registered range set only grows — see
    // `JIT_CODE_RANGES`'s doc comment in `jit/src/lib.rs`). Now the cached
    // generation is compared against `cratonvm_jit::jit_code_ranges_generation()`
    // first; the expensive resnapshot/resort only runs when the set actually
    // changed since this thread last looked (the overwhelmingly common case is
    // a burst of many calls between any two JIT compiles finishing).
    thread_local! {
        static RANGE_SNAPSHOT: std::cell::RefCell<(u64, Vec<(usize, usize)>)> =
            const { std::cell::RefCell::new((u64::MAX, Vec::new())) };
    }
    RANGE_SNAPSHOT.with(|cell| {
        let mut cached = cell.borrow_mut();
        let (cached_gen, ranges) = &mut *cached;
        let current_gen = cratonvm_jit::jit_code_ranges_generation();
        if *cached_gen != current_gen {
            cratonvm_jit::snapshot_code_ranges_into(ranges);
            *cached_gen = current_gen;
        }
        if ranges.is_empty() {
            return None;
        }
        // Address envelope of ALL code ranges: `ranges` is sorted by start, so
        // the smallest start is first; the largest end is the max over the (few)
        // entries. A stack word outside `[env_lo, env_hi)` cannot be in any
        // range, so reject it with a single compare before the binary search.
        // The overwhelming majority of stack words are ints / data pointers far
        // outside the small JIT code arena, so this prefilter skips the
        // partition_point for ~all words. Strict superset of the membership test
        // → result is identical.
        let env_lo = ranges[0].0;
        let env_hi = ranges.iter().map(|&(_, e)| e).max().unwrap_or(0);
        let mut addr = (lo + 7) & !7usize;
        const MAX_SCAN_BYTES: usize = 8 * 1024 * 1024;
        let hi = hi.min(addr.saturating_add(MAX_SCAN_BYTES));
        // Counted per CALL — see `rootprof::note_jit_probe`. Recorded up front
        // so an early `return Some(..)` (a hit, which stops the walk) still
        // reports the band this probe was ASKED for, which is the quantity the
        // memo's incremental-band logic is supposed to be shrinking.
        crate::memory::native_roots::rootprof::note_jit_probe(
            (hi.saturating_sub(addr) / 8) as u64, // Widening: bounded by MAX_SCAN_BYTES
        );
        while addr + 8 <= hi {
            // SAFETY: aligned read inside the calling thread's own live stack
            // band between two known stack pointers (same contract as
            // `scan_one_frame`).
            let w = unsafe { (addr as *const usize).read() };
            if w >= env_lo && w < env_hi {
                // Greatest range whose start <= w; its start itself is a
                // stored function pointer, while a native return PC is
                // strictly inside the range.
                let idx = ranges.partition_point(|&(s, _)| s <= w);
                if idx > 0
                    && w > ranges[idx - 1].0
                    && w < ranges[idx - 1].1
                    && is_plausible_return_pc(w)
                {
                    return Some((addr, w));
                }
            }
            addr += 8;
        }
        None
    })
}

/// Does a candidate A5 slot look like the return-address slot of a REAL frame?
///
/// `is_plausible_return_pc` asks whether the *word* could be a return address.
/// This asks the different question the A5 probe actually needs: whether the
/// *slot holding it* is where a live frame's return address sits. It is the
/// frame-shape half of the "raw-word scan, not a frame walk" follow-up that
/// [`native_stack_has_jit_frame`]'s own comment names.
///
/// A frame entered by `call` and opened with the JIT's prologue
/// (`push rbp; mov rbp, rsp`, which every compiled body emits) lays the stack
/// out exactly like this, addresses growing upward:
///
/// ```text
///   slot - 8  : saved caller RBP   <- callee's own RBP points here
///   slot      : return address     <- the candidate word
///   slot + 8  : caller's frame ...
/// ```
///
/// So the word one slot BELOW a genuine return address is the caller's frame
/// base: 8-aligned, strictly above this frame, inside the same stack, and
/// itself the base of a frame whose own return-address slot holds a plausible
/// return PC. A stale return address left behind in the uninitialised middle of
/// a live frame has no such neighbour except by coincidence.
///
/// Deliberately shallow — two links, no walk to the stack base. The chain above
/// a genuine JIT frame runs into VM Rust frames, and this tree does not build
/// with forced frame pointers, so those frames need not maintain RBP at all and
/// a deeper walk would reject real frames. Two links is what can be asserted
/// from the JIT's own calling convention alone.
///
/// Conservative in the safe direction on purpose: this is only ever used to
/// decide whether over-detection is happening, never to suppress a hit that
/// passes.
fn a5_slot_has_frame_shape(slot: usize, stack_hi: usize) -> bool {
    if !a5_frame_base_is_plausible(slot, stack_hi) {
        return false;
    }
    // SAFETY: `slot - 8` and `caller_rbp + 8` are 8-aligned addresses inside the
    // calling thread's own stack — `slot` came from the scan, which only offers
    // addresses it has already read, and `a5_frame_base_is_plausible` has
    // bounded `caller_rbp + 8` below `stack_hi`.
    let caller_rbp = unsafe { ((slot - 8) as *const usize).read() };
    if !a5_frame_base_is_plausible_link(slot, caller_rbp, stack_hi) {
        return false;
    }
    let caller_ret = unsafe { ((caller_rbp + 8) as *const usize).read() };
    // The caller of a JIT frame is either another JIT frame or the VM's own
    // code. Only the first is checkable from here; a caller outside every JIT
    // range is accepted, because the interpreter->JIT boundary is exactly that
    // and is the most common real case.
    match cratonvm_jit::lookup_jit_code_range(caller_ret) {
        Some(_) => is_plausible_return_pc(caller_ret),
        None => caller_ret != 0,
    }
}

/// Can `slot` be a return-address slot at all? Split out so the arithmetic is
/// testable without a real stack.
fn a5_frame_base_is_plausible(slot: usize, stack_hi: usize) -> bool {
    slot >= 8 && slot & 0x7 == 0 && slot < stack_hi
}

/// Is `caller_rbp`, read from `slot - 8`, shaped like the caller's frame base?
///
/// A saved caller RBP is 8-aligned, strictly OLDER than this frame (a higher
/// address, since the stack grows down), and leaves room for its own
/// return-address slot below `stack_hi`. Pure arithmetic, so the invariant this
/// rests on is stated once and tested directly.
fn a5_frame_base_is_plausible_link(slot: usize, caller_rbp: usize, stack_hi: usize) -> bool {
    caller_rbp & 0x7 == 0
        && caller_rbp > slot
        && caller_rbp.checked_add(8).is_some_and(|end| end < stack_hi)
}

/// Census of the A5 band: how many words look like JIT return addresses, and
/// how many of those sit at a slot with real frame shape.
///
/// Diagnostic only — nothing branches on it. It exists to PRICE the frame-shape
/// filter before anything is built on it, the way the fallback-reason mask
/// priced the indirect-call repair: if a cycle's hits are all shapeless, the
/// filter would have converted that cycle; if any hit has frame shape, it would
/// not, and the cycle is blocked by something the filter cannot reach.
fn native_stack_jit_frame_census(lo: usize, hi: usize) -> (usize, usize) {
    let mut total = 0usize;
    let mut shaped = 0usize;
    let mut addr = (lo + 7) & !7usize;
    const MAX_SCAN_BYTES: usize = 8 * 1024 * 1024;
    let hi_capped = hi.min(addr.saturating_add(MAX_SCAN_BYTES));
    while addr + 8 <= hi_capped {
        // SAFETY: same contract as `native_stack_has_jit_frame` — an aligned
        // read inside this thread's own live stack band.
        let w = unsafe { (addr as *const usize).read() };
        if let Some(cm_ptr) = cratonvm_jit::lookup_jit_code_range(w) {
            let cm = unsafe { &*(cm_ptr as *const cratonvm_jit::CompiledMethod) };
            if w != cm.entry_ptr() as usize && is_plausible_return_pc(w) {
                total += 1;
                if a5_slot_has_frame_shape(addr, hi) {
                    shaped += 1;
                }
            }
        }
        addr += 8;
    }
    (total, shaped)
}

/// Return-address validation for the A5 raw-word scan.
///
/// The scan is a word scan, not a frame walk: any stack slot whose value
/// happens to land inside a JIT code range reads as a return PC. That
/// over-detection is safe (it only forces the non-moving young sweep) but it is
/// not free — a persistent false positive means the young generation never
/// compacts, and `native_stack_has_jit_frame`'s own doc comment names cutting
/// the false-positive rate as the designated follow-up.
///
/// This is that filter, and it uses the one fact that separates a genuine
/// return address from a stored code pointer: **a return address is always the
/// address of the instruction after a `call`**. So the bytes immediately
/// preceding it must be the tail of a call encoding. On x86-64 the forms that
/// can appear in generated code and in the trampolines that enter it are:
///
/// * `E8 rel32`          — 5 bytes, direct near call
/// * `FF /2` (`callq *r/m`) — 2 to 7 bytes, indirect call (register, memory,
///   with or without REX / SIB / displacement)
/// * `9A`, `FF /3`       — far call; not emitted by this JIT, ignored
///
/// A stored function pointer (a cached `entry_ptr`, an OSR entry, a stub
/// address handed to a helper as an argument) is preceded by whatever
/// instruction bytes happen to sit before the callee's own entry — matching one
/// of these encodings is possible but no longer automatic.
///
/// Conservative in the safe direction: when the preceding bytes cannot be read
/// or do not decode, the word is REJECTED as a return PC. That is the direction
/// that costs moving cycles, never correctness — the opposite of a filter that
/// waves a real unregistered frame through.
///
/// Reading the bytes needs a keep-alive, not just an address. The caller's
/// range snapshot is a lock-free copy that can name a body whose last `Arc` has
/// since been dropped — every other user of that snapshot only *compares*
/// addresses, so a stale entry was harmless until this function wanted to
/// dereference one. [`cratonvm_jit::pin_jit_code_range_owner`] upgrades the
/// range's `Weak` and keeps the executable buffer alive for the read; `None`
/// (the body really is gone) is a rejection, and a correct one — no live frame
/// can be returning into reclaimed code.
#[cfg(any(target_os = "windows", target_os = "linux"))]
fn is_plausible_return_pc(w: usize) -> bool {
    if !return_pc_validation_enabled() {
        return true;
    }
    let Some(owner) = cratonvm_jit::pin_jit_code_range_owner(w) else {
        return false;
    };
    // Lower bound for the look-back: the body's own entry point, which is the
    // start of its registered range. `w` is strictly inside `[entry, end)`, so
    // the window never reaches the page before the code buffer.
    let entry = owner.entry_ptr() as usize;
    // Bytes available behind `w`, capped at the longest encoding tail we
    // recognise (REX + FF + ModRM + SIB + disp32, of which 7 precede the
    // return address at most).
    let avail = w.saturating_sub(entry).min(CALL_TAIL_MAX);
    let mut window = [0u8; CALL_TAIL_MAX];
    for i in 0..avail {
        // SAFETY: `w - avail + i` is in `[entry, w)`, inside the executable
        // buffer `owner` is holding alive for the duration of this call.
        window[i] = unsafe { ((w - avail + i) as *const u8).read() };
    }
    call_encoding_precedes(&window[..avail])
}

/// Longest instruction tail [`call_encoding_precedes`] inspects.
#[cfg(any(target_os = "windows", target_os = "linux"))]
const CALL_TAIL_MAX: usize = 7;

/// Does `window` — the bytes immediately preceding a candidate return address,
/// in address order, at most [`CALL_TAIL_MAX`] of them — end with a near-call
/// encoding?
///
/// Split out from [`is_plausible_return_pc`] so the decode is testable without
/// a live JIT code range: this half is pure, the other half is the pin-and-read.
#[cfg(any(target_os = "windows", target_os = "linux"))]
fn call_encoding_precedes(window: &[u8]) -> bool {
    let n = window.len();
    // A near call is at least two bytes (`FF /2` with a register operand), so
    // nothing shorter can decode.
    if n < 2 {
        return false;
    }
    // Right-align into a fixed array so an encoding of length `len` always has
    // its opcode at index `CALL_TAIL_MAX - len`.
    let mut prev = [0u8; CALL_TAIL_MAX];
    prev[CALL_TAIL_MAX - n..].copy_from_slice(window);
    // `E8 rel32`: the direct near call, 5 bytes, so its opcode sits 5 back.
    if n >= 5 && prev[CALL_TAIL_MAX - 5] == 0xE8 {
        return true;
    }
    // `FF /2` indirect near call: 2..=7 bytes. Walk every length the encoding
    // can take and accept if the byte at that distance is `FF` and the ModRM
    // that follows selects reg field 2 (`/2`, bits 5..3 == 0b010).
    for len in 2..=n {
        let op = CALL_TAIL_MAX - len;
        if prev[op] == 0xFF && (prev[op + 1] >> 3) & 0b111 == 0b010 {
            return true;
        }
    }
    // Same, one byte earlier, to allow a single REX prefix (0x40..=0x4F) in
    // front of the `FF`.
    for len in 3..=n {
        let rex = CALL_TAIL_MAX - len;
        if prev[rex] & 0xF0 == 0x40
            && prev[rex + 1] == 0xFF
            && (prev[rex + 2] >> 3) & 0b111 == 0b010
        {
            return true;
        }
    }
    false
}

/// Kill switch for [`is_plausible_return_pc`]: `CRATONVM_JIT_NO_RETPC_VALIDATE=1`
/// restores the pre-2026-08-04 behaviour where every in-range stack word counts
/// as a JIT frame. Exists so a soak can A/B the filter without a rebuild, and so
/// a future bug report has a one-flag bisect for it. Same cached-read rationale
/// as [`jit_range_scan_legacy`] — this sits on the per-native-call root-snapshot
/// path.
#[cfg(any(target_os = "windows", target_os = "linux"))]
fn return_pc_validation_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        cratonvm_types::flags::runtime_var_os("CRATONVM_JIT_NO_RETPC_VALIDATE").is_none()
    })
}

/// How many times the A5 probe diverted a moving-young cycle, split by what
/// the ONE hit slot it returned actually looked like.
///
/// The probe is a raw-word scan, and its own doc comment has named cutting the
/// false-positive rate as the designated follow-up since it was written. Two
/// screens exist for that and neither could be judged here, because the
/// population was never counted: `native_stack_jit_frame_census` re-walks the
/// whole band (so it is opt-in and off in every real run) and the residue
/// filter was applied at one of the probe's two call sites only.
///
/// `A5_FALLBACK_RESIDUE` counts hits at a slot BELOW this thread's
/// returned-JIT-frame high-water mark — the leftovers of a compiled frame that
/// has already returned. `A5_FALLBACK_SHAPELESS` counts hits at a slot that is
/// not the return-address slot of a frame at all, and is only computed when
/// something asks for it (it reaches the global code-range table, which the
/// probe's fast path exists to avoid). Either makes the hit a false positive,
/// and a false positive costs the young generation its copying collector for
/// that cycle.
pub static A5_FALLBACK_HITS: AtomicUsize = AtomicUsize::new(0);
/// Hits whose slot sits below [`jit_residue_hi`] — see [`A5_FALLBACK_HITS`].
pub static A5_FALLBACK_RESIDUE: AtomicUsize = AtomicUsize::new(0);
/// Hits whose slot has no frame shape — see [`A5_FALLBACK_HITS`]. Only counted
/// when the shape filter or the fallback dump is on.
pub static A5_FALLBACK_SHAPELESS: AtomicUsize = AtomicUsize::new(0);
/// Hits a screen suppressed — see [`A5_FALLBACK_HITS`].
pub static A5_FALLBACK_FILTERED: AtomicUsize = AtomicUsize::new(0);

/// Snapshot of the A5 fallback census: `(hits, residue, shapeless, filtered)`.
pub fn a5_fallback_census() -> (usize, usize, usize, usize) {
    (
        A5_FALLBACK_HITS.load(Ordering::Relaxed),
        A5_FALLBACK_RESIDUE.load(Ordering::Relaxed),
        A5_FALLBACK_SHAPELESS.load(Ordering::Relaxed),
        A5_FALLBACK_FILTERED.load(Ordering::Relaxed),
    )
}

/// `CRATONVM_DBG_A5_FALLBACK=1` — print the first sixteen A5 probe hits and
/// then every power of two, naming the slot, whether it is residue, and whether
/// it has frame shape.
///
/// Distinct from [`a5_census_enabled`], which re-walks the whole band to count
/// EVERY candidate word in it. This one reports what the probe already found.
/// The probe runs on the per-native-call root-snapshot path — measured at 1.1
/// million calls in one 240 s run — so it is rate-limited rather than
/// per-event, and the totals are in [`a5_fallback_census`].
#[cfg(any(target_os = "windows", target_os = "linux"))]
fn a5_fallback_dbg() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| cratonvm_types::flags::runtime_var_os("CRATONVM_DBG_A5_FALLBACK").is_some())
}

/// Kill switch for the residue filter on the MOVING-YOUNG COVERAGE probe —
/// `CRATONVM_JIT_A5_RESIDUE_FILTER=0` accepts every hit again.
///
/// `scan_active_jit_frames` has applied exactly this filter, with exactly this
/// argument, since the H2 `FileNioMapped.unMap` investigation: a compiled
/// method that has already returned leaves a return address into JIT code at
/// every depth below its own `entry_sp`, and that is indistinguishable by
/// inspection from a live guardless frame. The one live guardless frame the
/// probe exists for is the process entry point, which sits ABOVE every JIT
/// entry the run has ever made and therefore above the mark.
///
/// The coverage probe in `refresh_moving_young_coverage_for_current_thread` is
/// the same scan asking the same question and did NOT have it — the cheaper
/// half of the two, because there a false positive only declines to MOVE (the
/// sound direction), whereas at the marking site it declines to MARK. The
/// filter was therefore already trusted in the more dangerous of the two
/// places.
#[cfg(any(target_os = "windows", target_os = "linux"))]
fn a5_residue_filter_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        !matches!(
            cratonvm_types::flags::runtime_var("CRATONVM_JIT_A5_RESIDUE_FILTER").as_deref(),
            Ok("0") | Ok("false") | Ok("off")
        )
    })
}

/// Opt-in gate for the frame-shape filter on the moving-young coverage probe —
/// `CRATONVM_JIT_A5_SHAPE_FILTER=1`.
///
/// Default OFF, and deliberately so on two counts.
/// [`a5_slot_has_frame_shape`] was written as a PRICING instrument and its own
/// doc says it is "only ever used to decide whether over-detection is
/// happening, never to suppress a hit that passes"; turning it into a
/// suppressor is a different claim and needs its own measurement. And it costs
/// a `lookup_jit_code_range`, i.e. the global table lock that the probe's fast
/// path was rewritten to avoid.
#[cfg(any(target_os = "windows", target_os = "linux"))]
fn a5_shape_filter_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        matches!(
            cratonvm_types::flags::runtime_var("CRATONVM_JIT_A5_SHAPE_FILTER").as_deref(),
            Ok("1") | Ok("true") | Ok("on")
        )
    })
}

/// Diagnostic gate for [`native_stack_jit_frame_census`]. Off by default: the
/// census re-scans the whole band a second time, which is affordable only when
/// you are deliberately measuring.
#[cfg(any(target_os = "windows", target_os = "linux"))]
fn a5_census_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| cratonvm_types::flags::runtime_var_os("CRATONVM_DBG_A5_CENSUS").is_some())
}

/// How many times the residue filter DECLINED a hit, and how many heap objects
/// those declines chose not to conservatively mark.
///
/// The filter exists to stop a returned frame's leftovers from pinning the
/// whole native stack (the H2 `FileNioMapped.unMap` timeout). Declining is
/// therefore the safe direction for RETENTION and the unsafe direction for
/// LIVENESS: the band it skips is the only thing this site would have marked.
/// Nothing counted either half, so "the filter is why that object was swept"
/// could only ever be an inference.
pub static UNREG_DECLINED: AtomicUsize = AtomicUsize::new(0);
/// Objects the declines above chose not to mark — see [`UNREG_DECLINED`].
pub static UNREG_DECLINED_ROOTS: AtomicUsize = AtomicUsize::new(0);

/// Snapshot of the declined-band census: `(declines, objects not marked)`.
pub fn unreg_declined_census() -> (usize, usize) {
    (
        UNREG_DECLINED.load(Ordering::Relaxed),
        UNREG_DECLINED_ROOTS.load(Ordering::Relaxed),
    )
}

/// `CRATONVM_DBG_UNREG_DECLINED=1` — name the compiled body each declined hit
/// belongs to, and list the heap objects the decline is not marking.
///
/// The body's `method_label` is the question this census exists to answer: a
/// declined hit in a method whose frame is genuinely live means an entry door
/// that pushed no `JitEntryGuard`, and the repair is to register it. A declined
/// hit in a method that has demonstrably returned means the band is being kept
/// alive for some other reason, and the repair is elsewhere.
///
/// Costs a second walk of the band (the `scan_one_frame` the decline skipped,
/// into a throw-away vector), so it is opt-in.
#[cfg(any(target_os = "windows", target_os = "linux"))]
fn unreg_declined_dbg() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        cratonvm_types::flags::runtime_var_os("CRATONVM_DBG_UNREG_DECLINED").is_some()
    })
}

/// Kill switch for the residue filter on the unregistered-JIT-frame probe --
/// see its call site in `scan_active_jit_frames`. Set it to accept every hit
/// again, as before the filter existed.
#[cfg(any(target_os = "windows", target_os = "linux"))]
fn unreg_jit_accept_residue() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        cratonvm_types::flags::runtime_var_os("CRATONVM_JIT_UNREG_ACCEPT_RESIDUE").is_some()
    })
}

/// Kill switch for the residue test on the unregistered-JIT-frame probe's
/// RELOCATION LICENCE -- `CRATONVM_JIT_UNREG_RESIDUE_LICENCE=0` restores the
/// pre-2026-09-08 behaviour, where any accepted hit refused relocation for the
/// cycle even when the returned-frame residue mark explained it.
///
/// Marking is unaffected either way: the full band is conservatively scanned on
/// every accepted hit under both settings, so this switch can only change how
/// often the collector is ALLOWED TO COMPACT, never what it retains.
#[cfg(any(target_os = "windows", target_os = "linux"))]
fn unreg_residue_licence_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        !matches!(
            cratonvm_types::flags::runtime_var("CRATONVM_JIT_UNREG_RESIDUE_LICENCE").as_deref(),
            Ok("0") | Ok("false") | Ok("off")
        )
    })
}

/// Cached `CRATONVM_JIT_RANGE_SCAN_LEGACY` gate — see `native_stack_has_jit_frame`.
#[cfg(any(target_os = "windows", target_os = "linux"))]
fn jit_range_scan_legacy() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        cratonvm_types::flags::runtime_var_os("CRATONVM_JIT_RANGE_SCAN_LEGACY").is_some()
    })
}

/// Read the safepoint id a live compiled frame published into
/// `[rbp - cm.sp_id_slot_off]`. `None` when the method reserves no such slot.
fn active_safepoint_id(rbp: usize, cm: &cratonvm_jit::CompiledMethod) -> Option<u32> {
    let sp_id_off = cm.sp_id_slot_off;
    if sp_id_off == 0 {
        return None;
    }
    let id_addr = rbp.checked_sub(sp_id_off as usize)?;
    if id_addr & 0x7 != 0 {
        return None;
    }
    // SAFETY: aligned safepoint-id slot in a live JIT frame on this thread.
    Some((unsafe { (id_addr as *const usize).read() }) as u32)
}

/// The exclusive frame-offset bound of the LIVE part of this frame at its
/// active safepoint (`OopMapEntry::live_frame_hi`). `None` when the bound is
/// unknown — no sp-id slot, no matching map, or a map recorded without a
/// paired pre-safepoint spill — in which case the caller must scan the whole
/// region. When several maps share the safepoint id, the largest (most
/// conservative) bound wins.
fn moving_young_frame_live_hi(rbp: usize, cm: &cratonvm_jit::CompiledMethod) -> Option<i32> {
    let sp_id = active_safepoint_id(rbp, cm)?;
    let mut hi = 0i32;
    for map in cm.oop_maps.iter().filter(|m| m.bytecode_pc == sp_id) {
        if map.live_frame_hi <= 0 {
            return None;
        }
        hi = hi.max(map.live_frame_hi);
    }
    (hi > 0).then_some(hi)
}

/// Whether `exact_rbp` recorded for a chain entry actually belongs to a
/// DEEPER, unguarded compiled frame rather than to the entry's own method.
///
/// A chain entry is pushed at a Rust/interpreter -> JIT boundary, so the frame
/// it describes was called from non-JIT code and its return address does NOT
/// point into compiled code. Every compiled prologue, however, publishes its
/// own RBP into the innermost-RBP mirror, which is flushed into the top chain
/// entry before every GC walk — and two generated-code paths reach a compiled
/// callee with no guard at all: the inline MIC/PIC cascade and the hashed
/// megamorphic stub (`runtime_lowering::emit_hashed_vtable_stub`, which is not
/// gated by `direct_jit_callee_calls_enabled()`). After such a call the entry
/// still names the boundary method while `exact_rbp` names the callee's frame.
///
/// Detect it by the frame's own return address: if `[exact_rbp + 8]` points
/// into registered JIT code, the frame was entered FROM compiled code and this
/// entry cannot describe it. Using the entry's `compiled_method` for that RBP
/// would validate coverage against a safepoint id read out of an unrelated
/// frame slot and then rewrite `[rbp - off]` for the wrong method's oop map,
/// leaving the callee's real oops unrelocated across an evacuation.
///
/// The outward RBP-chain walk is unaffected: it resolves every ancestor from
/// its return address, which is correct regardless.
///
/// # The direct self-call exception
///
/// "Entered from compiled code" is not by itself a reason to give up: it only
/// matters when the frame belongs to a method the entry does not name. A
/// compiled method that recurses does so through a direct `E8 rel32` CALL back
/// to its OWN entry (`x64.rs`, `self_call_patches`), pushing no guard — so a
/// recursive workload leaves the mirror pointing at an inner activation of the
/// very method the entry names, and `info.compiled_method` describes that frame
/// exactly. Rejecting it costs every moving cycle for no safety gain: measured
/// on `BinTreesClassic`, whose `itemCheck`/`bottomUpTree` recursion made this
/// the reason for 100% of fallbacks (82/82 at depth 18, 59/59 at 16, 2/2 at 14)
/// once the exact-RBP defect above it was fixed.
///
/// The exception is recognised from the machine code, not inferred from which
/// direct-call features happen to be gated off: the return address must lie in
/// the entry's own body AND the five bytes ending there must be a direct CALL
/// whose target is that body's entry point. Anything else — an indirect call,
/// a call from a different method, bytes that cannot be read — stays foreign.
fn chain_entry_rbp_is_foreign(
    exact_rbp: usize,
    exact_cm_id: u32,
    entry_sp: usize,
    scanner_sp: usize,
    cm: *const cratonvm_jit::CompiledMethod,
) -> bool {
    innermost_frame_method(exact_rbp, exact_cm_id, entry_sp, scanner_sp, cm).is_none()
}

/// Whether the direct-call callee resolution below is enabled. Default ON;
/// opt out with `CRATONVM_GC_NO_CALLEE_RESOLVE=1`, which restores the
/// "any deeper frame is foreign" behaviour exactly.
fn callee_resolve_enabled() -> bool {
    static G: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *G.get_or_init(|| {
        cratonvm_types::flags::runtime_var_os("CRATONVM_GC_NO_CALLEE_RESOLVE").is_none()
    })
}

/// The [`cratonvm_jit::CompiledMethod`] that actually describes the frame
/// standing at `exact_rbp`, or `None` when it cannot be established — in which
/// case every caller must fail closed (no precise map, no relocation, and a
/// conservative band bounded by the scanner's own SP).
///
/// The entry's own `cm` is the answer in two cases: the frame was entered from
/// non-JIT code (the ordinary boundary frame this entry was pushed for), or it
/// was entered by a direct self-call (a recursive activation of the very method
/// the entry names — see the exception documented on
/// [`chain_entry_rbp_is_foreign`]).
///
/// # Resolving a different callee
///
/// Neither of those covers the shape that made this the reason for **100% of
/// bintrees fallbacks**: `binaryTrees` calls `bottomUpTree` and `itemCheck`, so
/// the mirror names a frame belonging to a method the entry does not, and the
/// old code gave up. But "the entry cannot describe this frame" is not the same
/// as "nothing can" — the frame's own return address says exactly which method
/// built it, provided the call was the direct form:
///
/// ```text
/// [exact_rbp + 8] = ret_addr
/// [ret_addr - 5]  = E8 rel32          <- direct CALL
/// ret_addr + rel32 == callee.entry_ptr()
/// ```
///
/// A callee resolved that way describes the frame exactly, because that frame
/// was built by the prologue at `entry_ptr`. The requirement that the target be
/// the registered **entry point** — not merely some address inside the method —
/// is what makes it exact: a call landing anywhere else (an OSR entry, a stub)
/// would not have run the prologue that establishes these slot offsets.
///
/// Everything else stays foreign, and that is the whole safety argument: an
/// indirect call (the inline MIC/PIC cascade, the hashed megamorphic stub) has
/// no decodable target, unreadable bytes decode to nothing, and a displacement
/// resolving into the middle of a method resolves to `None`. The failure
/// direction is a non-moving sweep, never a frame walked with the wrong map.
fn innermost_frame_method(
    exact_rbp: usize,
    exact_cm_id: u32,
    entry_sp: usize,
    scanner_sp: usize,
    cm: *const cratonvm_jit::CompiledMethod,
) -> Option<*const cratonvm_jit::CompiledMethod> {
    if exact_rbp == 0 || exact_rbp & 0x7 != 0 {
        return Some(cm);
    }
    if exact_rbp < scanner_sp || exact_rbp.saturating_add(16) > entry_sp {
        return Some(cm);
    }
    // SAFETY: aligned read of the saved return address inside this thread's own
    // live JIT stack band, bounded by `scanner_sp` / `entry_sp` exactly as the
    // neighbouring chain walks do.
    let ret_addr = unsafe { ((exact_rbp + 8) as *const usize).read() };
    let Some(caller_cm) = cratonvm_jit::lookup_jit_code_range(ret_addr) else {
        // A non-JIT caller is the ordinary boundary frame this entry was pushed
        // for.
        return Some(cm);
    };
    if caller_cm as *const cratonvm_jit::CompiledMethod == cm {
        // The caller is the entry's own method. That is a recursive activation
        // iff the call really was the direct self-call form.
        // SAFETY: `cm` is Arc-owned by the JIT cache while any of its frames is
        // live, which is the precondition for being on this chain at all.
        let entry_ptr = unsafe { (*cm).entry_ptr() } as usize;
        if returned_from_direct_self_call(ret_addr, entry_ptr) {
            return Some(cm);
        }
    }
    if !callee_resolve_enabled() {
        return None;
    }
    // The frame's own prologue named itself; no decode needed.
    if let Some(published) = published_innermost_method(exact_cm_id) {
        return Some(published);
    }
    direct_call_callee(ret_addr, caller_cm)
}

/// The method the innermost frame's own prologue named, or `None`.
///
/// [`direct_call_callee`] can only answer for a frame entered by a direct
/// `CALL rel32`. An INDIRECT JIT->JIT call — every inline-cache hit ends in
/// `CALL R11`, plus the megamorphic stub and the trampolines — encodes no
/// rel32, so it failed closed and diverted the whole young collection to the
/// non-moving sweep. That was 419 of 419 of `Log4J2LoggingSystemTests`'s
/// fallback cycles under `-XX:+UseGenerationalGC`, with no other obligation in
/// the set (`docs/known-issues/springboot/`, the moving-young page).
///
/// Codegen publishes the compile id beside the RBP it already stores, in the
/// same instruction pair, at every point that touches the mirror. `id` is that
/// value as captured by [`JitPreciseFrameInfo::exact_cm_id`] — snapshotted
/// together with the RBP it belongs to, NOT read here. Reading the mirror at
/// scan time instead cost a whole verification round: the owning thread has
/// moved on by then, and the scan often runs on the collector's thread whose
/// mirrors describe a different stack, so the id resolved in 1 of 786 cycles.
///
/// An unbound id (reserved but not yet published, or released on drop) answers
/// `None`, which returns the caller to the decode it always had.
fn published_innermost_method(id: u32) -> Option<*const cratonvm_jit::CompiledMethod> {
    let cm_ptr = cratonvm_jit::lookup_compile_id(id)?;
    Some(cm_ptr as *const cratonvm_jit::CompiledMethod)
}

/// Read the compile-id mirror — the identity half of the inline frame record.
/// Windows reads the probed TEB slot directly (as the RBP mirror does); Linux
/// goes through the JIT crate's accessor for the same Rust TLS cell generated
/// code writes. `0` = nothing published.
#[inline]
fn published_compile_id() -> u32 {
    #[cfg(windows)]
    {
        let disp = cratonvm_jit::x64::inline_cm_tls_disp();
        if disp != 0 {
            // SAFETY: `disp` was validated by the startup sentinel probe in
            // `inline_cm_tls_disp()` to be a live, 8-byte-aligned TEB TLS slot
            // present on every thread — the same earned safety as the RBP
            // mirror read directly above.
            return (unsafe { read_gs_qword(disp) } & 0xFFFF_FFFF) as u32;
        }
    }
    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    {
        if let Some(id) = cratonvm_jit::x64::inline_cm_tls_mirror_read() {
            return id;
        }
    }
    0
}

/// Decode the direct `CALL` whose return address is `ret_addr` and resolve its
/// target to the registered method whose **entry point** it is.
///
/// `caller_cm` is the method the return address lies in; its entry bounds the
/// five-byte backward read, so the read stays inside that method's own
/// executable buffer. Fails closed on every ambiguity.
fn direct_call_callee(
    ret_addr: usize,
    caller_cm: usize,
) -> Option<*const cratonvm_jit::CompiledMethod> {
    if caller_cm == 0 {
        return None;
    }
    // SAFETY: `caller_cm` is what `lookup_jit_code_range` resolved for a return
    // address in a LIVE frame, so the JIT cache still owns the `Arc` whose inner
    // value it addresses — the same keep-alive argument the neighbouring chain
    // walks rely on.
    let caller_entry =
        unsafe { (*(caller_cm as *const cratonvm_jit::CompiledMethod)).entry_ptr() } as usize;
    let target = direct_call_target(ret_addr, caller_entry)?;
    let callee =
        cratonvm_jit::lookup_jit_code_range(target)? as *const cratonvm_jit::CompiledMethod;
    // Exactness: only the registered entry point runs the prologue that
    // establishes the slot offsets every caller is about to read.
    // SAFETY: as above — `target` is a live code address, so its owner is
    // retained by the JIT cache for as long as a frame of it is on this stack.
    (unsafe { (*callee).entry_ptr() } as usize == target).then_some(callee)
}

/// Decode the `E8 rel32` whose return address is `ret_addr` and return its
/// target, or `None` when the bytes are not a direct near CALL.
///
/// `caller_entry` bounds the five-byte backward read so it cannot run below the
/// caller's own executable buffer. Split out from [`direct_call_callee`] with
/// no pointer dereference of its own, which is what makes it testable — the
/// resolution half needs a registered code range, and a unit test that
/// fabricated a `CompiledMethod` pointer to reach this logic would be reading
/// a `[u8; N]` as one.
fn direct_call_target(ret_addr: usize, caller_entry: usize) -> Option<usize> {
    if caller_entry == 0 {
        return None;
    }
    direct_call_target_rel32(ret_addr, caller_entry)
        .or_else(|| direct_call_target_abs64(ret_addr, caller_entry))
}

/// Length of the `MOVABS RAX, imm64` + `CALL RAX` sequence
/// [`direct_call_target_abs64`] decodes, in bytes.
const ABS64_CALL_LEN: usize = 12;

/// `E8 rel32` — the near CALL the JIT emits whenever the target is within
/// +/-2GB of the emit-time buffer position.
fn direct_call_target_rel32(ret_addr: usize, caller_entry: usize) -> Option<usize> {
    // The call instruction lies between the caller's entry and the return
    // address, so a return address within 5 bytes of it cannot be one.
    if ret_addr < caller_entry.saturating_add(5) {
        return None;
    }
    // SAFETY: `[ret_addr - 5, ret_addr)` lies inside the executable buffer of
    // the compiled method `lookup_jit_code_range(ret_addr)` resolved, at or
    // above its entry point, and that buffer is kept alive by the live frame
    // whose return address this is. Code pages are readable.
    if unsafe { ((ret_addr - 5) as *const u8).read() } != 0xE8 {
        return None;
    }
    let rel = unsafe { ((ret_addr - 4) as *const i32).read_unaligned() };
    // `E8 rel32` targets `next_instruction + rel32`, and `ret_addr` IS the next
    // instruction.
    Some(ret_addr.wrapping_add(rel as usize))
}

/// `48 B8 <imm64> FF D0` — `MOVABS RAX, imm64` followed by `CALL RAX`, the
/// OTHER form of the very same direct call.
///
/// `x64::emit::emit_call_absolute` picks between two encodings for one call to
/// one known address: `E8 rel32` when the target is within +/-2GB of the
/// emit-time buffer position, and this twelve-byte absolute sequence
/// (`emit_call_imm64_via_rax`) when it is not. Which one a given call site gets
/// therefore depends on where the staging buffer and the callee's code page
/// happened to be allocated — the same call, compiled twice, can come out
/// either way.
///
/// Decoding only the first form made the choice observable as a correctness
/// bug. `stackwalker_log4j_deep_repeated_walks_finish_under_jit` failed in
/// roughly 10 of 12 runs of the same binary: when `recurse`'s call to
/// `LoggerFactory.resolveCaller` came out as the absolute form,
/// [`innermost_frame_method`] could not name the frame it built, returned
/// `None`, and the whole `resolveCaller` frame vanished from the walk. Log4j2's
/// caller lookup then answered with `recurse`'s declaring class. Measured on a
/// failing run, the twelve bytes ending at that return address were
/// `48 b8 00 70 9c 5b ed 75 00 00 ff d0` — this sequence, targeting
/// `resolveCaller`'s registered entry point.
///
/// This is not a weaker decode than the rel32 one, it is a stronger one: the
/// target is a literal in the instruction stream rather than a displacement to
/// add. Every caller still requires the decoded address to equal a registered
/// method's ENTRY POINT ([`direct_call_callee`]), so an indirect call — the
/// inline-cache `CALL R11`, the megamorphic stub — decodes to nothing and stays
/// foreign exactly as before. A bare `FF D0` not preceded by the `MOVABS` half
/// is rejected here too.
fn direct_call_target_abs64(ret_addr: usize, caller_entry: usize) -> Option<usize> {
    // As above: the twelve-byte sequence has to fit between the caller's entry
    // and the return address, which is what bounds the backward read.
    if ret_addr < caller_entry.saturating_add(ABS64_CALL_LEN) {
        return None;
    }
    let seq = ret_addr - ABS64_CALL_LEN;
    // SAFETY: `[seq, ret_addr)` lies inside the executable buffer of the
    // compiled method `lookup_jit_code_range(ret_addr)` resolved, at or above
    // its entry point, and that buffer is kept alive by the live frame whose
    // return address this is. Code pages are readable. Same argument, and the
    // same bound, as `direct_call_target_rel32` — only the span differs.
    unsafe {
        // REX.W + B8+rd with rd = RAX: `MOVABS RAX, imm64`.
        if (seq as *const u8).read() != 0x48 || ((seq + 1) as *const u8).read() != 0xB8 {
            return None;
        }
        // FF /2 with ModRM mod=11 r/m=RAX: `CALL RAX`.
        if ((ret_addr - 2) as *const u8).read() != 0xFF
            || ((ret_addr - 1) as *const u8).read() != 0xD0
        {
            return None;
        }
        Some(((seq + 2) as *const u64).read_unaligned() as usize)
    }
}

/// Whether the five bytes ending at `ret_addr` are `E8 rel32` with the target
/// `entry_ptr` — i.e. `ret_addr` is the return address of a direct self-call.
///
/// Fails closed: a return address too close to the method entry to hold the
/// instruction, an opcode that is not `E8`, or a displacement that resolves
/// anywhere other than `entry_ptr` all answer `false`.
fn returned_from_direct_self_call(ret_addr: usize, entry_ptr: usize) -> bool {
    // The call instruction lies between the method entry and the return
    // address, so a return address within 5 bytes of the entry cannot be one.
    // A self-call's caller and callee are the same method, so the method's own
    // entry both bounds the backward read and IS the target to match. Both
    // encodings `emit_call_absolute` can produce count: a self-call is subject
    // to the same rel32-reach coin flip as any other direct call, and reading
    // only the `E8` form here would call a recursive activation foreign on the
    // runs that came out absolute (see [`direct_call_target_abs64`]).
    entry_ptr != 0 && direct_call_target(ret_addr, entry_ptr) == Some(entry_ptr)
}

/// Why [`moving_young_frame_coverage_complete`] said no, counted per cause.
///
/// `ACTIVE_FRAME_MAP` / `PARENT_FRAME_MAP` are one reason code each and this
/// predicate has four ways to produce them — a method with no sp-id slot, a
/// misaligned slot, a stored id no map matches, and a matched map that does
/// not claim coverage. They are four different repairs, and after the
/// 2026-08-23 OSR fix these two codes are what
/// `bug-h2-testkillprocess-zgc-oom-at-97-percent-free-20260821-FIXED-20260829.md`'s remaining
/// classes (`TestMVStoreTool`, 5 of 8 collections) refuse on, so the split is
/// the next question rather than a nicety.
pub mod frame_coverage_reason {
    use std::sync::atomic::AtomicUsize;
    /// The compilation reserved no safepoint-id slot, so no map can be located.
    pub static NO_SP_ID_SLOT: AtomicUsize = AtomicUsize::new(0);
    /// The safepoint-id slot address is misaligned — the frame base is wrong.
    pub static MISALIGNED_SLOT: AtomicUsize = AtomicUsize::new(0);
    /// The stored id matches no `OopMapEntry` of this method. The frame has not
    /// reached a safepoint yet (its slot holds prologue-era stack residue), or
    /// the method standing at this rbp is not the one being consulted.
    pub static NO_MAP_FOR_STORED_ID: AtomicUsize = AtomicUsize::new(0);
    /// A map matched and does NOT claim shadow coverage — the honest refusal.
    pub static MAP_NOT_COMPLETE: AtomicUsize = AtomicUsize::new(0);
    /// It said yes.
    pub static COMPLETE: AtomicUsize = AtomicUsize::new(0);

    /// `(no_slot, misaligned, no_map_for_id, map_incomplete, complete)`.
    pub fn snapshot() -> (usize, usize, usize, usize, usize) {
        use std::sync::atomic::Ordering::Relaxed;
        (
            NO_SP_ID_SLOT.load(Relaxed),
            MISALIGNED_SLOT.load(Relaxed),
            NO_MAP_FOR_STORED_ID.load(Relaxed),
            MAP_NOT_COMPLETE.load(Relaxed),
            COMPLETE.load(Relaxed),
        )
    }
}

fn moving_young_frame_coverage_complete(rbp: usize, cm: &cratonvm_jit::CompiledMethod) -> bool {
    moving_young_frame_coverage_complete_at(rbp, cm, None)
}

/// As [`moving_young_frame_coverage_complete`], plus the return address the
/// rbp-chain walk resolved `cm` from — `None` for the innermost frame, whose
/// method comes from the chain entry rather than from a return address.
///
/// The extra argument exists only for the `no_map` diagnostic: the walk's
/// "is this a JIT frame" test is `lookup_jit_code_range(ret_addr).is_some()`,
/// which the A5 probe's own comment says is not sufficient — "cached function
/// pointers and JIT helper arguments that happen to point inside generated code
/// read as return PCs and fabricate a frame". `is_plausible_return_pc` is the
/// sharper test that already exists for exactly that, and reporting it here
/// says whether a `no_map` frame is a real frame at an unmapped call site or a
/// fabricated one, which are opposite repairs.
fn moving_young_frame_coverage_complete_at(
    rbp: usize,
    cm: &cratonvm_jit::CompiledMethod,
    ret_addr: Option<usize>,
) -> bool {
    use std::sync::atomic::Ordering::Relaxed;
    let sp_id_off = cm.sp_id_slot_off;
    if sp_id_off == 0 {
        frame_coverage_reason::NO_SP_ID_SLOT.fetch_add(1, Relaxed);
        return false;
    }
    let id_addr = rbp.wrapping_sub(sp_id_off as usize);
    if id_addr & 0x7 != 0 {
        frame_coverage_reason::MISALIGNED_SLOT.fetch_add(1, Relaxed);
        return false;
    }
    // SAFETY: aligned safepoint-id slot in a live JIT frame on this thread.
    let sp_id = (unsafe { (id_addr as *const usize).read() }) as u32;
    let mut found = false;
    for map in cm.oop_maps.iter().filter(|m| m.bytecode_pc == sp_id) {
        found = true;
        if !map.moving_young_coverage_complete {
            frame_coverage_reason::MAP_NOT_COMPLETE.fetch_add(1, Relaxed);
            return false;
        }
    }
    if found {
        frame_coverage_reason::COMPLETE.fetch_add(1, Relaxed);
    } else {
        frame_coverage_reason::NO_MAP_FOR_STORED_ID.fetch_add(1, Relaxed);
        // WHICH frame, and what id was standing in its slot. This is the last
        // obligation blocking `TestMVStoreTool`
        // (`bug-h2-testkillprocess-zgc-oom-at-97-percent-free-20260821-FIXED-20260829.md`):
        // `no_map=9 incomplete=0 ok=46`, so no map ever refuses on its own
        // claim — nine frames simply cannot be located. A count cannot say
        // whether that is a frame that has not reached a safepoint yet, a call
        // site that recorded no map, or a method mis-resolved from a return
        // address, and those are three different repairs.
        //
        // Rate-limited to the first 32: it fires per frame per collection.
        if cratonvm_types::flags::runtime_var_os("CRATONVM_DBG_JIT_ROOTSCAN").is_some() {
            static N: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
            if N.fetch_add(1, Relaxed) < 32 {
                // For the INNERMOST frame the walk hands no return address, so
                // read the frame's own saved one here (`[rbp + 8]`). Who the
                // CALLER is settles the remaining question: if the frame at
                // `rbp` really is `cm`'s, its return address lands inside a
                // method that calls `cm`. If the (rbp, compile-id) mirror is
                // STALE — published by a callee's prologue and not restored on
                // its return, since `emit_post_call_rbp_republish` has only six
                // call sites — the two will not correspond, and
                // `own_ret_ok=false` says the word is not a return address at
                // all.
                let own_ret = if rbp != 0 && rbp & 0x7 == 0 {
                    // SAFETY: the same aligned in-band read the chain walk makes
                    // a few lines further on, on this thread's own stack.
                    Some(unsafe { ((rbp + 8) as *const usize).read() })
                } else {
                    None
                };
                let own_caller_cm = own_ret.and_then(cratonvm_jit::lookup_jit_code_range);
                let own_caller = own_caller_cm.map(|p| {
                    // SAFETY: a resolved range is Arc-owned by the JIT cache
                    // for as long as any of its frames is live.
                    let c: &cratonvm_jit::CompiledMethod =
                        unsafe { &*(p as *const cratonvm_jit::CompiledMethod) };
                    c.method_label.clone()
                });
                // THE DISCRIMINATOR. `innermost_frame_method` prefers the
                // compile-id MIRROR (`published_innermost_method`), which
                // generated code writes in its prologue and restores after a
                // JIT->JIT call. Decoding the caller's `E8 rel32` instead
                // answers the same question from the STACK, which cannot go
                // stale. If the two disagree, the mirror is naming a frame that
                // has already returned; if they agree, these are real frames
                // live at a call site that recorded no map — opposite repairs.
                let decoded = match (own_ret, own_caller_cm) {
                    (Some(r), Some(c)) => direct_call_callee(r, c).map(|p| {
                        // SAFETY: as above.
                        let m: &cratonvm_jit::CompiledMethod = unsafe { &*p };
                        m.method_label.clone()
                    }),
                    _ => None,
                };
                eprintln!(
                    "[frame-cov] no map for stored id: sp_id={sp_id} (0x{sp_id:x}) \
                     method={} maps={} ids={:?} sp_id_slot_off={sp_id_off} rbp=0x{rbp:x} \
                     walk_ret={:?} walk_ret_ok={:?} own_ret={:?} own_ret_ok={:?} \
                     own_caller={own_caller:?} decoded_callee={decoded:?}",
                    cm.method_label,
                    cm.oop_maps.len(),
                    cm.oop_maps
                        .iter()
                        .map(|m| m.bytecode_pc)
                        .take(24)
                        .collect::<Vec<_>>(),
                    ret_addr.map(|r| format!("0x{r:x}")),
                    ret_addr.map(is_plausible_return_pc),
                    own_ret.map(|r| format!("0x{r:x}")),
                    own_ret.map(is_plausible_return_pc),
                );
            }
        }
    }
    found
}

// ---------------------------------------------------------------------------
// Moving-young coverage VERIFICATION (arch-2026-07-26
// `moving-young-corruption-rootcause`)
// ---------------------------------------------------------------------------
//
// `OopMapEntry::moving_young_coverage_complete` is the codegen's ASSERTION that
// the shadow push at a safepoint published every live oop of that frame. The
// assertion is computed by `x64::moving_young_safepoint_coverage_complete` from
// the abstract interpreter's model of the frame: `self.stack` (the operand
// stack) plus `local_oop_masks` (the Java locals). `collect_live_oop_homes`
// publishes exactly that same set.
//
// A compiled frame holds oops in storage the abstract model does not describe:
//
//   * SCALAR-REPLACED OBJECT FIELDS. Escape analysis explodes an object into
//     raw frame slots at `[rbp - (field_base_offset + k*8)]`
//     (`x64.rs` `ScalarReplacedObject`, used by the `new` / `getfield` /
//     `putfield` arms). A reference-typed field of such an object is a genuine
//     heap pointer that is neither a Java local nor an operand-stack entry.
//   * LICM HOIST SLOTS. `hoist_info` hoists a loop-invariant `aaload` into
//     `hoist_offsets[idx]`; for a reference array that slot holds an object
//     reference for the whole loop, again outside the local/operand model.
//   * THE FULL-GPR SAFEPOINT SPILL AREA (`reg_spill_base`, the SB-CRASH-04
//     spill). It exists precisely to make register-resident values visible to a
//     CONSERVATIVE scan; the shadow push does not enumerate it.
//
// On the non-moving path all three are covered, because the conservative frame
// scan reads every word of the frame. Under moving-young `memory/roots.rs`
// SUPPRESSES that scan when the coverage proof passes — so those oops are
// neither marked nor rewritten, and a Cheney copy strands or reclaims them.
// That is a silent, GC-timing-dependent under-count with the exact shape the
// known-issue records for bt18.
//
// This verifier removes the need to trust the assertion. It walks each live
// compiled frame's OWN spill band — `[rbp - osr_frame_size, rbp)`, the same
// bounded band `scan_compiled_frame_bands` uses, so intervening interpreter and
// Rust frames are excluded — and checks every word that lands in a published
// YOUNG semispace against the thread's published shadow window `[base, top)`.
// A word that is young-resident and unpublished is, by definition, a reference
// this collection could relocate and could not rewrite.
//
// Direction of error: the band scan is CONSERVATIVE, so it can flag a non-oop
// `i64` that happens to look young-resident. The cost of a false positive is
// one non-moving collection (counted, labelled, logged); the cost of a false
// negative would be heap corruption. Fail-closed everywhere: an unbounded band,
// a missing frame size, or an unresolvable shadow window all report "not
// verified".

/// Locate this thread's published shadow window `[base, top)` from a live
/// compiled frame.
///
/// The frame caches its `*mut JvmThread` in `[rbp - cm.shadow_thread_slot_off]`
/// (written by the prologue, or by the OSR trampoline for an OSR entry), and
/// the `ShadowStack` sits at `thread + cm.shadow_off_in_thread` with the
/// `#[repr(C)]` layout asserted in `cratonvm_gc::shadow_stack`.
///
/// `None` means "cannot resolve" — including the legitimate null-thread case
/// (`maybe_nop_out_shadow_fetch` NOPs the prologue fetch for a method that
/// never emitted a push, leaving the slot null). The caller treats `None` as an
/// EMPTY published set rather than as an error: a method that published nothing
/// is fine exactly as long as its band contains no young words, which is what
/// the band scan then goes on to check.
fn shadow_window_from_frame(
    rbp: usize,
    cm: &cratonvm_jit::CompiledMethod,
) -> Option<(usize, usize)> {
    shadow_window_from_frame_why(rbp, cm)
        .or_else(|e| {
            if band_thread_window_disabled() {
                Err(e)
            } else {
                shadow_window_for_thread_why(cm)
            }
        })
        .ok()
}

/// `shadow_window_from_frame`, but NAMING the refusal instead of erasing it.
///
/// §25.4. The band reporter printed `published=0` on all 272 of a run's reports
/// and that read as "the shadow push declined to publish these oops" -- a
/// codegen fact, pointing at `collect_live_oop_homes`. It was not: the window
/// was never resolved, so `published_shadow_values` had nothing to read and
/// returned an empty set. `Option` folds ten distinct refusals and a genuinely
/// empty stack into one indistinguishable `None`, and those point at opposite
/// repairs -- a missing reserved slot is a codegen gap, a foreign thread
/// pointer is a frame mis-attribution, a size mismatch is a torn read.
///
/// The wrapper above is the only caller that discards the reason, so the
/// diagnostic cannot drift away from the path actually taken.
fn shadow_window_from_frame_why(
    rbp: usize,
    cm: &cratonvm_jit::CompiledMethod,
) -> Result<(usize, usize), &'static str> {
    let thr_off = cm.shadow_thread_slot_off;
    if thr_off <= 0 || cm.shadow_off_in_thread <= 0 || rbp < thr_off as usize {
        return Err("no-thread-slot");
    }
    let slot = rbp - thr_off as usize;
    if slot & 0x7 != 0 {
        return Err("frame-slot-misaligned");
    }
    // SAFETY: `slot` is an aligned address inside this thread's own live
    // compiled frame, bounded by the same frame-size check the caller applied
    // before calling here.
    let thread_ptr = unsafe { (slot as *const usize).read() };
    if thread_ptr == 0 || thread_ptr & 0x7 != 0 {
        return Err("thread-null-or-misaligned");
    }
    // That word is only a thread pointer when `cm` really describes the frame
    // at `rbp`. Callers are expected to have excluded the mis-attribution case
    // (`chain_entry_rbp_is_foreign`), but a wrong `cm` turns this into an
    // arbitrary aligned stack word and the two loads below would dereference
    // it — which is exactly how the band verifier SIGSEGV'd on a `base` of
    // `0x5555_0000_0004`. Every compiled frame on this thread caches THIS
    // thread's `JvmThread`, so reject anything else whenever a thread is
    // installed (it is null only in unit tests and on threads that never
    // entered JIT code, where there is no frame to mis-attribute).
    let current = crate::jit::helpers::current_jit_thread_ptr() as usize;
    if current != 0 && thread_ptr != current {
        return Err("foreign-thread");
    }
    shadow_window_at(thread_ptr, cm.shadow_off_in_thread)
}

/// The thread's shadow window WITHOUT going through a frame slot.
///
/// §25.4's repair. `shadow_window_from_frame_why` reads the cached thread
/// pointer out of `[rbp - shadow_thread_slot_off]`, and `ir_lower`'s
/// `finish_lazy_thread_fetch` ERASES the fetch that fills that slot whenever
/// the method publishes nothing (`!shadow_pushed_any`) -- so a leaf that has no
/// oops to push leaves the slot zero BY DESIGN. That is a correct optimisation;
/// the band verifier's use of it was not. It resolved the window from the
/// INNERMOST frame alone, and when that frame was such a leaf the window came
/// back `None`, `published_shadow_values` returned an empty set, and every oop
/// in every OUTER frame read as unpublished. Measured on `CoverageBench`: 228
/// of 229 pauses declined relocation with `compiled-frame-oop-not-published`
/// while `main` had pushed 3 of its 3 mapped slots and `alloc` -- the innermost
/// frame, whose map names nothing -- was the only reason the window was lost.
///
/// The shadow stack is per-THREAD, so the frame slot was only ever a cache.
/// When a thread is installed, ask it directly. The frame path stays for the
/// case that has no installed thread (unit tests, and threads that never
/// entered JIT code), which is also the only case where the two could disagree
/// -- `shadow_window_from_frame_why` refuses a `thread_ptr` that is not
/// `current` anyway.
fn shadow_window_for_thread_why(
    cm: &cratonvm_jit::CompiledMethod,
) -> Result<(usize, usize), &'static str> {
    if cm.shadow_off_in_thread <= 0 {
        return Err("no-shadow-offset");
    }
    let current = crate::jit::helpers::current_jit_thread_ptr() as usize;
    if current == 0 {
        return Err("no-installed-thread");
    }
    if current & 0x7 != 0 || current < 0x1_0000 {
        return Err("thread-null-or-misaligned");
    }
    shadow_window_at(current, cm.shadow_off_in_thread)
}

/// The shared half: validate `thread_ptr + shadow_off` as a `ShadowStack` and
/// return its `[base, top)` window. Both resolvers end here, so a window
/// reached through the frame cache and one reached through the installed
/// thread are held to identical invariants.
fn shadow_window_at(
    thread_ptr: usize,
    shadow_off: i32,
) -> Result<(usize, usize), &'static str> {
    let ss = thread_ptr
        .checked_add(shadow_off as usize)
        .ok_or("shadow-addr-overflow")?;
    if ss & 0x7 != 0 {
        return Err("shadow-addr-misaligned");
    }
    // `thread_ptr` came out of a raw frame slot and is trustworthy ONLY when
    // `cm` really describes the frame at `rbp`. It does not always: an
    // unguarded JIT->JIT call publishes the CALLEE's frame base into the chain
    // entry (`chain_entry_rbp_is_foreign`), so `[rbp - shadow_thread_slot_off]`
    // addresses an arbitrary word of a different frame -- a spilled `long`, a
    // `Value` discriminant, an interior pointer. Non-zero and 8-aligned (all
    // this used to require) is a bar such a word clears constantly, and the
    // three reads below then dereference it. That is a real SIGSEGV, not a
    // hypothetical: `ApplicationContextAotGeneratorTests
    // .processAheadOfTimeWithPropertySource` faulted here on `addr=0xefc`
    // roughly three minutes into every run, taking the whole class's `RESULT`
    // line with it.
    //
    // Read `end` as well and require the exact `#[repr(C)]` invariant
    // `ShadowStack::ensure_allocated` establishes and `set_top` maintains:
    // three 8-aligned addresses with `base < end`, `end - base` EXACTLY the
    // fixed buffer size, and `top` inside `[base, end]`. Garbage that
    // satisfies all of that would have to be an actual shadow stack.
    let read_word = |off: usize| -> Option<usize> {
        let at = ss.checked_add(off)?;
        // SAFETY: `ss` is the candidate `ShadowStack` address; the caller has
        // no way to prove it is mapped, which is exactly what the checks below
        // are for -- but the read itself must not fault. Reject anything that
        // is not a plausible userspace address before dereferencing.
        if at < 0x1_0000 || at & 0x7 != 0 {
            return None;
        }
        Some(unsafe { (at as *const usize).read() })
    };
    if thread_ptr < 0x1_0000 {
        return Err("thread-addr-low");
    }
    let top = read_word(cratonvm_gc::shadow_stack::ShadowStack::TOP_OFFSET)
        .ok_or("top-unreadable")?;
    let end = read_word(cratonvm_gc::shadow_stack::ShadowStack::END_OFFSET)
        .ok_or("end-unreadable")?;
    let base = read_word(cratonvm_gc::shadow_stack::ShadowStack::BASE_OFFSET)
        .ok_or("base-unreadable")?;
    const SHADOW_BYTES: usize = cratonvm_gc::shadow_stack::DEFAULT_SHADOW_SLOTS * 8;
    if base < 0x1_0000 || base & 0x7 != 0 || top & 0x7 != 0 || end & 0x7 != 0 {
        return Err("fields-misaligned");
    }
    if end.checked_sub(base) != Some(SHADOW_BYTES) {
        return Err("buffer-size-mismatch");
    }
    if top < base || top > end {
        return Err("top-out-of-range");
    }
    Ok((base, top))
}

/// Collect the values currently published on the shadow window `[base, top)`.
fn published_shadow_values(window: Option<(usize, usize)>) -> std::collections::HashSet<usize> {
    let mut out = std::collections::HashSet::new();
    let Some((base, top)) = window else {
        return out;
    };
    // A pathological `top` cannot widen this beyond the backing buffer, which
    // `ShadowStack::set_top` clamps; bound it anyway so a torn read cannot walk
    // off the allocation.
    const MAX_SLOTS: usize = cratonvm_gc::shadow_stack::DEFAULT_SHADOW_SLOTS;
    let slots = ((top - base) / 8).min(MAX_SLOTS);
    for i in 0..slots {
        // SAFETY: aligned slot inside the thread's own shadow-stack buffer.
        let v = unsafe { ((base + i * 8) as *const usize).read() };
        out.insert(v);
    }
    out
}

/// Verify that every young-resident word in this thread's live compiled frame
/// bands was published on the shadow stack.
///
/// Returns `true` when at least one word was **not** published (or when the
/// bands could not be bounded), i.e. when moving-young must NOT relocate this
/// cycle. See the module block above for why the codegen's own
/// `moving_young_coverage_complete` bit is not sufficient.
///
/// `reason_out` receives the specific `incomplete_reason` code on a `true`
/// result so the fallback histogram can distinguish "an oop was missed" from
/// "the frame could not be inspected at all".
pub fn moving_young_unpublished_frame_oop_present(reason_out: &mut usize) -> bool {
    if !moving_young_enabled() || band_verify_disabled() {
        return false;
    }
    // The band scan's residency test asks "could a moving cycle relocate the
    // object at this address?", and until 2026-08-21 it asked that as
    // `gen_heap::addr_in_published_young_regions`, which reads
    // `JIT_REGION_BOUNDS`. That table has one writer and it is
    // generational-only — G1 deliberately keeps it empty, ZGC never filled it.
    // Where it is empty the test answers `false` for EVERY address, so the scan
    // below inspects every verifiable slot, classifies none of them as young,
    // and returns "nothing unpublished" without having verified anything.
    //
    // That vacuous pass is not a theoretical hazard: it is how `PolynomialTest`
    // got `incomplete=false` under `-XX:+UseG1GC`, which let `roots.rs`
    // suppress the conservative scan, which left G1's pin set empty, which let
    // the pause evacuate a region a live compiled frame still referenced.
    //
    // It now asks `gen_heap::addr_is_movable`, the union of that young table
    // with `MOVABLE_BOUNDS` — a third table a collector fills to say what its
    // relocating phase may move, precisely because filling `JIT_REGION_BOUNDS`
    // to fix this verifier would silently re-enable an inline reference STORE
    // fast path G1 and ZGC must not have. ZGC publishes its arena envelope
    // there; generational still answers through the young table, so its
    // behaviour is unchanged.
    //
    // Fail closed, exactly as the module block above says this verifier does
    // for an unbounded band or an unresolvable shadow window: an uninspectable
    // frame reports "not verified", never "verified clean". The gate is on the
    // UNION being live, so a collector that publishes neither table still gets
    // the refusal it had before rather than a quiet pass.
    //
    // The gate has TWO ways to fire and they are reported apart, because the
    // second one looks like success from every angle the first is checked
    // from. A table can be useless because it is empty (nobody published) or
    // because it is about somebody else: both tables are process-global and
    // discriminated by slot 0, so each describes exactly ONE heap, and a second
    // live heap leaves the loser's every address answering `false` to
    // `addr_is_movable` — published, fresh, and not about these frames. See
    // `gen_heap::RELOCATABLE_HEAPS_LIVE`.
    if bounds_guard_enabled() {
        if !cratonvm_gc::gen_heap::published_bounds_represent_every_live_heap() {
            *reason_out =
                cratonvm_gc::gc_quiescence::incomplete_reason::BOUNDS_NOT_REPRESENTATIVE;
            return true;
        }
        if !cratonvm_gc::gen_heap::movable_bounds_are_live() {
            *reason_out = cratonvm_gc::gc_quiescence::incomplete_reason::YOUNG_BOUNDS_UNPUBLISHED;
            return true;
        }
    }
    let scanner_sp = current_stack_pointer();
    let mut unverified = false;
    let mut unpublished = false;
    JIT_ENTRY_CHAIN.with(|c| {
        {
            let mut chain = c.borrow_mut();
            flush_top_rbp_cache_to_chain(chain.as_mut_slice());
        }
        let chain = c.borrow();
        for entry in chain.iter() {
            let Some(info) = entry.precise else {
                // A conservative (map-less) entry is already reported by the
                // NO_PRECISE_MAP obligation; nothing to verify here.
                continue;
            };
            let entry_sp = entry.entry_sp;
            let mut rbp = info.exact_rbp;
            if rbp == 0 || rbp & 0x7 != 0 || rbp < scanner_sp || rbp >= entry_sp {
                // MISSING_EXACT_RBP already covers rbp == 0; an out-of-range
                // value means the band cannot be located at all.
                unverified = true;
                continue;
            }
            let Some(innermost_cm) = innermost_frame_method(
                rbp,
                info.exact_cm_id,
                entry_sp,
                scanner_sp,
                info.compiled_method,
            ) else {
                // Nothing describes the frame now standing at `exact_rbp` — the
                // inline MIC/PIC cascade or the hashed megamorphic stub reached
                // it through an indirect call, so its method cannot be recovered
                // from the return address. This is the case
                // `refresh_moving_young_coverage_for_current_thread` reports as
                // FOREIGN_INNERMOST_RBP. Every offset this loop reads
                // (`shadow_thread_slot_off`, `osr_frame_size`, the band bounds)
                // would belong to the wrong method, so nothing here can be
                // verified. `scan_one_frame_precise` already declines to publish
                // an oop map under the same condition; declining to VERIFY is the
                // fail-closed counterpart -- it forces the non-moving sweep for
                // this cycle instead of trusting a band read out of the wrong
                // frame.
                unverified = true;
                continue;
            };
            // SAFETY: same contract as `scan_compiled_frame_bands` — the chain
            // entry's CompiledMethod is Arc-owned by the JIT cache while any of
            // its frames is live, and a resolved callee is kept alive by the
            // live frame whose return address resolved it.
            let mut cm: &cratonvm_jit::CompiledMethod = unsafe { &*innermost_cm };
            // §25.4 -- keep the WINDOW, not just the values read out of it.
            // `published_shadow_values(None)` and a genuinely empty window both
            // return an empty set, so `published=0` alone cannot say whether
            // publication was declined or the window could not be resolved at
            // all. Those point at opposite repairs.
            let window = shadow_window_from_frame(rbp, cm);
            let published = published_shadow_values(window);
            let mut frames = 0usize;
            while frames < 4096 {
                frames += 1;
                let frame_size = cm.osr_frame_size;
                if frame_size <= 0 {
                    unverified = true;
                    break;
                }
                let frame_size = frame_size as usize;
                const MAX_COMPILED_FRAME_BYTES: usize = 1024 * 1024;
                if frame_size > MAX_COMPILED_FRAME_BYTES || frame_size > rbp {
                    unverified = true;
                    break;
                }
                let live_hi = moving_young_frame_live_hi(rbp, cm);
                if band_has_unpublished_young_word(rbp, frame_size, cm, live_hi, &published) {
                    if band_dbg() {
                        report_unpublished_band_words(
                            rbp, frame_size, cm, live_hi, &published, window,
                        );
                    }
                    unpublished = true;
                    break;
                }
                // `[rbp]` = saved caller RBP, `[rbp + 8]` = return PC into it.
                // SAFETY: `rbp` was validated to lie in this thread's live JIT
                // stack interval.
                let parent_rbp = unsafe { (rbp as *const usize).read() };
                let ret_addr = unsafe { ((rbp + 8) as *const usize).read() };
                let Some(parent_cm_ptr) = cratonvm_jit::lookup_jit_code_range(ret_addr) else {
                    // A non-JIT parent ends the walk cleanly: it owns no
                    // compiled spill band.
                    break;
                };
                if parent_rbp <= rbp
                    || parent_rbp & 0x7 != 0
                    || parent_rbp >= entry_sp
                    || parent_rbp < scanner_sp
                {
                    unverified = true;
                    break;
                }
                // SAFETY: code ranges retain their CompiledMethod metadata for
                // the lifetime of an active frame.
                cm = unsafe { &*(parent_cm_ptr as *const cratonvm_jit::CompiledMethod) };
                rbp = parent_rbp;
            }
            if unpublished {
                break;
            }
        }
    });
    if unpublished {
        *reason_out = cratonvm_gc::gc_quiescence::incomplete_reason::UNPUBLISHED_FRAME_OOP;
        return true;
    }
    if unverified {
        *reason_out = cratonvm_gc::gc_quiescence::incomplete_reason::UNBOUNDED_FRAME_BAND;
        return true;
    }
    false
}

/// `CRATONVM_MOVING_YOUNG_BAND_DBG` — dump the frame offset of every word the
/// band scan rejected, so the storage class responsible can be named instead of
/// guessed at. Latched once; the scan runs on every collection.
fn band_dbg() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        cratonvm_types::flags::runtime_var_os("CRATONVM_MOVING_YOUNG_BAND_DBG").is_some()
    })
}

/// The safepoint id the frame at `rbp` is standing on, or `None` when the
/// method reserved no id slot or the slot is misaligned.
///
/// Same read as `moving_young_frame_coverage_complete` makes; split out so the
/// band reporter can name the ACTIVE map without duplicating the contract.
/// Diagnostic only — every caller is behind `CRATONVM_MOVING_YOUNG_BAND_DBG`.
fn frame_active_sp_id(rbp: usize, cm: &cratonvm_jit::CompiledMethod) -> Option<u32> {
    let off = cm.sp_id_slot_off;
    if off == 0 {
        return None;
    }
    let addr = rbp.checked_sub(off as usize)?;
    if addr & 0x7 != 0 {
        return None;
    }
    // SAFETY: aligned safepoint-id slot of a live JIT frame on this thread,
    // reached through the same bounds the caller already validated `rbp` with.
    let v = unsafe { (addr as *const usize).read() };
    Some(v as u32)
}

fn report_unpublished_band_words(
    rbp: usize,
    frame_size: usize,
    cm: &cratonvm_jit::CompiledMethod,
    live_hi: Option<i32>,
    published: &std::collections::HashSet<usize>,
    // The shadow window `published` was read out of, resolved from the
    // INNERMOST frame of this chain. `None` means `shadow_window_from_frame`
    // declined -- no reserved thread slot, a misaligned or foreign thread
    // pointer -- which is a different fact from a window that resolved and held
    // nothing, and the `published` set cannot tell the two apart.
    window: Option<(usize, usize)>,
) {
    let win = match window {
        None => format!(
            "unresolved(frame={} thread={})",
            shadow_window_from_frame_why(rbp, cm).err().unwrap_or("ok"),
            shadow_window_for_thread_why(cm).err().unwrap_or("ok"),
        ),
        Some((base, top)) => format!("0x{base:x}..0x{top:x}({})", top.saturating_sub(base) / 8),
    };
    let map_slots = frame_active_map_slots(rbp, cm);
    let lo = rbp - frame_size;
    let mut addr = (lo + 7) & !7usize;
    let mut hits = 0usize;
    while addr + 8 <= rbp && hits < 32 {
        // SAFETY: same bounded, aligned walk as `band_has_unpublished_word_with`.
        let w = unsafe { (addr as *const usize).read() };
        // Cast: a compiled frame is far smaller than i32::MAX bytes.
        let off = (rbp - addr) as i32;
        if band_slot_is_verifiable_with_map(off, &cm.frame_layout, live_hi, map_slots.as_ref())
            && cratonvm_gc::gen_heap::addr_is_movable(w)
            && !published.contains(&w)
        {
            hits += 1;
            // THE FORK, decided per word. `in_map` asks whether the ACTIVE
            // safepoint's oop map already names this slot:
            //
            //   in_map=true  — the dataflow calls the slot a LIVE reference here
            //                  and the map names it, but the shadow push did not
            //                  publish it. The two channels disagree, and
            //                  `collect_live_oop_homes` is the side that is
            //                  wrong.
            //   in_map=false — the dataflow does NOT call it live. Declining to
            //                  publish is then CORRECT, the slot merely holds a
            //                  stale reference nothing will read, and the band
            //                  verifier is refusing on a dead word. The repair
            //                  belongs on the verifier (a liveness bound it can
            //                  consult), not on codegen.
            //
            // Without this the two are indistinguishable from the outside, which
            // is how `bug-h2-testkillprocess-zgc-oom-at-97-percent-free`'s
            // `MVStore.closeStore` residual sat unresolved: 64 of 83 unpublished
            // words are that method's java-locals, one object in three
            // consecutive slots.
            let sp_id = frame_active_sp_id(rbp, cm);
            // THREE answers, not two. `sp_id.map(..)` gave `Some(false)` both
            // when a map was found and did not name the slot AND when NO MAP
            // EXISTS for the stored id -- and those point at opposite repairs.
            //
            // Measured on `org.h2.test.jdbc.TestCachedQueryResults`
            // (2026-08-29): six of seven reported words came from one frame
            // whose stored id was `1729768472`. That is not a bytecode pc; it
            // is `0x671a2c18`, the low 32 bits of `0x200671a2c18` -- the heap
            // pointer this same scan reports two slots away in the same frame.
            // The frame's sp-id slot holds half an object pointer, so no map
            // can match it, and `live_hi=None` on the same line says the same
            // thing. Printed as `Some(false)` that read as "the dataflow calls
            // this slot dead", which sends a reader at the band verifier
            // instead of at the frame whose id is garbage.
            //
            // `no-map-for-id` is also the honest label for what the SCAN does
            // here: `frame_active_map_slots` returns `None`, so the dead-slot
            // relaxation deliberately does not fire and the word is reported.
            // The report now says which of the two it is.
            let in_map: &'static str = match sp_id {
                None => "no-sp-id",
                Some(id) => {
                    let mut any_map = false;
                    let mut names_slot = false;
                    for m in cm.oop_maps.iter().filter(|m| m.bytecode_pc == id) {
                        any_map = true;
                        if m.frame_slot_offsets.iter().any(|s| i32::from(*s) == off) {
                            names_slot = true;
                        }
                    }
                    if !any_map {
                        "no-map-for-id"
                    } else if names_slot {
                        "true"
                    } else {
                        "false"
                    }
                }
            };
            // §25.3's PROBE -- the safepoint KIND, printed as
            // `pushed/named` per map matching the resolved id.
            //
            // Four candidate explanations for an `in_map=true` word that the
            // shadow push did not publish were eliminated in §25.1-.2, and the
            // structural fact that survived is that publication is CALL-SCOPED:
            // `emit_shadow_push` runs on the path into a call, and
            // `emit_shadow_reload` pops the homes again at the top of
            // `emit_oop_map_for_safepoint`. A safepoint that is not call-shaped
            // -- a loop poll, a re-entry -- therefore carries a map naming N
            // slots while nothing was ever pushed for it. The band verifier's
            // obligation has no such scope: it asks about EVERY safepoint.
            //
            // `0/N` on the reporting safepoints is that answer, and it puts the
            // repair in the PAIRING (publish for poll-shaped safepoints too, or
            // let the verifier consult the map instead of the push) rather than
            // in either side alone. A nonzero left-hand number refutes it and
            // sends the search back to `collect_live_oop_homes`.
            let shadow_pushed: String = match sp_id {
                None => "n/a".to_string(),
                Some(id) => {
                    let v: Vec<String> = cm
                        .oop_maps
                        .iter()
                        .filter(|m| m.bytecode_pc == id)
                        .map(|m| format!("{}/{}", m.shadow_pushed, m.frame_slot_offsets.len()))
                        .collect();
                    if v.is_empty() {
                        "none".to_string()
                    } else {
                        v.join(",")
                    }
                }
            };
            // THE DISCRIMINATOR for a `no-map-for-id` frame, and the reason
            // this dump exists at all.
            //
            // A safepoint id that is not a bytecode pc has two opposite causes:
            // something STORED an oop into the reserved sp-id slot (a codegen
            // defect at that offset), or `rbp` is wrong for this frame so the
            // read lands on a NEIGHBOURING slot that legitimately holds one (a
            // frame-resolution defect, the family of the 2026-08-26
            // innermost-mirror repair). Printing the whole reserved-locals tail
            // beside `sp_id_off` separates them in one line: a pointer sitting
            // exactly at `sp_id_off` is the first; the whole tail reading like
            // some neighbouring frame's is the second.
            if in_map == "no-map-for-id" {
                let lo = cm.frame_layout.java_locals_hi.max(8);
                let hi = cm.frame_layout.locals_hi;
                let mut tail = String::new();
                let mut o = lo;
                while o <= hi && o - lo < 128 {
                    let a = rbp.wrapping_sub(o as usize);
                    if a >= rbp - frame_size && a + 8 <= rbp && a & 7 == 0 {
                        // SAFETY: the same bounded, aligned in-band read the
                        // walk above makes, on this thread's own frame.
                        let v = unsafe { (a as *const usize).read() };
                        tail.push_str(&format!(" [{o}]=0x{v:x}"));
                    }
                    o += 8;
                }
                eprintln!(
                    "[moving-young-band]   no-map-for-id sp_id_off={} tail({}..{}):{}",
                    cm.sp_id_slot_off, cm.frame_layout.java_locals_hi, hi, tail,
                );
            }
            eprintln!(
                "[moving-young-band] {} off={off} region={} value=0x{w:x} published={} window={win} \
                 sp_id={sp_id:?} sp_id_off={} in_map={in_map} shadow_pushed={shadow_pushed} \
                 live_hi={live_hi:?} layout={:?}",
                cm.method_label,
                cm.frame_layout.region_name(off),
                published.len(),
                cm.sp_id_slot_off,
                cm.frame_layout,
            );
        }
        addr += 8;
    }
}

/// `CRATONVM_MOVING_YOUNG_NO_BAND_VERIFY` — drop the frame-band verification
/// entirely and take the codegen's `moving_young_coverage_complete` bit at its
/// word. A MEASUREMENT INSTRUMENT: it is how "does the codegen model actually
/// cover this workload?" is asked, and it is unsafe to run with if the answer
/// is no. Not a supported configuration.
/// `CRATONVM_MOVING_YOUNG_NO_BOUNDS_GUARD=1` — let the frame-band verifier run
/// against an unpublished young-bounds table, i.e. let it pass vacuously.
///
/// The pre-fix behaviour, kept as an A/B arm. Unsafe on any collector that does
/// not publish `JIT_REGION_BOUNDS`, which is every collector except the
/// generational one.
fn bounds_guard_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        cratonvm_types::flags::runtime_var_os("CRATONVM_MOVING_YOUNG_NO_BOUNDS_GUARD").is_none()
    })
}

fn band_verify_disabled() -> bool {
    static OFF: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *OFF.get_or_init(|| {
        cratonvm_types::flags::runtime_var_os("CRATONVM_MOVING_YOUNG_NO_BAND_VERIFY").is_some()
    })
}

/// `CRATONVM_MOVING_YOUNG_NO_JIT` — re-arm the process-wide JIT relocation
/// boundary: refuse every moving young collection while ANY compiled code
/// exists, without consulting the per-frame coverage proof.
///
/// This was the hard-coded behaviour between `86e69e848` (2026-07-29) and the
/// default-on closeout; see [`refresh_moving_young_coverage_for_current_thread`]
/// for why the per-cycle proof is the authority instead. Kept as a knob because
/// it is the one setting that makes "is this a moving-young defect?" a
/// single-variable experiment, and because it is the correct emergency lever if
/// a workload ever exposes an obligation the verifier does not model.
fn moving_young_no_jit() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        cratonvm_types::flags::runtime_var_os("CRATONVM_MOVING_YOUNG_NO_JIT").is_some()
    })
}

/// Scan one compiled frame's band `[rbp - frame_size, rbp)` for a word that
/// lands in a published young semispace and is absent from `published`.
///
/// Slots that are only ever an IMAGE of a register are skipped
/// ([`cratonvm_jit::FrameLayout::is_register_image`]): the prologue's
/// callee-saved GPR/XMM save area holds the CALLER's values, and the
/// per-safepoint blind GPR spill is write-only. Neither is a place the owning
/// frame resumes from, and both are full of DEAD register values — scanning
/// them yields "an oop was missed" on every collection, which is a false
/// verdict that costs every moving cycle. The genuine storage classes (Java
/// locals, operand spills, LICM hoist slots, scalar-replacement fields,
/// outgoing stack args) are all still scanned.
fn band_has_unpublished_young_word(
    rbp: usize,
    frame_size: usize,
    cm: &cratonvm_jit::CompiledMethod,
    live_hi: Option<i32>,
    published: &std::collections::HashSet<usize>,
) -> bool {
    let map_slots = frame_active_map_slots(rbp, cm);
    // The safepoint id is what makes the verifier's answer specific to THIS
    // point in the method; without it there is no liveness question to ask.
    let sp_id = active_safepoint_id(rbp, cm);
    let ask = sp_id.map(|bci| move |off: i32| verifier_local_verdict(cm, off, bci));
    let ask_ref: Option<&dyn Fn(i32) -> VerifierSlotVerdict> = ask.as_ref().map(|x| x as _);
    band_has_unpublished_word_with_map(
        rbp,
        frame_size,
        &cm.frame_layout,
        live_hi,
        published,
        map_slots.as_ref(),
        cratonvm_gc::gen_heap::addr_is_movable,
        // The production predicate, whose range is the committed prefix.
        true,
        ask_ref,
    )
}

/// Whether the verifier must inspect the word at `[rbp - off]`.
///
/// Skipped:
///
///   * everything at or beyond `callee_saved_lo` — the prologue's callee-saved
///     GPR/XMM save area, the write-only per-safepoint blind GPR spill, the
///     frame-deopt `SavedRegisters` block, the ABI shadow space and the
///     outgoing stack-arg reserve. Register IMAGES and outgoing arguments, not
///     storage this frame resumes from, and full of dead register values;
///   * operand-spill slots above the safepoint's live cursor. The cursor
///     reclaims by moving, so those slots keep whatever the deepest earlier
///     operand stack left in them — in allocation-heavy code, a stale object
///     pointer. Nothing reads them again.
///
/// Scanned: Java locals, the reserved-locals tail, LICM hoist slots,
/// scalar-replacement fields and the live operand-spill slots — every place a
/// compiled frame actually keeps a reference it will use after the call.
/// [`active_map_slots`] as a set, for the band scan's per-word membership test.
///
/// `None` — no id, or no map for it — is the fail-closed answer: it leaves every
/// band word verifiable, because without a liveness statement the scan must not
/// assume anything is dead.
fn frame_active_map_slots(
    rbp: usize,
    cm: &cratonvm_jit::CompiledMethod,
) -> Option<std::collections::HashSet<i32>> {
    Some(
        active_map_slots(rbp, cm)?
            .into_iter()
            .map(i32::from)
            .collect(),
    )
}

/// Is `off` in a region the JIT's abstract interpreter MODELS?
///
/// Java locals and operand-spill slots are exactly what
/// `moving_young_coverage_complete` is computed from, so for those the
/// safepoint's oop map carries a definite liveness statement. Everything else
/// the band scan looks at — LICM hoist slots, scalar-replacement fields, the
/// reserved-locals tail — is precisely what the map has NO opinion about, which
/// is the gap the band verifier was built to close (see
/// `frame_band_scan_rejects_a_relocatable_word_the_shadow_stack_never_published`).
fn region_is_dataflow_modelled(layout: &cratonvm_jit::FrameLayout, off: i32) -> bool {
    if layout.java_locals_hi > 0 && off < layout.java_locals_hi {
        return true;
    }
    layout.spill_hi > layout.spill_lo && off >= layout.spill_lo && off < layout.spill_hi
}

/// `CRATONVM_GC_NO_BAND_MAP_LIVENESS=1` — make the band verifier inspect every
/// word in a modelled region again, whether or not the active safepoint map
/// names it.
///
/// The bisect lever for the dead-slot exemption, default-ON.
fn band_map_liveness_enabled() -> bool {
    static G: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *G.get_or_init(|| {
        cratonvm_types::flags::runtime_var_os("CRATONVM_GC_NO_BAND_MAP_LIVENESS").is_none()
    })
}

fn band_slot_is_verifiable(
    off: i32,
    layout: &cratonvm_jit::FrameLayout,
    live_hi: Option<i32>,
) -> bool {
    band_slot_is_verifiable_with_map(off, layout, live_hi, None)
}

/// [`band_slot_is_verifiable`] plus the ACTIVE safepoint's mapped slot offsets,
/// when one resolved.
///
/// `map_slots = Some(set)` means the dataflow has stated, for this exact pc,
/// which slots hold live references. In a region it MODELS, a word it does not
/// name is DEAD: the slot holds whatever an earlier scope left there, and
/// nothing will read it again — so demanding that it be published on the shadow
/// stack refuses a collection over a word that needs no rewriting.
///
/// Measured on `bug-h2-testkillprocess-zgc-oom-at-97-percent-free-20260821`:
/// **all 74** of one run's unpublished words reported `in_map=false`, 64 of them
/// in `MVStore.closeStore:(ZI)V` at `sp_id=174`, four slots (offsets 48/56/64/72
/// = locals 5..8) all holding the SAME object. `closeStore`'s
/// `LocalVariableTable` puts slot 5 (`map`) in scope 149..170, so at bci 174 it
/// is out of scope and 6..8 are javac's loop/finally scaffolding copies of it.
/// The map, the shadow push and the dataflow all agreed; only the band scan
/// objected, and it was objecting to dead words.
///
/// `None` — no map resolved for the frame's sp-id — keeps every word verifiable,
/// which is the fail-closed direction: without a liveness statement the scan
/// makes no assumption about what is dead.
fn band_slot_is_verifiable_with_map(
    off: i32,
    layout: &cratonvm_jit::FrameLayout,
    live_hi: Option<i32>,
    map_slots: Option<&std::collections::HashSet<i32>>,
) -> bool {
    // The half-line: on a frame whose callee-save area is DEEPEST (x86-64),
    // everything at or beyond it is a register image or past the frame. A
    // backend that puts that area next to the frame pointer instead says so,
    // and gets the range exclusion below rather than this -- otherwise the
    // half-line would swallow its whole spill area, which is precisely the
    // region the oop maps describe.
    if !layout.callee_saved_shallow && layout.callee_saved_lo > 0 && off >= layout.callee_saved_lo
    {
        return false;
    }
    if layout.is_register_image(off) {
        return false;
    }
    if let Some(hi) = live_hi {
        if layout.spill_hi > layout.spill_lo && off >= layout.spill_lo && off >= hi {
            return false;
        }
    }
    if band_map_liveness_enabled() {
        if let Some(slots) = map_slots {
            if region_is_dataflow_modelled(layout, off) && !slots.contains(&off) {
                return false;
            }
        }
    }
    // §22.3's experiment, opt-in: a slot the ACTIVE OOP MAP names is covered by
    // the precise path, so the shadow stack need not also publish it.
    //
    // This is the COMPLEMENT of the rule just above. That one says "modelled
    // region, absent from the map => dead, skip"; this one says "present in the
    // map => the collector already visits it, skip". Together they leave only
    // the unmodelled regions, which is where §22.2's dump puts the other 26 of
    // 93 words (`reserved-locals-tail`, `in_map=false`).
    //
    // The claim it rests on: `scan_active_oop_map_at_rbp` resolves the map for
    // this safepoint and iterates its `slot_offsets`, so the root scan visits
    // the slot and `remap_active_jit_frames` rewrites it after a move. If that
    // holds, the band test reporting the slot as "unpublished" is asking about
    // a mechanism (shadow-stack publication) that is not the one covering it.
    //
    // OPT-IN, and REFUTED — see §24 and the flag's own doc. The claim above is
    // false in practice: with this on, the stale-after-remap detector reports
    // 260-545 stale words per run against 2-18 with it off, 54-140 of them in
    // java locals. Whatever `in_map` guarantees, it is not "the precise path
    // rewrote this slot", and the shadow-publication requirement is not
    // redundant for such words. The switch survives as the lever that measured
    // that, and it is fail-OPEN, so off is safe.
    if band_skip_in_map_enabled() {
        if let Some(slots) = map_slots {
            if slots.contains(&off) {
                return false;
            }
        }
    }
    true
}

/// `CRATONVM_MOVING_YOUNG_BAND_SKIP_IN_MAP=1` — §22.3's experiment, and
/// **REFUTED**. Do not enable it outside that experiment.
///
/// The claim was that a slot the active oop map names is rewritten by
/// `remap_active_jit_frames`, so the shadow stack need not also publish it.
/// The stale-after-remap detector says otherwise (§24): with this on,
/// `CoverageBench` leaves 260-545 stale words per run against 2-18 with it off,
/// 54-140 of them in `region=java-local` against 0-10. Those are references the
/// collector moved and nothing rewrote — precisely the corruption the
/// shadow-publication requirement exists to prevent.
///
/// Kept, rather than deleted, because it is the lever that produced that
/// answer and the one that would re-test it if the underlying mechanism ever
/// changes. It is fail-OPEN, so leaving it off is leaving it safe.
fn band_skip_in_map_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        cratonvm_types::flags::runtime_var_os("CRATONVM_MOVING_YOUNG_BAND_SKIP_IN_MAP").is_some()
    })
}

/// Predicate-injected core of [`band_has_unpublished_young_word`], so the scan
/// itself is unit-testable without mutating the process-global published
/// region-bounds table (which parallel tests share).
fn band_has_unpublished_word_with(
    rbp: usize,
    frame_size: usize,
    layout: &cratonvm_jit::FrameLayout,
    live_hi: Option<i32>,
    published: &std::collections::HashSet<usize>,
    is_relocatable: impl Fn(usize) -> bool,
) -> bool {
    band_has_unpublished_word_with_map(
        rbp,
        frame_size,
        layout,
        live_hi,
        published,
        None,
        is_relocatable,
        // An arbitrary caller-supplied predicate: its words are not known to be
        // dereferenceable, so no header screen.
        false,
        // No compiled method in scope, so no liveness question can be asked —
        // and per the note in the word test, that keeps every report.
        None,
    )
}

/// [`band_has_unpublished_word_with`] plus the active map's live slots. See
/// [`band_slot_is_verifiable_with_map`].
#[allow(clippy::too_many_arguments)]
/// The object screen for the band test: is the word an actual object HEADER, or
/// merely a number that lands in the heap's address range?
///
/// `is_relocatable` answers the second question and the band test used to stop
/// there, so any stack word whose bit pattern fell inside the arena counted as
/// an unpublished oop. Every sibling instrument in this file screens with
/// `is_object_address` first; audit §16-§18 measured what skipping it costs
/// (hundreds to thousands of flagged words, `verifier_oop=0` on all of them).
///
/// Safe to read the header only because §18 bounded `MOVABLE_BOUNDS` to the
/// COMMITTED prefix — under the previous reservation-wide envelope this
/// dereference could touch a page that was never mapped.
///
/// `CRATONVM_MOVING_YOUNG_NO_BAND_OBJECT_SCREEN=1` restores the range-only
/// test. It is the fail-OPEN direction (more words flagged, more cycles
/// refusing to move), so it is the safe lever to reach for if a missed root is
/// ever suspected here.
#[inline]
fn band_word_is_an_object(w: usize) -> bool {
    if band_object_screen_disabled() {
        return true;
    }
    if w == 0 || w & 0x7 != 0 {
        return false;
    }
    // SAFETY: the caller has already established `w` is inside the published
    // movable range, which §18 bounds by the committed prefix, so the header
    // words are mapped. Alignment is checked just above.
    unsafe { cratonvm_types::plausible_object_header_at(w as *const u8) }
}

/// `CRATONVM_MOVING_YOUNG_NO_BAND_LIVENESS_SCREEN=1` — stop consulting the
/// class file's type maps about a band word, so every word the object screen
/// admits is reported.
///
/// The FAIL-OPEN direction, like the object screen's own switch: more words
/// flagged means more cycles refusing to move, so this is the lever to reach
/// for if a missed root is ever suspected here. It is also what makes the
/// screen's worth measurable in ONE binary, which is the whole reason these
/// switches exist in this file.
fn band_liveness_screen_disabled() -> bool {
    static OFF: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *OFF.get_or_init(|| {
        cratonvm_types::flags::runtime_var_os("CRATONVM_MOVING_YOUNG_NO_BAND_LIVENESS_SCREEN")
            .is_some()
    })
}

/// Kill switch for §25.4's thread-pointer fallback, so its effect is one
/// binary's A/B rather than two builds.
///
/// Off restores the pre-fix behaviour: the shadow window is resolved from the
/// innermost frame's cached thread pointer alone, and a no-publish leaf there
/// costs the whole chain its coverage proof.
fn band_thread_window_disabled() -> bool {
    static OFF: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *OFF.get_or_init(|| {
        cratonvm_types::flags::runtime_var_os("CRATONVM_MOVING_YOUNG_NO_BAND_THREAD_WINDOW")
            .is_some()
    })
}

fn band_object_screen_disabled() -> bool {
    static OFF: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *OFF.get_or_init(|| {
        cratonvm_types::flags::runtime_var_os("CRATONVM_MOVING_YOUNG_NO_BAND_OBJECT_SCREEN")
            .is_some()
    })
}

fn band_has_unpublished_word_with_map(
    rbp: usize,
    frame_size: usize,
    layout: &cratonvm_jit::FrameLayout,
    live_hi: Option<i32>,
    published: &std::collections::HashSet<usize>,
    map_slots: Option<&std::collections::HashSet<i32>>,
    is_relocatable: impl Fn(usize) -> bool,
    // Whether a word `is_relocatable` accepts may be DEREFERENCED.
    //
    // Only the caller knows. In production `is_relocatable` is
    // `gen_heap::addr_is_movable`, whose range §18 bounds by the COMMITTED
    // prefix, so the header words are mapped. But this function takes the
    // predicate as a parameter, and callers that pass an arbitrary closure —
    // every unit test here does — accept words that are not addresses at all.
    // Reading a header from one of those is a segfault, which is exactly what
    // the §19 object screen did until this flag existed: it inherited a safety
    // argument that held for one of its two callers.
    readable: bool,
    // The liveness oracle for a JAVA-LOCAL slot, when the caller has the
    // compiled method and safepoint id needed to consult it. `None` at a call
    // site means "cannot ask", which is not the same as "asked and got no".
    liveness: Option<&dyn Fn(i32) -> VerifierSlotVerdict>,
) -> bool {
    if frame_size == 0 || frame_size > rbp {
        return false;
    }
    let lo = rbp - frame_size;
    let mut addr = (lo + 7) & !7usize;
    // Same guard as `scan_one_frame`: a stale bound must never walk into
    // unmapped pages. A compiled frame is orders of magnitude smaller than
    // this, so the clamp is unreachable in practice.
    const MAX_SCAN_BYTES: usize = 1024 * 1024;
    let hi = rbp.min(addr.saturating_add(MAX_SCAN_BYTES));
    while addr + 8 <= hi {
        // Cast: a compiled frame is far smaller than i32::MAX bytes.
        let off = (rbp - addr) as i32;
        if !band_slot_is_verifiable_with_map(off, layout, live_hi, map_slots) {
            addr += 8;
            continue;
        }
        // SAFETY: aligned read inside the calling thread's own live compiled
        // frame, bounded by the frame size recorded at compile time.
        let w = unsafe { (addr as *const usize).read() };
        if is_relocatable(w) && !published.contains(&w) && (!readable || band_word_is_an_object(w))
        {
            // The LIVENESS screen. §19 established the survivors of the object
            // screen are header-shaped, so shape cannot separate a real missed
            // root from a dead slot still pointing at a live object. The class
            // file's own type maps can, for the java-locals band.
            //
            // ONE DIRECTION ONLY, and it is the whole safety argument here:
            // reporting an unpublished oop makes the cycle refuse to move, so
            // DISCARDING a report is the direction that permits movement. A
            // word is therefore discarded only on a positive `NotOop` — the
            // verifier saying this local definitely holds no reference at this
            // bci. `Unknown` (an inlined frame, a slot outside the locals band,
            // no type maps for the method) keeps the report, because "could not
            // ask" must never read as "answered no". That is the same
            // asymmetry §16 relies on, pointed the other way, because here the
            // consequence of being wrong is a missed root rather than a missed
            // refutation.
            let dead_by_verifier = !band_liveness_screen_disabled()
                && liveness
                    .map(|ask| ask(off) == VerifierSlotVerdict::NotOop)
                    .unwrap_or(false);
            if !dead_by_verifier {
                return true;
            }
        }
        addr += 8;
    }
    false
}

/// Refresh the current thread's moving-young coverage status for the active
/// JIT frames it owns. Returns `true` when every live frame reachable from this
/// thread has a complete active-safepoint proof; on `false`, the GC-side
/// per-cycle flag is marked so the collector diverts to the non-moving sweep.
pub fn refresh_moving_young_coverage_for_current_thread() -> bool {
    if !moving_young_enabled() {
        return true;
    }

    // Process-wide JIT relocation boundary (added 2026-07-29, `86e69e848`).
    //
    // When armed, the mere EXISTENCE of compiled code — not a specific unproven
    // frame — forces every young collection onto the non-moving sweep. It was
    // added after a Hibernate corruption chase in which the verifier below
    // observed unregistered entries, unavailable exact frame bases, and live
    // oops outside the published map, on the argument that discovering a gap
    // mid-scan is too late to repair that cycle.
    //
    // That argument does not describe how this code is wired. This function is
    // the collection's PRE-cycle proof: `roots.rs::collect_roots` calls it (via
    // `refresh_moving_young_coverage_for_collection`) *before* the collector
    // picks a young path, and `gen_heap::collect_garbage_inner` diverts on the
    // published verdict. A gap found here has always been found in time. What
    // the blanket actually bought was insurance against the verifier itself
    // missing an obligation — at the price of making moving-young unreachable
    // in every process that ever compiles a method, i.e. every real workload:
    // measured on `BinTreesClassic 18` at `-Xmx512m`, 66 of 66 young cycles
    // fell back, all of them attributed to this branch and none to a real
    // obligation.
    //
    // So it is a policy knob rather than a hard-coded law, and the per-cycle
    // proof is the default authority. `CRATONVM_MOVING_YOUNG_NO_JIT=1` restores
    // the blanket for a bisect or an emergency — the same fail-closed
    // direction, no longer the only available setting.
    if moving_young_no_jit() && cratonvm_jit::jit_code_range_count() != 0 {
        cratonvm_gc::gc_quiescence::mark_moving_young_coverage_incomplete_because(
            cratonvm_gc::gc_quiescence::incomplete_reason::JIT_RELOCATION_UNSUPPORTED,
        );
        return false;
    }

    let dbg = cratonvm_types::flags::runtime_var_os("CRATONVM_MOVING_YOUNG_COVERAGE_DBG").is_some();
    let scanner_sp = current_stack_pointer();
    if !dbg_no_prune() {
        let _ = prune_returned_jit_entries(scanner_sp);
    }

    let mut complete = true;

    JIT_ENTRY_CHAIN.with(|c| {
        {
            let mut chain = c.borrow_mut();
            if let Some(top) = chain.last_mut() {
                if let Some(info) = top.precise.as_mut() {
                    info.exact_rbp = top_rbp_get();
                info.exact_cm_id = published_compile_id();
                }
            }
        }

        let chain = c.borrow();
        if dbg {
            eprintln!(
                "[moving-young-coverage] chain_len={} top_rbp=0x{:x} scanner_sp=0x{:x}",
                chain.len(),
                top_rbp_get(),
                scanner_sp,
            );
        }
        for (entry_idx, entry) in chain.iter().enumerate() {
            let Some(info) = entry.precise else {
                if dbg {
                    eprintln!("[moving-young-coverage] incomplete: JIT entry has no precise map");
                }
                cratonvm_gc::gc_quiescence::mark_moving_young_coverage_incomplete_because(
                    cratonvm_gc::gc_quiescence::incomplete_reason::NO_PRECISE_MAP,
                );
                complete = false;
                continue;
            };
            // SAFETY: the compiled method pointer is the same Arc-stable pointer
            // used by the existing precise scan/remap paths.
            let cm: &cratonvm_jit::CompiledMethod =
                unsafe { &*(info.compiled_method as *const cratonvm_jit::CompiledMethod) };
            let exact_rbp = info.exact_rbp;
            if exact_rbp == 0 {
                if dbg {
                    eprintln!(
                        "[moving-young-coverage] incomplete: missing exact rbp entry_idx={} scanner_sp=0x{:x} entry_sp=0x{:x} maps={} sp_id_off={}",
                        entry_idx,
                        scanner_sp,
                        entry.entry_sp,
                        cm.oop_maps.len(),
                        cm.sp_id_slot_off,
                    );
                }
                cratonvm_gc::gc_quiescence::mark_moving_young_coverage_incomplete_because(
                    cratonvm_gc::gc_quiescence::incomplete_reason::MISSING_EXACT_RBP,
                );
                complete = false;
                continue;
            }
            let innermost =
                innermost_frame_method(exact_rbp, info.exact_cm_id, entry.entry_sp, scanner_sp, info.compiled_method);
            // The frame standing at the recorded RBP may belong to a method the
            // entry does not name (a JIT->JIT call published its own base
            // there). When the call was the direct form, the callee resolves
            // exactly and its own map is the one to check; when it was indirect
            // there is nothing to check it against, and relocating would strand
            // that frame's oops — take the non-moving sweep for this cycle.
            // SAFETY: a resolved method is kept alive by the live frame that
            // resolved it, the same contract as the chain entry's own pointer.
            let innermost_cm: Option<&cratonvm_jit::CompiledMethod> =
                innermost.map(|p| unsafe { &*p });
            if innermost_cm.is_none() {
                if dbg {
                    eprintln!(
                        "[moving-young-coverage] incomplete: innermost rbp=0x{exact_rbp:x} belongs to a JIT callee reached indirectly"
                    );
                }
                cratonvm_gc::gc_quiescence::mark_moving_young_coverage_incomplete_because(
                    cratonvm_gc::gc_quiescence::incomplete_reason::FOREIGN_INNERMOST_RBP,
                );
                complete = false;
            } else if !moving_young_frame_coverage_complete(
                exact_rbp,
                innermost_cm.unwrap_or(cm),
            ) {
                if dbg {
                    eprintln!(
                        "[moving-young-coverage] incomplete: active frame map at rbp=0x{:x}",
                        exact_rbp
                    );
                }
                cratonvm_gc::gc_quiescence::mark_moving_young_coverage_incomplete_because(
                    cratonvm_gc::gc_quiescence::incomplete_reason::ACTIVE_FRAME_MAP,
                );
                complete = false;
            }

            let mut child_rbp = exact_rbp;
            let mut guard = 0usize;
            while guard < 4096 {
                guard += 1;
                if child_rbp == 0 || child_rbp & 0x7 != 0 {
                    break;
                }
                if child_rbp < scanner_sp || child_rbp >= entry.entry_sp {
                    break;
                }
                // SAFETY: `child_rbp` is an aligned address inside this live
                // thread's JIT stack band, same invariant as
                // `remap_active_jit_frames`.
                let parent_rbp = unsafe { (child_rbp as *const usize).read() };
                let ret_addr = unsafe { ((child_rbp + 8) as *const usize).read() };
                // A link read out of the stack is NOT yet a frame base. Both
                // helpers below dereference `[parent_rbp - sp_id_slot_off]`
                // (and `remap_one_jit_frame` WRITES every slot its oop map
                // names), so the link must be validated BEFORE it is used, not
                // after: a zero/garbage word faulted at `0 - sp_id_slot_off`
                // while walking a json-smart chain. Same order and same
                // predicate as the other rbp-chain walks in this file
                // (`scan_active_oop_map_at_rbp`'s walk and the two conservative
                // band walks).
                if parent_rbp <= child_rbp
                    || parent_rbp & 0x7 != 0
                    || parent_rbp < scanner_sp
                    || parent_rbp >= entry.entry_sp
                {
                    break;
                }
                let Some(cm_ptr) = cratonvm_jit::lookup_jit_code_range(ret_addr) else {
                    break;
                };
                let cm: &cratonvm_jit::CompiledMethod =
                    unsafe { &*(cm_ptr as *const cratonvm_jit::CompiledMethod) };
                if !moving_young_frame_coverage_complete_at(parent_rbp, cm, Some(ret_addr)) {
                    if dbg {
                        eprintln!(
                            "[moving-young-coverage] incomplete: parent frame map at rbp=0x{:x}",
                            parent_rbp
                        );
                    }
                    cratonvm_gc::gc_quiescence::mark_moving_young_coverage_incomplete_because(
                        cratonvm_gc::gc_quiescence::incomplete_reason::PARENT_FRAME_MAP,
                    );
                    complete = false;
                }
                child_rbp = parent_rbp;
            }
        }
    });

    // A5 — unregistered JIT frame on the native stack.
    //
    // arch-2026-07-26 (`moving-young-precise-roots`): this probe used to be
    // SKIPPED entirely under moving-young, on the argument that "in precise
    // moving-young mode every actual compiled transition is registered by
    // JitEntryGuard and checked above". That argument is an assertion, not a
    // proof, and it was load-bearing for heap safety in exactly the direction
    // where being wrong is fatal:
    //
    //   * A JIT frame that is live WITHOUT a `JitEntryGuard` is not in the
    //     chain, so the loop above never examines it. It publishes no shadow
    //     homes and no per-safepoint oop map, so nothing can rewrite its
    //     register/spill slots.
    //   * With the probe skipped, `complete` stayed `true`, `roots.rs` took the
    //     precise-only branch (suppressing the conservative backstop that would
    //     at least have MARKED the frame's oops), and `gen_heap` ran the moving
    //     cycle — relocating objects out from under raw slots nobody rewrites.
    //     That is the canonical `main`-compiled bintrees corruption, re-armed.
    //   * The condition is known to be reachable: the whole A5 fix exists
    //     because it was observed (`Vm::invoke` → compiled app `main`, live
    //     while a clinit / interpreted callee triggers a GC).
    //
    // The probe therefore runs under moving-young as well, and a hit is treated
    // as an incomplete coverage proof (divert to the non-moving sweep) AND as
    // the A5 conservative-root condition, exactly as on the legacy path.
    //
    // The original skip was motivated by UTILITY, not soundness: the probe is a
    // raw-word scan, not a frame walk, so cached function pointers and JIT
    // helper arguments that happen to point inside generated code read as
    // return PCs and fabricate a frame. Over-detection costs moving cycles; it
    // never costs correctness. Reducing that false-positive rate (so
    // moving-young engages more often) is a real follow-up and is specified in
    // the design doc — it belongs in `native_stack_has_jit_frame` /
    // return-address validation, or in registering the entry-point transition,
    // NOT in suppressing the check.
    //
    // SB-LOADER-ZIPCONTENT (2026-08-04): that follow-up is now done, in the
    // place the paragraph above names — `is_plausible_return_pc` requires the
    // bytes before a candidate word to decode as the tail of a `call`. The
    // check itself is unchanged and still runs under moving-young.
    #[cfg(any(target_os = "windows", target_os = "linux"))]
    if cratonvm_jit::jit_code_range_count() > 0 {
        let cover_hi = JIT_ENTRY_CHAIN
            .with(|c| c.borrow().iter().map(|e| e.entry_sp).max())
            .unwrap_or(scanner_sp);
        let search_lo = scanner_sp.max(cover_hi);
        let high = current_thread_stack_high();
        // OBSERVATION ONLY — ask the memo what it WOULD have said, without
        // acting on it. Routing this call site through the memo measured inert
        // (reverted); this says whether that is because the memo never engages
        // or because engaging saves nothing. A flat profile cannot tell those
        // apart, and four attempts were judged on a flat profile.
        if a5_engagement_enabled() {
            A5_PROBE_CALLS.fetch_add(1, Ordering::Relaxed);
            A5_PROBE_WORDS.fetch_add(high.saturating_sub(search_lo) / 8, Ordering::Relaxed);
            let code_ranges = cratonvm_jit::jit_code_range_count();
            let hiwater_on = unreg_memo_hiwater_enabled();
            let would = UNREG_JIT_MEMO.with(|c| {
                let mut m = c.get();
                let d = m.observe(search_lo, code_ranges, hiwater_on);
                // Do NOT store: this is an observer, and `observe` mutates the
                // hiwater mark. Put the memo back exactly as it was.
                let _ = m;
                d
            });
            match would {
                UnregScan::AlreadyClean => A5_PROBE_MEMO_CLEAN.fetch_add(1, Ordering::Relaxed),
                UnregScan::Detect { hi: Some(_) } => {
                    A5_PROBE_MEMO_BANDED.fetch_add(1, Ordering::Relaxed)
                }
                UnregScan::Detect { hi: None } => {
                    A5_PROBE_FULL_RESCAN.fetch_add(1, Ordering::Relaxed)
                }
            };
        }
        let hit = if high > search_lo {
            native_stack_has_jit_frame(search_lo, high)
        } else {
            None
        };
        // Price the frame-shape filter without branching on it: for every cycle
        // this probe diverts, say how many band words looked like JIT return
        // addresses and how many of those sat at a slot with real frame shape.
        // `shaped=0` on a hit means the filter would have converted THIS cycle.
        if hit.is_some() && a5_census_enabled() {
            let (total, shaped) = native_stack_jit_frame_census(search_lo, high);
            eprintln!(
                "[a5-census] hits={total} shaped={shaped} band=[0x{search_lo:x},0x{high:x}) \
                 band_bytes={}",
                high.saturating_sub(search_lo),
            );
        }
        // CLASSIFY THE ONE HIT, then decide. The residue test is a
        // thread-local read of a mark the probe already maintains, so it runs
        // on every hit; the shape test reaches the global code-range table and
        // runs only when something asks. Counting happens whether or not either
        // screen is armed, which is what lets a run say how much of its OWN
        // fallback population each screen would convert — the number the two
        // screens have never had.
        let hit = match hit {
            None => None,
            Some((slot, word)) => {
                let n = A5_FALLBACK_HITS.fetch_add(1, Ordering::Relaxed) + 1;
                let residue_hi = jit_residue_hi();
                let is_residue = residue_hi != 0 && slot < residue_hi;
                if is_residue {
                    A5_FALLBACK_RESIDUE.fetch_add(1, Ordering::Relaxed);
                }
                let want_shape = a5_shape_filter_enabled() || a5_fallback_dbg();
                let shapeless = want_shape && !a5_slot_has_frame_shape(slot, high);
                if shapeless {
                    A5_FALLBACK_SHAPELESS.fetch_add(1, Ordering::Relaxed);
                }
                let filtered = (is_residue && a5_residue_filter_enabled())
                    || (shapeless && a5_shape_filter_enabled());
                if a5_fallback_dbg() && (n <= 16 || n.is_power_of_two()) {
                    eprintln!(
                        "[a5-fallback] #{n} slot=0x{slot:x} word=0x{word:x} \
                         residue_hi=0x{residue_hi:x} is_residue={is_residue} \
                         shapeless={shapeless} filtered={filtered} \
                         band=[0x{search_lo:x},0x{high:x}) chain_len={}",
                        JIT_ENTRY_CHAIN.with(|c| c.borrow().len()),
                    );
                }
                if filtered {
                    A5_FALLBACK_FILTERED.fetch_add(1, Ordering::Relaxed);
                    None
                } else {
                    Some((slot, word))
                }
            }
        };
        if let Some((slot, word)) = hit {
            if dbg {
                // Name the actual evidence: which slot, which word, how far
                // into which compiled body, and the bytes the return-address
                // filter accepted. "Some word somewhere looked like compiled
                // code" is not something the next reader can act on, and this
                // probe's whole cost is that it can be wrong.
                let (body, off, tail) = match cratonvm_jit::pin_jit_code_range_owner(word) {
                    Some(cm) => {
                        let entry = cm.entry_ptr() as usize;
                        let back = word.saturating_sub(entry).min(8);
                        let mut bytes = String::new();
                        for i in 0..back {
                            // SAFETY: inside the buffer `cm` holds alive.
                            let b = unsafe { ((word - back + i) as *const u8).read() };
                            bytes.push_str(&format!("{b:02x} "));
                        }
                        (entry, word - entry, bytes)
                    }
                    None => (0, 0, "<body reclaimed>".to_string()),
                };
                eprintln!(
                    "[moving-young-coverage] incomplete: unregistered JIT frame on native stack \
                     (stack slot 0x{slot:x} holds 0x{word:x} = body 0x{body:x}+0x{off:x}, \
                     preceding bytes [{tail}], search band [0x{search_lo:x}, 0x{high:x}), \
                     slot is 0x{depth:x} above search_lo)",
                    depth = slot.saturating_sub(search_lo),
                );
            }
            cratonvm_gc::gc_quiescence::set_unregistered_jit_frame_on_stack();
            cratonvm_gc::gc_quiescence::mark_moving_young_coverage_incomplete_because(
                cratonvm_gc::gc_quiescence::incomplete_reason::UNREGISTERED_JIT_FRAME,
            );
            complete = false;
        }
    }

    // COVERAGE VERIFICATION, not coverage assertion (arch-2026-07-26
    // `moving-young-corruption-rootcause`). Everything above trusts
    // `OopMapEntry::moving_young_coverage_complete`, a codegen bit computed
    // from the abstract interpreter's local/operand model. That model does not
    // describe scalar-replacement field slots, LICM hoist slots, or the
    // full-GPR safepoint spill area — all of which can hold live oops that the
    // shadow push never publishes and that `roots.rs` no longer scans
    // conservatively once the proof "passes". Check the frames' actual bytes
    // instead of taking the bit's word for it.
    //
    // Deliberately placed HERE rather than at the `roots.rs` call site: this
    // function is also what `vm_exec::deposit_root_snapshot` and
    // `interpreter::update_root_snapshot` call before suppressing their own
    // conservative scans, so a parked or safepointed thread gets the same
    // verification without those (differently-owned) files changing.
    {
        let mut reason = cratonvm_gc::gc_quiescence::incomplete_reason::NONE;
        if moving_young_unpublished_frame_oop_present(&mut reason) {
            if dbg {
                eprintln!(
                    "[moving-young-coverage] incomplete: {}",
                    cratonvm_gc::gc_quiescence::incomplete_reason::label(reason),
                );
            }
            cratonvm_gc::gc_quiescence::mark_moving_young_coverage_incomplete_because(reason);
            complete = false;
        }
    }

    if !complete {
        cratonvm_gc::gc_quiescence::mark_moving_young_coverage_incomplete();
    }
    complete
}

/// Whether some OTHER thread holds live JIT frames that this thread's scan
/// cannot account for: `GLOBAL_JIT_DEPTH` counts every registered JIT entry
/// process-wide, `current_thread_jit_depth()` counts this thread's, and a
/// positive difference means at least one peer is inside compiled code.
///
/// Test-only. It was written as the trigger for the cross-thread proof
/// obligation and then never called: as the block below
/// `refresh_moving_young_coverage_for_collection` says, those obligations are
/// discharged by machinery that does not consult a global-vs-local depth
/// comparison -- a parked peer publishes into its `root_snapshot`, and an
/// OS-frozen peer is scanned conservatively and marks the cycle incomplete.
/// The predicate is kept because the invariant it states is worth pinning; the
/// `pub` wrapper around it was surface with nothing behind it.
#[cfg(test)]
#[inline]
const fn peer_jit_frames_present(global_depth: usize, local_depth: usize) -> bool {
    global_depth > local_depth
}

/// Collection-authoritative moving-young coverage refresh.
///
/// [`refresh_moving_young_coverage_for_current_thread`] proves (or disproves)
/// the obligation for the frames THIS thread owns. It is called both by each
/// mutator's root-snapshot deposit — where per-thread scope is exactly right —
/// and by the collection's own root gather, where it is **not sufficient**:
/// a collection must account for every live JIT frame in the process, not just
/// the initiator's.
///
/// The cross-thread obligations are discharged elsewhere for the two cases that
/// have machinery: a cooperatively-parked peer publishes its shadow values into
/// its `root_snapshot` and remaps its own shadow stack on resume, and an
/// OS-frozen / helper-window peer is scanned conservatively and marks the cycle
/// incomplete.
///
/// Until 2026-08-23 neither mechanism gave the *initiator* a positive proof at
/// the moment it decides whether to relocate, so the rule was: **any** peer in
/// JIT makes the cycle unproven. That is the
/// `bug-h2-testkillprocess-zgc-oom-at-97-percent-free-20260821-FIXED-20260829.md` residual —
/// on a many-threaded workload it fires on nearly every cycle, and on ZGC,
/// where relocation is the only defragmentation there is, the consequence is an
/// `OutOfMemoryError` on a heap that is 97 % free.
///
/// [`publish_peer_jit_coverage_for_stw`] is the handshake that replaces it. A
/// peer proves its OWN frames rewritable, on its own thread, at its own park,
/// and deposits the proven depth; this function accepts the cycle only when the
/// deposits account for **every** peer JIT entry in the process. The
/// accounting is a depth comparison rather than a thread count because
/// `GLOBAL_JIT_DEPTH` is the only process-wide view available and it is a
/// depth: `peer = global - mine`, and `proven >= peer` is the acceptance test.
/// Everything the handshake cannot see — an OS-frozen peer, a peer blocked in a
/// native with compiled frames below it, a peer whose own proof failed —
/// deposits nothing, so the shortfall refuses the cycle exactly as before.
///
/// `CRATONVM_XT_JIT_COVERAGE_HANDSHAKE=0` restores the blanket refusal on the
/// same binary, which is what makes this an A/B rather than a rebuild.
/// Over-diverting costs compaction; under-diverting costs the heap.
pub fn refresh_moving_young_coverage_for_collection(pins_honoured: bool) -> bool {
    if !moving_young_enabled() {
        return true;
    }
    let mut complete = refresh_moving_young_coverage_for_current_thread();
    let peer_depth = peer_jit_depth();
    if peer_depth == 0 {
        // The unguarded door. Everything below is the peer ledger; a zero skips
        // all of it, so a zero has to be recorded where it happens or it is
        // indistinguishable from an accounted cycle in every census.
        cratonvm_gc::gc_quiescence::note_peer_depth_zero(
            global_jit_depth_raw(),
            current_thread_jit_depth(),
        );
    }
    if peer_depth > 0 {
        let proven = cratonvm_gc::gc_quiescence::peer_proven_jit_depth();
        // Depth belonging to peers this cycle discharged by PINNING instead of
        // by proof (see `add_xt_cycle_pinned_jit_depth`). Two conditions, and
        // both are load-bearing:
        //
        //  - the credit is enabled, so the old behaviour is one flag away; and
        //  - EVERY helper window this cycle was pinned. A single unpinned
        //    window means some frozen peer's stack is neither proven nor
        //    immobile, and since the account is one process-wide subtraction it
        //    cannot exclude just that peer -- so it credits nothing at all.
        //
        // `xt_cycle_pinned_jit_depth` applies the third condition itself: it
        // returns 0 if any pinned peer's depth was unknown.
        //  - AND THE COLLECTOR ABOUT TO RUN HONOURS THE PIN. This one was
        //    missing, and it is the difference between a discharge and a lie.
        //
        //    The credit says "this peer's frames need not be rewritable,
        //    because the objects they name will not move". Only a collector
        //    that can WITHHOLD an object makes that true. G1 withholds the
        //    region and ZGC the page, and both consume
        //    `gc_quiescence::pinned_jit_roots_snapshot()` to do it. The
        //    generational young collector is Cheney copying: from-space is
        //    reclaimed wholesale, so every live object in it moves by
        //    construction, and `gen_heap.rs` and `gen_evac.rs` accordingly
        //    contain no reader of that snapshot. The pinned addresses reach
        //    that backend only as ROOTS -- which keeps them ALIVE, and says
        //    nothing whatever about keeping them PUT.
        //
        //    So on the generational collector this credit discharged a peer's
        //    coverage obligation against a pin nobody applied, the cycle
        //    relocated, and the peer resumed with a stale reference in a
        //    compiled frame. Reproducible in ten seconds on the H2 JDBC corpus
        //    with `CRATONVM_GEN_UNCOMMIT=1` as the detector -- 3/3 SIGSEGV, and
        //    0/3 with either half of this credit switched off
        //    (`CRATONVM_XT_PINNED_PEER_DEPTH=0`,
        //    `CRATONVM_XT_PINNED_PEER_PUBLISH_ONLY=1`). The accounting line
        //    immediately before the fault was
        //    `peer_depth=2 proven=0 pinned=2 accounted=true`; the run that did
        //    not crash had `pinned=0 accounted=false`.
        //
        //    `CRATONVM_XT_PINNED_PEER_UNPINNABLE=1` restores the old behaviour
        //    on the same binary, which is what makes this an A/B rather than a
        //    rebuild -- and, on a collector that cannot pin, a way to reproduce
        //    the defect deliberately.
        let pinned = if pinned_credit_admissible(
            xt_pinned_peer_depth_enabled(),
            xt_pinned_peer_publish_only(),
            pins_honoured,
            xt_pinned_peer_credit_when_unpinnable(),
            crate::jit::xt_root_scan::helper_windows_all_pinned_this_cycle(),
        ) {
            cratonvm_gc::gc_quiescence::xt_cycle_pinned_jit_depth()
        } else {
            0
        };
        let covered = proven.saturating_add(pinned);
        let accounted = xt_jit_coverage_handshake_enabled()
            && (peer_coverage_accounted(peer_depth, covered) || xt_jit_coverage_assume());
        cratonvm_gc::gc_quiescence::note_peer_coverage_verdict(accounted);
        if xt_coverage_dbg() {
            eprintln!(
                "[xt-coverage] peer_depth={peer_depth} proven={proven} pinned={pinned} \
pins_honoured={pins_honoured} accounted={accounted}"
            );
        }
        if !accounted {
            cratonvm_gc::gc_quiescence::mark_moving_young_coverage_incomplete_because(
                cratonvm_gc::gc_quiescence::incomplete_reason::CROSS_THREAD_JIT_PEER,
            );
            complete = false;
        }
    }
    complete
}

/// May this cycle discharge a peer's coverage obligation by PINNING?
///
/// Split out for the same reason [`peer_coverage_accounted`] is: every input is
/// a process-global read at the call site, so the RULE is untestable there and
/// this is the part that has to be right.
///
/// `pins_honoured` is the condition that was missing until 2026-09-06, and it
/// is the one that makes the other four mean anything — see the call site for
/// the reproduction. `credit_when_unpinnable` is the deliberate override that
/// re-opens the defect, so the fix has a positive control.
#[inline]
const fn pinned_credit_admissible(
    enabled: bool,
    publish_only: bool,
    pins_honoured: bool,
    credit_when_unpinnable: bool,
    all_windows_pinned: bool,
) -> bool {
    enabled && !publish_only && (pins_honoured || credit_when_unpinnable) && all_windows_pinned
}

/// The handshake's acceptance test, split out so it is testable without racing
/// the process-global counters.
///
/// `>=` and not `==`: the ledger is a sum of per-thread deposits taken at each
/// peer's park, and a peer that returned from a JIT frame between its deposit
/// and this read lowers `peer_depth` without lowering `proven`. That direction
/// is safe — the frames it proved are a superset of the frames still live. The
/// unsafe direction is `proven` running ahead of what was actually proven this
/// pause, which is why the ledger is cleared by `request_stw` and by
/// `begin_moving_young_coverage_cycle` rather than after use.
#[inline]
const fn peer_coverage_accounted(peer_depth: usize, proven_depth: usize) -> bool {
    proven_depth >= peer_depth
}

/// JIT entries held by threads OTHER than this one, right now.
///
/// `saturating_sub` and not a plain subtraction: `GLOBAL_JIT_DEPTH` is a
/// striped counter and this thread's own chain is a thread-local, so a torn
/// read across the stripes can momentarily make the global look smaller than
/// the local. Zero is the safe reading of that — it means "no peer depth to
/// account for", and the caller's other obligations still stand.
#[inline]
fn peer_jit_depth() -> usize {
    GLOBAL_JIT_DEPTH
        .get()
        .saturating_sub(current_thread_jit_depth())
}

/// The striped process-wide JIT depth, unreduced.
///
/// `peer_jit_depth` folds this into a difference and clamps at zero, which
/// destroys the evidence for whether the zero was a quiet process or an
/// inconsistent read. The classifier needs the raw value.
#[inline]
fn global_jit_depth_raw() -> usize {
    GLOBAL_JIT_DEPTH.get()
}

/// May the cross-thread coverage handshake discharge `CROSS_THREAD_JIT_PEER`?
///
/// Default ON, so a KILL SWITCH: `CRATONVM_XT_JIT_COVERAGE_HANDSHAKE=0` (or
/// `off`/`false`/`no`) restores the blanket "any peer in JIT means unproven"
/// refusal. Default-off would leave the many-threaded case exactly where the
/// H2 page found it, which is the defect and not a conservative posture.
///
/// Read per cycle rather than latched: it is consulted once per collection, and
/// a `OnceLock` would let the first test to touch it decide the answer for
/// every later test in the binary.
fn xt_jit_coverage_handshake_enabled() -> bool {
    match cratonvm_types::flags::runtime_var_os("CRATONVM_XT_JIT_COVERAGE_HANDSHAKE") {
        Some(raw) => {
            let v = raw.to_string_lossy().trim().to_ascii_lowercase();
            !matches!(v.as_str(), "0" | "off" | "false" | "no")
        }
        None => true,
    }
}

fn xt_coverage_dbg() -> bool {
    cratonvm_types::flags::runtime_var_os("CRATONVM_DBG_XT_COVERAGE").is_some()
}

/// `CRATONVM_XT_JIT_COVERAGE_ASSUME=1` -- accept the peer accounting whatever
/// the ledger says.
///
/// **A MEASUREMENT INSTRUMENT, and unsafe to run with.** It is how the question
/// "is the cross-thread handshake the only thing between this workload and
/// compaction?" is asked, in the same spirit as
/// `CRATONVM_MOVING_YOUNG_NO_BAND_VERIFY`. A peer that deposited nothing has
/// NOT proved its frames rewritable, and relocating under it strands its oops.
///
/// It exists because the shortfall is now specific enough to be worth pricing.
/// With the 2026-08-30 safepoint repairs every per-frame proof in a 300 s run
/// of `org.h2.test.jdbc.TestCachedQueryResults` succeeds
/// (`frame_cov=(no_slot=0 misaligned=0 no_map=0 incomplete=0 ok=601)`), and the
/// peers that still refuse the cycle are the ones that never reach
/// `publish_peer_jit_coverage_for_stw` at all -- threads blocked in a native
/// with compiled frames below them, which park through the blocked-region
/// protocol rather than the STW barrier. Giving them a deposit means teaching
/// the blocked-region WAKE to remap JIT frames (it currently remaps only
/// interpreter frames), which is a real change; this says whether it is worth
/// making.
/// `CRATONVM_XT_PINNED_PEER_UNPINNABLE=1` -- credit the pinned-peer depth even
/// on a collector that cannot honour the pin.
///
/// Default OFF, which is the corrected behaviour. It exists because the defect
/// this closes is invisible without it: with it set, the generational
/// collector's ten-second H2 reproduction comes back, so the fix has a positive
/// control rather than only an absence of crashes.
fn xt_pinned_peer_credit_when_unpinnable() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        cratonvm_types::flags::runtime_var_os("CRATONVM_XT_PINNED_PEER_UNPINNABLE").is_some()
    })
}

fn xt_jit_coverage_assume() -> bool {
    cratonvm_types::flags::runtime_var_os("CRATONVM_XT_JIT_COVERAGE_ASSUME").is_some()
}

/// A peer thread's half of the cross-thread JIT coverage handshake.
///
/// Called by a mutator that is about to park at the STW barrier, immediately
/// before `arrive_and_wait`. It runs THIS thread's own per-thread coverage
/// proof — the only place in the process where that proof can be run for this
/// thread, because `JIT_ENTRY_CHAIN`, the cached top RBP and the shadow window
/// are all thread-local — and, if it holds, deposits this thread's JIT depth
/// into the pause's ledger.
///
/// Why a deposit here is a promise the thread can keep: a cooperatively-parked
/// peer resumes through `apply_pointer_map_to_thread`, which runs
/// `remap_active_jit_frames`, `remap_register_image_words` and
/// `shadow_stack.remap` over exactly the chain this proof just walked. The
/// proof says every live oop of those frames is reachable from a channel the
/// remap rewrites; the resume applies the rewrite. Nothing else in the process
/// has to touch the peer's registers.
///
/// Cheap when it cannot matter: a thread with no JIT entries contributes
/// nothing to the initiator's `peer_depth` arithmetic, so it returns before
/// paying for `refresh_moving_young_coverage_for_current_thread` (whose
/// `native_stack_has_jit_frame` band walk is the expensive part). That is the
/// overwhelmingly common shape at a safepoint park.
pub fn publish_peer_jit_coverage_for_stw() {
    if !moving_young_enabled() || !xt_jit_coverage_handshake_enabled() {
        return;
    }
    // Pruning inside the proof can only SHRINK the chain, so an already-empty
    // chain stays empty and this early-out cannot skip a nonzero deposit.
    if current_thread_jit_depth() == 0 {
        return;
    }
    // WHICH obligation the peer's own proof fails on, when it fails.
    //
    // `proven=false` is the shortfall that refuses the whole cycle, and a
    // count of them cannot be acted on: the proof has SIX ways to say no
    // (`JIT_RELOCATION_UNSUPPORTED`, `NO_PRECISE_MAP`, `MISSING_EXACT_RBP`,
    // `FOREIGN_INNERMOST_RBP`, `ACTIVE_FRAME_MAP`, `PARENT_FRAME_MAP`) and they
    // want completely different repairs. The per-reason counters are already
    // maintained process-wide, so a before/after snapshot around this one call
    // names the term without any new bookkeeping. Only taken when the debug
    // flag is on — it is two array reads either side of a proof that already
    // walks the stack.
    let before = xt_coverage_dbg().then(cratonvm_gc::gc_quiescence::moving_young_incomplete_reason_mask);
    let proven = refresh_moving_young_coverage_for_current_thread();
    // Read the depth AFTER the proof: it prunes returned entries, and the
    // deposit must not claim more than the proof covered.
    let depth = current_thread_jit_depth();
    if proven && depth > 0 {
        cratonvm_gc::gc_quiescence::add_peer_proven_jit_depth(depth);
    }
    if let Some(before) = before {
        let added = cratonvm_gc::gc_quiescence::moving_young_incomplete_reason_mask() & !before;
        let mut why = String::new();
        for i in 0..cratonvm_gc::gc_quiescence::incomplete_reason::COUNT {
            if added & (1usize << i) != 0 {
                if !why.is_empty() {
                    why.push(',');
                }
                why.push_str(cratonvm_gc::gc_quiescence::incomplete_reason::label(i));
            }
        }
        if why.is_empty() {
            // Distinguishable from a reason literally labelled "none": this
            // says the proof added no obligation to the mask at all, which for
            // a `proven=false` deposit means the reason was ALREADY recorded
            // this cycle (by this thread's earlier proof, or by a peer).
            why.push_str("<no-new-reason>");
        }
        eprintln!("[xt-coverage] peer deposit proven={proven} depth={depth} why={why}");
    }
}

/// Returns true if any thread anywhere in the process is currently inside a
/// JIT call. Used by the GC to decide whether compaction is safe.
#[inline]
pub fn any_thread_in_jit() -> bool {
    !GLOBAL_JIT_DEPTH.is_zero()
}

/// Returns the number of active JIT entries on the *current* thread.
/// Mostly useful for tests and assertions.
#[inline]
pub fn current_thread_jit_depth() -> usize {
    JIT_ENTRY_CHAIN.with(|c| c.borrow().len())
}

/// The active compiled frames of the CURRENT thread, outermost first, as
/// `(interpreter depth at entry, "class/Name.method:descriptor", owner class)`.
///
/// This is what makes a JIT-compiled method visible to `Thread.getStackTrace()`
/// and to every throwable's trace: compiled code pushes no interpreter frame,
/// so without it the Java-visible stack silently loses every method the JIT has
/// taken over. See [`JitFrameChainEntry::interp_depth`] for the H2 case that
/// found it.
///
/// Entries with no label are skipped rather than reported as an unnamed frame:
/// the only artifacts with an empty `method_label` are the legacy/test compile
/// wrapper's, and inventing a frame for one would be worse than omitting it.
/// Kill switch for the nested-activation walk in [`active_compiled_frames`].
///
/// Default ON. `CRATONVM_JIT_NO_NESTED_TRACE_FRAMES=1` restores the historical
/// one-frame-per-chain-entry answer, so the frame-count difference is an A/B
/// inside ONE binary instead of a comparison across two builds.
fn nested_trace_frames_enabled() -> bool {
    static G: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *G.get_or_init(|| {
        cratonvm_types::flags::runtime_var_os("CRATONVM_JIT_NO_NESTED_TRACE_FRAMES").is_none()
    })
}

/// Whether [`active_compiled_frames`] dumps the chain it walked to stderr.
///
/// `CRATONVM_DBG_SWCHAIN=1`. Off by default and cached, because this runs on
/// every VM-raised throw. What it prints per chain entry — the recorded
/// `entry_sp` / `exact_rbp` / published compile id, the boundary method, how
/// the innermost frame resolved, and the walked activation list — is the dump
/// that identified the defect this walk had: a frame the mirror named
/// correctly but no decoder could resolve, because only one of the two
/// encodings of a direct call was ever decoded.
fn dbg_swchain_enabled() -> bool {
    static G: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *G.get_or_init(|| cratonvm_types::flags::runtime_var_os("CRATONVM_DBG_SWCHAIN").is_some())
}

/// Kill switch for the bytecode index carried on [`ActiveCompiledFrame`].
///
/// Default ON. `CRATONVM_JIT_NO_COMPILED_FRAME_LINES=1` makes every compiled
/// frame report `bci: -1`, which is exactly the `byte_code_index: -1` /
/// `(Unknown Source)` answer every compiled frame gave before 2026-09-01. The
/// point of the switch is that a line that looks wrong in a warmed-up trace
/// can be attributed — one environment variable decides whether it came from
/// this recovery or from the method's own `LineNumberTable`, inside ONE
/// binary. It reverts the OSR line with it: `stackwalker`'s display-only
/// override is the bci of a compiled half that no longer has one.
fn compiled_frame_bci_enabled() -> bool {
    static G: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *G.get_or_init(|| {
        cratonvm_types::flags::runtime_var_os("CRATONVM_JIT_NO_COMPILED_FRAME_LINES").is_none()
    })
}

/// Kill switch for the inlined-callee chain carried on
/// [`ActiveCompiledFrame`].
///
/// Default ON, and the SAME variable the emitter half reads
/// (`x64::inlining`'s `inline_frame_map_enabled`), because a half-switched
/// feature is worse than either state: with the map emitted and the walk
/// ignoring it the artifact pays for metadata nobody reads, and with the walk
/// reading a map that was never emitted every frame silently loses its
/// callees. One variable, both halves.
/// Kill switch for the OPTIMIZING tier's line numbers.
///
/// Default ON. `CRATONVM_JIT_NO_IR_FRAME_LINES=1` restores the blanket
/// `used_ir_backend` refusal, i.e. `(Unknown Source)` for every frame the IR
/// backend compiled, which is what every such frame reported before
/// `CompiledMethod::safepoint_bci_table` existed.
///
/// It is a separate name from `CRATONVM_JIT_NO_COMPILED_FRAME_LINES` because
/// the two isolate different things. That one reverts the whole recovery, on
/// both backends at once, so it cannot say whether a suspect line came from the
/// id-to-bci translation this tier needs or from the slot read both tiers
/// share. The translation is the new evidence and is the half worth being able
/// to switch alone; a wrong line under this switch OFF and a right one under it
/// ON is a one-binary attribution.
fn ir_frame_bci_enabled() -> bool {
    static G: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *G.get_or_init(|| {
        cratonvm_types::flags::runtime_var_os("CRATONVM_JIT_NO_IR_FRAME_LINES").is_none()
    })
}

fn inline_frame_chains_enabled() -> bool {
    static G: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *G.get_or_init(|| {
        cratonvm_types::flags::runtime_var_os("CRATONVM_JIT_NO_INLINE_FRAME_MAP").is_none()
    })
}

/// Is `recorded` a value the JVM spec permits as a bytecode index?
///
/// `Code.code_length` must be less than 65536 (JVMS 4.9.1), so every genuine
/// bci is below that. Both synthetic pcs the single-pass backend stamps into
/// this same field are far above it — `ENTRY_POLL_BC_PC` (`u32::MAX`, the
/// method-entry poll) and `SP_ID_UNSET_BC_PC` (`u32::MAX - 1`, what the
/// prologue writes into the safepoint-id slot so an unsafepointed frame cannot
/// match the bci-0 map). One spec-derived bound rejects both, and it rejects
/// them for a reason a reader can check rather than by listing two constants
/// this crate cannot see (they are `pub(super)` inside the jit crate's x64
/// module).
fn plausible_bci(recorded: u32) -> Option<u32> {
    const MAX_CODE_LENGTH: u32 = 65_536;
    (recorded < MAX_CODE_LENGTH).then_some(recorded)
}

/// One live compiled activation, as a stack capture needs it.
///
/// Replaces the `(depth, label, class_id, cm_ptr)` tuple this function used to
/// return. The tuple was the reason a compiled frame had no line number: it
/// carried no bytecode index, and `stackwalker::compiled_frame_entry` — the
/// only consumer that wants one — could therefore do nothing but hard-code
/// `LINE_NUMBER_UNKNOWN`. See
/// `jit-compiled-frame-has-no-line-and-no-inlined-callees-FIXED-20260902.md`.
#[derive(Debug, Clone)]
pub struct ActiveCompiledFrame {
    /// Interpreter depth this activation's chain entry was pushed at; the
    /// splice point in `stackwalker::interleave_compiled_frames`.
    pub interp_depth: u32,
    /// `"class/Name.method:descriptor"`, from the artifact's `method_label`.
    pub label: String,
    /// `ObjectHeader` class id of the artifact's owner.
    pub owner_class_id: u32,
    /// The `CompiledMethod` this activation is running, as a raw address.
    /// Valid for exactly as long as the frame is live.
    pub cm_ptr: usize,
    /// Bytecode index this activation is stopped at, or `-1` when the frame
    /// published no usable safepoint id. NEVER a guess: see
    /// [`activation_bci`].
    pub bci: i32,
    /// The callees the JIT INLINED into this artifact at the program point
    /// this activation is standing at, INNERMOST FIRST, as
    /// `("class/Name.method:descriptor", bci)` pairs — the label in the same
    /// shape [`ActiveCompiledFrame::label`] uses, so one splitter parses both,
    /// and the bci in that callee's own code.
    ///
    /// It is HotSpot's `ScopeDesc` chain, and it is why a compiled frame is
    /// not one frame: an inlined callee pushes nothing and, before this,
    /// contributed nothing to a trace. Empty for every non-inlining method and
    /// for every refusal on the producer side, and an empty chain reproduces
    /// the historical one-entry-per-artifact answer exactly. See
    /// [`compiled_frame_inline_chain`].
    pub inline_chain: Vec<InlinedLevel>,
    /// Was [`Self::inline_chain`] obtained from the EXACT key (this
    /// activation's own return address), rather than the coarse safepoint-id
    /// one?
    ///
    /// Display does not care -- both keys are fail-closed for it. A CALLER
    /// ATTRIBUTION walk does: one `cur_bc_pc` covers a whole spliced region and
    /// is shared with the inline cache's MISS EDGE, where the spliced body did
    /// NOT run. A wrong frame in a trace is a wrong frame; a wrong frame in the
    /// JEP 403 gate is a fail-OPEN caller. So
    /// `stackwalker::frame_class_ids_with_compiled` expands only an exact
    /// chain, and this is the bit that says which it has.
    ///
    /// `true` on an EMPTY chain is meaningless and never read: there is nothing
    /// to expand.
    pub chain_exact: bool,
}

/// One level of a compiled frame's inline chain, as a stack walk consumes it.
///
/// Replaces the `(label, bci)` pair the chain used to carry. The third field is
/// the whole reason: `class_id` is recorded by the RESOLVER, at the moment it
/// looked the spliced body up, so a walk that must answer in `ClassId` --
/// `frame_class_ids_with_compiled`, feeding the JEP 403 deep-reflection gate
/// and `Class.forName`'s caller loader -- can see an inlined method without
/// resolving a JIT label BY NAME. That resolution is what made the blind spot
/// deliberate: a by-name answer in a security gate is a guess, and this is not
/// one.
#[derive(Debug, Clone)]
pub struct InlinedLevel {
    /// `"class/Name.method:descriptor"`, the shape
    /// [`ActiveCompiledFrame::label`] uses.
    pub label: String,
    /// Bytecode index in `label`'s own method.
    pub bci: u32,
    /// `ClassId` of the class `label` names, or `0` for "the producer supplied
    /// none". A consumer must REFUSE on `0`, never substitute.
    pub class_id: u32,
}

/// The bytecode index a live compiled activation is stopped at.
///
/// Reads the safepoint id the emitter publishes into `[rbp - sp_id_slot_off]`
/// before every GC-capable call — the same slot `moving_young_frame_live_hi`
/// and the precise root walkers already key off — and then REQUIRES that the
/// artifact actually recorded a safepoint with that id.
///
/// That second half is what makes this safe to put on a stack trace. The slot
/// is written before a call, so between calls it holds the id of the last
/// safepoint rather than the current position; and an artifact that reserves no
/// slot leaves whatever the frame's uninitialised memory held. Requiring the id
/// to name one of THIS artifact's own recorded safepoints rejects both, at the
/// cost of answering `None` for a frame stopped somewhere no safepoint covers.
/// A frame with no line is what this page's own reasoning asked for over a
/// frame with a wrong one, and it is what the caller falls back to.
///
/// # Two more things it refuses
///
/// * **An optimizing IR-backend id with no translation** (`used_ir_backend`).
///   There `OopMapEntry::bytecode_pc` is NOT a bci: `ir_lower` stores a
///   monotonic safepoint counter starting at 1 (its own doc says so, and the
///   single-pass backend's `SP_ID_UNSET_BC_PC` comment explains why THAT
///   backend cannot do the same). Those ids are small integers
///   indistinguishable from plausible bcis, AND the artifact's own table
///   records them, so the confirmation above passes and a real,
///   confidently-wrong line would come out if the id were used as one.
///
///   Until 2026-09-02 the whole backend was refused on that ground, and that
///   was the largest remaining population of line-less compiled frames. The
///   refusal is now exactly as wide as the hazard: `ir_lower` records the
///   `(id, bci)` pair for every safepoint it emits
///   (`CompiledMethod::safepoint_bci_table`), this reads the bci THROUGH it,
///   and an id the table does not name is still refused. What that leaves is
///   an artifact compiled before the table existed, or a safepoint whose bci
///   exceeded `u32` -- both `None`, neither a guess.
///   `CRATONVM_JIT_NO_IR_FRAME_LINES=1` restores the blanket refusal.
/// * **Anything outside the spec's bci range** — see [`plausible_bci`]. The
///   two synthetic pcs the single-pass backend stamps are already rejected by
///   the `i32::try_from` below (both are within one of `u32::MAX`), but that
///   is an accident of their VALUES rather than a rule, and the rule is what
///   the next synthetic pc will be measured against.
///
/// An artifact with no safepoint-id slot (`sp_id_slot_off == 0`) -- compiled
/// without the precise gate, or aarch64, whose backend hard-codes
/// `bytecode_pc: 0` and would otherwise resolve every compiled frame to the
/// first line of its method -- is already refused by `active_safepoint_id`.
///
/// # aarch64: the refusal is UNREACHABLE, not merely conservative (2026-09-02)
///
/// The 2026-09-01 page listed "aarch64 compiled frames still report no line"
/// as an open residual. It is not one, and the reason is worth writing down
/// because it is cheaper than re-deriving it: **an aarch64 compiled activation
/// cannot be on the stack while a Java-level stack capture runs.**
///
/// A trace is captured either at a throw or from a caller-sensitive native,
/// and both are reached by an `invoke*`. `Arm64Backend::emit_invoke` sets
/// `self.failed` UNCONDITIONALLY -- there is no call-target resolution in that
/// backend at all -- so a method containing any call bails and is interpreted
/// (`jit::aarch64_backend::tests::invokestatic_arm_exists_but_always_bails`).
/// `athrow` and every object-model opcode bail on the same rule
/// (`object_model_opcodes_are_all_unsupported`), so such a body cannot raise
/// either, and `label_for_pc` refuses every backward branch target, so it
/// cannot loop. What compiles there is leaf, straight-line, exception-free
/// arithmetic that runs to its `ret`.
///
/// `Arm64CompileResult::pending_oop_maps` is empty for the same generation of
/// reasons (`compiled_methods_carry_no_oop_maps`) — as of 2026-09-03 the map
/// WRITER is correct and its publication path works, but there is still no
/// safepoint of any kind to call it at, and so none to name a bci at.
/// A line number is downstream of a safepoint mechanism that backend does not
/// have; giving it one is a consequence of building that mechanism, not a
/// separate task. Three tests pin the premises, so a change that makes an
/// aarch64 frame reachable from a capture trips them first.
fn activation_bci(rbp: usize, cm: &cratonvm_jit::CompiledMethod) -> Option<i32> {
    use cratonvm_jit::{
        note_compiled_frame_line as note, FRAME_LINE_ANSWERED_IR,
        FRAME_LINE_ANSWERED_SINGLE_PASS, FRAME_LINE_REFUSED_ID_UNRECORDED,
        FRAME_LINE_REFUSED_IR_UNTRANSLATED, FRAME_LINE_REFUSED_NO_SP_ID,
        FRAME_LINE_REFUSED_OUT_OF_RANGE, FRAME_LINE_REFUSED_SWITCHED_OFF,
    };
    // Every exit below names a census column. That is not decoration: a trace
    // prints `(Unknown Source)` for all five refusals and there is no other
    // way to tell which population a line-less frame came from. See
    // `cratonvm_jit::COMPILED_FRAME_LINE_COUNTS`.
    let Some(id) = active_safepoint_id(rbp, cm) else {
        note(FRAME_LINE_REFUSED_NO_SP_ID);
        return None;
    };
    // `find_oop_map_for_safepoint_id` is `&self` and is the artifact's own
    // record of which bytecode PCs it emitted a safepoint at.
    if cm.find_oop_map_for_safepoint_id(id).is_none() {
        note(FRAME_LINE_REFUSED_ID_UNRECORDED);
        return None;
    }
    // The optimizing tier's slot holds an ID, not a bci. Translate it through
    // the artifact's own table, and refuse when there is none: an untranslated
    // id is a small integer that looks exactly like a bci, which is the wrong
    // answer this refusal was written to avoid.
    let recorded = if cm.used_ir_backend {
        if !ir_frame_bci_enabled() {
            note(FRAME_LINE_REFUSED_SWITCHED_OFF);
            return None;
        }
        match cm.safepoint_bci(id) {
            Some(bci) => bci,
            None => {
                note(FRAME_LINE_REFUSED_IR_UNTRANSLATED);
                return None;
            }
        }
    } else {
        id
    };
    let Some(bci) = plausible_bci(recorded).and_then(|v| i32::try_from(v).ok()) else {
        note(FRAME_LINE_REFUSED_OUT_OF_RANGE);
        return None;
    };
    note(if cm.used_ir_backend {
        FRAME_LINE_ANSWERED_IR
    } else {
        FRAME_LINE_ANSWERED_SINGLE_PASS
    });
    Some(bci)
}

/// The callees inlined into `cm` at the program point a live frame is standing
/// at, INNERMOST FIRST, as `(label, bci)` pairs — where `label` is
/// `"class/Name.method:descriptor"`, the shape
/// `CompiledMethod::method_label` already uses, and `bci` is that method's own
/// bytecode index.
///
/// The chain EXTENDS [`activation_bci`]'s answer and never replaces it: the
/// enclosing compiled method keeps the bci that function returns, and these
/// are the frames HotSpot would show *below* it, from `ScopeDesc`'s nested
/// scopes. An empty chain is the historical answer — one frame per artifact —
/// and is what every non-inlining method produces.
///
/// # The two keys, and why neither one falls back to the other
///
/// The emitter half lives in `jit/src/x64/inlining.rs`
/// (`begin_inline_frame_recording` / `finish_inline_frame_recording`) and
/// records, at every call it emits from inside a spliced body, a row holding
/// BOTH keys this walk can offer: `native_offset`, the buffer position
/// immediately after the `CALL` — which is exactly what a child frame's
/// RBP-chain walk reads out of `[rbp+8]` — and `safepoint_bci`, the
/// `cur_bc_pc` the emitter simultaneously stores into the frame's
/// safepoint-id slot.
///
/// Which key applies is decided by WHICH FRAME is asking, not by which lookup
/// happens to hit:
///
///   * a frame BELOW the innermost one owns a return address on this stack, so
///     `native_pc` is `Some` and the exact offset names one program point and
///     nothing else. That is the authoritative key and it is the only one
///     consulted;
///   * the INNERMOST frame owns no return address here (its caller's is the
///     one on the stack), so the safepoint-id slot — i.e. [`activation_bci`]'s
///     answer — is its only evidence.
///
/// **There is deliberately no fallback from the exact key to the coarse one.**
/// A bci is a property of the enclosing method and both keys answer with the
/// same `cur_bc_pc`. A CHAIN is not. One `cur_bc_pc` covers a whole spliced
/// region, and the calls emitted on the inline cache's MISS EDGE — the ones
/// that were not spliced — sit under that same bci while recording no row of
/// their own. A parent frame suspended on a miss edge would then be handed the
/// chain belonging to the splice beside it: a frame naming a method that never
/// ran, which is the one outcome this whole area refuses. A miss on the exact
/// key means "this program point recorded no chain", and that is the answer.
///
/// # The optimizing tier needs no extra refusal here
///
/// Key 2 is the safepoint-id slot read back as a bci, and since 2026-09-02
/// [`activation_bci`] can answer that for an `used_ir_backend` artifact too
/// (through `CompiledMethod::safepoint_bci_table`). Key 1 is a CODE LAYOUT
/// fact -- the byte offset of a return address in this artifact's own buffer
/// -- and carries no assumption about which backend emitted it. Neither needs
/// a backend refusal, and since 2026-09-08 neither gets one: an IR artifact
/// that spliced a body now arrives here with a POPULATED `inline_frame_map`
/// and its inlined callees are reported like any other.
///
/// Until then they were not. `record_inline_frame_row` is called only from the
/// single-pass splicer, so an IR artifact's map was empty and this function
/// returned on the `is_empty()` guard before either key was consulted -- and
/// IR-tier inlining contributed no frames at all. That was invisible while the
/// optimizing tier claimed few methods and became a live trace defect when it
/// claimed more: measured 2026-09-08 on `probes/StackTraceAfterOsr.java`,
/// `after_main_osr` printed `len=3 [leaf mid* outer* probe main]` against the
/// interpreter's `len=5`, the two starred frames being bodies the IR splicer
/// had inlined into `probe`.
///
/// The producer is `ir_lower`'s `note_inline_frame_return_site`, and it is
/// keyed off `ir::IrInlineFrameSites` -- the COMBINED-BUFFER pc -- not off
/// `InlineScopeTable` as this note used to predict. The reason is key 2's own
/// ambiguity: a spliced region is covered by the caller's snapshot at the
/// `invoke` pc, so every level of a NESTED splice reports one bci and
/// `from_rows` (correctly) poisons rows that disagree under it. Combined-pc
/// ranges are disjoint by construction, so they name one body and one nesting
/// exactly -- which is what key 1, the exact return address, then carries.
///
/// # The fail-closed rule
///
/// A missing or ambiguous chain must produce NO frame rather than a guessed
/// one. That is stricter than the rule for a line number, and for a stronger
/// reason: a wrong line is visibly a line, while an inlined frame naming the
/// wrong method is indistinguishable from a real one to the person reading the
/// trace. The emitter already refuses a row whose bci is not a spec-legal
/// bytecode index, and POISONS a safepoint id two program points disagree
/// about (`CRATONVM_JIT_NO_INLINE_MISS_EDGE_POISON=1` reverts that half alone)
/// — one bci covers a whole spliced region, so a splice containing two calls
/// with different chains cannot be told apart from the safepoint-id slot
/// alone, which is the innermost frame's only evidence. Both refusals arrive
/// here as an empty chain, which is exactly the pre-2026-09-01 behaviour.
fn compiled_frame_inline_chain(
    cm: &cratonvm_jit::CompiledMethod,
    bci: i32,
    native_pc: Option<usize>,
) -> (Vec<InlinedLevel>, bool) {
    fn levels(chain: &[cratonvm_jit::x64::InlineFrameLevel]) -> Vec<InlinedLevel> {
        chain
            .iter()
            .map(|l| InlinedLevel {
                label: l.label.clone(),
                bci: l.bci,
                class_id: l.class_id,
            })
            .collect()
    }
    if cm.inline_frame_map.is_empty() {
        // The overwhelming majority: a method that splices nothing carries an
        // empty map and no allocation, and this is the whole cost it adds to a
        // throw.
        return (Vec::new(), false);
    }
    // Key 1 — the exact return address, for every frame below the innermost.
    if let Some(pc) = native_pc {
        let entry = cm.entry_ptr() as usize;
        let end = entry.saturating_add(cm.code_len());
        // A return address is strictly INSIDE the body (there is at least a
        // call instruction before it) and at most one past its last byte.
        if entry != 0 && pc > entry && pc <= end {
            // Cast: bounded by `code_len()`, which is a JIT buffer position.
            let off = (pc - entry) as u32;
            if let Some(chain) = cm.inline_frame_map.chain_for_native_offset(off) {
                return (levels(chain), true);
            }
        }
        // No fallback to key 2 here -- see the doc above. An exact key that
        // misses is evidence that this program point recorded no chain, not
        // permission to consult a coarser one.
        return (Vec::new(), false);
    }
    // Key 2 — the safepoint id, the innermost frame's only evidence. `bci` is
    // what `activation_bci` recovered out of the frame's safepoint-id slot,
    // which is the same `cur_bc_pc` the emitter recorded the row under.
    if bci < 0 {
        return (Vec::new(), false);
    }
    match cm.inline_frame_map.chain_for_safepoint_bci(bci as u32) {
        // `false`: this is the COARSE key. Good enough to display, never good
        // enough to attribute a caller -- see `ActiveCompiledFrame::chain_exact`.
        Some(chain) => (levels(chain), false),
        // `None` is both "no row" and "two rows disagreed and the emitter
        // poisoned this bci". The caller cannot act differently on the two and
        // must not: an ambiguous chain and an absent one both mean no inlined
        // frame may be reported here.
        None => (Vec::new(), false),
    }
}

/// Give the INNERMOST frame of an NPE snapshot the line and the inlined
/// callees of the inline null check that raised.
///
/// An inline null check is not a GC-capable call, so it publishes no safepoint
/// id and [`activation_bci`] correctly refuses the stale one in the slot: the
/// frame that RAISED was the one frame in the snapshot with no line. The
/// emitter records what it knows at every such site
/// (`x64::inlining::record_npe_trap_site`) and the site's cold trampoline hands
/// its key to `jit_npe_with_action`; this is where the two meet.
///
/// `trap_key == 0` means the site was not described -- the feature switched
/// off, a bci out of spec range, a chain the emitter refused, or a helper that
/// signals an NPE without going through an inline check at all -- and leaves
/// the snapshot exactly as the walk built it.
///
/// Three things make a mismatched key harmless rather than wrong:
///
///   * keys are per-compile MONOTONIC ids, never indices, so one that belongs
///     to another artifact (or to a rewound splice) MISSES rather than landing
///     on a neighbouring site;
///   * the lookup is against the innermost frame's OWN artifact, which is the
///     one whose trampoline ran;
///   * both halves respect the switches the rest of this file does, so
///     `CRATONVM_JIT_NO_COMPILED_FRAME_LINES=1` still produces `-1` here and
///     `CRATONVM_JIT_NO_INLINE_FRAME_MAP=1` still produces no callee frames.
///
/// `frames` is OUTERMOST-first (see [`active_compiled_frames`]), so the frame
/// that trapped is the last one.
pub fn apply_npe_trap_site(frames: &mut [ActiveCompiledFrame], trap_key: u32) {
    if trap_key == 0 {
        return;
    }
    let want_bci = compiled_frame_bci_enabled();
    let want_chains = inline_frame_chains_enabled();
    if !want_bci && !want_chains {
        return;
    }
    let Some(top) = frames.last_mut() else {
        return;
    };
    if top.cm_ptr == 0 {
        return;
    }
    // SAFETY: the same window and the same contract as `active_compiled_frames`
    // -- this runs inside the JIT helper that trapped, on the owning thread,
    // with the artifact still registered in the JIT cache and its chain entry
    // still pushed, and `cm_ptr` came from that walk moments ago.
    let cm = unsafe { &*(top.cm_ptr as *const cratonvm_jit::CompiledMethod) };
    let Some(site) = cm.npe_trap_map.get(trap_key) else {
        return;
    };
    if want_bci {
        if let Ok(bci) = i32::try_from(site.bci) {
            top.bci = bci;
            cratonvm_jit::note_compiled_frame_line(cratonvm_jit::FRAME_LINE_ANSWERED_NPE_TRAP);
        }
    }
    if want_chains {
        top.inline_chain = site
            .chain
            .iter()
            .map(|l| InlinedLevel {
                label: l.label.clone(),
                bci: l.bci,
                class_id: l.class_id,
            })
            .collect();
        // The trap site is an EXACT program point -- the emitter recorded it at
        // the null check it is describing, not under a bci a whole spliced
        // region shares. So this chain is as authoritative as a return-address
        // one.
        top.chain_exact = true;
    }
}

pub fn active_compiled_frames() -> Vec<ActiveCompiledFrame> {
    let nested_enabled = nested_trace_frames_enabled();
    let want_bci = compiled_frame_bci_enabled();
    let want_chains = inline_frame_chains_enabled();
    let dbg_chain = dbg_swchain_enabled();
    let scanner_sp = current_stack_pointer();
    JIT_ENTRY_CHAIN.with(|c| {
        // The top entry's `exact_rbp` lives in the `TOP_RBP` mirror between
        // push/pop boundaries; the walk below needs the LIVE innermost RBP, so
        // flush it exactly as `remap_active_jit_frames` does. `try_borrow_mut`
        // rather than `borrow_mut`: a stack capture is reachable from paths
        // that may already hold the chain borrow, and a stale (higher)
        // `exact_rbp` only shortens the walk — it degrades this function to the
        // answer it gave before, and never walks past a bound.
        if nested_enabled {
            if let Ok(mut v) = c.try_borrow_mut() {
                flush_top_rbp_cache_to_chain(v.as_mut_slice());
            }
        }
        let chain = c.borrow();
        if dbg_chain {
            eprintln!(
                "[swchain] scanner_sp=0x{scanner_sp:x} entries={} nested={nested_enabled}",
                chain.len()
            );
        }
        let mut out: Vec<ActiveCompiledFrame> = Vec::with_capacity(chain.len());
        for (dbg_i, e) in chain.iter().enumerate() {
            let Some(info) = e.precise else {
                if dbg_chain {
                    eprintln!("[swchain] e{dbg_i} entry_sp=0x{:x} precise=none", e.entry_sp);
                }
                continue;
            };
            let entry_sp = e.entry_sp;
            if dbg_chain {
                // SAFETY: the same keep-alive contract as the reporting loop at
                // the end of this function.
                let b = unsafe { &*info.compiled_method }.method_label.clone();
                eprintln!(
                    "[swchain] e{dbg_i} entry_sp=0x{entry_sp:x} exact_rbp=0x{:x} cm_id={} depth={} boundary={b}",
                    info.exact_rbp, info.exact_cm_id, e.interp_depth
                );
            }
            // One chain entry is one interpreter->JIT boundary, but the
            // compiled region behind it can be many ACTIVATIONS deep: compiled
            // code calling itself never re-enters from the interpreter, so it
            // pushes no further chain entry. Reporting only the boundary method
            // made 64 nested activations read as ONE frame to
            // `Throwable.getStackTrace()` and `StackWalker` alike — Log4j2's
            // caller lookup then walked past the frame it wanted and answered
            // with the enclosing class
            // (`stackwalker_log4j_deep_repeated_walks_finish_under_jit`).
            // Walk the saved-RBP chain the way `remap_active_jit_frames`'
            // Stage 5 already does, and report every activation.
            //
            // Each activation is carried with the RBP of ITS OWN frame, not
            // just its method: that is where the safepoint id lives, and it is
            // the only thing standing between a compiled frame and a line
            // number. The walk already computes every one of these addresses
            // to find the next frame — it used to drop them on the floor.
            //
            // The THIRD member is that frame's own RETURN ADDRESS, when the
            // RBP-chain walk read one. It is `None` for the innermost
            // activation (whose return address is not on this stack) and for
            // the boundary fallback, and it is the EXACT key the inline-frame
            // map is recorded under — see `compiled_frame_inline_chain`.
            //
            // The RBP is an `Option` because `innermost_frame_method` ALSO
            // answers `Some(cm)` when the RBP is unusable (it falls back to
            // the boundary method) and the caller cannot tell the two answers
            // apart from the outside. Reading `[rbp - sp_id_slot_off]` for an
            // out-of-band RBP would be a wild read, not merely a wrong line,
            // so the bound is re-checked here; the walk's own `parent_rbp`
            // values were bounds-checked in the loop below and need no second
            // test.
            let exact_rbp = (info.exact_rbp != 0
                && info.exact_rbp & 0x7 == 0
                && info.exact_rbp >= scanner_sp
                && info.exact_rbp.saturating_add(16) <= entry_sp)
                .then_some(info.exact_rbp);
            let mut nested: Vec<(
                *const cratonvm_jit::CompiledMethod,
                Option<usize>,
                Option<usize>,
            )> = Vec::new();
            if nested_enabled {
                if let Some(innermost) = innermost_frame_method(
                    info.exact_rbp,
                    info.exact_cm_id,
                    entry_sp,
                    scanner_sp,
                    info.compiled_method,
                ) {
                    // The innermost frame has no return address of its own
                    // on this stack, so only the safepoint-id slot can place
                    // it.
                    nested.push((innermost, exact_rbp, None));
                }
                // JIT frames use `push rbp; mov rbp,rsp`, so `[rbp]` is the
                // caller RBP and `[rbp+8]` the return address INTO that caller.
                // Same bound checks, same order and the same 4096 guard as the
                // Stage 5 walk — this one only READS, where that one rewrites
                // oop slots.
                let mut child_rbp = info.exact_rbp;
                let mut guard = 0usize;
                while guard < 4096 {
                    guard += 1;
                    if child_rbp == 0 || child_rbp & 0x7 != 0 {
                        break;
                    }
                    if child_rbp < scanner_sp || child_rbp >= entry_sp {
                        break;
                    }
                    // SAFETY: `child_rbp` is an aligned address inside this
                    // thread's own live JIT stack region, bounded by
                    // `scanner_sp` (this frame) and `entry_sp` (the boundary
                    // that pushed the chain entry), and validated before use
                    // exactly as the other rbp-chain walks in this file do.
                    let parent_rbp = unsafe { (child_rbp as *const usize).read() };
                    let ret_addr = unsafe { ((child_rbp + 8) as *const usize).read() };
                    if parent_rbp <= child_rbp
                        || parent_rbp & 0x7 != 0
                        || parent_rbp < scanner_sp
                        || parent_rbp >= entry_sp
                    {
                        break;
                    }
                    match cratonvm_jit::lookup_jit_code_range(ret_addr) {
                        // `ret_addr` lies in the PARENT, so the frame this
                        // method is running in is the one at `parent_rbp`.
                        // `ret_addr` also lies in THIS method and is the
                        // instruction after the call it is suspended in — the
                        // exact key `InlineFrameMap` rows are recorded under.
                        Some(cm_ptr) => nested.push((
                            cm_ptr as *const cratonvm_jit::CompiledMethod,
                            Some(parent_rbp),
                            Some(ret_addr),
                        )),
                        // The parent is the interpreter / Rust boundary: this
                        // entry has no further compiled ancestors.
                        None => break,
                    }
                    child_rbp = parent_rbp;
                }
            }
            // Never report FEWER frames than the pre-walk answer. A walk cut
            // short by a bound, one that never started (`exact_rbp == 0`), and
            // the kill-switch path all still owe the boundary method the chain
            // entry was pushed for.
            if nested.last().map(|(p, _, _)| *p) != Some(info.compiled_method) {
                nested.push((info.compiled_method, exact_rbp, None));
            }
            if dbg_chain {
                let names: Vec<String> = nested
                    .iter()
                    // SAFETY: as the reporting loop below.
                    .map(|(p, _, _)| unsafe { &**p }.method_label.clone())
                    .collect();
                let mut runs: Vec<String> = Vec::new();
                for n in &names {
                    match runs.last_mut() {
                        Some(last) if last.starts_with(&format!("{n} x")) => {
                            let c: usize = last.rsplit(" x").next().unwrap_or("1").parse().unwrap_or(1);
                            *last = format!("{n} x{}", c + 1);
                        }
                        Some(last) if last == n => *last = format!("{n} x2"),
                        _ => runs.push(n.clone()),
                    }
                }
                eprintln!(
                    "[swchain] e{dbg_i} activations={} [{}]",
                    names.len(),
                    runs.join(" | ")
                );
            }
            // `nested` is innermost-first; the splice in
            // `runtime::stackwalker::interleave_compiled_frames` wants
            // outermost-first, and entries sharing an `interp_depth` keep their
            // push order.
            for (cm_ptr, frame_rbp, native_pc) in nested.iter().rev() {
                // SAFETY: exactly the contract documented on
                // `PreciseFrameInfo::compiled_method` — the JIT cache holds an
                // owning `Arc` for as long as the body is registered, and the
                // chain entry is popped the moment the call returns or unwinds,
                // so there is no stale-pointer window. This read happens on the
                // owning thread, from a Java-level stack capture, i.e. strictly
                // inside that window. Pointers added by the walk came from
                // `lookup_jit_code_range`, which only answers for a code range
                // still registered in the cache.
                let cm = unsafe { &**cm_ptr };
                if cm.method_label.is_empty() {
                    continue;
                }
                let bci = match frame_rbp {
                    Some(rbp) if want_bci => activation_bci(*rbp, cm).unwrap_or(-1),
                    _ => {
                        // Counted, so an all-`-1` trace under
                        // `CRATONVM_JIT_NO_COMPILED_FRAME_LINES=1` is
                        // distinguishable from one where the recovery ran and
                        // refused. A kill switch that looks identical to a
                        // defect is how a switch left set in an environment
                        // becomes a bug report.
                        cratonvm_jit::note_compiled_frame_line(
                            cratonvm_jit::FRAME_LINE_REFUSED_SWITCHED_OFF,
                        );
                        -1
                    }
                };
                let (inline_chain, chain_exact) = if want_chains {
                    compiled_frame_inline_chain(cm, bci, *native_pc)
                } else {
                    (Vec::new(), false)
                };
                out.push(ActiveCompiledFrame {
                    interp_depth: e.interp_depth,
                    label: cm.method_label.clone(),
                    owner_class_id: cm.owner_class_id,
                    // The artifact itself, so the trace assembler can ask it
                    // whether an interpreter frame's pc is one of ITS OSR entry
                    // points. Valid for exactly as long as the frame is live,
                    // which is the same window this whole function reads in.
                    cm_ptr: *cm_ptr as usize,
                    bci,
                    // The callees this artifact spliced at that same program
                    // point. `Vec::new()` does not allocate, so a method that
                    // inlines nothing -- and the whole feature switched off --
                    // costs one branch.
                    inline_chain,
                    chain_exact,
                });
            }
        }
        out
    })
}

// ---------------------------------------------------------------------------
// Cross-thread JIT-root gap detector (multi-thread-in-JIT-under-STW)
// ---------------------------------------------------------------------------

/// Diagnostic counter: number of times [`scan_active_jit_frames`] observed the
/// unsupported "multi-thread-in-JIT" condition (this thread's chain is empty
/// while another thread holds live JIT frames). Exposed for tests / JFR.
pub static CROSS_THREAD_JIT_GAP_HITS: AtomicUsize = AtomicUsize::new(0);

/// Cached `CRATONVM_STRICT_JIT_ROOTS` gate. When set, the cross-thread-JIT-gap
/// detector PANICS instead of merely warning, so a CI / fuzzing run can make
/// the (otherwise silent) unsupported condition a hard, visible failure. Off by
/// default so production keeps the safe published-snapshot path. Cached because
/// the detector runs on the per-native-call hot path.
fn strict_jit_roots() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| cratonvm_types::flags::runtime_var_os("CRATONVM_STRICT_JIT_ROOTS").is_some())
}

/// Detect — and loudly report — the unsupported *multi-thread-in-JIT* condition
/// for the thread-local JIT-frame scanner.
///
/// ## What it detects
///
/// `scan_active_jit_frames` only ever discovers the **calling** thread's JIT
/// roots (the chain is thread-local). When the calling thread's chain is empty
/// (`current_thread_jit_depth() == 0`) yet `any_thread_in_jit()` is true, this
/// scan contributes **nothing** for JIT roots while *some other* thread holds
/// live JIT spill slots. If this scan is part of a peer-triggered STW
/// collector's authoritative root walk, those peer roots are reachable ONLY via
/// that peer's last-published `root_snapshot` (see the `scan_active_jit_frames`
/// doc): if that snapshot is stale, a live root is silently dropped.
///
/// This is exactly the documented gap (a full cross-thread STW JIT root scan is
/// a separate, unimplemented feature). Rather than proceed silently, we surface
/// the condition:
///
/// - **Always:** a rate-limited `tracing::warn!` (first occurrence, then every
///   power-of-two thereafter) so the gap is visible in any run without flooding
///   the per-native-call hot path.
/// - **Opt-in (`CRATONVM_STRICT_JIT_ROOTS`):** a panic, turning the gap into a
///   hard failure for CI / fuzzing / bisection.
///
/// ## Why this is conservative (does NOT destabilize the single-thread path)
///
/// The detector NEVER changes the root set — it only observes and logs. The
/// common single-thread-in-JIT path (`current_thread_jit_depth() > 0`) never
/// trips it. A peer being in JIT while THIS thread is not is also benign in the
/// normal flow (the peer published a fresh snapshot at its safepoint); the warn
/// flags the *structural* window in which the unimplemented cross-thread scan
/// would be required, which is the actionable signal for the follow-up.
#[cold]
#[inline(never)]
fn warn_cross_thread_jit_gap() {
    // BUG-03 — when the cross-thread STW JIT root scan is enabled, the
    // collector forcibly stops every in-JIT peer and conservatively scans its
    // registers + stack at GC time (see `crate::jit::xt_root_scan`). The
    // condition this detector flags (a peer in JIT while this thread's chain
    // is empty) is therefore no longer a gap — peer JIT roots are covered
    // regardless of snapshot freshness. Treat it as a non-event: do not count
    // a hit and never panic under CRATONVM_STRICT_JIT_ROOTS (so the strict
    // gate verifies the fix reaches zero gap hits).
    if crate::jit::xt_root_scan::enabled() {
        return;
    }
    let hits = CROSS_THREAD_JIT_GAP_HITS.fetch_add(1, Ordering::Relaxed) + 1;
    // Rate-limit: log the 1st hit and every subsequent power of two so a
    // long-running multi-threaded JIT workload does not spam the log, but the
    // condition is never fully silent.
    if (hits & (hits - 1)) == 0 {
        tracing::warn!(
            cross_thread_jit_gap_hits = hits,
            global_jit_depth = GLOBAL_JIT_DEPTH.get(),
            "scan_active_jit_frames: another thread holds live JIT frames while \
             this thread's JIT chain is empty — the thread-local scanner cannot \
             see the peer's JIT roots. They are covered ONLY by that peer's \
             last-published root_snapshot; a stale snapshot would drop a live \
             root. This is the documented multi-thread-in-JIT-under-STW gap; the \
             cross-thread STW JIT root scan is a tracked follow-up. Set \
             CRATONVM_STRICT_JIT_ROOTS=1 to make this fatal."
        );
    }
    if strict_jit_roots() {
        panic!(
            "CRATONVM_STRICT_JIT_ROOTS: unsupported multi-thread-in-JIT condition \
             in scan_active_jit_frames (this thread's JIT chain is empty but \
             GLOBAL_JIT_DEPTH={} > 0). The thread-local conservative scanner \
             cannot enumerate a peer thread's JIT roots; a cross-thread STW JIT \
             root scan is required and is not yet implemented.",
            GLOBAL_JIT_DEPTH.get(),
        );
    }
}

// ---------------------------------------------------------------------------
// Scanner
// ---------------------------------------------------------------------------

/// Walk every active JIT spill region on the current thread and report each
/// qword whose value is a valid object address as a conservative root.
///
/// # ⚠ THREAD-LOCALITY — known limitation and safety envelope
///
/// This scanner is **strictly thread-local**: the `JIT_ENTRY_CHAIN` and the
/// captured stack pointers belong to the calling thread, so a single call
/// only ever discovers the *calling* thread's live JIT spill roots. It does
/// **not** and **cannot** discover the JIT roots of any *other* thread.
///
/// That distinction matters because two callers invoke it with different
/// expectations:
///
/// - **Single-thread-in-JIT (SAFE).** A self-triggered collection runs
///   `collect_roots` *on the same thread* whose JIT frames hold the roots
///   (`roots.rs`), and a thread snapshotting itself runs
///   `update_root_snapshot` *on its own thread* (`interpreter.rs`). In both
///   cases the calling thread == the thread owning the JIT chain, so the
///   thread-local scan is complete and correct. This is the common path.
///
/// - **Multi-thread-in-JIT under a peer STW (THE GAP).** A *cross-thread*
///   stop-the-world collector marks every parked thread from that thread's
///   `root_snapshot` (`collect_all_root_snapshots`); it does **not** call
///   `collect_roots` — nor this scanner — *for* a non-current thread (doing
///   so would scan the **collector's** empty JIT chain, not the parked
///   worker's). The only mechanism that carries a parked worker's JIT roots
///   to the collector is that worker having published a *fresh* snapshot via
///   its own `update_root_snapshot` (which folds this scan in) **before it
///   parked at the STW safepoint**. The STW barrier (`gc_barrier.rs`) does
///   force every running mutator — including a thread spinning in JIT — to a
///   safepoint before the collector proceeds, and the safepoint publishes a
///   fresh snapshot, so in the *normal* flow the worker's current JIT roots
///   are visible.
///
///   The residual gap: if a worker's *published* snapshot is ever stale
///   relative to its current JIT spill slots (its slots changed after its
///   last publish and it reached the STW park without re-publishing), the
///   peer collector is **blind to those roots**. A full, correct fix is a
///   *cross-thread STW JIT root scan* — a per-thread snapshot of
///   `(JIT_ENTRY_CHAIN, current_sp)` captured AT the safepoint and walked by
///   the collector. That is a hard, separate feature (precise oop maps /
///   shadow-stack work tracks it); it is **NOT implemented here**.
///
/// ## Why a missed root has not been observed to corrupt the heap
///
/// While *any* thread is in JIT, `gc_quiescence::is_active()` is set, which
/// forces the **non-moving** young sweep with selective promotion — objects
/// are never relocated out from under a stale stack qword. So the failure
/// mode of the gap is *reclamation* of a still-live object, not a wrong
/// relocation. The published-snapshot mitigation above closes that in the
/// normal flow; the strict guard below (see [`warn_cross_thread_jit_gap`])
/// makes the residual unsupported condition **fail loudly** instead of
/// silently dropping a root.
///
/// ## Follow-up
///
/// FOLLOW-UP (tracked): implement the real cross-thread STW JIT root scan so
/// the collector enumerates each parked worker's JIT chain directly, removing
/// the dependence on the worker's last-published snapshot being current. Until
/// then, single-thread-in-JIT is the supported, verified path.
///
/// # Filtering
///
/// 1. Only 8-byte aligned addresses are read (matches the JIT calling
///    convention's spill slot alignment).
/// 2. Each candidate value is passed to [`VmHeap::is_object_address`], which
///    confirms the address falls inside the heap arena AND lands on a valid
///    object header (correct alignment, valid kind, plausible class id).
/// 3. False positives only inflate the root set; they cannot cause incorrect
///    behavior because the GC is in non-compacting mode (see module docs).
///
/// # Safety
///
/// The scanner reads raw memory between two stack-pointer values. Both
/// pointers come from the same thread's call stack and the read range is
/// always non-empty / well-defined: if `current_sp >= entry_sp` (chain
/// inverted) or the range is empty, the entry is silently skipped. We never
/// dereference the read qword as a Rust reference; it is treated as an
/// opaque address until validated.
#[inline(always)]
/// `CRATONVM_DBG_NO_JIT_ROOT_SCAN=1` — skip the conservative JIT frame scan
/// UNCONDITIONALLY. **Diagnostic only, and unsound**: a JIT-held object whose
/// only reference is a register or spill slot stops being a root at all, so a
/// moving cycle will relocate it under the running frame. Never ship a run with
/// this set.
///
/// It exists because the two levers that LOOK like they answer "is the
/// conservative scan what retains this object?" do not:
///
/// * `CRATONVM_GC_PRECISE_ONLY_ROOTS` suppresses the scan only when the
///   coverage proof passes, and its own doc records that firing on **~0.1 % of
///   collections** (2 of 14 420, 31 of 46 135, 84 of 70 144). A run with it set
///   still scans conservatively on 999 collections in 1000, so a null result
///   from it is a vacuous zero, not evidence.
/// * `CRATONVM_NO_CONSERVATIVE_LOCALS` gates the INTERPRETER's local scan,
///   which is a different path.
///
/// The gate lives HERE and not at a call site because there are THREE doors
/// into this function — `memory::roots::collect_roots` (gc-roots),
/// `vm_exec`'s safepoint deposit, and `interpreter::gc_and_alloc`'s
/// blocked-deposit. Gating only the first leaves the other two publishing
/// conservative roots, which is a lever that reads as "no effect" while never
/// having been applied.
fn dbg_no_jit_root_scan() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        cratonvm_types::flags::runtime_var_os("CRATONVM_DBG_NO_JIT_ROOT_SCAN").is_some()
    })
}

/// How many times [`dbg_no_jit_root_scan`] actually suppressed a scan.
/// Reported at exit so the arm cannot be read as "no effect" when it was in
/// fact "never engaged" — the failure mode this whole flag exists to avoid.
pub static NO_JIT_ROOT_SCAN_SUPPRESSED: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

pub fn scan_active_jit_frames(heap: &VmHeap, out: &mut Vec<ObjectRef>) {
    if dbg_no_jit_root_scan() {
        let n = NO_JIT_ROOT_SCAN_SUPPRESSED
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            + 1;
        if n == 1 {
            eprintln!(
                "[jitrootscan] CRATONVM_DBG_NO_JIT_ROOT_SCAN engaged                  -- conservative JIT frame roots are NOT being published (UNSOUND)"
            );
        }
        return;
    }
    // Capture the scanner's own SP at the call site (inlined into the
    // caller). Every active JIT spill region has its *lowest* address at
    // or above this value (the Rust stack grows downward on every supported
    // target). We use `current_stack_pointer` rather than reading `RSP`
    // directly so the implementation is portable across architectures.
    let scanner_sp = current_stack_pointer();
    // DBG (CRATONVM_DBG_FULLSTACK_SCAN): scan the ENTIRE native stack
    // [scanner_sp, stack_high] as conservative roots, not just the per-entry
    // [scanner_sp, entry_sp] ranges. Decisive experiment for the bintrees18
    // non-moving-sweep corruption: if this makes a live object visible (and
    // stops the sweep zeroing it), the missed root WAS on the stack but
    // outside the JIT chain's bounds (a range bug); if corruption persists,
    // the missed root is not on the stack at all. Validated by is_object_address.
    #[cfg(any(target_os = "windows", target_os = "linux"))]
    if dbg_fullstack_scan() {
        let high = current_thread_stack_high();
        if high > scanner_sp {
            scan_one_frame(scanner_sp, high, heap, out);
        }
    }
    // Round-7 corruption fix: before scanning, reclaim any LEAKED JIT
    // entry (a returned frame whose guard Drop was bypassed) so the GC's
    // `gc_quiescence` flag reflects only genuinely-live JIT frames. A
    // wedged flag forces the heap-corrupting non-moving young sweep on
    // every GC; healing it here lets the moving collector resume. Sound:
    // only entries strictly below the scanner SP (provably returned) are
    // pruned — live frames (entry_sp >= scanner_sp) are always retained.
    // DBG: CRATONVM_DBG_NO_PRUNE disables the self-heal so the leak (and its
    // non-moving-sweep corruption) can be A/B-reproduced in the same binary.
    if !dbg_no_prune() {
        let _ = prune_returned_jit_entries(scanner_sp);
    }
    let chain_len = JIT_ENTRY_CHAIN.with(|c| c.borrow().len());

    // A5 fix — UNREGISTERED JIT frame on the native stack. A JIT method can be
    // live WITHOUT having pushed a `JitEntryGuard`: the process entry point
    // (`Vm::invoke` → compiled app `main`) is the canonical case — it can sit on
    // the stack while a clinit / interpreted callee runs and triggers a GC. With
    // `is_active()` false the generational collector picks the MOVING young
    // collector, which relocates that frame's live objects and cannot rewrite its
    // raw (register/spill) stack slots → stale all-zero-header receiver (the
    // `main`-compiled bintrees corruption).
    //
    // The registered chain covers `[scanner_sp, max(entry_sp))`; an unregistered
    // frame sits ABOVE that. Detect it via a JIT code return address in
    // `[cover_hi, stack_high)`. On a hit, conservatively scan that above-chain
    // band (MARK the frame's oops via `is_object_address`) and flag the collector
    // to run the NON-MOVING sweep — so the oops are pinned, not relocated, keeping
    // the unscannable raw slots valid. When the chain is NON-empty the sweep is
    // ALREADY non-moving (`is_active()` true), so this only adds the marking
    // (over-retention, safe — no collector-choice / throughput change); when it
    // is empty this also flips the collector off the moving path. Cheap-gated:
    // only when ≥1 method is compiled, only on the GC root-scan path, and the
    // scan is bounded + early-exits. Windows + Linux (the Linux port landed
    // with the 2026-07-10 GC audit INT-3 follow-up, once precise maps /
    // JIT_CODE_RANGES defaulted on everywhere).
    #[cfg(any(target_os = "windows", target_os = "linux"))]
    {
        let code_ranges = cratonvm_jit::jit_code_range_count();
        // arch-2026-07-26 (`moving-young-precise-roots`): this used to carry a
        // `!moving_young_enabled()` exemption matching the one in
        // `refresh_moving_young_coverage_for_current_thread`. Both are gone.
        // This function only runs at all when the precise-only path was NOT
        // taken (see `roots.rs`), i.e. when something already needs the
        // conservative backstop — and in that situation an unregistered frame's
        // oops MUST be marked, or the non-moving sweep reclaims them. Skipping
        // the probe here made the fallback path itself lossy under moving-young.
        if code_ranges > 0 {
            let cover_hi = JIT_ENTRY_CHAIN
                .with(|c| c.borrow().iter().map(|e| e.entry_sp).max())
                .unwrap_or(scanner_sp);
            let search_lo = scanner_sp.max(cover_hi);
            let hiwater_on = unreg_memo_hiwater_enabled();
            let decision = UNREG_JIT_MEMO.with(|c| {
                let mut m = c.get();
                let d = m.observe(search_lo, code_ranges, hiwater_on);
                c.set(m);
                d
            });
            // ENGAGEMENT census for the site that actually runs. The coverage
            // probe's counter reported `calls=0` on the StackWalker workload,
            // so this is where the 17.9% comes from; counting the verdict here
            // says whether the memo saves the scan or merely observes it.
            if a5_engagement_enabled() {
                A5_PROBE_CALLS.fetch_add(1, Ordering::Relaxed);
                match decision {
                    UnregScan::AlreadyClean => A5_PROBE_MEMO_CLEAN.fetch_add(1, Ordering::Relaxed),
                    UnregScan::Detect { hi: Some(_) } => {
                        A5_PROBE_MEMO_BANDED.fetch_add(1, Ordering::Relaxed)
                    }
                    UnregScan::Detect { hi: None } => {
                        A5_PROBE_FULL_RESCAN.fetch_add(1, Ordering::Relaxed)
                    }
                };
            }
            let already_clean = decision == UnregScan::AlreadyClean;
            // Consume the authoritative marker: this scan is the one the
            // preceding `invalidate_scan_cache_for_gc` was announcing.
            let authoritative = UNREG_JIT_AUTHORITATIVE.with(|c| c.replace(false));
            if already_clean && dbg_unreg_memo_audit() {
                // Pure observer: scan the range the memo just vouched for and
                // count the disagreement. Marks nothing, decides nothing.
                UNREG_MEMO_SHORTCIRCUITS.fetch_add(1, Ordering::Relaxed);
                let high = current_thread_stack_high();
                if high > search_lo && native_stack_has_jit_frame(search_lo, high).is_some() {
                    UNREG_MEMO_SUPPRESSED.fetch_add(1, Ordering::Relaxed);
                    if authoritative {
                        // A live JIT frame's oops are missing from a root set a
                        // collection is about to mark from. This is the defect.
                        UNREG_MEMO_SUPPRESSED_AUTHORITATIVE.fetch_add(1, Ordering::Relaxed);
                    }
                }
            }
            if !already_clean {
                let high = current_thread_stack_high();
                if high > search_lo {
                    // Incremental fast path (2026-07-17 throughput fix): when the
                    // code-range set is unchanged and we have recursed DEEPER
                    // since the last verification (`search_lo < verified_lo`),
                    // the once-verified `[verified_lo, high)` band is still
                    // guaranteed clean by the exact same invariant documented on
                    // `UNREG_JIT_VERIFIED_LO` above ("nothing above our current
                    // stack pointer can change while we are nested below it") —
                    // that argument is symmetric in shallower-vs-deeper: it only
                    // depends on `[verified_lo, high)` lying entirely above
                    // (numerically) the deepest point reached since it was
                    // proven clean, which `search_lo < verified_lo` establishes
                    // just as validly as the already-handled `search_lo >=
                    // verified_lo` case above. Only the NEW incremental band
                    // `[search_lo, verified_lo)` can possibly contain content
                    // that has changed since the last check, so only it needs
                    // scanning; if it comes back clean, extend the verified
                    // boundary down to `search_lo` exactly as the shallow path
                    // already does.
                    //
                    // Without this, a workload whose interpreter call depth
                    // keeps growing (rather than staying flat or shrinking) —
                    // e.g. Hibernate/JUnit5/H2's deeply nested reflective call
                    // chains — re-scans the ENTIRE live native stack (up to the
                    // 8 MiB cap in `native_stack_has_jit_frame`) on every single
                    // per-native-call root snapshot once any method has
                    // compiled, which profiling found to be the dominant cost
                    // (`cratonvm_gc::gen_heap::GenerationalHeap::is_object_address`
                    // + `update_root_snapshot` together ~66% of total CPU in a
                    // `perf record` capture; `CRATONVM_DBG_ROOTSNAP` showed
                    // per-call cost climbing from ~13us to ~64us over a
                    // 400k-call `LockTest` run, vs. a flat ~2us with `--nojit`
                    // or with compilation never completing). See
                    // hib-misc-residuals-20260716-FIXED.md's
                    // `LockTest` section for the full investigation.
                    //
                    // Falls back to the original full-range `[search_lo, high)`
                    // scan whenever the incremental argument can't be proven
                    // safe: the very first check (`verified_lo == usize::MAX`),
                    // a `search_lo` that is not strictly deeper than
                    // `verified_lo`, or a new compilation since the last check
                    // (`code_ranges != verified_ranges`) — identical to
                    // pre-fix behavior in all of those cases.
                    // H2-CID0: the band the memo asked for — bounded by the
                    // clean FLOOR, not by `verified_lo`, so stack rewritten
                    // while this thread was shallower is rescanned.
                    let scan_hi = match decision {
                        UnregScan::Detect { hi: Some(floor) } if floor <= high => floor,
                        _ => high,
                    };
                    let probe = native_stack_has_jit_frame(search_lo, scan_hi);
                    // RESIDUE FILTER. With entries on the chain, `search_lo` is
                    // already `cover_hi`, so anything found above it is a frame
                    // the chain does not cover and must be marked. With an EMPTY
                    // chain the probe searches the whole native stack, and a
                    // compiled method that has already returned left a return
                    // address into JIT code behind it at every depth below its
                    // own `entry_sp` -- indistinguishable, by inspection, from a
                    // live guardless frame. Accepting it marked
                    // `[scanner_sp, stack_high)`: the collector's own frames,
                    // every interpreter and native Rust frame, and the leftovers
                    // of everything this thread has run. Any heap address still
                    // lying in that band then became a root on every collection.
                    //
                    // H2's `FileNioMapped.unMap` spins on `System.gc()` until a
                    // `WeakReference<MappedByteBuffer>` clears; 15 dead stack
                    // words still held the buffer, and the return address that
                    // opened the band was itself residue --
                    // `FileChannelImpl.implWrite`, long since returned. The
                    // 10 s timeout therefore always fired. See the retired
                    // bug-h2-niomapped-unmap-gc-timeout write-up.
                    //
                    // The canonical live guardless frame this probe exists for
                    // is the process entry point (`Vm::invoke` -> compiled
                    // `main`), which sits ABOVE every JIT entry the run has ever
                    // made -- every one of those was entered deeper than main's
                    // own call site -- so the filter keeps it.
                    let accept = match probe {
                        None => false,
                        Some((hit_slot, _)) => {
                            let residue_hi = jit_residue_hi();
                            chain_len > 0
                                || residue_hi == 0
                                || hit_slot >= residue_hi
                                || unreg_jit_accept_residue()
                        }
                    };
                    // WHAT THE DECLINE COSTS. `accept == false` with a hit
                    // means the residue filter judged the band to hold no live
                    // compiled frame — and that band is the only thing this
                    // site would have conservatively marked. Under
                    // `CRATONVM_DBG_UNREG_DECLINED` say which body the hit
                    // belongs to and which objects the decline is dropping, so
                    // a `CRATONVM_DBG_SWEEP_ZERO` victim can be LOOKED UP here
                    // rather than attributed by argument.
                    #[cfg(any(target_os = "windows", target_os = "linux"))]
                    if !accept {
                        if let Some((hit_slot, word)) = probe {
                            let n = UNREG_DECLINED.fetch_add(1, Ordering::Relaxed) + 1;
                            if unreg_declined_dbg() {
                                let mut would: Vec<ObjectRef> = Vec::new();
                                scan_one_frame(search_lo, high, heap, &mut would);
                                UNREG_DECLINED_ROOTS
                                    .fetch_add(would.len(), Ordering::Relaxed);
                                // Every address, every time: the victim this is
                                // meant to explain is named by a DIFFERENT probe
                                // after the fact, so a rate-limited sample of
                                // the list is a list that will not contain it.
                                let mut addrs = String::new();
                                for o in would.iter() {
                                    addrs.push_str(&format!("{:x},", o.as_ptr() as usize));
                                }
                                let label = match cratonvm_jit::pin_jit_code_range_owner(word) {
                                    Some(cm) => {
                                        let entry = cm.entry_ptr() as usize;
                                        if cm.method_label.is_empty() {
                                            format!("<unlabelled>+0x{:x}", word - entry)
                                        } else {
                                            format!(
                                                "{}+0x{:x}",
                                                cm.method_label,
                                                word - entry
                                            )
                                        }
                                    }
                                    None => "<body reclaimed>".to_string(),
                                };
                                eprintln!(
                                    "[unreg-declined] #{n} body={label} slot=0x{hit_slot:x} \
                                     word=0x{word:x} residue_hi=0x{:x} chain_len={chain_len} \
                                     band=[0x{search_lo:x},0x{high:x}) unmarked={} objs={addrs}",
                                    jit_residue_hi(),
                                    would.len(),
                                );
                            }
                        }
                    }
                    if accept {
                        // A hit anywhere in the checked band still conservatively
                        // marks (and flags) the FULL `[search_lo, high)` span —
                        // unchanged from pre-fix behavior. Only the detection
                        // scan itself is narrowed above, never the marking scope
                        // once something is actually found.
                        scan_one_frame(search_lo, high, heap, out);
                        // MARKING AND THE RELOCATION LICENCE ARE TWO QUESTIONS,
                        // and until 2026-09-08 this site answered both with the
                        // one `accept` above.
                        //
                        // `accept`'s residue test is short-circuited by
                        // `chain_len > 0`, on the argument quoted above it:
                        // "with entries on the chain, `search_lo` is already
                        // `cover_hi`, so anything found above it is a frame the
                        // chain does not cover and must be marked". That is
                        // right about MARKING and wrong about LIVENESS.
                        // `JIT_RESIDUE_HI` is MONOTONIC over the thread's whole
                        // life (its own doc says why), so a shallower JIT call
                        // that returned long ago leaves residue ABOVE the
                        // current chain's `cover_hi` — and the band
                        // `[cover_hi, residue_hi)` is then scanned and its
                        // leftovers read as a live guardless frame. Every hit
                        // on the H2 `MvsCreate` ZGC OOM was of exactly that
                        // shape (`is_residue=true`, `chain_len=1..3`), and the
                        // refusal it raised cost the collector its only
                        // defragmentation for the life of the process.
                        //
                        // So: keep marking the full band unconditionally (a
                        // conservative mark is over-retention, never a
                        // correctness risk, and `roots.rs` republishes these
                        // addresses as `publish_pinned_jit_roots`, which ZGC
                        // withholds the PAGE of), and raise the refusal only for
                        // a hit the residue mark cannot explain.
                        //
                        // The re-probe is NOT "believe the first hit was
                        // residue and stop". `native_stack_has_jit_frame`
                        // returns the LOWEST hit in the band, so a residue hit
                        // can hide a genuine one above it; the band
                        // `[residue_hi, scan_hi)` is one no returned frame on
                        // this thread can have written, and the one live
                        // guardless frame this probe exists for — the process
                        // entry point — sits above every JIT entry the run ever
                        // makes and therefore inside it.
                        //
                        // CLASSIFY FIRST, THEN DECIDE. The census describes what
                        // the probe SAW and is therefore identical under either
                        // setting of the kill switch below; only the refusal
                        // consults the switch. A census that changed with the
                        // switch could not be used to judge the switch.
                        let live_hit = match probe {
                            None => None,
                            Some((hit_slot, w)) => {
                                let residue_hi = jit_residue_hi();
                                if residue_hi == 0 || hit_slot >= residue_hi {
                                    Some((hit_slot, w))
                                } else {
                                    let live_lo = residue_hi.max(search_lo);
                                    if scan_hi > live_lo {
                                        native_stack_has_jit_frame(live_lo, scan_hi)
                                    } else {
                                        None
                                    }
                                }
                            }
                        };
                        if live_hit.is_some() {
                            cratonvm_gc::gc_quiescence::note_unregistered_jit_frame_live();
                        } else {
                            cratonvm_gc::gc_quiescence::note_unregistered_jit_frame_residue();
                        }
                        if live_hit.is_some()
                            || !unreg_residue_licence_enabled()
                            || unreg_jit_accept_residue()
                        {
                            cratonvm_gc::gc_quiescence::set_unregistered_jit_frame_on_stack();
                            // The frame's oops are now MARKED but still not
                            // rewritable, so the cycle cannot be a moving one.
                            cratonvm_gc::gc_quiescence::mark_moving_young_coverage_incomplete_because(
                                cratonvm_gc::gc_quiescence::incomplete_reason::UNREGISTERED_JIT_FRAME,
                            );
                        }
                    } else if probe.is_none() {
                        UNREG_JIT_MEMO.with(|c| {
                            let mut m = c.get();
                            m.mark_clean(search_lo, code_ranges);
                            c.set(m);
                        });
                    }
                }
            }
        }
    }

    if chain_len == 0 {
        // Cross-thread JIT-root gap detector: this thread has no live JIT
        // frames, so this thread-local scan contributes nothing — but if some
        // OTHER thread is in JIT, and this scan is part of a peer-triggered STW
        // collector's root walk, that peer's JIT roots are invisible here (they
        // are carried only by the peer's last-published `root_snapshot`). Flag
        // the unsupported multi-thread-in-JIT condition loudly instead of
        // silently proceeding (see `warn_cross_thread_jit_gap` and the
        // `scan_active_jit_frames` doc). The `any_thread_in_jit()` guard keeps
        // the truly-quiescent common case (the overwhelming majority of
        // `update_root_snapshot`'s per-native-call invocations) zero-cost.
        if any_thread_in_jit() {
            warn_cross_thread_jit_gap();
        }
        // No live JIT frames on THIS thread — nothing to scan.
        return;
    }
    // ABOVE-CHAIN CONSERVATIVE BAND — an OPT-IN LEVER, not a fix. Read
    // `above_chain_scan_enabled` before reaching for it.
    //
    // The chain scan below covers `[scanner_sp, max(entry_sp))` — every frame
    // BELOW the JIT entry. Nothing covers `[max(entry_sp), stack_high)`, the
    // VM's own Rust frames between the outermost interpreter entry and the
    // compiled call, except the A5 probe — and the A5 probe only marks that
    // band when it first finds a JIT RETURN ADDRESS in it. That is a real hole
    // in the root set, and closing it was the obvious reading of the
    // BindableTests ByteBuddy reclaim, because `CRATONVM_DBG_FULLSTACK_SCAN=1`
    // cures that failure and scanning this band is most of what it does.
    //
    // It is not the hole that failure falls through. Walking the band on every
    // collection (4978 passes, 3 056 492 roots, 1.0 GB of stack read) leaves
    // 26/27 exactly as it was, and so do the two other levers that widen the
    // same neighbourhood: `CRATONVM_NO_PRECISE_JIT_MAPS=1` (every chain entry
    // conservative, so the chain band is at its widest) and
    // `CRATONVM_JIT_UNREG_ACCEPT_RESIDUE=1` (the A5 filter's veto removed).
    // What remains of the fullstack diagnostic's difference is the PATH, not
    // the range: it also scans from `update_root_snapshot`, on every
    // object-returning native call. See `above_chain_all_paths`, and
    // `docs/internal/springboot/bindabletests-bytebuddy-receiver-reclaimed-under-gc-stress-20260908.md`.
    //
    // Sound on the same terms as the chain band beside it, which has always
    // pushed conservative roots on cycles that could still relocate: a live
    // chain entry makes `gc_quiescence::is_active()` true, and
    // `collect_garbage_inner` then diverts to the non-moving sweep through
    // either `has_conservative_roots && !moving_young` or
    // `unrewritable_conservative_jit_roots` (which additionally requires that
    // a conservative scan ran this cycle — this one). Nothing is relocated, so
    // a pointer-shaped `i64` picked up here can only over-retain.
    //
    // `CRATONVM_DBG_ABOVE_CHAIN_KB=<n>` caps the band at `n` KiB above the
    // chain, which is how a holder's depth would be bisected once one is found
    // up there.
    #[cfg(any(target_os = "windows", target_os = "linux"))]
    if above_chain_scan_enabled() && (gc_root_pass_active() || above_chain_all_paths()) {
        let cover_hi = if above_chain_from_sp() {
            scanner_sp
        } else {
            JIT_ENTRY_CHAIN
                .with(|c| c.borrow().iter().map(|e| e.entry_sp).max())
                .unwrap_or(scanner_sp)
                .max(scanner_sp)
        };
        let high = current_thread_stack_high();
        let hi = match above_chain_scan_cap_bytes() {
            Some(cap) => high.min(cover_hi.saturating_add(cap)),
            None => high,
        };
        if hi > cover_hi {
            let before = out.len();
            scan_one_frame(cover_hi, hi, heap, out);
            above_chain::note(out.len() - before, hi - cover_hi);
        }
    }
    // WS1 JIT-scan cache (see the module-level comment at `JIT_SCAN_CACHE`):
    // reuse the previous scan's roots verbatim unless a Rust↔JIT boundary
    // was crossed since (generation bump), which is the only way a spill
    // slot can have changed.
    scan_prof::bump(&scan_prof::SCANS);
    let cache_on = jit_scan_cache_enabled();
    let gen = JIT_BOUNDARY_GEN.with(|g| g.get());
    // Heap collection count: a GC frees/relocates objects WITHOUT bumping the
    // boundary generation (and several young sweeps can run at one generation
    // during a single interpreted callee), so the cached object addresses are
    // only valid while this is unchanged. See `JitScanCache::collection_count`.
    // Only sampled when the (opt-in) cache is on — `collection_count()` builds a
    // full stats snapshot, and `scan_active_jit_frames` is a per-native-call hot
    // path, so we must not pay for it on the default cache-off path.
    let collection_count = if cache_on { heap.collection_count() } else { 0 };
    // The cached roots are raw addresses in ONE heap. See `JitScanCache::heap_id`.
    let heap_id = jit_scan_cache_heap_id(heap);
    if cache_on {
        let hit = JIT_SCAN_CACHE.with(|c| {
            let c = c.borrow();
            if c.matches(gen, chain_len, heap_id, collection_count) {
                out.extend_from_slice(&c.roots);
                true
            } else {
                false
            }
        });
        if hit {
            scan_prof::bump(&scan_prof::CACHE_HITS);
            return;
        }
    }
    let scan_start = out.len();
    scan_active_jit_frames_with_sp(scanner_sp, heap, out);
    if cache_on {
        JIT_SCAN_CACHE.with(|c| {
            let mut c = c.borrow_mut();
            c.filled_gen = gen;
            c.chain_len = chain_len;
            c.heap_id = heap_id;
            c.collection_count = collection_count;
            c.roots.clear();
            c.roots.extend_from_slice(&out[scan_start..]);
        });
    }
}

/// Inner scanner: takes the caller-supplied scanner SP so it can be a
/// non-inlined function (which keeps code size sensible).
///
/// NEW-12: dispatches each chain entry to either the precise oop-map
/// walker or the conservative range scan based on the entry's
/// `precise` field. The precise path enumerates exact oops from the
/// compiled method's map; the conservative path is the NEW-1 blind
/// scan, still required for entries that were pushed without precise
/// metadata.
pub fn scan_active_jit_frames_with_sp(scanner_sp: usize, heap: &VmHeap, out: &mut Vec<ObjectRef>) {
    JIT_ENTRY_CHAIN.with(|c| {
        {
            let mut chain = c.borrow_mut();
            flush_top_rbp_cache_to_chain(chain.as_mut_slice());
        }
        let chain = c.borrow();
        // Every CONSERVATIVE entry is scanned as `[scanner_sp, entry_sp)` --
        // they all share the same low bound, so a chain of K conservative
        // entries used to walk the innermost frames K times over (O(K * depth)
        // word reads and `is_object_address` calls for a root set that is, by
        // construction, the union). The union of intervals sharing a low bound
        // is just the widest one, so take the max `entry_sp` and scan once.
        // Coverage is identical; only duplicate root pushes disappear (`out` is
        // a root list -- order and multiplicity are already immaterial to every
        // consumer). The precise entries keep their per-frame walk: each one
        // enumerates its OWN frame's oop-map slots, not a range.
        let mut conservative_high = 0usize;
        for entry in chain.iter() {
            match entry.precise {
                Some(info) => {
                    scan_prof::bump(&scan_prof::PRECISE_FRAMES);
                    scan_one_frame_precise(info, heap, out)
                }
                None => conservative_high = conservative_high.max(entry.entry_sp),
            }
        }
        if conservative_high != 0 {
            scan_prof::bump(&scan_prof::BAND_SCANS);
            let band = conservative_high.saturating_sub(scanner_sp) as u64;
            scan_prof::add(&scan_prof::BAND_WORDS, band / 8);
            scan_prof::observe_max(&scan_prof::BAND_MAX_BYTES, band);
            scan_one_frame(scanner_sp, conservative_high, heap, out);
        }
    });
}

// ---------------------------------------------------------------------------
// Stage 3 (precise oop maps) — exact RBP recording + precise relocation
// ---------------------------------------------------------------------------

/// Stage 3 — record the EXACT RBP of the *innermost* active JIT frame.
///
/// Called from the JIT prologue (via the `frame_record` helper) immediately
/// after `mov rbp, rsp`, when `CRATONVM_PRECISE_JIT_MAPS` is on. The matching
/// chain entry was pushed by [`JitEntryGuard::enter_with_compiled`] just before
/// control transferred to compiled code, but it could only capture an
/// approximate Rust-side SP as `frame_base`. This overwrites it with the
/// precise value so the relocation walker can address oop-map slots as
/// `[rbp - offset]`.
///
/// No-op if the top entry is conservative (`precise == None`): such methods
/// have no oop maps and are never relocated through this path.
pub fn set_top_frame_base(rbp: usize) {
    // HOT path (once per JIT invocation): write only the cached mirror. The
    // value is synced into the top chain entry's `exact_rbp` at push/pop/retain
    // and flushed by `remap_active_jit_frames` before the (only) reader runs.
    // `frame_base` is deliberately NOT touched — the marking path uses it as the
    // upper bound of its conservative sweep.
    top_rbp_set(rbp);
    if moving_young_enabled() {
        JIT_ENTRY_CHAIN.with(|c| {
            if let Some(top) = c.borrow_mut().last_mut() {
                if let Some(info) = top.precise.as_mut() {
                    info.exact_rbp = rbp;
                    // This RBP came from the caller, not from the mirror, so
                    // the mirror's id is not known to describe it. Publishing
                    // nothing is the honest answer — the scan falls back to
                    // decoding the call, exactly as before.
                    info.exact_cm_id = 0;
                }
            }
        });
    }
}

/// Sync `TOP_RBP` to the current top entry's saved `exact_rbp` (or 0 when the
/// chain is empty). Called after pop/retain so the mirror tracks the new top.
///
/// **Only call this when the top entry actually CHANGED.** The mirror is the
/// live value (the prologue's inline `mov gs:[disp], rbp` writes it and nothing
/// writes `exact_rbp`); the field is only a snapshot taken when an entry stops
/// being top. Reloading the field over an unchanged top therefore replaces a
/// live RBP with a stale one — `0` for an entry that has been top since it was
/// pushed, which is the common case. See `prune_returned_jit_entries`.
#[inline]
fn reload_top_rbp_cache(v: &[JitFrameChainEntry]) {
    let (rbp, cm_id) = v
        .last()
        .and_then(|e| e.precise.as_ref())
        .map_or((0, 0), |info| (info.exact_rbp, info.exact_cm_id));
    top_rbp_set(rbp);
    // BOTH halves, from the SAME snapshot. Restoring only the RBP is what
    // `top_cm_id_mirror_read` warns about in as many words: it leaves the pair
    // naming two different frames — this entry's rbp beside whatever method
    // last ran — "and the scan cannot detect that, because both halves would
    // still read consistently out of the mirrors".
    //
    // That is not hypothetical. It is what
    // `bug-h2-testkillprocess-zgc-oom-at-97-percent-free-20260821`'s
    // `ACTIVE_FRAME_MAP` residual was: `[frame-cov]` showed ONE rbp
    // (`0x…3a00`, correctly restored here) claimed across collections by four
    // different methods (`MVMap.put`, `MVStore.openMap`,
    // `MVStore$Builder.autoCommitDisabled`, `RootReference.isLocked` — the last
    // with `maps=0`, so it emits no safepoint and cannot be the frame at a
    // collection at all). `moving_young_frame_coverage_complete` then read
    // `[rbp - wrong_method.sp_id_slot_off]`, matched no map, and refused the
    // cycle.
    //
    // The path that makes it the coverage proof's OWN input:
    // `refresh_moving_young_coverage_for_current_thread` calls
    // `prune_returned_jit_entries` first, which lands here when it pruned
    // anything, and then immediately stamps
    // `info.exact_rbp = top_rbp_get(); info.exact_cm_id = published_compile_id()`
    // — a correct rbp paired with the identity this function failed to move.
    //
    // Zero is the honest value when the top entry has no snapshot: it means
    // "nothing published", which routes `published_innermost_method` to the
    // stack decode and, failing that, fails closed. A WRONG id does not fail
    // closed; it resolves confidently to the wrong map.
    if cm_id_pairing_enabled() {
        top_cm_id_mirror_write(cm_id);
    }
}

/// `CRATONVM_GC_NO_CM_ID_PAIRING=1` — stop moving the compile-id mirror with
/// the RBP mirror, restoring the 2026-08-24 behaviour in which only the RBP
/// half was reset on push and restored on pop.
///
/// The bisect lever for that repair, default-ON. Latched: it decides what the
/// entry-chain bookkeeping does, and that must not change under a running
/// process.
fn cm_id_pairing_enabled() -> bool {
    static G: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *G.get_or_init(|| {
        cratonvm_types::flags::runtime_var_os("CRATONVM_GC_NO_CM_ID_PAIRING").is_none()
    })
}

#[inline]
fn flush_top_rbp_cache_to_chain(v: &mut [JitFrameChainEntry]) {
    if let Some(top) = v.last_mut() {
        if let Some(info) = top.precise.as_mut() {
            info.exact_rbp = top_rbp_get();
            info.exact_cm_id = published_compile_id();
        }
    }
}

/// Stage 3 — precisely relocate the oop slots of every active JIT frame on
/// the current thread after a moving collection.
///
/// For each frame compiled with the precise gate on (`cm.sp_id_slot_off != 0`,
/// which also guarantees `frame_base` is the EXACT RBP recorded by
/// [`set_top_frame_base`]):
///   1. read the active safepoint's bytecode PC from `[rbp - sp_id_slot_off]`;
///   2. look up the matching [`OopMapEntry`] (`bytecode_pc == sp_id`);
///   3. for each recorded slot at `[rbp - offset]`, if the stored address moved
///      (`pointer_map[old] = new`), rewrite the slot in place.
///
/// This is the JIT-frame analogue of `update_all_roots` for interpreter
/// frames — the piece that makes a *moving* collector safe while JIT frames
/// are live. Inert by default: when the gate is off every method has
/// `sp_id_slot_off == 0`, so the walk skips all frames and touches nothing.
///
/// # Safety
/// Reads and writes aligned qwords on the calling thread's own stack between
/// known-valid frame slots. The CompiledMethod is kept alive by the JIT cache
/// for the duration of the call (same contract as [`scan_one_frame_precise`]).
pub fn remap_active_jit_frames(pointer_map: &cratonvm_types::PointerMap) {
    if pointer_map.is_empty() {
        return;
    }
    let scanner_sp = current_stack_pointer();
    // Stage 5 diagnostic (CRATONVM_DBG_PRECISE): count frames walked / slots
    // rewritten / chain entries so we can see whether the RBP-chain walk
    // actually engages. Printed once per remap call (grep-friendly).
    let dbg = cratonvm_types::flags::runtime_var_os("CRATONVM_DBG_PRECISE").is_some();
    let mut dbg_entries = 0usize;
    let mut dbg_precise = 0usize;
    let mut dbg_frames = 0usize;
    let dbg_slots = std::cell::Cell::new(0usize);
    let dbg_maps_found = std::cell::Cell::new(0usize);
    let dbg_examined = std::cell::Cell::new(0usize);
    JIT_ENTRY_CHAIN.with(|c| {
        // Flush the cached top `exact_rbp` into the top chain entry so the walk
        // below sees the live innermost RBP.
        {
            let mut v = c.borrow_mut();
            flush_top_rbp_cache_to_chain(v.as_mut_slice());
        }
        let chain = c.borrow();
        for entry in chain.iter() {
            dbg_entries += 1;
            let Some(info) = entry.precise else { continue };
            dbg_precise += 1;
            let entry_sp = entry.entry_sp;
            // The innermost frame's child is a Rust helper, so its return
            // address cannot resolve this frame through the parent walk below.
            // Remap it directly from the CompiledMethod registered at the
            // interpreter-to-JIT boundary.  This is required for *every*
            // moving collection, not just OSR: the active safepoint id and
            // oop slots are addressed from the exact RBP recorded by the
            // prologue, and leaving this boundary frame unpatched strands the
            // current method's register/frame oops at pre-move addresses.
            if info.exact_rbp != 0
                && info.exact_rbp & 0x7 == 0
                && info.exact_rbp >= scanner_sp
                && info.exact_rbp < entry_sp
            {
                // See `innermost_frame_method`: after a JIT->JIT call this RBP
                // is the CALLEE's frame, which `info.compiled_method` does not
                // describe. Remap it with the method that DOES — the callee
                // resolved from the frame's own return address — and skip the
                // frame entirely when that resolution fails, because remapping
                // with another method's oop map is the exact corruption this
                // guard exists to prevent.
                //
                // Only the remap is skipped when resolution fails — the parent
                // chain walk below still runs, exactly as it did when this was a
                // `!chain_entry_rbp_is_foreign(..)` term in the condition above.
                if let Some(innermost_cm) = innermost_frame_method(
                    info.exact_rbp,
                    info.exact_cm_id,
                    entry_sp,
                    scanner_sp,
                    info.compiled_method,
                ) {
                    // SAFETY: the chain entry's pointer is kept alive by the JIT
                    // cache while the frame is active; a resolved callee is kept
                    // alive by the live frame whose return address resolved it.
                    let boundary_cm: &cratonvm_jit::CompiledMethod = unsafe { &*innermost_cm };
                    let (found, examined, n) =
                        remap_one_jit_frame(info.exact_rbp, boundary_cm, pointer_map);
                        if stale_frame_word_check_enabled() {
                            audit_stale_frame_words(info.exact_rbp, boundary_cm, pointer_map);
                        }
                    dbg_frames += 1;
                    dbg_slots.set(dbg_slots.get() + n);
                    if found {
                        dbg_maps_found.set(dbg_maps_found.get() + 1);
                    }
                    dbg_examined.set(dbg_examined.get() + examined);
                }
            }
            // Stage 5 — walk the JIT RBP chain from the innermost frame
            // (`info.frame_base`, the EXACT RBP recorded by the deepest
            // prologue's `set_top_frame_base`) outward to the interpreter
            // boundary, remapping EVERY ancestor frame.
            //
            // Recursive/nested JIT→JIT calls do not push `JIT_ENTRY_CHAIN`
            // entries (only the interpreter→JIT boundary does), so without this
            // walk only the innermost frame would be covered — the root cause
            // of the partial bt18 fix. JIT frames use `push rbp; mov rbp,rsp`,
            // so within `[scanner_sp, entry_sp)` the saved-RBP chain
            // (`[rbp]` = caller rbp, `[rbp+8]` = return address into the
            // caller) is walkable.
            //
            // `child_rbp`'s saved-rbp/return-address identify its PARENT, which
            // is the frame we remap each step. The innermost frame was already
            // remapped directly above because its child is a Rust helper rather
            // than a JIT return address.
            //
            // Start from `exact_rbp` (the precise innermost RBP from the
            // prologue), NOT `frame_base` (the Rust-guard SP). Skip if it was
            // never recorded (gate off / no precise prologue).
            if info.exact_rbp == 0 {
                continue;
            }
            let mut child_rbp = info.exact_rbp;
            let mut guard = 0usize;
            while guard < 4096 {
                guard += 1;
                if child_rbp == 0 || child_rbp & 0x7 != 0 {
                    break;
                }
                if child_rbp < scanner_sp || child_rbp >= entry_sp {
                    break;
                }
                // SAFETY: `child_rbp` is an aligned address inside the calling
                // thread's own live JIT stack region (bounded by scanner_sp /
                // entry_sp). `[child_rbp]` is the saved caller RBP, `[child_rbp
                // + 8]` the return address into the caller.
                let parent_rbp = unsafe { (child_rbp as *const usize).read() };
                let ret_addr = unsafe { ((child_rbp + 8) as *const usize).read() };
                // A link read out of the stack is NOT yet a frame base. Both
                // helpers below dereference `[parent_rbp - sp_id_slot_off]`
                // (and `remap_one_jit_frame` WRITES every slot its oop map
                // names), so the link must be validated BEFORE it is used, not
                // after: a zero/garbage word faulted at `0 - sp_id_slot_off`
                // while walking a json-smart chain. Same order and same
                // predicate as the other rbp-chain walks in this file
                // (`scan_active_oop_map_at_rbp`'s walk and the two conservative
                // band walks).
                if parent_rbp <= child_rbp
                    || parent_rbp & 0x7 != 0
                    || parent_rbp < scanner_sp
                    || parent_rbp >= entry_sp
                {
                    break;
                }
                // Resolve the PARENT frame's CompiledMethod from the return
                // address that points into it.
                match cratonvm_jit::lookup_jit_code_range(ret_addr) {
                    Some(cm_ptr) => {
                        // SAFETY: the cm is Arc-owned by the JIT cache while any
                        // of its frames is live (a live frame keeps it cached);
                        // the registry is evicted before a cm is dropped.
                        let cm: &cratonvm_jit::CompiledMethod =
                            unsafe { &*(cm_ptr as *const cratonvm_jit::CompiledMethod) };
                        let (found, examined, n) = remap_one_jit_frame(parent_rbp, cm, pointer_map);
                        if stale_frame_word_check_enabled() {
                            audit_stale_frame_words(parent_rbp, cm, pointer_map);
                        }
                        dbg_frames += 1;
                        dbg_slots.set(dbg_slots.get() + n);
                        if found {
                            dbg_maps_found.set(dbg_maps_found.get() + 1);
                        }
                        dbg_examined.set(dbg_examined.get() + examined);
                    }
                    None => break, // parent is the interpreter / Rust boundary
                }
                child_rbp = parent_rbp;
            }
        }
    });
    if dbg {
        eprintln!(
            "[PRECISE] remap: chain_entries={} precise={} frames_walked={} maps_found={} slots_examined={} slots_rewritten={} reg_size={}",
            dbg_entries,
            dbg_precise,
            dbg_frames,
            dbg_maps_found.get(),
            dbg_examined.get(),
            dbg_slots.get(),
            cratonvm_jit::jit_code_range_count(),
        );
    }
}

/// Stage 5 — rewrite one JIT frame's oop slots via `pointer_map`.
///
/// Reads the active safepoint's bytecode PC from `[rbp - sp_id_slot_off]`,
/// finds the matching [`cratonvm_jit::OopMapEntry`], and for each recorded
/// slot at `[rbp - off]` rewrites a relocated reference in place. No-op when
/// the method was compiled without the precise gate (`sp_id_slot_off == 0`).
/// Returns (map_found, slots_examined, slots_rewritten) — the extra counts are
/// for the CRATONVM_DBG_PRECISE diagnostic (distinguish "sp-id lookup miss"
/// from "mapped slots hold only pinned oops").
/// `CRATONVM_DBG_STALE_FRAME_WORDS=1` -- after a frame is remapped, report any
/// word still holding an address this collection MOVED.
///
/// This asks the failing question directly. `CRATONVM_DBG_VERIFY_OOP_MAPS` asks
/// "does the class file call this word a reference", and on the workloads that
/// crash it answers `verifier_oop=0` for all 2.8 M candidates -- it cannot see
/// the root. This asks "did we move the object this word points at and leave
/// the word pointing at the old address", which is the defect itself: a stale
/// reference in a live compiled frame is a use-after-free in its own right.
///
/// It was built for the SIGSEGV of the retired
/// `bug-box-unbox-intrinsic-segv-under-relocation-20260902`, on that page's
/// conclusion that the fault was such a reference being read. That conclusion
/// was RETRACTED on 2026-09-04: the fault was a WRITE, by `relocate_stw`'s own
/// `ptr::copy`, into an arena granule the give-back had decommitted -- see
/// `fixed-bugs/zgc-relocation-slides-wrote-into-decommitted-granules-FIXED-20260904`.
/// This detector is kept because what it FOUND is real and independent of that
/// crash (see `remap_unmapped_frame_dupes_enabled`), not because the crash is
/// still open.
///
/// A hit is only a candidate, not proof: a DEAD copy of a moved pointer left in
/// a spill slot is stale and harmless. What makes it actionable is the slot
/// CLASS -- `classify_frame_slot` names the three storage classes the coverage
/// model does not describe -- and the method label.
pub fn stale_frame_word_check_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        cratonvm_types::flags::runtime_var_os("CRATONVM_DBG_STALE_FRAME_WORDS").is_some()
    })
}

pub mod stale_frame_audit {
    use std::sync::atomic::AtomicU64;
    /// Frames audited. The denominator: a zero `STALE` with a zero here means
    /// the audit never ran, not that the frames were clean.
    pub static FRAMES: AtomicU64 = AtomicU64::new(0);
    /// Words still holding an address this collection moved.
    pub static STALE: AtomicU64 = AtomicU64::new(0);
    /// Of those, the ones in a slot class the coverage model does NOT describe
    /// -- the predicted defect.
    pub static STALE_UNMODELLED: AtomicU64 = AtomicU64::new(0);
}

/// Scan `[rbp - frame_size, rbp]` for words that still name a moved object.
///
/// Called AFTER `remap_one_jit_frame` has rewritten every slot the maps name,
/// so anything left is by definition a slot no map named.
fn audit_stale_frame_words(
    rbp: usize,
    cm: &cratonvm_jit::CompiledMethod,
    pointer_map: &cratonvm_types::PointerMap,
) {
    use std::sync::atomic::Ordering as AOrd;
    let frame_size = cm.osr_frame_size;
    if frame_size <= 0 {
        return;
    }
    let frame_size = frame_size as usize;
    const MAX_COMPILED_FRAME_BYTES: usize = 1024 * 1024;
    if frame_size > MAX_COMPILED_FRAME_BYTES || frame_size > rbp {
        return;
    }
    stale_frame_audit::FRAMES.fetch_add(1, AOrd::Relaxed);
    let lo = rbp - frame_size;
    let mut a = (lo + 7) & !7usize;
    while a <= rbp {
        // SAFETY: inside a live compiled frame of THIS thread, bounded by the
        // same frame-size check every other walk in this file applies.
        let v = unsafe { (a as *const usize).read() };
        if let Some(&moved_to) = pointer_map.get(&v) {
            let off = (rbp - a) as i32;
            let class = classify_frame_slot(off, &cm.frame_layout);
            let unmodelled = matches!(
                class,
                "scalar-replacement-field"
                    | "licm-ref-hoist"
                    | "licm-arith-hoist"
                    | "gpr-safepoint-spill"
            );
            stale_frame_audit::STALE.fetch_add(1, AOrd::Relaxed);
            if unmodelled {
                stale_frame_audit::STALE_UNMODELLED.fetch_add(1, AOrd::Relaxed);
            }
            static LOGGED: AtomicUsize = AtomicUsize::new(0);
            if LOGGED.fetch_add(1, AOrd::Relaxed) < 40 {
                eprintln!(
                    "[STALE-FRAME-WORD] method={} slot=[rbp-{off:#x}] class={class}                      stale={v:#x} should_be={moved_to:#x} frame_size={frame_size}                      maps={} covered={} shadow_covered={}",
                    cm.method_label,
                    cm.oop_maps.len(),
                    cm.fully_oop_covered,
                    cm.fully_shadow_covered,
                );
            }
        }
        a += 8;
    }
}

fn remap_one_jit_frame(
    rbp: usize,
    cm: &cratonvm_jit::CompiledMethod,
    pointer_map: &cratonvm_types::PointerMap,
) -> (bool, usize, usize) {
    let sp_id_off = cm.sp_id_slot_off;
    if sp_id_off == 0 {
        if remap_residue_dbg() {
            eprintln!(
                "[remap-frame] method={} NO_SP_ID_SLOT inlined={:?}",
                cm.method_label,
                cm.inlined_methods
                    .iter()
                    .map(|(c, m, d)| format!("{c}.{m}{d}"))
                    .collect::<Vec<_>>()
            );
        }
        return (false, 0, 0);
    }
    let id_addr = rbp.wrapping_sub(sp_id_off as usize);
    if id_addr & 0x7 != 0 {
        return (false, 0, 0);
    }
    // SAFETY: aligned frame slot of a live JIT frame on this thread.
    let sp_id = (unsafe { (id_addr as *const usize).read() }) as u32;
    // EVERY map recorded at this bytecode pc, not the first one.
    //
    // `bytecode_pc` identifies the SAFEPOINT'S BCI, and one bci can carry more
    // than one safepoint: a call-carrying inline splice emits a real call for
    // every `invoke*` in the spliced body while `cur_bc_pc` stays pinned to the
    // enclosing invoke's bci, so `AbstractLongAssert.<init>` spliced into
    // `LongAssert.<init>` produces two maps under one id with different live
    // sets. `find` took whichever was recorded first and silently left the
    // other safepoint's slots unrewritten.
    //
    // The union is the fail-safe direction and the direction every other reader
    // of this table already takes (`moving_young_frame_coverage_complete_at`
    // and the band verifier both `filter`). Rewriting a slot that is not live
    // at this particular safepoint can only replace a word that already equals
    // a from-space base with that object's new base -- which is what the
    // conservative sweep this path replaces did to every such word in the
    // frame.
    let mut examined = 0usize;
    let mut rewritten = 0usize;
    let mut found = false;
    let mut seen: [i16; 64] = [0; 64];
    let mut seen_len = 0usize;
    let mut coverage_complete = true;
    for map in cm.oop_maps.iter().filter(|m| m.bytecode_pc == sp_id) {
        found = true;
        coverage_complete &= map.moving_young_coverage_complete;
        for &off in &map.frame_slot_offsets {
            // Two maps at one bci overlap heavily (`this` is live at both), and
            // rewriting a slot twice would take the already-moved value as a
            // fresh key. The map is small and bounded, so a linear scan over
            // what has been done is cheaper than a set; past the bound, fall
            // through to rewriting again, which is safe because the second
            // lookup of a to-space address misses.
            let mut already = false;
            for s in seen.iter().take(seen_len) {
                if *s == off {
                    already = true;
                    break;
                }
            }
            if already {
                continue;
            }
            if seen_len < seen.len() {
                seen[seen_len] = off;
                seen_len += 1;
            }
            examined += 1;
            // Slots are positive offsets; the value lives at `[rbp - off]`.
            let slot_addr = rbp.wrapping_sub(off as usize);
            if slot_addr & 0x7 != 0 {
                continue;
            }
            // SAFETY: aligned frame slot of a live JIT frame on this thread.
            let old = unsafe { (slot_addr as *const usize).read() };
            if let Some(&new) = pointer_map.get(&old) {
                // SAFETY: same slot, rewriting the relocated reference.
                unsafe { (slot_addr as *mut usize).write(new) };
                rewritten += 1;
            }
        }
    }
    if !found {
        if remap_residue_dbg() {
            eprintln!(
                "[remap-frame] method={} sp_id={} NO_MAP_FOR_SP_ID maps={}",
                cm.method_label,
                sp_id,
                cm.oop_maps.len()
            );
        }
        return (false, 0, 0);
    }
    if remap_residue_dbg() {
        report_remap_residue(
            rbp,
            cm,
            pointer_map,
            sp_id,
            &seen[..seen_len],
            rewritten,
            coverage_complete,
        );
    }
    if remap_unmapped_dupes_enabled() {
        remap_unmapped_frame_dupes(rbp, cm, pointer_map, &seen[..seen_len]);
    }
    (true, examined, rewritten)
}

/// `CRATONVM_JIT_REMAP_UNMAPPED_DUPES=1` -- after the precise remap, rewrite any
/// word in the frame that STILL holds an address this collection moved.
///
/// The defect, measured 2026-09-03 with `CRATONVM_DBG_STALE_FRAME_WORDS=1`: the
/// register allocator keeps a reference in several frame slots at once and the
/// oop map names only the canonical home. `TestCachedQueryResults.queryCounter`
/// held one object at four slots -- a GPR safepoint spill, an operand spill and
/// two below the locals boundary -- and the remap rewrote one. The rest keep
/// pointing at the old address. Confirmed at scale:
/// `duplicate_of_mapped=47946355`.
///
/// That is a use-after-free waiting to be read, and it is NOT the SIGSEGV it
/// was found while hunting: the retired
/// `bug-box-unbox-intrinsic-segv-under-relocation-20260902` turned out to be a
/// decommitted-granule WRITE inside `relocate_stw` itself. Its own numbers
/// said so before the retraction did -- rewriting these duplicates left the
/// crash at 3 of 4. Real, enormous, and a different defect.
///
/// PINNING those objects was tried first and does not work: pins only withhold
/// PAGES in the low compaction region (`relocate_stw`'s
/// `addr >= base && addr < low_end`), so an object the general slide moves is
/// unprotected. 2 of 3 runs still SIGSEGV with 96 M pins published.
///
/// So rewrite instead. The conservative-write hazard is real but narrow, and it
/// is the hazard this path's PREDECESSOR already accepted -- see
/// `remap_one_jit_frame`'s own comment: the sweep it replaced "did [this] to
/// every such word in the frame". A word is only rewritten when its value is a
/// KEY of the pointer map, i.e. exactly the base of an object this collection
/// actually moved; a primitive holding that precise bit pattern is possible and
/// is the residual risk, recorded rather than hidden.
pub fn remap_unmapped_dupes_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        cratonvm_types::flags::runtime_var_os("CRATONVM_JIT_REMAP_UNMAPPED_DUPES").is_some()
    })
}

pub mod unmapped_dupe_remap {
    use std::sync::atomic::AtomicU64;
    /// Frames swept. The denominator.
    pub static FRAMES: AtomicU64 = AtomicU64::new(0);
    /// Words rewritten that no map named.
    pub static REWRITTEN: AtomicU64 = AtomicU64::new(0);
}

fn remap_unmapped_frame_dupes(
    rbp: usize,
    cm: &cratonvm_jit::CompiledMethod,
    pointer_map: &cratonvm_types::PointerMap,
    named: &[i16],
) {
    use std::sync::atomic::Ordering as AOrd;
    let frame_size = cm.osr_frame_size;
    if frame_size <= 0 {
        return;
    }
    let frame_size = frame_size as usize;
    const MAX_COMPILED_FRAME_BYTES: usize = 1024 * 1024;
    if frame_size > MAX_COMPILED_FRAME_BYTES || frame_size > rbp {
        return;
    }
    unmapped_dupe_remap::FRAMES.fetch_add(1, AOrd::Relaxed);
    let lo = rbp - frame_size;
    let mut a = (lo + 7) & !7usize;
    while a <= rbp {
        let off = (rbp - a) as i64;
        // Slots the precise loop already handled are skipped: it rewrote them,
        // so their value is now a to-space address, and a second lookup would
        // miss anyway -- but skipping keeps the intent explicit.
        let is_named =
            off > 0 && off <= i16::MAX as i64 && named.contains(&(off as i16));
        if !is_named {
            // SAFETY: aligned word inside this thread's live compiled frame,
            // bounded by the frame size recorded at compile time.
            let v = unsafe { (a as *const usize).read() };
            if let Some(&new) = pointer_map.get(&v) {
                // SAFETY: same aligned in-frame slot, rewriting a reference to
                // an object this collection relocated.
                unsafe { (a as *mut usize).write(new) };
                unmapped_dupe_remap::REWRITTEN.fetch_add(1, AOrd::Relaxed);
            }
        }
        a += 8;
    }
}

/// `CRATONVM_DBG=remap-residue` -- after a frame's oop map has been applied,
/// walk the WHOLE frame band and report any aligned word that is still a KEY of
/// the pointer map, i.e. still names a from-space address the slide moved.
///
/// This is the direct instrument for "which frame word did the oop map fail to
/// name". The map-driven rewrite has already run when this fires, so every hit
/// is a word the moving cycle left pointing at vacated memory; it prints the
/// method, the offset from RBP, the stale value and where the object went, plus
/// the frame's mapped slots and its coverage claim.
///
/// A hit is EVIDENCE, not a verdict: a compiled frame's band also holds dead
/// spill residue, and a stale word nothing will read harms nobody. What makes
/// it decisive is the method name beside it -- a spliced constructor listed here
/// while its `this` is live is the shape of
/// `known-issues/netty/longlonghashmaptest-nullpointerexception...`, and it is
/// how that page was root-caused.
///
/// # The `oracle=[...]` field is the part that can say "live"
///
/// `stale_live` (below `live_frame_hi`) is an UPPER BOUND and was twice read as
/// a verdict. The watermark is where the operand-spill cursor stood, not a
/// liveness bound, so a slot holding bytes some earlier expression left there
/// counts as LIVE. Each such word is therefore put to the compiler's own
/// forward "must be oop" dataflow, carried on the map as
/// `OopMapEntry::local_oop_mask`:
///
/// * `local_oop` -- the dataflow PROVES this local holds a reference and the
///   map did not name it. This is the only count that is a missed root, and it
///   should be structurally impossible (every mask bit is pushed into the slot
///   list); a non-zero here is the finding, and is always printed.
/// * `local_not_oop` -- the dataflow proves the slot is not a reference at this
///   bci. Dead storage. This is what the `String.substring` witness was.
/// * `local_unreached` -- the dataflow never reached this bci, so the map named
///   NO locals and nothing can be attributed. Suspicious but unproven; paired
///   with `map_incomplete_cause::LOCAL_MASK_UNREACHED`.
/// * `inline_local_oop` / `inline_local_not_oop` -- the same two answers for a
///   local of an inlined callee, from that splice's own mask
///   (`OopMapEntry::inline_local_scopes`). Spliced locals live in the SPILL
///   band, so without the scopes every one of them read as `outside_locals`,
///   and "a spliced callee's locals are named by no oop map" is a defect this
///   repo has already paid for once.
/// * `stack_not_oop` -- the safepoint's own operand-stack model classified that
///   spill slot as a non-reference and its marks were exact. Dead storage.
///   This is the band the H2 residue is actually in: 36 of 37 stale words on
///   `TestRandomMapOps` were `region=operand-spill`.
/// * `mapped_alias` -- the slot IS named by the map and was rewritten; the value
///   the rewrite wrote is itself a pointer-map key, because a slide moves
///   objects into space other objects vacated. This is why "the word is a key
///   of the pointer map" is not proof that the word is stale, and it produced
///   this instrument's first and only `LOCAL-OOP-UNMAPPED` before it was
///   separated out.
/// * `duplicate_of_mapped` -- no oracle claims the slot, but the object the word
///   points at is ALSO named by a mapped slot of the same frame, which
///   relocation rewrote. The object is not lost. Measured on H2: the map names
///   a group of spill slots and the stale words are the copies three slots
///   below them, holding the same objects at their pre-move addresses.
/// * `outside_locals` -- spill or staging band with no scope claiming it, where
///   this oracle is silent. The per-frame detail prints
///   `FrameLayout::region_name` beside each, which is what makes an
///   `outside_locals` word actionable.
///
/// Only the single-pass x86-64 tier homes local `k` at `[rbp - 8*(k+1)]`, so IR
/// frames record no mask and no scopes. They are not silent, though: the IR
/// lowerer's slot COLOURING publishes every provably-primitive spill slot as
/// `non_oop_stack_slots`, so `stack_not_oop` is the verdict that speaks for
/// them.
fn remap_residue_dbg() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        cratonvm_types::flags::runtime_var_os("CRATONVM_DBG_REMAP_RESIDUE").is_some()
    })
}

/// Process-wide totals for the stale-word oracle, so a soak does not have to be
/// read one `[remap-frame]` line at a time.
///
/// The per-frame lines are the evidence and stay; this is the number that can
/// be quoted. The population it summarizes was previously counted only as
/// `stale_live`, an upper bound that twice read as a verdict — see
/// `report_remap_residue`.
mod residue_census {
    use std::sync::atomic::AtomicUsize;
    pub static FRAMES: AtomicUsize = AtomicUsize::new(0);
    pub static FRAMES_WITH_LIVE: AtomicUsize = AtomicUsize::new(0);
    pub static LOCAL_OOP: AtomicUsize = AtomicUsize::new(0);
    pub static LOCAL_NOT_OOP: AtomicUsize = AtomicUsize::new(0);
    pub static LOCAL_UNREACHED: AtomicUsize = AtomicUsize::new(0);
    /// ENGAGEMENT for the inline half of the oracle: how many reported frames
    /// carried at least one splice scope, and how many scopes in total. Without
    /// these, `inline_local_*` reading zero cannot be told from an oracle armed
    /// where it can never fire.
    pub static FRAMES_WITH_INLINE_SCOPES: AtomicUsize = AtomicUsize::new(0);
    pub static INLINE_SCOPES_SEEN: AtomicUsize = AtomicUsize::new(0);
    pub static INLINE_LOCAL_OOP: AtomicUsize = AtomicUsize::new(0);
    pub static INLINE_LOCAL_NOT_OOP: AtomicUsize = AtomicUsize::new(0);
    pub static STACK_NOT_OOP: AtomicUsize = AtomicUsize::new(0);
    pub static DUPLICATE_OF_MAPPED: AtomicUsize = AtomicUsize::new(0);
    pub static MAPPED_ALIAS: AtomicUsize = AtomicUsize::new(0);
    /// ENGAGEMENT for the operand-stack half: how many reported frames carried
    /// an exact stack model with at least one non-oop slot in it.
    pub static FRAMES_WITH_STACK_MODEL: AtomicUsize = AtomicUsize::new(0);
    pub static OUTSIDE_LOCALS: AtomicUsize = AtomicUsize::new(0);
}

/// The stale-word oracle's run totals, on `CRATONVM_DBG=remap-residue`.
///
/// `local_oop` is the only count that names a missed root: a frame word the
/// compiler's own "must be oop" dataflow proves holds a reference, below the
/// live watermark, that the safepoint's map did not name. The other three are
/// the reasons the raw `stale_live` count overstates — dead storage, an
/// unreached bci, and the spill band this oracle cannot speak for.
///
/// `frames` is the ENGAGEMENT counter: a zero `local_oop` beside a zero
/// `frames` says the instrument never ran, which is not the same reading as a
/// zero beside thousands of frames.
pub fn report_remap_residue_census_at_exit() {
    use std::sync::atomic::Ordering::Relaxed;
    let frames = residue_census::FRAMES.load(Relaxed);
    if frames == 0 {
        return;
    }
    eprintln!(
        "[remap-residue-summary] frames={} frames_with_live_stale={} local_oop={} local_not_oop={} local_unreached={} frames_with_inline_scopes={} inline_scopes={} inline_local_oop={} inline_local_not_oop={} frames_with_stack_model={} stack_not_oop={} duplicate_of_mapped={} mapped_alias={} outside_locals={}",
        frames,
        residue_census::FRAMES_WITH_LIVE.load(Relaxed),
        residue_census::LOCAL_OOP.load(Relaxed),
        residue_census::LOCAL_NOT_OOP.load(Relaxed),
        residue_census::LOCAL_UNREACHED.load(Relaxed),
        residue_census::FRAMES_WITH_INLINE_SCOPES.load(Relaxed),
        residue_census::INLINE_SCOPES_SEEN.load(Relaxed),
        residue_census::INLINE_LOCAL_OOP.load(Relaxed),
        residue_census::INLINE_LOCAL_NOT_OOP.load(Relaxed),
        residue_census::FRAMES_WITH_STACK_MODEL.load(Relaxed),
        residue_census::STACK_NOT_OOP.load(Relaxed),
        residue_census::DUPLICATE_OF_MAPPED.load(Relaxed),
        residue_census::MAPPED_ALIAS.load(Relaxed),
        residue_census::OUTSIDE_LOCALS.load(Relaxed),
    );
}

/// What the local-oop dataflow says about a stale frame word that
/// `live_frame_hi` classified as LIVE. See `report_remap_residue`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StaleVerdict {
    /// Above the live watermark, or the watermark is unknown — not asked.
    NotLive,
    /// The dataflow PROVES this local holds a reference and the map did not
    /// name it. The only verdict that is a missed root.
    LocalOopUnmapped,
    /// The dataflow proves the slot is not a reference at this bci: dead
    /// storage the spill cursor abandoned without clearing.
    LocalNotOop,
    /// The dataflow never reached this bci, so the map named no locals and
    /// nothing can be attributed. Unproven either way.
    LocalUnreached,
    /// As `LocalOopUnmapped`, for a local of an inlined callee.
    InlineLocalOopUnmapped,
    /// As `LocalNotOop`, for a local of an inlined callee.
    InlineLocalNotOop,
    /// The safepoint's own operand-stack model classified this spill slot as
    /// NOT holding a reference, and the mark vector was exact. Dead storage.
    StackNotOop,
    /// No oracle claims the slot, but the object it points at IS named by
    /// another slot of this same frame, which relocation rewrote. The object is
    /// therefore not lost; this word is a stale DUPLICATE of a live root.
    DuplicateOfMapped,
    /// The slot IS in this safepoint's map and was rewritten, and the value the
    /// rewrite wrote is ITSELF a key of the pointer map -- a to-space address
    /// that aliases some other object's from-space address, because a slide
    /// moves objects into space other objects vacated.
    ///
    /// Never a missed root, and the reason "this word is a key of the pointer
    /// map" is not by itself proof that a word is stale. Observed on
    /// `FileStore.readChunkFooter` (`mapped=[8=..]`, `rewritten=4` of 4 slots),
    /// where it produced the instrument's first and only LOCAL-OOP-UNMAPPED.
    MappedAlias,
    /// Spill or staging band — not a local home, so this oracle is silent.
    OutsideLocals,
}

impl StaleVerdict {
    fn as_str(self) -> &'static str {
        match self {
            StaleVerdict::NotLive => "",
            StaleVerdict::LocalOopUnmapped => "LOCAL-OOP-UNMAPPED",
            StaleVerdict::LocalNotOop => "local-not-oop",
            StaleVerdict::LocalUnreached => "local-unreached",
            StaleVerdict::InlineLocalOopUnmapped => "INLINE-LOCAL-OOP-UNMAPPED",
            StaleVerdict::InlineLocalNotOop => "inline-local-not-oop",
            StaleVerdict::StackNotOop => "stack-not-oop",
            StaleVerdict::DuplicateOfMapped => "duplicate-of-mapped",
            StaleVerdict::MappedAlias => "mapped-alias",
            StaleVerdict::OutsideLocals => "outside-locals",
        }
    }
}

/// Put one stale frame word to the compiler's forward "must be oop" dataflow.
///
/// `off` is the positive distance below RBP of the word; the single-pass x86-64
/// emitter homes JVM local `k` at `[rbp - 8*(k+1)]` (`Compiler::local_offset`),
/// so the offset names a local exactly. `local_mask` is the safepoint's
/// `OopMapEntry::local_oop_mask` — `None` meaning the dataflow never reached
/// this bci, which is NOT the same as `Some(0)`.
fn classify_stale_local(
    off: usize,
    mapped: &[i16],
    local_mask: Option<u64>,
    num_locals: usize,
    inline_scopes: &[(i32, u16, u64)],
    non_oop_stack: &[i16],
) -> StaleVerdict {
    // FIRST, because a slot the map NAMED was rewritten and cannot be a missed
    // root. Its current value being a pointer-map key is address aliasing -- a
    // slide moves objects into space other objects vacated -- and asking the
    // locals oracle about it is how this instrument produced its only false
    // `LOCAL-OOP-UNMAPPED`. See `StaleVerdict::MappedAlias`.
    if i16::try_from(off).is_ok_and(|o| mapped.contains(&o)) {
        return StaleVerdict::MappedAlias;
    }
    if off < 8 || off % 8 != 0 || (off / 8 - 1) >= num_locals {
        // Not a java local of this method. A SPLICED callee's locals are
        // allocated out of the operand-spill band and are addressed from their
        // scope's own base, so ask each live scope before giving up.
        return match classify_stale_inline_local(off, inline_scopes) {
            // No splice claims it. The last oracle is the safepoint's own
            // operand-stack model, which is where the H2 residue actually sits.
            StaleVerdict::OutsideLocals => {
                if i16::try_from(off).is_ok_and(|o| non_oop_stack.contains(&o)) {
                    StaleVerdict::StackNotOop
                } else {
                    StaleVerdict::OutsideLocals
                }
            }
            other => other,
        };
    }
    let k = off / 8 - 1;
    match local_mask {
        None => StaleVerdict::LocalUnreached,
        // A mask is 64 bits wide and cannot speak past that. Unreachable today
        // (`compute_local_oop_masks` returns nothing at all above 64 locals, so
        // the entry records `None`), and it must stay UNPROVEN rather than
        // falling into the "proved not a reference" arm below if that changes.
        Some(_) if k >= 64 => StaleVerdict::LocalUnreached,
        Some(m) if m & (1u64 << k) != 0 => StaleVerdict::LocalOopUnmapped,
        Some(_) => StaleVerdict::LocalNotOop,
    }
}

/// The same question for the locals of an inlined callee. Spliced callee local
/// `k` of a scope based at `base` is homed at `[rbp - (base + 8*k)]`
/// (`x64::inlining`), so a word inside a scope's band names one of its locals
/// exactly and the scope's own mask answers for it.
fn classify_stale_inline_local(off: usize, inline_scopes: &[(i32, u16, u64)]) -> StaleVerdict {
    let Ok(off_i) = i32::try_from(off) else {
        return StaleVerdict::OutsideLocals;
    };
    for &(base, n, mask) in inline_scopes {
        if base <= 0 || n == 0 || off_i < base {
            continue;
        }
        let delta = off_i - base;
        if delta % 8 != 0 {
            continue;
        }
        let k = (delta / 8) as usize;
        if k >= n as usize {
            continue;
        }
        // Same 64-bit limit as the outer mask: past it the scope says nothing.
        if k >= 64 {
            return StaleVerdict::LocalUnreached;
        }
        return if mask & (1u64 << k) != 0 {
            StaleVerdict::InlineLocalOopUnmapped
        } else {
            StaleVerdict::InlineLocalNotOop
        };
    }
    StaleVerdict::OutsideLocals
}

fn report_remap_residue(
    rbp: usize,
    cm: &cratonvm_jit::CompiledMethod,
    pointer_map: &cratonvm_types::PointerMap,
    sp_id: u32,
    mapped: &[i16],
    rewritten: usize,
    coverage_complete: bool,
) {
    let frame_size = cm.osr_frame_size;
    let mut hits = 0usize;
    // A raw `stale_words` count is an UPPER BOUND and cannot be acted on. The
    // spill cursor "reclaims by moving, it does not clear" (see
    // `OopMapEntry::live_frame_hi`), so a word above the live bound still holds
    // whatever reference last occupied it — a from-space address there is DEAD
    // and rewriting it would be pointless, not a missed root. Only a stale word
    // BELOW the bound is a live reference the map failed to name, and only that
    // number says whether `moving_young_coverage_complete` is lying.
    //
    // `live_frame_hi == 0` means "unknown" (the same sentinel the band verifier
    // reads), so those are counted separately rather than being silently folded
    // into either answer.
    let live_hi = cm
        .oop_maps
        .iter()
        .filter(|m| m.bytecode_pc == sp_id)
        .map(|m| m.live_frame_hi)
        .max()
        .unwrap_or(0);
    // THE ORACLE. `live_hi` is a spill watermark, so "below it and unmapped" is
    // an upper bound on missed roots and cannot be acted on — it counts dead
    // spill residue as LIVE, which is how a correct map was read as short (see
    // the page cited on `OopMapEntry::local_oop_mask`). The forward "must be
    // oop" dataflow answers the question the watermark cannot: for a word in
    // the LOCALS band, is that slot a reference at this bci?
    //
    // A `None` from ANY map at this sp-id poisons the answer for the safepoint:
    // that map named no locals at all, so nothing here can be attributed to a
    // slot the dataflow classified. Otherwise take the union — two maps at one
    // bci both describe this frame, and a slot either names a reference or does
    // not.
    let mut local_mask: Option<u64> = Some(0);
    let mut num_locals: usize = 0;
    let mut inline_scopes: Vec<(i32, u16, u64)> = Vec::new();
    let mut non_oop_stack: Vec<i16> = Vec::new();
    for m in cm.oop_maps.iter().filter(|m| m.bytecode_pc == sp_id) {
        num_locals = num_locals.max(m.num_locals as usize);
        inline_scopes.extend_from_slice(&m.inline_local_scopes);
        // A non-exact mark vector was PADDED with "not an oop" for entries
        // nobody classified, so its complement is a default and not a proof.
        // Spend it only when the safepoint said it was exact.
        if m.stack_marks_exact {
            non_oop_stack.extend_from_slice(&m.non_oop_stack_slots);
        }
        local_mask = match (local_mask, m.local_oop_mask) {
            (Some(a), Some(b)) => Some(a | b),
            _ => None,
        };
    }
    let mut stale_live = 0usize;
    let mut stale_dead = 0usize;
    let mut stale_unknown = 0usize;
    // The four verdicts the oracle can return about a word BELOW the watermark.
    // Only the first is a missed root; the other three are the reasons a raw
    // `stale_live` count overstates.
    let mut oracle_local_oop = 0usize;
    let mut oracle_local_not_oop = 0usize;
    let mut oracle_local_unreached = 0usize;
    let mut oracle_inline_local_oop = 0usize;
    let mut oracle_inline_local_not_oop = 0usize;
    let mut oracle_stack_not_oop = 0usize;
    let mut oracle_duplicate = 0usize;
    let mut oracle_mapped_alias = 0usize;
    let mut oracle_outside_locals = 0usize;
    let mut detail = String::new();
    // The values the map's own slots hold NOW, i.e. after relocation rewrote
    // them. A stale word whose target is among these points at an object the
    // frame still reaches through a named root: the object is not lost, and the
    // word is a stale DUPLICATE rather than a missing root. Cheap, and it needs
    // no metadata the map does not already carry.
    let mut mapped_vals: Vec<usize> = Vec::new();
    for &off in mapped {
        let a = rbp.wrapping_sub(off as usize);
        // One entry per mapped slot, in order, so the description below can
        // index it; a misaligned slot (never seen) contributes 0, which is not
        // a pointer-map key and so cannot produce a false duplicate.
        mapped_vals.push(if a & 0x7 == 0 {
            // SAFETY: aligned frame slot of a live JIT frame on this thread.
            unsafe { (a as *const usize).read() }
        } else {
            0
        });
    }
    if frame_size > 0 && (frame_size as usize) <= 1024 * 1024 && (frame_size as usize) <= rbp {
        let frame_size = frame_size as usize;
        let lo = (rbp - frame_size + 7) & !7usize;
        let mut addr = lo;
        while addr + 8 <= rbp {
            // SAFETY: aligned word inside this thread's own live JIT frame band.
            let w = unsafe { (addr as *const usize).read() };
            if let Some(&new) = pointer_map.get(&w) {
                hits += 1;
                let off = rbp - addr;
                let class = if live_hi <= 0 {
                    stale_unknown += 1;
                    "unknown"
                } else if (off as i64) < live_hi as i64 {
                    stale_live += 1;
                    "LIVE"
                } else {
                    stale_dead += 1;
                    "dead"
                };
                // Ask the oracle about the words the watermark called LIVE.
                // Local `k` is homed at `[rbp - 8*(k+1)]` (the emitter's
                // `local_offset`), so an offset maps back to a local index
                // exactly; anything outside that band is spill or staging,
                // about which this oracle says nothing.
                let verdict = if class != "LIVE" {
                    StaleVerdict::NotLive
                } else {
                    classify_stale_local(
                        off,
                        mapped,
                        local_mask,
                        num_locals,
                        &inline_scopes,
                        &non_oop_stack,
                    )
                };
                // Last resort before "unexplained": is this the same object a
                // named slot of this frame already points at?
                let verdict = if verdict == StaleVerdict::OutsideLocals
                    && mapped_vals.contains(&new)
                {
                    StaleVerdict::DuplicateOfMapped
                } else {
                    verdict
                };
                match verdict {
                    StaleVerdict::NotLive => {}
                    StaleVerdict::LocalOopUnmapped => oracle_local_oop += 1,
                    StaleVerdict::LocalNotOop => oracle_local_not_oop += 1,
                    StaleVerdict::LocalUnreached => oracle_local_unreached += 1,
                    StaleVerdict::InlineLocalOopUnmapped => oracle_inline_local_oop += 1,
                    StaleVerdict::InlineLocalNotOop => oracle_inline_local_not_oop += 1,
                    StaleVerdict::StackNotOop => oracle_stack_not_oop += 1,
                    StaleVerdict::DuplicateOfMapped => oracle_duplicate += 1,
                    StaleVerdict::MappedAlias => oracle_mapped_alias += 1,
                    StaleVerdict::OutsideLocals => oracle_outside_locals += 1,
                }
                // A word the oracle proves is a reference is the finding, and
                // it is rare enough to always print. The rest share a budget.
                if matches!(
                    verdict,
                    StaleVerdict::LocalOopUnmapped | StaleVerdict::InlineLocalOopUnmapped
                ) || (stale_live <= 12 && class == "LIVE")
                {
                    detail.push_str(&format!(
                        " [LIVE off={} k={} {} region={} stale=0x{:x}->0x{:x}]",
                        off,
                        if off >= 8 && off % 8 == 0 {
                            (off / 8 - 1) as i64
                        } else {
                            -1
                        },
                        verdict.as_str(),
                        // Which band of the frame the word sits in. Free, from
                        // the layout the method already carries, and it is what
                        // makes an `outside-locals` verdict actionable: an
                        // operand spill slot, a callee-saved register image and
                        // an outgoing-argument word are three different
                        // questions, and the oracle above cannot tell them
                        // apart.
                        cm.frame_layout.region_name(i32::try_from(off).unwrap_or(i32::MAX)),
                        w,
                        new
                    ));
                }
            }
            addr += 8;
        }
    }
    {
        use std::sync::atomic::Ordering::Relaxed;
        residue_census::FRAMES.fetch_add(1, Relaxed);
        if stale_live > 0 {
            residue_census::FRAMES_WITH_LIVE.fetch_add(1, Relaxed);
        }
        residue_census::LOCAL_OOP.fetch_add(oracle_local_oop, Relaxed);
        residue_census::LOCAL_NOT_OOP.fetch_add(oracle_local_not_oop, Relaxed);
        residue_census::LOCAL_UNREACHED.fetch_add(oracle_local_unreached, Relaxed);
        if !inline_scopes.is_empty() {
            residue_census::FRAMES_WITH_INLINE_SCOPES.fetch_add(1, Relaxed);
            residue_census::INLINE_SCOPES_SEEN.fetch_add(inline_scopes.len(), Relaxed);
        }
        residue_census::INLINE_LOCAL_OOP.fetch_add(oracle_inline_local_oop, Relaxed);
        residue_census::INLINE_LOCAL_NOT_OOP.fetch_add(oracle_inline_local_not_oop, Relaxed);
        residue_census::STACK_NOT_OOP.fetch_add(oracle_stack_not_oop, Relaxed);
        residue_census::DUPLICATE_OF_MAPPED.fetch_add(oracle_duplicate, Relaxed);
        residue_census::MAPPED_ALIAS.fetch_add(oracle_mapped_alias, Relaxed);
        if !non_oop_stack.is_empty() {
            residue_census::FRAMES_WITH_STACK_MODEL.fetch_add(1, Relaxed);
        }
        residue_census::OUTSIDE_LOCALS.fetch_add(oracle_outside_locals, Relaxed);
    }
    let mut mapped_desc = String::new();
    for (i, &off) in mapped.iter().enumerate() {
        mapped_desc.push_str(&format!(
            " {}=0x{:x}",
            off,
            mapped_vals.get(i).copied().unwrap_or(0)
        ));
    }
    eprintln!(
        "[remap-frame] method={} sp_id={} frame_size={} cov_complete={} live_hi={} local_mask={:?} num_locals={} mapped=[{}] rewritten={} inlined={:?} stale_words={} stale_live={} stale_dead={} stale_unknown={} scopes={:?} oracle=[local_oop={} local_not_oop={} local_unreached={} inline_local_oop={} inline_local_not_oop={} stack_not_oop={} duplicate_of_mapped={} mapped_alias={} outside_locals={}]{}",
        cm.method_label,
        sp_id,
        frame_size,
        coverage_complete,
        live_hi,
        local_mask,
        num_locals,
        mapped_desc,
        rewritten,
        cm.inlined_methods
            .iter()
            .map(|(c, m, d)| format!("{c}.{m}{d}"))
            .collect::<Vec<_>>(),
        hits,
        stale_live,
        stale_dead,
        stale_unknown,
        inline_scopes,
        oracle_local_oop,
        oracle_local_not_oop,
        oracle_local_unreached,
        oracle_inline_local_oop,
        oracle_inline_local_not_oop,
        oracle_stack_not_oop,
        oracle_duplicate,
        oracle_mapped_alias,
        oracle_outside_locals,
        detail,
    );
}

/// NEW-12: enumerate exact oops in a JIT frame using the compiled
/// method's precise oop map.
///
/// The current PC in the active frame is obtained by subtracting the
/// compiled method's entry pointer from the current return-address at
/// `frame_base - 8` (the standard x86-64 calling convention stores the
/// return PC one word below RBP when RBP has been spilled; for
/// currently-executing frames with RBP = entry SP we take the next
/// word up as a safe over-approximation and accept that some
/// safepoints may fall through to the conservative fallback).
///
/// For correctness-at-any-PC coverage, if no exact map match is found
/// we fall through to the conservative scan of the frame region. This
/// keeps the walker functional even when the compiler has only
/// populated oop maps at a subset of safepoints — a realistic state
/// during the staged rollout described in `docs/roadmap.md` NEW-12.
// ---------------------------------------------------------------------------
// Step 3 (docs/feature-designs/precise-jit-maps-default.md) — coverage
// visibility + a completeness oracle. Both gates are default-OFF and read-only,
// so the default GC scan is unchanged (two cached-bool branches per frame).
// ---------------------------------------------------------------------------

/// `CRATONVM_DBG_VERIFY_OOP_MAPS` — when set, each precise-frame GC scan also
/// runs [`verify_precise_covers_conservative`], logging any in-band word that
/// looks like a live oop but is not recorded by ANY of the method's oop maps.
/// Default-off; the normal union scan still runs, so behaviour is unchanged.
/// This is the completeness oracle the design doc wants before the conservative
/// backstop could ever be lifted for a moving collector.
#[inline]
fn verify_oop_maps_enabled() -> bool {
    use std::sync::OnceLock;
    static E: OnceLock<bool> = OnceLock::new();
    *E.get_or_init(|| {
        if cratonvm_types::flags::runtime_var_os("CRATONVM_DBG_VERIFY_OOP_MAPS").is_some() {
            return true;
        }
        // ARMED AUTOMATICALLY FOR THE AARCH64 SAFEPOINT PATH.
        //
        // That backend's `fully_oop_covered` is computed from an argument, not
        // from a run: no host in this repository executes aarch64, so its maps
        // have never been checked against a live frame. This oracle is the
        // check -- it walks each frame against its OWN method's maps and
        // refutes a coverage claim it can disprove
        // (`incomplete_reason::COVERAGE_ORACLE_REFUTED`) -- and a claim that
        // has never been executed should not be trusted on its first run
        // merely because nobody remembered an environment variable.
        //
        // It costs a read-only walk per precise frame, on a path that is itself
        // opt-in and experimental. `CRATONVM_JIT_ARM64_SAFEPOINTS=0` (the
        // default) leaves this exactly as it was: off.
        matches!(
            cratonvm_types::flags::runtime_var("CRATONVM_JIT_ARM64_SAFEPOINTS")
                .as_deref()
                .map(str::trim)
                .map(str::to_ascii_lowercase)
                .as_deref(),
            Ok("1") | Ok("true") | Ok("on") | Ok("yes")
        )
    })
}

/// `CRATONVM_PRECISE_COVERAGE_PIN` — when set, surface
/// [`cratonvm_jit::CompiledMethod::fully_oop_covered`] at GC scan time: count
/// (and rate-limit log) precise frames that are NOT fully covered. On the
/// non-moving sweep the conservative backstop already pins every found oop, so
/// this is byte-identical today; it is the explicit visibility + scaffold for
/// the future moving path, where an un-covered frame must be PINNED, not
/// relocated. Default-off.
#[inline]
fn coverage_pin_enabled() -> bool {
    use std::sync::OnceLock;
    static E: OnceLock<bool> = OnceLock::new();
    *E.get_or_init(|| {
        cratonvm_types::flags::runtime_var_os("CRATONVM_PRECISE_COVERAGE_PIN").is_some()
    })
}

/// Set once the completeness oracle has REFUTED some frame's
/// `fully_oop_covered` claim: an in-band live object address named by no map of
/// a frame that asserted full coverage.
///
/// Latched for the life of the process, not per-cycle, and the reason is not
/// caution — it is that the refutation is a statement about COMPILED CODE, not
/// about a moment. The method that produced the unnamed slot is still in the
/// code cache and will run again; a later cycle that happens not to re-observe
/// it has learned nothing new. Clearing this per cycle would make the gate a
/// coin toss on whether the offending frame was on the stack when the oracle
/// last looked.
///
/// Production-global / `cfg(test)`-thread-local, the same split
/// `gc_quiescence` uses for its own cycle flags and for the same reason: the
/// latch is one-way by design, so a test that sets it in a shared process
/// would permanently change what every later test in that process observes.
#[cfg(not(test))]
static COVERAGE_ORACLE_REFUTED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

#[cfg(test)]
thread_local! {
    static COVERAGE_ORACLE_REFUTED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

#[cfg(not(test))]
fn coverage_oracle_refuted_get() -> bool {
    COVERAGE_ORACLE_REFUTED.load(Ordering::Acquire)
}

#[cfg(not(test))]
fn coverage_oracle_refuted_swap() -> bool {
    COVERAGE_ORACLE_REFUTED.swap(true, Ordering::AcqRel)
}

#[cfg(test)]
fn coverage_oracle_refuted_get() -> bool {
    COVERAGE_ORACLE_REFUTED.with(|c| c.get())
}

#[cfg(test)]
fn coverage_oracle_refuted_swap() -> bool {
    COVERAGE_ORACLE_REFUTED.with(|c| c.replace(true))
}

/// Clear the latch. Test-only; the production latch is deliberately one-way.
#[cfg(test)]
pub fn reset_coverage_oracle_refuted_for_test() {
    COVERAGE_ORACLE_REFUTED.with(|c| c.set(false));
}

/// Whether the completeness oracle has ever refuted a coverage claim in this
/// process. Read by the root scan before it spends the bit.
pub fn coverage_oracle_refuted() -> bool {
    coverage_oracle_refuted_get()
}

/// Record a refutation and mark THIS cycle incomplete, so the collector that is
/// mid-decision diverts as well as every later one.
fn note_coverage_oracle_refutation() {
    let first = !coverage_oracle_refuted_swap();
    cratonvm_gc::gc_quiescence::mark_moving_young_coverage_incomplete_because(
        cratonvm_gc::gc_quiescence::incomplete_reason::COVERAGE_ORACLE_REFUTED,
    );
    if first {
        eprintln!(
            "[VERIFY-OOP-MAPS] REFUTED: a frame asserting fully_oop_covered holds an \
             in-band oop no map names. Conservative JIT backstop is now forced ON for \
             the rest of this process."
        );
    }
}

/// Run the completeness oracle over this thread's live compiled frames and
/// report whether it refutes any frame's coverage claim.
///
/// This exists because of a structural hole, not a missing feature. The oracle
/// runs inside `scan_one_frame_precise`, which runs inside
/// `scan_active_jit_frames` — and `collect_roots` calls that only when it has
/// DECIDED NOT to suppress. So on every cycle where the coverage bit is
/// actually spent, the instrument designated to check it is not running. Every
/// `while_covered=0` reading ever taken was taken on cycles that did not use
/// the bit.
///
/// `roots` is scanned into and then truncated back by the caller when the proof
/// holds: the walk cannot be done without producing roots, and producing them
/// is how we know the walk really covered the same frames the backstop would.
pub fn verify_active_coverage_into(heap: &VmHeap, roots: &mut Vec<ObjectRef>) -> bool {
    let before = oop_map_audit::NEVER_MAPPED_WHILE_COVERED.load(Ordering::Relaxed);
    // Both claims, because the near-vacuous one cannot carry this alone:
    // `fully_oop_covered` is the FRAME-SLOT notion, false for any method with a
    // direct JIT->JIT call taking a reference argument, so a gate watching only
    // it almost never has a true guard to fire on. The moving path spends
    // `fully_shadow_covered`, so an unmapped live oop under THAT claim is the
    // refutation that matters.
    let before_shadow = oop_map_audit::NEVER_MAPPED_WHILE_SHADOW_COVERED.load(Ordering::Relaxed);
    // The map-SELECTION gap refutes the suppression exactly as a coverage gap
    // does, and this gate used not to look at it.
    //
    // `WRONG_MAP` counts an in-band live object address named by SOME oop map
    // of the owning method but NOT by the one its safepoint id selects — and
    // the selected map is all the precise scan reads
    // (`scan_active_oop_map_at_rbp` resolves one map through
    // `find_oop_map_for_safepoint_id` and iterates only its `slot_offsets`).
    // So such a word is invisible to the precise walk. Today that is harmless
    // because the conservative backstop finds it anyway; the whole point of
    // this gate is to decide whether the backstop may be SKIPPED, and on that
    // question a wrong-map word strands its object just as completely as an
    // unmapped one.
    //
    // Measured on the 2026-09-02 soak: `wrong_map` is non-zero on workloads
    // whose `while_covered` is zero, so a gate reading only the coverage
    // counters returns "proof holds" over frames it has just been shown hold
    // unreachable-by-the-scan oops.
    let before_wrong_map = oop_map_audit::WRONG_MAP_VERIFIER_OOP.load(Ordering::Relaxed);
    scan_active_jit_frames(heap, roots);
    if oracle_force_refute() {
        note_coverage_oracle_refutation();
        return true;
    }
    oop_map_audit::NEVER_MAPPED_WHILE_COVERED.load(Ordering::Relaxed) > before
        || oop_map_audit::NEVER_MAPPED_WHILE_SHADOW_COVERED.load(Ordering::Relaxed) > before_shadow
        || oop_map_audit::WRONG_MAP_VERIFIER_OOP.load(Ordering::Relaxed) > before_wrong_map
}

/// Whether the pre-suppression verification should run: only when the oracle is
/// enabled (it is the thing doing the verifying) or when the forced-refutation
/// switch is on. Default-off, so the default path pays one cached bool.
pub fn coverage_gate_active() -> bool {
    verify_oop_maps_enabled() || oracle_force_refute()
}

/// `CRATONVM_DBG_OOP_ORACLE_FORCE_REFUTE=1` — treat the first covered frame the
/// oracle inspects as refuted, without a real unmapped oop.
///
/// The gate below is only reachable when a compiled method actually strands an
/// oop, which is exactly the thing every fix on this page has been closing —
/// `while_covered` is 0 on every workload measured. A gate whose wiring has
/// never been executed is indistinguishable from a gate that does not work, so
/// this forces the branch instead of waiting for a defect to supply it.
/// Test-only; it makes the VM refuse a suppression it could legitimately take.
fn oracle_force_refute() -> bool {
    use std::sync::OnceLock;
    static E: OnceLock<bool> = OnceLock::new();
    *E.get_or_init(|| {
        cratonvm_types::flags::runtime_var_os("CRATONVM_DBG_OOP_ORACLE_FORCE_REFUTE").is_some()
    })
}

/// Count of precise frames observed NOT `fully_oop_covered` during GC scans
/// while `CRATONVM_PRECISE_COVERAGE_PIN` is on. Diagnostic only.
static UNCOVERED_PRECISE_FRAMES: AtomicUsize = AtomicUsize::new(0);
/// Shared rate-limit for the Step-3 diagnostic logs (so neither knob spams).
static STEP3_LOG_COUNT: AtomicUsize = AtomicUsize::new(0);
const STEP3_LOG_CAP: usize = 64;

/// Diagnostic read of the uncovered-precise-frame counter (see
/// [`coverage_pin_enabled`]). 0 when the knob was never on.
pub fn uncovered_precise_frame_count() -> usize {
    UNCOVERED_PRECISE_FRAMES.load(Ordering::Relaxed)
}

/// Step 3 completeness oracle — for one precise frame, diff the union of the
/// method's oop-map slots against a conservative sweep of the same stack band,
/// logging (rate-limited) any band word that looks like a live oop but is not
/// recorded by ANY map. Read-only.
///
/// CAVEAT: a `JIT_ENTRY_CHAIN` entry is pushed only at the interpreter→JIT
/// boundary, so the `[scanner_sp, frame_base)` band also spans this method's
/// *nested JIT→JIT callees*. Their oops are correctly absent from THIS method's
/// maps and therefore show here as "unmapped" — expected, not a gap. The signal
/// is sharpest for leaf-ish compiled frames (the register/spill-resident root
/// class, e.g. the bintrees-`main`-reads-`args` and `codePointAt` families).
/// Counters for [`verify_precise_covers_conservative`], reported at exit.
/// What the CLASS FILE's own verifier says about a java-local slot at a bci.
///
/// The reason this exists is that the oop-map oracle could not tell its three
/// populations apart, and said so: at 5–6 % of every in-band word on every
/// workload measured, `never_mapped` was dominated by two false positives it
/// declared but could not subtract —
///
///   * a primitive `i64` whose bits happen to land on a live object header, and
///   * a DEAD slot: an object reference left behind after its variable went out
///     of scope (`MVStore.closeStore` slot 5, scoped 149..170, read at bci 174,
///     plus javac's three `finally`-tail copies of it).
///
/// `org.h2.test.db.TestMultiThread` PASSES under a moving collector with 1.1 M
/// never-mapped words, 670 k of them in frames asserting shadow coverage. If
/// those were genuine the workload would corrupt. So the counter could not be a
/// backstop, and the page tracking it recorded that "a backstop that can tell a
/// dead slot from a live one without the map does not exist yet".
///
/// # Why the verifier's map is an INDEPENDENT answer and the oop map is not
///
/// Asking the JIT's own abstract interpreter would be circular: the oop map IS
/// its output, so "the map does not name this slot" and "the model says it is
/// not a live reference" are the same sentence. `classloading::type_maps` is a
/// different oracle with a different provenance — it is what
/// `bytecode_verifier` retains from the JVMS §4.10.1 StackMapTable walk, i.e.
/// **javac's own claim** about the type of every local at every instruction
/// start. It answers both false positives at once: a primitive local is not an
/// oop there, and an out-of-scope local is `Top`, which is not an oop either.
///
/// # What it refuses to answer, and why each refusal is load-bearing
///
/// * **An INLINED frame.** A spliced callee's locals live in the same java-local
///   region and the safepoint's bci belongs to the CALLEE's bytecode, so reading
///   the outer method's map at that pc reads a different method's types at a
///   coincidentally-valid pc. `Unknown`, always.
/// * **Any slot that is not a java local.** Operand spill is `Frame::stack`
///   indexed by runtime depth, which this frame does not carry; LICM hoists,
///   scalar-replaced fields and the register images are regions the verifier
///   has never heard of. Those are exactly the regions the band scan still
///   covers, so nothing is lost.
/// * **A pc with no row.** `local_oops_at` returns `None` for a pc that is not
///   an instruction start — which, for a safepoint id, means the frame is not
///   where its sp-id slot says it is. That is a finding of its own and must not
///   be read as "not an oop".
/// * **A name the index cannot resolve.** `class_id_of_name` is name-keyed and
///   first-writer-wins, so two loaders' versions of one class collapse; the
///   collision count is printed beside the verdicts so a reader can discount
///   them.
///
/// Every refusal counts as [`oop_map_audit::VERIFIER_UNKNOWN`] rather than
/// being folded into either verdict, because a backstop whose "no gap here"
/// silently includes "could not look" is the vacuous green this whole line of
/// work exists to avoid.
#[derive(Clone, Copy, PartialEq, Eq)]
enum VerifierSlotVerdict {
    /// The class file says this local holds a REFERENCE at this bci, and no oop
    /// map of the frame names its slot. This is the actionable number.
    Oop,
    /// The class file says it is a primitive or out of scope here. The word is
    /// a false positive of the address-shaped-bits test.
    NotOop,
    /// No independent answer available — see the refusals above.
    Unknown,
}

/// Split a `CompiledMethod::method_label` (`"org/h2/mvstore/MVStore.closeStore:(ZI)V"`)
/// into its three parts.
///
/// Descriptor first: a descriptor cannot contain `:`, and a method name cannot
/// contain `.` or `:`, so two `rsplit_once`es are exact. Labels the JIT writes
/// for synthetic bodies (`"lambda-adapter->0x…"`) have neither separator and
/// fall out as `None`.
fn split_method_label(label: &str) -> Option<(&str, &str, &str)> {
    let (owner_and_name, descriptor) = label.rsplit_once(':')?;
    let (class_name, method_name) = owner_and_name.rsplit_once('.')?;
    if class_name.is_empty() || method_name.is_empty() || descriptor.is_empty() {
        return None;
    }
    Some((class_name, method_name, descriptor))
}

/// The verifier's verdict for the java local at `[rbp - off]` of `cm`, at the
/// bci its active safepoint id names. See [`VerifierSlotVerdict`].
fn verifier_local_verdict(
    cm: &cratonvm_jit::CompiledMethod,
    off: i32,
    bci: u32,
) -> VerifierSlotVerdict {
    if !cm.inlined_methods.is_empty() {
        return VerifierSlotVerdict::Unknown;
    }
    if bci == u32::MAX {
        return VerifierSlotVerdict::Unknown;
    }
    // `FrameLayout`: java local `i` lives at `[rbp - (i + 1) * 8]`.
    if off < 8 || off % 8 != 0 || cm.frame_layout.java_locals_hi <= 0 {
        return VerifierSlotVerdict::Unknown;
    }
    if off > cm.frame_layout.java_locals_hi {
        return VerifierSlotVerdict::Unknown;
    }
    let local_index = (off / 8 - 1) as usize;
    let Some((class_name, method_name, descriptor)) = split_method_label(&cm.method_label) else {
        return VerifierSlotVerdict::Unknown;
    };
    let Some(class_id) = cratonvm_classloading::class_id_of_name(class_name) else {
        return VerifierSlotVerdict::Unknown;
    };
    let Some(maps) = cratonvm_classloading::type_maps_for_named(class_id, method_name, descriptor)
    else {
        return VerifierSlotVerdict::Unknown;
    };
    let Some(bits) = maps.local_oops_at(bci) else {
        return VerifierSlotVerdict::Unknown;
    };
    if local_index >= bits.len() {
        return VerifierSlotVerdict::Unknown;
    }
    if bits.get(local_index) {
        VerifierSlotVerdict::Oop
    } else {
        VerifierSlotVerdict::NotOop
    }
}

pub mod oop_map_audit {
    use std::sync::atomic::{AtomicU64, Ordering};

    /// Frames whose band was walked and whose maps were readable.
    pub static FRAMES: AtomicU64 = AtomicU64::new(0);
    /// Verifiable in-band words examined.
    pub static WORDS: AtomicU64 = AtomicU64::new(0);
    /// In-band object addresses named by NO oop map of the owning frame.
    /// **This is the number that says whether the codegen's coverage bit is
    /// sound**: a live reference in a slot the frame's maps never mention.
    pub static NEVER_MAPPED: AtomicU64 = AtomicU64::new(0);
    /// In-band object addresses named by SOME map of the owning frame but not
    /// by the one its safepoint id selects. Not a codegen coverage gap — a
    /// map-selection gap, which strands the oop just as effectively.
    ///
    /// **Raw, and it over-reports.** A slot whose value merely LOOKS like a
    /// heap address is counted here even when it is dead storage — an old
    /// pointer left in a reusable local or spill slot after the value died,
    /// which the precise map is right to omit and the conservative scan keeps
    /// alive for nothing. Read [`WRONG_MAP_VERIFIER_OOP`] for the population
    /// that is actually evidence.
    pub static WRONG_MAP: AtomicU64 = AtomicU64::new(0);
    /// The subset of [`WRONG_MAP`] that the CLASS FILE's own type maps confirm
    /// holds a reference at that bci — the same independent oracle
    /// `NEVER_MAPPED` is split by, applied to the map-selection population that
    /// had no verdict at all.
    ///
    /// This is the number a refutation may be built on. A non-zero reading is a
    /// live reference the selected map omits; a zero reading over a large
    /// `WRONG_MAP` says the raw counter was measuring dead slots.
    pub static WRONG_MAP_VERIFIER_OOP: AtomicU64 = AtomicU64::new(0);
    /// The rest of [`WRONG_MAP`]: the verifier says NOT a reference, or cannot
    /// answer (an inlined frame, a slot outside the java-locals band, no type
    /// maps for the method). Kept apart so a zero in
    /// [`WRONG_MAP_VERIFIER_OOP`] beside a large count here is legible as
    /// "asked and answered no", not "never asked".
    pub static WRONG_MAP_VERIFIER_OTHER: AtomicU64 = AtomicU64::new(0);
    /// Object addresses BELOW the innermost compiled frame — interpreter,
    /// native and Rust frames the compiled method called into. Not the oop
    /// map's responsibility; counted so it can be subtracted rather than
    /// mistaken for a gap, which is what the previous oracle did.
    pub static BELOW_JIT: AtomicU64 = AtomicU64::new(0);
    /// Frames whose safepoint id or maps could not be read, so their band was
    /// scanned against an EMPTY active set and their words are not counted.
    pub static UNREADABLE_FRAMES: AtomicU64 = AtomicU64::new(0);
    /// `never_mapped` hits on a frame whose `fully_oop_covered` is `true` —
    /// i.e. the codegen bit asserting complete coverage was wrong.
    pub static NEVER_MAPPED_WHILE_COVERED: AtomicU64 = AtomicU64::new(0);

    /// `never_mapped` hits on a frame whose `fully_shadow_covered` is `true`.
    ///
    /// **This is the number the question actually wants.** `fully_oop_covered`
    /// above is the FRAME-SLOT notion, and a direct JIT→JIT call taking a
    /// reference argument can never satisfy it — 439 of 449 recorded coverage
    /// failures were that one shape — so `while_covered` is near-vacuous: its
    /// guard is almost never true, and a 0 means "never asked", not "never
    /// wrong". The moving path spends `fully_shadow_covered` (the OSR check
    /// reads it, and it is the aggregate the per-frame proof is built from), so
    /// that is the claim an unmapped live oop would refute.
    ///
    /// It matters now because the 2026-08-27 band-verifier change gave up
    /// refusing collections over unpublished words in java-local and
    /// operand-spill slots. That backstop is gone; this counter is what
    /// replaces it, and it is only worth anything read beside its engagement
    /// counters below.
    pub static NEVER_MAPPED_WHILE_SHADOW_COVERED: AtomicU64 = AtomicU64::new(0);
    /// Frames inspected that asserted `fully_oop_covered`.
    pub static FRAMES_CLAIMING_OOP_COVERAGE: AtomicU64 = AtomicU64::new(0);
    /// Frames inspected that asserted `fully_shadow_covered`.
    ///
    /// The ENGAGEMENT counter for the line above: a zero refutation count means
    /// nothing while this is also zero, because the oracle then never inspected
    /// a frame making the claim.
    pub static FRAMES_CLAIMING_SHADOW_COVERAGE: AtomicU64 = AtomicU64::new(0);

    /// `never_mapped` hits the CLASS FILE's own verifier corroborates: the
    /// local is a REFERENCE at this bci and no map of the frame names its slot.
    ///
    /// **This is the backstop the page tracking this said did not exist.** The
    /// raw `never_mapped` above cannot be one -- 5-6% of every in-band word on
    /// every workload measured, dominated by the two false positives it
    /// declares (a primitive whose bits look like a header, and a dead
    /// out-of-scope slot), with `TestMultiThread` PASSING on 1.1 M of them. The
    /// verifier's type maps answer both, from a different oracle: they are what
    /// `bytecode_verifier` retains from the StackMapTable walk, i.e. javac's
    /// claim rather than this JIT's. A ZERO here IS a strong result; a non-zero
    /// is a site with a method and a bci.
    pub static VERIFIER_OOP: AtomicU64 = AtomicU64::new(0);
    /// `never_mapped` hits the verifier REFUTES: primitive, or out of scope at
    /// this bci. The false positives, now subtractable rather than merely
    /// declared.
    pub static VERIFIER_NOT_OOP: AtomicU64 = AtomicU64::new(0);
    /// `never_mapped` hits with no independent answer -- an inlined frame, a
    /// slot outside the java locals, a pc with no row, or a name the index
    /// cannot resolve. Counted rather than folded into either verdict: a
    /// backstop whose "no gap" silently includes "could not look" is the
    /// vacuous green this exists to avoid. See `VerifierSlotVerdict`.
    pub static VERIFIER_UNKNOWN: AtomicU64 = AtomicU64::new(0);

    /// Distinct `(method entry_ptr, slot offset)` pairs reported as
    /// never-mapped, with the storage class of the slot.
    ///
    /// The raw counter double-counts in two ways, both of which made the first
    /// run unreadable: the audit runs once per CHAIN ENTRY and every entry
    /// re-walks the same parent frames, and a workload takes many collections
    /// at the same stack shape. What the question needs is "which METHOD has
    /// which unmapped SLOT", which is what this records.
    pub static SITES: std::sync::Mutex<Option<std::collections::BTreeMap<(usize, i32), SiteInfo>>> =
        std::sync::Mutex::new(None);

    /// What a never-mapped site needs to be ACTED on rather than counted.
    ///
    /// A bare `code=0x781daa08b000 rbp-0x58 operand-spill` cannot be checked by
    /// anybody: settling whether the slot is genuinely live or merely stale
    /// means reading that method's `LocalVariableTable` scopes against the
    /// safepoint's bci, and for that you need the METHOD and the BCI. Both are
    /// in hand at the recording site and neither was kept.
    #[derive(Clone)]
    pub struct SiteInfo {
        pub class: &'static str,
        pub method: String,
        /// The active safepoint id (bytecode pc) the frame was standing on.
        pub sp_id: u32,
        /// The frame's `fully_oop_covered` (frame-slot notion).
        pub oop_covered: bool,
        /// The frame's `fully_shadow_covered` — the claim the moving path
        /// spends, and the one a never-mapped live oop would refute.
        pub shadow_covered: bool,
    }

    pub fn note_site(
        entry_ptr: usize,
        off: i32,
        class: &'static str,
        method: &str,
        sp_id: u32,
        oop_covered: bool,
        shadow_covered: bool,
    ) {
        if let Ok(mut g) = SITES.lock() {
            g.get_or_insert_with(Default::default).insert(
                (entry_ptr, off),
                SiteInfo {
                    class,
                    method: method.to_string(),
                    sp_id,
                    oop_covered,
                    shadow_covered,
                },
            );
        }
    }

    pub fn dump() {
        if FRAMES.load(Ordering::Relaxed) == 0 && WORDS.load(Ordering::Relaxed) == 0 {
            return;
        }
        if let Ok(g) = SITES.lock() {
            if let Some(map) = g.as_ref() {
                let mut by_class: std::collections::BTreeMap<&'static str, usize> =
                    Default::default();
                for info in map.values() {
                    *by_class.entry(info.class).or_default() += 1;
                }
                eprintln!(
                    "[cratonvm] oop-map audit: DISTINCT never-mapped sites={} by_class={:?}",
                    map.len(),
                    by_class
                );
                for ((ptr, off), info) in map.iter().take(32) {
                    eprintln!(
                        "[cratonvm] oop-map audit:   code={ptr:#x} rbp-{off:#x} {} \
                         sp_id={} oop_cov={} shadow_cov={} {}",
                        info.class, info.sp_id, info.oop_covered, info.shadow_covered, info.method
                    );
                }
            }
        }
        eprintln!(
            "[cratonvm] oop-map audit: frames={} unreadable_frames={} words={} \
             never_mapped={} (while_covered={} of {} claiming; \
             while_shadow_covered={} of {} claiming) wrong_map={}              (verifier_oop={} other={}) below_jit={}",
            FRAMES.load(Ordering::Relaxed),
            UNREADABLE_FRAMES.load(Ordering::Relaxed),
            WORDS.load(Ordering::Relaxed),
            NEVER_MAPPED.load(Ordering::Relaxed),
            NEVER_MAPPED_WHILE_COVERED.load(Ordering::Relaxed),
            FRAMES_CLAIMING_OOP_COVERAGE.load(Ordering::Relaxed),
            NEVER_MAPPED_WHILE_SHADOW_COVERED.load(Ordering::Relaxed),
            FRAMES_CLAIMING_SHADOW_COVERAGE.load(Ordering::Relaxed),
            WRONG_MAP.load(Ordering::Relaxed),
            WRONG_MAP_VERIFIER_OOP.load(Ordering::Relaxed),
            WRONG_MAP_VERIFIER_OTHER.load(Ordering::Relaxed),
            BELOW_JIT.load(Ordering::Relaxed),
        );
        // THE LINE TO READ FIRST. Everything above counts words that LOOK like
        // references; this splits them by what the class file itself says, and
        // only `verifier_oop` is a lead. `name_index` is printed because a zero
        // there means the resolver was empty and every verdict is `unknown` for
        // a reason that has nothing to do with the maps.
        let (names, name_collisions) = cratonvm_classloading::name_index_shape();
        eprintln!(
            "[cratonvm] oop-map audit: verifier_oop={} verifier_not_oop={} \
             verifier_unknown={} name_index=({names} names, {name_collisions} collisions)",
            VERIFIER_OOP.load(Ordering::Relaxed),
            VERIFIER_NOT_OOP.load(Ordering::Relaxed),
            VERIFIER_UNKNOWN.load(Ordering::Relaxed),
        );
    }
}

/// Which storage class of the compiled frame the slot at `[rbp - off]` is in.
///
/// This is what turns a never-mapped hit from a number into a verdict. The
/// abstract model behind `moving_young_coverage_complete` describes Java locals
/// and the operand stack; the module block on
/// `refresh_moving_young_coverage_for_current_thread` names the three storage
/// classes it does NOT describe — scalar-replacement field slots, LICM hoist
/// slots, and the full-GPR safepoint spill area. A hit in one of those is the
/// PREDICTED defect. A hit in a Java local is either a genuine miss of what the
/// model does claim, or this oracle's known false positive (a primitive whose
/// bits land on a live object header).
fn classify_frame_slot(off: i32, layout: &cratonvm_jit::FrameLayout) -> &'static str {
    let within = |lo: i32, hi: i32| hi > lo && off >= lo && off < hi;
    if within(layout.scalar_lo, layout.scalar_hi) {
        "scalar-replacement-field"
    } else if within(layout.ref_hoist_lo, layout.ref_hoist_hi) {
        "licm-ref-hoist"
    } else if within(layout.arith_lo, layout.arith_hi) {
        "licm-arith-hoist"
    } else if within(layout.reg_spill_lo, layout.reg_spill_hi) {
        "gpr-safepoint-spill"
    } else if layout.java_locals_hi > 0 && off <= layout.java_locals_hi {
        "java-local"
    } else if within(layout.spill_lo, layout.spill_hi) {
        "operand-spill"
    } else if layout.locals_hi > 0 && off <= layout.locals_hi {
        "reserved-locals-tail"
    } else {
        "other"
    }
}

/// The oop-map slot offsets the ACTIVE safepoint selects for the frame at
/// `rbp` — the exact set `scan_active_oop_map_at_rbp` would publish.
///
/// `None` means the id or its map could not be read, which is a different
/// finding from "the map is empty" and is counted separately.
fn active_map_slots(rbp: usize, cm: &cratonvm_jit::CompiledMethod) -> Option<Vec<i16>> {
    let sp_id_slot_off = cm.sp_id_slot_off;
    if sp_id_slot_off <= 0 || rbp < sp_id_slot_off as usize {
        return None;
    }
    let id_addr = rbp - sp_id_slot_off as usize;
    if id_addr & 0x7 != 0 {
        return None;
    }
    // SAFETY: the safepoint id lives in the validated frame's reserved local
    // slot, exactly as `scan_active_oop_map_at_rbp` reads it.
    let safepoint_id = unsafe { (id_addr as *const usize).read() } as u32;
    let map = cm.find_oop_map_for_safepoint_id(safepoint_id)?;
    Some(map.frame_slot_offsets.clone())
}

/// `CRATONVM_DBG_VERIFY_OOP_MAPS` — does the precise oop map actually cover
/// every live reference in the compiled frames it claims?
///
/// # What the previous version measured, and why it was an upper bound
///
/// It built its `mapped` set from ONE method's maps at ONE frame base
/// (`info.exact_rbp`) and then scanned the WHOLE band `[scanner_sp,
/// frame_base)`. That band spans every compiled frame in the chain plus the
/// interpreter, native and Rust frames the compiled code called into, so:
///
///   * a nested JIT callee's slots, correctly named by the CALLEE's own map at
///     the CALLEE's own rbp, counted as unmapped;
///   * every interpreter/native word counted as unmapped, though no oop map has
///     ever claimed to describe those — they are published by their own root
///     mechanisms;
///   * register images and dead spill slots counted as unmapped, which the band
///     verifier next door already documents as a false verdict "on every
///     collection".
///
/// It reported 64 hits on the `PolynomialTest` failure and its own header
/// admitted the ambiguity ("NB band may include nested-JIT-callee slots"), so
/// the number could not be used to say whether the codegen bit was sound.
///
/// # What this measures
///
/// The RBP chain is walked exactly as `scan_compiled_frame_bands` walks it, and
/// each frame is checked against ITS OWN method's maps at ITS OWN rbp, over its
/// own band only, skipping the slots `band_slot_is_verifiable` excludes. Every
/// in-band object address is then classified:
///
///   * `never_mapped` — named by no map of the owning frame. A live reference
///     the collector would neither publish nor rewrite. This is the number that
///     answers whether the coverage bit is sound, and `never_mapped_while_covered`
///     is the subset where the frame's `fully_oop_covered` asserted otherwise.
///   * `wrong_map` — named by some map of the frame but not the one its
///     safepoint id selects. Not a codegen coverage gap; a selection gap.
///   * `below_jit` — below the innermost frame. The interpreter/native region,
///     reported so it can be subtracted rather than counted as a gap.
///
/// Still conservative in ONE direction, deliberately: a primitive `i64` whose
/// bits land on a live object header is counted. So a non-zero `never_mapped`
/// is a lead, and a ZERO is the strong result — it says no verifiable in-band
/// word went unnamed.
fn verify_precise_covers_conservative(
    info: PreciseFrameInfo,
    cm: &cratonvm_jit::CompiledMethod,
    heap: &VmHeap,
) {
    use oop_map_audit as audit;
    use std::sync::atomic::Ordering as AOrd;

    let scanner_sp = current_stack_pointer();
    let entry_sp = info.frame_base;
    let mut rbp = info.exact_rbp;
    if rbp == 0 || rbp & 0x7 != 0 || rbp < scanner_sp || rbp >= entry_sp {
        audit::UNREADABLE_FRAMES.fetch_add(1, AOrd::Relaxed);
        return;
    }
    let Some(innermost) = innermost_frame_method(
        rbp,
        info.exact_cm_id,
        entry_sp,
        scanner_sp,
        info.compiled_method,
    ) else {
        // Nothing describes the frame at `exact_rbp`; every offset would be
        // read against the wrong method. Same fail-closed stance as the band
        // verifier.
        audit::UNREADABLE_FRAMES.fetch_add(1, AOrd::Relaxed);
        return;
    };
    // SAFETY: as in `scan_compiled_frame_bands` — a chain entry's method is
    // Arc-owned while any of its frames is live, and a resolved callee is kept
    // alive by the live frame whose return address resolved it.
    let mut frame_cm: &cratonvm_jit::CompiledMethod = unsafe { &*innermost };
    let _ = cm;

    let mut lowest_band_lo = usize::MAX;
    let mut frames = 0usize;
    while frames < 4096 {
        frames += 1;
        let frame_size = frame_cm.osr_frame_size;
        if frame_size <= 0 {
            audit::UNREADABLE_FRAMES.fetch_add(1, AOrd::Relaxed);
            break;
        }
        let frame_size = frame_size as usize;
        const MAX_COMPILED_FRAME_BYTES: usize = 1024 * 1024;
        if frame_size > MAX_COMPILED_FRAME_BYTES || frame_size > rbp {
            audit::UNREADABLE_FRAMES.fetch_add(1, AOrd::Relaxed);
            break;
        }
        let band_lo = rbp - frame_size;
        lowest_band_lo = lowest_band_lo.min(band_lo);

        // Engagement, counted per FRAME INSPECTED rather than per hit, so a zero
        // refutation above can be told from "the oracle never inspected a frame
        // that made the claim". Counted here — after the frame is known
        // readable, before its words are walked — so the denominator is exactly
        // the frames the refutation counters could have fired on.
        if frame_cm.fully_oop_covered {
            audit::FRAMES_CLAIMING_OOP_COVERAGE.fetch_add(1, AOrd::Relaxed);
        }
        if frame_cm.fully_shadow_covered {
            audit::FRAMES_CLAIMING_SHADOW_COVERAGE.fetch_add(1, AOrd::Relaxed);
        }

        // The bci this frame is standing on, so a reported site can be checked
        // against the method's LocalVariableTable scopes.
        let active_sp_id = frame_active_sp_id(rbp, frame_cm).unwrap_or(u32::MAX);
        let active: std::collections::HashSet<i16> = match active_map_slots(rbp, frame_cm) {
            Some(v) => v.into_iter().collect(),
            None => {
                audit::UNREADABLE_FRAMES.fetch_add(1, AOrd::Relaxed);
                std::collections::HashSet::new()
            }
        };
        let any: std::collections::HashSet<i16> = frame_cm
            .oop_maps
            .iter()
            .flat_map(|m| m.frame_slot_offsets.iter().copied())
            .collect();

        audit::FRAMES.fetch_add(1, AOrd::Relaxed);
        let live_hi = moving_young_frame_live_hi(rbp, frame_cm);
        let mut off = 8i32;
        while (off as usize) <= frame_size {
            if !band_slot_is_verifiable(off, &frame_cm.frame_layout, live_hi) {
                off += 8;
                continue;
            }
            let addr = rbp - off as usize;
            if addr & 0x7 == 0 {
                audit::WORDS.fetch_add(1, AOrd::Relaxed);
                // SAFETY: `addr` is an 8-aligned address inside this thread's
                // own live compiled frame band, the same region
                // `scan_one_frame` reads.
                let qword = unsafe { (addr as *const usize).read() };
                if heap.is_object_address(qword).is_some() {
                    let off16 = i16::try_from(off).unwrap_or(i16::MAX);
                    if active.contains(&off16) {
                        // covered by the map the collector will actually scan
                    } else if any.contains(&off16) {
                        audit::WRONG_MAP.fetch_add(1, AOrd::Relaxed);
                        // Split it the way NEVER_MAPPED is split. Without this
                        // the map-selection population has no verdict at all,
                        // and a raw non-zero reading cannot distinguish a live
                        // reference the selected map omits from an old pointer
                        // lying dead in a reusable slot.
                        match verifier_local_verdict(frame_cm, off, active_sp_id) {
                            VerifierSlotVerdict::Oop => {
                                audit::WRONG_MAP_VERIFIER_OOP.fetch_add(1, AOrd::Relaxed)
                            }
                            _ => audit::WRONG_MAP_VERIFIER_OTHER.fetch_add(1, AOrd::Relaxed),
                        };
                    } else {
                        audit::NEVER_MAPPED.fetch_add(1, AOrd::Relaxed);
                        let class = classify_frame_slot(off, &frame_cm.frame_layout);
                        // WHAT THE CLASS FILE ITSELF SAYS about this slot at
                        // this bci -- the independent answer that separates a
                        // real coverage gap from the two false positives the
                        // raw counter cannot subtract. See
                        // `VerifierSlotVerdict`.
                        let verdict = verifier_local_verdict(frame_cm, off, active_sp_id);
                        match verdict {
                            VerifierSlotVerdict::Oop => {
                                audit::VERIFIER_OOP.fetch_add(1, AOrd::Relaxed)
                            }
                            VerifierSlotVerdict::NotOop => {
                                audit::VERIFIER_NOT_OOP.fetch_add(1, AOrd::Relaxed)
                            }
                            VerifierSlotVerdict::Unknown => {
                                audit::VERIFIER_UNKNOWN.fetch_add(1, AOrd::Relaxed)
                            }
                        };
                        // Only a frame that ASSERTS full coverage is evidence about
                        // the codegen bit. A map-less frame (`maps=0 covered=false`)
                        // is already reported by the NO_PRECISE_MAP obligation and
                        // says nothing here -- the first run's probe arm was almost
                        // entirely those.
                        audit::note_site(
                            frame_cm.entry_ptr() as usize,
                            off,
                            class,
                            &frame_cm.method_label,
                            active_sp_id,
                            frame_cm.fully_oop_covered,
                            frame_cm.fully_shadow_covered,
                        );
                        if frame_cm.fully_shadow_covered {
                            audit::NEVER_MAPPED_WHILE_SHADOW_COVERED.fetch_add(1, AOrd::Relaxed);
                        }
                        if frame_cm.fully_oop_covered {
                            audit::NEVER_MAPPED_WHILE_COVERED.fetch_add(1, AOrd::Relaxed);
                            // The bit has been caught claiming coverage it does
                            // not have -- but only if the word really is an oop.
                            // Latched, and `collect_roots` consults the latch
                            // before skipping the conservative backstop.
                            //
                            // GATED ON THE VERIFIER, for exactly the reason the
                            // shadow latch below already gives: the raw counter
                            // flags any in-band word that LOOKS like a heap
                            // address, and an old pointer left in a reusable
                            // slot after its value died looks exactly like one.
                            // The two latches sat side by side with only the
                            // second one gated, and the first one's cost was
                            // measured on 2026-09-03: H2
                            // (`org.h2.test.store.TestMVStoreTool`, 612
                            // compiled frames) trips it 400 times with
                            // `verifier_oop=0` on every one, and because the
                            // latch is process-wide the run then reports
                            // `root coverage: incomplete` on 100% of pauses --
                            // where the same class without the oracle reports
                            // 0.00%. A refusal that fires on shape rather than
                            // evidence, and it fired on every real workload.
                            if verdict == VerifierSlotVerdict::Oop {
                                note_coverage_oracle_refutation();
                            }
                        }
                        // ...and the same latch on the claim the MOVING path
                        // actually spends, but ONLY on a corroborated hit.
                        //
                        // `fully_shadow_covered` is the bit the per-frame proof
                        // is built from, so it is the one an unmapped live oop
                        // refutes -- but latching on the raw counter would have
                        // suppressed every collection on every workload
                        // measured, since 5-6% of in-band words trip it and
                        // `TestMultiThread` passes with 670 k of them. Gated on
                        // the verifier's own verdict, this is a refusal that
                        // fires on evidence rather than on shape.
                        if verdict == VerifierSlotVerdict::Oop && frame_cm.fully_shadow_covered {
                            note_coverage_oracle_refutation();
                        }
                        if STEP3_LOG_COUNT.fetch_add(1, AOrd::Relaxed) < STEP3_LOG_CAP {
                            eprintln!(
                                "[VERIFY-OOP-MAPS] NEVER-MAPPED in-band oop: code@{:p} \
                                 rbp={rbp:#x} slot=[rbp-{off:#x}] class={class} \
                                 addr={addr:#x} \
                                 val={qword:#x} frame_size={frame_size} maps={} covered={}",
                                frame_cm.entry_ptr(),
                                frame_cm.oop_maps.len(),
                                frame_cm.fully_oop_covered,
                            );
                        }
                    }
                }
            }
            off += 8;
        }

        // `[rbp]` / `[rbp + 8]` are the saved caller RBP and return PC; a
        // non-JIT parent ends the walk.
        let parent_rbp = unsafe { (rbp as *const usize).read() };
        let ret_addr = unsafe { ((rbp + 8) as *const usize).read() };
        let Some(parent_cm_ptr) = cratonvm_jit::lookup_jit_code_range(ret_addr) else {
            break;
        };
        if parent_rbp <= rbp
            || parent_rbp & 0x7 != 0
            || parent_rbp >= entry_sp
            || parent_rbp < scanner_sp
        {
            audit::UNREADABLE_FRAMES.fetch_add(1, AOrd::Relaxed);
            break;
        }
        frame_cm = unsafe { &*(parent_cm_ptr as *const cratonvm_jit::CompiledMethod) };
        rbp = parent_rbp;
    }

    // Everything below the innermost compiled band is interpreter / native /
    // Rust. Counted, never blamed on an oop map.
    if lowest_band_lo != usize::MAX && lowest_band_lo > scanner_sp {
        let mut a = (scanner_sp + 7) & !7usize;
        while a + 8 <= lowest_band_lo {
            // SAFETY: 8-aligned address in this thread's own live stack band.
            let qword = unsafe { (a as *const usize).read() };
            if heap.is_object_address(qword).is_some() {
                audit::BELOW_JIT.fetch_add(1, AOrd::Relaxed);
            }
            a += 8;
        }
    }
}

fn scan_one_frame_precise(info: PreciseFrameInfo, heap: &VmHeap, out: &mut Vec<ObjectRef>) {
    // SAFETY: `info.compiled_method` was populated from a live
    // `&CompiledMethod` at push time, and the chain is popped before
    // the borrow ends. The JIT cache also keeps the CompiledMethod
    // alive via Arc for the duration of the call. Reading through
    // the pointer is valid for the lifetime of this function.
    let cm: &cratonvm_jit::CompiledMethod = unsafe { &*info.compiled_method };

    // Step 3 (precise-jit-maps-default.md) — coverage visibility + completeness
    // oracle. Both gates default-OFF; off → two cached-bool branches and the
    // scan below is unchanged.
    if coverage_pin_enabled() && !cm.fully_oop_covered {
        let n = UNCOVERED_PRECISE_FRAMES.fetch_add(1, Ordering::Relaxed);
        if n < STEP3_LOG_CAP {
            eprintln!(
                "[COVERAGE-PIN] precise frame NOT fully_oop_covered (pinned via backstop): \
                 code@{:p} maps={}",
                info.entry_ptr,
                cm.oop_maps.len(),
            );
        }
    }
    if verify_oop_maps_enabled() {
        verify_precise_covers_conservative(info, cm, heap);
    }

    // A GC runs inside a helper, so its instruction pointer cannot identify
    // the suspended JIT caller. The emitter therefore writes the bytecode PC
    // of the active safepoint into a dedicated `[rbp - sp_id_slot_off]` slot
    // before every GC-capable call. Use that id to select ONE exact map for the
    // innermost JIT frame, then follow the standard saved-RBP/return-address
    // chain to select the exact map for each compiled caller as well. The old
    // implementation scanned the union of every map in only the boundary
    // method; besides retaining dead oops, it omitted maps belonging to nested
    // JIT callers entirely.
    let scanner_sp = current_stack_pointer();
    if info.exact_rbp != 0
        && info.exact_rbp & 0x7 == 0
        && info.exact_rbp >= scanner_sp
        && info.exact_rbp < info.frame_base
    {
        // `cm` describes the frame this chain entry was pushed for. It does NOT
        // describe `exact_rbp` when an unguarded JIT->JIT direct call has since
        // published a deeper frame base there (see
        // `chain_entry_rbp_is_foreign`); reading a safepoint id out of that
        // frame at this method's offset and publishing this method's slot list
        // would leave the callee's real oops unmarked, and the non-moving sweep
        // then reclaims them while they are live. The parent walk below is
        // unaffected: it resolves every ancestor from its own return address,
        // so the true caller is still scanned precisely, and
        // `scan_compiled_frame_bands` covers the unidentified frame
        // conservatively.
        if let Some(innermost_cm) = innermost_frame_method(
            info.exact_rbp,
            info.exact_cm_id,
            info.frame_base,
            scanner_sp,
            info.compiled_method,
        ) {
            // Publish the innermost frame's map under the method that actually
            // describes it: the entry's own when the frame is the boundary or a
            // direct self-call, the resolved callee when a JIT->JIT direct call
            // published a deeper base here. An indirect callee resolves to
            // `None` and is left to the conservative band below.
            // SAFETY: as for the parent walk that follows — a code range retains
            // its CompiledMethod metadata for the lifetime of an active frame.
            let innermost_cm: &cratonvm_jit::CompiledMethod = unsafe { &*innermost_cm };
            scan_active_oop_map_at_rbp(info.exact_rbp, innermost_cm, heap, out);
            if pin_unnamed_frame_refs_enabled() {
                pin_unnamed_frame_refs(info.exact_rbp, innermost_cm, heap);
            }
        }

        let mut child_rbp = info.exact_rbp;
        let mut guard = 0usize;
        while guard < 4096 {
            guard += 1;
            // SAFETY: `child_rbp` is an aligned address in this thread's live
            // JIT stack interval. The frame prologue establishes `[rbp]` as the
            // caller RBP and `[rbp + 8]` as its return address.
            let parent_rbp = unsafe { (child_rbp as *const usize).read() };
            let ret_addr = unsafe { ((child_rbp + 8) as *const usize).read() };
            let Some(cm_ptr) = cratonvm_jit::lookup_jit_code_range(ret_addr) else {
                break;
            };
            if parent_rbp <= child_rbp
                || parent_rbp & 0x7 != 0
                || parent_rbp < scanner_sp
                || parent_rbp >= info.frame_base
            {
                break;
            }
            // SAFETY: code ranges retain their CompiledMethod metadata for the
            // lifetime of an active frame (the same contract as the relocation
            // walker immediately above in this module).
            let parent_cm: &cratonvm_jit::CompiledMethod =
                unsafe { &*(cm_ptr as *const cratonvm_jit::CompiledMethod) };
            scan_active_oop_map_at_rbp(parent_rbp, parent_cm, heap, out);
            if pin_unnamed_frame_refs_enabled() {
                pin_unnamed_frame_refs(parent_rbp, parent_cm, heap);
            }
            child_rbp = parent_rbp;
        }
    }
    // T1.1.a — Conservative sweep between the scanner's current SP and
    // the captured frame base to cover any oop living in a spill slot
    // not listed in any oop map. This is the correctness backstop
    // during the staged rollout of per-PC map population: the JIT
    // compiler populates maps at well-known safepoints (new,
    // anewarray, newarray, aaload, aload*) but may emit intermediate
    // spills between them; the sweep catches those. The
    // `heap.is_object_address` validation filters non-oop values so
    // false positives are harmless.
    // A JIT entry can remain live while it calls deeply into the interpreter.
    // The legacy whole-band sweep then revalidated those interpreter/Rust
    // frames on every native call even though their roots are published by
    // their own mechanisms. Prefer the bounded JIT frame bands recovered from
    // the live RBP chain; retain the historical sweep as a fail-safe whenever
    // a frame record or its size metadata is not trustworthy.
    // `CRATONVM_JIT_NO_FRAME_BANDS=1` restores the historical whole-band sweep
    // so the coverage difference between the two is an A/B inside ONE binary.
    // The bands cover each compiled frame's own `[rbp - frame_size, rbp)` and
    // nothing else; the whole-band sweep also covers `[scanner_sp, rbp_inner)`,
    // i.e. the interpreter / native / Rust frames the compiled method called
    // INTO. The narrowing rests on "their roots are published by their own
    // mechanisms", which does not hold for an object that has been allocated
    // and not yet stored anywhere tracked — see
    // `bug-g1-evacuates-live-jit-reference-20260819-FIXED.md`.
    if !frame_bands_enabled() || !scan_compiled_frame_bands(info, scanner_sp, heap, out) {
        scan_one_frame(scanner_sp, info.frame_base, heap, out);
    }
    let _ = info.entry_ptr; // reserved for future PC-precise lookup
}

/// Conservatively scan only the live compiled-frame spill bands reachable from
/// `info.exact_rbp`. Returns `false` when metadata is insufficient, allowing
/// the caller to keep the existing whole-band fallback.
///
/// A compiled x64 frame owns `[rbp - osr_frame_size, rbp)`. The field's name
/// is historical: normal compiled entries populate it too. Nested direct JIT
/// calls use the normal saved-RBP chain, so this excludes intervening
/// interpreter/Rust frames without excluding JIT spill space.
/// `CRATONVM_JIT_NO_FRAME_BANDS` kill switch for the bounded-band scan, so the
/// whole-band sweep it replaced can be reinstated in the same binary.
fn frame_bands_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        cratonvm_types::flags::runtime_var_os("CRATONVM_JIT_NO_FRAME_BANDS").is_none()
    })
}

fn scan_compiled_frame_bands(
    info: PreciseFrameInfo,
    scanner_sp: usize,
    heap: &VmHeap,
    out: &mut Vec<ObjectRef>,
) -> bool {
    let entry_sp = info.frame_base;
    let mut rbp = info.exact_rbp;
    if rbp == 0 || rbp & 0x7 != 0 || rbp < scanner_sp || rbp >= entry_sp {
        return false;
    }

    // The innermost frame is the method retained by the entry guard — UNLESS an
    // unguarded JIT->JIT direct call published a deeper frame base into
    // `exact_rbp`, in which case that method is unknown here and its frame size
    // must not be taken from the entry's method. Bound its band by the
    // scanner's own SP instead: the collector runs beneath that frame, so
    // `[scanner_sp, rbp)` covers all of it and nothing above it. Parent frames
    // are identified through the child frame's return address either way.
    let innermost = innermost_frame_method(
        rbp,
        info.exact_cm_id,
        entry_sp,
        scanner_sp,
        info.compiled_method,
    );
    let mut innermost_is_foreign = innermost.is_none();
    // SAFETY: the chain entry's pointer is Arc-owned by the JIT cache while any
    // of its frames is live; a resolved callee is kept alive by the live frame
    // whose return address resolved it. When resolution failed the pointer is
    // unused — `innermost_is_foreign` bounds that frame by `scanner_sp` instead
    // of by any method's frame size.
    let mut cm: &cratonvm_jit::CompiledMethod =
        unsafe { &*innermost.unwrap_or(info.compiled_method) };
    let mut frames = 0usize;
    while frames < 4096 {
        frames += 1;
        if innermost_is_foreign {
            innermost_is_foreign = false;
            scan_one_frame(scanner_sp, rbp, heap, out);
        } else {
            let frame_size = cm.osr_frame_size;
            if frame_size <= 0 {
                return false;
            }
            let frame_size = frame_size as usize;
            const MAX_COMPILED_FRAME_BYTES: usize = 1024 * 1024;
            if frame_size > MAX_COMPILED_FRAME_BYTES || frame_size > rbp {
                return false;
            }
            scan_one_frame(rbp - frame_size, rbp, heap, out);
            // The band was just read as marking roots, which is what keeps
            // these objects alive across the pause AND what gets them copied.
            // Say which of them arrived through a word no channel rewrites, so
            // the pin decision can veto a movable claim made elsewhere for the
            // same address. Only reachable with a resolved layout — the foreign
            // innermost frame above has none, and it already forces the
            // non-moving sweep through `FOREIGN_INNERMOST_RBP`, so there is no
            // move to veto there.
            publish_unrewritable_band_roots(rbp, frame_size, cm, heap);
        }

        // `[rbp]` and `[rbp + 8]` hold the saved caller RBP and return PC.
        // A non-JIT parent ends the successful walk: it has no JIT spill band.
        let parent_rbp = unsafe { (rbp as *const usize).read() };
        let ret_addr = unsafe { ((rbp + 8) as *const usize).read() };
        let Some(parent_cm_ptr) = cratonvm_jit::lookup_jit_code_range(ret_addr) else {
            return true;
        };
        if parent_rbp <= rbp
            || parent_rbp & 0x7 != 0
            || parent_rbp >= entry_sp
            || parent_rbp < scanner_sp
        {
            return false;
        }
        cm = unsafe { &*(parent_cm_ptr as *const cratonvm_jit::CompiledMethod) };
        rbp = parent_rbp;
    }
    false
}

/// Scan the one oop map selected by a live frame's safepoint-id slot.
/// `rbp` must be the exact frame base established by the compiled prologue.
/// A missing id or map deliberately scans nothing here: the caller's
/// conservative compatibility backstop remains responsible for legacy and
/// uncovered frames.
/// `CRATONVM_JIT_PIN_UNNAMED_FRAME_REFS=1` -- pin every object a live compiled
/// frame names in a slot its oop maps do NOT name.
///
/// The defect this closes, measured 2026-09-03 on
/// `TestCachedQueryResults` with `CRATONVM_DBG_STALE_FRAME_WORDS=1`: one
/// reference lives in SEVERAL slots of a frame at once -- a GPR safepoint
/// spill, an operand spill and two below the locals boundary all held
/// `0x1fef6b70878` -- while the map names only the canonical home. Relocation
/// rewrites that home and every duplicate keeps pointing at the vacated page.
/// Reading one is a use-after-free. It is not, as this comment said until
/// 2026-09-05, the SIGSEGV of the retired
/// `bug-box-unbox-intrinsic-segv-under-relocation-20260902`: that one was a
/// WRITE by the slide's own `ptr::copy` into a decommitted granule, and
/// rewriting these duplicates did not move it.
///
/// Map SELECTION is not the problem and was ruled out first: `NO_MAP_FOR_SP_ID`
/// and `NO_SP_ID_SLOT` are both zero on that workload, and dev's
/// precise-oop-maps-for->64-locals fix changes nothing. The maps are found and
/// correct; they simply do not enumerate every copy the register allocator made.
///
/// Why PIN rather than rewrite the duplicates. Rewriting every frame word whose
/// value equals a moved object's base is a conservative WRITE: a primitive that
/// happens to hold that bit pattern would be silently corrupted, which is worse
/// than the crash it fixes. Pinning is conservative in the safe direction -- the
/// object does not move, so every copy of the reference stays valid, named or
/// not. It costs compaction, not correctness.
pub fn pin_unnamed_frame_refs_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        cratonvm_types::flags::runtime_var_os("CRATONVM_JIT_PIN_UNNAMED_FRAME_REFS").is_some()
    })
}

pub mod unnamed_ref_pins {
    use std::sync::atomic::AtomicU64;
    /// Frames scanned. The denominator: `PINNED=0` with `FRAMES=0` means the
    /// pass never ran, not that there was nothing to pin.
    pub static FRAMES: AtomicU64 = AtomicU64::new(0);
    /// Objects pinned because a frame named them in an unmapped slot.
    pub static PINNED: AtomicU64 = AtomicU64::new(0);
    /// Of those, ones ALSO reachable from a slot the map does name -- i.e. a
    /// genuine duplicate home, the shape this exists for.
    pub static DUPLICATE_OF_MAPPED: AtomicU64 = AtomicU64::new(0);
}

/// Pin every object this frame names in a slot its maps do not.
///
/// Selection matches `remap_one_jit_frame` exactly -- the UNION of every map
/// whose `bytecode_pc` equals the frame's stamped safepoint id, not the first
/// one -- because a slot the remap WILL rewrite must not be pinned, and one it
/// will not must be.
fn pin_unnamed_frame_refs(rbp: usize, cm: &cratonvm_jit::CompiledMethod, heap: &VmHeap) {
    use std::sync::atomic::Ordering as AOrd;
    let sp_id_off = cm.sp_id_slot_off;
    let frame_size = cm.osr_frame_size;
    if sp_id_off <= 0 || frame_size <= 0 || rbp < sp_id_off as usize {
        return;
    }
    let frame_size = frame_size as usize;
    const MAX_COMPILED_FRAME_BYTES: usize = 1024 * 1024;
    if frame_size > MAX_COMPILED_FRAME_BYTES || frame_size > rbp {
        return;
    }
    let id_addr = rbp - sp_id_off as usize;
    if id_addr & 0x7 != 0 {
        return;
    }
    // SAFETY: the safepoint id lives in the frame's reserved slot, written
    // before the helper call that can trigger this scan.
    let sp_id = unsafe { (id_addr as *const usize).read() } as u32;

    // The offsets the remap will rewrite. Same 64-entry bound and same union
    // as `remap_one_jit_frame`; past the bound we pin conservatively, which is
    // the safe direction (an over-pin costs compaction).
    let mut named: [i16; 64] = [0; 64];
    let mut named_len = 0usize;
    for map in cm.oop_maps.iter().filter(|m| m.bytecode_pc == sp_id) {
        for &off in &map.frame_slot_offsets {
            if named_len < named.len() && !named[..named_len].contains(&off) {
                named[named_len] = off;
                named_len += 1;
            }
        }
    }

    // The values the map DOES name, so a duplicate can be told from a lone
    // unmapped reference. Diagnostic only -- both get pinned.
    let mut mapped_vals: [usize; 64] = [0; 64];
    let mut mapped_len = 0usize;
    for &off in named[..named_len].iter() {
        if off <= 0 || rbp < off as usize {
            continue;
        }
        let a = rbp - off as usize;
        if a & 0x7 != 0 {
            continue;
        }
        // SAFETY: aligned slot inside this thread's live frame.
        let v = unsafe { (a as *const usize).read() };
        if mapped_len < mapped_vals.len() {
            mapped_vals[mapped_len] = v;
            mapped_len += 1;
        }
    }

    unnamed_ref_pins::FRAMES.fetch_add(1, AOrd::Relaxed);
    let lo = rbp - frame_size;
    let mut a = (lo + 7) & !7usize;
    let mut pins: Vec<usize> = Vec::new();
    while a <= rbp {
        let off = (rbp - a) as i64;
        let is_named = off > 0
            && off <= i16::MAX as i64
            && named[..named_len].contains(&(off as i16));
        if !is_named {
            // SAFETY: aligned word inside this thread's live compiled frame,
            // bounded by the recorded frame size.
            let v = unsafe { (a as *const usize).read() };
            if heap.is_object_address(v).is_some() {
                pins.push(v);
                unnamed_ref_pins::PINNED.fetch_add(1, AOrd::Relaxed);
                if mapped_vals[..mapped_len].contains(&v) {
                    unnamed_ref_pins::DUPLICATE_OF_MAPPED.fetch_add(1, AOrd::Relaxed);
                }
            }
        }
        a += 8;
    }
    if !pins.is_empty() {
        cratonvm_gc::gc_quiescence::add_xt_cycle_pinned_jit_roots(&pins);
    }
}

fn scan_active_oop_map_at_rbp(
    rbp: usize,
    cm: &cratonvm_jit::CompiledMethod,
    heap: &VmHeap,
    out: &mut Vec<ObjectRef>,
) {
    let sp_id_slot_off = cm.sp_id_slot_off;
    if sp_id_slot_off <= 0 || rbp < sp_id_slot_off as usize {
        return;
    }
    let id_addr = rbp - sp_id_slot_off as usize;
    if id_addr & 0x7 != 0 {
        return;
    }
    // SAFETY: the safepoint id lives in the validated frame's reserved local
    // slot. It is written before the helper call that can trigger this scan.
    let safepoint_id = unsafe { (id_addr as *const usize).read() } as u32;
    let Some(map) = cm.find_oop_map_for_safepoint_id(safepoint_id) else {
        return;
    };
    scan_oop_slots(rbp, &map.frame_slot_offsets, heap, out);
}

/// Read each oop slot listed in `slot_offsets` (positive byte distances below
/// `rbp`), validate via `heap.is_object_address`, and push any hit into `out`.
/// Used by [`scan_one_frame_precise`].
fn scan_oop_slots(rbp: usize, slot_offsets: &[i16], heap: &VmHeap, out: &mut Vec<ObjectRef>) {
    for &offset in slot_offsets {
        // x64 map entries are positive distances from RBP to slots in the
        // downward-growing local/spill area: `off` means `[rbp - off]`.
        // Reject malformed zero/negative entries rather than ever reading a
        // saved RBP or return address as an oop slot.
        if offset <= 0 || rbp < offset as usize {
            continue;
        }
        let addr = rbp - offset as usize;
        // Alignment check defensively matches the conservative scan.
        if addr & 0x7 != 0 {
            continue;
        }
        // SAFETY: `frame_base` came from `current_stack_pointer()`
        // on this thread, and the offset is bounded by the frame
        // size recorded at compile time. The read is within the
        // calling thread's own stack region.
        let qword = unsafe { (addr as *const usize).read() };
        if let Some(obj) = heap.is_object_address(qword) {
            out.push(obj);
        }
    }
}

/// Scan a single JIT frame's spill region.
///
/// `low_sp` is the lowest address the scan should touch (typically the
/// scanner's own stack pointer or the next-inner JIT entry). `high_sp` is
/// the address recorded at JIT entry — one past the topmost spill slot.
/// We walk `[low_sp, high_sp)` in 8-byte strides.
fn scan_one_frame(low_sp: usize, high_sp: usize, heap: &VmHeap, out: &mut Vec<ObjectRef>) {
    if high_sp <= low_sp {
        // Either the chain is inverted or the JIT call hasn't actually
        // pushed any locals yet. Nothing to scan.
        return;
    }
    // Round low_sp up to the nearest 8-byte boundary so we never read an
    // unaligned qword (would be a bus error on some platforms).
    let aligned_low = (low_sp + 7) & !7usize;
    if aligned_low >= high_sp {
        return;
    }
    // Bound the scan to a sane upper limit so a stale `high_sp` (e.g. from a
    // recycled stack region after a thread tear-down) cannot send us into
    // unmapped pages. 8 MiB matches the default `CRATONVM_STACK` size and is
    // generously above any realistic JIT spill region.
    const MAX_SCAN_BYTES: usize = 8 * 1024 * 1024;
    let span = high_sp.saturating_sub(aligned_low);
    let span = span.min(MAX_SCAN_BYTES);
    let aligned_high = aligned_low + span;

    // SAFETY: the loop reads aligned qwords inside the calling thread's own
    // stack region between two known-valid stack pointers. The lower bound
    // came from `current_stack_pointer()` taken on the same thread; the upper
    // bound was captured at JIT entry on the same thread. Rust stacks are
    // backed by mapped pages for their entire reserved range, so reads in
    // this interval are well-defined. We never write through the pointer.
    //
    // Hoist the heap's address envelope out of the loop. `is_object_address`
    // is an out-of-line call that re-reads the arena bounds (three `Acquire`
    // load pairs on the generational backend) for EVERY word -- and almost
    // every word on a stack is a return address, an int, or a native pointer
    // that fails that very first test. Rejecting those inline against a
    // hoisted `[lo, hi)` leaves the full validator to run only for words that
    // could plausibly be object headers. `None` means the backend has no cheap
    // envelope (ZGC), in which case every word goes through the validator as
    // before.
    let span = heap.conservative_addr_span();
    let mut addr = aligned_low;
    let hits_before = out.len();
    while addr + 8 <= aligned_high {
        let qword = unsafe { (addr as *const usize).read() };
        addr += 8;
        if let Some((lo, hi)) = span {
            if qword < lo || qword >= hi {
                continue;
            }
        }
        if let Some(obj) = heap.is_object_address(qword) {
            out.push(obj);
        }
    }
    // Counted per CALL, never per word — see `rootprof::note_stack_scan` for
    // why these exist. Free when `CRATONVM_DBG_ROOTPROF` is unset (one
    // already-resolved `OnceLock` load).
    crate::memory::native_roots::rootprof::note_stack_scan(
        ((aligned_high - aligned_low) / 8) as u64, // Widening: bounded by MAX_SCAN_BYTES
        (out.len() - hits_before) as u64,          // Widening: a Vec length
    );
}

/// Total addresses published to the unrewritable-root veto this process.
static UNREWRITABLE_BAND_ROOTS: AtomicUsize = AtomicUsize::new(0);

/// Addresses published to the unrewritable-root veto since process start.
///
/// Diagnostic only. A non-zero count means at least one compiled frame held a
/// live object in a word `band_slot_is_verifiable` refuses to inspect — i.e.
/// the pin below is doing work, not just costing a branch.
pub fn unrewritable_band_root_count() -> usize {
    UNREWRITABLE_BAND_ROOTS.load(Ordering::Relaxed)
}

/// Publish every object reachable from one compiled frame's UNVERIFIABLE band
/// words to the pin veto (`gc_quiescence::add_unrewritable_jit_root`).
///
/// `scan_one_frame` has already walked the whole band and pushed these objects
/// as marking roots — that is what keeps them alive across the pause, and it is
/// also what gets them COPIED, because a movable claim from any other slot
/// naming the same address wins at the pin decision. This second, cheap pass
/// says which of those roots arrived through a word nobody can rewrite, so that
/// claim can be vetoed.
///
/// The two halves must stay the same partition: the words visited here are
/// exactly the ones `band_slot_is_verifiable` returns `false` for, which is
/// exactly the set `remap_register_image_words` would otherwise have to
/// REWRITE. Pinning is the sound half of that choice — see the module comment
/// on `UNREWRITABLE_JIT_ROOTS` in `gc_quiescence`.
fn publish_unrewritable_band_roots(
    rbp: usize,
    frame_size: usize,
    cm: &cratonvm_jit::CompiledMethod,
    heap: &VmHeap,
) {
    if frame_size == 0 || frame_size > rbp {
        return;
    }
    let live_hi = moving_young_frame_live_hi(rbp, cm);
    let lo = rbp - frame_size;
    let mut addr = (lo + 7) & !7usize;
    // Same clamp as `band_has_unpublished_word_with`: a stale bound must never
    // walk into unmapped pages. A compiled frame is orders of magnitude
    // smaller, so this is unreachable in practice.
    const MAX_SCAN_BYTES: usize = 1024 * 1024;
    let hi = rbp.min(addr.saturating_add(MAX_SCAN_BYTES));
    let envelope = heap.conservative_addr_span();
    let mut published = 0usize;
    while addr + 8 <= hi {
        // Cast: a compiled frame is far smaller than i32::MAX bytes.
        let off = (rbp - addr) as i32;
        if band_slot_is_verifiable(off, &cm.frame_layout, live_hi) {
            // Verified storage. An unpublished movable oop here already forces
            // the non-moving sweep, and a published one is rewritten by
            // `remap_one_jit_frame`, so it needs no pin and pinning it would
            // give back the drain this set exists to preserve.
            addr += 8;
            continue;
        }
        // SAFETY: aligned read inside this thread's own live compiled frame,
        // bounded by the frame size recorded at compile time — the same
        // interval `scan_one_frame` has already read.
        let qword = unsafe { (addr as *const usize).read() };
        addr += 8;
        if let Some((elo, ehi)) = envelope {
            if qword < elo || qword >= ehi {
                continue;
            }
        }
        if heap.is_object_address(qword).is_some() {
            cratonvm_gc::gc_quiescence::add_unrewritable_jit_root(qword);
            published += 1;
        }
    }
    if published > 0 {
        UNREWRITABLE_BAND_ROOTS.fetch_add(published, Ordering::Relaxed);
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod coverage_oracle_gate_tests {
    use super::*;

    /// THE STATE THESE TESTS EXERCISE IS PROCESS-GLOBAL, and `cargo test` runs
    /// one crate's tests as threads of one process. The refutation latch and
    /// the moving-young coverage cycle are one shared pair of globals, so
    /// `a_refutation_latches_and_does_not_clear`'s
    /// `note_coverage_oracle_refutation()` can land between this module's other
    /// test beginning a cycle and reading it back -- and `mod tests`'
    /// `beginning_a_coverage_cycle_clears_the_peer_ledger` begins a cycle of
    /// its own from a third thread.
    ///
    /// MEASURED on the build host: `assertion failed:
    /// !moving_young_coverage_incomplete()` at the FIRST assert of
    /// `a_refutation_marks_this_cycle_incomplete`, on a loaded host, in a tree
    /// where the same gate had passed the run before. The module alone passes
    /// 4 of 4 either way, which is what says race rather than defect.
    ///
    /// Serialising is the fix rather than merging the tests: they assert
    /// different properties and a reader should see which one broke.
    /// `parking_lot` is not a dependency here, and a poisoned `std` mutex would
    /// turn one failure into a cascade, so the guard is taken through
    /// `unwrap_or_else(|e| e.into_inner())`. Same shape as `types`'
    /// `arraylist_view` latch.
    pub(super) static COVERAGE_ORACLE_TEST_LATCH: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// The latch starts clear, a refutation sets it, and it does not clear
    /// itself. One-way is the point: the refutation is about compiled code
    /// that is still in the cache, not about the moment it was observed.
    #[test]
    fn a_refutation_latches_and_does_not_clear() {
        let _serialised = COVERAGE_ORACLE_TEST_LATCH
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        reset_coverage_oracle_refuted_for_test();
        assert!(!coverage_oracle_refuted(), "latch must start clear");
        note_coverage_oracle_refutation();
        assert!(coverage_oracle_refuted(), "a refutation must latch");
        // A later cycle that observes nothing must not un-refute.
        assert!(
            coverage_oracle_refuted(),
            "the latch must survive a cycle that saw nothing"
        );
        reset_coverage_oracle_refuted_for_test();
    }

    /// A refutation marks the CURRENT cycle incomplete too, not just later
    /// ones — the collector asking the question is mid-decision.
    #[test]
    fn a_refutation_marks_this_cycle_incomplete() {
        let _serialised = COVERAGE_ORACLE_TEST_LATCH
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        reset_coverage_oracle_refuted_for_test();
        cratonvm_gc::gc_quiescence::begin_moving_young_coverage_cycle();
        assert!(!cratonvm_gc::gc_quiescence::moving_young_coverage_incomplete());
        note_coverage_oracle_refutation();
        assert!(
            cratonvm_gc::gc_quiescence::moving_young_coverage_incomplete(),
            "the cycle being decided must go incomplete"
        );
        assert_eq!(
            cratonvm_gc::gc_quiescence::moving_young_incomplete_reason(),
            cratonvm_gc::gc_quiescence::incomplete_reason::COVERAGE_ORACLE_REFUTED,
        );
        reset_coverage_oracle_refuted_for_test();
        cratonvm_gc::gc_quiescence::begin_moving_young_coverage_cycle();
    }

    /// The gate is inert unless something is actually doing the verifying.
    /// Default-off is what keeps this free on the production path, so a
    /// regression that turns it on by accident must fail here.
    #[test]
    fn the_gate_is_inert_by_default() {
        assert!(
            !coverage_gate_active(),
            "neither CRATONVM_DBG_VERIFY_OOP_MAPS nor the force switch is set \
             in the test environment, so the gate must not run"
        );
    }

    /// The refutation gate must watch the map-SELECTION counter, not only the
    /// two coverage ones.
    ///
    /// `WRONG_MAP` counts a live in-band oop named by some map of the method
    /// but not by the one its safepoint id selects, and the precise scan reads
    /// only the selected map — so the suppression this gate licenses would
    /// strand it. Measured on the 2026-09-02 soak, `HumongousHold` reports
    /// `while_covered=0 wrong_map=160`: the two counters this gate used to read
    /// were BOTH zero over frames holding 160 oops the precise walk cannot
    /// reach.
    ///
    /// It must read the VERIFIER-CONFIRMED subset, not the raw counter. The raw
    /// one over-reports: a slot whose value merely looks like a heap address is
    /// counted even when it is dead storage, which the precise map is right to
    /// omit. Measured 2026-09-02, the class file's own type maps returned
    /// `verifier_oop=0` over every never-mapped word on these workloads, so a
    /// gate keyed on the raw count would refuse the suppression forever on
    /// evidence that is not evidence.
    ///
    /// A source-level assertion because the gate needs live compiled frames to
    /// run: what is pinned here is that the counter appears in the decision at
    /// all, which is the thing that was missing.
    #[test]
    fn the_refutation_gate_reads_the_map_selection_counter() {
        let src = include_str!("conservative_roots.rs");
        let gate = src
            .split("pub fn verify_active_coverage_into")
            .nth(1)
            .expect("the gate is defined in this file");
        let body = &gate[..gate.find("
}
").expect("the gate has a body")];
        for counter in [
            "NEVER_MAPPED_WHILE_COVERED",
            "NEVER_MAPPED_WHILE_SHADOW_COVERED",
            "WRONG_MAP_VERIFIER_OOP",
        ] {
            assert!(
                body.contains(counter),
                "verify_active_coverage_into must consult {counter}: a gate that                  skips one returns \"proof holds\" over frames the precise scan                  cannot cover"
            );
        }
    }

    /// The kill switch and the force switch read the keys they are documented
    /// under. A gate whose flag is misspelled reports itself off forever.
    #[test]
    fn the_switches_read_their_documented_keys() {
        let surface = include_str!("../../../types/tests/flag-surface.txt");
        for key in [
            "CRATONVM_DBG_OOP_ORACLE_FORCE_REFUTE",
            "CRATONVM_GC_PRECISE_ONLY_ROOTS",
        ] {
            assert!(
                surface.lines().any(|l| l.trim() == key),
                "{key} must be declared in the flag surface"
            );
        }
    }
}

// ---------------------------------------------------------------------------
// Post-remap stale-reference detector (`CRATONVM_DBG_JIT_STALE_AFTER_REMAP`)
// ---------------------------------------------------------------------------
//
// `remap_active_jit_frames` rewrites exactly the slots the active oop map
// names. Everything else in a compiled frame -- the callee-saved GPR/XMM save
// areas, the per-safepoint blind GPR spill, operand-spill slots above the
// safepoint's live cursor, the outgoing-argument reserve, and every word of the
// Rust/interpreter frames the compiled method called INTO -- keeps whatever it
// held before the move. The conservative scan READS those regions (that is what
// makes an object there survive), so a moving cycle can copy an object whose
// only holder is a word nothing rewrites.
//
// This walks the same bands the conservative scan reads, AFTER the remap, and
// reports any word that is still a key of `pointer_map` -- i.e. an address the
// collection moved away from. It names the method, the frame offset, the
// `FrameLayout` region and the CLASS of the object at the new address, so the
// storage class responsible is named rather than guessed at. Diagnostic only;
// default off.
fn stale_after_remap_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        cratonvm_types::flags::runtime_var_os("CRATONVM_DBG_JIT_STALE_AFTER_REMAP").is_some()
    })
}

/// `CRATONVM_DBG_JIT_STALE_BELOW_RBP` -- additionally walk the Rust /
/// interpreter frames beneath the innermost compiled frame. That range is NOT
/// read by the conservative band scan when the bounded bands are in use, so
/// most of what it holds is dead stack slop and it drowns out the
/// compiled-frame findings; separate flag.
fn stale_below_rbp_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        cratonvm_types::flags::runtime_var_os("CRATONVM_DBG_JIT_STALE_BELOW_RBP").is_some()
    })
}

static STALE_AFTER_REMAP_HITS: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);

/// Of those, the ones in a region something RESUMES FROM.
static STALE_AFTER_REMAP_RESUMED: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);

/// Total words found still naming a moved-from address after a remap.
pub fn stale_after_remap_hits() -> usize {
    STALE_AFTER_REMAP_HITS.load(Ordering::Relaxed)
}

/// Of those, how many sat in a region something resumes from — the
/// callee-saved GPR image. See [`stale_after_remap_census`].
pub fn stale_after_remap_resumed_hits() -> usize {
    STALE_AFTER_REMAP_RESUMED.load(Ordering::Relaxed)
}

/// Name the class of the object now living at `addr`, for diagnostics.
///
/// `addr` is a POST-move address (a `pointer_map` value), so the object is
/// live and its header is intact -- which is the whole reason the report is
/// taken here rather than at the point of use, where the from-space cell has
/// already been zeroed and reads back as `java.lang.Object`.
fn class_name_at(shared: Option<&crate::vm::SharedVm>, addr: usize) -> String {
    let Some(shared) = shared else {
        return "?".to_string();
    };
    let Some(obj) = shared.mem.heap.is_object_address(addr) else {
        return "<not-an-object>".to_string();
    };
    let cid = shared.mem.heap.class_id_of(obj);
    shared
        .classes
        .class_manager
        .try_read()
        .and_then(|cm| cm.get_class(cid).map(|c| c.name.to_string()))
        .unwrap_or_else(|| format!("class_id={}", cid.as_u32()))
}

fn report_stale_words_in(
    lo: usize,
    hi: usize,
    cm: Option<&cratonvm_jit::CompiledMethod>,
    rbp: usize,
    pointer_map: &cratonvm_types::PointerMap,
    shared: Option<&crate::vm::SharedVm>,
    tag: &str,
) {
    if hi <= lo {
        return;
    }
    let mut addr = (lo + 7) & !7usize;
    const MAX_SCAN_BYTES: usize = 1024 * 1024;
    let hi = hi.min(addr.saturating_add(MAX_SCAN_BYTES));
    while addr + 8 <= hi {
        // SAFETY: aligned read inside this thread's own live stack interval,
        // bounded by the caller's frame bounds.
        let w = unsafe { (addr as *const usize).read() };
        if let Some(&new) = pointer_map.get(&w) {
            if new == w {
                // The map records a no-op relocation for objects the copy left
                // where they were. Not a stale reference.
                addr += 8;
                continue;
            }
            let n = STALE_AFTER_REMAP_HITS.fetch_add(1, Ordering::Relaxed);
            // Split the hit by whether anything RESUMES from the word. The
            // shared census is what the `System.exit` shutdown trailer prints;
            // the local counter is what this crate's tests read. See
            // `cratonvm_types::stale_remap_census`.
            let resumed_from = cm.is_some_and(|cm| {
                // Cast: a compiled frame is far smaller than i32::MAX.
                is_callee_saved_gpr_image((rbp - addr) as i32, &cm.frame_layout)
            });
            if resumed_from {
                STALE_AFTER_REMAP_RESUMED.fetch_add(1, Ordering::Relaxed);
            }
            cratonvm_types::stale_remap_census::note(resumed_from);
            if n < 4000 {
                let class = class_name_at(shared, new);
                match cm {
                    Some(cm) => {
                        // Cast: a compiled frame is far smaller than i32::MAX.
                        let off = (rbp - addr) as i32;
                        eprintln!(
                            "[jit-stale-after-remap] {tag} method={} off={off} region={} \
                             verifiable={} resumed_from={} class={class} value=0x{w:x} \
                             moved_to=0x{new:x}",
                            cm.method_label,
                            cm.frame_layout.region_name(off),
                            band_slot_is_verifiable(
                                off,
                                &cm.frame_layout,
                                moving_young_frame_live_hi(rbp, cm),
                            ),
                            is_callee_saved_gpr_image(off, &cm.frame_layout),
                        );
                    }
                    None => {
                        eprintln!(
                            "[jit-stale-after-remap] {tag} depth_below_rbp={} class={class} \
                             value=0x{w:x} moved_to=0x{new:x}",
                            rbp.saturating_sub(addr),
                        );
                    }
                }
            }
        }
        addr += 8;
    }
}

/// Walk this thread's live compiled frame bands and report any word naming an
/// address the LAST collection vacated.
///
/// # Why this exists next to `reclaim_guard::audit_thread_frames`
///
/// That auditor tests `thread.frames` -- the INTERPRETER frames -- against the
/// same ledger, and its report ("a LIVE frame slot still names an address the
/// LAST collection moved an object away from") is the exact verdict wanted
/// here. It simply cannot see a compiled frame: a JIT frame's oops live in the
/// machine stack band, not in a `Frame`. So on a workload whose stale holder is
/// compiled, the interpreter auditor is silent and the first symptom is the
/// SIGSEGV.
///
/// The ledger is what makes this sharper than
/// [`report_stale_after_remap`]. That one flags any word equal to a pointer-map
/// KEY, which on a real stack is mostly dead slop that happens to look like a
/// moved address (thousands per run). `gc_quiescence::was_vacated` subtracts the
/// destination set, so a hit is a word naming an address the collector moved an
/// object AWAY from and did not move anything back INTO -- which is precisely
/// the read that faults.
///
/// No-op unless `CRATONVM_DBG_VACATED_FRAMES` is armed.
pub fn audit_jit_frames_for_vacated(
    shared: Option<&crate::vm::SharedVm>,
    tid: u64,
    site: &'static str,
) {
    if !cratonvm_gc::gc_quiescence::vacated_frames_enabled() {
        return;
    }
    let scanner_sp = current_stack_pointer();
    JIT_ENTRY_CHAIN.with(|c| {
        {
            let mut v = c.borrow_mut();
            flush_top_rbp_cache_to_chain(v.as_mut_slice());
        }
        let chain = c.borrow();
        for entry in chain.iter() {
            let Some(info) = entry.precise else { continue };
            let entry_sp = entry.entry_sp;
            let mut rbp = info.exact_rbp;
            if rbp == 0 || rbp & 0x7 != 0 || rbp < scanner_sp || rbp >= entry_sp {
                continue;
            }
            let Some(innermost_cm) = innermost_frame_method(
                rbp,
                info.exact_cm_id,
                entry_sp,
                scanner_sp,
                info.compiled_method,
            ) else {
                continue;
            };
            // SAFETY: same contract as `report_stale_after_remap` -- the chain
            // entry's CompiledMethod is Arc-owned by the JIT cache while any of
            // its frames is live.
            let mut cm: &cratonvm_jit::CompiledMethod = unsafe { &*innermost_cm };
            let mut frames = 0usize;
            while frames < 4096 {
                frames += 1;
                let frame_size = cm.osr_frame_size;
                if frame_size <= 0 {
                    break;
                }
                let frame_size = frame_size as usize;
                const MAX_COMPILED_FRAME_BYTES: usize = 1024 * 1024;
                if frame_size > MAX_COMPILED_FRAME_BYTES || frame_size > rbp {
                    break;
                }
                report_vacated_words_in(rbp - frame_size, rbp, cm, rbp, shared, tid, site);
                // SAFETY: `rbp` is a validated frame base in this thread's live
                // JIT stack interval.
                let parent_rbp = unsafe { (rbp as *const usize).read() };
                let ret_addr = unsafe { ((rbp + 8) as *const usize).read() };
                let Some(parent_cm_ptr) = cratonvm_jit::lookup_jit_code_range(ret_addr) else {
                    break;
                };
                if parent_rbp <= rbp
                    || parent_rbp & 0x7 != 0
                    || parent_rbp >= entry_sp
                    || parent_rbp < scanner_sp
                {
                    break;
                }
                // SAFETY: code ranges retain their CompiledMethod metadata for
                // the lifetime of an active frame.
                cm = unsafe { &*(parent_cm_ptr as *const cratonvm_jit::CompiledMethod) };
                rbp = parent_rbp;
            }
        }
    });
}

/// Hits reported by [`audit_jit_frames_for_vacated`] this process.
pub static JIT_VACATED_FRAME_HITS: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

fn report_vacated_words_in(
    lo: usize,
    hi: usize,
    cm: &cratonvm_jit::CompiledMethod,
    rbp: usize,
    shared: Option<&crate::vm::SharedVm>,
    tid: u64,
    site: &'static str,
) {
    if hi <= lo {
        return;
    }
    let mut addr = (lo + 7) & !7usize;
    const MAX_SCAN_BYTES: usize = 1024 * 1024;
    let hi = hi.min(addr.saturating_add(MAX_SCAN_BYTES));
    while addr + 8 <= hi {
        // SAFETY: aligned read inside this thread's own live stack interval,
        // bounded by the caller's frame bounds.
        let w = unsafe { (addr as *const usize).read() };
        if let Some(moved_to) = cratonvm_gc::gc_quiescence::was_vacated(w) {
            let n = JIT_VACATED_FRAME_HITS.fetch_add(1, Ordering::Relaxed);
            if n < 200 {
                // Cast: a compiled frame is far smaller than i32::MAX.
                let off = (rbp - addr) as i32;
                eprintln!(
                    "[jit-vacated-frame] site={site} tid={tid} method={} off={off} region={}                      verifiable={} resumed_from={} value=0x{w:x} moved_to=0x{moved_to:x}                      class_at_target={}",
                    cm.method_label,
                    cm.frame_layout.region_name(off),
                    band_slot_is_verifiable(
                        off,
                        &cm.frame_layout,
                        moving_young_frame_live_hi(rbp, cm),
                    ),
                    is_callee_saved_gpr_image(off, &cm.frame_layout),
                    class_name_at(shared, moved_to),
                );
            }
        }
        addr += 8;
    }
}

/// Walk every live compiled frame band after a moving collection has remapped
/// the oop-map slots and report words that still name a moved-from address.
///
/// See [`stale_after_remap_enabled`]. No-op unless the flag is set.
pub fn report_stale_after_remap(
    pointer_map: &cratonvm_types::PointerMap,
    shared: Option<&crate::vm::SharedVm>,
) {
    if pointer_map.is_empty() || !stale_after_remap_enabled() {
        return;
    }
    let scanner_sp = current_stack_pointer();
    JIT_ENTRY_CHAIN.with(|c| {
        {
            let mut v = c.borrow_mut();
            flush_top_rbp_cache_to_chain(v.as_mut_slice());
        }
        let chain = c.borrow();
        for entry in chain.iter() {
            let Some(info) = entry.precise else { continue };
            let entry_sp = entry.entry_sp;
            let mut rbp = info.exact_rbp;
            if rbp == 0 || rbp & 0x7 != 0 || rbp < scanner_sp || rbp >= entry_sp {
                continue;
            }
            if stale_below_rbp_enabled() {
                report_stale_words_in(
                    scanner_sp,
                    rbp,
                    None,
                    rbp,
                    pointer_map,
                    shared,
                    "below-innermost-rbp",
                );
            }
            let Some(innermost_cm) = innermost_frame_method(
                rbp,
                info.exact_cm_id,
                entry_sp,
                scanner_sp,
                info.compiled_method,
            ) else {
                continue;
            };
            // SAFETY: same contract as `scan_compiled_frame_bands` -- the chain
            // entry's CompiledMethod is Arc-owned by the JIT cache while any of
            // its frames is live.
            let mut cm: &cratonvm_jit::CompiledMethod = unsafe { &*innermost_cm };
            let mut frames = 0usize;
            while frames < 4096 {
                frames += 1;
                let frame_size = cm.osr_frame_size;
                if frame_size <= 0 {
                    break;
                }
                let frame_size = frame_size as usize;
                const MAX_COMPILED_FRAME_BYTES: usize = 1024 * 1024;
                if frame_size > MAX_COMPILED_FRAME_BYTES || frame_size > rbp {
                    break;
                }
                report_stale_words_in(
                    rbp - frame_size,
                    rbp,
                    Some(cm),
                    rbp,
                    pointer_map,
                    shared,
                    "frame-band",
                );
                // SAFETY: `rbp` is a validated frame base in this thread's live
                // JIT stack interval.
                let parent_rbp = unsafe { (rbp as *const usize).read() };
                let ret_addr = unsafe { ((rbp + 8) as *const usize).read() };
                let Some(parent_cm_ptr) = cratonvm_jit::lookup_jit_code_range(ret_addr) else {
                    break;
                };
                if parent_rbp <= rbp
                    || parent_rbp & 0x7 != 0
                    || parent_rbp >= entry_sp
                    || parent_rbp < scanner_sp
                {
                    break;
                }
                // SAFETY: code ranges retain their CompiledMethod metadata for
                // the lifetime of an active frame.
                cm = unsafe { &*(parent_cm_ptr as *const cratonvm_jit::CompiledMethod) };
                rbp = parent_rbp;
            }
        }
    });
}

// ---------------------------------------------------------------------------
// Register-image remap: the other half of the frame-word partition
// ---------------------------------------------------------------------------
//
// `band_slot_is_verifiable` splits a compiled frame's band in two. Words it
// inspects are VERIFIED: an unpublished movable oop in one of them makes
// `moving_young_unpublished_frame_oop_present` report
// `UNPUBLISHED_FRAME_OOP`, and the cycle diverts to the non-moving sweep.
// Words it skips were, until now, neither verified NOR rewritten -- the
// prologue's callee-saved GPR/XMM save areas, the per-safepoint blind GPR
// spill, the outgoing-argument / deopt-register reserve, and operand-spill
// slots above the safepoint's live cursor.
//
// The justification for skipping them is that they are register IMAGES and
// dead argument words: "not storage this frame resumes from". That is true of
// THIS frame and false of its caller. A compiled prologue saves the CALLER's
// callee-saved GPRs into its own frame, and the epilogue pops them straight
// back into the caller's registers -- so the caller resumes from exactly the
// words the verifier declined to look at. `scan_compiled_frame_bands` READS
// them, which is what keeps the referenced object alive across the pause; the
// moving young collection then copies it, `remap_active_jit_frames` rewrites
// only the slots an oop map names, and the saved word is left holding a
// from-space address.
//
// This pass closes the half of that partition that something RESUMES FROM, and
// it is deliberately narrower than "every word the verifier refuses". The
// unverifiable regions are not equivalent, and lumping them together is what
// made the repair look unsafe enough to ship off:
//
//   * `callee-saved-gpr-image` -- the prologue's save area for the CALLER's
//     callee-saved GPRs, which the epilogue pops straight back into the
//     caller's registers. This one IS resumed from, and it is the ONLY region
//     this pass rewrites.
//   * `callee-saved-xmm-image` -- resumed from as well, but an XMM never holds
//     an object reference in this VM's calling convention, so a hit there is a
//     false positive by construction and rewriting it could only corrupt a
//     double.
//   * `safepoint-gpr-spill-image` -- write-only. `emit_pre_safepoint_spill`
//     stores the GPR file purely so the conservative scan can SEE it and says
//     so in its own words ("no post-call reload is needed"); nothing ever loads
//     from these slots, so a stale word there is read by no one.
//   * `outgoing-args-or-deopt-regs`, and operand-spill slots above the
//     safepoint's live cursor -- dead by definition. The cursor reclaims by
//     moving, so those slots hold whatever the deepest earlier operand stack
//     left behind.
//
// Restricting to the GPR image is also what makes the write defensible. The
// dead regions are where a non-pointer that merely LOOKS like an object base is
// plausible -- they are full of abandoned values nobody screens. A caller's
// live callee-saved GPR is not: for a hit there to be a false positive, the
// caller would have to be holding a non-reference whose value is exactly a
// young object's base address, which `heap.is_object_address` validated against
// the arena bounds and the object-start bitmap. Loop counters, sizes and PCs do
// not reach those addresses, and Rust-side pointers are in different mappings.
//
// The compiled callers do not actually need this: a compiled frame reloads its
// live oops from the shadow stack after every safepoint, so its registers are
// refreshed whatever the image held. What needs it is the OUTERMOST compiled
// frame, whose caller is the VM's own Rust code at the interpreter->JIT
// boundary -- which has no reload and resumes from exactly those popped
// registers. That is the frame every observation on this defect has named.
//
// SHIPS ON, with `CRATONVM_REGISTER_IMAGE_REMAP=0` as the kill switch and
// `CRATONVM_DBG_JIT_STALE_AFTER_REMAP=1` as the instrument that says whether it
// has anything to do on a given workload. See
// `moving-young-left-a-callee-saved-register-image-unrewritten-FIXED-20260823`
// for the measurement.
//
// The other half of the repair is a PIN rather than a write:
// `publish_unrewritable_band_roots` publishes every object an unverifiable word
// holds to `gc_quiescence::add_unrewritable_jit_root`, which vetoes the movable
// claim at the young sweep's pin decision. That covers the non-moving arm at no
// risk at all; a Cheney moving young collection has no pin, which is why this
// write exists for it.
fn register_image_remap_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        !matches!(
            cratonvm_types::flags::runtime_var_os("CRATONVM_REGISTER_IMAGE_REMAP")
                .as_deref()
                .and_then(|s| s.to_str()),
            Some("0")
        )
    })
}

static REGISTER_IMAGE_REMAP_WORDS: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);

/// Total words rewritten by [`remap_register_image_words`] this process.
pub fn register_image_remap_words() -> usize {
    REGISTER_IMAGE_REMAP_WORDS.load(Ordering::Relaxed)
}

/// Is `off` inside the prologue's save area for the CALLER's callee-saved GPRs?
///
/// The one unverifiable region a frame's CALLER resumes from — the epilogue
/// pops these words straight back into its registers. `band_slot_is_verifiable`
/// refuses the whole `off >= callee_saved_lo` tail (GPR image, XMM image,
/// safepoint spill, outgoing-args/deopt reserve); this narrows it back to the
/// GPR image alone. See the module comment above for why the other three are
/// deliberately left stale.
#[inline]
fn is_callee_saved_gpr_image(off: i32, layout: &cratonvm_jit::FrameLayout) -> bool {
    layout.callee_saved_hi > layout.callee_saved_lo
        && off >= layout.callee_saved_lo
        && off < layout.callee_saved_hi
}

/// Does the register-image remap rewrite the word at `off`?
///
/// Default: the callee-saved GPR image alone, which is what
/// [`is_callee_saved_gpr_image`] answers and what the module comment above
/// argues for at length.
///
/// `CRATONVM_JIT_REMAP_ALL_UNVERIFIABLE=1` widens it to the whole unverifiable
/// tail (`off >= callee_saved_lo`). **A DIAGNOSTIC, and deliberately not a
/// proposed default.** It exists because that module comment partitions the
/// tail into one region something resumes from and three it calls dead or
/// write-only, and that partition is an ARGUMENT rather than a measurement --
/// while the instrument that looks like it could check it cannot: the
/// stale-word census reports `resumed_from` as `is_callee_saved_gpr_image` and
/// nothing else, so it reads `false` for all three of the regions in question
/// by construction, and its zero is not an all-clear for them.
///
/// The experiment this enables is a single A/B on the reproducer in
/// `internal/fixed-suite-bugs/netty/bytebuf-multiplethreads-npe-generational-blocked-wake-jit-remap-FIXED-20260908.md`
/// §10.4 -- `CRATONVM_GC_NO_PEER_PIN_DIVERT=1` on
/// `io.netty.handler.ipfilter.UniqueIpFilterTest`, which SIGSEGVs 3 runs in 13
/// with compiled code reading a decommitted span. If widening the write removes
/// the crash, one of the three "dead" regions is read after all and the
/// partition is wrong. If the crash survives, they are exonerated on this
/// workload and the stale reference lives somewhere neither pass touches.
///
/// Stated cost of the widened arm, so nobody ships it by accident: it also
/// rewrites the callee-saved XMM image, where an object-shaped bit pattern
/// would be a double rather than a reference. `heap.is_object_address` vets
/// every candidate against the arena bounds and the object-start bitmap, so a
/// false positive needs a double whose bits are exactly a live young object's
/// base -- unlikely, not impossible, and a reason this is off by default.
///
/// # THE ANSWER, measured 2026-09-06 -- widening changes NOTHING
///
/// | arm | crashes / runs | rate |
/// |---|---|---|
/// | default (callee-saved GPR image only) | **11 / 81** | 13.6 % |
/// | `CRATONVM_JIT_REMAP_ALL_UNVERIFIABLE=1` | **2 / 32** | 6.3 % |
///
/// Fisher p ~ 0.37 -- not a difference. Two interleaved matched batches on one
/// binary (narrow 0/14 vs wide 2/14, then narrow 2/18 vs wide 0/18) land on
/// either side of the pooled baseline, which is what a null result looks like
/// when each batch is read on its own; the first batch alone reads as "widening
/// makes it worse" and that was wrong.
///
/// So `operand-spill`, `safepoint-gpr-spill-image` and
/// `outgoing-args-or-deopt-regs` are **not** where this defect's repair goes:
/// rewriting every one of them does not move the crash rate. That is a negative
/// result worth keeping, because the blind GPR spill area is one of the three
/// storage classes `moving-young-corruption-rootcause.md` nominates and this
/// eliminates it on this workload.
///
/// The flag stays for the next person who wants to re-ask the question on a
/// different workload -- it is a few lines and it is off by default -- but it
/// is NOT a candidate fix and should not be flipped on.
#[inline]
fn register_image_remap_admits(off: i32, layout: &cratonvm_jit::FrameLayout) -> bool {
    if is_callee_saved_gpr_image(off, layout) {
        return true;
    }
    remap_all_unverifiable()
        && layout.callee_saved_hi > layout.callee_saved_lo
        && off >= layout.callee_saved_lo
}

/// `CRATONVM_JIT_REMAP_ALL_UNVERIFIABLE=1` -- see
/// [`register_image_remap_admits`]. Default off.
fn remap_all_unverifiable() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        cratonvm_types::flags::runtime_var_os("CRATONVM_JIT_REMAP_ALL_UNVERIFIABLE").is_some()
    })
}

/// Rewrite the moved references held in one frame's callee-saved GPR image.
fn remap_one_frame_register_images(
    rbp: usize,
    cm: &cratonvm_jit::CompiledMethod,
    pointer_map: &cratonvm_types::PointerMap,
    shared: Option<&crate::vm::SharedVm>,
    dbg: bool,
) -> usize {
    let frame_size = cm.osr_frame_size;
    if frame_size <= 0 {
        return 0;
    }
    let frame_size = frame_size as usize;
    const MAX_COMPILED_FRAME_BYTES: usize = 1024 * 1024;
    if frame_size > MAX_COMPILED_FRAME_BYTES || frame_size > rbp {
        return 0;
    }
    // No `live_hi` here, deliberately: the safepoint's live cursor bounds the
    // OPERAND-SPILL region, and this pass no longer touches it. Only the
    // prologue's callee-saved GPR image is in scope, and its bounds are static
    // frame geometry.
    let lo = rbp - frame_size;
    let mut addr = (lo + 7) & !7usize;
    let mut rewritten = 0usize;
    while addr + 8 <= rbp {
        // Cast: a compiled frame is far smaller than i32::MAX bytes.
        let off = (rbp - addr) as i32;
        if !register_image_remap_admits(off, &cm.frame_layout) {
            // Everything else is either VERIFIED storage -- where an
            // unpublished movable oop has already forced the non-moving sweep
            // and a published one was rewritten by `remap_one_jit_frame` -- or
            // an unverifiable region nothing resumes from. The module comment
            // above enumerates the four and says why each is excluded; this is
            // the line that keeps the write off the dead ones.
            addr += 8;
            continue;
        }
        // SAFETY: aligned read inside this thread's own live compiled frame,
        // bounded by the frame size recorded at compile time.
        let w = unsafe { (addr as *const usize).read() };
        if let Some(&new) = pointer_map.get(&w) {
            if new != w {
                // SAFETY: same slot, rewriting the relocated reference.
                unsafe { (addr as *mut usize).write(new) };
                rewritten += 1;
                if dbg {
                    eprintln!(
                        "[jit-register-image-remap] method={} off={off} region={} class={} \
                         0x{w:x}->0x{new:x}",
                        cm.method_label,
                        cm.frame_layout.region_name(off),
                        class_name_at(shared, new),
                    );
                }
            }
        }
        addr += 8;
    }
    rewritten
}

/// Rewrite moved references held in the register-image / outgoing-argument
/// words of every live compiled frame on this thread.
///
/// Companion to [`remap_active_jit_frames`], which covers the oop-map slots.
pub fn remap_register_image_words(
    pointer_map: &cratonvm_types::PointerMap,
    shared: Option<&crate::vm::SharedVm>,
) {
    if pointer_map.is_empty() || !register_image_remap_enabled() {
        return;
    }
    let dbg = stale_after_remap_enabled();
    let scanner_sp = current_stack_pointer();
    let mut total = 0usize;
    JIT_ENTRY_CHAIN.with(|c| {
        {
            let mut v = c.borrow_mut();
            flush_top_rbp_cache_to_chain(v.as_mut_slice());
        }
        let chain = c.borrow();
        for entry in chain.iter() {
            let Some(info) = entry.precise else { continue };
            let entry_sp = entry.entry_sp;
            let mut rbp = info.exact_rbp;
            if rbp == 0 || rbp & 0x7 != 0 || rbp < scanner_sp || rbp >= entry_sp {
                continue;
            }
            // Same resolution rule as `scan_compiled_frame_bands` and
            // `remap_active_jit_frames`: when nothing describes the frame at
            // `exact_rbp`, its layout would be read out of the wrong method,
            // so leave it alone. The coverage refresh reports
            // `FOREIGN_INNERMOST_RBP` for it, which already forces the
            // non-moving sweep, so there is no move to repair.
            let Some(innermost_cm) = innermost_frame_method(
                rbp,
                info.exact_cm_id,
                entry_sp,
                scanner_sp,
                info.compiled_method,
            ) else {
                continue;
            };
            // SAFETY: same contract as `scan_compiled_frame_bands` -- the chain
            // entry's CompiledMethod is Arc-owned by the JIT cache while any of
            // its frames is live, and a resolved callee is kept alive by the
            // live frame whose return address resolved it.
            let mut cm: &cratonvm_jit::CompiledMethod = unsafe { &*innermost_cm };
            let mut frames = 0usize;
            while frames < 4096 {
                frames += 1;
                total += remap_one_frame_register_images(rbp, cm, pointer_map, shared, dbg);
                // SAFETY: `rbp` is a validated frame base in this thread's live
                // JIT stack interval.
                let parent_rbp = unsafe { (rbp as *const usize).read() };
                let ret_addr = unsafe { ((rbp + 8) as *const usize).read() };
                let Some(parent_cm_ptr) = cratonvm_jit::lookup_jit_code_range(ret_addr) else {
                    break;
                };
                if parent_rbp <= rbp
                    || parent_rbp & 0x7 != 0
                    || parent_rbp >= entry_sp
                    || parent_rbp < scanner_sp
                {
                    break;
                }
                // SAFETY: code ranges retain their CompiledMethod metadata for
                // the lifetime of an active frame.
                cm = unsafe { &*(parent_cm_ptr as *const cratonvm_jit::CompiledMethod) };
                rbp = parent_rbp;
            }
        }
    });
    if total > 0 {
        REGISTER_IMAGE_REMAP_WORDS.fetch_add(total, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The oop-map oracle identifies a frame's method by parsing
    /// `CompiledMethod::method_label`, and everything downstream of it -- the
    /// class-id lookup, the verifier's type maps, the `verifier_oop` verdict --
    /// is wrong if this is. A descriptor cannot contain `:` and a method name
    /// cannot contain `.` or `:`, so two `rsplit_once`es are exact; the cases
    /// below are the ones that would break a `split_once` or a naive
    /// `split('.')`.
    #[test]
    fn a_method_label_splits_into_class_name_and_descriptor() {
        assert_eq!(
            split_method_label("org/h2/mvstore/MVStore.closeStore:(ZI)V"),
            Some(("org/h2/mvstore/MVStore", "closeStore", "(ZI)V"))
        );
        // A nested class, and a descriptor mentioning another class -- the two
        // shapes with extra `/` and `;` in them.
        assert_eq!(
            split_method_label("org/h2/mvstore/Page$PageReference.getPage:()Lorg/h2/mvstore/Page;"),
            Some((
                "org/h2/mvstore/Page$PageReference",
                "getPage",
                "()Lorg/h2/mvstore/Page;"
            ))
        );
        // `<init>` and `<clinit>` carry angle brackets, not separators.
        assert_eq!(
            split_method_label("java/lang/String.<init>:([BI)V"),
            Some(("java/lang/String", "<init>", "([BI)V"))
        );
        // The JIT writes labels with neither separator for synthetic bodies,
        // and an unstamped artifact carries the empty string. Both must be
        // `None` rather than a partial parse that then resolves to some other
        // class's maps.
        assert_eq!(split_method_label("lambda-adapter->0x7f0011223344"), None);
        assert_eq!(split_method_label(""), None);
        assert_eq!(split_method_label("NoDescriptor.method"), None);
        assert_eq!(split_method_label(".empty:()V"), None);
    }

    /// The frame-shape arithmetic the A5 census prices the probe with.
    ///
    /// A JIT frame's caller RBP sits one slot BELOW its return address (both
    /// x64 backends open every compiled body with `push rbp; mov rbp, rsp`),
    /// and, because the stack grows down, points at a HIGHER address. Getting
    /// that direction backwards would make the census report every hit as
    /// shaped and price the filter at zero.
    #[test]
    fn a5_frame_link_points_at_an_older_frame() {
        let hi = 0x7fff_0000_0000usize;
        let slot = 0x7ffe_0000_0000usize;
        // Caller's frame base: higher address, aligned, room for its own slots.
        assert!(a5_frame_base_is_plausible_link(slot, slot + 0x80, hi));
        // Younger than this frame — impossible for a caller.
        assert!(!a5_frame_base_is_plausible_link(slot, slot - 0x80, hi));
        // Equal is not "older" either: a frame cannot be its own caller.
        assert!(!a5_frame_base_is_plausible_link(slot, slot, hi));
        // Misaligned: never a frame base.
        assert!(!a5_frame_base_is_plausible_link(slot, slot + 0x84, hi));
        // Off the top of the stack, and the overflow edge of the same test.
        assert!(!a5_frame_base_is_plausible_link(slot, hi, hi));
        assert!(!a5_frame_base_is_plausible_link(slot, usize::MAX - 4, hi));
        // A stale zero word is the single most common shapeless value.
        assert!(!a5_frame_base_is_plausible_link(slot, 0, hi));
    }

    #[test]
    fn a5_slot_must_be_aligned_and_inside_the_stack() {
        let hi = 0x7fff_0000_0000usize;
        assert!(a5_frame_base_is_plausible(0x7ffe_0000_0000, hi));
        assert!(!a5_frame_base_is_plausible(0x7ffe_0000_0004, hi)); // misaligned
        assert!(!a5_frame_base_is_plausible(hi, hi)); // at the top
        assert!(!a5_frame_base_is_plausible(0, hi)); // no room for slot - 8
    }

    /// H2-CID0 (2026-08-05) — the sequence the pre-fix memo got wrong.
    ///
    /// Stack grows down. Verify deep, RETURN shallow, descend again: the band
    /// the thread rose through was rewritten while it was up there, and an
    /// already-compiled method entered in that window leaves an unregistered
    /// JIT frame in it without changing `code_ranges` or pushing a guard. The
    /// old rule (`search_lo >= verified_lo`) reported that band clean forever.
    #[test]
    fn unreg_memo_rescans_the_band_a_returning_thread_rewrote() {
        let ranges = 7usize;
        let mut m = UnregMemo::new();

        // First check at depth 1000 must scan everything.
        assert_eq!(
            m.observe(1000, ranges, true),
            UnregScan::Detect { hi: None }
        );
        m.mark_clean(1000, ranges);

        // Still nested below 1000 and deeper: only the new band is unscanned.
        assert_eq!(
            m.observe(800, ranges, true),
            UnregScan::Detect { hi: Some(1000) },
        );
        m.mark_clean(800, ranges);

        // The thread RETURNS to 1900 — everything below that was popped.
        assert_eq!(m.observe(1900, ranges, true), UnregScan::AlreadyClean);

        // …and descends again to 1500. The band [1500, 1900) is stack the
        // thread rewrote after the last clean verdict, so it MUST be rescanned.
        assert_eq!(
            m.observe(1500, ranges, true),
            UnregScan::Detect { hi: Some(1900) },
            "a band rewritten while the thread was shallower must not inherit \
             an older clean verdict — this is the H2-CID0 hole",
        );

        // The pre-fix rule is what the kill switch restores, and it is wrong
        // here: 1500 >= verified_lo (800), so it short-circuits.
        let mut old = UnregMemo::new();
        old.mark_clean(800, ranges);
        assert_eq!(old.observe(1900, ranges, false), UnregScan::AlreadyClean);
        assert_eq!(
            old.observe(1500, ranges, false),
            UnregScan::AlreadyClean,
            "documents the defect the hi-water rule fixes",
        );
    }

    /// H2-CID0 (2026-08-05) — a reset memo vouches for nothing.
    ///
    /// `invalidate_scan_cache_for_gc` resets the memo so the GC-authoritative
    /// scan cannot inherit a verdict about a stack that has since been
    /// rewritten. The property that matters is that a freshly-reset memo
    /// demands a FULL rescan (`hi: None`) at any depth, not an incremental
    /// band — an incremental band would still trust the missing prefix.
    #[test]
    fn a_reset_memo_demands_a_full_rescan_at_any_depth() {
        let mut m = UnregMemo::new();
        m.mark_clean(1000, 5);
        assert_eq!(m.observe(1200, 5, true), UnregScan::AlreadyClean);

        m = UnregMemo::new();
        for depth in [10usize, 1000, 100_000, usize::MAX - 1] {
            assert_eq!(
                m.observe(depth, 5, true),
                UnregScan::Detect { hi: None },
                "a reset memo must force a full rescan at depth {depth}, not an \
                 incremental band over a prefix nothing has verified",
            );
        }
    }

    /// A new compilation invalidates the verdict regardless of depth — a slot
    /// that held a plain value can now sit inside a brand-new code range.
    ///
    /// The rule is conservative and was measured non-binding on 2026-08-11 —
    /// see `UnregMemo::observe` for the numbers and why it stays anyway.
    #[test]
    fn unreg_memo_new_code_range_forces_a_full_rescan() {
        let mut m = UnregMemo::new();
        m.mark_clean(1000, 3);
        assert_eq!(m.observe(1200, 3, true), UnregScan::AlreadyClean);
        assert_eq!(m.observe(1200, 4, true), UnregScan::Detect { hi: None });
    }

    /// The throughput case the incremental path exists for must be unchanged:
    /// a thread that only ever descends scans each new band exactly once and
    /// never rescans what it already covered.
    #[test]
    fn unreg_memo_monotonic_descent_is_unchanged_by_the_fix() {
        let ranges = 2usize;
        for hiwater_on in [false, true] {
            let mut m = UnregMemo::new();
            m.mark_clean(5000, ranges);
            for depth in [4000usize, 3000, 2000, 1000] {
                let prev = m.verified_lo;
                assert_eq!(
                    m.observe(depth, ranges, hiwater_on),
                    UnregScan::Detect { hi: Some(prev) },
                    "descending must scan only the newly-exposed band \
                     (hiwater_on={hiwater_on})",
                );
                m.mark_clean(depth, ranges);
            }
            // Re-checking at the same depth is free.
            assert_eq!(m.observe(1000, ranges, hiwater_on), UnregScan::AlreadyClean);
        }
    }

    // -----------------------------------------------------------------------
    // JIT-scan cache keying (vm-jit-cache-keying.md)
    // -----------------------------------------------------------------------

    fn filled_scan_cache(heap_id: usize) -> JitScanCache {
        JitScanCache {
            filled_gen: 7,
            chain_len: 3,
            heap_id,
            collection_count: 0,
            roots: Vec::new(),
        }
    }

    #[test]
    fn scan_cache_hits_for_the_heap_it_was_filled_from() {
        let c = filled_scan_cache(0x1000);
        assert!(c.matches(7, 3, 0x1000, 0));
    }

    #[test]
    fn scan_cache_does_not_hit_for_another_heap() {
        // Everything else identical — same boundary generation, same chain
        // depth, same collection count. Two young heaps trivially agree on
        // `collection_count` because it is a per-heap counter, so before
        // `heap_id` existed this WAS a hit, and VM A's raw object addresses
        // were extended into VM B's root vector.
        let c = filled_scan_cache(0x1000);
        assert!(
            !c.matches(7, 3, 0x2000, 0),
            "a scan filtered through one heap must never be replayed for another"
        );
    }

    #[test]
    fn scan_cache_still_rejects_a_stale_generation_or_collection() {
        let c = filled_scan_cache(0x1000);
        assert!(!c.matches(8, 3, 0x1000, 0), "boundary crossing invalidates");
        assert!(
            !c.matches(7, 4, 0x1000, 0),
            "chain depth change invalidates"
        );
        assert!(!c.matches(7, 3, 0x1000, 1), "a collection invalidates");
    }

    #[test]
    fn empty_scan_cache_matches_nothing_plausible() {
        let c = JitScanCache::empty();
        assert!(!c.matches(0, 0, 0, 0));
        // The sentinel row is only "equal" to itself, which no real call can
        // produce: `heap_id` is a live `&VmHeap` address, never `usize::MAX`.
        assert!(c.matches(u64::MAX, usize::MAX, usize::MAX, u64::MAX));
    }

    #[test]
    fn empty_chain_is_quiescent() {
        // No JIT entries pushed → any_thread_in_jit reflects only this
        // thread's state, so an empty chain on this thread + no other
        // thread in JIT == false. Other tests in the suite may briefly
        // push, so we only assert the local depth.
        assert_eq!(current_thread_jit_depth(), 0);
    }

    #[test]
    fn push_pop_round_trip() {
        let depth_before = current_thread_jit_depth();
        let depth_after_push = push_jit_entry();
        assert_eq!(depth_after_push, depth_before + 1);
        assert_eq!(current_thread_jit_depth(), depth_before + 1);
        let popped = pop_jit_entry();
        assert!(popped.is_some(), "pop must return the previously pushed sp");
        assert_eq!(current_thread_jit_depth(), depth_before);
    }

    #[test]
    fn guard_drop_heals_abandoned_nested_entries() {
        let depth_before = current_thread_jit_depth();
        let guard = JitEntryGuard::enter();
        assert_eq!(current_thread_jit_depth(), depth_before + 1);

        // Model an OSR/deopt non-local return that bypassed two nested bridge
        // guards. Dropping the outer guard must remove those descendants before
        // removing itself, leaving any pre-existing outer frames intact.
        let _ = push_jit_entry();
        let _ = push_jit_entry();
        assert_eq!(current_thread_jit_depth(), depth_before + 3);

        drop(guard);
        assert_eq!(current_thread_jit_depth(), depth_before);
    }

    #[test]
    fn nested_push_pop_lifo() {
        let depth_before = current_thread_jit_depth();
        let _g1 = JitEntryGuard::enter();
        let _g2 = JitEntryGuard::enter();
        let _g3 = JitEntryGuard::enter();
        assert_eq!(current_thread_jit_depth(), depth_before + 3);
        // Drops happen in reverse order at scope exit (g3 then g2 then g1)
    }

    #[test]
    fn guard_drops_on_panic_unwind() {
        let depth_before = current_thread_jit_depth();
        let result = std::panic::catch_unwind(|| {
            let _g = JitEntryGuard::enter();
            assert_eq!(current_thread_jit_depth(), depth_before + 1);
            panic!("deliberate test panic");
        });
        assert!(result.is_err());
        assert_eq!(
            current_thread_jit_depth(),
            depth_before,
            "guard must pop the chain even on panic unwind"
        );
    }

    #[test]
    fn prune_reclaims_returned_entries_keeps_live() {
        // Round-7 corruption fix: self-heal a LEAKED JitEntryGuard. Build a
        // chain with one genuinely-live frame (entry_sp ABOVE a chosen scanner
        // SP) and one provably-returned/leaked frame (entry_sp BELOW it).
        // Pruning at the scanner SP must reclaim exactly the returned one and
        // retain the live one. Assertions are on the THREAD-LOCAL chain only
        // (race-free), never the shared global counter.
        let depth_before = current_thread_jit_depth();
        let scanner_sp: usize = 0x10_0000;
        // leaked / already-returned frame: entry_sp strictly below scanner SP.
        push_jit_entry_at(scanner_sp - 0x1000);
        // genuinely-live frame: entry_sp at/above scanner SP.
        push_jit_entry_at(scanner_sp + 0x1000);
        assert_eq!(current_thread_jit_depth(), depth_before + 2);

        let pruned = prune_returned_jit_entries(scanner_sp);
        assert_eq!(
            pruned, 1,
            "exactly the returned (below-scanner) entry is reclaimed"
        );
        assert_eq!(
            current_thread_jit_depth(),
            depth_before + 1,
            "the genuinely-live entry is retained"
        );
        JIT_ENTRY_CHAIN.with(|c| {
            assert!(
                c.borrow().iter().all(|e| e.entry_sp >= scanner_sp),
                "no entry below the scanner SP survives the prune"
            );
        });
        // Restore balance (pop the live entry) so the global counters and the
        // chain return to their pre-test state for any sibling test.
        let _ = pop_jit_entry();
        assert_eq!(current_thread_jit_depth(), depth_before);
    }

    #[test]
    fn prune_never_removes_live_frames() {
        // Soundness guard: a chain where every entry is at/above the scanner
        // SP must be left completely untouched — a live frame is never a
        // false-positive prune target.
        let depth_before = current_thread_jit_depth();
        let scanner_sp: usize = 0x10_0000;
        push_jit_entry_at(scanner_sp); // exactly at SP counts as live (>=)
        push_jit_entry_at(scanner_sp + 0x2000); // above = live
        let pruned = prune_returned_jit_entries(scanner_sp);
        assert_eq!(
            pruned, 0,
            "live frames (entry_sp >= scanner_sp) are never pruned"
        );
        assert_eq!(current_thread_jit_depth(), depth_before + 2);
        let _ = pop_jit_entry();
        let _ = pop_jit_entry();
        assert_eq!(current_thread_jit_depth(), depth_before);
    }

    #[test]
    fn current_sp_is_in_caller_frame() {
        // After `#[inline(always)]`, `current_stack_pointer` is inlined
        // into this test function, so the probe variable lives in this
        // test's own frame. Both addresses are therefore in the same
        // frame and within a small constant of each other (< 256 bytes
        // is generous; in practice they are within a single cache line).
        let local: u8 = 0;
        let test_sp = &local as *const u8 as usize;
        let probe_sp = current_stack_pointer();
        let delta = test_sp.abs_diff(probe_sp);
        assert!(
            delta < 4096,
            "expected probe and test SPs in the same frame; \
             probe={:#x} test={:#x} delta={}",
            probe_sp,
            test_sp,
            delta
        );
    }

    #[test]
    fn scan_one_frame_inverted_range_is_noop() {
        // If high_sp <= low_sp, the scanner must do nothing.
        // We can't easily fabricate a real VmHeap in a unit test without
        // pulling the whole crate, so this test exercises the early-return
        // path indirectly via scan_active_jit_frames with an empty chain.
        // The richer end-to-end test lives in roots.rs (sees a real heap).
        assert_eq!(current_thread_jit_depth(), 0);
    }

    #[test]
    fn global_depth_tracks_pushes() {
        // NEW-11: the old version of this test asserted exact equality
        // on the process-wide GLOBAL_JIT_DEPTH counter, which is raced
        // by any other test that happens to push/pop between our
        // reads. Assert instead on the thread-local depth and on
        // `any_thread_in_jit()` observability.
        let local_before = current_thread_jit_depth();
        let _g = JitEntryGuard::enter();
        assert_eq!(current_thread_jit_depth(), local_before + 1);
        // At least one thread (this one) is in JIT — no race risk
        // because `any_thread_in_jit` collapses all threads to a bool.
        assert!(any_thread_in_jit());
        drop(_g);
        assert_eq!(current_thread_jit_depth(), local_before);
    }

    /// The moving-young gate must be ONE decision that all three layers see,
    /// not three independently-parsed ones that happen to agree.
    ///
    /// Effective gate = codegen *can* AND config *wants*; the collector then
    /// gets that answer published to it. If this ever fails, a default flip on
    /// one layer has re-introduced the skew that makes moving-young unsound.
    #[test]
    fn moving_young_gate_is_a_single_decision_across_all_three_layers() {
        let expected =
            cratonvm_jit::x64::moving_young_enabled() && cratonvm_types::flags().gc.moving_young;
        assert_eq!(
            moving_young_enabled(),
            expected,
            "the root gatherer must never decide independently — suppressing the \
             conservative backstop while the codegen emitted no precise map is \
             silent heap corruption",
        );
        // Reading the gate publishes it; the collector must now agree too.
        publish_moving_young_gate();
        assert_eq!(
            cratonvm_gc::gc_quiescence::moving_young_enabled(),
            expected,
            "the collector must relocate only when the codegen actually emitted \
             a rewritable root map AND the config asked for compaction",
        );
    }

    /// The codegen gate is a veto the config cannot override.
    ///
    /// The codegen decision remains authoritative: a config/codegen skew must
    /// NOT be able to switch the collector on behind frames that emitted no
    /// rewritable roots. Guards the exact mistake that would turn a default
    /// change into heap corruption.
    #[test]
    fn codegen_gate_vetoes_moving_young_regardless_of_config() {
        if !cratonvm_jit::x64::moving_young_enabled() {
            assert!(
                !moving_young_enabled(),
                "no shadow push/reload was ever emitted, so nothing may relocate \
                 behind a JIT frame no matter what the typed config says",
            );
        }
    }

    /// **A PIN DOES NOT DISCHARGE COVERAGE ON A COLLECTOR THAT CANNOT PIN.**
    ///
    /// The cross-thread handshake lets a moving cycle proceed while a peer
    /// holds compiled frames nobody proved rewritable, provided their
    /// conservative roots were pinned instead. That is sound exactly when the
    /// collector withholds a pinned object from relocation — G1 withholds the
    /// region, ZGC the page — and the generational young collector cannot: it
    /// is Cheney copying, from-space is reclaimed wholesale, and `gen_heap.rs`
    /// contains no reader of `pinned_jit_roots_snapshot()` at all. The pinned
    /// addresses reach it as ROOTS, which keeps them ALIVE and says nothing
    /// about keeping them PUT.
    ///
    /// Until 2026-09-06 the credit was granted anyway, and the peer resumed
    /// with a stale reference in a compiled frame: `-XX:+UseGenerationalGC`
    /// over `org.h2.test.jdbc.TestPreparedStatement` SIGSEGV'd in ten seconds,
    /// 5/5 with the credit and 0/5 without.
    #[test]
    fn a_pin_discharges_coverage_only_where_the_collector_honours_it() {
        // The whole point: everything else says yes, and the collector says no.
        assert!(
            !pinned_credit_admissible(true, false, false, false, true),
            "a pinned peer credited its depth on a collector that cannot \
             withhold the object -- this is the defect"
        );
        // The same row on a collector that CAN pin is the behaviour being kept.
        assert!(pinned_credit_admissible(true, false, true, false, true));
        // The override re-opens it deliberately, which is what makes the fix
        // testable by a positive control rather than by an absence of crashes.
        assert!(pinned_credit_admissible(true, false, false, true, true));
        // The three pre-existing conditions still each veto on their own, so
        // this change added a term rather than replacing one.
        assert!(!pinned_credit_admissible(false, false, true, false, true));
        assert!(!pinned_credit_admissible(true, true, true, false, true));
        assert!(!pinned_credit_admissible(true, false, true, false, false));
    }

    /// With moving-young explicitly opted out, the collection-authoritative
    /// refresh is a no-op that reports "proven" — the coverage machinery must
    /// not impose cost or verdicts on the compatibility path.
    #[test]
    fn collection_coverage_refresh_is_inert_when_moving_young_is_off() {
        if moving_young_enabled() {
            return; // validating a moving-young build; nothing to assert here
        }
        assert!(refresh_moving_young_coverage_for_collection(false));
        assert!(refresh_moving_young_coverage_for_current_thread());
    }

    /// The innermost-RBP mirror is the LIVE value; `PreciseFrameInfo::exact_rbp`
    /// is only a snapshot taken when an entry stops being top. A prune that
    /// removes nothing must therefore leave the mirror alone.
    ///
    /// This is the regression test for the defect that kept the moving young
    /// generation switched off for every JIT process: `prune_returned_jit_entries`
    /// reloaded the mirror from that snapshot unconditionally, so a no-op prune
    /// replaced the prologue's live RBP with the `0` the top entry has carried
    /// since `enter_with_compiled`. `refresh_moving_young_coverage_for_current_thread`
    /// prunes and *then* reads the mirror, so it destroyed its own input and
    /// reported MISSING_EXACT_RBP on 100% of cycles.
    #[test]
    fn a_no_op_prune_does_not_clobber_the_live_rbp_mirror() {
        let cm = dummy_compiled_method();
        // Deep enough that the prune's `entry_sp >= scanner_sp` test keeps it.
        let entry_sp = current_stack_pointer() + 4096;
        push_entry_full(JitFrameChainEntry {
            entry_sp,
            // No interpreter stack in this unit test; `INTERP_DEPTH_INHERIT`
            // is what a site that cannot name its depth pushes, and with an
            // empty chain it resolves to 0.
            interp_depth: INTERP_DEPTH_INHERIT,
            precise: Some(PreciseFrameInfo {
                compiled_method: &cm as *const cratonvm_jit::CompiledMethod,
                frame_base: entry_sp,
                entry_ptr: cm.entry_ptr(),
                // Exactly what `enter_with_compiled` stores: the prologue
                // publishes the real RBP into the mirror, never into here.
                exact_rbp: 0,
                // …and its identity travels with it, so an unpublished RBP
                // carries an unpublished id.
                exact_cm_id: 0,
            }),
        });

        // Stand in for the compiled prologue's `mov gs:[disp], rbp`.
        let live_rbp = entry_sp - 512;
        top_rbp_set(live_rbp);

        let pruned = prune_returned_jit_entries(current_stack_pointer());
        assert_eq!(
            pruned, 0,
            "the entry is above the scanner SP, so nothing returned"
        );
        assert_eq!(
            top_rbp_get(),
            live_rbp,
            "a prune that removed nothing must not reload the mirror from the \
             top entry's stale snapshot — that erases the only record of the \
             innermost frame base and fails the moving-young coverage proof",
        );

        // And the proof's own read path sees it.
        JIT_ENTRY_CHAIN.with(|c| {
            let mut v = c.borrow_mut();
            flush_top_rbp_cache_to_chain(v.as_mut_slice());
            assert_eq!(
                v.last()
                    .and_then(|e| e.precise.as_ref())
                    .map(|i| i.exact_rbp),
                Some(live_rbp),
            );
        });

        let _ = pop_jit_entry();
    }

    /// `other_thread_in_jit()` is the cross-thread proof obligation's trigger.
    /// The initiator's OWN frames are proven by its own scan and must never
    /// trip it; only depth the local chain cannot account for may.
    #[test]
    fn peer_detection_ignores_this_threads_own_jit_frames() {
        assert!(!peer_jit_frames_present(0, 0), "quiescent process");
        assert!(
            !peer_jit_frames_present(3, 3),
            "all live JIT frames belong to this thread — its own scan proves them",
        );
        assert!(
            peer_jit_frames_present(4, 3),
            "depth this thread's chain cannot account for means a peer is in JIT, \
             and a peer's registers/frame slots are not rewritable by this cycle",
        );
        assert!(
            peer_jit_frames_present(1, 0),
            "a peer while we are quiescent"
        );
    }

    /// The cross-thread handshake's arithmetic. The whole soundness argument is
    /// "accept only when the deposits account for EVERY peer JIT entry", so the
    /// shortfall case is the one that matters and it is asserted in both the
    /// one-short and the nothing-deposited shapes.
    #[test]
    fn peer_coverage_is_accepted_only_when_every_peer_entry_is_accounted_for() {
        assert!(
            peer_coverage_accounted(0, 0),
            "no peer depth to account for",
        );
        assert!(
            peer_coverage_accounted(3, 3),
            "three peer entries, three proven",
        );
        assert!(
            !peer_coverage_accounted(3, 2),
            "one peer entry unaccounted for must refuse the cycle — an OS-frozen \
             peer, or one blocked in a native with compiled frames below it, \
             deposits nothing and is exactly this shortfall",
        );
        assert!(
            !peer_coverage_accounted(1, 0),
            "a peer in JIT that deposited nothing must refuse",
        );
        assert!(
            peer_coverage_accounted(2, 5),
            "a peer that returned from JIT after depositing lowers peer_depth \
             without lowering the ledger; the proven set is then a superset",
        );
    }

    /// The ledger is cleared on the way IN to a pause, and a stale carry-over
    /// is the one state that could license a relocation nobody proved.
    #[test]
    fn beginning_a_coverage_cycle_clears_the_peer_ledger() {
        let _serialised = super::coverage_oracle_gate_tests::COVERAGE_ORACLE_TEST_LATCH
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        cratonvm_gc::gc_quiescence::add_peer_proven_jit_depth(7);
        assert_eq!(
            cratonvm_gc::gc_quiescence::peer_proven_jit_depth(),
            7,
            "the deposit did not land, so the clear below would prove nothing",
        );
        cratonvm_gc::gc_quiescence::begin_moving_young_coverage_cycle();
        assert_eq!(cratonvm_gc::gc_quiescence::peer_proven_jit_depth(), 0);
        // And the dedicated entry point the barrier calls, on its own.
        cratonvm_gc::gc_quiescence::add_peer_proven_jit_depth(4);
        assert_eq!(cratonvm_gc::gc_quiescence::peer_proven_jit_depth(), 4);
        cratonvm_gc::gc_quiescence::reset_peer_proven_jit_depth();
        assert_eq!(cratonvm_gc::gc_quiescence::peer_proven_jit_depth(), 0);
    }

    /// A thread with no JIT entries contributes nothing to `peer_depth`, so the
    /// peer half must return without paying for the band walk — and, more to
    /// the point, without depositing anything. A deposit from a chain-less
    /// thread would be a claim about frames that do not exist.
    #[test]
    fn a_thread_with_no_jit_frames_deposits_nothing() {
        assert_eq!(current_thread_jit_depth(), 0, "test precondition");
        cratonvm_gc::gc_quiescence::reset_peer_proven_jit_depth();
        publish_peer_jit_coverage_for_stw();
        assert_eq!(cratonvm_gc::gc_quiescence::peer_proven_jit_depth(), 0);
    }

    fn dummy_compiled_method() -> cratonvm_jit::CompiledMethod {
        cratonvm_jit::CompiledMethod::new(
            cratonvm_jit::ExecutableBuffer::new(64)
                .expect("executable buffer alloc must succeed in tests"),
        )
    }

    /// The two innermost-frame mirrors move TOGETHER across every entry-chain
    /// mutation, or the pair names two different frames.
    ///
    /// `top_cm_id_mirror_read`'s own doc states the rule: "restoring one
    /// without the other is how the identity becomes actively wrong rather than
    /// merely absent", and the scan cannot detect it because both halves still
    /// read consistently out of the mirrors. Until 2026-08-24 the chain
    /// bookkeeping broke it in both directions — `push_entry_full` zeroed only
    /// the RBP for the incoming entry, and `reload_top_rbp_cache` restored only
    /// the RBP on pop.
    ///
    /// That is what `bug-h2-testkillprocess-zgc-oom-at-97-percent-free-20260821`
    /// saw as `ACTIVE_FRAME_MAP` / `frame_cov=(no_map=N …)`: ONE rbp claimed by
    /// four different methods across collections, one of them
    /// `RootReference.isLocked` with `maps=0` — a method that emits no
    /// safepoint and therefore cannot be the frame at a collection.
    /// `refresh_moving_young_coverage_for_current_thread` is what makes it the
    /// proof's own input: it calls `prune_returned_jit_entries` first, which
    /// reloads the mirror, then stamps the snapshot from BOTH mirrors.
    ///
    /// Asserted through the public push/pop entry points rather than on the
    /// private helpers, because the invariant is about what a JIT boundary
    /// leaves behind, not about one function.
    #[test]
    fn both_frame_record_mirrors_move_together_across_a_jit_boundary() {
        // A distinctive pair, so a stale value cannot be mistaken for a fresh
        // one. The RBP must be 8-aligned and non-zero to survive the readers.
        const RBP_A: usize = 0x7fff_0000_0000_1000;
        const ID_A: u32 = 0xABCD_1234;

        // Entries carrying PRECISE info, because the snapshot fields the pop
        // restores from live there — a bare `push_jit_entry()` has none, and
        // reloading from it correctly yields zero for both halves.
        let cm = dummy_compiled_method();
        let _outer = JitEntryGuard::enter_with_compiled(&cm);
        top_rbp_mirror_write(RBP_A);
        top_cm_id_mirror_write(ID_A);
        assert_eq!(top_rbp_mirror_read(), RBP_A, "mirror write must land");
        assert_eq!(top_cm_id_mirror_read(), ID_A, "identity write must land");

        // Pushing a nested entry makes the outer one stop being top. The
        // incoming entry is unrecorded, so BOTH halves must read as such —
        // zeroing only the RBP leaves `ID_A` naming the frame BELOW.
        let inner_cm = dummy_compiled_method();
        let inner = JitEntryGuard::enter_with_compiled(&inner_cm);
        assert_eq!(
            top_rbp_mirror_read(),
            0,
            "incoming entry must start with no rbp"
        );
        assert_eq!(
            top_cm_id_mirror_read(),
            0,
            "incoming entry must start with no IDENTITY either — a leftover id \
             pairs the new entry's rbp with the old entry's method"
        );

        // Popping restores the outer entry's snapshot. Both halves, or the
        // restored rbp is paired with whatever ran in between.
        drop(inner);
        assert_eq!(top_rbp_mirror_read(), RBP_A, "pop must restore the rbp");
        assert_eq!(
            top_cm_id_mirror_read(),
            ID_A,
            "pop must restore the IDENTITY that rbp was snapshotted with"
        );
    }

    /// The control for the test above: it must be able to FAIL.
    ///
    /// With `CRATONVM_GC_NO_CM_ID_PAIRING=1` the identity half stops moving, so
    /// running the suite under that variable turns the two identity assertions
    /// red. This test asserts only what holds in BOTH arms — the rbp half,
    /// which was always correct — so a future change that breaks the rbp
    /// bookkeeping cannot hide behind the pairing switch.
    #[test]
    fn the_rbp_mirror_half_is_restored_regardless_of_the_pairing_switch() {
        const RBP_B: usize = 0x7fff_0000_0000_2000;
        let cm = dummy_compiled_method();
        let _outer = JitEntryGuard::enter_with_compiled(&cm);
        top_rbp_mirror_write(RBP_B);
        let inner_cm = dummy_compiled_method();
        let inner = JitEntryGuard::enter_with_compiled(&inner_cm);
        assert_eq!(top_rbp_mirror_read(), 0);
        drop(inner);
        assert_eq!(top_rbp_mirror_read(), RBP_B);
    }

    fn add_shadow_osr_layout(cm: &mut cratonvm_jit::CompiledMethod) {
        cm.compiled_via_osr = true;
        cm.shadow_thread_slot_off = 8;
        cm.shadow_savetop_slot_off = 16;
        cm.shadow_off_in_thread = 24;
    }

    // -----------------------------------------------------------------
    // Moving-young coverage VERIFICATION (arch-2026-07-26
    // `moving-young-corruption-rootcause`)
    // -----------------------------------------------------------------

    /// THE regression test for this item.
    ///
    /// A live compiled frame must never be able to hold a relocatable
    /// reference that is neither published on the shadow stack (rewritable)
    /// nor covered by the conservative scan (which moving-young suppresses).
    /// The codegen's `moving_young_coverage_complete` bit cannot answer this:
    /// it is computed from the abstract interpreter's locals + operand stack
    /// only, while the frame also carries scalar-replacement field slots, LICM
    /// hoist slots and the blind full-GPR safepoint spill area. The band scan
    /// is what closes that gap, so it must FAIL when such a word is present.
    /// A movable word in a DEAD java-local slot is not a root, and must not
    /// refuse the collection.
    ///
    /// The band scan cannot tell live from dead on its own, so before
    /// 2026-08-26 it demanded that every movable word in a java-local or
    /// operand-spill slot be published on the shadow stack. For the regions the
    /// abstract interpreter MODELS that is stricter than the GC requires: the
    /// safepoint's oop map already states which of those slots hold live
    /// references at this exact pc, and a slot it does not name holds whatever
    /// an earlier scope left there.
    ///
    /// Measured on `bug-h2-testkillprocess-zgc-oom-at-97-percent-free-20260821`:
    /// ALL 74 unpublished words of one run reported `in_map=false`, 64 of them
    /// in `MVStore.closeStore:(ZI)V` at `sp_id=174`, with offsets 48/56/64/72
    /// (locals 5..8) all holding the SAME object. `closeStore`'s
    /// `LocalVariableTable` scopes slot 5 (`map`) to 149..170, so at bci 174 it
    /// is out of scope and 6..8 are javac's loop/finally copies of it. The
    /// dataflow, the map and the shadow push all agreed; only the band scan
    /// objected, and it objected to dead words.
    ///
    /// The exemption is deliberately narrow — see the sibling test for the
    /// regions it must NOT apply to.
    #[test]
    fn a_dead_java_local_is_not_a_root_the_band_scan_may_refuse_on() {
        let mut layout = cratonvm_jit::FrameLayout::default();
        layout.java_locals_hi = 80;
        layout.locals_hi = 80;

        let live = 16i32;
        let dead = 72i32;
        // The map names ONE of the two local slots at this pc.
        let mut map_slots = std::collections::HashSet::new();
        map_slots.insert(live);

        assert!(
            band_slot_is_verifiable_with_map(live, &layout, None, Some(&map_slots)),
            "a local the map DOES name is live and must stay verifiable"
        );
        assert!(
            !band_slot_is_verifiable_with_map(dead, &layout, None, Some(&map_slots)),
            "a java-local the map does not name is dead; refusing on it refuses \
             a collection over a word nothing will read"
        );
        // Fail closed when no map resolved: without a liveness statement the
        // scan must assume nothing is dead.
        assert!(
            band_slot_is_verifiable_with_map(dead, &layout, None, None),
            "no map = no liveness statement = every word stays verifiable"
        );
    }

    /// The exemption applies ONLY to regions the abstract interpreter models.
    ///
    /// LICM hoist slots, scalar-replacement fields and the reserved-locals tail
    /// are precisely what the map has no opinion about, and closing that gap is
    /// the whole reason the band scan exists
    /// (`frame_band_scan_rejects_a_relocatable_word_the_shadow_stack_never_published`).
    /// Letting an empty map excuse those would delete the check.
    #[test]
    fn the_dead_slot_exemption_does_not_reach_regions_the_map_cannot_describe() {
        let mut layout = cratonvm_jit::FrameLayout::default();
        layout.java_locals_hi = 32;
        layout.locals_hi = 32;
        layout.scalar_lo = 32;
        layout.scalar_hi = 48;
        layout.ref_hoist_lo = 48;
        layout.ref_hoist_hi = 64;

        // An EMPTY map: the dataflow named no live slot at this pc.
        let empty: std::collections::HashSet<i32> = std::collections::HashSet::new();
        for off in [40i32, 56i32] {
            assert!(
                band_slot_is_verifiable_with_map(off, &layout, None, Some(&empty)),
                "off={off} ({}) is not modelled by the abstract interpreter, so an \
                 empty map says nothing about it and it must stay verifiable",
                layout.region_name(off)
            );
        }
        // …while a java-local at the same empty map IS excused.
        assert!(
            !band_slot_is_verifiable_with_map(16, &layout, None, Some(&empty)),
            "a modelled region with an empty map is dead"
        );
    }

    /// Backing store for a band word that `is_relocatable` will vouch for.
    ///
    /// `band_word_is_an_object` DEREFERENCES any word the predicate accepts;
    /// that is its documented contract, and in production the predicate is a
    /// published-movable-range test, so the header is mapped. Three band
    /// tests passed a bare integer (`0xdead_0000`) plus a closure claiming it
    /// was relocatable, which breaks that precondition and reads unmapped
    /// memory -- measured as `EXCEPTION_ACCESS_VIOLATION ... read at address
    /// 0x00000000DEAD0008`, deterministic and reproducible in isolation. The
    /// object screen (audit 16-18) was added after these tests were written.
    ///
    /// 32 bytes is `HEADER_SIZE`, which bounds what
    /// `plausible_object_header_at` may read; the header is a plain `Object`
    /// with one slot so the screen answers TRUE, which is what these tests
    /// assume when they assert that a relocatable word diverts the cycle.
    struct BandOop {
        store: Box<[u64; 4]>,
    }

    impl BandOop {
        fn new() -> Self {
            let mut store: Box<[u64; 4]> = Box::new([0; 4]);
            // SAFETY: `store` is 32 bytes, 8-aligned and exclusively owned
            // here, so an `ObjectHeader` fits and is properly aligned.
            unsafe {
                std::ptr::write(
                    store.as_mut_ptr() as *mut cratonvm_types::ObjectHeader,
                    cratonvm_types::ObjectHeader::new(
                        cratonvm_types::ClassId::new(1),
                        cratonvm_types::ObjectKind::Object,
                        cratonvm_types::ArrayElementType::Reference,
                        0,
                        1,
                    ),
                );
            }
            Self { store }
        }

        fn addr(&self) -> usize {
            self.store.as_ptr() as usize
        }
    }

    fn frame_band_scan_rejects_a_relocatable_word_the_shadow_stack_never_published() {
        // A synthetic compiled-frame spill band. `hoisted` stands for any of
        // the three storage classes outside the local/operand model.
        let published_store = BandOop::new();
        let hoisted_store = BandOop::new();
        let published_oop = published_store.addr();
        let hoisted_oop = hoisted_store.addr();
        let band: Vec<usize> = vec![7, published_oop, 0x1234_5678, hoisted_oop, 0];
        let lo = band.as_ptr() as usize;
        let hi = lo + band.len() * 8;

        let relocatable = move |w: usize| w == published_oop || w == hoisted_oop;

        let flat = cratonvm_jit::FrameLayout::default();

        let mut published = std::collections::HashSet::new();
        published.insert(published_oop);
        assert!(
            band_has_unpublished_word_with(hi, hi - lo, &flat, None, &published, &relocatable),
            "a young-resident word absent from the shadow window is un-rewritable: \
             the cycle MUST NOT relocate",
        );

        published.insert(hoisted_oop);
        assert!(
            !band_has_unpublished_word_with(hi, hi - lo, &flat, None, &published, &relocatable),
            "once every relocatable word in the band is on the shadow stack, the \
             frame really is fully covered and the moving cycle may proceed",
        );
    }

    /// A word that is only an IMAGE of a register — the prologue's callee-saved
    /// save area, or the write-only per-safepoint blind GPR spill — is not
    /// independent oop storage, and both areas are routinely full of DEAD
    /// register values. Scanning them reports "an oop was missed" on frames
    /// that are in fact fully covered, which costs every moving cycle. That is
    /// the measured reason moving-young engaged zero times on bt18.
    #[test]
    fn frame_band_scan_skips_register_images() {
        let stale_store = BandOop::new();
        let stale_oop = stale_store.addr();
        // [0] = a genuine slot, [1..3] = a register-image band, [4] = genuine.
        let band: Vec<usize> = vec![0, stale_oop, stale_oop, stale_oop, 0];
        let lo = band.as_ptr() as usize;
        let hi = lo + band.len() * 8;
        let published = std::collections::HashSet::new();
        let relocatable = move |w: usize| w == stale_oop;

        // `off` counts back from `rbp` (= `hi`): band[4] is off 8, band[0] off 40.
        let images = cratonvm_jit::FrameLayout {
            reg_spill_lo: 16,
            reg_spill_hi: 40,
            ..Default::default()
        };
        assert!(
            !band_has_unpublished_word_with(hi, hi - lo, &images, None, &published, &relocatable),
            "every relocatable word lives in the register-image band, so the frame \
             proves nothing against it and the cycle may still relocate",
        );
        assert!(
            band_has_unpublished_word_with(
                hi,
                hi - lo,
                &cratonvm_jit::FrameLayout::default(),
                None,
                &published,
                &relocatable
            ),
            "without the exclusion the very same band diverts — this is the \
             false-positive floor the exclusion removes",
        );
    }


    /// A SHALLOW callee-save area must not blind the verifier to the spill area.
    ///
    /// x86-64 puts the caller's saved registers at the DEEP end of the frame,
    /// which is why `callee_saved_lo` can be read as a half-line: everything at
    /// or beyond it is a register image or past the frame. AArch64's prologue
    /// puts the saved FP/LR pair and the callee-saved GPRs immediately below the
    /// frame pointer, with the spill area BELOW those -- so the same half-line
    /// would exclude every spill word, which is exactly where that backend's
    /// oop maps point. The verifier would then inspect nothing and report a
    /// clean frame: a vacuous green, and the worst possible answer from a
    /// completeness oracle.
    ///
    /// `callee_saved_shallow` selects the range exclusion instead. Both arms are
    /// asserted on ONE layout so the difference is the flag and nothing else.
    #[test]
    fn a_shallow_callee_save_area_keeps_the_spill_region_verifiable() {
        // Register images at [FP-8..FP-24); this frame's spill words deeper, at
        // offsets 32 and 40.
        let mut shallow = cratonvm_jit::FrameLayout {
            callee_saved_lo: 8,
            callee_saved_hi: 24,
            spill_lo: 32,
            spill_hi: 48,
            ..Default::default()
        };
        shallow.callee_saved_shallow = true;

        for off in [32, 40] {
            assert!(
                band_slot_is_verifiable(off, &shallow, None),
                "spill word at {off} must stay verifiable on a shallow-save frame"
            );
        }
        // ...and the register images are still excluded, by the RANGE.
        for off in [8, 16] {
            assert!(
                !band_slot_is_verifiable(off, &shallow, None),
                "the caller's saved registers at {off} are not this frame's words"
            );
        }

        // The control: the same layout read with x86-64 geometry hides both
        // spill words behind the half-line.
        let deep = cratonvm_jit::FrameLayout {
            callee_saved_shallow: false,
            ..shallow
        };
        for off in [32, 40] {
            assert!(
                !band_slot_is_verifiable(off, &deep, None),
                "the half-line is what would have swallowed the spill area"
            );
        }
    }
    /// The scan must not divert on a frame full of primitives — otherwise
    /// moving-young could never engage at all and the fix would be a disguised
    /// default-off landing.
    /// The operand-spill reserve is sized for `max_stack` and reclaimed by
    /// moving a cursor, never by clearing. A slot above the cursor therefore
    /// holds a stale object pointer from a deeper earlier stack — dead, never
    /// read again, and the measured reason bt18 reported an unpublished oop on
    /// every collection while being fully covered.
    #[test]
    fn frame_band_scan_ignores_reclaimed_spill_slots() {
        let stale_store = BandOop::new();
        let live_store = BandOop::new();
        let stale_oop = stale_store.addr();
        let live_oop = live_store.addr();
        let band: Vec<usize> = vec![0, stale_oop, live_oop, 0, 0];
        let lo = band.as_ptr() as usize;
        let hi = lo + band.len() * 8;
        let published = std::collections::HashSet::new();
        let relocatable = move |w: usize| w == stale_oop || w == live_oop;
        // `off` counts back from `rbp` (= `hi`): band[4] is off 8 ... band[0]
        // off 40. Make the whole band the operand-spill region.
        let layout = cratonvm_jit::FrameLayout {
            spill_lo: 8,
            spill_hi: 48,
            callee_saved_lo: 48,
            ..Default::default()
        };

        // Cursor at 32 => band[1] (off 32, the stale word) is reclaimed,
        // band[2] (off 24, the live word) is not.
        assert!(
            band_has_unpublished_word_with(
                hi,
                hi - lo,
                &layout,
                Some(32),
                &published,
                &relocatable
            ),
            "a LIVE unpublished spill slot must still divert the cycle",
        );
        let mut published = published;
        published.insert(live_oop);
        assert!(
            !band_has_unpublished_word_with(
                hi,
                hi - lo,
                &layout,
                Some(32),
                &published,
                &relocatable
            ),
            "with the live slot published, the reclaimed slot above the cursor \
             must not divert: nothing reads it again",
        );
        assert!(
            band_has_unpublished_word_with(hi, hi - lo, &layout, None, &published, &relocatable),
            "an unknown cursor must fall back to scanning the whole region",
        );
    }

    /// The register-image REWRITE covers exactly one of the four regions the
    /// band verifier refuses, and this test is the partition.
    ///
    /// `band_slot_is_verifiable` says "no" to everything at or above
    /// `callee_saved_lo` — the caller's GPR image, the caller's XMM image, the
    /// per-safepoint blind GPR spill, and the outgoing-argument / deopt
    /// reserve above it. Only the FIRST is read by anything (the epilogue pops
    /// it into the caller's registers) AND capable of holding a reference, and
    /// widening the write back to the others is what made this repair look
    /// unsafe enough to ship off. A future edit that re-widens it fails here.
    #[test]
    fn only_the_callee_saved_gpr_image_is_rewritten() {
        let layout = cratonvm_jit::FrameLayout {
            java_locals_hi: 16,
            spill_lo: 16,
            spill_hi: 48,
            callee_saved_lo: 48,
            callee_saved_hi: 80,
            xmm_saved_lo: 80,
            xmm_saved_hi: 112,
            reg_spill_lo: 112,
            reg_spill_hi: 144,
            frame_size: 176,
            ..Default::default()
        };
        // Verified storage: rewritten by `remap_one_jit_frame`, not here.
        for off in [8, 16, 40] {
            assert!(
                band_slot_is_verifiable(off, &layout, None),
                "off={off} must be verified storage"
            );
            assert!(
                !is_callee_saved_gpr_image(off, &layout),
                "off={off} is verified storage and must not be rewritten here"
            );
        }
        // The one region something resumes from.
        for off in [48, 64, 72] {
            assert!(
                !band_slot_is_verifiable(off, &layout, None),
                "off={off} is a register image, so the verifier must skip it"
            );
            assert!(
                is_callee_saved_gpr_image(off, &layout),
                "off={off} is the callee-saved GPR image and must be rewritten"
            );
        }
        // Unverifiable AND unread: an XMM never holds a reference, the
        // per-safepoint spill is write-only, and everything above it is dead.
        for off in [80, 100, 112, 140, 152] {
            assert!(
                !band_slot_is_verifiable(off, &layout, None),
                "off={off} must be unverifiable"
            );
            assert!(
                !is_callee_saved_gpr_image(off, &layout),
                "off={off} is region `{}` — nothing resumes from it, so writing \
                 it can only corrupt",
                layout.region_name(off),
            );
        }
        // A frame with no save area at all: nothing to rewrite, and the
        // predicate must not admit an offset by an empty-range accident.
        let flat = cratonvm_jit::FrameLayout::default();
        for off in [0, 8, 64] {
            assert!(!is_callee_saved_gpr_image(off, &flat));
        }
    }

    /// The pin veto outranks the movable claim, and only for what it names.
    ///
    /// `is_movable_jit_root` is a claim about ONE slot while the pin set is
    /// keyed by OBJECT, so without the veto a single rewritable channel
    /// licensed moving an object out from under every unrewritable word that
    /// also held it. The per-pass clear is the other half: a stale entry would
    /// over-pin forever.
    #[test]
    fn the_unrewritable_veto_outranks_a_movable_claim() {
        use cratonvm_gc::gc_quiescence as q;
        q::clear_movable_jit_roots();
        q::clear_unrewritable_jit_roots();
        let movable_only = 0x1000usize;
        let both = 0x2000usize;
        q::add_movable_jit_root(movable_only);
        q::add_movable_jit_root(both);
        q::add_unrewritable_jit_root(both);

        assert!(q::is_movable_jit_root(movable_only));
        assert!(!q::is_unrewritable_jit_root(movable_only));
        assert!(q::is_movable_jit_root(both));
        assert!(
            q::is_unrewritable_jit_root(both),
            "an address held in an unrewritable frame word must be vetoed even \
             though a rewritable channel also names it"
        );
        assert_eq!(q::unrewritable_jit_root_count(), 1);

        q::clear_unrewritable_jit_roots();
        assert!(
            !q::is_unrewritable_jit_root(both),
            "the veto is a statement about THIS collection's frames"
        );
        assert_eq!(q::unrewritable_jit_root_count(), 0);
        q::clear_movable_jit_roots();
    }

    #[test]
    fn frame_band_scan_ignores_words_outside_the_young_regions() {
        let band: Vec<usize> = vec![0, 1, u64::MAX as usize, 42, 0x7fff_ffff];
        let lo = band.as_ptr() as usize;
        let hi = lo + band.len() * 8;
        let published = std::collections::HashSet::new();
        assert!(!band_has_unpublished_word_with(
            hi,
            hi - lo,
            &cratonvm_jit::FrameLayout::default(),
            None,
            &published,
            |_| { false }
        ));
    }

    #[test]
    fn frame_band_scan_handles_an_empty_or_inverted_band() {
        let published = std::collections::HashSet::new();
        let flat = cratonvm_jit::FrameLayout::default();
        // A frame size larger than RBP itself cannot name a real band.
        assert!(!band_has_unpublished_word_with(
            0x1000,
            0x2000,
            &flat,
            None,
            &published,
            |_| true
        ));
        assert!(!band_has_unpublished_word_with(
            0x1000,
            0,
            &flat,
            None,
            &published,
            |_| true
        ));
    }

    /// A synthetic `JvmThread`-shaped buffer whose `ShadowStack` sits at
    /// `shadow_off_in_thread`, plus a frame that caches a pointer to it in
    /// `[rbp - shadow_thread_slot_off]` — exactly the two indirections the
    /// verifier uses to recover a thread's published window from a frame.
    #[allow(dead_code)] // the three buffers exist to keep the addresses alive
    struct FakeShadowThread {
        _thread: Box<[usize]>,
        _slots: Box<[usize]>,
        _frame: Box<[usize]>,
        rbp: usize,
        cm: cratonvm_jit::CompiledMethod,
    }

    /// Build a frame + `JvmThread` + `ShadowStack` triple satisfying the EXACT
    /// invariant `shadow_window_from_frame` checks.
    ///
    /// That matters more than it looks. This fixture used to allocate a buffer
    /// only `values.len()` slots wide and alias `end` to `top`, which was fine
    /// while the resolver merely required non-null 8-aligned words. It is not
    /// fine since the SIGSEGV hardening (a `base` of `0x5555_0000_0004` walked
    /// as a shadow window): the resolver now demands the real
    /// `ShadowStack::ensure_allocated` shape — `end - base` EXACTLY
    /// `DEFAULT_SHADOW_SLOTS * 8`, `top` inside `[base, end]`. A three-slot
    /// buffer fails that, so every test built on this fixture was resolving
    /// `None`:
    ///
    ///   * `shadow_window_is_recovered_from_a_live_compiled_frame` failed
    ///     outright (`window must resolve`) — that is how this was found;
    ///   * `nulled_thread_slot_publishes_nothing_rather_than_proving_everything`,
    ///     `shadow_window_is_unresolvable_without_the_layout_offsets`,
    ///     `shadow_window_rejects_an_unaligned_base` and
    ///     `shadow_window_rejects_a_window_wider_than_the_backing_buffer`
    ///     all still PASSED — vacuously. Each asserts `is_none()`, and the
    ///     fixture was already `None` before the mutation each one applies.
    ///     Four tests were asserting nothing.
    ///
    /// The buffer is now full-size and `end` is derived rather than aliased;
    /// `values` occupy the published prefix `[base, top)`.
    fn fake_shadow_thread(values: &[usize], thread_is_null: bool) -> FakeShadowThread {
        const SHADOW_OFF_IN_THREAD: usize = 24;
        const THREAD_SLOT_OFF: usize = 8;
        use cratonvm_gc::shadow_stack::DEFAULT_SHADOW_SLOTS;

        assert!(
            values.len() <= DEFAULT_SHADOW_SLOTS,
            "fixture cannot publish more slots than the real buffer holds"
        );
        let mut slots: Box<[usize]> = vec![0usize; DEFAULT_SHADOW_SLOTS].into_boxed_slice();
        slots[..values.len()].copy_from_slice(values);
        let base = slots.as_ptr() as usize;
        let top = base + values.len() * 8;
        let end = base + DEFAULT_SHADOW_SLOTS * 8;

        // thread[0..] with a ShadowStack {top, end, base} at byte offset 24.
        let mut thread: Box<[usize]> = vec![0usize; 8].into_boxed_slice();
        let ss = SHADOW_OFF_IN_THREAD / 8;
        thread[ss] = top; // TOP_OFFSET  = 0
        thread[ss + 1] = end; // END_OFFSET  = 8
        thread[ss + 2] = base; // BASE_OFFSET = 16

        // frame[..] with the cached thread pointer at [rbp - 8].
        let mut frame: Box<[usize]> = vec![0usize; 4].into_boxed_slice();
        frame[0] = if thread_is_null {
            0
        } else {
            thread.as_ptr() as usize
        };
        let rbp = frame.as_ptr() as usize + THREAD_SLOT_OFF;

        let mut cm = dummy_compiled_method();
        cm.shadow_thread_slot_off = THREAD_SLOT_OFF as i32;
        cm.shadow_off_in_thread = SHADOW_OFF_IN_THREAD as i32;

        FakeShadowThread {
            _thread: thread,
            _slots: slots,
            _frame: frame,
            rbp,
            cm,
        }
    }

    #[test]
    fn shadow_window_is_recovered_from_a_live_compiled_frame() {
        let f = fake_shadow_thread(&[0x1111, 0x2222, 0x3333], false);
        let window = shadow_window_from_frame(f.rbp, &f.cm).expect("window must resolve");
        let published = published_shadow_values(Some(window));
        assert_eq!(published.len(), 3);
        for v in [0x1111usize, 0x2222, 0x3333] {
            assert!(
                published.contains(&v),
                "value {v:#x} must count as published"
            );
        }
    }

    /// `maybe_nop_out_shadow_fetch` (jit/src/x64.rs) overwrites the prologue's
    /// `get_current_thread` sequence with a JMP-over whenever a method emitted
    /// no shadow push, leaving `[rbp - shadow_thread_slot_off]` NULL. At
    /// runtime every push and reload in that method is then skipped by their
    /// null guards — while its oop maps still carry
    /// `moving_young_coverage_complete = true`. So a null thread slot means
    /// "this frame published NOTHING", and the verifier must treat it as an
    /// empty set rather than as "nothing to check": any relocatable word in
    /// such a frame's band is then correctly un-rewritable.
    #[test]
    fn nulled_thread_slot_publishes_nothing_rather_than_proving_everything() {
        let f = fake_shadow_thread(&[0x1111, 0x2222], true);
        assert!(
            shadow_window_from_frame(f.rbp, &f.cm).is_none(),
            "a NOP'd-out prologue fetch leaves the cached thread pointer null",
        );
        let published = published_shadow_values(None);
        assert!(published.is_empty());

        // Header-backed, for the same reason as the three band tests above:
        // `band_word_is_an_object` screens the word the closure vouches for,
        // and `0x1111` fails that screen on ALIGNMENT before it is ever
        // dereferenced -- so this assertion had been failing on dev, hidden
        // behind the crash in `frame_band_scan_ignores_reclaimed_spill_slots`
        // that aborted the run before it was reached.
        let live = BandOop::new();
        let live_oop = live.addr();
        let band: Vec<usize> = vec![live_oop];
        let lo = band.as_ptr() as usize;
        assert!(
            band_has_unpublished_word_with(
                lo + 8,
                8,
                &cratonvm_jit::FrameLayout::default(),
                None,
                &published,
                |w| w == live_oop
            ),
            "a frame that published nothing cannot prove coverage of a live oop",
        );
    }

    #[test]
    fn shadow_window_is_unresolvable_without_the_layout_offsets() {
        let mut f = fake_shadow_thread(&[0x1111], false);
        f.cm.shadow_thread_slot_off = 0;
        assert!(shadow_window_from_frame(f.rbp, &f.cm).is_none());
        f.cm.shadow_thread_slot_off = 8;
        f.cm.shadow_off_in_thread = 0;
        assert!(shadow_window_from_frame(f.rbp, &f.cm).is_none());
    }

    /// A frame whose `CompiledMethod` does not describe it (the
    /// FOREIGN_INNERMOST_RBP case: an RBP published by a compiled callee
    /// reached through the inline MIC/PIC cascade) hands
    /// `shadow_window_from_frame` an arbitrary aligned stack word in place of
    /// the cached `*mut JvmThread`, so `base`/`top` are read out of something
    /// that is not a `ShadowStack`. The verifier SIGSEGV'd walking such a
    /// window from a `base` of `0x5555_0000_0004`; an unaligned `base` cannot
    /// address 8-byte shadow slots and must be rejected, not walked.
    #[test]
    fn shadow_window_rejects_an_unaligned_base() {
        let mut f = fake_shadow_thread(&[0x1111, 0x2222], false);
        let ss = 24 / 8; // SHADOW_OFF_IN_THREAD / 8, BASE_OFFSET = 16
        f._thread[ss + 2] += 4;
        assert!(
            shadow_window_from_frame(f.rbp, &f.cm).is_none(),
            "an unaligned base is not a shadow window",
        );
    }

    /// Same failure mode, caught by size instead of alignment: the shadow
    /// buffer never holds more than `DEFAULT_SHADOW_SLOTS` slots, so a wider
    /// `[base, top)` proves the pair did not come from a `ShadowStack`.
    /// Clamping the walk (which is all `published_shadow_values` used to do)
    /// still reads up to 2 MiB from an address that was never mapped.
    #[test]
    fn shadow_window_rejects_a_window_wider_than_the_backing_buffer() {
        let mut f = fake_shadow_thread(&[0x1111], false);
        let ss = 24 / 8; // SHADOW_OFF_IN_THREAD / 8, TOP_OFFSET = 0
        f._thread[ss] =
            f._thread[ss + 2] + (cratonvm_gc::shadow_stack::DEFAULT_SHADOW_SLOTS + 1) * 8;
        assert!(
            shadow_window_from_frame(f.rbp, &f.cm).is_none(),
            "a window wider than the buffer is not a shadow window",
        );
    }

    /// The direct self-call decoder is what distinguishes "a recursive
    /// activation of the method this entry names" (describable, and the shape
    /// every recursive Java workload produces) from "some other method's frame"
    /// (not describable — stay foreign, take the non-moving sweep).
    ///
    /// It reads machine code, so it is pinned against hand-built bytes rather
    /// than against whichever direct-call features are gated off today.
    #[test]
    fn direct_self_call_return_is_recognised_only_for_a_real_e8_to_the_entry() {
        // 16 bytes of "method body": a 5-byte `E8 rel32` at offset 6 that
        // targets offset 0, so the return address is entry + 11.
        let mut body = [0x90u8; 16];
        let entry = body.as_ptr() as usize;
        let call_at = 6usize;
        let ret = entry + call_at + 5;
        // rel32 = target - next_instruction = entry - ret
        let rel = (entry as isize - ret as isize) as i32;
        body[call_at] = 0xE8;
        body[call_at + 1..call_at + 5].copy_from_slice(&rel.to_le_bytes());

        assert!(
            returned_from_direct_self_call(ret, entry),
            "E8 whose displacement resolves to the method entry IS a self-call",
        );
        assert!(
            !returned_from_direct_self_call(ret, entry + 8),
            "the same instruction targeting a different entry is not a self-call",
        );
        // An indirect call (`FF /2`, e.g. `call rax`) occupies fewer bytes and
        // never encodes its target, so the byte five back is body filler.
        assert!(
            !returned_from_direct_self_call(entry + 3, entry),
            "a return address whose preceding bytes are not E8 must stay foreign",
        );
        assert!(
            !returned_from_direct_self_call(entry + 2, entry),
            "a return address too close to the entry to hold a CALL is not one",
        );
        assert!(
            !returned_from_direct_self_call(ret, 0),
            "no entry pointer means nothing can be proven",
        );
    }

    /// `direct_call_callee` generalises the decoder above from "did this method
    /// call itself" to "which method did this call reach" — the difference
    /// between describing a self-recursive frame and describing every frame a
    /// multi-method JIT stack produces.
    ///
    /// The resolution step needs a registered code range, which a unit test
    /// cannot fabricate; what it CAN pin is the decode half, which is where the
    /// fail-closed behaviour lives. Each rejection below is a case that would
    /// otherwise hand a caller some other method's oop map.
    ///
    /// Writing this test is what found the null dereference in the first draft:
    /// `direct_call_callee` read `caller_cm->entry_ptr()` *before* validating
    /// the pointer, which the production path never exposes (the pointer always
    /// comes from `lookup_jit_code_range`) and which a test reaches on its
    /// first call.
    #[test]
    fn direct_call_target_decodes_only_a_real_e8_and_fails_closed_otherwise() {
        let mut body = [0x90u8; 32];
        let entry = body.as_ptr() as usize;
        let call_at = 6usize;
        let ret = entry + call_at + 5;
        // rel32 = target - next_instruction; aim it back at the buffer start.
        let rel = (entry as isize - ret as isize) as i32;
        body[call_at] = 0xE8;
        body[call_at + 1..call_at + 5].copy_from_slice(&rel.to_le_bytes());

        assert_eq!(
            direct_call_target(ret, entry),
            Some(entry),
            "E8 rel32 must decode to next_instruction + rel32",
        );
        assert_eq!(
            direct_call_target(ret, 0),
            None,
            "no caller entry means the backward read cannot be bounded",
        );
        // Too close to the caller's entry to hold a five-byte CALL. Bounded by
        // the CALLER's entry, since that is what keeps the read inside the
        // caller's own executable buffer.
        assert_eq!(
            direct_call_target(entry + 2, entry),
            None,
            "a return address within 5 bytes of the caller entry is not a call",
        );
        // `FF /2` (`call rax`) reached through a REGISTER whose value this code
        // never wrote — the inline MIC/PIC cascade and the hashed megamorphic
        // stub — encodes no target and must stay foreign. Filler bytes precede
        // it here, so neither the `E8` form nor the `MOVABS`+`CALL` pair below
        // matches.
        assert_eq!(
            direct_call_target(entry + 20, entry),
            None,
            "a return address whose preceding bytes are neither call form must stay foreign",
        );
        // Keep `body` alive across every read above.
        assert_eq!(body[call_at], 0xE8);
    }

    /// The SECOND encoding of the same direct call must decode too.
    ///
    /// `x64::emit::emit_call_absolute` emits `E8 rel32` when the target is
    /// within +/-2GB of the emit-time buffer position and
    /// `MOVABS RAX, imm64` + `CALL RAX` when it is not — one call site, two
    /// encodings, chosen by where the allocator happened to put things. While
    /// only the first decoded, `innermost_frame_method` returned `None` for any
    /// frame built by the second, dropping that frame from every Java-visible
    /// stack: `stackwalker_log4j_deep_repeated_walks_finish_under_jit` failed in
    /// 10 of 12 runs of ONE binary, and passed 20 of 20 once this decoded.
    ///
    /// The rejections matter as much as the acceptance. Half the sequence — a
    /// `CALL RAX` whose `MOVABS` is missing, or a `MOVABS` not followed by the
    /// call — is some other instruction pair, and answering for it would hand a
    /// caller a frame's worth of the wrong method's oop map.
    #[test]
    fn direct_call_target_decodes_the_movabs_rax_call_rax_form() {
        let mut body = [0x90u8; 48];
        let entry = body.as_ptr() as usize;
        let target: u64 = 0x1234_5678_9abc_def0;
        let seq = 16usize;
        let ret = entry + seq + ABS64_CALL_LEN;
        body[seq] = 0x48; // REX.W
        body[seq + 1] = 0xB8; // MOVABS RAX, imm64
        body[seq + 2..seq + 10].copy_from_slice(&target.to_le_bytes());
        body[seq + 10] = 0xFF; // CALL r/m64
        body[seq + 11] = 0xD0; // ModRM mod=11 reg=/2 r/m=RAX

        assert_eq!(
            direct_call_target(ret, entry),
            Some(target as usize),
            "MOVABS RAX, imm64 + CALL RAX must decode to the baked-in immediate",
        );
        // Too close to the caller's entry to hold the twelve-byte sequence, and
        // the five-byte E8 read finds filler.
        assert_eq!(
            direct_call_target(entry + 8, entry),
            None,
            "a return address within 12 bytes of the entry cannot be this form",
        );

        // `CALL RAX` without the `MOVABS` that loaded it: the register was set
        // somewhere this decoder cannot see, so there is no target to report.
        let mut lone = [0x90u8; 32];
        let lone_entry = lone.as_ptr() as usize;
        lone[20] = 0xFF;
        lone[21] = 0xD0;
        assert_eq!(
            direct_call_target(lone_entry + 22, lone_entry),
            None,
            "a bare CALL RAX encodes no target and must stay foreign",
        );

        // The `MOVABS` half present but the call is something else.
        let mut half = [0x90u8; 32];
        let half_entry = half.as_ptr() as usize;
        half[8] = 0x48;
        half[9] = 0xB8;
        half[10..18].copy_from_slice(&target.to_le_bytes());
        half[18] = 0xFF;
        half[19] = 0xE0; // JMP RAX, not CALL RAX
        assert_eq!(
            direct_call_target(half_entry + 20, half_entry),
            None,
            "MOVABS followed by something other than CALL RAX must stay foreign",
        );

        // Keep every buffer alive across the reads above.
        assert_eq!(body[seq], 0x48);
        assert_eq!(lone[20], 0xFF);
        assert_eq!(half[19], 0xE0);
    }

    /// The third arm of the same invariant, and the one that silently voided
    /// this module's other shadow tests for a while: `end - base` must be
    /// EXACTLY the fixed buffer size. A `[base, end)` pair that is merely
    /// *plausible* — aligned, ordered, with `top` inside it — is still not a
    /// `ShadowStack` if it is the wrong width, and reading three words out of
    /// an arbitrary stack slot is exactly the SIGSEGV this check exists to
    /// prevent.
    #[test]
    fn shadow_window_rejects_a_buffer_of_the_wrong_size() {
        let mut f = fake_shadow_thread(&[0x1111, 0x2222], false);
        let ss = 24 / 8; // SHADOW_OFF_IN_THREAD / 8, END_OFFSET = 8
                         // Shrink `end` to just past `top`: ordered, aligned, `top` in range —
                         // and the exact shape the old fixture built by accident.
        f._thread[ss + 1] = f._thread[ss];
        assert!(
            shadow_window_from_frame(f.rbp, &f.cm).is_none(),
            "end - base must be exactly DEFAULT_SHADOW_SLOTS * 8",
        );
    }

    /// The verifier must impose no verdict (and no cost) while moving-young is
    /// off — the legacy path's conservative scan already covers these frames.
    #[test]
    fn frame_oop_verifier_is_inert_when_moving_young_is_off() {
        if moving_young_enabled() {
            return; // validating a moving-young build
        }
        let mut reason = cratonvm_gc::gc_quiescence::incomplete_reason::NONE;
        assert!(!moving_young_unpublished_frame_oop_present(&mut reason));
        assert_eq!(reason, cratonvm_gc::gc_quiescence::incomplete_reason::NONE);
    }

    #[test]
    fn moving_young_osr_fallback_ignores_non_osr_methods() {
        let cm = dummy_compiled_method();
        assert!(!moving_young_osr_method_needs_fallback(&cm, 0, false));
    }

    #[test]
    fn moving_young_osr_fallback_requires_shadow_layout() {
        let mut cm = dummy_compiled_method();
        cm.compiled_via_osr = true;
        assert!(moving_young_osr_method_needs_fallback(&cm, 0, false));
    }

    #[test]
    fn moving_young_osr_fallback_accepts_shadow_layout_without_precise_maps() {
        let mut cm = dummy_compiled_method();
        add_shadow_osr_layout(&mut cm);
        assert!(!moving_young_osr_method_needs_fallback(&cm, 0, false));
    }

    #[test]
    fn moving_young_osr_fallback_requires_exact_rbp_for_precise_maps() {
        let mut cm = dummy_compiled_method();
        add_shadow_osr_layout(&mut cm);
        cm.push_oop_map(cratonvm_jit::OopMapEntry {
            native_pc_offset: 0,
            bytecode_pc: 0,
            frame_slot_offsets: vec![-16],
            moving_young_coverage_complete: true,
            live_frame_hi: 0,
            local_oop_mask: None,
            num_locals: 0,
            inline_local_scopes: Vec::new(),
            non_oop_stack_slots: Vec::new(),
            stack_marks_exact: false,
            shadow_pushed: 0,
        });
        cm.fully_oop_covered = true;
        cm.fully_shadow_covered = true;

        assert!(moving_young_osr_method_needs_fallback(&cm, 0, false));
        assert!(!moving_young_osr_method_needs_fallback(&cm, 0x1000, false));
    }

    /// The 2026-08-23 correction, as a pair that fails in BOTH directions if
    /// the predicate reads the wrong field.
    ///
    /// The fixture is the shape the fast tier actually emits for an OSR method
    /// containing a direct JIT→JIT call with a reference argument: every
    /// safepoint publishes complete SHADOW coverage, and `fully_oop_covered` is
    /// false because the marshalled argument sits in the outgoing-ABI area,
    /// which no frame-slot map can name (`pending_staged_args_unmapped`).
    /// Measured on `TestKillProcessWhileWriting`, that shape was 439 of 449
    /// coverage failures and refused relocation on 725 of 759 collections.
    ///
    /// Non-vacuous in both arms: the kill switch flips the answer on the SAME
    /// fixture, so this pins which field is being read rather than riding on a
    /// fixture that would pass either way.
    #[test]
    fn the_osr_coverage_check_reads_the_shadow_aggregate_not_the_frame_slot_subset() {
        let mut cm = dummy_compiled_method();
        add_shadow_osr_layout(&mut cm);
        cm.push_oop_map(cratonvm_jit::OopMapEntry {
            native_pc_offset: 0,
            bytecode_pc: 0,
            frame_slot_offsets: vec![-16],
            moving_young_coverage_complete: true,
            live_frame_hi: 0,
            local_oop_mask: None,
            num_locals: 0,
            inline_local_scopes: Vec::new(),
            non_oop_stack_slots: Vec::new(),
            stack_marks_exact: false,
            shadow_pushed: 0,
        });
        // The direct-call shape: shadow complete, frame-slot subset incomplete.
        cm.fully_shadow_covered = true;
        cm.fully_oop_covered = false;

        assert!(
            !moving_young_osr_method_needs_fallback(&cm, 0x1000, false),
            "a frame whose every safepoint published complete shadow coverage must not \
             force the OSR fallback merely because a marshalled argument has no frame slot"
        );

        cratonvm_types::flags::with_thread_overrides(
            &[("CRATONVM_OSR_COVERAGE_SHADOW", Some("0"))],
            || {
                assert!(
                    moving_young_osr_method_needs_fallback(&cm, 0x1000, false),
                    "the kill switch must restore the frame-slot reading on the SAME \
                     fixture, or it is not a bisect"
                );
            },
        );
    }

    /// The other half: the shadow aggregate is not a rubber stamp. A method
    /// with one incomplete safepoint map still forces the fallback.
    #[test]
    fn the_osr_coverage_check_still_refuses_an_incomplete_shadow_claim() {
        let mut cm = dummy_compiled_method();
        add_shadow_osr_layout(&mut cm);
        cm.push_oop_map(cratonvm_jit::OopMapEntry {
            native_pc_offset: 0,
            bytecode_pc: 0,
            frame_slot_offsets: vec![-16],
            moving_young_coverage_complete: false,
            live_frame_hi: 0,
            local_oop_mask: None,
            num_locals: 0,
            inline_local_scopes: Vec::new(),
            non_oop_stack_slots: Vec::new(),
            stack_marks_exact: false,
            shadow_pushed: 0,
        });
        cm.fully_shadow_covered = false;
        // `fully_oop_covered` true and shadow false is the inverse of the pair
        // above: if the predicate were still reading the old field this would
        // pass the check, so the assertion pins the direction.
        cm.fully_oop_covered = true;

        assert!(moving_young_osr_method_needs_fallback(&cm, 0x1000, false));
    }

    #[test]
    fn moving_young_osr_fallback_honors_debug_shadow_disable() {
        let mut cm = dummy_compiled_method();
        add_shadow_osr_layout(&mut cm);
        assert!(moving_young_osr_method_needs_fallback(&cm, 0, true));
    }

    /// Cross-thread-JIT-gap detector: when a PEER thread holds a live JIT
    /// frame while THIS thread's chain is empty, the detector recognizes the
    /// unsupported multi-thread-in-JIT condition and bumps its diagnostic
    /// counter only if cross-thread takeover is disabled. The detector must
    /// NEVER mutate the root set; it only observes.
    ///
    /// We drive a peer thread into JIT via a handshake (it pushes an entry,
    /// signals, then waits to be released), so the peer's `GLOBAL_JIT_DEPTH`
    /// contribution is live for the duration of our assertions. The main test
    /// thread keeps an empty chain.
    #[test]
    fn cross_thread_jit_gap_detector_obeys_xt_takeover_gate() {
        use std::sync::mpsc;
        // Pre-condition: this thread must not itself be in JIT.
        assert_eq!(current_thread_jit_depth(), 0);

        let (peer_in_jit_tx, peer_in_jit_rx) = mpsc::channel::<()>();
        let (release_tx, release_rx) = mpsc::channel::<()>();
        let handle = std::thread::spawn(move || {
            // Push a JIT entry on the PEER thread → bumps GLOBAL_JIT_DEPTH.
            let _g = JitEntryGuard::enter();
            peer_in_jit_tx.send(()).unwrap();
            // Hold the entry live until the main thread finishes asserting.
            release_rx.recv().unwrap();
            // _g drops here, balancing the global counter.
        });
        peer_in_jit_rx.recv().unwrap();

        // The unsupported condition now holds: this thread's chain is empty,
        // but a peer is in JIT.
        assert_eq!(current_thread_jit_depth(), 0);
        assert!(
            any_thread_in_jit(),
            "peer thread should be observable via GLOBAL_JIT_DEPTH"
        );

        // The detector must align with the takeover gate: default-on takeover
        // makes this a covered condition, while explicit opt-out keeps the old
        // diagnostic hit.
        let before = CROSS_THREAD_JIT_GAP_HITS.load(Ordering::Relaxed);
        warn_cross_thread_jit_gap();
        let after = CROSS_THREAD_JIT_GAP_HITS.load(Ordering::Relaxed);
        if crate::jit::xt_root_scan::enabled() {
            assert_eq!(
                after, before,
                "enabled cross-thread takeover should suppress gap hits"
            );
        } else {
            assert!(
                after > before,
                "detector must record the cross-thread JIT gap when takeover is disabled (before={before}, after={after})"
            );
        }

        // Release the peer and join.
        release_tx.send(()).unwrap();
        handle.join().unwrap();
    }

    // -----------------------------------------------------------------------
    // NEW-12 — precise oop map walker
    // -----------------------------------------------------------------------

    /// [`OopMapEntry::new`] starts empty and the slot count accumulates.
    #[test]
    fn new12_oop_map_entry_basics() {
        let mut entry = cratonvm_jit::OopMapEntry::new(0x1000);
        assert_eq!(entry.slot_count(), 0);
        entry.frame_slot_offsets.push(-16);
        entry.frame_slot_offsets.push(-24);
        assert_eq!(entry.slot_count(), 2);
        assert_eq!(entry.native_pc_offset, 0x1000);
    }

    /// [`cratonvm_jit::CompiledMethod::find_oop_map_for_pc`] returns the
    /// exact match when present and `None` otherwise, and sorts on
    /// first-use so out-of-order pushes work.
    #[test]
    fn new12_find_oop_map_for_pc_handles_unsorted_input() {
        // We need a real CompiledMethod to exercise the lookup. The
        // executable-buffer path requires a live buffer, so we
        // construct one from a minimal sequence.
        let mut buf = cratonvm_jit::ExecutableBuffer::new(64)
            .expect("executable buffer alloc must succeed in tests");
        buf.emit_byte(0xC3); // ret
        let mut cm = cratonvm_jit::CompiledMethod::new(buf);

        // Push out-of-order entries.
        cm.push_oop_map(cratonvm_jit::OopMapEntry {
            native_pc_offset: 0x40,
            bytecode_pc: 0,
            frame_slot_offsets: vec![-8],
            moving_young_coverage_complete: false,
            live_frame_hi: 0,
            local_oop_mask: None,
            num_locals: 0,
            inline_local_scopes: Vec::new(),
            non_oop_stack_slots: Vec::new(),
            stack_marks_exact: false,
            shadow_pushed: 0,
        });
        cm.push_oop_map(cratonvm_jit::OopMapEntry {
            native_pc_offset: 0x10,
            bytecode_pc: 0,
            frame_slot_offsets: vec![-16, -24],
            moving_young_coverage_complete: false,
            live_frame_hi: 0,
            local_oop_mask: None,
            num_locals: 0,
            inline_local_scopes: Vec::new(),
            non_oop_stack_slots: Vec::new(),
            stack_marks_exact: false,
            shadow_pushed: 0,
        });
        cm.push_oop_map(cratonvm_jit::OopMapEntry {
            native_pc_offset: 0x20,
            bytecode_pc: 0,
            frame_slot_offsets: vec![],
            moving_young_coverage_complete: false,
            live_frame_hi: 0,
            local_oop_mask: None,
            num_locals: 0,
            inline_local_scopes: Vec::new(),
            non_oop_stack_slots: Vec::new(),
            stack_marks_exact: false,
            shadow_pushed: 0,
        });

        // Exact-match lookups succeed regardless of insertion order.
        let m10 = cm.find_oop_map_for_pc(0x10);
        assert!(m10.is_some());
        assert_eq!(m10.unwrap().slot_count(), 2);

        let m20 = cm.find_oop_map_for_pc(0x20);
        assert!(m20.is_some());
        assert_eq!(m20.unwrap().slot_count(), 0);

        let m40 = cm.find_oop_map_for_pc(0x40);
        assert!(m40.is_some());
        assert_eq!(m40.unwrap().slot_count(), 1);
        assert_eq!(m40.unwrap().frame_slot_offsets, vec![-8]);

        // Non-match returns None (no nearest-neighbor).
        assert!(cm.find_oop_map_for_pc(0x30).is_none());
    }

    /// A CompiledMethod with no oop maps still registers its frame metadata so
    /// the conservative fallback can use its bounded compiled-frame band.
    #[test]
    fn new12_enter_with_compiled_empty_maps_registers_frame_metadata() {
        let buf = cratonvm_jit::ExecutableBuffer::new(64)
            .expect("executable buffer alloc must succeed in tests");
        let cm = cratonvm_jit::CompiledMethod::new(buf);
        assert!(!cm.has_precise_oop_maps());

        let local_before = current_thread_jit_depth();
        let _g = JitEntryGuard::enter_with_compiled(&cm);
        assert_eq!(current_thread_jit_depth(), local_before + 1);
        JIT_ENTRY_CHAIN.with(|c| {
            let chain = c.borrow();
            let top = chain.last().expect("chain must have one entry");
            let info = top.precise.expect("entry should retain frame metadata");
            assert_eq!(info.compiled_method, &cm as *const _);
            assert_eq!(info.entry_ptr, cm.entry_ptr());
        });
    }

    /// A CompiledMethod with at least one oop map registers a precise
    /// chain entry that carries the metadata the walker needs.
    #[test]
    fn new12_enter_with_compiled_with_maps_registers_precise() {
        let buf = cratonvm_jit::ExecutableBuffer::new(64)
            .expect("executable buffer alloc must succeed in tests");
        let mut cm = cratonvm_jit::CompiledMethod::new(buf);
        cm.push_oop_map(cratonvm_jit::OopMapEntry {
            native_pc_offset: 0,
            bytecode_pc: 0,
            frame_slot_offsets: vec![-16],
            moving_young_coverage_complete: false,
            live_frame_hi: 0,
            local_oop_mask: None,
            num_locals: 0,
            inline_local_scopes: Vec::new(),
            non_oop_stack_slots: Vec::new(),
            stack_marks_exact: false,
            shadow_pushed: 0,
        });
        assert!(cm.has_precise_oop_maps());

        let local_before = current_thread_jit_depth();
        let _g = JitEntryGuard::enter_with_compiled(&cm);
        assert_eq!(current_thread_jit_depth(), local_before + 1);

        JIT_ENTRY_CHAIN.with(|c| {
            let chain = c.borrow();
            let top = chain.last().expect("chain must have one entry");
            let info = top.precise.expect("precise info required");
            assert_eq!(info.compiled_method, &cm as *const _);
            assert_eq!(info.entry_ptr, cm.entry_ptr());
            // frame_base should roughly match entry_sp (both captured
            // at the same call site).
            assert_eq!(info.frame_base, top.entry_sp);
        });
    }

    /// [`scan_oop_slots`] reads each listed offset, validates via
    /// `heap.is_object_address`, and pushes hits into `out`. We use a
    /// real `VmHeap` backed by the default config so the validation
    /// layer exercises real heap bounds.
    ///
    /// The test:
    ///   1. Allocates a single object on the heap → we know a valid
    ///      address that must round-trip through `is_object_address`.
    ///   2. Stores that address into two stack locals along with a
    ///      non-heap poison value.
    ///   3. Calls `scan_oop_slots` with offsets relative to our
    ///      locally-captured "frame base" pointing at each slot.
    ///   4. Asserts the object is reported exactly once for each
    ///      real-oop slot, and the poison slot is filtered out.
    #[test]
    fn new12_scan_oop_slots_filters_via_heap_validation() {
        use crate::classloading::ClassId;
        use crate::memory::vm_heap::VmHeap;
        // Build a real heap and allocate one object so we have a
        // known-valid address.
        let heap = VmHeap::new(
            crate::memory::vm_heap::GcBackend::Generational,
            16 * 1024 * 1024,
        );
        let obj = heap.alloc_object(ClassId::new(0), 0);
        let obj_addr = obj.as_ptr() as usize;

        // Stack locals holding the values to be scanned. The layout
        // uses a stable `Box<[usize; 3]>` so the compiler cannot elide
        // the stores and the offsets are deterministic.
        let slots: Box<[usize; 3]> = Box::new([obj_addr, 0xdead_beef_dead_beefusize, obj_addr]);
        let frame_base = unsafe { slots.as_ptr().add(slots.len()) as usize };
        // Map offsets are positive distances below RBP; use one-past-end as RBP.
        let offsets: Vec<i16> = vec![24, 16, 8];

        let mut out = Vec::new();
        scan_oop_slots(frame_base, &offsets, &heap, &mut out);

        // Exactly two hits (slots 0 and 2) — slot 1 holds poison.
        assert_eq!(out.len(), 2, "should find two real-oop slots, got {out:?}");
        for hit in &out {
            assert_eq!(hit.as_ptr() as usize, obj_addr);
        }
    }

    /// Unaligned offsets are skipped defensively.
    #[test]
    fn new12_scan_oop_slots_skips_unaligned_offsets() {
        use crate::memory::vm_heap::VmHeap;
        let heap = VmHeap::new(
            crate::memory::vm_heap::GcBackend::Generational,
            16 * 1024 * 1024,
        );
        let slots: Box<[usize; 1]> = Box::new([0]);
        let frame_base = slots.as_ptr() as usize;
        // An offset of 1 gives an unaligned address — must be skipped
        // without reading memory.
        let offsets: Vec<i16> = vec![1];
        let mut out = Vec::new();
        scan_oop_slots(frame_base, &offsets, &heap, &mut out);
        assert!(out.is_empty());
    }

    // -----------------------------------------------------------------------
    // A5 return-address validation (`is_plausible_return_pc`)
    //
    // SB-LOADER-ZIPCONTENT (2026-08-04). The A5 probe is a raw-word scan, so
    // before this filter every stack word that merely POINTED somewhere inside
    // a JIT code range counted as a live unregistered JIT frame — and one such
    // word, held anywhere in the interpreter frames above the outermost
    // registered JIT entry, is enough to divert every young collection in the
    // process to the non-moving sweep. `ZipContentTests` ran 512 consecutive
    // `reason=unregistered-jit-frame-on-stack` fallbacks that way, never
    // compacted, and fragmented its young generation until an 8 KB array could
    // not be allocated.
    //
    // The tests below are byte-level: they hand `call_encoding_precedes` the
    // bytes that would sit in front of a candidate return address and ask
    // whether it decodes as a call tail. Both directions are asserted — a
    // filter that only ever says "no" would pass an accept-nothing
    // implementation, and one that only says "yes" would pass the pre-fix
    // accept-everything behaviour this replaces.
    // -----------------------------------------------------------------------

    /// `E8 rel32` — the direct near call the JIT emits for a static/known
    /// target. Its return address MUST be accepted, or the probe stops
    /// detecting the genuine unregistered frames it exists for.
    #[test]
    #[cfg(any(target_os = "windows", target_os = "linux"))]
    fn direct_near_call_return_address_is_accepted() {
        assert!(
            call_encoding_precedes(&[0x90, 0x90, 0xE8, 0x11, 0x22, 0x33, 0x44]),
            "the address after `E8 rel32` is a return address",
        );
    }

    /// `FF /2` indirect near call: register form (`callq *%rax` = `FF D0`),
    /// with a REX prefix (`callq *%r11` = `41 FF D3`), and the memory form
    /// (`callq *0x10(%rbx)` = `FF 53 10`) — the inline-cache, megamorphic-stub
    /// and vtable shapes.
    #[test]
    #[cfg(any(target_os = "windows", target_os = "linux"))]
    fn indirect_near_call_return_addresses_are_accepted() {
        assert!(
            call_encoding_precedes(&[0x90, 0x90, 0xFF, 0xD0]),
            "callq *%rax"
        );
        assert!(
            call_encoding_precedes(&[0x90, 0x41, 0xFF, 0xD3]),
            "callq *%r11 (REX.B)",
        );
        assert!(
            call_encoding_precedes(&[0x90, 0xFF, 0x53, 0x10]),
            "callq *0x10(%rbx)",
        );
    }

    /// The whole point: a word that merely points INTO compiled code, with
    /// ordinary non-call instructions in front of it, is rejected.
    #[test]
    #[cfg(any(target_os = "windows", target_os = "linux"))]
    fn a_stored_code_pointer_is_not_a_return_address() {
        // `mov %rax,-0x8(%rbp)` — about as common as generated code gets.
        assert!(
            !call_encoding_precedes(&[0x48, 0x89, 0x45, 0xF8]),
            "`mov %rax,-0x8(%rbp)` does not end a call — before this filter \
             every such word fabricated an unregistered JIT frame",
        );
        // `FF` present, but as the displacement byte of `mov -0x1(%rcx),%eax`
        // (`8B 41 FF`), not as a call opcode.
        assert!(
            !call_encoding_precedes(&[0x90, 0x90, 0x8B, 0x41, 0xFF]),
            "an `FF` byte is not a call unless it is the opcode AND its ModRM \
             reg field is /2",
        );
        // `FF` as an opcode, but `/1` (`dec`), not `/2` (`call`).
        assert!(
            !call_encoding_precedes(&[0x90, 0x90, 0xFF, 0xC8]),
            "`FF /1` is `dec`, not `call`",
        );
    }

    /// Clamping: a candidate so close to the start of its code range that no
    /// call encoding could fit behind it is rejected, rather than the decoder
    /// reading further back than the caller proved is inside the buffer.
    /// Asserted because getting this wrong is a read off the front of a JIT
    /// arena, not merely a wrong answer.
    #[test]
    #[cfg(any(target_os = "windows", target_os = "linux"))]
    fn a_window_shorter_than_any_call_encoding_is_rejected() {
        assert!(!call_encoding_precedes(&[]), "no window at all");
        assert!(
            !call_encoding_precedes(&[0xE8]),
            "one byte cannot hold any call encoding — not even the `FF /2` \
             register form, which needs two",
        );
    }

    /// The filter is ON by default. Asserted as a DECISION rather than left
    /// implicit: `CRATONVM_JIT=-retpc-validate` is the documented one-flag
    /// bisect, and a future default flip must fail HERE rather than silently
    /// voiding every test above (see
    /// `reference_presence_predicate_lies_after_default_flip`).
    #[test]
    #[cfg(any(target_os = "windows", target_os = "linux"))]
    fn return_pc_validation_is_on_by_default() {
        assert!(
            return_pc_validation_enabled(),
            "the A5 scan must validate return addresses unless \
             CRATONVM_JIT_NO_RETPC_VALIDATE is set",
        );
    }
}

#[cfg(test)]
mod stale_word_oracle_tests {
    use super::{classify_stale_local, StaleVerdict};

    /// `local_offset(k) == 8*(k+1)`, so offset 40 is local 4 and offset 32 is
    /// local 3. This is the `StringConcatHelper.doConcat` witness that
    /// `bug-h2-testrandommapops-small-heap-corruption-20260829.md` reported as
    /// a missed root: mask `Some(19)` = locals 0, 1 and 4, and the stale word
    /// sat at offset 32 — local 3, which `javap` shows is an `int`
    /// (`25: istore_3`). The oracle has to call that dead storage, not a root.
    #[test]
    fn the_doconcat_witness_is_dead_storage() {
        assert_eq!(
            classify_stale_local(32, &[], Some(0b10011), 6, &[], &[]),
            StaleVerdict::LocalNotOop
        );
        // ... while the slots the mask DOES name would be roots if they were
        // ever found stale.
        for off in [8usize, 16, 40] {
            assert_eq!(
                classify_stale_local(off, &[], Some(0b10011), 6, &[], &[]),
                StaleVerdict::LocalOopUnmapped,
                "offset {off}"
            );
        }
    }

    /// `None` and `Some(0)` are different claims and must not be folded: the
    /// first says the dataflow never reached the bci (so the map named no
    /// locals and nothing is attributable), the second says it reached and
    /// proved no local holds a reference.
    #[test]
    fn unreached_is_not_the_empty_mask() {
        assert_eq!(
            classify_stale_local(32, &[], None, 6, &[], &[]),
            StaleVerdict::LocalUnreached
        );
        assert_eq!(
            classify_stale_local(32, &[], Some(0), 6, &[], &[]),
            StaleVerdict::LocalNotOop
        );
    }

    /// Past the locals band the offsets are spill and staging slots, which are
    /// not local homes; the oracle must decline rather than answer about the
    /// wrong slot. Offsets 0 and unaligned words are not local homes either.
    #[test]
    fn outside_the_locals_band_the_oracle_is_silent() {
        assert_eq!(
            classify_stale_local(56, &[], Some(0), 6, &[], &[]),
            StaleVerdict::OutsideLocals
        );
        assert_eq!(
            classify_stale_local(0, &[], Some(0), 6, &[], &[]),
            StaleVerdict::OutsideLocals
        );
        assert_eq!(
            classify_stale_local(36, &[], Some(0), 6, &[], &[]),
            StaleVerdict::OutsideLocals
        );
        // A frame with no mask at all (the IR tier) has `num_locals == 0`, so
        // every word is outside the band and nothing is misattributed.
        assert_eq!(
            classify_stale_local(8, &[], None, 0, &[], &[]),
            StaleVerdict::OutsideLocals
        );
    }

    /// A spliced callee's locals live in the operand-spill band, addressed
    /// from the scope's own base — outside the java-locals band entirely. The
    /// outer mask cannot speak for them, and before the scopes were recorded
    /// every such word read as unclassifiable spill.
    #[test]
    fn a_spliced_callees_locals_are_classified_by_their_own_scope() {
        // Outer method has 2 locals (band 8..=16); a splice based at 96 with
        // 3 locals whose local 1 is a reference.
        let scopes = [(96i32, 3u16, 0b010u64)];
        assert_eq!(
            classify_stale_local(96, &[], Some(0), 2, &scopes, &[]),
            StaleVerdict::InlineLocalNotOop
        );
        assert_eq!(
            classify_stale_local(104, &[], Some(0), 2, &scopes, &[]),
            StaleVerdict::InlineLocalOopUnmapped
        );
        assert_eq!(
            classify_stale_local(112, &[], Some(0), 2, &scopes, &[]),
            StaleVerdict::InlineLocalNotOop
        );
        // One past the scope's last local is spill again, not local 3.
        assert_eq!(
            classify_stale_local(120, &[], Some(0), 2, &scopes, &[]),
            StaleVerdict::OutsideLocals
        );
        // Below the scope's base, likewise.
        assert_eq!(
            classify_stale_local(88, &[], Some(0), 2, &scopes, &[]),
            StaleVerdict::OutsideLocals
        );
        // A scope that could not classify its locals is not recorded at all,
        // so its band stays honestly unattributed rather than reading "dead".
        assert_eq!(
            classify_stale_local(104, &[], Some(0), 2, &[], &[]),
            StaleVerdict::OutsideLocals
        );
    }

    /// The java-locals band wins when the two could overlap: a splice base is
    /// allocated out of the spill band, so an overlap means the scope record is
    /// wrong, and answering from the method's own dataflow is the safer half.
    #[test]
    fn the_outer_locals_band_is_consulted_first() {
        let scopes = [(8i32, 4u16, u64::MAX)];
        assert_eq!(
            classify_stale_local(8, &[], Some(0), 4, &scopes, &[]),
            StaleVerdict::LocalNotOop
        );
    }

    /// A slot the map NAMED cannot be a missed root: relocation rewrote it, and
    /// its current value being a pointer-map key means that to-space address
    /// aliases some other object's from-space address. Asking the locals oracle
    /// about it produced the instrument's only false `LOCAL-OOP-UNMAPPED`, on
    /// `FileStore.readChunkFooter` -- `mapped=[8=..]` with `rewritten` equal to
    /// the slot count, so slot 8 held a value the rewrite had just written.
    #[test]
    fn a_mapped_slot_is_never_a_missed_root() {
        // Local 0, mask says it IS a reference: the exact shape that misfired.
        assert_eq!(
            classify_stale_local(8, &[], Some(0b1), 9, &[], &[]),
            StaleVerdict::LocalOopUnmapped,
            "an UNMAPPED reference local is still the finding"
        );
        assert_eq!(
            classify_stale_local(8, &[8], Some(0b1), 9, &[], &[]),
            StaleVerdict::MappedAlias,
            "the same word, once the map names it, is not a missed root"
        );
        // The mapped check outranks every other oracle, including the ones
        // that would otherwise call the word dead.
        assert_eq!(
            classify_stale_local(96, &[96], Some(0), 2, &[], &[96]),
            StaleVerdict::MappedAlias
        );
    }

    /// The operand-spill band is where the residue actually is on the workload
    /// this oracle was built for: 36 of 37 stale words on
    /// `org.h2.test.store.TestRandomMapOps` sat in `region=operand-spill`,
    /// which the locals and splice oracles both decline to answer for. The
    /// safepoint's own stack model closes that gap for the slots it modelled.
    #[test]
    fn an_operand_spill_slot_the_stack_model_calls_a_primitive_is_dead() {
        // Outer method: 2 locals; spill slots 96 and 104 modelled as non-oops.
        assert_eq!(
            classify_stale_local(96, &[], Some(0), 2, &[], &[96, 104]),
            StaleVerdict::StackNotOop
        );
        // A spill slot the model did not mention stays unproven.
        assert_eq!(
            classify_stale_local(112, &[], Some(0), 2, &[], &[96, 104]),
            StaleVerdict::OutsideLocals
        );
        // A splice's claim wins over the stack model for the same word: the
        // scope knows it is a local, the stack model only knows it is a slot.
        assert_eq!(
            classify_stale_local(96, &[], Some(0), 2, &[(96, 1, 0b1)], &[96]),
            StaleVerdict::InlineLocalOopUnmapped
        );
    }

    /// `compute_local_oop_masks` gives up past 64 locals, so a mask can only
    /// speak for bits 0..63; a local above that must not read as "not an oop",
    /// which would be claiming a proof the mask does not carry.
    #[test]
    fn a_local_past_the_mask_width_is_not_proven_dead() {
        assert_eq!(
            classify_stale_local(8 * 65, &[], Some(u64::MAX), 80, &[], &[]),
            StaleVerdict::LocalUnreached
        );
    }
}
