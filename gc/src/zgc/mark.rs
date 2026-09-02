// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! ZGC concurrent marking engine — striped work queues, work stealing, a
//! real termination handshake, and safepoint-yielding mark workers.
//!
//! This is the *marking half* of the collector that
//! [`ZgcRealHeap::collect_garbage`](crate::zgc::ZgcRealHeap) currently does
//! stop-the-world, single-threaded, with a plain `Vec<usize>` work stack
//! (`gc/src/zgc.rs`, the `while let Some(addr) = work.pop()` loop). Nothing
//! here is wired into that path yet: this module owns the engine, and the
//! heap will be moved onto it once the sibling ZGC modules (pages, load
//! barrier, forwarding) land.
//!
//! # What this module deliberately does not own
//!
//! * **The colored-pointer encoding.** That is
//!   [`crate::zgc::vaddr`]. This module never reads or writes a raw
//!   [`ZColoredWord`](crate::zgc::vaddr::ZColoredWord); it traffics in
//!   *unmasked machine addresses*. The translation is the
//!   [`ZMarkContext`]'s job, because the load barrier and the page allocator
//!   are the modules that know how a slot is encoded.
//! * **Pages, forwarding tables, remembered sets, metrics.** Those are
//!   sibling modules being written in parallel. The only coupling this
//!   module has to them is the [`ZMarkContext`] trait, which they will
//!   implement (or a heap-level type will implement on their behalf).
//! * **Reference processing.** `gc/src/reference.rs` already contains the
//!   canonical [`ReferenceProcessor`](crate::reference::ReferenceProcessor)
//!   used by all three collectors. This module provides the *phase hook*
//!   ([`ZMarkCoordinator::process_non_strong_refs`]) and nothing else. See
//!   "Weak / soft / phantom references" below — getting this wrong silently
//!   breaks `WeakReference` and `Cleaner`, which this tree has been bitten
//!   by before (`ZgcRealHeap::collect_garbage`'s `INT-8` comments are the
//!   scar tissue).
//!
//! # Address domain: this module is MACHINE ADDRESSES, the barrier is OFFSETS
//! (reconciled 2026-08-07, second pass)
//!
//! *This section exists because it was missing, and the omission was rated the
//! most dangerous item of a cross-module audit. Until this pass the file
//! contained no occurrence of "heap offset", `Z_OFFSET_MASK`, "offset domain"
//! or `barrier.rs` at all.*
//!
//! Every `u64` this module handles — [`ZMarkContext::try_mark`],
//! [`ZMarkContext::is_marked`], [`ZMarkContext::visit_refs`],
//! [`ZMarkContext::is_in_heap`], everything in a stripe, an ingress bucket or a
//! local stack, and the `addr` argument of [`ZMarkHandle::mark_live`] — is an
//! **unmasked machine address of an object base**. It is dereferenced: the
//! production [`ZMarkContext`] (`ZgcRealHeap`, `gc/src/zgc.rs`) does
//! `self.header_ref(addr as usize as *mut u8)` inside `try_mark`.
//!
//! `crate::zgc::barrier` is the opposite, and structurally so: a colored word is
//! `Z_COLORED_TAG | colour | (offset & Z_OFFSET_MASK)`, the metadata field
//! starts at bit 42, so the barrier's `address_mask` is a **42-bit heap offset**
//! and could not be widened into a machine pointer without corrupting the
//! colour. `crate::zgc::relocate` reconciled itself with that on the same day by
//! adding explicitly `_offset`-suffixed entry points
//! (`ZRelocate::forward_offset`, `ZRelocate::forward_lookup_offset`) beside its
//! absolute ones.
//!
//! **This module was left out of that reconciliation, and the resulting bug is
//! silent.** `barrier.rs`'s slow path calls `ZBarrierContext::mark_live` with a
//! bare offset; an implementation that forwarded that straight to
//! [`ZMarkHandle::mark_live`] would hand an offset to a machine-address API.
//! [`ZMarkContext::is_in_heap`] is a *registry membership* test for a real heap,
//! so a 42-bit offset is not a registered object base: the mark is refused,
//! counted in [`ZMarkStats::off_heap_children`], and **discarded**. No crash, no
//! assertion — the object the barrier meant to keep alive is simply swept, and
//! the only trace is a wild-pointer counter that looks like the collector doing
//! its job. On Windows, where the reservation is placed low, an offset and an
//! address are even numerically similar.
//!
//! ## The boundary, and which side converts
//!
//! **The conversion happens here, on the receiving side, in named entry
//! points** — [`ZMarkHandle::mark_live_offset`] and
//! [`ZMarkHandle::mark_live_buffered_offset`] — matching the spelling
//! `relocate.rs` already chose, for the same three reasons:
//!
//! 1. the barrier *cannot* convert: it has no heap base and, by the argument
//!    above, no room in its encoding for a machine address;
//! 2. the correct call is then the one whose **name states the domain**, so a
//!    barrier adapter that reaches for plain `mark_live` is a visibly different
//!    call rather than a silently different meaning;
//! 3. the conversion is written **once**, next to the guard, instead of being
//!    re-derived (and re-derived wrongly) at each adapter.
//!
//! The base comes from [`ZMarkContext::heap_base`], a defaulted trait method
//! returning `Option<u64>`. It defaults to `None` — "this context does not
//! participate in the barrier's offset domain" — and the `_offset` entry points
//! **refuse loudly** for such a context rather than guessing a base. Guessing is
//! the failure this whole section is about.
//!
//! Both entry points check the incoming value with the *same* predicate the
//! barrier uses (`barrier::is_bare_offset` against
//! [`Z_OFFSET_MASK`](crate::zgc::vaddr::Z_OFFSET_MASK)) so the two modules
//! cannot drift on what "an offset" means, and they check it in **release
//! builds as well as debug** (`tracing::error!` plus `debug_assert!`, never a
//! bare `debug_assert!`): on Linux a wrong-domain value is `0x7f…`, five bits
//! past the offset field; on Windows it is plausible and invisible. A
//! debug-only check would run only on the platform where the bug cannot be
//! seen. Refusals are counted in [`ZMarkStats::domain_refusals`], which is
//! deliberately *separate* from `off_heap_children` so "the barrier is wired to
//! the wrong entry point" cannot hide inside "a mutator raced a store".
//!
//! # Why ZGC marking is not SATB marking, and why that changes termination
//!
//! `gc/src/satb.rs` + `gc/src/concurrent_mark.rs` implement
//! **snapshot-at-the-beginning** marking for the generational collector, and
//! `gc/src/g1_concurrent.rs` does the same for G1. SATB's defining property
//! is that **the live set is fixed at the initial-mark pause**:
//!
//! * A *pre*-write barrier logs the **old** value of any overwritten
//!   reference. That is the snapshot being preserved — anything reachable at
//!   the pause stays reachable *to the marker*, even if the mutator
//!   subsequently unlinks it.
//! * Consequently the marker's work is drawn from a **bounded, monotonically
//!   shrinking** source: the initial roots, plus a SATB log whose contents
//!   were all reachable at the snapshot instant.
//! * Termination is therefore easy and is exactly what
//!   [`ConcurrentMarkController`](crate::g1_concurrent::ConcurrentMarkController)
//!   does: drain to a fixed point, then take a **remark** pause, drain the
//!   residual SATB buffers, and you are done. One pause suffices, because
//!   the pause quiesces the only producer.
//!
//! **ZGC has no SATB barrier and no write barrier at all.** It marks on
//! *read*, in the **load barrier**: a mutator that loads a reference whose
//! color is not the current good color takes the slow path, and the slow
//! path marks the target and hands it to the marker. Two consequences, both
//! of which land squarely on the termination protocol:
//!
//! 1. **The producer set is unbounded and includes every mutator thread.**
//!    In SATB, only the marker threads push work once the log is drained. In
//!    ZGC, any mutator can push at any instant, purely by executing a `getfield`
//!    on a reference it has not touched this cycle. "All work queues are
//!    empty" is therefore **not** a completion condition; it is a *fixed
//!    point with respect to what has been published so far*, and the very
//!    next instruction any thread executes can invalidate it.
//! 2. **The mark set grows during the cycle.** SATB's set is a snapshot;
//!    ZGC's is the transitive closure of "everything any thread has looked
//!    at since mark start, plus everything reachable from it". Objects
//!    allocated mid-cycle are implicitly live (see
//!    [`ZGoodMask::allocation_color`](crate::zgc::vaddr::ZGoodMask::allocation_color)),
//!    so they do not need marking, but a *pre-existing* object a mutator
//!    first touches at the very end of the cycle absolutely does.
//!
//! The protocol that follows from this, and that this module implements, is
//! OpenJDK's: **concurrent marking terminates at a fixed point, and the
//! mark-end safepoint decides whether that fixed point is final.** At the
//! safepoint every mutator is stopped and every mutator mark buffer is
//! flushed; if that flush produces any work, the answer is
//! [`ZMarkEndResult::Restart`] and the whole concurrent phase runs again.
//! Only an empty flush yields [`ZMarkEndResult::Complete`]. See
//! [`ZMarkCoordinator::mark_to_completion`].
//!
//! # Module layout
//!
//! | Type | Role |
//! |---|---|
//! | [`ZMarkContext`] | the decoupling boundary — heap shape, mark bits, good mask |
//! | [`ZMarkLocalStack`] | per-worker, lock-free-by-construction work stack |
//! | [`ZMarkStripeSet`] | shared striped overflow queues; publish + steal |
//! | [`ZMarkIngress`] | mutator (load barrier) hand-off, off the steal path |
//! | [`ZMarkTerminator`] | the termination handshake |
//! | [`ZMarkPauseControl`] | safepoint yield/resume for the drain loop |
//! | [`ZMarkWorker`] | one mark thread's state machine |
//! | [`ZMarkCoordinator`] | the pool, the phases, the public API |
//!
//! # Locking discipline (read this before adding a lock)
//!
//! There are three lock families here and exactly one legal ordering:
//!
//! ```text
//!   ZMarkTerminator::state   >   ZMarkStripeSet::stripes[i]
//!                            >   ZMarkIngress::buckets[j]
//! ```
//!
//! * The **termination probe** is the only place that holds the terminator
//!   state lock while acquiring a stripe or ingress lock. It must, because
//!   "the queues are empty" and "the cycle is terminated" have to be decided
//!   atomically with respect to each other.
//! * Every **publisher** (worker overflow publish, mutator flush) therefore
//!   acquires the stripe/ingress lock, *releases it*, and only then calls
//!   [`ZMarkTerminator::note_work_published`], which takes the state lock.
//!   Holding a stripe lock across that call is an ABBA deadlock and is the
//!   single easiest way to reintroduce the class of GC hang this tree has a
//!   documented history of.
//! * Stripe and ingress locks are never held simultaneously.
//!
//! # No process-global state
//!
//! Everything here is instance-owned and reachable only from a
//! [`ZMarkCoordinator`]. There is no `static`, no `OnceLock`, no
//! `thread_local!`. That is a hard requirement in this tree: process-global
//! GC caches have already caused parallel-test crashes, and the test harness
//! routinely hosts more than one heap per process. Even the work-stealing
//! PRNG is seeded from the coordinator's own allocation address rather than
//! from a shared counter.
//!
//! # Thread death, and why [`ZMarkMutatorBuffer`] owns a link back
//!
//! *Added 2026-08-07 after a protocol audit; see [`ZMarkMutatorBuffer`] for
//! the full argument and [`ZMarkHandle::new_buffer`] for the API.*
//!
//! [`ZMarkHandle::mark_live_buffered`] sets the mark bit (via
//! [`ZMarkContext::try_mark`]) **before** the address reaches [`ZMarkIngress`].
//! Between those two steps the object is *marked but unscanned*: no queue holds
//! it, so no worker will ever call `visit_refs` on it, and — because the mark
//! bit is set — no later `try_mark` will re-discover it either. Everything
//! reachable **only** through it is then swept while live. That is precisely
//! the use-after-free the termination protocol exists to prevent, arriving by
//! the back door, and it becomes permanent the moment the owning mutator thread
//! exits with a non-empty buffer.
//!
//! `crate::satb` solved the identical problem for the SATB write barrier with a
//! process-global registry of live per-thread buffers plus orphan-parking of a
//! dying thread's bucket (`flush_thread_satb_buffer` and the registry around
//! it). **That shape is not available here**: it is built out of a `static`
//! registry and a `thread_local!`, both of which the section above forbids —
//! and forbids for a reason that applies directly, since a buffer keyed to the
//! *thread* rather than to the *heap* is the cross-heap contamination bug this
//! module is written to avoid.
//!
//! `crate::zgc::barrier` reproduced SATB's registry shape for its **own**
//! `ZMarkQueue` (`Z_MARK_BUFFER_REGISTRY` / `Z_ORPHANED_MARK_BUFFERS`), and it
//! does cover thread death — **for that queue**. It does not cover this one:
//! the two are disjoint mechanisms that never meet, a `ZMarkMutatorBuffer` is
//! never registered anywhere, and `flush_all_thread_mark_buffers` has no handle
//! on one. Do not read the barrier's registry as covering this type.
//!
//! What this module does instead is **strictly stronger and needs no registry**:
//! an attached [`ZMarkMutatorBuffer`] holds a `Weak<ZMarkShared>` and flushes
//! itself into the ingress from its own `Drop`. Thread exit runs `Drop`;
//! there is nothing to enumerate, nothing to prune, and nothing process-global.
//! Orphan-parking exists in SATB only because a `thread_local!`'s contents have
//! nowhere to go at teardown; a buffer that knows its own pool has somewhere to
//! go.

use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Weak};
use std::thread::JoinHandle;
use std::time::Duration;

use parking_lot::{Condvar, Mutex, RwLock};

use crate::zgc::barrier::is_bare_offset;
use crate::zgc::vaddr::{ZColor, ZGoodMask, Z_MARKED0, Z_MARKED1, Z_OFFSET_MASK};

// ---------------------------------------------------------------------------
// Tunables
// ---------------------------------------------------------------------------

/// Local-stack high-water mark. Above this a worker publishes half of its
/// stack to its own stripe so idle workers have something to steal.
///
/// 256 × 8 bytes = 2 KiB, i.e. the private frontier stays inside L1 on every
/// target this VM runs on. Larger values reduce publish frequency (and thus
/// terminator-lock traffic) but delay the point at which a deep DFS becomes
/// stealable, which is exactly the load-imbalance case striping exists for.
pub const Z_MARK_LOCAL_HIGH_WATER: usize = 256;

/// Minimum local depth at which a worker will *eagerly* share, even below
/// [`Z_MARK_LOCAL_HIGH_WATER`], when its own stripe is empty.
///
/// Without this, a single worker that inherits the whole root set runs a
/// private DFS for a long time before crossing the high-water mark, and the
/// other N-1 workers spin on empty stripes for that entire window. 32 is
/// small enough to seed the stripes almost immediately and large enough that
/// a shallow graph never pays a publish at all.
pub const Z_MARK_LOCAL_SHARE_MIN: usize = 32;

/// Stripes per worker. See [`stripe_count_for`] for the justification.
pub const Z_MARK_STRIPE_OVERSUBSCRIPTION: usize = 4;

/// Floor on the stripe count.
pub const Z_MARK_MIN_STRIPES: usize = 8;

/// Ceiling on the stripe count. The termination probe is `O(stripes)` under
/// the terminator lock, so an unbounded stripe count would make the handshake
/// — the coldest but most latency-sensitive operation here — expensive.
pub const Z_MARK_MAX_STRIPES: usize = 128;

/// Random victim probes an idle worker makes before falling back to a
/// deterministic full sweep of every stripe.
pub const Z_MARK_STEAL_ATTEMPTS: usize = 4;

/// Upper bound on how many entries a single steal moves, so one steal cannot
/// swing the whole imbalance the other way.
pub const Z_MARK_STEAL_CAP: usize = 512;

/// Objects a worker scans per `drain` call before returning to its loop head
/// (where stop and pause are re-checked unconditionally).
pub const Z_MARK_DRAIN_BUDGET: usize = 1024;

/// How often, in scanned objects, the drain loop tests the safepoint yield
/// flag. See [`ZMarkPauseControl`] for the latency this implies.
pub const Z_MARK_YIELD_CHECK_INTERVAL: usize = 64;

/// Independent mutator hand-off buckets. Matches `satb.rs`'s `SHARDS = 16`
/// for the same reason: it is enough to decorrelate a realistic mutator
/// thread count without making the drain-side scan expensive.
pub const Z_MARK_INGRESS_BUCKETS: usize = 16;

/// Entries a per-mutator buffer holds before the owner must flush it.
/// Mirrors `satb.rs`'s per-thread spill threshold.
pub const Z_MARK_MUTATOR_BUFFER_CAPACITY: usize = 256;

/// Bounded park interval. Every wait in this module is a `wait_for`, never a
/// bare `wait`: a lost notification then costs one poll interval instead of
/// a hang. This tree diagnoses GC hangs often enough that "cannot hang even
/// if the signalling is wrong" is worth 200 wakeups a second per idle worker.
const Z_MARK_PARK_POLL_MS: u64 = 5;

/// How many `yield_now` spins the coordinator performs while waiting for
/// workers to reach a yield point before it logs a diagnostic. Purely a
/// watchdog log — it never gives up, because giving up would mean resuming
/// mutators while a marker is still walking objects.
const Z_MARK_QUIESCE_WARN_SPINS: u64 = 1_000_000;

/// How many stripes to use for `n_workers` mark threads.
///
/// * **At least one stripe per worker**, so a worker's own publishes do not
///   contend with another worker's.
/// * **4× oversubscription** ([`Z_MARK_STRIPE_OVERSUBSCRIPTION`]): with
///   random victim selection over `S` stripes and `W` idle thieves, the
///   expected number of thieves colliding on one victim in a probe round is
///   `W/S`. At `S = 4W` that is 0.25 — collisions become the exception, so a
///   steal is one uncontended mutex acquisition.
/// * **Power of two**, so stripe selection is `key & (S-1)` rather than a
///   division. Both bounds are themselves powers of two so the clamp cannot
///   break that.
/// * **Clamped to `[8, 128]`**: the floor keeps even a single-worker
///   configuration spreading its ingress flushes (which helps when the
///   worker count is later raised without re-seeding), and the ceiling
///   bounds the termination probe.
pub fn stripe_count_for(n_workers: usize) -> usize {
    let want = n_workers
        .max(1)
        .saturating_mul(Z_MARK_STRIPE_OVERSUBSCRIPTION)
        .next_power_of_two();
    want.clamp(Z_MARK_MIN_STRIPES, Z_MARK_MAX_STRIPES)
}

