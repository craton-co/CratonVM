// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! ZGC concurrent-relocation forwarding: the lock-free per-page forwarding
//! table, the relocation-set selector, and the page → table registry.
//!
//! This module is the *production-shaped* machinery that a real byte-copy
//! relocation will drive. It is deliberately independent of the page module
//! (agent Z3): the selector consumes a plain [`PageCandidate`] data struct and
//! the registry is keyed by a bare `page_id`, so nothing here needs to know how
//! a `ZPage` is represented or where its bytes live.
//!
//! The existing [`crate::zgc::ZgcCollector::concurrent_relocate`] is a
//! simulation: it records a `FxHashMap<u64, u64>` forwarding entry per *page*
//! and copies no object bytes (there is nothing to copy from — simulation pages
//! are metadata with synthetic `u64` addresses). That map is single-threaded,
//! per-cycle and per-page-base. This module replaces it, for the real
//! collector, with a per-page **object-granular, lock-free** table that mutator
//! threads may hit concurrently from the load barrier.
//!
//! # The relocation protocol
//!
//! ZGC's defining property is that relocation runs *concurrently with the
//! mutator*. A mutator does not know an object moved; it finds out by loading a
//! reference whose colour bits are stale for the current phase. The full
//! sequence, and where each piece of this module fits:
//!
//! 1. **Load barrier sees a bad-coloured pointer.** The fast path
//!    ([`crate::zgc::LoadBarrier::check`]) compares the pointer's colour bits
//!    against the phase's good-colour set. A match passes through with no work.
//!    A miss enters the slow path — the *only* place any of the below runs.
//!
//! 2. **Strip the colour, find the page.** The address bits
//!    ([`crate::zgc::ColoredPointer::address`], 42 bits) identify a page.
//!
//! 3. **Is the page in the relocation set?** [`ZForwardingRegistry::get`]
//!    answers with `Option<Arc<ZForwardingTable>>`. `None` means this page is
//!    not being evacuated this cycle: the object did not move, so the barrier
//!    only has to *recolour* the pointer (self-heal the slot) and return.
//!    `Some(table)` means the page is under evacuation.
//!
//! 4. **Look the object up.** [`ZForwardingTable::find_payload`] with the
//!    object's page-relative offset.
//!    - **Hit** → the object has already been relocated by *some* thread. Take
//!      the recorded [`ZForwardingPayload`], **decode it to an address**
//!      (see "The stored value is not an address" below), recolour, self-heal
//!      the slot, done. No copy.
//!    - **Miss** → nobody has relocated it yet, and *this mutator thread* must
//!      do it. Fall through to (5).
//!
//! 5. **Relocate.** Allocate `object_size` bytes in a target page, byte-copy
//!    the object from the from-space address to the fresh allocation, *encode*
//!    the destination into a [`ZForwardingPayload`], then publish with
//!    [`ZForwardingTable::insert`].
//!
//! 6. **Take the winner's answer.** `insert` returns **the winning payload**,
//!    which decodes to *our* address if we won the CAS and to *another
//!    thread's* address if we lost. If we lost, we **abandon our copy**: we
//!    simply drop the address on the floor and use the returned one. Every
//!    subsequent reader converges on the same object, so the identity of the
//!    relocated object is single-valued — which is the whole point.
//!
//!    *What "abandon" costs:* the bytes we allocated in the target page stay
//!    allocated and stay unreferenced. They are to-space garbage, reclaimed by
//!    the *next* cycle at zero extra machinery (a target page's live set is
//!    determined by the next mark, and nothing points at the abandoned copy).
//!    We never free it inline: the loser cannot know whether some *other*
//!    thread already observed a reference to it, and a bump-pointer target page
//!    has no free operation anyway. G1's parallel evacuator makes exactly the
//!    same trade (`SharedEvac::evacuate`, `gc/src/g1.rs`). The waste is bounded
//!    by (number of threads racing on one object − 1) × object size, and races
//!    on a single object are rare because the barrier only fires on a *stale*
//!    load.
//!
//! 7. **Remap.** Later — [`crate::zgc::ZgcPhase::ConcurrentRemap`], the
//!    *simulation's* state variable, and lazily during the next cycle's mark —
//!    every remaining stale reference is walked and rewritten through the same
//!    tables. Only once *that* is complete may the tables be dropped
//!    ([`ZForwardingRegistry::clear`]) — the tables must outlive the from-space
//!    pages they describe, which is the reason forwarding cannot live in the
//!    from-space object header (see below).
//!
//!    *Two enums share that name.* `zgc::ZgcPhase` (linked above) is the
//!    collector's state; [`crate::zgc::metrics::ZgcPhase`] is the measurement
//!    key that **times** this step. They were not in sync: until 2026-08-07 the
//!    metrics enum had no `ConcurrentRemap` counter at all, so this pass — a
//!    walk of every remaining stale reference through a per-page hash table —
//!    was **unmeasurable**. Whoever implements it would have had nowhere to
//!    record it, and the report would have read as though remapping were free.
//!    Both enums now carry the variant; a citation to
//!    "`ZgcPhase::ConcurrentRemap`" anywhere in this subsystem should say which
//!    of the two it means.
//!
//! # Why forwarding must live in a side table, not the object header
//!
//! CratonVM's [`cratonvm_types::ObjectHeader`] *does* have room: since the
//! 24 → 16 byte shrink of 2026-08-06 the `mark_word: AtomicU64` at offset
//! [`cratonvm_types::MARK_WORD_OFFSET`] encodes a `MARK_FORWARDED` (`0b11`)
//! state whose upper 62 bits carry a full 64-bit relocation target
//! (`ObjectHeader::make_forwarded` / `forwarding_target`). G1's parallel
//! evacuator uses precisely that slot. So the answer to "is there room?" is
//! *yes* — and ZGC still cannot use it. Three reasons, in order of severity:
//!
//! 1. **The mark word is live mutator state during concurrent relocation.**
//!    `MARK_THIN_LOCKED` / `MARK_INFLATED` are the thin-lock owner and the
//!    inflated-`Monitor` pointer. G1 stamps `MARK_FORWARDED` over them safely
//!    only because it evacuates inside a **stop-the-world pause** with every
//!    mutator parked. ZGC has no such pause: a thread may be inside
//!    `monitorenter` on the very object being relocated. Overwriting the word
//!    would destroy a live monitor pointer, and a CAS loop against a
//!    concurrently-locking mutator would serialise relocation behind locking.
//!
//! 2. **The from-space page dies before remapping finishes.** ZGC unmaps and
//!    recycles a relocated page as soon as *its* objects are copied — remapping
//!    of the remaining references is lazy and spills into the next cycle. A
//!    header-resident forwarding pointer is unmapped along with the object that
//!    holds it, so a late reader would fault. The forwarding data must outlive
//!    the memory it describes; only a side table can.
//!
//! 3. **`CompactHeader` cannot hold it either.** [`crate::compact_header`]'s
//!    `LockState::Forwarded` has a 30-bit inline field (29 usable after the
//!    overflow tag) and spills to a global `RwLock<FxHashMap>` side table for
//!    targets above 4 GB. That is a *lock-taking* side table on the barrier
//!    fast path, and the same lifetime problem as (2).
//!
//! **Verdict: the side table is mandatory for ZGC.** This is not a limitation
//! of CratonVM's header — it is why OpenJDK ZGC has `ZForwarding` /
//! `ZForwardingTable` at all while G1 does not.
//!
//! # Why one packed `AtomicU64` per entry
//!
//! An entry is `(from_offset, payload)`. Splitting it across two atomics —
//! `struct Entry { from: AtomicU64, to: AtomicU64 }` — is **racy no matter what
//! ordering is used**, because publication would take two stores and there is
//! no portable double-word CAS:
//!
//! ```text
//!   writer: from.store(F)          reader: if from.load() == F {
//!           to.store(T)                        return to.load()   // sees 0!
//!                                          }
//! ```
//!
//! The reader can observe the key it is looking for paired with an
//! uninitialised, stale, or *another thread's* value. Worse, two threads racing
//! to install the same key could interleave into a Frankenstein entry
//! (`from` from A, `to` from B) that no thread ever proposed — a torn value.
//! Packing both fields into one `AtomicU64` makes publication a single
//! `compare_exchange` from `EMPTY` to a fully-formed entry: a reader either
//! sees `EMPTY` or sees a `(from, to)` pair that some thread wrote *as a unit*.
//! There is no intermediate state to observe. See [`ZgcForwardingEntry`] for
//! the bit budget.
//!
//! # The stored value is not an address — read this before calling anything
//!
//! The destination half of an entry is a [`ZForwardingPayload`]: an **opaque
//! encoded word**, not a pointer. This module packs, stores, compares and hands
//! back payloads; it never learns what one means. Turning a payload into a
//! dereferenceable address is the *relocator's* job, because the relocator is
//! the thing that chose the encoding.
//!
//! Why an encoding exists at all is [`ZFWD_TO_BITS`]'s whole doc comment, and
//! it is worth the click: the short version is that the payload field is 41
//! bits at 8-byte granularity (a 16 TiB ceiling) while a raw Linux heap address
//! sits at ~2^47, so **absolute addresses do not fit and are not what gets
//! stored**. `gc/src/zgc/relocate.rs` stores `to_absolute - heap_base + 8` via
//! `ZRelocate::encode_to`, and `ZRelocate::decode_to` is the only sanctioned
//! reader.
//!
//! That makes a raw payload a **wrong pointer that looks plausible** — exactly
//! the shape of the fixed G1 `address + 3` defect described below, which
//! presented as a 3-hour hang rather than a crash. The defence here is the type
//! system, not a comment: [`ZForwardingTable::find_payload`] and
//! [`ZForwardingTable::try_insert`] traffic in [`ZForwardingPayload`], never in
//! `u64` addresses, so a caller who wants to treat one as an address has to
//! write [`ZForwardingPayload::encoded`] and mean it. The methods are also
//! *named* `find_payload` / `snapshot_payloads` so that the hazard survives a
//! grep as well as a compile.
//!
//! # What the fixed G1 evacuation CAS race taught us
//!
//! `gc/src/g1.rs` `SharedEvac::evacuate` had a bug fixed on dev `d1d026ad3`
//! (2026-08-07): the CAS **winner** encoded (`target | MARK_FORWARDED`) but the
//! **loser** returned the raw word `compare_exchange` handed back:
//!
//! ```text
//!     Err(winner) => Some((winner as *mut u8, false)),   // address + 3
//! ```
//!
//! Every reference the loser wrote was the real address plus the 2-bit tag.
//! Two arms of one function disagreed about *what type of word*
//! `compare_exchange` returns. It needed a diamond-shaped object graph (one
//! child, two parents) to hit at all, so every tree-shaped test passed.
//!
//! Three design rules here follow directly from that:
//!
//! - **The loser unpacks through the same helper as everyone else.** Every read
//!   of a slot's payload in this module goes through
//!   [`ZgcForwardingEntry::payload`] — the failed-CAS arm, the pre-CAS probe,
//!   `find_payload`, and `snapshot_payloads` all call it. There is no code path
//!   that casts a raw slot word to a payload, let alone to an address, so the
//!   two-arms-disagree shape cannot recur. The [`ZForwardingPayload`] newtype
//!   extends the same guarantee one layer *out*, across the module boundary,
//!   where the bias lives.
//! - **The loser also verifies the key.** G1's slot was *per object*, so a lost
//!   CAS necessarily meant "someone else forwarded my object". Here the slot is
//!   shared by every key that hashes to it, so a lost CAS may mean "someone
//!   else's key landed here". Adopting that winner's `to` would forward our
//!   object to a completely unrelated address. The loser compares the winner's
//!   `from` field and keeps probing on a mismatch.
//! - **Do not panic in a GC worker without a retire guard.** The G1 bug
//!   presented as a 3-hour hang rather than a crash, because the panicking
//!   worker skipped its `outstanding.fetch_sub(1)` and the survivors spun
//!   forever. [`ZForwardingTable::try_insert`] therefore returns a `Result`;
//!   the panicking [`ZForwardingTable::insert`] convenience wrapper is
//!   documented as callable only from a worker whose termination counter is
//!   retired by a `Drop` guard.

