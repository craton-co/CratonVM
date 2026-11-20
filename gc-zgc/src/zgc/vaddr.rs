// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! ZGC colored-pointer address space — the production-shaped replacement for
//! the metadata-only [`ColoredPointer`](crate::zgc::ColoredPointer) simulation.
//!
//! This module owns exactly one thing: **the bit encoding of a ZGC reference
//! word, and the global "good color" state the load barrier tests against.**
//! It holds no heap memory, spawns no threads, and knows nothing about pages,
//! marking or relocation. It is deliberately dependency-light so the load
//! barrier, the relocator and the page allocator can all be built on top of it
//! independently.
//!
//! # What a colored word is (and is not)
//!
//! A **colored word** ([`ZColoredWord`], a plain `u64`) is what ZGC stores in a
//! *heap reference slot*: an object's reference field, or an element of a
//! reference array. It is **not** a machine pointer, and it is **not** an
//! [`ObjectRef`]. It carries
//!
//! * a 42-bit **object offset** measured from the base of the reserved heap
//!   ([`ZVirtualAddressSpace::base`]), and
//! * 4 bits of **GC metadata** (the *color*), and
//! * a 1-bit **CratonVM colored-word tag** (see "Why bit 63" below).
//!
//! ```text
//!    63    62                            46 45 44 43 42 41                     0
//!   +-----+--------------------------------+--+--+--+--+------------------------+
//!   |  T  |          MBZ  (17 bits)        |Fn|Rm|M1|M0|  object offset (42 b)  |
//!   +-----+--------------------------------+--+--+--+--+------------------------+
//!      |                  |                 |  |  |  |              |
//!      |                  |                 |  |  |  |              +-- byte offset from
//!      |                  |                 |  |  |  |                  the heap base,
//!      |                  |                 |  |  |  |                  0 .. 4 TiB
//!      |                  |                 |  |  |  +----- bit 42  Marked0
//!      |                  |                 |  |  +-------- bit 43  Marked1
//!      |                  |                 |  +----------- bit 44  Remapped
//!      |                  |                 +-------------- bit 45  Finalizable
//!      |                  +-------------------------------- bits 62-46 must be zero
//!      +--------------------------------------------------- bit 63  colored-word tag
//! ```
//!
//! Bits 45-42 and 41-0 are **byte-for-byte OpenJDK ZGC's non-generational
//! layout** (`zGlobals.hpp`: 42-bit offset, 4 metadata bits at shift 42,
//! `Marked0` lowest). That is deliberate: every ZGC paper, blog post and
//! `hotspot/share/gc/z` source file you might read while extending this module
//! describes exactly these bits.
//!
//! ## There is no rival encoding — struck 2026-09-21, wave 4
//!
//! Until this commit the paragraph here was a ⚠ warning that `zgc.rs` assigns
//! `ZGC_COLOR_REMAPPED = 1 << 42` and `ZGC_COLOR_MARKED0 = 1 << 43` against
//! this module's OpenJDK layout, that the two encodings must never be mixed in
//! one word, and that [`is_colored_word`] is what keeps them apart.
//!
//! **Those two constants do not exist and did not exist when that was
//! written.** `grep -rn "ZGC_COLOR" --include=*.rs .` returned exactly one hit
//! and it was this comment claiming they existed; they are absent at the base
//! commit of the round that found it, so they were not deleted by it. A gap
//! page then copied the claim out of this header without checking it, and the
//! round's audit had to strike that page too
//! (`docs/feature-designs/zgc-architecture.md`,
//! finding (1)). One unchecked sentence in a module header became a documented
//! defect in a document about drift, which is the failure that round exists to
//! remove.
//!
//! The note is kept as a stub rather than deleted outright because the claim
//! outlived its code once already and a reader who remembers it needs to be
//! told it is gone, not left to find nothing.
//!
//! What is true, and is the reason [`Z_COLORED_TAG`] earns a bit of its own:
//! the heap stores **plain machine pointers**, and a colored word must be
//! mechanically distinguishable from one. That is not a defence against
//! another *colour* encoding; it is a defence against a colored word reaching
//! anything that expects a pointer — see "Why bit 63" below, and
//! `cratonvm_gc_core::heap`'s use of [`is_colored_word`] to tell the two apart at a read
//! boundary. No slot in this VM has ever held a colored word (`proposal-a`,
//! stage 2, is where that changes), so today the tag guards a hazard that is
//! still ahead rather than a divergence that is behind.
//!
//! # Address-space model: single-mapped + explicit unmasking
//!
//! OpenJDK ZGC **multi-maps** each physical heap page at several virtual
//! addresses — one per color — so that a `Marked0`-colored pointer and a
//! `Remapped`-colored pointer to the same object are *both* directly
//! dereferenceable by the CPU. The color bits are part of a real, mapped
//! virtual address; no unmasking instruction is needed on the load fast path.
//!
//! **CratonVM does not do this, and this module is built on the assumption that
//! it never will.** The decision, stated plainly so it can be argued with:
//!
//! *What we do instead.* The heap is [`cratonvm_gc_core::arena::Arena`]-backed **owned**
//! memory (a single large allocation whose address the VM does not choose).
//! A colored word is a *pure encoding*: an offset from the arena base plus
//! metadata plus a tag. It is never a valid virtual address, and every
//! dereference must first go through [`ZVirtualAddressSpace::uncolor`] to
//! recover the real machine address.
//!
//! *What that costs.* One `AND` + one `ADD` on every reference dereference,
//! forever, and the discipline of never handing a colored word to anything that
//! expects a pointer. In OpenJDK that cost is zero. This is a real,
//! non-recoverable throughput tax and it should be named as such rather than
//! buried.
//!
//! *What it buys.*
//!
//! 1. **Portability.** Multi-mapping needs `memfd_create` + repeated `mmap` of
//!    the same fd on Linux, and `CreateFileMapping` + `MapViewOfFile3` /
//!    `VirtualAlloc2` with placeholder reservations on Windows. Those are two
//!    entirely different implementations of a load-bearing invariant, on the
//!    two platforms this VM is developed and tested on (Windows 11 host, Azure
//!    Linux build host). A bug in either one is a wild-pointer bug.
//! 2. **No fixed-address reservation.** Multi-mapping requires the VM to own a
//!    large, contiguous, *specific* virtual range at a *known* base so that
//!    `offset | color` lands inside it. CratonVM's heap address comes from the
//!    system allocator and moves when the young generation grows (see
//!    `gc::compressed_oops::assert_region_encodable`, which exists precisely
//!    because arenas relocate). Encoding an *offset* rather than an address
//!    makes heap growth a base-pointer update instead of a re-mapping.
//! 3. **Colored words are structurally invalid pointers**, so a leak is loud
//!    rather than silent. See below.
//!
//! *When to revisit.* If ZGC ever becomes the default collector and profiling
//! shows the unmask on the deref path is material, the way back is narrow:
//! clear [`Z_COLORED_TAG`] (making colored words canonical 47-bit addresses
//! again) and multi-map. Nothing else in this layout would have to change.
//! That is the whole reason the tag lives in a bit of its own instead of being
//! folded into the metadata field.
//!
//! # Why bit 63 (the colored-word tag)
//!
//! The high bit is not decoration. `cratonvm_types` already contains two
//! tripwires that assume a reference word is a plausible machine pointer:
//!
//! * [`cratonvm_types::plausible_heap_pointer`] — "non-null, 8-byte aligned,
//!   `<= 2^47 - 1`" — is applied at *every* interpreter and JIT field/array
//!   read boundary to degrade stale references to null;
//! * `cratonvm_types::CompactValue` NaN-boxes an object pointer into a **47-bit
//!   payload** and `panic!`s on anything wider ("exceeds 47-bit address
//!   space").
//!
//! A bare OpenJDK-layout colored word — metadata at bits 45-42, offset below —
//! is 8-byte aligned, non-null and **less than 2^47**. It would sail through
//! both tripwires and be dereferenced as a wild pointer. That is the single
//! most dangerous interaction between ZGC and this codebase, and it is silent.
//!
//! Setting bit 63 on every non-null colored word makes it `> 2^47 - 1`, so a
//! colored word that escapes into a `Value` / `ObjectRef` / `CompactValue` slot
//! is rejected (degraded to null) or panics *at the boundary it escaped
//! through*, using machinery that already exists and is already on every hot
//! read path. We get leak detection for free instead of writing a new one.
//!
//! The invariant this module exists to protect:
//!
//! > **A colored word lives only in heap reference slots and in load-barrier
//! > registers. It is unmasked by the barrier before it ever becomes an
//! > [`ObjectRef`], and it is re-colored on the way back out.**
//!
//! [`is_colored_word`] / [`debug_assert_plain_word`] are the cheap assertions
//! for auditing that invariant at a boundary.
//!
//! # Interaction with compressed oops — MUTUALLY EXCLUSIVE
//!
//! **Verdict: ZGC and `-XX:+UseCompressedOops` cannot coexist, and the
//! selection rule is "ZGC wins, narrow oops are refused".**
//!
//! A narrow oop is 32 bits (`gc::compressed_oops::CompressedOop`). There is no
//! room in it for 4 metadata bits plus a tag: stealing 5 bits would drop the
//! encodable window from 32 GiB (29 payload bits × 8-byte alignment) to 1 GiB,
//! and would still leave the escaped-colored-word hazard above with nothing to
//! trip on, because a 32-bit slot cannot express "greater than 2^47". OpenJDK
//! reaches the same conclusion and refuses `UseCompressedOops` under ZGC.
//!
//! The rule is **already enforced in this tree** and no new gate is needed:
//!
//! * `vm/src/vm/vm_init.rs` (~line 1397) refuses the combination —
//!   `if gc_backend != GcBackend::Generational { … running with 64-bit
//!   references }` — and prints it on stderr. That check is load-bearing.
//! * `gc/src/compressed_oops.rs`'s module header, "Still open" item 6, already
//!   records `gc/src/zgc.rs` as entirely unmigrated for narrow slots.
//!
//! What this module adds is the reason it must *stay* refused rather than being
//! a to-do: it is not that ZGC's slot accessors are unmigrated, it is that the
//! encoding does not fit. Any future ZGC wiring should assert
//! `!cratonvm_types::narrow_oop::narrow_oops_enabled()` at construction rather
//! than assume the vm_init check ran.
//!
//! # Interaction with compact object headers — NO BIT COLLISION
//!
//! **Verdict: no collision. `CompactHeader` and a colored word never occupy the
//! same 64-bit word.** A [`CompactHeader`](cratonvm_gc_core::compact_header::CompactHeader)
//! is stored *inside an object*, at its base; a colored word is stored in a
//! *reference slot pointing at* an object. Every bit of `CompactHeader`
//! (narrow klass 63-32, GC age 31-25, lock 24-23, hash 22, array 21, flags 3-0)
//! is free to mean whatever it means, because nothing ever tests a header word
//! with [`is_good`] or a reference word with `CompactHeader::lock_state`.
//!
//! Two adjacent hazards are worth writing down anyway, because both look like
//! free functionality and neither is:
//!
//! 1. **Do not put ZGC forwarding pointers in the compact header.**
//!    `CompactHeader`'s `Forwarded` lock state stores a *machine address*
//!    `addr >> 3` in a 30-bit split field, and anything above
//!    `FORWARD_INLINE_MAX_ADDR` (~4 GB) is interned in a process-global
//!    `RwLock<FxHashMap>` side table. Writing a 42-bit ZGC *offset* there would
//!    decode as a garbage address; writing a real arena address there sends
//!    essentially every forwarding through a global write lock, which is
//!    exactly the thing a concurrent relocator cannot afford. ZGC's own design
//!    is a per-page `ZForwarding` table for this reason; follow it.
//! 2. **`CompactHeader` presupposes a `NarrowKlass`**, which presupposes the
//!    narrow-klass table — not compressed oops (those are separate, see
//!    `compressed_oops.rs`'s "verified NOT needed" note on klass compression),
//!    so compact headers are *not* transitively blocked by the verdict above.
//!    ZGC may use either header format.
//!
//! # Threading and VM scoping
//!
//! [`ZGoodMask`] is deliberately **not** a `static` / `OnceLock`. In OpenJDK the
//! good mask is a process global, but this VM can host more than one heap in a
//! process (the test harness routinely does), and process-global GC state has
//! already caused parallel-test crashes here. The mask is an ordinary field
//! owned by whichever ZGC heap instance created it; pass a `&ZGoodMask` down to
//! the barrier rather than reaching for a global.

