// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! ZGC relocation — the object copy protocol, the worker pool that drives it,
//! the lazy-remap lifetime state machine, and the per-cycle relocation record.
//!
//! This is the module that makes ZGC a *moving* collector in this tree. Every
//! other piece of CratonVM that moves an object — the semi-space Cheney copy in
//! [`crate::gc`], the generational promotion in [`crate::gen_heap`], G1's
//! parallel evacuation in [`crate::g1`] — does so with **every mutator parked**.
//! This module is the first one written against the opposite assumption, so it
//! is deliberately paranoid, and everything it cannot yet make safe against a
//! running mutator is named in the "Deferred to the STW path" section below
//! rather than being shipped as a plausible-looking race.
//!
//! # Read this first: the STW path is the one that lands
//!
//! [`ZRelocate::relocate_stw`] performs exactly the same relocation with all
//! mutators parked, behind a [`StopTheWorldToken`]. **That is the path intended
//! to land first**, for three reasons:
//!
//! 1. It is *testable today*. The concurrent path's correctness depends on a
//!    load barrier and a thread-stack handshake that do not exist yet (see
//!    "Deferred"), so a green concurrent test would be measuring its own test
//!    harness, not the VM.
//! 2. It is *correct without the barrier*. Under STW nothing can observe a
//!    from-space object between the copy and the reference fix-up, so the whole
//!    lazy-remap machinery collapses to "remap everything inside the pause",
//!    and the subtle page-lifetime invariant below collapses with it.
//! 3. It ships **compaction** — the actual user-visible win — before it ships
//!    *concurrency*. `ZgcRealHeap` is non-moving today and has no defragmenting
//!    story at all; an STW relocation phase fixes that on its own.
//!
//! The concurrent path ([`ZRelocateWorkers::relocate_set`]) is written, is
//! structurally hang-proof, and is exercised by the unit tests, but it should
//! stay behind a flag until the barrier and the handshake exist.
//!
//! # The empty `pointer_map` at `gc/src/zgc.rs`
//!
//! `<ZgcRealHeap as GarbageCollector>::collect_garbage` builds
//!
//! ```text
//!     // Non-moving: no object changed address, so roots and external
//!     // references need no fix-up and the pointer map is empty.
//!     let pointer_map: HashMap<usize, usize> = HashMap::new();
//! ```
//!
//! and its comment blesses that as exactly correct. **It is exactly correct
//! today and it becomes silent heap corruption the moment this module runs.**
//! An empty map is not "no information", it is the positive claim *nothing
//! moved*: [`crate::collector::MonitorCleanup::remap_after_gc`] early-returns
//! on an empty map, and the VM's external tables (monitor table, cas-lock
//! table, JNI/JVMTI handles, class statics, the interned string pool) would all
//! keep from-space addresses while the objects live somewhere else. The
//! `prune_dead` workaround bolted on for the non-moving backend makes it worse,
//! not better: a *relocated* object's old address is neither dead-and-gone nor
//! unchanged, and pruning it would drop a live object's monitor.
//!
//! [`ZRelocationRecord`] is the fix: it accumulates `from -> to` for the whole
//! cycle in `HashMap<usize, usize>` — the exact type
//! [`crate::gc::GcResult::pointer_map`] holds — so the wiring step is a
//! three-line change (see [`ZRelocationRecord::into_pointer_map`] for the
//! checklist).
//!
//! # A second integration defect, found while writing this: the 41-bit `to`
//!
//! `ZgcForwardingEntry` gives the destination field 41 bits at 8-byte
//! granularity — a **16 TiB ceiling** — justified by ZGC's 42-bit
//! colored-pointer *offset*. But [`ZPageAllocator`]'s heap is a `Vec<u8>`, so
//! the addresses it hands out are **raw process addresses**, and on Linux a
//! quarter-gigabyte `Vec` comes from `mmap` at around `0x7f…` ≈ 2^47 — eight
//! times past the ceiling. Every `try_insert` would return `Unencodable`.
//! Windows heap addresses happen to sit low enough to work, so this is a defect
//! that ships green on a developer box and fails on the build host. This module
//! stores destinations **relative to the heap base**; see
//! [`ZRelocateConfig::to_encoding_base`] and [`ZRelocate::decode_to`], and note
//! the rule that follows from it: **nothing outside this module may call
//! `ZForwardingTable::find` to obtain an address.**
//!
//! # Address domain: this module is ABSOLUTE, the barrier is OFFSET
//! (reconciled 2026-08-07)
//!
//! `gc/src/zgc/barrier.rs` decided, on the same day and with a derivation worth
//! reading, that **a reference word — and therefore everything
//! `ZBarrierContext::forward` takes and returns — is a 42-bit heap offset from
//! [`ZVirtualAddressSpace::base`](crate::zgc::vaddr::ZVirtualAddressSpace::base),
//! never a machine pointer.** That decision is correct and this module does not
//! contest it. Its three legs, restated so nobody has to re-derive them:
//!
//! 1. It is what is physically in the slot: `vaddr::color` builds
//!    `Z_COLORED_TAG | color.bit() | (offset & Z_OFFSET_MASK)`.
//! 2. The 42-bit `address_mask` is *forced*: `Z_METADATA_SHIFT == Z_OFFSET_BITS
//!    == 42`, so a mask widened toward `Z_MAX_ADDRESS`'s 47 bits would overlap
//!    `Z_MARKED0..Z_FINALIZABLE` and `destination | heal_color` would corrupt
//!    the color. There is no room in the encoding for a machine address.
//! 3. An offset is bounded by `Z_MAX_HEAP_SIZE` wherever `mmap` placed the
//!    reservation. A machine address is not, which is the Linux-only failure
//!    this effort has already hit once.
//!
//! **But this module is, and must remain, absolute-domain internally.** It
//! `copy_nonoverlapping`s object bytes, it range-checks against
//! `ZPageReal::base()`/`end()`, and `ZRelocateContext::alloc_in` hands back a
//! real pointer to write through. Every one of those needs a machine address.
//!
//! So the two domains meet *here*, and this module owns the conversion:
//!
//! | entry point | domain | audience |
//! |---|---|---|
//! | [`ZRelocate::forward`] / [`ZRelocate::forward_lookup`] | **absolute** | the relocator, the STW driver, external address-keyed tables (monitor table, JNI handles, interned strings) |
//! | [`ZRelocate::forward_offset`] / [`ZRelocate::forward_lookup_offset`] | **offset** | the load barrier, and *only* the load barrier |
//!
//! Earlier drafts of this file's docs sent the load barrier to
//! [`ZRelocate::forward`]. **That was wrong** and is corrected on both methods:
//! a `ZBarrierContext::forward` written naively over `ZRelocate::forward`
//! returns a machine pointer into the offset domain — a wrong value that looks
//! entirely plausible, and the exact shape of the fixed G1 `address + 3` defect.
//! The `_offset` pair exists so that the correct call is the one whose *name*
//! states the domain, and so that the conversion is written once rather than at
//! each adapter.
//!
//! ## No newtype, deliberately — agreeing with `barrier.rs`
//!
//! `forwarding.rs` needed [`ZForwardingPayload`] because a payload's relation to
//! an address is unknowable from the word: it differs by an arbitrary runtime
//! constant and nothing but the type can tell them apart. The offset/address
//! confusion is not like that — **it is machine-checkable.** An offset satisfies
//! `value & !Z_OFFSET_MASK == 0` by construction; a Linux heap address near
//! `0x7f…` misses it by five bits. So the `_offset` methods check, in **release
//! builds as well as debug** (`ZRelocate::check_offset_domain`), for the same
//! reason `barrier.rs` gives: on Windows the reservation sits low and the
//! mistake is invisible, so a debug-only check is a check that runs only where
//! the bug cannot be seen.
//!
//! ## `to_encoding_base: Some(0)` is a Linux footgun, and is now refused
//!
//! [`ZRelocateConfig::to_encoding_base`] documents `Some(0)` as the eventual
//! resting state — "once ZGC reserves its own low virtual address space … at
//! which point absolute and encoded coincide". True, but set today, over a
//! `Vec<u8>` heap that Linux `mmap`s near `0x7f…`, it makes **every** insert
//! fail. `forwarding.rs` cannot catch this: it never learns the heap base, by
//! design. This module does. [`ZRelocate::try_new`] refuses such a
//! configuration, and [`ZRelocate::new`] logs it at `error!` and falls back to
//! the allocator base rather than shipping a relocator that cannot forward.
//!
//! # Deferred to the STW path (things this module refuses to do concurrently)
//!
//! * **Copying an object whose mark word a mutator may be CAS-ing.** The byte
//!   copy is a `copy_nonoverlapping` over a range that contains an
//!   `AtomicU64` ([`cratonvm_types::ObjectHeader::mark_word`], which since the
//!   24 -> 16 shrink also carries `kind`/`element_type`/`gc_age`/`gc_flags`).
//!   A concurrent `monitorenter` on the from-space object is a data race in
//!   the abstract machine and a lost thin-lock in practice. This module
//!   therefore never touches the mark word itself: the pre/post snapshot pair
//!   ([`ZRelocateContext::pre_copy_snapshot`] /
//!   [`ZRelocateContext::post_copy_apply`]) exists so the *VM-side* impl does
//!   the atomic transfer, mirroring the sequence `SharedEvac::evacuate` uses in
//!   `gc/src/g1.rs`. Under STW the snapshot is trivially stable; concurrently
//!   it is only stable once the handshake below exists.
//! * **Starting concurrent relocation without a thread-stack handshake.**
//!   OpenJDK ZGC can relocate concurrently only because the relocate-start
//!   handshake has already remapped every mutator's stack and registers, so
//!   after it *no mutator holds a from-space reference in a root slot* and the
//!   only remaining way to reach a from-space object is a heap-slot load, which
//!   passes the load barrier. CratonVM has no thread-local handshake
//!   mechanism in the `gc` crate. Without it, a mutator can `monitorenter`,
//!   write a field, or hand a raw pointer to JNI on an object this module is
//!   mid-copy. **`ZRelocateWorkers::relocate_set` must not be enabled until
//!   that handshake exists.**
//! * **Recycling a from-space page's *address range* promptly.** See invariant
//!   ZR-1 below: without OpenJDK's virtual/physical split, prompt recycling is
//!   unsound, so this module quarantines the range instead.
//! * **Relocating a JNI-critical / pinned object.** There is no pin table
//!   wired in here. Large pages are already excluded by
//!   [`crate::zgc::forwarding::ZRelocationPolicy::relocate_large_pages`], which
//!   covers the common `GetPrimitiveArrayCritical` case by accident but not on
//!   purpose.
//! * **In-place slot rewriting on the from-space object.** G1 does this for
//!   its self-forwarded evacuation-failure objects, but only after every
//!   parallel worker has stopped. Concurrently it is a write to memory a
//!   mutator may still be reading. This module never writes to from-space.
//!
//! # INVARIANT ZR-1 — the from-space page lifetime rule
//!
//! *This is the subtlest rule in the module. Read it before changing anything
//! that frees, resets or re-selects a page.*
//!
//! Let `P` be a page in the relocation set and `T(P)` its
//! [`ZForwardingTable`], installed in the [`ZForwardingRegistry`] under
//! `P.id()`.
//!
//! **(a) When P's bytes stop being read.** Once every live object in
//! `P.walk_bounds()` has a published entry in `T(P)` — state
//! [`ZPageRemapState::Relocated`] — nothing ever reads P's bytes again. A
//! reader holding a P-address consults `T(P)`, gets a to-space address, and
//! dereferences *that*. Reading a from-space payload after `Relocated` is a bug
//! on the reader's side, not a lifetime the page owes anybody.
//!
//! **(b) When T(P) may be dropped.** *Not then.* ZGC does not fix up references
//! eagerly; the load barrier remaps them lazily as they are loaded, and the
//! stragglers are swept up by the **next cycle's mark**, which traverses the
//! whole heap and every root set. `T(P)` must stay resolvable until that
//! traversal has completed. Concretely: a page relocated during cycle *N* may
//! only have its table uninstalled after the mark of cycle *N+1* has run to
//! completion with `T(P)` installed throughout — that is
//! [`ZRemapState::note_mark_complete`]'s strictly-greater-cycle rule. Only then
//! does P reach [`ZPageRemapState::Remapped`].
//!
//! **(c) When P's address range may be recycled — the rule that is easy to get
//! wrong.** Not at (a). At (b).
//!
//! > **ZR-1: a from-space page may be returned to [`ZPageAllocator`] only after
//! > its forwarding table has been uninstalled, i.e. only in state
//! > [`ZPageRemapState::Remapped`]. Between `Relocated` and `Remapped` the page
//! > is *quarantined*: its bytes are dead, but its address range must not back
//! > a new allocation and its `page_id` must not be reused.**
//!
//! Why, precisely. `ZPageAllocator::free_page` zeroes a Small/Medium page and
//! pushes it onto the reuse cache **with the same base address and the same
//! `page_id`**. Suppose we recycled at (a) and a fresh object `X` were then
//! allocated at from-offset `F` inside the recycled range, with `T(P)` still
//! installed. References to `X` created now carry the *current* good colour, so
//! the load barrier's fast path passes them through and never consults `T(P)` —
//! all fine, right up until the next mark's colour flip makes every one of
//! those references bad-coloured. The barrier then does consult `T(P)`, finds
//! the entry the *dead* object at offset `F` left behind, and forwards a live
//! reference to `X` onto a completely unrelated to-space object. Two distinct
//! Java objects silently become one. This is not a rare interleaving; it is the
//! guaranteed outcome of recycling early on any page whose offsets get reused.
//!
//! OpenJDK dodges the quarantine with a **virtual/physical split**: the
//! *physical* segments of a relocated page are returned immediately (which is
//! where ZGC's famously prompt memory reclaim comes from) while the *virtual*
//! range is held back by `ZVirtualMemoryManager` until the `ZForwarding` object
//! is destroyed. Reusing physical pages under a fresh virtual range cannot
//! collide with a surviving forwarding table, because the collision is an
//! *address* collision. [`ZPageAllocator`] in this tree has exactly one
//! reservation and granule == virtual == physical, so the split does not exist
//! and **quarantine is the correct conservative rule here**. Lifting it
//! requires the split — not a smaller change, and not a flag.
//!
//! Under STW the whole staircase collapses:
//! [`ZRemapState::note_stw_remap_complete`] promotes `Relocated -> Remapped`
//! inside the pause, because the STW driver rewrote every reference before
//! resuming the world, so no stale reference exists to be mis-forwarded. That
//! is the third reason the STW path lands first.
//!
//! # What the fixed G1 evacuation CAS race taught this module
//!
//! `SharedEvac::evacuate` in `gc/src/g1.rs` had a defect (fixed on dev
//! `d1d026ad3`) where the CAS *winner* encoded `target | MARK_FORWARDED` but
//! the *loser* returned the raw word `compare_exchange` handed back, so every
//! reference the loser wrote was `address + 3`. It presented as a **3-hour
//! hang** rather than a crash, because the panicking worker skipped its
//! `outstanding.fetch_sub(1)` and every survivor spun forever on a counter that
//! would never reach zero. Two rules follow, and both are structural here:
//!
//! * **One payload helper.** Every destination address that leaves this module
//!   comes out of the value [`ZForwardingTable::try_insert`] returned — never
//!   out of the local variable holding *our* copy. The local is named
//!   `my_copy` and is dead after the publish; see
//!   [`ZRelocate::relocate_object`]. The decode itself lives one layer down in
//!   `ZgcForwardingEntry::to`, which `forwarding.rs` already funnels every arm
//!   through.
//! * **No worker may leak its outstanding count.** [`ZOutstandingGuard`] is an
//!   RAII guard taken when a page is *claimed* and retired on *every* exit
//!   including an unwind. On top of that, [`ZRelocateWorkers`] has **no loop
//!   that waits on the counter at all** — termination is "the claim cursor is
//!   past the end of a fixed page list", a monotone condition no peer can
//!   affect. The G1 failure needs both a leaked count *and* a peer that waits
//!   on it; neither exists here.

use std::collections::HashMap;
use std::panic::AssertUnwindSafe;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;

use parking_lot::{Condvar, Mutex};

use crate::collector::StopTheWorldToken;
use crate::heap::HEADER_SIZE;
use crate::zgc::barrier::is_bare_offset;
use crate::zgc::forwarding::{
    ZForwardingInsertError, ZForwardingPayload, ZForwardingRegistry, ZForwardingTable,
    ZFWD_MAX_PAYLOAD,
};
use crate::zgc::page::{ZPageAllocator, ZPageReal, ZPageState, ZPAGE_OBJECT_GRID};
use crate::zgc::vaddr::Z_OFFSET_MASK;

// The forwarding table packs a 41-bit `to` address and this module converts
// freely between `usize` (the page API) and `u64` (the forwarding API). Both
// assume a 64-bit target; on a 32-bit one the `as` casts would be lossy in the
// other direction and the page table's granule shift arithmetic would not hold
// either.
const _: () = assert!(
    std::mem::size_of::<usize>() == 8,
    "ZGC relocation assumes 64-bit addresses (forwarding entries pack 41 address bits)"
);

/// Smallest byte count this module will accept as an object.
///
/// A live object is always at least a header. A reported size below this means
/// a corrupt header or a walk that lost sync, and the response is to **stop
/// walking the page** — never to advance the cursor by the bogus amount and
/// never to advance it by zero. `gc/src/gc.rs`'s `object_total_size` returns
/// `0` as its corruption sentinel for exactly this handshake.
pub const ZRELOCATE_MIN_OBJECT_BYTES: usize = HEADER_SIZE;

/// Object alignment used for every to-space allocation. Matches
/// [`ZPAGE_OBJECT_GRID`] and the forwarding entry's 8-byte granularity.
pub const ZRELOCATE_ALIGN: usize = ZPAGE_OBJECT_GRID;

/// Added to every heap-relative destination before it is packed into a
/// forwarding entry, and subtracted on the way out.
///
/// `ZgcForwardingEntry::pack` rejects `to == 0` (a decoded occupied entry must
/// never yield a null destination), and the very first byte of the heap is a
/// perfectly legal relocation target whose heap-relative offset *is* zero.
/// Biasing by one object-grid unit keeps encoded destinations strictly
/// positive without costing a bit. It also preserves 8-alignment, which `pack`
/// requires.
pub const ZRELOCATE_ENCODING_BIAS: u64 = ZRELOCATE_ALIGN as u64;

/// Round an object size up onto the 8-byte object grid, which is the stride a
/// linear page walk must take. `ZPageReal::alloc` folds the same rounding into
/// each allocation's footprint, so the rounded size is exactly the distance to
/// the next object base.
#[inline]
fn round_to_grid(bytes: usize) -> usize {
    (bytes + (ZRELOCATE_ALIGN - 1)) & !(ZRELOCATE_ALIGN - 1)
}

// ===========================================================================
// Errors
// ===========================================================================

