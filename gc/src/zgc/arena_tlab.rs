// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! The arena-backed thread-local allocation buffer.
//!
//! Split out of `zgc.rs` on 2026-09-02: an adapter with one job -- carve a
//! chunk out of the shared arena, hand objects out of it without taking the
//! arena lock, and give the tail back at every collection -- and ~900 lines
//! of a file that had outgrown review.
//!
//! A CHILD module of `zgc`, so `impl super::ZgcRealHeap` here reaches the
//! heap's private fields exactly as the parent does.
//!
//! This is NOT `zgc::tlab`. That module buffers out of `zgc::page`'s page
//! allocator; this one buffers out of `crate::arena::Arena`, because that is
//! what `ZgcRealHeap` actually allocates from. It implements `zgc::tlab`'s
//! `ZTlabHeapHooks` trait so the two share a contract and their statistics are
//! comparable.

use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;

use parking_lot::Mutex;
use rustc_hash::FxHashMap;

use crate::tlab::Tlab;

use super::recycled_chunk_size;

use super::tlab::{ZTlabConfig, ZTlabHeapHooks, ZTlabStats};
use super::vaddr::ZColor;
use super::{
    current_thread_key, zgc_corpse_enabled, ZgcRealHeap, ZGC_TLAB_ALIGN, ZGC_TLAB_MAX_CHUNK,
    ZGC_TLAB_RESERVATION_SHARE,
};

/// Runtime kill switch: `CRATONVM_ZGC_TLAB`. **Default on.**
///
/// `0` / `off` / `false` / `no` (case-insensitive) disable the TLAB fast path;
/// anything else — including an unset variable — enables it. Read through
/// [`cratonvm_types::flags::runtime_var_os`] so it participates in the same
/// `-XX:` layering every other flag does, but deliberately **not** declared as
/// a [`cratonvm_types::GcFlags`] field: a declared flag latches on first read,
/// which makes a mid-run `set_var` invisible, and the whole point of this
/// switch is that a suite can A/B it. (Declaring it would also mean editing
/// `types/`, which this change does not touch.)
///
/// Read once per heap, in [`ZgcRealHeap::with_capacity`], into
/// [`ZgcRealHeap::tlab_enabled`].
pub(crate) fn zgc_tlab_enabled_by_default() -> bool {
    match cratonvm_types::flags::runtime_var_os("CRATONVM_ZGC_TLAB") {
        Some(raw) => {
            let v = raw.to_string_lossy().trim().to_ascii_lowercase();
            !matches!(v.as_str(), "0" | "off" | "false" | "no")
        }
        None => true,
    }
}

/// The reserved footprint of a `bytes` request at [`ZGC_TLAB_ALIGN`].
///
/// Mirrors [`Tlab::alloc_initialized`]'s own `footprint` computation, so the
/// bytes charged to [`ZgcRealHeap::allocated`] are the bytes the buffer
/// actually consumed. (There is no *leading* padding to account for: the
/// cursor never leaves the 8-byte grid because every request uses
/// [`ZGC_TLAB_ALIGN`].)
#[inline]
/// Kill switch for the mutator-owned TLAB chunks: `CRATONVM_ZGC_MUTATOR_TLAB=0`.
pub(crate) fn mutator_tlab_enabled() -> bool {
    use std::sync::OnceLock;
    static G: OnceLock<bool> = OnceLock::new();
    *G.get_or_init(|| {
        !matches!(
            cratonvm_types::flags::runtime_var("CRATONVM_ZGC_MUTATOR_TLAB").as_deref(),
            Ok("0") | Ok("false") | Ok("off") | Ok("no")
        )
    })
}

pub(crate) fn zgc_tlab_footprint(bytes: usize) -> usize {
    bytes.saturating_add(ZGC_TLAB_ALIGN - 1) & !(ZGC_TLAB_ALIGN - 1)
}

/// What one [`ZgcRealHeap::retire_all_tlabs`] did.
///
/// A distinct type rather than `zgc::tlab::ZTlabRetireSummary` for one honest
/// reason: that type's `waste_bytes` is documented as "tail bytes made
/// parseable by a filler" — i.e. *abandoned*. On this arena-backed adapter the
/// tail is handed straight back to the arena free list, so it is recovered,
/// not wasted, and reporting it under a field named `waste_bytes` would be a
/// lie a capacity investigation could act on.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct ZArenaTlabRetireSummary {
    /// Buffers visited.
    pub tlabs: usize,
    /// Buffers that actually held a chunk (the rest were already retired).
    pub live_chunks: usize,
    /// Chunk-tail bytes returned to the arena free list by this retirement.
    pub tail_bytes_returned: u64,
    /// Registered slots dropped because their owning thread has exited.
    pub slots_pruned: usize,
    /// Buffers left alone because their cell was locked — see
    /// [`ZgcRealHeap::retire_all_tlabs`] on why this is a safe skip and not a
    /// missed obligation. Expected to be zero; a non-zero value means a peer
    /// was mid-allocation (or forcibly stopped inside one) when the collection
    /// started.
    pub skipped_locked: usize,
}

/// A thread-private bump buffer carved out of [`ZgcRealHeap`]'s arena.
///
/// Wraps [`crate::tlab::Tlab`] by value — the same reuse decision
/// `gc::zgc::tlab::ZTlab` makes, and for the same reason: the bump path, the
/// "reserve the aligned FOOTPRINT" rule, the two-case tail filler and the
/// idempotent retire each carry a scar from a real defect, and a second copy
/// would have to be fixed twice.
pub(crate) struct ZArenaTlab {
    /// The shared TLAB doing the bump.
    pub(crate) inner: Tlab,
    /// `[start, end)` of the current chunk, or `None` when retired. Recorded
    /// separately because [`Tlab::retire`] nulls its own pointers and the
    /// retire path still needs the extent.
    pub(crate) chunk: Option<(usize, usize)>,
    /// Accounting, in `zgc::tlab`'s shape so the two allocators' numbers are
    /// comparable.
    ///
    /// [`ZTlabStats::waste_bytes`] stays **zero** here and that is deliberate,
    /// not an omission: nothing is abandoned on this adapter — see
    /// [`ZArenaTlabRetireSummary`]. Recovered tail bytes are counted in
    /// [`Self::tail_returned_bytes`] instead. `pages_taken` likewise stays zero
    /// (there are no pages), and `direct_*` stays zero (there is no
    /// keep-the-buffer direct path — see [`ZgcRealHeap::tlab_refill`]).
    pub(crate) stats: ZTlabStats,
    /// Cumulative chunk-tail bytes handed back to the arena free list.
    pub(crate) tail_returned_bytes: u64,
}

