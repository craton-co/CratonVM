// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! Java reference processing for the garbage collector.
//!
//! Implements the four reference types defined by `java.lang.ref`:
//! [`SoftReference`], [`WeakReference`], [`PhantomReference`], and the
//! internal [`Cleaner`] / [`FinalReference`] types used by the JDK.
//!
//! The phase ORDER matches HotSpot's (soft, weak, final, phantom), and
//! reachability is recomputed between phases so each level only sees objects
//! the stronger levels have released. The finalizer-reachable closure folded
//! into phases 1-2 below is NOT HotSpot's rule: HotSpot clears soft and weak
//! references to an object reachable only through a finalizable one before
//! finalization. Every backend gets HotSpot's answer by reporting the
//! collection's resurrection closure (`note_resurrected_finalizables`, below;
//! Generational since gc-common w36-d, its pinned in-place young cycle since
//! gci i40/gn, G1 and ZGC since 2026-10-10 -- the VM's
//! `note_resurrected_finalizables_for_reference_processing` doc; this line said
//! "G1 and ZGC do not report it yet" until gci i40/gn) (gcg g2/r review;
//! `docs/feature-designs/profile-guided-inlining.md`):
//! 1. SoftReferences  -- cleared only under memory pressure (LRU policy),
//!    honouring the finalizer-reachable closure;
//! 2. WeakReferences  -- cleared when the referent is unreachable, honouring
//!    the finalizer-reachable AND soft-reachable closures;
//! 3. FinalReferences -- enqueued so the finalizer thread can run `finalize()`,
//!    honouring the soft-reachable closure (soft reachability blocks
//!    finalization; finalizer reachability deliberately does not, so mutually
//!    reachable finalizables are enqueued together). gci i11/rp (R6): on every
//!    production backend this phase finds NOTHING to enqueue -- the collector
//!    resurrects (marks) each dead finalizable object inside the collection
//!    (`collect_garbage_with_finalizers`, fed by the VM's `finalizable_roots`),
//!    so `is_marked` is true for it here, and the VM queues it and flags its
//!    row through the resurrection channel after this round
//!    (`enqueue_resurrected_finalizers` -> [`ReferenceProcessor::mark_finalizer_enqueued`]).
//!    The phase's enqueue arm serves callers that do not resurrect (the unit
//!    tests, a bare `process_references`); the round only COUNTS the
//!    resurrected rows (`ReferenceProcessingStats::finalizer_refs_resurrected`,
//!    the `-Xlog:gc+ref` census);
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
//! Almost everything here runs inside the collector's stop-the-world pause (the
//! exception is the Generational concurrent sweep's row prune,
//! `remove_reference_objects_registered_before` / `flush_deferred_sweep`, which
//! runs outside a pause), under the
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
//! `docs/feature-designs/generative-gc-architecture.md`
//! (retired into `docs/feature-designs/generative-gc-architecture.md`:
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
    /// FINALIZER rows only (JIT round 21 wave 16, lane finalize-reinit,
    /// `CRATONVM_FINALIZER_REREGISTER_AT_INIT_RETURN`; residual 4R of
    /// `docs/feature-designs/generative-gc-architecture.md`): a
    /// `<init>` of this row's object was seen returning normally since the row
    /// was registered ([`ReferenceProcessor::note_finalizable_init_returned`]),
    /// so its `Object.<init>` returned -- the point where HotSpot registers it.
    /// [`ReferenceProcessor::withdraw_finalizable`] then refuses: a later
    /// `<init>` on the same object that throws before its `super(..)` (an
    /// unverified class, a JNI re-`<init>`) does not undo that registration.
    pub constructed: bool,
    /// FINALIZER rows only (JIT round 21 wave 22, lane finalize,
    /// `CRATONVM_JNI_REINIT_REGISTERS_FINALIZER_AGAIN`; item 2 of
    /// `docs/feature-designs/generative-gc-architecture.md`): how
    /// many MORE `Finalizer`s HotSpot holds for this row's object. HotSpot
    /// registers at every `Object.<init>` return (`_return_register_finalizer`),
    /// so a JNI `<init>` call on an object that is already registered makes a
    /// second `Finalizer`, and `finalize()` runs once per `Finalizer`. This VM
    /// keeps ONE row per object (`finalizer_active`, the withdraw logic, the
    /// per-object finalizer queue all rely on it), so the extra registrations
    /// are a count on that row
    /// ([`ReferenceProcessor::note_finalizer_registered_again`]); each
    /// `finalize()` run of the object takes one
    /// ([`ReferenceProcessor::take_extra_finalizer`]) and queues the object
    /// again. Zero unless that switch is on and a JNI re-`<init>` happened.
    pub extra_finalizers: u32,
    /// FINALIZER rows only (JIT round 21 wave 25, lane initwitness,
    /// `CRATONVM_JNI_REINIT_INIT_RETURN_WITNESS`; item 2 of
    /// `docs/feature-designs/generative-gc-architecture.md`, the exact
    /// answer): how many times the `<init>`-return hook
    /// ([`ReferenceProcessor::note_finalizable_init_returned`]) saw a
    /// constructor of this row's object return normally while the row was
    /// active. A verified constructor returns only after its `super(..)` /
    /// `this(..)` call returned, so a count that moved during a call is a
    /// witness that `Object.<init>` returned on the object -- HotSpot's
    /// registration point. The JNI re-`<init>` door reads it before and after
    /// the call ([`ReferenceProcessor::finalizable_init_returns`]). Wraps.
    pub init_returns: u32,
    /// FINALIZER rows only (JIT round 22 wave 2, lane review): how many JNI
    /// re-`<init>` calls of this row's object armed a witness
    /// ([`ReferenceProcessor::note_jni_reinit_call`]). A door that finds it
    /// moved during its own call knows a NESTED re-`<init>` of the same object
    /// ran, whose `<init>` returns also moved [`Self::init_returns`], so the
    /// witness proves nothing about its own call. Wraps.
    pub jni_reinit_calls: u32,
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
/// channel are not in `finalizer_refs_enqueued` (`ReferenceProcessor::finalizers_enqueued_by_channel`);
/// gci i11/rp: the ones this round was told about are `finalizer_refs_resurrected`.
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
    /// gci i11/rp: the ACTIVE finalizer rows whose referent the collection
    /// noted as resurrected for `finalize()` ([`ReferenceProcessor::note_resurrected_finalizables`]),
    /// counted when the round began. On every production backend the
    /// collector resurrects a dead finalizable object itself and the VM flags
    /// its row through the channel (`mark_finalizer_enqueued`) AFTER this
    /// round, so `finalizer_refs_enqueued` stays `0` for it; this is the
    /// number HotSpot reports as `FinalReference Discovered` / `Processed`
    /// for the same collection (`-Xlog:gc+phases+ref=debug`).
    pub finalizer_refs_resurrected: usize,
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
// `docs/feature-designs/generative-gc-architecture.md`
// for the full argument, and
// `docs/feature-designs/generative-gc-architecture.md`
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

/// Bit 63 of a referent-class stamp ([`ReferenceProcessor::stamp_referent_class`]):
/// the stamp also records whether the referent was an ARRAY (bit 62).
///
/// A class id alone aliases an array with its component: a CratonVM array's
/// header carries the COMPONENT's class id, so an `Integer[]` referent and an
/// `Integer` that later reuses the address stamp the same id and the restore
/// screen cannot tell them apart (`r11w13-rt-array-alias-audit-jni-refs-gc`,
/// open item 1). A stamp WITHOUT this bit keeps its old meaning — a bare class
/// id, kind unknown — so the screen ([`referent_stamp_admits`]) and the writer
/// (the VM's pre-GC null pass) can land in either order.
///
/// gc JNI retirement (2026-10-10): the kind bits sit ABOVE the 32-bit class
/// id. They were bits 31 / 30 of a `u32` stamp, on the claim that class ids
/// are dense and far below 2^30 -- true of the class store's ids, false of a
/// lambda proxy's (`SharedVm::alloc_lambda_proxy_id`, a counter from
/// `0x8000_0000`). A proxy's id ORed with the kind bits read back as class
/// `id & !0xC000_0000`, so the restore screen refused EVERY lambda referent
/// after every collection and cleared a `WeakReference` whose lambda was
/// still strongly reachable (JNA's interface-mapped callbacks hold theirs that
/// way: the callback silently never ran).
pub const REFERENT_STAMP_KIND_KNOWN: u64 = 1 << 63;
/// Bit 62 of a referent-class stamp: the referent was an ARRAY. Meaningful
/// only with [`REFERENT_STAMP_KIND_KNOWN`].
pub const REFERENT_STAMP_ARRAY: u64 = 1 << 62;

/// gci i16/fr (G-9, stop-the-world half): the round's reading of the VM's
/// probe ([`ReferenceProcessor::note_stw_undiscovered_probe`]). Returns
/// `(undiscovered, probe_dead)`:
///
/// * `undiscovered` -- the `Reference` of every pair the collection marked
///   only through its finalizer resurrection (`resurrected`): HotSpot never
///   discovered it, so its row is left alone and its referent stays;
/// * `probe_dead` -- `candidates_only` minus the referents those pairs keep:
///   objects alive only because the VM asked the collection to keep them
///   until this decision, unmarked for the whole round.
///
/// Both empty when the probe is empty (every round without the switch).
fn resolve_stw_undiscovered_probe(
    pairs: &[(usize, usize)],
    candidates_only: &FxHashSet<usize>,
    resurrected: &FxHashSet<usize>,
) -> (Vec<usize>, FxHashSet<usize>) {
    if pairs.is_empty() && candidates_only.is_empty() {
        return (Vec::new(), FxHashSet::default());
    }
    let mut undiscovered = Vec::new();
    let mut kept: FxHashSet<usize> = FxHashSet::default();
    for &(reference_obj, referent) in pairs {
        if reference_obj != 0 && resurrected.contains(&reference_obj) {
            undiscovered.push(reference_obj);
            kept.insert(referent);
        }
    }
    let probe_dead = candidates_only
        .iter()
        .copied()
        .filter(|a| !kept.contains(a))
        .collect();
    (undiscovered, probe_dead)
}

/// May the post-GC restore install the object now at the referent's address,
/// whose class id is `have_class` and whose kind is `have_is_array`, against
/// the stamp `want` recorded before the collection?
///
/// A plain class-id stamp compares the class id alone, exactly as the screen
/// always did. A kind-aware stamp also requires the kind to agree, which is
/// what refuses an `Integer` found where an `Integer[]` was stamped (and the
/// reverse). The caller treats `want == 0` as unstamped before asking.
pub fn referent_stamp_admits(want: u64, have_class: u32, have_is_array: bool) -> bool {
    let have_class = u64::from(have_class);
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
/// gcj j1yg-pause round 3: the rows [`ReferenceProcessor::stash_settled_old_rows`]
/// set aside, and each list's row order (registration sequence numbers) at the
/// moment they were, so the restore puts every row back where it was.
#[derive(Default)]
struct SettledOldStash {
    weak: Vec<ReferenceEntry>,
    phantom: Vec<ReferenceEntry>,
    weak_order: Vec<u64>,
    phantom_order: Vec<u64>,
    /// The caller's count of old-collecting pauses when the rows were set
    /// aside; a different count at the restore means the pause moved or freed
    /// old storage after all (see [`ReferenceProcessor::restore_settled_old_rows_after`]).
    epoch: u64,
}

/// Relocate every row through `map`; answer whether any `reference_obj` (the
/// key of every index over these rows) actually changed.
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
                tracing::warn!("update_after_gc: null target for referent 0x{:x}", e.referent);
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

    /// gci i11/rp (R7) — restores the VM's post-GC referent pass refused
    /// because it could not prove the object at the recorded address is still
    /// the referent ([`Self::note_restore_refused`]). Non-zero means this VM
    /// would have installed a stranger in a `Reference`'s slot 0. Was the
    /// process-global `REFERENT_RESTORE_REFUSALS` in `gc_and_alloc.rs`, which
    /// summed every VM of the process.
    restore_refusals: u64,

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
    referent_class_stamps: FxHashMap<usize, u64>,

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
    /// gcj j1yg-pause round 3: the weak / phantom rows a certainly-minor
    /// Generational pause cannot change, set aside for that pause's two
    /// reference passes (see [`Self::stash_settled_old_rows`]). `None` outside
    /// that window.
    settled_old_stash: Option<SettledOldStash>,
    /// gcj j1yg-pause round 3: rows the last pre-collection pass set aside
    /// (`None`: it set none aside -- the pause was not certainly minor).
    /// Read by the `[vmpause]` line.
    settled_old_last: Option<usize>,

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

    /// gcj j4/rf (`gcj-j1rf-reference-processor-review-residuals` item 5,
    /// [`Self::soft_touch_keyed_on_clock`]): the clock [`Self::soft_stamp`]
    /// caps a touch at, as of the last locked touch (`0`: the key is the
    /// millisecond). Shared like [`Self::soft_touch_epoch`] so the `get()` fast
    /// path computes the same key ([`soft_touch_key_ms`]) without the lock. A
    /// stale value only fails a match: a new clock always bumps the epoch
    /// (`commit_hotspot_soft_clock`), and every stamp naming the current
    /// epoch was written under the lock with the current clock.
    soft_touch_clock: Arc<AtomicU64>,

    /// gcj j4/rf (item 5): the mutator clock a `get()` that skipped the lock
    /// observed (`fetch_max`), folded into [`Self::last_observed_clock_ms`] by
    /// every reader ([`Self::observed_clock_ms`]). Only the fast path of
    /// [`Self::soft_touch_keyed_on_clock`] writes it: keyed on the millisecond,
    /// a matching stamp proves the locked touch already raised the field to
    /// that millisecond, but keyed on the clock it does not.
    soft_touch_observed_ms: Arc<AtomicU64>,

    /// gcj j4/rf — `CRATONVM_SOFTREF_TOUCH_KEYED_ON_CLOCK`, latched when the
    /// processor is built (one per VM): under [`Self::soft_stamp_gc_clock`]
    /// and the HotSpot LRU a row's stamp is the GC clock, which changes only
    /// at a collection, so the fast path's stamp is keyed on that clock rather
    /// than on the wall-clock millisecond, and a hot soft cache read from many
    /// threads stops taking the processor mutex once per thread per
    /// millisecond. OFF: keyed on the millisecond (gc-common w6-d).
    soft_touch_keyed_on_clock: bool,

    /// PRE-collection addresses of the finalizable objects the collection
    /// being processed found unreachable and RESURRECTED (the collector's
    /// dead-finalizer list, mapped back through the pointer map by the VM).
    /// Consumed by the next [`Self::process_references_with_finalizer_trace`]
    /// and emptied by [`Self::finish_pre_gc_cycle`]: it describes ONE
    /// collection. See [`Self::note_resurrected_finalizables`].
    resurrected_this_cycle: FxHashSet<usize>,

    /// gci i14/gq (G-1) -- the NEXT processing round clears soft and weak
    /// references before finalization, HotSpot's rule, for a round whose
    /// caller has NOT resurrected the dead finalizable objects first (G1's
    /// final remark: `is_marked` is the completed bitmap and a dead old
    /// finalizable is unmarked when the round starts). Such a round's Phase 0
    /// finalizer-reachable closure (`finalizer_live`) is then left out of the
    /// soft and weak liveness predicates; it still keeps finalization and the
    /// phantom / `Cleaner` phases as before. Set by
    /// [`Self::note_weak_before_final_round`], consumed by the next
    /// [`Self::process_references_with_finalizer_trace`] and cleared by
    /// [`Self::finish_pre_gc_cycle`]: it describes ONE round. `false` (every
    /// round no caller noted) is the processor exactly as before.
    weak_before_final_this_round: bool,

    /// zt t11/gn3 (`CRATONVM_ZGC_GEN_YOUNG_HOLDS_SOFT_FREE`, default ON since
    /// zt t20/gn7, `=0` kill): the
    /// NEXT processing round ends a ZGC generational young cycle that kept
    /// every referent (`CRATONVM_ZGC_GEN_YOUNG_SKIP_REFERENCES`), and leaves
    /// [`Self::soft_free_mb_at_last_gc`] where the last whole-heap round put
    /// it: the next major's LRU allowance is then computed from the free heap
    /// after the last MAJOR, not after the last minor. The clock commit is
    /// unchanged (on the default configuration it is already a no-op on such
    /// a round, `zt10gn2_an_armed_young_round_leaves_the_soft_clock`). Which
    /// figure HotSpot 25 reads is item 1 of
    /// `zgc-zt11gn3-generational-what-is-left-SUPERSEDED-20261019.md`, decided by
    /// `Zt10Gn2SoftFree minor`. Set by
    /// [`Self::set_young_round_holds_soft_free`], consumed by the next
    /// [`Self::process_references_with_finalizer_trace`], cleared by
    /// [`Self::finish_pre_gc_cycle`]: one round. `false` everywhere else.
    young_round_holds_soft_free: bool,

    /// gci i15/rf (G-9) -- `Reference` OBJECT addresses the NEXT processing
    /// round must leave alone: soft / weak / phantom rows whose `Reference`
    /// the caller found unmarked yet reachable from an object the same round
    /// keeps alive (a dead finalizable's closure, on G1's final remark). Such
    /// a `Reference` was never reached by the marker, so HotSpot never
    /// DISCOVERED it: its rows are not processed, and the keep-alive phase
    /// traces its referent strongly (`process_discovered_references` disables
    /// discovery first). The caller keeps the referents. Set by
    /// [`Self::note_undiscovered_references`], emptied at the end of the round
    /// and by [`Self::finish_pre_gc_cycle`]: it describes ONE round. Empty
    /// (every round nobody noted) is the processor exactly as before.
    undiscovered_this_round: FxHashSet<usize>,

    /// gci i16/fr (G-9, stop-the-world half) -- `(Reference, referent)` PRE-
    /// collection pairs the VM found, before a stop-the-world collection,
    /// reachable from a registered finalizable object with their slot 0
    /// nulled by the pre-collection pass (`CRATONVM_STW_UNDISCOVERED_REFS_STRONG`).
    /// The next round turns every pair whose `Reference` the collection marked
    /// ONLY through its finalizer resurrection ([`Self::resurrected_this_cycle`])
    /// into an undiscovered row ([`Self::undiscovered_this_round`]): HotSpot
    /// never discovered that `Reference`, and its keep-alive phase traced the
    /// referent strongly. Set by [`Self::note_stw_undiscovered_probe`];
    /// consumed by the next [`Self::process_references_with_finalizer_trace`]
    /// and emptied by [`Self::finish_pre_gc_cycle`]: ONE round.
    stw_probe_pairs: Vec<(usize, usize)>,

    /// gci i16/fr -- PRE-collection addresses of probe referents the VM handed
    /// the collection as extra resurrection candidates (so it would not free
    /// them before the round could decide) that the collection kept ONLY
    /// through that candidacy: no dead finalizable object's closure reached
    /// them. The next round treats each one as UNMARKED in every phase unless
    /// a pair of [`Self::stw_probe_pairs`] keeps it (its `Reference` was
    /// undiscovered), so a discovered reference to it is cleared / enqueued
    /// exactly as if the probe had not run. One round, like the pairs.
    // addr-keyed: one pause's pre-collection addresses, consumed by its own round.
    stw_probe_candidates_only: FxHashSet<usize>,
    /// gci i27/gr (`gci-i15rf-...-20261014` residual,
    /// `CRATONVM_G1_FULL_GC_CARRIES_UNDISCOVERED_REFS`): the PRE-collection
    /// `Reference` objects the last round left alone because its
    /// stop-the-world probe found them undiscovered (the `undiscovered` half of
    /// [`resolve_stw_undiscovered_probe`]). Rewritten by every round (empty
    /// without a probe); read only by [`Self::carry_probe_undiscovered`].
    last_round_probe_undiscovered: Vec<usize>,
    /// gci i27/gr: [`Self::last_round_probe_undiscovered`] carried past its
    /// round, re-addressed through that collection's pointer map, with the
    /// heap collection count it is valid for ([`Self::carried_undiscovered_for`]).
    /// G1's final remark of the same full collection leaves these rows alone
    /// too. NOT emptied by [`Self::finish_pre_gc_cycle`] (the remark comes
    /// after that); replaced by the next carry and ignored once the count
    /// moved.
    // addr-keyed: one collection's post-move addresses, valid only while the heap's collection count is unchanged.
    carried_undiscovered: Vec<usize>,
    /// The collection count [`Self::carried_undiscovered`] is valid for.
    carried_undiscovered_stamp: u64,
    /// gci i48/rf48 (`gci-i15rf-...-20261014` residual, default path under
    /// `CRATONVM_G1_REMARK_UNDISCOVERED_REFS_STRONG`): the `Reference` objects
    /// a G1 full collection left undiscovered (its young pause's probe, its
    /// remark's walk), staged by the remark with the collection count, ARMED
    /// by the door just before its compaction pause and TAKEN by that pause's
    /// pre-collection pass ([`Self::take_armed_full_gc_undiscovered`]): the
    /// pass leaves their slot 0 alone (the collection traces the referent, as
    /// HotSpot's one full collection does) and the round leaves the rows alone.
    // addr-keyed: one remark's addresses, valid while the collection count is unchanged.
    full_gc_undiscovered: Vec<usize>,
    /// The collection count [`Self::full_gc_undiscovered`] is valid for.
    full_gc_undiscovered_stamp: u64,
    /// [`Self::full_gc_undiscovered`] may be taken (the door armed it).
    full_gc_undiscovered_armed: bool,
    /// gci i31/gq (G-2 of `gci-i14gq-g1-reference-processing-and-finalization-review-findings-FIXED-20261016`,
    /// default-ON `CRATONVM_G1_SYSTEM_GC_DEFERS_QUEUED_FINALIZABLES`): the
    /// finalizable objects a G1 full-collection door's young pause just
    /// queued (post-move addresses), carried to the stop-the-world mark of the
    /// same full collection, with the heap collection count they are valid
    /// for ([`Self::take_carried_door_queued_for`]). Replaced by every carry,
    /// taken (emptied) by the read, ignored once the count moved.
    // addr-keyed: one collection's post-move addresses, valid only while the heap's collection count is unchanged.
    carried_door_queued: Vec<usize>,
    /// The collection count [`Self::carried_door_queued`] is valid for.
    carried_door_queued_stamp: u64,
    /// gci i31/gq (G-2): the objects the G1 stop-the-world full mark kept
    /// alive only for the finalizables it held out of its roots, staged by
    /// the remark callback for the remark round
    /// ([`Self::take_remark_staged_resurrected`]), which reads them as its
    /// `resurrected` set. Lives inside one remark callback; also emptied by
    /// [`Self::finish_pre_gc_cycle`].
    // addr-keyed: one remark's addresses, staged and consumed inside one pause.
    remark_staged_resurrected: Vec<usize>,
    /// zr z5/rv5: referent addresses the next processing round treats as
    /// unmarked in the SOFT phase only (ZGC's last-ditch clear of a condemned
    /// soft referent that only a conservative compiled-frame word kept). See
    /// [`Self::note_soft_phase_forced_dead`]. Consumed by the next
    /// [`Self::process_references_with_finalizer_trace`] and emptied by
    /// [`Self::finish_pre_gc_cycle`]: it describes ONE collection.
    soft_phase_forced_dead: FxHashSet<usize>,

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
    /// [`Self::new_for_heap`] — ON by default on the Generational heap, on
    /// G1 since gci i5g/g1o (and jr20 wave 9 lane gcE), and on ZGC since zr
    /// z1/rf ([`Self::new_for_backend`]); OFF for a processor built without a
    /// heap in hand, an
    /// explicit setting winning everywhere). `true` evaluates the soft LRU policy
    /// with HotSpot's `LRUMaxHeapPolicy` inputs: the clock as of the END of the
    /// previous collection ([`Self::soft_gc_clock_ms`]) and the free heap AFTER
    /// it ([`Self::soft_free_mb_at_last_gc`]), instead of the wall clock now
    /// and the free space the caller measured before the collection. See
    /// `docs/feature-designs/generative-gc-architecture.md`.
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
    /// Several implementation pauses can belong to one G1 full collection.
    /// They all judge against this snapshot of the previous clock/free space.
    /// Master-clock publication still happens inside each collection pause;
    /// releasing the snapshot makes those new inputs visible to the next GC.
    soft_policy_cycle_inputs: Option<(u64, Option<usize>)>,
    /// See [`Self::finalizers_enqueued_by_channel`].
    finalizers_enqueued_by_channel: u64,
    /// gcj j4/rf (rp residuals item 2): the soft and weak rows
    /// [`Self::clear_refused_row`] cleared since the last
    /// [`Self::take_late_cleared`]. Those clears happen after the round's
    /// `stats` were copied out (and the next round zeroes `stats`), so the
    /// census reads them here.
    late_cleared: [u64; 2],
    /// gcd d1/c — the sequence number the next discovered row gets (see
    /// [`ReferenceEntry::registration_seq`]). Monotone.
    next_registration_seq: u64,

    /// gcf f2/r — `CRATONVM_REF_SETTLED_ROWS_UNTRACKED` (default OFF), latched
    /// when the processor is built (one per VM). ON: a SETTLED soft / weak /
    /// phantom row ([`Self::row_is_settled`]) contributes only its `Reference`
    /// object to [`Self::all_tracked_addrs`], not its dead referent and its
    /// already-served queue
    /// (`gcf-f1r-proposal-settled-reference-rows-leave-the-per-pause-walks`).
    settled_rows_untracked: bool,

    /// gcj j4/rf — `CRATONVM_REF_ADDRESS_REBUILDS_EXACT` (default ON, `=0`
    /// kills), latched when the processor is built
    /// (`gcj-j1rf-reference-processor-review-residuals` item 3). ON: the
    /// address-keyed rebuilds of [`Self::update_after_gc`] (identity stamps,
    /// referent-class stamps, `soft_pre_nulled`) let a MOVED key win the
    /// address it moved onto, and keep an unmoved key only while a row still
    /// names it (and, for `soft_pre_nulled`, only when no moved key landed on
    /// it); [`Self::retain_shaped_weak_phantom`] prunes the stamps of the rows
    /// it drops. OFF: an unmoved key is kept verbatim, so a dead `Reference`'s
    /// stale stamp can sit on the address a live one slid onto and refuse its
    /// writes (which of the two survives was `FxHashMap` iteration order).
    address_rebuilds_exact: bool,

    /// gci i11/rp — `CRATONVM_SOFTREF_STAMP_GC_CLOCK` (default ON since
    /// 2026-10-17, `=0` kills), latched
    /// when the processor is built (one per VM). Only meaningful under
    /// [`Self::hotspot_soft_lru`]. ON: a soft row's LRU stamp is HotSpot's
    /// `SoftReference.timestamp = clock` -- the END of the previous processing
    /// round ([`Self::soft_gc_clock_ms`]) -- instead of the wall-clock
    /// millisecond of the construction / `get()`; see [`Self::soft_stamp`].
    /// OFF: a reference read (or allocated) between two collections is up to
    /// one collection interval YOUNGER here than on HotSpot, so an idle soft
    /// reference HotSpot clears at the second collection after its last read
    /// can survive it
    /// (`docs/feature-designs/generative-gc-architecture.md`
    /// finding R2).
    soft_stamp_gc_clock: bool,
    /// gci i20/gn (`gci-i11rp-...` R10): the mutator clock when this
    /// processor was built -- HotSpot's first soft clock is the VM-start time
    /// (`ReferenceProcessor::init_statics`). [`Self::soft_stamp`] uses it while
    /// no processing round has ended (`soft_gc_clock_ms == 0`). Read only under
    /// [`Self::soft_stamp_gc_clock`].
    soft_clock_origin_ms: u64,

    /// gcg g2/r — the generational concurrent sweep's freed spans whose rows
    /// are not pruned yet (`CRATONVM_GEN_CONC_SWEEP_REFROW_BATCH`; `None` on
    /// the default per-slice path and on every other backend). Flushed by the
    /// sweep at a pause boundary, at the cap and at its end, and, so no pause
    /// can ever read a row the sweep freed, by every pause-time entry point
    /// that reads rows by address BEFORE anything relocates them
    /// ([`Self::flush_deferred_sweep`]'s callers). See
    /// [`SweptReferenceRowBatch`].
    deferred_sweep: Option<SweptReferenceRowBatch>,
    /// gcg g2/r — walks [`Self::flush_deferred_sweep`] made and the ACTIVE
    /// rows they dropped, over the processor's life (the sweep reports the
    /// difference across one sweep, pause-time flushes included).
    deferred_sweep_flushes: u64,
    deferred_sweep_rows_dropped: u64,

    /// gcg g3/rw (`gcg-g2r-proposal-reference-discovery-at-allocation-not-at-the-constructor-bridge`,
    /// the processor half of stage 0 of `common-d-proposal-marker-discovered-references`):
    /// soft / weak / phantom `Reference` objects registered at ALLOCATION
    /// ([`Self::note_allocated_reference`]) whose referent this processor has
    /// not bound yet. The pre-collection pass takes them
    /// ([`Self::take_allocated_unbound`]) and binds each from its slots
    /// ([`Self::bind_allocated_reference`]) -- or hands back the ones whose
    /// constructor has not run yet ([`Self::requeue_allocated_unbound`]).
    ///
    /// They are not rows: no processing phase sees them. They are kept exact
    /// the way rows are: watched ([`Self::all_tracked_addrs`]), relocated
    /// ([`Self::update_after_gc`]) and pruned when their object dies
    /// ([`Self::remove_collected`] and every [`Self::retain_reference_rows`]
    /// pruner, the concurrent sweep's included), so a recycled address never
    /// reaches a bind. Empty unless a VM allocation site calls
    /// `note_allocated_reference`, which none does yet (the sites are hunks on
    /// the proposal page).
    allocated_unbound: Vec<AllocatedReference>,

    /// Interpreter round i7 wave 143, lane L6 (`CRATONVM_REF_NOTES_PER_THREAD`,
    /// default ON): the striped allocation notes not absorbed into
    /// [`Self::allocated_unbound`] yet, and the registration sequence counter
    /// while attached. `None` (`=0`): every note is a locked push, as
    /// before. See [`AllocationNoteStripes`] for the readers that absorb.
    allocation_notes: Option<Arc<AllocationNoteStripes>>,

    /// Persistent ZGC skip bitmap for active Weak/Phantom `Reference` objects.
        persistent_skip_bits: Option<Arc<crate::skip_bits::ZSkipBits>>,

    /// JIT round 21 wave 10, lane finalize (`CRATONVM_FINALIZER_REGISTER_AT_OBJECT_INIT`):
    /// `(object address, VM address epoch)` of finalizable objects whose
    /// constructor provably threw before any `<init>` call on them completed
    /// -- so before `Object.<init>` returned, the point where HotSpot
    /// registers them -- and whose creator was not an interpreted frame (a
    /// native door: a constructor `MethodHandle`, JNI `NewObject`, a
    /// `NativeContext` construction). The door that allocated the object
    /// takes its entry ([`Self::take_unconstructed_finalizable`]) and
    /// withdraws the registration it made at allocation. An address names one
    /// object only within one epoch, so a stale entry (no door took it) can
    /// never match another object; entries of an older epoch are dropped on
    /// the next note, and the list is capped ([`UNCONSTRUCTED_FINALIZABLES_MAX`]):
    /// a dropped entry only leaves its object registered, as before the switch.
    /// Not rows and not roots: the objects are garbage.
    unconstructed_finalizables: Vec<(usize, u64)>,

    /// JIT round 21 wave 16, lane finalize-reinit
    /// (`CRATONVM_FINALIZER_REREGISTER_AT_INIT_RETURN`, default ON, `0`
    /// kills; residual 4R of `docs/feature-designs/generative-gc-architecture.md`):
    /// the finalizer rows [`Self::withdraw_finalizable`] took, kept so a later
    /// successful `<init>` return on the same object re-registers it
    /// ([`Self::note_finalizable_init_returned`]). HotSpot registers at every
    /// `Object.<init>` return; a creator that catches the constructor's throw
    /// and runs `<init>` again on the object (bytecode no compiler emits; a
    /// COMPILED creator's retry is invisible to the unwinder's withdrawal)
    /// reaches that point on the retry.
    ///
    /// Not rows, not roots and (since wave 18) not watched: no processing
    /// phase reads them, and an object nobody retries is garbage. Watching
    /// them (wave 16) put up to 32 unconstructed objects into the set that
    /// seeds the true-root major's young marking, which kept them and
    /// whatever their constructors stored before `super(..)` alive past the
    /// collection HotSpot frees them in (a `WeakReference` to such a field's
    /// object cleared one collection late). So the entries live for one
    /// collection interval at most: [`Self::update_after_gc`] and
    /// [`Self::remove_collected`] drop them all, and the concurrent sweep's
    /// pruners drop the ones whose block it freed
    /// ([`Self::retain_reference_rows`]), so a recycled address never matches.
    /// A retry is immediate (the creator's handler, same thread), and the
    /// retry shape is reachable only in UNVERIFIED code (verified code cannot
    /// keep an `uninitialized` reference across the handler of its own
    /// `<init>` call: HotSpot's and this VM's verifier check the handler frame
    /// after the call too), so a collection in between only leaves that object
    /// unregistered, the answer before the switch. The same holds for the cap
    /// ([`WITHDRAWN_FINALIZABLES_MAX`]).
    withdrawn_finalizables: Vec<ReferenceEntry>,
    /// See [`Self::withdrawn_finalizables`]: `CRATONVM_FINALIZER_REREGISTER_AT_INIT_RETURN`,
    /// latched when the processor is built (one per VM).
    reregister_at_init_return: bool,
    /// The sum of [`ReferenceEntry::extra_finalizers`] over `finalizer_refs`
    /// (JIT round 21 wave 22, lane finalize; a count since wave 23, item 4 of
    /// `docs/feature-designs/generative-gc-architecture.md`: it was a
    /// sticky bool, so after ONE JNI re-`<init>` every later `finalize()`
    /// scanned every finalizer row under this processor's lock). While it is
    /// `0` -- always in a VM with no re-`<init>`, and again once the
    /// re-initialized objects are finalized or collected --
    /// [`Self::take_extra_finalizer`] answers without scanning. Raised by
    /// [`Self::note_finalizer_registered_again`], lowered by a take and by
    /// every removal of a row that still carries a count
    /// ([`Self::withdraw_finalizable`], [`Self::remove_collected`]).
    extra_finalizers_outstanding: u64,

    stats: ReferenceProcessingStats,
}