/// Why one object, or one page's drain, could not be relocated.
///
/// Every variant is *returned*, never panicked. A relocation failure must not
/// take the process down: the page stays quarantined, the cycle is reported as
/// failed, and the VM keeps running on a heap that is merely un-compacted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ZRelocateError {
    /// The page in the relocation set has no table in the registry. A driver
    /// bug: [`ZForwardingRegistry::install_for_set`] must run before any
    /// worker starts.
    NoForwardingTable { page_id: u64 },
    /// `from` is not inside the page it was presented with.
    NotInPage { from: u64, page_id: u64 },
    /// [`ZRelocateContext::object_size`] reported a size below
    /// [`ZRELOCATE_MIN_OBJECT_BYTES`] — a corrupt header, or a walk that lost
    /// object sync.
    ImplausibleSize { from: u64, size: usize },
    /// [`ZRelocateContext::alloc_in`] could not find to-space. Evacuation
    /// cannot proceed for this page; unlike G1 there is no self-forwarding
    /// fallback (see the note on [`ZRelocate::relocate_object`]).
    OutOfToSpace { from: u64, bytes: usize },
    /// The forwarding table refused the entry.
    Forwarding {
        from: u64,
        to: u64,
        cause: ZForwardingInsertError,
    },
    /// A to-space address could not be expressed relative to the encoding base
    /// — the destination lies *below* the heap the base was taken from, which
    /// means [`ZRelocateContext::alloc_in`] allocated somewhere this relocator
    /// does not model. See [`ZRelocate::encode_to`].
    UnencodableDestination { to: u64, base: u64 },
    /// A to-space address encoded to a value the forwarding entry's `to` field
    /// cannot hold — the destination lies *above* what the encoding base plus
    /// [`ZFWD_MAX_PAYLOAD`] can reach.
    ///
    /// # Why this is not `Forwarding { cause: Unencodable }`
    ///
    /// *Added 2026-08-07.* [`ZRelocate::encode_to`] used to bound-check only the
    /// lower end (`absolute < base`). A destination above the modelled heap
    /// therefore produced a legal-looking but enormous payload, which
    /// `ZgcForwardingEntry::pack` then rejected, which surfaced as
    /// [`ZForwardingInsertError::Unencodable`] — **blaming the forwarding table
    /// for a bad `alloc_in` or a bad encoding base.** The table is innocent: it
    /// was handed a payload outside its field and said so. Naming the real cause
    /// here costs one comparison on a path that is already about to fail.
    DestinationAboveEncodingRange {
        /// The absolute to-space address.
        to: u64,
        /// The encoding base in force.
        base: u64,
        /// What the encoding produced.
        encoded: u64,
        /// The largest payload a forwarding entry can hold.
        max_payload: u64,
    },
    /// An address / heap-offset conversion was asked for a value outside the
    /// page allocator's reservation. Produced by the offset-domain entry points
    /// ([`ZRelocate::forward_offset`], [`ZRelocate::forward_lookup_offset`]);
    /// see the module header's "Address domain" section.
    NotInHeap {
        /// The offending value, in whichever domain it was presented.
        value: u64,
        /// The allocator's base address.
        heap_base: u64,
        /// One past the allocator's last byte.
        heap_end: u64,
    },
    /// [`ZRelocateConfig::to_encoding_base`] cannot express this heap.
    ///
    /// Returned only by [`ZRelocate::try_new`]. The overwhelmingly common cause
    /// is `Some(0)` — "store absolute addresses" — over a heap Linux placed near
    /// `0x7f…`, which is ~8x past what the 41-bit `to` field can hold, so every
    /// `try_insert` would answer `Unencodable`. See the module header.
    EncodingBaseUnusable {
        /// The base the configuration asked for.
        requested: u64,
        /// The allocator's base address.
        heap_base: u64,
        /// One past the allocator's last byte.
        heap_end: u64,
        /// The largest payload a forwarding entry can hold.
        max_payload: u64,
    },
}

impl std::fmt::Display for ZRelocateError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ZRelocateError::NoForwardingTable { page_id } => write!(
                f,
                "zrelocate: page {page_id} is in the relocation set but has no forwarding table"
            ),
            ZRelocateError::NotInPage { from, page_id } => write!(
                f,
                "zrelocate: address {from:#x} is not inside page {page_id}"
            ),
            ZRelocateError::ImplausibleSize { from, size } => write!(
                f,
                "zrelocate: object at {from:#x} reports an implausible size of {size} B \
                 (minimum {ZRELOCATE_MIN_OBJECT_BYTES} B)"
            ),
            ZRelocateError::OutOfToSpace { from, bytes } => write!(
                f,
                "zrelocate: no to-space for the {bytes} B object at {from:#x}"
            ),
            ZRelocateError::Forwarding { from, to, cause } => write!(
                f,
                "zrelocate: could not publish {from:#x} -> {to:#x}: {cause}"
            ),
            ZRelocateError::UnencodableDestination { to, base } => write!(
                f,
                "zrelocate: to-space address {to:#x} is below the encoding base {base:#x} — \
                 alloc_in returned memory outside the modelled heap"
            ),
            ZRelocateError::DestinationAboveEncodingRange {
                to,
                base,
                encoded,
                max_payload,
            } => write!(
                f,
                "zrelocate: to-space address {to:#x} encodes (base {base:#x}) to {encoded:#x}, \
                 which exceeds the {max_payload:#x} a forwarding entry can hold — the \
                 destination is above the modelled heap, or the encoding base is wrong. \
                 This is NOT a forwarding-table defect"
            ),
            ZRelocateError::NotInHeap {
                value,
                heap_base,
                heap_end,
            } => write!(
                f,
                "zrelocate: {value:#x} is outside the page allocator's reservation \
                 [{heap_base:#x}, {heap_end:#x}) and has no address/offset translation"
            ),
            ZRelocateError::EncodingBaseUnusable {
                requested,
                heap_base,
                heap_end,
                max_payload,
            } => write!(
                f,
                "zrelocate: to_encoding_base {requested:#x} cannot express the heap \
                 [{heap_base:#x}, {heap_end:#x}) within the {max_payload:#x} a forwarding \
                 entry's `to` field holds — every try_insert would answer Unencodable. \
                 `Some(0)` is only correct once ZGC reserves its own LOW virtual address \
                 space; over a Vec<u8> heap Linux mmaps near 0x7f… it is a guaranteed failure"
            ),
        }
    }
}

impl std::error::Error for ZRelocateError {}

// ===========================================================================
// ZRelocateContext — the seam to the rest of the collector
// ===========================================================================

/// Everything relocation needs from the surrounding collector that this module
/// deliberately does not own.
///
/// The three required methods are the whole contract; the defaulted ones exist
/// so a test impl (and the STW-first landing) can ignore machinery that is
/// still in flight.
///
/// # Why a trait and not a concrete type
///
/// The mark bitmap, the load barrier, the metrics sink, the remembered set and
/// the generational policy are all being written in parallel with this module.
/// A direct dependency on any of them would either block this file or bake in a
/// guessed signature. A trait with a test impl lets the copy protocol be
/// written, reviewed and tested *now*, and lets the wiring step be a single
/// `impl ZRelocateContext for ...` next to the real collector.
///
/// # Thread safety
///
/// `Send + Sync` is a supertrait: [`ZRelocateWorkers`] shares one context
/// across N relocation threads and the load barrier calls into it from
/// arbitrary mutator threads. Every method takes `&self`; an implementation
/// that needs interior mutability must provide its own (atomics or
/// `parking_lot`), and must not hold a lock across a call back into this
/// module.
pub trait ZRelocateContext: Send + Sync {
    /// Total bytes occupied by the object based at `addr`, header included.
    ///
    /// The production impl is `gc::object_total_size(&*(addr as *const
    /// ObjectHeader))`. It must return a value **below**
    /// [`ZRELOCATE_MIN_OBJECT_BYTES`] (conventionally `0`) for a header it
    /// believes is corrupt — that is the signal that stops the page walk
    /// instead of striding into garbage.
    fn object_size(&self, addr: u64) -> usize;

    /// Allocate `bytes` of to-space at `align`, returning the base address.
    ///
    /// `gen_hint` is the source page's generational age
    /// ([`ZPageReal::age`], saturated into a `u8`), for the generational-ZGC
    /// (JEP 439) policy that decides young-vs-old placement. An implementation
    /// that is not generational ignores it.
    ///
    /// `None` means to-space is exhausted. It must **not** be reported by
    /// allocating out of the relocation set's own pages — a destination inside
    /// a from-space page would be evacuated again and could alias its source.
    fn alloc_in(&self, gen_hint: u8, bytes: usize, align: usize) -> Option<u64>;

    /// A distinct object has been relocated `from -> to`, exactly once per
    /// object per cycle: this fires only for the thread that performed the copy
    /// **and won the publishing CAS**.
    ///
    /// Feeds the metrics sink. It is deliberately *not* how the pointer map is
    /// built — [`ZRelocationRecord`] records every resolution including the
    /// ones that adopted another thread's winner, because a root pointing at a
    /// from-space address must be remappable whether or not *this* thread did
    /// the copying (that was G1's DEFECT-2: a fast-path hit that skipped the
    /// record left a root stranded on a from-space object).
    fn on_relocated(&self, from: u64, to: u64);

    /// Is the object at `addr` live according to this cycle's mark?
    ///
    /// Default `true` — every object on the page is copied. That is correct but
    /// wasteful: the whole point of choosing sparse pages is that most of their
    /// bytes are garbage. The real impl consults the mark bitmap (`mark.rs`,
    /// in flight). Wiring it is a pure win and changes nothing else here.
    fn is_live(&self, addr: u64) -> bool {
        let _ = addr;
        true
    }

    /// A speculative copy at `to` lost the publishing CAS and has been
    /// abandoned. Accounting only — see [`ZRelocate::relocate_object`] for why
    /// the bytes are not, and cannot be, freed.
    fn on_abandoned(&self, to: u64, bytes: usize) {
        let _ = (to, bytes);
    }

    /// Snapshot the one word of the object that the raw byte copy must not be
    /// allowed to move: [`cratonvm_types::ObjectHeader`]'s `mark_word`.
    ///
    /// Taken **before** the copy, and re-applied by [`Self::post_copy_apply`]
    /// **after** it. This pair exists because a `copy_nonoverlapping` across an
    /// `AtomicU64` is UB, and because the mark word is the one field a
    /// concurrent mutator can be changing under us (thin lock, inflated monitor
    /// pointer, identity hash, and since the 24 -> 16 shrink the
    /// `kind`/`element_type`/`gc_age`/`gc_flags` quartet). `SharedEvac::evacuate`
    /// in `gc/src/g1.rs` does exactly this, and its comment is worth repeating:
    /// re-*reading* the word after the copy instead of using the pre-copy
    /// snapshot can pick up a racing writer's value.
    ///
    /// Default `0` / no-op: this module knows nothing about `ObjectHeader` on
    /// purpose, and the test impl uses synthetic objects with no atomic fields.
    fn pre_copy_snapshot(&self, from: u64) -> u64 {
        let _ = from;
        0
    }

    /// Re-apply [`Self::pre_copy_snapshot`]'s value to the destination with an
    /// atomic store. See that method for why.
    fn post_copy_apply(&self, to: u64, snapshot: u64) {
        let _ = (to, snapshot);
    }
}

// ===========================================================================
// Counters
// ===========================================================================

/// Per-worker / per-cycle relocation counts.
///
/// Plain `usize`s rather than atomics: each worker accumulates into its own
/// copy with no sharing at all, and the copies are summed once at the end. The
/// shared [`ZRelocateStats`] mirror is updated on the same flush.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ZRelocateCounts {
    /// Objects this participant copied **and** whose publish it won. Equals the
    /// number of distinct objects moved.
    pub objects_relocated: usize,
    /// Resolutions that found an existing entry and copied nothing.
    pub objects_adopted: usize,
    /// Copies made and then abandoned after losing the publishing CAS.
    pub objects_abandoned: usize,
    /// Bytes of winning copies.
    pub bytes_copied: usize,
    /// Bytes of abandoned copies. To-space garbage; see
    /// [`ZRelocate::relocate_object`].
    pub bytes_abandoned: usize,
}

impl ZRelocateCounts {
    /// Fold `other` into `self`.
    pub fn add(&mut self, other: &ZRelocateCounts) {
        self.objects_relocated += other.objects_relocated;
        self.objects_adopted += other.objects_adopted;
        self.objects_abandoned += other.objects_abandoned;
        self.bytes_copied += other.bytes_copied;
        self.bytes_abandoned += other.bytes_abandoned;
    }

    /// Total resolutions served, whichever arm they took.
    pub fn resolutions(&self) -> usize {
        self.objects_relocated + self.objects_adopted + self.objects_abandoned
    }
}

/// Shared, cycle-wide counters. All `Relaxed`: nothing is published through
/// them, they exist for logging and for tests to assert on. Publication of the
/// *objects* is done by the forwarding table's release-CAS, not by these.
#[derive(Debug, Default)]
pub struct ZRelocateStats {
    objects_relocated: AtomicUsize,
    objects_adopted: AtomicUsize,
    objects_abandoned: AtomicUsize,
    bytes_copied: AtomicUsize,
    bytes_abandoned: AtomicUsize,
    alloc_failures: AtomicUsize,
    publish_failures: AtomicUsize,
}

impl ZRelocateStats {
    /// A zeroed counter set.
    pub fn new() -> Self {
        Self::default()
    }

    fn add(&self, c: &ZRelocateCounts) {
        self.objects_relocated
            .fetch_add(c.objects_relocated, Ordering::Relaxed);
        self.objects_adopted
            .fetch_add(c.objects_adopted, Ordering::Relaxed);
        self.objects_abandoned
            .fetch_add(c.objects_abandoned, Ordering::Relaxed);
        self.bytes_copied
            .fetch_add(c.bytes_copied, Ordering::Relaxed);
        self.bytes_abandoned
            .fetch_add(c.bytes_abandoned, Ordering::Relaxed);
    }

    /// Read the counters back as a plain struct.
    pub fn snapshot(&self) -> ZRelocateCounts {
        ZRelocateCounts {
            objects_relocated: self.objects_relocated.load(Ordering::Relaxed),
            objects_adopted: self.objects_adopted.load(Ordering::Relaxed),
            objects_abandoned: self.objects_abandoned.load(Ordering::Relaxed),
            bytes_copied: self.bytes_copied.load(Ordering::Relaxed),
            bytes_abandoned: self.bytes_abandoned.load(Ordering::Relaxed),
        }
    }

    /// To-space allocation failures seen this cycle.
    pub fn alloc_failures(&self) -> usize {
        self.alloc_failures.load(Ordering::Relaxed)
    }

    /// Forwarding-table publish failures seen this cycle.
    pub fn publish_failures(&self) -> usize {
        self.publish_failures.load(Ordering::Relaxed)
    }
}

// ===========================================================================
// The relocation record (pointer_map)
// ===========================================================================

/// Every `from -> to` this cycle resolved, in the exact shape
/// [`crate::gc::GcResult::pointer_map`] wants.
///
/// # Why `std::collections::HashMap` and not `FxHashMap`
///
/// Because `GcResult::pointer_map` is `HashMap<usize, usize>` and
/// `MonitorCleanup::remap_after_gc` takes `&HashMap<usize, usize>`. Building an
/// `FxHashMap` here would force a full rebuild at the boundary, which for a
/// multi-million-entry map is the expensive half of the whole operation.
///
/// # Size, and what to do about it
///
/// One entry per relocated object. At the default 64 MiB evacuation budget and
/// a 32-byte average object that is ~2 Mi entries; `std::collections::HashMap`
/// costs roughly 48 B/entry at its default load factor, so the *map* would be
/// ~100 MiB to describe a 64 MiB copy. That is not acceptable as a steady-state
/// cost, and pre-reserving (which [`ZRelocateConfig::pointer_map_reserve`]
/// does) makes it cheaper to build but not smaller.
///
/// The map exists for exactly one purpose: fixing up address-keyed tables that
/// live *outside* the heap — the monitor table, the cas-lock table, JNI/JVMTI
/// handles, class statics, the interned string pool. Those tables are orders of
/// magnitude smaller than the set of relocated objects. **The right fix is to
/// invert the API**: instead of the collector handing every external table a
/// map of every move, each external table walks its own keys and asks the
/// forwarding registry (`registry.get(page_id)?.find(offset)` —
/// [`ZRelocate::forward_lookup`] wraps it) whether that one address moved. That
/// is `O(external entries)` instead of `O(relocated objects)`, needs no map at
/// all, and reuses machinery that already exists for the load barrier. The
/// interim compromise implemented here:
///
/// * the record is **optional** ([`ZRelocateConfig::record_pointer_map`]),
///   defaulting **on** for the STW path (where it is the only fix-up channel
///   and the relocation set is bounded by the pause budget) and **off** for the
///   concurrent path (where the barrier does the fix-up lazily and the map
///   would be pure overhead);
/// * it pre-reserves from the relocation set's live bytes so building it is one
///   allocation rather than a rehash cascade.
/// The from→to table as the **rewrite pass** wants to read it: sorted, owned,
/// and answered without a lock or a hash.
///
/// [`ZRelocationRecord`] is a `Mutex<HashMap<_, _>>`, which is the right shape
/// for accumulating from several threads and the wrong one for the pass that
/// consumes it. That pass visits every reference slot of every survivor and
/// asks "did this target move?" — overwhelmingly the answer is no, and paying
/// a lock acquire plus a SipHash for each no is the compaction pause. The
/// stop-the-world slide has a single writer and knows every pair before the
/// first question is asked, so it can hand the pass this instead:
///
/// * a **range check** first. Every entry lies in `[lo, hi]`, so a slot
///   pointing anywhere else — the unmoved part of the heap, the large-object
///   end, anything off-heap — is rejected by two compares and never searched.
/// * a **binary search** for the rest, over one contiguous `Vec` rather than a
///   hash table's scattered buckets.
///
/// It also answers `contains_from`, which is what the two side structures the
/// slide built alongside the record (`moved_to`, a second `FxHashMap`, and
/// `moved_from`, an `FxHashSet`) existed for. One table serves all three
/// readers now.
#[derive(Debug, Default, Clone)]
pub struct ZForwardIndex {
    /// Ascending by `from`, and never holding an identity entry.
    entries: Vec<(usize, usize)>,
    lo: usize,
    hi: usize,
}

impl ZForwardIndex {
    /// Build from the slide's pair list. `pairs` need not be sorted: the low
    /// slide produces them ascending and the high pack descending, so the
    /// concatenation of the two is neither.
    pub fn from_pairs(pairs: &[(usize, usize)]) -> Self {
        let mut entries: Vec<(usize, usize)> = pairs
            .iter()
            .copied()
            .filter(|(from, to)| from != to)
            .collect();
        entries.sort_unstable_by_key(|(from, _)| *from);
        // A `from` recorded twice would make the search's answer depend on
        // which duplicate it landed on. The slide moves each object once, so
        // this is a contradiction rather than a case to handle.
        debug_assert!(
            entries.windows(2).all(|w| w[0].0 != w[1].0),
            "one address forwarded to two destinations",
        );
        let lo = entries.first().map_or(usize::MAX, |(from, _)| *from);
        let hi = entries.last().map_or(0, |(from, _)| *from);
        Self { entries, lo, hi }
    }

    /// Where `from` moved to, or `None` if it did not move.
    #[inline]
    pub fn get(&self, from: usize) -> Option<usize> {
        if from < self.lo || from > self.hi {
            return None;
        }
        self.entries
            .binary_search_by_key(&from, |(f, _)| *f)
            .ok()
            .map(|i| self.entries[i].1)
    }

    /// Whether `from` is an address the slide vacated.
    #[inline]
    pub fn contains_from(&self, from: usize) -> bool {
        self.get(from).is_some()
    }

    /// `from` if it did not move, its destination if it did.
    #[inline]
    pub fn resolve(&self, from: usize) -> usize {
        self.get(from).unwrap_or(from)
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// The pairs, ascending by `from`.
    pub fn entries(&self) -> &[(usize, usize)] {
        &self.entries
    }
}

#[derive(Debug)]
pub struct ZRelocationRecord {
    entries: Mutex<HashMap<usize, usize>>,
    enabled: bool,
}

impl ZRelocationRecord {
    /// A record that will accumulate entries, pre-reserving `reserve` of them.
    pub fn new(enabled: bool, reserve: usize) -> Self {
        let map = if enabled {
            HashMap::with_capacity(reserve.min(1 << 22))
        } else {
            HashMap::new()
        };
        Self {
            entries: Mutex::new(map),
            enabled,
        }
    }