impl ZArenaTlab {
    pub(crate) fn new() -> Self {
        Self {
            inner: Tlab::empty(),
            chunk: None,
            stats: ZTlabStats::default(),
            tail_returned_bytes: 0,
        }
    }

    /// **Fast path.** A plain load / add / compare / store on thread-private
    /// memory: no CAS, no arena lock, no free-list scan, no `write_bytes`.
    ///
    /// The `Release` fence inside [`Tlab::alloc_initialized`] stays (it orders
    /// the caller's header stores ahead of the cursor commit for a cross-thread
    /// STW reader, and emits no instruction on x86-64).
    #[inline]
    fn alloc(&mut self, bytes: usize) -> Option<usize> {
        let ptr = self.inner.alloc(bytes, ZGC_TLAB_ALIGN)?;
        let footprint = zgc_tlab_footprint(bytes);
        self.stats.fast_allocations += 1;
        self.stats.fast_bytes += footprint as u64;
        Some(ptr as usize)
    }
}

/// Every live [`ZArenaTlab`] on one heap, so a safepoint can retire all of them.
///
/// # What is reused from `gc::zgc::tlab`, and the one thing that is not
///
/// Reused verbatim: [`ZTlabHeapHooks`] (implemented for [`ZgcRealHeap`] below),
/// [`ZTlabConfig`] and its `normalized()` clamping, [`ZTlabStats`], and
/// [`current_thread_key`]. The bump itself is [`crate::tlab::Tlab`], which is
/// what `ZTlab` wraps too.
///
/// **Not reused: `ZTlab` / `ZTlabCell` / `ZTlabRegistry` themselves**, because
/// their refill source is a [`crate::zgc::page::ZPageAllocator`] and
/// `ZgcRealHeap` has no pages. That is not a stylistic difference:
///
/// * `ZTlab::refill` is private and takes `&ZPageAllocator`; there is no
///   public entry point that installs a chunk from arbitrary memory.
/// * `ZgcRealHeap::collect_garbage`'s sweep returns every dead object to the
///   **arena** free list (`arena.add_free_block(base - arena_base, size)`)
///   under nothing but a `base >= arena_base` screen. Objects carved from page
///   storage that happens to sit above the arena would be turned into
///   free-list offsets far past the arena's `Vec`, and the arena would then
///   hand out pointers outside its own allocation. Adopting page-backed
///   chunks therefore *requires* changing the sweep, which this change is
///   explicitly not doing.
///
/// So the chunk source — item 1 of the five ZGC-specific things `zgc::tlab`'s
/// header lists — is the one part re-derived here, over the arena. Items 2
/// (page handle) and 3 (allocation colour) do not exist on this heap; items 4
/// (registry/accounting hooks) and 5 (a registry of live TLABs) are what this
/// type and the [`ZTlabHeapHooks`] impl provide.
///
/// # Scoping
///
/// Instance-owned: no `static`, no `OnceLock`. Two heaps in one process (which
/// the gc unit tests routinely create) get two independent sets of buffers.
/// The only process-global is a monotonic *id* counter, which is an identity,
/// not a cache — it exists so a thread-local handle cache cannot alias a
/// recycled heap address.
///
/// # Lock order
///
/// `registry.slots -> ZArenaTlab cell -> ZgcRealHeap::arena` and
/// `ZArenaTlab cell -> ZgcRealHeap::registry`. Every method here snapshots
/// under `slots` and releases it before locking a cell, and no path takes the
/// arena or the object registry before a cell, so the reverse edge does not
/// exist.
pub struct ZArenaTlabRegistry {
    /// Process-unique identity, used only to key the thread-local handle cache.
    pub(crate) id: u64,
    /// Registered buffers, as an atomic so the refill path can read it without
    /// taking [`Self::slots`].
    ///
    /// Taking that lock at refill time would be a lock-order INVERSION: the
    /// documented order is `slots -> cell -> arena`, and a refill runs with the
    /// cell already held. Maintained at the two places `slots` changes size
    /// (insert in `attach`, prune in `retire_all_tlabs`), so it is exact
    /// between them and never more than one insert stale.
    pub(crate) live_slots: AtomicUsize,
    /// Normalised policy. `initial_chunk == 0` means "this heap is too small
    /// for a TLAB" — see [`Self::chunk_bytes_for_capacity`].
    pub(crate) config: ZTlabConfig,
    pub(crate) slots: Mutex<FxHashMap<u64, Arc<Mutex<ZArenaTlab>>>>,
}

/// Monotonic registry ids. Never reused, so a stale thread-local entry for a
/// dropped heap can never be mistaken for a live one that happens to have been
/// allocated at the same address.
static NEXT_ZTLAB_REGISTRY_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