use std::sync::atomic::{AtomicU64, AtomicU8, Ordering};

use cratonvm_types::ObjectRef;

// ---------------------------------------------------------------------------
// Bit layout
// ---------------------------------------------------------------------------

/// A ZGC colored reference word, as stored in a heap reference slot.
///
/// A transparent alias rather than a newtype: colored words are read and
/// written by the load barrier with raw atomic `u64` operations on heap memory
/// (`AtomicU64::compare_exchange` on a slot address), and a newtype would only
/// add casts at every one of those sites. The type alias documents intent; the
/// free functions in this module are the vocabulary.
pub type ZColoredWord = u64;

/// Number of bits of object offset in a colored word: 42, matching OpenJDK ZGC.
pub const Z_OFFSET_BITS: u32 = 42;

/// Bit position at which the 4-bit metadata (color) field starts.
pub const Z_METADATA_SHIFT: u32 = Z_OFFSET_BITS;

/// Number of metadata bits: `Marked0`, `Marked1`, `Remapped`, `Finalizable`.
pub const Z_METADATA_BITS: u32 = 4;

/// Mask selecting the object-offset field (bits 41-0).
pub const Z_OFFSET_MASK: u64 = (1u64 << Z_OFFSET_BITS) - 1;

/// `Marked0` — set on a pointer marked during an even-numbered mark cycle.
pub const Z_MARKED0: u64 = 1u64 << Z_METADATA_SHIFT; // bit 42

/// `Marked1` — set on a pointer marked during an odd-numbered mark cycle.
pub const Z_MARKED1: u64 = 1u64 << (Z_METADATA_SHIFT + 1); // bit 43

/// `Remapped` — set on a pointer known to point at the object's *current*
/// location (i.e. relocation has already been applied to it).
pub const Z_REMAPPED: u64 = 1u64 << (Z_METADATA_SHIFT + 2); // bit 44

/// `Finalizable` — set on a pointer reached **only** through a finalizable
/// object's reference chain. Such an object is kept alive for finalization but
/// is not strongly reachable, so a *strong* load through it must still take the
/// barrier slow path. This is why [`ZGoodMask::weak_bad`] exists separately
/// from [`ZGoodMask::bad`].
pub const Z_FINALIZABLE: u64 = 1u64 << (Z_METADATA_SHIFT + 3); // bit 45