    /// A disabled record. Every `record_*` call is a no-op and the map stays
    /// empty — which is the *truthful* empty map only when nothing moved, so a
    /// caller that disables the record must not then hand the result to
    /// `remap_after_gc` as though it were complete.
    pub fn disabled() -> Self {
        Self::new(false, 0)
    }

    /// Whether entries are being accumulated.
    pub fn is_enabled(&self) -> bool {
        self.enabled
    }

    /// Record a batch of `(from, to)` pairs.
    ///
    /// Duplicates are harmless and expected: two threads that resolve the same
    /// object both record `(from, winner)`, and the second insert overwrites
    /// the first with an identical value. Recording on *every* arm — including
    /// the adopt arm that copied nothing — is deliberate; see
    /// [`ZRelocateContext::on_relocated`] for the G1 defect that omitting it
    /// reproduces.
    pub fn record_many(&self, pairs: &[(usize, usize)]) {
        if !self.enabled || pairs.is_empty() {
            return;
        }
        let mut guard = self.entries.lock();
        for &(from, to) in pairs.iter() {
            guard.insert(from, to);
        }
        drop(guard);
    }

    /// Number of distinct from-addresses recorded.
    pub fn len(&self) -> usize {
        let guard = self.entries.lock();
        let n = guard.len();
        drop(guard);
        n
    }

    /// True if nothing has been recorded.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The destination recorded for `from`, if any.
    pub fn get(&self, from: usize) -> Option<usize> {
        let guard = self.entries.lock();
        let found = guard.get(&from).copied();
        drop(guard);
        found
    }

    /// A copy of the accumulated map.
    pub fn snapshot_pointer_map(&self) -> HashMap<usize, usize> {
        let guard = self.entries.lock();
        let out = guard.clone();
        drop(guard);
        out
    }

    /// Take the accumulated map, leaving the record empty.
    ///
    /// # The wiring checklist for `ZgcRealHeap::collect_garbage`
    ///
    /// Today that method builds `HashMap::new()` and documents it as "exactly
    /// correct for a non-compacting collector". Once relocation runs, three
    /// things must change together — doing any one alone is worse than doing
    /// none:
    ///
    /// 1. `let pointer_map = record.into_pointer_map();` instead of
    ///    `HashMap::new()`, and it must be the map from *this* cycle.
    /// 2. `monitors.remap_after_gc(&pointer_map)` keeps its call site but stops
    ///    being a no-op, so the monitor/cas-lock tables get re-keyed. Its
    ///    early-return-on-empty is what silently hid the gap.
    /// 3. `monitors.prune_dead(&dead)` must have every **relocated** address
    ///    removed from `dead` first. A relocated object's old address is
    ///    neither unchanged nor dead-and-gone; pruning it drops a live object's
    ///    monitor, and a later `synchronized` on that object then deadlocks
    ///    against inherited state. The `dead` list is built by the sweep, which
    ///    does not know about relocation.
    ///
    /// The same map must also reach the VM crate's remap of class statics,
    /// JNI/JVMTI handles and the interned string pool — `collect_garbage`
    /// returns it, but only the callers that already consume
    /// `GcResult::pointer_map` will see it.
    pub fn into_pointer_map(&self) -> HashMap<usize, usize> {
        let mut guard = self.entries.lock();
        let out = std::mem::take(&mut *guard);
        drop(guard);
        out
    }
}

// ===========================================================================
// Lazy remap state machine
// ===========================================================================

/// Where one from-space page sits on the road from "selected" to "reusable".
///
/// See invariant ZR-1 in the module header; this enum is that invariant made
/// executable.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(u8)]
pub enum ZPageRemapState {
    /// In the relocation set, table installed, nothing copied yet.
    Selected = 0,
    /// At least one object copied out; the page's drain has not finished.
    Relocating = 1,
    /// Every live object has a published forwarding entry. The page's *bytes*
    /// are dead. The page is **quarantined**: its table is still installed, so
    /// its address range must not be recycled (ZR-1).
    Relocated = 2,
    /// A full heap + root traversal has since completed with the table
    /// installed, so no stale reference to this page survives anywhere. The
    /// table may be uninstalled and the page recycled.
    Remapped = 3,
    /// Handed back to [`ZPageAllocator`]. Terminal.
    Recycled = 4,
}

/// One page's remap bookkeeping.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ZPageRemapEntry {
    /// The page.
    pub page_id: u64,
    /// The cycle in which the page was selected for relocation.
    pub cycle: u64,
    /// Where it is on the ZR-1 staircase.
    pub state: ZPageRemapState,
    /// Live objects the mark attributed to the page (sizing/diagnostics).
    pub live_objects: usize,
    /// Objects actually copied out.
    pub relocated_objects: usize,
}

#[derive(Debug, Default)]
struct ZRemapInner {
    cycle: u64,
    pages: HashMap<u64, ZPageRemapEntry>,
}

/// The lifetime authority for from-space pages and their forwarding tables.
///
/// Nothing in this module frees a page. `ZRemapState` decides *when it becomes
/// legal to*, and the driver asks. Keeping the decision in one small,
/// heavily-tested state machine is the only defence against ZR-1 being
/// re-derived incorrectly at each of the several call sites that would
/// otherwise want to call `free_page`.
///
/// # Locking
///
/// One `parking_lot::Mutex` around the whole map. Every method takes it,
/// mutates, and drops it before returning — no method calls out to anything
/// while holding it, and no method acquires a second lock. That is the same
/// discipline `ZForwardingRegistry` documents, and for the same scar: a fair
/// `RwLock` held across a nested acquire parked three threads forever in
/// `native-io/src/net.rs`.
#[derive(Debug)]
pub struct ZRemapState {
    inner: Mutex<ZRemapInner>,
}

impl Default for ZRemapState {
    fn default() -> Self {
        Self::new()
    }
}

impl ZRemapState {
    /// An empty state machine at cycle 0.
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(ZRemapInner::default()),
        }
    }

    /// Start a new GC cycle and return its number. Cycle numbers are what make
    /// [`Self::note_mark_complete`]'s "strictly older" rule expressible.
    pub fn begin_cycle(&self) -> u64 {
        let mut g = self.inner.lock();
        g.cycle += 1;
        let c = g.cycle;
        drop(g);
        c
    }

    /// The current cycle number.
    pub fn current_cycle(&self) -> u64 {
        let g = self.inner.lock();
        let c = g.cycle;
        drop(g);
        c
    }

    /// Register a page as selected for relocation in the current cycle.
    ///
    /// Re-selecting a page that is still quarantined from an earlier cycle is
    /// refused (returns `false`): its old table would have to be replaced while
    /// stale references still resolve through it, which is ZR-1's failure mode
    /// with extra steps.
    pub fn select_page(&self, page_id: u64, live_objects: usize) -> bool {
        let mut g = self.inner.lock();
        let cycle = g.cycle;
        // Copy the conflicting entry's fields out before touching `g` again:
        // holding a borrow of the map across the `drop(g)` below is the shape
        // that makes a guard outlive its data.
        let conflict: Option<(ZPageRemapState, u64)> = g
            .pages
            .get(&page_id)
            .filter(|e| e.state != ZPageRemapState::Recycled)
            .map(|e| (e.state, e.cycle));
        if let Some((state, old_cycle)) = conflict {
            drop(g);
            tracing::error!(
                target: "zgc",
                page_id = page_id,
                state = ?state,
                selected_in_cycle = old_cycle,
                current_cycle = cycle,
                "ZRemapState::select_page: refusing to re-select a page that is still \
                 quarantined from an earlier cycle (invariant ZR-1)"
            );
            return false;
        }
        g.pages.insert(
            page_id,
            ZPageRemapEntry {
                page_id,
                cycle,
                state: ZPageRemapState::Selected,
                live_objects,
                relocated_objects: 0,
            },
        );
        drop(g);
        true
    }

    /// Credit `n` copied objects to a page, moving `Selected -> Relocating`.
    pub fn note_relocated_objects(&self, page_id: u64, n: usize) {
        let mut g = self.inner.lock();
        if let Some(e) = g.pages.get_mut(&page_id) {
            e.relocated_objects += n;
            if e.state == ZPageRemapState::Selected {
                e.state = ZPageRemapState::Relocating;
            }
        }
        drop(g);
    }

    /// The page's drain walked `[base, top)` to completion and every live
    /// object has a published forwarding entry: `-> Relocated`.
    ///
    /// **This does not make the page reusable.** It makes its *bytes* dead. See
    /// ZR-1(c). Returns `false` if the page was not in a pre-`Relocated` state.
    pub fn mark_page_relocated(&self, page_id: u64) -> bool {
        let mut g = self.inner.lock();
        let ok = match g.pages.get_mut(&page_id) {
            Some(e)
                if e.state == ZPageRemapState::Selected
                    || e.state == ZPageRemapState::Relocating =>
            {
                e.state = ZPageRemapState::Relocated;
                true
            }
            _ => false,
        };
        drop(g);
        ok
    }

    /// The mark of cycle `marked_cycle` has traversed **every** reference in
    /// the heap and in every root set, with all forwarding tables installed
    /// throughout. Promote the pages it has provably finished remapping.
    ///
    /// Returns the page ids that just reached [`ZPageRemapState::Remapped`] —
    /// i.e. the ids whose tables may now be uninstalled
    /// ([`ZForwardingRegistry::clear`] or a per-page removal) and whose address
    /// ranges may now be handed back to [`ZPageAllocator`].
    ///
    /// # Why `entry.cycle < marked_cycle`, strictly
    ///
    /// A page relocated *during* cycle *N* is being copied out while cycle
    /// *N*'s own mark is running or already finished. References created or
    /// merely not-yet-loaded during that window are not guaranteed to have been
    /// walked by cycle *N*'s mark. Only the *next* full traversal — cycle
    /// *N+1*'s mark — is known to have started after every one of cycle *N*'s
    /// forwarding entries was published, and therefore to have seen and
    /// rewritten every stale reference. Using `<=` here would uninstall a table
    /// one whole cycle early, and the reference it strands would not fault: it
    /// would find no table, conclude "this page is not being relocated, the
    /// object did not move", and hand back a pointer into recycled memory.
    /// Silent, not loud. This off-by-one is the single easiest way to
    /// reintroduce a use-after-free here.
    pub fn note_mark_complete(&self, marked_cycle: u64) -> Vec<u64> {
        let mut g = self.inner.lock();
        let mut promoted: Vec<u64> = Vec::new();
        for e in g.pages.values_mut() {
            if e.state == ZPageRemapState::Relocated && e.cycle < marked_cycle {
                e.state = ZPageRemapState::Remapped;
                promoted.push(e.page_id);
            }
        }
        drop(g);
        promoted.sort_unstable();
        if !promoted.is_empty() {
            tracing::debug!(
                target: "zgc",
                marked_cycle = marked_cycle,
                pages = promoted.len(),
                "ZRemapState: lazy remap complete; from-space pages leaving quarantine"
            );
        }
        promoted
    }

    /// The STW shortcut: the driver rewrote **every** reference to `cycle`'s
    /// from-space pages before resuming the world, so there is no stale
    /// reference left to mis-forward and the quarantine can be skipped
    /// entirely.
    ///
    /// Note the `<=` where [`Self::note_mark_complete`] has `<`: that is not an
    /// inconsistency, it is the whole difference between the two paths. Under
    /// STW the fix-up is *eager and complete within the pause*, so the current
    /// cycle's own pages qualify. Calling this without having actually rewritten
    /// every reference — including roots, class statics, JNI/JVMTI handles, the
    /// monitor table and the interned string pool — is a use-after-free.
    pub fn note_stw_remap_complete(&self, cycle: u64) -> Vec<u64> {
        let mut g = self.inner.lock();
        let mut promoted: Vec<u64> = Vec::new();
        for e in g.pages.values_mut() {
            if e.state == ZPageRemapState::Relocated && e.cycle <= cycle {
                e.state = ZPageRemapState::Remapped;
                promoted.push(e.page_id);
            }
        }
        drop(g);
        promoted.sort_unstable();
        promoted
    }

    /// May this page's address range be returned to the allocator?
    ///
    /// The single question ZR-1 answers, and the only one a driver should ask
    /// before calling `ZPageAllocator::free_page`.
    pub fn may_recycle(&self, page_id: u64) -> bool {
        let g = self.inner.lock();
        let ok = g
            .pages
            .get(&page_id)
            .map(|e| e.state == ZPageRemapState::Remapped)
            .unwrap_or(false);
        drop(g);
        ok
    }

    /// Record that a page has been handed back. `Remapped -> Recycled`.
    pub fn note_recycled(&self, page_id: u64) -> bool {
        let mut g = self.inner.lock();
        let ok = match g.pages.get_mut(&page_id) {
            Some(e) if e.state == ZPageRemapState::Remapped => {
                e.state = ZPageRemapState::Recycled;
                true
            }
            _ => false,
        };
        drop(g);
        ok
    }

    /// This page's current state, if it is tracked.
    pub fn state_of(&self, page_id: u64) -> Option<ZPageRemapState> {
        let g = self.inner.lock();
        let s = g.pages.get(&page_id).map(|e| e.state);
        drop(g);
        s
    }

    /// A copy of one page's bookkeeping.
    pub fn entry(&self, page_id: u64) -> Option<ZPageRemapEntry> {
        let g = self.inner.lock();
        let e = g.pages.get(&page_id).cloned();
        drop(g);
        e
    }

    /// Pages whose bytes are dead but whose tables must stay installed —
    /// the quarantine set. Its size is the memory ZGC's laziness costs.
    pub fn quarantined_pages(&self) -> Vec<u64> {
        let g = self.inner.lock();
        let mut out: Vec<u64> = g
            .pages
            .values()
            .filter(|e| e.state == ZPageRemapState::Relocated)
            .map(|e| e.page_id)
            .collect();
        drop(g);
        out.sort_unstable();
        out
    }

    /// Every tracked page id, sorted.
    pub fn tracked_pages(&self) -> Vec<u64> {
        let g = self.inner.lock();
        let mut out: Vec<u64> = g.pages.keys().copied().collect();
        drop(g);
        out.sort_unstable();
        out
    }

    /// Forget every `Recycled` entry. Pure bookkeeping hygiene; safe at any
    /// time because a `Recycled` page is no longer anybody's from-space.
    pub fn prune_recycled(&self) -> usize {
        let mut g = self.inner.lock();
        let before = g.pages.len();
        g.pages.retain(|_, e| e.state != ZPageRemapState::Recycled);
        let removed = before - g.pages.len();
        drop(g);
        removed
    }
}

// ===========================================================================
// Safepoint / yield
// ===========================================================================

/// The relocation pool's yield point.
///
/// # Why this exists at all
///
/// A GC worker that does not notice a safepoint request reads as a **hang**,
/// and this tree has a documented history of exactly that: young-GC trigger
/// livelocks, STW hangs diagnosed only with the watchdog, and the 3h08m G1
/// evacuation wedge. A relocation worker holds no lock a mutator needs, so it
/// cannot deadlock anybody — but it can keep a whole VM waiting for a pause it
/// is not participating in.
///
/// # Protocol
///
/// * Each worker calls [`Self::join`] on entry, holding the returned membership
///   for its lifetime. The membership decrements the active count on drop,
///   **including on an unwind**, so a worker that dies cannot leave the driver
///   waiting for it to park.
/// * Workers call [`Self::poll`] between objects. The fast path is one
///   `Acquire` load of a `bool`; against a >100 ns object copy that is free, and
///   polling per-object rather than per-page is what makes the yield *prompt* —
///   a 32 MiB medium page can hold a million objects, so a per-page poll is a
///   per-page-long pause extension.
/// * The driver calls [`Self::request`], then [`Self::wait_until_all_parked`],
///   does its safepoint work, then [`Self::release`].
pub struct ZRelocateSafepoint {
    /// Fast-path flag. `Release` on the write side, `Acquire` on the read side:
    /// a worker that observes the request must also observe everything the
    /// requester published before making it.
    requested: AtomicBool,
    counts: Mutex<ZSafepointCounts>,
    cv: Condvar,
}

#[derive(Debug, Default)]
struct ZSafepointCounts {
    active: usize,
    parked: usize,
}

impl Default for ZRelocateSafepoint {
    fn default() -> Self {
        Self::new()
    }
}

impl ZRelocateSafepoint {
    /// A safepoint with no request outstanding and no members.
    pub fn new() -> Self {
        Self {
            requested: AtomicBool::new(false),
            counts: Mutex::new(ZSafepointCounts::default()),
            cv: Condvar::new(),
        }
    }

    /// Register the calling thread as a participant.
    pub fn join(&self) -> ZSafepointMembership<'_> {
        let mut g = self.counts.lock();
        g.active += 1;
        drop(g);
        self.cv.notify_all();
        ZSafepointMembership { sp: self }
    }

    /// Is a yield outstanding? One `Acquire` load.
    #[inline]
    pub fn is_requested(&self) -> bool {
        self.requested.load(Ordering::Acquire)
    }

    /// Members currently registered.
    pub fn active(&self) -> usize {
        let g = self.counts.lock();
        let n = g.active;
        drop(g);
        n
    }

    /// Members currently parked inside [`Self::poll`].
    pub fn parked(&self) -> usize {
        let g = self.counts.lock();
        let n = g.parked;
        drop(g);
        n
    }

    /// Ask every member to park at the next object boundary.
    pub fn request(&self) {
        // The lock is taken around the store so a worker cannot evaluate
        // `requested` and enter `Condvar::wait` in the window between the store
        // and a notification — the classic missed-wakeup.
        let g = self.counts.lock();
        self.requested.store(true, Ordering::Release);
        drop(g);
        self.cv.notify_all();
    }

    /// Let parked members continue.
    pub fn release(&self) {
        let g = self.counts.lock();
        self.requested.store(false, Ordering::Release);
        drop(g);
        self.cv.notify_all();
    }

    /// Block until every *active* member is parked.
    ///
    /// Waiting on `parked >= active` rather than on a fixed count is what makes
    /// this safe against a worker that panicked: its membership guard already
    /// decremented `active` on the way out, so the driver stops waiting for a
    /// thread that no longer exists. Waiting for a constant would be the G1
    /// hang shape in a different costume.
    pub fn wait_until_all_parked(&self) {
        let mut g = self.counts.lock();
        while g.active > 0 && g.parked < g.active {
            self.cv.wait(&mut g);
        }
        drop(g);
    }

    /// Worker-side poll. Returns immediately unless a yield is outstanding.
    pub fn poll(&self, _membership: &ZSafepointMembership<'_>) {
        if !self.is_requested() {
            return;
        }
        let mut g = self.counts.lock();
        g.parked += 1;
        self.cv.notify_all();
        while self.requested.load(Ordering::Acquire) {
            self.cv.wait(&mut g);
        }
        g.parked -= 1;
        drop(g);
        self.cv.notify_all();
    }
}

/// Hand-written rather than derived: `parking_lot::Condvar`'s `Debug` impl is
/// not something this file should depend on, and printing a live count is more
/// useful than printing the primitives.
impl std::fmt::Debug for ZRelocateSafepoint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let g = self.counts.lock();
        let (active, parked) = (g.active, g.parked);
        drop(g);
        f.debug_struct("ZRelocateSafepoint")
            .field("requested", &self.requested.load(Ordering::Relaxed))
            .field("active", &active)
            .field("parked", &parked)
            .finish()
    }
}

/// RAII membership in a [`ZRelocateSafepoint`]. Decrements the active count on
/// drop, including on an unwind.
pub struct ZSafepointMembership<'a> {
    sp: &'a ZRelocateSafepoint,
}

impl std::fmt::Debug for ZSafepointMembership<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ZSafepointMembership")
    }
}

impl Drop for ZSafepointMembership<'_> {
    fn drop(&mut self) {
        let mut g = self.sp.counts.lock();
        g.active = g.active.saturating_sub(1);
        drop(g);
        self.sp.cv.notify_all();
    }
}