/// Which [`ZColor`] a good mask corresponds to during a mark phase, or
/// `None` when the mask is not a mark color (i.e. the collector is in
/// `Relocate`, whose good color is `Remapped`).
///
/// Marking never *needs* this — the mark bit lives in the [`ZMarkContext`]'s
/// side table, not in the pointer this module handles — but the barrier and
/// the metrics module both want to log "which parity is this cycle", and
/// deriving it in one place keeps the two mark bits from being open-coded.
pub fn mark_color_for(good_mask: u64) -> Option<ZColor> {
    match good_mask {
        Z_MARKED0 => Some(ZColor::Marked0),
        Z_MARKED1 => Some(ZColor::Marked1),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// ZMarkContext
// ---------------------------------------------------------------------------

/// Everything the marking engine needs to know about the heap, and nothing
/// more.
///
/// This trait exists because pages, the load barrier and forwarding are being
/// written in parallel with this module and cannot be imported. It is also
/// what makes the engine unit-testable against a plain in-memory graph
/// ([`TestMarkContext`]) instead of against a real arena.
///
/// # Address domain
///
/// Every `u64` crossing this trait is an **unmasked machine address of an
/// object base** — never a
/// [`ZColoredWord`](crate::zgc::vaddr::ZColoredWord), never an interior
/// pointer, and **never a 42-bit heap offset**. The implementor is responsible
/// for [`uncolor`](crate::zgc::vaddr::ZVirtualAddressSpace::uncolor)ing slot
/// contents inside [`visit_refs`](Self::visit_refs). Keeping colored words
/// out of this module is deliberate: `vaddr`'s bit-63 tag exists so an
/// escaped colored word trips
/// [`cratonvm_types::plausible_heap_pointer`], and the fewer modules that
/// handle raw colored words, the fewer places that can leak one.
///
/// **`crate::zgc::barrier` is the other domain**, and handing one of its bare
/// offsets to [`try_mark`](Self::try_mark) here is not a loud failure — it is a
/// mark silently refused by [`is_in_heap`](Self::is_in_heap) and counted as a
/// wild pointer, while the object it named is swept. The module header's
/// "Address domain" section is the full argument; the seam is
/// [`ZMarkHandle::mark_live_offset`], and the base it converts with is
/// [`heap_base`](Self::heap_base) below.
///
/// # Threading
///
/// Every method is called concurrently from all mark workers **and** from
/// arbitrary mutator threads (via [`ZMarkIngress`]). Implementations must be
/// internally synchronised and must not block: the drain loop's safepoint
/// responsiveness is bounded by the slowest [`visit_refs`](Self::visit_refs)
/// call, and a blocking implementation turns
/// [`ZMarkCoordinator::pause_for_safepoint`] into a hang.
pub trait ZMarkContext: Send + Sync {
    /// The current good mask, i.e.
    /// [`ZGoodMask::good`](crate::zgc::vaddr::ZGoodMask::good).
    ///
    /// The engine does not branch on this; it is surfaced for logging and
    /// for the load barrier, which shares the context.
    fn good_mask(&self) -> u64;

    /// Mark the object at `addr` live for this cycle.
    ///
    /// Returns `true` iff the object was **newly** marked — i.e. iff the
    /// caller now owns the obligation to scan it. Returning `false` for an
    /// already-marked object is what makes cyclic graphs terminate, and it
    /// must be atomic with respect to concurrent callers: two workers (or a
    /// worker and a mutator) racing on the same object must see exactly one
    /// `true`, or the object is scanned twice (wasteful) — or, if the
    /// implementation is non-atomic, pushed twice and popped once (a live
    /// object with unscanned out-edges, which is a use-after-free).
    fn try_mark(&self, addr: u64) -> bool;

    /// Is the object at `addr` marked live for this cycle?
    ///
    /// Query-only; never sets the bit. Required by
    /// [`ZMarkCoordinator::process_non_strong_refs`], which must ask
    /// "is the referent live?" without answering "yes, because I just marked
    /// it" — the failure mode that makes every weak reference immortal.
    fn is_marked(&self, addr: u64) -> bool;

    /// Invoke `f` once per outgoing **strong** reference of the object at
    /// `addr`, passing each referent's unmasked address. Null slots may be
    /// passed as `0` or skipped; the engine tolerates both.
    ///
    /// "Strong" is load-bearing: the referent slot of a registered
    /// `Weak`/`Soft`/`Phantom`/`Cleaner` reference object must **not** be
    /// reported here, or the referent is trivially reachable through its own
    /// `Reference` and can never be cleared. `ZgcRealHeap::collect_garbage`
    /// implements exactly this skip today via its `ref_skip_objs` set and
    /// `skip_for(addr)` closure; an implementor of this trait should reuse
    /// that mechanism rather than reinvent it.
    fn visit_refs(&self, addr: u64, f: &mut dyn FnMut(u64));

    /// Is `addr` the base address of a live allocation in this heap?
    ///
    /// This is the wild-pointer gate. Reference slots are raw bytes that a
    /// racing mutator may be mid-store into, so a child address read out of
    /// one can be anything at all; `ZgcRealHeap` already refuses non-base
    /// child pointers with a `wild_skipped` counter for the same reason.
    /// The engine counts refusals in
    /// [`ZMarkStats::off_heap_children`] rather than trusting them.
    fn is_in_heap(&self, addr: u64) -> bool;

    /// Bytes to charge to live-set accounting for the object at `addr`.
    /// Default `0`, meaning "accounting disabled"; a real page-backed
    /// context returns the allocation size so the relocation-set chooser can
    /// compute per-page liveness.
    fn object_size(&self, _addr: u64) -> usize {
        0
    }

    /// The origin of `crate::zgc::barrier`'s **offset domain** for this heap:
    /// the address such that `machine_address == heap_base + heap_offset`.
    ///
    /// *Added 2026-08-07 (second reconciliation pass). This is the only piece
    /// of offset-domain knowledge in the module, and it exists solely so
    /// [`ZMarkHandle::mark_live_offset`] can convert at the seam instead of the
    /// barrier guessing — see the module header's "Address domain" section.*
    ///
    /// # `None` is the default, and it means "refuse", not "zero"
    ///
    /// `None` says **this context does not participate in the barrier's offset
    /// domain**, and the `_offset` entry points then refuse — loudly, counted in
    /// [`ZMarkStats::domain_refusals`] — rather than converting with a
    /// fabricated base.
    ///
    /// Defaulting to `Some(0)` was considered and rejected: `0` is a *correct*
    /// base for a heap whose reservation starts at 0 and a *catastrophic* one
    /// for a `Vec<u8>` heap that Linux `mmap`s near `0x7f…`, and the wrong
    /// answer is indistinguishable from the right one at the call site. This
    /// tree has already been bitten by exactly that shape
    /// (`relocate.rs`'s `to_encoding_base: Some(0)` footgun, refused there for
    /// the same reason). A default that cannot be wrong beats a default that is
    /// usually right.
    ///
    /// Defaulting rather than requiring is also deliberate: `ZgcRealHeap` is a
    /// live implementor whose slots hold raw pointers and are never coloured, so
    /// `None` is the *honest* answer for it today, and a required method would
    /// have forced it to invent one.
    ///
    /// An implementor that returns `Some(base)` promises that `base + offset` is
    /// the address its own [`try_mark`](Self::try_mark) and
    /// [`is_in_heap`](Self::is_in_heap) expect, for every offset the barrier can
    /// produce.
    fn heap_base(&self) -> Option<u64> {
        None
    }

    /// Called on each worker thread before its first drain of the pool's
    /// life, and symmetrically at thread exit. Hooks for per-thread state a
    /// context may need (a TLAB-like scratch buffer, a JFR thread binding).
    fn on_worker_start(&self, _worker_id: usize) {}

    /// See [`on_worker_start`](Self::on_worker_start).
    fn on_worker_end(&self, _worker_id: usize) {}
}

/// The weak / soft / phantom / final reference phase hook.
///
/// **This is a hook, not a reference processor.** The canonical implementation
/// is [`crate::reference::ReferenceProcessor`], which all three collectors
/// share and which encodes the HotSpot phase ordering (soft → weak → final →
/// phantom), the soft-reference LRU policy, and the JLS reachability
/// ordering. An implementor of this trait is expected to do roughly what
/// `ZgcRealHeap::collect_garbage` does today:
///
/// ```text
/// let result = processor.process_references(&|a| ctx.is_marked(a), free_mb, now_ms);
/// for addr in processor.soft_survivor_referents()  { keep_alive(addr); }
/// for addr in processor.finalizer_referent_addresses() { keep_alive(addr); }
/// // hand `result.to_enqueue` / `to_finalize` / `cleaner_actions` to the runtime
/// ```
///
/// Everything passed to `keep_alive` is marked and re-published as mark work
/// by [`ZMarkCoordinator::process_non_strong_refs`]; the caller must then
/// drain again, because a resurrected soft referent drags a whole subgraph
/// with it. Skipping that re-drain is precisely the bug the
/// `INT-8 remark: resurrect policy-kept soft referents` block in `zgc.rs`
/// exists to fix, and it presents as a `WeakReference` whose referent was
/// swept while still softly reachable.
pub trait ZNonStrongRefHook {
    /// Run reference processing against the live set computed so far.
    ///
    /// `is_marked` reports the strong-reachability answer as of *now*.
    /// `keep_alive` accepts addresses that must be resurrected.
    fn process(&self, is_marked: &dyn Fn(u64) -> bool, keep_alive: &mut dyn FnMut(u64));
}

// ---------------------------------------------------------------------------
// Statistics
// ---------------------------------------------------------------------------

/// Instance-owned counters for one [`ZMarkCoordinator`].
///
/// Every counter is `Relaxed`: they are pure telemetry, no control flow
/// depends on any of them, and no other memory is published through them.
/// (The termination protocol deliberately uses a mutex-protected generation
/// counter rather than these, so that no correctness argument rests on a
/// statistics field.)
#[derive(Debug, Default)]
pub struct ZMarkStats {
    /// Objects popped and scanned (`visit_refs` calls).
    pub objects_scanned: AtomicU64,
    /// Objects whose mark bit this engine set (excluding roots).
    pub objects_marked: AtomicU64,
    /// Root objects marked by [`ZMarkCoordinator::push_roots`].
    pub roots_marked: AtomicU64,
    /// Publish operations from a local stack into a stripe.
    pub stripe_publishes: AtomicU64,
    /// Objects moved by those publishes.
    pub published_objects: AtomicU64,
    /// Steal probes made (own stripe refills excluded).
    pub steal_attempts: AtomicU64,
    /// Steal probes that returned work.
    pub steals_succeeded: AtomicU64,
    /// Objects acquired by successful steals.
    pub stolen_objects: AtomicU64,
    /// Refills a worker took from its *own* stripe (not a steal).
    pub own_stripe_refills: AtomicU64,
    /// Addresses handed in by mutators through [`ZMarkIngress`].
    pub ingress_pushes: AtomicU64,
    /// Times a worker (or the mark-end flush) drained the ingress.
    pub ingress_drains: AtomicU64,
    /// Objects moved out of the ingress.
    pub ingress_objects: AtomicU64,
    /// Child pointers refused by [`ZMarkContext::is_in_heap`].
    pub off_heap_children: AtomicU64,
    /// Calls to [`ZMarkHandle::mark_live_offset`] /
    /// [`ZMarkHandle::mark_live_buffered_offset`] refused **before** any
    /// conversion, because the argument was not a bare 42-bit heap offset, or
    /// because the [`ZMarkContext`] declared no
    /// [`heap_base`](ZMarkContext::heap_base).
    ///
    /// **Any nonzero value is a wiring defect on the barrier's side**, not a
    /// mutator race. That is exactly why it is a separate counter from
    /// [`off_heap_children`](Self::off_heap_children): before this counter
    /// existed, a barrier wired to the wrong entry point produced a stream of
    /// `off_heap_children` — indistinguishable from the honest wild-pointer
    /// refusals that counter is for, and reading like the collector working
    /// correctly while live objects were swept. See the module header's
    /// "Address domain" section.
    pub domain_refusals: AtomicU64,
    /// Times the last active worker declared a fixed point.
    pub terminations: AtomicU64,
    /// Times the last active worker *tried* to declare a fixed point and
    /// found work in the final probe. A persistently non-zero value means
    /// the publish/idle interleaving is tight — informative, not a defect.
    pub termination_aborts: AtomicU64,
    /// Times [`ZMarkCoordinator::try_end_mark`] answered
    /// [`ZMarkEndResult::Restart`]. This is the mutator-marking rate at the
    /// end of a cycle; a large number means marking is losing the race with
    /// the application and the cycle should have started earlier.
    pub mark_end_restarts: AtomicU64,
    /// Times a worker left its drain loop for a safepoint.
    ///
    /// **Not a reliable oracle for "did this worker participate".** It is
    /// bumped at the loop head of `ZMarkWorker::run_cycle` only, so a worker
    /// that is resumed and re-parked without reaching that head yields without
    /// counting. Assert on the mark set, or on
    /// [`ZMarkTerminator::work_generation`], never on this.
    pub yields: AtomicU64,
    /// [`ZMarkHandle::mark_live`] / [`ZMarkHandle::mark_live_buffered`] calls
    /// that newly marked an object while
    /// [`ZMarkCoordinator::begin_cycle`]/[`end_cycle`](ZMarkCoordinator::end_cycle)
    /// says no cycle is in flight.
    ///
    /// A nonzero value means **the load barrier called us after `end_cycle`**.
    /// The gate is the barrier's obligation, not this handle's — see
    /// [`ZMarkHandle::mark_live`]'s "Whose job is the `is_marking()` gate"
    /// section for the decision and why the failure is bounded rather than
    /// fatal. This counter exists so that obligation is *observable* instead of
    /// silent; it is telemetry and no control flow reads it.
    pub late_marks: AtomicU64,
}

/// A plain-old-data snapshot of [`ZMarkStats`], for assertions and logging.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ZMarkStatsSnapshot {
    #[allow(missing_docs)]
    pub objects_scanned: u64,
    #[allow(missing_docs)]
    pub objects_marked: u64,
    #[allow(missing_docs)]
    pub roots_marked: u64,
    #[allow(missing_docs)]
    pub stripe_publishes: u64,
    #[allow(missing_docs)]
    pub published_objects: u64,
    #[allow(missing_docs)]
    pub steal_attempts: u64,
    #[allow(missing_docs)]
    pub steals_succeeded: u64,
    #[allow(missing_docs)]
    pub stolen_objects: u64,
    #[allow(missing_docs)]
    pub own_stripe_refills: u64,
    #[allow(missing_docs)]
    pub ingress_pushes: u64,
    #[allow(missing_docs)]
    pub ingress_drains: u64,
    #[allow(missing_docs)]
    pub ingress_objects: u64,
    #[allow(missing_docs)]
    pub off_heap_children: u64,
    /// See [`ZMarkStats::domain_refusals`]. Nonzero means the load barrier is
    /// wired to the wrong entry point.
    pub domain_refusals: u64,
    #[allow(missing_docs)]
    pub terminations: u64,
    #[allow(missing_docs)]
    pub termination_aborts: u64,
    #[allow(missing_docs)]
    pub mark_end_restarts: u64,
    #[allow(missing_docs)]
    pub yields: u64,
    #[allow(missing_docs)]
    pub late_marks: u64,
}

impl ZMarkStats {
    /// Zero every counter: what a POOL REUSED ACROSS CYCLES must do before
    /// each one, because its readers ask "how much did THIS cycle do?".
    ///
    /// `off_heap_children` is the one that would bite silently:
    /// `collect_garbage` reads it as that cycle.s `wild_skipped` and warns
    /// when it is non-zero, so a cumulative count would report corrupt
    /// reference slots on every cycle after the first one that saw any.
    pub fn reset(&self) {
        for counter in [
            &self.objects_scanned,
            &self.objects_marked,
            &self.roots_marked,
            &self.stripe_publishes,
            &self.published_objects,
            &self.steal_attempts,
            &self.steals_succeeded,
            &self.stolen_objects,
            &self.own_stripe_refills,
            &self.ingress_pushes,
            &self.ingress_drains,
            &self.ingress_objects,
            &self.off_heap_children,
            &self.domain_refusals,
            &self.terminations,
            &self.termination_aborts,
        ] {
            counter.store(0, Ordering::Relaxed);
        }
    }

    /// Read every counter. Not atomic as a group; diagnostics only.
    pub fn snapshot(&self) -> ZMarkStatsSnapshot {
        ZMarkStatsSnapshot {
            objects_scanned: self.objects_scanned.load(Ordering::Relaxed),
            objects_marked: self.objects_marked.load(Ordering::Relaxed),
            roots_marked: self.roots_marked.load(Ordering::Relaxed),
            stripe_publishes: self.stripe_publishes.load(Ordering::Relaxed),
            published_objects: self.published_objects.load(Ordering::Relaxed),
            steal_attempts: self.steal_attempts.load(Ordering::Relaxed),
            steals_succeeded: self.steals_succeeded.load(Ordering::Relaxed),
            stolen_objects: self.stolen_objects.load(Ordering::Relaxed),
            own_stripe_refills: self.own_stripe_refills.load(Ordering::Relaxed),
            ingress_pushes: self.ingress_pushes.load(Ordering::Relaxed),
            ingress_drains: self.ingress_drains.load(Ordering::Relaxed),
            ingress_objects: self.ingress_objects.load(Ordering::Relaxed),
            off_heap_children: self.off_heap_children.load(Ordering::Relaxed),
            domain_refusals: self.domain_refusals.load(Ordering::Relaxed),
            terminations: self.terminations.load(Ordering::Relaxed),
            termination_aborts: self.termination_aborts.load(Ordering::Relaxed),
            mark_end_restarts: self.mark_end_restarts.load(Ordering::Relaxed),
            yields: self.yields.load(Ordering::Relaxed),
            late_marks: self.late_marks.load(Ordering::Relaxed),
        }
    }
}

// ---------------------------------------------------------------------------
// ZMarkLocalStack
// ---------------------------------------------------------------------------

/// A mark worker's private work stack.
///
/// Plain `Vec<u64>`, owned by exactly one thread, therefore **no
/// synchronisation of any kind** on push/pop — that is the whole point.
/// OpenJDK gets the same effect with a per-worker `ZMarkStripe` cache; the
/// Rust ownership model gives it to us for free, and gives it to us
/// *provably* rather than by convention.
///
/// LIFO, because depth-first marking has far better locality than
/// breadth-first: the children a worker just discovered are the ones whose
/// headers are still in L1.
#[derive(Debug, Default)]
pub struct ZMarkLocalStack {
    buf: Vec<u64>,
}

impl ZMarkLocalStack {
    /// A stack pre-sized so that reaching the high-water mark and publishing
    /// half of it never reallocates in steady state.
    pub fn new() -> Self {
        ZMarkLocalStack {
            buf: Vec::with_capacity(Z_MARK_LOCAL_HIGH_WATER * 2),
        }
    }

    /// Push. No lock, no atomic, no bounds logic.
    #[inline]
    pub fn push(&mut self, addr: u64) {
        self.buf.push(addr);
    }

    /// Pop. See [`push`](Self::push).
    #[inline]
    pub fn pop(&mut self) -> Option<u64> {
        self.buf.pop()
    }

    /// Current depth.
    #[inline]
    pub fn len(&self) -> usize {
        self.buf.len()
    }

    /// Is the stack empty?
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }

    /// Remove and return the **oldest** half of the stack.
    ///
    /// Oldest, not newest, and that choice matters twice over:
    ///
    /// * The newest entries are this worker's current DFS frontier — the
    ///   objects whose parents it just touched. Keeping them local preserves
    ///   the locality the LIFO discipline bought.
    /// * The oldest entries sit nearest the roots, so each one is likely to
    ///   head a large unexplored subtree. Handing *those* out gives a thief
    ///   real work rather than a handful of leaves, which is what makes one
    ///   steal worth its mutex acquisition.
    pub fn split_off_oldest_half(&mut self) -> Vec<u64> {
        let half = self.buf.len() / 2;
        self.buf.drain(..half).collect()
    }

    /// Remove and return everything.
    pub fn take_all(&mut self) -> Vec<u64> {
        std::mem::take(&mut self.buf)
    }

    /// Mutable access to the backing storage, so a steal can `extend` into it
    /// without a temporary allocation.
    #[inline]
    fn buf_mut(&mut self) -> &mut Vec<u64> {
        &mut self.buf
    }
}

// ---------------------------------------------------------------------------
// ZMarkStripeSet
// ---------------------------------------------------------------------------

/// The shared, striped overflow queues that workers publish to and steal
/// from.
///
/// One `parking_lot::Mutex<Vec<u64>>` per stripe. A lock-free Chase–Lev deque
/// would be the textbook answer, but it is not available here: no new crate
/// dependencies are permitted, and hand-rolling one is a materially worse
/// trade than a striped mutex given (a) the stripe is touched only on
/// publish/steal, not per object — the per-object path is
/// [`ZMarkLocalStack`], which takes no lock at all — and (b)
/// `parking_lot::Mutex` is a single atomic RMW when uncontended, which at
/// [`Z_MARK_STRIPE_OVERSUBSCRIPTION`] = 4 it almost always is.
///
/// The count is always a power of two; see [`stripe_count_for`].
#[derive(Debug)]
pub struct ZMarkStripeSet {
    stripes: Vec<Mutex<Vec<u64>>>,
    mask: usize,
}

impl ZMarkStripeSet {
    /// Build `count` stripes. `count` must be a non-zero power of two.
    pub fn new(count: usize) -> Self {
        assert!(
            count.is_power_of_two() && count > 0,
            "ZGC mark stripe count must be a non-zero power of two (got {count})"
        );
        let stripes = (0..count).map(|_| Mutex::new(Vec::new())).collect();
        ZMarkStripeSet {
            stripes,
            mask: count - 1,
        }
    }

    /// How many stripes exist.
    #[inline]
    pub fn stripe_count(&self) -> usize {
        self.stripes.len()
    }

    /// Map any key (worker id, mutator slot, round-robin counter) onto a
    /// stripe index. One `AND`, because the count is a power of two.
    #[inline]
    pub fn index_for(&self, key: usize) -> usize {
        key & self.mask
    }

    /// Move everything in `work` into the stripe owned by `key`.
    ///
    /// Returns the number of entries published. **Does not** notify the
    /// terminator: the caller must call
    /// [`ZMarkTerminator::note_work_published`] *after* this returns, so that
    /// the stripe lock is already released when the terminator's state lock
    /// is taken. See the module docs on locking discipline.
    pub fn publish(&self, key: usize, work: &mut Vec<u64>) -> usize {
        if work.is_empty() {
            return 0;
        }
        let idx = self.index_for(key);
        let n = work.len();
        let mut stripe = self.stripes[idx].lock();
        stripe.append(work);
        n
    }

    /// Take up to `cap` entries from stripe `idx` into `out`. Used by a
    /// worker refilling from its **own** stripe, where there is no reason to
    /// leave anything behind.
    pub fn drain_from(&self, idx: usize, out: &mut Vec<u64>, cap: usize) -> usize {
        let mut stripe = self.stripes[idx & self.mask].lock();
        let take = stripe.len().min(cap);
        if take == 0 {
            return 0;
        }
        let at = stripe.len() - take;
        out.extend(stripe.drain(at..));
        take
    }

    /// Take **half** (rounded up) of stripe `idx`, capped at `cap`, into
    /// `out`. Returns the number taken.
    ///
    /// Halving rather than draining is what makes stealing converge: a thief
    /// that emptied its victim would immediately have to find a new one,
    /// while the victim — which is by definition making progress — would be
    /// starved. Splitting leaves both parties with work and halves the
    /// remaining imbalance per steal, so `log2(n)` steals suffice to spread
    /// `n` items across the pool.
    pub fn steal_half(&self, idx: usize, out: &mut Vec<u64>, cap: usize) -> usize {
        let mut stripe = self.stripes[idx & self.mask].lock();
        let len = stripe.len();
        if len == 0 {
            return 0;
        }
        let take = len.div_ceil(2).min(cap);
        let at = len - take;
        out.extend(stripe.drain(at..));
        take
    }

    /// Is stripe `idx` empty right now?
    pub fn is_stripe_empty(&self, idx: usize) -> bool {
        self.stripes[idx & self.mask].lock().is_empty()
    }

    /// Does **any** stripe hold work?
    ///
    /// This is the termination probe's stripe half. It acquires every stripe
    /// lock in turn, which is exactly what makes it authoritative: a
    /// publisher's write became visible at *its* stripe unlock, so any
    /// subsequent acquisition of that stripe observes it. There is no
    /// atomic-hint fast path here on purpose — a stale hint in the "empty"
    /// direction would terminate a cycle with live work outstanding.
    pub fn has_work(&self) -> bool {
        self.stripes.iter().any(|s| !s.lock().is_empty())
    }

    /// Total entries across all stripes (diagnostics).
    pub fn total_len(&self) -> usize {
        self.stripes.iter().map(|s| s.lock().len()).sum()
    }

    /// Drop all pending work. Only legal between cycles, with no worker
    /// running.
    pub fn clear(&self) {
        for stripe in &self.stripes {
            stripe.lock().clear();
        }
    }
}

// ---------------------------------------------------------------------------
// ZMarkIngress
// ---------------------------------------------------------------------------

/// The mutator → marker hand-off.
///
/// The ZGC load barrier's slow path runs on an arbitrary application thread
/// and needs to say "I just resolved a stale reference; this object is live
/// and has not been scanned". That address has to reach a mark worker.
///
/// # Why this is not just another stripe
///
/// If mutators published straight into [`ZMarkStripeSet`], every load-barrier
/// slow path would contend with the workers' steal path on the same mutexes,
/// and the steal path is the one that runs when the pool is *starved* — i.e.
/// exactly when added latency hurts most. So the ingress is a separate set of
/// buckets, and workers touch it only in
/// [`ZMarkWorker::acquire_work`]'s last resort, after their own stripe, after
/// random victims, and after the full stripe sweep. In steady state the steal
/// path never acquires an ingress lock at all.
///
/// # Slotting
///
/// The caller supplies a `slot` (a mutator thread index, or any stable
/// per-thread number — the load barrier module owns that choice). It selects
/// a bucket, so two mutator threads with different slots never contend. There
/// is deliberately no `thread_local!` here: this tree has had parallel-test
/// crashes from process-global GC state, and a thread-local buffer keyed to
/// the *thread* rather than the *heap* is exactly that bug in miniature (see
/// `satb.rs`'s `thread_local_buffers_are_queue_scoped` regression test, which
/// exists because the SATB buffers were once not queue-scoped).
/// One ingress bucket: the queue and its counters, all under one lock.
///
/// # Why the counters are inside the mutex rather than atomics beside it
///
/// `ZMarkIngress` buckets its queues so mutators publishing SATB work land on
/// different locks instead of contending on one. That striping was **defeated by
/// two shared `AtomicUsize`s** on the same path: `pending_hint` here and
/// `ZgcRealHeap::mark_ingress_pushes` at the caller, each a `fetch_add` on one
/// cache line per reference store, from every mutator. Striping N locks and then
/// funnelling every push through one atomic counter leaves the contention exactly
/// where it was.
///
/// The push already holds this bucket's lock, so a plain `usize` increment under
/// it is free. Telemetry reads sum across buckets, which is rare and is allowed
/// to be slow.
#[derive(Debug, Default)]
struct ZIngressBucket {
    queue: Vec<u64>,
    /// Cumulative pushes into this bucket, for
    /// [`ZMarkIngress::pushed_total`]. Never reset by `drain_into` — a
    /// *cumulative* count is what a caller deciding "have I published enough to
    /// hand off?" wants, and it is what the suppression-channel test asserts on.
    pushed_total: usize,
}

#[derive(Debug)]
pub struct ZMarkIngress {
    buckets: Vec<Mutex<ZIngressBucket>>,
    mask: usize,
}

impl Default for ZMarkIngress {
    fn default() -> Self {
        ZMarkIngress::new()
    }
}

impl ZMarkIngress {
    /// A fresh ingress with [`Z_MARK_INGRESS_BUCKETS`] buckets.
    pub fn new() -> Self {
        let buckets = (0..Z_MARK_INGRESS_BUCKETS)
            .map(|_| Mutex::new(ZIngressBucket::default()))
            .collect();
        ZMarkIngress {
            buckets,
            mask: Z_MARK_INGRESS_BUCKETS - 1,
        }
    }

    #[inline]
    fn bucket_for(&self, slot: usize) -> usize {
        slot & self.mask
    }

    /// Push one address, and return this **bucket's** cumulative push count.
    ///
    /// Costs one uncontended mutex per call, which is why
    /// [`ZMarkMutatorBuffer`] exists and should be preferred wherever the
    /// caller can hold per-thread state.
    ///
    /// # Why it returns a count
    ///
    /// So a caller deciding "have I published enough to hand off to the marker?"
    /// can read the answer out of the lock it is already holding, instead of
    /// keeping a shared atomic of its own. `ZgcRealHeap::satb_pre_barrier_slow`
    /// did keep one, and it was a `fetch_add` on a single cache line per
    /// reference store from every mutator — which is precisely the contention
    /// the buckets exist to avoid. It is a PER-BUCKET count, so a handoff
    /// interval of K now means "K pushes into one bucket" rather than "K pushes
    /// in total"; with `Z_MARK_INGRESS_BUCKETS` buckets and addresses spread over
    /// them, the effective interval is that much longer, and the caller's
    /// constant is chosen with that in mind.
    pub fn push(&self, slot: usize, addr: u64) -> usize {
        let idx = self.bucket_for(slot);
        let mut b = self.buckets[idx].lock();
        b.queue.push(addr);
        b.pushed_total += 1;
        b.pushed_total
    }

    /// Move everything in `buf` into this slot's bucket.
    pub fn push_batch(&self, slot: usize, buf: &mut Vec<u64>) -> usize {
        if buf.is_empty() {
            return 0;
        }
        let idx = self.bucket_for(slot);
        let n = buf.len();
        let mut b = self.buckets[idx].lock();
        b.queue.append(buf);
        b.pushed_total += n;
        n
    }

    /// Does any bucket hold work? Authoritative — scans under the locks, for
    /// the same reason [`ZMarkStripeSet::has_work`] does.
    pub fn has_work(&self) -> bool {
        self.buckets.iter().any(|b| !b.lock().queue.is_empty())
    }

    /// Move everything out of every bucket into `out`. Returns the count.
    pub fn drain_into(&self, out: &mut Vec<u64>) -> usize {
        let mut moved = 0usize;
        for bucket in &self.buckets {
            let mut b = bucket.lock();
            if b.queue.is_empty() {
                continue;
            }
            moved += b.queue.len();
            out.append(&mut b.queue);
        }
        moved
    }

    /// Telemetry hint: how much is queued right now. Never use this to decide
    /// termination.
    ///
    /// Summed under the locks rather than kept in a shared counter — see
    /// [`ZIngressBucket`]. It is O(buckets) and it runs on a diagnostic path.
    pub fn pending_hint(&self) -> usize {
        self.buckets.iter().map(|b| b.lock().queue.len()).sum()
    }

    /// Cumulative pushes since the last [`Self::clear`], across every bucket.
    pub fn pushed_total(&self) -> usize {
        self.buckets.iter().map(|b| b.lock().pushed_total).sum()
    }