/// Mask selecting the whole 4-bit metadata field (bits 45-42).
pub const Z_METADATA_MASK: u64 = ((1u64 << Z_METADATA_BITS) - 1) << Z_METADATA_SHIFT;

/// CratonVM-specific: bit 63, set on every non-null colored word.
///
/// See the module docs, "Why bit 63". Briefly: it pushes a colored word above
/// the 47-bit address window that
/// [`cratonvm_types::plausible_heap_pointer`] and `CompactValue` both assume,
/// so a colored word that escapes into a pointer slot is rejected loudly by
/// tripwires that already exist instead of being dereferenced silently.
pub const Z_COLORED_TAG: u64 = 1u64 << 63;

/// Bits 62-46: reserved, **must be zero** in a well-formed colored word.
///
/// Kept explicitly reserved rather than widening the offset field, because
/// clearing [`Z_COLORED_TAG`] must be sufficient to turn a colored word back
/// into a canonical user-space virtual address if CratonVM ever adopts real
/// OS multi-mapping (see the module docs). That requires everything at bit 46
/// and above to be zero.
pub const Z_RESERVED_MBZ_MASK: u64 = !(Z_OFFSET_MASK | Z_METADATA_MASK | Z_COLORED_TAG);

/// The null colored word. Carries no tag and no metadata: it is the all-zero
/// word an untouched heap slot already holds, so a zeroed page is a page full
/// of nulls with no initialisation pass.
pub const Z_NULL: ZColoredWord = 0;

/// Largest heap this encoding can address: `2^42` bytes = **4 TiB**.
///
/// This is the *offset field* capacity, not a policy limit. A heap larger than
/// this cannot be expressed at all, which is why
/// [`ZVirtualAddressSpace::new`] refuses it rather than truncating.
pub const Z_MAX_HEAP_SIZE: u64 = 1u64 << Z_OFFSET_BITS;

/// Largest machine address this module will accept as part of a reserved heap
/// range: `2^47 - 1`.
///
/// Matches the assumption baked into [`cratonvm_types::plausible_heap_pointer`]
/// and `CompactValue`'s NaN box. A heap outside this window would produce
/// unmasked addresses those layers cannot represent, so it is refused here
/// rather than corrupting values later.
pub const Z_MAX_ADDRESS: u64 = (1u64 << 47) - 1;

/// Alignment every heap object (and therefore every offset) satisfies.
pub const Z_OBJECT_ALIGNMENT: u64 = 8;

// ---------------------------------------------------------------------------
// ZColor
// ---------------------------------------------------------------------------

/// One of the four metadata bits a colored word may carry.
///
/// A well-formed colored word carries **exactly one** of these. That is what
/// makes the load-barrier fast path a single mask-and-branch: with one bit set,
/// `word & good_mask != 0` and `word & bad_mask == 0` are the same question, so
/// whichever form is cheaper on the target can be used (see [`is_good`] /
/// [`is_bad`] for why ZGC picks the bad-mask form).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum ZColor {
    /// Marked during an even-numbered mark cycle.
    Marked0 = 0,
    /// Marked during an odd-numbered mark cycle.
    Marked1 = 1,
    /// Points at the object's current location.
    Remapped = 2,
    /// Reached only through a finalizable chain.
    Finalizable = 3,
}

impl ZColor {
    /// The single metadata bit this color occupies.
    #[inline]
    pub const fn bit(self) -> u64 {
        match self {
            ZColor::Marked0 => Z_MARKED0,
            ZColor::Marked1 => Z_MARKED1,
            ZColor::Remapped => Z_REMAPPED,
            ZColor::Finalizable => Z_FINALIZABLE,
        }
    }

    /// Recover a color from a single metadata bit.
    ///
    /// Returns `None` for zero, for a non-metadata bit, or for a word with more
    /// than one metadata bit set — all three are malformed, and none of them
    /// should be papered over with a default.
    #[inline]
    pub fn from_bit(bit: u64) -> Option<ZColor> {
        match bit & Z_METADATA_MASK {
            Z_MARKED0 => Some(ZColor::Marked0),
            Z_MARKED1 => Some(ZColor::Marked1),
            Z_REMAPPED => Some(ZColor::Remapped),
            Z_FINALIZABLE => Some(ZColor::Finalizable),
            _ => None,
        }
    }

    /// All four colors, for exhaustive tests and diagnostics.
    pub const ALL: [ZColor; 4] = [
        ZColor::Marked0,
        ZColor::Marked1,
        ZColor::Remapped,
        ZColor::Finalizable,
    ];
}

// ---------------------------------------------------------------------------
// Encoding / decoding
// ---------------------------------------------------------------------------

/// Build a colored word from a heap offset and a color.
///
/// `offset` is a **byte offset from the heap base**, not a machine address.
/// Use [`ZVirtualAddressSpace::color_address`] when you hold a real pointer.
///
/// An offset that does not fit the 42-bit field trips a `debug_assert!` and is
/// masked in release. Masking rather than panicking is the right release
/// behaviour here only because [`ZVirtualAddressSpace::new`] has already
/// refused any heap that could produce such an offset — the assert exists to
/// catch a caller that bypassed the address space, not to validate untrusted
/// input.
#[inline]
pub fn color(offset: u64, color: ZColor) -> ZColoredWord {
    debug_assert!(
        offset <= Z_OFFSET_MASK,
        "ZGC offset {offset:#x} exceeds the {Z_OFFSET_BITS}-bit offset field"
    );
    Z_COLORED_TAG | color.bit() | (offset & Z_OFFSET_MASK)
}

/// Strip all metadata and the tag, yielding the bare heap offset.
///
/// This is the "explicit unmasking" the module docs argue for: it is what a
/// single-mapped design pays on every dereference, and what an OS-multi-mapped
/// design would get for free.
#[inline]
pub const fn offset_of(colored: ZColoredWord) -> u64 {
    colored & Z_OFFSET_MASK
}

/// The metadata bits of a colored word, as a mask (not a [`ZColor`]).
#[inline]
pub const fn metadata_of(colored: ZColoredWord) -> u64 {
    colored & Z_METADATA_MASK
}

/// The color of a colored word, or `None` if it carries zero or several
/// metadata bits.
#[inline]
pub fn color_of(colored: ZColoredWord) -> Option<ZColor> {
    ZColor::from_bit(metadata_of(colored))
}

/// Replace a colored word's metadata, preserving its offset.
///
/// This is the **self-heal** operation: after the barrier slow path resolves a
/// stale word it writes the recolored value back into the slot with a CAS, so
/// the next load of that slot takes the fast path. (The CAS itself belongs to
/// the barrier, not here — this module owns the encoding only.)
#[inline]
pub fn recolor(colored: ZColoredWord, color: ZColor) -> ZColoredWord {
    if colored == Z_NULL {
        return Z_NULL;
    }
    Z_COLORED_TAG | color.bit() | (colored & Z_OFFSET_MASK)
}

/// The load-barrier fast-path test, good-mask form: `word & good_mask != 0`.
///
/// **Null is not good under this form.** `Z_NULL` is all-zero, so it ANDs to
/// zero and reports `false`; a caller using this form must null-check first
/// (which is what the legacy `LoadBarrier::check` does) or tolerate nulls in
/// its slow path.
///
/// Prefer [`is_bad`] on the actual hot path — see its docs for why ZGC does.
#[inline]
pub const fn is_good(colored: ZColoredWord, good_mask: u64) -> bool {
    (colored & good_mask) != 0
}