// ===========================================================================
// The outstanding-count guard
// ===========================================================================

/// Retires one unit of a termination counter on drop, **including on an
/// unwind**.
///
/// This is the direct descendant of `RetireOnExit` in `gc/src/g1.rs`, which
/// exists because a bare `fetch_sub` after a fallible call is skipped by a
/// panic and the resulting leaked count turned a crash into an unkillable
/// 3h08m hang that also hid its own cause.
///
/// `AcqRel`: the release half publishes everything the worker did with the page
/// to whoever observes the count reach zero; the acquire half keeps the RMW
/// symmetric with the rest of the crate's convention.
#[derive(Debug)]
pub struct ZOutstandingGuard<'a> {
    counter: &'a AtomicUsize,
}

impl<'a> ZOutstandingGuard<'a> {
    /// Take a unit of `counter` (the caller must already have counted it in).
    pub fn new(counter: &'a AtomicUsize) -> Self {
        Self { counter }
    }
}

impl Drop for ZOutstandingGuard<'_> {
    fn drop(&mut self) {
        self.counter.fetch_sub(1, Ordering::AcqRel);
    }
}

// ===========================================================================
// ZRelocate — the copy protocol
// ===========================================================================

/// Tunables for one relocation cycle.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ZRelocateConfig {
    /// Accumulate the `from -> to` record for `GcResult::pointer_map`.
    ///
    /// Default `true`, because the STW path is the one that lands first and it
    /// has no other fix-up channel. The concurrent path should turn it off (the
    /// barrier fixes references lazily) — see [`ZRelocationRecord`] on size.
    pub record_pointer_map: bool,
    /// Entries to pre-reserve in the record.
    pub pointer_map_reserve: usize,
    /// Base that destination addresses are expressed relative to before being
    /// packed into a forwarding entry. `None` (the default) means "the page
    /// allocator's heap base"; `Some(0)` means "store absolute addresses".
    ///
    /// # Why this knob has to exist — a real cross-module mismatch
    ///
    /// `ZgcForwardingEntry` gives the `to` field **41 bits at 8-byte
    /// granularity = 16 TiB**, and `forwarding.rs` justifies that as covering
    /// "the entire ZGC address space with two bits of headroom", because ZGC's
    /// colored-pointer offset field is 42 bits (4 TiB). That reasoning is
    /// correct *for a ZGC virtual-address-space offset*. It does not hold for a
    /// **raw process address**, and [`ZPageAllocator`] hands out raw process
    /// addresses: its heap is one `Vec<u8>`, and on Linux an allocation that
    /// size comes from `mmap`, which places it around `0x7f…` — roughly 2^47,
    /// i.e. **eight times past the 16 TiB ceiling**. `pack` would reject every
    /// entry with `Unencodable`. Windows heap addresses happen to sit low
    /// enough to work by luck, which is exactly the kind of platform-dependent
    /// near-miss that ships green and fails on the build host.
    ///
    /// Expressing destinations relative to the heap base fixes it for good: the
    /// heap is at most `max_capacity` bytes, so the encoded value is bounded by
    /// the heap size rather than by where the OS happened to put it. The cost
    /// is that a raw [`ZForwardingTable::find`] now returns an **encoded**
    /// value — see [`ZRelocate::decode_to`] for who is allowed to read one.
    ///
    /// Set this to `Some(0)` once ZGC reserves its own low virtual address
    /// space (the `vaddr.rs` `ZVirtualAddressSpace` shape), at which point
    /// absolute and encoded coincide and the knob can be deleted.
    ///
    /// # `Some(0)` is checked, not trusted (added 2026-08-07)
    ///
    /// "Once" is doing a great deal of work in that sentence, and the failure
    /// for setting it *early* is total: over today's `Vec<u8>` heap, Linux
    /// returns a base near `0x7f…`, absolute encoding puts that straight into a
    /// 41-bit field, and **every** `try_insert` answers
    /// [`ZForwardingInsertError::Unencodable`] — while on Windows, where the
    /// heap sits low, the same configuration works perfectly.
    ///
    /// [`ZForwardingTable`] cannot defend against this: it never learns the heap
    /// base, deliberately (`forwarding.rs` says so in its module header — the
    /// encoding belongs to whoever chose the base). **This module knows the
    /// base, so this module checks it.** [`ZRelocate::try_new`] returns
    /// [`ZRelocateError::EncodingBaseUnusable`]; [`ZRelocate::new`] logs at
    /// `error!` (release builds included), trips a `debug_assert!`, and falls
    /// back to the allocator base.
    pub to_encoding_base: Option<u64>,
}

impl Default for ZRelocateConfig {
    fn default() -> Self {
        Self {
            record_pointer_map: true,
            pointer_map_reserve: 0,
            to_encoding_base: None,
        }
    }
}

/// What one resolution produced.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ZRelocateOutcome {
    /// **The winner's address.** Not necessarily the copy this call made — see
    /// [`ZRelocate::relocate_object`]. Every caller must use this and nothing
    /// else.
    pub to: u64,
    /// True iff this call performed the copy *and* won the publishing CAS, i.e.
    /// iff this call is the one that moved the object.
    pub fresh: bool,
    /// Bytes copied by this call: the object size when a copy was made
    /// (winning or abandoned), `0` when an existing entry was adopted.
    pub bytes: usize,
}

/// Per-participant scratch: forwarding pairs and counters that are flushed to
/// the shared record and stats in one batch.
///
/// Batching is not just a throughput trick. It keeps the record's mutex off the
/// per-object path entirely, which is what stops relocation from serialising on
/// a single lock the moment more than one worker exists.
#[derive(Debug, Default)]
pub struct ZRelocateLocal {
    forwards: Vec<(usize, usize)>,
    counts: ZRelocateCounts,
}

impl ZRelocateLocal {
    /// An empty scratch buffer.
    pub fn new() -> Self {
        Self::default()
    }

    /// This participant's counters so far.
    pub fn counts(&self) -> &ZRelocateCounts {
        &self.counts
    }

    /// Pairs not yet flushed.
    pub fn pending(&self) -> usize {
        self.forwards.len()
    }
}

/// The object copy protocol.
///
/// Owns nothing about *policy* (which pages, which order, how many threads) and
/// everything about *mechanism* (find, copy, publish, converge). One instance
/// per relocation cycle, shared by every worker and by the load barrier.
pub struct ZRelocate {
    ctx: Arc<dyn ZRelocateContext>,
    allocator: Arc<ZPageAllocator>,
    registry: Arc<ZForwardingRegistry>,
    record: ZRelocationRecord,
    stats: ZRelocateStats,
    config: ZRelocateConfig,
    /// Resolved [`ZRelocateConfig::to_encoding_base`]. `0` means absolute.
    to_encoding_base: u64,
    /// The page allocator's base address, cached at construction.
    ///
    /// **Not the same thing as [`Self::to_encoding_base`], and the two must not
    /// be conflated.** `to_encoding_base` is the *forwarding-payload* encoding
    /// base and may legitimately be `0`. `heap_base` is the origin of the
    /// **barrier's offset domain** — `offset = address - heap_base` — and is
    /// always the real reservation base. Using `to_encoding_base` for an offset
    /// conversion would produce a correct-looking offset only in the default
    /// configuration and garbage in the absolute one.
    heap_base: u64,
    /// One past the last byte of the page allocator's reservation.
    heap_end: u64,
}

/// Hand-written because the context is a trait object with no `Debug` bound,
/// and because a derived impl would format the [`ZPageAllocator`] — which takes
/// its state mutex, so `{:?}` on a relocator from inside a locked region would
/// self-deadlock (`parking_lot::Mutex` is not reentrant).
impl std::fmt::Debug for ZRelocate {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let counts = self.stats.snapshot();
        f.debug_struct("ZRelocate")
            .field("relocated", &counts.objects_relocated)
            .field("adopted", &counts.objects_adopted)
            .field("abandoned", &counts.objects_abandoned)
            .field("bytes_copied", &counts.bytes_copied)
            .field("recording_pointer_map", &self.record.is_enabled())
            .finish()
    }
}

impl ZRelocate {
    /// Build a relocator for one cycle.
    ///
    /// An unusable [`ZRelocateConfig::to_encoding_base`] is **not** silently
    /// accepted: it is logged at `error!` (in release builds too), trips a
    /// `debug_assert!`, and is replaced by the allocator base, which always
    /// works. Use [`Self::try_new`] when a hard refusal is wanted instead.
    pub fn new(
        ctx: Arc<dyn ZRelocateContext>,
        allocator: Arc<ZPageAllocator>,
        registry: Arc<ZForwardingRegistry>,
        config: ZRelocateConfig,
    ) -> Self {
        let heap_base = allocator.base() as u64;
        let heap_end = allocator.end() as u64;
        let requested = config.to_encoding_base.unwrap_or(heap_base);
        let to_encoding_base = match Self::check_encoding_base(requested, heap_base, heap_end) {
            Ok(()) => requested,
            Err(e) => {
                // `error!`, not `debug!`: the whole hazard is that this
                // configuration works on the Windows dev host (heap placed low)
                // and fails on every Linux build host (heap near 0x7f…). A
                // diagnostic that only exists in debug builds would only exist
                // where the bug is invisible — the same argument `barrier.rs`
                // makes for `is_bare_offset`.
                //
                // `reason` is materialised as a `&str` rather than passed with
                // tracing's `%` sigil, which consumes its argument in some
                // versions of the macro and would leave nothing for the
                // `debug_assert!` below.
                let reason: String = e.to_string();
                tracing::error!(
                    target: "zgc",
                    requested = requested,
                    heap_base = heap_base,
                    heap_end = heap_end,
                    max_payload = ZFWD_MAX_PAYLOAD,
                    reason = reason.as_str(),
                    "ZRelocate::new: refusing the configured to_encoding_base and falling \
                     back to the allocator base; with the configured value every \
                     forwarding insert would fail as Unencodable"
                );
                debug_assert!(
                    false,
                    "ZRelocateConfig::to_encoding_base cannot express this heap: {reason}"
                );
                heap_base
            }
        };
        Self::build(
            ctx,
            allocator,
            registry,
            config,
            to_encoding_base,
            heap_base,
            heap_end,
        )
    }

    /// Build a relocator, **refusing** an encoding base that cannot express this
    /// heap instead of falling back.
    ///
    /// The check is exactly the one `forwarding.rs` cannot make. A
    /// [`ZForwardingTable`] never learns the heap base — that is a deliberate
    /// independence, stated in its module header — so it can only report
    /// [`ZForwardingInsertError::Unencodable`] *per object, after the copy has
    /// already been made*, and it cannot say why. Here the whole answer is
    /// available before the first object moves:
    ///
    /// * `Some(0)` (absolute) needs `heap_end <= ZFWD_MAX_PAYLOAD` — i.e. a heap
    ///   entirely below 16 TiB. Linux `mmap` puts a large `Vec<u8>` near `0x7f…`
    ///   ≈ 2^47, which is ~8x past that. **This is the case the sibling review
    ///   asked for and it is why `Some(0)` is a footgun today.**
    /// * `Some(b)` with `b > heap_base` cannot encode the bottom of the heap at
    ///   all — every destination there fails `checked_sub`.
    /// * `Some(b)` at or below the base needs
    ///   `heap_end - b + ZRELOCATE_ENCODING_BIAS <= ZFWD_MAX_PAYLOAD`.
    /// * `None` resolves to the allocator base, whose worst case is
    ///   `max_capacity + 8` and is therefore fine for any heap the allocator
    ///   would accept.
    pub fn try_new(
        ctx: Arc<dyn ZRelocateContext>,
        allocator: Arc<ZPageAllocator>,
        registry: Arc<ZForwardingRegistry>,
        config: ZRelocateConfig,
    ) -> Result<Self, ZRelocateError> {
        let heap_base = allocator.base() as u64;
        let heap_end = allocator.end() as u64;
        let requested = config.to_encoding_base.unwrap_or(heap_base);
        Self::check_encoding_base(requested, heap_base, heap_end)?;
        Ok(Self::build(
            ctx, allocator, registry, config, requested, heap_base, heap_end,
        ))
    }

    /// Can `requested` express every address in `[heap_base, heap_end)` inside a
    /// forwarding entry's `to` field? See [`Self::try_new`] for the four cases.
    fn check_encoding_base(
        requested: u64,
        heap_base: u64,
        heap_end: u64,
    ) -> Result<(), ZRelocateError> {
        // The largest value `encode_to` could ever be asked to produce for an
        // address in this heap. `None` means "cannot encode the heap at all".
        let worst_encoded: Option<u64> = if requested == 0 {
            // Absolute: the encoded value IS the address, unbiased.
            Some(heap_end)
        } else if requested > heap_base {
            // The bottom of the heap would not encode at all — `checked_sub`
            // would fail for every destination below `requested`.
            None
        } else {
            heap_end
                .checked_sub(requested)
                .and_then(|d| d.checked_add(ZRELOCATE_ENCODING_BIAS))
        };
        match worst_encoded {
            Some(w) if w <= ZFWD_MAX_PAYLOAD => Ok(()),
            _ => Err(ZRelocateError::EncodingBaseUnusable {
                requested,
                heap_base,
                heap_end,
                max_payload: ZFWD_MAX_PAYLOAD,
            }),
        }
    }

    /// The shared tail of [`Self::new`] and [`Self::try_new`]. `to_encoding_base`
    /// has already been validated (or deliberately overridden) by the caller.
    fn build(
        ctx: Arc<dyn ZRelocateContext>,
        allocator: Arc<ZPageAllocator>,
        registry: Arc<ZForwardingRegistry>,
        config: ZRelocateConfig,
        to_encoding_base: u64,
        heap_base: u64,
        heap_end: u64,
    ) -> Self {
        let record = ZRelocationRecord::new(config.record_pointer_map, config.pointer_map_reserve);
        tracing::debug!(
            target: "zgc",
            heap_base = heap_base,
            heap_end = heap_end,
            to_encoding_base = to_encoding_base,
            record_pointer_map = config.record_pointer_map,
            "ZRelocate: relocator built for this cycle"
        );
        Self {
            ctx,
            allocator,
            registry,
            record,
            stats: ZRelocateStats::new(),
            config,
            to_encoding_base,
            heap_base,
            heap_end,
        }
    }

    /// The base destination addresses are stored relative to (`0` = absolute).
    ///
    /// **This is the forwarding-payload encoding base, not the offset domain's
    /// origin.** For the latter see [`Self::heap_base`].
    pub fn to_encoding_base(&self) -> u64 {
        self.to_encoding_base
    }

    /// The page allocator's base address — the origin of the **offset domain**
    /// the load barrier traffics in (module header, "Address domain").
    ///
    /// Whoever builds a `ZVirtualAddressSpace` for this heap must build it over
    /// the same `[base, end)`, or the barrier's offsets and this module's
    /// offsets will differ by a constant, which is the fixed G1 `address + 3`
    /// defect wearing a different hat.
    pub fn heap_base(&self) -> u64 {
        self.heap_base
    }

    /// One past the last byte of the page allocator's reservation.
    pub fn heap_end(&self) -> u64 {
        self.heap_end
    }

    /// Absolute to-space address -> the value stored in a forwarding entry.
    ///
    /// **The only encoder.** See [`ZRelocateConfig::to_encoding_base`] for why
    /// an encoding exists at all.
    ///
    /// # Both ends are checked (upper bound added 2026-08-07)
    ///
    /// This used to reject only `absolute < base` and return `Option`. A
    /// destination *above* `base + max_capacity` therefore sailed through and
    /// produced a huge payload, which `ZgcForwardingEntry::pack` then refused,
    /// which reached the caller as
    /// [`ZForwardingInsertError::Unencodable`] — **an error that blames the
    /// forwarding table for the relocator's (or the context's) mistake.** The
    /// table did nothing wrong; it was handed a payload outside its field.
    /// Returning a `Result` lets the two ends be told apart at the point where
    /// the cause is still known:
    ///
    /// * below the base ⇒ [`ZRelocateError::UnencodableDestination`]
    /// * above what the `to` field holds ⇒
    ///   [`ZRelocateError::DestinationAboveEncodingRange`]
    ///
    /// Both mean the same practical thing — `ZRelocateContext::alloc_in`
    /// returned memory outside the heap this relocator models, or the encoding
    /// base is wrong — and neither is a forwarding-table defect.
    pub fn encode_to(&self, absolute: u64) -> Result<ZForwardingPayload, ZRelocateError> {
        let encoded: u64 = if self.to_encoding_base == 0 {
            absolute
        } else {
            absolute
                .checked_sub(self.to_encoding_base)
                .and_then(|d| d.checked_add(ZRELOCATE_ENCODING_BIAS))
                .ok_or(ZRelocateError::UnencodableDestination {
                    to: absolute,
                    base: self.to_encoding_base,
                })?
        };
        if encoded > ZFWD_MAX_PAYLOAD {
            return Err(ZRelocateError::DestinationAboveEncodingRange {
                to: absolute,
                base: self.to_encoding_base,
                encoded,
                max_payload: ZFWD_MAX_PAYLOAD,
            });
        }
        Ok(ZForwardingPayload::from_encoded(encoded))
    }

    /// The value stored in a forwarding entry -> the absolute to-space address.
    ///
    /// **The only decoder, and the only sanctioned reader of a slot payload.**
    ///
    /// This is the G1 lesson applied one layer up. `forwarding.rs` already
    /// funnels every arm through `ZgcForwardingEntry::to` so that the CAS
    /// winner and the CAS loser cannot disagree about *bit layout*. The same
    /// hazard exists one level higher for *address bias*: a caller that reads
    /// `ZForwardingTable::find(..)` directly and treats the result as an
    /// address gets a value biased by the heap base, which is a wrong pointer
    /// that looks plausible — precisely the `address + 3` shape that hid for
    /// months in G1 because two arms of one function disagreed about what type
    /// of word they were holding.
    ///
    /// **Rule: outside this module, nothing may call `ZForwardingTable::find`
    /// to obtain an address.** Use [`Self::forward_lookup`] or [`Self::forward`],
    /// which decode.
    ///
    /// **Correction, 2026-08-07:** this doc used to add "the load barrier in
    /// particular must go through them". It must not — those two are
    /// *absolute*-domain and the barrier is *offset*-domain. The barrier's
    /// entry points are [`Self::forward_offset`] and
    /// [`Self::forward_lookup_offset`]; see the module header.
    pub fn decode_to(&self, payload: ZForwardingPayload) -> u64 {
        let encoded = payload.encoded();
        if self.to_encoding_base == 0 {
            return encoded;
        }
        encoded
            .wrapping_add(self.to_encoding_base)
            .wrapping_sub(ZRELOCATE_ENCODING_BIAS)
    }

    /// The cycle's `from -> to` record.
    pub fn record(&self) -> &ZRelocationRecord {
        &self.record
    }

    /// The cycle's counters.
    pub fn stats(&self) -> &ZRelocateStats {
        &self.stats
    }

    /// The forwarding registry this relocator publishes into.
    pub fn registry(&self) -> &Arc<ZForwardingRegistry> {
        &self.registry
    }

    /// The page allocator whose table maps addresses to pages.
    pub fn allocator(&self) -> &Arc<ZPageAllocator> {
        &self.allocator
    }

    /// The configuration in force.
    pub fn config(&self) -> &ZRelocateConfig {
        &self.config
    }

    /// Flush a participant's scratch into the shared record and stats.
    ///
    /// Called at the end of a worker's run, and by [`Self::forward`] for its
    /// single-entry temporary. Also called after a *caught panic*: the pairs a
    /// dying worker had accumulated describe real, already-published table
    /// entries, so dropping them would lose exactly the mappings a stranded
    /// root needs.
    pub fn flush_local(&self, local: &mut ZRelocateLocal) {
        if !local.forwards.is_empty() {
            self.record.record_many(&local.forwards);
            local.forwards.clear();
        }
        self.stats.add(&local.counts);
        local.counts = ZRelocateCounts::default();
    }