use std::sync::atomic::{fence, AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;

use parking_lot::RwLock;
use rustc_hash::{FxHashMap, FxHashSet};

// ===========================================================================
// Entry packing
// ===========================================================================

/// Log2 of the object alignment. Every heap object is 8-byte aligned
/// (`cratonvm_types::HEADER_SIZE % 8 == 0` is a compile-time assertion and
/// every allocator aligns to 8), so the low 3 bits of both the from-offset and
/// the destination payload are known zero and are not stored. The relocator's
/// encoding preserves that alignment by construction — its bias is one grid
/// unit, not one byte.
pub const ZFWD_ALIGN_SHIFT: u32 = 3;

/// Bits reserved for the *page-relative* from-offset, pre-shift.
///
/// 22 bits × 8-byte granularity = 32 MiB, which is exactly
/// `ZPageSizeClass::Medium`'s page size (`ZPAGE_DEFAULT_MEDIUM`,
/// `gc/src/zgc/page.rs`) and the largest page that participates in relocation
/// (Small is 2 MiB; Large is excluded by policy — see
/// [`ZRelocationPolicy::relocate_large_pages`], which also explains why a Large
/// page would be encodable anyway). Keeping `from` *page relative* rather than
/// absolute is what makes 22 bits enough, and is the reason the table is
/// per-page in the first place.
///
/// This field is the one that genuinely is sized by a structure of this tree,
/// which is why it needed no correction when [`ZFWD_TO_BITS`]'s justification
/// turned out to be imported from a collector we are not.
pub const ZFWD_FROM_BITS: u32 = 22;

/// Bits reserved for the destination [`ZForwardingPayload`], pre-shift.
///
/// 41 bits × 8-byte granularity = **16 TiB** of encodable payload.
///
/// # The reason that used to be recorded here was false
///
/// This comment previously justified 41 bits with: *"ZGC's colored-pointer
/// address field is 42 bits (`ZGC_ADDRESS_BITS`, 4 TiB), so this covers the
/// entire ZGC address space with two bits of headroom"*, and described the
/// stored value as "a **full heap address**".
///
/// The number is fine. The argument is wrong, and it is the dangerous kind of
/// wrong: it defends an adequate constant, so it reads as settled and invites a
/// later editor to "simplify" the encoding away on its authority.
///
/// ZGC's 42 bits are an offset into a virtual address space **ZGC itself
/// reserves**, at a base it chose. `ZPageAllocator` (`gc/src/zgc/page.rs`)
/// reserves nothing of the kind: its heap is one owned `Vec<u8>`
/// (`vec![0u8; max_capacity + granule]`, `gc/src/zgc/page.rs`), so every
/// address it hands out is a **raw process address at whatever the platform
/// allocator picked**. There is no ZGC offset here to be 42 bits wide.
///
/// # What that costs on Linux, and why Windows hid it
///
/// glibc routes a `malloc` of the default 256 MiB budget
/// (`ZPAGE_DEFAULT_MAX_CAPACITY`) straight to `mmap`, and Linux places mmap
/// regions in the gap below the stack — around `0x7f00_0000_0000`:
///
/// ```text
///   plausible Linux heap base   0x7f00_0000_0000 = 139_637_976_727_552  ≈ 2^47
///   this field's ceiling        (2^41 - 1) << 3  =  17_592_186_044_408  = 2^44 - 8
///   ratio                                        ≈ 7.94
/// ```
///
/// An *absolute* destination address is therefore ~8× past the field. `pack`
/// would return `None`, [`ZForwardingTable::try_insert`] would return
/// [`ZForwardingInsertError::Unencodable`], and it would do so for **every
/// object**: relocation would not degrade, it would stop dead. Windows places
/// heap allocations low enough that absolute addresses encode fine, so this
/// ships green on a developer box and breaks on the Linux build host. The unit
/// test `linux_shaped_absolute_destination_is_unencodable` pins both halves so
/// the suite can see it on either platform.
///
/// # The bit budget, re-derived honestly
///
/// The fix is not a wider field, it is a **smaller quantity**. What this field
/// holds is a [`ZForwardingPayload`], and the production encoding
/// (`ZRelocate::encode_to`, `gc/src/zgc/relocate.rs`) is
///
/// ```text
///   payload = to_absolute - heap_base + ZRELOCATE_ENCODING_BIAS   (bias = 8)
/// ```
///
/// which is bounded by the **heap**, not by where the OS put it:
///
/// ```text
///   max payload   = ZPageAllocator::max_capacity() + 8
///   field covers  = 2^41 granules × 8 B = 2^44 B = 16 TiB
///   default heap  = 256 MiB = 2^28 B                → uses 25 of the 41 bits
///   headroom      = 16 TiB / 256 MiB = 65_536×
/// ```
///
/// 16 TiB of *heap* is ample — larger than any JVM heap this tree will see, and
/// more headroom than [`ZFWD_FROM_BITS`] has. **The widths do not change; the
/// reason does.** The bias exists because `pack` rejects a zero payload and the
/// first byte of the heap is heap-relative zero; it costs no bits.
///
/// # Two things a future editor must not do
///
/// * **Do not restore absolute addresses** on the grounds that 41 bits covers
///   ZGC's 42-bit offset. It does not cover a raw `mmap` address, and this tree
///   has no reserved virtual space that would make the "offset" reading true.
///   If and when `gc/src/zgc/vaddr.rs` grows a real `ZVirtualAddressSpace` with
///   a low reserved base, absolute and heap-relative coincide, the encoding
///   becomes the identity (`ZRelocateConfig::to_encoding_base = Some(0)`), and
///   the payload newtype stays correct — merely trivial.
/// * **Do not widen this field by borrowing from [`ZFWD_FROM_BITS`]** to make
///   absolute addresses fit. A raw Linux address needs 47 bits, i.e. 44 after
///   the 8-byte shift, and `44 + 22 + 1 = 67 > 64`. No split of one word holds
///   a raw Linux address *and* a 32 MiB page offset *and* an occupancy tag. The
///   encoding is not an optimisation — it is what makes one word sufficient,
///   and one word is what makes publication a single CAS (see the module
///   header).
pub const ZFWD_TO_BITS: u32 = 41;

/// Bit position of the occupancy tag. `1` = this slot holds a published entry.
///
/// An explicit tag rather than "non-zero means occupied": from-offset `0` is a
/// perfectly legal key (the first object on a page), so a zero-valued entry
/// would be indistinguishable from an empty slot on that one key.
///
/// # INVARIANT ZFWD-1: a forwarding entry is never stored in a reference slot
///
/// *Recorded 2026-08-07, from `gc/tests/zgc_module_integration.rs`.*
///
/// This bit is **the same bit** as [`crate::zgc::vaddr::Z_COLORED_TAG`] — bit
/// 63 — and the collision is not an accident of two independent choices that
/// can be un-chosen. Each side needs the top bit for the same structural
/// reason: it is the one bit a 47-bit machine address can never set, so it is
/// the only free tag in a 64-bit word that also has to carry an address-shaped
/// payload. `vaddr` uses it to say "this word is a colored *reference*, not a
/// machine pointer"; this module uses it to say "this slot is *published*".
///
/// That is safe **only** under this invariant:
///
/// > A packed [`ZgcForwardingEntry`] word lives in a
/// > [`ZForwardingTable`] slot and nowhere else. It is never written into a
/// > Java reference field, a root slot, an oop stack cell, or anything else the
/// > load barrier or a GC walker may read as a `vaddr` colored word.
///
/// **What breaks if it is violated.** An occupied entry has bit 63 set, so
/// [`crate::zgc::vaddr::is_colored_word`] reports `true` for it and
/// [`crate::zgc::vaddr::offset_of`] happily returns its low 42 bits — which are
/// the from-offset field with the bottom of the payload field spliced on top.
/// The barrier would then treat a forwarding *entry* as a forwarded *pointer*
/// and hand the mutator an address that is neither the object's old nor its new
/// location. That is the [`ZForwardingPayload`] hazard one level worse: not a
/// wrong pointer off by a known base, but a wrong pointer assembled from two
/// unrelated fields. `an_occupied_entry_is_bit_indistinguishable_from_a_vaddr_colored_word`
/// demonstrates it concretely rather than describing it.
///
/// **The leak vector to watch.** [`ZgcForwardingEntry::pack`] is `pub` and
/// returns a bare `Option<u64>`, so the type system does *not* stop a caller
/// from storing one. Nothing in the tree does today; the const assertion below
/// exists so that anyone who moves either bit is forced to read this paragraph
/// first, and the invariant is stated here so a reviewer of a *new* call site
/// has something to check against.
pub const ZFWD_OCCUPIED_BIT: u64 = 1 << 63;

/// Shift of the payload field within the packed word.
pub const ZFWD_TO_SHIFT: u32 = ZFWD_FROM_BITS;

/// Mask of the from-offset field (already shifted down by [`ZFWD_ALIGN_SHIFT`]).
pub const ZFWD_FROM_MASK: u64 = (1u64 << ZFWD_FROM_BITS) - 1;

/// Mask of the payload field (already shifted down by [`ZFWD_ALIGN_SHIFT`]).
pub const ZFWD_TO_MASK: u64 = (1u64 << ZFWD_TO_BITS) - 1;

/// Largest representable page-relative from-offset, in bytes.
///
/// `32 MiB - 8` — which is **exactly** the highest 8-aligned object offset in a
/// [`crate::zgc::page::ZPAGE_DEFAULT_MEDIUM`] page. See
/// [`ZFWD_MEDIUM_PAGE_HEADROOM_GRANULES`] for why that zero is worth a
/// compile-time gate.
pub const ZFWD_MAX_FROM_OFFSET: u64 = ZFWD_FROM_MASK << ZFWD_ALIGN_SHIFT;

/// The object grid: `1 << ZFWD_ALIGN_SHIFT` bytes. Named so the derivations
/// below read as arithmetic rather than as magic 8s.
pub const ZFWD_ALIGN_BYTES: u64 = 1u64 << ZFWD_ALIGN_SHIFT;

/// How many 8-byte granules of growth [`crate::zgc::page::ZPAGE_DEFAULT_MEDIUM`]
/// has before the top of every medium page becomes unforwardable.
///
/// *Recorded 2026-08-07, from `gc/tests/zgc_module_integration.rs`.*
///
/// **It is zero.** The two numbers were chosen independently — `ZFWD_FROM_BITS
/// = 22` here, `ZPAGE_DEFAULT_MEDIUM = 32 MiB` in `gc/src/zgc/page.rs` — and
/// they meet with no slack at all:
///
/// ```text
///   medium page                     32 MiB       = 33_554_432 B
///   highest 8-aligned object offset 32 MiB - 8   = 33_554_424 B
///   ZFWD_MAX_FROM_OFFSET   (2^22-1) << 3         = 33_554_424 B
///   headroom                                     = 0 granules
/// ```
///
/// One granule of growth in `page.rs` — a medium page of `32 MiB + 8` — makes
/// the last object slot of *every* medium page unencodable. The failure is
/// quiet where it hurts: [`ZForwardingTable::try_insert`] would return
/// [`ZForwardingInsertError::Unencodable`] (with a `tracing::warn!`, which in a
/// suite run nobody reads), the relocation would not be published, and the
/// object's identity would split exactly as the module header describes. It
/// would also be *rare* — only objects in the top granule of a page — which is
/// the worst possible failure profile: it survives every test that does not
/// deliberately fill a page to its last slot.
///
/// So the relationship is a compile-time assertion rather than a comment. If
/// `page.rs` grows the medium page, the build breaks here with the derivation
/// attached; the fix is to grow [`ZFWD_FROM_BITS`] and shrink [`ZFWD_TO_BITS`]
/// by the same amount, because the two must still tile 63 bits.
///
/// **Scope of the gate.** [`crate::zgc::page::ZPageConfig::medium_page_size`]
/// is a runtime field, so a caller can configure a medium page larger than the
/// default and this constant will not see it. The default is what production
/// uses and what a growth commit would edit; a bespoke oversized config is
/// caught at runtime by `Unencodable` and by
/// `forwarding_from_offset_field_exactly_covers_the_default_medium_page`.
pub const ZFWD_MEDIUM_PAGE_HEADROOM_GRANULES: u64 = {
    // Saturating, not wrapping: an overgrown page must trip the assertion
    // below with its explanation, not a bare "attempt to subtract with
    // overflow" from a constant nobody was reading.
    let field_top: u64 = ZFWD_MAX_FROM_OFFSET + ZFWD_ALIGN_BYTES;
    let page: u64 = crate::zgc::page::ZPAGE_DEFAULT_MEDIUM as u64;
    if field_top >= page {
        (field_top - page) / ZFWD_ALIGN_BYTES
    } else {
        0
    }
};

// The from-offset field must reach the last object slot of the largest
// RELOCATABLE page. Large pages are excluded by
// `ZRelocationPolicy::relocate_large_pages` (and hold one object at offset 0
// even if admitted), so Medium is the bound. See
// ZFWD_MEDIUM_PAGE_HEADROOM_GRANULES for the derivation and for what a silent
// violation costs.
const _: () = assert!(
    ZFWD_MAX_FROM_OFFSET + ZFWD_ALIGN_BYTES >= crate::zgc::page::ZPAGE_DEFAULT_MEDIUM as u64,
    "zgc::page's ZPAGE_DEFAULT_MEDIUM has outgrown zgc::forwarding's \
     ZFWD_FROM_BITS: the top objects of every medium page are now unforwardable \
     and try_insert would return Unencodable for them. Grow ZFWD_FROM_BITS and \
     shrink ZFWD_TO_BITS by the same amount (they must still tile 63 bits)."
);

/// Largest representable [`ZForwardingPayload`], `2^44 - 8`.
///
/// This is a bound on the *encoded* value, which under the production encoding
/// is a heap-relative offset — **not** a bound on any absolute address. See
/// [`ZFWD_TO_BITS`] for why the distinction is the whole point, and for what
/// happens on Linux to anyone who forgets it.
pub const ZFWD_MAX_PAYLOAD: u64 = ZFWD_TO_MASK << ZFWD_ALIGN_SHIFT;

/// The empty-slot sentinel. Bit 63 clear ⇒ unoccupied, and every published
/// entry has bit 63 set, so `EMPTY` is the *only* unoccupied bit pattern this
/// module ever stores. That equivalence is what lets `try_insert` use `EMPTY`
/// as the `compare_exchange` expected value.
const ZFWD_EMPTY: u64 = 0;

// The three fields must exactly tile the 64-bit word: one tag bit plus the two
// payload fields. If a future *heap* grows past 16 TiB or a page past 32 MiB,
// this assertion is the thing that must be re-derived first. Note "heap", not
// "address space": the destination field holds a heap-relative payload, and
// ZFWD_TO_BITS explains at length why conflating the two is the defect this
// module was written with.
const _: () = assert!(
    1 + ZFWD_TO_BITS + ZFWD_FROM_BITS == 64,
    "forwarding entry fields must exactly tile a u64 (1 tag + to + from)"
);

// INVARIANT ZFWD-1 (see ZFWD_OCCUPIED_BIT), pinned at compile time.
//
// The occupancy tag and vaddr's colored-word tag are deliberately the same bit
// (63) — both need the one bit a 47-bit machine address cannot set. Moving
// either one does not fix anything by itself, so this assertion is not "these
// must never collide"; it is "the collision is load-bearing and documented, and
// you do not get to change one side quietly". If a future design DOES want the
// two separated, it has to delete this assertion, which means reading the
// invariant on ZFWD_OCCUPIED_BIT and confirming that a forwarding entry still
// never reaches a reference slot.
const _: () = assert!(
    ZFWD_OCCUPIED_BIT == crate::zgc::vaddr::Z_COLORED_TAG,
    "zgc::forwarding's occupancy tag and zgc::vaddr's Z_COLORED_TAG are the same \
     bit by design (63). One of them moved. Re-read INVARIANT ZFWD-1 on \
     ZFWD_OCCUPIED_BIT before changing this assertion: an occupied entry that \
     reaches a reference slot reads as a well-formed colored word."
);
const _: () = assert!(
    ZFWD_OCCUPIED_BIT == 1u64 << 63,
    "the occupancy tag must be the top bit — the payload and from fields tile \
     bits 0..=62 and no machine address sets bit 63"
);

/// The destination half of a forwarding entry: an **opaque encoded word, not an
/// address**.
///
/// # Why this is a newtype and not a `u64`
///
/// The value stored in a slot is whatever the relocator's encoder produced. In
/// this tree that is `to_absolute - heap_base + 8`
/// (`ZRelocate::encode_to` in `gc/src/zgc/relocate.rs`), because a raw process
/// address does not fit the 41-bit field on Linux — [`ZFWD_TO_BITS`] has the
/// full derivation. A payload therefore *differs from the address it stands
/// for by a large constant*, which makes it a **wrong pointer that looks
/// plausible**.
///
/// That is precisely the fixed G1 `address + 3` defect (dev `d1d026ad3`), which
/// hid for months and presented as a 3-hour hang. G1's version was one function
/// whose two arms disagreed about what kind of word they held; the version
/// available here is worse, because the disagreement would be *between modules*
/// — a load-barrier author reading `forwarding.rs` in isolation has no way to
/// know an encoding exists.
///
/// So it is not left to documentation. `find_payload`, `try_insert`, `insert`
/// and `snapshot_payloads` all traffic in this type, so treating a stored value
/// as an address does not compile until someone writes [`Self::encoded`] and
/// takes responsibility for it.
///
/// # What this type does *not* do
///
/// It does not know the encoding. It cannot validate a payload, cannot decode
/// one, and deliberately implements neither arithmetic nor `Deref`. This module
/// stays independent of the page module and of the relocator (see the module
/// header); the encoding belongs to whoever chose the base.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ZForwardingPayload(u64);

impl ZForwardingPayload {
    /// Wrap an already-encoded word.
    ///
    /// The caller asserts that `encoded` came out of *its* encoder. Passing an
    /// absolute address here is the bug this type exists to prevent, and on
    /// Linux it will at least fail loudly ([`ZForwardingInsertError::Unencodable`])
    /// rather than silently — see [`ZFWD_TO_BITS`].
    #[inline]
    pub const fn from_encoded(encoded: u64) -> Self {
        Self(encoded)
    }

    /// The encoded word. **Not an address**; decode it before dereferencing.
    #[inline]
    pub const fn encoded(self) -> u64 {
        self.0
    }

    /// True for the one payload value [`ZgcForwardingEntry::pack`] refuses.
    ///
    /// Zero is rejected so that a decoded occupied entry can never yield a null
    /// destination. It is also why the relocator biases its encoding by 8: the
    /// first byte of the heap is heap-relative zero, and it is a perfectly legal
    /// relocation target.
    #[inline]
    pub const fn is_null(self) -> bool {
        self.0 == 0
    }
}

/// Hand-written so a payload can never be mistaken for an address in a log
/// line: the word "payload" is part of the rendering.
impl std::fmt::Debug for ZForwardingPayload {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "payload({:#x})", self.0)
    }
}