    /// Drop everything. Only legal between cycles.
    pub fn clear(&self) {
        for bucket in &self.buckets {
            let mut b = bucket.lock();
            b.queue.clear();
            b.pushed_total = 0;
        }
    }
}

/// A per-mutator-thread batching buffer for the load barrier.
///
/// The owner pushes into it with no synchronisation whatsoever, and flushes
/// into [`ZMarkIngress`] when [`push`](Self::push) reports the buffer is
/// full. Storage is the caller's: the load barrier module puts one of these
/// in its per-thread state. Nothing here is a `thread_local!`, so two heaps
/// in one process cannot cross-contaminate.
///
/// # Thread death is a use-after-free, and `Drop` is what closes it
///
/// *Finding A of the 2026-08-07 protocol audit. No test caught it.*
///
/// [`ZMarkHandle::mark_live_buffered`] calls
/// [`ZMarkContext::try_mark`] — which **sets the mark bit** — and only then
/// pushes into this buffer. So between those two steps, and for as long as the
/// entry sits here unflushed, the object is in the one state the whole
/// termination protocol exists to make impossible: **marked, therefore
/// undiscoverable by any future `try_mark`, and simultaneously absent from
/// every queue, therefore never scanned.** Its out-edges are never traced and
/// everything reachable only through it is swept while live.
///
/// While the thread is alive this is merely a deferral, and the safepoint
/// contract on [`ZMarkCoordinator::try_end_mark`] (flush every buffer first)
/// discharges it. **Thread exit is where it becomes permanent**: a dying
/// mutator's buffer is simply dropped, and the safepoint has nobody left to
/// flush.
///
/// ## Why not `crate::satb`'s registry, which solved exactly this
///
/// SATB's answer — a `static` registry of live per-thread buffers plus
/// orphan-parking of a dying thread's bucket — is built out of the two things
/// this module's header forbids (`static`, `thread_local!`), and forbids for a
/// reason that applies here directly. `crate::zgc::barrier` did reproduce that
/// registry, but for its **own** `ZMarkQueue`; a `ZMarkMutatorBuffer` is never
/// registered there and `flush_all_thread_mark_buffers` cannot see one. The two
/// mechanisms are disjoint.
///
/// ## What is done instead
///
/// An **attached** buffer ([`ZMarkHandle::new_buffer`]) carries a
/// `Weak<ZMarkShared>` and flushes itself into the ingress from its own
/// [`Drop`]. That is strictly stronger than a registry — thread exit runs
/// `Drop` unconditionally, so there is no window in which a buffer is live but
/// unregistered — and it keeps the no-process-global-state rule intact.
///
/// The audit noted that `impl Drop` "cannot fix it alone, because the buffer
/// holds no handle to flush into". Correct, and that is the change: the buffer
/// now holds one. `Weak`, not `Arc`, so a mutator parked on a buffer cannot
/// keep a dropped pool's shared state alive; if the pool is already gone the
/// upgrade fails and the entries are discarded, which is sound because no sweep
/// can be run against that pool's mark bits any more.
///
/// ## The one window `Drop` does not close
///
/// A thread that is **aborted** (not unwound) between `try_mark` returning
/// `true` and [`push`](Self::push) storing the address runs no destructor, and
/// that address is lost. The window is two instructions wide and cannot be
/// closed from this side: the mark bit *is* the deduplication, so it cannot be
/// set after the push without admitting duplicates. HotSpot's ZGC has the same
/// shape. A mutator abort during a mark cycle is already fatal to the VM.
///
/// ## Detached buffers are legal and are the caller's problem
///
/// [`new`](Self::new) still exists and still produces a buffer with no link to
/// any pool — the load barrier may legitimately want one for a thread whose
/// lifetime it manages itself. Dropping such a buffer non-empty logs a
/// `tracing::error!` (not a `debug_assert!`: this repo has been bitten by
/// release runs silently skipping debug-only checks) and loses the entries.
/// Prefer [`ZMarkHandle::new_buffer`] unless you can prove the flush.
#[derive(Debug)]
pub struct ZMarkMutatorBuffer {
    slot: usize,
    buf: Vec<u64>,
    /// The pool this buffer flushes into on thread death, if any.
    ///
    /// `None` for a buffer built by [`new`](Self::new); `Some` for one built by
    /// [`ZMarkHandle::new_buffer`]. See the type docs for why `Weak`.
    owner: Option<Weak<ZMarkShared>>,
}

impl ZMarkMutatorBuffer {
    /// A **detached** buffer bound to ingress bucket `slot & (buckets-1)`.
    ///
    /// Nothing will flush this buffer for you, including on thread exit — read
    /// the "Detached buffers" section on the type before choosing this over
    /// [`ZMarkHandle::new_buffer`], which returns an equivalent buffer that
    /// flushes itself from `Drop`.
    pub fn new(slot: usize) -> Self {
        ZMarkMutatorBuffer {
            slot,
            buf: Vec::with_capacity(Z_MARK_MUTATOR_BUFFER_CAPACITY),
            owner: None,
        }
    }

    /// This buffer's ingress slot.
    #[inline]
    pub fn slot(&self) -> usize {
        self.slot
    }

    /// Whether this buffer will flush itself into its pool on drop.
    ///
    /// `false` for [`new`](Self::new), `true` for
    /// [`ZMarkHandle::new_buffer`]. Exposed so a caller assembling per-thread
    /// state can assert the property rather than assume it.
    #[inline]
    pub fn is_attached(&self) -> bool {
        self.owner.is_some()
    }

    /// Buffer an address. Returns `true` when the buffer is full and the
    /// caller must flush it via [`ZMarkHandle::flush_buffer`].
    ///
    /// Returning a "please flush" signal rather than flushing internally
    /// keeps this type free of any reference to the coordinator, so it can
    /// live in a per-thread struct with no lifetime or `Arc` entanglement.
    #[inline]
    pub fn push(&mut self, addr: u64) -> bool {
        self.buf.push(addr);
        self.buf.len() >= Z_MARK_MUTATOR_BUFFER_CAPACITY
    }

    /// Entries currently buffered.
    #[inline]
    pub fn len(&self) -> usize {
        self.buf.len()
    }

    /// Is the buffer empty?
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }
}

/// Thread-death flush. See the "Thread death is a use-after-free" section on
/// [`ZMarkMutatorBuffer`] for why this exists.
///
/// # Lock order
///
/// [`ZMarkIngress::push_batch`] takes and releases one ingress bucket lock;
/// [`ZMarkTerminator::note_work_published`] is called only afterwards, with no
/// ingress lock held. That is the module's documented order
/// (`terminator.state > stripe[i] > ingress[j]`) and the inverse is the ABBA
/// deadlock the header warns about.
///
/// # Why it flushes even when the cycle is over
///
/// This runs on an arbitrary mutator thread at an arbitrary instant, so the
/// cycle may already have ended. It flushes anyway, for the same reason
/// [`ZMarkWorker::run`]'s leftover-publish does: dropping mark work is a
/// use-after-free, publishing it is at worst floating garbage that the next
/// [`ZMarkCoordinator::begin_cycle`] clears out of the ingress.
impl Drop for ZMarkMutatorBuffer {
    fn drop(&mut self) {
        if self.buf.is_empty() {
            return;
        }
        // Bound to a local first so the borrow of `self.owner` is over before
        // the arms touch `self.buf` — the two are disjoint fields, but there is
        // no reason to make a reader (or a future edit) rely on that.
        let owner = self.owner.as_ref().and_then(Weak::upgrade);
        let shared = match owner {
            Some(s) => s,
            None => {
                // Either a detached buffer, or an attached one whose pool has
                // already been dropped. The second case is benign (no sweep can
                // run against a dropped pool's mark bits); the first is a
                // caller contract violation and is the whole reason
                // `ZMarkHandle::new_buffer` exists.
                if self.owner.is_none() {
                    tracing::error!(
                        target: "zgc",
                        slot = self.slot,
                        pending = self.buf.len(),
                        "ZGC mark: a DETACHED ZMarkMutatorBuffer was dropped holding \
                         entries. Those objects are already marked, so no later \
                         try_mark will rediscover them, and they are in no queue, so \
                         no worker will ever scan them: everything reachable only \
                         through them will be swept while live. Build mutator buffers \
                         with ZMarkHandle::new_buffer, which flushes on drop."
                    );
                }
                return;
            }
        };
        let n = shared.ingress.push_batch(self.slot, &mut self.buf);
        if n > 0 {
            shared
                .stats
                .ingress_pushes
                .fetch_add(n as u64, Ordering::Relaxed);
            // Ingress lock released inside `push_batch`; the terminator lock is
            // legal to take here and nowhere earlier.
            shared.terminator.note_work_published();
            tracing::debug!(
                target: "zgc",
                slot = self.slot,
                flushed = n,
                "ZGC mark: a mutator buffer was flushed from Drop (thread exit or \
                 scope end) rather than being lost"
            );
        }
    }
}

// ---------------------------------------------------------------------------
// ZMarkTerminator
// ---------------------------------------------------------------------------

/// Outcome of a worker offering itself for termination.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ZIdleOutcome {
    /// Work appeared; the worker is counted active again and must retry.
    Resume,
    /// The cycle reached a fixed point (or is being torn down); the worker
    /// leaves the cycle and waits for the next arm.
    Terminated,
}

/// State the terminator guards with one mutex.
#[derive(Debug)]
struct ZMarkTerminationState {
    /// Workers that have *not* offered themselves for termination.
    ///
    /// Deliberately a plain `usize` under the lock rather than an atomic.
    /// Every read and every write happens with the lock held, so the "am I
    /// the last one out?" test and the queue probe that follows it are one
    /// critical section. Making this an atomic would buy nothing (the probe
    /// needs the lock anyway) and would replace a trivially-checkable
    /// mutual-exclusion argument with an ordering argument.
    active: usize,
    /// Has a fixed point been declared for the current cycle?
    terminated: bool,
    /// Bumped by [`ZMarkTerminator::arm`]. Workers carry the value they
    /// joined with, so a worker cannot accidentally participate in two
    /// cycles or re-enter one it already left.
    cycle_generation: u64,
    /// Bumped by every [`ZMarkTerminator::note_work_published`]. A worker
    /// parked in the handshake compares this against the value it saw on the
    /// way in; a change means "somebody published, go look again".
    work_generation: u64,
}

/// The termination handshake.
///
/// # The race this closes
///
/// Consider two workers, A and B, and the naive test *"my stack is empty and
/// every stripe is empty, therefore marking is done"*:
///
/// ```text
///   worker A                        worker B
///   --------                        --------
///   pop() -> None                   pop() -> obj X
///                                   ctx.visit_refs(X, ...)
///   probe stripe 0 .. n : empty       |  child Y is in a register:
///                                     |  it is in NO stack and NO stripe
///   "everything is empty"             |
///   declare marking complete   <----- |  ... and only now push(Y)
/// ```
///
/// At the instant A probes, `Y` exists nowhere the probe can see it. A
/// terminates, the sweep runs, and `Y` — reachable, live — is freed. That is
/// a use-after-free produced by a marker that was individually correct at
/// every step.
///
/// The fix is the **active-worker count**, and it works because of one
/// invariant: *a worker may only offer itself for termination with an empty
/// local stack, and it takes this mutex to do so.* Therefore:
///
/// * B is still counted active while it is inside `visit_refs`, so when A
///   decrements, `active` goes 2 → 1, not 1 → 0. A is **not** the last
///   worker out and may not declare anything. It parks.
/// * When B eventually goes idle it must first have finished `visit_refs`,
///   which means `Y` is in B's local stack (or, if B published, in a
///   stripe). B then drains `Y` — so B is not idle at all — and only reaches
///   the handshake once `Y`'s whole subtree is done.
/// * Whichever worker *is* last (`active` 1 → 0) re-probes **every** stripe
///   and **every** ingress bucket while still holding this mutex. Every
///   earlier publish is visible to that probe: a publisher wrote under a
///   stripe mutex and released it before taking this one, so the probe's
///   acquisition of that same stripe mutex is ordered after the write.
///
/// So the fixed point is only declared when (i) no worker holds work in a
/// register or a local stack, and (ii) no queue holds work, and (iii) both
/// facts were established without either being able to change underneath the
/// other.
///
/// # And the mutators?
///
/// The count above bounds only the *workers*. Mutator threads are not in
/// `active` and never will be — the whole point of ZGC is that they keep
/// running. A mutator can publish into the ingress the instant after the
/// last worker's probe reads that bucket as empty.
///
/// **That is not a bug and is not fixable concurrently.** It is why a fixed
/// point is not a completion:
///
/// * If the mutator publishes *before* the probe reads its bucket → the probe
///   sees it, [`ZIdleOutcome::Resume`], marking continues.
/// * If the mutator publishes *after* → the fixed point is declared, and the
///   work sits in the ingress. Nothing has been lost; it has merely not been
///   drained yet. [`ZMarkCoordinator::try_end_mark`], which runs at the
///   mark-end **safepoint** with every mutator stopped, flushes the ingress
///   and finds it, answers [`ZMarkEndResult::Restart`], and the concurrent
///   phase runs again.
///
/// This is exactly OpenJDK's `ZMark::try_end()` / restart loop, and the
/// reason ZGC's mark-end pause can iterate while G1's remark cannot: G1's
/// SATB producer is quiesced by the pause and by construction cannot have
/// anything left, whereas ZGC's producers only stop *at* the pause, so the
/// pause has to look and may have to send everyone back around.
#[derive(Debug)]
pub struct ZMarkTerminator {
    n_workers: usize,
    state: Mutex<ZMarkTerminationState>,
    wake: Condvar,
    /// Lock-free mirror of `state.terminated`, for cheap polling by callers
    /// that must not block (diagnostics, the load barrier's "is marking
    /// converged" hint). Written under the state lock, so it can lag a reader
    /// that does not take the lock — never make a correctness decision on it.
    /// `Release` on store / `Acquire` on load so a reader that *does* observe
    /// `true` also observes everything the terminating worker did first.
    terminated_hint: AtomicBool,
    /// Times a [`Self::wait_for_fixed_point`] wait expired instead of being
    /// woken by the termination edge.
    ///
    /// # Why a counter and not a timing
    ///
    /// Every `wait_for` in this module is `wait_for` and never a bare `wait`,
    /// because a lost notification must cost a poll interval rather than a hang
    /// -- this codebase has a documented history of GC livelocks presenting as an
    /// unexplained freeze. That insurance is invisible when it is *load-bearing*:
    /// a wait that always times out behaves identically to one that is notified,
    /// only `Z_MARK_PARK_POLL_MS` slower, and no assertion anywhere fires.
    ///
    /// So this counts the expiries. **Nonzero on a stop-the-world cycle means a
    /// notification is missing**, and it is a count rather than a duration, so it
    /// says so on a loaded host where a timing could not.
    ///
    /// It caught exactly that: the mark driver
    /// (`ZgcConcurrentMarkController::await_fixed_point`) polled a *different*
    /// condvar, on a 5 ms grid, which nothing ever notified -- so every pass of
    /// every cycle waited out the full interval before noticing a mark that had
    /// already finished.
    park_timeouts: AtomicU64,
    /// Times a [`Self::wait_for_fixed_point`] wait was released by a
    /// notification **and found the cycle terminated** — i.e. the termination
    /// edge itself woke the waiter.
    ///
    /// # Why not just count notified wakes
    ///
    /// The driver and the workers wait on the same condvar, and `notify_all` is
    /// also called when a worker publishes work (`work_generation`) and when a
    /// cycle is armed. A plain "was it notified?" counter would therefore be
    /// incremented by traffic that has nothing to do with the fixed point, and
    /// would stay nonzero even with the termination notify removed. Requiring
    /// `terminated` to be true on wake is what makes this specific.
    ///
    /// # Why `park_timeouts == 0` was not a sufficient assertion
    ///
    /// It is satisfied by a wait that was never entered, which is exactly what a
    /// fast mark produces: the driver arrives after the workers have already
    /// converged, takes the `is_terminated` fast path, and waits zero times. The
    /// test therefore has to make the mark slow enough that the wait is certain,
    /// and then assert on THIS — a probe that cannot fail is not a probe.
    park_termination_wakes: AtomicU64,
}

impl ZMarkTerminator {
    /// A terminator for `n_workers` mark threads, starting disarmed.
    pub fn new(n_workers: usize) -> Self {
        ZMarkTerminator {
            n_workers: n_workers.max(1),
            state: Mutex::new(ZMarkTerminationState {
                active: 0,
                terminated: true,
                cycle_generation: 0,
                work_generation: 0,
            }),
            wake: Condvar::new(),
            terminated_hint: AtomicBool::new(true),
            park_timeouts: AtomicU64::new(0),
            park_termination_wakes: AtomicU64::new(0),
        }
    }

    /// Begin a marking cycle: every worker becomes active, the fixed-point
    /// flag clears, and the cycle generation advances so parked workers join.
    ///
    /// Returns the new cycle generation.
    ///
    /// `active` is set to the full worker count *before* any worker has
    /// actually joined. That is intentional: a worker that is slow to wake is
    /// still counted, so a fast worker that drains everything and goes idle
    /// cannot declare a fixed point on behalf of a colleague that has not yet
    /// looked at anything.
    pub fn arm(&self) -> u64 {
        let mut g = self.state.lock();
        g.active = self.n_workers;
        g.terminated = false;
        g.cycle_generation = g.cycle_generation.wrapping_add(1);
        self.terminated_hint.store(false, Ordering::Release);
        self.wake.notify_all();
        g.cycle_generation
    }

    /// Announce that work became available.
    ///
    /// **Callers must hold no stripe or ingress lock.** This takes the
    /// terminator state lock, and the termination probe takes stripe and
    /// ingress locks while holding it; the reverse order deadlocks.
    ///
    /// This intentionally does **not** clear `terminated`. Once a fixed point
    /// is declared the workers have left the cycle, and only
    /// [`arm`](Self::arm) brings them back — so clearing the flag here would
    /// produce a state nobody can leave (`wait_for_fixed_point` would block
    /// forever on a flag no running worker will ever set again). Late work is
    /// instead picked up by the mark-end probe, which is where a decision
    /// about it can actually be acted on.
    pub fn note_work_published(&self) {
        let mut g = self.state.lock();
        g.work_generation = g.work_generation.wrapping_add(1);
        self.wake.notify_all();
    }

    /// Wait until a cycle newer than `last` is armed. Returns its generation,
    /// or `None` if the pool is shutting down.
    pub fn wait_for_cycle(&self, last: u64, should_stop: &AtomicBool) -> Option<u64> {
        let mut g = self.state.lock();
        loop {
            if should_stop.load(Ordering::Acquire) {
                return None;
            }
            if g.cycle_generation != last && !g.terminated {
                return Some(g.cycle_generation);
            }
            self.wake
                .wait_for(&mut g, Duration::from_millis(Z_MARK_PARK_POLL_MS));
        }
    }

    /// A worker with an empty local stack and no stealable work offers itself
    /// for termination. See the type-level docs for the full argument.
    ///
    /// The entire handshake runs under one acquisition of the state lock,
    /// including the queue probe. That is the point: `active == 0` and
    /// "queues empty" must be established together or neither is worth
    /// anything.
    pub fn worker_idle(
        &self,
        my_cycle: u64,
        stripes: &ZMarkStripeSet,
        ingress: &ZMarkIngress,
        stats: &ZMarkStats,
        should_stop: &AtomicBool,
    ) -> ZIdleOutcome {
        let mut g = self.state.lock();

        // Already over (someone else declared it, or a new cycle started
        // without us): leave without touching the count, which `arm` resets
        // wholesale anyway.
        if g.cycle_generation != my_cycle || g.terminated {
            return ZIdleOutcome::Terminated;
        }
        let entry_work_gen = g.work_generation;

        // Fail closed on an impossible count rather than wrapping. `active`
        // can only be zero here if `terminated` were false with every worker
        // already out, which the branch below makes unreachable — but a
        // wrapping `0 - 1` would give a count no worker can ever drive back
        // to zero, i.e. a cycle that never terminates. This tree diagnoses
        // enough GC hangs already; a loud, non-hanging failure is strictly
        // better.
        debug_assert!(g.active > 0, "ZGC mark: active worker count underflow");
        if g.active == 0 {
            tracing::error!(
                target: "zgc",
                cycle = my_cycle,
                "ZGC mark: worker offered termination with an already-zero active \
                 count; declaring the fixed point rather than wrapping the counter"
            );
            g.terminated = true;
            self.terminated_hint.store(true, Ordering::Release);
            self.wake.notify_all();
            return ZIdleOutcome::Terminated;
        }
        g.active -= 1;

        if g.active == 0 {
            // Last worker out. Nobody else can be holding a child in a
            // register, so a queue probe is now a complete picture of the
            // outstanding work — and it happens under this same lock, so a
            // publisher cannot slip between the probe and the decision
            // without first blocking on us.
            if stripes.has_work() || ingress.has_work() {
                g.active += 1;
                stats.termination_aborts.fetch_add(1, Ordering::Relaxed);
                // FINDING B (2026-08-07). Publish a wakeup as well as resuming
                // ourselves, or the pool can silently collapse to one worker.
                //
                // The n-1 workers parked in the loop below are waiting for
                // `work_generation` to move. Reaching this branch means work is
                // demonstrably queued that at least one of them has not been
                // told about — it re-read `work_generation` on the way in here,
                // so its `entry_work_gen` is the *current* value and its 5 ms
                // poll will keep re-reading that same value forever.
                //
                // Whether anybody else will bump it for us is not something
                // this branch can know. Sometimes yes: a mutator that pushed
                // into the ingress calls `note_work_published` immediately
                // after, so the gap is microseconds. Sometimes **no**: if the
                // publisher bumped the generation *before* the other workers
                // parked (worker publishes -> bumps -> a colleague whose steal
                // sweep already passed that stripe then parks at the new
                // generation), the bump has been spent and no further one is
                // pending. Those workers then stay parked until *we* publish,
                // which needs `local.len() >= 32` with our own stripe empty
                // (`maybe_share_early`) or `>= 256` (`publish_half`) — and on a
                // deep, narrow graph neither ever happens. The pool runs
                // single-threaded for the rest of the cycle. No correctness
                // loss, no hang, just the whole parallel mark quietly gone.
                //
                // Bumping here cannot storm: this branch is reachable only by
                // the *last* worker out (`active` 1 -> 0), and we immediately
                // put ourselves back to `active`, so `active` cannot reach 0
                // again until we too have drained and re-offered. A woken
                // colleague that finds nothing simply re-enters `worker_idle`,
                // captures the new generation, and parks — one extra round trip
                // per abort, and aborts are bounded by real publishes.
                //
                // Done inline rather than via `note_work_published` because we
                // already hold `state`, which is not a reentrant mutex.
                // `wrapping_add` matches that method exactly. `notify_all`
                // under the lock matches `arm`.
                g.work_generation = g.work_generation.wrapping_add(1);
                self.wake.notify_all();
                return ZIdleOutcome::Resume;
            }
            g.terminated = true;
            self.terminated_hint.store(true, Ordering::Release);
            stats.terminations.fetch_add(1, Ordering::Relaxed);
            self.wake.notify_all();
            tracing::debug!(
                target: "zgc",
                cycle = my_cycle,
                "ZGC concurrent mark reached a fixed point (all workers idle, all queues empty)"
            );
            return ZIdleOutcome::Terminated;
        }

        // Not the last one out: park until either somebody publishes, the
        // cycle ends, or the pool shuts down. `wait_for` (never a bare
        // `wait`) so a lost notification costs a poll interval instead of a
        // hang — this codebase has a documented history of GC livelocks that
        // present to the user as an unexplained freeze.
        loop {
            if should_stop.load(Ordering::Acquire) {
                return ZIdleOutcome::Terminated;
            }
            if g.cycle_generation != my_cycle || g.terminated {
                return ZIdleOutcome::Terminated;
            }
            if g.work_generation != entry_work_gen {
                g.active += 1;
                return ZIdleOutcome::Resume;
            }
            self.wake
                .wait_for(&mut g, Duration::from_millis(Z_MARK_PARK_POLL_MS));
        }
    }

