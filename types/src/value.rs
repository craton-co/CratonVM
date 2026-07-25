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
/// **Layout invariant:** this enum is compile-time asserted below to be
/// exactly 16 bytes with alignment ≤ 8 on x86-64 / AArch64.  Adding `repr(C)`
/// would change size to 24 bytes and break JIT slot layout.  The static
/// asserts at the bottom of this file own the invariant; the `jit` crate
/// replicates them as belt-and-suspenders.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Value {
    /// A 32-bit integer (also used for boolean, byte, char, short).
    Int(i32),

    /// A 64-bit long integer. Occupies two stack/local slots.
    Long(i64),

    /// A 32-bit IEEE 754 float.
    Float(f32),

    /// A 64-bit IEEE 754 double. Occupies two stack/local slots.
    Double(f64),

    /// A reference to an object or array. Represented as a raw pointer internally.
    /// `None` represents the `null` reference.
    Object(Option<ObjectRef>),

    /// A return address for `jsr`/`ret` instructions (used by older `finally` implementations).
    ReturnAddress(u32),

    /// Uninitialized slot placeholder (e.g., second slot of a long/double, or unset local).
    Uninitialized,
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
// (e.g. in SharedVm.class_mirrors_reverse).  Hashing a raw pointer is
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

// ---------------------------------------------------------------------------
// MED (full-review-2026-06-20 #row "ObjectRef Send+Sync sound only by accident
// of the single-threaded scheduler"): single-OS-thread invariant tripwire.
// ---------------------------------------------------------------------------
//
// HISTORICAL NOTE — this tripwire was added when the VM ran every Java thread on
// one OS thread under cooperative scheduling, to fail loudly if a future
// threading change ever violated that assumption. **The assumption no longer
// holds**: `Thread.start` spawns a real OS thread per Java thread (see soundness
// argument point (4) below, which has been corrected). Any multithreaded Java
// program now trips this guard by design.
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