/// Typed view over one packed forwarding-table slot value.
///
/// ```text
///  63          62                     22 21                    0
/// +-----------+------------------------+-----------------------+
/// | OCCUPIED  |    payload >> 3 (41b)  | from_offset >> 3 (22b)|
/// +-----------+------------------------+-----------------------+
/// ```
///
/// This is a *namespace*, not a stored type — the table stores bare
/// `AtomicU64`. Every unpack in this module goes through these associated
/// functions, which is the structural defence against the G1
/// winner-encodes-loser-doesn't defect described in the module header.
pub struct ZgcForwardingEntry;

impl ZgcForwardingEntry {
    /// Pack a `(from_offset, payload)` pair into one publishable word.
    ///
    /// Returns `None` if either value is not 8-byte aligned or does not fit its
    /// field, or if the payload is zero. Zero is rejected because it is the
    /// value a null destination would decode to under an identity encoding, and
    /// rejecting it means a published occupied entry can never carry one.
    ///
    /// **The payload must already be encoded.** Handing this an absolute
    /// address is the cross-module trap [`ZForwardingPayload`] documents; on
    /// Linux it returns `None` for every object, on Windows it silently
    /// "works".
    #[inline]
    pub fn pack(from_offset: u64, payload: ZForwardingPayload) -> Option<u64> {
        let raw = payload.encoded();
        if raw == 0 {
            return None;
        }
        let align_mask = (1u64 << ZFWD_ALIGN_SHIFT) - 1;
        if (from_offset & align_mask) != 0 || (raw & align_mask) != 0 {
            return None;
        }
        let f = from_offset >> ZFWD_ALIGN_SHIFT;
        let t = raw >> ZFWD_ALIGN_SHIFT;
        if f > ZFWD_FROM_MASK || t > ZFWD_TO_MASK {
            return None;
        }
        Some(ZFWD_OCCUPIED_BIT | (t << ZFWD_TO_SHIFT) | f)
    }

    /// True if the slot holds a published entry.
    #[inline]
    pub fn is_occupied(entry: u64) -> bool {
        (entry & ZFWD_OCCUPIED_BIT) != 0
    }

    /// Decode the from-offset (page-relative, bytes).
    #[inline]
    pub fn from(entry: u64) -> u64 {
        (entry & ZFWD_FROM_MASK) << ZFWD_ALIGN_SHIFT
    }

    /// Unpack the destination payload.
    ///
    /// **This is the only way any caller may obtain a stored payload**, and a
    /// payload is still not an address — see [`ZForwardingPayload`]. The
    /// failed-`compare_exchange` arm in [`ZForwardingTable::try_insert`] routes
    /// through here exactly like the success arm, because the whole G1
    /// evacuation defect was one arm forgetting to unpack.
    #[inline]
    pub fn payload(entry: u64) -> ZForwardingPayload {
        ZForwardingPayload::from_encoded(
            ((entry >> ZFWD_TO_SHIFT) & ZFWD_TO_MASK) << ZFWD_ALIGN_SHIFT,
        )
    }

    /// The raw (pre-shift) key field, for slot comparisons without a decode.
    #[inline]
    fn key_bits(entry: u64) -> u64 {
        entry & ZFWD_FROM_MASK
    }
}

/// splitmix64's finalizer. The keys are 8-byte-aligned offsets shifted down by
/// 3, so they are dense small integers with no entropy in the high bits;
/// masking them straight into a power-of-two table would cluster every object
/// on a page into a contiguous run and turn linear probing into a linear scan.
/// This mixes in ~5 instructions with no table and no dependency.
#[inline]
fn zfwd_mix(key: u64) -> u64 {
    let mut z = key.wrapping_add(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

// ===========================================================================
// ZForwardingTable
// ===========================================================================

/// Why a [`ZForwardingTable::try_insert`] could not publish.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ZForwardingInsertError {
    /// `from` and/or the payload do not fit the packed entry: misaligned, out
    /// of range for the field widths, or a zero payload. A caller bug — the
    /// page-size and heap-size bounds that make the fields sufficient are
    /// documented on [`ZFWD_FROM_BITS`] / [`ZFWD_TO_BITS`].
    ///
    /// **The most likely cause is an unencoded destination.** A caller that
    /// passes a raw process address instead of a [`ZForwardingPayload`] from its
    /// own encoder gets this for every object on Linux (heap base ~2^47 against
    /// a 2^44 field) and gets away with it on Windows. [`ZFWD_TO_BITS`] has the
    /// arithmetic; if this variant appears in a log, check the encoder first.
    Unencodable,
    /// Every slot in the table was probed and none was free or matching. Only
    /// reachable if the table was sized from a live-object count that
    /// undercounted reality; see [`ZForwardingTable::with_capacity_for`].
    TableFull,
}

impl std::fmt::Display for ZForwardingInsertError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unencodable => write!(f, "forwarding entry is not encodable"),
            Self::TableFull => write!(f, "forwarding table is full"),
        }
    }
}

/// Smallest table we ever build. A page with one live object still gets a
/// handful of slots so the probe loop has room to disambiguate and the
/// `TableFull` path stays unreachable in practice.
pub const ZFWD_MIN_CAPACITY: usize = 8;

/// Largest table we ever build: 8 Mi entries = 64 MiB of side table.
///
/// The clamp is unreachable for a legal page: the largest relocatable page is
/// 32 MiB (Medium) and the smallest object is `cratonvm_types::HEADER_SIZE`,
/// which is **16** bytes as of the 2026-08-06 header shrink, giving at most
/// 2 Mi live objects → 4 Mi requested → 4 Mi capacity, still under the clamp.
/// It exists so a corrupt live count cannot request a terabyte allocation.
///
/// (**16 is the value, not the doc comment.** `types/src/heap_types.rs`'s own
/// comment on `HEADER_SIZE` still describes *"the 32 -> 24 shrink of
/// 2026-08-06"* above a constant that reads `16`, and `gc/src/heap.rs`'s module
/// header still says *"`HEADER_SIZE` is 32 today"*. Neither is this module's to
/// fix; both are why the derivation below reads the constant instead of any
/// prose about it.)
///
/// Note this is the tightest the arithmetic has ever been: at the pre-shrink
/// 24 bytes the worst case was ~1.4 Mi objects → 4 Mi capacity, and at 16 it is
/// exactly 4 Mi. A further shrink to 8 would land on 8 Mi — *at* the clamp, not
/// under it — so the clamp stops being unreachable and starts being a silent
/// `TableFull` risk.
///
/// # 2026-08-07: "re-derive here" is now the compiler's job
///
/// The paragraph above used to end *"Re-derive here if `HEADER_SIZE` moves
/// again"* — an instruction to a human, which is the same class of guard as the
/// one that let `remembered.rs` ship believing `HEADER_SIZE` was 24. The
/// argument is now evaluated from [`cratonvm_types::HEADER_SIZE`] and
/// [`crate::zgc::page::ZPAGE_MIN_ALLOC`] at compile time — see
/// [`ZFWD_WORST_CASE_CAPACITY`] and the assertion beneath it — so the *next*
/// header change fails the build with the derivation attached instead of
/// silently invalidating a prose argument that still reads as settled.
pub const ZFWD_MAX_CAPACITY: usize = 1 << 23;

/// The smallest object a page can hold, symbolically.
///
/// `page.rs` clamps every allocation up to [`crate::zgc::page::ZPAGE_MIN_ALLOC`]
/// (which is [`cratonvm_types::HEADER_SIZE`] today), so either constant bounds
/// objects-per-page. The smaller of the two is taken rather than one of them,
/// because they are owned by different modules and this derivation must stay
/// conservative if they ever diverge: a *smaller* minimum means *more* objects,
/// which is the direction that could overrun [`ZFWD_MAX_CAPACITY`].
const ZFWD_SMALLEST_OBJECT_BYTES: usize = if cratonvm_types::HEADER_SIZE
    < crate::zgc::page::ZPAGE_MIN_ALLOC
{
    cratonvm_types::HEADER_SIZE
} else {
    crate::zgc::page::ZPAGE_MIN_ALLOC
};

/// Slots [`zfwd_capacity_for`] would request for a **fully live** medium page —
/// the worst case a legal page can produce, and the number
/// [`ZFWD_MAX_CAPACITY`]'s "unreachable clamp" claim is about.
///
/// ```text
///   objects   = ZPAGE_DEFAULT_MEDIUM / ZFWD_SMALLEST_OBJECT_BYTES
///   requested = 2 x objects                    (the 0.5 load factor)
///   capacity  = next_power_of_two(requested)
/// ```
///
/// This constant stops at `requested`, deliberately: `next_power_of_two` is not
/// available in a const context here, and it is not needed. `ZFWD_MAX_CAPACITY`
/// is a power of two (asserted below), and for a power-of-two bound
/// `next_power_of_two(r) <= MAX` holds **iff** `r <= MAX`. So comparing
/// `requested` against the clamp is exactly equivalent to comparing the rounded
/// capacity against it, with no rounding to evaluate.
///
/// At today's numbers: `32 MiB / 16 = 2 Mi` objects → `4 Mi` requested →
/// `4 Mi` capacity, against a `8 Mi` clamp. The claim holds with one doubling
/// of margin — which is `HEADER_SIZE` going from 16 to 8 and no further.
pub const ZFWD_WORST_CASE_CAPACITY: usize =
    2 * (crate::zgc::page::ZPAGE_DEFAULT_MEDIUM / ZFWD_SMALLEST_OBJECT_BYTES);

// The clamp must stay unreachable for a legal page. See ZFWD_WORST_CASE_CAPACITY
// for why comparing the pre-rounding request is equivalent to comparing the
// rounded capacity.
const _: () = assert!(
    ZFWD_MAX_CAPACITY & (ZFWD_MAX_CAPACITY - 1) == 0,
    "ZFWD_MAX_CAPACITY must be a power of two, or the ZFWD_WORST_CASE_CAPACITY \
     derivation (next_power_of_two(r) <= MAX iff r <= MAX) does not hold"
);
const _: () = assert!(
    ZFWD_WORST_CASE_CAPACITY <= ZFWD_MAX_CAPACITY,
    "a fully-live medium page now wants more forwarding slots than \
     ZFWD_MAX_CAPACITY allows, so the clamp would silently UNDER-SIZE the table \
     and turn legal relocations into TableFull. Either the object header shrank \
     (cratonvm_types::HEADER_SIZE / zgc::page::ZPAGE_MIN_ALLOC) or the medium \
     page grew (zgc::page::ZPAGE_DEFAULT_MEDIUM). Raise ZFWD_MAX_CAPACITY, and \
     re-read its doc comment: the clamp exists so a CORRUPT live count cannot \
     request a terabyte, not to bound a legal page."
);

/// Slot count [`ZForwardingTable::with_capacity_for`] will choose for a page
/// with `live_objects` live objects: `next_power_of_two(2 × live_objects)`,
/// clamped to `[ZFWD_MIN_CAPACITY, ZFWD_MAX_CAPACITY]`.
///
/// Exposed separately from the constructor so callers (and tests) can reason
/// about the memory a relocation set will cost without allocating it.
pub fn zfwd_capacity_for(live_objects: usize) -> usize {
    let requested = live_objects.saturating_mul(2);
    let rounded = requested
        .checked_next_power_of_two()
        .unwrap_or(ZFWD_MAX_CAPACITY);
    rounded.clamp(ZFWD_MIN_CAPACITY, ZFWD_MAX_CAPACITY)
}

/// Lock-free, open-addressed, insert-only hash table mapping one page's
/// `from_offset` → [`ZForwardingPayload`].
///
/// # The table stores payloads, not addresses
///
/// Read [`ZForwardingPayload`] and [`ZFWD_TO_BITS`] before using this type from
/// a new call site. In one line: what comes out of [`Self::find_payload`] is
/// the relocator's *encoded* destination, off by the heap base, and
/// dereferencing it without `ZRelocate::decode_to` is a wrong pointer that
/// looks plausible. The types make that a compile error rather than a hang;
/// this paragraph exists so nobody has to discover it from a compile error.
///
/// # Concurrency contract
///
/// Every operation is lock-free and safe from any number of mutator and GC
/// threads simultaneously. Entries are **never removed or overwritten**: once a
/// slot is published it holds that `(from, to)` pair until the whole table is
/// dropped at the end of the cycle. That immutability is load-bearing in three
/// places — it is why `find_payload` may stop at the first empty slot (no
/// tombstones),
/// why a probe run can never be broken by a concurrent writer, and why a reader
/// that observed a `to` address can rely on it forever.
///
/// # Sizing and load factor
///
/// [`ZForwardingTable::with_capacity_for`] rounds `2 × live_objects` up to a
/// power of two, i.e. a **target load factor of 0.5** — the same rule OpenJDK
/// ZGC's `ZForwarding` uses. Linear probing's expected probe count is
/// `(1 + 1/(1-α)²)/2`, which is 2.5 at α = 0.5 and 25.5 at α = 0.9: the
/// degradation past ~0.7 is steep enough that halving the load factor is much
/// cheaper than the probing it saves, and the memory is transient (one cycle).
///
/// # Probe limit — why this cannot livelock
///
/// The capacity is a power of two and probing steps by exactly 1 modulo the
/// capacity (`idx = (idx + 1) & mask`, where `mask == capacity - 1`), so a
/// probe sequence visits **every slot exactly once** and
/// then terminates. Both `find_payload` and `try_insert` bound their loop by
/// `capacity` iterations; there is no retry-from-the-top, no unbounded CAS
/// loop, and no backoff. A lost CAS advances the probe (the slot is now
/// occupied by someone, and we re-examine it once at most). Worst case is
/// `O(capacity)` work and then `TableFull` / `None` — never a spin.
pub struct ZForwardingTable {
    /// One packed `(from, to)` per slot; [`ZFWD_EMPTY`] when unpublished.
    entries: Box<[AtomicU64]>,
    /// `capacity - 1`. Capacity is always a power of two.
    mask: usize,
    /// The from-space page this table describes. Diagnostics only.
    page_id: u64,
    /// Successful publications (CAS winners). Diagnostics only; a lost CAS is
    /// deliberately *not* counted, so this is the number of distinct relocated
    /// objects rather than the number of `insert` calls.
    published: AtomicUsize,
}