thread_local! {
    /// This thread's [`ZArenaTlab`] handle per registry id.
    ///
    /// A *cache*, not a registry: the authoritative map is
    /// [`ZArenaTlabRegistry::slots`], which is per-heap. Without it every
    /// allocation would pay a mutex plus a hash lookup to find its own buffer,
    /// which is most of the cost the TLAB exists to remove.
    ///
    /// Keyed by [`ZArenaTlabRegistry::id`] and never by address, so an entry
    /// left behind by a dropped heap is unreachable rather than aliasable. Such
    /// an entry keeps an `Arc` alive whose chunk points into freed arena
    /// memory — that is inert: nothing reads it (the id will never match again)
    /// and dropping it runs no destructor that touches the chunk.
    static ZGC_TLAB_HANDLES: std::cell::RefCell<Vec<(u64, Arc<Mutex<ZArenaTlab>>)>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

impl ZArenaTlabRegistry {
    /// Chunk size for an arena of `capacity` bytes, or `0` to disable.
    ///
    /// `capacity / 1024`, clamped into `[crate::tlab::min_tlab_size(),
    /// ZGC_TLAB_MAX_CHUNK]`, and refused outright when one chunk would be more
    /// than a quarter of the whole heap — at that point the buffer is a second
    /// heap rather than a buffer, and the honest answer is to allocate the way
    /// this backend always has. (The 8 KiB unit tests take that branch.)
    pub(crate) fn chunk_bytes_for_capacity(capacity: usize) -> usize {
        let want = (capacity / 1024).clamp(crate::tlab::min_tlab_size(), ZGC_TLAB_MAX_CHUNK);
        if want.saturating_mul(4) > capacity {
            return 0;
        }
        want & !(ZGC_TLAB_ALIGN - 1)
    }

    /// The chunk size to carve RIGHT NOW, for an arena of `capacity` bytes.
    ///
    /// [`Self::chunk_bytes_for_capacity`] fixes a ceiling once, at
    /// construction. This divides the reservation budget
    /// ([`ZGC_TLAB_RESERVATION_SHARE`]) by the buffers actually registered, so
    /// the chunk shrinks as the thread count rises and the total claimed by
    /// TLABs stays bounded whatever the workload does.
    ///
    /// `0` (the too-small-heap answer) stays `0`: the ceiling decides whether
    /// this heap has TLABs at all, and this only decides how big they are.
    pub(crate) fn chunk_bytes_now(&self, capacity: usize) -> usize {
        let ceiling = self.config.initial_chunk;
        if ceiling == 0 {
            return 0;
        }
        let live = self.live_slots.load(Ordering::Relaxed).max(1);
        let budget = capacity / ZGC_TLAB_RESERVATION_SHARE;
        let want = (budget / live).clamp(crate::tlab::min_tlab_size(), ceiling);
        want & !(ZGC_TLAB_ALIGN - 1)
    }

    /// A registry sized for an arena of `capacity` bytes.
    pub(crate) fn for_capacity(capacity: usize) -> Self {
        let chunk = Self::chunk_bytes_for_capacity(capacity);
        let config = ZTlabConfig {
            initial_chunk: chunk,
            min_chunk: chunk,
            max_chunk: chunk,
            // An object big enough to eat an eighth of a chunk is not worth
            // carving one for; it goes to `alloc_raw`, which is exactly where
            // it went before this change.
            max_tlab_alloc: (chunk / 8).min(crate::tlab::tlab_max_alloc()),
            // HotSpot's refill-waste policy exists to avoid ABANDONING a large
            // chunk remainder. This adapter abandons nothing — the remainder
            // goes back to the arena free list at retire — so the policy has no
            // work to do and the ratchet that guards its pathological middle is
            // likewise unnecessary. Stated as 1/0 rather than left at the
            // defaults so that "there is no waste policy" is visible here.
            refill_waste_fraction: 1,
            waste_increment: 0,
            // BATCHING IS REVERTED, and this is where that is declared.
            //
            // `ZTlabHeapHooks`'s doc argues for per-chunk batched registry
            // inserts, and states the precondition: "a collection only ever
            // observes the registry at a safepoint, and `retire_all` — which
            // every collection must call first — flushes every pending batch
            // before it returns."
            //
            // That precondition does not hold for `ZgcRealHeap`.
            // `is_object_address` is not a GC-only predicate here: the JIT
            // calls it on the *mutator* path as its "is this a valid object
            // pointer" oracle (`vm/src/jit/helpers.rs` — `jit_checkcast`
            // resolves its receiver through it and turns a miss into a silent
            // null, and the conservative-root scanner in
            // `vm/src/jit/conservative_roots.rs` validates every candidate
            // qword through it *before* `collect_garbage` is entered). An
            // object sitting in an unflushed batch would answer `false` to both
            // — a mutator-visible miscompare and a use-after-free respectively,
            // neither of which any safepoint ordering can fix.
            //
            // So a TLAB-served object is inserted into the registry the moment
            // it is handed out, and `registry_batch: 1` says so. The arena
            // mutex — which covers free-list tier scans and a per-object
            // `write_bytes`, not a pointer bump — is what this change removes;
            // the object registry's single hash insert stays. Sharding
            // `ZgcRealHeap::registry` is the obvious follow-up and is a
            // separate change.
            registry_batch: 1,
        }
        .normalized();
        Self {
            id: NEXT_ZTLAB_REGISTRY_ID.fetch_add(1, Ordering::Relaxed),
            // `normalized()` clamps a zero chunk up to the object grid, which
            // would re-enable a TLAB this heap is too small for. Restore the
            // sentinel.
            config: if chunk == 0 {
                ZTlabConfig {
                    initial_chunk: 0,
                    max_tlab_alloc: 0,
                    ..config
                }
            } else {
                config
            },
            slots: Mutex::new(FxHashMap::default()),
            live_slots: AtomicUsize::new(0),
        }
    }

    /// The policy in force.
    pub(crate) fn config(&self) -> &ZTlabConfig {
        &self.config
    }

    /// Is a TLAB available on this heap at all?
    pub(crate) fn is_active(&self) -> bool {
        self.config.initial_chunk > 0
    }

    /// This thread's buffer, creating and registering it on first use.
    ///
    /// `try_with` rather than `with`: a thread-local is inaccessible while its
    /// own destructors run, and an allocation from a TLS destructor must fall
    /// back to the map rather than panic.
    pub(crate) fn attach(&self) -> Arc<Mutex<ZArenaTlab>> {
        let cached = ZGC_TLAB_HANDLES
            .try_with(|c| {
                c.borrow()
                    .iter()
                    .find(|(id, _)| *id == self.id)
                    .map(|(_, cell)| Arc::clone(cell))
            })
            .ok()
            .flatten();
        if let Some(cell) = cached {
            return cell;
        }
        let key = current_thread_key();
        let cell = {
            let mut slots = self.slots.lock();
            match slots.get(&key) {
                Some(existing) => Arc::clone(existing),
                None => {
                    let cell = Arc::new(Mutex::new(ZArenaTlab::new()));
                    slots.insert(key, Arc::clone(&cell));
                    self.live_slots.store(slots.len(), Ordering::Relaxed);
                    cell
                }
            }
        };
        let _ = ZGC_TLAB_HANDLES.try_with(|c| c.borrow_mut().push((self.id, Arc::clone(&cell))));
        cell
    }

    /// Snapshot of every registered buffer. Taken under `slots` and returned
    /// with the guard released — the caller locks cells, and the reverse edge
    /// must not exist.
    pub(crate) fn cells(&self) -> Vec<Arc<Mutex<ZArenaTlab>>> {
        self.slots.lock().values().map(Arc::clone).collect()
    }
}

impl std::fmt::Debug for ZArenaTlabRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ZArenaTlabRegistry")
            .field("id", &self.id)
            .field("chunk_bytes", &self.config.initial_chunk)
            .field("max_tlab_alloc", &self.config.max_tlab_alloc)
            .field("live_tlabs", &self.slots.lock().len())
            .finish()
    }
}