    /// Block until the current cycle reaches a fixed point, or the pool shuts
    /// down.
    ///
    /// **Do not call this while the workers are paused for a safepoint** —
    /// they cannot make progress, so this cannot return.
    pub fn wait_for_fixed_point(&self, should_stop: &AtomicBool) {
        let mut g = self.state.lock();
        loop {
            if should_stop.load(Ordering::Acquire) || g.terminated {
                return;
            }
            let timed_out = self
                .wake
                .wait_for(&mut g, Duration::from_millis(Z_MARK_PARK_POLL_MS))
                .timed_out();
            if timed_out {
                // See `park_timeouts`: the timeout is insurance against a lost
                // notification, and insurance that is load-bearing is
                // indistinguishable from a working notification except in speed.
                self.park_timeouts.fetch_add(1, Ordering::Relaxed);
            } else if g.terminated {
                // Woken by the termination edge itself -- `g` is re-locked here,
                // so this is the current state and not a guess. See
                // `park_termination_wakes` for why "notified" alone is too weak.
                self.park_termination_wakes.fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    /// Waits this terminator's [`Self::wait_for_fixed_point`] has served by
    /// TIMING OUT rather than by being woken. See the field.
    pub fn park_timeouts(&self) -> u64 {
        self.park_timeouts.load(Ordering::Relaxed)
    }

    /// Zero the timeout count, for a terminator reused across cycles: the
    /// heap folds this into a process counter once per cycle, and a
    /// cumulative value would be re-added every time.
    pub fn reset_park_timeouts(&self) {
        self.park_timeouts.store(0, Ordering::Relaxed);
    }

    /// Waits released by the termination edge itself. See the field.
    pub fn park_termination_wakes(&self) -> u64 {
        self.park_termination_wakes.load(Ordering::Relaxed)
    }

    /// Wake everything blocked on this terminator, changing no state.
    ///
    /// For a caller that has just set a stop flag of its own and needs a waiter
    /// parked in [`Self::wait_for_fixed_point`] to observe it now rather than at
    /// the next poll interval. Spurious wakes are always safe here: every waiter
    /// re-checks its predicate under the lock in a loop.
    ///
    /// Deliberately NOT [`Self::note_work_published`], which is the other way to
    /// reach this condvar: that bumps `work_generation` and therefore resumes
    /// idle workers, which is precisely wrong for a stop.
    pub fn wake_blocked_waiters(&self) {
        let _g = self.state.lock();
        self.wake.notify_all();
    }

    /// Non-blocking read of the fixed-point flag. Lags; diagnostics only.
    pub fn is_terminated_hint(&self) -> bool {
        self.terminated_hint.load(Ordering::Acquire)
    }

    /// Authoritative read of the fixed-point flag (takes the lock).
    pub fn is_terminated(&self) -> bool {
        self.state.lock().terminated
    }

    /// Workers not currently offered for termination. Test/diagnostic hook —
    /// tests use it to construct the "one worker goes idle while another is
    /// mid-`visit_refs`" interleaving deliberately.
    pub fn active_workers(&self) -> usize {
        self.state.lock().active
    }

    /// Current cycle generation.
    pub fn cycle_generation(&self) -> u64 {
        self.state.lock().cycle_generation
    }

    /// Current work generation — the value a parked worker is waiting to see
    /// change.
    ///
    /// Diagnostic/test hook, and the **only** sound oracle for "was a wakeup
    /// published". [`ZMarkStats::yields`] is not: it is bumped at the drain
    /// loop's head only, so a worker resumed and re-parked before reaching that
    /// head yields without counting. This value moves under the state lock on
    /// every publish, so an observer that sees it change has a happens-before
    /// edge to whatever was queued first.
    pub fn work_generation(&self) -> u64 {
        self.state.lock().work_generation
    }

    /// Wake everything parked on this terminator (shutdown, or a phase
    /// transition the workers must observe).
    pub fn wake_all(&self) {
        let _g = self.state.lock();
        self.wake.notify_all();
    }
}

// ---------------------------------------------------------------------------
// ZMarkPauseControl
// ---------------------------------------------------------------------------

/// Safepoint yield/resume for the mark workers.
///
/// # Why this exists
///
/// A concurrent marker that does not stop promptly when the VM wants a
/// safepoint *is* a hang, from the operator's point of view. This tree has a
/// documented history of GC livelocks presenting as unexplained freezes
/// (young-GC trigger livelock under a non-moving sweep, and the STW-hang
/// watchdog that exists to diagnose them), so the yield path is explicit and
/// counted rather than implicit.
///
/// # Granularity
///
/// [`ZMarkWorker::drain`] tests [`pause_requested`](Self::pause_requested)
/// once per [`Z_MARK_YIELD_CHECK_INTERVAL`] (64) scanned objects, and
/// unconditionally at every loop-head between drain calls. Worst-case
/// time-to-yield is therefore
///
/// ```text
///   64 x cost(ZMarkContext::visit_refs)
/// ```
///
/// which for ordinary objects (a handful of slots) is a few microseconds.
/// **The known tail is a huge reference array**: `visit_refs` on a
/// million-element `Object[]` is one indivisible call, so it alone can
/// exceed the budget. OpenJDK solves this by splitting large object arrays
/// into fixed-size mark stripes; doing the same is the follow-up, and it
/// belongs in the [`ZMarkContext`] implementation (which owns array shape),
/// not here. Until then, a pathological array is the yield-latency bound and
/// should be named as such rather than discovered.
///
/// # The flag/counter race, and why these are `SeqCst`
///
/// Two threads each write one location and read the other:
///
/// ```text
///   coordinator                      worker
///   -----------                      ------
///   store(pause_flag, true)          fetch_add(in_drain, 1)
///   load(in_drain)                   load(pause_flag)
/// ```
///
/// With anything weaker than `SeqCst` on all four accesses, both may read the
/// stale value: the coordinator sees `in_drain == 0` and resumes mutators
/// while the worker, having seen `pause_flag == false`, walks into
/// `visit_refs`. `SeqCst` gives a single total order over these accesses, in
/// which at least one of the two must observe the other — the standard
/// Dekker argument. The worker therefore re-checks the flag *after*
/// incrementing and backs out if it lost the race. On x86-64 the increment is
/// a `lock xadd` and is already a full fence, so the cost is confined to the
/// two loads; on AArch64 it is a real `dmb ish`, paid once per drain call
/// (not per object).
#[derive(Debug, Default)]
pub struct ZMarkPauseControl {
    flag: AtomicBool,
    in_drain: AtomicUsize,
    paused: Mutex<bool>,
    resume: Condvar,
}

impl ZMarkPauseControl {
    /// A control in the running (not paused) state.
    pub fn new() -> Self {
        ZMarkPauseControl {
            flag: AtomicBool::new(false),
            in_drain: AtomicUsize::new(0),
            paused: Mutex::new(false),
            resume: Condvar::new(),
        }
    }

    /// Has a pause been requested? The drain loop's periodic check.
    #[inline]
    pub fn pause_requested(&self) -> bool {
        self.flag.load(Ordering::Acquire)
    }

    /// Try to enter the drain region. Returns `false` if a pause is pending,
    /// in which case the caller must not scan any object.
    #[inline]
    pub fn try_enter_drain(&self) -> bool {
        if self.flag.load(Ordering::Acquire) {
            return false;
        }
        // SeqCst: half of the Dekker pair documented on the type.
        self.in_drain.fetch_add(1, Ordering::SeqCst);
        if self.flag.load(Ordering::SeqCst) {
            self.in_drain.fetch_sub(1, Ordering::SeqCst);
            return false;
        }
        true
    }

    /// Leave the drain region.
    #[inline]
    pub fn leave_drain(&self) {
        self.in_drain.fetch_sub(1, Ordering::SeqCst);
    }

    /// Request a pause. Returns immediately; pair with
    /// [`wait_until_quiescent`](Self::wait_until_quiescent).
    pub fn request_pause(&self) {
        let mut g = self.paused.lock();
        *g = true;
        // SeqCst: the other half of the Dekker pair.
        self.flag.store(true, Ordering::SeqCst);
    }

    /// Spin until no worker is inside a drain region.
    ///
    /// A spin rather than a condvar, because the alternative would put a
    /// lock acquisition on the per-drain-call path purely to signal an event
    /// that happens once per safepoint. The wait is bounded in practice by
    /// the yield granularity documented above; it never times out, because
    /// giving up would mean resuming mutators while a marker is still
    /// dereferencing object headers.
    pub fn wait_until_quiescent(&self, should_stop: &AtomicBool) {
        let mut spins: u64 = 0;
        while self.in_drain.load(Ordering::SeqCst) != 0 {
            if should_stop.load(Ordering::Acquire) {
                return;
            }
            spins = spins.wrapping_add(1);
            if spins % Z_MARK_QUIESCE_WARN_SPINS == 0 {
                tracing::warn!(
                    target: "zgc",
                    spins,
                    in_drain = self.in_drain.load(Ordering::Relaxed),
                    "ZGC mark: still waiting for mark workers to reach a yield point; \
                     a ZMarkContext::visit_refs call is taking an unbounded amount of \
                     time (huge reference array, or a blocking implementation)"
                );
            }
            std::thread::yield_now();
        }
    }

    /// Release a pause and wake every parked worker.
    pub fn release_pause(&self) {
        let mut g = self.paused.lock();
        *g = false;
        self.flag.store(false, Ordering::SeqCst);
        self.resume.notify_all();
    }

    /// Worker side: block until the pause is released or the pool stops.
    pub fn wait_for_resume(&self, should_stop: &AtomicBool) {
        let mut g = self.paused.lock();
        while *g && !should_stop.load(Ordering::Acquire) {
            self.resume
                .wait_for(&mut g, Duration::from_millis(Z_MARK_PARK_POLL_MS));
        }
    }
}

// ---------------------------------------------------------------------------
// Shared pool state
// ---------------------------------------------------------------------------

/// Everything a mark worker and the coordinator share. One per
/// [`ZMarkCoordinator`]; never a `static`.
pub struct ZMarkShared {
    ctx: Arc<dyn ZMarkContext>,
    /// Per-cycle override of `ctx`. See `context_arc`.
    cycle_ctx: RwLock<Option<Arc<dyn ZMarkContext>>>,
    stripes: ZMarkStripeSet,
    ingress: ZMarkIngress,
    terminator: ZMarkTerminator,
    pause: ZMarkPauseControl,
    stats: ZMarkStats,
    should_stop: AtomicBool,
    /// `true` between [`ZMarkCoordinator::begin_cycle`] and
    /// [`ZMarkCoordinator::end_cycle`]. The load barrier reads this to decide
    /// whether to call into the ingress at all. `Release`/`Acquire` so a
    /// mutator that observes `true` also observes the armed terminator state.
    marking_active: AtomicBool,
    n_workers: usize,
}

impl std::fmt::Debug for ZMarkShared {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ZMarkShared")
            .field("n_workers", &self.n_workers)
            .field("stripes", &self.stripes.stripe_count())
            .field(
                "marking_active",
                &self.marking_active.load(Ordering::Relaxed),
            )
            .field("terminated", &self.terminator.is_terminated_hint())
            .finish()
    }
}

impl ZMarkShared {
    /// The heap-shape context this cycle is marking through.
    ///
    /// `None` in the slot means "use the one this pool was constructed with",
    /// which is every existing caller. The slot exists for a POOL THAT
    /// OUTLIVES ITS CYCLES: such a pool is owned by the heap, and on this
    /// backend the mark context IS the heap -- so a pool holding an `Arc` to
    /// it would keep the heap alive, the heap would never drop, and its worker
    /// threads would never be joined. Binding the context per cycle
    /// ([`ZMarkCoordinator::begin_cycle_with`]) and dropping it at `end_cycle`
    /// breaks that loop at the only point where it would otherwise close.
    #[inline]
    pub fn context_arc(&self) -> Arc<dyn ZMarkContext> {
        match self.cycle_ctx.read().as_ref() {
            Some(ctx) => Arc::clone(ctx),
            None => Arc::clone(&self.ctx),
        }
    }

    /// Bind (or clear) the context for one cycle.
    fn set_cycle_context(&self, ctx: Option<Arc<dyn ZMarkContext>>) {
        *self.cycle_ctx.write() = ctx;
    }

    /// The constructed context, borrowed.
    ///
    /// Does NOT see a per-cycle binding, so it is only for callers that know
    /// the pool was built with the context they mean -- everything on the
    /// per-cycle path uses [`Self::context_arc`].
    #[inline]
    pub fn context(&self) -> &dyn ZMarkContext {
        &*self.ctx
    }

    /// The shared work stripes.
    #[inline]
    pub fn stripes(&self) -> &ZMarkStripeSet {
        &self.stripes
    }

    /// The mutator hand-off.
    #[inline]
    pub fn ingress(&self) -> &ZMarkIngress {
        &self.ingress
    }

    /// The termination handshake.
    #[inline]
    pub fn terminator(&self) -> &ZMarkTerminator {
        &self.terminator
    }

    /// Safepoint yield control.
    #[inline]
    pub fn pause(&self) -> &ZMarkPauseControl {
        &self.pause
    }

    /// Telemetry.
    #[inline]
    pub fn stats(&self) -> &ZMarkStats {
        &self.stats
    }

    /// Is a mark cycle in flight?
    #[inline]
    pub fn is_marking(&self) -> bool {
        self.marking_active.load(Ordering::Acquire)
    }

    /// Publish a batch spread evenly across all stripes, then notify.
    ///
    /// Round-robin rather than "all into one stripe" so the pool starts with
    /// every worker able to find work in its *own* stripe — the first steal
    /// then only happens once the graph shape has actually made the load
    /// uneven, instead of immediately and always.
    fn publish_distributed(&self, addrs: Vec<u64>) {
        if addrs.is_empty() {
            return;
        }
        let n_stripes = self.stripes.stripe_count();
        let mut buckets: Vec<Vec<u64>> = (0..n_stripes).map(|_| Vec::new()).collect();
        for (i, addr) in addrs.into_iter().enumerate() {
            buckets[i % n_stripes].push(addr);
        }
        let mut published = 0usize;
        for (i, mut bucket) in buckets.into_iter().enumerate() {
            published += self.stripes.publish(i, &mut bucket);
        }
        if published > 0 {
            self.stats.stripe_publishes.fetch_add(1, Ordering::Relaxed);
            self.stats
                .published_objects
                .fetch_add(published as u64, Ordering::Relaxed);
        }
        // Stripe locks are released by now — safe to take the terminator's.
        self.terminator.note_work_published();
    }
}

// ---------------------------------------------------------------------------
// ZMarkHandle
// ---------------------------------------------------------------------------

/// A cheap, cloneable, `Send + Sync` handle for the **load barrier**.
///
/// This is the surface agent Z5's barrier module needs and the only surface it
/// needs: "here is an address I resolved; mark it if it is not already
/// marked". It deliberately exposes no way to start, stop or inspect a cycle.
#[derive(Clone, Debug)]
pub struct ZMarkHandle {
    shared: Arc<ZMarkShared>,
}

impl ZMarkHandle {
    /// Is a mark cycle in flight? The barrier's gate — when this is `false`
    /// the barrier must not call [`mark_live`](Self::mark_live), because the
    /// mark bits belong to a finished cycle.
    ///
    /// **The obligation is the barrier's and this is not enforced here**;
    /// [`mark_live`](Self::mark_live)'s "Whose job is the `is_marking()` gate"
    /// section records the decision, why a check on this side could not be
    /// authoritative, and how the violation is counted
    /// ([`ZMarkStats::late_marks`]) instead.
    #[inline]
    pub fn is_marking(&self) -> bool {
        self.shared.is_marking()
    }

    /// The current good mask, forwarded from the context.
    #[inline]
    pub fn good_mask(&self) -> u64 {
        self.shared.context_arc().good_mask()
    }

    /// A [`ZMarkMutatorBuffer`] that **flushes itself into this pool on drop**.
    ///
    /// Prefer this over [`ZMarkMutatorBuffer::new`] for anything stored in
    /// per-thread state. A buffer built here survives its owning thread's exit:
    /// its `Drop` moves the pending entries into the ingress, so an address
    /// whose mark bit is already set can never end up in no queue at all. See
    /// [`ZMarkMutatorBuffer`]'s type docs for why that state is a
    /// use-after-free and why a registry (`crate::satb`'s answer) is not the
    /// mechanism used here.
    ///
    /// `slot` selects an ingress bucket; pass a stable per-thread number.
    pub fn new_buffer(&self, slot: usize) -> ZMarkMutatorBuffer {
        ZMarkMutatorBuffer {
            slot,
            buf: Vec::with_capacity(Z_MARK_MUTATOR_BUFFER_CAPACITY),
            owner: Some(Arc::downgrade(&self.shared)),
        }
    }

    /// Mark `addr` live and, if it was not already marked, hand it to the
    /// markers.
    ///
    /// Returns `true` iff this call newly marked the object. The filter is
    /// here rather than in the caller because *not* filtering is a
    /// correctness-preserving performance disaster: a hot field read on an
    /// already-marked object would enqueue it on every single load.
    ///
    /// `slot` selects an ingress bucket; pass a stable per-thread number.
    ///
    /// # ⚠ `addr` is a MACHINE ADDRESS — the load barrier wants
    /// [`mark_live_offset`](Self::mark_live_offset)
    ///
    /// *Added 2026-08-07, second reconciliation pass.* `crate::zgc::barrier`
    /// hands `ZBarrierContext::mark_live` a bare 42-bit **heap offset**. Passing
    /// one here does not fail loudly: [`ZMarkContext::is_in_heap`] refuses it as
    /// a wild pointer, this method returns `false`, an
    /// [`off_heap_children`](ZMarkStats::off_heap_children) is counted, and the
    /// object is swept while live. Use
    /// [`mark_live_offset`](Self::mark_live_offset) from any barrier adapter;
    /// the module header's "Address domain" section is the full argument.
    ///
    /// # Whose job is the `is_marking()` gate? (decided 2026-08-07)
    ///
    /// This method deliberately does **not** check
    /// [`is_marking`](Self::is_marking), so a call that lands after
    /// [`ZMarkCoordinator::end_cycle`] does set a mark bit belonging to a
    /// finished cycle. That was previously undocumented in either direction;
    /// the decision is recorded here.
    ///
    /// **The gate belongs to the caller — the load barrier — and it is a
    /// filter, not a guarantee.** Three reasons, in order of weight:
    ///
    /// 1. **The barrier already has a better gate than a flag: the colour
    ///    mask.** `crate::zgc::barrier`'s header makes this its central design
    ///    point — when marking is off the marked colour *is* a good colour, the
    ///    fast path hits, and `ZBarrierContext::mark_live` is never reached at
    ///    all. That is a data-dependent gate on the value in the slot, and it
    ///    is why the ZGC barrier needs no `ACTIVE`/`DRAINING` tri-state the way
    ///    `crate::satb`'s write barrier does. `load_barrier_slow` additionally
    ///    tests `ctx.is_marking()` before calling through. Adding a *third*
    ///    check here would be the weakest of the three.
    /// 2. **A check here could not be authoritative anyway.** `is_marking()`
    ///    then `try_mark` is check-then-act across two unsynchronised steps;
    ///    `end_cycle` can land in between however the branch is written. A gate
    ///    that reads as a guarantee but is a race is worse than no gate — this
    ///    tree already has the scar tissue for that pattern.
    /// 3. **The failure is bounded to floating garbage, by the phase protocol
    ///    rather than by any gate.** A late call leaves (a) a stale mark bit and
    ///    (b) a stale ingress entry. Both are discarded, and neither needs
    ///    anyone to notice: [`ZMarkCoordinator::begin_cycle`] clears the ingress
    ///    and the stripes outright, and the mark bit belongs to *this* cycle's
    ///    colour — the next cycle flips to the other one
    ///    ([`ZColor::Marked0`]/[`ZColor::Marked1`], see
    ///    [`mark_color_for`]), which is exactly why ZGC has two mark bits
    ///    instead of one. The one thing that must not happen is a late call
    ///    racing a *sweep* that is reading this cycle's mark bits, and that is
    ///    the heap's phase discipline — it is not something this handle could
    ///    enforce even if it wanted to.
    ///
    /// What this method does do is **count** the violation:
    /// [`ZMarkStats::late_marks`] is bumped whenever a call newly marks an
    /// object with no cycle in flight, so a barrier that gets the gate wrong is
    /// visible in telemetry instead of silent. The counter is read on the rare
    /// path (a *newly* marked object) so it costs nothing on the hot
    /// already-marked path.
    pub fn mark_live(&self, slot: usize, addr: u64) -> bool {
        if addr == 0 {
            return false;
        }
        let ctx_arc = self.shared.context_arc();
        let ctx = &*ctx_arc;
        if !ctx.is_in_heap(addr) {
            self.shared
                .stats
                .off_heap_children
                .fetch_add(1, Ordering::Relaxed);
            return false;
        }
        if !ctx.try_mark(addr) {
            return false;
        }
        self.note_if_late();
        self.shared.ingress.push(slot, addr);
        self.shared
            .stats
            .ingress_pushes
            .fetch_add(1, Ordering::Relaxed);
        // Ingress lock released inside `push`; safe to take the terminator's.
        self.shared.terminator.note_work_published();
        true
    }

    // -- the offset domain: the load barrier's boundary ----------------------
    //
    // Added 2026-08-07 (second reconciliation pass). The naming deliberately
    // matches `ZRelocate::forward_offset` / `forward_lookup_offset`: this
    // subsystem now has exactly one spelling for "the argument is a heap
    // offset, not a machine address", and it is a suffix on the method name.

    /// The origin of the barrier's offset domain for this pool's context, or
    /// `None` if the context declares none. See [`ZMarkContext::heap_base`].
    #[inline]
    pub fn heap_base(&self) -> Option<u64> {
        self.shared.context_arc().heap_base()
    }

    /// The release-mode tripwire and the conversion, in one place.
    ///
    /// Returns the machine address `heap_base + offset`, or `None` after
    /// logging and counting a [`ZMarkStats::domain_refusals`].
    ///
    /// # Why this fires in release builds
    ///
    /// The same argument `barrier.rs` and `relocate.rs` both make, restated
    /// because the instinct is to reach for a bare `debug_assert!`: a
    /// wrong-domain value is a Linux heap address near `0x7f…`, five bits past
    /// the 42-bit offset field, and it is *invisible* on the Windows dev host
    /// where the reservation is placed low. A debug-only check therefore runs
    /// only on the platform where the bug cannot be seen. This tree has been
    /// bitten by exactly that shape before
    /// (`release-test-runs-had-lock-order-enforcement-off`). The
    /// `tracing::error!` is the half that survives `--release`; the
    /// `debug_assert!` turns it into a test failure where asserts are on.
    ///
    /// The predicate is `barrier::is_bare_offset` itself, not a local copy, so
    /// the two modules cannot drift on what "an offset" means.
    fn machine_address_of_offset(&self, what: &'static str, offset: u64) -> Option<u64> {
        if !is_bare_offset(offset, Z_OFFSET_MASK) {
            let stray_bits = offset & !Z_OFFSET_MASK;
            self.shared
                .stats
                .domain_refusals
                .fetch_add(1, Ordering::Relaxed);
            tracing::error!(
                target: "zgc::mark",
                method = what,
                value = offset,
                stray_bits = stray_bits,
                offset_mask = Z_OFFSET_MASK,
                "ZMarkHandle: a value handed to an _offset entry point carries bits above \
                 the 42-bit offset field — it is a machine address (or is still coloured). \
                 The machine-address entry point is ZMarkHandle::mark_live. See the module \
                 header, \"Address domain\""
            );
            debug_assert!(
                is_bare_offset(offset, Z_OFFSET_MASK),
                "ZMarkHandle::{what}: {offset:#x} is not a bare heap offset \
                 (stray bits {stray_bits:#x})"
            );
            return None;
        }
        let Some(base) = self.shared.context_arc().heap_base() else {
            self.shared
                .stats
                .domain_refusals
                .fetch_add(1, Ordering::Relaxed);
            tracing::error!(
                target: "zgc::mark",
                method = what,
                value = offset,
                "ZMarkHandle: the load barrier called an _offset entry point on a \
                 ZMarkContext that declares no heap_base, so there is no way to turn this \
                 offset into the machine address try_mark needs. REFUSING rather than \
                 guessing a base: a guess would mark the wrong object, or none. Implement \
                 ZMarkContext::heap_base, or route the barrier elsewhere"
            );
            debug_assert!(
                false,
                "ZMarkHandle::{what}: ZMarkContext::heap_base() is None; the barrier's \
                 offset domain is not wired to this heap"
            );
            return None;
        };
        let Some(addr) = base.checked_add(offset) else {
            self.shared
                .stats
                .domain_refusals
                .fetch_add(1, Ordering::Relaxed);
            tracing::error!(
                target: "zgc::mark",
                method = what,
                value = offset,
                heap_base = base,
                "ZMarkHandle: heap_base + offset overflows u64; the declared heap_base \
                 cannot be the origin of this offset"
            );
            debug_assert!(
                false,
                "ZMarkHandle::{what}: heap_base {base:#x} + offset {offset:#x} overflows"
            );
            return None;
        };
        Some(addr)
    }

    /// **The load barrier's entry point.** [`mark_live`](Self::mark_live) in the
    /// **heap-offset domain**.
    ///
    /// `offset` is a bare 42-bit offset from [`ZMarkContext::heap_base`] —
    /// exactly what `ZBarrierContext::mark_live` is handed, and exactly what
    /// [`mark_live`](Self::mark_live) must **not** be handed. Returns `true` iff
    /// this call newly marked the object, and `false` for every refusal.
    ///
    /// # Why this exists (2026-08-07, second reconciliation pass)
    ///
    /// A cross-module audit found that `barrier.rs`'s slow path calls
    /// `ZBarrierContext::mark_live(destination)` with a bare offset while this
    /// module's trait declares machine addresses and the production
    /// implementation dereferences them. The first symptom of wiring the two
    /// together naively is **not a crash**: [`ZMarkContext::is_in_heap`] is a
    /// registry membership test, an offset is not a registered object base, so
    /// [`mark_live`](Self::mark_live) refuses it, counts an
    /// [`off_heap_children`](ZMarkStats::off_heap_children), returns `false` —
    /// and the object the barrier was keeping alive is swept. Silent, and
    /// indistinguishable from correct wild-pointer refusal.
    ///
    /// This method is the boundary, and it is on this side because the barrier
    /// has neither a heap base nor room in its encoding for a machine address.
    /// The module header's "Address domain" section carries the full argument.
    ///
    /// # Refusals
    ///
    /// A value that is not a bare offset, or a context with no
    /// [`heap_base`](ZMarkContext::heap_base), is refused **loudly** — a
    /// release-surviving `tracing::error!`, a `debug_assert!`, and a
    /// [`ZMarkStats::domain_refusals`] bump — and nothing is marked. The
    /// refusal is deliberately never silent and deliberately not pooled with
    /// `off_heap_children`.
    ///
    /// # Offset `0`
    ///
    /// Offset `0` is a **real heap location** in the barrier's domain — that is
    /// stated at length on `ZBarrierContext::forward`, and `vaddr::color(0, c)`
    /// is a well-formed non-null word. It survives this boundary intact for any
    /// `heap_base != 0`, because what reaches [`mark_live`](Self::mark_live) is
    /// `heap_base + 0`. A context that declares `heap_base == Some(0)` collapses
    /// it onto this engine's null address and the mark is dropped; such a
    /// context has the barrier's offsets and its machine addresses numerically
    /// identical, so it should implement `heap_base` only if that is genuinely
    /// true of its heap.
    ///
    /// `slot` selects an ingress bucket; pass a stable per-thread number.
    pub fn mark_live_offset(&self, slot: usize, offset: u64) -> bool {
        match self.machine_address_of_offset("mark_live_offset", offset) {
            Some(addr) => self.mark_live(slot, addr),
            None => false,
        }
    }

    /// Record a call that newly marked an object with no cycle in flight. See
    /// [`mark_live`](Self::mark_live)'s "Whose job is the `is_marking()` gate"
    /// section — this is telemetry, and it deliberately does not change the
    /// call's behaviour.
    #[inline]
    fn note_if_late(&self) {
        if !self.shared.is_marking() {
            self.shared.stats.late_marks.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Buffered variant of [`mark_live`](Self::mark_live) for a caller that
    /// owns a [`ZMarkMutatorBuffer`]. Flushes automatically when the buffer
    /// fills.
    ///
    /// # The mark bit is set here; the address leaves later
    ///
    /// `try_mark` below sets the bit **before** `buf.push` stores the address,
    /// and the address does not reach [`ZMarkIngress`] until the buffer fills
    /// or is flushed. Until then the object is marked (so no future `try_mark`
    /// can rediscover it) and in no queue (so no worker will scan it). Two
    /// things discharge that:
    ///
    /// * while the thread lives, the safepoint contract on
    ///   [`ZMarkCoordinator::try_end_mark`] — flush every buffer before the
    ///   mark-end probe;
    /// * when the thread dies, `buf`'s own [`Drop`], **provided the buffer came
    ///   from [`new_buffer`](Self::new_buffer)**. A detached
    ///   [`ZMarkMutatorBuffer::new`] buffer has nothing to flush into and its
    ///   entries are lost, which is a use-after-free of everything reachable
    ///   only through them.
    ///
    /// The `is_marking()` discussion on [`mark_live`](Self::mark_live) applies
    /// verbatim here, including [`ZMarkStats::late_marks`] — and so does its
    /// "`addr` is a MACHINE ADDRESS" warning: a load barrier holds heap
    /// **offsets** and wants
    /// [`mark_live_buffered_offset`](Self::mark_live_buffered_offset).
    pub fn mark_live_buffered(&self, buf: &mut ZMarkMutatorBuffer, addr: u64) -> bool {
        if addr == 0 {
            return false;
        }
        let ctx_arc = self.shared.context_arc();
        let ctx = &*ctx_arc;
        if !ctx.is_in_heap(addr) {
            self.shared
                .stats
                .off_heap_children
                .fetch_add(1, Ordering::Relaxed);
            return false;
        }
        if !ctx.try_mark(addr) {
            return false;
        }
        self.note_if_late();
        if buf.push(addr) {
            self.flush_buffer(buf);
        }
        true
    }

    /// **The load barrier's buffered entry point.**
    /// [`mark_live_buffered`](Self::mark_live_buffered) in the **heap-offset
    /// domain**; the twin of [`mark_live_offset`](Self::mark_live_offset).
    ///
    /// Everything on [`mark_live_offset`](Self::mark_live_offset) applies
    /// verbatim — the domain check, the refusal counter, offset `0` — and so
    /// does everything on
    /// [`mark_live_buffered`](Self::mark_live_buffered), including the rule that
    /// `buf` must have come from [`new_buffer`](Self::new_buffer) or a dying
    /// thread's entries are lost.
    ///
    /// Note the ordering that makes the domain check load-bearing here rather
    /// than merely tidy: `mark_live_buffered` sets the mark bit **before** the
    /// address reaches the ingress. A wrong-domain value refused at the
    /// boundary therefore costs nothing, whereas one that got past the boundary
    /// and was then refused by [`ZMarkContext::is_in_heap`] would look
    /// identical to a mutator race in the telemetry.
    pub fn mark_live_buffered_offset(&self, buf: &mut ZMarkMutatorBuffer, offset: u64) -> bool {
        match self.machine_address_of_offset("mark_live_buffered_offset", offset) {
            Some(addr) => self.mark_live_buffered(buf, addr),
            None => false,
        }
    }

    /// Move a mutator buffer's contents into the ingress.
    ///
    /// The safepoint protocol must call this for every mutator thread before
    /// [`ZMarkCoordinator::try_end_mark`], exactly as `g1_concurrent`'s
    /// remark calls [`crate::satb::flush_thread_satb_buffer`]: an address
    /// sitting in a per-thread buffer is invisible to the mark-end probe, and
    /// a mark-end probe that misses it declares the cycle complete with a
    /// live object unscanned.
    ///
    /// This covers **live** threads only. A thread that exits between two
    /// safepoints is covered by the buffer's own `Drop`, and only if the buffer
    /// was built by [`new_buffer`](Self::new_buffer) — see
    /// [`ZMarkMutatorBuffer`]'s "Thread death is a use-after-free" section.
    ///
    // TODO(zgc_concurrent): `gc/src/zgc_concurrent.rs` still tells implementors
    // of `ZgcMarkSafepoint::flush_mutator_buffers` to hold a
    // `ZMarkMutatorBuffer` without saying which constructor, at two places:
    // the module header's numbered step 2 (`zgc_concurrent.rs:137-144`) and the
    // trait's `# Contract` bullet (`zgc_concurrent.rs:276-279`). Both should
    // name `ZMarkHandle::new_buffer` and add the clause "a thread that exits
    // between safepoints is covered by the buffer's Drop, but ONLY for an
    // attached buffer; `ZMarkMutatorBuffer::new` is covered by nothing". That
    // file is owned by another agent in this effort, so the edit is recorded
    // here rather than made.
    pub fn flush_buffer(&self, buf: &mut ZMarkMutatorBuffer) -> usize {
        let slot = buf.slot;
        let n = self.shared.ingress.push_batch(slot, &mut buf.buf);
        if n > 0 {
            self.shared
                .stats
                .ingress_pushes
                .fetch_add(n as u64, Ordering::Relaxed);
            self.shared.terminator.note_work_published();
        }
        n
    }
}

// ---------------------------------------------------------------------------
// ZMarkWorker
// ---------------------------------------------------------------------------

/// One mark thread's state machine.
///
/// Public so tests (and, later, a foreground "help the collector" path on a
/// mutator thread) can drive one directly; the pool owns them normally.
pub struct ZMarkWorker {
    id: usize,
    shared: Arc<ZMarkShared>,
    local: ZMarkLocalStack,
    /// xorshift64 state for victim selection. Per-worker, seeded from the
    /// coordinator's own allocation address — instance-derived, so two heaps
    /// in one process do not share a sequence and no global counter exists to
    /// contend on.
    rng: u64,
    last_cycle: u64,
}

impl std::fmt::Debug for ZMarkWorker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ZMarkWorker")
            .field("id", &self.id)
            .field("local_len", &self.local.len())
            .field("last_cycle", &self.last_cycle)
            .finish()
    }
}

impl ZMarkWorker {
    /// Build a worker. `seed` must be non-zero (xorshift is degenerate at 0);
    /// the constructor forces the low bit to guarantee it.
    pub fn new(id: usize, shared: Arc<ZMarkShared>, seed: u64) -> Self {
        ZMarkWorker {
            id,
            shared,
            local: ZMarkLocalStack::new(),
            rng: seed | 1,
            last_cycle: 0,
        }
    }

    /// This worker's index.
    #[inline]
    pub fn id(&self) -> usize {
        self.id
    }

    #[inline]
    fn next_random(&mut self) -> usize {
        // xorshift64. Not a cryptographic PRNG and does not need to be; it
        // needs to be cheap and to decorrelate victim choices.
        let mut x = self.rng;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.rng = x;
        (x >> 16) as usize
    }

    /// The thread body: join cycles, drain them, park between them.
    pub fn run(&mut self) {
        let shared = Arc::clone(&self.shared);
        shared.context_arc().on_worker_start(self.id);
        loop {
            if shared.should_stop.load(Ordering::Acquire) {
                break;
            }
            let my_cycle = match shared
                .terminator
                .wait_for_cycle(self.last_cycle, &shared.should_stop)
            {
                Some(c) => c,
                None => break,
            };
            self.last_cycle = my_cycle;

            // Defensive: a previous cycle should always have left this empty
            // (the handshake is only reachable with an empty local stack),
            // but if a shutdown or an abort ever leaves work behind, publish
            // it rather than dropping it. Dropping mark work is a
            // use-after-free; publishing it is at worst floating garbage.
            if !self.local.is_empty() {
                let mut leftovers = self.local.take_all();
                shared.stripes.publish(self.id, &mut leftovers);
                shared.terminator.note_work_published();
            }

            self.run_cycle(&shared, my_cycle);
        }
        shared.context_arc().on_worker_end(self.id);
    }

    /// Drain one cycle to its fixed point.
    fn run_cycle(&mut self, shared: &Arc<ZMarkShared>, my_cycle: u64) {
        loop {
            if shared.should_stop.load(Ordering::Acquire) {
                return;
            }
            if shared.pause.pause_requested() {
                shared.stats.yields.fetch_add(1, Ordering::Relaxed);
                shared.pause.wait_for_resume(&shared.should_stop);
                continue;
            }

            let scanned = self.drain(Z_MARK_DRAIN_BUDGET);
            // A non-empty local stack after a zero-scan drain means the drain
            // bailed for a pause. Go round the loop so the pause branch above
            // handles it; do NOT fall through to the handshake, which would
            // offer termination while still holding work.
            if scanned > 0 || !self.local.is_empty() {
                continue;
            }
            if self.acquire_work() {
                continue;
            }
            match shared.terminator.worker_idle(
                my_cycle,
                &shared.stripes,
                &shared.ingress,
                &shared.stats,
                &shared.should_stop,
            ) {
                ZIdleOutcome::Resume => continue,
                ZIdleOutcome::Terminated => return,
            }
        }
    }

    /// Scan up to `budget` objects from the local stack. Returns how many
    /// were scanned; `0` means either "local stack empty" or "yielded".
    pub fn drain(&mut self, budget: usize) -> usize {
        if !self.shared.pause.try_enter_drain() {
            return 0;
        }
        let shared = Arc::clone(&self.shared);
        let ctx_arc = shared.context_arc();
        let ctx: &dyn ZMarkContext = &*ctx_arc;
        let stripes = &shared.stripes;
        let stats = &shared.stats;
        let pause = &shared.pause;
        let terminator = &shared.terminator;
        let id = self.id;

        // Share before descending, so idle colleagues have something to take
        // while this worker is deep in its own subtree.
        Self::maybe_share_early(&mut self.local, id, stripes, terminator, stats);

        let mut scanned: usize = 0;
        let mut marked_here: u64 = 0;
        let mut off_heap_here: u64 = 0;
        while scanned < budget {
            if scanned % Z_MARK_YIELD_CHECK_INTERVAL == 0 && pause.pause_requested() {
                break;
            }
            let addr = match self.local.pop() {
                Some(a) => a,
                None => break,
            };
            scanned += 1;

            {
                let local = &mut self.local;
                // COUNTED LOCALLY, folded once per drain below. These two are
                // shared by every worker, so a `fetch_add` per edge is a
                // contended write to one cache line on the hottest line of the
                // whole collector -- the sort of shared counter that makes a
                // parallel marker scale negatively (see this module.s header).
                let marked = &mut marked_here;
                let off_heap = &mut off_heap_here;
                ctx.visit_refs(addr, &mut |child: u64| {
                    if child == 0 {
                        return;
                    }
                    if !ctx.is_in_heap(child) {
                        // Raw reference-slot bytes can be anything at all
                        // when a mutator is mid-store; refuse rather than
                        // dereference. `ZgcRealHeap::collect_garbage` keeps
                        // the same counter under the name `wild_skipped`.
                        *off_heap += 1;
                        return;
                    }
                    if ctx.try_mark(child) {
                        *marked += 1;
                        local.push(child);
                    }
                });
            }

            if self.local.len() >= Z_MARK_LOCAL_HIGH_WATER {
                Self::publish_half(&mut self.local, id, stripes, terminator, stats);
            }
        }

        if scanned > 0 {
            stats
                .objects_scanned
                .fetch_add(scanned as u64, Ordering::Relaxed);
        }
        if marked_here > 0 {
            stats.objects_marked.fetch_add(marked_here, Ordering::Relaxed);
        }
        if off_heap_here > 0 {
            stats
                .off_heap_children
                .fetch_add(off_heap_here, Ordering::Relaxed);
        }
        pause.leave_drain();
        scanned
    }

    /// Publish half the local stack to this worker's own stripe.
    fn publish_half(
        local: &mut ZMarkLocalStack,
        id: usize,
        stripes: &ZMarkStripeSet,
        terminator: &ZMarkTerminator,
        stats: &ZMarkStats,
    ) {
        let mut chunk = local.split_off_oldest_half();
        if chunk.is_empty() {
            return;
        }
        let n = stripes.publish(id, &mut chunk);
        stats.stripe_publishes.fetch_add(1, Ordering::Relaxed);
        stats
            .published_objects
            .fetch_add(n as u64, Ordering::Relaxed);
        // Stripe lock is released; taking the terminator lock here is the
        // legal order. Never call this with a stripe guard alive.
        terminator.note_work_published();
    }

    /// If this worker has a reasonable backlog and its own stripe is empty,
    /// publish half now rather than waiting for the high-water mark.
    fn maybe_share_early(
        local: &mut ZMarkLocalStack,
        id: usize,
        stripes: &ZMarkStripeSet,
        terminator: &ZMarkTerminator,
        stats: &ZMarkStats,
    ) {
        if local.len() < Z_MARK_LOCAL_SHARE_MIN {
            return;
        }
        if !stripes.is_stripe_empty(stripes.index_for(id)) {
            return;
        }
        Self::publish_half(local, id, stripes, terminator, stats);
    }

    /// Refill the local stack from somewhere. Returns `true` if it found
    /// anything.
    ///
    /// # Victim selection, and why it is random
    ///
    /// The order is: **own stripe → up to [`Z_MARK_STEAL_ATTEMPTS`] uniformly
    /// random victims → a deterministic sweep of every stripe → the mutator
    /// ingress.**
    ///
    /// *Own stripe first* because it is this worker's own overflow: warm in
    /// cache, and by construction the least contended stripe for this thread.
    ///
    /// *Random victims next, rather than round-robin, to avoid convoying.*
    /// With a fixed probe order every idle worker looks at the same stripe at
    /// the same time. One wins; the rest block on that one mutex, wake in
    /// turn, find it empty, and move to the next stripe — *together*. The
    /// pool stays phase-locked, an O(1) steal degrades to O(W) serialised
    /// lock handoffs, and the convoy persists because losing a race is what
    /// synchronises the losers. Independent random draws decorrelate the
    /// probes: two thieves collide with probability `1/S`, and a collision
    /// does not make their *next* choices agree.
    ///
    /// *The deterministic full sweep last* is the correctness backstop.
    /// Random probing can miss a non-empty stripe by luck, and "I found
    /// nothing" is the precondition for offering termination — so before a
    /// worker is allowed to say that, it must have looked everywhere. The
    /// sweep can convoy, but it only runs when the pool is nearly drained,
    /// where being right beats being fast.
    ///
    /// *The ingress last of all* keeps mutator hand-off traffic off the steal
    /// path entirely in steady state.
    pub fn acquire_work(&mut self) -> bool {
        let shared = Arc::clone(&self.shared);
        let stripes = &shared.stripes;
        let stats = &shared.stats;
        let own = stripes.index_for(self.id);
        let n_stripes = stripes.stripe_count();

        // 1. Own stripe: take everything (up to the cap).
        {
            let got = stripes.drain_from(own, self.local.buf_mut(), Z_MARK_STEAL_CAP);
            if got > 0 {
                stats.own_stripe_refills.fetch_add(1, Ordering::Relaxed);
                return true;
            }
        }

        // 2. Random victims.
        for _ in 0..Z_MARK_STEAL_ATTEMPTS {
            let victim = stripes.index_for(self.next_random());
            if victim == own {
                continue;
            }
            stats.steal_attempts.fetch_add(1, Ordering::Relaxed);
            let got = stripes.steal_half(victim, self.local.buf_mut(), Z_MARK_STEAL_CAP);
            if got > 0 {
                stats.steals_succeeded.fetch_add(1, Ordering::Relaxed);
                stats
                    .stolen_objects
                    .fetch_add(got as u64, Ordering::Relaxed);
                return true;
            }
        }

        // 3. Deterministic sweep — the "I really did look everywhere" pass.
        for offset in 1..=n_stripes {
            let victim = stripes.index_for(own.wrapping_add(offset));
            if victim == own {
                continue;
            }
            stats.steal_attempts.fetch_add(1, Ordering::Relaxed);
            let got = stripes.steal_half(victim, self.local.buf_mut(), Z_MARK_STEAL_CAP);
            if got > 0 {
                stats.steals_succeeded.fetch_add(1, Ordering::Relaxed);
                stats
                    .stolen_objects
                    .fetch_add(got as u64, Ordering::Relaxed);
                return true;
            }
        }

        // 4. Mutator hand-off, last.
        let got = shared.ingress.drain_into(self.local.buf_mut());
        if got > 0 {
            stats.ingress_drains.fetch_add(1, Ordering::Relaxed);
            stats
                .ingress_objects
                .fetch_add(got as u64, Ordering::Relaxed);
            return true;
        }

        false
    }
}

// ---------------------------------------------------------------------------
// ZMarkCoordinator
// ---------------------------------------------------------------------------

/// What the mark-end safepoint decided.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ZMarkEndResult {
    /// No work remained after every mutator buffer was flushed. Marking is
    /// final; the caller may flip to `MarkComplete`.
    Complete,
    /// Flushing the mutator hand-off produced work. Marking is **not** done;
    /// the caller must run the concurrent phase again.
    Restart,
}

/// Summary of one [`ZMarkCoordinator::mark_to_completion`] run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ZMarkCycleReport {
    /// How many concurrent phases ran (1 = no restarts were needed).
    pub passes: usize,
    /// How many of those were forced by a non-empty mark-end flush.
    pub restarts: usize,
    /// `true` if the restart budget was exhausted rather than converging.
    pub budget_exhausted: bool,
    /// Counters at the end of the run.
    pub stats: ZMarkStatsSnapshot,
}