impl ZForwardingTable {
    /// Build a table sized for `live_objects` entries at a 0.5 load factor.
    ///
    /// Capacity is `next_power_of_two(2 × live_objects)`, clamped to
    /// `[ZFWD_MIN_CAPACITY, ZFWD_MAX_CAPACITY]`.
    pub fn with_capacity_for(live_objects: usize) -> Self {
        Self::for_page(0, live_objects)
    }

    /// [`Self::with_capacity_for`], tagged with the source page id for logging.
    pub fn for_page(page_id: u64, live_objects: usize) -> Self {
        let capacity = zfwd_capacity_for(live_objects);
        debug_assert!(capacity.is_power_of_two());

        let mut v: Vec<AtomicU64> = Vec::with_capacity(capacity);
        for _ in 0..capacity {
            v.push(AtomicU64::new(ZFWD_EMPTY));
        }

        tracing::debug!(
            target: "zgc",
            page_id = page_id,
            live_objects = live_objects,
            capacity = capacity,
            slot_bytes = capacity * 8,
            "ZForwardingTable: allocated forwarding slots for a relocating page"
        );

        Self {
            entries: v.into_boxed_slice(),
            mask: capacity - 1,
            page_id,
            published: AtomicUsize::new(0),
        }
    }

    /// Total slots. Always a power of two.
    #[inline]
    pub fn capacity(&self) -> usize {
        self.entries.len()
    }

    /// The page id this table was built for (0 if unset).
    #[inline]
    pub fn page_id(&self) -> u64 {
        self.page_id
    }

    /// Number of distinct objects successfully forwarded through this table.
    #[inline]
    pub fn entry_count(&self) -> usize {
        self.published.load(Ordering::Relaxed)
    }

    /// Current load factor, for logging and for asserting the sizing rule.
    pub fn load_factor(&self) -> f64 {
        self.entry_count() as f64 / self.capacity() as f64
    }

    /// Publish `from → payload`, returning **the winning payload**.
    ///
    /// `payload` must already be encoded by the caller's encoder — see
    /// [`ZForwardingPayload`]. This table never encodes, decodes or validates
    /// one; it only checks that the word fits the field.
    ///
    /// # The return-the-winner contract (read this before calling)
    ///
    /// The returned payload is **not necessarily the one you passed in**. If
    /// another thread published this same `from` first, you get *its* payload
    /// and your own destination copy is now garbage — abandon it (see the
    /// module header, step 6). Callers must therefore write:
    ///
    /// ```text
    ///     let winner = table.try_insert(from, my_copy_encoded)?;
    ///     // decode and use `winner`, NOT `my_copy`, from here on
    /// ```
    ///
    /// Using `my_copy` after a lost race splits the object's identity: two
    /// threads would install references to two different copies of the same
    /// Java object, and `a == b` would go false for what must be one object.
    /// This is the single most important invariant in the module.
    ///
    /// # Atomic orderings
    ///
    /// - **Probe load: `Relaxed`.** The probe only compares *key bits*; it
    ///   consumes no data through a non-matching slot, so no synchronisation is
    ///   required to look at one. On AArch64 this is a plain `LDR` instead of
    ///   an `LDAR` per probe step, which matters because probing is the fast
    ///   path.
    /// - **On a key match: `fence(Acquire)` before unpacking.** An acquire fence
    ///   *after* a relaxed load that read a release store establishes the same
    ///   happens-before as an acquire load would. We need it: the winner
    ///   byte-copied the object *before* its release-CAS, and we are about to
    ///   hand out a payload the caller will decode and dereference. Without it,
    ///   ARM could let us return a destination whose contents are still
    ///   uninitialised to-space. One fence at the single matching slot beats an
    ///   acquire on every probe step.
    /// - **Publishing CAS: success `AcqRel`.** The `Release` half is the
    ///   essential one — it orders our `copy_nonoverlapping` of the object
    ///   *before* the slot becomes visible, so anyone who acquires the entry
    ///   sees a fully-written object. The `Acquire` half costs nothing on x86
    ///   (`LOCK CMPXCHG` is already a full barrier) and keeps the RMW
    ///   symmetric with the `mark_bitmap`/G1 convention in this crate.
    /// - **Publishing CAS: failure `Acquire`.** On a loss we adopt the
    ///   winner's address and will dereference the winner's copy, so the
    ///   failed CAS must synchronise-with the winner's release exactly as the
    ///   match path does. This is where a `Relaxed` failure ordering would be
    ///   a real, ARM-only, once-in-a-billion heap corruption.
    /// - **`published` counter: `Relaxed`.** Diagnostics; no data is published
    ///   through it.
    pub fn try_insert(
        &self,
        from: u64,
        payload: ZForwardingPayload,
    ) -> Result<ZForwardingPayload, ZForwardingInsertError> {
        let desired = match ZgcForwardingEntry::pack(from, payload) {
            Some(d) => d,
            None => {
                tracing::warn!(
                    target: "zgc",
                    page_id = self.page_id,
                    from = from,
                    payload = payload.encoded(),
                    max_from = ZFWD_MAX_FROM_OFFSET,
                    max_payload = ZFWD_MAX_PAYLOAD,
                    "ZForwardingTable::try_insert: unencodable entry (misaligned, \
                     out of field range, or null payload) — if the payload looks \
                     like a raw process address, the caller skipped its encoder; \
                     see ZFWD_TO_BITS"
                );
                return Err(ZForwardingInsertError::Unencodable);
            }
        };
        let key = from >> ZFWD_ALIGN_SHIFT;
        let capacity = self.entries.len();
        let mut idx = (zfwd_mix(key) as usize) & self.mask;

        for _ in 0..capacity {
            let slot = &self.entries[idx];
            let observed = slot.load(Ordering::Relaxed);

            if ZgcForwardingEntry::is_occupied(observed) {
                if ZgcForwardingEntry::key_bits(observed) == key {
                    // Somebody already forwarded this object. Acquire-fence
                    // before we hand out their destination (see above), then
                    // unpack through the shared helper.
                    fence(Ordering::Acquire);
                    return Ok(ZgcForwardingEntry::payload(observed));
                }
                // Different key colliding in this slot — keep probing.
            } else {
                // `ZFWD_EMPTY` is the only unoccupied pattern we ever store,
                // so it is the correct expected value here.
                match slot.compare_exchange(
                    ZFWD_EMPTY,
                    desired,
                    Ordering::AcqRel,
                    Ordering::Acquire,
                ) {
                    Ok(_) => {
                        self.published.fetch_add(1, Ordering::Relaxed);
                        return Ok(payload);
                    }
                    Err(winner) => {
                        // G1 LESSON (dev d1d026ad3): the loser must UNPACK the
                        // word `compare_exchange` hands back, never adopt it
                        // verbatim — and here it must ALSO check the key,
                        // because unlike G1's per-object mark word this slot is
                        // shared by every colliding key. Adopting a colliding
                        // winner's payload would forward our object to an
                        // unrelated address.
                        if ZgcForwardingEntry::is_occupied(winner)
                            && ZgcForwardingEntry::key_bits(winner) == key
                        {
                            // The failure ordering above already supplied the
                            // Acquire that pairs with the winner's Release.
                            return Ok(ZgcForwardingEntry::payload(winner));
                        }
                        // Lost the slot to a different key: fall through and
                        // probe on. We re-examine this slot at most once.
                    }
                }
            }

            idx = (idx + 1) & self.mask;
        }

        tracing::error!(
            target: "zgc",
            page_id = self.page_id,
            capacity = capacity,
            published = self.published.load(Ordering::Relaxed),
            "ZForwardingTable: table full after probing every slot — the page's \
             live-object count was undercounted when the table was sized"
        );
        Err(ZForwardingInsertError::TableFull)
    }

    /// [`Self::try_insert`], panicking on failure.
    ///
    /// Returns the **winning** payload — see [`Self::try_insert`] for the
    /// return-the-winner contract, which is the reason this method exists in
    /// the shape the barrier wants (a bare payload, not a `Result`).
    ///
    /// # Panics
    ///
    /// On [`ZForwardingInsertError`]. Both variants are logic errors (a
    /// mis-sized table or an out-of-range address), and the alternative —
    /// returning the caller's own `to` — would silently break the single-winner
    /// invariant and split an object's identity, which is strictly worse than a
    /// loud failure.
    ///
    /// **Caller requirement:** a GC worker that calls this must retire its
    /// termination counter from a `Drop` guard. The fixed G1 evacuation defect
    /// turned a worker panic into a 3-hour hang precisely because the retire
    /// was a statement after the fallible call rather than a `Drop`; see
    /// `RetireOnExit` in `gc/src/g1.rs`.
    pub fn insert(&self, from: u64, payload: ZForwardingPayload) -> ZForwardingPayload {
        match self.try_insert(from, payload) {
            Ok(winner) => winner,
            Err(e) => {
                let raw = payload.encoded();
                panic!(
                    "ZForwardingTable::insert failed for page {} \
                     ({from:#x} -> payload {raw:#x}): {e}",
                    self.page_id
                )
            }
        }
    }

    /// Lock-free lookup of a published **payload**. `None` means "no thread has
    /// published this object's relocation *yet*" — which is a legitimate
    /// observation, not an error: the caller then races to relocate it via
    /// [`Self::try_insert`].
    ///
    /// # The name is a warning
    ///
    /// It is `find_payload`, not `find`, because the value it returns is not an
    /// address and must go through the relocator's decoder
    /// (`ZRelocate::decode_to`, or the wrappers `ZRelocate::forward` /
    /// `forward_lookup`) before anyone dereferences it. The return **type**
    /// already enforces that; the **name** is what makes the hazard visible to
    /// a grep and to a reviewer reading a call site out of context. See
    /// [`ZForwardingPayload`].
    ///
    /// # Why stopping at the first empty slot is correct
    ///
    /// The table is insert-only, so a slot never reverts to empty and there are
    /// no tombstones. An inserter always claims the *first* empty slot in its
    /// probe run, so once key `K` is published, every slot from `hash(K)` up to
    /// `K`'s slot is occupied and stays occupied. A reader that hits an empty
    /// slot before finding `K` has therefore proved `K` is not (yet) present.
    ///
    /// # Ordering
    ///
    /// `Relaxed` probe loads plus a single `Acquire` fence on the match, for
    /// exactly the reasons spelled out on [`Self::try_insert`]: the fence is
    /// what makes the destination the returned payload names safe to
    /// dereference once decoded.
    pub fn find_payload(&self, from: u64) -> Option<ZForwardingPayload> {
        let align_mask = (1u64 << ZFWD_ALIGN_SHIFT) - 1;
        if (from & align_mask) != 0 {
            return None;
        }
        let key = from >> ZFWD_ALIGN_SHIFT;
        if key > ZFWD_FROM_MASK {
            return None;
        }
        let capacity = self.entries.len();
        let mut idx = (zfwd_mix(key) as usize) & self.mask;

        for _ in 0..capacity {
            let observed = self.entries[idx].load(Ordering::Relaxed);
            if !ZgcForwardingEntry::is_occupied(observed) {
                return None;
            }
            if ZgcForwardingEntry::key_bits(observed) == key {
                fence(Ordering::Acquire);
                return Some(ZgcForwardingEntry::payload(observed));
            }
            idx = (idx + 1) & self.mask;
        }
        None
    }

    /// All published `(from_offset, payload)` pairs, in slot order.
    ///
    /// For the remap phase and for diagnostics. The second element is a
    /// [`ZForwardingPayload`], **not** an address — the same rule as
    /// [`Self::find_payload`], and the reason this is not called `snapshot`. A
    /// remap pass that dumps these straight into a pointer map produces a table
    /// of plausible-looking wrong pointers.
    ///
    /// `Acquire` per slot here (rather than relaxed-plus-fence) because every
    /// occupied slot we return names an object the caller will dereference, and
    /// unlike the probe loops there is no single "matching" slot at which one
    /// fence would cover the whole scan.
    pub fn snapshot_payloads(&self) -> Vec<(u64, ZForwardingPayload)> {
        let mut out: Vec<(u64, ZForwardingPayload)> = Vec::new();
        for slot in self.entries.iter() {
            let e = slot.load(Ordering::Acquire);
            if ZgcForwardingEntry::is_occupied(e) {
                out.push((
                    ZgcForwardingEntry::from(e),
                    ZgcForwardingEntry::payload(e),
                ));
            }
        }
        out
    }
}

impl std::fmt::Debug for ZForwardingTable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ZForwardingTable")
            .field("page_id", &self.page_id)
            .field("capacity", &self.capacity())
            .field("entries", &self.entry_count())
            .finish()
    }
}

// ===========================================================================
// Relocation-set selection
// ===========================================================================

/// Size class of a [`PageCandidate`]: 2 MiB small page.
pub const ZFWD_SIZE_CLASS_SMALL: u8 = 0;
/// Size class of a [`PageCandidate`]: 32 MiB medium page.
pub const ZFWD_SIZE_CLASS_MEDIUM: u8 = 1;
/// Size class of a [`PageCandidate`]: large page, one object, sized to fit.
pub const ZFWD_SIZE_CLASS_LARGE: u8 = 2;

/// Plain-data description of one page, as input to [`ZRelocationSet::select`].
///
/// Deliberately *not* a reference to a page type: this module has no dependency
/// on the page module, so the coupling between page management and relocation
/// stays at "a struct of four integers". Whoever owns pages fills these in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PageCandidate {
    /// Identifies the page. The key used by [`ZForwardingRegistry`].
    pub page_id: u64,
    /// Bytes of live (marked) data on the page. This is what evacuation must
    /// copy, so it is the cost side of the trade.
    pub live_bytes: usize,
    /// The page's **allocated extent** — its bump cursor, i.e. exactly
    /// `ZPageReal::relocation_capacity_bytes()`. `capacity - live` is then the
    /// reclaimable garbage, i.e. the benefit side.
    ///
    /// **Not the page's span.** This field used to be documented as "total
    /// bytes the page spans", and that was the source of a real ambiguity:
    /// bytes above the cursor were never allocated, so they cost nothing to
    /// reclaim. Counting them as garbage lets a page that is sparsely filled
    /// but wholly live clear [`ZRelocationSet::select`]'s zero-garbage filter,
    /// report a low [`Self::live_occupancy`], and sort *above* a genuinely
    /// half-dead page — pure copy, no reclaim, and this selector's own
    /// profitability ranking inverted. See the dated 2026-08-07 note on
    /// `ZPageReal::relocation_capacity_bytes` in `zgc::page` for the full
    /// argument and for why the footprint question it does not answer needs its
    /// own predicate rather than this denominator.
    pub capacity_bytes: usize,
    /// One of [`ZFWD_SIZE_CLASS_SMALL`] / `_MEDIUM` / `_LARGE`.
    pub size_class_index: u8,
}

