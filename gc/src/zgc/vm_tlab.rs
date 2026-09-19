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
//! six atomic operations per object. `tlab_hit_count` was zero on this backend
//! by construction.
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
//! OPT-IN: `CRATONVM_ZGC_JIT_TLAB=1` turns it on, and unset means
//! `refill_tlab -> None` exactly as before -- nothing else in this module then
//! runs, because no chunk is ever handed out. The default is ONE constant,
//! [`ZGC_VM_TLAB_DEFAULT_ON`]; see [`zgc_vm_tlab_enabled_by_default`] for the
//! measurements that set it and for the gate that flips it.

use std::cell::Cell;
use std::sync::atomic::{AtomicU64, Ordering};

use cratonvm_types::ClassId;

use super::{zgc_corpse_enabled, ZgcRealHeap, ZGC_TLAB_ALIGN};

/// `CRATONVM_ZGC_TLAB_OWNED_STARTS`: register a VM-TLAB object's start bit
/// with a plain store when its bitmap word lies wholly inside the chunk this
/// thread owns. See [`OwnedVmTlabChunk`]. Default: [`ZGC_TLAB_OWNED_STARTS_DEFAULT_ON`]
/// (OFF; round 9 wave 5, lane `zgc5`, could not build or measure it). Read once
/// per refill, never per object.
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
    owned_vm_tlab_chunk()
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
    static OWNED_VM_TLAB_CHUNK: Cell<OwnedVmTlabChunk> =
        const { Cell::new(OwnedVmTlabChunk::NONE) };
}

/// This thread's record, if it is still valid.
#[inline]
fn owned_vm_tlab_chunk() -> Option<OwnedVmTlabChunk> {
    let owned = OWNED_VM_TLAB_CHUNK
        .try_with(|c| c.get())
        .unwrap_or(OwnedVmTlabChunk::NONE);
    (owned.hi > owned.lo && owned.epoch == ownership_epoch().load(Ordering::Acquire))
        .then_some(owned)
}

#[inline]
fn set_owned_vm_tlab_chunk(owned: OwnedVmTlabChunk) {
    let _ = OWNED_VM_TLAB_CHUNK.try_with(|c| c.set(owned));
}

/// `CRATONVM_ZGC_LAZY_TLAB_ZERO`: hand the VM thread's buffer its chunk
/// UNZEROED and let the buffer zero it [`crate::tlab::TLAB_LAZY_ZERO_STEP`]
/// at a time, just ahead of its bumps (round 9 wave 11, `zgc11`;
/// `perf-zgc-tlab-chunk-zeroing-is-the-last-allocation-cost-20260918.md`).
///
/// **Default OFF** (`runtime_flag_on`): written without a build, so it lands
/// dark until an interleaved A/B on a built binary shows it (the page lists
/// the runs). Read once per heap at construction.
///
/// # Why it should help where the pristine skip did not
///
/// The pristine skip (`CRATONVM_ZGC_PRISTINE_CHUNKS`) only covers memory the
/// arena never handed out, i.e. the start-up regime: after the first
/// collection every chunk is carved from recycled memory below the old
/// high-water mark and pays the whole chunk `memset` (`rep stosb` 11.9 % of
/// busy samples on `CratonBench bintrees` at `-Xmx512m`, wave-10 binary).
/// That `memset` is a separate pass over a ~512 KiB chunk that the
/// allocations then pull back into the cache line by line. Zeroing 32 KiB
/// just ahead of the cursor writes lines the next few hundred allocations
/// hit in L1/L2, and the unused tail of a retired chunk is never zeroed at
/// all.
///
/// # Contract
///
/// Every consumer's "`[cursor, end)` reads as zero" still holds: `end` is a
/// SOFT end (`Tlab::chunk_end` is the owned one), the JIT's inline bumps
/// miss at it and take their helper, and `Tlab::alloc_initialized` zeroes
/// the next step before it bumps. The hand-over is the thread-local
/// [`LazyZeroChunk`] record, which `Tlab::new` consumes when -- and only
/// when -- it is given exactly the recorded chunk. The one production caller
/// of `refill_tlab` (`gc_and_alloc::tlab_alloc_shaped_inner`) installs the
/// chunk with `Tlab::new(buf, size)` on the same thread immediately.
pub(crate) fn zgc_lazy_tlab_zero_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_on("CRATONVM_ZGC_LAZY_TLAB_ZERO")
}

