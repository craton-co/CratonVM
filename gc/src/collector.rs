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
// a balance between stripe COLLISION (a too-small pool serializes unrelated
// fields) and memory.
const VOLATILE_STRIPE_COUNT: usize = 64;

/// One stripe, on a cache line of its own.
///
/// # Why the padding is load-bearing
///
/// `parking_lot::Mutex<()>` is ONE BYTE — its `RawMutex` is an `AtomicU8` and
/// the `()` payload is zero-sized. So `[parking_lot::Mutex<()>; 64]`, which is
/// what this pool used to be, occupied exactly 64 bytes: **one cache line, for
/// every stripe of every object of every field**. Striping by
/// `(obj_ref, index)` removed the LOGICAL contention (two threads rarely pick
/// the same stripe) and left the HARDWARE contention completely untouched: a
/// `lock()`/`unlock()` pair is two atomic read-modify-writes, each of which
/// takes that single line exclusive, so N threads storing to N different
/// volatile fields of N different objects still serialised on the coherence
/// protocol exactly as if the pool had one stripe.
///
/// That is what `HibfixVarHandleScale` measured, and it is why removing the
/// `vh_meta_table` convoy in 2026-08-24 made the curve FLAT rather than
/// scaling: with each thread owning its own object and its own field, there
/// was no contention left on the data, no contention left on the metadata, and
/// throughput still did not rise past 4 threads. The comment that used to sit
/// here reasoned about "false-sharing" meaning stripe collision, and put the
/// pool's whole size at "64 × ~5 bytes is negligible" — which is precisely
/// the property that made every stripe share one line.
///
/// 128 rather than 64 because x86-64's adjacent-cache-line prefetcher pulls
/// lines in pairs, so a 64-byte stride still lets two stripes travel together.
/// The whole pool is 8 KiB.
///
/// See `performance/varhandle-writes-and-cas-have-no-fast-path-FIXED-20260827.md`.
#[repr(align(128))]
struct VolatileStripe(parking_lot::Mutex<()>);

static VOLATILE_STRIPES: std::sync::OnceLock<[VolatileStripe; VOLATILE_STRIPE_COUNT]> =
    std::sync::OnceLock::new();

#[inline]
fn volatile_stripes() -> &'static [VolatileStripe; VOLATILE_STRIPE_COUNT] {
    VOLATILE_STRIPES
        .get_or_init(|| std::array::from_fn(|_| VolatileStripe(parking_lot::Mutex::new(()))))
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
    volatile_stripes()[stripe].0.lock()
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
static DISPLACED_HASH_RESOLVER: std::sync::OnceLock<fn(u64) -> i32> = std::sync::OnceLock::new();

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

/// Resolve a `ClassId` to its binary name, for **diagnostics only**.
///
/// Same shape and the same reason as [`DISPLACED_HASH_RESOLVER`] above: class
/// names live in the VM's class manager, a type this crate cannot name. The VM
/// installs the hook once at start-up.
///
/// Every consumer is a failure-path or debug-flag report. A GC diagnostic that
/// can only say `class_id=418` makes the reader do a second run with a
/// different flag to learn what 418 is — and on the report that matters most
/// here (the objects walling a fragmented heap) that second run is a different
/// process with a different heap layout, so the answer does not carry over.
static CLASS_NAMER: std::sync::OnceLock<fn(u32) -> Option<String>> = std::sync::OnceLock::new();

/// Install the diagnostic class-name resolver. Idempotent; first install wins.
pub fn set_class_namer(namer: fn(u32) -> Option<String>) {
    let _ = CLASS_NAMER.set(namer);
}