/// The load-barrier fast-path test, bad-mask form: `word & bad_mask != 0`
/// means "take the slow path".
///
/// This is the form OpenJDK's barrier actually emits, and the reason is worth
/// stating because it is not obvious: **null passes for free.** `Z_NULL & bad
/// == 0`, so a null reference needs no separate branch, and the emitted
/// sequence is one `test` plus one not-taken `jnz`. The good-mask form needs an
/// extra null check to get the same behaviour.
///
/// The two forms agree for every well-formed colored word precisely because
/// such a word carries exactly one metadata bit and
/// `bad_mask == good_mask ^ Z_METADATA_MASK`.
#[inline]
pub const fn is_bad(colored: ZColoredWord, bad_mask: u64) -> bool {
    (colored & bad_mask) != 0
}

/// Whether a colored word is null.
#[inline]
pub const fn is_null(colored: ZColoredWord) -> bool {
    colored == Z_NULL
}

// -- per-bit predicates ------------------------------------------------------

/// Does this word carry `Marked0`?
#[inline]
pub const fn is_marked0(colored: ZColoredWord) -> bool {
    (colored & Z_MARKED0) != 0
}

/// Does this word carry `Marked1`?
#[inline]
pub const fn is_marked1(colored: ZColoredWord) -> bool {
    (colored & Z_MARKED1) != 0
}

/// Does this word carry either mark bit?
#[inline]
pub const fn is_marked_any(colored: ZColoredWord) -> bool {
    (colored & (Z_MARKED0 | Z_MARKED1)) != 0
}

/// Does this word carry `Remapped`?
#[inline]
pub const fn is_remapped(colored: ZColoredWord) -> bool {
    (colored & Z_REMAPPED) != 0
}

/// Does this word carry `Finalizable`?
#[inline]
pub const fn is_finalizable(colored: ZColoredWord) -> bool {
    (colored & Z_FINALIZABLE) != 0
}

// -- structural predicates ---------------------------------------------------

/// Is this word tagged as a ZGC colored word (bit 63 set)?
///
/// The audit primitive for the module-level invariant: a word reaching a
/// pointer-shaped slot must answer `false` here, and a word read out of a heap
/// reference slot under ZGC must answer `true` unless it is null.
#[inline]
pub const fn is_colored_word(word: u64) -> bool {
    (word & Z_COLORED_TAG) != 0
}

/// Is this word structurally well-formed as a colored reference?
///
/// Null, or: tagged, reserved bits clear, and exactly one metadata bit set.
/// Intended for `debug_assert!`s and diagnostics, not for the hot path.
#[inline]
pub fn is_well_formed(colored: ZColoredWord) -> bool {
    if colored == Z_NULL {
        return true;
    }
    is_colored_word(colored)
        && (colored & Z_RESERVED_MBZ_MASK) == 0
        && metadata_of(colored).count_ones() == 1
}

/// Debug-only tripwire for "this word is about to be treated as a machine
/// pointer, so it had better not be a colored word".
///
/// Costs nothing in release. Put it wherever a colored word could plausibly
/// leak into an [`ObjectRef`], a `Value` or a `CompactValue`.
#[inline]
pub fn debug_assert_plain_word(word: u64) {
    debug_assert!(
        !is_colored_word(word),
        "ZGC colored word {word:#x} escaped into a pointer-shaped slot \
         (offset={:#x}, metadata={:#x})",
        offset_of(word),
        metadata_of(word)
    );
}

/// Do two colored words refer to the same object, ignoring color?
#[inline]
pub const fn same_object(a: ZColoredWord, b: ZColoredWord) -> bool {
    offset_of(a) == offset_of(b)
}

/// **Seam 4, decision 3: a word read out of a reference slot → a root the
/// generation marker can use.**
///
/// `word` is whatever an old-generation slot held. `space` is the address
/// space that slot's heap is encoded against, or `None` for a heap that stores
/// plain machine pointers (which is every heap that actually runs in this tree
/// today — see `gap-c-colored-pointers-are-encoded-but-never-stored.md`).
///
/// Returns the **uncolored machine address**, or `None` for null and for a
/// word whose encoded offset is outside `space`.
///
/// # Why this exists as a named function instead of two lines at the call site
///
/// `adapters.rs`'s seam-4 header names this the *"highest-risk unassigned
/// responsibility"* at the remembered↔generation seam, and the risk is not
/// that the conversion is hard — it is [`ZVirtualAddressSpace::uncolor`], two
/// instructions. The risk is that **omitting it does not fail.**
/// `ZGenerationScope::admits` answers `false` for a bare 42-bit offset, so
/// every cross-generation edge is *filtered*, the remembered set reads empty,
/// and live young objects are freed with no error anywhere. And it is
/// platform-dependent: on the Windows dev host the heap base is small enough
/// that an unconverted offset can still land inside the heap and the bug
/// hides; on Linux the base is around `0x7f…` and every edge drops.
///
/// So the rule is written once, here, in the module that owns the encoding,
/// with the pass-through case *inside* it — because the pass-through is the
/// case a call site gets right by accident today and wrong the day colored
/// pointers land.
///
/// `remembered.rs` deliberately holds no colour knowledge (importing this
/// module would give the store barrier's hot path a dependency on the pointer
/// encoding) and `generation.rs` holds none either beyond a one-bit tripwire
/// on [`Z_COLORED_TAG`]. The adapter is the only place that legitimately knows
/// both domains, and this is the call it should make.
///
/// # The three cases
///
/// | `word` | answer |
/// |---|---|
/// | tagged colored, offset in range | `Some(base + offset)` |
/// | tagged colored, offset out of range, or one that uncolors to `0` | `None` — corrupt, or from another address space; either way not a root |
/// | [`Z_NULL`], or any untagged zero | `Some(0)` — null is not this function's business to filter; see below |
/// | untagged, non-zero | `Some(word)` — already a machine address, passed through |
///
/// (zr z1/br, 2026-10-12: the `Z_NULL` row used to say `None`, which the code
/// never did -- `is_colored_word(0)` is false, so null takes the untagged
/// pass-through. `gap-p-vaddr-null-row-is-wrong.md`; pinned by
/// `gc/tests/zgc_p_slot_word_conversion.rs`.)
///
/// An untagged **zero** answers `Some(0)`: a null slot is not this function's
/// business to filter, and a caller that treats `0` as a root has a bug this
/// function must not hide by returning `None` for two different reasons.
#[inline]
pub fn root_address_for_slot_word(word: u64, space: Option<&ZVirtualAddressSpace>) -> Option<u64> {
    if !is_colored_word(word) {
        // Already an uncolored machine address -- the `ZgcRealHeap` case, and
        // the test doubles'. Passed through unchanged.
        return Some(word);
    }
    // Tagged. Without an address space there is nothing to add the offset to,
    // and returning the raw word would hand the marker a 42-bit offset --
    // precisely the silent filter this function exists to prevent. `None` is
    // the honest answer and the caller must count it.
    space?.uncolor(word).filter(|addr| *addr != 0)
}

// ---------------------------------------------------------------------------
// ZGlobalPhase
// ---------------------------------------------------------------------------

/// The global ZGC phase, which is what determines the good color.
///
/// Mirrors OpenJDK's `ZGlobalPhase`. A cycle runs
/// `Relocate → Mark → MarkComplete → Relocate → …`; the collector idles in
/// `Relocate` between cycles, because "relocation finished, every pointer
/// should be `Remapped`" is also the correct description of a quiescent heap.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum ZGlobalPhase {
    /// Relocation in progress, or no cycle running. Good color: `Remapped`.
    Relocate = 0,
    /// Concurrent mark in progress. Good color: the current `MarkedN`.
    Mark = 1,
    /// Mark has finished but relocation has not started. Good color is still
    /// the current `MarkedN` — nothing has moved yet.
    MarkComplete = 2,
}

