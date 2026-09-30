// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Java reference processing for the garbage collector.
//!
//! Implements the four reference types defined by `java.lang.ref`:
//! [`SoftReference`], [`WeakReference`], [`PhantomReference`], and the
//! internal [`Cleaner`] / [`FinalReference`] types used by the JDK.
//!
//! Processing order matches HotSpot, and reachability is recomputed between
//! phases so each level only sees objects the stronger levels have released:
//! 1. SoftReferences  -- cleared only under memory pressure (LRU policy),
//!    honouring the finalizer-reachable closure;
//! 2. WeakReferences  -- cleared when the referent is unreachable, honouring
//!    the finalizer-reachable AND soft-reachable closures;
//! 3. FinalReferences -- enqueued so the finalizer thread can run `finalize()`,
//!    honouring the soft-reachable closure (soft reachability blocks
//!    finalization; finalizer reachability deliberately does not, so mutually
//!    reachable finalizables are enqueued together);
//! 4. PhantomReferences and `Cleaner`s -- enqueued/fired only once the referent
//!    is neither soft- nor weak-reachable AND has already been finalized
//!    (JDK 9+: the phantom referent IS cleared as it is enqueued; the VM's
//!    referent writers do that, see `process_phantom_refs`).
//!
//! A finalizable object the collection itself resurrected (it was dead, and
//! the collector marked it only so `finalize()` can run) is unmarked for
//! phases 1-2 when the VM notes it (`note_resurrected_finalizables`, w6-d):
//! HotSpot clears soft and weak references to it before finalization.
//!
//! # Cost and locking
//!
//! Everything here runs inside the collector's stop-the-world pause, under the
//! single global `ref_processor` mutex (L7 in `vm/src/runtime/lock_order.rs`).
//! The lock therefore serialises mutator-side `discover_reference` /
//! `touch_soft_reference` against each other, not against the GC.
//!
//! Per-collection cost is **O(registered references)**, not O(live references):
//! phases 2-4 are flat scans over `weak_refs` / `cleaner_refs` /
//! `finalizer_refs` / `phantom_refs`, and an already-cleared or already-enqueued
//! entry is skipped but still visited. Phase 1's range scan is the one
//! sublinear walk — the `soft_ref_lru_index` BTreeMap range visits just the
//! entries idle enough to be clear candidates — but phase 1 is not sublinear as
//! a whole: its condemned-set arm and the soft-survivor closure roots walk every
//! soft row, and `condemn_idle_soft_refs` (pre-collection) walks them all too
//! (gen r5w1/refs5 corrected "Only phase 1 is sublinear"). The lists are bounded by `remove_collected`, which drops
//! entries whose `Reference` OBJECT died, so the scans stay proportional to
//! *live Reference objects* rather than to every reference ever created. A
//! `WeakHashMap` with a million live entries still costs a million-entry scan
//! per collection; see `refs-metaspace-unloading.md`
//! for the "cleared entries could be segregated into a cold list" sketch, and
//! `docs/internal/gc-common-round-20260923/common-d-proposal-settled-reference-cold-list-RETIRED-20260923.md`
//! (retired into `docs/known-issues/gc/common-d-proposal-marker-discovered-references.md`:
//! with a million LIVE `WeakHashMap` entries the rows are active, not settled,
//! so only discovery by the marker makes the pass O(unmarked referents)).
//!
//! That bound only holds if settled `Reference` objects can DIE. Until
//! 2026-09-23 an enqueued phantom stayed a synthetic GC root forever
//! (`pending_reference_object_addresses` filtered on `cleared`, which a
//! phantom never sets), so the phantom list — every `DirectByteBuffer`
//! `Cleaner`, every `PhantomCleanable` — grew without bound on every backend.
//! See that method's doc.

use std::collections::{BTreeMap, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::gc_flags;
use parking_lot::{Condvar, Mutex};
use rustc_hash::{FxHashMap, FxHashSet};

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

/// The kind of `java.lang.ref.Reference` subclass.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReferenceType {
    Strong,
    Soft,
    Weak,
    Phantom,
    Cleaner,
    Finalizer,
}

/// A single discovered reference.
#[derive(Debug, Clone)]
pub struct ReferenceEntry {
    pub ref_type: ReferenceType,
    /// Address of the `Reference` object itself on the heap.
    pub reference_obj: usize,
    /// Address of the referent (the object the reference points to).
    pub referent: usize,
    /// Address of the associated `ReferenceQueue`, if any.
    pub queue_addr: Option<usize>,
    /// Whether this reference has already been enqueued.
    pub enqueued: bool,
    /// Whether `clear()` has been called / the referent nulled.
    pub cleared: bool,
    /// Timestamp of the last `get()` call -- used for SoftReference LRU.
    pub last_access_time_ms: u64,
    /// bc math-ec 0x4 ROOT-CAUSE FIX (2026-06-10): once-only emission flags.
    /// The VM-side consumers WRITE heap fields for each emitted action
    /// (null the referent; submit the cleaner action). Before these flags the
    /// registry RE-EMITTED every cleared entry on EVERY GC, forever — and once
    /// the Reference object died, its recycled address was "remapped" onto
    /// whatever innocent object reused the memory, corrupting it with
    /// perfectly-legal-looking writes (the FixedPointTest `Object(Some(0x4))`
    /// / silent-null corruption; see h2-testscript-segv-findings.md).
    /// `clear_emitted`: the referent-null for this entry was already handed out.
    pub clear_emitted: bool,
    /// `action_emitted`: the cleaner action for this entry was already handed out.
    pub action_emitted: bool,
    /// PHANTOM entries only: this reference is a `jdk.internal.ref.Cleaner`, so
    /// clearing it must RUN it rather than enqueue it.
    ///
    /// The JDK draws exactly this distinction inside its `ReferenceHandler`
    /// thread: an ordinary phantom goes on its `ReferenceQueue`, while a
    /// `Cleaner` — whose queue is a private `dummyQueue` nothing ever polls —
    /// has `clean()` invoked directly. It has to stay a *phantom* entry rather
    /// than move to [`ReferenceType::Cleaner`], because only the phantom list
    /// gets its referent nulled before marking
    /// (`weakref_null_referents_pre_gc` -> `weak_phantom_active_pairs`): a
    /// `Cleaner` is reachable forever from its class's own static list, so
    /// without that nulling its referent is strongly reachable through it and
    /// can never die — a leak with no symptom until something counts the bytes.
    pub runs_cleaner: bool,
    /// gcd d1/c (2026-09-27) — the processor's registration sequence number
    /// when this row was discovered ([`ReferenceProcessor::registration_seq`]).
    /// Lets a caller prune only the rows that existed before some point, so a
    /// row registered for a NEW object at a recycled address is never taken
    /// for the dead one's (see
    /// [`ReferenceProcessor::remove_reference_objects_registered_before`]).
    pub registration_seq: u64,
}

/// Aggregated stats for one round of reference processing.
///
/// gen r5w1/refs5 (`gengc-r4-mark-counters-and-comments-that-lie`): the
/// `*_discovered` fields are the LENGTHS of the processor's lists when the
/// round began — every registered row, settled ones included — not
/// discoveries made by this round (rows are registered by the VM at
/// `Reference` construction, not by a marker). `phantom_refs_enqueued`
/// counts queue deliveries only; a `jdk.internal.ref.Cleaner` that runs is
/// `cleaner_refs_processed`. Finalizables flagged later by the resurrection
/// channel are not in any snapshot (`ReferenceProcessor::finalizers_enqueued_by_channel`).
#[derive(Debug, Default, Clone)]
pub struct ReferenceProcessingStats {
    pub soft_refs_discovered: usize,
    pub soft_refs_cleared: usize,
    pub weak_refs_discovered: usize,
    pub weak_refs_cleared: usize,
    pub phantom_refs_discovered: usize,
    pub phantom_refs_enqueued: usize,
    pub cleaner_refs_processed: usize,
    pub finalizer_refs_discovered: usize,
    pub finalizer_refs_enqueued: usize,
}

/// Outcome of [`ReferenceProcessor::process_references`].
pub struct ReferenceProcessingResult {
    /// `(reference_obj, queue_addr)` pairs to enqueue.
    pub to_enqueue: Vec<(usize, usize)>,
    /// Objects that need `finalize()` executed.
    pub to_finalize: Vec<usize>,
    /// Cleaner action addresses to run.
    pub cleaner_actions: Vec<usize>,
    /// gen r5w3/unload7 — the `reference_obj` of every soft, weak or phantom
    /// row THIS round moved out of the active state (neither cleared nor
    /// enqueued before, cleared or enqueued after): Phases 1, 2 and 4 push it
    /// at the transition. Finalizer and cleaner rows are not included (their
    /// object is the finalizable / the cleaner, not a `Reference` whose slot 0
    /// a consumer nulls). A consumer that needs "which rows retired" — the
    /// generational remark, which nulls each live one's referent slot — reads
    /// this instead of diffing `active_reference_object_set()` before and
    /// after (`gengc-r5w1-refs5-proposal-processor-reports-what-it-retired`).
    pub retired: Vec<usize>,
    /// Stats snapshot.
    pub stats: ReferenceProcessingStats,
}

// ---------------------------------------------------------------------------
// ReferenceQueue — RETIRED 2026-09-21
// ---------------------------------------------------------------------------

// `pub struct ReferenceQueue` and its `impl` lived here.
//
// It was a `pub`, `pub use`-exported `VecDeque` wrapper with **no production
// caller** anywhere in the workspace: every hit for the name outside this file
// was either the Java class NAME (`java/lang/ref/ReferenceQueue` in a
// descriptor, a thread-dump fixture string, or a registry entry) or a test
// constructing the Rust type to test the Rust type. Nothing else reached it.
//
// What a Java program actually observes is the heap `ReferenceQueue` OBJECT:
// the collector's part in it is the `(reference_obj, queue_addr)` pairs in
// `ReferenceProcessingResult::to_enqueue`, which
// `vm/src/runtime/interpreter/gc_and_alloc.rs` splices into that object's own
// linked list, and `ReferenceQueue.remove(long)` is served by
// `native-builtins/src/reference.rs` — which pins the receiver as a native
// root, backs off exponentially, and parks inside a blocking region so the
// collector can still reach a safepoint. None of that was in this type, and no
// caller could have been migrated onto it.
//
// It went because, of the four methods that did anything (`enqueue`, `poll`,
// `remove_timeout`, `remove_blocking` — the rest are accessors), two could not
// do what they said and a third lost data on purpose:
//
//   * `remove_timeout(&mut self, ms)` / `remove_blocking(&mut self)` took an
//     EXCLUSIVE borrow of the queue and then waited for someone else to
//     `enqueue` into it. Nothing in the process could, for the whole of the
//     wait: the borrow is the proof. `remove_blocking` asked for sixty
//     seconds of that. A caller reaching for the obvious-looking name got a
//     one-minute hang instead of a wait. Making the wait honest means
//     returning immediately, which is not a wait, so there was nothing to
//     port the tests that pinned the elapsed duration onto.
//   * `enqueue` evicted the HEAD at capacity — dropping the entry a consumer's
//     drain loop was about to take, keeping the one it had not asked for, and
//     returning `true` on the path that had just destroyed a different entry.
//     `java.lang.ref`'s queue has no capacity at all.
//
// Fixing it properly would have been interior mutability (`Mutex<VecDeque>`
// behind `&self`) plus a `Condvar` — i.e. writing, from scratch, a second
// implementation of what `native-builtins/src/reference.rs` already ships, for
// no caller. See
// `docs/internal/gc/mark-reference-queue-dead-surface-20260920-RETIRED-20260921.md`
// for the full argument, and
// `docs/internal/gc/gengc-mark-reference-queue-overflow-loses-entries-RETIRED-20260923.md`
// for the eviction-policy page this retirement closes.
//
// The tests that constructed it went with it: seven in this file's `tests`
// module (numbered 16, 17, 41, 42, 43, 47, plus
// `remove_timeout_still_honours_its_deadline`), two in `gc/tests/wp1_10_reference.rs`
// (WP1.10.F), and `vm/tests/tier1_tests.rs::t1_reference_queue_remove_honors_timeout`.
// Each site carries a marker saying what went and why.

// ---------------------------------------------------------------------------
// Referent-class stamps that also record the referent's KIND
// ---------------------------------------------------------------------------

/// Bit 31 of a referent-class stamp ([`ReferenceProcessor::stamp_referent_class`]):
/// the stamp also records whether the referent was an ARRAY (bit 30).
///
/// A class id alone aliases an array with its component: a CratonVM array's
/// header carries the COMPONENT's class id, so an `Integer[]` referent and an
/// `Integer` that later reuses the address stamp the same id and the restore
/// screen cannot tell them apart (`r11w13-rt-array-alias-audit-jni-refs-gc`,
/// open item 1). A stamp WITHOUT this bit keeps its old meaning — a bare class
/// id, kind unknown — so the screen ([`referent_stamp_admits`]) and the writer
/// (the VM's pre-GC null pass) can land in either order. Class ids are dense
/// and far below 2^30.
pub const REFERENT_STAMP_KIND_KNOWN: u32 = 1 << 31;
/// Bit 30 of a referent-class stamp: the referent was an ARRAY. Meaningful
/// only with [`REFERENT_STAMP_KIND_KNOWN`].
pub const REFERENT_STAMP_ARRAY: u32 = 1 << 30;

/// May the post-GC restore install the object now at the referent's address,
/// whose class id is `have_class` and whose kind is `have_is_array`, against
/// the stamp `want` recorded before the collection?
///
/// A plain class-id stamp compares the class id alone, exactly as the screen
/// always did. A kind-aware stamp also requires the kind to agree, which is
/// what refuses an `Integer` found where an `Integer[]` was stamped (and the
/// reverse). The caller treats `want == 0` as unstamped before asking.
pub fn referent_stamp_admits(want: u32, have_class: u32, have_is_array: bool) -> bool {
    if want & REFERENT_STAMP_KIND_KNOWN == 0 {
        return have_class == want;
    }
    let want_class = want & !(REFERENT_STAMP_KIND_KNOWN | REFERENT_STAMP_ARRAY);
    let want_is_array = want & REFERENT_STAMP_ARRAY != 0;
    have_class == want_class && have_is_array == want_is_array
}

// ---------------------------------------------------------------------------
// ReferenceProcessor
// ---------------------------------------------------------------------------

/// Central reference processor invoked during GC pauses.
pub struct ReferenceProcessor {
    soft_refs: Vec<ReferenceEntry>,
    weak_refs: Vec<ReferenceEntry>,
    phantom_refs: Vec<ReferenceEntry>,
    cleaner_refs: Vec<ReferenceEntry>,
    finalizer_refs: Vec<ReferenceEntry>,

    /// gen r5w3/unload7 — the rows the processing round in progress retired;
    /// emptied at the round's start and moved into
    /// [`ReferenceProcessingResult::retired`] at its end.
    round_retired: Vec<usize>,

    /// `-XX:SoftRefLRUPolicyMSPerMB` equivalent (default 1000).
    soft_ref_lru_policy_ms_per_mb: u64,

    /// `queue_addr -> [reference_obj ...]` for pending enqueue notifications.
    /// T10.9.B: FxHashMap — queue addresses are internal pointer values.
    pending_queues: FxHashMap<usize, Vec<usize>>,

    /// gcd d9/e — pairs dropped from `pending_queues` because their
    /// `Reference` died before delivery (see `drop_pending_enqueues_of`).
    pending_enqueues_dropped: u64,

    /// Objects awaiting `finalize()`.
    ///
    /// Round-2 fix (GC §7): `VecDeque` so `take_pending_finalizer` is O(1)
    /// pop-front instead of O(n) `Vec::remove(0)`.
    finalization_queue: std::collections::VecDeque<usize>,

    /// Index of soft references by `last_access_time_ms` for efficient
    /// range-based LRU clearing. Maps `(timestamp, index_in_soft_refs)` to
    /// allow O(log n) range queries instead of O(n) linear scans.
    soft_ref_lru_index: BTreeMap<(u64, usize), usize>,

    /// PERF: address -> position-in-`soft_refs` index, so `touch_soft_reference`
    /// (called on every `SoftReference.get()` that advances the LRU timer) is
    /// O(1) instead of a linear scan over all soft refs. For large soft-ref
    /// populations (memory-sensitive caches) the old per-`get()` linear scan
    /// made `get()` O(n). This map is kept in lock-step with `soft_refs`
    /// positions at every mutation site (discover/relocate/remove_collected);
    /// the `(reference_obj -> idx)` invariant mirrors the `soft_ref_lru_index`
    /// `(timestamp, idx) -> idx` invariant the LRU-leak fix already maintains.
    soft_ref_addr_index: cratonvm_types::PointerMap,

    /// Highest wall-clock millisecond value the *mutator* side has ever handed
    /// this processor through [`Self::touch_soft_reference`].
    ///
    /// SOFT-POLICY FIX (2026-07-26): the soft-reference LRU compares two
    /// numbers that came from different places, and in production they were
    /// on different clocks, which disabled the policy outright.
    ///
    /// * `last_access_time_ms` on each soft entry is stamped by
    ///   `NativeContext::touch_soft_reference` (`vm/src/vm/vm_exec.rs`), which
    ///   uses `SystemTime::now()` — i.e. real Unix-epoch milliseconds
    ///   (~1.7e12). It fires from `SoftReference.<init>` *and* every
    ///   `SoftReference.get()` (`native-builtins/src/reference.rs`).
    /// * `current_time_ms`, the argument to [`Self::process_references`], is a
    ///   hard-coded `0` at every production call site
    ///   (`vm/src/runtime/interpreter/gc_and_alloc.rs`: the post-GC path
    ///   `process_references_after_gc` and the G1 final remark
    ///   `g1_remark_process_references`, which the generational remark reuses
    ///   since gen r5w1/refs5; the pre-collection `condemn_idle_soft_refs` call
    ///   in `vm/src/runtime/interpreter.rs` passes a real clock).
    ///
    /// With `current_time_ms == 0` the cutoff in [`Self::process_soft_refs`] is
    /// `0.saturating_sub(threshold) == 0`, so the BTreeMap range selects only
    /// entries still at timestamp `0`, and for those `idle_ms` is also `0` —
    /// never greater than the threshold. Net effect: **no SoftReference could
    /// ever be cleared on either VM path**, so soft references behaved exactly
    /// like strong ones and `OutOfMemoryError` was reached with a heap full of
    /// reclaimable soft-reachable objects. (Independently observed in
    /// `core39-clusterD-lifecycle-ssl-validation-FIXED.md`: "`process_soft_refs`
    /// never ran even once during the whole failing run".)
    ///
    /// Rather than depend on a caller-side change in a file this module does
    /// not own, the processor now tracks the mutator clock itself and uses
    /// `max(current_time_ms, last_observed_clock_ms)` as "now". A caller that
    /// supplies a coherent clock (ZGC's backend, and every unit/integration
    /// test) is unaffected, because its `current_time_ms` already dominates.
    /// A caller that supplies `0` gets the real clock instead of a dead policy.
    last_observed_clock_ms: u64,

    /// `reference_obj -> the identity hash the object carried when this
    /// processor discovered it`.
    ///
    /// THE SAME-CLASS HOLE. Every write this processor's consumers perform
    /// goes through a raw address, and the guards around them are SHAPE tests:
    /// "does the object at this address still look like a `Reference`?". A
    /// shape test cannot see the case where a reclaimed `Reference`'s address
    /// is re-issued to ANOTHER `Reference` -- and H2 allocates
    /// `org.h2.util.CloseWatcher` (a `PhantomReference`) per connection, so
    /// that case is not hypothetical. Measured on the fixed VM: over 3200
    /// phantom references through one queue on 8 threads, 2-5 of them per run
    /// arrive as a DIFFERENT, half-constructed instance of the same class.
    ///
    /// The identity hash is the exact test the shape test approximates. It is
    /// minted from a monotonic counter (`VmHeap::identity_hash_code`), lives in
    /// the object's own mark word, and travels with the object when a collector
    /// relocates it -- so it is the "monotonic registration id written into the
    /// object" the write-up asked for, and it already exists. A reused address
    /// answers with a different hash (or is unhashed and mints a fresh one,
    /// which is likewise different), so the mismatch is decisive.
    ///
    /// `0` and a missing key both mean UNSTAMPED, and unstamped falls back to
    /// the shape guards rather than declining: the in-tree tests construct
    /// entries with no heap behind them, and a stamp that defaulted to
    /// "refuse" would silently stop reference processing there.
    identity_stamps: FxHashMap<usize, i32>,

    /// The CLASS of each entry's referent, keyed by the same `reference_obj`
    /// address as [`Self::identity_stamps`].
    ///
    /// The identity stamp above proves the REFERENCE object is the one that
    /// was discovered. Nothing proved the same about the REFERENT, and the
    /// post-GC restore pass writes it into slot 0 — so an address that was
    /// freed and handed to a different object made that pass install a
    /// stranger as somebody's referent. Under a compacting collector that is
    /// not exotic: survivors slide DOWN into the space dead objects vacated,
    /// so a dead referent's base is very often a live object's new base, and
    /// ZGC's `is_object_address` answers "yes, an object lives here" for it.
    /// Measured: `SoftReference.get()` returning a `java.io.ClassCache$CacheRef`
    /// where `MethodTypeForm.cachedLambdaForm` expects a `LambdaForm`.
    ///
    /// A class id is a header READ — unlike an identity hash it mints
    /// nothing, which matters because the only place with the referent in hand
    /// is the pre-GC null pass, and minting into a mark word the collector is
    /// about to use is not a trade worth making.
    ///
    /// `0` and a missing key both mean UNSTAMPED, and unstamped admits — same
    /// convention as the identity stamps, and for the same reason.
    ///
    /// A class id alone cannot tell an `Integer[]` from an `Integer`: an
    /// array's header carries its COMPONENT's class id. A stamp may therefore
    /// also record the referent's KIND ([`REFERENT_STAMP_KIND_KNOWN`] /
    /// [`REFERENT_STAMP_ARRAY`]); the restore screen judges it with
    /// [`referent_stamp_admits`], which keeps a plain class-id stamp's meaning.
    referent_class_stamps: FxHashMap<usize, u32>,

    /// `reference_obj` addresses of the SOFT entries the *pre-collection* pass
    /// condemned for this cycle, i.e. the ones whose referent slot the VM
    /// nulled before the marker ran (see [`Self::condemn_idle_soft_refs`]).
    ///
    /// Rewritten wholesale by every `condemn_*` call, which is the reset
    /// point, and cleared by [`Self::finish_pre_gc_cycle`] once the post-GC
    /// pass has consumed it: the set describes one collection and must never
    /// span two.
    ///
    /// gc-common w1-d (2026-09-23): the "a cycle that skips the pre-collection
    /// pass sees the previous cycle's set, and that is deliberately harmless"
    /// argument this doc used to make does not hold for G1's final remark. The
    /// remark runs no pre-collection pass, HIDES every soft referent slot from
    /// the marker, and then runs `process_soft_refs` against the bitmap — so a
    /// stale entry here took the condemned branch (no idle check at all) for a
    /// soft reference the application may have read a moment ago, and cleared
    /// it. `finish_pre_gc_cycle` now empties the set after the pass that filled
    /// it.
    soft_pre_nulled: FxHashSet<usize>,

    /// `reference_obj` addresses whose referent slot the pre-collection pass
    /// ACTUALLY nulled this cycle — i.e. the slot held a non-null referent and
    /// every guard in `weakref_null_referents_pre_gc` admitted the write.
    /// Filled through [`Self::stamp_referent_class`], which that pass calls for
    /// exactly those entries; reset by [`Self::weak_phantom_active_triples`]
    /// (the pass's first call) and by [`Self::finish_pre_gc_cycle`].
    ///
    /// gc-common w1-d (2026-09-23). The post-GC restore pass used to write the
    /// registry's `referent` back into slot 0 of EVERY still-active entry. That
    /// is right only for a slot this cycle nulled. For a slot the APPLICATION
    /// nulled — `Reference.clear()`, which native `native_ref_clear`
    /// implements as a bare field store the registry never hears about — it
    /// resurrected the referent: `ref.clear(); System.gc(); ref.get()` answered
    /// the old object whenever it was still strongly reachable, on all three
    /// collectors. See [`Self::was_nulled_by_pre_gc_pass`].
    pre_gc_nulled: FxHashSet<usize>,

    /// `reference_obj` addresses of the WEAK and SOFT entries the
    /// pre-collection pass examined this cycle (every active weak entry, and
    /// the soft entries it condemned). An examined entry the pass did NOT null
    /// had a slot that was already null — the application cleared it — or
    /// failed the pass's shape/identity guards (a recycled address). Either
    /// way the GC must never clear-and-enqueue it later; see
    /// [`Self::retire_entries_the_pre_gc_pass_found_cleared`]. Phantom entries
    /// are deliberately not recorded: a phantom slot is invisible to `get()`,
    /// and a refused restore leaves one null for reasons that must NOT cancel
    /// its notification.
    pre_gc_examined: FxHashSet<usize>,

    /// `reference_obj -> (list, position)` for the soft, weak and phantom
    /// lists: the O(1) lookup behind [`Self::retire_by_application`], which
    /// every application `Reference.clear()` / `Reference.enqueue()` on a
    /// queued reference reaches (gc-common w2-f). It used to be a linear
    /// `find` over four lists under the global processor mutex, per call.
    ///
    /// LAZY. Positions shift whenever a list is `retain`ed and addresses change
    /// whenever a collection relocates rows; rather than maintain the map at
    /// each of those sites inside the pause, they only set
    /// [`Self::app_row_index_stale`], and the first lookup after them rebuilds
    /// the map (O(rows), once per collection at most, and never when the
    /// application does not clear or enqueue a queued reference).
    /// `discover_reference` appends to a fresh map incrementally.
    ///
    /// Where two rows share an address (a recycled address whose old row has
    /// not been pruned yet), the ACTIVE row wins, and among rows of the same
    /// state the later-discovered one — the live object is always the newer
    /// registration.
    app_row_index: FxHashMap<usize, (AppRowList, usize)>,
    /// See [`Self::app_row_index`]. `true` until first built.
    app_row_index_stale: bool,

    /// A last-ditch soft clear some thread asked for and no collection has
    /// applied yet. See [`Self::request_clear_all_soft_refs`].
    ///
    /// gc-common w4-d (2026-09-23), `common-d-reference-protocol-residuals` §4.
    /// Per VM (this processor is `SharedVm::mem.ref_processor`) and read under
    /// the processor lock the pre-collection pass already holds, so it is
    /// neither a process global nor a new lock.
    clear_all_soft_requested: bool,

    /// The "soft index epoch": bumped whenever the address -> soft-row
    /// resolution [`Self::touch_soft_reference`] uses may have changed (a
    /// relocation, a prune). Shared as an atomic so the VM's `get()` fast path
    /// can read it WITHOUT this processor's lock. See [`soft_touch_stamp`].
    ///
    /// gc-common w6-d (2026-09-24),
    /// `common-d-proposal-soft-reference-clock-without-the-global-lock-REJECTED-20260928`. Per VM
    /// (this processor is `SharedVm::mem.ref_processor`); not a process global.
    soft_touch_epoch: Arc<AtomicU64>,

    /// PRE-collection addresses of the finalizable objects the collection
    /// being processed found unreachable and RESURRECTED (the collector's
    /// dead-finalizer list, mapped back through the pointer map by the VM).
    /// Consumed by the next [`Self::process_references_with_finalizer_trace`]
    /// and emptied by [`Self::finish_pre_gc_cycle`]: it describes ONE
    /// collection. See [`Self::note_resurrected_finalizables`].
    resurrected_this_cycle: FxHashSet<usize>,

    /// `reference_obj` addresses of the ACTIVE (neither enqueued nor cleared)
    /// rows of `finalizer_refs`: what makes a second
    /// `discover_reference(Finalizer, ..)` of the same object a no-op in O(1).
    ///
    /// gc-common w18-g (2026-09-25), `handoff-w2d-finalizer-registration-native`
    /// item 4. `SharedVm::register_finalizable` appended a row per call, so an
    /// object registered twice (the `Finalizer.register` native, `clone()`, JNI
    /// `NewObject` and the interpreter's `new` are all registrars since w2-d /
    /// w4 / w5-d) would have been finalized twice.
    ///
    /// ACTIVE rows only, deliberately. An enqueued row whose object was
    /// finalized and then reclaimed may still be listed when a NEW finalizable
    /// object is allocated at its recycled address; refusing that
    /// registration would switch the new object's `finalize()` off (the
    /// address-keyed refusal w1-d removed from `FinalizerThread::enqueue`).
    /// An ACTIVE row at the address is either this very object's row (the
    /// duplicate) or a stale row of a dead object nothing processed, which
    /// already fires for whatever lives at the address -- so declining the
    /// second row yields exactly one `finalize()` in both cases.
    ///
    /// LAZY, like [`Self::app_row_index`]: every site that can make an active
    /// row inactive, move it or drop it only sets
    /// [`Self::finalizer_active_stale`], and the next Finalizer discovery
    /// rebuilds the set (O(finalizer rows), at most once per collection -- the
    /// same walk `process_final_refs` already makes every collection). A
    /// SUPERSET would be the dangerous direction (it refuses a genuine
    /// registration), which is why every such site marks it stale rather than
    /// patching it.
    finalizer_active: FxHashSet<usize>,
    /// See [`Self::finalizer_active`]. `true` until first built.
    finalizer_active_stale: bool,

    /// gen r5w1/refs5 — `CRATONVM_SOFTREF_HOTSPOT_LRU`, latched when the
    /// processor is built (one per VM; gen r5w4/defaults8: for the VM's heap,
    /// [`Self::new_for_heap`] — ON by default on the Generational heap, OFF on
    /// G1 and ZGC and for a processor built without a heap in hand, an
    /// explicit setting winning everywhere). `true` evaluates the soft LRU policy
    /// with HotSpot's `LRUMaxHeapPolicy` inputs: the clock as of the END of the
    /// previous collection ([`Self::soft_gc_clock_ms`]) and the free heap AFTER
    /// it ([`Self::soft_free_mb_at_last_gc`]), instead of the wall clock now
    /// and the free space the caller measured before the collection. See
    /// `docs/internal/gc/gengc-r4-mark-softref-policy-uses-prefill-free-space-FIXED-20260928.md`.
    hotspot_soft_lru: bool,
    /// HotSpot's `SoftReference.clock` (`_soft_ref_timestamp_clock`): the
    /// time, in the processor's mutator-clock milliseconds, at which the
    /// previous processing round ended. `0` before the first one, so before
    /// any collection no soft reference is idle (HotSpot: every timestamp
    /// equals the initial clock). Only read under [`Self::hotspot_soft_lru`].
    soft_gc_clock_ms: u64,
    /// The "now" a pre-collection pass saw ([`Self::condemn_idle_soft_refs`]),
    /// committed into [`Self::soft_gc_clock_ms`] by the processing round that
    /// ends the same collection. `0` when none is pending.
    soft_clock_pending_ms: u64,
    /// HotSpot's `max_capacity - used_at_last_gc`, in megabytes: the free heap
    /// the previous processing round was handed (post-collection on the STW
    /// paths). `None` before the first round, when the caller's figure is used.
    soft_free_mb_at_last_gc: Option<usize>,
    /// See [`Self::finalizers_enqueued_by_channel`].
    finalizers_enqueued_by_channel: u64,
    /// gcd d1/c — the sequence number the next discovered row gets (see
    /// [`ReferenceEntry::registration_seq`]). Monotone.
    next_registration_seq: u64,

    stats: ReferenceProcessingStats,
}

/// Lowest bit the millisecond clock occupies in a [`soft_touch_stamp`]; the
/// bits below it carry the low bits of the soft index epoch.
const SOFT_TOUCH_EPOCH_BITS: u32 = 20;

/// The value the VM stores in a `SoftReference`'s own `timestamp` field after
/// a locked [`ReferenceProcessor::touch_soft_reference`] at `now_ms`, with the
/// processor's soft index epoch read under that same lock; and the value a
/// later `get()` compares the field against, WITHOUT the lock, to prove that
/// its own touch would change nothing.
///
/// gc-common w6-d (2026-09-24),
/// `common-d-proposal-soft-reference-clock-without-the-global-lock-REJECTED-20260928`.
///
/// # Why a matching field makes the touch a no-op
///
/// The field lives in the object, so it is this object's alone: a relocation
/// carries it, and a new object at a recycled address starts at `0`, which no
/// stamp equals (`now_ms` is never `0` here). A matching field therefore says
/// that an earlier touch OF THIS OBJECT, at this same millisecond, completed
/// under the lock while the epoch was what it is now. The epoch changes
/// whenever a collection moves or prunes anything the address -> row
/// resolution depends on ([`ReferenceProcessor::update_after_gc`],
/// [`ReferenceProcessor::remove_collected`] and its narrow twin), so the
/// address still resolves to the row that touch updated. Only a touch of the
/// same address re-keys that row, and every such touch rewrites the field
/// under the same lock, so the row still holds `now_ms`; and
/// `last_observed_clock_ms` is monotone and was raised to at least `now_ms`.
/// A second touch would hit the `now_ms == last_access_time_ms` return and
/// leave every table as it is. The unit test
/// `w6d_stamped_soft_touch_is_equivalent_to_the_locked_touch` replays random
/// operation sequences through both protocols and compares the processors.
///
/// `None` for a clock of `0` (the VM's "no clock" fallback) or past 2^43 ms
/// (year 2248): the caller then always takes the locked path, and after it
/// writes `0` to the field -- a locked touch without a stamp still moves the
/// row, so the field must stop vouching for an earlier stamp (w6 orchestrator:
/// touch at 1, touch at 0, touch at 1 again was otherwise skipped).
pub fn soft_touch_stamp(now_ms: u64, epoch: u64) -> Option<i64> {
    if now_ms == 0 || now_ms >= (1u64 << (63 - SOFT_TOUCH_EPOCH_BITS)) {
        return None;
    }
    let epoch_bits = epoch & ((1u64 << SOFT_TOUCH_EPOCH_BITS) - 1);
    Some(((now_ms << SOFT_TOUCH_EPOCH_BITS) | epoch_bits) as i64)
}

/// Point the soft address index at `rows[idx]` unless a LATER registration
/// already owns its address; answer whether an earlier row lost it.
///
/// gcd d3/n (2026-09-27, `gcd-d2g-soft-touch-index-prefers-a-dead-row-at-a-
/// reused-address`): two soft rows share an address only while a dead
/// `Reference`'s row outlives its object and a new `Reference` was allocated
/// on the freed block; the new one registered later, and it is the one a
/// `get()` at that address touches. Same rule as
/// [`ReferenceProcessor::rebuild_app_row_index`] applies between active rows.
/// `soft_refs` is appended in registration order, so this is also "the last
/// occurrence wins"; the sequence is compared anyway so the rule does not rest
/// on that ordering.
fn claim_soft_addr(
    index: &mut cratonvm_types::PointerMap,
    rows: &[ReferenceEntry],
    idx: usize,
) -> bool {
    let e = &rows[idx];
    let displaced = match index.get(&e.reference_obj) {
        Some(&owner) if owner == idx => return false,
        Some(&owner) => match rows.get(owner) {
            Some(prev) if prev.registration_seq > e.registration_seq => return false,
            // A later registration (or an index past the list, which no
            // caller leaves) does not keep the address.
            _ => true,
        },
        None => false,
    };
    index.insert(e.reference_obj, idx);
    displaced
}

/// Which list an [`ReferenceProcessor::app_row_index`] position is in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AppRowList {
    Soft,
    Weak,
    Phantom,
}

impl ReferenceProcessor {
    pub fn new() -> Self {
        Self::new_with_policy(1000)
    }

    /// The VM's processor: `-XX:SoftRefLRUPolicyMSPerMB` (`None` = HotSpot's
    /// 1000, exactly [`Self::new`]) and the soft LRU inputs selected for the
    /// heap the VM built — gen r5w4/defaults8.
    ///
    /// `generational` is `VmHeap::is_generational()` (or `GcBackend ==
    /// Generational`) of the VM this processor serves. `CRATONVM_SOFTREF_HOTSPOT_LRU`'s
    /// default is per backend (`alloc_policy_defaults::SOFTREF_HOTSPOT_LRU`):
    /// ON for the Generational heap, where HotSpot's inputs were measured to
    /// match HotSpot Serial (`GenR5W2SoftRefIdleYoungProbe` `lost=0 PASS`,
    /// against `lost=2 FAIL` on the wall-clock inputs); OFF for G1 and ZGC,
    /// which are therefore byte-for-byte what [`Self::new_with_policy`] built
    /// before. An explicit setting wins on every backend. Per VM: the choice is
    /// latched in this processor, no process state.
    pub fn new_for_heap(soft_ref_lru_ms_per_mb: Option<u64>, generational: bool) -> Self {
        let mut proc = match soft_ref_lru_ms_per_mb {
            Some(ms) => Self::new_with_policy(ms),
            None => Self::new(),
        };
        proc.hotspot_soft_lru = gc_flags().softref_hotspot_lru_for(generational);
        proc
    }

    /// A processor with no heap in hand latches the soft LRU switch's
    /// NON-Generational default (off unless set) — what every processor did
    /// before gen r5w4/defaults8. The VM builds its own with
    /// [`Self::new_for_heap`].
    pub fn new_with_policy(soft_ref_lru_ms_per_mb: u64) -> Self {
        Self {
            soft_refs: Vec::new(),
            weak_refs: Vec::new(),
            phantom_refs: Vec::new(),
            cleaner_refs: Vec::new(),
            finalizer_refs: Vec::new(),
            round_retired: Vec::new(),
            soft_ref_lru_policy_ms_per_mb: soft_ref_lru_ms_per_mb,
            pending_queues: FxHashMap::default(),
            finalization_queue: std::collections::VecDeque::new(),
            soft_ref_lru_index: BTreeMap::new(),
            soft_ref_addr_index: cratonvm_types::PointerMap::default(),
            last_observed_clock_ms: 0,
            identity_stamps: FxHashMap::default(),
            referent_class_stamps: FxHashMap::default(),
            soft_pre_nulled: FxHashSet::default(),
            pre_gc_nulled: FxHashSet::default(),
            pre_gc_examined: FxHashSet::default(),
            app_row_index: FxHashMap::default(),
            app_row_index_stale: true,
            clear_all_soft_requested: false,
            soft_touch_epoch: Arc::new(AtomicU64::new(0)),
            resurrected_this_cycle: FxHashSet::default(),
            finalizer_active: FxHashSet::default(),
            finalizer_active_stale: true,
            hotspot_soft_lru: gc_flags().softref_hotspot_lru_for(false),
            soft_gc_clock_ms: 0,
            soft_clock_pending_ms: 0,
            soft_free_mb_at_last_gc: None,
            finalizers_enqueued_by_channel: 0,
            next_registration_seq: 0,
            pending_enqueues_dropped: 0,
            stats: ReferenceProcessingStats::default(),
        }
    }

    /// gen r5w1/refs5 — the soft LRU policy's `(clock, max_interval_ms)` under
    /// [`Self::hotspot_soft_lru`]: HotSpot's `LRUMaxHeapPolicy::setup` with
    /// `SoftRefLRUPolicyMSPerMB` = this processor's `soft_ref_lru_policy_ms_per_mb`.
    /// A reference is condemned iff `clock - last_access > max_interval`.
    fn hotspot_soft_policy(&self, fallback_free_mb: usize) -> (u64, u64) {
        let free_mb = self.soft_free_mb_at_last_gc.unwrap_or(fallback_free_mb);
        (
            self.soft_gc_clock_ms,
            self.soft_ref_lru_policy_ms_per_mb
                .saturating_mul(free_mb as u64),
        )
    }

    /// gen r5w1/refs5 — end of a processing round under
    /// [`Self::hotspot_soft_lru`]: remember the free heap it was handed and
    /// advance the soft clock (HotSpot's `update_soft_ref_master_clock`, which
    /// runs after the soft list is processed). The clock is the pre-collection
    /// pass's "now" when one ran this collection, else the latest time the
    /// processor knows of (a remark has no pre-collection pass and passes no
    /// clock; the mutator clock from `SoftReference.get()` is then the lower
    /// bound it has). Monotone.
    fn commit_hotspot_soft_clock(&mut self, free_heap_mb: usize, now_ms: u64) {
        let pending = std::mem::take(&mut self.soft_clock_pending_ms);
        self.soft_gc_clock_ms = self.soft_gc_clock_ms.max(pending).max(now_ms);
        self.soft_free_mb_at_last_gc = Some(free_heap_mb);
    }

    /// The soft index epoch, shared: the VM's `SoftReference.get()` fast path
    /// keeps a clone and reads it without this processor's lock. See
    /// [`soft_touch_stamp`].
    pub fn soft_touch_epoch(&self) -> Arc<AtomicU64> {
        Arc::clone(&self.soft_touch_epoch)
    }

    /// The soft index epoch's current value. Read under this processor's lock
    /// by the locked touch, whose stamp must name the epoch its row resolution
    /// belongs to.
    pub fn soft_touch_epoch_now(&self) -> u64 {
        self.soft_touch_epoch.load(Ordering::Acquire)
    }

    /// Invalidate every `SoftReference.timestamp` stamp minted so far: the
    /// address -> soft-row resolution may just have changed.
    fn bump_soft_touch_epoch(&self) {
        self.soft_touch_epoch.fetch_add(1, Ordering::AcqRel);
    }

    // -- Discovery ----------------------------------------------------------

    /// [`Self::discover_reference`] for a `jdk.internal.ref.Cleaner`.
    ///
    /// Tracked as a PHANTOM (which is what it is — the class extends
    /// `PhantomReference`) so it participates in the pre-GC referent nulling,
    /// but flagged [`ReferenceEntry::runs_cleaner`] so clearing it emits a
    /// cleaner ACTION instead of a queue entry. See that field for why the two
    /// halves cannot be separated.
    pub fn discover_phantom_cleaner(
        &mut self,
        reference_obj: usize,
        referent: usize,
        queue: Option<usize>,
    ) {
        self.discover_reference(ReferenceType::Phantom, reference_obj, referent, queue);
        if let Some(entry) = self.phantom_refs.last_mut() {
            entry.runs_cleaner = true;
        }
        if dm_dbg_enabled() {
            DM_PC_DISCOVERED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
    }

    /// Register a newly-discovered reference: called by the VM when a
    /// `Reference` is constructed (the constructor bridges) or a finalizable
    /// object is registered, not by a marker — this processor has no
    /// marking-phase discovery. (gen r5w1/refs5: the first line of this doc,
    /// "during the marking phase", sat on `discover_phantom_cleaner`.)
    pub fn discover_reference(
        &mut self,
        ref_type: ReferenceType,
        reference_obj: usize,
        referent: usize,
        queue: Option<usize>,
    ) {
        // gc-common w18-g: a finalizable object is registered at most once --
        // a second registration while its row is still active is a no-op, so
        // `finalize()` cannot run twice. See `finalizer_active` for why only
        // ACTIVE rows count.
        if ref_type == ReferenceType::Finalizer {
            if self.finalizer_active_stale {
                self.rebuild_finalizer_active();
            }
            if !self.finalizer_active.insert(reference_obj) {
                return;
            }
        }
        // SOFT-POLICY FIX (2026-07-26): stamp a soft entry with the processor's
        // best knowledge of the mutator clock at *creation* time, mirroring
        // `java.lang.ref.SoftReference`'s constructor (`this.timestamp = clock`).
        //
        // This matters now that [`Self::process_soft_refs`] falls back to
        // `last_observed_clock_ms` for "now": a soft reference discovered
        // through a path that does not also call
        // [`Self::touch_soft_reference`] would otherwise sit at timestamp `0`
        // and look infinitely idle against a wall-clock "now", so the very
        // first collection after the process had been up for
        // `SoftRefLRUPolicyMSPerMB * free_mb` would clear it even though the
        // application had just allocated it.
        //
        // `last_observed_clock_ms` is `0` until the first
        // `touch_soft_reference`, so this is a no-op for every caller that
        // never touches (all current unit and integration tests), and equals
        // "roughly now" for the live VM, where `SoftReference.<init>` touches.
        let creation_stamp = match ref_type {
            ReferenceType::Soft => self.last_observed_clock_ms,
            _ => 0,
        };
        let entry = ReferenceEntry {
            ref_type,
            reference_obj,
            referent,
            queue_addr: queue,
            enqueued: false,
            cleared: false,
            last_access_time_ms: creation_stamp,
            clear_emitted: false,
            action_emitted: false,
            runs_cleaner: false,
            registration_seq: self.next_registration_seq,
        };
        self.next_registration_seq += 1;
        // A fresh row is active, so it wins its address in the application
        // retirement index (see `app_row_index`). Only while that index is
        // current; a stale one is rebuilt from the lists on its next use.
        if !self.app_row_index_stale {
            let slot = match ref_type {
                ReferenceType::Soft => Some((AppRowList::Soft, self.soft_refs.len())),
                ReferenceType::Weak => Some((AppRowList::Weak, self.weak_refs.len())),
                ReferenceType::Phantom => Some((AppRowList::Phantom, self.phantom_refs.len())),
                _ => None,
            };
            if let Some(slot) = slot {
                self.app_row_index.insert(reference_obj, slot);
            }
        }
        match ref_type {
            ReferenceType::Soft => {
                let idx = self.soft_refs.len();
                self.soft_ref_lru_index
                    .insert((entry.last_access_time_ms, idx), idx);
                self.soft_refs.push(entry);
                // PERF: maintain the address->idx index for O(1) touch.
                //
                // gcd d3/n (`gcd-d2g-soft-touch-index-prefers-a-dead-row-at-a-
                // reused-address`): the LATEST registration at an address owns
                // it (`claim_soft_addr`). A second row at an address this
                // processor already indexes belongs to a new object on a block
                // whose previous `Reference` died and whose row is not pruned
                // yet (the generational concurrent sweep prunes a slice's rows
                // just after it drops the old-gen guard, and a direct old-gen
                // allocation can land in between). The old first-occurrence
                // rule sent that object's `get()` touches to the dead row. A
                // `Reference` constructor registers its object once, so the
                // "same object discovered twice" case the old `or_insert`
                // guarded does not arise. The stamped touch's proof
                // (`soft_touch_stamp`) needs every change of an address's row
                // to bump the epoch, so a steal does.
                if claim_soft_addr(&mut self.soft_ref_addr_index, &self.soft_refs, idx) {
                    self.bump_soft_touch_epoch();
                }
            }
            ReferenceType::Weak => self.weak_refs.push(entry),
            ReferenceType::Phantom => self.phantom_refs.push(entry),
            ReferenceType::Cleaner => self.cleaner_refs.push(entry),
            ReferenceType::Finalizer => self.finalizer_refs.push(entry),
            ReferenceType::Strong => {} // strong refs need no special treatment
        }
    }

    /// Record an access to a SoftReference's referent, updating the LRU
    /// timestamp used by [`Self::process_soft_refs`].
    ///
    /// The runtime should call this from the `SoftReference.get()` native
    /// after confirming the referent is still live. Without this call the
    /// LRU index forever sees `last_access_time_ms == 0`, so every
    /// SoftReference looks infinitely stale and gets cleared on the next
    /// major GC — defeating the whole point of memory-sensitive caches.
    ///
    /// Cost is O(1) for the address-index lookup plus O(log n) for the
    /// BTreeMap re-key; no global lock beyond the caller's existing
    /// `Mutex<ReferenceProcessor>` is taken.
    pub fn touch_soft_reference(&mut self, reference_obj: usize, now_ms: u64) {
        // SOFT-POLICY FIX (2026-07-26): record the mutator's clock BEFORE any
        // early return, so the processor learns the real time base even from a
        // touch that resolves no entry or does not advance the timestamp. This
        // is the only clock the processor ever sees that is guaranteed to be on
        // the same scale as `last_access_time_ms`; `process_soft_refs` falls
        // back to it when the collector-side caller passes `0` (see the field
        // doc on `last_observed_clock_ms`). Monotone by construction so a
        // non-monotonic `SystemTime` (NTP step-back) cannot rewind the policy.
        if now_ms > self.last_observed_clock_ms {
            self.last_observed_clock_ms = now_ms;
        }
        // PERF: O(1) address-index lookup replaces the former O(n) linear
        // scan over all soft refs. `SoftReference.get()` calls this on the
        // timer-advance path; for large cache populations the linear scan made
        // `get()` O(n). `soft_ref_addr_index` is kept in lock-step with
        // `soft_refs` positions. Between two rows at one address the later
        // registration wins (gcd d3/n; see `claim_soft_addr`).
        let idx = match self.soft_ref_addr_index.get(&reference_obj) {
            Some(&idx) => idx,
            None => return,
        };
        let entry = &mut self.soft_refs[idx];
        let old_key = (entry.last_access_time_ms, idx);
        // Only re-insert when the timestamp actually advances;
        // many caches `get()` faster than the timer resolution.
        if now_ms == entry.last_access_time_ms {
            return;
        }
        self.soft_ref_lru_index.remove(&old_key);
        entry.last_access_time_ms = now_ms;
        self.soft_ref_lru_index.insert((now_ms, idx), idx);
    }

    // -- Main entry point ---------------------------------------------------

    /// Process all reference types in HotSpot order.
    ///
    /// This is the legacy entry point: it has no access to a heap-graph
    /// tracing primitive, so it can only keep the *direct* finalizer referents
    /// (for soft+weak clearing) and the *direct* surviving soft referents (for
    /// weak clearing, per the JLS soft > weak ordering) alive — the deeper
    /// transitive cases need a caller-supplied tracer (see
    /// [`Self::process_references_with_finalizer_trace`] for the full
    /// transitive closure and a discussion of the residual gap).
    pub fn process_references(
        &mut self,
        is_marked: &dyn Fn(usize) -> bool,
        free_heap_mb: usize,
        current_time_ms: u64,
    ) -> ReferenceProcessingResult {
        // SECURITY FIX (V18): route the legacy entry point through the
        // finalizer-aware path with no transitive tracer. Passing `None`
        // still closes the common single-hop case (a weak/soft ref whose
        // referent *is* a finalizable object) by treating the direct
        // finalizer referents as live for Phases 1-2.
        self.process_references_with_finalizer_trace(is_marked, None, free_heap_mb, current_time_ms)
    }

    /// Process all reference types in HotSpot order, recomputing the
    /// finalizer-reachable closure before soft/weak refs are cleared and the
    /// soft-reachable closure before weak refs are cleared.
    ///
    /// SECURITY FIX (V18): spec-conformance. Previously, Phase 1 (soft) and
    /// Phase 2 (weak) cleared references using a single `is_marked` snapshot
    /// that did *not* include objects kept alive only because they are
    /// reachable from an about-to-be-finalized object. A finalizer that ran
    /// in this same cycle could therefore observe an already-nulled
    /// weak/soft referent. HotSpot avoids this by treating the
    /// finalizer-reachable set as live during weak/soft processing.
    ///
    /// SPEC FIX (JLS reachability ordering): Phase 2 (weak) additionally
    /// honours the *soft*-reachable closure. Per the JLS strength ordering
    /// (strong > soft > weak > phantom), a weak reference must not be cleared
    /// while its referent is still softly reachable — i.e. reachable from a
    /// soft referent that Phase 1 chose to retain. Previously a weak ref to an
    /// object kept alive only via a surviving soft reference was wrongly
    /// cleared. We now re-trace from the surviving (uncleared) soft referents
    /// after Phase 1 and fold that closure into the weak-clearing predicate.
    ///
    /// `trace_from`, when supplied, is the heap's "mark + trace from a set of
    /// roots" primitive: given the finalizer roots (the referents of finalizer
    /// references whose referent is currently unmarked, i.e. those about to be
    /// enqueued for finalization), it returns *all* addresses transitively
    /// reachable from them. This is the same `&dyn Fn` callback mechanism the
    /// module already uses for `is_marked` — the heap owns the field layout,
    /// so only it can walk the graph; reference.rs has no field-offset
    /// knowledge and intentionally does not gain a cross-module dependency on
    /// `gen_heap`/`g1`.
    ///
    /// When `trace_from` is `None` (legacy callers), we fall back to marking
    /// only the *direct* finalizer referents as live. RESIDUAL GAP: in that
    /// mode a weak/soft ref to an object reachable only *transitively* through
    /// a finalizable object (depth >= 2) can still be cleared prematurely.
    /// Closing that gap fully requires a caller to pass `trace_from`, which in
    /// turn requires the heap (`gen_heap.rs` / `g1.rs`) to expose its tracing
    /// primitive — an out-of-scope change for this fix.
    pub fn process_references_with_finalizer_trace(
        &mut self,
        is_marked: &dyn Fn(usize) -> bool,
        trace_from: Option<&dyn Fn(&[usize]) -> Vec<usize>>,
        free_heap_mb: usize,
        current_time_ms: u64,
    ) -> ReferenceProcessingResult {
        self.stats = ReferenceProcessingStats::default();
        // gen r5w3/unload7: this round's retirements (see
        // `ReferenceProcessingResult::retired`).
        self.round_retired.clear();

        self.stats.soft_refs_discovered = self.soft_refs.len();
        self.stats.weak_refs_discovered = self.weak_refs.len();
        self.stats.phantom_refs_discovered = self.phantom_refs.len();
        self.stats.finalizer_refs_discovered = self.finalizer_refs.len();

        // gc-common w6-d (2026-09-24): the finalizable objects THIS collection
        // found unreachable and resurrected inside the collection
        // (`note_resurrected_finalizables`). `is_marked` answers `true` for
        // them only because the collector marked them for `finalize()`; they
        // were not strongly or softly reachable, so soft and weak references
        // to them are cleared, as HotSpot clears them before finalization.
        // Empty unless the caller noted a set, so every other path is
        // unchanged. See `common-d-weak-refs-honour-finalizer-reachability-unlike-hotspot.md`.
        let resurrected = std::mem::take(&mut self.resurrected_this_cycle);
        let strongly_marked =
            |addr: usize| -> bool { is_marked(addr) && !resurrected.contains(&addr) };

        // SECURITY FIX (V18): Phase 0 — compute the finalizer-reachable
        // closure *before* any soft/weak ref is cleared.
        //
        // Roots are the referents of finalizer references that are not
        // already marked (those are exactly the objects Phase 3 will enqueue
        // for finalization and which must stay live for the finalizer to run)
        // and not already cleared/enqueued in a prior cycle.
        let finalizer_roots: Vec<usize> = self
            .finalizer_refs
            .iter()
            .filter(|e| !e.cleared && !e.enqueued && !is_marked(e.referent))
            .map(|e| e.referent)
            .collect();

        // Build the live closure. With a tracer we get the full transitive
        // set; without one we keep only the direct referents (single hop).
        //
        // `FxHashSet`, not `std::collections::HashSet` (gc-common w1-d): the
        // keys are heap addresses this process minted, so SipHash's DoS
        // resistance buys nothing, and both sets are rebuilt inside every STW
        // pause — `soft_live` from EVERY uncleared soft referent.
        let finalizer_live: FxHashSet<usize> = if finalizer_roots.is_empty() {
            FxHashSet::default()
        } else if let Some(trace) = trace_from {
            trace(&finalizer_roots).into_iter().collect()
        } else {
            finalizer_roots.iter().copied().collect()
        };

        // Augmented liveness predicate used for Phase 1 (soft): an object is
        // "live" if the collector already marked it OR it is reachable from a
        // to-be-finalized object. Phases 2-4 build on this with the additional
        // soft-reachable closure — see each phase's construction below.
        let soft_is_live =
            |addr: usize| -> bool { strongly_marked(addr) || finalizer_live.contains(&addr) };

        // SOFT-POLICY FIX (2026-07-26): "now" for the LRU comparison is the
        // later of the caller's clock and the mutator clock the processor has
        // observed through `touch_soft_reference`. The production call sites
        // (`vm/src/runtime/interpreter/gc_and_alloc.rs`: the post-GC path and
        // the G1/generational remark) pass a literal `0`, which made the
        // whole soft-ref policy unreachable; a caller with a real clock (ZGC's
        // backend, every test) already dominates and is unaffected. See the
        // `last_observed_clock_ms` field doc for the full analysis.
        let effective_now_ms = current_time_ms.max(self.last_observed_clock_ms);

        // Phase 1 (soft) honours the finalizer-reachable closure.
        //
        // gen r5w1/refs5: under `CRATONVM_SOFTREF_HOTSPOT_LRU` the LRU rule
        // runs on HotSpot's inputs — the clock of the previous round's end and
        // the free heap it was handed — expressed through the same two
        // arguments (`threshold = ms_per_mb * free`, `idle = now - access`).
        if self.hotspot_soft_lru {
            let free_mb = self.soft_free_mb_at_last_gc.unwrap_or(free_heap_mb);
            let clock = self.soft_gc_clock_ms;
            self.process_soft_refs(&soft_is_live, free_mb, clock);
        } else {
            self.process_soft_refs(&soft_is_live, free_heap_mb, effective_now_ms);
        }

        // SPEC FIX (JLS reachability ordering, strong > soft > weak > phantom):
        // weak references must NOT be cleared for a referent that is still
        // softly reachable. A soft referent that survived Phase 1 keeps
        // everything strongly reachable *from it* alive with respect to the
        // weaker reference levels; clearing a weak ref to such an object
        // (whether the object is itself a surviving soft referent, or is only
        // reachable through one) would violate the ordering — HotSpot keeps the
        // soft-reachable set live across weak processing.
        //
        // Roots are the referents of soft references that Phase 1 left
        // uncleared (and which are not already enqueued/cleared from a prior
        // cycle). With a tracer we fold the full transitive closure from those
        // roots into the weak-clearing liveness predicate; without one we fall
        // back to keeping the direct surviving soft referents alive (single
        // hop), which still protects a weak ref pointing straight at a retained
        // soft referent. The deeper, tracer-less transitive case is the same
        // documented residual gap as the finalizer closure above.
        //
        // gengc-mark 2026-09-20, REVERTED 2026-09-20 during the same round —
        // do NOT screen these roots with `soft_is_live`.
        //
        // The round-1 review added `&& soft_is_live(e.referent)` here, reasoning
        // that `!cleared && !enqueued` admits entries whose referent `is_marked`
        // says is garbage, so feeding them in leaves a `WeakReference` to a dead
        // object uncleared. The hazard it names is real (see below), but this
        // screen is the wrong instrument and it broke the soft contract:
        // `soft_is_live` is `is_marked(addr) || finalizer_live.contains(addr)`,
        // and a soft referent RETAINED BY THE LRU POLICY is unmarked *by
        // construction* — that is the entire reason soft references exist. So
        // the screen dropped exactly the policy-retained case, and a weak
        // reference to a still-softly-reachable object was cleared: a direct
        // violation of the JLS reachability ordering this block enforces
        // (strong > soft > weak > phantom), caught by tests 62, 63 and
        // `cleaner_not_fired_while_referent_softly_reachable`.
        //
        // "Retained by Phase 1" is what `!cleared && !enqueued` already means:
        // Phase 1 clears the entries the policy condemns, so an entry that
        // survives it is one whose referent is softly reachable and therefore
        // live with respect to every weaker reference level.
        //
        // The residual hazard is narrower than the reverted comment claimed and
        // is keyed on the wrong object: it is an entry whose `reference_obj` —
        // the `SoftReference` INSTANCE — is itself dead, so nothing can reach
        // the referent through it and the referent is not softly reachable at
        // all. Screening that case needs the reference object's liveness, not
        // the referent's. See
        // `docs/internal/gc/gengc-mark-dead-softref-object-still-roots-weak-closure-FIXED-20260923.md`.
        //
        // gengc-mark2 2026-09-20 — AND THAT SCREEN DOES NOT WORK EITHER, for
        // the same reason the first one did not. Read this before writing
        // `&& is_marked(e.reference_obj)` here.
        //
        // The gap page prescribes `reference_obj_is_live(e)` =
        // `is_marked(e.reference_obj) || finalizer_live.contains(..)`. All
        // three of the tests that page names as its own regression fence —
        // `weak_ref_to_surviving_soft_referent_not_cleared_no_tracer`,
        // `weak_ref_kept_alive_by_surviving_soft_referent_transitive`,
        // `cleaner_not_fired_while_referent_softly_reachable` — build their
        // scenario with `always_dead`. Under that predicate `is_marked` is
        // false for EVERYTHING, including the `SoftReference` instance at
        // 0x100, so the prescribed screen drops every soft survivor root and
        // fails all three, exactly as the referent-keyed version did. The
        // proposed fix breaks its own fence.
        //
        // The deeper point is that `is_marked` is not an oracle for "this
        // Reference object is dead". It is "the collector marked this address
        // in THIS cycle", and a caller is free to hand in a predicate for
        // which that is not the same question. The processor's own answer to
        // "is this row's Reference object gone" is `remove_collected`, which
        // uses the identical predicate and which every backend runs AFTER
        // reference processing, not before (`gc_and_alloc.rs`'s post-GC path;
        // `zgc.rs`'s `process_references` tail). Moving that call ahead of
        // Phase 1 would remove the hazard by construction — no screen here at
        // all — but it changes what a dead Reference's referent gets, for
        // three backends, in files this lane does not own.
        //
        // gengc-round3 (lane `refdriver`, 2026-09-21) — CLOSED, and NOT by a
        // screen here. Do not add one.
        //
        // The reordering above is what landed, in the narrow form that is
        // actually safe: `ReferenceProcessor::remove_collected_reference_objects`,
        // called by the generational driver immediately before
        // `process_references`. It drops soft/weak/phantom rows whose
        // `Reference` OBJECT did not survive, so a dead `SoftReference`'s row
        // never reaches the construction below and there is nothing left for a
        // predicate to screen. Being a reordering rather than a predicate, it
        // cannot suffer the `always_dead` problem: the three fence tests call
        // `process_references` directly and never run the pre-pass.
        //
        // It is NOT the whole of `remove_collected`, and the difference is
        // load-bearing: `finalizer_refs` and `cleaner_refs` are registered by
        // the VM with `reference_obj == referent == the finalizable object`
        // (`SharedVm::register_finalizable`, `Finalizer.register`), so pruning
        // THOSE on reference-object liveness would delete exactly the rows
        // whose job is to fire when the object dies. See that method's doc.
        //
        // So the filter below stays as it is — but now it is correct rather
        // than merely conservative, because every row it can see has a live
        // `Reference` object behind it.
        let soft_survivor_roots: Vec<usize> = self
            .soft_refs
            .iter()
            .filter(|e| !e.cleared && !e.enqueued)
            .map(|e| e.referent)
            .collect();
        let soft_live: FxHashSet<usize> = if soft_survivor_roots.is_empty() {
            FxHashSet::default()
        } else if let Some(trace) = trace_from {
            trace(&soft_survivor_roots).into_iter().collect()
        } else {
            soft_survivor_roots.iter().copied().collect()
        };

        // Phase 2 (weak) liveness folds in BOTH the finalizer-reachable closure
        // and the soft-reachable closure computed above.
        let weak_is_live = |addr: usize| -> bool {
            strongly_marked(addr) || finalizer_live.contains(&addr) || soft_live.contains(&addr)
        };
        self.process_weak_refs(&weak_is_live);

        // SPEC FIX (JLS/`java.lang.ref` reachability ordering, phases 3-4),
        // 2026-07-26. Phases 3 and 4 previously used the RAW `is_marked`
        // snapshot, with a comment claiming phantom reachability should be
        // "unaffected by the resurrection closure". That is the wrong way
        // round: the *whole reason* phantom is processed last is that an object
        // is phantom-reachable only once it is neither strongly, softly, nor
        // weakly reachable AND it has already been finalized. Using raw marking
        // meant:
        //
        //   * a `Cleaner`/`PhantomReference` fired for an object that Phase 1
        //     had just decided to RETAIN through a surviving SoftReference.
        //     For the JDK's most important cleaner — `DirectByteBuffer`'s —
        //     that means freeing the native allocation backing a buffer the
        //     application can still reach via `SoftReference.get()`:
        //     use-after-free, not merely a spec nit; and
        //   * `finalize()` was scheduled for an object that was still softly
        //     reachable, and a phantom was enqueued for an object whose
        //     `finalize()` had not run yet (so a resurrecting finalizer would
        //     race an already-delivered phantom notification).
        //
        // Both phases now fold in the closures Phases 1-2 already computed.
        // The split is deliberate:
        //
        //   * FINALIZER entries fold in `soft_live` ONLY. Soft reachability
        //     blocks finalization, but finalizer-reachability must NOT: when
        //     two mutually-reachable objects both override `finalize()`,
        //     HotSpot enqueues both in the same cycle. Folding `finalizer_live`
        //     here would make each block the other forever.
        //   * CLEANER and PHANTOM entries fold in BOTH. `Cleaner` is a
        //     `PhantomReference` subclass, so it takes the phantom rule, and
        //     "has been finalized" is part of that rule.
        //
        // This is strictly *less* clearing/enqueueing than before, so it can
        // only over-retain, never free early. Over-retention is bounded at one
        // collection for the finalizer closure: once Phase 3 flags an entry
        // `enqueued`, it stops contributing a root, and the next cycle fires
        // the phantom.
        let final_is_live = |addr: usize| -> bool { is_marked(addr) || soft_live.contains(&addr) };
        let phantom_is_live = |addr: usize| -> bool {
            is_marked(addr) || soft_live.contains(&addr) || finalizer_live.contains(&addr)
        };
        self.process_final_refs(&final_is_live, &phantom_is_live);
        self.process_phantom_refs(&phantom_is_live);

        // Build result
        let mut to_enqueue = Vec::new();
        let mut to_finalize = Vec::new();
        let mut cleaner_actions = Vec::new();

        // bc math-ec 0x4 ROOT-CAUSE FIX (2026-06-10): ONCE-ONLY emission.
        //
        // Previously this block RE-EMITTED, on EVERY GC forever: every pending
        // enqueue pair (`pending_queues` was read non-destructively), every
        // pending finalization (`finalization_queue.iter()`), and every
        // cleared cleaner. The VM-side consumer WRITES heap fields per emitted
        // action (queue head/size/next; referent null; cleaner submit). Those
        // raw registry addresses are only single-step-remapped per cycle, so
        // the moment an emitted Reference/queue object DIED, its recycled
        // address aliased an innocent live object on a later cycle — and the
        // per-GC re-emission then corrupted that object with valid-looking
        // writes every collection (FixedPointTest `Object(Some(0x4))` /
        // silent-null corruption — proven by hexdump + the NO_REFPROC 6/6
        // exclusion run; h2-testscript-segv-findings.md).
        //
        // Java semantics want each of these EXACTLY ONCE: Reference.enqueue is
        // one-shot, a referent is nulled once, a Cleaner runs once, finalize()
        // runs once. Drain/flag accordingly.

        // Gather enqueue pairs — DRAIN pending_queues (each pair was pushed
        // exactly once when its entry was first cleared).
        for (queue_addr, refs) in std::mem::take(&mut self.pending_queues) {
            for ref_obj in refs {
                to_enqueue.push((ref_obj, queue_addr));
            }
        }

        // DRAIN the finalization queue. (Also fixes the double-enqueue: the
        // interpreter additionally drains via `dequeue_for_finalization`
        // right after consuming this result — that loop now finds it empty.)
        to_finalize.extend(self.finalization_queue.drain(..));

        // Cleaner actions: emit each cleared cleaner ONCE.
        for entry in &mut self.cleaner_refs {
            if entry.cleared && !entry.action_emitted {
                entry.action_emitted = true;
                cleaner_actions.push(entry.reference_obj);
            }
        }
        // `jdk.internal.ref.Cleaner`s live in `phantom_refs` (see
        // `ReferenceEntry::runs_cleaner`) and become actions once their referent
        // is gone — `enqueued` is what `process_phantom_refs` sets for a phantom
        // whose referent died, and it is set exactly once.
        for entry in &mut self.phantom_refs {
            if entry.runs_cleaner && entry.enqueued && !entry.action_emitted {
                entry.action_emitted = true;
                cleaner_actions.push(entry.reference_obj);
            }
        }
        if dm_dbg_enabled() {
            use std::sync::atomic::Ordering::Relaxed;
            // A `runs_cleaner` phantom that is NOT yet enqueued is one whose
            // referent is STILL REACHABLE -- i.e. a direct buffer something in
            // Java is still holding. That count answers lead 3 directly.
            let retained = self
                .phantom_refs
                .iter()
                .filter(|e| e.runs_cleaner && !e.enqueued)
                .count();
            let emitted = cleaner_actions.len();
            let cum = DM_PC_EMITTED.fetch_add(emitted as u64, Relaxed) + emitted as u64;
            let g = DM_REFPROC_ROUNDS.fetch_add(1, Relaxed) + 1;
            if emitted > 0 || g % 50 == 0 {
                eprintln!(
                    "[dm] refproc round={} emitted={} cum_emitted={} discovered={} retained_live={} phantom_total={}",
                    g,
                    emitted,
                    cum,
                    DM_PC_DISCOVERED.load(Relaxed),
                    retained,
                    self.phantom_refs.len()
                );
            }
        }
        self.stats.cleaner_refs_processed = cleaner_actions.len();

        if self.hotspot_soft_lru {
            self.commit_hotspot_soft_clock(free_heap_mb, effective_now_ms);
        }

        ReferenceProcessingResult {
            to_enqueue,
            to_finalize,
            cleaner_actions,
            retired: std::mem::take(&mut self.round_retired),
            stats: self.stats.clone(),
        }
    }

    // -- Phase 1: SoftReferences -------------------------------------------

    fn process_soft_refs(
        &mut self,
        is_marked: &dyn Fn(usize) -> bool,
        free_heap_mb: usize,
        current_time_ms: u64,
    ) {
        let threshold_ms = self
            .soft_ref_lru_policy_ms_per_mb
            .saturating_mul(free_heap_mb as u64);

        // ---- Entries condemned BEFORE the mark ---------------------------
        //
        // `condemn_idle_soft_refs` already applied the LRU rule to these, on
        // the pre-collection heap headroom, and the VM nulled their referent
        // slot so the marker could not keep the referent alive through the
        // `SoftReference` itself. They are settled here rather than by the
        // range scan below for two independent reasons:
        //
        // * re-deriving the verdict now would use POST-collection headroom,
        //   which is larger, which makes `threshold_ms` larger — so the
        //   re-derivation can only ever disagree in the direction of "keep".
        //   Keeping an entry whose referent this collection has already
        //   reclaimed leaves a dead address inside an entry that still reads
        //   as active;
        // * `candidate_indices` below is derived from that same larger
        //   threshold, so a condemned entry can fall outside the range and
        //   never be visited at all.
        //
        // `is_marked` still has the last word. A condemned referent that was
        // reachable on a strong path was marked anyway and is kept; the
        // post-GC restore pass writes its slot back
        // (`soft_pre_nulled_active_pairs`).
        if !self.soft_pre_nulled.is_empty() {
            // (index, reference_obj, queue_addr, last_access_time_ms) — read
            // out first so the mutation loop is not holding a borrow of
            // `self.soft_refs` while it touches `pending_queues` / the LRU
            // index, which are disjoint fields the borrow checker cannot see
            // through `self`.
            let mut condemned: Vec<(usize, usize, Option<usize>, u64)> = Vec::new();
            for (idx, entry) in self.soft_refs.iter().enumerate() {
                if entry.cleared
                    || entry.enqueued
                    || !self.soft_pre_nulled.contains(&entry.reference_obj)
                    || is_marked(entry.referent)
                {
                    continue;
                }
                condemned.push((
                    idx,
                    entry.reference_obj,
                    entry.queue_addr,
                    entry.last_access_time_ms,
                ));
            }
            for (idx, reference_obj, queue_addr, last_access) in condemned {
                // Active until now (the filter above): a retirement.
                self.round_retired.push(reference_obj);
                self.soft_refs[idx].cleared = true;
                self.stats.soft_refs_cleared += 1;
                if let Some(q) = queue_addr {
                    self.pending_queues
                        .entry(q)
                        .or_default()
                        .push(reference_obj);
                    self.soft_refs[idx].enqueued = true;
                }
                // Same LRU-index hygiene the range scan below applies when it
                // clears an entry: a cleared soft ref is never a candidate
                // again, so its key would make every later range scan re-walk
                // it.
                self.soft_ref_lru_index.remove(&(last_access, idx));
            }
        }

        // Use the BTreeMap index to efficiently find soft refs whose
        // last_access_time is old enough to exceed the idle threshold.
        // Only entries with last_access_time <= cutoff can have idle_ms > threshold_ms.
        let cutoff = current_time_ms.saturating_sub(threshold_ms);

        // Collect indices of candidates from the BTreeMap range [0..=cutoff].
        let candidate_indices: Vec<usize> = self
            .soft_ref_lru_index
            .range(..=(cutoff, usize::MAX))
            .map(|(_, &idx)| idx)
            .collect();

        for idx in candidate_indices {
            // MEDIUM FIX (LRU-index leak): collect the LRU keys to drop in this
            // pass and remove them from the BTreeMap *after* the `entry` borrow
            // of `self.soft_refs[idx]` is released — `soft_ref_lru_index` and
            // `soft_refs` are disjoint fields, but the borrow checker can't see
            // that across the index's `&mut self` method call while `entry`
            // (a borrow of `self.soft_refs`) is live.
            let stale_lru_key: Option<(u64, usize)>;
            {
                let entry = &mut self.soft_refs[idx];
                if entry.cleared {
                    // A previously-cleared entry whose key the range query still
                    // returned. Its referent is dead and it will never become a
                    // clear candidate again, so drop its key — a defensive sweep
                    // in case some other path set `cleared` without pruning.
                    stale_lru_key = Some((entry.last_access_time_ms, idx));
                } else if is_marked(entry.referent) {
                    continue; // referent still live; keep its LRU key intact
                } else {
                    // Double-check LRU policy (the BTreeMap range is an
                    // approximation since we key on insertion time; re-verify the
                    // exact idle window).
                    let idle_ms = current_time_ms.saturating_sub(entry.last_access_time_ms);
                    if idle_ms > threshold_ms {
                        // Not cleared (above); active iff not enqueued either.
                        if !entry.enqueued {
                            self.round_retired.push(entry.reference_obj);
                        }
                        entry.cleared = true;
                        self.stats.soft_refs_cleared += 1;
                        if let Some(q) = entry.queue_addr {
                            self.pending_queues
                                .entry(q)
                                .or_default()
                                .push(entry.reference_obj);
                            entry.enqueued = true;
                        }
                        // A cleared/enqueued soft ref will never be a clear
                        // candidate again (the `entry.cleared` guard skips it).
                        // Leaving its `(timestamp, idx)` key in the BTreeMap
                        // means the range scan in every subsequent GC re-walks
                        // it (and `touch_soft_reference` would scan it), so the
                        // index grew unbounded relative to live soft refs. Drop
                        // its key so the LRU index stays in sync with the *live*
                        // (uncleared) soft refs. Surviving entries keep their
                        // keys untouched, so LRU ordering for them is preserved.
                        // The key must use the entry's current
                        // `last_access_time_ms` (what `touch_soft_reference`
                        // last inserted).
                        stale_lru_key = Some((entry.last_access_time_ms, idx));
                    } else {
                        // Not idle enough this cycle; keep its LRU key.
                        stale_lru_key = None;
                    }
                }
            }
            if let Some(key) = stale_lru_key {
                self.soft_ref_lru_index.remove(&key);
            }
        }
    }

    // -- Phase 2: WeakReferences -------------------------------------------

    fn process_weak_refs(&mut self, is_marked: &dyn Fn(usize) -> bool) {
        let dbg = gc_flags().dbg_watchref;
        for entry in &mut self.weak_refs {
            if entry.cleared {
                continue;
            }
            if is_marked(entry.referent) {
                if dbg {
                    eprintln!(
                        "[watchref] weak KEEP ref_obj=0x{:x} referent=0x{:x}",
                        entry.reference_obj, entry.referent
                    );
                }
                continue;
            }
            if dbg {
                eprintln!(
                    "[watchref] weak CLEAR ref_obj=0x{:x} referent=0x{:x}",
                    entry.reference_obj, entry.referent
                );
            }
            // gen r5w3/unload7: not cleared (above); active iff not enqueued.
            if !entry.enqueued {
                self.round_retired.push(entry.reference_obj);
            }
            entry.cleared = true;
            self.stats.weak_refs_cleared += 1;
            if let Some(q) = entry.queue_addr {
                self.pending_queues
                    .entry(q)
                    .or_default()
                    .push(entry.reference_obj);
                entry.enqueued = true;
            }
        }
    }

    // -- Phase 3: Cleaner / FinalReferences --------------------------------

    /// `finalizer_is_live` gates `finalize()` scheduling (raw marking + the
    /// soft-reachable closure); `cleaner_is_live` gates `Cleaner` actions
    /// (raw marking + soft-reachable + finalizer-reachable, because `Cleaner`
    /// is a `PhantomReference`). See the call site for why the two differ.
    fn process_final_refs(
        &mut self,
        finalizer_is_live: &dyn Fn(usize) -> bool,
        cleaner_is_live: &dyn Fn(usize) -> bool,
    ) {
        // Cleaners
        for entry in &mut self.cleaner_refs {
            if entry.cleared {
                continue;
            }
            if cleaner_is_live(entry.referent) {
                continue;
            }
            entry.cleared = true;
            // Cleaners don't use ReferenceQueue -- they run an action directly
        }

        // Finalizers: enqueue referent for finalization but do NOT clear the
        // referent yet (the finalizer thread needs to reach it).
        for entry in &mut self.finalizer_refs {
            if entry.enqueued || entry.cleared {
                continue;
            }
            if finalizer_is_live(entry.referent) {
                continue;
            }
            entry.enqueued = true;
            // The row stopped being active (`finalizer_active`).
            self.finalizer_active_stale = true;
            self.stats.finalizer_refs_enqueued += 1;
            self.finalization_queue.push_back(entry.referent);
            if let Some(q) = entry.queue_addr {
                self.pending_queues
                    .entry(q)
                    .or_default()
                    .push(entry.reference_obj);
            }
        }
    }

    // -- Phase 4: PhantomReferences ----------------------------------------

    fn process_phantom_refs(&mut self, is_marked: &dyn Fn(usize) -> bool) {
        for entry in &mut self.phantom_refs {
            if entry.enqueued {
                continue;
            }
            if is_marked(entry.referent) {
                continue;
            }
            // JDK 9+ CLEARS a phantom reference as it is enqueued (JDK 8 did
            // not). The processor records the verdict; the slot is null
            // because the VM's pre-collection pass nulled it and never restores
            // an enqueued phantom (`weak_phantom_active_pairs` excludes it), and
            // the generational remark nulls it itself
            // (`gen_remark_process_references`). gen r5w1/refs5: this comment
            // used to say "Java 9+: referent is NOT cleared", the JDK 8 rule.
            //
            // gen r5w3/unload7: not enqueued (above); active iff not cleared.
            // A running `Cleaner` retires here too: its row leaves the active
            // set, which is what the generational remark's null pass keys on.
            if !entry.cleared {
                self.round_retired.push(entry.reference_obj);
            }
            entry.enqueued = true;
            // A `jdk.internal.ref.Cleaner` is never enqueued — its queue is a
            // private `dummyQueue` with no consumer. It is RUN, in
            // `process_references`'s gather step. See
            // `ReferenceEntry::runs_cleaner`.
            if entry.runs_cleaner {
                continue;
            }
            // gen r5w1/refs5: counted AFTER the cleaner arm. A running
            // `Cleaner` is `cleaner_refs_processed` (the gather step); counting
            // it here too made one cleaner two events
            // (`gengc-r4-mark-counters-and-comments-that-lie`).
            self.stats.phantom_refs_enqueued += 1;
            if let Some(q) = entry.queue_addr {
                self.pending_queues
                    .entry(q)
                    .or_default()
                    .push(entry.reference_obj);
            }
        }
    }

    // -- Post-GC maintenance ------------------------------------------------

    /// Relocate addresses after a compacting / copying GC.
    ///
    /// Validates that all target addresses in the pointer map are non-null
    /// and within a plausible heap range (non-zero) to prevent corruption
    /// from a bad relocation map.
    pub fn update_after_gc(&mut self, pointer_map: &cratonvm_types::PointerMap) {
        // PERF (gc-common w2-f): nothing below does anything for an empty map
        // — every relocation is a lookup miss and every rebuild reproduces the
        // table it replaces — yet each rebuild allocated and re-hashed a whole
        // table, inside the pause. Same for a map that moved none of this
        // processor's KEYS (the identity entries a non-moving cycle emits for
        // watched survivors): see `moved_any` and the `*_moved` flags below.
        if pointer_map.is_empty() {
            return;
        }
        // gc-common w6-d: objects may have moved, including one WITHOUT a row
        // onto the address of a stale row, so no `SoftReference.timestamp`
        // stamp minted before this point may short-cut a touch any more. See
        // `soft_touch_stamp`. Unconditional on a non-empty map: a moving
        // collection is rare next to `get()`, and each stamp costs one locked
        // touch to renew.
        self.bump_soft_touch_epoch();
        /// Relocate every row; answer whether any `reference_obj` (the key of
        /// every index over these rows) actually changed.
        fn relocate_list(list: &mut [ReferenceEntry], map: &cratonvm_types::PointerMap) -> bool {
            let mut moved = false;
            for e in list.iter_mut() {
                if let Some(&new_addr) = map.get(&e.reference_obj) {
                    if new_addr != 0 {
                        moved |= new_addr != e.reference_obj;
                        e.reference_obj = new_addr;
                    } else {
                        tracing::warn!(
                            "update_after_gc: null target for reference_obj 0x{:x}",
                            e.reference_obj
                        );
                    }
                }
                if let Some(&new_addr) = map.get(&e.referent) {
                    if new_addr != 0 {
                        e.referent = new_addr;
                    } else {
                        tracing::warn!(
                            "update_after_gc: null target for referent 0x{:x}",
                            e.referent
                        );
                    }
                }
                if let Some(q) = e.queue_addr {
                    if let Some(&new_q) = map.get(&q) {
                        if new_q != 0 {
                            e.queue_addr = Some(new_q);
                        } else {
                            tracing::warn!("update_after_gc: null target for queue 0x{:x}", q);
                        }
                    }
                }
            }
            moved
        }
        /// Would a rebuild of an address-keyed table change any key?
        fn moved_any<'a>(
            mut keys: impl Iterator<Item = &'a usize>,
            map: &cratonvm_types::PointerMap,
        ) -> bool {
            keys.any(|k| matches!(map.get(k), Some(&new) if new != 0 && new != *k))
        }

        let soft_moved = relocate_list(&mut self.soft_refs, pointer_map);
        let weak_moved = relocate_list(&mut self.weak_refs, pointer_map);
        let phantom_moved = relocate_list(&mut self.phantom_refs, pointer_map);
        relocate_list(&mut self.cleaner_refs, pointer_map);
        if relocate_list(&mut self.finalizer_refs, pointer_map) {
            // `finalizer_active` is keyed on these addresses.
            self.finalizer_active_stale = true;
        }
        // Row addresses changed; the application retirement index is keyed on
        // them. Rebuilt lazily on its next use.
        if soft_moved || weak_moved || phantom_moved {
            self.app_row_index_stale = true;
        }

        // gc-common w1-d (2026-09-23): `pending_queues` is address-keyed on
        // BOTH sides (queue -> [reference_obj]). It used to be empty between
        // collections by construction — every push happened inside
        // `process_references`, which drains it in the same call — so nothing
        // relocated it. `clear_after_refused_restore` now defers an enqueue to
        // the NEXT processing round, which means a pending pair can outlive a
        // collection, and a moving one would leave it naming the pre-move
        // addresses. Rebuilt rather than rewritten in place, for the reason the
        // stamp tables below are: a key can be another key's target.
        if !self.pending_queues.is_empty() {
            let relocate = |addr: usize| -> usize {
                match pointer_map.get(&addr) {
                    Some(&new) if new != 0 => new,
                    _ => addr,
                }
            };
            let old = std::mem::take(&mut self.pending_queues);
            for (queue, refs) in old {
                self.pending_queues
                    .entry(relocate(queue))
                    .or_default()
                    .extend(refs.into_iter().map(relocate));
            }
        }

        // PERF: relocation rewrote `reference_obj` on the soft refs, so the
        // address->idx index is stale. Positions in `soft_refs` did not
        // change (relocate is in-place), so rebuild the keys from the new
        // addresses. The later registration wins an address, as in
        // `discover_reference` (gcd d3/n, `claim_soft_addr`). (It runs after
        // any collection whose pointer map moved a soft row — a moving young
        // collection that relocated a `SoftReference` counts, not only a compacting one; gen r5w1/refs5
        // corrected "only runs after a compacting GC".) Only when a soft row actually moved: the
        // index is in step with the rows between collections (every mutation
        // site maintains it), so with no moved key the rebuild reproduces it.
        if soft_moved {
            self.soft_ref_addr_index.clear();
            for idx in 0..self.soft_refs.len() {
                claim_soft_addr(&mut self.soft_ref_addr_index, &self.soft_refs, idx);
            }
        }

        // The stamp table is address-keyed too, and a compacting collector
        // moves the keys. Rebuilt rather than relocated in place: two entries
        // can swap addresses across one slide (a survivor slides onto the base
        // a dead object vacated), and an in-place rewrite of a map whose keys
        // are also its targets picks whichever insert lands second. Skipped
        // when no key moves — the rebuild would reproduce the table.
        let rekey_identity = moved_any(self.identity_stamps.keys(), pointer_map);
        if rekey_identity {
            let mut moved: FxHashMap<usize, i32> =
                FxHashMap::with_capacity_and_hasher(self.identity_stamps.len(), Default::default());
            for (addr, hash) in self.identity_stamps.iter() {
                let now = match pointer_map.get(addr) {
                    Some(&new) if new != 0 => new,
                    _ => *addr,
                };
                moved.insert(now, *hash);
            }
            self.identity_stamps = moved;
        }

        // Same treatment, same reason, for the referent-class table: its keys
        // are `reference_obj` addresses and a slide moves them.
        let rekey_referent_class = moved_any(self.referent_class_stamps.keys(), pointer_map);
        if rekey_referent_class {
            let mut moved: FxHashMap<usize, u32> = FxHashMap::with_capacity_and_hasher(
                self.referent_class_stamps.len(),
                Default::default(),
            );
            for (addr, cid) in self.referent_class_stamps.iter() {
                let now = match pointer_map.get(addr) {
                    Some(&new) if new != 0 => new,
                    _ => *addr,
                };
                moved.insert(now, *cid);
            }
            self.referent_class_stamps = moved;
        }

        // gengc-mark 2026-09-20 — the condemned-soft set is address-keyed too.
        //
        // `soft_pre_nulled` holds `reference_obj` addresses, and `relocate_list`
        // above has just rewritten those very addresses on the entries. Left
        // unrelocated the set says nothing true about the post-collection heap,
        // and the field doc's "a cycle that skips the pre-collection pass sees
        // the previous cycle's set, and that is deliberately harmless" argument
        // stops holding the moment a collector MOVES anything: a stale address
        // can collide with the NEW address of a different soft entry (a
        // survivor slides onto the base a dead object vacated — the identity
        // stamp rebuild ten lines up exists for exactly this hazard), and that
        // entry then takes the condemned branch in `process_soft_refs`, which
        // skips the idle check entirely. The result is a `SoftReference`
        // cleared without memory pressure, which is the one thing the soft
        // contract forbids.
        //
        // Rebuilt rather than rewritten in place, for the same reason as the
        // stamp tables: the keys are also the targets.
        if !self.soft_pre_nulled.is_empty() {
            let mut moved: FxHashSet<usize> =
                FxHashSet::with_capacity_and_hasher(self.soft_pre_nulled.len(), Default::default());
            for addr in self.soft_pre_nulled.iter() {
                let now = match pointer_map.get(addr) {
                    Some(&new) if new != 0 => new,
                    _ => *addr,
                };
                moved.insert(now);
            }
            self.soft_pre_nulled = moved;
        }

        // Relocate finalization queue entries
        for addr in &mut self.finalization_queue {
            if let Some(&new_addr) = pointer_map.get(addr) {
                if new_addr != 0 {
                    *addr = new_addr;
                } else {
                    tracing::warn!(
                        "update_after_gc: null target for finalizer queue entry 0x{:x}",
                        *addr
                    );
                }
            }
        }
    }

    /// Drop weak/phantom entries whose `Reference` object no longer *looks
    /// like* a `Reference` — i.e. its memory has been reclaimed and recycled.
    ///
    /// HIB-WEAKREF-RECYCLE.1 (2026-07-31). [`Self::remove_collected`]'s
    /// survivor predicate ultimately bottoms out in `is_addr_live`, which for
    /// an old-generation address is a pure range check: freed old-gen memory
    /// still answers "live". A weak/phantom entry whose `Reference` object was
    /// reclaimed by an old-gen sweep therefore survives every prune, and both
    /// the pre-GC null pass and the post-GC restore pass keep writing slot 0 of
    /// whatever now occupies that address, forever. Declining the write at each
    /// of those sites stops the corruption but leaves the dangling entry (and
    /// its per-cycle cost) in place; this prunes it.
    ///
    /// Deliberately scoped to `weak_refs`/`phantom_refs`: their `reference_obj`
    /// is always a `java.lang.ref.Reference` subclass, so "has >= 2 instance
    /// fields" is a sound shape test. `finalizer_refs`/`cleaner_refs` are NOT
    /// safe to test this way — a finalizable object is an arbitrary class and
    /// may legitimately declare fewer than two fields.
    pub fn retain_shaped_weak_phantom(&mut self, still_a_reference: &dyn Fn(usize) -> bool) {
        let before = self.weak_refs.len() + self.phantom_refs.len();
        // gcd d9/e: a recycled `Reference`'s deferred enqueue pair goes with
        // its row (`drop_pending_enqueues_of`).
        let track_pending = !self.pending_queues.is_empty();
        let mut dropped_enqueued: FxHashSet<usize> = FxHashSet::default();
        let mut keep = |e: &ReferenceEntry| -> bool {
            let live = still_a_reference(e.reference_obj);
            if !live && track_pending && e.enqueued {
                dropped_enqueued.insert(e.reference_obj);
            }
            live
        };
        self.weak_refs.retain(&mut keep);
        self.phantom_refs.retain(&mut keep);
        self.drop_pending_enqueues_of(dropped_enqueued);
        if self.weak_refs.len() + self.phantom_refs.len() != before {
            self.app_row_index_stale = true;
        }
    }

    /// Record the identity hash `reference_obj` carries, so every later write
    /// through its address can prove the object is still the one discovered.
    /// See [`Self::identity_stamps`].
    ///
    /// Called by the VM immediately after a `discover_*`, because minting the
    /// hash needs the heap and this crate has only addresses.
    pub fn stamp_reference(&mut self, reference_obj: usize, identity_hash: i32) {
        if identity_hash != 0 {
            self.identity_stamps.insert(reference_obj, identity_hash);
        }
    }

    /// Record the CLASS of the referent this entry currently points at, so the
    /// post-GC restore pass can refuse to install a stranger. See
    /// [`Self::referent_class_stamps`].
    ///
    /// Called by the VM from the pre-GC null pass, which is the one place that
    /// holds the referent and the heap at the same time.
    ///
    /// That pass calls this for EXACTLY the entries whose slot 0 held a
    /// non-null referent and which it then nulled, so the call doubles as the
    /// record of "this cycle nulled this slot" ([`Self::pre_gc_nulled`]) — the
    /// fact the post-GC restore pass needs and did not have. The membership is
    /// recorded whatever the class id, including `0` (`java.lang.Object`, the
    /// commonest referent class of all), which only the class TABLE declines.
    pub fn stamp_referent_class(&mut self, reference_obj: usize, class_id: u32) {
        self.pre_gc_nulled.insert(reference_obj);
        if class_id != 0 {
            self.referent_class_stamps.insert(reference_obj, class_id);
        }
    }

    /// Did this cycle's pre-collection pass null `reference_obj`'s referent
    /// slot? The post-GC restore pass writes a referent back ONLY when it did:
    /// a slot that was already null when the pass reached it was nulled by the
    /// application (`Reference.clear()`), and writing the referent back
    /// resurrects a reference the program cleared. See
    /// [`Self::pre_gc_nulled`].
    ///
    /// `reference_obj` is the PRE-collection address, the key the pass used;
    /// the restore pass runs before [`Self::update_after_gc`], so its entries
    /// still hold that address.
    pub fn was_nulled_by_pre_gc_pass(&self, reference_obj: usize) -> bool {
        self.pre_gc_nulled.contains(&reference_obj)
    }

    /// Retire every weak/soft entry the pre-collection pass examined but did
    /// not null, returning how many it retired.
    ///
    /// # Why (gc-common w1-d, 2026-09-23)
    ///
    /// The pass nulls a slot only when it holds a referent and the object at
    /// the address is still the discovered `Reference`. So an examined entry
    /// left alone is one of two things, and neither may ever be cleared and
    /// ENQUEUED by the collector later:
    ///
    /// * the application cleared it — `Reference.clear()` is a bare slot store
    ///   (`native_ref_clear`) that never reaches this registry. HotSpot never
    ///   discovers a `Reference` whose referent is null, so it is never
    ///   enqueued; here the row stayed active and was cleared-and-enqueued the
    ///   moment its OLD referent died, delivering a reference the program had
    ///   already cancelled — the same ghost `mark_manually_enqueued` exists to
    ///   stop for `enqueue()`;
    /// * the address was recycled — the shape/identity guards declined the
    ///   write, and the row is garbage whose every later write would land on
    ///   the new occupant.
    ///
    /// Retiring marks the entry `cleared` + `enqueued` + `clear_emitted`
    /// without queueing anything, exactly as [`Self::mark_manually_enqueued`]
    /// does, so every phase's re-entry guard skips it and no slot write is ever
    /// emitted for it. It also drops the row from
    /// [`Self::pending_reference_object_addresses`], so a cancelled reference
    /// stops being a candidate for the frame rescue that list feeds.
    ///
    /// Phantom entries are never examined (see [`Self::pre_gc_examined`]), so
    /// this never touches one.
    ///
    /// Call it after the collection and BEFORE `process_references`, while the
    /// entries still hold pre-collection addresses. Idempotent.
    pub fn retire_entries_the_pre_gc_pass_found_cleared(&mut self) -> usize {
        if self.pre_gc_examined.is_empty() {
            return 0;
        }
        let mut retired = 0usize;
        for e in self.weak_refs.iter_mut() {
            if !e.cleared
                && !e.enqueued
                && self.pre_gc_examined.contains(&e.reference_obj)
                && !self.pre_gc_nulled.contains(&e.reference_obj)
            {
                e.cleared = true;
                e.enqueued = true;
                e.clear_emitted = true;
                retired += 1;
            }
        }
        let mut stale_lru_keys: Vec<(u64, usize)> = Vec::new();
        for (idx, e) in self.soft_refs.iter_mut().enumerate() {
            if !e.cleared
                && !e.enqueued
                && self.pre_gc_examined.contains(&e.reference_obj)
                && !self.pre_gc_nulled.contains(&e.reference_obj)
            {
                e.cleared = true;
                e.enqueued = true;
                e.clear_emitted = true;
                stale_lru_keys.push((e.last_access_time_ms, idx));
                retired += 1;
            }
        }
        for key in stale_lru_keys {
            self.soft_ref_lru_index.remove(&key);
        }
        retired
    }

    /// The post-GC restore pass nulled this entry's slot before the mark and
    /// then could not write the referent back — it refused (the address could
    /// not be proven to still be the referent) or the referent did not survive.
    /// Turn that into what the application can already observe: a reference
    /// the collector CLEARED. Returns whether an entry was converted.
    ///
    /// # Why (gc-common w1-d, 2026-09-23)
    ///
    /// Before this, such an entry stayed ACTIVE with a null slot and a
    /// registry `referent` naming an address the pass had just declined to
    /// trust. Two outcomes, both wrong: the next cycle's restore wrote that
    /// untrusted address back (the "retry" installed whatever now lived
    /// there), or — with restores now gated on
    /// [`Self::was_nulled_by_pre_gc_pass`] — the row sat active forever and a
    /// `WeakReference` whose `get()` already answered `null` was never
    /// enqueued, so a `WeakHashMap` never expunged it.
    ///
    /// Weak and soft only. A PHANTOM is left exactly as it was: its slot is
    /// invisible to `get()`, and enqueueing one on a refusal would run a
    /// `Cleaner` for a referent that may well be alive — freeing native memory
    /// under a live `DirectByteBuffer`.
    ///
    /// The enqueue is DEFERRED to the next `process_references` round (this
    /// round's `to_enqueue` has already been consumed); `update_after_gc`
    /// relocates the pending pair in between.
    pub fn clear_after_refused_restore(&mut self, reference_obj: usize) -> bool {
        if let Some(e) = self
            .weak_refs
            .iter_mut()
            .find(|e| e.reference_obj == reference_obj && !e.cleared && !e.enqueued)
        {
            e.cleared = true;
            e.clear_emitted = true;
            self.stats.weak_refs_cleared += 1;
            if let Some(q) = e.queue_addr {
                e.enqueued = true;
                self.pending_queues.entry(q).or_default().push(reference_obj);
            }
            return true;
        }
        let soft_idx = self.soft_ref_addr_index.get(&reference_obj).copied();
        if let Some(idx) = soft_idx {
            let e = &mut self.soft_refs[idx];
            if !e.cleared && !e.enqueued {
                e.cleared = true;
                e.clear_emitted = true;
                self.stats.soft_refs_cleared += 1;
                let key = (e.last_access_time_ms, idx);
                if let Some(q) = e.queue_addr {
                    e.enqueued = true;
                    self.pending_queues.entry(q).or_default().push(reference_obj);
                }
                self.soft_ref_lru_index.remove(&key);
                return true;
            }
        }
        false
    }

    /// Close out one collection's pre/post protocol: forget which slots the
    /// pre-collection pass nulled, which entries it examined, and which soft
    /// entries it condemned.
    ///
    /// Called by the VM at the end of every post-GC reference pass, and by the
    /// G1 remark driver after it consumes a last-ditch condemnation. Every one
    /// of these sets describes ONE collection; a leftover set is read by the
    /// next pass that runs without a pre-collection pass of its own — G1's
    /// final remark — as if it were current. See [`Self::soft_pre_nulled`].
    pub fn finish_pre_gc_cycle(&mut self) {
        self.soft_pre_nulled.clear();
        self.pre_gc_nulled.clear();
        self.pre_gc_examined.clear();
        self.resurrected_this_cycle.clear();
    }

    /// Record, for the processing round of the collection that just ran, the
    /// PRE-collection addresses of the finalizable objects that collection
    /// found unreachable and resurrected (its dead-finalizer list, mapped
    /// back to the addresses this processor's rows still hold).
    ///
    /// # Why (gc-common w6-d, 2026-09-24)
    ///
    /// Every backend's `collect_garbage_with_finalizers` marks a dead
    /// finalizable object INSIDE the collection so `finalize()` can run, so by
    /// the time the rows are processed `is_marked` is `true` for it and a
    /// `WeakReference` to it was kept — and, once `finalize()` resurrected it,
    /// kept for good. HotSpot processes Soft and Weak BEFORE Final: an object
    /// reachable only through its `FinalReference` has its weak references
    /// cleared first (the `java.lang.ref` package documentation: weak
    /// references are cleared "at the same time" the object is declared
    /// finalizable). The resurrected set is exactly the set of objects the
    /// collector proved not strongly (and, since policy-kept soft referents
    /// are traced strongly, not softly) reachable, so the next
    /// [`Self::process_references_with_finalizer_trace`] treats them as
    /// unmarked for soft and weak processing only. Final and phantom
    /// processing still see them marked: the phantom rule requires
    /// finalization first, and their `Finalizer` rows are flagged by
    /// [`Self::mark_finalizer_enqueued`].
    ///
    /// This closes the depth-1 case (a reference to the finalizable object
    /// itself) on every collector. A reference to an object reachable only
    /// THROUGH the finalizable object needs the collector's pre-resurrection
    /// mark verdict, which only the collector has; see
    /// `docs/known-issues/gc/common-d-weak-refs-honour-finalizer-reachability-unlike-hotspot.md`.
    ///
    /// # Depth 2 takes the same call (gc-common w7-d)
    ///
    /// The rule is not specific to finalizable objects: any address noted
    /// here is "marked only by the resurrection pass". So once a collector
    /// reports its whole resurrection CLOSURE (every object its resurrection
    /// drain newly marked, as pre-collection addresses), the VM passes that
    /// superset here unchanged and a weak reference to a child reachable only
    /// through the finalizable object is cleared too (test W7D-1). The
    /// collector's contract is what keeps this sound: the drain marks only
    /// what the strong (and policy-kept soft) closure had NOT marked, so a
    /// shared, strongly reachable object is never in the set.
    /// `docs/internal/gc-common-round-20260923/handoff-w7d-collectors-report-the-resurrection-closure.md`.
    ///
    /// Consumed by the next processing round and cleared by
    /// [`Self::finish_pre_gc_cycle`], so it never spans two collections.
    pub fn note_resurrected_finalizables(&mut self, pre_gc_addrs: &[usize]) {
        self.resurrected_this_cycle
            .extend(pre_gc_addrs.iter().copied().filter(|&a| a != 0));
    }

    /// The referent-class stamp for `reference_obj` (see
    /// [`Self::referent_class_stamps`]), or `None` when it has none.
    ///
    /// gc-common w6-d (2026-09-24): replaces `referent_class_stamps_snapshot`,
    /// which CLONED the whole table once per collection so the post-GC pass
    /// could consult it while also mutating this processor. That pass holds
    /// the processor's guard throughout and makes each check between its
    /// mutating calls, so a direct lookup reads exactly what the clone held:
    /// nothing between the old clone point and the last check writes this
    /// table (`remove_collected`, the only pruner, runs after the restore
    /// loop). `common-d-proposal-settled-reference-cold-list`.
    pub fn referent_class_stamp(&self, reference_obj: usize) -> Option<u32> {
        self.referent_class_stamps.get(&reference_obj).copied()
    }

    /// The stamp for `reference_obj`, or `None` when it was never stamped.
    ///
    /// gc-common w6-d: also what the post-GC pass now reads instead of a
    /// per-collection clone of the whole table (`identity_stamps_snapshot`,
    /// deleted). See [`Self::referent_class_stamp`] for why the direct read is
    /// the same answer.
    pub fn identity_stamp(&self, reference_obj: usize) -> Option<i32> {
        self.identity_stamps.get(&reference_obj).copied()
    }

    /// [`Self::weak_phantom_active_pairs`] plus each entry's identity stamp
    /// (`0` = unstamped), so the pre-GC referent-null pass can check identity
    /// after it has dropped this processor's lock.
    ///
    /// This is the pre-collection pass's FIRST call into the processor, so it
    /// also opens that pass's per-cycle bookkeeping (gc-common w1-d): it
    /// forgets the previous cycle's nulled/examined sets and records every
    /// active WEAK entry it hands out as examined. See
    /// [`Self::pre_gc_examined`] and
    /// [`Self::retire_entries_the_pre_gc_pass_found_cleared`]. `&mut self` for
    /// that reason; its only caller already holds the lock mutably.
    pub fn weak_phantom_active_triples(&mut self) -> Vec<(usize, usize, i32)> {
        self.pre_gc_nulled.clear();
        self.pre_gc_examined.clear();
        for e in &self.weak_refs {
            if !e.cleared && !e.enqueued {
                self.pre_gc_examined.insert(e.reference_obj);
            }
        }
        self.weak_phantom_active_pairs()
            .into_iter()
            .map(|(r, t)| (r, t, self.identity_stamp(r).unwrap_or(0)))
            .collect()
    }

    // gen r5w1/refs5: `soft_pre_nulled_active_triples` (the soft twin of the
    // stamp column above) lived here with no caller anywhere in the workspace
    // (`gengc-r4-mark-counters-and-comments-that-lie`); removed.

    /// Remove entries whose `Reference` object has itself been collected.
    ///
    /// Every backend calls this at the END of a collection, after the
    /// processing phases have run and their emissions have been consumed —
    /// see `vm/src/runtime/interpreter/gc_and_alloc.rs`'s post-GC path and
    /// `gc/src/zgc.rs`'s `process_references` tail. That tail position is part
    /// of the contract, not an accident: entries this drops from
    /// `finalizer_refs` / `cleaner_refs` have already had their verdict taken
    /// (see [`Self::remove_collected_reference_objects`] for why dropping them
    /// EARLY is not the same thing).
    ///
    /// IDEMPOTENT for a fixed `is_live`. `retain` keeps exactly the entries
    /// satisfying the predicate, so a second call over the result of the first
    /// removes nothing; the index rebuild and stamp prune below are both pure
    /// functions of the surviving entries. That matters because the
    /// generational driver now also runs the narrow pre-pass below, and
    /// because ZGC runs this at its own tail regardless.
    pub fn remove_collected(&mut self, is_live: &dyn Fn(usize) -> bool) {
        let before = self.total_entry_count();
        let soft_before = self.soft_refs.len();
        let finalizer_before = self.finalizer_refs.len();
        // gcd d9/e: a dead `Reference`'s deferred enqueue pair goes with its
        // row (`drop_pending_enqueues_of`). Collected only when a pair exists.
        let track_pending = !self.pending_queues.is_empty();
        let mut dropped_enqueued: FxHashSet<usize> = FxHashSet::default();
        let mut keep = |e: &ReferenceEntry| -> bool {
            let live = is_live(e.reference_obj);
            if !live && track_pending && e.enqueued {
                dropped_enqueued.insert(e.reference_obj);
            }
            live
        };
        self.soft_refs.retain(&mut keep);
        self.weak_refs.retain(&mut keep);
        self.phantom_refs.retain(&mut keep);
        self.drop_pending_enqueues_of(dropped_enqueued);
        self.cleaner_refs.retain(|e| is_live(e.reference_obj));
        self.finalizer_refs.retain(|e| is_live(e.reference_obj));
        if self.finalizer_refs.len() != finalizer_before {
            // A pruned active row must not keep refusing its address.
            self.finalizer_active_stale = true;
        }
        // PERF (gc-common w1-d): both follow-ups are pure functions of the
        // surviving rows, so when `retain` removed nothing they would rebuild
        // exactly what is already there — an O(soft · log soft) BTreeMap rebuild
        // and an O(entries) hash-set build, inside the pause, on EVERY
        // collection, including the ones that retired no Reference at all.
        if self.soft_refs.len() != soft_before {
            self.rebuild_soft_position_indices();
        }
        // Drop stamps for entries that just left. A leaked stamp is not
        // dangerous (a later `discover_*` at the same address overwrites it,
        // which is the ABA case) but it is unbounded, and this walk is already
        // O(entries).
        if self.total_entry_count() != before {
            self.prune_identity_stamps();
            self.app_row_index_stale = true;
        }
    }

    /// Rows across all five lists.
    fn total_entry_count(&self) -> usize {
        self.soft_refs.len()
            + self.weak_refs.len()
            + self.phantom_refs.len()
            + self.cleaner_refs.len()
            + self.finalizer_refs.len()
    }

    /// The part of [`Self::remove_collected`] that is safe to run BEFORE the
    /// processing phases: drop `soft_refs` / `weak_refs` / `phantom_refs`
    /// entries whose `java.lang.ref.Reference` OBJECT did not survive.
    ///
    /// # Why this exists (gengc-mark / gengc-mark2 / gengc-round3)
    ///
    /// `process_references_with_finalizer_trace` seeds `soft_live` — the
    /// closure that decides whether a `WeakReference` may be cleared — from
    /// every soft entry Phase 1 left alone. An entry whose `SoftReference`
    /// INSTANCE is dead has no softly-reachable referent (nothing can reach
    /// the referent *through* a `SoftReference` nobody can reach), so feeding
    /// it in leaves a weak reference to a dead object uncleared: a dangling
    /// `get()`, and a violation of the JLS strength ordering the block is
    /// there to enforce.
    ///
    /// Two attempts to fix this with a PREDICATE inside Phase 1 failed, both
    /// caught by `weak_ref_to_surviving_soft_referent_not_cleared_no_tracer`,
    /// `weak_ref_kept_alive_by_surviving_soft_referent_transitive` and
    /// `cleaner_not_fired_while_referent_softly_reachable`. A referent-keyed
    /// screen drops the policy-retained case (a soft referent kept by the LRU
    /// rule is unmarked by construction); a `reference_obj`-keyed screen on
    /// `is_marked` fails too, because a caller may legitimately pass a
    /// predicate — `always_dead` — for which "marked in this cycle" and "this
    /// object still exists" are different questions. Dropping the ROW is
    /// neither: it asks the caller's own survivor predicate the one question
    /// it is an oracle for, and it asks it before the phases can form an
    /// opinion. Those three tests call `process_references` directly and never
    /// run this pass, so they are untouched by construction.
    ///
    /// # Why it is NOT the whole of `remove_collected`
    ///
    /// `finalizer_refs` and `cleaner_refs` are deliberately left alone, for
    /// exactly the reason [`Self::retain_shaped_weak_phantom`] leaves them
    /// alone: their `reference_obj` is frequently not a `Reference` at all.
    /// `SharedVm::register_finalizable` registers a finalizable object as
    /// `reference_obj == referent == the object itself`, and the
    /// `java/lang/ref/Finalizer.register` native does the same through the
    /// `Cleaner` wire type. For those rows "the reference object is dead" IS
    /// "the referent is dead" — i.e. precisely the condition under which the
    /// row must FIRE — so pruning them here would silently switch finalization
    /// and those cleaners off instead of running them. HotSpot has no such
    /// hazard because its `Finalizer` instances are strongly reachable from
    /// the static `Finalizer.unfinalized` list for as long as the object is
    /// unfinalized; this list is CratonVM's stand-in for that list, and it
    /// must be treated as strongly reachable in the same way.
    ///
    /// For soft/weak/phantom the prune matches HotSpot exactly: HotSpot
    /// discovers a `Reference` only when the marker walks the `Reference`
    /// object itself, so an unreachable `Reference` is never on a discovered
    /// list and never clears or enqueues anything — it is simply garbage.
    ///
    /// Safe to combine with [`Self::remove_collected`] in the same cycle: this
    /// prunes a subset of that one's lists with the same predicate, so the
    /// later full call finds nothing left to do on those three lists.
    ///
    /// gen r5w3/unload7: returns how many of the rows it dropped were ACTIVE
    /// (neither cleared nor enqueued) — the pre-pass half of what a remark
    /// retires, next to [`ReferenceProcessingResult::retired`]'s phase half.
    /// Every existing caller ignores it.
    pub fn remove_collected_reference_objects(&mut self, is_live: &dyn Fn(usize) -> bool) -> usize {
        self.retain_reference_rows(&|e: &ReferenceEntry| is_live(e.reference_obj))
    }

    /// The sequence number the NEXT discovered row will carry
    /// ([`ReferenceEntry::registration_seq`]). Every row registered so far has
    /// a smaller one.
    ///
    /// gcd d1/c (2026-09-27): read by the generational concurrent sweep before
    /// it frees anything, for [`Self::remove_reference_objects_registered_before`].
    pub fn registration_seq(&self) -> u64 {
        self.next_registration_seq
    }

    /// [`Self::remove_collected_reference_objects`] for rows a reclamation
    /// that ran OUTSIDE a collection freed: drop the soft / weak / phantom rows
    /// registered before `registered_before` (a [`Self::registration_seq`]
    /// value) whose `Reference` object `is_freed` names. Rows registered later
    /// are kept whatever their address: they belong to objects allocated after
    /// the reclamation began, possibly on a block it freed. Returns the ACTIVE
    /// rows dropped, as the narrow pre-pass does.
    ///
    /// # Why (gcd d1/c, `gengc-r5w5-conc9-concurrent-sweep-leaves-reference-rows-of-freed-references`)
    ///
    /// The Generational concurrent sweep frees dead old objects between two
    /// collections. Without remark-time reference processing nothing pruned
    /// the row of a dead `Reference` it freed: the row kept naming the block
    /// until a later collection proved the address dead, and an old address
    /// is "live" to a young pause. Meanwhile a promotion or a direct old-gen
    /// allocation could re-issue the block, and every later pass reading the
    /// row (the pre-collection null pass, the post-collection restore / clear /
    /// enqueue loops, the next cycle's skip-set candidates) screens only by
    /// shape and identity stamp, which admits an object with no identity hash
    /// yet. The sweep prunes with a pure range test (`is_freed`), so a block
    /// re-issued since cannot make a dead row look live; the sequence bound
    /// keeps the new object's own row.
    ///
    /// `finalizer_refs` / `cleaner_refs` are left alone for the reason
    /// [`Self::remove_collected_reference_objects`] gives.
    pub fn remove_reference_objects_registered_before(
        &mut self,
        registered_before: u64,
        is_freed: &dyn Fn(usize) -> bool,
    ) -> usize {
        self.retain_reference_rows(&|e: &ReferenceEntry| {
            e.registration_seq >= registered_before || !is_freed(e.reference_obj)
        })
    }

    /// Keep the soft / weak / phantom rows `keep_row` accepts; the shared body
    /// of [`Self::remove_collected_reference_objects`] and
    /// [`Self::remove_reference_objects_registered_before`]. Returns the ACTIVE
    /// rows dropped.
    fn retain_reference_rows(&mut self, keep_row: &dyn Fn(&ReferenceEntry) -> bool) -> usize {
        let before = self.total_entry_count();
        let soft_before = self.soft_refs.len();
        let mut active_dropped = 0usize;
        // gcd d9/e: the dropped rows that may own a DEFERRED enqueue pair
        // (see `drop_pending_enqueues_of`). Only collected when a pair exists.
        let track_pending = !self.pending_queues.is_empty();
        let mut dropped_enqueued: FxHashSet<usize> = FxHashSet::default();
        let mut keep = |e: &ReferenceEntry| -> bool {
            let live = keep_row(e);
            if !live && !e.cleared && !e.enqueued {
                active_dropped += 1;
            }
            if !live && track_pending && e.enqueued {
                dropped_enqueued.insert(e.reference_obj);
            }
            live
        };
        self.soft_refs.retain(&mut keep);
        self.weak_refs.retain(&mut keep);
        self.phantom_refs.retain(&mut keep);
        self.drop_pending_enqueues_of(dropped_enqueued);
        // Same "nothing left, nothing to rebuild" short-cut as
        // `remove_collected`.
        if self.soft_refs.len() != soft_before {
            self.rebuild_soft_position_indices();
        }
        if self.total_entry_count() != before {
            self.prune_identity_stamps();
            self.app_row_index_stale = true;
        }
        active_dropped
    }

    /// gcd d9/e (2026-09-28) — drop the DEFERRED enqueue pairs
    /// (`pending_queues`) of `Reference` objects whose rows were just dropped
    /// as dead or freed, unless a surviving enqueued row still names the same
    /// address. Returns how many pairs it dropped.
    ///
    /// # Why (`docs/internal/gc/gcd-d9e-deferred-enqueue-of-a-freed-reference-writes-into-freed-old-gen-FIXED-20260928.md`)
    ///
    /// [`Self::clear_after_refused_restore`] marks a row cleared + enqueued and
    /// defers the physical enqueue to the NEXT processing round. Until then the
    /// `Reference` object is rooted by nothing: the pair lives in this side
    /// table, which is not a root, and the row is no longer "pending queued"
    /// (`is_pending_queued` wants `!enqueued`), so the frame rescue does not
    /// cover it either. If the program drops the `Reference`, the next
    /// collection's pre-pass (`remove_collected_reference_objects`) or a
    /// concurrent sweep (`remove_reference_objects_registered_before`) drops its
    /// ROW -- but the pair stayed, the next round emitted it in `to_enqueue`,
    /// and the VM's enqueue splice guards only a dead YOUNG address
    /// (`process_references_after_gc`, `is_stale_young`): for an OLD one it
    /// wrote the queue link, the `next` field and the queue head through the
    /// freed block, into whatever the old generation had re-issued there.
    /// An unreachable `Reference` is garbage and is never enqueued (the rule
    /// this module already applies to every undelivered row), so its pair goes
    /// with its row.
    ///
    /// A surviving enqueued row at a dropped address (a later registration on
    /// a re-issued block) keeps the pair that names it AND its own queue.
    fn drop_pending_enqueues_of(&mut self, dropped: FxHashSet<usize>) -> usize {
        if dropped.is_empty() || self.pending_queues.is_empty() {
            return 0;
        }
        let mut survivors: FxHashSet<(usize, usize)> = FxHashSet::default();
        for e in self
            .soft_refs
            .iter()
            .chain(self.weak_refs.iter())
            .chain(self.phantom_refs.iter())
        {
            if e.enqueued && dropped.contains(&e.reference_obj) {
                if let Some(q) = e.queue_addr {
                    survivors.insert((e.reference_obj, q));
                }
            }
        }
        let mut n = 0usize;
        self.pending_queues.retain(|&queue, refs| {
            let before = refs.len();
            refs.retain(|&r| !dropped.contains(&r) || survivors.contains(&(r, queue)));
            n += before - refs.len();
            !refs.is_empty()
        });
        if n != 0 {
            self.pending_enqueues_dropped = self.pending_enqueues_dropped.saturating_add(n as u64);
        }
        n
    }

    /// gcd d9/e — deferred enqueue pairs this processor dropped because their
    /// `Reference` died before delivery ([`Self::drop_pending_enqueues_of`]),
    /// over its life.
    pub fn pending_enqueues_dropped(&self) -> u64 {
        self.pending_enqueues_dropped
    }

    /// Rebuild `soft_ref_lru_index` to match the current `soft_refs` Vec.
    ///
    /// Without this, surviving `(timestamp, old_idx)` keys would point past
    /// the new `soft_refs.len()`, causing a panic when `process_soft_refs`
    /// indexes the BTreeMap-returned `idx`.
    ///
    /// PERF: rebuild the address->idx index in the same walk for the same
    /// reason — `retain` shifted positions, so old `idx` values are stale.
    /// The later registration wins an address, as at discovery (gcd d3/n,
    /// [`claim_soft_addr`]).
    ///
    /// Only UNCLEARED entries get an LRU key (gc-common w1-d). A cleared soft
    /// reference is never a clear candidate again, which is why
    /// `process_soft_refs` drops its key the moment it clears it — and this
    /// rebuild used to put every one of those keys back, so each collection
    /// after a prune re-walked every cleared soft entry in the candidate range
    /// just to drop its key again. The address index still covers every entry:
    /// `touch_soft_reference` must keep resolving a cleared one (it re-keys it
    /// harmlessly; the range scan's `cleared` arm drops the key again).
    fn rebuild_soft_position_indices(&mut self) {
        // gc-common w6-d: rows left, so an address may now resolve to a
        // different row (a stale row at a recycled address was pruned). See
        // `soft_touch_stamp`.
        self.bump_soft_touch_epoch();
        self.soft_ref_lru_index.clear();
        self.soft_ref_addr_index.clear();
        for (new_idx, entry) in self.soft_refs.iter().enumerate() {
            if !entry.cleared {
                self.soft_ref_lru_index
                    .insert((entry.last_access_time_ms, new_idx), new_idx);
            }
            claim_soft_addr(&mut self.soft_ref_addr_index, &self.soft_refs, new_idx);
        }
    }

    /// Keep only the stamps of entries this processor still holds.
    fn prune_identity_stamps(&mut self) {
        if self.identity_stamps.is_empty() && self.referent_class_stamps.is_empty() {
            return;
        }
        let mut live: FxHashSet<usize> = FxHashSet::default();
        for e in self
            .soft_refs
            .iter()
            .chain(self.weak_refs.iter())
            .chain(self.phantom_refs.iter())
            .chain(self.cleaner_refs.iter())
            .chain(self.finalizer_refs.iter())
        {
            live.insert(e.reference_obj);
        }
        self.identity_stamps.retain(|addr, _| live.contains(addr));
        self.referent_class_stamps
            .retain(|addr, _| live.contains(addr));
    }

    /// Retire the registry's bookkeeping entry for a `Reference` the
    /// application just enqueued *itself*, via an explicit `Reference.enqueue()`
    /// call (as opposed to the GC discovering the referent dead).
    ///
    /// PGJDBC-PHANTOM-GHOST (2026-08-07). `Reference.enqueue()` is a public,
    /// unconditional JDK API: it queues the `Reference` regardless of whether
    /// its referent is still reachable. Real code uses this — e.g. pgjdbc's
    /// `SimpleQuery.setCleanupRef`/`unprepare()` retire the *previous*
    /// `PhantomReference` wrapping a still-alive, about-to-be-reprepared
    /// `SimpleQuery` by calling `oldRef.clear(); oldRef.enqueue();` before
    /// installing a *new* `PhantomReference` around the SAME referent. Both
    /// the old and new `PhantomReference` objects are registered with this
    /// processor at construction time (`discover_reference`), and both share
    /// the same `referent` address.
    ///
    /// The native `Reference.enqueue()` path (`native_ref_enqueue` in
    /// `native-builtins/src/reference.rs`) physically links the Reference onto
    /// its `ReferenceQueue`'s Java-visible linked list directly (or, for a
    /// real-JDK-layout Reference, by invoking the JDK's own
    /// `ReferenceQueue.enqueue()` bytecode) — but until this fix, it never told
    /// *this* registry that the reference had been handled. The stale entry
    /// (`enqueued: false`) sat in `phantom_refs` (or `weak_refs`) until its own
    /// `Reference` object was later collected. If the shared referent (the
    /// `SimpleQuery`) outlived that window — entirely realistic for a
    /// long-lived, repeatedly-reprepared statement — the GC's own
    /// `process_phantom_refs` would eventually discover the referent dead and
    /// enqueue EVERY still-registered entry pointing at it, including the
    /// STALE one the application had already drained, removed from its own
    /// bookkeeping (e.g. `parsedQueryMap.remove(ref)`), and forgotten about. A
    /// second, unsolicited queue delivery of an already-fully-processed
    /// `Reference` is a ghost: `ReferenceQueue.poll()` legitimately returns
    /// non-null, but nothing recognises it any more — pgjdbc's
    /// `QueryExecutorImpl.processDeadParsedQueries()` observed exactly this as
    /// `parsedQueryMap.remove(ref)` returning `null`, feeding a `null`
    /// statement name into `sendCloseStatement` and NPEing on
    /// `statementName.getBytes(...)`.
    ///
    /// Marking the entry `cleared = true` and `enqueued = true` here (without
    /// touching `pending_queues` — the application already performed the real
    /// enqueue itself) makes every processing phase's re-entry guard
    /// (`process_weak_refs` gates on `cleared`; `process_phantom_refs` gates on
    /// `enqueued`) skip it on every subsequent cycle, so it is never
    /// rediscovered and delivered a second time. Returns whether a matching
    /// entry was found (informational only; a caller with no matching entry —
    /// e.g. `Reference.enqueue()` on a `Reference` this processor never saw
    /// `discover_reference`d, such as one constructed with a null referent —
    /// has nothing to retire and that is not an error).
    ///
    /// gc-common w2-f (2026-09-23): now [`Self::retire_by_application`] — O(1)
    /// instead of a linear scan of four lists under the global mutex, and see
    /// that method for the two rows it deliberately no longer touches.
    pub fn mark_manually_enqueued(&mut self, reference_obj: usize) -> bool {
        self.retire_by_application(reference_obj)
    }

    /// Retire the row of a `Reference` the APPLICATION has settled itself —
    /// by `Reference.clear()` or by `Reference.enqueue()` — so the collector
    /// never clears or enqueues it later. Returns whether an active row was
    /// retired.
    ///
    /// # Why (gc-common w2-f, 2026-09-23)
    ///
    /// HotSpot discovers a `Reference` only while its referent field is
    /// non-null, and both `clear()` and `enqueue()` null it, so from that
    /// moment the collector never touches the reference again: a cleared
    /// reference is NEVER enqueued by the GC. This registry learns about a
    /// reference once, at construction, and until now heard about `enqueue()`
    /// (through [`Self::mark_manually_enqueued`]) but never about `clear()`.
    /// The pre-collection pass inferred a clear from a null slot
    /// ([`Self::retire_entries_the_pre_gc_pass_found_cleared`]), which left
    /// three holes — `docs/internal/gc-common-round-20260923/common-d-reference-protocol-residuals-FIXED-20260923.md`
    /// §1-§3: a cleared PHANTOM was still delivered when its referent died (the
    /// inference cannot tell an application clear from a refused restore for a
    /// phantom), G1's final remark runs no pre-collection pass so it
    /// cleared-and-enqueued weak/soft references the program had cleared since
    /// the last young pause, and every `enqueue()` was a linear scan. The clear
    /// natives now call here directly, so none of that is inferred any more.
    ///
    /// # What it does to the row
    ///
    /// An ACTIVE soft/weak/phantom row (neither cleared nor enqueued) becomes
    /// `cleared + enqueued + clear_emitted` with nothing queued — every phase's
    /// re-entry guard skips it, no slot write is emitted for it, and it stops
    /// being a synthetic root ([`Self::pending_reference_object_addresses`]).
    /// A `jdk.internal.ref.Cleaner` row (`runs_cleaner`) also gets
    /// `action_emitted`: the gather step in `process_references` RUNS every
    /// `runs_cleaner && enqueued && !action_emitted` phantom, so the old
    /// `mark_manually_enqueued` turned an application `enqueue()` of such a
    /// Cleaner into a GC-driven `clean()` at the next collection. HotSpot runs
    /// a Cleaner only from `ReferenceHandler`, for a Cleaner the GC discovered;
    /// one whose referent the program nulled is never discovered and never run.
    ///
    /// # What it no longer touches
    ///
    /// * Rows that are already settled. A row the collector cleared or
    ///   enqueued (including one whose delivery is deferred by
    ///   [`Self::clear_after_refused_restore`]) keeps its verdict: HotSpot
    ///   delivers a reference its GC already enqueued even if the program
    ///   clears it afterwards.
    /// * `cleaner_refs`. Those rows are the synthetic `Cleaner$Cleanable`
    ///   shape and `Finalizer.register`'s `reference_obj == referent == the
    ///   finalizable object` rows. The first is not a `Reference`, so neither
    ///   native can reach it; the second IS reachable when the finalizable
    ///   object is itself a `Reference` subclass, and retiring it would have
    ///   switched that object's `finalize()` off.
    pub fn retire_by_application(&mut self, reference_obj: usize) -> bool {
        let Some((list, idx)) = self.app_row_lookup(reference_obj) else {
            return false;
        };
        let rows = match list {
            AppRowList::Soft => &mut self.soft_refs,
            AppRowList::Weak => &mut self.weak_refs,
            AppRowList::Phantom => &mut self.phantom_refs,
        };
        let e = &mut rows[idx];
        if e.cleared || e.enqueued {
            return false;
        }
        e.cleared = true;
        e.enqueued = true;
        e.clear_emitted = true;
        if e.runs_cleaner {
            e.action_emitted = true;
        }
        let lru_key = (e.last_access_time_ms, idx);
        if list == AppRowList::Soft {
            // Same hygiene as every other soft clear: a settled soft row is
            // never a clear candidate again, so its LRU key would only make
            // later range scans re-walk it.
            self.soft_ref_lru_index.remove(&lru_key);
        }
        true
    }

    /// The `(list, position)` of `reference_obj`'s row, rebuilding
    /// [`Self::app_row_index`] first when a collection invalidated it. A
    /// position that does not name `reference_obj` any more (it cannot, while
    /// every list mutation marks the index stale — but a stale answer here
    /// would retire an unrelated reference) forces one rebuild and a re-read.
    fn app_row_lookup(&mut self, reference_obj: usize) -> Option<(AppRowList, usize)> {
        if self.app_row_index_stale {
            self.rebuild_app_row_index();
        }
        for attempt in 0..2 {
            let (list, idx) = *self.app_row_index.get(&reference_obj)?;
            let rows = match list {
                AppRowList::Soft => &self.soft_refs,
                AppRowList::Weak => &self.weak_refs,
                AppRowList::Phantom => &self.phantom_refs,
            };
            if rows.get(idx).is_some_and(|e| e.reference_obj == reference_obj) {
                return Some((list, idx));
            }
            if attempt == 0 {
                self.rebuild_app_row_index();
            }
        }
        None
    }

    /// Rebuild [`Self::app_row_index`] from the three lists. Active rows win
    /// an address over settled ones; otherwise the later row wins.
    ///
    /// gcd d2/g: between two ACTIVE rows at one address, the later
    /// REGISTRATION wins (`registration_seq`), not the later list. The two
    /// coexist only while a dead `Reference`'s row outlives the object — the
    /// generational concurrent sweep frees it and prunes its row a moment
    /// later, outside the old-gen guard, and a new `Reference` allocated on
    /// the freed block in between registers at the same address. Within one
    /// list the rule is unchanged (rows are appended in registration order);
    /// across lists the later-list rule handed an application `clear()` /
    /// `enqueue()` of a new `SoftReference` to a dead `WeakReference`'s row.
    fn rebuild_app_row_index(&mut self) {
        let rows = self.soft_refs.len() + self.weak_refs.len() + self.phantom_refs.len();
        let mut index: FxHashMap<usize, (AppRowList, usize)> =
            FxHashMap::with_capacity_and_hasher(rows, Default::default());
        // Address -> registration of the active row that owns it.
        let mut active_at: FxHashMap<usize, u64> = FxHashMap::default();
        for (list, rows) in [
            (AppRowList::Soft, &self.soft_refs),
            (AppRowList::Weak, &self.weak_refs),
            (AppRowList::Phantom, &self.phantom_refs),
        ] {
            for (idx, e) in rows.iter().enumerate() {
                let active = !e.cleared && !e.enqueued;
                if active {
                    if active_at
                        .get(&e.reference_obj)
                        .is_some_and(|&owner| owner > e.registration_seq)
                    {
                        continue; // a later-registered active row owns it
                    }
                    active_at.insert(e.reference_obj, e.registration_seq);
                } else if active_at.contains_key(&e.reference_obj) {
                    continue; // an active row already owns this address
                }
                index.insert(e.reference_obj, (list, idx));
            }
        }
        self.app_row_index = index;
        self.app_row_index_stale = false;
    }

    /// Rebuild [`Self::finalizer_active`] from `finalizer_refs`.
    fn rebuild_finalizer_active(&mut self) {
        self.finalizer_active.clear();
        for e in &self.finalizer_refs {
            if !e.cleared && !e.enqueued {
                self.finalizer_active.insert(e.reference_obj);
            }
        }
        self.finalizer_active_stale = false;
    }

    /// Whether `obj` has an ACTIVE finalizer row, i.e. whether a
    /// `discover_reference(Finalizer, obj, ..)` now would be a no-op.
    pub fn is_registered_finalizable(&mut self, obj: usize) -> bool {
        if self.finalizer_active_stale {
            self.rebuild_finalizer_active();
        }
        self.finalizer_active.contains(&obj)
    }

    /// Return reference_obj addresses of all entries whose referent was cleared.
    /// The caller should null the referent field (field 0) on each of these objects.
    pub fn cleared_ref_objects(&self) -> Vec<usize> {
        let mut result = Vec::new();
        for e in &self.soft_refs {
            if e.cleared {
                result.push(e.reference_obj);
            }
        }
        for e in &self.weak_refs {
            if e.cleared {
                result.push(e.reference_obj);
            }
        }
        // Phantom refs: Java 9+ does NOT clear the referent, but we enqueue them.
        // Cleaners: cleared flag used for cleaner actions, already handled.
        result
    }

    /// bc math-ec 0x4 ROOT-CAUSE FIX (2026-06-10): once-only variant of
    /// [`Self::cleared_ref_objects`] for the post-GC referent-null writer.
    /// Returns each cleared soft/weak Reference EXACTLY ONCE across the
    /// registry's lifetime (flagging `clear_emitted`). The legacy idempotent
    /// accessor re-emitted every cleared entry on every GC forever — and once
    /// the Reference object died, the per-cycle `set_field(.., 0,
    /// Object(None))` through its recycled/remapped address corrupted
    /// whatever innocent object reused the memory (the FixedPointTest
    /// `0x4`/silent-null corruption). A referent is nulled once; re-nulling
    /// is never needed.
    pub fn take_newly_cleared(&mut self) -> Vec<usize> {
        let mut result = Vec::new();
        for e in self.soft_refs.iter_mut().chain(self.weak_refs.iter_mut()) {
            if e.cleared && !e.clear_emitted {
                e.clear_emitted = true;
                result.push(e.reference_obj);
            }
        }
        result
    }

    // -- Finalization helpers -----------------------------------------------

    pub fn pending_finalization_count(&self) -> usize {
        self.finalization_queue.len()
    }

    pub fn dequeue_for_finalization(&mut self) -> Option<usize> {
        // Round-2 fix (GC §7): VecDeque::pop_front is O(1) vs old Vec::remove(0) which was O(n).
        self.finalization_queue.pop_front()
    }

    /// Addresses of every uncleared, unenqueued Weak/Soft/Phantom `Reference`
    /// registered WITH a queue — the only ones whose delivery a program can
    /// observe.
    ///
    /// **Not a root set.** Until gc-common w2-b (2026-09-23) `collect_roots`
    /// rooted all of these; that made an unreachable registered `Reference`
    /// immortal with every strong field it holds (`ClassCache.CacheRef.type`,
    /// `WeakHashMap.Entry.value`), which the `java.lang.ref` contract does not
    /// ask for — an unreachable registered reference is never enqueued, on
    /// HotSpot as here. `vm::memory::roots::push_pending_references_held_by_frames`
    /// now uses this list only to rescue the `r = new WeakReference<>(x, q)`
    /// local that this VM's per-bci liveness drops where HotSpot's interpreter
    /// keeps it.
    ///
    /// # Why the frame rescue needs it (the netty `System.gc()`-twice bug)
    ///
    /// A caller commonly does `WeakReference<T> r = new WeakReference<>(x, q);`
    /// and never reads the local `r` again, relying on `q` to hand it back. A
    /// precise root scan drops that local once no later bytecode reads it, so
    /// the `Reference` object could be judged dead by the SAME cycle that is
    /// deciding whether to clear it, and the enqueue splice in
    /// `process_references_after_gc` then declined it forever (its
    /// "young, not in map" guard, correctly refusing to write through a dead
    /// address) — a `WeakReference` cleared but never delivered. Measured on
    /// every backend with two back-to-back `System.gc()` calls; the mechanism
    /// behind netty's `PooledByteBufAllocatorTest.testThreadCacheDestroyedByThreadCleaner`
    /// never terminating. The frame rescue keeps exactly that local alive.
    ///
    /// Once `cleared` is true the entry is no longer a candidate: a
    /// successfully-enqueued Reference is reachable through the real
    /// `ReferenceQueue` object's own linked list (a genuine heap edge), and
    /// one that failed to clear this cycle because its REFERENT survived is
    /// still reachable through whatever keeps the referent.
    ///
    /// # Narrowed twice (gc-common w1-d, 2026-09-23)
    ///
    /// These two narrowings were made while the list was still the root set
    /// (see above); they now narrow the candidate set for the frame rescue.
    ///
    /// **An ENQUEUED phantom is settled too.** `process_phantom_refs` never
    /// sets `cleared` on a phantom — only `enqueued` — so the old `!cleared`
    /// filter rooted every phantom FOREVER after its notification had been
    /// delivered. Its `Reference` object therefore never died,
    /// `remove_collected` never pruned its row, and the phantom list grew by
    /// one row (and one live `Reference` + whatever it holds) per phantom ever
    /// created: every `jdk.internal.ref.Cleaner` behind every
    /// `DirectByteBuffer`, every `java.lang.ref.Cleaner` `PhantomCleanable`,
    /// every H2 `CloseWatcher` — each one also re-walked by phase 4 on every
    /// collection. The argument above ("reachable through the queue's own
    /// linked list once delivered") holds for an enqueued phantom exactly as it
    /// does for a cleared weak reference; a `jdk.internal.ref.Cleaner` is in
    /// addition held by its class's static list until `clean()` unlinks it.
    ///
    /// **A reference with NO queue needs no guarantee.** The mechanism above
    /// exists so a `Reference` can be DELIVERED after the program dropped its
    /// last handle on it; without a queue there is nothing to deliver, and an
    /// unreachable queue-less `Reference` is simply garbage — as on HotSpot,
    /// which never discovers one. Rooting it anyway made it immortal for as
    /// long as its referent lived, together with every field it holds:
    /// `ThreadLocal.ThreadLocalMap.Entry` is a queue-less `WeakReference`
    /// whose `value` is a strong field, so every thread-local VALUE of every
    /// thread that ever died stayed reachable for as long as its (typically
    /// `static`) `ThreadLocal` key did; a dropped soft cache kept every
    /// referent the LRU policy had not yet condemned.
    ///
    /// Dropping those rows from the root set is what makes them collectable;
    /// what then happens to a dead row is the ordinary dead-`Reference` path
    /// every cleared row already takes (`remove_collected_reference_objects`
    /// before the phases, `remove_collected` after, and the staleness guards
    /// on every write in between).
    pub fn pending_reference_object_addresses(&self) -> Vec<usize> {
        self.weak_refs
            .iter()
            .chain(self.soft_refs.iter())
            .chain(self.phantom_refs.iter())
            .filter(|e| Self::is_pending_queued(e))
            .map(|e| e.reference_obj)
            .collect()
    }

    /// The row predicate of [`Self::pending_reference_object_addresses`]:
    /// uncleared, unenqueued, registered with a queue.
    #[inline]
    fn is_pending_queued(e: &ReferenceEntry) -> bool {
        !e.cleared && !e.enqueued && e.queue_addr.is_some()
    }

    /// gen r5w1/refs5 — [`Self::pending_reference_object_addresses`]
    /// restricted to `candidates`: the addresses among `candidates` that are
    /// pending queued `Reference` objects.
    ///
    /// For `collect_roots`' step 22, whose question is "which of the few
    /// objects this thread's frames hold in liveness-dead locals are pending
    /// queued references?". It used to build the whole pending list (a `Vec`
    /// with one entry per queued row, e.g. every `WeakHashMap.Entry` in the
    /// process) and then an `FxHashSet` of it, inside every collection's root
    /// scan, to intersect it with a handful of locals. This walks the rows
    /// once against the small set instead and allocates only the answer.
    pub fn pending_reference_objects_among(
        &self,
        candidates: &FxHashSet<usize>,
    ) -> FxHashSet<usize> {
        if candidates.is_empty() {
            return FxHashSet::default();
        }
        self.weak_refs
            .iter()
            .chain(self.soft_refs.iter())
            .chain(self.phantom_refs.iter())
            .filter(|e| candidates.contains(&e.reference_obj) && Self::is_pending_queued(e))
            .map(|e| e.reference_obj)
            .collect()
    }

    /// INT-8: addresses of every registered Weak/Soft/Phantom `Reference`
    /// OBJECT (not referent). The G1 marker hides slot 0 (the referent) of
    /// exactly these objects for the duration of a concurrent mark cycle so
    /// the trace cannot keep weak referents alive through their References.
    ///
    /// Deliberately EXCLUDED:
    /// - Finalizer entries — their `reference_obj` IS the finalizable object
    ///   itself; its slot 0 is an ordinary strong field.
    /// - Cleaner entries — the registered object is the cleanable, whose
    ///   layout is not the referent-at-slot-0 `Reference` shape; hiding a
    ///   strong slot would under-mark (freed-live corruption), whereas
    ///   including nothing merely over-retains cleaner referents one cycle.
    ///
    /// Cleared/enqueued entries are included: their referent slot is already
    /// null, so hiding it is a no-op either way and the set stays cheap to
    /// build (no per-entry filtering).
    ///
    /// (This paragraph sat above [`Self::pending_reference_object_addresses`]
    /// until gc-common w3-d, fused into that method's doc.)
    pub fn reference_object_addresses(&self) -> Vec<usize> {
        self.weak_refs
            .iter()
            .chain(self.soft_refs.iter())
            .chain(self.phantom_refs.iter())
            .map(|e| e.reference_obj)
            .collect()
    }

    /// gen r5w1/refs5 — the ACTIVE (neither cleared nor enqueued) Weak, Soft
    /// and Phantom `Reference` objects, each with its identity stamp (`0` =
    /// unstamped): the generational concurrent cycle's referent-slot skip set
    /// input (`ConcurrentMarker::set_reference_skip`).
    ///
    /// Active only, unlike [`Self::reference_object_addresses`] (G1's input):
    /// hiding a referent is the dangerous direction — an unmarked referent is
    /// freed by the sweep — so the set must hold only rows the remark's
    /// processing will evaluate. An inactive row whose slot 0 still holds a
    /// referent (an application `enqueue()` that did not clear, a refused
    /// clear) would otherwise hide a live object nothing processes. Finalizer
    /// and Cleaner rows are excluded for the reasons that method gives.
    pub fn active_reference_objects_with_stamps(&self) -> Vec<(usize, i32)> {
        self.weak_refs
            .iter()
            .chain(self.soft_refs.iter())
            .chain(self.phantom_refs.iter())
            .filter(|e| !e.cleared && !e.enqueued)
            .map(|e| {
                (
                    e.reference_obj,
                    self.identity_stamps
                        .get(&e.reference_obj)
                        .copied()
                        .unwrap_or(0),
                )
            })
            .collect()
    }

    /// gen r5w1/refs5 — the addresses [`Self::active_reference_objects_with_stamps`]
    /// reports, as a set: what the generational remark compares before and
    /// after its processing round to learn which rows it retired.
    pub fn active_reference_object_set(&self) -> FxHashSet<usize> {
        self.weak_refs
            .iter()
            .chain(self.soft_refs.iter())
            .chain(self.phantom_refs.iter())
            .filter(|e| !e.cleared && !e.enqueued)
            .map(|e| e.reference_obj)
            .collect()
    }

    /// INT-8: referents of soft references that the last processing round
    /// chose to KEEP (uncleared, unenqueued). Under referent-slot hiding the
    /// marker never traced these, so a softly-only-reachable referent is
    /// unmarked even though the policy retained it — the remark driver must
    /// resurrect (mark) each one before cleanup frees regions, or the kept
    /// soft reference's `get()` would return a dangling pointer. HotSpot
    /// does the same: policy-retained soft referents are kept alive by
    /// reference processing itself.
    pub fn soft_survivor_referents(&self) -> Vec<usize> {
        self.soft_refs
            .iter()
            .filter(|e| !e.cleared && !e.enqueued)
            .map(|e| e.referent)
            .collect()
    }

    /// INT-8: addresses of registered, not-yet-fired `Cleanable` objects
    /// whose action may still run — the remark driver resurrects these so a
    /// cleanup in-place free can never invalidate a pending cleaner chain
    /// (the HotSpot equivalent is the Cleaner's internal strong list of
    /// PhantomCleanables). Two exclusions:
    /// - entries whose action already fired (`action_emitted`) — nothing
    ///   left to protect;
    /// - SELF-REFERENT entries (`reference_obj == referent`, the
    ///   finalizer-style `discover_reference(3, obj, obj, ..)` shape) —
    ///   resurrecting those would keep the referent itself alive forever
    ///   and the cleaner would never fire.
    ///
    /// Note the Cleanable's heap layout holds only the ACTION (slot 0), not
    /// the referent, so keeping it live cannot retain the referent.
    pub fn cleaner_pending_object_addresses(&self) -> Vec<usize> {
        self.cleaner_refs
            .iter()
            .filter(|e| !e.action_emitted && e.reference_obj != e.referent)
            .map(|e| e.reference_obj)
            .collect()
    }

    /// Return the referent addresses of all registered (non-cleared, non-enqueued)
    /// finalizer references.  Used before GC to add them as resurrection roots.
    pub fn finalizer_referent_addresses(&self) -> Vec<usize> {
        self.finalizer_refs
            .iter()
            .filter(|e| !e.cleared && !e.enqueued)
            .map(|e| e.referent)
            .collect()
    }

    /// Mark the finalizer entries whose CURRENT referent address appears in
    /// `referents` as enqueued.
    ///
    /// Called by the VM after the GC's resurrection channel
    /// (`collect_garbage_with_finalizers`) reports which finalizables were
    /// dead-but-resurrected: without this flag, a resurrected object looks
    /// ALIVE to the next collection's `is_marked` (it is in the pointer map),
    /// so `process_final_refs` never claims it and
    /// `finalizer_referent_addresses` keeps re-returning it — the GC then
    /// resurrects and re-enqueues it EVERY cycle and `finalize()` runs
    /// repeatedly (observed 3× per object) instead of exactly once.
    ///
    /// Must be called AFTER `update_after_gc` for the same collection, so
    /// entry referents already hold the post-GC addresses the resurrection
    /// channel reports.
    pub fn mark_finalizer_enqueued(&mut self, referents: &[usize]) {
        if referents.is_empty() {
            return;
        }
        let set: FxHashSet<usize> = referents.iter().copied().collect();
        for e in &mut self.finalizer_refs {
            if !e.enqueued && set.contains(&e.referent) {
                e.enqueued = true;
                self.finalizer_active_stale = true;
                // gen r5w1/refs5: NOT `stats.finalizer_refs_enqueued`. This
                // runs after the round's result was snapshotted, and the next
                // round zeroes `stats` first, so the bump could never be read
                // (`gengc-r4-mark-counters-and-comments-that-lie`). The
                // channel's own cumulative count is below.
                self.finalizers_enqueued_by_channel += 1;
            }
        }
    }

    /// gen r5w1/refs5 — finalizer rows [`Self::mark_finalizer_enqueued`] has
    /// marked enqueued over this processor's life (the resurrection channel's
    /// finalizables), which no `ReferenceProcessingStats` snapshot can carry.
    pub fn finalizers_enqueued_by_channel(&self) -> u64 {
        self.finalizers_enqueued_by_channel
    }

    // -- Stats & config -----------------------------------------------------

    pub fn stats(&self) -> &ReferenceProcessingStats {
        &self.stats
    }

    pub fn reset_stats(&mut self) {
        self.stats = ReferenceProcessingStats::default();
    }

    pub fn set_soft_ref_lru_policy(&mut self, ms_per_mb: u64) {
        self.soft_ref_lru_policy_ms_per_mb = ms_per_mb;
    }

    pub fn weak_ref_count(&self) -> usize {
        self.weak_refs.len()
    }

    pub fn soft_ref_count(&self) -> usize {
        self.soft_refs.len()
    }

    pub fn phantom_ref_count(&self) -> usize {
        self.phantom_refs.len()
    }

    /// Number of registered cleaner references (phantom-typed Cleaner entries).
    /// Exposed for NEW-17 tests verifying that `Cleaner.register` / direct
    /// buffer allocation correctly discover their cleanables with the GC.
    pub fn cleaner_ref_count(&self) -> usize {
        self.cleaner_refs.len()
    }

    /// Apply the SoftReference LRU policy *before* the collection and report
    /// the entries it condemns, so the caller can null their referent slot
    /// ahead of the mark.
    ///
    /// # Why the decision has to happen here and not after the mark
    ///
    /// [`Self::process_soft_refs`] refuses to clear an entry whose referent
    /// `is_marked`, and CratonVM's markers trace a `Reference`'s slot 0 as an
    /// ordinary strong edge. A soft referent is therefore *always* marked
    /// through its own `SoftReference`, that check always wins, and the LRU
    /// policy underneath it is unreachable — soft references behaved exactly
    /// like strong ones on every collector. Measured 2026-08-15 with a
    /// four-arm probe in a 64 MiB heap: HotSpot cleared the soft reference and
    /// allocated 30 MiB past it; CratonVM threw `OutOfMemoryError` under both
    /// ZGC and G1 with the referent still reachable only softly.
    ///
    /// `weakref_null_referents_pre_gc` already solves precisely this problem
    /// for Weak and Phantom by nulling slot 0 before the mark, which is why
    /// those two work. Soft differs in exactly one respect: a weak referent
    /// dies whenever nothing else holds it, while a soft referent dies only
    /// when the LRU policy judges the heap tight enough. So the policy runs
    /// first and only its condemned set is nulled; an entry the policy wants
    /// to keep is left traced strongly and is retained bit-for-bit as before.
    ///
    /// `free_heap_mb` and `current_time_ms` carry the same meaning as in
    /// [`Self::process_references`], including the `0`-clock convention: the
    /// processor substitutes the mutator clock it has observed through
    /// [`Self::touch_soft_reference`] when the caller has none.
    ///
    /// Returns `(reference_obj, referent)` per condemned entry. Calling this
    /// also RESETS the condemned set, so it must be called once per
    /// collection, before the mark.
    ///
    /// # The clock argument is load-bearing here, unlike post-GC
    ///
    /// `current_time_ms == 0` makes "now" the last value a mutator handed
    /// [`Self::touch_soft_reference`] — i.e. the moment of the most recent
    /// `SoftReference.get()` anywhere in the process. For the reference that
    /// *made* that call the idle window is then exactly zero, forever, and a
    /// program in a tight allocation loop reading its own cache is precisely
    /// the program that keeps re-stamping it. The caller here is ordinary VM
    /// code on the mutator side of a safepoint, so it can and does supply a
    /// real `SystemTime` reading; `0` is accepted only so tests can pin the
    /// clock.
    ///
    /// # `CRATONVM_SOFTREF_HOTSPOT_LRU` (gen r5w1/refs5; default ON on the Generational heap since gen r5w4/defaults8)
    ///
    /// Selected per VM by [`Self::new_for_heap`]. Both arguments stop being
    /// the policy's inputs. The clock is the END of
    /// the previous processing round and the free space the one that round was
    /// handed (post-collection on the STW paths), HotSpot's `LRUMaxHeapPolicy`:
    /// a soft reference read since the last collection has idle time 0 and is
    /// never condemned, and an unread one in a heap with 200 MB free after the
    /// last collection is kept for about 200 s. `current_time_ms` is recorded
    /// as this collection's clock, committed when its processing round ends;
    /// `free_heap_mb` is used only before the first round. Timestamps stay the
    /// wall-clock stamps `SoftReference.get()` writes (HotSpot stamps the
    /// clock instead), so a reference read between two collections is at most
    /// one collection interval YOUNGER here than on HotSpot — never cleared
    /// sooner.
    pub fn condemn_idle_soft_refs(
        &mut self,
        free_heap_mb: usize,
        current_time_ms: u64,
    ) -> Vec<(usize, usize)> {
        if self.hotspot_soft_lru {
            let now_ms = current_time_ms.max(self.last_observed_clock_ms);
            self.soft_clock_pending_ms = self.soft_clock_pending_ms.max(now_ms);
            let (clock, max_interval_ms) = self.hotspot_soft_policy(free_heap_mb);
            return self
                .condemn(|e| clock.saturating_sub(e.last_access_time_ms) > max_interval_ms);
        }
        let now_ms = current_time_ms.max(self.last_observed_clock_ms);
        let threshold_ms = self
            .soft_ref_lru_policy_ms_per_mb
            .saturating_mul(free_heap_mb as u64);
        self.condemn(|e| now_ms.saturating_sub(e.last_access_time_ms) > threshold_ms)
    }

    /// Condemn EVERY active soft reference, ignoring the LRU policy: the
    /// last-ditch rule the `java.lang.ref` specification states outright —
    /// "all soft references to softly-reachable objects are guaranteed to have
    /// been cleared before the virtual machine throws an
    /// `OutOfMemoryError`". HotSpot implements it as
    /// `SoftRefPolicy::should_clear_all_soft_refs`, armed for the full GC it
    /// runs when an allocation has already failed.
    ///
    /// This is a genuinely different rule from [`Self::condemn_idle_soft_refs`]
    /// and not a limiting case of it: the LRU policy asks how long ago the
    /// application last read the reference, and a program looping on its own
    /// soft-referenced cache re-stamps that clock on every iteration, so its
    /// idle window never opens however tight the heap gets. Measured: a
    /// 64 MiB heap where HotSpot cleared the reference and completed, and
    /// CratonVM threw `OutOfMemoryError` with a megabyte of softly-reachable
    /// garbage in hand.
    ///
    /// `is_marked` still decides the outcome, exactly as for the idle set: a
    /// condemned referent that is also strongly reachable was marked anyway
    /// and is kept, then restored. "Clear all soft references" means all the
    /// ones nothing else holds.
    pub fn condemn_all_soft_refs(&mut self) -> Vec<(usize, usize)> {
        self.condemn(|_| true)
    }

    /// Ask that the NEXT collection on this VM — whichever thread initiates
    /// it — apply the last-ditch rule ([`Self::condemn_all_soft_refs`]).
    ///
    /// gc-common w4-d (2026-09-23), `common-d-reference-protocol-residuals` §4.
    /// The allocation-failure ladder arms the rule thread-locally
    /// (`with_last_ditch_soft_clear`) and then tries to initiate its own
    /// collection; when another thread's pause wins every attempt, the
    /// collection that actually runs is not armed and the caller can throw
    /// `OutOfMemoryError` with softly-reachable objects in hand. A request
    /// recorded here is consumed by the next pre-collection pass
    /// ([`Self::take_clear_all_soft_request`]), so the rule reaches the
    /// collection that runs, not only the one this thread would have run.
    pub fn request_clear_all_soft_refs(&mut self) {
        self.clear_all_soft_requested = true;
    }

    /// Consume a [`Self::request_clear_all_soft_refs`]: `true` exactly once per
    /// request. Called by the pre-collection pass, which then condemns every
    /// soft reference instead of the idle ones.
    pub fn take_clear_all_soft_request(&mut self) -> bool {
        std::mem::take(&mut self.clear_all_soft_requested)
    }

    /// Whether any soft entry is still live enough to be worth a last-ditch
    /// collection — so the escalation ladder can skip one full GC when the
    /// application uses no soft references at all.
    pub fn has_active_soft_refs(&self) -> bool {
        self.soft_refs.iter().any(|e| !e.cleared && !e.enqueued)
    }

    /// Shared body of the two condemnation rules: select from the active soft
    /// entries, publish the selection as this cycle's condemned set (replacing
    /// any previous cycle's), and hand the pairs back for the caller to null.
    fn condemn(&mut self, pick: impl Fn(&ReferenceEntry) -> bool) -> Vec<(usize, usize)> {
        let condemned: Vec<(usize, usize)> = self
            .soft_refs
            .iter()
            .filter(|e| !e.cleared && !e.enqueued)
            .filter(|e| pick(e))
            .map(|e| (e.reference_obj, e.referent))
            .collect();
        self.soft_pre_nulled.clear();
        self.soft_pre_nulled
            .extend(condemned.iter().map(|&(ref_obj, _)| ref_obj));
        // The pre-collection pass nulls exactly what this returns, so every
        // condemned soft entry is one it EXAMINED — see `pre_gc_examined`.
        self.pre_gc_examined
            .extend(condemned.iter().map(|&(ref_obj, _)| ref_obj));
        condemned
    }

    /// The `(reference_obj, referent)` pairs [`Self::condemn_idle_soft_refs`]
    /// condemned this cycle whose entry is STILL active — the referent turned
    /// out to be strongly reachable, so `process_soft_refs` kept it and the
    /// slot 0 the pre-collection pass nulled has to be written back.
    ///
    /// The soft twin of [`Self::weak_phantom_active_pairs`], consumed by the
    /// same post-GC restore loop.
    pub fn soft_pre_nulled_active_pairs(&self) -> Vec<(usize, usize)> {
        self.soft_refs
            .iter()
            .filter(|e| {
                !e.cleared && !e.enqueued && self.soft_pre_nulled.contains(&e.reference_obj)
            })
            .map(|e| (e.reference_obj, e.referent))
            .collect()
    }

    /// HIB-CV-24 — `(reference_obj, referent)` for every Weak/Phantom reference
    /// that is neither cleared nor already enqueued. Used by the VM's
    /// before/after-GC referent fixup: the referent slots are nulled before a
    /// collection (so the mark phase does NOT keep them alive through the live
    /// Reference object), then survivors are restored afterwards. The addresses
    /// are the processor's current (pre-collection) view; the caller must apply
    /// the GC pointer map to locate the post-collection objects.
    ///
    /// SoftReferences are excluded: the policy-condemned ones travel through
    /// [`Self::condemn_idle_soft_refs`] / [`Self::soft_pre_nulled_active_pairs`];
    /// the rest stay traced strongly. (gen r5w1/refs5: this doc sat on
    /// `condemn_idle_soft_refs`, fused into its first paragraph.)
    pub fn weak_phantom_active_pairs(&self) -> Vec<(usize, usize)> {
        let mut v = Vec::with_capacity(self.weak_refs.len() + self.phantom_refs.len());
        for e in self.weak_refs.iter().chain(self.phantom_refs.iter()) {
            if !e.cleared && !e.enqueued {
                v.push((e.reference_obj, e.referent));
            }
        }
        v
    }

    /// Every heap address this processor holds a raw pointer to, across ALL
    /// reference kinds (soft / weak / phantom / cleaner / finalizer): the
    /// `Reference` object itself, its referent, and its `ReferenceQueue`.
    ///
    /// Published to `gc_quiescence::set_watched_referents` before each
    /// collection. Both young collectors and the old-gen collector emit an
    /// identity `pointer_map` entry for every watched address that SURVIVES
    /// but does not move, which is what makes "absent from the pointer map"
    /// an exact death proof for precisely the addresses post-GC reference
    /// processing writes through — see
    /// `VmHeap::watched_pre_gc_addr_survived`.
    ///
    /// Superset of [`Self::weak_phantom_active_pairs`] and the former
    /// `weak_phantom_active_queue_addrs` (no caller; removed in gen r5w1/refs5),
    /// which covered only the weak/phantom halves and only their non-cleared
    /// entries: the survival
    /// predicate is consulted for soft/cleaner/finalizer entries too (via
    /// `process_references` and `remove_collected`), so every one of them
    /// needs the same proof.
    pub fn all_tracked_addrs(&self) -> Vec<usize> {
        let n = self.soft_refs.len()
            + self.weak_refs.len()
            + self.phantom_refs.len()
            + self.cleaner_refs.len()
            + self.finalizer_refs.len();
        let mut v = Vec::with_capacity(n * 3);
        for e in self
            .soft_refs
            .iter()
            .chain(self.weak_refs.iter())
            .chain(self.phantom_refs.iter())
            .chain(self.cleaner_refs.iter())
            .chain(self.finalizer_refs.iter())
        {
            v.push(e.reference_obj);
            v.push(e.referent);
            if let Some(q) = e.queue_addr {
                v.push(q);
            }
        }
        v
    }
}

impl Default for ReferenceProcessor {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// CleanerThread
// ---------------------------------------------------------------------------

/// Manages pending `Cleaner` actions that need to be invoked, and the
/// `ReferenceQueue`s whose waiters the collector owes a wake-up.
pub struct CleanerThread {
    pending_actions: Mutex<VecDeque<usize>>,
    /// POST-collection addresses of `ReferenceQueue`s the collector spliced a
    /// `Reference` onto since the last drain, deduplicated, sorted.
    ///
    /// gc-common w3-d (2026-09-23), `common-w2f-gc-enqueue-never-wakes-queue-waiters`.
    /// The GC's enqueue loops link a reference into the queue's list with raw
    /// slot writes; the JDK's `enqueue0` ends with `lock.notifyAll()`, which the
    /// collector cannot do inside a pause (a waiter stopped at a safepoint
    /// inside `synchronized (lock)` would deadlock the initiator). So the pause
    /// records the queue here and the reference-delivery thread notifies it
    /// afterwards (`vm/src/runtime/interpreter/gc_and_alloc.rs`,
    /// `gc_enqueued_queues_notify`). Only filled under the full hand-off
    /// (`CRATONVM_FINALIZER_THREAD=1`).
    ///
    /// Raw addresses, kept current exactly like `pending_actions`: relocated
    /// by [`Self::update_after_gc`] and reported by [`Self::pending_addresses`],
    /// which the VM roots (`roots.rs` step 22b), so a queue waiting here is
    /// neither moved away from nor freed under its entry.
    queues_to_notify: Mutex<Vec<usize>>,
    running: AtomicBool,
}

impl CleanerThread {
    pub fn new() -> Self {
        Self {
            pending_actions: Mutex::new(VecDeque::new()),
            queues_to_notify: Mutex::new(Vec::new()),
            running: AtomicBool::new(false),
        }
    }

    /// Record `ReferenceQueue`s (post-collection addresses) whose waiters must
    /// be notified once the pause is over. Duplicates collapse. See
    /// `queues_to_notify`.
    pub fn note_queues_to_notify(&self, queues: &[usize]) {
        if queues.is_empty() {
            return;
        }
        let mut list = self.queues_to_notify.lock();
        list.extend_from_slice(queues);
        list.sort_unstable();
        list.dedup();
    }

    /// Take every queue recorded by [`Self::note_queues_to_notify`].
    pub fn drain_queues_to_notify(&self) -> Vec<usize> {
        std::mem::take(&mut *self.queues_to_notify.lock())
    }

    /// Whether any cleaner action or queue notification is waiting.
    pub fn has_pending_work(&self) -> bool {
        // Two statements, so the two guards are never held together (a
        // temporary in `a || b` lives to the end of the whole expression).
        let actions = !self.pending_actions.lock().is_empty();
        if actions {
            return true;
        }
        !self.queues_to_notify.lock().is_empty()
    }

    pub fn submit_action(&self, addr: usize) {
        self.pending_actions.lock().push_back(addr);
    }

    pub fn drain_actions(&self) -> Vec<usize> {
        let mut lock = self.pending_actions.lock();
        lock.drain(..).collect()
    }

    /// Relocate every pending cleaner-action address through a GC pointer map.
    ///
    /// Cleaner actions can be *deferred* across GC cycles (e.g. when the only
    /// safepoint is reached from inside a JIT helper that holds the `&mut
    /// JvmThread`, running the action's `run()` there would re-enter the JIT
    /// and alias the borrow — so the interpreter leaves the actions queued and
    /// runs them at the next top-level safepoint). While queued, the cleanable
    /// objects they point at can be evacuated by a compacting collector, so
    /// their raw addresses must be remapped on every GC or a later
    /// `drain_actions` would deref freed/moved memory → SEGV.
    pub fn update_after_gc(&self, pointer_map: &cratonvm_types::PointerMap) {
        if pointer_map.is_empty() {
            return;
        }
        {
            let mut lock = self.pending_actions.lock();
            for addr in lock.iter_mut() {
                if let Some(&new) = pointer_map.get(addr) {
                    *addr = new;
                }
            }
        }
        // gc-common w3-d: the queues still owed a wake-up move like the actions.
        // Re-sorted, because a relocation can reorder (and, on a collision
        // between a moved key and an old address, duplicate) entries.
        let mut queues = self.queues_to_notify.lock();
        if !queues.is_empty() {
            for addr in queues.iter_mut() {
                if let Some(&new) = pointer_map.get(addr) {
                    *addr = new;
                }
            }
            queues.sort_unstable();
            queues.dedup();
        }
    }

    pub fn pending_count(&self) -> usize {
        self.pending_actions.lock().len()
    }

    /// Addresses of every submitted-but-not-yet-run cleaner action object.
    ///
    /// gc-common w1-d (2026-09-23). Like [`FinalizerThread::pending_addresses`],
    /// these are raw addresses no marker can see, and like finalizers they are
    /// routinely DEFERRED across collections (`run_cleaner_actions` declines
    /// while a JIT helper holds the thread borrow). [`Self::update_after_gc`]
    /// covers a MOVING collection; nothing covered a sweeping one, which frees a
    /// cleanable whose last strong holder was the object that just died —
    /// `native-io`'s direct-buffer cleanable is held only by its buffer — and
    /// leaves this queue naming reclaimed memory that `run_cleaner_actions`
    /// will write slot 0/1 of and then invoke. The VM must root these; see
    /// `docs/internal/gc-common-round-20260923/applied/handoff-d-root-pending-cleaner-actions.md`.
    ///
    /// gc-common w3-d: also the `ReferenceQueue`s still owed a wake-up
    /// ([`Self::note_queues_to_notify`]) — raw addresses for the same reason,
    /// and rooted through the same call, so no `roots.rs` change was needed.
    /// A queue kept alive this way is one a `Reference` was just linked into;
    /// it is released by the drain that notifies it.
    pub fn pending_addresses(&self) -> Vec<usize> {
        let mut addrs: Vec<usize> = self.pending_actions.lock().iter().copied().collect();
        addrs.extend(self.queues_to_notify.lock().iter().copied());
        addrs
    }

    pub fn is_running(&self) -> bool {
        self.running.load(Ordering::Acquire)
    }

    pub fn start(&self) {
        self.running.store(true, Ordering::Release);
    }

    pub fn stop(&self) {
        self.running.store(false, Ordering::Release);
    }
}

impl Default for CleanerThread {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// FinalizerThread
// ---------------------------------------------------------------------------

/// Maximum number of objects in the finalizer queue to prevent memory exhaustion.
const FINALIZER_QUEUE_MAX_CAPACITY: usize = 100_000;

/// Per-finalizer timeout in milliseconds: this VM's own watchdog budget for
/// one `finalize()` call. HotSpot has no per-finalizer timeout (its
/// `Finalizer` thread runs each one to completion); gen r5w1/refs5 removed the
/// "matches HotSpot's 2-second default" this doc used to claim.
const FINALIZER_TIMEOUT_MS: u64 = 2_000;

/// The finalizer queue: FIFO order plus an O(1) membership index.
///
/// The two halves live behind ONE lock and are only ever updated together,
/// because an index that drifts from the order fails silently in both
/// directions — refusing a legitimate enqueue forever, or letting a duplicate
/// through. `FINALIZER_QUEUE_MAX_CAPACITY` is 100_000, so the index is not a
/// micro-optimization: scanning the order on every enqueue would make a burst
/// of enqueues quadratic.
#[derive(Default)]
struct FinalizerQueue {
    order: VecDeque<usize>,
    present: FxHashSet<usize>,
}

impl FinalizerQueue {
    fn contains(&self, addr: usize) -> bool {
        self.present.contains(&addr)
    }

    fn push(&mut self, addr: usize) {
        self.order.push_back(addr);
        self.present.insert(addr);
    }

    fn pop(&mut self) -> Option<usize> {
        let addr = self.order.pop_front()?;
        self.present.remove(&addr);
        Some(addr)
    }

    fn len(&self) -> usize {
        self.order.len()
    }

    fn addresses(&self) -> Vec<usize> {
        self.order.iter().copied().collect()
    }

    /// Rewrite every entry through `relocate`, rebuilding the index from the
    /// result rather than patching it: a collection can relocate one entry onto
    /// another's old address, so only a rebuild is guaranteed to agree with the
    /// order it indexes.
    fn remap(&mut self, relocate: impl Fn(usize) -> usize) {
        for addr in self.order.iter_mut() {
            *addr = relocate(*addr);
        }
        self.present.clear();
        self.present.extend(self.order.iter().copied());
    }
}

/// The wake-up channel between the collector and the VM's reference-delivery
/// thread — the thread that runs `finalize()`, cleaner actions and the
/// `ReferenceQueue` notifications the collector owes, instead of whichever
/// mutator happened to trigger the collection.
///
/// gc-common w3-d (2026-09-23), `common-w2f-finalizers-and-cleaners-run-on-the-allocating-mutator`.
/// A counter pair rather than a flag, so neither side can lose a request:
///
/// * [`Self::request`] bumps `requested` (any thread, including a GC
///   initiator inside its pause: a lock and a notify, nothing else);
/// * the delivery thread waits in [`Self::wait_for_request`] until
///   `requested != served`, snapshots `requested` in [`Self::begin_batch`],
///   drains every queue, and publishes that snapshot as `served` in
///   [`Self::finish_batch`]. Work requested DURING a batch bumped `requested`
///   past the snapshot, so the next wait returns at once;
/// * a caller that must see the queues drained before it returns
///   (`System.gc()`, which is also `Bits.reserveMemory`'s last resort) waits in
///   [`Self::wait_until_served`] for `served` to reach the value `requested`
///   had when it asked.
///
/// Lives in an `Arc` so the delivery thread can park on it while holding only
/// a weak handle to its VM.
pub struct DeliverySignal {
    state: Mutex<DeliveryState>,
    changed: Condvar,
}

#[derive(Default)]
struct DeliveryState {
    /// Bumped by every [`DeliverySignal::request`].
    requested: u64,
    /// The `requested` value the last finished batch was opened at.
    served: u64,
}

impl DeliverySignal {
    pub fn new() -> Self {
        Self {
            state: Mutex::new(DeliveryState::default()),
            changed: Condvar::new(),
        }
    }

    /// Ask the delivery thread for a batch.
    pub fn request(&self) {
        {
            let mut s = self.state.lock();
            s.requested += 1;
        }
        self.changed.notify_all();
    }

    /// Delivery-thread side: block until a batch is owed or `timeout` passes.
    /// `true` when a batch is owed.
    pub fn wait_for_request(&self, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        let mut s = self.state.lock();
        while s.requested == s.served {
            if self.changed.wait_until(&mut s, deadline).timed_out() {
                return s.requested != s.served;
            }
        }
        true
    }

    /// Delivery-thread side: open a batch. Returns the ticket for
    /// [`Self::finish_batch`].
    pub fn begin_batch(&self) -> u64 {
        self.state.lock().requested
    }

    /// Delivery-thread side: close the batch opened by [`Self::begin_batch`].
    pub fn finish_batch(&self, ticket: u64) {
        {
            let mut s = self.state.lock();
            if ticket > s.served {
                s.served = ticket;
            }
        }
        self.changed.notify_all();
    }

    /// Requester side: wait until every batch requested before this call has
    /// been served, for at most `timeout`. `true` when it was.
    pub fn wait_until_served(&self, timeout: Duration) -> bool {
        let target = self.ticket();
        self.wait_for_ticket(target, timeout)
    }

    /// Requester side: the ticket that covers every request made so far — the
    /// `requested` count now. Pass it to [`Self::wait_for_ticket`].
    ///
    /// gc-common w4-d (2026-09-23). A requester that waits in SLICES (to look
    /// for a deadlock between slices, `gc_and_alloc.rs`'s
    /// `delivery_thread_waits_on_our_lock`) must keep one target across them:
    /// calling [`Self::wait_until_served`] per slice would re-read `requested`
    /// each time and chase every later request from other threads.
    pub fn ticket(&self) -> u64 {
        self.state.lock().requested
    }

    /// Whether a batch is owed or running: some request has not yet been
    /// covered by a finished batch. The analogue of HotSpot's
    /// `Reference.processPendingActive || hasReferencePendingList()`, which
    /// `JavaLangRefAccess.waitForReferenceProcessing()` asks before it waits.
    ///
    /// gc-common w5-d (2026-09-24). A batch that has already taken every queued
    /// action (`begin_batch` snapshots `requested`, `finish_batch` publishes it)
    /// leaves the queues empty while it still runs them, so "is anything
    /// queued" alone would answer "nothing in flight" for the very batch that is
    /// about to free a `DirectByteBuffer`'s memory.
    pub fn in_flight(&self) -> bool {
        let s = self.state.lock();
        s.requested != s.served
    }

    /// Requester side: wait until a batch opened at or after `target` (a value
    /// [`Self::ticket`] returned) has finished, for at most `timeout`. `true`
    /// when it has.
    pub fn wait_for_ticket(&self, target: u64, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        let mut s = self.state.lock();
        while s.served < target {
            if self.changed.wait_until(&mut s, deadline).timed_out() {
                return s.served >= target;
            }
        }
        true
    }
}

impl Default for DeliverySignal {
    fn default() -> Self {
        Self::new()
    }
}

/// Manages the queue of objects awaiting `finalize()` execution.
///
/// Includes resurrection detection: objects that have already been finalized
/// once are tracked and will not be finalized again (per JLS §12.6).
pub struct FinalizerThread {
    finalization_queue: Mutex<FinalizerQueue>,
    /// Set of object addresses that have already been finalized once.
    /// Prevents double-finalization from resurrection attacks.
    /// T10.9.B: FxHashSet — object addresses are internal.
    already_finalized: Mutex<FxHashSet<usize>>,
    running: AtomicBool,
    /// Counter of dropped objects due to queue overflow.
    dropped_count: AtomicUsize,
    /// gc-common w3-d: the reference-delivery thread's wake-up channel. See
    /// [`DeliverySignal`]. Per VM (this struct is `SharedVm::mem.finalizer_thread`).
    delivery: Arc<DeliverySignal>,
    /// gc-common w3-d: `false` until one caller wins
    /// [`Self::claim_delivery_thread_start`]; the delivery thread is spawned
    /// at most once per VM.
    delivery_start_claimed: AtomicBool,
    /// gc-common w3-d: the delivery thread's VM thread id PLUS ONE once it is
    /// registered and serving (0 = no delivery thread). Read to answer "is
    /// there a thread to hand work to" and "is the caller that thread".
    delivery_thread: AtomicU64,
    /// gc-common w7-d: a collecting thread that held a user-visible lock
    /// handed this VM's queued GC notifications to the delivery thread
    /// instead of running the listeners itself, and the next batch owes their
    /// delivery. Only the default policy's `--compatible` arm uses it (the
    /// full hand-off and `--jdk-only` deliver every batch's notifications
    /// anyway); it keeps a batch from taking over the notifications a
    /// lock-free door is about to deliver inline, so that door's output is
    /// unchanged. See `gc_notifications_go_to_delivery_thread`
    /// (`vm/src/runtime/interpreter/gc_and_alloc.rs`).
    gc_notifications_handed_off: AtomicBool,
}

impl FinalizerThread {
    pub fn new() -> Self {
        Self {
            finalization_queue: Mutex::new(FinalizerQueue::default()),
            already_finalized: Mutex::new(FxHashSet::default()),
            running: AtomicBool::new(false),
            dropped_count: AtomicUsize::new(0),
            delivery: Arc::new(DeliverySignal::new()),
            delivery_start_claimed: AtomicBool::new(false),
            delivery_thread: AtomicU64::new(0),
            gc_notifications_handed_off: AtomicBool::new(false),
        }
    }

    /// Record that a door handed this VM's queued GC notifications to the
    /// delivery thread (gc-common w7-d). Call BEFORE requesting the batch: the
    /// batch reads the mark after `DeliverySignal::begin_batch`, which takes
    /// the signal's lock, so a batch opened for that request always sees it.
    pub fn note_gc_notifications_handed_off(&self) {
        self.gc_notifications_handed_off.store(true, Ordering::Release);
    }

    /// Delivery-thread side: take (and lower) the mark
    /// [`Self::note_gc_notifications_handed_off`] raised. `true` when a door
    /// handed notifications over since the last take.
    pub fn take_gc_notifications_handed_off(&self) -> bool {
        self.gc_notifications_handed_off.swap(false, Ordering::AcqRel)
    }

    /// The delivery thread's wake-up channel (gc-common w3-d).
    pub fn delivery_signal(&self) -> Arc<DeliverySignal> {
        Arc::clone(&self.delivery)
    }

    /// Ask the delivery thread for a batch. Always recorded — a thread that
    /// is still starting serves it with its first batch — and answers whether
    /// a delivery thread is already serving.
    pub fn request_delivery(&self) -> bool {
        self.delivery.request();
        self.delivery_thread.load(Ordering::Acquire) != 0
    }

    /// `true` for exactly one caller per VM: the one that must spawn the
    /// delivery thread. A caller whose spawn then fails hands the claim back
    /// with [`Self::release_delivery_thread_start`].
    pub fn claim_delivery_thread_start(&self) -> bool {
        self.delivery_start_claimed
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    }

    /// Undo a [`Self::claim_delivery_thread_start`] whose spawn failed.
    pub fn release_delivery_thread_start(&self) {
        self.delivery_start_claimed.store(false, Ordering::Release);
    }

    /// Whether a delivery thread has ever been claimed for this VM (it may
    /// still be starting).
    pub fn delivery_thread_claimed(&self) -> bool {
        self.delivery_start_claimed.load(Ordering::Acquire)
    }

    /// Published by the delivery thread once it can serve: its VM thread id.
    pub fn set_delivery_thread(&self, thread_id: u64) {
        self.delivery_thread
            .store(thread_id.saturating_add(1), Ordering::Release);
    }

    /// The serving delivery thread's VM thread id, if any.
    pub fn delivery_thread(&self) -> Option<u64> {
        match self.delivery_thread.load(Ordering::Acquire) {
            0 => None,
            n => Some(n - 1),
        }
    }

    /// Enqueue an object for finalization. Returns `false` if the object has
    /// already been finalized (resurrection), is already waiting in the queue,
    /// or the queue is full.
    pub fn enqueue(&self, obj_addr: usize) -> bool {
        // Check resurrection: skip if already finalized once
        if self.already_finalized.lock().contains(&obj_addr) {
            return false;
        }
        let mut queue = self.finalization_queue.lock();
        // Already waiting to run. `finalize()` must happen at most once per
        // object (JLS §12.6), and callers legitimately offer the same address
        // more than once: `finalizable_roots` hands the collector this queue's
        // own contents (`pending_addresses`) so that a deferred entry stays
        // rooted, and a collector that reports unreachable finalizable
        // candidates then reports those queued objects right back — correctly,
        // since they ARE unreachable. Without this guard every collection
        // between an enqueue and its `run_finalizers` appends the object again,
        // and `finalize()` runs once per GC the object waited through.
        //
        // Keying on the address is sound HERE, unlike `already_finalized`
        // above: an entry in this queue is handed to the collector as a
        // finalizable root, so its object cannot have been reclaimed and its
        // address cannot have been recycled while it sits here. The
        // `already_finalized` set has no such protection — it outlives its
        // object, which is why it is a genuine address-reuse hazard and this
        // is not.
        if queue.contains(obj_addr) {
            return false;
        }
        if queue.len() >= FINALIZER_QUEUE_MAX_CAPACITY {
            self.dropped_count.fetch_add(1, Ordering::Relaxed);
            tracing::warn!(
                "Finalizer queue at capacity ({}), dropping object 0x{:x}",
                FINALIZER_QUEUE_MAX_CAPACITY,
                obj_addr
            );
            return false;
        }
        queue.push(obj_addr);
        true
    }

    /// Enqueue an object the [`ReferenceProcessor`] has just reported from a
    /// finalizer row that was NEVER enqueued before — `to_finalize`, the
    /// processor's own finalization queue, or the resurrection channel's
    /// `dead_finalizers` (which the processor's `finalizer_referent_addresses`
    /// fed). Returns `false` only for a duplicate already waiting in the queue.
    ///
    /// # Never dropped at capacity (gc-common w4-d, 2026-09-23)
    ///
    /// Unlike [`Self::enqueue`], this does not refuse past
    /// `FINALIZER_QUEUE_MAX_CAPACITY`. Every caller has already flagged (or is
    /// about to flag) the object's processor row `enqueued`
    /// (`mark_finalizer_enqueued`, `to_finalize`), so a refusal here was not a
    /// deferral: `finalizer_referent_addresses` never offers the row again, the
    /// object — rooted only by this queue — is freed at the next collection, and
    /// its `finalize()` silently never runs. HotSpot's finalizer queue has no
    /// cap: a program producing finalizable garbage faster than the finalizer
    /// drains it keeps those objects alive and ends in `OutOfMemoryError`, not
    /// in skipped finalizers. The queue entries themselves are 8 bytes plus an
    /// index slot; the objects they root are what costs memory, as there. With
    /// the reference-delivery thread draining asynchronously (w3-d / w4-d) the
    /// backlog can now legitimately exceed the old cap, so the drop went from
    /// theoretical to reachable. Crossing the old cap is still logged, once
    /// per crossing.
    ///
    /// # Why [`Self::enqueue`] is wrong for these (gc-common w1-d, 2026-09-23)
    ///
    /// `enqueue` refuses any address in `already_finalized`, a set keyed by the
    /// ADDRESS a finalized object had when it was dequeued. Nothing in
    /// production ever pruned it (`cleanup_collected` had no caller) or
    /// relocated it, so it grew by one entry per object ever finalized, and —
    /// the defect — once a finalized object died, a NEW finalizable object
    /// allocated at the same address had its `finalize()` refused, silently and
    /// forever: the processor flags its row `enqueued` as it reports it, so
    /// nothing ever offers it again. Address reuse is the normal case under a
    /// free-list sweep and under bump allocation into a recycled G1 region.
    ///
    /// The refusal was never needed on this path: exactly-once finalization is
    /// already enforced where the identity is exact — per processor ROW, whose
    /// `enqueued` flag is set as the row is reported and never cleared, and a
    /// row is created once per allocation (`register_finalizable`). An address
    /// reported from a never-enqueued row therefore names an object that has
    /// not been finalized, and this also forgets the address from
    /// `already_finalized` so a stale entry cannot refuse it later.
    pub fn enqueue_unfinalized(&self, obj_addr: usize) -> bool {
        self.already_finalized.lock().remove(&obj_addr);
        let mut queue = self.finalization_queue.lock();
        if queue.contains(obj_addr) {
            return false;
        }
        if queue.len() == FINALIZER_QUEUE_MAX_CAPACITY {
            tracing::warn!(
                "Finalizer queue passed {} pending objects; finalization is falling \
                 behind allocation (queued, not dropped)",
                FINALIZER_QUEUE_MAX_CAPACITY
            );
        }
        queue.push(obj_addr);
        true
    }

    /// Dequeue the next object for finalization, marking it as finalized.
    pub fn dequeue(&self) -> Option<usize> {
        let addr = self.finalization_queue.lock().pop()?;
        // Mark as finalized — prevents double-finalization on resurrection
        self.already_finalized.lock().insert(addr);
        Some(addr)
    }

    /// Check if an object has already been finalized.
    pub fn was_finalized(&self, obj_addr: usize) -> bool {
        self.already_finalized.lock().contains(&obj_addr)
    }

    /// Relocate every queued finalizable object address through a GC pointer
    /// map. Like cleaner actions, finalizers can be deferred across GC cycles
    /// (a `finalize()` invoked while a JIT borrow is live would alias the
    /// `&mut JvmThread`), so a persisted queue entry must track object motion
    /// or `dequeue` would later hand the interpreter a freed/moved address.
    pub fn update_after_gc(&self, pointer_map: &cratonvm_types::PointerMap) {
        if pointer_map.is_empty() {
            return;
        }
        self.finalization_queue
            .lock()
            .remap(|addr| pointer_map.get(&addr).copied().unwrap_or(addr));
        // gc-common w1-d: `already_finalized` is address-keyed too, and a
        // finalized-then-resurrected object that MOVES would otherwise leave
        // its old address behind (refusing whatever is allocated there next)
        // and lose its own entry. Rebuilt, because a key can be another key's
        // target.
        let mut done = self.already_finalized.lock();
        if !done.is_empty() {
            let moved: FxHashSet<usize> = done
                .iter()
                .map(|a| match pointer_map.get(a) {
                    Some(&new) if new != 0 => new,
                    _ => *a,
                })
                .collect();
            *done = moved;
        }
    }

    pub fn pending_count(&self) -> usize {
        self.finalization_queue.lock().len()
    }

    /// Addresses of every object still waiting for its `finalize()` to run.
    ///
    /// These MUST be handed to the collector as finalizable roots. Once
    /// `ReferenceProcessor::mark_finalizer_enqueued` flags an entry,
    /// `finalizer_referent_addresses` stops reporting it (that flag exists to
    /// stop the resurrection channel re-enqueuing the same object on every
    /// later cycle), so from that moment the only thing still referring to the
    /// object is this queue — and it holds a *raw address*, which no marker
    /// can see. `update_after_gc` already covers a moving collection, but a
    /// non-moving young mark-sweep just frees the now-unreachable object and
    /// leaves the queued address dangling; `run_finalizers` then dereferences
    /// it (`heap.class_id_of`) and faults. Keeping them alive is what JLS
    /// §12.6 requires anyway: `finalize()` runs *on* the object and may even
    /// resurrect it.
    pub fn pending_addresses(&self) -> Vec<usize> {
        self.finalization_queue.lock().addresses()
    }

    pub fn dropped_count(&self) -> usize {
        self.dropped_count.load(Ordering::Relaxed)
    }

    /// Get the per-finalizer timeout in milliseconds.
    pub fn timeout_ms(&self) -> u64 {
        FINALIZER_TIMEOUT_MS
    }

    pub fn is_running(&self) -> bool {
        self.running.load(Ordering::Acquire)
    }

    pub fn start(&self) {
        self.running.store(true, Ordering::Release);
    }

    pub fn stop(&self) {
        self.running.store(false, Ordering::Release);
    }

    /// Clean up finalization tracking for objects that have been GC'd.
    pub fn cleanup_collected(&self, is_live: &dyn Fn(usize) -> bool) {
        self.already_finalized.lock().retain(|addr| is_live(*addr));
    }
}

impl Default for FinalizerThread {
    fn default() -> Self {
        Self::new()
    }
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    // Helpers ---------------------------------------------------------------

    fn always_dead(_addr: usize) -> bool {
        false
    }
    fn always_live(_addr: usize) -> bool {
        true
    }
    fn live_set(set: &[usize]) -> impl Fn(usize) -> bool + '_ {
        move |addr| set.contains(&addr)
    }

    // gengc-mark 2026-09-20 regressions ------------------------------------

    // ORCHESTRATOR REMOVAL, 2026-09-20 — a test asserting the inverse of three
    // older ones stood here: `a_weak_ref_is_cleared_even_when_a_kept_soft_ref_
    // names_the_same_dead_object`. It set up a policy-KEPT soft entry under an
    // `always_dead` mark predicate with a weak reference to the same referent —
    // byte for byte the setup of tests 62, 63 and
    // `cleaner_not_fired_while_referent_softly_reachable` — and asserted the
    // weak reference MUST be cleared, where those three assert it must NOT be.
    // Both cannot hold, and the older three have the JLS behind them: a soft
    // reference the policy declines to clear leaves its referent SOFTLY
    // REACHABLE, and a weak reference is cleared only once its referent is
    // weakly reachable, i.e. neither strongly nor softly. The Phase 1 policy
    // decision outranks `is_marked` for the rest of the cycle; that is what the
    // soft-reachable closure is for.
    //
    // It was added alongside the `soft_is_live(e.referent)` screen it encoded,
    // and was removed with it. The narrower hazard that screen was reaching for
    // is real but keyed on the SoftReference INSTANCE rather than the referent,
    // and is filed as
    // `docs/internal/gc/gengc-mark-dead-softref-object-still-roots-weak-closure-FIXED-20260923.md`.
    // A test for that gap needs an unmarked `reference_obj`, which this one did
    // not have (it passed `None`).

    /// A weak reference to a referent a soft reference genuinely KEEPS ALIVE
    /// must still be retained — the fix above must not over-clear.
    #[test]
    fn a_weak_ref_to_a_live_soft_referent_is_retained() {
        const REFERENT: usize = 0x100;
        let mut proc = ReferenceProcessor::new();
        proc.discover_reference(ReferenceType::Soft, 0x10, REFERENT, None);
        proc.discover_reference(ReferenceType::Weak, 0x20, REFERENT, Some(0x900));

        // The referent IS marked: strongly/softly reachable, so the JLS
        // strength ordering forbids clearing the weak reference.
        let live = [REFERENT];
        let _ = proc.process_references(&live_set(&live), 1024, 1_000_000);

        assert_eq!(
            proc.stats().weak_refs_cleared,
            0,
            "strong > soft > weak: a live soft referent keeps the weak ref intact",
        );
    }

    /// The condemned-soft set is address-keyed and must follow a compacting
    /// collection, exactly like the identity and referent-class stamp tables.
    ///
    /// Left stale it can collide with the NEW address of a different soft
    /// entry, which then takes the condemned branch in `process_soft_refs` —
    /// a branch that skips the idle check entirely. That clears a
    /// `SoftReference` with no memory pressure at all.
    #[test]
    fn update_after_gc_relocates_the_condemned_soft_set() {
        let mut proc = ReferenceProcessor::new();
        proc.discover_reference(ReferenceType::Soft, 0x10, 0x100, None);

        // `free_heap_mb == 0` makes the threshold 0, so the entry (timestamp 0,
        // "now" 5000) is condemned.
        let condemned = proc.condemn_idle_soft_refs(0, 5_000);
        assert_eq!(condemned, vec![(0x10, 0x100)]);

        // A compacting collection slides both the Reference and its referent.
        let mut map = cratonvm_types::PointerMap::default();
        map.insert(0x10, 0x30);
        map.insert(0x100, 0x300);
        proc.update_after_gc(&map);

        assert_eq!(
            proc.soft_pre_nulled_active_pairs(),
            vec![(0x30, 0x300)],
            "the post-GC restore pass must still recognise the entry whose slot \
             the pre-GC pass nulled",
        );
    }

    // RETIRED 2026-09-21 with `ReferenceQueue` itself (see the retirement
    // note above `ReferenceProcessor`): `remove_timeout_still_honours_its_deadline`
    // asserted that an empty queue's wait lasts ~40 ms. It could not be
    // ported. The only honest implementation of a wait on a `&mut self`
    // queue is to return at once — nothing in the process can enqueue while
    // the borrow is held — so the elapsed time this test pinned WAS the
    // defect, stated as an expectation.

    // 1. Discover soft reference -------------------------------------------
    #[test]
    fn discover_soft_ref() {
        let mut proc = ReferenceProcessor::new();
        proc.discover_reference(ReferenceType::Soft, 100, 200, Some(300));
        assert_eq!(proc.soft_refs.len(), 1);
        assert_eq!(proc.soft_refs[0].referent, 200);
    }

    // 2. Discover weak reference -------------------------------------------
    #[test]
    fn discover_weak_ref() {
        let mut proc = ReferenceProcessor::new();
        proc.discover_reference(ReferenceType::Weak, 100, 200, None);
        assert_eq!(proc.weak_refs.len(), 1);
    }

    // 3. Discover phantom reference ----------------------------------------
    #[test]
    fn discover_phantom_ref() {
        let mut proc = ReferenceProcessor::new();
        proc.discover_reference(ReferenceType::Phantom, 100, 200, Some(300));
        assert_eq!(proc.phantom_refs.len(), 1);
    }

    // 4. Discover cleaner reference ----------------------------------------
    #[test]
    fn discover_cleaner_ref() {
        let mut proc = ReferenceProcessor::new();
        proc.discover_reference(ReferenceType::Cleaner, 100, 200, None);
        assert_eq!(proc.cleaner_refs.len(), 1);
    }

    // 5. Discover finalizer reference --------------------------------------
    #[test]
    fn discover_finalizer_ref() {
        let mut proc = ReferenceProcessor::new();
        proc.discover_reference(ReferenceType::Finalizer, 100, 200, Some(400));
        assert_eq!(proc.finalizer_refs.len(), 1);
    }

    // 6. Strong references are not tracked ---------------------------------
    #[test]
    fn strong_refs_not_tracked() {
        let mut proc = ReferenceProcessor::new();
        proc.discover_reference(ReferenceType::Strong, 100, 200, None);
        assert!(proc.soft_refs.is_empty());
        assert!(proc.weak_refs.is_empty());
        assert!(proc.phantom_refs.is_empty());
    }

    // 7. SoftRef cleared when time exceeds threshold -----------------------
    #[test]
    fn soft_ref_cleared_when_old() {
        let mut proc = ReferenceProcessor::new_with_policy(1000);
        proc.discover_reference(ReferenceType::Soft, 100, 200, Some(300));
        // last_access_time_ms = 0, current = 5000, free = 2 MB
        // threshold = 1000 * 2 = 2000; idle = 5000 > 2000 => clear
        let result = proc.process_references(&always_dead, 2, 5000);
        assert_eq!(result.stats.soft_refs_cleared, 1);
        assert!(proc.soft_refs[0].cleared);
    }

    // 8. SoftRef kept when recently accessed --------------------------------
    #[test]
    fn soft_ref_kept_when_recent() {
        let mut proc = ReferenceProcessor::new_with_policy(1000);
        proc.discover_reference(ReferenceType::Soft, 100, 200, Some(300));
        proc.soft_refs[0].last_access_time_ms = 4500;
        // threshold = 1000 * 10 = 10000; idle = 5000 - 4500 = 500 < 10000
        let result = proc.process_references(&always_dead, 10, 5000);
        assert_eq!(result.stats.soft_refs_cleared, 0);
    }

    // 9. SoftRef kept when referent is live --------------------------------
    #[test]
    fn soft_ref_kept_when_referent_live() {
        let mut proc = ReferenceProcessor::new_with_policy(1000);
        proc.discover_reference(ReferenceType::Soft, 100, 200, Some(300));
        let result = proc.process_references(&always_live, 0, 99999);
        assert_eq!(result.stats.soft_refs_cleared, 0);
    }

    // 10. WeakRef cleared when referent unreachable -------------------------
    #[test]
    fn weak_ref_cleared_when_dead() {
        let mut proc = ReferenceProcessor::new();
        proc.discover_reference(ReferenceType::Weak, 100, 200, Some(300));
        let result = proc.process_references(&always_dead, 100, 0);
        assert_eq!(result.stats.weak_refs_cleared, 1);
        assert!(proc.weak_refs[0].cleared);
    }

    // 11. WeakRef kept when referent reachable ------------------------------
    #[test]
    fn weak_ref_kept_when_live() {
        let mut proc = ReferenceProcessor::new();
        proc.discover_reference(ReferenceType::Weak, 100, 200, None);
        let result = proc.process_references(&always_live, 100, 0);
        assert_eq!(result.stats.weak_refs_cleared, 0);
    }

    // 12. PhantomRef enqueued when referent unreachable ---------------------
    #[test]
    fn phantom_ref_enqueued_when_dead() {
        let mut proc = ReferenceProcessor::new();
        proc.discover_reference(ReferenceType::Phantom, 100, 200, Some(300));
        let result = proc.process_references(&always_dead, 100, 0);
        assert_eq!(result.stats.phantom_refs_enqueued, 1);
        assert!(proc.phantom_refs[0].enqueued);
        assert_eq!(result.to_enqueue.len(), 1);
        assert_eq!(result.to_enqueue[0], (100, 300));
    }

    // 13. PhantomRef referent NOT cleared (Java 9+ semantics) --------------
    #[test]
    fn phantom_ref_not_cleared() {
        let mut proc = ReferenceProcessor::new();
        proc.discover_reference(ReferenceType::Phantom, 100, 200, Some(300));
        let _result = proc.process_references(&always_dead, 100, 0);
        assert!(!proc.phantom_refs[0].cleared);
    }

    // 14. CleanerThread submit and drain -----------------------------------
    #[test]
    fn cleaner_submit_and_drain() {
        let ct = CleanerThread::new();
        ct.submit_action(0xA000);
        ct.submit_action(0xB000);
        assert_eq!(ct.pending_count(), 2);
        let drained = ct.drain_actions();
        assert_eq!(drained, vec![0xA000, 0xB000]);
        assert_eq!(ct.pending_count(), 0);
    }

    // 15. FinalizerThread enqueue and dequeue (FIFO) -----------------------
    #[test]
    fn finalizer_enqueue_dequeue_fifo() {
        let ft = FinalizerThread::new();
        ft.enqueue(1);
        ft.enqueue(2);
        ft.enqueue(3);
        assert_eq!(ft.pending_count(), 3);
        assert_eq!(ft.dequeue(), Some(1));
        assert_eq!(ft.dequeue(), Some(2));
        assert_eq!(ft.dequeue(), Some(3));
        assert_eq!(ft.dequeue(), None);
    }

    /// An object already waiting in the queue must not be enqueued again.
    ///
    /// `finalizable_roots` hands the collector this queue's own contents so a
    /// deferred entry stays rooted, and the collector then reports those
    /// (genuinely unreachable) objects back as dead on every cycle — so without
    /// the guard, `finalize()` runs once per GC the object waited through.
    #[test]
    fn a_queued_object_is_not_enqueued_twice() {
        let ft = FinalizerThread::new();
        assert!(ft.enqueue(0xBEEF), "first enqueue is accepted");
        assert!(
            !ft.enqueue(0xBEEF),
            "a second enqueue while queued is refused"
        );
        assert!(!ft.enqueue(0xBEEF), "and stays refused");
        assert_eq!(ft.pending_count(), 1);

        // Once it has actually run, the `already_finalized` guard takes over.
        assert_eq!(ft.dequeue(), Some(0xBEEF));
        assert_eq!(ft.pending_count(), 0);
        assert!(
            !ft.enqueue(0xBEEF),
            "an object that has already been finalized is never re-enqueued"
        );
    }

    /// The membership index must survive a relocating collection: after
    /// `update_after_gc` rewrites an entry, the NEW address is the one that
    /// must be refused and the OLD one must no longer be indexed.
    #[test]
    fn the_queue_index_follows_a_relocation() {
        let ft = FinalizerThread::new();
        ft.enqueue(0x1000);

        let mut map = cratonvm_types::PointerMap::default();
        map.insert(0x1000, 0x2000);
        ft.update_after_gc(&map);

        assert_eq!(ft.pending_addresses(), vec![0x2000]);
        assert!(
            !ft.enqueue(0x2000),
            "the relocated address is the one now queued, and must be refused"
        );
        // The vacated address is no longer queued, so an object that legitimately
        // lands there later is still allowed in.
        assert!(
            ft.enqueue(0x1000),
            "the pre-relocation address must not stay indexed"
        );
        assert_eq!(ft.pending_count(), 2);
    }

    // 16, 17. RETIRED 2026-09-21 with `ReferenceQueue`:
    // `reference_queue_enqueue_poll` and `reference_queue_capacity_evicts_oldest`.
    // The second was the test that PINNED the evict-the-head overflow policy
    // (survivors `[2, 3]`, and `true` returned from the `enqueue` that had
    // just destroyed a different entry), which is why that policy could never
    // be fixed as a drive-by. Both went with the type.

    // 18. update_after_gc relocates addresses ------------------------------
    #[test]
    fn update_after_gc_relocates() {
        let mut proc = ReferenceProcessor::new();
        proc.discover_reference(ReferenceType::Weak, 100, 200, Some(300));
        let mut map = cratonvm_types::PointerMap::default();
        map.insert(100, 1100);
        map.insert(200, 1200);
        map.insert(300, 1300);
        proc.update_after_gc(&map);
        assert_eq!(proc.weak_refs[0].reference_obj, 1100);
        assert_eq!(proc.weak_refs[0].referent, 1200);
        assert_eq!(proc.weak_refs[0].queue_addr, Some(1300));
    }

    // 19. remove_collected cleans up dead Reference objects -----------------
    #[test]
    fn all_tracked_addrs_covers_every_reference_kind() {
        let mut proc = ReferenceProcessor::new();
        proc.discover_reference(ReferenceType::Soft, 100, 200, None);
        proc.discover_reference(ReferenceType::Weak, 101, 201, Some(301));
        proc.discover_reference(ReferenceType::Phantom, 102, 202, None);
        proc.discover_reference(ReferenceType::Cleaner, 103, 203, None);
        proc.discover_reference(ReferenceType::Finalizer, 104, 204, None);

        let addrs = proc.all_tracked_addrs();
        // Soft / cleaner / finalizer entries were NOT published before the
        // old-gen-reclamation fix, so the collector emitted no survival proof
        // for them and the exact post-GC predicate could not be used.
        for a in [100, 200, 101, 201, 301, 102, 202, 103, 203, 104, 204] {
            assert!(
                addrs.contains(&a),
                "address {a} must be published as watched"
            );
        }
    }

    #[test]
    fn remove_collected_cleans_up() {
        let mut proc = ReferenceProcessor::new();
        proc.discover_reference(ReferenceType::Weak, 100, 200, None);
        proc.discover_reference(ReferenceType::Weak, 101, 201, None);
        let live = [101usize];
        proc.remove_collected(&live_set(&live));
        assert_eq!(proc.weak_refs.len(), 1);
        assert_eq!(proc.weak_refs[0].reference_obj, 101);
    }

    // 19b. HIB-WEAKREF-RECYCLE.1 — the shape prune drops weak/phantom entries
    //      whose Reference object has been recycled, and leaves the
    //      finalizer/cleaner lists (whose reference_obj is an arbitrary object)
    //      completely alone.
    #[test]
    fn retain_shaped_weak_phantom_prunes_recycled_and_spares_finalizers() {
        let mut proc = ReferenceProcessor::new();
        proc.discover_reference(ReferenceType::Weak, 100, 200, None);
        proc.discover_reference(ReferenceType::Weak, 101, 201, None);
        proc.discover_reference(ReferenceType::Phantom, 102, 202, Some(302));
        // A finalizable object with a shape the weak/phantom test would reject.
        proc.discover_reference(ReferenceType::Finalizer, 103, 203, None);
        proc.discover_reference(ReferenceType::Cleaner, 104, 204, None);

        // 100 and 102 have been reclaimed and their memory recycled: whatever
        // occupies those addresses no longer has >= 2 instance fields.
        let still_a_reference = [101usize];
        proc.retain_shaped_weak_phantom(&live_set(&still_a_reference));

        assert_eq!(proc.weak_refs.len(), 1);
        assert_eq!(proc.weak_refs[0].reference_obj, 101);
        assert!(proc.phantom_refs.is_empty());
        // Untouched — pruning these on a field-count test would drop live
        // finalizable objects that legitimately declare fewer than two fields.
        assert_eq!(proc.finalizer_refs.len(), 1);
        assert_eq!(proc.cleaner_refs.len(), 1);
    }

    // 20. Stats tracking ---------------------------------------------------
    #[test]
    fn stats_tracking() {
        let mut proc = ReferenceProcessor::new();
        proc.discover_reference(ReferenceType::Weak, 10, 20, None);
        proc.discover_reference(ReferenceType::Weak, 11, 21, None);
        proc.discover_reference(ReferenceType::Phantom, 30, 40, Some(50));
        let result = proc.process_references(&always_dead, 100, 0);
        assert_eq!(result.stats.weak_refs_discovered, 2);
        assert_eq!(result.stats.weak_refs_cleared, 2);
        assert_eq!(result.stats.phantom_refs_discovered, 1);
        assert_eq!(result.stats.phantom_refs_enqueued, 1);
    }

    // 21. Multiple reference types in single processing --------------------
    #[test]
    fn mixed_reference_types() {
        let mut proc = ReferenceProcessor::new_with_policy(1000);
        proc.discover_reference(ReferenceType::Soft, 1, 2, Some(500));
        proc.discover_reference(ReferenceType::Weak, 3, 4, Some(500));
        proc.discover_reference(ReferenceType::Phantom, 5, 6, Some(600));
        proc.discover_reference(ReferenceType::Finalizer, 7, 8, Some(700));
        proc.discover_reference(ReferenceType::Cleaner, 9, 10, None);

        // All referents dead, soft ref old enough to clear
        let result = proc.process_references(&always_dead, 1, 5000);
        assert_eq!(result.stats.soft_refs_cleared, 1);
        assert_eq!(result.stats.weak_refs_cleared, 1);
        assert_eq!(result.stats.phantom_refs_enqueued, 1);
        assert_eq!(result.stats.finalizer_refs_enqueued, 1);
        assert_eq!(result.stats.cleaner_refs_processed, 1);
    }

    // 22. Policy configuration change --------------------------------------
    #[test]
    fn policy_configuration() {
        let mut proc = ReferenceProcessor::new();
        proc.set_soft_ref_lru_policy(500);
        proc.discover_reference(ReferenceType::Soft, 1, 2, None);
        // threshold = 500 * 4 = 2000; idle = 2500 > 2000 => clear
        let result = proc.process_references(&always_dead, 4, 2500);
        assert_eq!(result.stats.soft_refs_cleared, 1);
    }

    // 23. Empty processor returns empty result -----------------------------
    #[test]
    fn empty_processor() {
        let mut proc = ReferenceProcessor::new();
        let result = proc.process_references(&always_dead, 100, 0);
        assert!(result.to_enqueue.is_empty());
        assert!(result.to_finalize.is_empty());
        assert!(result.cleaner_actions.is_empty());
    }

    // 24. to_enqueue contains correct pairs --------------------------------
    #[test]
    fn to_enqueue_pairs() {
        let mut proc = ReferenceProcessor::new();
        proc.discover_reference(ReferenceType::Weak, 10, 20, Some(500));
        proc.discover_reference(ReferenceType::Weak, 11, 21, Some(600));
        let result = proc.process_references(&always_dead, 100, 0);
        assert_eq!(result.to_enqueue.len(), 2);
        // Both should be present (order may depend on HashMap iteration)
        let has_10 = result.to_enqueue.iter().any(|&(r, q)| r == 10 && q == 500);
        let has_11 = result.to_enqueue.iter().any(|&(r, q)| r == 11 && q == 600);
        assert!(has_10);
        assert!(has_11);
    }

    // 25. Finalization queue ordering (FIFO) -------------------------------
    #[test]
    fn finalization_queue_fifo() {
        let mut proc = ReferenceProcessor::new();
        proc.discover_reference(ReferenceType::Finalizer, 1, 100, None);
        proc.discover_reference(ReferenceType::Finalizer, 2, 200, None);
        proc.discover_reference(ReferenceType::Finalizer, 3, 300, None);
        let result = proc.process_references(&always_dead, 100, 0);
        // Once-only emission (bc math-ec 0x4 fix, 2026-06-10): process_references
        // now DRAINS the dead finalizers into the result in FIFO discovery order
        // rather than leaving them in the internal queue, so the interpreter
        // consumes them exactly once. The internal queue is therefore empty
        // afterwards.
        assert_eq!(result.to_finalize, vec![100, 200, 300]);
        assert_eq!(proc.dequeue_for_finalization(), None);
    }

    // 26. Reset stats works ------------------------------------------------
    #[test]
    fn reset_stats() {
        let mut proc = ReferenceProcessor::new();
        proc.discover_reference(ReferenceType::Weak, 1, 2, None);
        let _result = proc.process_references(&always_dead, 100, 0);
        assert_eq!(proc.stats().weak_refs_cleared, 1);
        proc.reset_stats();
        assert_eq!(proc.stats().weak_refs_cleared, 0);
    }

    // 27. Weak ref not enqueued without queue ------------------------------
    #[test]
    fn weak_ref_no_queue_no_enqueue() {
        let mut proc = ReferenceProcessor::new();
        proc.discover_reference(ReferenceType::Weak, 10, 20, None);
        let result = proc.process_references(&always_dead, 100, 0);
        assert!(result.to_enqueue.is_empty());
        assert!(proc.weak_refs[0].cleared);
        assert!(!proc.weak_refs[0].enqueued);
    }

    // 28. Selective liveness -----------------------------------------------
    #[test]
    fn selective_liveness() {
        let mut proc = ReferenceProcessor::new();
        proc.discover_reference(ReferenceType::Weak, 1, 100, Some(500));
        proc.discover_reference(ReferenceType::Weak, 2, 200, Some(500));
        // Only referent 100 is live
        let live = [100usize];
        let result = proc.process_references(&live_set(&live), 100, 0);
        assert_eq!(result.stats.weak_refs_cleared, 1);
        assert!(!proc.weak_refs[0].cleared); // referent 100 alive
        assert!(proc.weak_refs[1].cleared); // referent 200 dead
    }

    // 28b. SOFT-CLEAR GAP (2026-08-15) -------------------------------------
    //
    // These five pin the fix for the defect the retired
    // `zgc-resourceleakdetector-corpse-read` write-up left as an open
    // question. `process_soft_refs` skips any entry whose referent
    // `is_marked`, and the VM's markers trace a `Reference`'s slot 0 as an
    // ordinary strong edge -- so a soft referent was always marked through its
    // own `SoftReference` and the LRU policy underneath was unreachable on
    // every collector. `condemn_idle_soft_refs` moves the decision ahead of
    // the mark, where the VM can null the slot the way it already does for
    // weak and phantom.

    /// The policy picks out the idle entries and leaves the freshly-read one.
    #[test]
    fn r5w1_hotspot_lru_uses_the_previous_collections_clock_and_free_heap() {
        // Latched from the flag at construction.
        let latched = cratonvm_types::flags::with_thread_overrides(
            &[("CRATONVM_SOFTREF_HOTSPOT_LRU", Some("1"))],
            || ReferenceProcessor::new_with_policy(1000).hotspot_soft_lru,
        );
        assert!(latched, "the flag is latched by the constructor");
        assert!(
            !cratonvm_types::flags::with_thread_overrides(
                &[("CRATONVM_SOFTREF_HOTSPOT_LRU", None)],
                || ReferenceProcessor::new_with_policy(1000).hotspot_soft_lru,
            ),
            "unset: the legacy inputs"
        );
        // gen r5w4/defaults8: the VM's constructor latches the per-backend
        // default for its heap — HotSpot's inputs on the Generational heap,
        // the legacy ones elsewhere — and an explicit value wins on both.
        let for_heap = |v: Option<&str>, generational: bool| {
            cratonvm_types::flags::with_thread_overrides(
                &[("CRATONVM_SOFTREF_HOTSPOT_LRU", v)],
                || ReferenceProcessor::new_for_heap(None, generational).hotspot_soft_lru,
            )
        };
        assert!(for_heap(None, true), "unset, Generational: HotSpot's inputs");
        assert!(!for_heap(None, false), "unset, G1/ZGC: the legacy inputs");
        assert!(for_heap(Some("1"), false), "explicit ON wins on G1/ZGC");
        assert!(!for_heap(Some("0"), true), "explicit OFF wins on Generational");
        // The `-XX:SoftRefLRUPolicyMSPerMB` half is `new_with_policy`'s.
        assert_eq!(
            ReferenceProcessor::new_for_heap(Some(7), true).soft_ref_lru_policy_ms_per_mb,
            7
        );
        assert_eq!(
            ReferenceProcessor::new_for_heap(None, false).soft_ref_lru_policy_ms_per_mb,
            ReferenceProcessor::new().soft_ref_lru_policy_ms_per_mb
        );

        let mut proc = ReferenceProcessor::new_with_policy(1000);
        proc.hotspot_soft_lru = true;
        proc.discover_reference(ReferenceType::Soft, 10, 100, None);
        proc.touch_soft_reference(10, 1_000);

        // Before any collection the clock is 0: nothing is idle, whatever the
        // wall clock and the pre-collection free space say (the legacy inputs
        // condemn it: 4 s idle against a 0 ms threshold).
        assert!(proc.condemn_idle_soft_refs(0, 5_000).is_empty());
        // That collection's processing round ends: its "now" (5 000) becomes
        // the clock, and the free heap it was handed (0 MB) the next budget.
        let _ = proc.process_references(&always_live, 0, 0);
        assert_eq!(proc.soft_gc_clock_ms, 5_000);
        assert_eq!(proc.soft_free_mb_at_last_gc, Some(0));
        proc.finish_pre_gc_cycle();

        // The next collection: idle = 5 000 - 1 000 = 4 s > 0 ms (0 MB free
        // after the last collection) — condemned, although the caller now
        // reports 1 GB free before this one.
        assert_eq!(proc.condemn_idle_soft_refs(1024, 9_000), vec![(10, 100)]);
        let _ = proc.process_references(&always_live, 1024, 0);
        proc.finish_pre_gc_cycle();
        assert_eq!(proc.soft_gc_clock_ms, 9_000);

        // Read since the last collection (9 000): idle 0, never condemned by
        // the policy even with no room left.
        proc.touch_soft_reference(10, 9_500);
        let _ = proc.process_references(&always_live, 0, 0);
        proc.finish_pre_gc_cycle();
        assert!(proc.condemn_idle_soft_refs(0, 20_000).is_empty());
        // ...and with 1 GB free after the last collection an unread
        // reference is kept for ~1 024 s of clock.
        let mut roomy = ReferenceProcessor::new_with_policy(1000);
        roomy.hotspot_soft_lru = true;
        roomy.discover_reference(ReferenceType::Soft, 30, 300, None);
        roomy.touch_soft_reference(30, 1_000);
        let _ = roomy.condemn_idle_soft_refs(1024, 2_000);
        let _ = roomy.process_references(&always_live, 1024, 0);
        roomy.finish_pre_gc_cycle();
        assert!(roomy.condemn_idle_soft_refs(0, 500_000).is_empty());
    }

    #[test]
    fn condemn_idle_soft_refs_selects_only_the_idle_entries() {
        let mut proc = ReferenceProcessor::new_with_policy(1000);
        proc.discover_reference(ReferenceType::Soft, 10, 100, Some(900));
        proc.discover_reference(ReferenceType::Soft, 20, 200, None);
        proc.touch_soft_reference(10, 1_000);
        proc.touch_soft_reference(20, 61_000);
        // 1 MB allocatable => a 1000 ms idle threshold; "now" is the mutator
        // clock the processor observed (61_000), so entry 10 is 60 s idle and
        // entry 20 is 0 s idle.
        assert_eq!(proc.condemn_idle_soft_refs(1, 0), vec![(10, 100)]);
    }

    /// A roomy heap condemns nothing -- the LRU threshold scales with free
    /// megabytes, so the same 60 s idle window is nowhere near it.
    #[test]
    fn condemn_idle_soft_refs_condemns_nothing_when_the_heap_is_roomy() {
        let mut proc = ReferenceProcessor::new_with_policy(1000);
        proc.discover_reference(ReferenceType::Soft, 10, 100, None);
        proc.discover_reference(ReferenceType::Soft, 20, 200, None);
        proc.touch_soft_reference(10, 1_000);
        proc.touch_soft_reference(20, 61_000);
        assert!(proc.condemn_idle_soft_refs(1024, 0).is_empty());
        assert!(proc.soft_pre_nulled_active_pairs().is_empty());
    }

    /// The load-bearing one. The pre-mark verdict has to STAND even though
    /// re-deriving it after the collection would say "keep": the collection
    /// freed memory, so the post-GC threshold is larger, and the BTreeMap
    /// range the ordinary scan walks is derived from that same larger
    /// threshold -- here it selects no candidates at all. Without the
    /// pre-condemned pass the referent is gone (its slot was nulled before the
    /// mark) while the entry still reads as active, holding a dead address.
    #[test]
    fn a_pre_condemned_soft_ref_is_cleared_even_when_the_post_gc_threshold_would_keep_it() {
        let mut proc = ReferenceProcessor::new_with_policy(1000);
        proc.discover_reference(ReferenceType::Soft, 10, 100, Some(900));
        proc.discover_reference(ReferenceType::Soft, 20, 200, None);
        proc.touch_soft_reference(10, 1_000);
        proc.touch_soft_reference(20, 61_000);
        assert_eq!(proc.condemn_idle_soft_refs(1, 0), vec![(10, 100)]);

        // 10_000 MB free after the collection => a 10_000_000 ms threshold, so
        // the range scan's cutoff is 0 and it visits nothing.
        let result = proc.process_references(&always_dead, 10_000, 0);
        assert!(
            proc.soft_refs[0].cleared,
            "the pre-mark verdict must stand: this referent is already gone"
        );
        assert!(proc.soft_refs[0].enqueued);
        assert!(result.to_enqueue.contains(&(10, 900)));
        // The entry the policy KEPT is untouched, dead referent or not --
        // nothing nulled its slot, so the marker kept its referent alive and
        // `always_dead` is a fiction for it.
        assert!(!proc.soft_refs[1].cleared);
    }

    /// A condemned entry whose referent turned out to be strongly reachable is
    /// kept, and is reported for the post-GC slot-0 restore.
    #[test]
    fn a_condemned_soft_ref_whose_referent_survived_is_offered_for_restore() {
        let mut proc = ReferenceProcessor::new_with_policy(1000);
        proc.discover_reference(ReferenceType::Soft, 10, 100, None);
        proc.discover_reference(ReferenceType::Soft, 20, 200, None);
        proc.touch_soft_reference(10, 1_000);
        proc.touch_soft_reference(20, 61_000);
        assert_eq!(proc.condemn_idle_soft_refs(1, 0), vec![(10, 100)]);

        let live = [100usize];
        proc.process_references(&live_set(&live), 1, 0);
        assert!(!proc.soft_refs[0].cleared);
        assert_eq!(proc.soft_pre_nulled_active_pairs(), vec![(10, 100)]);
    }

    /// The condemned set describes ONE collection. A later cycle that condemns
    /// nothing must not inherit the previous cycle's verdict -- otherwise an
    /// entry the policy has since decided to keep would be cleared the moment
    /// its referent looked unmarked for any other reason.
    #[test]
    fn condemn_idle_soft_refs_resets_the_condemned_set_each_cycle() {
        let mut proc = ReferenceProcessor::new_with_policy(1000);
        proc.discover_reference(ReferenceType::Soft, 10, 100, None);
        proc.discover_reference(ReferenceType::Soft, 20, 200, None);
        proc.touch_soft_reference(10, 1_000);
        proc.touch_soft_reference(20, 61_000);
        assert_eq!(proc.condemn_idle_soft_refs(1, 0), vec![(10, 100)]);
        // Second cycle, roomy heap: nothing is condemned, so nothing carries
        // over.
        assert!(proc.condemn_idle_soft_refs(1024, 0).is_empty());
        assert!(proc.soft_pre_nulled_active_pairs().is_empty());
        proc.process_references(&always_dead, 1024, 0);
        assert!(
            !proc.soft_refs[0].cleared,
            "no slot was nulled this cycle, so no entry may be force-cleared"
        );
    }

    /// The last-ditch rule clears a reference the LRU policy would keep, which
    /// is the whole point of having it: a program looping on its own
    /// soft-referenced cache re-stamps the LRU clock every iteration, so its
    /// idle window never opens however tight the heap becomes.
    #[test]
    fn the_last_ditch_rule_condemns_a_soft_ref_the_lru_policy_would_keep() {
        let mut proc = ReferenceProcessor::new_with_policy(1000);
        proc.discover_reference(ReferenceType::Soft, 10, 100, Some(900));
        // Read just now: zero idle time, so the LRU policy keeps it even with
        // the heap reporting no free megabytes at all.
        proc.touch_soft_reference(10, 61_000);
        assert!(proc.condemn_idle_soft_refs(0, 61_000).is_empty());

        assert_eq!(proc.condemn_all_soft_refs(), vec![(10, 100)]);
        let result = proc.process_references(&always_dead, 0, 61_000);
        assert!(proc.soft_refs[0].cleared);
        assert!(result.to_enqueue.contains(&(10, 900)));
    }

    /// "Clear all soft references" means all the ones nothing else holds. A
    /// condemned referent that is still strongly reachable was marked anyway
    /// and must survive — otherwise the last-ditch collection would hand the
    /// application a null for an object it can still reach by a strong path.
    #[test]
    fn the_last_ditch_rule_still_keeps_a_strongly_reachable_referent() {
        let mut proc = ReferenceProcessor::new_with_policy(1000);
        proc.discover_reference(ReferenceType::Soft, 10, 100, None);
        proc.discover_reference(ReferenceType::Soft, 20, 200, None);
        proc.touch_soft_reference(10, 61_000);
        proc.touch_soft_reference(20, 61_000);
        assert_eq!(proc.condemn_all_soft_refs(), vec![(10, 100), (20, 200)]);
        let live = [100usize];
        proc.process_references(&live_set(&live), 0, 61_000);
        assert!(
            !proc.soft_refs[0].cleared,
            "referent 100 is strongly reachable"
        );
        assert!(
            proc.soft_refs[1].cleared,
            "referent 200 is only softly reachable"
        );
        assert_eq!(proc.soft_pre_nulled_active_pairs(), vec![(10, 100)]);
    }

    /// The ladder skips its extra collection when there is nothing to clear.
    #[test]
    fn has_active_soft_refs_tracks_the_uncleared_population() {
        let mut proc = ReferenceProcessor::new();
        assert!(!proc.has_active_soft_refs());
        proc.discover_reference(ReferenceType::Weak, 1, 2, None);
        assert!(!proc.has_active_soft_refs(), "a weak ref is not a soft ref");
        proc.discover_reference(ReferenceType::Soft, 10, 100, None);
        assert!(proc.has_active_soft_refs());
        proc.condemn_all_soft_refs();
        proc.process_references(&always_dead, 0, 1);
        assert!(!proc.has_active_soft_refs());
    }

    // 29. CleanerThread start/stop -----------------------------------------
    #[test]
    fn cleaner_thread_lifecycle() {
        let ct = CleanerThread::new();
        assert!(!ct.is_running());
        ct.start();
        assert!(ct.is_running());
        ct.stop();
        assert!(!ct.is_running());
    }

    // 30. FinalizerThread start/stop ---------------------------------------
    #[test]
    fn finalizer_thread_lifecycle() {
        let ft = FinalizerThread::new();
        assert!(!ft.is_running());
        ft.start();
        assert!(ft.is_running());
        ft.stop();
        assert!(!ft.is_running());
    }

    // 31. update_after_gc relocates finalization queue ----------------------
    #[test]
    fn update_after_gc_finalization_queue() {
        let mut proc = ReferenceProcessor::new();
        // process_references now DRAINS the finalization queue into its result
        // (once-only emission, see finalization_queue_fifo), so populate the
        // internal queue directly to isolate update_after_gc's relocation of a
        // still-pending entry (an object moved by a GC between enqueue and
        // consumption).
        proc.finalization_queue.push_back(100);
        let mut map = cratonvm_types::PointerMap::default();
        map.insert(100, 9999);
        proc.update_after_gc(&map);
        assert_eq!(proc.finalization_queue, vec![9999]);
    }

    // 32. Already-cleared entry skipped ------------------------------------
    #[test]
    fn already_cleared_skipped() {
        let mut proc = ReferenceProcessor::new();
        proc.discover_reference(ReferenceType::Weak, 1, 2, Some(500));
        proc.weak_refs[0].cleared = true;
        let result = proc.process_references(&always_dead, 100, 0);
        assert_eq!(result.stats.weak_refs_cleared, 0);
    }

    // 33. Phantom not re-enqueued ------------------------------------------
    #[test]
    fn phantom_not_re_enqueued() {
        let mut proc = ReferenceProcessor::new();
        proc.discover_reference(ReferenceType::Phantom, 1, 2, Some(500));
        let r1 = proc.process_references(&always_dead, 100, 0);
        assert_eq!(r1.stats.phantom_refs_enqueued, 1);
        // Process again -- should not double-enqueue
        let r2 = proc.process_references(&always_dead, 100, 0);
        assert_eq!(r2.stats.phantom_refs_enqueued, 0);
    }

    // 34. Default trait impl -----------------------------------------------
    #[test]
    fn default_impl() {
        let proc = ReferenceProcessor::default();
        assert_eq!(proc.soft_ref_lru_policy_ms_per_mb, 1000);
    }

    // 35. to_finalize populated correctly ----------------------------------
    #[test]
    fn to_finalize_populated() {
        let mut proc = ReferenceProcessor::new();
        proc.discover_reference(ReferenceType::Finalizer, 1, 0xBEEF, None);
        let result = proc.process_references(&always_dead, 100, 0);
        assert_eq!(result.to_finalize, vec![0xBEEF]);
    }

    // ======================================================================
    // M16-M20 fix verification tests
    // ======================================================================

    // 36. FinalizerThread resurrection detection (one-finalization-per-object)
    #[test]
    fn finalizer_resurrection_detection() {
        let ft = FinalizerThread::new();
        // First finalization: enqueue succeeds
        assert!(ft.enqueue(0xDEAD));
        assert_eq!(ft.pending_count(), 1);
        // Dequeue marks it as finalized
        let addr = ft.dequeue().unwrap();
        assert_eq!(addr, 0xDEAD);
        assert!(ft.was_finalized(0xDEAD));
        // Re-enqueue after resurrection: should be rejected
        assert!(!ft.enqueue(0xDEAD));
        assert_eq!(ft.pending_count(), 0);
    }

    // 37. FinalizerThread queue capacity limit
    #[test]
    fn finalizer_queue_capacity_limit() {
        let ft = FinalizerThread::new();
        // Fill to capacity
        for i in 0..FINALIZER_QUEUE_MAX_CAPACITY {
            assert!(ft.enqueue(i + 1), "enqueue #{i} should succeed");
        }
        assert_eq!(ft.pending_count(), FINALIZER_QUEUE_MAX_CAPACITY);
        // One more should be dropped
        assert!(!ft.enqueue(FINALIZER_QUEUE_MAX_CAPACITY + 1));
        assert_eq!(ft.dropped_count(), 1);
        assert_eq!(ft.pending_count(), FINALIZER_QUEUE_MAX_CAPACITY);
    }

    // 38. FinalizerThread was_finalized tracks dequeued objects
    #[test]
    fn finalizer_was_finalized_tracking() {
        let ft = FinalizerThread::new();
        assert!(!ft.was_finalized(100));
        ft.enqueue(100);
        // Still not finalized until dequeued
        assert!(!ft.was_finalized(100));
        ft.dequeue();
        assert!(ft.was_finalized(100));
    }

    // 39. FinalizerThread cleanup_collected removes dead entries from tracking
    #[test]
    fn finalizer_cleanup_collected() {
        let ft = FinalizerThread::new();
        ft.enqueue(100);
        ft.enqueue(200);
        ft.dequeue(); // 100 -> finalized
        ft.dequeue(); // 200 -> finalized
        assert!(ft.was_finalized(100));
        assert!(ft.was_finalized(200));
        // Simulate GC: only 200 is still live
        ft.cleanup_collected(&|addr| addr == 200);
        assert!(!ft.was_finalized(100)); // cleaned up
        assert!(ft.was_finalized(200)); // still tracked
    }

    // 40. FinalizerThread timeout constant
    #[test]
    fn finalizer_timeout_ms() {
        let ft = FinalizerThread::new();
        assert_eq!(ft.timeout_ms(), 2_000);
    }

    // 41, 42, 43. RETIRED 2026-09-21 with `ReferenceQueue`:
    // `reference_queue_overflow_counter`, `reference_queue_remove_timeout_immediate`
    // and `reference_queue_remove_timeout_empty`. 43 is the second of the two
    // tests in the tree that asserted an elapsed duration produced by a wait
    // that could not succeed; see the note on
    // `remove_timeout_still_honours_its_deadline` above.

    // 44. update_after_gc skips null target addresses (pointer validation)
    #[test]
    fn update_after_gc_skips_null_targets() {
        let mut proc = ReferenceProcessor::new();
        proc.discover_reference(ReferenceType::Weak, 100, 200, Some(300));
        let mut map = cratonvm_types::PointerMap::default();
        // Map reference_obj to 0 (invalid) — should be skipped
        map.insert(100usize, 0usize);
        // Map referent to valid address
        map.insert(200, 1200);
        // Map queue to 0 (invalid) — should be skipped
        map.insert(300, 0);
        proc.update_after_gc(&map);
        // reference_obj and queue should remain unchanged (null target skipped)
        assert_eq!(proc.weak_refs[0].reference_obj, 100);
        assert_eq!(proc.weak_refs[0].referent, 1200);
        assert_eq!(proc.weak_refs[0].queue_addr, Some(300));
    }

    // 45. update_after_gc skips null for finalization queue entries
    #[test]
    fn update_after_gc_finalization_null_skipped() {
        let mut proc = ReferenceProcessor::new();
        // Populate the queue directly — process_references now drains it (see
        // finalization_queue_fifo) — to isolate update_after_gc's null-target
        // skip behaviour.
        proc.finalization_queue.push_back(0xBEEF);
        let mut map = cratonvm_types::PointerMap::default();
        map.insert(0xBEEF, 0usize); // null target
        proc.update_after_gc(&map);
        // Should NOT have been relocated to 0
        assert_eq!(proc.finalization_queue, vec![0xBEEF]);
    }

    // 46. update_after_gc relocates across all reference types
    #[test]
    fn update_after_gc_all_ref_types() {
        let mut proc = ReferenceProcessor::new();
        proc.discover_reference(ReferenceType::Soft, 10, 20, Some(30));
        proc.discover_reference(ReferenceType::Weak, 40, 50, None);
        proc.discover_reference(ReferenceType::Phantom, 60, 70, Some(80));
        proc.discover_reference(ReferenceType::Cleaner, 90, 100, None);
        proc.discover_reference(ReferenceType::Finalizer, 110, 120, Some(130));

        let mut map = cratonvm_types::PointerMap::default();
        for old in (10..=130).step_by(10) {
            map.insert(old, old + 1000);
        }
        proc.update_after_gc(&map);

        assert_eq!(proc.soft_refs[0].reference_obj, 1010);
        assert_eq!(proc.soft_refs[0].referent, 1020);
        assert_eq!(proc.soft_refs[0].queue_addr, Some(1030));
        assert_eq!(proc.weak_refs[0].reference_obj, 1040);
        assert_eq!(proc.weak_refs[0].referent, 1050);
        assert_eq!(proc.phantom_refs[0].reference_obj, 1060);
        assert_eq!(proc.phantom_refs[0].queue_addr, Some(1080));
        assert_eq!(proc.cleaner_refs[0].reference_obj, 1090);
        assert_eq!(proc.finalizer_refs[0].reference_obj, 1110);
        assert_eq!(proc.finalizer_refs[0].queue_addr, Some(1130));
    }

    // 47. RETIRED 2026-09-21 with `ReferenceQueue`:
    // `reference_queue_remove_blocking_with_data`. It exercised the ONE input
    // for which `remove_blocking` returned without waiting — a queue that
    // already had an entry — and so was green while the sixty-second cap it
    // nominally covered was unreachable by any caller.

    // 48. FinalizerThread multiple resurrections all blocked
    #[test]
    fn finalizer_multiple_resurrections_blocked() {
        let ft = FinalizerThread::new();
        ft.enqueue(0xA);
        ft.enqueue(0xB);
        ft.dequeue(); // finalizes 0xA
        ft.dequeue(); // finalizes 0xB
                      // Both resurrections blocked
        assert!(!ft.enqueue(0xA));
        assert!(!ft.enqueue(0xB));
        // New objects still accepted
        assert!(ft.enqueue(0xC));
        assert_eq!(ft.pending_count(), 1);
    }

    // 49. FinalizerThread dropped_count starts at zero
    #[test]
    fn finalizer_dropped_count_initially_zero() {
        let ft = FinalizerThread::new();
        assert_eq!(ft.dropped_count(), 0);
    }

    // ======================================================================
    // V18: finalizer-reachable closure protects weak/soft refs
    // ======================================================================

    // 50. A weak ref whose referent is itself a to-be-finalized object must
    //     NOT be cleared in the same cycle the finalizer is enqueued
    //     (single-hop closure, no tracer needed).
    #[test]
    fn v18_weak_ref_to_finalizable_object_not_cleared() {
        let mut proc = ReferenceProcessor::new();
        // finalizable object lives at 0x200; nothing is marked.
        proc.discover_reference(ReferenceType::Finalizer, 0x100, 0x200, None);
        // weak ref points directly at the finalizable object 0x200.
        proc.discover_reference(ReferenceType::Weak, 0x300, 0x200, Some(0x400));

        let result = proc.process_references(&always_dead, 64, 0);

        // The finalizer is enqueued for the object...
        assert_eq!(result.stats.finalizer_refs_enqueued, 1);
        // ...but the weak ref to that same object must survive this cycle.
        assert_eq!(result.stats.weak_refs_cleared, 0);
        assert!(!proc.weak_refs[0].cleared);
    }

    // 51. With a tracer, a weak ref reachable only transitively (depth 2)
    //     through a finalizable object is also protected.
    #[test]
    fn v18_transitive_closure_protects_weak_ref_with_tracer() {
        let mut proc = ReferenceProcessor::new();
        // 0x200 is finalizable; 0x500 is reachable only via 0x200's fields.
        proc.discover_reference(ReferenceType::Finalizer, 0x100, 0x200, None);
        proc.discover_reference(ReferenceType::Weak, 0x300, 0x500, Some(0x400));

        // Tracer: from root 0x200, reach {0x200, 0x500}.
        let trace = |roots: &[usize]| -> Vec<usize> {
            let mut out = roots.to_vec();
            if roots.contains(&0x200) {
                out.push(0x500);
            }
            out
        };

        let result =
            proc.process_references_with_finalizer_trace(&always_dead, Some(&trace), 64, 0);

        assert_eq!(result.stats.finalizer_refs_enqueued, 1);
        // Transitively reachable referent must not be cleared this cycle.
        assert_eq!(result.stats.weak_refs_cleared, 0);
        assert!(!proc.weak_refs[0].cleared);
    }

    // 52. Residual gap: WITHOUT a tracer, a transitively-reachable (depth 2)
    //     weak referent is still cleared. This documents the known gap that
    //     requires an out-of-scope caller change to pass `trace_from`.
    #[test]
    fn v18_residual_gap_transitive_without_tracer() {
        let mut proc = ReferenceProcessor::new();
        proc.discover_reference(ReferenceType::Finalizer, 0x100, 0x200, None);
        // weak referent 0x500 is NOT a direct finalizer referent.
        proc.discover_reference(ReferenceType::Weak, 0x300, 0x500, None);

        // Legacy entry point (no tracer): only direct referents protected.
        let result = proc.process_references(&always_dead, 64, 0);
        assert_eq!(result.stats.weak_refs_cleared, 1);
    }

    // 53. Closure does not resurrect already-marked finalizers as roots, and
    //     ordinary weak clearing of unrelated dead referents still happens.
    #[test]
    fn v18_unrelated_weak_ref_still_cleared() {
        let mut proc = ReferenceProcessor::new();
        proc.discover_reference(ReferenceType::Finalizer, 0x100, 0x200, None);
        // weak ref to a totally unrelated dead object 0x999.
        proc.discover_reference(ReferenceType::Weak, 0x300, 0x999, None);

        let result = proc.process_references(&always_dead, 64, 0);
        assert_eq!(result.stats.weak_refs_cleared, 1);
        assert!(proc.weak_refs[0].cleared);
    }

    // ======================================================================
    // LRU-index leak fix: cleared soft refs are pruned from soft_ref_lru_index
    // ======================================================================

    // 54. Clearing a soft ref removes its key from the LRU index so the index
    //     stays in sync with the live (uncleared) soft refs and is not
    //     re-scanned on subsequent GCs.
    #[test]
    fn soft_ref_lru_index_pruned_on_clear() {
        let mut proc = ReferenceProcessor::new_with_policy(1000);
        proc.discover_reference(ReferenceType::Soft, 100, 200, Some(300));
        // One live key in the index after discovery.
        assert_eq!(proc.soft_ref_lru_index.len(), 1);
        // threshold = 1000 * 2 = 2000; idle = 5000 > 2000 => clear.
        let result = proc.process_references(&always_dead, 2, 5000);
        assert_eq!(result.stats.soft_refs_cleared, 1);
        assert!(proc.soft_refs[0].cleared);
        // The cleared entry's key must have been removed from the LRU index.
        assert_eq!(
            proc.soft_ref_lru_index.len(),
            0,
            "cleared soft ref left a stale LRU-index entry"
        );
    }

    // 55. A surviving soft ref keeps its LRU key while a sibling is cleared;
    //     ordering/keys of survivors are preserved.
    #[test]
    fn soft_ref_lru_index_keeps_survivors() {
        let mut proc = ReferenceProcessor::new_with_policy(1000);
        // idx 0 referent dead, idx 1 referent live.
        proc.discover_reference(ReferenceType::Soft, 100, 200, Some(300));
        proc.discover_reference(ReferenceType::Soft, 101, 201, Some(301));
        assert_eq!(proc.soft_ref_lru_index.len(), 2);
        // Only referent 201 stays live; threshold small so the dead one clears.
        let live = [201usize];
        let result = proc.process_references(&live_set(&live), 1, 5000);
        assert_eq!(result.stats.soft_refs_cleared, 1);
        assert!(proc.soft_refs[0].cleared);
        assert!(!proc.soft_refs[1].cleared);
        // Exactly one (survivor) key remains, and it is the survivor's key.
        assert_eq!(proc.soft_ref_lru_index.len(), 1);
        assert!(proc
            .soft_ref_lru_index
            .contains_key(&(proc.soft_refs[1].last_access_time_ms, 1)));
    }

    // 56. Re-processing after a clear does not re-walk the cleared entry: the
    //     index no longer contains its key, and a second pass is a no-op.
    #[test]
    fn soft_ref_lru_index_no_rescan_after_clear() {
        let mut proc = ReferenceProcessor::new_with_policy(1000);
        proc.discover_reference(ReferenceType::Soft, 100, 200, Some(300));
        let r1 = proc.process_references(&always_dead, 2, 5000);
        assert_eq!(r1.stats.soft_refs_cleared, 1);
        assert_eq!(proc.soft_ref_lru_index.len(), 0);
        // Second pass: nothing to clear (already cleared) and index stays empty.
        let r2 = proc.process_references(&always_dead, 2, 5000);
        assert_eq!(r2.stats.soft_refs_cleared, 0);
        assert_eq!(proc.soft_ref_lru_index.len(), 0);
    }

    // ======================================================================
    // PERF: O(1) touch_soft_reference via address index
    // ======================================================================

    // 57. touch_soft_reference re-keys the matching entry in the LRU index
    //     using the O(1) address index (same effect the old linear scan had).
    #[test]
    fn touch_soft_reference_rekeys_lru_index() {
        let mut proc = ReferenceProcessor::new_with_policy(1000);
        proc.discover_reference(ReferenceType::Soft, 0xAA, 0x10, Some(0x100));
        proc.discover_reference(ReferenceType::Soft, 0xBB, 0x20, Some(0x200));
        // Initial keys are both at timestamp 0.
        assert!(proc.soft_ref_lru_index.contains_key(&(0, 0)));
        assert!(proc.soft_ref_lru_index.contains_key(&(0, 1)));

        // Touch the second soft ref (idx 1) to time 5000.
        proc.touch_soft_reference(0xBB, 5000);
        assert_eq!(proc.soft_refs[1].last_access_time_ms, 5000);
        // Old key removed, new key inserted; first entry untouched.
        assert!(!proc.soft_ref_lru_index.contains_key(&(0, 1)));
        assert!(proc.soft_ref_lru_index.contains_key(&(5000, 1)));
        assert!(proc.soft_ref_lru_index.contains_key(&(0, 0)));
        // Index size unchanged (re-key, not add).
        assert_eq!(proc.soft_ref_lru_index.len(), 2);
    }

    // INT-8: reference_object_addresses returns weak/soft/phantom Reference
    // OBJECT addresses (cleared included), never finalizer/cleaner entries.
    #[test]
    fn reference_object_addresses_covers_weak_soft_phantom_only() {
        let mut proc = ReferenceProcessor::new();
        proc.discover_reference(ReferenceType::Weak, 0x10, 0x11, None);
        proc.discover_reference(ReferenceType::Soft, 0x20, 0x21, None);
        proc.discover_reference(ReferenceType::Phantom, 0x30, 0x31, Some(0x99));
        proc.discover_reference(ReferenceType::Finalizer, 0x40, 0x40, None);
        proc.discover_reference(ReferenceType::Cleaner, 0x50, 0x51, None);
        let mut addrs = proc.reference_object_addresses();
        addrs.sort_unstable();
        assert_eq!(addrs, vec![0x10, 0x20, 0x30]);
    }

    // Regression for the netty `System.gc()`-called-twice bug: a
    // WeakReference discovered but not yet cleared must be reported: the list
    // is the candidate set for the frame rescue
    // (`push_pending_references_held_by_frames`), which keeps a liveness-dead
    // local `r = new WeakReference<>(x, q)` alive. Once cleared, it is no
    // longer a candidate -- see `pending_reference_object_addresses`'s doc.
    #[test]
    fn pending_reference_object_addresses_excludes_cleared_and_finalizer_cleaner() {
        let mut proc = ReferenceProcessor::new();
        // gc-common w1-d: queued, because a queue-less reference is no longer
        // a frame-rescue candidate at all — see `queueless_and_enqueued_references_are_not_rooted`.
        proc.discover_reference(ReferenceType::Weak, 0x10, 0x11, Some(0x97));
        proc.discover_reference(ReferenceType::Soft, 0x20, 0x21, Some(0x98));
        proc.discover_reference(ReferenceType::Phantom, 0x30, 0x31, Some(0x99));
        proc.discover_reference(ReferenceType::Finalizer, 0x40, 0x40, None);
        proc.discover_reference(ReferenceType::Cleaner, 0x50, 0x51, None);

        let mut addrs = proc.pending_reference_object_addresses();
        addrs.sort_unstable();
        assert_eq!(
            addrs,
            vec![0x10, 0x20, 0x30],
            "every not-yet-cleared weak/soft/phantom Reference object must be reported"
        );

        // Clear the weak ref (its referent, 0x11, is judged dead) -- once
        // cleared, it should drop out of the pending set. Soft/phantom are
        // untouched by `process_weak_refs`, so they stay pending.
        proc.process_weak_refs(&|addr| addr != 0x11);
        let mut addrs = proc.pending_reference_object_addresses();
        addrs.sort_unstable();
        assert_eq!(
            addrs,
            vec![0x20, 0x30],
            "a cleared reference no longer needs synthetic rooting; untouched ones still do"
        );
    }

    // INT-8: cleaner_pending_object_addresses excludes self-referent
    // (finalizer-style) registrations and already-fired actions.
    #[test]
    fn cleaner_pending_object_addresses_excludes_self_and_fired() {
        let mut proc = ReferenceProcessor::new();
        proc.discover_reference(ReferenceType::Cleaner, 0x50, 0x51, None); // pending
        proc.discover_reference(ReferenceType::Cleaner, 0x60, 0x60, None); // self-referent
        proc.discover_reference(ReferenceType::Cleaner, 0x70, 0x71, None); // will fire
                                                                           // Fire 0x70's action: its referent 0x71 is dead.
        let result = proc.process_references(&|addr| addr != 0x71, 64, 0);
        assert!(result.cleaner_actions.contains(&0x70));
        let addrs = proc.cleaner_pending_object_addresses();
        assert_eq!(addrs, vec![0x50]);
    }

    // INT-8: soft_survivor_referents returns kept (uncleared, unenqueued)
    // soft referents — the set the remark driver must resurrect.
    #[test]
    fn soft_survivor_referents_tracks_kept_entries() {
        let mut proc = ReferenceProcessor::new();
        proc.discover_reference(ReferenceType::Soft, 0x20, 0x21, None);
        proc.discover_reference(ReferenceType::Soft, 0x30, 0x31, None);
        // Plenty of free heap: policy keeps both even though 0x31 is dead.
        let _ = proc.process_references(&|addr| addr != 0x31, 1024, 0);
        let mut kept = proc.soft_survivor_referents();
        kept.sort_unstable();
        // Whatever the policy decided, the accessor must mirror the
        // uncleared set exactly.
        let expected: Vec<usize> = proc
            .soft_refs
            .iter()
            .filter(|e| !e.cleared && !e.enqueued)
            .map(|e| e.referent)
            .collect();
        let mut expected_sorted = expected;
        expected_sorted.sort_unstable();
        assert_eq!(kept, expected_sorted);
        assert!(kept.contains(&0x21), "live soft referent is always kept");
    }

    // 58. touch on an unknown address is a no-op (O(1) miss, no scan needed).
    #[test]
    fn touch_soft_reference_unknown_addr_noop() {
        let mut proc = ReferenceProcessor::new_with_policy(1000);
        proc.discover_reference(ReferenceType::Soft, 0xAA, 0x10, None);
        proc.touch_soft_reference(0xDEAD, 5000);
        assert_eq!(proc.soft_refs[0].last_access_time_ms, 0);
        assert_eq!(proc.soft_ref_lru_index.len(), 1);
    }

    // 59. touch with an unadvanced timestamp is a no-op (preserves the
    //     "only re-insert when the timer actually advances" fast path).
    #[test]
    fn touch_soft_reference_same_timestamp_noop() {
        let mut proc = ReferenceProcessor::new_with_policy(1000);
        proc.discover_reference(ReferenceType::Soft, 0xAA, 0x10, None);
        proc.touch_soft_reference(0xAA, 7000);
        // Touching again with the same time must not churn the index.
        proc.touch_soft_reference(0xAA, 7000);
        assert_eq!(proc.soft_refs[0].last_access_time_ms, 7000);
        assert!(proc.soft_ref_lru_index.contains_key(&(7000, 0)));
        assert_eq!(proc.soft_ref_lru_index.len(), 1);
    }

    // 60. After remove_collected compacts soft_refs, the address index is
    //     rebuilt so touch still resolves the (now-shifted) survivor.
    #[test]
    fn touch_soft_reference_after_remove_collected() {
        let mut proc = ReferenceProcessor::new_with_policy(1000);
        // idx 0 (0xAA) will be collected; idx 1 (0xBB) survives -> shifts to 0.
        proc.discover_reference(ReferenceType::Soft, 0xAA, 0x10, None);
        proc.discover_reference(ReferenceType::Soft, 0xBB, 0x20, None);
        let live = [0xBBusize];
        proc.remove_collected(&live_set(&live));
        assert_eq!(proc.soft_refs.len(), 1);
        assert_eq!(proc.soft_refs[0].reference_obj, 0xBB);

        // Touch the survivor at its NEW position; LRU key must use idx 0.
        proc.touch_soft_reference(0xBB, 9000);
        assert_eq!(proc.soft_refs[0].last_access_time_ms, 9000);
        assert!(proc.soft_ref_lru_index.contains_key(&(9000, 0)));
        assert_eq!(proc.soft_ref_lru_index.len(), 1);
        // The collected entry's address no longer resolves.
        proc.touch_soft_reference(0xAA, 9999);
        assert_eq!(proc.soft_ref_lru_index.len(), 1);
    }

    // 61. After update_after_gc relocates reference_obj, the address index is
    //     rebuilt so touch resolves the NEW address (not the stale one).
    #[test]
    fn touch_soft_reference_after_update_after_gc() {
        let mut proc = ReferenceProcessor::new_with_policy(1000);
        proc.discover_reference(ReferenceType::Soft, 0xAA, 0x10, None);
        let mut map = cratonvm_types::PointerMap::default();
        map.insert(0xAAusize, 0xCCusize); // reference_obj 0xAA -> 0xCC
        proc.update_after_gc(&map);
        assert_eq!(proc.soft_refs[0].reference_obj, 0xCC);

        // Old address must no longer resolve; new address must.
        proc.touch_soft_reference(0xAA, 1234);
        assert_eq!(proc.soft_refs[0].last_access_time_ms, 0);
        proc.touch_soft_reference(0xCC, 1234);
        assert_eq!(proc.soft_refs[0].last_access_time_ms, 1234);
        assert!(proc.soft_ref_lru_index.contains_key(&(1234, 0)));
    }

    // ======================================================================
    // JLS reachability ordering (strong > soft > weak): a weak ref must not be
    // cleared while its referent is still softly reachable through a surviving
    // soft reference.
    // ======================================================================

    // 62. A weak ref to an object reachable ONLY transitively (depth 2) through
    //     a surviving soft referent must NOT be cleared (requires a tracer).
    #[test]
    fn weak_ref_kept_alive_by_surviving_soft_referent_transitive() {
        let mut proc = ReferenceProcessor::new_with_policy(1000);
        // Soft referent 0x200 is recently accessed, so Phase 1 retains it.
        proc.discover_reference(ReferenceType::Soft, 0x100, 0x200, Some(0x180));
        proc.touch_soft_reference(0x100, 5000);
        // Weak referent 0x500 is reachable only via 0x200's fields (depth 2).
        proc.discover_reference(ReferenceType::Weak, 0x300, 0x500, Some(0x400));

        // Tracer: from the surviving soft referent 0x200 reach {0x200, 0x500}.
        let trace = |roots: &[usize]| -> Vec<usize> {
            let mut out = roots.to_vec();
            if roots.contains(&0x200) {
                out.push(0x500);
            }
            out
        };

        // free_heap large + recent access => soft ref survives Phase 1.
        let result =
            proc.process_references_with_finalizer_trace(&always_dead, Some(&trace), 64, 5000);

        // The soft ref survived...
        assert_eq!(result.stats.soft_refs_cleared, 0);
        assert!(!proc.soft_refs[0].cleared);
        // ...so the transitively-reachable weak referent must NOT be cleared.
        assert_eq!(result.stats.weak_refs_cleared, 0);
        assert!(!proc.weak_refs[0].cleared);
    }

    // 63. A weak ref pointing DIRECTLY at a surviving soft referent must not be
    //     cleared even without a tracer (single-hop soft keep-alive).
    #[test]
    fn weak_ref_to_surviving_soft_referent_not_cleared_no_tracer() {
        let mut proc = ReferenceProcessor::new_with_policy(1000);
        // Soft referent 0x200 retained (recent access, plenty of free heap).
        proc.discover_reference(ReferenceType::Soft, 0x100, 0x200, Some(0x180));
        proc.touch_soft_reference(0x100, 5000);
        // Weak ref points straight at the still-soft-reachable object 0x200.
        proc.discover_reference(ReferenceType::Weak, 0x300, 0x200, Some(0x400));

        let result = proc.process_references(&always_dead, 64, 5000);

        assert_eq!(result.stats.soft_refs_cleared, 0);
        assert_eq!(result.stats.weak_refs_cleared, 0);
        assert!(!proc.weak_refs[0].cleared);
    }

    // 64. When the soft ref is itself CLEARED in Phase 1 (idle past the LRU
    //     threshold), its dead referent must NOT protect a weak ref to it.
    #[test]
    fn weak_ref_not_protected_when_soft_ref_cleared() {
        let mut proc = ReferenceProcessor::new_with_policy(1000);
        // last_access_time_ms = 0, current = 5000, free = 1 MB => threshold
        // 1000; idle 5000 > 1000 => soft ref cleared in Phase 1.
        proc.discover_reference(ReferenceType::Soft, 0x100, 0x200, Some(0x180));
        proc.discover_reference(ReferenceType::Weak, 0x300, 0x200, Some(0x400));

        let result = proc.process_references(&always_dead, 1, 5000);

        assert_eq!(result.stats.soft_refs_cleared, 1);
        assert!(proc.soft_refs[0].cleared);
        // The soft ref no longer keeps 0x200 alive, so the weak ref clears.
        assert_eq!(result.stats.weak_refs_cleared, 1);
        assert!(proc.weak_refs[0].cleared);
    }

    // 65. The soft-survivor closure does not over-retain: an unrelated dead
    //     weak referent is still cleared while a soft ref survives.
    #[test]
    fn unrelated_weak_ref_still_cleared_with_surviving_soft_ref() {
        let mut proc = ReferenceProcessor::new_with_policy(1000);
        proc.discover_reference(ReferenceType::Soft, 0x100, 0x200, Some(0x180));
        proc.touch_soft_reference(0x100, 5000); // soft ref survives
                                                // Weak ref to a totally unrelated dead object, not reachable from 0x200.
        proc.discover_reference(ReferenceType::Weak, 0x300, 0x999, Some(0x400));

        let trace = |roots: &[usize]| -> Vec<usize> { roots.to_vec() }; // 0x200 -> {0x200}
        let result =
            proc.process_references_with_finalizer_trace(&always_dead, Some(&trace), 64, 5000);

        assert_eq!(result.stats.soft_refs_cleared, 0);
        assert_eq!(result.stats.weak_refs_cleared, 1);
        assert!(proc.weak_refs[0].cleared);
    }

    // ======================================================================
    // SOFT-POLICY FIX (2026-07-26): the LRU must survive a caller that has no
    // clock. Both production call sites in `vm/src/runtime/interpreter.rs`
    // pass `current_time_ms == 0`, which used to make `process_soft_refs`
    // structurally incapable of clearing anything. See the
    // `last_observed_clock_ms` field doc.
    // ======================================================================

    // 66. With a `0` caller clock, the mutator clock observed through
    //     `touch_soft_reference` is used instead, so an idle soft ref clears.
    #[test]
    fn soft_policy_uses_mutator_clock_when_caller_passes_zero() {
        let mut proc = ReferenceProcessor::new_with_policy(1000);
        proc.discover_reference(ReferenceType::Soft, 0x100, 0x200, Some(0x180));
        // `SoftReference.get()` stamps a wall-clock time on the entry.
        proc.touch_soft_reference(0x100, 1_000_000);
        // Time passes; another (unrelated / unknown) touch advances only the
        // processor's clock. This also pins that the clock is recorded BEFORE
        // the unknown-address early return.
        proc.touch_soft_reference(0xDEAD, 1_100_000);

        // Exactly what the interpreter passes today.
        let result = proc.process_references(&always_dead, 64, 0);

        // threshold = 1000 ms/MB * 64 MB = 64_000; idle = 100_000 > 64_000.
        assert_eq!(
            result.stats.soft_refs_cleared, 1,
            "soft-ref LRU must run even when the collector supplies no clock"
        );
        assert!(proc.soft_refs[0].cleared);
    }

    // 67. A soft ref created after the clock is known carries a *creation*
    //     timestamp (like `SoftReference`'s constructor), so it is not
    //     instantly stale against a wall-clock "now". Non-soft types keep
    //     stamping 0 — the field is meaningless for them.
    #[test]
    fn soft_policy_creation_stamp_protects_fresh_ref() {
        let mut proc = ReferenceProcessor::new_with_policy(1000);
        // Establish the mutator clock first (some earlier SoftReference.get()).
        proc.touch_soft_reference(0xDEAD, 5_000_000);

        proc.discover_reference(ReferenceType::Soft, 0x100, 0x200, None);
        assert_eq!(
            proc.soft_refs[0].last_access_time_ms, 5_000_000,
            "a freshly discovered soft ref must be stamped at creation"
        );
        proc.discover_reference(ReferenceType::Weak, 0x300, 0x400, None);
        assert_eq!(
            proc.weak_refs[0].last_access_time_ms, 0,
            "only soft refs carry an LRU timestamp"
        );

        // Production (64, 0): idle == 0, so the fresh ref must survive.
        let result = proc.process_references(&always_dead, 64, 0);
        assert_eq!(result.stats.soft_refs_cleared, 0);
        assert!(!proc.soft_refs[0].cleared);
    }

    // 68. A caller that DOES supply a coherent clock keeps full control: the
    //     larger of the two wins, so ZGC's backend and every test behave
    //     exactly as before the fix.
    #[test]
    fn soft_policy_caller_clock_dominates_observed_clock() {
        let mut proc = ReferenceProcessor::new_with_policy(1000);
        proc.discover_reference(ReferenceType::Soft, 0x100, 0x200, None);
        proc.touch_soft_reference(0x100, 1_000);

        // threshold = 1000 * 1 = 1000; idle = 1_000_000 - 1_000 > 1000.
        let result = proc.process_references(&always_dead, 1, 1_000_000);
        assert_eq!(result.stats.soft_refs_cleared, 1);
    }

    // ======================================================================
    // SPEC FIX (2026-07-26): phases 3-4 honour the soft/finalizer closures.
    // ======================================================================

    // 69. A `Cleaner` must NOT fire while its referent is still softly
    //     reachable. The live case is `DirectByteBuffer`: firing here frees
    //     native memory still reachable through `SoftReference.get()`.
    #[test]
    fn cleaner_not_fired_while_referent_softly_reachable() {
        let mut proc = ReferenceProcessor::new_with_policy(1000);
        // Soft ref retains 0x200 (recently touched, ample headroom).
        proc.discover_reference(ReferenceType::Soft, 0x100, 0x200, None);
        proc.touch_soft_reference(0x100, 5_000);
        // A Cleaner registered against that very object.
        proc.discover_reference(ReferenceType::Cleaner, 0x300, 0x200, None);

        let result = proc.process_references(&always_dead, 64, 5_000);

        assert_eq!(result.stats.soft_refs_cleared, 0, "soft ref must survive");
        assert!(
            result.cleaner_actions.is_empty(),
            "cleaner fired for a still-soft-reachable referent"
        );
        assert!(!proc.cleaner_refs[0].cleared);
    }

    // 70. `finalize()` must NOT be scheduled while the object is still softly
    //     reachable.
    #[test]
    fn finalizer_not_enqueued_while_referent_softly_reachable() {
        let mut proc = ReferenceProcessor::new_with_policy(1000);
        proc.discover_reference(ReferenceType::Soft, 0x100, 0x200, None);
        proc.touch_soft_reference(0x100, 5_000);
        proc.discover_reference(ReferenceType::Finalizer, 0x300, 0x200, None);

        let result = proc.process_references(&always_dead, 64, 5_000);

        assert_eq!(result.stats.soft_refs_cleared, 0);
        assert_eq!(result.stats.finalizer_refs_enqueued, 0);
        assert!(result.to_finalize.is_empty());
    }

    // 71. A phantom must wait until the referent has been finalized, then fire
    //     on the following cycle (bounded one-cycle delay, never permanent).
    #[test]
    fn phantom_waits_one_cycle_for_finalization() {
        let mut proc = ReferenceProcessor::new();
        proc.discover_reference(ReferenceType::Finalizer, 0x100, 0x200, None);
        proc.discover_reference(ReferenceType::Phantom, 0x300, 0x200, Some(0x400));

        let r1 = proc.process_references(&always_dead, 64, 0);
        assert_eq!(r1.stats.finalizer_refs_enqueued, 1);
        assert_eq!(
            r1.stats.phantom_refs_enqueued, 0,
            "phantom must not fire before finalization"
        );

        // The finalizer entry is now flagged enqueued, so it stops rooting the
        // object and the phantom is delivered.
        let r2 = proc.process_references(&always_dead, 64, 0);
        assert_eq!(r2.stats.phantom_refs_enqueued, 1);
        assert!(r2.to_enqueue.iter().any(|&(r, q)| r == 0x300 && q == 0x400));
    }

    // 72. Mutually-reachable finalizable objects must BOTH be enqueued in the
    //     same cycle. This pins the deliberate asymmetry: the finalizer phase
    //     folds in the soft closure but NOT the finalizer closure, otherwise
    //     each object would block the other forever.
    #[test]
    fn mutually_reachable_finalizers_both_enqueued() {
        let mut proc = ReferenceProcessor::new();
        proc.discover_reference(ReferenceType::Finalizer, 0x100, 0x200, None);
        proc.discover_reference(ReferenceType::Finalizer, 0x300, 0x400, None);

        // 0x200 and 0x400 reference each other.
        let trace = |roots: &[usize]| -> Vec<usize> {
            let mut out = roots.to_vec();
            if roots.contains(&0x200) {
                out.push(0x400);
            }
            if roots.contains(&0x400) {
                out.push(0x200);
            }
            out
        };

        let result =
            proc.process_references_with_finalizer_trace(&always_dead, Some(&trace), 64, 0);
        assert_eq!(
            result.stats.finalizer_refs_enqueued, 2,
            "finalizer-reachability must not block finalization"
        );
    }

    // 73. No over-retention: an unrelated dead cleaner/phantom referent still
    //     fires while a soft reference survives elsewhere.
    #[test]
    fn unrelated_cleaner_and_phantom_still_fire_with_surviving_soft_ref() {
        let mut proc = ReferenceProcessor::new_with_policy(1000);
        proc.discover_reference(ReferenceType::Soft, 0x100, 0x200, None);
        proc.touch_soft_reference(0x100, 5_000);
        // Unrelated dead referents.
        proc.discover_reference(ReferenceType::Cleaner, 0x300, 0x999, None);
        proc.discover_reference(ReferenceType::Phantom, 0x500, 0x888, Some(0x600));

        let result = proc.process_references(&always_dead, 64, 5_000);

        assert_eq!(result.stats.soft_refs_cleared, 0);
        assert!(result.cleaner_actions.contains(&0x300));
        assert_eq!(result.stats.phantom_refs_enqueued, 1);
    }

    // ======================================================================
    // 74-83. gengc-round3, lane `refdriver`: the ORDER of
    // `remove_collected` relative to the processing phases.
    //
    // These tests exist in PAIRS. The `..._post_pass_..` half pins what the
    // historical order (process, THEN prune) does; the `..._pre_pass_..` half
    // states what the order the generational driver now runs (prune, THEN
    // process) does instead. Reading a pair side by side is the whole
    // behavioural diff of the change, which is why they are written as a pair
    // rather than as a single updated expectation.
    //
    // The shared scenario models the real driver rather than `always_dead`:
    // `survivor` is the collector's survivor predicate, and the point of every
    // one of these cases is that a REFERENCE OBJECT and its REFERENT get
    // different answers from it.
    // ======================================================================

    /// Only the addresses in `live` survived the collection. Modelled on the
    /// driver's `is_marked` (`watched_pre_gc_addr_survived`), which answers
    /// per-address and is emphatically not `always_dead`.
    fn survivor(live: &[usize]) -> impl Fn(usize) -> bool + '_ {
        move |addr: usize| live.contains(&addr)
    }

    // 74. THE GAP (gengc-mark). Historical order: a `SoftReference` whose own
    //     instance died still seeds `soft_live`, so a `WeakReference` to the
    //     same (dead) referent is left uncleared and `get()` hands back
    //     reclaimable storage.
    #[test]
    fn dead_softref_instance_roots_the_weak_closure_under_post_pass_ordering() {
        let mut proc = ReferenceProcessor::new_with_policy(1000);
        // The SoftReference instance 0x100 is itself dead; so is its referent.
        proc.discover_reference(ReferenceType::Soft, 0x100, 0x200, None);
        proc.touch_soft_reference(0x100, 5_000);
        // A live WeakReference aimed at the same object.
        proc.discover_reference(ReferenceType::Weak, 0x300, 0x200, None);

        // Nothing survived except the WeakReference object itself.
        let live = [0x300usize];
        let result = proc.process_references(&survivor(&live), 64, 5_000);
        proc.remove_collected(&survivor(&live));

        // PINNED DEFECT: the weak ref was NOT cleared, because the dead soft
        // row survived into the soft-survivor-root construction.
        assert_eq!(
            result.stats.weak_refs_cleared, 0,
            "pins the historical (defective) behaviour this change fixes"
        );
        // ...and the soft row is only dropped afterwards, too late to matter.
        assert!(proc.soft_refs.is_empty());
    }

    // 75. The same scenario under the driver's new order: the dead soft row is
    //     gone before Phase 1, so nothing roots the weak closure and the weak
    //     reference clears. This is the gap page's requested test.
    #[test]
    fn dead_softref_instance_no_longer_roots_the_weak_closure_under_pre_pass_ordering() {
        let mut proc = ReferenceProcessor::new_with_policy(1000);
        proc.discover_reference(ReferenceType::Soft, 0x100, 0x200, None);
        proc.touch_soft_reference(0x100, 5_000);
        proc.discover_reference(ReferenceType::Weak, 0x300, 0x200, None);

        let live = [0x300usize];
        proc.remove_collected_reference_objects(&survivor(&live));
        // The dead SoftReference's row never reaches Phase 1.
        assert!(proc.soft_refs.is_empty());

        let result = proc.process_references(&survivor(&live), 64, 5_000);

        assert_eq!(
            result.stats.weak_refs_cleared, 1,
            "a weak ref to an object that is not softly reachable must clear"
        );
        assert!(proc.weak_refs[0].cleared);
    }

    // 76. THE FENCE, restated against the pre-pass. A LIVE `SoftReference`
    //     whose referent the LRU policy retains is unmarked BY CONSTRUCTION —
    //     that is the entire point of a soft reference — and the pre-pass must
    //     not touch it. This is the case both previous attempts got wrong, and
    //     it is the reason the prune is keyed on the reference object's
    //     survival rather than on anything about the referent.
    #[test]
    fn pre_pass_keeps_a_live_softref_whose_policy_retained_referent_is_unmarked() {
        let mut proc = ReferenceProcessor::new_with_policy(1000);
        // The SoftReference instance survives; its referent does not appear in
        // the survivor set at all (nothing strongly reaches it).
        proc.discover_reference(ReferenceType::Soft, 0x100, 0x200, None);
        proc.touch_soft_reference(0x100, 5_000);
        proc.discover_reference(ReferenceType::Weak, 0x300, 0x200, None);

        let live = [0x100usize, 0x300usize];
        proc.remove_collected_reference_objects(&survivor(&live));
        assert_eq!(proc.soft_refs.len(), 1, "a live SoftReference must survive");

        // Ample headroom + recent access => Phase 1 retains the referent.
        let result = proc.process_references(&survivor(&live), 64, 5_000);

        assert_eq!(result.stats.soft_refs_cleared, 0);
        assert_eq!(
            result.stats.weak_refs_cleared, 0,
            "JLS strong > soft > weak: a softly-reachable referent's weak ref \
             must not be cleared"
        );
    }

    // 77-78. PHANTOM. Historical order: a `PhantomReference` whose own
    //     instance died in this cycle is still enqueued (against a queue
    //     nobody can poll, through an address the driver then refuses to
    //     write). Pre-pass order: the row is gone, so it is not enqueued at
    //     all — which is what HotSpot does, since it never DISCOVERS an
    //     unreachable Reference in the first place.
    #[test]
    fn dead_phantom_instance_still_enqueues_under_post_pass_ordering() {
        let mut proc = ReferenceProcessor::new();
        proc.discover_reference(ReferenceType::Phantom, 0x500, 0x888, Some(0x600));

        let live: [usize; 0] = [];
        let result = proc.process_references(&survivor(&live), 64, 0);

        assert_eq!(result.stats.phantom_refs_enqueued, 1);
        assert!(proc.phantom_refs[0].enqueued);
    }

    #[test]
    fn dead_phantom_instance_is_dropped_before_it_enqueues_under_pre_pass_ordering() {
        let mut proc = ReferenceProcessor::new();
        proc.discover_reference(ReferenceType::Phantom, 0x500, 0x888, Some(0x600));

        let live: [usize; 0] = [];
        proc.remove_collected_reference_objects(&survivor(&live));
        let result = proc.process_references(&survivor(&live), 64, 0);

        assert!(proc.phantom_refs.is_empty());
        assert_eq!(result.stats.phantom_refs_enqueued, 0);
        assert!(result.to_enqueue.is_empty());
    }

    // 79-80. `jdk.internal.ref.Cleaner` (a phantom that RUNS). Same pair. The
    //     emitted action is the CLEANER OBJECT's address, so when the cleaner
    //     object itself died the historical order emits an action the VM
    //     driver then declines (`is_stale_young`); the pre-pass order never
    //     emits it.
    #[test]
    fn dead_cleaner_phantom_still_emits_an_action_under_post_pass_ordering() {
        let mut proc = ReferenceProcessor::new();
        proc.discover_phantom_cleaner(0x700, 0x888, None);

        let live: [usize; 0] = [];
        let result = proc.process_references(&survivor(&live), 64, 0);

        assert!(result.cleaner_actions.contains(&0x700));
    }

    #[test]
    fn dead_cleaner_phantom_emits_no_action_under_pre_pass_ordering() {
        let mut proc = ReferenceProcessor::new();
        proc.discover_phantom_cleaner(0x700, 0x888, None);

        let live: [usize; 0] = [];
        proc.remove_collected_reference_objects(&survivor(&live));
        let result = proc.process_references(&survivor(&live), 64, 0);

        assert!(result.cleaner_actions.is_empty());
    }

    // 81. A LIVE cleaner whose referent died still fires under the pre-pass —
    //     the case that actually matters for `DirectByteBuffer`. The pre-pass
    //     must not cost a single real cleaner delivery.
    #[test]
    fn live_cleaner_phantom_with_dead_referent_still_fires_under_pre_pass_ordering() {
        let mut proc = ReferenceProcessor::new();
        proc.discover_phantom_cleaner(0x700, 0x888, None);

        let live = [0x700usize];
        proc.remove_collected_reference_objects(&survivor(&live));
        let result = proc.process_references(&survivor(&live), 64, 0);

        assert!(result.cleaner_actions.contains(&0x700));
    }

    // 82. THE ONE THAT MAKES THE NARROW PASS NECESSARY. The VM registers a
    //     finalizable object as `reference_obj == referent == the object`
    //     (`SharedVm::register_finalizable`), and `Finalizer.register` does the
    //     same through the `Cleaner` wire type. For those rows "the reference
    //     object did not survive" IS "the object is dead", i.e. exactly when
    //     the row must fire. A pre-pass over ALL FIVE lists would therefore
    //     switch finalization off; `remove_collected_reference_objects` leaves
    //     both lists alone, and `finalize()` is still scheduled.
    #[test]
    fn pre_pass_never_prunes_self_referential_finalizer_and_cleaner_rows() {
        let mut proc = ReferenceProcessor::new();
        // Exactly the shape `register_finalizable` produces.
        proc.discover_reference(ReferenceType::Finalizer, 0xF00, 0xF00, None);
        // Exactly the shape the `Finalizer.register` native produces (wire 3).
        proc.discover_reference(ReferenceType::Cleaner, 0xC00, 0xC00, None);

        let live: [usize; 0] = [];
        proc.remove_collected_reference_objects(&survivor(&live));

        assert_eq!(
            proc.finalizer_refs.len(),
            1,
            "finalization must not be pruned away"
        );
        assert_eq!(proc.cleaner_refs.len(), 1);

        let result = proc.process_references(&survivor(&live), 64, 0);
        assert_eq!(result.stats.finalizer_refs_enqueued, 1);
        assert!(result.to_finalize.contains(&0xF00));
        assert!(result.cleaner_actions.contains(&0xC00));

        // The full `remove_collected` at the driver's tail is what retires
        // them, AFTER the verdict has been taken. That ordering is the
        // contract, and it is unchanged.
        proc.remove_collected(&survivor(&live));
        assert!(proc.finalizer_refs.is_empty());
        assert!(proc.cleaner_refs.is_empty());
    }

    // 83. DOUBLE-CALL SAFETY. ZGC still runs `remove_collected` at its own
    //     tail and the VM driver runs it at the end of every cycle, so the
    //     pre-pass means the same predicate is applied twice (three times for
    //     ZGC). It must be a no-op after the first, and in particular the soft
    //     position indices must stay consistent — a stale `(timestamp, idx)`
    //     key is an out-of-bounds index in `process_soft_refs`, not a wrong
    //     answer.
    #[test]
    fn remove_collected_is_idempotent_across_the_pre_pass_and_the_tail_call() {
        let mut proc = ReferenceProcessor::new_with_policy(1000);
        proc.discover_reference(ReferenceType::Soft, 0x100, 0x200, None);
        proc.discover_reference(ReferenceType::Soft, 0x110, 0x210, None);
        proc.discover_reference(ReferenceType::Soft, 0x120, 0x220, None);
        proc.touch_soft_reference(0x110, 5_000);
        proc.discover_reference(ReferenceType::Weak, 0x300, 0x200, None);
        proc.discover_reference(ReferenceType::Phantom, 0x400, 0x220, None);
        proc.discover_reference(ReferenceType::Finalizer, 0xF00, 0xF00, None);

        let live = [0x110usize, 0x300usize, 0xF00usize];

        proc.remove_collected_reference_objects(&survivor(&live));
        let after_pre = (
            proc.soft_refs.len(),
            proc.weak_refs.len(),
            proc.phantom_refs.len(),
        );
        assert_eq!(after_pre, (1, 1, 0));

        // Second application of the same predicate: nothing more to remove.
        proc.remove_collected_reference_objects(&survivor(&live));
        assert_eq!(
            (
                proc.soft_refs.len(),
                proc.weak_refs.len(),
                proc.phantom_refs.len()
            ),
            after_pre
        );

        // The driver's tail call, twice (the second stands in for ZGC's own).
        proc.remove_collected(&survivor(&live));
        proc.remove_collected(&survivor(&live));
        assert_eq!(
            (
                proc.soft_refs.len(),
                proc.weak_refs.len(),
                proc.phantom_refs.len()
            ),
            after_pre
        );
        assert_eq!(proc.finalizer_refs.len(), 1);

        // Every surviving soft entry is addressable by BOTH indices at its
        // current position, and neither index names a position that no longer
        // exists.
        assert_eq!(proc.soft_ref_lru_index.len(), proc.soft_refs.len());
        for (&(_, key_idx), &val_idx) in proc.soft_ref_lru_index.iter() {
            assert_eq!(key_idx, val_idx);
            assert!(key_idx < proc.soft_refs.len(), "stale LRU index position");
        }
        proc.touch_soft_reference(0x110, 6_000);
        assert_eq!(proc.soft_refs[0].last_access_time_ms, 6_000);

        // And the phases still run against the compacted Vec.
        let _ = proc.process_references(&survivor(&live), 64, 6_000);
    }

    // ======================================================================
    // gc-common w1-d (2026-09-23)
    // ======================================================================

    // W1D-1. Only an UNSETTLED reference WITH A QUEUE is in the candidate set
    //        for the frame rescue (it was the synthetic root set until w2-b):
    //        an enqueued phantom is delivered (and was rooted forever before),
    //        a queue-less reference has nothing to deliver.
    #[test]
    fn queueless_and_enqueued_references_are_not_rooted() {
        let mut proc = ReferenceProcessor::new();
        proc.discover_reference(ReferenceType::Weak, 0x10, 0x11, None);
        proc.discover_reference(ReferenceType::Soft, 0x20, 0x21, None);
        proc.discover_reference(ReferenceType::Phantom, 0x30, 0x31, Some(0x99));
        proc.discover_reference(ReferenceType::Weak, 0x40, 0x41, Some(0x98));
        let mut addrs = proc.pending_reference_object_addresses();
        addrs.sort_unstable();
        assert_eq!(addrs, vec![0x30, 0x40], "queue-less rows are not frame-rescue candidates");

        // The phantom's referent dies: it is enqueued (never `cleared`), and
        // from then on it must stop being a candidate.
        let result = proc.process_references(&|a| a != 0x31, 64, 0);
        assert!(result.to_enqueue.contains(&(0x30, 0x99)));
        assert_eq!(proc.pending_reference_object_addresses(), vec![0x40]);
    }

    // W1D-2. The restore gate: only slots the pre-GC pass reported nulling.
    #[test]
    fn the_restore_gate_names_exactly_the_slots_the_pass_nulled() {
        let mut proc = ReferenceProcessor::new();
        proc.discover_reference(ReferenceType::Weak, 0x10, 0x11, Some(0x90));
        proc.discover_reference(ReferenceType::Weak, 0x20, 0x21, Some(0x90));
        let _ = proc.weak_phantom_active_triples();
        // The pass found 0x10's slot non-null (and nulled it); 0x20's was null.
        // Class id 0 (java.lang.Object) must still count as nulled.
        proc.stamp_referent_class(0x10, 0);
        assert!(proc.was_nulled_by_pre_gc_pass(0x10));
        assert!(!proc.was_nulled_by_pre_gc_pass(0x20));
        // A new pass forgets the previous one.
        let _ = proc.weak_phantom_active_triples();
        assert!(!proc.was_nulled_by_pre_gc_pass(0x10));
    }

    // W1D-3. `Reference.clear()` then the referent dies: HotSpot never
    //        enqueues a reference whose referent the program cleared.
    #[test]
    fn a_weak_ref_the_application_cleared_is_retired_and_never_enqueued() {
        let mut proc = ReferenceProcessor::new();
        proc.discover_reference(ReferenceType::Weak, 0x10, 0x11, Some(0x90));
        proc.discover_reference(ReferenceType::Weak, 0x20, 0x21, Some(0x90));
        let _ = proc.weak_phantom_active_triples();
        proc.stamp_referent_class(0x20, 7); // only 0x20's slot held its referent
        assert_eq!(proc.retire_entries_the_pre_gc_pass_found_cleared(), 1);
        // Both referents die this cycle.
        let result = proc.process_references(&always_dead, 64, 0);
        assert_eq!(
            result.to_enqueue,
            vec![(0x20, 0x90)],
            "only the reference the collector cleared may be delivered"
        );
        // The retired row emits no slot write either.
        assert_eq!(proc.take_newly_cleared(), vec![0x20]);
        // And it is no longer a synthetic root.
        assert!(proc.pending_reference_object_addresses().is_empty());
        // Idempotent.
        assert_eq!(proc.retire_entries_the_pre_gc_pass_found_cleared(), 0);
    }

    // W1D-4. Retirement never touches a phantom, and is inert with no pass.
    #[test]
    fn retirement_spares_phantoms_and_needs_a_pass() {
        let mut proc = ReferenceProcessor::new();
        proc.discover_reference(ReferenceType::Weak, 0x10, 0x11, Some(0x90));
        proc.discover_reference(ReferenceType::Phantom, 0x30, 0x31, Some(0x90));
        assert_eq!(
            proc.retire_entries_the_pre_gc_pass_found_cleared(),
            0,
            "no pass ran: nothing was examined"
        );
        let _ = proc.weak_phantom_active_triples();
        // Nothing stamped: the weak row is retired, the phantom is not.
        assert_eq!(proc.retire_entries_the_pre_gc_pass_found_cleared(), 1);
        let result = proc.process_references(&always_dead, 64, 0);
        assert_eq!(result.to_enqueue, vec![(0x30, 0x90)]);
    }

    // W1D-5. A condemned soft ref whose slot was already null is retired and
    //        loses its LRU key.
    #[test]
    fn a_condemned_soft_ref_found_already_null_is_retired() {
        let mut proc = ReferenceProcessor::new_with_policy(1000);
        proc.discover_reference(ReferenceType::Soft, 0x10, 0x11, Some(0x90));
        let _ = proc.weak_phantom_active_triples();
        assert_eq!(proc.condemn_all_soft_refs(), vec![(0x10, 0x11)]);
        assert_eq!(proc.retire_entries_the_pre_gc_pass_found_cleared(), 1);
        assert!(proc.soft_refs[0].cleared && proc.soft_refs[0].enqueued);
        assert!(proc.soft_ref_lru_index.is_empty());
        let result = proc.process_references(&always_dead, 0, 0);
        assert!(result.to_enqueue.is_empty());
        assert_eq!(result.stats.soft_refs_cleared, 0);
    }

    // W1D-6. A refused restore becomes a collector clear, delivered by the
    //        NEXT round, and survives a relocation in between.
    #[test]
    fn a_refused_restore_is_cleared_and_enqueued_on_the_next_round() {
        let mut proc = ReferenceProcessor::new();
        proc.discover_reference(ReferenceType::Weak, 0x10, 0x11, Some(0x90));
        proc.discover_reference(ReferenceType::Phantom, 0x30, 0x31, Some(0x90));
        assert!(proc.clear_after_refused_restore(0x10));
        assert!(
            !proc.clear_after_refused_restore(0x30),
            "a phantom must never be fired by a refusal"
        );
        assert!(!proc.clear_after_refused_restore(0x10), "once only");
        assert_eq!(proc.take_newly_cleared(), Vec::<usize>::new(), "slot is already null");

        let mut map = cratonvm_types::PointerMap::default();
        map.insert(0x10, 0x50);
        map.insert(0x90, 0x95);
        proc.update_after_gc(&map);

        let result = proc.process_references(&always_live, 64, 0);
        assert_eq!(result.to_enqueue, vec![(0x50, 0x95)]);
        assert!(!proc.phantom_refs[0].enqueued, "phantom untouched");
    }

    // W1D-7. A soft refusal takes the same route.
    #[test]
    fn a_refused_soft_restore_is_cleared() {
        let mut proc = ReferenceProcessor::new_with_policy(1000);
        proc.discover_reference(ReferenceType::Soft, 0x10, 0x11, Some(0x90));
        assert!(proc.clear_after_refused_restore(0x10));
        assert!(proc.soft_refs[0].cleared);
        assert!(proc.soft_ref_lru_index.is_empty());
        let result = proc.process_references(&always_live, 64, 0);
        assert_eq!(result.to_enqueue, vec![(0x10, 0x90)]);
    }

    // W1D-8. `finish_pre_gc_cycle` stops a young pause's condemnation from
    //        being read by a later pass that has none of its own (G1 remark).
    #[test]
    fn a_finished_cycle_leaves_no_condemnation_for_the_next_pass() {
        let mut proc = ReferenceProcessor::new_with_policy(1000);
        proc.discover_reference(ReferenceType::Soft, 0x10, 0x11, None);
        proc.touch_soft_reference(0x10, 1_000_000);
        // Young pause: heap tight, entry condemned; referent strongly
        // reachable, so it is kept and restored.
        assert_eq!(proc.condemn_idle_soft_refs(0, 1_000_001), vec![(0x10, 0x11)]);
        let _ = proc.process_references(&always_live, 0, 1_000_001);
        assert!(!proc.soft_refs[0].cleared);
        proc.finish_pre_gc_cycle();
        // "Remark": slot hidden so the referent reads unmarked, heap roomy,
        // entry freshly read — the LRU policy keeps it.
        let _ = proc.process_references(&always_dead, 1024, 1_000_002);
        assert!(
            !proc.soft_refs[0].cleared,
            "a stale condemnation must not clear a soft ref the policy keeps"
        );
    }

    // W1D-9. The prune's index rebuild does not resurrect cleared soft keys,
    //        and a prune that removes nothing rebuilds nothing.
    #[test]
    fn remove_collected_keeps_cleared_soft_rows_out_of_the_lru_index() {
        let mut proc = ReferenceProcessor::new_with_policy(1000);
        proc.discover_reference(ReferenceType::Soft, 0x10, 0x11, None);
        proc.discover_reference(ReferenceType::Soft, 0x20, 0x21, None);
        proc.discover_reference(ReferenceType::Soft, 0x30, 0x31, None);
        // Clear 0x10 (referent dead, idle, no headroom).
        let _ = proc.process_references(&|a| a != 0x11, 0, 5_000);
        assert!(proc.soft_refs[0].cleared);
        // Drop 0x30's Reference object: a real removal, so a rebuild.
        proc.remove_collected(&|a| a != 0x30);
        assert_eq!(proc.soft_refs.len(), 2);
        assert_eq!(proc.soft_ref_lru_index.len(), 1, "cleared row got its key back");
        assert!(proc.soft_ref_lru_index.contains_key(&(0, 1)));
        // The address index still covers every row.
        proc.touch_soft_reference(0x10, 6_000);
        assert_eq!(proc.soft_refs[0].last_access_time_ms, 6_000);
        // A prune that removes nothing leaves the indices alone.
        let before = proc.soft_ref_lru_index.clone();
        proc.remove_collected(&always_live);
        assert_eq!(proc.soft_ref_lru_index, before);
    }

    // W1D-10. A finalized object's address is recycled: a fresh object there
    //         must still be finalized.
    #[test]
    fn a_recycled_address_is_finalized_again_through_enqueue_unfinalized() {
        let ft = FinalizerThread::new();
        assert!(ft.enqueue(0xA0));
        assert_eq!(ft.dequeue(), Some(0xA0));
        assert!(!ft.enqueue(0xA0), "the legacy gate still refuses");
        assert!(ft.enqueue_unfinalized(0xA0), "a never-enqueued row is fresh");
        assert!(!ft.was_finalized(0xA0));
        assert!(!ft.enqueue_unfinalized(0xA0), "queue duplicates still refused");
        assert_eq!(ft.pending_count(), 1);
    }

    // W1D-11. `already_finalized` follows a moving collection.
    #[test]
    fn already_finalized_is_relocated() {
        let ft = FinalizerThread::new();
        ft.enqueue(0xA0);
        ft.dequeue();
        let mut map = cratonvm_types::PointerMap::default();
        map.insert(0xA0, 0xB0);
        ft.update_after_gc(&map);
        assert!(ft.was_finalized(0xB0));
        assert!(!ft.was_finalized(0xA0), "the vacated address must not refuse");
    }

    // W1D-12. Deferred cleaner actions are visible for rooting.
    #[test]
    fn cleaner_pending_addresses_lists_the_queue() {
        let ct = CleanerThread::new();
        ct.submit_action(0x10);
        ct.submit_action(0x20);
        assert_eq!(ct.pending_addresses(), vec![0x10, 0x20]);
        let _ = ct.drain_actions();
        assert!(ct.pending_addresses().is_empty());
    }

    // -----------------------------------------------------------------------
    // gc-common w2-f — the application settles a reference itself.
    // -----------------------------------------------------------------------

    // W2F-1. `phantom.clear()` then the referent dies: never delivered
    //        (HotSpot never discovers a cleared reference). The pre-GC
    //        inference could not do this for a phantom.
    #[test]
    fn an_application_cleared_phantom_is_never_delivered() {
        let mut proc = ReferenceProcessor::new();
        proc.discover_reference(ReferenceType::Phantom, 0x10, 0x11, Some(0x90));
        proc.discover_reference(ReferenceType::Phantom, 0x20, 0x21, Some(0x90));
        assert!(proc.retire_by_application(0x10));
        assert!(!proc.retire_by_application(0x10), "idempotent");
        let result = proc.process_references(&always_dead, 64, 0);
        assert_eq!(result.to_enqueue, vec![(0x20, 0x90)]);
        assert_eq!(
            proc.pending_reference_object_addresses(),
            Vec::<usize>::new(),
            "a settled phantom is no longer a synthetic root"
        );
    }

    // W2F-2. G1's final remark runs `process_references` with no
    //        pre-collection pass: a weak/soft reference the application
    //        cleared since the last pause must not be cleared-and-enqueued
    //        there.
    #[test]
    fn a_reference_cleared_before_a_remark_is_not_enqueued_by_it() {
        let mut proc = ReferenceProcessor::new_with_policy(0);
        proc.discover_reference(ReferenceType::Weak, 0x10, 0x11, Some(0x90));
        proc.discover_reference(ReferenceType::Soft, 0x30, 0x31, Some(0x90));
        assert!(proc.retire_by_application(0x10));
        assert!(proc.retire_by_application(0x30));
        // No `weak_phantom_active_triples` / condemnation: the remark shape.
        let result = proc.process_references(&always_dead, 0, 10_000);
        assert!(result.to_enqueue.is_empty(), "{:?}", result.to_enqueue);
        assert!(proc.take_newly_cleared().is_empty(), "no slot write either");
        assert!(proc.soft_ref_lru_index.is_empty(), "settled soft row dropped its LRU key");
    }

    // W2F-3. A row the collector already settled keeps its verdict: a
    //        reference the GC enqueued is still delivered after a clear().
    #[test]
    fn a_collector_verdict_is_not_undone_by_a_later_application_clear() {
        let mut proc = ReferenceProcessor::new();
        proc.discover_reference(ReferenceType::Weak, 0x10, 0x11, Some(0x90));
        // A refused restore: cleared, enqueue deferred to the next round.
        assert!(proc.clear_after_refused_restore(0x10));
        assert!(!proc.retire_by_application(0x10));
        let result = proc.process_references(&always_live, 64, 0);
        assert_eq!(result.to_enqueue, vec![(0x10, 0x90)]);
    }

    // W2F-4. A `jdk.internal.ref.Cleaner` the application enqueued or cleared
    //        is never RUN by the collector. The old `mark_manually_enqueued`
    //        set `enqueued` without `action_emitted`, and the gather step runs
    //        every `runs_cleaner && enqueued && !action_emitted` phantom.
    #[test]
    fn a_settled_jdk_cleaner_is_never_run_by_the_collector() {
        let mut proc = ReferenceProcessor::new();
        proc.discover_phantom_cleaner(0x10, 0x11, Some(0x90));
        assert!(proc.mark_manually_enqueued(0x10));
        let result = proc.process_references(&always_dead, 64, 0);
        assert!(result.cleaner_actions.is_empty());
        assert!(result.to_enqueue.is_empty());
    }

    // W2F-5. `Finalizer.register`'s self-referent `Cleaner`-list row is not
    //        an application-settleable reference: an `enqueue()`/`clear()` on
    //        a finalizable object that is itself a `Reference` must not switch
    //        its `finalize()` off.
    #[test]
    fn a_self_referent_cleaner_row_is_not_retired() {
        let mut proc = ReferenceProcessor::new();
        proc.discover_reference(ReferenceType::Cleaner, 0x10, 0x10, None);
        assert!(!proc.mark_manually_enqueued(0x10));
        let result = proc.process_references(&always_dead, 64, 0);
        assert_eq!(result.cleaner_actions, vec![0x10]);
    }

    // W2F-6. The lazy index follows prunes, relocations and fresh discoveries,
    //        and prefers the active row when an address is shared.
    #[test]
    fn the_retirement_index_follows_prunes_moves_and_recycled_addresses() {
        let mut proc = ReferenceProcessor::new();
        proc.discover_reference(ReferenceType::Weak, 0x10, 0x11, Some(0x90));
        proc.discover_reference(ReferenceType::Weak, 0x20, 0x21, Some(0x90));
        proc.discover_reference(ReferenceType::Phantom, 0x30, 0x31, Some(0x90));
        assert!(!proc.retire_by_application(0x99), "unknown address");
        // Built now. A prune shifts 0x20 down to position 0.
        proc.remove_collected(&|a| a != 0x10);
        assert!(proc.retire_by_application(0x20));
        assert!(proc.weak_refs[0].cleared && proc.weak_refs[0].reference_obj == 0x20);
        // A move.
        let mut map = cratonvm_types::PointerMap::default();
        map.insert(0x30, 0x40);
        proc.update_after_gc(&map);
        assert!(!proc.retire_by_application(0x30), "the vacated address");
        assert!(proc.retire_by_application(0x40));
        // A recycled address: the settled row at 0x20 and a fresh one there.
        proc.discover_reference(ReferenceType::Weak, 0x20, 0x22, Some(0x90));
        assert!(proc.retire_by_application(0x20), "the fresh (active) row wins");
        assert!(proc.weak_refs.iter().all(|e| e.cleared));
        // And again after a rebuild.
        proc.discover_reference(ReferenceType::Soft, 0x20, 0x23, Some(0x90));
        proc.remove_collected(&|a| a != 0x40);
        assert!(proc.retire_by_application(0x20));
        assert!(proc.soft_refs[0].cleared);
    }

    // gcd d2/g. Two ACTIVE rows at one address (a dead `WeakReference`'s row
    // the concurrent sweep has not pruned yet, and a new `SoftReference` on
    // the freed block): a rebuilt index hands the application's clear to the
    // LATER registration, not to the row in the later list.
    #[test]
    fn gcd_d2g_the_later_registration_owns_a_shared_active_address() {
        let mut proc = ReferenceProcessor::new();
        proc.discover_reference(ReferenceType::Weak, 0x10, 0x11, Some(0x90));
        proc.discover_reference(ReferenceType::Weak, 0x50, 0x51, Some(0x90));
        assert!(!proc.retire_by_application(0x99), "builds the index");
        // Any prune marks the index stale; the fresh row below is then not
        // inserted incrementally, so the lookup rebuilds.
        proc.remove_collected(&|a| a != 0x50);
        proc.discover_reference(ReferenceType::Weak, 0x10, 0x11, Some(0x90));
        proc.discover_reference(ReferenceType::Soft, 0x10, 0x12, Some(0x90));
        assert!(proc.retire_by_application(0x10));
        assert!(proc.soft_refs[0].cleared, "the later (soft) registration is retired");
        assert!(
            proc.weak_refs.iter().filter(|e| e.reference_obj == 0x10).all(|e| !e.cleared),
            "not the earlier weak row at the same address"
        );
    }

    // gcd d3/n (`gcd-d2g-soft-touch-index-prefers-a-dead-row-at-a-reused-
    // address`). A dead `SoftReference`'s row the concurrent sweep has not
    // pruned yet and a new `SoftReference` on the freed block share `0x10`:
    // `get()` on the new one touches the NEW row -- at discovery, after a
    // prune's rebuild and after a relocation's rebuild -- and the steal moves
    // the stamped-touch epoch.
    #[test]
    fn gcd_d3n_a_soft_touch_lands_on_the_later_registration_at_a_shared_address() {
        let mut proc = ReferenceProcessor::new_with_policy(1000);
        proc.discover_reference(ReferenceType::Soft, 0x10, 0x11, None); // seq 0, dead
        proc.discover_reference(ReferenceType::Soft, 0x30, 0x31, None); // seq 1
        let epoch = proc.soft_touch_epoch_now();
        proc.discover_reference(ReferenceType::Soft, 0x10, 0x12, None); // seq 2, new
        assert_ne!(proc.soft_touch_epoch_now(), epoch, "a steal bumps the epoch");
        proc.touch_soft_reference(0x10, 5_000);
        assert_eq!(proc.soft_refs[2].last_access_time_ms, 5_000, "the new row");
        assert_eq!(proc.soft_refs[0].last_access_time_ms, 0, "not the dead one");
        // A prune elsewhere rebuilds the index: still the new row.
        proc.remove_collected(&|a| a != 0x30);
        assert_eq!(proc.soft_refs.len(), 2);
        proc.touch_soft_reference(0x10, 6_000);
        assert_eq!(proc.soft_refs[1].referent, 0x12);
        assert_eq!(proc.soft_refs[1].last_access_time_ms, 6_000);
        assert_eq!(proc.soft_refs[0].last_access_time_ms, 0);
        // A relocation rebuild (some soft row moved): still the new row.
        let mut map = cratonvm_types::PointerMap::default();
        map.insert(0x10usize, 0x70usize);
        proc.update_after_gc(&map);
        proc.touch_soft_reference(0x70, 7_000);
        assert_eq!(proc.soft_refs[1].last_access_time_ms, 7_000);
        assert_eq!(proc.soft_refs[0].last_access_time_ms, 0);
        // The LRU index stays in step with the rows it names.
        for (&(ts, idx), &v) in proc.soft_ref_lru_index.iter() {
            assert_eq!(idx, v);
            assert_eq!(proc.soft_refs[idx].last_access_time_ms, ts);
        }
        // A plain re-discovery of a fresh address steals nothing.
        let epoch = proc.soft_touch_epoch_now();
        proc.discover_reference(ReferenceType::Soft, 0x90, 0x91, None);
        assert_eq!(proc.soft_touch_epoch_now(), epoch);
    }

    // r11w13-rt-array-alias-audit-jni-refs-gc, open item 1 (round 11 wave
    // 16, lane vh): a plain class-id stamp keeps its old meaning, and a
    // kind-aware stamp tells an `Integer[]` from an `Integer` that share the
    // component's class id.
    #[test]
    fn a_kind_aware_referent_stamp_refuses_the_array_component_alias() {
        const INTEGER: u32 = 9;
        // Plain stamp: class id only, kind ignored (the pre-wave-16 screen).
        assert!(referent_stamp_admits(INTEGER, INTEGER, false));
        assert!(referent_stamp_admits(INTEGER, INTEGER, true));
        assert!(!referent_stamp_admits(INTEGER, 10, false));
        // Kind-aware stamps.
        let plain = INTEGER | REFERENT_STAMP_KIND_KNOWN;
        let array = INTEGER | REFERENT_STAMP_KIND_KNOWN | REFERENT_STAMP_ARRAY;
        assert!(referent_stamp_admits(plain, INTEGER, false));
        assert!(!referent_stamp_admits(plain, INTEGER, true), "Integer[] reused an Integer's address");
        assert!(referent_stamp_admits(array, INTEGER, true));
        assert!(!referent_stamp_admits(array, INTEGER, false), "Integer reused an Integer[]'s address");
        assert!(!referent_stamp_admits(array, 10, true));
        // `java.lang.Object` (class 0) becomes stampable once the kind is known.
        let object = REFERENT_STAMP_KIND_KNOWN;
        assert!(referent_stamp_admits(object, 0, false));
        assert!(!referent_stamp_admits(object, 0, true), "an Object[] is not an Object");
        let mut proc = ReferenceProcessor::new_with_policy(1000);
        proc.stamp_referent_class(0xAB, object);
        assert_eq!(proc.referent_class_stamp(0xAB), Some(object));
    }

    // W2F-7. `update_after_gc`'s skips: an empty map, or one that moves no
    //        key (a non-moving cycle's identity entries), leaves every index
    //        resolving exactly as before.
    #[test]
    fn a_map_that_moves_no_key_leaves_every_index_resolvable() {
        let mut proc = ReferenceProcessor::new_with_policy(1000);
        proc.discover_reference(ReferenceType::Soft, 0xAA, 0x10, Some(0x90));
        proc.stamp_reference(0xAA, 7);
        proc.stamp_referent_class(0xAA, 9);
        let mut map = cratonvm_types::PointerMap::default();
        map.insert(0xAA, 0xAA);
        map.insert(0x10, 0x10);
        proc.update_after_gc(&map);
        proc.update_after_gc(&cratonvm_types::PointerMap::default());
        assert_eq!(proc.identity_stamp(0xAA), Some(7));
        assert_eq!(proc.referent_class_stamp(0xAA), Some(9));
        proc.touch_soft_reference(0xAA, 1234);
        assert_eq!(proc.soft_refs[0].last_access_time_ms, 1234);
        assert!(proc.retire_by_application(0xAA));
    }

    // ======================================================================
    // gc-common w3-d (2026-09-23): the reference-delivery thread's channel
    // ======================================================================

    // W3D-1. A request made before the wait is not lost, and a served signal
    //        waits (and times out) until the next request.
    #[test]
    fn a_delivery_request_is_never_lost() {
        let s = DeliverySignal::new();
        assert!(!s.wait_for_request(Duration::from_millis(1)), "nothing owed yet");
        s.request();
        assert!(s.wait_for_request(Duration::from_millis(1)));
        // Still owed until a batch covering it finishes.
        assert!(s.wait_for_request(Duration::from_millis(1)));
        let ticket = s.begin_batch();
        s.finish_batch(ticket);
        assert!(!s.wait_for_request(Duration::from_millis(1)));
    }

    // W3D-2. Work requested DURING a batch is owed another batch: the ticket is
    //        the request count when the batch opened.
    #[test]
    fn work_requested_during_a_batch_is_owed_another_batch() {
        let s = DeliverySignal::new();
        s.request();
        let ticket = s.begin_batch();
        s.request(); // e.g. a finalizer's own allocation collected
        s.finish_batch(ticket);
        assert!(s.wait_for_request(Duration::from_millis(1)));
        assert!(
            !s.wait_until_served(Duration::from_millis(1)),
            "the second request is not served yet"
        );
        let ticket = s.begin_batch();
        s.finish_batch(ticket);
        assert!(s.wait_until_served(Duration::from_millis(1)));
    }

    // W3D-3. `System.gc()`'s wait returns once a batch opened after its request
    //        finishes on another thread, and a stale ticket never moves
    //        `served` backwards.
    #[test]
    fn a_requester_waits_for_the_batch_that_covers_it() {
        let s = Arc::new(DeliverySignal::new());
        s.request();
        let server = {
            let s = Arc::clone(&s);
            std::thread::spawn(move || {
                assert!(s.wait_for_request(Duration::from_secs(10)));
                let ticket = s.begin_batch();
                s.finish_batch(ticket);
                s.finish_batch(0); // stale: must not undo the batch above
            })
        };
        assert!(s.wait_until_served(Duration::from_secs(10)));
        server.join().unwrap();
        assert!(s.wait_until_served(Duration::from_millis(1)));
    }

    // W3D-4. Queue wake-ups: deduplicated, relocated with the actions,
    //        reported as roots, and gone once drained.
    #[test]
    fn queue_wake_ups_are_deduplicated_relocated_and_rooted() {
        let ct = CleanerThread::new();
        assert!(!ct.has_pending_work());
        ct.note_queues_to_notify(&[0x30, 0x10, 0x30]);
        ct.note_queues_to_notify(&[0x10, 0x20]);
        ct.note_queues_to_notify(&[]);
        assert!(ct.has_pending_work());
        let mut map = cratonvm_types::PointerMap::default();
        map.insert(0x10, 0x90);
        map.insert(0x20, 0x30); // lands on another entry's address: one entry
        ct.update_after_gc(&map);
        let mut rooted = ct.pending_addresses();
        rooted.sort_unstable();
        assert_eq!(rooted, vec![0x30, 0x90]);
        assert_eq!(ct.drain_queues_to_notify(), vec![0x30, 0x90]);
        assert!(!ct.has_pending_work());
        assert!(ct.pending_addresses().is_empty());
        // Actions and wake-ups are separate: draining one leaves the other.
        ct.submit_action(0x40);
        ct.note_queues_to_notify(&[0x50]);
        assert_eq!(ct.drain_actions(), vec![0x40]);
        assert!(ct.has_pending_work());
        assert_eq!(ct.pending_count(), 0);
    }

    // W3D-5. One start claim per VM, handed back on a failed spawn; the
    //        delivery thread's id round-trips (thread id 0 included).
    #[test]
    fn the_delivery_thread_is_claimed_once_and_its_id_round_trips() {
        let ft = FinalizerThread::new();
        assert!(!ft.delivery_thread_claimed());
        assert_eq!(ft.delivery_thread(), None);
        assert!(!ft.request_delivery(), "nobody serves yet");
        assert!(ft.claim_delivery_thread_start());
        assert!(!ft.claim_delivery_thread_start());
        ft.release_delivery_thread_start();
        assert!(ft.claim_delivery_thread_start());
        ft.set_delivery_thread(0);
        assert_eq!(ft.delivery_thread(), Some(0));
        assert!(ft.request_delivery());
        // Both requests above were recorded for the (starting) thread.
        let signal = ft.delivery_signal();
        assert!(signal.wait_for_request(Duration::from_millis(1)));
    }

    // W4D-1. A sliced wait keeps ONE target: requests made after the ticket
    //        was taken do not extend it, and a batch opened before the ticket
    //        does not satisfy it.
    #[test]
    fn a_ticket_is_satisfied_by_the_first_batch_that_covers_it() {
        let s = DeliverySignal::new();
        s.request();
        let early = s.begin_batch(); // opened before our request
        s.request();
        let target = s.ticket();
        s.finish_batch(early);
        assert!(
            !s.wait_for_ticket(target, Duration::from_millis(1)),
            "a batch opened before the ticket does not cover it"
        );
        let covering = s.begin_batch();
        s.request(); // another thread's later request
        s.finish_batch(covering);
        assert!(
            s.wait_for_ticket(target, Duration::from_millis(1)),
            "a later request must not move a ticket already taken"
        );
        assert!(
            !s.wait_until_served(Duration::from_millis(1)),
            "the later request itself is still owed a batch"
        );
    }

    // W4D-2. A last-ditch soft-clear request is per processor, consumed
    //        exactly once, and repeat requests before a collection collapse.
    #[test]
    fn a_clear_all_soft_request_is_taken_exactly_once() {
        let mut rp = ReferenceProcessor::new();
        assert!(!rp.take_clear_all_soft_request(), "nothing requested");
        rp.request_clear_all_soft_refs();
        rp.request_clear_all_soft_refs();
        assert!(rp.take_clear_all_soft_request());
        assert!(!rp.take_clear_all_soft_request(), "consumed by one collection");
        // Another VM's processor never sees it.
        let mut other = ReferenceProcessor::new();
        rp.request_clear_all_soft_refs();
        assert!(!other.take_clear_all_soft_request());
        assert!(rp.take_clear_all_soft_request());
    }

    // W4D-3. A reported finalizable object is queued past the old capacity:
    //        its row is already flagged enqueued, so a refusal would lose its
    //        `finalize()` for good. The address-keyed `enqueue` keeps its cap.
    #[test]
    fn enqueue_unfinalized_never_drops_past_the_old_capacity() {
        let ft = FinalizerThread::new();
        for i in 0..=FINALIZER_QUEUE_MAX_CAPACITY {
            assert!(ft.enqueue_unfinalized((i + 1) * 16));
        }
        assert_eq!(ft.pending_count(), FINALIZER_QUEUE_MAX_CAPACITY + 1);
        assert_eq!(ft.dropped_count(), 0);
        assert!(
            !ft.enqueue_unfinalized(16),
            "a duplicate already waiting is still refused"
        );
        assert!(!ft.enqueue(0xFFFF_0000), "`enqueue` keeps its cap");
        assert_eq!(ft.dropped_count(), 1);
        assert_eq!(ft.dequeue(), Some(16), "FIFO order is kept");
    }

    // W5D-1. `in_flight` covers a batch from its request to its finish,
    //        including the stretch in which the batch has emptied the queues
    //        but is still running what it took (the window
    //        `waitForReferenceProcessing` exists to wait out).
    #[test]
    fn a_batch_is_in_flight_from_request_to_finish() {
        let s = DeliverySignal::new();
        assert!(!s.in_flight(), "nothing requested");
        s.request();
        assert!(s.in_flight(), "requested, not started");
        let t = s.begin_batch();
        assert!(s.in_flight(), "running");
        s.finish_batch(t);
        assert!(!s.in_flight(), "served");
        let t = s.begin_batch();
        s.request(); // arrives mid-batch
        s.finish_batch(t);
        assert!(s.in_flight(), "a request made during a batch is still owed");
    }

    // ======================================================================
    // gc-common w6-d (2026-09-24)
    // ======================================================================

    /// Deterministic generator for the model tests below (no `rand` here).
    struct W6dLcg(u64);
    impl W6dLcg {
        fn advance(&mut self) -> u64 {
            self.0 = self
                .0
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            self.0 >> 33
        }
        fn below(&mut self, n: u64) -> u64 {
            self.advance() % n
        }
    }

    /// The VM's stamped touch (`NativeContextImpl::touch_soft_reference`),
    /// with the object's `SoftReference.timestamp` field modelled by `fields`:
    /// compare the field with the stamp WITHOUT the lock; otherwise touch and
    /// stamp.
    fn w6d_stamped_touch(
        proc: &mut ReferenceProcessor,
        fields: &mut FxHashMap<usize, i64>,
        addr: usize,
        now_ms: u64,
    ) {
        let epoch = proc.soft_touch_epoch();
        if let Some(stamp) = soft_touch_stamp(now_ms, epoch.load(Ordering::Acquire)) {
            if fields.get(&addr) == Some(&stamp) {
                return;
            }
        }
        proc.touch_soft_reference(addr, now_ms);
        // No stamp for a `0` clock: reset the field, as the VM does, so it
        // stops vouching for the row state an earlier stamp named.
        let stamp = soft_touch_stamp(now_ms, proc.soft_touch_epoch_now()).unwrap_or(0);
        if let Some(field) = fields.get_mut(&addr) {
            *field = stamp;
        }
    }

    fn w6d_assert_same_soft_state(a: &ReferenceProcessor, b: &ReferenceProcessor, at: &str) {
        let rows = |p: &ReferenceProcessor| -> Vec<(usize, usize, u64, bool, bool)> {
            p.soft_refs
                .iter()
                .map(|e| {
                    (
                        e.reference_obj,
                        e.referent,
                        e.last_access_time_ms,
                        e.cleared,
                        e.enqueued,
                    )
                })
                .collect()
        };
        assert_eq!(rows(a), rows(b), "{at}: soft rows differ");
        assert_eq!(
            a.soft_ref_lru_index, b.soft_ref_lru_index,
            "{at}: LRU index differs"
        );
        assert_eq!(
            a.soft_ref_addr_index, b.soft_ref_addr_index,
            "{at}: address index differs"
        );
        assert_eq!(
            a.last_observed_clock_ms, b.last_observed_clock_ms,
            "{at}: mutator clock differs"
        );
    }

    /// One random history, replayed through the locked touch (`plain`) and the
    /// stamped one (`stamped`). Everything else is applied to both.
    fn w6d_run_soft_touch_model(seed: u64, steps: usize) {
        const SLOTS: u64 = 10;
        let mut rng = W6dLcg(seed);
        let mut plain = ReferenceProcessor::new_with_policy(1);
        let mut stamped = ReferenceProcessor::new_with_policy(1);
        // Live SoftReference objects: address -> their `timestamp` field
        // (a fresh object reads 0). A dead object leaves this map at once; its
        // row may linger until a prune, which is how a recycled address meets
        // a stale row.
        let mut live: FxHashMap<usize, i64> = FxHashMap::default();
        let mut now: u64 = 1_000;
        for step in 0..steps {
            let at = format!("seed {seed} step {step}");
            match rng.below(100) {
                // `new SoftReference<>(x)`: discover, then the constructor's touch.
                0..=14 => {
                    let addr = 0x1000 * (1 + rng.below(SLOTS) as usize);
                    if live.contains_key(&addr) {
                        continue;
                    }
                    live.insert(addr, 0);
                    let referent = 0x100 + 8 * rng.below(6) as usize;
                    plain.discover_reference(ReferenceType::Soft, addr, referent, None);
                    stamped.discover_reference(ReferenceType::Soft, addr, referent, None);
                    plain.touch_soft_reference(addr, now);
                    w6d_stamped_touch(&mut stamped, &mut live, addr, now);
                }
                // `get()` on a live object.
                15..=59 => {
                    let addrs: Vec<usize> = {
                        let mut v: Vec<usize> = live.keys().copied().collect();
                        v.sort_unstable();
                        v
                    };
                    if addrs.is_empty() {
                        continue;
                    }
                    let addr = addrs[rng.below(addrs.len() as u64) as usize];
                    plain.touch_soft_reference(addr, now);
                    w6d_stamped_touch(&mut stamped, &mut live, addr, now);
                }
                // The clock: mostly the same millisecond, sometimes forward,
                // rarely backwards (a `SystemTime` step), rarely `0`.
                60..=69 => {
                    now = match rng.below(40) {
                        0 => 0,
                        1..=2 => now.saturating_sub(1),
                        3..=24 => now,
                        _ => now + rng.below(3),
                    };
                }
                // An object dies; its row stays until a prune.
                70..=77 => {
                    let mut v: Vec<usize> = live.keys().copied().collect();
                    v.sort_unstable();
                    if !v.is_empty() {
                        let victim = v[rng.below(v.len() as u64) as usize];
                        live.remove(&victim);
                    }
                }
                // A collection: prune the dead, then maybe move survivors
                // (to free addresses; stale rows may sit there).
                78..=89 => {
                    let alive = live.clone();
                    let is_live = |a: usize| alive.contains_key(&a);
                    if rng.below(2) == 0 {
                        plain.remove_collected(&is_live);
                        stamped.remove_collected(&is_live);
                    }
                    let mut map = cratonvm_types::PointerMap::default();
                    let mut moved: FxHashMap<usize, i64> = FxHashMap::default();
                    let mut sorted: Vec<(usize, i64)> = live.iter().map(|(&a, &f)| (a, f)).collect();
                    sorted.sort_unstable();
                    for (addr, field) in sorted {
                        let target = if rng.below(3) == 0 {
                            0x1000 * (1 + rng.below(SLOTS) as usize)
                        } else {
                            addr
                        };
                        let target = if moved.contains_key(&target)
                            || (target != addr && live.contains_key(&target))
                        {
                            addr
                        } else {
                            target
                        };
                        if moved.contains_key(&target) {
                            // Unreachable by construction (no survivor may move
                            // onto another's current address); the length check
                            // below abandons the relocation if it ever is.
                            continue;
                        }
                        if rng.below(2) == 0 || target != addr {
                            map.insert(addr, target);
                        }
                        moved.insert(target, field);
                    }
                    if moved.len() == live.len() {
                        plain.update_after_gc(&map);
                        stamped.update_after_gc(&map);
                        live = moved;
                    }
                }
                // Reference processing, with random pressure and marks.
                90..=95 => {
                    let marks: Vec<bool> = (0..6).map(|_| rng.below(2) == 0).collect();
                    let is_marked = |a: usize| -> bool {
                        if (0x100..0x100 + 8 * 6).contains(&a) {
                            marks[(a - 0x100) / 8]
                        } else {
                            true
                        }
                    };
                    let free_mb = rng.below(3) as usize;
                    let clock = if rng.below(2) == 0 { 0 } else { now };
                    let _ = plain.process_references(&is_marked, free_mb, clock);
                    let _ = stamped.process_references(&is_marked, free_mb, clock);
                }
                // The pre-collection condemnation, then its close-out.
                _ => {
                    let free_mb = rng.below(3) as usize;
                    let a = plain.condemn_idle_soft_refs(free_mb, now);
                    let b = stamped.condemn_idle_soft_refs(free_mb, now);
                    assert_eq!(a, b, "{at}: condemned sets differ");
                    plain.finish_pre_gc_cycle();
                    stamped.finish_pre_gc_cycle();
                }
            }
            w6d_assert_same_soft_state(&plain, &stamped, &at);
        }
    }

    // W6D-1. The stamped touch leaves the processor exactly as the locked
    //        touch would, over random histories of discovery, `get()`, clock
    //        steps (including backwards), deaths without a prune (recycled
    //        addresses meeting stale rows), prunes, relocations, processing
    //        and condemnation.
    #[test]
    fn w6d_stamped_soft_touch_is_equivalent_to_the_locked_touch() {
        for seed in 1..=96u64 {
            w6d_run_soft_touch_model(seed, 600);
        }
    }

    // W6D-2. The stamp never collides with a fresh object's field, declines a
    //        missing clock, and changes with the epoch.
    #[test]
    fn w6d_the_soft_touch_stamp_encodes_clock_and_epoch() {
        assert_eq!(soft_touch_stamp(0, 7), None, "no clock, no stamp");
        assert_eq!(soft_touch_stamp(1u64 << 43, 0), None, "out of range");
        let a = soft_touch_stamp(1_700_000_000_000, 5).expect("a real clock stamps");
        assert_ne!(a, 0, "a stamp is never a fresh field");
        assert_ne!(
            Some(a),
            soft_touch_stamp(1_700_000_000_000, 6),
            "a new epoch voids the stamp"
        );
        assert_ne!(
            Some(a),
            soft_touch_stamp(1_700_000_000_001, 5),
            "a new millisecond voids the stamp"
        );
    }

    // W6D-3. The epoch moves exactly when the address -> row resolution may:
    //        a moving map and a soft prune bump it; an empty map, a touch, a
    //        discovery and processing do not.
    #[test]
    fn w6d_the_soft_touch_epoch_moves_with_moves_and_prunes() {
        let mut proc = ReferenceProcessor::new();
        let e0 = proc.soft_touch_epoch_now();
        proc.discover_reference(ReferenceType::Soft, 0x10, 0x11, None);
        proc.touch_soft_reference(0x10, 5_000);
        let _ = proc.process_references(&always_live, 64, 5_000);
        proc.update_after_gc(&cratonvm_types::PointerMap::default());
        assert_eq!(proc.soft_touch_epoch_now(), e0);
        let mut map = cratonvm_types::PointerMap::default();
        map.insert(0x10, 0x20);
        proc.update_after_gc(&map);
        let e1 = proc.soft_touch_epoch_now();
        assert!(e1 > e0, "a moving map bumps");
        proc.remove_collected(&always_live);
        assert_eq!(proc.soft_touch_epoch_now(), e1, "nothing pruned, no bump");
        proc.remove_collected(&always_dead);
        assert!(proc.soft_touch_epoch_now() > e1, "a soft prune bumps");
    }

    // W6D-4. A weak reference to a finalizable object the collection found
    //        unreachable and resurrected is CLEARED (HotSpot processes Weak
    //        before Final); the phantom to it waits for finalization, and the
    //        noted set lasts one round.
    #[test]
    fn w6d_a_weak_ref_to_a_resurrected_finalizable_is_cleared() {
        const OBJ: usize = 0x200;
        let mut proc = ReferenceProcessor::new();
        proc.discover_reference(ReferenceType::Finalizer, OBJ, OBJ, None);
        proc.discover_reference(ReferenceType::Weak, 0x300, OBJ, Some(0x400));
        proc.discover_reference(ReferenceType::Soft, 0x500, OBJ, None);
        proc.discover_reference(ReferenceType::Phantom, 0x600, OBJ, Some(0x400));
        // The collector marked OBJ only to resurrect it.
        proc.note_resurrected_finalizables(&[OBJ]);
        let result = proc.process_references(&always_live, 0, 1_000_000);
        assert!(proc.weak_refs[0].cleared, "weak cleared before finalization");
        assert!(proc.soft_refs[0].cleared, "soft cleared under pressure");
        assert!(
            !proc.phantom_refs[0].enqueued,
            "the phantom waits until the object is finalized"
        );
        assert_eq!(result.to_enqueue, vec![(0x300, 0x400)]);
        assert!(
            result.to_finalize.is_empty(),
            "the resurrected row is flagged by `mark_finalizer_enqueued`, not phase 3"
        );
        // One round only: a second weak ref to OBJ is kept next time.
        proc.discover_reference(ReferenceType::Weak, 0x700, OBJ, None);
        let _ = proc.process_references(&always_live, 0, 1_000_000);
        assert!(!proc.weak_refs[1].cleared, "the noted set does not span rounds");
    }

    // W6D-5. Without a noted set nothing changes (the V18 rows above), and a
    //        strongly reachable object that merely has a finalizer keeps its
    //        weak references; `finish_pre_gc_cycle` drops an unconsumed set.
    #[test]
    fn w6d_only_the_noted_objects_lose_their_weak_refs() {
        let mut proc = ReferenceProcessor::new();
        proc.discover_reference(ReferenceType::Finalizer, 0x200, 0x200, None);
        proc.discover_reference(ReferenceType::Finalizer, 0x210, 0x210, None);
        proc.discover_reference(ReferenceType::Weak, 0x300, 0x200, None);
        proc.discover_reference(ReferenceType::Weak, 0x310, 0x210, None);
        proc.note_resurrected_finalizables(&[0x200]);
        let _ = proc.process_references(&always_live, 64, 0);
        assert!(proc.weak_refs[0].cleared);
        assert!(!proc.weak_refs[1].cleared, "0x210 is strongly reachable");

        let mut other = ReferenceProcessor::new();
        other.discover_reference(ReferenceType::Weak, 0x300, 0x200, None);
        other.note_resurrected_finalizables(&[0x200]);
        other.finish_pre_gc_cycle();
        let _ = other.process_references(&always_live, 64, 0);
        assert!(!other.weak_refs[0].cleared, "a finished cycle's set is gone");
    }

    // W7D-1. Depth 2 needs nothing new from the processor: handed the whole
    //        resurrection closure (the finalizable object AND what the
    //        collector marked only through it), a weak reference to the child
    //        is cleared, the phantom to the child still waits for
    //        finalization, and a child that is also strongly reachable (never
    //        in the closure, by the collector's contract) keeps its weak
    //        reference. `handoff-w7d-collectors-report-the-resurrection-closure`.
    #[test]
    fn w7d_the_resurrection_closure_clears_weak_refs_at_depth_two() {
        const PARENT: usize = 0x200;
        const CHILD: usize = 0x240;
        const SHARED: usize = 0x280;
        let mut proc = ReferenceProcessor::new();
        proc.discover_reference(ReferenceType::Finalizer, PARENT, PARENT, None);
        proc.discover_reference(ReferenceType::Weak, 0x300, CHILD, Some(0x400));
        proc.discover_reference(ReferenceType::Weak, 0x310, SHARED, Some(0x400));
        proc.discover_reference(ReferenceType::Phantom, 0x320, CHILD, Some(0x400));
        proc.discover_reference(ReferenceType::Soft, 0x330, CHILD, None);
        // The collector marked PARENT and CHILD only in its resurrection
        // drain; SHARED was reached by the strong closure first.
        proc.note_resurrected_finalizables(&[PARENT, CHILD]);
        let result = proc.process_references(&always_live, 0, 1_000_000);
        assert!(proc.weak_refs[0].cleared, "weak to the child cleared");
        assert!(!proc.weak_refs[1].cleared, "a strongly reachable object keeps it");
        assert!(proc.soft_refs[0].cleared, "soft to the child cleared under pressure");
        assert!(
            !proc.phantom_refs[0].enqueued,
            "the phantom to the child waits until the parent is finalized"
        );
        assert_eq!(result.to_enqueue, vec![(0x300, 0x400)]);
        assert!(result.to_finalize.is_empty());
    }

    // W7D-2. The GC-notification hand-off mark is raised by a door and taken
    //        exactly once by the delivery thread's batch.
    #[test]
    fn w7d_the_gc_notification_hand_off_mark_is_taken_once() {
        let ft = FinalizerThread::new();
        assert!(!ft.take_gc_notifications_handed_off(), "starts lowered");
        ft.note_gc_notifications_handed_off();
        ft.note_gc_notifications_handed_off();
        assert!(ft.take_gc_notifications_handed_off());
        assert!(!ft.take_gc_notifications_handed_off(), "taken once");
    }

    // W18G-1. A second registration of a finalizable object whose row is
    //         still active is a no-op: one row, one `finalize()`.
    //         `handoff-w2d-finalizer-registration-native` item 4.
    #[test]
    fn w18g_a_second_finalizer_registration_is_a_no_op() {
        let mut proc = ReferenceProcessor::new();
        proc.discover_reference(ReferenceType::Finalizer, 0x100, 0x100, None);
        proc.discover_reference(ReferenceType::Finalizer, 0x100, 0x100, None);
        proc.discover_reference(ReferenceType::Finalizer, 0x140, 0x140, None);
        assert_eq!(proc.finalizer_refs.len(), 2);
        assert!(proc.is_registered_finalizable(0x100));
        let result = proc.process_references(&always_dead, 64, 0);
        assert_eq!(result.to_finalize, vec![0x100, 0x140]);
        assert_eq!(result.stats.finalizer_refs_enqueued, 2);
    }

    // W18G-2. A row that was already ENQUEUED does not refuse its address: a
    //         new finalizable object allocated where a finalized one used to
    //         live must be registered (the w5-d warning on this handoff).
    #[test]
    fn w18g_an_enqueued_row_does_not_refuse_a_recycled_address() {
        let mut proc = ReferenceProcessor::new();
        proc.discover_reference(ReferenceType::Finalizer, 0x100, 0x100, None);
        // Warm the index so the enqueue below has to invalidate it.
        assert!(proc.is_registered_finalizable(0x100));
        let first = proc.process_references(&always_dead, 64, 0);
        assert_eq!(first.to_finalize, vec![0x100]);
        assert!(!proc.is_registered_finalizable(0x100));
        // The same verdict through the resurrection channel.
        proc.discover_reference(ReferenceType::Finalizer, 0x180, 0x180, None);
        proc.mark_finalizer_enqueued(&[0x180]);
        assert!(!proc.is_registered_finalizable(0x180));

        proc.discover_reference(ReferenceType::Finalizer, 0x100, 0x100, None);
        proc.discover_reference(ReferenceType::Finalizer, 0x180, 0x180, None);
        assert_eq!(proc.finalizer_refs.len(), 4, "both recycled addresses registered");
        let second = proc.process_references(&always_dead, 64, 0);
        assert_eq!(second.to_finalize, vec![0x100, 0x180]);
    }

    // W18G-3. The index follows a moving collection and a prune: the old
    //         address stops refusing, the new one refuses.
    #[test]
    fn w18g_the_registration_index_follows_relocation_and_prune() {
        let mut proc = ReferenceProcessor::new();
        proc.discover_reference(ReferenceType::Finalizer, 0x100, 0x100, None);
        proc.discover_reference(ReferenceType::Finalizer, 0x200, 0x200, None);
        assert!(proc.is_registered_finalizable(0x100));
        let mut map = cratonvm_types::PointerMap::default();
        map.insert(0x100, 0x900);
        proc.update_after_gc(&map);
        assert!(!proc.is_registered_finalizable(0x100));
        assert!(proc.is_registered_finalizable(0x900));
        proc.discover_reference(ReferenceType::Finalizer, 0x900, 0x900, None);
        assert_eq!(proc.finalizer_refs.len(), 2, "the moved object is still registered once");

        // The row at 0x200 is pruned (its object is gone without a verdict);
        // a new object there registers again.
        proc.remove_collected(&|a| a != 0x200);
        assert!(!proc.is_registered_finalizable(0x200));
        proc.discover_reference(ReferenceType::Finalizer, 0x200, 0x200, None);
        assert_eq!(proc.finalizer_refs.len(), 2);
        assert!(proc.is_registered_finalizable(0x200));
    }

    /// gen r5w3/unload7 (`gengc-r5w1-refs5-proposal-processor-reports-what-it-retired`):
    /// the round's `retired` list plus the pre-pass's dropped ACTIVE rows is
    /// exactly the before/after difference of `active_reference_object_set`
    /// the generational remark used to compute with two hash-set builds.
    #[test]
    fn r5w3_retired_rows_equal_the_active_set_difference() {
        let mut proc = ReferenceProcessor::new_with_policy(1000);
        let queue = 0x9000usize;
        // Weak: dead referent (retires, enqueued), live referent (stays).
        proc.discover_reference(ReferenceType::Weak, 0x1000, 0x2000, Some(queue));
        proc.discover_reference(ReferenceType::Weak, 0x1100, 0x2100, None);
        // Soft: idle and dead (retires), strongly reachable (stays).
        proc.discover_reference(ReferenceType::Soft, 0x1200, 0x2200, None);
        proc.discover_reference(ReferenceType::Soft, 0x1210, 0x2210, None);
        // Phantom: dead referent (retires), live referent (stays).
        proc.discover_reference(ReferenceType::Phantom, 0x1300, 0x2300, Some(queue));
        proc.discover_reference(ReferenceType::Phantom, 0x1310, 0x2310, None);
        // A weak row whose own `Reference` died: the pre-pass drops it.
        proc.discover_reference(ReferenceType::Weak, 0x1400, 0x2400, None);
        // A finalizer row never appears in either set.
        proc.discover_reference(ReferenceType::Finalizer, 0x2500, 0x2500, None);

        let live = [
            0x1000usize,
            0x1100,
            0x1200,
            0x1210,
            0x1300,
            0x1310,
            queue,
            0x2100,
            0x2210,
            0x2310,
        ];
        let before = proc.active_reference_object_set();
        let pruned = proc.remove_collected_reference_objects(&survivor(&live));
        // `free_heap_mb = 0`: every idle soft entry with an unmarked referent
        // is condemned by the LRU rule.
        let result = proc.process_references(&survivor(&live), 0, 1_000_000);
        let after = proc.active_reference_object_set();

        assert_eq!(pruned, 1, "the dead weak Reference's active row");
        let diff: rustc_hash::FxHashSet<usize> = before.difference(&after).copied().collect();
        let mut reported: rustc_hash::FxHashSet<usize> = result.retired.iter().copied().collect();
        assert_eq!(reported.len(), result.retired.len(), "no row reported twice");
        assert!(!reported.contains(&0x2500), "finalizer rows are not retirements");
        for kept in [0x1100usize, 0x1210, 0x1310] {
            assert!(!reported.contains(&kept), "{kept:#x} is still active");
        }
        reported.insert(0x1400);
        assert_eq!(reported, diff);
        // A second round retires nothing new.
        let again = proc.process_references(&survivor(&live), 0, 1_000_000);
        assert!(again.retired.is_empty(), "{:?}", again.retired);
    }

    /// gcd d1/c (`gengc-r5w5-conc9-concurrent-sweep-leaves-reference-rows-of-freed-references`):
    /// the concurrent sweep's prune drops the soft / weak / phantom rows of
    /// the `Reference`s it freed, registered before its snapshot, and only
    /// those: a row registered after the snapshot at a freed (re-issued)
    /// address, a row outside the freed range and every finalizer row stay.
    #[test]
    fn gcd_d1c_sweep_prune_drops_only_rows_registered_before_it_in_freed_blocks() {
        let mut proc = ReferenceProcessor::new_with_policy(1000);
        proc.discover_reference(ReferenceType::Weak, 0x1000, 0x5000, None);
        proc.discover_reference(ReferenceType::Soft, 0x1010, 0x5010, None);
        proc.discover_reference(ReferenceType::Phantom, 0x1020, 0x5020, Some(0x9000));
        proc.discover_reference(ReferenceType::Weak, 0x2000, 0x5030, None);
        proc.discover_reference(ReferenceType::Finalizer, 0x1030, 0x1030, None);
        let snapshot = proc.registration_seq();
        assert_eq!(snapshot, 5, "one number per discovered row");
        // A NEW `Reference` allocated on a block the sweep is freeing.
        proc.discover_reference(ReferenceType::Weak, 0x1000, 0x6000, None);
        assert_eq!(proc.registration_seq(), 6);

        // The sweep freed [0x1000, 0x1040).
        let freed = |a: usize| (0x1000..0x1040).contains(&a);
        let dropped = proc.remove_reference_objects_registered_before(snapshot, &freed);
        assert_eq!(dropped, 3, "the dead weak, soft and phantom rows");
        let weak: Vec<(usize, usize)> = proc
            .weak_refs
            .iter()
            .map(|e| (e.reference_obj, e.referent))
            .collect();
        assert_eq!(weak, vec![(0x2000, 0x5030), (0x1000, 0x6000)]);
        assert!(proc.soft_refs.is_empty());
        assert!(proc.phantom_refs.is_empty());
        assert_eq!(proc.finalizer_refs.len(), 1, "finalizer rows are never pruned here");
        // Idempotent.
        assert_eq!(proc.remove_reference_objects_registered_before(snapshot, &freed), 0);
        // The pre-pass it shares a body with is unchanged.
        assert_eq!(proc.remove_collected_reference_objects(&|a| a != 0x2000), 1);
        assert_eq!(proc.weak_refs.len(), 1);
    }

    /// gcd d9/e (`docs/internal/gc/gcd-d9e-deferred-enqueue-of-a-freed-reference-writes-into-freed-old-gen-FIXED-20260928.md`):
    /// a refused restore's DEFERRED enqueue pair goes with its row when the
    /// `Reference` dies before the next round -- through the pre-pass, the
    /// concurrent sweep's prune, `remove_collected` and the shape screen --
    /// so the next round never emits a dead (possibly re-issued) address. A
    /// live one is still delivered, and a later registration at a re-issued
    /// address keeps its own pair.
    #[test]
    fn gcd_d9e_a_deferred_enqueue_dies_with_its_reference() {
        let refused = |r: &mut ReferenceProcessor, obj: usize| {
            r.discover_reference(ReferenceType::Weak, obj, obj + 1, Some(0x90));
            assert!(r.clear_after_refused_restore(obj));
        };
        // The pre-pass (and, through the same body, the sweep prune).
        let mut proc = ReferenceProcessor::new();
        refused(&mut proc, 0x10);
        refused(&mut proc, 0x20);
        assert_eq!(proc.remove_collected_reference_objects(&|a| a != 0x10), 0);
        assert_eq!(proc.pending_enqueues_dropped(), 1);
        let result = proc.process_references(&always_live, 64, 0);
        assert_eq!(
            result.to_enqueue,
            vec![(0x20, 0x90)],
            "the live one is delivered"
        );

        // The concurrent sweep's prune, with a later registration on the
        // re-issued block that has its own deferred pair on ANOTHER queue.
        let mut proc = ReferenceProcessor::new();
        refused(&mut proc, 0x10);
        let snapshot = proc.registration_seq();
        proc.discover_reference(ReferenceType::Weak, 0x10, 0x77, Some(0xa0));
        assert!(
            proc.clear_after_refused_restore(0x10),
            "the new row (the old one is settled)"
        );
        let freed = |a: usize| a == 0x10;
        let _ = proc.remove_reference_objects_registered_before(snapshot, &freed);
        assert_eq!(
            proc.process_references(&always_live, 64, 0).to_enqueue,
            vec![(0x10, 0xa0)],
            "the freed Reference's pair is gone, the new one's is kept"
        );

        // `remove_collected` and the shape screen.
        let mut proc = ReferenceProcessor::new();
        refused(&mut proc, 0x10);
        proc.remove_collected(&|a| a != 0x10);
        let result = proc.process_references(&always_live, 64, 0);
        assert!(result.to_enqueue.is_empty());
        let mut proc = ReferenceProcessor::new();
        refused(&mut proc, 0x10);
        proc.retain_shaped_weak_phantom(&|a| a != 0x10);
        let result = proc.process_references(&always_live, 64, 0);
        assert!(result.to_enqueue.is_empty());
        assert_eq!(proc.pending_enqueues_dropped(), 1);
    }
}

/// DBG (CRATONVM_DBG_DM) -- see `gc_and_alloc::dm_dbg_enabled`. Cached gate.
fn dm_dbg_enabled() -> bool {
    use std::sync::OnceLock;
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| cratonvm_types::flags::runtime_var_os("CRATONVM_DBG_DM").is_some())
}

/// `jdk.internal.ref.Cleaner`s discovered as run-instead-of-enqueue phantoms.
static DM_PC_DISCOVERED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
/// Cleaner actions emitted by reference processing.
static DM_PC_EMITTED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
/// Reference-processing rounds, for the periodic tally.
static DM_REFPROC_ROUNDS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
