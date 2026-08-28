// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Core JVM runtime value representation.
//!
//! Defines [`Value`] — the enum for any value that can live in a local
//! variable or on the operand stack (the JVM computational types) — and
//! [`ObjectRef`], the opaque, niche-optimized reference to a heap-allocated
//! Java object. The `Value` layout is size/alignment-asserted to stay
//! compatible with the JIT slot layout.

use std::fmt;
use std::ptr::NonNull;
use std::sync::atomic::{AtomicPtr, AtomicU64, Ordering};

/// A JVM runtime value.
///
/// Represents any value that can be stored in a local variable or on the operand stack.
/// The JVM specification defines computational types that map to these variants.
///
/// **Layout invariant — `#[repr(u32)]` is load-bearing, not decoration.**
///
/// The JIT does not treat a `Value` as an opaque Rust enum. It emits raw
/// machine code against this type's in-memory layout: `ir_lower.rs` writes the
/// `Int` discriminant with `MOV dword [rax + FIELD_CELL_TAG_OFFSET], 0`,
/// `x64/bytecode_walk.rs` does the same for inline `putfield`, `x64/objects.rs`
/// recognises an `Object` cell by the literal word `4`, and compiled
/// `getstatic` loads straight out of a `StaticsBlock` at a baked address. Four
/// separate facts have to hold for that code to be correct:
///
/// | Fact | Consumer |
/// |------|----------|
/// | tag is a `u32` at byte 0 | [`FIELD_CELL_TAG_OFFSET`] |
/// | 4-byte payload at byte 4 | [`FIELD_CELL_PAYLOAD32_OFFSET`] |
/// | 8-byte payload at byte 8 | [`FIELD_CELL_PAYLOAD64_OFFSET`] |
/// | discriminants are 0..=6 in declaration order | every baked `0` / `4` above |
///
/// `#[repr(u32)]` makes all four a *language guarantee*: the enum is laid out
/// as `#[repr(C)] struct { tag: u32, payload: union { .. } }`, so the tag is a
/// `u32` at offset 0, each variant's payload follows at its natural alignment
/// (4 for `Int`/`Float`/`ReturnAddress`, 8 for `Long`/`Double`/`Object`), and
/// unassigned discriminants take declaration order from 0.
///
/// It was previously `#[repr(Rust)]`, with only `size_of == 16` and
/// `align_of <= 8` asserted — neither of which pins the tag's *position*, its
/// *width*, or its *values*, all three of which rustc is free to change for a
/// `repr(Rust)` enum. The header here used to claim that "adding `repr(C)`
/// would change size to 24 bytes and break JIT slot layout". That was measured
/// and is **false**: on rustc 1.97.1 / x86-64, `repr(Rust)`, `repr(u32)` and
/// `repr(C)` all produce size 16, align 8, tag at byte 0 with values 0..=6,
/// 32-bit payload at byte 4, 64-bit payload at byte 8, and `Object(None)`
/// zeroing the pointer word (the `Option<ObjectRef>` niche survives, because
/// the niche is internal to the payload type and not the enum's own tag).
/// `repr(u32)` was chosen over `repr(C)` because it names the tag width the
/// JIT actually encodes.
///
/// [`ValueLayout`] pins every one of those facts as a `const` assertion, so a
/// layout change is a compile error in this crate rather than a miscompile in
/// the JIT. The `jit` crate replicates the size/align asserts as
/// belt-and-suspenders.
///
/// [`FIELD_CELL_TAG_OFFSET`]: crate::heap_types::FIELD_CELL_TAG_OFFSET
/// [`FIELD_CELL_PAYLOAD32_OFFSET`]: crate::heap_types::FIELD_CELL_PAYLOAD32_OFFSET
/// [`FIELD_CELL_PAYLOAD64_OFFSET`]: crate::heap_types::FIELD_CELL_PAYLOAD64_OFFSET
#[derive(Debug, Clone, Copy, PartialEq)]
#[repr(u32)]
pub enum Value {
    /// A 32-bit integer (also used for boolean, byte, char, short).
    ///
    /// Discriminant `0` — baked into JIT codegen; see the type-level note.
    Int(i32) = 0,

    /// A 64-bit long integer. Occupies two stack/local slots.
    Long(i64) = 1,

    /// A 32-bit IEEE 754 float.
    Float(f32) = 2,

    /// A 64-bit IEEE 754 double. Occupies two stack/local slots.
    Double(f64) = 3,

    /// A reference to an object or array. Represented as a raw pointer internally.
    /// `None` represents the `null` reference.
    ///
    /// Discriminant `4` — baked into JIT codegen; see the type-level note.
    Object(Option<ObjectRef>) = 4,

    /// A return address for `jsr`/`ret` instructions (used by older `finally` implementations).
    ReturnAddress(u32) = 5,

    /// Uninitialized slot placeholder (e.g., second slot of a long/double, or unset local).
    Uninitialized = 6,
}

/// An opaque reference to a heap-allocated Java object.
///
/// This will be replaced with a proper GC-managed pointer in Phase 6.
/// For now it's a simple wrapper around a raw pointer.
///
/// **A4 (architectural improvement):** the inner pointer is `NonNull<u8>`,
/// not `*mut u8`. This gives `Option<ObjectRef>` (and thus
/// `Value::Object(Option<ObjectRef>)`) the niche optimization: the `None`
/// case occupies the all-zero bit pattern, so the option is pointer-sized
/// and tag-free. Layout-compatible with `*mut u8` for FFI / casting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ObjectRef {
    ptr: NonNull<u8>,
}

// SAFETY: ObjectRef is a Copy wrapper around a non-null pointer.  We implement
// Hash based on the pointer value so ObjectRef can be used as a HashMap key
// (e.g. in SharedVm.classes.class_mirrors_reverse).  Hashing a raw pointer is
// deterministic within a single process run.
impl std::hash::Hash for ObjectRef {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        (self.ptr.as_ptr() as usize).hash(state);
    }
}

/// Round-5: shared helper used by both `ObjectRef::from_raw` and
/// `ObjectRef::from_raw_nonnull` to debug-assert 8-byte alignment.
///
/// Why a single helper: prior to this change the two constructors used
/// different assertion mechanisms (`from_raw` panicked unconditionally,
/// `from_raw_nonnull` only `debug_assert!`-ed). Either both are load-bearing
/// in release or neither is — and per the `from_raw_nonnull` analysis, the
/// post-condition is already guaranteed by callers (and by the heap layout
/// invariants). The helper makes both call sites use identical wording so a
/// future refactor can't accidentally re-introduce the asymmetry.
#[inline]
fn debug_assert_aligned(ptr: *mut u8) {
    debug_assert!(
        (ptr as usize) % 8 == 0,
        "ObjectRef pointer not 8-byte aligned: {ptr:p}"
    );
}

/// Debug-only re-check of the two invariants an `ObjectRef` is *constructed*
/// with, applied again at every point the raw address is handed back out
/// (`as_ptr` / `as_nonnull` — the sole gateways to a dereference).
///
/// Why re-check what the constructors already assert: `ObjectRef` is `Copy`,
/// pointer-sized and niche-optimized, so it is routinely produced by
/// `transmute`, by raw slot decode (`decode_value`), by JIT-emitted stores into
/// `Value` slots, and by pointer-map rewrites — paths that do not all funnel
/// through `from_raw`. This catches a corrupted or fabricated ref at the frame
/// *before* the deref faults, which is the difference between a named assertion
/// and an unattributable SIGSEGV in collector or JIT code.
///
/// Deliberately NOT checked here: [`plausible_heap_pointer`]'s 47-bit /
/// null-guard-page range. `types` cannot see a heap, and the workspace
/// legitimately constructs sentinel `ObjectRef`s outside that range in tests
/// and synthetic-object paths (e.g. `0xdead_beef_0000_0100`), so a hard assert
/// on the range would fire on correct code. Callers that genuinely require the
/// range invariant should use [`ObjectRef::is_plausible_heap_pointer`], which
/// reports rather than panics. See
/// `docs/threading/objectref-concurrency-contract.md` §7.
///
/// Behaviour note for reviewers: the alignment check can, in principle, turn a
/// silently-degrading corruption into a debug-build panic. The bypass path is
/// `read_value_atomic` / `read_value_checked_atomic`, which validate the
/// 16-byte slot's *discriminant* but not the object payload — so a slot holding
/// `{disc = Object, payload = misaligned}` can reach here without ever crossing
/// `from_raw`. That is a genuine defect, and a named assertion beats the
/// SIGSEGV it otherwise becomes, which is why the check is here. If it ever
/// needs to be stood down, drop the alignment `debug_assert!` and keep the
/// null one.
#[cfg(debug_assertions)]
#[inline]
fn debug_check_deref_invariants(ptr: *mut u8) {
    debug_assert!(
        !ptr.is_null(),
        "ObjectRef holds a null pointer (NonNull niche violated — likely a \
         transmute or a torn 16-byte Value slot)"
    );
    debug_assert!(
        (ptr as usize) % 8 == 0,
        "ObjectRef pointer not 8-byte aligned at deref: {ptr:p}"
    );
}

/// Release-build no-op counterpart to [`debug_check_deref_invariants`].
#[cfg(not(debug_assertions))]
#[inline(always)]
fn debug_check_deref_invariants(_ptr: *mut u8) {}

// ---------------------------------------------------------------------------
// MED (full-review-2026-06-20 #row "ObjectRef Send+Sync sound only by accident
// of the single-threaded scheduler"): single-OS-thread invariant tripwire.
// ---------------------------------------------------------------------------
//
// HISTORICAL NOTE — this tripwire was added when the VM ran every Java thread on
// one OS thread under cooperative scheduling, to fail loudly if a future
// threading change ever violated that assumption. **The assumption no longer
// holds**: `Thread.start` spawns a real OS thread per Java thread. Any
// multithreaded Java program now trips this guard by design.
//
// It is also NOT a soundness guard for `unsafe impl Send/Sync for ObjectRef`.
// Those impls hold under real OS-level parallelism for reasons that have
// nothing to do with the thread count — see the SAFETY block above the impls
// and `docs/threading/objectref-concurrency-contract.md` §6.
//
// It therefore survives only as a narrow diagnostic: arming it confirms that a
// particular workload really is single-OS-threaded, which is occasionally useful
// when bisecting whether a bug requires parallelism to reproduce. A trip is NOT
// evidence of a defect.
//
// It records the first OS thread to construct an `ObjectRef` and aborts if a
// *different* OS thread ever constructs one. It stays **opt-in**, gated on the
// `CRATONVM_ASSERT_SINGLE_OS_THREAD` env var, for two reasons:
//   * the default (production) path must pay no atomic-load cost in the hot
//     object-construction path beyond a single relaxed load;
//   * the crate test harness (and parallel `cargo test`) legitimately
//     constructs `ObjectRef`s from many worker threads, so an always-on guard
//     would spuriously trip — as would essentially every real Java workload.
//
// Sentinel `0` means "no thread recorded yet". `std::thread::ThreadId` is not
// a stable integer, so we derive a non-zero u64 token from it via its `Hash`.
const SINGLE_THREAD_GUARD_UNSET: u64 = 0;
static SINGLE_THREAD_GUARD: AtomicU64 = AtomicU64::new(SINGLE_THREAD_GUARD_UNSET);
// ---------------------------------------------------------------------------
// Object-reference provenance bitmap (context-free decode hardening)
// ---------------------------------------------------------------------------
//
// Context-free decoders (`decode_value`, `CompactValue::to_value`) must not
// fabricate an `ObjectRef` out of arbitrary long bits whose pattern merely
// *looks* like a pointer (HIGH long↔object type-confusion). Every pointer
// that legitimately becomes an object reference crosses the unsafe
// `ObjectRef::from_raw` / `from_raw_nonnull` boundary, so provenance is
// recorded there and consulted at decode time.
//
// This was first implemented as a global `Mutex<HashSet<usize>>`. That
// serialized every `ObjectRef` construction AND every object-slot decode on
// one process-global mutex, and — because entries were never evicted while
// arena addresses recycle — the set grew monotonically until every probe was
// a cache miss. Measured: ~1.7x wall-time regression on the allocation-churn
// bintrees18 benchmark. It is now a lock-free two-level atomic bitmap over
// the 47-bit user address space:
//
//   * granule: 64 bytes — one bit per 64-byte-aligned address block;
//   * leaf: covers 1 GiB of address space = 2^24 bits = 2 MiB, allocated
//     zeroed on first record into that GiB and never freed;
//   * L1: a static array of 2^17 `AtomicPtr` leaf slots (1 MiB of .bss).
//
// Record = two loads + (only if the bit is not yet set) one `fetch_or`.
// Check = two loads + a bit test. No locks anywhere; memory is bounded by
// 2 MiB per GiB of address space that ever hosted an object.
//
// SECURITY TRADEOFF (vs. the exact HashSet): membership is per 64-byte
// granule, so a fabricated payload landing within 64 bytes of a
// once-recorded reference is accepted. This is equivalent-in-the-limit to
// the exact set: the set never evicted while the allocator recycles arena
// addresses, so over a process lifetime exact membership converges to "every
// address the heap ever handed out" anyway. Both are heuristic backstops —
// the load-bearing defense against long↔object confusion remains the typed
// decode paths (`decode_value_checked`, `decode_by_descriptor`,
// `to_value_checked`), which validate against the live heap.
//
// Never-evict semantics are intentional and match the previous
// implementation: a stale-but-once-valid pointer stays "known" (the decode
// then degrades or survives via the GC's own stale-ref containment); only
// bit patterns that were NEVER a reference are rejected here.

/// log2 of the provenance granule (64 bytes).
const PROVENANCE_GRANULE_SHIFT: u32 = 6;
/// log2 of the address span one leaf covers (1 GiB).
const PROVENANCE_LEAF_COVER_SHIFT: u32 = 30;
/// Granule bits per leaf: 2^(30-6) = 2^24.
const PROVENANCE_LEAF_GRANULES: usize =
    1 << (PROVENANCE_LEAF_COVER_SHIFT - PROVENANCE_GRANULE_SHIFT);
/// `u64` words per leaf: 2^24 / 64 = 2^18 (2 MiB).
const PROVENANCE_LEAF_WORDS: usize = PROVENANCE_LEAF_GRANULES / 64;
/// L1 slots: 47-bit address space / 1 GiB per leaf = 2^17.
const PROVENANCE_L1_LEN: usize = 1 << (47 - PROVENANCE_LEAF_COVER_SHIFT);

/// Top-level table: one lazily-allocated leaf bitmap per GiB of address
/// space. A null slot means "no object reference ever recorded in this GiB".
static PROVENANCE_L1: [AtomicPtr<AtomicU64>; PROVENANCE_L1_LEN] =
    [const { AtomicPtr::new(std::ptr::null_mut()) }; PROVENANCE_L1_LEN];

/// Split a plausible pointer into (L1 slot, word-in-leaf, bit mask).
///
/// Callers must have already checked [`plausible_heap_pointer`]; that caps
/// `raw` below 2^47, which bounds the L1 index below `PROVENANCE_L1_LEN`.
#[inline(always)]
fn provenance_indices(raw: u64) -> (usize, usize, u64) {
    let granule = raw >> PROVENANCE_GRANULE_SHIFT;
    let l1 = (raw >> PROVENANCE_LEAF_COVER_SHIFT) as usize;
    let word = ((granule as usize) & (PROVENANCE_LEAF_GRANULES - 1)) >> 6;
    let bit = 1u64 << (granule & 63);
    (l1, word, bit)
}