/// Cap of [`ReferenceProcessor::unconstructed_finalizables`] (JIT round 21
/// wave 10, lane finalize). The oldest entry goes first.
pub const UNCONSTRUCTED_FINALIZABLES_MAX: usize = 32;

/// Cap of [`ReferenceProcessor::withdrawn_finalizables`] (JIT round 21 wave
/// 16, lane finalize-reinit). The oldest entry goes first.
pub const WITHDRAWN_FINALIZABLES_MAX: usize = 32;

/// gcg g3/rw — one `Reference` registered at allocation and not bound yet
/// (see `ReferenceProcessor::allocated_unbound`). `row` is the row it will
/// become: `referent == 0`, no queue, and its registration sequence number
/// taken at allocation, so binding it later does not move it past the
/// concurrent sweep's sequence bound (a sweep that began after the
/// allocation still prunes it when it frees the object).
#[derive(Debug, Clone)]
pub struct AllocatedReference {
    /// The future row (`referent == 0`, `queue_addr == None`).
    pub row: ReferenceEntry,
    /// The allocated object's class id, for the binder's shape check (an
    /// address is re-checked against it before its slots are read).
    pub class_id: u32,
}

/// gcg g3/rw — what [`ReferenceProcessor::bind_allocated_reference`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BindOutcome {
    /// A row was registered with the bound referent and queue.
    Bound,
    /// The constructor ran with a null referent: HotSpot never discovers
    /// such a `Reference`, and neither does this processor. Dropped.
    NullReferent,
    /// An ACTIVE row already names this object (the constructor bridge
    /// registered it): nothing registered twice. Dropped.
    AlreadyRegistered,
}

/// How many stripes [`AllocationNoteStripes`] has.
const ALLOCATION_NOTE_STRIPES: usize = 16;

/// One allocation note waiting in an [`AllocationNoteStripes`] stripe: what
/// [`ReferenceProcessor::note_allocated_reference`] reads, with the
/// registration sequence number and the soft allocation clock taken AT the
/// note, so a late absorption changes neither.
#[derive(Debug, Clone, Copy)]
struct StripedAllocationNote {
    ref_type: ReferenceType,
    reference_obj: usize,
    pub class_id: u32,
    runs_cleaner: bool,
    seq: u64,
    clock_ms: u64,
}

/// How many of the newest notes of its own stripe
/// [`AllocationNoteStripes::cancel_recent`] looks at (t1 wave 6, lane G6).
const CANCEL_WINDOW: usize = 64;

/// One stripe of [`AllocationNoteStripes`] (t1 wave 6, lane G6): its notes
/// and their count, on a cache line of its own so neighbouring stripes'
/// lock words do not false-share.
///
/// `len` mirrors `notes.len()` and is written ONLY while `notes` is locked,
/// with a plain release store: the round-i7 A/B found the striped `t=1` rows
/// 9-14% slower than the locked push, and the shared `pending` counter's
/// `lock xadd` per note (plus one more per drain) was one of the atomic
/// read-modify-writes a note paid over the lock it replaced. A reader's
/// "anything to absorb?" check is now a sum of 16 acquire loads: a note
/// completed before the reader started (any happens-before edge, e.g. the
/// allocating thread reaching the safepoint) has its store visible; a note
/// racing the reader may or may not be seen, exactly as a locked push racing
/// the reader's lock acquisition could land on either side.
#[derive(Debug, Default)]
#[repr(align(64))]
struct NoteStripe {
    notes: Mutex<Vec<StripedAllocationNote>>,
    len: AtomicUsize,
}

/// Interpreter round i7 wave 143, lane L6 (proposal
/// `i77-L5-proposal-reference-registry-notes-without-the-global-mutex`,
/// `CRATONVM_REF_NOTES_PER_THREAD`, performance, default ON; read once per VM
/// by the VM, which attaches one of these to its processor with
/// [`ReferenceProcessor::attach_allocation_note_stripes`]).
///
/// The allocation notes of soft / weak / phantom `Reference`s made WITHOUT
/// the processor's mutex: a note takes one of [`ALLOCATION_NOTE_STRIPES`]
/// small mutexes, chosen by the calling OS thread's stack address (threads'
/// stacks are megabytes apart, so concurrent allocators land on different
/// stripes; a collision only shares a lock). No process state: one per VM.
///
/// # The invariant
///
/// A note in a stripe is NOT in `allocated_unbound`, so every processor method
/// that reads that list (or the registration sequence) first absorbs the
/// stripes (`&mut self`) or reads them in place (`&self`), under the
/// processor's lock: [`ReferenceProcessor::take_allocated_unbound`],
/// [`ReferenceProcessor::allocated_unbound_len`],
/// [`ReferenceProcessor::update_after_gc`] (a buffered address must be
/// relocated), [`ReferenceProcessor::remove_collected`] and every
/// `retain_reference_rows` pruner (a dead buffered note must be pruned),
/// [`ReferenceProcessor::all_tracked_addrs`] (a buffered note is watched),
/// [`ReferenceProcessor::has_active_soft_refs`],
/// [`ReferenceProcessor::active_soft_referents`] and
/// [`ReferenceProcessor::registration_seq`]. Each such call therefore sees
/// every note made before it, exactly as it saw every locked push before:
/// the stripes change who waits, not what any reader sees. The registration
/// sequence counter moves into this object (`next_seq`), so a note's number
/// is taken at the allocation, as the locked push took it, and the
/// concurrent sweep's sequence bound keeps pruning a buffered note whose
/// object it frees.
///
/// Lock order: processor (L7), then a stripe; a note takes a stripe alone,
/// and so does a cancel ([`Self::cancel_recent`]).
///
/// # The application retire (t1 wave 6, lane G6)
///
/// `Reference.clear()` / `enqueue()` (and so `Cleaner.clean()`, whose
/// `PhantomCleanable.clean()` calls `clear()`) reach the VM's
/// `mark_reference_manually_enqueued` AFTER the `clear0` native nulled the
/// referent slot. With these stripes attached the VM first offers the retire
/// to [`Self::cancel_recent`]: when the reference's own allocation note still
/// waits in the calling thread's stripe (the common shape: a `ZipFile`
/// stream's `PhantomCleanableRef`, a `WeakHashMap` entry removed, a
/// `ThreadLocal` entry cleared, all before the next collection), the note is
/// removed and the processor's mutex is not taken at all. Otherwise (`false`)
/// the VM takes the locked `retire_by_application`, as before.
///
/// Why the cancel is exactly the locked verdict: a striped note is not a row,
/// so the locked retire finds nothing for it (`app_row_lookup` reads only the
/// bound lists) and the row it becomes is later DROPPED by the pre-collection
/// bind because its referent slot reads null (HotSpot's rule: a reference
/// with a null referent is never discovered, so never enqueued, never
/// polled from its queue, and a Cleaner it belongs to never runs). Cancelling
/// only reaches that verdict before any reader looks. Every reader takes the
/// processor's lock and then the stripe's lock, and the cancel takes the same
/// stripe lock, so each note is either drained (then the cancel cannot find
/// it, returns `false`, and the bind drops the row) or cancelled (then no
/// drain sees it); never both, never neither. The `Reference` constructor has
/// stored its referent before any method of `this` can run (no `clear()` in a
/// constructor prologue), so a cancelled note is never one the bind would
/// have requeued for a constructor still to come. A note at the same address
/// left by a reference the concurrent sweep freed is dead, and pruning it is
/// what the sweep's sequence bound does anyway. Enqueue order, `poll` answers
/// and Cleaner run order involve only bound rows the collector settles, which
/// a cancel never touches.
///
/// The argument needs every soft / weak / phantom row to come from the bind.
/// A `java/lang/ref` constructor bridge (`ReferenceProcessor::discover_reference`
/// from the VM, with discovery at allocation opted into) registers a row at
/// `<init>` while the `new`'s note still waits, so that reference owns both;
/// once any such row exists the cancel still removes the note but answers
/// `false` (t1 wave 8, lane I77), and the locked retire settles the row.
///
/// The cancel looks only at the last [`CANCEL_WINDOW`] notes of the caller's
/// own stripe (a reference allocated long ago, or by another thread, takes
/// the locked path), so it is O(1) however many notes wait.
#[derive(Debug)]
pub struct AllocationNoteStripes {
    stripes: Box<[NoteStripe]>,
    /// The registration sequence counter while attached (see the invariant).
    next_seq: AtomicU64,
    /// Notes removed by [`Self::cancel_recent`] (census).
    cancelled: AtomicU64,
    /// Notes ever moved out by an absorption (census). Counted at the drain,
    /// not at the note (t1 2026-10-25, lane interpB): the round-i7 A/B found
    /// the striped `t=1` rows 9-14% slower than the locked push, and a
    /// census `lock xadd` per note was one of three atomic read-modify-writes
    /// the note paid over the lock it replaced. Notes taken = this plus
    /// [`Self::pending`] plus `cancelled`.
    absorbed: AtomicU64,
    /// Absorptions that moved at least one note (census).
    absorptions: AtomicU64,
    /// t1 wave 8, lane I77: has the attached processor registered a soft /
    /// weak / phantom row OUTSIDE the pre-collection bind (a `java/lang/ref`
    /// constructor bridge through `NativeContext::discover_reference`)? Such
    /// a reference can hold a bound row AND a striped allocation note at the
    /// same time (the note at `new`, the row at `<init>`), so removing its
    /// note does not settle it: the locked `retire_by_application` must still
    /// retire the row. Once set, [`Self::cancel_recent`] still removes the
    /// note but answers `false`, sending the caller to the locked path. Only
    /// a VM whose constructors are bridges AND that opted into discovery at
    /// allocation sets it; `--real-jdk` and `--compatible` with the
    /// `JdkReferences` row register through the bind alone.
    rows_outside_bind: AtomicBool,
}
impl Default for AllocationNoteStripes {
    fn default() -> Self {
        Self::new()
    }
}

impl AllocationNoteStripes {
    /// Empty stripes.
    pub fn new() -> Self {
        Self {
            stripes: (0..ALLOCATION_NOTE_STRIPES)
                .map(|_| NoteStripe::default())
                .collect(),
            next_seq: AtomicU64::new(0),
            cancelled: AtomicU64::new(0),
            absorbed: AtomicU64::new(0),
            absorptions: AtomicU64::new(0),
            rows_outside_bind: AtomicBool::new(false),
        }
    }

    /// t1 wave 8, lane I77: the attached processor just registered a soft /
    /// weak / phantom row outside the pre-collection bind (see
    /// `rows_outside_bind`). Called under the processor's lock; the release
    /// store pairs with [`Self::cancel_recent`]'s acquire load, so a clear
    /// that happens after the bridge registration of its own reference (the
    /// constructor ran before any method of the object) sees it.
    fn note_row_outside_bind(&self) {
        if !self.rows_outside_bind.load(Ordering::Relaxed) {
            self.rows_outside_bind.store(true, Ordering::Release);
        }
    }

    /// The stripe of the calling OS thread.
    #[inline]
    fn stripe_index() -> usize {
        let marker = 0u8;
        let addr = std::ptr::addr_of!(marker) as usize;
        ((addr >> 21) ^ (addr >> 27)) % ALLOCATION_NOTE_STRIPES
    }

    /// [`ReferenceProcessor::note_allocated_reference`] without the
    /// processor's lock: the same filter, the sequence number and the soft
    /// allocation clock taken now.
    pub fn note(
        &self,
        ref_type: ReferenceType,
        reference_obj: usize,
        class_id: u32,
        runs_cleaner: bool,
    ) {
        self.note_in(
            Self::stripe_index(),
            ref_type,
            reference_obj,
            class_id,
            runs_cleaner,
        );
    }

    /// [`Self::note`] into stripe `idx` (the unit tests pick it).
    fn note_in(
        &self,
        idx: usize,
        ref_type: ReferenceType,
        reference_obj: usize,
        class_id: u32,
        runs_cleaner: bool,
    ) {
        if reference_obj == 0
            || !matches!(
                ref_type,
                ReferenceType::Soft | ReferenceType::Weak | ReferenceType::Phantom
            )
        {
            return;
        }
        let note = StripedAllocationNote {
            ref_type,
            reference_obj,
            class_id,
            runs_cleaner,
            seq: self.next_seq.fetch_add(1, Ordering::Relaxed),
            clock_ms: if ref_type == ReferenceType::Soft {
                allocation_clock_ms()
            } else {
                0
            },
        };
        let stripe = &self.stripes[idx % ALLOCATION_NOTE_STRIPES];
        let mut notes = stripe.notes.lock();
        notes.push(note);
        stripe.len.store(notes.len(), Ordering::Release);
    }

    /// t1 wave 6, lane G6: the application retire of `reference_obj`
    /// (`clear()` / `enqueue()`, its referent slot already nulled) WITHOUT the
    /// processor's lock, when its allocation note still waits among the last
    /// [`CANCEL_WINDOW`] notes of the calling thread's stripe: the note is
    /// removed and `true` returned. `false`: not found (drained already, made
    /// on another thread or stripe, or too old); the caller takes the locked
    /// `retire_by_application`. See "The application retire" on the type for
    /// why removing the note is the locked path's verdict.
    ///
    /// t1 wave 8, lane I77: once the processor has registered a row outside
    /// the bind (`rows_outside_bind`, constructor bridges), a found note is
    /// still removed but the answer is `false`: the reference may also own a
    /// bound row, which only the locked retire settles.
    pub fn cancel_recent(&self, reference_obj: usize) -> bool {
        self.cancel_recent_in(Self::stripe_index(), reference_obj)
    }