impl PageCandidate {
    /// Bytes reclaimed if this page is evacuated and freed.
    #[inline]
    pub fn garbage_bytes(&self) -> usize {
        self.capacity_bytes.saturating_sub(self.live_bytes)
    }

    /// Live share of the page in `0.0..=1.0`. A page with zero capacity reports
    /// `1.0` (fully occupied) so it is never selected on a divide-by-zero.
    #[inline]
    pub fn live_occupancy(&self) -> f64 {
        if self.capacity_bytes == 0 {
            return 1.0;
        }
        self.live_bytes as f64 / self.capacity_bytes as f64
    }

    /// True for a large (single-object) page.
    #[inline]
    pub fn is_large(&self) -> bool {
        self.size_class_index == ZFWD_SIZE_CLASS_LARGE
    }
}

/// Default evacuation budget per cycle: 64 MiB of *live* bytes copied.
pub const ZFWD_DEFAULT_MAX_EVACUATION_BYTES: usize = 64 * 1024 * 1024;

/// Default live-occupancy cutoff. Matches
/// [`crate::zgc::ZgcConfig::relocation_threshold`]'s 0.25 so the two selectors
/// agree on which pages are "sparse".
pub const ZFWD_DEFAULT_MAX_LIVE_OCCUPANCY: f64 = 0.25;

/// Knobs governing which pages are worth evacuating.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ZRelocationPolicy {
    /// Ceiling on the total **live bytes** the cycle will copy.
    ///
    /// The budget is on live bytes, not on page count or garbage: evacuation
    /// cost is the byte-copy, and the byte-copy is proportional to live data.
    /// Budgeting on garbage would happily admit a hundred nearly-empty pages
    /// and then again a single dense one at wildly different cost.
    pub max_evacuation_bytes: usize,

    /// A page whose live occupancy is **at or above** this is not evacuated.
    ///
    /// Above the cutoff the copy cost approaches the reclaim benefit and the
    /// page is better left alone until it decays. 0.25 by default: copy 1 byte
    /// to reclaim at least 3.
    pub max_live_occupancy: f64,

    /// A page-id range the last allocation failure needs emptied, inclusive.
    ///
    /// **The profitability ranking below cannot find these pages.** It sorts by
    /// garbage ratio, which is the right question for "reclaim the most bytes
    /// per byte copied" and the wrong one for "make one CONTIGUOUS hole of size
    /// N". A window can be 98.9 % free and still refuse a request, and the few
    /// dense pages walling it are exactly the ones the ranking puts last and
    /// `max_live_occupancy` then filters out entirely.
    ///
    /// Measured on `bug-h2-testkillprocess-zgc-oom-at-97-percent-free-20260821`:
    /// `TestMVStoreTool` OOMs on a 262 160-byte request while 2 888 live bytes
    /// in 49 runs are all that stand between it and 266 104 contiguous bytes —
    /// in a cycle that relocated 1 817 474 objects by this ranking and left
    /// those 34 objects where they were.
    ///
    /// `Arena::frag_profile` already computes that window; this is how it
    /// reaches the selector. Pages in the range bypass the zero-garbage and
    /// occupancy filters (a wholly-live page inside the window is precisely
    /// what must move) and sort ahead of everything else, so the evacuation
    /// budget cannot be spent before reaching them.
    pub target_pages: Option<(u64, u64)>,

    /// Whether large (single-object) pages participate. **`false` by default.**
    ///
    /// A large page in ZGC holds exactly one object and is committed at that
    /// object's size, so its occupancy is binary: the object is live (0%
    /// garbage — nothing to reclaim, and relocating it just copies a huge
    /// object to an identical-sized page) or the object is dead (100% garbage —
    /// the page is freed outright with no evacuation at all). Either way
    /// evacuation buys nothing, while the copy is the most expensive one in the
    /// heap. There is also no internal fragmentation to compact away, because
    /// the page *is* the object.
    ///
    /// # Sourcing — reasoned and cross-checked, not quoted
    ///
    /// The OpenJDK sentence that used to sit here (*"`ZRelocationSetSelector`
    /// maintains `ZRelocationSetSelectorGroup`s for the small and medium
    /// classes only, and routes large pages to the not-selected path"*) is
    /// **recalled, not cited** — no OpenJDK source was read while writing this
    /// module, and it must not be quoted as though it were. Marking it is the
    /// point: an unmarked recollection is indistinguishable from a citation the
    /// next time somebody wants to change the default.
    ///
    /// The first-principles argument above stands on its own and is what
    /// actually justifies the default. It is also corroborated **inside this
    /// tree**, independently of any recollection:
    ///
    /// * `gc/src/zgc/page.rs`'s `ZPageReal::is_relocation_candidate`
    ///   hard-excludes `ZPageSizeClass::Large` with the same reasoning, so a
    ///   Large page cannot enter a relocation set even if this knob is flipped
    ///   *and* the selector admits it — two independent gates, which is the
    ///   right shape for a policy whose violation is expensive rather than
    ///   unsound.
    /// * `ZPageConfig::page_size_for` sizes a Large page to its one object, so
    ///   `garbage_bytes()` on a live Large page is structurally zero and
    ///   [`ZRelocationSet::select`]'s existing zero-garbage filter would drop
    ///   it anyway. The knob is therefore *inert on live pages* regardless of
    ///   its value; flipping it only admits Large pages whose object is
    ///   partially dead, which cannot happen.
    ///
    /// Large pages are also the natural pinning target for JNI critical
    /// regions, which is easier if they never move — `relocate.rs` notes it
    /// covers the common `GetPrimitiveArrayCritical` case by accident rather
    /// than by design, and that remains true.
    ///
    /// One encoding consequence, since it is easy to worry about the wrong
    /// thing: a Large page can exceed [`ZFWD_FROM_BITS`]'s 32 MiB from-offset
    /// range, but it holds exactly **one** object, at offset 0, so its only
    /// possible key is `0` and the field is sufficient even if this knob is
    /// flipped. The 22-bit budget is bounded by the Medium page size, not by
    /// the largest page.
    ///
    /// The knob exists so a future experiment (e.g. defragmenting the *virtual*
    /// address space for a huge long-lived array) can flip it without editing
    /// the selector.
    pub relocate_large_pages: bool,
}

impl Default for ZRelocationPolicy {
    fn default() -> Self {
        Self {
            max_evacuation_bytes: ZFWD_DEFAULT_MAX_EVACUATION_BYTES,
            max_live_occupancy: ZFWD_DEFAULT_MAX_LIVE_OCCUPANCY,
            target_pages: None,
            relocate_large_pages: false,
        }
    }
}

/// The pages chosen for evacuation this cycle, in evacuation order.
#[derive(Debug, Clone, Default)]
pub struct ZRelocationSet {
    pages: Vec<PageCandidate>,
    ids: FxHashSet<u64>,
    total_live_bytes: usize,
    total_garbage_bytes: usize,
    /// Candidates rejected purely because the budget ran out (as opposed to
    /// being filtered on occupancy or size class). Feeds the "we are falling
    /// behind" heuristic a future controller will want.
    deferred_for_budget: usize,
}

/// Pages admitted to a relocation set because they fell in the window an
/// allocation failure named — the ENGAGEMENT counter for targeted compaction.
///
/// A workload-level result is unreadable without it: "the OOM went away" and
/// "no target was ever set" produce the same `compaction_cycles`.
pub static TARGETED_PAGES_SELECTED: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);

/// Read [`TARGETED_PAGES_SELECTED`].
pub fn targeted_pages_selected() -> usize {
    TARGETED_PAGES_SELECTED.load(std::sync::atomic::Ordering::Relaxed)
}

impl ZRelocationSet {
    /// An empty relocation set (no page moves this cycle).
    pub fn empty() -> Self {
        Self::default()
    }

    /// Choose the pages to evacuate.
    ///
    /// # Algorithm
    ///
    /// 1. **Filter.** Drop large pages (unless
    ///    [`ZRelocationPolicy::relocate_large_pages`]), zero-capacity pages,
    ///    pages with no garbage at all, and pages at or above the occupancy
    ///    cutoff.
    /// 2. **Sort by garbage ratio, descending**, tie-broken by ascending
    ///    `page_id`. The ratio comparison is done with `u128`
    ///    cross-multiplication (`a.garbage × b.capacity` vs
    ///    `b.garbage × a.capacity`) rather than by dividing into `f64`: exact,
    ///    total, and deterministic, so the selection is reproducible run to run
    ///    — which matters because a flaky ordering makes every downstream GC
    ///    measurement flaky too.
    /// 3. **Take a prefix** until the next page would exceed
    ///    [`ZRelocationPolicy::max_evacuation_bytes`], then **stop**.
    ///
    /// Step 3 stops rather than skipping ahead to a page that would still fit.
    /// Skipping would be a knapsack, and since the list is already sorted by
    /// profitability, anything it could reach is strictly less profitable per
    /// byte copied than what we just declined. A prefix is also stable: adding
    /// one page to the input cannot reshuffle the pages already chosen.
    ///
    /// A page whose `live_bytes` alone exceeds the whole budget therefore ends
    /// the selection. That is intentional — it is a page we can never afford
    /// under this budget, and the occupancy filter has already established it
    /// is worth moving, so the right response is to raise the budget rather
    /// than to quietly evacuate everything except the page that matters.
    pub fn select(pages: &[PageCandidate], policy: &ZRelocationPolicy) -> ZRelocationSet {
        // Does this page sit in the window the last allocation failure named?
        let targeted = |p: &PageCandidate| -> bool {
            policy
                .target_pages
                .is_some_and(|(lo, hi)| p.page_id >= lo && p.page_id <= hi)
        };

        let mut eligible: Vec<PageCandidate> = Vec::new();
        for p in pages.iter() {
            if p.is_large() && !policy.relocate_large_pages {
                continue;
            }
            if p.capacity_bytes == 0 {
                continue;
            }
            // A TARGETED page is admitted on both of the next two filters. Both
            // ask "is this page worth evacuating for the bytes it reclaims",
            // which is the wrong question for a page whose only job is to stop
            // walling a contiguous window: reclaiming nothing while unblocking a
            // 256 KiB request is the trade being made on purpose.
            if !targeted(p) {
                if p.garbage_bytes() == 0 {
                    // Nothing to reclaim: pure copy cost.
                    continue;
                }
                if p.live_occupancy() >= policy.max_live_occupancy {
                    continue;
                }
            }
            eligible.push(*p);
        }

        // Descending garbage ratio; `page_id` ascending as the deterministic
        // tie-break. u128 keeps the cross-product exact for any plausible page
        // size (a 32 MiB page is 2^25, so the product is ~2^50).
        eligible.sort_by(|a, b| {
            // Targeted pages first, ahead of the profitability ranking. The
            // budget below is a PREFIX rule, so anything sorted after a full
            // budget is silently dropped -- and the whole point of a target is
            // that it must not be the thing dropped.
            let lhs = (a.garbage_bytes() as u128) * (b.capacity_bytes as u128);
            let rhs = (b.garbage_bytes() as u128) * (a.capacity_bytes as u128);
            targeted(b)
                .cmp(&targeted(a))
                .then_with(|| rhs.cmp(&lhs))
                .then_with(|| a.page_id.cmp(&b.page_id))
        });

        let mut selected: Vec<PageCandidate> = Vec::new();
        let mut ids: FxHashSet<u64> = FxHashSet::default();
        let mut live_total: usize = 0;
        let mut garbage_total: usize = 0;
        let mut deferred: usize = 0;

        for p in eligible.iter() {
            if targeted(p) {
                TARGETED_PAGES_SELECTED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }
            if live_total.saturating_add(p.live_bytes) > policy.max_evacuation_bytes {
                // Prefix rule: stop here, count the rest as budget-deferred.
                deferred = eligible.len() - selected.len();
                break;
            }
            live_total += p.live_bytes;
            garbage_total += p.garbage_bytes();
            ids.insert(p.page_id);
            selected.push(*p);
        }

        tracing::debug!(
            target: "zgc",
            candidates = pages.len(),
            eligible = eligible.len(),
            selected = selected.len(),
            deferred = deferred,
            live_bytes = live_total,
            garbage_bytes = garbage_total,
            "ZRelocationSet::select: relocation set chosen"
        );

        ZRelocationSet {
            pages: selected,
            ids,
            total_live_bytes: live_total,
            total_garbage_bytes: garbage_total,
            deferred_for_budget: deferred,
        }
    }

    /// The selected pages, most-profitable first.
    #[inline]
    pub fn pages(&self) -> &[PageCandidate] {
        &self.pages
    }

    /// The selected page ids, in selection order.
    pub fn page_ids(&self) -> Vec<u64> {
        self.pages.iter().map(|p| p.page_id).collect()
    }

    /// Whether `page_id` is being evacuated this cycle. `O(1)`.
    ///
    /// This is the load barrier's step-3 question, so it must not be a linear
    /// scan of `pages()`.
    #[inline]
    pub fn contains(&self, page_id: u64) -> bool {
        self.ids.contains(&page_id)
    }

    /// Number of pages selected.
    #[inline]
    pub fn len(&self) -> usize {
        self.pages.len()
    }

    /// True if no page will be evacuated this cycle.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.pages.is_empty()
    }

    /// Total live bytes that must be copied. Bounded by
    /// [`ZRelocationPolicy::max_evacuation_bytes`].
    #[inline]
    pub fn total_live_bytes(&self) -> usize {
        self.total_live_bytes
    }

    /// Total bytes reclaimed once the selected pages are freed.
    #[inline]
    pub fn total_garbage_bytes(&self) -> usize {
        self.total_garbage_bytes
    }

    /// Eligible pages left unselected because the budget ran out.
    #[inline]
    pub fn deferred_for_budget(&self) -> usize {
        self.deferred_for_budget
    }
}

// ===========================================================================
// ZForwardingRegistry
// ===========================================================================

/// Average object size assumed when sizing a page's table from its live
/// *bytes* rather than a live *object count*. Deliberately small — a bare
/// `ObjectHeader` is 16 bytes since the 2026-08-06 shrink, so this is only two
/// headers — because underestimating the average overestimates the object
/// count, which oversizes the table: the safe direction. Overestimating would
/// undersize it and risk `TableFull`.
pub const ZFWD_ASSUMED_AVG_OBJECT_BYTES: usize = 32;