/// RAII pause of the mark workers, for a stop-the-world phase.
///
/// On construction every worker has left its drain region; on drop they
/// resume. Holding this while calling
/// [`ZMarkCoordinator::wait_for_fixed_point`] deadlocks by construction — the
/// workers cannot make progress — so don't.
#[must_use = "dropping the guard immediately resumes the mark workers"]
pub struct ZMarkPauseGuard<'a> {
    shared: &'a ZMarkShared,
}

impl Drop for ZMarkPauseGuard<'_> {
    fn drop(&mut self) {
        self.shared.pause.release_pause();
    }
}

/// The mark worker pool and the phase API.
///
/// # Lifecycle
///
/// ```text
///   new(ctx, n)                      threads spawned, parked
///        |
///   begin_cycle()                    marking_active = true
///   push_roots(&roots)               STW mark-start: seed the stripes
///        |
///   +--> start_marking()             arm: workers join
///   |    wait_for_fixed_point()      concurrent; mutators run
///   |    [safepoint: flush every mutator buffer]
///   |    try_end_mark()
///   |        |-- Restart ------------+
///   |        `-- Complete
///   |
///   process_non_strong_refs(hook)    (then loop back once more)
///   end_cycle()                      marking_active = false
///   shutdown()
/// ```
///
/// [`mark_to_completion`](Self::mark_to_completion) packages the inner loop.
/// A context that marks nothing: what a POOL OUTLIVING ITS CYCLES is built
/// with, so it holds no heap between them.
///
/// Every method is the refusing answer, and `is_in_heap` returning `false` is
/// the load-bearing one -- a worker that somehow drained with no cycle bound
/// discards its work rather than dereferencing an address on behalf of a heap
/// that may no longer exist.
#[derive(Debug, Default)]
pub struct ZInertMarkContext;

impl ZMarkContext for ZInertMarkContext {
    fn good_mask(&self) -> u64 {
        0
    }
    fn try_mark(&self, _addr: u64) -> bool {
        false
    }
    fn is_marked(&self, _addr: u64) -> bool {
        false
    }
    fn visit_refs(&self, _addr: u64, _f: &mut dyn FnMut(u64)) {}
    fn is_in_heap(&self, _addr: u64) -> bool {
        false
    }
}

pub struct ZMarkCoordinator {
    shared: Arc<ZMarkShared>,
    handles: Vec<JoinHandle<()>>,
}

impl std::fmt::Debug for ZMarkCoordinator {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ZMarkCoordinator")
            .field("shared", &self.shared)
            .field("threads", &self.handles.len())
            .finish()
    }
}

impl ZMarkCoordinator {
    /// Spawn `n_workers` mark threads (minimum 1) over `ctx`.
    ///
    /// The threads start parked: no cycle is armed until
    /// [`start_marking`](Self::start_marking).
    pub fn new(ctx: Arc<dyn ZMarkContext>, n_workers: usize) -> Self {
        let n = n_workers.max(1);
        let stripe_count = stripe_count_for(n);
        let shared = Arc::new(ZMarkShared {
            ctx,
            cycle_ctx: RwLock::new(None),
            stripes: ZMarkStripeSet::new(stripe_count),
            ingress: ZMarkIngress::new(),
            terminator: ZMarkTerminator::new(n),
            pause: ZMarkPauseControl::new(),
            stats: ZMarkStats::default(),
            should_stop: AtomicBool::new(false),
            marking_active: AtomicBool::new(false),
            n_workers: n,
        });

        // Instance-derived PRNG nonce: the allocation address of this pool's
        // shared state. Deliberately not a process-global counter.
        let nonce = Arc::as_ptr(&shared) as usize as u64;

        let mut handles = Vec::with_capacity(n);
        for id in 0..n {
            let worker_shared = Arc::clone(&shared);
            let seed = ((id as u64).wrapping_add(1)).wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ nonce;
            let handle = std::thread::Builder::new()
                .name(format!("zgc-mark-{id}"))
                .spawn(move || {
                    let mut worker = ZMarkWorker::new(id, worker_shared, seed);
                    worker.run();
                })
                .expect("zgc-mark worker thread spawn failed");
            handles.push(handle);
        }

        tracing::debug!(
            target: "zgc",
            workers = n,
            stripes = stripe_count,
            "ZGC concurrent mark pool started"
        );

        ZMarkCoordinator { shared, handles }
    }

    /// The shared state, for a sibling module that needs deeper access than
    /// [`handle`](Self::handle) gives.
    #[inline]
    pub fn shared(&self) -> &Arc<ZMarkShared> {
        &self.shared
    }

    /// A cloneable handle for the load barrier.
    #[inline]
    pub fn handle(&self) -> ZMarkHandle {
        ZMarkHandle {
            shared: Arc::clone(&self.shared),
        }
    }

    /// Counters.
    #[inline]
    pub fn stats(&self) -> &ZMarkStats {
        &self.shared.stats
    }

    /// Number of worker threads.
    #[inline]
    pub fn worker_count(&self) -> usize {
        self.shared.n_workers
    }