/// The bookkeeping a TLAB-served object still needs, because it never touched
/// [`ZgcRealHeap::alloc_raw`].
///
/// This is `zgc::tlab`'s trait, implemented against the heap it was written
/// for. It is the whole of the contract: registry membership, the `allocated`
/// counter, the native-allocation-pressure latch, an identity-hash source and
/// an allocation colour.
impl ZTlabHeapHooks for ZgcRealHeap {
    /// Register freshly allocated object bases and charge their footprint.
    ///
    /// `bytes` is the **reserved footprint**, which is what `allocated` has to
    /// count for `needs_gc` to be right.
    ///
    /// The threshold test below is [`GarbageCollector::needs_gc`]'s predicate
    /// verbatim — `allocated >= gc_threshold && allocated >= gc_rearm` — copied
    /// from [`ZgcRealHeap::alloc_raw`] rather than re-derived, and that is the
    /// whole reason this path cannot reopen the GC storm the
    /// [`gc_rearm`](ZgcRealHeap::gc_rearm) field exists to prevent: a live set
    /// parked above the static threshold leaves `gc_rearm` above `allocated`
    /// after every sweep, so the latch stays DOWN until genuinely new
    /// allocation clears the re-arm floor. Nothing here writes `gc_rearm`;
    /// only the sweep does.
    fn register_allocations(&self, addrs: &[usize], bytes: usize) {
        if zgc_corpse_enabled() {
            // Per-object sizes are not passed, but a TLAB hands out bases in
            // increasing order inside one chunk, so consecutive entries bound
            // each other. The LAST has no successor -- and skipping it was a
            // hole, because the last object in a batch is exactly the one with
            // no upper bound on its extent. Bound it by what the batch as a
            // whole reserved: `bytes` covers every object in it, so
            // `first + bytes` is at or above the last one's end.
            let batch_end = addrs.first().map_or(0, |f| f.saturating_add(bytes));
            for (i, &a) in addrs.iter().enumerate() {
                let sz = match addrs.get(i + 1) {
                    Some(next) => next.saturating_sub(a),
                    None => batch_end.saturating_sub(a),
                };
                self.audit_registry_insert(a, sz, "tlab_batch");
            }
        }
        self.registry.insert_all(addrs);
        // `CRATONVM_DBG_VACATED_FRAMES`: these addresses are live objects again,
        // so a reference to one is no longer evidence that a holder went stale.
        // See `gc_quiescence::note_allocated`.
        crate::gc_quiescence::note_allocated(addrs);
        if bytes == 0 {
            return;
        }
        let after = self.allocated.fetch_add(bytes, Ordering::Relaxed) + bytes;
        if after >= self.gc_threshold && after >= self.gc_rearm.load(Ordering::Relaxed) {
            self.native_alloc_pressure.store(true, Ordering::Relaxed);
        }
    }

    /// [`ZColor::Remapped`] — the quiescent good colour, unconditionally.
    ///
    /// The same answer, for the same reason, as this file's
    /// [`mark::ZMarkContext::good_mask`] impl: **`ZgcRealHeap` stores plain
    /// machine pointers and colours nothing.** Liveness lives in the object
    /// header's [`GC_FLAG_MARKED`] bit, not in a pointer's metadata bits, and
    /// no load barrier runs on this backend. Any answer is therefore inert and
    /// the only question is which one is honest: `Z_REMAPPED` is what
    /// [`vaddr::ZGoodMask::new`] installs and what `mark::mark_color_for` maps
    /// to `None`, so a metrics line reads "this cycle has no mark colour"
    /// rather than claiming a `Marked0`/`Marked1` parity nothing maintains.
    /// Returning a mark colour would be a lie a future barrier could act on.
    ///
    /// When colored pointers land this becomes `self.good_mask.allocation_color()`.
    fn allocation_color(&self) -> ZColor {
        ZColor::Remapped
    }

    /// Identity-hash source.
    ///
    /// Provided because the trait requires it, but **nothing on this heap calls
    /// it**: contrary to the shape the trait's doc assumes,
    /// [`ZgcRealHeap::alloc_array`] does not stamp a hash at allocation — it
    /// leaves the mark word zero and lets
    /// [`GarbageCollector::identity_hash_code`] install one lazily through
    /// `ObjectHeader::mark_word_identity_hash`. A TLAB-served array therefore
    /// needs nothing extra, which is why [`ZgcRealHeap::alloc_tlab`] does not
    /// consult this.
    ///
    /// `ZgcRealHeap::next_hash` is the *inherent* method (inherent candidates
    /// are resolved before trait ones, so this is not a self-call).
    fn next_hash(&self) -> i32 {
        ZgcRealHeap::next_hash(self)
    }

    /// Charge bytes consumed from the arena that will never hold a live object.
    ///
    /// Implemented to the contract — and **not called by this adapter**, which
    /// is the load-bearing half of the note. `zgc::tlab` assumes a retired
    /// chunk tail is abandoned inside a page whose reclamation is a whole-page
    /// event; an arena has a free list, so
    /// [`ZgcRealHeap::tlab_retire_locked`] hands the tail straight back and the
    /// bytes are *recovered*, not wasted. Charging them here as well would
    /// double-count the same span and re-arm the collection trigger early.
    fn note_waste(&self, bytes: usize) {
        if bytes == 0 {
            return;
        }
        self.allocated.fetch_add(bytes, Ordering::Relaxed);
    }
}

impl ZgcRealHeap {
    /// Is the TLAB fast path armed on this heap?
    ///
    /// False either because `CRATONVM_ZGC_TLAB` (or
    /// [`Self::set_tlab_enabled`]) turned it off, or because the heap is too
    /// small to carve a chunk from — see
    /// [`ZArenaTlabRegistry::chunk_bytes_for_capacity`].
    pub fn tlab_enabled(&self) -> bool {
        self.tlab_enabled.load(Ordering::Relaxed) && self.tlabs.is_active()
    }

    /// Flip the kill switch at runtime. See [`Self::tlab_enabled`].
    ///
    /// Turning it off leaves already-issued chunks in place; they are closed
    /// and returned by the next [`Self::retire_all_tlabs`], which is *not*
    /// gated on this flag.
    pub fn set_tlab_enabled(&self, enabled: bool) {
        self.tlab_enabled.store(enabled, Ordering::Relaxed);
    }

    /// Aggregate TLAB accounting across every registered buffer.
    ///
    /// [`ZTlabStats::waste_bytes`] is always zero here and that is the honest
    /// answer — this adapter abandons nothing. Recovered tail bytes are
    /// [`Self::tlab_tail_returned_bytes`].
    pub fn tlab_stats(&self) -> ZTlabStats {
        let mut total = ZTlabStats::default();
        for cell in self.tlabs.cells() {
            total.add(&cell.lock().stats);
        }
        total
    }