    /// [`Self::cancel_recent`] in stripe `idx` (the unit tests pick it).
    fn cancel_recent_in(&self, idx: usize, reference_obj: usize) -> bool {
        if reference_obj == 0 {
            return false;
        }
        let stripe = &self.stripes[idx % ALLOCATION_NOTE_STRIPES];
        // The caller's own notes are visible to it in program order; a miss
        // here only sends the caller to the locked path.
        if stripe.len.load(Ordering::Relaxed) == 0 {
            return false;
        }
        let mut notes = stripe.notes.lock();
        let n = notes.len();
        let Some(pos) = (n.saturating_sub(CANCEL_WINDOW)..n)
            .rev()
            .find(|&i| notes[i].reference_obj == reference_obj)
        else {
            return false;
        };
        // Order inside a stripe does not matter: the drain sorts by `seq`.
        notes.swap_remove(pos);
        stripe.len.store(notes.len(), Ordering::Release);
        drop(notes);
        self.cancelled.fetch_add(1, Ordering::Relaxed);
        // t1 wave 8, lane I77: the note is gone either way, but where a
        // constructor bridge may have registered a row for this same
        // reference, only the locked retire settles that row; skipping it
        // brought back the PGJDBC phantom ghost and the G1-remark clear of an
        // application-cleared reference (W2-F-2).
        !self.rows_outside_bind.load(Ordering::Acquire)
    }

    /// Notes waiting in the stripes.
    pub fn pending(&self) -> usize {
        self.stripes
            .iter()
            .map(|s| s.len.load(Ordering::Acquire))
            .sum()
    }
    /// `(notes taken, absorptions that moved one)` over the stripes' life.
    /// Notes taken is the absorbed count plus what still waits plus what was
    /// cancelled; a note being made concurrently may or may not be counted
    /// yet.
    pub fn census(&self) -> (u64, u64) {
        (
            self.absorbed.load(Ordering::Relaxed)
                + self.pending() as u64
                + self.cancelled.load(Ordering::Relaxed),
            self.absorptions.load(Ordering::Relaxed),
        )
    }

    /// Notes removed by [`Self::cancel_recent`] over the stripes' life.
    pub fn cancelled(&self) -> u64 {
        self.cancelled.load(Ordering::Relaxed)
    }

    /// Move every waiting note out, stripe by stripe, without sorting.
    fn drain_unsorted(&self) -> Vec<StripedAllocationNote> {
        let total: usize = self.stripes.iter().map(|s| s.len.load(Ordering::Acquire)).sum();
        let mut out = Vec::with_capacity(total);
        for stripe in self.stripes.iter() {
            if stripe.len.load(Ordering::Acquire) == 0 {
                continue;
            }
            let mut s = stripe.notes.lock();
            if s.is_empty() {
                continue;
            }
            out.append(&mut *s);
            stripe.len.store(0, Ordering::Release);
        }
        if !out.is_empty() {
            self.absorptions.fetch_add(1, Ordering::Relaxed);
            self.absorbed.fetch_add(out.len() as u64, Ordering::Relaxed);
        }
        out
    }

    /// Move every waiting note out, stripe by stripe.
    fn drain(&self) -> Vec<StripedAllocationNote> {
        let mut out = self.drain_unsorted();
        // Registration order, as the locked pushes made it.
        out.sort_unstable_by_key(|n| n.seq);
        out
    }

    /// Visit every waiting note in place (the `&self` readers).
    fn for_each(&self, mut f: impl FnMut(&StripedAllocationNote)) {
        for stripe in self.stripes.iter() {
            if stripe.len.load(Ordering::Acquire) == 0 {
                continue;
            }
            for n in stripe.notes.lock().iter() {
                f(n);
            }
        }
    }
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

/// gcj j4/rf (`gcj-j1rf-reference-processor-review-residuals` item 5): the
/// millisecond a [`soft_touch_stamp`] is keyed on for a touch at `now_ms`,
/// given the processor's published touch clock
/// ([`ReferenceProcessor::soft_touch_clock`]): the clock when a touch at
/// `now_ms` would stamp the row with it (`now_ms >= clock`, so
/// `soft_stamp(now_ms) == clock`), `now_ms` otherwise (a `0` clock: the key
/// is the millisecond, as before). Both sides of the fast path compute it
/// the same way, so every touch between two collections shares one stamp.
pub fn soft_touch_key_ms(now_ms: u64, clock: u64) -> u64 {
    if clock != 0 && now_ms >= clock {
        clock
    } else {
        now_ms
    }
}

/// gcg g5/rs: the wall clock (Unix-epoch milliseconds) the VM's
/// `SoftReference` touch reads, for the allocation stamp of
/// [`ReferenceProcessor::note_allocated_reference`]. `0` if the clock is
/// before the epoch.
fn allocation_clock_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// gcj j4/rf: rebuild an address-keyed stamp table across a relocation (see
/// [`ReferenceProcessor::update_after_gc`]). `exact` OFF is the old rebuild:
/// an unmapped key is kept verbatim and insertion order decides a collision.
/// ON: the moved keys are inserted after the unmoved ones, so a live
/// `Reference` that slid onto a dead one's address keeps its own stamp, and an
/// unmoved key is kept only while `row_keys` (every row's relocated
/// `reference_obj`) names it.
fn rebuild_address_table<V: Copy>(
    table: &FxHashMap<usize, V>,
    pointer_map: &cratonvm_types::PointerMap,
    exact: bool,
    row_keys: &FxHashSet<usize>,
) -> FxHashMap<usize, V> {
    let mut out: FxHashMap<usize, V> =
        FxHashMap::with_capacity_and_hasher(table.len(), Default::default());
    if !exact {
        for (addr, v) in table.iter() {
            let now = match pointer_map.get(addr) {
                Some(&new) if new != 0 => new,
                _ => *addr,
            };
            out.insert(now, *v);
        }
        return out;
    }
    let mut moved: Vec<(usize, V)> = Vec::new();
    for (addr, v) in table.iter() {
        match pointer_map.get(addr) {
            Some(&new) if new != 0 => moved.push((new, *v)),
            _ if row_keys.contains(addr) => {
                out.insert(*addr, *v);
            }
            _ => {}
        }
    }
    out.extend(moved);
    out
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
    /// `generational` selects the Generational arm of
    /// `CRATONVM_SOFTREF_HOTSPOT_LRU`'s per-backend default
    /// (`alloc_policy_defaults::SOFTREF_HOTSPOT_LRU`): ON for that arm, where
    /// HotSpot's inputs were measured to match HotSpot Serial
    /// (`GenR5W2SoftRefIdleYoungProbe` `lost=0 PASS`, against `lost=2 FAIL` on
    /// the wall-clock inputs); OFF for the other arm. Despite its name the VM
    /// passes `vm_init::soft_lru_takes_hotspot_inputs_by_default(backend)`,
    /// which is `true` for Generational AND G1 since gci i5g/g1o (vi5g-g1o S:
    /// G1 takes HotSpot's inputs by default, `=0` kills it; jr20 wave 9 lane
    /// gcE made the same decision: HotSpot's G1 and ZGC run `LRUMaxHeapPolicy`
    /// too), and ZGC takes them through [`Self::new_for_backend`] (zr z1/rf),
    /// so only a processor built without a heap gets the wall-clock inputs by
    /// default (gci i14/gq doc fix). An explicit setting wins on every backend.
    /// Per VM: the choice is latched in this processor, no process state.
    pub fn new_for_heap(soft_ref_lru_ms_per_mb: Option<u64>, generational: bool) -> Self {
        Self::new_for_backend(soft_ref_lru_ms_per_mb, generational, false)
    }

    /// zr z1/rf: [`Self::new_for_heap`] with the ZGC heap told apart from G1
    /// (`zgc` = the VM built a ZGC heap). An unset `CRATONVM_SOFTREF_HOTSPOT_LRU`
    /// selects HotSpot's inputs on ZGC
    /// (`alloc_policy_defaults::SOFTREF_HOTSPOT_LRU_ZGC`); `zgc == false` is
    /// exactly `new_for_heap`.
    pub fn new_for_backend(
        soft_ref_lru_ms_per_mb: Option<u64>,
        generational: bool,
        zgc: bool,
    ) -> Self {
        let mut proc = match soft_ref_lru_ms_per_mb {
            Some(ms) => Self::new_with_policy(ms),
            None => Self::new(),
        };
        proc.hotspot_soft_lru = gc_flags().softref_hotspot_lru_for_backend(generational, zgc);
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
            settled_old_stash: None,
            settled_old_last: None,
            clear_all_soft_requested: false,
            soft_touch_epoch: Arc::new(AtomicU64::new(0)),
            soft_touch_clock: Arc::new(AtomicU64::new(0)),
            soft_touch_observed_ms: Arc::new(AtomicU64::new(0)),
            soft_touch_keyed_on_clock: true,
            resurrected_this_cycle: FxHashSet::default(),
            weak_before_final_this_round: false,
            young_round_holds_soft_free: false,
            undiscovered_this_round: FxHashSet::default(),
            stw_probe_pairs: Vec::new(),
            stw_probe_candidates_only: FxHashSet::default(),
            last_round_probe_undiscovered: Vec::new(),
            carried_undiscovered: Vec::new(),
            carried_undiscovered_stamp: 0,
            full_gc_undiscovered: Vec::new(),
            full_gc_undiscovered_stamp: 0,
            full_gc_undiscovered_armed: false,
            carried_door_queued: Vec::new(),
            carried_door_queued_stamp: 0,
            remark_staged_resurrected: Vec::new(),
            soft_phase_forced_dead: FxHashSet::default(),
            finalizer_active: FxHashSet::default(),
            finalizer_active_stale: true,
            hotspot_soft_lru: gc_flags().softref_hotspot_lru_for(false),
            soft_gc_clock_ms: 0,
            soft_clock_pending_ms: 0,
            soft_free_mb_at_last_gc: None,
            soft_policy_cycle_inputs: None,
            finalizers_enqueued_by_channel: 0,
            late_cleared: [0; 2],
            next_registration_seq: 0,
            pending_enqueues_dropped: 0,
            restore_refusals: 0,
            settled_rows_untracked: true,
            address_rebuilds_exact: true,
            soft_stamp_gc_clock: true,
            soft_clock_origin_ms: allocation_clock_ms(),
            deferred_sweep: None,
            deferred_sweep_flushes: 0,
            deferred_sweep_rows_dropped: 0,
            allocated_unbound: Vec::new(),
            allocation_notes: None,
                        persistent_skip_bits: None,
            unconstructed_finalizables: Vec::new(),
            withdrawn_finalizables: Vec::new(),
            reregister_at_init_return: true,
            extra_finalizers_outstanding: 0,
            stats: ReferenceProcessingStats::default(),
        }
    }

    /// gen r5w1/refs5 — the soft LRU policy's `(clock, max_interval_ms)` under
    /// [`Self::hotspot_soft_lru`]: HotSpot's `LRUMaxHeapPolicy::setup` with
    /// `SoftRefLRUPolicyMSPerMB` = this processor's `soft_ref_lru_policy_ms_per_mb`.
    /// A reference is condemned iff `clock - last_access > max_interval`.
    fn hotspot_soft_policy(&self, fallback_free_mb: usize) -> (u64, u64) {
        let (clock, free_mb) = self.hotspot_soft_inputs(fallback_free_mb);
        (
            clock,
            self.soft_ref_lru_policy_ms_per_mb
                .saturating_mul(free_mb as u64),
        )
    }

    fn hotspot_soft_inputs(&self, fallback_free_mb: usize) -> (u64, usize) {
        let (clock, free_mb) = self.soft_policy_cycle_inputs
            .unwrap_or((self.soft_gc_clock_ms, self.soft_free_mb_at_last_gc));
        (clock, free_mb.unwrap_or(fallback_free_mb))
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
        let before = self.soft_gc_clock_ms;
        self.soft_gc_clock_ms = self.soft_gc_clock_ms.max(pending).max(now_ms);
        self.soft_free_mb_at_last_gc = Some(free_heap_mb);
        // gci i11/rp: under `CRATONVM_SOFTREF_STAMP_GC_CLOCK` a touch's stamp
        // is this clock, so a new clock changes what a touch would write: no
        // `SoftReference.timestamp` fast-path stamp minted before it may
        // short-cut a touch after it (`soft_touch_stamp`).
        if self.soft_stamp_gc_clock && self.soft_gc_clock_ms != before {
            self.bump_soft_touch_epoch();
        }
    }

    /// Hold the previous collection's policy inputs across one logical full
    /// collection, including its evacuation, mark/remark and compaction pauses.
    /// Only one full door owns the scope. Other doors wait with safepoint
    /// checks, then coalesce or start their own operation after it completes.
    pub fn try_begin_soft_policy_cycle(&mut self) -> bool {
        if self.soft_policy_cycle_inputs.is_some() {
            return false;
        }
        self.soft_policy_cycle_inputs = Some((self.soft_gc_clock_ms, self.soft_free_mb_at_last_gc));
        true
    }

    /// Release the policy snapshot. Master-clock updates stay in the actual
    /// reference rounds and are already published before their pauses resume.
    /// Last-ditch condemnation remains independent of the snapshot.
    pub fn finish_soft_policy_cycle(&mut self) -> u64 {
        assert!(self.soft_policy_cycle_inputs.take().is_some());
        self.soft_gc_clock_ms
    }

    /// gci i11/rp — the LRU stamp a soft row takes for an access (or a
    /// construction) at `ms` on the processor's mutator clock.
    ///
    /// Under [`Self::soft_stamp_gc_clock`] and [`Self::hotspot_soft_lru`],
    /// once a processing round has ended, HotSpot's rule: `SoftReference.<init>`
    /// and `get()` store `timestamp = clock`, the clock as of the end of the
    /// previous collection, so every access between two collections carries
    /// the SAME stamp. `ms` is capped at that clock (a wall-clock access is
    /// always later than it; a value already on the clock's scale -- the Java
    /// `timestamp` field the allocation-discovery refresh hands in -- is kept),
    /// and `0` ("no access known") becomes the clock, as the constructor's
    /// copy does. Otherwise `ms`, unchanged.
    fn soft_stamp(&self, ms: u64) -> u64 {
        let clock = self.soft_stamp_clock();
        if clock == 0 {
            ms
        } else if ms == 0 {
            clock
        } else {
            ms.min(clock)
        }
    }

    /// The clock [`Self::soft_stamp`] caps a stamp at, `0` when it caps none
    /// (not [`Self::soft_stamp_gc_clock`] under the HotSpot LRU).
    fn soft_stamp_clock(&self) -> u64 {
        if !(self.soft_stamp_gc_clock && self.hotspot_soft_lru) {
            return 0;
        }
        // gci i20/gn (R10): before the first round, HotSpot's clock is the
        // VM-start time; here, the processor's construction time.
        if self.soft_gc_clock_ms != 0 {
            self.soft_gc_clock_ms
        } else {
            self.soft_clock_origin_ms
        }
    }

    /// gcj j4/rf (item 5): the stamp a locked touch at `now_ms` writes into
    /// the `SoftReference`'s `timestamp` field, keyed by
    /// [`soft_touch_key_ms`] on the clock this call publishes
    /// ([`Self::soft_touch_clock`]; `0` -- the millisecond -- unless
    /// [`Self::soft_touch_keyed_on_clock`]). Call under the lock that made the
    /// touch, as the epoch read.
    pub fn soft_touch_stamp_now(&self, now_ms: u64) -> Option<i64> {
        let clock = if self.soft_touch_keyed_on_clock {
            self.soft_stamp_clock()
        } else {
            0
        };
        self.soft_touch_clock.store(clock, Ordering::Release);
        soft_touch_stamp(
            soft_touch_key_ms(now_ms, clock),
            self.soft_touch_epoch_now(),
        )
    }

    /// The published touch clock, shared with the `get()` fast path (see
    /// [`Self::soft_touch_clock`]).
    pub fn soft_touch_clock(&self) -> Arc<AtomicU64> {
        Arc::clone(&self.soft_touch_clock)
    }

    /// The mutator-clock cell the `get()` fast path raises when it skips the
    /// lock (see [`Self::soft_touch_observed_ms`]).
    pub fn soft_touch_observed_ms(&self) -> Arc<AtomicU64> {
        Arc::clone(&self.soft_touch_observed_ms)
    }

    /// The mutator clock this processor has observed: the locked touches'
    /// [`Self::last_observed_clock_ms`] and the skipped ones'
    /// [`Self::soft_touch_observed_ms`].
    fn observed_clock_ms(&self) -> u64 {
        self.last_observed_clock_ms
            .max(self.soft_touch_observed_ms.load(Ordering::Acquire))
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
        self.note_row_outside_bind(ReferenceType::Phantom);
        self.register_phantom_cleaner_row(reference_obj, referent, queue);
    }

    /// t1 wave 8, lane I77: tell the attached [`AllocationNoteStripes`] that
    /// a soft / weak / phantom row was registered outside the pre-collection
    /// bind (the public `discover_*` entry points: constructor bridges, tests),
    /// so an application retire never skips the locked path for a reference
    /// that may own such a row ([`AllocationNoteStripes::cancel_recent`]).
    fn note_row_outside_bind(&self, ref_type: ReferenceType) {
        if matches!(
            ref_type,
            ReferenceType::Soft | ReferenceType::Weak | ReferenceType::Phantom
        ) {
            if let Some(notes) = &self.allocation_notes {
                notes.note_row_outside_bind();
            }
        }
    }

    /// [`Self::discover_phantom_cleaner`]'s registration, without the
    /// outside-the-bind mark (the bind's own registration).
    fn register_phantom_cleaner_row(
        &mut self,
        reference_obj: usize,
        referent: usize,
        queue: Option<usize>,
    ) {
        self.register_row(ReferenceType::Phantom, reference_obj, referent, queue);
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
        self.note_row_outside_bind(ref_type);
        self.register_row(ref_type, reference_obj, referent, queue);
    }

    /// [`Self::discover_reference`]'s registration, without the
    /// outside-the-bind mark (t1 wave 8, lane I77: the pre-collection bind
    /// registers through here, so it never sets the mark).
    fn register_row(
        &mut self,
        ref_type: ReferenceType,
        reference_obj: usize,
        referent: usize,
        queue: Option<usize>,
    ) {
        self.register_row_inner(ref_type, reference_obj, referent, queue, None, None, false);
    }