/// Allocate and publish the leaf for an L1 slot (first record into that GiB).
/// Cold: happens at most once per GiB of address space per process.
#[cold]
#[inline(never)]
fn provenance_leaf_alloc(slot: &AtomicPtr<AtomicU64>) -> *mut AtomicU64 {
    let layout = std::alloc::Layout::array::<AtomicU64>(PROVENANCE_LEAF_WORDS).unwrap();
    // Zeroed = every granule starts "unknown".
    let fresh = unsafe { std::alloc::alloc_zeroed(layout) } as *mut AtomicU64;
    if fresh.is_null() {
        std::alloc::handle_alloc_error(layout);
    }
    // AcqRel publish pairs with the Acquire loads in record/check so the
    // zeroed contents are visible before the pointer is. The losing racer
    // frees its copy and adopts the winner's.
    match slot.compare_exchange(
        std::ptr::null_mut(),
        fresh,
        Ordering::AcqRel,
        Ordering::Acquire,
    ) {
        Ok(_) => fresh,
        Err(winner) => {
            unsafe { std::alloc::dealloc(fresh as *mut u8, layout) };
            winner
        }
    }
}

/// Ways in the per-thread provenance memo.
///
/// It was ONE entry, which is exactly right for the shape it was written for —
/// a TLAB bump-allocating through a single 4 KiB block for ~100 consecutive
/// objects — and exactly wrong for the other common shape: a native that reads
/// two or three long-lived objects in a row. An `ArrayList` and its backing
/// array are separate allocations in separate blocks, so `size()` alternated
/// between two `block` values and every single call evicted the other's entry
/// and took the slow path. `record_object_ref_payload_slow` measured **3.3%**
/// of a flat profile of an `ArrayList.size()` loop, which touches exactly those
/// two blocks (2026-08-13).
///
/// Eight ways is ~128 bytes per thread and covers the working set of a native
/// walking a small object graph. Direct-mapped on the low bits of `block`:
/// a collision costs a slow-path visit, never a wrong answer, because each way
/// stores the `block` it was filled for and is only trusted on an exact match.
const PROVENANCE_MEMO_WAYS: usize = 8;

thread_local! {
    /// Per-thread memo for [`record_object_ref_payload`]: `(block, bits)` per
    /// way, where `block` is `raw >> 12` — the 4 KiB span one leaf `u64` covers
    /// (64 granules x 64 bytes) — and `bits` is a subset of that word's
    /// granule bits this thread has already observed *set* in the global
    /// bitmap.
    ///
    /// PERF: recording is idempotent and the bitmap never clears, so a
    /// remembered set bit means the global store is already done. That turns
    /// the steady-state hot path — object references handed back out of a
    /// TLAB, where consecutive allocations share a 4 KiB block for ~100
    /// objects — into two shifts, a compare and a bit test, replacing an
    /// indexed load out of the 1 MiB L1 table plus a relaxed atomic load out
    /// of a 2 MiB leaf. Measured at 6.1% of the `CratonBench hashmap` phase
    /// before this memo (`record_object_ref_payload` was the fourth-hottest
    /// symbol in the profile).
    ///
    /// SOUNDNESS: the memo can only skip work that would have been a no-op.
    /// Bits are recorded here only after the global bit is known set, and
    /// `PROVENANCE_L1` leaves are never freed and bits never cleared, so a
    /// hit cannot be stale. It is thread-local, so it adds no cross-thread
    /// obligations to the ordering argument below. Widening it to several ways
    /// changes none of that: each way carries the same invariant on its own.
    static PROVENANCE_MEMO: [std::cell::Cell<(u64, u64)>; PROVENANCE_MEMO_WAYS] =
        const { [const { std::cell::Cell::new((u64::MAX, 0)) }; PROVENANCE_MEMO_WAYS] };
}

/// Direct-mapped way for `block`. The empty marker is `u64::MAX`, which no real
/// `raw >> PROVENANCE_WORD_COVER_SHIFT` can equal, so an untouched way cannot
/// be mistaken for a hit.
#[inline(always)]
fn provenance_memo_way(block: u64) -> usize {
    (block as usize) & (PROVENANCE_MEMO_WAYS - 1)
}

/// log2 of the address span one leaf `u64` word covers (64 granules x 64 B).
const PROVENANCE_WORD_COVER_SHIFT: u32 = PROVENANCE_GRANULE_SHIFT + 6;

///
/// `pub(crate)` so [`crate::compact_value::CompactValue`]'s four SUB_OBJECT
/// *encoders* can record too. They are reference-construction boundaries just
/// as much as `ObjectRef::from_raw` is, and until they recorded, the encoder
/// could mint a slot its own decoder would refuse -- see the module note on
/// `CompactValue::object`.
#[inline(always)]
pub(crate) fn record_object_ref_payload(ptr: *mut u8) {
    let raw = ptr as u64;
    if !plausible_heap_pointer(raw) {
        return;
    }
    let block = raw >> PROVENANCE_WORD_COVER_SHIFT;
    let bit = 1u64 << ((raw >> PROVENANCE_GRANULE_SHIFT) & 63);
    let way = provenance_memo_way(block);
    let memo = PROVENANCE_MEMO.with(|memo| memo[way].get());
    if memo.0 == block && memo.1 & bit != 0 {
        return;
    }
    record_object_ref_payload_slow(raw, block, bit, memo, way);
}

/// Out-of-line remainder of [`record_object_ref_payload`]: consult (and, if
/// needed, allocate) the real bitmap leaf, then refresh the per-thread memo
/// with every granule bit that word already has set.
#[inline(never)]
fn record_object_ref_payload_slow(raw: u64, block: u64, bit: u64, memo: (u64, u64), way: usize) {
    let (l1, word, _) = provenance_indices(raw);
    let slot = &PROVENANCE_L1[l1];
    let mut leaf = slot.load(Ordering::Acquire);
    if leaf.is_null() {
        leaf = provenance_leaf_alloc(slot);
    }
    // SAFETY: `leaf` points to PROVENANCE_LEAF_WORDS live AtomicU64 words
    // (published above, never freed); `word` is in-bounds by construction.
    let w = unsafe { &*leaf.add(word) };
    // In steady state the granule bit is already set (arena reuse), so the
    // hot path is a single relaxed load with no store traffic.
    //
    // Ordering rationale: `Relaxed` rests on the claim that any cross-thread
    // transfer of the pointer value carries its own synchronizes-with edge
    // (publishing the pointer through a field write, monitor, or safepoint
    // orders this `fetch_or` before a remote decode's check). That argument is
    // the load-bearing one and is *probably* fine.
    //
    // It was originally buttressed by "and today Java execution is
    // single-OS-thread", which is no longer true — `Thread.start` spawns a real
    // OS thread per Java thread. That clause has been removed rather than
    // rewritten, because it was never the real justification; see the corrected
    // soundness argument above `unsafe impl Send for ObjectRef` for why these
    // orderings still want a proper concurrent audit.
    //
    // Worst case if the edge is ever missing: a decode observes a stale-clear
    // bit and rejects a genuine reference, which degrades to the typed decode
    // paths (`decode_value_checked` and friends) — those are the load-bearing
    // defence, so a miss here is conservative, not memory-unsafe.
    let mut observed = w.load(Ordering::Relaxed);
    if observed & bit == 0 {
        w.fetch_or(bit, Ordering::Relaxed);
        observed |= bit;
    }
    // Remember every bit this word already carries, not just ours: within a
    // TLAB the next ~100 object references land in this same word.
    let carried = if memo.0 == block { memo.1 } else { 0 };
    PROVENANCE_MEMO.with(|m| m[way].set((block, carried | observed)));
}

#[inline]
pub(crate) fn object_ref_payload_is_known(raw: u64) -> bool {
    if !plausible_heap_pointer(raw) {
        return false;
    }
    let (l1, word, bit) = provenance_indices(raw);
    let leaf = PROVENANCE_L1[l1].load(Ordering::Acquire);
    if leaf.is_null() {
        return false;
    }
    // SAFETY: non-null leaves point to PROVENANCE_LEAF_WORDS live AtomicU64
    // words (never freed); `word` is in-bounds by construction.
    let w = unsafe { &*leaf.add(word) };
    w.load(Ordering::Relaxed) & bit != 0
}

/// Derive a stable, non-zero u64 token for the current OS thread.
///
/// `ThreadId`'s integer value is intentionally opaque, so we hash it. The low
/// bit is forced on to guarantee the result never collides with the
/// `SINGLE_THREAD_GUARD_UNSET` sentinel (`0`).
#[inline]
fn current_thread_token() -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    std::thread::current().id().hash(&mut hasher);
    hasher.finish() | 1
}

/// Returns `true` if the single-OS-thread tripwire is enabled.
///
/// Read once and cached for the process lifetime so the hot construction path
/// pays only a single relaxed atomic load, not an `std::env::var` syscall.
#[inline]
fn single_thread_guard_enabled() -> bool {
    // 0 = unknown, 1 = disabled, 2 = enabled.
    static CACHED: AtomicU64 = AtomicU64::new(0);
    match CACHED.load(Ordering::Relaxed) {
        1 => false,
        2 => true,
        _ => {
            let on = crate::flags::runtime_var_os("CRATONVM_ASSERT_SINGLE_OS_THREAD")
                .map(|v| v != "0" && !v.is_empty())
                .unwrap_or(false);
            CACHED.store(if on { 2 } else { 1 }, Ordering::Relaxed);
            on
        }
    }
}

/// Core of the single-OS-thread tripwire, factored out so it can be unit
/// tested directly against an arbitrary guard cell without touching the
/// process-global one.
///
/// Records `token` as the owning thread on first observation; on any later
/// observation, returns `Err(recorded)` if `token` differs from the recorded
/// owner. Returns `Ok(())` when the invariant holds.
#[inline]
fn check_single_thread_against(guard: &AtomicU64, token: u64) -> Result<(), u64> {
    // CAS the sentinel to claim ownership; if another thread already claimed
    // it, `compare_exchange` fails and hands back the recorded owner.
    match guard.compare_exchange(
        SINGLE_THREAD_GUARD_UNSET,
        token,
        Ordering::AcqRel,
        Ordering::Acquire,
    ) {
        Ok(_) => Ok(()), // we are the first; invariant trivially holds.
        Err(recorded) if recorded == token => Ok(()), // same thread again.
        Err(recorded) => Err(recorded), // a *different* OS thread — violation.
    }
}

/// Report a violation of the *opt-in* single-OS-thread diagnostic. Cold and
/// never-inlined so the enabled-check stays a cheap predictable branch on the
/// hot path.
///
/// This is NOT a soundness failure: `Send`/`Sync` for `ObjectRef` hold under
/// real OS-level parallelism (see the SAFETY block above the impls, and
/// `docs/threading/objectref-concurrency-contract.md` §6). It means only that
/// the workload under investigation is genuinely multithreaded.
#[cold]
#[inline(never)]
fn single_thread_guard_violation(recorded: u64, token: u64) -> ! {
    panic!(
        "ObjectRef constructed on a second OS thread (recorded={recorded:#x}, \
         current={token:#x}). CRATONVM_ASSERT_SINGLE_OS_THREAD is a diagnostic, \
         not a soundness guard: it reports that this workload is genuinely \
         multi-OS-threaded. Multi-OS-thread execution is the VM's normal mode \
         and does not invalidate `unsafe impl Send/Sync for ObjectRef`. See \
         docs/threading/objectref-concurrency-contract.md."
    );
}

/// Hot-path entry: enforce the single-OS-thread diagnostic when the tripwire is
/// armed. A no-op (single relaxed load) otherwise.
///
/// `inline(always)` with the armed branch outlined: the intent has always been
/// that a disabled tripwire costs one predictable relaxed load, but the
/// compiler was emitting a real call per `ObjectRef` construction instead
/// (1.4% of the `CratonBench hashmap` phase, as its own profile symbol).
#[inline(always)]
fn enforce_single_os_thread() {
    if single_thread_guard_enabled() {
        enforce_single_os_thread_armed();
    }
}

#[inline(never)]
fn enforce_single_os_thread_armed() {
    let token = current_thread_token();
    if let Err(recorded) = check_single_thread_against(&SINGLE_THREAD_GUARD, token) {
        single_thread_guard_violation(recorded, token);
    }
}

impl ObjectRef {
    /// Create a new object reference from a raw pointer.
    ///
    /// # Safety
    /// The caller must ensure the pointer is valid and points to a properly allocated object.
    /// The pointer must be non-null and aligned to at least 8 bytes (heap object alignment).
    ///
    /// **MED-1:** the null check here is `debug_assert!`-only. All callers
    /// in the value layer (notably `decode_value` for `VTAG_OBJECT`) already
    /// null-check upstream and route null bit patterns to `Value::Object(None)`
    /// without entering this constructor, so the release build elides the
    /// redundant branch. Debug builds still trip the assert if a caller
    /// violates the precondition.
    // SAFETY: callers guarantee `ptr` is non-null and aligned to 8 bytes
    // (heap object alignment). Both preconditions are checked via
    // `debug_assert!` only — release builds elide the branch.
    //
    // Round-7: alignment check downgraded from unconditional panic to
    // `debug_assert!` for consistency with `from_raw_nonnull`. The hot
    // path (object dereference) does not check alignment in release
    // anyway — a misaligned slot would corrupt the heap before reaching
    // this constructor — so the release-build panic was redundant.
    #[inline]
    pub unsafe fn from_raw(ptr: *mut u8) -> Self {
        debug_assert!(
            !ptr.is_null(),
            "ObjectRef::from_raw called with null pointer"
        );
        // Round-5: shared alignment helper. Both `from_raw` and
        // `from_raw_nonnull` go through `debug_assert_aligned`, so the
        // assertion text and check site are identical.
        // SAFETY: callers guarantee `ptr` is non-null and 8-byte aligned.
        debug_assert_aligned(ptr);
        // Opt-in single-OS-thread DIAGNOSTIC (CRATONVM_ASSERT_SINGLE_OS_THREAD).
        // Not a Send/Sync soundness guard — see the SAFETY block above the
        // impls and docs/threading/objectref-concurrency-contract.md §6.
        enforce_single_os_thread();
        record_object_ref_payload(ptr);
        Self {
            ptr: unsafe { NonNull::new_unchecked(ptr) },
        }
    }

    /// Create a new object reference from an already-validated `NonNull<u8>`.
    ///
    /// Prefer this over [`from_raw`] when the caller already holds a
    /// `NonNull` — it avoids re-checking nullness in any build.
    ///
    /// # Safety
    /// The pointer must point to a properly allocated, 8-byte-aligned
    /// heap object for the lifetime that the returned `ObjectRef` is used.
    // SAFETY: callers guarantee `ptr` is aligned to 8 bytes (heap object
    // alignment); checked in debug only.
    #[inline]
    pub unsafe fn from_raw_nonnull(ptr: NonNull<u8>) -> Self {
        // SAFETY: callers guarantee 8-byte alignment; checked in debug only
        // via the shared `debug_assert_aligned` helper for consistency with
        // `from_raw`.
        debug_assert_aligned(ptr.as_ptr());
        // Opt-in single-OS-thread DIAGNOSTIC (CRATONVM_ASSERT_SINGLE_OS_THREAD).
        // Not a Send/Sync soundness guard — see the SAFETY block above the
        // impls and docs/threading/objectref-concurrency-contract.md §6.
        enforce_single_os_thread();
        record_object_ref_payload(ptr.as_ptr());
        Self { ptr }
    }

