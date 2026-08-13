// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! GarbageCollector trait -- abstraction over heap implementations.
//!
//! Allows swapping between the simple semi-space `Heap` and the
//! generational `GenerationalHeap` without changing call sites.

use std::collections::HashMap;

use crate::gc::GcResult;
use crate::heap::{ArrayElementType, ObjectHeader, ObjectKind};
use cratonvm_types::{ClassId, ObjectRef, Value};

// ---------------------------------------------------------------------------
// Volatile field atomicity — striped locks
// ---------------------------------------------------------------------------
//
// JLS §17.7 requires reads and writes of `volatile long` / `volatile double`
// (and by extension all `volatile`-declared fields) to be atomic. The CratonVM
// heap stores fields as 16-byte `Value` (8-byte tag + 8-byte payload), which
// is wider than any stable Rust atomic primitive on x86-64 — a SeqCst fence
// pair gives the required happens-before ordering but does **not** guarantee
// atomicity of the 16-byte slot write itself. A concurrent volatile read
// could otherwise observe a torn (tag, payload) pair from two overlapping
// writes.
//
// We provide atomicity with a small striped-mutex pool (`parking_lot::Mutex`
// is uncontended-fast and avoids OS calls). The previous design used a single
// heap-wide mutex which serialized every volatile op across the whole heap
// — striping by `(obj_ref, index)` removes that bottleneck while preserving
// per-slot atomicity. `Mutex<()>` is the minimum primitive that gives mutual
// exclusion without storing the data inside the lock.
//
// Stripe count is a power of two so the index modulo is a single AND. 64 is
// a balance between false-sharing (a too-small pool serializes unrelated
// fields) and memory (64 × ~5 bytes is negligible).
const VOLATILE_STRIPE_COUNT: usize = 64;

static VOLATILE_STRIPES: std::sync::OnceLock<[parking_lot::Mutex<()>; VOLATILE_STRIPE_COUNT]> =
    std::sync::OnceLock::new();

#[inline]
fn volatile_stripes() -> &'static [parking_lot::Mutex<()>; VOLATILE_STRIPE_COUNT] {
    VOLATILE_STRIPES.get_or_init(|| std::array::from_fn(|_| parking_lot::Mutex::new(())))
}

/// Acquire the stripe lock guarding volatile reads/writes for the given
/// `(obj_ref, index)`. The hash spreads keys across [`VOLATILE_STRIPE_COUNT`]
/// stripes so unrelated volatile fields rarely contend. The caller must
/// hold the returned guard for the duration of the slot read or write.
#[inline]
pub fn volatile_stripe_lock(
    obj_ref: ObjectRef,
    index: usize,
) -> parking_lot::MutexGuard<'static, ()> {
    // Mix the object address (already 8-byte-aligned, so low 3 bits are 0)
    // with the slot index. A multiplicative mix gives even distribution
    // across stripes for both small object pools and large sequentially-
    // allocated heaps.
    let addr = obj_ref.as_ptr() as usize;
    let mixed = (addr.wrapping_shr(3))
        .wrapping_mul(0x9E37_79B9_7F4A_7C15)
        .wrapping_add(index);
    let stripe = mixed & (VOLATILE_STRIPE_COUNT - 1);
    volatile_stripes()[stripe].lock()
}

/// Resolves the identity hash of an object whose mark word is no longer
/// `NEUTRAL`, installing one if it has none yet.
///
/// An identity hash normally lives in the upper bits of a `MARK_NEUTRAL` mark
/// word. Inflation overwrites that word, so the hash is displaced into the
/// object's `Monitor` -- a `vm` type this crate cannot name, hence the hook.
/// The VM installs it once at start-up; until then, and in gc-only tests, the
/// displaced case is unreachable because nothing has inflated anything.
///
/// Takes the mark-word snapshot rather than the object address on purpose: the
/// snapshot already carries the `Monitor` pointer, so resolving is a pointer
/// dereference, not a lookup in an address-keyed table that would then need its
/// own GC re-keying.
static DISPLACED_HASH_RESOLVER: std::sync::OnceLock<fn(u64) -> i32> =
    std::sync::OnceLock::new();