    fn register_row_inner(
        &mut self,
        ref_type: ReferenceType,
        reference_obj: usize,
        referent: usize,
        queue: Option<usize>,
        override_seq: Option<u64>,
        mut deferred_skip: Option<&mut Vec<usize>>,
        skip_app_row_index: bool,
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
        // gci i11/rp: on HotSpot's clock under `CRATONVM_SOFTREF_STAMP_GC_CLOCK`
        // (`soft_stamp`; the identity otherwise).
        let creation_stamp = match ref_type {
            ReferenceType::Soft => self.soft_stamp(self.observed_clock_ms()),
            _ => 0,
        };
        let seq = override_seq.unwrap_or_else(|| self.take_registration_seq());
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
            registration_seq: seq,
            constructed: false,
            extra_finalizers: 0,
            init_returns: 0,
            jni_reinit_calls: 0,
        };
        // A fresh row is active, so it wins its address in the application
        // retirement index (see `app_row_index`). Only while that index is
        // current; a stale one is rebuilt from the lists on its next use.
        // Not `app_row_index_stale = true` (tried 2026-11-07): the pre-collection bind
        // pass asks `has_active_reference_row` once per bound row, so a stale mark here
        // rebuilt the whole index per row (quadratic; 100 000 pending rows never finished).
        if skip_app_row_index {
            self.app_row_index_stale = true;
        } else if !self.app_row_index_stale {
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
            ReferenceType::Weak => {
                if !entry.cleared && !entry.enqueued {
                    if let Some(buf) = deferred_skip.as_deref_mut() {
                        buf.push(entry.reference_obj);
                    } else {
                        self.persistent_skip_set(entry.reference_obj);
                    }
                }
                self.weak_refs.push(entry);
            }
            ReferenceType::Phantom => {
                if !entry.cleared && !entry.enqueued {
                    if let Some(buf) = deferred_skip.as_deref_mut() {
                        buf.push(entry.reference_obj);
                    } else {
                        self.persistent_skip_set(entry.reference_obj);
                    }
                }
                self.phantom_refs.push(entry);
            }
            ReferenceType::Cleaner => self.cleaner_refs.push(entry),
            ReferenceType::Finalizer => self.finalizer_refs.push(entry),
            ReferenceType::Strong => {} // strong refs need no special treatment
        }
    }

    /// The next registration sequence number, taken (interpreter round i7
    /// wave 143, lane L6): from the attached [`AllocationNoteStripes`] while
    /// one is attached (its notes take theirs without this lock), else from
    /// this processor's own counter, as before.
    fn take_registration_seq(&mut self) -> u64 {
        match &self.allocation_notes {
            Some(notes) => notes.next_seq.fetch_add(1, Ordering::AcqRel),
            None => {
                let seq = self.next_registration_seq;
                self.next_registration_seq += 1;
                seq
            }
        }
    }

    /// Interpreter round i7 wave 143, lane L6 (`CRATONVM_REF_NOTES_PER_THREAD`,
    /// default ON): take allocation notes through `notes` from now on. The
    /// stripes continue this processor's registration sequence. Called once,
    /// by the VM, right after it builds its processor and before any
    /// allocation; a second call (or one after notes were made) keeps the
    /// stripes already attached, absorbing nothing twice.
        pub fn attach_persistent_skip_bits(&mut self, skip: Arc<crate::skip_bits::ZSkipBits>) {
        for e in self.weak_refs.iter().chain(self.phantom_refs.iter()) {
            if !e.cleared && !e.enqueued {
                skip.persistent_set(e.reference_obj);
            }
        }
        self.persistent_skip_bits = Some(skip);
    }

    #[inline]
    fn persistent_skip_set(&self, addr: usize) {
        if let Some(skip) = &self.persistent_skip_bits {
            skip.persistent_set(addr);
        }
    }

    pub fn persistent_skip_set_batch(&self, addrs: &[usize]) {
        if let Some(skip) = &self.persistent_skip_bits {
            skip.persistent_set_batch(addrs);
        }
    }

    #[inline]
    fn persistent_skip_clear(&self, addr: usize) {
        if let Some(skip) = &self.persistent_skip_bits {
            skip.persistent_clear(addr);
        }
    }

    pub fn attach_allocation_note_stripes(&mut self, notes: Arc<AllocationNoteStripes>) {
        if self.allocation_notes.is_some() {
            return;
        }
        notes
            .next_seq
            .fetch_max(self.next_registration_seq, Ordering::AcqRel);
        self.allocation_notes = Some(notes);
    }

    /// Move every striped allocation note into [`Self::allocated_unbound`],
    /// as [`Self::note_allocated_reference`] would have pushed it at its
    /// allocation (its sequence number and soft clock are the note's own).
    /// One relaxed-acquire load when nothing waits. See
    /// [`AllocationNoteStripes`] for the callers.
    pub fn absorb_allocation_notes(&mut self) {
        let Some(notes) = self.allocation_notes.as_ref().map(Arc::clone) else {
            return;
        };
        for n in notes.drain_unsorted() {
            let row = ReferenceEntry {
                ref_type: n.ref_type,
                reference_obj: n.reference_obj,
                referent: 0,
                queue_addr: None,
                enqueued: false,
                cleared: false,
                // As `note_allocated_reference` stamps it, from the clock
                // read at the note (the processor's clocks read now, which
                // are only ever later).
                last_access_time_ms: if n.ref_type == ReferenceType::Soft {
                    self.soft_stamp(
                        n.clock_ms
                            .max(self.soft_gc_clock_ms)
                            .max(self.last_observed_clock_ms),
                    )
                } else {
                    0
                },
                clear_emitted: false,
                action_emitted: false,
                runs_cleaner: n.runs_cleaner && n.ref_type == ReferenceType::Phantom,
                registration_seq: n.seq,
                constructed: false,
                extra_finalizers: 0,
                init_returns: 0,
                jni_reinit_calls: 0,
            };
            self.allocated_unbound.push(AllocatedReference {
                row,
                class_id: n.class_id,
            });
        }
    }

    /// gcg g3/rw — register a soft / weak / phantom `Reference` at
    /// ALLOCATION, from its class, before its constructor has run (stage 0 of
    /// `common-d-proposal-marker-discovered-references`; the proposal page
    /// `gcg-g2r-proposal-reference-discovery-at-allocation-not-at-the-constructor-bridge`).
    /// The row is bound to its referent and queue by the next pre-collection
    /// pass ([`Self::bind_allocated_reference`]), which reads them from the
    /// object's own slots -- so discovery no longer depends on the
    /// `java/lang/ref` constructor bridges having run.
    ///
    /// `runs_cleaner` marks a `jdk.internal.ref.Cleaner` (a phantom that runs,
    /// [`ReferenceEntry::runs_cleaner`]); it is ignored for other kinds. Any
    /// other `ref_type` (strong, the synthetic cleaner shape, a finalizer) and
    /// a null address are ignored: finalizables keep their own registration.
    pub fn note_allocated_reference(
        &mut self,
        ref_type: ReferenceType,
        reference_obj: usize,
        class_id: u32,
        runs_cleaner: bool,
    ) {
        if reference_obj == 0
            || !matches!(
                ref_type,
                ReferenceType::Soft | ReferenceType::Weak | ReferenceType::Phantom
            )
        {
            return;
        }
        let seq = self.take_registration_seq();
        let row = ReferenceEntry {
            ref_type,
            reference_obj,
            referent: 0,
            queue_addr: None,
            enqueued: false,
            cleared: false,
            // gcg g5/rs: a `SoftReference`'s LRU stamp at its allocation,
            // as the JDK constructor's `this.timestamp = clock` gives it
            // (the bridge's construction touch, which a shadowed constructor
            // never runs). Applied by `bind_allocated_reference`. Without
            // it the bound row read `last_access == 0` and the next
            // collection cleared a brand-new soft reference
            // (`rs4_gate_shadowed_on`: `soft-recent kept=false`). The same
            // wall clock the bridge's touch reads (`touch_soft_reference`'s
            // caller), never below the processor's own clocks.
            // gci i11/rp: capped at the GC clock under
            // `CRATONVM_SOFTREF_STAMP_GC_CLOCK` (`soft_stamp`), which is
            // exactly the `timestamp = clock` the JDK constructor stores.
            last_access_time_ms: if ref_type == ReferenceType::Soft {
                self.soft_stamp(
                    allocation_clock_ms()
                        .max(self.soft_gc_clock_ms)
                        .max(self.observed_clock_ms()),
                )
            } else {
                0
            },
            clear_emitted: false,
            action_emitted: false,
            runs_cleaner: runs_cleaner && ref_type == ReferenceType::Phantom,
            registration_seq: seq,
            constructed: false,
            extra_finalizers: 0,
            init_returns: 0,
            jni_reinit_calls: 0,
        };
        self.allocated_unbound
            .push(AllocatedReference { row, class_id });
    }

    /// gcg g3/rw — does an ACTIVE soft / weak / phantom row (neither cleared
    /// nor enqueued) name `reference_obj`? O(1) through the application
    /// retirement index (rebuilt first when a collection invalidated it).
    pub fn has_active_reference_row(&mut self, reference_obj: usize) -> bool {
        let Some((list, idx)) = self.app_row_lookup(reference_obj) else {
            return false;
        };
        let rows = match list {
            AppRowList::Soft => &self.soft_refs,
            AppRowList::Weak => &self.weak_refs,
            AppRowList::Phantom => &self.phantom_refs,
        };
        rows.get(idx).is_some_and(|e| !e.cleared && !e.enqueued)
    }

    /// gcg g3/rw — how many allocated `Reference`s wait for a bind.
    pub fn allocated_unbound_len(&self) -> usize {
        // Wave 143 (L6): striped notes not absorbed yet count too.
        self.allocated_unbound.len()
            + self.allocation_notes.as_ref().map_or(0, |n| n.pending())
    }

    /// gcg g3/rw — take every allocated, unbound `Reference` (the
    /// pre-collection pass's first step; it binds or requeues each).
    pub fn take_allocated_unbound(&mut self) -> Vec<AllocatedReference> {
        self.absorb_allocation_notes();
        std::mem::take(&mut self.allocated_unbound)
    }

    /// gcg g3/rw — hand back an allocated `Reference` the binder could not
    /// bind yet: its constructor has not run (a collection between `new` and
    /// `<init>`; the JDK constructor stores a non-null `queue`, at least
    /// `ReferenceQueue.NULL`, so a null queue slot is the binder's test). It
    /// keeps its registration sequence number.
    pub fn requeue_allocated_unbound(&mut self, pending: AllocatedReference) {
        self.allocated_unbound.push(pending);
    }

    /// gcg g3/rw — bind an allocated `Reference` (from
    /// [`Self::take_allocated_unbound`]) to the referent and queue its slots
    /// hold now. The caller has checked that the object at
    /// `pending.row.reference_obj` is still that `Reference` (its class id is
    /// `pending.class_id`) and that its constructor has run, and maps
    /// `ReferenceQueue.NULL` to `None`.
    ///
    /// * a null referent: never discovered (HotSpot's rule; the gate probe's
    ///   `null-referent enqueued=false` line), dropped;
    /// * an ACTIVE row already names the object (the constructor bridge
    ///   registered it): dropped, so nothing is registered twice;
    /// * otherwise the row is registered exactly as
    ///   [`Self::discover_reference`] / [`Self::discover_phantom_cleaner`]
    ///   would, with the registration sequence number of the allocation.
    pub fn has_rows_outside_bind(&self) -> bool {
        match &self.allocation_notes {
            Some(notes) => notes.rows_outside_bind.load(Ordering::Acquire),
            None => true,
        }
    }

    pub fn reserve_active_rows(&mut self, additional: usize) {
        if additional == 0 {
            return;
        }
        self.weak_refs.reserve(additional);
        if !self.app_row_index_stale {
            self.app_row_index.reserve(additional);
        }
    }

    pub fn reserve_active_rows_no_index(&mut self, additional: usize) {
        if additional == 0 {
            return;
        }
        self.weak_refs.reserve(additional);
    }

    pub fn bind_allocated_reference(
        &mut self,
        pending: AllocatedReference,
        referent: usize,
        queue: Option<usize>,
    ) -> BindOutcome {
        self.bind_allocated_reference_inner(pending, referent, queue, None, false)
    }

    pub fn register_discovered_weak_phantom(
        &mut self,
        ref_type: ReferenceType,
        reference_obj: usize,
        class_id: u32,
        runs_cleaner: bool,
        referent: usize,
        queue: Option<usize>,
    ) {
        let already = match ref_type {
            ReferenceType::Weak => self.weak_refs.iter().any(|e| e.reference_obj == reference_obj),
            ReferenceType::Phantom => self.phantom_refs.iter().any(|e| e.reference_obj == reference_obj),
            _ => false,
        };
        if !already {
            self.register_row_inner(
                ref_type,
                reference_obj,
                referent,
                queue,
                None,
                None,
                false,
            );
            let _ = (class_id, runs_cleaner);
        }
    }

    pub fn bind_allocated_reference_deferred_skip(
        &mut self,
        pending: AllocatedReference,
        referent: usize,
        queue: Option<usize>,
        deferred_skip: &mut Vec<usize>,
    ) -> BindOutcome {
        self.bind_allocated_reference_inner(pending, referent, queue, Some(deferred_skip), true)
    }

    fn bind_allocated_reference_inner(
        &mut self,
        pending: AllocatedReference,
        referent: usize,
        queue: Option<usize>,
        deferred_skip: Option<&mut Vec<usize>>,
        batch_prechecked: bool,
    ) -> BindOutcome {
        let obj = pending.row.reference_obj;
        if !batch_prechecked && self.has_rows_outside_bind() && self.has_active_reference_row(obj) {
            return BindOutcome::AlreadyRegistered;
        }
        if referent == 0 {
            return BindOutcome::NullReferent;
        }
        let row = pending.row;
        // t1 wave 8, lane I77: the non-marking registrations -- a bound row
        // whose note was absorbed is not one a striped note can shadow.
        if row.runs_cleaner {
            self.register_phantom_cleaner_row(obj, referent, queue);
        } else {
            self.register_row_inner(
                row.ref_type,
                obj,
                referent,
                queue,
                Some(row.registration_seq),
                deferred_skip,
                batch_prechecked,
            );
        }
        let list = match row.ref_type {
            ReferenceType::Soft => &mut self.soft_refs,
            ReferenceType::Weak => &mut self.weak_refs,
            _ => &mut self.phantom_refs,
        };
        if let Some(e) = list.last_mut().filter(|e| e.reference_obj == obj) {
            e.registration_seq = row.registration_seq;
        }
        // gcg g5/rs: a soft row starts at its allocation's stamp (see
        // `note_allocated_reference`), through the touch so the LRU index
        // is re-keyed with it.
        if row.ref_type == ReferenceType::Soft && row.last_access_time_ms != 0 {
            self.touch_soft_reference(obj, row.last_access_time_ms);
        }
        BindOutcome::Bound
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
        // gci i11/rp: the row's stamp is HotSpot's `timestamp = clock` under
        // `CRATONVM_SOFTREF_STAMP_GC_CLOCK` (`soft_stamp`); `now_ms` itself
        // otherwise. The observed clock above keeps the raw value.
        let now_ms = self.soft_stamp(now_ms);
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

    /// Number of tracked `WeakReference` rows.
    pub fn weak_refs_len(&self) -> usize {
        self.weak_refs.len()
    }

    /// Parallel-capable variant of [`Self::process_references`] for `Sync`
    /// liveness predicates (used when `weak_refs` is large).
    pub fn process_references_sync<F: Fn(usize) -> bool + Sync>(
        &mut self,
        is_marked: &F,
        prune_weak_ref_objs: bool,
        free_heap_mb: usize,
        current_time_ms: u64,
    ) -> ReferenceProcessingResult {
        self.process_references_with_finalizer_trace_inner(
            is_marked,
            Some(is_marked),
            prune_weak_ref_objs,
            None,
            free_heap_mb,
            current_time_ms,
        )
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
        self.process_references_with_finalizer_trace_inner(
            is_marked,
            None,
            false,
            trace_from,
            free_heap_mb,
            current_time_ms,
        )
    }

    fn process_references_with_finalizer_trace_inner(
        &mut self,
        is_marked: &dyn Fn(usize) -> bool,
        sync_is_marked: Option<&(dyn Fn(usize) -> bool + Sync)>,
        prune_weak_ref_objs: bool,
        trace_from: Option<&dyn Fn(&[usize]) -> Vec<usize>>,
        free_heap_mb: usize,
        current_time_ms: u64,
    ) -> ReferenceProcessingResult {
        // gcg g2/r: see `flush_deferred_sweep`.
        self.flush_deferred_sweep();
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
        // unchanged. See `common-d-weak-refs-honour-finalizer-reachability-unlike-hotspot-FIXED-20261010.md`.
        let resurrected = std::mem::take(&mut self.resurrected_this_cycle);
        // gci i11/rp: the census half of the same set -- see
        // `ReferenceProcessingStats::finalizer_refs_resurrected`.
        if !resurrected.is_empty() {
            self.stats.finalizer_refs_resurrected = self
                .finalizer_refs
                .iter()
                .filter(|e| !e.cleared && !e.enqueued && resurrected.contains(&e.referent))
                .count();
        }
        // gci i16/fr (G-9, stop-the-world half): resolve the VM's probe against
        // what the collection resurrected. Both inputs are empty unless the VM
        // noted a probe (`CRATONVM_STW_UNDISCOVERED_REFS_STRONG`), so every
        // other round is unchanged.
        let probe_pairs = std::mem::take(&mut self.stw_probe_pairs);
        let probe_candidates_only = std::mem::take(&mut self.stw_probe_candidates_only);
        let (probe_undiscovered, probe_dead) =
            resolve_stw_undiscovered_probe(&probe_pairs, &probe_candidates_only, &resurrected);
        // gci i27/gr: kept for `carry_probe_undiscovered` (empty without a probe).
        self.last_round_probe_undiscovered.clone_from(&probe_undiscovered);
        self.undiscovered_this_round.extend(probe_undiscovered);
        // A probe referent the collection kept only because the VM made it a
        // resurrection candidate is unmarked for this whole round.
        let raw_is_marked = is_marked;
        let is_marked =
            |addr: usize| -> bool { raw_is_marked(addr) && !probe_dead.contains(&addr) };
        let strongly_marked =
            |addr: usize| -> bool { is_marked(addr) && !resurrected.contains(&addr) };
        // gci i14/gq (G-1): see `weak_before_final_this_round`. One round only.
        let weak_before_final = std::mem::take(&mut self.weak_before_final_this_round);

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
        //
        // gci i14/gq (G-1): a round noted `weak_before_final` leaves the
        // finalizer-reachable closure out of the soft and weak predicates
        // (HotSpot's SoftWeakFinalRefsPhase clears both before the keep-alive
        // phase marks the finalizable objects); `final_is_live` and
        // `phantom_is_live` below are unchanged by it.
        let soft_is_live = |addr: usize| -> bool {
            strongly_marked(addr) || (!weak_before_final && finalizer_live.contains(&addr))
        };

        // SOFT-POLICY FIX (2026-07-26): "now" for the LRU comparison is the
        // later of the caller's clock and the mutator clock the processor has
        // observed through `touch_soft_reference`. The production call sites
        // (`vm/src/runtime/interpreter/gc_and_alloc.rs`: the post-GC path and
        // the G1/generational remark) pass a literal `0`, which made the
        // whole soft-ref policy unreachable; a caller with a real clock (ZGC's
        // backend, every test) already dominates and is unaffected. See the
        // `last_observed_clock_ms` field doc for the full analysis.
        let effective_now_ms = current_time_ms.max(self.observed_clock_ms());

        // Phase 1 (soft) honours the finalizer-reachable closure.
        //
        // gen r5w1/refs5: under `CRATONVM_SOFTREF_HOTSPOT_LRU` the LRU rule
        // runs on HotSpot's inputs — the clock of the previous round's end and
        // the free heap it was handed — expressed through the same two
        // arguments (`threshold = ms_per_mb * free`, `idle = now - access`).
        //
        // zr z5/rv5: the caller's soft-phase-only verdict
        // ([`Self::note_soft_phase_forced_dead`], ZGC's last-ditch
        // conservative clear) applies HERE and nowhere else. It used to reach
        // the processor folded into `is_marked`, so Phases 0 and 2-4 read a
        // still-reachable object as dead too: its weak references were
        // cleared and its `Cleaner` fired while a compiled frame (or a real
        // heap path) still held it, and a finalizable one became a Phase 0
        // root whose closure kept the soft reference after all.
        let soft_forced_dead = std::mem::take(&mut self.soft_phase_forced_dead);
        let soft_phase_is_live =
            |addr: usize| -> bool { !soft_forced_dead.contains(&addr) && soft_is_live(addr) };
        if self.hotspot_soft_lru {
            let (clock, free_mb) = self.hotspot_soft_inputs(free_heap_mb);
            self.process_soft_refs(&soft_phase_is_live, free_mb, clock);
        } else {
            self.process_soft_refs(&soft_phase_is_live, free_heap_mb, effective_now_ms);
        }
        drop(soft_forced_dead);

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
        // `docs/feature-designs/generative-gc-architecture.md`.
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
        // load-bearing: `finalizer_refs` are registered by the VM with
        // `reference_obj == referent == the finalizable object`
        // (`SharedVm::register_finalizable`, which the `Finalizer.register`
        // native reaches through `register_if_finalizable` since gc-common
        // w5-d), and a `cleaner_refs` row's `reference_obj` is the synthetic
        // `Cleaner$Cleanable` shape rather than a `Reference`, so pruning
        // THOSE on reference-object liveness would delete exactly the rows
        // whose job is to fire when the object dies. See that method's doc.
        //
        // So the filter below stays as it is — but now it is correct rather
        // than merely conservative, because every row it can see has a live
        // `Reference` object behind it.
        // gci i15/rf (G-9): an undiscovered row is not a soft survivor of
        // this round (HotSpot never discovered it); its referent is kept by
        // the caller, not by the soft-reachable closure.
        let soft_survivor_roots: Vec<usize> = self
            .soft_refs
            .iter()
            .filter(|e| {
                !e.cleared
                    && !e.enqueued
                    && !self.undiscovered_this_round.contains(&e.reference_obj)
            })
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
        if let Some(sync_raw) = sync_is_marked.filter(|_| self.weak_refs.len() >= 32_768) {
            let weak_is_live_sync = |addr: usize| -> bool {
                (sync_raw(addr) && !probe_dead.contains(&addr) && !resurrected.contains(&addr))
                    || (!weak_before_final && finalizer_live.contains(&addr))
                    || soft_live.contains(&addr)
            };
            self.process_weak_refs_parallel(
                &weak_is_live_sync,
                prune_weak_ref_objs.then_some(sync_raw),
            );
        } else {
            if prune_weak_ref_objs {
                if let Some(sync_raw) = sync_is_marked {
                    self.remove_collected_weak_refs_sync(&|addr| sync_raw(addr));
                    self.stats.weak_refs_discovered = self.weak_refs.len();
                }
            }
            let weak_is_live = |addr: usize| -> bool {
                strongly_marked(addr)
                    || (!weak_before_final && finalizer_live.contains(&addr))
                    || soft_live.contains(&addr)
            };
            self.process_weak_refs(&weak_is_live);
        }

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
        // gci i15/rf (G-9): the note describes this round only.
        self.undiscovered_this_round.clear();

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

        // zt t10/gn2: a ZGC generational young cycle that kept its referents
        // (`CRATONVM_ZGC_GEN_YOUNG_SKIP_REFERENCES`) commits here too, and on
        // the default configuration that leaves the clock where the last major
        // put it, as HotSpot 25's minor does: such a cycle runs no
        // `condemn_idle_soft_refs` (so `soft_clock_pending_ms` is 0), the VM
        // passes `current_time_ms == 0`, and under discovery at allocation
        // every touch the processor hears is already on the clock's scale
        // (`soft_stamp`, the Java `timestamp` refresh), so
        // `last_observed_clock_ms` never exceeds the clock. The t9 opt-in that
        // skipped this commit changed nothing on `Zt9GnSoftClock` and was
        // retired (`zt10gn2_an_armed_young_round_leaves_the_soft_clock`).
        //
        // zt t11/gn3: the free figure is a separate input; under the opt-in
        // `young_round_holds_soft_free` such a round keeps the one the last
        // whole-heap round stored. One round only.
        let hold_soft_free = std::mem::take(&mut self.young_round_holds_soft_free);
        if self.hotspot_soft_lru {
            let held_free = self.soft_free_mb_at_last_gc;
            self.commit_hotspot_soft_clock(free_heap_mb, effective_now_ms);
            if hold_soft_free {
                self.soft_free_mb_at_last_gc = held_free;
            }
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
                    // gci i15/rf (G-9): not discovered this round.
                    || self.undiscovered_this_round.contains(&entry.reference_obj)
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
                } else if self.undiscovered_this_round.contains(&entry.reference_obj) {
                    // gci i15/rf (G-9): not discovered this round; its LRU key
                    // stays for the next one.
                    continue;
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
        for idx in 0..self.weak_refs.len() {
            let entry = &self.weak_refs[idx];
            if entry.cleared || self.undiscovered_this_round.contains(&entry.reference_obj) {
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
            self.clear_weak_ref_at(idx, dbg);
        }
    }

    fn weak_ref_pool() -> &'static crate::evac_pool::EvacPool {
        static POOL: std::sync::OnceLock<crate::evac_pool::EvacPool> = std::sync::OnceLock::new();
        POOL.get_or_init(|| crate::evac_pool::EvacPool::with_name(0, "gc-refs"))
    }

    fn process_weak_refs_parallel<
        F: Fn(usize) -> bool + Sync,
        R: Fn(usize) -> bool + Sync + ?Sized,
    >(
        &mut self,
        is_marked: &F,
        prune_ref_obj_is_live: Option<&R>,
    ) {
        let dbg = gc_flags().dbg_watchref;
        let len = self.weak_refs.len();
        let max_workers = if len >= 131_072 { 16 } else { 4 };
        let workers = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(4)
            .clamp(1, max_workers);
        let weak_refs = &self.weak_refs;
        let undiscovered = &self.undiscovered_this_round;
        let slots: Vec<std::sync::Mutex<(Vec<usize>, Vec<usize>)>> = (0..workers)
            .map(|_| std::sync::Mutex::new((Vec::new(), Vec::new())))
            .collect();
        Self::weak_ref_pool().run_parallel(workers, |w, n| {
            let chunk_size = len.div_ceil(n);
            let start = w * chunk_size;
            if start >= len {
                return;
            }
            let end = (start + chunk_size).min(len);
            let mut to_clear = Vec::new();
            let mut dead_ref_objs = Vec::new();
            for (offset, entry) in weak_refs[start..end].iter().enumerate() {
                if let Some(ref_obj_live) = prune_ref_obj_is_live {
                    if !ref_obj_live(entry.reference_obj) {
                        dead_ref_objs.push(start + offset);
                        continue;
                    }
                }
                if entry.cleared || undiscovered.contains(&entry.reference_obj) {
                    continue;
                }
                if !is_marked(entry.referent) {
                    to_clear.push(start + offset);
                } else if dbg {
                    eprintln!(
                        "[watchref] weak KEEP ref_obj=0x{:x} referent=0x{:x}",
                        entry.reference_obj, entry.referent
                    );
                }
            }
            *slots[w].lock().unwrap() = (to_clear, dead_ref_objs);
        });
        let mut dead_lists = Vec::with_capacity(slots.len());
        let mut total_dead = 0usize;
        for slot in slots {
            let (to_clear, dead_ref_objs) = slot.into_inner().unwrap();
            for idx in to_clear {
                self.clear_weak_ref_at(idx, dbg);
            }
            total_dead += dead_ref_objs.len();
            dead_lists.push(dead_ref_objs);
        }
        if total_dead != 0 {
            self.prune_weak_ref_indices(dead_lists);
            self.stats.weak_refs_discovered =
                self.stats.weak_refs_discovered.saturating_sub(total_dead);
        }
    }

    fn clear_weak_ref_at(&mut self, idx: usize, dbg: bool) {
        let entry = &mut self.weak_refs[idx];
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
                if let Some(s) = &self.persistent_skip_bits {
            s.persistent_clear(entry.reference_obj);
        }
        self.stats.weak_refs_cleared += 1;
        if let Some(q) = entry.queue_addr {
            self.pending_queues
                .entry(q)
                .or_default()
                .push(entry.reference_obj);
            entry.enqueued = true;
        }
    }

    // -- Phase 3: Cleaner / FinalReferences --------------------------------

    /// `finalizer_is_live` gates `finalize()` scheduling (raw marking + the
    /// soft-reachable closure); `cleaner_is_live` gates `Cleaner` actions
    /// (raw marking + soft-reachable + finalizer-reachable, because `Cleaner`
    /// is a `PhantomReference`). See the call site for why the two differ.
    ///
    /// gci i11/rp (R6): the finalizer arm never fires in production; every
    /// backend resurrects (marks) a dead finalizable object before this round,
    /// and finalization goes through the resurrection channel instead. See the
    /// module doc, item 3.
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
            // gci i15/rf (G-9): not discovered this round.
            if self.undiscovered_this_round.contains(&entry.reference_obj) {
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
                        if let Some(skip) = &self.persistent_skip_bits {
                skip.persistent_clear(entry.reference_obj);
            }
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
        // Wave 143 (L6): a striped note's address moves with its object too.
        self.absorb_allocation_notes();
        // PERF (gc-common w2-f): nothing below does anything for an empty map
        // — every relocation is a lookup miss and every rebuild reproduces the
        // table it replaces — yet each rebuild allocated and re-hashed a whole
        // table, inside the pause. Same for a map that moved none of this
        // processor's KEYS (the identity entries a non-moving cycle emits for
        // watched survivors): see `moved_any` and the `*_moved` flags below.
        //
        // gcg g2/r: held sweep spans name PRE-collection addresses, so they
        // are flushed while the rows still carry those addresses, before the
        // relocation below (see `flush_deferred_sweep`).
        self.flush_deferred_sweep();
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
        /// Would a rebuild of an address-keyed table change any key?
        fn moved_any<'a>(
            mut keys: impl Iterator<Item = &'a usize>,
            map: &cratonvm_types::PointerMap,
        ) -> bool {
            keys.any(|k| matches!(map.get(k), Some(&new) if new != 0 && new != *k))
        }

        // gcj j4/rf: where the soft rows moved to, for the `soft_pre_nulled`
        // rebuild below (only when it has something to rebuild).
        let soft_move_targets: FxHashSet<usize> =
            if self.address_rebuilds_exact && !self.soft_pre_nulled.is_empty() {
                self.soft_refs
                    .iter()
                    .filter_map(|e| match pointer_map.get(&e.reference_obj) {
                        Some(&new) if new != 0 && new != e.reference_obj => Some(new),
                        _ => None,
                    })
                    .collect()
            } else {
                FxHashSet::default()
            };
        let soft_moved = relocate_list(&mut self.soft_refs, pointer_map);
        let mut moved_skip_addrs = Vec::new();
        if let Some(skip) = &self.persistent_skip_bits {
            for e in self.weak_refs.iter().chain(self.phantom_refs.iter()) {
                if !e.cleared && !e.enqueued {
                    if let Some(&new) = pointer_map.get(&e.reference_obj) {
                        if new != 0 && new != e.reference_obj {
                            skip.persistent_clear(e.reference_obj);
                            moved_skip_addrs.push(new);
                        }
                    }
                }
            }
        }
        let weak_moved = relocate_list(&mut self.weak_refs, pointer_map);
        let phantom_moved = relocate_list(&mut self.phantom_refs, pointer_map);
        if !moved_skip_addrs.is_empty() {
            if let Some(skip) = &self.persistent_skip_bits {
                for addr in moved_skip_addrs {
                    skip.persistent_set(addr);
                }
            }
        }
        relocate_list(&mut self.cleaner_refs, pointer_map);
        if relocate_list(&mut self.finalizer_refs, pointer_map) {
            // `finalizer_active` is keyed on these addresses.
            self.finalizer_active_stale = true;
        }
        // JIT round 21 wave 18: the withdrawn finalizer rows are not watched
        // (see `withdrawn_finalizables`), so a collection forgets them rather
        // than trusting a map that never saw their objects.
        self.withdrawn_finalizables.clear();
        // Row addresses changed; the application retirement index is keyed on
        // them. Rebuilt lazily on its next use.
        if soft_moved || weak_moved || phantom_moved {
            self.app_row_index_stale = true;
        }

        // gcg g3/rw: the allocated, unbound `Reference`s move with their
        // objects (a plain list: no key is another's target).
        for a in self.allocated_unbound.iter_mut() {
            if let Some(&new_addr) = pointer_map.get(&a.row.reference_obj) {
                if new_addr != 0 {
                    a.row.reference_obj = new_addr;
                }
            }
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
        //
        // gcj j4/rf (`address_rebuilds_exact`): a key absent from the map did
        // not move, so it is either its row's (unmoved) address or a dead
        // `Reference`'s; a live one may have slid onto the latter. The moved
        // keys are inserted second, so they win such an address, and an
        // unmoved key is kept only while a row (already relocated above)
        // names it.
        let exact = self.address_rebuilds_exact;
        let rekey_identity = moved_any(self.identity_stamps.keys(), pointer_map);
        let rekey_referent_class = moved_any(self.referent_class_stamps.keys(), pointer_map);
        let row_keys: FxHashSet<usize> = if exact && (rekey_identity || rekey_referent_class) {
            self.soft_refs
                .iter()
                .chain(self.weak_refs.iter())
                .chain(self.phantom_refs.iter())
                .chain(self.cleaner_refs.iter())
                .chain(self.finalizer_refs.iter())
                .map(|e| e.reference_obj)
                .collect()
        } else {
            FxHashSet::default()
        };
        if rekey_identity {
            self.identity_stamps =
                rebuild_address_table(&self.identity_stamps, pointer_map, exact, &row_keys);
        }

        // Same treatment, same reason, for the referent-class table: its keys
        // are `reference_obj` addresses and a slide moves them.
        if rekey_referent_class {
            self.referent_class_stamps =
                rebuild_address_table(&self.referent_class_stamps, pointer_map, exact, &row_keys);
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
        //
        // gcj j4/rf (`address_rebuilds_exact`): an unmoved key that a MOVED
        // soft row landed on is dropped -- its own object did not move (it is
        // not a map key) and lost its address, so it is dead, and the address
        // is now the moved row's, which this collection did not condemn.
        if !self.soft_pre_nulled.is_empty() {
            let mut moved: FxHashSet<usize> =
                FxHashSet::with_capacity_and_hasher(self.soft_pre_nulled.len(), Default::default());
            for addr in self.soft_pre_nulled.iter() {
                let now = match pointer_map.get(addr) {
                    Some(&new) if new != 0 => new,
                    _ if soft_move_targets.contains(addr) => continue,
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
    ///
    /// Returns how many rows it dropped (gcj j4/rf: the Generational audit of
    /// the skipped pass reads it).
    pub fn retain_shaped_weak_phantom(
        &mut self,
        still_a_reference: &dyn Fn(usize) -> bool,
    ) -> usize {
        let before = self.weak_refs.len() + self.phantom_refs.len();
        // gcd d9/e: a recycled `Reference`'s deferred enqueue pair goes with
        // its row (`drop_pending_enqueues_of`).
        let track_pending = !self.pending_queues.is_empty();
        let mut dropped_enqueued: FxHashSet<usize> = FxHashSet::default();
                let skip = self.persistent_skip_bits.clone();
        let mut keep = |e: &ReferenceEntry| -> bool {
            let live = still_a_reference(e.reference_obj);
            if !live {
                                if let Some(s) = &skip {
                    s.persistent_clear(e.reference_obj);
                }
                if track_pending && e.enqueued {
                    dropped_enqueued.insert(e.reference_obj);
                }
            }
            live
        };
        self.weak_refs.retain(&mut keep);
        self.phantom_refs.retain(&mut keep);
        self.drop_pending_enqueues_of(dropped_enqueued);
        let dropped = before - (self.weak_refs.len() + self.phantom_refs.len());
        if dropped != 0 {
            self.app_row_index_stale = true;
            // gcj j4/rf (`address_rebuilds_exact`): the dropped rows' stamps
            // go with them, as in `remove_collected`; left, a stale stamp
            // refuses the writes of the next `Reference` at that address.
            if self.address_rebuilds_exact {
                self.prune_identity_stamps();
            }
        }
        dropped
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
    pub fn stamp_referent_class(&mut self, reference_obj: usize, class_id: u64) {
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

    /// Whether the pre-collection pass nulled at least one referent slot this
    /// cycle (`false` on ZGC's concurrent-refs skip path, or when zero active
    /// slots were nulled).
    pub fn has_pre_gc_nulled(&self) -> bool {
        !self.pre_gc_nulled.is_empty()
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
    ///
    /// gcj j4/rf (`gcj-j1rf-reference-processor-review-residuals` item 1): a
    /// row of [`Self::undiscovered_this_round`] is not retired either. The G1
    /// full collection's compaction pause notes the rows its remark left
    /// undiscovered AFTER [`Self::weak_phantom_active_triples`] marked them
    /// examined, and takes them out of the null list, so they reach here
    /// examined and not nulled although the round means to leave them alone:
    /// retired, the referent was never cleared, the queue never notified, and
    /// the row held its referent strongly while the `Reference` lived.
    pub fn retire_entries_the_pre_gc_pass_found_cleared(&mut self) -> usize {
        if self.pre_gc_examined.is_empty() {
            return 0;
        }
        let undiscovered = &self.undiscovered_this_round;
        let mut retired = 0usize;
                let skip = self.persistent_skip_bits.clone();
        for e in self.weak_refs.iter_mut() {
            if !e.cleared
                && !e.enqueued
                && self.pre_gc_examined.contains(&e.reference_obj)
                && !self.pre_gc_nulled.contains(&e.reference_obj)
                && !undiscovered.contains(&e.reference_obj)
            {
                e.cleared = true;
                e.enqueued = true;
                e.clear_emitted = true;
                                if let Some(s) = &skip {
                    s.persistent_clear(e.reference_obj);
                }
                retired += 1;
            }
        }
        let mut stale_lru_keys: Vec<(u64, usize)> = Vec::new();
        for (idx, e) in self.soft_refs.iter_mut().enumerate() {
            if !e.cleared
                && !e.enqueued
                && self.pre_gc_examined.contains(&e.reference_obj)
                && !self.pre_gc_nulled.contains(&e.reference_obj)
                && !undiscovered.contains(&e.reference_obj)
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
    ///
    /// gci i11/rp (R4): when two ACTIVE rows share the address (a dead
    /// `Reference`'s row not pruned yet and a new one registered on the
    /// recycled block, gcd d2/g), the LATER registration is the one converted,
    /// the rule every other address resolver follows (`rebuild_app_row_index`,
    /// `claim_soft_addr`); it used to be the first row in list order. A caller
    /// that knows the refused pair's referent says so with
    /// [`Self::clear_after_refused_restore_of`], which picks the row exactly.
    pub fn clear_after_refused_restore(&mut self, reference_obj: usize) -> bool {
        self.clear_refused_row(reference_obj, None)
    }

    /// gcj j4/rf (rp residuals item 2): the `[soft, weak]` rows the refused
    /// restores cleared since the last call, and reset. The VM adds them to
    /// the pause's `-Xlog:gc+ref` census, which it builds from the round's
    /// `stats` before the restore pass runs.
    pub fn take_late_cleared(&mut self) -> [u64; 2] {
        std::mem::take(&mut self.late_cleared)
    }

    /// gci i11/rp (R4): [`Self::clear_after_refused_restore`] for the pair
    /// `(reference_obj, referent)` the restore pass refused -- the row whose
    /// registry referent is `referent` wins the address (two active rows at one
    /// address name different referents: the dead `Reference`'s and the new
    /// one's), the later registration otherwise.
    pub fn clear_after_refused_restore_of(
        &mut self,
        reference_obj: usize,
        referent: usize,
    ) -> bool {
        self.clear_refused_row(reference_obj, Some(referent))
    }

    /// The position in `rows` of the ACTIVE row at `reference_obj` a refusal
    /// converts: among the active rows at the address, those naming
    /// `referent` when one is given and any does, then the latest
    /// registration.
    fn refused_row_position(
        rows: &[ReferenceEntry],
        reference_obj: usize,
        referent: Option<usize>,
    ) -> Option<usize> {
        let active =
            |e: &ReferenceEntry| e.reference_obj == reference_obj && !e.cleared && !e.enqueued;
        let latest = |want_referent: Option<usize>| {
            rows.iter()
                .enumerate()
                .filter(|&(_, e)| active(e) && want_referent.map_or(true, |r| e.referent == r))
                .max_by_key(|&(_, e)| e.registration_seq)
                .map(|(i, _)| i)
        };
        match referent {
            Some(r) => latest(Some(r)).or_else(|| latest(None)),
            None => latest(None),
        }
    }

    fn clear_refused_row(&mut self, reference_obj: usize, referent: Option<usize>) -> bool {
        if let Some(i) = Self::refused_row_position(&self.weak_refs, reference_obj, referent) {
            let e = &mut self.weak_refs[i];
            e.cleared = true;
            e.clear_emitted = true;
                        if let Some(skip) = &self.persistent_skip_bits {
                skip.persistent_clear(reference_obj);
            }
            self.stats.weak_refs_cleared += 1;
            self.late_cleared[1] += 1;
            if let Some(q) = e.queue_addr {
                e.enqueued = true;
                self.pending_queues
                    .entry(q)
                    .or_default()
                    .push(reference_obj);
            }
            // The row stopped being active (`app_row_index` prefers active rows).
            self.app_row_index_stale = true;
            return true;
        }
        if let Some(idx) = Self::refused_row_position(&self.soft_refs, reference_obj, referent) {
            let e = &mut self.soft_refs[idx];
            e.cleared = true;
            e.clear_emitted = true;
            self.stats.soft_refs_cleared += 1;
            self.late_cleared[0] += 1;
            let key = (e.last_access_time_ms, idx);
            if let Some(q) = e.queue_addr {
                e.enqueued = true;
                self.pending_queues
                    .entry(q)
                    .or_default()
                    .push(reference_obj);
            }
            self.soft_ref_lru_index.remove(&key);
            self.app_row_index_stale = true;
            return true;
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
        self.referent_class_stamps.clear();
        self.resurrected_this_cycle.clear();
        self.weak_before_final_this_round = false;
        self.undiscovered_this_round.clear();
        self.stw_probe_pairs.clear();
        self.stw_probe_candidates_only.clear();
        self.soft_phase_forced_dead.clear();
        // gci i31/gq (G-2): a staged set never outlives the round it was for.
        self.remark_staged_resurrected.clear();
        // zt t11/gn3: nor does a held free figure.
        self.young_round_holds_soft_free = false;
    }

    /// gci i14/gq (G-1): the NEXT processing round clears soft and weak
    /// references to objects reachable only through a dead finalizable object
    /// (HotSpot: soft / weak references are cleared before finalization), for
    /// a caller that has not resurrected those objects before the round -- G1's
    /// final remark (`g1_remark_process_references`, under
    /// `CRATONVM_G1_REMARK_WEAK_BEFORE_FINAL`). The generational remark and the
    /// stop-the-world paths mark the dead finalizables first and say so through
    /// [`Self::note_resurrected_finalizables`]; their Phase 0 closure is empty,
    /// so the note changes nothing there. One round: consumed by the next
    /// [`Self::process_references_with_finalizer_trace`], cleared by
    /// [`Self::finish_pre_gc_cycle`].
    pub fn note_weak_before_final_round(&mut self) {
        self.weak_before_final_this_round = true;
    }

    /// zt t11/gn3 (`CRATONVM_ZGC_GEN_YOUNG_HOLDS_SOFT_FREE`, default ON): the NEXT
    /// processing round ends a young cycle that kept every referent, and
    /// leaves the free-at-last-gc figure of the soft LRU policy as the last
    /// whole-heap round stored it (see [`Self::young_round_holds_soft_free`]).
    /// The VM's pre-collection pass STORES its answer on every collection
    /// (`false` almost always), so a note never reaches a later collection's
    /// round. One round: consumed by the next
    /// [`Self::process_references_with_finalizer_trace`], cleared by
    /// [`Self::finish_pre_gc_cycle`].
    pub fn set_young_round_holds_soft_free(&mut self, hold: bool) {
        self.young_round_holds_soft_free = hold;
    }

    /// gci i15/rf (G-9): the NEXT processing round leaves the soft / weak /
    /// phantom rows of these `Reference` OBJECTS alone -- neither cleared nor
    /// enqueued, LRU key kept, not a soft survivor root -- because HotSpot
    /// never discovered them: the caller (G1's final remark, under
    /// `CRATONVM_G1_REMARK_UNDISCOVERED_REFS_STRONG`) found each one unmarked
    /// but reachable from an object the same remark keeps alive (a dead
    /// finalizable), and keeps its referent itself. One round: emptied at the
    /// end of the next [`Self::process_references_with_finalizer_trace`] and
    /// by [`Self::finish_pre_gc_cycle`]. Finalizer and cleaner rows are never
    /// affected (their `Reference` is held by the JDK's own strong lists).
    pub fn note_undiscovered_references(&mut self, reference_objs: &[usize]) {
        self.undiscovered_this_round
            .extend(reference_objs.iter().copied());
    }

    /// gci i16/fr (G-9, stop-the-world half; `CRATONVM_STW_UNDISCOVERED_REFS_STRONG`):
    /// the VM's probe for the NEXT round, all PRE-collection addresses.
    ///
    /// * `pairs` -- `(Reference, referent)` rows whose slot 0 the
    ///   pre-collection pass nulled and which a walk from the registered
    ///   finalizable objects reached before the collection. Every pair whose
    ///   `Reference` the round finds in the resurrection set
    ///   ([`Self::note_resurrected_finalizables`]: marked only by the
    ///   collection's finalizer resurrection) is left alone, as
    ///   [`Self::note_undiscovered_references`] does; the VM had the
    ///   collection keep its referent.
    /// * `candidates_only` -- probe referents the collection kept only because
    ///   the VM made them resurrection candidates. Unless a resolved pair keeps
    ///   one, the round treats it as unmarked in every phase.
    ///
    /// Note it AFTER `note_resurrected_finalizables` or before; both are read
    /// at the round's start. One round: consumed by the next
    /// [`Self::process_references_with_finalizer_trace`] and emptied by
    /// [`Self::finish_pre_gc_cycle`].
    pub fn note_stw_undiscovered_probe(
        &mut self,
        pairs: &[(usize, usize)],
        candidates_only: &[usize],
    ) {
        self.stw_probe_pairs.extend_from_slice(pairs);
        self.stw_probe_candidates_only
            .extend(candidates_only.iter().copied().filter(|&a| a != 0));
    }

    /// gci i17/fz (i16/fr's residual): the PRE-collection addresses noted so
    /// far for the next round as marked only by the collection's finalizer
    /// resurrection ([`Self::note_resurrected_finalizables`]: the dead
    /// finalizable objects and the collector's resurrection closure). Read by
    /// the VM's stop-the-world probe, inside the pause and after the note, to
    /// find the part of that closure only a candidate-only probe referent
    /// reaches. Read-only; the round consumes the set as before.
    pub fn resurrected_noted_this_cycle(&self) -> &FxHashSet<usize> {
        &self.resurrected_this_cycle
    }

    /// gci i27/gr (`gci-i15rf-g1-remark-loses-a-reference-only-a-dead-finalizable-reaches-RETIRED-UNFIXED-20261018`
    /// residual, opt-in `CRATONVM_G1_FULL_GC_CARRIES_UNDISCOVERED_REFS`): carry
    /// the `Reference` objects the round just run left alone as undiscovered
    /// (its stop-the-world probe's verdict) past that round, re-addressed
    /// through the collection's `pointer_map` (an address it does not map did
    /// not move), valid while the heap's collection count is `stamp`. Replaces
    /// any earlier carry; an empty round carries nothing. Call after the round
    /// (`process_references`), before the next one.
    ///
    /// Why: G1's `System.gc()` is a young pause followed by a marking cycle,
    /// where HotSpot runs ONE full collection. A `Reference` only a dead
    /// finalizable object reaches is never discovered by that collection, so
    /// HotSpot keeps its referent and clears it in the NEXT collection. The
    /// young pause gets that right (the probe); the cycle's remark then marks
    /// the `Reference` through the now-queued finalizable (a mark root) and
    /// discovers it a second time inside the same full collection, clearing a
    /// referent HotSpot keeps. The remark reads the carry
    /// ([`Self::carried_undiscovered_for`]) and leaves those rows alone too.
    pub fn carry_probe_undiscovered(
        &mut self,
        pointer_map: &cratonvm_types::PointerMap,
        stamp: u64,
    ) {
        let last = std::mem::take(&mut self.last_round_probe_undiscovered);
        self.carried_undiscovered = last
            .into_iter()
            .map(|a| pointer_map.get(&a).copied().unwrap_or(a))
            .filter(|&a| a != 0)
            .collect();
        self.carried_undiscovered_stamp = stamp;
    }

    /// gci i27/gr: the carried `Reference` objects ([`Self::carry_probe_undiscovered`])
    /// when the heap's collection count is still the one they were carried at
    /// (no collection moved anything since), empty otherwise. Read-only: every
    /// remark of the same full collection reads the same carry (a full cycle
    /// that finishes an open cycle first runs two).
    pub fn carried_undiscovered_for(&self, stamp: u64) -> Vec<usize> {
        if self.carried_undiscovered_stamp == stamp {
            self.carried_undiscovered.clone()
        } else {
            Vec::new()
        }
    }

    /// gci i27/gr: forget the carry (the switch is off, or the caller is done
    /// with it).
    pub fn drop_carried_undiscovered(&mut self) {
        self.carried_undiscovered.clear();
        self.carried_undiscovered_stamp = 0;
    }

    /// gci i48/rf48 (`gci-i15rf-...-20261014` residual, G1 `System.gc()`):
    /// stage the `Reference` objects a G1 remark left undiscovered
    /// (its rows survived the pre-prune and the round left them alone), valid
    /// while the heap's collection count is `stamp`. Replaces any earlier
    /// stage, disarmed. Nothing reads it until [`Self::arm_full_gc_undiscovered`].
    ///
    /// Why: G1's `System.gc()` is a young pause, a marking cycle and a
    /// compaction pause, where HotSpot runs ONE full collection. The
    /// compaction pause is a stop-the-world pause like any other: its
    /// pre-collection pass nulled the slot 0 of a `Reference` the remark had
    /// just left undiscovered, its finalizer resurrection reached the
    /// `Reference` through the now-queued finalizable but not the referent,
    /// and its round cleared the reference -- a second discovery inside the
    /// one collection, which no carry into the remark could prevent.
    pub fn stage_full_gc_undiscovered(&mut self, reference_objs: &[usize], stamp: u64) {
        let mut refs = reference_objs.to_vec();
        refs.retain(|&a| a != 0);
        refs.sort_unstable();
        refs.dedup();
        self.full_gc_undiscovered = refs;
        self.full_gc_undiscovered_stamp = stamp;
        self.full_gc_undiscovered_armed = false;
    }

    /// gci i48/rf48: the full-collection door, just before its compaction
    /// pause, arms the staged set when no collection ran since the remark
    /// staged it (`stamp` is the heap's collection count now); a stale stage
    /// is dropped. Returns whether a non-empty set is armed.
    pub fn arm_full_gc_undiscovered(&mut self, stamp: u64) -> bool {
        if self.full_gc_undiscovered_stamp != stamp || self.full_gc_undiscovered.is_empty() {
            self.drop_full_gc_undiscovered();
            return false;
        }
        self.full_gc_undiscovered_armed = true;
        true
    }

    /// gci i48/rf48: the armed set, taken (emptied) by the first
    /// pre-collection pass that asks at the count it was staged for; empty
    /// when unarmed or stale (a stale set is dropped). Ascending, each once.
    pub fn take_armed_full_gc_undiscovered(&mut self, stamp: u64) -> Vec<usize> {
        if !self.full_gc_undiscovered_armed || self.full_gc_undiscovered_stamp != stamp {
            if self.full_gc_undiscovered_armed {
                self.drop_full_gc_undiscovered();
            }
            return Vec::new();
        }
        let refs = std::mem::take(&mut self.full_gc_undiscovered);
        self.drop_full_gc_undiscovered();
        refs
    }

    /// gci i48/rf48: forget the staged / armed set (the door is over).
    pub fn drop_full_gc_undiscovered(&mut self) {
        self.full_gc_undiscovered.clear();
        self.full_gc_undiscovered_stamp = 0;
        self.full_gc_undiscovered_armed = false;
    }

    /// gci i31/gq (G-2 of `gci-i14gq-g1-reference-processing-and-finalization-review-findings-FIXED-20261016`,
    /// default-ON `CRATONVM_G1_SYSTEM_GC_DEFERS_QUEUED_FINALIZABLES`): carry the
    /// finalizable objects a G1 full-collection door's young pause just
    /// queued (`queued`, post-move addresses, null dropped) to the
    /// stop-the-world mark of the same full collection, valid while the
    /// heap's collection count is `stamp`. Replaces any earlier carry.
    ///
    /// Why: HotSpot's G1 `System.gc()` is ONE full collection, in which a dead
    /// finalizable `X` is finalizer-reachable -- a soft / weak reference to
    /// anything only `X` reaches is cleared before `finalize()`. This VM's is
    /// a young pause (which queues `X`) and a marking cycle that roots the
    /// queue, so an Old child only `X` reaches read marked there and kept its
    /// weak reference. The mark holds the carried objects out of its roots
    /// and the remark round reads their closure as resurrected.
    pub fn carry_door_queued_finalizables(&mut self, queued: &[usize], stamp: u64) {
        self.carried_door_queued = queued.iter().copied().filter(|&a| a != 0).collect();
        self.carried_door_queued_stamp = stamp;
    }

    /// gci i31/gq (G-2): the carry ([`Self::carry_door_queued_finalizables`])
    /// when the heap's collection count is still `stamp` (no collection moved
    /// anything since), empty otherwise; the carry is emptied either way, so
    /// one mark reads it at most once.
    pub fn take_carried_door_queued_for(&mut self, stamp: u64) -> Vec<usize> {
        let carried = std::mem::take(&mut self.carried_door_queued);
        let valid = self.carried_door_queued_stamp == stamp;
        self.carried_door_queued_stamp = 0;
        if valid {
            carried
        } else {
            Vec::new()
        }
    }

    /// gci i31/gq (G-2): stage the objects the G1 stop-the-world full mark kept
    /// only for the finalizables it held out of its roots, for the remark round
    /// that follows inside the same callback (replaces any staged set).
    pub fn stage_remark_resurrected(&mut self, closure: Vec<usize>) {
        self.remark_staged_resurrected = closure;
    }

    /// gci i31/gq (G-2): take the staged set ([`Self::stage_remark_resurrected`]);
    /// empty when nothing is staged (every remark but the opt-in's).
    pub fn take_remark_staged_resurrected(&mut self) -> Vec<usize> {
        std::mem::take(&mut self.remark_staged_resurrected)
    }

    /// gci i14/gq (G-3): the NEXT processing round ends at `now_ms` (the
    /// processor's mutator clock, wall milliseconds), for a caller with no
    /// pre-collection pass to record it -- G1's final remark, under
    /// `CRATONVM_G1_REMARK_SOFT_CLOCK_NOW`. HotSpot's
    /// `update_soft_ref_master_clock` sets the soft clock to "now" at the end
    /// of every `process_discovered_references`, the concurrent-mark round
    /// included; without this the remark's round committed only
    /// `last_observed_clock_ms` (the last `touch_soft_reference`, never
    /// advanced under discovery at allocation). Only under
    /// [`Self::hotspot_soft_lru`] (the clock is not read otherwise); the
    /// round's own Phase 1 still judges with the PREVIOUS clock, as HotSpot's.
    /// Monotone like the pre-collection pass's record
    /// ([`Self::condemn_idle_soft_refs`]): a value no round consumes is taken
    /// by the next one, whose own "now" is later.
    pub fn note_round_clock(&mut self, now_ms: u64) {
        if self.hotspot_soft_lru {
            self.soft_clock_pending_ms = self.soft_clock_pending_ms.max(now_ms);
        }
    }

    /// [`Self::note_round_clock`] at the processor's mutator clock now (the
    /// clock `SoftReference` stamps and the pre-collection pass use).
    pub fn note_round_clock_now(&mut self) {
        self.note_round_clock(allocation_clock_ms());
    }

    /// zr z5/rv5: for the processing round of the collection that just ran,
    /// treat `addrs` (referent addresses in this processor's view) as
    /// UNMARKED when deciding SOFT references only -- so a soft reference to
    /// one of them is cleared (and enqueued) rather than kept, while its weak
    /// references, its `FinalReference` and its phantoms / `Cleaner`s still
    /// see the collector's real verdict (the object is alive: memory is never
    /// freed through this set).
    ///
    /// The one caller is ZGC's last-ditch clear-all collection
    /// (`CRATONVM_ZGC_SOFT_CLEAR_CONSERVATIVE`): the specification wants every
    /// softly reachable object released before `OutOfMemoryError`, and a stale
    /// conservatively scanned compiled-frame word is not a Java reference. A
    /// no-op set for every other collector and collection.
    ///
    /// Consumed by the next round and cleared by
    /// [`Self::finish_pre_gc_cycle`], so it never spans two collections.
    pub fn note_soft_phase_forced_dead(&mut self, addrs: impl IntoIterator<Item = usize>) {
        self.soft_phase_forced_dead
            .extend(addrs.into_iter().filter(|&a| a != 0));
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
    /// `docs/feature-designs/profile-guided-inlining.md`.
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
    /// `docs/feature-designs/generative-gc-architecture.md`.
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
    pub fn referent_class_stamp(&self, reference_obj: usize) -> Option<u64> {
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
        // gcg g2/r: a pause never reads a row the concurrent sweep freed.
        self.flush_deferred_sweep();
        self.pre_gc_nulled.clear();
        self.pre_gc_examined.clear();
        self.referent_class_stamps.clear();
        self.pre_gc_nulled.reserve(self.weak_refs.len());
        self.pre_gc_examined.reserve(self.weak_refs.len());
        self.referent_class_stamps.reserve(self.weak_refs.len());
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

    /// gcj j4/rf (`gcj-j1rf-gen-reference-and-finalizer-review-residuals` item
    /// 4): the pre-collection pass left these rows' slots alone on purpose (an
    /// old referent on a pause certain to collect no old storage), so they
    /// were not examined: a row "examined and not nulled" would be retired as
    /// an application clear ([`Self::retire_entries_the_pre_gc_pass_found_cleared`]).
    pub fn forget_pre_gc_examined(&mut self, reference_objs: &[usize]) {
        for r in reference_objs {
            self.pre_gc_examined.remove(r);
        }
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
        // Wave 143 (L6): a striped note whose object died is pruned too.
        self.absorb_allocation_notes();
        // gcg g2/r: see `flush_deferred_sweep`.
        self.flush_deferred_sweep();
        let before = self.total_entry_count();
        let soft_before = self.soft_refs.len();
        let finalizer_before = self.finalizer_refs.len();
        // gcd d9/e: a dead `Reference`'s deferred enqueue pair goes with its
        // row (`drop_pending_enqueues_of`). Collected only when a pair exists.
        let track_pending = !self.pending_queues.is_empty();
        let mut dropped_enqueued: FxHashSet<usize> = FxHashSet::default();
                let skip = self.persistent_skip_bits.clone();
        let mut keep = |e: &ReferenceEntry| -> bool {
            let live = is_live(e.reference_obj);
            if !live {
                                if matches!(e.ref_type, ReferenceType::Weak | ReferenceType::Phantom) {
                    if let Some(s) = &skip {
                        s.persistent_clear(e.reference_obj);
                    }
                }
                if track_pending && e.enqueued {
                    dropped_enqueued.insert(e.reference_obj);
                }
            }
            live
        };
        self.soft_refs.retain(&mut keep);
        self.weak_refs.retain(&mut keep);
        self.phantom_refs.retain(&mut keep);
        self.drop_pending_enqueues_of(dropped_enqueued);
        self.prune_non_reference_rows(is_live);
        // gcg g3/rw: an allocated, unbound `Reference` that died goes too (it
        // is watched, so a survivor is provably live).
        self.allocated_unbound
            .retain(|a| is_live(a.row.reference_obj));
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

    /// Tail of [`Self::remove_collected`] when
    /// [`Self::remove_collected_reference_objects`] already pruned `soft_refs`,
    /// `weak_refs`, `phantom_refs`, and `allocated_unbound` with the same
    /// predicate in this round.
    pub fn remove_collected_non_reference_rows(&mut self, is_live: &dyn Fn(usize) -> bool) {
        if self.prune_non_reference_rows(is_live) {
            self.prune_identity_stamps();
            self.app_row_index_stale = true;
        }
    }

    fn prune_non_reference_rows(&mut self, is_live: &dyn Fn(usize) -> bool) -> bool {
        let cleaner_before = self.cleaner_refs.len();
        let finalizer_before = self.finalizer_refs.len();
        self.cleaner_refs.retain(|e| is_live(e.reference_obj));
        // Wave 23 (item 4): a pruned row's outstanding count goes with it.
        let mut extra_dropped = 0u64;
        self.finalizer_refs.retain(|e| {
            let live = is_live(e.reference_obj);
            if !live {
                extra_dropped += u64::from(e.extra_finalizers);
            }
            live
        });
        self.extra_finalizers_outstanding =
            self.extra_finalizers_outstanding.saturating_sub(extra_dropped);
        // JIT round 21 wave 18: the withdrawn rows live for one collection
        // interval at most (they are not watched, so `is_live` cannot answer
        // for them; see `withdrawn_finalizables`).
        self.withdrawn_finalizables.clear();
        if self.finalizer_refs.len() != finalizer_before {
            // A pruned active row must not keep refusing its address.
            self.finalizer_active_stale = true;
        }
        self.cleaner_refs.len() != cleaner_before || self.finalizer_refs.len() != finalizer_before
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
    /// `reference_obj == referent == the object itself` (the
    /// `java/lang/ref/Finalizer.register` native reaches it through
    /// `register_if_finalizable` since gc-common w5-d; it no longer makes a
    /// `Cleaner` row), and a cleaner row's `reference_obj` is the synthetic
    /// `Cleaner$Cleanable` shape. For those rows "the reference object is dead" IS
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
        // gcg g2/r: see `flush_deferred_sweep`.
        self.flush_deferred_sweep();
        self.retain_reference_rows(&|e: &ReferenceEntry| is_live(e.reference_obj))
    }

    /// Prune collected rows from all reference lists EXCEPT `weak_refs`.
    /// Used when `weak_refs` will be pruned in the same parallel pass as
    /// `process_weak_refs_parallel`.
    pub fn remove_collected_non_weak_reference_objects(
        &mut self,
        is_live: &dyn Fn(usize) -> bool,
    ) -> usize {
        self.flush_deferred_sweep();
        self.absorb_allocation_notes();
        let before = self.total_entry_count();
        let soft_before = self.soft_refs.len();
        let track_pending = !self.pending_queues.is_empty();
        let mut dropped_enqueued: FxHashSet<usize> = FxHashSet::default();
        let keep_row = |e: &ReferenceEntry| is_live(e.reference_obj);
        let active_dropped =
            self.retain_non_weak_reference_rows(&keep_row, &mut dropped_enqueued, track_pending);
        self.finish_retain_reference_rows(before, soft_before, dropped_enqueued);
        active_dropped
    }

    /// Prune collected `weak_refs` rows in parallel using a `Sync` liveness predicate.
    pub fn remove_collected_weak_refs_sync<F: Fn(usize) -> bool + Sync>(
        &mut self,
        is_live: &F,
    ) -> usize {
        let len = self.weak_refs.len();
        if len == 0 {
            return 0;
        }
        let max_workers = if len >= 131_072 { 16 } else { 4 };
        let workers = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(4)
            .clamp(1, max_workers);
        let chunk_size = len.div_ceil(workers);
        let weak_refs = &self.weak_refs;
        let dead_lists: Vec<Vec<usize>> = std::thread::scope(|s| {
            let mut handles = Vec::with_capacity(workers);
            for start in (0..len).step_by(chunk_size) {
                let end = (start + chunk_size).min(len);
                handles.push(s.spawn(move || {
                    let mut dead = Vec::new();
                    for (offset, e) in weak_refs[start..end].iter().enumerate() {
                        if !is_live(e.reference_obj) {
                            dead.push(start + offset);
                        }
                    }
                    dead
                }));
            }
            handles
                .into_iter()
                .map(|h| h.join().expect("weak retain worker"))
                .collect()
        });
        self.prune_weak_ref_indices(dead_lists)
    }

    fn prune_weak_ref_indices(&mut self, dead_lists: Vec<Vec<usize>>) -> usize {
        let total_dead: usize = dead_lists.iter().map(Vec::len).sum();
        if total_dead == 0 {
            return 0;
        }
        let track_pending = !self.pending_queues.is_empty();
        let mut dropped_enqueued: FxHashSet<usize> = FxHashSet::default();
        let mut active_dropped = 0usize;
                let skip = self.persistent_skip_bits.clone();
        let mut dead_iter = dead_lists.into_iter().flatten().peekable();
        let mut idx = 0usize;
        self.weak_refs.retain(|e| {
            let is_dead = dead_iter.peek().copied() == Some(idx);
            if is_dead {
                dead_iter.next();
                if !e.cleared && !e.enqueued {
                    active_dropped += 1;
                }
                                if let Some(s) = &skip {
                    s.persistent_clear(e.reference_obj);
                }
                if track_pending && e.enqueued {
                    dropped_enqueued.insert(e.reference_obj);
                }
            }
            idx += 1;
            !is_dead
        });
        self.drop_pending_enqueues_of(dropped_enqueued);
        self.prune_identity_stamps();
        self.app_row_index_stale = true;
        active_dropped
    }

    /// Parallel-capable variant of [`Self::remove_collected_reference_objects`]
    /// for `Sync` liveness predicates when `weak_refs` is large.
    pub fn remove_collected_reference_objects_sync<F: Fn(usize) -> bool + Sync>(
        &mut self,
        is_live: &F,
    ) -> usize {
        self.flush_deferred_sweep();
        if self.weak_refs.len() < 32_768 {
            return self.retain_reference_rows(&|e: &ReferenceEntry| is_live(e.reference_obj));
        }
        self.absorb_allocation_notes();
        let before = self.total_entry_count();
        let soft_before = self.soft_refs.len();
        let track_pending = !self.pending_queues.is_empty();
        let mut dropped_enqueued: FxHashSet<usize> = FxHashSet::default();
        let mut active_dropped = self.remove_collected_weak_refs_sync(is_live);
        let keep_row = |e: &ReferenceEntry| is_live(e.reference_obj);
        active_dropped += self.retain_non_weak_reference_rows(
            &keep_row,
            &mut dropped_enqueued,
            track_pending,
        );
        self.finish_retain_reference_rows(before, soft_before, dropped_enqueued);
        active_dropped
    }

    /// The sequence number the NEXT discovered row will carry
    /// ([`ReferenceEntry::registration_seq`]). Every row registered so far has
    /// a smaller one.
    ///
    /// gcd d1/c (2026-09-27): read by the generational concurrent sweep before
    /// it frees anything, for [`Self::remove_reference_objects_registered_before`].
    pub fn registration_seq(&self) -> u64 {
        // Wave 143 (L6): the stripes' counter while attached.
        match &self.allocation_notes {
            Some(notes) => notes.next_seq.load(Ordering::Acquire),
            None => self.next_registration_seq,
        }
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

    /// gcg g2/r (`gcd-d3n-proposal-sweep-reference-row-prune-at-pause-boundaries`,
    /// `CRATONVM_GEN_CONC_SWEEP_REFROW_BATCH`) — hold one concurrent-sweep
    /// slice's freed spans instead of pruning their rows now. O(spans): no
    /// row is visited. A batch of an EARLIER sweep (another
    /// `registered_before`) still held is flushed first, so two sweeps'
    /// snapshots never mix. Nothing for an empty slice.
    pub fn defer_swept_spans(&mut self, registered_before: u64, freed_spans: &[(usize, usize)]) {
        if freed_spans.is_empty() {
            return;
        }
        if self
            .deferred_sweep
            .as_ref()
            .is_some_and(|b| b.registered_before != registered_before)
        {
            self.flush_deferred_sweep();
        }
        self.deferred_sweep
            .get_or_insert_with(|| SweptReferenceRowBatch::new(registered_before))
            .add_slice(freed_spans);
    }

    /// gcg g2/r — whether the sweep should flush its held spans now (see
    /// [`SweptReferenceRowBatch::wants_flush`]).
    pub fn deferred_sweep_wants_flush(&self, pause_requested: bool) -> bool {
        self.deferred_sweep
            .as_ref()
            .is_some_and(|b| b.wants_flush(pause_requested))
    }

    /// gcg g2/r — prune the rows of every span [`Self::defer_swept_spans`]
    /// holds, in one walk. Returns the ACTIVE rows dropped; nothing (one
    /// `Option` test) when nothing is held, which is always the case off the
    /// opt-in Generational path.
    ///
    /// Called by the sweep, and ALSO at the top of every pause-time entry
    /// point that reads rows by address before the collection relocates them
    /// ([`Self::weak_phantom_active_triples`], the pre-collection pass's
    /// first call; [`Self::remove_collected_reference_objects`],
    /// [`Self::process_references_with_finalizer_trace`],
    /// [`Self::remove_collected`] and [`Self::update_after_gc`]). That closes
    /// the window the sweep's own `stw_requested` test leaves (a pause
    /// requested between that load and the sweep's `safepoint_check`): the
    /// pause then flushes before it reads a row. Flushing BEFORE
    /// `update_after_gc` relocates anything is what makes it exact: the spans
    /// name pre-collection addresses, and a live row's pre-collection address
    /// is never inside a span the sweep freed.
    pub fn flush_deferred_sweep(&mut self) -> usize {
        let Some(mut batch) = self.deferred_sweep.take() else {
            return 0;
        };
        let n = batch.flush(self);
        self.deferred_sweep_flushes += batch.flushes();
        self.deferred_sweep_rows_dropped += n as u64;
        n
    }

    /// gcg g2/r — `(walks, active rows dropped)` by
    /// [`Self::flush_deferred_sweep`] over the processor's life.
    pub fn deferred_sweep_totals(&self) -> (u64, u64) {
        (
            self.deferred_sweep_flushes,
            self.deferred_sweep_rows_dropped,
        )
    }

    /// Keep the soft / weak / phantom rows `keep_row` accepts; the shared body
    /// of [`Self::remove_collected_reference_objects`] and
    /// [`Self::remove_reference_objects_registered_before`]. Returns the ACTIVE
    /// rows dropped.
    fn retain_reference_rows(&mut self, keep_row: &dyn Fn(&ReferenceEntry) -> bool) -> usize {
        // Wave 143 (L6): every pruner sees the striped notes too.
        self.absorb_allocation_notes();
        let before = self.total_entry_count();
        let soft_before = self.soft_refs.len();
        let mut active_dropped = 0usize;
        // gcd d9/e: the dropped rows that may own a DEFERRED enqueue pair
        // (see `drop_pending_enqueues_of`). Only collected when a pair exists.
        let track_pending = !self.pending_queues.is_empty();
        let mut dropped_enqueued: FxHashSet<usize> = FxHashSet::default();
                let skip = self.persistent_skip_bits.clone();
        self.weak_refs.retain(|e| {
            let live = keep_row(e);
            if !live {
                if !e.cleared && !e.enqueued {
                    active_dropped += 1;
                }
                                if let Some(s) = &skip {
                    s.persistent_clear(e.reference_obj);
                }
                if track_pending && e.enqueued {
                    dropped_enqueued.insert(e.reference_obj);
                }
            }
            live
        });
        active_dropped += self.retain_non_weak_reference_rows(
            keep_row,
            &mut dropped_enqueued,
            track_pending,
        );
        self.finish_retain_reference_rows(before, soft_before, dropped_enqueued);
        active_dropped
    }

    fn retain_non_weak_reference_rows(
        &mut self,
        keep_row: &dyn Fn(&ReferenceEntry) -> bool,
        dropped_enqueued: &mut FxHashSet<usize>,
        track_pending: bool,
    ) -> usize {
        let mut active_dropped = 0usize;
                let skip = self.persistent_skip_bits.clone();
        let mut keep = |e: &ReferenceEntry| -> bool {
            let live = keep_row(e);
            if !live && !e.cleared && !e.enqueued {
                active_dropped += 1;
            }
            if !live {
                                if matches!(e.ref_type, ReferenceType::Weak | ReferenceType::Phantom) {
                    if let Some(s) = &skip {
                        s.persistent_clear(e.reference_obj);
                    }
                }
                if track_pending && e.enqueued {
                    dropped_enqueued.insert(e.reference_obj);
                }
            }
            live
        };
        self.soft_refs.retain(&mut keep);
        self.phantom_refs.retain(&mut keep);
        self.allocated_unbound.retain(|a| keep_row(&a.row));
        self.withdrawn_finalizables.retain(|e| keep_row(e));
        active_dropped
    }

    fn finish_retain_reference_rows(
        &mut self,
        before: usize,
        soft_before: usize,
        dropped_enqueued: FxHashSet<usize>,
    ) {
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
    }

    /// gcd d9/e (2026-09-28) — drop the DEFERRED enqueue pairs
    /// (`pending_queues`) of `Reference` objects whose rows were just dropped
    /// as dead or freed, unless a surviving enqueued row still names the same
    /// address. Returns how many pairs it dropped.
    ///
    /// # Why (`docs/feature-designs/generative-gc-architecture.md`)
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

    /// gci i11/rp (R7) — count one refused referent restore (see
    /// [`Self::restore_refusals`]).
    pub fn note_restore_refused(&mut self) {
        self.restore_refusals = self.restore_refusals.saturating_add(1);
    }

    /// gci i11/rp (R7) — referent restores this VM refused over its life.
    /// Printed beside each refusal.
    pub fn restore_refusals(&self) -> u64 {
        self.restore_refusals
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
        // gcj j1yg-pause round 3: a set-aside row keeps its stamps.
        if let Some(st) = self.settled_old_stash.as_ref() {
            for e in st.weak.iter().chain(st.phantom.iter()) {
                live.insert(e.reference_obj);
            }
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
    /// three holes — `docs/feature-designs/generative-gc-architecture.md`
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
    ///   shape, which is not a `Reference`, so neither native can reach it.
    ///   (`Finalizer.register` used to add a `reference_obj == referent`
    ///   cleaner row here, reachable when the finalizable object is itself a
    ///   `Reference` subclass; since gc-common w5-d it makes a `Finalizer` row
    ///   through `register_if_finalizable`, which this never touches.)
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
                if matches!(list, AppRowList::Weak | AppRowList::Phantom) {
            if let Some(skip) = &self.persistent_skip_bits {
                skip.persistent_clear(reference_obj);
            }
        }
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
            if rows
                .get(idx)
                .is_some_and(|e| e.reference_obj == reference_obj)
            {
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

    /// Withdraw `obj`'s ACTIVE finalizer row (JIT round 21 wave 10, lane
    /// finalize, `CRATONVM_FINALIZER_REGISTER_AT_OBJECT_INIT`): the object was
    /// registered at its allocation, and its constructor provably threw before
    /// `Object.<init>` returned, where HotSpot (`RegisterFinalizersAtInit`)
    /// would have registered it -- so HotSpot never finalizes it. Every active
    /// row at the address goes (at most one, see [`Self::finalizer_active`]);
    /// enqueued or cleared rows are a verdict already given and stay. Returns
    /// whether a row was withdrawn. The caller guarantees `obj` names the
    /// unconstructed object (an address epoch check).
    pub fn withdraw_finalizable(&mut self, obj: usize) -> bool {
        if !self.is_registered_finalizable(obj) {
            return false;
        }
        // JIT round 21 wave 13 (review rev-native item 2 of
        // `r21-review-w13-native-findings-20261019.md`): the row is found from
        // the TAIL. The withdrawn row was pushed by this very construction
        // moments ago, so it is (almost always) the last row; a `retain` over
        // every row made each withdrawal O(live finalizable objects). There is
        // at most one active row per address (`discover_reference` refuses a
        // second one while `finalizer_active` holds the address, and the set
        // is current here), so removing that one row is what the `retain`
        // removed. `remove` keeps the rows' order (registration order).
        let Some(pos) = self
            .finalizer_refs
            .iter()
            .rposition(|e| e.reference_obj == obj && !e.cleared && !e.enqueued)
        else {
            // The set named the address but no active row holds it: the set
            // was wrong; drop the address so it never refuses again.
            self.finalizer_active.remove(&obj);
            return false;
        };
        // JIT round 21 wave 16 (lane finalize-reinit, residual 4R case (b)):
        // an object whose `<init>` already returned was registered where
        // HotSpot registers it; a later `<init>` that throws before its
        // `super(..)` (an unverified class, a JNI re-`<init>`) leaves that
        // registration alone.
        if self.reregister_at_init_return && self.finalizer_refs[pos].constructed {
            return false;
        }
        let row = self.finalizer_refs.remove(pos);
        // Wave 23 (item 4): the row's outstanding count leaves with it.
        self.extra_finalizers_outstanding = self
            .extra_finalizers_outstanding
            .saturating_sub(u64::from(row.extra_finalizers));
        // Wave 16: kept for a retry's `<init>` return (residual 4R case (a)).
        if self.reregister_at_init_return {
            self.remember_withdrawn_finalizable(row);
        }
        debug_assert!(
            !self
                .finalizer_refs
                .iter()
                .any(|e| e.reference_obj == obj && !e.cleared && !e.enqueued),
            "at most one active finalizer row per address"
        );
        // JIT round 21 wave 11 (lane finalize; item 2 of
        // `r21-review-w10-findings-FIXED-20261020.md`): the set is current here (the
        // check above rebuilt it if it was stale), and the removal above took
        // the one active row at `obj`, so dropping the address in place is exact.
        // Marking the set stale instead made the next `new` of a finalizable
        // class rebuild it from every row: quadratic in a loop of throwing
        // constructors.
        self.finalizer_active.remove(&obj);
        true
    }

    /// Keep a row [`Self::withdraw_finalizable`] took in
    /// [`Self::withdrawn_finalizables`]: one entry per address (a newer
    /// withdrawal replaces an older one), the oldest dropped at the cap.
    fn remember_withdrawn_finalizable(&mut self, row: ReferenceEntry) {
        self.withdrawn_finalizables
            .retain(|e| e.reference_obj != row.reference_obj);
        if self.withdrawn_finalizables.len() >= WITHDRAWN_FINALIZABLES_MAX {
            self.withdrawn_finalizables.remove(0);
        }
        self.withdrawn_finalizables.push(row);
    }

    /// A `<init>` whose receiver is the finalizable object `obj` returned
    /// normally, so `obj`'s `Object.<init>` returned (JIT round 21 wave 16,
    /// lane finalize-reinit, `CRATONVM_FINALIZER_REREGISTER_AT_INIT_RETURN`;
    /// residual 4R of `docs/feature-designs/generative-gc-architecture.md`).
    /// HotSpot registers the object there (`RegisterFinalizersAtInit`); this
    /// VM registered it at its allocation, and may since have withdrawn that
    /// registration because an earlier `<init>` attempt threw before its
    /// `super(..)`. Two effects, both idempotent:
    ///
    /// - a withdrawn object ([`Self::withdrawn_finalizables`]) is registered
    ///   again -- one row: a discovery while an active row exists is a no-op
    ///   ([`Self::finalizer_active`]), so an object never gets two Finalizers;
    /// - its active row is marked [`ReferenceEntry::constructed`], which a
    ///   later withdrawal refuses.
    ///
    /// Nothing happens for an object that is neither withdrawn nor registered
    /// (a non-finalizable class never reaches here; an object whose `<init>`
    /// never returns never does either). Returns whether `obj` was
    /// re-registered. The caller guarantees `obj` names the live receiver
    /// (it holds it).
    pub fn note_finalizable_init_returned(&mut self, obj: usize) -> bool {
        if !self.reregister_at_init_return {
            return false;
        }
        let mut reregistered = false;
        if let Some(i) = self
            .withdrawn_finalizables
            .iter()
            .rposition(|e| e.reference_obj == obj)
        {
            let row = self.withdrawn_finalizables.remove(i);
            let before = self.finalizer_refs.len();
            self.discover_reference(ReferenceType::Finalizer, obj, obj, row.queue_addr);
            reregistered = self.finalizer_refs.len() != before;
        }
        if !self.is_registered_finalizable(obj) {
            return reregistered;
        }
        // The active row is (almost always) near the tail: it was pushed by
        // this object's allocation or just above.
        if let Some(e) = self
            .finalizer_refs
            .iter_mut()
            .rev()
            .find(|e| e.reference_obj == obj && !e.cleared && !e.enqueued)
        {
            e.constructed = true;
            // Wave 25 (lane initwitness): the per-row witness the JNI
            // re-`<init>` door compares across its call.
            e.init_returns = e.init_returns.wrapping_add(1);
        }
        reregistered
    }

    /// The `<init>`-return witness of `obj`'s ACTIVE finalizer row
    /// ([`ReferenceEntry::init_returns`]; JIT round 21 wave 25, lane
    /// initwitness, `CRATONVM_JNI_REINIT_INIT_RETURN_WITNESS`). `None` when
    /// `obj` has no active row. The JNI re-`<init>` door reads it before and
    /// after its call: a value that moved means a constructor of `obj`
    /// returned normally during the call, so its `Object.<init>` returned.
    pub fn finalizable_init_returns(&mut self, obj: usize) -> Option<u32> {
        if !self.is_registered_finalizable(obj) {
            return None;
        }
        self.finalizer_refs
            .iter()
            .rev()
            .find(|e| e.reference_obj == obj && !e.cleared && !e.enqueued)
            .map(|e| e.init_returns)
    }

    /// Count one JNI re-`<init>` call of `obj` on its ACTIVE finalizer row
    /// ([`ReferenceEntry::jni_reinit_calls`]; JIT round 22 wave 2, lane
    /// review) and answer the new count, or `None` when `obj` has no active
    /// row.
    pub fn note_jni_reinit_call(&mut self, obj: usize) -> Option<u32> {
        if !self.is_registered_finalizable(obj) {
            return None;
        }
        self.finalizer_refs
            .iter_mut()
            .rev()
            .find(|e| e.reference_obj == obj && !e.cleared && !e.enqueued)
            .map(|e| {
                e.jni_reinit_calls = e.jni_reinit_calls.wrapping_add(1);
                e.jni_reinit_calls
            })
    }

    /// [`ReferenceEntry::jni_reinit_calls`] of `obj`'s ACTIVE finalizer row,
    /// or `None` (JIT round 22 wave 2, lane review).
    pub fn jni_reinit_calls(&mut self, obj: usize) -> Option<u32> {
        if !self.is_registered_finalizable(obj) {
            return None;
        }
        self.finalizer_refs
            .iter()
            .rev()
            .find(|e| e.reference_obj == obj && !e.cleared && !e.enqueued)
            .map(|e| e.jni_reinit_calls)
    }

    /// The latched `CRATONVM_FINALIZER_REREGISTER_AT_INIT_RETURN`.
    pub fn reregister_at_init_return(&self) -> bool {
        self.reregister_at_init_return
    }

    /// [`Self::withdrawn_finalizables`]' length (tests, diagnostics).
    pub fn withdrawn_finalizables_len(&self) -> usize {
        self.withdrawn_finalizables.len()
    }

    /// Override the latched `CRATONVM_FINALIZER_REREGISTER_AT_INIT_RETURN`
    /// (tests; the VM keeps the latched value). Turning it off drops the kept
    /// rows.
    pub fn set_reregister_at_init_return(&mut self, on: bool) {
        self.reregister_at_init_return = on;
        if !on {
            self.withdrawn_finalizables.clear();
        }
    }

    /// Note an unconstructed finalizable object for its native door (see
    /// [`Self::unconstructed_finalizables`]). `epoch` is the VM's address
    /// epoch when the constructor's frames were unwound; entries of any other
    /// epoch are dropped first.
    pub fn note_unconstructed_finalizable(&mut self, obj: usize, epoch: u64) {
        self.unconstructed_finalizables.retain(|&(_, e)| e == epoch);
        if self
            .unconstructed_finalizables
            .iter()
            .any(|&(a, _)| a == obj)
        {
            return;
        }
        if self.unconstructed_finalizables.len() >= UNCONSTRUCTED_FINALIZABLES_MAX {
            self.unconstructed_finalizables.remove(0);
        }
        self.unconstructed_finalizables.push((obj, epoch));
    }

    /// The door half of [`Self::note_unconstructed_finalizable`]: when an
    /// entry for `obj` noted in `epoch` exists, remove it and withdraw `obj`'s
    /// active finalizer row ([`Self::withdraw_finalizable`]). Returns whether
    /// a row was withdrawn.
    pub fn take_unconstructed_finalizable(&mut self, obj: usize, epoch: u64) -> bool {
        let Some(pos) = self
            .unconstructed_finalizables
            .iter()
            .position(|&(a, e)| a == obj && e == epoch)
        else {
            return false;
        };
        self.unconstructed_finalizables.remove(pos);
        self.withdraw_finalizable(obj)
    }

    /// The JNI re-`<init>` door's half of [`Self::note_unconstructed_finalizable`]
    /// (JIT round 21 wave 22, lane finalize,
    /// `CRATONVM_JNI_REINIT_REGISTERS_FINALIZER_AGAIN`): drop the entry for
    /// `obj` noted in `epoch`, withdrawing NOTHING, and say whether it existed
    /// -- whether the unwinder proved that the call's constructor threw before
    /// `Object.<init>` returned (HotSpot then makes no second `Finalizer`).
    pub fn forget_unconstructed_finalizable(&mut self, obj: usize, epoch: u64) -> bool {
        let Some(pos) = self
            .unconstructed_finalizables
            .iter()
            .position(|&(a, e)| a == obj && e == epoch)
        else {
            return false;
        };
        self.unconstructed_finalizables.remove(pos);
        true
    }

    /// A JNI `<init>` call on `obj`, which already had an ACTIVE finalizer
    /// row before the call, reached `Object.<init>`'s return (JIT round 21
    /// wave 22, lane finalize, `CRATONVM_JNI_REINIT_REGISTERS_FINALIZER_AGAIN`;
    /// item 2 of `docs/feature-designs/generative-gc-architecture.md`).
    /// HotSpot registers a second `Finalizer` there, so `finalize()` runs once
    /// more: the active row's [`ReferenceEntry::extra_finalizers`] goes up by
    /// one, and the row is marked [`ReferenceEntry::constructed`] (its
    /// `Object.<init>` returned). Still one row: [`Self::finalizer_active`],
    /// [`Self::withdraw_finalizable`] and the per-object finalizer queue are
    /// unchanged. Returns whether a row was found (none: an object whose row
    /// was enqueued meanwhile, which the caller's own registration covers).
    pub fn note_finalizer_registered_again(&mut self, obj: usize) -> bool {
        if !self.is_registered_finalizable(obj) {
            return false;
        }
        let Some(e) = self
            .finalizer_refs
            .iter_mut()
            .rev()
            .find(|e| e.reference_obj == obj && !e.cleared && !e.enqueued)
        else {
            return false;
        };
        e.constructed = true;
        if e.extra_finalizers < u32::MAX {
            e.extra_finalizers += 1;
            self.extra_finalizers_outstanding += 1;
        }
        true
    }

    /// The finalizer door is about to run `obj.finalize()` (JIT round 21 wave
    /// 22, lane finalize): does the object's row owe another run
    /// ([`ReferenceEntry::extra_finalizers`])? Takes one when it does; the
    /// caller then queues the object again, so the next run happens through
    /// the queue, rooted and relocated like the first. Only the NEWEST
    /// enqueued row at the address is asked, so an older row of a dead object
    /// at a recycled address can never answer for this one. One integer test
    /// while no row carries a count ([`Self::extra_finalizers_outstanding`]).
    pub fn take_extra_finalizer(&mut self, obj: usize) -> bool {
        if self.extra_finalizers_outstanding == 0 {
            return false;
        }
        let Some(e) = self
            .finalizer_refs
            .iter_mut()
            .rev()
            .find(|e| e.referent == obj && e.enqueued)
        else {
            return false;
        };
        if e.extra_finalizers == 0 {
            return false;
        }
        e.extra_finalizers -= 1;
        self.extra_finalizers_outstanding = self.extra_finalizers_outstanding.saturating_sub(1);
        true
    }

    /// [`Self::extra_finalizers_outstanding`] (tests, diagnostics).
    pub fn extra_finalizers_outstanding(&self) -> u64 {
        self.extra_finalizers_outstanding
    }

    /// Return reference_obj addresses of all soft / weak entries whose referent
    /// was cleared, every call (idempotent). Tests and diagnostics only: no
    /// production code reads it (gcj j4/rf) -- the post-GC writer takes
    /// [`Self::take_newly_cleared`], which emits each row once, because
    /// re-nulling a dead `Reference`'s recycled address corrupts its occupant.
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
        // Phantom refs are not listed: JDK 9+ clears a phantom referent as it is
        // enqueued (see the phantom phase); this list (read by tests only) predates that.
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
    /// Every Soft, Weak and Phantom row as `(kind, Reference object, referent,
    /// cleared, enqueued)`, for the G1 remark's per-row trace
    /// (`CRATONVM_DBG_REFPROC_REMARK`, zt t31c). Read-only; diagnostic callers only.
    pub fn reference_rows_for_trace(&self) -> Vec<(ReferenceType, usize, usize, bool, bool)> {
        self.weak_refs
            .iter()
            .chain(self.soft_refs.iter())
            .chain(self.phantom_refs.iter())
            .map(|e| {
                (
                    e.ref_type,
                    e.reference_obj,
                    e.referent,
                    e.cleared,
                    e.enqueued,
                )
            })
            .collect()
    }

    pub fn reference_object_addresses(&self) -> Vec<usize> {
        self.weak_refs
            .iter()
            .chain(self.soft_refs.iter())
            .chain(self.phantom_refs.iter())
            .map(|e| e.reference_obj)
            .collect()
    }

    pub fn soft_reference_object_addresses(&self) -> Vec<usize> {
        self.soft_refs.iter().map(|e| e.reference_obj).collect()
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

    /// zr z1/rf — [`Self::active_reference_objects_with_stamps`] without the
    /// Soft rows: the ACTIVE Weak and Phantom `Reference` objects with their
    /// identity stamps, the ZGC concurrent mark's referent-slot skip set input
    /// (`CRATONVM_ZGC_CONC_REF_SKIP`). Soft rows are left out because ZGC's
    /// mark end has no soft-referent resurrection pass for a hidden slot: the
    /// policy keeps most soft referents, and a kept one whose slot the marker
    /// hid would be swept live. Weak and Phantom referents are nulled by the
    /// pre-collection pass anyway, so hiding them changes no verdict that pass
    /// would not make. (zr z2/cc: the soft rows now have their own accessor,
    /// [`Self::active_soft_objects_with_stamps`], hidden for the concurrent
    /// PHASE only, with the kept referents re-rooted at the mark end.)
    pub fn active_weak_phantom_objects_with_stamps(&self) -> Vec<(usize, i32)> {
        self.weak_refs
            .iter()
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

    /// zr z2/cc — the ACTIVE Soft rows, `(Reference object, identity stamp)`:
    /// the soft half [`Self::active_weak_phantom_objects_with_stamps`] leaves
    /// out. ZGC's concurrent mark hides their referent slots for the
    /// concurrent PHASE only and takes the policy-kept referents as mark-end
    /// roots (`ZgcRealHeap::set_concurrent_vm_soft_reference_skip`).
    pub fn active_soft_objects_with_stamps(&self) -> Vec<(usize, i32)> {
        self.soft_refs
            .iter()
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

    /// Before a mark-exact owner prune, discover Reference objects reached
    /// through policy-kept soft edges, to a fixed point. The mark deliberately
    /// omits referent slots: a nested Reference can be live without being
    /// strongly marked. Dead, disconnected soft rows must not seed this walk.
    pub fn soft_retention_closure(
        &self,
        is_marked: &dyn Fn(usize) -> bool,
        trace: &dyn Fn(&[usize]) -> Vec<usize>,
        free_heap_mb: usize,
        current_time_ms: u64,
    ) -> FxHashSet<usize> {
        let (clock, interval) = if self.hotspot_soft_lru {
            self.hotspot_soft_policy(free_heap_mb)
        } else {
            (
                current_time_ms.max(self.observed_clock_ms()),
                self.soft_ref_lru_policy_ms_per_mb
                    .saturating_mul(free_heap_mb as u64),
            )
        };
        let candidates: Vec<_> = self
            .soft_refs
            .iter()
            .filter(|e| {
                !e.cleared
                    && !e.enqueued
                    && (is_marked(e.referent)
                        || (!self.soft_pre_nulled.contains(&e.reference_obj)
                            && clock.saturating_sub(e.last_access_time_ms) <= interval))
            })
            .map(|e| (e.reference_obj, e.referent))
            .collect();
        let mut admitted = FxHashSet::default();
        let mut closure = FxHashSet::default();
        loop {
            let roots: Vec<_> = candidates
                .iter()
                .filter_map(|&(owner, referent)| {
                    ((is_marked(owner) || closure.contains(&owner)) && admitted.insert(owner))
                        .then_some(referent)
                })
                .collect();
            if roots.is_empty() {
                break;
            }
            closure.extend(roots.iter().copied());
            closure.extend(trace(&roots));
        }
        closure
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

    /// gcj j1yg-pause round 3 (`CRATONVM_GEN_YOUNG_PAUSE_SETS_ASIDE_OLD_ROWS`,
    /// the VM's switch): set aside every ACTIVE weak / phantom row whose
    /// `Reference`, referent and optional queue all lie in `old` (`[lo, hi)`, the old
    /// generation), for one certainly-minor Generational pause. Such a pause
    /// collects no old storage and moves nothing old, and the pre-collection
    /// pass already leaves an old referent in its slot
    /// (`CRATONVM_GEN_YOUNG_PAUSE_KEEPS_OLD_REFERENTS`) and takes the row back
    /// out of its examined set -- so neither pass can change such a row, and
    /// visiting it was the bulk of both passes on a large application (10-17 K
    /// such rows on JIT-warm javac). [`Self::restore_settled_old_rows`] puts
    /// them back, each in its old place. Returns how many were set aside. A
    /// stash left from an earlier pause is restored first.
    pub fn stash_settled_old_rows(&mut self, old: (usize, usize)) -> usize {
        self.stash_settled_rows(&|a| a >= old.0 && a < old.1, 0)
    }

    /// [`Self::stash_settled_old_rows`] for any notion of "old": `in_old` says
    /// whether an address lies in storage the coming pause neither moves nor
    /// frees (G1: the `Old` regions of a plain young pause). `epoch` is the
    /// caller's count of old-collecting pauses, handed back to
    /// [`Self::restore_settled_old_rows_after`].
    pub fn stash_settled_rows(&mut self, in_old: &dyn Fn(usize) -> bool, epoch: u64) -> usize {
        self.restore_settled_old_rows();
        // Held sweep spans name pre-collection addresses (see
        // `bind_allocated_references_pre_gc`): prune first.
        self.flush_deferred_sweep();
        let settled = |e: &ReferenceEntry| {
            !e.enqueued
                && !e.cleared
                && e.referent != 0
                && in_old(e.reference_obj)
                && in_old(e.referent)
                // A young queue can move even though the Reference and its
                // referent cannot. Keep that row in the remapping passes.
                && e.queue_addr.is_none_or(in_old)
        };
        let mut st = SettledOldStash {
            weak_order: self.weak_refs.iter().map(|e| e.registration_seq).collect(),
            phantom_order: self.phantom_refs.iter().map(|e| e.registration_seq).collect(),
            epoch,
            ..Default::default()
        };
        let split = |list: &mut Vec<ReferenceEntry>, out: &mut Vec<ReferenceEntry>| {
            let mut kept = Vec::with_capacity(list.len());
            for e in list.drain(..) {
                if settled(&e) {
                    out.push(e);
                } else {
                    kept.push(e);
                }
            }
            *list = kept;
        };
        split(&mut self.weak_refs, &mut st.weak);
        split(&mut self.phantom_refs, &mut st.phantom);
        let n = st.weak.len() + st.phantom.len();
        if n == 0 {
            return 0;
        }
        self.settled_old_stash = Some(st);
        self.app_row_index_stale = true;
        n
    }

    /// gcj j1yg-pause round 3: undo [`Self::stash_settled_old_rows`]. Each
    /// set-aside row goes back to its position relative to the rows that were
    /// in its list at the stash (by registration sequence); rows the passes
    /// removed stay removed, rows added since stay after every older row, in
    /// their own order. A no-op without a stash.
    pub fn restore_settled_old_rows(&mut self) {
        let Some(st) = self.settled_old_stash.take() else {
            return;
        };
        fn merge(list: &mut Vec<ReferenceEntry>, stashed: Vec<ReferenceEntry>, order: &[u64]) {
            if stashed.is_empty() {
                return;
            }
            // Registration appends monotonically, and the reference passes
            // retain rows in order. In that ordinary case the two lists are
            // already sorted runs: restoring 10-17 K old rows needs a linear
            // merge, without rebuilding a rank hash table and sorting every
            // row. Prove the preconditions; unusual order/duplicate sequence
            // numbers still take the historical stable rank sort below.
            let last = order.last().copied().unwrap_or(0);
            if order.windows(2).all(|w| w[0] < w[1])
                && list.windows(2).all(|w| w[0].registration_seq < w[1].registration_seq)
                && stashed.windows(2).all(|w| w[0].registration_seq < w[1].registration_seq)
                && list.iter().all(|e| {
                    e.registration_seq > last || order.binary_search(&e.registration_seq).is_ok()
                })
            {
                let mut restored = Vec::with_capacity(list.len() + stashed.len());
                let mut active = list.drain(..).peekable();
                let mut old = stashed.into_iter().peekable();
                while let (Some(a), Some(b)) = (active.peek(), old.peek()) {
                    if a.registration_seq <= b.registration_seq {
                        restored.push(active.next().unwrap());
                    } else {
                        restored.push(old.next().unwrap());
                    }
                }
                restored.extend(active);
                restored.extend(old);
                *list = restored;
                return;
            }
            let pos: FxHashMap<u64, usize> =
                order.iter().enumerate().map(|(i, &seq)| (seq, i)).collect();
            let n = order.len();
            let mut keyed: Vec<(usize, ReferenceEntry)> =
                Vec::with_capacity(list.len() + stashed.len());
            for (i, e) in list.drain(..).enumerate() {
                let k = pos.get(&e.registration_seq).copied().unwrap_or(n + i);
                keyed.push((k, e));
            }
            for e in stashed {
                let k = pos.get(&e.registration_seq).copied().unwrap_or(n);
                keyed.push((k, e));
            }
            // Stable: equal keys (a sequence number shared by two rows) keep
            // their relative order.
            keyed.sort_by_key(|&(k, _)| k);
            list.extend(keyed.into_iter().map(|(_, e)| e));
        }
        merge(&mut self.weak_refs, st.weak, &st.weak_order);
        merge(&mut self.phantom_refs, st.phantom, &st.phantom_order);
        self.app_row_index_stale = true;
    }

    /// [`Self::restore_settled_old_rows`] at the end of a pause: when the
    /// caller's old-collecting count is not the one the rows were set aside
    /// under, the pause moved old storage after all (a retry or escalation
    /// inside a pause predicted young-only), so the set-aside rows are
    /// relocated through the pause's `map` first, as `update_after_gc` did
    /// for the rest.
    // shortcut: a row whose Reference that pause freed stays until the next
    // sweep prunes it; upgrade if a reused address is ever seen to inherit one.
    pub fn restore_settled_old_rows_after(
        &mut self,
        epoch: u64,
        map: &cratonvm_types::PointerMap,
    ) {
        if let Some(st) = self.settled_old_stash.as_mut().filter(|st| st.epoch != epoch) {
            relocate_list(&mut st.weak, map);
            relocate_list(&mut st.phantom, map);
        }
        self.restore_settled_old_rows();
    }

    /// gcj j1yg-pause round 3: what the last pre-collection pass set aside
    /// (see [`Self::settled_old_last`]'s field).
    pub fn settled_old_last(&self) -> Option<usize> {
        self.settled_old_last
    }

    /// gcj j1yg-pause round 3: record what this pause's pre-collection pass
    /// set aside (`None`: nothing, the pause was not eligible).
    pub fn note_settled_old_last(&mut self, n: Option<usize>) {
        self.settled_old_last = n;
    }

    /// gcj j1yg-pause round 3: rows currently set aside (0 outside the window).
    pub fn settled_old_stashed(&self) -> usize {
        self.settled_old_stash
            .as_ref()
            .map_or(0, |st| st.weak.len() + st.phantom.len())
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
            let now_ms = current_time_ms.max(self.observed_clock_ms());
            self.soft_clock_pending_ms = self.soft_clock_pending_ms.max(now_ms);
            let (clock, max_interval_ms) = self.hotspot_soft_policy(free_heap_mb);
            return self.condemn(|e| clock.saturating_sub(e.last_access_time_ms) > max_interval_ms);
        }
        let now_ms = current_time_ms.max(self.observed_clock_ms());
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

    /// Is a [`Self::request_clear_all_soft_refs`] pending? Reads without
    /// consuming it (zt t7/cc: the ZGC generational young-cycle arm of the
    /// pre-collection pass must not run a collection that owes a clear as a
    /// young one, and must leave the request to the pass that serves it).
    pub fn clear_all_soft_requested(&self) -> bool {
        self.clear_all_soft_requested
    }

    /// Whether any soft entry is still live enough to be worth a last-ditch
    /// collection — so the escalation ladder can skip one full GC when the
    /// application uses no soft references at all.
    ///
    /// gci i2/oo: a `SoftReference` ALLOCATED since the last collection is not
    /// a row yet -- it waits in `allocated_unbound` for the next
    /// pre-collection pass to bind it (gcg g3/rw) -- and it counts too. The
    /// rung's own collection binds it and then condemns it with the rest;
    /// skipping the rung for it threw `OutOfMemoryError` with that referent
    /// still softly reachable, which `java.lang.ref.SoftReference` forbids.
    pub fn has_active_soft_refs(&self) -> bool {
        self.soft_refs.iter().any(|e| !e.cleared && !e.enqueued)
            || self
                .allocated_unbound
                .iter()
                .any(|p| p.row.ref_type == ReferenceType::Soft)
            || self.striped_soft_note_waits()
    }

    /// Wave 143 (L6): does a striped, not yet absorbed soft note wait?
    fn striped_soft_note_waits(&self) -> bool {
        let mut soft = false;
        if let Some(notes) = &self.allocation_notes {
            notes.for_each(|n| soft |= n.ref_type == ReferenceType::Soft);
        }
        soft
    }

    /// gci i13/oa item 2 — the referent of every active soft row, or `None`
    /// while a soft `Reference` allocated but not yet bound is pending (its
    /// referent is unknown here, so no caller may conclude anything about it).
    pub fn active_soft_referents(&self) -> Option<Vec<usize>> {
        if self
            .allocated_unbound
            .iter()
            .any(|p| p.row.ref_type == ReferenceType::Soft)
            || self.striped_soft_note_waits()
        {
            return None;
        }
        Some(
            self.soft_refs
                .iter()
                .filter(|e| !e.cleared && !e.enqueued)
                .map(|e| e.referent)
                .collect(),
        )
    }

    /// gcg g6/rs — `(reference_obj, last_access_time_ms)` of every active soft
    /// row, for the VM's refresh from the Java `SoftReference.timestamp`
    /// field when the `get()` bridge does not run
    /// (`gcg-g5rs-shadowed-softreference-get-never-touches-the-lru-row`).
    pub fn active_soft_stamps(&self) -> Vec<(usize, u64)> {
        self.soft_refs
            .iter()
            .filter(|e| !e.cleared && !e.enqueued)
            .map(|e| (e.reference_obj, e.last_access_time_ms))
            .collect()
    }

    /// gcg g6/rs — HotSpot's `SoftReference.clock` as this processor keeps it
    /// (the `soft_gc_clock_ms` field): the end of the last processing round,
    /// `0` before the first.
    pub fn soft_gc_clock(&self) -> u64 {
        self.soft_gc_clock_ms
    }

    /// The VM-owned Java master clock exists before the first collection too.
    /// HotSpot initializes it at VM startup; the processor uses that same
    /// per-VM origin for construction stamps before any round has completed.
    pub fn soft_clock_for_java_initialization(&self) -> u64 {
        self.soft_gc_clock_ms.max(self.soft_clock_origin_ms)
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
    ///
    /// gcf f2/r: with [`Self::settled_rows_untracked`] a settled soft / weak /
    /// phantom row contributes its `reference_obj` only (see
    /// [`Self::row_is_settled`] for why no reader needs the other two words).
    /// Besides the identity `pointer_map` entries, the set is also the
    /// true-root major's young seed (`gen_heap`, "every address a WEAK table
    /// hands back"), so a settled row's referent -- by now a dead address or
    /// another object's -- no longer over-retains whatever lies there.
    pub fn all_tracked_addrs(&self) -> Vec<usize> {
        self.tracked_addrs(true)
    }

    /// Addresses this collection's reference passes can examine. A stash
    /// exists only during a certainly-minor Generational pause; every address
    /// in a stashed row (including its queue) is old, cannot move or die, and
    /// is not read by either pass. Publishing those 30-50 K old words to the
    /// young survivor watch set was redundant. Other callers still get the
    /// complete set through `all_tracked_addrs`.
    pub fn active_round_tracked_addrs(&self) -> Vec<usize> {
        self.tracked_addrs(false)
    }

    fn tracked_addrs(&self, include_stash: bool) -> Vec<usize> {
        let n = self.soft_refs.len()
            + self.weak_refs.len()
            + self.phantom_refs.len()
            + self.cleaner_refs.len()
            + self.finalizer_refs.len();
        let mut v = Vec::with_capacity(n * 3);
        let push_row = |v: &mut Vec<usize>, e: &ReferenceEntry| {
            v.push(e.reference_obj);
            v.push(e.referent);
            if let Some(q) = e.queue_addr {
                v.push(q);
            }
        };
        // Only built when the switch is on and a pair is pending (rare: a
        // deferred enqueue, `clear_after_refused_restore`).
        let pending: FxHashSet<usize> = if self.settled_rows_untracked {
            self.pending_queues.values().flatten().copied().collect()
        } else {
            FxHashSet::default()
        };
        for e in self
            .soft_refs
            .iter()
            .chain(self.weak_refs.iter())
            .chain(self.phantom_refs.iter())
        {
            if self.settled_rows_untracked && Self::row_is_settled(e, &pending) {
                v.push(e.reference_obj);
            } else {
                push_row(&mut v, e);
            }
        }
        for e in self.cleaner_refs.iter().chain(self.finalizer_refs.iter()) {
            push_row(&mut v, e);
        }
        // gcj j1yg-pause round 3: the set-aside rows are watched as before.
        if let Some(st) = self.settled_old_stash.as_ref().filter(|_| include_stash) {
            for e in st.weak.iter().chain(st.phantom.iter()) {
                if self.settled_rows_untracked && Self::row_is_settled(e, &pending) {
                    v.push(e.reference_obj);
                } else {
                    push_row(&mut v, e);
                }
            }
        }
        // gcg g3/rw: an allocated, unbound `Reference` is watched too, so a
        // collection that runs without the binding pass proves its survival
        // (`remove_collected` prunes it otherwise).
        v.extend(self.allocated_unbound.iter().map(|a| a.row.reference_obj));
        // Wave 143 (L6): a striped note is watched as its pushed row would be.
        if let Some(notes) = &self.allocation_notes {
            notes.for_each(|n| v.push(n.reference_obj));
        }
        // JIT round 21 wave 18: the withdrawn finalizer rows are deliberately
        // NOT watched. The set seeds the true-root major's young marking, so
        // watching them kept up to 32 unconstructed (garbage) objects, and
        // whatever their constructors stored before `super(..)`, alive past
        // the collection HotSpot frees them in.
        v
    }

    /// gcf f2/r: has a soft / weak / phantom row reached a verdict no later
    /// pause can change, with everything it owes already handed out?
    ///
    /// * weak / soft: `cleared` (the referent is gone for good; the
    ///   null-the-slot write needs only `reference_obj`), and its queue
    ///   delivery decided (`enqueued`, or no queue);
    /// * phantom: `enqueued`, and for a `jdk.internal.ref.Cleaner` its
    ///   `clean()` already emitted (`action_emitted`);
    /// * in both cases its enqueue is not PENDING (`pending`, the
    ///   `reference_obj`s in `pending_queues`: a delivery deferred by
    ///   [`Self::clear_after_refused_restore`] still writes into the queue, so
    ///   that row keeps its queue watched).
    ///
    /// The readers of the watched set that could still ask about a settled
    /// row's referent or queue, checked by reading: every processing phase
    /// skips a settled row before it reads `referent` (`process_weak_refs`
    /// on `cleared`, `process_phantom_refs` on `enqueued`, `process_soft_refs`
    /// on `cleared`/`enqueued`); the soft-survivor roots, the restore pairs and
    /// `pending_reference_object_addresses` take active rows only; the pruners
    /// (`remove_collected`, `retain_shaped_weak_phantom`) and the application
    /// door (`retire_by_application`) key on `reference_obj`, which stays
    /// watched. A settled row's `referent` / `queue_addr` keep being relocated
    /// by [`Self::update_after_gc`] when the map names them and are otherwise
    /// left stale, which nothing reads.
    fn row_is_settled(e: &ReferenceEntry, pending: &FxHashSet<usize>) -> bool {
        let verdict_final = match e.ref_type {
            ReferenceType::Phantom => e.enqueued && (!e.runs_cleaner || e.action_emitted),
            ReferenceType::Soft | ReferenceType::Weak => {
                e.cleared && (e.enqueued || e.queue_addr.is_none())
            }
            _ => false,
        };
        verdict_final && !pending.contains(&e.reference_obj)
    }
}

impl Default for ReferenceProcessor {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// gcg g2/r: the concurrent sweep's reference-row prune, batched per pause
// boundary (`gcd-d3n-proposal-sweep-reference-row-prune-at-pause-boundaries`)
// ---------------------------------------------------------------------------

/// The most freed spans a [`SweptReferenceRowBatch`] holds before it asks to
/// be flushed on its own (the d3n page's memory cap). 16 bytes a span, so the
/// batch never holds more than 1 MiB.
pub const SWEPT_ROW_BATCH_MAX_SPANS: usize = 64 * 1024;

/// gcg g2/r — the freed spans of several generational concurrent-sweep
/// slices, pruned from the [`ReferenceProcessor`] in ONE
/// [`ReferenceProcessor::remove_reference_objects_registered_before`] walk
/// instead of one walk (and one `ref_processor` lock) per slice.
///
/// # Why batching is sound (the d3n page's argument, restated for the code)
///
/// A dead row must be gone before the next PAUSE, not before the next slice:
/// every reader of a row by address that runs between pauses resolves a
/// shared address to the LATER registration (the application retirement
/// index, `rebuild_app_row_index`; the soft touch index, `claim_soft_addr`),
/// and the registration bound (`registered_before`, taken once per sweep)
/// keeps the row of any `Reference` allocated on a freed block since. The
/// readers that do not take the later row (the pre-collection null pass, the
/// post-collection restore / clear / enqueue loops, the skip-set candidates)
/// all run inside a pause. So the sweep flushes:
///
/// * before a between-slice `safepoint_check` that is about to join a
///   requested pause ([`Self::wants_flush`] with `pause_requested`): a pause
///   requested after that check cannot complete without the sweeping
///   (counted) thread's next arrival, which is at the next between-slice
///   check, where this flush runs first -- the argument the address-keyed
///   sweep in the same loop already rests on;
/// * when the batch holds [`SWEPT_ROW_BATCH_MAX_SPANS`] spans;
/// * once after the last slice.
///
/// The spans of different slices are merged at flush time, so overlapping or
/// adjacent reports (a slice whose free-list coalesce absorbed a block an
/// earlier slice freed) are harmless.
///
/// Not used unless the VM opts in (`CRATONVM_GEN_CONC_SWEEP_REFROW_BATCH`, read
/// at the sweep in `gc_and_alloc.rs::maybe_concurrent_gc_at`); the default
/// path keeps the per-slice prune.
#[derive(Debug)]
pub struct SweptReferenceRowBatch {
    /// [`ReferenceProcessor::registration_seq`] at the sweep's start.
    registered_before: u64,
    /// `(start, len)` spans accumulated since the last flush, in arrival
    /// order (sorted and merged by [`Self::flush`]).
    spans: Vec<(usize, usize)>,
    /// Flushes that walked the processor (an empty batch is not one).
    flushes: u64,
    /// ACTIVE rows those flushes dropped, summed.
    rows_dropped: usize,
}

impl SweptReferenceRowBatch {
    /// A batch for one sweep whose rows registered before `registered_before`
    /// (a [`ReferenceProcessor::registration_seq`] value) may be pruned.
    pub fn new(registered_before: u64) -> Self {
        Self {
            registered_before,
            spans: Vec::new(),
            flushes: 0,
            rows_dropped: 0,
        }
    }

    /// Add one slice's freed `(start, len)` spans (any order; empty spans are
    /// ignored).
    pub fn add_slice(&mut self, freed_spans: &[(usize, usize)]) {
        self.spans
            .extend(freed_spans.iter().copied().filter(|&(_, len)| len != 0));
    }

    /// Spans held and not yet pruned.
    pub fn pending_spans(&self) -> usize {
        self.spans.len()
    }

    /// Whether the sweep must flush now: something is held, and either a
    /// pause is about to be joined or the batch is full.
    pub fn wants_flush(&self, pause_requested: bool) -> bool {
        !self.spans.is_empty() && (pause_requested || self.spans.len() >= SWEPT_ROW_BATCH_MAX_SPANS)
    }

    /// Prune the held spans' rows from `ref_proc` in one walk and empty the
    /// batch. Returns the ACTIVE rows dropped. Nothing (no walk, no flush
    /// counted) for an empty batch.
    pub fn flush(&mut self, ref_proc: &mut ReferenceProcessor) -> usize {
        if self.spans.is_empty() {
            return 0;
        }
        coalesce_spans(&mut self.spans);
        let spans = &self.spans;
        let n = ref_proc
            .remove_reference_objects_registered_before(self.registered_before, &|addr: usize| {
                span_contains(spans, addr)
            });
        self.spans.clear();
        self.flushes += 1;
        self.rows_dropped += n;
        n
    }

    /// Flushes that walked the processor so far (the d3n census
    /// `concdrv_sweep_reference_row_prunes` counts these per sweep).
    pub fn flushes(&self) -> u64 {
        self.flushes
    }

    /// ACTIVE rows dropped by this batch's flushes, summed.
    pub fn rows_dropped(&self) -> usize {
        self.rows_dropped
    }
}

/// Sort `spans` by start and merge overlapping or adjacent ones, dropping
/// empty spans. The result is the form [`span_contains`] needs.
fn coalesce_spans(spans: &mut Vec<(usize, usize)>) {
    spans.sort_unstable_by_key(|&(start, _)| start);
    let mut out: Vec<(usize, usize)> = Vec::with_capacity(spans.len());
    for &(start, len) in spans.iter() {
        if len == 0 {
            continue;
        }
        let end = start.saturating_add(len);
        if let Some(last) = out.last_mut() {
            let last_end = last.0.saturating_add(last.1);
            if start <= last_end {
                last.1 = last_end.max(end) - last.0;
                continue;
            }
        }
        out.push((start, len));
    }
    *spans = out;
}

/// `addr` lies in one of `spans` (`(start, len)`, sorted by start,
/// non-overlapping) -- the gc-crate twin of the VM's `addr_keyed::in_spans`.
pub fn span_contains(spans: &[(usize, usize)], addr: usize) -> bool {
    let i = spans.partition_point(|&(start, _)| start <= addr);
    i > 0 && addr - spans[i - 1].0 < spans[i - 1].1
}

// ---------------------------------------------------------------------------
// CleanerThread
// ---------------------------------------------------------------------------

/// Manages pending `Cleaner` actions that need to be invoked, and the
/// `ReferenceQueue`s whose waiters the collector owes a wake-up.
/// Also roots pending References owned by the genuine JDK Reference Handler;
/// those have a separate readiness query from the VM's cleaner delivery work.
pub struct CleanerThread {
    pending_actions: Mutex<VecDeque<usize>>,
    /// Reference/queue pairs awaiting Java enqueue under ReferenceQueue.lock.
    /// A pause cannot mutate the queue: its lock owner may be stopped midway
    /// through poll0 and overwrite a collector-installed head when resumed.
    pending_reference_enqueues: Mutex<VecDeque<(usize, usize)>>,
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
            pending_reference_enqueues: Mutex::new(VecDeque::new()),
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

    pub fn submit_reference_enqueues(&self, pairs: &[(usize, usize)]) {
        self.pending_reference_enqueues.lock().extend(pairs.iter().copied());
    }

    pub fn drain_reference_enqueues(&self) -> Vec<(usize, usize)> {
        self.pending_reference_enqueues.lock().drain(..).collect()
    }

    pub fn has_reference_enqueues(&self) -> bool {
        !self.pending_reference_enqueues.lock().is_empty()
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
        // gcg g3/rw: a `0` target is skipped, as `ReferenceProcessor::update_after_gc`
        // skips one: rewriting an action to address 0 would make the drain
        // hand `run_cleaner_actions` a null cleanable.
        {
            let mut lock = self.pending_actions.lock();
            for addr in lock.iter_mut() {
                if let Some(&new) = pointer_map.get(addr) {
                    if new != 0 {
                        *addr = new;
                    }
                }
            }
        }
        {
            let mut pairs = self.pending_reference_enqueues.lock();
            for (reference, queue) in pairs.iter_mut() {
                for addr in [reference, queue] {
                    if let Some(&new) = pointer_map.get(addr) {
                        if new != 0 {
                            *addr = new;
                        }
                    }
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
                    if new != 0 {
                        *addr = new;
                    }
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
    /// `docs/feature-designs/generative-gc-architecture.md`.
    ///
    /// gc-common w3-d: also the `ReferenceQueue`s still owed a wake-up
    /// ([`Self::note_queues_to_notify`]) — raw addresses for the same reason,
    /// and rooted through the same call, so no `roots.rs` change was needed.
    /// A queue kept alive this way is one a `Reference` was just linked into;
    /// it is released by the drain that notifies it.
    pub fn pending_addresses(&self) -> Vec<usize> {
        let mut addrs: Vec<usize> = self.pending_actions.lock().iter().copied().collect();
        addrs.extend(self.queues_to_notify.lock().iter().copied());
        for &(reference, queue) in self.pending_reference_enqueues.lock().iter() {
            addrs.extend([reference, queue]);
        }
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
    /// zt t31c: queued entries no `pop` hands out yet (always a subset of
    /// `present`); see [`FinalizerThread::hold_until_judged`].
    held: FxHashSet<usize>,
    /// A non-empty hold was released since the last
    /// [`FinalizerThread::take_hold_released`].
    hold_released: bool,
}

impl FinalizerQueue {
    pub fn contains(&self, addr: usize) -> bool {
        self.present.contains(&addr)
    }

    fn push(&mut self, addr: usize) {
        self.order.push_back(addr);
        self.present.insert(addr);
    }

    /// The oldest entry not held back ([`FinalizerThread::hold_until_judged`]).
    fn pop(&mut self) -> Option<usize> {
        let at = if self.held.is_empty() {
            0
        } else {
            self.order.iter().position(|a| !self.held.contains(a))?
        };
        let addr = self.order.remove(at)?;
        self.present.remove(&addr);
        Some(addr)
    }

    pub fn len(&self) -> usize {
        self.order.len()
    }

    /// Entries a `pop` can hand out now.
    fn deliverable_len(&self) -> usize {
        self.order.len() - self.held.len()
    }

    fn hold(&mut self, addrs: &[usize]) {
        self.release();
        self.held
            .extend(addrs.iter().copied().filter(|a| self.present.contains(a)));
    }

    fn release(&mut self) {
        if !self.held.is_empty() {
            self.held.clear();
            self.hold_released = true;
        }
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
        let held: Vec<usize> = self.held.drain().map(&relocate).collect();
        self.held
            .extend(held.into_iter().filter(|a| self.present.contains(a)));
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
    /// zt t15/x12 (`CRATONVM_ZGC_SYSTEM_GC_WAITS_FINALIZER_STEP_ONLY`): the
    /// batch in flight is running the `finalize()` methods it dequeued
    /// ([`DeliverySignal::begin_finalizer_step`]).
    finalizer_step: bool,
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

    /// zt t15/x12 (`CRATONVM_ZGC_SYSTEM_GC_WAITS_FINALIZER_STEP_ONLY`):
    /// delivery-thread side, set before the batch dequeues its first
    /// finalizable object and cleared (with a notify) once the last one it
    /// took has run. A `System.gc()` caller whose inline drain emptied the
    /// queue can only be missing a `finalize()` the batch took while this is
    /// set, so it waits for this step, not for the whole batch: the batch's
    /// GC-notification step, which another thread's collections can keep
    /// feeding, is no part of "finalizers have run when `System.gc()`
    /// returns" (zt13x10 `late=8`, every `System.gc()` held for the 2-s
    /// finalizer timeout while a churn thread kept the batch delivering).
    pub fn begin_finalizer_step(&self) {
        self.state.lock().finalizer_step = true;
    }

    /// zt t15/x12: the end of [`Self::begin_finalizer_step`]'s step.
    pub fn end_finalizer_step(&self) {
        {
            self.state.lock().finalizer_step = false;
        }
        self.changed.notify_all();
    }

    /// zt t15/x12: whether the batch in flight is running finalizers
    /// ([`Self::begin_finalizer_step`]).
    pub fn finalizer_step_running(&self) -> bool {
        self.state.lock().finalizer_step
    }

    /// zt t15/x12: requester side, wait until no finalizer step runs, for at
    /// most `timeout`. `true` when none runs.
    pub fn wait_for_finalizer_step(&self, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        let mut s = self.state.lock();
        while s.finalizer_step {
            if self.changed.wait_until(&mut s, deadline).timed_out() {
                return !s.finalizer_step;
            }
        }
        true
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
/// Finalize-at-most-once (JLS §12.6) is enforced per processor ROW, not here:
/// a finalizer row is enqueued once and never re-armed (HotSpot's equivalent
/// is the `Finalizer` leaving `unfinalized` for good). gcj j4/rf
/// (`gcj-j1rf-reference-processor-review-residuals` item 4): the legacy
/// address-keyed door (`enqueue`, `was_finalized`, the `already_finalized`
/// set and its `cleanup_collected` prune) had no production caller since
/// gci i11/rp and is gone; an ADDRESS gate is unsound anyway (it refused a
/// new object at a recycled address, gc-common w1-d).
pub struct FinalizerThread {
    finalization_queue: Mutex<FinalizerQueue>,
    running: AtomicBool,
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
    /// full hand-off and `--real-jdk` deliver every batch's notifications
    /// anyway); it keeps a batch from taking over the notifications a
    /// lock-free door is about to deliver inline, so that door's output is
    /// unchanged. See `gc_notifications_go_to_delivery_thread`
    /// (`vm/src/runtime/interpreter/gc_and_alloc.rs`).
    gc_notifications_handed_off: AtomicBool,
    /// JIT round 21 wave 24, lane finalize (item 8 of
    /// `docs/feature-designs/generative-gc-architecture.md`): how many
    /// times the finalizer door queued an object AGAIN for the extra
    /// `finalize()` a JNI re-`<init>` owes it, and the queue accepted it
    /// ([`Self::note_extra_run_requeued`]). A refused re-queue is not
    /// counted, so a test can see the second run was really queued.
    extra_runs_requeued: AtomicU64,
}

mod finalizer_thread;

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests;

/// DBG (CRATONVM_DBG_DM) -- see `gc_and_alloc::dm_dbg_enabled`. Cached gate.
fn dm_dbg_enabled() -> bool {
    use std::sync::OnceLock;
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| false)}

/// `jdk.internal.ref.Cleaner`s discovered as run-instead-of-enqueue phantoms.
static DM_PC_DISCOVERED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
/// Cleaner actions emitted by reference processing.
static DM_PC_EMITTED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
/// Reference-processing rounds, for the periodic tally.
static DM_REFPROC_ROUNDS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

// ===========================================================================
// gcg g2/r tests (`cargo test -p cratonvm-gc --lib gcg_g2r_`)
// ===========================================================================

#[cfg(test)]
mod gcg_g2r_tests;

#[cfg(test)]
mod gcg_g3rw_tests;

#[cfg(test)]
mod i61_nested_soft_retention_tests;

/// zr z5/rv5: the soft-phase-only forced-dead set
/// ([`ReferenceProcessor::note_soft_phase_forced_dead`]).
#[cfg(test)]
mod zr5_rv5_soft_phase_tests;

/// gci i11/rp (`docs/feature-designs/generative-gc-architecture.md`):
/// the FinalReference census (R1) and `CRATONVM_SOFTREF_STAMP_GC_CLOCK` (R2).
#[cfg(test)]
mod gci_i11rp_tests;

/// gci i14/gq (`gci-i14gq-g1-reference-processing-and-finalization-review-findings-FIXED-20261016`
/// G-1): a round whose caller did not resurrect the dead finalizables first
/// (G1's final remark) clears soft and weak references before finalization when
/// noted; finalization and the phantom phase are unchanged; one round only.
#[cfg(test)]
mod gci_i14gq_tests;

#[cfg(test)]
mod gci_i15rf_tests;

#[cfg(test)]
mod gci_i16fr_tests;

/// gci i27/gr (`gci-i15rf-g1-remark-loses-a-reference-only-a-dead-finalizable-reaches-RETIRED-UNFIXED-20261018`
/// residual): the stop-the-world probe's undiscovered `Reference` objects are
/// carried past their round for the remark of the same full collection.
/// `cargo test -j 5 -p cratonvm-gc --lib gci_i27_gr_`.
#[cfg(test)]
mod gci_i27_gr_carry_tests;

/// gci i31/gq (G-2): the door's carry to the stop-the-world full mark and the
/// remark callback's staged set. `cargo test -j 5 -p cratonvm-gc --lib gci_i31_gq_`.
#[cfg(test)]
mod gci_i31_gq_carry_tests;

/// gci i48/rf48 (`gci-i15rf-...-20261014` residual): the G1 full collection's
/// undiscovered set, staged by the remark, armed by the door, taken by the
/// compaction pause. `cargo test -j 5 -p cratonvm-gc --lib gci_i48_rf48_`.
#[cfg(test)]
mod gci_i48_rf48_full_gc_undiscovered_tests;

#[cfg(test)]
mod r21_finalize_withdraw_tests;

#[cfg(test)]
mod r21_finalize_reregister_tests;

/// Interpreter round i7 wave 143, lane L6 (`CRATONVM_REF_NOTES_PER_THREAD`):
/// striped allocation notes are seen by every reader of the unbound list
/// exactly as locked pushes are.
#[cfg(test)]
mod i143_l6_allocation_note_stripes_tests;