    /// Hand back the raw object-header address.
    ///
    /// This (and [`Self::as_nonnull`]) is the only gateway from an `ObjectRef`
    /// to a dereference, so the construction-time invariants are re-checked
    /// here in debug builds — see [`debug_check_deref_invariants`]. Zero cost
    /// in release.
    ///
    /// Returning the address asserts **nothing** about the pointee. The
    /// obligations a caller takes on by dereferencing it (root coverage,
    /// permitted GC phase, staleness across safepoints and blocking regions)
    /// are tabulated in `docs/threading/objectref-concurrency-contract.md` §5.
    #[inline]
    pub fn as_ptr(&self) -> *mut u8 {
        let p = self.ptr.as_ptr();
        debug_check_deref_invariants(p);
        p
    }

    /// Return the underlying `NonNull<u8>` without going through a raw pointer round-trip.
    ///
    /// Same debug-only invariant check and same caller obligations as
    /// [`Self::as_ptr`].
    #[inline]
    pub fn as_nonnull(&self) -> NonNull<u8> {
        debug_check_deref_invariants(self.ptr.as_ptr());
        self.ptr
    }

    /// Whether this reference's address satisfies the cheap, context-free
    /// heap-pointer plausibility test ([`plausible_heap_pointer`]): non-null,
    /// 8-byte aligned, above the null-guard page, and within 47 bits.
    ///
    /// Reports rather than panics, because the workspace legitimately mints
    /// out-of-range sentinel refs (tests, synthetic-object identities). Use it
    /// where the range invariant is genuinely required — it is the local half
    /// of the "is this reference plausible?" question; the load-bearing half is
    /// a live-heap probe (`VmHeap::is_object_address`), which this crate cannot
    /// see.
    #[inline]
    pub fn is_plausible_heap_pointer(&self) -> bool {
        plausible_heap_pointer(self.ptr.as_ptr() as u64)
    }
}

// SAFETY: `ObjectRef` is `Send` and `Sync`.
//
// Full derivation, the real threading model, and the per-operation contract
// table: `docs/threading/objectref-concurrency-contract.md`. Read that before
// changing anything here. What follows is the short form.
//
// `NonNull<u8>` is `!Send + !Sync` by default (same as `*mut u8`), so these
// `unsafe impl`s are required — the A4 switch from `*mut u8` to `NonNull<u8>`
// was a layout change (the niche for `Option<ObjectRef>`), not an auto-trait
// change.
//
// WHAT IS BEING ASSERTED
//
// Exactly two things about *values of this type*, and nothing about the
// pointee:
//
//   Send — an `ObjectRef` value may be moved/copied to another OS thread.
//   Sync — a `&ObjectRef` may be shared across OS threads.
//
// WHY THIS HOLDS UNDER REAL OS THREADS
//
// 1. `ObjectRef` is `#[derive(Copy)]` over a single `NonNull<u8>` field, has
//    no `Drop` impl, and has no interior mutability. Sending it transfers a
//    bit pattern; nothing is deallocated, unshared or invalidated by the
//    transfer.
// 2. `Sync` needs `&ObjectRef` to be race-free to share. The one field is
//    written at construction and never mutated, so a shared reference grants
//    read-only access to an immutable word. No data race on `ObjectRef`
//    itself is expressible.
// 3. The `NonNull` default is conservative about *ownership* semantics the
//    standard library cannot see. `ObjectRef` has none — the collector owns
//    the pointee. The correct comparison is `usize` (which is `Send + Sync`),
//    not `Box<u8>`.
// 4. Multi-OS-thread Java execution (`Thread.start` spawns a real
//    `std::thread::Builder` worker per Java thread — `thread_start` in
//    `vm/src/vm/vm_exec.rs`; virtual threads are multiplexed over those
//    carriers) changes nothing in (1)-(3). It changes a great deal for
//    *dereferencing*, which is a property of the deref sites and is already
//    `unsafe` at each of them.
//
// THIS IS DELIBERATELY NARROWER THAN THE ARGUMENT IT REPLACES
//
// The previous note derived `Send`/`Sync` from three whole-VM properties
// (root coverage, a claimed monitor/atomic field-access protocol, and
// atomically-observed compaction) and from a since-falsified
// "single-OS-thread, cooperatively scheduled" premise. That coupling was the
// bug: it tied two auto-trait impls to the entire GC design, so the impls
// looked unsound the moment the scheduler changed. They were not. Two of the
// three properties were also misstated — plain (non-volatile) field access
// takes NO lock (safety comes from per-word `AtomicU64` access, see
// `read_value_atomic`/`write_value_atomic` below), and the JIT's inline
// `jit_putfield_*` helpers DO write slots directly outside the GC.
//
// INVARIANTS THE REST OF THE VM MUST UPHOLD
//
// These are real and load-bearing — they are just not obligations of *these
// impls*. An `ObjectRef` is a value, not a capability: holding one asserts
// nothing about the pointee's validity, on any thread including the one that
// created it. Dereferencing one requires, per §5 and §7 of the doc:
//
//   * the ref is reachable from the GC root set for the dereferencing thread
//     (frame slot, `native_pin_roots`, `handle_slots`, deposited root
//     snapshot, JNI local/global table, or a registered `ExternalRootProvider`);
//   * relocation happens only under stop-the-world (witnessed by
//     `StopTheWorldToken`), and EVERY holder of the address is rewritten
//     through the collection's pointer map — or, where a holder cannot be
//     rewritten (conservative JIT roots), the collection does not move at all;
//   * an `ObjectRef` kept in a Rust local across a safepoint poll, a blocking
//     region, or an allocation is NOT rewritten by any of those paths and is
//     stale afterwards;
//   * `Hash`/`Eq` are address-based, so `ObjectRef` identity is NOT stable
//     across a moving collection: any `HashMap<ObjectRef, _>` needs a paired
//     remap in `update_all_roots`.
//
// None of these are enforced by types or assertions today; §7 of the doc
// lists them as follow-up work.
//
// Knock-on, now discharged: the provenance bitmap earlier in this file used to
// justify its `Ordering::Relaxed` accesses partly on the same single-OS-thread
// premise. That clause is gone; the surviving argument is the
// self-carrying-happens-before-edge one, whose failure mode is a conservative
// reject (degrade to `Object(None)`), never a fabricated pointer.
//
// TRIPWIRE: `enforce_single_os_thread()` in `ObjectRef::from_raw` /
// `from_raw_nonnull`, opt-in via `CRATONVM_ASSERT_SINGLE_OS_THREAD`, aborts if
// a second OS thread ever constructs an `ObjectRef`. It is NOT a soundness
// guard for these impls (see above — they hold under parallelism). Any
// multithreaded Java program trips it by design. It survives only as a
// diagnostic for confirming that a specific workload really is
// single-OS-threaded, e.g. when bisecting whether a bug needs parallelism to
// reproduce. Do not enable it in a multithreaded run and do not read a trip as
// evidence of a bug.
unsafe impl Send for ObjectRef {}
unsafe impl Sync for ObjectRef {}

impl Value {
    /// Returns true if this value is the null reference.
    pub fn is_null(&self) -> bool {
        matches!(self, Value::Object(None))
    }

    /// Returns true if this value occupies two stack slots (long or double).
    pub fn is_category2(&self) -> bool {
        matches!(self, Value::Long(_) | Value::Double(_))
    }

    /// Extract an int value, or None if this isn't an Int.
    pub fn as_int(&self) -> Option<i32> {
        match self {
            Value::Int(v) => Some(*v),
            _ => None,
        }
    }

    /// Extract a long value, or None if this isn't a Long.
    pub fn as_long(&self) -> Option<i64> {
        match self {
            Value::Long(v) => Some(*v),
            _ => None,
        }
    }

    /// Extract a float value, or None if this isn't a Float.
    pub fn as_float(&self) -> Option<f32> {
        match self {
            Value::Float(v) => Some(*v),
            _ => None,
        }
    }

    /// Extract a double value, or None if this isn't a Double.
    pub fn as_double(&self) -> Option<f64> {
        match self {
            Value::Double(v) => Some(*v),
            _ => None,
        }
    }

    /// Extract a non-null object reference.
    ///
    /// **MED-P3 flatten:** returns `Some(r)` only when this is a
    /// `Value::Object(Some(r))` (a real, non-null reference). Returns
    /// `None` for both `Value::Object(None)` (the JVM `null`) and any
    /// non-Object variant. Callers that need to distinguish "is this an
    /// Object slot at all" from "is this null" should match `Value::Object(_)`
    /// directly or use [`Value::is_null`] together with this accessor.
    pub fn as_object(&self) -> Option<ObjectRef> {
        match self {
            Value::Object(r) => *r,
            _ => None,
        }
    }
}

// -- Compact encoding: Value <-> (u64, u8) for SoA storage --
// Reduces per-slot memory from 16 bytes (enum) to 9 bytes (u64 + u8 tag).

/// Type tags for compact SoA (Structure of Arrays) value storage.
pub const VTAG_INT: u8 = 0;
pub const VTAG_LONG: u8 = 1;
pub const VTAG_FLOAT: u8 = 2;
pub const VTAG_DOUBLE: u8 = 3;
pub const VTAG_OBJECT: u8 = 4;
pub const VTAG_NULL: u8 = 5;
pub const VTAG_UNINIT: u8 = 6;
pub const VTAG_RETADDR: u8 = 7;

/// Encode a Value into a compact (u64, u8) pair for SoA storage.
#[inline(always)]
pub fn encode_value(v: Value) -> (u64, u8) {
    match v {
        Value::Int(i) => (i as u32 as u64, VTAG_INT),
        Value::Long(l) => (l as u64, VTAG_LONG),
        Value::Float(f) => (f.to_bits() as u64, VTAG_FLOAT),
        Value::Double(d) => (d.to_bits(), VTAG_DOUBLE),
        Value::Object(Some(r)) => (r.as_ptr() as u64, VTAG_OBJECT),
        Value::Object(None) => (0, VTAG_NULL),
        Value::Uninitialized => (0, VTAG_UNINIT),
        Value::ReturnAddress(a) => (a as u64, VTAG_RETADDR),
    }
}

/// Cold path for [`decode_value`]'s `VTAG_OBJECT` branch: a `VTAG_OBJECT`
/// slot whose pointer is implausible or lacks prior `ObjectRef` provenance.
/// This degrades to `Value::Object(None)` in release builds, counts the
/// reclassification, and returns `Object(None)`.
///
/// Splitting this out as a `#[cold]` non-inlined function mirrors
/// `compact_value::cold_degraded_object_ptr` and gives LLVM permission to
/// place it off the hot path, freeing icache for the well-formed object
/// branch in `decode_value`. The well-formed slot satisfies the plausibility
/// predicate, so this is taken essentially never in steady-state interpretation.
#[cold]
#[inline(never)]
fn cold_decode_degraded_object_ptr(ptr: *mut u8) -> Value {
    // T14 / KC16 SIGSEGV audit: gracefully handle corrupted or
    // zero-initialized slots that have VTAG_OBJECT but an unacceptable pointer
    // (null, unaligned, null-page, outside the supported address range, or
    // never seen through ObjectRef construction).
    // This is a deliberately-handled, *counted, non-fatal*
    // reclassification — never dereference the bogus pointer, return
    // Object(None) instead. This is a recoverable fallback, NOT an invariant
    // violation, so it must not `panic!`/`debug_assert!(false)` (a prior
    // tripwire here contradicted `decode_value_rejects_null_object` /
    // `decode_value_rejects_unaligned_object` / the release-mode degrade
    // contract, which require Object(None) without panicking).
    //
    // FIX: surface the reclassification via the shared degradation counter
    // (countable in both debug and release) rather than aborting in debug.
    // Mirrors `compact_value::emit_first_degradation_diag` (assert removed).
    let _ = ptr;
    crate::compact_value::note_object_degradation();
    Value::Object(None)
}

/// Cheap, context-free plausibility test for a raw heap-object pointer.
///
/// A genuine object reference on x86-64 / AArch64 user space is non-null,
/// 8-byte aligned, above the null-guard page, and fits in 47 bits. Any value
/// failing these is provably NOT a live object pointer — it is reused/garbage
/// memory surfaced through a STALE reference (a GC root-coverage gap that
/// swept-then-reused the slot a ref still points at). Pure bit ops (no heap
/// probe), so it is safe on the hottest field/array read paths, and it NEVER
/// rejects a valid pointer (zero false positives).
///
/// Shared by the interpreter decode (`read_prim_element`,
/// `CompactValue::from_value`) and the JIT field/array read helpers
/// (`jit_getfield`, `jit_aaload`) so a stale reference is degraded to null at
/// EVERY read boundary — interpreter and JIT alike — instead of being
/// dereferenced (SIGSEGV) or fabricated into an out-of-47-bit `CompactValue`
/// (the compact_value.rs panic).
#[inline(always)]
pub const fn plausible_heap_pointer(raw: u64) -> bool {
    const NULL_GUARD_PAGE: u64 = 0x1000;
    const ADDR_BITS_47: u64 = (1u64 << 47) - 1;
    raw >= NULL_GUARD_PAGE && raw & 0x7 == 0 && raw <= ADDR_BITS_47
}

/// Decode a compact (u64, u8) pair back into a Value.
///
/// For `VTAG_OBJECT`, this safe context-free decoder only recreates an
/// `ObjectRef` for payloads that have already crossed the unsafe
/// `ObjectRef::from_raw` / `from_raw_nonnull` boundary in this process.
/// Arbitrary aligned raw bits degrade to null instead of fabricating a fresh
/// object reference.
#[inline(always)]
pub fn decode_value(val: u64, tag: u8) -> Value {
    match tag {
        VTAG_INT => Value::Int(val as i32),
        VTAG_LONG => Value::Long(val as i64),
        VTAG_FLOAT => Value::Float(f32::from_bits(val as u32)),
        VTAG_DOUBLE => Value::Double(f64::from_bits(val)),
        VTAG_OBJECT => {
            let ptr = val as *mut u8;
            // The degraded paths (implausible or never-seen pointer payloads)
            // are split into a `#[cold]` helper: every well-formed object slot
            // should have crossed ObjectRef construction already, so the branch
            // predictor and LLVM's basic-block layout treat them as cold.
            if !object_ref_payload_is_known(val) {
                cold_decode_degraded_object_ptr(ptr)
            } else {
                Value::Object(Some(unsafe { ObjectRef::from_raw(ptr) }))
            }
        }
        VTAG_NULL => Value::Object(None),
        VTAG_RETADDR => Value::ReturnAddress(val as u32),
        _ => Value::Uninitialized,
    }
}

/// Decode a compact `(u64, u8)` pair with a caller-supplied live-heap check.
///
/// This is the preferred path for code that has heap context. It accepts a
/// non-null object payload only when it is both pointer-plausible and the
/// supplied predicate confirms that the address is currently a live object.
#[inline]
pub fn decode_value_checked(val: u64, tag: u8, is_heap_object: impl Fn(u64) -> bool) -> Value {
    match tag {
        VTAG_OBJECT => {
            let ptr = val as *mut u8;
            if plausible_heap_pointer(val) && is_heap_object(val) {
                Value::Object(Some(unsafe { ObjectRef::from_raw(ptr) }))
            } else {
                cold_decode_degraded_object_ptr(ptr)
            }
        }
        _ => decode_value(val, tag),
    }
}

/// Check if a tag represents an Object reference (non-null) for GC scanning.
#[inline(always)]
pub fn is_object_tag(tag: u8) -> bool {
    tag == VTAG_OBJECT
}

