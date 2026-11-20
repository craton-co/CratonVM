// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! GarbageCollector trait -- abstraction over heap implementations.
//!
//! Implemented by the three shipped collectors (`GenerationalHeap`,
//! `G1Collector`, `ZgcRealHeap`), which `VmHeap` dispatches over. The legacy
//! semi-space `Heap` has inherent methods only and does NOT implement it
//! (gcj j1/cm corrected this line).

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
/// See `varhandle-writes-and-cas-have-no-fast-path-FIXED-20260827.md`.
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
    volatile_stripes()[volatile_stripe_index(obj_ref.as_ptr() as usize, index)]
        .0
        .lock()
}

/// Which stripe guards `(addr, index)`.
///
/// Fibonacci hashing: multiply by the golden-ratio constant and take the
/// TOP bits. gc-common 2026-09-23 (lane A): this used to take the BOTTOM six
/// bits of the product (`mixed & 63`). The low `k` bits of a product depend
/// only on the low `k` bits of its operands, so that "mix" was a bijection of
/// address bits 3..8 and nothing else — the multiplication bought no spread at
/// all. Objects are allocated at a fixed stride, so a stride of `2^s * 8`
/// bytes reaches only `64 >> s` stripes for any one field index: 16 of 64 for
/// the common 32-byte object, 8 for 64-byte objects, one for any object 512
/// bytes or larger. The high bits of the product depend on EVERY address bit.
///
/// Any deterministic mapping is correct (a slot must simply always map to the
/// same stripe, and every caller goes through this function); only the
/// collision rate changes. The index is still added after hashing so the
/// fields of one object land on consecutive stripes.
#[inline]
fn volatile_stripe_index(addr: usize, index: usize) -> usize {
    const SHIFT: u32 = u64::BITS - VOLATILE_STRIPE_COUNT.trailing_zeros();
    let hashed = ((addr as u64 >> 3).wrapping_mul(0x9E37_79B9_7F4A_7C15) >> SHIFT) as usize;
    hashed.wrapping_add(index) & (VOLATILE_STRIPE_COUNT - 1)
}

/// Resolves the identity hash of a compact instance whose mark word is no
/// longer `NEUTRAL` (thin-locked or inflated), installing one if it has none.
///
/// A compact instance's identity hash lives in a `MARK_NEUTRAL` mark word. A
/// lock overwrites that word, so the hash lives in the object's `Monitor`
/// instead -- a `vm` type this crate cannot name, hence the hook. The VM
/// installs it once at start-up; until then, and in gc-only tests, the case is
/// unreachable because nothing has locked anything.
///
/// Takes the object ADDRESS: an inflated mark word no longer carries a
/// `Monitor` pointer (the 8-byte header has no room), and the VM's monitor
/// index is keyed by address and remapped by every moving collector.
static DISPLACED_HASH_RESOLVER: std::sync::OnceLock<fn(usize) -> i32> = std::sync::OnceLock::new();

/// Install the displaced-hash resolver. Idempotent; the first install wins.
pub fn set_displaced_hash_resolver(resolver: fn(usize) -> i32) {
    let _ = DISPLACED_HASH_RESOLVER.set(resolver);
}