/// The chunk [`ZgcRealHeap::refill_tlab`] just carved on this thread without
/// zeroing it, and the absolute pristine range inside it that needs no
/// zeroing (`(0, 0)` when none). See [`zgc_lazy_tlab_zero_enabled`].
#[derive(Clone, Copy)]
struct LazyZeroChunk {
    lo: usize,
    hi: usize,
    skip: (usize, usize),
}

impl LazyZeroChunk {
    const NONE: Self = Self {
        lo: 0,
        hi: 0,
        skip: (0, 0),
    };
}

thread_local! {
    static LAZY_ZERO_CHUNK: Cell<LazyZeroChunk> = const { Cell::new(LazyZeroChunk::NONE) };
}

/// Record `[lo, hi)` as this thread's unzeroed VM-TLAB chunk. `false` when
/// the thread-local is gone (thread teardown): the caller must then zero the
/// chunk itself, because no `Tlab::new` will ever learn it is unzeroed.
fn set_lazy_zero_chunk(lo: usize, hi: usize, skip: (usize, usize)) -> bool {
    LAZY_ZERO_CHUNK
        .try_with(|c| c.set(LazyZeroChunk { lo, hi, skip }))
        .is_ok()
}

/// `Tlab::new`'s question: is `[lo, hi)` exactly the chunk this thread's
/// last lazy refill carved unzeroed? Then consume the record and return its
/// pristine skip range. `None` -- record untouched -- for any other span,
/// which is then zeroed memory exactly as before.
pub(crate) fn take_lazy_zero_chunk(lo: usize, hi: usize) -> Option<(usize, usize)> {
    LAZY_ZERO_CHUNK
        .try_with(|c| {
            let rec = c.get();
            if rec.hi > rec.lo && rec.lo == lo && rec.hi == hi {
                c.set(LazyZeroChunk::NONE);
                Some(rec.skip)
            } else {
                None
            }
        })
        .ok()
        .flatten()
}

/// `(lo, hi)` of this thread's record, valid or not. Tests only: the epoch
/// is process-wide, and any concurrent test's collection bumps it.
#[cfg(test)]
fn owned_vm_tlab_chunk_raw() -> (usize, usize) {
    let owned = OWNED_VM_TLAB_CHUNK
        .try_with(|c| c.get())
        .unwrap_or(OwnedVmTlabChunk::NONE);
    (owned.lo, owned.hi)
}