    /// Cumulative chunk-tail bytes handed back to the arena free list by
    /// retirement, across every registered buffer.
    ///
    /// The counterpart of [`ZTlabStats::waste_bytes`] on a page-backed TLAB:
    /// there the tail is abandoned, here it is recovered. Buffers reaped by
    /// [`Self::retire_all_tlabs`]'s dead-thread prune drop their contribution,
    /// so this is a live-buffer figure, not a lifetime total.
    pub fn tlab_tail_returned_bytes(&self) -> u64 {
        self.tlabs
            .cells()
            .iter()
            .map(|cell| cell.lock().tail_returned_bytes)
            .sum()
    }

    /// **The tripwire.** Reserved `[cursor, end)` tails of every registered
    /// buffer that still holds a live chunk.
    ///
    /// Empty immediately after [`Self::retire_all_tlabs`]. A non-empty result
    /// at a walk point names exactly the spans that would have been walked as
    /// objects — far more useful than the SIGSEGV three phases later.
    pub fn tlab_reserved_tails(&self) -> Vec<(usize, usize)> {
        let mut out: Vec<(usize, usize)> = Vec::new();
        for cell in self.tlabs.cells() {
            if let Some(span) = cell.lock().inner.reserved_tail() {
                out.push(span);
            }
        }
        out.sort_unstable();
        out
    }

    /// **Retire every registered TLAB.** The safepoint entry point, and the
    /// precondition every heap-walking path in this file states.
    ///
    /// When this returns: no registered buffer holds a live chunk, every
    /// chunk tail has been closed with `zgc::tlab`'s filler sentinels and
    /// returned to the arena free list, and every object a TLAB served is in
    /// [`Self::registry`] (it was inserted at hand-out time — see
    /// [`ZArenaTlabRegistry`]'s `registry_batch` note).
    ///
    /// Cells are snapshotted under the `slots` lock, which is released before
    /// any cell is taken, so a thread that is mid-allocation (holding its cell,
    /// waiting on the arena) can never be the far side of a deadlock with a
    /// thread that is attaching.
    ///
    /// # `try_lock`, and why blocking here would be a hang, not a handshake
    ///
    /// `zgc::tlab::ZTlabRegistry::retire_all` blocks on a peer's cell
    /// deliberately: over a *page*-backed TLAB, retiring is a correctness
    /// precondition (an un-retired chunk desyncs a `top`-bounded linear page
    /// walk), so waiting is the only safe answer and blocking substitutes for
    /// an OS-level suspension handshake.
    ///
    /// **On this heap it is not a correctness precondition.** Every walker —
    /// the mark snapshot, the sweep, the census walk, `walk_objects`,
    /// `is_heap_addr` — is driven by [`Self::registry`], so a live chunk's
    /// un-handed-out tail is *invisible* rather than misparsed, and every
    /// object inside a chunk is already registered. Blocking would therefore
    /// buy nothing and risk everything: BUG-03 says this VM does forcibly stop
    /// peers (a frozen in-JIT thread), and one stopped inside its own bump
    /// would hold its cell for the rest of the process — the collector would
    /// wait on it forever. That is the exact failure this change exists to
    /// remove, reintroduced one layer down.
    ///
    /// So an uncontended cell is retired and a contended one is **skipped and
    /// counted** ([`ZArenaTlabRetireSummary::skipped_locked`]). The cost of a
    /// skip is that one chunk tail is not returned to the arena this cycle; the
    /// next collection gets it. This is the same "skip list rather than block"
    /// resolution `zgc::tlab::ZTlabRegistry::reserved_tails` offers for BUG-03.
    ///
    /// Not gated on [`Self::tlab_enabled`]: a chunk issued before the kill
    /// switch was flipped still has to be closed and returned.
    pub fn retire_all_tlabs(&self) -> ZArenaTlabRetireSummary {
        let cells = self.tlabs.cells();
        let mut summary = ZArenaTlabRetireSummary {
            tlabs: cells.len(),
            ..ZArenaTlabRetireSummary::default()
        };
        for cell in cells.iter() {
            let Some(mut tlab) = cell.try_lock() else {
                summary.skipped_locked += 1;
                continue;
            };
            if tlab.chunk.is_some() {
                summary.live_chunks += 1;
            }
            summary.tail_bytes_returned += self.tlab_retire_locked(&mut tlab) as u64;
        }
        // Drop our own handles BEFORE the prune below reads `Arc::strong_count`.
        drop(cells);
        // Reap buffers whose owning thread has exited. A live thread always
        // holds a second `Arc` — either in `ZGC_TLAB_HANDLES` for its whole
        // life, or on its stack while it is inside `attach`/`alloc_tlab` — so
        // `strong_count == 1` means "only the map still refers to this", i.e.
        // the owner is gone. Every such buffer was retired one loop above, so
        // dropping it releases nothing the arena still needs. Without this the
        // map grows by one entry per thread ever created, and `ThreadId`s are
        // never reused, so nothing would ever remove them.
        {
            let mut slots = self.tlabs.slots.lock();
            let before = slots.len();
            slots.retain(|_, cell| Arc::strong_count(cell) > 1);
            summary.slots_pruned = before - slots.len();
            self.tlabs.live_slots.store(slots.len(), Ordering::Relaxed);
        }
        // The per-buffer tripwire is asserted inside `tlab_retire_locked`,
        // while that buffer's cell lock is still held — race-free, and strictly
        // stronger than a sweep of `tlab_reserved_tails()` after the fact. A
        // global assert here would be a false alarm waiting to happen: this
        // entry point is reachable from `walk_objects` and from the census
        // driver, neither of which stops the world, so a peer may legitimately
        // refill between the loop and the check. `tlab_reserved_tails()` stays
        // available as the tripwire for a caller that IS at a safepoint.
        if summary.skipped_locked > 0 {
            self.counters.tlab_retire_skipped_total
                .fetch_add(summary.skipped_locked, Ordering::Relaxed);
        }
        if summary.live_chunks > 0 || summary.slots_pruned > 0 || summary.skipped_locked > 0 {
            tracing::debug!(
                target: "zgc",
                tlabs = summary.tlabs,
                live_chunks = summary.live_chunks,
                tail_bytes_returned = summary.tail_bytes_returned,
                slots_pruned = summary.slots_pruned,
                skipped_locked = summary.skipped_locked,
                "zgc real: retired TLABs",
            );
        }
        summary
    }