/// Raw bits stored in a local/stack cell tagged [`VTAG_LONG`] that should be
/// treated as a non-null object pointer for GC rooting and pointer remapping.
///
/// JNI and internal bridges sometimes surface `jobject` handles as raw `i64`
/// (`Value::Long`). When those bits are written into a reference local without
/// widening to [`VTAG_OBJECT`], they may be traced only if they look like a
/// plausible pointer and have already crossed the unsafe `ObjectRef`
/// construction boundary in this process.
///
/// Returns `None` for patterns that `coerce_value_for_return` maps to `null`
/// or that cannot be a VM heap object pointer by the rules used by
/// [`decode_value`].
#[inline(always)]
pub fn jlong_bits_as_aligned_object_ptr(bits: u64) -> Option<usize> {
    if object_ref_payload_is_known(bits) {
        Some(bits as usize)
    } else {
        None
    }
}

// ---------------------------------------------------------------------------
// Untagged slots (`RawSlot`) — the verifier-map-driven frame representation
// ---------------------------------------------------------------------------
//
// # Why this exists
//
// The interpreter currently stores every local and every operand-stack slot as
// an 8-byte NaN-boxed [`crate::compact_value::CompactValue`] **plus** a
// parallel `kinds: Vec<u8>` byte (`vm::runtime::value_stack::ValueStack::kinds`
// and `vm::runtime::frame::Frame::local_kinds`). Those byte arrays exist for
// exactly one consumer — the GC root scan — and for exactly one reason: a
// 64-bit `long` uses all 64 bits, so `CompactValue::long(0xFFFC_0000_0000_0000)`
// is bit-identical to `CompactValue::int(0)`. NaN-boxing cannot tag a full
// 64-bit payload, so the tag has to live somewhere else. That is a *proven*
// property of the encoding, not a bug to be fixed: see the five round-trip
// tests in `compact_value.rs` that pin it.
//
// The static fix is to stop carrying a runtime tag at all. Bytecode
// verification already proves, at every instruction start, exactly which local
// slots and which operand-stack slots hold an object reference, and
// `classloading::type_maps::MethodTypeMaps` now *retains* that proof
// (`local_oops_at` / `stack_oops_at` / `stack_depth_at`). Given the map, a slot
// needs no tag: the GC reads the oop bit, and the interpreter reads the type
// from the opcode it is already executing.
//
// `RawSlot` is the `types`-side half of that change: the untagged 8-byte slot
// itself, plus the decoders that turn one into a `Value` when an *external*
// type source says what it is.
//
// # Encoding
//
// A `RawSlot` is a bare `u64` with no marker bits whatsoever. The encoding is
// identical to the *value* half of [`encode_value`]'s `(u64, u8)` SoA pair — so
// a consumer migrating from `(vals, tags)` storage keeps its bit patterns and
// only changes where the tag comes from:
//
// | Java type      | 64 bits hold                                   |
// |----------------|------------------------------------------------|
// | `int`          | the `i32`, zero-extended                       |
// | `long`         | the `i64`, verbatim — **all 64 bits**          |
// | `float`        | `f32::to_bits`, zero-extended                  |
// | `double`       | `f64::to_bits`                                 |
// | reference      | a **bare pointer**; `0` is `null`              |
// | `returnAddress`| the pc, zero-extended                          |
// | uninitialized  | `0`                                            |
//
// Two consequences worth stating explicitly:
//
// * **A `long` is bit-exact and cannot collide with anything.** The collision
//   `CompactValue` has is structural: it must reserve bit patterns for tags out
//   of the same 64 bits the payload needs. `RawSlot` reserves none, so
//   `RawSlot::from_long(x).bits() == x as u64` for every `x: i64`, including the
//   NaN-box patterns. This is the whole point.
// * **A reference is a bare pointer, not NaN-boxed.** A moving collector
//   updates a root with a plain store ([`RawSlot::set_oop`]) instead of
//   re-deriving tag bits, and a `RawSlot` object slot can be compared to a heap
//   address directly.
//
// # Slot width and compressed oops
//
// A `RawSlot` is always 8 bytes, including when compressed oops are on. Narrow
// oops (`crate::narrow_oop`) narrow *heap* reference slots — instance fields
// and array elements — because those are what dominate the heap footprint.
// Frame locals and operand-stack slots are not heap slots: they are bounded by
// stack depth, not by live-set size, and narrowing them would cost an
// encode/decode on every `aload`/`astore` for no footprint win. Frames
// therefore hold full 64-bit addresses regardless of the narrow-oop setting,
// and [`RawSlot::oop`] returns a decoded, ready-to-dereference address in both
// configurations.
//
// # What a consumer still needs, and what it does NOT
//
// `MethodTypeMaps` records **oop-vs-not** per slot, plus the operand-stack
// depth. That is precisely, and only, what the GC root scan needs — it must
// distinguish "reference" from "everything else" and nothing finer. So the GC
// can consume `RawSlot` today with no further verifier work.
//
// It is *not* enough to reconstruct a fully-typed [`Value`] at an arbitrary pc,
// because the maps do not distinguish `int` from `long` from `float` from
// `double`. Every consumer that needs that distinction already has a better
// source for it:
//
// * the interpreter knows from the opcode (`ladd` operates on `long`s);
// * a call/return boundary knows from the method descriptor
//   (see [`RawSlot::decode_by_descriptor`]);
// * a field access knows from the field descriptor.
//
// A consumer that needs a typed `Value` at an *arbitrary* pc with no such
// context — JVMTI local-variable inspection, a debugger, a heap-dump frame
// walker — cannot be served by the current maps. Closing that requires
// `MethodTypeMaps` to grow a 2-bit-per-slot kind table alongside the 1-bit oop
// table (`Int32 | Int64 | Float32 | Float64`, references already covered by the
// oop bit). That is a `classloading` change, is not required by any consumer
// listed above, and is deliberately not attempted here — see
// `arch-2026-07-26/value-repr-and-compressed-oops.md`.

/// What an untagged [`RawSlot`] holds, supplied by the caller's type source
/// (a verifier oop map, an opcode, or a descriptor) rather than by the slot.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SlotType {
    Int,
    Long,
    Float,
    Double,
    /// An object reference or `null`. `null` is the all-zero bit pattern; there
    /// is no separate `Null` slot type, because a verifier oop bit does not
    /// distinguish the two and does not need to.
    Reference,
    ReturnAddress,
    Uninitialized,
}

impl SlotType {
    /// Whether a slot of this type is a GC root candidate.
    #[inline(always)]
    pub const fn is_reference(self) -> bool {
        matches!(self, SlotType::Reference)
    }

    /// Whether this is a JVMS category-2 type (`long` / `double`).
    ///
    /// Note this describes the *Java* type, not the slot count: CratonVM's
    /// operand stack gives a category-2 value **one** slot (see the index-space
    /// table in `classloading::type_maps`), while locals give it two.
    #[inline(always)]
    pub const fn is_category2(self) -> bool {
        matches!(self, SlotType::Long | SlotType::Double)
    }

    /// The `SlotType` a JVM field/parameter descriptor's first byte denotes.
    /// `L` and `[` are references; `V` and anything unrecognised yield `None`.
    #[inline]
    pub const fn from_descriptor_byte(b: u8) -> Option<SlotType> {
        Some(match b {
            b'Z' | b'B' | b'C' | b'S' | b'I' => SlotType::Int,
            b'J' => SlotType::Long,
            b'F' => SlotType::Float,
            b'D' => SlotType::Double,
            b'L' | b'[' => SlotType::Reference,
            _ => return None,
        })
    }
}

/// An **untagged** 8-byte interpreter slot.
///
/// See the module section above for the encoding, the compressed-oops
/// interaction, and what a consumer must supply to decode one.
///
/// `repr(transparent)` over `u64`, so `Vec<RawSlot>`, `Vec<u64>` and
/// `Vec<CompactValue>` all share one layout and the existing frame-pool
/// `Vec<u64>` buffers can back untagged frames with no reallocation.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Default)]
#[repr(transparent)]
pub struct RawSlot(u64);

const _: () = assert!(std::mem::size_of::<RawSlot>() == 8);
const _: () = assert!(std::mem::align_of::<RawSlot>() == std::mem::align_of::<u64>());

impl RawSlot {
    /// The all-zero slot: `null`, `uninitialized`, `0`, and `0.0` all share it.
    /// Which one it *is* comes from the caller's [`SlotType`].
    pub const ZERO: RawSlot = RawSlot(0);

    // -- constructors -------------------------------------------------------

    #[inline(always)]
    pub const fn from_int(v: i32) -> Self {
        RawSlot(v as u32 as u64)
    }

    /// Store a `long` **verbatim**. Unlike [`CompactValue::long`] this can never
    /// alias another slot type's encoding, because `RawSlot` reserves no bits.
    ///
    /// [`CompactValue::long`]: crate::compact_value::CompactValue::long
    #[inline(always)]
    pub const fn from_long(v: i64) -> Self {
        RawSlot(v as u64)
    }

    #[inline(always)]
    pub const fn from_float(v: f32) -> Self {
        RawSlot(v.to_bits() as u64)
    }

    /// Store a `double` **verbatim**, including every NaN payload.
    /// [`CompactValue::double`] must canonicalise NaNs whose bits collide with
    /// its tag space; `RawSlot` has no tag space, so it does not.
    ///
    /// [`CompactValue::double`]: crate::compact_value::CompactValue::double
    #[inline(always)]
    pub const fn from_double(v: f64) -> Self {
        RawSlot(v.to_bits())
    }

    /// Store a reference as a bare address. `0` is `null`.
    #[inline(always)]
    pub const fn from_oop(addr: u64) -> Self {
        RawSlot(addr)
    }

    #[inline(always)]
    pub const fn from_return_address(pc: u32) -> Self {
        RawSlot(pc as u64)
    }

    #[inline(always)]
    pub const fn from_bits(bits: u64) -> Self {
        RawSlot(bits)
    }

    // -- accessors ----------------------------------------------------------

    #[inline(always)]
    pub const fn bits(self) -> u64 {
        self.0
    }

    #[inline(always)]
    pub const fn as_int(self) -> i32 {
        self.0 as i32
    }

    #[inline(always)]
    pub const fn as_long(self) -> i64 {
        self.0 as i64
    }

    #[inline(always)]
    pub fn as_float(self) -> f32 {
        f32::from_bits(self.0 as u32)
    }

    #[inline(always)]
    pub fn as_double(self) -> f64 {
        f64::from_bits(self.0)
    }

    #[inline(always)]
    pub const fn as_return_address(self) -> u32 {
        self.0 as u32
    }

    // -- GC interface -------------------------------------------------------

    /// The heap address this slot roots, or `None` for `null`.
    ///
    /// **This is the GC root-scan entry point, and it is deliberately not
    /// heuristic.** The caller has already proven the slot is a reference by
    /// consulting `MethodTypeMaps::local_oops_at` / `stack_oops_at`, so this
    /// performs none of the guesswork the tagged decoders are forced into:
    ///
    /// * no [`plausible_heap_pointer`] filter — a proven reference does not
    ///   need one, and applying it would silently drop a legitimate root that
    ///   happened to look odd;
    /// * no provenance-bitmap probe ([`object_ref_payload_is_known`], two
    ///   dependent loads plus a bit test, on a 2 MiB-per-GiB table that is a
    ///   near-guaranteed cache miss in a root scan);
    /// * no degradation counting, because there is nothing to degrade *to* —
    ///   the alternative reading of these bits does not exist.
    ///
    /// Removing those three is the actual throughput argument for untagged
    /// slots, over and above deleting the `kinds` byte per slot.
    ///
    /// A `debug_assert!` still catches a caller that passes a non-reference
    /// slot (or a corrupt one) during development.
    #[inline(always)]
    pub fn oop(self) -> Option<u64> {
        if self.0 == 0 {
            return None;
        }
        debug_assert!(
            plausible_heap_pointer(self.0),
            "RawSlot::oop on a slot the verifier map called a reference, but \
             whose bits {:#x} cannot be a heap address — the oop map and the \
             frame have drifted out of sync",
            self.0
        );
        Some(self.0)
    }

    /// Repoint a root after a moving collector relocates its target.
    ///
    /// A plain store: there are no tag bits to preserve. The `CompactValue`
    /// counterpart (`update_object_ptr`) has to rebuild the NaN box and can
    /// fail; this cannot.
    #[inline(always)]
    pub fn set_oop(&mut self, addr: u64) {
        debug_assert!(
            addr == 0 || plausible_heap_pointer(addr),
            "RawSlot::set_oop with a non-heap address {addr:#x}"
        );
        self.0 = addr;
    }

    // -- typed decode / encode ----------------------------------------------

    /// Reconstruct a [`Value`], given the type from the caller's type source.
    ///
    /// For [`SlotType::Reference`] this is the one place a bare address becomes
    /// an `ObjectRef`, so it keeps the cheap pure-bit-ops
    /// [`plausible_heap_pointer`] guard: `ObjectRef::from_raw` is `unsafe` and
    /// its result is handed to code that will dereference it, so a corrupt
    /// frame must degrade to `null` rather than fabricate a wild pointer. The
    /// guard costs three ALU ops and no memory traffic — unlike the provenance
    /// bitmap, which is what this path drops.
    #[inline]
    pub fn decode(self, ty: SlotType) -> Value {
        match ty {
            SlotType::Int => Value::Int(self.0 as i32),
            SlotType::Long => Value::Long(self.0 as i64),
            SlotType::Float => Value::Float(f32::from_bits(self.0 as u32)),
            SlotType::Double => Value::Double(f64::from_bits(self.0)),
            SlotType::Reference => {
                if self.0 == 0 {
                    Value::Object(None)
                } else if plausible_heap_pointer(self.0) {
                    // SAFETY: non-null, 8-byte aligned, above the null guard
                    // page and within 47 bits. The verifier proved this slot is
                    // a reference; the guard above rejects a frame that has been
                    // corrupted out from under that proof.
                    Value::Object(Some(unsafe { ObjectRef::from_raw(self.0 as *mut u8) }))
                } else {
                    cold_decode_degraded_object_ptr(self.0 as *mut u8)
                }
            }
            SlotType::ReturnAddress => Value::ReturnAddress(self.0 as u32),
            SlotType::Uninitialized => Value::Uninitialized,
        }
    }

    /// Decode using a JVM descriptor's first byte as the type source — the
    /// call/return boundary case, where the descriptor is the authority and no
    /// oop map is consulted. Unrecognised bytes (including `V`) yield
    /// [`Value::Uninitialized`].
    #[inline]
    pub fn decode_by_descriptor(self, desc_byte: u8) -> Value {
        match SlotType::from_descriptor_byte(desc_byte) {
            Some(ty) => self.decode(ty),
            None => Value::Uninitialized,
        }
    }

    /// Split a [`Value`] into its untagged bits and the type a consumer must
    /// later supply to read them back. The inverse of [`RawSlot::decode`].
    ///
    /// `Value::Object(None)` encodes as `(ZERO, Reference)`, not a distinct
    /// null type — matching what a verifier oop bit can express.
    #[inline]
    pub fn encode(v: Value) -> (RawSlot, SlotType) {
        match v {
            Value::Int(i) => (RawSlot::from_int(i), SlotType::Int),
            Value::Long(l) => (RawSlot::from_long(l), SlotType::Long),
            Value::Float(f) => (RawSlot::from_float(f), SlotType::Float),
            Value::Double(d) => (RawSlot::from_double(d), SlotType::Double),
            Value::Object(Some(r)) => (RawSlot(r.as_ptr() as u64), SlotType::Reference),
            Value::Object(None) => (RawSlot::ZERO, SlotType::Reference),
            Value::ReturnAddress(a) => (RawSlot::from_return_address(a), SlotType::ReturnAddress),
            Value::Uninitialized => (RawSlot::ZERO, SlotType::Uninitialized),
        }
    }

    // -- migration bridge ---------------------------------------------------