    /// Number of stripes.
    #[inline]
    pub fn stripe_count(&self) -> usize {
        self.shared.stripes.stripe_count()
    }

    /// Workers not currently offered for termination. Diagnostic/test hook.
    #[inline]
    pub fn active_workers(&self) -> usize {
        self.shared.terminator.active_workers()
    }

    // -- phases -------------------------------------------------------------

    /// Open a mark cycle. Call at the mark-start safepoint, after the good
    /// mask has been flipped with
    /// [`ZGoodMask::flip_to_mark`](crate::zgc::vaddr::ZGoodMask::flip_to_mark).
    ///
    /// Clears any residue from a previous cycle and opens the ingress so the
    /// load barrier may begin handing addresses over.
    pub fn begin_cycle(&self) {
        self.shared.stripes.clear();
        self.shared.ingress.clear();
        self.shared.marking_active.store(true, Ordering::Release);
        tracing::debug!(
            target: "zgc",
            good_mask = self.shared.context_arc().good_mask(),
            mark_color = ?mark_color_for(self.shared.context_arc().good_mask()),
            "ZGC mark cycle opened"
        );
    }

    /// Close the cycle. The load barrier must stop calling
    /// [`ZMarkHandle::mark_live`] after this.
    pub fn end_cycle(&self) {
        self.shared.marking_active.store(false, Ordering::Release);
        // Drop the cycle's context. A pool constructed with a real one falls
        // back to that and is unaffected; a PERSISTENT pool stops referencing
        // the heap it just marked here, which is what lets that heap drop.
        self.shared.set_cycle_context(None);
    }

    /// [`Self::begin_cycle`], marking through `ctx` for this cycle only.
    ///
    /// What makes a pool reusable across cycles: the context is dropped again
    /// by [`Self::end_cycle`], so between cycles the pool holds no reference
    /// to any heap. See [`ZMarkShared::context_arc`] for why that matters.
    pub fn begin_cycle_with(&self, ctx: Arc<dyn ZMarkContext>) {
        // A reused pool carries the previous cycle.s counters, and every
        // reader of them asks about THIS cycle -- see `ZMarkStats::reset`.
        self.shared.stats.reset();
        self.shared.terminator.reset_park_timeouts();
        self.shared.set_cycle_context(Some(ctx));
        self.begin_cycle();
    }

    /// Mark a root set and seed the stripes with it.
    ///
    /// Returns how many roots were newly marked. Already-marked and
    /// out-of-heap roots are dropped, matching
    /// `ZgcRealHeap::collect_garbage`'s `registered.contains(&addr)` gate.
    ///
    /// Call at a safepoint, before [`start_marking`](Self::start_marking).
    pub fn push_roots(&self, roots: &[u64]) -> usize {
        let ctx_arc = self.shared.context_arc();
        let ctx = &*ctx_arc;
        let mut batch: Vec<u64> = Vec::with_capacity(roots.len());
        for &root in roots {
            if root == 0 || !ctx.is_in_heap(root) {
                continue;
            }
            if ctx.try_mark(root) {
                batch.push(root);
            }
        }
        let marked = batch.len();
        self.shared
            .stats
            .roots_marked
            .fetch_add(marked as u64, Ordering::Relaxed);
        self.shared.publish_distributed(batch);
        marked
    }

    /// Seed one specific stripe. **Test hook** — production callers want
    /// [`push_roots`](Self::push_roots), whose round-robin distribution is
    /// what keeps the pool from starting out imbalanced. This exists so a
    /// test can create an imbalance on purpose and observe work stealing fix
    /// it.
    pub fn dbg_push_roots_to_stripe(&self, stripe_key: usize, roots: &[u64]) -> usize {
        let ctx_arc = self.shared.context_arc();
        let ctx = &*ctx_arc;
        let mut batch: Vec<u64> = Vec::with_capacity(roots.len());
        for &root in roots {
            if root == 0 || !ctx.is_in_heap(root) {
                continue;
            }
            if ctx.try_mark(root) {
                batch.push(root);
            }
        }
        let marked = batch.len();
        self.shared
            .stats
            .roots_marked
            .fetch_add(marked as u64, Ordering::Relaxed);
        self.shared.stripes.publish(stripe_key, &mut batch);
        self.shared.terminator.note_work_published();
        marked
    }

    /// Arm the workers for a concurrent phase.
    pub fn start_marking(&self) -> u64 {
        let cycle = self.shared.terminator.arm();
        tracing::debug!(
            target: "zgc",
            cycle,
            workers = self.shared.n_workers,
            pending = self.shared.stripes.total_len(),
            "ZGC concurrent mark armed"
        );
        cycle
    }

    /// Block until the workers reach a fixed point.
    ///
    /// A fixed point is **not** completion — see the module docs and
    /// [`ZMarkTerminator`]. Follow it with [`try_end_mark`](Self::try_end_mark)
    /// at a safepoint.
    pub fn wait_for_fixed_point(&self) {
        self.shared
            .terminator
            .wait_for_fixed_point(&self.shared.should_stop);
    }

    /// The mark-end safepoint decision.
    ///
    /// # Caller contract
    ///
    /// 1. Every mutator thread is stopped.
    /// 2. Every per-thread [`ZMarkMutatorBuffer`] of a **live** thread has been
    ///    flushed with [`ZMarkHandle::flush_buffer`]. This is the exact analogue
    ///    of `g1_concurrent`'s remark calling
    ///    [`crate::satb::flush_thread_satb_buffer`], and the exact analogue of
    ///    the bug that motivated it: an address still sitting in a per-thread
    ///    buffer is invisible here, so skipping the flush lets this function
    ///    answer `Complete` with a live object unscanned.
    ///
    ///    **Threads that have already exited are covered without the caller
    ///    doing anything, but only for attached buffers** — one built by
    ///    [`ZMarkHandle::new_buffer`] flushes itself from `Drop`, so its entries
    ///    are in the ingress by the time this runs. A detached
    ///    [`ZMarkMutatorBuffer::new`] buffer is not covered by anything; see
    ///    that type's docs.
    /// 3. The workers are at a fixed point (or paused).
    ///
    /// It then folds the mutator hand-off into the work stripes and reports
    /// whether anything survived that fold.
    pub fn try_end_mark(&self) -> ZMarkEndResult {
        // Ingress -> local vec (ingress locks taken and released), then
        // local vec -> stripes. The two lock families are never held
        // together; see the module docs.
        let mut pending: Vec<u64> = Vec::new();
        let moved = self.shared.ingress.drain_into(&mut pending);
        if moved > 0 {
            self.shared
                .stats
                .ingress_drains
                .fetch_add(1, Ordering::Relaxed);
            self.shared
                .stats
                .ingress_objects
                .fetch_add(moved as u64, Ordering::Relaxed);
            self.shared.publish_distributed(pending);
        }

        if self.shared.stripes.has_work() {
            self.shared
                .stats
                .mark_end_restarts
                .fetch_add(1, Ordering::Relaxed);
            tracing::debug!(
                target: "zgc",
                flushed = moved,
                pending = self.shared.stripes.total_len(),
                "ZGC mark end: mutator marks arrived during the fixed point; restarting \
                 concurrent mark"
            );
            return ZMarkEndResult::Restart;
        }
        ZMarkEndResult::Complete
    }

    /// Run concurrent phases until [`try_end_mark`](Self::try_end_mark) says
    /// `Complete`, or `max_restarts` is exceeded.
    ///
    /// # The restart budget
    ///
    /// In production the caller drives this loop itself, because each
    /// iteration needs a real safepoint (to stop mutators and flush their
    /// buffers) between `wait_for_fixed_point` and `try_end_mark`. This
    /// convenience form is correct whenever the caller can guarantee that
    /// contract — including in tests, where the "mutator" is the test thread
    /// and is stopped by virtue of being here.
    ///
    /// The budget exists because a mutator that touches new references faster
    /// than the pool can trace them can, in principle, restart forever.
    /// HotSpot handles this by escalating (eventually marking during a
    /// pause); until this module has that escalation, exhausting the budget
    /// is logged loudly and reported in
    /// [`ZMarkCycleReport::budget_exhausted`], because completing a cycle
    /// with unmarked live objects is a use-after-free and the caller must
    /// not treat it as success.
    pub fn mark_to_completion(&self, max_restarts: usize) -> ZMarkCycleReport {
        let mut passes = 0usize;
        let mut restarts = 0usize;
        let mut budget_exhausted = false;
        loop {
            self.start_marking();
            self.wait_for_fixed_point();
            passes += 1;
            if self.shared.should_stop.load(Ordering::Acquire) {
                break;
            }
            match self.try_end_mark() {
                ZMarkEndResult::Complete => break,
                ZMarkEndResult::Restart => {
                    restarts += 1;
                    if restarts > max_restarts {
                        budget_exhausted = true;
                        tracing::warn!(
                            target: "zgc",
                            restarts,
                            max_restarts,
                            pending = self.shared.stripes.total_len(),
                            "ZGC mark: restart budget exhausted with work still pending; \
                             the mark set is INCOMPLETE and must not be swept against"
                        );
                        break;
                    }
                }
            }
        }
        ZMarkCycleReport {
            passes,
            restarts,
            budget_exhausted,
            stats: self.shared.stats.snapshot(),
        }
    }

    /// Pause every worker at a yield point. The guard resumes them on drop.
    pub fn pause_for_safepoint(&self) -> ZMarkPauseGuard<'_> {
        self.shared.pause.request_pause();
        self.shared
            .pause
            .wait_until_quiescent(&self.shared.should_stop);
        ZMarkPauseGuard {
            // `&*` explicitly: the field is `Arc<ZMarkShared>` and the guard
            // holds `&ZMarkShared`. Spelling the deref out rather than
            // leaning on a coercion keeps this compiling if the field type
            // ever changes.
            shared: &*self.shared,
        }
    }

    /// Run the weak / soft / phantom / final reference phase.
    ///
    /// Returns how many objects the hook resurrected. **A non-zero return
    /// means the caller must drain again** — those objects have unscanned
    /// out-edges, and a resurrected soft referent typically drags a whole
    /// subgraph behind it. [`mark_to_completion`](Self::mark_to_completion)
    /// is the way to do that.
    ///
    /// This is deliberately only a hook: the phase ordering, the soft-ref LRU
    /// policy and the JLS strength ordering all live in
    /// [`crate::reference::ReferenceProcessor`], which every collector in this
    /// tree shares. Reimplementing any of it here would give ZGC its own
    /// subtly different `WeakReference` semantics, which is precisely the
    /// class of bug the shared processor exists to prevent.
    ///
    /// Call at a safepoint, after [`try_end_mark`](Self::try_end_mark)
    /// reported `Complete` — the liveness answers the hook sees must be
    /// final.
    pub fn process_non_strong_refs(&self, hook: &dyn ZNonStrongRefHook) -> usize {
        let ctx_arc = self.shared.context_arc();
        let ctx: &dyn ZMarkContext = &*ctx_arc;
        let mut keep: Vec<u64> = Vec::new();
        {
            let is_marked = |addr: u64| -> bool { ctx.is_marked(addr) };
            let mut keep_alive = |addr: u64| keep.push(addr);
            hook.process(&is_marked, &mut keep_alive);
        }

        let mut batch: Vec<u64> = Vec::with_capacity(keep.len());
        for addr in keep {
            if addr == 0 || !ctx.is_in_heap(addr) {
                continue;
            }
            if ctx.try_mark(addr) {
                batch.push(addr);
            }
        }
        let resurrected = batch.len();
        if resurrected > 0 {
            self.shared
                .stats
                .objects_marked
                .fetch_add(resurrected as u64, Ordering::Relaxed);
            self.shared.publish_distributed(batch);
            tracing::debug!(
                target: "zgc",
                resurrected,
                "ZGC mark: reference processing resurrected objects; a re-drain is required"
            );
        }
        resurrected
    }

    // -- shutdown -----------------------------------------------------------

    fn request_stop(&self) {
        self.shared.should_stop.store(true, Ordering::Release);
        self.shared.terminator.wake_all();
        self.shared.pause.release_pause();
    }

    /// Stop and join every worker. Consumes the pool.
    pub fn shutdown(mut self) {
        self.request_stop();
        for handle in self.handles.drain(..) {
            let _ = handle.join();
        }
    }
}

impl Drop for ZMarkCoordinator {
    fn drop(&mut self) {
        // Unlike the G1/ZGC controllers in this crate, this Drop *does* join.
        // Those detach because their worker may be blocked holding the
        // collector's mutex; ours cannot be — every wait here is a bounded
        // `wait_for`, and `should_stop` is checked on every loop head — so a
        // deterministic join is both safe and much better behaved in tests,
        // where a detached worker outliving its heap is exactly the
        // process-global-state failure mode this module is written to avoid.
        self.request_stop();
        for handle in self.handles.drain(..) {
            let _ = handle.join();
        }
    }
}

// ---------------------------------------------------------------------------
// TestMarkContext
// ---------------------------------------------------------------------------

/// An in-memory [`ZMarkContext`] over an explicit adjacency map.
///
/// Not `#[cfg(test)]`: the sibling ZGC modules being written in parallel need
/// something to drive the marker with before a real page-backed heap exists,
/// and duplicating this in three places would guarantee three subtly
/// different definitions of "in heap".
///
/// Addresses are opaque `u64` node ids; a node is "in heap" iff it is a key
/// of the graph, which makes "off-heap child" trivially expressible in a test
/// by naming an id that was never inserted.
pub struct TestMarkContext {
    graph: rustc_hash::FxHashMap<u64, Vec<u64>>,
    marked: Mutex<rustc_hash::FxHashSet<u64>>,
    good: ZGoodMask,
    /// Invoked at the top of every [`visit_refs`](ZMarkContext::visit_refs).
    /// Tests use it to construct an interleaving on purpose (e.g. to block
    /// one worker inside a scan while another goes idle).
    visit_hook: Option<Box<dyn Fn(u64) + Send + Sync>>,
    /// Reported by [`ZMarkContext::heap_base`]. `None` by default, matching the
    /// trait default and the honest answer for a graph of opaque node ids that
    /// are not addresses at all. [`with_heap_base`](Self::with_heap_base) opts
    /// in so a test can drive [`ZMarkHandle::mark_live_offset`].
    heap_base: Option<u64>,
}

impl std::fmt::Debug for TestMarkContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TestMarkContext")
            .field("nodes", &self.graph.len())
            .field("marked", &self.marked.lock().len())
            .field("good_mask", &self.good.good())
            .finish()
    }
}

impl TestMarkContext {
    /// Build a context over `graph`. Every referenced node should also be a
    /// key (with an empty child list for leaves); ids that are not keys model
    /// wild pointers.
    pub fn new(graph: rustc_hash::FxHashMap<u64, Vec<u64>>) -> Self {
        // Address 0 is this engine's null in every direction:
        // `ZMarkCoordinator::push_roots` and `process_non_strong_refs` skip a
        // zero address, `ZMarkHandle::mark_live`/`mark_live_buffered` return
        // `false` for one, and `ZMarkWorker::drain`'s child filter drops it.
        // A graph keyed on 0 therefore does not fail — it silently marks less
        // than it should, which reads as a termination bug and is not one.
        // Fail loudly at construction instead.
        assert!(
            !graph.contains_key(&0),
            "TestMarkContext: node id 0 is the engine's null address and would be \
             silently dropped by push_roots / mark_live / the child filter. \
             Number test nodes from 1."
        );
        let good = ZGoodMask::new();
        // Enter a mark phase so `good_mask()` reports a real mark color,
        // matching what the barrier would see during a cycle.
        good.flip_to_mark();
        TestMarkContext {
            graph,
            marked: Mutex::new(rustc_hash::FxHashSet::default()),
            good,
            visit_hook: None,
            heap_base: None,
        }
    }

    /// Install a per-visit callback. See [`visit_hook`](Self::visit_hook).
    pub fn with_visit_hook(mut self, hook: Box<dyn Fn(u64) + Send + Sync>) -> Self {
        self.visit_hook = Some(hook);
        self
    }

    /// Declare a [`ZMarkContext::heap_base`], so this context participates in
    /// `crate::zgc::barrier`'s offset domain and
    /// [`ZMarkHandle::mark_live_offset`] can be driven against it.
    ///
    /// The node ids then stand for machine addresses: the barrier's offset for
    /// node `n` is `n - base`. Pick a `base` smaller than every node id.
    pub fn with_heap_base(mut self, base: u64) -> Self {
        self.heap_base = Some(base);
        self
    }

    /// The set of marked node ids, sorted, for set-equality assertions.
    pub fn marked_sorted(&self) -> Vec<u64> {
        let mut v: Vec<u64> = self.marked.lock().iter().copied().collect();
        v.sort_unstable();
        v
    }

    /// How many nodes are marked.
    pub fn marked_count(&self) -> usize {
        self.marked.lock().len()
    }

    /// Forget every mark (start a new cycle).
    pub fn reset_marks(&self) {
        self.marked.lock().clear();
    }

    /// The good-mask state, so a test can drive a phase flip.
    pub fn good_mask_state(&self) -> &ZGoodMask {
        &self.good
    }
}

impl ZMarkContext for TestMarkContext {
    fn good_mask(&self) -> u64 {
        self.good.good()
    }

    fn try_mark(&self, addr: u64) -> bool {
        // A single mutex is the whole point: this must be atomic, and a test
        // double should be obviously correct rather than fast.
        self.marked.lock().insert(addr)
    }

    fn is_marked(&self, addr: u64) -> bool {
        self.marked.lock().contains(&addr)
    }

    fn visit_refs(&self, addr: u64, f: &mut dyn FnMut(u64)) {
        if let Some(hook) = &self.visit_hook {
            hook(addr);
        }
        if let Some(children) = self.graph.get(&addr) {
            for &child in children {
                f(child);
            }
        }
    }

    fn is_in_heap(&self, addr: u64) -> bool {
        self.graph.contains_key(&addr)
    }

    fn object_size(&self, _addr: u64) -> usize {
        // Uniform, so live-byte accounting is a node count in tests.
        16
    }