    /// Close one buffer's chunk and hand its tail back to the arena.
    ///
    /// Returns the tail bytes returned. Idempotent: a second call finds no
    /// chunk and does nothing, which matters because several paths can retire
    /// the same buffer with no synchronisation between them (a collection, then
    /// a census walk, then a thread teardown) — exactly as [`Tlab::retire`]'s
    /// own idempotence note describes.
    ///
    /// The caller must hold the cell lock. The arena lock is taken *inside*
    /// (cell -> arena, never the reverse) and is not held across anything else.
    pub(crate) fn tlab_retire_locked(&self, tlab: &mut ZArenaTlab) -> usize {
        let Some((chunk_start, chunk_end)) = tlab.chunk.take() else {
            return 0;
        };
        // Read the tail BEFORE `retire()`, which nulls the cursor.
        let tail = tlab.inner.reserved_tail();
        // Installs the `int[]` filler over `[cursor, end)`, or the
        // `GAP_FILLER_CLASS_ID` sentinel for a sub-header tail. Strictly
        // speaking this heap has no linear walker to protect — its sweep,
        // its census walk and `walk_objects` are all driven by
        // `ZgcRealHeap::registry`, so an un-parsed span is invisible rather
        // than fatal. It is installed anyway: the span is about to enter the
        // arena free list shared with every other allocator in this crate, the
        // write is one header, and "walkable" is the state the rest of the tree
        // assumes of arena bytes below the cursor.
        tlab.inner.retire();
        tlab.stats.retires += 1;
        debug_assert!(tlab.inner.reserved_tail().is_none());
        let Some((tail_start, tail_end)) = tail else {
            return 0;
        };
        debug_assert!(tail_start >= chunk_start && tail_end <= chunk_end);
        let bytes = tail_end - tail_start;
        if bytes == 0 {
            return 0;
        }
        // Return the tail to the free list rather than abandoning it. This is
        // the difference between an arena and a page, and it is not an
        // optimisation: `retire_all_tlabs` runs at EVERY collection, so every
        // thread abandons a partly-used chunk on every cycle. At 64 KiB and 50
        // threads that is up to 3 MiB per collection of memory no walker can
        // ever see again (the filler is not in `registry`, so the sweep never
        // visits it and never frees it). A few dozen cycles would exhaust the
        // heap. `note_waste` is therefore deliberately NOT called for these
        // bytes — they come back.
        {
            let mut arena = self.arena.lock();
            let base = arena.base_ptr() as usize;
            if tail_start >= base {
                arena.add_free_block(tail_start - base, bytes);
            }
        }
        // `stats.waste_bytes` stays zero on purpose — nothing was wasted. The
        // recovered bytes are counted separately.
        tlab.tail_returned_bytes += bytes as u64;
        bytes
    }

    /// Carve a fresh chunk out of the arena for `tlab`.
    ///
    /// `Some(())` means the caller's retry is guaranteed to fit. The chunk is
    /// no longer always `initial_chunk` — a recycled one may be shorter, see
    /// the note in the body — but it is never shorter than `need`, which is
    /// what the guarantee actually rests on. `None` means the arena could not
    /// serve a chunk; the caller falls back to [`Self::alloc_raw`], which is
    /// the pre-TLAB path unchanged.
    ///
    /// There is no keep-the-buffer "direct" path and no HotSpot refill-waste
    /// ratchet, because the remainder is recovered rather than abandoned — see
    /// [`ZArenaTlabRegistry::for_capacity`].
    // ── Mutator-owned TLAB chunks (the `JvmThread::tlab` the JIT bumps) ──
    //
    // `VmHeap::refill_tlab` returned `None` on this collector, so the inline
    // TLAB bump both JIT tiers emit never hit under the default collector:
    // `thread.tlab` stayed empty and every compiled `new` was a helper call
    // into `alloc_raw_tlab`. This gives the thread a chunk carved exactly the
    // way this heap's own `ZArenaTlab` chunks are, with two differences that
    // follow from the chunk being owned by VM code this heap cannot see into:
    //
    // * every object bump-allocated from it is registered EAGERLY, one at a
    //   time, by `note_mutator_tlab_object` -- called from the interpreter's
    //   `alloc_initialized` closure site and from the JIT's post-init helper,
    //   both after the header is written -- instead of through a pending
    //   batch, because nothing on this side sees the allocation happen;
    // * the unused tail comes back through the `Tlab::retire` hook registered
    //   in `new_shared` (`return_mutator_tail`), not through
    //   `retire_all_tlabs`, which only reaches this heap's own cells.
    //
    // Kill switch: `CRATONVM_ZGC_MUTATOR_TLAB=0` makes `refill_mutator_tlab`
    // answer `None` again, which is byte-for-byte the previous behaviour.

    /// Carve a chunk for a mutator thread's own `Tlab`, or `None` when the
    /// heap cannot serve one (switch off, TLABs disabled, chunk budget zero,
    /// request larger than a chunk, arena exhausted).
    pub fn refill_mutator_tlab(&self, requested: usize) -> Option<(*mut u8, usize)> {
        if !mutator_tlab_enabled() || !self.tlab_enabled.load(Ordering::Relaxed) {
            return None;
        }
        let want = self
            .tlabs
            .chunk_bytes_now(self.arena_end.saturating_sub(self.arena_base));
        if want == 0 || requested == 0 || requested > want {
            return None;
        }
        let carved = {
            let mut arena = self.arena.lock();
            let largest = arena.largest_low_free_block();
            let headroom = arena.low_bump_headroom();
            let sized = recycled_chunk_size(
                want,
                requested,
                largest,
                headroom,
                self.publish_vacated_enabled.load(Ordering::Relaxed),
                arena.low_free_blocks_at_least(
                    (want / 8).min(crate::tlab::tlab_max_alloc()),
                    2,
                ) >= 2,
            )
            .and_then(|size| arena.alloc(size, ZGC_TLAB_ALIGN).map(|p| (p, size)));
            sized.or_else(|| arena.alloc(want, ZGC_TLAB_ALIGN).map(|p| (p, want)))
        };
        let (ptr, size) = carved?;
        // The refill invariant both JIT tiers rely on: a chunk is zero-filled,
        // so the inline bump writes only the header.
        unsafe { std::ptr::write_bytes(ptr, 0, size) };
        self.note_young_page_span(ptr as usize, size);
        self.counters.mutator_tlab_refills.fetch_add(1, Ordering::Relaxed);
        self.counters
            .mutator_tlab_refill_bytes
            .fetch_add(size, Ordering::Relaxed);
        Some((ptr, size))
    }