impl ZGlobalPhase {
    #[inline]
    fn from_u8(v: u8) -> ZGlobalPhase {
        match v {
            1 => ZGlobalPhase::Mark,
            2 => ZGlobalPhase::MarkComplete,
            // 0 and anything malformed: the quiescent phase is the safe
            // answer, because it is the one whose good color (`Remapped`)
            // is what a heap with no cycle in flight actually carries.
            _ => ZGlobalPhase::Relocate,
        }
    }
}

// ---------------------------------------------------------------------------
// ZGoodMask
// ---------------------------------------------------------------------------

/// The global good-color state the load barrier tests against.
///
/// # The rotation
///
/// ZGC alternates `Marked0` / `Marked1` across successive mark cycles so that a
/// single word — the good mask — answers "is this pointer up to date?" without
/// the barrier having to know the cycle number, and without a heap-wide pass to
/// clear last cycle's mark bits. Cycle *N*'s marks become cycle *N+1*'s stale
/// color automatically.
///
/// | phase                   | good mask   | bad mask (`good ^ metadata`) |
/// |-------------------------|-------------|------------------------------|
/// | `Relocate` (or idle)    | `Remapped`  | `M0 \| M1 \| Finalizable`    |
/// | `Mark` / `MarkComplete` | `MarkedN`   | `MarkedN' \| Remapped \| Fin`|
///
/// Note that `Remapped` is **bad** during marking. That is deliberate and is
/// what makes concurrent marking work: an ordinary, up-to-date, `Remapped`
/// pointer takes the slow path exactly once per mark cycle, which is the event
/// that pushes its target onto the mark stack. The barrier *is* the marking
/// loop's work source.
///
/// # Derived masks, one atomic
///
/// Only [`good`](Self::good) is stored. [`bad`](Self::bad) and
/// [`weak_bad`](Self::weak_bad) are derived with an XOR against a constant on
/// every read. That is one instruction, and it removes the alternative's real
/// hazard: three separately-stored masks cannot be updated atomically together,
/// so a reader could observe a half-flipped state. Deriving them makes that
/// state unrepresentable rather than merely unlikely.
///
/// # Memory ordering, and the safepoint assumption
///
/// **Assumption: [`flip_to_mark`](Self::flip_to_mark) and
/// [`flip_to_remap`](Self::flip_to_remap) are called at a safepoint, with every
/// mutator thread stopped.** Everything below depends on it.
///
/// * The fast path uses `Ordering::Relaxed`. A relaxed load may in principle
///   return a stale value indefinitely, which would be unsound for a mask that
///   flipped underneath a running mutator — the mutator would keep letting
///   last-phase pointers through the fast path. It is sound here *only* because
///   a mutator cannot be running when the flip happens: it is parked at a
///   safepoint, and the release/acquire pair that resumes it (the safepoint
///   protocol's own handshake) is what publishes the new mask. The barrier
///   therefore pays no fence on the hottest path in the VM.
/// * The flips still store with `Ordering::Release`. That is not redundant with
///   the safepoint handshake, it is defence in depth: it keeps this type
///   correct in isolation (unit tests, and any future caller that flips outside
///   a safepoint) rather than correct only in context. `Release` also orders
///   the mask store after any collector state the flip depends on — e.g. the
///   relocation set being published before pointers start being called stale.
///
/// If a future design needs to flip *without* a safepoint, the fast-path load
/// must be upgraded to `Acquire` and the barrier must be re-audited. Do not
/// change one without the other.
///
/// # Reconciling with `ZgcRealHeap::mark_active` — the second phase machine
///
/// *Closes finding (3) of
/// `docs/feature-designs/zgc-architecture.md`,
/// 2026-09-21, wave 4.* That page says this type and
/// `ZgcRealHeap::mark_active` / `set_mark_active` are "two phase machines with
/// different ordering rules" that "cannot coexist and disagree once colored
/// pointers land", and leaves it there. Leaving it there is the problem: an
/// unwritten disagreement is settled by whoever wires them, in whichever
/// direction they happen to read first. So it is written down, and the parts
/// of it that are checkable are checked
/// (`good_mask_and_mark_active_disagree_only_across_mark_complete`).
///
/// The two are **not** the same boolean, and three things separate them:
///
/// 1. **They arm different barriers.** `mark_active` arms a *pre-write* SATB
///    store barrier: marking is snapshot-at-the-beginning and the mutator's
///    stores are the work source. A mark-phase good mask arms a *load*
///    barrier, in which `Remapped` is **bad** and the first read of each slot
///    is the work source. Both can be on; only one may own the mark set, and
///    proposal A stage 5 ("retire the SATB store barrier for ZGC") is where
///    that ownership transfers. Until then `mark_active` is authoritative and
///    the mask is inert — `ZgcRealHeap::good_mask()` answers `Z_REMAPPED`
///    unconditionally, so `classify_bad_masked` calls every word `Good`.
/// 2. **They disagree across `MarkComplete`, by design, and only there.**
///    [`Self::set_mark_complete`] deliberately leaves the good color on
///    `MarkedN` — nothing has moved, so every pointer a mutator holds is still
///    correctly colored — while `set_mark_active(false)` is what ends the SATB
///    window. So between mark end and [`Self::flip_to_remap`],
///    [`Self::marking`] is `true` and `mark_active` is `false`. That is the
///    one window in which "is it marking?" has two right answers, and a wiring
///    that assumes they are the same boolean will either re-arm SATB for the
///    relocate pause or treat `Remapped` as good while the mark bits are still
///    this cycle's.
/// 3. **Their ordering rules are mirror images, for the same reason.**
///    `set_mark_active` raises the JIT's gate *before* the truth when arming
///    and lowers it *after* the truth when disarming, so the compiled fast
///    path errs toward calling the helper it did not need. This type has no
///    mirror to keep in step — that is the whole argument for deriving `bad`
///    and `weak_bad` instead of storing them — **until a compiled load barrier
///    bakes [`Self::bad`] into a `test` immediate**, at which point that
///    immediate *is* a mirror and inherits `set_mark_active`'s rule verbatim:
///    re-patch to the new bad mask before the flip when entering a phase that
///    makes more words bad, and after the flip when leaving one. Entering
///    mark makes `Remapped` bad, so the patch comes first; entering remap
///    makes the mark bits bad and `Remapped` good, so the patch comes first
///    there too. **The compiled immediate is patched before every flip, both
///    directions**, because in both directions the stale immediate is the one
///    that lets a bad word through.
///
/// The rule that follows, and the only one a wiring has to remember: **the
/// mask is the authority on a word's colour and `mark_active` is the authority
/// on whether the store barrier runs.** Neither may be derived from the other
/// while both exist.
///
/// # Scoping
///
/// Instance state, never a `static`. See the module docs.
#[derive(Debug)]
pub struct ZGoodMask {
    /// The current good mask. Read on the barrier fast path with `Relaxed`.
    good: AtomicU64,
    /// Which of `Z_MARKED0` / `Z_MARKED1` the *next* mark cycle will use once
    /// flipped. Read and written only at a safepoint.
    marked: AtomicU64,
    /// [`ZGlobalPhase`] as a `u8`. Diagnostic; the barrier never reads it.
    phase: AtomicU8,
}