/// Install the displaced-hash resolver. Idempotent; the first install wins.
pub fn set_displaced_hash_resolver(resolver: fn(u64) -> i32) {
    let _ = DISPLACED_HASH_RESOLVER.set(resolver);
}

/// Identity hash for an object whose mark word snapshot is `mark`.
///
/// Returns `0` when no resolver is installed. A caller must treat that as "no
/// answer" and NOT fall back to minting: minting on a non-`NEUTRAL` object
/// hands out a fresh value on every call, which is a *changing* identity hash --
/// strictly worse than a missing one, and invisible to any test that calls it
/// once.
pub fn displaced_identity_hash(mark: u64) -> i32 {
    match DISPLACED_HASH_RESOLVER.get() {
        Some(resolve) => resolve(mark),
        None => 0,
    }
}

/// Trait for monitor table cleanup after GC relocation.
///
/// The VM implements this for its `MonitorTable` so the gc crate does not
/// need to depend on VM-internal types.
pub trait MonitorCleanup {
    /// Re-key monitors using the old-address-to-new-address mapping.
    fn remap_after_gc(&self, pointer_map: &cratonvm_types::PointerMap);

    /// Prune registry entries keyed by addresses a WHOLE-HEAP collection
    /// just swept (`dead` is exact: every element was a live allocation
    /// base before this collection and its memory is now freed).
    ///
    /// Needed by non-moving whole-heap collectors (the ZGC backend), whose
    /// `pointer_map` is always empty: `remap_after_gc` early-returns on an
    /// empty map, so no collection ever pruned the monitor/cas-lock tables —
    /// an unbounded leak, and worse, a NEW object allocated at a recycled
    /// address silently inherited the dead object's monitor (a non-idle
    /// inherited monitor deadlocks the new object's first `synchronized`).
    ///
    /// Default no-op so moving collectors (whose remap path already handles
    /// reclamation) need no change.
    fn prune_dead(&self, _dead: &[usize]) {}
}

// ---------------------------------------------------------------------------
// Stop-The-World token — type-level proof of an STW pause
// ---------------------------------------------------------------------------
//
// The moving collectors (`Heap`, `GenerationalHeap`, `G1Collector`) require
// every mutator to be parked at a safepoint before `collect_garbage` rewrites
// object addresses. Historically that invariant was *enforced* purely by
// convention inside the `vm` crate's safepoint orchestration: the GC crate
// just exposed `&self` mutating entry points and trusted the caller.
//
// Pairing the invariant with a `unsafe impl Send + Sync` on the heap types
// (heap.rs:129, gen_heap.rs:225) meant any code with an `&Heap` could call
// `collect_garbage` from any thread without holding STW, which would have
// been instantly unsound — only luck (i.e. there is only one orchestrator)
// prevented a bug.
//
// `StopTheWorldToken` is a *type-level proof token*. Every mutating-by-
// `&self` collector entry point now requires `&StopTheWorldToken` as its
// first argument, so a thread that does not hold the token literally cannot
// call `collect_garbage` — the call won't compile. The orchestrator in
// `vm/src/runtime/interpreter.rs` constructs one token per STW round
// (after `gc_barrier.wait_for_all`) and threads it down to the heap.
//
// The token is intentionally `!Clone` and zero-sized; passing it by
// reference is the canonical pattern. Construction is unsafe because the GC
// crate cannot observe the VM's safepoint barrier directly: callers must mark
// the point where they have proved every mutator is stopped.
/// Type-level proof that the caller has stopped every mutator thread at a
/// safepoint and is therefore allowed to invoke a *moving* GC entry point.
///
/// `StopTheWorldToken` is zero-sized and has no public fields; constructing
/// one requires `unsafe` so safe public code cannot fabricate STW proof.
///
/// The token is `!Clone` and `!Copy`: orchestrator code holds a single
/// token for the duration of one STW round and passes it by reference to
/// each heap entry point that needs it. Dropping the token does not end
/// the STW pause (the pause is managed by the orchestrator's barrier) —
/// the token is just a static witness that the pause is in effect.
///
/// # Soundness
///
/// Construction asserts a runtime invariant that the compiler cannot
/// verify (every Java thread is parked at a safepoint). It is the
/// caller's job to uphold this. Building a token in non-STW code makes
/// every subsequent `collect_garbage` call a UAF hazard — the JIT and
/// interpreter may be reading object addresses that the GC then
/// rewrites.
#[must_use = "constructing a StopTheWorldToken without holding STW is a soundness bug"]
pub struct StopTheWorldToken {
    // Private unit field so external code cannot construct via field syntax.
    _priv: (),
}