    fn heap_base(&self) -> Option<u64> {
        self.heap_base
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use rustc_hash::FxHashMap;
    use std::sync::atomic::AtomicBool as StdAtomicBool;

    /// Build a graph from `(node, children)` pairs, auto-inserting any child
    /// that was not declared as a leaf.
    fn graph(edges: &[(u64, &[u64])]) -> FxHashMap<u64, Vec<u64>> {
        let mut g: FxHashMap<u64, Vec<u64>> = FxHashMap::default();
        for (node, children) in edges {
            g.insert(*node, children.to_vec());
            for c in children.iter() {
                g.entry(*c).or_default();
            }
        }
        g
    }

    fn sorted(mut v: Vec<u64>) -> Vec<u64> {
        v.sort_unstable();
        v
    }

    /// Generous restart budget: none of these tests should need more than a
    /// couple, but a loaded CI box can interleave a mutator mark into more
    /// than one pass and that is correct behaviour, not a failure.
    const RESTART_BUDGET: usize = 64;

    // -- sizing -------------------------------------------------------------

    #[test]
    fn stripe_count_is_a_clamped_power_of_two() {
        for workers in 1..=64usize {
            let s = stripe_count_for(workers);
            assert!(s.is_power_of_two(), "workers={workers} -> {s}");
            assert!(s >= Z_MARK_MIN_STRIPES, "workers={workers} -> {s}");
            assert!(s <= Z_MARK_MAX_STRIPES, "workers={workers} -> {s}");
            assert!(
                s >= workers.min(Z_MARK_MAX_STRIPES),
                "at least one stripe per worker: workers={workers} -> {s}"
            );
        }
        assert_eq!(stripe_count_for(1), Z_MARK_MIN_STRIPES);
        assert_eq!(stripe_count_for(4), 16);
        assert_eq!(stripe_count_for(1024), Z_MARK_MAX_STRIPES);
    }

    #[test]
    fn mark_color_maps_only_the_two_mark_bits() {
        assert_eq!(mark_color_for(Z_MARKED0), Some(ZColor::Marked0));
        assert_eq!(mark_color_for(Z_MARKED1), Some(ZColor::Marked1));
        assert_eq!(mark_color_for(0), None);
        assert_eq!(mark_color_for(Z_MARKED0 | Z_MARKED1), None);
    }

    // -- local stack / stripes ----------------------------------------------

    #[test]
    fn local_stack_publishes_the_oldest_half() {
        let mut s = ZMarkLocalStack::new();
        for i in 0..10u64 {
            s.push(i);
        }
        let half = s.split_off_oldest_half();
        assert_eq!(half, vec![0, 1, 2, 3, 4], "the OLDEST half is handed out");
        // The newest entries — this worker's DFS frontier — stay local, and
        // still pop in LIFO order.
        assert_eq!(s.pop(), Some(9));
        assert_eq!(s.len(), 4);
    }

    #[test]
    fn steal_half_splits_and_drain_from_takes_everything() {
        let stripes = ZMarkStripeSet::new(8);
        let mut work: Vec<u64> = (0..10).collect();
        assert_eq!(stripes.publish(3, &mut work), 10);
        assert!(work.is_empty(), "publish moves, it does not copy");
        assert!(stripes.has_work());

        let mut out = Vec::new();
        assert_eq!(stripes.steal_half(3, &mut out, Z_MARK_STEAL_CAP), 5);
        assert_eq!(out.len(), 5);
        assert!(stripes.has_work(), "the victim keeps half");

        let mut rest = Vec::new();
        assert_eq!(stripes.drain_from(3, &mut rest, Z_MARK_STEAL_CAP), 5);
        assert!(!stripes.has_work());
        assert_eq!(stripes.total_len(), 0);
    }

    #[test]
    fn steal_cap_bounds_a_single_steal() {
        let stripes = ZMarkStripeSet::new(8);
        let mut work: Vec<u64> = (0..(Z_MARK_STEAL_CAP as u64 * 4)).collect();
        stripes.publish(0, &mut work);
        let mut out = Vec::new();
        let got = stripes.steal_half(0, &mut out, Z_MARK_STEAL_CAP);
        assert_eq!(got, Z_MARK_STEAL_CAP);
        assert_eq!(out.len(), Z_MARK_STEAL_CAP);
    }

    // -- single worker ------------------------------------------------------

    #[test]
    fn single_worker_marks_the_reachable_set_and_nothing_else() {
        // Reachable from root 1: {1,2,3,4,5}. Detached: {90,91}. Never
        // declared at all (a wild pointer): 999, referenced by 5.
        let mut g = graph(&[
            (1, &[2, 3]),
            (2, &[4]),
            (3, &[4, 5]),
            (4, &[]),
            (5, &[999]),
            (90, &[91]),
            (91, &[]),
        ]);
        g.remove(&999);
        let ctx = Arc::new(TestMarkContext::new(g));
        let pool = ZMarkCoordinator::new(ctx.clone(), 1);

        pool.begin_cycle();
        assert_eq!(pool.push_roots(&[1]), 1);
        let report = pool.mark_to_completion(RESTART_BUDGET);
        pool.end_cycle();

        assert!(!report.budget_exhausted);
        assert_eq!(ctx.marked_sorted(), vec![1, 2, 3, 4, 5]);
        assert!(
            report.stats.off_heap_children >= 1,
            "the undeclared child 999 must have been refused, not traced"
        );
        pool.shutdown();
    }

    #[test]
    fn cycles_in_the_object_graph_terminate() {
        // A pure cycle, plus a self-loop, plus a back edge to the root.
        let g = graph(&[(1, &[2]), (2, &[3]), (3, &[1, 4]), (4, &[4, 2])]);
        let ctx = Arc::new(TestMarkContext::new(g));
        let pool = ZMarkCoordinator::new(ctx.clone(), 2);

        pool.begin_cycle();
        pool.push_roots(&[1]);
        let report = pool.mark_to_completion(RESTART_BUDGET);
        pool.end_cycle();

        assert!(!report.budget_exhausted);
        assert_eq!(ctx.marked_sorted(), vec![1, 2, 3, 4]);
        // Each node scanned at most a bounded number of times: the mark bit,
        // not the graph shape, is what bounds the work.
        assert_eq!(
            report.stats.objects_scanned, 4,
            "try_mark must gate re-entry, or a cycle re-scans forever"
        );
        pool.shutdown();
    }

    // -- multi worker determinism -------------------------------------------

    /// A wide graph: 1 root -> 64 children -> 16 grandchildren each, plus a
    /// detached component that must stay unmarked.
    fn wide_graph() -> (FxHashMap<u64, Vec<u64>>, Vec<u64>) {
        let mut g: FxHashMap<u64, Vec<u64>> = FxHashMap::default();
        let mut expected: Vec<u64> = vec![1];
        let mut children = Vec::new();
        for c in 0..64u64 {
            let cid = 1000 + c;
            children.push(cid);
            expected.push(cid);
            let mut grand = Vec::new();
            for k in 0..16u64 {
                let gid = 100_000 + c * 100 + k;
                grand.push(gid);
                expected.push(gid);
                g.insert(gid, Vec::new());
            }
            g.insert(cid, grand);
        }
        g.insert(1, children);
        // Detached component.
        for d in 0..32u64 {
            g.insert(900_000 + d, vec![900_000 + ((d + 1) % 32)]);
        }
        (g, sorted(expected))
    }

    #[test]
    fn four_workers_mark_exactly_the_same_set_as_one() {
        let (g, expected) = wide_graph();

        let ctx1 = Arc::new(TestMarkContext::new(g.clone()));
        let pool1 = ZMarkCoordinator::new(ctx1.clone(), 1);
        pool1.begin_cycle();
        pool1.push_roots(&[1]);
        let r1 = pool1.mark_to_completion(RESTART_BUDGET);
        pool1.end_cycle();
        let single = ctx1.marked_sorted();
        pool1.shutdown();

        let ctx4 = Arc::new(TestMarkContext::new(g));
        let pool4 = ZMarkCoordinator::new(ctx4.clone(), 4);
        pool4.begin_cycle();
        pool4.push_roots(&[1]);
        let r4 = pool4.mark_to_completion(RESTART_BUDGET);
        pool4.end_cycle();
        let multi = ctx4.marked_sorted();
        pool4.shutdown();

        assert!(!r1.budget_exhausted && !r4.budget_exhausted);
        assert_eq!(single, expected, "single-worker result");
        assert_eq!(
            multi, single,
            "N=4 must produce the identical mark SET; only the order may differ"
        );
        // Every node is scanned exactly once regardless of worker count —
        // that is the `try_mark` contract, and it is the thing that would
        // break first if the local/stripe hand-off ever duplicated work.
        assert_eq!(r1.stats.objects_scanned, expected.len() as u64);
        assert_eq!(r4.stats.objects_scanned, expected.len() as u64);
    }

    // -- work stealing ------------------------------------------------------

    #[test]
    fn work_stealing_fires_on_a_deliberately_unbalanced_distribution() {
        // 4 workers -> 16 stripes. Worker `i` owns stripe `i & 15`, i.e.
        // stripes 0..=3. Seeding stripe 9 means NO worker owns the work, so
        // the very first acquisition by any worker is by definition a steal.
        // That makes this test deterministic rather than a race on timing.
        let n_workers = 4usize;
        let stripes = stripe_count_for(n_workers);
        assert!(
            stripes > n_workers,
            "this test needs an unowned stripe to exist"
        );
        let unowned_stripe = n_workers + 5;
        assert!(unowned_stripe < stripes);

        let mut g: FxHashMap<u64, Vec<u64>> = FxHashMap::default();
        let mut roots = Vec::new();
        let mut expected = Vec::new();
        for i in 0..2048u64 {
            let root = 10_000 + i;
            let child = 500_000 + i;
            g.insert(root, vec![child]);
            g.insert(child, Vec::new());
            roots.push(root);
            expected.push(root);
            expected.push(child);
        }

        let ctx = Arc::new(TestMarkContext::new(g));
        let pool = ZMarkCoordinator::new(ctx.clone(), n_workers);
        assert_eq!(pool.stripe_count(), stripes);

        pool.begin_cycle();
        assert_eq!(pool.dbg_push_roots_to_stripe(unowned_stripe, &roots), 2048);
        let report = pool.mark_to_completion(RESTART_BUDGET);
        pool.end_cycle();

        assert!(!report.budget_exhausted);
        assert_eq!(ctx.marked_sorted(), sorted(expected));
        assert!(
            report.stats.steals_succeeded > 0,
            "no steal fired despite every root living in an unowned stripe \
             (attempts={}, own_refills={})",
            report.stats.steal_attempts,
            report.stats.own_stripe_refills
        );
        assert!(
            report.stats.stolen_objects > 0,
            "a successful steal must move at least one object"
        );
        pool.shutdown();
    }

    // -- termination --------------------------------------------------------

    /// The race the [`ZMarkTerminator`] docs describe, constructed on
    /// purpose.
    ///
    /// Two workers. One root is a leaf (`FAST`), so whichever worker takes it
    /// finishes instantly and offers itself for termination. The other root
    /// (`SLOW`) blocks *inside* `visit_refs` — i.e. its 300 children exist
    /// nowhere the termination probe could see them — until the test thread
    /// observes that a worker has actually gone idle.
    ///
    /// With a naive "all queues are empty, therefore we are done" test, the
    /// idle worker declares completion here and the 300 children are never
    /// marked. With the active-worker handshake, it cannot: the worker inside
    /// `visit_refs` is still counted, so the idle worker is not the last one
    /// out and must park.
    ///
    /// The assertion is set equality, never elapsed time.
    #[test]
    fn termination_does_not_fire_while_a_worker_holds_work_in_flight() {
        const FAST: u64 = 1;
        const SLOW: u64 = 2;

        let mut g: FxHashMap<u64, Vec<u64>> = FxHashMap::default();
        g.insert(FAST, Vec::new());
        let mut slow_children = Vec::new();
        let mut expected = vec![FAST, SLOW];
        for i in 0..300u64 {
            let c = 700_000 + i;
            slow_children.push(c);
            expected.push(c);
            g.insert(c, Vec::new());
        }
        g.insert(SLOW, slow_children);

        // The test thread flips this once it has seen a worker go idle.
        let release = Arc::new(StdAtomicBool::new(false));
        let entered_slow = Arc::new(AtomicUsize::new(0));

        let hook_release = Arc::clone(&release);
        let hook_entered = Arc::clone(&entered_slow);
        let ctx = Arc::new(
            TestMarkContext::new(g).with_visit_hook(Box::new(move |addr: u64| {
                if addr != SLOW {
                    return;
                }
                hook_entered.fetch_add(1, Ordering::SeqCst);
                // Bounded spin: if the interleaving does not materialise on
                // this run (e.g. a single-CPU CI box), proceed anyway. The
                // test still checks correctness; it just does not exercise
                // the race that time. Never assert on how long this took.
                let mut spins: u64 = 0;
                while !hook_release.load(Ordering::Acquire) && spins < 20_000_000 {
                    spins += 1;
                    std::thread::yield_now();
                }
            })),
        );

        let pool = ZMarkCoordinator::new(ctx.clone(), 2);
        pool.begin_cycle();
        // Two stripes, one root each, so both workers find work immediately.
        pool.dbg_push_roots_to_stripe(0, &[FAST]);
        pool.dbg_push_roots_to_stripe(1, &[SLOW]);
        pool.start_marking();

        // Wait until a worker is actually inside SLOW's visit_refs AND the
        // other has offered itself for termination. Bounded, and the loop
        // asserts nothing about duration.
        let mut spins: u64 = 0;
        while spins < 20_000_000 {
            if entered_slow.load(Ordering::SeqCst) > 0 && pool.active_workers() < 2 {
                break;
            }
            spins += 1;
            std::thread::yield_now();
        }
        let raced = entered_slow.load(Ordering::SeqCst) > 0 && pool.active_workers() < 2;
        release.store(true, Ordering::Release);

        pool.wait_for_fixed_point();
        assert_eq!(pool.try_end_mark(), ZMarkEndResult::Complete);
        pool.end_cycle();

        assert_eq!(
            ctx.marked_sorted(),
            sorted(expected),
            "a worker that went idle while another held children in flight \
             terminated the cycle early (raced={raced})"
        );
        pool.shutdown();
    }

    #[test]
    fn a_fixed_point_with_an_empty_ingress_completes_in_one_pass() {
        let g = graph(&[(1, &[2, 3]), (2, &[]), (3, &[])]);
        let ctx = Arc::new(TestMarkContext::new(g));
        let pool = ZMarkCoordinator::new(ctx.clone(), 3);
        pool.begin_cycle();
        pool.push_roots(&[1]);
        let report = pool.mark_to_completion(RESTART_BUDGET);
        pool.end_cycle();
        assert_eq!(report.restarts, 0);
        assert_eq!(report.passes, 1);
        assert_eq!(ctx.marked_sorted(), vec![1, 2, 3]);
        pool.shutdown();
    }

    // -- mutator marking ----------------------------------------------------

    /// A mutator mark that arrives after the workers reached a fixed point
    /// must not be lost: the mark-end probe has to see it and restart.
    ///
    /// This is the difference between ZGC and SATB stated as a test. Under
    /// SATB the remark pause is final by construction; here it is a decision
    /// point that can send everybody back around.
    #[test]
    fn a_mutator_mark_after_the_fixed_point_forces_a_restart_and_is_not_lost() {
        let g = graph(&[
            (1, &[2]),
            (2, &[]),
            // Reachable only via the simulated load barrier.
            (50, &[51, 52]),
            (51, &[53]),
            (52, &[]),
            (53, &[]),
        ]);
        let ctx = Arc::new(TestMarkContext::new(g));
        let pool = ZMarkCoordinator::new(ctx.clone(), 2);
        let handle = pool.handle();

        pool.begin_cycle();
        assert!(handle.is_marking());
        pool.push_roots(&[1]);
        pool.start_marking();
        pool.wait_for_fixed_point();
        assert_eq!(ctx.marked_sorted(), vec![1, 2]);

        // A mutator loads a stale reference to 50 *after* the workers
        // converged. In OpenJDK this is the load-barrier slow path.
        assert!(handle.mark_live(0, 50));
        assert!(
            !handle.mark_live(0, 50),
            "a second load of the same reference must not re-enqueue it"
        );

        // Mark end, mutators stopped (we are the only one and we are here).
        assert_eq!(
            pool.try_end_mark(),
            ZMarkEndResult::Restart,
            "the mark-end probe must see the mutator's mark"
        );

        // Run the concurrent phase again.
        pool.start_marking();
        pool.wait_for_fixed_point();
        assert_eq!(pool.try_end_mark(), ZMarkEndResult::Complete);
        pool.end_cycle();

        assert_eq!(
            ctx.marked_sorted(),
            vec![1, 2, 50, 51, 52, 53],
            "the mutator's object AND its transitive closure must be marked"
        );
        assert!(pool.stats().mark_end_restarts.load(Ordering::Relaxed) >= 1);
        pool.shutdown();
    }

    // -----------------------------------------------------------------------
    // N3 (2026-08-07): the barrier's offset domain meets this module's
    // machine-address domain, and the meeting point is `mark_live_offset`.
    // -----------------------------------------------------------------------

    /// The origin used by the offset-domain tests below. Node ids in
    /// [`TestMarkContext`] then stand for machine addresses, and the barrier's
    /// offset for node `n` is `n - HEAP_BASE` — the same relation
    /// `ZRelocate::offset_of_address` implements over a real reservation.
    const HEAP_BASE: u64 = 0x4000;

    /// N3, POSITIVE half. A bare heap offset — what
    /// `barrier::load_barrier_slow` actually passes to
    /// `ZBarrierContext::mark_live` — reaches the marker when it goes through
    /// [`ZMarkHandle::mark_live_offset`], and drags its transitive closure with
    /// it through the restart loop.
    #[test]
    fn a_barrier_offset_marks_its_object_through_mark_live_offset() {
        let g = graph(&[
            (HEAP_BASE + 1, &[HEAP_BASE + 2]),
            (HEAP_BASE + 2, &[]),
            // Reachable only through the simulated load barrier.
            (HEAP_BASE + 50, &[HEAP_BASE + 51]),
            (HEAP_BASE + 51, &[]),
        ]);
        let ctx = Arc::new(TestMarkContext::new(g).with_heap_base(HEAP_BASE));
        let pool = ZMarkCoordinator::new(ctx.clone(), 2);
        let handle = pool.handle();
        assert_eq!(handle.heap_base(), Some(HEAP_BASE));

        pool.begin_cycle();
        pool.push_roots(&[HEAP_BASE + 1]);
        pool.start_marking();
        pool.wait_for_fixed_point();

        // This is the value the barrier holds: `raw & Z_OFFSET_MASK`, with the
        // heap base already subtracted by whoever wrote the slot. It is NOT an
        // address, and `is_in_heap` would refuse it as one.
        let barrier_offset: u64 = 50;
        assert!(is_bare_offset(barrier_offset, Z_OFFSET_MASK));
        assert!(
            handle.mark_live_offset(0, barrier_offset),
            "the seam must convert the offset to HEAP_BASE + 50 and mark it"
        );
        assert!(
            !handle.mark_live_offset(0, barrier_offset),
            "a second load of the same reference must not re-enqueue it"
        );

        assert_eq!(pool.try_end_mark(), ZMarkEndResult::Restart);
        pool.start_marking();
        pool.wait_for_fixed_point();
        assert_eq!(pool.try_end_mark(), ZMarkEndResult::Complete);
        pool.end_cycle();

        assert_eq!(
            ctx.marked_sorted(),
            vec![HEAP_BASE + 1, HEAP_BASE + 2, HEAP_BASE + 50, HEAP_BASE + 51],
            "the barrier's object AND its transitive closure must be marked"
        );
        let s = pool.stats().snapshot();
        assert_eq!(s.domain_refusals, 0);
        assert_eq!(s.off_heap_children, 0);
        pool.shutdown();
    }

    /// N3, THE DEFECT. This is the test that would have caught it, and it
    /// asserts the *silence* rather than a crash.
    ///
    /// Handing the same bare offset to [`ZMarkHandle::mark_live`] — the
    /// machine-address entry point, which is what a naive
    /// `ZBarrierContext::mark_live` adapter would have called — does not fail,
    /// does not assert and does not log anything a reader would act on. It is
    /// refused by [`ZMarkContext::is_in_heap`] exactly as an honest wild pointer
    /// would be, counted as `off_heap_children`, and the object stays unmarked
    /// and is swept. The two calls in this test differ only in which method
    /// name the adapter reached for.
    #[test]
    fn the_same_offset_is_silently_discarded_by_the_machine_address_entry_point() {
        let g = graph(&[(HEAP_BASE + 50, &[])]);
        let ctx = Arc::new(TestMarkContext::new(g).with_heap_base(HEAP_BASE));
        let pool = ZMarkCoordinator::new(ctx.clone(), 1);
        let handle = pool.handle();
        pool.begin_cycle();

        let barrier_offset: u64 = 50;

        // The wrong entry point. No panic, no error log, no domain refusal —
        // just a `false` and a wild-pointer counter, which is what makes this
        // the most dangerous shape a seam bug can have.
        assert!(
            !handle.mark_live(0, barrier_offset),
            "an offset is not a registered object base, so is_in_heap refuses it"
        );
        let s = pool.stats().snapshot();
        assert_eq!(
            s.off_heap_children, 1,
            "the mark was discarded and counted as a wild pointer"
        );
        assert_eq!(
            s.domain_refusals, 0,
            "and nothing anywhere identified it as a DOMAIN error"
        );
        assert_eq!(ctx.marked_count(), 0, "the live object is now sweepable");

        // The right entry point, same value, same heap, same cycle.
        assert!(handle.mark_live_offset(0, barrier_offset));
        assert_eq!(ctx.marked_sorted(), vec![HEAP_BASE + 50]);

        pool.end_cycle();
        pool.shutdown();
    }

    /// N3, NEGATIVE half: a machine address handed to the *offset* entry point
    /// is refused at the boundary, counted separately from a wild pointer, and
    /// never reaches `try_mark`.
    ///
    /// # Why `catch_unwind` and not `#[should_panic]`
    ///
    /// The tripwire is a `tracing::error!` **plus** a `debug_assert!`, so it
    /// aborts the caller where assertions are on and returns `false` where they
    /// are not. A `#[should_panic]` test would therefore pass in debug and fail
    /// in release — cfg-dependent test behaviour is a documented trap in this
    /// repo, and it is the same trap that motivated making the check itself
    /// survive `--release`. The counter is bumped *before* the assert, so
    /// asserting on the counter means the same thing in both profiles.
    #[test]
    fn mark_live_offset_refuses_a_machine_address_and_counts_a_domain_refusal() {
        // ~47 bits, five bits past the 42-bit offset field: a legal Linux
        // reservation address, and invisible as a bug on the Windows dev host
        // where the reservation is placed low. Same value `barrier.rs`'s
        // `is_bare_offset_separates_a_heap_offset_from_a_machine_pointer` uses.
        const LINUX_SHAPED_HEAP_ADDRESS: u64 = 0x7f12_3456_7890;
        assert!(!is_bare_offset(LINUX_SHAPED_HEAP_ADDRESS, Z_OFFSET_MASK));

        let g = graph(&[(HEAP_BASE + 1, &[])]);
        let ctx = Arc::new(TestMarkContext::new(g).with_heap_base(HEAP_BASE));
        let pool = ZMarkCoordinator::new(ctx.clone(), 1);
        let handle = pool.handle();
        pool.begin_cycle();

        let attempted = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            handle.mark_live_offset(0, LINUX_SHAPED_HEAP_ADDRESS)
        }));
        if let Ok(marked) = attempted {
            assert!(!marked, "a machine address must not mark anything");
        }

        let s = pool.stats().snapshot();
        assert_eq!(
            s.domain_refusals, 1,
            "the refusal must be attributed to the DOMAIN"
        );
        assert_eq!(
            s.off_heap_children, 0,
            "and must NOT hide inside the wild-pointer counter"
        );
        assert_eq!(ctx.marked_count(), 0);

        pool.end_cycle();
        pool.shutdown();
    }

    /// N3, the "no base" arm: a context that does not participate in the offset
    /// domain refuses instead of guessing a base.
    ///
    /// `ZgcRealHeap` is exactly this context today — it stores raw pointers in
    /// its slots and never colours them — so this is the behaviour a barrier
    /// wired to it right now would get: loud and counted, not silent.
    #[test]
    fn mark_live_offset_refuses_a_context_with_no_heap_base() {
        let g = graph(&[(50, &[])]);
        // No `.with_heap_base(..)`: the trait default, `None`.
        let ctx = Arc::new(TestMarkContext::new(g));
        let pool = ZMarkCoordinator::new(ctx.clone(), 1);
        let handle = pool.handle();
        assert_eq!(handle.heap_base(), None);
        pool.begin_cycle();

        // 50 IS a valid node id here, so a context that silently assumed a base
        // of 0 would have marked it and looked entirely correct — which is why
        // the default must refuse rather than default to zero.
        let attempted = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            handle.mark_live_offset(0, 50)
        }));
        if let Ok(marked) = attempted {
            assert!(!marked);
        }

        assert_eq!(pool.stats().domain_refusals.load(Ordering::Relaxed), 1);
        assert_eq!(
            ctx.marked_count(),
            0,
            "refusing beats guessing a base of 0 and marking by coincidence"
        );

        pool.end_cycle();
        pool.shutdown();
    }

    /// The buffered twin carries the same boundary. It matters more here than
    /// on the unbuffered path: `mark_live_buffered` sets the mark bit before
    /// the address reaches the ingress, so a wrong-domain value that got past
    /// the boundary would be marked-and-unscanned rather than merely refused.
    #[test]
    fn mark_live_buffered_offset_converts_and_the_buffer_flushes_the_address() {
        let g = graph(&[(HEAP_BASE + 60, &[HEAP_BASE + 61]), (HEAP_BASE + 61, &[])]);
        let ctx = Arc::new(TestMarkContext::new(g).with_heap_base(HEAP_BASE));
        let pool = ZMarkCoordinator::new(ctx.clone(), 1);
        let handle = pool.handle();
        pool.begin_cycle();

        let mut buf = handle.new_buffer(0);
        assert!(handle.mark_live_buffered_offset(&mut buf, 60));
        assert_eq!(
            ctx.marked_sorted(),
            vec![HEAP_BASE + 60],
            "the mark bit is set at HEAP_BASE + 60, not at 60"
        );

        assert_eq!(handle.flush_buffer(&mut buf), 1);
        // Fold the ingress into the stripes deterministically rather than
        // racing a worker for it, then run the concurrent phase.
        assert_eq!(pool.try_end_mark(), ZMarkEndResult::Restart);
        pool.start_marking();
        pool.wait_for_fixed_point();
        assert_eq!(pool.try_end_mark(), ZMarkEndResult::Complete);
        pool.end_cycle();

        assert_eq!(
            ctx.marked_sorted(),
            vec![HEAP_BASE + 60, HEAP_BASE + 61],
            "the converted address must be scannable, not just markable"
        );
        assert_eq!(pool.stats().snapshot().domain_refusals, 0);
        pool.shutdown();
    }

    /// The same guarantee, but with the mutator marking *while* the workers
    /// are draining, from a different thread.
    #[test]
    fn concurrent_mutator_marks_are_folded_in_by_the_restart_loop() {
        let mut g: FxHashMap<u64, Vec<u64>> = FxHashMap::default();
        let mut expected: Vec<u64> = Vec::new();
        // A chain the workers trace from the roots. Node ids start at **1**:
        // address `0` is this engine's null everywhere — `push_roots` skips
        // it, `ZMarkHandle::mark_live` returns `false` for it, and `drain`'s
        // child filter drops it — so a graph keyed on 0 silently loses its own
        // root and marks nothing at all.
        for i in 1..=500u64 {
            g.insert(i, if i < 500 { vec![i + 1] } else { Vec::new() });
            expected.push(i);
        }
        // A disjoint component only the "mutator" knows about.
        let mut mutator_seeds = Vec::new();
        for i in 0..64u64 {
            let a = 800_000 + i * 2;
            let b = 800_001 + i * 2;
            g.insert(a, vec![b]);
            g.insert(b, Vec::new());
            mutator_seeds.push(a);
            expected.push(a);
            expected.push(b);
        }

        let ctx = Arc::new(TestMarkContext::new(g));
        let pool = ZMarkCoordinator::new(ctx.clone(), 4);
        let handle = pool.handle();

        pool.begin_cycle();
        assert_eq!(
            pool.push_roots(&[1]),
            1,
            "the root must be accepted; a silently dropped root would make the \
             mark-set assertion below fail for a reason that has nothing to do \
             with the mutator fold this test is about"
        );
        pool.start_marking();

        // "Mutator" thread: hand addresses over while the pool is running.
        let mutator_handle = handle.clone();
        let mutator = std::thread::spawn(move || {
            let mut buf = ZMarkMutatorBuffer::new(7);
            for seed in mutator_seeds {
                mutator_handle.mark_live_buffered(&mut buf, seed);
                std::thread::yield_now();
            }
            // Safepoint contract: every per-thread buffer must be flushed
            // before the mark-end probe runs, exactly as G1's remark flushes
            // the thread-local SATB buffer.
            mutator_handle.flush_buffer(&mut buf);
        });

        pool.wait_for_fixed_point();
        mutator.join().expect("mutator thread");

        // Now drive the restart loop to completion.
        loop {
            match pool.try_end_mark() {
                ZMarkEndResult::Complete => break,
                ZMarkEndResult::Restart => {
                    pool.start_marking();
                    pool.wait_for_fixed_point();
                }
            }
        }
        pool.end_cycle();

        assert_eq!(
            ctx.marked_sorted(),
            sorted(expected),
            "mid-cycle mutator marks (and their closures) must survive"
        );
        pool.shutdown();
    }

    /// An unflushed mutator buffer is invisible to the mark-end probe. This
    /// is a *contract* test, not a bug report: it pins the reason the
    /// safepoint must flush every buffer, so that anyone who removes the
    /// flush from the safepoint path fails here instead of in production.
    #[test]
    fn an_unflushed_mutator_buffer_is_invisible_to_the_mark_end_probe() {
        let g = graph(&[(1, &[]), (77, &[78]), (78, &[])]);
        let ctx = Arc::new(TestMarkContext::new(g));
        let pool = ZMarkCoordinator::new(ctx.clone(), 1);
        let handle = pool.handle();

        pool.begin_cycle();
        pool.push_roots(&[1]);
        pool.start_marking();
        pool.wait_for_fixed_point();

        let mut buf = ZMarkMutatorBuffer::new(3);
        assert!(handle.mark_live_buffered(&mut buf, 77));
        assert_eq!(buf.len(), 1, "well below the flush threshold");

        assert_eq!(
            pool.try_end_mark(),
            ZMarkEndResult::Complete,
            "an unflushed buffer cannot be seen — this is why the safepoint must flush"
        );

        // Flushing makes it visible, and the restart picks up the closure.
        handle.flush_buffer(&mut buf);
        assert_eq!(pool.try_end_mark(), ZMarkEndResult::Restart);
        pool.start_marking();
        pool.wait_for_fixed_point();
        assert_eq!(pool.try_end_mark(), ZMarkEndResult::Complete);
        pool.end_cycle();

        assert_eq!(ctx.marked_sorted(), vec![1, 77, 78]);
        pool.shutdown();
    }

    // -- thread death (FINDING A) -------------------------------------------

    /// FINDING A (2026-08-07). A mutator that **exits** holding a non-empty
    /// buffer must not strand the objects it already marked.
    ///
    /// `mark_live_buffered` sets the mark bit before the address reaches the
    /// ingress. So an entry left in a dying thread's buffer leaves its object
    /// *marked* — no future `try_mark` can rediscover it — and in *no queue* —
    /// no worker will ever scan it. Its out-edges are never traced and
    /// everything reachable only through it is swept while live: the exact
    /// use-after-free the termination protocol exists to prevent, arriving by
    /// the back door.
    ///
    /// `88 -> 89 -> 90` is that "everything reachable only through it". The
    /// thread below exits without flushing, so only the attached buffer's
    /// `Drop` can save them. Against the pre-fix code — where
    /// [`ZMarkMutatorBuffer`] had no link to the pool and no `Drop` — this
    /// fails at the `Restart` assertion.
    ///
    /// Nothing here is timed; the assertions are on the mark **set** and on the
    /// mark-end verdict.
    #[test]
    fn a_dying_thread_with_an_attached_buffer_does_not_strand_a_marked_object() {
        let g = graph(&[(1, &[]), (88, &[89]), (89, &[90]), (90, &[])]);
        let ctx = Arc::new(TestMarkContext::new(g));
        let pool = ZMarkCoordinator::new(ctx.clone(), 2);
        let handle = pool.handle();

        pool.begin_cycle();
        pool.push_roots(&[1]);
        pool.start_marking();
        pool.wait_for_fixed_point();
        assert_eq!(ctx.marked_sorted(), vec![1]);

        let mutator_handle = handle.clone();
        std::thread::spawn(move || {
            let mut buf = mutator_handle.new_buffer(5);
            assert!(
                buf.is_attached(),
                "new_buffer must produce an attached buffer"
            );
            assert!(mutator_handle.mark_live_buffered(&mut buf, 88));
            assert_eq!(buf.len(), 1, "well below the auto-flush threshold");
            // Deliberately NO flush_buffer: the thread ends here and `buf` is
            // dropped by ordinary thread teardown. That drop is the fix.
        })
        .join()
        .expect("mutator thread");

        assert_eq!(
            pool.try_end_mark(),
            ZMarkEndResult::Restart,
            "the dying thread's buffered entry was lost: 88 is already marked, \
             so nothing can rediscover it, and 89/90 are reachable only through \
             it — they would be swept while live"
        );
        pool.start_marking();
        pool.wait_for_fixed_point();
        assert_eq!(pool.try_end_mark(), ZMarkEndResult::Complete);
        pool.end_cycle();

        assert_eq!(
            ctx.marked_sorted(),
            vec![1, 88, 89, 90],
            "the stranded object's whole closure must be traced"
        );
        pool.shutdown();
    }

    /// The negative counterpart, and the reason
    /// [`ZMarkHandle::new_buffer`] exists: a **detached**
    /// [`ZMarkMutatorBuffer::new`] buffer has nowhere to flush to, so its `Drop`
    /// can only log. This pins the damage shape as a named property rather than
    /// a footnote, exactly as
    /// `an_unflushed_mutator_buffer_is_invisible_to_the_mark_end_probe` does for
    /// the live-thread case.
    ///
    /// It is a **contract** test, not a bug report: anyone who makes `new`
    /// attached-by-default, or who removes the distinction, fails here and is
    /// forced to read why the distinction was drawn.
    #[test]
    fn a_detached_buffer_dropped_non_empty_leaves_a_marked_but_unscanned_object() {
        let g = graph(&[(1, &[]), (88, &[89]), (89, &[])]);
        let ctx = Arc::new(TestMarkContext::new(g));
        let pool = ZMarkCoordinator::new(ctx.clone(), 1);
        let handle = pool.handle();

        pool.begin_cycle();
        pool.push_roots(&[1]);
        pool.start_marking();
        pool.wait_for_fixed_point();

        let mutator_handle = handle.clone();
        std::thread::spawn(move || {
            let mut buf = ZMarkMutatorBuffer::new(5);
            assert!(
                !buf.is_attached(),
                "ZMarkMutatorBuffer::new is the detached form"
            );
            assert!(mutator_handle.mark_live_buffered(&mut buf, 88));
        })
        .join()
        .expect("mutator thread");

        assert_eq!(
            pool.try_end_mark(),
            ZMarkEndResult::Complete,
            "a detached buffer's entries reach nobody — this is the whole \
             reason ZMarkHandle::new_buffer exists"
        );
        pool.end_cycle();

        // And this is the damage, stated exactly: the object is marked (so no
        // later `try_mark` will ever hand it to a worker) and its child is not
        // marked at all (so it would be swept while live).
        assert!(
            ctx.is_marked(88),
            "the mark bit is set BEFORE the push — that is what makes a lost \
             buffer entry unrecoverable rather than merely late"
        );
        assert!(
            !ctx.is_marked(89),
            "89 is reachable only through the stranded 88; nothing traced it"
        );
        pool.shutdown();
    }

    /// The `is_marking()` gate is the **barrier's** obligation, not this
    /// handle's — see [`ZMarkHandle::mark_live`] for the decision and the three
    /// reasons. What the handle owes is visibility, and this pins it.
    #[test]
    fn a_mark_after_end_cycle_is_counted_late_and_cannot_leak_into_the_next_cycle() {
        let g = graph(&[(1, &[]), (44, &[])]);
        let ctx = Arc::new(TestMarkContext::new(g));
        let pool = ZMarkCoordinator::new(ctx.clone(), 1);
        let handle = pool.handle();

        pool.begin_cycle();
        pool.push_roots(&[1]);
        let report = pool.mark_to_completion(RESTART_BUDGET);
        assert!(!report.budget_exhausted);
        assert_eq!(
            report.stats.late_marks, 0,
            "nothing inside a cycle may be counted late"
        );
        pool.end_cycle();

        // A late call still marks: the handle does not gate, deliberately.
        assert!(!handle.is_marking());
        assert!(handle.mark_live(0, 44));
        assert_eq!(
            pool.stats().late_marks.load(Ordering::Relaxed),
            1,
            "a mark with no cycle in flight must be COUNTED, so a barrier that \
             gets its own gate wrong is visible in telemetry instead of \
             silently setting bits for a finished cycle"
        );

        // ...and the stale entry cannot survive into the next cycle, which is
        // why the (inherently racy) gate is not needed for correctness:
        // `begin_cycle` clears the ingress and the stripes outright, and the
        // stale mark bit belongs to a colour the next cycle flips away from.
        ctx.reset_marks();
        pool.begin_cycle();
        assert_eq!(
            pool.try_end_mark(),
            ZMarkEndResult::Complete,
            "begin_cycle must have discarded the late entry"
        );
        pool.end_cycle();
        pool.shutdown();
    }

    // -- termination abort wakeup (FINDING B) -------------------------------

    /// Everything [`ZMarkTerminator::worker_idle`] needs, so the handshake can
    /// be driven directly instead of raced for through a whole pool.
    struct TerminatorRig {
        stripes: ZMarkStripeSet,
        ingress: ZMarkIngress,
        stats: ZMarkStats,
        stop: AtomicBool,
        term: ZMarkTerminator,
    }

    /// FINDING B (2026-08-07). When the last worker out finds work in the
    /// termination probe it re-counts itself active and returns `Resume` — and
    /// it must also **publish a wakeup**, or the other `n-1` workers stay
    /// parked for the rest of the cycle and the pool silently runs
    /// single-threaded.
    ///
    /// # The interleaving, built rather than raced for
    ///
    /// A parked worker waits for `work_generation` to move away from the value
    /// it captured on the way in. The trap is that the bump for the *queued*
    /// work may already have been **spent**: a worker publishes (bumping), and
    /// a colleague whose steal sweep had already passed that stripe then parks
    /// — at the new value. No further bump is pending, so its 5 ms poll re-reads
    /// the identical number forever. It rejoins only once the resuming worker
    /// publishes, which needs `local.len() >= 32` with its own stripe empty
    /// (`maybe_share_early`) or `>= 256` (`publish_half`); on a deep, narrow
    /// graph neither ever happens.
    ///
    /// The rig below reproduces that state exactly: queue the work, spend its
    /// bump, *then* let the colleague park.
    ///
    /// # No wall clock anywhere
    ///
    /// The primary assertion is on `work_generation`, which is deterministic.
    /// The participation assertion that follows is on a recorded **outcome**,
    /// not on elapsed time, and its spin budget only has to cover a condvar
    /// wakeup — the 5 ms poll cannot substitute for the missing bump, which is
    /// precisely why this test measures the bump and not the notification.
    /// [`ZMarkStats::yields`] is deliberately not used: it is bumped at the
    /// drain loop's head only, so a worker resumed and re-parked before
    /// reaching that head yields without counting.
    #[test]
    fn a_termination_abort_publishes_a_wakeup_for_the_parked_workers() {
        const SPIN_BUDGET: u64 = 20_000_000;

        let rig = Arc::new(TerminatorRig {
            stripes: ZMarkStripeSet::new(8),
            ingress: ZMarkIngress::new(),
            stats: ZMarkStats::default(),
            stop: AtomicBool::new(false),
            term: ZMarkTerminator::new(2),
        });
        let cycle = rig.term.arm();
        assert_eq!(rig.term.active_workers(), 2);

        // Queue work and SPEND its generation bump.
        let mut work = vec![101u64, 102, 103];
        assert_eq!(rig.stripes.publish(0, &mut work), 3);
        rig.term.note_work_published();

        // The colleague now parks at that same, already-current generation.
        let outcome_slot: Arc<Mutex<Option<ZIdleOutcome>>> = Arc::new(Mutex::new(None));
        let t_rig = Arc::clone(&rig);
        let t_slot = Arc::clone(&outcome_slot);
        let colleague = std::thread::spawn(move || {
            let o = t_rig.term.worker_idle(
                cycle,
                &t_rig.stripes,
                &t_rig.ingress,
                &t_rig.stats,
                &t_rig.stop,
            );
            *t_slot.lock() = Some(o);
        });

        // Bounded, and asserts nothing about how long it took.
        let mut spins: u64 = 0;
        while rig.term.active_workers() != 1 && spins < SPIN_BUDGET {
            spins += 1;
            std::thread::yield_now();
        }
        assert_eq!(
            rig.term.active_workers(),
            1,
            "the colleague never reached the handshake; the rest of this test \
             would be vacuous"
        );

        let gen_before = rig.term.work_generation();

        // Be the last worker out: active 1 -> 0, the probe finds the queued
        // work, and the abort path runs.
        let mine = rig
            .term
            .worker_idle(cycle, &rig.stripes, &rig.ingress, &rig.stats, &rig.stop);
        assert_eq!(
            mine,
            ZIdleOutcome::Resume,
            "the probe must have found the work that is sitting in stripe 0"
        );
        assert_eq!(rig.stats.termination_aborts.load(Ordering::Relaxed), 1);
        assert_ne!(
            rig.term.work_generation(),
            gen_before,
            "FINDING B: the abort re-counted itself active but published no \
             wakeup. The colleague is parked at generation {gen_before} and its \
             5 ms poll re-reads that same value, so it can only rejoin once we \
             publish >= 32 or >= 256 entries — on a deep, narrow graph, never."
        );

        // Participation: the parked worker must actually rejoin the cycle.
        let mut spins: u64 = 0;
        while outcome_slot.lock().is_none() && spins < SPIN_BUDGET {
            spins += 1;
            std::thread::yield_now();
        }
        // Release it unconditionally so this test can never hang, whatever the
        // outcome was.
        rig.stop.store(true, Ordering::Release);
        rig.term.wake_all();
        colleague.join().expect("colleague thread");

        assert_eq!(
            *outcome_slot.lock(),
            Some(ZIdleOutcome::Resume),
            "the parked worker never rejoined; with 2 workers that is the whole \
             pool collapsing to one for the rest of the cycle"
        );
        assert_eq!(
            rig.term.active_workers(),
            2,
            "both workers must be counted active again after the abort"
        );
    }

    /// The abort's wakeup must not become a storm: a worker woken into an
    /// already-emptied pool has to settle, not spin.
    ///
    /// This is the failure the fix could plausibly have traded for, and this
    /// tree has been bitten by a livelock-for-throughput trade before. The
    /// structural reason it cannot happen is asserted here: the abort branch is
    /// reachable **only** by the last worker out (`active` 1 -> 0), and it
    /// immediately re-counts itself active, so `active` cannot reach 0 again —
    /// and the branch cannot re-fire — until that worker has itself drained and
    /// re-offered. Each abort therefore corresponds to work that really was
    /// queued, and the count is bounded by real publishes.
    #[test]
    fn an_abort_wakeup_settles_instead_of_storming() {
        let rig = TerminatorRig {
            stripes: ZMarkStripeSet::new(8),
            ingress: ZMarkIngress::new(),
            stats: ZMarkStats::default(),
            stop: AtomicBool::new(false),
            term: ZMarkTerminator::new(1),
        };
        let cycle = rig.term.arm();

        let mut work = vec![7u64];
        rig.stripes.publish(0, &mut work);
        rig.term.note_work_published();

        // Sole worker: abort fires, generation moves once.
        let g0 = rig.term.work_generation();
        assert_eq!(
            rig.term
                .worker_idle(cycle, &rig.stripes, &rig.ingress, &rig.stats, &rig.stop),
            ZIdleOutcome::Resume
        );
        let g1 = rig.term.work_generation();
        assert_ne!(g1, g0);

        // Take the work, as the resuming worker does, and re-offer. The queues
        // are empty now, so the probe must terminate rather than abort again —
        // one wakeup per abort, and no abort without work.
        let mut taken = Vec::new();
        assert_eq!(rig.stripes.drain_from(0, &mut taken, Z_MARK_STEAL_CAP), 1);
        assert_eq!(
            rig.term
                .worker_idle(cycle, &rig.stripes, &rig.ingress, &rig.stats, &rig.stop),
            ZIdleOutcome::Terminated
        );
        assert_eq!(
            rig.term.work_generation(),
            g1,
            "a terminating probe must not publish a wakeup — that would be the \
             storm: a generation that keeps moving with no work behind it"
        );
        assert_eq!(rig.stats.termination_aborts.load(Ordering::Relaxed), 1);
        assert_eq!(rig.stats.terminations.load(Ordering::Relaxed), 1);
    }

    // -- safepoint yielding -------------------------------------------------

    /// The safepoint yield, with the interleaving *constructed* rather than
    /// raced for.
    ///
    /// The first worker to enter `visit_refs` parks inside it, which pins
    /// `ZMarkPauseControl::in_drain >= 1` until a releaser thread lets it go —
    /// and the releaser only does so once the pause has actually been
    /// requested. So at the instant `pause_for_safepoint` stores the flag,
    /// a worker is *known* to be inside a drain region, and the guard's
    /// construction is a real assertion that the coordinator waited for it.
    /// Without the handoff, a 5000-node chain is often fully marked before the
    /// test thread reaches its first pause, and the test passes vacuously.
    ///
    /// Every spin here is bounded and falls through, exactly as in
    /// `termination_does_not_fire_while_a_worker_holds_work_in_flight`: this
    /// test asserts on the mark **set**, never on elapsed time, and can never
    /// hang waiting for an interleaving that did not materialise.
    #[test]
    fn workers_yield_for_a_safepoint_and_resume_without_losing_work() {
        // Bounded-spin budget; see the doc comment. A spin count, never a
        // duration — nothing here asserts on wall clock.
        const SPIN_BUDGET: u64 = 20_000_000;

        let mut g: FxHashMap<u64, Vec<u64>> = FxHashMap::default();
        let mut expected = Vec::new();
        // Ids start at 1 — address `0` is this engine's null; see the comment
        // in `concurrent_mutator_marks_are_folded_in_by_the_restart_loop`.
        for i in 1..=5000u64 {
            g.insert(i, if i < 5000 { vec![i + 1] } else { Vec::new() });
            expected.push(i);
        }

        let in_visit = Arc::new(StdAtomicBool::new(false));
        let released = Arc::new(StdAtomicBool::new(false));
        let gate_taken = Arc::new(AtomicUsize::new(0));

        let hook_in_visit = Arc::clone(&in_visit);
        let hook_released = Arc::clone(&released);
        let hook_taken = Arc::clone(&gate_taken);
        let ctx = Arc::new(
            TestMarkContext::new(g).with_visit_hook(Box::new(move |_addr: u64| {
                // Only the very first visit of the run gates; every later one
                // passes straight through, so this costs one atomic per
                // scanned object and cannot stall the drain.
                if hook_taken.fetch_add(1, Ordering::SeqCst) != 0 {
                    return;
                }
                hook_in_visit.store(true, Ordering::SeqCst);
                let mut spins: u64 = 0;
                while !hook_released.load(Ordering::Acquire) && spins < SPIN_BUDGET {
                    spins += 1;
                    std::thread::yield_now();
                }
            })),
        );

        let pool = ZMarkCoordinator::new(ctx.clone(), 3);

        pool.begin_cycle();
        assert_eq!(pool.push_roots(&[1]), 1, "the root must be accepted");
        pool.start_marking();

        // Releaser: frees the gated worker only once a pause has actually been
        // requested, so `wait_until_quiescent` is guaranteed to have started
        // with a non-empty drain region. Bounded, so a missed interleaving
        // degrades coverage instead of deadlocking.
        let releaser_shared = Arc::clone(pool.shared());
        let releaser_released = Arc::clone(&released);
        let releaser = std::thread::spawn(move || {
            let mut spins: u64 = 0;
            while !releaser_shared.pause().pause_requested() && spins < SPIN_BUDGET {
                spins += 1;
                std::thread::yield_now();
            }
            releaser_released.store(true, Ordering::Release);
        });

        // Wait until a worker is demonstrably inside `visit_refs`.
        let mut spins: u64 = 0;
        while !in_visit.load(Ordering::SeqCst) && spins < SPIN_BUDGET {
            spins += 1;
            std::thread::yield_now();
        }
        let gated = in_visit.load(Ordering::SeqCst) && !released.load(Ordering::Acquire);

        // Take a "safepoint" a few times while marking is in flight. The
        // guard's construction is the assertion: it returns only once every
        // worker has left its drain region.
        for _ in 0..3 {
            let guard = pool.pause_for_safepoint();
            // Marking is quiescent here; nothing may be inside visit_refs.
            drop(guard);
            std::thread::yield_now();
        }
        released.store(true, Ordering::Release);
        releaser.join().expect("releaser thread");

        pool.wait_for_fixed_point();
        assert_eq!(pool.try_end_mark(), ZMarkEndResult::Complete);
        pool.end_cycle();

        assert_eq!(
            ctx.marked_sorted(),
            sorted(expected),
            "a safepoint yield dropped work that had already been popped into a \
             worker's local stack (gated={gated})"
        );
        pool.shutdown();
    }

    // -- reference processing hook ------------------------------------------

    struct KeepAliveHook {
        /// Referents to resurrect if they are not already strongly marked.
        candidates: Vec<u64>,
        /// Records what liveness the hook observed, so the test can prove the
        /// hook saw the *final* strong-reachability answer.
        observed_live: Mutex<Vec<u64>>,
    }

    impl ZNonStrongRefHook for KeepAliveHook {
        fn process(&self, is_marked: &dyn Fn(u64) -> bool, keep_alive: &mut dyn FnMut(u64)) {
            let mut live = self.observed_live.lock();
            for &c in &self.candidates {
                if is_marked(c) {
                    live.push(c);
                } else {
                    // Policy says keep it: resurrect.
                    keep_alive(c);
                }
            }
        }
    }

    #[test]
    fn the_non_strong_ref_hook_resurrects_and_the_redrain_traces_the_closure() {
        let g = graph(&[
            (1, &[2]),
            (2, &[]),
            // Soft-reachable island, unreachable strongly.
            (60, &[61]),
            (61, &[62]),
            (62, &[]),
        ]);
        let ctx = Arc::new(TestMarkContext::new(g));
        let pool = ZMarkCoordinator::new(ctx.clone(), 2);

        pool.begin_cycle();
        pool.push_roots(&[1]);
        let first = pool.mark_to_completion(RESTART_BUDGET);
        assert!(!first.budget_exhausted);
        assert_eq!(ctx.marked_sorted(), vec![1, 2]);

        let hook = KeepAliveHook {
            candidates: vec![2, 60],
            observed_live: Mutex::new(Vec::new()),
        };
        let resurrected = pool.process_non_strong_refs(&hook);
        assert_eq!(resurrected, 1, "only 60 was dead; 2 was already live");
        assert_eq!(
            hook.observed_live.lock().clone(),
            vec![2],
            "the hook must see the final strong-reachability answer, and must \
             not be told an object is live merely because the hook itself \
             just marked it"
        );

        // The resurrection has unscanned out-edges — re-drain.
        let second = pool.mark_to_completion(RESTART_BUDGET);
        pool.end_cycle();
        assert!(!second.budget_exhausted);
        assert_eq!(
            ctx.marked_sorted(),
            vec![1, 2, 60, 61, 62],
            "a resurrected referent drags its whole subgraph with it"
        );
        pool.shutdown();
    }

    // -- pool lifecycle -----------------------------------------------------

    #[test]
    fn a_pool_with_no_roots_reaches_a_fixed_point_immediately() {
        let ctx = Arc::new(TestMarkContext::new(graph(&[(1, &[])])));
        let pool = ZMarkCoordinator::new(ctx.clone(), 4);
        pool.begin_cycle();
        let report = pool.mark_to_completion(RESTART_BUDGET);
        pool.end_cycle();
        assert_eq!(report.passes, 1);
        assert_eq!(report.restarts, 0);
        assert_eq!(ctx.marked_count(), 0);
        pool.shutdown();
    }

    #[test]
    fn successive_cycles_reuse_the_same_pool() {
        let g = graph(&[(1, &[2]), (2, &[3]), (3, &[]), (10, &[11]), (11, &[])]);
        let ctx = Arc::new(TestMarkContext::new(g));
        let pool = ZMarkCoordinator::new(ctx.clone(), 3);

        for round in 0..4 {
            ctx.reset_marks();
            pool.begin_cycle();
            let roots: &[u64] = if round % 2 == 0 { &[1] } else { &[10] };
            pool.push_roots(roots);
            let report = pool.mark_to_completion(RESTART_BUDGET);
            pool.end_cycle();
            assert!(!report.budget_exhausted, "round {round}");
            let expected: Vec<u64> = if round % 2 == 0 {
                vec![1, 2, 3]
            } else {
                vec![10, 11]
            };
            assert_eq!(ctx.marked_sorted(), expected, "round {round}");
        }
        pool.shutdown();
    }

    #[test]
    fn dropping_the_coordinator_stops_and_joins_every_worker() {
        let ctx = Arc::new(TestMarkContext::new(graph(&[(1, &[])])));
        {
            let pool = ZMarkCoordinator::new(ctx.clone(), 4);
            pool.begin_cycle();
            pool.push_roots(&[1]);
            pool.start_marking();
            // Dropped here with a cycle armed: Drop must stop and join, not
            // detach. A detached worker outliving its heap is exactly the
            // process-global-state failure mode this module avoids.
        }
        assert_eq!(ctx.marked_sorted(), vec![1]);
    }

    #[test]
    fn ingress_buckets_do_not_alias_across_slots() {
        let ingress = ZMarkIngress::new();
        for slot in 0..Z_MARK_INGRESS_BUCKETS {
            ingress.push(slot, 1000 + slot as u64);
        }
        assert!(ingress.has_work());
        assert_eq!(ingress.pending_hint(), Z_MARK_INGRESS_BUCKETS);
        let mut out = Vec::new();
        assert_eq!(ingress.drain_into(&mut out), Z_MARK_INGRESS_BUCKETS);
        assert_eq!(sorted(out).len(), Z_MARK_INGRESS_BUCKETS);
        assert!(!ingress.has_work());
    }
}