    /// Relocate one object out of `page`, converging on a single winner.
    ///
    /// # The protocol
    ///
    /// 1. **Look up.** If the object already has a forwarding entry, adopt it
    ///    and copy nothing.
    /// 2. **Size and allocate.** Ask the context for the object's size, then
    ///    for to-space.
    /// 3. **Copy.** Snapshot the mark word, `copy_nonoverlapping` the payload,
    ///    re-apply the snapshot atomically.
    /// 4. **Publish.** `try_insert` the `(from_offset, my_copy)` pair.
    /// 5. **Take the winner's answer.** `try_insert` returns **the winning
    ///    address**, which is another thread's if we lost the CAS.
    ///
    /// # What "abandon" costs, and why we do not free
    ///
    /// A loser's copy stays allocated and unreferenced. It is to-space garbage,
    /// reclaimed by the *next* cycle at zero extra machinery: a to-space page's
    /// live set is determined by the next mark, and nothing points at the
    /// abandoned copy. It is never freed inline for two independent reasons.
    /// First, a bump-pointer page has no free operation — the cursor only goes
    /// forward, and rewinding it would require proving that no *other*
    /// allocation landed after ours, which under concurrency it cannot.
    /// Second, even if the cursor could be rewound, the loser cannot know
    /// whether some other thread already observed a reference to those bytes.
    /// The waste is bounded by `(racers on one object - 1) x object size`, and
    /// races on a single object are rare because the barrier only fires on a
    /// *stale* load. G1's parallel evacuator makes the identical trade.
    ///
    /// # Why there is no self-forwarding fallback
    ///
    /// G1 responds to to-space exhaustion by CAS-ing an identity forward
    /// (`old -> old`) and keeping the region. That works because G1 is inside a
    /// pause and can decide, at the end of it, to keep the region. Here it
    /// would leave a page half-evacuated with its table installed and its
    /// bytes still live — a state ZR-1 has no rung for, and one in which the
    /// page can never be recycled *and* can never be re-selected. Returning
    /// [`ZRelocateError::OutOfToSpace`] and failing the page loudly is the
    /// honest answer; the driver's response should be to shrink the relocation
    /// set, not to invent a rung.
    ///
    /// # The G1 lesson, structurally
    ///
    /// `my_copy` is dead the instant `try_insert` returns. Every subsequent use
    /// is of `winner`, and the outcome is constructed from `winner` alone. This
    /// is the one place a "the loser used its own value" defect could live, so
    /// it is the one place the variable is named to make that impossible to
    /// write by accident.
    pub fn relocate_object(
        &self,
        page: &ZPageReal,
        table: &ZForwardingTable,
        from: u64,
        gen_hint: u8,
        local: &mut ZRelocateLocal,
    ) -> Result<ZRelocateOutcome, ZRelocateError> {
        let base = page.base() as u64;
        let end = page.end() as u64;
        if from < base || from >= end {
            return Err(ZRelocateError::NotInPage {
                from,
                page_id: page.id(),
            });
        }
        let from_offset = from - base;

        // (1) Already forwarded? The common case once any thread has run.
        //     Note the decode: a slot payload is never an address until it has
        //     been through `decode_to`.
        if let Some(existing) = table.find_payload(from_offset) {
            let existing = self.decode_to(existing);
            local.counts.objects_adopted += 1;
            local.forwards.push((from as usize, existing as usize));
            return Ok(ZRelocateOutcome {
                to: existing,
                fresh: false,
                bytes: 0,
            });
        }

        // (2) Size, then to-space.
        let size = self.ctx.object_size(from);
        if size < ZRELOCATE_MIN_OBJECT_BYTES {
            return Err(ZRelocateError::ImplausibleSize { from, size });
        }
        let my_copy = match self.ctx.alloc_in(gen_hint, size, ZRELOCATE_ALIGN) {
            Some(a) => a,
            None => {
                self.stats.alloc_failures.fetch_add(1, Ordering::Relaxed);
                return Err(ZRelocateError::OutOfToSpace { from, bytes: size });
            }
        };

        // (3) Copy. The snapshot pair brackets the raw copy because the copy
        //     range contains an `AtomicU64` the copy is not allowed to move.
        let snapshot = self.ctx.pre_copy_snapshot(from);
        // SAFETY: `from` lies in `[page.base(), page.end())`, checked above,
        // which is a window into the `ZPageAllocator` reservation that outlives
        // this call and is never resized. `size` came from the context's
        // `object_size` and is at least `ZRELOCATE_MIN_OBJECT_BYTES`; the page
        // walk that produced `from` is bounded by `walk_bounds()`, so
        // `from + size` stays within the page's used extent. `my_copy` was just
        // handed out by `alloc_in` for exactly `size` bytes and is in a
        // different page than `from` (the context must not allocate to-space
        // inside the relocation set — see `alloc_in`'s contract), so the two
        // ranges cannot overlap. Exclusivity of the destination is guaranteed
        // by the bump allocator: no other thread was handed these bytes.
        unsafe {
            std::ptr::copy_nonoverlapping(from as *const u8, my_copy as *mut u8, size);
        }
        self.ctx.post_copy_apply(my_copy, snapshot);

        // (4) Publish. The release half of this CAS is what orders the copy
        //     above before the entry becomes visible; see
        //     `ZForwardingTable::try_insert`'s ordering notes.
        let my_copy_encoded = self.encode_to(my_copy)?;
        let winner_encoded = match table.try_insert(from_offset, my_copy_encoded) {
            Ok(w) => w,
            Err(cause) => {
                self.stats.publish_failures.fetch_add(1, Ordering::Relaxed);
                return Err(ZRelocateError::Forwarding {
                    from,
                    to: my_copy,
                    cause,
                });
            }
        };
        // The single decode. `try_insert` hands back the WINNING payload, which
        // is another thread's when we lost, so this is the exact arm where G1
        // forgot to decode and shipped `address + 3`.
        let winner = self.decode_to(winner_encoded);

        // (5) `my_copy` is dead from here on. Only `winner` may be used.
        let fresh = winner == my_copy;
        if fresh {
            local.counts.objects_relocated += 1;
            local.counts.bytes_copied += size;
            self.ctx.on_relocated(from, winner);
        } else {
            local.counts.objects_abandoned += 1;
            local.counts.bytes_abandoned += size;
            self.ctx.on_abandoned(my_copy, size);
            tracing::trace!(
                target: "zgc",
                page_id = page.id(),
                from = from,
                abandoned = my_copy,
                winner = winner,
                "ZRelocate: lost the publishing CAS; abandoning the speculative copy"
            );
        }
        // Recorded on BOTH arms: a root pointing at `from` must be remappable
        // regardless of which thread did the copying (G1 DEFECT-2).
        local.forwards.push((from as usize, winner as usize));

        Ok(ZRelocateOutcome {
            to: winner,
            fresh,
            bytes: size,
        })
    }

    /// Relocate-or-adopt, in the **absolute machine-address domain**.
    ///
    /// # ⚠ Not the load barrier's entry point — use [`Self::forward_offset`]
    ///
    /// *Corrected 2026-08-07.* This method's docs used to call it "the load
    /// barrier's slow-path entry point". They were wrong, and the way they were
    /// wrong is the dangerous kind: `barrier.rs` decided that
    /// `ZBarrierContext::forward` takes and returns a **42-bit heap offset**, so
    /// a `ZBarrierContext::forward` implemented directly over this method hands
    /// the barrier a machine pointer wearing an offset's clothes. On Linux
    /// (`0x7f…`) those five stray bits get OR'd into the healed word's metadata
    /// field and silently recolour a live reference; on the Windows dev host the
    /// heap sits low and nothing at all happens. That asymmetry is what makes it
    /// worth a named method rather than a comment. See the module header,
    /// "Address domain".
    ///
    /// This absolute form is the right one for the relocator itself, the STW
    /// driver, and the external address-keyed tables (monitor table, cas-lock
    /// table, JNI/JVMTI handles, class statics, interned strings) — all of which
    /// hold real pointers and always did.
    ///
    /// `addr` is a **plain, uncoloured** heap address: stripping the colour
    /// metadata and re-colouring the result are the barrier's job (`vaddr.rs`
    /// owns that encoding, and keeping it out of here means relocation has no
    /// opinion about the pointer format).
    ///
    /// * `Ok(None)` — `addr` is not inside a page under relocation. The object
    ///   did not move; the barrier only has to re-colour and self-heal the slot.
    /// * `Ok(Some(to))` — the object's current address, relocating it now if
    ///   nobody has yet.
    /// * `Err(_)` — relocation was needed and failed. The barrier must **not**
    ///   fall back to `addr`: the page may already be quarantined, and handing
    ///   back a from-space pointer is the use-after-free ZR-1 exists to prevent.
    ///
    /// The `gen_hint` is the from-page's age, so an object promoted out of a
    /// young page lands where the generational policy wants it.
    pub fn forward(&self, addr: u64) -> Result<Option<u64>, ZRelocateError> {
        let page = match self.allocator.page_for(addr as usize) {
            Some(p) => p,
            None => return Ok(None),
        };
        let table = match self.registry.get(page.id()) {
            Some(t) => t,
            None => return Ok(None),
        };
        let gen_hint = page.age().min(u8::MAX as u32) as u8;
        let mut local = ZRelocateLocal::new();
        let result = self.relocate_object(&page, &table, addr, gen_hint, &mut local);
        // Flush on both arms: a failed relocation may still have recorded an
        // adopt from an earlier probe, and a panic-free error path must not
        // silently drop a published mapping.
        self.flush_local(&mut local);
        result.map(|o| Some(o.to))
    }

    /// Resolve `addr` **without** relocating it, in the **absolute
    /// machine-address domain**.
    ///
    /// `None` means either "not in a relocating page" or "in one, but nobody
    /// has copied this object yet" — the two are deliberately not
    /// distinguished, because every caller of this function (external
    /// address-keyed tables doing their own fix-up; diagnostics) treats them
    /// the same way. This is the primitive the `pointer_map`-inversion
    /// described on [`ZRelocationRecord`] would be built from.
    ///
    /// The load barrier wants [`Self::forward_lookup_offset`] instead — see
    /// [`Self::forward`] for why, and the module header for the decision.
    pub fn forward_lookup(&self, addr: u64) -> Option<u64> {
        let page = self.allocator.page_for(addr as usize)?;
        let table = self.registry.get(page.id())?;
        let base = page.base() as u64;
        if addr < base {
            return None;
        }
        table.find_payload(addr - base).map(|e| self.decode_to(e))
    }

    // -- the offset domain: the load barrier's boundary ---------------------

    /// Absolute machine address -> heap offset, or `None` outside the
    /// reservation.
    ///
    /// The offset domain's origin is [`Self::heap_base`] — the page allocator's
    /// base — **not** [`Self::to_encoding_base`], which is a different base that
    /// happens to have the same default value. Mixing them up would be correct
    /// in the default configuration and garbage in the absolute one, which is
    /// the worst available failure mode.
    #[inline]
    pub fn offset_of_address(&self, absolute: u64) -> Option<u64> {
        if absolute < self.heap_base || absolute >= self.heap_end {
            return None;
        }
        Some(absolute - self.heap_base)
    }

    /// Heap offset -> absolute machine address, or `None` outside the
    /// reservation.
    #[inline]
    pub fn address_of_offset(&self, offset: u64) -> Option<u64> {
        let absolute = self.heap_base.checked_add(offset)?;
        if absolute >= self.heap_end {
            return None;
        }
        Some(absolute)
    }

    /// The release-mode tripwire for the offset domain.
    ///
    /// Returns `true` when `offset` is a bare 42-bit heap offset — no colour
    /// bits, no tag, no heap base — using the *same* predicate the barrier's
    /// slow path uses ([`is_bare_offset`] against
    /// [`Z_OFFSET_MASK`](crate::zgc::vaddr::Z_OFFSET_MASK)), so the two modules
    /// cannot drift apart on what "an offset" means.
    ///
    /// # Why this fires in release builds
    ///
    /// Same reason `barrier.rs` gives for its copy, and it is worth restating
    /// because the instinct is to reach for a bare `debug_assert!`: on Linux a
    /// value in the wrong domain is `0x7f…`, five bits past the offset field,
    /// and those bits land in the healed word's metadata; on Windows the heap is
    /// placed low, the same mistake produces a plausible word, and there is no
    /// symptom. A debug-only check therefore runs *only on the platform where
    /// the bug is invisible*. This tree has been bitten by exactly that shape
    /// before (`release-test-runs-had-lock-order-enforcement-off`).
    ///
    /// The `tracing::error!` is the part that survives `--release`; the
    /// `debug_assert!` turns it into a test failure where asserts are on.
    fn check_offset_domain(&self, what: &'static str, offset: u64) -> bool {
        if is_bare_offset(offset, Z_OFFSET_MASK) {
            return true;
        }
        let stray_bits = offset & !Z_OFFSET_MASK;
        tracing::error!(
            target: "zgc",
            method = what,
            value = offset,
            stray_bits = stray_bits,
            heap_base = self.heap_base,
            offset_mask = Z_OFFSET_MASK,
            "ZRelocate: a value in the barrier's OFFSET domain carries bits above the \
             42-bit offset field — it is a machine pointer (or is still coloured). \
             Handing this to the load barrier would corrupt the metadata field of \
             every slot it heals. See the module header, \"Address domain\""
        );
        debug_assert!(
            is_bare_offset(offset, Z_OFFSET_MASK),
            "ZRelocate::{what}: {offset:#x} is not a bare heap offset \
             (stray bits {stray_bits:#x}) — the load barrier's domain is a 42-bit \
             offset from heap_base, not a machine address"
        );
        false
    }

    /// **The load barrier's slow-path entry point.** Relocate-or-adopt, in the
    /// **heap-offset domain**.
    ///
    /// `offset` and the returned value are offsets from [`Self::heap_base`],
    /// exactly as `ZBarrierContext::forward` requires (decided 2026-08-07; the
    /// derivation is in `barrier.rs`'s module header and is summarised in this
    /// module's). This is the adapter that header says "does not exist yet in
    /// `src`; whoever writes it owns this" — it exists now, and it lives here
    /// rather than in the eventual `ZgcRealHeap` impl because *this* is the
    /// module that owns both the decoder and the heap base, and because an
    /// adapter written once cannot be re-derived incorrectly at the next call
    /// site.
    ///
    /// * `Ok(None)` — the object did not move. The barrier's `forward` is the
    ///   identity for such an offset; **it must return the input, not `0`.**
    ///   Offset `0` is a real heap location, so `0` is a legal answer only when
    ///   the input was `0`.
    /// * `Ok(Some(to))` — the object's current offset.
    /// * `Err(_)` — the offset is outside the reservation, or relocation was
    ///   needed and failed. The barrier must **not** fall back to `offset`: the
    ///   from-space page may already be quarantined, and handing back a
    ///   from-space reference is the use-after-free ZR-1 exists to prevent.
    ///
    /// The sanctioned `ZBarrierContext::forward` body over a `ZRelocate` is
    /// therefore:
    ///
    /// ```text
    ///     fn forward(&self, addr: u64) -> Option<u64> {
    ///         match self.relocate.forward_offset(addr) {
    ///             Ok(Some(to)) => Some(to),
    ///             Ok(None)     => Some(addr),  // identity: it did not move
    ///             Err(e)       => { tracing::error!(?e, "..."); None }
    ///         }
    ///     }
    /// ```
    ///
    /// Corrected 2026-08-07. This block used to end `Err(e) => { ..; addr }` —
    /// the very fallback the paragraph above forbids, ten lines below the
    /// prohibition. It was the only body expressible at the time, because
    /// `ZBarrierContext::forward` returned a bare `u64` with no error channel.
    /// It now returns `Option<u64>`, and `None` routes to
    /// `ZBarrierContext::on_forward_failure`, whose `!` return type makes
    /// "log it and carry on with the stale address" unwritable rather than
    /// merely discouraged.
    ///
    /// Both answers are checked by `check_offset_domain` before they leave, in
    /// release builds as well as debug.
    pub fn forward_offset(&self, offset: u64) -> Result<Option<u64>, ZRelocateError> {
        if !self.check_offset_domain("forward_offset", offset) {
            return Err(ZRelocateError::NotInHeap {
                value: offset,
                heap_base: self.heap_base,
                heap_end: self.heap_end,
            });
        }
        let absolute = self
            .address_of_offset(offset)
            .ok_or(ZRelocateError::NotInHeap {
                value: offset,
                heap_base: self.heap_base,
                heap_end: self.heap_end,
            })?;
        let to_absolute = match self.forward(absolute)? {
            Some(a) => a,
            None => return Ok(None),
        };
        let to_offset = self
            .offset_of_address(to_absolute)
            .ok_or(ZRelocateError::NotInHeap {
                value: to_absolute,
                heap_base: self.heap_base,
                heap_end: self.heap_end,
            })?;
        // Belt and braces: `offset_of_address` already bounded this by
        // `heap_end`, but a reservation is not required to be smaller than
        // 2^42, and the barrier's field is.
        if !self.check_offset_domain("forward_offset", to_offset) {
            return Err(ZRelocateError::NotInHeap {
                value: to_absolute,
                heap_base: self.heap_base,
                heap_end: self.heap_end,
            });
        }
        Ok(Some(to_offset))
    }

    /// Resolve `offset` **without** relocating it, in the **heap-offset
    /// domain**. The non-relocating twin of [`Self::forward_offset`].
    ///
    /// `None` means "did not move, or has not moved yet, or is not in the heap"
    /// — the same deliberate conflation [`Self::forward_lookup`] makes, for the
    /// same reason.
    pub fn forward_lookup_offset(&self, offset: u64) -> Option<u64> {
        if !self.check_offset_domain("forward_lookup_offset", offset) {
            return None;
        }
        let absolute = self.address_of_offset(offset)?;
        let to_absolute = self.forward_lookup(absolute)?;
        let to_offset = self.offset_of_address(to_absolute)?;
        if !self.check_offset_domain("forward_lookup_offset", to_offset) {
            return None;
        }
        Some(to_offset)
    }

    /// Walk one page and relocate every live object in it.
    ///
    /// The walk is **page-driven and `top`-bounded**, per the contract
    /// `ZPageAllocator` documents: `[base, base + top)` is the only range that
    /// decodes as objects, everything above `top` is untouched reservation, and
    /// this tree writes no `HumongousFiller` sentinels in ZGC pages (G1's
    /// filler was found unscreened at 24 of 26 walker call sites; not having one
    /// is strictly better than screening for one).
    ///
    /// # Why the walk can never spin
    ///
    /// The cursor advances by `round_to_grid(object_size)`, which is at least
    /// [`ZRELOCATE_MIN_OBJECT_BYTES`] because a smaller report aborts the walk
    /// instead. There is also a hard iteration cap derived from the page size.
    /// A zero-stride walk is the classic GC hang in this codebase and it is
    /// worth two independent guards.
    pub fn drain_page(
        &self,
        work: &ZRelocatePage,
        local: &mut ZRelocateLocal,
        safepoint: &ZRelocateSafepoint,
        membership: &ZSafepointMembership<'_>,
    ) -> Result<ZPageDrainOutcome, ZRelocateError> {
        let page = &work.page;
        let table = self
            .registry
            .get(page.id())
            .ok_or(ZRelocateError::NoForwardingTable { page_id: page.id() })?;

        let (start, end) = page.walk_bounds();
        let cap = (page.size() / ZRELOCATE_MIN_OBJECT_BYTES).saturating_add(1);
        let mut addr = start;
        let mut visited: usize = 0;
        let mut outcome = ZPageDrainOutcome {
            page_id: page.id(),
            objects_seen: 0,
            objects_relocated: 0,
            bytes_copied: 0,
            walk_aborted: false,
        };

        while addr < end {
            // Prompt yield: one Acquire bool load per object.
            safepoint.poll(membership);

            if visited >= cap {
                tracing::error!(
                    target: "zgc",
                    page_id = page.id(),
                    visited = visited,
                    cap = cap,
                    "ZRelocate::drain_page: iteration cap hit — the object walk lost sync; \
                     aborting this page rather than spinning"
                );
                outcome.walk_aborted = true;
                break;
            }
            visited += 1;

            let size = self.ctx.object_size(addr as u64);
            if size < ZRELOCATE_MIN_OBJECT_BYTES {
                tracing::error!(
                    target: "zgc",
                    page_id = page.id(),
                    addr = addr,
                    size = size,
                    min = ZRELOCATE_MIN_OBJECT_BYTES,
                    "ZRelocate::drain_page: implausible object size — aborting the walk of \
                     this page rather than striding by it (a stride below the minimum, and \
                     in particular the 0 that gc::object_total_size returns for a corrupt \
                     header, would not terminate)"
                );
                outcome.walk_aborted = true;
                break;
            }
            let step = round_to_grid(size);
            outcome.objects_seen += 1;

            if self.ctx.is_live(addr as u64) {
                let r = self.relocate_object(page, &table, addr as u64, work.gen_hint, local)?;
                if r.fresh {
                    outcome.objects_relocated += 1;
                    outcome.bytes_copied += r.bytes;
                }
            }

            addr = match addr.checked_add(step) {
                Some(a) => a,
                None => {
                    outcome.walk_aborted = true;
                    break;
                }
            };
        }

        Ok(outcome)
    }