    /// Convert to the tagged [`CompactValue`] the frame uses today.
    ///
    /// Lets a consumer migrate one array at a time: a frame can hold `RawSlot`
    /// locals while its operand stack is still `CompactValue`, or vice versa.
    ///
    /// [`CompactValue`]: crate::compact_value::CompactValue
    #[inline]
    pub fn to_compact(self, ty: SlotType) -> crate::compact_value::CompactValue {
        use crate::compact_value::CompactValue;
        match ty {
            SlotType::Int => CompactValue::int(self.0 as i32),
            SlotType::Long => CompactValue::long(self.0 as i64),
            SlotType::Float => CompactValue::float(f32::from_bits(self.0 as u32)),
            // The caller named the slot type, so the double's bits need no
            // collision canonicalization - see `CompactValue::double_raw`.
            SlotType::Double => CompactValue::double_raw(f64::from_bits(self.0)),
            SlotType::Reference => {
                if self.0 != 0 && plausible_heap_pointer(self.0) {
                    CompactValue::object(self.0)
                } else {
                    CompactValue::null()
                }
            }
            SlotType::ReturnAddress => CompactValue::return_address(self.0 as u32),
            SlotType::Uninitialized => CompactValue::uninitialized(),
        }
    }

    /// Convert from the tagged [`CompactValue`], using the caller's type source
    /// rather than the value's own (ambiguous for `long`/`double`) tag.
    ///
    /// [`CompactValue`]: crate::compact_value::CompactValue
    #[inline]
    pub fn from_compact(cv: crate::compact_value::CompactValue, ty: SlotType) -> RawSlot {
        match ty {
            // A `long` in a CompactValue is stored verbatim as raw bits.
            SlotType::Long => RawSlot(cv.as_long_bits_unchecked() as u64),
            SlotType::Double => RawSlot(cv.raw_bits()),
            SlotType::Int => RawSlot::from_int(cv.as_int().unwrap_or(0)),
            SlotType::Float => RawSlot::from_float(cv.as_float().unwrap_or(0.0)),
            SlotType::Reference => RawSlot(cv.as_object_ptr().unwrap_or(0)),
            SlotType::ReturnAddress => {
                RawSlot::from_return_address(cv.as_return_address().unwrap_or(0))
            }
            SlotType::Uninitialized => RawSlot::ZERO,
        }
    }
}

impl fmt::Debug for RawSlot {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Untagged by construction: printing a type would be a lie.
        write!(f, "RawSlot({:#018x})", self.0)
    }
}

// ---------------------------------------------------------------------------
// Layout invariants (owned by `types` since `Value` is defined here)
// ---------------------------------------------------------------------------
//
// The JIT slot layout depends on `Value` being exactly 16 bytes with align ≤ 8
// and on `ObjectRef` being pointer-sized.  These asserts live here (alongside
// the type definitions) so the invariant is owned by the crate that owns the
// types.  The `jit` crate replicates the same asserts as belt-and-suspenders
// so a JIT-only platform-port still fails to compile if the layout drifts.
const _: () = assert!(
    std::mem::size_of::<Value>() == 16,
    "Value must be exactly 16 bytes (JIT slot layout depends on this)"
);
const _: () = assert!(
    std::mem::align_of::<Value>() <= 8,
    "Value alignment must not exceed 8 bytes"
);
const _: () = assert!(
    std::mem::size_of::<ObjectRef>() == std::mem::size_of::<*mut u8>(),
    "ObjectRef must be pointer-sized"
);

// ---------------------------------------------------------------------------
// `Value` cell layout — the four facts the JIT bakes into machine code
// ---------------------------------------------------------------------------
//
// `size_of == 16` and `align_of <= 8` above do NOT pin the layout the JIT
// actually encodes. They say nothing about where the discriminant sits, how
// wide it is, or what values it takes — and the JIT emits literal `0` (Int) and
// `4` (Object) tag words at a literal byte offset (`ir_lower.rs`,
// `x64/bytecode_walk.rs`, `x64/objects.rs`), then loads payloads at literal
// +4 / +8. Before `Value` became `#[repr(u32)]` those four facts were held only
// by a runtime unit test in `heap_types.rs`, which catches drift after the fact
// on whoever happens to run `-p cratonvm-types`; a rustc upgrade that reordered
// the tag would have shipped a miscompile everywhere else.
//
// `#[repr(u32)]` plus the explicit `= 0 .. = 6` discriminants on the variants
// make all four language guarantees. These assertions verify that the
// guarantee is the layout the JIT's constants actually name, so a future edit
// that changes `FIELD_CELL_*_OFFSET`, reorders the variants, or drops the
// `repr` is a compile error in this crate — not a wrong answer in compiled
// code. Const-eval reads the tag and payload words directly; that is sound
// here because both are plain integer bytes with no pointer provenance (the
// `Object(None)` case is the all-zero niche, never a real address).
//
// Keep these in sync with `heap_types::FIELD_CELL_*_OFFSET`; the asserts below
// reference those constants rather than repeating the numbers, so the two
// cannot drift apart silently.

/// Read the discriminant word of a `Value` at `FIELD_CELL_TAG_OFFSET`.
///
/// SAFETY: `#[repr(u32)]` guarantees a `u32` tag at offset 0, which is
/// `FIELD_CELL_TAG_OFFSET` (asserted below). The read is of initialized
/// integer bytes carrying no provenance.
const fn value_tag_word(v: &Value) -> u32 {
    unsafe { *(v as *const Value as *const u32) }
}

/// Read the 4-byte payload of a `Value` at `FIELD_CELL_PAYLOAD32_OFFSET`.
///
/// SAFETY: as [`value_tag_word`]; callers below pass only `Int`-like variants,
/// whose payload at this offset is an initialized `i32`.
const fn value_payload32(v: &Value) -> i32 {
    unsafe {
        *((v as *const Value as *const u8).add(crate::heap_types::FIELD_CELL_PAYLOAD32_OFFSET)
            as *const i32)
    }
}

/// Read the 8-byte payload of a `Value` at `FIELD_CELL_PAYLOAD64_OFFSET`.
///
/// SAFETY: as [`value_tag_word`]; callers below pass only `Long` and
/// `Object(None)`, whose payload at this offset is an initialized integer
/// word (the `None` niche is all-zero, so no pointer provenance is read).
const fn value_payload64(v: &Value) -> i64 {
    unsafe {
        *((v as *const Value as *const u8).add(crate::heap_types::FIELD_CELL_PAYLOAD64_OFFSET)
            as *const i64)
    }
}

// Fact 1 — the tag is a `u32` at `FIELD_CELL_TAG_OFFSET` (byte 0).
const _: () = assert!(
    crate::heap_types::FIELD_CELL_TAG_OFFSET == 0,
    "FIELD_CELL_TAG_OFFSET must be 0: #[repr(u32)] puts the tag at offset 0, \
     and the JIT emits `MOV dword [recv + FIELD_CELL_TAG_OFFSET], imm` against it"
);

// Fact 2 — discriminants are 0..=6 in declaration order. The JIT bakes `0`
// (Int) and `4` (Object) as literals; the rest are pinned so a reorder that
// would shift those two is caught even if the JIT's own literals are not
// touched.
const _: () = assert!(value_tag_word(&Value::Int(0)) == 0, "Value::Int tag must be 0");
const _: () = assert!(value_tag_word(&Value::Long(0)) == 1, "Value::Long tag must be 1");
const _: () = assert!(
    value_tag_word(&Value::Float(0.0)) == 2,
    "Value::Float tag must be 2"
);
const _: () = assert!(
    value_tag_word(&Value::Double(0.0)) == 3,
    "Value::Double tag must be 3"
);
const _: () = assert!(
    value_tag_word(&Value::Object(None)) == crate::heap_types::FIELD_CELL_TAG_OBJECT,
    "Value::Object tag must match FIELD_CELL_TAG_OBJECT (x64/objects.rs and the IR backend's inline legacy getfield both recognise an Object cell by it)"
);
const _: () = assert!(
    value_tag_word(&Value::ReturnAddress(0)) == 5,
    "Value::ReturnAddress tag must be 5"
);
const _: () = assert!(
    value_tag_word(&Value::Uninitialized) == 6,
    "Value::Uninitialized tag must be 6"
);

// Fact 3 — a 4-byte payload lands at `FIELD_CELL_PAYLOAD32_OFFSET` (byte 4).
const _: () = assert!(
    value_payload32(&Value::Int(0x1234_5678)) == 0x1234_5678,
    "Value::Int payload must sit at FIELD_CELL_PAYLOAD32_OFFSET"
);

// Fact 4 — an 8-byte payload lands at `FIELD_CELL_PAYLOAD64_OFFSET` (byte 8),
// and `Object(None)` zeroes that word. The latter is what lets JIT'd code test
// a reference field for null with a plain `cmp qword [cell + 8], 0`.
const _: () = assert!(
    value_payload64(&Value::Long(0x0102_0304_0506_0708)) == 0x0102_0304_0506_0708,
    "Value::Long payload must sit at FIELD_CELL_PAYLOAD64_OFFSET"
);
const _: () = assert!(
    value_payload64(&Value::Object(None)) == 0,
    "Value::Object(None) must zero the payload word (JIT null-tests it directly)"
);
// Alignment half of the same invariant. Size alone does not pin the layout:
// adding `#[repr(align(16))]`, or swapping the field for a type with a
// stricter alignment, keeps the size at 8 while silently changing how
// `ObjectRef` packs inside `Value`, inside `[ObjectRef]` root vectors, and
// inside the JIT's 16-byte slots. Pinned here so a layout change is a compile
// error rather than a runtime mystery. See
// `docs/threading/objectref-concurrency-contract.md` §2.
const _: () = assert!(
    std::mem::align_of::<ObjectRef>() == std::mem::align_of::<*mut u8>(),
    "ObjectRef must have pointer alignment"
);
// `Option<ObjectRef>` must not gain alignment either — `Value::Object` embeds
// it, and `Value`'s own align <= 8 assert above depends on this staying true.
const _: () = assert!(
    std::mem::align_of::<Option<ObjectRef>>() == std::mem::align_of::<*mut u8>(),
    "Option<ObjectRef> must have pointer alignment"
);
// A4: confirm the NonNull niche optimization — Option<ObjectRef> must be
// pointer-sized (no discriminant tag) because `null` is the niche for the
// `None` case.  If this assert ever fires, something has been added to
// ObjectRef (e.g. a non-niche-aware field) that defeats the layout opt
// and silently inflates Value to 24 bytes — breaking the JIT slot layout.
const _: () = assert!(
    std::mem::size_of::<Option<ObjectRef>>() == std::mem::size_of::<*mut u8>(),
    "Option<ObjectRef> must be pointer-sized (NonNull niche optimization)"
);

// ---------------------------------------------------------------------------
// Atomic 16-byte object-slot access (concurrent-GC correctness)
// ---------------------------------------------------------------------------
//
// The GC marker scans an object's field slots *concurrently* with mutator
// stores. A `Value` is exactly 16 bytes (asserted above) — wider than any
// single machine word — so a plain `ptr::read::<Value>` racing a plain
// `ptr::write::<Value>` is a data race: formal UB under the Rust/C++ memory
// model, and in principle able to splice the words of two different stores into
// a garbage pointer the marker would then dereference.
//
// These helpers read/write a slot as two `AtomicU64` words. Using them on BOTH
// the collector read side (`g1`/`concurrent_mark` object scan) and the writer
// side (the JIT `jit_putfield_*` field-store helpers, which write the slot
// directly rather than going through the heap's lock-serialized `set_field`)
// makes every concurrent slot access well-defined and free of within-word
// tearing. For a statically-typed Java field the discriminant word is invariant
// across stores, so even a cross-word "torn" pair reconstructs to a valid
// `Object(ptr-or-null)`/primitive — never a spliced garbage pointer.
//
// `Relaxed` is sufficient: marking *correctness* (no lost live reference) is
// carried by the SATB pre-barrier, not by the ordering of this access; the
// atomics are here only for per-word atomicity (UB-freedom + no torn pointer).

/// Atomically read a 16-byte `Value` object slot as two relaxed `AtomicU64`
/// words. See the module note above for why.
///
/// # Safety
/// `slot` must be a valid, 8-byte-aligned pointer to a live 16-byte `Value`
/// slot. The reconstructed `Value` has the same validity contract as
/// `ptr::read::<Value>` (the bytes must form a valid `Value`, which holds for a
/// typed Java field whose discriminant is invariant across stores).
#[inline]
pub unsafe fn read_value_atomic(slot: *const Value) -> Value {
    let w0 = (*(slot as *const AtomicU64)).load(Ordering::Relaxed);
    let w1 = (*((slot as *const u8).add(8) as *const AtomicU64)).load(Ordering::Relaxed);
    std::mem::transmute::<[u64; 2], Value>([w0, w1])
}

/// Atomically write a 16-byte `Value` object slot as two relaxed `AtomicU64`
/// words — the writer-side counterpart of [`read_value_atomic`].
///
/// # Safety
/// `slot` must be a valid, 8-byte-aligned pointer to a 16-byte `Value` slot.
#[inline]
pub unsafe fn write_value_atomic(slot: *mut Value, value: Value) {
    let [w0, w1] = value_words(value);
    (*(slot as *const AtomicU64)).store(w0, Ordering::Relaxed);
    (*((slot as *const u8).add(8) as *const AtomicU64)).store(w1, Ordering::Relaxed);
}

/// The two 64-bit words a [`Value`] occupies in a 16-byte cell, with every byte
/// the variant does not use written as **zero**.
///
/// # Why this is not `transmute::<Value, [u64; 2]>`
///
/// `Value` is `repr(u32)` with a 32-bit payload at byte 4 and a 64-bit payload
/// at byte 8, so a narrow variant — `Int`, `Float`, `ReturnAddress`,
/// `Uninitialized` — leaves bytes 8..16 as **padding**. Transmuting the whole
/// value reads that padding: formally UB (a `transmute` out of a type with
/// padding produces uninitialised bytes), and in practice whatever the caller's
/// stack temp happened to hold. `write_value_atomic` then commits those bytes
/// to the cell's `FIELD_CELL_PAYLOAD64_OFFSET` word.
///
/// That word is not inert. It is the word a compiled reference `getfield`
/// **dereferences**: the inline arm loads the cell's 8-byte payload and uses it
/// as the object pointer. The VM's own containment argument for a primitive
/// that lands in a declared-reference slot (the G30-1 species) is that the
/// payload word is zero — "a reference field of a freshly allocated object,
/// whose cell is still zero-filled and so decodes as `Int(0)` — that word is 0,
/// i.e. the correct null, by accident" (`jit_getfield_impl`). Garbage padding
/// is exactly what turns that benign null into a wild pointer, and it is why a
/// punned `SQLChar.rawData` cell was reported as `tag=0 payload32=0x1
/// payload64=0x1` rather than `payload64=0x0`, and why the compiled
/// `arraylength` that followed faulted at `addr=0x5` instead of throwing
/// NullPointerException
/// (`known-issues/tomcat/punned-sqlchar-rawdata-cell-writer-localized-…`).
///
/// Zeroing the unused half does not make a punned store correct — it makes it
/// **contained and diagnosable**, which is what every other reader on this path
/// already assumes.
#[inline]
pub fn value_words(value: Value) -> [u64; 2] {
    match value {
        // Narrow variants: discriminant in the low half of word 0, the 32-bit
        // payload in the high half, word 1 explicitly zero.
        Value::Int(i) => [(i as u32 as u64) << 32, 0],
        Value::Float(f) => [2u64 | ((f.to_bits() as u64) << 32), 0],
        Value::ReturnAddress(a) => [5u64 | ((a as u64) << 32), 0],
        Value::Uninitialized => [6, 0],
        // Wide variants: the 64-bit payload IS word 1, and bytes 4..8 are the
        // padding this time — zeroed for the same reason.
        Value::Long(l) => [1, l as u64],
        Value::Double(d) => [3, d.to_bits()],
        Value::Object(o) => [4, o.map_or(0, |r| r.as_ptr() as u64)],
    }
}