    /// Register one object bump-allocated from a mutator chunk: the
    /// object-start registry, the quiescence note, the allocation account and
    /// the GC trigger it feeds, and black allocation under a live mark -- the
    /// same four things `alloc_raw` does for an object it hands out itself.
    /// Called AFTER the header is written (black allocation reads it).
    pub fn note_mutator_tlab_object(&self, addr: usize, size: usize) {
        self.audit_registry_insert(addr, size, "mutator_tlab");
        self.registry.insert(addr);
        crate::gc_quiescence::note_allocated(&[addr]);
        let footprint = zgc_tlab_footprint(size);
        let after = self.allocated.fetch_add(footprint, Ordering::Relaxed) + footprint;
        if after >= self.gc_threshold && after >= self.gc_rearm.load(Ordering::Relaxed) {
            self.native_alloc_pressure.store(true, Ordering::Relaxed);
        }
        self.allocate_black_if_marking(addr as *mut u8);
        self.counters.mutator_tlab_objects.fetch_add(1, Ordering::Relaxed);
    }

    /// The `Tlab::retire` hook: a reserved tail inside this arena goes back on
    /// the free list, exactly as `tlab_retire_locked` returns an own cell's.
    /// `false` when the tail is not this heap's, so another hook may take it.
    pub fn return_mutator_tail(&self, tail_start: usize, tail_end: usize) -> bool {
        if tail_end <= tail_start {
            return false;
        }
        let mut arena = self.arena.lock();
        let base = arena.base_ptr() as usize;
        let end = base.saturating_add(arena.capacity());
        if tail_start < base || tail_end > end {
            return false;
        }
        arena.add_free_block(tail_start - base, tail_end - tail_start);
        drop(arena);
        self.counters
            .mutator_tlab_tail_bytes
            .fetch_add(tail_end - tail_start, Ordering::Relaxed);
        true
    }

    /// `(refills, refill bytes, objects registered, tail bytes returned)` --
    /// the engagement census; a zero in the first column under the default
    /// collector means the switch is off or the chunk budget is zero.
    pub fn mutator_tlab_counts(&self) -> (u64, usize, u64, usize) {
        (
            self.counters.mutator_tlab_refills.load(Ordering::Relaxed),
            self.counters.mutator_tlab_refill_bytes.load(Ordering::Relaxed),
            self.counters.mutator_tlab_objects.load(Ordering::Relaxed),
            self.counters.mutator_tlab_tail_bytes.load(Ordering::Relaxed),
        )
    }

    /// Generational bookkeeping for a whole chunk: `alloc_raw` notes one
    /// address per object; a chunk notes its span once.
    fn note_young_page_span(&self, addr: usize, size: usize) {
        if !self.generational_enabled.load(Ordering::Relaxed) {
            return;
        }
        self.mark_young_pages(addr, addr.saturating_add(size));
    }

    pub(crate) fn tlab_refill(&self, tlab: &mut ZArenaTlab, need: usize) -> Option<()> {
        // Sized against the LIVE buffer count, not once at construction — see
        // `ZGC_TLAB_RESERVATION_SHARE`. A request too big for the current chunk
        // takes `alloc_raw`, which is where it went before TLABs existed.
        let want = self
            .tlabs
            .chunk_bytes_now(self.arena_end.saturating_sub(self.arena_base));
        if want == 0 || need > want {
            return None;
        }
        self.tlab_retire_locked(tlab);
        // Take the arena lock for the bump ONLY. The zeroing below is the
        // expensive half and must not be inside it — that is the very
        // serialisation this whole section exists to remove.
        let carved = {
            let mut arena = self.arena.lock();
            // ACCEPT A SHORTER CHUNK WHEN THE FREE LIST HAS ONE.
            //
            // This is the difference between an allocator that recycles and one
            // that only consumes. A retired chunk gives back the span BELOW its
            // survivors, so a chunk that held even one live object comes back
            // SHORTER than a chunk — and a fixed-size request for `want` can
            // then never be served by the remains of any chunk that ever
            // contained a survivor. The free list fills with near-chunk-sized
            // spans that only a bump can be an alternative to, and the bump is
            // finite.
            //
            // Measured on `TestNonBlockingAPI` (2026-08-13, `-Xmx 2g`) with the
            // fixed-size request: **3,828 free spans of 524,192 bytes** — one
            // per thread that had ever parked — against a 524,288-byte chunk
            // request. Every one of them was **96 bytes short**: exactly one
            // `AbstractQueuedSynchronizer$ConditionNode`. 1.96 GB of a 2.15 GB
            // heap sat on the free list in pieces that were each one small
            // object short of reusable, so every refill in the process bumped,
            // the arena reached capacity, and a 2 MB `char[]` had nowhere to go.
            //
            // The floor is `want / 8` — the same `max_tlab_alloc` bound that
            // decides what a TLAB will serve at all, so a chunk at the floor
            // still holds at least eight of the largest object it can ever be
            // asked for. Below that a buffer is churning rather than buffering
            // and the honest answer is a full-size chunk (or the failure that
            // follows it).
            let largest = arena.largest_low_free_block();
            let headroom = arena.low_bump_headroom();
            let sized = recycled_chunk_size(
                want,
                need,
                largest,
                headroom,
                self.publish_vacated_enabled.load(Ordering::Relaxed),
                // ">= 2" is the point: one of them is the block being offered.
                //
                // The reserve is `max_tlab_alloc`, recomputed from `want` the
                // way `ZTlabConfig` computes it. NOT `ZGC_LARGE_OBJECT_MIN`:
                // that was the first cut and it is a disguised kill switch,
                // because the starved regime is by definition
                // `largest_low_free < want / 8` = 64 KiB, so a spare block of
                // 64 KiB can never exist while the rung is being asked and the
                // gate refuses every time. Measured: it took
                // `TestKillProcessWhileWriting` from PASS to FAIL, which is
                // exactly what turning the rung off does.
                //
                // `max_tlab_alloc` is the largest object a TLAB will ever
                // serve, so it is the largest allocation that can fall back to
                // a direct arena request when its own buffer cannot take it —
                // which is the request this rung is competing with. Reserving
                // one block that size is a real discriminator inside the
                // starved band rather than a refusal of the whole band.
                arena.low_free_blocks_at_least(
                    (want / 8).min(crate::tlab::tlab_max_alloc()),
                    2,
                ) >= 2,
            )
                .and_then(|size| arena.alloc(size, ZGC_TLAB_ALIGN).map(|p| (p, size)));
            // Which RUNG served this refill. Counted at the call site rather
            // than inside `recycled_chunk_size` so the pure function stays
            // pure and testable; the two rungs are separated by the same
            // `want / 8` the function uses.
            if let Some((_, size)) = sized {
                if size >= want / 8 {
                    self.counters.tlab_refill_recycled.fetch_add(1, Ordering::Relaxed);
                } else {
                    self.counters.tlab_refill_starved.fetch_add(1, Ordering::Relaxed);
                    self.counters.tlab_refill_starved_bytes
                        .fetch_add(size, Ordering::Relaxed);
                }
            }
            // `alloc(want)` covers both the "the free list has a full-size
            // block" case and the bump.
            sized.or_else(|| arena.alloc(want, ZGC_TLAB_ALIGN).map(|p| (p, want)))
        };
        let (ptr, want) = carved?;
        // An arena block can carry stale bytes: a split remainder, or the tail
        // filler header a previous retire left behind. `alloc_raw` zeroes per
        // object for exactly this reason; the TLAB pays it once per chunk
        // instead, so `Tlab::alloc` can hand out already-zero memory the way
        // `zgc::tlab` does over a zeroed page reservation.
        // SAFETY: `arena.alloc` guarantees `want` valid bytes at `ptr`, and the
        // span is exclusively ours until retire.
        unsafe { std::ptr::write_bytes(ptr, 0, want) };
        // SAFETY: `[ptr, ptr + want)` was just reserved from the arena and is
        // owned exclusively by this thread until retire; it is zeroed above;
        // `want` is a multiple of `ZGC_TLAB_ALIGN`, which is what `Tlab::new`'s
        // tail-filler contract requires of `ptr + want`.
        tlab.inner = unsafe { Tlab::new(ptr, want) };
        tlab.chunk = Some((ptr as usize, ptr as usize + want));
        tlab.stats.refills += 1;
        tlab.stats.refill_bytes += want as u64;
        // ---- ALLOCATE-BLACK, ONCE PER CHUNK ------------------------------
        //
        // Under snapshot-at-the-beginning every object allocated during a cycle
        // must be born marked, or the sweep frees everything the mutators
        // allocated while the marker ran. That was one atomic `fetch_or` per
        // OBJECT on the allocation fast path -- 22.4M of them on the
        // single-threaded probe `Z_CONC_START_PERCENT_DEFAULT` records, which
        // is part of why that measurement's wall clock rose 37-55%.
        //
        // This chunk is space only this thread can allocate into, so the whole
        // of it can be blackened now: one `fetch_or` per 512 arena bytes rather
        // than one per object, and no atomic at all on the bump path
        // afterwards. `alloc_tlab`'s `black_end` test is what collects the
        // saving.
        //
        // Stamped with the epoch, never with a bare flag -- see
        // the bitmap itself -- see `allocate_black_if_marking`.
        if self.mark_active.load(Ordering::Relaxed) {
            self.blacken_range(ptr as usize, ptr as usize + want);
        }
        // ...and the same for the young set, once per CHUNK rather than once
        // per object: a chunk is contiguous and every object it will serve is
        // inside it. See `note_young_page`.
        if self.generational_enabled.load(Ordering::Relaxed) {
            self.mark_young_pages(ptr as usize, ptr as usize + want);
        }
        Some(())
    }