/// Identity hash for the non-`NEUTRAL` compact instance at `obj_addr`.
///
/// Returns `0` when no resolver is installed. A caller must treat that as "no
/// answer" and NOT fall back to minting: minting on a non-`NEUTRAL` object
/// hands out a fresh value on every call, which is a *changing* identity hash --
/// strictly worse than a missing one, and invisible to any test that calls it
/// once.
pub fn displaced_identity_hash(obj_addr: usize) -> i32 {
    match DISPLACED_HASH_RESOLVER.get() {
        Some(resolve) => resolve(obj_addr),
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

/// The owner-scoped twin of [`CLASS_NAMER`]: `(layout domain, class id) ->
/// name`. A `ClassId` is per class store, so with two VMs in the process the
/// domain-less namer answers from whichever VM has the id first, which may
/// name another VM's class. A report raised by a heap that knows its layout
/// domain (the VM's owner token, interpreter round i1 wave 23, lane L5; the
/// i18-L5 owner-token proposal's step 4) names the class in that heap's VM.
static CLASS_NAMER_IN_DOMAIN: std::sync::OnceLock<fn(u32, u32) -> Option<String>> =
    std::sync::OnceLock::new();

/// Install the owner-scoped diagnostic class-name resolver. Idempotent; first
/// install wins.
pub fn set_class_namer_in_domain(namer: fn(u32, u32) -> Option<String>) {
    let _ = CLASS_NAMER_IN_DOMAIN.set(namer);
}

/// [`class_name_for_diagnostics`] for a report whose heap knows its layout
/// domain: the name of `class_id` in the VM that owns `layout_domain`. Without
/// an owner-scoped resolver it is [`class_name_for_diagnostics`].
pub fn class_name_for_diagnostics_in(layout_domain: u32, class_id: u32) -> String {
    let Some(namer) = CLASS_NAMER_IN_DOMAIN.get() else {
        return class_name_for_diagnostics(class_id);
    };
    match namer(layout_domain, class_id) {
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

    /// The addresses [`Self::prune_dead`] could act on -- every key of every
    /// table it prunes -- so the collector can intersect them with its dead
    /// set itself instead of listing every dead base (zr z5/sw).
    ///
    /// `None` (the default) means "not supported: hand me the full slice",
    /// and the collector does exactly what it did before. `Some(keys)` is a
    /// promise that `prune_dead(keys ∩ dead)` has the same effect as
    /// `prune_dead(dead)`, i.e. that the prune only ever acts on slice
    /// elements that are keys. Asked once per collection under the
    /// stop-the-world token, so the keys cannot change before the prune.
    /// Only the ZGC backend asks, under `CRATONVM_ZGC_SWEEP_LIVE_PROPORTIONAL`.
    fn prune_candidate_keys(&self) -> Option<Vec<usize>> {
        None
    }

    /// [`Self::prune_dead`] with a slice the collector already filtered to
    /// [`Self::prune_candidate_keys`] (zr z5/sw). Possibly empty: an
    /// implementation that does per-pause bookkeeping only behind a
    /// non-empty slice must do it here regardless, because the unfiltered
    /// slice it used to receive was almost never empty. The default
    /// delegates.
    fn prune_dead_filtered(&self, dead: &[usize]) {
        self.prune_dead(dead)
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
// (`impl Send/Sync for Heap` in heap.rs, `GenerationalHeap`'s in
// gen_heap.rs; line numbers dropped, they drift) meant any code with an `&Heap` could call
// `collect_garbage` from any thread without holding STW, which would have
// been instantly unsound — only luck (i.e. there is only one orchestrator)
// prevented a bug.
//
// `StopTheWorldToken` is a *type-level proof token*. Every mutating-by-
// `&self` collector entry point now requires `&StopTheWorldToken` as its
// first argument, so a thread that does not hold the token literally cannot
// call `collect_garbage` — the call won't compile. The orchestrator in
// `vm/src/runtime/interpreter/gc_and_alloc.rs` (`run_collection_pause`, the one
// pause behind `maybe_gc`, `maybe_gc_forced_at` and `force_gc_from_native`;
// the concurrent-mark pause drivers) constructs one token per STW round —
// after `stw_take_over_and_wait` has satisfied the barrier (every counted
// mutator parked, every in-JIT peer frozen and excused; with one thread the
// wait returns at once) — and threads it down to the heap.
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
    /// `vm/src/runtime/interpreter/gc_and_alloc.rs`, which builds a token only
    /// after the barrier is satisfied (`stw_take_over_and_wait` returned:
    /// every counted thread parked, every in-JIT peer OS-frozen and excused).
    /// Every VM collection door, including a one-thread process, requests the
    /// barrier first (`run_collection_pause`, gc-common w2-a): there is no
    /// barrier-free single-threaded fast path any more.
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
/// Trait abstracting a garbage-collected heap.
///
/// `GenerationalHeap`, `G1Collector` and `ZgcRealHeap` implement this trait
/// (the legacy semi-space `Heap` does not). The VM accesses the heap
/// through `VmHeap`, which dispatches to these methods.
pub trait GarbageCollector: Send + Sync {
    // -- Allocation --

    /// Allocate without collecting, with a fatal contract on heap exhaustion.
    ///
    /// This legacy entry has no failure channel and may abort or panic. It is
    /// only for callers that explicitly accept that contract. Java execution
    /// must use `try_alloc_object`, publish its roots, collect and retry, then
    /// throw `OutOfMemoryError` if allocation still fails.
    /// All `num_fields` slots of a successful allocation are zeroed.
    ///
    /// # Exhaustion is fatal
    ///
    /// On an exhausted heap every backend ends the process: ZGC prints a
    /// `FATAL:` report and `abort()`s, Generational aborts once its
    /// old-generation spill is full too, and G1 aborts after its emergency
    /// reserve. ZGC first offers the VM's exhaustion unwind
    /// (`ZgcRealHeap::set_exhaustion_unwind`), which raises a catchable
    /// `OutOfMemoryError` only inside a converting native funnel with no
    /// compiled frame between. HotSpot never aborts on heap exhaustion
    /// (`docs/feature-designs/zgc-architecture.md`).
    fn alloc_object(&self, class_id: ClassId, num_fields: usize) -> ObjectRef;

    /// Allocate an array without collecting; fatal on exhaustion or an invalid
    /// length, as for [`Self::alloc_object`]. Prefer `try_alloc_array` in Java
    /// paths.
    fn alloc_array(
        &self,
        class_id: ClassId,
        element_type: ArrayElementType,
        length: usize,
    ) -> ObjectRef;

    /// A collection-free object allocation attempt. `None` reports exhaustion
    /// or an unrepresentable layout. The caller owns rooting and GC/retry.
    ///
    /// The default refuses allocation, so older third-party collectors remain
    /// source compatible without silently calling a fatal allocation method.
    fn try_alloc_object(&self, _class_id: ClassId, _num_fields: usize) -> Option<ObjectRef> {
        None
    }

    /// A collection-free array allocation attempt, including length checks.
    /// The default refuses; it never delegates to `alloc_array`.
    fn try_alloc_array(
        &self,
        _class_id: ClassId,
        _element_type: ArrayElementType,
        _length: usize,
    ) -> Option<ObjectRef> {
        None
    }

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
    /// `*_as` methods — `GenerationalHeap`, `G1Collector` and `ZgcRealHeap`
    /// (`rg 'impl GarbageCollector for'`) all
    /// inherit these bodies verbatim, and `VmHeap`'s enum `dispatch!`
    /// (`macro_rules! dispatch` in vm_heap.rs) is what routes every descriptor-aware field access
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
    /// `VmHeap::g1_is_marking_active`; G1 and Generational decide on their
    /// SATB queue's `is_active()`, gc-common w5-e), callers MUST invoke
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
    /// branches; an implementor that keeps it pays zero instructions at the
    /// trait dispatch site. All three shipped collectors override it
    /// (gcj j1/cm: this named the semi-space `Heap`, which does not
    /// implement the trait, and `GenerationalHeap`, which overrides it,
    /// as users of the default). G1 overrides this and
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
mod coercion_provenance_tests;