/// Binary name of `class_id`, or `class_id=<n>` when no resolver is installed
/// (gc-only tests, or a report raised before start-up finished).
pub fn class_name_for_diagnostics(class_id: u32) -> String {
    match CLASS_NAMER.get().and_then(|f| f(class_id)) {
        Some(name) => name,
        None => format!("class_id={class_id}"),
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

    /// Does this cleanup actually need the dead-address slice
    /// [`Self::prune_dead`] takes?
    ///
    /// # Why the collector has to ask
    ///
    /// Building that slice is not free on the collector's side: it is one
    /// `usize` per object the sweep reclaimed, pushed inside the
    /// stop-the-world pause. On the whole-heap cycle `gc::zgc`'s own pause
    /// anatomy is written against — 13.0M dead objects — that is a **104 MB
    /// allocation inside the pause**, plus a second pass over 104 MB of cold
    /// memory to consume it. It is the same defect that was found and fixed
    /// for `ZObjectStartsSnapshot::bases()` (87 MB, measured at 13% of the
    /// pause); the fix never reached the sweep's `dead` vector.
    ///
    /// And the VM's `MonitorTable::prune_dead` says in its own comment that
    /// the slice is usually consumed to remove **nothing**: "the number of
    /// INFLATED monitors is usually zero and never more than a handful --
    /// inflation needs real contention", measured at 5.36% of
    /// `LegendreHighPrecisionTest` and 4.98% of `PSquarePercentileTest`, "two
    /// workloads with no contended monitor in them at all". Its shard survey
    /// already turns that into 128 uncontended lock pairs — but only *after*
    /// the collector has paid to build the slice. This asks the same question
    /// one step earlier, where the cost actually is.
    ///
    /// # Why the default is `true`
    ///
    /// Fail-safe. An implementation that overrides `prune_dead` and forgets
    /// this method still receives the complete slice. The failure mode of a
    /// `false` default would be a silently empty prune — a new object
    /// inheriting a dead one's monitor, which is precisely what `prune_dead`
    /// exists to prevent, and it would present as a deadlock rather than as a
    /// missing optimisation.
    ///
    /// # Contract
    ///
    /// Answered once per collection, at a safepoint, before the sweep runs. An
    /// implementation may answer `false` only if `prune_dead` would do nothing
    /// for *any* input — which under a stop-the-world token means "my tables
    /// are empty, and no mutator can fill them before the prune".
    fn wants_dead_addresses(&self) -> bool {
        true
    }
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
    ///
    /// # G45: these four defaults ARE the live coercion path
    ///
    /// None of the three shipped collectors overrides any of the four
    /// `*_as` methods — `GenerationalHeap` (`gen_heap.rs:17200`),
    /// `G1Collector` (`g1.rs:9692`) and `ZgcRealHeap` (`zgc.rs:8502`) all
    /// inherit these bodies verbatim, and `VmHeap`'s enum `dispatch!`
    /// (`vm_heap.rs:222`) is what routes every descriptor-aware field access
    /// in the VM into them. So until 2026-08-17 EVERY coercion event the
    /// G30 instrument reported from a real run said `class_id=-1 index=-1`:
    /// these four call sites were the only ones on the path and none of
    /// them passed provenance. MEASURED before the change, one `RCrypto`
    /// run under `CRATONVM_DBG_COERCION=1`: 33 events, 33 of them
    /// `access="unattributed"`.
    ///
    /// [`crate::heap::coerce_field_value_for_slot`] already existed and is
    /// already the body of `coerce_field_value_by_descriptor` — the latter
    /// is literally the former with
    /// [`FieldCoercionSite::UNATTRIBUTED`](crate::heap::FieldCoercionSite::UNATTRIBUTED).
    /// So no signature anywhere had to change; the fix is the site argument.
    ///
    /// **Hot path.** `site` is a three-word `Copy` struct that
    /// `coerce_field_value_for_slot` reads in exactly one place: as an
    /// argument to the `#[cold]` `note_field_coercion_loss`. The returned
    /// `Value` is bit-identical on every arm, lossy and non-lossy alike.
    /// The only added work on the non-lossy path is
    /// [`class_id_of`](Self::class_id_of), which on all three collectors is
    /// a single load out of the object header — the same header the
    /// adjacent `get_field`/`set_field` dereferences in the very same call,
    /// so it is an L1 hit on a line already resident. No branch, no atomic,
    /// no allocation.
    fn get_field_as(&self, obj: ObjectRef, index: usize, desc_byte: u8) -> Value {
        let raw = self.get_field(obj, index);
        crate::heap::coerce_field_value_for_slot(
            raw,
            desc_byte,
            crate::heap::FieldCoercionSite::read(Some(self.class_id_of(obj)), index),
        )
    }

    /// Volatile descriptor-aware read. Provenance as in
    /// [`get_field_as`](Self::get_field_as).
    fn get_field_volatile_as(&self, obj: ObjectRef, index: usize, desc_byte: u8) -> Value {
        let raw = self.get_field_volatile(obj, index);
        crate::heap::coerce_field_value_for_slot(
            raw,
            desc_byte,
            crate::heap::FieldCoercionSite::read(Some(self.class_id_of(obj)), index),
        )
    }

    /// Descriptor-aware write — normalizes the stored `Value` to match the
    /// declared field type before the underlying slot write.
    ///
    /// The STORE half is the one that matters: a read that coerces is
    /// usually the slot repairing a never-initialised tag, a store that
    /// coerces has destroyed something a writer meant. Reporting them in
    /// one bucket is how the signal was lost, so
    /// [`FieldAccessKind`](crate::heap::FieldAccessKind) separates them and
    /// this is the arm that says `store`.
    fn set_field_as(&self, obj: ObjectRef, index: usize, value: Value, desc_byte: u8) {
        let coerced = crate::heap::coerce_field_value_for_slot(
            value,
            desc_byte,
            crate::heap::FieldCoercionSite::store(Some(self.class_id_of(obj)), index),
        );
        self.set_field(obj, index, coerced);
    }

    /// Volatile descriptor-aware write. Provenance as in
    /// [`set_field_as`](Self::set_field_as).
    fn set_field_volatile_as(&self, obj: ObjectRef, index: usize, value: Value, desc_byte: u8) {
        let coerced = crate::heap::coerce_field_value_for_slot(
            value,
            desc_byte,
            crate::heap::FieldCoercionSite::store(Some(self.class_id_of(obj)), index),
        );
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

#[cfg(test)]
mod coercion_provenance_tests {
    use super::*;
    use std::sync::Mutex;

    /// The class the stub claims every object belongs to. Distinctive on
    /// purpose: `-1` (the old report) and `0` (a plausible default) are both
    /// wrong answers this must not be confused with.
    const STUB_CLASS: u32 = 4242;

    /// A collector that implements only the raw half of field access and
    /// records what the four `*_as` defaults did on top of it.
    ///
    /// Modelled on `g1.rs`'s `StubCollector`: everything the tests do not
    /// call is `unreachable!()`, so a default body that silently starts
    /// depending on a new method fails loudly instead of quietly.
    ///
    /// The recording is the whole point. `FieldCoercionSite` is consumed by
    /// the `#[cold]` reporter and never returned, and the loss counters are
    /// process-global — `heap.rs`'s own G30 tests assert EXACT deltas on
    /// them under a module-private lock this module cannot take, so a test
    /// here that fired a lossy coercion would make those exact deltas flaky
    /// from another module. Instead these tests observe the one part of
    /// provenance that is locally visible and equally load-bearing: whether
    /// the default asked `class_id_of` at all. It cannot have built a real
    /// site without asking, and `coerce_field_value_by_descriptor` — the
    /// call this change replaced — never asks.
    struct RecordingCollector {
        header: ObjectHeader,
        // `GarbageCollector` is `Sync`, so a `RefCell` recorder does not satisfy the
        // bound. `Mutex` is the smallest change that keeps this a plain struct.
        class_id_calls: Mutex<usize>,
        stored: Mutex<Vec<(usize, Value)>>,
        slot: Mutex<Value>,
    }

    impl RecordingCollector {
        fn new(slot: Value) -> Self {
            Self {
                header: ObjectHeader::new(
                    ClassId::new(STUB_CLASS),
                    ObjectKind::Object,
                    ArrayElementType::Reference,
                    0,
                    2,
                ),
                class_id_calls: Mutex::new(0),
                stored: Mutex::new(Vec::new()),
                slot: Mutex::new(slot),
            }
        }

        /// A pointer-shaped, 8-aligned `ObjectRef` the stub never
        /// dereferences — every accessor it reaches answers from the
        /// struct above.
        fn obj(&self) -> ObjectRef {
            // SAFETY: `from_raw` only requires a non-null, 8-aligned
            // pointer. `&self.header` is both, and nothing in this module
            // reads through the resulting `ObjectRef`.
            unsafe { ObjectRef::from_raw(&self.header as *const ObjectHeader as *mut u8) }
        }
    }

    impl GarbageCollector for RecordingCollector {
        fn alloc_object(&self, _: ClassId, _: usize) -> ObjectRef {
            unreachable!("not called by these tests")
        }
        fn alloc_array(&self, _: ClassId, _: ArrayElementType, _: usize) -> ObjectRef {
            unreachable!("not called by these tests")
        }
        fn get_header(&self, _: ObjectRef) -> &ObjectHeader {
            &self.header
        }
        fn class_id_of(&self, _: ObjectRef) -> ClassId {
            *self.class_id_calls.lock().unwrap() += 1;
            ClassId::new(STUB_CLASS)
        }
        fn kind_of(&self, _: ObjectRef) -> ObjectKind {
            ObjectKind::Object
        }
        fn element_type_of(&self, _: ObjectRef) -> ArrayElementType {
            ArrayElementType::Reference
        }
        fn identity_hash_code(&self, _: ObjectRef) -> i32 {
            unreachable!("not called by these tests")
        }
        fn get_field(&self, _: ObjectRef, _: usize) -> Value {
            *self.slot.lock().unwrap()
        }
        fn set_field(&self, _: ObjectRef, index: usize, value: Value) {
            self.stored.lock().unwrap().push((index, value));
        }
        fn get_field_volatile(&self, _: ObjectRef, _: usize) -> Value {
            *self.slot.lock().unwrap()
        }
        fn set_field_volatile(&self, _: ObjectRef, index: usize, value: Value) {
            self.stored.lock().unwrap().push((index, value));
        }
        fn array_length(&self, _: ObjectRef) -> usize {
            unreachable!("not called by these tests")
        }
        fn get_array_element(&self, _: ObjectRef, _: usize) -> Result<Value, i32> {
            unreachable!("not called by these tests")
        }
        fn set_array_element(&self, _: ObjectRef, _: usize, _: Value) -> Result<(), i32> {
            unreachable!("not called by these tests")
        }
        fn needs_gc(&self) -> bool {
            false
        }
        fn collect_garbage(
            &self,
            _: &StopTheWorldToken,
            _: &mut [ObjectRef],
            _: &dyn MonitorCleanup,
        ) -> GcResult {
            unreachable!("not called by these tests")
        }
        fn write_barrier(&self, _: ObjectRef, _: Value) {}
        fn allocated_bytes(&self) -> usize {
            0
        }
    }

    /// G45, THE PIN. Each of the four descriptor-aware defaults must ask the
    /// collector who the object is, exactly once per access.
    ///
    /// This is the whole defect in one assertion. Every one of the 5,431
    /// coercion events measured across the vector corpus printed
    /// `class_id=-1 index=-1`, because these four bodies — which
    /// `GenerationalHeap`, `G1Collector` and `ZgcRealHeap` all inherit
    /// unchanged, and which `VmHeap`'s `dispatch!` routes the whole VM
    /// through — called `coerce_field_value_by_descriptor`, whose site is
    /// the constant `FieldCoercionSite::UNATTRIBUTED`. Reverting any of the
    /// four to that call leaves its counter at 0 here.
    ///
    /// "Exactly once", not "at least once": twice would mean the header is
    /// being re-read per access for no reason, on the allocation and
    /// collection hot path.
    #[test]
    fn the_four_descriptor_aware_defaults_ask_who_the_object_is() {
        for (name, run) in [
            (
                "get_field_as",
                Box::new(|c: &RecordingCollector| {
                    c.get_field_as(c.obj(), 1, b'I');
                }) as Box<dyn Fn(&RecordingCollector)>,
            ),
            (
                "get_field_volatile_as",
                Box::new(|c: &RecordingCollector| {
                    c.get_field_volatile_as(c.obj(), 1, b'I');
                }),
            ),
            (
                "set_field_as",
                Box::new(|c: &RecordingCollector| {
                    c.set_field_as(c.obj(), 1, Value::Int(5), b'I');
                }),
            ),
            (
                "set_field_volatile_as",
                Box::new(|c: &RecordingCollector| {
                    c.set_field_volatile_as(c.obj(), 1, Value::Int(5), b'I');
                }),
            ),
        ] {
            let c = RecordingCollector::new(Value::Int(5));
            run(&c);
            assert_eq!(
                *c.class_id_calls.lock().unwrap(),
                1,
                "{name} must resolve the class exactly once so the coercion \
                 instrument can name it (0 = reverted to \
                 coerce_field_value_by_descriptor and every event goes back \
                 to class_id=-1)",
            );
        }
    }

    /// G45: and the value the slot sees is unchanged.
    ///
    /// This is the risk half. These four bodies are the VM's entire
    /// descriptor-aware field path and 97 of 99 `--jdk-only` vectors pass
    /// over them, so the acceptable behavioural delta is zero.
    /// `coerce_field_value_for_slot` reads `site` in exactly one place — as
    /// an argument to the `#[cold]` `note_field_coercion_loss` — so this
    /// holds by construction; the table pins it anyway.
    ///
    /// DELIBERATELY only non-reporting arms. Every lossy input would
    /// increment the process-global counters that `heap.rs`'s G30 tests
    /// assert exact deltas on, from a module that cannot take their lock.
    /// The normalising arms are the ones this change could plausibly have
    /// disturbed; the lossy arms are covered where the lock lives.
    #[test]
    fn provenance_did_not_change_what_the_slot_receives() {
        // (value, descriptor) — normalisation and identity only, no arm
        // here calls `note_field_coercion_loss`.
        let cases: &[(Value, u8)] = &[
            (Value::Int(-7), b'J'),
            (Value::Double(f64::from_bits(0x0102_0304_0506_0708)), b'J'),
            (Value::Float(1.5), b'J'),
            (Value::Long(0x0102_0304_0506_0708), b'D'),
            (Value::Int(3), b'F'),
            (Value::Long(0x1_0000_0001), b'I'),
            (Value::Double(2.5), b'S'),
            (Value::Uninitialized, b'J'),
            (Value::Uninitialized, b'D'),
            (Value::Uninitialized, b'F'),
            (Value::Uninitialized, b'I'),
            (Value::Object(None), b'L'),
            (Value::Object(None), b'['),
            // Unknown descriptor: the `_ => value` arm, untouched.
            (Value::Int(99), b'V'),
        ];
        for &(value, desc) in cases {
            let expected = crate::heap::coerce_field_value_by_descriptor(value, desc);

            let store = RecordingCollector::new(Value::Uninitialized);
            store.set_field_as(store.obj(), 1, value, desc);
            store.set_field_volatile_as(store.obj(), 0, value, desc);
            assert_eq!(
                *store.stored.lock().unwrap().clone(),
                vec![(1, expected), (0, expected)],
                "storing {value:?} at a '{}' slot must land exactly what the \
                 descriptor-only helper lands",
                desc as char,
            );

            let load = RecordingCollector::new(value);
            assert_eq!(
                load.get_field_as(load.obj(), 1, desc),
                expected,
                "reading {value:?} from a '{}' slot",
                desc as char,
            );
            assert_eq!(
                load.get_field_volatile_as(load.obj(), 1, desc),
                expected,
                "volatile-reading {value:?} from a '{}' slot",
                desc as char,
            );
        }
    }

    /// G45: the reads say `read` and the writes say `store`.
    ///
    /// The direction is not decoration. MEASURED on the pre-G30 binary, 336
    /// of the 352 cross-type accesses in one `RJdkNet` run are READS of a
    /// slot that was never descriptor-initialised, and answering `null`
    /// there is correct. Filing those under the same heading as a store is
    /// how the handful of real stores became invisible, so the site each
    /// default builds is asserted directly rather than through the counters.
    #[test]
    fn the_reads_are_reads_and_the_writes_are_stores() {
        let cid = Some(ClassId::new(STUB_CLASS));
        let r = crate::heap::FieldCoercionSite::read(cid, 3);
        let s = crate::heap::FieldCoercionSite::store(cid, 3);
        assert_eq!(r.kind.name(), "read");
        assert_eq!(s.kind.name(), "store");
        assert_eq!(r.class_id.map(|c| c.as_u32()), Some(STUB_CLASS));
        assert_eq!(s.index, Some(3));
        // And the thing that was there before names neither.
        let u = crate::heap::FieldCoercionSite::UNATTRIBUTED;
        assert_eq!(u.kind.name(), "unattributed");
        assert!(u.class_id.is_none() && u.index.is_none());
    }

    /// The volatile stripe pool must occupy one cache line PER STRIPE, not one
    /// cache line in TOTAL.
    ///
    /// This asserts both halves, because either alone is satisfiable by the
    /// bug: the second assertion is the fix, and the first is the reason the
    /// fix was needed and the thing that would make a future "simplification"
    /// back to a bare `[Mutex<()>; N]` look harmless. A `parking_lot::Mutex<()>`
    /// is one byte, so an unpadded 64-entry pool is 64 bytes — every stripe on
    /// one line, and every `lock()` an atomic RMW that takes that line
    /// exclusive from every other thread.
    #[test]
    fn volatile_stripes_do_not_share_a_cache_line() {
        assert!(
            std::mem::size_of::<parking_lot::Mutex<()>>() <= 8,
            "a bare `Mutex<()>` is {} bytes — that is WHY the pool is padded",
            std::mem::size_of::<parking_lot::Mutex<()>>(),
        );
        assert_eq!(
            std::mem::align_of::<VolatileStripe>(),
            128,
            "each volatile stripe must start on its own 128-byte boundary",
        );
        assert_eq!(
            std::mem::size_of::<VolatileStripe>(),
            128,
            "each volatile stripe must OCCUPY 128 bytes, not merely start on a 128-byte boundary",
        );
        // And the pool as a whole: 64 stripes that genuinely do not overlap.
        assert_eq!(
            std::mem::size_of::<[VolatileStripe; VOLATILE_STRIPE_COUNT]>(),
            128 * VOLATILE_STRIPE_COUNT,
        );
    }
}