#[cfg(test)]
mod pool_cost {
    use super::*;

    /// How much of the parallel marker's per-cycle handicap is the POOL
    /// ITSELF: spawning `n` OS threads at `begin`, and stopping and joining
    /// them at `drop`.
    ///
    /// Asked before building a persistent pool, because the arithmetic decides
    /// whether one is worth building. The 2026-09-02 re-measurement put the
    /// parallel marker ~2 ms/cycle behind the serial loop at four workers and
    /// ~6 ms behind at one; if construction is a small fraction of that, a
    /// persistent pool cannot close it and the cost is in the coordination
    /// protocol instead.
    #[test]
    #[ignore = "timing measurement; wants --release and a quiet box"]
    fn measure_the_pool_construction_cost() {
        const ROUNDS: usize = 20;
        let mut graph = rustc_hash::FxHashMap::default();
        graph.insert(1u64, vec![2u64]);
        graph.insert(2u64, vec![]);
        let ctx: Arc<dyn ZMarkContext> = Arc::new(TestMarkContext::new(graph));

        for workers in [1usize, 2, 4, 8] {
            let start = std::time::Instant::now();
            for _ in 0..ROUNDS {
                let pool = ZMarkCoordinator::new(Arc::clone(&ctx), workers);
                pool.begin_cycle();
                pool.end_cycle();
                drop(pool);
            }
            let per = start.elapsed() / ROUNDS as u32;
            eprintln!(
                "[pool-cost] workers={workers} construct+begin+end+drop = {per:?} per cycle"
            );
        }
    }
}
