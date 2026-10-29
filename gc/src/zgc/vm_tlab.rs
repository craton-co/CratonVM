// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! The VM thread's TLAB on the ZGC backend.
//!
//! # What this is
//!
//! Every mutator thread owns a [`crate::tlab::Tlab`] (`JvmThread::tlab`) that
//! the interpreter's `new` and the JIT's inline allocator bump into without a
//! call. `VmHeap::refill_tlab` is how that buffer gets a chunk, and until
//! 2026-09-02 its `Zgc` arm returned `None`: the thread's buffer stayed empty
//! for the life of the process, the inline bump missed every time, and every
//! compiled allocation took the `jit_new_object` helper into
//! [`super::ZgcRealHeap::alloc_tlab`] -- an `Arc<Mutex<_>>` buffer with about
//! six atomic operations per object. The former VM-wide TLAB-hit metric was
//! zero on this backend by construction.
//!
//! This module is the `Zgc` arm. It carves chunks from the same low-arena
//! source the heap's own buffers use ([`super::ZgcRealHeap::carve_tlab_chunk`]),
//! registers each object the VM allocates into one, takes the unused tail back
//! when the thread retires the buffer, and keeps a published un-retired tail
//! out of the compactor's way.
//!
//! # The four obligations, and where each is met
//!
//! 1. **Every object is in the start registry before anyone else can see
//!    it.** The registry is a mutator-path oracle on this backend
//!    (`is_object_address`, the SATB barrier, the conservative scans), not
//!    only a GC one. So registration is per object, at the moment the header
//!    is complete: the VM calls [`super::ZgcRealHeap::note_tlab_object`] from
//!    the header-initialising closure of its TLAB path and from
//!    `jit_post_tlab_init`, the helper the inline allocator always calls. The
//!    bytes an inline bump has reserved but not yet handed to that helper are
//!    invisible to the sweep (unregistered) and cannot be handed to anyone
//!    else (below the thread's cursor), which is the same window
//!    `alloc_tlab` has between its bump and its `register_allocations`.
//!
//! 2. **Allocate-black.** A chunk carved while a concurrent mark is running
//!    is blackened whole by the carve; an object allocated into a chunk that
//!    predates the mark start is blackened per object by `note_tlab_object`
//!    (`allocate_black_if_marking` is one bitmap test when marking and one
//!    flag load when not).
//!
//! 3. **The tail goes back.** `Tlab::retire` has no heap in scope, so this
//!    heap registers as a [`crate::tlab::TlabTailSink`] (in `new_shared`) and
//!    takes `[cursor, end)` straight into the arena free list, exactly as
//!    [`super::ZgcRealHeap::tlab_retire_locked`] does for the heap's own
//!    buffers. A sink declines a span outside its arena, so several heaps in
//!    one process (every unit test) cannot take each other's tails.
//!
//! 4. **An un-retired tail is never slid over.** A thread parked at a
//!    safepoint has retired (`safepoint_check`), and so has the collector's
//!    own thread (`maybe_gc`); a thread blocked in native or frozen in
//!    compiled code has not, and the STW protocol publishes those tails via
//!    `set_jit_tlab_skip_regions`. The sweep needs nothing (it walks the
//!    registry, and a tail holds no registered base), but the slide does:
//!    `relocate_stw` withholds every page a published tail touches from the
//!    relocation set and never lowers the bump cursor below a published
//!    tail's end, so `compact_low_to` cannot zero it or hand it out.
//!
//! # Accounting
//!
//! The chunk is charged to `allocated` in full at refill and the tail is
//! credited back at retire. That is the opposite of `alloc_tlab`'s per-object
//! charge (see its doc for why that path chose so), and it is chosen here
//! because the per-object charge is a `fetch_add` on one cache line shared by
//! every allocating thread -- the cost this module exists to remove from the
//! fast path. `allocated` is republished as the live byte count by every
//! collection, so the reservation error is bounded by `threads * chunk` and
//! never survives a cycle.
//!
//! # Switch
//!
//! DEFAULT ON since round 9 wave 5 ([`ZGC_VM_TLAB_DEFAULT_ON`]);
//! `CRATONVM_ZGC_JIT_TLAB=0` is the kill switch, and then `refill_tlab`
//! answers `None` exactly as before -- nothing else in this module runs,
//! because no chunk is ever handed out. See [`zgc_vm_tlab_enabled_by_default`]
//! for the measurements that set the default. (zs s5/ta: this section still
//! said "OPT-IN" on 2026-10-15, a month after the constant moved.)

use std::cell::Cell;
use std::sync::atomic::{AtomicU64, Ordering};

use cratonvm_types::ClassId;

use super::{zgc_corpse_enabled, ZgcRealHeap, ZGC_TLAB_ALIGN};

/// `CRATONVM_ZGC_TLAB_OWNED_STARTS`: register a VM-TLAB object's start bit
/// with a plain store when its bitmap word lies wholly inside the chunk this
/// thread owns. See [`OwnedVmTlabChunk`]. Default: [`ZGC_TLAB_OWNED_STARTS_DEFAULT_ON`]
/// (ON since the round 9 wave 6 measurement below; wave 5, lane `zgc5`, had
/// shipped it OFF unmeasured). Read once per heap since zt t2/al (X1): seeded
/// into `ZArenaTlabRegistry::owned_starts`, which the refill and
/// `publish_jit_inline_announce` read; never per object.
///
/// A set value decides by `runtime_flag_on`'s rule whatever the default (empty
/// or `0`/`false`/`off`/`no` is off, anything else on), so with the constant
/// `false` this is exactly `runtime_flag_on`, and flipping the default is one
/// token plus the INVENTORY row's `off_word`.
///
/// Measured (round 9 wave 6, lane `zgc6`) on `cratonvm-jitr9-w5b.exe` (VM TLAB
/// default ON), 6 interleaved rounds, `=0` against `=1`, min (mean) ms:
/// `CratonBench bintrees` 2 020 (2 175) against **1 909 (1 994)**; `hashmap`
/// 4 804 (5 636) against 4 711 (5 545); `BtSplit` build 21.8 against **20.2**
/// ns/node; `PutScale 8 20000000` 196 against 202 (no allocation in its timed
/// loop). `ZgcTlabStress 8 200`, 4 rounds each, every output identical to
/// HotSpot's: `=1` 12 031-13 881 against `=0` 12 472-14 076 (both ruled by
/// `relocate_stw`). A small, consistent win and no regression: the flip is
/// recommended, with the edits listed in `NOTES-w6-zgc6.md`.
pub(crate) fn zgc_tlab_owned_starts_enabled() -> bool {
    match cratonvm_types::flags::runtime_var_os("CRATONVM_ZGC_TLAB_OWNED_STARTS") {
        Some(raw) => {
            let v = raw.to_string_lossy().trim().to_ascii_lowercase();
            !(v.is_empty() || matches!(v.as_str(), "0" | "false" | "off" | "no"))
        }
        None => ZGC_TLAB_OWNED_STARTS_DEFAULT_ON,
    }
}

/// What an unset `CRATONVM_ZGC_TLAB_OWNED_STARTS` means. See
/// [`zgc_tlab_owned_starts_enabled`] for the measurement behind flipping it.
pub(crate) const ZGC_TLAB_OWNED_STARTS_DEFAULT_ON: bool = true;

/// Bumped whenever memory that a thread's [`OwnedVmTlabChunk`] may name can be
/// handed to someone else: a collection (the sweep frees, the slide moves), a
/// retired tail that the recording thread did not itself retire, and a new
/// heap (whose arena may reuse a dropped one's addresses). A thread's record
/// is valid only while its `epoch` equals this. Starts at 1, so the empty
/// record (epoch 0) never matches.
///
/// Stored in [`crate::tlab::JIT_ZGC_ANNOUNCE`]`.epoch` since round 9 wave 8
/// (`zgc8`): the JIT's inline start-bit store compares a buffer's recorded
/// epoch against it without a call, so it has to live at an address the
/// thread's `Tlab` can hand the compiled code. Same value, same updates.
#[inline]
fn ownership_epoch() -> &'static AtomicU64 {
    &crate::tlab::JIT_ZGC_ANNOUNCE.epoch
}

/// Invalidate every thread's [`OwnedVmTlabChunk`] at once. Called by
/// `collect_garbage` and `relocate_stw` (stop-the-world, so every mutator
/// sees it when it resumes), by `with_capacity`, and by a tail reclaim that
/// the recording thread did not perform.
#[inline]
pub(crate) fn invalidate_owned_vm_tlab_chunks() {
    ownership_epoch().fetch_add(1, Ordering::AcqRel);
}

/// The epoch of this thread's owned-chunk record when that record is VALID
/// and names exactly `[lo, hi)`; `None` otherwise. `Tlab::new` asks this to
/// arm the JIT's inline start-bit store (`Tlab::zgc_owned_epoch`) for the
/// chunk the VM is installing, which is the one `refill_tlab` just carved on
/// this thread. Any other buffer -- a heap staging chunk, a stale record, a
/// record of a different chunk -- answers `None` and keeps the helper.
pub(crate) fn owned_vm_tlab_chunk_epoch(lo: usize, hi: usize) -> Option<u64> {
    owned_vm_tlab_chunk_containing(lo)
        .filter(|owned| owned.lo == lo && owned.hi == hi)
        .map(|owned| owned.epoch)
}

/// `CRATONVM_ZGC_JIT_INLINE_ANNOUNCE`: let the single-pass JIT set a VM-TLAB
/// object's start bit inline instead of calling the announce helper (round 9
/// wave 8, `zgc8`). **Default ON** since the round 9 wave 8 integration
/// (`=0` is the kill switch), measured on the wave-8 binary: `CratonBench bintrees` 1742/1734/1761 -> 1469/1468/1485 ms with both announce flags on, identical checksums; probes, `ZgcTlabStress 8 200` (SAME) and the 95-vector regression suite green with them on. The JIT reads the same variable at compile time and
/// emits the inline sequence only when it is on; this side publishes
/// [`crate::tlab::JIT_ZGC_ANNOUNCE`] only when it is on, so either half alone
/// is inert. Read once per heap (at `new_shared`). `runtime_flag_default_on`'s
/// rule: unset is on; `0`/`false`/`off`/`no` is off.
pub(crate) fn zgc_jit_inline_announce_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_ZGC_JIT_INLINE_ANNOUNCE")
}

/// `CRATONVM_ZGC_CARVE_NOTES_BLOCKS` (default ON; nio21 wave 9 L3): note the
/// object-start registry's summary blocks over every VM-TLAB chunk at carve,
/// so the JIT's inline start-bit store (which bypasses `HeapBitmap::insert`)
/// cannot leave a populated block marked "never noted". `0` restores the
/// pre-fix carve; it also turns `CRATONVM_ZGC_FORGET_EMPTY_BLOCKS` off,
/// because forgetting blocks the inline store may refill is only sound with
/// the carve re-noting them. Read once per heap since zt t2/al (X1): seeded
/// into `ZArenaTlabRegistry::carve_notes_blocks`, which the carve and the
/// forget pass read.
pub(crate) fn zgc_carve_notes_blocks_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_ZGC_CARVE_NOTES_BLOCKS")
}

/// `CRATONVM_ZGC_SKIP_COVERS_INFLIGHT` (default ON; nio21 wave 9 L3): lower
/// each published VM-TLAB tail's start over the unregistered bytes below it,
/// so a frozen peer's bumped-but-unannounced object is withheld from the sweep
/// with the tail. See `ZgcRealHeap::inflight_cover_start`. `0` publishes the
/// tails exactly as reported.
pub(crate) fn zgc_skip_covers_inflight_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_ZGC_SKIP_COVERS_INFLIGHT")
}

/// `CRATONVM_ZGC_FORGET_EMPTY_BLOCKS` (default ON; nio21 wave 9 L3): at the
/// end of each collection's sweep, clear the summary bit of every registry
/// block the sweep left empty, so the pin probe's backwards scan skips a
/// swept stretch in one probe instead of walking it. See
/// `ZgcRealHeap::forget_empty_registry_blocks`. Requires
/// [`zgc_carve_notes_blocks_enabled`].
pub(crate) fn zgc_forget_empty_blocks_enabled() -> bool {
    zgc_carve_notes_blocks_enabled()
        && cratonvm_types::flags::runtime_flag_default_on("CRATONVM_ZGC_FORGET_EMPTY_BLOCKS")
}

// zr z1/al: the slide withholds a published tail from relocation by pinning
// the pages of its two END addresses only (`relocate_stw`: "Pin both ends of
// every published tail"). That covers every page the tail touches only while
// the tail cannot span three logical pages. A tail is at most one chunk, and
// `inflight_cover_start` lowers its start by at most one more, so two chunks
// must fit in a page. Was an unstated invariant; a larger `ZGC_TLAB_MAX_CHUNK`
// would silently leave a middle page relocatable under a frozen peer's tail.
const _: () = assert!(2 * super::ZGC_TLAB_MAX_CHUNK <= ZgcRealHeap::Z_LOGICAL_PAGE_BYTES);

/// The VM-TLAB chunk the CURRENT thread carved in its last
/// [`ZgcRealHeap::refill_tlab`], while nothing can have handed any of its
/// memory to anyone else.
///
/// # What it licenses
///
/// [`ZgcRealHeap::note_tlab_object`] registers an object inside `[lo, hi)`
/// through `ZObjectStarts::insert_in_owned_chunk`: a plain store, not a
/// locked `fetch_or`, for a bitmap word wholly inside the chunk. That is sound
/// when no other thread can write such a word while this one runs. Writers of
/// a word wholly inside the chunk are: (a) whoever registers objects bumped
/// into the chunk, i.e. whoever runs the buffer over it, one thread at a time
/// with a happens-before at every hand-over; (b) the collector, only while
/// every mutator is stopped at a poll, and the registering code has no poll;
/// (c) whoever is later given part of the chunk's memory. The record exists to
/// exclude (c).
///
/// # Why a valid record excludes (c)
///
/// The chunk's memory can reach anyone else only through the arena free list
/// or a collection. Its unused tail goes to the free list only through
/// [`crate::tlab::TlabTailSink::reclaim_tlab_tail`]. If the recording thread
/// retires it, the record is cleared first. If any other thread does (a
/// virtual thread whose buffer moved carriers, a cross-thread retire), the
/// epoch is bumped first. The bumped-into part is freed only by a collection,
/// which bumps the epoch. So while a record is valid, none of its range has
/// been free, and valid records of different threads are disjoint. A thread
/// registers only objects in the buffer it is running, so it never writes a
/// word of someone else's valid range.
///
/// Every bump happens-before the reissue it guards: the reclaim bumps before
/// `add_free_block` takes the arena lock, and the carve that reissues takes
/// the same lock. A collection bumps inside the pause. The note's `Acquire`
/// load of the epoch therefore sees the bump whenever the object it registers
/// sits in reissued memory.
#[derive(Clone, Copy)]
struct OwnedVmTlabChunk {
    lo: usize,
    hi: usize,
    epoch: u64,
}