/// `page_id` → that page's [`ZForwardingTable`], for every page in the
/// relocation set.
///
/// # Locking discipline — read this before adding a method
///
/// The map is a `parking_lot::RwLock<FxHashMap<..>>` and every lookup
/// **clones the `Arc` and drops the guard before returning**, so the caller
/// does all of its work — allocation, byte-copy, `try_insert`, anything that
/// can block — with **no registry lock held**.
///
/// This is not defensive style, it is a scar. In `native-io/src/net.rs` two
/// sites held a `parking_lot::RwLock` registry read-guard across a per-entry
/// `Mutex` acquire (`net_poll`, fixed `84fa69a55`; `net_configure_blocking`,
/// fixed `06196c15b`), while a third path took the two locks in the opposite
/// order. `parking_lot`'s `RwLock` is **fair**, so as soon as any writer
/// queued, a *new reader* blocked even though a reader already held the lock —
/// and all three threads parked at 0% CPU forever. "Two readers cannot
/// deadlock" is false under a fair lock.
///
/// Concretely, for this registry:
///
/// - The write lock is taken only by [`Self::install`], [`Self::install_for_set`]
///   and [`Self::clear`], each for the duration of a single map operation.
/// - [`Self::get`] takes the read lock, clones one `Arc`, and explicitly
///   `drop`s the guard before returning. Note the `net_poll` trap in
///   particular: a `return f(map.get(..))` inside a `let guard = ...read()`
///   block evaluates `f` **while the guard is still alive**, because locals
///   drop after the return expression. Never call anything through a guard.
/// - No method here ever acquires a second lock.
pub struct ZForwardingRegistry {
    tables: RwLock<FxHashMap<u64, Arc<ZForwardingTable>>>,
}

impl ZForwardingRegistry {
    /// An empty registry (no page is being relocated).
    pub fn new() -> Self {
        Self {
            tables: RwLock::new(FxHashMap::default()),
        }
    }

    /// Install (or replace) the table for `page_id`, returning the `Arc` now
    /// registered.
    pub fn install(&self, page_id: u64, table: Arc<ZForwardingTable>) -> Arc<ZForwardingTable> {
        let handed_back = Arc::clone(&table);
        let mut guard = self.tables.write();
        guard.insert(page_id, table);
        drop(guard);
        handed_back
    }

    /// Build and install a table for every page in `set`.
    ///
    /// Table sizes come from `live_bytes / avg_object_bytes`; pass
    /// [`ZFWD_ASSUMED_AVG_OBJECT_BYTES`] unless the caller has a real live
    /// object count, in which case use [`Self::install`] with
    /// [`ZForwardingTable::for_page`] directly. `avg_object_bytes` is clamped
    /// to at least 8 so a zero can never divide.
    pub fn install_for_set(&self, set: &ZRelocationSet, avg_object_bytes: usize) {
        let avg = avg_object_bytes.max(8);
        // Build every table OUTSIDE the lock — allocating a multi-megabyte
        // slot array under the registry write lock would stall every barrier
        // lookup in the VM for the duration.
        let mut built: Vec<(u64, Arc<ZForwardingTable>)> = Vec::with_capacity(set.len());
        for p in set.pages().iter() {
            let estimate = p.live_bytes / avg;
            built.push((
                p.page_id,
                Arc::new(ZForwardingTable::for_page(p.page_id, estimate)),
            ));
        }
        let mut guard = self.tables.write();
        for (page_id, table) in built.into_iter() {
            guard.insert(page_id, table);
        }
        drop(guard);

        tracing::debug!(
            target: "zgc",
            pages = set.len(),
            "ZForwardingRegistry: installed forwarding tables for the relocation set"
        );
    }

    /// The table for `page_id`, or `None` if that page is not being relocated.
    ///
    /// **The read guard is dropped before this returns** — the caller holds
    /// only an `Arc`. Do every piece of work (allocate, copy, insert) after the
    /// call, never inside a closure passed to something that would hold the
    /// lock. See the type-level locking discipline.
    pub fn get(&self, page_id: u64) -> Option<Arc<ZForwardingTable>> {
        let guard = self.tables.read();
        let found: Option<Arc<ZForwardingTable>> = guard.get(&page_id).cloned();
        drop(guard);
        found
    }

    /// Whether `page_id` has a forwarding table installed.
    pub fn contains(&self, page_id: u64) -> bool {
        let guard = self.tables.read();
        let present = guard.contains_key(&page_id);
        drop(guard);
        present
    }

    /// Number of pages with an installed table.
    pub fn table_count(&self) -> usize {
        let guard = self.tables.read();
        let n = guard.len();
        drop(guard);
        n
    }

    /// Every registered page id (unordered).
    pub fn page_ids(&self) -> Vec<u64> {
        let guard = self.tables.read();
        let ids: Vec<u64> = guard.keys().copied().collect();
        drop(guard);
        ids
    }

    /// Drop every table.
    ///
    /// **Only safe once remapping is complete.** A reference that still carries
    /// a from-space address needs its page's table to be resolvable; clearing
    /// early strands it. The `Arc` means a table already handed out to a
    /// mutator stays alive until that mutator drops it, so `clear` cannot pull
    /// memory out from under an in-flight barrier — but it *can* make a
    /// subsequent [`Self::get`] miss.
    pub fn clear(&self) {
        let mut guard = self.tables.write();
        let n = guard.len();
        guard.clear();
        drop(guard);
        tracing::debug!(
            target: "zgc",
            tables = n,
            "ZForwardingRegistry: cleared (remap complete)"
        );
    }
}