    /// **The path that lands first.** Relocate the whole set with every mutator
    /// parked.
    ///
    /// Identical mechanism to [`ZRelocateWorkers::relocate_set`] — same copy,
    /// same forwarding tables, same record — minus every hazard that only
    /// exists when a mutator is running:
    ///
    /// * No object can be locked, written or read mid-copy, so the mark-word
    ///   snapshot is trivially stable and the raw byte copy is not a data race.
    /// * No thread holds a from-space reference the barrier has not seen, so
    ///   the thread-stack handshake this tree does not have is not needed.
    /// * The driver rewrites every reference before resuming, so the quarantine
    ///   in ZR-1 collapses: call [`ZRemapState::note_stw_remap_complete`] after
    ///   the fix-up and pages become recyclable inside the same pause.
    ///
    /// The `_stw` token is type-level proof the caller parked the world; see
    /// [`StopTheWorldToken`]. It is not otherwise used, which is the point —
    /// it makes "you may only call this inside a pause" a compile error rather
    /// than a comment.
    ///
    /// Single-threaded on the calling thread: a pause has no safepoint to
    /// yield at, and parallelising an STW phase is a throughput question to
    /// answer after correctness, not before.
    pub fn relocate_stw(
        &self,
        _stw: &StopTheWorldToken,
        pages: &[ZRelocatePage],
        remap: &ZRemapState,
    ) -> ZRelocateRunResult {
        let safepoint = ZRelocateSafepoint::new();
        let membership = safepoint.join();
        let outstanding = AtomicUsize::new(pages.len());
        let mut result = ZRelocateRunResult::new(pages.len());
        let mut local = ZRelocateLocal::new();

        for work in pages.iter() {
            let page_id = work.page.id();
            let _retire = ZOutstandingGuard::new(&outstanding);
            match self.drain_page(work, &mut local, &safepoint, &membership) {
                Ok(drain) => {
                    remap.note_relocated_objects(page_id, drain.objects_relocated);
                    result.objects_seen += drain.objects_seen;
                    if drain.walk_aborted {
                        result.aborted_walk_page_ids.push(page_id);
                    } else if remap.mark_page_relocated(page_id) {
                        result.pages_completed += 1;
                    } else {
                        result.failed_page_ids.push(page_id);
                    }
                }
                Err(e) => {
                    tracing::error!(
                        target: "zgc",
                        page_id = page_id,
                        error = %e,
                        "ZRelocate::relocate_stw: page drain failed; page stays quarantined"
                    );
                    result.failed_page_ids.push(page_id);
                }
            }
        }

        result.counts = local.counts.clone();
        self.flush_local(&mut local);
        result.outstanding_at_exit = outstanding.load(Ordering::Acquire);
        drop(membership);
        result.log("relocate_stw");
        result
    }
}

// ===========================================================================
// Work items and results
// ===========================================================================

/// One page of relocation work.
#[derive(Debug, Clone)]
pub struct ZRelocatePage {
    /// The from-space page.
    pub page: Arc<ZPageReal>,
    /// Generational placement hint handed to
    /// [`ZRelocateContext::alloc_in`]; the page's age, saturated.
    pub gen_hint: u8,
}

impl ZRelocatePage {
    /// Work item for `page`, taking the hint from the page's own age.
    pub fn new(page: Arc<ZPageReal>) -> Self {
        let gen_hint = page.age().min(u8::MAX as u32) as u8;
        Self { page, gen_hint }
    }

    /// Work item with an explicit hint.
    pub fn with_gen_hint(page: Arc<ZPageReal>, gen_hint: u8) -> Self {
        Self { page, gen_hint }
    }

    /// Is this page in the state a relocation set member should be in?
    ///
    /// Advisory: a page still `Allocating` is taking mutator allocations and
    /// must not be evacuated, and `Free` is nobody's from-space.
    pub fn is_claimed(&self) -> bool {
        self.page.state() == ZPageState::InRelocationSet
    }
}

/// What one page's drain produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ZPageDrainOutcome {
    /// The page.
    pub page_id: u64,
    /// Objects the walk decoded, live or not.
    pub objects_seen: usize,
    /// Objects this drain copied and won.
    pub objects_relocated: usize,
    /// Bytes of winning copies.
    pub bytes_copied: usize,
    /// The walk stopped early on an implausible size or the iteration cap. The
    /// page is **not** fully evacuated and must not be marked `Relocated`.
    pub walk_aborted: bool,
}

/// The outcome of a whole relocation run.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ZRelocateRunResult {
    /// Pages handed to the run.
    pub pages_attempted: usize,
    /// Pages fully evacuated and moved to [`ZPageRemapState::Relocated`].
    pub pages_completed: usize,
    /// Objects decoded across every page.
    pub objects_seen: usize,
    /// Summed participant counters.
    pub counts: ZRelocateCounts,
    /// Pages whose drain returned an error or panicked. **Still quarantined**
    /// and not evacuated.
    pub failed_page_ids: Vec<u64>,
    /// Pages whose object walk lost sync. Same status as a failure, reported
    /// separately because the cause is header corruption rather than resource
    /// exhaustion.
    pub aborted_walk_page_ids: Vec<u64>,
    /// The termination counter after every worker exited. **Must be zero.** A
    /// non-zero value here is the leaked-count shape that hung G1 for three
    /// hours, and it is asserted in the tests rather than merely logged.
    pub outstanding_at_exit: usize,
    /// Worker threads that unwound. Non-zero means the cycle failed loudly,
    /// which is the intended outcome — see [`ZRelocateWorkers`].
    pub worker_panics: usize,
}

impl ZRelocateRunResult {
    fn new(pages_attempted: usize) -> Self {
        Self {
            pages_attempted,
            ..Default::default()
        }
    }

    /// Did every page complete, with no panic and no leaked count?
    pub fn is_clean(&self) -> bool {
        self.pages_completed == self.pages_attempted
            && self.failed_page_ids.is_empty()
            && self.aborted_walk_page_ids.is_empty()
            && self.outstanding_at_exit == 0
            && self.worker_panics == 0
    }

    fn merge_worker(&mut self, w: ZWorkerTotals) {
        self.pages_completed += w.pages_completed;
        self.objects_seen += w.objects_seen;
        self.counts.add(&w.counts);
        self.failed_page_ids.extend(w.failed_page_ids);
        self.aborted_walk_page_ids.extend(w.aborted_walk_page_ids);
        self.worker_panics += w.panics;
    }

    fn log(&self, what: &str) {
        if self.is_clean() {
            tracing::debug!(
                target: "zgc",
                path = what,
                pages = self.pages_attempted,
                relocated = self.counts.objects_relocated,
                adopted = self.counts.objects_adopted,
                abandoned = self.counts.objects_abandoned,
                bytes = self.counts.bytes_copied,
                "ZRelocate: relocation complete"
            );
        } else {
            tracing::error!(
                target: "zgc",
                path = what,
                pages = self.pages_attempted,
                completed = self.pages_completed,
                failed = self.failed_page_ids.len(),
                aborted = self.aborted_walk_page_ids.len(),
                panics = self.worker_panics,
                outstanding = self.outstanding_at_exit,
                "ZRelocate: relocation did NOT complete cleanly — the affected pages stay \
                 quarantined and must not be recycled"
            );
        }
    }
}

#[derive(Debug, Default)]
struct ZWorkerTotals {
    pages_completed: usize,
    objects_seen: usize,
    counts: ZRelocateCounts,
    failed_page_ids: Vec<u64>,
    aborted_walk_page_ids: Vec<u64>,
    panics: usize,
}

// ===========================================================================
// ZRelocateWorkers — the concurrent pool
// ===========================================================================

/// N workers draining a fixed relocation set.
///
/// # Why this cannot hang, structurally
///
/// The G1 evacuation wedge needed two ingredients: a worker that skipped its
/// `outstanding.fetch_sub(1)`, and peers that *waited* on `outstanding` to
/// reach zero. Neither exists here.
///
/// * **Nothing waits on the counter.** Termination is `cursor.fetch_add >=
///   pages.len()` over a fixed, immutable page list. That is monotone, no peer
///   can affect it, and it is reached even if every other worker dies. The
///   `outstanding` counter is an *observability* device — it is asserted to be
///   zero at exit precisely so a future refactor that reintroduces a wait finds
///   a red test instead of a three-hour hang.
/// * **The counter cannot leak anyway.** [`ZOutstandingGuard`] retires it from
///   `Drop`, so an unwind out of a page drain retires it on the way past.
/// * **A panic is caught per page, not per pool.** A worker that panics on one
///   page records the page as failed and moves to the next, so one corrupt
///   header cannot abandon the rest of the set. The failure is *reported*, the
///   page is left un-evacuated and quarantined, and the driver must treat the
///   cycle as failed. That is the "fail loudly rather than wedge" behaviour the
///   G1 post-mortem asked for.
/// * **The safepoint's driver waits on `parked >= active`, not on a constant.**
///   A dead worker's membership guard already decremented `active`, so it
///   cannot be waited for.
#[derive(Debug)]
pub struct ZRelocateWorkers {
    worker_count: usize,
    safepoint: Arc<ZRelocateSafepoint>,
}

impl ZRelocateWorkers {
    /// A pool of `worker_count` participants, clamped to at least 1 (the
    /// calling thread always participates, exactly as G1's driver does).
    pub fn new(worker_count: usize) -> Self {
        Self {
            worker_count: worker_count.max(1),
            safepoint: Arc::new(ZRelocateSafepoint::new()),
        }
    }

    /// A pool sharing an externally-owned safepoint, so the VM's pause
    /// orchestrator can drive it.
    pub fn with_safepoint(worker_count: usize, safepoint: Arc<ZRelocateSafepoint>) -> Self {
        Self {
            worker_count: worker_count.max(1),
            safepoint,
        }
    }

    /// Participants, including the calling thread.
    pub fn worker_count(&self) -> usize {
        self.worker_count
    }

    /// The yield point the workers poll.
    pub fn safepoint(&self) -> &Arc<ZRelocateSafepoint> {
        &self.safepoint
    }

    /// Drain every page in `pages` with the pool.
    ///
    /// **Do not enable this in the VM until the thread-stack handshake exists**
    /// — see the module header's "Deferred" list. The mechanism is complete and
    /// tested; the *precondition* is not yet satisfiable in this tree.
    pub fn relocate_set(
        &self,
        relocate: &ZRelocate,
        pages: &[ZRelocatePage],
        remap: &ZRemapState,
    ) -> ZRelocateRunResult {
        let mut result = ZRelocateRunResult::new(pages.len());
        if pages.is_empty() {
            return result;
        }

        // Claim cursor and termination counter. `outstanding` starts at the
        // page count: every page is claimed exactly once (the cursor is
        // monotone) and every claim takes a guard, so the counter reaches zero
        // iff every claimed page's drain returned or unwound.
        let cursor = AtomicUsize::new(0);
        let outstanding = AtomicUsize::new(pages.len());

        let extra = self.worker_count.saturating_sub(1);
        let mut totals: Vec<ZWorkerTotals> = Vec::with_capacity(self.worker_count);

        std::thread::scope(|scope| {
            let mut handles = Vec::with_capacity(extra);
            for id in 1..=extra {
                let cursor_ref = &cursor;
                let outstanding_ref = &outstanding;
                handles.push(scope.spawn(move || {
                    self.run_worker(relocate, pages, cursor_ref, outstanding_ref, remap, id)
                }));
            }
            // The calling thread is worker 0.
            totals.push(self.run_worker(relocate, pages, &cursor, &outstanding, remap, 0));
            for h in handles {
                match h.join() {
                    Ok(t) => totals.push(t),
                    Err(_) => {
                        // Unreachable: the worker catches its own unwinds. Kept
                        // because "unreachable" is exactly what the G1 wedge
                        // was assumed to be.
                        tracing::error!(
                            target: "zgc",
                            "ZRelocateWorkers: a worker thread unwound past its own \
                             catch_unwind — treating the cycle as failed"
                        );
                        totals.push(ZWorkerTotals {
                            panics: 1,
                            ..Default::default()
                        });
                    }
                }
            }
        });

        for t in totals.into_iter() {
            result.merge_worker(t);
        }
        result.outstanding_at_exit = outstanding.load(Ordering::Acquire);
        result.log("relocate_set");
        result
    }

    fn run_worker(
        &self,
        relocate: &ZRelocate,
        pages: &[ZRelocatePage],
        cursor: &AtomicUsize,
        outstanding: &AtomicUsize,
        remap: &ZRemapState,
        worker_id: usize,
    ) -> ZWorkerTotals {
        let membership = self.safepoint.join();
        let mut totals = ZWorkerTotals::default();
        let mut local = ZRelocateLocal::new();

        loop {
            // `Relaxed`: the claim publishes no data. `pages` is an immutable
            // slice already visible to every worker through `thread::scope`'s
            // spawn, and the *object* data is published by the forwarding
            // table's release-CAS, not by this counter. All we need is that
            // each index is handed out once, which `fetch_add` guarantees on
            // its own.
            let idx = cursor.fetch_add(1, Ordering::Relaxed);
            if idx >= pages.len() {
                break;
            }
            let work = &pages[idx];
            let page_id = work.page.id();

            // Taken BEFORE any fallible work and retired by Drop, so an unwind
            // out of the drain retires it on the way past.
            let _retire = ZOutstandingGuard::new(outstanding);

            let drained = std::panic::catch_unwind(AssertUnwindSafe(|| {
                relocate.drain_page(work, &mut local, &self.safepoint, &membership)
            }));

            match drained {
                Ok(Ok(drain)) => {
                    remap.note_relocated_objects(page_id, drain.objects_relocated);
                    totals.objects_seen += drain.objects_seen;
                    if drain.walk_aborted {
                        totals.aborted_walk_page_ids.push(page_id);
                    } else if remap.mark_page_relocated(page_id) {
                        totals.pages_completed += 1;
                    } else {
                        totals.failed_page_ids.push(page_id);
                    }
                }
                Ok(Err(e)) => {
                    tracing::error!(
                        target: "zgc",
                        worker = worker_id,
                        page_id = page_id,
                        error = %e,
                        "ZRelocateWorkers: page drain failed; page stays quarantined"
                    );
                    totals.failed_page_ids.push(page_id);
                }
                Err(_) => {
                    // The page is left un-evacuated and quarantined, and the
                    // run result carries a non-zero panic count so the driver
                    // fails the cycle. Continuing to the next page is
                    // deliberate: one corrupt header must not abandon the rest
                    // of the relocation set.
                    tracing::error!(
                        target: "zgc",
                        worker = worker_id,
                        page_id = page_id,
                        "ZRelocateWorkers: worker panicked while draining a page — the \
                         outstanding count is retired by its Drop guard, so the cycle FAILS \
                         loudly instead of hanging"
                    );
                    totals.panics += 1;
                    totals.failed_page_ids.push(page_id);
                }
            }
        }

        // Even after a panic these pairs describe entries that were really
        // published; dropping them would strand exactly the roots that need
        // them.
        totals.counts.add(&local.counts);
        relocate.flush_local(&mut local);
        drop(membership);
        totals
    }
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::zgc::page::{ZPageConfig, ZPageSizeClass};
    use std::sync::atomic::AtomicU64;
    use std::sync::Barrier;

    // -- test fixture -------------------------------------------------------

    /// A miniature heap geometry, the same shape page.rs's own tests use: a
    /// 256 KiB reservation instead of 256 MiB.
    fn test_page_config() -> ZPageConfig {
        ZPageConfig {
            granule_size: 4096,
            small_page_size: 8192,
            medium_page_size: 65536,
            small_object_limit: 8192 / 8,
            medium_object_limit: 65536 / 8,
            max_capacity: 4096 * 64,
        }
    }

    /// A synthetic test object: `[u64 size][u64 tag][payload...]`.
    ///
    /// Deliberately **not** an `ObjectHeader` — this module never decodes one
    /// (that is `ZRelocateContext::object_size`'s job), so the tests do not
    /// need one and pretending otherwise would couple them to a layout that has
    /// changed three times this year.
    const TEST_TAG: u64 = 0x5A47_4352_4C43_0001;

    /// Fill `[addr, addr + size)` with a size word, a tag and a seeded pattern.
    fn write_test_object(addr: usize, size: usize, seed: u8) {
        assert!(size >= 16 && size % 8 == 0);
        // SAFETY: `addr` was handed out by `ZPageReal::alloc` for at least
        // `size` bytes inside the allocator's reservation, which outlives the
        // test, and no other thread has been given these bytes.
        unsafe {
            std::ptr::write(addr as *mut u64, size as u64);
            std::ptr::write((addr + 8) as *mut u64, TEST_TAG);
            for i in 16..size {
                std::ptr::write((addr + i) as *mut u8, seed.wrapping_add(i as u8));
            }
        }
    }

    /// Read `[addr, addr + size)` back as bytes.
    fn read_bytes(addr: usize, size: usize) -> Vec<u8> {
        let mut out = vec![0u8; size];
        // SAFETY: same provenance as `write_test_object`; the range is inside a
        // live page of the allocator's reservation.
        unsafe {
            std::ptr::copy_nonoverlapping(addr as *const u8, out.as_mut_ptr(), size);
        }
        out
    }

    /// Test context over a real [`ZPageAllocator`].
    struct TestCtx {
        allocator: Arc<ZPageAllocator>,
        relocated: Mutex<Vec<(u64, u64)>>,
        abandoned: AtomicUsize,
        alloc_calls: AtomicUsize,
        /// When non-zero, `object_size` panics for this address (panic test).
        panic_at: AtomicU64,
        /// When true, `alloc_in` always fails (to-space exhaustion test).
        starve: AtomicBool,
    }

    impl TestCtx {
        fn new(allocator: Arc<ZPageAllocator>) -> Self {
            Self {
                allocator,
                relocated: Mutex::new(Vec::new()),
                abandoned: AtomicUsize::new(0),
                alloc_calls: AtomicUsize::new(0),
                panic_at: AtomicU64::new(0),
                starve: AtomicBool::new(false),
            }
        }

        fn relocated_pairs(&self) -> Vec<(u64, u64)> {
            let g = self.relocated.lock();
            let v = g.clone();
            drop(g);
            v
        }
    }