    /// Bump-allocate `size` zeroed bytes from this thread's TLAB.
    ///
    /// Returns `None` when the caller must take [`Self::alloc_raw`]: the kill
    /// switch is off, the heap is too small for a chunk, the object is larger
    /// than `max_tlab_alloc`, or the arena could not serve a refill.
    ///
    /// # Accounting
    ///
    /// Each object charges its own **footprint** to `allocated` through
    /// [`ZTlabHeapHooks::register_allocations`], in the same instant it enters
    /// [`Self::registry`] — the chunk itself is never charged, so nothing is
    /// counted twice. The residual is that the not-yet-handed-out part of a
    /// live chunk is invisible to `allocated`, bounded by
    /// `live_threads * chunk` and squeezed by [`ZGC_TLAB_MAX_CHUNK`]. That
    /// errs on the *late* side of the collection trigger, which is the same
    /// direction `alloc_raw` already errs (it charges the unrounded `size`
    /// while the arena consumes the rounded footprint plus alignment padding),
    /// and it collapses to zero at every collection because
    /// [`Self::retire_all_tlabs`] runs first and hands every remainder back.
    ///
    /// Charging the whole chunk at refill instead would make `allocated`
    /// exact between collections and wrong across one: a sweep republishes
    /// `allocated` as the surviving *object* bytes, so a threshold crossed by
    /// chunk reservations would not still be crossed afterwards, and the
    /// parked-live-set re-arm reasoning (`gc_rearm`) would be reading two
    /// different quantities on either side of the sweep.
    pub(crate) fn alloc_tlab(&self, size: usize) -> Option<*mut u8> {
        if !self.tlab_enabled.load(Ordering::Relaxed) {
            return None;
        }
        let config = self.tlabs.config();
        if config.initial_chunk == 0 || size == 0 || size > config.max_tlab_alloc {
            return None;
        }
        let cell = self.tlabs.attach();
        let mut tlab = cell.lock();
        let addr = match tlab.alloc(size) {
            Some(addr) => addr,
            None => {
                self.tlab_refill(&mut tlab, size)?;
                tlab.alloc(size)?
            }
        };
        // Registered INSIDE the cell lock, and before the pointer is returned
        // to anyone: `is_object_address` is a mutator-path oracle on this heap,
        // not just a GC one, so the window in which a live object is absent
        // from the registry has to be as narrow as `alloc_raw`'s.
        // NOT the place to allocate black. `allocate_black_if_marking` must run
        // AFTER the header write -- `try_alloc_object` `ptr::write`s the whole
        // `ObjectHeader` over this address, and on the header arm that clobbers
        // the flags byte, so a bit set here would be silently erased. The four
        // callers that own the header write are where it happens; the per-chunk
        // blackening below is what makes it cheap there.
        <Self as ZTlabHeapHooks>::register_allocations(
            self,
            std::slice::from_ref(&addr),
            zgc_tlab_footprint(size),
        );
        Some(addr as *mut u8)
    }

    /// [`Self::alloc_tlab`] with [`Self::alloc_raw`] as the fallback.
    ///
    /// The single funnel every object and array allocation on this backend now
    /// takes. The fallback is the pre-TLAB path byte-for-byte, so a heap with
    /// the kill switch off behaves exactly as it did before this change.
    #[inline]
    pub(crate) fn alloc_raw_tlab(&self, size: usize) -> Option<*mut u8> {
        match self.alloc_tlab(size) {
            Some(ptr) => Some(ptr),
            None => self.alloc_raw(size),
        }
    }
}