/// `CRATONVM_ZGC_JIT_TLAB`: hand the VM thread's TLAB a chunk on this backend.
///
/// **Default OFF.** (2026-09-02 reasoning below; 2026-09-18 re-measure after
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
/// the rounds; the whole table is in `docs/internal/jit-review-r9/NOTES-w3-gczgc3.md`):
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
/// `docs/internal/jit-review-r9/NOTES-w5-zgc5.md`.
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

    /// Whether [`Self::refill_tlab`] hands its chunks out unzeroed for the
    /// buffer to zero lazily (`CRATONVM_ZGC_LAZY_TLAB_ZERO`).
    pub fn vm_tlab_lazy_zero_enabled(&self) -> bool {
        self.counters.vm_tlab_lazy_zero.load(Ordering::Relaxed)
    }

    /// Test and diagnostic override of [`zgc_lazy_tlab_zero_enabled`].
    pub fn set_vm_tlab_lazy_zero(&self, on: bool) {
        self.counters.vm_tlab_lazy_zero.store(on, Ordering::Relaxed);
    }

    /// The `Zgc` arm of `VmHeap::refill_tlab`: a zeroed chunk of at most
    /// `requested` bytes (never below the VM's minimum TLAB, never above the
    /// per-thread ceiling the heap's own buffers observe), or `None` when the
    /// arena cannot serve one -- the caller then allocates per object through
    /// `try_alloc_object_full`, which is the path every allocation took before.
    ///
    /// Under `CRATONVM_ZGC_LAZY_TLAB_ZERO` the chunk is NOT zeroed: it must be
    /// installed with `Tlab::new(ptr, size)` on this same thread, which adopts
    /// it as lazily zeroed (see [`zgc_lazy_tlab_zero_enabled`]). The VM's one
    /// caller does exactly that.
    pub fn refill_tlab(&self, requested: usize) -> Option<(*mut u8, usize)> {
        if !self.vm_tlab_enabled() {
            return None;
        }
        let capacity = self.arena_end.saturating_sub(self.arena_base);
        let ceiling = self.tlabs.chunk_bytes_now(capacity);
        if ceiling == 0 {
            return None;
        }
        let floor = crate::tlab::min_tlab_size().min(ceiling);
        let want = requested.clamp(floor, ceiling) & !(ZGC_TLAB_ALIGN - 1);
        if want == 0 {
            return None;
        }
        // A recycled block shorter than `want` is accepted down to the VM's
        // minimum buffer: the VM retries the object that missed against the
        // new buffer and falls to the per-object path if it does not fit, so
        // a short chunk costs one miss, not a failure.
        let need = floor;
        let (ptr, size) = if self.counters.vm_tlab_lazy_zero.load(Ordering::Relaxed) {
            // LAZY ZEROING (`zgc_lazy_tlab_zero_enabled`): carve without the
            // `memset` and tell this thread's next `Tlab::new` so. If the
            // record cannot be written, nobody would ever zero the chunk, so
            // zero it here exactly as the eager carve would have.
            let (ptr, size, skip) = self.carve_tlab_chunk_unzeroed(want, need)?;
            let (lo, hi) = (ptr as usize, ptr as usize + size);
            if !set_lazy_zero_chunk(lo, hi, skip) {
                let (s_lo, s_hi) = if skip.0 < skip.1 {
                    (skip.0.clamp(lo, hi), skip.1.clamp(lo, hi))
                } else {
                    (hi, hi)
                };
                // SAFETY: `[lo, hi)` was just carved for this caller alone;
                // both pieces lie inside it (`lo <= s_lo <= s_hi <= hi`).
                unsafe {
                    std::ptr::write_bytes(ptr, 0, s_lo - lo);
                    std::ptr::write_bytes(s_hi as *mut u8, 0, hi - s_hi);
                }
            }
            (ptr, size)
        } else {
            // A record left by an earlier lazy refill must never outlive the
            // mode: its pristine range would be stale by the time anything
            // matched it.
            let _ = set_lazy_zero_chunk(0, 0, (0, 0));
            self.carve_tlab_chunk(want, need)?
        };
        // The VM installs this chunk in the calling thread's buffer (the one
        // caller, `gc_and_alloc::tlab_alloc_shaped_inner`, refills
        // `thread.tlab` on the thread it runs on). Record it for
        // `note_tlab_object`'s owned-word path, or clear any older record when
        // the switch is off.
        set_owned_vm_tlab_chunk(if zgc_tlab_owned_starts_enabled() {
            OwnedVmTlabChunk {
                lo: ptr as usize,
                hi: ptr as usize + size,
                epoch: ownership_epoch().load(Ordering::Acquire),
            }
        } else {
            OwnedVmTlabChunk::NONE
        });
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
    /// Behind `CRATONVM_ZGC_TLAB_OWNED_STARTS` (default off): when the object's
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
        match owned_vm_tlab_chunk() {
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
    pub fn set_jit_tlab_skip_regions(&self, regions: &[(usize, usize)]) {
        let mut kept = self.counters.jit_tlab_skip.lock();
        kept.clear();
        kept.extend(
            regions
                .iter()
                .copied()
                .filter(|(s, e)| e > s && *s >= self.arena_base && *e <= self.arena_end),
        );
    }

    /// Clear the list set by [`Self::set_jit_tlab_skip_regions`].
    pub fn clear_jit_tlab_skip_regions(&self) {
        self.counters.jit_tlab_skip.lock().clear();
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
            || !zgc_tlab_owned_starts_enabled()
            || zgc_corpse_enabled()
            || crate::a2dbg::enabled()
            || self.arena_base == 0
        {
            return false;
        }
        let Some((words, base, span)) = self.registry.jit_bitmap_geometry() else {
            return false;
        };
        if words == 0 || span == 0 {
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
        table.blocked.store(
            usize::from(self.jit_inline_announce_blocked_now()),
            Ordering::Release,
        );
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
        table.words.store(0, Ordering::Release);
        table.span.store(0, Ordering::Release);
        table.base.store(0, Ordering::Release);
        table.blocked.store(0, Ordering::Release);
        table.owner.store(0, Ordering::Release);
    }

    /// What the table's `blocked` word must say right now: the inline store
    /// does neither allocate-black nor `note_young_page`, so it is closed
    /// while either could be needed.
    fn jit_inline_announce_blocked_now(&self) -> bool {
        self.mark_active.load(Ordering::Relaxed)
            || self.generational_enabled.load(Ordering::Relaxed)
    }

    /// Set the table's `blocked` word, if this heap owns the table. Callers
    /// raise it BEFORE the state that needs the helper becomes true and lower
    /// it only AFTER that state is false again (`set_mark_active`,
    /// `set_generational_enabled`).
    pub(crate) fn set_jit_inline_announce_blocked(&self, blocked: bool) {
        let table = &crate::tlab::JIT_ZGC_ANNOUNCE;
        if self.arena_base != 0 && table.owner.load(Ordering::Acquire) == self.arena_base {
            table.blocked.store(usize::from(blocked), Ordering::Release);
        }
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
    fn reclaim_tlab_tail(&self, start: usize, end: usize) -> bool {
        if end <= start || start < self.arena_base || end > self.arena_end {
            return false;
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
        let own_record = OWNED_VM_TLAB_CHUNK
            .try_with(|c| {
                let owned = c.get();
                let covers = owned.hi > owned.lo && start >= owned.lo && end <= owned.hi;
                if covers {
                    c.set(OwnedVmTlabChunk::NONE);
                }
                covers && owned.epoch == ownership_epoch().load(Ordering::Acquire)
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
        // The chunk was charged whole at refill; the part that was never
        // handed out goes back. Saturating: a collection between the refill
        // and this retire has already republished `allocated` as live bytes.
        let _ = self
            .allocated
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |a| {
                Some(a.saturating_sub(bytes))
            });
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

    #[test]
    fn a_shared_heap_hands_the_vm_a_zeroed_chunk_inside_its_arena() {
        let heap = ZgcRealHeap::new_shared(16 * 1024 * 1024);
        heap.set_vm_tlab_enabled(true);
        // The EAGER arm: this test reads the chunk before any `Tlab` exists.
        heap.set_vm_tlab_lazy_zero(false);
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
        let (ptr, size) = cratonvm_types::flags::with_thread_overrides(
            &[("CRATONVM_ZGC_TLAB_OWNED_STARTS", Some("1"))],
            || heap.refill_tlab(64 * 1024),
        )
        .expect("chunk");
        let lo = ptr as usize;
        assert_eq!(owned_vm_tlab_chunk_raw(), (lo, lo + size));
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
            owned_vm_tlab_chunk_raw(),
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

        let _ = cratonvm_types::flags::with_thread_overrides(
            &[("CRATONVM_ZGC_TLAB_OWNED_STARTS", Some("0"))],
            || heap.refill_tlab(64 * 1024),
        )
        .expect("second chunk");
        assert_eq!(owned_vm_tlab_chunk_raw(), (0, 0), "switch off: no record");
    }

    /// A tail the retiring thread holds no record of invalidates every
    /// thread's record (the epoch moves), and a collection does too.
    #[test]
    fn a_foreign_tail_reclaim_and_a_collection_both_move_the_epoch() {
        let heap = ZgcRealHeap::new_shared(16 * 1024 * 1024);
        heap.set_vm_tlab_enabled(true);
        let (ptr, size) = cratonvm_types::flags::with_thread_overrides(
            &[("CRATONVM_ZGC_TLAB_OWNED_STARTS", Some("0"))],
            || heap.refill_tlab(64 * 1024),
        )
        .expect("chunk");
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

        // Marking closes it, and ending the mark reopens it (unless the heap
        // is generational).
        heap.set_mark_active(true);
        assert_eq!(
            table.blocked.load(Ordering::SeqCst),
            1,
            "closed while marking"
        );
        heap.set_mark_active(false);
        assert_eq!(table.blocked.load(Ordering::SeqCst), usize::from(gen_on));

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

    /// Lazy zeroing (round 9 wave 11, `zgc11`): a chunk carved UNZEROED from
    /// memory holding stale bytes still hands out nothing but zero, the
    /// buffer owns (and publishes, and returns) the whole chunk from the
    /// start, and zeroing only ever runs a step ahead of the cursor.
    #[test]
    fn a_lazily_zeroed_vm_tlab_hands_out_only_zero_and_owns_the_whole_chunk() {
        // 64 MiB: the chunk ceiling is `capacity / 1024`, so a 64 KiB chunk
        // (two zeroing steps) needs at least this much arena.
        let heap = ZgcRealHeap::new_shared(64 * 1024 * 1024);
        heap.set_vm_tlab_enabled(true);
        heap.set_vm_tlab_lazy_zero(true);
        // A recycled block, not fresh memory: nothing may be skipped.
        heap.arena.lock().set_skip_pristine_zeroing(false);
        let (ptr, size) = heap.refill_tlab(64 * 1024).expect("chunk");
        let (lo, hi) = (ptr as usize, ptr as usize + size);
        assert!(
            size >= 2 * crate::tlab::TLAB_LAZY_ZERO_STEP,
            "chunk of {size} bytes"
        );
        // Stale bytes, as a block off the free list would hold. The carve did
        // not clear them, so this is exactly what the buffer is handed.
        // SAFETY: the chunk was just carved for this thread alone.
        unsafe { std::ptr::write_bytes(ptr, 0xAB, size) };
        let mut tlab = unsafe { crate::tlab::Tlab::new(ptr, size) };
        assert_eq!(
            tlab.lazy_zero_state(),
            (lo, hi),
            "adopted: nothing zeroed yet"
        );
        assert_eq!(tlab.remaining(), size);
        assert_eq!(
            tlab.reserved_tail(),
            Some((lo, hi)),
            "the whole chunk is owned"
        );

        let first = tlab.alloc(48, 8).expect("the first bump zeroes a step");
        assert_eq!(first as usize, lo);
        let (end, chunk_end) = tlab.lazy_zero_state();
        assert_eq!(
            end,
            lo + crate::tlab::TLAB_LAZY_ZERO_STEP,
            "one step, not the chunk"
        );
        assert_eq!(chunk_end, hi);
        // SAFETY: inside the chunk, which is mapped.
        let above = unsafe { std::slice::from_raw_parts(end as *const u8, hi - end) };
        assert!(
            above.iter().all(|b| *b == 0xAB),
            "nothing zeroed past the step"
        );
        assert_eq!(tlab.reserved_tail(), Some((lo + 48, hi)));

        // Everything handed out reads zero, all the way to the chunk's end.
        let mut n = 1usize;
        let mut last = first as usize;
        while let Some(p) = tlab.alloc(48, 8) {
            // SAFETY: `p..p+48` was just handed out by the buffer.
            let obj = unsafe { std::slice::from_raw_parts(p as *const u8, 48) };
            assert!(
                obj.iter().all(|b| *b == 0),
                "stale byte handed out at {p:p}"
            );
            last = p as usize;
            n += 1;
        }
        assert_eq!(n, size / 48, "the whole chunk is usable");
        assert!(last + 48 <= hi);

        assert_eq!(tlab.lazy_zero_state(), (hi, hi), "zeroed to the end by now");
        // A lazy record is consumed by the one `Tlab::new` it names.
        assert_eq!(take_lazy_zero_chunk(lo, hi), None);
        tlab.retire();
        assert!(tlab.is_retired());
        assert_eq!(tlab.lazy_zero_state(), (0, 0));
    }

    /// The retire of a lazily zeroed buffer early in its life returns the
    /// whole never-zeroed rest of the chunk, and a buffer over any other span
    /// never adopts the record.
    #[test]
    fn a_lazy_record_matches_only_its_own_chunk_and_the_early_tail_goes_back_whole() {
        let heap = ZgcRealHeap::new_shared(16 * 1024 * 1024);
        heap.set_vm_tlab_enabled(true);
        heap.set_vm_tlab_lazy_zero(true);
        let (ptr, size) = heap.refill_tlab(64 * 1024).expect("chunk");
        let (lo, hi) = (ptr as usize, ptr as usize + size);
        // A heap staging buffer over the same span never adopts it...
        let staging = unsafe { crate::tlab::Tlab::new_heap_staging(ptr, size) };
        assert_eq!(staging.lazy_zero_state(), (hi, hi));
        drop(staging);
        // ...nor does a VM buffer over a different span; the record survives.
        // SAFETY: a sub-span of the chunk just carved, owned by this thread;
        // this buffer is dropped without allocating.
        let shifted = unsafe { crate::tlab::Tlab::new(ptr.add(512), size - 512) };
        assert_eq!(shifted.lazy_zero_state(), (hi, hi));
        drop(shifted);
        let mut tlab = unsafe { crate::tlab::Tlab::new(ptr, size) };
        assert_eq!(tlab.lazy_zero_state(), (lo, hi));
        assert!(tlab.alloc(64, 8).is_some());
        let free_before = heap.arena.lock().free_list_bytes();
        tlab.retire();
        assert_eq!(heap.arena.lock().free_list_bytes() - free_before, size - 64);

        // With the switch off, a refill leaves no record behind.
        heap.set_vm_tlab_lazy_zero(false);
        let (p2, s2) = heap.refill_tlab(64 * 1024).expect("chunk");
        assert_eq!(take_lazy_zero_chunk(p2 as usize, p2 as usize + s2), None);
        let eager = unsafe { crate::tlab::Tlab::new(p2, s2) };
        assert_eq!(
            eager.lazy_zero_state(),
            (p2 as usize + s2, p2 as usize + s2)
        );
    }

    // The compaction-side obligation (a published tail is neither slid over
    // nor reclaimed) is asserted in `zgc.rs`'s test module, beside the other
    // relocation tests, because it needs that module's quiescence test lock:
    // `a_published_vm_tlab_tail_is_neither_slid_over_nor_reclaimed`.
}