    impl ZRelocateContext for TestCtx {
        fn object_size(&self, addr: u64) -> usize {
            let boom = self.panic_at.load(Ordering::Relaxed);
            if boom != 0 && boom == addr {
                panic!("TestCtx: deliberate panic decoding the object at {addr:#x}");
            }
            // SAFETY: every address the tests pass here is an object base
            // inside a live page, written by `write_test_object`.
            unsafe { std::ptr::read(addr as *const u64) as usize }
        }

        fn alloc_in(&self, _gen_hint: u8, bytes: usize, align: usize) -> Option<u64> {
            self.alloc_calls.fetch_add(1, Ordering::Relaxed);
            if self.starve.load(Ordering::Relaxed) {
                return None;
            }
            self.allocator
                .alloc_object(bytes, align)
                .ok()
                .map(|a| a as u64)
        }

        fn on_relocated(&self, from: u64, to: u64) {
            let mut g = self.relocated.lock();
            g.push((from, to));
            drop(g);
        }

        fn on_abandoned(&self, _to: u64, _bytes: usize) {
            self.abandoned.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Everything a test needs, wired together.
    struct Fixture {
        allocator: Arc<ZPageAllocator>,
        ctx: Arc<TestCtx>,
        registry: Arc<ZForwardingRegistry>,
        relocate: Arc<ZRelocate>,
        remap: Arc<ZRemapState>,
    }

    fn fixture() -> Fixture {
        let allocator =
            Arc::new(ZPageAllocator::new(test_page_config()).expect("test geometry must validate"));
        let ctx = Arc::new(TestCtx::new(Arc::clone(&allocator)));
        let registry = Arc::new(ZForwardingRegistry::new());
        // Explicit binding rather than an `as` cast: the unsizing coercion
        // `Arc<TestCtx> -> Arc<dyn ZRelocateContext>` needs a coercion site.
        // `ctx.clone()` (method form), NOT `Arc::clone(&ctx)`: the associated-fn
        // form infers its type parameter from the *expected* type, so it wants a
        // `&Arc<dyn ZRelocateContext>` argument and never reaches the unsizing
        // coercion. The method form clones `Arc<TestCtx>` and then coerces.
        let dyn_ctx: Arc<dyn ZRelocateContext> = ctx.clone();
        let relocate = Arc::new(ZRelocate::new(
            dyn_ctx,
            Arc::clone(&allocator),
            Arc::clone(&registry),
            ZRelocateConfig::default(),
        ));
        Fixture {
            allocator,
            ctx,
            registry,
            relocate,
            remap: Arc::new(ZRemapState::new()),
        }
    }

    /// Build a from-space page holding `count` objects of `size` bytes, claim
    /// it for the relocation set, and install its forwarding table.
    ///
    /// The page comes from `alloc_page`, not from `alloc_object`, so it is
    /// never the allocator's *shared* page — which is what guarantees that
    /// to-space allocations (which go through `alloc_object`) land in a
    /// different page and can never alias the source.
    fn make_from_page(f: &Fixture, count: usize, size: usize) -> (Arc<ZPageReal>, Vec<usize>) {
        let page = f
            .allocator
            .alloc_page(ZPageSizeClass::Small, 0)
            .expect("small page");
        let mut addrs = Vec::with_capacity(count);
        for i in 0..count {
            let a = page
                .alloc(size, 8)
                .expect("from-page must hold the objects");
            write_test_object(a, size, i as u8);
            addrs.push(a);
        }
        page.set_live_bytes(count * size);
        page.set_state(ZPageState::Relocatable);
        assert!(page.try_transition(ZPageState::Relocatable, ZPageState::InRelocationSet));

        f.registry.install(
            page.id(),
            Arc::new(ZForwardingTable::for_page(page.id(), count.max(1))),
        );
        f.remap.begin_cycle();
        assert!(f.remap.select_page(page.id(), count));
        (page, addrs)
    }

    // -- 1. single-object round trip ---------------------------------------

    #[test]
    fn single_object_relocate_round_trip() {
        let f = fixture();
        let (page, addrs) = make_from_page(&f, 1, 64);
        let table = f.registry.get(page.id()).unwrap();
        let mut local = ZRelocateLocal::new();

        let before = read_bytes(addrs[0], 64);
        let out = f
            .relocate
            .relocate_object(&page, &table, addrs[0] as u64, 0, &mut local)
            .expect("relocation must succeed");

        assert!(out.fresh, "the first relocation of an object is fresh");
        assert_eq!(out.bytes, 64);
        assert_ne!(out.to, addrs[0] as u64, "the object must have moved");
        assert_eq!(read_bytes(out.to as usize, 64), before);

        // A second resolution adopts and copies nothing.
        let again = f
            .relocate
            .relocate_object(&page, &table, addrs[0] as u64, 0, &mut local)
            .expect("second resolution must succeed");
        assert_eq!(again.to, out.to);
        assert!(!again.fresh);
        assert_eq!(again.bytes, 0);

        assert_eq!(table.entry_count(), 1, "exactly one entry published");
        f.relocate.flush_local(&mut local);
        assert_eq!(f.relocate.stats().snapshot().objects_relocated, 1);
        assert_eq!(f.relocate.stats().snapshot().objects_adopted, 1);
    }

    // -- 2. THE important test: N threads race the SAME object -------------

    #[test]
    fn racing_threads_converge_on_one_winner_and_one_copy() {
        const THREADS: usize = 8;
        const SIZE: usize = 96;

        let f = fixture();
        let (page, addrs) = make_from_page(&f, 1, SIZE);
        let from = addrs[0] as u64;
        let expected = read_bytes(addrs[0], SIZE);
        let table = f.registry.get(page.id()).unwrap();

        let barrier = Arc::new(Barrier::new(THREADS));
        let mut results: Vec<u64> = Vec::new();
        let mut fresh_count = 0usize;

        std::thread::scope(|scope| {
            let mut handles = Vec::with_capacity(THREADS);
            for _ in 0..THREADS {
                let relocate = Arc::clone(&f.relocate);
                let table = Arc::clone(&table);
                let page = Arc::clone(&page);
                let barrier = Arc::clone(&barrier);
                handles.push(scope.spawn(move || {
                    let mut local = ZRelocateLocal::new();
                    // Release every thread into `relocate_object` at once — the
                    // narrower the window, the likelier a real CAS collision.
                    barrier.wait();
                    let out = relocate
                        .relocate_object(&page, &table, from, 0, &mut local)
                        .expect("every racer must succeed");
                    relocate.flush_local(&mut local);
                    out
                }));
            }
            for h in handles {
                let out = h.join().expect("no racer may panic");
                if out.fresh {
                    fresh_count += 1;
                }
                results.push(out.to);
            }
        });

        // (a) Every thread observed the SAME winner.
        let winner = results[0];
        assert!(winner != 0);
        for (i, r) in results.iter().enumerate() {
            assert_eq!(
                *r, winner,
                "thread {i} disagreed about the winner — the object's identity split, which \
                 is exactly the G1 loser-used-its-own-value defect"
            );
        }

        // (b) Exactly ONE copy is published.
        assert_eq!(fresh_count, 1, "exactly one thread may report `fresh`");
        assert_eq!(table.entry_count(), 1, "exactly one forwarding entry");
        assert_eq!(
            f.ctx.relocated_pairs().len(),
            1,
            "`on_relocated` must fire once per distinct object"
        );
        assert_eq!(f.ctx.relocated_pairs()[0], (from, winner));

        // (c) The published copy is byte-identical to the source.
        assert_eq!(read_bytes(winner as usize, SIZE), expected);

        // (d) Every racer that copied and lost is accounted for, and every
        //     racer is accounted for exactly once.
        let counts = f.relocate.stats().snapshot();
        assert_eq!(counts.objects_relocated, 1);
        assert_eq!(counts.resolutions(), THREADS);
        assert_eq!(
            counts.objects_abandoned,
            f.ctx.abandoned.load(Ordering::Relaxed)
        );

        // (e) The pointer map has the move, once.
        assert_eq!(f.relocate.record().len(), 1);
        assert_eq!(
            f.relocate.record().get(from as usize),
            Some(winner as usize)
        );
    }

    // -- 3. byte identity for a range of sizes ------------------------------

    #[test]
    fn copied_object_bytes_are_identical() {
        let f = fixture();
        for &size in &[16usize, 24, 64, 200, 512] {
            let (page, addrs) = make_from_page(&f, 1, size);
            let table = f.registry.get(page.id()).unwrap();
            let mut local = ZRelocateLocal::new();
            let before = read_bytes(addrs[0], size);
            let out = f
                .relocate
                .relocate_object(&page, &table, addrs[0] as u64, 0, &mut local)
                .expect("relocation must succeed");
            assert_eq!(
                read_bytes(out.to as usize, size),
                before,
                "size {size}: the copy must be byte-identical"
            );
            f.relocate.flush_local(&mut local);
        }
    }

    // -- 4. a fully drained page can be recycled ---------------------------

    #[test]
    fn full_page_relocation_empties_the_page_and_permits_recycling() {
        const COUNT: usize = 24;
        const SIZE: usize = 64;

        let f = fixture();
        let (page, addrs) = make_from_page(&f, COUNT, SIZE);
        let page_id = page.id();
        let cycle = f.remap.current_cycle();

        let work = vec![ZRelocatePage::new(Arc::clone(&page))];
        assert!(work[0].is_claimed());

        // SAFETY: this test is single-threaded and has no mutator to park.
        let stw = unsafe { StopTheWorldToken::new() };
        let result = f.relocate.relocate_stw(&stw, &work, &f.remap);

        assert!(result.is_clean(), "clean drain: {result:?}");
        assert_eq!(result.pages_completed, 1);
        assert_eq!(result.counts.objects_relocated, COUNT);
        assert_eq!(result.outstanding_at_exit, 0);

        let table = f.registry.get(page_id).unwrap();
        assert_eq!(table.entry_count(), COUNT);
        for a in addrs.iter() {
            assert!(table.find_payload((*a - page.base()) as u64).is_some());
        }

        // ZR-1: `Relocated` is NOT recyclable. The page is quarantined.
        assert_eq!(f.remap.state_of(page_id), Some(ZPageRemapState::Relocated));
        assert!(
            !f.remap.may_recycle(page_id),
            "a Relocated page is quarantined until its table is uninstalled (ZR-1)"
        );
        assert_eq!(f.remap.quarantined_pages(), vec![page_id]);

        // The concurrent rule needs a STRICTLY later mark; the same cycle's
        // mark does not qualify.
        assert!(f.remap.note_mark_complete(cycle).is_empty());
        assert!(!f.remap.may_recycle(page_id));
        assert_eq!(f.remap.note_mark_complete(cycle + 1), vec![page_id]);
        assert!(f.remap.may_recycle(page_id));

        // Now the page may go back.
        f.registry.clear();
        f.allocator.free_page(&page);
        assert!(f.remap.note_recycled(page_id));
        assert_eq!(page.state(), ZPageState::Free);
        assert_eq!(page.used(), 0);
        assert!(f.remap.quarantined_pages().is_empty());
    }

    #[test]
    fn stw_remap_completes_within_the_pause() {
        let f = fixture();
        let (page, _addrs) = make_from_page(&f, 4, 64);
        let page_id = page.id();
        let cycle = f.remap.current_cycle();
        let work = vec![ZRelocatePage::new(Arc::clone(&page))];
        // SAFETY: single-threaded test, no mutator to park.
        let stw = unsafe { StopTheWorldToken::new() };
        let _ = f.relocate.relocate_stw(&stw, &work, &f.remap);

        // The STW driver rewrites every reference inside the pause, so the
        // SAME cycle qualifies — the `<=` that distinguishes this path.
        assert!(!f.remap.may_recycle(page_id));
        assert_eq!(f.remap.note_stw_remap_complete(cycle), vec![page_id]);
        assert!(f.remap.may_recycle(page_id));
    }

    #[test]
    fn a_quarantined_page_cannot_be_reselected() {
        let f = fixture();
        let (page, _addrs) = make_from_page(&f, 2, 64);
        let page_id = page.id();
        let work = vec![ZRelocatePage::new(Arc::clone(&page))];
        // SAFETY: single-threaded test.
        let stw = unsafe { StopTheWorldToken::new() };
        let _ = f.relocate.relocate_stw(&stw, &work, &f.remap);

        f.remap.begin_cycle();
        assert!(
            !f.remap.select_page(page_id, 2),
            "re-selecting a quarantined page would replace a table stale references \
             still resolve through (ZR-1)"
        );
    }

    // -- 5. the abandoned copy must not corrupt the target cursor -----------

    #[test]
    fn abandoned_copies_do_not_corrupt_the_target_page_cursor() {
        const THREADS: usize = 6;
        const SIZE: usize = 64;

        let f = fixture();
        let (page, addrs) = make_from_page(&f, 1, SIZE);
        let from = addrs[0] as u64;
        let expected = read_bytes(addrs[0], SIZE);
        let table = f.registry.get(page.id()).unwrap();
        let barrier = Arc::new(Barrier::new(THREADS));
        let mut winner = 0u64;

        std::thread::scope(|scope| {
            let mut handles = Vec::with_capacity(THREADS);
            for _ in 0..THREADS {
                let relocate = Arc::clone(&f.relocate);
                let table = Arc::clone(&table);
                let page = Arc::clone(&page);
                let barrier = Arc::clone(&barrier);
                handles.push(scope.spawn(move || {
                    let mut local = ZRelocateLocal::new();
                    barrier.wait();
                    let out = relocate
                        .relocate_object(&page, &table, from, 0, &mut local)
                        .expect("racer");
                    relocate.flush_local(&mut local);
                    out.to
                }));
            }
            for h in handles {
                let to = h.join().expect("racer");
                if winner == 0 {
                    winner = to;
                }
                assert_eq!(to, winner);
            }
        });

        // A fresh allocation must not be handed any byte the winner owns: an
        // abandoned copy leaves the cursor *ahead*, never behind. (Rewinding it
        // is the tempting "reclaim the waste" optimisation, and this assertion
        // is what would catch it.)
        let fresh = f
            .ctx
            .alloc_in(0, SIZE, ZRELOCATE_ALIGN)
            .expect("to-space must still have room");
        let w = winner;
        assert!(
            fresh >= w + SIZE as u64 || fresh + SIZE as u64 <= w,
            "a post-race allocation at {fresh:#x} overlaps the winning copy at {w:#x}"
        );

        // ...and the winner's bytes are untouched by everything that followed.
        assert_eq!(read_bytes(winner as usize, SIZE), expected);
        assert_eq!(table.entry_count(), 1);
    }

    // -- 6. a panicking worker must not hang the cycle ---------------------

    #[test]
    fn outstanding_guard_retires_on_unwind() {
        let outstanding = AtomicUsize::new(1);
        let r = std::panic::catch_unwind(AssertUnwindSafe(|| {
            let _g = ZOutstandingGuard::new(&outstanding);
            panic!("deliberate: a GC worker exploding mid-page");
        }));
        assert!(r.is_err(), "the panic must be observed, not swallowed");
        assert_eq!(
            outstanding.load(Ordering::Acquire),
            0,
            "the RAII guard must retire the count on the unwind path — a bare fetch_sub \
             after the fallible call is what turned the G1 crash into a 3h08m hang"
        );
    }

    #[test]
    fn a_panicking_worker_fails_the_cycle_without_hanging() {
        const COUNT: usize = 8;
        const SIZE: usize = 64;

        let f = fixture();
        // Two pages: one healthy, one whose third object explodes on decode.
        let (good, _ga) = make_from_page(&f, COUNT, SIZE);
        let (bad, ba) = make_from_page(&f, COUNT, SIZE);
        f.ctx.panic_at.store(ba[2] as u64, Ordering::Relaxed);

        let work = vec![
            ZRelocatePage::new(Arc::clone(&good)),
            ZRelocatePage::new(Arc::clone(&bad)),
        ];
        let workers = ZRelocateWorkers::new(4);
        let result = workers.relocate_set(&f.relocate, &work, &f.remap);

        // The whole point: the run RETURNS.
        assert_eq!(
            result.outstanding_at_exit, 0,
            "every claimed page must retire its outstanding count, panic or not"
        );
        assert_eq!(result.worker_panics, 1);
        assert!(!result.is_clean());
        assert_eq!(result.failed_page_ids, vec![bad.id()]);

        // The healthy page still finished — one bad header must not abandon the
        // rest of the relocation set.
        assert_eq!(result.pages_completed, 1);
        assert_eq!(
            f.remap.state_of(good.id()),
            Some(ZPageRemapState::Relocated)
        );

        // The failed page is NOT marked relocated and therefore can never be
        // recycled by ZR-1's rule — which is the correct outcome for a page
        // whose survivors were not all copied out. It is still `Selected`: the
        // drain never returned, so its object credit was never applied either.
        assert_eq!(f.remap.state_of(bad.id()), Some(ZPageRemapState::Selected));
        assert!(!f.remap.may_recycle(bad.id()));
        // Only the healthy page can ever leave quarantine.
        assert_eq!(
            f.remap.note_mark_complete(f.remap.current_cycle() + 1),
            vec![good.id()]
        );
    }

    #[test]
    fn to_space_exhaustion_fails_the_page_loudly() {
        let f = fixture();
        let (page, _addrs) = make_from_page(&f, 4, 64);
        f.ctx.starve.store(true, Ordering::Relaxed);

        let work = vec![ZRelocatePage::new(Arc::clone(&page))];
        // SAFETY: single-threaded test.
        let stw = unsafe { StopTheWorldToken::new() };
        let result = f.relocate.relocate_stw(&stw, &work, &f.remap);

        assert!(!result.is_clean());
        assert_eq!(result.failed_page_ids, vec![page.id()]);
        assert_eq!(result.outstanding_at_exit, 0);
        assert!(f.relocate.stats().alloc_failures() >= 1);
        assert!(!f.remap.may_recycle(page.id()));
    }

    #[test]
    fn an_implausible_object_size_aborts_the_walk_instead_of_spinning() {
        let f = fixture();
        let (page, addrs) = make_from_page(&f, 4, 64);
        // Corrupt the second object's size word to 0 — `gc::object_total_size`'s
        // corruption sentinel. A stride of 0 would spin forever.
        // SAFETY: `addrs[1]` is a live object base written by the fixture.
        unsafe {
            std::ptr::write(addrs[1] as *mut u64, 0u64);
        }
        let work = vec![ZRelocatePage::new(Arc::clone(&page))];
        // SAFETY: single-threaded test.
        let stw = unsafe { StopTheWorldToken::new() };
        let result = f.relocate.relocate_stw(&stw, &work, &f.remap);

        assert_eq!(result.aborted_walk_page_ids, vec![page.id()]);
        assert_eq!(result.pages_completed, 0);
        assert_eq!(result.outstanding_at_exit, 0);
        assert_ne!(
            f.remap.state_of(page.id()),
            Some(ZPageRemapState::Relocated)
        );
    }

    // -- 7. the pointer map records every move -----------------------------

    #[test]
    fn the_pointer_map_records_every_move() {
        const COUNT: usize = 32;
        const SIZE: usize = 48;

        let f = fixture();
        let (page, addrs) = make_from_page(&f, COUNT, SIZE);
        let work = vec![ZRelocatePage::new(Arc::clone(&page))];
        // SAFETY: single-threaded test.
        let stw = unsafe { StopTheWorldToken::new() };
        let result = f.relocate.relocate_stw(&stw, &work, &f.remap);
        assert!(result.is_clean());

        let map = f.relocate.record().snapshot_pointer_map();
        assert_eq!(map.len(), COUNT, "one entry per relocated object");

        for a in addrs.iter() {
            let to = *map.get(a).expect("every from-address must be in the map");
            assert_ne!(to, *a, "a recorded move must actually move");
            // Read the table through the sanctioned decoder, never raw: a bare
            // `find` returns an ENCODED payload (see `ZRelocate::decode_to`).
            assert_eq!(
                f.relocate.forward_lookup(*a as u64),
                Some(to as u64),
                "the map and the forwarding table must agree"
            );
            // And the bytes really are there.
            let size = f.ctx.object_size(to as u64);
            assert_eq!(size, SIZE);
        }

        // `into_pointer_map` hands the map over and leaves the record empty —
        // this is the value `GcResult::pointer_map` must carry.
        let taken: HashMap<usize, usize> = f.relocate.record().into_pointer_map();
        assert_eq!(taken.len(), COUNT);
        assert!(f.relocate.record().is_empty());
    }

    #[test]
    fn a_disabled_record_accumulates_nothing() {
        let allocator = Arc::new(ZPageAllocator::new(test_page_config()).expect("test geometry"));
        let ctx = Arc::new(TestCtx::new(Arc::clone(&allocator)));
        let registry = Arc::new(ZForwardingRegistry::new());
        // `ctx.clone()` (method form), NOT `Arc::clone(&ctx)`: the associated-fn
        // form infers its type parameter from the *expected* type, so it wants a
        // `&Arc<dyn ZRelocateContext>` argument and never reaches the unsizing
        // coercion. The method form clones `Arc<TestCtx>` and then coerces.
        let dyn_ctx: Arc<dyn ZRelocateContext> = ctx.clone();
        let relocate = ZRelocate::new(
            dyn_ctx,
            Arc::clone(&allocator),
            Arc::clone(&registry),
            ZRelocateConfig {
                record_pointer_map: false,
                ..ZRelocateConfig::default()
            },
        );
        assert!(!relocate.record().is_enabled());
        relocate.record().record_many(&[(0x1000, 0x2000)]);
        assert!(relocate.record().is_empty());
    }

    #[test]
    fn destination_addresses_round_trip_through_the_encoding() {
        let f = fixture();
        let base = f.allocator.base() as u64;
        assert_eq!(f.relocate.to_encoding_base(), base);

        for &off in &[0u64, 8, 4096, 1 << 18] {
            let abs = base + off;
            let enc = f
                .relocate
                .encode_to(abs)
                .expect("an in-heap address must encode");
            // The three things `ZgcForwardingEntry::pack` demands.
            assert_ne!(enc.encoded(), 0, "pack rejects a zero destination");
            assert_eq!(enc.encoded() % 8, 0, "pack requires 8-alignment");
            assert!(
                enc.encoded() <= (1u64 << 44) - 8,
                "a raw process address would blow the 41-bit `to` field on Linux; the \
                 heap-relative encoding is what keeps it in range"
            );
            assert_eq!(f.relocate.decode_to(enc), abs, "round trip");
        }

        // Below the base is a modelling error, reported rather than wrapped.
        assert!(matches!(
            f.relocate.encode_to(base - 8),
            Err(ZRelocateError::UnencodableDestination { .. })
        ));

        // The absolute mode is still expressible, for the day ZGC owns a low
        // reserved virtual range — but only over a heap that actually fits
        // below the 16 TiB the `to` field reaches. WHICH ARM RUNS IS THE POINT:
        // `Some(0)` is precisely the configuration that works on the Windows dev
        // host (heap placed low) and cannot work on the Linux build host.
        let cfg_absolute = ZRelocateConfig {
            to_encoding_base: Some(0),
            ..ZRelocateConfig::default()
        };
        let dyn_ctx: Arc<dyn ZRelocateContext> = f.ctx.clone();
        let built = ZRelocate::try_new(
            dyn_ctx,
            Arc::clone(&f.allocator),
            Arc::clone(&f.registry),
            cfg_absolute,
        );
        if f.allocator.end() as u64 <= ZFWD_MAX_PAYLOAD {
            let absolute = built.expect("a heap entirely below 16 TiB encodes absolutely");
            assert_eq!(absolute.to_encoding_base(), 0);
            assert_eq!(
                absolute.encode_to(0x1234_5678),
                Ok(ZForwardingPayload::from_encoded(0x1234_5678))
            );
            assert_eq!(
                absolute.decode_to(ZForwardingPayload::from_encoded(0x1234_5678)),
                0x1234_5678
            );
        } else {
            assert!(
                matches!(
                    built,
                    Err(ZRelocateError::EncodingBaseUnusable { requested: 0, .. })
                ),
                "a heap above 16 TiB must refuse absolute encoding, not discover it \
                 one Unencodable insert at a time: {built:?}"
            );
        }
    }

    // -- 7b. the address-domain boundary (added 2026-08-07) ----------------

    /// The offset domain the load barrier traffics in, round-tripped through
    /// the same relocation the absolute entry points perform.
    #[test]
    fn the_offset_domain_round_trips_and_agrees_with_the_absolute_one() {
        let f = fixture();
        let (page, addrs) = make_from_page(&f, 2, 64);
        let heap_base = f.relocate.heap_base();
        assert_eq!(heap_base, f.allocator.base() as u64);

        let from_abs = addrs[0] as u64;
        let from_off = f
            .relocate
            .offset_of_address(from_abs)
            .expect("an object in the reservation has an offset");
        assert_eq!(from_off, from_abs - heap_base);
        assert_eq!(f.relocate.address_of_offset(from_off), Some(from_abs));
        assert!(
            is_bare_offset(from_off, Z_OFFSET_MASK),
            "an in-heap offset is a bare 42-bit value by construction"
        );

        // Nothing has moved yet.
        assert_eq!(f.relocate.forward_lookup_offset(from_off), None);

        let to_off = f
            .relocate
            .forward_offset(from_off)
            .expect("forward_offset must succeed")
            .expect("the offset is in a relocating page");
        assert!(
            is_bare_offset(to_off, Z_OFFSET_MASK),
            "the value handed to the load barrier must be a bare offset"
        );
        assert_ne!(to_off, from_off, "the object must have moved");

        // The two domains describe the SAME move, differing by exactly the heap
        // base. A discrepancy here is the fixed G1 `address + 3` shape.
        let to_abs = f
            .relocate
            .forward_lookup(from_abs)
            .expect("the absolute lookup sees the same entry");
        assert_eq!(to_off, to_abs - heap_base);
        assert_eq!(f.relocate.address_of_offset(to_off), Some(to_abs));
        assert_eq!(f.relocate.forward_lookup_offset(from_off), Some(to_off));

        // An offset in a page with no installed table is the identity case.
        assert_eq!(f.relocate.forward_offset(to_off).unwrap(), None);
        let _ = page;
    }

    /// A machine address handed to an offset-domain entry point must be
    /// **caught**, never silently accepted — in release builds as well as
    /// debug, because on the Windows dev host the mistake is invisible.
    #[test]
    fn a_machine_address_where_an_offset_belongs_is_caught() {
        let f = fixture();

        // A Linux-shaped heap address: ~2^47, five whole bits past the 42-bit
        // offset field. This is what `page.rs` hands out on the build host.
        const LINUX_SHAPED: u64 = 0x7F00_0000_1000;
        // A still-coloured word, which is the other way to be in the wrong
        // domain: `vaddr::Z_COLORED_TAG` is bit 63.
        const STILL_COLOURED: u64 = (1u64 << 63) | (1u64 << 44) | 0x1000;

        for bad in [LINUX_SHAPED, STILL_COLOURED] {
            assert!(
                !is_bare_offset(bad, Z_OFFSET_MASK),
                "{bad:#x} must not pass the barrier's own domain predicate"
            );

            let caught =
                std::panic::catch_unwind(AssertUnwindSafe(|| f.relocate.forward_offset(bad)));
            match caught {
                // Debug builds: the `debug_assert!` in `check_offset_domain`
                // turns it into a test failure at the boundary.
                Err(_) => {}
                // Release builds: asserts are off, so what must survive is the
                // REFUSAL (plus the `tracing::error!`). Silently answering
                // `Ok(_)` here would be the bug this file exists to prevent.
                Ok(v) => assert!(
                    matches!(v, Err(ZRelocateError::NotInHeap { .. })),
                    "a machine address must be refused, not translated: {v:?}"
                ),
            }

            let caught = std::panic::catch_unwind(AssertUnwindSafe(|| {
                f.relocate.forward_lookup_offset(bad)
            }));
            match caught {
                Err(_) => {}
                Ok(v) => assert_eq!(v, None, "a machine address must not resolve"),
            }
        }
    }

    /// `to_encoding_base: Some(0)` over a Linux-shaped reservation must be
    /// refused up front, not discovered one `Unencodable` insert at a time.
    ///
    /// Driven through [`ZRelocate::check_encoding_base`] with synthetic bases so
    /// the verdict does not depend on where *this* host's allocator landed —
    /// which is the entire hazard being pinned.
    #[test]
    fn an_unusable_encoding_base_is_refused_before_the_first_object_moves() {
        // Where Linux `mmap`s a quarter-gigabyte `Vec<u8>`.
        const LINUX_BASE: u64 = 0x7F00_0000_0000;
        // Where a Windows heap of the same size tends to land.
        const WINDOWS_BASE: u64 = 0x0000_0200_0000;
        const HEAP: u64 = 256 << 20;

        // (a) The footgun itself.
        assert!(
            matches!(
                ZRelocate::check_encoding_base(0, LINUX_BASE, LINUX_BASE + HEAP),
                Err(ZRelocateError::EncodingBaseUnusable { requested: 0, .. })
            ),
            "absolute encoding over a 0x7f… heap cannot fit the 41-bit `to` field"
        );

        // (b) ...and why it ships green: the same configuration is genuinely
        //     fine on a low heap, so no dev-host test would ever notice.
        assert!(ZRelocate::check_encoding_base(0, WINDOWS_BASE, WINDOWS_BASE + HEAP).is_ok());

        // (c) The default — the allocator base — works at either placement,
        //     because the encoded value is bounded by the heap SIZE.
        assert!(ZRelocate::check_encoding_base(LINUX_BASE, LINUX_BASE, LINUX_BASE + HEAP).is_ok());
        assert!(
            ZRelocate::check_encoding_base(WINDOWS_BASE, WINDOWS_BASE, WINDOWS_BASE + HEAP).is_ok()
        );

        // (d) A base above the heap base cannot express the bottom of the heap
        //     at all — every destination there fails `checked_sub`.
        assert!(
            ZRelocate::check_encoding_base(LINUX_BASE + 8, LINUX_BASE, LINUX_BASE + HEAP).is_err()
        );

        // (e) The boundary: a heap ending exactly at ZFWD_MAX_PAYLOAD encodes
        //     absolutely; one byte-grid past it does not.
        assert!(ZRelocate::check_encoding_base(0, 8, ZFWD_MAX_PAYLOAD).is_ok());
        assert!(ZRelocate::check_encoding_base(0, 8, ZFWD_MAX_PAYLOAD + 8).is_err());
    }

    /// `encode_to` must bound the destination at BOTH ends. Before 2026-08-07 it
    /// checked only the lower one, so a destination above the modelled heap
    /// surfaced as `Forwarding { cause: Unencodable }` — blaming the forwarding
    /// table for the relocator's mistake.
    #[test]
    fn encode_to_rejects_a_destination_above_the_encoding_range() {
        let f = fixture();
        let base = f.relocate.to_encoding_base();
        assert_ne!(base, 0, "the default configuration is heap-relative");

        // The largest destination the `to` field can hold, and the next grid
        // slot up.
        let highest_ok = base + ZFWD_MAX_PAYLOAD - ZRELOCATE_ENCODING_BIAS;
        let too_high = highest_ok + ZRELOCATE_ENCODING_BIAS;

        let enc = f
            .relocate
            .encode_to(highest_ok)
            .expect("the top of the encodable range must encode");
        assert_eq!(enc.encoded(), ZFWD_MAX_PAYLOAD);

        match f.relocate.encode_to(too_high) {
            Err(ZRelocateError::DestinationAboveEncodingRange {
                to,
                encoded,
                max_payload,
                ..
            }) => {
                assert_eq!(to, too_high);
                assert!(encoded > max_payload);
                assert_eq!(max_payload, ZFWD_MAX_PAYLOAD);
            }
            other => panic!(
                "a destination above the encoding range must name ITSELF as the cause, \
                 not reach the forwarding table and come back as Unencodable: {other:?}"
            ),
        }

        // And the message must not point at the table.
        let msg = ZRelocateError::DestinationAboveEncodingRange {
            to: too_high,
            base,
            encoded: ZFWD_MAX_PAYLOAD + 8,
            max_payload: ZFWD_MAX_PAYLOAD,
        }
        .to_string();
        assert!(msg.contains("NOT a forwarding-table defect"), "{msg}");
    }

    // -- 8. the barrier entry point ----------------------------------------

    #[test]
    fn forward_relocates_on_demand_and_passes_through_unrelocating_pages() {
        let f = fixture();
        let (page, addrs) = make_from_page(&f, 2, 64);
        let from = addrs[0] as u64;
        let before = read_bytes(addrs[0], 64);

        assert_eq!(f.relocate.forward_lookup(from), None, "nothing copied yet");
        let to = f
            .relocate
            .forward(from)
            .expect("forward must succeed")
            .expect("the address is in a relocating page");
        assert_ne!(to, from);
        assert_eq!(read_bytes(to as usize, 64), before);
        assert_eq!(f.relocate.forward_lookup(from), Some(to));
        // Idempotent.
        assert_eq!(f.relocate.forward(from).unwrap(), Some(to));

        // An address in a page with no installed table did not move.
        assert_eq!(f.relocate.forward(to).unwrap(), None);
        // An address outside the heap entirely.
        assert_eq!(f.relocate.forward(8).unwrap(), None);
        let _ = page;
    }

    // -- 9. the safepoint --------------------------------------------------

    #[test]
    fn a_worker_parks_at_a_requested_safepoint_and_resumes() {
        let sp = Arc::new(ZRelocateSafepoint::new());
        let entered = Arc::new(Barrier::new(2));
        let observed = Arc::new(AtomicUsize::new(0));

        std::thread::scope(|scope| {
            let sp_w = Arc::clone(&sp);
            let entered_w = Arc::clone(&entered);
            let observed_w = Arc::clone(&observed);
            let h = scope.spawn(move || {
                let m = sp_w.join();
                entered_w.wait();
                // Poll until the request has been seen and released. Counts
                // only, never a wall-clock bound — fixed timing bounds are a
                // documented source of CI flakes in this tree.
                loop {
                    sp_w.poll(&m);
                    observed_w.fetch_add(1, Ordering::Relaxed);
                    if !sp_w.is_requested() && observed_w.load(Ordering::Relaxed) > 1 {
                        break;
                    }
                    std::thread::yield_now();
                }
                drop(m);
            });

            entered.wait();
            sp.request();
            sp.wait_until_all_parked();
            // While parked, the worker is provably not touching the heap.
            assert!(sp.parked() >= 1 || sp.active() == 0);
            sp.release();
            h.join().expect("worker must finish");
        });

        assert_eq!(sp.parked(), 0, "no member may stay parked after release");
        assert_eq!(sp.active(), 0, "membership must be retired on drop");
    }

    #[test]
    fn a_dead_member_cannot_be_waited_for() {
        // `wait_until_all_parked` must not block on a member that has gone
        // away — the membership guard's Drop is what makes that true.
        let sp = ZRelocateSafepoint::new();
        {
            let _m = sp.join();
            assert_eq!(sp.active(), 1);
        }
        assert_eq!(sp.active(), 0);
        sp.request();
        sp.wait_until_all_parked(); // returns immediately: nobody is active
        sp.release();
    }

    // -- 10. parallel drain over several pages -----------------------------

    #[test]
    fn parallel_workers_drain_every_page_exactly_once() {
        const PAGES: usize = 6;
        const COUNT: usize = 16;
        const SIZE: usize = 48;

        let f = fixture();
        let mut work = Vec::with_capacity(PAGES);
        let mut all_addrs: Vec<(u64, Arc<ZPageReal>)> = Vec::new();
        for _ in 0..PAGES {
            let (page, addrs) = make_from_page(&f, COUNT, SIZE);
            for a in addrs {
                all_addrs.push((a as u64, Arc::clone(&page)));
            }
            work.push(ZRelocatePage::new(page));
        }

        let workers = ZRelocateWorkers::new(4);
        let result = workers.relocate_set(&f.relocate, &work, &f.remap);

        assert!(result.is_clean(), "{result:?}");
        assert_eq!(result.pages_completed, PAGES);
        assert_eq!(result.counts.objects_relocated, PAGES * COUNT);
        assert_eq!(
            result.counts.objects_abandoned, 0,
            "no two workers share a page"
        );
        assert_eq!(result.outstanding_at_exit, 0);
        assert_eq!(f.relocate.record().len(), PAGES * COUNT);

        for (from, page) in all_addrs.iter() {
            let to = f
                .relocate
                .forward_lookup(*from)
                .expect("every object must be forwarded");
            assert!(
                !page.contains(to as usize),
                "to-space must not be from-space"
            );
            assert_eq!(f.relocate.record().get(*from as usize), Some(to as usize));
        }
    }

    // -- 11. types are shareable across workers ----------------------------

    #[test]
    fn relocation_types_are_send_and_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<ZRelocate>();
        assert_send_sync::<ZRemapState>();
        assert_send_sync::<ZRelocateSafepoint>();
        assert_send_sync::<ZRelocateWorkers>();
        assert_send_sync::<ZRelocationRecord>();
        assert_send_sync::<ZRelocateStats>();
        assert_send_sync::<ZRelocatePage>();
    }