/// The largest valid discriminant of [`Value`] — the index of the last variant
/// (`Uninitialized`). The seven variants occupy discriminants `0..=6`; the
/// discriminant is pinned to the **low 32 bits of word 0** by the layout test
/// `value_discriminant_is_byte0_low32` below.
///
/// A 16-byte slot whose discriminant word exceeds this does **not** form a
/// valid `Value`. Reading it with `ptr::read::<Value>` and then matching on it
/// is undefined behavior: the compiler lowers a `Value` `match` to a jump table
/// indexed by the discriminant, so a corrupt discriminant produces a wild
/// indexed jump (an unrecoverable SIGSEGV far outside the program).
pub const VALUE_MAX_DISCRIMINANT: u32 = 6;

/// Read a `Value` from a heap slot, validating its discriminant **before**
/// constructing the enum.
///
/// A heap field cell can be corrupted by a reference-integrity defect — e.g. a
/// live object that was reclaimed and its storage reused (HIB-CV-32) — so the
/// 16 bytes no longer form a valid `Value`: the discriminant word holds a heap
/// pointer instead of a small enum tag. Decoding such bytes with
/// `ptr::read::<Value>` yields a `Value` with an out-of-range discriminant, and
/// the next `match` on it (e.g. the jump table in `CompactValue::from_value` at
/// a `ValueStack::push`) performs a wild indexed jump — a SIGSEGV with no
/// diagnosable context.
///
/// This decoder reads the discriminant word **as raw bits first** and rejects
/// an out-of-range value, returning `None`. The caller turns `None` into a
/// safe, diagnosable fallback (a benign null read) instead of constructing — or
/// ever matching on — a corrupt `Value`. It is the only sound place to bound
/// the discriminant: once the bytes are a `Value`, inspecting them is already
/// UB.
///
/// # Safety
/// `ptr` must be a readable, 8-byte-aligned pointer to a 16-byte `Value` slot.
#[inline]
pub unsafe fn read_value_checked(ptr: *const Value) -> Option<Value> {
    // Read the discriminant as raw bits — NOT as a `Value` — so an out-of-range
    // tag never reaches a `match`. Pinned to word-0 low-32 by the layout test.
    let disc = std::ptr::read(ptr as *const u32);
    if disc > VALUE_MAX_DISCRIMINANT {
        return None;
    }
    // Discriminant is in range; the bytes form a valid `Value`.
    Some(std::ptr::read(ptr))
}

/// Atomic counterpart of [`read_value_checked`] — validates the discriminant
/// **and** reads the slot tear-free, for a field slot that can be
/// concurrently written by another mutator thread doing a plain (non-JIT)
/// `putfield`.
///
/// `read_value_checked` reads via plain `ptr::read`, which is fine for a
/// slot no other thread can be concurrently mutating (e.g. a one-shot
/// forensic scan after the fact). But the interpreter's plain `get_field` —
/// used for ordinary, non-`volatile` Java fields — can race a concurrent
/// plain `set_field` on the SAME slot from another mutator thread. Real JDK
/// library code legally relies on exactly this being tear-free (e.g.
/// `ReentrantReadWriteLock$Sync`'s plain `firstReader`/`firstReaderHoldCount`
/// fields, published via a nearby `volatile`/CAS write to `state` — see
/// fixed-suite-bugs/elasticsearch-suite/elasticsearch-lucene-binary-docvalues-range-hangs.md
/// #3). This combines [`read_value_atomic`]'s tear-free two-word read with
/// `read_value_checked`'s discriminant validation, so a corrupted slot still
/// safely returns `None` instead of risking a wild jump-table match.
///
/// # Safety
/// `ptr` must be a readable, 8-byte-aligned pointer to a 16-byte `Value` slot.
#[inline]
pub unsafe fn read_value_checked_atomic(ptr: *const Value) -> Option<Value> {
    let w0 = (*(ptr as *const AtomicU64)).load(Ordering::Relaxed);
    let disc = w0 as u32;
    if disc > VALUE_MAX_DISCRIMINANT {
        return None;
    }
    let w1 = (*((ptr as *const u8).add(8) as *const AtomicU64)).load(Ordering::Relaxed);
    Some(std::mem::transmute::<[u64; 2], Value>([w0, w1]))
}