impl Default for ZForwardingRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for ZForwardingRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ZForwardingRegistry")
            .field("tables", &self.table_count())
            .finish()
    }
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Barrier;

    /// Shorthand for "an already-encoded payload of this value".
    ///
    /// Tests are the one place where writing a bare number as a payload is
    /// fine: there is no heap and no encoder, so the identity encoding is the
    /// honest one. Production code must never construct a payload this way —
    /// see [`ZForwardingPayload`] and
    /// `linux_shaped_absolute_destination_is_unencodable`.
    fn p(encoded: u64) -> ZForwardingPayload {
        ZForwardingPayload::from_encoded(encoded)
    }

    // -- packing ------------------------------------------------------------

    #[test]
    fn entry_fields_tile_the_word() {
        assert_eq!(1 + ZFWD_TO_BITS + ZFWD_FROM_BITS, 64);
        // 22 bits of 8-byte granules = 32 MiB, the Medium page size.
        assert_eq!(ZFWD_MAX_FROM_OFFSET, 32 * 1024 * 1024 - 8);
        // 41 bits of 8-byte granules = 16 TiB. NOT "above ZGC's 42-bit
        // colored-pointer field" — that comparison is the false justification
        // ZFWD_TO_BITS documents. 16 TiB is a bound on the HEAP, because what
        // is stored is a heap-relative payload.
        assert_eq!(ZFWD_MAX_PAYLOAD, (1u64 << 44) - 8);
        // The 256 MiB default heap budget uses 25 of the 41 payload bits:
        // 16 TiB / 256 MiB = 65 536x headroom, exactly.
        let default_heap: u64 = 256 * 1024 * 1024;
        assert_eq!(ZFWD_MAX_PAYLOAD + 8, default_heap * 65_536);
    }

    /// FINDING 5 (2026-08-07). **INVARIANT ZFWD-1, demonstrated rather than
    /// described.**
    ///
    /// [`ZFWD_OCCUPIED_BIT`] and [`crate::zgc::vaddr::Z_COLORED_TAG`] are the
    /// same bit. The doc comment on `ZFWD_OCCUPIED_BIT` says why that is safe;
    /// this test shows what it would cost if the invariant it rests on were
    /// broken, so nobody has to take the paragraph on faith.
    #[test]
    fn an_occupied_entry_is_bit_indistinguishable_from_a_vaddr_colored_word() {
        use crate::zgc::vaddr;

        assert_eq!(
            ZFWD_OCCUPIED_BIT,
            vaddr::Z_COLORED_TAG,
            "the two tags are the same bit BY DESIGN; if they have diverged, \
             re-read INVARIANT ZFWD-1 before assuming the divergence is a fix",
        );
        assert_eq!(ZFWD_OCCUPIED_BIT, 1u64 << 63);

        let from: u64 = 0x1_8000;
        let payload = p(0x2_4000);
        let entry = ZgcForwardingEntry::pack(from, payload).expect("a legal entry");
        assert!(ZgcForwardingEntry::is_occupied(entry));

        // (1) vaddr cannot tell a published forwarding entry from one of its
        //     own colored words. This is the hazard, not an accident.
        assert!(
            vaddr::is_colored_word(entry),
            "entry {entry:#x} does NOT look like a vaddr colored word any more; \
             if that is now structural, INVARIANT ZFWD-1 can be relaxed",
        );

        // (2) And what vaddr would decode out of it is neither the source nor
        //     the destination — it is the from field with the bottom of the
        //     payload field spliced on top. A barrier handed this word would
        //     dereference an address nobody ever computed.
        let misread = vaddr::offset_of(entry);
        assert_ne!(
            misread, from,
            "a mis-read entry that happened to equal the from-offset would make \
             this hazard invisible in exactly the test written to show it",
        );
        assert_ne!(misread, payload.encoded());

        // (3) The one entry value that is safe by accident: the empty slot is
        //     Z_NULL, which vaddr treats as null rather than as a colored word.
        //     That is why an all-zero table cannot be misread even if a slot
        //     word escaped.
        assert_eq!(ZFWD_EMPTY, vaddr::Z_NULL);
        assert!(!vaddr::is_colored_word(ZFWD_EMPTY));
    }

    /// FINDING 6 (2026-08-07). The from-offset field has **exactly zero**
    /// granules of headroom over `page.rs`'s medium page.
    ///
    /// The compile-time gate next to [`ZFWD_MEDIUM_PAGE_HEADROOM_GRANULES`]
    /// catches a page that grows. This test states the zero itself, and proves
    /// the boundary at the encoder rather than only in arithmetic — because the
    /// production failure is not an arithmetic one, it is a rare
    /// `Unencodable` on objects in the top granule of a page.
    #[test]
    fn forwarding_from_offset_field_exactly_covers_the_default_medium_page() {
        use crate::zgc::page;

        let medium = page::ZPAGE_DEFAULT_MEDIUM as u64;
        let highest_object = medium - ZFWD_ALIGN_BYTES;

        let from_bits: u32 = ZFWD_FROM_BITS;
        let field_max: u64 = ZFWD_MAX_FROM_OFFSET;
        assert!(
            highest_object <= field_max,
            "page.rs's medium page is {medium} B, whose highest 8-aligned object \
             offset is {highest_object:#x}, but ZFWD_FROM_BITS={from_bits} only \
             reaches {field_max:#x}",
        );
        assert_eq!(
            ZFWD_MEDIUM_PAGE_HEADROOM_GRANULES, 0,
            "the recorded fact is that the headroom is EXACTLY zero. A page that \
             GREW would have tripped the const assertion instead; a page that \
             SHRANK is safe, and the right response is to update \
             ZFWD_MEDIUM_PAGE_HEADROOM_GRANULES' doc comment (which argues from \
             the zero) rather than to delete this assertion.",
        );
        assert_eq!(ZFWD_MAX_FROM_OFFSET + ZFWD_ALIGN_BYTES, medium);

        // At the encoder: the last object slot of a medium page is encodable,
        // one granule past it is refused.
        assert!(
            ZgcForwardingEntry::pack(highest_object, p(0x1000)).is_some(),
            "the last object slot of a medium page must be forwardable",
        );
        assert!(
            ZgcForwardingEntry::pack(medium, p(0x1000)).is_none(),
            "an offset one granule past the page end must be refused, not wrapped",
        );

        // And at the table: refused as Unencodable, and — the part that makes
        // the field width load-bearing rather than cosmetic — it must NOT alias
        // onto offset 0 by truncation.
        let t = ZForwardingTable::for_page(1, 16);
        assert_eq!(
            t.try_insert(medium, p(0x1000)),
            Err(ZForwardingInsertError::Unencodable),
        );
        assert_eq!(
            t.find_payload(0),
            None,
            "an out-of-range key must be refused, never truncated onto offset 0",
        );
        assert_eq!(t.entry_count(), 0);

        // Small pages sit strictly inside the bound, so only Medium is tight.
        assert!(page::ZPAGE_DEFAULT_SMALL as u64 - ZFWD_ALIGN_BYTES < ZFWD_MAX_FROM_OFFSET);
    }

    /// FINDING 7 (2026-08-07). The "unreachable clamp" argument, evaluated from
    /// [`cratonvm_types::HEADER_SIZE`] instead of recited from prose.
    ///
    /// The clamp still holds at `HEADER_SIZE = 16`, but with **one doubling** of
    /// margin: 4 Mi wanted against an 8 Mi clamp. The const assertion beside
    /// [`ZFWD_WORST_CASE_CAPACITY`] is what fails the build if that margin is
    /// spent; this test names the numbers so the failure is legible.
    #[test]
    fn the_capacity_clamp_is_unreachable_at_the_real_header_size() {
        use crate::zgc::page;

        // Read the constant, never a comment about it: heap_types.rs's own doc
        // on HEADER_SIZE still describes a "32 -> 24 shrink" above a 16.
        assert_eq!(cratonvm_types::HEADER_SIZE, 16, "the derivation's premise");
        assert_eq!(
            page::ZPAGE_MIN_ALLOC,
            cratonvm_types::HEADER_SIZE,
            "page.rs's minimum allocation is what bounds objects-per-page; if it \
             stops being HEADER_SIZE, ZFWD_SMALLEST_OBJECT_BYTES takes the \
             smaller of the two and this assertion is the notice to re-read it",
        );
        assert_eq!(ZFWD_SMALLEST_OBJECT_BYTES, cratonvm_types::HEADER_SIZE);

        let max_objects = page::ZPAGE_DEFAULT_MEDIUM / ZFWD_SMALLEST_OBJECT_BYTES;
        assert_eq!(max_objects, 2 * 1024 * 1024, "32 MiB / 16 B");
        assert_eq!(ZFWD_WORST_CASE_CAPACITY, 2 * max_objects);

        // The pre-rounding shortcut must agree with the sizing function that
        // actually runs — that equivalence is the whole reason
        // ZFWD_WORST_CASE_CAPACITY may skip next_power_of_two in a const.
        let capacity = zfwd_capacity_for(max_objects);
        assert_eq!(
            capacity,
            ZFWD_WORST_CASE_CAPACITY.next_power_of_two(),
            "ZFWD_WORST_CASE_CAPACITY's shortcut disagrees with zfwd_capacity_for",
        );
        assert!(capacity <= ZFWD_MAX_CAPACITY);

        // The margin, as a number, so spending it is visible.
        assert_eq!(capacity, 4 * 1024 * 1024);
        assert_eq!(ZFWD_MAX_CAPACITY, 8 * 1024 * 1024);
        assert_eq!(
            ZFWD_MAX_CAPACITY / capacity,
            2,
            "exactly one doubling of margin: HEADER_SIZE may go 16 -> 8 and no \
             further before the clamp starts truncating a LEGAL page's table",
        );

        // The pre-shrink figure the original argument was written against, kept
        // so the reasoning trail survives: at 24 B the worst case was ~1.4 Mi
        // objects, which rounds to the same 4 Mi capacity. The clamp held then
        // too — for a different reason than the one written down.
        assert_eq!(zfwd_capacity_for(page::ZPAGE_DEFAULT_MEDIUM / 24), 4 * 1024 * 1024);
    }

    /// # 2026-08-07: this test's `from` was address-shaped, not offset-shaped
    ///
    /// It was written as `pack(0x1234_5670 & !7, ...)` — 291 MiB — and it had
    /// **never been executed** (nothing in CI built `--features zgc` until
    /// today), so the mistake shipped unseen. `from` is not an address: it is a
    /// *page-relative* offset bounded by [`ZFWD_MAX_FROM_OFFSET`] (32 MiB − 8),
    /// which is the entire reason 22 bits is enough and the table is per-page
    /// (see [`ZFWD_FROM_BITS`]). 291 MiB is not a legal offset into any page
    /// this collector relocates, so `pack` was right to answer `None` and the
    /// `.unwrap()` was the bug. The literal below keeps the old one's digits,
    /// shifted into the field's actual range, so the correction is legible.
    ///
    /// The bound must **not** be loosened to accommodate it: widening `from`
    /// costs bits from [`ZFWD_TO_BITS`], and `1 + 41 + 22 == 64` is already
    /// exact.
    #[test]
    fn pack_round_trips() {
        // 0x0123_4560 = 18.2 MiB: inside a medium page, and wide enough that a
        // shift error in either field would move it.
        const FROM: u64 = 0x0123_4567 & !7;
        assert!(FROM <= ZFWD_MAX_FROM_OFFSET, "the key must be a legal offset");

        let e = ZgcForwardingEntry::pack(FROM, p(0x7FFF_FFF8)).unwrap();
        assert!(ZgcForwardingEntry::is_occupied(e));
        assert_eq!(ZgcForwardingEntry::from(e), FROM);
        assert_eq!(ZgcForwardingEntry::payload(e), p(0x7FFF_FFF8));

        // Both fields at their maxima at once: the only pattern that shows the
        // three fields tile the word with no bleed. `from` reads back without
        // the payload's low bits, the payload reads back without the tag, and
        // the word is exactly all-ones.
        let full =
            ZgcForwardingEntry::pack(ZFWD_MAX_FROM_OFFSET, p(ZFWD_MAX_PAYLOAD)).unwrap();
        assert_eq!(full, u64::MAX);
        assert!(ZgcForwardingEntry::is_occupied(full));
        assert_eq!(ZgcForwardingEntry::from(full), ZFWD_MAX_FROM_OFFSET);
        assert_eq!(ZgcForwardingEntry::payload(full), p(ZFWD_MAX_PAYLOAD));

        // from == 0 is a legal key and must still read back as occupied — this
        // is why the occupancy tag is explicit rather than "non-zero".
        let z = ZgcForwardingEntry::pack(0, p(0x1000)).unwrap();
        assert!(ZgcForwardingEntry::is_occupied(z));
        assert_eq!(ZgcForwardingEntry::from(z), 0);
        assert_eq!(ZgcForwardingEntry::payload(z), p(0x1000));
        assert_ne!(z, 0);
    }

    #[test]
    fn pack_rejects_unencodable() {
        // Misaligned.
        assert!(ZgcForwardingEntry::pack(4, p(0x1000)).is_none());
        assert!(ZgcForwardingEntry::pack(8, p(0x1004)).is_none());
        // Null payload.
        assert!(ZgcForwardingEntry::pack(8, p(0)).is_none());
        assert!(p(0).is_null());
        // from beyond the 32 MiB page bound.
        assert!(ZgcForwardingEntry::pack(ZFWD_MAX_FROM_OFFSET + 8, p(0x1000)).is_none());
        assert!(ZgcForwardingEntry::pack(ZFWD_MAX_FROM_OFFSET, p(0x1000)).is_some());
        // payload beyond 16 TiB.
        assert!(ZgcForwardingEntry::pack(8, p(ZFWD_MAX_PAYLOAD + 8)).is_none());
        assert!(ZgcForwardingEntry::pack(8, p(ZFWD_MAX_PAYLOAD)).is_some());
    }

    /// **The defect this module shipped with, pinned on both platforms.**
    ///
    /// Every other test in this file uses small, Windows-shaped destinations
    /// (`0x9000`, `0x10_0000`, `0x100_0000`) — all of which encode fine
    /// anywhere. That is precisely why a 16 TiB ceiling on what the docs called
    /// a "full heap address" survived review: the suite could not see the
    /// failure, because on Linux the real heap base is ~2^47 and never appeared
    /// in a test.
    ///
    /// This asserts the *documented contract* in both directions:
    ///
    /// 1. a raw Linux-shaped absolute address is refused with `Unencodable` —
    ///    specified behaviour, not a bug, and the reason nothing may pack one;
    /// 2. the same destination expressed heap-relative (exactly what
    ///    `ZRelocate::encode_to` produces) packs, stores and round-trips.
    ///
    /// If someone "simplifies" the encoding away and stores absolute addresses,
    /// (2) fails on every platform and (1) documents why. If someone widens the
    /// field to make absolute addresses fit, `entry_fields_tile_the_word` fails
    /// — 44 + 22 + 1 does not fit a `u64`.
    #[test]
    fn linux_shaped_absolute_destination_is_unencodable() {
        // A plausible placement for a 256 MiB `Vec<u8>` on Linux: glibc routes
        // an allocation that size to `mmap`, and the kernel maps it in the gap
        // below the stack. Granule-aligned, so alignment is not what fails.
        const LINUX_HEAP_BASE: u64 = 0x7f00_0000_0000;
        // ~2^47 against the field's 2^44: eight times too big.
        assert!(LINUX_HEAP_BASE > ZFWD_MAX_PAYLOAD);
        assert!(LINUX_HEAP_BASE / ZFWD_MAX_PAYLOAD >= 7);

        let absolute: u64 = LINUX_HEAP_BASE + 0x2_0000;
        let t = ZForwardingTable::for_page(1, 16);

        // (1) Absolute: refused, loudly, on every platform.
        assert_eq!(ZgcForwardingEntry::pack(0x40, p(absolute)), None);
        assert_eq!(
            t.try_insert(0x40, p(absolute)),
            Err(ZForwardingInsertError::Unencodable),
            "a raw Linux heap address must be refused — see ZFWD_TO_BITS"
        );
        assert_eq!(t.find_payload(0x40), None);
        assert_eq!(t.entry_count(), 0);

        // (2) Heap-relative, biased off zero exactly as `ZRelocate::encode_to`
        //     does. Kept as a literal rather than importing
        //     `relocate::ZRELOCATE_ENCODING_BIAS`, so that a change to the bias
        //     over there has to come back through this file.
        const BIAS: u64 = 8;
        let payload = p(absolute - LINUX_HEAP_BASE + BIAS);
        assert_eq!(t.try_insert(0x40, payload), Ok(payload));
        assert_eq!(t.find_payload(0x40), Some(payload));
        assert_eq!(t.entry_count(), 1);

        // Reconstructing the address is the CALLER's decode. The table neither
        // performs nor validates it — that is the whole content of the
        // `ZForwardingPayload` newtype.
        assert_eq!(payload.encoded() + LINUX_HEAP_BASE - BIAS, absolute);

        // The bias is load-bearing, not cosmetic: the first byte of the heap is
        // heap-relative zero, and a zero payload is rejected.
        assert_eq!(ZgcForwardingEntry::pack(0x48, p(0)), None);
        assert!(ZgcForwardingEntry::pack(0x48, p(BIAS)).is_some());

        // And the whole 256 MiB default heap encodes, at any base.
        let top_of_default_heap = p(256 * 1024 * 1024 - 8 + BIAS);
        assert!(ZgcForwardingEntry::pack(0x50, top_of_default_heap).is_some());
    }

    // -- single-threaded table ---------------------------------------------

    #[test]
    fn insert_find_round_trip() {
        let t = ZForwardingTable::with_capacity_for(64);
        assert!(t.capacity().is_power_of_two());
        assert!(t.capacity() >= 128);

        // 0x400 sits outside the `i * 8` range used below, so the loop cannot
        // collide with it and lose its own CAS.
        assert_eq!(t.find_payload(0x400), None);
        assert_eq!(t.insert(0x400, p(0x9000)), p(0x9000));
        assert_eq!(t.find_payload(0x400), Some(p(0x9000)));
        assert_eq!(t.entry_count(), 1);

        for i in 0u64..32 {
            let from = i * 8;
            let to = p(0x10_0000 + i * 16);
            assert_eq!(t.insert(from, to), to);
        }
        for i in 0u64..32 {
            assert_eq!(t.find_payload(i * 8), Some(p(0x10_0000 + i * 16)));
        }
        assert_eq!(t.find_payload(0x400), Some(p(0x9000)));
        assert_eq!(t.entry_count(), 33);
    }

    #[test]
    fn repeat_insert_returns_the_first_winner() {
        let t = ZForwardingTable::with_capacity_for(16);
        assert_eq!(t.insert(0x80, p(0xAAA0)), p(0xAAA0));
        // A second thread's speculative copy loses; it must get the first
        // winner's payload back, not its own.
        assert_eq!(t.insert(0x80, p(0xBBB0)), p(0xAAA0));
        assert_eq!(t.insert(0x80, p(0xCCC0)), p(0xAAA0));
        assert_eq!(t.find_payload(0x80), Some(p(0xAAA0)));
        // Only one publication was counted.
        assert_eq!(t.entry_count(), 1);
    }

    #[test]
    fn find_rejects_bad_keys_without_probing_forever() {
        let t = ZForwardingTable::with_capacity_for(4);
        assert_eq!(t.find_payload(3), None); // misaligned
        assert_eq!(t.find_payload(ZFWD_MAX_FROM_OFFSET + 8), None); // out of range
        assert_eq!(t.find_payload(0x18), None); // simply absent
    }

    #[test]
    fn try_insert_reports_unencodable() {
        let t = ZForwardingTable::with_capacity_for(4);
        assert_eq!(
            t.try_insert(4, p(0x1000)),
            Err(ZForwardingInsertError::Unencodable)
        );
        assert_eq!(
            t.try_insert(8, p(0)),
            Err(ZForwardingInsertError::Unencodable)
        );
        assert_eq!(
            t.try_insert(ZFWD_MAX_FROM_OFFSET + 8, p(0x1000)),
            Err(ZForwardingInsertError::Unencodable)
        );
        // The out-of-field-range arm, which on Linux is what an unencoded
        // destination hits. See `linux_shaped_absolute_destination_is_unencodable`.
        assert_eq!(
            t.try_insert(8, p(ZFWD_MAX_PAYLOAD + 8)),
            Err(ZForwardingInsertError::Unencodable)
        );
    }

    /// A full table must terminate with `TableFull`, not spin. The probe
    /// sequence visits every slot exactly once (power-of-two capacity, step 1),
    /// so the loop is bounded by `capacity` and cannot livelock.
    #[test]
    fn table_full_terminates_and_still_finds_existing_keys() {
        // live_objects = 1 -> 2 -> clamped up to ZFWD_MIN_CAPACITY.
        let t = ZForwardingTable::with_capacity_for(1);
        assert_eq!(t.capacity(), ZFWD_MIN_CAPACITY);

        for i in 0u64..(ZFWD_MIN_CAPACITY as u64) {
            let from = i * 8;
            let to = p(0x2000 + i * 8);
            assert_eq!(t.try_insert(from, to), Ok(to), "slot {i} should be free");
        }
        assert_eq!(t.entry_count(), ZFWD_MIN_CAPACITY);

        // One more distinct key: every slot is occupied by a different key.
        let overflow_from = (ZFWD_MIN_CAPACITY as u64) * 8;
        assert_eq!(
            t.try_insert(overflow_from, p(0x9000)),
            Err(ZForwardingInsertError::TableFull)
        );
        // ...and it is absent, without hanging.
        assert_eq!(t.find_payload(overflow_from), None);

        // A key that IS present must still resolve on a full table (the probe
        // finds the match before exhausting the slots).
        for i in 0u64..(ZFWD_MIN_CAPACITY as u64) {
            assert_eq!(t.find_payload(i * 8), Some(p(0x2000 + i * 8)));
            assert_eq!(t.try_insert(i * 8, p(0xDEAD_0000)), Ok(p(0x2000 + i * 8)));
        }
    }

    #[test]
    fn sizing_honours_load_factor_and_clamps() {
        // Sizing is asserted through `zfwd_capacity_for` so the clamp cases do
        // not have to allocate a 64 MiB slot array to be covered.
        assert_eq!(zfwd_capacity_for(0), ZFWD_MIN_CAPACITY);
        assert_eq!(zfwd_capacity_for(1), ZFWD_MIN_CAPACITY);
        assert_eq!(zfwd_capacity_for(3), ZFWD_MIN_CAPACITY);
        // 100 live objects -> 200 -> 256 slots -> load factor < 0.5.
        assert_eq!(zfwd_capacity_for(100), 256);
        assert_eq!(zfwd_capacity_for(128), 256);
        assert_eq!(zfwd_capacity_for(129), 512);
        // A corrupt count must clamp rather than request a terabyte, on both
        // the overflow path (`checked_next_power_of_two` -> None) and the
        // merely-too-large path.
        assert_eq!(zfwd_capacity_for(usize::MAX), ZFWD_MAX_CAPACITY);
        assert_eq!(zfwd_capacity_for(ZFWD_MAX_CAPACITY), ZFWD_MAX_CAPACITY);
        // The constructor honours the same rule.
        assert_eq!(ZForwardingTable::with_capacity_for(100).capacity(), 256);
        assert_eq!(
            ZForwardingTable::with_capacity_for(0).capacity(),
            ZFWD_MIN_CAPACITY
        );
    }

    #[test]
    fn snapshot_returns_every_published_pair() {
        let t = ZForwardingTable::with_capacity_for(8);
        for i in 0u64..6 {
            t.insert(i * 8, p(0x5000 + i * 8));
        }
        let mut snap = t.snapshot_payloads();
        snap.sort();
        let expect: Vec<(u64, ZForwardingPayload)> =
            (0u64..6).map(|i| (i * 8, p(0x5000 + i * 8))).collect();
        assert_eq!(snap, expect);
    }

    // -- the concurrency test that matters ---------------------------------

    /// **The load barrier's core race.** N threads all discover the same stale
    /// pointer, all speculatively copy the object to their own destination, and
    /// all call `insert`. Exactly one must win; every thread — winner and
    /// losers alike — must come away with the *same* address, or the object's
    /// identity has split into N copies.
    ///
    /// The `Barrier` releases every thread into `insert` at once so the CAS
    /// actually collides, and the outer loop repeats over many keys so the
    /// window is hit many times per run rather than once.
    #[test]
    fn concurrent_insert_same_key_converges_on_one_winner() {
        const THREADS: u64 = 8;
        const ROUNDS: u64 = 64;

        let table = Arc::new(ZForwardingTable::with_capacity_for(ROUNDS as usize * 2));
        let barrier = Arc::new(Barrier::new(THREADS as usize));

        let mut handles = Vec::new();
        for t in 0..THREADS {
            let table = Arc::clone(&table);
            let barrier = Arc::clone(&barrier);
            handles.push(std::thread::spawn(move || {
                // (winner_payload, did_i_win) per round.
                let mut observed: Vec<(ZForwardingPayload, bool)> =
                    Vec::with_capacity(ROUNDS as usize);
                for r in 0..ROUNDS {
                    let from = r * 8;
                    // Every thread proposes a DIFFERENT destination for the
                    // SAME source, so the returned value identifies the winner.
                    let my_to = p(0x10_0000 + r * 0x1000 + (t + 1) * 8);
                    barrier.wait();
                    let winner = table.insert(from, my_to);
                    observed.push((winner, winner == my_to));
                }
                observed
            }));
        }

        let per_thread: Vec<Vec<(ZForwardingPayload, bool)>> =
            handles.into_iter().map(|h| h.join().unwrap()).collect();

        for r in 0..ROUNDS as usize {
            let first = per_thread[0][r].0;
            let mut winners = 0usize;
            for t in 0..THREADS as usize {
                let (payload, i_won) = per_thread[t][r];
                assert_eq!(
                    payload, first,
                    "round {r}: thread {t} observed {payload:?}, thread 0 observed {first:?} \
                     — the object's identity split"
                );
                if i_won {
                    winners += 1;
                }
            }
            assert_eq!(winners, 1, "round {r}: expected exactly one CAS winner");
            // The published entry must agree with what every thread was told.
            assert_eq!(table.find_payload(r as u64 * 8), Some(first));
        }
        assert_eq!(table.entry_count(), ROUNDS as usize);
    }

    /// Distinct keys inserted concurrently must all be present and correct —
    /// this is the collision-probing half (a lost CAS to a *different* key must
    /// make the loser probe on, not adopt the winner's address).
    #[test]
    fn concurrent_insert_distinct_keys_all_resolve() {
        const THREADS: u64 = 8;
        const PER_THREAD: u64 = 128;

        let total = (THREADS * PER_THREAD) as usize;
        let table = Arc::new(ZForwardingTable::with_capacity_for(total));
        let barrier = Arc::new(Barrier::new(THREADS as usize));

        let mut handles = Vec::new();
        for t in 0..THREADS {
            let table = Arc::clone(&table);
            let barrier = Arc::clone(&barrier);
            handles.push(std::thread::spawn(move || {
                barrier.wait();
                for i in 0..PER_THREAD {
                    let from = (t * PER_THREAD + i) * 8;
                    let to = p(0x100_0000 + from);
                    assert_eq!(table.insert(from, to), to);
                }
            }));
        }
        for h in handles {
            h.join().unwrap();
        }

        for t in 0..THREADS {
            for i in 0..PER_THREAD {
                let from = (t * PER_THREAD + i) * 8;
                assert_eq!(table.find_payload(from), Some(p(0x100_0000 + from)));
            }
        }
        assert_eq!(table.entry_count(), total);
    }

    // -- relocation-set selection ------------------------------------------

    fn small(page_id: u64, live_bytes: usize) -> PageCandidate {
        PageCandidate {
            page_id,
            live_bytes,
            capacity_bytes: 2 * 1024 * 1024,
            size_class_index: ZFWD_SIZE_CLASS_SMALL,
        }
    }

    /// A page the profitability ranking REFUSES is evacuated anyway when an
    /// allocation failure named its window.
    ///
    /// This is the whole feature. `select` sorts by garbage ratio and drops
    /// anything at or above `max_live_occupancy`, which answers "reclaim the
    /// most bytes per byte copied". A window can be 98.9 % free and still
    /// refuse a request, and the pages walling it are dense by construction —
    /// exactly what both filters throw away.
    ///
    /// `bug-h2-testkillprocess-zgc-oom-at-97-percent-free-20260821`:
    /// `TestMVStoreTool` OOMs on a 262 160-byte request with 2 888 live bytes in
    /// 49 runs standing between it and 266 104 contiguous bytes, in a cycle that
    /// relocated 1 817 474 objects by this ranking and left those 34 objects.
    #[test]
    fn a_targeted_page_is_evacuated_even_though_the_ranking_refuses_it() {
        let mb = 1024 * 1024;
        // Page 7 is 90 % live: above `max_live_occupancy`, and the LAST thing
        // the garbage ranking would pick.
        let dense_wall = small(7, (mb * 9) / 10);
        let pages = vec![small(1, mb / 8), dense_wall, small(3, mb / 4)];

        let untargeted = ZRelocationPolicy {
            max_evacuation_bytes: usize::MAX,
            ..ZRelocationPolicy::default()
        };
        let set = ZRelocationSet::select(&pages, &untargeted);
        assert!(
            !set.contains(7),
            "control: the ranking must refuse a 90%-live page, or this test \
             proves nothing"
        );

        let targeted = ZRelocationPolicy {
            max_evacuation_bytes: usize::MAX,
            target_pages: Some((7, 7)),
            ..ZRelocationPolicy::default()
        };
        let set = ZRelocationSet::select(&pages, &targeted);
        assert!(
            set.contains(7),
            "a page in the window an allocation failure named must be evacuated \
             even with nothing to reclaim — unblocking the request IS the benefit"
        );
        assert_eq!(
            set.pages().first().map(|p| p.page_id),
            Some(7),
            "and it must sort FIRST: the evacuation budget is a prefix rule, so \
             a target sorted after a full budget is silently dropped"
        );
    }

    /// The target does not drag in the rest of the arena.
    ///
    /// A bias that admitted everything would "fix" the window by evacuating the
    /// whole heap, which is not a trade anybody chose. Only pages inside the
    /// named range get the exemption.
    #[test]
    fn targeting_one_window_does_not_admit_the_pages_around_it() {
        let mb = 1024 * 1024;
        let pages = vec![
            small(5, (mb * 9) / 10), // dense, OUTSIDE the window
            small(7, (mb * 9) / 10), // dense, inside
            small(9, (mb * 9) / 10), // dense, OUTSIDE
        ];
        let policy = ZRelocationPolicy {
            max_evacuation_bytes: usize::MAX,
            target_pages: Some((7, 7)),
            ..ZRelocationPolicy::default()
        };
        let set = ZRelocationSet::select(&pages, &policy);
        assert!(set.contains(7), "the targeted page is in");
        assert!(!set.contains(5), "page 5 is outside the window and stays out");
        assert!(!set.contains(9), "page 9 is outside the window and stays out");
    }

    #[test]
    fn select_orders_by_garbage_ratio_descending() {
        let mb = 1024 * 1024;
        let pages = vec![
            small(1, mb / 2),  // 25% live
            small(2, mb / 8),  // 6.25% live  <- most garbage
            small(3, mb / 4),  // 12.5% live
        ];
        let policy = ZRelocationPolicy {
            max_live_occupancy: 0.5,
            ..Default::default()
        };
        let set = ZRelocationSet::select(&pages, &policy);
        assert_eq!(set.page_ids(), vec![2, 3, 1]);
        assert_eq!(set.len(), 3);
        assert_eq!(set.total_live_bytes(), mb / 2 + mb / 8 + mb / 4);
        assert!(set.contains(2));
        assert!(!set.contains(99));
    }

    #[test]
    fn select_tie_breaks_on_page_id_for_determinism() {
        let mb = 1024 * 1024;
        let pages = vec![small(7, mb / 8), small(3, mb / 8), small(5, mb / 8)];
        let set = ZRelocationSet::select(&pages, &ZRelocationPolicy::default());
        assert_eq!(set.page_ids(), vec![3, 5, 7]);
    }

    #[test]
    fn select_stops_at_the_budget() {
        // Four pages, 128 KiB live each; budget fits exactly two.
        let pages = vec![
            small(1, 128 * 1024),
            small(2, 128 * 1024),
            small(3, 128 * 1024),
            small(4, 128 * 1024),
        ];
        let policy = ZRelocationPolicy {
            max_evacuation_bytes: 256 * 1024,
            ..Default::default()
        };
        let set = ZRelocationSet::select(&pages, &policy);
        assert_eq!(set.len(), 2);
        assert_eq!(set.total_live_bytes(), 256 * 1024);
        assert_eq!(set.deferred_for_budget(), 2);
        // Tie on ratio -> ascending page id.
        assert_eq!(set.page_ids(), vec![1, 2]);
    }

    #[test]
    fn select_zero_budget_selects_nothing() {
        let pages = vec![small(1, 1024)];
        let policy = ZRelocationPolicy {
            max_evacuation_bytes: 0,
            ..Default::default()
        };
        let set = ZRelocationSet::select(&pages, &policy);
        assert!(set.is_empty());
        assert_eq!(set.deferred_for_budget(), 1);
    }

    #[test]
    fn select_skips_pages_at_or_above_the_occupancy_cutoff() {
        let mb = 1024 * 1024;
        let pages = vec![
            small(1, mb / 16),      // 3.1% live -> selected
            small(2, mb),           // 50% live  -> above 0.25 cutoff
            small(3, 2 * mb),       // 100% live -> zero garbage AND above cutoff
        ];
        let set = ZRelocationSet::select(&pages, &ZRelocationPolicy::default());
        assert_eq!(set.page_ids(), vec![1]);
    }

    #[test]
    fn select_skips_zero_garbage_and_zero_capacity_pages() {
        let pages = vec![
            PageCandidate {
                page_id: 1,
                live_bytes: 4096,
                capacity_bytes: 4096, // no garbage
                size_class_index: ZFWD_SIZE_CLASS_SMALL,
            },
            PageCandidate {
                page_id: 2,
                live_bytes: 0,
                capacity_bytes: 0, // degenerate
                size_class_index: ZFWD_SIZE_CLASS_SMALL,
            },
        ];
        let set = ZRelocationSet::select(&pages, &ZRelocationPolicy::default());
        assert!(set.is_empty());
    }

    /// Large pages hold one object: evacuating buys no compaction (the page is
    /// either fully live or freed whole), so the default policy excludes them.
    #[test]
    fn select_excludes_large_pages_by_default() {
        let mb = 1024 * 1024;
        let large = PageCandidate {
            page_id: 42,
            live_bytes: mb / 16,
            capacity_bytes: 8 * mb,
            size_class_index: ZFWD_SIZE_CLASS_LARGE,
        };
        let pages = vec![large, small(1, mb / 8)];

        let default_set = ZRelocationSet::select(&pages, &ZRelocationPolicy::default());
        assert_eq!(default_set.page_ids(), vec![1]);
        assert!(!default_set.contains(42));

        // The knob exists, and flipping it admits the large page — which, by
        // garbage ratio, outranks the small one.
        let opt_in = ZRelocationPolicy {
            relocate_large_pages: true,
            ..Default::default()
        };
        let opt_in_set = ZRelocationSet::select(&pages, &opt_in);
        assert_eq!(opt_in_set.page_ids(), vec![42, 1]);
    }

    #[test]
    fn empty_relocation_set_is_inert() {
        let set = ZRelocationSet::empty();
        assert!(set.is_empty());
        assert_eq!(set.len(), 0);
        assert_eq!(set.total_live_bytes(), 0);
        assert_eq!(set.total_garbage_bytes(), 0);
        assert!(!set.contains(1));
        assert!(set.pages().is_empty());
    }

    // -- registry -----------------------------------------------------------

    #[test]
    fn registry_install_get_and_clear() {
        let reg = ZForwardingRegistry::new();
        assert!(reg.get(1).is_none());
        assert_eq!(reg.table_count(), 0);

        let t = Arc::new(ZForwardingTable::for_page(1, 32));
        reg.install(1, t);
        assert!(reg.contains(1));
        assert_eq!(reg.table_count(), 1);

        let got = reg.get(1).expect("page 1 must be registered");
        assert_eq!(got.page_id(), 1);
        got.insert(0x18, p(0x7000));
        // The registry hands out a shared Arc, not a copy.
        assert_eq!(reg.get(1).unwrap().find_payload(0x18), Some(p(0x7000)));

        reg.clear();
        assert!(reg.get(1).is_none());
        // The Arc already handed out stays alive and usable.
        assert_eq!(got.find_payload(0x18), Some(p(0x7000)));
    }

    #[test]
    fn registry_install_for_set_covers_every_selected_page() {
        let mb = 1024 * 1024;
        let pages = vec![small(1, mb / 8), small(2, mb / 16), small(3, mb / 4)];
        let set = ZRelocationSet::select(&pages, &ZRelocationPolicy::default());
        assert_eq!(set.len(), 3);

        let reg = ZForwardingRegistry::new();
        reg.install_for_set(&set, ZFWD_ASSUMED_AVG_OBJECT_BYTES);
        assert_eq!(reg.table_count(), 3);
        for id in set.page_ids() {
            let t = reg.get(id).expect("selected page must have a table");
            assert_eq!(t.page_id(), id);
            assert!(t.capacity().is_power_of_two());
        }
        let mut ids = reg.page_ids();
        ids.sort();
        assert_eq!(ids, vec![1, 2, 3]);
    }

    #[test]
    fn registry_install_for_set_tolerates_a_zero_average() {
        let pages = vec![small(1, 4096)];
        let set = ZRelocationSet::select(&pages, &ZRelocationPolicy::default());
        let reg = ZForwardingRegistry::new();
        reg.install_for_set(&set, 0); // clamped to 8, must not divide by zero
        assert_eq!(reg.table_count(), 1);
    }

    /// Concurrent readers must never block each other out, and every one must
    /// see the same table. Exercises the "clone the Arc, drop the guard"
    /// discipline under contention with a writer.
    #[test]
    fn registry_concurrent_get_is_consistent() {
        const THREADS: usize = 8;
        let reg = Arc::new(ZForwardingRegistry::new());
        reg.install(9, Arc::new(ZForwardingTable::for_page(9, 256)));
        reg.get(9).unwrap().insert(0x20, p(0xABC0));

        let barrier = Arc::new(Barrier::new(THREADS));
        let mut handles = Vec::new();
        for t in 0..THREADS {
            let reg = Arc::clone(&reg);
            let barrier = Arc::clone(&barrier);
            handles.push(std::thread::spawn(move || {
                barrier.wait();
                for i in 0..256u64 {
                    // Half the threads also write, so a fair-RwLock writer is
                    // queued while readers are in flight.
                    if t % 2 == 0 {
                        let page = 100 + (t as u64) * 256 + i;
                        reg.install(page, Arc::new(ZForwardingTable::for_page(page, 1)));
                    }
                    let table = reg.get(9).expect("page 9 stays registered");
                    assert_eq!(table.find_payload(0x20), Some(p(0xABC0)));
                }
            }));
        }
        for h in handles {
            h.join().unwrap();
        }
        assert_eq!(reg.get(9).unwrap().find_payload(0x20), Some(p(0xABC0)));
    }
}