impl StopTheWorldToken {
    /// Construct a new token after proving a real stop-the-world pause.
    ///
    /// # Safety
    ///
    /// The caller MUST have already parked every other mutator thread at a
    /// safepoint before calling this. The canonical orchestrator is
    /// `vm::runtime::interpreter`, which builds a token only after
    /// `gc_barrier.wait_for_all()` returns (every other thread is parked)
    /// or on the single-threaded fast path (no other mutator exists).
    ///
    /// Constructing a token in any other context is a soundness bug.
    #[inline]
    pub unsafe fn new() -> Self {
        Self { _priv: () }
    }

    /// Migration constructor — same as [`Self::new`] but the name signals
    /// that the callsite has not yet been audited for STW correctness.
    ///
    /// Existing pre-token code paths that already hold STW (e.g. test
    /// harnesses that run on a single thread with no JIT) may use this
    /// indefinitely.
    ///
    /// # Safety
    ///
    /// Same as [`Self::new`]: every mutator must be stopped, or there must be
    /// no other mutator thread.
    #[inline]
    pub unsafe fn new_unchecked() -> Self {
        Self { _priv: () }
    }
}

impl std::fmt::Debug for StopTheWorldToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("StopTheWorldToken")
    }
}

// ---------------------------------------------------------------------------
// Compile-fail demonstration of the token requirement
// ---------------------------------------------------------------------------
//
// The doc-tests below ensure (via `compile_fail`) that the token-signature
// change actually catches a missing-token caller at compile time. If a
// future refactor relaxes the signature, these tests will start to compile
// and the test suite will fail, alerting the maintainer.
//
// We deliberately reference `Heap::collect_garbage` here rather than the
// trait method, since the trait can in principle be reshaped while the
// inherent method is the load-bearing entry point.
/// Calling `collect_garbage` without a `StopTheWorldToken` must not compile.
///
/// ```compile_fail
/// use cratonvm_gc::Heap;
/// use cratonvm_gc::collector::MonitorCleanup;
/// use std::collections::HashMap;
///
/// struct NoMonitors;
/// impl MonitorCleanup for NoMonitors {
///     fn remap_after_gc(&self, _: &cratonvm_types::PointerMap) {}
/// }
///
/// let heap = Heap::new();
/// let mut roots = Vec::new();
/// // Missing &StopTheWorldToken — should fail to compile.
/// let _ = heap.collect_garbage(&mut roots, &NoMonitors);
/// ```
///
/// With the token threaded through, the call compiles:
///
/// ```
/// use cratonvm_gc::Heap;
/// use cratonvm_gc::collector::{MonitorCleanup, StopTheWorldToken};
/// use std::collections::HashMap;
///
/// struct NoMonitors;
/// impl MonitorCleanup for NoMonitors {
///     fn remap_after_gc(&self, _: &cratonvm_types::PointerMap) {}
/// }
///
/// let heap = Heap::new();
/// let mut roots = Vec::new();
/// // SAFETY: this doctest has no other mutator threads.
/// let stw = unsafe { StopTheWorldToken::new() };
/// let _ = heap.collect_garbage(&stw, &mut roots, &NoMonitors);
/// ```
///
/// Safe construction of STW proof must not compile:
///
/// ```compile_fail
/// use cratonvm_gc::collector::StopTheWorldToken;
///
/// let _stw = StopTheWorldToken::new();
/// ```
#[cfg(doctest)]
#[allow(dead_code)]
struct _StopTheWorldTokenCompileFailDocs;

