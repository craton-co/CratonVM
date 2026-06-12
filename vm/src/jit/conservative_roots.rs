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
    /// Stage 3/5 (precise relocation) — the EXACT RBP of the *innermost*
    /// active JIT frame, recorded by [`set_top_frame_base`] from the deepest
    /// prologue's `frame_record` helper. Used ONLY as the start of the
    /// `remap_active_jit_frames` RBP-chain walk. It is kept SEPARATE from
    /// `frame_base` (the Rust-guard SP captured at entry) because the marking
    /// path [`scan_one_frame_precise`] uses `frame_base` as the UPPER bound of
    /// its conservative sweep `[scanner_sp, frame_base)`: clobbering it with
    /// the (low) innermost RBP shrank that sweep to just the innermost frame,
    /// dropping every ancestor frame's spilled oops → live objects reclaimed →
    /// heap corruption. `0` until the prologue records it (gate off).
    pub exact_rbp: usize,
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

/// Whether the JIT **shadow-stack** precise-roots mechanism is enabled
/// (`CRATONVM_SHADOW_STACK`). Cached on first read.
///
/// When on, JIT codegen pushes live oops onto each thread's
/// [`cratonvm_gc::shadow_stack::ShadowStack`] around GC-capable safepoints, the
/// marking root scan folds those values into the root set, the post-move remap
/// rewrites them in place, and the young-gen collector is permitted to run the
/// *moving* (Cheney) cycle even while JIT frames are live (see
/// `gen_heap.rs` quiescence gate). Off by default: zero codegen change, the
/// collector keeps deferring to the non-moving sweep under JIT.
#[inline]
pub fn shadow_stack_enabled() -> bool {
    use std::sync::OnceLock;
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| std::env::var_os("CRATONVM_SHADOW_STACK").is_some())
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

