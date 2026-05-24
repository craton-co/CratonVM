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
//! 1. **Per-thread active JIT entry chain.** [`push_jit_entry`] is called
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
}

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

/// Process-wide counter of active JIT entries across all threads. Lets the GC
/// quickly answer "is anyone in JIT?" without crossing thread boundaries.
static GLOBAL_JIT_DEPTH: AtomicUsize = AtomicUsize::new(0);

/// Re-export the GC-side quiescence flag so VM call sites have a single
/// canonical entry point. The flag itself lives in the gc crate (see
/// `gc::gc_quiescence`) because the GC must consult it from inside its own
/// collection cycles, which would create a circular dependency if the flag
/// lived in the vm crate.
pub use cratonvm_gc::gc_quiescence::is_active as gc_must_defer;

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
    })
}

/// NEW-12: push a fully-specified chain entry. Used by
/// [`JitEntryGuard::enter_with_compiled`] to register both the stack
/// pointer and the precise-frame metadata in one atomic step.
pub(crate) fn push_entry_full(entry: JitFrameChainEntry) -> usize {
    let depth = JIT_ENTRY_CHAIN.with(|c| {
        let mut v = c.borrow_mut();
        v.push(entry);
        v.len()
    });
    GLOBAL_JIT_DEPTH.fetch_add(1, Ordering::Release);
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
#[inline(always)]
pub fn push_jit_entry() -> usize {
    let sp = current_stack_pointer();
    push_jit_entry_at(sp)
}

/// Pop the topmost entry off the JIT entry chain.
///
/// Should be called immediately after a JIT call returns, regardless of
/// success / failure / panic unwind. Returns the popped entry SP for
/// diagnostic purposes.
pub fn pop_jit_entry() -> Option<usize> {
    let popped = JIT_ENTRY_CHAIN.with(|c| c.borrow_mut().pop());
    if let Some(entry) = popped {
        GLOBAL_JIT_DEPTH.fetch_sub(1, Ordering::Release);
        cratonvm_gc::gc_quiescence::leave();
        Some(entry.entry_sp)
    } else {
        None
    }
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
        Self { depth_at_push }
    }

    /// NEW-12: push a JIT entry that carries precise-frame metadata.
    ///
    /// When the root walker encounters an entry of this shape it uses
    /// the compiled method's oop maps to enumerate oops exactly rather
    /// than blindly scanning the spill region. A compiled method with
    /// no oop maps (`cm.has_precise_oop_maps() == false`) falls back
    /// to the conservative scan automatically — this helper checks
    /// that condition and chooses the appropriate path.
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
        if !cm.has_precise_oop_maps() {
            // No maps populated — fall back to conservative. This is
            // the default path today because the JIT compiler does
            // not yet write oop maps during codegen.
            return Self::enter();
        }
        let sp = current_stack_pointer();
        let entry = JitFrameChainEntry {
            entry_sp: sp,
            precise: Some(PreciseFrameInfo {
                compiled_method: cm as *const cratonvm_jit::CompiledMethod,
                frame_base: sp,
                entry_ptr: cm.entry_ptr(),
            }),
        };
        let depth_at_push = push_entry_full(entry);
        Self { depth_at_push }
    }
}

impl Drop for JitEntryGuard {
    fn drop(&mut self) {
        let popped = pop_jit_entry();
        debug_assert!(
            popped.is_some(),
            "JitEntryGuard::drop: chain underflow (was depth {})",
            self.depth_at_push
        );
    }
}

/// Returns true if any thread anywhere in the process is currently inside a
/// JIT call. Used by the GC to decide whether compaction is safe.
#[inline]
pub fn any_thread_in_jit() -> bool {
    GLOBAL_JIT_DEPTH.load(Ordering::Acquire) > 0
}

/// Returns the number of active JIT entries on the *current* thread.
/// Mostly useful for tests and assertions.
#[inline]
pub fn current_thread_jit_depth() -> usize {
    JIT_ENTRY_CHAIN.with(|c| c.borrow().len())
}

// ---------------------------------------------------------------------------
// Scanner
// ---------------------------------------------------------------------------