/// I-17: G1's three mark-cycle entry points require the same witness.
///
/// `start_concurrent_mark`, `remark` and `cleanup` are all STW phases —
/// `cleanup` reclassifies and frees regions — and until 2026-08-13 they were
/// the only such entry points on the collector taking no [`StopTheWorldToken`],
/// i.e. the one invariant in the G1 audit's §7 table enforced by nothing but a
/// comment. These doctests keep it a signature: a future refactor that drops
/// the parameter to "simplify" the call sites turns the suite red instead of
/// silently un-enforcing the phase.
///
/// ```compile_fail
/// use cratonvm_gc::{G1Collector, G1CollectorConfig};
///
/// let gc = G1Collector::new(G1CollectorConfig::default());
/// // Missing &StopTheWorldToken — should fail to compile.
/// gc.cleanup();
/// ```
///
/// ```compile_fail
/// use cratonvm_gc::{G1Collector, G1CollectorConfig};
///
/// let gc = G1Collector::new(G1CollectorConfig::default());
/// // Missing &StopTheWorldToken — should fail to compile.
/// gc.start_concurrent_mark();
/// ```
///
/// ```compile_fail
/// use cratonvm_gc::{G1Collector, G1CollectorConfig};
///
/// let gc = G1Collector::new(G1CollectorConfig::default());
/// // Missing &StopTheWorldToken — should fail to compile.
/// gc.remark(&[]);
/// ```
///
/// With the token threaded through, the cycle compiles:
///
/// ```
/// use cratonvm_gc::{G1Collector, G1CollectorConfig, StopTheWorldToken};
///
/// let gc = G1Collector::new(G1CollectorConfig::default());
/// // SAFETY: this doctest has no other mutator threads.
/// let stw = unsafe { StopTheWorldToken::new() };
/// gc.start_concurrent_mark(&stw);
/// gc.remark(&stw, &[]);
/// gc.cleanup(&stw);
/// ```
#[cfg(doctest)]
#[allow(dead_code)]
struct _G1MarkCycleStwCompileFailDocs;

/// Trait abstracting a garbage-collected heap.
///
/// Both the simple semi-space `Heap` and the generational
/// `GenerationalHeap` implement this trait. The VM accesses the heap
/// exclusively through these methods.
pub trait GarbageCollector: Send + Sync {
    // -- Allocation --

    /// Allocate a new Java object with `num_fields` field slots, all zeroed.
    fn alloc_object(&self, class_id: ClassId, num_fields: usize) -> ObjectRef;

    /// Allocate a new Java array with the given element type and length.
    fn alloc_array(
        &self,
        class_id: ClassId,
        element_type: ArrayElementType,
        length: usize,
    ) -> ObjectRef;

    // -- Header access --

    /// Read the object header from a heap reference.
    fn get_header(&self, obj: ObjectRef) -> &ObjectHeader;

    /// Get the class id of a heap object.
    fn class_id_of(&self, obj: ObjectRef) -> ClassId;

    /// Get the kind (Object or Array) of a heap allocation.
    fn kind_of(&self, obj: ObjectRef) -> ObjectKind;

    /// Get the element type of an array object.
    fn element_type_of(&self, obj: ObjectRef) -> ArrayElementType;

    /// Get the identity hash code of a heap object.
    fn identity_hash_code(&self, obj: ObjectRef) -> i32;

    // -- Field access --

    /// Get the value of a field at the given index.
    fn get_field(&self, obj: ObjectRef, index: usize) -> Value;