impl ZGoodMask {
    /// A quiescent mask: phase `Relocate`, good color `Remapped`.
    ///
    /// `marked` starts at `Z_MARKED1` so that the first
    /// [`flip_to_mark`](Self::flip_to_mark) XORs it to `Z_MARKED0` — i.e. the
    /// first mark cycle of a process is cycle 0 and uses `Marked0`, which is
    /// what anyone reading a log would assume.
    pub const fn new() -> Self {
        ZGoodMask {
            good: AtomicU64::new(Z_REMAPPED),
            marked: AtomicU64::new(Z_MARKED1),
            phase: AtomicU8::new(ZGlobalPhase::Relocate as u8),
        }
    }

    // -- fast path ----------------------------------------------------------

    /// The current good mask. **Fast path**: `Relaxed`, see the type docs.
    #[inline]
    pub fn good(&self) -> u64 {
        self.good.load(Ordering::Relaxed)
    }

    /// The current bad mask, derived as `good ^ Z_METADATA_MASK`.
    ///
    /// This is the mask a compiled load barrier should bake into its `test`
    /// instruction. **Fast path**: `Relaxed`.
    #[inline]
    pub fn bad(&self) -> u64 {
        self.good() ^ Z_METADATA_MASK
    }

    /// The bad mask for a **weak** load (`Reference.get`, weak/soft/phantom
    /// referent reads, and `java.lang.ref` internals).
    ///
    /// A weak load tolerates a pointer that is only finalizable-reachable and a
    /// pointer that is merely `Remapped`, because it is not asserting strong
    /// reachability and must not mark the referent. Derived exactly as OpenJDK
    /// does: `(good | Remapped | Finalizable) ^ metadata`.
    #[inline]
    pub fn weak_bad(&self) -> u64 {
        (self.good() | Z_REMAPPED | Z_FINALIZABLE) ^ Z_METADATA_MASK
    }

    /// Fast-path test in good-mask form. Null reports `false`; see [`is_good`].
    #[inline]
    pub fn is_good(&self, colored: ZColoredWord) -> bool {
        is_good(colored, self.good())
    }

    /// Fast-path test in bad-mask form — "does this load need the slow path?".
    /// Null reports `false` (i.e. passes) with no extra branch. See [`is_bad`].
    #[inline]
    pub fn is_bad(&self, colored: ZColoredWord) -> bool {
        is_bad(colored, self.bad())
    }

    /// Weak-load fast-path test.
    #[inline]
    pub fn is_weak_bad(&self, colored: ZColoredWord) -> bool {
        is_bad(colored, self.weak_bad())
    }

    // -- phase transitions (safepoint only) ---------------------------------

    /// Enter a mark phase: rotate `Marked0` ⇄ `Marked1` and make the new mark
    /// bit the good color. Returns the new good mask.
    ///
    /// **Must be called at a safepoint** with mutators stopped — see the type
    /// docs on ordering. Stores with `Release`.
    ///
    /// The XOR rotation, rather than "pick by cycle parity", is what makes the
    /// two mark bits self-clearing: last cycle's good color becomes this
    /// cycle's bad color with no heap traversal.
    pub fn flip_to_mark(&self) -> u64 {
        let next = self.marked.load(Ordering::Relaxed) ^ (Z_MARKED0 | Z_MARKED1);
        self.marked.store(next, Ordering::Relaxed);
        self.phase
            .store(ZGlobalPhase::Mark as u8, Ordering::Relaxed);
        self.good.store(next, Ordering::Release);
        tracing::debug!(
            target: "zgc",
            good_mask = next,
            marked0 = (next == Z_MARKED0),
            "ZGC good mask flipped to mark"
        );
        next
    }

    /// Record that marking has completed. The good color is unchanged —
    /// nothing has moved yet, so every pointer the mutator holds is still
    /// correctly colored. Only the reported phase changes.
    ///
    /// **Must be called at a safepoint.**
    pub fn set_mark_complete(&self) {
        self.phase
            .store(ZGlobalPhase::MarkComplete as u8, Ordering::Release);
    }

    /// Enter the relocate phase: `Remapped` becomes the good color, so every
    /// pointer still carrying a mark bit is now bad and will be forwarded and
    /// self-healed by the barrier on first touch. Returns the new good mask.
    ///
    /// **Must be called at a safepoint** with mutators stopped — and, critically,
    /// *after* the relocation set is published, since the `Release` store here
    /// is what orders that publication ahead of the first mutator that observes
    /// its pointers as stale.
    pub fn flip_to_remap(&self) -> u64 {
        self.phase
            .store(ZGlobalPhase::Relocate as u8, Ordering::Relaxed);
        self.good.store(Z_REMAPPED, Ordering::Release);
        tracing::debug!(
            target: "zgc",
            good_mask = Z_REMAPPED,
            "ZGC good mask flipped to remap"
        );
        Z_REMAPPED
    }

    // -- queries ------------------------------------------------------------

    /// The current global phase (diagnostic).
    #[inline]
    pub fn phase(&self) -> ZGlobalPhase {
        ZGlobalPhase::from_u8(self.phase.load(Ordering::Acquire))
    }

    /// The mark bit the current (or most recent) mark cycle uses.
    #[inline]
    pub fn current_marked_bit(&self) -> u64 {
        self.marked.load(Ordering::Relaxed)
    }

    /// **Does the mask say a mark colour is in force?** — i.e. is the good
    /// colour one of the two mark bits rather than `Remapped`?
    ///
    /// This is the mask's own answer to "is it marking", and it is
    /// deliberately **not** `ZgcRealHeap::mark_active`. The two coincide
    /// through the mark phase and part across `MarkComplete`: this stays
    /// `true` until [`Self::flip_to_remap`], because the pointers a mutator
    /// holds are still coloured with this cycle's mark bit and calling them
    /// stale before anything has moved would send every load to the slow path
    /// for nothing. `mark_active` goes `false` at mark end, because that is
    /// when the SATB store barrier stops being needed.
    ///
    /// Read the type docs' "Reconciling with `ZgcRealHeap::mark_active`"
    /// before using either one as the other.
    ///
    /// Derived from [`Self::good`] alone — one relaxed load of the word the
    /// barrier reads anyway, and therefore never a half-flipped answer, for
    /// the same reason [`Self::allocation_color`] is derived from it.
    #[inline]
    pub fn marking(&self) -> bool {
        self.good() != Z_REMAPPED
    }

    /// The color a freshly allocated object's references should be given.
    ///
    /// During a mark phase that is the current mark bit — an object allocated
    /// mid-cycle is implicitly live and must not be re-marked. Otherwise it is
    /// `Remapped`.
    ///
    /// # Derived from [`Self::good`], and that is the whole implementation
    ///
    /// The good mask already *is* this answer: [`Self::flip_to_mark`] stores the
    /// new mark bit into `good`, [`Self::set_mark_complete`] leaves it alone,
    /// and [`Self::flip_to_remap`] stores `Z_REMAPPED`. Reading it back is one
    /// relaxed load of the word the load barrier reads anyway, on a line that is
    /// already hot in every core's cache.
    ///
    /// It used to be computed from `phase()` and `current_marked_bit()` — two
    /// separate atomics, an `Acquire` and a `Relaxed`, with a decode in between.
    /// That is not merely slower on a path that runs once per allocation
    /// ([`crate::zgc::tlab::ZTlabHeapHooks::allocation_color`]); it is the only
    /// place in this type where a reader could observe a **half-flipped** state,
    /// because `flip_to_mark` writes `marked` and `phase` as two independent
    /// stores. The type's own doc argues at length that the three masks are
    /// derived rather than stored precisely so "a reader could observe a
    /// half-flipped state" is *unrepresentable* rather than merely unlikely;
    /// this accessor was the exception. It no longer is: one load, one decode,
    /// and the answer is by construction the same color the barrier is letting
    /// through at that instant.
    ///
    /// The `unwrap_or` arm is unreachable — `good` only ever holds a single
    /// metadata bit — and answers `Remapped`, the quiescent color, for the same
    /// fail-safe reason [`ZGlobalPhase::from_u8`] does.
    #[inline]
    pub fn allocation_color(&self) -> ZColor {
        ZColor::from_bit(self.good()).unwrap_or(ZColor::Remapped)
    }
}