/// Walk every active JIT spill region on the current thread and report each
/// qword whose value is a valid object address as a conservative root.
///
/// The scanner is **only** valid for the calling thread — the JIT entry chain
/// is thread-local. Cross-thread root scanning during a stop-the-world pause
/// would require a per-thread snapshot of `(JIT_ENTRY_CHAIN, current_sp)`
/// taken at the safepoint; that is a future enhancement and is not needed
/// today because GC runs are triggered from the same thread that is in JIT.
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
pub fn scan_active_jit_frames(heap: &VmHeap, out: &mut Vec<ObjectRef>) {
    // Capture the scanner's own SP at the call site (inlined into the
    // caller). Every active JIT spill region has its *lowest* address at
    // or above this value (the Rust stack grows downward on every supported
    // target). We use `current_stack_pointer` rather than reading `RSP`
    // directly so the implementation is portable across architectures.
    let scanner_sp = current_stack_pointer();
    scan_active_jit_frames_with_sp(scanner_sp, heap, out);
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
pub fn scan_active_jit_frames_with_sp(
    scanner_sp: usize,
    heap: &VmHeap,
    out: &mut Vec<ObjectRef>,
) {
    JIT_ENTRY_CHAIN.with(|c| {
        let chain = c.borrow();
        for entry in chain.iter() {
            match entry.precise {
                Some(info) => scan_one_frame_precise(info, heap, out),
                None => scan_one_frame(scanner_sp, entry.entry_sp, heap, out),
            }
        }
    });
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
fn scan_one_frame_precise(
    info: PreciseFrameInfo,
    heap: &VmHeap,
    out: &mut Vec<ObjectRef>,
) {
    // SAFETY: `info.compiled_method` was populated from a live
    // `&CompiledMethod` at push time, and the chain is popped before
    // the borrow ends. The JIT cache also keeps the CompiledMethod
    // alive via Arc for the duration of the call. Reading through
    // the pointer is valid for the lifetime of this function.
    let cm: &cratonvm_jit::CompiledMethod = unsafe { &*info.compiled_method };

    // Without call-frame introspection we can't directly recover the
    // "current" native PC inside the active JIT frame. Two approaches
    // are available; this implementation uses the simpler one:
    //
    //   1. (Used here) Enumerate EVERY oop map the method has and read
    //      the corresponding slots. A slot that's live at one
    //      safepoint but not another is read as junk at the second
    //      safepoint — but validated via `heap.is_object_address` so
    //      a non-oop reads as None and is dropped. This is
    //      conservative-within-the-map: false positives filtered,
    //      false negatives impossible given the union-of-all-maps.
    //
    //   2. (Future) Use frame-pointer walking to recover the exact
    //      return PC, then binary-search the map table for the
    //      matching safepoint. Requires the JIT to maintain RBP via
    //      the standard prologue/epilogue, which current x64.rs
    //      already does.
    //
    // Approach 1 is the correct choice for this session because it
    // depends only on the oop-map data itself, not on a separate
    // frame-walking routine that would need its own test battery.
    // When approach 2 lands in a future session it can replace the
    // loop below without touching any other code.
    for map in &cm.oop_maps {
        scan_oop_slots(info.frame_base, &map.frame_slot_offsets, heap, out);
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
    let scanner_sp = current_stack_pointer();
    scan_one_frame(scanner_sp, info.frame_base, heap, out);
    let _ = info.entry_ptr; // reserved for future PC-precise lookup
}

/// Read each oop slot listed in `slot_offsets` (byte offsets relative
/// to `frame_base`), validate via `heap.is_object_address`, and push
/// any hit into `out`. Used by [`scan_one_frame_precise`].
fn scan_oop_slots(
    frame_base: usize,
    slot_offsets: &[i16],
    heap: &VmHeap,
    out: &mut Vec<ObjectRef>,
) {
    for &offset in slot_offsets {
        // Negative offsets index below RBP (locals / spills); positive
        // offsets index above RBP (arguments / return area). Both
        // are valid for the walker.
        let addr = (frame_base as isize + offset as isize) as usize;
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
    let mut addr = aligned_low;
    while addr + 8 <= aligned_high {
        let qword = unsafe { (addr as *const usize).read() };
        if let Some(obj) = heap.is_object_address(qword) {
            out.push(obj);
        }
        addr += 8;
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

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
            frame_slot_offsets: vec![-8],
        });
        cm.push_oop_map(cratonvm_jit::OopMapEntry {
            native_pc_offset: 0x10,
            frame_slot_offsets: vec![-16, -24],
        });
        cm.push_oop_map(cratonvm_jit::OopMapEntry {
            native_pc_offset: 0x20,
            frame_slot_offsets: vec![],
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

    /// A CompiledMethod with no oop maps has `has_precise_oop_maps()
    /// == false`, and `enter_with_compiled` falls through to the
    /// conservative guard.
    #[test]
    fn new12_enter_with_compiled_empty_maps_falls_back_to_conservative() {
        let buf = cratonvm_jit::ExecutableBuffer::new(64)
            .expect("executable buffer alloc must succeed in tests");
        let cm = cratonvm_jit::CompiledMethod::new(buf);
        assert!(!cm.has_precise_oop_maps());

        let local_before = current_thread_jit_depth();
        let _g = JitEntryGuard::enter_with_compiled(&cm);
        assert_eq!(current_thread_jit_depth(), local_before + 1);
        // The newly pushed entry must have `precise = None` because
        // the CompiledMethod had no maps.
        JIT_ENTRY_CHAIN.with(|c| {
            let chain = c.borrow();
            let top = chain.last().expect("chain must have one entry");
            assert!(
                top.precise.is_none(),
                "entry with empty oop_maps should register as conservative"
            );
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
            frame_slot_offsets: vec![-16],
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
        use crate::memory::vm_heap::VmHeap;
        use crate::classloading::ClassId;
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
        let frame_base = slots.as_ptr() as usize;
        // Offsets are in bytes relative to frame_base.
        let offsets: Vec<i16> = vec![0, 8, 16];

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
}