impl OwnedVmTlabChunk {
    const NONE: Self = Self {
        lo: 0,
        hi: 0,
        epoch: 0,
    };
}

thread_local! {
    static OWNED_VM_TLAB_CHUNKS: std::cell::RefCell<Vec<(std::sync::Weak<u64>, u64, OwnedVmTlabChunk)>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

#[inline]
fn owned_vm_tlab_chunk_containing(addr: usize) -> Option<OwnedVmTlabChunk> {
    OWNED_VM_TLAB_CHUNKS
        .try_with(|records| {
            records.borrow().iter().find_map(|(life, _, owned)| {
                (life.strong_count() != 0
                    && addr >= owned.lo
                    && addr < owned.hi
                    && owned.epoch == ownership_epoch().load(Ordering::Acquire))
                .then_some(*owned)
            })
        })
        .ok()
        .flatten()
}

pub(super) fn owned_vm_tlab_chunk_end_at(addr: usize) -> Option<usize> {
    owned_vm_tlab_chunk_containing(addr).map(|owned| owned.hi)
}

fn set_owned_vm_tlab_chunk(
    registry: &super::arena_tlab::ZArenaTlabRegistry,
    owned: OwnedVmTlabChunk,
) {
    let _ = OWNED_VM_TLAB_CHUNKS.try_with(|records| {
        let mut records = records.borrow_mut();
        records.retain(|(life, id, _)| life.strong_count() != 0 && *id != registry.id());
        if owned.hi > owned.lo {
            records.push((
                std::sync::Arc::downgrade(&registry.lifetime),
                registry.id(),
                owned,
            ));
        }
    });
}

/// `CRATONVM_ZGC_VM_TLAB_SHARE`: count this thread's VM-thread buffer against
/// `ZGC_TLAB_RESERVATION_SHARE`, so the chunk size shrinks with the
/// number of threads actually claiming one. **Default ON**; `0`/`false`/
/// `off`/`no` restores the pre-2026-09-20 sizing byte for byte.
///
/// # The bug it closes
///
/// `ZArenaTlabRegistry::chunk_bytes_now` — the one sizing entry point, called
/// by [`ZgcRealHeap::refill_tlab`] below and by `tlab_refill` — divides a
/// sixteenth of the arena by the number of registered buffers, because a chunk
/// is *claimed* memory no collection can reclaim while its owner lives. That
/// divisor counted only `zgc::arena_tlab`'s own cells. A thread whose
/// allocations all go through the VM buffer (which is every ordinary Java
/// thread since [`ZGC_VM_TLAB_DEFAULT_ON`] became `true`) never registers such
/// a cell, so the divisor stayed near 1 and every thread was sized as if it
/// were alone — while all of them carved from the same arena through the same
/// `carve_tlab_chunk`.
///
/// That is exactly the shape `ZGC_TLAB_RESERVATION_SHARE` was introduced to
/// fix, measured on Tomcat's `TestNonBlockingAPI` at `-Xmx2g`: ~4 000 threads ×
/// 512 KiB is the whole heap, 1.99 GB of it on the free list in pieces, and a
/// 2 MB `char[]` with nowhere to go. Re-opened, on the path that carries
/// essentially all allocation, by a default flip in a different file.
///
/// # What it costs when there is no explosion
///
/// Nothing measurable, and not by luck: the per-thread quotient is clamped up
/// to the `initial_chunk` ceiling, which at `-Xmx2g` is
/// `ZGC_TLAB_MAX_CHUNK` (512 KiB) and is not reached until roughly
/// 256 buffers. Below that the clamp decides and the divisor is inert. The
/// switch exists so the claim is falsifiable on a built binary rather than
/// argued: `=0` is the A arm.
///
/// Cached in a `OnceLock`, exactly as [`zgc_vm_tlab_tail_sink_enabled`] below
/// is and for the same reason: this is consulted once per refill, and
/// `flags::runtime_var_os`'s own census note says a flag read that is not
/// cached by its gate *is* the bug (it names `CRATONVM_DBG_COMPACT_INLINE` at
/// 99.1 % of 4.6 M reads). A refill is per-chunk rather than per-object, so the
/// uncached read would not have shown up there — which is precisely why it
/// would have stayed.
pub(crate) fn zgc_vm_tlab_share_enabled() -> bool {
    static CACHED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *CACHED.get_or_init(|| {
        cratonvm_types::flags::runtime_flag_default_on("CRATONVM_ZGC_VM_TLAB_SHARE")
    })
}

/// `(registry id, epoch)` this thread last counted its VM buffer in. Zero is
/// the never-counted value, and no live registry has id 0 (ids start at 1) and
/// no live epoch is 0 (epochs start at 1), so it can never alias.
///
/// zs s2/nd2 (`zgc-zr4ab-infallible-residuals-20261012.md` item 4): one row
/// PER HEAP, as [`VM_TLAB_RESERVED`] is. With one slot a thread alternating
/// between two heaps re-counted itself in each heap's reservation share on
/// every switch (an over-count: smaller chunks, the safe direction, but a
/// wrong share). Rows are keyed by registry id and the heap's weak lifetime,
/// with no fixed slot limit (dev's zgc-infallible rework of the per-heap
/// records); a dropped heap's row is pruned.
/// `CRATONVM_ZGC_VM_TLAB_RECORD_PER_HEAP=0` restores the single slot.
thread_local! {
    static VM_TLAB_SLOTS_COUNTED: std::cell::RefCell<Vec<VmTlabSlotRecord>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

/// One heap's row in [`VM_TLAB_SLOTS_COUNTED`]: the heap's weak lifetime, its
/// registry id, and the epoch this thread last counted itself in.
type VmTlabSlotRecord = (std::sync::Weak<u64>, u64, u64);

/// Count this thread in heap `id`'s share through `note` (given the epoch it
/// last counted in there, `0` for never; answers the epoch to keep). Rows of
/// dropped heaps are pruned; under the kill switch (`per_heap == false`) only
/// `id`'s row survives, the pre-nd2 single slot.
fn vm_tlab_slot_count(
    records: &mut Vec<VmTlabSlotRecord>,
    life: &std::sync::Arc<u64>,
    id: u64,
    per_heap: bool,
    note: impl FnOnce(u64) -> u64,
) {
    records.retain(|(l, key, _)| l.strong_count() != 0 && (per_heap || *key == id));
    if let Some((_, _, epoch)) = records.iter_mut().find(|(_, key, _)| *key == id) {
        *epoch = note(*epoch);
    } else {
        records.push((std::sync::Arc::downgrade(life), id, note(0)));
    }
}

/// `(registry id, lo, hi)` of the chunk this thread has charged to
/// [`super::arena_tlab::ZArenaTlabRegistry::reserved_bytes`] and not yet given
/// back. `(0, 0, 0)` means "nothing outstanding"; no live registry has id 0.
///
/// # Why the VM side needs a record at all
///
/// `reserved_bytes` is charged at the one carve and released at the one
/// retire. This module's buffer has no retire this crate can see: the chunk is
/// installed in a bare [`crate::tlab::Tlab`] the VM owns inside `JvmThread`,
/// and the only callback that comes back here is
/// [`crate::tlab::TlabTailSink::reclaim_tlab_tail`], which is handed the
/// **tail** and not the chunk. So the extent has to be remembered on the side,
/// and the thread that carved it is the only party that knows it.
///
/// Keyed by registry id, not by address: an entry left behind by a dropped
/// heap can never be matched against a live one, exactly as
/// `ZGC_TLAB_HANDLES` is keyed.
///
/// This is deliberately NOT [`OWNED_VM_TLAB_CHUNKS`], even though that record
/// holds the same two numbers. That one exists to authorise a *plain store*
/// into the start bitmap and is therefore written only when
/// `CRATONVM_ZGC_TLAB_OWNED_STARTS` is on (default ON, `=0` kills it) and
/// cleared by any epoch bump; reusing it would make the reservation accounting
/// silently depend on an unrelated switch and on a global epoch that a
/// collection bumps. This record is unconditional and is invalidated by
/// nothing but its own release.
///
/// # One record PER HEAP (zr z4/ab)
///
/// Records are keyed by heap and weak lifetime, with no fixed slot limit.
/// Switching among more than four VMs must not evict a live heap's charge.
thread_local! {
    static VM_TLAB_RESERVED: std::cell::RefCell<Vec<(std::sync::Weak<u64>, VmTlabCharge)>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

/// One heap's outstanding VM-TLAB charge on this thread: `(registry id, lo,
/// hi)` of the chunk charged to `reserved_bytes`, and the heap's `gc_count`
/// at that refill -- which collection the chunk's `allocated` charge belongs
/// to (zr z1/al; see [`zgc_tail_return_cycle_check_enabled`]). `id == 0` is a
/// free entry; no live registry has id 0.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct VmTlabCharge {
    id: u64,
    lo: usize,
    hi: usize,
    charged_at: usize,
}

impl VmTlabCharge {
    const NONE: Self = Self {
        id: 0,
        lo: 0,
        hi: 0,
        charged_at: 0,
    };
}

/// `CRATONVM_ZGC_VM_TLAB_RECORD_PER_HEAP` (zr z4/ab): keep this thread's
/// VM-TLAB charge record per heap. **Default ON**; `=0` restores the single
/// slot (a second heap's refill overwrites the first heap's record). Cached:
/// read once per refill.
fn zgc_vm_tlab_record_per_heap_enabled() -> bool {
    static CACHED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *CACHED.get_or_init(|| {
        cratonvm_types::flags::runtime_flag_default_on("CRATONVM_ZGC_VM_TLAB_RECORD_PER_HEAP")
    })
}

/// This thread's charge record for heap `id`, if any.
fn vm_tlab_charge(id: u64) -> Option<VmTlabCharge> {
    VM_TLAB_RESERVED
        .try_with(|records| {
            records
                .borrow()
                .iter()
                .find(|(life, e)| life.strong_count() != 0 && e.id == id)
                .map(|(_, e)| *e)
        })
        .ok()
        .flatten()
}

fn set_vm_tlab_charge(
    registry: &super::arena_tlab::ZArenaTlabRegistry,
    charge: VmTlabCharge,
    per_heap: bool,
) {
    let _ = VM_TLAB_RESERVED.try_with(|records| {
        let mut records = records.borrow_mut();
        records.retain(|(life, e)| per_heap && life.strong_count() != 0 && e.id != charge.id);
        records.push((std::sync::Arc::downgrade(&registry.lifetime), charge));
    });
}

fn take_vm_tlab_charge(id: u64) -> Option<VmTlabCharge> {
    VM_TLAB_RESERVED
        .try_with(|records| {
            let mut records = records.borrow_mut();
            let i = records.iter().position(|(_, e)| e.id == id)?;
            Some(records.remove(i).1)
        })
        .ok()
        .flatten()
}

/// `CRATONVM_ZGC_TAIL_RETURN_CYCLE_CHECK` (default ON, `=0` kills; zr z1/al):
/// a retired VM-TLAB tail gives its bytes back to `allocated` only when no
/// collection has run since the chunk was charged.
///
/// `refill_tlab` charges the whole chunk to `allocated`; a retire subtracts
/// the unused tail. But `collect_garbage` REPUBLISHES `allocated` as the live
/// bytes, which never contained the tail, so a tail retired after a
/// collection (a peer that was blocked in native or frozen across it, whose
/// tail was published and withheld) was subtracted from a figure it was not
/// in: `saturating_sub` kept it from wrapping and left the gauge low by the
/// whole tail, per such peer per cycle -- the late direction for every
/// `allocated`-driven trigger. With the check the stale subtraction is
/// skipped; a retire by a thread that is not the chunk's carver keeps the old
/// arithmetic, because only the carver knows the cycle.
///
/// The same switch RE-CHARGES the published tails (2026-10-17, the page's
/// residual): skipping the subtraction alone left every byte the resumed peer
/// bumped into its old tail after the collection charged to no figure. The
/// collection now adds each tail it withheld (`set_jit_tlab_skip_regions`) to
/// the live bytes it republishes ([`ZgcRealHeap::recharge_published_tails`]):
/// the tail is still the peer's reservation, so it stays in `allocated`, the
/// peer's later bumps are inside it, and its retire subtracts the unused rest
/// as an ordinary retire does ([`ZgcRealHeap::tail_was_recharged`]). A tail the
/// collection did not withhold keeps the skip.
///
/// Read once per heap since zt t2/al (X1): seeded into
/// `ZArenaTlabRegistry::tail_return_cycle_check`, which the tail return and
/// the collection's re-charge read.
pub(crate) fn zgc_tail_return_cycle_check_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_ZGC_TAIL_RETURN_CYCLE_CHECK")
}

/// Give back whatever this thread still has charged to `registry`, and return
/// the bytes released.
///
/// Called from two places, and it must be both:
///
/// * [`ZgcRealHeap::reclaim_tlab_tail`], the ordinary retire — the chunk is
///   over, and the part that was never handed out is going to the free list.
/// * [`ZgcRealHeap::refill_tlab`], **before** the new chunk is sized — a
///   buffer that consumed its chunk to the last byte has no tail, so
///   `Tlab::retire` calls no sink and the record would otherwise survive into
///   the next chunk and be counted twice. Releasing here also means a thread
///   can never shrink its own next chunk with its own previous one.
fn release_vm_reservation(registry: &super::arena_tlab::ZArenaTlabRegistry) -> usize {
    let taken = take_vm_tlab_charge(registry.id())
        .map(|e| e.hi.saturating_sub(e.lo))
        .unwrap_or(0);
    registry.release_chunk_reserved(taken);
    taken
}

/// `(lo, hi)` of this thread's record, valid or not. Tests only: the epoch
/// is process-wide, and any concurrent test's collection bumps it.
#[cfg(test)]
fn owned_vm_tlab_chunk_raw(id: u64) -> (usize, usize) {
    OWNED_VM_TLAB_CHUNKS.with(|records| {
        records
            .borrow()
            .iter()
            .find(|(_, key, _)| *key == id)
            .map_or((0, 0), |(_, _, owned)| (owned.lo, owned.hi))
    })
}

/// `CRATONVM_ZGC_JIT_TLAB`: hand the VM thread's TLAB a chunk on this backend.
///
/// **Default ON since round 9 wave 5** ([`ZGC_VM_TLAB_DEFAULT_ON`]; the
/// sections below are the history that led there, oldest first, and their
/// "default OFF" is the default of their day.)
///
/// Was **Default OFF.** (2026-09-02 reasoning below; 2026-09-18 re-measure after
/// the suite note.) The arm is correct — the
/// probes and both suites are green with it on — but on BinTreesClassic 16 at
/// `-Xmx512m`, release build, interleaved on a quiet host, it is *slower*
/// every round: 2841/3049/2746/2564 ms with it on against 2218/2188/2172/2007
/// ms with it off, identical checksums, and FEWER collections on the slow arm
/// (1 vs 2), so it is the allocation path itself and not collection frequency.
///
/// The cost is structural and is worth stating precisely, because it is what
/// the next attempt has to remove. This collector finds objects through an
/// allocation-base registry, so every inline allocation must be ANNOUNCED, and
/// the only existing announcement point is `jit_post_tlab_init` — a helper
/// that also mints an identity hash, looks up the class's compact layout and
/// dispatches primitive-field initialisation. The other two backends skip that
/// call entirely for a class that needs none of it (`skip_post_init_helper`),
/// so ZGC pays a call per allocation that they do not, and it costs more than
/// the `jit_new_object` preamble the inline bump saves.
///
/// The win needs a register-only helper in the JIT's ABI — one call that does
/// nothing but set the start bit — at which point this becomes a candidate for
/// default-on again, against this same measurement.
///
/// # It IS suite-clean, as of 2026-09-06 — 92/92, and the reason is one bug
///
/// This section used to read "it is not suite-clean either, and that is
/// UNTRIAGED", over five extra failures against the default arm
/// (`RJitMultiArrayClass`, `RArrayStoreLibrary`, `ROverlaySystemGcStress`,
/// `RSyncMethodJit`, `RVarHandleAccess`) and a note that five failures need
/// not be five defects. They were not. Three were closed by unrelated work
/// between 2026-09-02 and 2026-09-06; the last two were ONE defect, and it was
/// not in this module.
///
/// `jit_post_tlab_init` stamped a freshly minted identity hash at raw offset 8
/// of the object header, which was an `identity_hash_code: i32` field when
/// that store was written and has been the MARK WORD since 2026-08-07. The
/// hash went in unshifted, over the two-bit lock-state tag, so three objects
/// in four came back not-`MARK_NEUTRAL` — read afterwards as thin-locked, as
/// INFLATED (a monitor pointer synthesised from hash bits: the SIGSEGV) or as
/// FORWARDED (a relocation target synthesised the same way: the hang).
///
/// **The arm is what made that reachable, and nothing else can.** The inline
/// bump calls that helper only when `skip_helper` is false, and `skip_helper`
/// is `helper_is_noop && !jit_tlab_registration_required()` — registration is
/// required only here. Every other configuration either skips the helper or
/// never inlines, so the store was dead code everywhere else in the process.
/// It is the third defect this arm has exposed rather than caused; see the
/// two in the module doc's obligation 1.
///
/// `regression-suite/run.sh`, release binary, one run each, 2026-09-06:
/// unset **92/92**, `=1` **92/92**.
///
/// # Re-measured 2026-09-18: the single-thread loss is gone, a multi-thread one is not
///
/// Round 9 wave 3 (lane `gczgc3`), `cratonvm-jitr9-w2.exe`, interleaved
/// off/on, two or three rounds, every output identical to HotSpot 25's (ms, min of
/// the rounds; the whole table is in `docs/feature-designs/jit-ir-optimizations.md`):
///
/// | workload | `=0` | `=1` |
/// |---|---:|---:|
/// | `CratonBench bintrees` | 10 378 | **2 696** |
/// | `CratonBench hashmap` | 8 612 | **6 302** |
/// | `IrEscapeProbe` | 9 024 | **7 501** |
/// | `CratonBenchC2 dispatch` | 295 | **234** |
/// | `CratonBench stringregex` | 124 | 147 |
/// | the other CratonBench / C2 phases, `RefStoreLoopProbe` | = | = |
/// | `ZgcTlabStress 8 200` (8 threads, GC-heavy) | **13 265** | 21 367 |
/// | `PutScale 8 20000000` (8 threads, field stores) | **457** | 23 323 |
///
/// The first half is the 2026-09-02 measurement reversed: the per-allocation
/// helper cost that decided it has been cut since. The second half is new and
/// is why the default is still OFF. With the VM TLAB on, objects are laid out
/// COMPACT (`plan_tlab_object_shape_at`, and the JIT inline bump), and a
/// compiled store to a compact object that the emitter does not inline is a
/// helper whose layout lookup clones and drops the class's shared
/// `Arc<CompactLayout>` — two atomic RMWs on one cache line per store, from
/// every thread. On one thread that is 4x a legacy inline store (PutScale 1
/// thread: 259 ms legacy, 1 053 ms compact); on eight it is a 50x coherence
/// storm. The native profile of `ZgcTlabStress 8 200` with this on: 30 % of
/// busy samples in `class_layout_for_fields`' refcount, 19 % in
/// `ZgcRealHeap::set_field_no_card`. G1 and Generational show the same
/// collapse today (PutScale 8 threads: 44 s and 25 s) — it is not this arm's
/// defect, but this arm is what would bring it to the default collector.
///
/// `set_field_no_card`/`get_field` stopped cloning (`zgc_compact_field_storage`
/// in `zgc.rs`). The rest is outside `gc/`: `jit_compact_field_slot`
/// (`vm/src/jit/helpers.rs`) and `cratonvm_types::compact_object_field_storage`
/// must borrow through `with_class_layout` too (cross-lane requests in the
/// lane notes named above). **Flip [`ZGC_VM_TLAB_DEFAULT_ON`] once a build carrying both
/// shows `PutScale 8 20000000` under `=1` within 2x of `=0` and `ZgcTlabStress
/// 8 200` not slower** — the allocation-side win above is already in hand.
///
/// # Wave 5 (`zgc5`): the gate holds on the wave-4 binary
///
/// `cratonvm-jitr9-w4.exe`, interleaved, every output identical to HotSpot's.
/// `PutScale 8`: 837/763 ms on against 498/538 off (within 2x). bintrees:
/// 1 923/2 090 against 11 501/12 167. `ZgcTlabStress 8 200`, 10 rounds each:
/// min **9 349** on against **12 300** off, mean 15.2 s against 16.9 s. The
/// earlier "10-27 % slower" was two rounds of a bimodal workload. Its wall
/// clock is ruled by `relocate_stw`, a whole-live-set pass that runs after the
/// logged `pause_us`. With the VM TLAB on, threads stop being caught inside
/// `jit_new_object`, the coverage proof completes, and nearly every cycle
/// compacts. With `CRATONVM_ZGC_RELOCATE=0` in both arms, on beats off in
/// every round, 10.7-11.6 s against 12.5-13.3 s. Numbers and the flip edit:
/// `docs/feature-designs/jit-ir-optimizations.md`.
pub(crate) fn zgc_vm_tlab_enabled_by_default() -> bool {
    match cratonvm_types::flags::runtime_var_os("CRATONVM_ZGC_JIT_TLAB") {
        Some(raw) => {
            let v = raw.to_string_lossy().trim().to_ascii_lowercase();
            match v.as_str() {
                "1" | "on" | "true" | "yes" => true,
                "0" | "off" | "false" | "no" | "" => false,
                _ => ZGC_VM_TLAB_DEFAULT_ON,
            }
        }
        None => ZGC_VM_TLAB_DEFAULT_ON,
    }
}

/// What an unset (or unparseable) `CRATONVM_ZGC_JIT_TLAB` means: ON since round
/// 9 wave 5's integration (`=0` is the kill switch; the INVENTORY row carries
/// `off_word: Some("0")`). The gate held on the wave-5 binary
/// (`cratonvm-jitr9-w5.exe`, interleaved, every output identical to HotSpot's):
/// `ZgcTlabStress 8 200`, 6 rounds, on faster in 5 (mean ~11.8 s against
/// ~13.4 s off; the pause and sweep columns lower in every round);
/// `PutScale 8 20000000` 220/208 ms on against 214/301 off -- the compact-field
/// contention the wave-3 measurement found is gone -- and bintrees ~6x faster.
pub(crate) const ZGC_VM_TLAB_DEFAULT_ON: bool = true;

impl ZgcRealHeap {
    /// Whether [`Self::refill_tlab`] hands out chunks. Off by the kill switch,
    /// by `set_tlab_enabled(false)`, or on a heap with no `Arc` identity (no
    /// sink registered, so a retired tail would have nowhere to go).
    pub fn vm_tlab_enabled(&self) -> bool {
        self.counters.vm_tlab_enabled.load(Ordering::Relaxed)
            && self.tlab_enabled.load(Ordering::Relaxed)
            && self.self_weak.get().is_some()
    }

    /// Test and diagnostic override of the kill switch.
    pub fn set_vm_tlab_enabled(&self, on: bool) {
        self.counters.vm_tlab_enabled.store(on, Ordering::Relaxed);
    }

    /// The `Zgc` arm of `VmHeap::refill_tlab`: a zeroed chunk of at most
    /// `requested` bytes (never below the VM's minimum TLAB, never above the
    /// per-thread ceiling the heap's own buffers observe), or `None` when the
    /// arena cannot serve one -- the caller then allocates per object through
    /// `try_alloc_object_full`, which is the path every allocation took before.
    pub fn refill_tlab(&self, requested: usize) -> Option<(*mut u8, usize)> {
        self.refill_tlab_at_least(requested, 0)
    }

    /// [`Self::refill_tlab`] with a floor: never hands out a chunk smaller
    /// than `min_size` (the object whose miss asked for the refill, rounded
    /// up to the chunk grid). When the per-thread ceiling is below the floor
    /// it declines BEFORE carving, so no useless chunk is installed and
    /// retired around an object that cannot fit in it; the caller's `None`
    /// handling is the same as for any failed refill. `min_size == 0` is
    /// [`Self::refill_tlab`], byte for byte. Wave 9, lane gcI (the ZGC half of
    /// `gengc-r5w5-sizer9-refill-can-carve-less-than-the-object`; the
    /// Generational and G1 halves are `GenerationalHeap::refill_tlab_at_least`
    /// and `G1Collector::refill_tlab_at_least`).
    pub fn refill_tlab_at_least(
        &self,
        requested: usize,
        min_size: usize,
    ) -> Option<(*mut u8, usize)> {
        if !self.vm_tlab_enabled() {
            return None;
        }
        let capacity = self.arena_end.saturating_sub(self.arena_base);
        // Give back this thread's PREVIOUS chunk before sizing the next one.
        //
        // Ordinarily the tail sink has already done it (the VM retires its
        // buffer before asking for a refill), and this is a no-op. It is not
        // a no-op for a buffer that consumed its chunk to the last byte: there
        // is no tail, so `Tlab::retire` calls no sink, and without this the
        // thread would carry its own spent chunk in `reserved_bytes` forever
        // and shrink every later chunk it asks for. See `VM_TLAB_RESERVED`.
        release_vm_reservation(&self.tlabs);
        // Count this thread against the reservation share BEFORE the size is
        // read, so a thread never sizes its own chunk as though it were the
        // only claimant. One relaxed load of a thread-local plus, once per
        // collection per thread, one relaxed `fetch_add`. See
        // `zgc_vm_tlab_share_enabled` for the defect this closes and
        // `ZArenaTlabRegistry::vm_tlab_slots` for why the count is rebuilt each
        // cycle rather than decremented.
        if zgc_vm_tlab_share_enabled() {
            let id = self.tlabs.id();
            let per_heap = zgc_vm_tlab_record_per_heap_enabled();
            let _ = VM_TLAB_SLOTS_COUNTED.try_with(|records| {
                vm_tlab_slot_count(
                    &mut records.borrow_mut(),
                    &self.tlabs.lifetime,
                    id,
                    per_heap,
                    |already| self.tlabs.note_vm_tlab_slot(already),
                );
            });
        }
        let ceiling = self.tlabs.chunk_bytes_now(capacity);
        if ceiling == 0 {
            return None;
        }
        // gci i48/zl3 (`zx48-collector-scope-audit.md` A8,
        // `CRATONVM_TLAB_HOTSPOT_FLAGS` on ZGC): a FLOORED refill (the VM's
        // refill site, `min_size > 0`) already sized its request under the
        // VM's TLAB flags (`HeapRealm::tlab_sizing`), so a request below the
        // 8 KiB chunk floor is the flags speaking -- a typed `MinTLABSize`, or
        // the object alone under `-XX:-UseTLAB` -- and is carved as asked
        // (never below the caller's floor). Under `TlabSizing::DEFAULT` (the
        // switch at `=0`, or nothing typed) every such request is at least
        // 8 KiB, so the floor is the old one byte for byte; a plain
        // `refill_tlab` (`min_size == 0`) keeps it too.
        let chunk_floor = crate::tlab::min_tlab_size().min(ceiling);
        let floor = if min_size > 0 {
            let asked = requested.max(min_size).saturating_add(ZGC_TLAB_ALIGN - 1);
            chunk_floor.min(asked & !(ZGC_TLAB_ALIGN - 1))
        } else {
            chunk_floor
        };
        let mut want = requested.clamp(floor, ceiling) & !(ZGC_TLAB_ALIGN - 1);
        if want == 0 {
            return None;
        }
        // The caller's floor on the chunk grid (0 for a plain refill). A
        // floor above the ceiling cannot be served by any chunk: decline
        // without carving.
        let min_need = min_size.saturating_add(ZGC_TLAB_ALIGN - 1) & !(ZGC_TLAB_ALIGN - 1);
        if min_need > want {
            if min_need > ceiling {
                return None;
            }
            want = min_need;
        }
        // A recycled block shorter than `want` is accepted down to the VM's
        // minimum buffer: the VM retries the object that missed against the
        // new buffer and falls to the per-object path if it does not fit, so
        // a short chunk costs one miss, not a failure. Never below the
        // caller's floor, though: such a chunk cannot hold the object.
        let need = floor.max(min_need);
        let (ptr, size) = self.carve_tlab_chunk(want, need)?;
        // zr z8/st (`CRATONVM_ZGC_LAZY_TLAB_STARTS`): record the chunk for the
        // retire / published-tail walks and the oracle census.
        if self.counters.lazy_starts.on() {
            self.counters
                .lazy_starts
                .note_carve(ptr as usize, ptr as usize + size);
        }
        // The VM installs this chunk in the calling thread's buffer (the one
        // caller, `gc_and_alloc::tlab_alloc_shaped_inner`, refills
        // `thread.tlab` on the thread it runs on). Record it for
        // `note_tlab_object`'s owned-word path, or clear any older record when
        // the switch is off.
        set_owned_vm_tlab_chunk(
            &self.tlabs,
            if self.tlabs.owned_starts.load(Ordering::Relaxed) {
            // NOTE THE CHUNK'S SUMMARY BLOCKS BEFORE THE RECORD ARMS ANYTHING
            // (nio21 wave 9 L3). The record is what lets `Tlab::new` arm the
            // JIT's inline start-bit store, and that store writes the bitmap
            // words directly -- it cannot set `HeapBitmap::blocks`. A block
            // whose first start came from it kept a clear summary bit, which
            // `nearest_base_in` reads as "every word here is zero": the pin
            // probe and `nearest_base_at_or_below` then skipped live objects
            // in it. One summary load per 32 KiB of chunk, on this thread,
            // before the buffer exists. `CRATONVM_ZGC_CARVE_NOTES_BLOCKS=0`
            // restores the old (unsound) carve.
            if self.tlabs.carve_notes_blocks.load(Ordering::Relaxed) {
                self.registry
                    .note_blocks_covering(ptr as usize, ptr as usize + size);
            }
            OwnedVmTlabChunk {
                lo: ptr as usize,
                hi: ptr as usize + size,
                epoch: ownership_epoch().load(Ordering::Acquire),
            }
        } else {
            OwnedVmTlabChunk::NONE
            },
        );
        // `carve_tlab_chunk` charged `size` to
        // `reserved_bytes`. Record the extent so it can be given back: this
        // thread is the only party that will ever know it. Unconditional, and
        // not folded into the record above -- see `VM_TLAB_RESERVED`.
        //
        // If the thread-local is gone (teardown) the charge simply stays,
        // which is the documented, bounded, safe-direction leak: it makes
        // later chunks smaller, never larger.
        set_vm_tlab_charge(
            &self.tlabs,
            VmTlabCharge {
                id: self.tlabs.id(),
                lo: ptr as usize,
                hi: ptr as usize + size,
                charged_at: self.gc_count(),
            },
            zgc_vm_tlab_record_per_heap_enabled(),
        );
        let after = self.allocated.fetch_add(size, Ordering::Relaxed) + size;
        if after >= self.gc_threshold && after >= self.gc_rearm.load(Ordering::Relaxed) {
            self.native_alloc_pressure.store(true, Ordering::Relaxed);
        }
        self.counters
            .vm_tlab_refills
            .fetch_add(1, Ordering::Relaxed);
        self.counters
            .vm_tlab_refill_bytes
            .fetch_add(size, Ordering::Relaxed);
        Some((ptr, size))
    }

    /// zr z5/cr: bytes charged to `allocated` since the end of the last
    /// collection -- every `alloc_raw` object and every carved TLAB chunk IN
    /// FULL (a chunk is charged when carved, its unused tail given back at
    /// retire), against the watermark `collect_garbage` resets to the post-sweep
    /// live figure. So it over-reads what the mutators really laid out, never
    /// under-reads: zero means no object and no chunk was handed out since.
    /// Two relaxed loads; the refusal doors' futile test and census read it.
    pub fn bytes_allocated_since_cycle(&self) -> usize {
        self.allocated
            .load(Ordering::Relaxed)
            .saturating_sub(self.cycle_alloc_watermark.load(Ordering::Relaxed))
    }

    /// zr z5/cr: after exactly ONE collection since `alloc_raw` last refused a
    /// request, does the arena now hold a single span that serves it (a free
    /// block or the un-reserved bump tail, with 16 bytes of alignment slack)?
    /// `None` when there is no such record (no refusal, or another collection
    /// ran since): the caller then knows nothing and keeps its old ladder.
    /// Conservative by construction -- the small tier's bound is its floor and
    /// the large-object reserve is not counted -- so `Some(true)` means the
    /// refused request's retry finds room unless another thread takes it
    /// first. One arena lock; asked by a refusal door, never per allocation.
    pub fn room_for_last_refusal(&self) -> Option<bool> {
        let epoch = self.counters.last_refusal_epoch.load(Ordering::Relaxed);
        if epoch == usize::MAX || self.gc_count.load(Ordering::Relaxed) != epoch.wrapping_add(1) {
            return None;
        }
        let need = self
            .counters
            .last_refusal_size
            .load(Ordering::Relaxed)
            .saturating_add(16);
        let arena = self.arena.lock();
        // zt t1/sp: while the opt-in final-refusal rule
        // (`CRATONVM_ZGC_RETIRE_SUB_REQUEST_SPANS`) has set a threshold, no
        // block or bump tail below it serves the retry, so "room" means at
        // least that much (`0` when the rule is off: unchanged).
        let need = need.max(arena.min_free_span());
        Some(arena.low_bump_headroom() >= need || arena.largest_free_block() >= need)
    }

    /// An object the VM just wrote a complete header for at `ptr`, inside a
    /// chunk this heap handed out through [`Self::refill_tlab`]. `footprint`
    /// is the bytes the buffer's cursor advanced by.
    ///
    /// Registers the base (obligation 1 in the module doc), notes the young
    /// grain when generational, and blackens the object when a mark is
    /// running. Does NOT charge `allocated`: the chunk was charged at refill.
    ///
    /// # Per-object cost (round 9 wave 4, `gc4`)
    ///
    /// This runs once per object on every VM-TLAB allocation, compiled or
    /// interpreted (24 % of `CratonBench bintrees` with the VM TLAB on,
    /// wave-3 profile). What is left is the irreducible part: ONE `fetch_or`
    /// into the start registry, plus relaxed loads. The start bit itself cannot
    /// be batched per refill: the registry is a mutator-path oracle
    /// (`is_object_address`, the SATB barrier, the conservative scans), so a
    /// base must be in it before the object can be seen, and the object does
    /// not exist at refill.
    ///
    /// What moved to the refill, where it is sound:
    ///
    /// * **The vacated-address ledger** (`gc_quiescence::note_allocated`, an
    ///   out-of-line call per object even with the ledger off). Dropped here:
    ///   `VmHeap::refill_tlab` already purges the WHOLE chunk through
    ///   `note_allocated_range` before any object is bumped into it, and no
    ///   collection can re-add an address inside the chunk's unused tail — a
    ///   published tail's pages are withheld from relocation (obligation 4), so
    ///   nothing is ever vacated from, or slid into, the span this cursor has
    ///   yet to hand out. The per-object purge could only ever remove an entry
    ///   that the range purge had already removed.
    /// * **The young grain.** The carve marks the whole chunk young
    ///   (`carve_tlab_chunk`), so `note_young_page` now tests the bit with a
    ///   plain load and skips the RMW when it is set; it still sets it for an
    ///   object that lands in a chunk a whole-heap collection cleared the set
    ///   under (a tail published across the cycle).
    ///
    /// # The start bit without the lock (round 9 wave 5, `zgc5`)
    ///
    /// Behind `CRATONVM_ZGC_TLAB_OWNED_STARTS` (default ON, `=0` kills): when the object's
    /// bitmap word lies wholly inside the chunk this thread carved and still
    /// owns, the bit is set with a plain store. See [`OwnedVmTlabChunk`] for
    /// the ownership argument and `ZObjectStarts::insert_in_owned_chunk` for
    /// the word test. The chunk's two edge words keep the `fetch_or`.
    #[inline]
    pub fn note_tlab_object(&self, ptr: *mut u8, footprint: usize) {
        let addr = ptr as usize;
        if zgc_corpse_enabled() {
            self.audit_registry_insert(addr, footprint, "vm_tlab");
        }
        // A bitmap word wholly inside the chunk this thread owns takes a plain
        // store instead of the locked RMW (`CRATONVM_ZGC_TLAB_OWNED_STARTS`;
        // see `OwnedVmTlabChunk` for why that is sound). The record is empty
        // unless the switch was on at this thread's last refill.
        match owned_vm_tlab_chunk_containing(addr) {
            Some(owned) if addr >= owned.lo && addr < owned.hi => {
                self.registry
                    .insert_in_owned_chunk(addr, owned.lo, owned.hi);
            }
            _ => self.registry.insert(addr),
        }
        self.note_young_page(addr);
        self.allocate_black_if_marking(ptr);
    }

    /// The reserved tails of TLABs whose owners could not retire before this
    /// collection (blocked in native, or frozen in compiled code). Consumed
    /// by `relocate_stw`: see obligation 4 in the module doc. Replaces any
    /// previous list; spans outside the arena are dropped.
    ///
    /// nio21 wave 9 L3 (`CRATONVM_ZGC_SKIP_COVERS_INFLIGHT`, default ON): each
    /// kept region's START is lowered over the bytes no registered object
    /// covers below it -- see [`Self::inflight_cover_start`]. The end is
    /// unchanged, so `jit_tlab_skip_floor` (the cursor floor) is too.
    pub fn set_jit_tlab_skip_regions(&self, regions: &[(usize, usize)]) {
        // zr z8/st (`CRATONVM_ZGC_LAZY_TLAB_STARTS`): walk each published
        // tail's span below its cursor while the world is stopped, before the
        // cover reads the registry. Nothing when the switch is off.
        self.lazy_note_published_tails(regions);
        let cover = zgc_skip_covers_inflight_enabled();
        let mut kept = self.counters.jit_tlab_skip.lock();
        kept.clear();
        kept.extend(
            regions
                .iter()
                .copied()
                .filter(|(s, e)| e > s && *s >= self.arena_base && *e <= self.arena_end)
                .map(|(s, e)| {
                    if !cover {
                        return (s, e);
                    }
                    let lowered = self.inflight_cover_start(s);
                    if lowered < s {
                        // zr z1/al: the cover's engagement and cost counters.
                        self.counters
                            .inflight_covers_lowered
                            .fetch_add(1, Ordering::Relaxed);
                        self.counters
                            .inflight_cover_bytes
                            .fetch_add(s - lowered, Ordering::Relaxed);
                    }
                    (lowered, e)
                }),
        );
    }

    /// Where a published tail starting at `s` must really start for the
    /// sweep to withhold an object its frozen owner has bumped but not yet
    /// registered: the END of the highest registered object below `s` (or
    /// `s` itself when that object ends there or cannot be sized).
    ///
    /// # The hole (nio21 wave 9 L3, `r21-zgc-frozen-peer-in-flight-object-is-swept-CLOSED-20261012.md`)
    ///
    /// The JIT's inline allocation commits the bump cursor and THEN runs the
    /// inline start-bit store (`emit_zgc_inline_announce`: "entered after the
    /// bump has committed a fully headed object"). A peer OS-suspended between
    /// the two publishes the tail `[cursor, end)`, so the object just below
    /// `cursor` is outside the tail, is not in the registry, and is not
    /// rooted (the take-over probe resolves its address as the one-past-the-end
    /// cursor of the PREVIOUS object). The complement sweep then hands its
    /// bytes to the free list while the peer, on resume, registers and uses
    /// it. Every byte between the highest registered object's end and `s` is
    /// unregistered, so withholding it for this one cycle costs at most a
    /// chunk of reclaim and covers that object whatever its size.
    ///
    /// Bounded below by one VM-TLAB chunk (an in-flight object lies inside
    /// its owner's chunk) and the arena base. Runs with the world stopped and
    /// the registry stable.
    pub(crate) fn inflight_cover_start(&self, s: usize) -> usize {
        let reach = self.tlabs.config().initial_chunk;
        if reach == 0 || s <= self.arena_base {
            return s;
        }
        let floor = s.saturating_sub(reach).max(self.arena_base);
        match self.registry.nearest_base_within_total(s - 1, floor) {
            Some(b) => {
                // SAFETY: the registry holds only live allocation bases, and
                // the world is stopped while tails are published.
                let header = unsafe { &*(b as *const crate::heap::ObjectHeader) };
                match Self::alloc_size(header).and_then(|size| b.checked_add(size)) {
                    Some(end) if end <= s => end.max(floor),
                    // Unsizable, or an extent running into the tail (which a
                    // healthy registry never has): keep the published start.
                    _ => s,
                }
            }
            None => floor,
        }
    }

    /// Clear the list set by [`Self::set_jit_tlab_skip_regions`].
    pub fn clear_jit_tlab_skip_regions(&self) {
        self.counters.jit_tlab_skip.lock().clear();
    }

    /// The bytes of this collection's published tails, to be added to the
    /// live bytes it republishes as `allocated` (zr z1/al residual, under
    /// [`zgc_tail_return_cycle_check_enabled`]; 0 with the switch off).
    ///
    /// A published tail is a VM-TLAB reservation its owner could not retire
    /// before the pause: charged whole at refill, withheld from the sweep,
    /// and not in the live figure. Its owner resumes and bumps into it, so it
    /// is still allocated. The spans (merged, so overlapping publications
    /// count once) are recorded with the collection count this collection
    /// ends on, for [`Self::tail_was_recharged`]. Called once per collection,
    /// with the world stopped, BEFORE `gc_count` advances; every call
    /// replaces the previous record.
    pub(crate) fn recharge_published_tails(&self) -> usize {
        let mut spans = if self.tlabs.tail_return_cycle_check.load(Ordering::Relaxed) {
            self.jit_tlab_skip_regions()
        } else {
            Vec::new()
        };
        spans.sort_unstable();
        let mut merged: Vec<(usize, usize)> = Vec::with_capacity(spans.len());
        for (s, e) in spans {
            match merged.last_mut() {
                Some(last) if s <= last.1 => last.1 = last.1.max(e),
                _ => merged.push((s, e)),
            }
        }
        let bytes = merged.iter().map(|&(s, e)| e - s).sum();
        if bytes != 0 {
            self.counters
                .published_tail_recharge_bytes
                .fetch_add(bytes, Ordering::Relaxed);
        }
        *self.counters.recharged_tails.lock() = (self.gc_count() + 1, merged);
        bytes
    }

    /// Did the LAST collection re-charge a tail covering `[start, end)`
    /// ([`Self::recharge_published_tails`])? Then its retire gives the unused
    /// bytes back as a same-cycle retire does. Asked only for a tail charged
    /// before the last collection, which is rare: one lock.
    fn tail_was_recharged(&self, start: usize, end: usize) -> bool {
        let rec = self.counters.recharged_tails.lock();
        rec.0 == self.gc_count() && rec.1.iter().any(|&(s, e)| start >= s && end <= e)
    }

    /// Bytes [`Self::recharge_published_tails`] has added back to
    /// `allocated`, over the heap's life: the residual fix's engagement
    /// counter.
    pub fn published_tail_recharge_bytes(&self) -> usize {
        self.counters
            .published_tail_recharge_bytes
            .load(Ordering::Relaxed)
    }

    /// Snapshot of the published tails, for the compactor.
    pub(crate) fn jit_tlab_skip_regions(&self) -> Vec<(usize, usize)> {
        self.counters.jit_tlab_skip.lock().clone()
    }

    /// `(refills, refill_bytes, tails_returned, tail_bytes_returned)` for the
    /// VM thread's TLAB on this backend. The first is the engagement counter:
    /// zero on a run that allocated means the switch is off or the arm is
    /// unreachable, and no throughput claim about it can stand.
    pub fn vm_tlab_engagement(&self) -> (usize, usize, usize, usize) {
        (
            self.counters.vm_tlab_refills.load(Ordering::Relaxed),
            self.counters.vm_tlab_refill_bytes.load(Ordering::Relaxed),
            self.counters.vm_tlab_tails_returned.load(Ordering::Relaxed),
            self.counters
                .vm_tlab_tail_bytes_returned
                .load(Ordering::Relaxed),
        )
    }

    /// The bump-cursor floor the published tails impose on a slide: the end
    /// of the highest published tail inside `[base, low_end)`, or `base`.
    pub(crate) fn jit_tlab_skip_floor(&self, base: usize, low_end: usize) -> usize {
        self.counters
            .jit_tlab_skip
            .lock()
            .iter()
            .filter(|(s, _)| *s >= base && *s < low_end)
            .map(|(_, e)| (*e).min(low_end))
            .max()
            .unwrap_or(base)
    }

    /// Publish this heap's object-start bitmap into
    /// [`crate::tlab::JIT_ZGC_ANNOUNCE`], arming the JIT's inline start-bit
    /// store for VM-TLAB chunks this heap carves (round 9 wave 8, `zgc8`).
    /// `true` when this heap now owns the table.
    ///
    /// Declines -- and the helper keeps doing everything, exactly as before --
    /// when the switch is off, when the registry is the `Hash` arm, when owned
    /// starts are off (no owned-chunk record, so no buffer could be armed
    /// anyway), when a forensic mode the helper serves is on
    /// (`CRATONVM_DBG_ZGC_CORPSE`'s registry audit, the `a2dbg` ring), or when
    /// another live heap already owns the table.
    pub(crate) fn publish_jit_inline_announce(&self) -> bool {
        if !zgc_jit_inline_announce_enabled()
            || !self.tlabs.owned_starts.load(Ordering::Relaxed)
            || zgc_corpse_enabled()
            || crate::a2dbg::enabled()
            || self.arena_base == 0
        {
            return false;
        }
        let Some((words, base, span)) = self.registry.jit_bitmap_geometry() else {
            return false;
        };
        // zr z11/ia: an 8-aligned `base`, which the JIT's
        // `CRATONVM_JIT_ZGC_ANNOUNCE_SKIP_GRID_TEST` relies on (its objects
        // are 8-aligned bumps). An unaligned base could never pass the grid
        // test anyway, so declining here changes no answer: every store would
        // have gone to the helper.
        if words == 0 || span == 0 || base & 7 != 0 {
            return false;
        }
        let table = &crate::tlab::JIT_ZGC_ANNOUNCE;
        if table
            .owner
            .compare_exchange(0, self.arena_base, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return false;
        }
        table.base.store(base, Ordering::Release);
        table.span.store(span, Ordering::Release);
        let blocked = self.jit_inline_announce_blocked_now();
        table.blocked.store(usize::from(blocked), Ordering::Release);
        // zr z9/bo: the short store's geometry, and its gate (closed on a
        // base that is not 512-aligned). Both before `words`, so no buffer is
        // armed before they hold.
        let (bias, short_ok) = crate::tlab::JitZgcAnnounceTable::short_geometry(words, base);
        table.granule_bias.store(bias, Ordering::Release);
        table
            .short_blocked
            .store(usize::from(blocked || !short_ok), Ordering::Release);
        // LAST: a non-zero `words` is what `Tlab::new` and the JIT test first.
        table.words.store(words, Ordering::Release);
        true
    }

    /// Withdraw the table if this heap owns it (`Drop`). `words` goes FIRST,
    /// so a reader that still sees it non-zero sees this heap's geometry.
    pub(crate) fn withdraw_jit_inline_announce(&self) {
        let table = &crate::tlab::JIT_ZGC_ANNOUNCE;
        if self.arena_base == 0 || table.owner.load(Ordering::Acquire) != self.arena_base {
            return;
        }
        // zr z9/bo: the short store has no `words` test of its own, so its
        // gate closes first.
        table.short_blocked.store(1, Ordering::Release);
        table.words.store(0, Ordering::Release);
        table.span.store(0, Ordering::Release);
        table.base.store(0, Ordering::Release);
        table.blocked.store(0, Ordering::Release);
        table.granule_bias.store(0, Ordering::Release);
        table.owner.store(0, Ordering::Release);
    }

    /// What the table's `blocked` word must say right now: the inline store
    /// does neither allocate-black nor `note_young_page`, so it is closed
    /// while either could be needed. (zr z2/g2: a generational heap needs no
    /// per-object `note_young_page` under the inline store's epoch test, so
    /// it closes the store only under `CRATONVM_ZGC_GEN_INLINE_ANNOUNCE=0` --
    /// see `zgc_gen_inline_announce_enabled`.)
    fn jit_inline_announce_blocked_now(&self) -> bool {
        self.mark_active.load(Ordering::Relaxed) || self.gen_blocks_inline_announce()
    }

    /// Set the table's `blocked` word, if this heap owns the table. Callers
    /// raise it BEFORE the state that needs the helper becomes true and lower
    /// it only AFTER that state is false again (`set_mark_active`,
    /// `set_generational_enabled`).
    pub(crate) fn set_jit_inline_announce_blocked(&self, blocked: bool) {
        let table = &crate::tlab::JIT_ZGC_ANNOUNCE;
        if self.arena_base != 0 && table.owner.load(Ordering::Acquire) == self.arena_base {
            // zr z9/bo: the short store's gate follows `blocked`, and stays
            // closed on a geometry it cannot serve. Raised before `blocked`
            // and lowered after it, so it is never open while `blocked` is.
            let (_, short_ok) = crate::tlab::JitZgcAnnounceTable::short_geometry(
                table.words.load(Ordering::Acquire),
                table.base.load(Ordering::Acquire),
            );
            if blocked {
                table.short_blocked.store(1, Ordering::Release);
                table.blocked.store(1, Ordering::Release);
            } else {
                table.blocked.store(0, Ordering::Release);
                table
                    .short_blocked
                    .store(usize::from(!short_ok), Ordering::Release);
            }
        }
    }

    /// zr z9/bo: whether the SHORT inline start-bit store
    /// (`CRATONVM_ZGC_JIT_SHORT_ANNOUNCE`) is open for this heap right now:
    /// `None` when this heap does not own the published table, else
    /// `Some(short_blocked == 0)`. `Some(false)` outside a mark on a
    /// non-generational heap means the arena base is not 512-aligned, and
    /// every short site then takes the announce helper.
    pub fn jit_short_announce_open(&self) -> Option<bool> {
        let table = &crate::tlab::JIT_ZGC_ANNOUNCE;
        (self.arena_base != 0
            && table.owner.load(Ordering::Acquire) == self.arena_base
            && table.words.load(Ordering::Acquire) != 0)
            .then(|| table.short_blocked.load(Ordering::Acquire) == 0)
    }
}

/// `CRATONVM_ZGC_TLAB_TAIL_SINK`: take retired VM TLAB tails back into the
/// arena free list. `0`/`off`/`false`/`no` declines every tail, which leaves
/// the VM's filler object in place -- an A/B lever for the reclamation, at
/// the cost of the tail bytes until the next slide.
pub(crate) fn zgc_vm_tlab_tail_sink_enabled() -> bool {
    static CACHED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *CACHED.get_or_init(|| {
        match cratonvm_types::flags::runtime_var_os("CRATONVM_ZGC_TLAB_TAIL_SINK") {
            Some(raw) => {
                let v = raw.to_string_lossy().trim().to_ascii_lowercase();
                !matches!(v.as_str(), "0" | "off" | "false" | "no")
            }
            None => true,
        }
    })
}

impl crate::tlab::TlabTailSink for ZgcRealHeap {
    /// zr z8/st: the consumed span of a retiring VM buffer
    /// (`CRATONVM_ZGC_LAZY_TLAB_STARTS`); spans this heap did not carve, and
    /// every span with the switch off, are ignored.
    fn note_retired_span(&self, start: usize, cursor: usize, end: usize) {
        self.lazy_note_retired_span(start, cursor, end);
    }

    fn reclaim_tlab_tail(&self, start: usize, end: usize) -> bool {
        if end <= start || start < self.arena_base || end > self.arena_end {
            return false;
        }
        // THE RESERVATION COMES BACK EVEN WHEN THE TAIL DOES NOT.
        //
        // Deliberately above the `zgc_vm_tlab_tail_sink_enabled` screen and
        // above the alignment screen. Those two decide whether the *bytes*
        // rejoin the arena's free list; this decides whether the *chunk* is
        // still a chunk, and it is not — the buffer is retiring either way.
        // Keeping the charge when the sink is switched off would make
        // `CRATONVM_ZGC_TLAB_TAIL_SINK=0` ratchet `reserved_bytes` by a chunk
        // per refill, which would silently starve the byte-budget sizing on
        // the one arm that exists to A/B the tail reclamation.
        //
        // Released only when THIS thread's record covers the span, so a tail
        // belonging to some other buffer — including one of this module's own
        // `ZArenaTlab` cells, which release through `tlab_retire_locked`
        // instead — cannot double-release. `release_vm_reservation` clears the
        // record as it takes it, so a second call for the same chunk finds
        // nothing.
        let record = vm_tlab_charge(self.tlabs.id());
        let covers = record.is_some_and(|e| e.hi > e.lo && start >= e.lo && end <= e.hi);
        // Was this chunk charged before the last collection? Asked BEFORE the
        // release below clears the record, and only of the carver's own
        // record (`covers`).
        // ...unless that collection re-charged the tail it withheld
        // (`recharge_published_tails`): then it is back in `allocated`.
        let charged_before_last_gc = covers
            && self.tlabs.tail_return_cycle_check.load(Ordering::Relaxed)
            && record.is_some_and(|e| e.charged_at != self.gc_count())
            && !self.tail_was_recharged(start, end);
        if covers {
            release_vm_reservation(&self.tlabs);
        }
        if !zgc_vm_tlab_tail_sink_enabled() {
            return false;
        }
        if (start | end) & (ZGC_TLAB_ALIGN - 1) != 0 {
            return false;
        }
        let bytes = end - start;
        // The span is about to become free: no thread may keep an owned-chunk
        // record that covers it (`OwnedVmTlabChunk`). When the retiring thread
        // holds a VALID record of this chunk, only that record can cover the
        // span, so clearing it is enough. Anyone else retiring it -- a buffer
        // that moved threads, a cross-thread retire -- invalidates every
        // record. Both happen before `add_free_block` below. A covering record
        // is cleared even when stale: clearing is always safe.
        let own_record = OWNED_VM_TLAB_CHUNKS
            .try_with(|records| {
                let mut records = records.borrow_mut();
                let index = records.iter().position(|(life, id, owned)| {
                    life.strong_count() != 0
                        && *id == self.tlabs.id()
                        && owned.hi > owned.lo
                        && start >= owned.lo
                        && end <= owned.hi
                });
                index.is_some_and(|i| {
                    let (_, _, owned) = records.remove(i);
                    owned.epoch == ownership_epoch().load(Ordering::Acquire)
                })
            })
            .unwrap_or(false);
        if !own_record {
            invalidate_owned_vm_tlab_chunks();
        }
        {
            let mut arena = self.arena.lock();
            let base = arena.base_ptr() as usize;
            arena.add_free_block(start - base, bytes);
        }
        // zt t27/pc2: the free list gained a block (`ZCarveFutileLatch`).
        self.note_carve_space_gained();
        // The chunk was charged whole at refill; the part that was never
        // handed out goes back -- unless a collection ran since the charge:
        // it republished `allocated` as live bytes, which never held this
        // tail (`zgc_tail_return_cycle_check_enabled`). Saturating for the
        // retires that cannot tell (not the carver, or the check off).
        if !charged_before_last_gc {
            let _ = self
                .allocated
                .try_update(Ordering::Relaxed, Ordering::Relaxed, |a| {
                    Some(a.saturating_sub(bytes))
                });
        }
        self.counters
            .vm_tlab_tails_returned
            .fetch_add(1, Ordering::Relaxed);
        self.counters
            .vm_tlab_tail_bytes_returned
            .fetch_add(bytes, Ordering::Relaxed);
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::collector::{GarbageCollector, MonitorCleanup, StopTheWorldToken};
    use crate::heap::{HEADER_SIZE, SLOT_SIZE};
    use cratonvm_types::{ObjectRef, Value};

    struct NoMonitors;
    impl MonitorCleanup for NoMonitors {
        fn remap_after_gc(&self, _: &cratonvm_types::PointerMap) {}
    }

    /// zs s2/nd2 (zr4ab item 4): the share count's row is per heap -- a thread
    /// alternating heaps A, B, A finds A's epoch again (no re-count); the
    /// single-slot kill arm is the positive control (A's epoch is lost to B);
    /// a dropped heap's row is pruned.
    #[test]
    fn the_share_count_row_is_kept_per_heap() {
        let (a, b) = (std::sync::Arc::new(11u64), std::sync::Arc::new(22u64));
        let mut rows = Vec::new();
        // A counts at epoch 7, then B at epoch 3.
        let mut seen = u64::MAX;
        vm_tlab_slot_count(&mut rows, &a, 11, true, |e| {
            seen = e;
            7
        });
        assert_eq!(seen, 0);
        vm_tlab_slot_count(&mut rows, &b, 22, true, |e| {
            seen = e;
            3
        });
        assert_eq!(seen, 0, "B takes its own row, A's survives");
        vm_tlab_slot_count(&mut rows, &a, 11, true, |e| {
            seen = e;
            e
        });
        assert_eq!(seen, 7);
        vm_tlab_slot_count(&mut rows, &b, 22, true, |e| {
            seen = e;
            e
        });
        assert_eq!(seen, 3);
        // Kill arm: one slot, B's count dropped A's row.
        vm_tlab_slot_count(&mut rows, &b, 22, false, |e| e);
        vm_tlab_slot_count(&mut rows, &a, 11, false, |e| {
            seen = e;
            e
        });
        assert_eq!(seen, 0);
        // A dropped heap's row is pruned.
        drop(b);
        vm_tlab_slot_count(&mut rows, &a, 11, true, |e| e);
        assert_eq!(rows.len(), 1);
    }

    /// Lay an object out in a VM-side `Tlab` the way `tlab_alloc_shaped_inner`
    /// does: bump, write a legacy header, then `note_tlab_object`.
    fn tlab_new_object(
        heap: &ZgcRealHeap,
        tlab: &mut crate::tlab::Tlab,
        class_id: ClassId,
        num_fields: usize,
    ) -> Option<ObjectRef> {
        use crate::heap::{ArrayElementType, ObjectHeader, ObjectKind};
        let total = HEADER_SIZE + num_fields * SLOT_SIZE;
        let ptr = tlab.alloc_initialized(total, 8, |ptr| {
            let header = ObjectHeader::new(
                class_id,
                ObjectKind::Object,
                ArrayElementType::Reference,
                0,
                num_fields as u32,
            );
            unsafe { std::ptr::write(ptr as *mut ObjectHeader, header) };
            heap.note_tlab_object(ptr, total);
        })?;
        Some(unsafe { ObjectRef::from_raw(ptr) })
    }

    /// zr z4/ab (`common-watched-referent-set-is-per-thread-not-per-heap-20261012.md` §1): one thread
    /// refilling from heap A, then heap B, then A again gives A's first chunk
    /// back, and retiring A's last chunk returns A's `reserved_bytes` to where
    /// it started. Before the per-heap record B's refill overwrote A's, so
    /// A's second refill released nothing and A kept the first chunk charged
    /// for good. The kill-switch arm (`per_heap = false`, the single slot) is
    /// the positive control: it must leak exactly one chunk.
    #[test]
    fn one_thread_refilling_from_two_heaps_gives_each_its_reservation_back() {
        fn run(per_heap: bool) -> (usize, usize) {
            let a = ZgcRealHeap::new_shared(16 * 1024 * 1024);
            let b = ZgcRealHeap::new_shared(16 * 1024 * 1024);
            a.set_vm_tlab_enabled(true);
            b.set_vm_tlab_enabled(true);
            let a_before = a.tlabs.reserved_bytes.load(Ordering::Relaxed);
            let refill = |h: &ZgcRealHeap| -> (*mut u8, usize) {
                // The production refill, with the record written under the arm
                // being tested (the cached switch cannot be flipped per test).
                let (ptr, size) = h.refill_tlab(64 * 1024).expect("chunk");
                take_vm_tlab_charge(h.tlabs.id());
                set_vm_tlab_charge(
                    &h.tlabs,
                    VmTlabCharge {
                        id: h.tlabs.id(),
                        lo: ptr as usize,
                        hi: ptr as usize + size,
                        charged_at: h.gc_count(),
                    },
                    per_heap,
                );
                (ptr, size)
            };
            let _a1 = refill(&a);
            let (b_ptr, b_size) = refill(&b);
            let (a_ptr, a_size) = refill(&a);
            let a_mid = a.tlabs.reserved_bytes.load(Ordering::Relaxed);
            let mut ta = unsafe { crate::tlab::Tlab::new(a_ptr, a_size) };
            ta.retire();
            let mut tb = unsafe { crate::tlab::Tlab::new(b_ptr, b_size) };
            tb.retire();
            let a_after = a.tlabs.reserved_bytes.load(Ordering::Relaxed);
            assert!(a_mid >= a_size);
            (a_after - a_before, a_size)
        }
        let (leak_on, _) = run(true);
        assert_eq!(
            leak_on, 0,
            "per-heap record: A's reservation comes back whole"
        );
        let (leak_off, a_chunk) = run(false);
        assert!(
            leak_off > 0 && leak_off <= a_chunk * 2,
            "single slot (kill switch): B's refill overwrote A's record, so A leaks a chunk \
             (leaked {leak_off})"
        );
    }

    #[test]
    fn a_shared_heap_hands_the_vm_a_zeroed_chunk_inside_its_arena() {
        let heap = ZgcRealHeap::new_shared(16 * 1024 * 1024);
        heap.set_vm_tlab_enabled(true);
        let (ptr, size) = heap
            .refill_tlab(64 * 1024)
            .expect("a fresh 16 MiB heap must serve a 64 KiB chunk");
        let (lo, hi) = heap.conservative_addr_span().expect("arena span");
        let start = ptr as usize;
        assert!(start >= lo && start + size <= hi, "chunk outside the arena");
        assert!(size >= crate::tlab::min_tlab_size() && size <= 64 * 1024);
        assert_eq!(size % ZGC_TLAB_ALIGN, 0);
        let bytes = unsafe { std::slice::from_raw_parts(ptr, size) };
        assert!(bytes.iter().all(|b| *b == 0), "chunk must be zeroed");
        let (refills, refill_bytes, _, _) = heap.vm_tlab_engagement();
        assert_eq!((refills, refill_bytes), (1, size));
    }

    /// Wave 9, lane gcI (sizer9's ZGC half): a floored refill hands out at
    /// least the floor even when the request is smaller, and a floor above
    /// the heap's chunk ceiling is declined without carving (no refill
    /// booked). A 16 MiB arena's ceiling is 16 KiB
    /// (`chunk_bytes_for_capacity`: capacity / 1024, at least
    /// `min_tlab_size()` = 8 KiB), so the served floor sits between the
    /// 8 KiB request and that ceiling, and a 20 KiB floor (an object no chunk
    /// of this heap could hold) is a decline case.
    #[test]
    fn gc21i_a_floored_refill_never_carves_less_than_the_floor() {
        let heap = ZgcRealHeap::new_shared(16 * 1024 * 1024);
        heap.set_vm_tlab_enabled(true);
        let floor = 12 * 1024 + 8;
        let (_, size) = heap
            .refill_tlab_at_least(8 * 1024, floor)
            .expect("a fresh 16 MiB heap must serve a floor below its 16 KiB chunk ceiling");
        assert!(size >= floor, "chunk {size} below the floor {floor}");
        assert_eq!(size % ZGC_TLAB_ALIGN, 0);
        let (refills, _, _, _) = heap.vm_tlab_engagement();
        assert_eq!(refills, 1);
        for above in [20 * 1024 + 8, 64 * 1024 * 1024] {
            assert!(
                heap.refill_tlab_at_least(8 * 1024, above).is_none(),
                "a floor of {above} above the chunk ceiling must decline"
            );
        }
        let (refills, _, _, _) = heap.vm_tlab_engagement();
        assert_eq!(refills, 1, "the declined refills carved nothing");
    }

    /// zt t1/sp (`zgc-tlab-refill-ignores-the-object-floor-FIXED-20261019.md`, its
    /// "How to verify"): the RECYCLED-block arm of the carve honours the
    /// floor too. A spent arena whose only free block is 12 KiB answers a
    /// floor no chunk of this heap can hold (20 KiB over a 16 KiB ceiling)
    /// with `None` and books nothing, and a 4 KiB floor with that block.
    #[test]
    fn zt1sp_a_floored_refill_takes_a_recycled_block_only_above_the_floor() {
        let heap = ZgcRealHeap::new_shared(16 * 1024 * 1024);
        heap.set_vm_tlab_enabled(true);
        let (base, hole) = {
            let mut arena = heap.arena.lock();
            let base = arena.base_ptr() as usize;
            let room = arena.capacity() - arena.used();
            let p = arena.alloc(room, 8).expect("the whole arena") as usize - base;
            arena.add_free_block(p + 4096, 12 * 1024);
            (base, p + 4096)
        };
        assert!(heap.refill_tlab_at_least(32 * 1024, 20 * 1024).is_none());
        let (refills, _, _, _) = heap.vm_tlab_engagement();
        assert_eq!(refills, 0, "the declined refill carved nothing");
        let (ptr, size) = heap
            .refill_tlab_at_least(32 * 1024, 4 * 1024)
            .expect("the 12 KiB block");
        assert_eq!((ptr as usize - base, size), (hole, 12 * 1024));
    }

    /// gci i48/zl3 (`zx48-collector-scope-audit.md` A8): a floored refill
    /// below the 8 KiB chunk floor -- what the VM's refill site asks under a
    /// typed `MinTLABSize` or `-XX:-UseTLAB` -- is carved as asked, never
    /// below the object; a plain refill and an 8 KiB request keep the floor.
    #[test]
    fn gci_i48_zl3_a_floored_refill_below_the_chunk_floor_is_carved_as_asked() {
        let heap = ZgcRealHeap::new_shared(16 * 1024 * 1024);
        heap.set_vm_tlab_enabled(true);
        // `-XX:MinTLABSize=4k`: the request is 4 KiB, the object 64 bytes.
        let (_, size) = heap.refill_tlab_at_least(4096, 64).expect("a 4 KiB chunk");
        assert_eq!(size, 4096, "a typed MinTLABSize below 8 KiB is honoured");
        // `-XX:-UseTLAB`: the request IS the object (40 bytes).
        let (_, size) = heap.refill_tlab_at_least(40, 40).expect("a 40 B chunk");
        assert_eq!(size, 40, "no buffer beyond the object");
        // Unfloored: the chunk floor stands, as before.
        let (_, size) = heap.refill_tlab(40).expect("a plain refill");
        assert_eq!(size, crate::tlab::min_tlab_size(), "plain refill keeps 8 KiB");
        // The default sizing never asks below 8 KiB: unchanged.
        let (_, size) = heap
            .refill_tlab_at_least(crate::tlab::min_tlab_size(), 64)
            .expect("an 8 KiB chunk");
        assert_eq!(size, crate::tlab::min_tlab_size());
    }

    #[test]
    fn the_switch_and_a_heap_without_an_arc_identity_both_refuse() {
        let plain = ZgcRealHeap::with_capacity(16 * 1024 * 1024);
        plain.set_vm_tlab_enabled(true);
        assert!(
            plain.refill_tlab(64 * 1024).is_none(),
            "no sink can be registered for a heap that is not shared, so no chunk"
        );
        let shared = ZgcRealHeap::new_shared(16 * 1024 * 1024);
        shared.set_vm_tlab_enabled(false);
        assert!(shared.refill_tlab(64 * 1024).is_none());
        shared.set_vm_tlab_enabled(true);
        assert!(shared.refill_tlab(64 * 1024).is_some());
    }

    /// Unset means [`ZGC_VM_TLAB_DEFAULT_ON`], and both spellings override it
    /// either way — so flipping that one constant is the whole default change
    /// and `=0` stays a kill switch afterwards. See
    /// [`zgc_vm_tlab_enabled_by_default`] for the numbers.
    #[test]
    fn the_vm_tlab_is_opt_in_and_the_switch_reads_both_ways() {
        cratonvm_types::flags::with_thread_overrides(&[("CRATONVM_ZGC_JIT_TLAB", None)], || {
            assert_eq!(
                zgc_vm_tlab_enabled_by_default(),
                ZGC_VM_TLAB_DEFAULT_ON,
                "unset must mean the documented default"
            )
        });
        cratonvm_types::flags::with_thread_overrides(
            &[("CRATONVM_ZGC_JIT_TLAB", Some("maybe"))],
            || assert_eq!(zgc_vm_tlab_enabled_by_default(), ZGC_VM_TLAB_DEFAULT_ON),
        );
        for on in ["1", "on", "true", "yes"] {
            cratonvm_types::flags::with_thread_overrides(
                &[("CRATONVM_ZGC_JIT_TLAB", Some(on))],
                || assert!(zgc_vm_tlab_enabled_by_default(), "`{on}` must enable it"),
            );
        }
        for off in ["0", "off", "false", "no", ""] {
            cratonvm_types::flags::with_thread_overrides(
                &[("CRATONVM_ZGC_JIT_TLAB", Some(off))],
                || assert!(!zgc_vm_tlab_enabled_by_default(), "`{off}` must not"),
            );
        }
    }

    /// The seam that keeps the JIT's inline allocator announcing what it
    /// allocates. Without it every inline allocation is absent from the start
    /// registry, and `is_object_address` — a mutator-path oracle here, not
    /// just the sweep's — answers `None` for a perfectly live object.
    #[test]
    fn a_shared_heap_tells_the_jit_that_tlab_objects_must_be_announced() {
        cratonvm_types::set_jit_tlab_registration_required(false);
        let heap = cratonvm_types::flags::with_thread_overrides(
            &[("CRATONVM_ZGC_JIT_TLAB", Some("1"))],
            || ZgcRealHeap::new_shared(16 * 1024 * 1024),
        );
        assert!(
            cratonvm_types::jit_tlab_registration_required(),
            "a ZGC heap that hands out VM TLABs must require the announcing call"
        );
        drop(heap);
        cratonvm_types::set_jit_tlab_registration_required(false);
    }

    #[test]
    fn objects_laid_out_in_the_vm_tlab_are_registered_and_collected_like_any_other() {
        let heap = ZgcRealHeap::new_shared(16 * 1024 * 1024);
        heap.set_vm_tlab_enabled(true);
        let (ptr, size) = heap.refill_tlab(64 * 1024).expect("chunk");
        let mut tlab = unsafe { crate::tlab::Tlab::new(ptr, size) };
        let kept = tlab_new_object(&heap, &mut tlab, ClassId::new(7), 2).expect("fits");
        let dropped = tlab_new_object(&heap, &mut tlab, ClassId::new(7), 2).expect("fits");
        heap.set_field(kept, 0, Value::Int(41));
        assert!(heap.is_object_address(kept.as_ptr() as usize).is_some());
        assert!(heap.is_object_address(dropped.as_ptr() as usize).is_some());
        let dropped_addr = dropped.as_ptr() as usize;
        tlab.retire();
        let stw = unsafe { StopTheWorldToken::new_unchecked() };
        let mut roots = vec![kept];
        cratonvm_types::flags::with_thread_overrides(
            &[("CRATONVM_ZGC_RELOCATE", Some("0"))],
            || {
                heap.collect_garbage(&stw, &mut roots, &NoMonitors);
            },
        );
        assert_eq!(heap.get_field(roots[0], 0), Value::Int(41));
        assert!(
            heap.is_object_address(dropped_addr).is_none(),
            "an unrooted TLAB object must be swept"
        );
    }

    #[test]
    fn retiring_the_vm_tlab_returns_its_tail_to_the_arena() {
        let heap = ZgcRealHeap::new_shared(16 * 1024 * 1024);
        heap.set_vm_tlab_enabled(true);
        let (ptr, size) = heap.refill_tlab(64 * 1024).expect("chunk");
        let mut tlab = unsafe { crate::tlab::Tlab::new(ptr, size) };
        let _ = tlab_new_object(&heap, &mut tlab, ClassId::new(7), 4).expect("fits");
        let consumed = tlab.consumed_bytes();
        let free_before = heap.arena.lock().free_list_bytes();
        let allocated_before = heap.allocated.load(Ordering::Relaxed);
        tlab.retire();
        let tail = size - consumed;
        let free_after = heap.arena.lock().free_list_bytes();
        assert_eq!(
            free_after - free_before,
            tail,
            "the tail must be free-listed once"
        );
        assert_eq!(
            allocated_before - heap.allocated.load(Ordering::Relaxed),
            tail,
            "the tail's reservation must be credited back"
        );
        let (_, _, tails, tail_bytes) = heap.vm_tlab_engagement();
        assert_eq!((tails, tail_bytes), (1, tail));
        assert!(tlab.is_retired());
    }

    #[test]
    fn a_tail_outside_this_arena_is_declined_and_takes_the_filler_path() {
        let _heap = ZgcRealHeap::new_shared(16 * 1024 * 1024);
        let mut backing = vec![0u64; 1024];
        let ptr = backing.as_mut_ptr() as *mut u8;
        let mut tlab = unsafe { crate::tlab::Tlab::new(ptr, 8192) };
        assert!(tlab.alloc(64, 8).is_some());
        tlab.retire();
        let header = unsafe { &*(ptr.add(64) as *const crate::heap::ObjectHeader) };
        assert_eq!(
            header.class_id,
            crate::tlab::TLAB_FILLER_CLASS_ID,
            "a foreign tail must still get its filler"
        );
    }

    /// `CRATONVM_ZGC_TLAB_OWNED_STARTS=1`: the refill records the chunk for
    /// this thread, objects noted into it are registered exactly as the
    /// `fetch_or` path registers them (and collected like any other), and
    /// retiring the chunk on the recording thread clears the record. With the
    /// switch off the refill leaves no record. Asserts the record's range,
    /// never its validity: the epoch is process-wide and any concurrent
    /// test's collection or heap construction bumps it.
    #[test]
    fn the_owned_chunk_record_follows_refill_and_retire() {
        let heap = ZgcRealHeap::new_shared(16 * 1024 * 1024);
        heap.set_vm_tlab_enabled(true);
        // zt t2/al (X1): the switch is seeded on the registry at construction,
        // so a live heap is flipped through the seed, not the environment.
        heap.tlabs.owned_starts.store(true, Ordering::Relaxed);
        let (ptr, size) = heap.refill_tlab(64 * 1024).expect("chunk");
        let lo = ptr as usize;
        assert_eq!(owned_vm_tlab_chunk_raw(heap.tlabs.id()), (lo, lo + size));
        let mut tlab = unsafe { crate::tlab::Tlab::new(ptr, size) };
        // 64 objects of 40 bytes: 2 560 bytes, so several whole bitmap words
        // (the plain-store path) and the chunk's first word (`fetch_or`).
        let objs: Vec<ObjectRef> = (0..64)
            .map(|_| tlab_new_object(&heap, &mut tlab, ClassId::new(7), 2).expect("fits"))
            .collect();
        for o in &objs {
            let a = o.as_ptr() as usize;
            assert!(
                heap.is_object_address(a).is_some(),
                "{a:#x} must be registered"
            );
            assert!(
                !heap.registry.contains(a + 8),
                "{:#x}: an interior address must not be a base",
                a + 8
            );
        }
        heap.set_field(objs[5], 0, Value::Int(55));
        let dropped = objs[40].as_ptr() as usize;
        tlab.retire();
        assert_eq!(
            owned_vm_tlab_chunk_raw(heap.tlabs.id()),
            (0, 0),
            "retiring the recorded chunk on its own thread clears the record"
        );
        let stw = unsafe { StopTheWorldToken::new_unchecked() };
        let mut roots = vec![objs[5]];
        cratonvm_types::flags::with_thread_overrides(
            &[("CRATONVM_ZGC_RELOCATE", Some("0"))],
            || {
                heap.collect_garbage(&stw, &mut roots, &NoMonitors);
            },
        );
        assert_eq!(heap.get_field(roots[0], 0), Value::Int(55));
        assert!(heap.is_object_address(dropped).is_none(), "unrooted: swept");

        heap.tlabs.owned_starts.store(false, Ordering::Relaxed);
        let _ = heap.refill_tlab(64 * 1024).expect("second chunk");
        assert_eq!(
            owned_vm_tlab_chunk_raw(heap.tlabs.id()),
            (0, 0),
            "switch off: no record"
        );
    }

    /// zt t2/al (X1): the three refill switches are read once, when the heap
    /// is built, and a refill outside that scope follows the seed: a heap
    /// built under `CRATONVM_ZGC_TLAB_OWNED_STARTS=0` /
    /// `CRATONVM_ZGC_CARVE_NOTES_BLOCKS=0` / `CRATONVM_ZGC_TAIL_RETURN_CYCLE_CHECK=0`
    /// keeps them off, and the default heap keeps them on (the control).
    #[test]
    fn zt2al_the_refill_switches_are_seeded_once_per_heap() {
        let kill = [
            ("CRATONVM_ZGC_TLAB_OWNED_STARTS", Some("0")),
            ("CRATONVM_ZGC_CARVE_NOTES_BLOCKS", Some("0")),
            ("CRATONVM_ZGC_TAIL_RETURN_CYCLE_CHECK", Some("0")),
        ];
        let off = cratonvm_types::flags::with_thread_overrides(&kill, || {
            ZgcRealHeap::new_shared(16 * 1024 * 1024)
        });
        let unset = [
            ("CRATONVM_ZGC_TLAB_OWNED_STARTS", None),
            ("CRATONVM_ZGC_CARVE_NOTES_BLOCKS", None),
            ("CRATONVM_ZGC_TAIL_RETURN_CYCLE_CHECK", None),
        ];
        let on = cratonvm_types::flags::with_thread_overrides(&unset, || {
            ZgcRealHeap::new_shared(16 * 1024 * 1024)
        });
        let seeds = |h: &ZgcRealHeap| {
            (
                h.tlabs.owned_starts.load(Ordering::Relaxed),
                h.tlabs.carve_notes_blocks.load(Ordering::Relaxed),
                h.tlabs.tail_return_cycle_check.load(Ordering::Relaxed),
            )
        };
        assert_eq!(seeds(&off), (false, false, false));
        assert_eq!(seeds(&on), (true, true, true));
        // A refill outside the override follows the seed, not the environment.
        off.set_vm_tlab_enabled(true);
        on.set_vm_tlab_enabled(true);
        let _ = cratonvm_types::flags::with_thread_overrides(&unset, || off.refill_tlab(64 * 1024))
            .expect("chunk");
        assert_eq!(
            owned_vm_tlab_chunk_raw(off.tlabs.id()),
            (0, 0),
            "seeded off: the refill records no owned chunk"
        );
        let (ptr, size) =
            cratonvm_types::flags::with_thread_overrides(&kill, || on.refill_tlab(64 * 1024))
                .expect("chunk");
        let lo = ptr as usize;
        assert_eq!(
            owned_vm_tlab_chunk_raw(on.tlabs.id()),
            (lo, lo + size),
            "seeded on: an override at refill time no longer turns it off"
        );
    }

    /// A tail the retiring thread holds no record of invalidates every
    /// thread's record (the epoch moves), and a collection does too.
    #[test]
    fn a_foreign_tail_reclaim_and_a_collection_both_move_the_epoch() {
        let heap = ZgcRealHeap::new_shared(16 * 1024 * 1024);
        heap.set_vm_tlab_enabled(true);
        // zt t2/al (X1): seeded per heap; see the test above.
        heap.tlabs.owned_starts.store(false, Ordering::Relaxed);
        let (ptr, size) = heap.refill_tlab(64 * 1024).expect("chunk");
        let mut tlab = unsafe { crate::tlab::Tlab::new(ptr, size) };
        let _ = tlab_new_object(&heap, &mut tlab, ClassId::new(7), 2).expect("fits");
        let before = ownership_epoch().load(Ordering::SeqCst);
        tlab.retire();
        let after_retire = ownership_epoch().load(Ordering::SeqCst);
        assert!(
            after_retire > before,
            "an unrecorded tail must move the epoch"
        );
        let stw = unsafe { StopTheWorldToken::new_unchecked() };
        let mut roots: Vec<ObjectRef> = Vec::new();
        cratonvm_types::flags::with_thread_overrides(
            &[("CRATONVM_ZGC_RELOCATE", Some("0"))],
            || {
                heap.collect_garbage(&stw, &mut roots, &NoMonitors);
            },
        );
        assert!(
            ownership_epoch().load(Ordering::SeqCst) > after_retire,
            "a collection must move the epoch"
        );
    }

    /// Round 9 wave 8 (`zgc8`), `CRATONVM_ZGC_JIT_INLINE_ANNOUNCE`: a heap
    /// that owns the announce table publishes its bitmap geometry, closes the
    /// table while marking, arms exactly the VM-TLAB chunk it carved on this
    /// thread (under the record's epoch), and withdraws on drop. Nothing here
    /// runs JIT code; the machine-code half is
    /// `jit/src/x64/objects.rs::r9w8_zgc_inline_announce_tests`.
    ///
    /// The epoch is process-wide and every concurrent heap construction or
    /// collection bumps it, so arming is retried rather than asserted once;
    /// and the table has ONE owner, so if another heap in this process owns
    /// it the ownership assertions are skipped (nothing else in the suite
    /// sets the switch, so that should not happen).
    #[test]
    fn the_inline_announce_table_arms_only_the_carved_chunk() {
        let edits = [
            ("CRATONVM_ZGC_JIT_INLINE_ANNOUNCE", Some("1")),
            ("CRATONVM_ZGC_TLAB_OWNED_STARTS", Some("1")),
        ];
        let heap = cratonvm_types::flags::with_thread_overrides(&edits, || {
            ZgcRealHeap::new_shared(16 * 1024 * 1024)
        });
        heap.set_vm_tlab_enabled(true);
        let table = &crate::tlab::JIT_ZGC_ANNOUNCE;
        if table.owner.load(Ordering::SeqCst) != heap.arena_base {
            return;
        }
        let (words, base, span) = heap.registry.jit_bitmap_geometry().expect("bitmap arm");
        assert_eq!(table.words.load(Ordering::SeqCst), words);
        assert_eq!(table.base.load(Ordering::SeqCst), base);
        assert_eq!(table.span.load(Ordering::SeqCst), span);
        let gen_on = heap.generational_enabled.load(Ordering::SeqCst);
        assert_eq!(table.blocked.load(Ordering::SeqCst), usize::from(gen_on));
        // zr z9/bo: the short store's geometry and gate.
        let (bias, short_ok) = crate::tlab::JitZgcAnnounceTable::short_geometry(words, base);
        let short_closed = usize::from(gen_on || !short_ok);
        assert_eq!(table.granule_bias.load(Ordering::SeqCst), bias);
        assert_eq!(table.short_blocked.load(Ordering::SeqCst), short_closed);
        assert_eq!(heap.jit_short_announce_open(), Some(short_closed == 0));

        // Marking closes it, and ending the mark reopens it (unless the heap
        // is generational). The short gate follows.
        heap.set_mark_active(true);
        assert_eq!(
            table.blocked.load(Ordering::SeqCst),
            1,
            "closed while marking"
        );
        assert_eq!(table.short_blocked.load(Ordering::SeqCst), 1);
        heap.set_mark_active(false);
        assert_eq!(table.blocked.load(Ordering::SeqCst), usize::from(gen_on));
        assert_eq!(table.short_blocked.load(Ordering::SeqCst), short_closed);

        // The carved chunk is armed under the record's epoch.
        let mut armed = false;
        for _ in 0..64 {
            let (ptr, size) = cratonvm_types::flags::with_thread_overrides(&edits, || {
                heap.refill_tlab(64 * 1024)
            })
            .expect("chunk");
            let mut tlab = unsafe { crate::tlab::Tlab::new(ptr, size) };
            let (epoch, addr) = tlab.zgc_inline_announce_state();
            if addr != 0 {
                assert_eq!(addr, crate::tlab::jit_zgc_announce_table_addr());
                assert_ne!(epoch, 0);
                // A collection (or any invalidation) makes the JIT's epoch
                // compare fail, which sends it to the helper.
                invalidate_owned_vm_tlab_chunks();
                assert_ne!(ownership_epoch().load(Ordering::SeqCst), epoch);
                armed = true;
            }
            tlab.retire();
            assert_eq!(tlab.zgc_inline_announce_state(), (0, 0), "retire disarms");
            if armed {
                break;
            }
        }
        assert!(armed, "a freshly carved VM-TLAB chunk must be armed");

        // A buffer over a chunk this thread did not just carve is not armed.
        let (ptr, size) =
            cratonvm_types::flags::with_thread_overrides(&edits, || heap.refill_tlab(64 * 1024))
                .expect("chunk");
        // SAFETY: the tail of the chunk just carved, owned by this thread.
        let shifted = unsafe { crate::tlab::Tlab::new(ptr.add(512), size - 512) };
        assert_eq!(shifted.zgc_inline_announce_state(), (0, 0));
        drop(shifted);

        let arena_base = heap.arena_base;
        drop(heap);
        assert_ne!(
            table.owner.load(Ordering::SeqCst),
            arena_base,
            "a dropped heap withdraws its table"
        );
    }

    /// nio21 wave 9 L3: a VM-TLAB carve that records an owned chunk (the
    /// record that arms the JIT's inline start-bit store) notes every
    /// 32 KiB summary block the chunk covers BEFORE any object is registered,
    /// so a start bit the inline store writes straight into the words is
    /// never hidden from `nearest_base_in` by a clear block bit. With
    /// `CRATONVM_ZGC_CARVE_NOTES_BLOCKS=0` the carve notes nothing.
    #[test]
    fn a_vm_tlab_carve_notes_every_summary_block_of_its_chunk() {
        const BLOCK_BYTES: usize = 64 * 512;
        for (switch, want) in [(None, true), (Some("0"), false)] {
            let edits = [
                ("CRATONVM_ZGC_TLAB_OWNED_STARTS", Some("1")),
                ("CRATONVM_ZGC_CARVE_NOTES_BLOCKS", switch),
            ];
            let heap = cratonvm_types::flags::with_thread_overrides(&edits, || {
                ZgcRealHeap::new_shared(16 * 1024 * 1024)
            });
            heap.set_vm_tlab_enabled(true);
            let Some((ptr, size)) = cratonvm_types::flags::with_thread_overrides(&edits, || {
                heap.refill_tlab(256 * 1024)
            }) else {
                panic!("a 16 MiB heap serves a VM-TLAB chunk");
            };
            let bits = heap.registry.bits_for_test().expect("bitmap arm");
            let (lo, hi) = (ptr as usize, ptr as usize + size);
            let first = (lo - bits.base) / BLOCK_BYTES;
            let last = (hi - 1 - bits.base) / BLOCK_BYTES;
            if want {
                for blk in first..=last {
                    assert!(
                        bits.block_noted(blk),
                        "block {blk} of chunk {lo:#x}..{hi:#x} is not noted"
                    );
                }
            } else if last >= first + 2 {
                // Off: a block wholly inside a fresh chunk has had nothing
                // registered in it, so nothing noted it.
                assert!(
                    (first + 1..last).any(|blk| !bits.block_noted(blk)),
                    "the carve noted chunk {lo:#x}..{hi:#x} with the switch off"
                );
            }
        }
    }

    /// zr z1/al: a VM-TLAB tail retired in the cycle it was charged comes off
    /// `allocated`; a tail published across a collection (its owner could not
    /// retire) is re-charged by that collection, so after the owner resumes,
    /// what it bumped stays charged and only the unused rest comes off; and
    /// `CRATONVM_ZGC_TAIL_RETURN_CYCLE_CHECK=0` restores the old subtraction
    /// with no re-charge.
    #[test]
    fn a_tail_retired_after_a_collection_charges_exactly_what_its_owner_bumped() {
        /// `(after_gc, after_retire, published, retired, recharged)` for one
        /// run: `published` is the kept tail's bytes, `retired` the part the
        /// peer never bumped.
        fn run(check: Option<&str>) -> (usize, usize, usize, usize, usize) {
            let edits = [
                ("CRATONVM_ZGC_RELOCATE", Some("0")),
                ("CRATONVM_ZGC_TAIL_RETURN_CYCLE_CHECK", check),
            ];
            cratonvm_types::flags::with_thread_overrides(&edits, || {
                let heap = ZgcRealHeap::new_shared(16 * 1024 * 1024);
                heap.set_vm_tlab_enabled(true);
                // Same cycle: the unused tail comes off.
                let (p0, s0) = heap.refill_tlab(64 * 1024).expect("a chunk");
                let before = heap.allocated.load(Ordering::Relaxed);
                let tail0 = s0 - 4096;
                assert!(crate::tlab::TlabTailSink::reclaim_tlab_tail(
                    &*heap,
                    p0 as usize + 4096,
                    p0 as usize + s0
                ));
                let same_cycle_drop = before - heap.allocated.load(Ordering::Relaxed);
                assert_eq!(same_cycle_drop, tail0, "same cycle: the tail is given back");

                // Across a collection: the tail is published (a peer that could
                // not retire), the collection republishes `allocated`, the peer
                // resumes and bumps `bumped` bytes into its tail, and retires
                // the rest afterwards.
                let (p1, s1) = heap.refill_tlab(64 * 1024).expect("a chunk");
                let tail = (p1 as usize + 4096, p1 as usize + s1);
                heap.set_jit_tlab_skip_regions(&[tail]);
                // The in-flight cover may lower the published start over the
                // chunk's unregistered head; the recharge charges what was kept.
                let kept = heap.jit_tlab_skip_regions();
                let tail_len: usize = kept.iter().map(|&(s, e)| e - s).sum();
                let mut roots: Vec<ObjectRef> = vec![heap.alloc_object(ClassId::new(3), 4)];
                {
                    // SAFETY: single-threaded unit test.
                    let stw = unsafe { StopTheWorldToken::new_unchecked() };
                    heap.collect_garbage(&stw, &mut roots, &NoMonitors);
                }
                heap.clear_jit_tlab_skip_regions();
                let after_gc = heap.allocated.load(Ordering::Relaxed);
                // Half the tail (the chunk can be as small as 12 KiB here).
                let bumped = ((tail.1 - tail.0) / 2) & !(ZGC_TLAB_ALIGN - 1);
                assert!(bumped > 0);
                assert!(crate::tlab::TlabTailSink::reclaim_tlab_tail(
                    &*heap,
                    tail.0 + bumped,
                    tail.1
                ));
                (
                    after_gc,
                    heap.allocated.load(Ordering::Relaxed),
                    tail_len,
                    tail.1 - (tail.0 + bumped),
                    heap.published_tail_recharge_bytes(),
                )
            })
        }
        let (gc_on, retired_on, published, retired, recharged) = run(None);
        assert_eq!(
            recharged, published,
            "default: the published tail is re-charged"
        );
        assert!(gc_on >= published, "the re-charge is inside `allocated`");
        assert_eq!(
            retired_on,
            gc_on - retired,
            "default: what the peer bumped after the collection stays charged, the rest comes off"
        );
        let (gc_off, retired_off, _, _, recharged_off) = run(Some("0"));
        assert_eq!(recharged_off, 0, "kill switch: no re-charge");
        assert!(
            retired_off < gc_off || gc_off == 0,
            "kill switch: the old subtraction ({gc_off} -> {retired_off})"
        );
    }

    /// nio21 wave 9 L3: a published tail's start is lowered over the
    /// unregistered bytes below it (a frozen peer's bumped-but-unannounced
    /// object), down to the end of the highest registered object and never
    /// into one; a tail that starts exactly at an object's end, or inside one,
    /// keeps its start; and `CRATONVM_ZGC_SKIP_COVERS_INFLIGHT=0` publishes
    /// the tails as reported.
    #[test]
    fn a_published_tail_covers_the_unregistered_bytes_below_it() {
        let heap = ZgcRealHeap::new_shared(16 * 1024 * 1024);
        heap.set_vm_tlab_enabled(true);
        let x = heap.alloc_object(ClassId::new(3), 4).as_ptr() as usize;
        // SAFETY: `x` is a live registered base.
        let x_end =
            x + ZgcRealHeap::alloc_size(unsafe { &*(x as *const crate::heap::ObjectHeader) })
                .expect("sizable");
        let Some((ptr, size)) = heap.refill_tlab(64 * 1024) else {
            return; // no VM TLAB on this configuration: nothing to cover
        };
        let (lo, hi) = (ptr as usize, ptr as usize + size);
        assert!(lo >= x_end, "the chunk is carved above the object");
        let s = lo + 4096;
        let start = heap.inflight_cover_start(s);
        assert!(
            start <= lo,
            "the chunk's unregistered prefix is covered ({start:#x} > {lo:#x})"
        );
        assert!(start >= x_end, "never into a registered object");
        assert!(
            heap.registry
                .nearest_base_within_total(s - 1, start)
                .is_none(),
            "no registered base inside the lowered span"
        );
        assert_eq!(
            heap.inflight_cover_start(x_end),
            x_end,
            "ends exactly at the tail"
        );
        assert_eq!(
            heap.inflight_cover_start(x + 8),
            x + 8,
            "an extent into the tail keeps it"
        );

        assert_eq!(heap.inflight_cover_stats(), (0, 0), "nothing published yet");
        heap.set_jit_tlab_skip_regions(&[(s, hi)]);
        assert_eq!(heap.jit_tlab_skip_regions(), vec![(start, hi)]);
        // zr z1/al: the cover's engagement counter is the positive control
        // the host rows rely on, and it must count exactly the lowering.
        assert_eq!(heap.inflight_cover_stats(), (1, s - start));
        cratonvm_types::flags::with_thread_overrides(
            &[("CRATONVM_ZGC_SKIP_COVERS_INFLIGHT", Some("0"))],
            || heap.set_jit_tlab_skip_regions(&[(s, hi)]),
        );
        assert_eq!(heap.jit_tlab_skip_regions(), vec![(s, hi)], "kill switch");
        assert_eq!(
            heap.inflight_cover_stats(),
            (1, s - start),
            "the kill switch lowers nothing, so it counts nothing"
        );
        heap.clear_jit_tlab_skip_regions();
    }

    /// zs s5/ta: `CRATONVM_DBG_ZGC_POST_REFUSAL` parses as documented, arms
    /// its budget at an `alloc_raw` refusal, and spends one per hand-out after
    /// it. Off (the default), a refusal arms nothing.
    #[test]
    fn zs5ta_a_refusal_arms_the_post_refusal_log_only_when_asked() {
        use cratonvm_types::flags::with_thread_overrides;
        let key = "CRATONVM_DBG_ZGC_POST_REFUSAL";
        for (v, want) in [
            (None, 0),
            (Some("0"), 0),
            (Some("off"), 0),
            (Some("1"), 16),
            (Some("yes"), 16),
            (Some("5"), 5),
        ] {
            let got = with_thread_overrides(&[(key, v)], super::super::zgc_dbg_post_refusal_limit);
            assert_eq!(got, want, "{key}={v:?}");
        }
        // Fill a small heap with 4 KiB objects until `alloc_raw` refuses, then
        // ask for 16 bytes, which the leftover under 4 KiB can serve.
        let run = |v: Option<&str>| {
            with_thread_overrides(&[(key, v)], || {
                let heap = ZgcRealHeap::with_capacity(1 << 20);
                let mut refused = false;
                for _ in 0..4096 {
                    if heap.alloc_raw(4096, &|_| {}).is_none() {
                        refused = true;
                        break;
                    }
                }
                assert!(
                    refused,
                    "a 1 MiB heap must refuse a 4 KiB object eventually"
                );
                let armed = heap.counters.post_refusal_budget.load(Ordering::Relaxed);
                let small = heap.alloc_raw(16, &|_| {});
                let after = heap.counters.post_refusal_budget.load(Ordering::Relaxed);
                (armed, small.is_some(), after)
            })
        };
        let (armed, served, after) = run(Some("4"));
        assert_eq!(armed, 4, "the refusal arms the budget");
        if served {
            assert_eq!(after, 3, "a hand-out after the refusal spends one");
        }
        assert_eq!(run(None).0, 0, "off: a refusal arms nothing");
    }

    // The compaction-side obligation (a published tail is neither slid over
    // nor reclaimed) is asserted in `zgc.rs`'s test module, beside the other
    // relocation tests, because it needs that module's quiescence test lock:
    // `a_published_vm_tlab_tail_is_neither_slid_over_nor_reclaimed`.
}

#[cfg(test)]
mod infallible_retirement_tls_tests {
    use super::*;
    #[test]
    fn switching_six_heaps_keeps_each_threads_slot_charge_and_owned_chunk() {
        let heaps: Vec<_> = (0..6).map(|_| ZgcRealHeap::new_shared(16 << 20)).collect();
        for heap in &heaps {
            heap.set_vm_tlab_enabled(true);
        }
        let mut chunks = Vec::new();
        for heap in &heaps {
            let (ptr, bytes) = cratonvm_types::flags::with_thread_overrides(
                &[("CRATONVM_ZGC_TLAB_OWNED_STARTS", Some("1"))],
                || heap.refill_tlab(64 << 10),
            )
            .expect("first chunk");
            chunks.push((ptr as usize, ptr as usize + bytes));
        }
        // JIT round 21 wave 13 (lane vm-tests): read each heap's record raw.
        // Its VALIDITY is the process-wide `ownership_epoch`, which any
        // concurrent test's new heap (`with_capacity`) or collection bumps, so
        // `owned_vm_tlab_chunk_epoch(lo, hi).is_some()` failed under the
        // parallel harness whenever another gc test ran between the refills
        // and this loop. What this test owns is that switching heaps keeps
        // each heap's record: that is the raw `(lo, hi)` per registry id.
        for (heap, &(lo, hi)) in heaps.iter().zip(&chunks) {
            // The record itself, not `owned_vm_tlab_chunk_epoch`: that also
            // asks the process-wide ownership epoch, which any other test's
            // heap construction or collection bumps while this one runs.
            let recorded = OWNED_VM_TLAB_CHUNKS.with(|records| {
                records.borrow().iter().any(|(life, id, owned)| {
                    life.strong_count() != 0
                        && *id == heap.tlabs.id()
                        && owned.lo == lo
                        && owned.hi == hi
                })
            });
            assert!(recorded, "another heap replaced ownership");
            assert_eq!(heap.tlabs.vm_tlab_slots.load(Ordering::Relaxed), 1);
            assert_eq!(vm_tlab_charge(heap.tlabs.id()).unwrap().lo, lo);
        }
        for heap in &heaps {
            let (_, size) = heap.refill_tlab(64 << 10).expect("next chunk");
            assert_eq!(
                heap.tlabs.vm_tlab_slots.load(Ordering::Relaxed),
                1,
                "switching heaps counted the same thread twice"
            );
            assert_eq!(
                heap.tlabs.reserved_bytes.load(Ordering::Relaxed),
                size,
                "a live heap charge was evicted at the old four-slot boundary"
            );
        }
        let old_id = heaps[0].tlabs.id();
        drop(heaps);
        assert!(
            vm_tlab_charge(old_id).is_none(),
            "a destroyed heap remains live in TLS"
        );
    }
}