#[inline]
fn record_object_ref_payload(ptr: *mut u8) {
    let raw = ptr as u64;
    if !plausible_heap_pointer(raw) {
        return;
    }
    let (l1, word, bit) = provenance_indices(raw);
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
    if w.load(Ordering::Relaxed) & bit == 0 {
        w.fetch_or(bit, Ordering::Relaxed);
    }
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
            let on = std::env::var_os("CRATONVM_ASSERT_SINGLE_OS_THREAD")
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

/// Assert the single-OS-thread invariant the `Send`/`Sync` impls rely on, if
/// the tripwire is enabled. Cold and never-inlined so the enabled-check stays
/// a cheap predictable branch on the hot path.
#[cold]
#[inline(never)]
fn single_thread_guard_violation(recorded: u64, token: u64) -> ! {
    panic!(
        "ObjectRef constructed on a second OS thread (recorded={recorded:#x}, \
         current={token:#x}). The `unsafe impl Send/Sync for ObjectRef` is sound \
         only under single-OS-thread Java execution; multi-OS-thread execution \
         requires re-deriving Send/Sync (handle indirection or per-thread \
         transfer barriers). See the soundness note in types/src/value.rs."
    );
}

/// Hot-path entry: enforce the single-OS-thread invariant when the tripwire is
/// armed. A no-op (single relaxed load) otherwise.
#[inline]
fn enforce_single_os_thread() {
    if single_thread_guard_enabled() {
        let token = current_thread_token();
        if let Err(recorded) = check_single_thread_against(&SINGLE_THREAD_GUARD, token) {
            single_thread_guard_violation(recorded, token);
        }
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
        // MED tripwire: assert the single-OS-thread invariant the Send/Sync
        // impls rely on (opt-in via CRATONVM_ASSERT_SINGLE_OS_THREAD).
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
        // MED tripwire: assert the single-OS-thread invariant the Send/Sync
        // impls rely on (opt-in via CRATONVM_ASSERT_SINGLE_OS_THREAD).
        enforce_single_os_thread();
        record_object_ref_payload(ptr.as_ptr());
        Self { ptr }
    }

    pub fn as_ptr(&self) -> *mut u8 {
        self.ptr.as_ptr()
    }

    /// Return the underlying `NonNull<u8>` without going through a raw pointer round-trip.
    pub fn as_nonnull(&self) -> NonNull<u8> {
        self.ptr
    }
}

// SAFETY: ObjectRef implements Send and Sync.
//
// `NonNull<u8>` is `!Send + !Sync` by default (same as `*mut u8`), so the
// `unsafe impl`s below are still required after the A4 switch from
// `*mut u8` to `NonNull<u8>` — the niche optimization is a layout change,
// not an auto-trait change.
//
// Soundness argument:
//
// 1. **Lifetime guarantee** — The pointer refers to a GC-managed heap object.
//    The collector will not free an object while any root (thread stack, global
//    root set) holds an ObjectRef to it. Roots are scanned at safepoints.
//
// 2. **Mutation protocol** — All field reads/writes go through the VM's
//    field-access helpers (`get_field` / `set_field` on `NativeContext`), which
//    hold the appropriate monitor lock or use atomic operations for volatile
//    fields. Direct pointer mutation is never performed outside the GC.
//
// 3. **Compaction safety** — GC stop-the-world pauses ensure no thread
//    observes a half-moved object during relocation. Pointer updates happen
//    atomically from each thread's perspective.
//
// 4. **Current execution model** — one OS thread per Java thread. `Thread.start`
//    spawns a real `std::thread::Builder` worker (see `thread_start` in
//    `vm/src/vm/vm_exec.rs`, the `std::thread::Builder::new()` call around line
//    6781); virtual threads are multiplexed over those carriers by
//    `threading/virtual_scheduler.rs`. Java code therefore runs with genuine
//    preemptive OS-level parallelism, and (1)-(3) must hold concurrently.
//
// !!! THE RE-AUDIT THIS BLOCK DEMANDS IS OVERDUE — READ BEFORE TRUSTING (1)-(3) !!!
//
// This note previously stated that the VM executed all Java threads on a single
// OS thread under cooperative scheduling, and that these `unsafe impl`s were
// "sound by accident of the single-threaded scheduler" — with an explicit
// instruction to re-derive the `Send`/`Sync` claim from first principles "once
// `threading/jvm_thread.rs` spawns Java threads on multiple OS threads".
//
// **That trigger has already fired.** Multi-OS-thread Java execution is the
// current, default behaviour (point (4) above), and the stated re-audit does not
// appear to have happened — the premise simply went stale in place. Corrected
// here so the next reader is not misled into thinking the single-thread
// assumption still holds.
//
// What this correction does NOT do: it does not certify (1)-(3) as sound under
// parallelism, and it does not claim they are broken. Neither conclusion has been
// established. `ObjectRef` remains a bare, non-atomic, GC-unmanaged raw pointer
// with no lifetime tracking, and the three properties it depends on are exactly
// the ones that need a real concurrent audit:
//   - GC roots are scanned at safepoints across ALL OS threads (no thread can
//     hide an `ObjectRef` from the collector);
//   - every field access genuinely routes through the monitor/atomic helpers
//     in (2) — no raw pointer dereference escapes that protocol;
//   - relocation during compaction is observed atomically by every thread.
// The plausible reason these hold in practice is that the safepoint/STW protocol
// (`threading/gc_barrier.rs`) serialises relocation against every mutator, so no
// thread observes a moving object — but "plausible" is not the audit. Until that
// audit is written down, treat this `unsafe impl` as load-bearing and
// under-justified. If any property cannot be established, the fix is a handle
// indirection or an explicit `!Send` marker plus per-thread transfer barriers.
//
// Knock-on: the object-reference provenance bitmap earlier in this file justifies
// its `Ordering::Relaxed` accesses partly on the same now-false single-OS-thread
// premise. Those orderings need re-deriving alongside this argument.
//
// TRIPWIRE: `enforce_single_os_thread()` in `ObjectRef::from_raw` /
// `from_raw_nonnull`, opt-in via `CRATONVM_ASSERT_SINGLE_OS_THREAD`, aborts if a
// second OS thread ever constructs an `ObjectRef`. Note what this means now that
// point (4) has changed: it is no longer a guard against a *future* regression —
// any multithreaded Java program trips it by design. It survives only as a
// diagnostic for confirming that a specific workload really is single-threaded
// (e.g. when bisecting whether a bug needs parallelism to reproduce). Do not
// enable it in a multithreaded run and do not read a trip as evidence of a bug.
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
    let [w0, w1] = std::mem::transmute::<Value, [u64; 2]>(value);
    (*(slot as *const AtomicU64)).store(w0, Ordering::Relaxed);
    (*((slot as *const u8).add(8) as *const AtomicU64)).store(w1, Ordering::Relaxed);
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
/// docs/known-issues/elasticsearch-lucene-binary-docvalues-range-hangs.md
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

    #[test]
    fn encode_decode_retaddr() {
        let (val, tag) = encode_value(Value::ReturnAddress(999));
        assert!(matches!(decode_value(val, tag), Value::ReturnAddress(999)));
    }

    #[test]
    fn decode_value_rejects_null_object() {
        // T14 graceful degradation: a VTAG_OBJECT tag paired with a null
        // pointer is treated as Value::Object(None) rather than panicking,
        // so corrupted or zero-initialized slots don't crash the VM.
        assert!(matches!(decode_value(0, VTAG_OBJECT), Value::Object(None)));
    }

    #[test]
    fn decode_value_rejects_unaligned_object() {
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
        assert!(matches!(decode_value(8, VTAG_OBJECT), Value::Object(None)));
        assert!(matches!(
            decode_value(0xff8, VTAG_OBJECT),
            Value::Object(None)
        ));
    }

    #[test]
    fn decode_value_rejects_out_of_range_object() {
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
        use crate::compact_value::{object_degradation_count, reset_object_degradation_count};
        reset_object_degradation_count();
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
            object_degradation_count() >= 4,
            "expected at least 4 degradations recorded, got {}",
            object_degradation_count()
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
}