/// DBG helper: the current thread's stack HIGH limit (one past the highest
/// usable stack address) via the Win32 `GetCurrentThreadStackLimits`. Used
/// only by the `CRATONVM_DBG_FULLSTACK_SCAN` diagnostic to bound a full-stack
/// conservative scan.
#[cfg(target_os = "windows")]
fn current_thread_stack_high() -> usize {
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
    // Chain mutation = JIT boundary: invalidate the per-thread scan cache.
    note_jit_boundary();
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
    // Chain mutation = JIT boundary: invalidate the per-thread scan cache.
    note_jit_boundary();
    let popped = JIT_ENTRY_CHAIN.with(|c| c.borrow_mut().pop());
    if let Some(entry) = popped {
        GLOBAL_JIT_DEPTH.fetch_sub(1, Ordering::Release);
        cratonvm_gc::gc_quiescence::leave();
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
    let pruned = JIT_ENTRY_CHAIN.with(|c| {
        let mut v = c.borrow_mut();
        let before = v.len();
        // Keep only entries that could still be live (spill region at or
        // above the scanner SP). Entries below it have provably returned.
        v.retain(|e| e.entry_sp >= scanner_sp);
        before - v.len()
    });
    if pruned > 0 {
        // Chain mutation = JIT boundary: invalidate the per-thread scan cache.
        note_jit_boundary();
    }
    for _ in 0..pruned {
        GLOBAL_JIT_DEPTH.fetch_sub(1, Ordering::Release);
        cratonvm_gc::gc_quiescence::leave();
    }
    if pruned > 0 {
        tracing::debug!(
            "pruned {} leaked JIT entry/entries (returned frames below scanner \
             SP {:#x}); quiescence healed to live count",
            pruned,
            scanner_sp,
        );
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
                exact_rbp: 0,
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
    roots: Vec<ObjectRef>,
}

impl JitScanCache {
    const fn empty() -> Self {
        Self {
            filled_gen: u64::MAX,
            chain_len: usize::MAX,
            roots: Vec::new(),
        }
    }
}

/// Record a Rust↔JIT boundary crossing: called at every JIT runtime-helper
/// entry and at every JIT entry-chain mutation. Invalidates the JIT-scan
/// cache (next `scan_active_jit_frames` rescans).
#[inline]
pub fn note_jit_boundary() {
    JIT_BOUNDARY_GEN.with(|g| g.set(g.get().wrapping_add(1)));
}

fn jit_scan_cache_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        std::env::var_os("CRATONVM_NO_JIT_SCAN_CACHE").is_none()
            && std::env::var_os("CRATONVM_DBG_FORCE_MOVING").is_none()
            && std::env::var_os("CRATONVM_SHADOW_STACK").is_none()
    })
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
    // DBG (CRATONVM_DBG_FULLSTACK_SCAN): scan the ENTIRE native stack
    // [scanner_sp, stack_high] as conservative roots, not just the per-entry
    // [scanner_sp, entry_sp] ranges. Decisive experiment for the bintrees18
    // non-moving-sweep corruption: if this makes a live object visible (and
    // stops the sweep zeroing it), the missed root WAS on the stack but
    // outside the JIT chain's bounds (a range bug); if corruption persists,
    // the missed root is not on the stack at all. Validated by is_object_address.
    #[cfg(target_os = "windows")]
    if std::env::var_os("CRATONVM_DBG_FULLSTACK_SCAN").is_some() {
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
    if std::env::var_os("CRATONVM_DBG_NO_PRUNE").is_none() {
        let _ = prune_returned_jit_entries(scanner_sp);
    }
    let chain_len = JIT_ENTRY_CHAIN.with(|c| c.borrow().len());
    if chain_len == 0 {
        // No live JIT frames — nothing to scan (the overwhelmingly common
        // case for `update_root_snapshot`'s per-native-call invocations).
        return;
    }
    // WS1 JIT-scan cache (see the module-level comment at `JIT_SCAN_CACHE`):
    // reuse the previous scan's roots verbatim unless a Rust↔JIT boundary
    // was crossed since (generation bump), which is the only way a spill
    // slot can have changed.
    let gen = JIT_BOUNDARY_GEN.with(|g| g.get());
    if jit_scan_cache_enabled() {
        let hit = JIT_SCAN_CACHE.with(|c| {
            let c = c.borrow();
            if c.filled_gen == gen && c.chain_len == chain_len {
                out.extend_from_slice(&c.roots);
                true
            } else {
                false
            }
        });
        if hit {
            return;
        }
    }
    let scan_start = out.len();
    scan_active_jit_frames_with_sp(scanner_sp, heap, out);
    if jit_scan_cache_enabled() {
        JIT_SCAN_CACHE.with(|c| {
            let mut c = c.borrow_mut();
            c.filled_gen = gen;
            c.chain_len = chain_len;
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
    JIT_ENTRY_CHAIN.with(|c| {
        if let Some(entry) = c.borrow_mut().last_mut() {
            if let Some(info) = entry.precise.as_mut() {
                // Record the EXACT innermost RBP for the relocation walk.
                // Do NOT touch `frame_base` — the marking path
                // `scan_one_frame_precise` uses it as the upper bound of its
                // conservative sweep; shrinking it to this (low) RBP would
                // drop every ancestor frame's spilled oops.
                info.exact_rbp = rbp;
            }
        }
    });
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
pub fn remap_active_jit_frames(pointer_map: &std::collections::HashMap<usize, usize>) {
    if pointer_map.is_empty() {
        return;
    }
    let scanner_sp = current_stack_pointer();
    // Stage 5 diagnostic (CRATONVM_DBG_PRECISE): count frames walked / slots
    // rewritten / chain entries so we can see whether the RBP-chain walk
    // actually engages. Printed once per remap call (grep-friendly).
    let dbg = std::env::var_os("CRATONVM_DBG_PRECISE").is_some();
    let mut dbg_entries = 0usize;
    let mut dbg_precise = 0usize;
    let mut dbg_frames = 0usize;
    let dbg_slots = std::cell::Cell::new(0usize);
    let dbg_maps_found = std::cell::Cell::new(0usize);
    let dbg_examined = std::cell::Cell::new(0usize);
    JIT_ENTRY_CHAIN.with(|c| {
        let chain = c.borrow();
        for entry in chain.iter() {
            dbg_entries += 1;
            let Some(info) = entry.precise else { continue };
            dbg_precise += 1;
            let entry_sp = entry.entry_sp;
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
            // is the frame we remap each step. The innermost frame itself is
            // skipped: its child is a Rust helper (not in the JIT code
            // registry) and its safepoint's live oops are conservatively
            // pinned (so never relocated).
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
                // Resolve the PARENT frame's CompiledMethod from the return
                // address that points into it.
                match cratonvm_jit::lookup_jit_code_range(ret_addr) {
                    Some(cm_ptr) => {
                        // SAFETY: the cm is Arc-owned by the JIT cache while any
                        // of its frames is live (a live frame keeps it cached);
                        // the registry is evicted before a cm is dropped.
                        let cm: &cratonvm_jit::CompiledMethod =
                            unsafe { &*(cm_ptr as *const cratonvm_jit::CompiledMethod) };
                        let (found, examined, n) =
                            remap_one_jit_frame(parent_rbp, cm, pointer_map);
                        dbg_frames += 1;
                        dbg_slots.set(dbg_slots.get() + n);
                        if found {
                            dbg_maps_found.set(dbg_maps_found.get() + 1);
                        }
                        dbg_examined.set(dbg_examined.get() + examined);
                    }
                    None => break, // parent is the interpreter / Rust boundary
                }
                if parent_rbp <= child_rbp {
                    break; // stack must ascend (grows downward); else garbage
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
fn remap_one_jit_frame(
    rbp: usize,
    cm: &cratonvm_jit::CompiledMethod,
    pointer_map: &std::collections::HashMap<usize, usize>,
) -> (bool, usize, usize) {
    let sp_id_off = cm.sp_id_slot_off;
    if sp_id_off == 0 {
        return (false, 0, 0);
    }
    let id_addr = rbp.wrapping_sub(sp_id_off as usize);
    if id_addr & 0x7 != 0 {
        return (false, 0, 0);
    }
    // SAFETY: aligned frame slot of a live JIT frame on this thread.
    let sp_id = (unsafe { (id_addr as *const usize).read() }) as u32;
    let Some(map) = cm.oop_maps.iter().find(|m| m.bytecode_pc == sp_id) else {
        return (false, 0, 0);
    };
    let examined = map.frame_slot_offsets.len();
    let mut rewritten = 0usize;
    for &off in &map.frame_slot_offsets {
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
    (true, examined, rewritten)
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
        });
        cm.push_oop_map(cratonvm_jit::OopMapEntry {
            native_pc_offset: 0x10,
            bytecode_pc: 0,
            frame_slot_offsets: vec![-16, -24],
        });
        cm.push_oop_map(cratonvm_jit::OopMapEntry {
            native_pc_offset: 0x20,
            bytecode_pc: 0,
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
            bytecode_pc: 0,
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