impl Default for ZGoodMask {
    fn default() -> Self {
        ZGoodMask::new()
    }
}

// ---------------------------------------------------------------------------
// ZCompiledBadMask (zt t21/zb4, stage 2e groundwork)
// ---------------------------------------------------------------------------

/// The bad mask as ONE stable, naturally aligned word that compiled code can
/// test in memory: `MOV R11, imm64 &word ; TEST RAX, [R11] ; JNZ slow`.
///
/// # Why a cell, and not the immediate the [`ZGoodMask`] docs discuss
///
/// Stage 2d's inline decode (`jit/src/x64/licm.rs`,
/// `zgc_colored_slot_decode_bytes`) has no bad-mask test at all: it is sound
/// only while the good color is pinned at `Remapped`. The color flip (stage
/// 2e) makes colored words bad once per phase, so the compiled fast path must
/// test the CURRENT bad mask. Baking it as an immediate would mean re-patching
/// every compiled site at every flip (the rule the [`ZGoodMask`] docs state
/// for that design); a memory operand is one store per flip and no patching,
/// at the cost of one load that stays in L1 between flips.
///
/// # The rule: publish before the good mask moves
///
/// [`ZGoodMask::flip_to_mark_publishing`] and
/// [`ZGoodMask::flip_to_remap_publishing`] store the NEW bad mask here first
/// and flip the good mask second, both `Release`. At a safepoint (the only
/// place a flip happens) the order is invisible, and outside one it errs
/// toward the slow path in both directions -- entering mark makes `Remapped`
/// bad, entering remap makes the mark bits bad, and in each case the new mask
/// is the one that sends the newly stale words to the slow path. This is the
/// "patched before every flip, both directions" rule of the [`ZGoodMask`]
/// docs, for a word instead of an immediate.
///
/// Compiled code reads it with a plain load (`Relaxed`): the flip safepoint's
/// own release/acquire handshake publishes it, exactly as for the good mask.
///
/// # Scoping
///
/// Instance state, like [`ZGoodMask`]: the heap that owns the good mask owns
/// this cell (boxed, so its address never moves), and hands its address to
/// the JIT. Cache-line aligned so the flip's store does not share a line with
/// a hot counter.
#[derive(Debug)]
#[repr(C, align(64))]
pub struct ZCompiledBadMask {
    word: AtomicU64,
}

impl ZCompiledBadMask {
    /// The quiescent cell: the bad mask of `Remapped`
    /// (`Marked0 | Marked1 | Finalizable`), matching [`ZGoodMask::new`].
    pub const fn new() -> Self {
        ZCompiledBadMask {
            word: AtomicU64::new(Z_REMAPPED ^ Z_METADATA_MASK),
        }
    }

    /// The published bad mask, as compiled code reads it (`Relaxed`).
    #[inline]
    pub fn load(&self) -> u64 {
        self.word.load(Ordering::Relaxed)
    }

    /// The word's address, for an `imm64` in compiled code. Stable for the
    /// life of `self`; the owner must not move it (box it).
    #[inline]
    pub fn word_address(&self) -> usize {
        &self.word as *const AtomicU64 as usize
    }

    /// Publish the bad mask that goes with `good` (`good ^ Z_METADATA_MASK`,
    /// the derivation [`ZGoodMask::bad`] uses).
    pub fn publish_for_good(&self, good: u64) {
        self.word.store(good ^ Z_METADATA_MASK, Ordering::Release);
    }
}

impl Default for ZCompiledBadMask {
    fn default() -> Self {
        ZCompiledBadMask::new()
    }
}

impl ZGoodMask {
    /// [`Self::flip_to_mark`], publishing the new bad mask into `cell` FIRST
    /// (see [`ZCompiledBadMask`]'s rule). Returns the new good mask.
    ///
    /// **Must be called at a safepoint**, as [`Self::flip_to_mark`].
    pub fn flip_to_mark_publishing(&self, cell: &ZCompiledBadMask) -> u64 {
        let next = self.marked.load(Ordering::Relaxed) ^ (Z_MARKED0 | Z_MARKED1);
        cell.publish_for_good(next);
        let flipped = self.flip_to_mark();
        debug_assert_eq!(
            flipped, next,
            "flip_to_mark rotated to a different mark bit"
        );
        flipped
    }

    /// [`Self::flip_to_remap`], publishing the new bad mask into `cell` FIRST.
    ///
    /// **Must be called at a safepoint**, as [`Self::flip_to_remap`].
    pub fn flip_to_remap_publishing(&self, cell: &ZCompiledBadMask) -> u64 {
        cell.publish_for_good(Z_REMAPPED);
        self.flip_to_remap()
    }
}

// ---------------------------------------------------------------------------
// ZVirtualAddressSpace
// ---------------------------------------------------------------------------

/// Why a [`ZVirtualAddressSpace`] could not be constructed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ZAddressSpaceError {
    /// The requested heap exceeds the 42-bit offset field (4 TiB).
    HeapTooLarge {
        /// Bytes requested.
        requested: u64,
        /// Bytes the encoding can express ([`Z_MAX_HEAP_SIZE`]).
        max: u64,
    },
    /// A zero-byte heap. There is no such thing, and it would make every
    /// containment check vacuously false.
    ZeroSize,
    /// The heap base is null, or not [`Z_OBJECT_ALIGNMENT`]-aligned. An
    /// unaligned base would make `base + offset` unaligned for every object.
    BadBase {
        /// The offending base address.
        base: u64,
    },
    /// `base + size` overflows, or leaves the 47-bit window
    /// ([`Z_MAX_ADDRESS`]) that the rest of the VM's value representation
    /// assumes. See the module docs on `CompactValue`.
    RangeOutOfBounds {
        /// The offending base address.
        base: u64,
        /// The requested size in bytes.
        size: u64,
    },
}

impl std::fmt::Display for ZAddressSpaceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ZAddressSpaceError::HeapTooLarge { requested, max } => write!(
                f,
                "ZGC heap of {requested} bytes exceeds the {Z_OFFSET_BITS}-bit \
                 colored-pointer offset field (max {max} bytes)"
            ),
            ZAddressSpaceError::ZeroSize => {
                write!(f, "ZGC heap size must be greater than zero")
            }
            ZAddressSpaceError::BadBase { base } => write!(
                f,
                "ZGC heap base {base:#x} is null or not {Z_OBJECT_ALIGNMENT}-byte aligned"
            ),
            ZAddressSpaceError::RangeOutOfBounds { base, size } => write!(
                f,
                "ZGC heap range {base:#x}..+{size:#x} leaves the {Z_MAX_ADDRESS:#x} \
                 address window the VM's value representation assumes"
            ),
        }
    }
}

impl std::error::Error for ZAddressSpaceError {}