impl fmt::Display for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Value::Int(v) => write!(f, "int({v})"),
            Value::Long(v) => write!(f, "long({v})"),
            Value::Float(v) => write!(f, "float({v})"),
            Value::Double(v) => write!(f, "double({v})"),
            Value::Object(None) => write!(f, "null"),
            Value::Object(Some(r)) => write!(f, "ref({:p})", r.as_ptr()),
            Value::ReturnAddress(addr) => write!(f, "retaddr({addr})"),
            Value::Uninitialized => write!(f, "<uninitialized>"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn value_int() {
        let v = Value::Int(42);
        assert_eq!(v.as_int(), Some(42));
        assert!(!v.is_category2());
        assert!(!v.is_null());
    }

    #[test]
    fn value_long_is_category2() {
        let v = Value::Long(100);
        assert!(v.is_category2());
    }

    #[test]
    fn jlong_bits_as_aligned_object_ptr_matches_coerce_contract() {
        assert_eq!(jlong_bits_as_aligned_object_ptr(0), None);
        assert_eq!(jlong_bits_as_aligned_object_ptr(4), None);
        assert_eq!(jlong_bits_as_aligned_object_ptr(8), None);
        assert_eq!(
            jlong_bits_as_aligned_object_ptr(0x0000_5DDD_EEEE_F000),
            None
        );
        let known = 0x0000_5555_7777_8000usize;
        let _obj = unsafe { ObjectRef::from_raw(known as *mut u8) };
        assert_eq!(jlong_bits_as_aligned_object_ptr(known as u64), Some(known));
        assert_eq!(jlong_bits_as_aligned_object_ptr(1u64 << 47), None);
    }

    #[test]
    fn value_null() {
        let v = Value::Object(None);
        assert!(v.is_null());
    }

    #[test]
    fn value_discriminant_is_byte0_low32() {
        // `read_value_checked` validates corruption by reading the discriminant
        // as the low 32 bits of word 0. This pins that compiler-chosen layout:
        // every variant must encode its declaration-order discriminant there,
        // and every valid discriminant must be <= VALUE_MAX_DISCRIMINANT. If
        // the layout ever drifts (reordering the tag, or moving it off word 0),
        // this fails loudly — otherwise `read_value_checked` could reject valid
        // values (e.g. an `Int` whose payload aliases byte 0) or pass corrupt
        // ones.
        let cases: [(Value, u32); 7] = [
            (Value::Int(0), 0),
            (Value::Long(0), 1),
            (Value::Float(0.0), 2),
            (Value::Double(0.0), 3),
            (Value::Object(None), 4),
            (Value::ReturnAddress(0), 5),
            (Value::Uninitialized, 6),
        ];
        for (v, disc) in cases {
            // SAFETY: Value is asserted to be exactly 16 bytes; reinterpreting
            // it as two u64 words reads its own initialized bytes.
            let words = unsafe { std::mem::transmute::<Value, [u64; 2]>(v) };
            assert_eq!(
                words[0] as u32, disc,
                "discriminant of {v:?} is not at word-0 low-32"
            );
            assert!(disc <= VALUE_MAX_DISCRIMINANT);
        }
    }

    /// [`value_words`] agrees with the compiler's own layout for every byte
    /// the variant actually USES, and writes zero for every byte it does not.
    ///
    /// Asserted as a round trip through the real reader rather than against a
    /// hand-written byte pattern: a hand-written pattern would re-state the
    /// layout this file already pins in `value_discriminant_is_byte0_low32`,
    /// and would pass even if `value_words` and `read_value_atomic` drifted
    /// together. The round trip fails the moment they disagree.
    #[test]
    fn value_words_round_trips_through_the_atomic_reader() {
        for v in [
            Value::Int(i32::MIN),
            Value::Int(1),
            Value::Long(i64::MIN),
            Value::Float(-0.0),
            Value::Double(f64::NAN),
            Value::Object(None),
            Value::ReturnAddress(u32::MAX),
            Value::Uninitialized,
        ] {
            let mut cell = value_words(v);
            // SAFETY: `cell` is a 16-byte, 8-aligned buffer holding the words
            // `value_words` produced for a valid `Value`.
            let got = unsafe { read_value_atomic(cell.as_mut_ptr() as *const Value) };
            match (v, got) {
                // NaN is not `==` itself, so compare the bit pattern.
                (Value::Double(a), Value::Double(b)) => assert_eq!(a.to_bits(), b.to_bits()),
                (Value::Float(a), Value::Float(b)) => assert_eq!(a.to_bits(), b.to_bits()),
                _ => assert_eq!(got, v, "value_words({v:?}) did not round-trip"),
            }
        }
    }

    /// The invariant the containment argument rests on: a **narrow** variant
    /// leaves the cell's 64-bit payload word — the word a compiled reference
    /// `getfield` dereferences — as a hard zero, not as stack padding.
    ///
    /// Written against a deliberately DIRTIED destination, because that is the
    /// failure this pins: `transmute::<Value, [u64; 2]>` propagated whatever
    /// the caller's temp held into `FIELD_CELL_PAYLOAD64_OFFSET`, so a cell
    /// that already held a pointer-shaped word could keep it under an `Int`
    /// tag. Zero here is what turns a punned slot into a benign null instead of
    /// a wild pointer.
    #[test]
    fn narrow_variants_zero_the_payload64_word() {
        for v in [
            Value::Int(1),
            Value::Int(-1),
            Value::Float(1.0),
            Value::ReturnAddress(9),
            Value::Uninitialized,
        ] {
            let mut cell: [u64; 2] = [0xDEAD_BEEF_DEAD_BEEF, 0xCAFE_F00D_CAFE_F00D];
            // SAFETY: `cell` is a 16-byte, 8-aligned writable buffer.
            unsafe { write_value_atomic(cell.as_mut_ptr() as *mut Value, v) };
            assert_eq!(
                cell[1], 0,
                "{v:?} left the payload64 word non-zero — a compiled reference \
                 getfield would dereference {:#x}",
                cell[1]
            );
        }
        // And the wide variants still carry their payload in that word.
        let mut cell: [u64; 2] = [0, 0];
        // SAFETY: as above.
        unsafe { write_value_atomic(cell.as_mut_ptr() as *mut Value, Value::Long(0x1234_5678)) };
        assert_eq!(cell[1], 0x1234_5678);
    }

    #[test]
    fn read_value_checked_round_trips_valid_and_rejects_corrupt() {
        // Valid values decode unchanged.
        for v in [
            Value::Int(97),
            Value::Long(-1),
            Value::Double(1.5),
            Value::Object(None),
            Value::ReturnAddress(7),
            Value::Uninitialized,
        ] {
            // SAFETY: &v is a valid, aligned 16-byte Value slot.
            let got = unsafe { read_value_checked(&v as *const Value) };
            assert_eq!(got, Some(v));
        }
        // A slot whose discriminant word is a heap-pointer-shaped value (the
        // HIB-CV-32 corruption shape: {disc = pointer-low-32, payload = 6}) is
        // rejected rather than decoded into a UB-on-match `Value`.
        let corrupt: [u64; 2] = [0x0000_0001_02e9_1188, 6];
        // SAFETY: `corrupt` is a 16-byte, 8-aligned buffer; we only read it.
        let got = unsafe { read_value_checked(corrupt.as_ptr() as *const Value) };
        assert_eq!(got, None);
    }

    #[test]
    fn value_display() {
        assert_eq!(format!("{}", Value::Int(42)), "int(42)");
        assert_eq!(format!("{}", Value::Object(None)), "null");
        assert_eq!(format!("{}", Value::Uninitialized), "<uninitialized>");
    }

    #[test]
    fn encode_decode_int() {
        let (val, tag) = encode_value(Value::Int(42));
        assert_eq!(decode_value(val, tag).as_int(), Some(42));
        let (val, tag) = encode_value(Value::Int(-1));
        assert_eq!(decode_value(val, tag).as_int(), Some(-1));
        let (val, tag) = encode_value(Value::Int(i32::MAX));
        assert_eq!(decode_value(val, tag).as_int(), Some(i32::MAX));
        let (val, tag) = encode_value(Value::Int(i32::MIN));
        assert_eq!(decode_value(val, tag).as_int(), Some(i32::MIN));
    }

    #[test]
    fn encode_decode_long() {
        let (val, tag) = encode_value(Value::Long(123456789012345));
        assert_eq!(decode_value(val, tag).as_long(), Some(123456789012345));
        let (val, tag) = encode_value(Value::Long(-1));
        assert_eq!(decode_value(val, tag).as_long(), Some(-1));
        let (val, tag) = encode_value(Value::Long(i64::MAX));
        assert_eq!(decode_value(val, tag).as_long(), Some(i64::MAX));
        let (val, tag) = encode_value(Value::Long(i64::MIN));
        assert_eq!(decode_value(val, tag).as_long(), Some(i64::MIN));
    }

    #[test]
    fn encode_decode_float() {
        let (val, tag) = encode_value(Value::Float(3.15));
        let decoded = decode_value(val, tag).as_float().unwrap();
        assert!((decoded - 3.15).abs() < 1e-6);
        let (val, tag) = encode_value(Value::Float(-0.0));
        assert_eq!(
            decode_value(val, tag).as_float().unwrap().to_bits(),
            (-0.0f32).to_bits()
        );
    }

    #[test]
    fn encode_decode_double() {
        let (val, tag) = encode_value(Value::Double(2.719281828));
        let decoded = decode_value(val, tag).as_double().unwrap();
        assert!((decoded - 2.719281828).abs() < 1e-9);
    }

    #[test]
    fn encode_decode_null() {
        let (val, tag) = encode_value(Value::Object(None));
        assert!(decode_value(val, tag).is_null());
    }

    #[test]
    fn encode_decode_uninit() {
        let (val, tag) = encode_value(Value::Uninitialized);
        assert_eq!(decode_value(val, tag), Value::Uninitialized);
    }

    #[test]
    fn encode_decode_object_ref() {
        // Use an aligned pointer (multiple of 8)
        let fake_ptr = 0x1234_5678_ABC0_u64 as *mut u8;
        let obj = unsafe { ObjectRef::from_raw(fake_ptr) };
        let (val, tag) = encode_value(Value::Object(Some(obj)));
        assert!(is_object_tag(tag));
        let decoded = decode_value(val, tag);
        assert!(matches!(decoded, Value::Object(Some(r)) if r.as_ptr() as u64 == 0x1234_5678_ABC0));
        let checked = decode_value_checked(val, tag, |addr| addr == val);
        assert!(matches!(checked, Value::Object(Some(r)) if r.as_ptr() as u64 == 0x1234_5678_ABC0));
    }

    #[test]
    fn decode_value_rejects_unseen_plausible_object_payload() {
        let _guard = crate::compact_value::degrade_counter_test_lock();
        let unseen = 0x0000_6AAA_BBBB_C000u64;
        assert!(plausible_heap_pointer(unseen));
        assert!(matches!(
            decode_value(unseen, VTAG_OBJECT),
            Value::Object(None)
        ));
        assert!(matches!(
            decode_value_checked(unseen, VTAG_OBJECT, |_| false),
            Value::Object(None)
        ));
    }

    /// Pins the provenance-bitmap granule semantics: recording a reference
    /// marks its whole 64-byte granule known (a documented false-positive
    /// tradeoff vs. the old exact set — see the module note on
    /// PROVENANCE_L1), while the neighboring granule stays unknown.
    ///
    /// Uses an address region no other test records into, since the bitmap
    /// is process-global and `cargo test` shares one process.
    #[test]
    fn provenance_bitmap_granule_semantics() {
        let base = 0x0000_4A11_2233_4400u64; // 64-byte aligned, unique region
        assert!(!object_ref_payload_is_known(base));
        assert!(!object_ref_payload_is_known(base + 0x40));
        let _obj = unsafe { ObjectRef::from_raw(base as *mut u8) };
        // The recorded address and its granule-mates are known …
        assert!(object_ref_payload_is_known(base));
        assert!(object_ref_payload_is_known(base + 8));
        assert!(object_ref_payload_is_known(base + 0x38));
        // … the adjacent granule is not, and implausible bits never are.
        assert!(!object_ref_payload_is_known(base + 0x40));
        assert!(!object_ref_payload_is_known(base + 1)); // unaligned
        assert!(!object_ref_payload_is_known(0));
    }

    /// The per-thread `PROVENANCE_MEMO` must never suppress a *global* record.
    /// It remembers granule bits per 4 KiB word, so the danger is a second
    /// address in the same word being answered from the memo and never
    /// reaching the bitmap. Record two granules in one word, then a third in
    /// the next word, and check each is independently known while their
    /// untouched neighbours are not.
    #[test]
    fn provenance_memo_does_not_suppress_records_within_a_word() {
        // 4 KiB aligned, in a region no other test records into.
        let word_base = 0x0000_4A11_2244_0000u64;
        let a = word_base;
        let b = word_base + 0x40 * 17; // same word (one word covers 64 granules)
        let c = word_base + 0x1000; // first granule of the NEXT word
        for addr in [a, b, c] {
            assert!(!object_ref_payload_is_known(addr));
        }
        let _pa = unsafe { ObjectRef::from_raw(a as *mut u8) };
        // `b` must take the slow path even though the memo now holds this
        // word: its own granule bit is still clear.
        let _pb = unsafe { ObjectRef::from_raw(b as *mut u8) };
        let _pc = unsafe { ObjectRef::from_raw(c as *mut u8) };
        for addr in [a, b, c] {
            assert!(object_ref_payload_is_known(addr), "{addr:#x} not recorded");
        }
        // Re-recording `a` after the memo has moved on to `c`'s word must
        // still leave it known (idempotence, not just first-write).
        let _pa2 = unsafe { ObjectRef::from_raw(a as *mut u8) };
        assert!(object_ref_payload_is_known(a));
        // Untouched neighbours in both words stay unknown.
        assert!(!object_ref_payload_is_known(a + 0x40));
        assert!(!object_ref_payload_is_known(c + 0x40));
    }

    #[test]
    fn encode_decode_retaddr() {
        let (val, tag) = encode_value(Value::ReturnAddress(999));
        assert!(matches!(decode_value(val, tag), Value::ReturnAddress(999)));
    }

    #[test]
    fn decode_value_rejects_null_object() {
        let _guard = crate::compact_value::degrade_counter_test_lock();
        // T14 graceful degradation: a VTAG_OBJECT tag paired with a null
        // pointer is treated as Value::Object(None) rather than panicking,
        // so corrupted or zero-initialized slots don't crash the VM.
        assert!(matches!(decode_value(0, VTAG_OBJECT), Value::Object(None)));
    }

    #[test]
    fn decode_value_rejects_unaligned_object() {
        let _guard = crate::compact_value::degrade_counter_test_lock();
        // T14 graceful degradation: a VTAG_OBJECT tag paired with an
        // unaligned (non-8-byte-aligned) pointer is treated as
        // Value::Object(None) rather than panicking (KC16 SIGSEGV audit).
        assert!(matches!(
            decode_value(0x1001, VTAG_OBJECT),
            Value::Object(None)
        ));
    }

    #[test]
    fn decode_value_rejects_null_page_object() {
        let _guard = crate::compact_value::degrade_counter_test_lock();
        assert!(matches!(decode_value(8, VTAG_OBJECT), Value::Object(None)));
        assert!(matches!(
            decode_value(0xff8, VTAG_OBJECT),
            Value::Object(None)
        ));
    }

    #[test]
    fn decode_value_rejects_out_of_range_object() {
        let _guard = crate::compact_value::degrade_counter_test_lock();
        assert!(matches!(
            decode_value(1u64 << 47, VTAG_OBJECT),
            Value::Object(None)
        ));
    }

    /// HIGH long↔object audit: a `VTAG_OBJECT` slot whose pointer is null or
    /// otherwise implausible degrades to `Object(None)` and must bump the
    /// shared degradation counter so the reclassification is countable in release.
    /// Mirrors the precedent of `decode_value_rejects_null_object` /
    /// `decode_value_rejects_unaligned_object`, which exercise the same
    /// release-mode degrade path.
    #[test]
    fn decode_value_object_degrade_increments_counter() {
        let _guard = crate::compact_value::degrade_counter_test_lock();
        use crate::compact_value::object_degradation_count;
        // DELTA, not a reset: the counter is process-wide and zeroing it would
        // break whatever other degrading test is mid-count. The guard above
        // keeps that "other test" from running concurrently at all.
        let base = object_degradation_count();
        // Null pointer with VTAG_OBJECT → degrade.
        assert!(matches!(decode_value(0, VTAG_OBJECT), Value::Object(None)));
        // Unaligned non-null pointer with VTAG_OBJECT → degrade.
        assert!(matches!(
            decode_value(0x1001, VTAG_OBJECT),
            Value::Object(None)
        ));
        // Aligned null-page pointer with VTAG_OBJECT -> degrade.
        assert!(matches!(decode_value(8, VTAG_OBJECT), Value::Object(None)));
        // Above the supported 47-bit user-space range -> degrade.
        assert!(matches!(
            decode_value(1u64 << 47, VTAG_OBJECT),
            Value::Object(None)
        ));
        assert!(
            object_degradation_count() - base >= 4,
            "expected at least 4 degradations recorded, got {}",
            object_degradation_count() - base
        );
    }

    #[test]
    fn object_ref_as_ptr_roundtrip() {
        let ptr = 0xDEAD_BEE0_u64 as *mut u8;
        let obj = unsafe { ObjectRef::from_raw(ptr) };
        assert_eq!(obj.as_ptr(), ptr);
    }

    // ---- single-OS-thread Send/Sync tripwire ----

    #[test]
    fn current_thread_token_is_nonzero_and_stable() {
        // Must never collide with the SINGLE_THREAD_GUARD_UNSET sentinel,
        // otherwise a freshly-claimed guard would look unclaimed.
        let a = current_thread_token();
        let b = current_thread_token();
        assert_ne!(a, SINGLE_THREAD_GUARD_UNSET);
        assert_eq!(a, b, "token must be stable for the same OS thread");
    }

    #[test]
    fn single_thread_guard_first_claim_then_same_thread_ok() {
        // A fresh guard cell: first claim succeeds, repeat from the same token
        // succeeds (no violation), exercising the cooperative-scheduler case.
        let guard = AtomicU64::new(SINGLE_THREAD_GUARD_UNSET);
        let token = current_thread_token();
        assert_eq!(check_single_thread_against(&guard, token), Ok(()));
        assert_eq!(check_single_thread_against(&guard, token), Ok(()));
        // The owner is now recorded as our token.
        assert_eq!(guard.load(Ordering::Acquire), token);
    }

    #[test]
    fn single_thread_guard_detects_second_thread() {
        // Simulate a second OS thread by claiming the guard with one token,
        // then probing with a different one — must report a violation that
        // hands back the recorded owner.
        let guard = AtomicU64::new(SINGLE_THREAD_GUARD_UNSET);
        let first = 0xAAAA_AAAA_AAAA_AAA1_u64; // low bit set, like real tokens
        let second = 0xBBBB_BBBB_BBBB_BBB1_u64;
        assert_eq!(check_single_thread_against(&guard, first), Ok(()));
        assert_eq!(check_single_thread_against(&guard, second), Err(first));
        // Recorded owner is unchanged by a failed probe.
        assert_eq!(guard.load(Ordering::Acquire), first);
    }

    #[test]
    fn single_thread_guard_concurrent_claim_loses_one() {
        // Two threads racing to construct the first ObjectRef: exactly one
        // wins the CAS; the other observes the winner's token and (since the
        // tokens differ) would trip the tripwire. This proves the guard does
        // not silently accept a genuine second OS thread.
        let guard = std::sync::Arc::new(AtomicU64::new(SINGLE_THREAD_GUARD_UNSET));
        let g2 = std::sync::Arc::clone(&guard);
        let token_a = 0x1111_1111_1111_1111_u64;
        let token_b = 0x2222_2222_2222_2223_u64;
        let handle = std::thread::spawn(move || check_single_thread_against(&g2, token_b));
        let r_a = check_single_thread_against(&guard, token_a);
        let r_b = handle.join().unwrap();
        // Exactly one of the two distinct tokens wins the claim.
        let recorded = guard.load(Ordering::Acquire);
        assert!(recorded == token_a || recorded == token_b);
        // The winner sees Ok, the loser sees Err(recorded-winner).
        let oks = [r_a, r_b].iter().filter(|r| r.is_ok()).count();
        assert_eq!(oks, 1, "exactly one claimant may win");
        let errs = [r_a, r_b];
        let err = errs.iter().find(|r| r.is_err()).unwrap();
        assert_eq!(*err, Err(recorded));
    }

    // ------------------------------------------------------------------
    // RawSlot — untagged, verifier-map-driven slots
    // ------------------------------------------------------------------

    /// The property that motivates the whole design. `CompactValue::int(0)` and
    /// `CompactValue::long(0xFFFC_0000_0000_0000)` are bit-identical, so a
    /// tagged 8-byte slot cannot tell them apart without a side array. Untagged
    /// slots carry no tag bits at all, so they are *supposed* to be
    /// indistinguishable — and both decode correctly once the type comes from
    /// the caller.
    #[test]
    fn rawslot_int_and_colliding_long_share_bits_but_decode_correctly() {
        let collide: i64 = 0xFFFC_0000_0000_0000u64 as i64;

        let as_long = RawSlot::from_long(collide);
        let as_int = RawSlot::from_int(0);

        // The bit patterns genuinely differ here (a long keeps all 64 bits, an
        // int zero-extends 32), which is already better than the NaN-boxed
        // case — but that is incidental. What matters is the next assertion.
        assert_eq!(as_long.bits(), 0xFFFC_0000_0000_0000);
        assert_eq!(as_int.bits(), 0);

        // With the type supplied externally, both round-trip exactly.
        assert_eq!(as_long.decode(SlotType::Long), Value::Long(collide));
        assert_eq!(as_int.decode(SlotType::Int), Value::Int(0));
    }

    /// The stronger statement: for EVERY `i64`, the untagged bits are the value
    /// verbatim, so no long can ever be mistaken for anything. This is exactly
    /// what `CompactValue` cannot promise, and it is why the `kinds` side array
    /// becomes removable.
    #[test]
    fn rawslot_long_is_bit_exact_for_every_tag_pattern() {
        let hostile: [i64; 12] = [
            0,
            -1,
            i64::MIN,
            i64::MAX,
            0xFFFC_0000_0000_0000u64 as i64, // the NaN-box marker pattern
            0xFFFD_0000_0000_0000u64 as i64, // BouncyCastle LongArray lxor shape
            0xFFFE_0000_0000_0000u64 as i64,
            0xFFFF_FFFF_FFFF_FFF8u64 as i64,
            0x7FF8_0000_0000_0000u64 as i64, // canonical quiet NaN bits
            0x0000_5555_7777_8000u64 as i64, // looks exactly like a heap pointer
            1,
            -1234567890123456789,
        ];
        for v in hostile {
            let s = RawSlot::from_long(v);
            assert_eq!(s.bits(), v as u64, "long {v:#x} did not store verbatim");
            assert_eq!(s.as_long(), v);
            assert_eq!(s.decode(SlotType::Long), Value::Long(v));
        }
    }

    /// Every primitive slot type round-trips through `encode`/`decode`.
    #[test]
    fn rawslot_encode_decode_round_trip() {
        let cases = [
            Value::Int(0),
            Value::Int(-1),
            Value::Int(i32::MIN),
            Value::Int(i32::MAX),
            Value::Long(0),
            Value::Long(i64::MIN),
            Value::Float(0.0),
            Value::Float(-1.5),
            Value::Float(f32::MIN),
            Value::Double(0.0),
            Value::Double(3.141_592_653_589_793),
            Value::Double(f64::MAX),
            Value::Object(None),
            Value::ReturnAddress(0),
            Value::ReturnAddress(u32::MAX),
            Value::Uninitialized,
        ];
        for v in cases {
            let (slot, ty) = RawSlot::encode(v);
            assert_eq!(slot.decode(ty), v, "{v:?} did not round-trip");
        }
    }

    /// `RawSlot` stores a double verbatim — including NaN payloads that
    /// `CompactValue::double` is forced to canonicalise into the hardware quiet
    /// NaN because they collide with its tag space.
    #[test]
    fn rawslot_preserves_nan_payloads_compactvalue_must_canonicalise() {
        // A NaN whose bits set the NaN-box marker; CompactValue rewrites it.
        let exotic = f64::from_bits(0xFFFC_0000_0000_0001);
        assert!(exotic.is_nan());

        let compact = crate::compact_value::CompactValue::double(exotic);
        assert_ne!(
            compact.raw_bits(),
            0xFFFC_0000_0000_0001,
            "CompactValue is expected to canonicalise this NaN"
        );

        let raw = RawSlot::from_double(exotic);
        assert_eq!(raw.bits(), 0xFFFC_0000_0000_0001);
        assert!(raw.as_double().is_nan());
    }

    /// A reference slot holds a bare pointer; `oop()` is a plain read and
    /// `set_oop` a plain store, which is what lets a moving collector relocate a
    /// root without rebuilding tag bits.
    #[test]
    fn rawslot_reference_is_a_bare_pointer() {
        let addr = 0x0000_5555_7777_8000u64;
        let mut slot = RawSlot::from_oop(addr);
        assert_eq!(slot.bits(), addr, "reference must NOT be NaN-boxed");
        assert_eq!(slot.oop(), Some(addr));

        // Relocation.
        let moved = 0x0000_5555_7777_9000u64;
        slot.set_oop(moved);
        assert_eq!(slot.oop(), Some(moved));
        assert_eq!(slot.bits(), moved);

        // Null.
        let null = RawSlot::from_oop(0);
        assert_eq!(null.oop(), None);
        assert_eq!(null.decode(SlotType::Reference), Value::Object(None));
        assert_eq!(RawSlot::ZERO, null);
    }

    /// `decode(Reference)` must never fabricate an `ObjectRef` from bits that
    /// cannot be a heap address, even though the verifier map claimed the slot
    /// is a reference — a corrupt frame degrades to null instead of producing a
    /// wild pointer that the caller would dereference.
    #[test]
    fn rawslot_decode_reference_degrades_implausible_bits() {
        let _guard = crate::compact_value::degrade_counter_test_lock();
        for bad in [1u64, 4, 0xFFF, (1u64 << 47) | 8] {
            let slot = RawSlot::from_bits(bad);
            assert_eq!(
                slot.decode(SlotType::Reference),
                Value::Object(None),
                "implausible pointer {bad:#x} must degrade to null"
            );
        }
    }

    /// Unlike `decode_value`, the untagged reference decode does NOT consult the
    /// provenance bitmap: a pointer that never crossed `ObjectRef::from_raw` is
    /// still decoded, because the verifier map — not a heuristic — is the
    /// authority. Dropping that probe is the throughput argument for untagged
    /// slots, so pin it.
    #[test]
    fn rawslot_decode_reference_needs_no_provenance_record() {
        let _guard = crate::compact_value::degrade_counter_test_lock();
        // Deliberately an address this process has never constructed an
        // ObjectRef for. Plausible (aligned, above the guard page, < 2^47) but
        // unknown to the provenance bitmap.
        let never_seen = 0x0000_4242_4242_4000u64;
        assert!(
            !object_ref_payload_is_known(never_seen),
            "test precondition: this address must be unrecorded"
        );

        // The tagged SoA decoder rejects it...
        assert_eq!(
            decode_value(never_seen, VTAG_OBJECT),
            Value::Object(None),
            "decode_value is expected to reject an unrecorded pointer"
        );

        // ...the untagged decoder accepts it, on the verifier's authority.
        match RawSlot::from_oop(never_seen).decode(SlotType::Reference) {
            Value::Object(Some(r)) => assert_eq!(r.as_ptr() as u64, never_seen),
            other => panic!("expected an object reference, got {other:?}"),
        }
        assert_eq!(RawSlot::from_oop(never_seen).oop(), Some(never_seen));
    }

    /// The migration bridge: a frame can be half-converted, so `RawSlot` and
    /// `CompactValue` must agree on every value they can both represent.
    #[test]
    fn rawslot_compact_value_bridge_round_trips() {
        let cases: [(RawSlot, SlotType); 9] = [
            (RawSlot::from_int(-7), SlotType::Int),
            (RawSlot::from_int(i32::MIN), SlotType::Int),
            (RawSlot::from_long(i64::MIN), SlotType::Long),
            (
                RawSlot::from_long(0xFFFC_0000_0000_0000u64 as i64),
                SlotType::Long,
            ),
            (RawSlot::from_float(-2.5), SlotType::Float),
            (RawSlot::from_double(1.25), SlotType::Double),
            (RawSlot::from_oop(0), SlotType::Reference),
            (RawSlot::from_return_address(99), SlotType::ReturnAddress),
            (RawSlot::ZERO, SlotType::Uninitialized),
        ];
        for (slot, ty) in cases {
            let compact = slot.to_compact(ty);
            let back = RawSlot::from_compact(compact, ty);
            assert_eq!(back, slot, "{slot:?} as {ty:?} did not survive the bridge");
        }

        // A non-null reference needs a plausible address for CompactValue's
        // (release-active) constructor assertions.
        let addr = 0x0000_5555_7777_8000u64;
        let slot = RawSlot::from_oop(addr);
        let compact = slot.to_compact(SlotType::Reference);
        assert_eq!(compact.as_object_ptr(), Some(addr));
        assert_eq!(RawSlot::from_compact(compact, SlotType::Reference), slot);
    }

    /// `RawSlot` must stay layout-identical to `u64` / `CompactValue` so the
    /// existing frame-pool `Vec<u64>` buffers can back untagged frames and the
    /// three GC scanners that transmute `Vec<u64> <-> Vec<CompactValue>` keep
    /// working during the migration.
    #[test]
    fn rawslot_is_layout_compatible_with_u64_and_compact_value() {
        use crate::compact_value::CompactValue;
        assert_eq!(std::mem::size_of::<RawSlot>(), std::mem::size_of::<u64>());
        assert_eq!(std::mem::align_of::<RawSlot>(), std::mem::align_of::<u64>());
        assert_eq!(
            std::mem::size_of::<RawSlot>(),
            std::mem::size_of::<CompactValue>()
        );

        let v: [RawSlot; 2] = [RawSlot::from_long(-1), RawSlot::from_int(5)];
        // SAFETY: RawSlot is repr(transparent) over u64 with identical size and
        // alignment, so a slice of one reinterprets as a slice of the other —
        // the same property the frame pool relies on for Vec<u64> reuse.
        let as_u64: &[u64] =
            unsafe { std::slice::from_raw_parts(v.as_ptr().cast::<u64>(), v.len()) };
        assert_eq!(as_u64, &[u64::MAX, 5]);
    }

    /// Descriptor-driven decode, the call/return-boundary type source.
    #[test]
    fn rawslot_decode_by_descriptor() {
        assert_eq!(
            RawSlot::from_int(1).decode_by_descriptor(b'Z'),
            Value::Int(1)
        );
        assert_eq!(
            RawSlot::from_int(-1).decode_by_descriptor(b'I'),
            Value::Int(-1)
        );
        assert_eq!(
            RawSlot::from_long(i64::MIN).decode_by_descriptor(b'J'),
            Value::Long(i64::MIN)
        );
        assert_eq!(
            RawSlot::from_double(0.5).decode_by_descriptor(b'D'),
            Value::Double(0.5)
        );
        assert_eq!(
            RawSlot::from_oop(0).decode_by_descriptor(b'L'),
            Value::Object(None)
        );
        assert_eq!(
            RawSlot::from_oop(0).decode_by_descriptor(b'['),
            Value::Object(None)
        );
        // `V` and junk are not slot types.
        assert_eq!(
            RawSlot::ZERO.decode_by_descriptor(b'V'),
            Value::Uninitialized
        );
        assert_eq!(
            RawSlot::ZERO.decode_by_descriptor(b'?'),
            Value::Uninitialized
        );
    }

    #[test]
    fn slot_type_classification() {
        assert!(SlotType::Reference.is_reference());
        assert!(!SlotType::Long.is_reference());
        assert!(SlotType::Long.is_category2());
        assert!(SlotType::Double.is_category2());
        assert!(!SlotType::Int.is_category2());
        assert!(!SlotType::Reference.is_category2());

        assert_eq!(SlotType::from_descriptor_byte(b'J'), Some(SlotType::Long));
        assert_eq!(SlotType::from_descriptor_byte(b'C'), Some(SlotType::Int));
        assert_eq!(
            SlotType::from_descriptor_byte(b'['),
            Some(SlotType::Reference)
        );
        assert_eq!(SlotType::from_descriptor_byte(b'V'), None);
    }

    /// A `RawSlot` is 8 bytes in both narrow-oop configurations: frame slots are
    /// not heap slots and are never narrowed. Pins the interaction so a future
    /// compressed-oops change cannot silently narrow the interpreter's frames.
    #[test]
    fn rawslot_width_is_independent_of_compressed_oops() {
        assert_eq!(std::mem::size_of::<RawSlot>(), 8);
        // Heap reference slots do vary; frame slots must not.
        assert!(
            crate::narrow_oop::ref_field_size() == 4 || crate::narrow_oop::ref_field_size() == 8
        );
        let addr = 0x0000_5555_7777_8000u64;
        assert_eq!(
            RawSlot::from_oop(addr).oop(),
            Some(addr),
            "RawSlot holds a decoded 64-bit address regardless of narrow oops"
        );
    }

    #[test]
    fn send_sync_invariant_layout_holds() {
        // The Send/Sync impls assume ObjectRef stays a bare pointer-sized
        // value (no added synchronization state). Re-assert it here so a field
        // addition that would invalidate the soundness argument is caught.
        assert_eq!(
            std::mem::size_of::<ObjectRef>(),
            std::mem::size_of::<*mut u8>()
        );
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<ObjectRef>();
    }

    // -----------------------------------------------------------------------
    // `ObjectRef` concurrency contract
    // (docs/threading/objectref-concurrency-contract.md)
    //
    // These pin the parts of the contract that are checkable from inside
    // `types`. The parts that are NOT — root coverage, permitted GC phase,
    // staleness across safepoints and blocking regions — live in `vm`/`gc` and
    // are listed as unenforced invariants in §7 of the doc.
    // -----------------------------------------------------------------------

    fn assert_send<T: Send>() {}
    fn assert_sync<T: Sync>() {}

    /// §6 of the contract: `Send` and `Sync` are asserted separately, and they
    /// must survive the compositions the VM actually relies on to move roots
    /// between OS threads (`Arc<Mutex<Vec<ObjectRef>>>` is literally
    /// `ThreadRegistry`'s `root_snapshot`).
    #[test]
    fn objectref_send_and_sync_bounds_hold_for_the_shapes_the_vm_uses() {
        assert_send::<ObjectRef>();
        assert_sync::<ObjectRef>();
        assert_send::<Option<ObjectRef>>();
        assert_sync::<Option<ObjectRef>>();
        assert_send::<Value>();
        assert_sync::<Value>();
        assert_send::<Vec<ObjectRef>>();
        assert_sync::<Vec<ObjectRef>>();
        // The cross-thread root-publication shape.
        assert_send::<std::sync::Arc<std::sync::Mutex<Vec<ObjectRef>>>>();
        assert_sync::<std::sync::Arc<std::sync::Mutex<Vec<ObjectRef>>>>();
        // The pointer-map rewrite shape.
        assert_send::<std::collections::HashMap<ObjectRef, usize>>();
    }

    /// §2: copying an `ObjectRef` is a bit copy. Both copies name the same
    /// address, compare equal, hash equal, and neither is invalidated by the
    /// other — which is exactly why `Send` is sound (nothing is transferred,
    /// unshared or dropped).
    #[test]
    fn objectref_copy_semantics_are_a_pure_bit_copy() {
        let addr = 0x0000_5555_7777_9000usize;
        // SAFETY: aligned, non-null sentinel address; never dereferenced here.
        let a = unsafe { ObjectRef::from_raw(addr as *mut u8) };
        let b = a; // Copy, not a move-out.
        assert_eq!(a, b);
        assert_eq!(a.as_ptr(), b.as_ptr());
        assert_eq!(a.as_nonnull(), b.as_nonnull());
        assert_eq!(a.as_ptr() as usize, addr);
        // `a` is still usable after `b` was created — no move occurred.
        assert_eq!(a.as_ptr() as usize, addr);

        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};
        let hash_of = |r: ObjectRef| {
            let mut h = DefaultHasher::new();
            r.hash(&mut h);
            h.finish()
        };
        assert_eq!(hash_of(a), hash_of(b), "Hash must follow address equality");

        // Address identity, not object identity: a DIFFERENT address is a
        // different `ObjectRef` even if a moving collection would call them
        // the same object. This is unenforced-invariant #3 in the doc.
        // SAFETY: aligned, non-null sentinel address; never dereferenced.
        let moved = unsafe { ObjectRef::from_raw((addr + 0x1000) as *mut u8) };
        assert_ne!(a, moved);
        assert_ne!(hash_of(a), hash_of(moved));
    }

    /// §2: `None` is the all-zero niche. This is what lets `Value` stay 16
    /// bytes, and it is why a torn/zeroed 16-byte slot decodes as `Int(0)`
    /// rather than as a null reference (see `read_slot`'s R-niche rule in
    /// `gc/src/heap.rs`).
    #[test]
    fn objectref_none_is_the_all_zero_niche() {
        assert_eq!(
            std::mem::size_of::<Option<ObjectRef>>(),
            std::mem::size_of::<ObjectRef>()
        );
        // SAFETY: both types are pointer-sized (asserted above and at compile
        // time); reading `None`'s own initialized bytes as a `usize`.
        let none_bits: usize = unsafe { std::mem::transmute(None::<ObjectRef>) };
        assert_eq!(none_bits, 0, "None must be the zero bit pattern");

        // SAFETY: aligned, non-null sentinel address; never dereferenced.
        let some = Some(unsafe { ObjectRef::from_raw(0x0000_5555_7777_A000usize as *mut u8) });
        // SAFETY: same size/validity argument as above.
        let some_bits: usize = unsafe { std::mem::transmute(some) };
        assert_eq!(some_bits, 0x0000_5555_7777_A000);

        assert!(Value::Object(None).is_null());
        assert!(!Value::Object(some).is_null());
    }

    /// The local half of "is this reference plausible?". Reports rather than
    /// panics precisely because the workspace mints out-of-range sentinel refs
    /// — pinning that behaviour so a future hard assert in `as_ptr` is a
    /// deliberate decision, not an accident.
    #[test]
    fn objectref_plausibility_reports_and_does_not_panic() {
        // A realistic heap address.
        // SAFETY: aligned, non-null; never dereferenced.
        let ok = unsafe { ObjectRef::from_raw(0x0000_5555_7777_B000usize as *mut u8) };
        assert!(ok.is_plausible_heap_pointer());

        // A sentinel above the 47-bit user-address window, of the shape the
        // workspace actually uses for synthetic identities. `as_ptr` must NOT
        // panic on it (debug builds included) — only the report says "no".
        let sentinel = 0xdead_beef_0000_0100usize;
        assert_eq!(sentinel % 8, 0, "sentinel must still be 8-byte aligned");
        // SAFETY: aligned, non-null; never dereferenced.
        let odd = unsafe { ObjectRef::from_raw(sentinel as *mut u8) };
        assert_eq!(odd.as_ptr() as usize, sentinel);
        assert!(!odd.is_plausible_heap_pointer());

        // And the null-guard page is rejected by the same predicate.
        assert!(!plausible_heap_pointer(0x8));
        assert!(!plausible_heap_pointer(0));
    }

    /// §2 layout invariant, checked at runtime as well as at compile time so a
    /// `cfg`-dependent field addition cannot slip past the `const _` asserts on
    /// one target only.
    #[test]
    fn objectref_layout_is_pinned_for_size_and_alignment() {
        assert_eq!(
            std::mem::size_of::<ObjectRef>(),
            std::mem::size_of::<*mut u8>()
        );
        assert_eq!(
            std::mem::align_of::<ObjectRef>(),
            std::mem::align_of::<*mut u8>()
        );
        assert_eq!(
            std::mem::align_of::<Option<ObjectRef>>(),
            std::mem::align_of::<*mut u8>()
        );
        assert_eq!(std::mem::size_of::<Value>(), 16);
        assert!(std::mem::align_of::<Value>() <= 8);
    }

    /// §6 (1): `ObjectRef` must stay `Drop`-free. A `Drop` impl would make the
    /// `Send` argument ("sending transfers a bit pattern; nothing is
    /// deallocated") false, and would also break `Copy`.
    #[test]
    fn objectref_has_no_drop_glue() {
        assert!(
            !std::mem::needs_drop::<ObjectRef>(),
            "ObjectRef must have no drop glue — see contract doc §6(1)"
        );
        assert!(!std::mem::needs_drop::<Option<ObjectRef>>());
        assert!(!std::mem::needs_drop::<Value>());
    }

    /// The contract explicitly permits copying an `ObjectRef` to another OS
    /// thread and reading it there. Exercised for real (not just via trait
    /// bounds) so the `Send`/`Sync` claim is checked by execution too.
    #[test]
    fn objectref_survives_a_real_cross_thread_transfer() {
        let addr = 0x0000_5555_7777_C000usize;
        // SAFETY: aligned, non-null sentinel address; never dereferenced.
        let r = unsafe { ObjectRef::from_raw(addr as *mut u8) };

        // Send: move a copy into another OS thread.
        let moved = std::thread::spawn(move || r.as_ptr() as usize)
            .join()
            .expect("worker thread panicked");
        assert_eq!(moved, addr);

        // Sync: share `&ObjectRef` across threads.
        let shared = std::sync::Arc::new(r);
        let mut handles = Vec::new();
        for _ in 0..4 {
            let s = std::sync::Arc::clone(&shared);
            handles.push(std::thread::spawn(move || s.as_ptr() as usize));
        }
        for h in handles {
            assert_eq!(h.join().expect("worker thread panicked"), addr);
        }
    }
}