    /// The index must answer exactly what the map it replaced answered, for
    /// every address in and around the moved set — including the two ends,
    /// which the range check special-cases.
    #[test]
    fn the_forward_index_answers_what_a_map_of_the_same_pairs_would() {
        let pairs = [(0x9000, 0x1000), (0x3000, 0x2000), (0x7000, 0x4000)];
        let index = ZForwardIndex::from_pairs(&pairs);
        let reference: HashMap<usize, usize> = pairs.iter().copied().collect();
        for addr in (0..0xC000).step_by(8) {
            assert_eq!(
                index.get(addr),
                reference.get(&addr).copied(),
                "disagreement at {addr:#x}"
            );
            assert_eq!(index.contains_from(addr), reference.contains_key(&addr));
            assert_eq!(
                index.resolve(addr),
                reference.get(&addr).copied().unwrap_or(addr)
            );
        }
        assert_eq!(index.len(), 3);
        assert_eq!(
            index.entries(),
            &[(0x3000, 0x2000), (0x7000, 0x4000), (0x9000, 0x1000)],
            "entries must come back ascending by `from`"
        );
    }

    /// An identity pair is not a move. The high pack emits one for a pinned
    /// survivor it left in place, and recording it would make the rewrite pass
    /// store a slot's own value back over itself — harmless, but it would also
    /// make `contains_from` claim the address was vacated, which the
    /// post-slide verifier reads as a MISSED REWRITE rather than an untouched
    /// object.
    #[test]
    fn an_identity_pair_is_not_a_move() {
        let index = ZForwardIndex::from_pairs(&[(0x4000, 0x4000), (0x8000, 0x2000)]);
        assert_eq!(index.get(0x4000), None);
        assert!(!index.contains_from(0x4000));
        assert_eq!(index.resolve(0x4000), 0x4000);
        assert_eq!(index.get(0x8000), Some(0x2000));
        assert_eq!(index.len(), 1);
    }

    /// The empty index must not claim the whole address space. `lo`/`hi` are
    /// seeded `usize::MAX`/`0` so the range check rejects everything before
    /// the search can look at an empty slice.
    #[test]
    fn an_empty_index_forwards_nothing() {
        let index = ZForwardIndex::from_pairs(&[]);
        assert!(index.is_empty());
        for addr in [0usize, 8, 0x1000, usize::MAX / 2, usize::MAX] {
            assert_eq!(index.get(addr), None);
            assert_eq!(index.resolve(addr), addr);
        }
    }
}