    /// Set the value of a field at the given index.
    fn set_field(&self, obj: ObjectRef, index: usize, value: Value);

    /// Get the value of a volatile field at the given index.
    fn get_field_volatile(&self, obj: ObjectRef, index: usize) -> Value;

    /// Set the value of a volatile field at the given index.
    fn set_field_volatile(&self, obj: ObjectRef, index: usize, value: Value);

    /// T10.9.E — Descriptor-aware field read.
    ///
    /// Reads the slot via [`get_field`](Self::get_field) and then normalizes
    /// the returned `Value` to match the declared field type encoded as a
    /// JVM descriptor byte (`b'J'`, `b'D'`, `b'L'`, etc.). This guarantees
    /// that a long-typed field never surfaces as `Value::Double` even if an
    /// upstream putfield corrupted the slot tag via a CompactValue round
    /// trip.
    ///
    /// Default implementation: raw read + coercion. Collectors may override
    /// for a fused fast path; the default is always correct.
    fn get_field_as(&self, obj: ObjectRef, index: usize, desc_byte: u8) -> Value {
        let raw = self.get_field(obj, index);
        crate::heap::coerce_field_value_by_descriptor(raw, desc_byte)
    }

    /// Volatile descriptor-aware read.
    fn get_field_volatile_as(&self, obj: ObjectRef, index: usize, desc_byte: u8) -> Value {
        let raw = self.get_field_volatile(obj, index);
        crate::heap::coerce_field_value_by_descriptor(raw, desc_byte)
    }

    /// Descriptor-aware write — normalizes the stored `Value` to match the
    /// declared field type before the underlying slot write.
    fn set_field_as(&self, obj: ObjectRef, index: usize, value: Value, desc_byte: u8) {
        let coerced = crate::heap::coerce_field_value_by_descriptor(value, desc_byte);
        self.set_field(obj, index, coerced);
    }

    /// Volatile descriptor-aware write.
    fn set_field_volatile_as(&self, obj: ObjectRef, index: usize, value: Value, desc_byte: u8) {
        let coerced = crate::heap::coerce_field_value_by_descriptor(value, desc_byte);
        self.set_field_volatile(obj, index, coerced);
    }

    // -- Array access --

    /// Get the length of an array.
    fn array_length(&self, obj: ObjectRef) -> usize;

    /// Get an array element at the given index.
    fn get_array_element(&self, obj: ObjectRef, index: usize) -> Result<Value, i32>;

    /// Set an array element at the given index.
    fn set_array_element(&self, obj: ObjectRef, index: usize, value: Value) -> Result<(), i32>;

    // -- GC operations --

    /// Returns true when the heap should be garbage collected.
    fn needs_gc(&self) -> bool;

    /// Run a garbage collection cycle.
    ///
    /// The `_stw` parameter is type-level proof that the caller has
    /// stopped every mutator thread at a safepoint — see
    /// [`StopTheWorldToken`]. Implementations may treat the token as a
    /// no-op witness; its sole purpose is to prevent non-STW code from
    /// calling this method.
    fn collect_garbage(
        &self,
        _stw: &StopTheWorldToken,
        roots: &mut [ObjectRef],
        monitors: &dyn MonitorCleanup,
    ) -> GcResult;