/// The reserved heap-offset range, and the translation between real machine
/// addresses and colored words.
///
/// This is CratonVM's stand-in for OpenJDK's multi-mapped virtual address
/// reservation. **It maps nothing.** It holds a base and a size, and it is the
/// single place that knows how to get from an [`Arena`](cratonvm_gc_core::arena::Arena)
/// address to a colored word and back. See the module docs for why that trade
/// was made and what it costs.
///
/// # Rebasing
///
/// `base` is a snapshot of where the backing memory lives. CratonVM arenas can
/// be reallocated when the heap grows, and every colored word in the heap is
/// expressed as an *offset*, so a move is a base update rather than a heap-wide
/// pointer rewrite — but it is still a stop-the-world operation, because a
/// mutator holding an unmasked address across the move would hold a dangling
/// one. Construct a new address space at a safepoint; do not mutate `base`
/// under a running mutator.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ZVirtualAddressSpace {
    base: u64,
    size: u64,
}

impl ZVirtualAddressSpace {
    /// Largest heap this encoding supports: [`Z_MAX_HEAP_SIZE`] (4 TiB).
    pub const MAX_HEAP_SIZE: u64 = Z_MAX_HEAP_SIZE;

    /// Reserve an offset range over the backing memory at `base`.
    ///
    /// Returns [`ZAddressSpaceError::HeapTooLarge`] — rather than truncating —
    /// when the request cannot be expressed in the 42-bit offset field. A
    /// truncating constructor would produce colored words that alias two
    /// different objects, which is the worst class of GC bug and the least
    /// debuggable; refusing at construction turns it into a startup error.
    pub fn new(base: u64, size: u64) -> Result<Self, ZAddressSpaceError> {
        if size == 0 {
            return Err(ZAddressSpaceError::ZeroSize);
        }
        if size > Z_MAX_HEAP_SIZE {
            return Err(ZAddressSpaceError::HeapTooLarge {
                requested: size,
                max: Z_MAX_HEAP_SIZE,
            });
        }
        if base == 0 || (base % Z_OBJECT_ALIGNMENT) != 0 {
            return Err(ZAddressSpaceError::BadBase { base });
        }
        match base.checked_add(size) {
            Some(end) if end <= Z_MAX_ADDRESS => {}
            _ => return Err(ZAddressSpaceError::RangeOutOfBounds { base, size }),
        }
        tracing::debug!(
            target: "zgc",
            base,
            size,
            "ZGC virtual address space reserved (single-mapped, explicit unmasking)"
        );
        Ok(ZVirtualAddressSpace { base, size })
    }

    /// Convenience constructor over a raw backing region, e.g. an
    /// [`Arena`](cratonvm_gc_core::arena::Arena)'s `base_ptr()` and `capacity()`.
    ///
    /// Takes the pointer's *address* only; nothing is dereferenced, so this is
    /// safe.
    pub fn for_region(base: *const u8, len: usize) -> Result<Self, ZAddressSpaceError> {
        Self::new(base as u64, len as u64)
    }

    /// Base machine address of the reserved region (heap offset 0).
    #[inline]
    pub const fn base(&self) -> u64 {
        self.base
    }

    /// Size of the reserved region in bytes.
    #[inline]
    pub const fn size(&self) -> u64 {
        self.size
    }

    /// One past the last byte of the reserved region.
    #[inline]
    pub const fn end(&self) -> u64 {
        self.base + self.size
    }

    /// Is `addr` inside the reserved region?
    #[inline]
    pub const fn contains_address(&self, addr: u64) -> bool {
        addr >= self.base && addr < self.base + self.size
    }

    /// Is `offset` inside the reserved region?
    #[inline]
    pub const fn contains_offset(&self, offset: u64) -> bool {
        offset < self.size
    }

    // -- address <-> offset -------------------------------------------------

    /// Machine address → heap offset, or `None` if the address is outside the
    /// reserved region.
    #[inline]
    pub fn offset_for_address(&self, addr: u64) -> Option<u64> {
        if self.contains_address(addr) {
            Some(addr - self.base)
        } else {
            None
        }
    }

    /// Heap offset → machine address, or `None` if the offset is out of range.
    #[inline]
    pub fn address_for_offset(&self, offset: u64) -> Option<u64> {
        if self.contains_offset(offset) {
            Some(self.base + offset)
        } else {
            None
        }
    }

    // -- address <-> colored word -------------------------------------------

    /// Machine address → colored word. `0` maps to [`Z_NULL`].
    ///
    /// Returns `None` for a non-null address outside the reserved region: the
    /// caller is holding something that is not a heap object, and silently
    /// encoding it would fabricate a reference to whatever lives at that
    /// offset.
    pub fn color_address(&self, addr: u64, c: ZColor) -> Option<ZColoredWord> {
        if addr == 0 {
            return Some(Z_NULL);
        }
        let offset = self.offset_for_address(addr)?;
        Some(color(offset, c))
    }

    /// Colored word → machine address. [`Z_NULL`] maps to `0`.
    ///
    /// This is the unmask every dereference pays under the single-mapped
    /// design. Returns `None` when the encoded offset is outside the reserved
    /// region, which means the word is corrupt (or belongs to a different
    /// address space after a rebase).
    #[inline]
    pub fn uncolor(&self, colored: ZColoredWord) -> Option<u64> {
        if is_null(colored) {
            return Some(0);
        }
        let offset = offset_of(colored);
        self.address_for_offset(offset)
    }

    /// The unchecked unmask, for the barrier fast path once the word is known
    /// to be well formed and in range.
    ///
    /// Two instructions: `AND` then `ADD`. This is precisely the cost that OS
    /// multi-mapping would eliminate.
    ///
    /// # Correctness
    ///
    /// Not `unsafe` — it cannot violate memory safety on its own, it only
    /// returns an integer. But it *will* return a nonsense address for a word
    /// whose offset is out of range, and dereferencing that is unsafe. Use
    /// [`uncolor`](Self::uncolor) unless you are on a measured hot path and the
    /// range invariant is established.
    #[inline]
    pub const fn uncolor_unchecked(&self, colored: ZColoredWord) -> u64 {
        self.base + (colored & Z_OFFSET_MASK)
    }

    // -- ObjectRef interop --------------------------------------------------

    /// [`ObjectRef`] → colored word.
    ///
    /// The **only** sanctioned way for a colored word to be produced from a
    /// live reference. Returns `None` when the reference points outside this
    /// address space.
    pub fn color_ref(&self, obj: ObjectRef, c: ZColor) -> Option<ZColoredWord> {
        self.color_address(obj.as_ptr() as u64, c)
    }

    /// Colored word → [`ObjectRef`].
    ///
    /// The **only** sanctioned way for a colored word to leave the encoded
    /// domain. Returns `None` for null and for an out-of-range offset.
    ///
    /// # Safety
    ///
    /// The caller must guarantee that the object the word encodes is live and
    /// has not been relocated — i.e. that the word has already been through the
    /// load barrier and carries the current good color. Calling this on a stale
    /// word hands out a dangling `ObjectRef`, which is exactly the failure mode
    /// the barrier exists to prevent.
    pub unsafe fn uncolor_to_ref(&self, colored: ZColoredWord) -> Option<ObjectRef> {
        let addr = self.uncolor(colored)?;
        if addr == 0 {
            return None;
        }
        // SAFETY: `addr` is inside the reserved region (checked by `uncolor`),
        // non-zero, and 8-byte aligned because the base is aligned and every
        // object offset is. Liveness is the caller's obligation, documented
        // above.
        Some(unsafe { ObjectRef::from_raw(addr as *mut u8) })
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests;