    /// Write barrier -- called **after** every reference store into a heap
    /// object.
    ///
    /// For the simple heap this is a no-op. The generational heap marks
    /// the card table entry dirty when an old-gen object stores a reference
    /// to a young-gen object. G1 records the cross-region edge in the
    /// destination region's remembered set.
    ///
    /// # SATB pre-barrier precondition
    ///
    /// This trait method is a **post-store** hook: the new reference value
    /// is already in the slot by the time it fires. Concurrent collectors
    /// that use Snapshot-At-The-Beginning (SATB) marking (currently G1) also
    /// require the *old* slot value to be logged **before** the store —
    /// otherwise objects reachable only through the overwritten reference
    /// at the start of the marking cycle can be lost (lost-object bug,
    /// downstream UAF on the next collection).
    ///
    /// When the underlying collector advertises `is_marking_active()` (see
    /// `VmHeap::g1_is_marking_active`), callers MUST invoke
    /// `VmHeap::satb_barrier(old_value)` **before** the store whose
    /// completion is being signalled here. The trait shape cannot deliver
    /// the old value after the fact, so this two-stage protocol is a caller
    /// contract — there is no way for the GC alone to recover a missed
    /// pre-barrier. G1 ships a best-effort `debug_assert!` that fires when
    /// `write_barrier` is invoked while marking is active without any
    /// matching pre-call on the same thread; release builds skip the check.
    ///
    /// # Publication ordering
    ///
    /// A reference update is published as one ordered triad:
    ///
    /// `SATB(old) -> slot.store(new) -> remembered_set/card.release(new)`
    ///
    /// No safepoint or other call may occur between the slot store and its
    /// post barrier. The collector consumes remembered-set/card state with
    /// acquire ordering after the stop-the-world handshake, so observing a
    /// dirty card also observes the reference value stored before it. JIT
    /// inline barriers and helper-backed barriers implement the same order.
    fn write_barrier(&self, obj: ObjectRef, stored_value: Value);

    /// SATB **pre**-store barrier — called BEFORE a reference-typed slot
    /// is overwritten, with the value about to be lost. Pairs with
    /// [`Self::write_barrier`] (post-store) to form the
    /// pre-barrier / store / post-barrier triad required for concurrent
    /// marking correctness (snapshot-at-the-beginning).
    ///
    /// Task #25 (HIGH soundness): previously this was caller discipline
    /// — interpreter/JIT call sites manually invoked the heap-specific
    /// `satb_barrier(old_value)` and the `GarbageCollector` trait knew
    /// nothing about pre-stores. A `debug_assert!` could in principle
    /// have verified the discipline but release builds compiled it out.
    /// Promoting the pre-barrier to a trait method makes the contract
    /// part of the type system: every collector now opts in or out
    /// explicitly, and a missing call site fails to compile rather than
    /// silently dropping SATB log entries.
    ///
    /// ## Cost
    ///
    /// The default implementation is `#[inline]` empty and contains no
    /// branches — non-SATB collectors (semi-space `Heap`, the
    /// `GenerationalHeap` minor-GC path when concurrent mark is idle)
    /// pay literally zero instructions at the trait dispatch site
    /// because LLVM elides the dispatched call. G1 overrides this and
    /// routes `old` into the per-thread SATB buffer when
    /// [`crate::satb::SatbQueue::is_active`] is true; the cost there is
    /// one Acquire load (gated check) on the inactive path and one
    /// thread-local push on the active path.
    ///
    /// ## Parameters
    ///
    /// * `slot` — pointer to the heap slot about to be overwritten.
    ///   The pointer is used only for ordering assertions in debug
    ///   builds (see `set_field`'s triad assertion); it is NOT
    ///   dereferenced by the default or G1 implementation.
    /// * `old` — the previous reference value of the slot, read before
    ///   the store. Concurrent marking treats `old` as a root for the
    ///   rest of this mark cycle.
    ///
    /// ## SAFETY
    ///
    /// `slot` may be passed as `std::ptr::null_mut()` when the caller
    /// only has the value and not the slot address (e.g. monitor exit
    /// paths) — implementations MUST NOT dereference it.
    #[inline]
    fn write_barrier_pre(&self, _slot: *mut ObjectRef, _old: ObjectRef) {
        // Zero-cost default: empty body, leading-underscore parameter
        // names so LLVM has no live values to materialize. Non-SATB
        // collectors keep this implementation and pay literally nothing
        // (no register save, no branch, no memory write) per ref-store.
    }

    /// Total bytes currently allocated.
    fn allocated_bytes(&self) -> usize;
}
